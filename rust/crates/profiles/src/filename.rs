//! Release-title matching, ported from `backend/Utils/FilenameMatcher.cs` (v1 fork).
//! Lookaround patterns are compiled with `fancy-regex` (.NET regex supports them;
//! the `regex` crate does not).

use std::collections::HashSet;
use std::sync::LazyLock;

use fancy_regex::Regex;
use unicode_normalization::UnicodeNormalization;

static BOUNDARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(\d{4}|S\d{1,2}(E\d{1,3})?|\d{3,4}p)\b").expect("valid regex")
});
static NON_ALNUM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-z0-9]+").expect("valid regex"));
static AMPERSAND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*&\s*").expect("valid regex"));
static RELEASE_YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?<!\d)(?<year>(?:19|20|21)\d{2})(?![0-9pPiIxX])").expect("valid regex")
});
static SEASON_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bs(\d{1,2})[. _-]?e(\d{1,3})(?:(?:-|e)e?(\d{1,3}))?\b").expect("valid regex")
});
static ALT_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?<![a-z0-9])(\d{1,2})x(\d{1,3})(?:[-x](\d{1,3}))?(?![a-z0-9])")
        .expect("valid regex")
});
static SEASON_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(?:season|series)[. _]*(\d{1,2})\b").expect("valid regex"));
static SEASON_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bs(\d{1,2})(?![. _-]?e?\d)\b").expect("valid regex"));

static LEADING_ARTICLES: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| HashSet::from(["the", "a", "an"]));
static TRAILING_QUALIFIERS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| HashSet::from(["us", "uk", "au", "ca", "nz", "ie", "za"]));

fn latin_folding(c: char) -> Option<&'static str> {
    Some(match c {
        'ø' => "o",
        'œ' => "oe",
        'æ' => "ae",
        'ł' => "l",
        'đ' | 'ð' => "d",
        'þ' => "th",
        'ß' => "ss",
        'ı' => "i",
        'ŋ' => "n",
        _ => return None,
    })
}

/// v1 `Fold`: lowercase, NFD-decompose, drop combining marks, apply the Latin folding map.
fn fold(s: &str) -> String {
    let lower = s.to_lowercase();
    let decomposed: String = lower.nfd().collect();
    let mut out = String::with_capacity(decomposed.len());
    for c in decomposed.chars() {
        if unicode_normalization::char::is_combining_mark(c) {
            continue;
        }
        match latin_folding(c) {
            Some(rep) => out.push_str(rep),
            None => out.push(c),
        }
    }
    out
}

