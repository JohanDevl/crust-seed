//! Torrent metainfo parsing.
//!
//! Ported from `parseTorrent.ts`.
//!
//! The info hash is computed over the **original bytes** of the `info`
//! dictionary rather than over a re-encode. The JS version re-encoded with the
//! `bencode` package and relied on it being canonical; a torrent that does not
//! round-trip byte-for-byte (unsorted keys, redundant integer encodings) would
//! produce a wrong hash, and a wrong hash silently breaks every downstream
//! decision. Taking the source span removes the question — see
//! [`crate::torrent::bencode::decode_root`].

use std::path::PathBuf;

use sha1::{Digest, Sha1};

use super::bencode::{self, Value};
use crate::searchee::{File, parse_title};

#[derive(Debug, thiserror::Error)]
pub enum MetafileError {
    #[error("Torrent is missing required field: {0}")]
    MissingField(&'static str),
    #[error("could not parse torrent: {0}")]
    Bencode(#[from] bencode::BencodeError),
}

#[derive(Debug, Clone)]
pub struct Metafile {
    pub info_hash: String,
    pub length: i64,
    pub name: String,
    /// Always derived from `name`; present so a `Metafile` can be logged with
    /// the same helper as a `Searchee`.
    pub title: String,
    pub piece_length: i64,
    pub files: Vec<File>,
    pub is_single_file_torrent: bool,
    /// Populated from a client's fastresume file, not from the torrent itself.
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub trackers: Vec<String>,
    pub raw: Value,
    /// The source bytes, kept so [`Metafile::encode`] can hand a client back
    /// exactly what was read from disk.
    raw_bytes: Option<Vec<u8>>,
}

/// Tracker metadata a torrent client stores *next to* the torrent, not inside
/// it: qBittorrent `.fastresume`, Transmission `.resume`, rTorrent `.rtorrent`.
/// (Deluge keeps labels in `label.conf` instead, handled by its client module.)
#[derive(Debug, Clone, Default)]
pub struct TorrentMetadata {
    pub trackers: Option<Vec<Vec<String>>>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
}

/// Reduces a tracker announce URL to its host, the form the blocklist and the
/// indexer-tracker mapping compare against.
///
/// Matches `new URL(url).host`: the port is included only when it is not the
/// scheme's default, so `https://x/announce` and `https://x:443/announce`
/// both reduce to `x`.
pub fn sanitize_tracker_url(url: &str) -> Option<String> {
    url::Url::parse(url).ok().and_then(|parsed| {
        parsed.host_str().map(|host| match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        })
    })
}

fn sanitize_tracker_urls(urls: &[String]) -> Vec<String> {
    urls.iter()
        .filter_map(|u| sanitize_tracker_url(u))
        .collect()
}

/// `localeCompare` stand-in for sorting file paths.
///
/// Node's `Array#sort((a, b) => a.path.localeCompare(b.path))` uses ICU
/// collation, under which `"a" < "B"`; a plain byte comparison would order
/// those the other way and change which file `parseTitle` inspects first.
/// Comparing case-folded, then breaking ties in *reverse* byte order,
/// reproduces ICU's ordering for the ASCII range release names live in: ICU
/// puts lowercase before uppercase at the tertiary level (`"apple" < "Apple"`),
/// which is the opposite of a raw byte comparison.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase()
        .cmp(&b.to_lowercase())
        .then_with(|| b.cmp(a))
}

impl Metafile {
    /// How this torrent is named in a log line. A candidate has no client host
    /// and no path, so this is always `title` plus the truncated info hash.
    /// See [`crate::utils::log_string`].
    pub fn log_string(&self) -> String {
        crate::utils::log_string(&self.title, &self.name, Some(&self.info_hash), None, None)
    }

