//! v1-fork search routes, ported from `backend/Api/Controllers/SearchIndexers` and
//! `backend/Api/Controllers/Profiles`:
//! - `GET /api/search-indexers?q&limit` — raw Newznab search across all enabled indexers
//!   (admin API key, like v1's `BaseApiController`).
//! - `GET /adapters/newznab/{token}/api` — Newznab adapter for a profile (token-scoped, public).
//! - `GET|OPTIONS /adapters/addon/{token}/manifest.json` — Stremio-style addon manifest
//!   (token-scoped, public, CORS).

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use infinidysk_indexer::NewznabClient;
use infinidysk_indexer::models::IndexerConfig;
use infinidysk_profiles::filename;
use infinidysk_profiles::hits::HitType;
use infinidysk_profiles::resolvers::ResolverConfig;
use infinidysk_profiles::{Resolvers, SearchService, SearchSettings};
use infinidysk_search::profile::Profile;

use crate::auth::Auth;
use crate::config::{ConfigStore, Settings};
use crate::sab::same;

/// Shared state for the search routes.
pub struct Search {
    pub service: Arc<SearchService>,
    pub config: Arc<RwLock<ConfigStore>>,
    pub auth: Arc<Auth>,
}

/// Builds the [`SearchSettings`] snapshot for the service from the app settings.
pub fn search_settings(s: &Settings) -> SearchSettings {
    SearchSettings {
        profiles: s.profiles.clone().normalized(),
        indexers: s.indexers.clone(),
        exclude_patterns_raw: s.search.exclude_patterns.clone(),
        search_user_agent: s.search.search_user_agent.clone(),
        user_agent: s.search.user_agent.clone(),
        watchtower_enabled: false, // Phase 3
        warden_hide_dead: false,   // Phase 3
    }
}

fn resolver_config(s: &Settings) -> ResolverConfig {
    ResolverConfig {
        enabled: s.search.resolvers_enabled,
        ..Default::default()
    }
}

/// Builds the shared search service from the boot settings.
#[must_use]
pub fn service_from_settings(s: &Settings) -> SearchService {
    SearchService::new(search_settings(s), Resolvers::new(resolver_config(s)))
}

/// Builds the router state: service from the current settings, plus config/auth handles.
pub async fn build(config: Arc<RwLock<ConfigStore>>, auth: Arc<Auth>) -> Arc<Search> {
    let settings = config.read().await.settings().clone();
    Arc::new(Search {
        service: Arc::new(service_from_settings(&settings)),
        config,
        auth,
    })
}

pub fn router(state: Arc<Search>) -> Router {
    Router::new()
        .route("/api/search-indexers", get(search_indexers))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            require_api_key,
        ))
        .route("/adapters/newznab/{token}/api", get(newznab_api))
        .route(
            "/adapters/addon/{token}/manifest.json",
            get(manifest).options(manifest_options),
        )
        .with_state(state)
}

impl Search {
    /// Pushes the current settings into the service when they changed since the last request.
    async fn sync_settings(&self) {
        let snapshot = {
            let store = self.config.read().await;
            search_settings(store.settings())
        };
        self.service.apply_settings(snapshot).await;
    }
}

/// v1 `ApiKeyValidator`: session cookie / admin key via `Auth::allowed`, or the SAB-style
/// `apikey` query param / `X-Api-Key` header matching `sab.api_key` or `auth.api_key`.
async fn require_api_key(State(state): State<Arc<Search>>, req: Request, next: Next) -> Response {
    if state.auth.allowed(req.headers()).await {
        return next.run(req).await;
    }
    let given = req
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            req.uri().query().and_then(|q| {
                q.split('&').find_map(|p| {
                    p.split_once('=')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("apikey"))
                        .map(|(_, v)| v.to_owned())
                })
            })
        });
    if let Some(given) = given {
        let (sab, admin) = {
            let store = state.config.read().await;
            let s = store.settings();
            let keys = (s.sab.api_key.0.clone(), s.auth.api_key.0.clone());
            drop(store);
            keys
        };
        if (!sab.is_empty() && same(given.as_bytes(), sab.as_bytes()))
            || (!admin.is_empty() && same(given.as_bytes(), admin.as_bytes()))
        {
            return next.run(req).await;
        }
    }
    (StatusCode::UNAUTHORIZED, "API Key Required").into_response()
}

// --- GET /api/search-indexers ---