/// v1 `HeadTokens`: fold, cut at the first boundary token (year / `SxxExx` / resolution)
/// that does not start the string, then split on non-alphanumerics.
#[must_use]
pub fn head_tokens(s: Option<&str>) -> Vec<String> {
    let Some(s) = s else { return Vec::new() };
    if s.trim().is_empty() {
        return Vec::new();
    }
    let lower = fold(s);
    let mut head_end = lower.len();
    for m in BOUNDARY.find_iter(&lower) {
        let Ok(m) = m else { break };
        if m.start() == 0 {
            continue;
        }
        head_end = m.start();
        break;
    }
    let head = &lower[..head_end];
    NON_ALNUM
        .replace_all(head, " ")
        .split(' ')
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

#[must_use]
pub fn tokens_equal(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

#[must_use]
pub fn matches(query: Option<&str>, candidate: Option<&str>) -> bool {
    let q = head_tokens(query);
    if q.is_empty() {
        return true;
    }
    tokens_equal(&q, &head_tokens(candidate))
}

/// v1 `EpisodeTag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeTag {
    pub season: i32,
    pub episode: Option<i32>,
    pub episode_end: Option<i32>,
}

fn group_i32(c: &fancy_regex::Captures, i: usize) -> Option<i32> {
    c.get(i).and_then(|m| m.as_str().parse::<i32>().ok())
}

/// v1 `ParseEpisode`: first matching pattern wins (`SxxEyy`, `NxNN`, "season N", bare `Sxx`).
#[must_use]
pub fn parse_episode(title: Option<&str>) -> Option<EpisodeTag> {
    let title = title?;
    if title.trim().is_empty() {
        return None;
    }
    if let Ok(Some(c)) = SEASON_EPISODE.captures(title) {
        return Some(EpisodeTag {
            season: group_i32(&c, 1)?,
            episode: group_i32(&c, 2),
            episode_end: group_i32(&c, 3),
        });
    }
    if let Ok(Some(c)) = ALT_EPISODE.captures(title) {
        return Some(EpisodeTag {
            season: group_i32(&c, 1)?,
            episode: group_i32(&c, 2),
            episode_end: group_i32(&c, 3),
        });
    }
    if let Ok(Some(c)) = SEASON_WORD.captures(title) {
        return Some(EpisodeTag {
            season: group_i32(&c, 1)?,
            episode: None,
            episode_end: None,
        });
    }
    if let Ok(Some(c)) = SEASON_TOKEN.captures(title) {
        return Some(EpisodeTag {
            season: group_i32(&c, 1)?,
            episode: None,
            episode_end: None,
        });
    }
    None
}

/// v1 `EpisodeCompatible`.
#[must_use]
pub fn episode_compatible(title: Option<&str>, season: Option<i32>, episode: Option<i32>) -> bool {
    let Some(tag) = parse_episode(title) else {
        return true;
    };
    if let Some(s) = season
        && tag.season != s
    {
        return false;
    }
    if let (Some(e), Some(te)) = (episode, tag.episode) {
        let end = tag.episode_end.unwrap_or(te);
        if e < te || e > end {
            return false;
        }
    }
    true
}

fn strip_leading_article(tokens: &[String]) -> &[String] {
    if tokens.len() > 1 && LEADING_ARTICLES.contains(tokens[0].as_str()) {
        &tokens[1..]
    } else {
        tokens
    }
}

fn fold_conjunctions(tokens: &mut [String]) {
    for t in tokens.iter_mut() {
        if t == "und" {
            "and".clone_into(t);
        }
    }
}

fn ampersand_fold(s: Option<&str>) -> String {
    match s {
        Some(s) if !s.is_empty() => AMPERSAND.replace_all(s, " and ").into_owned(),
        _ => String::new(),
    }
}

/// v1 `NormalizeTitle`: fold, ampersand→"and", split, drop a leading article, fold "und".
#[must_use]
pub fn normalize_title(title: Option<&str>) -> String {
    let Some(title) = title else {
        return String::new();
    };
    if title.trim().is_empty() {
        return String::new();
    }
    let folded = fold(&ampersand_fold(Some(title)));
    let mut tokens: Vec<String> = NON_ALNUM
        .replace_all(&folded, " ")
        .split(' ')
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect();
    fold_conjunctions(&mut tokens);
    strip_leading_article(&tokens).join(" ")
}

/// v1 `TitleMatches`.
#[must_use]
#[expect(
    clippy::implicit_hasher,
    reason = "v1 uses ordinal string sets; the default hasher is fine"
)]
pub fn title_matches(expected_normalized: &HashSet<String>, release_title: Option<&str>) -> bool {
    if expected_normalized.is_empty() {
        return true;
    }
    let mut head = head_tokens(Some(&ampersand_fold(release_title)));
    fold_conjunctions(&mut head);
    let head = strip_leading_article(&head);
    if head.is_empty() {
        return false;
    }
    if expected_normalized.contains(&head.join(" ")) {
        return true;
    }
    // A release head that is a leading run of canonical tokens matches (marketing subtitles
    // in the canonical title); longer heads (spin-offs) must not match, so one-directional,
    // and single-token heads are excluded to avoid near-name collisions.
    if head.len() >= 2 && is_head_prefix_of_canonical(expected_normalized, head) {
        return true;
    }
    if head.len() >= 2 {
        let last = &head[head.len() - 1];
        if TRAILING_QUALIFIERS.contains(last.as_str())
            || (last.len() == 4 && last.chars().all(|c| c.is_ascii_digit()))
        {
            return expected_normalized.contains(&head[..head.len() - 1].join(" "));
        }
    }
    false
}

fn is_head_prefix_of_canonical(expected_normalized: &HashSet<String>, head: &[String]) -> bool {
    expected_normalized.iter().any(|expected| {
        let tokens: Vec<&str> = expected.split(' ').filter(|t| !t.is_empty()).collect();
        if tokens.len() <= head.len() {
            return false;
        }
        tokens
            .iter()
            .zip(head.iter())
            .all(|(et, ht)| *et == ht.as_str())
    })
}

/// v1 `ParseReleaseYears`: plausible standalone release years (1900..=2199), resolution
/// tokens and `WxH` pairs excluded by the lookarounds.
#[must_use]
pub fn parse_release_years(release_title: Option<&str>) -> Vec<i32> {
    let Some(title) = release_title else {
        return Vec::new();
    };
    if title.trim().is_empty() {
        return Vec::new();
    }
    let mut years = Vec::new();
    for c in RELEASE_YEAR.captures_iter(title) {
        let Ok(c) = c else { break };
        if let Some(y) = c.name("year").and_then(|m| m.as_str().parse::<i32>().ok())
            && (1900..=2199).contains(&y)
        {
            years.push(y);
        }
    }
    years
}

