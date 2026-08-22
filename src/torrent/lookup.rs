//! Reverse lookup: finding local content that could match an incoming release.
//!
//! Ported from the lookup half of `torrent.ts`.
//!
//! Announce handling has a hard latency budget, so this deliberately does *not*
//! compare file trees. It narrows to a handful of plausible candidates by
//! parsed key title plus season/episode, and only then does the expensive work.

use std::path::Path;

use sqlx::SqlitePool;

use crate::config::RuntimeConfig;
use crate::constants::{
    ALL_PARENTHESES_REGEX, ALL_SPACES_REGEX, ALL_SQUARE_BRACKETS_REGEX, LEVENSHTEIN_DIVISOR,
    MIN_VIDEO_QUERY_LENGTH,
};
use crate::db::ClientSearcheeRow;
use crate::searchee::{
    Searchee, get_anime_keys, get_episode_keys, get_movie_keys, get_season_keys,
    searchee_from_db_row, searchee_from_metafile,
};
use crate::utils::{create_key_title, exists, strip_extension};

/// The parsed identity of a release name: the key titles it could be filed
/// under, plus the season/episode/release element that must match exactly.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NameKeys {
    pub key_titles: Vec<String>,
    pub element: Option<String>,
    /// Nothing parsed: the caller must fall back to a fuzzy name sweep.
    pub use_fallback: bool,
}

pub fn get_keys_from_name(stem: &str) -> NameKeys {
    if let Some(keys) = get_episode_keys(stem) {
        let element = match (&keys.season, &keys.episode) {
            (Some(season), episode) => format!("{season}.{}", element_str(episode)),
            (None, episode) => element_str(episode),
        };
        return NameKeys {
            key_titles: keys.key_titles,
            element: Some(element),
            use_fallback: false,
        };
    }
    if let Some(keys) = get_season_keys(stem) {
        return NameKeys {
            key_titles: keys.key_titles,
            element: Some(keys.season),
            use_fallback: false,
        };
    }
    if let Some(keys) = get_movie_keys(stem) {
        return NameKeys {
            key_titles: keys.key_titles,
            element: None,
            use_fallback: false,
        };
    }
    if let Some(keys) = get_anime_keys(stem) {
        return NameKeys {
            key_titles: keys.key_titles,
            element: Some(keys.release.to_string()),
            use_fallback: false,
        };
    }
    NameKeys {
        key_titles: Vec::new(),
        element: None,
        use_fallback: true,
    }
}

fn element_str(episode: &crate::searchee::EpisodeId) -> String {
    match episode {
        crate::searchee::EpisodeId::Number(n) => n.to_string(),
        crate::searchee::EpisodeId::Date(d) => d.clone(),
    }
}

/// The Levenshtein budget for a set of key titles: a third of the *shortest*
/// one, so a long title cannot swallow a short unrelated one.
fn max_distance_for(key_titles: &[String]) -> usize {
    key_titles
        .iter()
        .map(|t| t.chars().count())
        .min()
        .unwrap_or(0)
        / LEVENSHTEIN_DIVISOR
}

/// Whether a stored entry's parsed name is close enough to the candidate's.
pub fn entry_matches(candidate: &NameKeys, entry_name: &str) -> bool {
    let entry = get_keys_from_name(&strip_extension(entry_name));
    // The season/episode must match exactly — a fuzzy title match on the wrong
    // episode is worse than no match.
    if entry.element != candidate.element {
        return false;
    }
    if entry.key_titles.is_empty() {
        return false;
    }
    let max_distance =
        max_distance_for(&candidate.key_titles).min(max_distance_for(&entry.key_titles));
    entry.key_titles.iter().any(|entry_title| {
        candidate
            .key_titles
            .iter()
            .any(|key_title| strsim::levenshtein(key_title, entry_title) <= max_distance)
    })
}

#[derive(Debug, Clone, Default)]
pub struct SimilarResult {
    /// The `<keyTitle>.<element>` keys used, for logging. Empty means the fuzzy
    /// fallback was used.
    pub keys: Vec<String>,
    pub client_searchees: Vec<Searchee>,
    pub data_searchees: Vec<Searchee>,
}

