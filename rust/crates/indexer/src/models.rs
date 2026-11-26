//! Indexer configuration model, ported from `backend/Config/IndexerConfig.cs` and
//! `backend/Config/ExternalMetadataResponseLimits.cs` (v1 fork). Serde-ready so the app
//! can embed it in `Settings` later.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Hard default when neither the indexer nor the global override sets a timeout.
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
/// Hard default results per indexer per search; above 100 the indexer is paged.
pub const DEFAULT_SEARCH_RESULT_LIMIT: u32 = 100;
/// Default parser-visible byte ceiling for Newznab caps/search XML (4 MiB).
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
/// Operator-facing hard clamp for the byte ceiling (16 MiB).
pub const HARD_MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

/// Ported from `IndexerConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexerConfig {
    /// Global HTTP(S) proxy URL applied to indexers without their own. `None` = the
    /// process-wide SOCKS5 proxy policy applies (see `http::build_client`).
    pub proxy_url: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub search_result_limit: Option<u32>,
    pub max_response_bytes: Option<u64>,
    pub indexers: Vec<ConnectionDetails>,
}

impl IndexerConfig {
    pub fn effective_timeout_seconds(&self, indexer: &ConnectionDetails) -> u64 {
        if let Some(per) = indexer.timeout_seconds
            && per > 0
        {
            return per;
        }
        if let Some(global) = self.timeout_seconds
            && global > 0
        {
            return global;
        }
        DEFAULT_TIMEOUT_SECONDS
    }

    pub fn effective_search_result_limit(&self, indexer: &ConnectionDetails) -> u32 {
        if let Some(per) = indexer.search_result_limit
            && per > 0
        {
            return per;
        }
        if let Some(global) = self.search_result_limit
            && global > 0
        {
            return global;
        }
        DEFAULT_SEARCH_RESULT_LIMIT
    }

    pub fn effective_max_response_bytes(&self, indexer: &ConnectionDetails) -> u64 {
        let raw = if let Some(per) = indexer.max_response_bytes.filter(|v| *v > 0) {
            per
        } else if let Some(global) = self.max_response_bytes.filter(|v| *v > 0) {
            global
        } else {
            DEFAULT_MAX_RESPONSE_BYTES
        };
        raw.clamp(1, HARD_MAX_RESPONSE_BYTES)
    }

    pub fn should_skip_tls_verification(&self, indexer_name: &str) -> bool {
        self.indexers
            .iter()
            .find(|x| x.enabled && x.name == indexer_name)
            .is_some_and(|x| x.skip_tls_verification)
    }

    pub fn per_indexer_search_user_agent(indexer: &ConnectionDetails) -> Option<String> {
        first_non_blank(
            indexer.search_user_agent.as_deref(),
            indexer.user_agent.as_deref(),
        )
    }

    pub fn per_indexer_retrieve_user_agent(indexer: &ConnectionDetails) -> Option<String> {
        first_non_blank(
            indexer.retrieve_user_agent.as_deref(),
            indexer.user_agent.as_deref(),
        )
    }
}

fn first_non_blank(a: Option<&str>, b: Option<&str>) -> Option<String> {
    let pick = |v: Option<&str>| v.filter(|s| !s.trim().is_empty()).map(str::to_owned);
    pick(a).or_else(|| pick(b))
}

/// Ported from `IndexerConfig.ConnectionDetails`.
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors the v1 ConnectionDetails field set 1:1"
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectionDetails {
    pub name: String,
    pub url: String,
    pub api_key: String,
    pub enabled: bool,
    pub user_agent: Option<String>,
    pub search_user_agent: Option<String>,
    pub retrieve_user_agent: Option<String>,
    pub skip_tls_verification: bool,
    pub max_requests_per_minute: u32,
    pub enable_strict_matching: bool,
    pub proxy_url: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub search_result_limit: Option<u32>,
    pub max_response_bytes: Option<u64>,
    pub hit_limit: Option<u32>,
    pub download_limit: Option<u32>,
    pub hit_limit_reset_time: Option<u32>,
    pub extra_movie_categories: Option<String>,
    pub extra_tv_categories: Option<String>,
    pub ignore_category_filter: bool,
    pub filter: Option<ResultFilter>,
    /// `Some` marks this entry as owned by Prowlarr pull-sync; manual entries omit it.
    pub prowlarr_indexer_id: Option<i32>,
}

impl Default for ConnectionDetails {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: String::new(),
            api_key: String::new(),
            enabled: true,
            user_agent: None,
            search_user_agent: None,
            retrieve_user_agent: None,
            skip_tls_verification: false,
            max_requests_per_minute: 0,
            enable_strict_matching: false,
            proxy_url: None,
            timeout_seconds: None,
            search_result_limit: None,
            max_response_bytes: None,
            hit_limit: None,
            download_limit: None,
            hit_limit_reset_time: None,
            extra_movie_categories: None,
            extra_tv_categories: None,
            ignore_category_filter: false,
            filter: None,
            prowlarr_indexer_id: None,
        }
    }
}

