//! The on-disk torrent cache and the naming scheme for saved `.torrent` files.
//!
//! Ported from the filename/caching half of `torrent.ts` plus the cache
//! helpers in `decide.ts`.

use std::path::{Path, PathBuf};

use crate::config::torrent_cache_dir;
use crate::constants::{MAX_PATH_BYTES, MediaType, SAVED_TORRENTS_INFO_REGEX};
use crate::utils::strip_extension;

use super::Metafile;

/// Metadata recoverable from a saved torrent's filename.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilenameMetadata {
    pub media_type: Option<String>,
    pub tracker: Option<String>,
    pub name: Option<String>,
    pub info_hash: Option<String>,
    pub cached: bool,
}

fn build_torrent_save_name(
    media_type: MediaType,
    tracker: &str,
    name: &str,
    info_hash: &str,
    ext: &str,
) -> String {
    format!(
        "[{}][{tracker}]{name}[{info_hash}]{ext}",
        media_type.as_str()
    )
}

/// Where a snatched torrent is written.
///
/// Most filesystems cap a path component at 255 **bytes**, and release names
/// are frequently non-ASCII, so the name is truncated by code point until the
/// encoded path fits — truncating by character count would still overflow.
pub fn get_torrent_save_path(
    meta: &Metafile,
    media_type: MediaType,
    tracker: &str,
    dir: &Path,
    cached: bool,
) -> PathBuf {
    let full_name = strip_extension(&meta.file_system_safe_name());
    let ext = if cached {
        ".cached.torrent"
    } else {
        ".torrent"
    };

    let full_path = dir.join(build_torrent_save_name(
        media_type,
        tracker,
        &full_name,
        &meta.info_hash,
        ext,
    ));
    if full_path.to_string_lossy().len() <= MAX_PATH_BYTES {
        return full_path;
    }

    let code_points: Vec<char> = full_name.chars().collect();
    let mut current_bytes = full_path.to_string_lossy().len() + "...".len();
    let mut to_remove = 0usize;
    for ch in code_points.iter().rev() {
        to_remove += 1;
        current_bytes -= ch.len_utf8();
        if current_bytes <= MAX_PATH_BYTES {
            break;
        }
    }
    let kept: String = code_points[..code_points.len().saturating_sub(to_remove)]
        .iter()
        .collect();
    dir.join(build_torrent_save_name(
        media_type,
        tracker,
        &format!("{kept}..."),
        &meta.info_hash,
        ext,
    ))
}

/// Inverse of [`get_torrent_save_path`]. Returns an empty result when the name
/// does not follow the scheme or names an unknown media type.
pub fn parse_metadata_from_filename(filename: &str) -> FilenameMetadata {
    let Ok(Some(caps)) = SAVED_TORRENTS_INFO_REGEX.captures(filename) else {
        return FilenameMetadata::default();
    };
    let media_type = caps.name("mediaType").map(|m| m.as_str().to_string());
    let known = [
        MediaType::Episode,
        MediaType::Season,
        MediaType::Movie,
        MediaType::Anime,
        MediaType::Video,
        MediaType::Audio,
        MediaType::Book,
        MediaType::Other,
    ]
    .iter()
    .any(|mt| Some(mt.as_str()) == media_type.as_deref());
    if !known {
        return FilenameMetadata::default();
    }

    FilenameMetadata {
        media_type,
        tracker: caps.name("tracker").map(|m| m.as_str().to_string()),
        name: caps.name("name").map(|m| m.as_str().to_string()),
        info_hash: caps.name("infoHash").map(|m| m.as_str().to_string()),
        cached: caps.name("cached").is_some(),
    }
}

