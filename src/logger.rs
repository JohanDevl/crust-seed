//! Logging.
//!
//! Ported from `logger.ts`. winston + winston-daily-rotate-file becomes
//! `tracing` with a hand-written formatter and three daily-rotating file
//! writers.
//!
//! ## Why the line format is load-bearing
//!
//! The web UI streams logs by *tail-parsing* `logs/verbose.<date>.log` with
//! this regex:
//!
//! ```text
//! ^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d{3})?) (\w+):\s*(?:\[([^\]]+)\])?\s*(.*)$
//! ```
//!
//! so `{timestamp} {level}: [{label}] {message}` is an API, not a preference.
//! The winston level vocabulary (`verbose`, `debug`) is preserved for the same
//! reason: the UI's level filter compares against those names.

use std::collections::HashSet;
use std::fmt;
use std::sync::{LazyLock, Mutex, OnceLock};

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, format::Writer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, filter::LevelFilter};

use crate::config::logs_dir;

/// Subsystem tag printed as `[label]`.
///
/// Emitted as a `tracing` field: `tracing::info!(label = Label::Search.as_str(), "...")`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    QBittorrent,
    RTorrent,
    Transmission,
    Deluge,
    Decide,
    PreFilter,
    Config,
    Torznab,
    Server,
    Scheduler,
    Search,
    Rss,
    Announce,
    Webhook,
    Inject,
    Perf,
    Cleanup,
    Arrs,
    Radarr,
    Sonarr,
    Auth,
    Index,
}

impl Label {
    pub fn as_str(self) -> &'static str {
        match self {
            Label::QBittorrent => "qbittorrent",
            Label::RTorrent => "rtorrent",
            Label::Transmission => "transmission",
            Label::Deluge => "deluge",
            Label::Decide => "decide",
            Label::PreFilter => "prefilter",
            Label::Config => "config",
            Label::Torznab => "torznab",
            Label::Server => "server",
            Label::Scheduler => "scheduler",
            Label::Search => "search",
            Label::Rss => "rss",
            Label::Announce => "announce",
            Label::Webhook => "webhook",
            Label::Inject => "inject",
            Label::Perf => "perf",
            Label::Cleanup => "cleanup",
            Label::Arrs => "arrs",
            Label::Radarr => "radarr",
            Label::Sonarr => "sonarr",
            Label::Auth => "auth",
            Label::Index => "index",
        }
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Redaction ──────────────────────────────────────────────────────────────

const REDACTED: &str = "[REDACTED]";

/// URL passwords and tracker keys that must never reach a log file, learned
/// from the configuration at startup.
static SECRETS: LazyLock<Mutex<Vec<String>>> = LazyLock::new(|| Mutex::new(Vec::new()));

static REDACTION_PATTERNS: LazyLock<Vec<(regex::Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (regex::Regex::new(r"key=[a-zA-Z0-9]+").unwrap(), "key="),
        (regex::Regex::new(r"pass=[a-zA-Z0-9]+").unwrap(), "pass="),
        (regex::Regex::new(r"apiKey: '[^']+'").unwrap(), "apiKey: "),
    ]
});

/// Passkeys embedded in announce/download URLs, e.g. `/download/12345/<key>`.
static PASSKEY_PATTERNS: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    vec![
        regex::Regex::new(r"(?:(?:auto|download)[./]\d+[./])([a-zA-Z0-9]+)").unwrap(),
        regex::Regex::new(r"(?:\d+[./](?:auto|download)[./])([a-zA-Z0-9]+)").unwrap(),
        regex::Regex::new(r"/notification/crossSeed/([a-zA-Z0-9_-]+)").unwrap(),
    ]
});

/// Registers a URL's password (in raw, decoded and encoded forms) for redaction.
pub fn register_secret_url(url_str: &str) {
    let Ok(url) = url::Url::parse(url_str) else {
        return;
    };
    let Some(password) = url.password() else {
        return;
    };
    if password.is_empty() {
        return;
    }
    let decoded = percent_encoding::percent_decode_str(password)
        .decode_utf8_lossy()
        .into_owned();
    let encoded =
        percent_encoding::utf8_percent_encode(password, percent_encoding::NON_ALPHANUMERIC)
            .to_string();

    let mut secrets = SECRETS.lock().expect("secrets lock");
    for candidate in [password.to_string(), decoded, encoded] {
        if !candidate.is_empty() && !secrets.contains(&candidate) {
            secrets.push(candidate);
        }
    }
}

