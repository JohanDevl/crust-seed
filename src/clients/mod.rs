//! Torrent client integrations.
//!
//! Ported from `clients/TorrentClient.ts`. The four clients differ enormously
//! in protocol (qBittorrent: form-encoded HTTP; Transmission: JSON-RPC with a
//! session-id handshake; Deluge: JSON-RPC with a cookie; rTorrent: XML-RPC), so
//! the shared surface is the [`TorrentClient`] trait plus the decision helpers
//! in this module.

pub mod deluge;
pub mod qbittorrent;
pub mod rtorrent;
pub mod transmission;
pub mod xmlrpc;

use std::sync::{Arc, LazyLock, RwLock};

use async_trait::async_trait;

use crate::config::RuntimeConfig;
use crate::config::runtime::get_runtime_config;
use crate::constants::{
    Action, Decision, InjectionResult, MatchMode, RESUME_EXCLUDED_EXTS, RESUME_EXCLUDED_KEYWORDS,
    VIDEO_DISC_EXTENSIONS,
};
use crate::db::ClientSearcheeRow;
use crate::decide::get_partial_size_ratio;
use crate::errors::CrustSeedError;
use crate::logger::Label;
use crate::problems::{Problem, ProblemSeverity};
use crate::searchee::{File, Searchee, has_ext};
use crate::torrent::{Metafile, sanitize_tracker_url};
use crate::utils::human_readable_size;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientType {
    QBittorrent,
    RTorrent,
    Transmission,
    Deluge,
}

