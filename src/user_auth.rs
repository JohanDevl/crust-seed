//! Web-UI users and sessions.
//!
//! Ported from `userAuth.ts`, `auth.ts` and `sessionCookies.ts`.
//!
//! Passwords are bcrypt-hashed; sessions are opaque 32-byte random ids stored
//! in the `session` table and carried in an httpOnly cookie. The API key is a
//! separate credential used by the machine-to-machine routes.

use base64::Engine;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::db::{SettingsRow, UserRow};
use crate::errors::CrustSeedError;
use crate::logger::Label;
use crate::utils::now_ms;

/// 30 days, matching the session cookie's max-age.
pub const SESSION_EXPIRY_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const SESSION_COOKIE_NAME: &str = "cross-seed-session";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUser {
    pub id: i64,
    pub username: String,
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // OS entropy, not a PRNG: session ids and API keys are security tokens.
    getrandom::fill(&mut buf).expect("OS random number generator");
    hex::encode(buf)
}

pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> Result<SessionUser, CrustSeedError> {
    let hashed = bcrypt::hash(password, bcrypt::DEFAULT_COST)
        .map_err(|e| CrustSeedError::new(format!("Could not hash password: {e}")))?;
    let id = sqlx::query("INSERT INTO user (username, password) VALUES (?, ?)")
        .bind(username)
        .bind(&hashed)
        .execute(pool)
        .await
        .map_err(|e| CrustSeedError::new(format!("Could not create user: {e}")))?
        .last_insert_rowid();

    tracing::info!(label = Label::Auth.as_str(), "Created user: {username}");
    Ok(SessionUser {
        id,
        username: username.to_string(),
    })
}

pub async fn find_user_by_username(
    pool: &SqlitePool,
    username: &str,
) -> sqlx::Result<Option<UserRow>> {
    sqlx::query_as("SELECT id, username, password FROM user WHERE username = ?")
        .bind(username)
        .fetch_optional(pool)
        .await
}

pub async fn validate_user_credentials(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> sqlx::Result<Option<SessionUser>> {
    let Some(user) = find_user_by_username(pool, username).await? else {
        return Ok(None);
    };
    let valid = bcrypt::verify(password, &user.password).unwrap_or(false);
    Ok(valid.then_some(SessionUser {
        id: user.id,
        username: user.username,
    }))
}

pub async fn create_session(pool: &SqlitePool, user_id: i64) -> sqlx::Result<String> {
    let session_id = random_hex(32);
    let now = now_ms();
    sqlx::query("INSERT INTO session (id, user_id, expires_at, created_at) VALUES (?, ?, ?, ?)")
        .bind(&session_id)
        .bind(user_id)
        .bind(now + SESSION_EXPIRY_MS)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(session_id)
}

/// Resolves a session id to its user, rejecting expired sessions.
pub async fn validate_session(
    pool: &SqlitePool,
    session_id: &str,
) -> sqlx::Result<Option<SessionUser>> {
    let row: Option<(i64, String)> = sqlx::query_as(
        r#"
        SELECT user.id, user.username
        FROM session
        JOIN user ON user.id = session.user_id
        WHERE session.id = ? AND session.expires_at > ?
        "#,
    )
    .bind(session_id)
    .bind(now_ms())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, username)| SessionUser { id, username }))
}

pub async fn remove_session(pool: &SqlitePool, session_id: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM session WHERE id = ?")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn has_users(pool: &SqlitePool) -> sqlx::Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user")
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

