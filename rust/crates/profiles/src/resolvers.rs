//! External-ID resolvers, ported from the v1 fork: `TvdbIdResolver` (`TVmaze` + `Wikidata`),
//! `TmdbIdResolver` (`Wikidata`), `ImdbTitleResolver` (`TVmaze` + `Wikidata`),
//! `ExternalIdResolver` + `AnimeListMappingResolver` (Kitsu + the community anime-lists
//! dataset). All upstream APIs are keyless in v1, so no API-key config is needed here
//! (Prowlarr's key lives in the indexer config). Each resolver caches positive results
//! for 24h and negative results for 15min. HTTP goes through the shared SOCKS5-aware
//! transport in `infinidysk-indexer`; lookups never retry and never fail the caller.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Url;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

use infinidysk_indexer::{Error, http};

/// v1 `Uri.EscapeDataString` (RFC 3986 unreserved set kept).
const ESCAPE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn escape(s: &str) -> String {
    utf8_percent_encode(s, ESCAPE_SET).to_string()
}

const TVMAZE_BASE: &str = "https://api.tvmaze.com";
const WIKIDATA_BASE: &str = "https://query.wikidata.org/sparql";
const KITSU_BASE: &str = "https://kitsu.io/api/edge";
const ANIME_LISTS_URL: &str =
    "https://raw.githubusercontent.com/Fribb/anime-lists/master/anime-list-full.json";

const RESOLVER_UA: &str = "InfiniDysk (https://github.com/infinidysk/infinidysk)";
const CACHE_TTL: SignedDuration = SignedDuration::from_hours(24);
const NEGATIVE_TTL: SignedDuration = SignedDuration::from_secs(15 * 60);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(6);
const EXTERNAL_TIMEOUT: Duration = Duration::from_secs(10);
const DATASET_TIMEOUT: Duration = Duration::from_secs(60);
const DATASET_MAX_BYTES: u64 = 256 * 1024 * 1024;
const FIRST_LOAD_WAIT: Duration = Duration::from_secs(8);
const REFRESH_INTERVAL: SignedDuration = SignedDuration::from_hours(24);
const RETRY_INTERVAL: SignedDuration = SignedDuration::from_secs(5 * 60);

/// Base URLs / timeouts for the resolvers; override the bases in tests (loopback server).
#[derive(Debug, Clone)]
pub struct ResolverConfig {
    pub tvmaze_base: String,
    pub wikidata_base: String,
    pub kitsu_base: String,
    pub anime_lists_url: String,
    /// Master switch: when false every lookup returns `None` without any I/O (offline mode).
    pub enabled: bool,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            tvmaze_base: TVMAZE_BASE.to_owned(),
            wikidata_base: WIKIDATA_BASE.to_owned(),
            kitsu_base: KITSU_BASE.to_owned(),
            anime_lists_url: ANIME_LISTS_URL.to_owned(),
            enabled: true,
        }
    }
}

/// GET `url` as JSON through the shared transport. No retry (v1 sends once), `timeout`
/// budget, caller cancellation wins. Any failure → `None`.
async fn fetch_json(
    url: &str,
    accept: Option<&str>,
    timeout: Duration,
    max_bytes: u64,
    ct: &CancellationToken,
) -> Option<Value> {
    let host = Url::parse(url).ok()?.host_str()?.to_owned();
    http::with_timeout(timeout, ct, async {
        let client = http::build_client(false, &host)?;
        let mut req = client
            .get(url)
            .header(reqwest::header::USER_AGENT, RESOLVER_UA);
        if let Some(accept) = accept {
            req = req.header(reqwest::header::ACCEPT, accept);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return Err(if ct.is_cancelled() {
                    Error::Cancelled
                } else {
                    match e.status() {
                        Some(s) => Error::Status(s),
                        None => Error::Transport,
                    }
                });
            }
        };
        let status = resp.status();
        if !status.is_success() {
            if status.as_u16() == 429 || status.as_u16() >= 500 {
                tracing::warn!(
                    "resolver GET {url} returned HTTP {status} — rate-limited or unavailable"
                );
            } else {
                tracing::debug!("resolver GET {url} returned HTTP {status}");
            }
            return Err(Error::Status(status));
        }
        let status_code = status.as_u16();
        let content_type = http::safe_media_type(resp.headers());
        let body = http::read_bounded(resp, max_bytes).await?;
        serde_json::from_slice(&body)
            .map_err(|_| Error::format(status_code, &content_type, body.len(), 0))
    })
    .await
    .ok()
}

async fn fetch_json_stream(
    url: &str,
    timeout: Duration,
    ct: &CancellationToken,
) -> Option<Vec<Value>> {
    let host = Url::parse(url).ok()?.host_str()?.to_owned();
    http::with_timeout(timeout, ct, async {
        let client = http::build_client(false, &host)?;
        let resp = client
            .get(url)
            .header(reqwest::header::USER_AGENT, RESOLVER_UA)
            .send()
            .await
            .map_err(|e| match e.status() {
                Some(s) => Error::Status(s),
                None => Error::Transport,
            })?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status()));
        }
        let status_code = resp.status().as_u16();
        let content_type = http::safe_media_type(resp.headers());
        let body = http::read_bounded(resp, DATASET_MAX_BYTES).await?;
        serde_json::from_slice(&body)
            .map_err(|_| Error::format(status_code, &content_type, body.len(), 0))
    })
    .await
    .ok()
}

