//! The inject command and job.
//!
//! Ported from `inject.ts`.
//!
//! Saved `.torrent` files accumulate in `outputDir` for two reasons: the user
//! runs with `action: "save"`, or an injected torrent could not be seeded yet
//! (source incomplete, client unreachable). This walks them, matches each
//! against local content, injects what it can, and deletes what is done.
//!
//! Deletion is deliberately conservative: a file is only removed when its
//! *filename* proves crust-seed wrote it, or the torrent is confirmed complete
//! in a client.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sqlx::SqlitePool;

use crate::action::perform_action;
use crate::clients::{by_client_host_priority, get_clients, wait_for_torrent_to_complete};
use crate::config::{RuntimeConfig, torrent_cache_dir};
use crate::constants::{ActionResult, Decision, InjectionResult, MediaType, UNKNOWN_TRACKER};
use crate::decide::{MetaOrCandidate, assess_candidate};
use crate::indexers::get_host_to_name_map;
use crate::logger::Label;
use crate::prefilter::find_blocked_string_in_release_maybe;
use crate::searchee::{Searchee, SearcheeLabel, get_all_titles, media_type_of};
use crate::torrent::Metafile;
use crate::torrent::cache::{
    find_all_torrent_files_in_dir, get_torrent_save_path, parse_metadata_from_filename,
};
use crate::utils::{are_media_titles_similar, exists, sanitize_info_hash};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InjectSummary {
    pub total: usize,
    pub injected: usize,
    pub full_matches: usize,
    pub partial_matches: usize,
    pub blocked: usize,
    pub already_exists: usize,
    pub incomplete_candidates: usize,
    pub incomplete_searchees: usize,
    pub failed: usize,
    pub unmatched: usize,
    /// At least one file did not follow the naming scheme, so its tracker had
    /// to be inferred (or fell back to the sentinel).
    pub found_bad_format: bool,
}

#[derive(Debug, Clone)]
struct SearcheeMatch {
    searchee: Searchee,
    decision: Decision,
}

/// Deletes a saved torrent only when its filename proves crust-seed wrote it.
///
/// `injectDir` may point at a directory of the user's own torrents; deleting
/// those would be data loss.
async fn delete_torrent_file_if_safe(torrent_file_path: &Path) {
    let filename = torrent_file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let metadata = parse_metadata_from_filename(&filename);
    let safe =
        metadata.tracker.is_some() && metadata.name.is_some() && metadata.media_type.is_some();
    if !safe {
        tracing::warn!(
            label = Label::Inject.as_str(),
            "Will not delete {}: missing metadata from filename",
            torrent_file_path.display()
        );
        return;
    }
    tracing::debug!(
        label = Label::Inject.as_str(),
        "Deleting {}",
        torrent_file_path.display()
    );
    if let Err(e) = tokio::fs::remove_file(torrent_file_path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::error!(
            label = Label::Inject.as_str(),
            "Failed to delete {}: {e}",
            torrent_file_path.display()
        );
    }
}

/// Whether two names are close enough that injecting one for the other is not
/// a mistake.
///
/// All four name/title combinations are tried because a searchee's `title` may
/// be a *derived* season name while the torrent carries the original.
fn titles_are_similar(searchee: &Searchee, meta: &Metafile) -> bool {
    let expand = |titles: &[String]| get_all_titles(titles);
    are_media_titles_similar(&searchee.title, &meta.title, expand)
        || are_media_titles_similar(&searchee.title, &meta.name, expand)
        || are_media_titles_similar(&searchee.name, &meta.name, expand)
        || are_media_titles_similar(&searchee.name, &meta.title, expand)
}