/// Every `.torrent` directly inside `dir`, sorted, as absolute paths.
pub async fn find_all_torrent_files_in_dir(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut paths = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("torrent") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// The cache filename for an info hash — flat, so a lookup is a single `stat`.
pub fn cached_torrent_name(info_hash: &str) -> String {
    format!("{info_hash}.cached.torrent")
}

pub fn cached_torrent_path(info_hash: &str) -> PathBuf {
    torrent_cache_dir().join(cached_torrent_name(info_hash))
}

/// Reads a cached torrent, deleting it if it no longer parses.
///
/// A corrupt cache entry would otherwise make the same candidate fail forever;
/// the cleanup job removes the orphaned decision rows.
pub async fn get_cached_torrent(info_hash: &str) -> Option<Metafile> {
    let path = cached_torrent_path(info_hash);
    let bytes = tokio::fs::read(&path).await.ok()?;
    match Metafile::decode(&bytes) {
        Ok(meta) => Some(meta),
        Err(e) => {
            tracing::error!(
                "Failed to parse cached torrent for {} - deleting: {e}",
                crate::utils::sanitize_info_hash(info_hash)
            );
            let _ = tokio::fs::remove_file(&path).await;
            None
        }
    }
}

/// Writes a snatched torrent into the cache.
pub async fn write_cached_torrent(meta: &Metafile) -> std::io::Result<()> {
    let dir = torrent_cache_dir();
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(
        dir.join(cached_torrent_name(&meta.info_hash)),
        meta.encode(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::metafile::fixtures::single_file_torrent;

    fn meta(name: &str) -> Metafile {
        Metafile::decode(&single_file_torrent(name, 100)).unwrap()
    }

    #[test]
    fn save_path_encodes_media_type_tracker_name_and_hash() {
        let meta = meta("Some.Show.S01E01.1080p.mkv");
        let path = get_torrent_save_path(
            &meta,
            MediaType::Episode,
            "TrackerName",
            Path::new("/out"),
            false,
        );
        let filename = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(filename.starts_with("[episode][TrackerName]Some.Show.S01E01.1080p["));
        assert!(filename.ends_with(&format!("[{}].torrent", meta.info_hash)));
    }

    #[test]
    fn cached_save_paths_use_the_cached_extension() {
        let meta = meta("Movie.2020.mkv");
        let path = get_torrent_save_path(&meta, MediaType::Movie, "T", Path::new("/out"), true);
        assert!(path.to_string_lossy().ends_with(".cached.torrent"));
    }

    /// Long non-ASCII names must be truncated by *bytes*, not characters.
    #[test]
    fn overlong_names_are_truncated_to_fit_the_byte_limit() {
        let long_name = format!("{}.mkv", "é".repeat(300));
        let meta = meta(&long_name);
        let path = get_torrent_save_path(&meta, MediaType::Movie, "T", Path::new("/out"), false);
        let rendered = path.to_string_lossy();
        assert!(rendered.len() <= MAX_PATH_BYTES, "{} bytes", rendered.len());
        assert!(rendered.contains("..."));
        assert!(rendered.ends_with(".torrent"));
    }

    #[test]
    fn filenames_round_trip_through_the_parser() {
        let meta = meta("Some.Show.S01E01.mkv");
        let path = get_torrent_save_path(
            &meta,
            MediaType::Episode,
            "TrackerName",
            Path::new("/out"),
            false,
        );
        let parsed = parse_metadata_from_filename(&path.file_name().unwrap().to_string_lossy());
        assert_eq!(parsed.media_type.as_deref(), Some("episode"));
        assert_eq!(parsed.tracker.as_deref(), Some("TrackerName"));
        assert_eq!(parsed.name.as_deref(), Some("Some.Show.S01E01"));
        assert_eq!(parsed.info_hash.as_deref(), Some(meta.info_hash.as_str()));
        assert!(!parsed.cached);
    }

    #[test]
    fn cached_filenames_are_flagged() {
        let parsed = parse_metadata_from_filename(
            "[movie][T]Name[0123456789abcdef0123456789abcdef01234567].cached.torrent",
        );
        assert!(parsed.cached);
    }

    #[test]
    fn unknown_media_types_are_rejected() {
        let parsed = parse_metadata_from_filename(
            "[nonsense][T]Name[0123456789abcdef0123456789abcdef01234567].torrent",
        );
        assert_eq!(parsed, FilenameMetadata::default());
    }

    #[test]
    fn cache_names_are_derived_from_the_info_hash() {
        assert_eq!(cached_torrent_name("abc"), "abc.cached.torrent");
    }
}
