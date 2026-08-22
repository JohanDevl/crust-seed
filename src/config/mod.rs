//! Runtime configuration: the resolved option set the whole app reads from.
//!
//! Ported from `configuration.ts`, `runtimeConfig.ts`, `dbConfig.ts` and
//! `shared/configSchema.ts`.
//!
//! ## Layering
//!
//! Effective config = **defaults** ← **config file** ← **database overrides**
//! ← **CLI flags**. That is the original's order, with the database (edited
//! through the web UI) winning over the file.
//!
//! ## The one deliberate divergence: the config file format
//!
//! cross-seed's `config.js` is *executable JavaScript* that the daemon
//! `import()`s. A Rust binary cannot evaluate it. crust-seed reads a
//! declarative `config.toml` (or `config.json`) from the same directory with
//! the same option names and the same `ms`-style duration strings, so
//! translating an existing config is mechanical. See `config.example.toml`.

pub mod db_config;
pub mod duration;
pub mod file;
pub mod runtime;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::constants::{
    Action, LOGS_FOLDER, LinkType, MatchMode, PROGRAM_NAME, TORRENT_CACHE_FOLDER,
};
use crate::errors::CrustSeedError;

/// A notification target: either a bare URL or a URL plus custom headers and
/// an extra JSON payload merged into the body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WebhookEntry {
    Url(String),
    Object(WebhookObject),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookObject {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
}

impl WebhookEntry {
    pub fn url(&self) -> &str {
        match self {
            WebhookEntry::Url(u) => u,
            WebhookEntry::Object(o) => &o.url,
        }
    }
}

/// The fully-resolved option set.
///
/// Field names are the camelCase JSON keys the web UI's zod schema expects —
/// this struct is serialised straight into the `settings.get` response, so the
/// names are part of the API contract. `None` fields are *omitted* rather than
/// serialised as `null`, because the UI's schema uses `.optional()`, which
/// accepts `undefined` but rejects `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    /// Seconds to wait between searches (30–3600).
    pub delay: i64,
    pub torznab: Vec<String>,
    pub use_client_torrents: bool,
    pub data_dirs: Vec<String>,
    pub match_mode: MatchMode,
    pub skip_recheck: bool,
    pub auto_resume_max_download: i64,
    pub ignore_non_relevant_files_to_resume: bool,
    pub link_dirs: Vec<String>,
    pub link_type: LinkType,
    pub flat_linking: bool,
    pub max_data_depth: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub torrent_dir: Option<String>,
    pub output_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_titles: Option<bool>,
    pub include_single_episodes: bool,
    pub verbose: bool,
    pub include_non_videos: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub season_from_episodes: Option<f64>,
    pub fuzzy_size_threshold: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_older: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_recent_search: Option<i64>,
    pub action: Action,
    pub torrent_clients: Vec<String>,
    pub duplicate_categories: bool,
    pub notification_webhook_urls: Vec<WebhookEntry>,
    pub torrents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_cadence: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rss_cadence: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snatch_timeout: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_timeout: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_limit: Option<i64>,
    pub block_list: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub sonarr: Vec<String>,
    pub radarr: Vec<String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        default_runtime_config()
    }
}

impl RuntimeConfig {
    /// `seasonFromEpisodes` as an *enabled* ratio.
    ///
    /// The original stores `0` for "disabled" — `seasonFromEpisodes: null` and
    /// `false` both normalise to `0`, and every JS call site then guards with
    /// `if (!seasonFromEpisodes)`. In Rust `Some(0.0)` is not falsy, so reading
    /// the field directly turns "disabled" into "enabled with a 0%
    /// availability threshold", which would synthesise a virtual season out of
    /// any three episodes. Every consumer must go through this.
    pub fn season_from_episodes_ratio(&self) -> Option<f64> {
        self.season_from_episodes.filter(|ratio| *ratio > 0.0)
    }
}