/// Which local searchees this torrent could be seeded from, best first.
async fn which_searchees_match_torrent(
    meta: &Metafile,
    searchees: &[Searchee],
    config: &RuntimeConfig,
    ignore_titles: bool,
) -> (Vec<SearcheeMatch>, bool, bool) {
    let mut matches: Vec<SearcheeMatch> = Vec::new();
    let mut found_blocked = false;
    let mut fuzzy_fail = false;
    let mut is_complete: Option<bool> = None;

    // The blocklist is applied explicitly below, not inside the assessment:
    // a blocked *match* has to be counted as "blocked", and letting
    // assess_candidate reject it would make it indistinguishable from
    // "no match at all", which changes both the summary and whether the
    // saved .torrent gets deleted.
    let config_without_blocklist = RuntimeConfig {
        block_list: Vec::new(),
        ..config.clone()
    };

    for searchee in searchees {
        let assessment = assess_candidate(
            MetaOrCandidate::Meta(meta),
            searchee,
            &Default::default(),
            &config_without_blocklist,
        )
        .await;
        if !assessment.decision.is_any_match() {
            continue;
        }
        if find_blocked_string_in_release_maybe(searchee, &config.block_list).is_some() {
            found_blocked = true;
            continue;
        }

        if !titles_are_similar(searchee, meta) {
            if ignore_titles {
                tracing::warn!(
                    label = Label::Inject.as_str(),
                    "Ignoring title mismatch for {} with {}",
                    meta.name,
                    searchee.title
                );
            } else {
                // A torrent already complete in a client is proof enough that
                // the data matches, whatever the titles say.
                if is_complete.is_none() {
                    let mut complete = false;
                    for client in get_clients() {
                        if client
                            .is_torrent_complete(&meta.info_hash)
                            .await
                            .unwrap_or(false)
                        {
                            complete = true;
                            break;
                        }
                    }
                    is_complete = Some(complete);
                }
                if is_complete != Some(true) {
                    tracing::warn!(
                        label = Label::Inject.as_str(),
                        "Skipping match for {} with {} due to title mismatch (use \"crust-seed inject --ignore-titles\" if this is an erroneous rejection)",
                        meta.name,
                        searchee.title
                    );
                    fuzzy_fail = true;
                    continue;
                }
            }
        }

        matches.push(SearcheeMatch {
            searchee: searchee.clone(),
            decision: assessment.decision,
        });
    }

    // Client priority, then full matches before size-only before partial, then
    // torrent before data before virtual, then the most complete copy.
    matches.sort_by_key(|m| {
        let decision_rank = match m.decision {
            Decision::Match => 0,
            Decision::MatchSizeOnly => 1,
            _ => 2,
        };
        (
            by_client_host_priority(m.searchee.client_host.as_deref()),
            decision_rank,
            m.searchee.info_hash.is_none(),
            m.searchee.path.is_none(),
            std::cmp::Reverse(m.searchee.files.len()),
        )
    });

    (matches, found_blocked, fuzzy_fail)
}

/// Tries each match in turn, keeping the best outcome.
///
/// A `FAILURE` that linked files stops the loop: the links could not be rolled
/// back, so the next job must see the same state rather than a second attempt
/// layered on top.
async fn inject_initial_action(
    meta: &Metafile,
    matches: &[SearcheeMatch],
    tracker: &str,
) -> (InjectionResult, Option<Decision>) {
    let mut injection_result = InjectionResult::Failure;
    let mut matched_decision = None;

    for entry in matches {
        let outcome = perform_action(meta, entry.decision, &entry.searchee, tracker).await;
        match outcome.action_result {
            ActionResult::Injection(InjectionResult::Failure) => {
                if outcome.linked_new_files {
                    break;
                }
                continue;
            }
            ActionResult::Saved => continue,
            ActionResult::Injection(result) => {
                if injection_result == InjectionResult::Success {
                    continue;
                }
                match result {
                    InjectionResult::AlreadyExists => {
                        injection_result = result;
                    }
                    InjectionResult::TorrentNotComplete => {
                        if injection_result != InjectionResult::AlreadyExists {
                            injection_result = result;
                        }
                    }
                    InjectionResult::Success => {
                        injection_result = InjectionResult::Success;
                        matched_decision = Some(entry.decision);
                    }
                    InjectionResult::Failure => {}
                }
            }
        }
    }

    (injection_result, matched_decision)
}

/// Resolves the tracker a saved torrent belongs to.
///
/// The filename is authoritative; failing that the announce hosts are matched
/// against known indexers, and failing that the sentinel is used (which is what
/// puts a torrent in `linkDir/UnknownTracker`).
async fn resolve_tracker(
    pool: &SqlitePool,
    meta: &Metafile,
    torrent_file_path: &Path,
    summary: &mut InjectSummary,
) -> String {
    let filename = torrent_file_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(tracker) = parse_metadata_from_filename(&filename).tracker {
        return tracker;
    }
    summary.found_bad_format = true;

    let host_to_name = get_host_to_name_map(pool).await.unwrap_or_default();
    meta.trackers
        .iter()
        .find_map(|host| host_to_name.get(host).cloned())
        .unwrap_or_else(|| UNKNOWN_TRACKER.to_string())
}

