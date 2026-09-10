use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::watch;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::http::Uri;

use crate::plex::models::{CurrentSessions, MediaMetadataResponse, WebsocketNotification};
use crate::plex::server::ServerState;
use crate::plex::sessions::{SessionState, Sessions};

const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Connects to the Plex server's notification websocket and forwards
/// playback state changes into `Sessions`. Reconnects with a fixed delay on
/// error or unexpected disconnect, until `shutdown` fires.
pub async fn run(server: Arc<ServerState>, sessions: Arc<Sessions>, mut shutdown: watch::Receiver<bool>) {
    let metrics = Arc::clone(server.metrics());

    let mut first_attempt = true;
    loop {
        if *shutdown.borrow() {
            return;
        }

        // The server's identity stays empty until a refresh has succeeded, which
        // may not have happened yet now that an unreachable Plex no longer stops
        // the exporter from starting. Labeling the websocket metrics with empty
        // strings would strand that series once the real identity arrives, so
        // wait for it rather than publish a placeholder.
        let name = server.name();
        let id = server.id();
        if id.is_empty() {
            tracing::debug!("waiting for plex server identity before connecting to the websocket");
            tokio::select! {
                _ = tokio::time::sleep(RECONNECT_DELAY) => continue,
                _ = shutdown.changed() => return,
            }
        }
        let label_values = ["plex", name.as_str(), id.as_str()];
        metrics.websocket_connected.with_label_values(&label_values).set(0.0);

        if !first_attempt {
            metrics
                .websocket_reconnects_total
                .with_label_values(&label_values)
                .inc();
        }
        first_attempt = false;

        tokio::select! {
            result = connect_and_listen(&server, &sessions, &metrics, &label_values) => {
                match result {
                    Ok(()) => tracing::info!("plex websocket closed"),
                    Err(e) => {
                        metrics.scrape_errors_total.with_label_values(&["websocket"]).inc();
                        tracing::error!(error = %e, "plex websocket error, reconnecting");
                    }
                }
            }
            _ = shutdown.changed() => return,
        }

        metrics.websocket_connected.with_label_values(&label_values).set(0.0);

        tokio::select! {
            _ = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = shutdown.changed() => return,
        }
    }
}

async fn connect_and_listen(
    server: &Arc<ServerState>,
    sessions: &Arc<Sessions>,
    metrics: &crate::metrics::GlobalMetrics,
    label_values: &[&str; 3],
) -> anyhow::Result<()> {
    let base_url = &server.client.base_url;
    let ws_scheme = if base_url.scheme() == "https" { "wss" } else { "ws" };
    let host = base_url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("plex server url has no host"))?;
    let authority = match base_url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let ws_url = format!("{ws_scheme}://{authority}/:/websockets/notifications");
    let uri: Uri = ws_url.parse()?;

    let request = ClientRequestBuilder::new(uri).with_header("X-Plex-Token", server.client.token.clone());

    let (ws_stream, _) = connect_async(request).await?;
    tracing::info!("connected to plex websocket notifications");
    metrics.websocket_connected.with_label_values(label_values).set(1.0);

    let (mut write, mut read) = ws_stream.split();
    let mut keepalive = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if write.send(Message::text(unix_millis().to_string())).await.is_err() {
                    return Ok(());
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(e) = handle_message(&text, server, sessions).await {
                            tracing::error!(error = %e, "error handling websocket notification");
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                }
            }
        }
    }
}

async fn handle_message(text: &str, server: &Arc<ServerState>, sessions: &Arc<Sessions>) -> anyhow::Result<()> {
    let notification: WebsocketNotification = serde_json::from_str(text)?;
    if notification.notification_container.notification_type != "playing" {
        return Ok(());
    }

    for n in notification.notification_container.play_session_state_notification {
        if let Err(e) = handle_notification(n, server, sessions).await {
            tracing::warn!(error = %e, "skipping websocket notification");
        }
    }

    Ok(())
}

/// Number of times to retry looking up a session in `/status/sessions` after
/// a "playing" notification. Plex's session list lags slightly behind its own
/// websocket notifications, so a fresh session can briefly be missing from it.
const SESSION_LOOKUP_RETRIES: u32 = 3;
const SESSION_LOOKUP_RETRY_DELAY: Duration = Duration::from_millis(200);

async fn handle_notification(
    n: crate::plex::models::PlaySessionStateNotification,
    server: &Arc<ServerState>,
    sessions: &Arc<Sessions>,
) -> anyhow::Result<()> {
    let state = SessionState::from_plex(&n.state);

    if state == SessionState::Stopped {
        // When the session is stopped we can't look up the user info or media anymore.
        sessions.update(&n.session_key, state, None, None);
        return Ok(());
    }

    let mut session = None;
    for attempt in 0..=SESSION_LOOKUP_RETRIES {
        let current: CurrentSessions = server.get("sessions", "/status/sessions").await?;
        session = current
            .media_container
            .metadata
            .into_iter()
            .find(|m| m.session_key == n.session_key);

        if session.is_some() || attempt == SESSION_LOOKUP_RETRIES {
            break;
        }
        tokio::time::sleep(SESSION_LOOKUP_RETRY_DELAY).await;
    }

    let Some(session) = session else {
        anyhow::bail!("no active session found for session key {}", n.session_key);
    };

    let metadata: MediaMetadataResponse = server
        .get("metadata", &format!("/library/metadata/{}", n.rating_key))
        .await?;

    let Some(media) = metadata.media_container.metadata.into_iter().next() else {
        anyhow::bail!("no metadata found for rating key {}", n.rating_key);
    };

    tracing::info!(
        session_key = %n.session_key,
        user = %session.user.title,
        state = %n.state,
        media_title = %media.title,
        media_id = %media.rating_key,
        "received PlaySessionStateNotification",
    );

    sessions.update(&n.session_key, state, Some(session), Some(media));

    Ok(())
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
