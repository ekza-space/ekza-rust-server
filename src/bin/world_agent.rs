use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use reqwest::Client as HttpClient;
use rust_socketio::asynchronous::Client as SocketClient;
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::Payload;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, sleep, timeout};

// -----------------------------------------------------------------------------
// World agent (no bridge)
//
// Goal:
// - Connect as a real Socket.IO client (same protocol as the browser)
// - Spawn as a proper 3D character (nickname+avatar must be non-empty)
// - React to chat, move towards players at a comfortable distance, and avoid spam
// - Survive server restarts (reconnect + re-send user data)
//
// Protocol (ekza realtime; see `src/realtime/mod.rs`):
// - Incoming: "existing clients", "new user", "delete", "move", "chat message"
// - Outgoing: "set user data", "goto", "move", "chat message"
//
// Usage:
//   EKZA_URL=http://127.0.0.1:3001 EKZA_NICKNAME=Agent1 EKZA_AVATAR=ipfs://... \
//     cargo run -p server --bin world_agent
//
// Optional LLM (same env as `agent_bot`):
//   MOONSHOT_API_KEY=... KIMI_BASE_URL=... KIMI_MODEL=... AGENT_SYSTEM_PROMPT=...
// -----------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn get_arg_value(args: &[String], key: &str) -> Option<String> {
    let long = format!("--{key}");
    for (i, arg) in args.iter().enumerate() {
        if arg == &long {
            return args.get(i + 1).cloned();
        }
        if let Some(rest) = arg.strip_prefix(&(long.clone() + "=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn env_or_default(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_bool(name: &str, default: bool) -> bool {
    let v = std::env::var(name).unwrap_or_default();
    if v.trim().is_empty() {
        return default;
    }
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y" | "on"
    )
}

fn env_required_any(names: &[&str]) -> Result<String, Box<dyn Error>> {
    for name in names {
        let v = std::env::var(name).unwrap_or_default();
        if !v.trim().is_empty() {
            return Ok(v);
        }
    }
    Err(format!(
        "missing required env var (one of): {}",
        names.join(", ")
    )
    .into())
}

fn parse_u64(value: Option<String>, default: u64) -> u64 {
    value.and_then(|v| v.parse::<u64>().ok()).unwrap_or(default)
}

fn parse_f32(value: Option<String>, default: f32) -> f32 {
    value.and_then(|v| v.parse::<f32>().ok()).unwrap_or(default)
}

fn norm(s: &str) -> String {
    // Unicode-aware (chat is often non-ASCII).
    s.trim().to_lowercase()
}

#[allow(deprecated)]
fn payload_first_json(payload: &Payload) -> Option<Value> {
    match payload {
        Payload::Text(values) => values.get(0).cloned(),
        // deprecated but keep compatibility
        Payload::String(s) => serde_json::from_str::<Value>(s).ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ClientInfo {
    pub position: Vec<f32>,
    pub rotation: f32,
    pub avatar: String,
    pub nickname: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewUserBroadcast {
    id: String,
    user_data: ClientInfo,
}

#[derive(Debug, Clone, Deserialize)]
struct MoveBroadcast {
    id: String,
    position: Vec<f32>,
    rotation: f32,
    avatar: String,
    nickname: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatBroadcast {
    id: String,
    nickname: String,
    message: String,
}

#[derive(Debug, Default)]
struct WorldState {
    clients: HashMap<String, ClientInfo>,
    // Keep the last few incoming chat messages (excluding bot's own).
    recent_chat: VecDeque<(String, String, String)>, // (from_id, nickname, message)
    chat_seq: u64,
}

#[derive(Debug, Clone)]
enum CtrlEvent {
    IdentifiedSelf { id: String },
    Disconnected,
}

#[derive(Debug, Clone)]
struct SharedState {
    world: std::sync::Arc<RwLock<WorldState>>,
}

async fn connect_socket(
    ekza_url: &str,
    bot_nickname: &str,
    bot_avatar: &str,
    shared: SharedState,
    tx_ctrl: mpsc::Sender<CtrlEvent>,
) -> Result<SocketClient, Box<dyn Error>> {
    let bot_nickname = bot_nickname.to_string();
    let bot_avatar = bot_avatar.to_string();

    let socket = ClientBuilder::new(ekza_url.to_string())
        .on("existing clients", {
            let shared = shared.clone();
            move |payload: Payload, _socket: SocketClient| {
                let shared = shared.clone();
                async move {
                    let Some(v) = payload_first_json(&payload) else {
                        return;
                    };
                    if let Ok(map) = serde_json::from_value::<HashMap<String, ClientInfo>>(v) {
                        eprintln!("[world_agent] <- existing clients: {}", map.len());
                        let mut world = shared.world.write().await;
                        world.clients = map;
                    }
                }
                .boxed()
            }
        })
        .on("new user", {
            let shared = shared.clone();
            let tx_ctrl = tx_ctrl.clone();
            let bot_nickname = bot_nickname.clone();
            move |payload: Payload, _socket: SocketClient| {
                let shared = shared.clone();
                let tx_ctrl = tx_ctrl.clone();
                let bot_nickname = bot_nickname.clone();
                async move {
                    let Some(v) = payload_first_json(&payload) else {
                        return;
                    };
                    let Ok(b) = serde_json::from_value::<NewUserBroadcast>(v) else {
                        return;
                    };

                    let nick = b.user_data.nickname.trim();
                    if !nick.is_empty() {
                        eprintln!(
                            "[world_agent] <- new user id={} nickname='{}' avatar='{}'",
                            b.id,
                            nick,
                            b.user_data.avatar.trim()
                        );
                    }

                    {
                        let mut world = shared.world.write().await;
                        world.clients.insert(b.id.clone(), b.user_data.clone());
                    }

                    // Identify self by nickname (avatar can be normalized by the client).
                    let nick_ok = !bot_nickname.trim().is_empty()
                        && norm(b.user_data.nickname.trim()) == norm(bot_nickname.trim());
                    if nick_ok {
                        eprintln!("[world_agent] identified self id={}", b.id);
                        let _ = tx_ctrl
                            .send(CtrlEvent::IdentifiedSelf { id: b.id.clone() })
                            .await;
                    }
                }
                .boxed()
            }
        })
        .on("delete", {
            let shared = shared.clone();
            move |payload: Payload, _socket: SocketClient| {
                let shared = shared.clone();
                async move {
                    let Some(v) = payload_first_json(&payload) else {
                        return;
                    };
                    let id = v.as_str().unwrap_or("").to_string();
                    if id.is_empty() {
                        return;
                    }
                    eprintln!("[world_agent] <- delete id={}", id);
                    let mut world = shared.world.write().await;
                    world.clients.remove(&id);
                }
                .boxed()
            }
        })
        .on("move", {
            let shared = shared.clone();
            move |payload: Payload, _socket: SocketClient| {
                let shared = shared.clone();
                async move {
                    let Some(v) = payload_first_json(&payload) else {
                        return;
                    };
                    let Ok(m) = serde_json::from_value::<MoveBroadcast>(v) else {
                        return;
                    };
                    let mut world = shared.world.write().await;
                    world.clients.insert(
                        m.id.clone(),
                        ClientInfo {
                            position: m.position,
                            rotation: m.rotation,
                            avatar: m.avatar,
                            nickname: m.nickname,
                        },
                    );
                }
                .boxed()
            }
        })
        .on("chat message", {
            let shared = shared.clone();
            let bot_nickname = bot_nickname.clone();
            move |payload: Payload, _socket: SocketClient| {
                let shared = shared.clone();
                let bot_nickname = bot_nickname.clone();
                async move {
                    let Some(v) = payload_first_json(&payload) else {
                        return;
                    };
                    let Ok(c) = serde_json::from_value::<ChatBroadcast>(v) else {
                        return;
                    };
                    // Ignore our own messages (best-effort by nickname).
                    if !bot_nickname.trim().is_empty() && c.nickname.trim() == bot_nickname.trim() {
                        return;
                    }
                    let msg = c.message.trim();
                    let nick = c.nickname.trim();
                    if msg.is_empty() || nick.is_empty() {
                        return;
                    }

                    eprintln!("[world_agent] <- chat {}({}): {}", nick, c.id, msg);
                    let mut world = shared.world.write().await;
                    world.recent_chat.push_back((c.id, nick.to_string(), msg.to_string()));
                    while world.recent_chat.len() > 20 {
                        world.recent_chat.pop_front();
                    }
                    world.chat_seq = world.chat_seq.saturating_add(1);
                }
                .boxed()
            }
        })
        .on("disconnect", {
            let tx_ctrl = tx_ctrl.clone();
            move |_payload: Payload, _socket: SocketClient| {
                let tx_ctrl = tx_ctrl.clone();
                async move {
                    let _ = tx_ctrl.send(CtrlEvent::Disconnected).await;
                }
                .boxed()
            }
        })
        .on("connect_error", {
            // Keep the callback to avoid silently swallowing errors.
            move |payload: Payload, _socket: SocketClient| {
                async move {
                    if let Some(v) = payload_first_json(&payload) {
                        eprintln!("[world_agent] connect_error: {}", v);
                    }
                }
                .boxed()
            }
        })
        .on("error", move |payload: Payload, _socket: SocketClient| {
            async move {
                if let Some(v) = payload_first_json(&payload) {
                    eprintln!("[world_agent] socket error: {}", v);
                }
            }
            .boxed()
        })
        .connect()
        .await?;

    // Identify this client on the ekza realtime server.
    eprintln!(
        "[world_agent] connected; -> set user data nickname='{}' avatar='{}'",
        bot_nickname.trim(),
        bot_avatar.trim()
    );
    socket
        .emit(
            "set user data",
            json!({
                "nickname": bot_nickname,
                "avatar": bot_avatar,
            }),
        )
        .await?;

    Ok(socket)
}

#[derive(Clone)]
struct KimiClient {
    http: HttpClient,
    base_url: String,
    api_key: String,
    model: String,
}

impl KimiClient {
    fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            http: HttpClient::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
        }
    }

    async fn chat_json(&self, system: &str, user: &str) -> Result<Value, Box<dyn Error>> {
        // Kimi/Moonshot provides an OpenAI-compatible Chat Completions API.
        let url = format!("{}/chat/completions", self.base_url);
        let thinking_type = env_or_default("KIMI_THINKING_TYPE", "disabled");
        let body = json!({
            "model": self.model,
            // Moonshot enforces temperature for some models (e.g. kimi-k2.5 -> only 0.6).
            "temperature": 0.6,
            "max_tokens": 500,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ],
            "thinking": { "type": thinking_type }
        });

        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("Kimi HTTP {status} response: {text}").into());
        }

        let res: Value = serde_json::from_str(&text)
            .map_err(|e| format!("failed to parse Kimi JSON: {e}; body={text:?}"))?;

        let content = res
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        // Expect strict JSON from the model; best-effort parse.
        let parsed = serde_json::from_str::<Value>(&content)
            .map_err(|e| format!("LLM returned non-JSON content: {e}; content={content:?}"))?;
        Ok(parsed)
    }
}

