//! Per-indexer request rate limiter, ported from `backend/Services/NewznabRateLimiter.cs`
//! (v1 fork): a per-indexer gate spacing requests by `60s / requests_per_minute`; a refused
//! `try_wait` does not consume budget.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Returned when the caller's cancellation token fired while waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;

#[derive(Debug, Default)]
pub struct RateLimiter {
    gates: Mutex<HashMap<String, Instant>>,
}

impl RateLimiter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// v1 `WaitAsync`: reserves a slot and waits for it.
    pub async fn wait(
        &self,
        indexer_name: &str,
        requests_per_minute: u32,
        ct: &CancellationToken,
    ) -> Result<(), Cancelled> {
        let Some(wait) = self.reserve(indexer_name, requests_per_minute, None).await else {
            return Ok(());
        };
        self.sleep(wait, ct).await
    }

    /// v1 `TryWaitAsync`: reserves a slot only when the wait fits `max_wait`.
    pub async fn try_wait(
        &self,
        indexer_name: &str,
        requests_per_minute: u32,
        max_wait: Duration,
        ct: &CancellationToken,
    ) -> Result<bool, Cancelled> {
        let Some(wait) = self
            .reserve(indexer_name, requests_per_minute, Some(max_wait))
            .await
        else {
            return Ok(false);
        };
        self.sleep(wait, ct).await?;
        Ok(true)
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the gate is mutated under the lock by design (v1 lock(gate)); holding it across the update is intentional"
    )]
    async fn reserve(
        &self,
        indexer_name: &str,
        requests_per_minute: u32,
        max_wait: Option<Duration>,
    ) -> Option<Duration> {
        if requests_per_minute == 0 {
            return Some(Duration::ZERO);
        }
        let interval = Duration::from_secs(60) / requests_per_minute;
        let mut gates = self.gates.lock().await;
        let now = Instant::now();
        let gate = gates.entry(indexer_name.to_owned()).or_insert(now);
        if *gate < now {
            *gate = now;
        }
        let wait = *gate - now;
        if max_wait.is_some_and(|max| wait > max) {
            return None;
        }
        *gate += interval;
        Some(wait)
    }

    async fn sleep(&self, wait: Duration, ct: &CancellationToken) -> Result<(), Cancelled> {
        if wait.is_zero() {
            return Ok(());
        }
        tokio::select! {
            biased;
            () = ct.cancelled() => Err(Cancelled),
            () = tokio::time::sleep(wait) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zero_rate_is_unlimited() {
        let rl = RateLimiter::new();
        let ct = CancellationToken::new();
        let start = Instant::now();
        for _ in 0..10 {
            rl.wait("a", 0, &ct).await.expect("ok");
        }
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn wait_spaces_requests_by_the_interval() {
        let rl = RateLimiter::new();
        let ct = CancellationToken::new();
        // 60000 rpm -> 1 ms interval.
        let start = Instant::now();
        rl.wait("a", 60_000, &ct).await.expect("ok");
        rl.wait("a", 60_000, &ct).await.expect("ok");
        assert!(start.elapsed() >= Duration::from_millis(1));
    }

    #[tokio::test]
    async fn gates_are_per_indexer() {
        let rl = RateLimiter::new();
        let ct = CancellationToken::new();
        rl.wait("a", 60, &ct).await.expect("ok"); // 1s interval
        let start = Instant::now();
        rl.wait("b", 60, &ct).await.expect("ok"); // different gate: no wait
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn try_wait_refusal_does_not_consume_budget() {
        let rl = RateLimiter::new();
        let ct = CancellationToken::new();
        // 1 rpm -> 60s interval; first call reserves immediately, second must refuse.
        assert!(
            rl.try_wait("a", 1, Duration::from_secs(60), &ct)
                .await
                .expect("ok")
        );
        assert!(
            !rl.try_wait("a", 1, Duration::from_millis(10), &ct)
                .await
                .expect("ok")
        );
    }

    #[tokio::test]
    async fn cancellation_aborts_the_wait() {
        let rl = RateLimiter::new();
        let ct = CancellationToken::new();
        rl.wait("a", 1, &ct).await.expect("ok");
        ct.cancel();
        let err = rl.wait("a", 1, &ct).await.unwrap_err();
        assert_eq!(err, Cancelled);
    }
}
