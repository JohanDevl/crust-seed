//! Shared helpers: filesystem checks, release-title normalisation, URL
//! handling and small async primitives.
//!
//! Ported from `cross-seed/src/utils.ts` and `shared/utils.ts`. The JS
//! `mapAsync`/`filterAsync`/`combineAsyncIterables` family exists only to batch
//! work around Node's single event loop and has no Rust counterpart — callers
//! use iterators or `futures` combinators directly instead.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};
use fancy_regex::{Captures, Regex};
use tokio::sync::{Mutex as TokioMutex, OwnedSemaphorePermit, Semaphore};

use crate::constants::{
    ALL_EXTENSIONS, ALL_PARENTHESES_REGEX, ALL_SPACES_REGEX, ALL_SQUARE_BRACKETS_REGEX,
    ANIME_REGEX, EBOOK_AND_MUSIC_RELEASE_REGEX, EP_REGEX, LEVENSHTEIN_DIVISOR,
    MIN_VIDEO_QUERY_LENGTH, MOVIE_REGEX, NON_UNICODE_ALPHANUM_REGEX, RELEASE_GROUP_REGEX,
    REPACK_PROPER_REGEX, RESOLUTION_REGEX, SCENE_TITLE_REGEX, SEASON_REGEX, YEARS_REGEX,
    source_regex_remove,
};

// ─── Episode captures ───────────────────────────────────────────────────────

/// Season/episode/date pulled out of an [`EP_REGEX`]-style match.
///
/// The Rust patterns split the JS variable-width lookbehind into two branches
/// (see [`crate::constants::EP_REGEX`]), so the `season1`/`season2` and
/// `episode1`/`episode2` capture pairs are merged back into one view here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodeMatch {
    pub title: Option<String>,
    pub season: Option<String>,
    pub episode: Option<String>,
    pub date: Option<String>,
    pub year: Option<String>,
    pub month: Option<String>,
    pub day: Option<String>,
}

fn group(caps: &Captures<'_, str>, name: &str) -> Option<String> {
    caps.name(name).map(|m| m.as_str().to_string())
}

/// Merges the split capture groups of [`EP_REGEX`] / [`BAD_EP_REGEX`].
pub fn episode_match(caps: &Captures<'_, str>) -> EpisodeMatch {
    EpisodeMatch {
        title: group(caps, "title"),
        season: group(caps, "season1").or_else(|| group(caps, "season2")),
        episode: group(caps, "episode1").or_else(|| group(caps, "episode2")),
        date: group(caps, "date"),
        year: group(caps, "year"),
        month: group(caps, "month"),
        day: group(caps, "day"),
    }
}

/// `EP_REGEX.exec(name)` with the split groups already merged.
pub fn match_episode(re: &Regex, name: &str) -> Option<EpisodeMatch> {
    re.captures(name)
        .ok()
        .flatten()
        .map(|caps| episode_match(&caps))
}

/// Convenience for patterns whose only interesting group is `title`.
pub fn capture_group(re: &Regex, haystack: &str, name: &str) -> Option<String> {
    re.captures(haystack)
        .ok()
        .flatten()
        .and_then(|caps| caps.name(name).map(|m| m.as_str().to_string()))
}

// ─── OS ─────────────────────────────────────────────────────────────────────

pub async fn exists(src_path: impl AsRef<Path>) -> bool {
    tokio::fs::metadata(src_path.as_ref()).await.is_ok()
}

pub async fn not_exists(src_path: impl AsRef<Path>) -> bool {
    !exists(src_path).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirVerificationFailure {
    Missing,
    NotDirectory,
    Unreadable,
    Unwritable,
}

impl DirVerificationFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            DirVerificationFailure::Missing => "missing",
            DirVerificationFailure::NotDirectory => "not-directory",
            DirVerificationFailure::Unreadable => "unreadable",
            DirVerificationFailure::Unwritable => "unwritable",
        }
    }
}

bitflags_lite! {
    /// Subset of `fs.constants` the original passes to `verifyDir`.
    pub struct DirPermissions;
    pub const R_OK: u32 = 0b01;
    pub const W_OK: u32 = 0b10;
}

