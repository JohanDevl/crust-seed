//! Virtual season searchees.
//!
//! Ported from the `createEnsembleSearchees` half of `searchee.ts` and
//! `getEnsembleForCandidate` in `pipeline.ts`.
//!
//! `seasonFromEpisodes` lets a *season pack* be cross-seeded from individual
//! episodes the user already has. Since no single local torrent corresponds to
//! the pack, one is synthesised: a searchee whose files are the episode video
//! files on disk, which the matcher and the linker then treat like any other.
//!
//! Two construction paths, matching the original:
//!
//! * [`create_ensemble_searchees`] builds every season it can from a list of
//!   episode searchees (used by search and inject);
//! * [`get_ensemble_for_candidate`] works backwards from one candidate season
//!   pack, using the `ensemble` index (used by announce and RSS).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use sqlx::SqlitePool;

use crate::clients::{by_client_host_priority, get_clients};
use crate::config::RuntimeConfig;
use crate::logger::Label;
use crate::searchee::{
    EpisodeId, File, Searchee, SearcheeLabel, get_season_keys, largest_file, organize_ensemble_keys,
};
use crate::utils::{human_readable_date, now_ms, strip_extension};

/// A season needs at least this many episodes to be worth synthesising.
const MIN_EPISODES: usize = 3;
/// Episodes younger than this are still arriving; a pack built from them would
/// be incomplete and the match would be wasted.
const MIN_AGE_MS: f64 = 8.0 * 24.0 * 60.0 * 60.0 * 1000.0;
/// An episode file must be most of its torrent, or the "episode" is really a
/// pack and using its largest file would misrepresent it.
const MIN_LARGEST_FILE_RATIO: f64 = 0.5;

async fn mtime_ms(path: &Path) -> Option<f64> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let modified = metadata.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as f64)
}

/// Resolves the absolute path of a searchee's largest file, if it is on disk.
async fn absolute_largest_file(
    searchee: &Searchee,
    torrent_save_paths: &HashMap<String, String>,
) -> Option<File> {
    let save_path = match (&searchee.path, &searchee.save_path) {
        // A data-dir searchee's files are relative to its parent.
        (Some(path), _) => Path::new(path).parent()?.to_string_lossy().into_owned(),
        (None, Some(save_path)) => save_path.clone(),
        (None, None) => torrent_save_paths
            .get(searchee.info_hash.as_deref()?)?
            .clone(),
    };

    let largest = largest_file(&searchee.files)?;
    if searchee.length == 0
        || (largest.length as f64 / searchee.length as f64) < MIN_LARGEST_FILE_RATIO
    {
        return None;
    }

    let absolute = Path::new(&save_path).join(&largest.path);
    if tokio::fs::metadata(&absolute).await.is_err() {
        return None;
    }
    Some(File {
        name: largest.name.clone(),
        path: absolute.to_string_lossy().into_owned(),
        length: largest.length,
    })
}

/// Adds one episode's file to the collection, preferring the oldest copy when
/// the same episode is present twice (a cross-seed of itself).
async fn push_ensemble_episode(
    searchee: &Searchee,
    episode_files: &mut Vec<File>,
    hosts: &mut BTreeMap<String, usize>,
    torrent_save_paths: &HashMap<String, String>,
) {
    let Some(file) = absolute_largest_file(searchee, torrent_save_paths).await else {
        return;
    };

    if let Some(index) = episode_files
        .iter()
        .position(|existing| existing.length == file.length)
    {
        let existing_age = mtime_ms(Path::new(&episode_files[index].path)).await;
        let new_age = mtime_ms(Path::new(&file.path)).await;
        match (existing_age, new_age) {
            // The older file is the original; the newer one is the cross-seed.
            (Some(existing_age), Some(new_age)) if existing_age <= new_age => return,
            _ => {
                episode_files.remove(index);
            }
        }
    }

    episode_files.push(file);
    if let Some(client_host) = &searchee.client_host {
        *hosts.entry(client_host.clone()).or_insert(0) += 1;
    }
}