/// Local content that could plausibly be the same release as `name`.
pub async fn get_similar_by_name(
    pool: &SqlitePool,
    name: &str,
    config: &RuntimeConfig,
) -> sqlx::Result<SimilarResult> {
    let stem = strip_extension(name);
    let parsed = get_keys_from_name(&stem);

    let mut result = SimilarResult::default();

    if parsed.use_fallback {
        result.client_searchees = get_torrent_by_fuzzy_name(pool, &stem, config).await?;
        result.data_searchees = data_searchees_by_fuzzy_name(pool, &stem).await?;
    }

    if parsed.key_titles.is_empty() {
        // Anime that fails the anime regex is often `[group] Title (Extra)`;
        // retry once with the brackets stripped.
        let no_brackets = ALL_SQUARE_BRACKETS_REGEX.replace_all(&stem, "");
        let no_parens = ALL_PARENTHESES_REGEX.replace_all(&no_brackets, "");
        let squeezed = ALL_SPACES_REGEX
            .replace_all(&no_parens, " ")
            .trim()
            .to_string();
        if squeezed.chars().count() >= MIN_VIDEO_QUERY_LENGTH && squeezed != stem {
            result
                .client_searchees
                .extend(get_torrent_by_fuzzy_name(pool, &squeezed, config).await?);
            result
                .data_searchees
                .extend(data_searchees_by_fuzzy_name(pool, &squeezed).await?);
        }
        return Ok(result);
    }

    // ─── client / torrentDir side ───────────────────────────────────────
    if config.use_client_torrents {
        let rows: Vec<ClientSearcheeRow> = sqlx::query_as("SELECT * FROM client_searchee")
            .fetch_all(pool)
            .await?;
        result.client_searchees.extend(
            rows.iter()
                .filter(|row| {
                    entry_matches(
                        &parsed,
                        row.title.as_deref().or(row.name.as_deref()).unwrap_or(""),
                    )
                })
                .map(searchee_from_db_row),
        );
    } else if config.torrent_dir.is_some() {
        let rows: Vec<(Option<String>, Option<String>)> =
            sqlx::query_as("SELECT name, file_path FROM torrent")
                .fetch_all(pool)
                .await?;
        for (name, file_path) in rows {
            let (Some(name), Some(file_path)) = (name, file_path) else {
                continue;
            };
            if !entry_matches(&parsed, &name) {
                continue;
            }
            if let Ok(meta) = super::index::parse_torrent_with_metadata(Path::new(&file_path)).await
                && let Ok(searchee) = searchee_from_metafile(&meta)
            {
                result.client_searchees.push(searchee);
            }
        }
    }

    // ─── dataDirs side ──────────────────────────────────────────────────
    let entries: Vec<crate::data_files::DataEntry> = sqlx::query_as("SELECT path, title FROM data")
        .fetch_all(pool)
        .await?;
    let mut missing = Vec::new();
    for entry in entries {
        if !entry_matches(&parsed, &entry.title) {
            continue;
        }
        if !exists(&entry.path).await {
            missing.push(entry.path);
            continue;
        }
        if let Some(searchee) = searchee_from_data_path(&entry.path).await {
            result.data_searchees.push(searchee);
        }
    }
    crate::data_files::delete_data_paths(pool, &missing).await?;

    result.keys = match &parsed.element {
        Some(element) => parsed
            .key_titles
            .iter()
            .map(|key_title| format!("{key_title}.{element}"))
            .collect(),
        None => parsed.key_titles.clone(),
    };
    Ok(result)
}

