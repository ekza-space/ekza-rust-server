use std::error::Error;

use ekza_rust_server::app;
use ekza_rust_server::config::Config;
use ekza_rust_server::realtime;
use ekza_rust_server::state::AppState;
use ekza_rust_server::telemetry;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    telemetry::init(&config);

    let state = AppState::new(config.clone());
    let (socket_layer, io) = realtime::build_layer();
    realtime::register_handlers(&io);

    let app = app::build_app(state, &config)
        .layer(socket_layer)
        .layer(app::cors_layer(&config));

    let addr = config.bind_addr();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};

        let mut term_signal =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        term_signal.recv().await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
