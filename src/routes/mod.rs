use axum::routing::get;
use axum::Router;

use crate::state::AppState;

mod health;
pub mod v1;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health::handler))
        .nest("/api/v1", v1::router())
        .with_state(state)
}
