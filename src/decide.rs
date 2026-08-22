//! Match assessment: deciding whether a candidate cross-seeds a searchee.
//!
//! Ported from `decide.ts`. This is the heart of cross-seed. The order of the
//! checks is load-bearing — cheap name-based rejections run *before* the
//! torrent is downloaded, so a mismatched release group never costs a request.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::config::RuntimeConfig;
use crate::constants::{
    ANIME_GROUP_REGEX, Decision, MatchMode, REPACK_PROPER_REGEX, RES_STRICT_REGEX, SEASON_REGEX,
    parse_source,
};
use crate::logger::Label;
use crate::prefilter::{find_blocked_string_in_release_maybe, is_single_episode};
use crate::searchee::{
    File, Searchee, get_fuzzy_size_factor, get_min_size_ratio, get_release_group, media_type_of,
};
use crate::torrent::cache::get_cached_torrent;
use crate::torrent::{Metafile, SnatchError, SnatchOptions, snatch};
use crate::torznab::Candidate;
use crate::utils::{capture_group, extract_int, now_ms, strip_extension};

#[derive(Debug, Clone)]
pub struct ResultAssessment {
    pub decision: Decision,
    pub metafile: Option<Metafile>,
    /// Whether the metafile is present in the on-disk torrent cache, which
    /// gates writing a `decision` row (a row without its torrent is useless).
    pub meta_cached: bool,
}

impl ResultAssessment {
    fn just(decision: Decision) -> Self {
        ResultAssessment {
            decision,
            metafile: None,
            meta_cached: false,
        }
    }
}

// ─── File-tree comparisons ──────────────────────────────────────────────────

/// Every candidate file has a searchee counterpart with the same length *and*
/// path — a perfect cross-seed, 100% progress with no rechecking.
///
/// Virtual (ensemble) searchees carry absolute paths assembled from several
/// torrents, so for those the comparison falls back to file *names*.
pub fn compare_file_trees(candidate: &Metafile, searchee: &Searchee) -> bool {
    let by_name = searchee.is_virtual();
    candidate.files.iter().all(|candidate_file| {
        searchee.files.iter().any(|searchee_file| {
            searchee_file.length == candidate_file.length
                && if by_name {
                    searchee_file.name == candidate_file.name
                } else {
                    searchee_file.path == candidate_file.path
                }
        })
    })
}

/// Same lengths, different layout — the client will show 100% but the files
/// need linking into the candidate's structure.
///
/// Matching consumes searchee files so two candidate files of equal length
/// cannot both claim the same one.
pub fn compare_file_trees_ignoring_names(candidate: &Metafile, searchee: &Searchee) -> bool {
    let mut available: Vec<&File> = searchee.files.iter().collect();
    for candidate_file in &candidate.files {
        let Some(index) = pick_matching_file(&available, candidate_file) else {
            return false;
        };
        available.remove(index);
    }
    true
}

/// Index into `available` of the file that should satisfy `candidate_file`.
/// When several have the same length, prefer one whose name also matches.
fn pick_matching_file(available: &[&File], candidate_file: &File) -> Option<usize> {
    let same_length: Vec<usize> = available
        .iter()
        .enumerate()
        .filter(|(_, f)| f.length == candidate_file.length)
        .map(|(i, _)| i)
        .collect();
    match same_length.len() {
        0 => None,
        1 => Some(same_length[0]),
        _ => {
            let by_name: Vec<usize> = same_length
                .iter()
                .copied()
                .filter(|i| available[*i].name == candidate_file.name)
                .collect();
            by_name
                .first()
                .copied()
                .or_else(|| same_length.first().copied())
        }
    }
}

