//! qBittorrent (WebUI API v2).
//!
//! Ported from `clients/QBittorrent.ts`.
//!
//! Two qBittorrent-specific concerns dominate this file:
//!
//! * **Content layout.** qBittorrent can add a torrent with "Create Subfolder"
//!   or "Don't Create Subfolder", which changes where the data actually lives
//!   relative to the reported save path. Getting this wrong points a linked
//!   cross-seed at the wrong directory, so the layout is inferred from
//!   `content_path` versus `save_path`.
//! * **Session cookies.** The API answers 403 when the SID expires; every
//!   request retries through a re-login rather than failing the job.
//!
//! qBittorrent 5.2 added stateless API keys, which cross-seed does not support.
//! A URL whose userinfo is a bare `qbt_…` key uses them instead of the cookie
//! flow — see [`api_key_from`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::RwLock;

use super::{
    ClientSearcheeResult, ClientType, DownloadDirError, GetSearcheesOptions, InjectOptions,
    RESUME_ERR_SLEEP_MS, RESUME_SLEEP_MS, TorrentClient, TorrentMetadataInClient, Tracker,
    client_searchee_modified, get_max_remaining_bytes, get_resume_stop_time, organize_trackers,
    should_resume_from_non_relevant_files,
};
use crate::config::runtime::get_runtime_config;
use crate::constants::{
    ABS_WIN_PATH_REGEX, Decision, InjectionResult, TORRENT_CATEGORY_SUFFIX, TORRENT_TAG,
};
use crate::db::{ClientSearcheeRow, db};
use crate::errors::CrustSeedError;
use crate::http::cookie_client;
use crate::searchee::{File, Searchee, parse_title, searchee_from_db_row};
use crate::torrent::Metafile;
use crate::utils::{
    UrlCredentials, extract_credentials_from_url, extract_int, human_readable_size, now_ms,
    sanitize_info_hash, wait,
};

const FORM_URLENCODED: &str = "application/x-www-form-urlencoded";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Preferences {
    #[serde(default)]
    resume_data_storage_type: Option<String>,
    #[serde(default)]
    bypass_auth_subnet_whitelist_enabled: Option<bool>,
    #[serde(default)]
    bypass_local_auth: Option<bool>,
}

