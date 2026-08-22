//! Acting on a match: linking the data into place and injecting the torrent.
//!
//! Ported from `action.ts`.
//!
//! With `action: "save"` the `.torrent` is written to `outputDir` and nothing
//! else happens. With `action: "inject"` the work is: find where the searchee's
//! data actually lives, link it into a directory the target client can seed
//! from, hand the torrent to the client, and on failure roll the links back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::clients::{DownloadDirError, InjectOptions, TorrentClient, get_clients, should_recheck};
use crate::config::RuntimeConfig;
use crate::config::runtime::get_runtime_config;
use crate::constants::{ALL_EXTENSIONS, Action, ActionResult, Decision, InjectionResult, LinkType};
use crate::errors::CrustSeedError;
use crate::searchee::{File, Searchee, get_root, get_root_folder};
use crate::torrent::Metafile;
use crate::torrent::cache::get_torrent_save_path;
use crate::utils::{exists, format_as_list, not_exists};

const LINK_DIR_SRC_NAME: &str = "linkDirSrc.cross-seed";
const LINK_DIR_DEST_NAME: &str = "linkDirDest.cross-seed";
const CLIENT_DEST_NAME: &str = "torrentClientDest.cross-seed";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkResult {
    /// At least one destination file was already present, so a rollback would
    /// delete data that was not ours to delete.
    pub already_existed: bool,
    pub linked_new_files: bool,
}

/// Pairs of (source file, destination file) for a match.
///
/// A perfect [`Decision::Match`] with a known save path can map paths
/// one-to-one. Anything looser has to pair files up by length (then name),
/// consuming each searchee file so two candidate files of equal length cannot
/// both link from the same source.
pub fn plan_links(
    searchee: &Searchee,
    new_meta: &Metafile,
    decision: Decision,
    destination_dir: &Path,
    save_path: Option<&Path>,
) -> Vec<(PathBuf, PathBuf)> {
    if decision == Decision::Match
        && let Some(save_path) = save_path
    {
        return new_meta
            .files
            .iter()
            .map(|file| (save_path.join(&file.path), destination_dir.join(&file.path)))
            .collect();
    }

    let mut available: Vec<&File> = searchee.files.iter().collect();
    let mut pairs = Vec::new();
    for new_file in &new_meta.files {
        let same_length: Vec<usize> = available
            .iter()
            .enumerate()
            .filter(|(_, f)| f.length == new_file.length)
            .map(|(i, _)| i)
            .collect();
        let index = match same_length.len() {
            0 => continue,
            1 => same_length[0],
            _ => same_length
                .iter()
                .copied()
                .find(|i| available[*i].name == new_file.name)
                .unwrap_or(same_length[0]),
        };
        let matched = available.remove(index);
        // A data-based searchee's file paths are already absolute.
        let src = match save_path {
            Some(save_path) => save_path.join(&matched.path),
            None => PathBuf::from(&matched.path),
        };
        pairs.push((src, destination_dir.join(&new_file.path)));
    }
    pairs
}

/// Links every file of `new_meta` into `destination_dir`.
///
/// A missing *source* is fatal unless `ignore_missing` — an incomplete
/// searchee is expected to have gaps, a complete one is not.
pub async fn link_all_files_in_metafile(
    searchee: &Searchee,
    new_meta: &Metafile,
    decision: Decision,
    destination_dir: &Path,
    save_path: Option<&Path>,
    ignore_missing: bool,
    link_type: LinkType,
) -> Result<LinkResult, CrustSeedError> {
    let paths = plan_links(searchee, new_meta, decision, destination_dir, save_path);
    let mut result = LinkResult::default();

    tracing::debug!(
        "Linking {} from {} to {}",
        new_meta.name,
        searchee.title,
        destination_dir.display()
    );

    let mut valid: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (src, dest) in paths {
        if exists(&dest).await {
            tracing::debug!(
                "--- Skipping {} -> {}, already exists",
                src.display(),
                dest.display()
            );
            result.already_existed = true;
            continue;
        }
        if exists(&src).await {
            valid.push((src, dest));
        } else if !ignore_missing {
            return Err(CrustSeedError::new(format!(
                "Linking failed, {} not found.",
                src.display()
            )));
        }
    }

    for (src, dest) in valid {
        if let Some(parent) = dest.parent()
            && not_exists(parent).await
        {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                CrustSeedError::new(format!("Could not create {}: {e}", parent.display()))
            })?;
        }
        match link_file(&src, &dest, link_type).await {
            Ok(true) => result.linked_new_files = true,
            Ok(false) => {}
            Err(e) => {
                tracing::error!(
                    "--- Linking failed, {} -> {}: {e}",
                    src.display(),
                    dest.display()
                );
                return Err(e);
            }
        }
    }

    Ok(result)
}

