//! Deluge (WebUI JSON-RPC).
//!
//! Ported from `clients/Deluge.ts`.
//!
//! Deluge's WebUI is a thin JSON-RPC proxy in front of the daemon, which means
//! two failure modes crust-seed has to handle explicitly: the session cookie
//! expiring (error code 1 → re-authenticate and retry once) and the WebUI
//! itself losing its connection to the daemon (`web.connected` false → pick the
//! first configured host and reconnect).
//!
//! Deluge has no categories; the Label *plugin* provides the closest thing, and
//! it may not be installed — every label operation is therefore best-effort.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use super::{
    ClientSearcheeResult, ClientType, DownloadDirError, GetSearcheesOptions, InjectOptions,
    RESUME_ERR_SLEEP_MS, RESUME_SLEEP_MS, TorrentClient, TorrentMetadataInClient, Tracker,
    client_searchee_modified, get_max_remaining_bytes, get_resume_stop_time, organize_trackers,
    should_resume_from_non_relevant_files,
};
use crate::config::runtime::get_runtime_config;
use crate::constants::{Decision, InjectionResult, TORRENT_CATEGORY_SUFFIX, TORRENT_TAG};
use crate::db::{ClientSearcheeRow, db};
use crate::errors::CrustSeedError;
use crate::searchee::{File, Searchee, parse_title, searchee_from_db_row};
use crate::torrent::Metafile;
use crate::utils::{
    UrlCredentials, extract_credentials_from_url, human_readable_size, now_ms, sanitize_info_hash,
    wait,
};

/// Deluge JSON-RPC error codes. Only NO_AUTH is acted on.
const ERROR_NO_AUTH: i64 = 1;