    pub fn decode(buf: &[u8]) -> Result<Metafile, MetafileError> {
        let (raw, info_span) = bencode::decode_root(buf)?;
        let info = raw.get("info").ok_or(MetafileError::MissingField("info"))?;

        // Prefer hashing the untouched source bytes.
        let info_bytes = match info_span {
            Some(span) => buf[span].to_vec(),
            None => bencode::encode(info),
        };
        let info_hash = hex::encode(Sha1::digest(&info_bytes));

        let name_bytes = info
            .get("name.utf-8")
            .or_else(|| info.get("name"))
            .and_then(|v| v.as_str())
            .ok_or(MetafileError::MissingField("info.name"))?;
        let piece_length = info
            .get("piece length")
            .and_then(|v| v.as_int())
            .ok_or(MetafileError::MissingField("info['piece length']"))?;
        if info.get("pieces").and_then(|v| v.as_bytes()).is_none() {
            return Err(MetafileError::MissingField("info.pieces"));
        }

        let name = name_bytes;
        let (files, length, is_single_file_torrent) =
            match info.get("files").and_then(|v| v.as_list()) {
                None => {
                    let length = info
                        .get("length")
                        .and_then(|v| v.as_int())
                        .ok_or(MetafileError::MissingField("info.length"))?;
                    (
                        vec![File {
                            name: name.clone(),
                            path: name.clone(),
                            length,
                        }],
                        length,
                        true,
                    )
                }
                Some(entries) => {
                    let mut files = Vec::with_capacity(entries.len());
                    for entry in entries {
                        let file_length = entry
                            .get("length")
                            .and_then(|v| v.as_int())
                            .ok_or(MetafileError::MissingField("info.files[0].length"))?;
                        let segments = entry
                            .get("path.utf-8")
                            .or_else(|| entry.get("path"))
                            .and_then(|v| v.as_list())
                            .ok_or(MetafileError::MissingField("info.files[0].path"))?;

                        let segments: Vec<String> = segments
                            .iter()
                            .map(|segment| {
                                let text = segment.as_str().unwrap_or_default();
                                // Zero-length path segments are conventionally
                                // rendered as underscores.
                                if text.is_empty() {
                                    "_".to_string()
                                } else {
                                    text
                                }
                            })
                            .collect();

                        let mut path = PathBuf::from(&name);
                        for segment in &segments {
                            path.push(segment);
                        }
                        files.push(File {
                            name: segments.last().cloned().unwrap_or_default(),
                            length: file_length,
                            path: path.to_string_lossy().into_owned(),
                        });
                    }
                    files.sort_by(|a, b| locale_compare(&a.path, &b.path));
                    let total = files.iter().map(|f| f.length).sum();
                    (files, total, false)
                }
            };

        let trackers = match raw.get("announce-list").and_then(|v| v.as_list()) {
            Some(tiers) if !tiers.is_empty() => tiers
                .iter()
                .filter_map(|tier| tier.as_list())
                .flat_map(|tier| {
                    let urls: Vec<String> = tier.iter().filter_map(|u| u.as_str()).collect();
                    sanitize_tracker_urls(&urls)
                })
                .collect(),
            _ => match raw.get("announce").and_then(|v| v.as_str()) {
                Some(announce) => sanitize_tracker_urls(&[announce]),
                None => Vec::new(),
            },
        };

        let title = parse_title(&name, &files, None).unwrap_or_else(|| name.clone());

        Ok(Metafile {
            info_hash,
            length,
            name,
            title,
            piece_length,
            files,
            is_single_file_torrent,
            category: None,
            tags: None,
            trackers,
            raw,
            raw_bytes: Some(buf.to_vec()),
        })
    }

    /// `updateMetafileMetadata` — folds a client's fastresume data in.
    pub fn apply_metadata(&mut self, metadata: &TorrentMetadata) {
        if let Some(category) = &metadata.category {
            self.category = Some(category.clone());
        }
        if let Some(tags) = &metadata.tags {
            self.tags = Some(tags.clone());
        }
        if let Some(tiers) = &metadata.trackers {
            self.trackers = tiers
                .iter()
                .flat_map(|tier| sanitize_tracker_urls(tier))
                .collect();
        }
    }

