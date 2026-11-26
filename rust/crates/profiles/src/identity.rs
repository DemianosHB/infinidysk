//! Strict identity matching for profile results, ported from
//! `backend/Utils/ProfileIdentityFilter.cs` (v1 fork). Indexers flagged
//! `enable_strict_matching` only contribute results whose title matches the canonical
//! identity (or the consensus head when no canonical metadata could be resolved).

use std::collections::HashSet;

use crate::filename;

/// v1 `ProfileIdentityFilter.ExpectedIdentity`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpectedIdentity {
    pub normalized_titles: HashSet<String>,
    pub movie_year: Option<i32>,
}

impl ExpectedIdentity {
    #[must_use]
    pub fn new(normalized_titles: HashSet<String>, movie_year: Option<i32>) -> Self {
        Self {
            normalized_titles,
            movie_year,
        }
    }
}

#[must_use]
pub fn has_canonical_identity(identity: &ExpectedIdentity) -> bool {
    !identity.normalized_titles.is_empty() || identity.movie_year.is_some()
}

/// v1 `ApplyStrictMatching`. Items from non-strict indexers always pass; strict-indexer
/// items must match the canonical identity, or (when no canonical identity resolved and
/// there are at least two results) the majority head-token consensus.
#[expect(
    clippy::implicit_hasher,
    reason = "v1 uses ordinal string sets; the default hasher is fine"
)]
#[must_use]
pub fn apply_strict_matching<T: Clone>(
    items: &[T],
    indexer_name: impl Fn(&T) -> &str,
    title: impl Fn(&T) -> Option<&str>,
    strict_indexers: &HashSet<String>,
    identity: &ExpectedIdentity,
    apply_movie_year: bool,
) -> Vec<T> {
    if strict_indexers.is_empty() {
        return items.to_vec();
    }
    if has_canonical_identity(identity) {
        return items
            .iter()
            .filter(|item| {
                !strict_indexers.contains(indexer_name(item))
                    || matches_canonical(title(item), identity, apply_movie_year)
            })
            .cloned()
            .collect();
    }
    // Consensus fallback: only when canonical metadata could not be resolved, and it
    // still requires at least two results before it will reject anything.
    if items.len() < 2 {
        return items.to_vec();
    }
    let with_head: Vec<(&T, Vec<String>)> = items
        .iter()
        .map(|item| (item, filename::head_tokens(title(item))))
        .collect();
    // Group by joined head in first-seen order. LINQ's stable OrderByDescending keeps the
    // first group among ties, so pick the earliest group at the maximum count explicitly.
    let mut groups: Vec<(String, Vec<String>, usize)> = Vec::new();
    for (_, head) in &with_head {
        if head.is_empty() {
            continue;
        }
        let key = head.join(" ");
        if let Some(pos) = groups.iter().position(|(k, _, _)| *k == key) {
            groups[pos].2 += 1;
        } else {
            groups.push((key, head.clone(), 1));
        }
    }
    let max_count = groups.iter().map(|g| g.2).filter(|c| *c >= 2).max();
    let consensus = match max_count {
        Some(mc) => groups.into_iter().find(|(_, _, c)| *c == mc),
        None => None,
    };
    let Some((_, consensus_head, _)) = consensus else {
        return items.to_vec();
    };
    with_head
        .into_iter()
        .filter(|(item, head)| {
            !strict_indexers.contains(indexer_name(item))
                || filename::tokens_equal(head, &consensus_head)
        })
        .map(|(item, _)| item.clone())
        .collect()
}