/// Removes the directories a failed injection linked into.
///
/// Three guards keep this from deleting anything outside our own destination:
/// the root must live under `destination_dir`, must not *be* it, and must not
/// share its inode (which would mean it resolved to the same directory).
pub async fn unlink_metafile(meta: &Metafile, destination_dir: &Path) {
    let mut roots: Vec<PathBuf> = Vec::new();
    for file in &meta.files {
        match get_root(file) {
            Err(message) => {
                tracing::error!(
                    "Unable to unlink {} in {}: {message}",
                    meta.name,
                    destination_dir.display()
                );
                return;
            }
            Ok(root) => roots.push(destination_dir.join(root)),
        }
    }

    let destination_id = file_identity(destination_dir).await;
    for root in roots {
        if not_exists(&root).await {
            continue;
        }
        if !root.starts_with(destination_dir) {
            continue;
        }
        if crate::utils::normalize_absolute(&root)
            == crate::utils::normalize_absolute(destination_dir)
        {
            continue;
        }
        if destination_id.is_some() && file_identity(&root).await == destination_id {
            continue;
        }
        tracing::debug!("Unlinking {}", root.display());
        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}

/// `(device, inode)` — the pair that identifies a file on Unix.
#[cfg(unix)]
async fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
async fn file_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// The device a path lives on. Hardlinks and reflinks cannot cross devices, so
/// this is how a compatible `linkDir` is found without trial and error.
#[cfg(unix)]
pub async fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    tokio::fs::metadata(path).await.ok().map(|m| m.dev())
}

#[cfg(not(unix))]
pub async fn device_of(_path: &Path) -> Option<u64> {
    // Windows metadata carries no device id; callers fall back to a link probe.
    None
}