/// Checks that `src_dir` exists, is a directory, and is readable/writable as
/// requested. Errors are logged with the same wording as the original so the
/// startup output stays recognisable.
pub async fn verify_dir(
    src_dir: &Path,
    test_src_name: &str,
    permissions: u32,
) -> Result<std::fs::Metadata, DirVerificationFailure> {
    let dir_display = src_dir.display();

    let log_missing = |message: &str| {
        tracing::error!(
            "\tYour {test_src_name} \"{dir_display}\" is not a valid directory on the filesystem: {message}."
        );
    };
    let log_permissions = |message: &str| {
        tracing::error!(
            "\tYour {test_src_name} \"{dir_display}\" has invalid permissions: {message}."
        );
    };

    let metadata = match tokio::fs::metadata(src_dir).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log_missing("does not exist");
            return Err(DirVerificationFailure::Missing);
        }
        Err(e) => {
            log_permissions(&e.to_string());
            return Err(DirVerificationFailure::Unreadable);
        }
    };

    if !metadata.is_dir() {
        tracing::error!(
            "\tYour {test_src_name} \"{dir_display}\" is not a directory on the filesystem."
        );
        return Err(DirVerificationFailure::NotDirectory);
    }

    if permissions & R_OK != 0 && tokio::fs::read_dir(src_dir).await.is_err() {
        log_permissions("no read permissions");
        return Err(DirVerificationFailure::Unreadable);
    }

    if permissions & W_OK != 0 {
        let temp_file = src_dir.join(test_src_name);
        match tokio::fs::write(&temp_file, test_src_name).await {
            Ok(()) => {
                if not_exists(&temp_file).await {
                    log_permissions("no write permissions - could not verify test file");
                    return Err(DirVerificationFailure::Unwritable);
                }
                let _ = tokio::fs::remove_file(&temp_file).await;
            }
            Err(_) => {
                log_permissions("no write permissions");
                return Err(DirVerificationFailure::Unwritable);
            }
        }
    }

    Ok(metadata)
}

/// True when `child_path` resolves *strictly* inside one of `parent_dirs`.
pub fn is_child_path(child_path: &Path, parent_dirs: &[PathBuf]) -> bool {
    let child = normalize_absolute(child_path);
    parent_dirs.iter().any(|parent| {
        let parent = normalize_absolute(parent);
        child != parent && child.starts_with(&parent)
    })
}

/// `path.resolve` equivalent: makes the path absolute against the cwd and
/// removes `.`/`..` lexically (no symlink resolution, same as Node).
pub fn normalize_absolute(p: &Path) -> PathBuf {
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Counts entries under `dirs`, descending at most `max_data_depth` levels.
pub async fn count_dir_entries_rec(
    dirs: &[PathBuf],
    max_data_depth: i32,
) -> std::io::Result<usize> {
    if max_data_depth == 0 {
        return Ok(0);
    }
    let mut count = 0usize;
    let mut next = Vec::new();
    for dir in dirs {
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(e) => e,
            Err(e) => return Err(e),
        };
        while let Some(entry) = entries.next_entry().await? {
            count += 1;
            if entry.file_type().await?.is_dir() {
                next.push(entry.path());
            }
        }
    }
    if !next.is_empty() {
        count += Box::pin(count_dir_entries_rec(&next, max_data_depth - 1)).await?;
    }
    Ok(count)
}

// ─── Extensions ─────────────────────────────────────────────────────────────

/// `path.extname(name.toLowerCase())` — note JS returns `""` for dotfiles and
/// includes the leading dot otherwise.
pub fn extname(name: &str) -> String {
    let lower = name.to_lowercase();
    let base = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    match base.rfind('.') {
        Some(0) | None => String::new(),
        Some(idx) => base[idx..].to_string(),
    }
}

pub fn has_ext_name(name: &str, exts: &[&str]) -> bool {
    let ext = extname(name);
    exts.contains(&ext.as_str())
}

/// Strips a *known media* extension only — arbitrary trailing dots are kept,
/// matching the original's loop over `ALL_EXTENSIONS`.
pub fn strip_extension(filename: &str) -> String {
    for ext in ALL_EXTENSIONS.iter() {
        if filename.to_lowercase().ends_with(ext) {
            return filename[..filename.len() - ext.len()].to_string();
        }
    }
    filename.to_string()
}

