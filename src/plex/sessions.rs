use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::{CounterVec, GaugeVec, Opts};

use crate::metrics::{PLAY_LABELS, SERVER_LABELS, active_session_labels};
use crate::plex::models::Metadata;
use crate::plex::server::ServerState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Playing,
    Stopped,
    Paused,
    Buffering,
}

impl SessionState {
    pub fn from_plex(state: &str) -> Self {
        match state {
            "playing" => SessionState::Playing,
            "paused" => SessionState::Paused,
            "buffering" => SessionState::Buffering,
            _ => SessionState::Stopped,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Playing => "playing",
            SessionState::Paused => "paused",
            SessionState::Buffering => "buffering",
            SessionState::Stopped => "stopped",
        }
    }
}

/// How long metrics for a stopped session are kept after its last update.
/// Used to prune tracked sessions and keep cardinality down.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// Safety-net timeout for sessions that never receive a "stopped"
/// notification (e.g. the exporter misses it across a reconnect or the Plex
/// server restarts mid-session). Without this, such a session would report
/// `plex_active_sessions=1` forever.
const STALE_SESSION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
struct SessionEntry {
    session: Metadata,
    media: Metadata,
    state: Option<SessionState>,
    last_update: Option<Instant>,
    play_started: Option<Instant>,
    prev_played: Duration,
    play_count: u64,
}

/// Whether a tracked session should be dropped, given its current state and
/// time since its last update.
fn should_prune(state: Option<SessionState>, elapsed: Duration) -> bool {
    match state {
        Some(SessionState::Stopped) => elapsed > SESSION_TIMEOUT,
        _ => elapsed > STALE_SESSION_TIMEOUT,
    }
}

struct SessionsInner {
    sessions: HashMap<String, SessionEntry>,
    total_estimated_transmitted_kbits: f64,
}

pub struct Sessions {
    inner: Mutex<SessionsInner>,
    server: Arc<ServerState>,
}

impl Sessions {
    pub fn new(server: Arc<ServerState>) -> Arc<Self> {
        let sessions = Arc::new(Self {
            inner: Mutex::new(SessionsInner {
                sessions: HashMap::new(),
                total_estimated_transmitted_kbits: 0.0,
            }),
            server,
        });

        let weak = Arc::downgrade(&sessions);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(sessions) = weak.upgrade() else {
                    break;
                };
                sessions.prune_old_sessions();
            }
        });

        sessions
    }

    fn prune_old_sessions(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.sessions.retain(|_, entry| {
            !entry
                .last_update
                .is_some_and(|t| should_prune(entry.state, t.elapsed()))
        });
    }

    pub fn update(
        &self,
        session_id: &str,
        new_state: SessionState,
        new_session: Option<Metadata>,
        media: Option<Metadata>,
    ) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = inner.sessions.entry(session_id.to_string()).or_default();

        if let Some(s) = new_session {
            entry.session = s;
        }
        if let Some(m) = media {
            entry.media = m;
        }

        let mut transmitted_delta = 0.0;

        if entry.state == Some(SessionState::Playing) && new_state != SessionState::Playing {
            // Session was playing but now is not: flatten the play time into the total.
            if let Some(started) = entry.play_started {
                let elapsed = started.elapsed();
                entry.prev_played += elapsed;
                let bitrate = entry.session.media.first().map(|m| m.bitrate).unwrap_or(0) as f64;
                transmitted_delta = elapsed.as_secs_f64() * bitrate;
            }
        }

        if entry.state != Some(SessionState::Playing) && new_state == SessionState::Playing {
            entry.play_started = Some(Instant::now());
            entry.play_count += 1;
        }

        entry.state = Some(new_state);
        entry.last_update = Some(Instant::now());

        inner.total_estimated_transmitted_kbits += transmitted_delta;
    }

    fn extrapolated_transmitted_bytes(&self, inner: &SessionsInner) -> f64 {
        let mut total = inner.total_estimated_transmitted_kbits;

        for entry in inner.sessions.values() {
            if entry.state == Some(SessionState::Playing)
                && let Some(started) = entry.play_started
            {
                let bitrate = entry.session.media.first().map(|m| m.bitrate).unwrap_or(0) as f64;
                total += started.elapsed().as_secs_f64() * bitrate;
            }
        }

        total * 128.0 // Kbits -> Bytes, 1024 / 8
    }
}

/// Prometheus collector for play/session gauges-as-counters. Recomputed
/// from the current session snapshot on every scrape, mirroring the
/// upstream Go exporter's use of `prometheus.MustNewConstMetric` in
/// `sessions.Collect`.
pub struct SessionsCollector {
    sessions: Arc<Sessions>,
    plays_total: CounterVec,
    play_seconds_total: CounterVec,
    estimated_transmit_bytes_total: CounterVec,
    active_sessions: GaugeVec,
    transcode_speed: GaugeVec,
    transcode_throttled: GaugeVec,
}

impl SessionsCollector {
    pub fn new(sessions: Arc<Sessions>) -> prometheus::Result<Self> {
        let active_session_labels = active_session_labels();

        Ok(Self {
            sessions,
            plays_total: CounterVec::new(Opts::new("plex_plays_total", "Total play counts"), PLAY_LABELS)?,
            play_seconds_total: CounterVec::new(
                Opts::new("plex_play_seconds_total", "Total play time per session"),
                PLAY_LABELS,
            )?,
            estimated_transmit_bytes_total: CounterVec::new(
                Opts::new(
                    "plex_estimated_transmit_bytes_total",
                    "Total estimated bytes transmitted",
                ),
                SERVER_LABELS,
            )?,
            active_sessions: GaugeVec::new(
                Opts::new("plex_active_sessions", "Currently active playback sessions"),
                &active_session_labels,
            )?,
            transcode_speed: GaugeVec::new(
                Opts::new(
                    "plex_transcode_speed",
                    "Current transcode speed for an active session, where 1.0 is real-time",
                ),
                PLAY_LABELS,
            )?,
            transcode_throttled: GaugeVec::new(
                Opts::new(
                    "plex_transcode_throttled",
                    "Whether an active session's transcode is currently throttled",
                ),
                PLAY_LABELS,
            )?,
        })
    }
}

