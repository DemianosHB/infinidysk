//! Release quality parsing (resolution and source ranks) from release names, ported
//! from `backend/Utils/ReleaseQuality.cs`. The token-boundary lookaround guards are
//! why this crate uses `fancy-regex` instead of `regex`.

use std::sync::LazyLock;

use fancy_regex::Regex;

pub const RESOLUTION_4320: i32 = 5;
pub const RESOLUTION_2160: i32 = 4;
pub const RESOLUTION_1080: i32 = 3;
pub const RESOLUTION_720: i32 = 2;
pub const RESOLUTION_SD: i32 = 1;
pub const RESOLUTION_UNKNOWN: i32 = 0;

pub const SOURCE_REMUX: i32 = 6;
pub const SOURCE_BLURAY: i32 = 5;
pub const SOURCE_WEB_DL: i32 = 4;
pub const SOURCE_WEB_RIP: i32 = 3;
pub const SOURCE_HDTV: i32 = 2;
pub const SOURCE_DVD: i32 = 1;
pub const SOURCE_UNKNOWN: i32 = 0;
pub const SOURCE_CAM: i32 = -1;

/// Resolution and source ranks of one release name. Higher ranks are better; `CAM`
/// ranks below unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseQualityRanks {
    pub resolution: i32,
    pub source: i32,
}

impl ReleaseQualityRanks {
    pub const UNKNOWN: Self = Self {
        resolution: RESOLUTION_UNKNOWN,
        source: SOURCE_UNKNOWN,
    };
}

static RESOLUTION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?<![A-Za-z0-9])(?:(?<uhd8k>4320p|8k)|(?<uhd>2160p|4k|uhd)|(?<fhd>1080p|1080i)|(?<hd>720p)|(?<sd>576p|480p|sd))(?![A-Za-z0-9])",
    )
    .expect("valid regex literal")
});

static SOURCE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?<![A-Za-z0-9])(?:(?<remux>remux)|(?<bluray>blu[-.]?ray|bdrip|brrip)|(?<webdl>web[-.]?dl)|(?<webrip>webrip)|(?<hdtv>hdtv)|(?<dvd>dvdrip|dvd)|(?<cam>telesync|cam|ts))(?![A-Za-z0-9])",
    )
    .expect("valid regex literal")
});

/// Parses the quality ranks of a release name. Underscores are treated as dots;
/// matching is case-insensitive. `None` or whitespace-only yields
/// [`ReleaseQualityRanks::UNKNOWN`].
#[must_use]
pub fn parse(title: Option<&str>) -> ReleaseQualityRanks {
    let Some(title) = title else {
        return ReleaseQualityRanks::UNKNOWN;
    };
    if title.trim().is_empty() {
        return ReleaseQualityRanks::UNKNOWN;
    }
    let normalized = title.replace('_', ".");
    ReleaseQualityRanks {
        resolution: parse_resolution(&normalized),
        source: parse_source(&normalized),
    }
}

fn parse_resolution(title: &str) -> i32 {
    let Some(caps) = RESOLUTION_REGEX.captures(title).expect("valid regex") else {
        return RESOLUTION_UNKNOWN;
    };
    if caps.name("uhd8k").is_some() {
        RESOLUTION_4320
    } else if caps.name("uhd").is_some() {
        RESOLUTION_2160
    } else if caps.name("fhd").is_some() {
        RESOLUTION_1080
    } else if caps.name("hd").is_some() {
        RESOLUTION_720
    } else if caps.name("sd").is_some() {
        RESOLUTION_SD
    } else {
        RESOLUTION_UNKNOWN
    }
}