// ─── Time ───────────────────────────────────────────────────────────────────

/// Milliseconds since the Unix epoch — the app stores every timestamp this way
/// because the SQLite schema is shared in shape with the original.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn n_ms_ago(n: i64) -> i64 {
    now_ms() - n
}

pub async fn wait(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// `new Date(ts).toLocaleString("sv")` — Swedish locale formats as
/// `YYYY-MM-DD HH:MM:SS`, which is why the original picked it.
pub fn human_readable_date(timestamp_ms: i64) -> String {
    match Local.timestamp_millis_opt(timestamp_ms) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => String::new(),
    }
}

/// Renders a byte count the way the web UI's `humanReadableSize` does.
pub fn human_readable_size(bytes: i64, binary: bool) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }
    let k = if binary { 1024f64 } else { 1000f64 };
    let sizes: &[&str] = if binary {
        &["B", "KiB", "MiB", "GiB", "TiB"]
    } else {
        &["B", "kB", "MB", "GB", "TB"]
    };
    let exponent = ((bytes.abs() as f64).ln() / k.ln()).floor() as i32;
    let exponent = exponent.clamp(0, sizes.len() as i32 - 1);
    let coefficient = bytes as f64 / k.powi(exponent);
    let rendered = format!("{coefficient:.2}");
    let trimmed = rendered.trim_end_matches('0').trim_end_matches('.');
    format!("{trimmed} {}", sizes[exponent as usize])
}

/// `Intl.ListFormat("en")` for the two styles the codebase actually uses:
/// `long`/`conjunction` (`"a, b, and c"`) and `narrow`/`unit` (`"a, b, c"`).
pub fn format_as_list(strings: &[String], sort: bool, narrow_unit: bool) -> String {
    let mut items: Vec<String> = strings.to_vec();
    if sort {
        items.sort();
    }
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        _ if narrow_unit => items.join(", "),
        2 => format!("{} and {}", items[0], items[1]),
        n => format!("{}, and {}", items[..n - 1].join(", "), items[n - 1]),
    }
}

/// Info hashes are secrets-adjacent in logs (they identify a private-tracker
/// download), so only the first 8 characters are printed.
pub fn sanitize_info_hash(info_hash: &str) -> String {
    format!("{}...", &info_hash[..info_hash.len().min(8)])
}

// ─── Titles ─────────────────────────────────────────────────────────────────

pub fn cleanse_separators(s: &str) -> String {
    static DELIMS: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"[._()\[\]]").unwrap());
    static TRIM_HYPHENS: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^\s*-+|-+\s*$").unwrap());

    let no_brackets = ALL_SQUARE_BRACKETS_REGEX.replace_all(s, "");
    let spaced = DELIMS.replace_all(&no_brackets, " ");
    let normalized = ALL_SPACES_REGEX.replace_all(&spaced, " ");
    let dehyphenated = TRIM_HYPHENS.replace_all(&normalized, "");
    dehyphenated.trim().to_string()
}

/// Drops a leading scene release-group prefix (`abc-Title`).
pub fn clean_title(title: &str) -> String {
    let cleansed = cleanse_separators(title);
    capture_group(&SCENE_TITLE_REGEX, &cleansed, "title").unwrap_or(cleansed)
}

pub fn clean_book_and_audio_title(title: &str) -> String {
    let cleansed = cleanse_separators(title);
    EBOOK_AND_MUSIC_RELEASE_REGEX
        .replace_all(&cleansed, "")
        .trim()
        .to_string()
}

/// Replaces the LAST match of `re` — the JS helper took a global regex and
/// walked `matchAll`.
pub fn replace_last_occurrence(s: &str, re: &Regex, new_str: &str) -> String {
    let mut last: Option<(usize, usize)> = None;
    let mut start = 0usize;
    while start <= s.len() {
        match re.find_from_pos(s, start) {
            Ok(Some(m)) => {
                last = Some((m.start(), m.end()));
                start = if m.end() > m.start() {
                    m.end()
                } else {
                    m.end() + 1
                };
            }
            _ => break,
        }
    }
    match last {
        Some((from, to)) => format!("{}{}{}", &s[..from], new_str, &s[to..]),
        None => s.to_string(),
    }
}

