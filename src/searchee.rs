//! Searchees: the things cross-seed searches *for*.
//!
//! Ported from `searchee.ts`. A searchee is whatever local content a search is
//! based on — a torrent in a client, a `.torrent` file, a directory under
//! `dataDirs`, or a "virtual" season assembled from individual episodes.
//!
//! This module owns the release-name intelligence: turning `Season 07` plus a
//! folder of episodes into `Show S7 1080p-GROUP`, and deriving the key titles
//! that group episodes into ensembles.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::constants::{
    AKA_REGEX, ANIME_GROUP_REGEX, ANIME_REGEX, ARR_DIR_REGEX, AUDIO_EXTENSIONS,
    BAD_GROUP_PARSE_REGEX, BOOK_EXTENSIONS, EP_REGEX, MOVIE_REGEX, MediaType, RELEASE_GROUP_REGEX,
    REPACK_PROPER_REGEX, RES_STRICT_REGEX, SEASON_REGEX, SONARR_SUBFOLDERS_REGEX,
    VIDEO_DISC_EXTENSIONS, VIDEO_EXTENSIONS, parse_source,
};
use crate::logger::Label;
use crate::utils::{
    capture_group, create_key_title, extname, extract_int, is_bad_title, match_episode,
    strip_extension,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct File {
    pub name: String,
    pub path: String,
    pub length: i64,
}

/// Which pipeline produced this searchee — printed in log labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearcheeLabel {
    Search,
    Rss,
    Inject,
    Announce,
    Webhook,
}

impl SearcheeLabel {
    pub fn as_label(self) -> Label {
        match self {
            SearcheeLabel::Search => Label::Search,
            SearcheeLabel::Rss => Label::Rss,
            SearcheeLabel::Inject => Label::Inject,
            SearcheeLabel::Announce => Label::Announce,
            SearcheeLabel::Webhook => Label::Webhook,
        }
    }

    pub fn as_str(self) -> &'static str {
        self.as_label().as_str()
    }
}

/// The TypeScript original expressed the four shapes of a searchee as a union
/// of narrowed types (`SearcheeWithInfoHash`, `SearcheeClient`, …). Rust keeps
/// one struct and the [`SearcheeSource`] discriminator, with the narrowing
/// expressed by the accessors below — the invariants are the same:
///
/// * a **client** searchee has `info_hash`, `client_host`, `save_path`, `trackers`;
/// * a **torrent-file** searchee has `info_hash` only;
/// * a **data** searchee has `path` only;
/// * a **virtual** searchee has neither.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Searchee {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub files: Vec<File>,
    /// The original name. Use this when touching the filesystem or logging a
    /// client action.
    pub name: String,
    /// Usually `name`, but improved where possible — `Season 7` becomes
    /// `Show S7`.
    pub title: String,
    pub length: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trackers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<SearcheeLabel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearcheeSource {
    #[serde(rename = "torrentClient")]
    Client,
    #[serde(rename = "torrentFile")]
    Torrent,
    #[serde(rename = "dataDir")]
    Data,
    #[serde(rename = "virtual")]
    Virtual,
}

impl SearcheeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SearcheeSource::Client => "torrentClient",
            SearcheeSource::Torrent => "torrentFile",
            SearcheeSource::Data => "dataDir",
            SearcheeSource::Virtual => "virtual",
        }
    }
}

impl Searchee {
    pub fn source(&self) -> SearcheeSource {
        if self.save_path.is_some() {
            SearcheeSource::Client
        } else if self.info_hash.is_some() {
            SearcheeSource::Torrent
        } else if self.path.is_some() {
            SearcheeSource::Data
        } else {
            SearcheeSource::Virtual
        }
    }

    pub fn is_virtual(&self) -> bool {
        self.info_hash.is_none() && self.path.is_none()
    }

    pub fn media_type(&self) -> MediaType {
        media_type_of(&self.title, &self.files)
    }
}

