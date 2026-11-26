//! Audio-language detection and the preferred-language ordering bias, ported from
//! `backend/Utils/AudioLanguagePreference.cs` (the v1 fork's "preferred audio languages"
//! feature). An ordering bias, never a filter: releases with no confidently detected
//! language never get a boost, but are never demoted below other non-matches.

use std::collections::HashSet;
use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::language::parse_languages;

// Token-boundary guards ported from the AIOStreams/Torrentio convention: a language
// token must not be preceded by a word character and must be followed by a separator
// or end of string, so "ger" inside "Stranger" or "dl" inside "DLNA" never matches.
const TOKEN_GUARD_PREFIX: &str = r"(?<![^\s\[(_\-.,])";
const TOKEN_GUARD_SUFFIX: &str = r"(?=[\s\[()\]_.\-,]|$)";
const SUBTITLE_MARKER_EXCLUSION: &str = r"(?![ .\-_]?sub(title)?s?)";

/// German-Usenet convention: a bare "DL" token marks a dual-language release with German
/// audio. The WEB-DL source tag is neutralised first so the source is never mistaken
/// for the token.
static WEB_DL_SOURCE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)web[ .\-_]?dl").expect("valid regex"));

static DUAL_LANGUAGE_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        "(?i){TOKEN_GUARD_PREFIX}dl{SUBTITLE_MARKER_EXCLUSION}{TOKEN_GUARD_SUFFIX}"
    ))
    .expect("valid regex")
});

/// Release-name token patterns per canonical language, ported from the `AIOStreams`
/// language table.
const TITLE_PATTERNS: &[(&str, &str)] = &[
    ("en", r"english|eng"),
    ("de", r"deu(tsch)?(land)?|ger(man)?"),
    ("fr", r"french|fra|fr|vf|vff|vfi|vf2|vfq|truefrench"),
    ("es", r"spanish|spa|esp|latino"),
    ("it", r"italian|ita"),
    ("pt", r"portuguese[ .\-_]?brazil|pt[ .\-_]?br"),
    ("pt", r"portuguese"),
    ("nl", r"dutch|dut"),
    ("ru", r"russian|rus"),
    ("ja", r"japanese|jap|jpn"),
    ("ko", r"korean|kor"),
    ("zh", r"chinese|chi"),
    ("ar", r"arabic|ara"),
    ("hi", r"hindi|hin"),
    ("tr", r"turkish|tur"),
    ("swe", r"swedish|swe"),
    ("nor", r"norwegian|nor"),
    ("dan", r"danish|dan"),
    ("fin", r"finnish|fin"),
    ("pol", r"polish|pol"),
    ("bn", r"bengali|ben(?![ .\-_]?the[ .\-_]?men)"),
    ("pa", r"punjabi|pan"),
    ("mr", r"marathi|mar"),
    ("gu", r"gujarati|guj"),
    ("ta", r"tamil|tam"),
    ("te", r"telugu|tel"),
    ("kn", r"kannada|kan"),
    ("ml", r"malayalam|mal"),
    ("th", r"thai|tha"),
    ("vi", r"vietnamese|vie"),
    ("id", r"indonesian|ind"),
    ("he", r"hebrew|heb"),
    ("fa", r"persian|per"),
    ("uk", r"ukrainian|ukr"),
    ("el", r"greek|ell"),
    ("lt", r"lithuanian|lit"),
    ("lv", r"latvian|lav"),
    ("et", r"estonian|est"),
    ("cs", r"czech|cze"),
    ("sk", r"slovak|slo"),
    ("hu", r"hungarian|hun"),
    ("ro", r"romanian|rum"),
    ("bg", r"bulgarian|bul"),
    ("sr", r"serbian|srp"),
    ("hr", r"croatian|hrv"),
    ("sl", r"slovenian|slv"),
    ("ms", r"malay"),
];

static TITLE_TOKEN_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    let mut combined: Vec<(&str, Vec<&str>)> = Vec::new();
    for (canonical, pattern) in TITLE_PATTERNS {
        if let Some((_, list)) = combined.iter_mut().find(|(c, _)| *c == *canonical) {
            list.push(pattern);
        } else {
            combined.push((canonical, vec![pattern]));
        }
    }
    combined
        .into_iter()
        .map(|(canonical, alts)| {
            let pattern = format!(
                "{TOKEN_GUARD_PREFIX}(?:{}){SUBTITLE_MARKER_EXCLUSION}{TOKEN_GUARD_SUFFIX}",
                alts.join("|")
            );
            (
                canonical,
                Regex::new(&format!("(?i){pattern}")).expect("valid regex"),
            )
        })
        .collect()
});

