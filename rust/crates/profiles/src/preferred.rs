//! Preferred stream order reported by addons and release identity keys, ported from
//! `backend/Services/PreferredOrderStore.cs`, `backend/Utils/ReleaseIdentity.cs` and
//! `backend/Utils/WardenFingerprint.cs` (v1 fork).

use std::collections::HashMap;
use std::sync::RwLock;

use jiff::Timestamp;
use sha2::{Digest, Sha256};

/// v1 `WardenFingerprint.Compute`: `"wd1:" + hex(sha256("{size}|{poster}|{dayBucket}")[:16])`,
/// or `None` when no fingerprint is possible (no positive size, neither poster nor date).
#[must_use]
pub fn warden_fingerprint(
    size: i64,
    poster: Option<&str>,
    usenet_date: Option<Timestamp>,
) -> Option<String> {
    let has_poster = poster.is_some_and(|p| !p.trim().is_empty());
    let has_date = usenet_date.is_some();
    if size <= 0 || (!has_poster && !has_date) {
        return None;
    }
    let poster_norm = if has_poster {
        poster.expect("checked").trim().to_lowercase()
    } else {
        String::new()
    };
    let day_bucket = usenet_date.map_or(0, |d| d.as_second() / 86_400);
    let canonical = format!("{size}|{poster_norm}|{day_bucket}");
    let hash = Sha256::digest(canonical.as_bytes());
    Some(format!("wd1:{}", hex_lower(&hash[..16])))
}

/// v1 `WardenFingerprint.Backbone`: lowercased host without port, or "unknown".
#[must_use]
pub fn backbone(host: Option<&str>) -> String {
    let Some(h) = host else {
        return "unknown".to_owned();
    };
    let h = h.trim().to_lowercase();
    let h = h.split(':').next().unwrap_or("");
    if h.is_empty() {
        "unknown".to_owned()
    } else {
        h.to_owned()
    }
}

/// v1 `WardenFingerprint.RootDomain`: public-suffix-style root domain (2 labels, or 3 for
/// 2-letter TLDs with a short second-level label), IPs verbatim, else "unknown".
#[must_use]
pub fn root_domain(host: Option<&str>) -> String {
    let Some(h) = host else {
        return "unknown".to_owned();
    };
    let h = h.trim().to_lowercase();
    let h = h.split(':').next().unwrap_or("").trim_matches('.');
    if h.is_empty() {
        return "unknown".to_owned();
    }
    if h.parse::<std::net::IpAddr>().is_ok() {
        return h.to_owned();
    }
    let labels: Vec<&str> = h.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 2 {
        return h.to_owned();
    }
    let take = if labels[labels.len() - 1].len() == 2 && labels[labels.len() - 2].len() <= 3 {
        3
    } else {
        2
    };
    if labels.len() <= take {
        h.to_owned()
    } else {
        labels[labels.len() - take..].join(".")
    }
}

