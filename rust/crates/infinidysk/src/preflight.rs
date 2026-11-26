//! Preflight (v1 fork Phase 3), ported from `backend/Services/PreflightOrchestrator.cs`,
//! `PreflightCache.cs`, `PreflightSessionRegistry.cs`, `PlaybackFastVerifier.cs` and
//! `backend/Services/CandidateNegativeCache.cs`: after a profile search, background-verify
//! the top candidates (fetch NZB, STAT/BODY-probe sample segments on the Usenet providers)
//! so the play path can skip dead releases. Warden fingerprints feed in via `Warden`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use infinidysk_nntp::{Client, Error as NntpError, Workload};
use infinidysk_profiles::cache::Candidate;
use infinidysk_profiles::hits::HitType;
use infinidysk_profiles::preferred;
use infinidysk_queue::nzb;

use crate::config::ConfigStore;
use crate::nzb_fetch::{FetchError, FetchKey, MAX_RESPONSE_BYTES, NzbFetchCoalescer};
use crate::warden::Warden;

/// v1 `PlaybackFastVerifier.Verdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Available,
    Dead,
    Timeout,
}

#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    pub verdict: Verdict,
    /// v1 attributes the responder host; the Rust client does not expose it per call, so
    /// this is always `None` (documented deviation).
    pub responder_host: Option<String>,
}

/// v1 `PlaybackFastVerifier`: probes sample segments of the largest non-PAR2 file.
pub struct FastVerifier {
    client: Arc<Client>,
}

impl FastVerifier {
    #[must_use]
    pub fn new(client: Arc<Client>) -> Self {
        Self { client }
    }

    /// v1 `VerifyAsync`. `mode`: "stat" (STAT, fall back to BODY on indefinite-missing),
    /// "body" (BODY directly), "none" → Available without probing.
    pub async fn verify(
        &self,
        nzb_bytes: &[u8],
        mode: &str,
        sample_count: u32,
        segment_timeout: Duration,
        ct: &CancellationToken,
    ) -> VerifyOutcome {
        if mode == "none" {
            return VerifyOutcome {
                verdict: Verdict::Available,
                responder_host: None,
            };
        }
        let xml = match nzb::decompress(nzb_bytes) {
            Ok(x) => x,
            Err(e) => {
                debug!(reason = %e, "fast-verify: NZB decompress failed");
                return VerifyOutcome {
                    verdict: Verdict::Dead,
                    responder_host: None,
                };
            }
        };
        let doc = match nzb::parse(&xml) {
            Ok(d) => d,
            Err(e) => {
                debug!(reason = %e, "fast-verify: NZB parse failed");
                return VerifyOutcome {
                    verdict: Verdict::Dead,
                    responder_host: None,
                };
            }
        };
        let samples = pick_sample_segments(&doc, sample_count.max(1) as usize);
        if samples.is_empty() {
            return VerifyOutcome {
                verdict: Verdict::Dead,
                responder_host: None,
            };
        }
        let mut results = Vec::with_capacity(samples.len());
        for id in &samples {
            results.push(self.check_segment(id, mode, segment_timeout, ct).await);
        }
        if ct.is_cancelled() {
            return VerifyOutcome {
                verdict: Verdict::Timeout,
                responder_host: None,
            };
        }
        if results.contains(&Verdict::Dead) {
            return VerifyOutcome {
                verdict: Verdict::Dead,
                responder_host: None,
            };
        }
        if results.iter().all(|r| *r == Verdict::Timeout) {
            return VerifyOutcome {
                verdict: Verdict::Timeout,
                responder_host: None,
            };
        }
        VerifyOutcome {
            verdict: Verdict::Available,
            responder_host: None,
        }
    }

    async fn check_segment(
        &self,
        message_id: &str,
        mode: &str,
        timeout: Duration,
        ct: &CancellationToken,
    ) -> Verdict {
        let probe_ct = CancellationToken::new();
        let link = ct.clone();
        let watcher = probe_ct.clone();
        tokio::spawn(async move {
            link.cancelled().await;
            watcher.cancel();
        });
        let work = async {
            if mode == "body" {
                return self.probe_body(message_id, ct).await;
            }
            match self.client.stat(message_id, Workload::Health, None).await {
                Ok(()) => Verdict::Available,
                Err(NntpError::NotFound) => self.probe_body(message_id, ct).await,
                Err(_) => Verdict::Timeout,
            }
        };
        tokio::select! {
            () = probe_ct.cancelled() => Verdict::Timeout,
            () = tokio::time::sleep(timeout) => Verdict::Timeout,
            r = work => r,
        }
    }

