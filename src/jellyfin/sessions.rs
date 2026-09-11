use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::{CounterVec, GaugeVec, Opts};

use crate::jellyfin::library::find_library_for_path;
use crate::jellyfin::models::{NowPlayingItem, SessionInfo, TranscodingInfo};
use crate::jellyfin::server::ServerState;
use crate::metrics::{PLAY_LABELS, SERVER_LABELS, active_session_labels};

/// Jellyfin's `PlayState` only ever tells us paused-or-not; there's no
/// "buffering" signal like Plex's notification stream provides, so that state
/// simply doesn't exist here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Playing,
    Paused,
    Stopped,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Playing => "playing",
            SessionState::Paused => "paused",
            SessionState::Stopped => "stopped",
        }
    }
}

/// How long metrics for a stopped session are kept after its last update.
/// Used to prune tracked sessions and keep cardinality down.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// Safety-net timeout for a session stuck non-stopped without ever being
/// updated again, in case the polling task itself dies or stalls. Since every
/// poll reflects Jellyfin's full, current session list (there's nothing to
/// "miss" the way Plex can miss a push notification), this should in practice
/// never fire, but it's cheap defense-in-depth against the same class of bug.
const STALE_SESSION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// One continuous play of a single item within a session. Jellyfin reuses the
/// same session id across successive items played back to back (e.g.
/// autoplaying the next episode), unlike Plex's per-playback session key, so
/// an item change is tracked as closing out the current segment and opening a
/// new one rather than relabeling one long-running counter.
struct PlaySegment {
    item: NowPlayingItem,
    play_method: String,
    transcoding_info: Option<TranscodingInfo>,
    audio_stream_index: Option<i64>,

    play_count: u64,
    prev_played: Duration,
    play_started: Option<Instant>,
}

impl PlaySegment {
    fn total_played(&self) -> Duration {
        let mut total = self.prev_played;
        if let Some(started) = self.play_started {
            total += started.elapsed();
        }
        total
    }

    /// The bitrate actually being delivered: the transcode output bitrate when
    /// transcoding, otherwise the source stream's own bitrate (there's no
    /// session-reported "delivered bitrate" for direct play/stream the way
    /// Plex's session media node provides).
    fn bitrate_bps(&self) -> i64 {
        self.transcoding_info
            .as_ref()
            .map(|t| t.bitrate)
            .filter(|b| *b > 0)
            .unwrap_or_else(|| self.item.direct_play_bitrate(self.audio_stream_index))
    }

    /// The resolution actually being delivered: the transcode output
    /// resolution when transcoding, otherwise the source file's resolution.
    fn resolution(&self) -> i64 {
        self.transcoding_info
            .as_ref()
            .map(|t| t.height)
            .filter(|h| *h > 0)
            .unwrap_or(self.item.height)
    }
}

#[derive(Default)]
struct SessionEntry {
    current: Option<PlaySegment>,
    completed: Vec<PlaySegment>,

    user_name: String,
    client: String,
    device_name: String,

    state: Option<SessionState>,
    last_update: Option<Instant>,
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
    total_estimated_transmitted_bytes: f64,
}

pub struct Sessions {
    inner: Mutex<SessionsInner>,
    server: Arc<ServerState>,
}

/// Everything about a session's current playback that `Sessions::update` may
/// have fresh data for. Fields are `Option` because a poll of a since-vanished
/// session, or of an idle (non-playing) session, has nothing new to report.
#[derive(Default)]
pub struct SessionUpdate {
    pub now_playing: Option<NowPlayingItem>,
    pub play_method: Option<String>,
    pub transcoding_info: Option<TranscodingInfo>,
    pub audio_stream_index: Option<i64>,
    pub user_name: Option<String>,
    pub client: Option<String>,
    pub device_name: Option<String>,
}

