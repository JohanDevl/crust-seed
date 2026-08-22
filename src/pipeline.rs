//! The search, RSS and announce pipelines.
//!
//! Ported from `pipeline.ts`. Three entry points:
//!
//! * [`bulk_search`] — walk every local searchee and query the indexers;
//! * [`scan_rss_feeds`] — page each indexer's recent releases and check them;
//! * [`check_new_candidate_match`] — a single release arriving by announce or
//!   webhook, matched against local content by reverse lookup.

use std::collections::{BTreeMap, HashSet};

use sqlx::SqlitePool;

use crate::action::perform_action;
use crate::arr::{ParsedMedia, scan_all_arrs_for_media};
use crate::clients::by_client_host_priority;
use crate::config::RuntimeConfig;
use crate::constants::{ActionResult, Decision, InjectionResult, MediaType};
use crate::decide::{ResultAssessment, assess_candidate_caching, get_guid_info_hash_map};
use crate::indexers::{
    Indexer, IndexerStatus, get_all_indexers, get_enabled_indexers, update_indexer_status,
    update_search_timestamps,
};
use crate::logger::Label;
use crate::prefilter::{
    ContentFilterOptions, fetch_timestamp_aggregate, filter_by_content, filter_dupes_from_similar,
    timestamps_allow_search,
};
use crate::push_notifier::send_results_notification;
use crate::searchee::{Searchee, SearcheeLabel};
use crate::torrent::index::get_info_hashes_to_exclude;
use crate::torznab::{
    Candidate, TorznabRequest, caps_of, create_torznab_search_queries, get_search_string,
    indexer_does_support_media_type, make_requests,
};
use crate::utils::{human_readable_size, now_ms, wait};

/// A candidate plus the tracker it came from, after assessment.
pub struct AssessmentWithTracker {
    pub assessment: ResultAssessment,
    pub tracker: String,
}

/// Candidates already fetched for the current search string, so several
/// searchees that produce the same query hit the indexers once.
#[derive(Debug, Default)]
pub struct CachedSearch {
    pub q: Option<String>,
    pub indexer_candidates: Vec<crate::torznab::IndexerCandidates>,
    pub last_search: i64,
}

