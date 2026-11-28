//! Profile-scoped search execution, ported from `backend/Services/SearchProfileService.cs`
//! (v1 fork): per-indexer paged Newznab queries (100/page, max 50 pages), per-indexer hit
//! limits + rate limiting, exclude patterns, dedupe by `NzbUrl`, quality/audio ordering,
//! strict identity matching, fallback text queries and IMDB/external-ID query building.
//!
//! Watchtower candidate warming, Warden dead-hiding and preflight start are hook-driven;
//! the corresponding hooks (`warmer`/`is_dead`/`on_preflight`) are optional and wired by
//! the app (Phase 3).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use jiff::Timestamp;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use infinidysk_indexer::models::{ConnectionDetails, IndexerConfig};
use infinidysk_indexer::{NewznabClient, filter};
use infinidysk_search::audio;
use infinidysk_search::profile::{FallbackMode, Profile, ProfileConfig};
use infinidysk_search::result::SearchResult;
use infinidysk_search::sorter::{self, QualitySortMode};

use crate::cache::{Candidate, ResolutionCache};
use crate::exclude::{self, ParsedPattern};
use crate::filename;
use crate::hits::{HitTracker, HitType};
use crate::identity::{self, ExpectedIdentity};
use crate::ratelimit::RateLimiter;
use crate::resolvers::Resolvers;

/// Newznab caps a single request at 100 results (v1 `SearchPageSize`).
const SEARCH_PAGE_SIZE: usize = 100;
/// Hard runaway guard for paging (v1 `MaxSearchPages`).
const MAX_SEARCH_PAGES: usize = 50;

/// Snapshot of the config the search service reads, replaced atomically on config change
/// (v1 reads `ConfigManager` per request).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSettings {
    pub profiles: ProfileConfig,
    pub indexers: IndexerConfig,
    /// Raw multi-line exclude patterns (`search.exclude-patterns`, falling back to
    /// `play.exclude-patterns` in v1 — the app resolves the fallback).
    pub exclude_patterns_raw: String,
    /// v1 `GetSearchUserAgent()` default `nzbdav/{version}`.
    pub search_user_agent: String,
    /// v1 `GetUserAgent()` default `SABnzbd/5.1.0`.
    pub user_agent: String,
    /// v1 `IsWatchtowerEnabled()` — gates the `warmer` candidate-warming hook.
    pub watchtower_enabled: bool,
    /// v1 `IsWardenHideDeadEnabled()` — gates the `is_dead` filter hook.
    pub warden_hide_dead: bool,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            profiles: ProfileConfig::default().normalized(),
            indexers: IndexerConfig::default(),
            exclude_patterns_raw: String::new(),
            search_user_agent: format!("nzbdav/{}", env!("CARGO_PKG_VERSION")),
            user_agent: "SABnzbd/5.1.0".to_owned(),
            watchtower_enabled: false,
            warden_hide_dead: false,
        }
    }
}

/// v1 `SearchProfileService.IndexerHit`.
#[derive(Debug, Clone)]
pub struct IndexerHit {
    pub indexer_name: String,
    pub indexer_user_agent: String,
    pub indexer_proxy_url: Option<String>,
    pub item: SearchResult,
}

/// v1 `SearchProfileService.SearchResult`.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub profile_token: String,
    pub r#type: String,
    pub id: String,
    pub candidates: Vec<Candidate>,
    pub play_tokens: Vec<String>,
}

/// Warden hook (Phase 3): fingerprint inputs (size, poster, `usenet_date`) → dead anywhere?
pub type IsDeadHook = Arc<dyn Fn(i64, Option<&str>, Option<Timestamp>) -> bool + Send + Sync>;
/// Preflight hook (Phase 3): called with the candidate list when a search starts one.
pub type PreflightHook = Arc<dyn Fn(&str, &str, &str, &[Candidate]) + Send + Sync>;
/// Watchtower hook (Phase 3): wanted-item key (`{type}:{id}` or `season:{imdb}:{season}`) →
/// the item's verified-available shortlist picks as warm candidates (v1 reads
/// `WantedItems.Shortlist` pointers with verdict `available`).
pub type WarmerHook = Arc<
    dyn for<'a> Fn(&'a str, &'a CancellationToken) -> BoxFuture<'a, Vec<Candidate>> + Send + Sync,
>;

pub struct SearchService {
    settings: RwLock<SearchSettings>,
    exclude: RwLock<Vec<ParsedPattern>>,
    rate_limiter: RateLimiter,
    hit_tracker: HitTracker,
    cache: ResolutionCache,
    resolvers: Resolvers,
    is_dead: std::sync::RwLock<Option<IsDeadHook>>,
    on_preflight: std::sync::RwLock<Option<PreflightHook>>,
    warmer: std::sync::RwLock<Option<WarmerHook>>,
}

impl SearchService {
    #[must_use]
    pub fn new(settings: SearchSettings, resolvers: Resolvers) -> Self {
        let exclude = exclude::compile_lines(&settings.exclude_patterns_raw);
        Self {
            settings: RwLock::new(settings),
            exclude: RwLock::new(exclude),
            rate_limiter: RateLimiter::new(),
            hit_tracker: HitTracker::new(),
            cache: ResolutionCache::new(),
            resolvers,
            is_dead: std::sync::RwLock::new(None),
            on_preflight: std::sync::RwLock::new(None),
            warmer: std::sync::RwLock::new(None),
        }
    }

