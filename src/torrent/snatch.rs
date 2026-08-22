//! Downloading a candidate's `.torrent` file.
//!
//! Ported from the snatch half of `torrent.ts`.
//!
//! The retry policy is the interesting part: a tracker that keeps failing gets
//! backed off *across* candidates, not just within one snatch, so a tracker
//! outage does not turn into hundreds of doomed requests. That state lives in
//! [`snatch_history`], keyed by both the download link and the tracker name.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::config::runtime::get_runtime_config;
use crate::http::client;
use crate::searchee::SearcheeLabel;
use crate::torznab::Candidate;
use crate::utils::{now_ms, wait};

use super::Metafile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnatchError {
    Aborted,
    RateLimited,
    MagnetLink,
    InvalidContents,
    UnknownError,
}

impl SnatchError {
    pub fn as_str(self) -> &'static str {
        match self {
            SnatchError::Aborted => "ABORTED",
            SnatchError::RateLimited => "RATE_LIMITED",
            SnatchError::MagnetLink => "MAGNET_LINK",
            SnatchError::InvalidContents => "INVALID_CONTENTS",
            SnatchError::UnknownError => "UNKNOWN_ERROR",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FailureRecord {
    pub initial_failure_at: i64,
    pub num_failures: u32,
}

/// Failures keyed by download link *and* by tracker name. The cleanup job
/// prunes entries older than a day.
pub static SNATCH_HISTORY: LazyLock<Mutex<HashMap<String, FailureRecord>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn snatch_history() -> &'static Mutex<HashMap<String, FailureRecord>> {
    &SNATCH_HISTORY
}

fn record_failure(key: &str) -> FailureRecord {
    let mut history = SNATCH_HISTORY.lock().expect("snatch history lock");
    let entry = history.entry(key.to_string()).or_insert(FailureRecord {
        initial_failure_at: now_ms(),
        num_failures: 0,
    });
    entry.num_failures += 1;
    *entry
}

fn clear_failure(key: &str) {
    if let Ok(mut history) = SNATCH_HISTORY.lock() {
        history.remove(key);
    }
}

struct SnatchFailure {
    error: SnatchError,
    retry_after_ms: Option<i64>,
    extra: Option<String>,
}

async fn snatch_once(candidate: &Candidate) -> Result<Metafile, SnatchFailure> {
    let config = get_runtime_config();
    let url = &candidate.link;

    // A magnet URI has no torrent file to download; the original detected this
    // by the fetch error, but the scheme can simply be checked up front.
    if url.starts_with("magnet:") {
        return Err(SnatchFailure {
            error: SnatchError::MagnetLink,
            retry_after_ms: None,
            extra: None,
        });
    }

    let mut request = client().get(url);
    if let Some(timeout) = config.snatch_timeout {
        request = request.timeout(std::time::Duration::from_millis(timeout.max(0) as u64));
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(e) if e.is_timeout() => {
            tracing::trace!("{}: {url}", candidate.name);
            return Err(SnatchFailure {
                error: SnatchError::Aborted,
                retry_after_ms: None,
                extra: Some("snatch timed out".into()),
            });
        }
        Err(e) => {
            tracing::trace!("{}: {url} - {e}", candidate.name);
            return Err(SnatchFailure {
                error: SnatchError::UnknownError,
                retry_after_ms: None,
                extra: Some("failed to access".into()),
            });
        }
    };

    let status = response.status();
    let retry_after_ms = response
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|seconds| seconds * 1000);
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if status.as_u16() == 429 {
        return Err(SnatchFailure {
            error: SnatchError::RateLimited,
            retry_after_ms,
            extra: None,
        });
    }
    if !status.is_success() {
        return Err(SnatchFailure {
            error: SnatchError::UnknownError,
            retry_after_ms,
            extra: Some(format!(
                "error downloading torrent - {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            )),
        });
    }
    // Some trackers answer a download link with the RSS feed when the passkey
    // is wrong; that is a configuration problem, not a transient one.
    if content_type == "application/rss+xml" {
        return Err(SnatchFailure {
            error: SnatchError::InvalidContents,
            retry_after_ms,
            extra: None,
        });
    }

    let bytes = response.bytes().await.map_err(|_| SnatchFailure {
        error: SnatchError::InvalidContents,
        retry_after_ms,
        extra: None,
    })?;

    Metafile::decode(&bytes).map_err(|e| {
        tracing::trace!(
            "{}: {url} - Content-Type: {content_type} - {e}",
            candidate.name
        );
        SnatchFailure {
            error: SnatchError::InvalidContents,
            retry_after_ms,
            extra: None,
        }
    })
}

#[derive(Debug, Clone, Copy)]
pub struct SnatchOptions {
    pub retries: u32,
    pub delay_ms: i64,
}

/// Downloads a candidate with retries.
///
/// Rate limiting and magnet links are terminal — retrying either is pointless —
/// so they clear the failure history and return immediately.
pub async fn snatch(
    candidate: &Candidate,
    label: SearcheeLabel,
    options: SnatchOptions,
) -> Result<Metafile, SnatchError> {
    let retries = options.retries;
    let retry_after_end_time = now_ms() + retries as i64 * options.delay_ms;
    let mut last_error = SnatchError::UnknownError;

    for attempt in 0..=retries {
        let progress = format!("{}/{}", attempt + 1, retries + 1);
        match snatch_once(candidate).await {
            Ok(metafile) => {
                tracing::debug!(
                    label = label.as_str(),
                    "Snatched {} from {}{}",
                    candidate.name,
                    candidate.tracker,
                    if attempt > 0 {
                        format!(" on attempt {progress}")
                    } else {
                        String::new()
                    }
                );
                clear_failure(&candidate.link);
                return Ok(metafile);
            }
            Err(failure) => {
                last_error = failure.error;
                if matches!(
                    failure.error,
                    SnatchError::RateLimited | SnatchError::MagnetLink
                ) {
                    clear_failure(&candidate.link);
                    return Err(failure.error);
                }

                let link_history = record_failure(&candidate.link);
                let tracker_history = record_failure(&candidate.tracker);
                let extra = failure
                    .extra
                    .as_deref()
                    .map(|e| format!(" - {e}"))
                    .unwrap_or_default();

                if link_history.num_failures > retries + 1 {
                    tracing::warn!(
                        label = label.as_str(),
                        "Snatching {} from {} stopped after attempt {progress}, this snatch has failed too many times recently: {}{extra}",
                        candidate.name,
                        candidate.tracker,
                        failure.error.as_str()
                    );
                    return Err(failure.error);
                }
                if tracker_history.num_failures > retries * 2 + 1 {
                    tracing::warn!(
                        label = label.as_str(),
                        "Snatching {} from {} stopped after attempt {progress}, this tracker has failed too many times recently: {}{extra}",
                        candidate.name,
                        candidate.tracker,
                        failure.error.as_str()
                    );
                    return Err(failure.error);
                }
                if let Some(retry_after_ms) = failure.retry_after_ms
                    && now_ms() + retry_after_ms >= retry_after_end_time
                {
                    tracing::warn!(
                        label = label.as_str(),
                        "Snatching {} from {} stopped after attempt {progress}, Retry-After of {}s exceeds timeout: {}{extra}",
                        candidate.name,
                        candidate.tracker,
                        retry_after_ms / 1000,
                        failure.error.as_str()
                    );
                    return Err(failure.error);
                }

                let delay_ms = options.delay_ms.max(failure.retry_after_ms.unwrap_or(0));
                tracing::error!(
                    label = label.as_str(),
                    "Snatch attempt {progress} from {} for {} failed{}: {}{extra}",
                    candidate.tracker,
                    candidate.name,
                    if attempt < retries {
                        format!(", retrying in {}s", delay_ms / 1000)
                    } else {
                        String::new()
                    },
                    failure.error.as_str()
                );
                if attempt >= retries {
                    break;
                }
                wait(delay_ms.max(0) as u64).await;
            }
        }
    }

    Err(last_error)
}

/// Drops snatch-history entries older than a day (the cleanup job).
pub fn prune_snatch_history(max_age_ms: i64) -> Vec<String> {
    let now = now_ms();
    let mut pruned = Vec::new();
    if let Ok(mut history) = SNATCH_HISTORY.lock() {
        history.retain(|key, record| {
            if now - record.initial_failure_at > max_age_ms {
                pruned.push(key.clone());
                false
            } else {
                true
            }
        });
    }
    pruned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(link: &str) -> Candidate {
        Candidate {
            guid: "guid".into(),
            name: "Some.Release".into(),
            tracker: "TrackerName".into(),
            link: link.into(),
            size: 1,
            pub_date: None,
            indexer_id: Some(1),
        }
    }

    #[tokio::test]
    async fn magnet_links_fail_immediately_without_a_request() {
        let result = snatch(
            &candidate("magnet:?xt=urn:btih:abcd"),
            SearcheeLabel::Search,
            SnatchOptions {
                retries: 4,
                delay_ms: 1,
            },
        )
        .await;
        assert_eq!(result.unwrap_err(), SnatchError::MagnetLink);
    }

    #[test]
    fn failure_history_accumulates_per_key_and_clears() {
        let key = "test-key-accumulate";
        clear_failure(key);
        assert_eq!(record_failure(key).num_failures, 1);
        assert_eq!(record_failure(key).num_failures, 2);
        clear_failure(key);
        assert_eq!(record_failure(key).num_failures, 1);
        clear_failure(key);
    }

    /// SNATCH_HISTORY is a process-global that every test in this module shares
    /// while they run in parallel, so this test owns two uniquely-named keys and
    /// touches nothing else: clearing the whole map here used to wipe the
    /// counters `failure_history_accumulates_per_key_and_clears` was mid-way
    /// through asserting on.
    #[test]
    fn pruning_drops_only_stale_entries() {
        let stale = "test-prune-stale";
        let fresh = "test-prune-fresh";
        {
            let mut history = SNATCH_HISTORY.lock().unwrap();
            history.insert(
                stale.into(),
                FailureRecord {
                    initial_failure_at: now_ms() - 100_000,
                    num_failures: 3,
                },
            );
            history.insert(
                fresh.into(),
                FailureRecord {
                    initial_failure_at: now_ms(),
                    num_failures: 1,
                },
            );
        }

        // Entries another test just recorded are fresh, so pruning cannot drop
        // them and the assertions below hold whatever else is in the map.
        let pruned = prune_snatch_history(50_000);
        assert!(pruned.contains(&stale.to_string()));
        assert!(!pruned.contains(&fresh.to_string()));

        let mut history = SNATCH_HISTORY.lock().unwrap();
        assert!(history.contains_key(fresh));
        assert!(!history.contains_key(stale));
        history.remove(fresh);
    }
}
