//! Process-global runtime configuration.
//!
//! Ported from `runtimeConfig.ts`. The original kept a module-level `let` and
//! merged a `configOverride` on every read; jobs use that to force a search to
//! ignore `excludeRecentSearch`. Here the global lives behind an `ArcSwap`-like
//! `RwLock<Arc<..>>` so readers never block each other and the web UI can swap
//! the whole config in after a settings save.

use std::sync::{Arc, LazyLock, RwLock};

use serde_json::{Map, Value};

use super::{ConfigOverrides, RuntimeConfig, default_runtime_config, merge_overrides};

static RUNTIME_CONFIG: LazyLock<RwLock<Arc<RuntimeConfig>>> =
    LazyLock::new(|| RwLock::new(Arc::new(default_runtime_config())));

pub fn set_runtime_config(config: RuntimeConfig) {
    *RUNTIME_CONFIG.write().expect("runtime config lock") = Arc::new(config);
}

pub fn get_runtime_config() -> Arc<RuntimeConfig> {
    RUNTIME_CONFIG.read().expect("runtime config lock").clone()
}

/// `getRuntimeConfig(configOverride)` — a per-call view with some options
/// replaced. Invalid overrides fall back to the base config rather than
/// panicking, matching the original's spread semantics (which could not fail).
pub fn get_runtime_config_with(config_override: &ConfigOverrides) -> Arc<RuntimeConfig> {
    if config_override.is_empty() {
        return get_runtime_config();
    }
    let base = get_runtime_config();
    let Ok(Value::Object(mut merged)) = serde_json::to_value(base.as_ref()) else {
        return base;
    };
    for (key, value) in config_override {
        if value.is_null() {
            merged.remove(key);
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    match super::parse_runtime_config(Value::Object(merged)) {
        Ok(config) => Arc::new(config),
        Err(_) => base,
    }
}

/// Convenience for building a one-off override map.
pub fn overrides_from(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> ConfigOverrides {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key.to_string(), value);
    }
    map
}

/// Rebuilds the global from sparse overrides layered on the defaults.
pub fn apply_overrides(overrides: &ConfigOverrides) -> Result<(), crate::errors::CrustSeedError> {
    set_runtime_config(merge_overrides(overrides)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::MatchMode;

    #[test]
    fn per_call_overrides_do_not_mutate_the_global() {
        set_runtime_config(default_runtime_config());
        let overridden = get_runtime_config_with(&overrides_from([(
            "matchMode",
            serde_json::json!("partial"),
        )]));
        assert_eq!(overridden.match_mode, MatchMode::Partial);
        assert_eq!(get_runtime_config().match_mode, MatchMode::Flexible);
    }
}