/// Registers every secret the runtime config carries: torrent-client URLs,
/// torznab URLs, and arr URLs.
pub fn register_config_secrets(config: &crate::config::RuntimeConfig) {
    let mut urls: Vec<&String> = Vec::new();
    urls.extend(config.torznab.iter());
    urls.extend(config.sonarr.iter());
    urls.extend(config.radarr.iter());
    for url in urls {
        register_secret_url(url);
    }
    for entry in &config.torrent_clients {
        // Entries look like "qbittorrent:http://user:pass@host:8080".
        if let Some((_, url)) = entry.split_once(':') {
            register_secret_url(url);
        }
    }
}

pub fn redact(message: &str) -> String {
    let mut out = message.to_string();

    for (pattern, prefix) in REDACTION_PATTERNS.iter() {
        out = pattern
            .replace_all(&out, format!("{prefix}{REDACTED}").as_str())
            .into_owned();
    }
    for pattern in PASSKEY_PATTERNS.iter() {
        out = pattern
            .replace_all(&out, |caps: &regex::Captures<'_>| {
                caps[0].replace(&caps[1], REDACTED)
            })
            .into_owned();
    }
    if let Ok(secrets) = SECRETS.lock() {
        for secret in secrets.iter() {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
    }
    out
}

/// Removes SGR escape sequences — the console transport colourises, the file
/// transports must not.
pub fn strip_ansi(input: &str) -> String {
    static ANSI: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
    ANSI.replace_all(input, "").into_owned()
}

// ─── log-once ───────────────────────────────────────────────────────────────

static LOG_ONCE_CACHE: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Runs `f` only the first time this `cache_key` is seen. With `ttl`, the key
/// expires so the message can repeat later.
pub fn log_once(cache_key: &str, f: impl FnOnce(), ttl: Option<std::time::Duration>) {
    {
        let mut cache = LOG_ONCE_CACHE.lock().expect("log-once lock");
        if !cache.insert(cache_key.to_string()) {
            return;
        }
    }
    f();
    if let Some(ttl) = ttl {
        let key = cache_key.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            if let Ok(mut cache) = LOG_ONCE_CACHE.lock() {
                cache.remove(&key);
            }
        });
    }
}

// ─── Formatting ─────────────────────────────────────────────────────────────

/// winston's level names, which the web UI's log filter compares against.
fn level_name(level: &Level) -> &'static str {
    match *level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG => "verbose",
        Level::TRACE => "debug",
    }
}

/// Pulls the `label` field and the message out of an event.
#[derive(Default)]
struct EventParts {
    label: Option<String>,
    message: String,
}

impl tracing::field::Visit for EventParts {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "label" => self.label = Some(value.to_string()),
            "message" => self.message = value.to_string(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        let rendered = format!("{value:?}");
        match field.name() {
            "label" => self.label = Some(rendered.trim_matches('"').to_string()),
            "message" => self.message = rendered,
            _ => {}
        }
    }
}

struct CrossSeedFormat {
    /// The console transport prints seconds only and keeps colour; the file
    /// transports print milliseconds and strip it.
    with_millis: bool,
    colorize: bool,
}

impl<S, N> FormatEvent<S, N> for CrossSeedFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut parts = EventParts::default();
        event.record(&mut parts);

        let now = chrono::Local::now();
        let timestamp = if self.with_millis {
            now.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
        } else {
            now.format("%Y-%m-%d %H:%M:%S").to_string()
        };

        let level = level_name(event.metadata().level());
        let level_rendered = if self.colorize {
            let color = match *event.metadata().level() {
                Level::ERROR => "\x1b[31m",
                Level::WARN => "\x1b[33m",
                Level::INFO => "\x1b[32m",
                _ => "\x1b[36m",
            };
            format!("{color}{level}\x1b[0m")
        } else {
            level.to_string()
        };

        let label = parts
            .label
            .map(|label| format!("[{label}] "))
            .unwrap_or_default();

        let message = redact(&parts.message);
        let message = if self.colorize {
            message
        } else {
            strip_ansi(&message)
        };

        writeln!(writer, "{timestamp} {level_rendered}: {label}{message}")
    }
}

// ─── Initialisation ─────────────────────────────────────────────────────────

