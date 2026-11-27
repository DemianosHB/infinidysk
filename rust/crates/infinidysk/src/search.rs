//! v1-fork search routes, ported from `backend/Api/Controllers/SearchIndexers` and
//! `backend/Api/Controllers/Profiles`:
//! - `GET /api/search-indexers?q&limit` — raw Newznab search across all enabled indexers
//!   (admin API key, like v1's `BaseApiController`).
//! - `GET /adapters/newznab/{token}/api` — Newznab adapter for a profile (token-scoped, public).
//! - `GET|OPTIONS /adapters/addon/{token}/manifest.json` — Stremio-style addon manifest
//!   (token-scoped, public, CORS).
//! - `GET /api/search/{token}/nzb/{playToken}.nzb` — NZB proxy (v1 `NzbProxyController`).
//! - `GET|OPTIONS /api/search/{token}/lookup` — JSON search (v1 `JsonSearchController`).
//! - `POST|OPTIONS /adapters/addon/{token}/failover_order` — reported stream order
//!   (v1 `FailoverOrderController`).
//!
//! The `stream/{type}/{id}.json` and `play/{nzbToken}.mkv` routes need the Phase-3
//! stream-state/preflight services and land with Phase 3.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, Query, Request, State};
use axum::http::{Extensions, HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use jiff::Timestamp;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use infinidysk_indexer::NewznabClient;
use infinidysk_indexer::http;
use infinidysk_indexer::models::IndexerConfig;
use infinidysk_profiles::filename;
use infinidysk_profiles::hits::HitType;
use infinidysk_profiles::preferred::{self, PreferredOrderStore};
use infinidysk_profiles::resolvers::ResolverConfig;
use infinidysk_profiles::{Resolvers, SearchService, SearchSettings};
use infinidysk_search::profile::Profile;

use crate::auth::Auth;
use crate::config::{ConfigStore, Settings};
use crate::nzb_fetch::{FetchError, FetchKey, MAX_RESPONSE_BYTES, NzbFetchCoalescer};
use crate::sab::same;

/// Shared state for the search routes.
pub struct Search {
    pub service: Arc<SearchService>,
    pub config: Arc<RwLock<ConfigStore>>,
    pub auth: Arc<Auth>,
    pub coalescer: Arc<NzbFetchCoalescer>,
    pub preferred_order: Arc<PreferredOrderStore>,
}

/// Builds the [`SearchSettings`] snapshot for the service from the app settings.
pub fn search_settings(s: &Settings) -> SearchSettings {
    SearchSettings {
        profiles: s.profiles.clone().normalized(),
        indexers: s.indexers.clone(),
        exclude_patterns_raw: s.search.exclude_patterns.clone(),
        search_user_agent: s.search.search_user_agent.clone(),
        user_agent: s.search.user_agent.clone(),
        watchtower_enabled: false, // Phase 3 (Watchtower service)
        warden_hide_dead: s.warden.hide_dead,
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
    build_with_hooks(config, auth, None, None).await
}

/// [`build`] with the Phase-3 hooks: Warden dead-filter and the preflight starter.
pub async fn build_with_hooks(
    config: Arc<RwLock<ConfigStore>>,
    auth: Arc<Auth>,
    is_dead: Option<infinidysk_profiles::IsDeadHook>,
    on_preflight: Option<infinidysk_profiles::PreflightHook>,
) -> Arc<Search> {
    let settings = config.read().await.settings().clone();
    let service = Arc::new(service_from_settings(&settings).with_hooks(is_dead, on_preflight));
    build_with_service(config, auth, service)
}

/// [`build`] around an already-built service (so Phase-3 services can share it).
pub fn build_with_service(
    config: Arc<RwLock<ConfigStore>>,
    auth: Arc<Auth>,
    service: Arc<SearchService>,
) -> Arc<Search> {
    Arc::new(Search {
        service,
        config,
        auth,
        coalescer: Arc::new(NzbFetchCoalescer::new()),
        preferred_order: Arc::new(PreferredOrderStore::new()),
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
        // axum allows no static suffix on a parameter segment, so the `.nzb` suffix is
        // matched via a wildcard and checked in the handler (v1 route: `{playToken}.nzb`).
        .route("/api/search/{token}/nzb/{*file}", get(nzb_proxy))
        .route(
            "/api/search/{token}/lookup",
            get(lookup).options(lookup_options),
        )
        .route(
            "/adapters/addon/{token}/failover_order",
            post(failover_order).options(failover_options),
        )
        .with_state(state)
}

impl Search {
    /// Pushes the current settings into the service when they changed since the last request.
    pub(crate) async fn sync_settings(&self) {
        let snapshot = {
            let store = self.config.read().await;
            search_settings(store.settings())
        };
        self.service.apply_settings(snapshot).await;
    }
}

/// v1 `ApiKeyValidator`: exempt like the admin routes (`Auth::exempt`), session cookie /
/// admin key via `Auth::allowed`, or the SAB-style `apikey` query param / `X-Api-Key`
/// header matching `sab.api_key` or `auth.api_key`.
pub(crate) async fn api_key_allowed(
    config: &Arc<RwLock<ConfigStore>>,
    auth: &Arc<Auth>,
    headers: &HeaderMap,
    extensions: &Extensions,
    query: Option<&str>,
) -> bool {
    if auth.exempt(headers, extensions).await || auth.allowed(headers).await {
        return true;
    }
    let given = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            query.and_then(|q| {
                q.split('&').find_map(|p| {
                    p.split_once('=')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("apikey"))
                        .map(|(_, v)| v.to_owned())
                })
            })
        });
    if let Some(given) = given {
        let (sab, admin) = {
            let store = config.read().await;
            let s = store.settings();
            let keys = (s.sab.api_key.0.clone(), s.auth.api_key.0.clone());
            drop(store);
            keys
        };
        if (!sab.is_empty() && same(given.as_bytes(), sab.as_bytes()))
            || (!admin.is_empty() && same(given.as_bytes(), admin.as_bytes()))
        {
            return true;
        }
    }
    false
}

async fn require_api_key(State(state): State<Arc<Search>>, req: Request, next: Next) -> Response {
    let allowed = api_key_allowed(
        &state.config,
        &state.auth,
        req.headers(),
        req.extensions(),
        req.uri().query(),
    )
    .await;
    if allowed {
        return next.run(req).await;
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
                tracing::warn!(indexer = %x.name, "Indexer search failed: rate limiter wait failed.");
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
                    tracing::warn!(reason = %e, indexer = %x.name, "Indexer search failed: invalid indexer configuration.");
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
                        infinidysk_indexer::Error::Indexer(m) => {
                            // Newznab <error> protocol element (e.g. "[100] ..."): a
                            // known failure, logged like the response errors below.
                            tracing::warn!(reason = %e, indexer = %x.name, "Indexer search failed.");
                            m.clone()
                        }
                        other => {
                            if matches!(
                                other,
                                infinidysk_indexer::Error::Status(_)
                                    | infinidysk_indexer::Error::TooLarge { .. }
                                    | infinidysk_indexer::Error::Format { .. }
                            ) {
                                // Known failure: one human-friendly warning with the
                                // controlled reason, no backtrace (the message carries
                                // safe response metadata but never URL/key/body).
                                tracing::warn!(reason = %other, indexer = %x.name, "Indexer search failed.");
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

pub(crate) fn public_base_url(headers: &HeaderMap, uri: &Uri, configured: &str) -> String {
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

pub(crate) fn cors_headers(mut response: Response) -> Response {
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

// --- GET /api/search/{token}/nzb/{playToken}.nzb (v1 NzbProxyController) ---

/// v1 `ProfileReleaseName.SanitizeFileName`: invalid filename chars become `_`, trimmed,
/// empty → "untitled".
pub(crate) fn sanitize_file_name(title: &str) -> String {
    if title.trim().is_empty() {
        return "untitled".to_owned();
    }
    let clean: String = title
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect::<String>()
        .trim()
        .to_owned();
    if clean.is_empty() {
        "untitled".to_owned()
    } else {
        clean
    }
}

/// v1 `ProfileReleaseName.ToNzbFileName`: `SanitizeFileName` plus `.nzb`.
pub(crate) fn to_nzb_file_name(title: &str) -> String {
    format!("{}.nzb", sanitize_file_name(title))
}

/// v1 `NzbProxyController.Get`: resolve the play token, then fetch (coalesced, bounded)
/// the NZB from the source indexer and serve it as an attachment.
async fn nzb_proxy(
    State(state): State<Arc<Search>>,
    Path((token, file)): Path<(String, String)>,
) -> Response {
    let Some(play_token) = file.strip_suffix(".nzb") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let play_token = play_token.to_owned();
    state.sync_settings().await;
    if state.service.get_profile(&token).await.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !state.service.is_adapter_enabled(&token, "json").await
        && !state.service.is_adapter_enabled(&token, "newznab").await
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(entry) = state.service.cache().get(&play_token) else {
        return (StatusCode::NOT_FOUND, "Link expired. Re-search to refresh.").into_response();
    };
    if entry.profile_token != token {
        return StatusCode::NOT_FOUND.into_response();
    }
    let candidate = entry.primary().clone();
    if candidate.nzb_url.trim().is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }

    let skip_tls = {
        let store = state.config.read().await;
        store
            .settings()
            .indexers
            .should_skip_tls_verification(&candidate.indexer_name)
    };
    let key = FetchKey {
        url: candidate.nzb_url.clone(),
        proxy_url: candidate.proxy_url.clone().unwrap_or_default(),
        skip_tls_verification: skip_tls,
    };
    let ct = CancellationToken::new();
    let ua = candidate.indexer_user_agent.clone();
    let proxy = candidate.proxy_url.clone();
    let url = candidate.nzb_url.clone();
    let result = state
        .coalescer
        .get_or_fetch(key, &ct, move |hard| {
            let url = url.clone();
            let ua = ua.clone();
            let proxy = proxy.clone();
            async move {
                // v1: linked token with CancelAfter(FetchTimeout = 60s).
                let fetch_ct = CancellationToken::new();
                let link = hard.clone();
                let watcher = fetch_ct.clone();
                tokio::spawn(async move {
                    link.cancelled().await;
                    watcher.cancel();
                });
                let host = Url::parse(&url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default();
                let client = http::build_client_with_proxy(skip_tls, &host, proxy.as_deref())
                    .map_err(|_| FetchError::Failed)?;
                let resp = http::with_timeout(
                    std::time::Duration::from_secs(60),
                    &fetch_ct,
                    http::get(&client, &url, &ua, &fetch_ct, 1),
                )
                .await
                .map_err(|e| match e {
                    infinidysk_indexer::Error::Cancelled => FetchError::Cancelled,
                    _ => FetchError::Failed,
                })?;
                if !resp.status().is_success() {
                    return Ok(None);
                }
                match http::read_bounded(resp, MAX_RESPONSE_BYTES).await {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(infinidysk_indexer::Error::TooLarge { max_bytes, .. }) => {
                        tracing::warn!(
                            url = %url,
                            limit = max_bytes,
                            "NZB proxy rejected oversized response"
                        );
                        Ok(None)
                    }
                    Err(infinidysk_indexer::Error::Cancelled) => Err(FetchError::Cancelled),
                    Err(_) => Err(FetchError::Failed),
                }
            }
        })
        .await;

    match result {
        Err(FetchError::Cancelled) => StatusCode::OK.into_response(),
        Err(FetchError::Failed) => (
            StatusCode::BAD_GATEWAY,
            "Failed to fetch NZB from source indexer.",
        )
            .into_response(),
        Ok(None) => (
            StatusCode::BAD_GATEWAY,
            "Source indexer failed to return the NZB.",
        )
            .into_response(),
        Ok(Some(bytes)) => {
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/x-nzb")],
                bytes.as_ref().clone(),
            )
                .into_response();
            let disposition = axum::http::HeaderValue::from_str(&format!(
                "attachment; filename=\"{}\"",
                to_nzb_file_name(&candidate.title)
            ))
            .expect("header value");
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, disposition);
            response
        }
    }
}

// --- GET|OPTIONS /api/search/{token}/lookup (v1 JsonSearchController) ---

#[derive(Debug, Deserialize)]
struct LookupQuery {
    r#type: Option<String>,
    id: Option<String>,
    season: Option<i32>,
    episode: Option<i32>,
    q: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LookupItem {
    title: String,
    indexer: String,
    source_indexer: Option<String>,
    size_bytes: i64,
    posted_at: Option<Timestamp>,
    usenet_date: Option<Timestamp>,
    grabs: Option<i32>,
    password_flag: Option<i32>,
    language: Option<String>,
    subs: Option<String>,
    info_hash: Option<String>,
    play_url: String,
}

async fn lookup_options() -> Response {
    cors_headers(StatusCode::NO_CONTENT.into_response())
}

async fn lookup(
    State(state): State<Arc<Search>>,
    Path(token): Path<String>,
    Query(params): Query<LookupQuery>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    if state.service.get_profile(&token).await.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !state.service.is_adapter_enabled(&token, "json").await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (Some(r#type), Some(id)) = (params.r#type, params.id) else {
        return cors_headers(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "type and id are required (type=movie|series, id=tt1234567)."})),
            )
                .into_response(),
        );
    };
    if r#type.is_empty() || id.trim().is_empty() {
        return cors_headers(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "type and id are required (type=movie|series, id=tt1234567)."})),
            )
                .into_response(),
        );
    }
    // v1: series with season+episode and a bare id becomes "{id}:{season}:{episode}".
    let id = if r#type.eq_ignore_ascii_case("series")
        && let (Some(season), Some(episode)) = (params.season, params.episode)
        && !id.contains(':')
    {
        format!("{id}:{season}:{episode}")
    } else {
        id
    };

    state.sync_settings().await;
    let ct = CancellationToken::new();
    let Some(result) = state
        .service
        .search_by_imdb(&token, &r#type, &id, &ct, params.q.as_deref(), false, true)
        .await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let base_url = {
        let store = state.config.read().await;
        public_base_url(&headers, &uri, &store.settings().network.public_url)
    };
    let profile_name = state
        .service
        .get_profile(&token)
        .await
        .map(|p| p.name)
        .unwrap_or_default();
    let items: Vec<LookupItem> = result
        .candidates
        .iter()
        .zip(result.play_tokens.iter())
        .map(|(c, play_token)| LookupItem {
            title: c.title.clone(),
            indexer: c.indexer_name.clone(),
            source_indexer: c.source_indexer_name.clone(),
            size_bytes: c.size,
            posted_at: c.posted,
            usenet_date: c.usenet_date,
            grabs: c.grabs,
            password_flag: c.password,
            language: c.language.clone(),
            subs: c.subs.clone(),
            info_hash: c.info_hash.clone(),
            play_url: format!("{base_url}/api/search/{token}/play/{play_token}.mkv"),
        })
        .collect();
    let count = items.len();
    cors_headers(
        Json(serde_json::json!({
            "profile": profile_name,
            "type": result.r#type,
            "id": result.id,
            "count": count,
            "results": items,
        }))
        .into_response(),
    )
}

