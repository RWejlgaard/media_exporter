mod jellyfin;
mod media;
mod metrics;
mod plex;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus::core::Collector;
use prometheus::{Encoder, Registry, TextEncoder};
use tokio::signal;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use jellyfin::poller as jellyfin_poller;
use jellyfin::server::{ServerCollector as JellyfinServerCollector, ServerState as JellyfinServerState};
use jellyfin::sessions::{Sessions as JellyfinSessions, SessionsCollector as JellyfinSessionsCollector};
use media::MediaCollector;
use metrics::{GlobalMetrics, JellyfinGlobalMetrics};
use plex::listener as plex_listener;
use plex::server::{ServerCollector as PlexServerCollector, ServerState as PlexServerState};
use plex::sessions::{Sessions as PlexSessions, SessionsCollector as PlexSessionsCollector};

const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0";
const DEFAULT_PORT: &str = "9000";
const DEFAULT_JELLYFIN_LIBRARY_STATS_INTERVAL_SECS: u64 = 1800;

#[derive(Clone)]
struct AppState {
    registry: Arc<Registry>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let plex_server = std::env::var("PLEX_SERVER").ok();
    let jellyfin_server = std::env::var("JELLYFIN_SERVER").ok();
    if plex_server.is_none() && jellyfin_server.is_none() {
        anyhow::bail!("at least one of PLEX_SERVER or JELLYFIN_SERVER must be specified");
    }

    let bind_address = std::env::var("BIND_ADDRESS").unwrap_or_else(|_| DEFAULT_BIND_ADDRESS.to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| DEFAULT_PORT.to_string());
    let metrics_addr = format!("{bind_address}:{port}");

    let registry = Arc::new(Registry::new());
    // The process collector is Linux-only in the prometheus crate; gating it
    // keeps the exporter buildable on other platforms for local development.
    #[cfg(target_os = "linux")]
    registry.register(Box::new(prometheus::process_collector::ProcessCollector::for_self()))?;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut background_tasks: Vec<JoinHandle<()>> = Vec::new();
    // Collected per backend rather than registered straight away, so that with
    // more than one backend they can be wrapped in a `MediaCollector`.
    let mut backend_collectors: Vec<Vec<Box<dyn Collector>>> = Vec::new();

    if let Some(server_url) = plex_server {
        let plex_token = std::env::var("PLEX_TOKEN")
            .map_err(|_| anyhow::anyhow!("PLEX_TOKEN environment variable must be specified alongside PLEX_SERVER"))?;

        let global_metrics = Arc::new(GlobalMetrics::new()?);
        let mut collectors = global_metrics.collectors();

        let server = PlexServerState::connect(&server_url, &plex_token, Arc::clone(&global_metrics))
            .await
            .map_err(|e| anyhow::anyhow!("cannot initialize plex client: {e}"))?;

        collectors.push(Box::new(PlexServerCollector::new(Arc::clone(&server))?));

        let sessions = PlexSessions::new(Arc::clone(&server));
        collectors.push(Box::new(PlexSessionsCollector::new(Arc::clone(&sessions))?));
        backend_collectors.push(collectors);

        background_tasks.push(tokio::spawn(plex_listener::run(server, sessions, shutdown_rx.clone())));
    }

    if let Some(server_url) = jellyfin_server {
        let jellyfin_token = std::env::var("JELLYFIN_TOKEN").map_err(|_| {
            anyhow::anyhow!("JELLYFIN_TOKEN environment variable must be specified alongside JELLYFIN_SERVER")
        })?;
        let library_stats_interval = std::env::var("JELLYFIN_LIBRARY_STATS_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_JELLYFIN_LIBRARY_STATS_INTERVAL_SECS);

        let global_metrics = Arc::new(JellyfinGlobalMetrics::new()?);
        let mut collectors = global_metrics.collectors();

        let server = JellyfinServerState::connect(
            &server_url,
            &jellyfin_token,
            Duration::from_secs(library_stats_interval),
            Arc::clone(&global_metrics),
        )
        .await
        .map_err(|e| anyhow::anyhow!("cannot initialize jellyfin client: {e}"))?;

        collectors.push(Box::new(JellyfinServerCollector::new(Arc::clone(&server))?));

        let sessions = JellyfinSessions::new(Arc::clone(&server));
        collectors.push(Box::new(JellyfinSessionsCollector::new(Arc::clone(&sessions))?));
        backend_collectors.push(collectors);

        background_tasks.push(tokio::spawn(jellyfin_poller::run(
            server,
            sessions,
            shutdown_rx.clone(),
        )));
    }

    if backend_collectors.len() > 1 {
        let sources = backend_collectors.into_iter().flatten().collect();
        registry.register(Box::new(MediaCollector::new(sources)?))?;
    } else {
        for collector in backend_collectors.into_iter().flatten() {
            registry.register(collector)?;
        }
    }

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

    axum::serve(tcp_listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::debug!("shutting down");
    let _ = shutdown_tx.send(true);
    for task in background_tasks {
        task.abort();
    }

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