    #[must_use]
    pub fn with_hooks(
        self,
        is_dead: Option<IsDeadHook>,
        on_preflight: Option<PreflightHook>,
    ) -> Self {
        self.install_hooks(is_dead, on_preflight);
        self
    }

    /// Installs the Phase-3 hooks after construction (the app builds the orchestrator around
    /// this service, so hooks cannot be passed to the constructor).
    pub fn install_hooks(&self, is_dead: Option<IsDeadHook>, on_preflight: Option<PreflightHook>) {
        if let Ok(mut g) = self.is_dead.write() {
            *g = is_dead;
        }
        if let Ok(mut g) = self.on_preflight.write() {
            *g = on_preflight;
        }
    }

    /// Installs the watchtower warming hook (the app wires it once the DB-backed
    /// watchtower store exists).
    pub fn install_warmer(&self, warmer: Option<WarmerHook>) {
        if let Ok(mut g) = self.warmer.write() {
            *g = warmer;
        }
    }

    /// Replaces the config snapshot (and recompiles the exclude patterns) when it changed.
    pub async fn apply_settings(&self, settings: SearchSettings) {
        if *self.settings.read().await != settings {
            self.update_settings(settings).await;
        }
    }

    /// Replaces the config snapshot and recompiles the exclude patterns.
    pub async fn update_settings(&self, settings: SearchSettings) {
        let exclude = exclude::compile_lines(&settings.exclude_patterns_raw);
        *self.exclude.write().await = exclude;
        *self.settings.write().await = settings;
    }

    #[must_use]
    pub fn hit_tracker(&self) -> &HitTracker {
        &self.hit_tracker
    }

    #[must_use]
    pub fn rate_limiter(&self) -> &RateLimiter {
        &self.rate_limiter
    }

    #[must_use]
    pub fn cache(&self) -> &ResolutionCache {
        &self.cache
    }

    #[must_use]
    pub fn resolvers(&self) -> &Resolvers {
        &self.resolvers
    }

    pub async fn get_profile(&self, token: &str) -> Option<Profile> {
        self.settings
            .read()
            .await
            .profiles
            .find_by_token(token)
            .cloned()
    }

    /// v1 `IsAdapterEnabled`: no/empty adapter list means all adapters are enabled.
    pub async fn is_adapter_enabled(&self, profile_token: &str, adapter: &str) -> bool {
        let Some(profile) = self.get_profile(profile_token).await else {
            return false;
        };
        profile.enabled_adapters.as_ref().is_none_or(Vec::is_empty)
            || profile
                .enabled_adapters
                .as_ref()
                .is_some_and(|list| list.iter().any(|a| a.eq_ignore_ascii_case(adapter)))
    }

    /// v1 `HasResolveHeadroomAsync`: true when at least one of the profile's indexers has
    /// both search and download hit budget left.
    pub async fn has_resolve_headroom(&self, profile_token: &str, _ct: &CancellationToken) -> bool {
        let Some(profile) = self.get_profile(profile_token).await else {
            return false;
        };
        let indexer_cfg = self.settings.read().await.indexers.clone();
        let indexers = selected_indexers(&indexer_cfg, &profile);
        if indexers.is_empty() {
            return false;
        }
        let now = Timestamp::now();
        for x in &indexers {
            let search = self
                .hit_tracker
                .check(
                    &x.name,
                    HitType::Search,
                    opt_i64(x.hit_limit),
                    opt_i32(x.hit_limit_reset_time),
                    now,
                )
                .await;
            if search.is_some_and(|c| !c.allowed) {
                continue;
            }
            let download = self
                .hit_tracker
                .check(
                    &x.name,
                    HitType::Download,
                    opt_i64(x.download_limit),
                    opt_i32(x.hit_limit_reset_time),
                    now,
                )
                .await;
            if download.is_some_and(|c| !c.allowed) {
                continue;
            }
            return true;
        }
        false
    }

