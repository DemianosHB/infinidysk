//! Search profiles, ported from `backend/Config/ProfileConfig.cs` (the v1 fork's
//! "profiles and adapters" model): a profile is a token-addressable search configuration
//! that selects indexers and biases result ordering (quality sort, preferred audio
//! languages) and fallback behavior.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::sorter::QualitySortMode;

/// Ported from `ProfileConfig.FallbackMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
pub enum FallbackMode {
    #[default]
    #[serde(rename = "Off")]
    Off,
    #[serde(rename = "Title")]
    Title,
    #[serde(rename = "Broad")]
    Broad,
}

fn default_fallback_min_results() -> u32 {
    3
}

/// One search profile. Ported from `ProfileConfig.Profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Profile {
    pub token: String,
    pub name: String,
    /// Empty = all enabled indexers.
    pub indexer_names: Vec<String>,
    /// `None`/empty = all adapters enabled.
    pub enabled_adapters: Option<Vec<String>>,
    pub quality_sort: QualitySortMode,
    /// Ordered preferred audio languages (e.g. `["de", "en"]`); names like "German" are
    /// normalised by [`crate::audio::normalize_preferred`]. Ordering bias, never a filter.
    pub preferred_audio_languages: Option<Vec<String>>,
    pub movie_fallback: FallbackMode,
    pub tv_fallback: FallbackMode,
    pub movie_fallback_min_results: u32,
    pub tv_fallback_min_results: u32,
    /// Legacy key; migrated by [`Profile::migrate_legacy`].
    pub query_fallback_min_results: Option<u32>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            token: String::new(),
            name: String::new(),
            indexer_names: Vec::new(),
            enabled_adapters: None,
            quality_sort: QualitySortMode::Off,
            preferred_audio_languages: None,
            movie_fallback: FallbackMode::Off,
            tv_fallback: FallbackMode::Off,
            movie_fallback_min_results: default_fallback_min_results(),
            tv_fallback_min_results: default_fallback_min_results(),
            query_fallback_min_results: None,
        }
    }
}

impl Profile {
    /// Ported from `ProfileConfig.Profile.MigrateLegacy`.
    pub fn migrate_legacy(&mut self) {
        if let Some(legacy) = self.query_fallback_min_results
            && legacy > 0
            && self.movie_fallback == FallbackMode::Off
            && self.tv_fallback == FallbackMode::Off
        {
            self.movie_fallback = FallbackMode::Title;
            self.tv_fallback = FallbackMode::Broad;
            self.movie_fallback_min_results = legacy;
            self.tv_fallback_min_results = legacy;
        }
        self.query_fallback_min_results = None;
    }
}

/// Ported from `ProfileConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct ProfileConfig {
    pub profiles: Vec<Profile>,
}

impl ProfileConfig {
    #[must_use]
    pub fn normalized(mut self) -> Self {
        for p in &mut self.profiles {
            p.migrate_legacy();
        }
        self
    }

    /// Finds a profile by token, comparing every entry without early exit so timing does
    /// not leak token prefix/position (ported from `ProfileConfig.FindByToken` with
    /// `FixedTimeEquals`).
    #[must_use]
    pub fn find_by_token(&self, token: &str) -> Option<&Profile> {
        let mut match_: Option<&Profile> = None;
        for profile in &self.profiles {
            if fixed_time_eq(token.as_bytes(), profile.token.as_bytes()) && match_.is_none() {
                match_ = Some(profile);
            }
        }
        match_
    }
}

/// Constant-time-ish equality over bytes: always compares the full length of both inputs
/// (length itself is not secret), accumulating differences instead of short-circuiting.
fn fixed_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_by_token_matches_full_token_only() {
        let cfg = ProfileConfig {
            profiles: vec![
                Profile {
                    token: "abcdef".into(),
                    name: "a".into(),
                    ..Default::default()
                },
                Profile {
                    token: "abcdef0".into(),
                    name: "b".into(),
                    ..Default::default()
                },
            ],
        };
        assert_eq!(
            cfg.find_by_token("abcdef").map(|p| p.name.as_str()),
            Some("a")
        );
        assert_eq!(
            cfg.find_by_token("abcdef0").map(|p| p.name.as_str()),
            Some("b")
        );
        assert!(cfg.find_by_token("abcde").is_none());
        assert!(cfg.find_by_token("").is_none());
    }

    #[test]
    fn migrate_legacy_moves_query_fallback_into_movie_and_tv() {
        let mut p = Profile {
            query_fallback_min_results: Some(5),
            ..Default::default()
        };
        p.migrate_legacy();
        assert_eq!(p.movie_fallback, FallbackMode::Title);
        assert_eq!(p.tv_fallback, FallbackMode::Broad);
        assert_eq!(p.movie_fallback_min_results, 5);
        assert_eq!(p.tv_fallback_min_results, 5);
        assert_eq!(p.query_fallback_min_results, None);

        // already-configured fallbacks are not overwritten
        let mut p = Profile {
            query_fallback_min_results: Some(5),
            movie_fallback: FallbackMode::Broad,
            ..Default::default()
        };
        p.migrate_legacy();
        assert_eq!(p.movie_fallback, FallbackMode::Broad);
        assert_eq!(p.tv_fallback, FallbackMode::Off);
        assert_eq!(p.query_fallback_min_results, None);
    }
}
