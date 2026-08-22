//! `dataDirs` discovery and the reverse-lookup index.
//!
//! Ported from `dataFiles.ts`. Two jobs:
//!
//! * walk the configured data directories to find things that look like
//!   releases (used as searchees when there is no torrent client), and
//! * maintain the `data` / `ensemble` tables so an announce can be matched to
//!   local content by name without re-walking the filesystem.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::config::RuntimeConfig;
use crate::constants::{IGNORED_FOLDERS_SUBSTRINGS, LEVENSHTEIN_DIVISOR, VIDEO_EXTENSIONS};
use crate::logger::Label;
use crate::searchee::{File, parse_title};
use crate::utils::{create_key_title, exists, extname, strip_extension};

/// A `data` table row: a local path plus the title it parses to.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DataEntry {
    pub path: String,
    pub title: String,
}

/// Whether a path is worth considering as a release root.
///
/// Directories named `sample`, `proof`, `bdmv`, … are release *internals*, and
/// a non-video file at the leaf is not a release on its own.
pub fn should_ignore_path_heuristically(root: &Path, is_dir: bool) -> bool {
    let base = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if is_dir {
        IGNORED_FOLDERS_SUBSTRINGS.contains(&base.to_lowercase().as_str())
    } else {
        !VIDEO_EXTENSIONS.contains(&extname(&base).as_str())
    }
}

/// Every path under `root` that could be a release, down to `depth` levels.
///
/// Deepest paths come first so a caller memoising file listings fills its cache
/// bottom-up — the original relied on that ordering for the same reason.
pub async fn find_potential_nested_roots(root: &Path, depth: i32) -> Vec<PathBuf> {
    let Ok(metadata) = tokio::fs::metadata(root).await else {
        return Vec::new();
    };
    let is_dir = metadata.is_dir();
    if depth <= 0 || should_ignore_path_heuristically(root, is_dir) {
        return Vec::new();
    }
    if !is_dir {
        return vec![root.to_path_buf()];
    }

    let mut found = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(root).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            found.extend(Box::pin(find_potential_nested_roots(&entry.path(), depth - 1)).await);
        }
    }
    found.push(root.to_path_buf());
    found
}

pub async fn find_searchees_from_all_data_dirs(config: &RuntimeConfig) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for data_dir in &config.data_dirs {
        let Ok(mut entries) = tokio::fs::read_dir(data_dir).await else {
            tracing::warn!(
                label = Label::Index.as_str(),
                "Could not read dataDir {data_dir}"
            );
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            roots.extend(
                find_potential_nested_roots(&entry.path(), config.max_data_depth as i32).await,
            );
        }
    }
    roots
}

/// Reads a release root's file list, with paths relative to its parent so they
/// match a torrent's own file tree.
pub async fn get_files_from_data_root(root: &Path) -> Vec<File> {
    let parent = root.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(current) = stack.pop() {
        let Ok(metadata) = tokio::fs::metadata(&current).await else {
            continue;
        };
        if metadata.is_file() {
            let relative = current.strip_prefix(&parent).unwrap_or(&current);
            files.push(File {
                name: current
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path: relative.to_string_lossy().into_owned(),
                length: metadata.len() as i64,
            });
            continue;
        }
        if let Ok(mut entries) = tokio::fs::read_dir(&current).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                stack.push(entry.path());
            }
        }
    }
    files.sort_by(|a, b| crate::torrent::metafile::locale_compare(&a.path, &b.path));
    files
}

/// Writes `data` rows (and their ensemble pieces) for the given roots.
pub async fn index_data_paths(
    pool: &SqlitePool,
    paths: &[PathBuf],
    config: &RuntimeConfig,
) -> sqlx::Result<usize> {
    let mut rows: Vec<DataEntry> = Vec::new();
    let mut ensemble_rows: Vec<crate::torrent::index::EnsembleEntry> = Vec::new();

    for path in paths {
        let files = get_files_from_data_root(path).await;
        if files.is_empty() {
            continue;
        }
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(title) = parse_title(&base, &files, Some(&path.to_string_lossy())) else {
            continue;
        };

        if config.season_from_episodes.is_some()
            && let Some(pieces) = crate::torrent::index::create_ensemble_pieces(&title, &files)
        {
            let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            for piece in pieces {
                ensemble_rows.push(crate::torrent::index::EnsembleEntry {
                    // No client host: this content is on disk, not in a client.
                    client_host: None,
                    path: parent
                        .join(&piece.largest_file.path)
                        .to_string_lossy()
                        .into_owned(),
                    info_hash: None,
                    ensemble: piece.key,
                    element: piece.element,
                });
            }
        }

        rows.push(DataEntry {
            path: path.to_string_lossy().into_owned(),
            title,
        });
    }

    for row in &rows {
        sqlx::query(
            "INSERT INTO data (path, title) VALUES (?, ?)
             ON CONFLICT (path) DO UPDATE SET title = excluded.title",
        )
        .bind(&row.path)
        .bind(&row.title)
        .execute(pool)
        .await?;
    }
    crate::torrent::index::upsert_ensemble_rows(pool, &ensemble_rows).await?;

    Ok(rows.len())
}