#[derive(Debug, Deserialize)]
struct SearchIndexersQuery {
    q: Option<String>,
    limit: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchIndexersResponse {
    status: bool,
    results: Vec<ResultItem>,
    indexers: Vec<IndexerStatus>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResultItem {
    indexer: String,
    title: String,
    nzb_url: String,
    size: i64,
    posted: Option<Timestamp>,
    source_indexer: Option<String>,
    language: Option<String>,
    subs: Option<String>,
    info_hash: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexerStatus {
    name: String,
    ok: bool,
    result_count: usize,
    error: Option<String>,
    elapsed_ms: u128,
}

async fn search_indexers(
    State(state): State<Arc<Search>>,
    Query(params): Query<SearchIndexersQuery>,
) -> Response {
    state.sync_settings().await;
    let q = params.q.unwrap_or_default();
    if q.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status": false, "error": "Query `q` is required"})),
        )
            .into_response();
    }
    // v1: int.TryParse && n is > 0 and <= 500, else 100.
    let limit: u32 = params
        .limit
        .as_deref()
        .and_then(|l| l.parse::<i64>().ok())
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| (1..=500).contains(n))
        .unwrap_or(100);

    let ct = CancellationToken::new();
    let indexer_cfg = {
        let store = state.config.read().await;
        store.settings().indexers.clone()
    };
    let global_proxy = indexer_cfg.proxy_url.clone();
    let search_ua_default = {
        let store = state.config.read().await;
        store.settings().search.search_user_agent.clone()
    };
    let indexers: Vec<_> = indexer_cfg
        .indexers
        .iter()
        .filter(|x| x.enabled)
        .cloned()
        .collect();

    let per_indexer = futures_util::future::join_all(indexers.iter().map(|x| {
        let state = Arc::clone(&state);
        let indexer_cfg = indexer_cfg.clone();
        let global_proxy = global_proxy.clone();
        let ua_default = search_ua_default.clone();
        let q = q.clone();
        let ct = ct.clone();
        async move {
            let sw = Instant::now();
            let now = Timestamp::now();
            let hit_check = state
                .service
                .hit_tracker()
                .check(
                    &x.name,
                    HitType::Search,
                    x.hit_limit.map(i64::from),
                    x.hit_limit_reset_time.and_then(|h| i32::try_from(h).ok()),
                    now,
                )
                .await;
            if let Some(c) = &hit_check
                && !c.allowed
            {
                return (
                    IndexerStatus {
                        name: x.name.clone(),
                        ok: false,
                        result_count: 0,
                        error: Some(infinidysk_profiles::hits::HitTracker::format_skip_reason(
                            c,
                            HitType::Search,
                            now,
                        )),
                        elapsed_ms: sw.elapsed().as_millis(),
                    },
                    Vec::new(),
                );
            }
            let ua = IndexerConfig::per_indexer_search_user_agent(x).unwrap_or(ua_default);
            let proxy = if x.proxy_url.as_deref().is_none_or(|p| p.trim().is_empty()) {
                global_proxy.clone()
            } else {
                x.proxy_url.clone()
            };
            let timeout = indexer_cfg.effective_timeout_seconds(x);
            if state
                .service
                .rate_limiter()
                .wait(&x.name, x.max_requests_per_minute, &ct)
                .await
                .is_err()
            {
                return (
                    IndexerStatus {
                        name: x.name.clone(),
                        ok: false,
                        result_count: 0,
                        error: Some("Indexer search failed unexpectedly.".to_owned()),
                        elapsed_ms: sw.elapsed().as_millis(),
                    },
                    Vec::new(),
                );
            }
            let client = match NewznabClient::with_options(
                &x.url,
                &x.api_key,
                indexer_cfg.effective_max_response_bytes(x),
                &ua,
                std::time::Duration::from_secs(timeout),
                x.skip_tls_verification,
            ) {
                Ok(c) => c.with_proxy(proxy.clone()),
                Err(e) => {
                    return (
                        IndexerStatus {
                            name: x.name.clone(),
                            ok: false,
                            result_count: 0,
                            error: Some(e.to_string()),
                            elapsed_ms: sw.elapsed().as_millis(),
                        },
                        Vec::new(),
                    );
                }
            };
            let items = match client.search(&q, limit, &ct).await {
                Ok(items) => items,
                Err(e) => {
                    let msg = match &e {
                        infinidysk_indexer::Error::Indexer(m) => m.clone(),
                        other => {
                            if matches!(
                                other,
                                infinidysk_indexer::Error::Status(_)
                                    | infinidysk_indexer::Error::TooLarge { .. }
                                    | infinidysk_indexer::Error::Format
                            ) {
                                other.to_string()
                            } else {
                                tracing::warn!(reason = %other, indexer = %x.name, "Indexer search failed unexpectedly.");
                                "Indexer search failed unexpectedly.".to_owned()
                            }
                        }
                    };
                    return (
                        IndexerStatus {
                            name: x.name.clone(),
                            ok: false,
                            result_count: 0,
                            error: Some(msg),
                            elapsed_ms: sw.elapsed().as_millis(),
                        },
                        Vec::new(),
                    );
                }
            };
            state
                .service
                .hit_tracker()
                .record(&x.name, HitType::Search, Timestamp::now())
                .await;
            let mapped: Vec<ResultItem> = items
                .into_iter()
                .filter(|i| !x.enable_strict_matching || filename::matches(Some(&q), Some(&i.title)))
                .map(|i| ResultItem {
                    indexer: x.name.clone(),
                    title: i.title,
                    nzb_url: i.nzb_url,
                    size: i.size,
                    posted: i.posted,
                    source_indexer: i.source_indexer_name,
                    language: i.language,
                    subs: i.subs,
                    info_hash: i.info_hash,
                })
                .collect();
            let count = mapped.len();
            (
                IndexerStatus {
                    name: x.name.clone(),
                    ok: true,
                    result_count: count,
                    error: None,
                    elapsed_ms: sw.elapsed().as_millis(),
                },
                mapped,
            )
        }
    }))
    .await;

