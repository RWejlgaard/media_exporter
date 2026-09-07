mod metrics;
mod plex;

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus::{Encoder, Registry, TextEncoder};
use tokio::signal;
use tokio::sync::watch;

use metrics::GlobalMetrics;
use plex::listener;
use plex::server::{ServerCollector, ServerState};
use plex::sessions::{Sessions, SessionsCollector};

const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0";
const DEFAULT_PORT: &str = "9000";

#[derive(Clone)]
struct AppState {
    registry: Arc<Registry>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let server_url = std::env::var("PLEX_SERVER")
        .map_err(|_| anyhow::anyhow!("PLEX_SERVER environment variable must be specified"))?;
    let plex_token = std::env::var("PLEX_TOKEN")
        .map_err(|_| anyhow::anyhow!("PLEX_TOKEN environment variable must be specified"))?;
    let bind_address = std::env::var("BIND_ADDRESS").unwrap_or_else(|_| DEFAULT_BIND_ADDRESS.to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| DEFAULT_PORT.to_string());
    let metrics_addr = format!("{bind_address}:{port}");

    let registry = Arc::new(Registry::new());
    registry.register(Box::new(prometheus::process_collector::ProcessCollector::for_self()))?;

    let global_metrics = Arc::new(GlobalMetrics::new()?);
    global_metrics.register(&registry)?;

    let server = ServerState::connect(&server_url, &plex_token, Arc::clone(&global_metrics))
        .await
        .map_err(|e| anyhow::anyhow!("cannot initialize connection to plex server: {e}"))?;

    registry.register(Box::new(ServerCollector::new(Arc::clone(&server))?))?;

    let sessions = Sessions::new(Arc::clone(&server));
    registry.register(Box::new(SessionsCollector::new(Arc::clone(&sessions))?))?;

    let app_state = AppState {
        registry: Arc::clone(&registry),
    };
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(app_state);

    let tcp_listener = tokio::net::TcpListener::bind(&metrics_addr)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind to {metrics_addr}: {e}"))?;
    tracing::info!("starting metrics server on {metrics_addr}");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let listener_task = tokio::spawn(listener::run(Arc::clone(&server), sessions, shutdown_rx));

    axum::serve(tcp_listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::debug!("shutting down");
    let _ = shutdown_tx.send(true);
    listener_task.abort();

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let metric_families = state.registry.gather();
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        tracing::error!(error = %e, "failed to encode metrics");
        return (StatusCode::INTERNAL_SERVER_ERROR, String::new());
    }
    (StatusCode::OK, String::from_utf8(buffer).unwrap_or_default())
}
