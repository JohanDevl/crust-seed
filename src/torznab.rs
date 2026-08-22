//! Torznab: querying indexers and reading their capabilities.
//!
//! Ported from `torznab.ts`. `xml2js` is replaced by `quick-xml`, driven
//! manually rather than through serde — Torznab responses put the interesting
//! data in attributes and in namespaced `<torznab:attr>` elements, and a
//! hand-written reader handles the shape variations across Prowlarr, Jackett
//! and NZBHydra without a struct per variant.

use std::collections::BTreeMap;

use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::arr::{IdSearchParams, ParsedMedia, get_relevant_arr_ids};
use crate::config::runtime::get_runtime_config;
use crate::constants::{
    CALIBRE_INDEXNUM_REGEX, EP_REGEX, MediaType, SEASON_REGEX, UNKNOWN_TRACKER,
};
use crate::errors::CrustSeedError;
use crate::http::{body_sample, client};
use crate::indexers::{
    Caps, IdSearchCaps, Indexer, IndexerCategories, IndexerLimits, IndexerStatus,
};
use crate::logger::Label;
use crate::searchee::{Searchee, media_type_of};
use crate::utils::{
    capture_group, clean_book_and_audio_title, clean_title, extract_int, get_anime_queries,
    get_video_queries, human_readable_date, match_episode, now_ms, reformat_title_for_searching,
    strip_extension,
};

/// A release returned by an indexer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub guid: String,
    pub name: String,
    pub tracker: String,
    pub link: String,
    pub size: i64,
    /// Epoch milliseconds; `None` when the indexer's `pubDate` was unparseable.
    pub pub_date: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Caps,
    Search,
    TvSearch,
    Movie,
}

impl QueryKind {
    fn as_str(self) -> &'static str {
        match self {
            QueryKind::Caps => "caps",
            QueryKind::Search => "search",
            QueryKind::TvSearch => "tvsearch",
            QueryKind::Movie => "movie",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub t: QueryKind,
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Season number, or a year for dated shows — hence a string.
    pub season: Option<String>,
    /// Episode number, or `MM/DD` for dated shows.
    pub ep: Option<String>,
    pub ids: IdSearchParams,
}

impl Query {
    pub fn new(t: QueryKind) -> Self {
        Query {
            t,
            q: None,
            limit: None,
            offset: None,
            season: None,
            ep: None,
            ids: IdSearchParams::default(),
        }
    }

    /// The query string parameters, in the order `assembleUrl` produced them.
    fn params(&self) -> Vec<(String, String)> {
        let mut params: Vec<(String, String)> = vec![("t".into(), self.t.as_str().into())];
        if let Some(q) = &self.q {
            params.push(("q".into(), q.clone()));
        }
        if let Some(limit) = self.limit {
            params.push(("limit".into(), limit.to_string()));
        }
        if let Some(offset) = self.offset {
            params.push(("offset".into(), offset.to_string()));
        }
        if let Some(season) = &self.season {
            params.push(("season".into(), season.clone()));
        }
        if let Some(ep) = &self.ep {
            params.push(("ep".into(), ep.clone()));
        }
        for (key, value) in self.ids.pairs() {
            params.push((key.to_string(), value));
        }
        params
    }
}

/// `assembleUrl(baseUrl, apikey, params)`.
pub fn assemble_url(base_url: &str, apikey: &str, query: &Query) -> Result<String, CrustSeedError> {
    let mut url = url::Url::parse(base_url)
        .map_err(|e| CrustSeedError::new(format!("invalid indexer URL {base_url}: {e}")))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.clear();
        pairs.append_pair("apikey", apikey);
        for (key, value) in query.params() {
            pairs.append_pair(&key, &value);
        }
    }
    Ok(url.to_string())
}

// ─── XML parsing ────────────────────────────────────────────────────────────

/// A flattened XML element: text content plus attributes, with children keyed
/// by local name. Torznab documents are shallow, so this is enough structure
/// to read both `<caps>` and an RSS `<item>`.
#[derive(Debug, Default, Clone)]
struct Element {
    text: String,
    attrs: BTreeMap<String, String>,
    children: BTreeMap<String, Vec<Element>>,
}

impl Element {
    fn child(&self, name: &str) -> Option<&Element> {
        self.children.get(name).and_then(|list| list.first())
    }

    fn children_named(&self, name: &str) -> &[Element] {
        self.children.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }

    fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).map(String::as_str)
    }
}

/// Strips an XML namespace prefix — Torznab mixes `torznab:attr` with plain
/// RSS elements and the prefix carries no information here.
fn local_name(qname: &[u8]) -> String {
    let name = String::from_utf8_lossy(qname);
    match name.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => name.into_owned(),
    }
}

