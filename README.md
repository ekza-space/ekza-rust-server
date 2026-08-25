# ekza-rust-server

Realtime backend for [space.ekza.io](https://space.ekza.io): presence, movement,
chat and per-space room state over Socket.IO (axum + socketioxide).

## Trust model

| Concern | Rule |
|---|---|
| Identity | Wallet pubkey, proven by signing a server nonce (`auth`). Guests may look, move and chat. |
| Rooms | `join-space` accepts only space ids `1..=Config.total_spaces`, read from the `solana_ekza_space` program. |
| Edits | `room program update` requires the signer to be the **current NFT holder** of that space, an on-chain **editor** (only while `Space.owner` still equals the holder), or a `MODERATOR_WALLETS` entry. Resolved via RPC, cached `OWNERSHIP_CACHE_SECS`. |
| Concurrency | Client echoes the `serverRevision` it last saw; mismatch → `rejected: "stale_revision"` with the current state. |
| Durability | Every applied update is written to `DATA_DIR/rooms/<id>.json` (atomic rename) **before** broadcast. Survives restarts. |
| Content | Room state is schema-validated: ≤200 objects, known kinds, finite transforms, `linkUrl` must be `http(s)://`, `modelDataUrl` must be `https://` or `ipfs://` (inline `data:` refused). |
| Abuse | Per-socket token buckets (chat, move, edits, auth, joins), string caps, world bounds. |
| Origins | `CORS_ALLOWED_ORIGINS` is enforced on the Socket.IO handshake (403 for foreign `Origin`) and as REST CORS. No wildcard default. |

## Protocol (client → server)

| Event | Payload | Notes |
|---|---|---|
| `auth` | `{ pubkey, signature }` base58 | Sign the `message` from `auth nonce` with `signMessage`. |
| `join-space` | `"<spaceId>"` | Replies `existing clients`, `room program state`, `room access`. |
| `leave-space` | `"<spaceId>"` | |
| `set user data` | `{ nickname, avatar, avatarHeightScale? }` | ≤32 / ≤512 chars; finite height scale is clamped to `0.5..=2` |
| `move` | `{ position:[x,y,z], rotation, seq?, sentAt? }` | |
| `chat message` | `{ message }` or `"text"` | ≤500 chars |
| `request room program` | `{ roomId }` | Must have joined. |
| `room program update` | `{ roomId, state, serverRevision }` | Auth + ownership required. |

Server → client: `auth nonce { nonce, message }`, `auth result { ok, wallet?, error? }`,
`room access { roomId, canEdit, holder?, isOpen, minted }`,
`room program state { roomId, state, serverRevision, serverTime, sourceClientId?, rejected? }`,
`room error { roomId, code, detail? }` with codes
`invalid_room | chain_unavailable | auth_required | forbidden | stale_revision | invalid_state | rate_limited | storage_failed`,
plus `existing clients`, `new user`, `move`, `chat message`, `delete`. Presence
snapshots and `new user`/`move` payloads include `avatarHeightScale` when set.

## Run

```bash
cp .env.example .env        # edit
cargo run --bin server
```

Tests:

```bash
cargo test                  # unit: auth, chain decoding, validation, store, limits
# e2e against a live cluster + a wallet that owns SPACE_ID:
cd e2e && yarn && SERVER_URL=http://127.0.0.1:3001 OWNER_KEYPAIR=~/owner.json SPACE_ID=3 node realtime.mjs
# then restart the server and:
PHASE=verify node realtime.mjs
```

## Deploy

`make deploy` (binary strategy) builds a static musl binary, uploads it and
restarts the container with `DATA_DIR` bind-mounted at `$(REMOTE_DATA_DIR)`
(default `$(DEPLOY_DIR)/data`) and log rotation. Override per environment:

```bash
make deploy DEPLOY_HOST=user@host \
  CORS_ALLOWED_ORIGINS=https://space.ekza.io \
  SOLANA_RPC_URL=https://<paid-rpc> \
  SPACE_PROGRAM_ID=2WtuXG6AX3erRp6eK5WiSTEEBec5zprQ7qLyLENfMQEH
```

Back up `$(REMOTE_DATA_DIR)` — it is the only copy of every space's layout.

## Known limits

- Single process, in-memory presence; horizontal scaling needs a shared adapter.
- WebSocket frame size is not capped below tungstenite's default (64 MiB); room
  payloads are validated after decode. Put the service behind a reverse proxy
  with connection limits.
- Movement is bounds-checked but not speed-checked (no anti-cheat).