/// The subset of `/torrents/info` crust-seed reads. The endpoint returns far
/// more; unknown fields are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
struct TorrentInfo {
    hash: String,
    #[serde(default)]
    infohash_v1: Option<String>,
    #[serde(default)]
    infohash_v2: Option<String>,
    name: String,
    save_path: String,
    content_path: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    tags: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    amount_left: i64,
    #[serde(default)]
    total_size: i64,
    #[serde(default)]
    auto_tmm: bool,
    #[serde(default)]
    files: Option<Vec<TorrentFile>>,
    #[serde(default)]
    trackers: Option<Vec<TorrentTracker>>,
    #[serde(default)]
    tracker: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TorrentFile {
    name: String,
    size: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct TorrentTracker {
    url: String,
    #[serde(default)]
    tier: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct CategoryInfo {
    name: String,
    #[serde(rename = "savePath")]
    save_path: String,
}

/// States in which qBittorrent considers a torrent fully downloaded.
const COMPLETE_STATES: &[&str] = &[
    "uploading",
    "pausedUP",
    "stoppedUP",
    "queuedUP",
    "stalledUP",
    "checkingUP",
    "forcedUP",
];
const CHECKING_STATES: &[&str] = &["checkingDL", "checkingUP"];
const PAUSED_STATES: &[&str] = &["pausedDL", "stoppedDL", "pausedUP", "stoppedUP"];

#[derive(Debug, Clone, Copy, Default)]
struct Version {
    major: i64,
    minor: i64,
    patch: i64,
}

pub struct QBittorrent {
    url: UrlCredentials,
    /// `Some` when the URL carried an API key instead of a password, in which
    /// case no session is ever established.
    api_key: Option<String>,
    client_host: String,
    client_priority: usize,
    readonly: bool,
    label: String,
    http: reqwest::Client,
    version: RwLock<Version>,
}

impl QBittorrent {
    pub fn new(
        url: &str,
        client_host: String,
        priority: usize,
        readonly: bool,
    ) -> Result<Self, CrustSeedError> {
        let label = format!("qbittorrent@{client_host}");
        let credentials = extract_credentials_from_url(url, Some("/api/v2")).map_err(|_| {
            CrustSeedError::new(format!("[{label}] qBittorrent url must be percent-encoded"))
        })?;
        let http = cookie_client().map_err(|e| CrustSeedError::new(format!("[{label}] {e}")))?;
        let api_key = api_key_from(&credentials);

        Ok(QBittorrent {
            url: credentials,
            api_key,
            client_host,
            client_priority: priority,
            readonly,
            label,
            http,
            version: RwLock::new(Version::default()),
        })
    }

    /// POSTs the credentials and stores the SID cookie.
    ///
    /// Kept separate from [`QBittorrent::login`] so the 403-retry path in
    /// [`QBittorrent::request`] can re-authenticate without recursing back
    /// through the version probe (which itself goes through `request`).
    async fn authenticate(&self) -> Result<(), CrustSeedError> {
        // API keys are rejected by `/auth/login` by design — they exist so a
        // client never holds a session. There is nothing to establish here.
        if self.api_key.is_some() {
            return Ok(());
        }
        let response = self
            .http
            .post(format!("{}/auth/login", self.url.href))
            .header(reqwest::header::CONTENT_TYPE, FORM_URLENCODED)
            .body(format!(
                "username={}&password={}",
                urlencode(&self.url.username),
                urlencode(&self.url.password)
            ))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| CrustSeedError::new(format!("qBittorrent login failed: {e}")))?;

        if !response.status().is_success() {
            return Err(CrustSeedError::new(format!(
                "qBittorrent login failed with code {}",
                response.status().as_u16()
            )));
        }
        // qBittorrent answers 200 with the body "Fails." on bad credentials.
        let body = response.text().await.unwrap_or_default();
        if body.trim().eq_ignore_ascii_case("Fails.") {
            return Err(CrustSeedError::new(
                "qBittorrent login failed: Invalid username or password".to_string(),
            ));
        }
        Ok(())
    }

    /// Attaches the API key, if one is configured. Cookie auth needs nothing
    /// here: `cookie_client` replays the SID on its own.
    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    pub async fn login(&self) -> Result<(), CrustSeedError> {
        self.authenticate().await?;

        let version = self
            .request("/app/version", RequestBody::Form(String::new()))
            .await
            .ok_or_else(|| {
                CrustSeedError::new("qBittorrent login failed: Unable to retrieve version")
            })?;

        let parts: Vec<&str> = version.trim().split('.').collect();
        // Under an API key `request` never re-authenticates on 403, so a
        // rejected key arrives here as the body of that response rather than a
        // version string — reporting it as an ancient qBittorrent would send
        // the user looking in the wrong place.
        if self.api_key.is_some() && extract_int(parts.first().copied().unwrap_or("")).is_none() {
            return Err(CrustSeedError::new(
                "qBittorrent rejected the API key".to_string(),
            ));
        }
        let parsed = Version {
            major: extract_int(parts.first().copied().unwrap_or("")).unwrap_or(0),
            minor: extract_int(parts.get(1).copied().unwrap_or("")).unwrap_or(0),
            patch: extract_int(parts.get(2).copied().unwrap_or("")).unwrap_or(0),
        };
        if parsed.major < 4
            || (parsed.major == 4 && parsed.minor < 3)
            || (parsed.major == 4 && parsed.minor == 3 && parsed.patch < 1)
        {
            return Err(CrustSeedError::new(format!(
                "qBittorrent minimum supported version is v4.3.1, current version is {}",
                version.trim()
            )));
        }
        // API keys landed in qBittorrent 5.2.0 (WebAPI 2.14.1). Against an
        // older server every request would simply 403, which reads as a wrong
        // key rather than an unsupported one.
        if self.api_key.is_some() && (parsed.major < 5 || (parsed.major == 5 && parsed.minor < 2)) {
            return Err(CrustSeedError::new(format!(
                "qBittorrent API key authentication requires v5.2.0, current version is {}",
                version.trim()
            )));
        }
        *self.version.write().await = parsed;

        tracing::info!(
            label = self.label.as_str(),
            "{} {} successfully{}",
            if self.api_key.is_some() {
                "Connected to"
            } else {
                "Logged in to"
            },
            version.trim(),
            if self.readonly { " (readonly)" } else { "" }
        );
        Ok(())
    }

    /// qBittorrent 5 renamed pause/resume to stop/start.
    async fn pause_verb(&self) -> &'static str {
        if self.version.read().await.major >= 5 {
            "stop"
        } else {
            "pause"
        }
    }

    async fn resume_verb(&self) -> &'static str {
        if self.version.read().await.major >= 5 {
            "start"
        } else {
            "resume"
        }
    }