#[derive(Debug, Clone, Default, Deserialize)]
struct TorrentInfo {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    save_path: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    progress: Option<f64>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    total_size: Option<i64>,
    #[serde(default)]
    total_remaining: Option<i64>,
    #[serde(default)]
    files: Option<Vec<DelugeFile>>,
    #[serde(default)]
    trackers: Option<Vec<DelugeTracker>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DelugeFile {
    path: String,
    size: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct DelugeTracker {
    url: String,
    #[serde(default)]
    tier: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TorrentStatus {
    #[serde(default)]
    torrents: Option<HashMap<String, TorrentInfo>>,
}

#[derive(Debug, Clone)]
pub struct DelugeError {
    pub message: String,
    pub code: Option<i64>,
}

pub struct Deluge {
    url: UrlCredentials,
    client_host: String,
    client_priority: usize,
    readonly: bool,
    label: String,
    http: reqwest::Client,
    request_id: AtomicU64,
    label_plugin_enabled: RwLock<bool>,
}

impl Deluge {
    pub fn new(
        url: &str,
        client_host: String,
        priority: usize,
        readonly: bool,
    ) -> Result<Self, CrustSeedError> {
        let label = format!("deluge@{client_host}");
        let credentials = extract_credentials_from_url(url, None).map_err(|_| {
            CrustSeedError::new(format!(
                "[{label}] The Deluge WebUI URL must be percent-encoded"
            ))
        })?;
        // The cookie jar is what carries the Deluge session.
        let http = crate::http::cookie_client()
            .map_err(|e| CrustSeedError::new(format!("[{label}] {e}")))?;

        Ok(Deluge {
            url: credentials,
            client_host,
            client_priority: priority,
            readonly,
            label,
            http,
            request_id: AtomicU64::new(0),
            label_plugin_enabled: RwLock::new(false),
        })
    }

    /// One JSON-RPC round trip, with no retry.
    async fn call_once(&self, method: &str, params: Value) -> Result<Value, DelugeError> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let response = self
            .http
            .post(&self.url.href)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(std::time::Duration::from_secs(10))
            .body(json!({ "method": method, "params": params, "id": id }).to_string())
            .send()
            .await
            .map_err(|e| DelugeError {
                message: if e.is_timeout() {
                    format!(
                        "[{}] Deluge method {method} timed out after 10 seconds",
                        self.label
                    )
                } else {
                    format!(
                        "[{}] Failed to connect to Deluge at {}",
                        self.label, self.url.href
                    )
                },
                code: None,
            })?;

        let text = response.text().await.map_err(|e| DelugeError {
            message: e.to_string(),
            code: None,
        })?;
        let body: Value = serde_json::from_str(&text).map_err(|e| DelugeError {
            message: format!(
                "[{}] Deluge method {method} response was non-JSON {e}",
                self.label
            ),
            code: None,
        })?;

        if let Some(error) = body.get("error").filter(|e| !e.is_null()) {
            return Err(DelugeError {
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                code: error.get("code").and_then(Value::as_i64),
            });
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    /// A call that re-authenticates once if the session has expired.
    async fn call(&self, method: &str, params: Value) -> Result<Value, DelugeError> {
        match self.call_once(method, params.clone()).await {
            Err(error) if error.code == Some(ERROR_NO_AUTH) => {
                self.authenticate().await.map_err(|e| DelugeError {
                    message: e.to_string(),
                    code: None,
                })?;
                self.call_once(method, params).await
            }
            other => other,
        }
    }

    /// `web.update_ui` — Deluge's one query endpoint, returning the requested
    /// fields for every torrent matching the filter.
    async fn update_ui(
        &self,
        fields: &[&str],
        filter: Value,
    ) -> Result<HashMap<String, TorrentInfo>, DelugeError> {
        let value = self.call("web.update_ui", json!([fields, filter])).await?;
        let status: TorrentStatus = serde_json::from_value(value).map_err(|e| DelugeError {
            message: e.to_string(),
            code: None,
        })?;
        status.torrents.ok_or(DelugeError {
            message: "Client returned unexpected response (object missing)".into(),
            code: None,
        })
    }

    pub async fn authenticate(&self) -> Result<(), CrustSeedError> {
        if self.url.password.is_empty() {
            return Err(CrustSeedError::new(format!(
                "[{}] You need to define a password in the Deluge WebUI URL. (e.g. http://:<PASSWORD>@localhost:8112)",
                self.label
            )));
        }

        let authenticated = self
            .call_once("auth.login", json!([self.url.password]))
            .await
            .map_err(|e| CrustSeedError::new(e.message))?;
        if authenticated.as_bool() != Some(true) {
            return Err(CrustSeedError::new(format!(
                "[{}] Reached Deluge, but failed to authenticate: {}",
                self.label, self.url.href
            )));
        }

        // The WebUI can be up while its connection to the daemon is down.
        if let Ok(connected) = self.call_once("web.connected", json!([])).await
            && connected.as_bool() == Some(false)
        {
            tracing::warn!(
                label = self.label.as_str(),
                "Deluge WebUI disconnected from daemon...attempting to reconnect."
            );
            let hosts = self
                .call_once("web.get_hosts", json!([]))
                .await
                .map_err(|e| CrustSeedError::new(e.message))?;
            let first_host = hosts
                .as_array()
                .and_then(|hosts| hosts.first())
                .and_then(|host| host.as_array())
                .and_then(|host| host.first())
                .cloned()
                .ok_or_else(|| {
                    CrustSeedError::new(format!(
                        "[{}] failed to get host-list for reconnect",
                        self.label
                    ))
                })?;
            self.call_once("web.connect", json!([first_host]))
                .await
                .map_err(|_| {
                    CrustSeedError::new(format!(
                        "[{}] Unable to connect WebUI to Deluge daemon. Connect to the WebUI to resolve this.",
                        self.label
                    ))
                })?;
            tracing::info!(
                label = self.label.as_str(),
                "Deluge WebUI connected to the daemon."
            );
        }

        // call_once, not call: `call` re-enters authenticate() on a session
        // error, and we are inside authenticate().
        let label_enabled = self
            .call_once("core.get_enabled_plugins", json!([]))
            .await
            .map(|plugins| {
                plugins
                    .as_array()
                    .is_some_and(|plugins| plugins.iter().any(|p| p.as_str() == Some("Label")))
            })
            .unwrap_or(false);
        *self.label_plugin_enabled.write().await = label_enabled;

        Ok(())
    }

    /// Deluge reports completeness three different ways depending on state.
    fn is_complete(torrent: &TorrentInfo) -> bool {
        torrent.state.as_deref() == Some("Seeding")
            || torrent.progress == Some(100.0)
            || (torrent.state.as_deref() == Some("Paused")
                && torrent.total_remaining.unwrap_or(0) == 0)
    }

    async fn get_torrent_info(&self, info_hash: &str) -> Result<TorrentInfo, DelugeError> {
        let torrents = self
            .update_ui(
                &[
                    "name",
                    "state",
                    "progress",
                    "save_path",
                    "label",
                    "total_remaining",
                ],
                json!({ "hash": info_hash }),
            )
            .await?;
        torrents.get(info_hash).cloned().ok_or(DelugeError {
            message: format!("Torrent not found in client ({info_hash})"),
            code: None,
        })
    }

    /// The label an injected torrent should carry.
    ///
    /// Without `duplicateCategories` the original label is reused; with it, a
    /// `.cross-seed` suffix separates the injected copy so the user can seed it
    /// under different rules.
    fn calculate_label(&self, searchee: &Searchee, torrent_label: Option<&str>) -> String {
        let config = get_runtime_config();
        let (Some(_), Some(original)) =
            (&searchee.info_hash, torrent_label.filter(|l| !l.is_empty()))
        else {
            return TORRENT_TAG.to_string();
        };
        if !config.duplicate_categories {
            return original.to_string();
        }
        let should_suffix = !original.ends_with(TORRENT_CATEGORY_SUFFIX)
            && Some(original) != config.link_category.as_deref();
        if should_suffix {
            format!("{original}{TORRENT_CATEGORY_SUFFIX}")
        } else {
            original.to_string()
        }
    }

    /// Best-effort labelling: the Label plugin may not be installed, and a
    /// failure must not fail the injection.
    async fn set_label(&self, info_hash: &str, label: &str) {
        if !*self.label_plugin_enabled.read().await {
            return;
        }
        let existing = match self.call("label.get_labels", json!([])).await {
            Ok(value) => value,
            Err(_) => {
                *self.label_plugin_enabled.write().await = false;
                tracing::warn!(label = self.label.as_str(), "Labels have been disabled.");
                return;
            }
        };
        let has_label = existing
            .as_array()
            .is_some_and(|labels| labels.iter().any(|l| l.as_str() == Some(label)));
        if !has_label {
            let _ = self.call("label.add", json!([label])).await;
            wait(300).await;
        }
        if let Err(e) = self
            .call("label.set_torrent", json!([info_hash, label]))
            .await
        {
            tracing::warn!(
                label = self.label.as_str(),
                "Failed to label {info_hash} as {label}: {}",
                e.message
            );
        }
    }
}

#[async_trait]
impl TorrentClient for Deluge {
    fn client_host(&self) -> &str {
        &self.client_host
    }
    fn client_priority(&self) -> usize {
        self.client_priority
    }
    fn client_type(&self) -> ClientType {
        ClientType::Deluge
    }
    fn readonly(&self) -> bool {
        self.readonly
    }
    fn label(&self) -> &str {
        &self.label
    }

    async fn is_torrent_in_client(&self, info_hash: &str) -> Result<bool, String> {
        let info_hash = info_hash.to_lowercase();
        // Deluge already lowercases its hashes.
        let torrents = self
            .update_ui(&[], json!({ "hash": info_hash }))
            .await
            .map_err(|e| e.message)?;
        Ok(torrents.contains_key(&info_hash))
    }

    async fn is_torrent_complete(&self, info_hash: &str) -> Result<bool, DownloadDirError> {
        match self.get_torrent_info(info_hash).await {
            Ok(torrent) => Ok(Self::is_complete(&torrent)),
            Err(_) => Err(DownloadDirError::NotFound),
        }
    }

    async fn is_torrent_checking(&self, info_hash: &str) -> Result<bool, DownloadDirError> {
        match self.get_torrent_info(info_hash).await {
            Ok(torrent) => Ok(torrent.state.as_deref() == Some("Checking")),
            Err(_) => Err(DownloadDirError::NotFound),
        }
    }

    async fn get_all_torrents(&self) -> Result<Vec<TorrentMetadataInClient>, String> {
        let torrents = self
            .update_ui(&["hash", "label"], json!({}))
            .await
            .map_err(|e| e.message)?;
        Ok(torrents
            .into_iter()
            .map(|(hash, torrent)| TorrentMetadataInClient {
                info_hash: hash,
                category: Some(torrent.label.unwrap_or_default()),
                ..Default::default()
            })
            .collect())
    }

    async fn get_client_searchees(
        &self,
        options: GetSearcheesOptions,
    ) -> Result<ClientSearcheeResult, String> {
        let mut result = ClientSearcheeResult::default();
        let mut info_hashes: HashSet<String> = HashSet::new();

        let torrents = match self
            .update_ui(
                &[
                    "name",
                    "label",
                    "save_path",
                    "total_size",
                    "files",
                    "trackers",
                ],
                json!({}),
            )
            .await
        {
            Ok(torrents) => torrents,
            Err(e) => {
                tracing::error!(
                    label = self.label.as_str(),
                    "Failed to get torrents from client: {}",
                    e.message
                );
                return Ok(result);
            }
        };
        if torrents.is_empty() {
            tracing::debug!(label = self.label.as_str(), "No torrents found in client");
            return Ok(result);
        }

        for (hash, torrent) in torrents {
            let info_hash = hash.to_lowercase();
            info_hashes.insert(info_hash.clone());

            let db_torrent: Option<ClientSearcheeRow> = sqlx::query_as(
                "SELECT * FROM client_searchee WHERE info_hash = ? AND client_host = ?",
            )
            .bind(&info_hash)
            .bind(&self.client_host)
            .fetch_optional(db())
            .await
            .ok()
            .flatten();

            let name = torrent.name.clone().unwrap_or_default();
            let save_path = torrent.save_path.clone().unwrap_or_default();
            let category = torrent.label.clone().unwrap_or_default();
            // Deluge exposes labels as a category, never as tags.
            let modified = client_searchee_modified(
                db_torrent.as_ref(),
                &name,
                &save_path,
                Some(category.as_str()),
                &[],
            );
            let refresh = match &options.refresh {
                None => false,
                Some(list) if list.is_empty() => true,
                Some(list) => list.contains(&info_hash),
            };
            if !modified && !refresh {
                if !options.new_searchees_only
                    && let Some(row) = &db_torrent
                {
                    result.searchees.push(searchee_from_db_row(row));
                }
                continue;
            }

            let files: Vec<File> = torrent
                .files
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|file| File {
                    name: Path::new(&file.path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| file.path.clone()),
                    path: file.path.clone(),
                    length: file.size,
                })
                .collect();
            if files.is_empty() {
                tracing::debug!(
                    label = self.label.as_str(),
                    "No files found for {name} [{}]: skipping",
                    sanitize_info_hash(&info_hash)
                );
                continue;
            }

            let trackers = organize_trackers(
                &torrent
                    .trackers
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|t| Tracker {
                        url: t.url.clone(),
                        tier: t.tier,
                    })
                    .collect::<Vec<_>>(),
            );
            let title = parse_title(&name, &files, None).unwrap_or_else(|| name.clone());

            let searchee = Searchee {
                info_hash: Some(info_hash),
                name: name.clone(),
                title,
                files,
                length: torrent.total_size.unwrap_or(0),
                client_host: Some(self.client_host.clone()),
                save_path: Some(save_path),
                category: Some(category),
                trackers: Some(trackers),
                ..Default::default()
            };
            result.new_searchees.push(searchee.clone());
            result.searchees.push(searchee);
        }

        super::persist_client_searchees(&self.client_host, &result.new_searchees, &info_hashes)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result)
    }

    async fn get_download_dir(
        &self,
        meta: &Searchee,
        only_completed: bool,
    ) -> Result<String, DownloadDirError> {
        let info_hash = meta
            .info_hash
            .as_deref()
            .ok_or(DownloadDirError::NotFound)?;
        let torrents = self
            .update_ui(
                &["save_path", "progress", "state", "total_remaining"],
                json!({ "hash": info_hash }),
            )
            .await
            .map_err(|_| DownloadDirError::UnknownError)?;
        let torrent = torrents.get(info_hash).ok_or(DownloadDirError::NotFound)?;
        if only_completed && !Self::is_complete(torrent) {
            return Err(DownloadDirError::TorrentNotComplete);
        }
        Ok(torrent.save_path.clone().unwrap_or_default())
    }

    async fn get_all_download_dirs(
        &self,
        _metas: &[Searchee],
        only_completed: bool,
        _v1_hash_only: bool,
    ) -> Result<HashMap<String, String>, String> {
        let torrents = self
            .update_ui(
                &["save_path", "progress", "state", "total_remaining"],
                json!({}),
            )
            .await
            .map_err(|e| e.message)?;
        Ok(torrents
            .into_iter()
            .filter(|(_, torrent)| !only_completed || Self::is_complete(torrent))
            .map(|(hash, torrent)| (hash, torrent.save_path.unwrap_or_default()))
            .collect())
    }

    async fn recheck_torrent(&self, info_hash: &str) -> Result<(), String> {
        // Pause first: Deluge may resume automatically once a recheck ends.
        let _ = self.call("core.pause_torrent", json!([[info_hash]])).await;
        let _ = self.call("core.force_recheck", json!([[info_hash]])).await;
        Ok(())
    }

    async fn resume_injection(&self, meta: &Metafile, decision: Decision, check_once: bool) {
        let config = get_runtime_config();
        let info_hash = &meta.info_hash;
        let mut sleep_time = RESUME_SLEEP_MS;
        let stop_time = get_resume_stop_time();
        let mut stop = false;

        while now_ms() < stop_time {
            if check_once {
                if stop {
                    return;
                }
                stop = true;
            }
            wait(sleep_time).await;

            let Ok(torrent) = self.get_torrent_info(info_hash).await else {
                sleep_time = RESUME_ERR_SLEEP_MS;
                continue;
            };
            if torrent.state.as_deref() == Some("Checking") {
                continue;
            }
            let torrent_log = format!(
                "{} [{}]",
                torrent.name.clone().unwrap_or_default(),
                sanitize_info_hash(info_hash)
            );
            if torrent.state.as_deref() != Some("Paused") {
                tracing::warn!(
                    label = self.label.as_str(),
                    "Will not resume {torrent_log}: state is {}",
                    torrent.state.clone().unwrap_or_default()
                );
                return;
            }

            let remaining = torrent.total_remaining.unwrap_or(0);
            let max_remaining =
                get_max_remaining_bytes(meta, decision, &config, Some((&torrent_log, &self.label)));
            if remaining > max_remaining
                && !should_resume_from_non_relevant_files(
                    meta,
                    remaining,
                    decision,
                    &config,
                    Some((&torrent_log, &self.label)),
                )
            {
                tracing::warn!(
                    label = self.label.as_str(),
                    "autoResumeMaxDownload will not resume {torrent_log}: remainingSize {} > {} limit",
                    human_readable_size(remaining, true),
                    human_readable_size(max_remaining, true)
                );
                return;
            }

            tracing::info!(
                label = self.label.as_str(),
                "Resuming {torrent_log}: {} remaining",
                human_readable_size(remaining, true)
            );
            let _ = self.call("core.resume_torrent", json!([[info_hash]])).await;
            return;
        }

        tracing::warn!(
            label = self.label.as_str(),
            "Will not resume torrent {info_hash}: timeout"
        );
    }

    async fn inject(
        &self,
        new_torrent: &Metafile,
        searchee: &Searchee,
        decision: Decision,
        options: InjectOptions,
    ) -> InjectionResult {
        let config = get_runtime_config();

        match self.is_torrent_in_client(&new_torrent.info_hash).await {
            Err(_) => return InjectionResult::Failure,
            Ok(true) => return InjectionResult::AlreadyExists,
            Ok(false) => {}
        }

        let mut searchee_info: Option<TorrentInfo> = None;
        if options.only_completed
            && let Some(hash) = searchee.info_hash.as_deref()
        {
            match self.get_torrent_info(hash).await {
                Ok(torrent) if Self::is_complete(&torrent) => searchee_info = Some(torrent),
                Ok(_) => return InjectionResult::TorrentNotComplete,
                Err(_) => return InjectionResult::Failure,
            }
        } else if let Some(hash) = searchee.info_hash.as_deref() {
            searchee_info = self.get_torrent_info(hash).await.ok();
        }

        let destination_dir = match (&options.destination_dir, &searchee_info) {
            (Some(dir), _) => dir.clone(),
            (None, Some(info)) => info.save_path.clone().unwrap_or_default(),
            (None, None) => {
                tracing::debug!(
                    label = self.label.as_str(),
                    "Injection failure: {} was missing critical data.",
                    searchee.title
                );
                return InjectionResult::Failure;
            }
        };

        let to_recheck = super::should_recheck(new_torrent, decision, &config);
        let filename = format!("{}.cross-seed.torrent", new_torrent.file_system_safe_name());
        let filedump = base64::engine::general_purpose::STANDARD.encode(new_torrent.encode());

        let add_result = self
            .call(
                "core.add_torrent_file",
                json!([
                    filename,
                    filedump,
                    {
                        "add_paused": to_recheck,
                        "seed_mode": !to_recheck,
                        "download_location": destination_dir,
                    }
                ]),
            )
            .await;

        if let Err(error) = add_result {
            if error.message.contains("already") {
                return InjectionResult::AlreadyExists;
            }
            tracing::debug!(
                label = self.label.as_str(),
                "Injection failed: {}",
                error.message
            );
            return InjectionResult::Failure;
        }

        wait(250).await;
        self.set_label(
            &new_torrent.info_hash,
            &self.calculate_label(
                searchee,
                searchee_info.as_ref().and_then(|i| i.label.as_deref()),
            ),
        )
        .await;

        if to_recheck {
            // A paused torrent will not start rechecking on its own in
            // libtorrent, so it is nudged after a moment.
            wait(1000).await;
            let _ = self.recheck_torrent(&new_torrent.info_hash).await;
            self.resume_injection(new_torrent, decision, false).await;
        }

        InjectionResult::Success
    }

    async fn validate_config(&self) -> Result<(), CrustSeedError> {
        let config = get_runtime_config();
        self.authenticate().await?;
        tracing::info!(
            label = self.label.as_str(),
            "Logged in successfully{}",
            if self.readonly { " (readonly)" } else { "" }
        );
        let _ = self.call_once("auth.delete_session", json!([])).await;

        let Some(torrent_dir) = &config.torrent_dir else {
            return Ok(());
        };
        let mut entries = tokio::fs::read_dir(torrent_dir)
            .await
            .map_err(|e| CrustSeedError::new(format!("[{}] {torrent_dir}: {e}", self.label)))?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().ends_with(".state") {
                return Ok(());
            }
        }
        Err(CrustSeedError::new(format!(
            "[{}] Invalid torrentDir, if no torrents are in client set to null for now",
            self.label
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use crate::config::runtime::set_runtime_config;

    fn client() -> Deluge {
        Deluge::new(
            "http://:secret@localhost:8112/json",
            "localhost:8112".into(),
            0,
            false,
        )
        .unwrap()
    }

    #[test]
    fn the_password_is_taken_from_the_url() {
        let d = client();
        assert_eq!(d.url.password, "secret");
        assert_eq!(d.url.href, "http://localhost:8112/json");
        assert_eq!(d.label, "deluge@localhost:8112");
    }

    /// Deluge reports "done" three different ways depending on state.
    #[test]
    fn completeness_covers_seeding_progress_and_paused() {
        let seeding = TorrentInfo {
            state: Some("Seeding".into()),
            ..Default::default()
        };
        let progressed = TorrentInfo {
            state: Some("Downloading".into()),
            progress: Some(100.0),
            ..Default::default()
        };
        let paused_done = TorrentInfo {
            state: Some("Paused".into()),
            total_remaining: Some(0),
            ..Default::default()
        };
        let paused_partial = TorrentInfo {
            state: Some("Paused".into()),
            total_remaining: Some(1000),
            ..Default::default()
        };

        assert!(Deluge::is_complete(&seeding));
        assert!(Deluge::is_complete(&progressed));
        assert!(Deluge::is_complete(&paused_done));
        assert!(!Deluge::is_complete(&paused_partial));
    }

    #[test]
    fn labels_are_suffixed_only_when_duplicating_categories() {
        let _guard = crate::config::runtime::config_test_guard();
        let d = client();
        let searchee = Searchee {
            info_hash: Some("abc".into()),
            ..Default::default()
        };

        set_runtime_config(RuntimeConfig {
            duplicate_categories: false,
            ..Default::default()
        });
        assert_eq!(d.calculate_label(&searchee, Some("movies")), "movies");

        set_runtime_config(RuntimeConfig {
            duplicate_categories: true,
            link_category: Some("cross-seed-link".into()),
            ..Default::default()
        });
        assert_eq!(
            d.calculate_label(&searchee, Some("movies")),
            "movies.cross-seed"
        );
        // Already suffixed, and the link category itself, are left alone.
        assert_eq!(
            d.calculate_label(&searchee, Some("movies.cross-seed")),
            "movies.cross-seed"
        );
        assert_eq!(
            d.calculate_label(&searchee, Some("cross-seed-link")),
            "cross-seed-link"
        );
        // A data-based searchee has no original label to reuse.
        assert_eq!(
            d.calculate_label(&Searchee::default(), Some("movies")),
            "cross-seed"
        );
    }

    #[test]
    fn torrent_status_deserialises_from_update_ui() {
        let status: TorrentStatus = serde_json::from_value(json!({
            "torrents": {
                "abc": {
                    "name": "Some.Show.S01E01",
                    "save_path": "/downloads",
                    "state": "Seeding",
                    "total_size": 100,
                    "files": [{ "path": "Some.Show.S01E01/a.mkv", "size": 100 }],
                    "trackers": [{ "url": "https://tracker.example/announce", "tier": 0 }]
                }
            }
        }))
        .unwrap();
        let torrents = status.torrents.unwrap();
        assert_eq!(torrents["abc"].files.as_ref().unwrap()[0].size, 100);
        assert_eq!(torrents["abc"].state.as_deref(), Some("Seeding"));
    }
}
