//! The shared outbound HTTP client.
//!
//! cross-seed used the global `fetch`; here one `reqwest::Client` is built once
//! and reused so connection pooling actually happens across indexer requests.
//! `User-Agent` is set globally because every outbound call in the original
//! passed it explicitly.

use std::sync::LazyLock;
use std::time::Duration;

use crate::constants::user_agent;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(user_agent())
        // No global timeout: callers pass their own, because the config
        // exposes separate searchTimeout and snatchTimeout values.
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("build HTTP client")
});

pub fn client() -> &'static reqwest::Client {
    &CLIENT
}

/// A client that keeps a cookie jar — the torrent clients that authenticate
/// with a session cookie (qBittorrent, Deluge) need one that is not shared with
/// indexer traffic.
pub fn cookie_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent())
        .cookie_store(true)
        .build()
}

/// Truncates a response body for the "check verbose logs" debug lines.
pub fn body_sample(text: &str) -> String {
    let sample: String = text.chars().take(1000).collect();
    if sample.is_empty() {
        String::new()
    } else {
        format!("first 1000 characters: {sample}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_sample_truncates_and_handles_empty() {
        assert_eq!(body_sample(""), "");
        let long = "x".repeat(2000);
        let sample = body_sample(&long);
        assert!(sample.starts_with("first 1000 characters: "));
        assert_eq!(
            sample.chars().count(),
            "first 1000 characters: ".len() + 1000
        );
    }
}