    /// POSTs to the API, re-authenticating on 403 and retrying 5xx.
    async fn request(&self, path: &str, body: RequestBody) -> Option<String> {
        const RETRIES: u32 = 3;
        for attempt in 0..=RETRIES {
            let request = self.authorized(
                self.http
                    .post(format!("{}{path}", self.url.href))
                    .timeout(std::time::Duration::from_secs(600)),
            );
            let request = match &body {
                RequestBody::Form(form) => request
                    .header(reqwest::header::CONTENT_TYPE, FORM_URLENCODED)
                    .body(form.clone()),
                RequestBody::Multipart(_) => request, // handled by add_torrent
            };

            match request.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    // A 403 under an API key means the key is wrong, not that
                    // a session lapsed, so retrying would only delay the error.
                    if status == 403 && attempt < RETRIES && self.api_key.is_none() {
                        tracing::debug!(
                            label = self.label.as_str(),
                            "Received 403 from API, re-authenticating and retrying"
                        );
                        let _ = self.authenticate().await;
                        wait(backoff_ms(attempt)).await;
                        continue;
                    }
                    if (500..600).contains(&status) && attempt < RETRIES {
                        tracing::debug!(
                            label = self.label.as_str(),
                            "Received {status} from API, retrying"
                        );
                        wait(backoff_ms(attempt)).await;
                        continue;
                    }
                    return response.text().await.ok();
                }
                Err(e) => {
                    if attempt >= RETRIES {
                        tracing::error!(
                            label = self.label.as_str(),
                            "Request failed after {RETRIES} retries: {e}"
                        );
                        return None;
                    }
                    wait(backoff_ms(attempt)).await;
                }
            }
        }
        None
    }

    pub async fn get_preferences(&self) -> Result<Preferences, CrustSeedError> {
        let text = self
            .request("/app/preferences", RequestBody::Form(String::new()))
            .await
            .ok_or_else(|| {
                CrustSeedError::new(format!(
                    "[{}] qBittorrent failed to retrieve preferences",
                    self.label
                ))
            })?;
        serde_json::from_str(&text)
            .map_err(|e| CrustSeedError::new(format!("[{}] {e}", self.label)))
    }

    async fn get_all_torrent_info(
        &self,
        include_files: bool,
        include_trackers: bool,
    ) -> Vec<TorrentInfo> {
        let mut params = Vec::new();
        if include_files {
            params.push("includeFiles=true");
        }
        if include_trackers {
            params.push("includeTrackers=true");
        }
        let Some(text) = self
            .request("/torrents/info", RequestBody::Form(params.join("&")))
            .await
        else {
            return Vec::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Looks a torrent up by hash, falling back to a full listing.
    ///
    /// The direct `hashes=` query only matches the *primary* hash, so a v2
    /// torrent added under a different hash form is only found by scanning.
    async fn get_torrent_info(&self, hash: &str, retries: u32) -> Option<TorrentInfo> {
        for attempt in 0..=retries {
            if let Some(text) = self
                .request(
                    "/torrents/info",
                    RequestBody::Form(format!("hashes={hash}")),
                )
                .await
                && let Ok(torrents) = serde_json::from_str::<Vec<TorrentInfo>>(&text)
                && let Some(first) = torrents.into_iter().next()
            {
                return Some(first);
            }

            let torrents = self.get_all_torrent_info(false, false).await;
            if let Some(found) = torrents.into_iter().find(|t| matches_hash(t, hash)) {
                return Some(found);
            }
            if attempt < retries {
                wait(backoff_ms(attempt)).await;
            }
        }
        None
    }

    async fn get_files(&self, info_hash: &str) -> Option<Vec<File>> {
        let text = self
            .request(
                "/torrents/files",
                RequestBody::Form(format!("hash={info_hash}")),
            )
            .await?;
        let files: Vec<TorrentFile> = serde_json::from_str(&text).ok()?;
        Some(files.iter().map(torrent_file_to_file).collect())
    }

    async fn get_trackers(&self, info_hash: &str) -> Option<Vec<String>> {
        let text = self
            .request(
                "/torrents/trackers",
                RequestBody::Form(format!("hash={info_hash}")),
            )
            .await?;
        let trackers: Vec<TorrentTracker> = serde_json::from_str(&text).ok()?;
        Some(organize_trackers(
            &trackers
                .into_iter()
                .map(|t| Tracker {
                    url: t.url,
                    tier: t.tier,
                })
                .collect::<Vec<_>>(),
        ))
    }

    pub async fn create_tag(&self) {
        let _ = self
            .request(
                "/torrents/createTags",
                RequestBody::Form(format!("tags={TORRENT_TAG}")),
            )
            .await;
    }

    async fn get_all_categories(&self) -> Vec<CategoryInfo> {
        let Some(text) = self
            .request("/torrents/categories", RequestBody::Form(String::new()))
            .await
        else {
            return Vec::new();
        };
        serde_json::from_str::<HashMap<String, CategoryInfo>>(&text)
            .map(|map| map.into_values().collect())
            .unwrap_or_default()
    }

    async fn create_category(&self, category: &str, save_path: &str) {
        let _ = self
            .request(
                "/torrents/createCategory",
                RequestBody::Form(format!(
                    "category={}&savePath={}",
                    urlencode(category),
                    urlencode(save_path)
                )),
            )
            .await;
    }

    async fn edit_category(&self, category: &str, save_path: &str) {
        let _ = self
            .request(
                "/torrents/editCategory",
                RequestBody::Form(format!(
                    "category={}&savePath={}",
                    urlencode(category),
                    urlencode(save_path)
                )),
            )
            .await;
    }

    /// With `duplicateCategories`, an injected torrent goes into
    /// `<category>.cross-seed` so the user can seed it under different rules.
    async fn category_for_new_torrent(
        &self,
        category: &str,
        save_path: &str,
        auto_tmm: bool,
    ) -> String {
        let config = get_runtime_config();
        if !config.duplicate_categories {
            return category.to_string();
        }
        if category.is_empty() || Some(category) == config.link_category.as_deref() {
            return category.to_string(); // Linking duplicates via tags instead
        }

        let dupe_category = if category.ends_with(TORRENT_CATEGORY_SUFFIX) {
            category.to_string()
        } else {
            format!("{category}{TORRENT_CATEGORY_SUFFIX}")
        };
        if !auto_tmm {
            return dupe_category;
        }

        // Under automatic torrent management the category owns the save path,
        // so the duplicate must be created pointing at the same place.
        let categories = self.get_all_categories().await;
        match categories.iter().find(|c| c.name == dupe_category) {
            None => self.create_category(&dupe_category, save_path).await,
            Some(existing) if existing.save_path != save_path => {
                self.edit_category(&dupe_category, save_path).await
            }
            Some(_) => {}
        }
        dupe_category
    }

    fn tags_for_new_torrent(
        &self,
        searchee_category: Option<&str>,
        destination_dir: Option<&str>,
    ) -> String {
        let config = get_runtime_config();
        if !config.duplicate_categories || destination_dir.is_none() {
            return TORRENT_TAG.to_string();
        }
        let Some(category) = searchee_category.filter(|c| !c.is_empty()) else {
            return TORRENT_TAG.to_string();
        };
        if Some(category) == config.link_category.as_deref() {
            return TORRENT_TAG.to_string();
        }
        if category.ends_with(TORRENT_CATEGORY_SUFFIX) {
            format!("{TORRENT_TAG},{category}")
        } else {
            format!("{TORRENT_TAG},{category}{TORRENT_CATEGORY_SUFFIX}")
        }
    }

    /// True when the torrent was added with "Create Subfolder": a single-file
    /// torrent whose content sits one level below the save path.
    ///
    /// Only meaningful for searchees built from `.torrent` files — the API
    /// already reports the effective layout.
    fn is_subfolder_content_layout(&self, files: &[File], info: &TorrentInfo) -> bool {
        if get_runtime_config().use_client_torrents {
            return false;
        }
        if files.len() != 1 {
            return false;
        }
        if Path::new(&files[0].path)
            .parent()
            .is_some_and(|p| p != Path::new(""))
        {
            return false;
        }
        let content_parent = dirname_for(&info.content_path);
        normalize(&content_parent) != normalize(&info.save_path)
    }

    /// True when the torrent was added with "Don't Create Subfolder": the
    /// client's path is shallower than the torrent's own file tree.
    fn is_no_subfolder_content_layout(&self, files: &[File], info: &TorrentInfo) -> bool {
        if get_runtime_config().use_client_torrents {
            return false;
        }
        if files.len() > 1 {
            return info.content_path == info.save_path;
        }
        if files.is_empty() {
            return false;
        }
        if Path::new(&files[0].path)
            .parent()
            .is_none_or(|p| p == Path::new(""))
        {
            return false;
        }
        let client_path = info
            .content_path
            .strip_prefix(&info.save_path)
            .unwrap_or(&info.content_path);
        crate::utils::get_path_parts(client_path).len()
            < crate::utils::get_path_parts(&files[0].path).len()
    }

    fn correct_save_path(&self, files: &[File], info: &TorrentInfo) -> String {
        if self.is_subfolder_content_layout(files, info) {
            dirname_for(&info.content_path)
        } else {
            info.save_path.clone()
        }
    }

    fn is_complete(info: &TorrentInfo) -> bool {
        COMPLETE_STATES.contains(&info.state.as_str())
    }

    async fn add_torrent(&self, form: reqwest::multipart::Form) -> Result<(), String> {
        let response = self
            .authorized(
                self.http
                    .post(format!("{}/torrents/add", self.url.href))
                    .multipart(form)
                    .timeout(std::time::Duration::from_secs(600)),
            )
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("status {}", response.status().as_u16()))
        }
    }
}

