//! SQLite access.
//!
//! Ported from `db.ts`. knex + better-sqlite3 becomes `sqlx` with hand-written
//! SQL; the schema itself lives in `migrations/` (see the header of
//! `0001_initial_schema.sql` for why the 18-step knex chain is collapsed).
//!
//! The pool is a process-global initialised once during startup, mirroring the
//! original's module-level `db` export. `journal_mode = WAL` is set for the
//! same reason it was there: the daemon reads while jobs write.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};

use crate::config::db_path;
use crate::errors::CrustSeedError;

static POOL: OnceLock<SqlitePool> = OnceLock::new();

/// Opens (creating if needed) the database at `<appDir>/crust-seed.db` and runs
/// pending migrations.
pub async fn init_db() -> Result<SqlitePool, CrustSeedError> {
    let path = db_path();
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // NORMAL is the standard companion to WAL: a crash can lose the last
        // transaction, which for a cache of search decisions is recoverable.
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(30));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .map_err(|e| {
            CrustSeedError::new(format!("Could not open database {}: {e}", path.display()))
        })?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|e| CrustSeedError::new(format!("Database migration failed: {e}")))?;

    let _ = POOL.set(pool.clone());
    Ok(pool)
}

/// Installs an already-built pool — used by tests to point at a temp file or
/// an in-memory database.
pub fn set_pool(pool: SqlitePool) {
    let _ = POOL.set(pool);
}

/// The process-global pool. Panics if called before [`init_db`]; every caller
/// runs after startup, matching the original's import-time initialisation.
pub fn db() -> &'static SqlitePool {
    POOL.get().expect("database not initialised")
}

/// An in-memory database with the schema applied, for tests.
#[cfg(test)]
pub async fn test_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await
        .expect("in-memory sqlite");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate in-memory sqlite");
    pool
}

// ─── Row types ──────────────────────────────────────────────────────────────
//
// These mirror `tables.d.ts` plus the tables it never declared (client_searchee,
// data, ensemble, user, session). JSON columns stay `Option<String>` here and
// are decoded by the module that owns them, exactly as knex handed back raw
// text.

#[derive(Debug, Clone, FromRow)]
pub struct SearcheeRow {
    pub id: i64,
    pub name: Option<String>,
    pub first_searched: Option<i64>,
    pub last_searched: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct DecisionRow {
    pub id: i64,
    pub searchee_id: Option<i64>,
    pub guid: Option<String>,
    pub info_hash: Option<String>,
    pub decision: Option<String>,
    pub first_seen: Option<i64>,
    pub last_seen: Option<i64>,
    pub fuzzy_size_factor: Option<f64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct IndexerRow {
    pub id: i64,
    pub name: Option<String>,
    pub url: String,
    pub apikey: Option<String>,
    pub trackers: Option<String>,
    pub enabled: bool,
    pub status: Option<String>,
    pub retry_after: Option<i64>,
    pub search_cap: Option<bool>,
    pub tv_search_cap: Option<bool>,
    pub movie_search_cap: Option<bool>,
    pub music_search_cap: Option<bool>,
    pub audio_search_cap: Option<bool>,
    pub book_search_cap: Option<bool>,
    pub tv_id_caps: Option<String>,
    pub movie_id_caps: Option<String>,
    pub cat_caps: Option<String>,
    pub limits_caps: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct TorrentRow {
    pub id: i64,
    pub info_hash: Option<String>,
    pub name: Option<String>,
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct JobLogRow {
    pub id: i64,
    pub name: Option<String>,
    pub last_run: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct TimestampRow {
    pub searchee_id: i64,
    pub indexer_id: i64,
    pub first_searched: Option<i64>,
    pub last_searched: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct SettingsRow {
    pub id: i64,
    pub apikey: Option<String>,
    pub settings_json: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct RssRow {
    pub indexer_id: i64,
    pub last_seen_guid: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct ClientSearcheeRow {
    pub client_host: String,
    pub info_hash: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub files: Option<String>,
    pub length: Option<i64>,
    pub save_path: Option<String>,
    pub category: Option<String>,
    pub tags: Option<String>,
    pub trackers: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct DataRow {
    pub path: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct EnsembleRow {
    pub client_host: String,
    pub path: String,
    pub info_hash: Option<String>,
    pub ensemble: Option<String>,
    pub element: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct SessionRow {
    pub id: String,
    pub user_id: i64,
    pub expires_at: i64,
    pub created_at: i64,
}

// ─── Small shared queries ───────────────────────────────────────────────────

/// Inserts a searchee name if absent and returns its id. Every search path
/// needs this, so it lives here rather than in `pipeline`.
pub async fn upsert_searchee(pool: &SqlitePool, name: &str) -> sqlx::Result<i64> {
    sqlx::query("INSERT OR IGNORE INTO searchee (name) VALUES (?)")
        .bind(name)
        .execute(pool)
        .await?;
    let id: i64 = sqlx::query_scalar("SELECT id FROM searchee WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await?;
    Ok(id)
}

/// SQLite has a 999-variable limit on prepared statements, so bulk `IN (...)`
/// and multi-row inserts are chunked. The original chunked at 100 for the same
/// reason (`inBatches`).
pub const BATCH_SIZE: usize = 100;

pub fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_produce_every_table() {
        let pool = test_pool().await;
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&pool)
                .await
                .unwrap();
        for expected in [
            "searchee",
            "indexer",
            "decision",
            "torrent",
            "job_log",
            "timestamp",
            "settings",
            "rss",
            "client_searchee",
            "data",
            "ensemble",
            "user",
            "session",
        ] {
            assert!(tables.iter().any(|t| t == expected), "missing {expected}");
        }
    }

    /// Migration 04 seeded the single settings row; the API-key lookup assumes
    /// it exists.
    #[tokio::test]
    async fn settings_row_is_seeded() {
        let pool = test_pool().await;
        let row: SettingsRow = sqlx::query_as("SELECT * FROM settings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.id, 0);
        assert!(row.settings_json.is_none());
    }

    /// Migration 06 added this constraint to stop duplicate decisions.
    #[tokio::test]
    async fn decision_is_unique_per_searchee_and_guid() {
        let pool = test_pool().await;
        let searchee_id = upsert_searchee(&pool, "Some.Show.S01E01").await.unwrap();
        let insert = |guid: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO decision (searchee_id, guid, decision) VALUES (?, ?, 'MATCH')",
                )
                .bind(searchee_id)
                .bind(guid)
                .execute(&pool)
                .await
            }
        };
        insert("guid-1").await.unwrap();
        assert!(insert("guid-1").await.is_err());
        insert("guid-2").await.unwrap();
    }

    /// Migration 15 dropped the unique constraint on indexer.url.
    #[tokio::test]
    async fn indexer_url_is_not_unique() {
        let pool = test_pool().await;
        for key in ["key-a", "key-b"] {
            sqlx::query("INSERT INTO indexer (url, apikey) VALUES ('https://x.example/api', ?)")
                .bind(key)
                .execute(&pool)
                .await
                .unwrap();
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexer")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn upsert_searchee_is_idempotent() {
        let pool = test_pool().await;
        let first = upsert_searchee(&pool, "Movie.2020").await.unwrap();
        let second = upsert_searchee(&pool, "Movie.2020").await.unwrap();
        assert_eq!(first, second);
    }
}
