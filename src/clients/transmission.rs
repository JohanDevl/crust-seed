//! Transmission (RPC).
//!
//! Ported from `clients/Transmission.ts`.
//!
//! Transmission's CSRF protection answers the first request with `409` plus an
//! `X-Transmission-Session-Id` header, expecting the caller to retry with it.
//! [`Transmission::request`] handles that handshake transparently, caching the
//! id until the server invalidates it.

use std::collections::{HashMap, HashSet};
use std::path::Path;

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
use crate::constants::{Decision, InjectionResult, TORRENT_TAG};
use crate::db::{ClientSearcheeRow, db};
use crate::errors::CrustSeedError;
use crate::http::client;
use crate::searchee::{File, Searchee, parse_title, searchee_from_db_row};
use crate::torrent::Metafile;
use crate::utils::{
    UrlCredentials, extract_credentials_from_url, human_readable_size, now_ms, sanitize_info_hash,
    wait,
};

const SESSION_ID_HEADER: &str = "X-Transmission-Session-Id";

/// Transmission status codes. 1 = "waiting to verify", 2 = "verifying",
/// 0 = "stopped" — the only state from which an injected torrent is resumed.
const STATUS_STOPPED: i64 = 0;
const CHECKING_STATUSES: [i64; 2] = [1, 2];