struct Cache<T> {
    map: Mutex<HashMap<String, (Option<T>, Timestamp)>>,
}

impl<T: Clone> Cache<T> {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    async fn get(&self, key: &str) -> Option<Option<T>> {
        let map = self.map.lock().await;
        map.get(key)
            .filter(|(_, expires)| *expires > Timestamp::now())
            .map(|(v, _)| v.clone())
    }

    async fn put(&self, key: &str, value: Option<T>) {
        let ttl = if value.is_some() {
            CACHE_TTL
        } else {
            NEGATIVE_TTL
        };
        let expires = Timestamp::now() + ttl;
        self.map
            .lock()
            .await
            .insert(key.to_owned(), (value, expires));
    }
}

// --- TVmaze / Wikidata parsing helpers (also unit-tested directly) ---

fn tvmaze_thetvdb(root: &Value) -> Option<i32> {
    let externals = root.get("externals")?;
    let tvdb = externals.get("thetvdb")?;
    tvdb.as_i64().and_then(|v| i32::try_from(v).ok())
}

fn parse_tvmaze_title(root: &Value) -> Option<String> {
    let name = root.get("name")?.as_str()?;
    if name.trim().is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

fn sparql_bindings(root: &Value) -> Option<&Vec<Value>> {
    root.get("results")?.get("bindings")?.as_array()
}

fn sparql_string(node: &Value) -> Option<&str> {
    node.get("value")?.as_str()
}

/// v1 `ImdbTitleResolver.ParseWikidataMetadata`: first non-blank label as title, earliest
/// valid year (1900..=2199) from a `year` or `date` binding.
fn parse_wikidata_metadata(root: &Value) -> Option<ResolvedTitleMetadata> {
    if !root.is_object() {
        return None;
    }
    let bindings = sparql_bindings(root)?;
    if bindings.is_empty() {
        return None;
    }
    let mut title: Option<String> = None;
    let mut earliest_year: Option<i32> = None;
    for binding in bindings {
        if title.is_none()
            && let Some(label) = binding.get("label").and_then(sparql_string)
            && !label.trim().is_empty()
        {
            title = Some(label.to_owned());
        }
        if let Some(year) = read_sparql_year(binding)
            && (earliest_year.is_none() || year < earliest_year.unwrap_or(i32::MAX))
        {
            earliest_year = Some(year);
        }
    }
    title.map(|t| ResolvedTitleMetadata {
        title: t,
        year: earliest_year,
    })
}

fn read_sparql_year(binding: &Value) -> Option<i32> {
    if let Some(y) = binding
        .get("year")
        .and_then(sparql_string)
        .and_then(|s| s.parse::<i32>().ok())
        && (1900..=2199).contains(&y)
    {
        return Some(y);
    }
    if let Some(d) = binding.get("date").and_then(sparql_string)
        && d.len() >= 4
        && let Ok(y) = d[..4].parse::<i32>()
        && (1900..=2199).contains(&y)
    {
        return Some(y);
    }
    None
}

fn normalize_name(s: Option<&str>) -> String {
    let Some(s) = s else { return String::new() };
    let mut sb = String::with_capacity(s.len());
    for ch in s.to_lowercase().chars() {
        if ch.is_alphanumeric() {
            sb.push(ch);
        } else if ch.is_whitespace() || matches!(ch, '-' | ':' | '.' | '\'' | '_') {
            sb.push(' ');
        }
    }
    sb.split(' ')
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn names_equal(a: Option<&str>, b: Option<&str>) -> bool {
    let na = normalize_name(a);
    !na.is_empty() && na == normalize_name(b)
}

// --- TvdbIdResolver ---

/// Ported from `backend/Services/TvdbIdResolver.cs`: imdb digits → tvdb id via `TVmaze`,
/// `Wikidata` P345→P4835, and a title+year fallback through `TVmaze`'s name search.
pub struct TvdbIdResolver {
    cfg: ResolverConfig,
    cache: Cache<i32>,
}

impl TvdbIdResolver {
    pub fn new(cfg: ResolverConfig) -> Self {
        Self {
            cfg,
            cache: Cache::new(),
        }
    }

    pub async fn get_tvdb_id(&self, imdb_digits: &str, ct: &CancellationToken) -> Option<i32> {
        if let Some(cached) = self.cache.get(imdb_digits).await {
            return cached;
        }
        let tvdb = if self.cfg.enabled {
            let tvmaze = self.try_tvmaze(imdb_digits, ct).await;
            let wikidata = self.try_wikidata(imdb_digits, ct).await;
            let mut tvdb = tvmaze.or(wikidata);
            if tvdb.is_none() {
                let (title, year) = self.try_wikidata_title_and_year(imdb_digits, ct).await;
                tvdb = self.try_tvmaze_by_title(title.as_deref(), year, ct).await;
            }
            if tvdb.is_none() {
                tracing::info!(
                    "TvdbResolver: no tvdb id for tt{imdb_digits}; TV search will fall back to imdbid"
                );
            } else {
                tracing::info!(
                    "TvdbResolver: resolved tt{imdb_digits} to tvdb {}",
                    tvdb.unwrap_or(0)
                );
            }
            tvdb
        } else {
            None
        };
        self.cache.put(imdb_digits, tvdb).await;
        tvdb
    }

    async fn try_tvmaze(&self, imdb_digits: &str, ct: &CancellationToken) -> Option<i32> {
        let url = format!("{}/lookup/shows?imdb=tt{imdb_digits}", self.cfg.tvmaze_base);
        let root = fetch_json(&url, None, LOOKUP_TIMEOUT, 1 << 20, ct).await?;
        if !root.is_object() {
            return None;
        }
        tvmaze_thetvdb(&root)
    }

    async fn try_wikidata(&self, imdb_digits: &str, ct: &CancellationToken) -> Option<i32> {
        let query = format!(
            "SELECT ?tvdb WHERE {{ ?item wdt:P345 \"tt{imdb_digits}\" . ?item wdt:P4835 ?tvdb . }} LIMIT 1"
        );
        let url = format!("{}?query={}", self.cfg.wikidata_base, escape(&query));
        let root = fetch_json(
            &url,
            Some("application/sparql-results+json"),
            LOOKUP_TIMEOUT,
            1 << 20,
            ct,
        )
        .await?;
        let bindings = sparql_bindings(&root)?;
        let first = bindings.first()?;
        first
            .get("tvdb")?
            .get("value")?
            .as_str()?
            .parse::<i32>()
            .ok()
    }

    async fn try_tvmaze_by_title(
        &self,
        title: Option<&str>,
        year: Option<i32>,
        ct: &CancellationToken,
    ) -> Option<i32> {
        let title = title?;
        if title.trim().is_empty() {
            return None;
        }
        let url = format!(
            "{}/singlesearch/shows?q={}",
            self.cfg.tvmaze_base,
            escape(title)
        );
        let root = fetch_json(&url, None, LOOKUP_TIMEOUT, 1 << 20, ct).await?;
        if !root.is_object() {
            return None;
        }
        let name = root.get("name").and_then(|n| n.as_str());
        if !names_equal(name, Some(title)) {
            return None;
        }
        if let (Some(expected_year), Some(prem)) = (
            year,
            root.get("premiered")
                .and_then(|p| p.as_str())
                .filter(|p| p.len() >= 4),
        ) && let Ok(prem_year) = prem[..4].parse::<i32>()
            && (prem_year - expected_year).abs() > 1
        {
            return None;
        }
        tvmaze_thetvdb(&root)
    }

    async fn try_wikidata_title_and_year(
        &self,
        imdb_digits: &str,
        ct: &CancellationToken,
    ) -> (Option<String>, Option<i32>) {
        let query = format!(
            "SELECT ?label ?date WHERE {{ ?item wdt:P345 \"tt{imdb_digits}\" . ?item rdfs:label ?label . FILTER(LANG(?label) = \"en\") OPTIONAL {{ ?item wdt:P580 ?start }} OPTIONAL {{ ?item wdt:P577 ?pub }} BIND(COALESCE(?start, ?pub) AS ?date) }} LIMIT 1"
        );
        let url = format!("{}?query={}", self.cfg.wikidata_base, escape(&query));
        let Some(root) = fetch_json(
            &url,
            Some("application/sparql-results+json"),
            LOOKUP_TIMEOUT,
            1 << 20,
            ct,
        )
        .await
        else {
            return (None, None);
        };
        let Some(bindings) = sparql_bindings(&root) else {
            return (None, None);
        };
        let Some(row) = bindings.first() else {
            return (None, None);
        };
        let title = row
            .get("label")
            .and_then(sparql_string)
            .filter(|t| !t.trim().is_empty())
            .map(str::to_owned);
        let year = row
            .get("date")
            .and_then(sparql_string)
            .filter(|d| d.len() >= 4)
            .and_then(|d| d[..4].parse::<i32>().ok());
        (title, year)
    }
}

// --- TmdbIdResolver ---

/// Ported from `backend/Services/TmdbIdResolver.cs`: tmdb id → imdb id via Wikidata
/// (P4947 for movies, P4983 for series).
pub struct TmdbIdResolver {
    cfg: ResolverConfig,
    cache: Cache<String>,
}

impl TmdbIdResolver {
    pub fn new(cfg: ResolverConfig) -> Self {
        Self {
            cfg,
            cache: Cache::new(),
        }
    }

    pub async fn get_imdb_id(
        &self,
        r#type: &str,
        tmdb_id: &str,
        ct: &CancellationToken,
    ) -> Option<String> {
        let property = match r#type {
            "movie" => "P4947",
            "series" => "P4983",
            _ => return None,
        };
        if tmdb_id.parse::<u64>().is_err() {
            return None;
        }
        let key = format!("{type}|{tmdb_id}");
        if let Some(cached) = self.cache.get(&key).await {
            return cached;
        }
        let imdb = if self.cfg.enabled {
            let query = format!(
                "SELECT ?imdb WHERE {{ ?item wdt:{property} \"{tmdb_id}\" . ?item wdt:P345 ?imdb . }} LIMIT 1"
            );
            let url = format!("{}?query={}", self.cfg.wikidata_base, escape(&query));
            fetch_json(
                &url,
                Some("application/sparql-results+json"),
                LOOKUP_TIMEOUT,
                1 << 20,
                ct,
            )
            .await
            .and_then(|root| {
                let bindings = sparql_bindings(&root)?;
                let imdb = bindings.first()?.get("imdb")?.get("value")?.as_str()?;
                if imdb.trim().is_empty() {
                    return None;
                }
                Some(if imdb.starts_with("tt") {
                    imdb.to_owned()
                } else {
                    format!("tt{imdb}")
                })
            })
        } else {
            None
        };
        self.cache.put(&key, imdb.clone()).await;
        imdb
    }
}

// --- ImdbTitleResolver ---

/// v1 `ResolvedTitleMetadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTitleMetadata {
    pub title: String,
    pub year: Option<i32>,
}