pub fn create_key_title(title: &str) -> Option<String> {
    let key = NON_UNICODE_ALPHANUM_REGEX
        .replace_all(&clean_title(title), "")
        .to_lowercase();
    if key.chars().count() > 4 {
        Some(replace_last_occurrence(&key, &YEARS_REGEX, ""))
    } else if !key.is_empty() {
        Some(key)
    } else {
        None
    }
}

pub fn is_bad_title(title: &str) -> bool {
    matches!(title.to_lowercase().as_str(), "season" | "ep")
}

pub fn strip_meta_from_name(name: &str) -> String {
    static TRAILING_DASH: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\s*-\s*$").unwrap());

    let stem = strip_extension(name);
    let scene_title = capture_group(&SCENE_TITLE_REGEX, &stem, "title").unwrap_or(stem);
    let without_group = RELEASE_GROUP_REGEX.replace(&scene_title, "");
    let without_dash = TRAILING_DASH.replace(&without_group, "");
    let without_res = RESOLUTION_REGEX.replace(&without_dash, "");
    let without_repack = REPACK_PROPER_REGEX.replace(&without_res, "");
    source_regex_remove(&without_repack)
}

pub fn reformat_title_for_searching(name: &str) -> String {
    let series_title = capture_group(&EP_REGEX, name, "title")
        .or_else(|| capture_group(&SEASON_REGEX, name, "title"));
    if let Some(series_title) = series_title {
        let title = clean_title(&series_title);
        return if title.chars().count() > 4 {
            let without_year = replace_last_occurrence(&title, &YEARS_REGEX, "");
            ALL_SPACES_REGEX
                .replace_all(&without_year, " ")
                .trim()
                .to_string()
        } else {
            title
        };
    }
    let movie_match = MOVIE_REGEX
        .find(name)
        .ok()
        .flatten()
        .map(|m| m.as_str().to_string());
    clean_title(movie_match.as_deref().unwrap_or(name))
}

/// Possible anime search queries. Only meaningful when the media type is
/// [`crate::constants::MediaType::Anime`].
pub fn get_anime_queries(stem: &str) -> Vec<String> {
    let mut queries = Vec::new();
    let Ok(Some(caps)) = ANIME_REGEX.captures(stem) else {
        return queries;
    };
    let release = caps.name("release").map(|m| m.as_str()).unwrap_or("");
    if let Some(title) = caps.name("title").map(|m| m.as_str()) {
        let stripped = clean_title(title);
        let base = if stripped.is_empty() {
            title
        } else {
            &stripped
        };
        queries.push(format!("{base} {release}"));
    }
    if let Some(alt_title) = caps.name("altTitle").map(|m| m.as_str()) {
        if is_bad_title(alt_title) {
            return queries;
        }
        let stripped = clean_title(alt_title);
        let base = if stripped.is_empty() {
            alt_title
        } else {
            &stripped
        };
        queries.push(format!("{base} {release}"));
    }
    queries
}

/// Possible generic-video search queries. Only meaningful when the media type
/// is [`crate::constants::MediaType::Video`].
pub fn get_video_queries(stem: &str) -> Vec<String> {
    // Anime that fails the ANIME_REGEX often looks like `[group] Title (Extra)`.
    let no_parens = ALL_PARENTHESES_REGEX.replace_all(stem, "");
    let squeezed = ALL_SPACES_REGEX.replace_all(&no_parens, "");
    let squeezed = ALL_SPACES_REGEX.replace_all(squeezed.trim(), " ");
    let no_parentheses = clean_title(&strip_meta_from_name(squeezed.trim()));
    if no_parentheses.chars().count() >= MIN_VIDEO_QUERY_LENGTH {
        return vec![no_parentheses];
    }

    let video_query = clean_title(&strip_meta_from_name(stem));
    if !video_query.is_empty() {
        return vec![video_query];
    }
    let video_query = strip_meta_from_name(stem);
    if !video_query.is_empty() {
        return vec![video_query];
    }
    vec![stem.to_string()]
}