/// Creates one link. Returns `false` when the destination already exists — the
/// caller treats that as "nothing new was linked" rather than an error.
pub async fn link_file(
    old_path: &Path,
    new_path: &Path,
    link_type: LinkType,
) -> Result<bool, CrustSeedError> {
    if exists(new_path).await {
        return Ok(false);
    }
    let resolved = unwrap_symlinks(old_path).await?;
    let new_path = new_path.to_path_buf();

    let outcome = match link_type {
        LinkType::Hardlink => tokio::fs::hard_link(&resolved, &new_path)
            .await
            .map_err(|e| e.to_string()),
        LinkType::Symlink => {
            // Symlink targets are resolved relative to the link's own
            // directory, not crust-seed's cwd, so both sides are absolutised.
            let target = crate::utils::normalize_absolute(&resolved);
            let link = crate::utils::normalize_absolute(&new_path);
            #[cfg(unix)]
            {
                tokio::fs::symlink(&target, &link)
                    .await
                    .map_err(|e| e.to_string())
            }
            #[cfg(windows)]
            {
                tokio::fs::symlink_file(&target, &link)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
        LinkType::Reflink => {
            let src = resolved.clone();
            let dest = new_path.clone();
            match tokio::task::spawn_blocking(move || reflink_copy::reflink(&src, &dest)).await {
                Err(e) => Err(e.to_string()),
                Ok(result) => result.map_err(|e| e.to_string()),
            }
        }
        LinkType::ReflinkOrCopy => {
            let src = resolved.clone();
            let dest = new_path.clone();
            match tokio::task::spawn_blocking(move || {
                reflink_copy::reflink_or_copy(&src, &dest).map(|_| ())
            })
            .await
            {
                Err(e) => Err(e.to_string()),
                Ok(result) => result.map_err(|e| e.to_string()),
            }
        }
    };

    match outcome {
        Ok(()) => Ok(true),
        Err(message) if message.contains("exists") => Ok(false),
        Err(message) => Err(CrustSeedError::new(message)),
    }
}

/// Resolves a chain of *file* symlinks to the real file.
///
/// Deliberately not `canonicalize`: directory symlinks in the middle of the
/// path are left alone, because the user's mount layout may depend on them.
pub async fn unwrap_symlinks(path: &Path) -> Result<PathBuf, CrustSeedError> {
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        let metadata = tokio::fs::symlink_metadata(&current)
            .await
            .map_err(|e| CrustSeedError::new(format!("{}: {e}", current.display())))?;
        if !metadata.file_type().is_symlink() {
            return Ok(current);
        }
        let target = tokio::fs::read_link(&current)
            .await
            .map_err(|e| CrustSeedError::new(e.to_string()))?;
        let parent = current.parent().unwrap_or(Path::new(".")).to_path_buf();
        current = crate::utils::normalize_absolute(&parent.join(target));
    }
    Err(CrustSeedError::new(format!(
        "too many levels of symbolic links at {}",
        path.display()
    )))
}

/// Picks the `linkDir` that can actually receive links from `path`.
///
/// Device comparison is tried first because it is cheap and exact; when the
/// platform does not report devices (Windows) or they are ambiguous, an actual
/// link is attempted into each candidate. Symlinks work across devices, so they
/// fall back to the first configured directory.
pub async fn get_link_dir(path: &Path, config: &RuntimeConfig) -> Option<PathBuf> {
    let link_dirs: Vec<PathBuf> = config.link_dirs.iter().map(PathBuf::from).collect();
    if link_dirs.is_empty() {
        return None;
    }

    let path_metadata = tokio::fs::metadata(path).await.ok()?;
    if let Some(path_device) = device_of(path).await {
        let mut devices = Vec::with_capacity(link_dirs.len());
        for dir in &link_dirs {
            devices.push(device_of(dir).await);
        }
        // Only trust device matching when every linkDir is on a distinct
        // device; otherwise the mapping is ambiguous.
        let distinct = crate::utils::dedupe_preserving_order(&devices).len() == link_dirs.len();
        if distinct && let Some(index) = devices.iter().position(|d| *d == Some(path_device)) {
            return Some(link_dirs[index].clone());
        }
    }

    let mut temp_file: Option<PathBuf> = None;
    let mut src_file: Option<PathBuf> = if path_metadata.is_file() {
        Some(path.to_path_buf())
    } else if path_metadata.is_dir() {
        find_a_file_with_ext(path, &ALL_EXTENSIONS).await
    } else {
        None
    };

    if src_file.is_none() {
        let candidate = if path_metadata.is_dir() {
            path.join(LINK_DIR_SRC_NAME)
        } else {
            path.parent()
                .unwrap_or(Path::new("."))
                .join(LINK_DIR_SRC_NAME)
        };
        if tokio::fs::write(&candidate, b"").await.is_ok() {
            src_file = Some(candidate.clone());
            temp_file = Some(candidate);
        }
    }

    // Reflinks have their own cross-device rules, so probe with the configured
    // type when it is a reflink and with a hardlink otherwise.
    let probe_type = match config.link_type {
        LinkType::Reflink | LinkType::ReflinkOrCopy => config.link_type,
        _ => LinkType::Hardlink,
    };

    if let Some(src_file) = &src_file {
        for link_dir in &link_dirs {
            let test_path = link_dir.join(LINK_DIR_DEST_NAME);
            if link_file(src_file, &test_path, probe_type).await.is_ok() {
                let _ = tokio::fs::remove_file(&test_path).await;
                if let Some(temp) = &temp_file {
                    let _ = tokio::fs::remove_file(temp).await;
                }
                return Some(link_dir.clone());
            }
        }
    }
    if let Some(temp) = &temp_file {
        let _ = tokio::fs::remove_file(temp).await;
    }

    if config.link_type != LinkType::Symlink {
        tracing::error!(
            "Cannot find any linkDir from linkDirs on the same drive to {:?} {}",
            config.link_type,
            path.display()
        );
        return None;
    }
    if link_dirs.len() > 1 {
        tracing::warn!(
            "Cannot find any linkDir from linkDirs on the same drive, using first linkDir for symlink: {}",
            path.display()
        );
    }
    link_dirs.first().cloned()
}

/// A virtual season is assembled from episodes that may live on different
/// drives; if they do, there is no single linkDir that can hold the result.
pub async fn get_link_dir_virtual(searchee: &Searchee, config: &RuntimeConfig) -> Option<PathBuf> {
    let first = searchee.files.first()?;
    let link_dir = get_link_dir(Path::new(&first.path), config).await?;
    for file in searchee.files.iter().skip(1) {
        if get_link_dir(Path::new(&file.path), config).await.as_ref() != Some(&link_dir) {
            tracing::error!(
                "Cannot link files to multiple linkDirs for seasonFromEpisodes aggregation, source episodes are spread across multiple drives."
            );
            return None;
        }
    }
    Some(link_dir)
}

/// First file under `dir` with one of `exts`, searched depth-first.
pub async fn find_a_file_with_ext(dir: &Path, exts: &[&str]) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if exts.contains(&crate::utils::extname(&path.to_string_lossy()).as_str()) {
                return Some(path);
            }
        }
    }
    None
}