/// Ported from `backend/Services/ImdbTitleResolver.cs`: canonical title (and year for
/// movies) for an imdb id / tvdb id via `TVmaze` and `Wikidata`.
pub struct ImdbTitleResolver {
    cfg: ResolverConfig,
    cache: Cache<ResolvedTitleMetadata>,
}

impl ImdbTitleResolver {
    pub fn new(cfg: ResolverConfig) -> Self {
        Self {
            cfg,
            cache: Cache::new(),
        }
    }

    #[must_use]
    pub async fn get_title(
        &self,
        r#type: &str,
        imdb_digits: Option<&str>,
        tvdb_id: Option<i32>,
        ct: &CancellationToken,
    ) -> Option<String> {
        self.get_metadata(r#type, imdb_digits, tvdb_id, ct)
            .await
            .map(|m| m.title)
    }

    pub async fn get_metadata(
        &self,
        r#type: &str,
        imdb_digits: Option<&str>,
        tvdb_id: Option<i32>,
        ct: &CancellationToken,
    ) -> Option<ResolvedTitleMetadata> {
        let key = format!(
            "{type}|{}|{}",
            imdb_digits.unwrap_or(""),
            tvdb_id.map_or(String::new(), |t| t.to_string())
        );
        if let Some(cached) = self.cache.get(&key).await {
            return cached;
        }
        let metadata = if self.cfg.enabled {
            if r#type == "series" {
                let mut title = self.try_tvmaze(imdb_digits, tvdb_id, ct).await;
                if title.as_deref().is_none_or(|t| t.trim().is_empty())
                    && let Some(imdb) = imdb_digits
                {
                    title = self.try_wikidata(imdb, ct).await.map(|m| m.title);
                }
                // Series premiere/release years are not used for the movie-only year gate.
                title
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| ResolvedTitleMetadata {
                        title: t,
                        year: None,
                    })
            } else if r#type == "movie"
                && let Some(imdb) = imdb_digits
            {
                self.try_wikidata(imdb, ct).await
            } else {
                None
            }
        } else {
            None
        };
        self.cache.put(&key, metadata.clone()).await;
        metadata
    }

    async fn try_tvmaze(
        &self,
        imdb_digits: Option<&str>,
        tvdb_id: Option<i32>,
        ct: &CancellationToken,
    ) -> Option<String> {
        let url = if let Some(imdb) = imdb_digits.filter(|s| !s.is_empty()) {
            format!("{}/lookup/shows?imdb=tt{imdb}", self.cfg.tvmaze_base)
        } else {
            let tvdb = tvdb_id?;
            format!("{}/lookup/shows?thetvdb={tvdb}", self.cfg.tvmaze_base)
        };
        let root = fetch_json(&url, None, LOOKUP_TIMEOUT, 1 << 20, ct).await?;
        parse_tvmaze_title(&root)
    }

    async fn try_wikidata(
        &self,
        imdb_digits: &str,
        ct: &CancellationToken,
    ) -> Option<ResolvedTitleMetadata> {
        let query = format!(
            "SELECT ?label ?date WHERE {{ ?item wdt:P345 \"tt{imdb_digits}\" . ?item rdfs:label ?label . FILTER(LANG(?label) = \"en\") OPTIONAL {{ ?item wdt:P577 ?date }} }}"
        );
        let url = format!("{}?query={}", self.cfg.wikidata_base, escape(&query));
        let root = fetch_json(
            &url,
            Some("application/sparql-results+json"),
            LOOKUP_TIMEOUT,
            1 << 20,
            ct,
        )
        .await?;
        parse_wikidata_metadata(&root)
    }
}