    /// `getFileSystemSafeName` — the name is used as a directory, so a `/` in
    /// it would silently create a nested path.
    pub fn file_system_safe_name(&self) -> String {
        self.name.replace('/', "")
    }

    /// Returns the original bytes when available, so a `.torrent` handed to a
    /// client is byte-identical to what was read.
    pub fn encode(&self) -> Vec<u8> {
        match &self.raw_bytes {
            Some(bytes) => bytes.clone(),
            None => bencode::encode(&self.raw),
        }
    }

    /// True when the `private` flag is set, which decides whether a torrent may
    /// be announced to other trackers.
    pub fn is_private(&self) -> bool {
        self.raw
            .get("info")
            .and_then(|info| info.get("private"))
            .and_then(|v| v.as_int())
            .is_some_and(|v| v != 0)
    }
}

#[cfg(test)]
pub mod fixtures {
    use super::*;
    use std::collections::BTreeMap;

    pub fn bytes(value: &str) -> Value {
        Value::Bytes(value.as_bytes().to_vec())
    }

    fn dict(entries: Vec<(&str, Value)>) -> Value {
        let mut map = BTreeMap::new();
        for (key, value) in entries {
            map.insert(key.as_bytes().to_vec(), value);
        }
        Value::Dict(map)
    }

    pub fn single_file_torrent(name: &str, length: i64) -> Vec<u8> {
        bencode::encode(&dict(vec![
            ("announce", bytes("http://tracker.example/announce")),
            (
                "info",
                dict(vec![
                    ("name", bytes(name)),
                    ("piece length", Value::Int(16384)),
                    ("pieces", Value::Bytes(vec![0u8; 20])),
                    ("length", Value::Int(length)),
                    ("private", Value::Int(1)),
                ]),
            ),
        ]))
    }