fn is_addressed_to_bot(bot_nickname: &str, msg: &str) -> bool {
    let bot = norm(bot_nickname);
    let m = norm(msg);
    if bot.is_empty() || m.is_empty() {
        return false;
    }
    m.contains(&bot) || m.contains("agent") || m.contains("бот") || m.contains("@bot")
}

fn to_vec3(v: &[f32]) -> Option<[f32; 3]> {
    if v.len() != 3 {
        return None;
    }
    Some([v[0], v[1], v[2]])
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[derive(Debug, Clone)]
struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        // LCG constants (PCG-ish). Not crypto; just for "human-ish" jitter.
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state
    }

    fn gen_range_u64(&mut self, min: u64, max_inclusive: u64) -> u64 {
        if max_inclusive <= min {
            return min;
        }
        let span = max_inclusive - min + 1;
        min + (self.next_u64() % span)
    }

    fn gen_range_f32(&mut self, min: f32, max: f32) -> f32 {
        if max <= min {
            return min;
        }
        // 53-bit fraction in [0,1).
        let frac = ((self.next_u64() >> 11) as f64) / ((1u64 << 53) as f64);
        (min as f64 + (max - min) as f64 * frac) as f32
    }
}

fn pick_nearby_point(player_pos: [f32; 3], min_r: f32, max_r: f32, rng: &mut SimpleRng) -> [f32; 3] {
    let r = rng.gen_range_f32(min_r, max_r);
    let ang = rng.gen_range_f32(0.0, std::f32::consts::TAU);
    [player_pos[0] + r * ang.cos(), player_pos[1], player_pos[2] + r * ang.sin()]
}

