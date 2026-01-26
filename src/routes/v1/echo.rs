use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct EchoRequest {
    pub message: String,
}

#[derive(Serialize)]
pub struct EchoResponse {
    pub message: String,
}

pub async fn handler(
    State(state): State<AppState>,
    Json(payload): Json<EchoRequest>,
) -> AppResult<Json<EchoResponse>> {
    let message = state.services.echo.echo(payload.message);
    Ok(Json(EchoResponse { message }))
}