/// Builds every virtual season that the given episode searchees support.
///
/// `use_filters` is off for the inject path, which is working from a torrent
/// that already exists and so does not need the "is this season worth
/// searching for" heuristics.
pub async fn create_ensemble_searchees(
    all_searchees: &[Searchee],
    label: SearcheeLabel,
    config: &RuntimeConfig,
    use_filters: bool,
) -> Vec<Searchee> {
    let Some(season_from_episodes) = config.season_from_episodes_ratio() else {
        return Vec::new();
    };
    if all_searchees.is_empty() {
        return Vec::new();
    }
    if use_filters {
        tracing::info!(
            label = label.as_str(),
            "Creating virtual seasons from episode searchees..."
        );
    }

    let (key_map, ensemble_titles) = organize_ensemble_keys(all_searchees, use_filters);

    // Torrent-file searchees carry no save path of their own, so the client is
    // asked once for all of them rather than per episode.
    let torrent_save_paths: HashMap<String, String> = if config.use_client_torrents {
        HashMap::new()
    } else {
        match get_clients().into_iter().next() {
            Some(client) => client
                .get_all_download_dirs(all_searchees, false, false)
                .await
                .unwrap_or_default(),
            None => HashMap::new(),
        }
    };

    let mut season_searchees = Vec::new();
    for (key, episodes) in &key_map {
        if let Some(searchee) = create_virtual_season(
            key,
            episodes,
            all_searchees,
            &ensemble_titles,
            &torrent_save_paths,
            label,
            season_from_episodes,
            use_filters,
        )
        .await
        {
            season_searchees.push(searchee);
        }
    }

    if use_filters {
        tracing::debug!(
            label = label.as_str(),
            "Created {} virtual season searchees...",
            season_searchees.len()
        );
    }
    season_searchees
}

#[allow(clippy::too_many_arguments)]
async fn create_virtual_season(
    key: &str,
    episodes: &BTreeMap<EpisodeId, Vec<usize>>,
    all_searchees: &[Searchee],
    ensemble_titles: &BTreeMap<String, String>,
    torrent_save_paths: &HashMap<String, String>,
    label: SearcheeLabel,
    season_from_episodes: f64,
    use_filters: bool,
) -> Option<Searchee> {
    if use_filters && episodes.len() < MIN_EPISODES {
        return None;
    }
    let ensemble_title = ensemble_titles.get(key)?.clone();

    // Availability is measured against the highest episode number seen, which
    // approximates the season length without asking an arr.
    if use_filters {
        let numbers: Vec<i64> = episodes
            .keys()
            .filter_map(|episode| match episode {
                EpisodeId::Number(n) => Some(*n),
                EpisodeId::Date(_) => None,
            })
            .collect();
        if numbers.len() == episodes.len()
            && let Some(highest) = numbers.iter().max()
            && *highest > 0
        {
            let available = episodes.len() as f64 / *highest as f64;
            if available < season_from_episodes {
                tracing::debug!(
                    label = label.as_str(),
                    "Skipping virtual searchee for {ensemble_title} episodes as there's only {}/{highest} ({available:.2} < {season_from_episodes:.2})",
                    episodes.len()
                );
                return None;
            }
        }
    }

    let mut files: Vec<File> = Vec::new();
    let mut length = 0i64;
    let mut newest_file_age = 0f64;
    let mut hosts: BTreeMap<String, usize> = BTreeMap::new();

    for indices in episodes.values() {
        let mut episode_files: Vec<File> = Vec::new();
        for index in indices {
            let Some(searchee) = all_searchees.get(*index) else {
                continue;
            };
            push_ensemble_episode(searchee, &mut episode_files, &mut hosts, torrent_save_paths)
                .await;
        }
        if episode_files.is_empty() {
            continue;
        }
        // An episode split across files contributes its average, so a season's
        // length stays comparable to a real pack's.
        let total: i64 = episode_files.iter().map(|f| f.length).sum();
        length += (total as f64 / episode_files.len() as f64).round() as i64;
        for file in &episode_files {
            if let Some(age) = mtime_ms(Path::new(&file.path)).await {
                newest_file_age = newest_file_age.max(age);
            }
        }
        files.extend(episode_files);
    }

    if files.len() < MIN_EPISODES {
        tracing::debug!(
            label = label.as_str(),
            "Skipping virtual searchee for {ensemble_title} episodes as only {} episode files were found (min: {MIN_EPISODES})",
            files.len()
        );
        return None;
    }
    if use_filters && (now_ms() as f64 - newest_file_age) < MIN_AGE_MS {
        tracing::debug!(
            label = label.as_str(),
            "Skipping virtual searchee for {ensemble_title} episodes as some are below the minimum age of 8 days: {}",
            human_readable_date(newest_file_age as i64)
        );
        return None;
    }

    // The client hosting most of the episodes owns the virtual season; ties go
    // to the higher-priority client.
    let client_host = hosts
        .iter()
        .max_by_key(|(host, count)| {
            (
                **count,
                std::cmp::Reverse(by_client_host_priority(Some(host))),
            )
        })
        .map(|(host, _)| host.clone());

    Some(Searchee {
        name: ensemble_title.clone(),
        title: ensemble_title,
        files,
        length,
        mtime_ms: Some(newest_file_age),
        client_host,
        label: Some(label),
        ..Default::default()
    })
}

