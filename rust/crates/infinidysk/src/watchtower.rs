//! v1-fork Watchtower, ported from `backend/Services/Watchtower/*` and
//! `backend/Api/Controllers/Watchtower/*`: list sources (Stremio catalogs, URL lists) feed
//! `wanted_items`; a background loop syncs sources, expands bare series into episode rows,
//! resolves due items (search + verify + shortlist, ready/unavailable/parked), and keeps
//! ready items fresh with backoff. `GET /api/get-watchtower`, `POST /api/watchtower-mutate`,
//! `POST /api/watchtower-discover-catalogs` are the admin routes (API-key gated like v1's
//! `BaseApiController`).
//!
//! Deviations from v1 (documented in `PORTING.md`): the `ActiveCycle` supervision machinery
//! (`CycleLifetime`) is replaced by a plain 15-minute per-cycle timeout; EF change-tracker
//! detach semantics become read-modify-write under `Db::write_permit`; the benchmark-gate
//! pause is not wired (no speed-test gate exists yet); episode enumeration hits TVmaze/Kitsu
//! directly (v1's Wikidata path for non-IMDb ids is not ported — `IMDb` and kitsu: ids only).

// The v1 controllers answer with a ready axum `Response` in the `Err` variant so handlers
// stay terse and the rejection response is built once at the rejection site.
#![allow(clippy::result_large_err)]

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Form, FromRequest as _, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;

use infinidysk_db::Db;
use infinidysk_db::watchtower::{
    self, KIND_MANUAL, KIND_STREMIO_CATALOG, KIND_URL_LIST, ListSourceRow, STATE_EXPANDER,
    STATE_PARKED, STATE_READY, STATE_SCOUTING, STATE_UNAVAILABLE, WantedItemRow,
};
use infinidysk_profiles::cache::Candidate;
use infinidysk_profiles::hits::HitType;
use infinidysk_profiles::preferred::{self, PreferredOrderStore};

use crate::auth::Auth;
use crate::config::{ConfigStore, Settings};
use crate::nzb_fetch::{FetchError, FetchKey, MAX_RESPONSE_BYTES};
use crate::preflight::{Orchestrator, Verdict};
use crate::search::{self, Search};
use crate::warden::Warden;

/// v1 `WatchtowerService.CycleInterval`.
const CYCLE_INTERVAL: Duration = Duration::from_secs(20);
/// v1 `WatchtowerService.AdmissionPollInterval`.
const ADMISSION_POLL: Duration = Duration::from_secs(10);
/// v1 `WatchtowerService.CycleWatchdogTimeout`.
const CYCLE_WATCHDOG: Duration = Duration::from_mins(15);
/// v1 `WatchtowerService.NzbFetchTimeout`.
const NZB_FETCH_TIMEOUT: Duration = Duration::from_secs(15);
/// v1 `ListSourceEnumerator.FetchTimeout`.
const LIST_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// v1 `ListSourceEnumerator.MaxCatalogPages`.
const MAX_CATALOG_PAGES: usize = 100;
const RESOLVES_PER_TICK: i64 = 3;
const AUTO_RESOLVE_BATCH: i64 = 25;
const AUTO_STAGE_BUDGET: Duration = Duration::from_secs(120);
const KEEP_FRESH_PER_TICK: i64 = 5;
const EXPANDS_PER_TICK: i64 = 5;
/// v1 `SeasonBundleGraceSeconds` (14 days).
const SEASON_BUNDLE_GRACE_SECS: i64 = 14 * 86_400;

// --- v1 WatchtowerModels ---

/// v1 `WtPointer`. Serialized `PascalCase` like v1's default STJ options so stored
/// `shortlist` JSON stays compatible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct WtPointer {
    pub nzb_url: String,
    pub indexer_name: String,
    #[serde(default = "default_ua")]
    pub indexer_user_agent: String,
    pub proxy_url: Option<String>,
    pub title: String,
    pub size: i64,
    pub grabs: Option<i32>,
    pub poster: Option<String>,
    pub usenet_date: Option<Timestamp>,
    #[serde(default = "default_verdict")]
    pub verdict: String,
    pub last_verified_at_unix: Option<i64>,
}

fn default_ua() -> String {
    "NzbDav".to_owned()
}

fn default_verdict() -> String {
    "unknown".to_owned()
}

/// v1 `WtContentRef`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WtContentRef {
    pub r#type: String,
    pub content_id: String,
    pub title: Option<String>,
}

fn read_pointers(json: &str) -> Vec<WtPointer> {
    serde_json::from_str(json).unwrap_or_default()
}

fn write_pointers(pointers: &[WtPointer]) -> String {
    serde_json::to_string(pointers).unwrap_or_else(|_| "[]".to_owned())
}