/// `getMediaType`. The original leaned on a `switch (true)` with deliberate
/// fall-through from the `.rar` arm into the audio/book defaults; that
/// fall-through is reproduced explicitly here.
pub fn media_type_of(title: &str, files: &[File]) -> MediaType {
    if EP_REGEX.is_match(title).unwrap_or(false) {
        return MediaType::Episode;
    }
    if SEASON_REGEX.is_match(title).unwrap_or(false) {
        return MediaType::Season;
    }
    if has_ext(files, VIDEO_EXTENSIONS) {
        if MOVIE_REGEX.is_match(title).unwrap_or(false) {
            return MediaType::Movie;
        }
        if ANIME_REGEX.is_match(title).unwrap_or(false) {
            return MediaType::Anime;
        }
        return MediaType::Video;
    }
    if has_ext(files, VIDEO_DISC_EXTENSIONS) {
        if MOVIE_REGEX.is_match(title).unwrap_or(false) {
            return MediaType::Movie;
        }
        return MediaType::Video;
    }
    if has_ext(files, &[".rar"]) && MOVIE_REGEX.is_match(title).unwrap_or(false) {
        return MediaType::Movie;
    }
    if has_ext(files, AUDIO_EXTENSIONS) {
        return MediaType::Audio;
    }
    if has_ext(files, BOOK_EXTENSIONS) {
        return MediaType::Book;
    }
    MediaType::Other
}

/// How far a candidate's *total* size may drift from the searchee's.
///
/// A virtual (ensemble) searchee is an approximation assembled from episodes,
/// so it is judged by `seasonFromEpisodes` — the fraction of the season that
/// must be present — rather than by `fuzzySizeThreshold`.
pub fn get_fuzzy_size_factor(searchee: &Searchee) -> f64 {
    let config = crate::config::runtime::get_runtime_config();
    match config.season_from_episodes {
        Some(season_from_episodes) if searchee.is_virtual() => 1.0 - season_from_episodes,
        _ => config.fuzzy_size_threshold,
    }
}

/// The complement of [`get_fuzzy_size_factor`]: the minimum fraction of a
/// candidate that must be satisfied for a partial match.
pub fn get_min_size_ratio(searchee: &Searchee) -> f64 {
    let config = crate::config::runtime::get_runtime_config();
    match config.season_from_episodes {
        Some(season_from_episodes) if searchee.is_virtual() => season_from_episodes,
        _ => 1.0 - config.fuzzy_size_threshold,
    }
}

pub fn has_ext(files: &[File], exts: &[&str]) -> bool {
    files
        .iter()
        .any(|f| exts.contains(&extname(&f.name).as_str()))
}

pub fn files_with_ext(files: &[File], exts: &[&str]) -> Vec<File> {
    files
        .iter()
        .filter(|f| exts.contains(&extname(&f.name).as_str()))
        .cloned()
        .collect()
}

pub fn largest_file(files: &[File]) -> Option<&File> {
    files
        .iter()
        .reduce(|a, b| if a.length > b.length { a } else { b })
}

/// `getRoot` — the first path component of a torrent-relative file path.
/// Absolute paths are a hard error: a torrent's file tree is relative to the
/// save path by definition, and treating one as absolute would write outside it.
pub fn get_root(file: &File) -> Result<String, String> {
    let path = &file.path;
    if Path::new(path).is_absolute()
        || path.starts_with('/')
        || crate::constants::ABS_WIN_PATH_REGEX
            .is_match(path)
            .unwrap_or(false)
    {
        return Err(format!(
            "absolute paths for the torrent file tree are not supported. File tree paths must be relative to the torrent save path: {path}"
        ));
    }
    let mut components = Path::new(path).components();
    match components.next() {
        Some(first) => Ok(first.as_os_str().to_string_lossy().into_owned()),
        None => Ok(path.clone()),
    }
}

/// `getRootFolder` — `None` when the file sits at the torrent root.
pub fn get_root_folder(file: &File) -> Result<Option<String>, String> {
    let root = get_root(file)?;
    if root == file.path {
        Ok(None)
    } else {
        Ok(Some(root))
    }
}

// ─── Title parsing ──────────────────────────────────────────────────────────