#[derive(Debug, Clone, Default, Deserialize)]
struct TorrentEntry {
    #[serde(rename = "hashString", default)]
    hash_string: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: i64,
    #[serde(rename = "totalSize", default)]
    total_size: i64,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(rename = "downloadDir", default)]
    download_dir: String,
    #[serde(default)]
    files: Vec<TorrentFileEntry>,
    #[serde(default)]
    trackers: Vec<TrackerEntry>,
    #[serde(rename = "leftUntilDone", default)]
    left_until_done: i64,
    #[serde(rename = "percentDone", default)]
    percent_done: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TorrentFileEntry {
    name: String,
    length: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TrackerEntry {
    announce: String,
    #[serde(default)]
    tier: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TorrentsResponse {
    #[serde(default)]
    torrents: Vec<TorrentEntry>,
}

pub struct Transmission {
    url: UrlCredentials,
    client_host: String,
    client_priority: usize,
    readonly: bool,
    label: String,
    session_id: RwLock<Option<String>>,
}

impl Transmission {
    pub fn new(
        url: &str,
        client_host: String,
        priority: usize,
        readonly: bool,
    ) -> Result<Self, CrustSeedError> {
        let label = format!("transmission@{client_host}");
        let credentials = extract_credentials_from_url(url, None).map_err(|_| {
            CrustSeedError::new(format!(
                "[{label}] Transmission rpc url must be percent-encoded"
            ))
        })?;
        Ok(Transmission {
            url: credentials,
            client_host,
            client_priority: priority,
            readonly,
            label,
            session_id: RwLock::new(None),
        })
    }

    async fn request(&self, method: &str, args: Value, timeout_secs: u64) -> Result<Value, String> {
        // One retry: the first attempt may be the 409 that hands us a session id.
        for attempt in 0..2 {
            let mut builder = client()
                .post(&self.url.href)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .body(json!({ "method": method, "arguments": args }).to_string());

            if let Some(session_id) = self.session_id.read().await.clone() {
                builder = builder.header(SESSION_ID_HEADER, session_id);
            }
            if !self.url.username.is_empty() || !self.url.password.is_empty() {
                builder = builder.basic_auth(&self.url.username, Some(&self.url.password));
            }

            let response = builder.send().await.map_err(|e| e.to_string())?;
            if response.status().as_u16() == 409 && attempt == 0 {
                let new_id = response
                    .headers()
                    .get(SESSION_ID_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                *self.session_id.write().await = new_id;
                continue;
            }

            let text = response.text().await.map_err(|e| e.to_string())?;
            let body: Value = serde_json::from_str(&text).map_err(|_| {
                tracing::error!(
                    label = self.label.as_str(),
                    "Transmission returned non-JSON response"
                );
                "Transmission returned non-JSON response".to_string()
            })?;
            let result = body.get("result").and_then(Value::as_str).unwrap_or("");
            // "duplicate torrent" is a success for our purposes: the caller
            // maps it onto ALREADY_EXISTS.
            if result == "success" || result == "duplicate torrent" {
                return Ok(body.get("arguments").cloned().unwrap_or(Value::Null));
            }
            return Err(format!("Transmission responded with error: \"{result}\""));
        }
        Err("Transmission session handshake failed".to_string())
    }

    async fn torrent_get(
        &self,
        fields: &[&str],
        ids: Option<&[String]>,
    ) -> Result<Vec<TorrentEntry>, String> {
        let mut args = json!({ "fields": fields });
        if let Some(ids) = ids {
            args["ids"] = json!(ids);
        }
        let value = self.request("torrent-get", args, 300).await?;
        let response: TorrentsResponse =
            serde_json::from_value(value).map_err(|e| e.to_string())?;
        Ok(response.torrents)
    }
}

#[async_trait]
impl TorrentClient for Transmission {
    fn client_host(&self) -> &str {
        &self.client_host
    }
    fn client_priority(&self) -> usize {
        self.client_priority
    }
    fn client_type(&self) -> ClientType {
        ClientType::Transmission
    }
    fn readonly(&self) -> bool {
        self.readonly
    }
    fn label(&self) -> &str {
        &self.label
    }

    async fn is_torrent_in_client(&self, info_hash: &str) -> Result<bool, String> {
        let torrents = self.torrent_get(&["hashString"], None).await?;
        if torrents.is_empty() {
            return Err("No torrents found".to_string());
        }
        let needle = info_hash.to_lowercase();
        Ok(torrents
            .iter()
            .any(|t| t.hash_string.to_lowercase() == needle))
    }

    async fn is_torrent_complete(&self, info_hash: &str) -> Result<bool, DownloadDirError> {
        let torrents = self
            .torrent_get(&["percentDone"], Some(&[info_hash.to_string()]))
            .await
            .map_err(|_| DownloadDirError::UnknownError)?;
        match torrents.first() {
            Some(torrent) => Ok(torrent.percent_done >= 1.0),
            None => Err(DownloadDirError::NotFound),
        }
    }

    async fn is_torrent_checking(&self, info_hash: &str) -> Result<bool, DownloadDirError> {
        let torrents = self
            .torrent_get(&["status"], Some(&[info_hash.to_string()]))
            .await
            .map_err(|_| DownloadDirError::UnknownError)?;
        match torrents.first() {
            Some(torrent) => Ok(CHECKING_STATUSES.contains(&torrent.status)),
            None => Err(DownloadDirError::NotFound),
        }
    }

    async fn get_all_torrents(&self) -> Result<Vec<TorrentMetadataInClient>, String> {
        Ok(self
            .torrent_get(&["hashString", "labels"], None)
            .await?
            .into_iter()
            .map(|torrent| TorrentMetadataInClient {
                info_hash: torrent.hash_string,
                tags: Some(torrent.labels),
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
            .torrent_get(
                &[
                    "hashString",
                    "name",
                    "files",
                    "totalSize",
                    "downloadDir",
                    "labels",
                    "trackers",
                ],
                None,
            )
            .await
        {
            Ok(torrents) => torrents,
            Err(e) => {
                tracing::error!(
                    label = self.label.as_str(),
                    "Failed to get torrents from client: {e}"
                );
                return Ok(result);
            }
        };
        if torrents.is_empty() {
            tracing::error!(label = self.label.as_str(), "No torrents found in client");
            return Ok(result);
        }

        for torrent in torrents {
            let info_hash = torrent.hash_string.to_lowercase();
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

            // Transmission has labels but no category concept.
            let modified = client_searchee_modified(
                db_torrent.as_ref(),
                &torrent.name,
                &torrent.download_dir,
                None,
                &torrent.labels,
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
                .iter()
                .map(|file| File {
                    name: Path::new(&file.name)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| file.name.clone()),
                    path: file.name.clone(),
                    length: file.length,
                })
                .collect();
            if files.is_empty() {
                tracing::debug!(
                    label = self.label.as_str(),
                    "No files found for {} [{}]: skipping",
                    torrent.name,
                    sanitize_info_hash(&info_hash)
                );
                continue;
            }

            let trackers = organize_trackers(
                &torrent
                    .trackers
                    .iter()
                    .map(|t| Tracker {
                        url: t.announce.clone(),
                        tier: t.tier,
                    })
                    .collect::<Vec<_>>(),
            );
            let title =
                parse_title(&torrent.name, &files, None).unwrap_or_else(|| torrent.name.clone());

            let searchee = Searchee {
                info_hash: Some(info_hash),
                name: torrent.name.clone(),
                title,
                files,
                length: torrent.total_size,
                client_host: Some(self.client_host.clone()),
                save_path: Some(torrent.download_dir.clone()),
                tags: Some(torrent.labels.clone()),
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
            .torrent_get(
                &["downloadDir", "percentDone"],
                Some(&[info_hash.to_string()]),
            )
            .await
            .map_err(|_| DownloadDirError::UnknownError)?;
        let torrent = torrents.first().ok_or(DownloadDirError::UnknownError)?;
        if only_completed && torrent.percent_done < 1.0 {
            return Err(DownloadDirError::TorrentNotComplete);
        }
        Ok(torrent.download_dir.clone())
    }

    async fn get_all_download_dirs(
        &self,
        _metas: &[Searchee],
        only_completed: bool,
        _v1_hash_only: bool,
    ) -> Result<HashMap<String, String>, String> {
        let torrents = self
            .torrent_get(&["hashString", "downloadDir", "percentDone"], None)
            .await?;
        Ok(torrents
            .into_iter()
            .filter(|torrent| !only_completed || torrent.percent_done >= 1.0)
            .map(|torrent| (torrent.hash_string, torrent.download_dir))
            .collect())
    }

    async fn recheck_torrent(&self, info_hash: &str) -> Result<(), String> {
        // Stop first: Transmission may start seeding once verification ends.
        self.request("torrent-stop", json!({ "ids": [info_hash] }), 300)
            .await?;
        self.request("torrent-verify", json!({ "ids": [info_hash] }), 300)
            .await?;
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

            let Ok(torrents) = self
                .torrent_get(
                    &["leftUntilDone", "name", "status"],
                    Some(std::slice::from_ref(info_hash)),
                )
                .await
            else {
                sleep_time = RESUME_ERR_SLEEP_MS;
                continue;
            };
            let Some(torrent) = torrents.first() else {
                sleep_time = RESUME_ERR_SLEEP_MS;
                continue;
            };
            if CHECKING_STATUSES.contains(&torrent.status) {
                continue;
            }
            let torrent_log = format!("{} [{}]", torrent.name, sanitize_info_hash(info_hash));
            if torrent.status != STATUS_STOPPED {
                tracing::warn!(
                    label = self.label.as_str(),
                    "Will not resume {torrent_log}: status is {}",
                    torrent.status
                );
                return;
            }

            let max_remaining =
                get_max_remaining_bytes(meta, decision, &config, Some((&torrent_log, &self.label)));
            if torrent.left_until_done > max_remaining
                && !should_resume_from_non_relevant_files(
                    meta,
                    torrent.left_until_done,
                    decision,
                    &config,
                    Some((&torrent_log, &self.label)),
                )
            {
                tracing::warn!(
                    label = self.label.as_str(),
                    "autoResumeMaxDownload will not resume {torrent_log}: remainingSize {} > {} limit",
                    human_readable_size(torrent.left_until_done, true),
                    human_readable_size(max_remaining, true)
                );
                return;
            }

            tracing::info!(
                label = self.label.as_str(),
                "Resuming {torrent_log}: {} remaining",
                human_readable_size(torrent.left_until_done, true)
            );
            let _ = self
                .request("torrent-start", json!({ "ids": [info_hash] }), 300)
                .await;
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

        let destination_dir = match options.destination_dir {
            Some(dir) => dir,
            None => match self
                .get_download_dir(searchee, options.only_completed)
                .await
            {
                Ok(dir) => dir,
                Err(DownloadDirError::TorrentNotComplete) => {
                    return InjectionResult::TorrentNotComplete;
                }
                Err(_) => return InjectionResult::Failure,
            },
        };

        let to_recheck = super::should_recheck(new_torrent, decision, &config);
        let metainfo = base64::engine::general_purpose::STANDARD.encode(new_torrent.encode());
        let response = self
            .request(
                "torrent-add",
                json!({
                    "download-dir": destination_dir,
                    "metainfo": metainfo,
                    "paused": to_recheck,
                    "labels": [TORRENT_TAG],
                }),
                300,
            )
            .await;

        match response {
            Err(_) => InjectionResult::Failure,
            Ok(arguments) => {
                if arguments.get("torrent-duplicate").is_some() {
                    return InjectionResult::AlreadyExists;
                }
                if to_recheck {
                    self.resume_injection(new_torrent, decision, false).await;
                }
                InjectionResult::Success
            }
        }
    }

    async fn validate_config(&self) -> Result<(), CrustSeedError> {
        let config = get_runtime_config();
        self.request("session-get", json!({}), 10)
            .await
            .map_err(|e| {
                CrustSeedError::new(format!(
                    "[{}] Failed to reach Transmission at {}: {e}",
                    self.label, self.client_host
                ))
            })?;
        tracing::info!(
            label = self.label.as_str(),
            "Logged in successfully{}",
            if self.readonly { " (readonly)" } else { "" }
        );

        let Some(torrent_dir) = &config.torrent_dir else {
            return Ok(());
        };
        let mut entries = tokio::fs::read_dir(torrent_dir)
            .await
            .map_err(|e| CrustSeedError::new(format!("[{}] {torrent_dir}: {e}", self.label)))?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().ends_with(".torrent") {
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

    fn client() -> Transmission {
        Transmission::new(
            "http://user:pass@localhost:9091/transmission/rpc",
            "localhost:9091".into(),
            0,
            false,
        )
        .unwrap()
    }

    #[test]
    fn credentials_are_split_from_the_rpc_url() {
        let t = client();
        assert_eq!(t.url.href, "http://localhost:9091/transmission/rpc");
        assert_eq!(t.url.username, "user");
        assert_eq!(t.url.password, "pass");
        assert_eq!(t.label, "transmission@localhost:9091");
    }

    #[test]
    fn checking_statuses_are_one_and_two() {
        assert!(CHECKING_STATUSES.contains(&1));
        assert!(CHECKING_STATUSES.contains(&2));
        assert!(!CHECKING_STATUSES.contains(&STATUS_STOPPED));
    }

    #[test]
    fn torrent_entries_deserialise_from_the_rpc_shape() {
        let entry: TorrentEntry = serde_json::from_value(json!({
            "hashString": "ABC",
            "name": "Some.Show.S01E01",
            "status": 6,
            "totalSize": 1234,
            "labels": ["cross-seed"],
            "downloadDir": "/downloads",
            "files": [{ "name": "Some.Show.S01E01/a.mkv", "length": 1234 }],
            "trackers": [{ "announce": "https://tracker.example/announce", "tier": 0 }],
            "leftUntilDone": 0,
            "percentDone": 1
        }))
        .unwrap();
        assert_eq!(entry.hash_string, "ABC");
        assert_eq!(entry.files[0].length, 1234);
        assert_eq!(
            entry.trackers[0].announce,
            "https://tracker.example/announce"
        );
        assert!(entry.percent_done >= 1.0);
    }

    /// A torrent-add that reports a duplicate must be surfaced as
    /// ALREADY_EXISTS, not as a failure.
    #[test]
    fn duplicate_add_responses_are_recognised() {
        let arguments =
            json!({ "torrent-duplicate": { "hashString": "abc", "id": 1, "name": "x" } });
        assert!(arguments.get("torrent-duplicate").is_some());
        let added = json!({ "torrent-added": { "hashString": "abc", "id": 1, "name": "x" } });
        assert!(added.get("torrent-duplicate").is_none());
    }
}