/// `getDefaultRuntimeConfig()`.
///
/// Note this is *not* `shared/constants.ts`'s `defaultConfig`, which the web UI
/// uses to seed empty forms; the two disagree (e.g. `matchMode`) and the
/// backend has always used this one.
pub fn default_runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        delay: 30,
        torznab: Vec::new(),
        use_client_torrents: true,
        data_dirs: Vec::new(),
        match_mode: MatchMode::Flexible,
        skip_recheck: true,
        auto_resume_max_download: 52_428_800,
        ignore_non_relevant_files_to_resume: false,
        link_dirs: Vec::new(),
        link_type: LinkType::Hardlink,
        flat_linking: false,
        max_data_depth: 2,
        link_category: Some("cross-seed-link".to_string()),
        torrent_dir: None,
        output_dir: app_dir().join("cross-seeds").to_string_lossy().into_owned(),
        inject_dir: None,
        ignore_titles: Some(false),
        include_single_episodes: false,
        verbose: false,
        include_non_videos: false,
        season_from_episodes: Some(1.0),
        fuzzy_size_threshold: 0.02,
        exclude_older: Some(duration::parse_ms_i64("2 weeks").unwrap()),
        exclude_recent_search: Some(duration::parse_ms_i64("3 days").unwrap()),
        action: Action::Inject,
        torrent_clients: Vec::new(),
        duplicate_categories: false,
        notification_webhook_urls: Vec::new(),
        torrents: Vec::new(),
        port: Some(2468),
        host: Some("0.0.0.0".to_string()),
        base_path: Some(String::new()),
        search_cadence: Some(duration::parse_ms_i64("1 day").unwrap()),
        rss_cadence: Some(duration::parse_ms_i64("30 minutes").unwrap()),
        snatch_timeout: Some(duration::parse_ms_i64("30 seconds").unwrap()),
        search_timeout: Some(duration::parse_ms_i64("2 minutes").unwrap()),
        search_limit: Some(400),
        block_list: Vec::new(),
        api_key: None,
        sonarr: Vec::new(),
        radarr: Vec::new(),
    }
}

// ─── Overrides ──────────────────────────────────────────────────────────────

/// A sparse config: only the keys that differ from the defaults.
///
/// The original modelled this as `Partial<RuntimeConfig>` and stored it as JSON
/// in `settings.settings_json`; keeping it as a raw JSON object here preserves
/// the exact on-disk representation, including which keys are present.
pub type ConfigOverrides = Map<String, Value>;