impl Collector for SessionsCollector {
    fn desc(&self) -> Vec<&Desc> {
        let mut descs = self.plays_total.desc();
        descs.extend(self.play_seconds_total.desc());
        descs.extend(self.estimated_transmit_bytes_total.desc());
        descs.extend(self.active_sessions.desc());
        descs.extend(self.transcode_speed.desc());
        descs.extend(self.transcode_throttled.desc());
        descs
    }

    fn collect(&self) -> Vec<MetricFamily> {
        self.plays_total.reset();
        self.play_seconds_total.reset();
        self.estimated_transmit_bytes_total.reset();
        self.active_sessions.reset();
        self.transcode_speed.reset();
        self.transcode_throttled.reset();

        let server = &self.sessions.server;
        let server_name = server.name();
        let server_id = server.id();

        let inner = self.sessions.inner.lock().unwrap_or_else(|e| e.into_inner());

        for (id, entry) in inner.sessions.iter() {
            let Some(play_started) = entry.play_started else {
                continue;
            };

            let library_section_id = entry.media.library_section_id.to_string();
            let Some(library) = server.library(&library_section_id) else {
                continue;
            };

            let (title, child_title, grandchild_title) = entry.media.play_labels();

            let media0 = entry.session.media.first().cloned().unwrap_or_default();
            let part0 = media0.part.first().cloned().unwrap_or_default();
            let file_resolution = entry
                .media
                .media
                .first()
                .map(|m| m.video_resolution.as_str())
                .unwrap_or("");
            let bitrate = media0.bitrate.to_string();

            let label_values: [&str; 18] = [
                "plex",
                &server_name,
                &server_id,
                &library.library_type,
                &library.name,
                &library.id,
                &entry.media.media_type,
                title,
                child_title,
                grandchild_title,
                &part0.decision,
                &media0.video_resolution,
                file_resolution,
                &bitrate,
                &entry.session.player.device,
                &entry.session.player.product,
                &entry.session.user.title,
                id,
            ];

            self.plays_total
                .with_label_values(&label_values)
                .inc_by(entry.play_count as f64);

            let mut total_play_time = entry.prev_played;
            if entry.state == Some(SessionState::Playing) {
                total_play_time += play_started.elapsed();
            }
            self.play_seconds_total
                .with_label_values(&label_values)
                .inc_by(total_play_time.as_secs_f64());

            if let Some(state) = entry.state
                && state != SessionState::Stopped
            {
                let mut active_label_values = label_values.to_vec();
                active_label_values.push(state.as_str());
                self.active_sessions.with_label_values(&active_label_values).set(1.0);

                if let Some(transcode) = &entry.session.transcode_session {
                    self.transcode_speed
                        .with_label_values(&label_values)
                        .set(transcode.speed);
                    self.transcode_throttled
                        .with_label_values(&label_values)
                        .set(if transcode.throttled { 1.0 } else { 0.0 });
                }
            }
        }

        // Held back until the server identity is known. Emitting it before the
        // first successful refresh would publish a series labeled with empty
        // strings, which then lingers alongside the real one forever.
        if !server_id.is_empty() {
            self.estimated_transmit_bytes_total
                .with_label_values(&["plex", &server_name, &server_id])
                .inc_by(self.sessions.extrapolated_transmitted_bytes(&inner));
        }

        drop(inner);

        let mut mfs = self.plays_total.collect();
        mfs.extend(self.play_seconds_total.collect());
        mfs.extend(self.estimated_transmit_bytes_total.collect());
        mfs.extend(self.active_sessions.collect());
        mfs.extend(self.transcode_speed.collect());
        mfs.extend(self.transcode_throttled.collect());
        mfs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_from_plex_maps_known_states() {
        assert_eq!(SessionState::from_plex("playing"), SessionState::Playing);
        assert_eq!(SessionState::from_plex("paused"), SessionState::Paused);
        assert_eq!(SessionState::from_plex("buffering"), SessionState::Buffering);
    }

    #[test]
    fn session_state_from_plex_defaults_unknown_to_stopped() {
        assert_eq!(SessionState::from_plex("stopped"), SessionState::Stopped);
        assert_eq!(SessionState::from_plex("garbage"), SessionState::Stopped);
    }

    #[test]
    fn stopped_sessions_are_pruned_after_session_timeout() {
        assert!(!should_prune(
            Some(SessionState::Stopped),
            SESSION_TIMEOUT - Duration::from_secs(1)
        ));
        assert!(should_prune(
            Some(SessionState::Stopped),
            SESSION_TIMEOUT + Duration::from_secs(1)
        ));
    }

    #[test]
    fn active_sessions_survive_until_stale_timeout_even_without_a_stop_event() {
        // A session stuck in "playing" (e.g. a missed stop notification) must
        // not be pruned at the short stopped-session timeout...
        assert!(!should_prune(
            Some(SessionState::Playing),
            SESSION_TIMEOUT + Duration::from_secs(1)
        ));
        // ...but must eventually be reclaimed by the stale safety net.
        assert!(should_prune(
            Some(SessionState::Playing),
            STALE_SESSION_TIMEOUT + Duration::from_secs(1)
        ));
    }
}
