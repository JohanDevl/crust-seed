//! Sonarr/Radarr lookups.
//!
//! Ported from `arr.ts`. The arrs are asked to *parse* a release title; when
//! they recognise it they return the external IDs (imdb/tmdb/tvdb/tvmaze),
//! which turn a fuzzy text search into an exact ID search against indexers that
//! support one.

use serde::{Deserialize, Serialize};

use crate::config::runtime::get_runtime_config;
use crate::constants::{MediaType, SCENE_TITLE_REGEX};
use crate::http::{body_sample, client};
use crate::indexers::Caps;
use crate::logger::Label;
use crate::problems::{Problem, ProblemSeverity};
use crate::utils::{
    capture_group, cleanse_separators, get_apikey, join_posix, sanitize_url, strip_meta_from_name,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tvdb_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tv_maze_id: Option<String>,
}

impl ExternalIds {
    pub fn any(&self) -> bool {
        [
            &self.imdb_id,
            &self.tmdb_id,
            &self.tvdb_id,
            &self.tv_maze_id,
        ]
        .iter()
        .any(|id| id.as_deref().is_some_and(|v| !v.is_empty()))
    }

    /// Arrs return `0` for "no id"; the original rewrote those to `undefined`
    /// before deciding whether anything was found.
    fn normalize(&mut self) {
        for id in [
            &mut self.imdb_id,
            &mut self.tmdb_id,
            &mut self.tvdb_id,
            &mut self.tv_maze_id,
        ] {
            if id.as_deref() == Some("0") || id.as_deref() == Some("") {
                *id = None;
            }
        }
    }
}

pub fn arr_ids_equal(a: Option<&ExternalIds>, b: Option<&ExternalIds>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedEpisode {
    pub season_number: i64,
    pub episode_number: i64,
}

/// The `/api/v3/parse` response. Radarr fills `movie`, Sonarr fills
/// `series` + `episodes`; exactly one of the two is present.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedMedia {
    #[serde(default)]
    pub movie: Option<ExternalIds>,
    #[serde(default)]
    pub series: Option<ExternalIds>,
    #[serde(default)]
    pub episodes: Vec<ParsedEpisode>,
}

impl ParsedMedia {
    pub fn ids(&self) -> Option<&ExternalIds> {
        self.movie.as_ref().or(self.series.as_ref())
    }
}

/// The `tvdbid`/`tmdbid`/… query parameters for a Torznab ID search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdSearchParams {
    pub tvdbid: Option<String>,
    pub tmdbid: Option<String>,
    pub imdbid: Option<String>,
    pub tvmazeid: Option<String>,
}

impl IdSearchParams {
    pub fn any(&self) -> bool {
        [&self.tvdbid, &self.tmdbid, &self.imdbid, &self.tvmazeid]
            .iter()
            .any(|v| v.as_deref().is_some_and(|s| !s.is_empty()))
    }

    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        [
            ("tvdbid", &self.tvdbid),
            ("tmdbid", &self.tmdbid),
            ("imdbid", &self.imdbid),
            ("tvmazeid", &self.tvmazeid),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.clone().map(|v| (key, v)))
        .collect()
    }
}

/// `getRelevantArrIds` — only forwards an ID the indexer actually advertises
/// support for, otherwise the query silently returns nothing.
pub fn get_relevant_arr_ids(caps: &Caps, parsed_media: &ParsedMedia) -> IdSearchParams {
    let id_search_caps = if parsed_media.movie.is_some() {
        &caps.movie_id_search
    } else {
        &caps.tv_id_search
    };
    let Some(ids) = parsed_media.ids() else {
        return IdSearchParams::default();
    };
    IdSearchParams {
        tvdbid: id_search_caps
            .tvdb_id
            .unwrap_or(false)
            .then(|| ids.tvdb_id.clone())
            .flatten(),
        tmdbid: id_search_caps
            .tmdb_id
            .unwrap_or(false)
            .then(|| ids.tmdb_id.clone())
            .flatten(),
        imdbid: id_search_caps
            .imdb_id
            .unwrap_or(false)
            .then(|| ids.imdb_id.clone())
            .flatten(),
        tvmazeid: id_search_caps
            .tv_maze_id
            .unwrap_or(false)
            .then(|| ids.tv_maze_id.clone())
            .flatten(),
    }
}

