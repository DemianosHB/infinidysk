//! NZB fetch coalescer, ported from `backend/Services/NzbFetchCoalescer.cs` (v1 fork):
//! concurrent fetches of the same `(url, proxy, skipTls)` share one in-flight task, and
//! successful bodies are cached for 5 minutes within a 256 MiB / 64-entry budget.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use tokio_util::sync::CancellationToken;

/// v1 `NzbFetchLimits.MaxResponseBytes` (50 MiB).
pub const MAX_RESPONSE_BYTES: u64 = 50 * 1024 * 1024;
/// v1 `NzbFetchLimits.CoalescerMaxCacheBytes`.
pub const COALESCER_MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
/// v1 `NzbFetchLimits.CoalescerMaxCacheEntries`.
pub const COALESCER_MAX_CACHE_ENTRIES: usize = 64;
/// v1 `NzbFetchCoalescer` default cache TTL (5 minutes).
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(300);
/// v1 `NzbFetchCoalescer` hard fetch cap (90 s); the caller applies its own shorter timeout.
pub const HARD_FETCH_CAP: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FetchKey {
    pub url: String,
    pub proxy_url: String,
    pub skip_tls_verification: bool,
}

#[derive(Debug)]
pub enum FetchError {
    /// The caller was cancelled while waiting (v1 `OperationCanceledException`).
    Cancelled,
    /// Transport/HTTP failure (v1 `HttpRequestException`/`IOException`/`TimeoutException`).
    Failed,
}

struct CacheEntry {
    bytes: Arc<Vec<u8>>,
    expires_at: Timestamp,
}

impl std::fmt::Debug for CacheEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheEntry")
            .field("len", &self.bytes.len())
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// One shared in-flight fetch: the first caller runs it, the others wait on `done`.
struct InFlight {
    result: Mutex<InFlightResult>,
    done: tokio::sync::Notify,
}

enum InFlightResult {
    Pending,
    Done(Option<Arc<Vec<u8>>>),
}

impl std::fmt::Debug for InFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlight")
            .field(
                "done",
                &self
                    .result
                    .lock()
                    .map(|r| !matches!(*r, InFlightResult::Pending)),
            )
            .finish()
    }
}

/// v1 `NzbFetchCoalescer`.
#[derive(Debug, Default)]
pub struct NzbFetchCoalescer {
    in_flight: Mutex<HashMap<FetchKey, Arc<InFlight>>>,
    cache: Mutex<(HashMap<FetchKey, CacheEntry>, u64)>,
}

