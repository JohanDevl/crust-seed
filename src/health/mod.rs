//! The health page's data sources.
//!
//! Ported from `problems/path.ts`, `problems/linking.ts` and
//! `diagnostics/db.ts`.

pub mod diagnostics;
pub mod linking;
pub mod paths;

use sqlx::SqlitePool;

use crate::problems::{Problem, ProblemFuture, collect_from};

/// Runs every problem provider. A provider that fails becomes a problem of its
/// own rather than silently dropping out.
pub async fn collect_problems(pool: &SqlitePool) -> Vec<Problem> {
    let config = crate::config::runtime::get_runtime_config();
    let config_for_paths = config.clone();
    let config_for_linking = config.clone();

    collect_from(vec![
        (
            "indexers",
            Box::pin(crate::indexers::collect_indexer_problems(pool)) as ProblemFuture<'_>,
        ),
        (
            "clients",
            Box::pin(crate::clients::collect_client_problems()) as ProblemFuture<'_>,
        ),
        (
            "arr",
            Box::pin(crate::arr::collect_arr_problems()) as ProblemFuture<'_>,
        ),
        (
            "paths",
            Box::pin(async move { Ok(paths::collect_path_problems(&config_for_paths).await) })
                as ProblemFuture<'_>,
        ),
        (
            "linking",
            Box::pin(async move {
                Ok(linking::collect_data_linking_problems(&config_for_linking).await)
            }) as ProblemFuture<'_>,
        ),
        (
            "searchees",
            Box::pin(collect_searchee_problems(pool)) as ProblemFuture<'_>,
        ),
        (
            "recommendations",
            Box::pin(async { Ok(collect_recommendation_problems()) }) as ProblemFuture<'_>,
        ),
    ])
    .await
}

/// Warns when a configuration that should produce searchees has produced none —
/// usually a mis-mounted volume rather than an empty library.
pub async fn collect_searchee_problems(pool: &SqlitePool) -> Result<Vec<Problem>, String> {
    let config = crate::config::runtime::get_runtime_config();
    let expects_searchees =
        config.use_client_torrents || config.torrent_dir.is_some() || !config.data_dirs.is_empty();
    if !expects_searchees {
        return Ok(Vec::new());
    }

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM searchee")
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    if total > 0 {
        return Ok(Vec::new());
    }

    Ok(vec![
        Problem::new(
            "searchees:none-indexed",
            crate::problems::ProblemSeverity::Warning,
            "No searchees have been indexed yet.",
            "crust-seed has not indexed any torrents. Check that the indexing job succeeds and that torrentDir, useClientTorrents, or dataDirs are configured correctly.",
        )
        .with_metadata(
            serde_json::json!({
                "useClientTorrents": config.use_client_torrents,
                "torrentDir": config.torrent_dir,
                "dataDirsCount": config.data_dirs.len(),
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        ),
    ])
}

/// A nudge toward partial matching, which materially improves hit rate.
pub fn collect_recommendation_problems() -> Vec<Problem> {
    if crate::config::runtime::get_runtime_config().match_mode
        == crate::constants::MatchMode::Partial
    {
        return Vec::new();
    }
    vec![
        Problem::new(
            "recommendation:partial-matching",
            crate::problems::ProblemSeverity::Info,
            "Enable partial matching for better results",
            "Partial matching skips tiny files and improves match success. Enable it under Settings → Search & RSS when linking is available.",
        )
        .with_metadata(
            serde_json::json!({ "recommendation": true })
                .as_object()
                .cloned()
                .unwrap_or_default(),
        ),
    ]
}
