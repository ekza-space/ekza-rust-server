use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use futures_util::FutureExt;
use rust_socketio::asynchronous::Client;
use rust_socketio::asynchronous::ClientBuilder;
use rust_socketio::Payload;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio::time::{timeout, Duration};

// This binary is a "bridge" process:
// - Connects to ekza realtime via Socket.IO (same as the browser client)
// - Exposes a small REST API so an external agent can:
//   - read current world state / recent events
//   - send commands (move/chat/set user data) via HTTP
// - Can host multiple agent connections at once (each agent is its own socket.io client)
//
// Usage:
//   cargo run -p server --bin agent_bridge -- \
//     --ekza-url http://127.0.0.1:3001 \
//     --bind 127.0.0.1:5055 \
//     --nickname Agent
//
// REST:
//   GET  /health
//   GET  /api/v1/state
//   GET  /api/v1/events?after=<id>&limit=<n>
//   POST /api/v1/command/chat      { "message": "hi" }
//   POST /api/v1/command/move      { "position": [x,y,z], "rotation": 0.0 }
//   POST /api/v1/command/goto      { "position": [x,y,z], "speed": 3.0 }
//   POST /api/v1/command/user_data { "nickname": "...", "avatar": "..." }
//   POST /api/v1/command/emit      { "event": "move", "data": {...} }
//
// Multi-agent:
//   POST   /api/v1/agents                      { "nickname": "...", "avatar": "..." } -> { "agentId": 1 }
//   GET    /api/v1/agents                      -> [{ "agentId": 1, ... }]
//   GET    /api/v1/agents/{agentId}            -> { "agentId": 1, ... }
//   DELETE /api/v1/agents/{agentId}            -> 204
//   POST   /api/v1/agents/{agentId}/command/... -> same as /api/v1/command/...

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ClientInfo {
    pub position: Vec<f32>,
    pub rotation: f32,
    pub avatar: String,
    pub nickname: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BridgeEvent {
    ExistingClients { clients: HashMap<String, ClientInfo> },
    NewUser { id: String, user_data: ClientInfo },
    Delete { id: String },
    Move {
        id: String,
        position: Vec<f32>,
        rotation: f32,
        avatar: String,
        nickname: String,
    },
    ChatMessage {
        id: String,
        nickname: String,
        message: String,
    },
    Raw { event: String, data: Value },
}

#[derive(Debug, Clone, Serialize)]
struct EventEnvelope {
    id: u64,
    ts_ms: u64,
    event: BridgeEvent,
}

#[derive(Debug, Default)]
struct WorldState {
    clients: HashMap<String, ClientInfo>,
    last_chat: VecDeque<BridgeEvent>,
}

#[derive(Clone)]
struct SharedState {
    world: Arc<RwLock<WorldState>>,
    events: Arc<RwLock<VecDeque<EventEnvelope>>>,
    next_event_id: Arc<RwLock<u64>>,
}

#[derive(Clone)]
struct AppState {
    ekza_url: String,
    // Observer socket: receives world/events. Also used as the "default agent" socket for
    // the legacy /api/v1/command/* endpoints.
    socket: Arc<Client>,
    shared: SharedState,
    agents: Arc<RwLock<HashMap<u64, AgentEntry>>>,
    next_agent_id: Arc<RwLock<u64>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn push_event(shared: &SharedState, event: BridgeEvent) {
    let mut id_guard = shared.next_event_id.write().await;
    let id = *id_guard;
    *id_guard = id.saturating_add(1);
    drop(id_guard);

    let mut events = shared.events.write().await;
    events.push_back(EventEnvelope {
        id,
        ts_ms: now_ms(),
        event,
    });
    // bound memory
    while events.len() > 2_000 {
        events.pop_front();
    }
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentInfo {
    agent_id: u64,
    nickname: String,
    avatar: String,
    created_ts_ms: u64,
}

#[derive(Clone)]
struct AgentEntry {
    socket: Arc<Client>,
    info: AgentInfo,
}

async fn connect_socket(
    ekza_url: &str,
    nickname: &str,
    avatar: &str,
    shared: SharedState,
) -> Result<Client, Box<dyn Error>> {
    let nickname = nickname.to_string();
    let avatar = avatar.to_string();

    // NOTE: callbacks run on the socket client background task.
    let socket = ClientBuilder::new(ekza_url.to_string())
        .on("existing clients", {
            let shared = shared.clone();
            move |payload: Payload, _socket: Client| {
                let shared = shared.clone();
                async move {
                    if let Some(v) = payload_first_json(&payload) {
                        if let Ok(map) =
                            serde_json::from_value::<HashMap<String, ClientInfo>>(v.clone())
                        {
                            {
                                let mut world = shared.world.write().await;
                                world.clients = map.clone();
                            }
                            push_event(&shared, BridgeEvent::ExistingClients { clients: map }).await;
                        } else {
                            push_event(
                                &shared,
                                BridgeEvent::Raw {
                                    event: "existing clients".to_string(),
                                    data: v,
                                },
                            )
                            .await;
                        }
                    }
                }
                .boxed()
            }
        })
        .on("new user", {
            let shared = shared.clone();
            move |payload: Payload, _socket: Client| {
                let shared = shared.clone();
                async move {
                    if let Some(v) = payload_first_json(&payload) {
                        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let user_data = v
                            .get("userData")
                            .cloned()
                            .and_then(|u| serde_json::from_value::<ClientInfo>(u).ok())
                            .unwrap_or_default();
                        if !id.is_empty() {
                            {
                                let mut world = shared.world.write().await;
                                world.clients.insert(id.clone(), user_data.clone());
                            }
                            push_event(&shared, BridgeEvent::NewUser { id, user_data }).await;
                            return;
                        }
                        push_event(
                            &shared,
                            BridgeEvent::Raw {
                                event: "new user".to_string(),
                                data: v,
                            },
                        )
                        .await;
                    }
                }
                .boxed()
            }
        })
        .on("delete", {
            let shared = shared.clone();
            move |payload: Payload, _socket: Client| {
                let shared = shared.clone();
                async move {
                    if let Some(v) = payload_first_json(&payload) {
                        let id = v.as_str().unwrap_or("").to_string();
                        if !id.is_empty() {
                            {
                                let mut world = shared.world.write().await;
                                world.clients.remove(&id);
                            }
                            push_event(&shared, BridgeEvent::Delete { id }).await;
                            return;
                        }
                        push_event(
                            &shared,
                            BridgeEvent::Raw {
                                event: "delete".to_string(),
                                data: v,
                            },
                        )
                        .await;
                    }
                }
                .boxed()
            }
        })
        .on("move", {
            let shared = shared.clone();
            move |payload: Payload, _socket: Client| {
                let shared = shared.clone();
                async move {
                    if let Some(v) = payload_first_json(&payload) {
                        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let position = v
                            .get("position")
                            .cloned()
                            .and_then(|p| serde_json::from_value::<Vec<f32>>(p).ok())
                            .unwrap_or_default();
                        let rotation =
                            v.get("rotation").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
                        let avatar = v.get("avatar").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let nickname = v
                            .get("nickname")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();

                        if !id.is_empty() && position.len() == 3 {
                            {
                                let mut world = shared.world.write().await;
                                let entry = world.clients.entry(id.clone()).or_default();
                                entry.position = position.clone();
                                entry.rotation = rotation;
                                if !avatar.is_empty() {
                                    entry.avatar = avatar.clone();
                                }
                                if !nickname.is_empty() {
                                    entry.nickname = nickname.clone();
                                }
                            }
                            push_event(
                                &shared,
                                BridgeEvent::Move {
                                    id,
                                    position,
                                    rotation,
                                    avatar,
                                    nickname,
                                },
                            )
                            .await;
                            return;
                        }

                        push_event(
                            &shared,
                            BridgeEvent::Raw {
                                event: "move".to_string(),
                                data: v,
                            },
                        )
                        .await;
                    }
                }
                .boxed()
            }
        })
        .on("chat message", {
            let shared = shared.clone();
            move |payload: Payload, _socket: Client| {
                let shared = shared.clone();
                async move {
                    if let Some(v) = payload_first_json(&payload) {
                        let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let nickname = v
                            .get("nickname")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let message = v
                            .get("message")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();

                        if !message.is_empty() {
                            let ev = BridgeEvent::ChatMessage {
                                id,
                                nickname,
                                message,
                            };
                            {
                                let mut world = shared.world.write().await;
                                world.last_chat.push_back(ev.clone());
                                while world.last_chat.len() > 200 {
                                    world.last_chat.pop_front();
                                }
                            }
                            push_event(&shared, ev).await;
                            return;
                        }

                        push_event(
                            &shared,
                            BridgeEvent::Raw {
                                event: "chat message".to_string(),
                                data: v,
                            },
                        )
                        .await;
                    }
                }
                .boxed()
            }
        })
        .on("error", {
            let shared = shared.clone();
            move |payload: Payload, _socket: Client| {
                let shared = shared.clone();
                async move {
                    if let Some(v) = payload_first_json(&payload) {
                        push_event(
                            &shared,
                            BridgeEvent::Raw {
                                event: "error".to_string(),
                                data: v,
                            },
                        )
                        .await;
                    }
                }
                .boxed()
            }
        })
        .connect()
        .await?;

    // Identify this client on the ekza realtime server.
    socket
        .emit(
            "set user data",
            json!({
                "nickname": nickname,
                "avatar": avatar,
            }),
        )
        .await?;

    Ok(socket)
}

async fn connect_agent_socket(
    ekza_url: &str,
    nickname: &str,
    avatar: &str,
) -> Result<Client, Box<dyn Error>> {
    // This socket is for emitting commands (move/chat/etc). We keep callbacks minimal to avoid
    // duplicating events for every agent connection.
    let socket = ClientBuilder::new(ekza_url.to_string())
        .on("error", |_payload: Payload, _socket: Client| async move {}.boxed())
        .connect()
        .await?;

    socket
        .emit(
            "set user data",
            json!({
                "nickname": nickname,
                "avatar": avatar,
            }),
        )
        .await?;

    Ok(socket)
}

#[derive(Serialize)]
struct StateResponse {
    ts_ms: u64,
    clients: HashMap<String, ClientInfo>,
    last_chat: Vec<BridgeEvent>,
}

async fn health() -> &'static str {
    "ok"
}

async fn get_state(State(state): State<AppState>) -> Json<StateResponse> {
    let world = state.shared.world.read().await;
    Json(StateResponse {
        ts_ms: now_ms(),
        clients: world.clients.clone(),
        last_chat: world.last_chat.iter().cloned().collect(),
    })
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<u64>,
    limit: Option<usize>,
}

async fn get_events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> Json<Vec<EventEnvelope>> {
    let after = q.after.unwrap_or(0);
    let limit = q.limit.unwrap_or(200).clamp(1, 2_000);

    let events = state.shared.events.read().await;
    let mut out = Vec::with_capacity(limit);
    for ev in events.iter() {
        if ev.id > after {
            out.push(ev.clone());
            if out.len() >= limit {
                break;
            }
        }
    }
    Json(out)
}

#[derive(Deserialize)]
struct CreateAgentRequest {
    nickname: String,
    #[serde(default)]
    avatar: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateAgentResponse {
    agent_id: u64,
}

async fn create_agent(
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<Json<CreateAgentResponse>, (StatusCode, String)> {
    let nickname = req.nickname.trim();
    if nickname.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "nickname is empty".to_string()));
    }

    let mut id_guard = state.next_agent_id.write().await;
    let agent_id = *id_guard;
    *id_guard = agent_id.saturating_add(1);
    drop(id_guard);

    let socket = connect_agent_socket(
        &state.ekza_url,
        nickname,
        req.avatar.as_str(),
    )
    .await
    .map_err(|e| (StatusCode::BAD_GATEWAY, format!("connect agent socket failed: {e:?}")))?;

    let info = AgentInfo {
        agent_id,
        nickname: nickname.to_string(),
        avatar: req.avatar,
        created_ts_ms: now_ms(),
    };

    let mut agents = state.agents.write().await;
    agents.insert(
        agent_id,
        AgentEntry {
            socket: Arc::new(socket),
            info,
        },
    );

    Ok(Json(CreateAgentResponse { agent_id }))
}

async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentInfo>> {
    let agents = state.agents.read().await;
    let mut out = agents.values().map(|a| a.info.clone()).collect::<Vec<_>>();
    out.sort_by_key(|a| a.agent_id);
    Json(out)
}

async fn get_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<u64>,
) -> Result<Json<AgentInfo>, (StatusCode, String)> {
    let agents = state.agents.read().await;
    let Some(agent) = agents.get(&agent_id) else {
        return Err((StatusCode::NOT_FOUND, "agent not found".to_string()));
    };
    Ok(Json(agent.info.clone()))
}

