use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use socketioxide::extract::{Data, SocketRef, State};
use socketioxide::{layer::SocketIoLayer, SocketIo};
use tokio::sync::RwLock;

#[derive(Clone, Default)]
struct ClientsState {
    inner: Arc<RwLock<HashMap<String, ClientInfo>>>,
}

impl ClientsState {
    fn new() -> Self {
        Self::default()
    }

    async fn insert_default(&self, id: String) {
        let mut guard = self.inner.write().await;
        guard.entry(id).or_insert_with(ClientInfo::default);
    }

    async fn update_user_data(&self, id: &str, data: UserDataPayload) -> ClientInfo {
        let mut guard = self.inner.write().await;
        let entry = guard.entry(id.to_string()).or_insert_with(ClientInfo::default);
        entry.avatar = data.avatar;
        entry.nickname = data.nickname;
        entry.clone()
    }

    async fn update_move(&self, id: &str, position: Vec<f32>, rotation: f32) -> ClientInfo {
        let mut guard = self.inner.write().await;
        let entry = guard.entry(id.to_string()).or_insert_with(ClientInfo::default);
        entry.position = position;
        entry.rotation = rotation;
        entry.clone()
    }

    async fn get(&self, id: &str) -> Option<ClientInfo> {
        let guard = self.inner.read().await;
        guard.get(id).cloned()
    }

    async fn remove(&self, id: &str) {
        let mut guard = self.inner.write().await;
        guard.remove(id);
    }

    async fn snapshot(&self) -> HashMap<String, ClientInfo> {
        let guard = self.inner.read().await;
        guard.clone()
    }

    async fn len(&self) -> usize {
        let guard = self.inner.read().await;
        guard.len()
    }
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
#[serde(untagged)]
enum ChatMessagePayload {
    Text(String),
    Object { message: String },
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
#[serde(rename_all = "camelCase")]
struct ChatMessageBroadcast {
    id: String,
    nickname: String,
    message: String,
}

pub fn build_layer() -> (SocketIoLayer, SocketIo) {
    SocketIo::builder()
        .with_state(ClientsState::new())
        .build_layer()
}

pub fn register_handlers(io: &SocketIo) {
    io.ns("/", on_connect);
}

async fn on_connect(s: SocketRef, _io: SocketIo, state: State<ClientsState>) {
    let id = s.id.to_string();
    state.insert_default(id.clone()).await;
    let count = state.len().await;
    tracing::info!(client_id = %id, client_count = count, "client connected");

    // Only the joining socket needs the snapshot; broadcasting to everyone
    // would replace each client's state and briefly show newcomers at default [0,0,0].
    let clients = state.snapshot().await;
    if let Err(err) = s.emit("existing clients", &clients) {
        tracing::warn!(?err, "failed to emit existing clients");
    }

    s.on("chat message", on_chat_message);
    s.on("set user data", on_set_user_data);
    s.on("move", on_move);
    s.on_disconnect(on_disconnect);
}

async fn on_chat_message(
    s: SocketRef,
    io: SocketIo,
    state: State<ClientsState>,
    Data(payload): Data<ChatMessagePayload>,
) {
    let raw_message = match payload {
        ChatMessagePayload::Text(message) => message,
        ChatMessagePayload::Object { message } => message,
    };
    let message = raw_message.trim();
    if message.is_empty() {
        return;
    }

    let message = if message.len() > 200 {
        message.chars().take(200).collect::<String>()
    } else {
        message.to_string()
    };

    let id = s.id.to_string();
    let nickname = state
        .get(&id)
        .await
        .map(|client| client.nickname)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Unknown".to_string());

    tracing::info!(client_id = %id, message = %message, "chat message");
    let payload = ChatMessageBroadcast {
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

async fn on_disconnect(s: SocketRef, io: SocketIo, state: State<ClientsState>) {
    let id = s.id.to_string();
    state.remove(&id).await;
    let count = state.len().await;
    tracing::info!(client_id = %id, client_count = count, "client disconnected");
    if let Err(err) = io.emit("delete", &id).await {
        tracing::warn!(?err, "failed to emit delete");
    }
}