    pub fn multi_file_torrent(name: &str, files: &[(&[&str], i64)]) -> Vec<u8> {
        let entries: Vec<Value> = files
            .iter()
            .map(|(segments, length)| {
                dict(vec![
                    ("length", Value::Int(*length)),
                    (
                        "path",
                        Value::List(segments.iter().map(|s| bytes(s)).collect()),
                    ),
                ])
            })
            .collect();
        bencode::encode(&dict(vec![
            (
                "announce-list",
                Value::List(vec![Value::List(vec![bytes(
                    "http://tracker.example:8080/announce",
                )])]),
            ),
            (
                "info",
                dict(vec![
                    ("name", bytes(name)),
                    ("piece length", Value::Int(16384)),
                    ("pieces", Value::Bytes(vec![0u8; 40])),
                    ("files", Value::List(entries)),
                ]),
            ),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn parses_a_single_file_torrent() {
        let meta = Metafile::decode(&single_file_torrent("Movie.2020.1080p.mkv", 1234)).unwrap();
        assert_eq!(meta.name, "Movie.2020.1080p.mkv");
        assert!(meta.is_single_file_torrent);
        assert_eq!(meta.length, 1234);
        assert_eq!(meta.files.len(), 1);
        assert_eq!(meta.files[0].path, "Movie.2020.1080p.mkv");
        assert_eq!(meta.trackers, vec!["tracker.example"]);
        assert!(meta.is_private());
    }

    #[test]
    fn parses_a_multi_file_torrent_and_sums_lengths() {
        let meta = Metafile::decode(&multi_file_torrent(
            "Some.Show.S01",
            &[
                (&["Some.Show.S01E02.mkv"], 200),
                (&["Some.Show.S01E01.mkv"], 100),
            ],
        ))
        .unwrap();
        assert!(!meta.is_single_file_torrent);
        assert_eq!(meta.length, 300);
        // Sorted by path, so E01 comes first despite the input order.
        assert_eq!(meta.files[0].path, "Some.Show.S01/Some.Show.S01E01.mkv");
        assert_eq!(meta.files[0].name, "Some.Show.S01E01.mkv");
        assert_eq!(meta.trackers, vec!["tracker.example:8080"]);
    }

    /// The keystone property: the hash must be sha1 of the info dict's bytes.
    #[test]
    fn info_hash_is_sha1_of_the_info_dict_bytes() {
        let buf = single_file_torrent("Movie.2020.mkv", 42);
        let meta = Metafile::decode(&buf).unwrap();

        let (_, span) = bencode::decode_root(&buf).unwrap();
        let expected = hex::encode(Sha1::digest(&buf[span.unwrap()]));
        assert_eq!(meta.info_hash, expected);
        assert_eq!(meta.info_hash.len(), 40);
    }

    /// A torrent whose dictionary keys are NOT in sorted order still has to
    /// hash over its own bytes — this is exactly the case a decode/re-encode
    /// round trip would silently get wrong.
    #[test]
    fn info_hash_survives_non_canonical_input() {
        // "name" before "length" is not sorted order; a canonical re-encode
        // would reorder them and change the hash.
        let raw = b"d4:infod4:name9:Movie.mkv12:piece lengthi16384e6:pieces20:00000000000000000000\
6:lengthi42eee";
        let meta = Metafile::decode(raw).unwrap();

        let info_start = raw.windows(2).position(|w| w == b"d4").unwrap();
        let _ = info_start;
        let (_, span) = bencode::decode_root(raw).unwrap();
        let source_bytes = &raw[span.unwrap()];
        assert_eq!(meta.info_hash, hex::encode(Sha1::digest(source_bytes)));

        // Prove the round-trip WOULD differ, so the test above is meaningful.
        let reencoded = bencode::encode(meta.raw.get("info").unwrap());
        assert_ne!(reencoded.as_slice(), source_bytes);
        assert_ne!(meta.info_hash, hex::encode(Sha1::digest(&reencoded)));
    }

    #[test]
    fn empty_path_segments_become_underscores() {
        let meta =
            Metafile::decode(&multi_file_torrent("Pack", &[(&["", "file.mkv"], 10)])).unwrap();
        assert_eq!(meta.files[0].path, "Pack/_/file.mkv");
    }

    #[test]
    fn missing_required_fields_are_rejected() {
        let no_info = bencode::encode(&Value::Dict(Default::default()));
        assert!(matches!(
            Metafile::decode(&no_info),
            Err(MetafileError::MissingField("info"))
        ));
    }

    #[test]
    fn encode_returns_the_original_bytes() {
        let buf = single_file_torrent("Movie.mkv", 1);
        let meta = Metafile::decode(&buf).unwrap();
        assert_eq!(meta.encode(), buf);
    }

    #[test]
    fn client_metadata_overrides_trackers_and_labels() {
        let mut meta = Metafile::decode(&single_file_torrent("Movie.mkv", 1)).unwrap();
        meta.apply_metadata(&TorrentMetadata {
            trackers: Some(vec![vec![
                "https://other.example:443/announce".into(),
                "http://third.example:2710/announce".into(),
            ]]),
            category: Some("movies".into()),
            tags: Some(vec!["cross-seed".into()]),
        });
        // 443 is https's default port and is dropped, as `new URL().host` does;
        // a non-default port is kept.
        assert_eq!(meta.trackers, vec!["other.example", "third.example:2710"]);
        assert_eq!(meta.category.as_deref(), Some("movies"));
        assert_eq!(meta.tags.as_deref(), Some(&["cross-seed".to_string()][..]));
    }

    #[test]
    fn locale_compare_matches_icu_ordering() {
        // Byte order would put "Banana" first; ICU (and Node) does not.
        assert_eq!(locale_compare("apple", "Banana"), std::cmp::Ordering::Less);
        // Tertiary level: lowercase sorts before uppercase.
        assert_eq!(
            locale_compare("Apple", "apple"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(locale_compare("apple", "apple"), std::cmp::Ordering::Equal);
    }
}