/// Two-letter attribute tokens are only confident when they are known ISO codes;
/// indexers sometimes emit junk like "n/a" which splits into single letters.
const KNOWN_TWO_LETTER_TOKENS: &[&str] = &[
    "en", "es", "fr", "de", "it", "pt", "nl", "ru", "ja", "ko", "zh", "ar", "hi",
];

const NOISE_TOKENS: &[&str] = &[
    "unknown",
    "none",
    "other",
    "original",
    "noidentity",
    "nolanguage",
];

fn parse_attribute_languages(indexer_language: Option<&str>) -> HashSet<String> {
    parse_languages(indexer_language)
        .into_iter()
        .filter(|token| {
            (token.len() > 2 || KNOWN_TWO_LETTER_TOKENS.contains(&token.as_str()))
                && !NOISE_TOKENS.contains(&token.as_str())
        })
        .collect()
}

/// Detects the audio languages of a release. The indexer's language attribute wins when
/// it yields a confident language; otherwise the release name is parsed. Empty when
/// neither yields a confident language (fail-open).
#[must_use]
pub fn detect(indexer_language: Option<&str>, title: Option<&str>) -> HashSet<String> {
    let from_attribute = parse_attribute_languages(indexer_language);
    if !from_attribute.is_empty() {
        return from_attribute;
    }
    parse_title_languages(title)
}

/// Parses release-name tokens into canonical language tokens. MULTI/DUAL AUDIO/DUBBED
/// markers are deliberately not mapped: they do not state which languages are present.
#[must_use]
pub fn parse_title_languages(title: Option<&str>) -> HashSet<String> {
    let mut result = HashSet::new();
    let Some(title) = title else { return result };
    if title.trim().is_empty() {
        return result;
    }
    let normalized = WEB_DL_SOURCE_TAG.replace_all(title, "web");
    for (canonical, regex) in TITLE_TOKEN_PATTERNS.iter() {
        if regex.is_match(&normalized).expect("valid regex") {
            result.insert((*canonical).to_owned());
        }
    }
    if DUAL_LANGUAGE_TOKEN
        .is_match(&normalized)
        .expect("valid regex")
    {
        result.insert("de".to_owned());
        result.insert("en".to_owned());
    }
    result
}

/// Ranks a release against the ordered preferred list: the index of the best matching
/// preferred entry, or `preferred.len()` when nothing matches. Non-matches share the
/// non-match rank, so the bias only boosts matches. Returns 0 for an empty preferred
/// list (bias disabled).
#[must_use]
pub fn rank(preferred: &[String], indexer_language: Option<&str>, title: Option<&str>) -> usize {
    if preferred.is_empty() {
        return 0;
    }
    let languages = detect(indexer_language, title);
    if languages.is_empty() {
        return preferred.len();
    }
    preferred
        .iter()
        .position(|p| languages.contains(p))
        .unwrap_or(preferred.len())
}