    /// v1 `BuildImdbQueryAsync`.
    pub async fn build_imdb_query(
        &self,
        r#type: &str,
        id: &str,
        ct: &CancellationToken,
    ) -> Option<Vec<(String, String)>> {
        if is_tvdb_id(id) {
            return build_tvdb_query(r#type, id);
        }
        let rewritten;
        let id = if is_tmdb_id(id) {
            rewritten = self.rewrite_tmdb_to_imdb(r#type, id, ct).await?;
            rewritten.as_str()
        } else {
            id
        };
        let external_provider = external_provider(id);

        if r#type == "movie" {
            if let Some(provider) = external_provider {
                return self.build_external_movie_query(provider, id, ct).await;
            }
            let imdb = strip_imdb_prefix(id)?;
            return Some(vec![
                ("t".into(), "movie".into()),
                ("imdbid".into(), imdb),
                ("cat".into(), "2000".into()),
            ]);
        }
        if r#type == "season" {
            let parts: Vec<&str> = id.split(':').collect();
            if parts.len() < 2 {
                return None;
            }
            let season_imdb = strip_imdb_prefix(parts[0])?;
            let season_num: i32 = parts[1].parse().ok()?;
            let mut dict = vec![
                ("t".into(), "tvsearch".into()),
                ("season".into(), season_num.to_string()),
                ("cat".into(), "5000".into()),
            ];
            let tvdb = self.resolvers.tvdb.get_tvdb_id(&season_imdb, ct).await;
            if let Some(tvdb) = tvdb {
                dict.push(("tvdbid".into(), tvdb.to_string()));
            } else {
                dict.push(("imdbid".into(), season_imdb));
            }
            return Some(dict);
        }
        if r#type == "series" {
            if let Some(provider) = external_provider {
                return self.build_external_series_query(provider, id, ct).await;
            }
            let parts: Vec<&str> = id.split(':').collect();
            if parts.len() < 3 {
                return None;
            }
            let imdb = strip_imdb_prefix(parts[0])?;
            let season: i32 = parts[1].parse().ok()?;
            let episode: i32 = parts[2].parse().ok()?;
            let mut dict = vec![
                ("t".into(), "tvsearch".into()),
                ("season".into(), season.to_string()),
                ("ep".into(), episode.to_string()),
                ("cat".into(), "5000".into()),
            ];
            let tvdb = self.resolvers.tvdb.get_tvdb_id(&imdb, ct).await;
            if let Some(tvdb) = tvdb {
                dict.push(("tvdbid".into(), tvdb.to_string()));
            } else {
                dict.push(("imdbid".into(), imdb));
            }
            return Some(dict);
        }
        None
    }

    async fn build_external_movie_query(
        &self,
        provider: &str,
        id: &str,
        ct: &CancellationToken,
    ) -> Option<Vec<(String, String)>> {
        let parts: Vec<&str> = id.split(':').collect();
        if parts.len() < 2 {
            return None;
        }
        let external_id: i64 = parts[1].parse().ok()?;
        let mapping = self
            .resolvers
            .external
            .resolve(provider, external_id, ct)
            .await?;
        let imdb = mapping.imdb_id.as_deref().and_then(strip_imdb_prefix);
        if let Some(imdb) = imdb {
            return Some(vec![
                ("t".into(), "movie".into()),
                ("imdbid".into(), imdb),
                ("cat".into(), "2000,2070".into()),
            ]);
        }
        // Last resort: no cross-id anywhere, search by the anime's title.
        if let Some(title) = mapping.title.filter(|t| !t.trim().is_empty()) {
            return Some(vec![
                ("t".into(), "movie".into()),
                ("q".into(), title),
                ("cat".into(), "2000,2070".into()),
            ]);
        }
        None
    }

    async fn build_external_series_query(
        &self,
        provider: &str,
        id: &str,
        ct: &CancellationToken,
    ) -> Option<Vec<(String, String)>> {
        let parts: Vec<&str> = id.split(':').collect();
        if parts.len() < 2 {
            return None;
        }
        let external_id: i64 = parts[1].parse().ok()?;
        let mapping = self
            .resolvers
            .external
            .resolve(provider, external_id, ct)
            .await?;

        // <provider>:ID, <provider>:ID:EP, <provider>:ID:SEASON:EP
        let mut season = mapping.season;
        let mut episode: Option<i32> = None;
        if parts.len() == 3
            && let Ok(ep) = parts[2].parse::<i32>()
        {
            episode = Some(ep);
        } else if parts.len() >= 4
            && let (Ok(s), Ok(ep)) = (parts[2].parse::<i32>(), parts[3].parse::<i32>())
        {
            if s > 0 {
                season = s;
            }
            episode = Some(ep);
        }

        // Films sometimes arrive on the series endpoint — treat as movies regardless of any episode component.
        if mapping.is_movie {
            let imdb = mapping.imdb_id.as_deref().and_then(strip_imdb_prefix);
            if let Some(imdb) = imdb {
                return Some(vec![
                    ("t".into(), "movie".into()),
                    ("imdbid".into(), imdb),
                    ("cat".into(), "2000,2070".into()),
                ]);
            }
            if let Some(title) = mapping.title.filter(|t| !t.trim().is_empty()) {
                return Some(vec![
                    ("t".into(), "movie".into()),
                    ("q".into(), title),
                    ("cat".into(), "2000,2070".into()),
                ]);
            }
            return None;
        }

        let episode = episode?;
        let mut dict = vec![
            ("t".into(), "tvsearch".into()),
            ("season".into(), season.to_string()),
            ("ep".into(), episode.to_string()),
            ("cat".into(), "5000,5070".into()),
        ];
        if let Some(tvdb) = mapping.tvdb_id {
            dict.push(("tvdbid".into(), tvdb.to_string()));
        } else if let Some(imdb) = mapping.imdb_id.as_deref().and_then(strip_imdb_prefix) {
            dict.push(("imdbid".into(), imdb));
        } else {
            let title = mapping.title.filter(|t| !t.trim().is_empty())?;
            dict.push(("q".into(), title));
        }
        Some(dict)
    }

    async fn rewrite_tmdb_to_imdb(
        &self,
        r#type: &str,
        id: &str,
        ct: &CancellationToken,
    ) -> Option<String> {
        let parts: Vec<&str> = id[4..].trim_start_matches(['-', ':']).split(':').collect();
        if parts.first().is_none_or(|p| p.trim().is_empty()) {
            return None;
        }
        let imdb = self
            .resolvers
            .tmdb
            .get_imdb_id(r#type, parts[0], ct)
            .await?;
        if r#type == "movie" {
            return Some(imdb);
        }
        if r#type == "series" {
            if parts.len() < 3 {
                return None;
            }
            return Some(format!("{imdb}:{}:{}", parts[1], parts[2]));
        }
        None
    }

    /// v1 `SearchByImdbAsync`.
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the v1 SearchByImdbAsync signature"
    )]
    pub async fn search_by_imdb(
        &self,
        profile_token: &str,
        r#type: &str,
        id: &str,
        ct: &CancellationToken,
        client_query: Option<&str>,
        verify_identity: bool,
        start_preflight: bool,
    ) -> Option<SearchOutcome> {
        // v1: a null query map yields an empty (not missing) result.
        let Some(query_params) = self.build_imdb_query(r#type, id, ct).await else {
            return Some(empty_outcome(profile_token, r#type, id));
        };
        self.search(
            profile_token,
            r#type,
            id,
            &query_params,
            ct,
            client_query,
            verify_identity,
            start_preflight,
        )
        .await
    }

    /// v1 `SearchAsync`.
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the v1 SearchAsync signature"
    )]
    pub async fn search(
        &self,
        profile_token: &str,
        r#type: &str,
        id: &str,
        query_params: &[(String, String)],
        ct: &CancellationToken,
        client_query: Option<&str>,
        verify_identity: bool,
        start_preflight: bool,
    ) -> Option<SearchOutcome> {
        let (profile, indexer_cfg, watchtower_enabled, warden_hide_dead) = {
            let s = self.settings.read().await;
            (
                s.profiles.find_by_token(profile_token).cloned(),
                s.indexers.clone(),
                s.watchtower_enabled,
                s.warden_hide_dead,
            )
        };
        let profile = profile?;
        let indexers = selected_indexers(&indexer_cfg, &profile);
        let global_proxy = indexer_cfg.proxy_url.clone();
        if indexers.is_empty() {
            return Some(empty_outcome(profile_token, r#type, id));
        }

        let now = Timestamp::now();
        let exclude = self.exclude.read().await.clone();
        let any_prefer_downloaded = indexers.iter().any(|x| {
            x.filter
                .as_ref()
                .is_some_and(|f| f.enabled && f.prefer_downloaded)
        });
        let preferred_audio =
            audio::normalize_preferred(profile.preferred_audio_languages.as_deref());

        let per_indexer = self
            .run_per_indexer_query(
                &indexers,
                query_params,
                &indexer_cfg,
                global_proxy.as_deref(),
                now,
                ct,
            )
            .await;

        let mut deduped = dedupe_and_sort(
            &per_indexer,
            &exclude,
            profile.quality_sort,
            any_prefer_downloaded,
            &preferred_audio,
        );

        let strict_indexers: HashSet<String> = indexers
            .iter()
            .filter(|x| x.enable_strict_matching)
            .map(|x| x.name.clone())
            .collect();
        let strict_identity = if strict_indexers.is_empty() {
            ExpectedIdentity::default()
        } else {
            self.build_expected_identity(r#type, query_params, ct).await
        };
        deduped = apply_strict_matching_logged(
            &deduped,
            &strict_indexers,
            &strict_identity,
            r#type == "movie",
            r#type,
            id,
        );

        let fallback_mode = if r#type == "movie" {
            profile.movie_fallback
        } else {
            profile.tv_fallback
        };
        let fallback_threshold = (if r#type == "movie" {
            profile.movie_fallback_min_results
        } else {
            profile.tv_fallback_min_results
        })
        .max(1) as usize;
        let req_season = param(query_params, "season").and_then(|s| s.parse::<i32>().ok());
        let req_episode = param(query_params, "ep").and_then(|s| s.parse::<i32>().ok());
        let episode_ok = |h: &IndexerHit| {
            r#type != "series"
                || (req_season.is_none() && req_episode.is_none())
                || filename::episode_compatible(Some(&h.item.title), req_season, req_episode)
        };

        if fallback_mode != FallbackMode::Off && deduped.len() < fallback_threshold {
            let variants = self
                .build_fallback_queries(r#type, query_params, client_query, fallback_mode, ct)
                .await;
            if !variants.is_empty() {
                tracing::info!(
                    "Profile {} {}/{}: only {} result(s) (< {fallback_threshold}); running {fallback_mode:?} text-query fallback ({} variant(s))",
                    profile.name,
                    r#type,
                    id,
                    deduped.len(),
                    variants.len()
                );
                let mut combined: Vec<IndexerHit> = per_indexer.iter().flatten().cloned().collect();
                // Escalate through variants (targeted -> broad), stopping as soon as we clear
                // the threshold so a broad title-only query only runs when needed.
                for variant in &variants {
                    let variant_per_indexer = self
                        .run_per_indexer_query(
                            &indexers,
                            variant.as_slice(),
                            &indexer_cfg,
                            global_proxy.as_deref(),
                            now,
                            ct,
                        )
                        .await;
                    combined.extend(variant_per_indexer.into_iter().flatten());
                    let distinct = combined
                        .iter()
                        .filter(|h| !h.item.nzb_url.trim().is_empty())
                        .filter(|h| !exclude::matches_any(Some(&h.item.title), &exclude))
                        .filter(|h| episode_ok(h))
                        .map(|h| h.item.nzb_url.clone())
                        .collect::<HashSet<_>>()
                        .len();
                    if distinct >= fallback_threshold {
                        break;
                    }
                }
                let grouped = combined
                    .into_iter()
                    .filter(|h| !h.item.nzb_url.trim().is_empty())
                    .filter(|h| !exclude::matches_any(Some(&h.item.title), &exclude))
                    .filter(|h| episode_ok(h))
                    .fold(HashMap::<String, IndexerHit>::new(), |mut acc, h| {
                        acc.entry(h.item.nzb_url.clone()).or_insert(h);
                        acc
                    })
                    .into_values()
                    .collect::<Vec<_>>();
                deduped = sort_hits(
                    &grouped,
                    profile.quality_sort,
                    any_prefer_downloaded,
                    &preferred_audio,
                );
                deduped = apply_strict_matching_logged(
                    &deduped,
                    &strict_indexers,
                    &strict_identity,
                    r#type == "movie",
                    r#type,
                    id,
                );
            }
        }

        if verify_identity && (r#type == "series" || r#type == "season") {
            // tvdbid-anchored queries are already identity-scoped by the indexer, so only
            // guard queries that are not anchored to a tvdbid.
            if param(query_params, "tvdbid").is_some() {
                tracing::info!("Identity guard {type}/{id}: skipped, query is tvdbid-anchored");
            } else {
                let expected = self.build_expected_titles(r#type, query_params, ct).await;
                if expected.is_empty() {
                    tracing::debug!(
                        "Identity guard {type}/{id}: no canonical title resolved; not filtering"
                    );
                } else {
                    let before = deduped.len();
                    deduped = deduped
                        .into_iter()
                        .filter(|h| episode_ok(h))
                        .filter(|h| filename::title_matches(&expected, Some(&h.item.title)))
                        .collect();
                    if deduped.len() != before {
                        tracing::info!(
                            "Identity guard {type}/{id}: kept {}/{before} result(s)",
                            deduped.len()
                        );
                    }
                }
            }
        }

        let mut candidates: Vec<Candidate> = deduped
            .iter()
            .map(|h| Candidate {
                indexer_name: h.indexer_name.clone(),
                indexer_user_agent: h.indexer_user_agent.clone(),
                nzb_url: h.item.nzb_url.clone(),
                title: h.item.title.clone(),
                size: h.item.size,
                posted: h.item.posted,
                usenet_date: h.item.usenet_date,
                poster: h.item.poster.clone(),
                grabs: h.item.grabs,
                password: h.item.password,
                proxy_url: h.indexer_proxy_url.clone(),
                source_indexer_name: h.item.source_indexer_name.clone(),
                language: h.item.language.clone(),
                subs: h.item.subs.clone(),
                info_hash: h.item.info_hash.clone(),
                verified_available: false,
            })
            .collect();

        // Clone the hook out of the lock so no guard is held across the awaits below.
        let warmer = if watchtower_enabled {
            self.warmer.read().ok().and_then(|g| g.clone())
        } else {
            None
        };
        if let Some(warmer) = warmer {
            add_warmed_exact(&mut candidates, r#type, id, &warmer, ct).await;
            add_warmed_season_bundle(&mut candidates, r#type, id, &warmer, ct).await;
        }

        if candidates.is_empty() {
            return Some(empty_outcome(profile_token, r#type, id));
        }

        if warden_hide_dead
            && let Ok(g) = self.is_dead.read()
            && let Some(is_dead) = g.as_ref()
        {
            let alive: Vec<Candidate> = candidates
                .iter()
                .filter(|c| !is_dead(c.size, c.poster.as_deref(), c.usenet_date))
                .cloned()
                .collect();
            if !alive.is_empty() {
                candidates = alive;
            }
        }

        let play_tokens = self
            .cache
            .add_group(candidates.clone(), r#type, profile_token, id, now);
        if start_preflight
            && let Ok(g) = self.on_preflight.read()
            && let Some(on_preflight) = g.as_ref()
        {
            on_preflight(profile_token, r#type, id, &candidates);
        }

        Some(SearchOutcome {
            profile_token: profile_token.to_owned(),
            r#type: r#type.to_owned(),
            id: id.to_owned(),
            candidates,
            play_tokens,
        })
    }

    /// v1 `RunPerIndexerQueryAsync`: one paged query per indexer, concurrently; per-indexer
    /// failures (transport/timeout/cancel/protocol) degrade to an empty result, never fail
    /// the whole search.
    async fn run_per_indexer_query(
        &self,
        indexers: &[ConnectionDetails],
        query_params: &[(String, String)],
        indexer_config: &IndexerConfig,
        global_proxy: Option<&str>,
        now: Timestamp,
        ct: &CancellationToken,
    ) -> Vec<Vec<IndexerHit>> {
        let settings = self.settings.read().await;
        let search_ua_default = settings.search_user_agent.clone();
        let retrieve_ua_default = settings.user_agent.clone();
        drop(settings);
        let futures = indexers.iter().map(|x| {
            self.run_one_indexer(
                x,
                query_params,
                indexer_config,
                global_proxy,
                &search_ua_default,
                &retrieve_ua_default,
                now,
                ct,
            )
        });
        futures_util::future::join_all(futures).await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "per-indexer query context, mirroring v1 RunPerIndexerQueryAsync's closure captures"
    )]
    async fn run_one_indexer(
        &self,
        x: &ConnectionDetails,
        query_params: &[(String, String)],
        indexer_config: &IndexerConfig,
        global_proxy: Option<&str>,
        search_ua_default: &str,
        retrieve_ua_default: &str,
        now: Timestamp,
        ct: &CancellationToken,
    ) -> Vec<IndexerHit> {
        let check = self
            .hit_tracker
            .check(
                &x.name,
                HitType::Search,
                opt_i64(x.hit_limit),
                opt_i32(x.hit_limit_reset_time),
                Timestamp::now(),
            )
            .await;
        if let Some(c) = &check
            && !c.allowed
        {
            tracing::info!(
                "Indexer {} skipped: {}",
                x.name,
                HitTracker::format_skip_reason(c, HitType::Search, Timestamp::now())
            );
            return Vec::new();
        }

        let search_ua = IndexerConfig::per_indexer_search_user_agent(x)
            .unwrap_or_else(|| search_ua_default.to_owned());
        let retrieve_ua = IndexerConfig::per_indexer_retrieve_user_agent(x)
            .unwrap_or_else(|| retrieve_ua_default.to_owned());
        let proxy = if x.proxy_url.as_deref().is_none_or(|p| p.trim().is_empty()) {
            global_proxy.map(str::to_owned)
        } else {
            x.proxy_url.clone()
        };
        let timeout = Duration::from_secs(indexer_config.effective_timeout_seconds(x));
        let target = indexer_config.effective_search_result_limit(x) as usize;
        let max_bytes = indexer_config.effective_max_response_bytes(x);
        let client = match NewznabClient::with_options(
            &x.url,
            &x.api_key,
            max_bytes,
            &search_ua,
            timeout,
            x.skip_tls_verification,
        ) {
            Ok(c) => c.with_proxy(proxy.clone()),
            Err(e) => {
                tracing::warn!("Indexer {} search failed: {e}", x.name);
                return Vec::new();
            }
        };
        let base_query = apply_indexer_category_overrides(query_params, x);

        // Page through the indexer (offset += 100) until we've gathered `target` results,
        // the indexer runs dry (a short page), or we hit the safety cap.
        let mut collected: Vec<SearchResult> = Vec::new();
        let max_pages = target.div_ceil(SEARCH_PAGE_SIZE);
        let max_pages = max_pages.min(MAX_SEARCH_PAGES);
        for page in 0..max_pages {
            // Re-check the hit budget before each extra page so paging never overruns a
            // configured API-hit limit (the first page was already cleared above).
            if page > 0 {
                let page_check = self
                    .hit_tracker
                    .check(
                        &x.name,
                        HitType::Search,
                        opt_i64(x.hit_limit),
                        opt_i32(x.hit_limit_reset_time),
                        Timestamp::now(),
                    )
                    .await;
                if page_check.is_some_and(|c| !c.allowed) {
                    break;
                }
            }
            if self
                .rate_limiter
                .wait(&x.name, x.max_requests_per_minute, ct)
                .await
                .is_err()
            {
                return Vec::new();
            }
            let mut page_query = base_query.clone();
            set_param(&mut page_query, "limit", &SEARCH_PAGE_SIZE.to_string());
            set_param(
                &mut page_query,
                "offset",
                &(page * SEARCH_PAGE_SIZE).to_string(),
            );
            let pairs: Vec<(&str, &str)> = page_query
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let page_items = match client.query(&pairs, ct).await {
                Ok(items) => items,
                Err(e) => {
                    if !matches!(e, infinidysk_indexer::Error::Cancelled) {
                        tracing::warn!("Indexer {} search failed: {e}", x.name);
                    }
                    return Vec::new();
                }
            };
            self.hit_tracker
                .record(&x.name, HitType::Search, Timestamp::now())
                .await;
            let page_len = page_items.len();
            collected.extend(page_items);
            if page_len < SEARCH_PAGE_SIZE {
                break; // indexer has no more results
            }
            if collected.len() >= target {
                break; // gathered enough
            }
        }

        let limited: Vec<SearchResult> = collected.into_iter().take(target).collect();
        let filtered = filter::apply(&limited, x.filter.as_ref(), now);
        filtered
            .into_iter()
            .map(|item| IndexerHit {
                indexer_name: x.name.clone(),
                indexer_user_agent: retrieve_ua.clone(),
                indexer_proxy_url: proxy.clone(),
                item,
            })
            .collect()
    }

    /// v1 `BuildFallbackQueriesAsync`.
    async fn build_fallback_queries(
        &self,
        r#type: &str,
        original_params: &[(String, String)],
        client_query: Option<&str>,
        mode: FallbackMode,
        ct: &CancellationToken,
    ) -> Vec<Vec<(String, String)>> {
        let imdb = param(original_params, "imdbid");
        let tvdb_id = param(original_params, "tvdbid").and_then(|s| s.parse::<i32>().ok());
        let title = match client_query {
            Some(q) if !q.trim().is_empty() => Some(q.trim().to_owned()),
            _ => {
                self.resolvers
                    .title
                    .get_title(r#type, imdb, tvdb_id, ct)
                    .await
            }
        };
        let Some(title) = title else {
            return Vec::new();
        };
        if title.trim().is_empty() {
            return Vec::new();
        }

        if r#type == "movie" {
            return vec![vec![
                ("t".into(), "movie".into()),
                ("q".into(), title),
                ("cat".into(), "2000".into()),
            ]];
        }
        if r#type == "series" {
            let mut variants = Vec::new();
            // Targeted: title + structured season/ep.
            let mut targeted = vec![
                ("t".into(), "tvsearch".into()),
                ("q".into(), title.clone()),
                ("cat".into(), "5000".into()),
            ];
            if let Some(s) = param(original_params, "season") {
                targeted.push(("season".into(), s.to_owned()));
            }
            if let Some(e) = param(original_params, "ep") {
                targeted.push(("ep".into(), e.to_owned()));
            }
            let has_se = targeted.iter().any(|(k, _)| k == "season" || k == "ep");
            variants.push(targeted);
            // Broad: title only — catches flat/special releases without episode metadata.
            if mode == FallbackMode::Broad && has_se {
                variants.push(vec![
                    ("t".into(), "tvsearch".into()),
                    ("q".into(), title),
                    ("cat".into(), "5000".into()),
                ]);
            }
            return variants;
        }
        Vec::new()
    }

    async fn build_expected_titles(
        &self,
        r#type: &str,
        query_params: &[(String, String)],
        ct: &CancellationToken,
    ) -> HashSet<String> {
        self.build_expected_identity(r#type, query_params, ct)
            .await
            .normalized_titles
    }

    async fn build_expected_identity(
        &self,
        r#type: &str,
        query_params: &[(String, String)],
        ct: &CancellationToken,
    ) -> ExpectedIdentity {
        let imdb = param(query_params, "imdbid");
        let tvdb_id = param(query_params, "tvdbid").and_then(|s| s.parse::<i32>().ok());
        if imdb.is_none() && tvdb_id.is_none() {
            return ExpectedIdentity::default();
        }
        let title_type = if r#type == "movie" { "movie" } else { "series" };
        let metadata = self
            .resolvers
            .title
            .get_metadata(title_type, imdb, tvdb_id, ct)
            .await;
        let mut set = HashSet::new();
        let norm = filename::normalize_title(metadata.as_ref().map(|m| m.title.as_str()));
        if !norm.is_empty() {
            set.insert(norm);
        }
        let year = if r#type == "movie" {
            metadata.and_then(|m| m.year)
        } else {
            None
        };
        ExpectedIdentity::new(set, year)
    }
}

fn selected_indexers(indexer_config: &IndexerConfig, profile: &Profile) -> Vec<ConnectionDetails> {
    let all: Vec<&ConnectionDetails> = indexer_config
        .indexers
        .iter()
        .filter(|x| x.enabled)
        .collect();
    if profile.indexer_names.is_empty() {
        all.into_iter().cloned().collect()
    } else {
        all.into_iter()
            .filter(|x| profile.indexer_names.contains(&x.name))
            .cloned()
            .collect()
    }
}

/// v1 `AddWarmedExactAsync`: verified-available picks from the `{type}:{id}` wanted item are
/// flagged in place (or injected) and boosted to the top of the candidate list.
async fn add_warmed_exact(
    candidates: &mut Vec<Candidate>,
    r#type: &str,
    id: &str,
    warmer: &WarmerHook,
    ct: &CancellationToken,
) {
    if r#type != "series" && r#type != "movie" {
        return;
    }
    let verified = warmer(&format!("{type}:{id}"), ct).await;
    let verified: Vec<Candidate> = verified
        .into_iter()
        .filter(|c| !c.nzb_url.trim().is_empty())
        .collect();
    if verified.is_empty() {
        return;
    }
    for p in &verified {
        if let Some(existing) = candidates.iter_mut().find(|c| c.nzb_url == p.nzb_url) {
            existing.verified_available = true;
        }
    }
    let mut ordered = Vec::with_capacity(candidates.len() + verified.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut boosted = 0_usize;
    for mut p in verified {
        if !seen.insert(p.nzb_url.clone()) {
            continue;
        }
        boosted += 1;
        match candidates.iter().position(|c| c.nzb_url == p.nzb_url) {
            Some(pos) => ordered.push(candidates.remove(pos)),
            None => {
                p.verified_available = true;
                ordered.push(p);
            }
        }
    }
    for c in std::mem::take(candidates) {
        if seen.insert(c.nzb_url.clone()) {
            ordered.push(c);
        }
    }
    tracing::debug!("watchtower: boosted {boosted} verified pick(s) to top for {type}/{id}");
    *candidates = ordered;
}

/// v1 `AddWarmedSeasonBundleAsync`: verified-available picks from the `season:{imdb}:{season}`
/// wanted item are flagged in place or appended (no reordering).
async fn add_warmed_season_bundle(
    candidates: &mut Vec<Candidate>,
    r#type: &str,
    id: &str,
    warmer: &WarmerHook,
    ct: &CancellationToken,
) {
    if r#type != "series" {
        return;
    }
    let parts: Vec<&str> = id.split(':').collect();
    if parts.len() < 3 {
        return;
    }
    let Ok(season) = parts[parts.len() - 2].parse::<i32>() else {
        return;
    };
    let imdb = parts[0];
    if !imdb.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("tt")) {
        return;
    }
    let verified = warmer(&format!("season:{imdb}:{season}"), ct).await;
    for mut p in verified {
        if p.nzb_url.trim().is_empty() {
            continue;
        }
        if let Some(existing) = candidates.iter_mut().find(|c| c.nzb_url == p.nzb_url) {
            existing.verified_available = true;
        } else {
            p.verified_available = true;
            candidates.push(p);
        }
    }
}

fn empty_outcome(profile_token: &str, r#type: &str, id: &str) -> SearchOutcome {
    SearchOutcome {
        profile_token: profile_token.to_owned(),
        r#type: r#type.to_owned(),
        id: id.to_owned(),
        candidates: Vec::new(),
        play_tokens: Vec::new(),
    }
}

fn opt_i64(v: Option<u32>) -> Option<i64> {
    v.map(i64::from)
}

fn opt_i32(v: Option<u32>) -> Option<i32> {
    v.and_then(|x| i32::try_from(x).ok())
}

/// Case-insensitive param lookup (v1 dictionaries for these keys are `OrdinalIgnoreCase`).
fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn set_param(params: &mut Vec<(String, String)>, key: &str, value: &str) {
    if let Some((_, v)) = params.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
        value.clone_into(v);
    } else {
        params.push((key.to_owned(), value.to_owned()));
    }
}

fn remove_param(params: &mut Vec<(String, String)>, key: &str) {
    params.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
}

/// v1 `SearchProfileService.ApplyIndexerCategoryOverrides`.
fn apply_indexer_category_overrides(
    query_params: &[(String, String)],
    indexer: &ConnectionDetails,
) -> Vec<(String, String)> {
    let needs_cat_change = indexer.ignore_category_filter
        || indexer
            .extra_movie_categories
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        || indexer
            .extra_tv_categories
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
    if !needs_cat_change {
        return query_params.to_vec();
    }
    let mut copy = query_params.to_vec();
    if indexer.ignore_category_filter {
        remove_param(&mut copy, "cat");
        return copy;
    }
    let t = param(&copy, "t").map(str::to_owned);
    let extras = match t.as_deref() {
        Some("movie") => indexer.extra_movie_categories.as_deref(),
        Some("tvsearch") => indexer.extra_tv_categories.as_deref(),
        _ => None,
    };
    let Some(extras) = extras.filter(|s| !s.trim().is_empty()) else {
        return copy;
    };
    let existing = param(&copy, "cat").map(str::to_owned);
    let merged = merge_category_list(existing.as_deref(), extras);
    set_param(&mut copy, "cat", &merged);
    copy
}

/// v1 `MergeCategoryList`: comma-separated union, deduped, base first.
fn merge_category_list(base_list: Option<&str>, extras: &str) -> String {
    let mut seen: HashSet<String> = HashSet::new();
    let mut result: Vec<String> = Vec::new();
    let mut add_all = |raw: Option<&str>| {
        let Some(raw) = raw else { return };
        if raw.trim().is_empty() {
            return;
        }
        for v in raw.split(',').map(str::trim).filter(|v| !v.is_empty()) {
            if seen.insert(v.to_owned()) {
                result.push(v.to_owned());
            }
        }
    };
    add_all(base_list);
    add_all(Some(extras));
    result.join(",")
}

/// v1 `DedupeAndSort`: drop blank `NzbUrl` + exclude-pattern matches, group by `NzbUrl`
/// (first wins), then sort.
fn dedupe_and_sort(
    per_indexer: &[Vec<IndexerHit>],
    exclude: &[ParsedPattern],
    quality_sort: QualitySortMode,
    any_prefer_downloaded: bool,
    preferred_audio: &[String],
) -> Vec<IndexerHit> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<IndexerHit> = Vec::new();
    for hit in per_indexer.iter().flatten() {
        if hit.item.nzb_url.trim().is_empty() {
            continue;
        }
        if exclude::matches_any(Some(&hit.item.title), exclude) {
            continue;
        }
        if seen.insert(hit.item.nzb_url.clone()) {
            out.push(hit.clone());
        }
    }
    sort_hits(&out, quality_sort, any_prefer_downloaded, preferred_audio)
}

/// v1 `SortHits` via `ProfileResultSorter.Sort`.
fn sort_hits(
    items: &[IndexerHit],
    quality_sort: QualitySortMode,
    prefer_downloaded: bool,
    preferred_audio: &[String],
) -> Vec<IndexerHit> {
    sorter::sort(
        items.iter().cloned(),
        &|h: &IndexerHit| Some(h.item.title.as_str()),
        &|h: &IndexerHit| h.item.grabs,
        &|h: &IndexerHit| h.item.size,
        &|h: &IndexerHit| h.item.posted,
        quality_sort,
        prefer_downloaded,
        Some(&|h: &IndexerHit| h.item.language.as_deref()),
        preferred_audio,
    )
}

fn apply_strict_matching_logged(
    items: &[IndexerHit],
    strict_indexers: &HashSet<String>,
    identity: &ExpectedIdentity,
    apply_movie_year: bool,
    r#type: &str,
    id: &str,
) -> Vec<IndexerHit> {
    let before = items.len();
    let filtered = identity::apply_strict_matching(
        items,
        |h| h.indexer_name.as_str(),
        |h| Some(h.item.title.as_str()),
        strict_indexers,
        identity,
        apply_movie_year,
    );
    if filtered.len() != before {
        tracing::info!(
            "Strict matching {type}/{id}: kept {}/{before} result(s)",
            filtered.len()
        );
    }
    filtered
}

fn is_tvdb_id(id: &str) -> bool {
    id.get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("tvdb-") || p.eq_ignore_ascii_case("tvdb:"))
}

fn is_tmdb_id(id: &str) -> bool {
    id.get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("tmdb:") || p.eq_ignore_ascii_case("tmdb-"))
}

