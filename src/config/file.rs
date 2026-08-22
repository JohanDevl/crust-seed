//! The user-editable config file.
//!
//! Ported from the `FileConfig` half of `configuration.ts`.
//!
//! cross-seed shipped `config.js`, an ES module the daemon `import()`ed. Rust
//! cannot evaluate JavaScript, so crust-seed reads a declarative file with the
//! same option names from the same directory:
//!
//! * `config.toml` (preferred)
//! * `config.json` (accepted — a `config.js` that is just a literal object
//!   translates to this almost verbatim)
//!
//! Every legacy alias the original accepted is still honoured: `linkDir`
//! (singular), `notificationWebhookUrl` (singular), the four `*RpcUrl` /
//! `qbittorrentUrl` shorthands that expand into `torrentClients` entries, and
//! `matchMode: "risky"` as a synonym for `flexible`.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::duration::parse_ms_i64;
use super::{ConfigOverrides, WebhookEntry, app_dir};
use crate::constants::{Action, LinkType, MatchMode};
use crate::errors::CrustSeedError;

/// A duration option: either an `ms` string (`"2 weeks"`), a raw millisecond
/// number, or `null` meaning "disabled" (which the original mapped to `0`).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DurationValue {
    Number(f64),
    Text(String),
    Null,
}

impl DurationValue {
    fn to_ms(&self) -> Option<i64> {
        match self {
            DurationValue::Null => Some(0),
            DurationValue::Number(n) if n.is_finite() => Some(*n as i64),
            DurationValue::Number(_) => None,
            DurationValue::Text(s) => parse_ms_i64(s),
        }
    }
}

/// `seasonFromEpisodes` accepted `null`/`false` as "off" (0) as well as a ratio.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SeasonFromEpisodes {
    Number(f64),
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileConfig {
    pub action: Option<String>,
    /// Present in cross-seed's generated config; carried for compatibility and
    /// otherwise unused.
    pub pconfig_version: Option<i64>,
    pub delay: Option<f64>,
    pub include_single_episodes: Option<bool>,
    pub output_dir: Option<String>,
    pub inject_dir: Option<String>,
    pub ignore_titles: Option<bool>,
    pub include_non_videos: Option<bool>,
    pub season_from_episodes: Option<SeasonFromEpisodes>,
    pub fuzzy_size_threshold: Option<f64>,
    pub exclude_older: Option<DurationValue>,
    pub exclude_recent_search: Option<DurationValue>,
    pub use_client_torrents: Option<bool>,
    pub data_dirs: Option<Vec<String>>,
    pub match_mode: Option<String>,
    pub skip_recheck: Option<bool>,
    pub auto_resume_max_download: Option<f64>,
    pub ignore_non_relevant_files_to_resume: Option<bool>,
    /// Legacy singular form of `linkDirs`.
    pub link_dir: Option<String>,
    pub link_dirs: Option<Vec<String>>,
    pub link_type: Option<String>,
    pub flat_linking: Option<bool>,
    pub max_data_depth: Option<f64>,
    pub link_category: Option<String>,
    pub torrent_dir: Option<String>,
    pub torznab: Option<Vec<String>>,
    pub torrent_clients: Option<Vec<String>>,
    pub qbittorrent_url: Option<String>,
    pub rtorrent_rpc_url: Option<String>,
    pub transmission_rpc_url: Option<String>,
    pub deluge_rpc_url: Option<String>,
    pub duplicate_categories: Option<bool>,
    pub notification_webhook_urls: Option<Vec<WebhookEntry>>,
    /// Legacy singular form of `notificationWebhookUrls`.
    pub notification_webhook_url: Option<String>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub base_path: Option<String>,
    pub verbose: Option<bool>,
    pub search_cadence: Option<DurationValue>,
    pub rss_cadence: Option<DurationValue>,
    pub snatch_timeout: Option<DurationValue>,
    pub search_timeout: Option<DurationValue>,
    pub search_limit: Option<f64>,
    pub block_list: Option<Vec<String>>,
    pub api_key: Option<String>,
    pub sonarr: Option<Vec<String>>,
    pub radarr: Option<Vec<String>>,
}

pub const UNPARSABLE_CONFIG_MESSAGE: &str = "\
Your config file is improperly formatted. The location of the error is above, \
but you may have to look backwards to see the root cause.
Make sure that
  - strings (words, URLs, etc) are wrapped in \"quotation marks\"
  - any arrays (lists of things, even one thing) are wrapped in [square brackets]
  - every entry has a comma after it, including inside arrays";

