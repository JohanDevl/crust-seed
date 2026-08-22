//! Indexer records: the Torznab endpoints cross-seed searches.
//!
//! Ported from `indexers.ts` and the persistence half of
//! `services/indexerService.ts`.
//!
//! The `indexer` table stores capabilities ("caps") as JSON text columns; they
//! are decoded into typed structs on the way out, exactly where the original's
//! `deserialize()` did the `JSON.parse`.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::db::IndexerRow;
use crate::logger::Label;
use crate::problems::{Problem, ProblemSeverity};
use crate::utils::{human_readable_date, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexerStatus {
    /// Equivalent to a NULL status.
    #[serde(rename = "OK")]
    Ok,
    #[serde(rename = "RATE_LIMITED")]
    RateLimited,
    #[serde(rename = "UNKNOWN_ERROR")]
    UnknownError,
}

impl IndexerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            IndexerStatus::Ok => "OK",
            IndexerStatus::RateLimited => "RATE_LIMITED",
            IndexerStatus::UnknownError => "UNKNOWN_ERROR",
        }
    }

    pub fn from_str_exact(s: &str) -> Option<IndexerStatus> {
        match s {
            "OK" => Some(IndexerStatus::Ok),
            "RATE_LIMITED" => Some(IndexerStatus::RateLimited),
            "UNKNOWN_ERROR" => Some(IndexerStatus::UnknownError),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IndexerCategories {
    pub tv: bool,
    pub movie: bool,
    pub anime: bool,
    pub xxx: bool,
    pub audio: bool,
    pub book: bool,
    /// The indexer has a category not covered by the fields above.
    pub additional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerLimits {
    pub default: i64,
    pub max: i64,
}

impl Default for IndexerLimits {
    fn default() -> Self {
        IndexerLimits {
            default: 100,
            max: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdSearchCaps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tvdb_id: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb_id: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb_id: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tv_maze_id: Option<bool>,
}

impl IdSearchCaps {
    pub fn all() -> Self {
        IdSearchCaps {
            tvdb_id: Some(true),
            tmdb_id: Some(true),
            imdb_id: Some(true),
            tv_maze_id: Some(true),
        }
    }

    pub fn any(&self) -> bool {
        [self.tvdb_id, self.tmdb_id, self.imdb_id, self.tv_maze_id]
            .iter()
            .any(|c| c.unwrap_or(false))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub search: bool,
    pub tv_search: bool,
    pub movie_search: bool,
    pub music_search: bool,
    pub audio_search: bool,
    pub book_search: bool,
    pub movie_id_search: IdSearchCaps,
    pub tv_id_search: IdSearchCaps,
    pub categories: IndexerCategories,
    pub limits: IndexerLimits,
}

impl Caps {
    /// `ALL_CAPS` — the optimistic assumption used when an indexer's caps
    /// endpoint cannot be read but the user wants to search anyway.
    pub fn all() -> Self {
        Caps {
            search: true,
            tv_search: true,
            movie_search: true,
            music_search: true,
            audio_search: true,
            book_search: true,
            movie_id_search: IdSearchCaps::all(),
            tv_id_search: IdSearchCaps::all(),
            categories: IndexerCategories {
                tv: true,
                movie: true,
                anime: true,
                xxx: true,
                audio: true,
                book: true,
                additional: true,
            },
            limits: IndexerLimits {
                default: 100,
                max: 100,
            },
        }
    }
}

/// An indexer as the rest of the app (and the web UI) sees it.
///
/// Field names are the camelCase keys the `indexers.getAll` tRPC response uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Indexer {
    pub id: i64,
    pub name: Option<String>,
    pub url: String,
    pub apikey: String,
    pub trackers: Option<Vec<String>>,
    /// When false the user disabled it: excluded from search and RSS, but still
    /// usable for caps refresh and `restore`. Mirrors Prowlarr's enabled bit.
    pub enabled: bool,
    pub status: Option<IndexerStatus>,
    pub retry_after: Option<i64>,
    pub search_cap: bool,
    pub tv_search_cap: bool,
    pub movie_search_cap: bool,
    pub music_search_cap: bool,
    pub audio_search_cap: bool,
    pub book_search_cap: bool,
    pub tv_id_caps: Option<IdSearchCaps>,
    pub movie_id_caps: Option<IdSearchCaps>,
    pub categories: Option<IndexerCategories>,
    pub limits: Option<IndexerLimits>,
}

fn parse_json<T: for<'de> Deserialize<'de>>(raw: Option<&str>) -> Option<T> {
    raw.filter(|s| !s.is_empty() && *s != "null")
        .and_then(|s| serde_json::from_str(s).ok())
}

/// `deserialize(dbIndexer)`.
pub fn deserialize(row: &IndexerRow) -> Indexer {
    Indexer {
        id: row.id,
        name: row.name.clone(),
        url: row.url.clone(),
        apikey: row.apikey.clone().unwrap_or_default(),
        trackers: parse_json(row.trackers.as_deref()),
        enabled: row.enabled,
        status: row
            .status
            .as_deref()
            .and_then(IndexerStatus::from_str_exact),
        retry_after: row.retry_after,
        search_cap: row.search_cap.unwrap_or(false),
        tv_search_cap: row.tv_search_cap.unwrap_or(false),
        movie_search_cap: row.movie_search_cap.unwrap_or(false),
        music_search_cap: row.music_search_cap.unwrap_or(false),
        audio_search_cap: row.audio_search_cap.unwrap_or(false),
        book_search_cap: row.book_search_cap.unwrap_or(false),
        tv_id_caps: parse_json(row.tv_id_caps.as_deref()),
        movie_id_caps: parse_json(row.movie_id_caps.as_deref()),
        categories: parse_json(row.cat_caps.as_deref()),
        limits: parse_json(row.limits_caps.as_deref()),
    }
}

/// Every indexer the user has configured, working or not.
pub async fn get_all_indexers(pool: &SqlitePool) -> sqlx::Result<Vec<Indexer>> {
    let rows: Vec<IndexerRow> = sqlx::query_as("SELECT * FROM indexer")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(deserialize).collect())
}

/// Indexers that are enabled, have their caps populated, can search, and are
/// not currently snoozed.
pub async fn get_enabled_indexers(pool: &SqlitePool) -> sqlx::Result<Vec<Indexer>> {
    let rows: Vec<IndexerRow> = sqlx::query_as(
        r#"
        SELECT * FROM indexer
        WHERE enabled = 1
          AND search_cap = 1
          AND tv_search_cap    IS NOT NULL
          AND movie_search_cap IS NOT NULL
          AND music_search_cap IS NOT NULL
          AND audio_search_cap IS NOT NULL
          AND book_search_cap  IS NOT NULL
          AND tv_id_caps       IS NOT NULL
          AND movie_id_caps    IS NOT NULL
          AND cat_caps         IS NOT NULL
          AND limits_caps      IS NOT NULL
          AND (status IS NULL OR status = 'OK' OR retry_after < ?)
        "#,
    )
    .bind(now_ms())
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(deserialize).collect())
}

pub async fn get_indexer_by_id(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<Indexer>> {
    let row: Option<IndexerRow> = sqlx::query_as("SELECT * FROM indexer WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(deserialize))
}

/// Snoozes indexers until `retry_after`.
pub async fn update_indexer_status(
    pool: &SqlitePool,
    status: IndexerStatus,
    retry_after: i64,
    indexer_ids: &[i64],
    indexer_names: &[String],
) -> sqlx::Result<()> {
    if indexer_ids.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        label = Label::Torznab.as_str(),
        "Snoozing indexers [{}] with {} until {}",
        indexer_names.join(", "),
        status.as_str(),
        human_readable_date(retry_after)
    );

    let sql = format!(
        "UPDATE indexer SET retry_after = ?, status = ? WHERE id IN ({})",
        crate::db::placeholders(indexer_ids.len())
    );
    let mut query = sqlx::query(&sql).bind(retry_after).bind(status.as_str());
    for id in indexer_ids {
        query = query.bind(id);
    }
    query.execute(pool).await?;
    Ok(())
}

/// Records that `name` was searched against each indexer just now. `first_searched`
/// is only set on insert, so it keeps the original value on conflict.
pub async fn update_search_timestamps(
    pool: &SqlitePool,
    name: &str,
    indexer_ids: &[i64],
) -> sqlx::Result<()> {
    if indexer_ids.is_empty() {
        return Ok(());
    }
    let now = now_ms();
    let searchee_id: Option<i64> = sqlx::query_scalar("SELECT id FROM searchee WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    let Some(searchee_id) = searchee_id else {
        return Ok(());
    };

    let mut tx = pool.begin().await?;
    for indexer_id in indexer_ids {
        sqlx::query(
            r#"
            INSERT INTO timestamp (searchee_id, indexer_id, first_searched, last_searched)
            VALUES (?, ?, ?, ?)
            ON CONFLICT (searchee_id, indexer_id)
            DO UPDATE SET last_searched = excluded.last_searched
            "#,
        )
        .bind(searchee_id)
        .bind(indexer_id)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

pub async fn update_indexer_caps_by_id(
    pool: &SqlitePool,
    indexer_id: i64,
    caps: &Caps,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        UPDATE indexer SET
            search_cap = ?, tv_search_cap = ?, movie_search_cap = ?,
            music_search_cap = ?, audio_search_cap = ?, book_search_cap = ?,
            movie_id_caps = ?, tv_id_caps = ?, cat_caps = ?, limits_caps = ?
        WHERE id = ?
        "#,
    )
    .bind(caps.search)
    .bind(caps.tv_search)
    .bind(caps.movie_search)
    .bind(caps.music_search)
    .bind(caps.audio_search)
    .bind(caps.book_search)
    .bind(serde_json::to_string(&caps.movie_id_search).unwrap_or_default())
    .bind(serde_json::to_string(&caps.tv_id_search).unwrap_or_default())
    .bind(serde_json::to_string(&caps.categories).unwrap_or_default())
    .bind(serde_json::to_string(&caps.limits).unwrap_or_default())
    .bind(indexer_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_indexer_trackers(
    pool: &SqlitePool,
    indexer_id: i64,
    trackers: &[String],
) -> sqlx::Result<()> {
    sqlx::query("UPDATE indexer SET trackers = ? WHERE id = ?")
        .bind(serde_json::to_string(trackers).unwrap_or_default())
        .bind(indexer_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn clear_indexer_failures(pool: &SqlitePool) -> sqlx::Result<()> {
    sqlx::query("UPDATE indexer SET status = NULL, retry_after = NULL")
        .execute(pool)
        .await?;
    Ok(())
}

/// Maps a tracker host to the indexer name that announces it — used to label
/// a snatched torrent with the tracker it came from.
pub async fn get_host_to_name_map(
    pool: &SqlitePool,
) -> sqlx::Result<std::collections::HashMap<String, String>> {
    let mut host_to_name = std::collections::HashMap::new();
    for indexer in get_all_indexers(pool).await? {
        let (Some(name), Some(trackers)) = (indexer.name, indexer.trackers) else {
            continue;
        };
        for host in trackers {
            host_to_name.entry(host).or_insert_with(|| name.clone());
        }
    }
    Ok(host_to_name)
}

fn display_name(url: &str, name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_string(),
        None => url.to_string(),
    }
}

/// `collectIndexerProblems`.
pub async fn collect_indexer_problems(pool: &SqlitePool) -> Result<Vec<Problem>, String> {
    let indexers = get_all_indexers(pool).await.map_err(|e| e.to_string())?;
    let mut problems = Vec::new();

    if indexers.is_empty() {
        problems.push(Problem::new(
            "indexer:none-configured",
            ProblemSeverity::Error,
            "No indexers configured.",
            "Add at least one indexer so crust-seed can search for releases.",
        ));
        return Ok(problems);
    }

    if get_enabled_indexers(pool)
        .await
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        problems.push(Problem::new(
            "indexer:none-enabled",
            ProblemSeverity::Error,
            "All configured indexers are disabled.",
            "Enable at least one indexer so crust-seed can run searches.",
        ));
    }

    let now = now_ms();
    for indexer in &indexers {
        let name = display_name(&indexer.url, indexer.name.as_deref());

        if !indexer.search_cap {
            problems.push(Problem::new(
                format!("indexer:no-search-cap:{}", indexer.id),
                ProblemSeverity::Warning,
                format!("Indexer \"{name}\" does not support searching."),
                "Update the indexer's capabilities (caps) in Prowlarr/Jackett or disable it for searching.",
            ));
        }

        if indexer.status == Some(IndexerStatus::RateLimited)
            && indexer.retry_after.is_some_and(|retry| retry > now)
        {
            problems.push(Problem::new(
                format!("indexer:rate-limited:{}", indexer.id),
                ProblemSeverity::Warning,
                format!("Indexer \"{name}\" is rate limited."),
                format!(
                    "Crust-seed will retry after {}.",
                    human_readable_date(indexer.retry_after.unwrap_or(0))
                ),
            ));
        }

        if indexer.status == Some(IndexerStatus::UnknownError) {
            problems.push(Problem::new(
                format!("indexer:unknown-error:{}", indexer.id),
                ProblemSeverity::Warning,
                format!("Indexer \"{name}\" recently failed."),
                "Check logs for the underlying error and verify the indexer configuration.",
            ));
        }

        if !indexer.enabled {
            problems.push(Problem::new(
                format!("indexer:disabled:{}", indexer.id),
                ProblemSeverity::Info,
                format!("Indexer \"{name}\" is disabled."),
                "Re-enable the indexer if you want crust-seed to include it in searches.",
            ));
        }
    }

    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    async fn insert_indexer(pool: &SqlitePool, url: &str, enabled: bool) -> i64 {
        sqlx::query("INSERT INTO indexer (name, url, apikey, enabled) VALUES (?, ?, 'k', ?)")
            .bind(url)
            .bind(url)
            .bind(enabled)
            .execute(pool)
            .await
            .unwrap()
            .last_insert_rowid()
    }

    #[tokio::test]
    async fn enabled_indexers_require_populated_caps() {
        let pool = test_pool().await;
        let id = insert_indexer(&pool, "https://a.example/api", true).await;

        // Caps are NULL until the first refresh, so it is not yet usable.
        assert!(get_enabled_indexers(&pool).await.unwrap().is_empty());
        assert_eq!(get_all_indexers(&pool).await.unwrap().len(), 1);

        update_indexer_caps_by_id(&pool, id, &Caps::all())
            .await
            .unwrap();
        assert_eq!(get_enabled_indexers(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_snoozed_indexer_returns_once_its_retry_time_passes() {
        let pool = test_pool().await;
        let id = insert_indexer(&pool, "https://a.example/api", true).await;
        update_indexer_caps_by_id(&pool, id, &Caps::all())
            .await
            .unwrap();

        update_indexer_status(
            &pool,
            IndexerStatus::RateLimited,
            now_ms() + 60_000,
            &[id],
            &["a".into()],
        )
        .await
        .unwrap();
        assert!(get_enabled_indexers(&pool).await.unwrap().is_empty());

        update_indexer_status(
            &pool,
            IndexerStatus::RateLimited,
            now_ms() - 60_000,
            &[id],
            &["a".into()],
        )
        .await
        .unwrap();
        assert_eq!(get_enabled_indexers(&pool).await.unwrap().len(), 1);

        clear_indexer_failures(&pool).await.unwrap();
        let indexer = get_indexer_by_id(&pool, id).await.unwrap().unwrap();
        assert_eq!(indexer.status, None);
    }

    #[tokio::test]
    async fn disabled_indexers_are_excluded() {
        let pool = test_pool().await;
        let id = insert_indexer(&pool, "https://a.example/api", false).await;
        update_indexer_caps_by_id(&pool, id, &Caps::all())
            .await
            .unwrap();
        assert!(get_enabled_indexers(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn caps_round_trip_through_the_json_columns() {
        let pool = test_pool().await;
        let id = insert_indexer(&pool, "https://a.example/api", true).await;
        let mut caps = Caps::all();
        caps.book_search = false;
        caps.limits = IndexerLimits {
            default: 50,
            max: 200,
        };
        update_indexer_caps_by_id(&pool, id, &caps).await.unwrap();

        let indexer = get_indexer_by_id(&pool, id).await.unwrap().unwrap();
        assert!(!indexer.book_search_cap);
        assert_eq!(indexer.limits.unwrap().max, 200);
        assert!(indexer.categories.unwrap().anime);
    }

    #[tokio::test]
    async fn search_timestamps_keep_the_first_seen_value() {
        let pool = test_pool().await;
        let id = insert_indexer(&pool, "https://a.example/api", true).await;
        let searchee_id = crate::db::upsert_searchee(&pool, "Some.Show.S01E01")
            .await
            .unwrap();

        update_search_timestamps(&pool, "Some.Show.S01E01", &[id])
            .await
            .unwrap();
        let first: i64 =
            sqlx::query_scalar("SELECT first_searched FROM timestamp WHERE searchee_id = ?")
                .bind(searchee_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        update_search_timestamps(&pool, "Some.Show.S01E01", &[id])
            .await
            .unwrap();
        let row: (i64, i64) = sqlx::query_as(
            "SELECT first_searched, last_searched FROM timestamp WHERE searchee_id = ?",
        )
        .bind(searchee_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, first);
        assert!(row.1 >= first);
    }

    #[tokio::test]
    async fn problems_report_an_empty_indexer_list() {
        let pool = test_pool().await;
        let problems = collect_indexer_problems(&pool).await.unwrap();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].id, "indexer:none-configured");
    }

    #[tokio::test]
    async fn host_to_name_map_uses_the_first_indexer_per_host() {
        let pool = test_pool().await;
        let id = insert_indexer(&pool, "https://a.example/api", true).await;
        update_indexer_trackers(&pool, id, &["tracker.example".into()])
            .await
            .unwrap();
        let map = get_host_to_name_map(&pool).await.unwrap();
        assert_eq!(
            map.get("tracker.example").map(String::as_str),
            Some("https://a.example/api")
        );
    }
}
