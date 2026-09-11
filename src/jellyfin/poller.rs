use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::jellyfin::models::SessionInfo;
use crate::jellyfin::server::ServerState;
use crate::jellyfin::sessions::Sessions;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Polls the Jellyfin server's `/Sessions` endpoint and feeds the full
/// snapshot into `Sessions` on every tick.
///
/// Unlike Plex's push-notification listener, this is a plain poll: Jellyfin's
/// websocket doesn't reliably push periodic session updates even after
/// requesting them (verified live against a 12.0 server - only keepalives came
/// back), so REST polling is the option that actually works. It also has the
/// advantage that `/Sessions` already embeds full `NowPlayingItem`,
/// `PlayState` and `TranscodingInfo` detail, so no second per-session
/// metadata fetch is needed the way Plex's listener requires.
pub async fn run(server: Arc<ServerState>, sessions: Arc<Sessions>, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match server.get::<Vec<SessionInfo>>("sessions", "/Sessions").await {
                    Ok(current) => sessions.apply_poll(current),
                    Err(e) => tracing::warn!(error = %e, "failed to poll jellyfin sessions"),
                }
            }
            _ = shutdown.changed() => return,
        }

        if *shutdown.borrow() {
            return;
        }
    }
}