enum RequestBody {
    Form(String),
    #[allow(dead_code)]
    Multipart(()),
}

/// Reads a qBittorrent API key out of the URL's userinfo.
///
/// The key takes the place of the username, with no password:
///
/// ```text
/// qbittorrent:http://qbt_A1b2C3…@192.168.0.25:8080
/// ```
///
/// qBittorrent mints every key with a `qbt_` prefix, so requiring both that
/// prefix and an empty password keeps this from ever capturing a real account
/// whose password merely happens to be missing — that case still goes through
/// `/auth/login` and fails there with the message it deserves.
fn api_key_from(credentials: &UrlCredentials) -> Option<String> {
    (credentials.password.is_empty() && credentials.username.starts_with("qbt_"))
        .then(|| credentials.username.clone())
}

fn backoff_ms(attempt: u32) -> u64 {
    (1000u64 << attempt).min(10_000)
}

fn urlencode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn torrent_file_to_file(file: &TorrentFile) -> File {
    File {
        name: Path::new(&file.name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.name.clone()),
        path: file.name.clone(),
        length: file.size,
    }
}

fn matches_hash(info: &TorrentInfo, hash: &str) -> bool {
    let hash = hash.to_lowercase();
    info.hash.to_lowercase() == hash
        || info.infohash_v1.as_deref().map(str::to_lowercase) == Some(hash.clone())
        || info.infohash_v2.as_deref().map(str::to_lowercase) == Some(hash)
}

/// `path.dirname` picking the Windows or POSIX flavour from the path itself —
/// the client may run on a different platform than crust-seed.
fn dirname_for(path: &str) -> String {
    let separator = if ABS_WIN_PATH_REGEX.is_match(path).unwrap_or(false) {
        '\\'
    } else {
        '/'
    };
    match path.trim_end_matches(separator).rfind(separator) {
        Some(0) => separator.to_string(),
        Some(index) => path[..index].to_string(),
        None => ".".to_string(),
    }
}

fn normalize(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).to_string()
}

fn split_tags(tags: &str) -> Vec<String> {
    if tags.is_empty() {
        return Vec::new();
    }
    tags.split(',')
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect()
}

#[async_trait]
impl TorrentClient for QBittorrent {
    fn client_host(&self) -> &str {
        &self.client_host
    }
    fn client_priority(&self) -> usize {
        self.client_priority
    }
    fn client_type(&self) -> ClientType {
        ClientType::QBittorrent
    }
    fn readonly(&self) -> bool {
        self.readonly
    }
    fn label(&self) -> &str {
        &self.label
    }

