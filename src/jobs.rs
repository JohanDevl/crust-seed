//! The scheduler.
//!
//! Ported from `jobs.ts` plus `cleanupDB` from `db.ts`.
//!
//! Jobs are cadence-based rather than cron-based: each records its last run in
//! `job_log` and becomes eligible once `cadence` has elapsed. The loop ticks
//! once a minute, which is why a cadence shorter than that is meaningless.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock};

use crate::config::runtime::get_runtime_config;
use crate::config::{ConfigOverrides, RuntimeConfig};
use crate::constants::Action;
use crate::logger::Label;
use crate::utils::{human_readable_date, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JobName {
    Rss,
    Search,
    #[serde(rename = "updateIndexerCaps")]
    UpdateIndexerCaps,
    Inject,
    Cleanup,
}

impl JobName {
    pub fn as_str(self) -> &'static str {
        match self {
            JobName::Rss => "rss",
            JobName::Search => "search",
            JobName::UpdateIndexerCaps => "updateIndexerCaps",
            JobName::Inject => "inject",
            JobName::Cleanup => "cleanup",
        }
    }

    pub fn from_str_exact(s: &str) -> Option<JobName> {
        match s {
            "rss" => Some(JobName::Rss),
            "search" => Some(JobName::Search),
            "updateIndexerCaps" => Some(JobName::UpdateIndexerCaps),
            "inject" => Some(JobName::Inject),
            "cleanup" => Some(JobName::Cleanup),
            _ => None,
        }
    }

    pub const ALL: [JobName; 5] = [
        JobName::Rss,
        JobName::Search,
        JobName::UpdateIndexerCaps,
        JobName::Inject,
        JobName::Cleanup,
    ];
}

/// Mutable per-job state shared with the API routes that trigger runs.
#[derive(Debug, Default)]
pub struct JobState {
    pub is_active: bool,
    pub run_ahead_of_schedule: bool,
    pub config_override: ConfigOverrides,
}

pub struct Job {
    pub name: JobName,
    pub cadence_ms: i64,
    pub state: Mutex<JobState>,
}

static JOBS: LazyLock<RwLock<Vec<Arc<Job>>>> = LazyLock::new(|| RwLock::new(Vec::new()));
/// Serialises `check_jobs`, so a manual trigger cannot race the timer.
static CHECK_JOBS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Builds the job list from the configuration.
///
/// RSS and search only exist when their cadence is set; inject only when the
/// action is `inject`. That is what makes "unable to run, disabled in config"
/// the right answer for a trigger request naming a missing job.
pub async fn create_jobs(config: &RuntimeConfig) {
    let mut jobs: Vec<Arc<Job>> = Vec::new();
    let push = |jobs: &mut Vec<Arc<Job>>, name: JobName, cadence_ms: i64| {
        jobs.push(Arc::new(Job {
            name,
            cadence_ms,
            state: Mutex::new(JobState::default()),
        }));
    };

    if let Some(cadence) = config.rss_cadence.filter(|c| *c > 0) {
        push(&mut jobs, JobName::Rss, cadence);
    }
    if let Some(cadence) = config.search_cadence.filter(|c| *c > 0) {
        push(&mut jobs, JobName::Search, cadence);
    }
    push(&mut jobs, JobName::UpdateIndexerCaps, 24 * 60 * 60 * 1000);
    if config.action == Action::Inject {
        push(&mut jobs, JobName::Inject, 60 * 60 * 1000);
    }
    push(&mut jobs, JobName::Cleanup, 24 * 60 * 60 * 1000);

    *JOBS.write().await = jobs;
}

pub async fn get_jobs() -> Vec<Arc<Job>> {
    JOBS.read().await.clone()
}

pub async fn find_job(name: JobName) -> Option<Arc<Job>> {
    JOBS.read().await.iter().find(|j| j.name == name).cloned()
}

pub async fn get_job_last_run(pool: &SqlitePool, name: JobName) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>("SELECT last_run FROM job_log WHERE name = ?")
        .bind(name.as_str())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

