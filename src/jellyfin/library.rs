#[derive(Debug, Clone, Default)]
pub struct Library {
    pub id: String,
    pub name: String,
    pub library_type: String,
    pub locations: Vec<String>,

    pub duration_total: i64,
    pub item_count: i64,
}

impl Library {
    fn contains_path(&self, path: &str) -> bool {
        self.locations.iter().any(|loc| path.starts_with(loc.as_str()))
    }
}

pub fn is_library_collection_type(collection_type: &str) -> bool {
    matches!(collection_type, "movies" | "tvshows" | "music")
}

/// Jellyfin doesn't put a library/section id on a played item, and its
/// ID-based ancestor chain doesn't terminate at the library object listed by
/// `/Library/VirtualFolders` either. Matching the played item's path against
/// each library's configured locations is the reliable way to attribute it.
pub fn find_library_for_path<'a>(libraries: &'a [Library], path: &str) -> Option<&'a Library> {
    if path.is_empty() {
        return None;
    }
    libraries.iter().find(|l| l.contains_path(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(locations: &[&str]) -> Library {
        Library {
            locations: locations.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn recognizes_supported_collection_types() {
        assert!(is_library_collection_type("movies"));
        assert!(is_library_collection_type("tvshows"));
        assert!(is_library_collection_type("music"));
    }

    #[test]
    fn rejects_unsupported_collection_types() {
        assert!(!is_library_collection_type("boxsets"));
        assert!(!is_library_collection_type(""));
    }

    #[test]
    fn finds_library_by_path_prefix() {
        let libraries = vec![
            library(&["/hdd/movies"]),
            library(&["/hdd/tv"]),
            library(&["/hdd/music"]),
        ];
        let path = "/hdd/tv/Family Guy/S06E01.mkv";
        let found = find_library_for_path(&libraries, path).expect("expected a match");
        assert_eq!(found.locations, vec!["/hdd/tv".to_string()]);
    }

    #[test]
    fn no_match_for_unknown_path() {
        let libraries = vec![library(&["/hdd/movies"])];
        assert!(find_library_for_path(&libraries, "/hdd/other/file.mkv").is_none());
    }

    #[test]
    fn no_match_for_empty_path() {
        let libraries = vec![library(&["/hdd/movies"])];
        assert!(find_library_for_path(&libraries, "").is_none());
    }
}