    let mut results: Vec<ResultItem> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut statuses = Vec::with_capacity(per_indexer.len());
    for (status, items) in per_indexer {
        statuses.push(status);
        for item in items {
            if seen.insert(item.nzb_url.clone()) {
                results.push(item);
            }
        }
    }
    // v1: GroupBy(NzbUrl).First().OrderByDescending(Posted ?? MinValue).
    results.sort_by(|a, b| {
        b.posted
            .unwrap_or(Timestamp::MIN)
            .cmp(&a.posted.unwrap_or(Timestamp::MIN))
    });

    Json(SearchIndexersResponse {
        status: true,
        results,
        indexers: statuses,
    })
    .into_response()
}

// --- GET /adapters/newznab/{token}/api ---

#[derive(Debug, Deserialize)]
struct NewznabQuery {
    t: Option<String>,
    q: Option<String>,
    imdbid: Option<String>,
    #[allow(dead_code)]
    tvdbid: Option<String>,
    season: Option<i32>,
    ep: Option<i32>,
    #[allow(dead_code)]
    offset: Option<i32>,
    #[allow(dead_code)]
    limit: Option<i32>,
}

const NEWZNAB_NS: &str = "http://www.newznab.com/DTD/2010/feeds/attributes/";
const ATOM_NS: &str = "http://www.w3.org/2005/Atom";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// RFC 1123 (`"R"`) date like v1's `DateTimeOffset.ToString("R")`.
fn rfc1123(ts: Timestamp) -> String {
    let secs = u64::try_from(ts.as_second()).unwrap_or(0);
    httpdate::fmt_http_date(
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
    )
}

fn public_base_url(headers: &HeaderMap, uri: &Uri, configured: &str) -> String {
    let trimmed = configured.trim_end_matches('/');
    let mut base = if !trimmed.is_empty() && trimmed != "http://localhost:3000" {
        trimmed.to_owned()
    } else {
        let scheme = uri.scheme_str().unwrap_or("http");
        let host = uri.authority().map_or("localhost", |a| a.as_str());
        format!("{scheme}://{host}")
    };
    // The frontend proxy supplies its validated URL_BASE as this prefix (v1
    // GetPublicBaseUrl + X-Forwarded-Prefix).
    if let Some(raw) = headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
    {
        let prefix = raw.trim().trim_end_matches('/');
        if !prefix.is_empty() && !base.ends_with(prefix) {
            base.push_str(prefix);
        }
    }
    base
}

/// The largest result count this instance can return (global default, raised by any
/// per-indexer override) — advertised in `t=caps`.
fn advertised_limit(cfg: &IndexerConfig) -> u32 {
    let global = cfg
        .search_result_limit
        .filter(|g| *g > 0)
        .unwrap_or(infinidysk_indexer::models::DEFAULT_SEARCH_RESULT_LIMIT);
    cfg.indexers
        .iter()
        .filter_map(|i| i.search_result_limit.filter(|p| *p > 0))
        .fold(global, u32::max)
}

