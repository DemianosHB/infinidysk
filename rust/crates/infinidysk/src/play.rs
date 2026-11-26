//! v1-fork play routes, ported from `backend/Api/Controllers/Profiles/ProfileReadController.cs`
//! and `ProfilePlayController.cs`:
//! - `GET|OPTIONS /adapters/addon/{token}/stream/{type}/{id}.json` — Stremio stream listing.
//! - `GET /adapters/addon/{token}/play/{nzbToken}.mkv` and `GET /api/search/{token}/play/{nzbToken}.mkv`
//!   — resolve the clicked candidate group: reuse an existing/in-flight download, else run the
//!   hedged pre-verify + commit loop, enqueue the winning NZB to the queue, and redirect to
//!   `/view/.ids/…?downloadKey=…&extension=…`. Every attempt is recorded as a watchdog entry;
//!   stall-aborted candidates can be negative-marked (`play.stall-negative-marking-*`).
//!
//! Deviations from v1 (documented in `PORTING.md`): the `VariantResolver` group logic is not
//! ported (the content-group key is still computed and recorded, but there is no group-member
//! reuse/cap enforcement); queue lookups match by file name only (the rust queue table has no
//! content-group column); the play coalescer is a simpler leader/follower map; the play
//! verifier shares the preflight verifier (no separate High-priority gate); the request is not
//! tied to client disconnect via `RequestAborted` but to the total budget token.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use jiff::Timestamp;
use serde::Serialize;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use infinidysk_core::JobId;
use infinidysk_db::watchdog::{NewWatchdogEntry, WatchdogOutcome};
use infinidysk_db::{Db, Item, JobStatus};
use infinidysk_profiles::cache::{Candidate, Entry};
use infinidysk_profiles::hits::HitType;
use infinidysk_profiles::preferred::{self, PreferredOrderStore};
use infinidysk_queue::nzb;
use infinidysk_queue::{Queue, Submission};
use infinidysk_search::sorter::QualitySortMode;
use infinidysk_search::{audio, language, quality};

use crate::config::{ConfigStore, Settings};
use crate::nzb_fetch::{FetchError, FetchKey, MAX_RESPONSE_BYTES};
use crate::preflight::{Orchestrator, Verdict};
use crate::search::{self, Search};

/// v1 `NzbFetchTimeout` for the play path (8 s).
const NZB_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// v1 `DefaultSegmentTimeout` for play pre-verify probes.
const SEGMENT_TIMEOUT: Duration = Duration::from_secs(15);
/// v1 `PollInterval` while waiting for a queue item.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// v1 `OrphanIdleThreshold` for play-driven queue items (seconds).
const ORPHAN_IDLE_SECS: i64 = 5 * 60;
/// SAB priority `Force` (v1 `QueueItem.PriorityOption.Force`).
const PRIORITY_FORCE: i16 = 2;
/// SAB post-processing `None` (v1 `QueueItem.PostProcessingOption.None`).
const POST_PROCESSING_NONE: i16 = 0;

/// Shared state for the play routes.
pub struct Play {
    pub search: Arc<Search>,
    pub config: Arc<RwLock<ConfigStore>>,
    pub queue: Arc<Queue>,
    pub db: Db,
    pub preflight: Arc<Orchestrator>,
    /// Play-owned queue items and when a click last touched them (orphan cleanup).
    last_seen: Mutex<HashMap<JobId, Timestamp>>,
    /// In-flight leader leases per content group (v1 `PlayResolutionCoalescer`, simplified).
    coalescer: Mutex<HashMap<String, Arc<CoalesceSlot>>>,
}

/// Builds the play state around the shared search state.
#[must_use]
pub fn build(
    config: Arc<RwLock<ConfigStore>>,
    search: Arc<Search>,
    queue: Arc<Queue>,
    db: Db,
    preflight: Arc<Orchestrator>,
) -> Arc<Play> {
    Arc::new(Play {
        search,
        config,
        queue,
        db,
        preflight,
        last_seen: Mutex::new(HashMap::new()),
        coalescer: Mutex::new(HashMap::new()),
    })
}

pub fn router(state: Arc<Play>) -> Router {
    Router::new()
        // axum allows no static suffix on a parameter segment, so the `.json` suffix is
        // matched via a wildcard and checked in the handler (v1 route: `{id}.json`).
        .route(
            "/adapters/addon/{token}/stream/{type}/{*file}",
            get(stream).options(stream_options),
        )
        // axum allows no static suffix on a parameter segment, so the `.mkv` suffix is
        // matched via a wildcard and checked in the handler (v1 route: `{nzbToken}.mkv`).
        .route("/adapters/addon/{token}/play/{*file}", get(play))
        .route("/api/search/{token}/play/{*file}", get(play))
        .with_state(state)
}