fn read_strings(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

fn write_strings(values: &[String]) -> String {
    serde_json::to_string(values).unwrap_or_else(|_| "[]".to_owned())
}

/// v1 `WtReconcile.ExpanderTag`.
fn expander_tag(expander_key: &str) -> String {
    format!("exp:{expander_key}")
}

// --- v1 EpisodeEnumerator ---

#[derive(Debug, Clone)]
pub struct Episode {
    pub season: i32,
    pub number: i32,
    pub air_date_unix: Option<i64>,
    pub title: Option<String>,
}

struct CacheEntry {
    episodes: Vec<Episode>,
    expires_at: Timestamp,
}

/// v1 `EpisodeEnumerator`: `TVmaze` (by `IMDb` id) and Kitsu episode lists, 24 h cache
/// (15 min negative).
pub struct EpisodeEnumerator {
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl EpisodeEnumerator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn enumerate_imdb(&self, imdb_id: &str, ct: &CancellationToken) -> Vec<Episode> {
        let Some(imdb) = normalize_imdb(imdb_id) else {
            return Vec::new();
        };
        if let Some(cached) = self.cached(&imdb) {
            return cached;
        }
        let episodes = fetch_tvmaze(&imdb, ct).await;
        let ttl = if episodes.is_empty() {
            Duration::from_mins(15)
        } else {
            Duration::from_hours(24)
        };
        self.store(
            imdb,
            CacheEntry {
                episodes: episodes.clone(),
                expires_at: Timestamp::now() + ttl,
            },
        );
        episodes
    }

    pub async fn enumerate_kitsu(&self, kitsu_id: &str, ct: &CancellationToken) -> Vec<Episode> {
        if kitsu_id.is_empty() || !kitsu_id.bytes().all(|b| b.is_ascii_digit()) {
            return Vec::new();
        }
        let key = format!("kitsu:{kitsu_id}");
        if let Some(cached) = self.cached(&key) {
            return cached;
        }
        let episodes = fetch_kitsu(kitsu_id, ct).await;
        let ttl = if episodes.is_empty() {
            Duration::from_mins(15)
        } else {
            Duration::from_hours(24)
        };
        self.store(
            key,
            CacheEntry {
                episodes: episodes.clone(),
                expires_at: Timestamp::now() + ttl,
            },
        );
        episodes
    }

    fn cached(&self, key: &str) -> Option<Vec<Episode>> {
        let mut cache = self.cache.lock().expect("lock");
        if let Some(entry) = cache.get(key) {
            if entry.expires_at > Timestamp::now() {
                return Some(entry.episodes.clone());
            }
            cache.remove(key);
        }
        None
    }

    fn store(&self, key: String, entry: CacheEntry) {
        self.cache.lock().expect("lock").insert(key, entry);
    }
}

impl Default for EpisodeEnumerator {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize_imdb(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    if let Some(colon) = s.find(':') {
        s = &s[..colon];
    }
    if let Some(rest) = s.strip_prefix("tt").or_else(|| s.strip_prefix("TT")) {
        s = rest;
    }
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        Some(format!("tt{s}"))
    } else {
        None
    }
}

/// v1 `CanonicalImdb` (without the `tt` prefix requirement for display).
fn canonical_imdb(content_id: &str) -> String {
    let mut s = content_id;
    if let Some(colon) = s.find(':') {
        s = &s[..colon];
    }
    if s.len() >= 2 && s[..2].eq_ignore_ascii_case("tt") {
        s.to_owned()
    } else {
        format!("tt{s}")
    }
}

fn is_imdb_id(content_id: &str) -> bool {
    normalize_imdb(content_id).is_some()
}

/// True when a stored title is just a raw-id fallback (empty, equal to the content id,
/// or prefixed by the imdb id) instead of a resolved display title.
fn is_unresolved_title(title: &str, content_id: &str) -> bool {
    let t = title.trim();
    if t.is_empty() || t.eq_ignore_ascii_case(content_id) {
        return true;
    }
    let base = content_id.split(':').next().unwrap_or("");
    is_imdb_id(base)
        && t.get(..base.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(base))
}

/// Composes the display title for an imdb-shaped content id from the resolved base title.
fn compose_imdb_title(base_title: &str, content_id: &str) -> String {
    let parts: Vec<&str> = content_id.split(':').collect();
    if parts.len() >= 3
        && let (Ok(season), Ok(episode)) = (parts[1].parse::<i32>(), parts[2].parse::<i32>())
    {
        return format!("{base_title} S{season:02}E{episode:02}");
    }
    if parts.len() == 2
        && let Ok(season) = parts[1].parse::<i32>()
    {
        return format!("{base_title} S{season:02}");
    }
    base_title.to_owned()
}

async fn http_get_json(
    client: &reqwest::Client,
    url: &str,
    ua: &str,
    timeout: Duration,
    ct: &CancellationToken,
) -> Option<serde_json::Value> {
    let resp = http_get(client, url, ua, timeout, ct).await?;
    if !resp.status().is_success() {
        return None;
    }
    let Ok(bytes) = read_bounded(resp, ct).await else {
        return None;
    };
    serde_json::from_slice(&bytes).ok()
}

async fn http_get(
    client: &reqwest::Client,
    url: &str,
    ua: &str,
    timeout: Duration,
    ct: &CancellationToken,
) -> Option<reqwest::Response> {
    let fut = client
        .get(url)
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
        .send();
    tokio::select! {
        () = ct.cancelled() => None,
        () = tokio::time::sleep(timeout) => None,
        r = fut => r.ok(),
    }
}

async fn read_bounded(resp: reqwest::Response, ct: &CancellationToken) -> Result<Vec<u8>, ()> {
    tokio::select! {
        () = ct.cancelled() => Err(()),
        r = resp.bytes() => r.map(|b| b.to_vec()).map_err(|_| ()),
    }
}

async fn fetch_tvmaze(imdb: &str, ct: &CancellationToken) -> Vec<Episode> {
    let client = reqwest::Client::new();
    let Some(show) = http_get_json(
        &client,
        &format!("https://api.tvmaze.com/lookup/shows?imdb={imdb}"),
        "NzbDav-Watchtower",
        Duration::from_secs(8),
        ct,
    )
    .await
    else {
        return Vec::new();
    };
    let Some(show_id) = show.get("id").and_then(serde_json::Value::as_i64) else {
        return Vec::new();
    };
    let Some(arr) = http_get_json(
        &client,
        &format!("https://api.tvmaze.com/shows/{show_id}/episodes"),
        "NzbDav-Watchtower",
        Duration::from_secs(8),
        ct,
    )
    .await
    .and_then(|v| v.as_array().cloned()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for el in &arr {
        let (Some(season), Some(number)) = (
            el.get("season").and_then(serde_json::Value::as_i64),
            el.get("number").and_then(serde_json::Value::as_i64),
        ) else {
            continue;
        };
        if season < 1 || number < 1 {
            continue;
        }
        let title = el.get("name").and_then(|v| v.as_str()).map(str::to_owned);
        let air = el
            .get("airstamp")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Timestamp>().ok())
            .map(jiff::Timestamp::as_second)
            .or_else(|| {
                el.get("airdate")
                    .and_then(|v| v.as_str())
                    .and_then(|s| format!("{s}T00:00:00Z").parse::<Timestamp>().ok())
                    .map(jiff::Timestamp::as_second)
            });
        out.push(Episode {
            season: i32::try_from(season).unwrap_or(0),
            number: i32::try_from(number).unwrap_or(0),
            air_date_unix: air,
            title,
        });
    }
    out.sort_by(|a, b| a.season.cmp(&b.season).then(a.number.cmp(&b.number)));
    out
}

async fn fetch_kitsu(kitsu_id: &str, ct: &CancellationToken) -> Vec<Episode> {
    let client = reqwest::Client::new();
    let mut result = Vec::new();
    for page in 0..6 {
        let url = format!(
            "https://kitsu.io/api/edge/anime/{kitsu_id}/episodes?sort=-number&page%5Blimit%5D=20&page%5Boffset%5D={}",
            page * 20
        );
        let Some(doc) = http_get_json(
            &client,
            &url,
            "NzbDav-Watchtower",
            Duration::from_secs(8),
            ct,
        )
        .await
        else {
            break;
        };
        let Some(data) = doc.get("data").and_then(|v| v.as_array()) else {
            break;
        };
        let page_count = data.len();
        for el in data {
            let Some(attrs) = el.get("attributes") else {
                continue;
            };
            let Some(number) = attrs.get("number").and_then(serde_json::Value::as_i64) else {
                continue;
            };
            if number < 1 {
                continue;
            }
            let air = attrs
                .get("airDate")
                .and_then(|v| v.as_str())
                .and_then(|s| format!("{s}T00:00:00Z").parse::<Timestamp>().ok())
                .map(jiff::Timestamp::as_second);
            let title = attrs
                .get("canonicalTitle")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            result.push(Episode {
                season: 1,
                number: i32::try_from(number).unwrap_or(0),
                air_date_unix: air,
                title,
            });
        }
        if page_count < 20 {
            break;
        }
    }
    result.sort_by_key(|e| e.number);
    result
}

// --- v1 ListSourceEnumerator ---

pub struct CatalogChoice {
    pub r#type: String,
    pub id: String,
    pub name: String,
    pub url: String,
    pub extra_required: Option<String>,
}

pub struct DiscoverResult {
    pub addon_name: Option<String>,
    pub catalogs: Vec<CatalogChoice>,
}

/// v1 `ListSourceEnumerator`: Stremio catalog paging, URL lists, catalog discovery.
pub struct ListSourceEnumerator {
    max_response_bytes: u64,
}

impl ListSourceEnumerator {
    #[must_use]
    pub fn new(max_response_bytes: u64) -> Self {
        Self { max_response_bytes }
    }

    /// v1 `EnumerateAsync`.
    pub async fn enumerate(
        &self,
        source: &ListSourceRow,
        ct: &CancellationToken,
    ) -> Result<Vec<WtContentRef>, String> {
        match source.kind.as_str() {
            KIND_STREMIO_CATALOG => {
                self.fetch_stremio_catalog(&source.name, source.url.as_deref(), source.cap, ct)
                    .await
            }
            KIND_URL_LIST => self.fetch_url_list(source.url.as_deref(), ct).await,
            _ => Ok(Vec::new()),
        }
    }

    async fn fetch_stremio_catalog(
        &self,
        source_name: &str,
        url: Option<&str>,
        cap: i32,
        ct: &CancellationToken,
    ) -> Result<Vec<WtContentRef>, String> {
        let Some(url) = url.filter(|u| !u.trim().is_empty()) else {
            return Ok(Vec::new());
        };
        let mut refs: Vec<WtContentRef> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let limit = usize::try_from(cap).unwrap_or(usize::MAX);
        let mut page_size = 0usize;

        for page in 0..MAX_CATALOG_PAGES {
            let skip = if page_size > 0 { page * page_size } else { 0 };
            let body = match self.http_get_body(&build_paged_url(url, skip), ct).await {
                Ok(Some(b)) => b,
                Ok(None) => {
                    if page == 0 {
                        return Err(
                            "Catalog request failed or returned an empty response.".to_owned()
                        );
                    }
                    break;
                }
                Err(e) => return Err(e),
            };
            let doc: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|_| "The addon response was not valid JSON.".to_owned())?;
            let Some(metas) = doc.get("metas").and_then(|v| v.as_array()) else {
                if page > 0 {
                    break;
                }
                if doc.get("catalogs").is_some() || doc.get("resources").is_some() {
                    return Err("This URL is an addon manifest, not a catalog. Use \"Discover catalogs\" to pick which catalogs to add, or point this list at a catalog endpoint such as .../catalog/movie/<id>.json.".to_owned());
                }
                return Err("Catalog response did not contain a \"metas\" array.".to_owned());
            };
            let mut page_count = 0usize;
            let mut new_count = 0usize;
            for meta in metas {
                page_count += 1;
                let (Some(t), Some(id)) = (
                    meta.get("type").and_then(|v| v.as_str()),
                    meta.get("id").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                if t.trim().is_empty() || id.trim().is_empty() {
                    continue;
                }
                let key = format!("{}:{}", normalize_type(t), id);
                if !seen.insert(key) {
                    continue;
                }
                new_count += 1;
                refs.push(WtContentRef {
                    r#type: normalize_type(t),
                    content_id: id.to_owned(),
                    title: meta.get("name").and_then(|v| v.as_str()).map(str::to_owned),
                });
                if refs.len() >= limit {
                    return Ok(refs);
                }
            }
            if page_size == 0 {
                page_size = page_count;
            }
            if page_count == 0 || new_count == 0 || page_count < page_size {
                break;
            }
        }
        if page_size == 0 {
            info!(
                source = source_name,
                "watchtower: catalog source reached the {MAX_CATALOG_PAGES}-page ceiling"
            );
        }
        Ok(refs)
    }

    /// v1 `DiscoverCatalogsAsync`.
    pub async fn discover_catalogs(
        &self,
        url: &str,
        ct: &CancellationToken,
    ) -> Result<DiscoverResult, String> {
        if url.trim().is_empty() {
            return Err("A manifest URL is required.".to_owned());
        }
        let manifest_url = normalize_manifest_url(url.trim());
        let body = self
            .http_get_body(&manifest_url, ct)
            .await?
            .ok_or_else(|| "Could not fetch the addon manifest.".to_owned())?;
        let doc: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|_| "The addon response was not valid JSON.".to_owned())?;
        let Some(catalogs) = doc.get("catalogs").and_then(|v| v.as_array()) else {
            if doc.get("metas").is_some() {
                return Err("That looks like a catalog endpoint, not a manifest. Add it directly as a Stremio catalog list.".to_owned());
            }
            return Err("No catalogs were found in this addon manifest.".to_owned());
        };
        let addon_name = doc.get("name").and_then(|v| v.as_str()).map(str::to_owned);
        let base_url = strip_manifest_suffix(&manifest_url);
        let mut choices = Vec::new();
        for cat in catalogs {
            let (Some(t), Some(id)) = (
                cat.get("type").and_then(|v| v.as_str()),
                cat.get("id").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if t.trim().is_empty() || id.trim().is_empty() {
                continue;
            }
            let name = cat.get("name").and_then(|v| v.as_str());
            choices.push(CatalogChoice {
                r#type: t.to_owned(),
                id: id.to_owned(),
                name: name
                    .filter(|n| !n.trim().is_empty())
                    .map_or_else(|| format!("{t} · {id}"), str::to_owned),
                url: build_catalog_url(&base_url, t, id),
                extra_required: describe_required_extra(cat),
            });
        }
        if choices.is_empty() {
            return Err("This addon manifest lists no usable catalogs.".to_owned());
        }
        Ok(DiscoverResult {
            addon_name,
            catalogs: choices,
        })
    }

    async fn fetch_url_list(
        &self,
        url: Option<&str>,
        ct: &CancellationToken,
    ) -> Result<Vec<WtContentRef>, String> {
        let Some(url) = url.filter(|u| !u.trim().is_empty()) else {
            return Ok(Vec::new());
        };
        let (body, status) = self.http_get_body_with_status(url, ct).await?;
        let Some(body) = body else {
            return Err(match status {
                Some(code) => format!("List request failed (HTTP {code})."),
                None => "List request failed or returned an empty response.".to_owned(),
            });
        };
        if looks_like_json(&body) {
            parse_json_list(&body)
        } else {
            Ok(parse_plain_list(&String::from_utf8_lossy(&body)))
        }
    }

    async fn http_get_body(
        &self,
        url: &str,
        ct: &CancellationToken,
    ) -> Result<Option<Vec<u8>>, String> {
        self.http_get_body_with_status(url, ct)
            .await
            .map(|(body, _)| body)
    }

    async fn http_get_body_with_status(
        &self,
        url: &str,
        ct: &CancellationToken,
    ) -> Result<(Option<Vec<u8>>, Option<u16>), String> {
        let client = reqwest::Client::new();
        let resp = http_get(&client, url, "NzbDav-Watchtower", LIST_FETCH_TIMEOUT, ct)
            .await
            .ok_or_else(|| "List request failed.".to_owned())?;
        let status = resp.status();
        if !status.is_success() {
            return Ok((None, Some(status.as_u16())));
        }
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() as u64 > self.max_response_bytes {
            return Err(format!(
                "Response exceeded the {} byte cap.",
                self.max_response_bytes
            ));
        }
        Ok((Some(bytes.to_vec()), Some(status.as_u16())))
    }
}

fn normalize_manifest_url(url: &str) -> String {
    let mut url = if let Some(rest) = url
        .strip_prefix("stremio://")
        .or_else(|| url.strip_prefix("STREMIO://"))
    {
        format!("https://{rest}")
    } else {
        url.to_owned()
    };
    if !url.to_lowercase().ends_with(".json") {
        url = format!("{}/manifest.json", url.trim_end_matches('/'));
    }
    url
}

fn strip_manifest_suffix(manifest_url: &str) -> String {
    const SUFFIX: &str = "/manifest.json";
    if manifest_url.len() >= SUFFIX.len()
        && manifest_url[manifest_url.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
    {
        return manifest_url[..manifest_url.len() - SUFFIX.len()].to_owned();
    }
    match manifest_url.rfind('/') {
        Some(i) if i > "https://".len() => manifest_url[..i].to_owned(),
        _ => manifest_url.to_owned(),
    }
}

fn build_catalog_url(base_url: &str, r#type: &str, id: &str) -> String {
    format!(
        "{}/catalog/{}/{}.json",
        base_url,
        urlencoding_escape(r#type),
        urlencoding_escape(id)
    )
}

fn urlencoding_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// v1 `BuildPagedUrl`.
fn build_paged_url(url: &str, skip: usize) -> String {
    const EXT: &str = ".json";
    const MARKER: &str = "/catalog/";
    if skip == 0 {
        return url.to_owned();
    }
    if !url.to_lowercase().ends_with(EXT) {
        return if url.contains('?') {
            format!("{url}&skip={skip}")
        } else {
            format!("{url}?skip={skip}")
        };
    }
    let stem = &url[..url.len() - EXT.len()];
    let Some(marker_idx) = stem.to_lowercase().find(MARKER) else {
        return format!("{stem}/skip={skip}{EXT}");
    };
    let head = &stem[..marker_idx + MARKER.len()];
    let segments: Vec<&str> = stem[marker_idx + MARKER.len()..].split('/').collect();
    if segments.len() >= 3 && segments.last().is_some_and(|s| s.contains('=')) {
        let mut segs: Vec<String> = segments.iter().map(|s| (*s).to_owned()).collect();
        let last = segs.last_mut().expect("checked");
        let merged: Vec<String> = last
            .split('&')
            .filter(|p| !p.to_lowercase().starts_with("skip="))
            .map(str::to_owned)
            .chain([format!("skip={skip}")])
            .collect();
        *last = merged.join("&");
        return format!("{head}{}{EXT}", segs.join("/"));
    }
    format!("{stem}/skip={skip}{EXT}")
}

fn describe_required_extra(cat: &serde_json::Value) -> Option<String> {
    let mut names = Vec::new();
    if let Some(extra) = cat.get("extra").and_then(|v| v.as_array()) {
        for e in extra {
            if e.get("isRequired").and_then(serde_json::Value::as_bool) == Some(true)
                && let Some(nm) = e.get("name").and_then(|v| v.as_str())
                && !nm.trim().is_empty()
                && !nm.eq_ignore_ascii_case("skip")
            {
                names.push(nm.to_owned());
            }
        }
    } else if let Some(er) = cat.get("extraRequired").and_then(|v| v.as_array()) {
        for e in er {
            if let Some(nm) = e.as_str()
                && !nm.trim().is_empty()
                && !nm.eq_ignore_ascii_case("skip")
            {
                names.push(nm.to_owned());
            }
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

fn looks_like_json(body: &[u8]) -> bool {
    let mut i = 0;
    if body.len() >= 3 && body[0] == 0xEF && body[1] == 0xBB && body[2] == 0xBF {
        i = 3;
    }
    while i < body.len() {
        match body[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b => return b == b'[' || b == b'{',
        }
    }
    false
}

fn parse_json_list(body: &[u8]) -> Result<Vec<WtContentRef>, String> {
    let doc: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| "The list response was not valid JSON.".to_owned())?;
    let arr: Vec<serde_json::Value> = if doc.is_array() {
        doc.as_array().cloned().unwrap_or_default()
    } else if let Some(items) = doc.get("items").and_then(|v| v.as_array()) {
        items.clone()
    } else {
        Vec::new()
    };
    let mut refs = Vec::new();
    if !doc.is_array()
        && let Some(list) = doc.get("list").and_then(|v| v.as_str())
    {
        refs.extend(parse_plain_list(list));
    }
    for el in arr {
        if let Some(s) = el.as_str() {
            let (t, id) = split_type_id(s);
            if !id.is_empty() {
                refs.push(WtContentRef {
                    r#type: t,
                    content_id: id,
                    title: None,
                });
            }
        } else if el.is_object() {
            let id = el
                .get("id")
                .or_else(|| el.get("imdb"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !id.trim().is_empty() {
                refs.push(WtContentRef {
                    r#type: normalize_type(
                        el.get("type").and_then(|v| v.as_str()).unwrap_or("movie"),
                    ),
                    content_id: id.to_owned(),
                    title: el
                        .get("name")
                        .or_else(|| el.get("title"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                });
            } else if let Some(list) = el.get("list").and_then(|v| v.as_str()) {
                refs.extend(parse_plain_list(list));
            }
        }
    }
    Ok(refs)
}

fn parse_plain_list(body: &str) -> Vec<WtContentRef> {
    let mut refs = Vec::new();
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (t, id) = split_type_id(line);
        if !id.is_empty() {
            refs.push(WtContentRef {
                r#type: t,
                content_id: id,
                title: None,
            });
        }
    }
    refs
}

fn split_type_id(line: &str) -> (String, String) {
    if line.len() >= 2 && line[..2].eq_ignore_ascii_case("tt") && !line.contains(':') {
        return ("movie".to_owned(), line.to_owned());
    }
    if let Some(colon) = line.find(':')
        && colon > 0
    {
        let maybe = line[..colon].to_lowercase();
        if matches!(maybe.as_str(), "movie" | "series" | "tv" | "show") {
            return (normalize_type(&maybe), line[colon + 1..].to_owned());
        }
    }
    ("movie".to_owned(), line.to_owned())
}

fn normalize_type(t: &str) -> String {
    match t.trim().to_lowercase().as_str() {
        "series" | "tv" | "show" => "series".to_owned(),
        _ => "movie".to_owned(),
    }
}

// --- v1 WatchtowerService ---

/// Shared Watchtower state: background loop + store + routes.
pub struct Watchtower {
    pub config: Arc<RwLock<ConfigStore>>,
    pub auth: Arc<Auth>,
    pub db: Db,
    pub search: Arc<Search>,
    pub preflight: Arc<Orchestrator>,
    pub warden: Arc<Warden>,
    pub enumerator: ListSourceEnumerator,
    pub episodes: EpisodeEnumerator,
    /// Daily resolve budget counter (day key, count).
    budget: Mutex<(i64, u32)>,
}

/// Builds the Watchtower service (and its enumerators) from the current settings.
#[must_use]
pub async fn build(
    db: Db,
    config: Arc<RwLock<ConfigStore>>,
    auth: Arc<Auth>,
    search: Arc<Search>,
    preflight: Arc<Orchestrator>,
    warden: Arc<Warden>,
) -> Arc<Watchtower> {
    let max_bytes = config
        .read()
        .await
        .settings()
        .watchtower
        .list_source_max_response_bytes;
    Arc::new(Watchtower {
        config,
        auth,
        db,
        search,
        preflight,
        warden,
        enumerator: ListSourceEnumerator::new(max_bytes),
        episodes: EpisodeEnumerator::new(),
        budget: Mutex::new((-1, 0)),
    })
}

fn now() -> i64 {
    Timestamp::now().as_second()
}

impl Watchtower {
    async fn settings(&self) -> Settings {
        self.config.read().await.settings().clone()
    }

    /// v1 `ExecuteAsync` loop.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let enabled = self.settings().await.watchtower.enabled;
            if !enabled {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(ADMISSION_POLL) => {}
                }
                continue;
            }
            let this = Arc::clone(&self);
            let result = tokio::select! {
                () = cancel.cancelled() => return,
                r = tokio::time::timeout(CYCLE_WATCHDOG, this.cycle(&cancel)) => r,
            };
            if let Ok(()) = result {
            } else {
                warn!("watchtower: cycle exceeded the 15-minute watchdog; restarting");
            }
            let delay = CYCLE_INTERVAL;
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// v1 `RunCycleAsync` (one pass): sources, expanders, resolve, keep-fresh.
    async fn cycle(self: &Arc<Self>, ct: &CancellationToken) {
        if let Err(e) = self.sync_due_sources(ct).await {
            debug!(error = %e, "watchtower: source sync failed");
        }
        if let Err(e) = self.expand_due_expanders(ct).await {
            debug!(error = %e, "watchtower: expand failed");
        }
        if let Err(e) = self.resolve_due_items(ct).await {
            debug!(error = %e, "watchtower: resolve failed");
        }
        if let Err(e) = self.keep_fresh_due_items(ct).await {
            debug!(error = %e, "watchtower: keep-fresh failed");
        }
    }

    /// v1 `SyncDueSourcesAsync` + `ReconcileSourceAsync`.
    async fn sync_due_sources(
        self: &Arc<Self>,
        ct: &CancellationToken,
    ) -> Result<(), infinidysk_db::Error> {
        let s = self.settings().await;
        let wt = &s.watchtower;
        let now = now();
        let interval = i64::from(wt.sync_interval_seconds());
        let sources = self.db.enabled_remote_sources().await?;
        for mut source in sources {
            if ct.is_cancelled() {
                return Ok(());
            }
            if let Some(last) = source.last_synced_at
                && now - last < interval
            {
                continue;
            }
            let mut sync_error: Option<String> = None;
            match self.enumerator.enumerate(&source, ct).await {
                Ok(refs) => {
                    if let Err(e) = self.reconcile_source(&mut source, &refs, now, ct).await {
                        debug!(error = %e, key = %source.id, "watchtower: reconcile failed");
                    }
                }
                Err(msg) => {
                    sync_error = Some(msg.clone());
                    warn!(source = %source.name, error = %msg, "watchtower: sync failed for source");
                }
            }
            let _ = self
                .db
                .update_source_sync(source.id, now, sync_error.as_deref())
                .await;
        }
        Ok(())
    }

    /// Resolves a display title for an imdb-shaped content id via the shared
    /// `ImdbTitleResolver` (TVmaze/Wikidata, cached), composing SxxEyy suffixes.
    async fn resolve_content_title(
        self: &Arc<Self>,
        r#type: &str,
        content_id: &str,
        ct: &CancellationToken,
    ) -> Option<String> {
        if !is_imdb_id(content_id) {
            return None;
        }
        let imdb = canonical_imdb(content_id);
        let base_type = if r#type == "movie" { "movie" } else { "series" };
        let base = self
            .search
            .service
            .resolvers()
            .title
            .get_title(base_type, Some(&imdb[2..]), None, ct)
            .await?;
        if base.trim().is_empty() {
            return None;
        }
        Some(compose_imdb_title(&base, content_id))
    }

    /// Replaces a raw-id title on `item` with the resolved display title, if any.
    async fn backfill_title(self: &Arc<Self>, item: &mut WantedItemRow, ct: &CancellationToken) {
        if !is_unresolved_title(&item.title, &item.content_id) {
            return;
        }
        if let Some(resolved) = self
            .resolve_content_title(&item.r#type, &item.content_id, ct)
            .await
        {
            item.title = resolved;
        }
    }

    async fn reconcile_source(
        self: &Arc<Self>,
        source: &mut ListSourceRow,
        refs: &[WtContentRef],
        now: i64,
        ct: &CancellationToken,
    ) -> Result<(), infinidysk_db::Error> {
        let src_id = source.id.as_simple().to_string();
        let mut yielded: HashMap<String, WtContentRef> = HashMap::new();
        for r in refs
            .iter()
            .filter(|r| !r.r#type.trim().is_empty() && !r.content_id.trim().is_empty())
        {
            yielded.insert(format!("{}:{}", r.r#type, r.content_id), r.clone());
        }
        let keys: Vec<String> = yielded.keys().cloned().collect();
        let existing = self.db.wanted_by_keys(&keys).await?;
        let existing_by_key: HashMap<String, WantedItemRow> =
            existing.into_iter().map(|w| (w.key.clone(), w)).collect();
        let _permit = self.db.write_permit().await;

        for (key, r) in &yielded {
            match existing_by_key.get(key) {
                None => {
                    let row = WantedItemRow {
                        id: uuid::Uuid::new_v4(),
                        key: key.clone(),
                        r#type: r.r#type.clone(),
                        content_id: r.content_id.clone(),
                        title: match r.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                            Some(t) if !is_unresolved_title(t, &r.content_id) => t.to_owned(),
                            _ => self
                                .resolve_content_title(&r.r#type, &r.content_id, ct)
                                .await
                                .unwrap_or_else(|| r.content_id.clone()),
                        },
                        state: if watchtower::is_bare_series(&r.r#type, &r.content_id) {
                            STATE_EXPANDER.to_owned()
                        } else {
                            STATE_SCOUTING.to_owned()
                        },
                        provenance: write_strings(std::slice::from_ref(&src_id)),
                        shortlist: "[]".to_owned(),
                        winner_nzb: None,
                        responder_host: None,
                        fail_reason: None,
                        created_at: now,
                        updated_at: now,
                        last_resolved_at: None,
                        last_verified_at: None,
                        next_check_at: Some(now),
                    };
                    let _ = self.db.insert_wanted(&row).await;
                }
                Some(row) => {
                    let mut updated = row.clone();
                    let mut changed = false;
                    let mut prov = read_strings(&row.provenance);
                    if !prov.contains(&src_id) {
                        prov.push(src_id.clone());
                        updated.provenance = write_strings(&prov);
                        changed = true;
                    }
                    if let Some(t) = r.title.as_deref().map(str::trim).filter(|t| !t.is_empty())
                        && is_unresolved_title(&row.title, &row.content_id)
                    {
                        updated.title = t.to_owned();
                        changed = true;
                    } else if is_unresolved_title(&row.title, &row.content_id)
                        && let Some(resolved) = self
                            .resolve_content_title(&row.r#type, &row.content_id, ct)
                            .await
                    {
                        updated.title = resolved;
                        changed = true;
                    }

                    if row.state != STATE_EXPANDER
                        && watchtower::is_bare_series(&row.r#type, &row.content_id)
                    {
                        STATE_EXPANDER.clone_into(&mut updated.state);
                        "[]".clone_into(&mut updated.shortlist);
                        updated.winner_nzb = None;
                        updated.fail_reason = None;
                        updated.next_check_at = Some(now);
                        changed = true;
                    }
                    if changed {
                        updated.updated_at = now;
                        let _ = self.db.update_wanted(&updated).await;
                    }
                }
            }
        }

        // Drop provenance claims from this source for keys no longer yielded.
        let claimed = self.db.wanted_claimed_by(&src_id).await?;
        let mut cascaded = false;
        let mut stale_ids = Vec::new();
        for c in &claimed {
            if yielded.contains_key(&c.key) || c.state != STATE_EXPANDER {
                continue;
            }
            let mut prov = read_strings(&c.provenance);
            prov.retain(|p| p != &src_id);
            if prov.is_empty() {
                if let Err(e) = self.remove_with_children(c, now, ct).await {
                    debug!(error = %e, "watchtower: cascade remove failed");
                }
                cascaded = true;
            }
        }
        let claimed = if cascaded {
            self.db.wanted_claimed_by(&src_id).await?
        } else {
            claimed
        };
        for c in claimed.iter().filter(|c| !yielded.contains_key(&c.key)) {
            let mut prov = read_strings(&c.provenance);
            prov.retain(|p| p != &src_id);
            if prov.is_empty() {
                if c.state == STATE_EXPANDER {
                    let _ = self.remove_with_children(c, now, ct).await;
                } else {
                    stale_ids.push(c.id);
                }
            } else {
                let mut updated = c.clone();
                updated.provenance = write_strings(&prov);
                updated.updated_at = now;
                let _ = self.db.update_wanted(&updated).await;
            }
        }
        let _ = self.db.delete_wanted_by_ids(&stale_ids).await;
        Ok(())
    }

    /// v1 `WtReconcile.RemoveWithChildrenByRefAsync`.
    async fn remove_with_children(
        self: &Arc<Self>,
        item: &WantedItemRow,
        now: i64,
        ct: &CancellationToken,
    ) -> Result<(), infinidysk_db::Error> {
        let _ = ct;
        if item.state == STATE_EXPANDER {
            let tag = expander_tag(&item.key);
            let children = self.db.wanted_claimed_by(&tag).await?;
            let mut orphan_ids = Vec::new();
            for child in &children {
                let mut prov = read_strings(&child.provenance);
                if !prov.contains(&tag) {
                    continue;
                }
                prov.retain(|p| p != &tag);
                if prov.is_empty() {
                    orphan_ids.push(child.id);
                } else {
                    let mut updated = child.clone();
                    updated.provenance = write_strings(&prov);
                    updated.updated_at = now;
                    let _ = self.db.update_wanted(&updated).await;
                }
            }
            let _ = self.db.delete_wanted_by_ids(&orphan_ids).await;
        }
        let _ = self.db.delete_wanted_by_ids(&[item.id]).await;
        Ok(())
    }

    /// v1 `ExpandDueExpandersAsync` + `ExpandOneAsync`.
    async fn expand_due_expanders(
        self: &Arc<Self>,
        ct: &CancellationToken,
    ) -> Result<(), infinidysk_db::Error> {
        let s = self.settings().await;
        let global_scope = s.watchtower.series_scope().to_owned();
        let interval = i64::from(s.watchtower.sync_interval_seconds());
        let now = now();
        let due = self.db.wanted_due_expanders(now, EXPANDS_PER_TICK).await?;
        if due.is_empty() {
            return Ok(());
        }
        let sources = self.db.list_sources_all().await?;
        let scope_by_source: HashMap<String, String> = sources
            .iter()
            .map(|src| {
                (
                    src.id.as_simple().to_string(),
                    src.series_scope
                        .as_deref()
                        .and_then(normalize_series_scope)
                        .unwrap_or_else(|| global_scope.clone()),
                )
            })
            .collect();
        for mut expander in due {
            if ct.is_cancelled() {
                break;
            }
            let scopes = effective_scopes(&expander, &scope_by_source, &global_scope);
            self.backfill_title(&mut expander, ct).await;
            if !scopes.is_empty()
                && let Err(e) = self.expand_one(&expander, &scopes, now, ct).await
            {
                debug!(error = %e, key = %expander.key, "watchtower: expand failed");
            }
            expander.next_check_at = Some(now + interval);
            expander.updated_at = now;
            let _permit = self.db.write_permit().await;
            let _ = self.db.update_wanted(&expander).await;
        }
        Ok(())
    }

    async fn expand_one(
        self: &Arc<Self>,
        expander: &WantedItemRow,
        scopes: &[String],
        now: i64,
        ct: &CancellationToken,
    ) -> Result<(), String> {
        let tag = expander_tag(&expander.key);
        let Some(desired) = self.build_desired(expander, scopes, now, ct).await? else {
            return Ok(());
        };
        let keys: Vec<String> = desired.keys().cloned().collect();
        let existing = self
            .db
            .wanted_by_keys(&keys)
            .await
            .map_err(|e| e.to_string())?;
        let existing_by_key: HashMap<String, WantedItemRow> =
            existing.into_iter().map(|w| (w.key.clone(), w)).collect();
        let _permit = self.db.write_permit().await;
        for (child_key, row) in &desired {
            if let Some(child) = existing_by_key.get(child_key) {
                let mut prov = read_strings(&child.provenance);
                let mut updated = child.clone();
                let mut changed = false;
                if !prov.contains(&tag) {
                    prov.push(tag.clone());
                    updated.provenance = write_strings(&prov);
                    changed = true;
                }
                if is_unresolved_title(&child.title, &child.content_id)
                    && !is_unresolved_title(&row.2, &row.1)
                {
                    updated.title = row.2.clone();
                    changed = true;
                }
                if changed {
                    updated.updated_at = now;
                    let _ = self.db.update_wanted(&updated).await;
                }
            } else {
                let row_new = WantedItemRow {
                    id: uuid::Uuid::new_v4(),
                    key: child_key.clone(),
                    r#type: row.0.clone(),
                    content_id: row.1.clone(),
                    title: row.2.clone(),
                    state: STATE_SCOUTING.to_owned(),
                    provenance: write_strings(std::slice::from_ref(&tag)),
                    shortlist: "[]".to_owned(),
                    winner_nzb: None,
                    responder_host: None,
                    fail_reason: None,
                    created_at: now,
                    updated_at: now,
                    last_resolved_at: None,
                    last_verified_at: None,
                    next_check_at: Some(now),
                };
                let _ = self.db.insert_wanted(&row_new).await;
            }
        }
        // Drop the expander tag from children no longer desired.
        let tagged = self
            .db
            .wanted_claimed_by(&tag)
            .await
            .map_err(|e| e.to_string())?;
        let mut orphan_ids = Vec::new();
        for child in &tagged {
            let mut prov = read_strings(&child.provenance);
            if !prov.contains(&tag) || desired.contains_key(&child.key) {
                continue;
            }
            prov.retain(|p| p != &tag);
            if prov.is_empty() {
                orphan_ids.push(child.id);
            } else {
                let mut updated = child.clone();
                updated.provenance = write_strings(&prov);
                updated.updated_at = now;
                let _ = self.db.update_wanted(&updated).await;
            }
        }
        let _ = self.db.delete_wanted_by_ids(&orphan_ids).await;
        info!(key = %expander.key, count = desired.len(), scope = %scopes.join("+"), "watchtower: expanded");
        Ok(())
    }

    /// v1 `BuildDesiredAsync` → map key → (type, `content_id`, title).
    async fn build_desired(
        self: &Arc<Self>,
        expander: &WantedItemRow,
        scopes: &[String],
        now: i64,
        ct: &CancellationToken,
    ) -> Result<Option<HashMap<String, (String, String, String)>>, String> {
        let s = self.settings().await;
        let cfg = &s.watchtower;
        if is_imdb_id(&expander.content_id) {
            let episodes = self.episodes.enumerate_imdb(&expander.content_id, ct).await;
            if episodes.is_empty() {
                return Ok(None);
            }
            let imdb = canonical_imdb(&expander.content_id);
            let parked = if cfg.season_bundle_fallback {
                self.parked_fallback_seasons(&imdb).await
            } else {
                HashSet::new()
            };
            let mut desired = HashMap::new();
            for scope in scopes {
                for (k, v) in
                    build_desired_rows(cfg, &episodes, &expander.title, &imdb, scope, now, &parked)
                {
                    desired.insert(k, v);
                }
            }
            return Ok(Some(clamp_series(cfg, desired)));
        }
        if let Some(kitsu_id) = parse_kitsu_id(&expander.content_id) {
            let episodes = self.episodes.enumerate_kitsu(&kitsu_id, ct).await;
            if episodes.is_empty() {
                return Ok(None);
            }
            let mut desired = HashMap::new();
            for scope in scopes {
                for (k, v) in
                    build_anime_desired_rows(cfg, &episodes, &expander.title, &kitsu_id, scope, now)
                {
                    desired.insert(k, v);
                }
            }
            return Ok(Some(clamp_series(cfg, desired)));
        }
        Ok(None)
    }

    async fn parked_fallback_seasons(&self, imdb: &str) -> HashSet<i32> {
        let ids = self
            .db
            .season_ids_with_prefix(&format!("{imdb}:"), Some(STATE_PARKED))
            .await
            .unwrap_or_default();
        let mut seasons = HashSet::new();
        for id in ids {
            if let Some(last) = id.split(':').next_back()
                && let Ok(s) = last.parse::<i32>()
            {
                seasons.insert(s);
            }
        }
        seasons
    }
}

fn normalize_series_scope(value: &str) -> Option<String> {
    match value {
        "latest-season" | "first-season" | "all-aired" | "recent" | "off" => Some(value.to_owned()),
        _ => None,
    }
}

fn effective_scopes(
    expander: &WantedItemRow,
    scope_by_source: &HashMap<String, String>,
    global_scope: &str,
) -> Vec<String> {
    let mut scopes = Vec::new();
    for src_id in read_strings(&expander.provenance) {
        let scope = scope_by_source
            .get(&src_id)
            .cloned()
            .unwrap_or_else(|| global_scope.to_owned());
        if scope == "off" || scopes.contains(&scope) {
            continue;
        }
        scopes.push(scope);
    }
    scopes
}

fn parse_kitsu_id(content_id: &str) -> Option<String> {
    let parts: Vec<&str> = content_id.split(':').collect();
    if parts.len() == 2 && parts[0].eq_ignore_ascii_case("kitsu") {
        let id = parts[1];
        if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
            return Some(id.to_owned());
        }
    }
    None
}

fn anime_title(series_title: Option<&str>, number: i32) -> String {
    let code = format!("E{number:02}");
    match series_title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(base) => format!("{base} {code}"),
        None => code,
    }
}

fn child_title(series_title: Option<&str>, ep: &Episode) -> String {
    let code = format!("S{:02}E{:02}", ep.season, ep.number);
    match series_title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(base) => format!("{base} {code}"),
        None => code,
    }
}

fn season_title(series_title: Option<&str>, season: i32) -> String {
    let code = format!("S{season:02} (season bundle)");
    match series_title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(base) => format!("{base} {code}"),
        None => code,
    }
}

/// v1 `BuildAnimeDesiredRows`.
fn build_anime_desired_rows(
    wt: &crate::config::Watchtower,
    episodes: &[Episode],
    series_title: &str,
    kitsu_id: &str,
    scope: &str,
    now: i64,
) -> Vec<(String, (String, String, String))> {
    let aired: Vec<&Episode> = episodes
        .iter()
        .filter(|e| e.air_date_unix.is_none_or(|a| a <= now))
        .collect();
    if aired.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&Episode> = aired.clone();
    sorted.sort_by_key(|e| e.number);
    let count = if scope == "recent" {
        wt.series_recent_count() as usize
    } else {
        wt.series_max_episodes() as usize
    };
    let selected: Vec<&Episode> = if count == 0 {
        sorted.clone()
    } else if scope == "first-season" {
        sorted.iter().take(count).copied().collect()
    } else {
        sorted
            .iter()
            .skip(sorted.len().saturating_sub(count))
            .copied()
            .collect()
    };
    selected
        .into_iter()
        .map(|ep| {
            let content_id = format!("kitsu:{kitsu_id}:{}", ep.number);
            (
                format!("series:{content_id}"),
                (
                    "series".to_owned(),
                    content_id,
                    anime_title(Some(series_title), ep.number),
                ),
            )
        })
        .collect()
}

/// v1 `BuildDesiredRows` (scope per call).
fn build_desired_rows(
    wt: &crate::config::Watchtower,
    episodes: &[Episode],
    series_title: &str,
    imdb: &str,
    scope: &str,
    now: i64,
    parked_fallback_seasons: &HashSet<i32>,
) -> Vec<(String, (String, String, String))> {
    let mut desired: Vec<(String, (String, String, String))> = Vec::new();
    let aired: Vec<&Episode> = episodes
        .iter()
        .filter(|e| e.air_date_unix.is_none_or(|a| a <= now))
        .collect();
    if aired.is_empty() {
        return desired;
    }
    let w = wt;
    let recent_count = w.series_recent_count() as usize;
    if scope == "recent" {
        for ep in aired.iter().skip(aired.len().saturating_sub(recent_count)) {
            add_episode_row(&mut desired, imdb, series_title, ep);
        }
        return desired;
    }
    let bundles_enabled = w.season_bundles;
    let fallback_cap = w.season_bundle_fallback_max_episodes() as usize;
    let seasons: Vec<i32> = match scope {
        "all-aired" => {
            let mut v: Vec<i32> = aired.iter().map(|e| e.season).collect();
            v.sort_unstable();
            v.dedup();
            v.reverse();
            v
        }
        "first-season" => vec![aired.iter().map(|e| e.season).min().unwrap_or(1)],
        _ => vec![aired.iter().map(|e| e.season).max().unwrap_or(1)],
    };
    for season in seasons {
        if bundles_enabled && season_complete(episodes, season, now) {
            add_season_row(&mut desired, imdb, series_title, season);
            if parked_fallback_seasons.contains(&season) {
                let mut eps: Vec<&Episode> = aired
                    .iter()
                    .copied()
                    .filter(|e| e.season == season)
                    .collect();
                eps.sort_by_key(|e| e.number);
                for ep in eps.into_iter().take(fallback_cap) {
                    add_episode_row(&mut desired, imdb, series_title, ep);
                }
            }
            continue;
        }
        let mut eps: Vec<&Episode> = aired
            .iter()
            .copied()
            .filter(|e| e.season == season)
            .collect();
        eps.sort_by_key(|e| e.number);
        for ep in eps {
            add_episode_row(&mut desired, imdb, series_title, ep);
        }
    }
    desired
}

fn add_episode_row(
    desired: &mut Vec<(String, (String, String, String))>,
    imdb: &str,
    series_title: &str,
    ep: &Episode,
) {
    let content_id = format!("{imdb}:{}:{}", ep.season, ep.number);
    desired.push((
        format!("series:{content_id}"),
        (
            "series".to_owned(),
            content_id,
            child_title(Some(series_title), ep),
        ),
    ));
}

fn add_season_row(
    desired: &mut Vec<(String, (String, String, String))>,
    imdb: &str,
    series_title: &str,
    season: i32,
) {
    let content_id = format!("{imdb}:{season}");
    desired.push((
        format!("season:{content_id}"),
        (
            "season".to_owned(),
            content_id,
            season_title(Some(series_title), season),
        ),
    ));
}

fn season_complete(all: &[Episode], season: i32, now: i64) -> bool {
    let eps: Vec<&Episode> = all.iter().filter(|e| e.season == season).collect();
    if eps.is_empty() {
        return false;
    }
    if eps.iter().any(|e| e.air_date_unix.is_some_and(|a| a > now)) {
        return false;
    }
    let known: Vec<i64> = eps.iter().filter_map(|e| e.air_date_unix).collect();
    if known.is_empty() {
        return true;
    }
    now - known.iter().max().copied().unwrap_or(now) >= SEASON_BUNDLE_GRACE_SECS
}

/// v1 `ClampSeries` + `RowOrder`.
fn clamp_series(
    wt: &crate::config::Watchtower,
    desired: HashMap<String, (String, String, String)>,
) -> HashMap<String, (String, String, String)> {
    let cap = wt.series_max_episodes() as usize;
    if cap == 0 {
        return desired;
    }
    let episodes: Vec<(String, (String, String, String))> = desired
        .iter()
        .filter(|(_, v)| v.0 != "season")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if episodes.len() <= cap {
        return desired;
    }
    let keep_oldest = wt.series_cap_keep() == "oldest";
    let mut sorted = episodes;
    sorted.sort_by_key(|(_, v)| row_order(v));
    if !keep_oldest {
        sorted.reverse();
    }
    let kept: Vec<String> = sorted.into_iter().take(cap).map(|(k, _)| k).collect();
    desired
        .into_iter()
        .filter(|(k, v)| v.0 == "season" || kept.contains(k))
        .collect()
}

fn row_order(row: &(String, String, String)) -> (i32, i32) {
    let parts: Vec<&str> = row.1.split(':').collect();
    let last = |i: usize| {
        parts
            .get(i)
            .and_then(|p| p.parse::<i32>().ok())
            .unwrap_or(0)
    };
    if row.0 == "season" {
        return (last(parts.len().wrapping_sub(1)), i32::MAX);
    }
    if parts
        .first()
        .is_some_and(|p| p.eq_ignore_ascii_case("kitsu"))
    {
        return (1, last(parts.len().wrapping_sub(1)));
    }
    if parts.len() >= 3 {
        (
            parts[parts.len() - 2].parse::<i32>().unwrap_or(0),
            last(parts.len() - 1),
        )
    } else {
        (0, 0)
    }
}

impl Watchtower {
    async fn resolve_profile_token(&self) -> Option<String> {
        let s = self.settings().await;
        let configured = s.watchtower.profile_token.trim();
        if !configured.is_empty() && s.profiles.profiles.iter().any(|p| p.token == configured) {
            return Some(configured.to_owned());
        }
        s.profiles.profiles.first().map(|p| p.token.clone())
    }

    fn roll_daily_budget(&self) {
        let day_key = now() / 86_400;
        let mut budget = self.budget.lock().expect("lock");
        if budget.0 != day_key {
            *budget = (day_key, 0);
        }
    }

    /// v1 `ResolveDueItemsAsync`.
    async fn resolve_due_items(
        self: &Arc<Self>,
        ct: &CancellationToken,
    ) -> Result<(), infinidysk_db::Error> {
        let s = self.settings().await;
        let wt = &s.watchtower;
        let auto = wt.auto_throughput;
        let daily_budget = wt.daily_resolve_budget;
        self.roll_daily_budget();
        let Some(profile_token) = self.resolve_profile_token().await else {
            debug!("watchtower: no search profile configured; skipping resolve");
            return Ok(());
        };
        let concurrency = wt.resolve_concurrency() as usize;
        let stage_start = std::time::Instant::now();
        loop {
            if ct.is_cancelled() {
                return Ok(());
            }
            let budget_room = if auto || daily_budget == 0 {
                i64::MAX
            } else {
                let today = self.budget.lock().expect("lock").1;
                i64::from(daily_budget).saturating_sub(i64::from(today))
            };
            if budget_room <= 0 {
                return Ok(());
            }
            let now = now();
            let cap = i64::from(wt.active_set_cap());
            let counts = self.db.wanted_counts_by_state().await?;
            let active_ready = counts
                .iter()
                .find(|(st, _)| st == STATE_READY)
                .map_or(0, |(_, c)| *c);
            let cap_room = cap - active_ready;
            if cap_room <= 0 {
                return Ok(());
            }
            let batch = if auto {
                AUTO_RESOLVE_BATCH
            } else {
                RESOLVES_PER_TICK
            };
            let take = batch.min(cap_room).min(budget_room);
            if take <= 0 {
                return Ok(());
            }
            let due = self.db.wanted_due_resolve(now, take).await?;
            if due.is_empty() {
                return Ok(());
            }
            if auto
                && !self
                    .search
                    .service
                    .has_resolve_headroom(&profile_token, ct)
                    .await
            {
                debug!("watchtower: all indexers out of headroom; pausing resolve");
                return Ok(());
            }
            let mut resolved_today = 0u32;
            // Bounded concurrency via a semaphore.
            let gate = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
            let mut handles = Vec::new();
            for item in due {
                if auto
                    && !self
                        .search
                        .service
                        .has_resolve_headroom(&profile_token, ct)
                        .await
                {
                    break;
                }
                let this = Arc::clone(self);
                let profile_token = profile_token.clone();
                let gate = Arc::clone(&gate);
                let ct = ct.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = gate.acquire().await.expect("semaphore");
                    let mut resolved = false;
                    if let Err(e) = this.resolve_one(&profile_token, &item, &ct).await {
                        debug!(error = %e, key = %item.key, "watchtower: resolve failed");
                    } else {
                        resolved = true;
                    }
                    resolved
                }));
            }
            for h in handles {
                if h.await.unwrap_or(false) {
                    resolved_today += 1;
                }
            }
            if resolved_today > 0 {
                let mut budget = self.budget.lock().expect("lock");
                budget.1 += resolved_today;
            }
            if !auto || stage_start.elapsed() >= AUTO_STAGE_BUDGET {
                return Ok(());
            }
        }
    }

    /// v1 `ResolveOneAsync`.
    async fn resolve_one(
        self: &Arc<Self>,
        profile_token: &str,
        item: &WantedItemRow,
        ct: &CancellationToken,
    ) -> Result<(), String> {
        let s = self.settings().await;
        let wt = &s.watchtower;
        let now = now();
        let mut item = item.clone();
        if watchtower::is_bare_series(&item.r#type, &item.content_id) {
            STATE_EXPANDER.clone_into(&mut item.state);
            "[]".clone_into(&mut item.shortlist);
            item.winner_nzb = None;
            item.fail_reason = None;
            item.next_check_at = Some(now);
            item.updated_at = now;
            let _permit = self.db.write_permit().await;
            return self
                .db
                .update_wanted(&item)
                .await
                .map_err(|e| e.to_string());
        }
        self.backfill_title(&mut item, ct).await;
        // v1 calls SearchByImdbAsync(profileToken, type, contentId, ...) and verifies the
        // identity itself; preflight is started by this resolver, not by the search.
        let search = self
            .search
            .service
            .search_by_imdb(
                profile_token,
                &item.r#type,
                &item.content_id,
                ct,
                None,
                true,
                false,
            )
            .await;
        let Some(search) = search else {
            let _permit = self.db.write_permit().await;
            STATE_UNAVAILABLE.clone_into(&mut item.state);
            item.fail_reason = Some("No releases found".to_owned());
            item.next_check_at = Some(now + i64::from(wt.unavailable_retry_seconds()));
            item.updated_at = now;
            return self
                .db
                .update_wanted(&item)
                .await
                .map_err(|e| e.to_string());
        };
        let candidates = search.candidates;
        let floor = wt.size_floor_bytes;
        let ceiling = wt.size_ceiling_bytes;
        let min_grabs = wt.min_grabs;
        let mut filtered: Vec<Candidate> = candidates
            .into_iter()
            .filter(|c| c.password.unwrap_or(0) == 0)
            .filter(|c| floor <= 0 || c.size <= 0 || c.size >= floor)
            .filter(|c| ceiling <= 0 || c.size <= 0 || c.size <= ceiling)
            .filter(|c| min_grabs == 0 || c.grabs.unwrap_or(0) >= min_grabs.cast_signed())
            .collect();
        let preferred_order =
            self.search
                .preferred_order
                .get_order(profile_token, &item.r#type, &item.content_id);
        if let Some(order) = preferred_order.as_deref().filter(|o| !o.is_empty()) {
            filtered = PreferredOrderStore::apply_order(
                &filtered,
                |c| {
                    preferred::release_identity_key(
                        c.size,
                        c.poster.as_deref(),
                        c.usenet_date,
                        Some(&c.nzb_url),
                    )
                },
                Some(order),
            );
        } else if wt.ranking() == "largest" {
            filtered.sort_by_key(|c| std::cmp::Reverse(c.size));
        }
        let depth = wt.shortlist_depth() as usize;
        let grab_cap = wt.grab_cap_per_resolve() as usize;
        let sample = wt.verify_sample_count();
        let verify_timeout = Duration::from_secs(u64::from(wt.verify_timeout_seconds()));

        let mut shortlist: Vec<WtPointer> = Vec::new();
        let mut winner_bytes: Option<Vec<u8>> = None;
        let mut responder_host: Option<String> = None;
        let mut grabs = 0usize;
        let mut known_dead = 0usize;
        let mut verified_dead = 0usize;
        let mut timed_out = 0usize;

        for c in &filtered {
            if shortlist.len() >= depth || grabs >= grab_cap || ct.is_cancelled() {
                break;
            }
            let fp = preferred::warden_fingerprint(c.size, c.poster.as_deref(), c.usenet_date);
            if self.preflight.negative.is_failed(
                &c.nzb_url,
                Duration::from_secs(u64::from(s.play.candidate_negative_cache_ttl_minutes()) * 60),
            ) || self.warden.is_dead_anywhere(fp.as_deref()).await
            {
                known_dead += 1;
                continue;
            }
            let Some(bytes) = self.fetch_nzb(&s, c, ct).await else {
                grabs += 1;
                continue;
            };
            grabs += 1;
            let outcome = self
                .preflight
                .verifier
                .verify(&bytes, "stat", sample, verify_timeout, ct)
                .await;
            match outcome.verdict {
                Verdict::Dead => {
                    verified_dead += 1;
                    self.preflight.negative.mark_failed(&c.nzb_url);
                    self.warden.mark_dead(fp.as_deref()).await;
                    continue;
                }
                Verdict::Timeout => {
                    timed_out += 1;
                    continue;
                }
                Verdict::Available => {}
            }
            let ptr = WtPointer {
                nzb_url: c.nzb_url.clone(),
                indexer_name: c.indexer_name.clone(),
                indexer_user_agent: c.indexer_user_agent.clone(),
                proxy_url: c.proxy_url.clone(),
                title: c.title.clone(),
                size: c.size,
                grabs: c.grabs,
                poster: c.poster.clone(),
                usenet_date: c.usenet_date,
                verdict: "available".to_owned(),
                last_verified_at_unix: Some(now),
            };
            if shortlist.is_empty() {
                winner_bytes = Some(bytes.as_ref().clone());
                responder_host = outcome.responder_host.clone();
            }
            shortlist.push(ptr);
        }

        let _permit = self.db.write_permit().await;
        if shortlist.is_empty() {
            if self
                .try_park_for_episode_fallback(&mut item, filtered.len(), now)
                .await
            {
                return self
                    .db
                    .update_wanted(&item)
                    .await
                    .map_err(|e| e.to_string());
            }
            STATE_UNAVAILABLE.clone_into(&mut item.state);
            "[]".clone_into(&mut item.shortlist);
            item.winner_nzb = None;
            item.fail_reason = Some(if filtered.is_empty() {
                "No releases found".to_owned()
            } else {
                "No healthy release found".to_owned()
            });
            item.next_check_at = Some(now + i64::from(wt.unavailable_retry_seconds()));
            item.updated_at = now;
            debug!(key = %item.key, candidates = filtered.len(), known_dead, verified_dead, timed_out, "watchtower: unavailable");
            self.db
                .update_wanted(&item)
                .await
                .map_err(|e| e.to_string())?;
        } else {
            STATE_READY.clone_into(&mut item.state);
            item.shortlist = write_pointers(&shortlist);
            item.winner_nzb = winner_bytes.clone();
            item.responder_host = responder_host.clone();
            item.last_resolved_at = Some(now);
            item.last_verified_at = Some(now);
            item.next_check_at = Some(now + i64::from(wt.keepfresh_base_seconds.clamp(60, 86_400)));
            item.fail_reason = None;
            item.updated_at = now;
            self.db
                .update_wanted(&item)
                .await
                .map_err(|e| e.to_string())?;
            self.preflight.cache.set_verified(
                &shortlist[0].nzb_url,
                winner_bytes,
                Verdict::Available,
                responder_host,
                Duration::from_secs(u64::from(s.preflight.ttl_seconds())),
            );
            info!(key = %item.key, title = %shortlist[0].title, size = shortlist[0].size, count = shortlist.len(), "watchtower: ready");
        }
        Ok(())
    }

    /// v1 `TryParkForEpisodeFallbackAsync` + `SeasonEligibleForFallbackAsync` + `NudgeParentExpanderAsync`.
    async fn try_park_for_episode_fallback(
        self: &Arc<Self>,
        item: &mut WantedItemRow,
        candidate_count: usize,
        now: i64,
    ) -> bool {
        let s = self.settings().await;
        let wt = &s.watchtower;
        if item.r#type != "season" || !wt.season_bundle_fallback {
            return false;
        }
        let parts: Vec<&str> = item.content_id.split(':').collect();
        if parts.len() < 2 {
            return false;
        }
        let imdb = parts[0];
        let Ok(season) = parts[parts.len() - 1].parse::<i32>() else {
            return false;
        };
        let scope = wt.season_bundle_fallback_scope();
        if scope != "all" {
            let ids = self
                .db
                .season_ids_with_prefix(&format!("{imdb}:"), None)
                .await
                .unwrap_or_default();
            let mut max_season = season;
            for id in ids {
                if let Some(last) = id.split(':').next_back()
                    && let Ok(parsed) = last.parse::<i32>()
                    && parsed > max_season
                {
                    max_season = parsed;
                }
            }
            let eligible = if scope == "recent" {
                season > max_season - wt.season_bundle_fallback_recent_count().cast_signed()
            } else {
                season == max_season
            };
            if !eligible {
                return false;
            }
        }
        STATE_PARKED.clone_into(&mut item.state);
        "[]".clone_into(&mut item.shortlist);
        item.winner_nzb = None;
        item.fail_reason = Some(if candidate_count == 0 {
            "No season bundle found, using episodes".to_owned()
        } else {
            "No healthy season bundle, using episodes".to_owned()
        });
        item.next_check_at = None;
        item.updated_at = now;
        // Nudge the parent expander to re-expand now.
        if let Some(parent_key) = read_strings(&item.provenance)
            .into_iter()
            .find(|p| p.starts_with("exp:"))
            .map(|p| p[4..].to_owned())
            && let Some(mut parent) = self.db.wanted_by_key(&parent_key).await.ok().flatten()
        {
            parent.next_check_at = Some(now);
            parent.updated_at = now;
            let _permit = self.db.write_permit().await;
            let _ = self.db.update_wanted(&parent).await;
        }
        info!(key = %item.key, "watchtower: parked season bundle; falling back to episodes");
        true
    }

    /// v1 `KeepFreshDueItemsAsync` + `KeepFreshOneAsync` + `PromoteBackupAsync`.
    async fn keep_fresh_due_items(
        self: &Arc<Self>,
        ct: &CancellationToken,
    ) -> Result<(), infinidysk_db::Error> {
        let now = now();
        let due = self
            .db
            .wanted_due_keepfresh(now, KEEP_FRESH_PER_TICK)
            .await?;
        for mut item in due {
            if ct.is_cancelled() {
                break;
            }
            if let Err(e) = self.keep_fresh_one(&mut item, ct).await {
                debug!(error = %e, key = %item.key, "watchtower: keep-fresh failed");
            }
            let _permit = self.db.write_permit().await;
            let _ = self.db.update_wanted(&item).await;
        }
        Ok(())
    }

    async fn keep_fresh_one(
        self: &Arc<Self>,
        item: &mut WantedItemRow,
        ct: &CancellationToken,
    ) -> Result<(), String> {
        let s = self.settings().await;
        let wt = &s.watchtower;
        let now = now();
        self.backfill_title(item, ct).await;
        let mut shortlist = read_pointers(&item.shortlist);
        let Some(winner_nzb) = item.winner_nzb.clone() else {
            STATE_SCOUTING.clone_into(&mut item.state);
            item.next_check_at = Some(now);
            item.updated_at = now;
            return Ok(());
        };
        if shortlist.is_empty() {
            STATE_SCOUTING.clone_into(&mut item.state);
            item.next_check_at = Some(now);
            item.updated_at = now;
            return Ok(());
        }
        let sample = wt.verify_sample_count();
        let verify_timeout = Duration::from_secs(u64::from(wt.verify_timeout_seconds()));
        let outcome = self
            .preflight
            .verifier
            .verify(&winner_nzb, "stat", sample, verify_timeout, ct)
            .await;
        match outcome.verdict {
            Verdict::Available => {
                item.last_verified_at = Some(now);
                item.responder_host.clone_from(&outcome.responder_host);
                "available".clone_into(&mut shortlist[0].verdict);
                shortlist[0].last_verified_at_unix = Some(now);
                item.shortlist = write_pointers(&shortlist);
                item.next_check_at = Some(now + next_backoff(wt, item.last_resolved_at, now));
                item.updated_at = now;
                self.preflight.cache.set_verified(
                    &shortlist[0].nzb_url,
                    Some(winner_nzb),
                    Verdict::Available,
                    outcome.responder_host,
                    Duration::from_secs(u64::from(s.preflight.ttl_seconds())),
                );
            }
            Verdict::Timeout => {
                let base = i64::from(wt.keepfresh_base_seconds.clamp(60, 86_400));
                item.next_check_at = Some(now + base.min(1800));
                item.updated_at = now;
            }
            Verdict::Dead => {
                debug!(key = %item.key, title = %shortlist[0].title, "watchtower: re-check found winner dead; promoting backup");
                self.preflight.negative.mark_failed(&shortlist[0].nzb_url);
                let fp = preferred::warden_fingerprint(
                    shortlist[0].size,
                    shortlist[0].poster.as_deref(),
                    shortlist[0].usenet_date,
                );
                self.warden.mark_dead(fp.as_deref()).await;
                shortlist.remove(0);
                self.promote_backup(item, &mut shortlist, now, ct).await?;
            }
        }
        Ok(())
    }

    async fn promote_backup(
        self: &Arc<Self>,
        item: &mut WantedItemRow,
        shortlist: &mut Vec<WtPointer>,
        now: i64,
        ct: &CancellationToken,
    ) -> Result<(), String> {
        let s = self.settings().await;
        let wt = &s.watchtower;
        let sample = wt.verify_sample_count();
        let verify_timeout = Duration::from_secs(u64::from(wt.verify_timeout_seconds()));
        while !shortlist.is_empty() && !ct.is_cancelled() {
            let ptr = shortlist[0].clone();
            let c = make_candidate(&ptr);
            let Some(bytes) = self.fetch_nzb(&s, &c, ct).await else {
                shortlist.remove(0);
                continue;
            };
            let outcome = self
                .preflight
                .verifier
                .verify(&bytes, "stat", sample, verify_timeout, ct)
                .await;
            match outcome.verdict {
                Verdict::Available => {
                    "available".clone_into(&mut shortlist[0].verdict);
                    shortlist[0].last_verified_at_unix = Some(now);
                    item.winner_nzb = Some(bytes.as_ref().clone());
                    item.responder_host.clone_from(&outcome.responder_host);
                    item.shortlist = write_pointers(shortlist);
                    item.last_verified_at = Some(now);
                    item.next_check_at =
                        Some(now + i64::from(wt.keepfresh_base_seconds.clamp(60, 86_400)));
                    item.updated_at = now;
                    self.preflight.cache.set_verified(
                        &ptr.nzb_url,
                        Some(bytes.as_ref().clone()),
                        Verdict::Available,
                        outcome.responder_host,
                        Duration::from_secs(u64::from(s.preflight.ttl_seconds())),
                    );
                    info!(key = %item.key, title = %ptr.title, "watchtower: promoted backup");
                    return Ok(());
                }
                Verdict::Dead => {
                    self.preflight.negative.mark_failed(&ptr.nzb_url);
                    let fp = preferred::warden_fingerprint(
                        ptr.size,
                        ptr.poster.as_deref(),
                        ptr.usenet_date,
                    );
                    self.warden.mark_dead(fp.as_deref()).await;
                    shortlist.remove(0);
                }
                Verdict::Timeout => {
                    shortlist.remove(0);
                }
            }
        }
        STATE_SCOUTING.clone_into(&mut item.state);
        "[]".clone_into(&mut item.shortlist);
        item.winner_nzb = None;
        item.next_check_at = Some(now);
        item.updated_at = now;
        debug!(key = %item.key, "watchtower: shortlist exhausted; re-resolving");
        Ok(())
    }

    /// v1 `FetchNzbBytesAsync` (coalesced, hit-limited, 15 s-bounded).
    async fn fetch_nzb(
        self: &Arc<Self>,
        settings: &Settings,
        c: &Candidate,
        ct: &CancellationToken,
    ) -> Option<Arc<Vec<u8>>> {
        let skip_tls = settings
            .indexers
            .should_skip_tls_verification(&c.indexer_name);
        if let Some(indexer) = settings
            .indexers
            .indexers
            .iter()
            .find(|x| x.name == c.indexer_name)
        {
            let check = self
                .search
                .service
                .hit_tracker()
                .check(
                    &c.indexer_name,
                    HitType::Download,
                    indexer.download_limit.map(i64::from),
                    indexer
                        .hit_limit_reset_time
                        .and_then(|h| i32::try_from(h).ok()),
                    Timestamp::now(),
                )
                .await;
            if let Some(check) = &check
                && !check.allowed
            {
                info!(indexer = %c.indexer_name, "watchtower: NZB download skipped: hit limit");
                return None;
            }
            if self
                .search
                .service
                .rate_limiter()
                .wait(&c.indexer_name, indexer.max_requests_per_minute, ct)
                .await
                .is_err()
            {
                return None;
            }
        }
        let key = FetchKey {
            url: c.nzb_url.clone(),
            proxy_url: c.proxy_url.clone().unwrap_or_default(),
            skip_tls_verification: skip_tls,
        };
        let ua = c.indexer_user_agent.clone();
        let url = c.nzb_url.clone();
        let proxy = c.proxy_url.clone();
        let result = self
            .preflight
            .coalescer
            .get_or_fetch(key, ct, move |hard| {
                let url = url.clone();
                let ua = ua.clone();
                let proxy = proxy.clone();
                async move {
                    let fetch_ct = CancellationToken::new();
                    let link = hard.clone();
                    let watcher = fetch_ct.clone();
                    tokio::spawn(async move {
                        link.cancelled().await;
                        watcher.cancel();
                    });
                    let host = reqwest::Url::parse(&url)
                        .ok()
                        .and_then(|u| u.host_str().map(str::to_owned))
                        .unwrap_or_default();
                    let client = infinidysk_indexer::http::build_client_with_proxy(
                        skip_tls,
                        &host,
                        proxy.as_deref(),
                    )
                    .map_err(|_| FetchError::Failed)?;
                    let resp = infinidysk_indexer::http::with_timeout(
                        NZB_FETCH_TIMEOUT,
                        &fetch_ct,
                        infinidysk_indexer::http::get(&client, &url, &ua, &fetch_ct, 1),
                    )
                    .await
                    .map_err(|e| match e {
                        infinidysk_indexer::Error::Cancelled => FetchError::Cancelled,
                        _ => FetchError::Failed,
                    })?;
                    if !resp.status().is_success() {
                        return Ok(None);
                    }
                    match infinidysk_indexer::http::read_bounded(resp, MAX_RESPONSE_BYTES).await {
                        Ok(bytes) => Ok(Some(bytes)),
                        Err(infinidysk_indexer::Error::TooLarge { .. }) => Ok(None),
                        Err(infinidysk_indexer::Error::Cancelled) => Err(FetchError::Cancelled),
                        Err(_) => Err(FetchError::Failed),
                    }
                }
            })
            .await;
        match result {
            Ok(Some(bytes)) => {
                self.search
                    .service
                    .hit_tracker()
                    .record(&c.indexer_name, HitType::Download, Timestamp::now())
                    .await;
                Some(bytes)
            }
            _ => None,
        }
    }

    /// v1 `WatchtowerStore.TryWarmCacheAsync`: warm the preflight cache from a ready
    /// wanted item (also the bare-series season key).
    pub async fn try_warm_cache(&self, r#type: &str, content_id: &str) {
        let ttl = Duration::from_secs(u64::from(self.settings().await.preflight.ttl_seconds()));
        if self.warm_key(&format!("{type}:{content_id}"), ttl).await {
            return;
        }
        if r#type == "series" {
            let parts: Vec<&str> = content_id.split(':').collect();
            if parts.len() >= 3
                && let Ok(season) = parts[parts.len() - 2].parse::<i32>()
            {
                self.warm_key(&format!("season:{}:{season}", parts[0]), ttl)
                    .await;
            }
        }
    }

    async fn warm_key(&self, key: &str, ttl: Duration) -> bool {
        let Ok(Some(item)) = self.db.wanted_by_key(key).await else {
            return false;
        };
        if item.state != STATE_READY || item.winner_nzb.is_none() {
            return false;
        }
        let Some(winner) = read_pointers(&item.shortlist).into_iter().next() else {
            return false;
        };
        if winner.verdict != "available" {
            return false;
        }
        self.preflight.cache.set_verified(
            &winner.nzb_url,
            item.winner_nzb,
            Verdict::Available,
            item.responder_host,
            ttl,
        );
        debug!(key, title = %winner.title, "watchtower: warmed cache");
        true
    }
}

fn next_backoff(wt: &crate::config::Watchtower, last_resolved: Option<i64>, now: i64) -> i64 {
    let base = i64::from(wt.keepfresh_base_seconds.clamp(60, 86_400));
    let max = i64::from(wt.keepfresh_max_seconds);
    let since = last_resolved.map_or(base, |r| (now - r).max(0));
    since.clamp(base, max)
}

fn make_candidate(p: &WtPointer) -> Candidate {
    Candidate {
        indexer_name: p.indexer_name.clone(),
        indexer_user_agent: p.indexer_user_agent.clone(),
        nzb_url: p.nzb_url.clone(),
        title: p.title.clone(),
        size: p.size,
        posted: None,
        usenet_date: p.usenet_date,
        poster: p.poster.clone(),
        grabs: p.grabs,
        password: None,
        proxy_url: p.proxy_url.clone(),
        source_indexer_name: None,
        language: None,
        subs: None,
        info_hash: None,
        verified_available: false,
    }
}

// --- v1 Watchtower controllers (API-key gated like v1 `BaseApiController`) ---

/// Router for the three v1 watchtower admin routes.
pub fn router(state: Arc<Watchtower>) -> Router {
    Router::new()
        .route("/api/get-watchtower", get(get_watchtower))
        .route("/api/watchtower-mutate", post(mutate))
        .route("/api/watchtower-discover-catalogs", post(discover_catalogs))
        .route_layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            require_api_key,
        ))
        .with_state(state)
}

async fn require_api_key(
    State(state): State<Arc<Watchtower>>,
    req: Request,
    next: Next,
) -> Response {
    let allowed = search::api_key_allowed(
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceDto {
    id: String,
    kind: String,
    name: String,
    url: Option<String>,
    enabled: bool,
    cap: i32,
    series_scope: Option<String>,
    last_synced_at_unix: Option<i64>,
    last_sync_error: Option<String>,
}

impl From<&ListSourceRow> for SourceDto {
    fn from(s: &ListSourceRow) -> Self {
        // Provenance stores the simple (hyphen-less) GUID form; the API shows the plain UUID.
        Self {
            id: s.id.to_string(),
            kind: s.kind.clone(),
            name: s.name.clone(),
            url: s.url.clone(),
            enabled: s.enabled,
            cap: s.cap,
            series_scope: s.series_scope.clone(),
            last_synced_at_unix: s.last_synced_at,
            last_sync_error: s.last_sync_error.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemDto {
    key: String,
    r#type: String,
    content_id: String,
    title: String,
    state: String,
    provenance_count: usize,
    expander_key: Option<String>,
    child_total: Option<i64>,
    child_ready: Option<i64>,
    child_unavailable: Option<i64>,
    shortlist_count: usize,
    winner_title: Option<String>,
    winner_size: i64,
    last_verified_at_unix: Option<i64>,
    next_check_at_unix: Option<i64>,
    fail_reason: Option<String>,
}

fn leaf_item_dto(w: &WantedItemRow) -> ItemDto {
    let shortlist = read_pointers(&w.shortlist);
    let winner = shortlist.first();
    let provenance = read_strings(&w.provenance);
    let expander_key = provenance
        .iter()
        .find(|p| p.starts_with("exp:"))
        .map(|p| p["exp:".len()..].to_owned());
    ItemDto {
        key: w.key.clone(),
        r#type: w.r#type.clone(),
        content_id: w.content_id.clone(),
        title: w.title.clone(),
        state: w.state.clone(),
        provenance_count: provenance.len(),
        expander_key,
        child_total: None,
        child_ready: None,
        child_unavailable: None,
        shortlist_count: shortlist.len(),
        winner_title: winner.map(|p| p.title.clone()),
        winner_size: winner.map_or(0, |p| p.size),
        last_verified_at_unix: w.last_verified_at,
        next_check_at_unix: w.next_check_at,
        fail_reason: w.fail_reason.clone(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatsDto {
    total: i64,
    ready: i64,
    scouting: i64,
    unavailable: i64,
    parked: i64,
    expanders: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GetWatchtowerResponse {
    status: bool,
    enabled: bool,
    sources: Vec<SourceDto>,
    items: Vec<ItemDto>,
    shows: Vec<ItemDto>,
    total: i64,
    has_more: bool,
    stats: StatsDto,
}

/// v1 `GetWatchtowerController`.
async fn get_watchtower(
    State(state): State<Arc<Watchtower>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let stats_only = matches!(
        query.get("statsOnly").map(String::as_str),
        Some("1" | "true")
    );
    let Ok(sources) = state.db.list_sources_all().await else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": false, "error": "database error"})),
        )
            .into_response();
    };
    let counts = state.db.wanted_counts_by_state().await.unwrap_or_default();
    let count_for = |s: &str| counts.iter().find(|(k, _)| k == s).map_or(0, |(_, c)| *c);
    let stats = StatsDto {
        total: counts.iter().map(|(_, c)| c).sum(),
        ready: count_for(STATE_READY),
        scouting: count_for(STATE_SCOUTING),
        unavailable: count_for(STATE_UNAVAILABLE),
        parked: count_for(STATE_PARKED),
        expanders: count_for(STATE_EXPANDER),
    };
    let enabled = state.settings().await.watchtower.enabled;
    let source_dtos: Vec<SourceDto> = sources.iter().map(SourceDto::from).collect();

    if stats_only {
        return Json(GetWatchtowerResponse {
            status: true,
            enabled,
            sources: source_dtos,
            items: Vec::new(),
            shows: Vec::new(),
            total: stats.total,
            has_more: false,
            stats,
        })
        .into_response();
    }

    let offset = query
        .get("offset")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .map_or(100, |l| l.clamp(1, 200));
    let state_filter = query.get("state").cloned().unwrap_or_default();
    let q = query
        .get("q")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    let sort = query.get("sort").cloned().unwrap_or_default();
    let expander_param = query.get("expander").cloned().unwrap_or_default();
    let fetching_children = !expander_param.is_empty();
    let browsing_shows = !fetching_children && state_filter == STATE_EXPANDER;

    let leaf_states = [STATE_READY, STATE_SCOUTING, STATE_UNAVAILABLE, STATE_PARKED];
    let (rows, total) = if browsing_shows {
        (Vec::new(), 0i64)
    } else {
        let tag = fetching_children.then(|| expander_tag(&expander_param));
        let state_param = leaf_states.iter().find(|s| **s == state_filter).copied();
        let (rows, total) = state
            .db
            .wanted_page(
                state_param,
                if q.is_empty() { None } else { Some(&q) },
                tag.as_deref(),
                false,
                &sort,
                if fetching_children { 0 } else { offset },
                if fetching_children { i64::MAX } else { limit },
            )
            .await
            .unwrap_or_default();
        (rows, total)
    };
    let items: Vec<ItemDto> = rows.iter().map(leaf_item_dto).collect();
    let has_more = offset + i64::try_from(items.len()).unwrap_or(i64::MAX) < total;

    let shows = if offset == 0 && !fetching_children {
        build_shows(&state, &state_filter, &q).await
    } else {
        Vec::new()
    };

    Json(GetWatchtowerResponse {
        status: true,
        enabled,
        sources: source_dtos,
        items,
        shows,
        total,
        has_more,
        stats,
    })
    .into_response()
}

/// v1 `BuildShowsAsync`: expander rows with per-expander child tallies.
async fn build_shows(state: &Arc<Watchtower>, state_filter: &str, q: &str) -> Vec<ItemDto> {
    let Ok((mut expanders, _)) = state
        .db
        .wanted_page(Some(STATE_EXPANDER), None, None, true, "title", 0, i64::MAX)
        .await
    else {
        return Vec::new();
    };
    if expanders.is_empty() {
        return Vec::new();
    }
    expanders.sort_by_key(|a| a.title.to_lowercase());
    let Ok(children) = state.db.wanted_leaf_scan().await else {
        return Vec::new();
    };
    let has_state =
        [STATE_READY, STATE_SCOUTING, STATE_UNAVAILABLE, STATE_PARKED].contains(&state_filter);
    let has_q = !q.is_empty();
    let q_lower = q.to_lowercase();
    let mut tally: HashMap<String, [i64; 3]> = HashMap::new();
    let mut relevant: HashSet<String> = HashSet::new();
    for (cstate, title, content_id, provenance) in &children {
        let Some(tag) = read_strings(provenance)
            .into_iter()
            .find(|p| p.starts_with("exp:"))
        else {
            continue;
        };
        let key = tag["exp:".len()..].to_owned();
        let n = tally.entry(key.clone()).or_insert([0, 0, 0]);
        if cstate != STATE_PARKED {
            n[0] += 1;
        }
        if cstate == STATE_READY {
            n[1] += 1;
        }
        if cstate == STATE_UNAVAILABLE {
            n[2] += 1;
        }
        let matches = (!has_state || cstate == state_filter)
            && (!has_q
                || title.to_lowercase().contains(&q_lower)
                || content_id.to_lowercase().contains(&q_lower));
        if matches {
            relevant.insert(key);
        }
    }
    let show_all = !has_state && !has_q;
    expanders
        .iter()
        .filter(|ex| show_all || relevant.contains(&ex.key))
        .map(|ex| {
            let n = tally.get(&ex.key).copied().unwrap_or([0, 0, 0]);
            let mut dto = leaf_item_dto(ex);
            dto.child_total = Some(n[0]);
            dto.child_ready = Some(n[1]);
            dto.child_unavailable = Some(n[2]);
            dto
        })
        .collect()
}

fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"status": false, "error": msg})),
    )
        .into_response()
}

fn parse_source_id(form: &HashMap<String, String>) -> Result<Uuid, Response> {
    form.get("id")
        .and_then(|v| Uuid::parse_str(v.trim()).ok())
        .ok_or_else(|| bad_request("Invalid source id."))
}

fn read_keys(form: &HashMap<String, String>) -> Vec<String> {
    form.get("keys")
        .map(|s| {
            s.split('\n')
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect()
}

async fn manual_source(db: &Db, now: i64) -> Result<ListSourceRow, infinidysk_db::Error> {
    let sources = db.list_sources_all().await?;
    if let Some(m) = sources.iter().find(|s| s.kind == KIND_MANUAL) {
        return Ok(m.clone());
    }
    let row = ListSourceRow {
        id: Uuid::new_v4(),
        kind: KIND_MANUAL.to_owned(),
        name: "Manual".to_owned(),
        url: None,
        enabled: true,
        cap: 0,
        series_scope: None,
        created_at: now,
        last_synced_at: None,
        last_sync_error: None,
    };
    db.upsert_source(&row).await?;
    Ok(row)
}

/// Removes `src_id` from the provenance of every item claiming it; items left with no
/// provenance are removed with their children (v1 `DetachSourceItemsAsync`).
async fn detach_source_items(
    state: &Arc<Watchtower>,
    src_id: &str,
    now: i64,
) -> Result<(), infinidysk_db::Error> {
    let ct = CancellationToken::new();
    let claimed = state.db.wanted_claimed_by(src_id).await?;
    let _permit = state.db.write_permit().await;
    for c in &claimed {
        let mut prov = read_strings(&c.provenance);
        prov.retain(|p| p != src_id);
        if prov.is_empty() {
            state.remove_with_children(c, now, &ct).await?;
        } else {
            let mut updated = c.clone();
            updated.provenance = write_strings(&prov);
            updated.updated_at = now;
            state.db.update_wanted(&updated).await?;
        }
    }
    Ok(())
}

async fn recheck_row(
    state: &Arc<Watchtower>,
    item: &WantedItemRow,
    now: i64,
) -> Result<(), infinidysk_db::Error> {
    let mut updated = item.clone();
    if updated.state == STATE_PARKED {
        STATE_SCOUTING.clone_into(&mut updated.state);
    }
    updated.next_check_at = Some(now);
    updated.updated_at = now;
    state.db.update_wanted(&updated).await
}

#[derive(Deserialize)]
struct SourceInput {
    url: Option<String>,
    name: Option<String>,
    cap: Option<i32>,
    series_scope: Option<String>,
}

/// v1 `WatchtowerMutateController`.
async fn mutate(
    State(state): State<Arc<Watchtower>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let now = now();
    let action = form.get("action").cloned().unwrap_or_default();
    let ct = CancellationToken::new();
    let result: Result<(), Response> = match action.as_str() {
        "add-source" => add_source(&state, &form, now).await,
        "add-sources" => add_sources(&state, &form, now).await,
        "remove-source" => remove_source(&state, &form, now).await,
        "toggle-source" => toggle_source(&state, &form, now).await,
        "set-source-scope" => set_source_scope(&state, &form, now).await,
        "add-item" => add_item(&state, &form, now, &ct).await,
        "remove-item" => remove_item(&state, &form, now).await,
        "remove-items" => remove_items(&state, &form, now).await,
        "recheck-item" => recheck_item(&state, &form, now).await,
        "recheck-items" => recheck_items(&state, &form, now).await,
        "remove-by-filter" => remove_by_filter(&state, &form, now).await,
        "recheck-by-filter" => recheck_by_filter(&state, &form, now).await,
        "sync-source" => sync_source(&state, &form).await,
        other => Err(bad_request(&format!(
            "Unknown watchtower action: '{other}'"
        ))),
    };
    let _ = ct;
    match result {
        Ok(()) => Json(serde_json::json!({"status": true})).into_response(),
        Err(resp) => resp,
    }
}

async fn add_source(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let kind = form.get("kind").cloned().unwrap_or_default();
    if !matches!(
        kind.as_str(),
        KIND_STREMIO_CATALOG | KIND_URL_LIST | KIND_MANUAL
    ) {
        return Err(bad_request(&format!("Unknown source kind: '{kind}'")));
    }
    let is_manual = kind == KIND_MANUAL;
    let url = form
        .get("url")
        .map(|u| u.trim().to_owned())
        .unwrap_or_default();
    if !is_manual && url.is_empty() {
        return Err(bad_request("A URL is required for this source kind."));
    }
    let name = form
        .get("name")
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            if is_manual {
                "Manual".to_owned()
            } else {
                url.clone()
            }
        });
    let cap = form
        .get("cap")
        .and_then(|v| v.parse::<i32>().ok())
        .map_or(0, |c| c.max(0));
    let series_scope = form
        .get("seriesScope")
        .and_then(|v| normalize_series_scope(v));
    let row = ListSourceRow {
        id: Uuid::new_v4(),
        kind,
        name,
        url: if is_manual { None } else { Some(url) },
        enabled: true,
        cap,
        series_scope,
        created_at: now,
        last_synced_at: None,
        last_sync_error: None,
    };
    let _permit = state.db.write_permit().await;
    state
        .db
        .upsert_source(&row)
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    Ok(())
}

async fn add_sources(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let payload = form.get("sources").cloned().unwrap_or_default();
    if payload.trim().is_empty() {
        return Err(bad_request("No catalogs were selected."));
    }
    let inputs: Vec<SourceInput> = serde_json::from_str(&payload)
        .map_err(|e| bad_request(&format!("Invalid catalog selection: {e}")))?;
    if inputs.is_empty() {
        return Err(bad_request("No catalogs were selected."));
    }
    let bulk_scope = form
        .get("seriesScope")
        .and_then(|v| normalize_series_scope(v));
    let sources = state
        .db
        .list_sources_all()
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    let mut existing_urls: HashSet<String> = sources
        .iter()
        .filter_map(|s| s.url.as_ref())
        .map(|u| u.to_lowercase())
        .collect();
    let _permit = state.db.write_permit().await;
    for input in inputs {
        let Some(url) = input
            .url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        else {
            continue;
        };
        if !existing_urls.insert(url.to_lowercase()) {
            continue;
        }
        let row = ListSourceRow {
            id: Uuid::new_v4(),
            kind: KIND_STREMIO_CATALOG.to_owned(),
            name: input
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map_or_else(|| url.to_owned(), str::to_owned),
            url: Some(url.to_owned()),
            enabled: true,
            cap: input.cap.filter(|c| *c > 0).unwrap_or(0),
            series_scope: input
                .series_scope
                .as_deref()
                .and_then(normalize_series_scope)
                .or_else(|| bulk_scope.clone()),
            created_at: now,
            last_synced_at: None,
            last_sync_error: None,
        };
        state
            .db
            .upsert_source(&row)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
    }
    Ok(())
}

async fn remove_source(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let id = parse_source_id(form)?;
    let _permit = state.db.write_permit().await;
    state
        .db
        .delete_source(id)
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    detach_source_items(state, &id.as_simple().to_string(), now)
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    Ok(())
}

async fn toggle_source(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let id = parse_source_id(form)?;
    let sources = state
        .db
        .list_sources_all()
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    let Some(mut source) = sources.into_iter().find(|s| s.id == id) else {
        return Ok(());
    };
    source.enabled = form.get("enabled").map(String::as_str) == Some("true");
    let permit = state.db.write_permit().await;
    state
        .db
        .upsert_source(&source)
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    if source.enabled {
        state
            .db
            .clear_source_sync(id)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
    } else {
        drop(permit);
        detach_source_items(state, &id.as_simple().to_string(), now)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
    }
    Ok(())
}

async fn set_source_scope(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let id = parse_source_id(form)?;
    let sources = state
        .db
        .list_sources_all()
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    let Some(mut source) = sources.into_iter().find(|s| s.id == id) else {
        return Ok(());
    };
    source.series_scope = form
        .get("seriesScope")
        .and_then(|v| normalize_series_scope(v));
    let src_id = id.as_simple().to_string();
    let expanders: Vec<WantedItemRow> = state
        .db
        .wanted_claimed_by(&src_id)
        .await
        .map_err(|e| bad_request(&e.to_string()))?
        .into_iter()
        .filter(|w| w.state == STATE_EXPANDER)
        .collect();
    let _permit = state.db.write_permit().await;
    state
        .db
        .upsert_source(&source)
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    for mut expander in expanders {
        expander.next_check_at = Some(now);
        expander.updated_at = now;
        state
            .db
            .update_wanted(&expander)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
    }
    Ok(())
}

async fn add_item(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
    ct: &CancellationToken,
) -> Result<(), Response> {
    let r#type = match form.get("type").map_or("", String::as_str) {
        "series" | "tv" | "show" => "series",
        _ => "movie",
    };
    let content_id = form
        .get("id")
        .map(|v| v.trim().to_owned())
        .unwrap_or_default();
    if content_id.is_empty() {
        return Err(bad_request("A content id (e.g. an imdb id) is required."));
    }
    let title = match form
        .get("title")
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
    {
        Some(t) if !is_unresolved_title(t, &content_id) => t.to_owned(),
        _ => state
            .resolve_content_title(r#type, &content_id, ct)
            .await
            .unwrap_or_else(|| content_id.clone()),
    };
    let _permit = state.db.write_permit().await;
    let manual = manual_source(&state.db, now)
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    let src_id = manual.id.as_simple().to_string();
    let key = format!("{type}:{content_id}");
    if let Some(existing) = state
        .db
        .wanted_by_key(&key)
        .await
        .map_err(|e| bad_request(&e.to_string()))?
    {
        let mut updated = existing;
        let mut changed = false;
        let mut prov = read_strings(&updated.provenance);
        if !prov.contains(&src_id) {
            prov.push(src_id);
            updated.provenance = write_strings(&prov);
            changed = true;
        }
        let resolved_title = if is_unresolved_title(&updated.title, &updated.content_id) {
            state
                .resolve_content_title(&updated.r#type, &updated.content_id, ct)
                .await
        } else {
            None
        };
        if let Some(resolved) = resolved_title {
            updated.title = resolved;
            changed = true;
        }
        if changed {
            updated.updated_at = now;
            state
                .db
                .update_wanted(&updated)
                .await
                .map_err(|e| bad_request(&e.to_string()))?;
        }
        return Ok(());
    }
    let row = WantedItemRow {
        id: Uuid::new_v4(),
        key,
        r#type: r#type.to_owned(),
        state: if watchtower::is_bare_series(r#type, &content_id) {
            STATE_EXPANDER.to_owned()
        } else {
            STATE_SCOUTING.to_owned()
        },
        content_id,
        title,
        provenance: write_strings(&[src_id]),
        shortlist: "[]".to_owned(),
        winner_nzb: None,
        responder_host: None,
        fail_reason: None,
        created_at: now,
        updated_at: now,
        last_resolved_at: None,
        last_verified_at: None,
        next_check_at: Some(now),
    };
    state
        .db
        .insert_wanted(&row)
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    Ok(())
}

async fn remove_item(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let key = form.get("key").cloned().unwrap_or_default();
    if key.trim().is_empty() {
        return Err(bad_request("An item key is required."));
    }
    let ct = CancellationToken::new();
    let _permit = state.db.write_permit().await;
    if let Some(item) = state
        .db
        .wanted_by_key(&key)
        .await
        .map_err(|e| bad_request(&e.to_string()))?
    {
        state
            .remove_with_children(&item, now, &ct)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
    }
    Ok(())
}

async fn remove_items(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let keys = read_keys(form);
    let ct = CancellationToken::new();
    let _permit = state.db.write_permit().await;
    for chunk in keys.chunks(500) {
        let items = state
            .db
            .wanted_by_keys(chunk)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
        for item in &items {
            state
                .remove_with_children(item, now, &ct)
                .await
                .map_err(|e| bad_request(&e.to_string()))?;
        }
    }
    Ok(())
}

async fn recheck_item(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let key = form.get("key").cloned().unwrap_or_default();
    if key.trim().is_empty() {
        return Err(bad_request("An item key is required."));
    }
    let _permit = state.db.write_permit().await;
    if let Some(item) = state
        .db
        .wanted_by_key(&key)
        .await
        .map_err(|e| bad_request(&e.to_string()))?
    {
        recheck_row(state, &item, now)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
    }
    Ok(())
}

async fn recheck_items(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let keys = read_keys(form);
    let _permit = state.db.write_permit().await;
    for chunk in keys.chunks(500) {
        let items = state
            .db
            .wanted_by_keys(chunk)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
        for item in &items {
            recheck_row(state, item, now)
                .await
                .map_err(|e| bad_request(&e.to_string()))?;
        }
    }
    Ok(())
}

async fn remove_by_filter(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let keys = filtered_keys(state, form).await?;
    let ct = CancellationToken::new();
    let _permit = state.db.write_permit().await;
    for chunk in keys.chunks(500) {
        let items = state
            .db
            .wanted_by_keys(chunk)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
        for item in &items {
            state
                .remove_with_children(item, now, &ct)
                .await
                .map_err(|e| bad_request(&e.to_string()))?;
        }
    }
    Ok(())
}

async fn recheck_by_filter(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
    now: i64,
) -> Result<(), Response> {
    let keys = filtered_keys(state, form).await?;
    let _permit = state.db.write_permit().await;
    for chunk in keys.chunks(500) {
        let items = state
            .db
            .wanted_by_keys(chunk)
            .await
            .map_err(|e| bad_request(&e.to_string()))?;
        for item in &items {
            recheck_row(state, item, now)
                .await
                .map_err(|e| bad_request(&e.to_string()))?;
        }
    }
    Ok(())
}

async fn filtered_keys(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
) -> Result<Vec<String>, Response> {
    let st = form.get("state").map(String::as_str);
    let q = form.get("q").map(|s| s.trim().to_owned());
    state
        .db
        .wanted_keys_filtered(st, q.as_deref())
        .await
        .map_err(|e| bad_request(&e.to_string()))
}

async fn sync_source(
    state: &Arc<Watchtower>,
    form: &HashMap<String, String>,
) -> Result<(), Response> {
    let id = parse_source_id(form)?;
    let _permit = state.db.write_permit().await;
    state
        .db
        .clear_source_sync(id)
        .await
        .map_err(|e| bad_request(&e.to_string()))?;
    Ok(())
}

/// v1 `WatchtowerDiscoverController`: form `url` when the body is form-encoded, else query.
async fn discover_catalogs(State(state): State<Arc<Watchtower>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let is_form = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"));
    let url = if is_form {
        match Form::<HashMap<String, String>>::from_request(
            Request::from_parts(parts, body),
            &state,
        )
        .await
        {
            Ok(Form(f)) => f.get("url").cloned().unwrap_or_default(),
            Err(_) => String::new(),
        }
    } else {
        Query::<HashMap<String, String>>::try_from_uri(&parts.uri)
            .map(|Query(m)| m.get("url").cloned().unwrap_or_default())
            .unwrap_or_default()
    };
    if url.trim().is_empty() {
        return bad_request("A manifest URL is required.");
    }
    let ct = CancellationToken::new();
    match state.enumerator.discover_catalogs(url.trim(), &ct).await {
        Ok(result) => {
            let catalogs: Vec<serde_json::Value> = result
                .catalogs
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "type": c.r#type,
                        "id": c.id,
                        "name": c.name,
                        "url": c.url,
                        "extraRequired": c.extra_required,
                    })
                })
                .collect();
            Json(serde_json::json!({
                "status": true,
                "addonName": result.addon_name,
                "catalogs": catalogs,
            }))
            .into_response()
        }
        Err(e) => bad_request(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(season: i32, number: i32, air: Option<i64>) -> Episode {
        Episode {
            season,
            number,
            air_date_unix: air,
            title: None,
        }
    }

    fn cfg() -> crate::config::Watchtower {
        crate::config::Watchtower::default()
    }

    #[test]
    fn expander_tag_and_pointer_roundtrip() {
        assert_eq!(expander_tag("series:tt0000001"), "exp:series:tt0000001");
        let p = WtPointer {
            nzb_url: "https://nzb/1".to_owned(),
            indexer_name: "idx".to_owned(),
            indexer_user_agent: "ua".to_owned(),
            proxy_url: None,
            title: "Release".to_owned(),
            size: 42,
            grabs: Some(3),
            poster: None,
            usenet_date: None,
            verdict: "available".to_owned(),
            last_verified_at_unix: Some(7),
        };
        let json = write_pointers(&[p]);
        assert!(
            json.contains("\"NzbUrl\""),
            "PascalCase like v1 STJ: {json}"
        );
        let back = read_pointers(&json);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].nzb_url, "https://nzb/1");
        assert_eq!(back[0].size, 42);
        assert_eq!(back[0].last_verified_at_unix, Some(7));
        assert_eq!(read_pointers("not json"), Vec::<WtPointer>::new());
        assert_eq!(
            write_strings(&["a".to_owned(), "b".to_owned()]),
            "[\"a\",\"b\"]"
        );
        assert_eq!(
            read_strings("[\"a\",\"b\"]"),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn is_unresolved_title_detects_raw_id_fallbacks() {
        assert!(is_unresolved_title("", "tt1234567:2:1"));
        assert!(is_unresolved_title("tt1234567:2:1", "tt1234567:2:1"));
        assert!(is_unresolved_title("tt1234567 S02E01", "tt1234567:2:1"));
        assert!(is_unresolved_title("tt1234567", "tt1234567"));
        assert!(!is_unresolved_title(
            "Example Series S02E01",
            "tt1234567:2:1"
        ));
        assert!(!is_unresolved_title("Example Movie", "tt1234567"));
        assert!(!is_unresolved_title("Some name", "kitsu:123:4"));
        assert!(is_unresolved_title("kitsu:123:4", "kitsu:123:4"));
    }

    #[test]
    fn compose_imdb_title_appends_season_and_episode_codes() {
        assert_eq!(
            compose_imdb_title("Example Movie", "tt1234567"),
            "Example Movie"
        );
        assert_eq!(
            compose_imdb_title("Example Series", "tt1234567:2"),
            "Example Series S02"
        );
        assert_eq!(
            compose_imdb_title("Example Series", "tt1234567:2:1"),
            "Example Series S02E01"
        );
        assert_eq!(
            compose_imdb_title("Example Series", "tt1234567:11:13"),
            "Example Series S11E13"
        );
    }

    #[test]
    fn normalize_series_scope_matches_v1() {
        for v in [
            "latest-season",
            "first-season",
            "all-aired",
            "recent",
            "off",
        ] {
            assert_eq!(normalize_series_scope(v), Some(v.to_owned()));
        }
        assert_eq!(normalize_series_scope("bogus"), None);
        assert_eq!(normalize_series_scope(""), None);
    }

    #[test]
    fn normalize_type_maps_series_aliases() {
        assert_eq!(normalize_type("series"), "series");
        assert_eq!(normalize_type("TV"), "series");
        assert_eq!(normalize_type(" show "), "series");
        assert_eq!(normalize_type("movie"), "movie");
        assert_eq!(normalize_type("anything"), "movie");
    }

    #[test]
    fn manifest_url_normalization() {
        assert_eq!(
            normalize_manifest_url("stremio://addon.example.com"),
            "https://addon.example.com/manifest.json"
        );
        assert_eq!(
            normalize_manifest_url("https://addon.example.com/"),
            "https://addon.example.com/manifest.json"
        );
        assert_eq!(
            normalize_manifest_url("https://addon.example.com/manifest.json"),
            "https://addon.example.com/manifest.json"
        );
        assert_eq!(
            strip_manifest_suffix("https://addon.example.com/manifest.json"),
            "https://addon.example.com"
        );
    }

    #[test]
    fn paged_url_skip_logic() {
        assert_eq!(build_paged_url("https://x/list", 0), "https://x/list");
        assert_eq!(
            build_paged_url("https://x/list", 100),
            "https://x/list?skip=100"
        );
        assert_eq!(
            build_paged_url("https://x/list?a=1", 100),
            "https://x/list?a=1&skip=100"
        );
        assert_eq!(
            build_paged_url("https://x/catalog/series/top.json", 100),
            "https://x/catalog/series/top/skip=100.json"
        );
        assert_eq!(
            build_paged_url("https://x/catalog/series/top/genre=action.json", 100),
            "https://x/catalog/series/top/genre=action&skip=100.json"
        );
        assert_eq!(
            build_paged_url("https://x/catalog/series/top/skip=50.json", 100),
            "https://x/catalog/series/top/skip=100.json"
        );
    }

    #[test]
    fn split_type_id_lines() {
        assert_eq!(
            split_type_id("tt1234567"),
            ("movie".to_owned(), "tt1234567".to_owned())
        );
        assert_eq!(
            split_type_id("series:tt1234567"),
            ("series".to_owned(), "tt1234567".to_owned())
        );
        assert_eq!(
            split_type_id("movie:tt1234567:1"),
            ("movie".to_owned(), "tt1234567:1".to_owned())
        );
        assert_eq!(
            split_type_id("whatever"),
            ("movie".to_owned(), "whatever".to_owned())
        );
    }

    #[test]
    fn desired_rows_latest_season_bundle_when_complete() {
        let c = cfg();
        let now = 1_800_000_000i64;
        let day = 86_400i64;
        let eps = vec![
            ep(1, 1, Some(now - 100 * day)),
            ep(1, 2, Some(now - 99 * day)),
            ep(2, 1, Some(now - 20 * day)),
        ];
        let rows = build_desired_rows(
            &c,
            &eps,
            "Show",
            "tt0000001",
            "latest-season",
            now,
            &HashSet::new(),
        );
        // Latest season 2 aired 20 days ago (> 14d grace) and bundles are on: one season row.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "season:tt0000001:2");
        assert_eq!(rows[0].1.0, "season");
        assert_eq!(rows[0].1.2, "Show S02 (season bundle)");
    }

    #[test]
    fn desired_rows_episodes_when_season_incomplete() {
        let c = cfg();
        let now = 1_800_000_000i64;
        let day = 86_400i64;
        let eps = vec![ep(2, 1, Some(now - 5 * day)), ep(2, 2, Some(now - 4 * day))];
        let rows = build_desired_rows(
            &c,
            &eps,
            "Show",
            "tt0000001",
            "latest-season",
            now,
            &HashSet::new(),
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "series:tt0000001:2:1");
        assert_eq!(rows[0].1.2, "Show S02E01");
        assert_eq!(rows[1].0, "series:tt0000001:2:2");
    }

    #[test]
    fn desired_rows_recent_scope_takes_last_n() {
        let mut c = cfg();
        c.series_recent_count = 3;
        let now = 1_800_000_000i64;
        let day = 86_400i64;
        let eps: Vec<Episode> = (1..=5)
            .map(|n| ep(1, n, Some(now - i64::from(6 - n) * day)))
            .collect();
        let rows = build_desired_rows(
            &c,
            &eps,
            "Show",
            "tt0000001",
            "recent",
            now,
            &HashSet::new(),
        );
        let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "series:tt0000001:1:3",
                "series:tt0000001:1:4",
                "series:tt0000001:1:5"
            ]
        );
    }

    #[test]
    fn desired_rows_all_aired_lists_seasons_newest_first() {
        let mut c = cfg();
        c.season_bundles = false;
        let now = 1_800_000_000i64;
        let day = 86_400i64;
        let eps = vec![
            ep(1, 1, Some(now - 100 * day)),
            ep(2, 1, Some(now - 50 * day)),
            ep(3, 1, Some(now - day)),
        ];
        let rows = build_desired_rows(
            &c,
            &eps,
            "Show",
            "tt0000001",
            "all-aired",
            now,
            &HashSet::new(),
        );
        let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "series:tt0000001:3:1",
                "series:tt0000001:2:1",
                "series:tt0000001:1:1"
            ]
        );
    }

    #[test]
    fn clamp_series_keeps_newest_episodes_and_all_seasons() {
        let mut c = cfg();
        c.series_max_episodes = 2;
        c.series_cap_keep = "newest".to_owned();
        let mut desired = HashMap::new();
        for (k, v) in [
            ("series:tt:1:1", ("series", "tt:1:1", "S01E01")),
            ("series:tt:1:2", ("series", "tt:1:2", "S01E02")),
            ("series:tt:2:1", ("series", "tt:2:1", "S02E01")),
            ("season:tt:1", ("season", "tt:1", "S01 bundle")),
        ] {
            desired.insert(
                k.to_owned(),
                (v.0.to_owned(), v.1.to_owned(), v.2.to_owned()),
            );
        }
        let kept = clamp_series(&c, desired);
        assert!(kept.contains_key("season:tt:1"));
        assert!(kept.contains_key("series:tt:2:1"));
        assert!(kept.contains_key("series:tt:1:2"));
        assert!(!kept.contains_key("series:tt:1:1"));
    }

    #[test]
    fn row_order_season_after_episodes_of_same_season() {
        assert_eq!(
            row_order(&("season".to_owned(), "tt:2".to_owned(), String::new())),
            (2, i32::MAX)
        );
        assert_eq!(
            row_order(&("series".to_owned(), "tt:2:5".to_owned(), String::new())),
            (2, 5)
        );
        assert_eq!(
            row_order(&("series".to_owned(), "kitsu:99:7".to_owned(), String::new())),
            (1, 7)
        );
    }

    #[test]
    fn season_complete_grace_window() {
        let now = 1_800_000_000i64;
        let day = 86_400i64;
        let old = vec![
            ep(1, 1, Some(now - 30 * day)),
            ep(1, 2, Some(now - 20 * day)),
        ];
        assert!(season_complete(&old, 1, now));
        let recent = vec![ep(1, 1, Some(now - 5 * day))];
        assert!(!season_complete(&recent, 1, now));
        let future = vec![ep(1, 1, Some(now + day))];
        assert!(!season_complete(&future, 1, now));
        assert!(!season_complete(&[], 1, now));
    }

    #[test]
    fn bare_series_detection() {
        assert!(watchtower::is_bare_series("series", "tt0000001"));
        assert!(watchtower::is_bare_series("series", "tt0000001:2"));
        assert!(!watchtower::is_bare_series("series", "tt0000001:2:1"));
        assert!(!watchtower::is_bare_series("movie", "tt0000001"));
    }

    #[test]
    fn anime_desired_rows_recent_scope() {
        let mut c = cfg();
        c.series_recent_count = 2;
        let now = 1_800_000_000i64;
        let eps: Vec<Episode> = (1..=4)
            .map(|n| ep(0, n, Some(now - i64::from(5 - n))))
            .collect();
        let rows = build_anime_desired_rows(&c, &eps, "Anime", "99", "recent", now);
        let keys: Vec<&str> = rows.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["series:kitsu:99:3", "series:kitsu:99:4"]);
        assert_eq!(rows[0].1.2, "Anime E03");
    }

    #[test]
    fn next_backoff_grows_between_base_and_max() {
        let mut c = cfg();
        c.keepfresh_base_seconds = 3600;
        c.keepfresh_max_seconds = 86_400;
        let now = 1_800_000_000i64;
        assert_eq!(next_backoff(&c, None, now), 3600);
        assert_eq!(next_backoff(&c, Some(now - 7200), now), 7200);
        assert_eq!(next_backoff(&c, Some(now - 10 * 86_400), now), 86_400);
    }

    #[test]
    fn json_list_parses_embedded_plain_list_envelope() {
        let refs = parse_json_list(
            br##"[{"list":"# exported\nseries:tt0000001:10:7\nmovie:tt0000002\n"}]"##,
        )
        .unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].r#type, "series");
        assert_eq!(refs[0].content_id, "tt0000001:10:7");
        assert_eq!(refs[1].r#type, "movie");
        assert_eq!(refs[1].content_id, "tt0000002");
    }

    #[test]
    fn json_list_parses_root_object_list_property() {
        let refs = parse_json_list(br#"{"list":"tt0000003\nseries:tt0000004:1:1"}"#).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].r#type, "movie");
        assert_eq!(refs[0].content_id, "tt0000003");
        assert_eq!(refs[1].r#type, "series");
        assert_eq!(refs[1].content_id, "tt0000004:1:1");
    }

    #[test]
    fn json_list_keeps_string_object_and_items_forms() {
        let refs = parse_json_list(
            br#"["tt0000005",{"id":"tt0000006","type":"series","name":"n"},{"imdb":"tt0000007"}]"#,
        )
        .unwrap();
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].content_id, "tt0000005");
        assert_eq!(refs[1].r#type, "series");
        assert_eq!(refs[1].title.as_deref(), Some("n"));
        assert_eq!(refs[2].content_id, "tt0000007");
        let items = parse_json_list(br#"{"items":["tt0000008"]}"#).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content_id, "tt0000008");
        assert!(
            parse_json_list(br#"{"unrelated":true}"#)
                .unwrap()
                .is_empty()
        );
    }

    async fn serve_once(response: Vec<u8>) -> (String, tokio::sync::mpsc::Receiver<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(1);
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&request).to_string()).await;
            sock.write_all(&response).await.unwrap();
        });
        (format!("http://{addr}/list"), rx)
    }

    #[tokio::test]
    async fn url_list_reports_http_status_when_server_rejects() {
        let (url, _rx) = serve_once(
            b"HTTP/1.1 415 Unsupported Media Type\r\ncontent-type: text/plain\r\ncontent-length: 4\r\nconnection: close\r\n\r\n415!"
                .to_vec(),
        )
        .await;
        let enumerator = ListSourceEnumerator::new(1024 * 1024);
        let ct = CancellationToken::new();
        let err = enumerator
            .fetch_url_list(Some(&url), &ct)
            .await
            .unwrap_err();
        assert!(err.contains("HTTP 415"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn url_list_sends_accept_and_parses_list_envelope() {
        let payload = br#"[{"list":"series:tt0000001:10:7\n"}]"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            payload.len()
        );
        let mut response = head.into_bytes();
        response.extend_from_slice(payload);
        let (url, mut rx) = serve_once(response).await;
        let enumerator = ListSourceEnumerator::new(1024 * 1024);
        let ct = CancellationToken::new();
        let refs = enumerator.fetch_url_list(Some(&url), &ct).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].r#type, "series");
        assert_eq!(refs[0].content_id, "tt0000001:10:7");
        let request = rx.recv().await.unwrap();
        let lower = request.to_lowercase();
        assert!(
            lower.contains("accept: application/json"),
            "request lacked an Accept header: {request}"
        );
    }
}