fn truncate_chat(msg: &str, max_chars: usize) -> String {
    let trimmed = msg.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    trimmed.chars().take(max_chars).collect()
}

fn find_self_client<'a>(
    clients: &'a HashMap<String, ClientInfo>,
    nickname: &str,
    avatar: &str,
) -> Option<(&'a String, &'a ClientInfo)> {
    let nick = nickname.trim();
    if nick.is_empty() {
        return None;
    }
    let avatar = avatar.trim();

    // Prefer exact nickname+avatar match when an avatar is configured.
    if !avatar.is_empty() {
        if let Some(hit) = clients
            .iter()
            .find(|(_id, info)| info.nickname.trim() == nick && info.avatar.trim() == avatar)
        {
            return Some(hit);
        }
    }

    // Fallback: match by nickname only.
    clients.iter().find(|(_id, info)| info.nickname.trim() == nick)
}

#[derive(Debug, Clone)]
enum BrainEvent {
    LlmResult { action: Result<Value, String> },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load local env file if present (do not require it).
    let _ = dotenvy::dotenv();

    let args: Vec<String> = std::env::args().collect();

    // Env fallback:
    //   EKZA_URL, EKZA_NICKNAME, EKZA_AVATAR
    let ekza_url = get_arg_value(&args, "ekza-url")
        .unwrap_or_else(|| env_or_default("EKZA_URL", "http://127.0.0.1:3001"));
    let nickname = get_arg_value(&args, "nickname")
        .unwrap_or_else(|| env_or_default("EKZA_NICKNAME", "Agent"));
    let avatar = get_arg_value(&args, "avatar").unwrap_or_else(|| env_or_default("EKZA_AVATAR", ""));

