//! HTTP routes: tRPC transport, the REST API and the Prowlarr indexer API.
//!
//! Ported from `routes/baseApi.ts`, `routes/indexerApi.ts` and
//! `routes/devLogin.ts`, plus `trpc/fastifyAdapter.ts`.

use std::collections::HashMap;
use std::convert::Infallible;

use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use super::AppState;
use super::routers::{self, Context};
use super::trpc::{
    ProcedureResult, TrpcError, batch_status, input_for, parse_inputs, response_body, split_paths,
    sse,
};
use crate::constants::{Decision, InjectionResult, PROGRAM_NAME, PROGRAM_VERSION};
use crate::logger::Label;
use crate::searchee::SearcheeLabel;
use crate::user_auth::{SESSION_COOKIE_NAME, check_api_key, validate_session};

// ─── tRPC transport ─────────────────────────────────────────────────────────

pub fn trpc_router() -> Router<AppState> {
    // Only the wildcard form: the client always appends a procedure path, and
    // a bare "/trpc/" route would try to extract a path parameter that is not
    // there.
    Router::new().route("/trpc/{*path}", get(trpc_get).post(trpc_post))
}

fn session_id_from(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|cookie| {
        let (name, value) = cookie.trim().split_once('=')?;
        (name == SESSION_COOKIE_NAME).then(|| value.to_string())
    })
}

async fn build_context(pool: &SqlitePool, headers: &HeaderMap) -> Context {
    let user = match session_id_from(headers) {
        Some(session_id) => validate_session(pool, &session_id).await.ok().flatten(),
        None => None,
    };
    Context {
        pool: pool.clone(),
        user,
        set_session: None,
        clear_session: false,
    }
}

/// The session cookie. `SameSite=Lax` still permits the top-level navigation
/// the dev-login redirect performs.
fn session_cookie(session_id: &str) -> String {
    format!(
        "{SESSION_COOKIE_NAME}={session_id}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}",
        crate::user_auth::SESSION_EXPIRY_MS / 1000
    )
}

fn cleared_session_cookie() -> String {
    format!("{SESSION_COOKIE_NAME}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0")
}

async fn trpc_get(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if routers::is_subscription(&path) {
        return trpc_subscribe(state, path, params, headers).await;
    }
    let inputs = parse_inputs(params.get("input").map(String::as_str));
    dispatch(state, path, inputs, params.contains_key("batch"), headers).await
}

async fn trpc_post(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let inputs = parse_inputs(Some(&body));
    dispatch(state, path, inputs, params.contains_key("batch"), headers).await
}