fn parse_xml(input: &str) -> Result<Element, String> {
    let mut reader = quick_xml::Reader::from_str(input);
    reader.config_mut().trim_text(false);

    let mut root = Element::default();
    let mut stack: Vec<(String, Element)> = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => {
                return Err(format!(
                    "invalid XML at position {}: {e}",
                    reader.buffer_position()
                ));
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let mut element = Element::default();
                for attr in e.attributes().flatten() {
                    element.attrs.insert(
                        local_name(attr.key.as_ref()),
                        attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .unwrap_or_default()
                            .into_owned(),
                    );
                }
                stack.push((local_name(e.name().as_ref()), element));
            }
            Ok(Event::Empty(e)) => {
                let mut element = Element::default();
                for attr in e.attributes().flatten() {
                    element.attrs.insert(
                        local_name(attr.key.as_ref()),
                        attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                            .unwrap_or_default()
                            .into_owned(),
                    );
                }
                let name = local_name(e.name().as_ref());
                let parent = stack.last_mut().map(|(_, el)| el).unwrap_or(&mut root);
                parent.children.entry(name).or_default().push(element);
            }
            Ok(Event::Text(e)) => {
                if let Some((_, element)) = stack.last_mut() {
                    // Event::Text is still escaped in quick-xml; entities have
                    // to be resolved explicitly.
                    let raw = e.xml10_content().unwrap_or_default();
                    let text = quick_xml::escape::unescape(&raw)
                        .map(|c| c.into_owned())
                        .unwrap_or_else(|_| raw.into_owned());
                    element.text.push_str(&text);
                }
            }
            Ok(Event::CData(e)) => {
                if let Some((_, element)) = stack.last_mut() {
                    element
                        .text
                        .push_str(&String::from_utf8_lossy(e.into_inner().as_ref()));
                }
            }
            Ok(Event::End(_)) => {
                if let Some((name, element)) = stack.pop() {
                    let parent = stack.last_mut().map(|(_, el)| el).unwrap_or(&mut root);
                    parent.children.entry(name).or_default().push(element);
                }
            }
            _ => {}
        }
        buf.clear();
    }

    // quick-xml stops at EOF without complaining about unclosed elements,
    // whereas xml2js rejected. cross-seed reports "invalid XML" to the user on
    // that rejection, so the check has to be explicit here.
    if !stack.is_empty() {
        return Err(format!(
            "unclosed element <{}>",
            stack.last().map(|(name, _)| name.as_str()).unwrap_or("?")
        ));
    }
    if root.children.is_empty() {
        return Err("document has no root element".to_string());
    }

    Ok(root)
}

/// `parseTorznabResults`.
pub fn parse_torznab_results(xml: &str, indexer_id: i64) -> Result<Vec<Candidate>, String> {
    let root = parse_xml(xml)?;
    let Some(channel) = root.child("rss").and_then(|rss| rss.child("channel")) else {
        return Ok(Vec::new());
    };

    Ok(channel
        .children_named("item")
        .iter()
        .filter_map(|item| {
            // Prowlarr, Jackett and NZBHydra each name the tracker element
            // differently; the original tried all three in this order.
            let tracker = ["prowlarrindexer", "jackettindexer", "indexer"]
                .iter()
                .find_map(|name| item.text_of(name))
                .filter(|t| !t.is_empty())
                .unwrap_or(UNKNOWN_TRACKER)
                .trim()
                .to_string();

            Some(Candidate {
                guid: item.text_of("guid")?.to_string(),
                name: item.text_of("title")?.to_string(),
                tracker,
                link: item.text_of("link").unwrap_or_default().to_string(),
                size: item
                    .text_of("size")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                pub_date: item.text_of("pubDate").and_then(parse_rfc2822_ms),
                indexer_id: Some(indexer_id),
            })
        })
        .collect())
}

