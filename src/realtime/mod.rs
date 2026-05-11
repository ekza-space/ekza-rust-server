use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use socketioxide::extract::{Data, SocketRef, State};
use socketioxide::{layer::SocketIoLayer, SocketIo};
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

#[derive(Clone, Default)]
struct ClientsState {
    inner: Arc<RwLock<HashMap<String, ClientRecord>>>,
    room_programs: Arc<RwLock<HashMap<String, RoomProgramState>>>,
}

impl ClientsState {
    fn new() -> Self {
        Self::default()
    }

    async fn insert_default(&self, id: String) {
        let mut guard = self.inner.write().await;
        guard.entry(id).or_insert_with(ClientRecord::default);
    }

    async fn update_user_data(&self, id: &str, data: UserDataPayload) -> ClientInfo {
        let mut guard = self.inner.write().await;
        let entry = guard
            .entry(id.to_string())
            .or_insert_with(ClientRecord::default);
        entry.info.avatar = data.avatar;
        entry.info.nickname = data.nickname;
        entry.info.clone()
    }

    async fn update_move(
        &self,
        id: &str,
        position: Vec<f32>,
        rotation: f32,
    ) -> ClientInfo {
        let mut guard = self.inner.write().await;
        let entry = guard
            .entry(id.to_string())
            .or_insert_with(ClientRecord::default);
        // Manual move overrides any server-driven motion.
        entry.motion = None;
        entry.info.position = position;
        entry.info.rotation = rotation;
        entry.info.clone()
    }

    async fn remove(&self, id: &str) {
        let mut guard = self.inner.write().await;
        guard.remove(id);
    }

    async fn snapshot(&self) -> HashMap<String, ClientInfo> {
        let guard = self.inner.read().await;
        guard
            .iter()
            .map(|(id, rec)| (id.clone(), rec.info.clone()))
            .collect()
    }

    async fn get(&self, id: &str) -> Option<ClientInfo> {
        let guard = self.inner.read().await;
        guard.get(id).map(|rec| rec.info.clone())
    }

    async fn len(&self) -> usize {
        let guard = self.inner.read().await;
        guard.len()
    }

    async fn get_room_program(&self, room_id: &str) -> Option<RoomProgramState> {
        let guard = self.room_programs.read().await;
        guard.get(room_id).cloned()
    }

    async fn get_or_seed_room_program(
        &self,
        room_id: String,
        fallback_state: Option<RoomProgramState>,
    ) -> Option<RoomProgramState> {
        let mut guard = self.room_programs.write().await;
        if let Some(state) = guard.get(&room_id) {
            return Some(state.clone());
        }

        let state = fallback_state?;
        guard.insert(room_id, state.clone());
        Some(state)
    }

    async fn update_room_program(
        &self,
        room_id: String,
        state: RoomProgramState,
    ) -> RoomProgramState {
        let mut guard = self.room_programs.write().await;
        guard.insert(room_id, state.clone());
        state
    }

    async fn set_goto(
        &self,
        id: &str,
        target: [f32; 3],
        speed: f32,
        rotation: Option<f32>,
    ) {
        let mut guard = self.inner.write().await;
        let entry = guard
            .entry(id.to_string())
            .or_insert_with(ClientRecord::default);

        entry.motion = Some(Motion { target, speed });
        if let Some(rot) = rotation {
            entry.info.rotation = rot;
        }
    }

    async fn tick_motions(&self, dt_secs: f32) -> Vec<MoveBroadcast> {
        let mut out = Vec::new();
        let mut guard = self.inner.write().await;

        for (id, rec) in guard.iter_mut() {
            let Some(motion) = rec.motion.clone() else {
                continue;
            };

            // Current position (fallback if something weird got stored).
            let cur = to_vec3(&rec.info.position).unwrap_or([0.0, 0.0, 0.0]);
            let dx = motion.target[0] - cur[0];
            let dy = motion.target[1] - cur[1];
            let dz = motion.target[2] - cur[2];
            let dist = (dx * dx + dy * dy + dz * dz).sqrt();

            let step = (motion.speed.max(0.0)) * dt_secs.max(0.0);
            let next = if dist <= 1e-4 || step <= 1e-6 || dist <= step {
                // Arrived (or no movement configured).
                rec.motion = None;
                motion.target
            } else {
                let inv = 1.0 / dist;
                [cur[0] + dx * inv * step, cur[1] + dy * inv * step, cur[2] + dz * inv * step]
            };

            rec.info.position = vec![next[0], next[1], next[2]];

            out.push(MoveBroadcast {
                id: id.clone(),
                position: rec.info.position.clone(),
                rotation: rec.info.rotation,
                avatar: rec.info.avatar.clone(),
                nickname: rec.info.nickname.clone(),
            });
        }

        out
    }
}