/// Candidate config paths, most-preferred first.
pub fn file_config_paths() -> Vec<PathBuf> {
    let dir = app_dir();
    vec![dir.join("config.toml"), dir.join("config.json")]
}

/// Reads the first config file that exists. Returns `None` when the user has
/// none — a fresh install is configured entirely through the web UI.
pub fn get_file_config() -> Result<Option<(PathBuf, FileConfig)>, CrustSeedError> {
    for path in file_config_paths() {
        if !path.exists() {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| CrustSeedError::new(format!("Could not read {}: {e}", path.display())))?;

        let parsed: Result<FileConfig, String> =
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                toml::from_str(&contents).map_err(|e| e.to_string())
            } else {
                serde_json::from_str(&contents).map_err(|e| e.to_string())
            };

        return match parsed {
            Ok(config) => Ok(Some((path, config))),
            Err(message) => Err(CrustSeedError::new(format!(
                "{}\n{}\n{UNPARSABLE_CONFIG_MESSAGE}",
                path.display(),
                message
            ))),
        };
    }
    Ok(None)
}

fn insert<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: T) {
    if let Ok(value) = serde_json::to_value(value) {
        map.insert(key.to_string(), value);
    }
}

/// `transformFileConfig` — normalises the file's shape into sparse runtime
/// overrides, applying every legacy alias and coercion.
pub fn transform_file_config(file_config: &FileConfig) -> ConfigOverrides {
    let mut result = Map::new();

    if let Some(delay) = file_config.delay {
        insert(&mut result, "delay", delay as i64);
    }
    if let Some(torznab) = &file_config.torznab {
        insert(&mut result, "torznab", torznab);
    }
    if let Some(v) = file_config.use_client_torrents {
        insert(&mut result, "useClientTorrents", v);
    }
    if let Some(v) = &file_config.data_dirs {
        insert(&mut result, "dataDirs", v);
    }
    if let Some(mode) = file_config.match_mode.as_deref() {
        // "risky" was the pre-v6 name for flexible matching.
        let normalized = match mode {
            "strict" => Some(MatchMode::Strict),
            "flexible" | "risky" => Some(MatchMode::Flexible),
            "partial" => Some(MatchMode::Partial),
            _ => None,
        };
        if let Some(mode) = normalized {
            insert(&mut result, "matchMode", mode);
        }
    }
    if let Some(v) = file_config.skip_recheck {
        insert(&mut result, "skipRecheck", v);
    }
    if let Some(v) = file_config.auto_resume_max_download {
        insert(&mut result, "autoResumeMaxDownload", v as i64);
    }
    if let Some(v) = file_config.ignore_non_relevant_files_to_resume {
        insert(&mut result, "ignoreNonRelevantFilesToResume", v);
    }

    let link_dirs = resolve_link_dirs(file_config);
    if let Some(dirs) = &link_dirs {
        insert(&mut result, "linkDirs", dirs);
    }

    match file_config.link_type.as_deref() {
        Some("symlink") => insert(&mut result, "linkType", LinkType::Symlink),
        Some("hardlink") => insert(&mut result, "linkType", LinkType::Hardlink),
        Some("reflink") => insert(&mut result, "linkType", LinkType::Reflink),
        Some("reflinkOrCopy") => insert(&mut result, "linkType", LinkType::ReflinkOrCopy),
        Some(_) => {}
        // Historical quirk kept intact: configuring linkDirs without a linkType
        // selects symlink, not the global hardlink default.
        None => {
            if link_dirs.as_ref().is_some_and(|d| !d.is_empty()) {
                insert(&mut result, "linkType", LinkType::Symlink);
            }
        }
    }

    if let Some(v) = file_config.flat_linking {
        insert(&mut result, "flatLinking", v);
    }
    if let Some(v) = file_config.max_data_depth {
        insert(&mut result, "maxDataDepth", v as i64);
    }
    if let Some(v) = &file_config.link_category {
        insert(&mut result, "linkCategory", v);
    }
    if let Some(v) = &file_config.torrent_dir {
        insert(&mut result, "torrentDir", v);
    }
    if let Some(v) = &file_config.output_dir {
        insert(&mut result, "outputDir", v);
    }
    if let Some(v) = &file_config.inject_dir {
        insert(&mut result, "injectDir", v);
    }
    if let Some(v) = file_config.ignore_titles {
        insert(&mut result, "ignoreTitles", v);
    }
    if let Some(v) = file_config.include_single_episodes {
        insert(&mut result, "includeSingleEpisodes", v);
    }
    if let Some(v) = file_config.include_non_videos {
        insert(&mut result, "includeNonVideos", v);
    }
    if let Some(v) = file_config.verbose {
        insert(&mut result, "verbose", v);
    }

    if let Some(season_from_episodes) = &file_config.season_from_episodes {
        let normalized = match season_from_episodes {
            SeasonFromEpisodes::Null => Some(0.0),
            SeasonFromEpisodes::Bool(false) => Some(0.0),
            SeasonFromEpisodes::Bool(true) => None,
            SeasonFromEpisodes::Number(n) if n.is_finite() => Some(*n),
            SeasonFromEpisodes::Number(_) => None,
        };
        if let Some(n) = normalized {
            insert(&mut result, "seasonFromEpisodes", n);
        }
    }

    if let Some(v) = file_config.fuzzy_size_threshold {
        insert(&mut result, "fuzzySizeThreshold", v);
    }

    for (key, value) in [
        ("excludeOlder", &file_config.exclude_older),
        ("excludeRecentSearch", &file_config.exclude_recent_search),
        ("searchCadence", &file_config.search_cadence),
        ("rssCadence", &file_config.rss_cadence),
        ("snatchTimeout", &file_config.snatch_timeout),
        ("searchTimeout", &file_config.search_timeout),
    ] {
        if let Some(duration) = value
            && let Some(ms) = duration.to_ms()
        {
            insert(&mut result, key, ms);
        }
    }

    match file_config.action.as_deref() {
        Some("save") => insert(&mut result, "action", Action::Save),
        Some("inject") => insert(&mut result, "action", Action::Inject),
        _ => {}
    }

    if let Some(clients) = collect_torrent_clients(file_config) {
        insert(&mut result, "torrentClients", clients);
    }
    if let Some(v) = file_config.duplicate_categories {
        insert(&mut result, "duplicateCategories", v);
    }
    if let Some(webhooks) = collect_webhook_urls(file_config) {
        insert(&mut result, "notificationWebhookUrls", webhooks);
    }
    if let Some(v) = file_config.port {
        insert(&mut result, "port", v);
    }
    if let Some(v) = &file_config.host {
        insert(&mut result, "host", v);
    }
    if let Some(v) = &file_config.base_path {
        insert(&mut result, "basePath", v);
    }
    if let Some(v) = file_config.search_limit {
        insert(&mut result, "searchLimit", v as i64);
    }
    if let Some(v) = &file_config.block_list {
        insert(&mut result, "blockList", v);
    }
    if let Some(v) = &file_config.api_key {
        insert(&mut result, "apiKey", v);
    }
    if let Some(v) = &file_config.sonarr {
        insert(&mut result, "sonarr", v);
    }
    if let Some(v) = &file_config.radarr {
        insert(&mut result, "radarr", v);
    }

    result
}