/// RSS `pubDate` is RFC 2822; some indexers emit RFC 3339 instead. `new Date()`
/// accepted both, so both are accepted here.
fn parse_rfc2822_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(value)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// `parseTorznabCaps`.
pub fn parse_torznab_caps(xml: &str) -> Result<Caps, String> {
    let root = parse_xml(xml)?;
    let caps = root.child("caps");

    let limits = caps
        .and_then(|c| c.child("limits"))
        .map(|limit| IndexerLimits {
            default: limit
                .attr("default")
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),
            max: limit
                .attr("max")
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),
        })
        .unwrap_or_default();

    let searching = caps.and_then(|c| c.child("searching"));
    let is_available = |name: &str| {
        searching
            .and_then(|s| s.child(name))
            .and_then(|t| t.attr("available"))
            .is_some_and(|v| v == "yes")
    };
    let supported_ids = |name: &str| -> IdSearchCaps {
        let params = searching
            .and_then(|s| s.child(name))
            .and_then(|t| t.attr("supportedParams"))
            .unwrap_or_default()
            .to_string();
        let ids: Vec<&str> = params
            .split(',')
            .map(str::trim)
            .filter(|token| token.contains("id"))
            .collect();
        IdSearchCaps {
            tvdb_id: Some(ids.contains(&"tvdbid")),
            tmdb_id: Some(ids.contains(&"tmdbid")),
            imdb_id: Some(ids.contains(&"imdbid")),
            tv_maze_id: Some(ids.contains(&"tvmazeid")),
        }
    };

    let mut categories = IndexerCategories::default();
    if let Some(category_root) = caps.and_then(|c| c.child("categories")) {
        for category in category_root.children_named("category") {
            let id: i64 = category
                .attr("id")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let name = category.attr("name").unwrap_or_default().to_lowercase();

            let mut is_additional = true;
            for (needle, flag) in [
                ("movie", &mut categories.movie),
                ("tv", &mut categories.tv),
                ("anime", &mut categories.anime),
                ("xxx", &mut categories.xxx),
                ("audio", &mut categories.audio),
                ("book", &mut categories.book),
            ] {
                if name.contains(needle) {
                    *flag = true;
                    is_additional = false;
                }
            }
            // 100000+ are indexer-specific categories and 8000-8999 is "other";
            // neither implies a media type cross-seed can search.
            if is_additional && id < 100_000 && !(8000..=8999).contains(&id) {
                categories.additional = true;
            }
        }
    }

    Ok(Caps {
        search: is_available("search"),
        tv_search: is_available("tv-search"),
        movie_search: is_available("movie-search"),
        music_search: is_available("music-search"),
        audio_search: is_available("audio-search"),
        book_search: is_available("book-search"),
        movie_id_search: supported_ids("movie-search"),
        tv_id_search: supported_ids("tv-search"),
        categories,
        limits,
    })
}

// ─── Query construction ─────────────────────────────────────────────────────

/// `createTorznabSearchQueries` — decides *how* to ask an indexer for a given
/// searchee: a structured tv/movie search when the indexer supports it and the
/// title parses, an ID search when an arr recognised it, or free text.
pub fn create_torznab_search_queries(
    searchee: &Searchee,
    media_type: MediaType,
    caps: &Caps,
    parsed_media: Option<&ParsedMedia>,
) -> Vec<Query> {
    let stem = strip_extension(&searchee.title);
    let relevant_ids = parsed_media
        .map(|media| get_relevant_arr_ids(caps, media))
        .unwrap_or_default();
    let use_ids = relevant_ids.any();

    // With IDs in hand the free-text query is dropped entirely: mixing them
    // narrows results for no benefit.
    let text_query = |stem: &str| -> Option<String> {
        if use_ids {
            return None;
        }
        let query = reformat_title_for_searching(stem);
        Some(if query.is_empty() {
            stem.to_string()
        } else {
            query
        })
    };

    if media_type == MediaType::Episode && caps.tv_search {
        if let Some(ep) = match_episode(&EP_REGEX, &stem) {
            let mut query = Query::new(QueryKind::TvSearch);
            query.q = text_query(&stem);
            query.season = match (&ep.season, &ep.year) {
                (Some(season), _) => extract_int(season).map(|n| n.to_string()),
                (None, year) => year.clone(),
            };
            query.ep = match &ep.episode {
                Some(episode) => extract_int(episode).map(|n| n.to_string()),
                None => Some(format!(
                    "{}/{}",
                    ep.month.clone().unwrap_or_default(),
                    ep.day.clone().unwrap_or_default()
                )),
            };
            query.ids = relevant_ids;
            return vec![query];
        }
    } else if media_type == MediaType::Season && caps.tv_search {
        if let Some(season) = capture_group(&SEASON_REGEX, &stem, "season") {
            let mut query = Query::new(QueryKind::TvSearch);
            query.q = text_query(&stem);
            query.season = extract_int(&season).map(|n| n.to_string());
            query.ids = relevant_ids;
            return vec![query];
        }
    } else if media_type == MediaType::Movie && caps.movie_search {
        let mut query = Query::new(QueryKind::Movie);
        query.q = text_query(&stem);
        query.ids = relevant_ids;
        return vec![query];
    }

    if use_ids && caps.tv_search && parsed_media.is_some_and(|m| m.series.is_some()) {
        let episodes = &parsed_media.unwrap().episodes;
        let mut query = Query::new(QueryKind::TvSearch);
        query.season = episodes.first().map(|e| e.season_number.to_string());
        query.ep = (episodes.len() == 1).then(|| episodes[0].episode_number.to_string());
        query.ids = relevant_ids;
        return vec![query];
    }
    if use_ids && caps.movie_search && parsed_media.is_some_and(|m| m.movie.is_some()) {
        let mut query = Query::new(QueryKind::Movie);
        query.ids = relevant_ids;
        return vec![query];
    }

    if media_type == MediaType::Anime {
        let queries = get_anime_queries(&stem);
        if !queries.is_empty() {
            return queries
                .into_iter()
                .map(|q| {
                    let mut query = Query::new(QueryKind::Search);
                    query.q = Some(q);
                    query
                })
                .collect();
        }
    } else if media_type == MediaType::Video {
        return get_video_queries(&stem)
            .into_iter()
            .map(|q| {
                let mut query = Query::new(QueryKind::Search);
                query.q = Some(q);
                query
            })
            .collect();
    } else if media_type == MediaType::Book && caps.book_search {
        let mut query = Query::new(QueryKind::Search);
        // Calibre appends " (3)" to disambiguate duplicate titles.
        let without_index = CALIBRE_INDEXNUM_REGEX.replace(&stem, "");
        query.q = Some(clean_book_and_audio_title(&without_index));
        return vec![query];
    } else if media_type == MediaType::Audio && caps.audio_search {
        let mut query = Query::new(QueryKind::Search);
        query.q = Some(clean_book_and_audio_title(&stem));
        return vec![query];
    }

    let mut query = Query::new(QueryKind::Search);
    let cleaned = clean_title(&stem);
    query.q = Some(if cleaned.is_empty() { stem } else { cleaned });
    vec![query]
}

