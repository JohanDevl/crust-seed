//! Program-wide constants: release-name regexes, file-extension tables and the
//! enums shared by the matching pipeline.
//!
//! Ported from `cross-seed/src/constants.ts` and `shared/constants.ts`.
//!
//! ## A note on the regexes
//!
//! The originals are JavaScript literals that lean on lookahead/lookbehind, so
//! they are compiled with `fancy-regex` rather than the `regex` crate. Two of
//! them (`EP_REGEX`, `BAD_EP_REGEX`) additionally used a *variable-width*
//! lookbehind — `(?<=S\d+[_\s-]{1,3})` — which `fancy-regex` cannot express.
//! Those are rewritten as an explicit two-branch alternation that accepts the
//! same language; see [`EP_REGEX`] for the derivation. Because a branch cannot
//! reuse a capture name, the episode/season groups are suffixed `1`/`2` and
//! merged by [`crate::utils::match_episode`].

use std::sync::LazyLock;

use fancy_regex::Regex;
use serde::{Deserialize, Serialize};

pub const PROGRAM_NAME: &str = "crust-seed";
pub const PROGRAM_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TORRENT_TAG: &str = "cross-seed";
pub const TORRENT_CATEGORY_SUFFIX: &str = ".cross-seed";
pub const NEWLINE_INDENT: &str = "\n\t\t\t\t";

pub fn user_agent() -> String {
    format!("CrossSeed/{PROGRAM_VERSION}")
}

// ─── Enums shared with the web UI ───────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Save,
    Inject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    Strict,
    Flexible,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkType {
    #[serde(rename = "symlink")]
    Symlink,
    #[serde(rename = "hardlink")]
    Hardlink,
    #[serde(rename = "reflink")]
    Reflink,
    #[serde(rename = "reflinkOrCopy")]
    ReflinkOrCopy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlocklistType {
    #[serde(rename = "name")]
    Name,
    #[serde(rename = "nameRegex")]
    NameRegex,
    #[serde(rename = "folder")]
    Folder,
    #[serde(rename = "folderRegex")]
    FolderRegex,
    #[serde(rename = "category")]
    Category,
    #[serde(rename = "tag")]
    Tag,
    #[serde(rename = "tracker")]
    Tracker,
    #[serde(rename = "infoHash")]
    InfoHash,
    #[serde(rename = "sizeBelow")]
    SizeBelow,
    #[serde(rename = "sizeAbove")]
    SizeAbove,
    #[serde(rename = "legacy")]
    Legacy,
}

impl BlocklistType {
    pub const ALL: [BlocklistType; 11] = [
        BlocklistType::Name,
        BlocklistType::NameRegex,
        BlocklistType::Folder,
        BlocklistType::FolderRegex,
        BlocklistType::Category,
        BlocklistType::Tag,
        BlocklistType::Tracker,
        BlocklistType::InfoHash,
        BlocklistType::SizeBelow,
        BlocklistType::SizeAbove,
        BlocklistType::Legacy,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BlocklistType::Name => "name",
            BlocklistType::NameRegex => "nameRegex",
            BlocklistType::Folder => "folder",
            BlocklistType::FolderRegex => "folderRegex",
            BlocklistType::Category => "category",
            BlocklistType::Tag => "tag",
            BlocklistType::Tracker => "tracker",
            BlocklistType::InfoHash => "infoHash",
            BlocklistType::SizeBelow => "sizeBelow",
            BlocklistType::SizeAbove => "sizeAbove",
            BlocklistType::Legacy => "legacy",
        }
    }

    pub fn from_str_exact(s: &str) -> Option<BlocklistType> {
        BlocklistType::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Episode,
    #[serde(rename = "pack")]
    Season,
    Movie,
    Anime,
    Video,
    Audio,
    Book,
    #[serde(rename = "unknown")]
    Other,
}

impl MediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaType::Episode => "episode",
            MediaType::Season => "pack",
            MediaType::Movie => "movie",
            MediaType::Anime => "anime",
            MediaType::Video => "video",
            MediaType::Audio => "audio",
            MediaType::Book => "book",
            MediaType::Other => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InjectionResult {
    #[serde(rename = "INJECTED")]
    Success,
    #[serde(rename = "FAILURE")]
    Failure,
    #[serde(rename = "ALREADY_EXISTS")]
    AlreadyExists,
    #[serde(rename = "TORRENT_NOT_COMPLETE")]
    TorrentNotComplete,
}

impl InjectionResult {
    pub fn as_str(self) -> &'static str {
        match self {
            InjectionResult::Success => "INJECTED",
            InjectionResult::Failure => "FAILURE",
            InjectionResult::AlreadyExists => "ALREADY_EXISTS",
            InjectionResult::TorrentNotComplete => "TORRENT_NOT_COMPLETE",
        }
    }
}

