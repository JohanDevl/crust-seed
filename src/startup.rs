//! Process startup: configuration resolution and singleton initialisation.
//!
//! Ported from `startup.ts`.
//!
//! Configuration lives in the database and nowhere else. A first run stores
//! the defaults and generates an API key; every later run reads back what the
//! web UI has written. There is no config file to keep in sync — see
//! `config/mod.rs` for why.

use sqlx::SqlitePool;

use crate::config::db_config::{get_db_config, set_db_config};
use crate::config::runtime::set_runtime_config;
use crate::config::{
    ConfigOverrides, RuntimeConfig, app_dir, create_app_dir_hierarchy, default_runtime_config,
    merge_overrides,
};
use crate::errors::CrustSeedError;
use crate::logger::Label;
use crate::utils::not_exists;

/// Resolves the effective configuration from the database, seeding it with the
/// defaults on a fresh install.
pub async fn determine_runtime_config(
    pool: &SqlitePool,
    cli_overrides: &ConfigOverrides,
) -> Result<RuntimeConfig, CrustSeedError> {
    warn_about_abandoned_config_files().await;

    let stored = get_db_config(pool).await.unwrap_or_default();
    if !stored.is_empty() {
        return apply(&stored, cli_overrides);
    }

    let defaults = default_runtime_config();
    set_db_config(pool, &defaults).await?;
    let _ = crate::user_auth::reset_api_key(pool).await;
    tracing::info!(
        label = Label::Config.as_str(),
        "Created initial database config from defaults"
    );
    apply(&ConfigOverrides::new(), cli_overrides)
}

/// Earlier versions read a `config.toml`/`config.json` from the app directory.
/// A user upgrading from one of those would otherwise see their file silently
/// stop taking effect, so say so instead of ignoring it in silence. The file is
/// never parsed — only its presence is checked.
async fn warn_about_abandoned_config_files() {
    let dir = app_dir();
    for name in ["config.toml", "config.json", "config.js"] {
        let path = dir.join(name);
        if not_exists(&path).await {
            continue;
        }
        tracing::warn!(
            label = Label::Config.as_str(),
            "Ignoring {}: crust-seed is configured entirely through the Web UI, and settings live in the database. Delete the file once you have re-entered its settings.",
            path.display()
        );
    }
}

fn apply(
    stored: &ConfigOverrides,
    cli_overrides: &ConfigOverrides,
) -> Result<RuntimeConfig, CrustSeedError> {
    let mut merged = stored.clone();
    // CLI flags win: they are per-invocation and must not be persisted.
    for (key, value) in cli_overrides {
        merged.insert(key.clone(), value.clone());
    }
    merge_overrides(&merged)
}

/// Creates `outputDir` and every `linkDir` that does not exist yet.
pub async fn ensure_configured_directories(config: &RuntimeConfig) {
    let mut directories: Vec<(String, String)> = Vec::new();
    if !config.output_dir.is_empty() {
        directories.push((config.output_dir.clone(), "outputDir".to_string()));
    }
    for (index, link_dir) in config.link_dirs.iter().enumerate() {
        directories.push((link_dir.clone(), format!("linkDir{index}")));
    }

    for (path, label) in directories {
        if !not_exists(&path).await {
            continue;
        }
        tracing::info!("Creating {label}: {path}");
        if let Err(e) = tokio::fs::create_dir_all(&path).await {
            tracing::error!(
                label = Label::Server.as_str(),
                "Failed to create {label} at {path}: {e}"
            );
        }
    }
}

/// Opens the database and applies migrations — everything a CLI command needs.
pub async fn init_minimal_runtime() -> Result<SqlitePool, CrustSeedError> {
    crate::config::check_app_dir_permissions()?;
    create_app_dir_hierarchy()
        .map_err(|e| CrustSeedError::new(format!("Could not create config directories: {e}")))?;
    crate::db::init_db().await
}