    async fn probe_body(&self, message_id: &str, ct: &CancellationToken) -> Verdict {
        if ct.is_cancelled() {
            return Verdict::Timeout;
        }
        match self.client.body(message_id, Workload::Health, None).await {
            Ok(_) => Verdict::Available,
            Err(NntpError::NotFound) => Verdict::Dead,
            Err(_) => Verdict::Timeout,
        }
    }
}

/// v1 `PickSampleSegments`: largest non-PAR2 data file (else largest file), `n` segments
/// spread evenly across the part range.
fn pick_sample_segments(doc: &nzb::Nzb, sample_count: usize) -> Vec<String> {
    let is_par2 = |subject: &str| subject.to_ascii_lowercase().contains(".par2");
    let size_of = |f: &nzb::File| -> u64 { f.segments.iter().map(|s| u64::from(s.bytes)).sum() };
    let data_file = doc
        .files
        .iter()
        .filter(|f| !f.segments.is_empty() && !is_par2(&f.subject))
        .max_by_key(|f| size_of(f));
    let any_file = data_file
        .or_else(|| doc.files.iter().filter(|f| !f.segments.is_empty()).max_by_key(|f| size_of(f)));
    let Some(file) = any_file else { return Vec::new() };
    let segs = &file.segments;
    let n = sample_count.min(segs.len());
    if n <= 1 {
        return vec![segs[0].id.clone()];
    }
    let mut indices = std::collections::BTreeSet::new();
    for i in 0..n {
        // v1: Math.Round(i * (count - 1) / (n - 1))
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss, reason = "v1 Math.Round of a non-negative index")]
        let idx = ((i as f64 * (segs.len() - 1) as f64) / (n - 1) as f64).round() as usize;
        indices.insert(idx.min(segs.len() - 1));
    }
    indices.into_iter().map(|i| segs[i].id.clone()).collect()
}

/// A live [`PreflightCache`] hit.
#[derive(Debug, Clone)]
pub struct CacheHit {
    pub verdict: Verdict,
    pub nzb_bytes: Option<Arc<Vec<u8>>>,
    pub responder_host: Option<String>,
}

/// v1 `PreflightCache`: verified-NZB cache with byte/entry budget and per-entry TTL.
#[derive(Default)]
pub struct PreflightCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    current_bytes: Mutex<u64>,
}

struct CacheEntry {
    nzb_bytes: Option<Arc<Vec<u8>>>,
    verdict: Verdict,
    responder_host: Option<String>,
    expires_at: Timestamp,
}

/// v1 `NzbFetchLimits.PreflightMaxCacheBytes` (256 MiB) and entry cap.
pub const PREFLIGHT_MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
pub const PREFLIGHT_MAX_CACHE_ENTRIES: usize = 256;

