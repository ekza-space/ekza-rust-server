use axum::http::{HeaderValue, Method};
use axum::Router;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::routes;
use crate::state::AppState;

pub fn build_app(state: AppState, config: &Config) -> Router {
    let static_service = ServeDir::new(&config.static_dir)
        .fallback(ServeFile::new(format!("{}/index.html", config.static_dir)));

    routes::router(state)
        .layer(TraceLayer::new_for_http())
        .fallback_service(static_service)
}

/// Applied as the **outermost** HTTP layer (see `main.rs`) so `/socket.io` responses
/// from socketioxide still get `Access-Control-Allow-*` headers for browser clients.
pub fn cors_layer(config: &Config) -> CorsLayer {
    let mut cors = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(Any);

    if config.cors_allow_any() {
        cors = cors.allow_origin(Any);
    } else {
        let origins = config
            .cors_allowed_origins
            .iter()
            .filter_map(|origin| match HeaderValue::from_str(origin) {
                Ok(value) => Some(value),
                Err(_) => {
                    tracing::warn!(origin, "Invalid CORS origin ignored");
                    None
                }
            })
            .collect::<Vec<_>>();

        cors = cors.allow_origin(AllowOrigin::list(origins));
    }

    cors
}