async fn inject_saved_torrent(
    pool: &SqlitePool,
    torrent_file_path: &Path,
    summary: &mut InjectSummary,
    searchees: &[Searchee],
    config: &RuntimeConfig,
    ignore_titles: bool,
) {
    let Ok(bytes) = tokio::fs::read(torrent_file_path).await else {
        return;
    };
    let meta = match Metafile::decode(&bytes) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::error!(
                label = Label::Inject.as_str(),
                "Failed to parse {}: {e}",
                torrent_file_path.display()
            );
            return;
        }
    };

    let meta_as_searchee = Searchee {
        info_hash: Some(meta.info_hash.clone()),
        name: meta.name.clone(),
        title: meta.title.clone(),
        files: meta.files.clone(),
        length: meta.length,
        trackers: Some(meta.trackers.clone()),
        ..Default::default()
    };
    if let Some(blocked) =
        find_blocked_string_in_release_maybe(&meta_as_searchee, &config.block_list)
    {
        tracing::warn!(
            label = Label::Inject.as_str(),
            "{} is in the blockList: {blocked}",
            torrent_file_path.display()
        );
        summary.blocked += 1;
        return;
    }

    let tracker = resolve_tracker(pool, &meta, torrent_file_path, summary).await;
    let (matches, found_blocked, fuzzy_fail) =
        which_searchees_match_torrent(&meta, searchees, config, ignore_titles).await;

    if matches.is_empty() {
        if found_blocked {
            tracing::warn!(
                label = Label::Inject.as_str(),
                "{} has all matches in the blockList",
                torrent_file_path.display()
            );
            summary.blocked += 1;
        } else {
            tracing::error!(
                label = Label::Inject.as_str(),
                "{} has no matches",
                torrent_file_path.display()
            );
            summary.unmatched += 1;
        }
        if fuzzy_fail {
            // The user may want to retry with --ignore-titles, so the file stays.
            tracing::warn!(
                label = Label::Inject.as_str(),
                "Will not delete {}: it has no matches due to title mismatch",
                torrent_file_path.display()
            );
        } else {
            delete_torrent_file_if_safe(torrent_file_path).await;
        }
        return;
    }

    let (injection_result, matched_decision) =
        inject_initial_action(&meta, &matches, &tracker).await;

    match injection_result {
        InjectionResult::Success => {
            summary.injected += 1;
            match matched_decision {
                Some(Decision::MatchPartial) => summary.partial_matches += 1,
                Some(_) => summary.full_matches += 1,
                None => {}
            }
            tracing::info!(
                label = Label::Inject.as_str(),
                "Injected {} from {tracker}",
                meta.name
            );
            // Only delete once the client confirms the data is really there.
            for client in get_clients() {
                if wait_for_torrent_to_complete(client.as_ref(), &meta.info_hash, 6).await {
                    delete_torrent_file_if_safe(torrent_file_path).await;
                    break;
                }
            }
        }
        InjectionResult::AlreadyExists => {
            summary.already_exists += 1;
            delete_torrent_file_if_safe(torrent_file_path).await;
        }
        InjectionResult::TorrentNotComplete => {
            summary.incomplete_searchees += 1;
        }
        InjectionResult::Failure => {
            tracing::error!(
                label = Label::Inject.as_str(),
                "Failed to inject {}",
                torrent_file_path.display()
            );
            summary.failed += 1;
        }
    }
}