impl PreflightCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, nzb_url: &str) -> Option<CacheHit> {
        let now = Timestamp::now();
        let mut entries = self.entries.lock().expect("lock");
        match entries.get(nzb_url) {
            Some(e) if now <= e.expires_at => Some(CacheHit {
                verdict: e.verdict,
                nzb_bytes: e.nzb_bytes.clone(),
                responder_host: e.responder_host.clone(),
            }),
            Some(_) => {
                let e = entries.remove(nzb_url).expect("present");
                let mut current = self.current_bytes.lock().expect("lock");
                *current = current.saturating_sub(e.nzb_bytes.as_ref().map_or(0, |b| b.len() as u64));
                drop(current);
                drop(entries);
                None
            }
            None => None,
        }
    }

    /// v1 `SetVerified`.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "guards are used until the end of the fn"
    )]
    pub fn set_verified(
        &self,
        nzb_url: &str,
        nzb_bytes: Option<Vec<u8>>,
        verdict: Verdict,
        responder_host: Option<String>,
        ttl: Duration,
    ) {
        let now = Timestamp::now();
        let bytes = nzb_bytes.map(Arc::new);
        let len = bytes.as_ref().map_or(0, |b| b.len() as u64);
        let mut entries = self.entries.lock().expect("lock");
        let mut current = self.current_bytes.lock().expect("lock");
        // Sweep expired.
        let expired: Vec<String> = entries
            .iter()
            .filter(|(_, e)| now > e.expires_at)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            if let Some(e) = entries.remove(&k) {
                *current = current.saturating_sub(e.nzb_bytes.as_ref().map_or(0, |b| b.len() as u64));
            }
        }
        if let Some(old) = entries.remove(nzb_url) {
            *current = current.saturating_sub(old.nzb_bytes.as_ref().map_or(0, |b| b.len() as u64));
        }
        // Evict soonest-expiring until the budget fits.
        while (entries.len() >= PREFLIGHT_MAX_CACHE_ENTRIES || *current + len > PREFLIGHT_MAX_CACHE_BYTES)
            && !entries.is_empty()
        {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.expires_at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(e) = entries.remove(&oldest) {
                *current = current.saturating_sub(e.nzb_bytes.as_ref().map_or(0, |b| b.len() as u64));
            }
        }
        if entries.len() < PREFLIGHT_MAX_CACHE_ENTRIES && *current + len <= PREFLIGHT_MAX_CACHE_BYTES {
            entries.insert(
                nzb_url.to_owned(),
                CacheEntry {
                    nzb_bytes: bytes,
                    verdict,
                    responder_host,
                    expires_at: now + ttl,
                },
            );
            *current += len;
        }
    }

    pub fn invalidate(&self, nzb_url: &str) {
        let mut entries = self.entries.lock().expect("lock");
        if let Some(e) = entries.remove(nzb_url) {
            *self.current_bytes.lock().expect("lock") = self
                .current_bytes
                .lock()
                .expect("lock")
                .saturating_sub(e.nzb_bytes.as_ref().map_or(0, |b| b.len() as u64));
        }
    }
}

/// v1 `PreflightSessionRegistry`: one live session per (token, type, id); a new session
/// cancels the previous one.
#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, CancellationToken>>,
}

impl SessionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the session token; a previous session for the same key is cancelled.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the guard is used by insert at the end"
    )]
    pub fn begin(&self, profile_token: &str, r#type: &str, id: &str) -> CancellationToken {
        let key = format!("{profile_token}\x1f{type}\x1f{id}");
        let ct = CancellationToken::new();
        let mut sessions = self.sessions.lock().expect("lock");
        if let Some(prior) = sessions.insert(key, ct.clone()) {
            prior.cancel();
        }
        ct
    }

    pub fn cancel(&self, profile_token: &str, r#type: &str, id: &str) {
        let key = format!("{profile_token}\x1f{type}\x1f{id}");
        if let Some(ct) = self.sessions.lock().expect("lock").remove(&key) {
            ct.cancel();
        }
    }

    fn complete_if_owned(&self, key: &str, owned: &CancellationToken) {
        let mut sessions = self.sessions.lock().expect("lock");
        if sessions.get(key) == Some(owned) {
            sessions.remove(key);
        }
    }
}

/// v1 `CandidateNegativeCache`: failed (queue-failure) and stalled (serving-stall) marks,
/// each with its own TTL from `play.*`.
#[derive(Default)]
pub struct CandidateNegativeCache {
    failed_at: Mutex<HashMap<String, Timestamp>>,
    stalled_at: Mutex<HashMap<String, Timestamp>>,
}

impl CandidateNegativeCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_failed(&self, nzb_url: &str, ttl: Duration) -> bool {
        let now = Timestamp::now();
        let mut map = self.failed_at.lock().expect("lock");
        if let Some(at) = map.get(nzb_url) {
            if now.as_second() - at.as_second() < ttl.as_secs().cast_signed() {
                return true;
            }
            map.remove(nzb_url);
        }
        false
    }

    pub fn mark_failed(&self, nzb_url: &str) {
        let mut map = self.failed_at.lock().expect("lock");
        map.insert(nzb_url.to_owned(), Timestamp::now());
        if map.len() > 512 {
            let cutoff = Timestamp::now() - Duration::from_secs(3600);
            map.retain(|_, at| *at > cutoff);
        }
    }

    pub fn is_stalled(&self, nzb_url: &str, ttl: Duration) -> bool {
        let now = Timestamp::now();
        let mut map = self.stalled_at.lock().expect("lock");
        if let Some(at) = map.get(nzb_url) {
            if now.as_second() - at.as_second() < ttl.as_secs().cast_signed() {
                return true;
            }
            map.remove(nzb_url);
        }
        false
    }

    pub fn mark_stalled(&self, nzb_url: &str) {
        let mut map = self.stalled_at.lock().expect("lock");
        map.insert(nzb_url.to_owned(), Timestamp::now());
        if map.len() > 512 {
            let cutoff = Timestamp::now() - Duration::from_secs(3600);
            map.retain(|_, at| *at > cutoff);
        }
    }
}

