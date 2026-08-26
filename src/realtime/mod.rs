//! Socket.IO realtime: presence, chat, movement and per-space room state.
//!
//! Trust model
//! - Identity = wallet pubkey proven by signing a server nonce (`auth`).
//!   Unauthenticated sockets may look, move and chat; they cannot edit.
//! - Room ids are space ids `1..=Config.total_spaces` read from the chain.
//! - Room state writes require the caller to be the current NFT holder, an
//!   on-chain editor, or a configured moderator — resolved via RPC and cached.
//! - Writes are optimistic-concurrency: the client echoes the `serverRevision`
//!   it last saw; a mismatch is rejected with the current state.
//! - Every applied write is persisted before it is broadcast.

mod validate;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use socketioxide::extract::{SocketRef, State, TryData};
use socketioxide::socket::Sid;
use socketioxide::{layer::SocketIoLayer, SocketIo};
use solana_pubkey::Pubkey;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

use crate::auth;
use crate::chain::{ChainClient, ChainError};
use crate::config::Config;
use crate::limits::ClientLimits;
use crate::store::RoomStore;

pub use validate::{validate_room_state, RoomAssetInstance, RoomProgramState, ValidationError};

/// Payload budget for socket.io (applies to the polling transport; websocket
/// frames are bounded by [`validate`] limits after decode).
pub const MAX_PAYLOAD_BYTES: u64 = 256 * 1024;
const WORLD_BOUND_XZ: f32 = 2_000.0;
const WORLD_BOUND_Y: f32 = 500.0;
const MAX_CHAT_LEN: usize = 500;
const MAX_NICKNAME_LEN: usize = 32;
const MAX_AVATAR_LEN: usize = 512;
const MAX_PRESENCE_TOKEN_LEN: usize = 128;
const MIN_AVATAR_HEIGHT_SCALE: f32 = 0.5;
const MAX_AVATAR_HEIGHT_SCALE: f32 = 2.0;

// ------------------------------------------------------------------- state

#[derive(Clone)]
struct ClientsState {
    inner: Arc<RwLock<HashMap<String, ClientRecord>>>,
    /// Serializes logical presence changes with Socket.IO adapter membership.
    presence_transition: Arc<RwLock<()>>,
    room_programs: Arc<RwLock<HashMap<u32, RoomProgramRecord>>>,
    chain: ChainClient,
    store: RoomStore,
    moderators: Arc<Vec<Pubkey>>,
}

struct ClientRecord {
    info: ClientInfo,
    /// Opaque browser secret used only to collapse duplicate tabs in one room.
    /// It is deliberately never included in outbound presence payloads.
    presence_token: Option<String>,
    motion: Option<Motion>,
    room_id: Option<u32>,
    latest_claim_id: u64,
    last_client_move_seq: u64,
    server_move_seq: u64,
    wallet: Option<Pubkey>,
    nonce: String,
    limits: ClientLimits,
}

