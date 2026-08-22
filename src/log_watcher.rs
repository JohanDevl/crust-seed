//! Streaming the verbose log to the web UI.
//!
//! Ported from `utils/logWatcher.ts`.
//!
//! The UI reads logs by tailing the file the logger writes, not through an
//! in-process channel — which means a log line written before the UI connected
//! is still visible, and the format in `logger.rs` is the contract between the
//! two.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub message: String,
}

/// The line format `logger.rs` writes.
static LOG_LINE: std::sync::LazyLock<fancy_regex::Regex> = std::sync::LazyLock::new(|| {
    fancy_regex::Regex::new(
        r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d{3})?) (\w+):\s*(?:\[([^\]]+)\])?\s*(.*)$",
    )
    .expect("log line regex")
});

fn parse_log_line(line: &str) -> Option<LogEntry> {
    let caps = LOG_LINE.captures(line).ok().flatten()?;
    Some(LogEntry {
        timestamp: local_timestamp_to_iso(caps.get(1)?.as_str()),
        level: caps.get(2)?.as_str().to_string(),
        label: caps
            .get(3)
            .map(|m| m.as_str().to_string())
            .filter(|l| !l.is_empty()),
        message: caps
            .get(4)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default(),
    })
}

/// The log file carries local time with no offset; the UI expects ISO-8601, so
/// the local zone is reattached before converting.
fn local_timestamp_to_iso(timestamp: &str) -> String {
    use chrono::TimeZone;
    let formats = ["%Y-%m-%d %H:%M:%S%.3f", "%Y-%m-%d %H:%M:%S"];
    for format in formats {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(timestamp, format)
            && let chrono::LocalResult::Single(local) = chrono::Local.from_local_datetime(&naive)
        {
            return local
                .to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        }
    }
    timestamp.to_string()
}

/// Splits a chunk of log text into entries, attaching continuation lines
/// (stack traces, multi-line messages) to the entry above them.
pub fn parse_log_content(content: &str) -> Vec<LogEntry> {
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut current: Option<LogEntry> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_log_line(line) {
            Some(entry) => {
                if let Some(previous) = current.take() {
                    entries.push(previous);
                }
                current = Some(entry);
            }
            None => match current.as_mut() {
                Some(entry) => {
                    entry.message.push('\n');
                    entry.message.push_str(line);
                }
                // A continuation with nothing above it (a truncated file, or a
                // rotation boundary) still deserves to be shown.
                None => entries.push(LogEntry {
                    timestamp: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    level: "info".to_string(),
                    label: Some("raw".to_string()),
                    message: line.to_string(),
                }),
            },
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    entries
}

/// Today's verbose log. Resolved per call rather than cached, so the watcher
/// follows the daily rotation without a restart.
pub fn current_log_path() -> PathBuf {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    crate::config::logs_dir().join(format!("verbose.{today}.log"))
}

pub async fn get_recent_logs(limit: usize) -> Vec<LogEntry> {
    let Ok(content) = tokio::fs::read_to_string(current_log_path()).await else {
        return Vec::new();
    };
    let entries = parse_log_content(&content);
    let start = entries.len().saturating_sub(limit);
    entries[start..].to_vec()
}

/// Broadcast channel carrying newly appended log entries to every subscriber.
static LOG_BROADCAST: std::sync::LazyLock<broadcast::Sender<LogEntry>> =
    std::sync::LazyLock::new(|| broadcast::channel(1024).0);

pub fn subscribe() -> broadcast::Receiver<LogEntry> {
    LOG_BROADCAST.subscribe()
}

/// Tails the verbose log and broadcasts new entries.
///
/// Polling rather than filesystem events: the file is appended to constantly,
/// events would coalesce anyway, and polling behaves identically across the
/// bind-mounted volumes this runs on in Docker.
pub async fn watch_logs() {
    let mut path = current_log_path();
    let mut position = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let current = current_log_path();
        if current != path {
            // Rotated to a new day: start from the beginning of the new file.
            path = current;
            position = 0;
        }

        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            continue;
        };
        let size = metadata.len();
        if size < position {
            // Truncated or replaced.
            position = 0;
        }
        if size == position {
            continue;
        }

        let Ok(content) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        // Reading whole-file and slicing by byte offset keeps multi-byte
        // characters intact, which a raw seek+read could split.
        if content.len() as u64 <= position {
            position = content.len() as u64;
            continue;
        }
        let fresh = &content[position as usize..];
        for entry in parse_log_content(fresh) {
            let _ = LOG_BROADCAST.send(entry);
        }
        position = content.len() as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_line_parses_into_its_parts() {
        let entry =
            parse_log_line("2026-08-22 10:11:12.345 verbose: [search] searching for something")
                .unwrap();
        assert_eq!(entry.level, "verbose");
        assert_eq!(entry.label.as_deref(), Some("search"));
        assert_eq!(entry.message, "searching for something");
        assert!(entry.timestamp.ends_with('Z'));
    }

    #[test]
    fn lines_without_a_label_parse_too() {
        let entry = parse_log_line("2026-08-22 10:11:12 info: plain message").unwrap();
        assert_eq!(entry.label, None);
        assert_eq!(entry.message, "plain message");
    }

    /// Stack traces and other continuations belong to the entry above them.
    #[test]
    fn continuation_lines_attach_to_the_previous_entry() {
        let content = "\
2026-08-22 10:11:12.345 error: [inject] something failed
    at some::function
    at another::function
2026-08-22 10:11:13.000 info: [search] next";
        let entries = parse_log_content(content);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].message.contains("at some::function"));
        assert!(entries[0].message.contains("at another::function"));
        assert_eq!(entries[1].message, "next");
    }

    #[test]
    fn an_orphan_continuation_is_still_surfaced() {
        let entries = parse_log_content("    orphaned stack frame");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label.as_deref(), Some("raw"));
    }

    #[test]
    fn blank_lines_are_ignored() {
        let entries = parse_log_content("\n\n2026-08-22 10:11:12 info: x\n\n");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn timestamps_become_iso_utc() {
        let iso = local_timestamp_to_iso("2026-08-22 10:11:12.345");
        assert!(iso.ends_with('Z'), "got {iso}");
        assert!(iso.contains("2026-08-22") || iso.contains("2026-08-21"));
    }

    #[test]
    fn an_unparseable_timestamp_is_passed_through() {
        assert_eq!(local_timestamp_to_iso("not a date"), "not a date");
    }
}