// --- GET|OPTIONS /adapters/addon/{token}/stream/{type}/{id}.json (v1 ProfileReadController) ---

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamResponse {
    streams: Vec<StreamItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamItem {
    name: String,
    description: String,
    title: String,
    url: String,
    behavior_hints: StreamBehaviorHints,
    meta: StreamMeta,
    failover_id: String,
    extra: StreamExtra,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamBehaviorHints {
    filename: String,
    video_size: i64,
    binge_group: String,
    not_web_ready: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamMeta {
    indexer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    in_library: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    availability: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    languages: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle_languages: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StreamExtra {
    failover_id: String,
}

async fn stream_options() -> Response {
    search::cors_headers(StatusCode::NO_CONTENT.into_response())
}

async fn stream(
    State(state): State<Arc<Play>>,
    Path((token, r#type, file)): Path<(String, String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let Some(id) = file.strip_suffix(".json") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let id = id.to_owned();
    state.search.sync_settings().await;
    if state.search.service.get_profile(&token).await.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !state
        .search
        .service
        .is_adapter_enabled(&token, "addon")
        .await
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ct = CancellationToken::new();
    let Some(result) = state
        .search
        .service
        .search_by_imdb(&token, &r#type, &id, &ct, None, false, true)
        .await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let settings = state.config.read().await.settings().clone();
    let broken = state
        .preflight
        .negative
        .snapshot_broken_history_items(negative_ttl(&settings));
    let names: Vec<String> = result
        .candidates
        .iter()
        .map(|c| search::to_nzb_file_name(&c.title))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let ready: HashSet<String> = state
        .db
        .ready_nzb_file_names(&names, &broken, is_video_file)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.to_lowercase())
        .collect();
    let base_url = search::public_base_url(&headers, &uri, &settings.network.public_url);
    let streams: Vec<StreamItem> = result
        .candidates
        .iter()
        .zip(result.play_tokens.iter())
        .map(|(c, play_token)| {
            create_stream(
                c,
                &result.r#type,
                &token,
                play_token,
                &base_url,
                &ready,
                &state,
            )
        })
        .collect();
    search::cors_headers(Json(StreamResponse { streams }).into_response())
}

/// v1 `ProfileAddonFactory.CreateStream`.
fn create_stream(
    c: &Candidate,
    r#type: &str,
    token: &str,
    play_token: &str,
    base_url: &str,
    ready: &HashSet<String>,
    state: &Play,
) -> StreamItem {
    let ty = r#type;
    let indexer = if c
        .source_indexer_name
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        c.source_indexer_name.clone().unwrap_or_default()
    } else {
        c.indexer_name.clone()
    };
    let sorted = |value: Option<&str>| {
        let mut v: Vec<String> = language::parse_languages(value).into_iter().collect();
        v.sort();
        v
    };
    let audio_languages = sorted(c.language.as_deref());
    let subtitle_languages = sorted(c.subs.as_deref());
    let in_library = ready.contains(&search::to_nzb_file_name(&c.title).to_lowercase());
    let verified_available = c.verified_available
        || state
            .preflight
            .cache
            .get(&c.nzb_url)
            .is_some_and(|h| h.verdict == Verdict::Available);
    let description = build_description(
        c,
        &indexer,
        in_library,
        verified_available,
        &audio_languages,
        &subtitle_languages,
    );
    StreamItem {
        name: format!("[NZB] {indexer}"),
        title: description.clone(),
        description,
        url: format!(
            "{}/adapters/addon/{token}/play/{play_token}.mkv",
            base_url.trim_end_matches('/')
        ),
        behavior_hints: StreamBehaviorHints {
            filename: search::sanitize_file_name(&c.title),
            video_size: c.size.max(0),
            binge_group: format!("nzbdav|{indexer}|{ty}"),
            not_web_ready: true,
        },
        meta: StreamMeta {
            indexer,
            in_library: in_library.then_some(true),
            availability: verified_available.then_some("available".to_owned()),
            languages: (!audio_languages.is_empty()).then_some(audio_languages),
            subtitle_languages: (!subtitle_languages.is_empty()).then_some(subtitle_languages),
        },
        failover_id: play_token.to_owned(),
        extra: StreamExtra {
            failover_id: play_token.to_owned(),
        },
    }
}

/// v1 `ProfileAddonFactory.BuildDescription`.
fn build_description(
    c: &Candidate,
    indexer: &str,
    in_library: bool,
    verified_available: bool,
    audio_languages: &[String],
    subtitle_languages: &[String],
) -> String {
    let mut details = vec![format!("💾 {}", format_bytes(c.size))];
    if let Some(posted) = c.posted {
        details.push(format!("📅 {}", format_age(Timestamp::now() - posted)));
    }
    let mut lines = vec![
        c.title.clone(),
        details.join(" | "),
        format!("🌐 {indexer}"),
    ];
    let mut markers = Vec::new();
    if in_library {
        markers.push("⚡ Ready".to_owned());
    }
    if verified_available {
        markers.push("✅ Verified".to_owned());
    }
    markers.extend(audio_flag_markers(audio_languages));
    if !markers.is_empty() {
        lines.push(markers.join(" "));
    }
    if !subtitle_languages.is_empty() {
        lines.push(format!("💬 Subs: {}", subtitle_languages.join(", ")));
    }
    lines.join("\n")
}

/// v1 `ProfileAddonFactory.FormatBytes`.
fn format_bytes(bytes: i64) -> String {
    if bytes <= 0 {
        return "?".to_owned();
    }
    let suffixes = ["B", "KB", "MB", "GB", "TB"];
    let mut index = 0;
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    while value >= 1024.0 && index < suffixes.len() - 1 {
        value /= 1024.0;
        index += 1;
    }
    let mut s = format!("{value:.2}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    format!("{} {}", s, suffixes[index])
}

/// v1 `ProfileAddonFactory.FormatAge` (coarse units).
fn format_age(age: jiff::SignedDuration) -> String {
    let secs = age.as_secs();
    let days = secs / 86_400;
    if days >= 365 {
        return format!("{}y", days / 365);
    }
    if days >= 1 {
        return format!("{days}d");
    }
    let hours = secs / 3_600;
    if hours >= 1 {
        return format!("{hours}h");
    }
    format!("{}m", (secs / 60).max(1))
}

/// v1 `ProfileLanguageMetadata.AudioFlags` (flag emoji per language code).
fn audio_flag_markers(codes: &[String]) -> Vec<String> {
    const FLAGS: &[(&str, &str)] = &[
        ("en", "🇬🇧"),
        ("es", "🇪🇸"),
        ("fr", "🇫🇷"),
        ("de", "🇩🇪"),
        ("it", "🇮🇹"),
        ("pt", "🇵🇹"),
        ("nl", "🇳🇱"),
        ("ru", "🇷🇺"),
        ("ja", "🇯🇵"),
        ("ko", "🇰🇷"),
        ("zh", "🇨🇳"),
        ("ar", "🇸🇦"),
        ("hi", "🇮🇳"),
        ("swe", "🇸🇪"),
        ("nor", "🇳🇴"),
        ("dan", "🇩🇰"),
        ("fin", "🇫🇮"),
        ("pol", "🇵🇱"),
        ("tur", "🇹🇷"),
    ];
    let mut out: Vec<String> = codes
        .iter()
        .filter_map(|code| {
            FLAGS
                .iter()
                .find(|(k, _)| *k == code)
                .map(|(_, f)| (*f).to_owned())
        })
        .collect();
    out.dedup();
    out
}

// --- GET /adapters/addon/{token}/play/{nzbToken}.mkv, /api/search/{token}/play/{nzbToken}.mkv ---

async fn play(
    State(state): State<Arc<Play>>,
    Path((token, file)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let Some(nzb_token) = file.strip_suffix(".mkv") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let prefix = {
        let settings = state.config.read().await.settings().clone();
        public_path_prefix(&headers, &settings.network.public_url, &uri)
    };
    let mut response = handle(&state, &token, nzb_token, &prefix).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    response
}

/// v1 `ProfilePlayController.HandleAsync`.
async fn handle(state: &Arc<Play>, token: &str, nzb_token: &str, prefix: &str) -> Response {
    state.search.sync_settings().await;
    if state.search.service.get_profile(token).await.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(entry) = state.search.service.cache().get(nzb_token) else {
        return (
            StatusCode::NOT_FOUND,
            "Link expired. Re-search in your client.",
        )
            .into_response();
    };
    if entry.profile_token != token {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Phase 3 (Watchtower) wires `TryWarmCacheAsync` here; until then the hook is a no-op.
    debug!(
        "watchtower cache warming not wired yet (Phase 3) for {}/{}",
        entry.r#type, entry.id
    );
    state
        .preflight
        .sessions
        .cancel(&entry.profile_token, &entry.r#type, &entry.id);
    let settings = state.config.read().await.settings().clone();

    if let Some(redirect) = try_resolve_existing(state, &entry, &settings, prefix).await {
        return redirect;
    }
    if let Ok(Some(id)) = state
        .db
        .in_flight_queue_by_file_names(&entry_file_names(&entry))
        .await
        && let Some(redirect) = wait_for_in_flight(state, id, &entry, &settings, prefix).await
    {
        return redirect;
    }
    resolve_and_commit(state, nzb_token, entry, &settings, prefix).await
}

fn entry_file_names(entry: &Entry) -> Vec<String> {
    entry
        .candidates
        .iter()
        .map(|c| search::to_nzb_file_name(&c.title))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn negative_ttl(s: &Settings) -> Duration {
    Duration::from_secs(u64::from(s.play.candidate_negative_cache_ttl_minutes()) * 60)
}

fn stall_ttl(s: &Settings) -> Duration {
    Duration::from_secs(u64::from(s.play.stall_negative_marking_ttl_minutes()) * 60)
}

/// v1 `VariantResolver.BuildContentGroupKey`.
fn content_group_key(r#type: &str, id: &str) -> Option<String> {
    if r#type.trim().is_empty() || id.trim().is_empty() {
        return None;
    }
    let ty = r#type;
    Some(format!("{ty}:{id}"))
}

/// v1 `TryResolveExistingAsync`: match any candidate's file name against completed history.
async fn try_resolve_existing(
    state: &Arc<Play>,
    entry: &Entry,
    settings: &Settings,
    prefix: &str,
) -> Option<Response> {
    let broken = state
        .preflight
        .negative
        .snapshot_broken_history_items(negative_ttl(settings));
    let rows = state
        .db
        .completed_history_by_file_names(&entry_file_names(entry), None, &broken)
        .await
        .ok()?;
    for row in rows {
        if let Some(redirect) =
            build_redirect_for_history(state, row.id, Some(entry), settings, prefix).await
        {
            return Some(redirect);
        }
    }
    None
}

/// v1 `WaitForInFlightAsync`.
async fn wait_for_in_flight(
    state: &Arc<Play>,
    nzo_id: JobId,
    entry: &Entry,
    settings: &Settings,
    prefix: &str,
) -> Option<Response> {
    state.touch(nzo_id);
    let deadline =
        Instant::now() + Duration::from_secs(u64::from(settings.play.total_budget_seconds()));
    while Instant::now() < deadline {
        if let Ok(Some(h)) = state.db.history_entry(nzo_id).await {
            state.untouch(&nzo_id);
            if h.status != JobStatus::Completed {
                return None;
            }
            return build_redirect_for_history(state, nzo_id, Some(entry), settings, prefix).await;
        }
        state.touch(nzo_id);
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    None
}

// --- Coalescer (v1 PlayResolutionCoalescer, simplified leader/follower) ---

struct CoalesceSlot {
    /// `None` = not published yet; `Some(None)` = published with no winner.
    #[expect(
        clippy::option_option,
        reason = "distinguishes pending from published-without-winner"
    )]
    result: Mutex<Option<Option<JobId>>>,
    done: tokio::sync::Notify,
}

struct Lease {
    key: String,
    slot: Arc<CoalesceSlot>,
    is_leader: bool,
}

impl Play {
    fn acquire(&self, key: &str) -> Lease {
        let existing = self.coalescer.lock().expect("lock").get(key).cloned();
        if let Some(slot) = existing {
            return Lease {
                key: key.to_owned(),
                slot,
                is_leader: false,
            };
        }
        let slot = Arc::new(CoalesceSlot {
            result: Mutex::new(None),
            done: tokio::sync::Notify::new(),
        });
        self.coalescer
            .lock()
            .expect("lock")
            .insert(key.to_owned(), Arc::clone(&slot));
        Lease {
            key: key.to_owned(),
            slot,
            is_leader: true,
        }
    }

    fn touch(&self, id: JobId) {
        self.last_seen
            .lock()
            .expect("lock")
            .insert(id, Timestamp::now());
    }

    fn untouch(&self, id: &JobId) {
        self.last_seen.lock().expect("lock").remove(id);
    }
}

impl Lease {
    async fn wait(&self, ct: &CancellationToken) -> Option<JobId> {
        tokio::select! {
            () = ct.cancelled() => None,
            () = self.slot.done.notified() => {
                self.slot.result.lock().expect("lock").flatten()
            }
        }
    }

    fn publish(self, state: &Play, value: Option<JobId>) {
        {
            let mut result = self.slot.result.lock().expect("lock");
            if result.is_none() {
                *result = Some(value);
            }
        }
        self.slot.done.notify_waiters();
        state.coalescer.lock().expect("lock").remove(&self.key);
    }
}

/// v1 `ResolveAndCommitAsync`.
async fn resolve_and_commit(
    state: &Arc<Play>,
    nzb_token: &str,
    entry: Entry,
    settings: &Settings,
    prefix: &str,
) -> Response {
    let Some(key) = content_group_key(&entry.r#type, &entry.id) else {
        return leader_body(state, nzb_token, &entry, settings, prefix, None).await;
    };
    let budget = Duration::from_secs(u64::from(settings.play.total_budget_seconds()));
    let lease = state.acquire(&key);
    if !lease.is_leader {
        let wait_ct = CancellationToken::new();
        let watcher = wait_ct.clone();
        tokio::spawn(async move {
            tokio::time::sleep(budget).await;
            watcher.cancel();
        });
        if let Some(id) = lease.wait(&wait_ct).await
            && let Some(redirect) = wait_for_in_flight(state, id, &entry, settings, prefix).await
        {
            return redirect;
        }
        if let Some(redirect) = try_resolve_existing(state, &entry, settings, prefix).await {
            return redirect;
        }
        if let Ok(Some(id)) = state
            .db
            .in_flight_queue_by_file_names(&entry_file_names(&entry))
            .await
            && let Some(redirect) = wait_for_in_flight(state, id, &entry, settings, prefix).await
        {
            return redirect;
        }
        // The leader published nothing useful; run the body ourselves (v1: coalesceLease = null).
        let lease = state.acquire(&key);
        let mut resolved = None;
        let response = leader_body(
            state,
            nzb_token,
            &entry,
            settings,
            prefix,
            Some(&mut resolved),
        )
        .await;
        lease.publish(state, resolved);
        return response;
    }
    let mut resolved = None;
    let response = leader_body(
        state,
        nzb_token,
        &entry,
        settings,
        prefix,
        Some(&mut resolved),
    )
    .await;
    lease.publish(state, resolved);
    response
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitReason {
    Completed,
    QueueFailed,
    EnqueueFailed,
    BudgetTimeout,
    Cancelled,
    Aborted,
}

#[derive(Debug)]
enum BatchOutcome {
    Winner(Response, Option<JobId>),
    Cancelled(Option<JobId>),
    BudgetTimeout,
    AllFailed,
}

struct PreVerifyResult {
    candidate: Candidate,
    nzb_bytes: Option<Arc<Vec<u8>>>,
    verdict: Verdict,
    responder_host: Option<String>,
    has_sidecar_subs: bool,
}

/// v1 `ResolveLeaderBodyAsync`.
async fn leader_body(
    state: &Arc<Play>,
    nzb_token: &str,
    entry: &Entry,
    settings: &Settings,
    prefix: &str,
    resolved: Option<&mut Option<JobId>>,
) -> Response {
    let play = &settings.play;
    let total_budget = Duration::from_secs(u64::from(play.total_budget_seconds()));
    let deadline = Instant::now() + total_budget;
    let total_ct = CancellationToken::new();
    {
        let watcher = total_ct.clone();
        tokio::spawn(async move {
            tokio::time::sleep(total_budget).await;
            watcher.cancel();
        });
    }
    let click_id = uuid::Uuid::new_v4().as_simple().to_string();
    let requested_title = entry.primary().title.clone();
    let content_type = entry.r#type.clone();
    let cgk = content_group_key(&entry.r#type, &entry.id);
    let mut starts_at: HashMap<String, Timestamp> = HashMap::new();
    let neg_ttl = negative_ttl(settings);
    let stall_ttl = stall_ttl(settings);
    let providers = all_providers_display(settings);

    // Watchdog OFF → simple single-candidate flow (no fallback, no pre-verify, no negative cache).
    if !play.watchdog_enabled {
        let primary = entry.primary().clone();
        starts_at.insert(primary.nzb_url.clone(), Timestamp::now());
        let Some(nzb_bytes) = fetch_nzb_bytes(state, &primary, settings, &total_ct).await else {
            record_attempt(
                state,
                &click_id,
                &primary,
                &content_type,
                &requested_title,
                0,
                WatchdogOutcome::EnqueueFailed,
                Some("Indexer NZB fetch failed".to_owned()),
                &starts_at,
                false,
                cgk.as_deref(),
                None,
                None,
            );
            return resolve_existing_or_error(
                state,
                entry,
                settings,
                prefix,
                StatusCode::BAD_GATEWAY,
                "Failed to fetch NZB from indexer.",
                10,
            )
            .await;
        };
        let single = PreVerifyResult {
            candidate: primary.clone(),
            nzb_bytes: Some(nzb_bytes),
            verdict: Verdict::Available,
            responder_host: None,
            has_sidecar_subs: false,
        };
        let (result, reason, new_nzo) = commit(
            state, nzb_token, &single, entry, settings, prefix, deadline, &total_ct,
        )
        .await;
        record_attempt(
            state,
            &click_id,
            &primary,
            &content_type,
            &requested_title,
            0,
            map_commit_reason(reason),
            commit_reason_message(reason).map(str::to_owned),
            &starts_at,
            reason == CommitReason::Completed,
            cgk.as_deref(),
            None,
            new_nzo,
        );
        if reason == CommitReason::Completed
            && let Some(r) = resolved
        {
            *r = new_nzo;
        }
        if reason == CommitReason::BudgetTimeout
            && let Some(id) = new_nzo
        {
            schedule_orphan_cleanup(state, id);
        }
        if let Some(response) = result {
            return response;
        }
        return resolve_existing_or_error(
            state,
            entry,
            settings,
            prefix,
            StatusCode::SERVICE_UNAVAILABLE,
            "Still processing. Retry the link in a few seconds.",
            5,
        )
        .await;
    }

    let preferred_order =
        state
            .search
            .preferred_order
            .get_order(&entry.profile_token, &entry.r#type, &entry.id);
    let profile = state.search.service.get_profile(&entry.profile_token).await;
    let preferred_audio = audio::normalize_preferred(
        profile
            .as_ref()
            .and_then(|p| p.preferred_audio_languages.as_deref()),
    );
    let quality_sort = profile.map(|p| p.quality_sort).unwrap_or_default();
    let fallback_queue = build_fallback_queue(
        entry,
        preferred_order.as_deref(),
        play.prefer_subtitles,
        &preferred_audio,
        quality_sort,
    );
    let mark_stalled = play.stall_negative_marking_enabled;
    let warden = &state.preflight.warden;
    let is_dead = |c: &Candidate| {
        let fp = preferred::warden_fingerprint(c.size, c.poster.as_deref(), c.usenet_date);
        warden.is_dead_cached(fp.as_deref())
    };
    // Fork (stall negative marking): advisory marks; ignored for this click when every
    // non-permanently-excluded candidate already carries a warm stall mark (fail-open).
    let ignore_stall_marks = mark_stalled
        && fallback_queue
            .iter()
            .filter(|c| !state.preflight.negative.is_failed(&c.nzb_url, neg_ttl) && !is_dead(c))
            .all(|c| state.preflight.negative.is_stalled(&c.nzb_url, stall_ttl));

    let mut rank_index: HashMap<String, i32> = HashMap::new();
    let mut display_rank: i32 = 0;
    let mut queue_index = 0usize;
    let mut attempts_used = 0usize;
    let mut saw_any_batch = false;
    let max_candidates = play.max_candidates() as usize;
    let max_attempts = play.max_attempts() as usize;

    while attempts_used < max_attempts && queue_index < fallback_queue.len() {
        if total_ct.is_cancelled() || Instant::now() >= deadline {
            break;
        }
        let batch_budget = max_candidates.min(max_attempts - attempts_used);
        let mut pool: Vec<Candidate> = Vec::new();
        while queue_index < fallback_queue.len() && pool.len() < batch_budget {
            let c = fallback_queue[queue_index].clone();
            queue_index += 1;
            let skipped_rank = display_rank;
            if state.preflight.negative.is_failed(&c.nzb_url, neg_ttl) || is_dead(&c) {
                display_rank += 1;
                rank_index.insert(c.nzb_url.clone(), skipped_rank);
                starts_at.insert(c.nzb_url.clone(), Timestamp::now());
                record_attempt(
                    state,
                    &click_id,
                    &c,
                    &content_type,
                    &requested_title,
                    skipped_rank,
                    WatchdogOutcome::PreVerifyDead,
                    Some(
                        "Previously marked dead by preflight or earlier verify — skipped"
                            .to_owned(),
                    ),
                    &starts_at,
                    false,
                    cgk.as_deref(),
                    Some(providers.clone()),
                    None,
                );
                continue;
            }
            if mark_stalled
                && !ignore_stall_marks
                && state.preflight.negative.is_stalled(&c.nzb_url, stall_ttl)
            {
                display_rank += 1;
                rank_index.insert(c.nzb_url.clone(), skipped_rank);
                starts_at.insert(c.nzb_url.clone(), Timestamp::now());
                record_attempt(
                    state, &click_id, &c, &content_type, &requested_title, skipped_rank,
                    WatchdogOutcome::PreVerifyDead,
                    Some("Recently aborted for a serving stall — skipped while its stall mark is warm".to_owned()),
                    &starts_at, false, cgk.as_deref(), Some(providers.clone()), None,
                );
                continue;
            }
            let candidate_file_name = search::to_nzb_file_name(&c.title);
            if state
                .preflight
                .negative
                .is_file_name_broken(&candidate_file_name, neg_ttl)
            {
                display_rank += 1;
                rank_index.insert(c.nzb_url.clone(), skipped_rank);
                starts_at.insert(c.nzb_url.clone(), Timestamp::now());
                record_attempt(
                    state,
                    &click_id,
                    &c,
                    &content_type,
                    &requested_title,
                    skipped_rank,
                    WatchdogOutcome::PreVerifyDead,
                    Some(
                        "Candidate proven broken by a prior mid-read failure — skipped".to_owned(),
                    ),
                    &starts_at,
                    false,
                    cgk.as_deref(),
                    Some(providers.clone()),
                    None,
                );
                continue;
            }
            rank_index.insert(c.nzb_url.clone(), display_rank);
            display_rank += 1;
            pool.push(c);
        }
        if pool.is_empty() {
            break;
        }
        saw_any_batch = true;
        attempts_used += pool.len();

        let outcome = run_batch(
            state,
            &pool,
            &rank_index,
            nzb_token,
            &content_type,
            &requested_title,
            &click_id,
            &mut starts_at,
            play.verify_mode(),
            play.verify_sample_count(),
            Duration::from_secs(u64::from(play.hedge_delay_seconds())),
            deadline,
            &total_ct,
            cgk.as_deref(),
            play.prefer_subtitles,
            mark_stalled,
            neg_ttl,
            stall_ttl,
            entry,
            settings,
            prefix,
            &providers,
        )
        .await;
        match outcome {
            BatchOutcome::Winner(response, nzo) => {
                if let Some(r) = resolved {
                    *r = nzo;
                }
                return response;
            }
            BatchOutcome::Cancelled(nzo) => {
                if let Some(r) = resolved {
                    *r = nzo;
                }
                return StatusCode::OK.into_response();
            }
            BatchOutcome::BudgetTimeout => {
                return resolve_existing_or_error(
                    state,
                    entry,
                    settings,
                    prefix,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Still processing. Retry the link in a few seconds.",
                    5,
                )
                .await;
            }
            BatchOutcome::AllFailed => {}
        }
    }

    if !saw_any_batch {
        return resolve_existing_or_error(
            state,
            entry,
            settings,
            prefix,
            StatusCode::SERVICE_UNAVAILABLE,
            "All ranked candidates recently failed; try again shortly.",
            5,
        )
        .await;
    }
    resolve_existing_or_error(
        state,
        entry,
        settings,
        prefix,
        StatusCode::SERVICE_UNAVAILABLE,
        "All tried candidates failed. Retry in a few seconds.",
        5,
    )
    .await
}

/// v1 `BuildFallbackQueue`.
pub(crate) fn build_fallback_queue(
    entry: &Entry,
    preferred_order: Option<&[String]>,
    prefer_subtitles: bool,
    preferred_audio: &[String],
    quality_sort: QualitySortMode,
) -> Vec<Candidate> {
    let primary = entry.primary().clone();
    let mut queue = vec![primary.clone()];
    let others: Vec<Candidate> = entry
        .candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != entry.start_index)
        .map(|(_, c)| c.clone())
        .collect();

    // Base order: client-reported order, else size proximity to the clicked release.
    let mut ordered = if let Some(order) = preferred_order.filter(|o| !o.is_empty()) {
        PreferredOrderStore::apply_order(
            &others,
            |c| {
                preferred::release_identity_key(
                    c.size,
                    c.poster.as_deref(),
                    c.usenet_date,
                    Some(&c.nzb_url),
                )
            },
            Some(order),
        )
    } else if primary.size <= 0 {
        let mut v = others;
        v.sort_by_key(|c| std::cmp::Reverse(c.size));
        v
    } else {
        let mut smaller: Vec<Candidate> = others
            .iter()
            .filter(|c| c.size <= primary.size)
            .cloned()
            .collect();
        smaller.sort_by_key(|c| std::cmp::Reverse(c.size));
        let mut larger: Vec<Candidate> = others
            .iter()
            .filter(|c| c.size > primary.size)
            .cloned()
            .collect();
        larger.sort_by_key(|c| c.size);
        smaller.extend(larger);
        smaller
    };

    // Subtitle bias (stable, never discards).
    if prefer_subtitles {
        let primary_languages = language::parse_languages(primary.subs.as_deref());
        let primary_has_subs = !primary_languages.is_empty();
        ordered.sort_by_key(|c| {
            std::cmp::Reverse(language::subtitle_rank(
                c.subs.as_deref(),
                &primary_languages,
                primary_has_subs,
            ))
        });
    }

    // Fork (preferred audio languages): language tier first, then quality (v1 applies
    // resolution then source whenever qualitySort != Off), both stable over the base order.
    if !preferred_audio.is_empty() || quality_sort != QualitySortMode::Off {
        ordered.sort_by(|a, b| {
            let rank_of = |c: &Candidate| {
                if preferred_audio.is_empty() {
                    0
                } else {
                    audio::rank(preferred_audio, c.language.as_deref(), Some(&c.title))
                }
            };
            rank_of(a).cmp(&rank_of(b)).then_with(|| {
                if quality_sort == QualitySortMode::Off {
                    return std::cmp::Ordering::Equal;
                }
                let qa = quality::parse(Some(&a.title));
                let qb = quality::parse(Some(&b.title));
                qb.resolution
                    .cmp(&qa.resolution)
                    .then(qb.source.cmp(&qa.source))
            })
        });
    }

    queue.extend(ordered);
    queue
}

#[expect(clippy::too_many_arguments, reason = "v1 RunBatchAsync shape")]
fn spawn_preverify(
    state: &Arc<Play>,
    handles: &mut Vec<(usize, tokio::task::JoinHandle<()>)>,
    i: usize,
    c: Candidate,
    tx: &tokio::sync::mpsc::Sender<(usize, PreVerifyResult)>,
    ct: &CancellationToken,
    verify_mode: &str,
    sample_count: u32,
    prefer_subtitles: bool,
    settings: &Settings,
) {
    let state = Arc::clone(state);
    let tx = tx.clone();
    let ct = ct.clone();
    let vm = verify_mode.to_owned();
    let settings = settings.clone();
    handles.push((
        i,
        tokio::spawn(async move {
            let r = pre_verify(
                &state,
                &c,
                &vm,
                sample_count,
                prefer_subtitles,
                &settings,
                &ct,
            )
            .await;
            let _ = tx.send((i, r)).await;
        }),
    ));
}

/// v1 `RunBatchAsync`: hedged pre-verify, then commit the best-ranked verified candidate.
#[expect(clippy::too_many_arguments, reason = "v1 RunBatchAsync shape")]
#[allow(
    clippy::too_many_lines,
    reason = "v1 RunBatchAsync is one long state machine"
)]
async fn run_batch(
    state: &Arc<Play>,
    pool: &[Candidate],
    rank_index: &HashMap<String, i32>,
    nzb_token: &str,
    content_type: &str,
    requested_title: &str,
    click_id: &str,
    starts_at: &mut HashMap<String, Timestamp>,
    verify_mode: &str,
    verify_sample_count: u32,
    hedge_delay: Duration,
    deadline: Instant,
    total_ct: &CancellationToken,
    cgk: Option<&str>,
    prefer_subtitles: bool,
    mark_stalled: bool,
    neg_ttl: Duration,
    stall_ttl: Duration,
    entry: &Entry,
    settings: &Settings,
    prefix: &str,
    providers: &str,
) -> BatchOutcome {
    let _ = stall_ttl;
    for c in pool {
        starts_at.insert(c.nzb_url.clone(), Timestamp::now());
    }
    let batch_ct = total_ct.child_token();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(usize, PreVerifyResult)>(pool.len() + 1);
    let mut handles: Vec<(usize, tokio::task::JoinHandle<()>)> = Vec::new();
    let mut pending: HashSet<usize> = (0..pool.len()).collect();
    let handled: Arc<Mutex<HashSet<usize>>> = Arc::new(Mutex::new(HashSet::new()));

    spawn_preverify(
        state,
        &mut handles,
        0,
        pool[0].clone(),
        &tx,
        &batch_ct,
        verify_mode,
        verify_sample_count,
        prefer_subtitles,
        settings,
    );

    let mut ready: BTreeMap<i32, PreVerifyResult> = BTreeMap::new();

    // Hedge: if the primary has not passed by `hedge_delay`, fire the backups.
    if pool.len() > 1 {
        tokio::select! {
            r = rx.recv() => {
                if let Some((i, res)) = r {
                    pending.remove(&i);
                    absorb(state, i, res, pool, rank_index, &mut ready, click_id, content_type,
                        requested_title, starts_at, cgk, &handled, providers);
                }
            }
            () = tokio::time::sleep(hedge_delay) => {
                for (i, c) in pool.iter().enumerate().skip(1) {
                    starts_at.insert(c.nzb_url.clone(), Timestamp::now());
                    spawn_preverify(state, &mut handles, i, c.clone(), &tx, &batch_ct,
                        verify_mode, verify_sample_count, prefer_subtitles, settings);
                }
            }
        }
    }
    drop(tx);

    while !pending.is_empty() || !ready.is_empty() {
        while let Ok((i, res)) = rx.try_recv() {
            pending.remove(&i);
            absorb(
                state,
                i,
                res,
                pool,
                rank_index,
                &mut ready,
                click_id,
                content_type,
                requested_title,
                starts_at,
                cgk,
                &handled,
                providers,
            );
        }
        if !ready.is_empty()
            && !better_ranked_in_flight(&pending, pool, rank_index, &ready, deadline)
        {
            // Among the head verdict tier, prefer a candidate that carries subtitles.
            let head_verdict = ready.values().next().map(|r| r.verdict);
            let pick = if prefer_subtitles {
                ready
                    .iter()
                    .take_while(|(_, r)| Some(r.verdict) == head_verdict)
                    .find(|(_, r)| has_subtitles(r))
                    .map_or_else(|| *ready.keys().next().expect("non-empty"), |(k, _)| *k)
            } else {
                *ready.keys().next().expect("non-empty")
            };
            let best = ready.remove(&pick).expect("present");
            let (action, reason, nzo) = commit(
                state, nzb_token, &best, entry, settings, prefix, deadline, total_ct,
            )
            .await;
            record_attempt(
                state,
                click_id,
                &best.candidate,
                content_type,
                requested_title,
                rank_index
                    .get(&best.candidate.nzb_url)
                    .copied()
                    .unwrap_or(0),
                map_commit_reason(reason),
                commit_reason_message(reason).map(str::to_owned),
                starts_at,
                reason == CommitReason::Completed,
                cgk,
                best.responder_host.clone(),
                nzo,
            );
            match reason {
                CommitReason::Completed => {
                    cancel_remaining_and_record(
                        state,
                        pool,
                        &pending,
                        &ready,
                        rank_index,
                        click_id,
                        content_type,
                        requested_title,
                        starts_at,
                        &handled,
                        handles,
                        &batch_ct,
                        "Winner found; loser cancelled to free provider connections",
                        cgk,
                    );
                    return BatchOutcome::Winner(action.expect("winner has a response"), nzo);
                }
                CommitReason::Aborted => {
                    if mark_stalled {
                        state
                            .preflight
                            .negative
                            .mark_stalled(&best.candidate.nzb_url);
                    }
                    if let Some(id) = nzo {
                        remove_aborted_queue_item(state, id).await;
                    }
                }
                CommitReason::QueueFailed | CommitReason::EnqueueFailed => {
                    state
                        .preflight
                        .negative
                        .mark_failed(&best.candidate.nzb_url);
                }
                CommitReason::Cancelled => {
                    cancel_remaining_and_record(
                        state,
                        pool,
                        &pending,
                        &ready,
                        rank_index,
                        click_id,
                        content_type,
                        requested_title,
                        starts_at,
                        &handled,
                        handles,
                        &batch_ct,
                        "Client disconnected; loser cancelled",
                        cgk,
                    );
                    return BatchOutcome::Cancelled(nzo);
                }
                CommitReason::BudgetTimeout => {
                    if let Some(id) = nzo {
                        schedule_orphan_cleanup(state, id);
                    }
                    cancel_remaining_and_record(
                        state,
                        pool,
                        &pending,
                        &ready,
                        rank_index,
                        click_id,
                        content_type,
                        requested_title,
                        starts_at,
                        &handled,
                        handles,
                        &batch_ct,
                        "Budget exhausted; loser cancelled",
                        cgk,
                    );
                    return BatchOutcome::BudgetTimeout;
                }
            }
            continue;
        }
        if pending.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            () = batch_ct.cancelled() => break,
            r = rx.recv() => match r {
                Some((i, res)) => {
                    pending.remove(&i);
                    absorb(state, i, res, pool, rank_index, &mut ready, click_id, content_type,
                        requested_title, starts_at, cgk, &handled, providers);
                }
                None => break,
            },
        }
    }

    cancel_remaining_and_record(
        state,
        pool,
        &pending,
        &ready,
        rank_index,
        click_id,
        content_type,
        requested_title,
        starts_at,
        &handled,
        handles,
        &batch_ct,
        "Batch exited without a winner; remaining verifies cancelled",
        cgk,
    );
    let _ = neg_ttl;
    BatchOutcome::AllFailed
}

/// Folds one settled pre-verify into `ready` or records a Dead outcome (v1 `RunBatchAsync`).
#[expect(clippy::too_many_arguments, reason = "v1 RunBatchAsync bookkeeping")]
fn absorb(
    state: &Arc<Play>,
    i: usize,
    res: PreVerifyResult,
    pool: &[Candidate],
    rank_index: &HashMap<String, i32>,
    ready: &mut BTreeMap<i32, PreVerifyResult>,
    click_id: &str,
    content_type: &str,
    requested_title: &str,
    starts_at: &mut HashMap<String, Timestamp>,
    cgk: Option<&str>,
    handled: &Arc<Mutex<HashSet<usize>>>,
    providers: &str,
) {
    let rank = rank_index
        .get(&pool[i].nzb_url)
        .copied()
        .unwrap_or_default();
    handled.lock().expect("lock").insert(i);
    match res.verdict {
        Verdict::Available => {
            ready.insert(rank, res);
        }
        Verdict::Dead => {
            state.preflight.negative.mark_failed(&res.candidate.nzb_url);
            let fp = preferred::warden_fingerprint(
                res.candidate.size,
                res.candidate.poster.as_deref(),
                res.candidate.usenet_date,
            );
            let warden = Arc::clone(&state.preflight.warden);
            tokio::spawn(async move {
                warden.mark_dead(fp.as_deref()).await;
            });
            record_attempt(
                state,
                click_id,
                &res.candidate,
                content_type,
                requested_title,
                rank,
                WatchdogOutcome::PreVerifyDead,
                Some("STAT/HEAD reported article missing on every provider".to_owned()),
                starts_at,
                false,
                cgk,
                Some(providers.to_owned()),
                None,
            );
        }
        Verdict::Timeout => {
            // Don't poison on timeout; try it as a last resort only when the NZB was fetched.
            if res.nzb_bytes.is_some() {
                ready.insert(rank + 10_000, res);
            }
        }
    }
}

/// v1 `BetterRankedVerifyInFlight`.
fn better_ranked_in_flight(
    pending: &HashSet<usize>,
    pool: &[Candidate],
    rank_index: &HashMap<String, i32>,
    ready: &BTreeMap<i32, PreVerifyResult>,
    deadline: Instant,
) -> bool {
    if pending.is_empty() || ready.is_empty() || Instant::now() >= deadline {
        return false;
    }
    let best_ready = *ready.keys().next().expect("non-empty");
    pending.iter().any(|i| {
        rank_index
            .get(&pool[*i].nzb_url)
            .is_some_and(|r| *r < best_ready)
    })
}

fn has_subtitles(r: &PreVerifyResult) -> bool {
    !r.candidate
        .subs
        .as_deref()
        .is_none_or(|s| s.trim().is_empty())
        || r.has_sidecar_subs
}

/// v1 `CancelRemainingAndRecord`: abort in-flight verifies and record everything as Cancelled.
#[expect(
    clippy::too_many_arguments,
    reason = "v1 CancelRemainingAndRecord shape"
)]
fn cancel_remaining_and_record(
    state: &Arc<Play>,
    pool: &[Candidate],
    pending: &HashSet<usize>,
    ready: &BTreeMap<i32, PreVerifyResult>,
    rank_index: &HashMap<String, i32>,
    click_id: &str,
    content_type: &str,
    requested_title: &str,
    starts_at: &HashMap<String, Timestamp>,
    handled: &Arc<Mutex<HashSet<usize>>>,
    handles: Vec<(usize, tokio::task::JoinHandle<()>)>,
    batch_ct: &CancellationToken,
    reason: &str,
    cgk: Option<&str>,
) {
    if !pending.is_empty() && !batch_ct.is_cancelled() {
        batch_ct.cancel();
    }
    for r in ready.values() {
        let rank = rank_index.get(&r.candidate.nzb_url).copied().unwrap_or(0);
        record_attempt(
            state,
            click_id,
            &r.candidate,
            content_type,
            requested_title,
            rank,
            WatchdogOutcome::Cancelled,
            Some(reason.to_owned()),
            starts_at,
            false,
            cgk,
            None,
            None,
        );
    }
    if pending.is_empty() {
        return;
    }
    let state = Arc::clone(state);
    let pool: Vec<Candidate> = pool.to_vec();
    let pending: Vec<usize> = pending.iter().copied().collect();
    let handled = Arc::clone(handled);
    let handles: Vec<(usize, tokio::task::JoinHandle<()>)> = handles
        .into_iter()
        .filter(|(i, _)| pending.contains(i))
        .collect();
    let click_id = click_id.to_owned();
    let content_type = content_type.to_owned();
    let requested_title = requested_title.to_owned();
    let starts_at = starts_at.clone();
    let rank_index = rank_index.clone();
    let reason = reason.to_owned();
    let cgk = cgk.map(str::to_owned);
    tokio::spawn(async move {
        for (i, handle) in handles {
            let _ = handle.await;
            if handled.lock().expect("lock").contains(&i) {
                continue;
            }
            let c = &pool[i];
            let rank = rank_index.get(&c.nzb_url).copied().unwrap_or(0);
            record_attempt(
                &state,
                &click_id,
                c,
                &content_type,
                &requested_title,
                rank,
                WatchdogOutcome::Cancelled,
                Some(reason.clone()),
                &starts_at,
                false,
                cgk.as_deref(),
                None,
                None,
            );
        }
    });
}

/// v1 `PreVerifyAsync`.
async fn pre_verify(
    state: &Arc<Play>,
    c: &Candidate,
    verify_mode: &str,
    sample_count: u32,
    prefer_subtitles: bool,
    settings: &Settings,
    ct: &CancellationToken,
) -> PreVerifyResult {
    let detect_sidecar = prefer_subtitles && c.subs.as_deref().is_none_or(|s| s.trim().is_empty());
    let fail = |verdict, bytes: Option<Arc<Vec<u8>>>| PreVerifyResult {
        candidate: c.clone(),
        nzb_bytes: bytes,
        verdict,
        responder_host: None,
        has_sidecar_subs: false,
    };
    if verify_mode != "body"
        && let Some(hit) = state.preflight.cache.get(&c.nzb_url)
        && hit.verdict == Verdict::Available
        && let Some(bytes) = hit.nzb_bytes.clone()
    {
        let sidecar = detect_sidecar && has_sidecar_subtitles(&bytes);
        return PreVerifyResult {
            candidate: c.clone(),
            nzb_bytes: Some(bytes),
            verdict: Verdict::Available,
            responder_host: hit.responder_host,
            has_sidecar_subs: sidecar,
        };
    }
    if ct.is_cancelled() {
        return fail(Verdict::Timeout, None);
    }
    let Some(bytes) = fetch_nzb_bytes(state, c, settings, ct).await else {
        return fail(Verdict::Dead, None);
    };
    let outcome = state
        .preflight
        .verifier
        .verify(&bytes, verify_mode, sample_count, SEGMENT_TIMEOUT, ct)
        .await;
    if ct.is_cancelled() {
        // A cancellation has no bytes to commit and must not be enqueued (v1 PreVerifyAsync).
        return fail(Verdict::Timeout, None);
    }
    let sidecar = detect_sidecar && has_sidecar_subtitles(&bytes);
    PreVerifyResult {
        candidate: c.clone(),
        nzb_bytes: Some(bytes),
        verdict: outcome.verdict,
        responder_host: outcome.responder_host,
        has_sidecar_subs: sidecar,
    }
}

/// v1 `HasSidecarSubtitlesAsync`: any subtitle sidecar file inside the NZB.
fn has_sidecar_subtitles(nzb_bytes: &[u8]) -> bool {
    let Ok(xml) = nzb::decompress(nzb_bytes) else {
        return false;
    };
    let Ok(doc) = nzb::parse(&xml) else {
        return false;
    };
    doc.files
        .iter()
        .any(|f| language::is_subtitle_file(Some(f.name())))
}

/// v1 `FetchNzbBytesAsync`: preflight cache, then a coalesced, rate-limited, 8 s-bounded fetch.
async fn fetch_nzb_bytes(
    state: &Arc<Play>,
    c: &Candidate,
    settings: &Settings,
    ct: &CancellationToken,
) -> Option<Arc<Vec<u8>>> {
    if let Some(hit) = state.preflight.cache.get(&c.nzb_url)
        && let Some(bytes) = hit.nzb_bytes
    {
        return Some(bytes);
    }
    if let Some(indexer) = settings
        .indexers
        .indexers
        .iter()
        .find(|x| x.name == c.indexer_name)
    {
        let now = Timestamp::now();
        let check = state
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
                now,
            )
            .await;
        if let Some(check) = &check
            && !check.allowed
        {
            debug!(indexer = %c.indexer_name, "play NZB download skipped: hit limit");
            return None;
        }
        if state
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
    let skip_tls = settings
        .indexers
        .should_skip_tls_verification(&c.indexer_name);
    let key = FetchKey {
        url: c.nzb_url.clone(),
        proxy_url: c.proxy_url.clone().unwrap_or_default(),
        skip_tls_verification: skip_tls,
    };
    let ua = c.indexer_user_agent.clone();
    let url = c.nzb_url.clone();
    let proxy = c.proxy_url.clone();
    let result = state
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
                    Err(infinidysk_indexer::Error::TooLarge { max_bytes, .. }) => {
                        debug!(url = %url, limit = max_bytes, "play NZB fetch rejected oversized response");
                        Ok(None)
                    }
                    Err(infinidysk_indexer::Error::Cancelled) => Err(FetchError::Cancelled),
                    Err(_) => Err(FetchError::Failed),
                }
            }
        })
        .await;
    match result {
        Ok(Some(bytes)) => {
            state
                .search
                .service
                .hit_tracker()
                .record(&c.indexer_name, HitType::Download, Timestamp::now())
                .await;
            Some(bytes)
        }
        _ => None,
    }
}

/// v1 `CommitAsync`: reuse an existing queue/history item, else enqueue, then poll to completion.
#[expect(clippy::too_many_arguments, reason = "v1 CommitAsync shape")]
async fn commit(
    state: &Arc<Play>,
    nzb_token: &str,
    pv: &PreVerifyResult,
    entry: &Entry,
    settings: &Settings,
    prefix: &str,
    deadline: Instant,
    ct: &CancellationToken,
) -> (Option<Response>, CommitReason, Option<JobId>) {
    let _ = nzb_token;
    let c = &pv.candidate;
    let Some(nzb_bytes) = pv.nzb_bytes.clone() else {
        return (None, CommitReason::EnqueueFailed, None);
    };
    let file_name = search::to_nzb_file_name(&c.title);
    let category = if entry.r#type.trim().is_empty() {
        settings.sab.manual_category.clone()
    } else {
        entry.r#type.clone()
    };
    let neg_ttl = negative_ttl(settings);

    let nzo: JobId;
    let mut newly: Option<JobId> = None;
    match state
        .db
        .queue_item_by_file_name(&file_name, &category)
        .await
    {
        Ok(Some(id)) => nzo = id,
        Ok(None) => {
            let broken = state
                .preflight
                .negative
                .snapshot_broken_history_items(neg_ttl);
            let completed = state
                .db
                .completed_history_by_file_names(
                    std::slice::from_ref(&file_name),
                    Some(&category),
                    &broken,
                )
                .await;
            if let Ok(rows) = completed
                && let Some(row) = rows.into_iter().next()
            {
                nzo = row.id;
            } else {
                let job = Submission {
                    file_name: &file_name,
                    category: &category,
                    priority: PRIORITY_FORCE,
                    post_processing: POST_PROCESSING_NONE,
                };
                let _permit = state.queue.db().write_permit().await;
                match state
                    .queue
                    .enqueue_owned(&job, nzb_bytes.as_ref().clone())
                    .await
                {
                    Ok(id) => {
                        nzo = id;
                        newly = Some(id);
                        state.touch(id);
                    }
                    Err(e) => {
                        debug!(url = %c.nzb_url, error = %e, "play enqueue failed");
                        return (None, CommitReason::EnqueueFailed, None);
                    }
                }
            }
        }
        Err(_) => return (None, CommitReason::EnqueueFailed, None),
    }
    if newly.is_none() {
        state.touch(nzo);
    }

    let stall_failover =
        settings.grab.stall_failover_enabled && settings.play.watchdog_enabled && newly.is_some();
    let stall_window =
        Duration::from_secs(u64::from(settings.grab.stall_failover_window_seconds()));
    let ceiling = Duration::from_secs(u64::from(settings.grab.stall_failover_ceiling_seconds()));
    let candidate_start = Instant::now();
    let mut last_progress: Option<u8> = None;
    let mut last_progress_at = candidate_start;

    while Instant::now() < deadline {
        if ct.is_cancelled() {
            return (None, CommitReason::Cancelled, newly);
        }
        if let Ok(Some(h)) = state.db.history_entry(nzo).await {
            state.untouch(&nzo);
            if h.status != JobStatus::Completed {
                debug!(url = %c.nzb_url, fail = ?h.fail_message, "play candidate failed in queue");
                return (None, CommitReason::QueueFailed, newly);
            }
            return match build_redirect_for_history(state, nzo, Some(entry), settings, prefix).await
            {
                Some(r) => (Some(r), CommitReason::Completed, newly),
                None => (None, CommitReason::QueueFailed, newly),
            };
        }
        if stall_failover {
            let progress = state
                .queue
                .progress()
                .into_iter()
                .find(|(id, _)| *id == nzo)
                .map(|(_, s)| s.percent);
            let now = Instant::now();
            match progress {
                None => last_progress_at = now,
                Some(p) if last_progress != Some(p) => {
                    last_progress = Some(p);
                    last_progress_at = now;
                }
                Some(_) => {}
            }
            if (progress.is_some() && now.duration_since(last_progress_at) >= stall_window)
                || now.duration_since(candidate_start) >= ceiling
            {
                return (None, CommitReason::Aborted, newly);
            }
        }
        state.touch(nzo);
        tokio::select! {
            () = ct.cancelled() => return (None, CommitReason::Cancelled, newly),
            () = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
    (None, CommitReason::BudgetTimeout, newly)
}

/// v1 `ResolveExistingOrErrorAsync` (without the variant fallback, which is not ported).
async fn resolve_existing_or_error(
    state: &Arc<Play>,
    entry: &Entry,
    settings: &Settings,
    prefix: &str,
    status: StatusCode,
    message: &str,
    retry_after: u32,
) -> Response {
    if let Some(redirect) = try_resolve_existing(state, entry, settings, prefix).await {
        return redirect;
    }
    let mut response = (status, message.to_owned()).into_response();
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// v1 `BuildRedirectForHistoryItemAsync`: pick the best video file of a completed download and
/// redirect to its `/view` URL.
async fn build_redirect_for_history(
    state: &Arc<Play>,
    history_id: JobId,
    entry: Option<&Entry>,
    settings: &Settings,
    prefix: &str,
) -> Option<Response> {
    let items = state.db.history_file_items(history_id).await.ok()?;
    let videos: Vec<&Item> = items.iter().filter(|i| is_video_file(&i.name)).collect();
    if videos.is_empty() {
        return None;
    }
    let (season, episode) = match entry {
        Some(e) => parse_season_episode(&e.r#type, &e.id),
        None => (None, None),
    };
    let primary_title = entry.map(|e| e.primary().title.as_str());
    let best = select_best_video(&videos, season, episode, primary_title)?;
    let ext = best
        .name
        .rsplit('.')
        .next()
        .map(str::to_lowercase)
        .unwrap_or_default();
    Some(redirect_response(best, &ext, prefix, settings))
}

/// v1 `BuildRedirectAsync` → 302 to `/view/.ids/…?downloadKey=…&extension=…`.
fn redirect_response(item: &Item, extension: &str, prefix: &str, settings: &Settings) -> Response {
    let path = item.id.ids_path();
    let strm_key = &settings.imports.strm_key.0;
    let download_key = if strm_key.is_empty() {
        settings.webdav.download_key.0.clone()
    } else {
        infinidysk_webdav::strm_key(strm_key, &path)
    };
    let url = format!("{prefix}/view/{path}?downloadKey={download_key}&extension={extension}");
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(&url) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

/// v1 `TryParseSeasonEpisode`: series ids of the form `{imdb}:{season}:{episode}`.
fn parse_season_episode(r#type: &str, id: &str) -> (Option<i32>, Option<i32>) {
    if r#type != "series" || id.is_empty() {
        return (None, None);
    }
    let parts: Vec<&str> = id.split(':').collect();
    if parts.len() < 3 {
        return (None, None);
    }
    match (
        parts[parts.len() - 2].parse::<i32>(),
        parts[parts.len() - 1].parse::<i32>(),
    ) {
        (Ok(s), Ok(e)) => (Some(s), Some(e)),
        _ => (None, None),
    }
}

// --- v1 VideoSelection ---

/// v1 `VideoSelection.IsProbablyDecoy`.
fn is_probably_decoy(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains(".unpack.") || lower.contains("rickroll")
}

/// v1 `FileFilterUtil.LooksLikeSampleName`: whole-word `sample`/`samples` with
/// non-alphanumeric (or boundary) delimiters, case-insensitive, on the file name.
fn looks_like_sample_name(file_name: &str) -> bool {
    let name = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    let lower = name.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 6 <= bytes.len() {
        if bytes[i..i + 6].eq_ignore_ascii_case(b"sample") {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after = i + 6;
            let next = if bytes.get(after) == Some(&b's') || bytes.get(after) == Some(&b'S') {
                after + 1
            } else {
                after
            };
            let end_ok = next >= bytes.len() || !bytes[next].is_ascii_alphanumeric();
            if before_ok && end_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// v1 `FileFilterUtil.IsSampleFile` (size-ratio half; callers pre-filter to video files).
fn is_sample_file(file_name: &str, file_size: u64, largest_video: u64) -> bool {
    if !looks_like_sample_name(file_name) {
        return false;
    }
    if file_size == 0 || largest_video == 0 {
        return false;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = file_size as f64 / largest_video as f64;
    ratio < 0.20
}

fn is_sample_candidate(candidate: &Item, videos: &[&Item]) -> bool {
    let largest = videos
        .iter()
        .map(|v| v.size.unwrap_or(0))
        .max()
        .unwrap_or(0);
    let size = candidate.size.unwrap_or(0);
    if largest > 0 && size > 0 {
        return is_sample_file(&candidate.name, size, largest);
    }
    looks_like_sample_name(&candidate.name)
}

/// v1 `VideoSelection.SelectBest`.
fn select_best_video<'a>(
    videos: &[&'a Item],
    season: Option<i32>,
    episode: Option<i32>,
    primary_title: Option<&str>,
) -> Option<&'a Item> {
    if videos.is_empty() {
        return None;
    }
    if let (Some(s), Some(e)) = (season, episode)
        && let Some(file) = select_episode_file(videos, s, e)
    {
        return Some(file);
    }
    let Some(title) = primary_title else {
        return pick_fallback(videos);
    };
    let click_tokens = tokenize_name(title);
    if click_tokens.is_empty() {
        return pick_fallback(videos);
    }
    let mut best: Option<&Item> = None;
    let mut best_score = 0usize;
    let mut best_size = -1i64;
    for v in videos {
        if is_sample_candidate(v, videos) || is_probably_decoy(&v.name) {
            continue;
        }
        let stem = v
            .name
            .rsplit_once('.')
            .map_or(v.name.as_str(), |(stem, _)| stem);
        let file_tokens = tokenize_name(stem);
        if file_tokens.is_empty() {
            continue;
        }
        let score = file_tokens
            .iter()
            .filter(|t| click_tokens.contains(*t))
            .count();
        #[allow(clippy::cast_possible_wrap)]
        let size = v.size.map_or(0, |s| s as i64);
        if score > best_score || (score == best_score && score > 0 && size > best_size) {
            best = Some(v);
            best_score = score;
            best_size = size;
        }
    }
    if best_score > 0 { best } else { None }
}

fn select_episode_file<'a>(videos: &[&'a Item], season: i32, episode: i32) -> Option<&'a Item> {
    let mut best: Option<&Item> = None;
    let mut best_size = -1i64;
    for v in videos {
        if is_sample_candidate(v, videos) || is_probably_decoy(&v.name) {
            continue;
        }
        let Some(tag) = infinidysk_profiles::filename::parse_episode(Some(&v.name)) else {
            continue;
        };
        if tag.season != season {
            continue;
        }
        let Some(start) = tag.episode else {
            continue;
        };
        let end = tag.episode_end.unwrap_or(start);
        if !(start..=end).contains(&episode) {
            continue;
        }
        #[allow(clippy::cast_possible_wrap)]
        let size = v.size.map_or(0, |s| s as i64);
        if size > best_size {
            best = Some(v);
            best_size = size;
        }
    }
    best
}

fn pick_fallback<'a>(videos: &[&'a Item]) -> Option<&'a Item> {
    let non_decoy: Vec<&Item> = videos
        .iter()
        .copied()
        .filter(|v| !is_probably_decoy(&v.name) && !is_sample_candidate(v, videos))
        .collect();
    let pool: &[&Item] = if non_decoy.is_empty() {
        videos
    } else {
        non_decoy.as_slice()
    };
    if pool.len() == 1 {
        return pool.first().copied();
    }
    pool.iter().max_by_key(|v| v.size.unwrap_or(0)).copied()
}

/// v1 `VideoSelection.TokenizeName`.
fn tokenize_name(s: &str) -> HashSet<String> {
    s.split(['.', '_', '-', ' ', '(', ')', '[', ']', '{', '}', '+'])
        .map(str::trim)
        .filter(|t| t.len() > 1)
        .map(str::to_lowercase)
        .collect()
}

// --- Watchdog + cleanup helpers ---

/// v1 `RecordAttempt` → `watchdog_entries` row (written in the background).
#[expect(clippy::too_many_arguments, reason = "v1 RecordAttempt shape")]
fn record_attempt(
    state: &Arc<Play>,
    click_id: &str,
    c: &Candidate,
    content_type: &str,
    requested_title: &str,
    rank: i32,
    outcome: WatchdogOutcome,
    fail_reason: Option<String>,
    starts_at: &HashMap<String, Timestamp>,
    is_winner: bool,
    cgk: Option<&str>,
    provider_host: Option<String>,
    nzo: Option<JobId>,
) {
    let attempted_at = starts_at
        .get(&c.nzb_url)
        .copied()
        .unwrap_or_else(Timestamp::now);
    let now = Timestamp::now();
    let entry = NewWatchdogEntry {
        click_id: click_id.to_owned(),
        attempted_at: attempted_at.as_millisecond(),
        content_type: content_type.to_owned(),
        requested_title: requested_title.to_owned(),
        candidate_title: c.title.clone(),
        indexer_name: c.indexer_name.clone(),
        size: c.size,
        rank_index: rank,
        result: outcome,
        fail_reason,
        duration_ms: (now.as_millisecond() - attempted_at.as_millisecond()).max(0),
        is_winner,
        provider_host,
        queue_item_id: nzo.map(|j| j.0.as_simple().to_string()),
        content_group_key: cgk.map(str::to_owned),
    };
    let db = state.db.clone();
    tokio::spawn(async move {
        if let Err(e) = db.watchdog_record(&entry).await {
            debug!(error = %e, "watchdog record failed");
        }
    });
}

/// v1 `MapCommitReason`.
fn map_commit_reason(r: CommitReason) -> WatchdogOutcome {
    match r {
        CommitReason::Completed => WatchdogOutcome::QueueCompleted,
        CommitReason::QueueFailed => WatchdogOutcome::QueueFailed,
        CommitReason::EnqueueFailed => WatchdogOutcome::EnqueueFailed,
        CommitReason::BudgetTimeout => WatchdogOutcome::BudgetTimeout,
        CommitReason::Cancelled | CommitReason::Aborted => WatchdogOutcome::Cancelled,
    }
}

/// v1 `CommitReasonToMessage`.
fn commit_reason_message(r: CommitReason) -> Option<&'static str> {
    match r {
        CommitReason::Completed => None,
        CommitReason::QueueFailed => Some("Queue processing marked the item as Failed"),
        CommitReason::EnqueueFailed => Some("Could not enqueue the NZB (DB or fetch error)"),
        CommitReason::BudgetTimeout => Some("Queue still processing when total budget elapsed"),
        CommitReason::Cancelled => Some("Client disconnected or request cancelled"),
        CommitReason::Aborted => {
            Some("Aborted: no progress within the stall window — trying the next candidate")
        }
    }
}