/// `InjectionResult | SaveResult` in the original — `SaveResult` had a single
/// member, so it collapses into one enum here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionResult {
    Saved,
    Injection(InjectionResult),
}

impl ActionResult {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionResult::Saved => "SAVED",
            ActionResult::Injection(r) => r.as_str(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Decision {
    #[serde(rename = "MATCH")]
    Match,
    #[serde(rename = "MATCH_SIZE_ONLY")]
    MatchSizeOnly,
    #[serde(rename = "MATCH_PARTIAL")]
    MatchPartial,
    #[serde(rename = "FUZZY_SIZE_MISMATCH")]
    FuzzySizeMismatch,
    #[serde(rename = "SIZE_MISMATCH")]
    SizeMismatch,
    #[serde(rename = "PARTIAL_SIZE_MISMATCH")]
    PartialSizeMismatch,
    #[serde(rename = "NO_DOWNLOAD_LINK")]
    NoDownloadLink,
    #[serde(rename = "DOWNLOAD_FAILED")]
    DownloadFailed,
    #[serde(rename = "MAGNET_LINK")]
    MagnetLink,
    #[serde(rename = "RATE_LIMITED")]
    RateLimited,
    /// Searchee and candidate infoHash match. Usually public torrents, or
    /// torrents added by radarr/sonarr before cross-seed saw the announce.
    /// The inject job ignores `InfoHashAlreadyExists`, and the announce route
    /// reports 204 instead of 200 when this is the outcome.
    #[serde(rename = "SAME_INFO_HASH")]
    SameInfoHash,
    /// Checked after [`Decision::SameInfoHash`].
    #[serde(rename = "INFO_HASH_ALREADY_EXISTS")]
    InfoHashAlreadyExists,
    #[serde(rename = "FILE_TREE_MISMATCH")]
    FileTreeMismatch,
    #[serde(rename = "RELEASE_GROUP_MISMATCH")]
    ReleaseGroupMismatch,
    #[serde(rename = "BLOCKED_RELEASE")]
    BlockedRelease,
    #[serde(rename = "PROPER_REPACK_MISMATCH")]
    ProperRepackMismatch,
    #[serde(rename = "RESOLUTION_MISMATCH")]
    ResolutionMismatch,
    #[serde(rename = "SOURCE_MISMATCH")]
    SourceMismatch,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Match => "MATCH",
            Decision::MatchSizeOnly => "MATCH_SIZE_ONLY",
            Decision::MatchPartial => "MATCH_PARTIAL",
            Decision::FuzzySizeMismatch => "FUZZY_SIZE_MISMATCH",
            Decision::SizeMismatch => "SIZE_MISMATCH",
            Decision::PartialSizeMismatch => "PARTIAL_SIZE_MISMATCH",
            Decision::NoDownloadLink => "NO_DOWNLOAD_LINK",
            Decision::DownloadFailed => "DOWNLOAD_FAILED",
            Decision::MagnetLink => "MAGNET_LINK",
            Decision::RateLimited => "RATE_LIMITED",
            Decision::SameInfoHash => "SAME_INFO_HASH",
            Decision::InfoHashAlreadyExists => "INFO_HASH_ALREADY_EXISTS",
            Decision::FileTreeMismatch => "FILE_TREE_MISMATCH",
            Decision::ReleaseGroupMismatch => "RELEASE_GROUP_MISMATCH",
            Decision::BlockedRelease => "BLOCKED_RELEASE",
            Decision::ProperRepackMismatch => "PROPER_REPACK_MISMATCH",
            Decision::ResolutionMismatch => "RESOLUTION_MISMATCH",
            Decision::SourceMismatch => "SOURCE_MISMATCH",
        }
    }