fn log_inject_summary(summary: &InjectSummary, flat_linking: bool) {
    let mut parts = vec![format!(
        "Injected {}/{} torrents",
        summary.injected, summary.total
    )];
    if summary.full_matches > 0 {
        parts.push(format!("{} were full matches", summary.full_matches));
    }
    if summary.partial_matches > 0 {
        parts.push(format!("{} were partial matches", summary.partial_matches));
    }
    if summary.incomplete_searchees > 0 {
        parts.push(format!(
            "{} had incomplete sources",
            summary.incomplete_searchees
        ));
    }
    if summary.already_exists > 0 {
        parts.push(format!("{} existed in client", summary.already_exists));
    }
    if summary.blocked > 0 {
        parts.push(format!("{} were possibly blocklisted", summary.blocked));
    }
    if summary.failed > 0 {
        parts.push(format!("{} failed to inject", summary.failed));
    }
    if summary.unmatched > 0 {
        parts.push(format!("{} had no matches", summary.unmatched));
    }
    tracing::info!(label = Label::Inject.as_str(), "{}", parts.join(", "));

    if summary.unmatched > 0 {
        tracing::info!(
            label = Label::Inject.as_str(),
            "Use \"crust-seed diff\" to get the reasons two torrents are not considered matches"
        );
    }
    if summary.found_bad_format && !flat_linking {
        tracing::warn!(
            label = Label::Inject.as_str(),
            "Some torrents could be linked to linkDir/{UNKNOWN_TRACKER} - follow .torrent naming format in the docs to avoid this"
        );
    }
}

/// The `inject` command and job.
pub async fn inject_saved_torrents(
    pool: &SqlitePool,
    config: &RuntimeConfig,
    ignore_titles_override: Option<bool>,
    inject_dir_override: Option<String>,
) -> InjectSummary {
    let ignore_titles = ignore_titles_override
        .or(config.ignore_titles)
        .unwrap_or(false);
    let target_dir = inject_dir_override
        .or_else(|| config.inject_dir.clone())
        .unwrap_or_else(|| config.output_dir.clone());
    let target_dir = PathBuf::from(target_dir);

    if config.inject_dir.is_some() {
        tracing::warn!(
            label = Label::Inject.as_str(),
            "Manually injecting torrents performs minimal filtering which slightly increases chances of false positives, see the docs for more info"
        );
    }
    if ignore_titles {
        tracing::warn!(
            label = Label::Inject.as_str(),
            "Ignoring torrent titles when looking for matches, this may result in false positives"
        );
    }

    let torrent_file_paths = find_all_torrent_files_in_dir(&target_dir)
        .await
        .unwrap_or_default();
    if torrent_file_paths.is_empty() {
        tracing::info!(
            label = Label::Inject.as_str(),
            "No torrent files are awaiting injection in {}",
            target_dir.display()
        );
        return InjectSummary::default();
    }
    tracing::info!(
        label = Label::Inject.as_str(),
        "Found {} torrent file(s) to inject in {}",
        torrent_file_paths.len(),
        target_dir.display()
    );

    let mut summary = InjectSummary {
        total: torrent_file_paths.len(),
        ..Default::default()
    };
    let searchees = crate::pipeline::find_all_searchees(pool, SearcheeLabel::Inject, config).await;

    for torrent_file_path in &torrent_file_paths {
        inject_saved_torrent(
            pool,
            torrent_file_path,
            &mut summary,
            &searchees,
            config,
            ignore_titles,
        )
        .await;
    }

    log_inject_summary(&summary, config.flat_linking);
    summary
}