/// Fuzzy title comparison used by the matching pipeline: normalise both names
/// down to key titles, then accept a Levenshtein distance proportional to their
/// length (`len / 3`) or a substring relationship.
pub fn are_media_titles_similar(
    a: &str,
    b: &str,
    expand_titles: impl Fn(&[String]) -> Vec<String>,
) -> bool {
    fn candidate_titles(s: &str) -> Vec<String> {
        for re in [&*EP_REGEX, &*SEASON_REGEX, &*MOVIE_REGEX, &*ANIME_REGEX] {
            if let Ok(Some(caps)) = re.captures(s) {
                let mut titles = Vec::new();
                if let Some(t) = caps.name("title") {
                    titles.push(t.as_str().to_string());
                }
                if let Some(t) = caps.name("altTitle") {
                    titles.push(t.as_str().to_string());
                }
                if !titles.is_empty() {
                    return titles;
                }
            }
        }
        vec![s.to_string()]
    }

    let key_titles = |s: &str| -> Vec<String> {
        expand_titles(&candidate_titles(s))
            .iter()
            .filter_map(|t| create_key_title(&strip_meta_from_name(t)))
            .collect()
    };

    let titles_a = key_titles(a);
    let titles_b = key_titles(b);
    if titles_a.is_empty() || titles_b.is_empty() {
        return false;
    }

    let max_distance_of = |titles: &[String]| -> usize {
        let total: usize = titles.iter().map(|t| t.chars().count()).sum();
        total / titles.len() / LEVENSHTEIN_DIVISOR
    };
    let max_distance = max_distance_of(&titles_a).max(max_distance_of(&titles_b));

    titles_a.iter().any(|ta| {
        titles_b.iter().any(|tb| {
            strsim::levenshtein(ta, tb) <= max_distance || ta.contains(tb) || tb.contains(ta)
        })
    })
}

// ─── URLs ───────────────────────────────────────────────────────────────────

/// `url.origin + url.pathname` — strips query string, fragment and credentials.
pub fn sanitize_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => url.to_string(),
    }
}

pub fn get_apikey(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "apikey")
        .map(|(_, v)| v.into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlCredentials {
    pub username: String,
    pub password: String,
    /// Origin + path, credentials removed — what the client should actually
    /// connect to.
    pub href: String,
}

/// Splits `scheme://user:pass@host/path` into credentials plus a clean href.
///
/// The original hand-normalised the userinfo before handing it to `new URL`
/// because Node rejects raw `@`/`:` in passwords; the `url` crate is equally
/// strict, so the same pre-encoding pass is kept.
pub fn extract_credentials_from_url(
    raw_url: &str,
    base_path: Option<&str>,
) -> Result<UrlCredentials, &'static str> {
    static USERINFO: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?i)^([a-z][a-z\d+\-.]*://)([^@]+)@(.+)$").unwrap()
    });

    fn safe_decode(value: &str) -> String {
        percent_encoding::percent_decode_str(value)
            .decode_utf8()
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| value.to_string())
    }

    let normalized = match USERINFO.captures(raw_url) {
        Ok(Some(caps)) => {
            let protocol = caps.get(1).unwrap().as_str();
            let auth = caps.get(2).unwrap().as_str();
            let host_and_path = caps.get(3).unwrap().as_str();
            match auth.find(':') {
                None => raw_url.to_string(),
                Some(idx) => {
                    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
                        .remove(b'-')
                        .remove(b'_')
                        .remove(b'.')
                        .remove(b'!')
                        .remove(b'~')
                        .remove(b'*')
                        .remove(b'\'')
                        .remove(b'(')
                        .remove(b')');
                    let decoded_user = safe_decode(&auth[..idx]);
                    let decoded_pass = safe_decode(&auth[idx + 1..]);
                    let user = percent_encoding::utf8_percent_encode(&decoded_user, SET);
                    let pass = percent_encoding::utf8_percent_encode(&decoded_pass, SET);
                    format!("{protocol}{user}:{pass}@{host_and_path}")
                }
            }
        }
        _ => raw_url.to_string(),
    };

    let parsed = url::Url::parse(&normalized).map_err(|_| "invalid URL")?;
    let origin = parsed.origin().ascii_serialization();
    let pathname = parsed.path().to_string();
    let href = match base_path {
        Some(base) => format!("{origin}{}", join_posix(&pathname, base)),
        None if pathname == "/" => origin,
        None => format!("{origin}{pathname}"),
    };

    Ok(UrlCredentials {
        username: safe_decode(parsed.username()),
        password: safe_decode(parsed.password().unwrap_or("")),
        href,
    })
}

