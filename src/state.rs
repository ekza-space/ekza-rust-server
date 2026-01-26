use std::sync::Arc;
use std::time::Instant;

use crate::config::Config;
use crate::services::Services;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub services: Services,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            services: Services::new(),
            started_at: Instant::now(),
        }
    }
}