impl ClientType {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientType::QBittorrent => "qbittorrent",
            ClientType::RTorrent => "rtorrent",
            ClientType::Transmission => "transmission",
            ClientType::Deluge => "deluge",
        }
    }

    pub fn from_str_exact(s: &str) -> Option<ClientType> {
        match s {
            "qbittorrent" => Some(ClientType::QBittorrent),
            "rtorrent" => Some(ClientType::RTorrent),
            "transmission" => Some(ClientType::Transmission),
            "deluge" => Some(ClientType::Deluge),
            _ => None,
        }
    }

    pub fn label(self) -> Label {
        match self {
            ClientType::QBittorrent => Label::QBittorrent,
            ClientType::RTorrent => Label::RTorrent,
            ClientType::Transmission => Label::Transmission,
            ClientType::Deluge => Label::Deluge,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    pub url: String,
    pub tier: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TorrentMetadataInClient {
    pub info_hash: String,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub trackers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct ClientSearcheeResult {
    pub searchees: Vec<Searchee>,
    pub new_searchees: Vec<Searchee>,
}

/// Which searchees to (re)read from the client rather than the cache.
#[derive(Debug, Clone, Default)]
pub struct GetSearcheesOptions {
    pub new_searchees_only: bool,
    /// `None` uses the cache, `Some(vec![])` refreshes everything, and a
    /// non-empty list refreshes only those info hashes.
    pub refresh: Option<Vec<String>>,
    pub include_files: bool,
    pub include_trackers: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadDirError {
    NotFound,
    TorrentNotComplete,
    InvalidData,
    UnknownError,
}

#[derive(Debug, Clone, Default)]
pub struct InjectOptions {
    pub only_completed: bool,
    /// Set when the content was linked into a new location rather than being
    /// seeded from the searchee's own save path.
    pub destination_dir: Option<String>,
}

#[async_trait]
pub trait TorrentClient: Send + Sync {
    fn client_host(&self) -> &str;
    fn client_priority(&self) -> usize;
    fn client_type(&self) -> ClientType;
    fn readonly(&self) -> bool;
    fn label(&self) -> &str;

    async fn is_torrent_in_client(&self, info_hash: &str) -> Result<bool, String>;
    async fn is_torrent_complete(&self, info_hash: &str) -> Result<bool, DownloadDirError>;
    async fn is_torrent_checking(&self, info_hash: &str) -> Result<bool, DownloadDirError>;
    async fn get_all_torrents(&self) -> Result<Vec<TorrentMetadataInClient>, String>;
    async fn get_client_searchees(
        &self,
        options: GetSearcheesOptions,
    ) -> Result<ClientSearcheeResult, String>;
    async fn get_download_dir(
        &self,
        meta: &Searchee,
        only_completed: bool,
    ) -> Result<String, DownloadDirError>;
    async fn get_all_download_dirs(
        &self,
        metas: &[Searchee],
        only_completed: bool,
        v1_hash_only: bool,
    ) -> Result<std::collections::HashMap<String, String>, String>;
    async fn resume_injection(&self, meta: &Metafile, decision: Decision, check_once: bool);
    async fn inject(
        &self,
        new_torrent: &Metafile,
        searchee: &Searchee,
        decision: Decision,
        options: InjectOptions,
    ) -> InjectionResult;
    async fn recheck_torrent(&self, info_hash: &str) -> Result<(), String>;
    async fn validate_config(&self) -> Result<(), CrustSeedError>;
}

// ─── Registry ───────────────────────────────────────────────────────────────

type ClientList = Vec<Arc<dyn TorrentClient>>;

static ACTIVE_CLIENTS: LazyLock<RwLock<ClientList>> = LazyLock::new(|| RwLock::new(Vec::new()));

pub fn get_clients() -> ClientList {
    ACTIVE_CLIENTS.read().expect("clients lock").clone()
}

pub fn set_clients(clients: ClientList) {
    *ACTIVE_CLIENTS.write().expect("clients lock") = clients;
}

/// Priority of the client that owns `client_host`, used to pick which client a
/// virtual searchee belongs to. Unknown hosts sort last.
pub fn by_client_host_priority(client_host: Option<&str>) -> usize {
    let clients = get_clients();
    clients
        .iter()
        .find(|c| Some(c.client_host()) == client_host)
        .map(|c| c.client_priority())
        .unwrap_or(clients.len())
}

/// A `torrentClients` entry: `"<type>:[readonly:]<url>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientEntry {
    pub client_type: ClientType,
    pub readonly: bool,
    pub url: String,
}

pub fn parse_client_entry(entry: &str) -> Option<ClientEntry> {
    let (type_part, rest) = entry.split_once(':')?;
    let client_type = ClientType::from_str_exact(type_part)?;
    let (readonly, url) = match rest.strip_prefix("readonly:") {
        Some(url) => (true, url),
        None => (false, rest),
    };
    Some(ClientEntry {
        client_type,
        readonly,
        url: url.to_string(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientUniqueness {
    pub unique_hosts: bool,
    pub unique_with_pathname: bool,
}

/// Two clients behind the same host need their pathnames to tell them apart,
/// because `clientHost` is the primary key of the `client_searchee` cache.
pub fn clients_are_unique(torrent_clients: &[String]) -> ClientUniqueness {
    let hosts: Vec<String> = torrent_clients
        .iter()
        .filter_map(|entry| parse_client_entry(entry))
        .filter_map(|entry| url::Url::parse(&entry.url).ok())
        .map(|url| match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or("")),
            None => url.host_str().unwrap_or("").to_string(),
        })
        .collect();
    let unique_hosts = crate::utils::dedupe_preserving_order(&hosts).len() == torrent_clients.len();
    if unique_hosts {
        return ClientUniqueness {
            unique_hosts: true,
            unique_with_pathname: true,
        };
    }

    let with_paths: Vec<String> = torrent_clients
        .iter()
        .filter_map(|entry| parse_client_entry(entry))
        .filter_map(|entry| url::Url::parse(&entry.url).ok())
        .map(|url| {
            let host = match url.port() {
                Some(port) => format!("{}:{port}", url.host_str().unwrap_or("")),
                None => url.host_str().unwrap_or("").to_string(),
            };
            let path = url.path().trim_end_matches('/');
            format!("{host}{path}")
        })
        .collect();
    ClientUniqueness {
        unique_hosts: false,
        unique_with_pathname: crate::utils::dedupe_preserving_order(&with_paths).len()
            == torrent_clients.len(),
    }
}

/// The `clientHost` a given entry is keyed by: bare host when hosts are
/// unique, host + path otherwise.
pub fn client_host_for(entry: &ClientEntry, unique_hosts: bool) -> String {
    let Ok(url) = url::Url::parse(&entry.url) else {
        return entry.url.clone();
    };
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or("")),
        None => url.host_str().unwrap_or("").to_string(),
    };
    if unique_hosts {
        host
    } else {
        format!("{host}{}", url.path().trim_end_matches('/'))
    }
}

/// Builds the client list from `torrentClients` and installs it.
pub fn instantiate_download_clients(config: &RuntimeConfig) {
    let uniqueness = clients_are_unique(&config.torrent_clients);
    let mut clients: ClientList = Vec::new();

    for (priority, raw_entry) in config.torrent_clients.iter().enumerate() {
        let Some(entry) = parse_client_entry(raw_entry) else {
            tracing::error!(
                label = Label::Config.as_str(),
                "Could not parse torrent client entry: {raw_entry}"
            );
            continue;
        };
        let client_host = client_host_for(&entry, uniqueness.unique_hosts);
        crate::logger::register_secret_url(&entry.url);

        let client: Option<Arc<dyn TorrentClient>> = match entry.client_type {
            ClientType::QBittorrent => {
                qbittorrent::QBittorrent::new(&entry.url, client_host, priority, entry.readonly)
                    .map(|c| Arc::new(c) as Arc<dyn TorrentClient>)
            }
            ClientType::Transmission => {
                transmission::Transmission::new(&entry.url, client_host, priority, entry.readonly)
                    .map(|c| Arc::new(c) as Arc<dyn TorrentClient>)
            }
            ClientType::Deluge => {
                deluge::Deluge::new(&entry.url, client_host, priority, entry.readonly)
                    .map(|c| Arc::new(c) as Arc<dyn TorrentClient>)
            }
            ClientType::RTorrent => {
                rtorrent::RTorrent::new(&entry.url, client_host, priority, entry.readonly)
                    .map(|c| Arc::new(c) as Arc<dyn TorrentClient>)
            }
        }
        .map_err(|e| {
            tracing::error!(
                label = entry.client_type.label().as_str(),
                "Could not initialise client: {e}"
            );
            e
        })
        .ok();

        if let Some(client) = client {
            clients.push(client);
        }
    }

    set_clients(clients);
}

pub fn reload_download_clients() {
    instantiate_download_clients(&get_runtime_config());
}

// ─── Shared decisions ───────────────────────────────────────────────────────

/// Whether a cached `client_searchee` row is stale.
pub fn client_searchee_modified(
    db_torrent: Option<&ClientSearcheeRow>,
    name: &str,
    save_path: &str,
    category: Option<&str>,
    tags: &[String],
) -> bool {
    let Some(db_torrent) = db_torrent else {
        return true;
    };
    if db_torrent.name.as_deref() != Some(name) {
        return true;
    }
    if db_torrent.save_path.as_deref() != Some(save_path) {
        return true;
    }
    if db_torrent.category.as_deref() != category {
        return true;
    }
    let stored_tags: Vec<String> = db_torrent
        .tags
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default();
    stored_tags != tags
}

pub fn organize_trackers(trackers: &[Tracker]) -> Vec<String> {
    trackers
        .iter()
        .filter_map(|t| sanitize_tracker_url(&t.url))
        .collect()
}

/// Polls a client until a torrent finishes, backing off exponentially.
pub async fn wait_for_torrent_to_complete(
    client: &dyn TorrentClient,
    info_hash: &str,
    retries: u32,
) -> bool {
    for attempt in 0..=retries {
        if client.is_torrent_complete(info_hash).await.unwrap_or(false) {
            return true;
        }
        if attempt < retries {
            crate::utils::wait(1000u64 << attempt).await;
        }
    }
    false
}

/// Whether an injected torrent needs a hash check.
///
/// `skipRecheck` is a performance option; it is deliberately overridden for
/// partial matches (data really is missing) and for disc-based releases, whose
/// large files make a wrong assumption expensive to discover later.
pub fn should_recheck(meta: &Metafile, decision: Decision, config: &RuntimeConfig) -> bool {
    if !config.skip_recheck {
        return true;
    }
    if decision == Decision::MatchPartial {
        return true;
    }
    has_ext(&meta.files, VIDEO_DISC_EXTENSIONS)
}

/// How many bytes a partial match may still need before it is auto-resumed.
pub fn get_max_remaining_bytes(
    meta: &Metafile,
    decision: Decision,
    config: &RuntimeConfig,
    log: Option<(&str, &str)>,
) -> i64 {
    if decision != Decision::MatchPartial || config.match_mode != MatchMode::Partial {
        return 0;
    }
    if has_ext(&meta.files, VIDEO_DISC_EXTENSIONS) {
        if let Some((torrent_log, label)) = log {
            tracing::warn!(
                label = label,
                "autoResumeMaxDownload will not resume {torrent_log}: VIDEO_DISC_EXTENSIONS"
            );
        }
        return 0;
    }
    config.auto_resume_max_download
}

pub const RESUME_SLEEP_MS: u64 = 15_000;
pub const RESUME_ERR_SLEEP_MS: u64 = 300_000;

pub fn get_resume_stop_time() -> i64 {
    crate::utils::now_ms() + 60 * 60 * 1000
}

/// Whether the bytes still missing from a partial match are all *irrelevant* —
/// samples, trailers, subtitles — in which case seeding can start anyway.
///
/// The 200 MiB ceiling and the two-piece slack are the original's: a piece can
/// straddle a relevant and an irrelevant file, so the remaining size is allowed
/// to exceed the irrelevant total by two pieces.
pub fn should_resume_from_non_relevant_files(
    meta: &Metafile,
    remaining_size: i64,
    decision: Decision,
    config: &RuntimeConfig,
    log: Option<(&str, &str)>,
) -> bool {
    let warn = |message: String| {
        if let Some((_, label)) = log {
            tracing::warn!(label = label, "{message}");
        }
    };
    let torrent_log = log.map(|(t, _)| t).unwrap_or("");

    if !config.ignore_non_relevant_files_to_resume {
        return false;
    }
    if decision != Decision::MatchPartial || config.match_mode != MatchMode::Partial {
        return false;
    }
    if has_ext(&meta.files, VIDEO_DISC_EXTENSIONS) {
        warn(format!(
            "ignoreNonRelevantFilesToResume will not resume {torrent_log}: VIDEO_DISC_EXTENSIONS"
        ));
        return false;
    }
    if remaining_size > 209_715_200 {
        warn(format!(
            "ignoreNonRelevantFilesToResume will not resume {torrent_log}: 200 MiB limit"
        ));
        return false;
    }

    let irrelevant_size: i64 = meta
        .files
        .iter()
        .filter(|file| is_non_relevant_file(file))
        .map(|file| file.length)
        .sum();

    if irrelevant_size == 0 {
        warn(format!(
            "ignoreNonRelevantFilesToResume will not resume {torrent_log}: all files are relevant"
        ));
        return false;
    }
    if remaining_size <= irrelevant_size + meta.piece_length * 2 {
        return true;
    }
    warn(format!(
        "ignoreNonRelevantFilesToResume will not resume {torrent_log}: remainingSize {} > {} irrelevantSize",
        human_readable_size(remaining_size, true),
        human_readable_size(irrelevant_size, true)
    ));
    false
}

fn is_non_relevant_file(file: &File) -> bool {
    let path = file.path.to_lowercase();
    RESUME_EXCLUDED_KEYWORDS
        .iter()
        .any(|keyword| path.contains(keyword))
        || RESUME_EXCLUDED_EXTS.iter().any(|ext| path.ends_with(ext))
}

/// Whether an injected torrent should start paused.
pub fn estimate_paused_status(
    meta: &Metafile,
    searchee: &Searchee,
    decision: Decision,
    config: &RuntimeConfig,
) -> bool {
    let remaining =
        ((1.0 - get_partial_size_ratio(meta, searchee)) * meta.length as f64).round() as i64;
    if remaining <= get_max_remaining_bytes(meta, decision, config, None) {
        return false;
    }
    !should_resume_from_non_relevant_files(meta, remaining, decision, config, None)
}

/// `updateSearcheeClientDB` — refreshes the `client_searchee` cache for one
/// client and drops rows for torrents that have left it.
///
/// Removing a torrent also removes its ensemble rows: a virtual season built
/// from episodes that are no longer in the client would point at missing files.
pub async fn persist_client_searchees(
    client_host: &str,
    new_searchees: &[Searchee],
    info_hashes: &std::collections::HashSet<String>,
) -> sqlx::Result<()> {
    let pool = crate::db::db();

    let existing: Vec<String> =
        sqlx::query_scalar("SELECT info_hash FROM client_searchee WHERE client_host = ?")
            .bind(client_host)
            .fetch_all(pool)
            .await?;
    let removed: Vec<String> = existing
        .into_iter()
        .filter(|info_hash| !info_hashes.contains(info_hash))
        .collect();

    for batch in removed.chunks(crate::db::BATCH_SIZE) {
        let placeholders = crate::db::placeholders(batch.len());
        for table in ["client_searchee", "ensemble"] {
            let sql = format!(
                "DELETE FROM {table} WHERE client_host = ? AND info_hash IN ({placeholders})"
            );
            let mut query = sqlx::query(&sql).bind(client_host);
            for info_hash in batch {
                query = query.bind(info_hash);
            }
            query.execute(pool).await?;
        }
    }

    for searchee in new_searchees {
        let Some(info_hash) = &searchee.info_hash else {
            continue;
        };
        sqlx::query(
            r#"
            INSERT INTO client_searchee
                (client_host, info_hash, name, title, files, length, save_path, category, tags, trackers)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (client_host, info_hash) DO UPDATE SET
                name = excluded.name,
                title = excluded.title,
                files = excluded.files,
                length = excluded.length,
                save_path = excluded.save_path,
                category = excluded.category,
                tags = excluded.tags,
                trackers = excluded.trackers
            "#,
        )
        .bind(client_host)
        .bind(info_hash)
        .bind(&searchee.name)
        .bind(&searchee.title)
        .bind(serde_json::to_string(&searchee.files).unwrap_or_default())
        .bind(searchee.length)
        .bind(&searchee.save_path)
        .bind(&searchee.category)
        .bind(
            searchee
                .tags
                .as_ref()
                .map(|tags| serde_json::to_string(tags).unwrap_or_default()),
        )
        .bind(serde_json::to_string(searchee.trackers.as_deref().unwrap_or(&[])).unwrap_or_default())
        .execute(pool)
        .await?;
    }

    Ok(())
}

// ─── Health problems ────────────────────────────────────────────────────────

pub async fn collect_client_problems() -> Result<Vec<Problem>, String> {
    let config = get_runtime_config();
    let clients = get_clients();
    let mut problems = Vec::new();

    if config.action == Action::Inject && config.torrent_clients.is_empty() {
        problems.push(Problem::new(
            "client:inject-without-clients",
            ProblemSeverity::Error,
            "Injection requires at least one configured torrent client.",
            "Add a torrent client in Settings → Torrent Clients or switch the action away from Inject.",
        ));
        return Ok(problems);
    }

    if !config.torrent_clients.is_empty() && clients.is_empty() {
        problems.push(Problem::new(
            "client:initialization-failed",
            ProblemSeverity::Error,
            "Torrent clients failed to initialize.",
            "Check the configuration for typos or authentication issues, then restart crust-seed.",
        ));
        return Ok(problems);
    }

    if config.action == Action::Inject && !clients.iter().any(|c| !c.readonly()) {
        problems.push(Problem::new(
            "client:inject-readonly",
            ProblemSeverity::Error,
            "Injection is not possible when all clients are read-only.",
            "Mark at least one client as writable to allow injection.",
        ));
    }

    let validations =
        futures::future::join_all(clients.iter().map(|client| client.validate_config())).await;
    for (client, result) in clients.iter().zip(validations) {
        if let Err(error) = result {
            problems.push(
                Problem::new(
                    format!("client:validation:{}", client.label()),
                    ProblemSeverity::Error,
                    format!("{} failed validation.", client.label()),
                    error.to_string(),
                )
                .with_metadata(
                    serde_json::json!({
                        "clientType": client.client_type().as_str(),
                        "clientHost": client.client_host(),
                        "readonly": client.readonly(),
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ),
            );
        }
    }

    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::config::runtime::set_runtime_config;
    use crate::torrent::metafile::fixtures::multi_file_torrent;

    #[test]
    fn client_entries_parse_type_readonly_and_url() {
        let entry = parse_client_entry("qbittorrent:http://localhost:8080").unwrap();
        assert_eq!(entry.client_type, ClientType::QBittorrent);
        assert!(!entry.readonly);
        assert_eq!(entry.url, "http://localhost:8080");

        let readonly =
            parse_client_entry("deluge:readonly:http://:pw@localhost:8112/json").unwrap();
        assert_eq!(readonly.client_type, ClientType::Deluge);
        assert!(readonly.readonly);
        assert_eq!(readonly.url, "http://:pw@localhost:8112/json");

        assert!(parse_client_entry("notaclient:http://x").is_none());
    }

    /// clientHost keys the client_searchee cache, so two clients behind one
    /// host must be disambiguated by path.
    #[test]
    fn client_hosts_fall_back_to_paths_when_hosts_collide() {
        let entries = vec![
            "qbittorrent:http://localhost:8080".to_string(),
            "qbittorrent:http://localhost:8081".to_string(),
        ];
        assert!(clients_are_unique(&entries).unique_hosts);

        let same_host = vec![
            "qbittorrent:http://gateway/qbit-a".to_string(),
            "qbittorrent:http://gateway/qbit-b".to_string(),
        ];
        let uniqueness = clients_are_unique(&same_host);
        assert!(!uniqueness.unique_hosts);
        assert!(uniqueness.unique_with_pathname);

        let entry = parse_client_entry(&same_host[0]).unwrap();
        assert_eq!(client_host_for(&entry, false), "gateway/qbit-a");
        assert_eq!(client_host_for(&entry, true), "gateway");
    }

    #[test]
    fn duplicate_clients_are_detected() {
        let dupes = vec![
            "qbittorrent:http://gateway/qbit".to_string(),
            "qbittorrent:http://gateway/qbit/".to_string(),
        ];
        let uniqueness = clients_are_unique(&dupes);
        assert!(!uniqueness.unique_hosts);
        assert!(!uniqueness.unique_with_pathname);
    }

    fn meta(files: &[(&[&str], i64)]) -> Metafile {
        Metafile::decode(&multi_file_torrent("Pack", files)).unwrap()
    }

    #[test]
    fn recheck_is_forced_for_partial_matches_and_discs() {
        let mut config = default_runtime_config();
        config.skip_recheck = true;
        let normal = meta(&[(&["a.mkv"], 100)]);
        assert!(!should_recheck(&normal, Decision::Match, &config));
        assert!(should_recheck(&normal, Decision::MatchPartial, &config));

        let disc = meta(&[(&["BDMV", "STREAM", "00000.m2ts"], 100)]);
        assert!(should_recheck(&disc, Decision::Match, &config));

        config.skip_recheck = false;
        assert!(should_recheck(&normal, Decision::Match, &config));
    }

    #[test]
    fn max_remaining_bytes_only_applies_to_partial_mode_partial_matches() {
        let mut config = default_runtime_config();
        config.match_mode = MatchMode::Partial;
        config.auto_resume_max_download = 1000;
        let m = meta(&[(&["a.mkv"], 100)]);

        assert_eq!(
            get_max_remaining_bytes(&m, Decision::MatchPartial, &config, None),
            1000
        );
        assert_eq!(
            get_max_remaining_bytes(&m, Decision::Match, &config, None),
            0
        );

        config.match_mode = MatchMode::Flexible;
        assert_eq!(
            get_max_remaining_bytes(&m, Decision::MatchPartial, &config, None),
            0
        );
    }

    #[test]
    fn irrelevant_missing_files_allow_resuming() {
        let mut config = default_runtime_config();
        config.match_mode = MatchMode::Partial;
        config.ignore_non_relevant_files_to_resume = true;

        let m = meta(&[
            (&["Show.S01E01.mkv"], 1_000_000),
            (&["Sample", "sample.mkv"], 5_000),
            (&["Show.S01E01.srt"], 1_000),
        ]);
        // Missing exactly the sample + subtitle.
        assert!(should_resume_from_non_relevant_files(
            &m,
            6_000,
            Decision::MatchPartial,
            &config,
            None
        ));
        // Missing far more than that.
        assert!(!should_resume_from_non_relevant_files(
            &m,
            900_000,
            Decision::MatchPartial,
            &config,
            None
        ));
    }

    #[test]
    fn resuming_from_irrelevant_files_is_off_by_default() {
        let mut config = default_runtime_config();
        config.match_mode = MatchMode::Partial;
        let m = meta(&[(&["a.mkv"], 100), (&["sample.mkv"], 10)]);
        assert!(!should_resume_from_non_relevant_files(
            &m,
            10,
            Decision::MatchPartial,
            &config,
            None
        ));
    }

    #[test]
    fn stale_cache_rows_are_detected_by_name_path_category_and_tags() {
        let row = ClientSearcheeRow {
            client_host: "host".into(),
            info_hash: "abc".into(),
            name: Some("Name".into()),
            title: Some("Name".into()),
            files: None,
            length: Some(1),
            save_path: Some("/downloads".into()),
            category: Some("movies".into()),
            tags: Some(r#"["a"]"#.into()),
            trackers: None,
        };
        assert!(!client_searchee_modified(
            Some(&row),
            "Name",
            "/downloads",
            Some("movies"),
            &["a".to_string()]
        ));
        assert!(client_searchee_modified(
            Some(&row),
            "Renamed",
            "/downloads",
            Some("movies"),
            &["a".to_string()]
        ));
        assert!(client_searchee_modified(
            Some(&row),
            "Name",
            "/other",
            Some("movies"),
            &["a".to_string()]
        ));
        assert!(client_searchee_modified(
            Some(&row),
            "Name",
            "/downloads",
            None,
            &["a".to_string()]
        ));
        assert!(client_searchee_modified(
            Some(&row),
            "Name",
            "/downloads",
            Some("movies"),
            &[]
        ));
        assert!(client_searchee_modified(
            None,
            "Name",
            "/downloads",
            None,
            &[]
        ));
    }

    #[test]
    fn tracker_urls_are_reduced_to_hosts() {
        let trackers = vec![
            Tracker {
                url: "https://tracker.example/announce/key".into(),
                tier: 0,
            },
            Tracker {
                url: "** [DHT] **".into(),
                tier: 1,
            },
        ];
        assert_eq!(organize_trackers(&trackers), vec!["tracker.example"]);
    }

    #[tokio::test]
    async fn inject_without_a_client_is_a_health_error() {
        let _guard = crate::config::runtime::config_test_guard_async().await;
        let mut config = default_runtime_config();
        config.action = Action::Inject;
        config.torrent_clients.clear();
        set_runtime_config(config);
        set_clients(Vec::new());

        let problems = collect_client_problems().await.unwrap();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].id, "client:inject-without-clients");
    }
}
