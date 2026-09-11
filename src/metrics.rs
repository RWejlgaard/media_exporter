use prometheus::{CounterVec, Gauge, GaugeVec, Opts};

pub const SERVER_LABELS: &[&str] = &["server_type", "server", "server_id"];

/// Identifies which Plex API endpoint a scrape error came from. Deliberately
/// a small, fixed set of values so the metric stays low cardinality.
pub const ENDPOINT_LABELS: &[&str] = &["endpoint"];

pub const LIBRARY_LABELS: &[&str] = &[
    "server_type",
    "server",
    "server_id",
    "library_type",
    "library",
    "library_id",
];

/// `PLAY_LABELS` plus a `state` label, used for metrics that describe the
/// current playback state of a session rather than an accumulated total.
pub fn active_session_labels() -> Vec<&'static str> {
    let mut labels = PLAY_LABELS.to_vec();
    labels.push("state");
    labels
}

pub const PLAY_LABELS: &[&str] = &[
    "server_type",
    "server",
    "server_id",
    "library_type",
    "library",
    "library_id",
    "media_type",
    "title",
    "child_title",
    "grandchild_title",
    "stream_type",
    "stream_resolution",
    "stream_file_resolution",
    "stream_bitrate",
    "device",
    "device_type",
    "user",
    "session",
];

/// Global, always-on metrics updated directly from server refresh polling.
/// These mirror the promauto-registered vecs in the Go exporter: they are
/// never reset, matching the upstream behavior of leaving stale series in
/// place once set.
///
/// `up`, `scrape_errors_total` and `last_refresh_timestamp` have no upstream
/// equivalent. They describe the exporter's own view of the Plex API, so that a
/// server which has stopped answering is distinguishable from an idle one
/// instead of silently freezing every other metric at its last value.
pub struct GlobalMetrics {
    pub up: Gauge,
    pub scrape_errors_total: CounterVec,
    pub last_refresh_timestamp: Gauge,
    pub server_info: GaugeVec,
    pub host_cpu_util: GaugeVec,
    pub host_mem_util: GaugeVec,
    pub transmit_bytes_total: CounterVec,
    pub websocket_connected: GaugeVec,
    pub websocket_reconnects_total: CounterVec,
}

impl GlobalMetrics {
    pub fn new() -> prometheus::Result<Self> {
        let mut server_info_labels: Vec<&str> = SERVER_LABELS.to_vec();
        server_info_labels.extend(["version", "platform", "platform_version"]);

        Ok(Self {
            up: Gauge::new(
                "plex_up",
                "Whether the most recent refresh of server-level state from the Plex API succeeded",
            )?,
            scrape_errors_total: CounterVec::new(
                Opts::new(
                    "plex_scrape_errors_total",
                    "Total number of failed requests made by the exporter to the Plex API",
                ),
                ENDPOINT_LABELS,
            )?,
            last_refresh_timestamp: Gauge::new(
                "plex_last_refresh_timestamp_seconds",
                "Unix timestamp of the last successful refresh; 0 until one has succeeded",
            )?,
            server_info: GaugeVec::new(Opts::new("plex_server_info", "server_info"), &server_info_labels)?,
            host_cpu_util: GaugeVec::new(Opts::new("plex_host_cpu_util", "host_cpu_util"), SERVER_LABELS)?,
            host_mem_util: GaugeVec::new(Opts::new("plex_host_mem_util", "host_mem_util"), SERVER_LABELS)?,
            transmit_bytes_total: CounterVec::new(
                Opts::new("plex_transmit_bytes_total", "transmit_bytes_total"),
                SERVER_LABELS,
            )?,
            websocket_connected: GaugeVec::new(
                Opts::new(
                    "plex_websocket_connected",
                    "Whether the Plex notification websocket is currently connected",
                ),
                SERVER_LABELS,
            )?,
            websocket_reconnects_total: CounterVec::new(
                Opts::new(
                    "plex_websocket_reconnects_total",
                    "Total number of times the Plex notification websocket had to be (re)connected",
                ),
                SERVER_LABELS,
            )?,
        })
    }