    let tick_ms = parse_u64(get_arg_value(&args, "tick-ms"), 200).clamp(50, 2000);
    let reconnect_base_ms =
        parse_u64(get_arg_value(&args, "reconnect-base-ms"), 500).clamp(100, 30_000);
    let reconnect_max_ms =
        parse_u64(get_arg_value(&args, "reconnect-max-ms"), 10_000).clamp(reconnect_base_ms, 120_000);
    let approach_speed =
        parse_f32(get_arg_value(&args, "approach-speed"), 3.0).clamp(0.1, 50.0);

    // Bot behavior knobs (same naming as `agent_bot`).
    let llm_tick_ms = parse_u64(
        get_arg_value(&args, "llm-tick-ms").or_else(|| std::env::var("AGENT_LLM_TICK_MS").ok()),
        0,
    );
    let reply_to_all = env_bool("AGENT_REPLY_TO_ALL", false);
    let auto_approach = env_bool("AGENT_AUTO_APPROACH", true);
    let min_chat_interval_ms = parse_u64(std::env::var("AGENT_MIN_CHAT_INTERVAL_MS").ok(), 5000);
    let greeting = std::env::var("AGENT_GREETING")
        .ok()
        .and_then(|v| {
            let t = v.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        });

    let default_system_prompt = r#"You are an in-game AI agent controlling a single avatar in a 3D world.
You receive:
- a list of players with their nicknames and 3D positions
- recent chat messages

You MUST respond with STRICT JSON only (no markdown), with this schema:
{
  "say": string | null,
  "goto": { "x": number, "y": number, "z": number, "speed": number } | null
}

Rules:
- Keep "say" short (<= 200 chars).
- Prefer reacting to chat; do not spam unprompted messages.
- Only use "goto" if you want to approach someone or reposition.
- Do not include any keys other than say/goto.
"#;
    let system_prompt = std::env::var("AGENT_SYSTEM_PROMPT")
        .ok()
        .and_then(|v| {
            let t = v.trim().to_string();
            if t.is_empty() { None } else { Some(t) }
        })
        .unwrap_or_else(|| default_system_prompt.to_string());

    // Optional LLM mode: enabled only if the key is present and non-empty.
    let kimi = match env_required_any(&["MOONSHOT_API_KEY", "KIMI_API_KEY"]) {
        Ok(k) if !k.trim().is_empty() && k.trim() != "REPLACE_ME" => {
            let base = env_or_default("KIMI_BASE_URL", "https://api.moonshot.ai/v1");
            let model = env_or_default("KIMI_MODEL", "kimi-k2.5");
            Some(KimiClient::new(base, k.trim().to_string(), model))
        }
        _ => None,
    };

    eprintln!(
        "[world_agent] ekza={} nickname={} tick_ms={} llm={} (llm_tick_ms={})",
        ekza_url,
        nickname,
        tick_ms,
        kimi.is_some(),
        llm_tick_ms
    );

    let mut backoff_ms = reconnect_base_ms;
    loop {
        match run_session(
            &ekza_url,
            &nickname,
            &avatar,
            greeting.clone(),
            tick_ms,
            approach_speed,
            min_chat_interval_ms,
            reply_to_all,
            auto_approach,
            llm_tick_ms,
            &system_prompt,
            kimi.clone(),
        )
        .await
        {
            Ok(SessionEnd::Exit) => break,
            Ok(SessionEnd::Disconnected) => {
                backoff_ms = reconnect_base_ms;
            }
            Err(err) => {
                eprintln!("[world_agent] session error: {err}");
                backoff_ms = (backoff_ms * 2).min(reconnect_max_ms);
            }
        }

        eprintln!("[world_agent] reconnect in {}ms", backoff_ms);
        sleep(Duration::from_millis(backoff_ms)).await;
    }