    async fn is_torrent_in_client(&self, info_hash: &str) -> Result<bool, String> {
        let torrents = self.get_all_torrent_info(false, false).await;
        if torrents.is_empty() {
            return Err("No torrents found".to_string());
        }
        Ok(torrents.iter().any(|t| matches_hash(t, info_hash)))
    }

    async fn is_torrent_complete(&self, info_hash: &str) -> Result<bool, DownloadDirError> {
        match self.get_torrent_info(info_hash, 0).await {
            Some(info) => Ok(Self::is_complete(&info)),
            None => Err(DownloadDirError::NotFound),
        }
    }

    async fn is_torrent_checking(&self, info_hash: &str) -> Result<bool, DownloadDirError> {
        match self.get_torrent_info(info_hash, 0).await {
            Some(info) => Ok(CHECKING_STATES.contains(&info.state.as_str())),
            None => Err(DownloadDirError::NotFound),
        }
    }

    async fn get_all_torrents(&self) -> Result<Vec<TorrentMetadataInClient>, String> {
        Ok(self
            .get_all_torrent_info(false, true)
            .await
            .into_iter()
            .map(|torrent| TorrentMetadataInClient {
                info_hash: torrent.hash.clone(),
                category: Some(torrent.category.clone()),
                tags: Some(split_tags(&torrent.tags)),
                trackers: match &torrent.trackers {
                    Some(trackers) => Some(organize_trackers(
                        &trackers
                            .iter()
                            .map(|t| Tracker {
                                url: t.url.clone(),
                                tier: t.tier,
                            })
                            .collect::<Vec<_>>(),
                    )),
                    None if !torrent.tracker.is_empty() => Some(vec![torrent.tracker.clone()]),
                    None => None,
                },
            })
            .collect())
    }

    async fn get_client_searchees(
        &self,
        options: GetSearcheesOptions,
    ) -> Result<ClientSearcheeResult, String> {
        let mut result = ClientSearcheeResult::default();
        let mut info_hashes: HashSet<String> = HashSet::new();

        let torrents = self
            .get_all_torrent_info(options.include_files, options.include_trackers)
            .await;
        if torrents.is_empty() {
            tracing::error!(label = self.label.as_str(), "No torrents found in client");
            return Ok(result);
        }

        for torrent in torrents {
            // v1 hash is the stable identity across qBittorrent versions.
            let info_hash = torrent
                .infohash_v1
                .clone()
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| torrent.hash.clone())
                .to_lowercase();
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

            let tags = split_tags(&torrent.tags);
            let modified = client_searchee_modified(
                db_torrent.as_ref(),
                &torrent.name,
                &torrent.save_path,
                Some(torrent.category.as_str()),
                &tags,
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

            let files = match &torrent.files {
                Some(files) => Some(files.iter().map(torrent_file_to_file).collect::<Vec<_>>()),
                None => self.get_files(&torrent.hash).await,
            };
            let Some(files) = files else {
                tracing::debug!(
                    label = self.label.as_str(),
                    "Failed to get files for {} [{}] (likely transient)",
                    torrent.name,
                    sanitize_info_hash(&torrent.hash)
                );
                continue;
            };
            if files.is_empty() {
                tracing::debug!(
                    label = self.label.as_str(),
                    "No files found for {} [{}]: skipping",
                    torrent.name,
                    sanitize_info_hash(&torrent.hash)
                );
                continue;
            }

            let trackers = match &torrent.trackers {
                Some(trackers) => Some(organize_trackers(
                    &trackers
                        .iter()
                        .map(|t| Tracker {
                            url: t.url.clone(),
                            tier: t.tier,
                        })
                        .collect::<Vec<_>>(),
                )),
                None => self.get_trackers(&torrent.hash).await,
            };
            let Some(trackers) = trackers else {
                tracing::debug!(
                    label = self.label.as_str(),
                    "Failed to get trackers for {} [{}] (likely transient)",
                    torrent.name,
                    sanitize_info_hash(&torrent.hash)
                );
                continue;
            };

            let title =
                parse_title(&torrent.name, &files, None).unwrap_or_else(|| torrent.name.clone());
            let searchee = Searchee {
                info_hash: Some(info_hash),
                name: torrent.name.clone(),
                title,
                files,
                length: torrent.total_size,
                client_host: Some(self.client_host.clone()),
                save_path: Some(torrent.save_path.clone()),
                category: Some(torrent.category.clone()),
                tags: Some(tags),
                trackers: Some(trackers),
                ..Default::default()
            };
            result.new_searchees.push(searchee.clone());
            result.searchees.push(searchee);
        }

        crate::clients::persist_client_searchees(
            &self.client_host,
            &result.new_searchees,
            &info_hashes,
        )
        .await
        .map_err(|e| e.to_string())?;

        Ok(result)
    }

    async fn get_download_dir(
        &self,
        meta: &Searchee,
        only_completed: bool,
    ) -> Result<String, DownloadDirError> {
        let config = get_runtime_config();
        let info_hash = meta
            .info_hash
            .as_deref()
            .ok_or(DownloadDirError::NotFound)?;
        let Some(info) = self.get_torrent_info(info_hash, 0).await else {
            return Err(DownloadDirError::NotFound);
        };
        if config.torrent_dir.is_some() && self.is_no_subfolder_content_layout(&meta.files, &info) {
            tracing::error!(
                label = self.label.as_str(),
                "NoSubfolder content layout is not supported with torrentDir, use useClientTorrents: {} [{}]",
                info.name,
                sanitize_info_hash(&info.hash)
            );
            return Err(DownloadDirError::InvalidData);
        }
        if only_completed && !Self::is_complete(&info) {
            return Err(DownloadDirError::TorrentNotComplete);
        }
        Ok(self.correct_save_path(&meta.files, &info))
    }