pub fn format_found_ids(ids: &ExternalIds) -> String {
    [
        ("IMDB", &ids.imdb_id),
        ("TMDB", &ids.tmdb_id),
        ("TVDB", &ids.tvdb_id),
        ("TVMAZE", &ids.tv_maze_id),
    ]
    .iter()
    .map(|(name, value)| format!("{name}={}", value.as_deref().unwrap_or("N/A")))
    .collect::<Vec<_>>()
    .join(" ")
}

/// Builds `<origin><path>/<resource>?apikey=…` from a configured arr URL, which
/// carries its API key as a query parameter.
fn arr_request_url(uarrl: &str, resource_path: &str) -> Result<url::Url, String> {
    let mut url = url::Url::parse(&sanitize_url(uarrl)).map_err(|e| e.to_string())?;
    let joined = join_posix(url.path(), resource_path);
    url.set_path(&joined);
    Ok(url)
}

async fn make_arr_api_call<T: for<'de> Deserialize<'de>>(
    uarrl: &str,
    resource_path: &str,
    params: &[(&str, String)],
) -> Result<T, String> {
    let apikey = get_apikey(uarrl).unwrap_or_default();
    let mut url = arr_request_url(uarrl, resource_path)?;
    {
        let mut query = url.query_pairs_mut();
        query.clear();
        for (name, value) in params {
            query.append_pair(name, value);
        }
    }
    if url.query() == Some("") {
        url.set_query(None);
    }

    let response = client()
        .get(url)
        .header("X-Api-Key", apikey)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "connection timeout".to_string()
            } else {
                e.to_string()
            }
        })?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let label = if status.as_u16() == 401 {
            "401 Unauthorized (check apikey)".to_string()
        } else {
            status.as_u16().to_string()
        };
        return Err(format!(
            "{label} {} {}",
            status.canonical_reason().unwrap_or(""),
            body_sample(&text)
        ));
    }
    serde_json::from_str(&text)
        .map_err(|_| format!("Arr response was non-JSON. {}", body_sample(&text)))
}