/// Normalises user-supplied preferred languages ("German", "de", "DEU", "de, en") into
/// an ordered list of canonical tokens, preserving preference order, dropping duplicates.
#[must_use]
pub fn normalize_preferred(preferred: Option<&[String]>) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    let Some(preferred) = preferred else {
        return result;
    };
    for raw in preferred {
        for token in crate::language::canonical_tokens(Some(raw)) {
            if !result.contains(&token) {
                result.push(token);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(expected: &str) -> HashSet<String> {
        expected
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn parse_title_languages_detects_scene_conventions() {
        // Ported from AudioLanguagePreferenceTests.ParseTitleLanguages_DetectsSceneConventions.
        let cases = [
            (
                "Spider-Man.Brand.New.Day.2026.GERMAN.WEB-DL.2160p.ATMOS",
                "de",
            ),
            ("Show.S01E04.GER.720p.HDTV.x264", "de"),
            ("Movie.2026.DL.1080p.BluRay.x264", "de,en"),
            ("Movie.2026.DL.2160p.WEB.H265.TrueHD.Atmos", "de,en"),
            ("Stalingrad.2013.German.Dubbed.1080p.BluRay.x264", "de"),
            ("Movie.2026.PT-BR.BLURAY.2160p.HDR10-GRuP", "pt"),
            ("Movie.2026.Portuguese.Brazil.720p.WEBRip", "pt"),
            ("Movie.2024.ENGLiSH.720p.WEBRip.XviD", "en"),
            ("Movie.2024.FRENCH.1080p.BluRay.x264", "fr"),
            ("Movie.2024.TRUEFRENCH.720p.BDRip", "fr"),
            ("Movie.2024.VF2.720p.BluRay", "fr"),
            ("Movie.2024.NL.DUTCH.1080p.WEB-DL", "nl"),
            ("Movie.2024.SWEDISH.1080p.BluRay.x264", "swe"),
        ];
        for (title, expected) in cases {
            assert_eq!(parse_title_languages(Some(title)), set(expected), "{title}");
        }
    }

    #[test]
    fn parse_title_languages_fails_open_for_ambiguous_or_subtitle_markers() {
        // Ported from AudioLanguagePreferenceTests.ParseTitleLanguages_FailsOpenForAmbiguousOrSubtitleMarkers.
        let cases = [
            "Movie.2026.2160p.WEB-DL.x264",
            "Movie.2026.1080p.WEBDL.DD5.1.H.264-GRP",
            "Movie.2026.2160p.WEB.DL.Rip.x265",
            "Movie.2026.MULTi.1080p.BluRay.x264",
            "Movie.2026.DUAL.AUDIO.1080p.BluRay",
            "Movie.2020.GerSub.1080p.BluRay.x264",
            "Movie.2020.GERMAN.SUBS.1080p.BluRay.x264",
            "Stranger.Than.Fiction.2006.1080p.BluRay.x264",
            "The.DLNA.Movie.2020.1080p.BluRay",
            "Movie.2020.NL.SUBS.1080p.BluRay",
        ];
        for title in cases {
            assert!(parse_title_languages(Some(title)).is_empty(), "{title}");
        }
    }

    #[test]
    fn parse_title_languages_german_token_wins_over_neutralised_web_dl_tag() {
        for (title, expected) in [
            ("Movie.2024.German.Dubbed.1080p.WEB-DL.x264-GRP", "de"),
            ("Movie.2024.GERMAN.HDR.2160p.WEB-DL.H265-GRP", "de"),
        ] {
            assert!(
                parse_title_languages(Some(title)).contains(expected),
                "{title}"
            );
        }
    }

    #[test]
    fn detect_indexer_attribute_wins_over_title() {
        let languages = detect(Some("German"), Some("Movie.2026.PT-BR.BLURAY.2160p"));
        assert_eq!(languages, set("de"));
    }

    #[test]
    fn detect_junk_attribute_falls_back_to_title() {
        let languages = detect(Some("N/A"), Some("Movie.2026.GERMAN.WEB-DL.2160p"));
        assert_eq!(languages, set("de"));
    }

    #[test]
    fn detect_no_sources_yields_empty() {
        assert_eq!(detect(None, None), HashSet::new());
        assert_eq!(
            detect(Some("N/A"), Some("Movie.2026.2160p.WEB-DL")),
            HashSet::new()
        );
    }

    #[test]
    fn rank_best_preferred_index_wins_fail_open_otherwise() {
        // Ported from AudioLanguagePreferenceTests.Rank_BestPreferredIndexWinsFailOpenOtherwise.
        let preferred = vec!["de".to_owned(), "en".to_owned()];
        let cases = [
            (Some("German"), Some("Movie.2026.GERMAN.2160p"), 0),
            (None, Some("Movie.2026.GERMAN.2160p"), 0),
            (None, Some("Movie.2026.DL.2160p"), 0),
            (None, Some("Movie.2024.ENGLiSH.1080p"), 1),
            (None, Some("Movie.2026.PT-BR.BLURAY.2160p"), 2),
            (None, Some("Movie.2026.MULTi.1080p"), 2),
            (None, Some("Movie.2026.2160p.WEB-DL"), 2),
            (None, None, 2),
        ];
        for (attr, title, expected) in cases {
            assert_eq!(
                rank(&preferred, attr, title),
                expected,
                "{attr:?} {title:?}"
            );
        }
    }

    #[test]
    fn rank_empty_preferred_list_is_neutral() {
        assert_eq!(rank(&[], Some("German"), Some("Movie.GERMAN")), 0);
    }

    #[test]
    fn normalize_preferred_folds_names_and_preserves_order() {
        assert_eq!(
            normalize_preferred(Some(&["German".to_owned(), "EN".to_owned()])),
            vec!["de".to_owned(), "en".to_owned()]
        );
        assert_eq!(
            normalize_preferred(Some(&["de, en".to_owned(), "de".to_owned()])),
            vec!["de".to_owned(), "en".to_owned()]
        );
        assert_eq!(normalize_preferred(None), Vec::<String>::new());
        assert_eq!(
            normalize_preferred(Some(&["   ".to_owned()])),
            Vec::<String>::new()
        );
    }
}