/// v1 `PreflightOrchestrator`: background verification of the top search candidates.
pub struct Orchestrator {
    pub config: Arc<RwLock<ConfigStore>>,
    /// Last settings snapshot for the sync `start` hook (refreshed by [`Orchestrator::sync_settings`]).
    snapshot: std::sync::RwLock<Snapshot>,
    pub cache: Arc<PreflightCache>,
    pub sessions: Arc<SessionRegistry>,
    pub verifier: Arc<FastVerifier>,
    pub coalescer: Arc<NzbFetchCoalescer>,
    pub warden: Arc<Warden>,
    pub negative: Arc<CandidateNegativeCache>,
    /// The search service whose hit tracker and rate limiter gate preflight fetches.
    pub search: Arc<infinidysk_profiles::SearchService>,
}

/// Construction parts for [`Orchestrator::new`].
pub struct Parts {
    pub config: Arc<RwLock<ConfigStore>>,
    pub cache: Arc<PreflightCache>,
    pub sessions: Arc<SessionRegistry>,
    pub verifier: Arc<FastVerifier>,
    pub coalescer: Arc<NzbFetchCoalescer>,
    pub warden: Arc<Warden>,
    pub negative: Arc<CandidateNegativeCache>,
    pub search: Arc<infinidysk_profiles::SearchService>,
}

impl Orchestrator {
    #[must_use]
    pub fn new(p: Parts) -> Arc<Self> {
        let Parts {
            config,
            cache,
            sessions,
            verifier,
            coalescer,
            warden,
            negative,
            search,
        } = p;
        Arc::new(Self {
            config,
            snapshot: std::sync::RwLock::new(Snapshot {
                mode: "off".to_owned(),
                max_attempts: 20,
                ttl: Duration::from_secs(120),
                max_wait: Duration::from_secs(5),
            }),
            cache,
            sessions,
            verifier,
            coalescer,
            warden,
            negative,
            search,
        })
    }
}

/// v1 `FetchTimeout` for the preflight NZB fetch (8 s).
const PREFLIGHT_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
/// v1 `DefaultSegmentTimeout`.
const DEFAULT_SEGMENT_TIMEOUT: Duration = Duration::from_secs(15);