    Ok(())
}

#[derive(Debug)]
enum SessionEnd {
    Exit,
    Disconnected,
}

async fn run_session(
    ekza_url: &str,
    nickname: &str,
    avatar: &str,
    greeting: Option<String>,
    tick_ms: u64,
    approach_speed: f32,
    min_chat_interval_ms: u64,
    reply_to_all: bool,
    auto_approach: bool,
    llm_tick_ms: u64,
    system_prompt: &str,
    kimi: Option<KimiClient>,
) -> Result<SessionEnd, Box<dyn Error>> {
    let shared = SharedState {
        world: std::sync::Arc::new(RwLock::new(WorldState::default())),
    };
    let (tx_ctrl, mut rx_ctrl) = mpsc::channel::<CtrlEvent>(16);
    let (tx_brain, mut rx_brain) = mpsc::channel::<BrainEvent>(8);

    let socket = connect_socket(ekza_url, nickname, avatar, shared.clone(), tx_ctrl).await?;
    let mut rng = SimpleRng::new(now_ms() ^ 0x9E3779B97F4A7C15);
    let session_started_at_ms = now_ms();
    let mut resent_user_data = false;

    let mut self_id: Option<String> = None;
    let mut spawned = false;
    let mut greeting_sent = false;

    let mut greeted: HashMap<String, u64> = HashMap::new(); // client_id -> ts_ms
    let mut last_sent_chat_ms: u64 = 0;
    let mut last_sent_chat_text: String = String::new();
    let mut last_move_ms: u64 = 0;

    let mut last_llm_ms: u64 = 0;
    let mut llm_in_flight = false;

    let mut last_chat_seq_seen: u64 = 0;
    let mut last_incoming_chat: Option<(String, String, String)> = None; // (from_id, nick, msg)
    let mut last_incoming_chat_at_ms: u64 = 0;

    let mut next_idle_move_ms = now_ms() + rng.gen_range_u64(10_000, 30_000);

    let mut ticker = interval(Duration::from_millis(tick_ms));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                eprintln!("[world_agent] ctrl-c, exiting");
                let _ = timeout(Duration::from_millis(800), socket.disconnect()).await;
                return Ok(SessionEnd::Exit);
            }
            Some(ev) = rx_ctrl.recv() => {
                match ev {
                    CtrlEvent::IdentifiedSelf { id } => {
                        if self_id.is_none() {
                            eprintln!("[world_agent] self id={}", id);
                            self_id = Some(id);
                        }
                    }
                    CtrlEvent::Disconnected => {
                        let _ = timeout(Duration::from_millis(800), socket.disconnect()).await;
                        return Ok(SessionEnd::Disconnected);
                    }
                }
            }
            Some(ev) = rx_brain.recv() => {
                match ev {
                    BrainEvent::LlmResult { action } => {
                        llm_in_flight = false;
                        let now = now_ms();
                        last_llm_ms = now;

                        let action = match action {
                            Ok(v) => v,
                            Err(err) => {
                                eprintln!("[world_agent] llm error: {}", err);
                                continue;
                            }
                        };

                        let mut llm_requested_goto = false;

                        // say
                        if let Some(say) = action.get("say").and_then(|v| v.as_str()) {
                            let msg = truncate_chat(say, 200);
                            let is_dupe = norm(&msg) == norm(&last_sent_chat_text);
                            if !msg.is_empty()
                                && !is_dupe
                                && now.saturating_sub(last_sent_chat_ms) >= min_chat_interval_ms
                            {
                                eprintln!("[world_agent] -> chat (llm): {}", msg);
                                if socket.emit("chat message", msg.as_str()).await.is_ok() {
                                    last_sent_chat_ms = now;
                                    last_sent_chat_text = msg;
                                }
                            }
                        }

                        // goto
                        if let Some(g) = action.get("goto").and_then(|v| v.as_object()) {
                            let x = g.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            let y = g.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            let z = g.get("z").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                            let sp = g
                                .get("speed")
                                .and_then(|v| v.as_f64())
                                .unwrap_or(approach_speed as f64) as f32;
                            let sp = sp.clamp(0.1, 50.0);
                            if socket.emit("goto", json!({ "position": [x, y, z], "speed": sp })).await.is_ok() {
                                eprintln!(
                                    "[world_agent] -> goto (llm): [{:.2}, {:.2}, {:.2}] speed={:.2}",
                                    x, y, z, sp
                                );
                                llm_requested_goto = true;
                                last_move_ms = now;
                            }
                        }

                        // Fallback movement: approach nearest if enabled and LLM didn't request goto.
                        if auto_approach && !llm_requested_goto && now.saturating_sub(last_move_ms) > 2_000 {
                            let (self_pos, nearest) = snapshot_self_and_nearest(shared.world.clone(), self_id.clone(), nickname).await;
                            if let (Some(_me), Some((target_id, target_pos, d))) = (self_pos, nearest) {
                                let not_self = self_id.as_ref().map(|sid| sid != &target_id).unwrap_or(true);
                                if not_self && d > 2.0 {
                                    let target = pick_nearby_point(target_pos, 1.2, 2.0, &mut rng);
                                    eprintln!(
                                        "[world_agent] -> goto (fallback): [{:.2}, {:.2}, {:.2}] speed={:.2}",
                                        target[0], target[1], target[2], approach_speed
                                    );
                                    let _ = socket.emit("goto", json!({ "position": target, "speed": approach_speed })).await;
                                    last_move_ms = now;
                                }
                            }
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                // decision tick
            }
        }

        // -------- decision tick (runs after select tick branch) ----------
        let now = now_ms();

        // Snapshot world state (cheap and consistent for this tick).
        let (clients_snapshot, recent_chat_snapshot, chat_seq) = {
            let world = shared.world.read().await;
            (world.clients.clone(), world.recent_chat.clone(), world.chat_seq)
        };

        // Best-effort: discover our own socket id from the world snapshot.
        if self_id.is_none() {
            if let Some((id, _info)) = find_self_client(&clients_snapshot, nickname, avatar) {
                eprintln!("[world_agent] detected self id={} from snapshot", id);
                self_id = Some(id.clone());
            }
        }

        // If for some reason the initial identify packet was lost/racy, resend it once.
        if !resent_user_data
            && find_self_client(&clients_snapshot, nickname, avatar).is_none()
            && now.saturating_sub(session_started_at_ms) > 1500
        {
            eprintln!(
                "[world_agent] self not visible yet; -> set user data nickname='{}'",
                nickname.trim()
            );
            let _ = socket
                .emit(
                    "set user data",
                    json!({
                        "nickname": nickname,
                        "avatar": avatar,
                    }),
                )
                .await;
            resent_user_data = true;
        }

        // Detect new incoming chat.
        let saw_new_chat = chat_seq > last_chat_seq_seen;
        if saw_new_chat {
            last_chat_seq_seen = chat_seq;
            last_incoming_chat = recent_chat_snapshot.back().cloned();
            last_incoming_chat_at_ms = now;

            if let Some((_from_id, _nick, msg)) = last_incoming_chat.as_ref() {
                let addressed = is_addressed_to_bot(nickname, msg);
                if !addressed && !reply_to_all {
                    eprintln!(
                        "[world_agent] note: chat not addressed; mention '{}' or set AGENT_REPLY_TO_ALL=true",
                        nickname.trim()
                    );
                }
            }
        }

        let self_visible = find_self_client(&clients_snapshot, nickname, avatar).is_some();

        // One-time spawn move so other clients render us above ground.
        if self_visible && !spawned {
            // Move once so other clients render us above ground.
            eprintln!("[world_agent] -> move spawn [0,2,0]");
            let _ = socket
                .emit(
                    "move",
                    json!({
                        "position": [0.0, 2.0, 0.0],
                        "rotation": 0.0,
                    }),
                )
                .await;
            spawned = true;
        }

        // Greeting (once, after identity is visible so chat shows correct nickname).
        if self_visible && !greeting_sent {
            if let Some(g) = greeting.as_ref() {
                if now.saturating_sub(last_sent_chat_ms) >= min_chat_interval_ms {
                    let msg = truncate_chat(g, 200);
                    eprintln!("[world_agent] -> chat (greeting): {}", msg);
                    if !msg.is_empty() && socket.emit("chat message", msg.as_str()).await.is_ok() {
                        last_sent_chat_ms = now;
                        last_sent_chat_text = msg;
                        greeting_sent = true;
                    }
                }
            } else {
                greeting_sent = true;
            }
        }

        // Find self+nearest (best-effort).
        let self_pos = self_id
            .as_ref()
            .and_then(|id| clients_snapshot.get(id))
            .and_then(|info| to_vec3(&info.position))
            .or_else(|| {
                find_self_client(&clients_snapshot, nickname, avatar)
                    .and_then(|(_id, info)| to_vec3(&info.position))
            });

        let mut nearest: Option<(String, [f32; 3], f32, String)> = None; // (id, pos, dist, nickname)
        if let Some(me) = self_pos {
            for (id, info) in clients_snapshot.iter() {
                if self_id.as_ref().map(|sid| sid == id).unwrap_or(false) {
                    continue;
                }
                if self_id.is_none() && info.nickname.trim() == nickname.trim() {
                    // If we don't know our id yet, avoid selecting ourselves by nickname.
                    continue;
                }
                let nick = info.nickname.trim();
                if nick.is_empty() {
                    continue;
                }
                let Some(pos) = to_vec3(&info.position) else {
                    continue;
                };
                let d = distance(me, pos);
                if nearest.as_ref().map(|x| d < x.2).unwrap_or(true) {
                    nearest = Some((id.clone(), pos, d, nick.to_string()));
                }
            }
        }

        // Simple non-LLM greeting when close (so the bot "does something" even without chat).
        if let Some((id, _pos, d, nick)) = nearest.clone() {
            if d <= 3.0 && !greeted.contains_key(&id) && self_visible {
                let msg = format!("hey {nick} :)");
                if now.saturating_sub(last_sent_chat_ms) >= min_chat_interval_ms {
                    let msg = truncate_chat(&msg, 200);
                    eprintln!("[world_agent] -> chat (nearby greet): {}", msg);
                    if socket.emit("chat message", msg.as_str()).await.is_ok() {
                        last_sent_chat_ms = now;
                        last_sent_chat_text = msg;
                        greeted.insert(id, now);
                    }
                }
            }
        }

        // If someone addressed us in chat, do an immediate "approach" (movement) even before LLM.
        if saw_new_chat && self_visible {
            if let Some((from_id, _nick, msg)) = last_incoming_chat.as_ref() {
                if is_addressed_to_bot(nickname, msg) && now.saturating_sub(last_move_ms) > 1_000 {
                    let sender_pos = clients_snapshot
                        .get(from_id)
                        .and_then(|info| to_vec3(&info.position));
                    if let Some(p) = sender_pos {
                        let target = pick_nearby_point(p, 1.2, 2.0, &mut rng);
                        eprintln!(
                            "[world_agent] -> goto (approach): [{:.2}, {:.2}, {:.2}] speed={:.2}",
                            target[0], target[1], target[2], approach_speed
                        );
                        let _ = socket
                            .emit("goto", json!({ "position": target, "speed": approach_speed }))
                            .await;
                        last_move_ms = now;
                    }
                }
            }
        }

        // Idle movement (rare, non-spam).
        if self_pos.is_some() && now >= next_idle_move_ms && now.saturating_sub(last_move_ms) > 2_000 {
            if let Some(me) = self_pos {
                let wander = [
                    me[0] + rng.gen_range_f32(-1.5, 1.5),
                    me[1],
                    me[2] + rng.gen_range_f32(-1.5, 1.5),
                ];
                eprintln!(
                    "[world_agent] -> goto (idle): [{:.2}, {:.2}, {:.2}] speed={:.2}",
                    wander[0],
                    wander[1],
                    wander[2],
                    (approach_speed * 0.7).clamp(0.1, 50.0)
                );
                let _ = socket
                    .emit("goto", json!({ "position": wander, "speed": (approach_speed * 0.7).clamp(0.1, 50.0) }))
                    .await;
                last_move_ms = now;
            }
            next_idle_move_ms = now + rng.gen_range_u64(10_000, 30_000);
        }

        // LLM decision: on new chat (addressed / reply_to_all) OR periodically.
        let addressed = last_incoming_chat
            .as_ref()
            .map(|(_id, _n, m)| is_addressed_to_bot(nickname, m))
            .unwrap_or(false);
        let should_call_llm_on_chat = saw_new_chat
            && now.saturating_sub(last_llm_ms) > 1_500
            && (reply_to_all || addressed);
        let should_call_llm_proactive =
            llm_tick_ms > 0 && now.saturating_sub(last_llm_ms) > llm_tick_ms;
        let should_call_llm = should_call_llm_on_chat || should_call_llm_proactive;

        if should_call_llm && self_visible {
            if let Some(kimi) = kimi.clone() {
                if !llm_in_flight {
                    llm_in_flight = true;
                    last_llm_ms = now;
                    eprintln!(
                        "[world_agent] llm: calling (on_chat={}, proactive={}, addressed={}, reply_to_all={})",
                        should_call_llm_on_chat,
                        should_call_llm_proactive,
                        addressed,
                        reply_to_all
                    );

                    let chat_text = recent_chat_snapshot
                        .iter()
                        .map(|(_id, n, m)| format!("{n}: {m}"))
                        .collect::<Vec<_>>()
                        .join("\n");

                    let players_text = clients_snapshot
                        .values()
                        .filter_map(|info| {
                            let nick = info.nickname.trim();
                            if nick.is_empty() {
                                return None;
                            }
                            let p = to_vec3(&info.position)?;
                            Some(format!(
                                "{nick} at [{:.1}, {:.1}, {:.1}]",
                                p[0], p[1], p[2]
                            ))
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    let nearest_text = nearest
                        .as_ref()
                        .map(|(_id, p, d, nick)| {
                            format!(
                                "{nick} at [{:.1}, {:.1}, {:.1}] (dist {:.1})",
                                p[0], p[1], p[2], d
                            )
                        })
                        .unwrap_or_else(|| "none".to_string());

                    let self_pos_text = self_pos
                        .map(|p| format!("[{:.1}, {:.1}, {:.1}]", p[0], p[1], p[2]))
                        .unwrap_or_else(|| "[0.0, 0.0, 0.0]".to_string());

                    let user_prompt = format!(
                        "You are '{nickname}'. Your position is {self_pos_text}.\nNearest player: {nearest_text}\n\nPlayers:\n{players_text}\n\nRecent chat (may be empty):\n{chat_text}\n\nDecide what to do next.",
                    );

                    let tx_brain = tx_brain.clone();
                    let system_prompt = system_prompt.to_string();
                    tokio::spawn(async move {
                        let res = kimi
                            .chat_json(system_prompt.as_str(), user_prompt.as_str())
                            .await
                            .map_err(|e| e.to_string());
                        let _ = tx_brain.send(BrainEvent::LlmResult { action: res }).await;
                    });
                }
            } else if should_call_llm_on_chat && addressed {
                // No LLM configured: send a short deterministic reply (anti-silence).
                if let Some((_from_id, nick, msg)) = last_incoming_chat.as_ref() {
                    if now.saturating_sub(last_sent_chat_ms) >= min_chat_interval_ms {
                        let reply = truncate_chat(&format!("ok {nick}, you said: {msg}"), 200);
                        if socket.emit("chat message", reply.as_str()).await.is_ok() {
                            last_sent_chat_ms = now;
                            last_sent_chat_text = reply;
                        }
                    }
                }
            }
        }

        // If we haven't seen chat in a while, allow another LLM proactive call later.
        if last_incoming_chat_at_ms > 0 && now.saturating_sub(last_incoming_chat_at_ms) > 30_000 {
            last_incoming_chat_at_ms = 0;
        }
    }
}

async fn snapshot_self_and_nearest(
    world: std::sync::Arc<RwLock<WorldState>>,
    self_id: Option<String>,
    self_nickname: &str,
) -> (Option<[f32; 3]>, Option<(String, [f32; 3], f32)>) {
    let world = world.read().await;
    let clients = &world.clients;

    let self_pos = if let Some(id) = self_id.as_ref() {
        clients.get(id).and_then(|info| to_vec3(&info.position))
    } else {
        clients
            .values()
            .find(|info| info.nickname.trim() == self_nickname.trim())
            .and_then(|info| to_vec3(&info.position))
    };

    let Some(me) = self_pos else {
        return (None, None);
    };

    let mut nearest: Option<(String, [f32; 3], f32)> = None;
    for (id, info) in clients.iter() {
        if self_id.as_ref().map(|x| x == id).unwrap_or(false) {
            continue;
        }
        if self_id.is_none() && info.nickname.trim() == self_nickname.trim() {
            // When we don't know our id yet, avoid picking ourselves by nickname.
            continue;
        }
        if info.nickname.trim().is_empty() {
            continue;
        }
        let Some(p) = to_vec3(&info.position) else {
            continue;
        };
        let d = distance(me, p);
        if nearest.as_ref().map(|x| d < x.2).unwrap_or(true) {
            nearest = Some((id.clone(), p, d));
        }
    }

    (Some(me), nearest)
}