/// Fraction of the candidate's bytes the searchee can already supply.
pub fn get_partial_size_ratio(candidate: &Metafile, searchee: &Searchee) -> f64 {
    if candidate.length == 0 {
        return 0.0;
    }
    let matched: i64 = candidate
        .files
        .iter()
        .filter(|candidate_file| {
            searchee
                .files
                .iter()
                .any(|searchee_file| searchee_file.length == candidate_file.length)
        })
        .map(|f| f.length)
        .sum();
    matched as f64 / candidate.length as f64
}

/// Whether enough *whole pieces* are available for a partial match to be worth
/// injecting. Counting pieces rather than bytes is what makes the resulting
/// progress figure meaningful to the client.
pub fn compare_file_trees_partial(candidate: &Metafile, searchee: &Searchee) -> bool {
    let mut available: Vec<&File> = searchee.files.iter().collect();
    let mut matched_sizes: i64 = 0;
    for candidate_file in &candidate.files {
        if let Some(index) = pick_matching_file(&available, candidate_file) {
            matched_sizes += candidate_file.length;
            available.remove(index);
        }
    }
    if candidate.piece_length <= 0 {
        return false;
    }
    let total_pieces = (candidate.length as f64 / candidate.piece_length as f64).ceil();
    let available_pieces = (matched_sizes as f64 / candidate.piece_length as f64).floor();
    if total_pieces == 0.0 {
        return false;
    }
    available_pieces / total_pieces >= get_min_size_ratio(searchee)
}

fn fuzzy_size_does_match(result_size: i64, searchee: &Searchee) -> bool {
    let factor = get_fuzzy_size_factor(searchee);
    let length = searchee.length as f64;
    let lower = length - factor * length;
    let upper = length + factor * length;
    let size = result_size as f64;
    size >= lower && size <= upper
}

/// Resolutions must agree when *both* names declare one; a missing resolution
/// is not evidence of a mismatch.
fn resolution_does_match(searchee_title: &str, candidate_name: &str) -> bool {
    let of = |s: &str| capture_group(&RES_STRICT_REGEX, s, "res").map(|r| r.trim().to_lowercase());
    match (of(searchee_title), of(candidate_name)) {
        (Some(a), Some(b)) => extract_int(&a) == extract_int(&b),
        _ => true,
    }
}

/// Release groups must agree when both names declare one.
///
/// Anime naming regularly produces spurious `-GROUP` parses, so the bracketed
/// `[Group]` form is consulted before declaring a mismatch.
fn release_group_does_match(searchee_title: &str, candidate_name: &str) -> bool {
    let searchee_group =
        get_release_group(&strip_extension(searchee_title)).map(|g| g.to_lowercase());
    let candidate_group =
        get_release_group(&strip_extension(candidate_name)).map(|g| g.to_lowercase());

    let (Some(searchee_group), Some(candidate_group)) = (&searchee_group, &candidate_group) else {
        return true; // Pass if either is missing a -GRP
    };
    if searchee_group.starts_with(candidate_group.as_str())
        || candidate_group.starts_with(searchee_group.as_str())
    {
        return true;
    }

    let anime_group =
        |s: &str| capture_group(&ANIME_GROUP_REGEX, s, "group").map(|g| g.trim().to_lowercase());
    let searchee_anime = anime_group(searchee_title);
    let candidate_anime = anime_group(candidate_name);
    if searchee_anime.is_none() && candidate_anime.is_none() {
        return false;
    }

    // Rare edge cases: one side's bracketed group matching the other's suffix.
    if searchee_anime.is_some() && searchee_anime == candidate_anime {
        return true;
    }
    if searchee_anime.as_deref() == Some(candidate_group.as_str()) {
        return true;
    }
    if candidate_anime.as_deref() == Some(searchee_group.as_str()) {
        return true;
    }
    false
}