/// Held for the process lifetime — dropping these flushes and stops the
/// non-blocking writer threads.
pub struct LoggerGuards(#[allow(dead_code)] Vec<tracing_appender::non_blocking::WorkerGuard>);

static LOGGER_INITIALIZED: OnceLock<()> = OnceLock::new();

/// Installs the console + three-file logging stack.
///
/// `verbose` raises the console from `info` to `debug`; the files always
/// receive everything they are scoped to, exactly as winston was configured.
pub fn initialize_logger(verbose: bool) -> LoggerGuards {
    let mut guards = Vec::new();
    if LOGGER_INITIALIZED.set(()).is_err() {
        return LoggerGuards(guards);
    }

    let dir = logs_dir();
    let _ = std::fs::create_dir_all(&dir);

    // Each layer is boxed so the four can live in one `Vec<Box<dyn Layer>>`;
    // chaining `.with()` would otherwise give every layer a different `S`.
    type BoxedLayer = Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>;
    let mut layers: Vec<BoxedLayer> = Vec::new();

    for (prefix, level) in [
        ("error", LevelFilter::ERROR),
        ("info", LevelFilter::INFO),
        ("verbose", LevelFilter::TRACE),
    ] {
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(prefix)
            .filename_suffix("log")
            .max_log_files(14)
            .build(&dir)
            .expect("create log appender");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        guards.push(guard);
        layers.push(Box::new(
            tracing_subscriber::fmt::layer()
                .event_format(CrossSeedFormat {
                    with_millis: true,
                    colorize: false,
                })
                .with_writer(writer)
                .with_ansi(false)
                .with_filter(level),
        ));
    }

    layers.push(Box::new(
        tracing_subscriber::fmt::layer()
            .event_format(CrossSeedFormat {
                with_millis: false,
                colorize: true,
            })
            .with_writer(std::io::stdout)
            .with_filter(if verbose {
                LevelFilter::TRACE
            } else {
                LevelFilter::INFO
            }),
    ));

    tracing_subscriber::registry().with(layers).init();

    refresh_current_log_symlinks();
    LoggerGuards(guards)
}

/// Best-effort `*.current.log` symlinks pointing at today's files, matching
/// winston-daily-rotate-file's `createSymlink` option. Purely a convenience for
/// humans: the log-streaming code resolves the newest file itself.
pub fn refresh_current_log_symlinks() {
    #[cfg(unix)]
    {
        let dir = logs_dir();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        for prefix in ["error", "info", "verbose"] {
            let target = dir.join(format!("{prefix}.{today}.log"));
            let link = dir.join(format!("{prefix}.current.log"));
            let _ = std::fs::remove_file(&link);
            let _ = std::os::unix::fs::symlink(&target, &link);
        }
    }
}

/// A minimal stdout-only subscriber, used before the config directory is known
/// (argument parsing, permission failures).
pub fn initialize_bootstrap_logger() {
    let _ = tracing_subscriber::fmt()
        .event_format(CrossSeedFormat {
            with_millis: false,
            colorize: true,
        })
        .with_writer(std::io::stdout)
        .with_max_level(Level::INFO)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_keys_are_redacted() {
        assert_eq!(
            redact("https://tracker.example/api?key=abc123"),
            format!("https://tracker.example/api?key={REDACTED}")
        );
        assert_eq!(
            redact("https://tracker.example/download/12345/deadbeefkey/x.torrent"),
            format!("https://tracker.example/download/12345/{REDACTED}/x.torrent")
        );
    }

    #[test]
    fn url_passwords_are_redacted_once_registered() {
        register_secret_url("http://user:sup3rsecret@localhost:8112/json");
        assert!(!redact("connecting with sup3rsecret").contains("sup3rsecret"));
    }

    #[test]
    fn ansi_is_stripped_for_files() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn level_names_match_winston() {
        assert_eq!(level_name(&Level::DEBUG), "verbose");
        assert_eq!(level_name(&Level::TRACE), "debug");
        assert_eq!(level_name(&Level::INFO), "info");
    }

    /// The web UI parses log lines with a regex; this pins the shape it needs.
    #[test]
    fn rendered_lines_match_the_ui_parser() {
        let re = fancy_regex::Regex::new(
            r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}(?:\.\d{3})?) (\w+):\s*(?:\[([^\]]+)\])?\s*(.*)$",
        )
        .unwrap();
        let line = "2026-08-22 10:11:12.345 verbose: [search] searching for something";
        let caps = re.captures(line).unwrap().unwrap();
        assert_eq!(caps.get(2).unwrap().as_str(), "verbose");
        assert_eq!(caps.get(3).unwrap().as_str(), "search");
        assert_eq!(caps.get(4).unwrap().as_str(), "searching for something");
    }
}