async fn dispatch(
    state: AppState,
    path: String,
    inputs: Value,
    batched: bool,
    headers: HeaderMap,
) -> Response {
    let paths = split_paths(&path);
    if paths.is_empty() {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let mut ctx = build_context(&state.pool, &headers).await;
    let mut results: Vec<ProcedureResult> = Vec::with_capacity(paths.len());
    for (index, procedure) in paths.iter().enumerate() {
        let input = input_for(&inputs, index, batched);
        results.push(routers::call(&mut ctx, procedure, input).await);
    }

    let status = StatusCode::from_u16(batch_status(&results)).unwrap_or(StatusCode::OK);
    let body = response_body(&paths, &results, batched);

    let mut response = (status, Json(body)).into_response();
    if let Some(session_id) = &ctx.set_session
        && let Ok(value) = session_cookie(session_id).parse()
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    if ctx.clear_session
        && let Ok(value) = cleared_session_cookie().parse()
    {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

/// `logs.subscribe`, served as Server-Sent Events.
///
/// The client's `sseStreamConsumer` expects a `connected` frame first, then
/// unnamed `message` frames carrying each entry.
async fn trpc_subscribe(
    state: AppState,
    _path: String,
    params: HashMap<String, String>,
    headers: HeaderMap,
) -> Response {
    let ctx = build_context(&state.pool, &headers).await;
    if ctx.user.is_none() {
        let error = TrpcError::unauthorized();
        return (
            StatusCode::UNAUTHORIZED,
            Json(super::trpc::response_item("logs.subscribe", &Err(error))),
        )
            .into_response();
    }

    let limit = parse_inputs(params.get("input").map(String::as_str))
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;

    let history = crate::log_watcher::get_recent_logs(limit).await;
    let mut receiver = crate::log_watcher::subscribe();

    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(Event::default().event(sse::CONNECTED_EVENT).data("{}"));
        for entry in history {
            let data = serde_json::to_string(&entry).unwrap_or_else(|_| "null".into());
            yield Ok(Event::default().data(data));
        }
        loop {
            match receiver.recv().await {
                Ok(entry) => {
                    let data = serde_json::to_string(&entry).unwrap_or_else(|_| "null".into());
                    yield Ok(Event::default().data(data));
                }
                // Lagged: the subscriber fell behind. Keep going rather than
                // ending the stream — the UI would otherwise stop updating.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text(""),
        )
        .into_response()
}

// ─── Machine-to-machine API ─────────────────────────────────────────────────

pub fn base_api_router() -> Router<AppState> {
    Router::new()
        .route("/ping", get(|| async { "OK" }))
        .route("/status", get(status_route))
        .route("/webhook", post(webhook_route))
        .route("/announce", post(announce_route))
        .route("/job", post(job_route))
}

#[derive(Debug, Deserialize)]
struct ApiKeyQuery {
    apikey: Option<String>,
}

/// API-key check for the machine-to-machine routes: `?apikey=` or `X-Api-Key`.
async fn authorize(pool: &SqlitePool, headers: &HeaderMap, query: &ApiKeyQuery) -> bool {
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| query.apikey.clone())
        .unwrap_or_default();
    if check_api_key(pool, &api_key).await {
        return true;
    }
    tracing::error!(
        label = Label::Server.as_str(),
        "Unauthorized API access attempt"
    );
    false
}

const UNAUTHORIZED_MESSAGE: &str =
    "Specify the API key in an X-Api-Key header or an apikey query param.";

async fn status_route(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }
    (StatusCode::OK, "OK").into_response()
}

/// Accepts a JSON body or a form-encoded one, as autobrr and friends send both.
fn parse_request_body(content_type: Option<&str>, body: &str) -> Value {
    if content_type.is_some_and(|ct| ct.contains("application/x-www-form-urlencoded")) {
        let mut map = serde_json::Map::new();
        for (key, value) in form_urlencoded::parse(body.as_bytes()) {
            map.insert(key.into_owned(), Value::String(value.into_owned()));
        }
        return Value::Object(map);
    }
    serde_json::from_str(body).unwrap_or(Value::Null)
}

/// Coerces the string-typed values a form-encoded body produces.
fn as_bool(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => Some(s == "true"),
        _ => None,
    }
}

async fn webhook_route(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let data = parse_request_body(content_type, &body);

    let info_hash = data
        .get("infoHash")
        .and_then(Value::as_str)
        .map(|h| h.to_lowercase());
    let path = data.get("path").and_then(Value::as_str).map(str::to_string);

    // Exactly one of infoHash / path, as the original's zod refinement required.
    if info_hash.is_some() == path.is_some() {
        let message =
            "A valid infoHash or an accessible path must be provided (infoHash is recommended)";
        tracing::error!(label = Label::Webhook.as_str(), "{message}");
        return (StatusCode::BAD_REQUEST, message).into_response();
    }

    let mut config = crate::config::runtime::get_runtime_config()
        .as_ref()
        .clone();
    if let Some(value) = as_bool(data.get("includeSingleEpisodes")) {
        config.include_single_episodes = value;
    }
    if let Some(value) = as_bool(data.get("includeNonVideos")) {
        config.include_non_videos = value;
    }
    if as_bool(data.get("ignoreExcludeRecentSearch")).unwrap_or(false) {
        config.exclude_recent_search = Some(1);
    }
    if as_bool(data.get("ignoreExcludeOlder")).unwrap_or(false) {
        config.exclude_older = Some(i64::MAX);
    }
    if as_bool(data.get("ignoreBlockList")).unwrap_or(false) {
        config.block_list = Vec::new();
    }
    let ignore_cross_seeds = as_bool(data.get("ignoreCrossSeeds")).unwrap_or(true);

    // Answer immediately: a search takes far longer than any caller will wait.
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let _ = crate::torrent::index::index_torrents_and_data_dirs(&pool, &config, false).await;
        match crate::pipeline::search_for_local_torrent_by_criteria(
            &pool,
            info_hash.as_deref(),
            path.as_deref(),
            &config,
            ignore_cross_seeds,
        )
        .await
        {
            Some(found) => {
                tracing::info!(label = Label::Webhook.as_str(), "Found {found} torrents")
            }
            None => tracing::warn!(
                label = Label::Webhook.as_str(),
                "No searchee found for the given criteria"
            ),
        }
    });

    StatusCode::NO_CONTENT.into_response()
}