fn external_provider(id: &str) -> Option<&'static str> {
    let lower = id.to_ascii_lowercase();
    if lower.starts_with("kitsu:") {
        Some("kitsu")
    } else if lower.starts_with("mal:") {
        Some("mal")
    } else if lower.starts_with("anilist:") {
        Some("anilist")
    } else {
        None
    }
}

/// v1 `BuildTvdbQuery` (series only): tvdb-:S:E → t=tvsearch&tvdbid&season&ep&cat=5000.
fn build_tvdb_query(r#type: &str, id: &str) -> Option<Vec<(String, String)>> {
    if r#type != "series" {
        return None;
    }
    let rest = id[4..].trim_start_matches(['-', ':']);
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let tvdb: i32 = parts[0].parse().ok()?;
    let season: i32 = parts[1].parse().ok()?;
    let episode: i32 = parts[2].parse().ok()?;
    Some(vec![
        ("t".into(), "tvsearch".into()),
        ("tvdbid".into(), tvdb.to_string()),
        ("season".into(), season.to_string()),
        ("ep".into(), episode.to_string()),
        ("cat".into(), "5000".into()),
    ])
}

/// v1 `StripImdbPrefix`: "tt" + digits → digits.
fn strip_imdb_prefix(id: &str) -> Option<String> {
    if !id.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("tt")) {
        return None;
    }
    let digits = &id[2..];
    if digits.chars().all(|c| c.is_ascii_digit()) {
        Some(digits.to_owned())
    } else {
        None
    }
}