/// v1 `AllConfiguredProvidersDisplay`.
fn all_providers_display(settings: &Settings) -> String {
    let hosts: Vec<&str> = settings
        .providers
        .iter()
        .map(|p| p.hostname.trim())
        .filter(|h| !h.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if hosts.is_empty() {
        "—".to_owned()
    } else {
        hosts.join(", ")
    }
}

/// v1 `ScheduleOrphanCleanup`.
fn schedule_orphan_cleanup(state: &Arc<Play>, nzo: JobId) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(ORPHAN_IDLE_SECS as u64)).await;
        let last = state.last_seen.lock().expect("lock").get(&nzo).copied();
        let Some(last) = last else {
            return;
        };
        if Timestamp::now().as_second() - last.as_second() < ORPHAN_IDLE_SECS {
            schedule_orphan_cleanup(&state, nzo);
            return;
        }
        let still_queued = state
            .db
            .queue()
            .await
            .is_ok_and(|q| q.iter().any(|j| j.id == nzo));
        if still_queued {
            debug!(nzo = %nzo.0, "removing orphan play-driven queue item after idle threshold");
            let _ = state.db.remove_job(nzo).await;
        }
        state.untouch(&nzo);
    });
}

/// v1 `RemoveAbortedQueueItemAsync`.
async fn remove_aborted_queue_item(state: &Arc<Play>, nzo: JobId) {
    let _ = state.db.remove_job(nzo).await;
    state.untouch(&nzo);
}