/// Searches every enabled, eligible indexer for one searchee and acts on the
/// matches.
///
/// Returns the number of indexers actually queried (as opposed to served from
/// the cache) and the number of matches acted on.
async fn find_on_other_sites(
    pool: &SqlitePool,
    searchee: &Searchee,
    info_hashes_to_exclude: &HashSet<String>,
    indexer_search_count: &mut BTreeMap<i64, i64>,
    cached_search: &mut CachedSearch,
    config: &RuntimeConfig,
) -> sqlx::Result<(usize, usize)> {
    crate::db::upsert_searchee(pool, &searchee.title).await?;

    let media_type = searchee.media_type();
    let (indexers_to_search, parsed_media) = select_indexers(
        pool,
        searchee,
        media_type,
        indexer_search_count,
        cached_search,
        config,
    )
    .await?;

    let cached_indexers = cached_search.indexer_candidates.len();
    let label = searchee
        .label
        .unwrap_or(SearcheeLabel::Search)
        .as_str()
        .to_string();

    let mut requests: Vec<TorznabRequest> = Vec::new();
    for indexer in &indexers_to_search {
        let caps = caps_of(indexer);
        for query in
            create_torznab_search_queries(searchee, media_type, &caps, parsed_media.as_ref())
        {
            requests.push(TorznabRequest {
                indexer_id: indexer.id,
                name: indexer.name.clone(),
                base_url: indexer.url.clone(),
                apikey: indexer.apikey.clone(),
                query,
            });
        }
    }

    // Honour `delay` between rounds of indexer traffic.
    if !requests.is_empty() {
        let wait_until = cached_search.last_search + config.delay * 1000;
        let now = now_ms();
        if now < wait_until {
            wait((wait_until - now) as u64).await;
        }
        cached_search.last_search = now_ms();
    }

    let fresh = make_requests(pool, requests, &label).await;
    let searched_indexers = fresh.len();
    let mut all = cached_search.indexer_candidates.clone();
    all.extend(fresh);
    cached_search.indexer_candidates = all.clone();

    let mut candidates: Vec<Candidate> = all
        .iter()
        .flat_map(|group| group.candidates.clone())
        .collect();
    if candidates.is_empty() {
        return Ok((searched_indexers, 0));
    }

    tracing::debug!(
        label = Label::Decide.as_str(),
        "Assessing {} candidates for {} from {searched_indexers}|{cached_indexers} indexers by search|cache",
        candidates.len(),
        searchee.title
    );

    let guid_map = get_guid_info_hash_map(pool).await;
    let mut assessments: Vec<AssessmentWithTracker> = Vec::new();
    let mut rate_limited: Vec<i64> = Vec::new();
    let mut not_rate_limited: HashSet<i64> = all.iter().map(|g| g.indexer_id).collect();

    for candidate in &mut candidates {
        let assessment = assess_candidate_caching(
            pool,
            candidate,
            searchee,
            info_hashes_to_exclude,
            &guid_map,
            config,
        )
        .await;
        if assessment.decision == Decision::RateLimited
            && let Some(indexer_id) = candidate.indexer_id
        {
            not_rate_limited.remove(&indexer_id);
            if !rate_limited.contains(&indexer_id) {
                rate_limited.push(indexer_id);
            }
        }
        assessments.push(AssessmentWithTracker {
            assessment,
            tracker: candidate.tracker.clone(),
        });
    }

    let matches: Vec<&AssessmentWithTracker> = assessments
        .iter()
        .filter(|a| a.assessment.decision.is_any_match())
        .collect();

    let mut results: Vec<(ResultAssessment, String, ActionResult)> = Vec::new();
    for entry in &matches {
        let Some(metafile) = &entry.assessment.metafile else {
            continue;
        };
        let outcome = perform_action(
            metafile,
            entry.assessment.decision,
            searchee,
            &entry.tracker,
        )
        .await;
        results.push((
            entry.assessment.clone(),
            entry.tracker.clone(),
            outcome.action_result,
        ));
    }

    // Only indexers that answered are marked as searched; a rate-limited one
    // must be retried rather than recorded as done.
    let searched: Vec<i64> = not_rate_limited.into_iter().collect();
    update_search_timestamps(pool, &searchee.title, &searched).await?;

    if !rate_limited.is_empty() {
        let all_indexers = get_all_indexers(pool).await?;
        let names: Vec<String> = rate_limited
            .iter()
            .filter_map(|id| all_indexers.iter().find(|i| i.id == *id))
            .map(|i| i.name.clone().unwrap_or_else(|| i.url.clone()))
            .collect();
        update_indexer_status(
            pool,
            IndexerStatus::RateLimited,
            now_ms() + 60 * 60 * 1000,
            &rate_limited,
            &names,
        )
        .await?;
    }

    send_results_notification(searchee, &results, config).await;
    Ok((searched_indexers, matches.len()))
}

/// Chooses which indexers to query for a searchee.
///
/// Applies (in order) capability support, per-indexer search history, the
/// `searchLimit` budget, and the candidate cache — an indexer already
/// represented in the cache for this query is skipped.
async fn select_indexers(
    pool: &SqlitePool,
    searchee: &Searchee,
    media_type: MediaType,
    indexer_search_count: &mut BTreeMap<i64, i64>,
    cached_search: &mut CachedSearch,
    config: &RuntimeConfig,
) -> sqlx::Result<(Vec<Indexer>, Option<ParsedMedia>)> {
    let enabled = get_enabled_indexers(pool).await?;
    let supported: Vec<Indexer> = enabled
        .into_iter()
        .filter(|indexer| indexer_does_support_media_type(media_type, indexer))
        .collect();

    let aggregate = fetch_timestamp_aggregate(
        pool,
        &searchee.title,
        &supported.iter().map(|i| i.id).collect::<Vec<_>>(),
    )
    .await?;
    if !timestamps_allow_search(aggregate, config, searchee, searchee.mtime_ms) {
        return Ok((Vec::new(), None));
    }

    // Invalidate the candidate cache when the query itself changes.
    let search_string = get_search_string(searchee);
    if cached_search.q.as_deref() != Some(search_string.as_str()) {
        cached_search.q = Some(search_string);
        cached_search.indexer_candidates.clear();
    }

    let mut to_search = Vec::new();
    for indexer in supported {
        if cached_search
            .indexer_candidates
            .iter()
            .any(|group| group.indexer_id == indexer.id)
        {
            continue;
        }
        if let Some(search_limit) = config.search_limit.filter(|l| *l > 0) {
            let count = indexer_search_count.entry(indexer.id).or_insert(0);
            if *count >= search_limit {
                continue;
            }
            *count += 1;
        }
        to_search.push(indexer);
    }

    let parsed_media = scan_all_arrs_for_media(&searchee.title, media_type).await;
    Ok((to_search, parsed_media))
}

