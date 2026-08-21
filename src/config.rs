use std::env;
use std::str::FromStr;

use solana_pubkey::Pubkey;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub log_level: String,
    /// Browser Origin allowlist. Applied to REST (CORS) and to the Socket.IO
    /// handshake (requests carrying a non-matching `Origin` are refused).
    pub cors_allowed_origins: Vec<String>,
    pub static_dir: String,
    /// Directory for persisted room state (one JSON file per room).
    pub data_dir: String,
    /// Solana JSON-RPC endpoint used to resolve space ownership.
    pub solana_rpc_url: String,
    /// `solana_ekza_space` program id.
    pub space_program_id: Pubkey,
    /// How long a resolved holder/editor set is trusted before re-querying the chain.
    pub ownership_cache_secs: u64,
    /// Optional comma-separated list of wallets allowed to edit ANY room
    /// (ops / moderation). Empty by default.
    pub moderators: Vec<Pubkey>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        if host.trim().is_empty() {
            return Err(ConfigError::EmptyHost);
        }

        let port_raw = env::var("PORT").unwrap_or_else(|_| "3001".to_string());
        let port = port_raw
            .parse::<u16>()
            .map_err(|_| ConfigError::InvalidPort(port_raw.clone()))?;
        if port == 0 {
            return Err(ConfigError::InvalidPort(port_raw));
        }

        let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
        // No wildcard default: an unset allowlist means "browsers from nowhere".
        let cors_allowed_origins = parse_origins(
            &env::var("CORS_ALLOWED_ORIGINS")
                .unwrap_or_else(|_| "https://space.ekza.io".to_string()),
        )?;
        let static_dir = env::var("STATIC_DIR").unwrap_or_else(|_| "build".to_string());
        let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string());

        let solana_rpc_url = env::var("SOLANA_RPC_URL")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
        let space_program_id_raw = env::var("SPACE_PROGRAM_ID")
            .unwrap_or_else(|_| "2WtuXG6AX3erRp6eK5WiSTEEBec5zprQ7qLyLENfMQEH".to_string());
        let space_program_id = Pubkey::from_str(space_program_id_raw.trim())
            .map_err(|_| ConfigError::InvalidPubkey("SPACE_PROGRAM_ID", space_program_id_raw))?;

        let ownership_cache_secs = env::var("OWNERSHIP_CACHE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);

        let moderators = env::var("MODERATOR_WALLETS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                Pubkey::from_str(s)
                    .map_err(|_| ConfigError::InvalidPubkey("MODERATOR_WALLETS", s.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            host,
            port,
            log_level,
            cors_allowed_origins,
            static_dir,
            data_dir,
            solana_rpc_url,
            space_program_id,
            ownership_cache_secs,
            moderators,
        })
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn cors_allow_any(&self) -> bool {
        self.cors_allowed_origins.iter().any(|origin| origin == "*")
    }

    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.cors_allow_any()
            || self
                .cors_allowed_origins
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(origin.trim_end_matches('/')))
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("HOST must not be empty")]
    EmptyHost,
    #[error("invalid PORT value: {0}")]
    InvalidPort(String),
    #[error("CORS_ALLOWED_ORIGINS must not be empty")]
    EmptyCorsOrigins,
    #[error("invalid pubkey in {0}: {1}")]
    InvalidPubkey(&'static str, String),
}

fn parse_origins(raw: &str) -> Result<Vec<String>, ConfigError> {
    let origins: Vec<String> = raw
        .split(',')
        .map(|value| value.trim().trim_end_matches('/'))
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect();

    if origins.is_empty() {
        return Err(ConfigError::EmptyCorsOrigins);
    }

    Ok(origins)
}