/// Builds a data-directory searchee from a path on disk.
pub async fn searchee_from_data_path(path: &str) -> Option<Searchee> {
    let root = Path::new(path);
    let files = crate::data_files::get_files_from_data_root(root).await;
    if files.is_empty() {
        return None;
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let title = crate::searchee::parse_title(&name, &files, Some(path))?;
    let length = files.iter().map(|f| f.length).sum();

    let mut newest: Option<f64> = None;
    for file in &files {
        let absolute = root.parent().unwrap_or(Path::new("/")).join(&file.path);
        if let Ok(metadata) = tokio::fs::metadata(&absolute).await
            && let Ok(modified) = metadata.modified()
            && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
        {
            let ms = since.as_millis() as f64;
            newest = Some(newest.map_or(ms, |current: f64| current.max(ms)));
        }
    }

    Some(Searchee {
        path: Some(path.to_string()),
        files,
        name,
        title,
        length,
        mtime_ms: newest,
        ..Default::default()
    })
}

async fn data_searchees_by_fuzzy_name(
    pool: &SqlitePool,
    stem: &str,
) -> sqlx::Result<Vec<Searchee>> {
    let entries = crate::data_files::get_data_by_fuzzy_name(pool, stem).await?;
    let mut searchees = Vec::new();
    for entry in entries {
        if let Some(searchee) = searchee_from_data_path(&entry.path).await {
            searchees.push(searchee);
        }
    }
    Ok(searchees)
}

/// Fuzzy sweep over torrent names, used when the release name does not parse.
pub async fn get_torrent_by_fuzzy_name(
    pool: &SqlitePool,
    stem: &str,
    config: &RuntimeConfig,
) -> sqlx::Result<Vec<Searchee>> {
    let full_match = create_key_title(stem);
    let candidate_max_distance = stem.chars().count() / LEVENSHTEIN_DIVISOR;

    let close_enough = |title: &str| {
        let db_title = strip_extension(title);
        let max_distance =
            candidate_max_distance.min(db_title.chars().count() / LEVENSHTEIN_DIVISOR);
        strsim::levenshtein(stem, &db_title) <= max_distance
    };
    let exact = |title: &str| {
        full_match.is_some() && create_key_title(&strip_extension(title)) == full_match
    };

    if config.use_client_torrents {
        let rows: Vec<ClientSearcheeRow> = sqlx::query_as("SELECT * FROM client_searchee")
            .fetch_all(pool)
            .await?;
        let title_of = |row: &ClientSearcheeRow| {
            row.title
                .clone()
                .or_else(|| row.name.clone())
                .unwrap_or_default()
        };
        // An exact key-title match short-circuits the fuzzy sweep.
        let exact_rows: Vec<&ClientSearcheeRow> =
            rows.iter().filter(|row| exact(&title_of(row))).collect();
        let candidates: Vec<&ClientSearcheeRow> = if exact_rows.is_empty() {
            rows.iter().collect()
        } else {
            exact_rows
        };
        return Ok(candidates
            .into_iter()
            .filter(|row| close_enough(&title_of(row)))
            .map(searchee_from_db_row)
            .collect());
    }

    let rows: Vec<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT name, file_path FROM torrent")
            .fetch_all(pool)
            .await?;
    let exact_rows: Vec<&(Option<String>, Option<String>)> = rows
        .iter()
        .filter(|(name, _)| exact(name.as_deref().unwrap_or("")))
        .collect();
    let candidates: Vec<&(Option<String>, Option<String>)> = if exact_rows.is_empty() {
        rows.iter().collect()
    } else {
        exact_rows
    };

    let mut searchees = Vec::new();
    for (name, file_path) in candidates {
        let (Some(name), Some(file_path)) = (name, file_path) else {
            continue;
        };
        if !close_enough(name) {
            continue;
        }
        if let Ok(meta) = super::index::parse_torrent_with_metadata(Path::new(file_path)).await
            && let Ok(searchee) = searchee_from_metafile(&meta)
        {
            searchees.push(searchee);
        }
    }
    Ok(searchees)
}

/// Looks up a searchee by info hash — the `webhook` route's `infoHash` form.
pub async fn get_torrent_by_info_hash(
    pool: &SqlitePool,
    info_hash: &str,
    config: &RuntimeConfig,
) -> sqlx::Result<Vec<Searchee>> {
    if config.use_client_torrents {
        let rows: Vec<ClientSearcheeRow> =
            sqlx::query_as("SELECT * FROM client_searchee WHERE info_hash = ?")
                .bind(info_hash)
                .fetch_all(pool)
                .await?;
        return Ok(rows.iter().map(searchee_from_db_row).collect());
    }

    let file_path: Option<String> =
        sqlx::query_scalar("SELECT file_path FROM torrent WHERE info_hash = ?")
            .bind(info_hash)
            .fetch_optional(pool)
            .await?
            .flatten();
    let Some(file_path) = file_path else {
        return Ok(Vec::new());
    };
    match super::index::parse_torrent_with_metadata(Path::new(&file_path)).await {
        Ok(meta) => Ok(searchee_from_metafile(&meta).into_iter().collect()),
        Err(_) => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_names_key_on_title_season_and_episode() {
        let keys = get_keys_from_name("Some.Show.S01E05.1080p");
        assert_eq!(keys.key_titles, vec!["someshow"]);
        assert_eq!(keys.element.as_deref(), Some("S1.5"));
        assert!(!keys.use_fallback);
    }

    #[test]
    fn season_names_key_on_the_season() {
        let keys = get_keys_from_name("Some.Show.S03.1080p");
        assert_eq!(keys.element.as_deref(), Some("S3"));
    }

    #[test]
    fn movie_names_have_no_element() {
        let keys = get_keys_from_name("Some.Movie.2020.1080p.BluRay");
        assert_eq!(keys.element, None);
        assert!(!keys.key_titles.is_empty());
    }

    #[test]
    fn unparseable_names_request_the_fuzzy_fallback() {
        let keys = get_keys_from_name("just some words");
        assert!(keys.use_fallback);
        assert!(keys.key_titles.is_empty());
    }

    /// Two releases of the same episode match even with different scene
    /// spellings, because the key title is normalised first.
    #[test]
    fn the_same_episode_matches_across_release_names() {
        let candidate = get_keys_from_name("Some.Show.S01E05.2160p.WEB-DL-GRP");
        assert!(entry_matches(
            &candidate,
            "Some Show S01E05 1080p BluRay-OTHER"
        ));
    }

    /// The element is compared exactly: a fuzzy title match on the wrong
    /// episode would be worse than no match at all.
    #[test]
    fn a_different_episode_never_matches() {
        let candidate = get_keys_from_name("Some.Show.S01E05.1080p");
        assert!(!entry_matches(&candidate, "Some.Show.S01E06.1080p"));
        assert!(!entry_matches(&candidate, "Some.Show.S02E05.1080p"));
    }

    #[test]
    fn unrelated_titles_do_not_match() {
        let candidate = get_keys_from_name("Some.Show.S01E05.1080p");
        assert!(!entry_matches(
            &candidate,
            "Totally.Different.Thing.S01E05.1080p"
        ));
    }

    /// The distance budget uses the shorter title, so a long name cannot
    /// absorb a short unrelated one.
    #[test]
    fn the_distance_budget_uses_the_shorter_title() {
        assert_eq!(max_distance_for(&["abcdefghi".to_string()]), 3);
        assert_eq!(
            max_distance_for(&["abcdefghi".to_string(), "abc".to_string()]),
            1
        );
    }
}