async fn record_job_run(pool: &SqlitePool, name: JobName, at: i64) {
    let _ = sqlx::query(
        "INSERT INTO job_log (name, last_run) VALUES (?, ?)
         ON CONFLICT (name) DO UPDATE SET last_run = excluded.last_run",
    )
    .bind(name.as_str())
    .bind(at)
    .execute(pool)
    .await;
}

/// Whether a job is due, given its cadence and last run.
pub fn is_eligible(last_run: Option<i64>, cadence_ms: i64, now: i64) -> bool {
    match last_run {
        None => true,
        Some(last_run) => now >= last_run + cadence_ms,
    }
}

async fn run_job(pool: &SqlitePool, job: &Job, config_override: ConfigOverrides) {
    let config = crate::config::runtime::get_runtime_config_with(&config_override);
    tracing::info!(
        label = Label::Scheduler.as_str(),
        "starting job: {}",
        job.name.as_str()
    );

    match job.name {
        JobName::Rss => {
            let last_run = get_job_last_run(pool, JobName::Rss).await.unwrap_or(0);
            crate::pipeline::scan_rss_feeds(pool, &config, last_run).await;
        }
        JobName::Search => {
            crate::pipeline::bulk_search(pool, &config).await;
        }
        JobName::UpdateIndexerCaps => {
            let _ = crate::torznab::update_caps(pool).await;
        }
        JobName::Inject => {
            crate::inject::inject_saved_torrents(pool, &config, None, None).await;
        }
        JobName::Cleanup => {
            cleanup_db(pool, &config).await;
        }
    }
}

/// Runs every eligible job.
///
/// Two ordering rules carry over: nothing else starts while RSS is running
/// (announce-adjacent work is latency-sensitive), and cleanup waits until
/// everything else is idle (it deletes cache entries the others may be using).
pub async fn check_jobs(pool: &SqlitePool, is_first_run: bool) {
    let _guard = CHECK_JOBS_LOCK.lock().await;
    let now = now_ms();
    let jobs = get_jobs().await;

    let rss_active = match jobs.iter().find(|j| j.name == JobName::Rss) {
        Some(rss) => rss.state.lock().await.is_active,
        None => false,
    };
    let mut any_active = false;
    for job in &jobs {
        if job.state.lock().await.is_active {
            any_active = true;
            break;
        }
    }

    for job in jobs {
        let last_run = get_job_last_run(pool, job.name).await;
        if is_first_run {
            log_next_run(job.name, job.cadence_ms, last_run, now);
        }

        let (should_run, config_override) = {
            let mut state = job.state.lock().await;
            if state.is_active {
                continue;
            }
            if !state.run_ahead_of_schedule {
                if rss_active {
                    continue;
                }
                if job.name == JobName::Cleanup && any_active {
                    continue;
                }
                if !is_eligible(last_run, job.cadence_ms, now) {
                    continue;
                }
            }
            state.is_active = true;
            let overrides = std::mem::take(&mut state.config_override);
            state.run_ahead_of_schedule = false;
            (true, overrides)
        };
        if !should_run {
            continue;
        }

        let pool = pool.clone();
        let job = job.clone();
        tokio::spawn(async move {
            run_job(&pool, &job, config_override).await;
            record_job_run(&pool, job.name, now).await;
            log_next_run(job.name, job.cadence_ms, Some(now), now_ms());
            job.state.lock().await.is_active = false;
        });
    }
}

fn log_next_run(name: JobName, cadence_ms: i64, last_run: Option<i64>, now: i64) {
    let eligibility = last_run.map(|l| l + cadence_ms).unwrap_or(now);
    let last_run_str = match last_run {
        None => "never".to_string(),
        Some(last_run) if now >= last_run => format!(
            "{} ago",
            crate::config::duration::format_ms_long(now - last_run)
        ),
        Some(last_run) => format!("at {}", human_readable_date(last_run - cadence_ms)),
    };
    let next_run_str = if now >= eligibility {
        "now".to_string()
    } else {
        format!(
            "in {}",
            crate::config::duration::format_ms_long(eligibility - now)
        )
    };
    tracing::info!(
        label = Label::Scheduler.as_str(),
        "{}: last run {last_run_str}, next run {next_run_str}",
        name.as_str()
    );
}