// --- ExternalIdResolver + AnimeListMappingResolver ---

/// v1 `ExternalIdResolver.IdMapping`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMapping {
    pub is_movie: bool,
    pub tvdb_id: Option<i32>,
    pub imdb_id: Option<String>,
    pub season: i32,
    pub title: Option<String>,
}

/// v1 `AnimeMapping`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnimeMapping {
    pub tvdb_id: Option<i32>,
    pub imdb_id: Option<String>,
    pub is_movie: bool,
    pub tvdb_season: Option<i32>,
}

/// Ported from `backend/Services/AnimeListMappingResolver.cs`: downloads the community
/// anime-lists dataset once, indexes it by kitsu/mal/anilist id, serves stale data while
/// refreshing in the background (24h refresh, 5min retry throttle, 8s cold-start wait).
pub struct AnimeListResolver {
    url: String,
    index: RwLock<Index>,
    load_gate: Semaphore,
}

#[derive(Debug, Default)]
struct Index {
    by_kitsu: HashMap<i64, AnimeMapping>,
    by_mal: HashMap<i64, AnimeMapping>,
    by_anilist: HashMap<i64, AnimeMapping>,
    loaded_at: Option<Timestamp>,
    last_attempt: Option<Timestamp>,
}

impl Index {
    fn has_data(&self) -> bool {
        self.loaded_at.is_some()
    }

