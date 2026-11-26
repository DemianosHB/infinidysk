//! Per-indexer result filtering, ported from `backend/Services/IndexerResultFilter.cs`.
//! All rules are opt-in; when the filter is absent or disabled the input is returned
//! untouched. Rules that cannot evaluate due to missing attributes fail open (the item is
//! kept).

use jiff::{SignedDuration, Timestamp};

use infinidysk_search::result::SearchResult;

use crate::models::ResultFilter;

/// Applies the drop rules and (when configured) sorts by grabs descending. Stable: ties
/// keep the indexer's original order; missing grabs coerce to -1 so they sort below honest
/// 0-grab items.
#[must_use]
pub fn apply(
    items: &[SearchResult],
    filter: Option<&ResultFilter>,
    now: Timestamp,
) -> Vec<SearchResult> {
    let Some(filter) = filter else {
        return items.to_vec();
    };
    if !filter.enabled || items.is_empty() {
        return items.to_vec();
    }
    let mut kept: Vec<SearchResult> = items
        .iter()
        .filter(|item| !should_drop(item, filter, now))
        .cloned()
        .collect();
    if filter.prefer_downloaded && kept.len() > 1 {
        kept.sort_by_key(|x| std::cmp::Reverse(x.grabs.unwrap_or(-1)));
    }
    kept
}

fn should_drop(item: &SearchResult, f: &ResultFilter, now: Timestamp) -> bool {
    // 1. Passworded — drop only when the indexer explicitly says password != 0; a missing
    //    attribute is "unknown, keep".
    if f.skip_passworded && item.password.is_some_and(|p| p > 0) {
        return true;
    }

    let age_hours = effective_age(item, now).map(|a| a.as_secs_f64() / 3600.0);

    // 2. MaxAgeDaysWithoutGrabs — drop old releases with literally zero recorded grabs.
    //    Requires both a known age and known grabs.
    if f.max_age_days_without_grabs > 0
        && age_hours.is_some_and(|h| h >= f64::from(f.max_age_days_without_grabs) * 24.0)
        && item.grabs == Some(0)
    {
        return true;
    }

    // 3. MinGrabs — drop below the threshold, but only after the grace window; unknown age
    //    is treated as within the grace window.
    if f.min_grabs > 0
        && let Some(g) = item.grabs
        && g < i32::try_from(f.min_grabs).unwrap_or(i32::MAX)
    {
        if f.grabs_grace_hours > 0 && age_hours.is_none_or(|h| h < f64::from(f.grabs_grace_hours)) {
            return false;
        }
        return true;
    }

    false
}