/// The cache key that decides which searchees can share one indexer round trip.
pub fn get_search_string(searchee: &Searchee) -> String {
    let media_type = searchee.media_type();
    let caps = Caps::all();
    let queries = create_torznab_search_queries(searchee, media_type, &caps, None);
    let Some(params) = queries.first() else {
        return String::new();
    };
    let season = params
        .season
        .as_ref()
        .map(|s| format!(".S{s}"))
        .unwrap_or_default();
    let ep = params
        .ep
        .as_ref()
        .map(|e| format!(".E{e}"))
        .unwrap_or_default();
    format!(
        "{}{season}{ep}",
        params.q.clone().unwrap_or_else(|| "undefined".into())
    )
    .to_lowercase()
}

/// Approximates [`get_search_string`] from a bare name — used by the stats page
/// to count distinct queries without loading every searchee's file list.
pub fn estimate_search_string(name: &str) -> String {
    let searchee = Searchee {
        name: name.to_string(),
        title: name.to_string(),
        length: 1,
        files: vec![crate::searchee::File {
            name: "a.mkv".into(),
            path: "a.mkv".into(),
            length: 1,
        }],
        ..Default::default()
    };
    get_search_string(&searchee)
}

/// Whether it is worth asking this indexer about this kind of media at all.
pub fn indexer_does_support_media_type(media_type: MediaType, indexer: &Indexer) -> bool {
    let categories = indexer.categories.unwrap_or_default();
    match media_type {
        MediaType::Episode | MediaType::Season => indexer.tv_search_cap || categories.xxx,
        MediaType::Movie => indexer.movie_search_cap || categories.xxx,
        MediaType::Anime | MediaType::Video => {
            indexer.movie_search_cap
                || indexer.tv_search_cap
                || categories.movie
                || categories.tv
                || categories.anime
                || categories.xxx
        }
        MediaType::Audio => {
            indexer.audio_search_cap || indexer.music_search_cap || categories.audio
        }
        MediaType::Book => indexer.book_search_cap || categories.book,
        MediaType::Other => categories.additional,
    }
}

/// Per-indexer capability view, built from the stored columns.
pub fn caps_of(indexer: &Indexer) -> Caps {
    Caps {
        search: indexer.search_cap,
        tv_search: indexer.tv_search_cap,
        movie_search: indexer.movie_search_cap,
        music_search: indexer.music_search_cap,
        audio_search: indexer.audio_search_cap,
        book_search: indexer.book_search_cap,
        tv_id_search: indexer.tv_id_caps.unwrap_or_default(),
        movie_id_search: indexer.movie_id_caps.unwrap_or_default(),
        categories: indexer.categories.unwrap_or_default(),
        limits: indexer.limits.unwrap_or_default(),
    }
}

