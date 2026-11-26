//! Play-token resolution cache, ported from `backend/Services/NzbResolutionCache.cs`
//! (v1 fork). Registers a candidate group and returns one token per candidate; each token
//! points at the same ordered candidate list with its own start index so the play handler
//! can iterate from any position for fast-fail + fallback.
//!
//! Deviation from v1: v1 also persists groups to SQLite (`NzbResolutionGroups`) so tokens
//! survive restarts; this port is in-memory only (the V2 `crates/db` wiring, including
//! `HydrateAsync`/`PurgeExpiredAsync` persistence, is a follow-up).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use jiff::{Span, Timestamp};
use serde::{Deserialize, Serialize};

/// v1 `NzbResolutionCache.Candidate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub indexer_name: String,
    pub indexer_user_agent: String,
    pub nzb_url: String,
    pub title: String,
    pub size: i64,
    pub posted: Option<Timestamp>,
    pub usenet_date: Option<Timestamp>,
    pub poster: Option<String>,
    pub grabs: Option<i32>,
    pub password: Option<i32>,
    pub proxy_url: Option<String>,
    pub source_indexer_name: Option<String>,
    pub language: Option<String>,
    pub subs: Option<String>,
    pub info_hash: Option<String>,
    pub verified_available: bool,
}

/// v1 `NzbResolutionCache.Entry`.
#[derive(Debug, Clone)]
pub struct Entry {
    pub candidates: Arc<Vec<Candidate>>,
    pub start_index: usize,
    pub r#type: String,
    pub profile_token: String,
    pub id: String,
    pub created_at: Timestamp,
}

impl Entry {
    #[must_use]
    pub fn primary(&self) -> &Candidate {
        &self.candidates[self.start_index]
    }
}

#[derive(Debug, Default)]
pub struct ResolutionCache {
    entries: RwLock<HashMap<String, Entry>>,
}

impl ResolutionCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// v1 `AddGroupAsync` (in-memory part): one random 16-hex-char token per candidate.
    pub fn add_group(
        &self,
        candidates: Vec<Candidate>,
        r#type: &str,
        profile_token: &str,
        id: &str,
        now: Timestamp,
    ) -> Vec<String> {
        let candidates = Arc::new(candidates);
        let tokens: Vec<String> = (0..candidates.len()).map(|_| generate_token()).collect();
        let mut entries = self.entries.write().expect("lock");
        for (i, token) in tokens.iter().enumerate() {
            entries.insert(
                token.clone(),
                Entry {
                    candidates: Arc::clone(&candidates),
                    start_index: i,
                    r#type: r#type.to_owned(),
                    profile_token: profile_token.to_owned(),
                    id: id.to_owned(),
                    created_at: now,
                },
            );
        }
        tokens
    }

    #[must_use]
    pub fn get(&self, token: &str) -> Option<Entry> {
        self.entries.read().expect("lock").get(token).cloned()
    }

    /// Evicts expired in-memory entries (v1 `PurgeExpiredAsync` memory half).
    pub fn purge_expired(&self, ttl: Span, now: Timestamp) {
        let cutoff = now - ttl;
        let mut entries = self.entries.write().expect("lock");
        entries.retain(|_, e| e.created_at >= cutoff);
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.read().expect("lock").is_empty()
    }
}

/// v1 `GenerateToken`: 8 random bytes as lowercase hex (16 chars).
fn generate_token() -> String {
    use std::fmt::Write as _;
    let bytes = uuid::Uuid::new_v4();
    let b = bytes.as_bytes();
    let mut s = String::with_capacity(16);
    for byte in &b[..8] {
        write!(s, "{byte:02x}").expect("writing to a String is infallible");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(nzb_url: &str) -> Candidate {
        Candidate {
            indexer_name: "idx".into(),
            indexer_user_agent: "ua".into(),
            nzb_url: nzb_url.into(),
            title: "t".into(),
            size: 1,
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

    fn now() -> Timestamp {
        "2026-01-10T12:00:00Z".parse().expect("ts")
    }

    #[test]
    fn add_group_returns_one_token_per_candidate_with_start_indices() {
        let cache = ResolutionCache::new();
        let tokens = cache.add_group(
            vec![candidate("u1"), candidate("u2"), candidate("u3")],
            "series",
            "tok",
            "tt1:1:1",
            now(),
        );
        assert_eq!(tokens.len(), 3);
        for t in &tokens {
            assert_eq!(t.len(), 16);
            assert!(
                t.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
        }
        let distinct: std::collections::HashSet<&String> = tokens.iter().collect();
        assert_eq!(distinct.len(), 3);
        for (i, t) in tokens.iter().enumerate() {
            let e = cache.get(t).expect("entry");
            assert_eq!(e.start_index, i);
            assert_eq!(e.primary().nzb_url, format!("u{}", i + 1));
            assert_eq!(e.r#type, "series");
            assert_eq!(e.profile_token, "tok");
            assert_eq!(e.id, "tt1:1:1");
            assert_eq!(e.candidates.len(), 3);
        }
        assert!(cache.get("nope").is_none());
    }

    #[test]
    fn purge_expired_evicts_old_entries() {
        let cache = ResolutionCache::new();
        let t0 = now();
        let tokens = cache.add_group(vec![candidate("u1")], "movie", "tok", "tt1", t0);
        cache.purge_expired(Span::new().hours(1), t0 + Span::new().minutes(30));
        assert!(!cache.is_empty());
        cache.purge_expired(Span::new().hours(1), t0 + Span::new().hours(2));
        assert!(cache.is_empty());
        assert!(tokens.iter().all(|t| cache.get(t).is_none()));
    }
}
