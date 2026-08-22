//! The tRPC procedures.
//!
//! Ported from `trpc/routers/*.ts`. Names, inputs and outputs must match
//! `web/api-types/src/index.ts`, which is the contract the vendored UI is
//! type-checked against.

use serde_json::{Value, json};
use sqlx::SqlitePool;

use super::trpc::{ProcedureResult, TrpcError, TrpcErrorCode, ok};
use crate::config::db_config::{get_db_config, set_db_config, update_db_config};
use crate::config::runtime::set_runtime_config;
use crate::config::{
    ConfigOverrides, default_runtime_config, merge_overrides, parse_runtime_config,
};
use crate::constants::{PROGRAM_NAME, PROGRAM_VERSION};
use crate::jobs::{JobName, get_job_last_run, get_jobs, trigger_job};
use crate::user_auth::{
    SessionUser, create_initial_user_if_needed, create_session, get_api_key, has_users,
    reset_api_key, set_api_key, validate_user_credentials,
};
use crate::utils::now_ms;

/// Per-request context: who is calling, and what the handler may do to the
/// session cookie.
pub struct Context {
    pub pool: SqlitePool,
    pub user: Option<SessionUser>,
    /// Set by `auth.logIn`/`auth.setup`; the transport turns it into a cookie.
    pub set_session: Option<String>,
    pub clear_session: bool,
}

impl Context {
    pub fn require_user(&self) -> Result<&SessionUser, TrpcError> {
        self.user.as_ref().ok_or_else(TrpcError::unauthorized)
    }
}

/// The window during which the very first user may be created without
/// authentication. Bounded so an unattended instance cannot be claimed later.
const SIGNUP_WINDOW_MS: i64 = 5 * 60 * 1000;

static PROCESS_START: std::sync::LazyLock<i64> = std::sync::LazyLock::new(now_ms);

fn signup_window_ms_remaining() -> i64 {
    (SIGNUP_WINDOW_MS - (now_ms() - *PROCESS_START)).max(0)
}

/// Marks the point from which the signup window is measured.
pub fn start_signup_window() {
    let _ = *PROCESS_START;
}

