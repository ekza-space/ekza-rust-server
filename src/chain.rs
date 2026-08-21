//! Read-only Solana access: who may edit a given space right now.
//!
//! Authorization source of truth is the chain, not the client:
//! - `Config.total_spaces` bounds valid room ids.
//! - The current NFT holder (largest token account of `Space.mint`) may edit.
//! - `Space.editors` may edit only while `Space.owner` still equals the holder
//!   (mirrors the program's stale-editor rule after a transfer).
//!
//! Results are cached for `ownership_cache_secs`.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use solana_pubkey::Pubkey;
use tokio::sync::RwLock;

pub const CONFIG_SEED: &[u8] = b"config";
pub const SPACE_SEED_ROOT: &[u8] = b"space_v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceAccess {
    pub space_id: u32,
    pub mint: Pubkey,
    /// Last owner recorded in the PDA (may lag behind a transfer).
    pub recorded_owner: Pubkey,
    /// Wallet currently holding the NFT.
    pub holder: Option<Pubkey>,
    pub editors: Vec<Pubkey>,
    pub is_open: bool,
}

impl SpaceAccess {
    pub fn can_edit(&self, wallet: &Pubkey) -> bool {
        match self.holder {
            Some(holder) if holder == *wallet => true,
            Some(holder) => self.recorded_owner == holder && self.editors.contains(wallet),
            None => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChainConfig {
    pub total_spaces: u32,
    pub minted_spaces: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("rpc transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("account not found")]
    NotFound,
    #[error("account data malformed")]
    Malformed,
}

struct Cached<T> {
    value: T,
    fetched_at: Instant,
}

#[derive(Clone)]
pub struct ChainClient {
    http: reqwest::Client,
    rpc_url: String,
    program_id: Pubkey,
    config_pda: Pubkey,
    ttl: Duration,
    config_cache: Arc<RwLock<Option<Cached<ChainConfig>>>>,
    space_cache: Arc<RwLock<HashMap<u32, Cached<SpaceAccess>>>>,
}

impl ChainClient {
    pub fn new(rpc_url: String, program_id: Pubkey, ttl: Duration) -> Self {
        let (config_pda, _) = Pubkey::find_program_address(&[CONFIG_SEED], &program_id);
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .build()
                .expect("reqwest client"),
            rpc_url,
            program_id,
            config_pda,
            ttl,
            config_cache: Arc::new(RwLock::new(None)),
            space_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn config_pda(&self) -> Pubkey {
        self.config_pda
    }

    pub fn space_pda(&self, space_id: u32) -> Pubkey {
        Pubkey::find_program_address(
            &[
                SPACE_SEED_ROOT,
                self.config_pda.as_ref(),
                &space_id.to_le_bytes(),
            ],
            &self.program_id,
        )
        .0
    }

    /// Global config (total supply). Cached; on RPC failure a stale value is
    /// reused so a flaky RPC does not lock everyone out of rooms.
    pub async fn config(&self) -> Result<ChainConfig, ChainError> {
        if let Some(c) = self.config_cache.read().await.as_ref() {
            if c.fetched_at.elapsed() < self.ttl {
                return Ok(c.value.clone());
            }
        }
        match self.fetch_config().await {
            Ok(value) => {
                *self.config_cache.write().await = Some(Cached {
                    value: value.clone(),
                    fetched_at: Instant::now(),
                });
                Ok(value)
            }
            Err(err) => {
                if let Some(c) = self.config_cache.read().await.as_ref() {
                    tracing::warn!(?err, "config refresh failed, using stale value");
                    return Ok(c.value.clone());
                }
                Err(err)
            }
        }
    }

    /// Access record for one space. `Err(NotFound)` when the id is not minted.
    pub async fn space_access(&self, space_id: u32) -> Result<SpaceAccess, ChainError> {
        if let Some(c) = self.space_cache.read().await.get(&space_id) {
            if c.fetched_at.elapsed() < self.ttl {
                return Ok(c.value.clone());
            }
        }
        match self.fetch_space_access(space_id).await {
            Ok(value) => {
                self.space_cache.write().await.insert(
                    space_id,
                    Cached {
                        value: value.clone(),
                        fetched_at: Instant::now(),
                    },
                );
                Ok(value)
            }
            Err(err) => {
                if let Some(c) = self.space_cache.read().await.get(&space_id) {
                    tracing::warn!(space_id, ?err, "space refresh failed, using stale value");
                    return Ok(c.value.clone());
                }
                Err(err)
            }
        }
    }

    /// Drop a cached record (e.g. right after a client reports a transfer).
    pub async fn invalidate(&self, space_id: u32) {
        self.space_cache.write().await.remove(&space_id);
    }

    // ------------------------------------------------------------------ rpc

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, ChainError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self
            .http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            return Err(ChainError::Rpc(err.to_string()));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn account_data(&self, pubkey: &Pubkey) -> Result<Vec<u8>, ChainError> {
        let result = self
            .rpc(
                "getAccountInfo",
                json!([pubkey.to_string(), { "encoding": "base64", "commitment": "confirmed" }]),
            )
            .await?;
        let value = result.get("value").ok_or(ChainError::Malformed)?;
        if value.is_null() {
            return Err(ChainError::NotFound);
        }
        let data_b64 = value
            .get("data")
            .and_then(|d| d.get(0))
            .and_then(|s| s.as_str())
            .ok_or(ChainError::Malformed)?;
        base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .map_err(|_| ChainError::Malformed)
    }

    async fn fetch_config(&self) -> Result<ChainConfig, ChainError> {
        let data = self.account_data(&self.config_pda).await?;
        decode_config(&data)
    }

    async fn fetch_space_access(&self, space_id: u32) -> Result<SpaceAccess, ChainError> {
        let data = self.account_data(&self.space_pda(space_id)).await?;
        let mut access = decode_space(&data)?;
        access.holder = self.holder_of(&access.mint).await?;
        Ok(access)
    }

    async fn holder_of(&self, mint: &Pubkey) -> Result<Option<Pubkey>, ChainError> {
        #[derive(Deserialize)]
        struct Largest {
            address: String,
            amount: String,
        }
        let result = self
            .rpc(
                "getTokenLargestAccounts",
                json!([mint.to_string(), { "commitment": "confirmed" }]),
            )
            .await?;
        let accounts: Vec<Largest> =
            serde_json::from_value(result.get("value").cloned().unwrap_or(Value::Array(vec![])))
                .map_err(|_| ChainError::Malformed)?;
        let Some(holding) = accounts.into_iter().find(|a| a.amount == "1") else {
            return Ok(None);
        };
        let token_account =
            Pubkey::from_str(&holding.address).map_err(|_| ChainError::Malformed)?;
        let data = self.account_data(&token_account).await?;
        // SPL token account: mint (32) | owner (32) | ...
        if data.len() < 64 {
            return Err(ChainError::Malformed);
        }
        Ok(Some(
            Pubkey::try_from(&data[32..64]).map_err(|_| ChainError::Malformed)?,
        ))
    }
}

// -------------------------------------------------------------------- decode

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ChainError> {
        let end = self.pos.checked_add(n).ok_or(ChainError::Malformed)?;
        let slice = self.data.get(self.pos..end).ok_or(ChainError::Malformed)?;
        self.pos = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, ChainError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, ChainError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn pubkey(&mut self) -> Result<Pubkey, ChainError> {
        Pubkey::try_from(self.take(32)?).map_err(|_| ChainError::Malformed)
    }
    fn string(&mut self) -> Result<String, ChainError> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| ChainError::Malformed)
    }
}

/// `Config` layout: disc(8) authority pending treasury collection_mint (4×32)
/// total u32 minted u32 price u64 royalty u16 max_per_wallet u16 paused u8 base_uri string bump u8 reserved[64]
pub fn decode_config(data: &[u8]) -> Result<ChainConfig, ChainError> {
    let mut r = Reader::new(data);
    r.take(8)?;
    r.take(32 * 4)?;
    let total_spaces = r.u32()?;
    let minted_spaces = r.u32()?;
    Ok(ChainConfig {
        total_spaces,
        minted_spaces,
    })
}

/// `Space` layout: disc(8) space_id u32 mint owner name string uri string is_open u8 editors vec<pubkey> bump u8 reserved[32]
pub fn decode_space(data: &[u8]) -> Result<SpaceAccess, ChainError> {
    let mut r = Reader::new(data);
    r.take(8)?;
    let space_id = r.u32()?;
    let mint = r.pubkey()?;
    let recorded_owner = r.pubkey()?;
    let _name = r.string()?;
    let _uri = r.string()?;
    let is_open = r.u8()? == 1;
    let n = r.u32()? as usize;
    if n > 64 {
        return Err(ChainError::Malformed);
    }
    let mut editors = Vec::with_capacity(n);
    for _ in 0..n {
        editors.push(r.pubkey()?);
    }
    Ok(SpaceAccess {
        space_id,
        mint,
        recorded_owner,
        holder: None,
        editors,
        is_open,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    fn encode_space(owner: Pubkey, editors: &[Pubkey]) -> Vec<u8> {
        let mut v = vec![0u8; 8];
        v.extend_from_slice(&7u32.to_le_bytes());
        v.extend_from_slice(pk(9).as_ref());
        v.extend_from_slice(owner.as_ref());
        for s in ["name", "ipfs://x"] {
            v.extend_from_slice(&(s.len() as u32).to_le_bytes());
            v.extend_from_slice(s.as_bytes());
        }
        v.push(1);
        v.extend_from_slice(&(editors.len() as u32).to_le_bytes());
        for e in editors {
            v.extend_from_slice(e.as_ref());
        }
        v.push(255);
        v.extend_from_slice(&[0u8; 32]);
        v
    }

    #[test]
    fn decodes_space_and_applies_editor_rule() {
        let owner = pk(1);
        let editor = pk(2);
        let buyer = pk(3);
        let mut access = decode_space(&encode_space(owner, &[editor])).unwrap();
        assert_eq!(access.space_id, 7);
        assert_eq!(access.recorded_owner, owner);
        assert_eq!(access.editors, vec![editor]);

        access.holder = Some(owner);
        assert!(access.can_edit(&owner));
        assert!(access.can_edit(&editor));
        assert!(!access.can_edit(&buyer));

        // NFT sold: recorded owner lags behind. Buyer edits, seller + stale editor do not.
        access.holder = Some(buyer);
        assert!(access.can_edit(&buyer));
        assert!(!access.can_edit(&owner));
        assert!(!access.can_edit(&editor));

        access.holder = None;
        assert!(!access.can_edit(&owner));
    }

    #[test]
    fn decodes_config_total() {
        let mut v = vec![0u8; 8 + 32 * 4];
        v.extend_from_slice(&256u32.to_le_bytes());
        v.extend_from_slice(&12u32.to_le_bytes());
        v.extend_from_slice(&[0u8; 64]);
        let c = decode_config(&v).unwrap();
        assert_eq!(c.total_spaces, 256);
        assert_eq!(c.minted_spaces, 12);
        assert!(decode_config(&v[..20]).is_err());
    }
}