fn to_object(config: &RuntimeConfig) -> Map<String, Value> {
    match serde_json::to_value(config) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// `stripDefaults` — keeps only the entries that differ from the defaults.
pub fn strip_defaults(config: &RuntimeConfig) -> ConfigOverrides {
    let defaults = to_object(&default_runtime_config());
    to_object(config)
        .into_iter()
        .filter(|(key, value)| defaults.get(key) != Some(value))
        .collect()
}

/// `{ ...getDefaultRuntimeConfig(), ...overrides }` followed by validation.
pub fn merge_overrides(overrides: &ConfigOverrides) -> Result<RuntimeConfig, CrustSeedError> {
    let mut merged = to_object(&default_runtime_config());
    for (key, value) in overrides {
        if value.is_null() {
            // A `null` override means "unset this optional field"; dropping the
            // key makes serde see `None`, matching JS `undefined`.
            merged.remove(key);
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    parse_runtime_config(Value::Object(merged))
}

/// `parseRuntimeConfig` — deserialise plus the range checks zod enforced.
pub fn parse_runtime_config(value: Value) -> Result<RuntimeConfig, CrustSeedError> {
    let config: RuntimeConfig = serde_json::from_value(value)
        .map_err(|e| CrustSeedError::new(format!("Invalid configuration: {e}")))?;
    validate_ranges(&config)?;
    Ok(config)
}

/// `parseRuntimeConfigOverrides` — validates a sparse object by merging it onto
/// the defaults first, then returns the sparse form unchanged.
pub fn parse_runtime_config_overrides(value: Value) -> Result<ConfigOverrides, CrustSeedError> {
    let Value::Object(map) = value else {
        return Err(CrustSeedError::new(
            "Invalid configuration: expected an object",
        ));
    };
    let known: Vec<String> = to_object(&default_runtime_config())
        .keys()
        .cloned()
        .collect();
    let filtered: ConfigOverrides = map
        .into_iter()
        .filter(|(key, _)| known.contains(key))
        .collect();
    merge_overrides(&filtered)?;
    Ok(filtered)
}

/// The numeric bounds the zod schema declared. Kept separate from
/// deserialisation so the error messages can name the option.
fn validate_ranges(c: &RuntimeConfig) -> Result<(), CrustSeedError> {
    let bound = |ok: bool, msg: &str| -> Result<(), CrustSeedError> {
        if ok {
            Ok(())
        } else {
            Err(CrustSeedError::new(msg.to_string()))
        }
    };

    bound(
        (30..=3600).contains(&c.delay),
        "delay must be between 30 and 3600 seconds",
    )?;
    bound(
        (0..=52_428_800).contains(&c.auto_resume_max_download),
        "autoResumeMaxDownload must be an integer of bytes between 0 and 52428800 (50 MiB).",
    )?;
    bound(c.max_data_depth >= 1, "maxDataDepth must be at least 1")?;
    bound(
        c.fuzzy_size_threshold > 0.0 && c.fuzzy_size_threshold <= 1.0,
        "fuzzySizeThreshold and seasonFromEpisodes must be between 0 and 1.",
    )?;
    if let Some(season_from_episodes) = c.season_from_episodes {
        bound(
            (0.0..=1.0).contains(&season_from_episodes),
            "fuzzySizeThreshold and seasonFromEpisodes must be between 0 and 1.",
        )?;
    }
    if let Some(search_limit) = c.search_limit {
        bound(search_limit >= 0, "searchLimit must be at least 0")?;
    }
    if let Some(api_key) = &c.api_key {
        bound(
            api_key.chars().count() >= 24,
            "API key must be at least 24 characters",
        )?;
    }
    Ok(())
}

// ─── Application directory ──────────────────────────────────────────────────

/// Absolute path to the config directory: `$CONFIG_DIR`, else
/// `%LOCALAPPDATA%\crust-seed` on Windows, else `~/.crust-seed`.
///
/// Created on first use — the database opens during startup and would otherwise
/// fail on a fresh install.
pub fn app_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CONFIG_DIR")
        && !dir.is_empty()
    {
        return ensure_dir(PathBuf::from(dir));
    }
    #[cfg(windows)]
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    #[cfg(not(windows))]
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));

    #[cfg(windows)]
    let dir = base.join(PROGRAM_NAME);
    #[cfg(not(windows))]
    let dir = base.join(format!(".{PROGRAM_NAME}"));

    ensure_dir(dir)
}

fn ensure_dir(dir: PathBuf) -> PathBuf {
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
    }
    dir
}

/// Creates `torrent_cache/` and `logs/` under the config directory.
pub fn create_app_dir_hierarchy() -> std::io::Result<()> {
    let base = app_dir();
    std::fs::create_dir_all(base.join(TORRENT_CACHE_FOLDER))?;
    std::fs::create_dir_all(base.join(LOGS_FOLDER))?;
    Ok(())
}

/// Verifies the config directory is readable and writable, with the same
/// Docker-specific hint the original printed.
pub fn check_app_dir_permissions() -> Result<(), CrustSeedError> {
    let dir = app_dir();
    let probe = dir.join(".crust-seed-write-test");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(_) => {
            let docker_message = if std::env::var("DOCKER_ENV").as_deref() == Ok("true") {
                " Use chown to set the owner to 65532:65532"
            } else {
                ""
            };
            Err(CrustSeedError::new(format!(
                "{PROGRAM_NAME} does not have R/W permissions on your config directory.{docker_message}"
            )))
        }
    }
}

pub fn db_path() -> PathBuf {
    app_dir().join(format!("{PROGRAM_NAME}.db"))
}

pub fn torrent_cache_dir() -> PathBuf {
    app_dir().join(TORRENT_CACHE_FOLDER)
}