// ─── Requests ───────────────────────────────────────────────────────────────

pub struct TorznabRequest {
    pub indexer_id: i64,
    pub name: Option<String>,
    pub base_url: String,
    pub apikey: String,
    pub query: Query,
}

fn indexer_label(name: Option<&str>, url: &str) -> String {
    name.filter(|n| !n.is_empty()).unwrap_or(url).to_string()
}

/// Snoozes an indexer after a failed search, honouring `Retry-After` when the
/// indexer sends one.
async fn on_response_not_ok(
    pool: &SqlitePool,
    status: u16,
    retry_after_header: Option<&str>,
    indexer_id: i64,
    indexer_name: &str,
) -> i64 {
    let retry_after = match retry_after_header.and_then(|v| v.trim().parse::<i64>().ok()) {
        Some(seconds) => now_ms() + seconds * 1000,
        None if status == 429 => now_ms() + 60 * 60 * 1000,
        None => now_ms() + 10 * 60 * 1000,
    };
    let status_kind = if status == 429 {
        IndexerStatus::RateLimited
    } else {
        IndexerStatus::UnknownError
    };
    let _ = crate::indexers::update_indexer_status(
        pool,
        status_kind,
        retry_after,
        &[indexer_id],
        &[indexer_name.to_string()],
    )
    .await;
    retry_after
}

/// Performs one Torznab query. Failures snooze the indexer before propagating,
/// so a flapping tracker does not get hammered on the next searchee.
pub async fn make_request(
    pool: &SqlitePool,
    request: &TorznabRequest,
    searchee_label: &str,
) -> Result<Vec<Candidate>, String> {
    let config = get_runtime_config();
    let url = assemble_url(&request.base_url, &request.apikey, &request.query)
        .map_err(|e| e.to_string())?;
    let label = indexer_label(request.name.as_deref(), &request.base_url);

    tracing::debug!(
        label = searchee_label,
        "Querying {label} at {} with {:?}",
        request.base_url,
        request.query
    );

    let mut builder = client().get(&url);
    if let Some(timeout) = config.search_timeout {
        builder = builder.timeout(std::time::Duration::from_millis(timeout.max(0) as u64));
    }
    let response = builder.send().await.map_err(|e| e.to_string())?;
    let status = response.status().as_u16();

    if !response.status().is_success() {
        let retry_after_header = response
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let retry_after = on_response_not_ok(
            pool,
            status,
            retry_after_header.as_deref(),
            request.indexer_id,
            &label,
        )
        .await;
        return Err(format!(
            "request failed with code {status}{}, snoozing until {}",
            if status == 429 {
                " due to rate limiting"
            } else {
                ""
            },
            human_readable_date(retry_after)
        ));
    }

    let xml = response.text().await.map_err(|e| e.to_string())?;
    let candidates = parse_torznab_results(&xml, request.indexer_id)?;

    // Indexers report their own display name in the results; adopt it so the
    // UI shows "BeyondHD" rather than a bare URL.
    if let Some(first) = candidates.first()
        && first.tracker != UNKNOWN_TRACKER
    {
        let _ = sqlx::query("UPDATE indexer SET name = ? WHERE id = ?")
            .bind(&first.tracker)
            .bind(request.indexer_id)
            .execute(pool)
            .await;
    }

    Ok(candidates)
}

/// Candidates grouped by the indexer that produced them.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexerCandidates {
    pub indexer_id: i64,
    pub candidates: Vec<Candidate>,
}

/// Runs every request concurrently; a failing indexer is logged and skipped
/// rather than aborting the batch (`Promise.allSettled` in the original).
pub async fn make_requests(
    pool: &SqlitePool,
    requests: Vec<TorznabRequest>,
    searchee_label: &str,
) -> Vec<IndexerCandidates> {
    let results = futures::future::join_all(
        requests
            .iter()
            .map(|request| make_request(pool, request, searchee_label)),
    )
    .await;

    let mut grouped: Vec<IndexerCandidates> = Vec::new();
    for (request, result) in requests.iter().zip(results) {
        match result {
            Ok(candidates) => match grouped
                .iter_mut()
                .find(|group| group.indexer_id == request.indexer_id)
            {
                Some(group) => group.candidates.extend(candidates),
                None => grouped.push(IndexerCandidates {
                    indexer_id: request.indexer_id,
                    candidates,
                }),
            },
            Err(message) => {
                tracing::warn!(
                    label = searchee_label,
                    "Failed to reach {}: {message}",
                    indexer_label(request.name.as_deref(), &request.base_url)
                );
            }
        }
    }
    grouped
}