fn get_relevant_arr_instances(media_type: MediaType) -> Vec<String> {
    let config = get_runtime_config();
    match media_type {
        MediaType::Season | MediaType::Episode => config.sonarr.clone(),
        MediaType::Movie => config.radarr.clone(),
        MediaType::Anime | MediaType::Video => config
            .sonarr
            .iter()
            .chain(config.radarr.iter())
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// Asks each relevant arr to parse `searchee_title`, returning the first
/// response that carries at least one non-zero external ID.
pub async fn scan_all_arrs_for_media(
    searchee_title: &str,
    media_type: MediaType,
) -> Option<ParsedMedia> {
    let uarrls = get_relevant_arr_instances(media_type);
    if uarrls.is_empty() {
        return None;
    }

    // A VIDEO searchee has no reliable structure, so it is stripped harder
    // before being handed to the arr.
    let title = if media_type != MediaType::Video {
        capture_group(&SCENE_TITLE_REGEX, searchee_title, "title")
            .unwrap_or_else(|| searchee_title.to_string())
    } else {
        cleanse_separators(&strip_meta_from_name(searchee_title))
    };

    let config = get_runtime_config();
    let mut last_error = format!(
        "No ids found for {title} | MediaType: {}",
        media_type.as_str().to_uppercase()
    );

    for uarrl in &uarrls {
        // Sonarr's parse endpoint rejects a bare series name, so a dummy
        // season/episode is appended for the VIDEO case.
        let name = if media_type == MediaType::Video && config.sonarr.contains(uarrl) {
            format!("{title} S00E00")
        } else {
            title.clone()
        };

        match make_arr_api_call::<ParsedMedia>(uarrl, "/api/v3/parse", &[("title", name.clone())])
            .await
        {
            Err(message) => {
                last_error = message;
                continue;
            }
            Ok(mut response) => {
                if let Some(ids) = response.movie.as_mut() {
                    ids.normalize();
                }
                if let Some(ids) = response.series.as_mut() {
                    ids.normalize();
                }
                if response.ids().is_some_and(ExternalIds::any) {
                    let label = if response.movie.is_some() {
                        Label::Radarr
                    } else {
                        Label::Sonarr
                    };
                    tracing::debug!(
                        label = label.as_str(),
                        "Found media for {name} -> {}",
                        format_found_ids(response.ids().unwrap())
                    );
                    return Some(response);
                }
            }
        }
    }

    tracing::debug!(
        label = Label::Arrs.as_str(),
        "Lookup failed for {searchee_title} - {last_error} - make sure the item is added to an Arr instance."
    );
    None
}

// ─── Health problems ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrKind {
    Sonarr,
    Radarr,
}

impl ArrKind {
    fn as_str(self) -> &'static str {
        match self {
            ArrKind::Sonarr => "Sonarr",
            ArrKind::Radarr => "Radarr",
        }
    }
}

/// Strips a trailing `/api` so `/api` can be appended for the version probe.
fn normalize_arr_base_url(raw_url: &str) -> String {
    match url::Url::parse(raw_url) {
        Ok(mut parsed) => {
            let path = parsed.path().trim_end_matches('/').to_string();
            let stripped = path.strip_suffix("/api").unwrap_or(&path).to_string();
            parsed.set_path(if stripped.is_empty() { "/" } else { &stripped });
            parsed.to_string()
        }
        Err(_) => raw_url.to_string(),
    }
}

async fn check_arr_url(raw_url: &str, index: usize, kind: ArrKind) -> Vec<Problem> {
    let kind_name = kind.as_str();
    let problem_id =
        |category: &str| format!("arr:{}:{category}:{index}", kind_name.to_lowercase());

    let Ok(parsed_url) = url::Url::parse(raw_url) else {
        return vec![Problem::new(
            problem_id("invalid-url"),
            ProblemSeverity::Error,
            format!("{kind_name} URL {} is invalid.", index + 1),
            "Could not be parsed as a URL.",
        )];
    };
    let display_url = sanitize_url(raw_url);

    if get_apikey(raw_url).is_none_or(|k| k.is_empty()) {
        let _ = &parsed_url;
        return vec![Problem::new(
            problem_id("missing-apikey"),
            ProblemSeverity::Error,
            format!("{kind_name} URL is missing an apikey parameter."),
            format!("Add ?apikey=<KEY> (or &apikey when other parameters exist) to {display_url}."),
        )];
    }

    #[derive(Deserialize)]
    struct ApiVersion {
        #[serde(default)]
        current: Option<String>,
    }

    match make_arr_api_call::<ApiVersion>(&normalize_arr_base_url(raw_url), "/api", &[]).await {
        Ok(body) if body.current.as_deref().is_some_and(|v| !v.is_empty()) => Vec::new(),
        Ok(_) => vec![Problem::new(
            problem_id("unexpected-response"),
            ProblemSeverity::Warning,
            format!("{kind_name} at {display_url} returned an unexpected response."),
            "crust-seed expected a version string from /api but received something else.",
        )],
        Err(message) => vec![Problem::new(
            problem_id("http-error"),
            ProblemSeverity::Error,
            format!("{kind_name} at {display_url} could not be reached."),
            message,
        )],
    }
}