    pub fn from_str_exact(s: &str) -> Option<Decision> {
        use Decision::*;
        Some(match s {
            "MATCH" => Match,
            "MATCH_SIZE_ONLY" => MatchSizeOnly,
            "MATCH_PARTIAL" => MatchPartial,
            "FUZZY_SIZE_MISMATCH" => FuzzySizeMismatch,
            "SIZE_MISMATCH" => SizeMismatch,
            "PARTIAL_SIZE_MISMATCH" => PartialSizeMismatch,
            "NO_DOWNLOAD_LINK" => NoDownloadLink,
            "DOWNLOAD_FAILED" => DownloadFailed,
            "MAGNET_LINK" => MagnetLink,
            "RATE_LIMITED" => RateLimited,
            "SAME_INFO_HASH" => SameInfoHash,
            "INFO_HASH_ALREADY_EXISTS" => InfoHashAlreadyExists,
            "FILE_TREE_MISMATCH" => FileTreeMismatch,
            "RELEASE_GROUP_MISMATCH" => ReleaseGroupMismatch,
            "BLOCKED_RELEASE" => BlockedRelease,
            "PROPER_REPACK_MISMATCH" => ProperRepackMismatch,
            "RESOLUTION_MISMATCH" => ResolutionMismatch,
            "SOURCE_MISMATCH" => SourceMismatch,
            _ => return None,
        })
    }

    pub fn is_any_match(self) -> bool {
        matches!(
            self,
            Decision::Match | Decision::MatchSizeOnly | Decision::MatchPartial
        )
    }

    /// Decisions that can never change for a given (searchee, candidate) pair,
    /// so a cached row is authoritative forever.
    pub fn is_static(self) -> bool {
        matches!(
            self,
            Decision::ReleaseGroupMismatch
                | Decision::ResolutionMismatch
                | Decision::SourceMismatch
                | Decision::ProperRepackMismatch
                | Decision::MagnetLink
        )
    }
}

pub const ANY_MATCH_DECISIONS: [&str; 3] = ["MATCH", "MATCH_SIZE_ONLY", "MATCH_PARTIAL"];

// ─── Release-name regexes ───────────────────────────────────────────────────

macro_rules! lazy_regex {
    ($name:ident, $pattern:expr) => {
        pub static $name: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new($pattern)
                .unwrap_or_else(|e| panic!("invalid regex {}: {e}", stringify!($name)))
        });
    };
}

// Episode / dated-episode detector.
//
// The JS original is:
// ```text
// ^(?<title>.+?)[_.\s-]+(?:(?<season>S\d+)?[_.\s-]{0,3}(?!(?:19|20)\d{2})
//   (?<episode>(?:E|(?<=S\d+[_\s-]{1,3}))\d+(?:[\s-]?(?!(?:19|20)\d{2})E?\d+)?(?![pix]))
//   (?!\d+[pix])|(?<date>...))
// ```
// The inner `(?:E|(?<=S\d+[_\s-]{1,3}))` says "the episode number is either
// introduced by an `E`, or is bare digits immediately preceded by a season
// token and 1–3 separators". That lookbehind is variable-width. Splitting it
// into two alternatives — bare digits *after a consumed* `S\d+[_\s-]{1,3}`,
// versus an explicit `E`-prefixed number — accepts exactly the same strings,
// because the lookbehind could only ever succeed when the optional season
// group had matched and `[_.\s-]{0,3}` had consumed 1–3 non-dot separators.
lazy_regex!(
    EP_REGEX,
    r"(?i)^(?<title>.+?)[_.\s-]+(?:(?<season1>S\d+)[_\s-]{1,3}(?!(?:19|20)\d{2})(?<episode1>\d+(?:[\s-]?(?!(?:19|20)\d{2})E?\d+)?(?![pix]))(?!\d+[pix])|(?<season2>S\d+)?[_.\s-]{0,3}(?!(?:19|20)\d{2})(?<episode2>E\d+(?:[\s-]?(?!(?:19|20)\d{2})E?\d+)?(?![pix]))(?!\d+[pix])|(?<date>(?<year>(?:19|20)\d{2})[_.\s-](?<month>\d{2})[_.\s-](?<day>\d{2})))"
);

