use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use socketioxide::extract::{Data, SocketRef, State};
use socketioxide::{layer::SocketIoLayer, SocketIo};
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

#[derive(Clone, Default)]
struct ClientsState {
    inner: Arc<RwLock<HashMap<String, ClientRecord>>>,
    room_programs: Arc<RwLock<HashMap<String, RoomProgramRecord>>>,
}

impl ClientsState {
    fn new() -> Self {
        Self::default()
    }

    async fn insert_default(&self, id: String) {
        let mut guard = self.inner.write().await;
        guard.entry(id).or_insert_with(ClientRecord::default);
    }

    async fn update_user_data(
        &self,
        id: &str,
        data: UserDataPayload,
    ) -> (ClientInfo, Option<String>) {
        let mut guard = self.inner.write().await;
        let entry = guard
            .entry(id.to_string())
            .or_insert_with(ClientRecord::default);
        entry.info.avatar = data.avatar;
        entry.info.nickname = data.nickname;
        (entry.info.clone(), entry.room_id.clone())
    }

    async fn set_room(&self, id: &str, room_id: String) -> (Option<String>, ClientInfo) {
        let mut guard = self.inner.write().await;
        let entry = guard
            .entry(id.to_string())
            .or_insert_with(ClientRecord::default);
        let previous_room = entry.room_id.replace(room_id);
        (previous_room, entry.info.clone())
    }

    async fn clear_room(&self, id: &str, room_id: &str) -> Option<String> {
        let mut guard = self.inner.write().await;
        let entry = guard.get_mut(id)?;
        if entry.room_id.as_deref() != Some(room_id) {
            return None;
        }
        entry.motion = None;
        entry.room_id.take()
    }

    async fn update_move(
        &self,
        id: &str,
        position: Vec<f32>,
        rotation: f32,
        client_seq: Option<u64>,
    ) -> Option<(ClientInfo, String, u64)> {
        let mut guard = self.inner.write().await;
        let entry = guard
            .entry(id.to_string())
            .or_insert_with(ClientRecord::default);
        let room_id = entry.room_id.clone()?;
        if let Some(seq) = client_seq {
            if seq <= entry.last_client_move_seq {
                return None;
            }
            entry.last_client_move_seq = seq;
        }
        // Manual move overrides any server-driven motion.
        entry.motion = None;
        entry.info.position = position;
        entry.info.rotation = rotation;
        entry.server_move_seq = entry.server_move_seq.saturating_add(1);
        Some((entry.info.clone(), room_id, entry.server_move_seq))
    }

    async fn remove(&self, id: &str) -> Option<String> {
        let mut guard = self.inner.write().await;
        guard.remove(id).and_then(|rec| rec.room_id)
    }

    async fn snapshot_room(&self, room_id: &str) -> HashMap<String, ClientInfo> {
        let guard = self.inner.read().await;
        guard
            .iter()
            .filter(|(_, rec)| rec.room_id.as_deref() == Some(room_id))
            .map(|(id, rec)| (id.clone(), rec.info.clone()))
            .collect()
    }

    async fn get_with_room(&self, id: &str) -> Option<(ClientInfo, Option<String>)> {
        let guard = self.inner.read().await;
        guard
            .get(id)
            .map(|rec| (rec.info.clone(), rec.room_id.clone()))
    }

    async fn len(&self) -> usize {
        let guard = self.inner.read().await;
        guard.len()
    }

    async fn get_room_program(&self, room_id: &str) -> Option<(RoomProgramState, u64)> {
        let guard = self.room_programs.read().await;
        guard
            .get(room_id)
            .map(|rec| (rec.state.clone(), rec.revision))
    }

    async fn get_or_seed_room_program(
        &self,
        room_id: String,
        fallback_state: Option<RoomProgramState>,
    ) -> Option<(RoomProgramState, u64)> {
        let mut guard = self.room_programs.write().await;
        if let Some(rec) = guard.get(&room_id) {
            return Some((rec.state.clone(), rec.revision));
        }

        let state = fallback_state?;
        let revision = 1;
        guard.insert(
            room_id,
            RoomProgramRecord {
                state: state.clone(),
                revision,
            },
        );
        Some((state, revision))
    }

    async fn update_room_program(
        &self,
        room_id: String,
        state: RoomProgramState,
    ) -> (RoomProgramState, u64, bool) {
        let mut guard = self.room_programs.write().await;
        let entry = guard.entry(room_id).or_insert_with(|| RoomProgramRecord {
            state: state.clone(),
            revision: 0,
        });

        if state.updated_at < entry.state.updated_at {
            return (entry.state.clone(), entry.revision, false);
        }

        entry.revision = entry.revision.saturating_add(1);
        entry.state = state;
        (entry.state.clone(), entry.revision, true)
    }

    async fn set_goto(
        &self,
        id: &str,
        target: [f32; 3],
        speed: f32,
        rotation: Option<f32>,
    ) -> Option<String> {
        let mut guard = self.inner.write().await;
        let entry = guard
            .entry(id.to_string())
            .or_insert_with(ClientRecord::default);
        let room_id = entry.room_id.clone()?;

        entry.motion = Some(Motion { target, speed });
        if let Some(rot) = rotation {
            entry.info.rotation = rot;
        }
        Some(room_id)
    }