/// The `restore` command: copies the torrent cache back into `outputDir` so
/// `inject` can try to re-seed everything crust-seed ever snatched.
pub async fn restore_from_torrent_cache(pool: &SqlitePool, config: &RuntimeConfig) {
    let cache_dir = torrent_cache_dir();
    let torrent_file_paths = find_all_torrent_files_in_dir(&cache_dir)
        .await
        .unwrap_or_default();
    if torrent_file_paths.is_empty() {
        tracing::info!("No .torrent files found to restore from cache");
        return;
    }
    tracing::info!(
        "Found {} .torrent files to restore from cache",
        torrent_file_paths.len()
    );

    let host_to_name = get_host_to_name_map(pool).await.unwrap_or_default();
    let output_dir = Path::new(&config.output_dir);
    let _ = tokio::fs::create_dir_all(output_dir).await;

    let mut copied = 0usize;
    let mut missing_metadata = 0usize;

    for torrent_file_path in &torrent_file_paths {
        let bytes = match tokio::fs::read(torrent_file_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!("Failed to read {}: {e}", torrent_file_path.display());
                continue;
            }
        };

        let (dest_path, had_tracker) = match Metafile::decode(&bytes) {
            Ok(meta) => {
                let tracker_host = meta.trackers.iter().find(|h| host_to_name.contains_key(*h));
                let tracker = tracker_host
                    .and_then(|host| host_to_name.get(host).cloned())
                    .unwrap_or_else(|| UNKNOWN_TRACKER.to_string());
                (
                    get_torrent_save_path(
                        &meta,
                        media_type_of(&meta.title, &meta.files),
                        &tracker,
                        output_dir,
                        true,
                    ),
                    tracker_host.is_some(),
                )
            }
            Err(e) => {
                // Unparseable cache entries are still worth restoring; the
                // filename just carries no metadata.
                tracing::error!(
                    "Failure when processing {}, filename metadata will be unknown: {e}",
                    torrent_file_path.display()
                );
                let base = torrent_file_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (
                    output_dir.join(format!(
                        "[{}][{UNKNOWN_TRACKER}]{base}",
                        MediaType::Other.as_str()
                    )),
                    false,
                )
            }
        };

        if !had_tracker {
            missing_metadata += 1;
        }
        if exists(&dest_path).await {
            continue;
        }
        match tokio::fs::copy(torrent_file_path, &dest_path).await {
            Ok(_) => {
                copied += 1;
                if copied.is_multiple_of(1000) {
                    tracing::info!("({copied}/{}) restored", torrent_file_paths.len());
                }
            }
            Err(e) => tracing::error!("Failed to copy {}: {e}", torrent_file_path.display()),
        }
    }

    tracing::info!(
        "Copied {copied}/{} .torrent files from cache to outputDir, run \"crust-seed inject\" to inject into client using your dataDirs",
        torrent_file_paths.len()
    );
    if missing_metadata > 0 {
        tracing::info!("Copied {missing_metadata} .torrent files without filename metadata");
    }
    tracing::warn!(
        "Some of the restored torrents may be unregistered, you will need to remove them from your client after injecting"
    );
}