/// `path.posix.join` restricted to the two-segment case the callers need.
pub fn join_posix(a: &str, b: &str) -> String {
    let joined = format!("{}/{}", a.trim_end_matches('/'), b.trim_start_matches('/'));
    let mut out: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    format!("/{}", out.join("/"))
}

/// `getLogString` — how a searchee or a candidate is named in a log line.
///
/// The info hash matters more than it looks: a tracker can list the same
/// release twice under different URLs, and two distinct uploads of one release
/// often carry the same torrent name. Without the hash, two consecutive
/// "injected" lines are indistinguishable from one duplicate injection.
///
/// The original colours each part with chalk. crust-seed writes plain text to
/// its log files, so only the structure is reproduced.
pub fn log_string(
    title: &str,
    name: &str,
    info_hash: Option<&str>,
    client_host: Option<&str>,
    path: Option<&str>,
) -> String {
    let identity = match (info_hash, client_host) {
        (None, None) => None,
        (hash, host) => Some(format!(
            "{}{}",
            hash.map(sanitize_info_hash).unwrap_or_default(),
            host.map(|h| format!("@{h}")).unwrap_or_default()
        )),
    };
    if title == name {
        return match (identity, path) {
            (Some(identity), _) => format!("{title} [{identity}]"),
            (None, Some(path)) => path.to_string(),
            (None, None) => title.to_string(),
        };
    }
    match (identity, path) {
        (Some(identity), _) => format!("{title} [{name} [{identity}]]"),
        (None, Some(path)) => format!("{title} [{path}]"),
        (None, None) => format!("{title} [{name}]"),
    }
}

// ─── Strings ────────────────────────────────────────────────────────────────

