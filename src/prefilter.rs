//! Pre-search filtering: deciding whether a searchee is worth searching at all.
//!
//! Ported from `preFilter.ts`. Two independent gates:
//!
//! * [`filter_by_content`] — properties of the release itself (blocklist,
//!   single episodes, non-video ratio, arr library directories);
//! * [`filter_timestamps`] — search history, honouring `excludeOlder` and
//!   `excludeRecentSearch`.

use std::path::Path;

use sqlx::SqlitePool;

use crate::config::RuntimeConfig;
use crate::constants::{
    ARR_DIR_REGEX, BAD_EP_REGEX, BAD_SEASON_REGEX, BlocklistType, MediaType, SEASON_REGEX,
    SONARR_SUBFOLDERS_REGEX, TORRENT_CATEGORY_SUFFIX, TORRENT_TAG, VIDEO_DISC_EXTENSIONS,
    VIDEO_EXTENSIONS, parse_blocklist_entry,
};
use crate::logger::Label;
use crate::searchee::{File, Searchee, SearcheeLabel, files_with_ext, has_ext};
use crate::utils::{capture_group, extract_int, human_readable_date, n_ms_ago};

/// `Number.MAX_SAFE_INTEGER` — used as the "never searched" sentinel in the
/// timestamp aggregate, so it has to be the same value the original used.
const MAX_INT: i64 = 9_007_199_254_740_991;

pub fn log_filter_reason(reason: &str, searchee: &Searchee, media_type: MediaType) {
    let label = searchee.label.unwrap_or(SearcheeLabel::Search);
    let message = format!(
        "{} | MediaType: {} - {reason}",
        searchee.title,
        media_type.as_str().to_uppercase()
    );
    // Webhook searches are user-initiated, so their rejections are visible at
    // info level rather than buried in verbose logs.
    if label == SearcheeLabel::Webhook {
        tracing::info!(
            label = Label::Webhook.as_str(),
            "Did not search for {message}"
        );
    } else {
        tracing::debug!(label = Label::PreFilter.as_str(), "{message}");
    }
}

/// Whether the searchee represents one episode. Anime is judged by file count
/// because its naming rarely encodes a season.
pub fn is_single_episode(files: &[File], media_type: MediaType) -> bool {
    match media_type {
        MediaType::Episode => true,
        MediaType::Anime => files_with_ext(files, VIDEO_EXTENSIONS).len() == 1,
        _ => false,
    }
}