    fn is_stale(&self) -> bool {
        self.loaded_at
            .is_none_or(|loaded| Timestamp::now() - loaded > REFRESH_INTERVAL)
    }

    fn is_due_for_attempt(&self) -> bool {
        self.is_stale()
            && self
                .last_attempt
                .is_none_or(|a| Timestamp::now() - a > RETRY_INTERVAL)
    }

    fn get(&self, provider: &str, id: i64) -> Option<AnimeMapping> {
        let dict = match provider {
            "kitsu" => &self.by_kitsu,
            "mal" => &self.by_mal,
            "anilist" => &self.by_anilist,
            _ => return None,
        };
        dict.get(&id).cloned()
    }
}

impl AnimeListResolver {
    pub fn new(url: String) -> Self {
        Self {
            url,
            index: RwLock::new(Index::default()),
            load_gate: Semaphore::new(1),
        }
    }

    pub async fn lookup(
        self: Arc<Self>,
        provider: &str,
        external_id: i64,
        _ct: &CancellationToken,
    ) -> Option<AnimeMapping> {
        if self.index.read().await.is_stale() {
            if self.index.read().await.has_data() {
                // Serve the (stale) data we already have and refresh in the background.
                let this = Arc::clone(&self);
                tokio::spawn(async move {
                    this.load_if_due(&CancellationToken::new()).await;
                });
            } else {
                // Cold start: wait briefly for the first load, then let it finish in the background.
                let load_ct = CancellationToken::new();
                let load = self.load_if_due(&load_ct);
                let _ = tokio::time::timeout(FIRST_LOAD_WAIT, load).await;
            }
        }
        self.index.read().await.get(provider, external_id)
    }

    async fn load_if_due(&self, ct: &CancellationToken) {
        if !self.index.read().await.is_due_for_attempt() {
            return;
        }
        let Ok(_permit) = self.load_gate.acquire().await else {
            return;
        };
        if !self.index.read().await.is_due_for_attempt() {
            return;
        }
        let attempt_at = Timestamp::now();
        if let Some(elements) = fetch_json_stream(&self.url, DATASET_TIMEOUT, ct).await {
            let (by_kitsu, by_mal, by_anilist) = index_dataset(&elements);
            tracing::info!(
                "Anime-list mapping loaded ({} kitsu / {} mal / {} anilist ids)",
                by_kitsu.len(),
                by_mal.len(),
                by_anilist.len()
            );
            let new_index = Index {
                by_kitsu,
                by_mal,
                by_anilist,
                loaded_at: Some(Timestamp::now()),
                last_attempt: Some(attempt_at),
            };
            *self.index.write().await = new_index;
        } else {
            // Keep any previously-loaded data; throttle retries via last_attempt.
            tracing::warn!("Anime-list mapping load failed");
            let mut index = self.index.write().await;
            index.last_attempt = Some(attempt_at);
        }
    }
}