fn caps_xml(limit: u32) -> String {
    use std::fmt::Write as _;
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = write!(
        xml,
        concat!(
            r#"<caps><server appversion="{}" version="0.1" title="NzbDav Search Profile" strapline="Newznab adapter for a NzbDav Search Profile"/>"#,
            r#"<limits max="{limit}" default="{limit}"/>"#,
            "<searching>",
            r#"<search available="no" supportedParams=""/>"#,
            r#"<tv-search available="yes" supportedParams="imdbid,season,ep"/>"#,
            r#"<movie-search available="yes" supportedParams="imdbid"/>"#,
            r#"<audio-search available="no" supportedParams=""/>"#,
            r#"<book-search available="no" supportedParams=""/>"#,
            "</searching>",
            r#"<categories><category id="2000" name="Movies"/><category id="5000" name="TV"/></categories>"#,
            "</caps>"
        ),
        xml_escape(env!("CARGO_PKG_VERSION")),
        limit = limit,
    );
    xml
}

fn feed_xml(
    profile: &Profile,
    base_url: &str,
    token: &str,
    items: &[(infinidysk_profiles::cache::Candidate, String)],
) -> String {
    use std::fmt::Write as _;
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = write!(
        xml,
        r#"<rss version="2.0" xmlns:atom="{ATOM_NS}" xmlns:newznab="{NEWZNAB_NS}"><channel>"#
    );
    let _ = write!(
        xml,
        "<title>{}</title><description>Newznab adapter for a NzbDav Search Profile</description><link>{}</link><language>en-us</language><category>search</category>",
        xml_escape(&format!("NzbDav Search Profile: {}", profile.name)),
        xml_escape(base_url),
    );
    let _ = write!(
        xml,
        r#"<atom:link href="{}/adapters/newznab/{}/api" rel="self" type="application/rss+xml"/>"#,
        xml_escape(base_url),
        xml_escape(token),
    );
    let _ = write!(
        xml,
        r#"<newznab:response offset="0" total="{}"/>"#,
        items.len()
    );
    for (c, play_token) in items {
        let nzb_url = format!("{base_url}/api/search/{token}/nzb/{play_token}.nzb");
        let _ = write!(xml, "<item>");
        let _ = write!(xml, "<title>{}</title>", xml_escape(&c.title));
        let _ = write!(
            xml,
            r#"<guid isPermaLink="false">{}</guid>"#,
            xml_escape(play_token)
        );
        let _ = write!(xml, "<link>{}</link>", xml_escape(&nzb_url));
        let _ = write!(xml, "<comments></comments>");
        let _ = write!(
            xml,
            "<pubDate>{}</pubDate>",
            rfc1123(c.posted.unwrap_or_else(Timestamp::now))
        );
        let _ = write!(xml, "<category>Movies</category>");
        let _ = write!(xml, "<description>{}</description>", xml_escape(&c.title));
        let _ = write!(
            xml,
            r#"<enclosure url="{}" length="{}" type="application/x-nzb"/>"#,
            xml_escape(&nzb_url),
            c.size,
        );
        let _ = write!(xml, r#"<newznab:attr name="size" value="{}"/>"#, c.size);
        let _ = write!(xml, r#"<newznab:attr name="category" value="2000"/>"#);
        if let Some(g) = c.grabs {
            let _ = write!(xml, r#"<newznab:attr name="grabs" value="{g}"/>"#);
        }
        if let Some(p) = c.password {
            let _ = write!(xml, r#"<newznab:attr name="password" value="{p}"/>"#);
        }
        if let Some(d) = c.usenet_date {
            let _ = write!(
                xml,
                r#"<newznab:attr name="usenetdate" value="{}"/>"#,
                xml_escape(&rfc1123(d))
            );
        }
        let display_indexer = c
            .source_indexer_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&c.indexer_name);
        if !display_indexer.trim().is_empty() {
            let _ = write!(
                xml,
                r#"<newznab:attr name="hydraIndexerName" value="{}"/>"#,
                xml_escape(display_indexer)
            );
            let _ = write!(
                xml,
                r#"<newznab:attr name="sourceIndexerName" value="{}"/>"#,
                xml_escape(display_indexer)
            );
        }
        if let Some(l) = c.language.as_deref().filter(|s| !s.trim().is_empty()) {
            let _ = write!(
                xml,
                r#"<newznab:attr name="language" value="{}"/>"#,
                xml_escape(l)
            );
        }
        if let Some(subs) = c.subs.as_deref().filter(|s| !s.trim().is_empty()) {
            let _ = write!(
                xml,
                r#"<newznab:attr name="subs" value="{}"/>"#,
                xml_escape(subs)
            );
        }
        if let Some(h) = c.info_hash.as_deref().filter(|s| !s.trim().is_empty()) {
            let _ = write!(
                xml,
                r#"<newznab:attr name="infohash" value="{}"/>"#,
                xml_escape(h)
            );
        }
        let _ = write!(xml, "</item>");
    }
    let _ = write!(xml, "</channel></rss>");
    xml
}

fn xml_response(body: String) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/xml; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
        ],
        body,
    )
        .into_response()
}