    async fn tick_motions(&self, dt_secs: f32) -> Vec<(String, MoveBroadcast)> {
        let mut out = Vec::new();
        let mut guard = self.inner.write().await;

        for (id, rec) in guard.iter_mut() {
            let Some(motion) = rec.motion.clone() else {
                continue;
            };
            let Some(room_id) = rec.room_id.clone() else {
                rec.motion = None;
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
                [
                    cur[0] + dx * inv * step,
                    cur[1] + dy * inv * step,
                    cur[2] + dz * inv * step,
                ]
            };

            rec.info.position = vec![next[0], next[1], next[2]];
            rec.server_move_seq = rec.server_move_seq.saturating_add(1);

            out.push((
                room_id,
                MoveBroadcast {
                    id: id.clone(),
                    position: rec.info.position.clone(),
                    rotation: rec.info.rotation,
                    avatar: rec.info.avatar.clone(),
                    nickname: rec.info.nickname.clone(),
                    server_seq: rec.server_move_seq,
                    server_time: now_millis(),
                    client_seq: None,
                },
            ));
        }

        out
    }
}

#[derive(Clone)]
struct ClientRecord {
    info: ClientInfo,
    motion: Option<Motion>,
    room_id: Option<String>,
    last_client_move_seq: u64,
    server_move_seq: u64,
}

impl Default for ClientRecord {
    fn default() -> Self {
        Self {
            info: ClientInfo::default(),
            motion: None,
            room_id: None,
            last_client_move_seq: 0,
            server_move_seq: 0,
        }
    }
}

