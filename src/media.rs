use std::collections::HashMap;

use prometheus::core::{Collector, Desc};
use prometheus::proto::{LabelPair, Metric, MetricFamily};

const SERVER_TYPE_LABEL: &str = "server_type";

/// Backend metric name prefixes, paired with the `server_type` label value
/// their series carry.
const BACKEND_PREFIXES: &[(&str, &str)] = &[("plex_", "plex"), ("jellyfin_", "jellyfin")];

/// Metrics that every backend emits with the same meaning and labels, keyed by
/// their name without the backend prefix. Only these get a `media_` merged
/// counterpart; backend-specific metrics like `plex_transcode_speed` have
/// nothing to merge with.
const MERGED_METRICS: &[(&str, &str)] = &[
    (
        "up",
        "Whether the most recent refresh of server-level state from the media server's API succeeded",
    ),
    (
        "scrape_errors_total",
        "Total number of failed requests made by the exporter to the media server's API",
    ),
    (
        "last_refresh_timestamp_seconds",
        "Unix timestamp of the last successful refresh; 0 until one has succeeded",
    ),
    ("server_info", "server_info"),
    ("library_duration_total", "Total duration of a library in ms"),
    ("library_items_total", "Total number of items in a library"),
    ("plays_total", "Total play counts"),
    ("play_seconds_total", "Total play time per session"),
    ("estimated_transmit_bytes_total", "Total estimated bytes transmitted"),
    ("active_sessions", "Currently active playback sessions"),
];

/// Maps a backend metric name to its merged `media_` name, help text and the
/// `server_type` of the backend it came from, if it's one that gets merged.
fn merged_metric(fq_name: &str) -> Option<(String, &'static str, &'static str)> {
    BACKEND_PREFIXES.iter().find_map(|(prefix, server_type)| {
        let suffix = fq_name.strip_prefix(prefix)?;
        let (_, help) = MERGED_METRICS.iter().find(|(name, _)| *name == suffix)?;
        Some((format!("media_{suffix}"), *help, *server_type))
    })
}

/// Wraps every backend's collectors and, alongside their own metrics, emits a
/// `media_`-prefixed family for each metric in `MERGED_METRICS` that combines
/// the series of all backends. Series are told apart by `server_type`, which
/// is added to the handful of metrics (`up`, `scrape_errors_total`,
/// `last_refresh_timestamp_seconds`) that don't already carry it.
///
/// Only used when more than one backend is configured; with a single backend
/// the `media_` families would just duplicate it.
pub struct MediaCollector {
    sources: Vec<Box<dyn Collector>>,
    descs: Vec<Desc>,
}

impl MediaCollector {
    pub fn new(sources: Vec<Box<dyn Collector>>) -> prometheus::Result<Self> {
        let mut descs: Vec<Desc> = Vec::new();

        for desc in sources.iter().flat_map(|c| c.desc()) {
            let Some((name, help, _)) = merged_metric(&desc.fq_name) else {
                continue;
            };

            let mut labels = desc.variable_labels.clone();
            if !labels.iter().any(|l| l == SERVER_TYPE_LABEL) {
                labels.insert(0, SERVER_TYPE_LABEL.to_string());
            }

            match descs.iter().find(|d| d.fq_name == name) {
                Some(existing) if existing.variable_labels != labels => {
                    return Err(prometheus::Error::Msg(format!(
                        "cannot merge {} into {name}: labels {labels:?} differ from {:?}",
                        desc.fq_name, existing.variable_labels
                    )));
                }
                Some(_) => {}
                None => descs.push(Desc::new(name, help.to_string(), labels, HashMap::new())?),
            }
        }

        Ok(Self { sources, descs })
    }
}

fn ensure_server_type(metric: &mut Metric, server_type: &str) {
    if metric.get_label().iter().any(|l| l.name() == SERVER_TYPE_LABEL) {
        return;
    }
    let mut label = LabelPair::default();
    label.set_name(SERVER_TYPE_LABEL.to_string());
    label.set_value(server_type.to_string());

    let mut labels = metric.take_label();
    labels.insert(0, label);
    metric.set_label(labels);
}

