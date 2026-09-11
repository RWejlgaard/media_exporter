use std::sync::{Arc, RwLock};
use std::time::Duration;

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::{GaugeVec, Opts};
use serde::de::DeserializeOwned;

use crate::jellyfin::client::{Client, ClientError};
use crate::jellyfin::library::{Library, is_library_collection_type};
use crate::jellyfin::models::{ItemsResponse, SystemInfo, VirtualFolder};
use crate::metrics::{JellyfinGlobalMetrics, LIBRARY_LABELS};

const FAST_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Page size for the paginated `RunTimeTicks`-only duration scan. Small enough
/// to keep any single request's payload modest even on a library with tens of
/// thousands of items.
const DURATION_SCAN_PAGE_SIZE: i64 = 500;

/// 1 Jellyfin "tick" is 100ns; `library_duration_total` is reported in ms to
/// match Plex's `plex_library_duration_total`.
const TICKS_PER_MS: i64 = 10_000;

pub struct ServerState {
    pub client: Client,
    metrics: Arc<JellyfinGlobalMetrics>,

    id: RwLock<String>,
    name: RwLock<String>,
    libraries: RwLock<Vec<Library>>,
}

impl ServerState {
    pub async fn connect(
        server_url: &str,
        token: &str,
        library_stats_interval: Duration,
        metrics: Arc<JellyfinGlobalMetrics>,
    ) -> Result<Arc<Self>, ClientError> {
        let client = Client::new(server_url, token)?;

        let state = Arc::new(Self {
            client,
            metrics,
            id: RwLock::new(String::new()),
            name: RwLock::new(String::new()),
            libraries: RwLock::new(Vec::new()),
        });

        // A failed initial refresh is deliberately not fatal, mirroring the Plex
        // exporter: a Jellyfin server that's down when the exporter starts is
        // exactly the situation `jellyfin_up` exists to report.
        state.refresh_and_record().await;

        let fast = Arc::clone(&state);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(FAST_REFRESH_INTERVAL);
            ticker.tick().await; // skip the immediate first tick, we just refreshed above
            loop {
                ticker.tick().await;
                fast.refresh_and_record().await;
            }
        });

        // Library duration totals require a full paginated scan of every item,
        // which is far more expensive than the fast refresh above on a large
        // library. Run it on its own, much longer interval instead. Unlike the
        // fast loop this fires immediately in the background rather than
        // blocking startup, since a first scan can take a while.
        let slow = Arc::clone(&state);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(library_stats_interval);
            loop {
                ticker.tick().await;
                slow.refresh_library_durations().await;
            }
        });

        Ok(state)
    }

    /// Performs a GET against the Jellyfin API, counting a failure against
    /// `endpoint` in `jellyfin_scrape_errors_total`.
    pub async fn get<T: DeserializeOwned>(&self, endpoint: &str, path: &str) -> Result<T, ClientError> {
        let result = self.client.get(path).await;
        if let Err(e) = &result
            && !matches!(e, ClientError::NotFound)
        {
            self.metrics.scrape_errors_total.with_label_values(&[endpoint]).inc();
        }
        result
    }

    pub fn id(&self) -> String {
        self.id.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn name(&self) -> String {
        self.name.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn libraries(&self) -> Vec<Library> {
        self.libraries.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Refreshes server state, recording the outcome in `jellyfin_up` and
    /// `jellyfin_last_refresh_timestamp_seconds`.
    async fn refresh_and_record(&self) {
        match self.refresh().await {
            Ok(()) => {
                self.metrics.up.set(1.0);
                self.metrics.last_refresh_timestamp.set(unix_now() as f64);
            }
            Err(e) => {
                self.metrics.up.set(0.0);
                tracing::error!(error = %e, "failed to refresh jellyfin server state");
            }
        }
    }

    async fn refresh(&self) -> Result<(), ClientError> {
        let info: SystemInfo = self.get("system_info", "/System/Info").await?;

        *self.id.write().unwrap_or_else(|e| e.into_inner()) = info.id.clone();
        *self.name.write().unwrap_or_else(|e| e.into_inner()) = info.server_name.clone();

        self.metrics
            .server_info
            .with_label_values(&[
                "jellyfin",
                &info.server_name,
                &info.id,
                &info.version,
                &info.operating_system,
                "",
            ])
            .set(1.0);

        let virtual_folders: Vec<VirtualFolder> = self.get("libraries", "/Library/VirtualFolders").await?;
        let previous = self.libraries();

        let mut libraries = Vec::new();
        for vf in virtual_folders {
            if !is_library_collection_type(&vf.collection_type) {
                continue;
            }
            // duration_total is only ever updated by the separate slow scan;
            // carry the last known value forward so this fast refresh doesn't
            // clobber it back to zero.
            let duration_total = previous
                .iter()
                .find(|l| l.id == vf.item_id)
                .map(|l| l.duration_total)
                .unwrap_or(0);
            libraries.push(Library {
                id: vf.item_id,
                name: vf.name,
                library_type: vf.collection_type,
                locations: vf.locations,
                duration_total,
                item_count: 0,
            });
        }

        for library in &mut libraries {
            match self.library_item_count(&library.id).await {
                Ok(count) => library.item_count = count,
                Err(e) => {
                    tracing::warn!(error = %e, library = %library.name, "failed to fetch jellyfin library item count")
                }
            }
        }

        *self.libraries.write().unwrap_or_else(|e| e.into_inner()) = libraries;

        Ok(())
    }

    async fn library_item_count(&self, library_id: &str) -> Result<i64, ClientError> {
        let resp: ItemsResponse = self
            .get(
                "library_items",
                &format!("/Items?ParentId={library_id}&Recursive=true&Limit=0"),
            )
            .await?;
        Ok(resp.total_record_count)
    }

    async fn refresh_library_durations(&self) {
        for library in self.libraries() {
            match self.scan_library_duration_ms(&library.id).await {
                Ok(duration_total) => {
                    let mut libs = self.libraries.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(entry) = libs.iter_mut().find(|l| l.id == library.id) {
                        entry.duration_total = duration_total;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, library = %library.name, "failed to scan jellyfin library duration")
                }
            }
        }
    }

    async fn scan_library_duration_ms(&self, library_id: &str) -> Result<i64, ClientError> {
        let mut start_index: i64 = 0;
        let mut total_ticks: i64 = 0;

        loop {
            let resp: ItemsResponse = self
                .get(
                    "library_duration_scan",
                    &format!(
                        "/Items?ParentId={library_id}&Recursive=true&Fields=RunTimeTicks&EnableImages=false&StartIndex={start_index}&Limit={DURATION_SCAN_PAGE_SIZE}"
                    ),
                )
                .await?;

            if resp.items.is_empty() {
                break;
            }

            total_ticks += resp.items.iter().map(|i| i.run_time_ticks).sum::<i64>();
            start_index += resp.items.len() as i64;

            if start_index >= resp.total_record_count {
                break;
            }
        }

        Ok(ticks_to_ms(total_ticks))
    }
}

fn ticks_to_ms(ticks: i64) -> i64 {
    ticks / TICKS_PER_MS
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Prometheus collector for library-level gauges. Recomputed from the current
/// library snapshot on every scrape, mirroring `plex::server::ServerCollector`.
/// No `library_storage_total` equivalent: Jellyfin only exposes file size via
/// the much heavier `MediaSources` field, too expensive to scan alongside
/// duration.
pub struct ServerCollector {
    server: Arc<ServerState>,
    library_duration_total: GaugeVec,
    library_items_total: GaugeVec,
}

impl ServerCollector {
    pub fn new(server: Arc<ServerState>) -> prometheus::Result<Self> {
        Ok(Self {
            server,
            library_duration_total: GaugeVec::new(
                Opts::new("jellyfin_library_duration_total", "Total duration of a library in ms"),
                LIBRARY_LABELS,
            )?,
            library_items_total: GaugeVec::new(
                Opts::new("jellyfin_library_items_total", "Total number of items in a library"),
                LIBRARY_LABELS,
            )?,
        })
    }
}

impl Collector for ServerCollector {
    fn desc(&self) -> Vec<&Desc> {
        let mut descs = self.library_duration_total.desc();
        descs.extend(self.library_items_total.desc());
        descs
    }

    fn collect(&self) -> Vec<MetricFamily> {
        self.library_duration_total.reset();
        self.library_items_total.reset();

        let server_name = self.server.name();
        let server_id = self.server.id();

        for library in self.server.libraries() {
            let label_values = [
                "jellyfin",
                server_name.as_str(),
                server_id.as_str(),
                library.library_type.as_str(),
                library.name.as_str(),
                library.id.as_str(),
            ];
            self.library_duration_total
                .with_label_values(&label_values)
                .set(library.duration_total as f64);
            self.library_items_total
                .with_label_values(&label_values)
                .set(library.item_count as f64);
        }

        let mut mfs = self.library_duration_total.collect();
        mfs.extend(self.library_items_total.collect());
        mfs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_to_ms_converts_100ns_ticks() {
        // 1 second = 10_000_000 ticks = 1_000 ms.
        assert_eq!(ticks_to_ms(10_000_000), 1_000);
    }

    #[test]
    fn ticks_to_ms_of_zero_is_zero() {
        assert_eq!(ticks_to_ms(0), 0);
    }
}