/// Writes the `.torrent` to `outputDir`, touching it if already present so the
/// cleanup job sees it as live.
pub async fn save_to_output_dir(new_meta: &Metafile, tracker: &str, config: &RuntimeConfig) {
    let media_type = crate::searchee::media_type_of(&new_meta.title, &new_meta.files);
    let path = get_torrent_save_path(
        new_meta,
        media_type,
        tracker,
        Path::new(&config.output_dir),
        false,
    );
    if exists(&path).await {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Err(e) = tokio::fs::write(&path, new_meta.encode()).await {
        tracing::error!(
            "Failed to save {} on {tracker} to outputDir: {e}",
            new_meta.name
        );
    }
}

/// What [`perform_action`] did.
#[derive(Clone)]
pub struct ActionOutcome {
    pub action_result: ActionResult,
    pub client: Option<Arc<dyn TorrentClient>>,
    pub destination_dir: Option<String>,
    pub linked_new_files: bool,
}

impl ActionOutcome {
    fn failure(linked_new_files: bool) -> Self {
        ActionOutcome {
            action_result: ActionResult::Injection(InjectionResult::Failure),
            client: None,
            destination_dir: None,
            linked_new_files,
        }
    }
}

/// Resolves where a searchee's data actually lives.
///
/// Returns `Ok(None)` for a virtual searchee, whose files already carry
/// absolute paths.
pub async fn get_save_path(
    searchee: &Searchee,
    only_completed: bool,
) -> Result<Option<PathBuf>, DownloadDirError> {
    if let Some(path) = &searchee.path {
        if not_exists(path).await {
            tracing::error!("Linking failed, {path} not found.");
            return Err(DownloadDirError::InvalidData);
        }
        return Ok(Path::new(path).parent().map(Path::to_path_buf));
    }

    if searchee.info_hash.is_none() {
        // Virtual searchee: verify every episode file is still there, and for a
        // completeness check that it has not changed under us.
        for file in &searchee.files {
            let Ok(metadata) = tokio::fs::metadata(&file.path).await else {
                tracing::error!("Linking failed, {} not found.", file.path);
                return Err(DownloadDirError::InvalidData);
            };
            if only_completed && metadata.len() as i64 != file.length {
                return Err(DownloadDirError::TorrentNotComplete);
            }
        }
        return Ok(None);
    }

    let info_hash = searchee.info_hash.as_deref().unwrap();
    let clients = get_clients();
    let client = if clients.len() == 1 {
        clients.first().cloned()
    } else {
        clients
            .iter()
            .find(|c| Some(c.client_host()) == searchee.client_host.as_deref())
            .cloned()
    };
    let Some(client) = client else {
        return Err(DownloadDirError::NotFound);
    };

    let save_path = match &searchee.save_path {
        Some(save_path) => {
            if only_completed && !client.is_torrent_complete(info_hash).await.unwrap_or(false) {
                return Err(DownloadDirError::TorrentNotComplete);
            }
            PathBuf::from(save_path)
        }
        None => PathBuf::from(client.get_download_dir(searchee, only_completed).await?),
    };

    // Confirm the data is really there before linking from it.
    let first_file = searchee
        .files
        .first()
        .ok_or(DownloadDirError::InvalidData)?;
    let root_folder = get_root_folder(first_file).map_err(|message| {
        tracing::error!("Linking failed, {message}");
        DownloadDirError::InvalidData
    })?;
    let source_root = if searchee.files.len() == 1 {
        save_path.join(&first_file.path)
    } else {
        match root_folder {
            Some(root) => save_path.join(root),
            None => save_path.clone(),
        }
    };
    if not_exists(&source_root).await {
        tracing::error!("Linking failed, {} not found.", source_root.display());
        return Err(DownloadDirError::InvalidData);
    }

    Ok(Some(save_path))
}

/// Injects (or saves) one match.
pub async fn perform_action(
    new_meta: &Metafile,
    decision: Decision,
    searchee: &Searchee,
    tracker: &str,
) -> ActionOutcome {
    let config = get_runtime_config();

    if config.action == Action::Save {
        save_to_output_dir(new_meta, tracker, &config).await;
        tracing::info!("Found {} on {tracker} - saved", new_meta.name);
        return ActionOutcome {
            action_result: ActionResult::Saved,
            client: None,
            destination_dir: None,
            linked_new_files: false,
        };
    }

    let clients = get_clients();
    let mut client = if clients.len() == 1 {
        clients.first().filter(|c| !c.readonly()).cloned()
    } else {
        clients
            .iter()
            .find(|c| Some(c.client_host()) == searchee.client_host.as_deref() && !c.readonly())
            .cloned()
    };
    // A searchee that came from a read-only client has to be treated as
    // data-based: its own client cannot accept the injection.
    let readonly_source = client.is_none() && searchee.client_host.is_some();

    let mut save_path: Option<PathBuf> = None;
    let mut destination_dir: Option<PathBuf> = None;
    let mut destination_dir_from_client = false;
    let mut linked_new_files = false;
    let mut unlink_ok = false;

    if !config.link_dirs.is_empty() {
        match get_save_path(searchee, true).await {
            Err(DownloadDirError::TorrentNotComplete) => {
                tracing::warn!(
                    "Found {} on {tracker} - source is incomplete, saving...",
                    new_meta.name
                );
                save_to_output_dir(new_meta, tracker, &config).await;
                return ActionOutcome {
                    action_result: ActionResult::Injection(InjectionResult::TorrentNotComplete),
                    client,
                    destination_dir: None,
                    linked_new_files,
                };
            }
            Err(error) => {
                tracing::error!(
                    "Failed to link files for {} from {}: {error:?}",
                    new_meta.name,
                    searchee.title
                );
                save_to_output_dir(new_meta, tracker, &config).await;
                return ActionOutcome::failure(linked_new_files);
            }
            Ok(resolved) => save_path = resolved,
        }

        match resolve_destination(
            client.clone(),
            searchee,
            save_path.as_deref(),
            new_meta,
            tracker,
            &config,
        )
        .await
        {
            Some(resolved) => {
                client = Some(resolved.client);
                destination_dir = Some(resolved.destination_dir);
                destination_dir_from_client = resolved.from_client;
            }
            None => client = None,
        }
    }

    let Some(client) = client else {
        tracing::error!("Failed to find a torrent client for {}", searchee.title);
        save_to_output_dir(new_meta, tracker, &config).await;
        return ActionOutcome::failure(linked_new_files);
    };

    // Never inject a torrent that another client is already seeding: two
    // clients seeding the same data fight over the files.
    for other in clients.iter() {
        if other.client_host() == client.client_host() {
            continue;
        }
        match other.is_torrent_in_client(&new_meta.info_hash).await {
            Ok(false) => continue,
            Ok(true) => tracing::warn!(
                "Skipping {} injection into {} - already exists in {}",
                new_meta.name,
                client.client_host(),
                other.client_host()
            ),
            Err(e) => tracing::error!(
                "Failed to check if {} exists in {}: {e}",
                new_meta.name,
                other.client_host()
            ),
        }
        return ActionOutcome::failure(linked_new_files);
    }

    if !config.link_dirs.is_empty() {
        let destination = destination_dir.clone().unwrap_or_default();
        match link_all_files_in_metafile(
            searchee,
            new_meta,
            decision,
            &destination,
            save_path.as_deref(),
            false,
            config.link_type,
        )
        .await
        {
            Err(e) => {
                tracing::error!(
                    "Failed to link files for {} from {}: {e}",
                    new_meta.name,
                    searchee.title
                );
                save_to_output_dir(new_meta, tracker, &config).await;
                return ActionOutcome::failure(linked_new_files);
            }
            Ok(link_result) => {
                // Only roll back links we created ourselves.
                unlink_ok = !link_result.already_existed;
                linked_new_files = link_result.linked_new_files;
            }
        }
    } else if let Some(path) = &searchee.path {
        destination_dir = Path::new(path).parent().map(Path::to_path_buf);
    } else if readonly_source {
        match get_save_path(searchee, true).await {
            Ok(Some(path)) => destination_dir = Some(path),
            _ => {
                tracing::error!("Failed to find a save path for {}", searchee.title);
                save_to_output_dir(new_meta, tracker, &config).await;
                return ActionOutcome::failure(linked_new_files);
            }
        }
    }

    let inject_searchee = if readonly_source {
        Searchee {
            info_hash: None,
            ..searchee.clone()
        }
    } else {
        searchee.clone()
    };

    let injection = if destination_dir_from_client {
        // The client already knows this torrent's data location, which means it
        // is already seeding something at that path.
        InjectionResult::AlreadyExists
    } else {
        client
            .inject(
                new_meta,
                &inject_searchee,
                decision,
                InjectOptions {
                    only_completed: true,
                    destination_dir: destination_dir
                        .as_ref()
                        .map(|d| d.to_string_lossy().into_owned()),
                },
            )
            .await
    };

    match injection {
        InjectionResult::Success => {
            tracing::info!("Found {} on {tracker} - injected", new_meta.name);
            // The inject job may still need the file: a rechecking torrent, or
            // a data-based searchee whose match is not yet confirmed.
            if should_recheck(new_meta, decision, &config) || searchee.info_hash.is_none() {
                save_to_output_dir(new_meta, tracker, &config).await;
            }
        }
        InjectionResult::AlreadyExists => {
            tracing::info!("Found {} on {tracker} - exists", new_meta.name);
            if linked_new_files {
                tracing::info!(
                    "Rechecking {} as new files were linked from {}",
                    new_meta.name,
                    searchee.title
                );
                let _ = client.recheck_torrent(&new_meta.info_hash).await;
                client.resume_injection(new_meta, decision, false).await;
            }
        }
        _ => {
            save_to_output_dir(new_meta, tracker, &config).await;
            if unlink_ok && let Some(destination) = &destination_dir {
                unlink_metafile(new_meta, destination).await;
                linked_new_files = false;
            }
        }
    }

    if injection == InjectionResult::Failure {
        return ActionOutcome::failure(linked_new_files);
    }

    ActionOutcome {
        action_result: ActionResult::Injection(injection),
        client: Some(client),
        destination_dir: destination_dir
            .map(|d| d.to_string_lossy().into_owned())
            .or_else(|| searchee.save_path.clone()),
        linked_new_files,
    }
}

struct ResolvedDestination {
    client: Arc<dyn TorrentClient>,
    destination_dir: PathBuf,
    /// The client already had a save path for this torrent, so it is seeding it.
    from_client: bool,
}

async fn resolve_destination(
    client: Option<Arc<dyn TorrentClient>>,
    searchee: &Searchee,
    save_path: Option<&Path>,
    new_meta: &Metafile,
    tracker: &str,
    config: &RuntimeConfig,
) -> Option<ResolvedDestination> {
    let client = match client {
        Some(client) => client,
        // No client is bound to this searchee: pick one that can actually
        // receive links from where the data lives.
        None => pick_client_for(searchee, save_path, config).await?,
    };

    // get_download_dir works off a Searchee; the new torrent is wrapped in the
    // minimum shape it needs (info hash + file list).
    let new_meta_as_searchee = Searchee {
        info_hash: Some(new_meta.info_hash.clone()),
        name: new_meta.name.clone(),
        title: new_meta.title.clone(),
        files: new_meta.files.clone(),
        length: new_meta.length,
        ..Default::default()
    };
    let (destination_dir, from_client) =
        match client.get_download_dir(&new_meta_as_searchee, false).await {
            Ok(dir) => (PathBuf::from(dir), true),
            Err(DownloadDirError::InvalidData) => return None,
            Err(_) => {
                let link_dir = match save_path {
                    Some(save_path) => get_link_dir(save_path, config).await?,
                    None => get_link_dir_virtual(searchee, config).await?,
                };
                let dir = if config.flat_linking {
                    link_dir
                } else {
                    link_dir.join(tracker)
                };
                (dir, false)
            }
        };

    Some(ResolvedDestination {
        client,
        destination_dir,
        from_client,
    })
}

/// Finds a writable client whose save paths are on the same device as the
/// source data (or can at least be linked into).
async fn pick_client_for(
    searchee: &Searchee,
    save_path: Option<&Path>,
    config: &RuntimeConfig,
) -> Option<Arc<dyn TorrentClient>> {
    let mut src_path: Option<PathBuf> = None;
    for file in &searchee.files {
        let candidate = match save_path {
            Some(save_path) => save_path.join(&file.path),
            None => PathBuf::from(&file.path),
        };
        if exists(&candidate).await {
            src_path = Some(candidate);
            break;
        }
    }
    let src_path = src_path?;
    let src_device = device_of(&src_path).await;

    let probe_type = match config.link_type {
        LinkType::Reflink | LinkType::ReflinkOrCopy => config.link_type,
        _ => LinkType::Hardlink,
    };

    for candidate in get_clients().into_iter().filter(|c| !c.readonly()) {
        let Ok(save_paths) = candidate.get_all_download_dirs(&[], false, false).await else {
            continue;
        };
        if save_paths.is_empty() {
            tracing::debug!(
                "No save paths found to test with for {}, add at least one torrent to the client.",
                candidate.label()
            );
            continue;
        }
        for torrent_save_path in save_paths.values() {
            let torrent_save_path = Path::new(torrent_save_path);
            if src_device.is_some() && device_of(torrent_save_path).await == src_device {
                return Some(candidate);
            }
            let test_path = torrent_save_path.join(CLIENT_DEST_NAME);
            if link_file(&src_path, &test_path, probe_type).await.is_ok() {
                let _ = tokio::fs::remove_file(&test_path).await;
                return Some(candidate);
            }
        }
    }
    None
}

/// Health check: can `src_dir` be linked into any configured `linkDir`?
pub async fn test_linking(
    src_dir: &Path,
    test_src_name: &str,
    test_dest_name: &str,
    config: &RuntimeConfig,
) -> Result<bool, CrustSeedError> {
    let mut temp_file: Option<PathBuf> = None;
    let mut src_file = find_a_file_with_ext(src_dir, &ALL_EXTENSIONS).await;

    if src_file.is_none() {
        let candidate = src_dir.join(test_src_name);
        match tokio::fs::write(&candidate, test_src_name).await {
            Ok(()) => {
                src_file = Some(candidate.clone());
                temp_file = Some(candidate);
            }
            Err(_) => {
                tracing::error!(
                    "crust-seed is unable to verify linking for {} (likely due to incorrect/insufficient volume mounts).",
                    src_dir.display()
                );
                return Ok(false);
            }
        }
    }

    let cleanup = |temp_file: Option<PathBuf>| async move {
        if let Some(temp) = temp_file {
            let _ = tokio::fs::remove_file(temp).await;
        }
    };

    let Some(link_dir) = get_link_dir(src_dir, config).await else {
        cleanup(temp_file).await;
        return Err(link_failure_error(config, src_dir));
    };
    let test_path = link_dir.join(test_dest_name);
    let result = link_file(src_file.as_ref().unwrap(), &test_path, config.link_type).await;
    let _ = tokio::fs::remove_file(&test_path).await;
    cleanup(temp_file).await;

    match result {
        Ok(_) => Ok(true),
        Err(_) => Err(link_failure_error(config, src_dir)),
    }
}

fn link_failure_error(config: &RuntimeConfig, src_dir: &Path) -> CrustSeedError {
    let dirs: Vec<String> = config
        .link_dirs
        .iter()
        .map(|d| format!("\"{d}\""))
        .collect();
    CrustSeedError::new(format!(
        "Failed to create a test {:?} from {} in any linkDirs: [{}]. Ensure that it is supported between these paths (hardlink/reflink requires same drive, partition, and volume).",
        config.link_type,
        src_dir.display(),
        format_as_list(&dirs, false, true)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::torrent::metafile::fixtures::multi_file_torrent;

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

    fn meta(files: &[(&[&str], i64)]) -> Metafile {
        Metafile::decode(&multi_file_torrent("Pack", files)).unwrap()
    }

    #[test]
    fn a_perfect_match_maps_paths_one_to_one() {
        let new_meta = meta(&[(&["a.mkv"], 100), (&["b.mkv"], 200)]);
        let searchee = Searchee {
            info_hash: Some("abc".into()),
            files: vec![file("Pack/a.mkv", 100), file("Pack/b.mkv", 200)],
            length: 300,
            ..Default::default()
        };
        let plan = plan_links(
            &searchee,
            &new_meta,
            Decision::Match,
            Path::new("/links/Tracker"),
            Some(Path::new("/downloads")),
        );
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, PathBuf::from("/downloads/Pack/a.mkv"));
        assert_eq!(plan[0].1, PathBuf::from("/links/Tracker/Pack/a.mkv"));
    }

    /// A renamed release links by size, not by path.
    #[test]
    fn size_only_matches_pair_files_by_length() {
        let new_meta = meta(&[(&["renamed-a.mkv"], 100), (&["renamed-b.mkv"], 200)]);
        let searchee = Searchee {
            info_hash: Some("abc".into()),
            files: vec![file("Other/x.mkv", 200), file("Other/y.mkv", 100)],
            length: 300,
            ..Default::default()
        };
        let plan = plan_links(
            &searchee,
            &new_meta,
            Decision::MatchSizeOnly,
            Path::new("/links"),
            Some(Path::new("/downloads")),
        );
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, PathBuf::from("/downloads/Other/y.mkv"));
        assert_eq!(plan[0].1, PathBuf::from("/links/Pack/renamed-a.mkv"));
        assert_eq!(plan[1].0, PathBuf::from("/downloads/Other/x.mkv"));
    }

    /// Each source file may only be claimed once; a partial match simply omits
    /// the files it cannot satisfy.
    #[test]
    fn equal_length_source_files_are_not_reused() {
        let new_meta = meta(&[(&["a.mkv"], 100), (&["b.mkv"], 100)]);
        let searchee = Searchee {
            info_hash: Some("abc".into()),
            files: vec![file("Other/x.mkv", 100)],
            length: 100,
            ..Default::default()
        };
        let plan = plan_links(
            &searchee,
            &new_meta,
            Decision::MatchPartial,
            Path::new("/links"),
            Some(Path::new("/downloads")),
        );
        assert_eq!(plan.len(), 1);
    }

    /// A virtual searchee's file paths are already absolute.
    #[test]
    fn virtual_searchees_link_from_absolute_paths() {
        let new_meta = meta(&[(&["a.mkv"], 100)]);
        let searchee = Searchee {
            files: vec![file("/data/episodes/a.mkv", 100)],
            length: 100,
            ..Default::default()
        };
        let plan = plan_links(
            &searchee,
            &new_meta,
            Decision::MatchPartial,
            Path::new("/links"),
            None,
        );
        assert_eq!(plan[0].0, PathBuf::from("/data/episodes/a.mkv"));
    }

    #[tokio::test]
    async fn hardlinking_creates_the_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mkv");
        let dest = dir.path().join("dest.mkv");
        tokio::fs::write(&src, b"data").await.unwrap();

        assert!(link_file(&src, &dest, LinkType::Hardlink).await.unwrap());
        assert!(dest.exists());
        // A second call reports "nothing new", not an error.
        assert!(!link_file(&src, &dest, LinkType::Hardlink).await.unwrap());
    }

    #[tokio::test]
    async fn symlinks_resolve_to_absolute_targets() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mkv");
        let dest = dir.path().join("dest.mkv");
        tokio::fs::write(&src, b"data").await.unwrap();

        assert!(link_file(&src, &dest, LinkType::Symlink).await.unwrap());
        let target = tokio::fs::read_link(&dest).await.unwrap();
        assert!(target.is_absolute());
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"data");
    }

    /// Linking must follow a symlinked source to the real file, so the link
    /// does not break when the intermediate symlink is removed.
    #[tokio::test]
    async fn symlinked_sources_are_unwrapped_before_hardlinking() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.mkv");
        let link = dir.path().join("link.mkv");
        let dest = dir.path().join("dest.mkv");
        tokio::fs::write(&real, b"data").await.unwrap();
        tokio::fs::symlink(&real, &link).await.unwrap();

        assert_eq!(unwrap_symlinks(&link).await.unwrap(), real);
        assert!(link_file(&link, &dest, LinkType::Hardlink).await.unwrap());
        tokio::fs::remove_file(&link).await.unwrap();
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"data");
    }

    #[tokio::test]
    async fn symlink_loops_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        tokio::fs::symlink(&b, &a).await.unwrap();
        tokio::fs::symlink(&a, &b).await.unwrap();
        assert!(unwrap_symlinks(&a).await.is_err());
    }

    #[tokio::test]
    async fn linking_a_metafile_reports_new_and_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        let links = dir.path().join("links");
        tokio::fs::create_dir_all(downloads.join("Pack"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(&links).await.unwrap();
        tokio::fs::write(downloads.join("Pack/a.mkv"), vec![0u8; 100])
            .await
            .unwrap();

        let new_meta = meta(&[(&["a.mkv"], 100)]);
        let searchee = Searchee {
            info_hash: Some("abc".into()),
            files: vec![file("Pack/a.mkv", 100)],
            length: 100,
            ..Default::default()
        };

        let result = link_all_files_in_metafile(
            &searchee,
            &new_meta,
            Decision::Match,
            &links,
            Some(&downloads),
            false,
            LinkType::Hardlink,
        )
        .await
        .unwrap();
        assert!(result.linked_new_files);
        assert!(!result.already_existed);
        assert!(links.join("Pack/a.mkv").exists());

        // Second run: everything is already there.
        let again = link_all_files_in_metafile(
            &searchee,
            &new_meta,
            Decision::Match,
            &links,
            Some(&downloads),
            false,
            LinkType::Hardlink,
        )
        .await
        .unwrap();
        assert!(!again.linked_new_files);
        assert!(again.already_existed);
    }

    #[tokio::test]
    async fn a_missing_source_is_fatal_unless_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        let links = dir.path().join("links");
        tokio::fs::create_dir_all(&downloads).await.unwrap();
        tokio::fs::create_dir_all(&links).await.unwrap();

        let new_meta = meta(&[(&["a.mkv"], 100)]);
        let searchee = Searchee {
            info_hash: Some("abc".into()),
            files: vec![file("Pack/a.mkv", 100)],
            length: 100,
            ..Default::default()
        };

        assert!(
            link_all_files_in_metafile(
                &searchee,
                &new_meta,
                Decision::Match,
                &links,
                Some(&downloads),
                false,
                LinkType::Hardlink,
            )
            .await
            .is_err()
        );
        assert!(
            link_all_files_in_metafile(
                &searchee,
                &new_meta,
                Decision::Match,
                &links,
                Some(&downloads),
                true,
                LinkType::Hardlink,
            )
            .await
            .is_ok()
        );
    }

    /// The rollback must never touch the destination directory itself.
    #[tokio::test]
    async fn unlinking_removes_only_the_linked_roots() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("links");
        tokio::fs::create_dir_all(destination.join("Pack"))
            .await
            .unwrap();
        tokio::fs::write(destination.join("Pack/a.mkv"), b"x")
            .await
            .unwrap();
        tokio::fs::write(destination.join("unrelated.txt"), b"x")
            .await
            .unwrap();

        let new_meta = meta(&[(&["a.mkv"], 100)]);
        unlink_metafile(&new_meta, &destination).await;

        assert!(!destination.join("Pack").exists());
        assert!(destination.exists());
        assert!(destination.join("unrelated.txt").exists());
    }

    #[tokio::test]
    async fn the_link_dir_is_chosen_by_device_when_possible() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("downloads");
        let links = dir.path().join("links");
        tokio::fs::create_dir_all(&source).await.unwrap();
        tokio::fs::create_dir_all(&links).await.unwrap();
        tokio::fs::write(source.join("a.mkv"), b"x").await.unwrap();

        let mut config = default_runtime_config();
        config.link_dirs = vec![links.to_string_lossy().into_owned()];
        config.link_type = LinkType::Hardlink;

        assert_eq!(get_link_dir(&source, &config).await, Some(links));
    }

    #[tokio::test]
    async fn finding_a_media_file_descends_into_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("sub"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("sub/a.mkv"), b"x")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("readme.txt"), b"x")
            .await
            .unwrap();

        let found = find_a_file_with_ext(dir.path(), &ALL_EXTENSIONS)
            .await
            .unwrap();
        assert!(found.ends_with("a.mkv"));
    }

    #[tokio::test]
    async fn saving_to_the_output_dir_writes_the_torrent() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = default_runtime_config();
        config.output_dir = dir.path().to_string_lossy().into_owned();

        let new_meta = meta(&[(&["a.mkv"], 100)]);
        save_to_output_dir(&new_meta, "TrackerName", &config).await;

        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let entry = entries.next_entry().await.unwrap().unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let media_type = crate::searchee::media_type_of(&new_meta.title, &new_meta.files).as_str();
        assert!(
            name.starts_with(&format!("[{media_type}][TrackerName]Pack[")),
            "got {name}"
        );
        assert!(name.ends_with(&format!("[{}].torrent", new_meta.info_hash)));
    }
}