fn source_does_match(searchee_title: &str, candidate_name: &str) -> bool {
    match (parse_source(searchee_title), parse_source(candidate_name)) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// A REPACK and its original are different releases; both sides must agree on
/// whether they are one.
fn release_version_does_match(searchee_name: &str, candidate_name: &str) -> bool {
    REPACK_PROPER_REGEX.is_match(searchee_name).unwrap_or(false)
        == REPACK_PROPER_REGEX
            .is_match(candidate_name)
            .unwrap_or(false)
}

// ─── Assessment ─────────────────────────────────────────────────────────────

/// Either an already-downloaded torrent or a candidate that still needs one.
pub enum MetaOrCandidate<'a> {
    Meta(&'a Metafile),
    Candidate(&'a Candidate),
}

/// `assessCandidate`.
///
/// The pre-download gates (release group, resolution, source, repack, fuzzy
/// size, download link) only apply to a candidate; a `Metafile` in hand has
/// already been paid for, so it goes straight to the file-tree comparison.
pub async fn assess_candidate(
    meta_or_candidate: MetaOrCandidate<'_>,
    searchee: &Searchee,
    info_hashes_to_exclude: &std::collections::HashSet<String>,
    config: &RuntimeConfig,
) -> ResultAssessment {
    let label = searchee
        .label
        .unwrap_or(crate::searchee::SearcheeLabel::Search);

    let (metafile, mut meta_cached) = match meta_or_candidate {
        MetaOrCandidate::Meta(meta) => (meta.clone(), true),
        MetaOrCandidate::Candidate(candidate) => {
            let name = &candidate.name;
            if !release_group_does_match(&searchee.title, name) {
                return ResultAssessment::just(Decision::ReleaseGroupMismatch);
            }
            if !resolution_does_match(&searchee.title, name) {
                return ResultAssessment::just(Decision::ResolutionMismatch);
            }
            if !source_does_match(&searchee.title, name) {
                return ResultAssessment::just(Decision::SourceMismatch);
            }
            if !release_version_does_match(&searchee.title, name) {
                return ResultAssessment::just(Decision::ProperRepackMismatch);
            }
            if candidate.size > 0 && !fuzzy_size_does_match(candidate.size, searchee) {
                return ResultAssessment::just(Decision::FuzzySizeMismatch);
            }
            if candidate.link.is_empty() {
                return ResultAssessment::just(Decision::NoDownloadLink);
            }
            if find_blocked_string_in_release_maybe(searchee, &config.block_list).is_some() {
                return ResultAssessment::just(Decision::BlockedRelease);
            }

            // Announce candidates are time-sensitive but retried more slowly:
            // a tracker that just published a torrent may not serve it yet.
            let delay_ms = if label == crate::searchee::SearcheeLabel::Announce {
                5 * 60 * 1000
            } else {
                60 * 1000
            };
            match snatch(
                candidate,
                label,
                SnatchOptions {
                    retries: 4,
                    delay_ms,
                },
            )
            .await
            {
                Err(SnatchError::MagnetLink) => {
                    return ResultAssessment::just(Decision::MagnetLink);
                }
                Err(SnatchError::RateLimited) => {
                    return ResultAssessment::just(Decision::RateLimited);
                }
                Err(_) => return ResultAssessment::just(Decision::DownloadFailed),
                Ok(metafile) => (metafile, false),
            }
        }
    };

    if let MetaOrCandidate::Candidate(_) = meta_or_candidate {
        meta_cached = crate::torrent::cache::write_cached_torrent(&metafile)
            .await
            .is_ok();
    } else if find_blocked_string_in_release_maybe(searchee, &config.block_list).is_some() {
        return ResultAssessment {
            decision: Decision::BlockedRelease,
            metafile: Some(metafile),
            meta_cached,
        };
    }

    let with_meta = |decision: Decision| ResultAssessment {
        decision,
        metafile: Some(metafile.clone()),
        meta_cached,
    };

    if searchee.info_hash.as_deref() == Some(metafile.info_hash.as_str()) {
        return with_meta(Decision::SameInfoHash);
    }
    if info_hashes_to_exclude.contains(&metafile.info_hash) {
        return with_meta(Decision::InfoHashAlreadyExists);
    }

    let meta_searchee = Searchee {
        info_hash: Some(metafile.info_hash.clone()),
        name: metafile.name.clone(),
        title: metafile.title.clone(),
        files: metafile.files.clone(),
        length: metafile.length,
        category: metafile.category.clone(),
        tags: metafile.tags.clone(),
        trackers: Some(metafile.trackers.clone()),
        ..Default::default()
    };
    if find_blocked_string_in_release_maybe(&meta_searchee, &config.block_list).is_some() {
        return with_meta(Decision::BlockedRelease);
    }

    // A single episode must not be injected as a match for a season pack the
    // user already has.
    if !config.include_single_episodes
        && SEASON_REGEX.is_match(&searchee.title).unwrap_or(false)
        && is_single_episode(
            &metafile.files,
            media_type_of(&metafile.title, &metafile.files),
        )
    {
        return with_meta(Decision::FileTreeMismatch);
    }

    if compare_file_trees(&metafile, searchee) {
        return with_meta(Decision::Match);
    }

    let size_match = compare_file_trees_ignoring_names(&metafile, searchee);
    if size_match && config.match_mode != MatchMode::Strict {
        return with_meta(Decision::MatchSizeOnly);
    }

    if config.match_mode == MatchMode::Partial {
        if get_partial_size_ratio(&metafile, searchee) < get_min_size_ratio(searchee) {
            return with_meta(Decision::PartialSizeMismatch);
        }
        if compare_file_trees_partial(&metafile, searchee) {
            return with_meta(Decision::MatchPartial);
        }
    } else if !size_match {
        return with_meta(Decision::SizeMismatch);
    }

    with_meta(Decision::FileTreeMismatch)
}

// ─── guid → infoHash map ────────────────────────────────────────────────────

/// Some trackers give the same torrent several guids (one per alternative
/// title). Mapping guid to info hash lets a repeat encounter reuse the cached
/// torrent instead of snatching again.
static GUID_INFO_HASH_MAP: LazyLock<Arc<Mutex<HashMap<String, String>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

pub async fn rebuild_guid_info_hash_map(pool: &SqlitePool) -> sqlx::Result<()> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT guid, info_hash FROM decision WHERE info_hash IS NOT NULL")
            .fetch_all(pool)
            .await?;
    let mut map = GUID_INFO_HASH_MAP.lock().await;
    map.clear();
    for (guid, info_hash) in rows {
        map.insert(guid, info_hash);
    }
    Ok(())
}

pub async fn get_guid_info_hash_map(pool: &SqlitePool) -> HashMap<String, String> {
    if GUID_INFO_HASH_MAP.lock().await.is_empty() {
        let _ = rebuild_guid_info_hash_map(pool).await;
    }
    GUID_INFO_HASH_MAP.lock().await.clone()
}

pub async fn record_guid_info_hash(guid: &str, info_hash: &str) {
    GUID_INFO_HASH_MAP
        .lock()
        .await
        .insert(guid.to_string(), info_hash.to_string());
}

/// Looks up a previously seen torrent by guid or link.
///
/// The fallback exists for Gazelle-style trackers, where the same torrent is
/// reachable under several guids that differ only by group title but share the
/// numeric torrent id.
pub fn guid_lookup(
    guid: &str,
    link: &str,
    guid_info_hash_map: &HashMap<String, String>,
) -> Option<String> {
    if let Some(info_hash) = guid_info_hash_map
        .get(guid)
        .or_else(|| guid_info_hash_map.get(link))
    {
        return Some(info_hash.clone());
    }

    static TORRENT_ID: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\.tv/torrent/(\d+)/group").unwrap());
    for candidate in [guid, link] {
        let Some(torrent_id) = TORRENT_ID
            .captures(candidate)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
        else {
            continue;
        };
        for (key, value) in guid_info_hash_map {
            if TORRENT_ID
                .captures(key)
                .and_then(|c| c.get(1))
                .is_some_and(|m| m.as_str() == torrent_id)
            {
                return Some(value.clone());
            }
        }
    }
    None
}

/// The cached decision for a (searchee title, guid) pair.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CachedDecision {
    pub id: i64,
    pub info_hash: Option<String>,
    pub decision: Option<String>,
    pub first_seen: Option<i64>,
    pub fuzzy_size_factor: Option<f64>,
}