/// Dispatches one procedure call.
pub async fn call(ctx: &mut Context, path: &str, input: Option<Value>) -> ProcedureResult {
    match path {
        // ─── auth ───────────────────────────────────────────────────────
        "auth.authStatus" => {
            let user_exists = has_users(&ctx.pool).await.unwrap_or(false);
            ok(json!({
                "userExists": user_exists,
                "signupAllowed": !user_exists && signup_window_ms_remaining() > 0,
                "signupWindowMsRemaining": signup_window_ms_remaining(),
                "isDocker": std::env::var("DOCKER_ENV").as_deref() == Ok("true"),
                "isLoggedIn": ctx.user.is_some(),
                "user": ctx.user,
            }))
        }
        "auth.setup" => {
            let (username, password) = credentials(input.as_ref())?;
            if password.chars().count() < 8 {
                return Err(TrpcError::bad_request(
                    "Password must be at least 8 characters",
                ));
            }
            if has_users(&ctx.pool).await.unwrap_or(true) {
                return Err(TrpcError::bad_request("Setup has already been completed"));
            }
            if signup_window_ms_remaining() == 0 {
                return Err(TrpcError::new(
                    TrpcErrorCode::Forbidden,
                    "Initial setup window has expired. Restart crust-seed to create the first user.",
                ));
            }
            let user = create_initial_user_if_needed(&ctx.pool, &username, &password)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?
                .ok_or_else(|| TrpcError::internal("Failed to create user"))?;
            let session = create_session(&ctx.pool, user.id)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?;
            ctx.set_session = Some(session);
            ok(Value::Null)
        }
        "auth.logIn" => {
            let (username, password) = credentials(input.as_ref())?;
            // First-run convenience: logging in with no users yet creates the
            // first one, but only inside the signup window.
            if !has_users(&ctx.pool).await.unwrap_or(true) {
                if signup_window_ms_remaining() == 0 {
                    return Err(TrpcError::new(
                        TrpcErrorCode::Forbidden,
                        "Initial setup window has expired. Restart crust-seed to create the first user.",
                    ));
                }
                if let Some(user) = create_initial_user_if_needed(&ctx.pool, &username, &password)
                    .await
                    .map_err(|e| TrpcError::internal(e.to_string()))?
                {
                    let session = create_session(&ctx.pool, user.id)
                        .await
                        .map_err(|e| TrpcError::internal(e.to_string()))?;
                    ctx.set_session = Some(session);
                    return ok(Value::Null);
                }
            }

            let user = validate_user_credentials(&ctx.pool, &username, &password)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?
                .ok_or_else(TrpcError::unauthorized)?;
            let session = create_session(&ctx.pool, user.id)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?;
            ctx.set_session = Some(session);
            ok(Value::Null)
        }
        "auth.logOut" => {
            ctx.clear_session = true;
            ok(Value::Null)
        }

        // ─── meta ───────────────────────────────────────────────────────
        "meta.getBuildInfo" => ok(json!({
            "appName": PROGRAM_NAME,
            "version": PROGRAM_VERSION,
            "build": crate::build_info::build_info(),
        })),

        // ─── settings ───────────────────────────────────────────────────
        "settings.get" => {
            ctx.require_user()?;
            let overrides = get_db_config(&ctx.pool).await.unwrap_or_default();
            let config =
                merge_overrides(&overrides).map_err(|e| TrpcError::internal(e.to_string()))?;
            let api_key = get_api_key(&ctx.pool)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?;
            ok(json!({ "config": config, "apiKey": api_key }))
        }
        "settings.save" => {
            ctx.require_user()?;
            let overrides = object_input(input.as_ref())?;
            let config = update_db_config(&ctx.pool, &overrides)
                .await
                .map_err(|e| TrpcError::bad_request(e.to_string()))?;
            set_runtime_config(config);
            // Client settings may have changed under us.
            crate::clients::reload_download_clients();
            ok(json!({ "success": true }))
        }
        "settings.replace" => {
            ctx.require_user()?;
            let raw = input.clone().unwrap_or(Value::Null);
            let config =
                parse_runtime_config(raw).map_err(|e| TrpcError::bad_request(e.to_string()))?;
            set_db_config(&ctx.pool, &config)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?;
            set_runtime_config(config);
            ok(json!({ "success": true }))
        }
        "settings.setApiKey" => {
            ctx.require_user()?;
            let api_key = input
                .as_ref()
                .and_then(|v| v.get("apiKey"))
                .and_then(Value::as_str)
                .ok_or_else(|| TrpcError::bad_request("apiKey is required"))?;
            let api_key = set_api_key(&ctx.pool, api_key)
                .await
                .map_err(|e| TrpcError::bad_request(e.to_string()))?;
            ok(json!({ "apiKey": api_key }))
        }
        "settings.resetApiKey" => {
            ctx.require_user()?;
            let api_key = reset_api_key(&ctx.pool)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?;
            ok(json!({ "apiKey": api_key }))
        }
        "settings.validate" => {
            ctx.require_user()?;
            ok(json!({
                "status": "success",
                "validations": { "paths": true, "torznab": true }
            }))
        }
        "settings.testNotification" => {
            ctx.require_user()?;
            let webhooks = input
                .as_ref()
                .and_then(|v| v.get("webhooks"))
                .cloned()
                .unwrap_or(json!([]));
            let entries = serde_json::from_value(webhooks)
                .map_err(|e| TrpcError::bad_request(e.to_string()))?;
            let results = crate::push_notifier::send_test_notification(entries).await;
            ok(json!({ "results": results }))
        }

        // ─── indexers ───────────────────────────────────────────────────
        "indexers.getAll" => {
            ctx.require_user()?;
            let mut indexers = crate::indexers::get_all_indexers(&ctx.pool)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?;
            indexers.sort_by(|a, b| {
                a.name
                    .clone()
                    .unwrap_or_default()
                    .to_lowercase()
                    .cmp(&b.name.clone().unwrap_or_default().to_lowercase())
            });
            ok(indexers)
        }
        "indexers.create" => {
            ctx.require_user()?;
            let input = input.ok_or_else(|| TrpcError::bad_request("input is required"))?;
            let indexer = crate::server::routes::create_indexer(&ctx.pool, &input)
                .await
                .map_err(TrpcError::bad_request)?;
            ok(indexer)
        }
        "indexers.update" => {
            ctx.require_user()?;
            let input = input.ok_or_else(|| TrpcError::bad_request("input is required"))?;
            let indexer = crate::server::routes::update_indexer(&ctx.pool, &input)
                .await
                .map_err(|e| TrpcError::new(TrpcErrorCode::NotFound, e))?;
            ok(indexer)
        }
        "indexers.delete" => {
            ctx.require_user()?;
            let id = id_input(input.as_ref())?;
            let indexer = crate::server::routes::delete_indexer(&ctx.pool, id)
                .await
                .map_err(|e| TrpcError::new(TrpcErrorCode::NotFound, e))?;
            ok(json!({ "success": true, "indexer": indexer }))
        }
        "indexers.testExisting" => {
            ctx.require_user()?;
            let id = id_input(input.as_ref())?;
            let indexer = crate::indexers::get_indexer_by_id(&ctx.pool, id)
                .await
                .ok()
                .flatten()
                .ok_or_else(|| {
                    TrpcError::new(
                        TrpcErrorCode::NotFound,
                        format!("Indexer with ID {id} not found"),
                    )
                })?;
            crate::torznab::fetch_caps(&indexer)
                .await
                .map_err(TrpcError::bad_request)?;
            ok(json!({ "success": true, "message": "Connection successful" }))
        }
        "indexers.testNew" => {
            ctx.require_user()?;
            let input = input.ok_or_else(|| TrpcError::bad_request("input is required"))?;
            crate::server::routes::test_indexer_connection(&input)
                .await
                .map_err(TrpcError::bad_request)?;
            ok(json!({ "success": true, "message": "Connection successful" }))
        }
        "indexers.mergeDisabled" => {
            ctx.require_user()?;
            let source_id = field_i64(input.as_ref(), "sourceId")?;
            let target_id = field_i64(input.as_ref(), "targetId")?;
            let merged =
                crate::server::routes::merge_disabled_indexer(&ctx.pool, source_id, target_id)
                    .await
                    .map_err(TrpcError::bad_request)?;
            ok(merged)
        }

        // ─── jobs ───────────────────────────────────────────────────────
        "jobs.getJobStatuses" => {
            ctx.require_user()?;
            let now = now_ms();
            let mut statuses = Vec::new();
            for job in get_jobs().await {
                let last_run = get_job_last_run(&ctx.pool, job.name).await;
                let eligibility = last_run.map(|l| l + job.cadence_ms).unwrap_or(now);
                let is_active = job.state.lock().await.is_active;
                statuses.push(json!({
                    "name": job.name,
                    "interval": crate::config::duration::format_ms_long(job.cadence_ms),
                    "lastExecution": last_run.map(iso_from_ms),
                    "lastDuration": Value::Null,
                    "nextExecution": if now >= eligibility {
                        "now".to_string()
                    } else {
                        iso_from_ms(eligibility)
                    },
                    "isActive": is_active,
                    "canRunNow": !is_active,
                }));
            }
            ok(statuses)
        }
        "jobs.triggerJob" => {
            ctx.require_user()?;
            let name = input
                .as_ref()
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .and_then(JobName::from_str_exact)
                .ok_or_else(|| TrpcError::bad_request("Unknown job name"))?;
            trigger_job(name, ConfigOverrides::new())
                .await
                .map_err(TrpcError::bad_request)?;
            // Kick the scheduler so the job starts now rather than on the tick.
            let pool = ctx.pool.clone();
            tokio::spawn(async move { crate::jobs::check_jobs(&pool, false).await });
            ok(json!({
                "success": true,
                "message": format!("{}: running ahead of schedule", name.as_str())
            }))
        }

        // ─── logs ───────────────────────────────────────────────────────
        "logs.getVerbose" => {
            ctx.require_user()?;
            let logs = crate::log_watcher::get_recent_logs(1000).await;
            let text = logs
                .iter()
                .map(|log| {
                    format!(
                        "{} {}: {}{}",
                        log.timestamp,
                        log.level,
                        log.label
                            .as_ref()
                            .map(|l| format!("[{l}] "))
                            .unwrap_or_default(),
                        log.message
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            ok(text)
        }
        "logs.getRecentLogs" => {
            ctx.require_user()?;
            let limit = input
                .as_ref()
                .and_then(|v| v.get("limit"))
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 1000) as usize;
            let mut logs = crate::log_watcher::get_recent_logs(limit).await;
            logs.reverse(); // newest first
            ok(logs)
        }

        // ─── health / stats / searchees ─────────────────────────────────
        "health.get" => {
            ctx.require_user()?;
            let problems = crate::health::collect_problems(&ctx.pool).await;
            let db = crate::health::diagnostics::collect_db_diagnostics(&ctx.pool).await;
            ok(json!({ "problems": problems, "diagnostics": { "db": db } }))
        }
        "stats.getOverview" => {
            ctx.require_user()?;
            crate::server::routes::stats_overview(&ctx.pool)
                .await
                .map_err(TrpcError::internal)
        }
        "stats.getIndexerStats" => {
            ctx.require_user()?;
            let indexers = crate::indexers::get_all_indexers(&ctx.pool)
                .await
                .map_err(|e| TrpcError::internal(e.to_string()))?;
            let mut stats: Vec<Value> = indexers
                .iter()
                .map(|indexer| {
                    json!({
                        "id": indexer.id,
                        "name": indexer.name.clone().unwrap_or_else(|| format!("Indexer {}", indexer.id)),
                        "enabled": indexer.enabled,
                        "status": indexer.status.map(|s| s.as_str()).unwrap_or("unknown"),
                    })
                })
                .collect();
            stats.sort_by_key(|s| s["name"].as_str().unwrap_or_default().to_lowercase());
            ok(stats)
        }
        "searchees.list" => {
            ctx.require_user()?;
            crate::server::routes::searchees_list(&ctx.pool, input.as_ref())
                .await
                .map_err(TrpcError::internal)
        }
        "searchees.bulkSearch" => {
            ctx.require_user()?;
            let names: Vec<String> = input
                .as_ref()
                .and_then(|v| v.get("names"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            if names.is_empty() {
                return Err(TrpcError::bad_request(
                    "No valid item names provided for bulk search",
                ));
            }
            let force = input
                .as_ref()
                .and_then(|v| v.get("force"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // "Force" means ignore the search-history filters for this run.
            let config = if force {
                let mut config = crate::config::runtime::get_runtime_config()
                    .as_ref()
                    .clone();
                config.exclude_recent_search = Some(1);
                config.exclude_older = Some(i64::MAX);
                config
            } else {
                crate::config::runtime::get_runtime_config()
                    .as_ref()
                    .clone()
            };
            let summary = crate::pipeline::bulk_search_by_names(&ctx.pool, &names, &config).await;
            ok(summary)
        }

        // ─── clients ────────────────────────────────────────────────────
        "clients.testConnection" => {
            ctx.require_user()?;
            let input = input.ok_or_else(|| TrpcError::bad_request("input is required"))?;
            crate::server::routes::test_client_connection(&input)
                .await
                .map_err(TrpcError::bad_request)
        }

        _ => Err(TrpcError::new(
            TrpcErrorCode::NotFound,
            format!("No procedure found on path \"{path}\""),
        )),
    }
}

fn iso_from_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn credentials(input: Option<&Value>) -> Result<(String, String), TrpcError> {
    let input = input.ok_or_else(|| TrpcError::bad_request("input is required"))?;
    let username = input
        .get("username")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TrpcError::bad_request("Username is required"))?;
    let password = input
        .get("password")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| TrpcError::bad_request("Password is required"))?;
    Ok((username.to_string(), password.to_string()))
}

fn object_input(input: Option<&Value>) -> Result<ConfigOverrides, TrpcError> {
    match input {
        Some(Value::Object(map)) => Ok(map.clone()),
        _ => Err(TrpcError::bad_request("expected an object")),
    }
}

fn id_input(input: Option<&Value>) -> Result<i64, TrpcError> {
    field_i64(input, "id")
}

fn field_i64(input: Option<&Value>, field: &str) -> Result<i64, TrpcError> {
    input
        .and_then(|v| v.get(field))
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| TrpcError::bad_request(format!("{field} is required")))
}

/// Whether a procedure streams (and must therefore be served as SSE).
pub fn is_subscription(path: &str) -> bool {
    path == "logs.subscribe"
}

/// Defaults used when nothing has been configured yet, so `settings.get`
/// answers before the first save.
pub fn fallback_config() -> crate::config::RuntimeConfig {
    default_runtime_config()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    async fn context(pool: SqlitePool, user: Option<SessionUser>) -> Context {
        Context {
            pool,
            user,
            set_session: None,
            clear_session: false,
        }
    }

    fn logged_in() -> Option<SessionUser> {
        Some(SessionUser {
            id: 1,
            username: "alice".into(),
        })
    }

    #[tokio::test]
    async fn authed_procedures_reject_anonymous_callers() {
        let pool = test_pool().await;
        let mut ctx = context(pool, None).await;
        for path in [
            "settings.get",
            "indexers.getAll",
            "jobs.getJobStatuses",
            "health.get",
            "logs.getVerbose",
        ] {
            let error = call(&mut ctx, path, None).await.unwrap_err();
            assert_eq!(error.code, TrpcErrorCode::Unauthorized, "{path}");
        }
    }

    /// authStatus is deliberately unauthenticated — the login page needs it.
    #[tokio::test]
    async fn auth_status_is_public() {
        let pool = test_pool().await;
        let mut ctx = context(pool, None).await;
        let result = call(&mut ctx, "auth.authStatus", None).await.unwrap();
        assert_eq!(result["userExists"], json!(false));
        assert_eq!(result["isLoggedIn"], json!(false));
        assert!(result["signupAllowed"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn build_info_is_public() {
        let pool = test_pool().await;
        let mut ctx = context(pool, None).await;
        let result = call(&mut ctx, "meta.getBuildInfo", None).await.unwrap();
        assert_eq!(result["appName"], json!(PROGRAM_NAME));
        assert_eq!(result["version"], json!(PROGRAM_VERSION));
    }

    #[tokio::test]
    async fn logging_in_creates_the_first_user_and_a_session() {
        let pool = test_pool().await;
        let mut ctx = context(pool.clone(), None).await;
        call(
            &mut ctx,
            "auth.logIn",
            Some(json!({ "username": "alice", "password": "password" })),
        )
        .await
        .unwrap();

        assert!(ctx.set_session.is_some());
        assert!(has_users(&pool).await.unwrap());

        // A second login with the wrong password is rejected.
        let mut ctx = context(pool, None).await;
        let error = call(
            &mut ctx,
            "auth.logIn",
            Some(json!({ "username": "alice", "password": "wrong" })),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, TrpcErrorCode::Unauthorized);
    }

    #[tokio::test]
    async fn setup_refuses_a_short_password() {
        let pool = test_pool().await;
        let mut ctx = context(pool, None).await;
        let error = call(
            &mut ctx,
            "auth.setup",
            Some(json!({ "username": "alice", "password": "short" })),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, TrpcErrorCode::BadRequest);
    }

    #[tokio::test]
    async fn logging_out_clears_the_session() {
        let pool = test_pool().await;
        let mut ctx = context(pool, logged_in()).await;
        call(&mut ctx, "auth.logOut", None).await.unwrap();
        assert!(ctx.clear_session);
    }

    #[tokio::test]
    async fn settings_round_trip_through_save_and_get() {
        let pool = test_pool().await;
        let mut ctx = context(pool, logged_in()).await;

        call(
            &mut ctx,
            "settings.save",
            Some(json!({ "delay": 90, "matchMode": "partial" })),
        )
        .await
        .unwrap();

        let settings = call(&mut ctx, "settings.get", None).await.unwrap();
        assert_eq!(settings["config"]["delay"], json!(90));
        assert_eq!(settings["config"]["matchMode"], json!("partial"));
        assert!(settings["apiKey"].as_str().unwrap().len() >= 24);
    }

    #[tokio::test]
    async fn an_invalid_setting_is_a_bad_request_not_a_crash() {
        let pool = test_pool().await;
        let mut ctx = context(pool, logged_in()).await;
        let error = call(&mut ctx, "settings.save", Some(json!({ "delay": 1 })))
            .await
            .unwrap_err();
        assert_eq!(error.code, TrpcErrorCode::BadRequest);
    }

    #[tokio::test]
    async fn an_unknown_procedure_is_a_not_found() {
        let pool = test_pool().await;
        let mut ctx = context(pool, logged_in()).await;
        let error = call(&mut ctx, "nope.doesNotExist", None).await.unwrap_err();
        assert_eq!(error.code, TrpcErrorCode::NotFound);
    }

    #[tokio::test]
    async fn the_api_key_can_be_set_and_reset() {
        let pool = test_pool().await;
        let mut ctx = context(pool, logged_in()).await;

        let set = call(
            &mut ctx,
            "settings.setApiKey",
            Some(json!({ "apiKey": "a-key-that-is-long-enough-here" })),
        )
        .await
        .unwrap();
        assert_eq!(set["apiKey"], json!("a-key-that-is-long-enough-here"));

        let reset = call(&mut ctx, "settings.resetApiKey", None).await.unwrap();
        assert_ne!(reset["apiKey"], set["apiKey"]);
    }

    #[tokio::test]
    async fn a_short_api_key_is_rejected() {
        let pool = test_pool().await;
        let mut ctx = context(pool, logged_in()).await;
        let error = call(
            &mut ctx,
            "settings.setApiKey",
            Some(json!({ "apiKey": "short" })),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, TrpcErrorCode::BadRequest);
    }

    #[test]
    fn only_the_log_stream_is_a_subscription() {
        assert!(is_subscription("logs.subscribe"));
        assert!(!is_subscription("logs.getRecentLogs"));
    }
}