fn parse_source(title: &str) -> i32 {
    let Some(caps) = SOURCE_REGEX.captures(title).expect("valid regex") else {
        return SOURCE_UNKNOWN;
    };
    if caps.name("remux").is_some() {
        SOURCE_REMUX
    } else if caps.name("bluray").is_some() {
        SOURCE_BLURAY
    } else if caps.name("webdl").is_some() {
        SOURCE_WEB_DL
    } else if caps.name("webrip").is_some() {
        SOURCE_WEB_RIP
    } else if caps.name("hdtv").is_some() {
        SOURCE_HDTV
    } else if caps.name("dvd").is_some() {
        SOURCE_DVD
    } else if caps.name("cam").is_some() {
        SOURCE_CAM
    } else {
        SOURCE_UNKNOWN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognizes_resolution_aliases() {
        // Ported from ReleaseQualityTests.Parse_RecognizesResolutionAliases.
        let cases = [
            ("Movie.4320p.WEB-DL", RESOLUTION_4320),
            ("Movie.8K.WEB-DL", RESOLUTION_4320),
            ("Movie.2160p.WEB-DL", RESOLUTION_2160),
            ("Movie.4K.WEB-DL", RESOLUTION_2160),
            ("Movie.UHD.WEB-DL", RESOLUTION_2160),
            ("Movie.1080p.WEB-DL", RESOLUTION_1080),
            ("Movie.1080i.HDTV", RESOLUTION_1080),
            ("Movie.720p.WEB-DL", RESOLUTION_720),
            ("Movie.576p.DVD", RESOLUTION_SD),
            ("Movie.480p.DVD", RESOLUTION_SD),
            ("Movie.SD.WEB-DL", RESOLUTION_SD),
            ("Movie.WEB-DL", RESOLUTION_UNKNOWN),
        ];
        for (title, expected) in cases {
            assert_eq!(parse(Some(title)).resolution, expected, "{title}");
        }
    }

    #[test]
    fn parse_recognizes_source_aliases() {
        // Ported from ReleaseQualityTests.Parse_RecognizesSourceAliases.
        let cases = [
            ("Movie.1080p.REMUX", SOURCE_REMUX),
            ("Movie.1080p.BluRay", SOURCE_BLURAY),
            ("Movie.1080p.Blu-Ray", SOURCE_BLURAY),
            ("Movie.1080p.BDRip", SOURCE_BLURAY),
            ("Movie.1080p.BRRip", SOURCE_BLURAY),
            ("Movie.1080p.WEB-DL", SOURCE_WEB_DL),
            ("Movie.1080p.WEBDL", SOURCE_WEB_DL),
            ("Movie.1080p.WEBRip", SOURCE_WEB_RIP),
            ("Movie.1080p.HDTV", SOURCE_HDTV),
            ("Movie.480p.DVDRip", SOURCE_DVD),
            ("Movie.480p.DVD", SOURCE_DVD),
            ("Movie.1080p", SOURCE_UNKNOWN),
            ("Movie.1080p.CAM", SOURCE_CAM),
            ("Movie.1080p.TS", SOURCE_CAM),
            ("Movie.1080p.TELESYNC", SOURCE_CAM),
        ];
        for (title, expected) in cases {
            assert_eq!(parse(Some(title)).source, expected, "{title}");
        }
    }

    #[test]
    fn parse_is_case_and_punctuation_insensitive() {
        // Ported from ReleaseQualityTests.Parse_IsCaseAndPunctuationInsensitive.
        let dotted = parse(Some("movie.2024.2160p.web-dl"));
        let underscored = parse(Some("MOVIE_2024_2160P_WEB_DL"));
        assert_eq!(dotted.resolution, RESOLUTION_2160);
        assert_eq!(dotted.source, SOURCE_WEB_DL);
        assert_eq!(dotted, underscored);
    }

    #[test]
    fn parse_unknown_quality_returns_zero_ranks() {
        // Ported from ReleaseQualityTests.Parse_UnknownQuality_ReturnsZeroRanks.
        assert_eq!(
            parse(Some("Some.Untitled.Release")),
            ReleaseQualityRanks::UNKNOWN
        );
        assert_eq!(parse(None), ReleaseQualityRanks::UNKNOWN);
        assert_eq!(parse(Some("   ")), ReleaseQualityRanks::UNKNOWN);
    }

    #[test]
    fn parse_cam_ranks_below_unknown() {
        // Ported from ReleaseQualityTests.Parse_CamRanksBelowUnknown.
        assert!(parse(Some("Movie.CAM")).source < SOURCE_UNKNOWN);
    }

    #[test]
    fn parse_does_not_classify_ordinary_title_text() {
        // Ported from ReleaseQualityTests.Parse_DoesNotClassifyOrdinaryTitleText.
        let ranks = parse(Some("The.Web.Of.Fear"));
        assert_eq!(ranks.resolution, RESOLUTION_UNKNOWN);
        assert_eq!(ranks.source, SOURCE_UNKNOWN);
    }

    #[test]
    fn resolution_dominates_source() {
        // Ported from ReleaseQualityTests.ResolutionDominatesSource.
        let uhd_hdtv = parse(Some("Movie.2160p.HDTV"));
        let hd_remux = parse(Some("Movie.1080p.REMUX"));
        assert!(uhd_hdtv.resolution > hd_remux.resolution);
        assert!(uhd_hdtv.source < hd_remux.source);
    }
}
