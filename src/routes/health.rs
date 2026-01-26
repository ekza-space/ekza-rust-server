use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct HealthResponse {
    status: &'static str,
    uptime_secs: u64,
}

pub async fn handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let uptime_secs = state.started_at.elapsed().as_secs();
    Json(HealthResponse {
        status: "ok",
        uptime_secs,
    })
}