fn default_grabs_grace_hours() -> u32 {
    6
}

/// Ported from `IndexerConfig.ResultFilter` — per-indexer drop/rank rules applied before
/// merging (see [`crate::filter`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct ResultFilter {
    pub enabled: bool,
    /// Drop releases where the indexer explicitly says password != 0.
    pub skip_passworded: bool,
    /// Minimum download count to keep a release. 0 = disabled.
    pub min_grabs: u32,
    /// Grace window (hours) for the `MinGrabs` rule; 0 disables the grace window.
    #[serde(default = "default_grabs_grace_hours")]
    pub grabs_grace_hours: u32,
    /// Drop releases older than N days that still have zero recorded grabs. 0 = disabled.
    pub max_age_days_without_grabs: u32,
    /// Sort this indexer's items by grabs descending before merging (missing grabs sort
    /// below honest 0-grab items).
    pub prefer_downloaded: bool,
}

impl Default for ResultFilter {
    fn default() -> Self {
        Self {
            enabled: false,
            skip_passworded: false,
            min_grabs: 0,
            grabs_grace_hours: default_grabs_grace_hours(),
            max_age_days_without_grabs: 0,
            prefer_downloaded: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> ConnectionDetails {
        ConnectionDetails::default()
    }

    #[test]
    fn effective_values_fall_back_per_indexer_then_global_then_default() {
        let mut cfg = IndexerConfig::default();
        let mut x = conn();
        assert_eq!(cfg.effective_timeout_seconds(&x), DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(
            cfg.effective_search_result_limit(&x),
            DEFAULT_SEARCH_RESULT_LIMIT
        );
        assert_eq!(
            cfg.effective_max_response_bytes(&x),
            DEFAULT_MAX_RESPONSE_BYTES
        );

        cfg.timeout_seconds = Some(10);
        cfg.search_result_limit = Some(50);
        cfg.max_response_bytes = Some(1024);
        assert_eq!(cfg.effective_timeout_seconds(&x), 10);
        assert_eq!(cfg.effective_search_result_limit(&x), 50);
        assert_eq!(cfg.effective_max_response_bytes(&x), 1024);

        x.timeout_seconds = Some(0); // <= 0 inherits
        x.search_result_limit = Some(0);
        x.max_response_bytes = Some(0);
        assert_eq!(cfg.effective_timeout_seconds(&x), 10);
        x.timeout_seconds = Some(5);
        x.search_result_limit = Some(200);
        x.max_response_bytes = Some(u64::MAX); // clamped to hard max
        assert_eq!(cfg.effective_timeout_seconds(&x), 5);
        assert_eq!(cfg.effective_search_result_limit(&x), 200);
        assert_eq!(
            cfg.effective_max_response_bytes(&x),
            HARD_MAX_RESPONSE_BYTES
        );
    }

    #[test]
    fn skip_tls_verification_only_for_enabled_matching_name() {
        let mut x = conn();
        x.name = "a".into();
        x.enabled = true;
        x.skip_tls_verification = true;
        let mut y = conn();
        y.name = "b".into();
        let cfg = IndexerConfig {
            indexers: vec![x, y],
            ..Default::default()
        };
        assert!(cfg.should_skip_tls_verification("a"));
        assert!(!cfg.should_skip_tls_verification("b"));
        assert!(!cfg.should_skip_tls_verification("c"));
    }

    #[test]
    fn user_agent_prefers_specific_over_generic() {
        let mut x = conn();
        assert!(IndexerConfig::per_indexer_search_user_agent(&x).is_none());
        x.user_agent = Some("generic".into());
        assert_eq!(
            IndexerConfig::per_indexer_search_user_agent(&x).as_deref(),
            Some("generic")
        );
        x.search_user_agent = Some("  ".into());
        assert_eq!(
            IndexerConfig::per_indexer_search_user_agent(&x).as_deref(),
            Some("generic")
        );
        x.search_user_agent = Some("search-ua".into());
        assert_eq!(
            IndexerConfig::per_indexer_search_user_agent(&x).as_deref(),
            Some("search-ua")
        );
        assert_eq!(
            IndexerConfig::per_indexer_retrieve_user_agent(&x).as_deref(),
            Some("generic")
        );
        x.retrieve_user_agent = Some("dl-ua".into());
        assert_eq!(
            IndexerConfig::per_indexer_retrieve_user_agent(&x).as_deref(),
            Some("dl-ua")
        );
    }
}