/// Reads an indexer's `t=caps` document.
pub async fn fetch_caps(indexer: &Indexer) -> Result<Caps, String> {
    let label = indexer_label(indexer.name.as_deref(), &indexer.url);
    let url = assemble_url(&indexer.url, &indexer.apikey, &Query::new(QueryKind::Caps))
        .map_err(|e| e.to_string())?;

    let response = client()
        .get(&url)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("{label} failed to respond, check verbose logs: {e}"))?;

    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;

    if !status.is_success() {
        let message = match status.as_u16() {
            429 => format!("{label} was rate limited when fetching caps"),
            401 => format!(
                "{label} returned 401 Unauthorized when fetching caps, check your apikey (all torznab entries use the Prowlarr/Jackett apikey)"
            ),
            code => {
                format!("{label} responded with code {code} when fetching caps, check verbose logs")
            }
        };
        tracing::debug!(
            label = Label::Torznab.as_str(),
            "Response body {}",
            body_sample(&text)
        );
        return Err(message);
    }

    parse_torznab_caps(&text).map_err(|_| {
        tracing::debug!(
            label = Label::Torznab.as_str(),
            "Response body {}",
            body_sample(&text)
        );
        format!("{label} responded with invalid XML when fetching caps, check verbose logs")
    })
}

/// Refreshes one indexer's caps, logging rather than propagating a failure —
/// a broken indexer must not abort a settings save.
pub async fn update_caps_for_indexer(pool: &SqlitePool, indexer: &Indexer) {
    match fetch_caps(indexer).await {
        Ok(caps) => {
            if let Err(e) =
                crate::indexers::update_indexer_caps_by_id(pool, indexer.id, &caps).await
            {
                tracing::warn!(
                    label = Label::Torznab.as_str(),
                    "Failed to store caps for {}: {e}",
                    indexer_label(indexer.name.as_deref(), &indexer.url)
                );
            }
        }
        Err(message) => {
            tracing::warn!(
                label = Label::Torznab.as_str(),
                "Indexer {} failed to fetch caps: {message}",
                indexer_label(indexer.name.as_deref(), &indexer.url)
            );
        }
    }
}

/// Refreshes every indexer's caps concurrently.
pub async fn update_caps(pool: &SqlitePool) -> sqlx::Result<()> {
    let indexers = crate::indexers::get_all_indexers(pool).await?;
    futures::future::join_all(
        indexers
            .iter()
            .map(|indexer| update_caps_for_indexer(pool, indexer)),
    )
    .await;
    for indexer in &indexers {
        log_indexer_media_types(indexer);
    }
    Ok(())
}