// --- POST|OPTIONS /adapters/addon/{token}/failover_order (v1 FailoverOrderController) ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FailoverOrderRequest {
    streams: Option<Vec<FailoverStream>>,
    order: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FailoverStream {
    #[allow(dead_code)]
    name: Option<String>,
    failover_id: Option<String>,
}

fn cors_headers_with_methods(mut response: Response, methods: &'static str) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        "*".parse().expect("static header"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        methods.parse().expect("static header"),
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

async fn failover_options() -> Response {
    cors_headers_with_methods(StatusCode::NO_CONTENT.into_response(), "POST, OPTIONS")
}

async fn failover_order(
    State(state): State<Arc<Search>>,
    Path(token): Path<String>,
    Json(body): Json<FailoverOrderRequest>,
) -> Response {
    if state.service.get_profile(&token).await.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !state.service.is_adapter_enabled(&token, "addon").await {
        return StatusCode::NOT_FOUND.into_response();
    }
    // v1: `streams[*].failoverId` when `streams` is present, else `order`; blank entries dropped.
    let ids: Vec<String> = match &body.streams {
        Some(streams) => streams
            .iter()
            .filter_map(|s| s.failover_id.clone())
            .filter(|x| !x.trim().is_empty())
            .collect(),
        None => body
            .order
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|x| !x.trim().is_empty())
            .collect(),
    };
    if ids.is_empty() {
        return cors_headers_with_methods(
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": "no failoverIds in body"})),
            )
                .into_response(),
            "POST, OPTIONS",
        );
    }

    let mut groups: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    let mut seen: std::collections::HashMap<(String, String), std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    let mut matched = 0usize;
    for id in &ids {
        let Some(entry) = state.service.cache().get(id) else {
            continue;
        };
        if entry.profile_token != token {
            continue;
        }
        let c = entry.primary();
        let key = preferred::release_identity_key(
            c.size,
            c.poster.as_deref(),
            c.usenet_date,
            Some(&c.nzb_url),
        );
        let gk = (entry.r#type.clone(), entry.id.clone());
        let list = groups.entry(gk.clone()).or_default();
        let seen_set = seen.entry(gk).or_default();
        if seen_set.insert(key.clone()) {
            list.push(key);
            matched += 1;
        }
    }
    for ((r#type, id), keys) in &groups {
        state
            .preferred_order
            .report(&token, r#type, id, "aiostreams", keys);
    }
    if !groups.is_empty() {
        tracing::info!(
            profile_token = %token,
            matched,
            total = ids.len(),
            titles = groups.len(),
            "FailoverOrder reported"
        );
    }
    let titles: Vec<serde_json::Value> = groups
        .iter()
        .map(|((r#type, id), keys)| {
            serde_json::json!({"type": r#type, "id": id, "count": keys.len()})
        })
        .collect();
    cors_headers_with_methods(
        Json(serde_json::json!({
            "ok": true,
            "total": ids.len(),
            "matched": matched,
            "unmatched": ids.len() - matched,
            "titles": titles,
        }))
        .into_response(),
        "POST, OPTIONS",
    )
}