async fn announce_route(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let data = parse_request_body(content_type, &body);

    let field = |name: &str| {
        data.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let (Some(name), Some(guid), Some(link), Some(tracker)) = (
        field("name"),
        field("guid"),
        field("link"),
        field("tracker"),
    ) else {
        let message = "Missing required params: {guid, link, name, tracker}";
        tracing::error!(label = Label::Announce.as_str(), "{message}");
        return (StatusCode::BAD_REQUEST, message).into_response();
    };

    let config = crate::config::runtime::get_runtime_config();
    if !config.use_client_torrents && config.torrent_dir.is_none() && config.data_dirs.is_empty() {
        let message = "Announce requires at least one of useClientTorrents, torrentDir, or dataDirs to be set";
        tracing::error!(label = Label::Announce.as_str(), "{message}");
        return (StatusCode::INTERNAL_SERVER_ERROR, message).into_response();
    }

    tracing::debug!(
        label = Label::Announce.as_str(),
        "Received announce from {tracker}: {name}"
    );

    let mut candidate = crate::torznab::Candidate {
        guid,
        name,
        tracker,
        link,
        size: data.get("size").and_then(Value::as_i64).unwrap_or(0),
        pub_date: None,
        indexer_id: None,
    };

    let outcome = crate::pipeline::check_new_candidate_match(
        &state.pool,
        &mut candidate,
        SearcheeLabel::Announce,
        &config,
    )
    .await;

    // 204 means "understood, nothing to do" — the tracker's client uses it to
    // distinguish a miss from an error.
    let Some(decision) = outcome.decision else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match announce_status(decision, outcome.action_result) {
        Some(status) => StatusCode::from_u16(status)
            .unwrap_or(StatusCode::OK)
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Maps a decision plus action into the status code autobrr expects.
pub fn announce_status(
    decision: Decision,
    action_result: Option<crate::constants::ActionResult>,
) -> Option<u16> {
    use crate::constants::ActionResult;

    let injected = action_result == Some(ActionResult::Injection(InjectionResult::Success));
    let added = injected
        || action_result == Some(ActionResult::Injection(InjectionResult::Failure))
        || action_result == Some(ActionResult::Saved);
    let exists = decision == Decision::InfoHashAlreadyExists
        || action_result == Some(ActionResult::Injection(InjectionResult::AlreadyExists));
    let incomplete =
        action_result == Some(ActionResult::Injection(InjectionResult::TorrentNotComplete));

    if added || exists {
        Some(200)
    } else if incomplete {
        // 202: accepted but not seeding yet, so the caller knows to expect the
        // inject job to finish the work.
        Some(202)
    } else {
        None
    }
}

async fn job_route(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let data = parse_request_body(content_type, &body);

    let Some(name) = data
        .get("name")
        .and_then(Value::as_str)
        .and_then(crate::jobs::JobName::from_str_exact)
    else {
        return (
            StatusCode::BAD_REQUEST,
            "Job name must be one of rss, search, updateIndexerCaps, inject, cleanup",
        )
            .into_response();
    };

    let mut overrides = crate::config::ConfigOverrides::new();
    if as_bool(data.get("ignoreExcludeRecentSearch")).unwrap_or(false) {
        overrides.insert("excludeRecentSearch".into(), json!(1));
    }
    if as_bool(data.get("ignoreExcludeOlder")).unwrap_or(false) {
        overrides.insert("excludeOlder".into(), json!(i64::MAX));
    }

    match crate::jobs::trigger_job(name, overrides).await {
        Ok(()) => {
            let pool = state.pool.clone();
            tokio::spawn(async move { crate::jobs::check_jobs(&pool, false).await });
            (
                StatusCode::OK,
                format!("{}: running ahead of schedule", name.as_str()),
            )
                .into_response()
        }
        Err(message) if message.contains("already running") => {
            (StatusCode::CONFLICT, message).into_response()
        }
        Err(message) => (StatusCode::NOT_FOUND, message).into_response(),
    }
}

// ─── Prowlarr indexer API ───────────────────────────────────────────────────

pub fn indexer_router() -> Router<AppState> {
    Router::new()
        .route("/status", get(indexer_status))
        .route("/", get(indexer_list).post(indexer_create))
        .route(
            "/{id}",
            get(indexer_get).put(indexer_update).delete(indexer_delete),
        )
        .route("/test", post(indexer_test))
        // Prowlarr addresses the collection without a trailing slash.
        .route("/", put(indexer_update_root))
}

async fn indexer_update_root() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "").into_response()
}

async fn indexer_status(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }
    Json(json!({ "version": PROGRAM_VERSION, "appName": PROGRAM_NAME })).into_response()
}

async fn indexer_list(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }
    match crate::indexers::get_all_indexers(&state.pool).await {
        Ok(indexers) => Json(indexers).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn indexer_get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }
    match crate::indexers::get_indexer_by_id(&state.pool, id).await {
        Ok(Some(indexer)) => Json(indexer).into_response(),
        _ => not_found(id),
    }
}

fn not_found(id: i64) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "code": "INDEXER_NOT_FOUND",
            "message": format!("Indexer with ID {id} not found"),
        })),
    )
        .into_response()
}

async fn indexer_create(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }
    match create_indexer(&state.pool, &body).await {
        Ok(indexer) => Json(indexer).into_response(),
        Err(message) => validation_error(message),
    }
}

async fn indexer_update(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }
    // The path is authoritative; a mismatched body id is a client bug.
    if let Some(body_id) = body.get("id").and_then(Value::as_i64)
        && body_id != id
    {
        return validation_error("ID in request body must match URL parameter".into());
    }
    body["id"] = json!(id);
    match update_indexer(&state.pool, &body).await {
        Ok(indexer) => Json(indexer).into_response(),
        Err(_) => not_found(id),
    }
}

