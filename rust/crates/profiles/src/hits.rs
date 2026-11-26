//! Per-indexer API-hit tracking, ported from `backend/Services/IndexerHitTracker.cs`
//! (v1 fork) with NZBHydra2-style windows: a fixed daily reset hour (UTC) or a rolling
//! 24-hour window when no reset hour is set.
//!
//! Deviation from v1: v1 persists hits in the `IndexerApiHits` table (EF/SQLite) and
//! prunes with 5% probability per record. This port keeps the counters in memory (the
//! V2 `crates/db` wiring is a follow-up) and prunes deterministically every 20th record;
//! history is retained for 48h, the same safety buffer as v1.

use std::collections::HashMap;

use jiff::{SignedDuration, Span, Timestamp};
use tokio::sync::Mutex;

/// v1 `IndexerApiHit.HitType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitType {
    Search,
    Download,
}

/// v1 `HitCheckResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitCheck {
    pub allowed: bool,
    pub current_count: i64,
    pub limit: i64,
    pub reset_at: Timestamp,
}

/// One indexer's limits for [`HitTracker::get_usage`] (v1's anonymous tuple).
#[derive(Debug, Clone)]
pub struct UsageQuery {
    pub name: String,
    pub hit_limit: Option<i64>,
    pub download_limit: Option<i64>,
    pub reset_hour_utc: Option<i32>,
}

/// v1 `UsageSnapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub indexer_name: String,
    pub api_hits: i64,
    pub api_hit_limit: Option<i64>,
    pub download_hits: i64,
    pub download_hit_limit: Option<i64>,
    pub reset_at: Timestamp,
}

/// Keep at most this much history (longest enforced window is 24h; 48h is the v1 buffer).
const RETAIN_FOR: SignedDuration = SignedDuration::from_hours(48);
/// Deterministic stand-in for v1's 5% probabilistic prune.
const PRUNE_EVERY: u64 = 20;

#[derive(Debug, Default)]
struct Inner {
    hits: HashMap<(String, HitType), Vec<Timestamp>>,
    records: u64,
}

#[derive(Debug, Default)]
pub struct HitTracker {
    inner: Mutex<Inner>,
}

