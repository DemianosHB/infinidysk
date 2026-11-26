//! Profile result ordering, ported from `backend/Utils/ProfileResultSorter.cs` and
//! `ProfileConfig.QualitySortMode`. Order: language bias first, then resolution and
//! (optionally) source, then grabs (optional), then size, then posted date. Rust's
//! `sort_by` is stable, matching LINQ's stable `OrderBy`/`ThenBy` chain, so full ties
//! keep the input order.

use std::cmp::Reverse;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::audio;
use crate::quality::{self, ReleaseQualityRanks};

/// Ported from `ProfileConfig.QualitySortMode` (JSON string enum in v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
pub enum QualitySortMode {
    #[default]
    #[serde(rename = "Off")]
    Off,
    #[serde(rename = "Resolution")]
    Resolution,
    #[serde(rename = "ResolutionAndSource")]
    ResolutionAndSource,
}

/// Accessor for an item's indexer language attribute.
type LanguageOf<'a, T> = Option<&'a dyn Fn(&T) -> Option<&str>>;

struct Ranked<T> {
    item: T,
    language_rank: usize,
    resolution: i32,
    source: i32,
    grabs: i32,
    size: i64,
    posted: Timestamp,
}

/// Sorts release results for a profile. The language bias is an ordering bias, never a
/// filter: with a non-empty `preferred_audio_languages` and a `language` accessor, a
/// release matching an earlier preferred entry ranks above all non-matches; non-matches
/// share the last rank and keep the quality ordering. `grabs` only participates when
/// `prefer_downloaded` is set; a missing posted date counts as the smallest.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the v1 ProfileResultSorter.Sort parameter list; the per-field accessors are the port's spec"
)]
#[must_use]
pub fn sort<T>(
    items: impl IntoIterator<Item = T>,
    title: &dyn Fn(&T) -> Option<&str>,
    grabs: &dyn Fn(&T) -> Option<i32>,
    size: &dyn Fn(&T) -> i64,
    posted: &dyn Fn(&T) -> Option<Timestamp>,
    mode: QualitySortMode,
    prefer_downloaded: bool,
    language: LanguageOf<'_, T>,
    preferred_audio_languages: &[String],
) -> Vec<T> {
    let mut ranked: Vec<Ranked<T>> = items
        .into_iter()
        .map(|item| {
            let quality = if mode == QualitySortMode::Off {
                ReleaseQualityRanks::UNKNOWN
            } else {
                quality::parse(title(&item))
            };
            let language_rank = match (language, preferred_audio_languages.is_empty()) {
                (Some(language), false) => {
                    audio::rank(preferred_audio_languages, language(&item), title(&item))
                }
                _ => 0,
            };
            Ranked {
                language_rank,
                resolution: if mode == QualitySortMode::Off {
                    0
                } else {
                    quality.resolution
                },
                source: if mode == QualitySortMode::ResolutionAndSource {
                    quality.source
                } else {
                    0
                },
                grabs: if prefer_downloaded {
                    grabs(&item).unwrap_or(-1)
                } else {
                    0
                },
                size: size(&item),
                posted: posted(&item).unwrap_or(Timestamp::MIN),
                item,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        (
            a.language_rank,
            Reverse(a.resolution),
            Reverse(a.source),
            Reverse(a.grabs),
            Reverse(a.size),
            Reverse(a.posted),
        )
            .cmp(&(
                b.language_rank,
                Reverse(b.resolution),
                Reverse(b.source),
                Reverse(b.grabs),
                Reverse(b.size),
                Reverse(b.posted),
            ))
    });

    ranked.into_iter().map(|r| r.item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Release {
        title: &'static str,
        grabs: Option<i32>,
        size: i64,
        posted: Option<Timestamp>,
        language: Option<&'static str>,
    }

    impl Release {
        fn new(title: &'static str, grabs: i32, size: i64, posted: Option<Timestamp>) -> Self {
            Self {
                title,
                grabs: Some(grabs),
                size,
                posted,
                language: None,
            }
        }

        fn with_language(
            title: &'static str,
            grabs: i32,
            size: i64,
            posted: Option<Timestamp>,
            language: &'static str,
        ) -> Self {
            Self {
                language: Some(language),
                ..Self::new(title, grabs, size, posted)
            }
        }
    }

    const EPOCH: Option<Timestamp> = Some(Timestamp::UNIX_EPOCH);

    fn epoch_plus_day() -> Option<Timestamp> {
        Timestamp::from_second(86_400).ok()
    }

    fn sort_basic(
        items: Vec<Release>,
        mode: QualitySortMode,
        prefer_downloaded: bool,
    ) -> Vec<&'static str> {
        sort(
            items,
            &|r| Some(r.title),
            &|r| r.grabs,
            &|r| r.size,
            &|r| r.posted,
            mode,
            prefer_downloaded,
            None,
            &[],
        )
        .into_iter()
        .map(|r| r.title)
        .collect()
    }

    fn sort_biased(
        items: Vec<Release>,
        mode: QualitySortMode,
        preferred: &[&str],
        prefer_downloaded: bool,
    ) -> Vec<&'static str> {
        let preferred: Vec<String> = preferred.iter().map(|s| (*s).to_owned()).collect();
        sort(
            items,
            &|r| Some(r.title),
            &|r| r.grabs,
            &|r| r.size,
            &|r| r.posted,
            mode,
            prefer_downloaded,
            Some(&|r| r.language),
            &preferred,
        )
        .into_iter()
        .map(|r| r.title)
        .collect()
    }

    #[test]
    fn sort_off_reproduces_grabs_size_date_order() {
        // Ported from ProfileResultSorterTests.Sort_Off_ReproducesGrabsSizeDateOrder.
        let items = vec![
            Release::new("Movie.2024.2160p.WEB-DL", 1, 10, EPOCH),
            Release::new("Movie.2024.1080p.BluRay", 50, 20, epoch_plus_day()),
            Release::new("Movie.2024.720p.WEB-DL", 50, 30, EPOCH),
        ];

        let sorted = sort_basic(items, QualitySortMode::Off, true);

        assert_eq!(
            sorted,
            vec![
                "Movie.2024.720p.WEB-DL",
                "Movie.2024.1080p.BluRay",
                "Movie.2024.2160p.WEB-DL"
            ]
        );
    }

    #[test]
    fn sort_resolution_ignores_source_rank() {
        // Ported from ProfileResultSorterTests.Sort_Resolution_IgnoresSourceRank.
        let items = vec![
            Release::new("Movie.2024.1080p.REMUX", 100, 50, EPOCH),
            Release::new("Movie.2024.2160p.HDTV", 1, 10, EPOCH),
            Release::new("Movie.2024.1080p.WEB-DL", 500, 40, EPOCH),
        ];

        let sorted = sort_basic(items, QualitySortMode::Resolution, true);

        assert_eq!(
            sorted,
            vec![
                "Movie.2024.2160p.HDTV",
                "Movie.2024.1080p.WEB-DL",
                "Movie.2024.1080p.REMUX"
            ]
        );
    }

    #[test]
    fn sort_resolution_and_source_applies_source_only_within_resolution() {
        // Ported from ProfileResultSorterTests.Sort_ResolutionAndSource_AppliesSourceOnlyWithinResolution.
        let items = vec![
            Release::new("Movie.2024.1080p.REMUX", 100, 50, EPOCH),
            Release::new("Movie.2024.2160p.HDTV", 1, 10, EPOCH),
            Release::new("Movie.2024.1080p.WEB-DL", 500, 40, EPOCH),
        ];

        let sorted = sort_basic(items, QualitySortMode::ResolutionAndSource, false);

        assert_eq!(
            sorted,
            vec![
                "Movie.2024.2160p.HDTV",
                "Movie.2024.1080p.REMUX",
                "Movie.2024.1080p.WEB-DL"
            ]
        );
    }

    #[test]
    fn sort_fallback_merged_results_use_the_same_ordering() {
        // Ported from ProfileResultSorterTests.Sort_FallbackMergedResults_UseTheSameOrdering.
        let mut items = vec![Release::new("Movie.2024.720p.WEB-DL", 1, 10, EPOCH)];
        items.extend([
            Release::new("Movie.2024.2160p.WEB-DL", 1, 10, EPOCH),
            Release::new("Movie.2024.1080p.BluRay", 1, 20, EPOCH),
        ]);

        let sorted = sort_basic(items, QualitySortMode::ResolutionAndSource, false);

        assert_eq!(
            sorted,
            vec![
                "Movie.2024.2160p.WEB-DL",
                "Movie.2024.1080p.BluRay",
                "Movie.2024.720p.WEB-DL"
            ]
        );
    }

    #[test]
    fn sort_does_not_reorder_after_watchtower_boost() {
        // Ported from ProfileResultSorterTests.Sort_DoesNotReorderAfterWatchtowerBoost.
        let ordinary = sort_basic(
            vec![
                Release::new("Movie.2024.1080p.WEB-DL", 1, 10, EPOCH),
                Release::new("Movie.2024.2160p.WEB-DL", 1, 10, EPOCH),
            ],
            QualitySortMode::Resolution,
            false,
        );

        let watchtower_first = Release::new("Movie.2024.720p.HDTV", 1, 5, EPOCH);
        let mut merged = vec![watchtower_first.title];
        merged.extend(ordinary);

        assert_eq!(merged[0], "Movie.2024.720p.HDTV");
        assert_eq!(merged[1], "Movie.2024.2160p.WEB-DL");
    }

    #[test]
    fn sort_language_bias_puts_preferred_language_above_better_quality() {
        // Ported from ProfileResultSorterTests.Sort_LanguageBias_PutsPreferredLanguageAboveBetterQuality.
        let items = vec![
            Release::new("Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP", 50, 30, EPOCH),
            Release::new("Movie.2026.GERMAN.WEB-DL.1080p.x264-DE", 5, 10, EPOCH),
        ];

        let sorted = sort_biased(items, QualitySortMode::ResolutionAndSource, &["de"], false);

        assert_eq!(
            sorted,
            vec![
                "Movie.2026.GERMAN.WEB-DL.1080p.x264-DE",
                "Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP"
            ]
        );
    }

    #[test]
    fn sort_language_bias_indexer_attribute_beats_title_tokens() {
        // Ported from ProfileResultSorterTests.Sort_LanguageBias_IndexerAttributeBeatsTitleTokens.
        let items = vec![
            Release::with_language(
                "Movie.2026.GERMAN.2160p.WEB-DL-GRP",
                50,
                30,
                EPOCH,
                "Português (Brasil)",
            ),
            Release::with_language("Movie.2026.GERMAN.1080p.WEB-DL-DE2", 5, 10, EPOCH, "German"),
        ];

        let sorted = sort_biased(items, QualitySortMode::ResolutionAndSource, &["de"], false);

        assert_eq!(sorted[0], "Movie.2026.GERMAN.1080p.WEB-DL-DE2");
    }

    #[test]
    fn sort_language_bias_tie_breaks_among_matches_by_resolution() {
        // Ported from ProfileResultSorterTests.Sort_LanguageBias_TieBreaksAmongMatchesByResolution.
        let items = vec![
            Release::new("Movie.2026.GERMAN.1080p.WEB-DL", 1, 10, EPOCH),
            Release::new("Movie.2026.GERMAN.2160p.WEB-DL", 1, 20, EPOCH),
        ];

        let sorted = sort_biased(items, QualitySortMode::ResolutionAndSource, &["de"], false);

        assert_eq!(sorted[0], "Movie.2026.GERMAN.2160p.WEB-DL");
    }

    #[test]
    fn sort_language_bias_unknown_language_keeps_quality_order() {
        // Ported from ProfileResultSorterTests.Sort_LanguageBias_UnknownLanguageKeepsQualityOrder.
        let items = vec![
            Release::new("Movie.2026.MULTi.1080p.BluRay", 1, 10, EPOCH),
            Release::new("Movie.2026.PT-BR.2160p.BluRay", 1, 20, EPOCH),
        ];

        let sorted = sort_biased(items, QualitySortMode::ResolutionAndSource, &["de"], false);

        assert_eq!(sorted[0], "Movie.2026.PT-BR.2160p.BluRay");
    }

    #[test]
    fn sort_language_bias_applies_even_when_quality_sort_is_off() {
        // Ported from ProfileResultSorterTests.Sort_LanguageBias_AppliesEvenWhenQualitySortIsOff.
        let items = vec![
            Release::new("Movie.2026.PT-BR.2160p.BluRay", 1, 10, EPOCH),
            Release::new("Movie.2026.DL.1080p.WEB-DL", 1, 10, EPOCH),
        ];

        let sorted = sort_biased(items, QualitySortMode::Off, &["de", "en"], false);

        assert_eq!(sorted[0], "Movie.2026.DL.1080p.WEB-DL");
    }

    #[test]
    fn sort_without_preferred_languages_keeps_quality_order() {
        // Ported from ProfileResultSorterTests.Sort_WithoutPreferredLanguages_KeepsQualityOrder.
        let items = vec![
            Release::with_language(
                "Movie.2026.GERMAN.WEB-DL.1080p.x264-DE",
                5,
                10,
                EPOCH,
                "German",
            ),
            Release::new("Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP", 50, 30, EPOCH),
        ];
        let preferred: Vec<String> = Vec::new();

        let sorted = sort(
            items,
            &|r| Some(r.title),
            &|r| r.grabs,
            &|r| r.size,
            &|r| r.posted,
            QualitySortMode::ResolutionAndSource,
            false,
            Some(&|r| r.language),
            &preferred,
        );

        assert_eq!(sorted[0].title, "Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP");
    }
}