/// Runs a batch of searchees, sharing the candidate cache and the indexer
/// search budget between them.
pub async fn find_matches_batch(
    pool: &SqlitePool,
    searchees: &[Searchee],
    info_hashes_to_exclude: &HashSet<String>,
    config: &RuntimeConfig,
) -> usize {
    let mut indexer_search_count: BTreeMap<i64, i64> = BTreeMap::new();
    let mut cached_search = CachedSearch::default();
    let mut total_found = 0usize;

    for (index, searchee) in searchees.iter().enumerate() {
        let previous_search = cached_search.last_search;
        match find_on_other_sites(
            pool,
            searchee,
            info_hashes_to_exclude,
            &mut indexer_search_count,
            &mut cached_search,
            config,
        )
        .await
        {
            Ok((searched_indexers, matches)) => {
                total_found += matches;
                if let Some(search_limit) = config.search_limit.filter(|l| *l > 0)
                    && !indexer_search_count.is_empty()
                    && indexer_search_count.values().all(|n| *n >= search_limit)
                {
                    tracing::info!(
                        label = Label::Search.as_str(),
                        "Reached searchLimit of {search_limit} on all indexers"
                    );
                    break;
                }
                // Everything came from the cache, so no delay is owed.
                if searched_indexers == 0 {
                    cached_search.last_search = previous_search;
                }
            }
            Err(e) => {
                tracing::error!(
                    label = Label::Search.as_str(),
                    "({}/{}) Error searching for {}: {e}",
                    index + 1,
                    searchees.len(),
                    searchee.title
                );
            }
        }
    }
    total_found
}

/// Every local searchee, from whichever source the config selects.
pub async fn find_all_searchees(
    pool: &SqlitePool,
    label: SearcheeLabel,
    config: &RuntimeConfig,
) -> Vec<Searchee> {
    let mut searchees: Vec<Searchee> = Vec::new();

    // An explicit `--torrents` list overrides every other source.
    let explicit: Vec<&String> = config
        .torrents
        .iter()
        .filter(|t| !t.trim().is_empty())
        .collect();
    if !explicit.is_empty() {
        for torrent in explicit {
            if let Ok(meta) =
                crate::torrent::index::parse_torrent_with_metadata(std::path::Path::new(torrent))
                    .await
                && let Ok(searchee) = crate::searchee::searchee_from_metafile(&meta)
            {
                searchees.push(searchee);
            }
        }
    } else {
        if config.use_client_torrents {
            for client in crate::clients::get_clients() {
                let options = crate::clients::GetSearcheesOptions {
                    include_files: true,
                    include_trackers: true,
                    ..Default::default()
                };
                match client.get_client_searchees(options).await {
                    Ok(result) => searchees.extend(result.searchees),
                    Err(e) => tracing::warn!(
                        label = client.label(),
                        "Could not read torrents from client: {e}"
                    ),
                }
            }
        } else if let Some(torrent_dir) = &config.torrent_dir {
            searchees.extend(
                crate::torrent::index::load_torrent_dir_light(std::path::Path::new(torrent_dir))
                    .await,
            );
        }

        if !config.data_dirs.is_empty() {
            let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM data")
                .fetch_all(pool)
                .await
                .unwrap_or_default();
            for path in paths {
                if let Some(searchee) = crate::torrent::lookup::searchee_from_data_path(&path).await
                {
                    searchees.push(searchee);
                }
            }
        }
    }

    for searchee in &mut searchees {
        searchee.label = Some(label);
    }
    searchees
}

