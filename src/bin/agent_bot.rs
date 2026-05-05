use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde_json::{json, Value};
use tokio::time::{interval, sleep};

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

fn norm(s: &str) -> String {
    s.trim().to_ascii_lowercase()
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

#[derive(Clone)]
struct BridgeClient {
    http: Client,
    base_url: String,
}

impl BridgeClient {
    fn new(base_url: String) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    async fn create_agent(&self, nickname: &str, avatar: &str) -> Result<u64, Box<dyn Error>> {
        let res: Value = self
            .http
            .post(format!("{}/api/v1/agents", self.base_url))
            .json(&json!({ "nickname": nickname, "avatar": avatar }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let agent_id = res
            .get("agentId")
            .and_then(|v| v.as_u64())
            .ok_or("bridge response missing agentId")?;
        Ok(agent_id)
    }

    async fn wait_ready(&self, total_wait_ms: u64) -> Result<(), Box<dyn Error>> {
        let start = now_ms();
        loop {
            let url = format!("{}/health", self.base_url);
            match self.http.get(url).send().await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ => {
                    if now_ms().saturating_sub(start) >= total_wait_ms {
                        return Err(format!(
                            "agent_bridge is not reachable at {} (start it: `make bridge`)",
                            self.base_url
                        )
                        .into());
                    }
                    sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }

    async fn wait_for_nickname(
        &self,
        nickname: &str,
        total_wait_ms: u64,
    ) -> Result<(), Box<dyn Error>> {
        let nickname = nickname.trim();
        if nickname.is_empty() {
            return Ok(());
        }
        let start = now_ms();
        loop {
            let state = self.get_state().await.unwrap_or(Value::Null);
            if let Some(obj) = state.get("clients").and_then(|v| v.as_object()) {
                let ok = obj.values().any(|info| {
                    info.get("nickname")
                        .and_then(|n| n.as_str())
                        .map(|n| n.trim() == nickname)
                        .unwrap_or(false)
                });
                if ok {
                    return Ok(());
                }
            }
            if now_ms().saturating_sub(start) >= total_wait_ms {
                return Err(format!(
                    "agent nickname '{}' did not appear in bridge state; is agent_bridge observing the same ekza server?",
                    nickname
                )
                .into());
            }
            sleep(Duration::from_millis(200)).await;
        }
    }

    async fn get_state(&self) -> Result<Value, Box<dyn Error>> {
        let v: Value = self
            .http
            .get(format!("{}/api/v1/state", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v)
    }

    async fn get_events(&self, after: u64, limit: usize) -> Result<Vec<Value>, Box<dyn Error>> {
        let url = format!(
            "{}/api/v1/events?after={}&limit={}",
            self.base_url, after, limit
        );
        let v: Vec<Value> = self
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(v)
    }

    async fn send_chat(&self, agent_id: u64, message: &str) -> Result<(), Box<dyn Error>> {
        self.http
            .post(format!(
                "{}/api/v1/agents/{}/command/chat",
                self.base_url, agent_id
            ))
            .json(&json!({ "message": message }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn set_user_data(
        &self,
        agent_id: u64,
        nickname: &str,
        avatar: &str,
    ) -> Result<(), Box<dyn Error>> {
        self.http
            .post(format!(
                "{}/api/v1/agents/{}/command/user_data",
                self.base_url, agent_id
            ))
            .json(&json!({ "nickname": nickname, "avatar": avatar }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn goto(&self, agent_id: u64, position: [f32; 3], speed: f32) -> Result<(), Box<dyn Error>> {
        self.http
            .post(format!(
                "{}/api/v1/agents/{}/command/goto",
                self.base_url, agent_id
            ))
            .json(&json!({
                "position": position,
                "speed": speed,
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[derive(Clone)]
struct KimiClient {
    http: Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl KimiClient {
    fn new(base_url: String, api_key: String, model: String) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
        }
    }

    async fn chat_json(&self, system: &str, user: &str) -> Result<Value, Box<dyn Error>> {
        // Kimi/Moonshot provides an OpenAI-compatible Chat Completions API.
        // Base URL is usually: https://api.moonshot.cn/v1
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
            // Include response body to make auth/config issues obvious (401, etc).
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

fn extract_clients(state: &Value) -> Vec<(String, [f32; 3])> {
    let mut out = Vec::new();
    let Some(clients) = state.get("clients").and_then(|v| v.as_object()) else {
        return out;
    };
    for (_id, info) in clients {
        let nickname = info
            .get("nickname")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let pos = info.get("position").and_then(|p| p.as_array());
        if nickname.is_empty() {
            continue;
        }
        let Some(pos) = pos else { continue };
        if pos.len() != 3 {
            continue;
        }
        let x = pos[0].as_f64().unwrap_or(0.0) as f32;
        let y = pos[1].as_f64().unwrap_or(0.0) as f32;
        let z = pos[2].as_f64().unwrap_or(0.0) as f32;
        out.push((nickname, [x, y, z]));
    }
    out
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn is_addressed_to_bot(bot_nickname: &str, msg: &str) -> bool {
    let bot = norm(bot_nickname);
    let m = norm(msg);
    if bot.is_empty() || m.is_empty() {
        return false;
    }
    m.contains(&bot) || m.contains("agent")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load local env file if present (do not require it).
    let _ = dotenvy::dotenv();

    // IMPORTANT: do NOT hardcode any API keys. Set them via env vars.
    //
    // Required env:
    //   MOONSHOT_API_KEY
    //   (KIMI_API_KEY is accepted as a legacy alias)
    //
    // Optional env:
    //   BRIDGE_URL       (default http://127.0.0.1:5055)
    //   EKZA_NICKNAME    (default Agent)
    //   EKZA_AVATAR      (default "")
    //   KIMI_BASE_URL    (default https://api.moonshot.ai/v1)
    //   KIMI_MODEL       (default kimi-k2.5)
    //   KIMI_THINKING_TYPE (default disabled)
    //   AGENT_SYSTEM_PROMPT (optional; overrides default persona/instructions)
    //   AGENT_GREETING (optional; bot says it once on start)
    //   AGENT_LLM_TICK_MS (default 8000; call LLM even without new chat)
    //   AGENT_AUTO_APPROACH (default true; approach nearest even without LLM goto)
    //   AGENT_REPLY_TO_ALL (default false; if true, respond to any chat, not only addressed)
    //   AGENT_MIN_CHAT_INTERVAL_MS (default 5000; rate limit outgoing chat)
    //
    // Usage:
    //   BRIDGE_URL=http://127.0.0.1:5055 KIMI_API_KEY=... cargo run --bin agent_bot
    let args: Vec<String> = std::env::args().collect();

    let bridge_url = get_arg_value(&args, "bridge")
        .unwrap_or_else(|| env_or_default("BRIDGE_URL", "http://127.0.0.1:5055"));
    let nickname = get_arg_value(&args, "nickname")
        .unwrap_or_else(|| env_or_default("EKZA_NICKNAME", "Agent"));
    let avatar =
        get_arg_value(&args, "avatar").unwrap_or_else(|| env_or_default("EKZA_AVATAR", ""));

    // Prefer the official env var name used by Moonshot docs.
    let kimi_api_key = env_required_any(&["MOONSHOT_API_KEY", "KIMI_API_KEY"])?;
    let kimi_api_key = kimi_api_key.trim().to_string();
    if kimi_api_key.is_empty() || kimi_api_key == "REPLACE_ME" {
        return Err("MOONSHOT_API_KEY is not set (still REPLACE_ME). Put your real key in .env".into());
    }
    // Moonshot docs often use the .ai domain for OpenAI-compatible API.
    let kimi_base_url = env_or_default("KIMI_BASE_URL", "https://api.moonshot.ai/v1");
    let kimi_model = env_or_default("KIMI_MODEL", "kimi-k2.5");

    let poll_ms = parse_u64(get_arg_value(&args, "poll-ms"), 500);
    let approach_speed = parse_f32(get_arg_value(&args, "approach-speed"), 3.0).clamp(0.1, 50.0);
    let llm_tick_ms = parse_u64(
        get_arg_value(&args, "llm-tick-ms")
            .or_else(|| std::env::var("AGENT_LLM_TICK_MS").ok()),
        0,
    );
    let auto_approach = env_bool("AGENT_AUTO_APPROACH", true);
    let reply_to_all = env_bool("AGENT_REPLY_TO_ALL", false);
    let min_chat_interval_ms =
        parse_u64(std::env::var("AGENT_MIN_CHAT_INTERVAL_MS").ok(), 5000);

    eprintln!(
        "[agent_bot] bridge={} nickname={} poll_ms={} llm_tick_ms={}",
        bridge_url, nickname, poll_ms, llm_tick_ms
    );

    let bridge = BridgeClient::new(bridge_url);
    // Friendly UX: wait a bit for the bridge to come up.
    bridge.wait_ready(10_000).await?;
    let kimi = KimiClient::new(kimi_base_url, kimi_api_key, kimi_model);

    // Create one agent socket on the bridge (this becomes our in-world "player").
    let agent_id = bridge.create_agent(&nickname, &avatar).await?;
    eprintln!("[agent_bot] created agent_id={}", agent_id);

    // Make sure nickname/avatar are set even if connect ordering is racy.
    let _ = bridge.set_user_data(agent_id, &nickname, &avatar).await;
    // Avoid "Unknown" chat payloads: wait briefly until nickname is visible in the observed state.
    let _ = bridge.wait_for_nickname(&nickname, 4000).await;

    if let Ok(greeting) = std::env::var("AGENT_GREETING") {
        let msg = greeting.trim();
        if !msg.is_empty() {
            let _ = bridge.send_chat(agent_id, msg).await;
        }
    }

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
- Do not repeat the same greeting multiple times.
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

    let mut last_event_id: u64 = 0;
    let mut recent_chat: VecDeque<(String, String)> = VecDeque::new(); // (nickname, message)
    let mut last_llm_ms: u64 = 0;
    let mut last_move_ms: u64 = 0;
    let mut greeted: HashMap<String, u64> = HashMap::new(); // nickname -> ts_ms
    let mut last_sent_chat_ms: u64 = 0;
    let mut last_sent_chat_text: String = String::new();
    let mut ticker = interval(Duration::from_millis(poll_ms));

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("[agent_bot] ctrl-c, exiting");
                break;
            }
            _ = ticker.tick() => {}
        }

        // Pull new events.
        let events = match bridge.get_events(last_event_id, 200).await {
            Ok(v) => v,
            Err(err) => {
                eprintln!("[agent_bot] bridge get_events error: {err}");
                sleep(Duration::from_millis(750)).await;
                continue;
            }
        };

        let mut saw_new_chat = false;
        let mut last_incoming_chat: Option<(String, String)> = None;
        for env in events {
            let id = env.get("id").and_then(|v| v.as_u64()).unwrap_or(last_event_id);
            if id > last_event_id {
                last_event_id = id;
            }
            let ev = env.get("event").cloned().unwrap_or(Value::Null);
            if ev.get("type").and_then(|t| t.as_str()) == Some("chat_message") {
                let nick = ev.get("nickname").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let msg = ev.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
                if !nick.is_empty() && !msg.is_empty() && nick != nickname {
                    recent_chat.push_back((nick.clone(), msg.clone()));
                    while recent_chat.len() > 20 {
                        recent_chat.pop_front();
                    }
                    saw_new_chat = true;
                    last_incoming_chat = Some((nick, msg));
                }
            }
        }

        // Get current world snapshot (positions).
        let state = match bridge.get_state().await {
            Ok(v) => v,
            Err(err) => {
                eprintln!("[agent_bot] bridge get_state error: {err}");
                continue;
            }
        };
        let clients = extract_clients(&state);

        // Find our own position by nickname (best-effort).
        let self_pos = clients
            .iter()
            .find(|(n, _)| n == &nickname)
            .map(|(_, p)| *p)
            .unwrap_or([0.0, 0.0, 0.0]);

        // Find nearest other player.
        let mut nearest: Option<(String, [f32; 3], f32)> = None;
        for (n, p) in clients.iter() {
            if n == &nickname {
                continue;
            }
            let d = distance(self_pos, *p);
            if nearest.as_ref().map(|x| d < x.2).unwrap_or(true) {
                nearest = Some((n.clone(), *p, d));
            }
        }

        let now = now_ms();
        // Simple non-LLM greeting when close (so the bot "does something" even without chat).
        if let Some((n, _pos, d)) = nearest.clone() {
            if d <= 3.0 && !greeted.contains_key(&n) {
                let msg = format!("hey {n} :)");
                if now.saturating_sub(last_sent_chat_ms) >= min_chat_interval_ms {
                    let _ = bridge.send_chat(agent_id, &msg).await;
                    last_sent_chat_ms = now;
                    last_sent_chat_text = msg;
                    greeted.insert(n, now);
                }
            }
        }

        // Call LLM on new chat OR periodically, so it can be proactive.
        let addressed = last_incoming_chat
            .as_ref()
            .map(|(_n, m)| is_addressed_to_bot(&nickname, m))
            .unwrap_or(false);
        let should_call_llm_on_chat =
            saw_new_chat && now.saturating_sub(last_llm_ms) > 1_500 && (reply_to_all || addressed);
        let should_call_llm_proactive =
            llm_tick_ms > 0 && now.saturating_sub(last_llm_ms) > llm_tick_ms;
        let should_call_llm = should_call_llm_on_chat || should_call_llm_proactive;
        if should_call_llm {
            let chat_text = recent_chat
                .iter()
                .map(|(n, m)| format!("{n}: {m}"))
                .collect::<Vec<_>>()
                .join("\n");
            let players_text = clients
                .iter()
                .filter(|(n, _)| !n.is_empty())
                .map(|(n, p)| format!("{n} at [{:.1}, {:.1}, {:.1}]", p[0], p[1], p[2]))
                .collect::<Vec<_>>()
                .join("\n");

            let nearest_text = nearest
                .as_ref()
                .map(|(n, p, d)| format!("{n} at [{:.1}, {:.1}, {:.1}] (dist {:.1})", p[0], p[1], p[2], d))
                .unwrap_or_else(|| "none".to_string());

            let user_prompt = format!(
                "You are '{nickname}'. Your position is [{:.1}, {:.1}, {:.1}].\nNearest player: {nearest_text}\n\nPlayers:\n{players_text}\n\nRecent chat (may be empty):\n{chat_text}\n\nDecide what to do next (you can greet, ask a question, or move closer).",
                self_pos[0], self_pos[1], self_pos[2]
            );

            match kimi.chat_json(system_prompt.as_str(), &user_prompt).await {
                Ok(action) => {
                    last_llm_ms = now;
                    let mut llm_requested_goto = false;
                    // say
                    if let Some(say) = action.get("say").and_then(|v| v.as_str()) {
                        let msg = say.trim();
                        let is_dupe = norm(msg) == norm(&last_sent_chat_text);
                        if !msg.is_empty()
                            && !is_dupe
                            && now.saturating_sub(last_sent_chat_ms) >= min_chat_interval_ms
                        {
                            let _ = bridge.send_chat(agent_id, msg).await;
                            last_sent_chat_ms = now;
                            last_sent_chat_text = msg.to_string();
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
                        let _ = bridge.goto(agent_id, [x, y, z], sp.clamp(0.1, 50.0)).await;
                        llm_requested_goto = true;
                    }

                    // Fallback movement: approach nearest if enabled and LLM didn't request goto.
                    if auto_approach && !llm_requested_goto {
                        if let Some((_n, pos, d)) = nearest.clone() {
                            if d > 2.0 && now.saturating_sub(last_move_ms) > 2_000 {
                                let target = [pos[0] + 1.0, pos[1], pos[2] + 1.0];
                                if bridge.goto(agent_id, target, approach_speed).await.is_ok() {
                                    last_move_ms = now;
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    eprintln!("[agent_bot] LLM error: {err}");
                    last_llm_ms = now;
                }
            }
        }
    }

    Ok(())
}