#[derive(Clone)]
struct RoomProgramRecord {
    state: RoomProgramState,
    revision: u64,
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
#[serde(untagged)]
enum ChatMessageInput {
    Text(String),
    Object {
        message: String,
        #[allow(dead_code)]
        nickname: Option<String>,
    },
}

#[derive(Deserialize)]
struct MovePayload {
    position: Option<Vec<f32>>,
    rotation: Option<f32>,
    seq: Option<u64>,
    #[allow(dead_code)]
    sent_at: Option<u64>,
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
#[serde(rename_all = "camelCase")]
struct MoveBroadcast {
    id: String,
    position: Vec<f32>,
    rotation: f32,
    avatar: String,
    nickname: String,
    server_seq: u64,
    server_time: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_seq: Option<u64>,
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
    #[allow(dead_code)]
    server_revision: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomProgramBroadcast {
    room_id: String,
    state: RoomProgramState,
    server_revision: u64,
    server_time: u64,
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
    state: State<ClientsState>,
    Data(payload): Data<ChatMessageInput>,
) {
    let id = s.id.to_string();
    let Some((info, Some(room_id))) = state.get_with_room(&id).await else {
        return;
    };
    let message = match payload {
        ChatMessageInput::Text(message) => message,
        ChatMessageInput::Object { message, .. } => message,
    };
    let message = message.trim().to_string();
    if message.is_empty() {
        return;
    }
    tracing::info!(client_id = %id, message = %message, "chat message");
    let payload = ChatBroadcast {
        id,
        nickname: info.nickname,
        message,
    };
    if let Err(err) = s
        .within(space_room_name(&room_id))
        .emit("chat message", &payload)
        .await
    {
        tracing::warn!(?err, "failed to emit chat message");
    }
}

async fn on_set_user_data(
    s: SocketRef,
    state: State<ClientsState>,
    Data(payload): Data<UserDataPayload>,
) {
    let id = s.id.to_string();
    let (user_data, room_id) = state.update_user_data(&id, payload).await;
    let Some(room_id) = room_id else {
        return;
    };
    let payload = NewUserBroadcast { id, user_data };
    if let Err(err) = s
        .to(space_room_name(&room_id))
        .emit("new user", &payload)
        .await
    {
        tracing::warn!(?err, "failed to emit new user");
    }
}

async fn on_move(s: SocketRef, state: State<ClientsState>, Data(payload): Data<MovePayload>) {
    // Security: never trust a client-provided `id`. Otherwise one client can move another
    // player by spoofing their socket id.
    let id = s.id.to_string();
    let Some(position) = payload.position else {
        return;
    };
    let Some(position) = to_vec3(&position) else {
        return;
    };
    let rotation = payload.rotation.unwrap_or(0.0);
    if !rotation.is_finite() {
        return;
    }

    let Some((user, room_id, server_seq)) = state
        .update_move(
            &id,
            vec![position[0], position[1], position[2]],
            rotation,
            payload.seq,
        )
        .await
    else {
        return;
    };
    let payload = MoveBroadcast {
        id,
        position: user.position.clone(),
        rotation,
        avatar: user.avatar,
        nickname: user.nickname,
        server_seq,
        server_time: now_millis(),
        client_seq: payload.seq,
    };

    if let Err(err) = s
        .within(space_room_name(&room_id))
        .emit("move", &payload)
        .await
    {
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
    let _ = state.set_goto(&id, target, speed, payload.rotation).await;
}

async fn on_join_space(s: SocketRef, state: State<ClientsState>, Data(room_id): Data<String>) {
    let room_id = room_id.trim().to_string();
    if room_id.is_empty() {
        return;
    }
    let socket_room = space_room_name(&room_id);
    let id = s.id.to_string();
    let (previous_room, user_data) = state.set_room(&id, room_id.clone()).await;

    if let Some(previous_room) = previous_room.filter(|prev| prev != &room_id) {
        let previous_socket_room = space_room_name(&previous_room);
        s.leave(previous_socket_room.clone());
        if let Err(err) = s.to(previous_socket_room).emit("delete", &id).await {
            tracing::warn!(?err, "failed to emit delete on room switch");
        }
    }

    s.join(socket_room.clone());

    let clients = state.snapshot_room(&room_id).await;
    if let Err(err) = s.emit("existing clients", &clients) {
        tracing::warn!(?err, "failed to emit room clients");
    }

    if !user_data.nickname.is_empty() || !user_data.avatar.is_empty() {
        let payload = NewUserBroadcast {
            id: id.clone(),
            user_data,
        };
        if let Err(err) = s.to(socket_room.clone()).emit("new user", &payload).await {
            tracing::warn!(?err, "failed to emit new user on room join");
        }
    }

    if let Some((room_state, server_revision)) = state.get_room_program(&room_id).await {
        let payload = RoomProgramBroadcast {
            room_id,
            state: room_state,
            server_revision,
            server_time: now_millis(),
            source_client_id: None,
        };
        if let Err(err) = s.emit("room program state", &payload) {
            tracing::warn!(?err, "failed to emit room program state on join");
        }
    }
}

async fn on_leave_space(s: SocketRef, state: State<ClientsState>, Data(room_id): Data<String>) {
    let room_id = room_id.trim().to_string();
    if room_id.is_empty() {
        return;
    }
    let id = s.id.to_string();
    let Some(left_room) = state.clear_room(&id, &room_id).await else {
        return;
    };
    let socket_room = space_room_name(&left_room);
    s.leave(socket_room.clone());
    if let Err(err) = s.to(socket_room).emit("delete", &id).await {
        tracing::warn!(?err, "failed to emit delete on leave");
    }
}

async fn on_request_room_program(
    s: SocketRef,
    state: State<ClientsState>,
    Data(payload): Data<RoomProgramRequestPayload>,
) {
    let room_id = payload.room_id.trim().to_string();
    if room_id.is_empty() {
        return;
    }
    let id = s.id.to_string();
    let Some((_info, Some(current_room_id))) = state.get_with_room(&id).await else {
        return;
    };
    if current_room_id != room_id {
        return;
    }

    let Some((room_state, server_revision)) = state
        .get_or_seed_room_program(room_id.clone(), payload.fallback_state)
        .await
    else {
        return;
    };

    let payload = RoomProgramBroadcast {
        room_id,
        state: room_state,
        server_revision,
        server_time: now_millis(),
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
    let id = s.id.to_string();
    let Some((_info, Some(current_room_id))) = state.get_with_room(&id).await else {
        return;
    };
    if current_room_id != room_id {
        return;
    }

    let (room_state, server_revision, applied) = state
        .update_room_program(room_id.clone(), payload.state)
        .await;
    let payload = RoomProgramBroadcast {
        room_id: room_id.clone(),
        state: room_state,
        server_revision,
        server_time: now_millis(),
        source_client_id: Some(s.id.to_string()),
    };

    if applied {
        if let Err(err) = io
            .within(space_room_name(&room_id))
            .emit("room program state", &payload)
            .await
        {
            tracing::warn!(?err, "failed to emit room program state");
        }
    } else if let Err(err) = s.emit("room program state", &payload) {
        tracing::warn!(?err, "failed to emit stale room program state reply");
    }
}

async fn on_disconnect(s: SocketRef, io: SocketIo, state: State<ClientsState>) {
    let id = s.id.to_string();
    let room_id = state.remove(&id).await;
    let count = state.len().await;
    tracing::info!(client_id = %id, client_count = count, "client disconnected");
    if let Some(room_id) = room_id {
        if let Err(err) = io
            .within(space_room_name(&room_id))
            .emit("delete", &id)
            .await
        {
            tracing::warn!(?err, "failed to emit delete");
        }
    }
}

fn to_vec3(v: &[f32]) -> Option<[f32; 3]> {
    if v.len() != 3 {
        return None;
    }
    if !v.iter().all(|n| n.is_finite()) {
        return None;
    }
    Some([v[0], v[1], v[2]])
}

fn space_room_name(room_id: &str) -> String {
    format!("space:{room_id}")
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
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
            for (room_id, payload) in updates {
                if let Err(err) = io
                    .within(space_room_name(&room_id))
                    .emit("move", &payload)
                    .await
                {
                    tracing::warn!(?err, "failed to emit move (motion loop)");
                }
            }
        }
    });
}