/// Gathers and filters the searchees a bulk search should run over.
pub async fn find_searchable_torrents(
    pool: &SqlitePool,
    config: &RuntimeConfig,
) -> (Vec<Searchee>, HashSet<String>) {
    tracing::info!(label = Label::Search.as_str(), "Gathering searchees...");
    let real_searchees = find_all_searchees(pool, SearcheeLabel::Search, config).await;

    let info_hashes_to_exclude: HashSet<String> = real_searchees
        .iter()
        .filter_map(|s| s.info_hash.clone())
        .collect();

    let mut valid: Vec<Searchee> = Vec::new();
    for searchee in &real_searchees {
        if filter_by_content(searchee, config, ContentFilterOptions::default()).await {
            valid.push(searchee.clone());
        }
    }

    // Group by the query they would produce, so identical queries share one
    // round of indexer traffic.
    let mut grouping: BTreeMap<String, Vec<Searchee>> = BTreeMap::new();
    for searchee in valid {
        grouping
            .entry(get_search_string(&searchee))
            .or_default()
            .push(searchee);
    }

    let mut final_searchees = Vec::new();
    let mut unique_queries = 0usize;
    for (_, group) in grouping {
        let mut filtered = filter_dupes_from_similar(&group);
        // Prefer the client with the highest priority, then the most complete
        // copy, then a torrent over a data directory.
        filtered.sort_by_key(|searchee| {
            (
                by_client_host_priority(searchee.client_host.as_deref()),
                std::cmp::Reverse(searchee.files.len()),
                searchee.info_hash.is_none(),
            )
        });
        if filtered.is_empty() {
            continue;
        }
        unique_queries += 1;
        final_searchees.extend(filtered);
    }

    tracing::info!(
        label = Label::Search.as_str(),
        "Found {} torrents, {} suitable to search for matches using {unique_queries} unique queries",
        real_searchees.len(),
        final_searchees.len()
    );

    (final_searchees, info_hashes_to_exclude)
}

pub async fn bulk_search(pool: &SqlitePool, config: &RuntimeConfig) {
    let (searchees, info_hashes_to_exclude) = find_searchable_torrents(pool, config).await;
    let total_found = find_matches_batch(pool, &searchees, &info_hashes_to_exclude, config).await;
    tracing::info!(
        label = Label::Search.as_str(),
        "Found {total_found} cross seeds from {} original torrents",
        searchees.len()
    );
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct BulkSearchSummary {
    pub requested: usize,
    pub attempted: usize,
    pub total_found: usize,
    pub skipped: usize,
}

/// The web UI's "search selected" action.
pub async fn bulk_search_by_names(
    pool: &SqlitePool,
    names: &[String],
    config: &RuntimeConfig,
) -> BulkSearchSummary {
    let normalized = crate::utils::dedupe_preserving_order(
        &names
            .iter()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty() && name.chars().count() <= 500)
            .collect::<Vec<_>>(),
    );
    if normalized.is_empty() {
        return BulkSearchSummary::default();
    }

    let (searchees, info_hashes_to_exclude) = find_searchable_torrents(pool, config).await;
    let selected: Vec<Searchee> = searchees
        .into_iter()
        .filter(|searchee| {
            normalized.contains(&searchee.title) || normalized.contains(&searchee.name)
        })
        .collect();

    if selected.is_empty() {
        tracing::warn!(
            label = Label::Search.as_str(),
            "Manual bulk search requested for {} items, but none were eligible after filtering",
            normalized.len()
        );
        return BulkSearchSummary {
            requested: normalized.len(),
            ..Default::default()
        };
    }

    tracing::info!(
        label = Label::Search.as_str(),
        "Starting manual bulk search for {}/{} selected items",
        selected.len(),
        normalized.len()
    );
    let total_found = find_matches_batch(pool, &selected, &info_hashes_to_exclude, config).await;

    BulkSearchSummary {
        requested: normalized.len(),
        attempted: selected.len(),
        total_found,
        skipped: normalized.len().saturating_sub(selected.len()),
    }
}