fn log_indexer_media_types(indexer: &Indexer) {
    let label = indexer_label(indexer.name.as_deref(), &indexer.url);
    if indexer.categories.is_none() {
        tracing::error!(
            label = Label::Torznab.as_str(),
            "Indexer {label} failed to fetch caps"
        );
        return;
    }
    let all = [
        MediaType::Episode,
        MediaType::Season,
        MediaType::Movie,
        MediaType::Anime,
        MediaType::Video,
        MediaType::Audio,
        MediaType::Book,
        MediaType::Other,
    ];
    let (supported, unsupported): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|mt| indexer_does_support_media_type(*mt, indexer));
    tracing::debug!(
        label = Label::Torznab.as_str(),
        "{label} MediaTypes: Supported [{}] | Unsupported [{}]",
        supported
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        unsupported
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// Convenience wrapper used by the media-type helpers that take a whole
/// searchee.
pub fn media_type_for(searchee: &Searchee) -> MediaType {
    media_type_of(&searchee.title, &searchee.files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::searchee::File;

    fn searchee(title: &str, file: &str) -> Searchee {
        Searchee {
            name: title.to_string(),
            title: title.to_string(),
            length: 100,
            files: vec![File {
                name: file.to_string(),
                path: file.to_string(),
                length: 100,
            }],
            ..Default::default()
        }
    }

    const CAPS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<caps>
  <limits max="200" default="75"/>
  <searching>
    <search available="yes" supportedParams="q"/>
    <tv-search available="yes" supportedParams="q,season,ep,tvdbid,imdbid"/>
    <movie-search available="no" supportedParams="q"/>
    <book-search available="yes" supportedParams="q"/>
  </searching>
  <categories>
    <category id="5000" name="TV">
      <subcat id="5030" name="TV/SD"/>
    </category>
    <category id="2000" name="Movies"/>
    <category id="8010" name="Other/Misc"/>
    <category id="100001" name="Custom"/>
    <category id="3000" name="Audio"/>
  </categories>
</caps>"#;

    #[test]
    fn caps_are_read_from_attributes() {
        let caps = parse_torznab_caps(CAPS_XML).unwrap();
        assert!(caps.search);
        assert!(caps.tv_search);
        assert!(!caps.movie_search);
        assert!(caps.book_search);
        assert!(!caps.music_search);
        assert_eq!(caps.limits.max, 200);
        assert_eq!(caps.limits.default, 75);
    }

    #[test]
    fn supported_id_params_are_split_out() {
        let caps = parse_torznab_caps(CAPS_XML).unwrap();
        assert_eq!(caps.tv_id_search.tvdb_id, Some(true));
        assert_eq!(caps.tv_id_search.imdb_id, Some(true));
        assert_eq!(caps.tv_id_search.tmdb_id, Some(false));
        assert_eq!(caps.movie_id_search.tvdb_id, Some(false));
    }

    /// 8000-8999 ("other") and 100000+ (indexer-specific) must not count as an
    /// additional media type, or every indexer would claim to support OTHER.
    #[test]
    fn category_ids_outside_the_standard_range_are_not_additional() {
        let caps = parse_torznab_caps(CAPS_XML).unwrap();
        assert!(caps.categories.tv);
        assert!(caps.categories.movie);
        assert!(caps.categories.audio);
        assert!(!caps.categories.additional);
    }

    #[test]
    fn an_unrecognised_category_marks_additional() {
        let xml = r#"<caps><categories><category id="7000" name="Software"/></categories></caps>"#;
        let caps = parse_torznab_caps(xml).unwrap();
        assert!(caps.categories.additional);
    }

    #[test]
    fn missing_limits_fall_back_to_100() {
        let caps = parse_torznab_caps("<caps></caps>").unwrap();
        assert_eq!(caps.limits, IndexerLimits::default());
    }

    #[test]
    fn invalid_xml_is_an_error() {
        assert!(parse_torznab_caps("<caps><unclosed>").is_err());
    }

    const RESULTS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
  <channel>
    <item>
      <title>Some.Show.S01E01.1080p.WEB-DL-GRP</title>
      <guid>https://tracker.example/details/1</guid>
      <link>https://tracker.example/download/1/key/x.torrent</link>
      <size>1234567</size>
      <pubDate>Mon, 01 Jan 2024 12:00:00 +0000</pubDate>
      <prowlarrindexer id="3">BeyondHD</prowlarrindexer>
    </item>
    <item>
      <title>Other.Release</title>
      <guid>https://tracker.example/details/2</guid>
      <link>magnet:?xt=urn:btih:abcd</link>
      <size>7</size>
      <pubDate>Tue, 02 Jan 2024 12:00:00 +0000</pubDate>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn results_are_parsed_with_the_tracker_name() {
        let candidates = parse_torznab_results(RESULTS_XML, 3).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].name, "Some.Show.S01E01.1080p.WEB-DL-GRP");
        assert_eq!(candidates[0].tracker, "BeyondHD");
        assert_eq!(candidates[0].size, 1_234_567);
        assert_eq!(candidates[0].indexer_id, Some(3));
        assert_eq!(
            candidates[0].pub_date,
            Some(
                chrono::DateTime::parse_from_rfc2822("Mon, 01 Jan 2024 12:00:00 +0000")
                    .unwrap()
                    .timestamp_millis()
            )
        );
        // No indexer element at all -> the sentinel tracker name.
        assert_eq!(candidates[1].tracker, UNKNOWN_TRACKER);
    }

    #[test]
    fn a_feed_with_no_items_yields_nothing() {
        let xml = "<rss><channel></channel></rss>";
        assert!(parse_torznab_results(xml, 1).unwrap().is_empty());
    }

    #[test]
    fn episode_queries_use_tvsearch_with_season_and_episode() {
        let searchee = searchee("Some.Show.S01E05.1080p", "Some.Show.S01E05.1080p.mkv");
        let queries =
            create_torznab_search_queries(&searchee, MediaType::Episode, &Caps::all(), None);
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].t, QueryKind::TvSearch);
        assert_eq!(queries[0].season.as_deref(), Some("1"));
        assert_eq!(queries[0].ep.as_deref(), Some("5"));
        assert!(queries[0].q.is_some());
    }

    #[test]
    fn dated_episode_queries_use_a_month_day_episode() {
        let searchee = searchee("Some.Show.2024.01.02.WEB", "Some.Show.2024.01.02.WEB.mkv");
        let queries =
            create_torznab_search_queries(&searchee, MediaType::Episode, &Caps::all(), None);
        assert_eq!(queries[0].season.as_deref(), Some("2024"));
        assert_eq!(queries[0].ep.as_deref(), Some("01/02"));
    }

    #[test]
    fn season_queries_drop_the_episode() {
        let searchee = searchee("Some.Show.S03.1080p", "Some.Show.S03E01.mkv");
        let queries =
            create_torznab_search_queries(&searchee, MediaType::Season, &Caps::all(), None);
        assert_eq!(queries[0].t, QueryKind::TvSearch);
        assert_eq!(queries[0].season.as_deref(), Some("3"));
        assert_eq!(queries[0].ep, None);
    }

    /// When an arr supplied usable IDs the free-text query is dropped.
    #[test]
    fn id_searches_omit_the_text_query() {
        let searchee = searchee("Some.Show.S01E05.1080p", "Some.Show.S01E05.1080p.mkv");
        let parsed = ParsedMedia {
            series: Some(crate::arr::ExternalIds {
                tvdb_id: Some("77".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let queries = create_torznab_search_queries(
            &searchee,
            MediaType::Episode,
            &Caps::all(),
            Some(&parsed),
        );
        assert_eq!(queries[0].q, None);
        assert_eq!(queries[0].ids.tvdbid.as_deref(), Some("77"));
    }

    #[test]
    fn indexers_without_tv_search_fall_back_to_free_text() {
        let searchee = searchee("Some.Show.S01E05.1080p", "Some.Show.S01E05.1080p.mkv");
        let mut caps = Caps::all();
        caps.tv_search = false;
        let queries = create_torznab_search_queries(&searchee, MediaType::Episode, &caps, None);
        assert_eq!(queries[0].t, QueryKind::Search);
        assert!(queries[0].q.is_some());
    }

    #[test]
    fn book_queries_strip_the_calibre_index_suffix() {
        let searchee = searchee("Some Book (3)", "Some Book.epub");
        let queries = create_torznab_search_queries(&searchee, MediaType::Book, &Caps::all(), None);
        assert_eq!(queries[0].t, QueryKind::Search);
        assert!(!queries[0].q.as_deref().unwrap().contains("(3)"));
    }

    #[test]
    fn urls_carry_the_apikey_and_every_set_parameter() {
        let mut query = Query::new(QueryKind::TvSearch);
        query.q = Some("Some Show".into());
        query.season = Some("1".into());
        query.ep = Some("5".into());
        let url = assemble_url("https://x.example/api", "secret", &query).unwrap();
        assert!(url.starts_with("https://x.example/api?"));
        assert!(url.contains("apikey=secret"));
        assert!(url.contains("t=tvsearch"));
        assert!(url.contains("q=Some+Show"));
        assert!(url.contains("season=1"));
        assert!(url.contains("ep=5"));
        // Unset parameters must not appear at all.
        assert!(!url.contains("limit="));
    }

    #[test]
    fn media_type_support_follows_caps_and_categories() {
        let base = Indexer {
            id: 1,
            name: None,
            url: "https://x.example/api".into(),
            apikey: "k".into(),
            trackers: None,
            enabled: true,
            status: None,
            retry_after: None,
            search_cap: true,
            tv_search_cap: false,
            movie_search_cap: false,
            music_search_cap: false,
            audio_search_cap: false,
            book_search_cap: false,
            tv_id_caps: None,
            movie_id_caps: None,
            categories: Some(IndexerCategories::default()),
            limits: None,
        };

        assert!(!indexer_does_support_media_type(MediaType::Episode, &base));
        let tv = Indexer {
            tv_search_cap: true,
            ..base.clone()
        };
        assert!(indexer_does_support_media_type(MediaType::Episode, &tv));
        assert!(indexer_does_support_media_type(MediaType::Video, &tv));

        let other = Indexer {
            categories: Some(IndexerCategories {
                additional: true,
                ..Default::default()
            }),
            ..base
        };
        assert!(indexer_does_support_media_type(MediaType::Other, &other));
    }

    /// Two searchees with the same search string share one indexer round trip,
    /// so this must be stable and case-insensitive.
    #[test]
    fn search_strings_collapse_equivalent_searchees() {
        let a = searchee("Some.Show.S01E05.1080p.WEB-DL", "Some.Show.S01E05.mkv");
        let b = searchee("some show s01e05 720p bluray", "some show s01e05.mkv");
        assert_eq!(get_search_string(&a), get_search_string(&b));
    }

    #[test]
    fn estimate_search_string_matches_the_full_computation() {
        let name = "Some.Show.S01E05.1080p.WEB-DL";
        let full = get_search_string(&searchee(name, "Some.Show.S01E05.1080p.mkv"));
        assert_eq!(estimate_search_string(name), full);
    }
}