/// Works backwards from a candidate season pack to the episodes on disk.
///
/// Uses the `ensemble` index rather than re-deriving keys from every local
/// torrent, because an announce has to be answered quickly.
pub async fn get_ensemble_for_candidate(
    pool: &SqlitePool,
    candidate_name: &str,
    label: SearcheeLabel,
    config: &RuntimeConfig,
) -> Vec<Searchee> {
    if config.season_from_episodes_ratio().is_none() {
        return Vec::new();
    }
    let Some(season_keys) = get_season_keys(&strip_extension(candidate_name)) else {
        return Vec::new();
    };
    let keys: Vec<String> = season_keys
        .key_titles
        .iter()
        .map(|key_title| format!("{key_title}.{}", season_keys.season))
        .collect();
    if keys.is_empty() {
        return Vec::new();
    }

    let placeholders = crate::db::placeholders(keys.len());
    let sql = format!(
        "SELECT client_host, path, element FROM ensemble WHERE ensemble IN ({placeholders})"
    );
    let mut query = sqlx::query_as::<_, (String, String, Option<String>)>(&sql);
    for key in &keys {
        query = query.bind(key);
    }
    let Ok(rows) = query.fetch_all(pool).await else {
        return Vec::new();
    };
    if rows.is_empty() {
        tracing::debug!(
            label = label.as_str(),
            "Did not find an ensemble [{}] for {candidate_name}",
            season_keys.ensemble_titles.join(", ")
        );
        return Vec::new();
    }

    let mut files_with_element: Vec<(File, String)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut hosts: BTreeMap<String, usize> = BTreeMap::new();

    for (client_host, path, element) in rows {
        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            missing.push(path);
            continue;
        };
        let length = metadata.len() as i64;
        let element = element.unwrap_or_default();
        // The same episode cross-seeded to several trackers appears once per
        // client; only one copy should contribute.
        let unique_key = format!("{client_host}-{element}-{length}");
        if seen.contains(&unique_key) {
            continue;
        }
        seen.push(unique_key);

        if !client_host.is_empty() {
            *hosts.entry(client_host).or_insert(0) += 1;
        }
        files_with_element.push((
            File {
                name: Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                path,
                length,
            },
            element,
        ));
    }
    let _ = crate::data_files::delete_data_paths(pool, &missing).await;

    if files_with_element.is_empty() {
        tracing::debug!(
            label = label.as_str(),
            "Did not find any files for ensemble [{}] for {candidate_name}: sources may be incomplete or missing",
            season_keys.ensemble_titles.join(", ")
        );
        return Vec::new();
    }

    // One length per *episode*, averaged when an episode has several files.
    let mut per_element: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for (file, element) in &files_with_element {
        per_element.entry(element).or_default().push(file.length);
    }
    let total_length: i64 = per_element
        .values()
        .map(|lengths| (lengths.iter().sum::<i64>() as f64 / lengths.len() as f64).round() as i64)
        .sum();

    let files: Vec<File> = files_with_element
        .into_iter()
        .map(|(file, _)| file)
        .collect();
    let mut newest_file_age = 0f64;
    for file in &files {
        if let Some(age) = mtime_ms(Path::new(&file.path)).await {
            newest_file_age = newest_file_age.max(age);
        }
    }
    let client_host = hosts
        .iter()
        .max_by_key(|(host, count)| {
            (
                **count,
                std::cmp::Reverse(by_client_host_priority(Some(host))),
            )
        })
        .map(|(host, _)| host.clone());

    tracing::debug!(
        label = Label::PreFilter.as_str(),
        "Using ensemble [{}] for {candidate_name}: {} files",
        season_keys.ensemble_titles.join(", "),
        files.len()
    );

    // One searchee per title spelling, so an AKA name can match too.
    season_keys
        .ensemble_titles
        .iter()
        .map(|title| Searchee {
            name: title.clone(),
            title: title.clone(),
            files: files.clone(),
            length: total_length,
            mtime_ms: Some(newest_file_age),
            client_host: client_host.clone(),
            label: Some(label),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::db::test_pool;

    fn episode_searchee(title: &str, path: &str, length: i64) -> Searchee {
        Searchee {
            path: Some(path.to_string()),
            name: title.to_string(),
            title: title.to_string(),
            files: vec![File {
                name: format!("{title}.mkv"),
                path: format!("{title}/{title}.mkv"),
                length,
            }],
            length,
            label: Some(SearcheeLabel::Search),
            ..Default::default()
        }
    }

    /// Builds N episodes on disk, aged past the 8-day gate.
    async fn episodes_on_disk(dir: &Path, count: i64) -> Vec<Searchee> {
        let mut searchees = Vec::new();
        for episode in 1..=count {
            let title = format!("Some.Show.S01E{episode:02}.1080p");
            let root = dir.join(&title);
            tokio::fs::create_dir_all(&root).await.unwrap();
            tokio::fs::write(root.join(format!("{title}.mkv")), vec![0u8; 1000])
                .await
                .unwrap();
            searchees.push(episode_searchee(&title, &root.to_string_lossy(), 1000));
        }
        searchees
    }

    #[tokio::test]
    async fn a_full_season_of_episodes_becomes_one_virtual_searchee() {
        let dir = tempfile::tempdir().unwrap();
        let episodes = episodes_on_disk(dir.path(), 4).await;

        let mut config = default_runtime_config();
        config.season_from_episodes = Some(1.0);
        // The freshly-written files are younger than the 8-day gate, so this
        // exercises the inject path (filters off).
        let seasons =
            create_ensemble_searchees(&episodes, SearcheeLabel::Inject, &config, false).await;

        assert_eq!(seasons.len(), 1);
        let season = &seasons[0];
        assert_eq!(season.files.len(), 4);
        assert_eq!(season.length, 4000);
        assert!(season.title.contains("Some.Show"));
        // The files are absolute paths to the episodes on disk.
        assert!(
            season
                .files
                .iter()
                .all(|f| Path::new(&f.path).is_absolute())
        );
    }

    /// Too few episodes is not a season.
    #[tokio::test]
    async fn fewer_than_three_episodes_produces_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let episodes = episodes_on_disk(dir.path(), 2).await;

        let mut config = default_runtime_config();
        config.season_from_episodes = Some(1.0);
        assert!(
            create_ensemble_searchees(&episodes, SearcheeLabel::Inject, &config, false)
                .await
                .is_empty()
        );
    }

    /// Recently-written episodes are still arriving; searching for the pack now
    /// would waste the match.
    #[tokio::test]
    async fn freshly_written_episodes_are_gated_by_age_when_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let episodes = episodes_on_disk(dir.path(), 5).await;

        let mut config = default_runtime_config();
        config.season_from_episodes = Some(1.0);
        assert!(
            create_ensemble_searchees(&episodes, SearcheeLabel::Search, &config, true)
                .await
                .is_empty()
        );
    }

    /// A gappy season fails the availability ratio.
    #[tokio::test]
    async fn a_partial_season_fails_the_availability_ratio() {
        let dir = tempfile::tempdir().unwrap();
        let mut episodes = Vec::new();
        // Episodes 1, 2, 3 and 12 — four of twelve.
        for episode in [1, 2, 3, 12] {
            let title = format!("Some.Show.S01E{episode:02}.1080p");
            let root = dir.path().join(&title);
            tokio::fs::create_dir_all(&root).await.unwrap();
            tokio::fs::write(root.join(format!("{title}.mkv")), vec![0u8; 1000])
                .await
                .unwrap();
            episodes.push(episode_searchee(&title, &root.to_string_lossy(), 1000));
        }

        let mut config = default_runtime_config();
        config.season_from_episodes = Some(1.0);
        let (key_map, titles) = organize_ensemble_keys(&episodes, true);
        let key = key_map.keys().next().unwrap();
        let season = create_virtual_season(
            key,
            &key_map[key],
            &episodes,
            &titles,
            &HashMap::new(),
            SearcheeLabel::Search,
            1.0,
            true,
        )
        .await;
        assert!(season.is_none(), "4/12 episodes must not become a season");
    }

    /// A config with seasonFromEpisodes disabled (the file form is `null` or
    /// `false`, both stored as 0) must build nothing.
    #[tokio::test]
    async fn a_zero_ratio_disables_the_feature() {
        let dir = tempfile::tempdir().unwrap();
        let episodes = episodes_on_disk(dir.path(), 5).await;
        let mut config = default_runtime_config();
        config.season_from_episodes = Some(0.0);
        assert!(
            create_ensemble_searchees(&episodes, SearcheeLabel::Inject, &config, false)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_feature_is_off_when_unconfigured() {
        let dir = tempfile::tempdir().unwrap();
        let episodes = episodes_on_disk(dir.path(), 5).await;
        let mut config = default_runtime_config();
        config.season_from_episodes = None;
        assert!(
            create_ensemble_searchees(&episodes, SearcheeLabel::Inject, &config, false)
                .await
                .is_empty()
        );
    }

    /// The announce path: a season-pack candidate finds the episodes indexed in
    /// the ensemble table.
    #[tokio::test]
    async fn a_season_candidate_finds_indexed_episodes() {
        let pool = test_pool().await;
        let dir = tempfile::tempdir().unwrap();

        for episode in 1..=3 {
            let path = dir.path().join(format!("Some.Show.S01E{episode:02}.mkv"));
            tokio::fs::write(&path, vec![0u8; 1000]).await.unwrap();
            sqlx::query(
                "INSERT INTO ensemble (client_host, path, info_hash, ensemble, element)
                 VALUES ('', ?, NULL, 'someshow.S1', ?)",
            )
            .bind(path.to_string_lossy().into_owned())
            .bind(episode.to_string())
            .execute(&pool)
            .await
            .unwrap();
        }

        let config = default_runtime_config();
        let searchees = get_ensemble_for_candidate(
            &pool,
            "Some.Show.S01.1080p.WEB-DL",
            SearcheeLabel::Announce,
            &config,
        )
        .await;

        assert!(!searchees.is_empty());
        assert_eq!(searchees[0].files.len(), 3);
        assert_eq!(searchees[0].length, 3000);
        assert!(searchees[0].is_virtual());
    }

    #[tokio::test]
    async fn ensemble_rows_pointing_at_deleted_files_are_pruned() {
        let pool = test_pool().await;
        sqlx::query(
            "INSERT INTO ensemble (client_host, path, ensemble, element)
             VALUES ('', '/gone/a.mkv', 'someshow.S1', '1')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let config = default_runtime_config();
        let searchees =
            get_ensemble_for_candidate(&pool, "Some.Show.S01", SearcheeLabel::Announce, &config)
                .await;
        assert!(searchees.is_empty());

        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ensemble")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn a_non_season_candidate_is_ignored() {
        let pool = test_pool().await;
        let config = default_runtime_config();
        assert!(
            get_ensemble_for_candidate(
                &pool,
                "Some.Movie.2020.1080p",
                SearcheeLabel::Announce,
                &config
            )
            .await
            .is_empty()
        );
    }
}