/// Detects torrents crust-seed itself injected, by the category/tag it stamps
/// on them. Each client stores that marker in a different place, hence the
/// several checks.
fn is_cross_seed(searchee: &Searchee, config: &RuntimeConfig) -> bool {
    if let Some(link_category) = config.link_category.as_deref()
        && !link_category.is_empty()
        && searchee.category.as_deref() == Some(link_category)
    {
        return true; // qBittorrent, Deluge
    }
    if searchee.category.as_deref() == Some(TORRENT_TAG) {
        return true; // Deluge
    }
    if searchee
        .category
        .as_deref()
        .is_some_and(|c| c.ends_with(TORRENT_CATEGORY_SUFFIX))
    {
        return true; // qBittorrent, Deluge
    }
    if let Some(tags) = &searchee.tags {
        if tags.iter().any(|tag| tag == TORRENT_TAG) {
            return true; // qBittorrent, rTorrent, Transmission
        }
        if tags
            .iter()
            .any(|tag| tag.ends_with(TORRENT_CATEGORY_SUFFIX))
        {
            return true; // qBittorrent
        }
    }
    false
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ContentFilterOptions {
    pub allow_season_pack_episodes: bool,
    pub ignore_cross_seeds: bool,
    /// Skip every check except the blocklist — used by the inject path, which
    /// must not re-apply search-time heuristics.
    pub block_list_only: bool,
}

/// `filterByContent`.
pub async fn filter_by_content(
    searchee: &Searchee,
    config: &RuntimeConfig,
    options: ContentFilterOptions,
) -> bool {
    let media_type = searchee.media_type();

    if let Some(blocked) = find_blocked_string_in_release_maybe(searchee, &config.block_list) {
        log_filter_reason(
            &format!("it matches the blocklist: {blocked}"),
            searchee,
            media_type,
        );
        return false;
    }
    if options.block_list_only {
        return true;
    }

    let label = searchee.label.unwrap_or(SearcheeLabel::Search);
    let user_facing = matches!(label, SearcheeLabel::Search | SearcheeLabel::Webhook);

    // A lone file inside a "Season 03" directory is one episode of a pack the
    // user already has; searching for it would snatch a duplicate.
    if (!options.allow_season_pack_episodes || !config.include_single_episodes)
        && searchee.files.len() == 1
        && user_facing
        && let Some(path) = &searchee.path
    {
        let parent = Path::new(path)
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if SEASON_REGEX.is_match(&parent).unwrap_or(false)
            || SONARR_SUBFOLDERS_REGEX.is_match(&parent).unwrap_or(false)
        {
            log_filter_reason("it is a season pack episode", searchee, media_type);
            return false;
        }
    }

    if !config.include_single_episodes
        && label != SearcheeLabel::Announce
        && is_single_episode(&searchee.files, media_type)
    {
        log_filter_reason("it is a single episode", searchee, media_type);
        return false;
    }

    let non_video_size_ratio = non_video_size_ratio(searchee);
    if !config.include_non_videos && non_video_size_ratio > config.fuzzy_size_threshold {
        log_filter_reason(
            &format!(
                "nonVideoSizeRatio {non_video_size_ratio:.3} > {} fuzzySizeThreshold",
                config.fuzzy_size_threshold
            ),
            searchee,
            media_type,
        );
        return false;
    }

    if options.ignore_cross_seeds && is_cross_seed(searchee, config) {
        log_filter_reason("it is a cross seed", searchee, media_type);
        return false;
    }

    if let Some(path) = &searchee.path {
        let is_dir = tokio::fs::metadata(path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false);
        let base = Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // `/media/movies/Some Movie (2020)/` is a library folder, not a
        // release: searching for it would match nothing useful.
        if is_dir
            && ARR_DIR_REGEX.is_match(&base).unwrap_or(false)
            && non_video_size_ratio < 0.02
            && !matches!(media_type, MediaType::Episode | MediaType::Season)
            && !(searchee.files.len() > 1
                && SONARR_SUBFOLDERS_REGEX.is_match(&base).unwrap_or(false))
        {
            log_filter_reason(
                "it looks like an arr movie/series directory",
                searchee,
                media_type,
            );
            return false;
        }

        if media_type == MediaType::Season
            && capture_group(&SEASON_REGEX, &searchee.title, "season")
                .and_then(|season| extract_int(&season))
                == Some(0)
        {
            log_filter_reason("it is a Specials folder", searchee, media_type);
            return false;
        }
    }

    // A title whose season/episode token is not at the start usually means the
    // parse latched onto the wrong thing.
    if user_facing
        && (BAD_EP_REGEX.is_match(&searchee.title).unwrap_or(false)
            || BAD_SEASON_REGEX.is_match(&searchee.title).unwrap_or(false))
    {
        log_filter_reason(
            "it has a non-standard episode/season naming format",
            searchee,
            media_type,
        );
        return false;
    }

    true
}

fn non_video_size_ratio(searchee: &Searchee) -> f64 {
    if searchee.length == 0 {
        return 0.0;
    }
    let video_exts: Vec<&str> = VIDEO_EXTENSIONS
        .iter()
        .chain(VIDEO_DISC_EXTENSIONS)
        .copied()
        .collect();
    let non_video: i64 = searchee
        .files
        .iter()
        .filter(|file| !has_ext(std::slice::from_ref(*file), &video_exts))
        .map(|file| file.length)
        .sum();
    non_video as f64 / searchee.length as f64
}

/// `findBlockedStringInReleaseMaybe` — returns the blocklist entry that matched.
pub fn find_blocked_string_in_release_maybe(
    searchee: &Searchee,
    block_list: &[String],
) -> Option<String> {
    block_list
        .iter()
        .find(|entry| blocklist_entry_matches(searchee, entry))
        .cloned()
}

fn parent_dir(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn blocklist_entry_matches(searchee: &Searchee, entry: &str) -> bool {
    let (blocklist_type, value) = parse_blocklist_entry(entry);
    match blocklist_type {
        BlocklistType::Name => searchee.title.contains(&value),
        BlocklistType::NameRegex => regex::Regex::new(&value)
            .map(|re| re.is_match(&searchee.title))
            .unwrap_or(false),
        BlocklistType::Folder => searchee
            .path
            .as_deref()
            .is_some_and(|path| parent_dir(path).contains(&value)),
        BlocklistType::FolderRegex => searchee.path.as_deref().is_some_and(|path| {
            regex::Regex::new(&value)
                .map(|re| re.is_match(&parent_dir(path)))
                .unwrap_or(false)
        }),
        BlocklistType::Category => searchee.category.as_deref() == Some(value.as_str()),
        BlocklistType::Tag => match &searchee.tags {
            // No tags at all means the searchee is data-based or a snatched
            // metafile, which the tag blocklist does not apply to.
            None => false,
            // An empty value means "block anything with no tags".
            Some(tags) if value.is_empty() => tags.is_empty(),
            Some(tags) => tags.contains(&value),
        },
        BlocklistType::Tracker => searchee
            .trackers
            .as_ref()
            .is_some_and(|trackers| trackers.contains(&value)),
        BlocklistType::InfoHash => searchee.info_hash.as_deref() == Some(value.as_str()),
        BlocklistType::SizeBelow => value
            .parse::<i64>()
            .map(|limit| searchee.length < limit)
            .unwrap_or(false),
        BlocklistType::SizeAbove => value
            .parse::<i64>()
            .map(|limit| searchee.length > limit)
            .unwrap_or(false),
        // A bare entry with no `type:` prefix matches a name substring, an
        // exact info hash, or a parent-directory substring.
        BlocklistType::Legacy => {
            searchee.title.contains(entry)
                || searchee.info_hash.as_deref() == Some(entry)
                || searchee
                    .path
                    .as_deref()
                    .is_some_and(|path| parent_dir(path).contains(entry))
        }
    }
}

/// Collapses searchees that would produce identical searches — typically the
/// same release at several resolutions in the same client.
///
/// Torrent-backed searchees sort first so a real torrent survives over a
/// data-directory duplicate.
pub fn filter_dupes_from_similar(searchees: &[Searchee]) -> Vec<Searchee> {
    let mut ordered: Vec<&Searchee> = searchees.iter().collect();
    ordered.sort_by_key(|s| s.info_hash.is_none());

    let mut filtered: Vec<Searchee> = Vec::new();
    for searchee in ordered {
        let is_dupe = filtered.iter().any(|other| {
            if searchee.title != other.title
                || searchee.length != other.length
                || searchee.files.len() != other.files.len()
                || searchee.client_host != other.client_host
            {
                return false;
            }
            // Same multiset of file lengths => the same content.
            let mut potential: Vec<i64> = other.files.iter().map(|f| f.length).collect();
            searchee.files.iter().all(|file| {
                match potential.iter().position(|len| *len == file.length) {
                    Some(index) => {
                        potential.remove(index);
                        true
                    }
                    None => false,
                }
            })
        });
        if !is_dupe {
            filtered.push(searchee.clone());
        }
    }
    filtered
}

/// The aggregate `filterTimestamps` computes over an outer join of every
/// enabled indexer against this searchee's search history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
pub struct TimestampAggregate {
    pub earliest_first_search: i64,
    pub latest_first_search: i64,
    pub earliest_last_search: i64,
}

pub async fn fetch_timestamp_aggregate(
    pool: &SqlitePool,
    searchee_title: &str,
    indexer_ids: &[i64],
) -> sqlx::Result<TimestampAggregate> {
    if indexer_ids.is_empty() {
        return Ok(TimestampAggregate {
            earliest_first_search: MAX_INT,
            latest_first_search: MAX_INT,
            earliest_last_search: 0,
        });
    }
    // CROSS JOIN indexer so an indexer with no timestamp row still contributes
    // the MAX_INT / 0 sentinels — that is what makes "a new indexer was added"
    // detectable below.
    let sql = format!(
        r#"
        SELECT
            COALESCE(MIN(COALESCE(timestamp.first_searched, {MAX_INT})), {MAX_INT}) AS earliest_first_search,
            COALESCE(MAX(COALESCE(timestamp.first_searched, {MAX_INT})), {MAX_INT}) AS latest_first_search,
            COALESCE(MIN(COALESCE(timestamp.last_searched, 0)), 0)                  AS earliest_last_search
        FROM searchee
        CROSS JOIN indexer
        LEFT OUTER JOIN timestamp
            ON timestamp.indexer_id = indexer.id
           AND timestamp.searchee_id = searchee.id
        WHERE searchee.name = ?
          AND indexer.id IN ({})
        "#,
        crate::db::placeholders(indexer_ids.len())
    );
    let mut query = sqlx::query_as::<_, TimestampAggregate>(&sql).bind(searchee_title);
    for id in indexer_ids {
        query = query.bind(id);
    }
    query.fetch_one(pool).await
}

/// Applies `excludeOlder` / `excludeRecentSearch` to a timestamp aggregate.
///
/// Returns `true` when the searchee should still be searched.
pub fn timestamps_allow_search(
    aggregate: TimestampAggregate,
    config: &RuntimeConfig,
    searchee: &Searchee,
    newest_file_age: Option<f64>,
) -> bool {
    let media_type = searchee.media_type();

    // A virtual season keeps being searched while its episodes are still
    // arriving, regardless of when it was last searched.
    if config.season_from_episodes_ratio().is_some()
        && searchee.is_virtual()
        && let Some(newest_file_age) = newest_file_age
        && (aggregate.earliest_last_search as f64) < newest_file_age
    {
        return true;
    }

    if let Some(exclude_older) = config.exclude_older {
        let skip_before = n_ms_ago(exclude_older);
        // latest_first_search == MAX_INT means at least one enabled indexer has
        // never searched this title — usually a newly added indexer, which
        // must not be blocked by an old first-search timestamp.
        if aggregate.latest_first_search != MAX_INT
            && aggregate.earliest_first_search != 0
            && aggregate.earliest_first_search < skip_before
        {
            log_filter_reason(
                &format!(
                    "its first search timestamp {} is older than {} ago",
                    human_readable_date(aggregate.earliest_first_search),
                    crate::config::duration::format_ms_long(exclude_older)
                ),
                searchee,
                media_type,
            );
            return false;
        }
    }

    if let Some(exclude_recent_search) = config.exclude_recent_search {
        let skip_after = n_ms_ago(exclude_recent_search);
        if aggregate.earliest_last_search != 0 && aggregate.earliest_last_search > skip_after {
            log_filter_reason(
                &format!(
                    "its last search timestamp {} is newer than {} ago",
                    human_readable_date(aggregate.earliest_last_search),
                    crate::config::duration::format_ms_long(exclude_recent_search)
                ),
                searchee,
                media_type,
            );
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::db::test_pool;

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

    fn searchee(title: &str, files: Vec<File>) -> Searchee {
        let length = files.iter().map(|f| f.length).sum();
        Searchee {
            name: title.to_string(),
            title: title.to_string(),
            files,
            length,
            label: Some(SearcheeLabel::Search),
            ..Default::default()
        }
    }

    #[test]
    fn blocklist_name_and_regex() {
        let s = searchee("Some.Show.S01E01", vec![file("a.mkv", 10)]);
        assert_eq!(
            find_blocked_string_in_release_maybe(&s, &["name:Some.Show".into()]).as_deref(),
            Some("name:Some.Show")
        );
        assert_eq!(
            find_blocked_string_in_release_maybe(&s, &[r"nameRegex:^Some\..*S01".into()])
                .as_deref(),
            Some(r"nameRegex:^Some\..*S01")
        );
        assert_eq!(
            find_blocked_string_in_release_maybe(&s, &["name:Nope".into()]),
            None
        );
    }

    #[test]
    fn blocklist_size_bounds() {
        let s = searchee("X", vec![file("a.mkv", 500)]);
        assert!(find_blocked_string_in_release_maybe(&s, &["sizeBelow:1000".into()]).is_some());
        assert!(find_blocked_string_in_release_maybe(&s, &["sizeAbove:1000".into()]).is_none());
        assert!(find_blocked_string_in_release_maybe(&s, &["sizeAbove:100".into()]).is_some());
    }

    /// An empty `tag:` value means "block untagged torrents"; a searchee with
    /// no tag information at all (data dir, snatched metafile) is exempt.
    #[test]
    fn blocklist_tag_semantics() {
        let mut s = searchee("X", vec![file("a.mkv", 1)]);
        assert!(find_blocked_string_in_release_maybe(&s, &["tag:".into()]).is_none());

        s.tags = Some(vec![]);
        assert!(find_blocked_string_in_release_maybe(&s, &["tag:".into()]).is_some());

        s.tags = Some(vec!["keep".into()]);
        assert!(find_blocked_string_in_release_maybe(&s, &["tag:".into()]).is_none());
        assert!(find_blocked_string_in_release_maybe(&s, &["tag:keep".into()]).is_some());
    }

    #[test]
    fn legacy_blocklist_entries_match_name_infohash_or_folder() {
        let mut s = searchee("Some.Show.S01E01", vec![file("a.mkv", 1)]);
        assert!(find_blocked_string_in_release_maybe(&s, &["Some.Show".into()]).is_some());

        s.info_hash = Some("abc".into());
        assert!(find_blocked_string_in_release_maybe(&s, &["abc".into()]).is_some());

        s.path = Some("/data/blocked-dir/Some.Show.S01E01".into());
        assert!(find_blocked_string_in_release_maybe(&s, &["blocked-dir".into()]).is_some());
    }

    #[test]
    fn single_episode_detection() {
        assert!(is_single_episode(&[], MediaType::Episode));
        assert!(!is_single_episode(&[], MediaType::Season));
        assert!(is_single_episode(&[file("a.mkv", 1)], MediaType::Anime));
        assert!(!is_single_episode(
            &[file("a.mkv", 1), file("b.mkv", 1)],
            MediaType::Anime
        ));
    }

    #[tokio::test]
    async fn non_video_heavy_releases_are_filtered() {
        let mut config = default_runtime_config();
        config.include_single_episodes = true;
        let s = searchee(
            "Some.Pack",
            vec![file("movie.mkv", 10), file("extras.zip", 990)],
        );
        assert!(!filter_by_content(&s, &config, ContentFilterOptions::default()).await);

        config.include_non_videos = true;
        assert!(filter_by_content(&s, &config, ContentFilterOptions::default()).await);
    }

    #[tokio::test]
    async fn block_list_only_skips_the_other_checks() {
        let config = default_runtime_config();
        let s = searchee("Some.Show.S01E01", vec![file("a.mkv", 10)]);
        // Would normally be rejected as a single episode.
        assert!(!filter_by_content(&s, &config, ContentFilterOptions::default()).await);
        assert!(
            filter_by_content(
                &s,
                &config,
                ContentFilterOptions {
                    block_list_only: true,
                    ..Default::default()
                }
            )
            .await
        );
    }

    #[test]
    fn duplicate_searchees_collapse_keeping_the_torrent_backed_one() {
        let data = Searchee {
            path: Some("/data/Some.Show.S01".into()),
            ..searchee("Some.Show.S01", vec![file("a.mkv", 10), file("b.mkv", 20)])
        };
        let torrent = Searchee {
            info_hash: Some("abc".into()),
            ..searchee("Some.Show.S01", vec![file("b.mkv", 20), file("a.mkv", 10)])
        };
        let filtered = filter_dupes_from_similar(&[data, torrent]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].info_hash.as_deref(), Some("abc"));
    }

    #[test]
    fn different_file_sizes_are_not_duplicates() {
        let a = searchee("Some.Show.S01", vec![file("a.mkv", 10)]);
        let b = searchee("Some.Show.S01", vec![file("a.mkv", 11)]);
        assert_eq!(filter_dupes_from_similar(&[a, b]).len(), 2);
    }

    #[test]
    fn exclude_recent_search_blocks_a_just_searched_title() {
        let config = default_runtime_config();
        let s = searchee("Some.Show.S01", vec![file("a.mkv", 1)]);
        let aggregate = TimestampAggregate {
            earliest_first_search: n_ms_ago(1000),
            latest_first_search: n_ms_ago(1000),
            earliest_last_search: n_ms_ago(1000),
        };
        assert!(!timestamps_allow_search(aggregate, &config, &s, None));
    }

    /// A newly added indexer (no timestamp row, so latest == MAX_INT) must not
    /// be blocked by excludeOlder.
    #[test]
    fn a_new_indexer_defeats_exclude_older() {
        let mut config = default_runtime_config();
        config.exclude_recent_search = None;
        let s = searchee("Some.Show.S01", vec![file("a.mkv", 1)]);

        let old = n_ms_ago(config.exclude_older.unwrap() * 2);
        let all_searched = TimestampAggregate {
            earliest_first_search: old,
            latest_first_search: old,
            earliest_last_search: old,
        };
        assert!(!timestamps_allow_search(all_searched, &config, &s, None));

        let one_new = TimestampAggregate {
            latest_first_search: MAX_INT,
            ..all_searched
        };
        assert!(timestamps_allow_search(one_new, &config, &s, None));
    }

    #[tokio::test]
    async fn timestamp_aggregate_uses_sentinels_for_unsearched_indexers() {
        let pool = test_pool().await;
        let searchee_id = crate::db::upsert_searchee(&pool, "Some.Show.S01")
            .await
            .unwrap();
        let indexer_id: i64 =
            sqlx::query("INSERT INTO indexer (url, apikey) VALUES ('https://a/api', 'k')")
                .execute(&pool)
                .await
                .unwrap()
                .last_insert_rowid();
        let other_id: i64 =
            sqlx::query("INSERT INTO indexer (url, apikey) VALUES ('https://b/api', 'k')")
                .execute(&pool)
                .await
                .unwrap()
                .last_insert_rowid();

        sqlx::query(
            "INSERT INTO timestamp (searchee_id, indexer_id, first_searched, last_searched) VALUES (?, ?, 100, 200)",
        )
        .bind(searchee_id)
        .bind(indexer_id)
        .execute(&pool)
        .await
        .unwrap();

        let aggregate = fetch_timestamp_aggregate(&pool, "Some.Show.S01", &[indexer_id, other_id])
            .await
            .unwrap();
        assert_eq!(aggregate.earliest_first_search, 100);
        assert_eq!(aggregate.latest_first_search, MAX_INT);
        assert_eq!(aggregate.earliest_last_search, 0);
    }
}