async fn indexer_delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }
    match delete_indexer(&state.pool, id).await {
        Ok(indexer) => Json(json!({ "success": true, "indexer": indexer })).into_response(),
        Err(_) => not_found(id),
    }
}

async fn indexer_test(
    State(state): State<AppState>,
    Query(query): Query<ApiKeyQuery>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authorize(&state.pool, &headers, &query).await {
        return (StatusCode::UNAUTHORIZED, UNAUTHORIZED_MESSAGE).into_response();
    }

    let result = match body.get("id").and_then(Value::as_i64) {
        Some(id) => match crate::indexers::get_indexer_by_id(&state.pool, id).await {
            Ok(Some(indexer)) => crate::torznab::fetch_caps(&indexer).await.map(|_| ()),
            _ => return not_found(id),
        },
        None => test_indexer_connection(&body).await,
    };

    // A failed connection test is a successful *request*: the UI renders the
    // reason rather than treating it as a transport error.
    match result {
        Ok(()) => Json(json!({ "ok": true, "message": "Connection successful" })).into_response(),
        Err(message) => {
            tracing::warn!(
                label = Label::Server.as_str(),
                "Connection test failed: {message}"
            );
            Json(json!({ "ok": false, "code": "CONNECTION_FAILED", "message": message }))
                .into_response()
        }
    }
}

fn validation_error(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "code": "VALIDATION_ERROR", "message": message })),
    )
        .into_response()
}

// ─── Indexer service helpers (shared with the tRPC router) ──────────────────

