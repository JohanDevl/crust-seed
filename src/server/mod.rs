//! The HTTP server.
//!
//! Ported from `server.ts` and `routes/`. fastify becomes axum; the route
//! surface is unchanged because the vendored web UI and third-party
//! integrations (autobrr, Prowlarr) depend on it.

pub mod routers;
pub mod routes;
pub mod static_ui;
pub mod trpc;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use sqlx::SqlitePool;

use crate::logger::Label;

/// Shared state every handler needs.
#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub base_path: Arc<String>,
}

/// Builds the whole application router under `base_path`.
pub fn create_app(pool: SqlitePool, base_path: &str) -> Router {
    let state = AppState {
        pool,
        base_path: Arc::new(base_path.to_string()),
    };

    let api = Router::new()
        .merge(routes::trpc_router())
        .nest("/indexer/v1", routes::indexer_router())
        .merge(routes::base_api_router())
        .merge(routes::dev_login_router());

    let app = Router::new()
        .nest("/api", api)
        // The SPA handles its own routing, so anything not matched above is
        // served the static bundle (falling back to index.html).
        .fallback(static_ui::serve_static)
        .with_state(state);

    if base_path.is_empty() {
        app
    } else {
        // Behind a base path everything is nested, and bare requests are
        // redirected in so a user typing the host still lands on the UI.
        let base = base_path.trim_end_matches('/').to_string();
        let redirect_target = format!("{base}/");
        Router::new().nest(&base, app).fallback(move || {
            let redirect_target = redirect_target.clone();
            async move { axum::response::Redirect::temporary(&redirect_target) }
        })
    }
}

/// Starts the daemon's HTTP listener.
pub async fn serve(
    pool: SqlitePool,
    port: u16,
    host: &str,
    base_path: &str,
) -> Result<(), crate::errors::CrustSeedError> {
    let app = create_app(pool, base_path);
    let addr: SocketAddr = format!("{host}:{port}").parse().map_err(|e| {
        crate::errors::CrustSeedError::new(format!("Invalid host/port {host}:{port}: {e}"))
    })?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| crate::errors::CrustSeedError::new(format!("Failed to start server: {e}")))?;
    tracing::info!(
        label = Label::Server.as_str(),
        "Server is running on port {port}, ^C to stop."
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| crate::errors::CrustSeedError::new(format!("Server error: {e}")))?;

    tracing::info!(label = Label::Server.as_str(), "Server stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