    pub fn register(&self, registry: &prometheus::Registry) -> prometheus::Result<()> {
        registry.register(Box::new(self.up.clone()))?;
        registry.register(Box::new(self.scrape_errors_total.clone()))?;
        registry.register(Box::new(self.last_refresh_timestamp.clone()))?;
        registry.register(Box::new(self.server_info.clone()))?;
        registry.register(Box::new(self.host_cpu_util.clone()))?;
        registry.register(Box::new(self.host_mem_util.clone()))?;
        registry.register(Box::new(self.transmit_bytes_total.clone()))?;
        registry.register(Box::new(self.websocket_connected.clone()))?;
        registry.register(Box::new(self.websocket_reconnects_total.clone()))?;
        Ok(())
    }
}

/// Global, always-on metrics for the Jellyfin backend. A smaller field set than
/// `GlobalMetrics`: Jellyfin's API has no equivalent of Plex's host CPU/memory
/// stats, real bandwidth-history stats, or transcode speed/throttled state, so
/// those are simply not tracked here rather than approximated.
pub struct JellyfinGlobalMetrics {
    pub up: Gauge,
    pub scrape_errors_total: CounterVec,
    pub last_refresh_timestamp: Gauge,
    pub server_info: GaugeVec,
}

impl JellyfinGlobalMetrics {
    pub fn new() -> prometheus::Result<Self> {
        let mut server_info_labels: Vec<&str> = SERVER_LABELS.to_vec();
        server_info_labels.extend(["version", "platform", "platform_version"]);

        Ok(Self {
            up: Gauge::new(
                "jellyfin_up",
                "Whether the most recent refresh of server-level state from the Jellyfin API succeeded",
            )?,
            scrape_errors_total: CounterVec::new(
                Opts::new(
                    "jellyfin_scrape_errors_total",
                    "Total number of failed requests made by the exporter to the Jellyfin API",
                ),
                ENDPOINT_LABELS,
            )?,
            last_refresh_timestamp: Gauge::new(
                "jellyfin_last_refresh_timestamp_seconds",
                "Unix timestamp of the last successful refresh; 0 until one has succeeded",
            )?,
            server_info: GaugeVec::new(Opts::new("jellyfin_server_info", "server_info"), &server_info_labels)?,
        })
    }

    pub fn register(&self, registry: &prometheus::Registry) -> prometheus::Result<()> {
        registry.register(Box::new(self.up.clone()))?;
        registry.register(Box::new(self.scrape_errors_total.clone()))?;
        registry.register(Box::new(self.last_refresh_timestamp.clone()))?;
        registry.register(Box::new(self.server_info.clone()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_global_metric_registers_and_is_exposed() {
        let registry = prometheus::Registry::new();
        let metrics = GlobalMetrics::new().expect("failed to build metrics");
        metrics.register(&registry).expect("failed to register metrics");

        // Vec metrics only appear in `gather()` once they have a child series.
        metrics.scrape_errors_total.with_label_values(&["providers"]).inc();

        let names: Vec<String> = registry.gather().iter().map(|mf| mf.name().to_string()).collect();
        for expected in [
            "plex_up",
            "plex_scrape_errors_total",
            "plex_last_refresh_timestamp_seconds",
        ] {
            assert!(names.contains(&expected.to_string()), "{expected} was not exposed");
        }
    }

    #[test]
    fn up_reports_down_until_a_refresh_has_succeeded() {
        let metrics = GlobalMetrics::new().expect("failed to build metrics");
        assert_eq!(metrics.up.get(), 0.0);
        assert_eq!(metrics.last_refresh_timestamp.get(), 0.0);
    }

    #[test]
    fn every_jellyfin_global_metric_registers_and_is_exposed() {
        let registry = prometheus::Registry::new();
        let metrics = JellyfinGlobalMetrics::new().expect("failed to build metrics");
        metrics.register(&registry).expect("failed to register metrics");

        metrics.scrape_errors_total.with_label_values(&["sessions"]).inc();

        let names: Vec<String> = registry.gather().iter().map(|mf| mf.name().to_string()).collect();
        for expected in [
            "jellyfin_up",
            "jellyfin_scrape_errors_total",
            "jellyfin_last_refresh_timestamp_seconds",
        ] {
            assert!(names.contains(&expected.to_string()), "{expected} was not exposed");
        }
    }
}