/// v1 `YearCompatible`.
#[expect(
    clippy::implicit_hasher,
    reason = "v1 uses ordinal string sets; the default hasher is fine"
)]
#[must_use]
pub fn year_compatible(
    canonical_year: Option<i32>,
    release_title: Option<&str>,
    canonical_normalized_titles: Option<&HashSet<String>>,
) -> bool {
    let Some(expected) = canonical_year else {
        return true;
    };
    let mut title_years: HashSet<i32> = HashSet::new();
    if let Some(titles) = canonical_normalized_titles {
        for t in titles {
            title_years.extend(parse_release_years(Some(t)));
        }
    }
    let remaining: Vec<i32> = parse_release_years(release_title)
        .into_iter()
        .filter(|y| !title_years.contains(y))
        .collect();
    if remaining.is_empty() {
        return true;
    }
    remaining.iter().any(|y| (y - expected).abs() <= 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/NzbWebDAV.Tests/Utils/FilenameMatcherTests.cs.

    #[test]
    fn year_compatible_unknown_canonical_year_is_permissive() {
        assert!(year_compatible(None, Some("Dune.1984.1080p.BluRay"), None));
    }

    #[test]
    fn year_compatible_exact_year_matches() {
        assert!(year_compatible(
            Some(2024),
            Some("Dune.Part.Two.2024.2160p.WEB-DL"),
            None
        ));
    }

    #[test]
    fn year_compatible_plus_or_minus_one_matches() {
        assert!(year_compatible(
            Some(2024),
            Some("Dune.Part.Two.2023.1080p"),
            None
        ));
        assert!(year_compatible(
            Some(2024),
            Some("Dune.Part.Two.2025.1080p"),
            None
        ));
    }

    #[test]
    fn year_compatible_wrong_remake_year_is_rejected() {
        assert!(!year_compatible(
            Some(2024),
            Some("Dune.1984.1080p.BluRay"),
            None
        ));
    }

    #[test]
    fn year_compatible_missing_release_year_is_permissive() {
        assert!(year_compatible(Some(2024), Some("Dune.2160p.WEB-DL"), None));
    }

    #[test]
    fn parse_release_years_does_not_treat_resolution_as_year() {
        assert_eq!(parse_release_years(Some("Dune.2160p.WEB-DL")).len(), 0);
        assert_eq!(parse_release_years(Some("Movie.1080p.BluRay")).len(), 0);
        assert_eq!(parse_release_years(Some("Movie.1920x1080.WEB-DL")).len(), 0);
    }

    #[test]
    fn year_compatible_numeric_title_1917_uses_the_release_year() {
        let titles = HashSet::from([normalize_title(Some("1917"))]);
        assert!(year_compatible(
            Some(2019),
            Some("1917.2019.1080p.BluRay"),
            Some(&titles)
        ));
        assert!(!year_compatible(
            Some(2019),
            Some("1917.2017.1080p.BluRay"),
            Some(&titles)
        ));
        assert_eq!(
            parse_release_years(Some("1917.2019.1080p.BluRay")),
            vec![1917, 2019]
        );
    }

    #[test]
    fn year_compatible_numeric_title_1984_uses_the_release_year() {
        let titles = HashSet::from([normalize_title(Some("1984"))]);
        assert!(year_compatible(
            Some(1984),
            Some("1984.1984.1080p"),
            Some(&titles)
        ));
        assert!(!year_compatible(
            Some(1984),
            Some("1984.2023.1080p"),
            Some(&titles)
        ));
    }

    #[test]
    fn year_compatible_blade_runner_2049_does_not_treat_title_token_as_year() {
        let titles = HashSet::from([normalize_title(Some("Blade Runner 2049"))]);
        assert!(year_compatible(
            Some(2017),
            Some("Blade.Runner.2049.2017.1080p"),
            Some(&titles)
        ));
        assert!(!year_compatible(
            Some(2017),
            Some("Blade.Runner.2049.2015.1080p"),
            Some(&titles)
        ));
        assert!(year_compatible(
            Some(2017),
            Some("Blade.Runner.2049.2160p.Remux"),
            Some(&titles)
        ));
    }

    #[test]
    fn year_compatible_series_start_year_in_canonical_title_is_ignored() {
        let titles = HashSet::from([normalize_title(Some("Doctor Who 2005"))]);
        assert!(year_compatible(
            Some(2023),
            Some("Doctor.Who.2005.S01E01.720p"),
            Some(&titles)
        ));
    }

    #[test]
    fn normalize_title_folds_ampersand_to_and() {
        assert_eq!(normalize_title(Some("Law & Order")), "law and order");
        assert_eq!(
            normalize_title(Some("Feuer & Flamme - Mit Feuerwehrmännern im Einsatz")),
            "feuer and flamme mit feuerwehrmannern im einsatz"
        );
    }

    #[test]
    fn title_matches_ampersand_canonical_matches_and_und_conjunction_releases() {
        let canonical = HashSet::from([normalize_title(Some("Feuer & Flamme"))]);
        assert!(title_matches(
            &canonical,
            Some("Feuer.und.Flamme.S10E07.GERMAN.DOKU.1080p.WEB.x264-TSCC")
        ));
        assert!(title_matches(
            &canonical,
            Some("Feuer.and.Flamme.S10E07.GERMAN.DOKU.1080p.WEB.x264-TSCC")
        ));
        assert!(title_matches(
            &canonical,
            Some("Feuer & Flamme S10E07 GERMAN DOKU 1080p")
        ));
        let english = HashSet::from([normalize_title(Some("Law & Order"))]);
        assert!(title_matches(
            &english,
            Some("Law.and.Order.S20E01.720p.WEB.h264-GRP")
        ));
    }

    #[test]
    fn title_matches_canonical_marketing_subtitle_scene_short_head_matches_by_prefix() {
        let canonical = HashSet::from([normalize_title(Some(
            "Feuer & Flamme - Mit Feuerwehrmännern im Einsatz",
        ))]);
        assert!(title_matches(
            &canonical,
            Some("Feuer.und.Flamme.S10E07.GERMAN.DOKU.1080p.WEB.x264-TSCC")
        ));
        assert!(title_matches(
            &canonical,
            Some("Feuer.und.Flamme.2017.S10.GERMAN.1080p.ARD.WEB-DL.AAC2.0.H.264-KOMET")
        ));
    }

    #[test]
    fn title_matches_prefix_rule_is_one_directional() {
        let canonical = HashSet::from([normalize_title(Some("Law & Order"))]);
        assert!(!title_matches(
            &canonical,
            Some("Law.and.Order.SVU.S24E01.720p.WEB.h264-GRP")
        ));
        let svu = HashSet::from([normalize_title(Some("Law & Order: Special Victims Unit"))]);
        assert!(!title_matches(
            &svu,
            Some("Law.and.Order.SVU.S24E01.720p.WEB.h264-GRP")
        ));
    }

    #[test]
    fn title_matches_single_token_head_does_not_match_longer_canonical() {
        let canonical = HashSet::from([normalize_title(Some("CSI: Crime Scene Investigation"))]);
        assert!(!title_matches(
            &canonical,
            Some("CSI.S03E12.720p.WEB.h264-GRP")
        ));
    }

    #[test]
    fn title_matches_plain_and_apostrophe_titles_still_match_exactly() {
        let silo = HashSet::from([normalize_title(Some("Silo"))]);
        assert!(title_matches(
            &silo,
            Some("Silo.2023.S02E01.The.Engineer.2160p.ATV.WEB-DL.MULTi.DDP5.1.Atmos.H.265-FUZEER")
        ));
        let handmaid = HashSet::from([normalize_title(Some("The Handmaid's Tale"))]);
        assert!(title_matches(
            &handmaid,
            Some("The Handmaid's Tale - S01E07 - The Other Side WEBDL-2160p")
        ));
        let love_is_blind = HashSet::from([normalize_title(Some("Love Is Blind"))]);
        assert!(title_matches(
            &love_is_blind,
            Some("Love.Is.Blind.S11E13.1080p.WEB.H264-HOTDOGWATER")
        ));
    }

    #[test]
    fn episode_compatible_checks_season_and_range() {
        assert!(episode_compatible(
            Some("Show.S01E05.1080p"),
            Some(1),
            Some(5)
        ));
        assert!(!episode_compatible(
            Some("Show.S01E05.1080p"),
            Some(2),
            Some(5)
        ));
        assert!(!episode_compatible(
            Some("Show.S01E05.1080p"),
            Some(1),
            Some(6)
        ));
        assert!(episode_compatible(
            Some("Show.S01E05-E07.1080p"),
            Some(1),
            Some(6)
        ));
        assert!(episode_compatible(
            Some("Show.1x05.1080p"),
            Some(1),
            Some(5)
        ));
        assert!(episode_compatible(
            Some("Show.Season.2.1080p"),
            Some(2),
            None
        ));
        assert!(episode_compatible(Some("Show.S02.1080p"), Some(2), None));
        assert!(episode_compatible(
            Some("No.Episode.Here.1080p"),
            Some(1),
            Some(1)
        ));
        assert!(episode_compatible(None, Some(1), Some(1)));
    }
}