pub async fn create_indexer(
    pool: &SqlitePool,
    input: &Value,
) -> Result<crate::indexers::Indexer, String> {
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| url::Url::parse(u).is_ok())
        .ok_or("A valid url is required")?;
    let apikey = input
        .get("apikey")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
        .ok_or("apikey is required")?;
    let name = input.get("name").and_then(Value::as_str);
    let enabled = input
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let id = sqlx::query("INSERT INTO indexer (name, url, apikey, enabled) VALUES (?, ?, ?, ?)")
        .bind(name)
        .bind(url)
        .bind(apikey)
        .bind(enabled)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?
        .last_insert_rowid();

    let indexer = crate::indexers::get_indexer_by_id(pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Indexer disappeared after creation")?;
    // Populate caps immediately so the indexer is usable without waiting for
    // the daily refresh.
    crate::torznab::update_caps_for_indexer(pool, &indexer).await;

    crate::indexers::get_indexer_by_id(pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Indexer disappeared after creation".to_string())
}

pub async fn update_indexer(
    pool: &SqlitePool,
    input: &Value,
) -> Result<crate::indexers::Indexer, String> {
    let id = input
        .get("id")
        .and_then(Value::as_i64)
        .ok_or("id is required")?;
    let existing = crate::indexers::get_indexer_by_id(pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Indexer with ID {id} not found"))?;

    let name = input
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let url = input.get("url").and_then(Value::as_str).map(str::to_string);
    let apikey = input
        .get("apikey")
        .and_then(Value::as_str)
        .map(str::to_string);
    let enabled = input.get("enabled").and_then(Value::as_bool);

    sqlx::query("UPDATE indexer SET name = ?, url = ?, apikey = ?, enabled = ? WHERE id = ?")
        .bind(name.clone().or(existing.name.clone()))
        .bind(url.clone().unwrap_or_else(|| existing.url.clone()))
        .bind(apikey.clone().unwrap_or_else(|| existing.apikey.clone()))
        .bind(enabled.unwrap_or(existing.enabled))
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    let updated = crate::indexers::get_indexer_by_id(pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Indexer with ID {id} not found"))?;

    // A changed endpoint invalidates the stored caps.
    if url.is_some() || apikey.is_some() || updated.categories.is_none() {
        crate::torznab::update_caps_for_indexer(pool, &updated).await;
    }
    crate::indexers::get_indexer_by_id(pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Indexer with ID {id} not found"))
}

pub async fn delete_indexer(
    pool: &SqlitePool,
    id: i64,
) -> Result<crate::indexers::Indexer, String> {
    let indexer = crate::indexers::get_indexer_by_id(pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Indexer with ID {id} not found"))?;

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for sql in [
        "DELETE FROM timestamp WHERE indexer_id = ?",
        "DELETE FROM rss WHERE indexer_id = ?",
        "DELETE FROM indexer WHERE id = ?",
    ] {
        sqlx::query(sql)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(indexer)
}

/// Folds a disabled indexer's search history into an enabled one.
///
/// Used when a tracker changes URL: without this, every title would look
/// unsearched on the new indexer and get re-queried from scratch.
pub async fn merge_disabled_indexer(
    pool: &SqlitePool,
    source_id: i64,
    target_id: i64,
) -> Result<Value, String> {
    if source_id == target_id {
        return Err("Source and target indexers must be different".into());
    }
    let source = crate::indexers::get_indexer_by_id(pool, source_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Disabled indexer with ID {source_id} not found"))?;
    if source.enabled {
        return Err("Source indexer must be disabled before merging".into());
    }
    let target = crate::indexers::get_indexer_by_id(pool, target_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Target indexer with ID {target_id} not found"))?;
    if !target.enabled {
        return Err("Target indexer must be enabled".into());
    }

    let rows: Vec<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT searchee_id, first_searched, last_searched FROM timestamp WHERE indexer_id = ?",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    let merged_count = rows.len();
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for (searchee_id, first_searched, last_searched) in rows {
        // Keep the earliest first-search and the latest last-search.
        sqlx::query(
            r#"
            INSERT INTO timestamp (searchee_id, indexer_id, first_searched, last_searched)
            VALUES (?, ?, ?, ?)
            ON CONFLICT (searchee_id, indexer_id) DO UPDATE SET
                first_searched = MIN(timestamp.first_searched, excluded.first_searched),
                last_searched  = MAX(timestamp.last_searched,  excluded.last_searched)
            "#,
        )
        .bind(searchee_id)
        .bind(target_id)
        .bind(first_searched)
        .bind(last_searched)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }
    for sql in [
        "DELETE FROM timestamp WHERE indexer_id = ?",
        "DELETE FROM rss WHERE indexer_id = ?",
        "DELETE FROM indexer WHERE id = ?",
    ] {
        sqlx::query(sql)
            .bind(source_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;

    Ok(json!({ "mergedCount": merged_count, "deleted": true }))
}

pub async fn test_indexer_connection(input: &Value) -> Result<(), String> {
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .ok_or("url is required")?;
    let apikey = input
        .get("apikey")
        .and_then(Value::as_str)
        .ok_or("apikey is required")?;

    let probe = crate::indexers::Indexer {
        id: 0,
        name: None,
        url: url.to_string(),
        apikey: apikey.to_string(),
        trackers: None,
        enabled: true,
        status: None,
        retry_after: None,
        search_cap: false,
        tv_search_cap: false,
        movie_search_cap: false,
        music_search_cap: false,
        audio_search_cap: false,
        book_search_cap: false,
        tv_id_caps: None,
        movie_id_caps: None,
        categories: None,
        limits: None,
    };
    crate::torznab::fetch_caps(&probe).await.map(|_| ())
}

pub async fn test_client_connection(input: &Value) -> Result<Value, String> {
    let client_type = input
        .get("client")
        .and_then(Value::as_str)
        .and_then(crate::clients::ClientType::from_str_exact)
        .ok_or("Unknown client type")?;
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .ok_or("url is required")?;
    let readonly = input
        .get("readonly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let client_host = url::Url::parse(url)
        .map_err(|e| e.to_string())?
        .host_str()
        .unwrap_or_default()
        .to_string();

    match client_type {
        crate::clients::ClientType::QBittorrent => {
            let client =
                crate::clients::qbittorrent::QBittorrent::new(url, client_host, 0, readonly)
                    .map_err(|e| e.to_string())?;
            client.login().await.map_err(|e| e.to_string())?;
            // A successful login proves nothing when qBittorrent's local-auth
            // bypass is on, so say so rather than implying the credentials work.
            let message = if client.auth_bypass_enabled().await {
                "Note: Credential validation requires qBittorrent's 'Bypass authentication for local auth' setting to be disabled."
            } else {
                "Successfully connected to qbittorrent."
            };
            Ok(json!({ "success": true, "message": message }))
        }
        crate::clients::ClientType::Transmission => {
            let client =
                crate::clients::transmission::Transmission::new(url, client_host, 0, readonly)
                    .map_err(|e| e.to_string())?;
            crate::clients::TorrentClient::validate_config(&client)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "success": true, "message": "" }))
        }
        crate::clients::ClientType::Deluge => {
            let client = crate::clients::deluge::Deluge::new(url, client_host, 0, readonly)
                .map_err(|e| e.to_string())?;
            client.authenticate().await.map_err(|e| e.to_string())?;
            Ok(json!({ "success": true, "message": "" }))
        }
        crate::clients::ClientType::RTorrent => {
            let client = crate::clients::rtorrent::RTorrent::new(url, client_host, 0, readonly)
                .map_err(|e| e.to_string())?;
            client
                .validate_connection()
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "success": true, "message": "" }))
        }
    }
}

// ─── Stats and searchee listing ─────────────────────────────────────────────

pub async fn stats_overview(pool: &SqlitePool) -> Result<Value, String> {
    let scalar = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .unwrap_or(0)
    };

    let total_searchees = scalar("SELECT COUNT(*) FROM searchee").await;
    let total_indexers = scalar("SELECT COUNT(*) FROM indexer").await;
    let healthy_indexers = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM indexer WHERE enabled = 1 AND search_cap = 1
         AND (status IS NULL OR status = 'OK' OR retry_after < ?)",
    )
    .bind(crate::utils::now_ms())
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let query_indexer_count = scalar("SELECT COUNT(*) FROM timestamp").await;

    let decision_breakdown: Vec<(Option<String>, i64)> =
        sqlx::query_as("SELECT decision, COUNT(*) FROM decision GROUP BY decision")
            .fetch_all(pool)
            .await
            .unwrap_or_default();

    let recent_matches = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM decision
         WHERE decision IN ('MATCH','MATCH_SIZE_ONLY','MATCH_PARTIAL') AND last_seen > ?",
    )
    .bind(crate::utils::n_ms_ago(24 * 60 * 60 * 1000))
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let aggregates: (i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COUNT(DISTINCT info_hash),
            COUNT(DISTINCT CASE WHEN decision IN ('MATCH','MATCH_SIZE_ONLY','MATCH_PARTIAL') THEN info_hash END),
            COUNT(DISTINCT CASE WHEN decision IN ('MATCH','MATCH_SIZE_ONLY','MATCH_PARTIAL','SAME_INFO_HASH','INFO_HASH_ALREADY_EXISTS') THEN info_hash END)
        FROM decision WHERE info_hash IS NOT NULL
        "#,
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0, 0, 0));
    let (snatch_count, match_count, match_count_with_info_hash) = aggregates;

    // Distinct *queries*, not searchees: several titles can collapse to one.
    let names: Vec<Option<String>> = sqlx::query_scalar("SELECT name FROM searchee")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    let query_count = names
        .into_iter()
        .flatten()
        .map(|name| crate::torznab::estimate_search_string(&name))
        .collect::<std::collections::HashSet<_>>()
        .len() as i64;

    let ratio = |numerator: i64, denominator: i64| -> f64 {
        if denominator > 0 {
            (numerator as f64 / denominator as f64 * 1000.0).round() / 1000.0
        } else {
            0.0
        }
    };
    let wasted_snatch_count = (snatch_count - match_count_with_info_hash).max(0);
    let unhealthy_indexers = (total_indexers - healthy_indexers).max(0);

    Ok(json!({
        "totalSearchees": total_searchees,
        "totalMatches": match_count,
        "totalIndexers": total_indexers,
        "healthyIndexers": healthy_indexers,
        "recentMatches": recent_matches,
        "matchRate": if total_searchees > 0 {
            (match_count as f64 / total_searchees as f64 * 100.0).round() / 100.0
        } else { 0.0 },
        "matchesPerSnatch": ratio(match_count_with_info_hash, snatch_count),
        "matchesPerQuery": ratio(match_count, query_count),
        "matchesPerQueryIndexer": ratio(match_count, query_indexer_count),
        "snatchCount": snatch_count,
        "queryCount": query_count,
        "queryIndexerCount": query_indexer_count,
        "wastedSnatchCount": wasted_snatch_count,
        "wastedSnatchRate": ratio(wasted_snatch_count, snatch_count),
        "unhealthyIndexers": unhealthy_indexers,
        "allIndexersHealthy": unhealthy_indexers == 0,
        "decisionBreakdown": decision_breakdown
            .into_iter()
            .map(|(decision, count)| json!({
                "decision": decision.unwrap_or_default(),
                "count": count
            }))
            .collect::<Vec<_>>(),
    }))
}

pub async fn searchees_list(pool: &SqlitePool, input: Option<&Value>) -> Result<Value, String> {
    let search = input
        .and_then(|v| v.get("search"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let limit = input
        .and_then(|v| v.get("limit"))
        .and_then(Value::as_i64)
        .unwrap_or(50)
        .clamp(1, 200);
    let offset = input
        .and_then(|v| v.get("offset"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);

    let like = search.as_ref().map(|s| format!("%{s}%"));
    let total: i64 = match &like {
        Some(like) => {
            sqlx::query_scalar("SELECT COUNT(*) FROM searchee WHERE name LIKE ?")
                .bind(like)
                .fetch_one(pool)
                .await
        }
        None => {
            sqlx::query_scalar("SELECT COUNT(*) FROM searchee")
                .fetch_one(pool)
                .await
        }
    }
    .map_err(|e| e.to_string())?;

    let sql = format!(
        r#"
        SELECT searchee.id, searchee.name,
               MIN(timestamp.first_searched), MAX(timestamp.last_searched),
               COUNT(DISTINCT timestamp.indexer_id)
        FROM searchee
        LEFT JOIN timestamp ON searchee.id = timestamp.searchee_id
        {}
        GROUP BY searchee.id, searchee.name
        ORDER BY MAX(timestamp.last_searched) DESC,
                 MIN(timestamp.first_searched) DESC,
                 searchee.name ASC
        LIMIT ? OFFSET ?
        "#,
        if like.is_some() {
            "WHERE searchee.name LIKE ?"
        } else {
            ""
        }
    );
    let mut query = sqlx::query_as::<_, (i64, Option<String>, Option<i64>, Option<i64>, i64)>(&sql);
    if let Some(like) = &like {
        query = query.bind(like);
    }
    let rows = query
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;

    let all_indexers = crate::indexers::get_all_indexers(pool)
        .await
        .map_err(|e| e.to_string())?;
    let enabled_indexers = crate::indexers::get_enabled_indexers(pool)
        .await
        .map_err(|e| e.to_string())?;

    let iso = |ms: Option<i64>| {
        ms.and_then(chrono::DateTime::from_timestamp_millis)
            .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    };

    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, name, first, last, indexer_count)| {
            json!({
                "id": id,
                "name": name.unwrap_or_else(|| "(unknown)".into()),
                "indexerCount": indexer_count,
                "firstSearchedAt": iso(first),
                "lastSearchedAt": iso(last),
                "label": Value::Null,
                "source": Value::Null,
                "length": Value::Null,
                "clientHost": Value::Null,
            })
        })
        .collect();

    Ok(json!({
        "total": total,
        "pagination": { "limit": limit, "offset": offset },
        "indexerTotals": {
            "configured": all_indexers.len(),
            "enabled": enabled_indexers.len(),
        },
        "items": items,
    }))
}

// ─── Dev login ──────────────────────────────────────────────────────────────

pub fn dev_login_router() -> Router<AppState> {
    Router::new().route("/dev-login/{session_id}", get(dev_login))
}

/// Exchanges a session id created by `crust-seed dev-login` for a cookie.
///
/// Gated on `CRUST_SEED_DEV_LOGIN=true`, because it authenticates on the
/// strength of a URL alone.
async fn dev_login(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if std::env::var("CRUST_SEED_DEV_LOGIN").as_deref() != Ok("true") {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }
    let Ok(Some(_)) = validate_session(&state.pool, &session_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Dev login session not found" })),
        )
            .into_response();
    };

    // Only same-origin absolute paths, so the URL cannot be used as an open
    // redirect.
    let redirect_to = query
        .as_deref()
        .and_then(|q| {
            form_urlencoded::parse(q.as_bytes())
                .find(|(key, _)| key == "redirectTo")
                .map(|(_, value)| value.into_owned())
        })
        .filter(|path| path.starts_with('/') && !path.starts_with("//"))
        .unwrap_or_else(|| "/".to_string());

    let mut response =
        Redirect::temporary(&format!("{}{redirect_to}", state.base_path)).into_response();
    if let Ok(value) = session_cookie(&session_id).parse() {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::ActionResult;
    use crate::db::test_pool;

    #[test]
    fn form_encoded_bodies_parse_like_json_ones() {
        let json_body = parse_request_body(Some("application/json"), r#"{"name":"x","size":"5"}"#);
        let form_body =
            parse_request_body(Some("application/x-www-form-urlencoded"), "name=x&size=5");
        assert_eq!(json_body["name"], json!("x"));
        assert_eq!(form_body["name"], json!("x"));
        // A form body's values are always strings.
        assert_eq!(form_body["size"], json!("5"));
    }

    /// Form bodies stringify booleans; both spellings must work.
    #[test]
    fn boolean_fields_accept_both_forms() {
        assert_eq!(as_bool(Some(&json!(true))), Some(true));
        assert_eq!(as_bool(Some(&json!("true"))), Some(true));
        assert_eq!(as_bool(Some(&json!("false"))), Some(false));
        assert_eq!(as_bool(Some(&json!(5))), None);
        assert_eq!(as_bool(None), None);
    }

    #[test]
    fn the_session_cookie_is_http_only_and_scoped_to_the_root() {
        let cookie = session_cookie("abc");
        assert!(cookie.starts_with("cross-seed-session=abc;"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cleared_session_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn the_session_id_is_read_out_of_the_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "other=1; cross-seed-session=abc123; another=2"
                .parse()
                .unwrap(),
        );
        assert_eq!(session_id_from(&headers).as_deref(), Some("abc123"));

        let empty = HeaderMap::new();
        assert_eq!(session_id_from(&empty), None);
    }

    /// The announce status codes autobrr keys off.
    #[test]
    fn announce_status_codes_follow_the_outcome() {
        assert_eq!(
            announce_status(
                Decision::Match,
                Some(ActionResult::Injection(InjectionResult::Success))
            ),
            Some(200)
        );
        assert_eq!(
            announce_status(Decision::Match, Some(ActionResult::Saved)),
            Some(200)
        );
        assert_eq!(
            announce_status(Decision::InfoHashAlreadyExists, None),
            Some(200)
        );
        assert_eq!(
            announce_status(
                Decision::MatchPartial,
                Some(ActionResult::Injection(InjectionResult::TorrentNotComplete))
            ),
            Some(202)
        );
        // Nothing happened: the caller gets 204 from the route.
        assert_eq!(announce_status(Decision::SizeMismatch, None), None);
    }

    #[tokio::test]
    async fn indexers_are_created_read_and_deleted() {
        let pool = test_pool().await;
        // No network in tests, so the caps refresh fails and leaves them NULL.
        let created = create_indexer(
            &pool,
            &json!({ "url": "https://a.example/api", "apikey": "k", "name": "A" }),
        )
        .await
        .unwrap();
        assert_eq!(created.url, "https://a.example/api");
        assert!(created.enabled);

        let updated = update_indexer(
            &pool,
            &json!({ "id": created.id, "enabled": false, "name": "Renamed" }),
        )
        .await
        .unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.name.as_deref(), Some("Renamed"));

        delete_indexer(&pool, created.id).await.unwrap();
        assert!(
            crate::indexers::get_indexer_by_id(&pool, created.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn creating_an_indexer_requires_a_valid_url_and_key() {
        let pool = test_pool().await;
        assert!(
            create_indexer(&pool, &json!({ "apikey": "k" }))
                .await
                .is_err()
        );
        assert!(
            create_indexer(&pool, &json!({ "url": "not a url", "apikey": "k" }))
                .await
                .is_err()
        );
        assert!(
            create_indexer(&pool, &json!({ "url": "https://a/api" }))
                .await
                .is_err()
        );
    }

    /// Merging preserves the earliest first-search and the latest last-search,
    /// so a tracker that changed URL does not look unsearched.
    #[tokio::test]
    async fn merging_folds_search_history_into_the_target() {
        let pool = test_pool().await;
        let source = create_indexer(
            &pool,
            &json!({ "url": "https://old.example/api", "apikey": "k", "enabled": false }),
        )
        .await
        .unwrap();
        let target = create_indexer(
            &pool,
            &json!({ "url": "https://new.example/api", "apikey": "k", "enabled": true }),
        )
        .await
        .unwrap();

        let searchee_id = crate::db::upsert_searchee(&pool, "Some.Show")
            .await
            .unwrap();
        for (indexer_id, first, last) in [(source.id, 100, 200), (target.id, 150, 150)] {
            sqlx::query(
                "INSERT INTO timestamp (searchee_id, indexer_id, first_searched, last_searched) VALUES (?, ?, ?, ?)",
            )
            .bind(searchee_id)
            .bind(indexer_id)
            .bind(first)
            .bind(last)
            .execute(&pool)
            .await
            .unwrap();
        }

        let result = merge_disabled_indexer(&pool, source.id, target.id)
            .await
            .unwrap();
        assert_eq!(result["mergedCount"], json!(1));

        let (first, last): (i64, i64) = sqlx::query_as(
            "SELECT first_searched, last_searched FROM timestamp WHERE indexer_id = ?",
        )
        .bind(target.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(first, 100);
        assert_eq!(last, 200);
    }

    #[tokio::test]
    async fn merging_rejects_the_wrong_enabled_states() {
        let pool = test_pool().await;
        let enabled = create_indexer(
            &pool,
            &json!({ "url": "https://a.example/api", "apikey": "k", "enabled": true }),
        )
        .await
        .unwrap();
        let other = create_indexer(
            &pool,
            &json!({ "url": "https://b.example/api", "apikey": "k", "enabled": true }),
        )
        .await
        .unwrap();

        assert!(
            merge_disabled_indexer(&pool, enabled.id, enabled.id)
                .await
                .is_err()
        );
        // Source must be disabled.
        assert!(
            merge_disabled_indexer(&pool, enabled.id, other.id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_stats_overview_answers_on_an_empty_database() {
        let pool = test_pool().await;
        let stats = stats_overview(&pool).await.unwrap();
        assert_eq!(stats["totalSearchees"], json!(0));
        assert_eq!(stats["matchRate"], json!(0.0));
        assert_eq!(stats["allIndexersHealthy"], json!(true));
    }

    #[tokio::test]
    async fn the_searchee_list_paginates() {
        let pool = test_pool().await;
        for name in ["A.Show", "B.Show", "C.Show"] {
            crate::db::upsert_searchee(&pool, name).await.unwrap();
        }
        let page = searchees_list(&pool, Some(&json!({ "limit": 2, "offset": 0 })))
            .await
            .unwrap();
        assert_eq!(page["total"], json!(3));
        assert_eq!(page["items"].as_array().unwrap().len(), 2);

        let filtered = searchees_list(&pool, Some(&json!({ "search": "B." })))
            .await
            .unwrap();
        assert_eq!(filtered["total"], json!(1));
    }
}
