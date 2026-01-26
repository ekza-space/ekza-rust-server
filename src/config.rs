use std::env;

use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub log_level: String,
    pub cors_allowed_origins: Vec<String>,
    pub static_dir: String,
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
        let cors_allowed_origins =
            parse_origins(&env::var("CORS_ALLOWED_ORIGINS").unwrap_or_else(|_| "*".to_string()))?;
        let static_dir = env::var("STATIC_DIR").unwrap_or_else(|_| "build".to_string());

        Ok(Self {
            host,
            port,
            log_level,
            cors_allowed_origins,
            static_dir,
        })
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn cors_allow_any(&self) -> bool {
        self.cors_allowed_origins.iter().any(|origin| origin == "*")
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
}

fn parse_origins(raw: &str) -> Result<Vec<String>, ConfigError> {
    let origins: Vec<String> = raw
        .split(',')
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect();

    if origins.is_empty() {
        return Err(ConfigError::EmptyCorsOrigins);
    }

    Ok(origins)
}