    async fn get_all_download_dirs(
        &self,
        metas: &[Searchee],
        only_completed: bool,
        v1_hash_only: bool,
    ) -> Result<HashMap<String, String>, String> {
        let torrents = self.get_all_torrent_info(false, false).await;
        let by_hash: HashMap<&str, &Searchee> = metas
            .iter()
            .filter_map(|meta| meta.info_hash.as_deref().map(|hash| (hash, meta)))
            .collect();

        let mut save_paths = HashMap::new();
        for torrent in &torrents {
            let meta = by_hash
                .get(torrent.hash.as_str())
                .or_else(|| torrent.infohash_v2.as_deref().and_then(|h| by_hash.get(h)))
                .or_else(|| torrent.infohash_v1.as_deref().and_then(|h| by_hash.get(h)))
                .copied();

            if only_completed && !Self::is_complete(torrent) {
                continue;
            }
            let save_path = match meta {
                Some(meta) => self.correct_save_path(&meta.files, torrent),
                None => torrent.save_path.clone(),
            };
            if let Some(v1) = torrent.infohash_v1.as_deref().filter(|h| !h.is_empty()) {
                save_paths.insert(v1.to_string(), save_path.clone());
            }
            if v1_hash_only {
                continue;
            }
            save_paths.insert(torrent.hash.clone(), save_path.clone());
            if let Some(v2) = torrent.infohash_v2.as_deref().filter(|h| !h.is_empty()) {
                save_paths.insert(v2.to_string(), save_path);
            }
        }
        Ok(save_paths)
    }

    async fn recheck_torrent(&self, info_hash: &str) -> Result<(), String> {
        // Pause first: qBittorrent may resume automatically after a recheck.
        let pause = self.pause_verb().await;
        let _ = self
            .request(
                &format!("/torrents/{pause}"),
                RequestBody::Form(format!("hashes={info_hash}")),
            )
            .await;
        let _ = self
            .request(
                "/torrents/recheck",
                RequestBody::Form(format!("hashes={info_hash}")),
            )
            .await;
        Ok(())
    }

    /// Waits for a rechecking torrent to settle and resumes it if little enough
    /// data is missing. Runs for at most an hour.
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

            let Some(info) = self.get_torrent_info(info_hash, 0).await else {
                // Client restarting or dropping connections; back off harder.
                sleep_time = RESUME_ERR_SLEEP_MS;
                continue;
            };
            if CHECKING_STATES.contains(&info.state.as_str()) {
                continue;
            }
            let torrent_log = format!("{} [{}]", info.name, sanitize_info_hash(info_hash));
            if !PAUSED_STATES.contains(&info.state.as_str()) {
                tracing::warn!(
                    label = self.label.as_str(),
                    "Will not resume {torrent_log}: state is {}",
                    info.state
                );
                return;
            }

            let max_remaining =
                get_max_remaining_bytes(meta, decision, &config, Some((&torrent_log, &self.label)));
            if info.amount_left > max_remaining
                && !should_resume_from_non_relevant_files(
                    meta,
                    info.amount_left,
                    decision,
                    &config,
                    Some((&torrent_log, &self.label)),
                )
            {
                tracing::warn!(
                    label = self.label.as_str(),
                    "autoResumeMaxDownload will not resume {torrent_log}: remainingSize {} > {} limit",
                    human_readable_size(info.amount_left, true),
                    human_readable_size(max_remaining, true)
                );
                return;
            }

            tracing::info!(
                label = self.label.as_str(),
                "Resuming {torrent_log}: {} remaining",
                human_readable_size(info.amount_left, true)
            );
            let resume = self.resume_verb().await;
            let _ = self
                .request(
                    &format!("/torrents/{resume}"),
                    RequestBody::Form(format!("hashes={info_hash}")),
                )
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

        let searchee_info = match searchee.info_hash.as_deref() {
            Some(hash) => self.get_torrent_info(hash, 0).await,
            None => None,
        };
        if searchee_info.is_none() && options.destination_dir.is_none() {
            tracing::error!(
                label = self.label.as_str(),
                "Searchee torrent may have been deleted: {}",
                searchee.title
            );
            return InjectionResult::Failure;
        }

        let (save_path, is_complete, auto_tmm, category) = match &options.destination_dir {
            Some(dir) => (
                dir.clone(),
                true,
                false,
                config.link_category.clone().unwrap_or_default(),
            ),
            None => {
                let info = searchee_info.as_ref().unwrap();
                (
                    info.save_path.clone(),
                    Self::is_complete(info),
                    info.auto_tmm,
                    info.category.clone(),
                )
            }
        };
        if options.only_completed && !is_complete {
            return InjectionResult::TorrentNotComplete;
        }

        let to_recheck = super::should_recheck(new_torrent, decision, &config);
        let filename = format!(
            "{}.{TORRENT_TAG}.torrent",
            new_torrent.file_system_safe_name()
        );