/// v1 `ReleaseIdentity.Key`: the Warden fingerprint, else `"rk1:" + hex(sha256(nzbUrl)[:16])`.
#[must_use]
pub fn release_identity_key(
    size: i64,
    poster: Option<&str>,
    usenet_date: Option<Timestamp>,
    nzb_url: Option<&str>,
) -> String {
    if let Some(fp) = warden_fingerprint(size, poster, usenet_date) {
        return fp;
    }
    let hash = Sha256::digest(nzb_url.unwrap_or_default().as_bytes());
    format!("rk1:{}", hex_lower(&hash[..16]))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

type StoreKey = (String, String, String);
type StoreEntry = (Vec<String>, Timestamp);

/// v1 `PreferredOrderStore`: in-memory, 10-minute TTL, keys are `(profileToken, type, contentId)`.
#[derive(Debug, Default)]
pub struct PreferredOrderStore {
    entries: RwLock<HashMap<StoreKey, StoreEntry>>,
}

const TTL_SECS: i64 = 600;

impl PreferredOrderStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// v1 `Report`.
    pub fn report(
        &self,
        profile_token: &str,
        r#type: &str,
        content_id: &str,
        source: &str,
        ordered_keys: &[String],
    ) {
        let _ = source;
        if profile_token.trim().is_empty()
            || content_id.trim().is_empty()
            || ordered_keys.is_empty()
        {
            return;
        }
        let now = Timestamp::now();
        self.cleanup(now);
        self.entries.write().expect("lock").insert(
            (
                profile_token.to_owned(),
                r#type.to_owned(),
                content_id.to_owned(),
            ),
            (ordered_keys.to_vec(), now),
        );
    }

    /// v1 `GetOrder`.
    #[must_use]
    pub fn get_order(
        &self,
        profile_token: &str,
        r#type: &str,
        content_id: &str,
    ) -> Option<Vec<String>> {
        if profile_token.trim().is_empty() || content_id.trim().is_empty() {
            return None;
        }
        let key = (
            profile_token.to_owned(),
            r#type.to_owned(),
            content_id.to_owned(),
        );
        let entries = self.entries.read().expect("lock");
        match entries.get(&key) {
            Some((order, at)) if Timestamp::now().as_second() - at.as_second() <= TTL_SECS => {
                Some(order.clone())
            }
            _ => None,
        }
    }

    /// v1 `PreferredOrderStore.ApplyOrder`: matched items in reported rank (stable on ties),
    /// unmatched items keep their relative order afterwards.
    #[must_use]
    pub fn apply_order<T>(
        items: &[T],
        key_of: impl Fn(&T) -> String,
        order: Option<&[String]>,
    ) -> Vec<T>
    where
        T: Clone,
    {
        let Some(order) = order.filter(|o| !o.is_empty()) else {
            return items.to_vec();
        };
        let rank: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, k)| (k.as_str(), i))
            .collect();
        let mut matched: Vec<(usize, usize, T)> = Vec::new();
        let mut unmatched: Vec<T> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            if let Some(&r) = rank.get(key_of(item).as_str()) {
                matched.push((r, i, item.clone()));
            } else {
                unmatched.push(item.clone());
            }
        }
        matched.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut out = Vec::with_capacity(items.len());
        out.extend(matched.into_iter().map(|(_, _, item)| item));
        out.extend(unmatched);
        out
    }

    fn cleanup(&self, now: Timestamp) {
        let cutoff = now.as_second() - TTL_SECS;
        self.entries
            .write()
            .expect("lock")
            .retain(|_, (_, at)| at.as_second() > cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_v1_shape() {
        let date = Timestamp::from_second(1_700_000_000).expect("valid");
        assert_eq!(warden_fingerprint(0, None, Some(date)), None);
        assert_eq!(warden_fingerprint(-1, Some("P"), None), None);
        assert_eq!(warden_fingerprint(10, None, None), None);
        let fp = warden_fingerprint(123, Some(" Poster "), Some(date)).expect("fp");
        assert!(fp.starts_with("wd1:"), "{fp}");
        assert_eq!(fp.len(), "wd1:".len() + 32);
        let key = release_identity_key(0, None, None, Some("http://x/nzb"));
        assert!(key.starts_with("rk1:"), "{key}");
    }

    #[test]
    fn apply_order_ranks_matched_and_keeps_unmatched() {
        let items = vec!["a", "b", "c", "d"];
        let keys: Vec<String> = items.iter().map(|s| (*s).to_owned()).collect();
        let order = vec!["c".to_owned(), "a".to_owned()];
        let out = PreferredOrderStore::apply_order(&items, |s| (*s).to_owned(), Some(&order));
        assert_eq!(out, vec!["c", "a", "b", "d"]);
        let _ = keys;
    }

    #[test]
    fn report_and_get_roundtrip_with_expiry() {
        let store = PreferredOrderStore::new();
        store.report(
            "tok",
            "movie",
            "tt1",
            "aiostreams",
            &["k1".to_owned(), "k2".to_owned()],
        );
        assert_eq!(
            store.get_order("tok", "movie", "tt1").as_deref(),
            Some(&["k1".to_owned(), "k2".to_owned()][..])
        );
        assert!(store.get_order("tok", "movie", "tt2").is_none());
        assert!(store.get_order("", "movie", "tt1").is_none());
        store.report("tok", "movie", "tt1", "aiostreams", &[]);
        assert_eq!(
            store.get_order("tok", "movie", "tt1").as_deref(),
            Some(&["k1".to_owned(), "k2".to_owned()][..])
        );
    }
}