fn get_long(el: &Value, name: &str) -> Option<i64> {
    let p = el.get(name)?;
    match p {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn get_positive_int(el: &Value, name: &str) -> Option<i32> {
    let p = el.get(name)?;
    let v = match p {
        Value::Number(n) => n.as_i64()?,
        Value::String(s) => s.parse::<i64>().ok()?,
        _ => return None,
    };
    i32::try_from(v).ok().filter(|v| *v > 0)
}

fn get_tvdb_season(el: &Value) -> Option<i32> {
    // season 0 is valid (tvdb specials), so allow >= 0.
    let season = el.get("season")?.get("tvdb")?;
    let v = match season {
        Value::Number(n) => n.as_i64()?,
        _ => return None,
    };
    i32::try_from(v).ok().filter(|v| *v >= 0)
}

fn get_imdb(el: &Value, name: &str) -> Option<String> {
    let raw = el.get(name)?.as_str()?;
    if raw.trim().is_empty() {
        return None;
    }
    // The dataset occasionally stores several comma-separated ids; take the first valid one.
    let first = raw.split(',').map(str::trim).find(|p| !p.is_empty())?;
    first
        .get(..2)
        .is_some_and(|p| p.eq_ignore_ascii_case("tt"))
        .then(|| first.to_owned())
}

fn index_dataset(
    elements: &[Value],
) -> (
    HashMap<i64, AnimeMapping>,
    HashMap<i64, AnimeMapping>,
    HashMap<i64, AnimeMapping>,
) {
    let mut by_kitsu = HashMap::new();
    let mut by_mal = HashMap::new();
    let mut by_anilist = HashMap::new();
    for el in elements {
        if !el.is_object() {
            continue;
        }
        let tvdb = get_positive_int(el, "tvdb_id");
        let imdb = get_imdb(el, "imdb_id");
        if tvdb.is_none() && imdb.is_none() {
            continue;
        }
        let is_movie = el
            .get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.eq_ignore_ascii_case("MOVIE"));
        let mapping = AnimeMapping {
            tvdb_id: tvdb,
            imdb_id: imdb,
            is_movie,
            tvdb_season: get_tvdb_season(el),
        };
        if let Some(k) = get_long(el, "kitsu_id") {
            by_kitsu.insert(k, mapping.clone());
        }
        if let Some(m) = get_long(el, "mal_id") {
            by_mal.insert(m, mapping.clone());
        }
        if let Some(a) = get_long(el, "anilist_id") {
            by_anilist.insert(a, mapping);
        }
    }
    (by_kitsu, by_mal, by_anilist)
}

/// Ported from `backend/Services/ExternalIdResolver.cs`: kitsu/mal/anilist id →
/// cross-ids via Kitsu's API, falling back to the anime-lists dataset when Kitsu's own
/// mapping table yields no tvdb/imdb id.
pub struct ExternalIdResolver {
    cfg: ResolverConfig,
    anime_lists: Arc<AnimeListResolver>,
    cache: Cache<IdMapping>,
}

impl ExternalIdResolver {
    pub fn new(cfg: ResolverConfig, anime_lists: Arc<AnimeListResolver>) -> Self {
        Self {
            cfg,
            anime_lists,
            cache: Cache::new(),
        }
    }

    pub async fn resolve(
        &self,
        provider: &str,
        external_id: i64,
        ct: &CancellationToken,
    ) -> Option<IdMapping> {
        let key = format!("{provider}:{external_id}");
        if let Some(cached) = self.cache.get(&key).await {
            return cached;
        }
        let mut mapping = if self.cfg.enabled {
            let source_id = match provider {
                "kitsu" => Some(external_id),
                "mal" => {
                    self.lookup_source_id("myanimelist/anime", external_id, ct)
                        .await
                }
                "anilist" => {
                    self.lookup_source_id("anilist/anime", external_id, ct)
                        .await
                }
                _ => None,
            };
            match source_id {
                Some(s) => self.fetch_mapping(s, ct).await,
                None => None,
            }
        } else {
            None
        };

        // Kitsu's own mapping table is incomplete; when it yields no cross-id, consult the
        // community anime-lists dataset.
        if self.cfg.enabled
            && (mapping.is_none()
                || mapping
                    .as_ref()
                    .is_none_or(|m| m.tvdb_id.is_none() && m.imdb_id.is_none()))
        {
            let anime = Arc::clone(&self.anime_lists)
                .lookup(provider, external_id, ct)
                .await;
            if let Some(anime) = anime
                && (anime.tvdb_id.is_some() || anime.imdb_id.is_some())
            {
                mapping = Some(IdMapping {
                    is_movie: mapping.as_ref().map_or(anime.is_movie, |m| m.is_movie),
                    tvdb_id: anime.tvdb_id,
                    imdb_id: anime.imdb_id,
                    // The dataset's season pairs with its tvdb id, so prefer it over Kitsu's default.
                    season: anime
                        .tvdb_season
                        .unwrap_or_else(|| mapping.as_ref().map_or(1, |m| m.season)),
                    title: mapping.as_ref().and_then(|m| m.title.clone()),
                });
            }
        }

        self.cache.put(&key, mapping.clone()).await;
        mapping
    }

    async fn lookup_source_id(
        &self,
        external_site: &str,
        external_id: i64,
        ct: &CancellationToken,
    ) -> Option<i64> {
        let url = format!(
            "{}/mappings?filter%5BexternalSite%5D={}&filter%5BexternalId%5D={external_id}&include=item&fields%5Banime%5D=id&page%5Blimit%5D=1",
            self.cfg.kitsu_base,
            escape(external_site)
        );
        let root = fetch_json(&url, None, EXTERNAL_TIMEOUT, 1 << 20, ct).await?;
        let included = root.get("included")?.as_array()?;
        for item in included {
            if item.get("type").and_then(|t| t.as_str()) == Some("anime")
                && let Some(id) = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok())
            {
                return Some(id);
            }
        }
        None
    }

    async fn fetch_mapping(&self, source_id: i64, ct: &CancellationToken) -> Option<IdMapping> {
        let url = format!(
            "{}/anime/{source_id}?include=mappings&fields%5Banime%5D=subtype,canonicalTitle,titles,mappings&fields%5Bmappings%5D=externalSite,externalId",
            self.cfg.kitsu_base
        );
        let root = fetch_json(&url, None, EXTERNAL_TIMEOUT, 1 << 20, ct).await?;
        parse_kitsu_anime(&root)
    }
}