/// Drops `data`/`ensemble` rows for paths that no longer exist.
pub async fn prune_missing_data_paths(pool: &SqlitePool) -> sqlx::Result<usize> {
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM data")
        .fetch_all(pool)
        .await?;
    let mut deleted = Vec::new();
    for path in paths {
        if !exists(&path).await {
            deleted.push(path);
        }
    }
    delete_data_paths(pool, &deleted).await?;
    Ok(deleted.len())
}

pub async fn delete_data_paths(pool: &SqlitePool, paths: &[String]) -> sqlx::Result<()> {
    for batch in paths.chunks(crate::db::BATCH_SIZE) {
        let placeholders = crate::db::placeholders(batch.len());
        for sql in [
            format!("DELETE FROM data WHERE path IN ({placeholders})"),
            format!("DELETE FROM ensemble WHERE path IN ({placeholders})"),
        ] {
            let mut query = sqlx::query(&sql);
            for path in batch {
                query = query.bind(path);
            }
            query.execute(pool).await?;
        }
    }
    Ok(())
}

/// Finds indexed data whose title is close enough to `stem` to be worth a full
/// comparison — the reverse lookup an announce uses.
///
/// An exact key-title match is preferred; only when nothing matches exactly
/// does it fall back to a Levenshtein sweep over every row, bounded by the
/// shorter of the two titles so a long title cannot absorb a short one.
pub async fn get_data_by_fuzzy_name(pool: &SqlitePool, stem: &str) -> sqlx::Result<Vec<DataEntry>> {
    let all: Vec<DataEntry> = sqlx::query_as("SELECT path, title FROM data")
        .fetch_all(pool)
        .await?;

    let full_match = create_key_title(stem);
    let exact: Vec<DataEntry> = match &full_match {
        Some(key) => all
            .iter()
            .filter(|entry| create_key_title(&strip_extension(&entry.title)).as_ref() == Some(key))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let candidates = if exact.is_empty() { all } else { exact };

    let candidate_max_distance = stem.chars().count() / LEVENSHTEIN_DIVISOR;
    let mut matches = Vec::new();
    let mut missing = Vec::new();
    for entry in candidates {
        let db_title = strip_extension(&entry.title);
        let max_distance =
            candidate_max_distance.min(db_title.chars().count() / LEVENSHTEIN_DIVISOR);
        if strsim::levenshtein(stem, &db_title) > max_distance {
            continue;
        }
        if exists(&entry.path).await {
            matches.push(entry);
        } else {
            missing.push(entry.path);
        }
    }

    // Opportunistically clean up rows whose content has been deleted.
    delete_data_paths(pool, &missing).await?;
    Ok(matches)
}

/// Removes `data` rows that fell outside the configured `dataDirs`.
pub async fn prune_data_outside_dirs(pool: &SqlitePool, data_dirs: &[String]) -> sqlx::Result<()> {
    let parents: Vec<PathBuf> = data_dirs.iter().map(PathBuf::from).collect();
    let paths: Vec<String> = sqlx::query_scalar("SELECT path FROM data")
        .fetch_all(pool)
        .await?;
    let outside: Vec<String> = paths
        .into_iter()
        .filter(|path| !crate::utils::is_child_path(Path::new(path), &parents))
        .collect();
    delete_data_paths(pool, &outside).await
}

/// Paths currently indexed, for callers that need the set.
pub async fn indexed_paths(pool: &SqlitePool) -> sqlx::Result<HashSet<String>> {
    Ok(sqlx::query_scalar::<_, String>("SELECT path FROM data")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::db::test_pool;

    #[test]
    fn release_internals_are_ignored() {
        assert!(should_ignore_path_heuristically(
            Path::new("/x/Sample"),
            true
        ));
        assert!(should_ignore_path_heuristically(Path::new("/x/BDMV"), true));
        assert!(!should_ignore_path_heuristically(
            Path::new("/x/Show S01"),
            true
        ));
    }

    #[test]
    fn non_video_leaf_files_are_ignored() {
        assert!(should_ignore_path_heuristically(
            Path::new("/x/readme.txt"),
            false
        ));
        assert!(!should_ignore_path_heuristically(
            Path::new("/x/a.mkv"),
            false
        ));
    }

    #[tokio::test]
    async fn nested_roots_are_returned_deepest_first() {
        let dir = tempfile::tempdir().unwrap();
        let show = dir.path().join("Show.S01");
        tokio::fs::create_dir_all(&show).await.unwrap();
        tokio::fs::write(show.join("a.mkv"), b"x").await.unwrap();

        let roots = find_potential_nested_roots(&show, 2).await;
        assert_eq!(roots.last().unwrap(), &show);
        assert!(roots.iter().any(|p| p.ends_with("a.mkv")));
    }

    #[tokio::test]
    async fn depth_limits_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        tokio::fs::create_dir_all(&deep).await.unwrap();
        tokio::fs::write(deep.join("x.mkv"), b"x").await.unwrap();

        let shallow = find_potential_nested_roots(&dir.path().join("a"), 1).await;
        assert_eq!(shallow.len(), 1);
    }

    #[tokio::test]
    async fn data_root_files_are_relative_to_the_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Show.S01");
        tokio::fs::create_dir_all(root.join("Sub")).await.unwrap();
        tokio::fs::write(root.join("a.mkv"), vec![0u8; 10])
            .await
            .unwrap();
        tokio::fs::write(root.join("Sub/b.mkv"), vec![0u8; 20])
            .await
            .unwrap();

        let files = get_files_from_data_root(&root).await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "Show.S01/a.mkv");
        assert_eq!(files[1].path, "Show.S01/Sub/b.mkv");
        assert_eq!(files[1].length, 20);
    }

    #[tokio::test]
    async fn indexing_writes_data_rows() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Some.Show.S01E01.1080p");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("Some.Show.S01E01.1080p.mkv"), vec![0u8; 10])
            .await
            .unwrap();

        let mut config = default_runtime_config();
        config.season_from_episodes = None;
        let count = index_data_paths(&pool, std::slice::from_ref(&root), &config)
            .await
            .unwrap();
        assert_eq!(count, 1);

        let rows: Vec<DataEntry> = sqlx::query_as("SELECT path, title FROM data")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows[0].path, root.to_string_lossy());
    }

    #[tokio::test]
    async fn fuzzy_lookup_prefers_an_exact_key_title_match() {
        let pool = test_pool().await;
        for (path, title) in [
            ("/data/a", "Some.Show.S01E01.1080p"),
            ("/data/b", "Totally.Different.S01E01"),
        ] {
            sqlx::query("INSERT INTO data (path, title) VALUES (?, ?)")
                .bind(path)
                .bind(title)
                .execute(&pool)
                .await
                .unwrap();
        }

        // Neither path exists, so both are pruned and nothing matches — but the
        // pruning itself is the behaviour under test.
        let matches = get_data_by_fuzzy_name(&pool, "Some.Show.S01E01.1080p")
            .await
            .unwrap();
        assert!(matches.is_empty());
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM data")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1, "only the inspected row is pruned");
    }

    #[tokio::test]
    async fn fuzzy_lookup_returns_paths_that_still_exist() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Some.Show.S01E01.1080p");
        tokio::fs::create_dir_all(&root).await.unwrap();

        sqlx::query("INSERT INTO data (path, title) VALUES (?, 'Some.Show.S01E01.1080p')")
            .bind(root.to_string_lossy().into_owned())
            .execute(&pool)
            .await
            .unwrap();

        let matches = get_data_by_fuzzy_name(&pool, "Some.Show.S01E01.1080p")
            .await
            .unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[tokio::test]
    async fn paths_outside_the_configured_dirs_are_pruned() {
        let pool = test_pool().await;
        for path in ["/data/keep/x", "/elsewhere/y"] {
            sqlx::query("INSERT INTO data (path, title) VALUES (?, 't')")
                .bind(path)
                .execute(&pool)
                .await
                .unwrap();
        }
        prune_data_outside_dirs(&pool, &["/data".to_string()])
            .await
            .unwrap();
        let remaining: Vec<String> = sqlx::query_scalar("SELECT path FROM data")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, vec!["/data/keep/x"]);
    }
}