/// `crust-seed diff` — explains why two torrents do or do not match.
pub async fn diff_torrents(a: &Path, b: &Path, config: &RuntimeConfig) -> Result<String, String> {
    let searchee_bytes = tokio::fs::read(a).await.map_err(|e| e.to_string())?;
    let candidate_bytes = tokio::fs::read(b).await.map_err(|e| e.to_string())?;
    let searchee_meta = Metafile::decode(&searchee_bytes).map_err(|e| e.to_string())?;
    let candidate_meta = Metafile::decode(&candidate_bytes).map_err(|e| e.to_string())?;

    let searchee = crate::searchee::searchee_from_metafile(&searchee_meta)?;
    let assessment = assess_candidate(
        MetaOrCandidate::Meta(&candidate_meta),
        &searchee,
        &Default::default(),
        config,
    )
    .await;

    Ok(format!(
        "{} [{}]\n{} [{}]\n{}",
        searchee_meta.name,
        sanitize_info_hash(&searchee_meta.info_hash),
        candidate_meta.name,
        sanitize_info_hash(&candidate_meta.info_hash),
        assessment.decision.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::config::runtime::set_runtime_config;
    use crate::searchee::File;
    use crate::torrent::metafile::fixtures::multi_file_torrent;

    fn meta(name: &str, files: &[(&[&str], i64)]) -> Metafile {
        Metafile::decode(&multi_file_torrent(name, files)).unwrap()
    }

    fn searchee(title: &str, files: Vec<(&str, i64)>) -> Searchee {
        let files: Vec<File> = files
            .into_iter()
            .map(|(path, length)| File {
                name: Path::new(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                path: path.to_string(),
                length,
            })
            .collect();
        Searchee {
            info_hash: Some("searchee".into()),
            name: title.to_string(),
            title: title.to_string(),
            length: files.iter().map(|f| f.length).sum(),
            files,
            label: Some(SearcheeLabel::Inject),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_torrent_only_deletes_when_its_filename_proves_we_wrote_it() {
        let dir = tempfile::tempdir().unwrap();
        let ours = dir.path().join(
            "[episode][Tracker]Some.Show.S01E01[0123456789abcdef0123456789abcdef01234567].torrent",
        );
        let theirs = dir.path().join("random-download.torrent");
        tokio::fs::write(&ours, b"x").await.unwrap();
        tokio::fs::write(&theirs, b"x").await.unwrap();

        delete_torrent_file_if_safe(&ours).await;
        delete_torrent_file_if_safe(&theirs).await;

        assert!(!ours.exists(), "our own file should be deleted");
        assert!(theirs.exists(), "a user's file must never be deleted");
    }

    #[tokio::test]
    async fn matches_are_ranked_full_before_size_only_before_partial() {
        let mut config = default_runtime_config();
        config.match_mode = crate::constants::MatchMode::Partial;
        config.include_single_episodes = true;
        set_runtime_config(config.clone());

        let candidate = meta("Some.Show.S01", &[(&["a.mkv"], 100), (&["b.mkv"], 200)]);
        let perfect = searchee(
            "Some.Show.S01",
            vec![("Some.Show.S01/a.mkv", 100), ("Some.Show.S01/b.mkv", 200)],
        );
        let renamed = searchee(
            "Some.Show.S01",
            vec![("Other/x.mkv", 100), ("Other/y.mkv", 200)],
        );

        let (matches, _, _) =
            which_searchees_match_torrent(&candidate, &[renamed, perfect], &config, true).await;

        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].decision, Decision::Match);
        assert_eq!(matches[1].decision, Decision::MatchSizeOnly);
    }

    /// A title mismatch is reported so the caller keeps the file for a possible
    /// `--ignore-titles` retry, rather than deleting it as unmatchable.
    #[tokio::test]
    async fn a_title_mismatch_is_reported_rather_than_matched() {
        let mut config = default_runtime_config();
        config.include_single_episodes = true;
        set_runtime_config(config.clone());

        let candidate = meta("Some.Show.S01", &[(&["a.mkv"], 100)]);
        let wrong_title = searchee(
            "Totally.Unrelated.Thing.S01",
            vec![("Totally.Unrelated.Thing.S01/a.mkv", 100)],
        );

        let (matches, _, fuzzy_fail) = which_searchees_match_torrent(
            &candidate,
            std::slice::from_ref(&wrong_title),
            &config,
            false,
        )
        .await;
        assert!(matches.is_empty());
        assert!(fuzzy_fail);

        // …and --ignore-titles accepts it.
        let (matches, _, _) =
            which_searchees_match_torrent(&candidate, &[wrong_title], &config, true).await;
        assert_eq!(matches.len(), 1);
    }

    #[tokio::test]
    async fn blocklisted_searchees_are_reported_separately_from_no_matches() {
        let mut config = default_runtime_config();
        config.include_single_episodes = true;
        config.block_list = vec!["name:Some.Show".into()];
        set_runtime_config(config.clone());

        let candidate = meta("Some.Show.S01", &[(&["a.mkv"], 100)]);
        let blocked = searchee("Some.Show.S01", vec![("Some.Show.S01/a.mkv", 100)]);

        let (matches, found_blocked, _) =
            which_searchees_match_torrent(&candidate, &[blocked], &config, true).await;
        assert!(matches.is_empty());
        assert!(found_blocked);
    }

    #[tokio::test]
    async fn restoring_copies_the_cache_into_the_output_dir() {
        // CONFIG_DIR is process-global; serialise the tests that set it.
        let _guard = crate::config::runtime::config_test_guard_async().await;
        let pool = crate::db::test_pool().await;
        let cache = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        // Point the config dir at the temp cache for this test.
        unsafe { std::env::set_var("CONFIG_DIR", cache.path()) };
        tokio::fs::create_dir_all(cache.path().join("torrent_cache"))
            .await
            .unwrap();

        let torrent = meta("Movie.2020", &[(&["a.mkv"], 100)]);
        tokio::fs::write(
            cache
                .path()
                .join("torrent_cache")
                .join(format!("{}.cached.torrent", torrent.info_hash)),
            torrent.encode(),
        )
        .await
        .unwrap();

        let mut config = default_runtime_config();
        config.output_dir = output.path().to_string_lossy().into_owned();
        restore_from_torrent_cache(&pool, &config).await;

        let mut entries = tokio::fs::read_dir(output.path()).await.unwrap();
        let entry = entries.next_entry().await.unwrap();
        assert!(entry.is_some(), "expected a restored torrent");
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }
}
