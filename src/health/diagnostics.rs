//! SQLite space diagnostics for the health page.
//!
//! Ported from `diagnostics/db.ts`. The original opened a second read-only
//! connection with `node:sqlite`; here the same PRAGMAs run through the
//! existing pool.

use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbDiagnostics {
    pub path: String,
    pub sizes: DbSizes,
    pub page_size: Option<i64>,
    pub page_count: Option<i64>,
    pub freelist_count: Option<i64>,
    pub free_bytes: Option<i64>,
    pub free_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbstat_top: Option<Vec<DbStatRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dbstat_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DbSizes {
    pub db: Option<i64>,
    pub wal: Option<i64>,
    pub shm: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DbStatRow {
    pub name: String,
    pub bytes: i64,
    pub pages: i64,
}

async fn file_size(path: &std::path::Path) -> Option<i64> {
    tokio::fs::metadata(path).await.ok().map(|m| m.len() as i64)
}

pub async fn collect_db_diagnostics(pool: &SqlitePool) -> DbDiagnostics {
    let db_path = crate::config::db_path();
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");

    let mut diagnostics = DbDiagnostics {
        path: db_path.to_string_lossy().into_owned(),
        sizes: DbSizes {
            db: file_size(&db_path).await,
            wal: file_size(&wal_path).await,
            shm: file_size(&shm_path).await,
        },
        ..Default::default()
    };
    if diagnostics.sizes.db.is_none() {
        return diagnostics;
    }

    let pragma = |name: &'static str| async move {
        sqlx::query_scalar::<_, i64>(&format!("PRAGMA {name}"))
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
    };
    diagnostics.page_size = pragma("page_size").await;
    diagnostics.page_count = pragma("page_count").await;
    diagnostics.freelist_count = pragma("freelist_count").await;

    if let (Some(page_size), Some(freelist_count)) =
        (diagnostics.page_size, diagnostics.freelist_count)
    {
        diagnostics.free_bytes = Some(page_size * freelist_count);
    }
    if let (Some(page_size), Some(page_count), Some(free_bytes)) = (
        diagnostics.page_size,
        diagnostics.page_count,
        diagnostics.free_bytes,
    ) {
        let total = page_size * page_count;
        if total > 0 {
            diagnostics.free_percent = Some(free_bytes as f64 / total as f64 * 100.0);
        }
    }

    // dbstat is a compile-time option; its absence is reported, not fatal.
    match sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT name, SUM(pgsize) AS bytes, COUNT(*) AS pages
         FROM dbstat GROUP BY name ORDER BY bytes DESC LIMIT 10",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            diagnostics.dbstat_top = Some(
                rows.into_iter()
                    .map(|(name, bytes, pages)| DbStatRow { name, bytes, pages })
                    .collect(),
            );
        }
        Err(e) => diagnostics.dbstat_error = Some(e.to_string()),
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    #[tokio::test]
    async fn diagnostics_report_a_missing_database_file_without_erroring() {
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CONFIG_DIR", dir.path()) };
        let pool = test_pool().await;

        let diagnostics = collect_db_diagnostics(&pool).await;
        assert!(diagnostics.sizes.db.is_none());
        assert!(diagnostics.error.is_none());
        assert!(diagnostics.path.ends_with(".db"));
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }
}
