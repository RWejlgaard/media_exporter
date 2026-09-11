use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SystemInfo {
    #[serde(default, rename = "ServerName")]
    pub server_name: String,
    #[serde(default, rename = "Id")]
    pub id: String,
    #[serde(default, rename = "Version")]
    pub version: String,
    #[serde(default, rename = "OperatingSystem")]
    pub operating_system: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VirtualFolder {
    #[serde(default, rename = "Name")]
    pub name: String,
    #[serde(default, rename = "ItemId")]
    pub item_id: String,
    #[serde(default, rename = "CollectionType")]
    pub collection_type: String,
    #[serde(default, rename = "Locations")]
    pub locations: Vec<String>,
}

/// Response shape shared by both a cheap `Limit=0` item-count query and the
/// paginated `RunTimeTicks`-only duration scan.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemsResponse {
    #[serde(default, rename = "Items")]
    pub items: Vec<Item>,
    #[serde(default, rename = "TotalRecordCount")]
    pub total_record_count: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Item {
    #[serde(default, rename = "RunTimeTicks")]
    pub run_time_ticks: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaStream {
    #[serde(default, rename = "Type")]
    pub stream_type: String,
    #[serde(default, rename = "Index")]
    pub index: i64,
    #[serde(default, rename = "BitRate")]
    pub bit_rate: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct NowPlayingItem {
    #[serde(default, rename = "Id")]
    pub id: String,
    #[serde(default, rename = "Name")]
    pub name: String,
    #[serde(default, rename = "Type")]
    pub item_type: String,
    #[serde(default, rename = "SeriesName")]
    pub series_name: String,
    #[serde(default, rename = "SeasonName")]
    pub season_name: String,
    #[serde(default, rename = "Path")]
    pub path: String,
    #[serde(default, rename = "Height")]
    pub height: i64,
    #[serde(default, rename = "MediaStreams")]
    pub media_streams: Vec<MediaStream>,
}

impl NowPlayingItem {
    /// Mirrors Plex's `Metadata::play_labels()`: for episodes, title/season/episode
    /// come from series/season/self; for everything else only the top-level name is
    /// used.
    pub fn play_labels(&self) -> (&str, &str, &str) {
        if self.item_type == "Episode" {
            (&self.series_name, &self.season_name, &self.name)
        } else {
            (&self.name, "", "")
        }
    }

    /// Sum of the video stream's bitrate plus the currently selected audio stream's
    /// bitrate, used as a fallback when `TranscodingInfo` isn't present (direct play
    /// has no transcode-side bitrate figure to report).
    pub fn direct_play_bitrate(&self, audio_stream_index: Option<i64>) -> i64 {
        let video: i64 = self
            .media_streams
            .iter()
            .find(|s| s.stream_type == "Video")
            .map(|s| s.bit_rate)
            .unwrap_or(0);

        let audio: i64 = match audio_stream_index {
            Some(idx) => self
                .media_streams
                .iter()
                .find(|s| s.stream_type == "Audio" && s.index == idx)
                .map(|s| s.bit_rate)
                .unwrap_or(0),
            None => self
                .media_streams
                .iter()
                .find(|s| s.stream_type == "Audio")
                .map(|s| s.bit_rate)
                .unwrap_or(0),
        };

        video + audio
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TranscodingInfo {
    #[serde(default, rename = "Bitrate")]
    pub bitrate: i64,
    #[serde(default, rename = "Height")]
    pub height: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlayState {
    #[serde(default, rename = "IsPaused")]
    pub is_paused: bool,
    #[serde(default, rename = "PlayMethod")]
    pub play_method: String,
    #[serde(default, rename = "AudioStreamIndex")]
    pub audio_stream_index: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionInfo {
    #[serde(default, rename = "Id")]
    pub id: String,
    #[serde(default, rename = "UserName")]
    pub user_name: String,
    #[serde(default, rename = "Client")]
    pub client: String,
    #[serde(default, rename = "DeviceName")]
    pub device_name: String,
    #[serde(default, rename = "PlayState")]
    pub play_state: PlayState,
    #[serde(default, rename = "NowPlayingItem")]
    pub now_playing_item: Option<NowPlayingItem>,
    #[serde(default, rename = "TranscodingInfo")]
    pub transcoding_info: Option<TranscodingInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_labels_for_episode_uses_series_and_season_name() {
        let item = NowPlayingItem {
            item_type: "Episode".to_string(),
            name: "Episode Title".to_string(),
            season_name: "Season 1".to_string(),
            series_name: "Show Title".to_string(),
            ..Default::default()
        };

        assert_eq!(item.play_labels(), ("Show Title", "Season 1", "Episode Title"));
    }

    #[test]
    fn play_labels_for_movie_uses_only_name() {
        let item = NowPlayingItem {
            item_type: "Movie".to_string(),
            name: "Movie Title".to_string(),
            series_name: "should be ignored".to_string(),
            season_name: "should be ignored".to_string(),
            ..Default::default()
        };

        assert_eq!(item.play_labels(), ("Movie Title", "", ""));
    }

    #[test]
    fn direct_play_bitrate_sums_video_and_selected_audio_stream() {
        let item = NowPlayingItem {
            media_streams: vec![
                MediaStream {
                    stream_type: "Video".to_string(),
                    index: 0,
                    bit_rate: 3_000_000,
                },
                MediaStream {
                    stream_type: "Audio".to_string(),
                    index: 1,
                    bit_rate: 128_000,
                },
                MediaStream {
                    stream_type: "Audio".to_string(),
                    index: 2,
                    bit_rate: 192_000,
                },
            ],
            ..Default::default()
        };

        assert_eq!(item.direct_play_bitrate(Some(2)), 3_000_000 + 192_000);
    }

    #[test]
    fn direct_play_bitrate_falls_back_to_first_audio_stream_when_index_unknown() {
        let item = NowPlayingItem {
            media_streams: vec![
                MediaStream {
                    stream_type: "Video".to_string(),
                    index: 0,
                    bit_rate: 3_000_000,
                },
                MediaStream {
                    stream_type: "Audio".to_string(),
                    index: 1,
                    bit_rate: 128_000,
                },
            ],
            ..Default::default()
        };

        assert_eq!(item.direct_play_bitrate(None), 3_000_000 + 128_000);
    }
}