// Same shape as [`EP_REGEX`] but anchored to a leading garbage token — used to
// detect names whose "title" is really a release-group prefix.
lazy_regex!(
    BAD_EP_REGEX,
    r"(?i)^[_.\s-]*[^_.\s-]*?(?:(?<season1>S\d+)[_\s-]{1,3}(?!(?:19|20)\d{2})(?<episode1>\d+(?:[\s-]?(?!(?:19|20)\d{2})E?\d+)?(?![pix]))(?!\d+[pix])|(?<season2>S\d+)?[_.\s-]{0,3}(?!(?:19|20)\d{2})(?<episode2>E\d+(?:[\s-]?(?!(?:19|20)\d{2})E?\d+)?(?![pix]))(?!\d+[pix])|(?<date>(?<year>(?:19|20)\d{2})[_.\s-](?<month>\d{2})[_.\s-](?<day>\d{2})))"
);

lazy_regex!(
    IS_MULTI_EP_REGEX,
    r"(?i)E\d+(?:[-.]?S\d+E\d|[-.]?E\d|[-.]\d)"
);
lazy_regex!(
    SEASON_REGEX,
    r"(?i)^(?<title>.+?)[\[\(_.\s-]+(?<season>S(?:eason)?\s*\d+)(?=\b(?![_.\s~-]*E\d+))"
);
lazy_regex!(
    BAD_SEASON_REGEX,
    r"(?i)^[\[_.\s-]*[^\[_.\s-]*?(?<season>S\d+|(?:season)\s*\d+)(?=\b(?![_.\s~-]*E\d+))"
);
lazy_regex!(
    MOVIE_REGEX,
    r"(?i)^(?<title>.+?)-?[_.\s][\[\(]?(?<year>(?:18|19|20)\d{2})[\)\]]?(?![pix])"
);
lazy_regex!(
    ANIME_REGEX,
    r"(?i)^(?:\[(?<group>.*?)\][_\s-]?)?(?:\[?(?<title>.+?)[_\s-]?(?:\(?(?:\d{1,2}(?:st|nd|rd|th))?\s?Season)?[_\s-]?\]?)(?:[\(\[~/|-]\s?(?!\d{1,4})(?<altTitle>.+?)[\)\]~-]?\s?)?[_\s-]?(?:[\[\(]?(?<year>(?:19|20)\d{2})[\)\]]?)?[\[_\s-](?:S\d{1,2})?[_\s-]{0,3}(?:#|EP?|(?:SP))?[_\s-]{0,3}(?!\d+[a-uw-z])(?<release>\d{1,4})(?!\.[0-46-9])"
);
lazy_regex!(
    RELEASE_GROUP_REGEX,
    r"(?i)(?<=-)(?:\W|\b)(?!(?:\d{3,4}[ip]))(?!\d+\b)(?:\W|\b)(?<group>[\w .]+?)(?:\[.+\])?(?:\))?(?:\s\[.+\])?$"
);
lazy_regex!(ANIME_GROUP_REGEX, r"(?i)^\s*\[(?<group>.+?)\]");
lazy_regex!(
    EBOOK_AND_MUSIC_RELEASE_REGEX,
    r"(?i)(\d+k?\b|['\u{2019}]s\b|\[.*?\]|\(.*?\)|\{.*?\}|[kbps]{2,4}\b|m4b\b|pdf\b|docx\b|epub\b|mobi\b|azw3\b|m4a\b|mp3\b|flac|\sWEB\b|audiobook\b|\bebook\b|\bNew\b|[a\W]+(novela?|saga|series)|(Read|Narrated)?\W?By\W\w+\W*\w*\b|[^a-zA-Z0-9])"
);
lazy_regex!(
    RESOLUTION_REGEX,
    r"(?i)\b(?<res>\d{3,4}[pix](?:\d{3,4}[pi]?)?)\b"
);
lazy_regex!(RES_STRICT_REGEX, r"(?<res>(?:2160|1080|720)[pi])");
lazy_regex!(YEARS_REGEX, r"(?i)(?<year>(?:19|20)\d{2})(?![pix])");
lazy_regex!(
    REPACK_PROPER_REGEX,
    r"(?i)(?:\b(?<type>(?:REPACK|PROPER|\dv\d)\d?))\b"
);
lazy_regex!(ARR_PROPER_REGEX, r"(?:\b(?<arrtype>(?:Proper|\dv\d)))\b");
lazy_regex!(SCENE_TITLE_REGEX, r"^(?:[a-z0-9]{3,5}-)?(?<title>.*)");
lazy_regex!(
    ARR_DIR_REGEX,
    r"(?i)^(?<title>(?!.*(?:(\d{3,4}[ipx])|([xh.]+26[4-6])|(dvd)|(mpeg)|(xvid)|(?:(he)|a)vc|(?:uhd)|(?:blu[_.\s-]?ray)))[\p{L}\s:\w'\u{2019}!\(\);.,&\u{2013}+-]+(?:\(\d{4}\))?)(?<id>\s[\{\[](?:tm|tv|im)db(?:id)?-\w+?[\}\]])?$"
);
lazy_regex!(
    SONARR_SUBFOLDERS_REGEX,
    r"(?i)^(?:S(?:eason )?(?<seasonNum>\d{1,4}))$"
);
lazy_regex!(NON_UNICODE_ALPHANUM_REGEX, r"[^\p{L}\p{N}]+");
lazy_regex!(CALIBRE_INDEXNUM_REGEX, r"\s?\(\d+\)$");
lazy_regex!(INFO_HASH_REGEX, r"(?i)^[a-z0-9]{40}$");
lazy_regex!(
    SAVED_TORRENTS_INFO_REGEX,
    r"(?i)^\[(?<mediaType>.+?)\]\[(?<tracker>.+?)\](?<name>.+?)(?:\[(?<infoHash>[a-z0-9]{40})\])?(?<cached>\.cached)?\.torrent$"
);
lazy_regex!(
    BAD_GROUP_PARSE_REGEX,
    r"(?i)^(?<badmatch>(?:dl|DDP?|aac|eac3|atmos|dts|ma|hd|[heav.c]{3,5}|[xh.]{1,2}[2456]|[0-9]+[ip]?|dxva|full|blu|ray|s(?:eason)?\W\d+|\W){3,})$"
);
lazy_regex!(
    JSON_VALUES_REGEX,
    r#""(?s).+?"\s*:\s*"(?<value>.+?)"\s*(?:,|\})"#
);
lazy_regex!(ABS_WIN_PATH_REGEX, r"(?i)^[a-z]:|^\\");
lazy_regex!(
    AKA_REGEX,
    r"(?i)(?:[_.\s-]+|\b)a[_.\s-]?k[_.\s-]?a(?:[_.\s-]+|\b)"
);
lazy_regex!(ALL_SPACES_REGEX, r"\s+");
lazy_regex!(
    ALL_SQUARE_BRACKETS_REGEX,
    r"\[.*?\]|\u{300C}.*?\u{300D}|\u{FF62}.*?\u{FF63}|\u{3010}.*?\u{3011}"
);
lazy_regex!(ALL_PARENTHESES_REGEX, r"\(.*?\)");
lazy_regex!(
    PARSE_BLOCKLIST_REGEX,
    r"^(?<blocklistType>.+?):(?<blocklistValue>.*)$"
);