fn resolve_link_dirs(file_config: &FileConfig) -> Option<Vec<String>> {
    if let Some(dirs) = &file_config.link_dirs
        && !dirs.is_empty()
    {
        return Some(dirs.clone());
    }
    file_config.link_dir.as_ref().map(|dir| vec![dir.clone()])
}

/// Expands the four legacy `*Url` shorthands into `"<kind>:<url>"` entries and
/// merges them with any explicit `torrentClients` list.
fn collect_torrent_clients(file_config: &FileConfig) -> Option<Vec<String>> {
    let mut clients: Vec<String> = Vec::new();
    let mut push = |value: String| {
        if !clients.contains(&value) {
            clients.push(value);
        }
    };

    if let Some(explicit) = &file_config.torrent_clients {
        for client in explicit {
            push(client.clone());
        }
    }
    for (prefix, url) in [
        ("qbittorrent", &file_config.qbittorrent_url),
        ("rtorrent", &file_config.rtorrent_rpc_url),
        ("transmission", &file_config.transmission_rpc_url),
        ("deluge", &file_config.deluge_rpc_url),
    ] {
        if let Some(url) = url {
            push(format!("{prefix}:{url}"));
        }
    }

    (!clients.is_empty()).then_some(clients)
}

/// Canonical dedup key for a webhook entry: a bare string and an object that
/// only carries `{ url }` describe the same target.
fn webhook_key(entry: &WebhookEntry) -> String {
    match entry {
        WebhookEntry::Url(url) => url.clone(),
        WebhookEntry::Object(obj) => {
            if obj.headers.is_none() && obj.payload.is_none() {
                obj.url.clone()
            } else {
                serde_json::to_string(obj).unwrap_or_else(|_| obj.url.clone())
            }
        }
    }
}

