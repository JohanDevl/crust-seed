//! Reverse-lookup indexing of torrents and data directories.
//!
//! Ported from the indexing half of `torrent.ts`.
//!
//! The `torrent`, `client_searchee`, `data` and `ensemble` tables together let
//! an incoming announce be matched against local content without re-reading
//! every `.torrent` or re-walking every directory. This module keeps them in
//! sync with what is actually present.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::clients::get_clients;
use crate::config::RuntimeConfig;
use crate::logger::Label;
use crate::searchee::{
    EpisodeId, File, Searchee, get_episode_keys, largest_file, searchee_from_metafile,
};
use crate::torrent::Metafile;
use crate::torrent::cache::find_all_torrent_files_in_dir;
use crate::utils::strip_extension;

/// One row of the `ensemble` table: an episode file that could contribute to a
/// virtual season.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsembleEntry {
    pub client_host: Option<String>,
    pub path: String,
    pub info_hash: Option<String>,
    pub ensemble: String,
    /// Episode number, or `MM.DD` for a dated show.
    pub element: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsemblePiece {
    pub key: String,
    pub element: String,
    pub largest_file: File,
}

fn element_to_string(episode: &EpisodeId) -> String {
    match episode {
        EpisodeId::Number(n) => n.to_string(),
        EpisodeId::Date(d) => d.clone(),
    }
}

/// The ensemble keys a single episode contributes to.
///
/// The *largest* file is used because an episode release may carry samples and
/// subtitles; the video file is the one a virtual season needs to link.
pub fn create_ensemble_pieces(title: &str, files: &[File]) -> Option<Vec<EnsemblePiece>> {
    let episode_keys = get_episode_keys(&strip_extension(title))?;
    let largest = largest_file(files)?.clone();
    let element = element_to_string(&episode_keys.episode);

    Some(
        episode_keys
            .key_titles
            .iter()
            .map(|key_title| EnsemblePiece {
                key: match &episode_keys.season {
                    Some(season) => format!("{key_title}.{season}"),
                    None => key_title.clone(),
                },
                element: element.clone(),
                largest_file: largest.clone(),
            })
            .collect(),
    )
}