// ─── Streaming-source detection ─────────────────────────────────────────────

/// `(name, regex)` pairs, order-significant — the first hit wins, matching the
/// insertion order of the JS object literal.
static SOURCE_REGEXES: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    let specs: [(&str, &str); 8] = [
        (
            "AMZN",
            r"(?i)\b(amzn|amazon(hd)?)\b[ ._-]web[ ._-]?(dl|rip)?\b",
        ),
        ("DSNP", r"(?i)\b(dsnp|dsny|disney)\b"),
        ("NF", r"(?i)\b(nf|netflix(u?hd)?)\b"),
        ("HULU", r"(?i)\b(hulu)\b"),
        ("ATVP", r"(?i)\b(atvp|aptv)\b"),
        (
            "HBO",
            r"(?i)\b(hbo)(?![ ._-]max)\b|\b(hmax|hbom|hbo[ ._-]max)\b",
        ),
        ("PCOK", r"(?i)\b(pcok)\b"),
        ("PMTP", r"(?i)\b(pmtp|Paramount Plus)\b"),
    ];
    specs
        .into_iter()
        .map(|(name, pattern)| (name, Regex::new(pattern).expect("source regex")))
        .collect()
});

pub fn parse_source(title: &str) -> Option<&'static str> {
    SOURCE_REGEXES
        .iter()
        .find(|(_, re)| re.is_match(title).unwrap_or(false))
        .map(|(name, _)| *name)
}