fn matches_canonical(
    release_title: Option<&str>,
    identity: &ExpectedIdentity,
    apply_movie_year: bool,
) -> bool {
    if !identity.normalized_titles.is_empty()
        && !filename::title_matches(&identity.normalized_titles, release_title)
    {
        return false;
    }
    !apply_movie_year
        || filename::year_compatible(
            identity.movie_year,
            release_title,
            Some(&identity.normalized_titles),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/NzbWebDAV.Tests/Utils/ProfileIdentityFilterTests.cs.

    fn strict() -> HashSet<String> {
        HashSet::from(["strict".to_owned()])
    }

    fn identity(title: &str, year: Option<i32>) -> ExpectedIdentity {
        ExpectedIdentity::new(
            HashSet::from([filename::normalize_title(Some(title))]),
            year,
        )
    }

    fn apply(
        items: &[Hit],
        identity: &ExpectedIdentity,
        apply_movie_year: bool,
        strict_indexers: Option<&HashSet<String>>,
    ) -> Vec<Hit> {
        let default = strict();
        apply_strict_matching(
            items,
            |x| &x.indexer,
            |x| Some(x.title.as_str()),
            strict_indexers.unwrap_or(&default),
            identity,
            apply_movie_year,
        )
    }

    fn result(indexer: &str, title: &str) -> Hit {
        Hit {
            indexer: indexer.to_owned(),
            title: title.to_owned(),
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Hit {
        indexer: String,
        title: String,
    }

    #[test]
    fn strict_indexer_removes_wrong_year_movie() {
        let id = identity("Dune", Some(2024));
        let kept = apply(
            &[
                result("strict", "Dune.2024.2160p.WEB-DL"),
                result("strict", "Dune.1984.1080p.BluRay"),
            ],
            &id,
            true,
            None,
        );
        assert_eq!(
            kept.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(),
            ["Dune.2024.2160p.WEB-DL"]
        );
    }

    #[test]
    fn non_strict_indexer_retains_wrong_year_movie() {
        let id = identity("Dune", Some(2024));
        let kept = apply(
            &[result("open", "Dune.1984.1080p.BluRay")],
            &id,
            true,
            Some(&strict()),
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn sole_wrong_year_strict_result_is_removed_when_canonical_year_exists() {
        let id = identity("Dune", Some(2024));
        let kept = apply(
            &[result("strict", "Dune.1984.1080p.BluRay")],
            &id,
            true,
            None,
        );
        assert_eq!(kept.len(), 0);
    }

    #[test]
    fn resolver_failure_leaves_results_intact() {
        let empty = ExpectedIdentity::new(HashSet::new(), None);
        let items = [result("strict", "Dune.1984.1080p.BluRay")];
        let kept = apply(&items, &empty, true, None);
        assert_eq!(kept, items);
    }

    #[test]
    fn tv_results_are_not_rejected_by_show_premiere_year() {
        let id = ExpectedIdentity::new(
            HashSet::from([filename::normalize_title(Some("The Bear"))]),
            Some(2022),
        );
        let kept = apply(
            &[result("strict", "The.Bear.S03E01.2024.1080p.WEB-DL")],
            &id,
            false,
            None,
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn non_strict_indexer_passes_through_unchanged() {
        let id = identity("Dune", Some(2024));
        let items = [
            result("open", "Completely.Different.1999.1080p"),
            result("strict", "Dune.2024.1080p"),
        ];
        let kept = apply(&items, &id, true, None);
        assert_eq!(
            kept.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(),
            ["Completely.Different.1999.1080p", "Dune.2024.1080p"]
        );
    }

    #[test]
    fn consensus_fallback_requires_two_results_and_majority_head() {
        let empty = ExpectedIdentity::new(HashSet::new(), None);
        let strict_set = strict();
        // Two "Dune..." heads from the strict indexer form the consensus; the outlier drops.
        let kept = apply(
            &[
                result("strict", "Dune.2024.1080p.WEB"),
                result("strict", "Dune.2024.2160p.BluRay"),
                result("strict", "Something.Else.2020.1080p"),
            ],
            &empty,
            true,
            Some(&strict_set),
        );
        assert_eq!(kept.len(), 2);
        // A single strict result is never rejected by consensus.
        let kept_one = apply(
            &[result("strict", "Something.Else.2020.1080p")],
            &empty,
            true,
            Some(&strict_set),
        );
        assert_eq!(kept_one.len(), 1);
    }
}
