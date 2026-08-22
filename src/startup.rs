//! Process startup: configuration resolution and singleton initialisation.
//!
//! Ported from `startup.ts`.
//!
//! Configuration is resolved in one of three ways, in order:
//!
//! 1. **Database** — the normal path once the app has run at least once.
//! 2. **Config file** — imported into the database on first run and then
//!    renamed aside, so the file and the web UI can never disagree.
//! 3. **Defaults** — a fresh install, which also gets an API key generated.

use sqlx::SqlitePool;

use crate::config::db_config::{get_db_config, set_db_config};
use crate::config::file::{get_file_config, transform_file_config};
use crate::config::runtime::set_runtime_config;
use crate::config::{
    ConfigOverrides, RuntimeConfig, create_app_dir_hierarchy, default_runtime_config,
    merge_overrides,
};
use crate::errors::CrustSeedError;
use crate::logger::Label;
use crate::utils::not_exists;

const MIGRATED_CONFIG_COMMENT: &str = "# This config file was imported into the crust-seed database. Use the Web UI to view or change settings.\n\n";

/// Resolves the effective configuration, importing a file config on first run.
pub async fn determine_runtime_config(
    pool: &SqlitePool,
    cli_overrides: &ConfigOverrides,
) -> Result<RuntimeConfig, CrustSeedError> {
    let stored = get_db_config(pool).await.unwrap_or_default();
    if !stored.is_empty() {
        return apply(&stored, cli_overrides);
    }

    match get_file_config() {
        Ok(Some((path, file_config))) => {
            let overrides = transform_file_config(&file_config);
            let config = merge_overrides(&overrides)?;
            set_db_config(pool, &config).await?;
            match backup_file_config(&path).await {
                Ok(backup) => tracing::info!(
                    label = Label::Config.as_str(),
                    "Migrated file config to database; settings now live in the database and Web UI. Preserved the old config file as {}.",
                    backup.file_name().unwrap_or_default().to_string_lossy()
                ),
                Err(e) => tracing::warn!(
                    label = Label::Config.as_str(),
                    "Imported the config file but could not rename it aside: {e}"
                ),
            }
            return apply(&crate::config::strip_defaults(&config), cli_overrides);
        }
        Ok(None) => {}
        Err(e) => {
            // A malformed config must not silently become "defaults": the user
            // would see cross-seed running with settings they never chose.
            return Err(e);
        }
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

/// Renames the imported config aside, never overwriting an existing backup.
async fn backup_file_config(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let base = path.with_extension(format!(
        "{}.bak",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    let mut backup = base.clone();
    let mut index = 1;
    while !not_exists(&backup).await {
        backup = base.with_extension(format!("bak.{index}"));
        index += 1;
    }
    tokio::fs::rename(path, &backup).await?;

    // Prepend an explanation so a user who finds the file later knows why it
    // stopped being read.
    if let Ok(contents) = tokio::fs::read_to_string(&backup).await {
        let _ = tokio::fs::write(&backup, format!("{MIGRATED_CONFIG_COMMENT}{contents}")).await;
    }
    Ok(backup)
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

    /// The file config is imported once and then renamed aside, so the file and
    /// the web UI cannot disagree afterwards.
    #[tokio::test]
    async fn a_file_config_is_imported_and_moved_aside() {
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
        assert_eq!(config.delay, 75);

        assert!(!dir.path().join("config.toml").exists());
        assert_eq!(get_db_config(&pool).await.unwrap()["delay"], json!(75));
        unsafe { std::env::remove_var("CONFIG_DIR") };
    }

    /// A malformed config must fail loudly rather than silently starting with
    /// settings the user never chose.
    #[tokio::test]
    async fn a_malformed_config_file_is_an_error() {
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
                .is_err()
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