pub async fn get_cached_decision(
    pool: &SqlitePool,
    searchee_title: &str,
    guid: &str,
) -> sqlx::Result<Option<CachedDecision>> {
    sqlx::query_as(
        r#"
        SELECT decision.id, decision.info_hash, decision.decision,
               decision.first_seen, decision.fuzzy_size_factor
        FROM decision
        JOIN searchee ON decision.searchee_id = searchee.id
        WHERE searchee.name = ? AND decision.guid = ?
        "#,
    )
    .bind(searchee_title)
    .bind(guid)
    .fetch_optional(pool)
    .await
}

/// Writes (or refreshes) the decision row for a candidate.
pub async fn save_decision(
    pool: &SqlitePool,
    searchee_title: &str,
    guid: &str,
    info_hash: &str,
    decision: Decision,
    first_seen: i64,
    fuzzy_size_factor: f64,
) -> sqlx::Result<()> {
    let searchee_id = crate::db::upsert_searchee(pool, searchee_title).await?;
    sqlx::query(
        r#"
        INSERT INTO decision
            (searchee_id, guid, info_hash, decision, first_seen, last_seen, fuzzy_size_factor)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (searchee_id, guid) DO UPDATE SET
            info_hash = excluded.info_hash,
            decision = excluded.decision,
            last_seen = excluded.last_seen,
            fuzzy_size_factor = excluded.fuzzy_size_factor
        "#,
    )
    .bind(searchee_id)
    .bind(guid)
    .bind(info_hash)
    .bind(decision.as_str())
    .bind(first_seen)
    .bind(now_ms())
    .bind(fuzzy_size_factor)
    .execute(pool)
    .await?;
    record_guid_info_hash(guid, info_hash).await;
    Ok(())
}