        let part = reqwest::multipart::Part::bytes(new_torrent.encode())
            .file_name(filename)
            .mime_str("application/x-bittorrent")
            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(new_torrent.encode()));

        let mut form = reqwest::multipart::Form::new().part("torrents", part);
        if !auto_tmm {
            form = form
                .text("downloadPath", save_path.clone())
                .text("savepath", save_path.clone());
        }
        form = form.text("autoTMM", auto_tmm.to_string());
        if !category.is_empty() {
            form = form.text(
                "category",
                self.category_for_new_torrent(&category, &save_path, auto_tmm)
                    .await,
            );
        }
        form = form.text(
            "tags",
            self.tags_for_new_torrent(
                searchee_info.as_ref().map(|i| i.category.as_str()),
                options.destination_dir.as_deref(),
            ),
        );
        form = form.text(
            "contentLayout",
            if options.destination_dir.is_some() {
                "Original".to_string()
            } else if searchee_info
                .as_ref()
                .is_some_and(|info| self.is_subfolder_content_layout(&searchee.files, info))
            {
                "Subfolder".to_string()
            } else {
                "Original".to_string()
            },
        );
        form = form.text("skip_checking", (!to_recheck).to_string());
        let pause_field = if self.version.read().await.major >= 5 {
            "stopped"
        } else {
            "paused"
        };
        form = form.text(pause_field, to_recheck.to_string());
        // qBittorrent's multipart parser concatenates the final value with its
        // boundary sentinel, so a throwaway field is appended last.
        form = form.text("foo", "bar");

        if let Err(e) = self.add_torrent(form).await {
            tracing::error!(
                label = self.label.as_str(),
                "Failed to add torrent (polling client to confirm): {e}"
            );
        }

        let Some(new_info) = self.get_torrent_info(&new_torrent.info_hash, 5).await else {
            tracing::error!(
                label = self.label.as_str(),
                "Failed to retrieve torrent after adding"
            );
            return InjectionResult::Failure;
        };
        if to_recheck {
            let _ = self.recheck_torrent(&new_info.hash).await;
            self.resume_injection(new_torrent, decision, false).await;
        }

        InjectionResult::Success
    }

    async fn validate_config(&self) -> Result<(), CrustSeedError> {
        let config = get_runtime_config();
        self.login()
            .await
            .map_err(|e| CrustSeedError::new(format!("[{}] {e}", self.label)))?;
        self.create_tag().await;

        let Some(torrent_dir) = &config.torrent_dir else {
            return Ok(());
        };
        let preferences = self.get_preferences().await?;
        // With SQLite resume storage there are no .fastresume files to read.
        if preferences.resume_data_storage_type.as_deref() == Some("SQLite") {
            return Err(CrustSeedError::new(format!(
                "[{}] torrentDir is not compatible with SQLite mode in qBittorrent, use useClientTorrents",
                self.label
            )));
        }
        let mut entries = tokio::fs::read_dir(torrent_dir)
            .await
            .map_err(|e| CrustSeedError::new(format!("[{}] {torrent_dir}: {e}", self.label)))?;
        let mut has_fastresume = false;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().ends_with(".fastresume") {
                has_fastresume = true;
                break;
            }
        }
        if !has_fastresume {
            return Err(CrustSeedError::new(format!(
                "[{}] Invalid torrentDir, if no torrents are in client set to null for now",
                self.label
            )));
        }
        Ok(())
    }
}