/// Extra qualifiers appended to a parsed title so the matcher can tell a 1080p
/// REPACK from a 720p original. Only added when *every* video file agrees.
fn parse_meta_info(video_file_names: &[String]) -> String {
    let mut meta_info = String::new();
    let stems: Vec<String> = video_file_names
        .iter()
        .map(|n| strip_extension(n))
        .collect();

    let repacks = stems
        .iter()
        .filter_map(|stem| capture_group(&REPACK_PROPER_REGEX, stem, "type"))
        .count();
    if repacks > 0 {
        meta_info.push_str(" REPACK");
    }

    let resolutions: Vec<String> = stems
        .iter()
        .filter_map(|stem| capture_group(&RES_STRICT_REGEX, stem, "res"))
        .map(|res| res.trim().to_lowercase())
        .collect();
    if resolutions.len() == stems.len()
        && !resolutions.is_empty()
        && resolutions.iter().all(|r| r == &resolutions[0])
    {
        meta_info.push_str(&format!(" {}", resolutions[0]));
    }

    let sources: Vec<&str> = stems.iter().filter_map(|stem| parse_source(stem)).collect();
    if sources.len() == stems.len()
        && !sources.is_empty()
        && sources.iter().all(|s| *s == sources[0])
    {
        meta_info.push_str(&format!(" {}", sources[0]));
    }

    let groups: Vec<String> = stems
        .iter()
        .filter_map(|stem| get_release_group(stem))
        .collect();
    if groups.len() == stems.len()
        && !groups.is_empty()
        && groups
            .iter()
            .all(|g| g.to_lowercase() == groups[0].to_lowercase())
    {
        meta_info.push_str(&format!("-{}", groups[0]));
    }

    meta_info
}

/// Derives a searchable title from a name plus its file list.
///
/// Handles the Sonarr layout where a season lives in a folder literally called
/// `Season 07`: the name carries no series title, so it is recovered from the
/// episode filenames, or failing that from the parent directory.
///
/// Returns `None` when the name is a bare `Season NN` and nothing better could
/// be found — the caller treats that as "not searchable".
pub fn parse_title(name: &str, files: &[File], path: Option<&str>) -> Option<String> {
    let season_match = if name.chars().count() < 12 {
        SONARR_SUBFOLDERS_REGEX
            .captures(name)
            .ok()
            .flatten()
            .and_then(|caps| caps.name("seasonNum").map(|m| m.as_str().to_string()))
    } else {
        None
    };

    if season_match.is_none()
        && (name.chars().any(|c| c.is_ascii_digit()) || !has_ext(files, VIDEO_EXTENSIONS))
    {
        return Some(name.to_string());
    }

    let video_files = files_with_ext(files, VIDEO_EXTENSIONS);
    let video_names: Vec<String> = video_files.iter().map(|f| f.name.clone()).collect();

    for video_file in &video_files {
        if let Some(ep) = match_episode(&EP_REGEX, &video_file.name) {
            let season_val = ep
                .season
                .clone()
                .or_else(|| ep.year.clone())
                .or_else(|| season_match.clone());
            let season = season_val
                .and_then(|v| extract_int(&v))
                .map(|n| format!("S{n}"))
                .unwrap_or_default();
            let episode = if video_files.len() == 1 {
                match &ep.episode {
                    Some(episode) => format!("E{}", extract_int(episode).unwrap_or_default()),
                    None => format!(
                        "E{}.{}",
                        ep.month.clone().unwrap_or_default(),
                        ep.day.clone().unwrap_or_default()
                    ),
                }
            } else {
                String::new()
            };
            if !season.is_empty() || !episode.is_empty() || season_match.is_none() {
                let meta_info = parse_meta_info(&video_names);
                let title = ep.title.clone().unwrap_or_default();
                return Some(
                    format!("{title} {season}{episode}{meta_info}")
                        .trim()
                        .to_string(),
                );
            }
        }

        if let (Some(path), Some(season_num)) = (path, season_match.as_ref()) {
            let parent = Path::new(path)
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(title) = capture_group(&ARR_DIR_REGEX, &parent, "title")
                && !title.is_empty()
            {
                let meta_info = parse_meta_info(&video_names);
                return Some(format!("{title} S{season_num}{meta_info}"));
            }
        }

        if let Ok(Some(anime)) = ANIME_REGEX.captures(&video_file.name) {
            let season = season_match
                .as_ref()
                .map(|n| format!("S{n}"))
                .unwrap_or_default();
            if !season.is_empty() || season_match.is_none() {
                let meta_info = parse_meta_info(&video_names);
                let title = anime.name("title").map(|m| m.as_str()).unwrap_or_default();
                return Some(format!("{title} {season}{meta_info}").trim().to_string());
            }
        }
    }

    if season_match.is_none() {
        Some(name.to_string())
    } else {
        None
    }
}