impl NzbFetchCoalescer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// v1 `GetOrFetchAsync`: cached hit, else join the shared in-flight fetch.
    pub async fn get_or_fetch<F, Fut>(
        self: &Arc<Self>,
        key: FetchKey,
        ct: &CancellationToken,
        fetch: F,
    ) -> Result<Option<Arc<Vec<u8>>>, FetchError>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Option<Vec<u8>>, FetchError>> + Send,
    {
        if let Some(cached) = self.try_get_cached(&key) {
            return Ok(Some(cached));
        }
        let shared = {
            let mut in_flight = self.in_flight.lock().expect("lock");
            Arc::clone(in_flight.entry(key.clone()).or_insert_with(|| {
                let shared = Arc::new(InFlight {
                    result: Mutex::new(InFlightResult::Pending),
                    done: tokio::sync::Notify::new(),
                });
                let this = Arc::clone(self);
                let task_shared = Arc::clone(&shared);
                let task_key = key.clone();
                tokio::spawn(async move {
                    let outcome = Self::run_shared(fetch).await;
                    match outcome {
                        Ok(Some(bytes)) => {
                            let bytes = Arc::new(bytes);
                            this.try_admit(&task_key, Arc::clone(&bytes));
                            *task_shared.result.lock().expect("lock") =
                                InFlightResult::Done(Some(bytes));
                        }
                        _ => {
                            *task_shared.result.lock().expect("lock") = InFlightResult::Done(None);
                        }
                    }
                    task_shared.done.notify_waiters();
                    this.in_flight.lock().expect("lock").remove(&task_key);
                });
                shared
            }))
        };
        tokio::select! {
            () = ct.cancelled() => Err(FetchError::Cancelled),
            waited = async {
                loop {
                    let done = {
                        let result = shared.result.lock().expect("lock");
                        match &*result {
                            InFlightResult::Done(r) => Some(r.clone()),
                            InFlightResult::Pending => None,
                        }
                    };
                    if let Some(r) = done {
                        return Ok(r);
                    }
                    shared.done.notified().await;
                }
            } => waited,
        }
    }

    async fn run_shared<F, Fut>(fetch: F) -> Result<Option<Vec<u8>>, FetchError>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Option<Vec<u8>>, FetchError>> + Send,
    {
        let hard = CancellationToken::new();
        let cap = hard.clone();
        tokio::spawn(async move {
            tokio::time::sleep(HARD_FETCH_CAP).await;
            cap.cancel();
        });
        fetch(hard).await
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the guard is borrowed by map/current_bytes until the end"
    )]
    fn try_get_cached(&self, key: &FetchKey) -> Option<Arc<Vec<u8>>> {
        let now = Timestamp::now();
        let mut cache = self.cache.lock().expect("lock");
        let (map, current_bytes) = &mut *cache;
        let hit = map
            .get(key)
            .filter(|e| now <= e.expires_at)
            .map(|e| Arc::clone(&e.bytes));
        if hit.is_none()
            && let Some(removed) = map.remove(key)
        {
            *current_bytes = current_bytes.saturating_sub(removed.bytes.len() as u64);
        }
        hit
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the guard is borrowed by map/current_bytes until the end"
    )]
    fn try_admit(&self, key: &FetchKey, bytes: Arc<Vec<u8>>) {
        let now = Timestamp::now();
        let len = bytes.len() as u64;
        let mut cache = self.cache.lock().expect("lock");
        let (map, current_bytes) = &mut *cache;
        // Sweep expired entries first (v1 SweepExpiredUnlocked on admit).
        map.retain(|_, e| {
            let expired = now > e.expires_at;
            if expired {
                *current_bytes = current_bytes.saturating_sub(e.bytes.len() as u64);
            }
            !expired
        });
        if let Some(existing) = map.remove(key) {
            *current_bytes = current_bytes.saturating_sub(existing.bytes.len() as u64);
        }
        // Evict the soonest-expiring entries until the budget fits (v1 oldest-expiry loop).
        while (map.len() >= COALESCER_MAX_CACHE_ENTRIES
            || *current_bytes + len > COALESCER_MAX_CACHE_BYTES)
            && !map.is_empty()
        {
            let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.expires_at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(removed) = map.remove(&oldest) {
                *current_bytes = current_bytes.saturating_sub(removed.bytes.len() as u64);
            }
        }
        if map.len() < COALESCER_MAX_CACHE_ENTRIES
            && *current_bytes + len <= COALESCER_MAX_CACHE_BYTES
        {
            map.insert(
                key.clone(),
                CacheEntry {
                    bytes,
                    expires_at: now + DEFAULT_CACHE_TTL,
                },
            );
            *current_bytes += len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_fetches_share_one_call_and_cache_the_body() {
        let coalescer = Arc::new(NzbFetchCoalescer::new());
        let calls = Arc::new(Mutex::new(0usize));
        let key = FetchKey {
            url: "http://indexer/nzb".to_owned(),
            proxy_url: String::new(),
            skip_tls_verification: false,
        };
        let ct = CancellationToken::new();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let c = Arc::clone(&coalescer);
            let k = key.clone();
            let calls = Arc::clone(&calls);
            let ct = ct.clone();
            handles.push(tokio::spawn(async move {
                c.get_or_fetch(k, &ct, move |_hard| {
                    let calls = Arc::clone(&calls);
                    async move {
                        *calls.lock().expect("lock") += 1;
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(Some(vec![1u8, 2, 3]))
                    }
                })
                .await
            }));
        }
        for h in handles {
            assert_eq!(
                h.await.expect("join").expect("ok").expect("body").as_ref(),
                &[1u8, 2, 3]
            );
        }
        assert_eq!(*calls.lock().expect("lock"), 1);
        // Second round is served from the cache without a new fetch.
        let again = coalescer
            .get_or_fetch(key, &ct, |_| async { Ok(Some(vec![9u8])) })
            .await
            .expect("ok")
            .expect("body");
        assert_eq!(again.as_ref(), &[1u8, 2, 3]);
        assert_eq!(*calls.lock().expect("lock"), 1);
    }

    #[tokio::test]
    async fn caller_cancellation_is_reported_as_cancelled() {
        let coalescer = Arc::new(NzbFetchCoalescer::new());
        let key = FetchKey {
            url: "http://indexer/slow".to_owned(),
            proxy_url: String::new(),
            skip_tls_verification: false,
        };
        let ct = CancellationToken::new();
        let handle = tokio::spawn({
            let c = Arc::clone(&coalescer);
            let ct = ct.clone();
            async move {
                c.get_or_fetch(key, &ct, |_hard| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(Some(vec![]))
                })
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        ct.cancel();
        assert!(matches!(
            handle.await.expect("join"),
            Err(FetchError::Cancelled)
        ));
    }
}