impl QBittorrent {
    /// Whether qBittorrent's local-auth bypass is on, which makes a successful
    /// connection test say nothing about the credentials.
    pub async fn auth_bypass_enabled(&self) -> bool {
        self.get_preferences()
            .await
            .map(|prefs| {
                prefs.bypass_auth_subnet_whitelist_enabled.unwrap_or(false)
                    || prefs.bypass_local_auth.unwrap_or(false)
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_key_of(url: &str) -> Option<String> {
        api_key_from(&extract_credentials_from_url(url, Some("/api/v2")).unwrap())
    }

    #[test]
    fn a_bare_qbt_prefixed_userinfo_is_read_as_an_api_key() {
        assert_eq!(
            api_key_of("http://qbt_A1b2C3d4E5f6G7h8I9j0K1l2M3n4@localhost:8080").as_deref(),
            Some("qbt_A1b2C3d4E5f6G7h8I9j0K1l2M3n4")
        );
    }

    #[test]
    fn username_password_pairs_still_use_the_login_flow() {
        assert_eq!(api_key_of("http://user:pass@localhost:8080"), None);
        assert_eq!(api_key_of("http://localhost:8080"), None);
        // A password is present, so this is an account named like a key.
        assert_eq!(api_key_of("http://qbt_notakey:pass@localhost:8080"), None);
        // No `qbt_` prefix: a username that was simply left without a password
        // must still fail at /auth/login rather than be sent as a bearer token.
        assert_eq!(api_key_of("http://user@localhost:8080"), None);
    }

    fn info(save_path: &str, content_path: &str) -> TorrentInfo {
        TorrentInfo {
            hash: "abc".into(),
            name: "Name".into(),
            save_path: save_path.into(),
            content_path: content_path.into(),
            ..Default::default()
        }
    }

    fn client() -> QBittorrent {
        QBittorrent::new(
            "http://user:pass@localhost:8080",
            "localhost:8080".into(),
            0,
            false,
        )
        .unwrap()
    }

    #[test]
    fn the_api_base_path_is_appended_to_the_url() {
        let qb = client();
        assert_eq!(qb.url.href, "http://localhost:8080/api/v2");
        assert_eq!(qb.url.username, "user");
        assert_eq!(qb.url.password, "pass");
        assert_eq!(qb.label, "qbittorrent@localhost:8080");
    }

    #[test]
    fn subfolder_layout_is_detected_from_content_path() {
        let _guard = crate::config::runtime::config_test_guard();
        crate::config::runtime::set_runtime_config(crate::config::RuntimeConfig {
            use_client_torrents: false,
            ..Default::default()
        });
        let qb = client();
        let files = vec![File {
            name: "Movie.mkv".into(),
            path: "Movie.mkv".into(),
            length: 100,
        }];

        // Content sits inside a subfolder of the save path => "Create Subfolder".
        let subfolder = info("/downloads", "/downloads/Movie/Movie.mkv");
        assert!(qb.is_subfolder_content_layout(&files, &subfolder));
        assert_eq!(qb.correct_save_path(&files, &subfolder), "/downloads/Movie");

        // Content sits directly in the save path => normal layout.
        let flat = info("/downloads", "/downloads/Movie.mkv");
        assert!(!qb.is_subfolder_content_layout(&files, &flat));
        assert_eq!(qb.correct_save_path(&files, &flat), "/downloads");
    }

    /// With useClientTorrents the API already reports the effective layout, so
    /// the inference must be disabled or it would double-count the subfolder.
    #[test]
    fn layout_inference_is_disabled_when_reading_torrents_from_the_api() {
        let _guard = crate::config::runtime::config_test_guard();
        crate::config::runtime::set_runtime_config(crate::config::RuntimeConfig {
            use_client_torrents: true,
            ..Default::default()
        });
        let qb = client();
        let files = vec![File {
            name: "Movie.mkv".into(),
            path: "Movie.mkv".into(),
            length: 100,
        }];
        let subfolder = info("/downloads", "/downloads/Movie/Movie.mkv");
        assert!(!qb.is_subfolder_content_layout(&files, &subfolder));
    }

    #[test]
    fn no_subfolder_layout_is_detected_for_multi_file_torrents() {
        let _guard = crate::config::runtime::config_test_guard();
        crate::config::runtime::set_runtime_config(crate::config::RuntimeConfig {
            use_client_torrents: false,
            ..Default::default()
        });
        let qb = client();
        let files = vec![
            File {
                name: "a.mkv".into(),
                path: "Pack/a.mkv".into(),
                length: 1,
            },
            File {
                name: "b.mkv".into(),
                path: "Pack/b.mkv".into(),
                length: 1,
            },
        ];
        assert!(qb.is_no_subfolder_content_layout(&files, &info("/downloads", "/downloads")));
        assert!(!qb.is_no_subfolder_content_layout(&files, &info("/downloads", "/downloads/Pack")));
    }

    #[test]
    fn windows_paths_use_backslash_dirname() {
        assert_eq!(
            dirname_for(r"C:\downloads\Movie\Movie.mkv"),
            r"C:\downloads\Movie"
        );
        assert_eq!(
            dirname_for("/downloads/Movie/Movie.mkv"),
            "/downloads/Movie"
        );
    }

    #[test]
    fn complete_states_match_qbittorrents_vocabulary() {
        for state in ["uploading", "stalledUP", "pausedUP", "stoppedUP"] {
            assert!(QBittorrent::is_complete(&TorrentInfo {
                state: state.into(),
                ..Default::default()
            }));
        }
        for state in ["downloading", "stalledDL", "checkingDL"] {
            assert!(!QBittorrent::is_complete(&TorrentInfo {
                state: state.into(),
                ..Default::default()
            }));
        }
    }

    #[test]
    fn hash_matching_covers_v1_and_v2() {
        let torrent = TorrentInfo {
            hash: "aaaa".into(),
            infohash_v1: Some("bbbb".into()),
            infohash_v2: Some("cccc".into()),
            ..Default::default()
        };
        assert!(matches_hash(&torrent, "AAAA"));
        assert!(matches_hash(&torrent, "bbbb"));
        assert!(matches_hash(&torrent, "cccc"));
        assert!(!matches_hash(&torrent, "dddd"));
    }

    #[test]
    fn tags_are_split_and_trimmed() {
        assert_eq!(split_tags(""), Vec::<String>::new());
        assert_eq!(split_tags("a, b ,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn duplicate_category_tags_are_derived_from_the_searchee() {
        let _guard = crate::config::runtime::config_test_guard();
        crate::config::runtime::set_runtime_config(crate::config::RuntimeConfig {
            duplicate_categories: true,
            link_category: Some("cross-seed-link".into()),
            ..Default::default()
        });
        let qb = client();
        assert_eq!(
            qb.tags_for_new_torrent(Some("movies"), Some("/links")),
            "cross-seed,movies.cross-seed"
        );
        // No destination dir means no linking, so no category duplication.
        assert_eq!(qb.tags_for_new_torrent(Some("movies"), None), "cross-seed");
        // The link category itself is never duplicated.
        assert_eq!(
            qb.tags_for_new_torrent(Some("cross-seed-link"), Some("/links")),
            "cross-seed"
        );
    }

    #[test]
    fn torrent_files_become_searchee_files() {
        let file = torrent_file_to_file(&TorrentFile {
            name: "Pack/Sub/a.mkv".into(),
            size: 42,
        });
        assert_eq!(file.name, "a.mkv");
        assert_eq!(file.path, "Pack/Sub/a.mkv");
        assert_eq!(file.length, 42);
    }
}