/// Removes the first streaming-source token that actually shortens the title,
/// mirroring the JS early-return on length change.
pub fn source_regex_remove(title: &str) -> String {
    let original_len = title.len();
    for (_, re) in SOURCE_REGEXES.iter() {
        let new_title = re.replace(title, "");
        if new_title.len() != original_len {
            return new_title.into_owned();
        }
    }
    title.to_string()
}

// ─── File extensions ────────────────────────────────────────────────────────

pub const VIDEO_EXTENSIONS: &[&str] = &[
    // OG extensions
    ".mkv", ".mp4", ".avi", ".ts", // extensions from sonarr
    ".m4v", ".3gp", ".nsv", ".ty", ".strm", ".rm", ".rmvb", ".mov", ".qt", ".divx", ".xvid",
    ".bivx", ".pva", ".wmv", ".asf", ".asx", ".ogm", ".ogv", ".m2v", ".dvr-ms", ".mpg", ".mpeg",
    ".avc", ".vp3", ".svq3", ".nuv", ".viv", ".dv", ".fli", ".flv", ".wpl", ".wtv",
];
pub const VIDEO_DISC_EXTENSIONS: &[&str] = &[".m2ts", ".ifo", ".vob", ".bup"];
pub const AUDIO_EXTENSIONS: &[&str] = &[
    ".wav", ".aiff", ".alac", ".flac", ".ape", ".mp3", ".aac", ".m4a", ".m4b", ".m4p", ".ogg",
    ".wma", ".aa", ".aax",
];
pub const BOOK_EXTENSIONS: &[&str] = &[
    ".epub", ".mobi", ".azw", ".azw3", ".azw4", ".pdf", ".djvu", ".html", ".chm", ".cbr", ".cbz",
    ".cb7", ".cbt", ".cba",
];

pub static ALL_EXTENSIONS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
    VIDEO_EXTENSIONS
        .iter()
        .chain(AUDIO_EXTENSIONS)
        .chain(BOOK_EXTENSIONS)
        .chain(VIDEO_DISC_EXTENSIONS)
        .copied()
        .collect()
});

pub const TORRENT_CACHE_FOLDER: &str = "torrent_cache";
pub const LOGS_FOLDER: &str = "logs";
pub const UNKNOWN_TRACKER: &str = "UnknownTracker";
pub const MAX_PATH_BYTES: usize = 255;
pub const LEVENSHTEIN_DIVISOR: usize = 3;
pub const MIN_VIDEO_QUERY_LENGTH: usize = 3;

pub const IGNORED_FOLDERS_SUBSTRINGS: &[&str] = &[
    "sample",
    "proof",
    "bdmv",
    "bdrom",
    "certificate",
    "video_ts",
];
pub const RESUME_EXCLUDED_KEYWORDS: &[&str] = &["sample", "trailer", "extras", "bonus"];
pub const RESUME_EXCLUDED_EXTS: &[&str] = &[".nfo", ".srr", ".srt", ".txt", ".ass"];