impl Orchestrator {
    /// v1 `Start`: spawn the background preflight for one search result.
    pub fn start(
        self: &Arc<Self>,
        profile_token: &str,
        r#type: &str,
        id: &str,
        candidates: &[Candidate],
    ) {
        let snap = self.snapshot.read().expect("lock").clone();
        let Snapshot {
            mode,
            max_attempts,
            ttl,
            max_wait,
        } = snap;
        if mode == "off" || candidates.is_empty() {
            return;
        }
        let this = Arc::clone(self);
        let token = self.sessions.begin(profile_token, r#type, id);
        let key = format!("{profile_token}\x1f{type}\x1f{id}");
        let ty = r#type.to_owned();
        let idd = id.to_owned();
        let candidates = candidates.to_vec();
        tokio::spawn(async move {
            let prepared = this
                .preflight(&mode, &candidates, max_attempts, ttl, max_wait, &token)
                .await;
            if prepared > 0 {
                debug!(
                    count = prepared,
                    r#type = %ty,
                    id = %idd,
                    mode = %mode,
                    "preflight readied candidate(s)"
                );
            }
            this.sessions.complete_if_owned(&key, &token);
        });
    }

    async fn preflight(
        &self,
        mode: &str,
        candidates: &[Candidate],
        max_attempts: u32,
        ttl: Duration,
        max_wait: Duration,
        ct: &CancellationToken,
    ) -> usize {
        let max_attempts = (max_attempts as usize).min(candidates.len());
        let indexers: HashMap<String, infinidysk_indexer::models::ConnectionDetails> = {
            let store = self.config.read().await;
            store
                .settings()
                .indexers
                .indexers
                .iter()
                .map(|x| (x.name.clone(), x.clone()))
                .collect()
        };
        let sample_count = {
            let store = self.config.read().await;
            store.settings().play.verify_sample_count()
        };
        for (i, c) in candidates.iter().take(max_attempts).enumerate() {
            if ct.is_cancelled() {
                return 0;
            }
            let ok = self
                .preflight_candidate(mode, c, &indexers, max_wait, sample_count, ttl, ct)
                .await;
            if ok {
                return i + 1;
            }
        }
        0
    }

    #[expect(clippy::too_many_arguments, reason = "v1 PreflightCandidateAsync parameters")]
    async fn preflight_candidate(
        &self,
        mode: &str,
        candidate: &Candidate,
        indexers: &HashMap<String, infinidysk_indexer::models::ConnectionDetails>,
        max_wait: Duration,
        sample_count: u32,
        ttl: Duration,
        ct: &CancellationToken,
    ) -> bool {
        if ct.is_cancelled() {
            return false;
        }
        let fp = preferred::warden_fingerprint(candidate.size, candidate.poster.as_deref(), candidate.usenet_date);
        if self.warden.is_dead_anywhere(fp.as_deref()).await {
            return false;
        }
        if let Some(indexer) = indexers.get(&candidate.indexer_name) {
            let now = Timestamp::now();
            let check = self
                .search
                .hit_tracker()
                .check(
                    &candidate.indexer_name,
                    HitType::Download,
                    indexer.download_limit.map(i64::from),
                    indexer.hit_limit_reset_time.and_then(|h| i32::try_from(h).ok()),
                    now,
                )
                .await;
            if let Some(c) = &check
                && !c.allowed
            {
                debug!(indexer = %candidate.indexer_name, "preflight skipped: hit limit");
                return false;
            }
            if self
                .search
                .rate_limiter()
                .try_wait(&candidate.indexer_name, indexer.max_requests_per_minute, max_wait, ct)
                .await
                .is_err()
            {
                return false;
            }
        }
        let Some(nzb_bytes) = self.fetch_nzb(candidate, ct).await else {
            return false;
        };
        if ct.is_cancelled() {
            return false;
        }
        self.search
            .hit_tracker()
            .record(&candidate.indexer_name, HitType::Download, Timestamp::now())
            .await;
        let outcome = self
            .verifier
            .verify(&nzb_bytes, mode, sample_count, DEFAULT_SEGMENT_TIMEOUT, ct)
            .await;
        if outcome.verdict == Verdict::Dead {
            self.negative.mark_failed(&candidate.nzb_url);
            self.warden.mark_dead(fp.as_deref()).await;
            return false;
        }
        let bytes_to_cache = if mode == "light" {
            None
        } else {
            Some(nzb_bytes.as_ref().clone())
        };
        self.cache.set_verified(
            &candidate.nzb_url,
            bytes_to_cache,
            outcome.verdict,
            outcome.responder_host,
            ttl,
        );
        // v1 `full` mode additionally pre-warms lazy-RAR library items; that is local-library
        // machinery with no Rust equivalent yet (documented deviation), so `full` currently
        // behaves like `standard`.
        true
    }

    async fn fetch_nzb(&self, c: &Candidate, ct: &CancellationToken) -> Option<Arc<Vec<u8>>> {
        let skip_tls = {
            let store = self.config.read().await;
            store
                .settings()
                .indexers
                .should_skip_tls_verification(&c.indexer_name)
        };
        let key = FetchKey {
            url: c.nzb_url.clone(),
            proxy_url: c.proxy_url.clone().unwrap_or_default(),
            skip_tls_verification: skip_tls,
        };
        let ua = c.indexer_user_agent.clone();
        let url = c.nzb_url.clone();
        let proxy = c.proxy_url.clone();
        let result = self
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
                    let client =
                        infinidysk_indexer::http::build_client_with_proxy(skip_tls, &host, proxy.as_deref())
                            .map_err(|_| FetchError::Failed)?;
                    let resp = infinidysk_indexer::http::with_timeout(
                        PREFLIGHT_FETCH_TIMEOUT,
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
                            warn!(url = %url, limit = max_bytes, "preflight rejected oversized NZB response");
                            Ok(None)
                        }
                        Err(infinidysk_indexer::Error::Cancelled) => Err(FetchError::Cancelled),
                        Err(_) => Err(FetchError::Failed),
                    }
                }
            })
            .await;
        match result {
            Ok(Some(bytes)) => Some(bytes),
            Ok(None) | Err(FetchError::Cancelled) => None,
            Err(FetchError::Failed) => {
                debug!(url = %c.nzb_url, "preflight NZB fetch failed");
                None
            }
        }
    }
}