/// Prefers the indexer-asserted `usenet_date` over `posted`; falls back to `posted`;
/// `None` when neither is present or the age would be negative (clock skew clamps to zero).
fn effective_age(item: &SearchResult, now: Timestamp) -> Option<SignedDuration> {
    let reference = item.usenet_date.or(item.posted)?;
    let age = now - reference;
    Some(if age.is_negative() {
        SignedDuration::ZERO
    } else {
        age
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Span;

    fn item(grabs: Option<i32>, password: Option<i32>, posted: Option<Timestamp>) -> SearchResult {
        SearchResult {
            title: "t".into(),
            guid: "g".into(),
            nzb_url: "u".into(),
            size: 1,
            posted,
            usenet_date: None,
            grabs,
            comments: None,
            password,
            files: None,
            group: None,
            poster: None,
            source_indexer_name: None,
            language: None,
            subs: None,
            info_hash: None,
        }
    }

    fn now() -> Timestamp {
        "2026-01-10T12:00:00Z".parse().unwrap()
    }

    fn hours_ago(h: i64) -> Timestamp {
        now() - Span::new().hours(h)
    }

    #[test]
    fn disabled_or_missing_filter_returns_input_untouched() {
        let items = vec![item(Some(1), Some(2), None)];
        assert_eq!(apply(&items, None, now()), items);
        let f = ResultFilter {
            enabled: false,
            skip_passworded: true,
            ..Default::default()
        };
        assert_eq!(apply(&items, Some(&f), now()), items);
    }

    #[test]
    fn skip_passworded_drops_only_explicit_nonzero() {
        let f = ResultFilter {
            enabled: true,
            skip_passworded: true,
            ..Default::default()
        };
        let items = vec![
            item(Some(0), Some(2), None),
            item(Some(0), Some(0), None),
            item(Some(0), None, None),
        ];
        let kept = apply(&items, Some(&f), now());
        assert_eq!(kept.len(), 2, "passworded dropped, zero/unknown kept");
    }

    #[test]
    fn min_grabs_respects_grace_window_and_unknown_age() {
        let f = ResultFilter {
            enabled: true,
            min_grabs: 5,
            grabs_grace_hours: 6,
            ..Default::default()
        };
        let fresh = item(Some(1), None, Some(hours_ago(1))); // within grace -> kept
        let old = item(Some(1), None, Some(hours_ago(48))); // past grace -> dropped
        let unknown_age = item(Some(1), None, None); // unknown age -> within grace -> kept
        let enough = item(Some(9), None, Some(hours_ago(48)));
        let unknown_grabs = item(None, None, Some(hours_ago(48))); // rule can't evaluate -> kept
        let items = [fresh, old, unknown_age, enough, unknown_grabs];
        let kept = apply(&items, Some(&f), now());
        assert_eq!(kept.len(), 4);
        assert!(kept.contains(&items[0]));
        assert!(!kept.contains(&items[1]));
        assert!(kept.contains(&items[2]));
        assert!(kept.contains(&items[3]));
        assert!(kept.contains(&items[4]));
    }

    #[test]
    fn min_grabs_without_grace_applies_to_everything() {
        let f = ResultFilter {
            enabled: true,
            min_grabs: 5,
            grabs_grace_hours: 0,
            ..Default::default()
        };
        let fresh_low = item(Some(1), None, Some(hours_ago(1)));
        let kept = apply(&[fresh_low], Some(&f), now());
        assert!(kept.is_empty(), "grace disabled -> dropped even when fresh");
    }

    #[test]
    fn max_age_without_grabs_drops_old_zero_grab_only() {
        let f = ResultFilter {
            enabled: true,
            max_age_days_without_grabs: 7,
            ..Default::default()
        };
        let old_zero = item(Some(0), None, Some(hours_ago(24 * 10)));
        let old_some = item(Some(3), None, Some(hours_ago(24 * 10)));
        let new_zero = item(Some(0), None, Some(hours_ago(2)));
        let unknown_age_zero = item(Some(0), None, None);
        let unknown_grabs_old = item(None, None, Some(hours_ago(24 * 10)));
        let kept = apply(
            &[
                old_zero.clone(),
                old_some,
                new_zero,
                unknown_age_zero,
                unknown_grabs_old,
            ],
            Some(&f),
            now(),
        );
        assert_eq!(kept.len(), 4);
        assert!(!kept.contains(&old_zero));
    }

    #[test]
    fn usenet_date_is_preferred_over_posted_for_age() {
        let f = ResultFilter {
            enabled: true,
            max_age_days_without_grabs: 7,
            ..Default::default()
        };
        let mut it = item(Some(0), None, Some(hours_ago(2))); // posted recently
        it.usenet_date = Some(hours_ago(24 * 30)); // but on usenet for a month
        let kept = apply(&[it], Some(&f), now());
        assert!(kept.is_empty(), "usenetdate drives the age");
    }

    #[test]
    fn prefer_downloaded_sorts_grabs_desc_with_missing_at_bottom() {
        let filter = ResultFilter {
            enabled: true,
            prefer_downloaded: true,
            ..Default::default()
        };
        let one_grab = item(Some(1), None, None);
        let nine_grabs = item(Some(9), None, None);
        let unknown_grabs = item(None, None, None);
        let zero_grabs = item(Some(0), None, None);
        let kept = apply(
            &[one_grab, nine_grabs, unknown_grabs, zero_grabs],
            Some(&filter),
            now(),
        );
        let grabs: Vec<Option<i32>> = kept.iter().map(|x| x.grabs).collect();
        assert_eq!(grabs, vec![Some(9), Some(1), Some(0), None]);
    }
}