/// Result of checking one incoming candidate.
#[derive(Debug, Clone, Copy, Default)]
pub struct CandidateOutcome {
    pub decision: Option<Decision>,
    pub action_result: Option<ActionResult>,
}

/// Matches a single incoming release against local content.
///
/// Candidates are tried against the most promising searchee first, and the loop
/// stops as soon as a complete match is injected: a partial match is kept only
/// while nothing better has been found.
pub async fn check_new_candidate_match(
    pool: &SqlitePool,
    candidate: &mut Candidate,
    label: SearcheeLabel,
    config: &RuntimeConfig,
) -> CandidateOutcome {
    let similar =
        match crate::torrent::lookup::get_similar_by_name(pool, &candidate.name, config).await {
            Ok(similar) => similar,
            Err(e) => {
                tracing::error!(label = label.as_str(), "Reverse lookup failed: {e}");
                return CandidateOutcome::default();
            }
        };

    let mut searchees: Vec<Searchee> = Vec::new();
    for mut searchee in similar
        .client_searchees
        .into_iter()
        .chain(similar.data_searchees)
    {
        searchee.label = Some(label);
        if filter_by_content(&searchee, config, ContentFilterOptions::default()).await {
            searchees.push(searchee);
        }
    }
    let mut searchees = filter_dupes_from_similar(&searchees);
    if searchees.is_empty() {
        tracing::debug!(
            label = label.as_str(),
            "Did not find an existing entry for {} from {}",
            candidate.name,
            candidate.tracker
        );
        return CandidateOutcome::default();
    }

    searchees.sort_by_key(|searchee| {
        (
            by_client_host_priority(searchee.client_host.as_deref()),
            // Prefer a real torrent over a synthesised ensemble.
            searchee.info_hash.is_none(),
            std::cmp::Reverse(searchee.files.len()),
        )
    });

    let info_hashes_to_exclude = get_info_hashes_to_exclude(pool, config)
        .await
        .unwrap_or_default();
    let guid_map = get_guid_info_hash_map(pool).await;

    let mut outcome = CandidateOutcome::default();
    let mut matched: Option<(Searchee, ResultAssessment)> = None;

    for searchee in &searchees {
        let _ = crate::db::upsert_searchee(pool, &searchee.title).await;
        let assessment = assess_candidate_caching(
            pool,
            candidate,
            searchee,
            &info_hashes_to_exclude,
            &guid_map,
            config,
        )
        .await;

        if assessment.decision == Decision::InfoHashAlreadyExists {
            // Already in the client: nothing to do, and no other searchee can
            // change that.
            outcome.decision = Some(assessment.decision);
            break;
        }
        if !assessment.decision.is_any_match() {
            continue;
        }
        let Some(metafile) = assessment.metafile.clone() else {
            continue;
        };

        let action =
            perform_action(&metafile, assessment.decision, searchee, &candidate.tracker).await;

        if action.action_result == ActionResult::Saved {
            outcome.decision = Some(assessment.decision);
            outcome.action_result = Some(action.action_result);
            matched = Some((searchee.clone(), assessment));
            break;
        }
        let injected = matches!(
            action.action_result,
            ActionResult::Injection(InjectionResult::Success)
                | ActionResult::Injection(InjectionResult::AlreadyExists)
        );
        if assessment.decision != Decision::MatchPartial && injected {
            outcome.decision = Some(assessment.decision);
            outcome.action_result = Some(ActionResult::Injection(InjectionResult::Success));
            matched = Some((searchee.clone(), assessment));
            break;
        }
        // Keep looking for something better, but do not downgrade what we have.
        if outcome.action_result == Some(ActionResult::Injection(InjectionResult::Success)) {
            continue;
        }
        if outcome.action_result.is_some()
            && action.action_result == ActionResult::Injection(InjectionResult::Failure)
        {
            continue;
        }
        if outcome.action_result == Some(ActionResult::Injection(InjectionResult::AlreadyExists))
            && action.action_result == ActionResult::Injection(InjectionResult::TorrentNotComplete)
        {
            continue;
        }
        outcome.decision = Some(assessment.decision);
        outcome.action_result = Some(action.action_result);
        matched = Some((searchee.clone(), assessment));
    }

    if let Some((searchee, assessment)) = matched
        && let Some(action_result) = outcome.action_result
    {
        send_results_notification(
            &searchee,
            &[(assessment, candidate.tracker.clone(), action_result)],
            config,
        )
        .await;
    }
    outcome
}