#[derive(Clone)]
struct ClientRecord {
    info: ClientInfo,
    motion: Option<Motion>,
}

impl Default for ClientRecord {
    fn default() -> Self {
        Self {
            info: ClientInfo::default(),
            motion: None,
        }
    }
}

#[derive(Clone)]
struct Motion {
    target: [f32; 3],
    speed: f32,
}

#[derive(Clone, Serialize)]
struct ClientInfo {
    pub position: Vec<f32>,
    pub rotation: f32,
    pub avatar: String,
    pub nickname: String,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            position: vec![0.0, 0.0, 0.0],
            rotation: 0.0,
            avatar: String::new(),
            nickname: String::new(),
        }
    }
}

#[derive(Deserialize)]
struct UserDataPayload {
    avatar: String,
    nickname: String,
}

#[derive(Deserialize)]
struct MovePayload {
    position: Option<Vec<f32>>,
    rotation: Option<f32>,
}

#[derive(Deserialize)]
struct GotoPayload {
    position: Option<Vec<f32>>,
    speed: Option<f32>,
    rotation: Option<f32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NewUserBroadcast {
    id: String,
    user_data: ClientInfo,
}

#[derive(Serialize)]
struct MoveBroadcast {
    id: String,
    position: Vec<f32>,
    rotation: f32,
    avatar: String,
    nickname: String,
}

#[derive(Serialize)]
struct ChatBroadcast {
    id: String,
    nickname: String,
    message: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomAssetInstance {
    id: String,
    kind: String,
    label: String,
    position: Vec<f32>,
    rotation: Vec<f32>,
    scale: Vec<f32>,
    color: String,
    link_url: Option<String>,
    open_in_new_tab: Option<bool>,
    model_data_url: Option<String>,
    model_file_name: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomProgramState {
    version: u8,
    environment_id: String,
    objects: Vec<RoomAssetInstance>,
    updated_at: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomProgramRequestPayload {
    room_id: String,
    fallback_state: Option<RoomProgramState>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomProgramUpdatePayload {
    room_id: String,
    state: RoomProgramState,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomProgramBroadcast {
    room_id: String,
    state: RoomProgramState,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_client_id: Option<String>,
}

pub fn build_layer() -> (SocketIoLayer, SocketIo) {
    let state = ClientsState::new();
    let (layer, io) = SocketIo::builder().with_state(state.clone()).build_layer();
    start_motion_loop(io.clone(), state);
    (layer, io)
}

pub fn register_handlers(io: &SocketIo) {
    io.ns("/", on_connect);
}

async fn on_connect(s: SocketRef, _io: SocketIo, state: State<ClientsState>) {
    let id = s.id.to_string();
    state.insert_default(id.clone()).await;
    let count = state.len().await;
    tracing::info!(client_id = %id, client_count = count, "client connected");

    let clients = state.snapshot().await;
    // Only send the snapshot to the newly connected client.
    // Broadcasting it to all clients causes everyone to briefly see the new socket
    // with empty nickname/avatar ("undefined sphere") until `set user data` arrives.
    if let Err(err) = s.emit("existing clients", &clients) {
        tracing::warn!(?err, "failed to emit existing clients");
    }

    s.on("chat message", on_chat_message);
    s.on("set user data", on_set_user_data);
    s.on("move", on_move);
    s.on("goto", on_goto);
    s.on("join-space", on_join_space);
    s.on("leave-space", on_leave_space);
    s.on("request room program", on_request_room_program);
    s.on("room program update", on_room_program_update);
    s.on_disconnect(on_disconnect);
}

async fn on_chat_message(
    s: SocketRef,
    io: SocketIo,
    state: State<ClientsState>,
    Data(message): Data<String>,
) {
    let id = s.id.to_string();
    let nickname = state
        .get(&id)
        .await
        .map(|info| info.nickname)
        .unwrap_or_default();
    tracing::info!(client_id = %id, message = %message, "chat message");
    let payload = ChatBroadcast {
        id,
        nickname,
        message,
    };
    if let Err(err) = io.emit("chat message", &payload).await {
        tracing::warn!(?err, "failed to emit chat message");
    }
}

async fn on_set_user_data(
    s: SocketRef,
    io: SocketIo,
    state: State<ClientsState>,
    Data(payload): Data<UserDataPayload>,
) {
    let id = s.id.to_string();
    let user_data = state.update_user_data(&id, payload).await;
    let payload = NewUserBroadcast { id, user_data };
    if let Err(err) = io.emit("new user", &payload).await {
        tracing::warn!(?err, "failed to emit new user");
    }
}

async fn on_move(
    s: SocketRef,
    io: SocketIo,
    state: State<ClientsState>,
    Data(payload): Data<MovePayload>,
) {
    // Security: never trust a client-provided `id`. Otherwise one client can move another
    // player by spoofing their socket id.
    let id = s.id.to_string();
    let Some(position) = payload.position else {
        return;
    };
    let rotation = payload.rotation.unwrap_or(0.0);

    let user = state.update_move(&id, position.clone(), rotation).await;
    let payload = MoveBroadcast {
        id,
        position,
        rotation,
        avatar: user.avatar,
        nickname: user.nickname,
    };

    if let Err(err) = io.emit("move", &payload).await {
        tracing::warn!(?err, "failed to emit move");
    }
}

async fn on_goto(s: SocketRef, state: State<ClientsState>, Data(payload): Data<GotoPayload>) {
    let id = s.id.to_string();
    let Some(position) = payload.position else {
        return;
    };
    let Some(target) = to_vec3(&position) else {
        return;
    };

    // Units are "world units per second".
    let speed = payload.speed.unwrap_or(3.0).clamp(0.1, 50.0);
    state
        .set_goto(&id, target, speed, payload.rotation)
        .await;
}

async fn on_join_space(
    s: SocketRef,
    state: State<ClientsState>,
    Data(room_id): Data<String>,
) {
    let room_id = room_id.trim().to_string();
    if room_id.is_empty() {
        return;
    }

    if let Some(room_state) = state.get_room_program(&room_id).await {
        let payload = RoomProgramBroadcast {
            room_id,
            state: room_state,
            source_client_id: None,
        };
        if let Err(err) = s.emit("room program state", &payload) {
            tracing::warn!(?err, "failed to emit room program state on join");
        }
    }
}

async fn on_leave_space(_s: SocketRef, Data(_room_id): Data<String>) {}

async fn on_request_room_program(
    s: SocketRef,
    state: State<ClientsState>,
    Data(payload): Data<RoomProgramRequestPayload>,
) {
    let room_id = payload.room_id.trim().to_string();
    if room_id.is_empty() {
        return;
    }

    let Some(room_state) = state
        .get_or_seed_room_program(room_id.clone(), payload.fallback_state)
        .await
    else {
        return;
    };

    let payload = RoomProgramBroadcast {
        room_id,
        state: room_state,
        source_client_id: None,
    };
    if let Err(err) = s.emit("room program state", &payload) {
        tracing::warn!(?err, "failed to emit requested room program state");
    }
}

async fn on_room_program_update(
    s: SocketRef,
    io: SocketIo,
    state: State<ClientsState>,
    Data(payload): Data<RoomProgramUpdatePayload>,
) {
    let room_id = payload.room_id.trim().to_string();
    if room_id.is_empty() {
        return;
    }

    let room_state = state
        .update_room_program(room_id.clone(), payload.state)
        .await;
    let payload = RoomProgramBroadcast {
        room_id,
        state: room_state,
        source_client_id: Some(s.id.to_string()),
    };

    if let Err(err) = io.emit("room program state", &payload).await {
        tracing::warn!(?err, "failed to emit room program state");
    }
}

async fn on_disconnect(s: SocketRef, io: SocketIo, state: State<ClientsState>) {
    let id = s.id.to_string();
    state.remove(&id).await;
    let count = state.len().await;
    tracing::info!(client_id = %id, client_count = count, "client disconnected");
    if let Err(err) = io.emit("delete", &id).await {
        tracing::warn!(?err, "failed to emit delete");
    }
}

fn to_vec3(v: &[f32]) -> Option<[f32; 3]> {
    if v.len() != 3 {
        return None;
    }
    Some([v[0], v[1], v[2]])
}

fn start_motion_loop(io: SocketIo, state: ClientsState) {
    tokio::spawn(async move {
        // 20Hz server-side interpolation.
        let mut ticker = interval(Duration::from_millis(50));
        let dt_secs = 0.05_f32;
        loop {
            ticker.tick().await;
            let updates = state.tick_motions(dt_secs).await;
            if updates.is_empty() {
                continue;
            }
            for payload in updates {
                if let Err(err) = io.emit("move", &payload).await {
                    tracing::warn!(?err, "failed to emit move (motion loop)");
                }
            }
        }
    });
}