/// Owned Phase-3 preflight knobs (v1 getters with their clamps).
#[derive(Clone)]
pub struct Snapshot {
    pub mode: String,
    pub max_attempts: u32,
    pub ttl: Duration,
    pub max_wait: Duration,
}

impl Orchestrator {
    /// Refreshes the snapshot used by the sync `start` hook (call per request, like
    /// `Search::sync_settings`).
    pub async fn sync_settings(&self) {
        let snap = {
            let store = self.config.read().await;
            let p = store.settings().preflight.clone();
            drop(store);
            Snapshot {
                mode: p.mode().to_owned(),
                max_attempts: p.max_attempts(),
                ttl: Duration::from_secs(u64::from(p.ttl_seconds())),
                max_wait: Duration::from_secs(u64::from(p.indexer_max_wait_seconds())),
            }
        };
        *self.snapshot.write().expect("lock") = snap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(files: Vec<(&str, Vec<u32>)>) -> nzb::Nzb {
        nzb::Nzb {
            meta: Vec::new(),
            files: files
                .into_iter()
                .map(|(subject, segs)| nzb::File {
                    subject: subject.to_owned(),
                    date: None,
                    gap: false,
                    segments: segs
                        .into_iter()
                        .enumerate()
                        .map(|(i, b)| nzb::Segment {
                            id: format!("seg{i}@x"),
                            bytes: b,
                            number: i64::try_from(i).unwrap_or(i64::MAX) + 1,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn sample_segments_spread_evenly_and_skip_par2() {
        let d = doc(vec![
            ("movie.par2", vec![100, 100]),
            ("movie.mkv", vec![10; 10]),
        ]);
        let s = pick_sample_segments(&d, 3);
        assert_eq!(s, vec!["seg0@x", "seg4@x", "seg9@x"]);
        let s = pick_sample_segments(&d, 1);
        assert_eq!(s, vec!["seg0@x"]);
        let d2 = doc(vec![("only.par2", vec![5, 5])]);
        let s = pick_sample_segments(&d2, 2);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn negative_cache_ttls() {
        let c = CandidateNegativeCache::new();
        assert!(!c.is_failed("u", Duration::from_secs(60)));
        c.mark_failed("u");
        assert!(c.is_failed("u", Duration::from_secs(60)));
        assert!(!c.is_failed("u", Duration::from_millis(0)));
        c.mark_stalled("u");
        assert!(c.is_stalled("u", Duration::from_secs(60)));
        assert!(!c.is_stalled("v", Duration::from_secs(60)));
    }

    #[test]
    fn session_registry_supersedes() {
        let r = SessionRegistry::new();
        let a = r.begin("t", "movie", "tt1");
        let b = r.begin("t", "movie", "tt1");
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
        r.cancel("t", "movie", "tt1");
        assert!(b.is_cancelled());
    }

    #[test]
    fn cache_budget_and_ttl() {
        let c = PreflightCache::new();
        c.set_verified("u", Some(vec![1, 2, 3]), Verdict::Available, None, Duration::from_secs(60));
        let hit = c.get("u").expect("hit");
        assert_eq!(hit.verdict, Verdict::Available);
        assert_eq!(hit.nzb_bytes.as_deref().map(Vec::as_slice), Some(&[1, 2, 3][..]));
        c.set_verified("u", None, Verdict::Dead, None, Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(5));
        assert!(c.get("u").is_none());
        c.set_verified("v", Some(vec![9]), Verdict::Timeout, None, Duration::from_secs(60));
        c.invalidate("v");
        assert!(c.get("v").is_none());
    }
}