/// Splits `"type:value"`; anything without a recognised prefix is treated as a
/// legacy bare-name entry.
pub fn parse_blocklist_entry(entry: &str) -> (BlocklistType, String) {
    if let Ok(Some(caps)) = PARSE_BLOCKLIST_REGEX.captures(entry) {
        let ty = caps.name("blocklistType").map(|m| m.as_str()).unwrap_or("");
        let value = caps
            .name("blocklistValue")
            .map(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        // Unknown prefixes stay unknown here; validation reports them.
        if let Some(parsed) = BlocklistType::from_str_exact(ty) {
            return (parsed, value);
        }
        return (BlocklistType::Legacy, entry.to_string());
    }
    (BlocklistType::Legacy, entry.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern must compile — `LazyLock` would otherwise only panic the
    /// first time a given regex is touched at runtime.
    #[test]
    fn all_regexes_compile() {
        for re in [
            &*EP_REGEX,
            &*BAD_EP_REGEX,
            &*IS_MULTI_EP_REGEX,
            &*SEASON_REGEX,
            &*BAD_SEASON_REGEX,
            &*MOVIE_REGEX,
            &*ANIME_REGEX,
            &*RELEASE_GROUP_REGEX,
            &*ANIME_GROUP_REGEX,
            &*EBOOK_AND_MUSIC_RELEASE_REGEX,
            &*RESOLUTION_REGEX,
            &*RES_STRICT_REGEX,
            &*YEARS_REGEX,
            &*REPACK_PROPER_REGEX,
            &*ARR_PROPER_REGEX,
            &*SCENE_TITLE_REGEX,
            &*ARR_DIR_REGEX,
            &*SONARR_SUBFOLDERS_REGEX,
            &*NON_UNICODE_ALPHANUM_REGEX,
            &*CALIBRE_INDEXNUM_REGEX,
            &*INFO_HASH_REGEX,
            &*SAVED_TORRENTS_INFO_REGEX,
            &*BAD_GROUP_PARSE_REGEX,
            &*JSON_VALUES_REGEX,
            &*ABS_WIN_PATH_REGEX,
            &*AKA_REGEX,
            &*ALL_SPACES_REGEX,
            &*ALL_SQUARE_BRACKETS_REGEX,
            &*ALL_PARENTHESES_REGEX,
            &*PARSE_BLOCKLIST_REGEX,
        ] {
            assert!(!re.as_str().is_empty());
        }
        assert!(!SOURCE_REGEXES.is_empty());
    }

    #[test]
    fn ep_regex_matches_sxxexx() {
        let caps = EP_REGEX
            .captures("Some.Show.S01E05.1080p.WEB-DL")
            .unwrap()
            .unwrap();
        assert_eq!(caps.name("title").unwrap().as_str(), "Some.Show");
        assert_eq!(caps.name("season2").unwrap().as_str(), "S01");
        assert_eq!(caps.name("episode2").unwrap().as_str(), "E05");
    }

    /// The rewritten branch that replaces the variable-width lookbehind.
    #[test]
    fn ep_regex_matches_bare_episode_after_season() {
        let caps = EP_REGEX
            .captures("Some Show S01 05 1080p")
            .unwrap()
            .unwrap();
        assert_eq!(caps.name("season1").unwrap().as_str(), "S01");
        assert_eq!(caps.name("episode1").unwrap().as_str(), "05");
    }

    /// A dot separator was excluded by the original lookbehind's `[_\s-]`
    /// class, so `S01.05` must NOT parse as season+episode.
    #[test]
    fn ep_regex_rejects_dot_separated_bare_episode() {
        let caps = EP_REGEX.captures("Some.Show.S01.05.1080p").unwrap();
        let bare = caps
            .as_ref()
            .and_then(|c| c.name("episode1").or_else(|| c.name("episode2")));
        assert!(bare.is_none(), "unexpected episode match: {caps:?}");
    }

    #[test]
    fn ep_regex_matches_date() {
        let caps = EP_REGEX
            .captures("Some.Show.2024.01.02.WEB")
            .unwrap()
            .unwrap();
        assert_eq!(caps.name("date").unwrap().as_str(), "2024.01.02");
    }

    #[test]
    fn source_detection() {
        assert_eq!(parse_source("Show.S01E01.AMZN.WEB-DL"), Some("AMZN"));
        assert_eq!(parse_source("Show.S01E01.NF.WEB-DL"), Some("NF"));
        assert_eq!(parse_source("Show.S01E01.WEB-DL"), None);
    }

    #[test]
    fn blocklist_entry_parsing() {
        assert_eq!(
            parse_blocklist_entry("category:movies"),
            (BlocklistType::Category, "movies".to_string())
        );
        assert_eq!(
            parse_blocklist_entry("just a name"),
            (BlocklistType::Legacy, "just a name".to_string())
        );
    }
}