/// The `rss` command and job.
pub async fn scan_rss_feeds(pool: &SqlitePool, config: &RuntimeConfig, last_run: i64) {
    let enabled = get_enabled_indexers(pool).await.unwrap_or_default();
    if enabled.is_empty()
        || (!config.use_client_torrents
            && config.torrent_dir.is_none()
            && config.data_dirs.is_empty())
    {
        tracing::warn!(
            label = Label::Rss.as_str(),
            "RSS requires enabled indexers and at least one of useClientTorrents, torrentDir, or dataDirs to be set"
        );
        return;
    }

    tracing::debug!(label = Label::Rss.as_str(), "Querying RSS feeds...");
    let time_since_last_run = now_ms() - last_run;
    let mut num_candidates = 0usize;

    for indexer in &enabled {
        for mut candidate in rss_page(pool, indexer, time_since_last_run).await {
            check_new_candidate_match(pool, &mut candidate, SearcheeLabel::Rss, config).await;
            num_candidates += 1;
        }
    }

    tracing::info!(
        label = Label::Rss.as_str(),
        "RSS scan complete - checked {num_candidates} new candidates since {}",
        crate::utils::human_readable_date(last_run)
    );
}

/// Pages an indexer's recent releases, stopping at the last guid seen or at
/// releases older than the previous run.
///
/// Both stops matter: the guid catches the normal case, and the publication
/// date bounds the first run (and any run after the guid has aged out of the
/// feed) so a fresh install does not walk the entire history.
async fn rss_page(
    pool: &SqlitePool,
    indexer: &Indexer,
    time_since_last_run: i64,
) -> Vec<Candidate> {
    const MAX_PAGES: i64 = 10;
    const MAX_CANDIDATES: usize = 10_000;

    let limit = indexer.limits.map(|l| l.max).unwrap_or(100);
    let last_seen_guid: Option<String> =
        sqlx::query_scalar("SELECT last_seen_guid FROM rss WHERE indexer_id = ?")
            .bind(indexer.id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .flatten();

    let mut new_last_seen_guid = last_seen_guid.clone();
    let mut page_back_until = 0i64;
    let mut collected: Vec<Candidate> = Vec::new();

    for page in 0..MAX_PAGES {
        let mut query = crate::torznab::Query::new(crate::torznab::QueryKind::Search);
        query.q = Some(String::new());
        query.limit = Some(limit);
        query.offset = Some(page * limit);

        let request = TorznabRequest {
            indexer_id: indexer.id,
            name: indexer.name.clone(),
            base_url: indexer.url.clone(),
            apikey: indexer.apikey.clone(),
            query,
        };
        let Ok(mut page_candidates) =
            crate::torznab::make_request(pool, &request, Label::Rss.as_str()).await
        else {
            break;
        };
        page_candidates.sort_by_key(|c| std::cmp::Reverse(c.pub_date.unwrap_or(0)));
        if page_candidates.is_empty() {
            break;
        }
        if page == 0 {
            new_last_seen_guid = Some(page_candidates[0].guid.clone());
            page_back_until = page_candidates[0].pub_date.unwrap_or(0) - time_since_last_run;
        }

        let mut fresh: Vec<Candidate> = Vec::new();
        let mut found_last_seen = false;
        for candidate in &page_candidates {
            if Some(&candidate.guid) == last_seen_guid.as_ref() {
                found_last_seen = true;
                break;
            }
            fresh.push(candidate.clone());
        }
        if !found_last_seen {
            fresh.retain(|candidate| candidate.pub_date.unwrap_or(0) >= page_back_until);
        }
        if fresh.is_empty() {
            break;
        }

        let exhausted_page = fresh.len() != page_candidates.len();
        collected.extend(fresh);
        if exhausted_page || collected.len() >= MAX_CANDIDATES {
            break;
        }
    }

    if let Some(guid) = new_last_seen_guid {
        let _ = sqlx::query(
            "INSERT INTO rss (indexer_id, last_seen_guid) VALUES (?, ?)
             ON CONFLICT (indexer_id) DO UPDATE SET last_seen_guid = excluded.last_seen_guid",
        )
        .bind(indexer.id)
        .bind(guid)
        .execute(pool)
        .await;
    }
    collected
}

/// The `webhook` route's search-by-criteria path.
pub async fn search_for_local_torrent_by_criteria(
    pool: &SqlitePool,
    info_hash: Option<&str>,
    path: Option<&str>,
    config: &RuntimeConfig,
    ignore_cross_seeds: bool,
) -> Option<usize> {
    let mut searchees: Vec<Searchee> = Vec::new();
    match (info_hash, path) {
        (Some(info_hash), _) => {
            searchees.extend(
                crate::torrent::lookup::get_torrent_by_info_hash(pool, info_hash, config)
                    .await
                    .unwrap_or_default(),
            );
        }
        (None, Some(path)) => {
            if let Some(searchee) = crate::torrent::lookup::searchee_from_data_path(path).await {
                searchees.push(searchee);
            }
        }
        (None, None) => return None,
    }
    if searchees.is_empty() {
        return None;
    }

    let mut valid = Vec::new();
    for mut searchee in searchees {
        searchee.label = Some(SearcheeLabel::Webhook);
        if filter_by_content(
            &searchee,
            config,
            ContentFilterOptions {
                ignore_cross_seeds,
                ..Default::default()
            },
        )
        .await
        {
            valid.push(searchee);
        }
    }
    if valid.is_empty() {
        return Some(0);
    }

    let info_hashes_to_exclude = get_info_hashes_to_exclude(pool, config)
        .await
        .unwrap_or_default();
    let total = find_matches_batch(pool, &valid, &info_hashes_to_exclude, config).await;
    tracing::info!(
        label = Label::Webhook.as_str(),
        "Searched {} entries ({})",
        valid.len(),
        human_readable_size(valid.iter().map(|s| s.length).sum(), false)
    );
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::db::test_pool;
    use crate::searchee::File;

    fn searchee(title: &str, files: usize, info_hash: Option<&str>) -> Searchee {
        Searchee {
            info_hash: info_hash.map(str::to_string),
            name: title.to_string(),
            title: title.to_string(),
            files: (0..files)
                .map(|i| File {
                    name: format!("{i}.mkv"),
                    path: format!("{title}/{i}.mkv"),
                    length: 100,
                })
                .collect(),
            length: 100 * files as i64,
            label: Some(SearcheeLabel::Search),
            ..Default::default()
        }
    }

    /// A searchee whose timestamps say "recently searched" contributes no
    /// indexers, so no request is made.
    #[tokio::test]
    async fn recently_searched_titles_select_no_indexers() {
        let pool = test_pool().await;
        let config = default_runtime_config();

        let indexer_id: i64 = sqlx::query(
            "INSERT INTO indexer (url, apikey, enabled, search_cap, tv_search_cap, movie_search_cap,
             music_search_cap, audio_search_cap, book_search_cap, tv_id_caps, movie_id_caps,
             cat_caps, limits_caps)
             VALUES ('https://a/api', 'k', 1, 1, 1, 1, 1, 1, 1, '{}', '{}', '{}', '{}')",
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_rowid();

        let s = searchee("Some.Show.S01E01", 1, Some("abc"));
        let searchee_id = crate::db::upsert_searchee(&pool, &s.title).await.unwrap();
        sqlx::query(
            "INSERT INTO timestamp (searchee_id, indexer_id, first_searched, last_searched)
             VALUES (?, ?, ?, ?)",
        )
        .bind(searchee_id)
        .bind(indexer_id)
        .bind(now_ms())
        .bind(now_ms())
        .execute(&pool)
        .await
        .unwrap();

        let mut counts = BTreeMap::new();
        let mut cache = CachedSearch::default();
        let (indexers, _) =
            select_indexers(&pool, &s, s.media_type(), &mut counts, &mut cache, &config)
                .await
                .unwrap();
        assert!(indexers.is_empty());
    }

    #[tokio::test]
    async fn the_search_limit_caps_queries_per_indexer() {
        let pool = test_pool().await;
        let mut config = default_runtime_config();
        config.search_limit = Some(1);
        config.exclude_recent_search = None;
        config.exclude_older = None;

        sqlx::query(
            "INSERT INTO indexer (url, apikey, enabled, search_cap, tv_search_cap, movie_search_cap,
             music_search_cap, audio_search_cap, book_search_cap, tv_id_caps, movie_id_caps,
             cat_caps, limits_caps)
             VALUES ('https://a/api', 'k', 1, 1, 1, 1, 1, 1, 1, '{}', '{}', '{}', '{}')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut counts = BTreeMap::new();
        let first = searchee("Some.Show.S01E01", 1, Some("a"));
        let second = searchee("Other.Show.S01E01", 1, Some("b"));

        let mut cache = CachedSearch::default();
        let (indexers, _) = select_indexers(
            &pool,
            &first,
            first.media_type(),
            &mut counts,
            &mut cache,
            &config,
        )
        .await
        .unwrap();
        assert_eq!(indexers.len(), 1);

        let mut cache = CachedSearch::default();
        let (indexers, _) = select_indexers(
            &pool,
            &second,
            second.media_type(),
            &mut counts,
            &mut cache,
            &config,
        )
        .await
        .unwrap();
        assert!(indexers.is_empty(), "searchLimit exhausted");
    }

    /// Two searchees producing the same query must not both hit the indexers.
    #[tokio::test]
    async fn a_cached_indexer_is_not_queried_again() {
        let pool = test_pool().await;
        let mut config = default_runtime_config();
        config.exclude_recent_search = None;
        config.exclude_older = None;

        let indexer_id: i64 = sqlx::query(
            "INSERT INTO indexer (url, apikey, enabled, search_cap, tv_search_cap, movie_search_cap,
             music_search_cap, audio_search_cap, book_search_cap, tv_id_caps, movie_id_caps,
             cat_caps, limits_caps)
             VALUES ('https://a/api', 'k', 1, 1, 1, 1, 1, 1, 1, '{}', '{}', '{}', '{}')",
        )
        .execute(&pool)
        .await
        .unwrap()
        .last_insert_rowid();

        let s = searchee("Some.Show.S01E01", 1, Some("abc"));
        let mut counts = BTreeMap::new();
        let mut cache = CachedSearch {
            q: Some(get_search_string(&s)),
            indexer_candidates: vec![crate::torznab::IndexerCandidates {
                indexer_id,
                candidates: Vec::new(),
            }],
            last_search: 0,
        };

        let (indexers, _) =
            select_indexers(&pool, &s, s.media_type(), &mut counts, &mut cache, &config)
                .await
                .unwrap();
        assert!(indexers.is_empty());
    }

    /// Changing the query must invalidate the candidate cache.
    #[tokio::test]
    async fn a_different_query_clears_the_candidate_cache() {
        let pool = test_pool().await;
        let mut config = default_runtime_config();
        config.exclude_recent_search = None;
        config.exclude_older = None;

        let s = searchee("Some.Show.S01E01", 1, Some("abc"));
        let mut counts = BTreeMap::new();
        let mut cache = CachedSearch {
            q: Some("a completely different query".into()),
            indexer_candidates: vec![crate::torznab::IndexerCandidates {
                indexer_id: 1,
                candidates: Vec::new(),
            }],
            last_search: 0,
        };

        select_indexers(&pool, &s, s.media_type(), &mut counts, &mut cache, &config)
            .await
            .unwrap();
        assert!(cache.indexer_candidates.is_empty());
        assert_eq!(cache.q, Some(get_search_string(&s)));
    }

    #[tokio::test]
    async fn bulk_search_by_names_reports_what_it_skipped() {
        let pool = test_pool().await;
        let config = default_runtime_config();
        let summary = bulk_search_by_names(
            &pool,
            &["Nothing.Matching.This".to_string(), " ".to_string()],
            &config,
        )
        .await;
        assert_eq!(summary.requested, 1);
        assert_eq!(summary.attempted, 0);
    }
}