/// Expands `Title AKA Alternate` into both halves, so either spelling can match.
pub fn get_all_titles(titles: &[String]) -> Vec<String> {
    let mut all = titles.to_vec();
    for title in titles {
        if AKA_REGEX.is_match(title).unwrap_or(false) && title.trim().to_lowercase() != "aka" {
            let mut last = 0usize;
            let mut parts: Vec<String> = Vec::new();
            let mut pos = 0usize;
            while let Ok(Some(m)) = AKA_REGEX.find_from_pos(title, pos) {
                parts.push(title[last..m.start()].to_string());
                last = m.end();
                pos = if m.end() > m.start() {
                    m.end()
                } else {
                    m.end() + 1
                };
            }
            parts.push(title[last..].to_string());
            all.extend(parts);
        }
    }
    all
}

// ─── Ensemble keys ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieKeys {
    pub ensemble_titles: Vec<String>,
    pub key_titles: Vec<String>,
    pub year: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonKeys {
    pub ensemble_titles: Vec<String>,
    pub key_titles: Vec<String>,
    pub season: String,
}

/// An episode identifier: a number for `SxxEyy`, or `MM.DD` for dated shows.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum EpisodeId {
    Number(i64),
    Date(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeKeys {
    pub ensemble_titles: Vec<String>,
    pub key_titles: Vec<String>,
    pub season: Option<String>,
    pub episode: EpisodeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimeKeys {
    pub ensemble_titles: Vec<String>,
    pub key_titles: Vec<String>,
    pub release: i64,
}

pub fn get_movie_keys(stem: &str) -> Option<MovieKeys> {
    let caps = MOVIE_REGEX.captures(stem).ok().flatten()?;
    let titles = get_all_titles(&[caps.name("title")?.as_str().to_string()]);
    let year = extract_int(caps.name("year")?.as_str())?;
    let mut key_titles = Vec::new();
    let mut ensemble_titles = Vec::new();
    for title in titles {
        let Some(key_title) = create_key_title(&title) else {
            continue;
        };
        key_titles.push(key_title);
        ensemble_titles.push(format!("{title}.{year}"));
    }
    (!key_titles.is_empty()).then_some(MovieKeys {
        ensemble_titles,
        key_titles,
        year,
    })
}

pub fn get_season_keys(stem: &str) -> Option<SeasonKeys> {
    let caps = SEASON_REGEX.captures(stem).ok().flatten()?;
    let titles = get_all_titles(&[caps.name("title")?.as_str().to_string()]);
    let season = format!("S{}", extract_int(caps.name("season")?.as_str())?);
    let mut key_titles = Vec::new();
    let mut ensemble_titles = Vec::new();
    for title in titles {
        let Some(key_title) = create_key_title(&title) else {
            continue;
        };
        key_titles.push(key_title);
        ensemble_titles.push(format!("{title}.{season}"));
    }
    (!key_titles.is_empty()).then_some(SeasonKeys {
        ensemble_titles,
        key_titles,
        season,
    })
}

pub fn get_episode_keys(stem: &str) -> Option<EpisodeKeys> {
    let ep = match_episode(&EP_REGEX, stem)?;
    let titles = get_all_titles(&[ep.title.clone()?]);
    let season = match (&ep.season, &ep.year) {
        (Some(season), _) => Some(format!("S{}", extract_int(season)?)),
        (None, Some(year)) => Some(format!("S{year}")),
        _ => None,
    };
    let mut key_titles = Vec::new();
    let mut ensemble_titles = Vec::new();
    for title in titles {
        let Some(key_title) = create_key_title(&title) else {
            continue;
        };
        key_titles.push(key_title);
        ensemble_titles.push(match &season {
            Some(season) => format!("{title}.{season}"),
            None => title.clone(),
        });
    }
    if key_titles.is_empty() {
        return None;
    }
    let episode = match &ep.episode {
        Some(episode) => EpisodeId::Number(extract_int(episode)?),
        None => EpisodeId::Date(format!(
            "{}.{}",
            ep.month.clone().unwrap_or_default(),
            ep.day.clone().unwrap_or_default()
        )),
    };
    Some(EpisodeKeys {
        ensemble_titles,
        key_titles,
        season,
        episode,
    })
}

pub fn get_anime_keys(stem: &str) -> Option<AnimeKeys> {
    let caps = ANIME_REGEX.captures(stem).ok().flatten()?;
    let raw_titles: Vec<String> = ["title", "altTitle"]
        .iter()
        .filter_map(|name| caps.name(name).map(|m| m.as_str().to_string()))
        .collect();
    let titles = get_all_titles(&raw_titles);
    let mut key_titles = Vec::new();
    let mut ensemble_titles = Vec::new();
    for title in titles {
        if title.is_empty() || is_bad_title(&title) {
            continue;
        }
        let Some(key_title) = create_key_title(&title) else {
            continue;
        };
        key_titles.push(key_title);
        ensemble_titles.push(title);
    }
    if key_titles.is_empty() {
        return None;
    }
    let release = extract_int(caps.name("release")?.as_str())?;
    Some(AnimeKeys {
        ensemble_titles,
        key_titles,
        release,
    })
}

/// The scene group suffix (`-GROUP`), or `None` when what the regex found is
/// really part of the title or an encoding token.
pub fn get_release_group(stem: &str) -> Option<String> {
    let predicted = capture_group(&RELEASE_GROUP_REGEX, stem, "group")?;
    let predicted = predicted.trim().to_string();
    if BAD_GROUP_PARSE_REGEX.is_match(&predicted).unwrap_or(false) {
        return None;
    }

    let mut titles: Vec<String> = Vec::new();
    for re in [&*EP_REGEX, &*SEASON_REGEX, &*MOVIE_REGEX, &*ANIME_REGEX] {
        if let Ok(Some(caps)) = re.captures(stem) {
            for name in ["title", "altTitle"] {
                if let Some(m) = caps.name(name) {
                    titles.push(m.as_str().to_string());
                }
            }
            if !titles.is_empty() {
                break;
            }
        }
    }
    let titles = get_all_titles(&titles);

    // If the "group" is a substring of the title itself, it was a false positive.
    for title in titles {
        if let Some(group) = capture_group(&RELEASE_GROUP_REGEX, &title, "group")
            && predicted.contains(group.trim())
        {
            return None;
        }
    }
    Some(predicted)
}

/// The `.1080p.AMZN-GROUP` suffix appended to ensemble keys so different
/// releases of the same season do not collapse into one virtual searchee.
pub fn get_key_meta_info(stem: &str) -> String {
    let res = capture_group(&RES_STRICT_REGEX, stem, "res")
        .map(|res| format!(".{res}"))
        .unwrap_or_default();
    let source = parse_source(stem)
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    if let Some(group) = get_release_group(stem) {
        return format!("{res}{source}-{group}").to_lowercase();
    }
    if let Some(group) = capture_group(&ANIME_GROUP_REGEX, stem, "group") {
        return format!("{res}{source}-{group}").to_lowercase();
    }
    format!("{res}{source}").to_lowercase()
}

// ─── Persistence helpers ────────────────────────────────────────────────────

/// Rehydrates a client searchee from its `client_searchee` row. JSON columns
/// are decoded here rather than in `db`, matching the original's split.
pub fn searchee_from_db_row(row: &crate::db::ClientSearcheeRow) -> Searchee {
    Searchee {
        info_hash: Some(row.info_hash.clone()),
        name: row.name.clone().unwrap_or_default(),
        title: row.title.clone().unwrap_or_default(),
        files: row
            .files
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default(),
        length: row.length.unwrap_or(0),
        client_host: Some(row.client_host.clone()),
        save_path: row.save_path.clone(),
        category: row.category.clone(),
        tags: row
            .tags
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok()),
        trackers: Some(
            row.trackers
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default(),
        ),
        ..Default::default()
    }
}

/// `createSearcheeFromMetafile`.
pub fn searchee_from_metafile(meta: &crate::torrent::Metafile) -> Result<Searchee, String> {
    match parse_title(&meta.name, &meta.files, None) {
        Some(title) => Ok(Searchee {
            info_hash: Some(meta.info_hash.clone()),
            files: meta.files.clone(),
            name: meta.name.clone(),
            title,
            length: meta.length,
            category: meta.category.clone(),
            tags: meta.tags.clone(),
            trackers: Some(meta.trackers.clone()),
            ..Default::default()
        }),
        None => Err(format!(
            "Could not find title for {} from child files",
            meta.name
        )),
    }
}

/// Groups episode searchees by `<keyTitle>.<season><metaInfo>` and then by
/// episode, the first half of `createEnsembleSearchees`. Split out from the
/// filesystem-touching half so it can be tested without any I/O.
/// `{ ensembleKey: { episode: [index into all_searchees] } }`.
pub type EnsembleKeyMap = BTreeMap<String, BTreeMap<EpisodeId, Vec<usize>>>;
/// `{ ensembleKey: displayTitle }`.
pub type EnsembleTitleMap = BTreeMap<String, String>;

pub fn organize_ensemble_keys(
    all_searchees: &[Searchee],
    use_filters: bool,
) -> (EnsembleKeyMap, EnsembleTitleMap) {
    // Keys that already exist as a real season release are skipped: there is
    // no point synthesising a season we could search for directly.
    let mut existing_season_keys: Vec<String> = Vec::new();
    if use_filters {
        for searchee in all_searchees {
            let stem = strip_extension(&searchee.title);
            let Some(season_keys) = get_season_keys(&stem) else {
                continue;
            };
            let info = get_key_meta_info(&stem);
            for key_title in &season_keys.key_titles {
                existing_season_keys.push(format!("{key_title}.{}{info}", season_keys.season));
            }
        }
    }

    let mut key_map: EnsembleKeyMap = BTreeMap::new();
    let mut ensemble_title_map: EnsembleTitleMap = BTreeMap::new();

    let record = |keys: Vec<String>,
                  ensemble_titles: Vec<String>,
                  episode: EpisodeId,
                  index: usize,
                  key_map: &mut EnsembleKeyMap,
                  ensemble_title_map: &mut EnsembleTitleMap| {
        for (i, key) in keys.iter().enumerate() {
            if existing_season_keys.contains(key) {
                continue;
            }
            ensemble_title_map
                .entry(key.clone())
                .or_insert_with(|| ensemble_titles[i].clone());
            key_map
                .entry(key.clone())
                .or_default()
                .entry(episode.clone())
                .or_default()
                .push(index);
        }
    };

    for (index, searchee) in all_searchees.iter().enumerate() {
        let stem = strip_extension(&searchee.title);

        if let Some(episode_keys) = get_episode_keys(&stem) {
            let info = get_key_meta_info(&stem);
            let keys: Vec<String> = episode_keys
                .key_titles
                .iter()
                .map(|k| match &episode_keys.season {
                    Some(season) => format!("{k}.{season}{info}"),
                    None => format!("{k}{info}"),
                })
                .collect();
            let ensemble_titles: Vec<String> = episode_keys
                .ensemble_titles
                .iter()
                .map(|t| format!("{t}{info}"))
                .collect();
            record(
                keys,
                ensemble_titles,
                episode_keys.episode.clone(),
                index,
                &mut key_map,
                &mut ensemble_title_map,
            );
            if use_filters {
                continue;
            }
        }

        if use_filters && SEASON_REGEX.is_match(&stem).unwrap_or(false) {
            continue;
        }
        // The anime regex is loose enough to match non-video releases, so it is
        // only trusted when the searchee actually contains video.
        if !has_ext(&searchee.files, VIDEO_EXTENSIONS) {
            continue;
        }
        if let Some(anime_keys) = get_anime_keys(&stem) {
            let info = get_key_meta_info(&stem);
            let keys: Vec<String> = anime_keys
                .key_titles
                .iter()
                .map(|k| format!("{k}{info}"))
                .collect();
            let ensemble_titles: Vec<String> = anime_keys
                .ensemble_titles
                .iter()
                .map(|t| format!("{t}{info}"))
                .collect();
            record(
                keys,
                ensemble_titles,
                EpisodeId::Number(anime_keys.release),
                index,
                &mut key_map,
                &mut ensemble_title_map,
            );
        }
    }

    (key_map, ensemble_title_map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, length: i64) -> File {
        File {
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            path: path.to_string(),
            length,
        }
    }

    #[test]
    fn media_type_detection() {
        let video = vec![file("Show.S01E01.mkv", 100)];
        assert_eq!(media_type_of("Show.S01E01", &video), MediaType::Episode);
        assert_eq!(media_type_of("Show.S01", &video), MediaType::Season);
        assert_eq!(media_type_of("Movie.2020", &video), MediaType::Movie);
        assert_eq!(
            media_type_of("Something", &[file("book.epub", 1)]),
            MediaType::Book
        );
        assert_eq!(
            media_type_of("Something", &[file("song.flac", 1)]),
            MediaType::Audio
        );
        assert_eq!(
            media_type_of("Something", &[file("readme.txt", 1)]),
            MediaType::Other
        );
    }

    #[test]
    fn a_normal_release_name_is_its_own_title() {
        let files = vec![file("Some.Show.S01E01.1080p.mkv", 10)];
        assert_eq!(
            parse_title("Some.Show.S01E01.1080p.WEB-DL", &files, None).as_deref(),
            Some("Some.Show.S01E01.1080p.WEB-DL")
        );
    }

    /// The Sonarr layout: the folder is called `Season 07` and the series name
    /// only exists in the episode filenames.
    #[test]
    fn sonarr_season_folder_recovers_the_series_title_from_episodes() {
        let files = vec![
            file("Some.Show.S07E01.1080p.WEB-DL.mkv", 10),
            file("Some.Show.S07E02.1080p.WEB-DL.mkv", 10),
        ];
        let title = parse_title("Season 07", &files, None).unwrap();
        assert!(title.starts_with("Some.Show S7"), "got {title}");
        // Shared resolution across every episode is folded into the title.
        assert!(title.contains("1080p"), "got {title}");
    }

    /// …and when the filenames are useless, from the parent directory.
    #[test]
    fn sonarr_season_folder_falls_back_to_the_parent_directory() {
        let files = vec![file("01.mkv", 10), file("02.mkv", 10)];
        let title = parse_title("Season 03", &files, Some("/tv/Some Show (2019)/Season 03"));
        assert_eq!(title.as_deref(), Some("Some Show (2019) S03"));
    }

    #[test]
    fn unparseable_season_folder_yields_no_title() {
        let files = vec![file("01.mkv", 10)];
        assert_eq!(parse_title("Season 03", &files, None), None);
    }

    #[test]
    fn single_episode_titles_include_the_episode_number() {
        let files = vec![file("Some.Show.S07E04.mkv", 10)];
        let title = parse_title("Season 07", &files, None).unwrap();
        assert!(title.contains("S7E4"), "got {title}");
    }

    #[test]
    fn aka_titles_are_expanded() {
        let titles = get_all_titles(&["Le Film a.k.a. The Movie".to_string()]);
        assert!(titles.iter().any(|t| t.trim() == "Le Film"));
        assert!(titles.iter().any(|t| t.trim() == "The Movie"));
    }

    #[test]
    fn movie_keys_extract_title_and_year() {
        let keys = get_movie_keys("Some.Movie.2020.1080p.BluRay-GRP").unwrap();
        assert_eq!(keys.year, 2020);
        assert_eq!(keys.key_titles, vec!["somemovie"]);
    }

    #[test]
    fn season_keys_normalise_the_season_number() {
        let keys = get_season_keys("Some.Show.S07.1080p").unwrap();
        assert_eq!(keys.season, "S7");
    }

    #[test]
    fn episode_keys_carry_season_and_episode() {
        let keys = get_episode_keys("Some.Show.S07E04.1080p").unwrap();
        assert_eq!(keys.season.as_deref(), Some("S7"));
        assert_eq!(keys.episode, EpisodeId::Number(4));
    }

    #[test]
    fn dated_episode_keys_use_the_date() {
        let keys = get_episode_keys("Some.Show.2024.01.02.WEB").unwrap();
        assert_eq!(keys.episode, EpisodeId::Date("01.02".to_string()));
        assert_eq!(keys.season.as_deref(), Some("S2024"));
    }

    #[test]
    fn release_group_is_rejected_when_it_is_really_the_title() {
        // An encoding-token soup must not be mistaken for a group.
        assert_eq!(get_release_group("Some.Show.S01E01-1080p"), None);
    }

    #[test]
    fn absolute_file_paths_are_rejected() {
        assert!(get_root(&file("/abs/path.mkv", 1)).is_err());
        assert_eq!(get_root(&file("Pack/file.mkv", 1)).unwrap(), "Pack");
        assert_eq!(get_root_folder(&file("file.mkv", 1)).unwrap(), None);
    }

    #[test]
    fn ensemble_keys_group_episodes_of_the_same_season() {
        let searchees: Vec<Searchee> = ["Some.Show.S01E01.1080p", "Some.Show.S01E02.1080p"]
            .iter()
            .map(|name| Searchee {
                name: name.to_string(),
                title: name.to_string(),
                files: vec![file(&format!("{name}.mkv"), 100)],
                length: 100,
                ..Default::default()
            })
            .collect();

        let (key_map, titles) = organize_ensemble_keys(&searchees, true);
        assert_eq!(key_map.len(), 1);
        let (key, episodes) = key_map.iter().next().unwrap();
        assert_eq!(episodes.len(), 2);
        assert!(episodes.contains_key(&EpisodeId::Number(1)));
        assert!(episodes.contains_key(&EpisodeId::Number(2)));
        assert!(titles[key].contains("Some.Show"));
    }

    /// If the user already has the real season, no virtual one is synthesised.
    #[test]
    fn ensemble_keys_skip_seasons_that_already_exist() {
        let mut searchees: Vec<Searchee> = ["Some.Show.S01E01.1080p", "Some.Show.S01E02.1080p"]
            .iter()
            .map(|name| Searchee {
                name: name.to_string(),
                title: name.to_string(),
                files: vec![file(&format!("{name}.mkv"), 100)],
                length: 100,
                ..Default::default()
            })
            .collect();
        searchees.push(Searchee {
            name: "Some.Show.S01.1080p".into(),
            title: "Some.Show.S01.1080p".into(),
            files: vec![file("Some.Show.S01.1080p/e1.mkv", 100)],
            length: 100,
            ..Default::default()
        });

        let (key_map, _) = organize_ensemble_keys(&searchees, true);
        assert!(key_map.is_empty());
    }

    #[test]
    fn searchee_source_discriminates_the_four_shapes() {
        let base = Searchee::default();
        assert_eq!(base.source(), SearcheeSource::Virtual);
        assert_eq!(
            Searchee {
                info_hash: Some("x".into()),
                ..base.clone()
            }
            .source(),
            SearcheeSource::Torrent
        );
        assert_eq!(
            Searchee {
                path: Some("/data/x".into()),
                ..base.clone()
            }
            .source(),
            SearcheeSource::Data
        );
        assert_eq!(
            Searchee {
                info_hash: Some("x".into()),
                save_path: Some("/downloads".into()),
                ..base
            }
            .source(),
            SearcheeSource::Client
        );
    }
}