pub async fn upsert_ensemble_rows(pool: &SqlitePool, rows: &[EnsembleEntry]) -> sqlx::Result<()> {
    for row in rows {
        sqlx::query(
            r#"
            INSERT INTO ensemble (client_host, path, info_hash, ensemble, element)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT (client_host, path) DO UPDATE SET
                info_hash = excluded.info_hash,
                ensemble = excluded.ensemble,
                element = excluded.element
            "#,
        )
        // SQLite treats NULL as distinct in a unique index, so a NULL
        // client_host (data-dir content) would never conflict; the empty
        // string keeps the upsert working for those rows.
        .bind(row.client_host.clone().unwrap_or_default())
        .bind(&row.path)
        .bind(&row.info_hash)
        .bind(&row.ensemble)
        .bind(&row.element)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Ensemble rows for one client torrent.
pub async fn cache_ensemble_torrent_entry(
    searchee: &Searchee,
    torrent_save_paths: Option<&std::collections::HashMap<String, String>>,
) -> Option<Vec<EnsembleEntry>> {
    let pieces = create_ensemble_pieces(&searchee.title, &searchee.files)?;
    if pieces.is_empty() {
        return None;
    }
    let info_hash = searchee.info_hash.clone()?;

    let save_path = match &searchee.save_path {
        Some(save_path) => Some(save_path.clone()),
        None => match torrent_save_paths {
            Some(paths) => paths.get(&info_hash).cloned(),
            None => {
                let client = get_clients().into_iter().next()?;
                client.get_download_dir(searchee, false).await.ok()
            }
        },
    }?;

    Some(
        pieces
            .into_iter()
            .map(|piece| EnsembleEntry {
                client_host: searchee.client_host.clone(),
                path: Path::new(&save_path)
                    .join(&piece.largest_file.path)
                    .to_string_lossy()
                    .into_owned(),
                info_hash: Some(info_hash.clone()),
                ensemble: piece.key,
                element: piece.element,
            })
            .collect(),
    )
}

/// Info hashes already present locally — candidates matching one of these are
/// reported as `INFO_HASH_ALREADY_EXISTS` rather than snatched again.
pub async fn get_info_hashes_to_exclude(
    pool: &SqlitePool,
    config: &RuntimeConfig,
) -> sqlx::Result<HashSet<String>> {
    let sql = if config.use_client_torrents {
        "SELECT info_hash FROM client_searchee"
    } else {
        "SELECT info_hash FROM torrent"
    };
    Ok(sqlx::query_scalar::<_, Option<String>>(sql)
        .fetch_all(pool)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// Reads every `.torrent` in `torrent_dir` into a searchee.
///
/// qBittorrent is skipped when gathering client metadata because it stores
/// trackers in `.fastresume` files next to the torrents, which the parser
/// already reads.
pub async fn load_torrent_dir_light(torrent_dir: &Path) -> Vec<Searchee> {
    let Ok(paths) = find_all_torrent_files_in_dir(torrent_dir).await else {
        tracing::warn!(
            label = Label::Index.as_str(),
            "Skipping torrentDir indexing for \"{}\"",
            torrent_dir.display()
        );
        return Vec::new();
    };

    let mut searchees = Vec::new();
    for path in paths {
        match parse_torrent_with_metadata(&path).await {
            Ok(meta) => {
                if let Ok(searchee) = searchee_from_metafile(&meta) {
                    searchees.push(searchee);
                }
            }
            Err(e) => {
                tracing::error!(
                    label = Label::Index.as_str(),
                    "Failed to parse {}: {e}",
                    path.display()
                );
            }
        }
    }
    searchees
}

/// Reads a `.torrent`, folding in the sibling `.fastresume` when present.
///
/// qBittorrent keeps trackers, category and tags out of the torrent file, so
/// without the fastresume a torrentDir-based searchee would have no trackers
/// and the blocklist could not act on them.
pub async fn parse_torrent_with_metadata(path: &Path) -> Result<Metafile, String> {
    let bytes = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    let mut meta = Metafile::decode(&bytes).map_err(|e| e.to_string())?;

    let fastresume = path.with_extension("fastresume");
    if let Ok(resume_bytes) = tokio::fs::read(&fastresume).await
        && let Ok(resume) = super::bencode::decode(&resume_bytes)
    {
        meta.apply_metadata(&super::TorrentMetadata {
            trackers: resume.get("trackers").and_then(|tiers| {
                tiers.as_list().map(|tiers| {
                    tiers
                        .iter()
                        .map(|tier| {
                            tier.as_list()
                                .map(|urls| urls.iter().filter_map(|u| u.as_str()).collect())
                                .unwrap_or_default()
                        })
                        .collect()
                })
            }),
            category: resume.get("qBt-category").and_then(|v| v.as_str()),
            tags: resume
                .get("qBt-tags")
                .and_then(|v| v.as_str())
                .map(|tags| tags.split(',').map(str::to_string).collect()),
        });
    }
    Ok(meta)
}

/// Reconciles the `torrent` table with the files actually in `torrent_dir`,
/// returning searchees for the newly seen ones.
pub async fn index_torrent_dir(pool: &SqlitePool, dir: &Path) -> sqlx::Result<Vec<Searchee>> {
    let paths = find_all_torrent_files_in_dir(dir).await.unwrap_or_default();
    let path_strings: HashSet<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let mut new_searchees = Vec::new();
    for path in &paths {
        let file_path = path.to_string_lossy().into_owned();
        let known: Option<i64> = sqlx::query_scalar("SELECT id FROM torrent WHERE file_path = ?")
            .bind(&file_path)
            .fetch_optional(pool)
            .await?;
        if known.is_some() {
            continue;
        }

        let Ok(meta) = parse_torrent_with_metadata(path).await else {
            // Logged once per path so a permanently broken file does not spam.
            crate::logger::log_once(
                &format!("Failed to parse {file_path}"),
                || {
                    tracing::error!(label = Label::Index.as_str(), "Failed to parse {file_path}");
                },
                None,
            );
            continue;
        };

        sqlx::query("INSERT OR IGNORE INTO torrent (file_path, info_hash, name) VALUES (?, ?, ?)")
            .bind(&file_path)
            .bind(&meta.info_hash)
            .bind(&meta.name)
            .execute(pool)
            .await?;

        if let Ok(searchee) = searchee_from_metafile(&meta) {
            new_searchees.push(searchee);
        }
    }

    // Drop rows for torrents that have left the directory.
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT file_path, info_hash FROM torrent")
            .fetch_all(pool)
            .await?;
    let stale: Vec<(String, Option<String>)> = rows
        .into_iter()
        .filter(|(file_path, _)| !path_strings.contains(file_path))
        .collect();

    for batch in stale.chunks(crate::db::BATCH_SIZE) {
        let placeholders = crate::db::placeholders(batch.len());
        let sql = format!("DELETE FROM torrent WHERE file_path IN ({placeholders})");
        let mut query = sqlx::query(&sql);
        for (file_path, _) in batch {
            query = query.bind(file_path);
        }
        query.execute(pool).await?;

        let hashes: Vec<&String> = batch.iter().filter_map(|(_, h)| h.as_ref()).collect();
        if hashes.is_empty() {
            continue;
        }
        let placeholders = crate::db::placeholders(hashes.len());
        let sql = format!("DELETE FROM ensemble WHERE info_hash IN ({placeholders})");
        let mut query = sqlx::query(&sql);
        for hash in hashes {
            query = query.bind(hash);
        }
        query.execute(pool).await?;
    }

    Ok(new_searchees)
}

/// Clears cached rows that the current configuration can no longer produce.
///
/// Runs once at startup: switching from `torrentDir` to `useClientTorrents`
/// (or removing `dataDirs`) would otherwise leave rows that match nothing and
/// point at content crust-seed can no longer find.
pub async fn reconcile_caches_for_config(
    pool: &SqlitePool,
    config: &RuntimeConfig,
) -> sqlx::Result<()> {
    if !config.use_client_torrents {
        sqlx::query(
            "DELETE FROM ensemble WHERE info_hash IN (SELECT info_hash FROM client_searchee)",
        )
        .execute(pool)
        .await?;
        sqlx::query("DELETE FROM client_searchee")
            .execute(pool)
            .await?;
    } else {
        let hosts: Vec<String> = get_clients()
            .iter()
            .map(|c| c.client_host().to_string())
            .collect();
        if hosts.is_empty() {
            sqlx::query("DELETE FROM client_searchee")
                .execute(pool)
                .await?;
        } else {
            let placeholders = crate::db::placeholders(hosts.len());
            for sql in [
                format!("DELETE FROM client_searchee WHERE client_host NOT IN ({placeholders})"),
                format!(
                    "DELETE FROM ensemble WHERE client_host != '' AND client_host NOT IN ({placeholders})"
                ),
            ] {
                let mut query = sqlx::query(&sql);
                for host in &hosts {
                    query = query.bind(host);
                }
                query.execute(pool).await?;
            }
        }
    }

    if config.torrent_dir.is_none() {
        sqlx::query("DELETE FROM ensemble WHERE info_hash IN (SELECT info_hash FROM torrent)")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM torrent").execute(pool).await?;
    }

    if config.data_dirs.is_empty() {
        sqlx::query("DELETE FROM ensemble WHERE client_host = ''")
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM data").execute(pool).await?;
    } else {
        crate::data_files::prune_data_outside_dirs(pool, &config.data_dirs).await?;
    }

    if config.season_from_episodes.is_none() {
        sqlx::query("DELETE FROM ensemble").execute(pool).await?;
    }
    Ok(())
}

/// Full indexing pass over torrents and data directories.
pub async fn index_torrents_and_data_dirs(
    pool: &SqlitePool,
    config: &RuntimeConfig,
    startup: bool,
) -> sqlx::Result<()> {
    if startup {
        reconcile_caches_for_config(pool, config).await?;
    }

    // ─── dataDirs ───────────────────────────────────────────────────────
    if !config.data_dirs.is_empty() {
        if startup {
            tracing::info!(
                label = Label::Index.as_str(),
                "Indexing dataDirs for reverse lookup..."
            );
        }
        let roots = crate::data_files::find_searchees_from_all_data_dirs(config).await;
        let count = crate::data_files::index_data_paths(pool, &roots, config).await?;
        crate::data_files::prune_missing_data_paths(pool).await?;
        if startup {
            tracing::info!(
                label = Label::Index.as_str(),
                "Validated {count} entries from dataDirs..."
            );
        }
    }

    // ─── torrents ───────────────────────────────────────────────────────
    if !config.use_client_torrents && config.torrent_dir.is_none() {
        return Ok(());
    }

    let mut searchees: Vec<Searchee> = Vec::new();
    let mut save_paths: Option<std::collections::HashMap<String, String>> = None;

    if let Some(torrent_dir) = &config.torrent_dir {
        if startup {
            tracing::info!(
                label = Label::Index.as_str(),
                "Indexing torrentDir for reverse lookup..."
            );
            searchees = load_torrent_dir_light(Path::new(torrent_dir)).await;
            if let Some(client) = get_clients().into_iter().next() {
                save_paths = client
                    .get_all_download_dirs(&searchees, false, true)
                    .await
                    .ok();
            }
        } else {
            searchees = index_torrent_dir(pool, Path::new(torrent_dir)).await?;
        }
    } else {
        for client in get_clients() {
            let options = crate::clients::GetSearcheesOptions {
                new_searchees_only: !startup,
                include_files: true,
                include_trackers: true,
                ..Default::default()
            };
            match client.get_client_searchees(options).await {
                Ok(result) => {
                    if startup {
                        searchees.extend(result.searchees);
                    } else {
                        searchees.extend(result.new_searchees);
                    }
                }
                Err(e) => {
                    // A client that is down must not abort indexing of the
                    // others; it is retried on the next pass.
                    tracing::warn!(
                        label = client.label(),
                        "Indexing client torrents failed; disabling client until next retry: {e}"
                    );
                }
            }
        }
    }

    if config.season_from_episodes.is_none() {
        return Ok(());
    }
    let mut ensemble_rows = Vec::new();
    for searchee in &searchees {
        if let Some(rows) = cache_ensemble_torrent_entry(searchee, save_paths.as_ref()).await {
            ensemble_rows.extend(rows);
        }
    }
    upsert_ensemble_rows(pool, &ensemble_rows).await
}

/// Torrent-cache paths for an info hash — used by `restore`.
pub async fn torrent_paths_in_dir(dir: &Path) -> Vec<PathBuf> {
    find_all_torrent_files_in_dir(dir).await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::db::test_pool;
    use crate::torrent::metafile::fixtures::single_file_torrent;

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

    #[test]
    fn ensemble_pieces_key_on_title_and_season() {
        let pieces = create_ensemble_pieces(
            "Some.Show.S01E05.1080p",
            &[file("a.mkv", 1000), file("sample.mkv", 10)],
        )
        .unwrap();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].key, "someshow.S1");
        assert_eq!(pieces[0].element, "5");
        // The largest file is the one worth linking.
        assert_eq!(pieces[0].largest_file.name, "a.mkv");
    }

    #[test]
    fn a_season_pack_has_no_ensemble_pieces() {
        assert!(create_ensemble_pieces("Some.Show.S01.1080p", &[file("a.mkv", 1)]).is_none());
    }

    #[tokio::test]
    async fn ensemble_rows_upsert_for_data_dir_content() {
        let pool = test_pool().await;
        let rows = vec![EnsembleEntry {
            client_host: None,
            path: "/data/a.mkv".into(),
            info_hash: None,
            ensemble: "someshow.S1".into(),
            element: "1".into(),
        }];
        upsert_ensemble_rows(&pool, &rows).await.unwrap();
        upsert_ensemble_rows(&pool, &rows).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ensemble")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "NULL client_host must still conflict");
    }

    #[tokio::test]
    async fn torrent_dir_indexing_adds_and_prunes() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let torrent_path = dir.path().join("a.torrent");
        tokio::fs::write(&torrent_path, single_file_torrent("Movie.2020.mkv", 100))
            .await
            .unwrap();

        let added = index_torrent_dir(&pool, dir.path()).await.unwrap();
        assert_eq!(added.len(), 1);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM torrent")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // A second pass sees nothing new.
        assert!(
            index_torrent_dir(&pool, dir.path())
                .await
                .unwrap()
                .is_empty()
        );

        tokio::fs::remove_file(&torrent_path).await.unwrap();
        index_torrent_dir(&pool, dir.path()).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM torrent")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn excluded_info_hashes_come_from_the_configured_source() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO torrent (file_path, info_hash, name) VALUES ('/a', 'hash-a', 'a')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO client_searchee (client_host, info_hash, name) VALUES ('h', 'hash-b', 'b')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut config = default_runtime_config();
        config.use_client_torrents = false;
        let excluded = get_info_hashes_to_exclude(&pool, &config).await.unwrap();
        assert!(excluded.contains("hash-a"));
        assert!(!excluded.contains("hash-b"));

        config.use_client_torrents = true;
        let excluded = get_info_hashes_to_exclude(&pool, &config).await.unwrap();
        assert!(excluded.contains("hash-b"));
    }

    /// Switching away from torrentDir must clear rows that can no longer be
    /// produced, or they would match announces against content that is gone.
    #[tokio::test]
    async fn reconciling_clears_caches_the_config_no_longer_feeds() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO torrent (file_path, info_hash, name) VALUES ('/a', 'h', 'a')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO data (path, title) VALUES ('/data/x', 't')")
            .execute(&pool)
            .await
            .unwrap();

        let mut config = default_runtime_config();
        config.torrent_dir = None;
        config.data_dirs.clear();
        config.use_client_torrents = true;
        reconcile_caches_for_config(&pool, &config).await.unwrap();

        for table in ["torrent", "data"] {
            let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "{table} should be cleared");
        }
    }

    #[tokio::test]
    async fn fastresume_metadata_is_folded_into_the_torrent() {
        let dir = tempfile::tempdir().unwrap();
        let torrent_path = dir.path().join("a.torrent");
        tokio::fs::write(&torrent_path, single_file_torrent("Movie.mkv", 1))
            .await
            .unwrap();

        let mut resume = std::collections::BTreeMap::new();
        resume.insert(
            b"qBt-category".to_vec(),
            crate::torrent::bencode::Value::Bytes(b"movies".to_vec()),
        );
        resume.insert(
            b"qBt-tags".to_vec(),
            crate::torrent::bencode::Value::Bytes(b"cross-seed,keep".to_vec()),
        );
        tokio::fs::write(
            dir.path().join("a.fastresume"),
            crate::torrent::bencode::encode(&crate::torrent::bencode::Value::Dict(resume)),
        )
        .await
        .unwrap();

        let meta = parse_torrent_with_metadata(&torrent_path).await.unwrap();
        assert_eq!(meta.category.as_deref(), Some("movies"));
        assert_eq!(
            meta.tags.as_deref(),
            Some(&["cross-seed".to_string(), "keep".to_string()][..])
        );
    }
}
