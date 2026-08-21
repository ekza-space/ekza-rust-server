use std::error::Error;

use std::sync::Arc;

use server::app;
use server::config::Config;
use server::origin_guard::OriginGuardLayer;
use server::realtime;
use server::state::AppState;
use server::telemetry;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Load local env file if present (do not require it).
    let _ = dotenvy::dotenv();

    let config = Config::from_env()?;
    telemetry::init(&config);

    let state = AppState::new(config.clone());
    let (socket_layer, io) = realtime::build_layer(&config).await?;
    realtime::register_handlers(&io);

    // Layer order (outermost first): origin guard → CORS → socket.io → REST/static.
    // CORS must wrap the socket service or `/socket.io` polling gets no headers.
    let app = app::build_app(state, &config)
        .layer(socket_layer)
        .layer(app::cors_layer(&config))
        .layer(OriginGuardLayer::new(Arc::new(config.clone())));

    let addr = config.bind_addr();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        addr = %addr,
        data_dir = %config.data_dir,
        rpc = %config.solana_rpc_url,
        program = %config.space_program_id,
        origins = ?config.cors_allowed_origins,
        "server listening"
    );

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