/// Marks a job to run on the next tick.
pub async fn trigger_job(name: JobName, config_override: ConfigOverrides) -> Result<(), String> {
    let Some(job) = find_job(name).await else {
        return Err(format!(
            "{}: unable to run, disabled in config",
            name.as_str()
        ));
    };
    let mut state = job.state.lock().await;
    if state.is_active {
        return Err(format!("{}: already running", name.as_str()));
    }
    state.run_ahead_of_schedule = true;
    state.config_override = config_override;
    Ok(())
}

/// The daemon's scheduler loop.
pub async fn jobs_loop(pool: SqlitePool) {
    create_jobs(&get_runtime_config()).await;
    check_jobs(&pool, true).await;

    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
    ticker.tick().await; // the first tick completes immediately
    loop {
        ticker.tick().await;
        check_jobs(&pool, false).await;
    }
}

// ─── Cleanup ────────────────────────────────────────────────────────────────

/// The cleanup job.
///
/// Prunes the four things that go stale: data/ensemble rows for deleted paths,
/// the snatch-failure history, torrent-cache files nothing references any more,
/// and decision rows whose cached torrent is gone.
pub async fn cleanup_db(pool: &SqlitePool, config: &RuntimeConfig) {
    // Re-read every client torrent: names, save paths, categories and tags all
    // drift, and the cached rows are what searches and the blocklist read.
    if config.use_client_torrents {
        tracing::debug!(
            label = Label::Cleanup.as_str(),
            "Refreshing all client torrents..."
        );
        let mut searchees = Vec::new();
        for client in crate::clients::get_clients() {
            let options = crate::clients::GetSearcheesOptions {
                refresh: Some(Vec::new()),
                include_files: true,
                include_trackers: true,
                ..Default::default()
            };
            match client.get_client_searchees(options).await {
                Ok(result) => searchees.extend(result.searchees),
                Err(e) => tracing::error!(
                    label = Label::Cleanup.as_str(),
                    "Failed to refresh {}: {e}",
                    client.label()
                ),
            }
        }
        if config.season_from_episodes_ratio().is_some() {
            tracing::debug!(
                label = Label::Cleanup.as_str(),
                "Refreshing all ensemble torrents..."
            );
            let mut rows = Vec::new();
            for searchee in &searchees {
                if let Some(entries) =
                    crate::torrent::index::cache_ensemble_torrent_entry(searchee, None).await
                {
                    rows.extend(entries);
                }
            }
            let _ = crate::torrent::index::upsert_ensemble_rows(pool, &rows).await;
        }
    }

    tracing::debug!(
        label = Label::Cleanup.as_str(),
        "Pruning deleted dataDirs entries..."
    );
    let _ = crate::data_files::prune_missing_data_paths(pool).await;

    tracing::debug!(
        label = Label::Cleanup.as_str(),
        "Pruning failed snatch history entries..."
    );
    let pruned = crate::torrent::snatch::prune_snatch_history(24 * 60 * 60 * 1000);
    if !pruned.is_empty() {
        tracing::debug!(
            label = Label::Cleanup.as_str(),
            "Dropped {} snatch history entries",
            pruned.len()
        );
    }

    tracing::debug!(
        label = Label::Cleanup.as_str(),
        "Pruning unused torrent cache entries..."
    );
    prune_torrent_cache(pool, config).await;

    tracing::debug!(
        label = Label::Cleanup.as_str(),
        "Pruning invalid decision entries..."
    );
    prune_invalid_decisions(pool).await;

    let _ = crate::decide::rebuild_guid_info_hash_map(pool).await;
}