impl Collector for MediaCollector {
    fn desc(&self) -> Vec<&Desc> {
        let mut descs: Vec<&Desc> = self.sources.iter().flat_map(|c| c.desc()).collect();
        descs.extend(self.descs.iter());
        descs
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let mut mfs = Vec::new();
        let mut merged: Vec<MetricFamily> = Vec::new();

        for source in &self.sources {
            for mf in source.collect() {
                if let Some((name, help, server_type)) = merged_metric(mf.name()) {
                    let mut metrics = mf.get_metric().to_vec();
                    for metric in &mut metrics {
                        ensure_server_type(metric, server_type);
                    }

                    match merged.iter_mut().find(|m| m.name() == name) {
                        Some(family) => family.mut_metric().extend(metrics),
                        None => {
                            let mut family = MetricFamily::default();
                            family.set_name(name);
                            family.set_help(help.to_string());
                            family.set_field_type(mf.get_field_type());
                            family.set_metric(metrics);
                            merged.push(family);
                        }
                    }
                }
                mfs.push(mf);
            }
        }

        mfs.extend(merged);
        mfs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::{CounterVec, Gauge, GaugeVec, Opts, Registry};

    fn label<'a>(metric: &'a Metric, name: &str) -> &'a str {
        metric
            .get_label()
            .iter()
            .find(|l| l.name() == name)
            .map(|l| l.value())
            .unwrap_or("")
    }

    fn family<'a>(mfs: &'a [MetricFamily], name: &str) -> Option<&'a MetricFamily> {
        mfs.iter().find(|mf| mf.name() == name)
    }

    #[test]
    fn merges_shared_metrics_across_backends() {
        let plex_up = Gauge::new("plex_up", "up").unwrap();
        let jellyfin_up = Gauge::new("jellyfin_up", "up").unwrap();
        let plex_plays = CounterVec::new(Opts::new("plex_plays_total", "plays"), &["server_type", "user"]).unwrap();
        let jellyfin_plays =
            CounterVec::new(Opts::new("jellyfin_plays_total", "plays"), &["server_type", "user"]).unwrap();
        let plex_cpu = GaugeVec::new(Opts::new("plex_host_cpu_util", "cpu"), &["server_type"]).unwrap();

        plex_up.set(1.0);
        plex_plays.with_label_values(&["plex", "alice"]).inc_by(2.0);
        jellyfin_plays.with_label_values(&["jellyfin", "bob"]).inc_by(3.0);
        plex_cpu.with_label_values(&["plex"]).set(0.5);

        let collector = MediaCollector::new(vec![
            Box::new(plex_up),
            Box::new(plex_plays),
            Box::new(plex_cpu),
            Box::new(jellyfin_up),
            Box::new(jellyfin_plays),
        ])
        .unwrap();

        let registry = Registry::new();
        registry.register(Box::new(collector)).unwrap();
        let mfs = registry.gather();

        for original in ["plex_up", "jellyfin_up", "plex_plays_total", "jellyfin_plays_total"] {
            assert!(family(&mfs, original).is_some(), "{original} was not kept");
        }

        let plays = family(&mfs, "media_plays_total").expect("media_plays_total missing");
        let mut by_type: Vec<(&str, &str, f64)> = plays
            .get_metric()
            .iter()
            .map(|m| (label(m, "server_type"), label(m, "user"), m.get_counter().value()))
            .collect();
        by_type.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(by_type, vec![("jellyfin", "bob", 3.0), ("plex", "alice", 2.0)]);

        let up = family(&mfs, "media_up").expect("media_up missing");
        let mut up_types: Vec<&str> = up.get_metric().iter().map(|m| label(m, "server_type")).collect();
        up_types.sort();
        assert_eq!(up_types, vec!["jellyfin", "plex"]);

        assert!(family(&mfs, "media_host_cpu_util").is_none());
    }

    #[test]
    fn rejects_backends_with_mismatched_labels() {
        let plex = CounterVec::new(Opts::new("plex_plays_total", "plays"), &["server_type", "user"]).unwrap();
        let jellyfin = CounterVec::new(Opts::new("jellyfin_plays_total", "plays"), &["server_type"]).unwrap();

        assert!(MediaCollector::new(vec![Box::new(plex), Box::new(jellyfin)]).is_err());
    }
}
