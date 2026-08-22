//! The database-backed configuration overrides.
//!
//! Ported from `dbConfig.ts`. The web UI writes here; the file config and the
//! defaults sit underneath. Only values that *differ* from the defaults are
//! stored, so changing a default in a future version propagates to users who
//! never touched that option.

use serde_json::{Map, Value};
use sqlx::SqlitePool;

use super::{
    ConfigOverrides, RuntimeConfig, merge_overrides, parse_runtime_config_overrides, strip_defaults,
};
use crate::errors::CrustSeedError;

pub async fn get_db_config(pool: &SqlitePool) -> Result<ConfigOverrides, CrustSeedError> {
    let stored: Option<Option<String>> = sqlx::query_scalar("SELECT settings_json FROM settings")
        .fetch_optional(pool)
        .await
        .map_err(|e| CrustSeedError::new(e.to_string()))?;

    let Some(Some(json)) = stored else {
        return Ok(Map::new());
    };
    let value: Value = serde_json::from_str(&json)
        .map_err(|e| CrustSeedError::new(format!("Invalid stored settings: {e}")))?;
    parse_runtime_config_overrides(value)
}

async fn write_overrides(
    pool: &SqlitePool,
    overrides: &ConfigOverrides,
) -> Result<(), CrustSeedError> {
    let json = Value::Object(overrides.clone()).to_string();
    // The settings row is seeded by the migration, but INSERT-on-missing keeps
    // a hand-edited database working.
    sqlx::query(
        "INSERT INTO settings (id, apikey, settings_json) VALUES (0, NULL, ?)
         ON CONFLICT (id) DO UPDATE SET settings_json = excluded.settings_json",
    )
    .bind(json)
    .execute(pool)
    .await
    .map_err(|e| CrustSeedError::new(e.to_string()))?;
    Ok(())
}

/// Replaces the whole stored configuration (the debug page's "replace").
pub async fn set_db_config(
    pool: &SqlitePool,
    config: &RuntimeConfig,
) -> Result<(), CrustSeedError> {
    write_overrides(pool, &strip_defaults(config)).await?;
    register_torznab_indexers(pool, config).await;
    Ok(())
}

/// Merges a partial update into the stored configuration.
pub async fn update_db_config(
    pool: &SqlitePool,
    partial: &ConfigOverrides,
) -> Result<RuntimeConfig, CrustSeedError> {
    let mut merged = get_db_config(pool).await.unwrap_or_default();
    for (key, value) in partial {
        merged.insert(key.clone(), value.clone());
    }
    // Validate by materialising the full config before persisting anything.
    let config = merge_overrides(&merged)?;
    write_overrides(pool, &strip_defaults(&config)).await?;
    register_torznab_indexers(pool, &config).await;
    Ok(config)
}

/// Creates `indexer` rows for any `torznab` URL that does not have one.
///
/// `torznab` is the legacy way to configure indexers; the UI manages them as
/// rows. Mirroring one into the other keeps both surfaces consistent.
async fn register_torznab_indexers(pool: &SqlitePool, config: &RuntimeConfig) {
    for tracker in &config.torznab {
        let Ok(parsed) = url::Url::parse(tracker) else {
            continue;
        };
        let base_url = format!("{}{}", parsed.origin().ascii_serialization(), parsed.path());
        let apikey = crate::utils::get_apikey(tracker).unwrap_or_default();

        let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM indexer WHERE url = ?")
            .bind(&base_url)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
        if existing.is_some() {
            continue;
        }
        let _ = sqlx::query("INSERT INTO indexer (url, apikey, enabled) VALUES (?, ?, 1)")
            .bind(&base_url)
            .bind(&apikey)
            .execute(pool)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_runtime_config;
    use crate::db::test_pool;

    #[tokio::test]
    async fn an_unconfigured_database_yields_no_overrides() {
        let pool = test_pool().await;
        assert!(get_db_config(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_non_default_values_are_stored() {
        let pool = test_pool().await;
        let mut config = default_runtime_config();
        config.delay = 90;
        set_db_config(&pool, &config).await.unwrap();

        let stored: String = sqlx::query_scalar("SELECT settings_json FROM settings")
            .fetch_one(&pool)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(value["delay"], serde_json::json!(90));
        assert!(value.get("action").is_none(), "defaults are not persisted");
    }

    #[tokio::test]
    async fn partial_updates_merge_with_what_is_stored() {
        let pool = test_pool().await;
        update_db_config(
            &pool,
            &Map::from_iter([("delay".to_string(), serde_json::json!(90))]),
        )
        .await
        .unwrap();
        let config = update_db_config(
            &pool,
            &Map::from_iter([("matchMode".to_string(), serde_json::json!("partial"))]),
        )
        .await
        .unwrap();

        assert_eq!(config.delay, 90);
        assert_eq!(config.match_mode, crate::constants::MatchMode::Partial);
    }

    #[tokio::test]
    async fn an_invalid_update_is_rejected_without_persisting() {
        let pool = test_pool().await;
        update_db_config(
            &pool,
            &Map::from_iter([("delay".to_string(), serde_json::json!(90))]),
        )
        .await
        .unwrap();

        assert!(
            update_db_config(
                &pool,
                &Map::from_iter([("delay".to_string(), serde_json::json!(1))]),
            )
            .await
            .is_err()
        );
        // The earlier valid value survives.
        assert_eq!(
            get_db_config(&pool).await.unwrap()["delay"],
            serde_json::json!(90)
        );
    }

    #[tokio::test]
    async fn torznab_urls_become_indexer_rows() {
        let pool = test_pool().await;
        let mut config = default_runtime_config();
        config.torznab = vec!["https://indexer.example/api?apikey=secret".into()];
        set_db_config(&pool, &config).await.unwrap();

        let rows: Vec<(String, String)> = sqlx::query_as("SELECT url, apikey FROM indexer")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "https://indexer.example/api");
        assert_eq!(rows[0].1, "secret");

        // Running again must not duplicate the row.
        set_db_config(&pool, &config).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM indexer")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
}