impl ClientRecord {
    fn new() -> Self {
        Self {
            info: ClientInfo::default(),
            presence_token: None,
            motion: None,
            room_id: None,
            latest_claim_id: 0,
            last_client_move_seq: 0,
            server_move_seq: 0,
            wallet: None,
            nonce: auth::new_nonce(),
            limits: ClientLimits::default(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct RoomProgramRecord {
    state: RoomProgramState,
    revision: u64,
    #[serde(default)]
    updated_by: Option<String>,
    #[serde(default)]
    updated_at_ms: u64,
}

#[derive(Clone)]
struct Motion {
    target: [f32; 3],
    speed: f32,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientInfo {
    pub position: Vec<f32>,
    pub rotation: f32,
    pub avatar: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_height_scale: Option<f32>,
    pub nickname: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wallet: Option<String>,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            position: vec![0.0, 0.0, 0.0],
            rotation: 0.0,
            avatar: String::new(),
            avatar_height_scale: None,
            nickname: String::new(),
            wallet: None,
        }
    }
}

enum Limit {
    Chat,
    Moves,
    RoomUpdates,
    UserData,
    Auth,
    Joins,
}

impl ClientsState {
    fn new(chain: ChainClient, store: RoomStore, moderators: Vec<Pubkey>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            presence_transition: Arc::new(RwLock::new(())),
            room_programs: Arc::new(RwLock::new(HashMap::new())),
            chain,
            store,
            moderators: Arc::new(moderators),
        }
    }

    async fn insert_default(&self, id: String) -> String {
        let mut guard = self.inner.write().await;
        let rec = guard.entry(id).or_insert_with(ClientRecord::new);
        rec.nonce.clone()
    }

    /// Spend one token of `which` for client `id`. `false` = rate limited.
    async fn allow(&self, id: &str, which: Limit) -> bool {
        let mut guard = self.inner.write().await;
        let Some(rec) = guard.get_mut(id) else {
            return false;
        };
        let bucket = match which {
            Limit::Chat => &mut rec.limits.chat,
            Limit::Moves => &mut rec.limits.moves,
            Limit::RoomUpdates => &mut rec.limits.room_updates,
            Limit::UserData => &mut rec.limits.user_data,
            Limit::Auth => &mut rec.limits.auth,
            Limit::Joins => &mut rec.limits.joins,
        };
        bucket.try_take()
    }

    async fn authenticate(
        &self,
        id: &str,
        pubkey_b58: &str,
        signature_b58: &str,
    ) -> Result<(Pubkey, Option<u32>), auth::AuthError> {
        let mut guard = self.inner.write().await;
        let rec = guard.get_mut(id).ok_or(auth::AuthError::Mismatch)?;
        let wallet = auth::verify(pubkey_b58, signature_b58, &rec.nonce)?;
        // Nonce is single-use: rotate it so the same signature cannot re-auth.
        rec.nonce = auth::new_nonce();
        rec.wallet = Some(wallet);
        rec.info.wallet = Some(wallet.to_string());
        Ok((wallet, rec.room_id))
    }

    async fn wallet_of(&self, id: &str) -> Option<Pubkey> {
        self.inner.read().await.get(id).and_then(|rec| rec.wallet)
    }

    async fn update_user_data(
        &self,
        id: &str,
        data: UserDataPayload,
    ) -> Option<(ClientInfo, Option<u32>, Vec<String>)> {
        let mut guard = self.inner.write().await;
        let update_presence_token = data.presence_token.is_some();
        let presence_token = sanitize_presence_token(data.presence_token.as_deref());
        let (info, room_id, presence_token) = {
            // Never recreate a record after `disconnect` while a delayed event
            // handler is still finishing.
            let entry = guard.get_mut(id)?;
            entry.info.avatar = truncate(&data.avatar.unwrap_or_default(), MAX_AVATAR_LEN);
            entry.info.avatar_height_scale = sanitize_avatar_height_scale(data.avatar_height_scale);
            entry.info.nickname =
                truncate(data.nickname.unwrap_or_default().trim(), MAX_NICKNAME_LEN);
            if update_presence_token {
                entry.presence_token = presence_token;
            }
            (
                entry.info.clone(),
                entry.room_id,
                entry.presence_token.clone(),
            )
        };
        let superseded = room_id
            .map(|room_id| {
                supersede_duplicate_presence(&mut guard, id, room_id, presence_token.as_deref())
            })
            .unwrap_or_default();
        Some((info, room_id, superseded))
    }

    /// Record a route-generation before any potentially slow room lookup.
    /// A lower generation can never overwrite a newer navigation intent.
    async fn note_presence_claim(&self, id: &str, claim_id: Option<u64>) -> bool {
        let mut guard = self.inner.write().await;
        let Some(entry) = guard.get_mut(id) else {
            return false;
        };
        match claim_id {
            Some(claim_id) if claim_id >= entry.latest_claim_id => {
                entry.latest_claim_id = claim_id;
                true
            }
            Some(_) => false,
            // Legacy clients do not carry generations. Once a modern claim
            // was seen on this socket, do not let an old-format delayed event
            // override it.
            None => entry.latest_claim_id == 0,
        }
    }

    async fn set_room(
        &self,
        id: &str,
        room_id: u32,
        presence_token: Option<String>,
        claim_id: Option<u64>,
    ) -> Option<(Option<u32>, ClientInfo, Vec<String>)> {
        let mut guard = self.inner.write().await;
        let (previous_room, info, presence_token) = {
            // See `update_user_data`: a disconnected socket must stay gone.
            let entry = guard.get_mut(id)?;
            let claim_is_current = match claim_id {
                Some(claim_id) => claim_id == entry.latest_claim_id,
                None => entry.latest_claim_id == 0,
            };
            if !claim_is_current {
                return None;
            }
            if let Some(presence_token) = presence_token {
                entry.presence_token = sanitize_presence_token(Some(&presence_token));
            }
            let previous_room = entry.room_id.replace(room_id);
            if previous_room != Some(room_id) {
                entry.last_client_move_seq = 0;
                entry.server_move_seq = 0;
            }
            (
                previous_room,
                entry.info.clone(),
                entry.presence_token.clone(),
            )
        };
        let superseded =
            supersede_duplicate_presence(&mut guard, id, room_id, presence_token.as_deref());
        Some((previous_room, info, superseded))
    }

    async fn room_of(&self, id: &str) -> Option<u32> {
        self.inner.read().await.get(id).and_then(|rec| rec.room_id)
    }

    async fn take_current_room(&self, id: &str) -> Option<u32> {
        let mut guard = self.inner.write().await;
        let entry = guard.get_mut(id)?;
        entry.motion = None;
        entry.room_id.take()
    }

    async fn clear_room(&self, id: &str, room_id: u32, claim_id: Option<u64>) -> Option<u32> {
        let mut guard = self.inner.write().await;
        let entry = guard.get_mut(id)?;
        let claim_is_current = match claim_id {
            Some(claim_id) => claim_id == entry.latest_claim_id,
            None => entry.latest_claim_id == 0,
        };
        if !claim_is_current {
            return None;
        }
        if entry.room_id != Some(room_id) {
            return None;
        }
        entry.motion = None;
        entry.room_id.take()
    }

    async fn update_move(
        &self,
        id: &str,
        position: [f32; 3],
        rotation: f32,
        client_seq: Option<u64>,
    ) -> Option<(ClientInfo, u32, u64)> {
        let mut guard = self.inner.write().await;
        let entry = guard.get_mut(id)?;
        let room_id = entry.room_id?;
        if let Some(seq) = client_seq {
            if seq <= entry.last_client_move_seq {
                return None;
            }
            entry.last_client_move_seq = seq;
        }
        entry.motion = None;
        entry.info.position = position.to_vec();
        entry.info.rotation = rotation;
        entry.server_move_seq = entry.server_move_seq.saturating_add(1);
        Some((entry.info.clone(), room_id, entry.server_move_seq))
    }

    async fn remove(&self, id: &str) -> Option<u32> {
        let mut guard = self.inner.write().await;
        guard.remove(id).and_then(|rec| rec.room_id)
    }

    async fn snapshot_room(&self, room_id: u32) -> HashMap<String, ClientInfo> {
        let guard = self.inner.read().await;
        guard
            .iter()
            .filter(|(_, rec)| rec.room_id == Some(room_id))
            .map(|(id, rec)| (id.clone(), rec.info.clone()))
            .collect()
    }

    async fn get_with_room(&self, id: &str) -> Option<(ClientInfo, Option<u32>)> {
        let guard = self.inner.read().await;
        guard.get(id).map(|rec| (rec.info.clone(), rec.room_id))
    }

    async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    // ------------------------------------------------------------ rooms

    /// Validate a client-supplied room id against on-chain supply.
    async fn parse_room_id(&self, raw: &str) -> Result<u32, RoomErrorCode> {
        let id: u32 = raw.trim().parse().map_err(|_| RoomErrorCode::InvalidRoom)?;
        if id == 0 {
            return Err(RoomErrorCode::InvalidRoom);
        }
        let config = self
            .chain
            .config()
            .await
            .map_err(|_| RoomErrorCode::ChainUnavailable)?;
        if id > config.total_spaces {
            return Err(RoomErrorCode::InvalidRoom);
        }
        Ok(id)
    }

    /// Current room record, loading from disk on first access.
    async fn room_program(&self, room_id: u32) -> RoomProgramRecord {
        if let Some(rec) = self.room_programs.read().await.get(&room_id) {
            return rec.clone();
        }
        let loaded = match self.store.load::<RoomProgramRecord>(room_id).await {
            Ok(Some(rec)) => rec,
            Ok(None) => RoomProgramRecord {
                state: RoomProgramState::empty(now_millis()),
                revision: 0,
                updated_by: None,
                updated_at_ms: 0,
            },
            Err(err) => {
                tracing::error!(room_id, ?err, "failed to load room from store");
                RoomProgramRecord {
                    state: RoomProgramState::empty(now_millis()),
                    revision: 0,
                    updated_by: None,
                    updated_at_ms: 0,
                }
            }
        };
        let mut guard = self.room_programs.write().await;
        guard.entry(room_id).or_insert(loaded).clone()
    }

    /// Apply an update if `expected_revision` matches. Persists before returning.
    async fn apply_room_update(
        &self,
        room_id: u32,
        expected_revision: u64,
        mut state: RoomProgramState,
        by: &Pubkey,
    ) -> Result<RoomProgramRecord, (RoomProgramRecord, RoomErrorCode)> {
        // Make sure the room is loaded, then take the write lock for compare-and-set.
        let _ = self.room_program(room_id).await;
        let mut guard = self.room_programs.write().await;
        let entry = guard.get_mut(&room_id).expect("room loaded above");
        if entry.revision != expected_revision {
            return Err((entry.clone(), RoomErrorCode::StaleRevision));
        }
        let now = now_millis();
        state.updated_at = now;
        let next = RoomProgramRecord {
            state,
            revision: entry.revision + 1,
            updated_by: Some(by.to_string()),
            updated_at_ms: now,
        };
        if let Err(err) = self.store.save(room_id, &next).await {
            tracing::error!(room_id, ?err, "failed to persist room");
            return Err((entry.clone(), RoomErrorCode::StorageFailed));
        }
        *entry = next.clone();
        Ok(next)
    }

    async fn access_for(&self, room_id: u32, wallet: &Pubkey) -> Result<RoomAccess, RoomErrorCode> {
        if self.moderators.contains(wallet) {
            return Ok(RoomAccess {
                can_edit: true,
                holder: None,
                is_open: true,
                minted: true,
            });
        }
        match self.chain.space_access(room_id).await {
            Ok(access) => Ok(RoomAccess {
                can_edit: access.can_edit(wallet),
                holder: access.holder.map(|h| h.to_string()),
                is_open: access.is_open,
                minted: true,
            }),
            Err(ChainError::NotFound) => Ok(RoomAccess {
                can_edit: false,
                holder: None,
                is_open: true,
                minted: false,
            }),
            Err(err) => {
                tracing::warn!(room_id, ?err, "ownership lookup failed");
                Err(RoomErrorCode::ChainUnavailable)
            }
        }
    }

    async fn set_goto(
        &self,
        id: &str,
        target: [f32; 3],
        speed: f32,
        rotation: Option<f32>,
    ) -> Option<u32> {
        let mut guard = self.inner.write().await;
        let entry = guard.get_mut(id)?;
        let room_id = entry.room_id?;
        entry.motion = Some(Motion { target, speed });
        if let Some(rot) = rotation {
            entry.info.rotation = rot;
        }
        Some(room_id)
    }

    async fn tick_motions(&self, dt_secs: f32) -> Vec<(u32, MoveBroadcast)> {
        let mut out = Vec::new();
        let mut guard = self.inner.write().await;

        for (id, rec) in guard.iter_mut() {
            let Some(motion) = rec.motion.clone() else {
                continue;
            };
            let Some(room_id) = rec.room_id else {
                rec.motion = None;
                continue;
            };

            let cur = to_vec3(&rec.info.position).unwrap_or([0.0, 0.0, 0.0]);
            let dx = motion.target[0] - cur[0];
            let dy = motion.target[1] - cur[1];
            let dz = motion.target[2] - cur[2];
            let dist = (dx * dx + dy * dy + dz * dz).sqrt();

            let step = (motion.speed.max(0.0)) * dt_secs.max(0.0);
            let next = if dist <= 1e-4 || step <= 1e-6 || dist <= step {
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
                    avatar_height_scale: rec.info.avatar_height_scale,
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

// ---------------------------------------------------------------- payloads

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserDataPayload {
    avatar: Option<String>,
    avatar_height_scale: Option<f32>,
    nickname: Option<String>,
    presence_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JoinSpaceInput {
    RoomId(String),
    WithPresence {
        #[serde(rename = "roomId")]
        room_id: String,
        #[serde(rename = "presenceToken")]
        presence_token: Option<String>,
        #[serde(rename = "claimId")]
        claim_id: Option<u64>,
    },
}

impl JoinSpaceInput {
    fn into_parts(self) -> (String, Option<String>, Option<u64>) {
        match self {
            Self::RoomId(room_id) => (room_id, None, None),
            Self::WithPresence {
                room_id,
                presence_token,
                claim_id,
            } => (room_id, presence_token, claim_id),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LeaveSpaceInput {
    RoomId(String),
    WithClaim {
        #[serde(rename = "roomId")]
        room_id: String,
        #[serde(rename = "claimId")]
        claim_id: Option<u64>,
    },
}

impl LeaveSpaceInput {
    fn into_parts(self) -> (String, Option<u64>) {
        match self {
            Self::RoomId(room_id) => (room_id, None),
            Self::WithClaim { room_id, claim_id } => (room_id, claim_id),
        }
    }
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
#[serde(rename_all = "camelCase")]
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

#[derive(Deserialize)]
struct AuthPayload {
    pubkey: String,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthNonceBroadcast {
    nonce: String,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthResultBroadcast {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NewUserBroadcast {
    id: String,
    user_data: ClientInfo,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresenceSupersededBroadcast {
    room_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PresenceReadyBroadcast {
    room_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    claim_id: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MoveBroadcast {
    id: String,
    position: Vec<f32>,
    rotation: f32,
    avatar: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar_height_scale: Option<f32>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomProgramRequestPayload {
    room_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomProgramUpdatePayload {
    room_id: String,
    state: RoomProgramState,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    rejected: Option<RoomErrorCode>,
}

#[derive(Clone, Copy, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoomErrorCode {
    InvalidRoom,
    ChainUnavailable,
    AuthRequired,
    Forbidden,
    StaleRevision,
    InvalidState,
    RateLimited,
    StorageFailed,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomErrorBroadcast {
    room_id: String,
    code: RoomErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claim_id: Option<u64>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RoomAccess {
    can_edit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    holder: Option<String>,
    is_open: bool,
    minted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomAccessBroadcast {
    room_id: String,
    #[serde(flatten)]
    access: RoomAccess,
}

// ------------------------------------------------------------------ wiring

pub async fn build_layer(
    config: &Config,
) -> Result<(SocketIoLayer, SocketIo), crate::store::StoreError> {
    let chain = ChainClient::new(
        config.solana_rpc_url.clone(),
        config.space_program_id,
        Duration::from_secs(config.ownership_cache_secs),
    );
    let store = RoomStore::open(&config.data_dir).await?;
    let state = ClientsState::new(chain, store, config.moderators.clone());
    let (layer, io) = SocketIo::builder()
        .with_state(state.clone())
        .max_payload(MAX_PAYLOAD_BYTES)
        .build_layer();
    start_motion_loop(io.clone(), state);
    Ok((layer, io))
}

pub fn register_handlers(io: &SocketIo) {
    io.ns("/", on_connect);
}

async fn on_connect(s: SocketRef, _io: SocketIo, state: State<ClientsState>) {
    let id = s.id.to_string();
    let nonce = state.insert_default(id.clone()).await;
    let count = state.len().await;
    tracing::info!(client_id = %id, client_count = count, "client connected");

    let _ = s.emit(
        "auth nonce",
        &AuthNonceBroadcast {
            message: auth::auth_message(&nonce),
            nonce,
        },
    );

    s.on("auth", on_auth);
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

async fn on_auth(s: SocketRef, state: State<ClientsState>, TryData(payload): TryData<AuthPayload>) {
    let id = s.id.to_string();
    if !state.allow(&id, Limit::Auth).await {
        let _ = s.emit(
            "auth result",
            &AuthResultBroadcast {
                ok: false,
                wallet: None,
                error: Some("rate_limited"),
            },
        );
        return;
    }
    let Ok(payload) = payload else {
        let _ = s.emit(
            "auth result",
            &AuthResultBroadcast {
                ok: false,
                wallet: None,
                error: Some("bad_payload"),
            },
        );
        return;
    };
    match state
        .authenticate(&id, &payload.pubkey, &payload.signature)
        .await
    {
        Ok((wallet, room_id)) => {
            tracing::info!(client_id = %id, wallet = %wallet, "client authenticated");
            let _ = s.emit(
                "auth result",
                &AuthResultBroadcast {
                    ok: true,
                    wallet: Some(wallet.to_string()),
                    error: None,
                },
            );
            if let Some(room_id) = room_id {
                // Ownership lookup may be slow. Resolve it without blocking
                // all presence transitions, then revalidate the room while a
                // read guard keeps the access + presence broadcasts ordered
                // before any supersede/delete transition.
                let access = state.access_for(room_id, &wallet).await;
                let _presence_read = state.presence_transition.read().await;
                let Some((info, Some(current_room))) = state.get_with_room(&id).await else {
                    return;
                };
                if current_room != room_id {
                    return;
                }

                match access {
                    Ok(access) => {
                        let _ = s.emit(
                            "room access",
                            &RoomAccessBroadcast {
                                room_id: room_id.to_string(),
                                access,
                            },
                        );
                    }
                    Err(code) => {
                        emit_room_error(&s, room_id, code, None);
                    }
                }

                // Presence now carries the wallet.
                let _ = s
                    .to(space_room_name(room_id))
                    .emit(
                        "new user",
                        &NewUserBroadcast {
                            id,
                            user_data: info,
                        },
                    )
                    .await;
            }
        }
        Err(err) => {
            tracing::warn!(client_id = %id, ?err, "auth failed");
            let _ = s.emit(
                "auth result",
                &AuthResultBroadcast {
                    ok: false,
                    wallet: None,
                    error: Some("invalid_signature"),
                },
            );
        }
    }
}

async fn emit_room_access(
    s: &SocketRef,
    state: &ClientsState,
    room_id: u32,
    wallet: Option<Pubkey>,
) {
    let access = match wallet {
        Some(wallet) => match state.access_for(room_id, &wallet).await {
            Ok(access) => access,
            Err(code) => {
                emit_room_error(s, room_id, code, None);
                return;
            }
        },
        None => RoomAccess {
            can_edit: false,
            holder: None,
            is_open: true,
            minted: true,
        },
    };
    let _ = s.emit(
        "room access",
        &RoomAccessBroadcast {
            room_id: room_id.to_string(),
            access,
        },
    );
}

fn emit_room_error(
    s: &SocketRef,
    room_id: impl ToString,
    code: RoomErrorCode,
    detail: Option<String>,
) {
    let _ = s.emit(
        "room error",
        &RoomErrorBroadcast {
            room_id: room_id.to_string(),
            code,
            detail,
            claim_id: None,
        },
    );
}

fn emit_room_claim_error(
    s: &SocketRef,
    room_id: impl ToString,
    code: RoomErrorCode,
    claim_id: Option<u64>,
) {
    let _ = s.emit(
        "room error",
        &RoomErrorBroadcast {
            room_id: room_id.to_string(),
            code,
            detail: None,
            claim_id,
        },
    );
}

async fn on_chat_message(
    s: SocketRef,
    state: State<ClientsState>,
    TryData(payload): TryData<ChatMessageInput>,
) {
    let id = s.id.to_string();
    let Ok(payload) = payload else { return };
    // Keep room membership stable from the state read through the broadcast.
    // A superseding tab therefore observes either this message then `delete`,
    // or no message at all; never a message from an already-removed ghost.
    let _presence_read = state.presence_transition.read().await;
    let Some((info, Some(room_id))) = state.get_with_room(&id).await else {
        return;
    };
    if !state.allow(&id, Limit::Chat).await {
        return;
    }
    let message = match payload {
        ChatMessageInput::Text(message) => message,
        ChatMessageInput::Object { message, .. } => message,
    };
    let message = truncate(message.trim(), MAX_CHAT_LEN);
    if message.is_empty() {
        return;
    }
    tracing::debug!(client_id = %id, len = message.len(), "chat message");
    let payload = ChatBroadcast {
        id,
        nickname: info.nickname,
        message,
        wallet: info.wallet,
    };
    if let Err(err) = s
        .within(space_room_name(room_id))
        .emit("chat message", &payload)
        .await
    {
        tracing::warn!(?err, "failed to emit chat message");
    }
}

async fn on_set_user_data(
    s: SocketRef,
    io: SocketIo,
    state: State<ClientsState>,
    TryData(payload): TryData<UserDataPayload>,
) {
    let id = s.id.to_string();
    let Ok(payload) = payload else { return };
    if !state.allow(&id, Limit::UserData).await {
        return;
    }
    let _presence_transition = state.presence_transition.write().await;
    let Some((user_data, room_id, superseded)) = state.update_user_data(&id, payload).await else {
        return;
    };
    let Some(room_id) = room_id else {
        return;
    };
    evict_superseded_presences(&io, room_id, superseded).await;
    let payload = NewUserBroadcast { id, user_data };
    if let Err(err) = s
        .to(space_room_name(room_id))
        .emit("new user", &payload)
        .await
    {
        tracing::warn!(?err, "failed to emit new user");
    }
}

async fn on_move(s: SocketRef, state: State<ClientsState>, TryData(payload): TryData<MovePayload>) {
    // Never trust a client-provided `id`; identity is the socket.
    let id = s.id.to_string();
    let Ok(payload) = payload else { return };
    let Some(position) = payload.position.as_deref().and_then(to_vec3) else {
        return;
    };
    if !in_world_bounds(position) {
        return;
    }
    let rotation = payload.rotation.unwrap_or(0.0);
    if !rotation.is_finite() {
        return;
    }
    if !state.allow(&id, Limit::Moves).await {
        return;
    }

    let _presence_read = state.presence_transition.read().await;
    let Some((user, room_id, server_seq)) = state
        .update_move(&id, position, rotation, payload.seq)
        .await
    else {
        return;
    };
    let payload = MoveBroadcast {
        id,
        position: user.position.clone(),
        rotation,
        avatar: user.avatar,
        avatar_height_scale: user.avatar_height_scale,
        nickname: user.nickname,
        server_seq,
        server_time: now_millis(),
        client_seq: payload.seq,
    };

    if let Err(err) = s
        .within(space_room_name(room_id))
        .emit("move", &payload)
        .await
    {
        tracing::warn!(?err, "failed to emit move");
    }
}

async fn on_goto(s: SocketRef, state: State<ClientsState>, TryData(payload): TryData<GotoPayload>) {
    let id = s.id.to_string();
    let Ok(payload) = payload else { return };
    let Some(target) = payload.position.as_deref().and_then(to_vec3) else {
        return;
    };
    if !in_world_bounds(target) || !state.allow(&id, Limit::Moves).await {
        return;
    }
    let _presence_read = state.presence_transition.read().await;
    let speed = payload.speed.unwrap_or(3.0).clamp(0.1, 50.0);
    let _ = state.set_goto(&id, target, speed, payload.rotation).await;
}

async fn on_join_space(
    s: SocketRef,
    io: SocketIo,
    state: State<ClientsState>,
    TryData(payload): TryData<JoinSpaceInput>,
) {
    let id = s.id.to_string();
    let Ok(payload) = payload else { return };
    let (raw, presence_token, claim_id) = payload.into_parts();

    // Record ordering before the on-chain room validation below. This short
    // transition also prevents the claim generation from changing halfway
    // through another room-membership broadcast.
    {
        let _presence_transition = state.presence_transition.write().await;
        if !state.note_presence_claim(&id, claim_id).await {
            return;
        }
        let previous_room = state.take_current_room(&id).await;
        if let Some(previous_room) = previous_room {
            let previous_socket_room = space_room_name(previous_room);
            s.leave(previous_socket_room.clone());
            if let Err(err) = s.to(previous_socket_room).emit("delete", &id).await {
                tracing::warn!(?err, "failed to emit delete while beginning room claim");
            }
        }
    }
    if !state.allow(&id, Limit::Joins).await {
        emit_room_claim_error(&s, raw, RoomErrorCode::RateLimited, claim_id);
        return;
    }
    let room_id = match state.parse_room_id(&raw).await {
        Ok(room_id) => room_id,
        Err(code) => {
            emit_room_claim_error(&s, raw, code, claim_id);
            return;
        }
    };
    let presence_transition = state.presence_transition.write().await;
    let socket_room = space_room_name(room_id);
    let Some((previous_room, user_data, superseded)) =
        state.set_room(&id, room_id, presence_token, claim_id).await
    else {
        return;
    };

    // `begin room claim` already detached the current room. Keep this branch
    // for legacy/re-entrant safety if state is ever populated between phases.
    if let Some(previous_room) = previous_room.filter(|prev| *prev != room_id) {
        let previous_socket_room = space_room_name(previous_room);
        s.leave(previous_socket_room.clone());
        if let Err(err) = s.to(previous_socket_room).emit("delete", &id).await {
            tracing::warn!(?err, "failed to emit delete on room switch");
        }
    }

    evict_superseded_presences(&io, room_id, superseded).await;
    s.join(socket_room.clone());

    let clients = state.snapshot_room(room_id).await;
    if let Err(err) = s.emit("existing clients", &clients) {
        tracing::warn!(?err, "failed to emit room clients");
    }
    if let Err(err) = s.emit(
        "presence ready",
        &PresenceReadyBroadcast {
            room_id: room_id.to_string(),
            claim_id,
        },
    ) {
        tracing::warn!(?err, "failed to confirm room presence");
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

    drop(presence_transition);

    let record = state.room_program(room_id).await;
    let _ = s.emit(
        "room program state",
        &RoomProgramBroadcast {
            room_id: room_id.to_string(),
            state: record.state,
            server_revision: record.revision,
            server_time: now_millis(),
            source_client_id: None,
            rejected: None,
        },
    );

    let wallet = state.wallet_of(&id).await;
    emit_room_access(&s, &state, room_id, wallet).await;
}

async fn on_leave_space(
    s: SocketRef,
    state: State<ClientsState>,
    TryData(payload): TryData<LeaveSpaceInput>,
) {
    let Ok(payload) = payload else { return };
    let (raw, claim_id) = payload.into_parts();
    let Ok(room_id) = raw.trim().parse::<u32>() else {
        return;
    };
    let id = s.id.to_string();
    let _presence_transition = state.presence_transition.write().await;
    if !state.note_presence_claim(&id, claim_id).await {
        return;
    }
    let Some(left_room) = state.clear_room(&id, room_id, claim_id).await else {
        return;
    };
    let socket_room = space_room_name(left_room);
    s.leave(socket_room.clone());
    if let Err(err) = s.to(socket_room).emit("delete", &id).await {
        tracing::warn!(?err, "failed to emit delete on leave");
    }
}

async fn on_request_room_program(
    s: SocketRef,
    state: State<ClientsState>,
    TryData(payload): TryData<RoomProgramRequestPayload>,
) {
    let id = s.id.to_string();
    let Ok(payload) = payload else { return };
    if !state.allow(&id, Limit::Joins).await {
        return;
    }
    let room_id = match state.parse_room_id(&payload.room_id).await {
        Ok(room_id) => room_id,
        Err(code) => {
            emit_room_error(&s, payload.room_id, code, None);
            return;
        }
    };
    // Reading a room does not move the client into it; `join-space` does.
    if state.room_of(&id).await != Some(room_id) {
        emit_room_error(
            &s,
            room_id,
            RoomErrorCode::Forbidden,
            Some("join the space first".into()),
        );
        return;
    }
    let record = state.room_program(room_id).await;
    let _ = s.emit(
        "room program state",
        &RoomProgramBroadcast {
            room_id: room_id.to_string(),
            state: record.state,
            server_revision: record.revision,
            server_time: now_millis(),
            source_client_id: None,
            rejected: None,
        },
    );
}

async fn on_room_program_update(
    s: SocketRef,
    io: SocketIo,
    state: State<ClientsState>,
    TryData(payload): TryData<RoomProgramUpdatePayload>,
) {
    let id = s.id.to_string();
    let payload = match payload {
        Ok(payload) => payload,
        Err(err) => {
            tracing::debug!(client_id = %id, ?err, "room update: bad payload");
            emit_room_error(
                &s,
                "?",
                RoomErrorCode::InvalidState,
                Some("malformed payload".into()),
            );
            return;
        }
    };
    let room_id = match state.parse_room_id(&payload.room_id).await {
        Ok(room_id) => room_id,
        Err(code) => {
            emit_room_error(&s, payload.room_id, code, None);
            return;
        }
    };
    if state.room_of(&id).await != Some(room_id) {
        emit_room_error(
            &s,
            room_id,
            RoomErrorCode::Forbidden,
            Some("join the space first".into()),
        );
        return;
    }
    if !state.allow(&id, Limit::RoomUpdates).await {
        emit_room_error(&s, room_id, RoomErrorCode::RateLimited, None);
        return;
    }
    let Some(wallet) = state.wallet_of(&id).await else {
        emit_room_error(&s, room_id, RoomErrorCode::AuthRequired, None);
        return;
    };
    match state.access_for(room_id, &wallet).await {
        Ok(access) if access.can_edit => {}
        Ok(_) => {
            emit_room_error(&s, room_id, RoomErrorCode::Forbidden, None);
            return;
        }
        Err(code) => {
            emit_room_error(&s, room_id, code, None);
            return;
        }
    }
    if let Err(err) = validate_room_state(&payload.state) {
        emit_room_error(
            &s,
            room_id,
            RoomErrorCode::InvalidState,
            Some(err.to_string()),
        );
        return;
    }

    let expected = payload.server_revision.unwrap_or(0);
    match state
        .apply_room_update(room_id, expected, payload.state, &wallet)
        .await
    {
        Ok(record) => {
            let broadcast = RoomProgramBroadcast {
                room_id: room_id.to_string(),
                state: record.state,
                server_revision: record.revision,
                server_time: now_millis(),
                source_client_id: Some(id),
                rejected: None,
            };
            if let Err(err) = io
                .within(space_room_name(room_id))
                .emit("room program state", &broadcast)
                .await
            {
                tracing::warn!(?err, "failed to emit room program state");
            }
        }
        Err((current, code)) => {
            let _ = s.emit(
                "room program state",
                &RoomProgramBroadcast {
                    room_id: room_id.to_string(),
                    state: current.state,
                    server_revision: current.revision,
                    server_time: now_millis(),
                    source_client_id: None,
                    rejected: Some(code),
                },
            );
        }
    }
}

async fn on_disconnect(s: SocketRef, io: SocketIo, state: State<ClientsState>) {
    let id = s.id.to_string();
    let _presence_transition = state.presence_transition.write().await;
    let room_id = state.remove(&id).await;
    let count = state.len().await;
    tracing::info!(client_id = %id, client_count = count, "client disconnected");
    if let Some(room_id) = room_id {
        if let Err(err) = io
            .within(space_room_name(room_id))
            .emit("delete", &id)
            .await
        {
            tracing::warn!(?err, "failed to emit delete");
        }
    }
}

// ----------------------------------------------------------------- helpers

fn supersede_duplicate_presence(
    clients: &mut HashMap<String, ClientRecord>,
    current_id: &str,
    room_id: u32,
    presence_token: Option<&str>,
) -> Vec<String> {
    let Some(presence_token) = presence_token else {
        return Vec::new();
    };

    clients
        .iter_mut()
        .filter_map(|(id, record)| {
            let duplicate = id != current_id
                && record.room_id == Some(room_id)
                && record.presence_token.as_deref() == Some(presence_token);
            duplicate.then(|| {
                record.room_id = None;
                record.motion = None;
                id.clone()
            })
        })
        .collect()
}

async fn evict_superseded_presences(io: &SocketIo, room_id: u32, superseded: Vec<String>) {
    if superseded.is_empty() {
        return;
    }
    let socket_room = space_room_name(room_id);

    for id in &superseded {
        if let Ok(sid) = id.parse::<Sid>() {
            if let Some(socket) = io.get_socket(sid) {
                let _ = socket.emit(
                    "presence superseded",
                    &PresenceSupersededBroadcast {
                        room_id: room_id.to_string(),
                    },
                );
                socket.leave(socket_room.clone());
            }
        }
    }
    for id in superseded {
        tracing::info!(client_id = %id, room_id, "duplicate browser presence superseded");
        if let Err(err) = io.within(socket_room.clone()).emit("delete", &id).await {
            tracing::warn!(?err, "failed to emit delete for superseded presence");
        }
    }
}

fn to_vec3(v: &[f32]) -> Option<[f32; 3]> {
    if v.len() != 3 || !v.iter().all(|n| n.is_finite()) {
        return None;
    }
    Some([v[0], v[1], v[2]])
}

fn in_world_bounds(p: [f32; 3]) -> bool {
    p[0].abs() <= WORLD_BOUND_XZ && p[2].abs() <= WORLD_BOUND_XZ && p[1].abs() <= WORLD_BOUND_Y
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn sanitize_avatar_height_scale(value: Option<f32>) -> Option<f32> {
    value
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(MIN_AVATAR_HEIGHT_SCALE, MAX_AVATAR_HEIGHT_SCALE))
}

fn sanitize_presence_token(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let valid_length = (16..=MAX_PRESENCE_TOKEN_LEN).contains(&value.len());
    let valid_chars = value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    (valid_length && valid_chars).then(|| value.to_string())
}

fn space_room_name(room_id: u32) -> String {
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
        let mut ticker = interval(Duration::from_millis(50));
        let dt_secs = 0.05_f32;
        loop {
            ticker.tick().await;
            // Keep every generated motion update ordered before a possible
            // supersede/delete transition. Otherwise a queued move could
            // resurrect a just-removed remote avatar on clients.
            let _presence_read = state.presence_transition.read().await;
            let updates = state.tick_motions(dt_secs).await;
            if updates.is_empty() {
                continue;
            }
            for (room_id, payload) in updates {
                if let Err(err) = io
                    .within(space_room_name(room_id))
                    .emit("move", &payload)
                    .await
                {
                    tracing::warn!(?err, "failed to emit move (motion loop)");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_height_scale_is_finite_and_clamped() {
        assert_eq!(sanitize_avatar_height_scale(None), None);
        assert_eq!(sanitize_avatar_height_scale(Some(f32::NAN)), None);
        assert_eq!(sanitize_avatar_height_scale(Some(f32::INFINITY)), None);
        assert_eq!(sanitize_avatar_height_scale(Some(0.25)), Some(0.5));
        assert_eq!(sanitize_avatar_height_scale(Some(1.25)), Some(1.25));
        assert_eq!(sanitize_avatar_height_scale(Some(4.0)), Some(2.0));
    }

    #[test]
    fn avatar_height_scale_uses_optional_camel_case_json() {
        let payload: UserDataPayload = serde_json::from_value(serde_json::json!({
            "nickname": "Wang",
            "avatar": "ipfs://avatar",
            "avatarHeightScale": 1.25
        }))
        .expect("valid user data");
        assert_eq!(payload.avatar_height_scale, Some(1.25));

        let value = serde_json::to_value(ClientInfo {
            avatar_height_scale: Some(1.25),
            ..ClientInfo::default()
        })
        .expect("serializable client info");
        assert_eq!(
            value.get("avatarHeightScale"),
            Some(&serde_json::json!(1.25))
        );
        assert!(value.get("avatar_height_scale").is_none());

        let value =
            serde_json::to_value(ClientInfo::default()).expect("serializable default client info");
        assert!(value.get("avatarHeightScale").is_none());
    }

    #[test]
    fn presence_token_is_private_and_strictly_validated() {
        assert_eq!(sanitize_presence_token(None), None);
        assert_eq!(sanitize_presence_token(Some("short")), None);
        assert_eq!(sanitize_presence_token(Some("0123456789abcdef!")), None);
        assert_eq!(
            sanitize_presence_token(Some(" 01234567-89ab-cdef-0123-456789abcdef ")),
            Some("01234567-89ab-cdef-0123-456789abcdef".to_string())
        );

        let payload: UserDataPayload = serde_json::from_value(serde_json::json!({
            "nickname": "Wang",
            "avatar": "ipfs://avatar",
            "presenceToken": "01234567-89ab-cdef-0123-456789abcdef"
        }))
        .expect("valid user data");
        assert_eq!(
            payload.presence_token.as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );

        let outbound = serde_json::to_value(ClientInfo::default()).expect("client info");
        assert!(outbound.get("presenceToken").is_none());
    }

    #[test]
    fn join_space_accepts_legacy_and_atomic_presence_payloads() {
        let legacy: JoinSpaceInput =
            serde_json::from_value(serde_json::json!("7")).expect("legacy room id");
        assert_eq!(legacy.into_parts(), ("7".to_string(), None, None));

        let modern: JoinSpaceInput = serde_json::from_value(serde_json::json!({
            "roomId": "7",
            "presenceToken": "01234567-89ab-cdef-0123-456789abcdef",
            "claimId": 42
        }))
        .expect("presence-aware room claim");
        assert_eq!(
            modern.into_parts(),
            (
                "7".to_string(),
                Some("01234567-89ab-cdef-0123-456789abcdef".to_string()),
                Some(42)
            )
        );

        let leave: LeaveSpaceInput = serde_json::from_value(serde_json::json!({
            "roomId": "7",
            "claimId": 43
        }))
        .expect("claim-aware leave");
        assert_eq!(leave.into_parts(), ("7".to_string(), Some(43)));
    }

    #[test]
    fn newest_socket_supersedes_same_browser_presence_in_room() {
        let token = "01234567-89ab-cdef-0123-456789abcdef";
        let mut clients = HashMap::new();

        let mut old_same_room = ClientRecord::new();
        old_same_room.room_id = Some(1);
        old_same_room.presence_token = Some(token.to_string());
        old_same_room.motion = Some(Motion {
            target: [1.0, 0.0, 0.0],
            speed: 1.0,
        });
        clients.insert("old".to_string(), old_same_room);

        let mut other_browser = ClientRecord::new();
        other_browser.room_id = Some(1);
        other_browser.presence_token = Some("fedcba98-7654-3210-fedc-ba9876543210".to_string());
        clients.insert("other".to_string(), other_browser);

        let mut same_browser_other_room = ClientRecord::new();
        same_browser_other_room.room_id = Some(2);
        same_browser_other_room.presence_token = Some(token.to_string());
        clients.insert("other-room".to_string(), same_browser_other_room);

        let mut current = ClientRecord::new();
        current.room_id = Some(1);
        current.presence_token = Some(token.to_string());
        clients.insert("current".to_string(), current);

        let superseded = supersede_duplicate_presence(&mut clients, "current", 1, Some(token));

        assert_eq!(superseded, vec!["old"]);
        assert_eq!(clients["old"].room_id, None);
        assert!(clients["old"].motion.is_none());
        assert_eq!(clients["current"].room_id, Some(1));
        assert_eq!(clients["other"].room_id, Some(1));
        assert_eq!(clients["other-room"].room_id, Some(2));
    }
}