impl Sessions {
    pub fn new(server: Arc<ServerState>) -> Arc<Self> {
        let sessions = Arc::new(Self {
            inner: Mutex::new(SessionsInner {
                sessions: HashMap::new(),
                total_estimated_transmitted_bytes: 0.0,
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

    /// Applies one full `/Sessions` poll: updates every session reported by
    /// Jellyfin, then treats any session we were tracking that's no longer in
    /// the list at all (client disconnected) as stopped.
    pub fn apply_poll(&self, current: Vec<SessionInfo>) {
        let seen: std::collections::HashSet<String> = current.iter().map(|s| s.id.clone()).collect();

        for session in current {
            let new_state = match &session.now_playing_item {
                None => SessionState::Stopped,
                Some(_) if session.play_state.is_paused => SessionState::Paused,
                Some(_) => SessionState::Playing,
            };

            self.update(
                &session.id,
                new_state,
                SessionUpdate {
                    now_playing: session.now_playing_item,
                    play_method: (!session.play_state.play_method.is_empty()).then_some(session.play_state.play_method),
                    transcoding_info: session.transcoding_info,
                    audio_stream_index: session.play_state.audio_stream_index,
                    user_name: Some(session.user_name),
                    client: Some(session.client),
                    device_name: Some(session.device_name),
                },
            );
        }

        // Only stop sessions once, on the poll where they first vanish -
        // otherwise re-marking an already-stopped, still-absent session as
        // stopped on every subsequent poll would keep refreshing its
        // last_update and it would never actually become eligible for pruning.
        let vanished: Vec<String> = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner
                .sessions
                .iter()
                .filter(|(id, entry)| !seen.contains(*id) && entry.state != Some(SessionState::Stopped))
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in vanished {
            self.update(&id, SessionState::Stopped, SessionUpdate::default());
        }
    }

    fn update(&self, session_id: &str, new_state: SessionState, update: SessionUpdate) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = inner.sessions.entry(session_id.to_string()).or_default();

        if let Some(u) = update.user_name {
            entry.user_name = u;
        }
        if let Some(c) = update.client {
            entry.client = c;
        }
        if let Some(d) = update.device_name {
            entry.device_name = d;
        }

        let item_changed = match (&entry.current, &update.now_playing) {
            (Some(seg), Some(new_item)) => seg.item.id != new_item.id && !new_item.id.is_empty(),
            _ => false,
        };

        let mut transmitted_delta = 0.0;

        if item_changed {
            let mut old = entry.current.take().expect("item_changed implies a current segment");
            if let Some(started) = old.play_started.take() {
                let elapsed = started.elapsed();
                old.prev_played += elapsed;
                transmitted_delta += old.bitrate_bps() as f64 * elapsed.as_secs_f64() / 8.0;
            }
            entry.completed.push(old);

            if let Some(item) = update.now_playing {
                let playing_now = new_state == SessionState::Playing;
                entry.current = Some(PlaySegment {
                    item,
                    play_method: update.play_method.unwrap_or_default(),
                    transcoding_info: update.transcoding_info,
                    audio_stream_index: update.audio_stream_index,
                    play_count: if playing_now { 1 } else { 0 },
                    prev_played: Duration::ZERO,
                    play_started: playing_now.then(Instant::now),
                });
            }
        } else {
            match update.now_playing {
                Some(item) if entry.current.is_none() => {
                    entry.current = Some(PlaySegment {
                        item,
                        play_method: update.play_method.unwrap_or_default(),
                        transcoding_info: update.transcoding_info,
                        audio_stream_index: update.audio_stream_index,
                        play_count: 0,
                        prev_played: Duration::ZERO,
                        play_started: None,
                    });
                }
                _ => {
                    if let Some(seg) = entry.current.as_mut() {
                        if let Some(m) = update.play_method {
                            seg.play_method = m;
                        }
                        if update.transcoding_info.is_some() {
                            seg.transcoding_info = update.transcoding_info;
                        }
                        if update.audio_stream_index.is_some() {
                            seg.audio_stream_index = update.audio_stream_index;
                        }
                    }
                }
            }

            if let Some(seg) = entry.current.as_mut() {
                if entry.state == Some(SessionState::Playing)
                    && new_state != SessionState::Playing
                    && let Some(started) = seg.play_started.take()
                {
                    let elapsed = started.elapsed();
                    seg.prev_played += elapsed;
                    transmitted_delta += seg.bitrate_bps() as f64 * elapsed.as_secs_f64() / 8.0;
                }
                if entry.state != Some(SessionState::Playing) && new_state == SessionState::Playing {
                    seg.play_started = Some(Instant::now());
                    seg.play_count += 1;
                }
            }
        }

        entry.state = Some(new_state);
        entry.last_update = Some(Instant::now());

        inner.total_estimated_transmitted_bytes += transmitted_delta;
    }

    fn extrapolated_transmitted_bytes(&self, inner: &SessionsInner) -> f64 {
        let mut total = inner.total_estimated_transmitted_bytes;

        for entry in inner.sessions.values() {
            if entry.state == Some(SessionState::Playing)
                && let Some(segment) = &entry.current
                && let Some(started) = segment.play_started
            {
                total += segment.bitrate_bps() as f64 * started.elapsed().as_secs_f64() / 8.0;
            }
        }

        total
    }
}

/// Prometheus collector for play/session gauges-as-counters. Recomputed from
/// the current session snapshot on every scrape, mirroring
/// `plex::sessions::SessionsCollector`. No `transcode_speed`/
/// `transcode_throttled` equivalent: Jellyfin's `TranscodingInfo` has no speed
/// multiplier or throttled flag to report.
pub struct SessionsCollector {
    sessions: Arc<Sessions>,
    plays_total: CounterVec,
    play_seconds_total: CounterVec,
    estimated_transmit_bytes_total: CounterVec,
    active_sessions: GaugeVec,
}

impl SessionsCollector {
    pub fn new(sessions: Arc<Sessions>) -> prometheus::Result<Self> {
        let active_session_labels = active_session_labels();

        Ok(Self {
            sessions,
            plays_total: CounterVec::new(Opts::new("jellyfin_plays_total", "Total play counts"), PLAY_LABELS)?,
            play_seconds_total: CounterVec::new(
                Opts::new("jellyfin_play_seconds_total", "Total play time per session"),
                PLAY_LABELS,
            )?,
            estimated_transmit_bytes_total: CounterVec::new(
                Opts::new(
                    "jellyfin_estimated_transmit_bytes_total",
                    "Total estimated bytes transmitted",
                ),
                SERVER_LABELS,
            )?,
            active_sessions: GaugeVec::new(
                Opts::new("jellyfin_active_sessions", "Currently active playback sessions"),
                &active_session_labels,
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
        descs
    }

    fn collect(&self) -> Vec<MetricFamily> {
        self.plays_total.reset();
        self.play_seconds_total.reset();
        self.estimated_transmit_bytes_total.reset();
        self.active_sessions.reset();

        let server = &self.sessions.server;
        let server_name = server.name();
        let server_id = server.id();
        let libraries = server.libraries();

        let inner = self.sessions.inner.lock().unwrap_or_else(|e| e.into_inner());

        let emit =
            |segment: &PlaySegment, entry: &SessionEntry, active_state: Option<SessionState>, session_id: &str| {
                let Some(library) = find_library_for_path(&libraries, &segment.item.path) else {
                    return;
                };

                let (title, child_title, grandchild_title) = segment.item.play_labels();
                let stream_resolution = segment.resolution().to_string();
                let stream_file_resolution = segment.item.height.to_string();
                let bitrate = segment.bitrate_bps().to_string();

                let label_values: [&str; 18] = [
                    "jellyfin",
                    &server_name,
                    &server_id,
                    &library.library_type,
                    &library.name,
                    &library.id,
                    &segment.item.item_type,
                    title,
                    child_title,
                    grandchild_title,
                    &segment.play_method,
                    &stream_resolution,
                    &stream_file_resolution,
                    &bitrate,
                    &entry.device_name,
                    &entry.client,
                    &entry.user_name,
                    session_id,
                ];

                self.plays_total
                    .with_label_values(&label_values)
                    .inc_by(segment.play_count as f64);
                self.play_seconds_total
                    .with_label_values(&label_values)
                    .inc_by(segment.total_played().as_secs_f64());

                if let Some(state) = active_state {
                    let mut active_label_values = label_values.to_vec();
                    active_label_values.push(state.as_str());
                    self.active_sessions.with_label_values(&active_label_values).set(1.0);
                }
            };

        for (session_id, entry) in inner.sessions.iter() {
            for segment in &entry.completed {
                emit(segment, entry, None, session_id);
            }
            if let Some(segment) = &entry.current {
                let active_state = entry.state.filter(|s| *s != SessionState::Stopped);
                emit(segment, entry, active_state, session_id);
            }
        }

        // Held back until the server identity is known, same reasoning as the
        // Plex exporter: a series labeled with empty strings would otherwise
        // linger alongside the real one forever.
        if !server_id.is_empty() {
            self.estimated_transmit_bytes_total
                .with_label_values(&["jellyfin", &server_name, &server_id])
                .inc_by(self.sessions.extrapolated_transmitted_bytes(&inner));
        }

        drop(inner);

        let mut mfs = self.plays_total.collect();
        mfs.extend(self.play_seconds_total.collect());
        mfs.extend(self.estimated_transmit_bytes_total.collect());
        mfs.extend(self.active_sessions.collect());
        mfs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn active_sessions_survive_until_stale_timeout_even_without_a_new_poll() {
        assert!(!should_prune(
            Some(SessionState::Playing),
            SESSION_TIMEOUT + Duration::from_secs(1)
        ));
        assert!(should_prune(
            Some(SessionState::Playing),
            STALE_SESSION_TIMEOUT + Duration::from_secs(1)
        ));
    }
}