async fn delete_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<u64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let agent = {
        let mut agents = state.agents.write().await;
        agents.remove(&agent_id)
    };

    let Some(agent) = agent else {
        return Err((StatusCode::NOT_FOUND, "agent not found".to_string()));
    };

    let _ = timeout(Duration::from_millis(800), agent.socket.disconnect()).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn with_agent<F, Fut, T>(
    state: &AppState,
    agent_id: u64,
    f: F,
) -> Result<T, (StatusCode, String)>
where
    F: FnOnce(Arc<Client>) -> Fut,
    Fut: std::future::Future<Output = Result<T, (StatusCode, String)>>,
{
    let socket = {
        let agents = state.agents.read().await;
        let Some(agent) = agents.get(&agent_id) else {
            return Err((StatusCode::NOT_FOUND, "agent not found".to_string()));
        };
        agent.socket.clone()
    };
    f(socket).await
}

async fn agent_post_chat(
    State(state): State<AppState>,
    Path(agent_id): Path<u64>,
    Json(cmd): Json<ChatCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    with_agent(&state, agent_id, |socket| async move {
        let msg = cmd.message.trim();
        if msg.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "message is empty".to_string()));
        }
        socket
            .emit("chat message", msg)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn agent_post_move(
    State(state): State<AppState>,
    Path(agent_id): Path<u64>,
    Json(cmd): Json<MoveCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    with_agent(&state, agent_id, |socket| async move {
        socket
            .emit(
                "move",
                json!({
                    "position": cmd.position,
                    "rotation": cmd.rotation.unwrap_or(0.0),
                }),
            )
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct GotoCommand {
    position: [f32; 3],
    speed: Option<f32>,
    rotation: Option<f32>,
}

async fn agent_post_goto(
    State(state): State<AppState>,
    Path(agent_id): Path<u64>,
    Json(cmd): Json<GotoCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    with_agent(&state, agent_id, |socket| async move {
        socket
            .emit(
                "goto",
                json!({
                    "position": cmd.position,
                    "speed": cmd.speed,
                    "rotation": cmd.rotation,
                }),
            )
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn agent_post_user_data(
    State(state): State<AppState>,
    Path(agent_id): Path<u64>,
    Json(cmd): Json<UserDataCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    with_agent(&state, agent_id, |socket| async move {
        let nickname = cmd.nickname.trim();
        if nickname.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "nickname is empty".to_string()));
        }
        socket
            .emit(
                "set user data",
                json!({
                    "nickname": nickname,
                    "avatar": cmd.avatar,
                }),
            )
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

async fn agent_post_emit(
    State(state): State<AppState>,
    Path(agent_id): Path<u64>,
    Json(cmd): Json<EmitCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    with_agent(&state, agent_id, |socket| async move {
        let event = cmd.event.trim();
        if event.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "event is empty".to_string()));
        }
        socket
            .emit(event, cmd.data)
            .await
            .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
struct ChatCommand {
    message: String,
}

async fn post_chat(
    State(state): State<AppState>,
    Json(cmd): Json<ChatCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    let msg = cmd.message.trim();
    if msg.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "message is empty".to_string()));
    }

    state
        .socket
        .emit("chat message", msg)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MoveCommand {
    position: [f32; 3],
    rotation: Option<f32>,
}

async fn post_move(
    State(state): State<AppState>,
    Json(cmd): Json<MoveCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .socket
        .emit(
            "move",
            json!({
                "position": cmd.position,
                "rotation": cmd.rotation.unwrap_or(0.0),
            }),
        )
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn post_goto(
    State(state): State<AppState>,
    Json(cmd): Json<GotoCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    state
        .socket
        .emit(
            "goto",
            json!({
                "position": cmd.position,
                "speed": cmd.speed,
                "rotation": cmd.rotation,
            }),
        )
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UserDataCommand {
    nickname: String,
    avatar: String,
}

async fn post_user_data(
    State(state): State<AppState>,
    Json(cmd): Json<UserDataCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    let nickname = cmd.nickname.trim();
    if nickname.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "nickname is empty".to_string()));
    }
    state
        .socket
        .emit(
            "set user data",
            json!({
                "nickname": nickname,
                "avatar": cmd.avatar,
            }),
        )
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct EmitCommand {
    event: String,
    data: Value,
}

async fn post_emit(
    State(state): State<AppState>,
    Json(cmd): Json<EmitCommand>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = cmd.event.trim();
    if event.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "event is empty".to_string()));
    }
    state
        .socket
        .emit(event, cmd.data)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("socket emit failed: {e:?}")))?;
    Ok(StatusCode::NO_CONTENT)
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load local env file if present (do not require it).
    let _ = dotenvy::dotenv();

    // Env fallback:
    //   EKZA_URL, EKZA_NICKNAME, EKZA_AVATAR, BRIDGE_BIND
    let args: Vec<String> = std::env::args().collect();

    let ekza_url = get_arg_value(&args, "ekza-url")
        .unwrap_or_else(|| env_or_default("EKZA_URL", "http://127.0.0.1:3001"));
    let bind = get_arg_value(&args, "bind")
        .unwrap_or_else(|| env_or_default("BRIDGE_BIND", "127.0.0.1:5055"));
    let nickname = get_arg_value(&args, "nickname")
        .unwrap_or_else(|| env_or_default("EKZA_NICKNAME", "Agent"));
    let avatar =
        get_arg_value(&args, "avatar").unwrap_or_else(|| env_or_default("EKZA_AVATAR", ""));

    let bind_addr: SocketAddr = bind.parse()?;

    let shared = SharedState {
        world: Arc::new(RwLock::new(WorldState::default())),
        events: Arc::new(RwLock::new(VecDeque::new())),
        next_event_id: Arc::new(RwLock::new(1)),
    };

    // Connect observer/default-agent socket first (so REST starts "ready").
    let socket = connect_socket(&ekza_url, &nickname, &avatar, shared.clone()).await?;
    let app_state = AppState {
        ekza_url: ekza_url.clone(),
        socket: Arc::new(socket),
        shared: shared.clone(),
        agents: Arc::new(RwLock::new(HashMap::new())),
        next_agent_id: Arc::new(RwLock::new(1)),
    };

    // Record a startup event for the REST consumer.
    push_event(
        &shared,
        BridgeEvent::Raw {
            event: "bridge_started".to_string(),
            data: json!({
                "ekza_url": ekza_url,
                "bind": bind,
                "nickname": nickname,
            }),
        },
    )
    .await;

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/state", get(get_state))
        .route("/api/v1/events", get(get_events))
        // Multi-agent management.
        .route("/api/v1/agents", post(create_agent).get(list_agents))
        .route("/api/v1/agents/{agent_id}", get(get_agent).delete(delete_agent))
        .route("/api/v1/agents/{agent_id}/command/chat", post(agent_post_chat))
        .route("/api/v1/agents/{agent_id}/command/move", post(agent_post_move))
        .route("/api/v1/agents/{agent_id}/command/goto", post(agent_post_goto))
        .route(
            "/api/v1/agents/{agent_id}/command/user_data",
            post(agent_post_user_data),
        )
        .route("/api/v1/agents/{agent_id}/command/emit", post(agent_post_emit))
        // Legacy "default agent" commands (use observer socket).
        .route("/api/v1/command/chat", post(post_chat))
        .route("/api/v1/command/move", post(post_move))
        .route("/api/v1/command/goto", post(post_goto))
        .route("/api/v1/command/user_data", post(post_user_data))
        .route("/api/v1/command/emit", post(post_emit))
        .with_state(app_state.clone());

    eprintln!(
        "[agent_bridge] ekza={} bind={} nickname={}",
        ekza_url, bind_addr, nickname
    );

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            // best-effort: don't hang shutdown forever
            let _ = timeout(Duration::from_millis(800), app_state.socket.disconnect()).await;
            let agents = {
                let mut guard = app_state.agents.write().await;
                guard.drain().map(|(_, entry)| entry.socket).collect::<Vec<_>>()
            };
            for s in agents {
                let _ = timeout(Duration::from_millis(800), s.disconnect()).await;
            }
        })
        .await?;

    Ok(())
}