pub fn logs_dir() -> PathBuf {
    app_dir().join(LOGS_FOLDER)
}

pub fn path_is_absolute(p: &str) -> bool {
    Path::new(p).is_absolute()
        || crate::constants::ABS_WIN_PATH_REGEX
            .is_match(p)
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_json() {
        let defaults = default_runtime_config();
        let json = serde_json::to_value(&defaults).unwrap();
        let parsed = parse_runtime_config(json).unwrap();
        assert_eq!(defaults, parsed);
    }

    /// `None` must vanish from the payload — the web UI's zod schema uses
    /// `.optional()`, which rejects an explicit `null`.
    #[test]
    fn none_fields_are_omitted_not_nulled() {
        let mut config = default_runtime_config();
        config.torrent_dir = None;
        config.api_key = None;
        let json = serde_json::to_value(&config).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("torrentDir"));
        assert!(!obj.contains_key("apiKey"));
        assert!(obj.contains_key("outputDir"));
    }

    #[test]
    fn strip_defaults_keeps_only_changes() {
        let mut config = default_runtime_config();
        assert!(strip_defaults(&config).is_empty());

        config.delay = 60;
        config.torznab = vec!["https://x.example/api?apikey=k".into()];
        let overrides = strip_defaults(&config);
        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides["delay"], serde_json::json!(60));
        assert!(overrides.contains_key("torznab"));
    }

    #[test]
    fn merge_applies_overrides_over_defaults() {
        let mut overrides = ConfigOverrides::new();
        overrides.insert("delay".into(), serde_json::json!(90));
        overrides.insert("matchMode".into(), serde_json::json!("partial"));
        let merged = merge_overrides(&overrides).unwrap();
        assert_eq!(merged.delay, 90);
        assert_eq!(merged.match_mode, MatchMode::Partial);
        assert_eq!(merged.action, Action::Inject); // untouched default
    }

    #[test]
    fn merge_treats_null_as_unset() {
        let mut overrides = ConfigOverrides::new();
        overrides.insert("searchCadence".into(), Value::Null);
        let merged = merge_overrides(&overrides).unwrap();
        assert_eq!(merged.search_cadence, None);
    }

    #[test]
    fn range_violations_are_rejected() {
        let mut overrides = ConfigOverrides::new();
        overrides.insert("delay".into(), serde_json::json!(5));
        assert!(merge_overrides(&overrides).is_err());

        let mut overrides = ConfigOverrides::new();
        overrides.insert("fuzzySizeThreshold".into(), serde_json::json!(0));
        assert!(merge_overrides(&overrides).is_err());
    }

    #[test]
    fn unknown_keys_are_dropped_from_overrides() {
        let value = serde_json::json!({ "delay": 45, "notAnOption": true });
        let overrides = parse_runtime_config_overrides(value).unwrap();
        assert!(overrides.contains_key("delay"));
        assert!(!overrides.contains_key("notAnOption"));
    }

    /// `seasonFromEpisodes: null` / `false` normalise to 0, which the original
    /// treats as disabled through JS falsiness. Some(0.0) must not read as
    /// enabled, or any three episodes would become a virtual season.
    #[test]
    fn a_zero_season_from_episodes_reads_as_disabled() {
        let mut config = default_runtime_config();
        config.season_from_episodes = Some(0.0);
        assert_eq!(config.season_from_episodes_ratio(), None);

        config.season_from_episodes = None;
        assert_eq!(config.season_from_episodes_ratio(), None);

        config.season_from_episodes = Some(1.0);
        assert_eq!(config.season_from_episodes_ratio(), Some(1.0));
    }

    #[test]
    fn webhook_entries_accept_both_shapes() {
        let value = serde_json::json!([
            "https://hook.example/1",
            { "url": "https://hook.example/2", "headers": { "X-Token": "abc" } }
        ]);
        let entries: Vec<WebhookEntry> = serde_json::from_value(value).unwrap();
        assert_eq!(entries[0].url(), "https://hook.example/1");
        assert_eq!(entries[1].url(), "https://hook.example/2");
    }
}
