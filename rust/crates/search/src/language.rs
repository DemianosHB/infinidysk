//! Language-token parsing and subtitle ranking, ported from
//! `backend/Utils/SubtitlePreference.cs` (upstream v1, shared by the fork's audio bias).

use std::collections::HashSet;

use unicode_normalization::UnicodeNormalization;

/// Folds common language names and ISO-639 codes onto one canonical token, so
/// "English" / "eng" / "en" from different indexers compare equal. Unknown tokens pass
/// through in folded form.
fn alias(token: &str) -> &str {
    match token {
        "english" | "eng" | "en" => "en",
        "spanish" | "espanol" | "castellano" | "latino" | "spa" | "esp" | "es" => "es",
        "french" | "francais" | "fra" | "fre" | "fr" => "fr",
        "german" | "deutsch" | "ger" | "deu" | "de" => "de",
        "italian" | "italiano" | "ita" | "it" => "it",
        "portuguese" | "portugues" | "brazilian" | "por" | "pt" => "pt",
        "dutch" | "nederlands" | "nld" | "dut" | "nl" => "nl",
        "russian" | "rus" | "ru" => "ru",
        "japanese" | "jpn" | "jap" | "ja" | "jp" => "ja",
        "korean" | "kor" | "ko" => "ko",
        "chinese" | "mandarin" | "cantonese" | "zho" | "chi" | "zh" => "zh",
        "arabic" | "ara" | "ar" => "ar",
        "hindi" | "hin" | "hi" => "hi",
        "swedish" | "swe" | "sv" => "swe",
        "norwegian" | "nor" | "nb" => "nor",
        "danish" | "dan" | "da" => "dan",
        "finnish" | "fin" | "fi" => "fin",
        "polish" | "pol" | "pl" => "pol",
        "turkish" | "tur" | "tr" => "tur",
        other => other,
    }
}

const SEPARATORS: &[char] = &[
    ',', '/', ';', '|', '+', '&', ' ', '\t', '(', ')', '[', ']', '{', '}', '-', '_', '.',
];

/// Sidecar subtitle file extensions (.srt/.ass/.ssa/.sub/.idx/.vtt).
const SUBTITLE_EXTENSIONS: &[&str] = &[".srt", ".ass", ".ssa", ".sub", ".idx", ".vtt"];

/// Lowercase, strip accents, keep only letters/digits — so "Português" folds to
/// "portugues".
fn fold(s: &str) -> String {
    s.to_lowercase()
        .nfd()
        .filter(|c| c.is_alphabetic() || c.is_numeric())
        .collect()
}

/// Splits an indexer language/subtitle attribute on separators and maps each token to
/// its canonical form, preserving order of appearance. Empty input yields no tokens.
pub(crate) fn canonical_tokens(value: Option<&str>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if value.trim().is_empty() {
        return Vec::new();
    }
    value
        .split(SEPARATORS)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(fold)
        .filter(|t| !t.is_empty())
        .map(|t| alias(&t).to_owned())
        .collect()
}

/// Parses an indexer language/subtitle attribute (e.g. "English, Spanish") into canonical
/// language tokens. Empty input yields an empty set.
#[must_use]
pub fn parse_languages(value: Option<&str>) -> HashSet<String> {
    canonical_tokens(value).into_iter().collect()
}

/// Ranks a fallback candidate by subtitle desirability relative to the clicked release:
/// 2 = shares a subtitle language with the click, 1 = carries subtitles, 0 = none.
/// Higher ranks are preferred during failover.
#[must_use]
pub fn subtitle_rank(
    candidate_subs: Option<&str>,
    primary_languages: &HashSet<String, impl std::hash::BuildHasher>,
    primary_has_subs: bool,
) -> u8 {
    let languages = parse_languages(candidate_subs);
    if languages.is_empty() {
        return 0;
    }
    if primary_has_subs && languages.iter().any(|l| primary_languages.contains(l)) {
        return 2;
    }
    1
}

/// True when the file name is a sidecar subtitle file.
#[must_use]
pub fn is_subtitle_file(file_name: Option<&str>) -> bool {
    let Some(file_name) = file_name else {
        return false;
    };
    if file_name.trim().is_empty() {
        return false;
    }
    let ext = file_name.rsplit('.').next().map(|e| format!(".{e}"));
    ext.is_some_and(|ext| {
        SUBTITLE_EXTENSIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&ext))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_languages_canonicalizes_common_english_aliases() {
        // Ported from SubtitlePreferenceTests.
        for input in ["English", "eng", "en"] {
            assert_eq!(
                parse_languages(Some(input)),
                HashSet::from(["en".to_owned()])
            );
        }
    }

    #[test]
    fn parse_languages_splits_multiple_languages() {
        assert_eq!(
            parse_languages(Some("English, Spanish")),
            HashSet::from(["en".to_owned(), "es".to_owned()])
        );
    }

    #[test]
    fn parse_languages_empty_inputs_yield_empty_set() {
        for input in [None, Some(""), Some("   ")] {
            assert_eq!(
                parse_languages(input),
                HashSet::<String>::new(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn parse_languages_folds_accented_portuguese() {
        assert_eq!(
            parse_languages(Some("Português")),
            HashSet::from(["pt".to_owned()])
        );
    }

    #[test]
    fn parse_languages_passes_unknown_tokens_through_folded() {
        assert_eq!(
            parse_languages(Some("klingon")),
            HashSet::from(["klingon".to_owned()])
        );
    }

    #[test]
    fn subtitle_rank_shared_language_is_highest() {
        let primary = parse_languages(Some("English"));
        assert_eq!(subtitle_rank(Some("eng"), &primary, true), 2);
    }

    #[test]
    fn subtitle_rank_subbed_candidate_without_match_is_middle() {
        let primary = parse_languages(Some("English"));
        assert_eq!(subtitle_rank(Some("Spanish"), &primary, true), 1);
        assert_eq!(subtitle_rank(Some("English"), &HashSet::new(), false), 1);
    }

    #[test]
    fn subtitle_rank_subless_candidate_is_lowest() {
        let primary = parse_languages(Some("English"));
        assert_eq!(subtitle_rank(None, &primary, true), 0);
        assert_eq!(subtitle_rank(Some(""), &primary, true), 0);
    }

    #[test]
    fn is_subtitle_file_recognizes_sidecar_extensions() {
        assert!(is_subtitle_file(Some("Movie.srt")));
        assert!(is_subtitle_file(Some("a.ASS")));
        assert!(!is_subtitle_file(Some("movie.mkv")));
        assert!(!is_subtitle_file(None));
        assert!(!is_subtitle_file(Some("noext")));
    }

    #[test]
    fn subtitle_rank_preserves_relative_order_within_tier() {
        let primary = parse_languages(Some("English"));
        let items = [
            ("a", None),
            ("b", Some("Spanish")),
            ("c", Some("")),
            ("d", Some("English")),
        ];
        let mut ordered: Vec<_> = items.to_vec();
        ordered.sort_by_key(|(_, subs)| std::cmp::Reverse(subtitle_rank(*subs, &primary, true)));
        let ordered: Vec<_> = ordered.into_iter().map(|(n, _)| n).collect();
        assert_eq!(ordered, ["d", "b", "a", "c"]);
    }
}