/// `assessCandidateCaching` — the assessment path used by search and RSS.
///
/// Reuses a cached torrent when the guid is already known, and short-circuits
/// when the candidate's info hash is already in the client (preserving the
/// original match decision so statistics stay honest).
pub async fn assess_candidate_caching(
    pool: &SqlitePool,
    candidate: &mut Candidate,
    searchee: &Searchee,
    info_hashes_to_exclude: &std::collections::HashSet<String>,
    guid_info_hash_map: &HashMap<String, String>,
    config: &RuntimeConfig,
) -> ResultAssessment {
    let label = searchee
        .label
        .unwrap_or(crate::searchee::SearcheeLabel::Search);
    let cache_entry = get_cached_decision(pool, &searchee.title, &candidate.guid)
        .await
        .ok()
        .flatten();

    let cached_meta = match guid_lookup(&candidate.guid, &candidate.link, guid_info_hash_map) {
        Some(info_hash) => get_cached_torrent(&info_hash).await,
        None => None,
    };
    if let Some(meta) = &cached_meta {
        tracing::debug!(
            label = label.as_str(),
            "Using cached torrent for {} assessment {}: {}",
            candidate.tracker,
            candidate.name,
            meta.name
        );
        // Trackers frequently report a size that disagrees with the torrent.
        candidate.size = meta.length;
    }

    // Fast path: already injected. Keep whatever match decision was recorded so
    // the stats page does not lose the fact that it *was* a match.
    if let Some(entry) = &cache_entry
        && let Some(info_hash) = &entry.info_hash
        && info_hashes_to_exclude.contains(info_hash)
    {
        let preserved = entry
            .decision
            .as_deref()
            .and_then(Decision::from_str_exact)
            .filter(|d| d.is_any_match())
            .unwrap_or(Decision::InfoHashAlreadyExists);
        let _ = sqlx::query("UPDATE decision SET last_seen = ?, decision = ? WHERE id = ?")
            .bind(now_ms())
            .bind(preserved.as_str())
            .bind(entry.id)
            .execute(pool)
            .await;
        return ResultAssessment::just(Decision::InfoHashAlreadyExists);
    }

    let assessment = match &cached_meta {
        Some(meta) => {
            assess_candidate(
                MetaOrCandidate::Meta(meta),
                searchee,
                info_hashes_to_exclude,
                config,
            )
            .await
        }
        None => {
            assess_candidate(
                MetaOrCandidate::Candidate(candidate),
                searchee,
                info_hashes_to_exclude,
                config,
            )
            .await
        }
    };

    if assessment.meta_cached
        && let Some(metafile) = &assessment.metafile
    {
        let first_seen = cache_entry
            .as_ref()
            .and_then(|entry| entry.first_seen)
            .unwrap_or_else(now_ms);
        let _ = save_decision(
            pool,
            &searchee.title,
            &candidate.guid,
            &metafile.info_hash,
            assessment.decision,
            first_seen,
            get_fuzzy_size_factor(searchee),
        )
        .await;
    }

    tracing::debug!(
        label = Label::Decide.as_str(),
        "{} - {} for {} torrent {} - {}",
        searchee.title,
        if assessment.decision.is_any_match() {
            assessment.decision.as_str()
        } else {
            "no match"
        },
        candidate.tracker,
        candidate.name,
        assessment.decision.as_str()
    );

    assessment
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::config::runtime::set_runtime_config;
    use crate::db::test_pool;
    use crate::torrent::metafile::fixtures::multi_file_torrent;

    fn file(path: &str, length: i64) -> File {
        File {
            name: std::path::Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            path: path.to_string(),
            length,
        }
    }

    fn meta(name: &str, files: &[(&[&str], i64)]) -> Metafile {
        Metafile::decode(&multi_file_torrent(name, files)).unwrap()
    }

    fn searchee_from(name: &str, files: Vec<File>) -> Searchee {
        let length = files.iter().map(|f| f.length).sum();
        Searchee {
            info_hash: Some("searchee-hash".into()),
            name: name.to_string(),
            title: name.to_string(),
            files,
            length,
            ..Default::default()
        }
    }

    #[test]
    fn identical_trees_are_a_perfect_match() {
        set_runtime_config(default_runtime_config());
        let candidate = meta("Pack", &[(&["a.mkv"], 100), (&["b.mkv"], 200)]);
        let searchee = searchee_from(
            "Pack",
            vec![file("Pack/a.mkv", 100), file("Pack/b.mkv", 200)],
        );
        assert!(compare_file_trees(&candidate, &searchee));
    }

    #[test]
    fn a_renamed_file_is_only_a_size_match() {
        set_runtime_config(default_runtime_config());
        let candidate = meta("Pack", &[(&["a.mkv"], 100), (&["b.mkv"], 200)]);
        let searchee = searchee_from(
            "Other",
            vec![file("Other/x.mkv", 100), file("Other/y.mkv", 200)],
        );
        assert!(!compare_file_trees(&candidate, &searchee));
        assert!(compare_file_trees_ignoring_names(&candidate, &searchee));
    }

    /// Two candidate files of the same length must not both match one searchee
    /// file — that would report a complete match for half the data.
    #[test]
    fn equal_length_files_are_consumed_not_reused() {
        set_runtime_config(default_runtime_config());
        let candidate = meta("Pack", &[(&["a.mkv"], 100), (&["b.mkv"], 100)]);
        let searchee = searchee_from("Other", vec![file("Other/x.mkv", 100)]);
        assert!(!compare_file_trees_ignoring_names(&candidate, &searchee));
    }

    #[test]
    fn partial_size_ratio_counts_matched_bytes() {
        set_runtime_config(default_runtime_config());
        let candidate = meta("Pack", &[(&["a.mkv"], 100), (&["b.mkv"], 300)]);
        let searchee = searchee_from("Other", vec![file("Other/x.mkv", 100)]);
        assert!((get_partial_size_ratio(&candidate, &searchee) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn resolution_mismatch_is_only_reported_when_both_declare_one() {
        assert!(resolution_does_match(
            "Show.S01E01.1080p",
            "Show.S01E01.1080p"
        ));
        assert!(!resolution_does_match(
            "Show.S01E01.1080p",
            "Show.S01E01.720p"
        ));
        assert!(resolution_does_match("Show.S01E01", "Show.S01E01.720p"));
    }

    #[test]
    fn source_mismatch_is_only_reported_when_both_declare_one() {
        assert!(!source_does_match(
            "Show.S01E01.AMZN.WEB-DL",
            "Show.S01E01.NF.WEB-DL"
        ));
        assert!(source_does_match(
            "Show.S01E01.WEB-DL",
            "Show.S01E01.NF.WEB-DL"
        ));
    }

    #[test]
    fn repack_and_original_do_not_match() {
        assert!(!release_version_does_match(
            "Show.S01E01",
            "Show.S01E01.REPACK"
        ));
        assert!(release_version_does_match(
            "Show.S01E01.REPACK",
            "Show.S01E01.REPACK"
        ));
        assert!(release_version_does_match("Show.S01E01", "Show.S01E01"));
    }

    #[test]
    fn release_groups_match_by_prefix() {
        assert!(release_group_does_match(
            "Show.S01E01-GRP",
            "Show.S01E01-GRP"
        ));
        // Trackers truncate group names; a prefix relationship still counts.
        assert!(release_group_does_match(
            "Show.S01E01-GROUPNAME",
            "Show.S01E01-GROUP"
        ));
    }

    #[test]
    fn fuzzy_size_uses_the_configured_threshold() {
        let mut config = default_runtime_config();
        config.fuzzy_size_threshold = 0.02;
        set_runtime_config(config);
        let searchee = searchee_from("X", vec![file("X/a.mkv", 1000)]);
        assert!(fuzzy_size_does_match(1010, &searchee));
        assert!(!fuzzy_size_does_match(1100, &searchee));
    }

    /// In strict mode a same-sizes/different-names candidate is NOT reported as
    /// a size mismatch — the sizes did match — but as a file-tree mismatch, the
    /// decision whose message tells the user flexible mode would accept it.
    #[tokio::test]
    async fn strict_mode_rejects_a_size_only_match_as_a_tree_mismatch() {
        let mut config = default_runtime_config();
        config.match_mode = MatchMode::Strict;
        config.include_single_episodes = true;
        set_runtime_config(config.clone());

        let candidate = meta("Pack", &[(&["a.mkv"], 100), (&["b.mkv"], 200)]);
        let searchee = searchee_from(
            "Other",
            vec![file("Other/x.mkv", 100), file("Other/y.mkv", 200)],
        );
        let assessment = assess_candidate(
            MetaOrCandidate::Meta(&candidate),
            &searchee,
            &Default::default(),
            &config,
        )
        .await;
        assert_eq!(assessment.decision, Decision::FileTreeMismatch);
    }

    /// Genuinely different sizes outside partial mode are a size mismatch.
    #[tokio::test]
    async fn differing_sizes_are_a_size_mismatch() {
        let mut config = default_runtime_config();
        config.match_mode = MatchMode::Flexible;
        config.include_single_episodes = true;
        set_runtime_config(config.clone());

        let candidate = meta("Pack", &[(&["a.mkv"], 100), (&["b.mkv"], 200)]);
        let searchee = searchee_from("Other", vec![file("Other/x.mkv", 999)]);
        let assessment = assess_candidate(
            MetaOrCandidate::Meta(&candidate),
            &searchee,
            &Default::default(),
            &config,
        )
        .await;
        assert_eq!(assessment.decision, Decision::SizeMismatch);
    }

    #[tokio::test]
    async fn flexible_mode_accepts_a_size_only_match() {
        let mut config = default_runtime_config();
        config.match_mode = MatchMode::Flexible;
        config.include_single_episodes = true;
        set_runtime_config(config.clone());

        let candidate = meta("Pack", &[(&["a.mkv"], 100), (&["b.mkv"], 200)]);
        let searchee = searchee_from(
            "Other",
            vec![file("Other/x.mkv", 100), file("Other/y.mkv", 200)],
        );
        let assessment = assess_candidate(
            MetaOrCandidate::Meta(&candidate),
            &searchee,
            &Default::default(),
            &config,
        )
        .await;
        assert_eq!(assessment.decision, Decision::MatchSizeOnly);
    }

    #[tokio::test]
    async fn an_excluded_info_hash_short_circuits() {
        let config = default_runtime_config();
        set_runtime_config(config.clone());
        let candidate = meta("Pack", &[(&["a.mkv"], 100)]);
        let searchee = searchee_from("Pack", vec![file("Pack/a.mkv", 100)]);
        let mut exclude = std::collections::HashSet::new();
        exclude.insert(candidate.info_hash.clone());

        let assessment = assess_candidate(
            MetaOrCandidate::Meta(&candidate),
            &searchee,
            &exclude,
            &config,
        )
        .await;
        assert_eq!(assessment.decision, Decision::InfoHashAlreadyExists);
    }

    #[tokio::test]
    async fn the_same_info_hash_is_reported_separately() {
        let config = default_runtime_config();
        set_runtime_config(config.clone());
        let candidate = meta("Pack", &[(&["a.mkv"], 100)]);
        let searchee = Searchee {
            info_hash: Some(candidate.info_hash.clone()),
            ..searchee_from("Pack", vec![file("Pack/a.mkv", 100)])
        };
        let assessment = assess_candidate(
            MetaOrCandidate::Meta(&candidate),
            &searchee,
            &Default::default(),
            &config,
        )
        .await;
        assert_eq!(assessment.decision, Decision::SameInfoHash);
    }

    #[tokio::test]
    async fn decisions_round_trip_through_the_database() {
        let pool = test_pool().await;
        save_decision(
            &pool,
            "Some.Show.S01E01",
            "guid-1",
            "0123456789abcdef0123456789abcdef01234567",
            Decision::Match,
            1000,
            0.02,
        )
        .await
        .unwrap();

        let cached = get_cached_decision(&pool, "Some.Show.S01E01", "guid-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached.decision.as_deref(), Some("MATCH"));
        assert_eq!(cached.first_seen, Some(1000));

        // A second save must update in place, not violate the unique constraint.
        save_decision(
            &pool,
            "Some.Show.S01E01",
            "guid-1",
            "0123456789abcdef0123456789abcdef01234567",
            Decision::InfoHashAlreadyExists,
            1000,
            0.02,
        )
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decision")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn guid_lookup_falls_back_to_the_gazelle_torrent_id() {
        let mut map = HashMap::new();
        map.insert(
            "https://x.tv/torrent/12345/group".to_string(),
            "hash-a".to_string(),
        );

        assert_eq!(
            guid_lookup("https://x.tv/torrent/12345/group", "", &map).as_deref(),
            Some("hash-a")
        );
        // Different guid, same torrent id.
        assert_eq!(
            guid_lookup("https://x.tv/torrent/12345/group?alt=1", "", &map).as_deref(),
            Some("hash-a")
        );
        assert_eq!(
            guid_lookup("https://x.tv/torrent/999/group", "", &map),
            None
        );
    }
}