fn parse_kitsu_anime(root: &Value) -> Option<IdMapping> {
    let mut is_movie = false;
    let mut title: Option<String> = None;
    if let Some(attrs) = root.get("data").and_then(|d| d.get("attributes")) {
        if let Some(subtype) = attrs.get("subtype").and_then(|s| s.as_str()) {
            is_movie = subtype.eq_ignore_ascii_case("movie");
        }
        title = read_kitsu_title(attrs);
    }
    let mut tvdb_id: Option<i32> = None;
    let mut imdb_id: Option<String> = None;
    let mut season = 1;
    if let Some(included) = root.get("included").and_then(|i| i.as_array()) {
        for item in included {
            if item.get("type").and_then(|t| t.as_str()) != Some("mappings") {
                continue;
            }
            let Some(m_attrs) = item.get("attributes") else {
                continue;
            };
            let site = m_attrs.get("externalSite").and_then(|s| s.as_str());
            let ext = m_attrs.get("externalId").and_then(|s| s.as_str());
            let (Some(site), Some(ext)) = (site, ext) else {
                continue;
            };
            if site == "imdb" && imdb_id.is_none() {
                imdb_id = Some(if ext.starts_with("tt") {
                    ext.to_owned()
                } else {
                    format!("tt{ext}")
                });
            } else if site.eq_ignore_ascii_case("thetvdb") {
                // ext is "<seriesId>" or "<seriesId>/<season>" — canonical season source.
                if let Some(slash) = ext.find('/') {
                    if slash > 0
                        && let Ok(id) = ext[..slash].parse::<i32>()
                    {
                        tvdb_id = tvdb_id.or(Some(id));
                    }
                    if let Ok(s) = ext[slash + 1..].parse::<i32>() {
                        season = s;
                    }
                } else if let Ok(id) = ext.parse::<i32>() {
                    tvdb_id = tvdb_id.or(Some(id));
                }
            } else if site.eq_ignore_ascii_case("thetvdb/series")
                && let Ok(id) = ext.parse::<i32>()
            {
                // Fallback id source; never carries season.
                tvdb_id = tvdb_id.or(Some(id));
            }
            // "thetvdb/season" is ignored — its ext is the upstream row id, not a season number.
        }
    }
    if tvdb_id.is_none()
        && imdb_id.is_none()
        && title.as_deref().is_none_or(|t| t.trim().is_empty())
    {
        return None;
    }
    Some(IdMapping {
        is_movie,
        tvdb_id,
        imdb_id,
        season,
        title,
    })
}