impl HitTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// v1 `ComputeWindow`: fixed daily reset hour (UTC) when given, else rolling 24h.
    #[must_use]
    pub fn compute_window(now: Timestamp, reset_hour_utc: Option<i32>) -> (Timestamp, Timestamp) {
        match reset_hour_utc {
            Some(hour) if (0..=23).contains(&hour) => {
                let secs = now.as_second();
                let day_start = now - SignedDuration::from_secs(secs.rem_euclid(86_400));
                let today_at_hour = day_start + SignedDuration::from_secs(i64::from(hour) * 3_600);
                let window_start = if today_at_hour <= now {
                    today_at_hour
                } else {
                    today_at_hour - SignedDuration::from_secs(86_400)
                };
                let next_reset = window_start + SignedDuration::from_secs(86_400);
                (window_start, next_reset)
            }
            _ => (now - Span::new().hours(24), now + Span::new().hours(24)),
        }
    }

    /// v1 `CheckAsync`: `None` when no limit is configured (always allowed).
    pub async fn check(
        &self,
        indexer_name: &str,
        hit_type: HitType,
        limit: Option<i64>,
        reset_hour_utc: Option<i32>,
        now: Timestamp,
    ) -> Option<HitCheck> {
        let limit = limit?;
        if limit <= 0 {
            return None;
        }
        let (window_start, next_reset_at) = Self::compute_window(now, reset_hour_utc);
        let count = {
            let inner = self.inner.lock().await;
            inner
                .hits
                .get(&(indexer_name.to_owned(), hit_type))
                .map_or(0, |v| {
                    i64::try_from(v.iter().filter(|t| **t >= window_start).count())
                        .unwrap_or(i64::MAX)
                })
        };
        Some(HitCheck {
            allowed: count < limit,
            current_count: count,
            limit,
            reset_at: next_reset_at,
        })
    }

    /// v1 `RecordAsync` (best-effort in-memory variant, with periodic prune).
    pub async fn record(&self, indexer_name: &str, hit_type: HitType, now: Timestamp) {
        let mut inner = self.inner.lock().await;
        inner
            .hits
            .entry((indexer_name.to_owned(), hit_type))
            .or_default()
            .push(now);
        inner.records += 1;
        if inner.records % PRUNE_EVERY == 0 {
            let cutoff = now - RETAIN_FOR;
            for v in inner.hits.values_mut() {
                v.retain(|t| *t >= cutoff);
            }
            inner.hits.retain(|_, v| !v.is_empty());
        }
    }

    /// v1 `GetUsageAsync`: one snapshot per requested indexer for the current window.
    pub async fn get_usage(&self, indexers: &[UsageQuery], now: Timestamp) -> Vec<UsageSnapshot> {
        let inner = self.inner.lock().await;
        indexers
            .iter()
            .map(|q| {
                let (window_start, next_reset_at) = Self::compute_window(now, q.reset_hour_utc);
                let count = |t: HitType| {
                    inner.hits.get(&(q.name.clone(), t)).map_or(0, |v| {
                        i64::try_from(v.iter().filter(|ts| **ts >= window_start).count())
                            .unwrap_or(i64::MAX)
                    })
                };
                UsageSnapshot {
                    indexer_name: q.name.clone(),
                    api_hits: count(HitType::Search),
                    api_hit_limit: q.hit_limit.filter(|l| *l > 0),
                    download_hits: count(HitType::Download),
                    download_hit_limit: q.download_limit.filter(|l| *l > 0),
                    reset_at: next_reset_at,
                }
            })
            .collect()
    }

    /// v1 `FormatSkipReason`.
    #[must_use]
    pub fn format_skip_reason(check: &HitCheck, hit_type: HitType, now: Timestamp) -> String {
        let type_label = match hit_type {
            HitType::Search => "API hit",
            HitType::Download => "download",
        };
        let remaining = check.reset_at - now;
        let total_secs = remaining.as_secs();
        let human = if total_secs >= 3_600 {
            let h = total_secs / 3_600;
            let m = (total_secs % 3_600) / 60;
            format!("{h}h {m}m")
        } else {
            let m = total_secs / 60;
            format!("{}m", m.max(1))
        };
        format!(
            "{type_label} limit reached ({}/{}, resets in {human})",
            check.current_count, check.limit
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("timestamp")
    }

    #[test]
    fn window_rolling_24h_without_reset_hour() {
        let now = ts("2026-01-10T12:00:00Z");
        let (start, reset) = HitTracker::compute_window(now, None);
        assert_eq!(start, ts("2026-01-09T12:00:00Z"));
        assert_eq!(reset, ts("2026-01-11T12:00:00Z"));
    }

    #[test]
    fn window_fixed_hour_today_and_yesterday() {
        let now = ts("2026-01-10T12:00:00Z");
        let (start, reset) = HitTracker::compute_window(now, Some(6));
        assert_eq!(start, ts("2026-01-10T06:00:00Z"));
        assert_eq!(reset, ts("2026-01-11T06:00:00Z"));
        // Before the reset hour: the window started yesterday.
        let early = ts("2026-01-10T03:00:00Z");
        let (start, reset) = HitTracker::compute_window(early, Some(6));
        assert_eq!(start, ts("2026-01-09T06:00:00Z"));
        assert_eq!(reset, ts("2026-01-10T06:00:00Z"));
        // Out-of-range hours fall back to rolling 24h.
        let (start, _) = HitTracker::compute_window(now, Some(24));
        assert_eq!(start, ts("2026-01-09T12:00:00Z"));
    }

    #[tokio::test]
    async fn check_and_record_enforce_the_limit_within_the_window() {
        let tracker = HitTracker::new();
        let now = ts("2026-01-10T12:00:00Z");
        assert!(
            tracker
                .check("a", HitType::Search, None, None, now)
                .await
                .is_none()
        );
        assert!(
            tracker
                .check("a", HitType::Search, Some(0), None, now)
                .await
                .is_none()
        );

        tracker.record("a", HitType::Search, now).await;
        let c = tracker
            .check("a", HitType::Search, Some(2), None, now)
            .await
            .expect("check");
        assert!(c.allowed);
        assert_eq!(c.current_count, 1);
        tracker.record("a", HitType::Search, now).await;
        let c = tracker
            .check("a", HitType::Search, Some(2), None, now)
            .await
            .expect("check");
        assert!(!c.allowed);
        assert_eq!(c.current_count, 2);
        // Hits outside the window do not count.
        let c = tracker
            .check(
                "a",
                HitType::Search,
                Some(2),
                None,
                now + Span::new().hours(25),
            )
            .await
            .expect("check");
        assert!(c.allowed);
        assert_eq!(c.current_count, 0);
        // Download hits are tracked separately: the two search hits do not count here.
        let d = tracker
            .check("a", HitType::Download, Some(1), None, now)
            .await
            .expect("check");
        assert_eq!(d.current_count, 0);
        assert!(d.allowed);
    }

    #[tokio::test]
    async fn record_prunes_old_history() {
        let tracker = HitTracker::new();
        let now = ts("2026-01-10T12:00:00Z");
        let old = now - Span::new().hours(72);
        for _ in 0..20 {
            tracker.record("a", HitType::Search, old).await;
        }
        let usage = tracker
            .get_usage(
                &[UsageQuery {
                    name: "a".to_owned(),
                    hit_limit: None,
                    download_limit: None,
                    reset_hour_utc: None,
                }],
                now,
            )
            .await;
        assert_eq!(usage[0].api_hits, 0);
    }

    #[tokio::test]
    async fn usage_snapshots_report_both_types() {
        let tracker = HitTracker::new();
        let now = ts("2026-01-10T12:00:00Z");
        tracker.record("a", HitType::Search, now).await;
        tracker.record("a", HitType::Search, now).await;
        tracker.record("a", HitType::Download, now).await;
        let usage = tracker
            .get_usage(
                &[UsageQuery {
                    name: "a".to_owned(),
                    hit_limit: Some(5),
                    download_limit: Some(1),
                    reset_hour_utc: Some(6),
                }],
                now,
            )
            .await;
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].api_hits, 2);
        assert_eq!(usage[0].api_hit_limit, Some(5));
        assert_eq!(usage[0].download_hits, 1);
        assert_eq!(usage[0].download_hit_limit, Some(1));
    }

    #[test]
    fn format_skip_reason_humanises_the_reset() {
        let now = ts("2026-01-10T12:00:00Z");
        let check = HitCheck {
            allowed: false,
            current_count: 5,
            limit: 5,
            reset_at: now + Span::new().hours(2).minutes(30),
        };
        assert_eq!(
            HitTracker::format_skip_reason(&check, HitType::Search, now),
            "API hit limit reached (5/5, resets in 2h 30m)"
        );
        let check = HitCheck {
            allowed: false,
            current_count: 1,
            limit: 1,
            reset_at: now + Span::new().minutes(10),
        };
        assert_eq!(
            HitTracker::format_skip_reason(&check, HitType::Download, now),
            "download limit reached (1/1, resets in 10m)"
        );
    }
}