/// Deletes cached torrents that have not been referenced by a decision within
/// the retention window.
///
/// The window is at least a year, but is extended to `excludeRecentSearch`
/// plus 30 days when that is longer — otherwise a user with a long
/// `excludeRecentSearch` would lose cache entries just before they are needed.
async fn prune_torrent_cache(pool: &SqlitePool, config: &RuntimeConfig) {
    const ONE_YEAR_MS: i64 = 365 * 24 * 60 * 60 * 1000;
    const THIRTY_DAYS_MS: i64 = 30 * 24 * 60 * 60 * 1000;

    let exclude_cutoff = config.exclude_recent_search.unwrap_or(0) + THIRTY_DAYS_MS;
    let cutoff_ms = ONE_YEAR_MS.max(exclude_cutoff);

    let cache_dir = crate::config::torrent_cache_dir();
    let Ok(paths) = crate::torrent::cache::find_all_torrent_files_in_dir(&cache_dir).await else {
        return;
    };
    if paths.is_empty() {
        return;
    }

    let rows: Vec<(Option<String>, Option<i64>)> =
        sqlx::query_as("SELECT info_hash, last_seen FROM decision")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    let mut last_seen: HashMap<String, i64> = HashMap::new();
    for (info_hash, seen) in rows {
        let (Some(info_hash), Some(seen)) = (info_hash, seen) else {
            continue;
        };
        last_seen
            .entry(info_hash)
            .and_modify(|current| *current = (*current).max(seen))
            .or_insert(seen);
    }
    if last_seen.is_empty() {
        return;
    }

    let now = now_ms();
    let mut deleted: Vec<String> = Vec::new();
    for path in paths {
        let info_hash = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split('.').next())
            .unwrap_or_default()
            .to_string();
        if let Some(seen) = last_seen.get(&info_hash)
            && now - seen <= cutoff_ms
        {
            continue;
        }
        if tokio::fs::remove_file(&path).await.is_ok() {
            deleted.push(info_hash);
        }
    }

    for batch in deleted.chunks(crate::db::BATCH_SIZE) {
        let placeholders = crate::db::placeholders(batch.len());
        let sql = format!("DELETE FROM decision WHERE info_hash IN ({placeholders})");
        let mut query = sqlx::query(&sql);
        for info_hash in batch {
            query = query.bind(info_hash);
        }
        let _ = query.execute(pool).await;
    }
}