async fn newznab_api(
    State(state): State<Arc<Search>>,
    Path(token): Path<String>,
    Query(params): Query<NewznabQuery>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    state.sync_settings().await;
    let profile = state.service.get_profile(&token).await;
    let Some(profile) = profile else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !state.service.is_adapter_enabled(&token, "newznab").await {
        return StatusCode::NOT_FOUND.into_response();
    }

    let op = params.t.unwrap_or_default().to_lowercase();
    let indexer_cfg = {
        let store = state.config.read().await;
        store.settings().indexers.clone()
    };
    if op.is_empty() || op == "caps" {
        return xml_response(caps_xml(advertised_limit(&indexer_cfg)));
    }

    let base_url = {
        let store = state.config.read().await;
        public_base_url(&headers, &uri, &store.settings().network.public_url)
    };

    let ct = CancellationToken::new();
    let result = if op == "movie"
        && let Some(imdbid) = params.imdbid.as_deref().filter(|s| !s.trim().is_empty())
    {
        let id = if imdbid.starts_with("tt") {
            imdbid.to_owned()
        } else {
            format!("tt{imdbid}")
        };
        state
            .service
            .search_by_imdb(&token, "movie", &id, &ct, params.q.as_deref(), false, true)
            .await
    } else if op == "tvsearch"
        && let (Some(imdbid), Some(season), Some(ep)) =
            (params.imdbid.as_deref(), params.season, params.ep)
        && !imdbid.trim().is_empty()
    {
        let digits = imdbid.strip_prefix("tt").unwrap_or(imdbid);
        let id = format!("tt{digits}:{season}:{ep}");
        state
            .service
            .search_by_imdb(&token, "series", &id, &ct, params.q.as_deref(), false, true)
            .await
    } else {
        Some(infinidysk_profiles::SearchOutcome {
            profile_token: token.clone(),
            r#type: op.clone(),
            id: String::new(),
            candidates: Vec::new(),
            play_tokens: Vec::new(),
        })
    };

    let Some(result) = result else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let items: Vec<(infinidysk_profiles::cache::Candidate, String)> = result
        .candidates
        .into_iter()
        .zip(result.play_tokens.iter().cloned())
        .collect();
    xml_response(feed_xml(&profile, &base_url, &token, &items))
}

// --- GET|OPTIONS /adapters/addon/{token}/manifest.json ---

const ADDON_LOGO: &str =
    "https://raw.githubusercontent.com/infinidysk/infinidysk/main/docs/assets/logo.png";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AddonManifest {
    id: String,
    version: String,
    name: String,
    description: String,
    logo: String,
    resources: Vec<String>,
    types: Vec<String>,
    id_prefixes: Vec<String>,
    behavior_hints: BehaviorHints,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BehaviorHints {
    configurable: bool,
    configuration_required: bool,
}

fn cors_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        "*".parse().expect("static header"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, OPTIONS".parse().expect("static header"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "*".parse().expect("static header"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        "no-store, no-cache, must-revalidate"
            .parse()
            .expect("static header"),
    );
    response
}

async fn manifest_options() -> Response {
    cors_headers(StatusCode::NO_CONTENT.into_response())
}

async fn manifest(State(state): State<Arc<Search>>, Path(token): Path<String>) -> Response {
    state.sync_settings().await;
    let Some(profile) = state.service.get_profile(&token).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !state.service.is_adapter_enabled(&token, "addon").await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let name = if profile.name.trim().is_empty() {
        "InfiniDysk Search Profile".to_owned()
    } else {
        profile.name.clone()
    };
    let manifest = AddonManifest {
        id: format!("nzbdav.profile.{token}"),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        name,
        description:
            "Playable Usenet streams from this InfiniDysk Search Profile's configured indexers."
                .to_owned(),
        logo: ADDON_LOGO.to_owned(),
        resources: vec!["stream".to_owned()],
        types: vec!["movie".to_owned(), "series".to_owned()],
        id_prefixes: ["tt", "tmdb", "tvdb", "kitsu", "mal", "anilist"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        behavior_hints: BehaviorHints {
            configurable: false,
            configuration_required: false,
        },
    };
    cors_headers(Json(manifest).into_response())
}