fn collect_webhook_urls(file_config: &FileConfig) -> Option<Vec<WebhookEntry>> {
    let mut seen: Vec<String> = Vec::new();
    let mut entries: Vec<WebhookEntry> = Vec::new();

    if let Some(list) = &file_config.notification_webhook_urls {
        for entry in list {
            let key = webhook_key(entry);
            if !seen.contains(&key) {
                seen.push(key);
                entries.push(entry.clone());
            }
        }
    }
    if let Some(url) = &file_config.notification_webhook_url
        && !seen.contains(url)
    {
        entries.push(WebhookEntry::Url(url.clone()));
    }

    (!entries.is_empty()).then_some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_toml(s: &str) -> FileConfig {
        toml::from_str(s).expect("valid config")
    }

    #[test]
    fn durations_accept_ms_strings_numbers_and_null() {
        let config = from_toml(
            r#"
            excludeOlder = "2 weeks"
            searchTimeout = 5000
            "#,
        );
        let overrides = transform_file_config(&config);
        assert_eq!(overrides["excludeOlder"], serde_json::json!(1_209_600_000));
        assert_eq!(overrides["searchTimeout"], serde_json::json!(5000));
    }

    #[test]
    fn legacy_singular_link_dir_is_promoted() {
        let config = from_toml(r#"linkDir = "/links""#);
        let overrides = transform_file_config(&config);
        assert_eq!(overrides["linkDirs"], serde_json::json!(["/links"]));
        // No explicit linkType + linkDirs present => symlink, per the original.
        assert_eq!(overrides["linkType"], serde_json::json!("symlink"));
    }

    #[test]
    fn explicit_link_type_wins() {
        let config = from_toml(
            r#"
            linkDirs = ["/links"]
            linkType = "hardlink"
            "#,
        );
        let overrides = transform_file_config(&config);
        assert_eq!(overrides["linkType"], serde_json::json!("hardlink"));
    }

    #[test]
    fn client_url_shorthands_expand() {
        let config = from_toml(
            r#"
            qbittorrentUrl = "http://localhost:8080"
            delugeRpcUrl = "http://:pw@localhost:8112/json"
            "#,
        );
        let overrides = transform_file_config(&config);
        assert_eq!(
            overrides["torrentClients"],
            serde_json::json!([
                "qbittorrent:http://localhost:8080",
                "deluge:http://:pw@localhost:8112/json"
            ])
        );
    }

    #[test]
    fn risky_match_mode_maps_to_flexible() {
        let config = from_toml(r#"matchMode = "risky""#);
        let overrides = transform_file_config(&config);
        assert_eq!(overrides["matchMode"], serde_json::json!("flexible"));
    }

    #[test]
    fn webhooks_deduplicate_across_both_spellings() {
        let config = from_toml(
            r#"
            notificationWebhookUrls = ["https://hook.example/a"]
            notificationWebhookUrl = "https://hook.example/a"
            "#,
        );
        let overrides = transform_file_config(&config);
        assert_eq!(
            overrides["notificationWebhookUrls"],
            serde_json::json!(["https://hook.example/a"])
        );
    }

    #[test]
    fn season_from_episodes_off_switches_are_zero() {
        for src in ["seasonFromEpisodes = false", "seasonFromEpisodes = 0.5"] {
            let overrides = transform_file_config(&from_toml(src));
            assert!(overrides.contains_key("seasonFromEpisodes"));
        }
        let overrides = transform_file_config(&from_toml("seasonFromEpisodes = false"));
        assert_eq!(overrides["seasonFromEpisodes"], serde_json::json!(0.0));
    }

    #[test]
    fn json_configs_parse_too() {
        let config: FileConfig =
            serde_json::from_str(r#"{"delay": 45, "torznab": ["https://x/api?apikey=k"]}"#)
                .unwrap();
        let overrides = transform_file_config(&config);
        assert_eq!(overrides["delay"], serde_json::json!(45));
    }
}