/// Removes decision rows whose cached torrent no longer exists.
///
/// If *every* row would be deleted the prune is skipped: that means the cache
/// directory is missing or unreadable, not that every torrent went away, and
/// wiping the table would throw away the whole search history.
async fn prune_invalid_decisions(pool: &SqlitePool) {
    let _ = sqlx::query("DELETE FROM decision WHERE info_hash IS NULL")
        .execute(pool)
        .await;

    let info_hashes: Vec<String> =
        sqlx::query_scalar::<_, Option<String>>("SELECT info_hash FROM decision")
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect();
    if info_hashes.is_empty() {
        return;
    }

    let cache_dir = crate::config::torrent_cache_dir();
    let Ok(paths) = crate::torrent::cache::find_all_torrent_files_in_dir(&cache_dir).await else {
        return;
    };
    if paths.is_empty() {
        return;
    }
    let present: std::collections::HashSet<String> = paths
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .map(|n| n.split('.').next().unwrap_or_default().to_string())
        .collect();

    let missing: Vec<String> = info_hashes
        .iter()
        .filter(|info_hash| !present.contains(*info_hash))
        .cloned()
        .collect();
    if missing.len() == info_hashes.len() {
        tracing::debug!(
            label = Label::Cleanup.as_str(),
            "All decision entries are invalid - skipping deletion to avoid catastrophic data loss"
        );
        return;
    }

    for batch in missing.chunks(crate::db::BATCH_SIZE) {
        let placeholders = crate::db::placeholders(batch.len());
        let sql = format!("DELETE FROM decision WHERE info_hash IN ({placeholders})");
        let mut query = sqlx::query(&sql);
        for info_hash in batch {
            query = query.bind(info_hash);
        }
        let _ = query.execute(pool).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::db::test_pool;

    #[tokio::test]
    async fn jobs_exist_only_when_their_config_enables_them() {
        let mut config = default_runtime_config();
        config.rss_cadence = None;
        config.search_cadence = None;
        config.action = Action::Save;
        create_jobs(&config).await;

        let names: Vec<JobName> = get_jobs().await.iter().map(|j| j.name).collect();
        assert_eq!(names, vec![JobName::UpdateIndexerCaps, JobName::Cleanup]);

        config.rss_cadence = Some(60_000);
        config.search_cadence = Some(86_400_000);
        config.action = Action::Inject;
        create_jobs(&config).await;
        let names: Vec<JobName> = get_jobs().await.iter().map(|j| j.name).collect();
        assert!(names.contains(&JobName::Rss));
        assert!(names.contains(&JobName::Search));
        assert!(names.contains(&JobName::Inject));
    }

    #[test]
    fn eligibility_follows_the_cadence() {
        let now = 1_000_000;
        assert!(is_eligible(None, 1000, now));
        assert!(is_eligible(Some(now - 1000), 1000, now));
        assert!(!is_eligible(Some(now - 999), 1000, now));
    }

    #[tokio::test]
    async fn triggering_a_disabled_job_reports_it() {
        let mut config = default_runtime_config();
        config.rss_cadence = None;
        create_jobs(&config).await;

        let error = trigger_job(JobName::Rss, Default::default())
            .await
            .unwrap_err();
        assert!(error.contains("disabled in config"));
    }

    #[tokio::test]
    async fn triggering_sets_the_run_ahead_flag_and_overrides() {
        let mut config = default_runtime_config();
        config.rss_cadence = Some(60_000);
        create_jobs(&config).await;

        let overrides =
            crate::config::runtime::overrides_from([("excludeRecentSearch", serde_json::json!(1))]);
        trigger_job(JobName::Rss, overrides).await.unwrap();

        let job = find_job(JobName::Rss).await.unwrap();
        let state = job.state.lock().await;
        assert!(state.run_ahead_of_schedule);
        assert!(state.config_override.contains_key("excludeRecentSearch"));
    }

    #[tokio::test]
    async fn job_runs_are_recorded_and_read_back() {
        let pool = test_pool().await;
        assert_eq!(get_job_last_run(&pool, JobName::Search).await, None);
        record_job_run(&pool, JobName::Search, 12345).await;
        assert_eq!(get_job_last_run(&pool, JobName::Search).await, Some(12345));
        record_job_run(&pool, JobName::Search, 23456).await;
        assert_eq!(get_job_last_run(&pool, JobName::Search).await, Some(23456));
    }

    #[tokio::test]
    async fn decisions_without_an_info_hash_are_pruned() {
        let pool = test_pool().await;
        let searchee_id = crate::db::upsert_searchee(&pool, "Some.Show")
            .await
            .unwrap();
        sqlx::query("INSERT INTO decision (searchee_id, guid, decision) VALUES (?, 'g', 'MATCH')")
            .bind(searchee_id)
            .execute(&pool)
            .await
            .unwrap();

        prune_invalid_decisions(&pool).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decision")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    /// If the cache directory is missing, every row looks invalid — deleting
    /// them all would throw away the entire search history.
    #[tokio::test]
    async fn a_missing_cache_directory_does_not_wipe_the_decision_table() {
        // CONFIG_DIR is process-global; serialise the tests that set it.
        let _guard = crate::config::runtime::config_test_guard_async().await;
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CONFIG_DIR", dir.path()) };

        let pool = test_pool().await;
        let searchee_id = crate::db::upsert_searchee(&pool, "Some.Show")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO decision (searchee_id, guid, info_hash, decision) VALUES (?, 'g', 'hash', 'MATCH')",
        )
        .bind(searchee_id)
        .execute(&pool)
        .await
        .unwrap();

        prune_invalid_decisions(&pool).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decision")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }
}
