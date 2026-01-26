use axum::routing::post;
use axum::Router;

use crate::state::AppState;

mod echo;

pub fn router() -> Router<AppState> {
    Router::new().route("/echo", post(echo::handler))
}