pub async fn create_initial_user_if_needed(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> Result<Option<SessionUser>, CrustSeedError> {
    if has_users(pool).await.unwrap_or(true) {
        tracing::info!(
            label = Label::Auth.as_str(),
            "Initial user already exists, skipping creation"
        );
        return Ok(None);
    }
    create_user(pool, username, password).await.map(Some)
}

pub async fn reset_users(pool: &SqlitePool) -> sqlx::Result<String> {
    let sessions = sqlx::query("DELETE FROM session")
        .execute(pool)
        .await?
        .rows_affected();
    let users = sqlx::query("DELETE FROM user")
        .execute(pool)
        .await?
        .rows_affected();
    Ok(format!(
        "Deleted {users} {} and {sessions} {}.",
        if users == 1 { "user" } else { "users" },
        if sessions == 1 { "session" } else { "sessions" }
    ))
}

// ─── API key ────────────────────────────────────────────────────────────────

/// Generates a 48-character hex key (24 random bytes), the length the settings
/// validator enforces.
pub fn generate_api_key() -> String {
    random_hex(24)
}

/// Reads the API key, generating one on first use.
///
/// Migration 17 moved the key from `settings.apikey` into
/// `settings_json.apiKey`; the column is still consulted as a fallback for a
/// database restored mid-migration.
pub async fn get_api_key(pool: &SqlitePool) -> Result<String, CrustSeedError> {
    let row: Option<SettingsRow> = sqlx::query_as("SELECT * FROM settings")
        .fetch_optional(pool)
        .await
        .map_err(|e| CrustSeedError::new(e.to_string()))?;

    if let Some(row) = &row {
        if let Some(json) = &row.settings_json
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(json)
            && let Some(api_key) = value.get("apiKey").and_then(|v| v.as_str())
            && !api_key.is_empty()
        {
            return Ok(api_key.to_string());
        }
        if let Some(api_key) = row.apikey.as_deref().filter(|k| !k.is_empty()) {
            return Ok(api_key.to_string());
        }
    }

    if let Some(api_key) = crate::config::runtime::get_runtime_config().api_key.clone() {
        return Ok(api_key);
    }
    reset_api_key(pool).await
}

pub async fn set_api_key(pool: &SqlitePool, api_key: &str) -> Result<String, CrustSeedError> {
    if api_key.chars().count() < 24 {
        return Err(CrustSeedError::new(
            "API key must be at least 24 characters",
        ));
    }
    let mut overrides = crate::config::db_config::get_db_config(pool)
        .await
        .unwrap_or_default();
    overrides.insert("apiKey".into(), serde_json::json!(api_key));
    crate::config::db_config::update_db_config(pool, &overrides).await?;
    Ok(api_key.to_string())
}

pub async fn reset_api_key(pool: &SqlitePool) -> Result<String, CrustSeedError> {
    set_api_key(pool, &generate_api_key()).await
}

pub async fn check_api_key(pool: &SqlitePool, key_to_check: &str) -> bool {
    match get_api_key(pool).await {
        Ok(api_key) => !key_to_check.is_empty() && api_key == key_to_check,
        Err(_) => false,
    }
}

/// A development login URL, printed by `crust-seed dev-login`.
///
/// Only usable when the daemon runs with `CRUST_SEED_DEV_LOGIN=true`.
pub async fn create_dev_login(
    pool: &SqlitePool,
    username: &str,
    origin: &str,
    redirect_to: &str,
) -> Result<String, CrustSeedError> {
    let user = match find_user_by_username(pool, username).await {
        Ok(Some(user)) => SessionUser {
            id: user.id,
            username: user.username,
        },
        _ => create_user(pool, username, &random_hex(16)).await?,
    };
    let session_id = create_session(pool, user.id)
        .await
        .map_err(|e| CrustSeedError::new(e.to_string()))?;
    Ok(format!(
        "{}/api/dev-login/{session_id}?redirectTo={}",
        origin.trim_end_matches('/'),
        percent_encoding::utf8_percent_encode(redirect_to, percent_encoding::NON_ALPHANUMERIC)
    ))
}

/// HTTP Basic credentials, for the clients that use them.
pub fn basic_auth_header(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    #[tokio::test]
    async fn credentials_round_trip_through_bcrypt() {
        let pool = test_pool().await;
        create_user(&pool, "alice", "correct horse").await.unwrap();

        assert!(
            validate_user_credentials(&pool, "alice", "correct horse")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            validate_user_credentials(&pool, "alice", "wrong")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            validate_user_credentials(&pool, "bob", "correct horse")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_password_is_never_stored_in_the_clear() {
        let pool = test_pool().await;
        create_user(&pool, "alice", "secret").await.unwrap();
        let stored: String =
            sqlx::query_scalar("SELECT password FROM user WHERE username = 'alice'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stored.starts_with("$2"), "expected a bcrypt hash");
        assert!(!stored.contains("secret"));
    }

    #[tokio::test]
    async fn sessions_resolve_to_their_user_and_expire() {
        let pool = test_pool().await;
        let user = create_user(&pool, "alice", "pw").await.unwrap();
        let session_id = create_session(&pool, user.id).await.unwrap();

        let resolved = validate_session(&pool, &session_id).await.unwrap().unwrap();
        assert_eq!(resolved.username, "alice");

        sqlx::query("UPDATE session SET expires_at = ? WHERE id = ?")
            .bind(now_ms() - 1)
            .bind(&session_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            validate_session(&pool, &session_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn session_ids_are_unpredictable_and_removable() {
        let pool = test_pool().await;
        let user = create_user(&pool, "alice", "pw").await.unwrap();
        let a = create_session(&pool, user.id).await.unwrap();
        let b = create_session(&pool, user.id).await.unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);

        remove_session(&pool, &a).await.unwrap();
        assert!(validate_session(&pool, &a).await.unwrap().is_none());
        assert!(validate_session(&pool, &b).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_initial_user_is_only_created_once() {
        let pool = test_pool().await;
        assert!(!has_users(&pool).await.unwrap());
        assert!(
            create_initial_user_if_needed(&pool, "alice", "pw")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            create_initial_user_if_needed(&pool, "mallory", "pw")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn resetting_users_clears_sessions_too() {
        let pool = test_pool().await;
        let user = create_user(&pool, "alice", "pw").await.unwrap();
        create_session(&pool, user.id).await.unwrap();
        let message = reset_users(&pool).await.unwrap();
        assert_eq!(message, "Deleted 1 user and 1 session.");
        assert!(!has_users(&pool).await.unwrap());
    }

    #[tokio::test]
    async fn the_api_key_is_generated_once_and_then_stable() {
        let pool = test_pool().await;
        let first = get_api_key(&pool).await.unwrap();
        assert_eq!(first.len(), 48);
        assert_eq!(get_api_key(&pool).await.unwrap(), first);

        assert!(check_api_key(&pool, &first).await);
        assert!(!check_api_key(&pool, "wrong").await);
        // An empty key must never authorise.
        assert!(!check_api_key(&pool, "").await);
    }

    #[tokio::test]
    async fn short_api_keys_are_rejected() {
        let pool = test_pool().await;
        assert!(set_api_key(&pool, "tooshort").await.is_err());
    }

    /// Migration 17 moved the key into settings_json; a database restored
    /// mid-migration still has it in the column.
    #[tokio::test]
    async fn the_legacy_apikey_column_is_still_read() {
        let pool = test_pool().await;
        sqlx::query("UPDATE settings SET apikey = 'legacy-key-of-sufficient-length'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            get_api_key(&pool).await.unwrap(),
            "legacy-key-of-sufficient-length"
        );
    }
}