pub async fn collect_arr_problems() -> Result<Vec<Problem>, String> {
    let config = get_runtime_config();
    if config.sonarr.is_empty() && config.radarr.is_empty() {
        return Ok(vec![Problem::new(
            "arr:not-configured",
            ProblemSeverity::Info,
            "Sonarr/Radarr integrations are not configured.",
            "Configure Arr URLs for more accurate tracker searches.",
        )]);
    }

    let checks = config
        .sonarr
        .iter()
        .enumerate()
        .map(|(index, url)| (url.clone(), index, ArrKind::Sonarr))
        .chain(
            config
                .radarr
                .iter()
                .enumerate()
                .map(|(index, url)| (url.clone(), index, ArrKind::Radarr)),
        );

    let results = futures::future::join_all(
        checks.map(|(url, index, kind)| async move { check_arr_url(&url, index, kind).await }),
    )
    .await;

    Ok(results.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexers::IdSearchCaps;

    #[test]
    fn zero_ids_are_treated_as_missing() {
        let mut ids = ExternalIds {
            imdb_id: Some("0".into()),
            tvdb_id: Some("12345".into()),
            ..Default::default()
        };
        ids.normalize();
        assert_eq!(ids.imdb_id, None);
        assert_eq!(ids.tvdb_id.as_deref(), Some("12345"));
        assert!(ids.any());
    }

    #[test]
    fn only_ids_the_indexer_supports_are_forwarded() {
        let mut caps = Caps::all();
        caps.tv_id_search = IdSearchCaps {
            tvdb_id: Some(true),
            ..Default::default()
        };
        let parsed = ParsedMedia {
            series: Some(ExternalIds {
                tvdb_id: Some("111".into()),
                imdb_id: Some("tt222".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ids = get_relevant_arr_ids(&caps, &parsed);
        assert_eq!(ids.tvdbid.as_deref(), Some("111"));
        assert_eq!(ids.imdbid, None);
        assert!(ids.any());
    }

    #[test]
    fn movie_media_uses_the_movie_id_caps() {
        let mut caps = Caps::all();
        caps.movie_id_search = IdSearchCaps {
            imdb_id: Some(true),
            ..Default::default()
        };
        let parsed = ParsedMedia {
            movie: Some(ExternalIds {
                imdb_id: Some("tt1".into()),
                tvdb_id: Some("9".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ids = get_relevant_arr_ids(&caps, &parsed);
        assert_eq!(ids.imdbid.as_deref(), Some("tt1"));
        assert_eq!(ids.tvdbid, None);
    }

    #[test]
    fn arr_urls_are_normalised_for_the_version_probe() {
        assert_eq!(
            normalize_arr_base_url("http://sonarr:8989/api?apikey=k"),
            "http://sonarr:8989/?apikey=k"
        );
        assert_eq!(
            normalize_arr_base_url("http://sonarr:8989/sonarr?apikey=k"),
            "http://sonarr:8989/sonarr?apikey=k"
        );
    }

    #[test]
    fn resource_paths_append_to_a_base_path() {
        let url = arr_request_url("http://sonarr:8989/sonarr?apikey=k", "/api/v3/parse").unwrap();
        assert_eq!(url.path(), "/sonarr/api/v3/parse");
    }

    #[test]
    fn found_ids_render_missing_values_as_na() {
        let ids = ExternalIds {
            tvdb_id: Some("42".into()),
            ..Default::default()
        };
        assert_eq!(
            format_found_ids(&ids),
            "IMDB=N/A TMDB=N/A TVDB=42 TVMAZE=N/A"
        );
    }

    #[test]
    fn id_equality_ignores_ordering_but_not_absence() {
        let a = ExternalIds {
            tvdb_id: Some("1".into()),
            ..Default::default()
        };
        assert!(arr_ids_equal(Some(&a), Some(&a.clone())));
        assert!(!arr_ids_equal(Some(&a), None));
        assert!(arr_ids_equal(None, None));
    }
}