/// Everything [`init_minimal_runtime`] does, plus logging, configuration,
/// torrent clients and the directories they need.
pub async fn init_full_runtime(
    verbose: bool,
    cli_overrides: &ConfigOverrides,
) -> Result<(SqlitePool, RuntimeConfig, crate::logger::LoggerGuards), CrustSeedError> {
    crate::config::check_app_dir_permissions()?;
    create_app_dir_hierarchy()
        .map_err(|e| CrustSeedError::new(format!("Could not create config directories: {e}")))?;

    let guards = crate::logger::initialize_logger(verbose);
    let pool = crate::db::init_db().await?;

    let config = determine_runtime_config(&pool, cli_overrides).await?;
    // Register secrets before anything else logs a URL.
    crate::logger::register_config_secrets(&config);
    set_runtime_config(config.clone());

    ensure_configured_directories(&config).await;
    crate::clients::instantiate_download_clients(&config);

    Ok((pool, config, guards))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use serde_json::json;

    #[tokio::test]
    async fn a_fresh_database_gets_defaults_and_an_api_key() {
        // CONFIG_DIR is process-global; serialise the tests that set it.
        let _guard = crate::config::runtime::config_test_guard_async().await;
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CONFIG_DIR", dir.path()) };
        let pool = test_pool().await;

        let config = determine_runtime_config(&pool, &ConfigOverrides::new())
            .await
            .unwrap();
        assert_eq!(config.delay, default_runtime_config().delay);

        let api_key = crate::user_auth::get_api_key(&pool).await.unwrap();
        assert!(api_key.len() >= 24);
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }

    #[tokio::test]
    async fn stored_settings_are_used_on_subsequent_runs() {
        // CONFIG_DIR is process-global; serialise the tests that set it.
        let _guard = crate::config::runtime::config_test_guard_async().await;
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CONFIG_DIR", dir.path()) };
        let pool = test_pool().await;

        let mut config = default_runtime_config();
        config.delay = 120;
        set_db_config(&pool, &config).await.unwrap();

        let resolved = determine_runtime_config(&pool, &ConfigOverrides::new())
            .await
            .unwrap();
        assert_eq!(resolved.delay, 120);
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }

    /// CLI flags override the stored configuration for one invocation without
    /// being written back.
    #[tokio::test]
    async fn cli_overrides_win_and_are_not_persisted() {
        // CONFIG_DIR is process-global; serialise the tests that set it.
        let _guard = crate::config::runtime::config_test_guard_async().await;
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CONFIG_DIR", dir.path()) };
        let pool = test_pool().await;

        let mut config = default_runtime_config();
        config.delay = 120;
        set_db_config(&pool, &config).await.unwrap();

        let mut overrides = ConfigOverrides::new();
        overrides.insert("delay".into(), json!(45));
        let resolved = determine_runtime_config(&pool, &overrides).await.unwrap();
        assert_eq!(resolved.delay, 45);

        // The stored value is untouched.
        assert_eq!(get_db_config(&pool).await.unwrap()["delay"], json!(120));
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }

    /// A config file left over from an older version must not change what the
    /// daemon runs with: the database is the only source of settings now.
    #[tokio::test]
    async fn a_leftover_config_file_is_ignored() {
        // CONFIG_DIR is process-global; serialise the tests that set it.
        let _guard = crate::config::runtime::config_test_guard_async().await;
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CONFIG_DIR", dir.path()) };
        tokio::fs::write(dir.path().join("config.toml"), "delay = 75\n")
            .await
            .unwrap();

        let pool = test_pool().await;
        let config = determine_runtime_config(&pool, &ConfigOverrides::new())
            .await
            .unwrap();
        assert_eq!(config.delay, default_runtime_config().delay);

        // The file is left where it is — crust-seed no longer owns it — and its
        // contents never reach the database.
        assert!(dir.path().join("config.toml").exists());
        assert!(get_db_config(&pool).await.unwrap().is_empty());
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }

    /// A file that would once have failed to parse is now simply not read, so
    /// it cannot stop the daemon from starting.
    #[tokio::test]
    async fn a_malformed_config_file_no_longer_blocks_startup() {
        // CONFIG_DIR is process-global; serialise the tests that set it.
        let _guard = crate::config::runtime::config_test_guard_async().await;
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("CONFIG_DIR", dir.path()) };
        tokio::fs::write(dir.path().join("config.toml"), "delay = \"not a number\"\n")
            .await
            .unwrap();

        let pool = test_pool().await;
        assert!(
            determine_runtime_config(&pool, &ConfigOverrides::new())
                .await
                .is_ok()
        );
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }

    #[tokio::test]
    async fn configured_directories_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = default_runtime_config();
        config.output_dir = dir.path().join("out").to_string_lossy().into_owned();
        config.link_dirs = vec![dir.path().join("links").to_string_lossy().into_owned()];

        ensure_configured_directories(&config).await;
        assert!(dir.path().join("out").is_dir());
        assert!(dir.path().join("links").is_dir());
    }
}