fn read_kitsu_title(attrs: &Value) -> Option<String> {
    // canonicalTitle is usually the romaji title, which matches scene/usenet naming best.
    if let Some(c) = attrs
        .get("canonicalTitle")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    {
        return Some(c.to_owned());
    }
    if let Some(titles) = attrs.get("titles").and_then(|t| t.as_object()) {
        for key in ["en", "en_jp", "ja_jp"] {
            if let Some(v) = titles
                .get(key)
                .and_then(|t| t.as_str())
                .filter(|v| !v.is_empty())
            {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// All external-ID resolvers, grouped (v1 DI graph).
pub struct Resolvers {
    pub tvdb: TvdbIdResolver,
    pub tmdb: TmdbIdResolver,
    pub title: ImdbTitleResolver,
    pub external: ExternalIdResolver,
}

impl Resolvers {
    #[must_use]
    pub fn new(cfg: ResolverConfig) -> Self {
        let anime_lists = Arc::new(AnimeListResolver::new(cfg.anime_lists_url.clone()));
        Self {
            tvdb: TvdbIdResolver::new(cfg.clone()),
            tmdb: TmdbIdResolver::new(cfg.clone()),
            title: ImdbTitleResolver::new(cfg.clone()),
            external: ExternalIdResolver::new(cfg, anime_lists),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Ported from tests/NzbWebDAV.Tests/Services/ImdbTitleResolverTests.cs.

    #[test]
    fn parse_tvmaze_title_reads_name() {
        let root = json!({"name":"The Bear","premiered":"2022-06-23"});
        assert_eq!(parse_tvmaze_title(&root).as_deref(), Some("The Bear"));
    }

    #[test]
    fn parse_wikidata_metadata_reads_title_and_single_date() {
        let root = json!({"results":{"bindings":[{"label":{"value":"Dune"},"date":{"value":"2021-10-22T00:00:00Z"}}]}});
        let m = parse_wikidata_metadata(&root).expect("metadata");
        assert_eq!(m.title, "Dune");
        assert_eq!(m.year, Some(2021));
    }

    #[test]
    fn parse_wikidata_metadata_multiple_dates_chooses_earliest_valid_year() {
        let root = json!({"results":{"bindings":[
            {"label":{"value":"Blade Runner 2049"},"date":{"value":"2017-10-06T00:00:00Z"}},
            {"label":{"value":"Blade Runner 2049"},"date":{"value":"2018-03-01T00:00:00Z"}},
            {"year":{"value":"2016"}}
        ]}});
        let m = parse_wikidata_metadata(&root).expect("metadata");
        assert_eq!(m.title, "Blade Runner 2049");
        assert_eq!(m.year, Some(2016));
    }

    #[test]
    fn parse_wikidata_metadata_title_without_date_returns_null_year() {
        let root = json!({"results":{"bindings":[{"label":{"value":"Mystery Film"}}]}});
        let m = parse_wikidata_metadata(&root).expect("metadata");
        assert_eq!(m.title, "Mystery Film");
        assert_eq!(m.year, None);
    }

    #[test]
    fn tvmaze_thetvdb_reads_externals() {
        let root = json!({"externals":{"thetvdb": 81189}});
        assert_eq!(tvmaze_thetvdb(&root), Some(81189));
        assert_eq!(tvmaze_thetvdb(&json!({"externals":{}})), None);
    }

    #[test]
    fn names_equal_normalizes_separators_and_case() {
        // v1 NormalizeName maps whitespace and - : . ' _ to a space; other punctuation
        // (e.g. '&') is dropped, not treated as a separator.
        assert!(names_equal(Some("Some-Show"), Some("some show")));
        assert!(names_equal(Some("Law: Order"), Some("law order")));
        assert!(!names_equal(Some("Law & Order"), Some("law and order")));
        assert!(!names_equal(Some("Law & Order"), Some("Law & Order SVU")));
        assert!(!names_equal(None, Some("x")));
    }

    #[test]
    fn kitsu_anime_parsing_extracts_ids_season_and_title() {
        let root = json!({
            "data": {"attributes": {"subtype": "TV", "canonicalTitle": "Some Anime", "titles": {"en": "Some Anime EN"}}},
            "included": [
                {"type": "mappings", "attributes": {"externalSite": "imdb", "externalId": "1234567"}},
                {"type": "mappings", "attributes": {"externalSite": "thetvdb", "externalId": "456/2"}},
                {"type": "mappings", "attributes": {"externalSite": "thetvdb/season", "externalId": "999"}}
            ]
        });
        let m = parse_kitsu_anime(&root).expect("mapping");
        assert!(!m.is_movie);
        assert_eq!(m.imdb_id.as_deref(), Some("tt1234567"));
        assert_eq!(m.tvdb_id, Some(456));
        assert_eq!(m.season, 2);
        assert_eq!(m.title.as_deref(), Some("Some Anime"));
    }

    #[test]
    fn kitsu_movie_subtype_and_title_fallback() {
        let root = json!({
            "data": {"attributes": {"subtype": "movie", "titles": {"en_jp": "Eiga"}}},
            "included": []
        });
        let m = parse_kitsu_anime(&root).expect("mapping");
        assert!(m.is_movie);
        assert_eq!(m.title.as_deref(), Some("Eiga"));
        assert_eq!(m.season, 1);
    }

    #[test]
    fn kitsu_mapping_without_any_id_or_title_is_none() {
        let root = json!({"data": {"attributes": {}}, "included": []});
        assert!(parse_kitsu_anime(&root).is_none());
    }

    #[test]
    fn dataset_indexing_extracts_ids_and_season() {
        let elements = vec![
            json!({"tvdb_id": 123, "imdb_id": "tt0123456,tt999", "type": "TV", "kitsu_id": 7, "mal_id": "8", "anilist_id": 9, "season": {"tvdb": 0}}),
            json!({"imdb_id": "tt0000001", "type": "MOVIE", "mal_id": 8}),
            json!({"tvdb_id": "not-a-number"}),
        ];
        let (by_kitsu, by_mal, by_anilist) = index_dataset(&elements);
        let m = by_kitsu.get(&7).expect("kitsu");
        assert_eq!(m.tvdb_id, Some(123));
        assert_eq!(m.imdb_id.as_deref(), Some("tt0123456"));
        assert!(!m.is_movie);
        assert_eq!(m.tvdb_season, Some(0));
        assert_eq!(by_mal.get(&8).expect("mal").tvdb_id, None);
        assert!(by_mal.get(&8).expect("mal").is_movie);
        assert_eq!(by_anilist.get(&9).expect("anilist").tvdb_id, Some(123));
        assert_eq!(by_kitsu.len() + by_mal.len() + by_anilist.len(), 3);
    }

    #[tokio::test]
    async fn disabled_resolvers_do_no_io() {
        let cfg = ResolverConfig {
            enabled: false,
            ..Default::default()
        };
        let resolvers = Resolvers::new(cfg);
        let ct = CancellationToken::new();
        assert_eq!(resolvers.tvdb.get_tvdb_id("1234567", &ct).await, None);
        assert_eq!(resolvers.tmdb.get_imdb_id("movie", "550", &ct).await, None);
        assert!(
            resolvers
                .title
                .get_title("movie", Some("1234567"), None, &ct)
                .await
                .is_none()
        );
        assert!(resolvers.external.resolve("kitsu", 1, &ct).await.is_none());
    }
}
