use serde::{Deserialize, Deserializer};
use std::fmt;

/// A value that Plex may encode as either a JSON number or a JSON string,
/// normalized to its string form (mirrors Go's `json.Number` handling of
/// `librarySectionID`, which is looked up against `Library::id` — itself a
/// plain string).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NumOrString(pub String);

impl fmt::Display for NumOrString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'de> Deserialize<'de> for NumOrString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let s = match value {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Null => String::new(),
            other => return Err(serde::de::Error::custom(format!("unexpected value: {other}"))),
        };
        Ok(NumOrString(s))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Part {
    #[serde(default)]
    pub decision: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Media {
    #[serde(default)]
    pub bitrate: i64,
    #[serde(default, rename = "videoResolution")]
    pub video_resolution: String,
    #[serde(default, rename = "Part")]
    pub part: Vec<Part>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Player {
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub product: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct User {
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TranscodeSession {
    #[serde(default)]
    pub throttled: bool,
    #[serde(default)]
    pub speed: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Metadata {
    #[serde(default, rename = "Player")]
    pub player: Player,
    #[serde(default, rename = "User")]
    pub user: User,
    #[serde(default, rename = "TranscodeSession")]
    pub transcode_session: Option<TranscodeSession>,
    #[serde(default, rename = "sessionKey")]
    pub session_key: String,
    #[serde(default, rename = "ratingKey")]
    pub rating_key: String,
    #[serde(default, rename = "title")]
    pub title: String,
    #[serde(default, rename = "parentTitle")]
    pub parent_title: String,
    #[serde(default, rename = "grandparentTitle")]
    pub grandparent_title: String,
    #[serde(default, rename = "type")]
    pub media_type: String,
    #[serde(default, rename = "librarySectionID")]
    pub library_section_id: NumOrString,
    #[serde(default, rename = "Media")]
    pub media: Vec<Media>,
}

impl Metadata {
    /// Mirrors Go's `labels()` helper: for episodes, title/season/episode
    /// come from grandparent/parent/self; for everything else only the
    /// top-level title is used.
    pub fn play_labels(&self) -> (&str, &str, &str) {
        if self.media_type == "episode" {
            (&self.grandparent_title, &self.parent_title, &self.title)
        } else {
            (&self.title, "", "")
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MetadataContainer {
    #[serde(default, rename = "Metadata")]
    pub metadata: Vec<Metadata>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CurrentSessions {
    #[serde(default, rename = "MediaContainer")]
    pub media_container: MetadataContainer,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaMetadataResponse {
    #[serde(default, rename = "MediaContainer")]
    pub media_container: MetadataContainer,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlaySessionStateNotification {
    #[serde(default, rename = "sessionKey")]
    pub session_key: String,
    #[serde(default, rename = "ratingKey")]
    pub rating_key: String,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NotificationContainer {
    #[serde(default, rename = "PlaySessionStateNotification")]
    pub play_session_state_notification: Vec<PlaySessionStateNotification>,
    #[serde(default, rename = "type")]
    pub notification_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WebsocketNotification {
    #[serde(default, rename = "NotificationContainer")]
    pub notification_container: NotificationContainer,
}

// --- Server refresh responses ---

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RootMediaContainer {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default, rename = "platformVersion")]
    pub platform_version: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RootResponse {
    #[serde(default, rename = "MediaContainer")]
    pub media_container: RootMediaContainer,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Directory {
    #[serde(default, rename = "id")]
    pub identifier: String,
    #[serde(default, rename = "durationTotal")]
    pub duration_total: i64,
    #[serde(default, rename = "storageTotal")]
    pub storage_total: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "type")]
    pub directory_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Feature {
    #[serde(default, rename = "type")]
    pub feature_type: String,
    #[serde(default, rename = "Directory")]
    pub directories: Vec<Directory>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaProvider {
    #[serde(default)]
    pub identifier: String,
    #[serde(default, rename = "Feature")]
    pub features: Vec<Feature>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProvidersMediaContainer {
    #[serde(default, rename = "friendlyName")]
    pub friendly_name: String,
    #[serde(default, rename = "machineIdentifier")]
    pub machine_identifier: String,
    #[serde(default, rename = "MediaProvider")]
    pub media_providers: Vec<MediaProvider>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProvidersResponse {
    #[serde(default, rename = "MediaContainer")]
    pub media_container: ProvidersMediaContainer,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LibraryItemsMediaContainer {
    #[serde(default)]
    pub size: i64,
    #[serde(default, rename = "totalSize")]
    pub total_size: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LibraryItemsResponse {
    #[serde(default, rename = "MediaContainer")]
    pub media_container: LibraryItemsMediaContainer,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatisticsResources {
    #[serde(default, rename = "hostCpuUtilization")]
    pub host_cpu_util: f64,
    #[serde(default, rename = "hostMemoryUtilization")]
    pub host_mem_util: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResourcesMediaContainer {
    #[serde(default, rename = "StatisticsResources")]
    pub statistics_resources: Vec<StatisticsResources>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResourcesResponse {
    #[serde(default, rename = "MediaContainer")]
    pub media_container: ResourcesMediaContainer,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatisticsBandwidth {
    #[serde(default)]
    pub at: i64,
    #[serde(default)]
    pub bytes: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BandwidthMediaContainer {
    #[serde(default, rename = "StatisticsBandwidth")]
    pub statistics_bandwidth: Vec<StatisticsBandwidth>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BandwidthResponse {
    #[serde(default, rename = "MediaContainer")]
    pub media_container: BandwidthMediaContainer,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_or_string_accepts_number() {
        let v: NumOrString = serde_json::from_str("42").unwrap();
        assert_eq!(v.0, "42");
    }

    #[test]
    fn num_or_string_accepts_string() {
        let v: NumOrString = serde_json::from_str("\"42\"").unwrap();
        assert_eq!(v.0, "42");
    }

    #[test]
    fn num_or_string_accepts_null() {
        let v: NumOrString = serde_json::from_str("null").unwrap();
        assert_eq!(v.0, "");
    }

    #[test]
    fn play_labels_for_episode_uses_grandparent_parent_title() {
        let metadata = Metadata {
            media_type: "episode".to_string(),
            title: "Episode Title".to_string(),
            parent_title: "Season 1".to_string(),
            grandparent_title: "Show Title".to_string(),
            ..Default::default()
        };

        assert_eq!(metadata.play_labels(), ("Show Title", "Season 1", "Episode Title"));
    }

    #[test]
    fn play_labels_for_movie_uses_only_title() {
        let metadata = Metadata {
            media_type: "movie".to_string(),
            title: "Movie Title".to_string(),
            parent_title: "should be ignored".to_string(),
            grandparent_title: "should be ignored".to_string(),
            ..Default::default()
        };

        assert_eq!(metadata.play_labels(), ("Movie Title", "", ""));
    }
}