/// v1 `HttpContextExtensions.NormalizePathPrefix`.
fn normalize_path_prefix(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim().trim_end_matches('/');
    if raw.is_empty() || raw == "/" {
        return None;
    }
    if !raw.starts_with('/')
        || raw.contains("//")
        || raw
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-' | '/')))
    {
        return None;
    }
    Some(raw.to_owned())
}

/// v1 `HttpContextExtensions.GetPublicPathPrefix` (the request's own scheme/host are irrelevant
/// here; only the configured public URL's path and `X-Forwarded-Prefix` matter).
fn public_path_prefix(headers: &HeaderMap, configured: &str, _uri: &Uri) -> String {
    let configured_prefix = reqwest::Url::parse(configured)
        .ok()
        .and_then(|u| normalize_path_prefix(Some(u.path())));
    let forwarded = headers
        .get("x-forwarded-prefix")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| normalize_path_prefix(Some(v)));
    match (configured_prefix, forwarded) {
        (Some(c), Some(f)) if !c.ends_with(f.as_str()) => format!("{c}{f}"),
        (Some(c), _) => c,
        (None, Some(f)) => f,
        (None, None) => String::new(),
    }
}

/// v1 `FilenameUtil.VideoExtensions` restricted to what v1's content-type map reports as
/// `video/*` (`.rmvb`, `.iso`, `.img`, `.bin`… are `application/*` there and stay out).
fn is_video_file(name: &str) -> bool {
    const VIDEO: &[&str] = &[
        ".3gp", ".asf", ".asx", ".avi", ".divx", ".flv", ".m2ts", ".m2v", ".m4v", ".mkv", ".mk3d",
        ".mov", ".mp4", ".mpe", ".mpeg", ".mpg", ".ogm", ".ogv", ".qt", ".ts", ".vob", ".webm",
        ".wmv", ".wtv", ".xvid",
    ];
    let ext = name.rsplit('.').next().map(|e| format!(".{e}"));
    ext.is_some_and(|e| VIDEO.iter().any(|v| v.eq_ignore_ascii_case(&e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinidysk_core::ItemId;
    use infinidysk_profiles::cache::Candidate;

    fn candidate(title: &str, size: i64, nzb_url: &str) -> Candidate {
        Candidate {
            indexer_name: "idx".into(),
            indexer_user_agent: "ua".into(),
            nzb_url: nzb_url.into(),
            title: title.into(),
            size,
            posted: None,
            usenet_date: None,
            poster: None,
            grabs: None,
            password: None,
            proxy_url: None,
            source_indexer_name: None,
            language: None,
            subs: None,
            info_hash: None,
            verified_available: false,
        }
    }

    fn entry(candidates: Vec<Candidate>, start_index: usize) -> Entry {
        Entry {
            candidates: Arc::new(candidates),
            start_index,
            r#type: "movie".into(),
            profile_token: "tok".into(),
            id: "tt1".into(),
            created_at: Timestamp::now(),
        }
    }

    #[test]
    fn fallback_queue_keeps_primary_first_and_orders_by_size_proximity() {
        let e = entry(
            vec![
                candidate("A", 1000, "u1"),
                candidate("B", 900, "u2"),
                candidate("C", 1200, "u3"),
                candidate("D", 800, "u4"),
            ],
            0,
        );
        let q = build_fallback_queue(&e, None, false, &[], QualitySortMode::Off);
        let urls: Vec<&str> = q.iter().map(|c| c.nzb_url.as_str()).collect();
        assert_eq!(urls, ["u1", "u2", "u4", "u3"]);
    }

    #[test]
    fn fallback_queue_applies_preferred_order() {
        let e = entry(
            vec![
                candidate("A", 1000, "u1"),
                candidate("B", 900, "u2"),
                candidate("C", 800, "u3"),
            ],
            0,
        );
        // Reported order ranks u3 first among the non-primary candidates.
        let order = vec![
            preferred::release_identity_key(1000, None, None, Some("u1")),
            preferred::release_identity_key(800, None, None, Some("u3")),
            preferred::release_identity_key(900, None, None, Some("u2")),
        ];
        let q = build_fallback_queue(&e, Some(&order), false, &[], QualitySortMode::Off);
        let urls: Vec<&str> = q.iter().map(|c| c.nzb_url.as_str()).collect();
        assert_eq!(urls, ["u1", "u3", "u2"]);
    }

    #[test]
    fn fallback_queue_prefers_matching_subtitle_language() {
        let mut primary = candidate("A", 1000, "u1");
        primary.subs = Some("de".into());
        let mut with_de = candidate("B", 900, "u2");
        with_de.subs = Some("de".into());
        let mut with_en = candidate("C", 800, "u3");
        with_en.subs = Some("en".into());
        let plain = candidate("D", 700, "u4");
        let e = entry(vec![primary, with_de, with_en, plain], 0);
        let q = build_fallback_queue(&e, None, true, &[], QualitySortMode::Off);
        let urls: Vec<&str> = q.iter().map(|c| c.nzb_url.as_str()).collect();
        assert_eq!(urls, ["u1", "u2", "u3", "u4"]);
    }

    #[test]
    fn fallback_queue_ranks_preferred_audio_language_first() {
        let e = entry(
            vec![
                candidate("A", 1000, "u1"),
                candidate("B.2160p", 900, "u2"),
                candidate("C.German.1080p", 800, "u3"),
            ],
            0,
        );
        let mut c_lang = |c: &Candidate| c.clone();
        let _ = &mut c_lang;
        let mut e = e;
        Arc::get_mut(&mut e.candidates).unwrap()[1].language = Some("en".into());
        Arc::get_mut(&mut e.candidates).unwrap()[2].language = Some("de".into());
        let q = build_fallback_queue(&e, None, false, &["de".to_owned()], QualitySortMode::Off);
        let urls: Vec<&str> = q.iter().map(|c| c.nzb_url.as_str()).collect();
        assert_eq!(urls, ["u1", "u3", "u2"]);
    }

    fn item(name: &str, size: u64) -> Item {
        Item {
            id: ItemId::new(),
            parent: None,
            name: name.to_owned(),
            path: format!("/content/movie/x/{name}"),
            kind: infinidysk_core::ItemKind::NzbFile,
            size: Some(size),
            meta_blob: None,
            nzb_blob: None,
            created_at: 0,
            release_date: None,
            health_state: infinidysk_db::HealthState::Healthy,
            encrypted: false,
        }
    }

    #[test]
    fn video_selection_prefers_episode_match_then_token_overlap_and_skips_decoys() {
        let ep = item("Show.S01E02.1080p.mkv", 4_000);
        let other = item("Show.S01E01.1080p.mkv", 3_900);
        let decoy = item("Show.S01E02.unpack.mkv", 9_999);
        let sample = item("Show.S01E02.sample.mkv", 10);
        let videos = vec![&ep, &other, &decoy, &sample];
        let best = select_best_video(&videos, Some(1), Some(2), Some("Show.S01E02.1080p.WEB-DL"));
        assert_eq!(best.map(|b| b.name.as_str()), Some("Show.S01E02.1080p.mkv"));
    }

    #[test]
    fn video_selection_falls_back_to_largest_non_decoy() {
        let a = item("Release.A.mkv", 100);
        let b = item("Release.B.mkv", 500);
        let decoy = item("Release.unpack.mkv", 9_999);
        let videos = vec![&a, &b, &decoy];
        // v1 returns None when no file token overlaps the clicked title.
        let best = select_best_video(&videos, None, None, Some("Unrelated Title XYZ"));
        assert_eq!(best.map(|x| x.name.as_str()), None);
        // With overlap, the tie breaks to the larger file; the decoy never wins.
        let best = select_best_video(&videos, None, None, Some("Release.B.2024.WEB-DL"));
        assert_eq!(best.map(|x| x.name.as_str()), Some("Release.B.mkv"));
    }

    #[test]
    fn sample_and_decoy_detection() {
        assert!(looks_like_sample_name("Show.S01E01.sample.mkv"));
        assert!(looks_like_sample_name("sample-Show.mkv"));
        assert!(!looks_like_sample_name("Resampled.mkv"));
        assert!(is_probably_decoy("x.unpack.mkv"));
        assert!(is_probably_decoy("rickroll.mkv"));
        assert!(!is_probably_decoy("Unpacked.2017.mkv"));
        assert!(is_sample_file("a.sample.mkv", 10, 100));
        assert!(!is_sample_file("a.sample.mkv", 50, 100));
        assert!(!is_sample_file("a.mkv", 10, 100));
    }

    #[test]
    fn season_episode_parsing() {
        assert_eq!(
            parse_season_episode("series", "tt123:2:5"),
            (Some(2), Some(5))
        );
        assert_eq!(parse_season_episode("movie", "tt123:2:5"), (None, None));
        assert_eq!(parse_season_episode("series", "tt123"), (None, None));
    }

    #[test]
    fn path_prefix_combines_configured_and_forwarded() {
        let headers = HeaderMap::new();
        assert_eq!(
            public_path_prefix(&headers, "http://x/", &Uri::from_static("http://x/")),
            ""
        );
        let mut h2 = HeaderMap::new();
        h2.insert("x-forwarded-prefix", HeaderValue::from_static("/dysk"));
        assert_eq!(
            public_path_prefix(
                &h2,
                "https://example.com/app",
                &Uri::from_static("http://x/")
            ),
            "/app/dysk"
        );
        assert_eq!(
            public_path_prefix(
                &h2,
                "https://example.com/app/dysk",
                &Uri::from_static("http://x/")
            ),
            "/app/dysk"
        );
    }

    #[test]
    fn video_extension_filter() {
        assert!(is_video_file("a/b/movie.MKV"));
        assert!(is_video_file("movie.mp4"));
        assert!(!is_video_file("movie.rmvb"));
        assert!(!is_video_file("movie.nzb"));
        assert!(!is_video_file("noext"));
    }

    #[test]
    fn format_bytes_and_age() {
        assert_eq!(format_bytes(0), "?");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3 GB");
        assert_eq!(
            format_age(jiff::SignedDuration::from_secs(400 * 86_400)),
            "1y"
        );
        assert_eq!(
            format_age(jiff::SignedDuration::from_secs(2 * 86_400)),
            "2d"
        );
        assert_eq!(format_age(jiff::SignedDuration::from_secs(3 * 3_600)), "3h");
        assert_eq!(format_age(jiff::SignedDuration::ZERO), "1m");
    }

    #[test]
    fn content_group_key_format() {
        assert_eq!(
            content_group_key("movie", "tt1"),
            Some("movie:tt1".to_owned())
        );
        assert_eq!(content_group_key("", "tt1"), None);
        assert_eq!(content_group_key("movie", "  "), None);
    }
}