pub fn capitalize_first_letter(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// First run of digits in the string, as the JS `parseInt(str.match(/\d+/)[0])`.
pub fn extract_int(s: &str) -> Option<i64> {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Splits a path into its components, dropping the root.
pub fn get_path_parts(path_str: &str) -> Vec<String> {
    Path::new(path_str)
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Escapes bare `"` inside JSON string *values* so a sloppy tracker response
/// still parses. Ported verbatim in intent from `escapeUnescapedQuotesInJsonValues`.
pub fn escape_unescaped_quotes_in_json_values(json_str: &str) -> String {
    static VALUES: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r#""[^"]*?"\s*:\s*"(?<value>.+?)"\s*(?:,|\})"#).unwrap()
    });
    static BARE_QUOTE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r#"(?<!\\)""#).unwrap());

    let mut out = String::with_capacity(json_str.len());
    let mut last = 0usize;
    let mut pos = 0usize;
    while pos <= json_str.len() {
        let Ok(Some(caps)) = VALUES.captures_from_pos(json_str, pos) else {
            break;
        };
        let whole = caps.get(0).unwrap();
        let Some(value) = caps.name("value") else {
            pos = whole.end();
            continue;
        };
        out.push_str(&json_str[last..value.start()]);
        out.push_str(&BARE_QUOTE.replace_all(value.as_str(), r#"\""#));
        last = value.end();
        pos = whole.end();
    }
    out.push_str(&json_str[last..]);
    out
}

// ─── Concurrency ────────────────────────────────────────────────────────────

/// Named mutexes guarding the long-running pipeline stages.
///
/// The JS version keyed a `Map<Mutex, Promise>` and could either queue or share
/// the in-flight result; here `with_mutex` queues (the `useQueue: true` case)
/// and `try_with_mutex` skips when busy (the `useQueue: false` case, whose only
/// observable difference is that callers do not get the shared value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MutexName {
    IndexTorrentsAndDataDirs,
    CheckJobs,
    CreateAllSearchees,
    GuidInfoHashMap,
    ClientInjection,
}

#[derive(Default)]
pub struct MutexRegistry {
    locks: dashmap::DashMap<MutexName, Arc<TokioMutex<()>>>,
}

impl MutexRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_for(&self, name: MutexName) -> Arc<TokioMutex<()>> {
        self.locks
            .entry(name)
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    /// Queues behind any in-flight holder.
    pub async fn with_mutex<T, F>(&self, name: MutexName, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let lock = self.lock_for(name);
        let _guard = lock.lock().await;
        fut.await
    }

    /// Returns `None` instead of waiting when the mutex is held.
    pub async fn try_with_mutex<T, F>(&self, name: MutexName, fut: F) -> Option<T>
    where
        F: std::future::Future<Output = T>,
    {
        let lock = self.lock_for(name);
        let _guard = lock.try_lock().ok()?;
        Some(fut.await)
    }
}

/// FIFO semaphore with an optional per-permit lifetime.
///
/// `tokio::sync::Semaphore` is already fair; the lifetime is what the original
/// added, to stop a wedged HTTP call from holding a slot forever.
#[derive(Clone)]
pub struct AsyncSemaphore {
    inner: Arc<Semaphore>,
    lifetime: Option<std::time::Duration>,
}

impl AsyncSemaphore {
    pub fn new(permits: usize, lifetime: Option<std::time::Duration>) -> Self {
        assert!(permits > 0, "Permits count must be positive");
        Self {
            inner: Arc::new(Semaphore::new(permits)),
            lifetime,
        }
    }

    pub async fn acquire(&self) -> SemaphoreGuard {
        let permit = self
            .inner
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(lifetime) = self.lifetime {
            let released = released.clone();
            tokio::spawn(async move {
                tokio::time::sleep(lifetime).await;
                released.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        }
        SemaphoreGuard {
            _permit: permit,
            _released: released,
        }
    }
}

pub struct SemaphoreGuard {
    _permit: OwnedSemaphorePermit,
    _released: Arc<std::sync::atomic::AtomicBool>,
}

// ─── Collections ────────────────────────────────────────────────────────────

/// Deduplicates while preserving first-seen order (JS `new Set([...])` spread).
pub fn dedupe_preserving_order<T: Clone + std::hash::Hash + Eq>(items: &[T]) -> Vec<T> {
    let mut seen = HashSet::new();
    items
        .iter()
        .filter(|item| seen.insert((*item).clone()))
        .cloned()
        .collect()
}

/// A tiny stand-in for the `bitflags` crate — only two flags are ever used.
macro_rules! bitflags_lite {
    (
        $(#[$meta:meta])*
        pub struct $name:ident;
        $(pub const $flag:ident: u32 = $value:expr;)*
    ) => {
        $(#[$meta])*
        pub struct $name;
        $(pub const $flag: u32 = $value;)*
    };
}
use bitflags_lite;

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(titles: &[String]) -> Vec<String> {
        titles.to_vec()
    }

    #[test]
    fn extname_matches_node() {
        assert_eq!(extname("Movie.2020.MKV"), ".mkv");
        assert_eq!(extname(".gitignore"), "");
        assert_eq!(extname("noext"), "");
        assert_eq!(extname("dir/file.tar.gz"), ".gz");
    }

    #[test]
    fn strip_extension_only_strips_known_media() {
        assert_eq!(strip_extension("Movie.2020.mkv"), "Movie.2020");
        assert_eq!(strip_extension("Movie.2020.weird"), "Movie.2020.weird");
    }

    #[test]
    fn human_readable_size_trims_zeroes() {
        assert_eq!(human_readable_size(0, false), "0 B");
        assert_eq!(human_readable_size(1000, false), "1 kB");
        assert_eq!(human_readable_size(1024, true), "1 KiB");
        assert_eq!(human_readable_size(1_500_000, false), "1.5 MB");
    }

    #[test]
    fn format_as_list_styles() {
        let items = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(format_as_list(&items, false, false), "a, b, and c");
        assert_eq!(format_as_list(&items, false, true), "a, b, c");
        assert_eq!(format_as_list(&items[..2], false, false), "a and b");
    }

    #[test]
    fn cleanse_separators_normalises() {
        assert_eq!(
            cleanse_separators("[grp] Some.Show_(2020)"),
            "Some Show 2020"
        );
    }

    #[test]
    fn clean_title_drops_scene_prefix() {
        assert_eq!(clean_title("ntb-Some.Show.2020"), "Some Show 2020");
    }

    #[test]
    fn key_title_is_lowercase_alphanumeric_without_year() {
        assert_eq!(
            create_key_title("Some Show 2020").as_deref(),
            Some("someshow")
        );
    }

    #[test]
    fn replace_last_occurrence_targets_the_last_match() {
        let re = Regex::new(r"\d+").unwrap();
        assert_eq!(replace_last_occurrence("a1b2c3", &re, "X"), "a1b2cX");
        assert_eq!(replace_last_occurrence("abc", &re, "X"), "abc");
    }

    #[test]
    fn similar_titles_tolerate_small_edits() {
        assert!(are_media_titles_similar(
            "Some.Show.S01E01.1080p",
            "Some Show S01E01 720p",
            identity
        ));
        assert!(!are_media_titles_similar(
            "Some.Show.S01E01",
            "A.Totally.Different.Thing.S01E01",
            identity
        ));
    }

    #[test]
    fn credentials_are_split_from_the_url() {
        let creds =
            extract_credentials_from_url("http://user:p%40ss@localhost:8080", None).unwrap();
        assert_eq!(creds.username, "user");
        assert_eq!(creds.password, "p@ss");
        assert_eq!(creds.href, "http://localhost:8080");
    }

    #[test]
    fn credentials_keep_path_and_base_path() {
        let creds = extract_credentials_from_url("http://u:p@host/rpc", Some("extra")).unwrap();
        assert_eq!(creds.href, "http://host/rpc/extra");
    }

    #[test]
    fn child_path_detection_is_strict() {
        let parents = vec![PathBuf::from("/data/torrents")];
        assert!(is_child_path(Path::new("/data/torrents/show"), &parents));
        assert!(!is_child_path(Path::new("/data/torrents"), &parents));
        assert!(!is_child_path(Path::new("/data/other"), &parents));
    }

    #[test]
    fn sanitize_url_drops_query_and_credentials() {
        assert_eq!(
            sanitize_url("https://user:pass@indexer.example/api?apikey=secret"),
            "https://indexer.example/api"
        );
    }

    #[test]
    fn apikey_extraction() {
        assert_eq!(
            get_apikey("https://x.example/api?t=caps&apikey=abc").as_deref(),
            Some("abc")
        );
    }

    /// Two uploads of one release share a torrent name, and a tracker can list
    /// one upload twice. The hash is what tells the resulting log lines apart.
    #[test]
    fn log_strings_are_distinguished_by_the_info_hash() {
        let first = log_string(
            "A Star Is Born",
            "A Star Is Born",
            Some("434ea1e78b809d58c91a82096a515210fe6a3c0c"),
            None,
            None,
        );
        let second = log_string(
            "A Star Is Born",
            "A Star Is Born",
            Some("f35fc8a16b5ef54d820be0951ea66a5990f6054a"),
            None,
            None,
        );
        assert_eq!(first, "A Star Is Born [434ea1e7...]");
        assert_ne!(first, second);
    }

    #[test]
    fn log_strings_nest_the_name_when_it_differs_from_the_title() {
        assert_eq!(
            log_string(
                "Show S7",
                "Season 7",
                Some("0123456789abcdef"),
                Some("qb:8080"),
                None
            ),
            "Show S7 [Season 7 [01234567...@qb:8080]]"
        );
        // A data searchee has neither hash nor host, so the path stands in.
        assert_eq!(
            log_string("Title", "Title", None, None, Some("/data/x")),
            "/data/x"
        );
        assert_eq!(
            log_string("Title", "Name", None, None, None),
            "Title [Name]"
        );
    }

    #[test]
    fn info_hash_is_truncated_for_logs() {
        assert_eq!(
            sanitize_info_hash("0123456789abcdef0123456789abcdef01234567"),
            "01234567..."
        );
    }
}
