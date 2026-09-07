use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use prometheus::core::{Collector, Desc};
use prometheus::proto::MetricFamily;
use prometheus::{GaugeVec, Opts};

use crate::metrics::{GlobalMetrics, LIBRARY_LABELS};
use crate::plex::client::{Client, ClientError};
use crate::plex::library::{Library, is_library_directory_type};
use crate::plex::models::{
    BandwidthResponse, LibraryItemsResponse, ProvidersResponse, ResourcesResponse, RootResponse,
};

pub struct ServerState {
    pub client: Client,
    metrics: Arc<GlobalMetrics>,

    id: RwLock<String>,
    name: RwLock<String>,

    libraries: RwLock<Vec<Library>>,
    last_bandwidth_at: Mutex<i64>,
}

impl ServerState {
    pub async fn connect(server_url: &str, token: &str, metrics: Arc<GlobalMetrics>) -> Result<Arc<Self>, ClientError> {
        let client = Client::new(server_url, token)?;

        let state = Arc::new(Self {
            client,
            metrics,
            id: RwLock::new(String::new()),
            name: RwLock::new(String::new()),
            libraries: RwLock::new(Vec::new()),
            last_bandwidth_at: Mutex::new(unix_now()),
        });

        state.refresh().await?;

        let refresh_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(5));
            ticker.tick().await; // skip the immediate first tick, we just refreshed above
            loop {
                ticker.tick().await;
                if let Err(e) = refresh_state.refresh().await {
                    tracing::error!(error = %e, "failed to refresh server state");
                }
            }
        });

        Ok(state)
    }

    pub fn metrics(&self) -> &Arc<GlobalMetrics> {
        &self.metrics
    }

    pub fn id(&self) -> String {
        self.id.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn name(&self) -> String {
        self.name.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn library(&self, id: &str) -> Option<Library> {
        self.libraries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|l| l.id == id)
            .cloned()
    }

    pub fn libraries(&self) -> Vec<Library> {
        self.libraries.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn refresh(&self) -> Result<(), ClientError> {
        let container: ProvidersResponse = self.client.get("/media/providers?includeStorage=1").await?;

        let mut libraries = Vec::new();
        for provider in &container.media_container.media_providers {
            if provider.identifier != "com.plexapp.plugins.library" {
                continue;
            }
            for feature in &provider.features {
                if feature.feature_type != "content" {
                    continue;
                }
                for dir in &feature.directories {
                    if !is_library_directory_type(&dir.directory_type) {
                        continue;
                    }
                    libraries.push(Library {
                        id: dir.identifier.clone(),
                        name: dir.title.clone(),
                        library_type: dir.directory_type.clone(),
                        duration_total: dir.duration_total,
                        storage_total: dir.storage_total,
                        item_count: 0,
                    });
                }
            }
        }

        for library in &mut libraries {
            match self.library_item_count(&library.id).await {
                Ok(count) => library.item_count = count,
                Err(e) => {
                    tracing::warn!(error = %e, library = %library.name, "failed to fetch library item count")
                }
            }
        }

        *self.id.write().unwrap_or_else(|e| e.into_inner()) = container.media_container.machine_identifier;
        *self.name.write().unwrap_or_else(|e| e.into_inner()) = container.media_container.friendly_name;
        *self.libraries.write().unwrap_or_else(|e| e.into_inner()) = libraries;

        self.refresh_server_info().await?;
        self.refresh_resources().await?;
        self.refresh_bandwidth().await?;

        Ok(())
    }

    async fn library_item_count(&self, library_id: &str) -> Result<i64, ClientError> {
        let resp: LibraryItemsResponse = self
            .client
            .get(&format!(
                "/library/sections/{library_id}/all?X-Plex-Container-Start=0&X-Plex-Container-Size=0"
            ))
            .await?;
        let container = resp.media_container;
        Ok(if container.total_size > 0 {
            container.total_size
        } else {
            container.size
        })
    }

    async fn refresh_server_info(&self) -> Result<(), ClientError> {
        let resp: RootResponse = self.client.get("/").await?;

        self.metrics
            .server_info
            .with_label_values(&[
                "plex",
                &self.name(),
                &self.id(),
                &resp.media_container.version,
                &resp.media_container.platform,
                &resp.media_container.platform_version,
            ])
            .set(1.0);

        Ok(())
    }

    async fn refresh_resources(&self) -> Result<(), ClientError> {
        // This is a paid feature (Plex Pass) and may not be available.
        let resp: ResourcesResponse = match self.client.get("/statistics/resources?timespan=6").await {
            Ok(r) => r,
            Err(ClientError::NotFound) => return Ok(()),
            Err(e) => return Err(e),
        };

        if let Some(stats) = resp.media_container.statistics_resources.last() {
            let name = self.name();
            let id = self.id();
            self.metrics
                .host_cpu_util
                .with_label_values(&["plex", &name, &id])
                .set(stats.host_cpu_util);
            self.metrics
                .host_mem_util
                .with_label_values(&["plex", &name, &id])
                .set(stats.host_mem_util);
        }

        Ok(())
    }

    async fn refresh_bandwidth(&self) -> Result<(), ClientError> {
        // This is a paid feature (Plex Pass) and may not be available.
        let resp: BandwidthResponse = match self.client.get("/statistics/bandwidth?timespan=6").await {
            Ok(r) => r,
            Err(ClientError::NotFound) => return Ok(()),
            Err(e) => return Err(e),
        };

        let mut updates = resp.media_container.statistics_bandwidth;
        updates.sort_by_key(|u| u.at);

        let mut last_bandwidth_at = self.last_bandwidth_at.lock().unwrap_or_else(|e| e.into_inner());
        let (highest, total_bytes) = accumulate_bandwidth(*last_bandwidth_at, &updates);

        if total_bytes > 0 {
            let name = self.name();
            let id = self.id();
            self.metrics
                .transmit_bytes_total
                .with_label_values(&["plex", &name, &id])
                .inc_by(total_bytes as f64);
        }
        *last_bandwidth_at = highest;

        Ok(())
    }
}

/// Sums the bytes of bandwidth updates newer than `last_bandwidth_at`, and
/// returns the new high-water mark alongside that sum. `updates` must be
/// sorted by `at` ascending.
fn accumulate_bandwidth(last_bandwidth_at: i64, updates: &[crate::plex::models::StatisticsBandwidth]) -> (i64, i64) {
    let mut highest = last_bandwidth_at;
    let mut total_bytes: i64 = 0;

    for u in updates {
        if u.at > last_bandwidth_at {
            total_bytes += u.bytes;
            if u.at > highest {
                highest = u.at;
            }
        }
    }

    (highest, total_bytes)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Prometheus collector for library-level gauges. Recomputed from the
/// current library snapshot on every scrape, mirroring the upstream Go
/// exporter's use of `prometheus.MustNewConstMetric` in `Server.Collect`.
pub struct ServerCollector {
    server: Arc<ServerState>,
    library_duration_total: GaugeVec,
    library_storage_total: GaugeVec,
    library_items_total: GaugeVec,
}

impl ServerCollector {
    pub fn new(server: Arc<ServerState>) -> prometheus::Result<Self> {
        Ok(Self {
            server,
            library_duration_total: GaugeVec::new(
                Opts::new("plex_library_duration_total", "Total duration of a library in ms"),
                LIBRARY_LABELS,
            )?,
            library_storage_total: GaugeVec::new(
                Opts::new("plex_library_storage_total", "Total storage size of a library in Bytes"),
                LIBRARY_LABELS,
            )?,
            library_items_total: GaugeVec::new(
                Opts::new("plex_library_items_total", "Total number of items in a library"),
                LIBRARY_LABELS,
            )?,
        })
    }
}

impl Collector for ServerCollector {
    fn desc(&self) -> Vec<&Desc> {
        let mut descs = self.library_duration_total.desc();
        descs.extend(self.library_storage_total.desc());
        descs.extend(self.library_items_total.desc());
        descs
    }

    fn collect(&self) -> Vec<MetricFamily> {
        self.library_duration_total.reset();
        self.library_storage_total.reset();
        self.library_items_total.reset();

        let server_name = self.server.name();
        let server_id = self.server.id();

        for library in self.server.libraries() {
            let label_values = [
                "plex",
                server_name.as_str(),
                server_id.as_str(),
                library.library_type.as_str(),
                library.name.as_str(),
                library.id.as_str(),
            ];
            self.library_duration_total
                .with_label_values(&label_values)
                .set(library.duration_total as f64);
            self.library_storage_total
                .with_label_values(&label_values)
                .set(library.storage_total as f64);
            self.library_items_total
                .with_label_values(&label_values)
                .set(library.item_count as f64);
        }

        let mut mfs = self.library_duration_total.collect();
        mfs.extend(self.library_storage_total.collect());
        mfs.extend(self.library_items_total.collect());
        mfs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plex::models::StatisticsBandwidth;

    fn update(at: i64, bytes: i64) -> StatisticsBandwidth {
        StatisticsBandwidth { at, bytes }
    }

    #[test]
    fn accumulates_only_updates_newer_than_watermark() {
        let updates = vec![update(10, 100), update(20, 200), update(30, 300)];
        let (highest, total) = accumulate_bandwidth(20, &updates);
        assert_eq!(highest, 30);
        assert_eq!(total, 300);
    }

    #[test]
    fn no_new_updates_leaves_watermark_and_total_unchanged() {
        let updates = vec![update(5, 100), update(10, 200)];
        let (highest, total) = accumulate_bandwidth(10, &updates);
        assert_eq!(highest, 10);
        assert_eq!(total, 0);
    }

    #[test]
    fn empty_updates_are_a_no_op() {
        let (highest, total) = accumulate_bandwidth(42, &[]);
        assert_eq!(highest, 42);
        assert_eq!(total, 0);
    }
}
