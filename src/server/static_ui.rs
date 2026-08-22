//! Serving the embedded web UI.
//!
//! Ported from `routes/staticFrontendPlugin.ts`.
//!
//! The SPA is built with Vite's `base` set to the literal
//! `/__CROSS_SEED_BASE_PATH__/`, which the server rewrites at request time.
//! That is what lets one build work at `/` and behind any reverse-proxy base
//! path — but it also means serving the files verbatim yields a UI whose asset
//! URLs all 404.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

use super::AppState;

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/web/webui/dist"]
struct WebUi;

const SENTINEL_BASE_PATH: &str = "/__CROSS_SEED_BASE_PATH__";

/// Extensions whose contents are text and may therefore contain the sentinel.
const TEXT_EXTENSIONS: &[&str] = &[".html", ".css", ".js", ".mjs", ".json", ".map"];

fn is_text(path: &str) -> bool {
    TEXT_EXTENSIONS
        .iter()
        .any(|ext| path.to_lowercase().ends_with(ext))
}

fn inject_base_path(contents: &[u8], base_path: &str) -> Vec<u8> {
    match std::str::from_utf8(contents) {
        Ok(text) => text.replace(SENTINEL_BASE_PATH, base_path).into_bytes(),
        Err(_) => contents.to_vec(),
    }
}

pub async fn serve_static(State(state): State<AppState>, headers: HeaderMap, uri: Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    let requested = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };

    let (path, asset) = match WebUi::get(requested) {
        Some(asset) => (requested.to_string(), Some(asset)),
        None => {
            // A request that wants HTML is a client-side route; anything else
            // (a missing asset) is a genuine 404.
            let wants_html = headers
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|accept| accept.contains("text/html"));
            if !wants_html {
                return (StatusCode::NOT_FOUND, "Not Found").into_response();
            }
            ("index.html".to_string(), WebUi::get("index.html"))
        }
    };

    let Some(asset) = asset else {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    };

    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    let body = if is_text(&path) {
        inject_base_path(asset.data.as_ref(), &state.base_path)
    } else {
        asset.data.into_owned()
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .body(Body::from(body))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sentinel_is_replaced_with_the_configured_base_path() {
        let html = br#"<script src="/__CROSS_SEED_BASE_PATH__/assets/index.js"></script>"#;
        let rendered = inject_base_path(html, "/cross-seed");
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            r#"<script src="/cross-seed/assets/index.js"></script>"#
        );
    }

    /// Serving at the root replaces the sentinel with nothing, leaving
    /// absolute `/assets/...` URLs.
    #[test]
    fn an_empty_base_path_yields_root_relative_urls() {
        let html = br#"<link href="/__CROSS_SEED_BASE_PATH__/assets/a.css">"#;
        let rendered = inject_base_path(html, "");
        assert_eq!(
            String::from_utf8(rendered).unwrap(),
            r#"<link href="/assets/a.css">"#
        );
    }

    #[test]
    fn binary_assets_are_passed_through_untouched() {
        let png = [0x89u8, 0x50, 0x4e, 0x47, 0xff, 0xfe];
        assert_eq!(inject_base_path(&png, "/base"), png.to_vec());
    }

    #[test]
    fn only_text_extensions_are_rewritten() {
        assert!(is_text("index.html"));
        assert!(is_text("assets/index-abc123.js"));
        assert!(is_text("assets/index.css"));
        assert!(!is_text("favicon.ico"));
        assert!(!is_text("assets/logo.png"));
    }

    /// The build always produces an index.html — the placeholder from build.rs
    /// when the SPA was not built, the real one otherwise.
    #[test]
    fn the_bundle_always_contains_an_index() {
        assert!(WebUi::get("index.html").is_some());
    }
}
