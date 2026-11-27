//! Settings: defaults, overlaid by values stored in the database, overlaid by
//! `INFINIDYSK__<SECTION>__<KEY>` environment variables. The environment layer is
//! authoritative and never written back; only differences from the defaults are stored.

use std::collections::HashSet;
use std::fmt;
use std::ops::RangeInclusive;
use std::sync::Arc;

use infinidysk_db::Db;
use infinidysk_indexer::models::IndexerConfig;
use infinidysk_nntp::catalog;
use infinidysk_search::profile::ProfileConfig;
use nix::sys::stat::Mode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::ToSchema;
use uuid::Uuid;

/// What every non-empty secret looks like outside the process. Saving it back keeps the
/// stored secret.
pub const MASK: &str = "********";
const ENV_PREFIX: &str = "INFINIDYSK__";
/// The disk cache folder when none is saved, for callers that know no config folder.
pub const DEFAULT_CACHE_DIR: &str = "/config/cache";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config storage failed")]
    Db(#[from] infinidysk_db::Error),
    #[error("config is malformed")]
    Json(#[from] serde_json::Error),
    #[error("{message}")]
    Invalid {
        /// The dotted setting path, like `mount.dir`.
        path: &'static str,
        message: String,
    },
    #[error("{0} is set by an environment variable and cannot be changed here")]
    Locked(String),
}

impl Error {
    /// The dotted setting path the error is about.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Invalid { path, .. } => Some(path),
            Self::Locked(path) => Some(path),
            Self::Db(_) | Self::Json(_) => None,
        }
    }
}

fn invalid(path: &'static str, message: String) -> Error {
    Error::Invalid { path, message }
}

const CHECK_PERCENT: RangeInclusive<u8> = 5..=100;
const QUEUE_CONCURRENCY: RangeInclusive<u8> = 1..=10;
const NZB_RETENTION_DAYS: RangeInclusive<u32> = 0..=3650;
const READ_AHEAD_MB: RangeInclusive<u32> = 1..=1250;
const MEMORY_CACHE_MB: RangeInclusive<u32> = 16..=65_536;
const DISK_CACHE_GB: RangeInclusive<u32> = 1..=100_000;
const DISK_CACHE_MAX_AGE_HOURS: RangeInclusive<u32> = 1..=8760;
const READ_AHEAD_MEMORY_MB: RangeInclusive<u32> = 64..=65_536;
const FUSE_MAX_READAHEAD_KB: RangeInclusive<u32> = 128..=65_536;
const FUSE_MAX_BACKGROUND: RangeInclusive<u16> = 1..=1024;
const MAX_CONNECTIONS: RangeInclusive<u16> = 1..=u16::MAX;
const PIPELINE_DEPTH: RangeInclusive<u8> = 1..=16;
const PORT: RangeInclusive<u16> = 1..=u16::MAX;
const QUOTA_GB: RangeInclusive<u32> = 1..=u32::MAX;
const RESET_DAY: RangeInclusive<u8> = 1..=31;
/// Up to 100 Gbit/s.
const DOWNLOAD_LIMIT_MB_PER_S: RangeInclusive<u32> = 1..=12_500;
/// v1 `usenet.streaming-write-timeout-seconds`: 0 disables the timeout.
const WEBDAV_IDLE_TIMEOUT_SECONDS: RangeInclusive<u32> = 0..=600;

fn check<T: PartialOrd + fmt::Display + Copy>(
    path: &'static str,
    what: &str,
    value: T,
    range: &RangeInclusive<T>,
) -> Result<(), Error> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(invalid(
            path,
            format!("{what} must be {}-{}", range.start(), range.end()),
        ))
    }
}

fn reset<T: PartialOrd + fmt::Display>(
    field: &str,
    value: &mut T,
    range: &RangeInclusive<T>,
    default: T,
) {
    if !range.contains(value) {
        reset_warning(field, &*value, &default);
        *value = default;
    }
}

/// An override outside `range` falls back to the preset's value.
fn reset_override<T: PartialOrd + fmt::Display + Copy>(
    field: &str,
    value: &mut Option<T>,
    range: &RangeInclusive<T>,
    preset: T,
) {
    if let Some(v) = *value
        && !range.contains(&v)
    {
        reset_warning(field, &v, &preset);
        *value = None;
    }
}

fn reset_warning(field: &str, value: &dyn fmt::Display, default: &dyn fmt::Display) {
    tracing::warn!(
        field,
        %value,
        %default,
        "stored setting is out of range; using the default"
    );
}

/// A password or key. `Debug` never prints it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(transparent)]
pub struct Secret(pub String);

impl Secret {
    fn masked(&self) -> Self {
        if self.0.is_empty() {
            Self::default()
        } else {
            Self(MASK.to_owned())
        }
    }

    pub(crate) fn restore(&mut self, current: &Self) {
        if self.0 == MASK {
            self.0.clone_from(&current.0);
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.masked().0, f)
    }
}

/// v1-fork search settings: `search.exclude-patterns` and the `api.*-user-agent` values,
/// plus the master switch for the external-ID resolvers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Search {
    /// One exclude regex per line (bare .NET body or `/body/flags`); matching titles drop.
    pub exclude_patterns: String,
    /// User-Agent for NZB retrieval (v1 `api.user-agent`); default `SABnzbd/5.1.0`.
    pub user_agent: String,
    /// User-Agent for indexer search requests (v1 `api.search-user-agent`).
    pub search_user_agent: String,
    /// Master switch for the external-ID resolvers (TVmaze/Wikidata/Kitsu/anime-lists).
    pub resolvers_enabled: bool,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            exclude_patterns: String::new(),
            user_agent: "SABnzbd/5.1.0".to_owned(),
            search_user_agent: format!("nzbdav/{}", env!("CARGO_PKG_VERSION")),
            resolvers_enabled: true,
        }
    }
}

/// v1 `prowlarr.*`: pull-sync connection, interval and persisted last-sync status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Prowlarr {
    pub url: String,
    pub api_key: Secret,
    pub sync_enabled: bool,
    /// Minutes between automatic syncs; clamped to [`PROWLARR_SYNC_INTERVAL_MINUTES`].
    pub sync_interval_minutes: u32,
    pub sync_status: Option<ProwlarrSyncStatus>,
}

/// v1 `prowlarr.sync-interval-minutes` clamp (5..=10080, default 60).
pub const PROWLARR_SYNC_INTERVAL_MINUTES: RangeInclusive<u32> = 5..=10080;
pub const DEFAULT_PROWLARR_SYNC_INTERVAL_MINUTES: u32 = 60;

impl Default for Prowlarr {
    fn default() -> Self {
        Self {
            url: String::new(),
            api_key: Secret::default(),
            sync_enabled: false,
            sync_interval_minutes: DEFAULT_PROWLARR_SYNC_INTERVAL_MINUTES,
            sync_status: None,
        }
    }
}

/// v1 `prowlarr.sync-status` (persisted last sync outcome).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct ProwlarrSyncStatus {
    pub last_attempt_at: i64,
    pub last_success_at: Option<i64>,
    pub last_error: Option<String>,
    pub remote_indexer_count: i32,
    pub added: i32,
    pub updated: i32,
    pub removed: i32,
    pub skipped: i32,
}

/// v1 `preflight.*` — background verification of search candidates (Phase 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Preflight {
    /// `off` | `light` | `standard` | `full` (v1 `GetPreflightMode`, default off).
    pub mode: String,
    /// v1 default 20, clamp 1..=50.
    pub max_attempts: u32,
    /// v1 default 120, clamp 10..=1800.
    pub ttl_seconds: u32,
    /// v1 default 5, clamp 0..=120.
    pub indexer_max_wait_seconds: u32,
}

impl Default for Preflight {
    fn default() -> Self {
        Self {
            mode: "off".to_owned(),
            max_attempts: 20,
            ttl_seconds: 120,
            indexer_max_wait_seconds: 5,
        }
    }
}

impl Preflight {
    #[must_use]
    pub fn mode(&self) -> &str {
        match self.mode.as_str() {
            "light" | "standard" | "full" => &self.mode,
            _ => "off",
        }
    }
    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts.clamp(1, 50)
    }
    #[must_use]
    pub fn ttl_seconds(&self) -> u32 {
        self.ttl_seconds.clamp(10, 1800)
    }
    #[must_use]
    pub fn indexer_max_wait_seconds(&self) -> u32 {
        self.indexer_max_wait_seconds.clamp(0, 120)
    }
}

/// v1 `play.*` — the play/failover path (Phase 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Play {
    /// `stat` | `body` | `none` (v1 `GetPlayVerifyMode`, default none).
    pub verify_mode: String,
    /// v1 default 3, clamp 1..=10.
    pub verify_sample_count: u32,
    /// v1 default 30, clamp 3..=180.
    pub total_budget_seconds: u32,
    /// v1 default 3, clamp 1..=30.
    pub hedge_delay_seconds: u32,
    /// v1 default 3, clamp 1..=10.
    pub max_candidates: u32,
    /// v1 default 10, clamp 1..=200.
    pub max_attempts: u32,
    /// v1 default true.
    pub prefer_subtitles: bool,
    /// v1 default true.
    pub watchdog_enabled: bool,
    /// Fork stall negative marking, default false.
    pub stall_negative_marking_enabled: bool,
    /// v1 default 15 min, clamp 1..=1440.
    pub stall_negative_marking_minutes: u32,
    /// v1 default 5 min, clamp 1..=1440.
    pub candidate_negative_cache_minutes: u32,
    /// v1 default 168 h, clamp 1..=720.
    pub resolution_cache_ttl_hours: u32,
    /// v1 `play.exclude-patterns`, fallback `search.exclude-patterns`.
    pub exclude_patterns: String,
}

impl Default for Play {
    fn default() -> Self {
        Self {
            verify_mode: "none".to_owned(),
            verify_sample_count: 3,
            total_budget_seconds: 30,
            hedge_delay_seconds: 3,
            max_candidates: 3,
            max_attempts: 10,
            prefer_subtitles: true,
            watchdog_enabled: true,
            stall_negative_marking_enabled: false,
            stall_negative_marking_minutes: 15,
            candidate_negative_cache_minutes: 5,
            resolution_cache_ttl_hours: 168,
            exclude_patterns: String::new(),
        }
    }
}

impl Play {
    #[must_use]
    pub fn verify_mode(&self) -> &str {
        match self.verify_mode.as_str() {
            "body" | "stat" | "none" => &self.verify_mode,
            _ => "none",
        }
    }
    #[must_use]
    pub fn verify_sample_count(&self) -> u32 {
        self.verify_sample_count.clamp(1, 10)
    }
    #[must_use]
    pub fn total_budget_seconds(&self) -> u32 {
        self.total_budget_seconds.clamp(3, 180)
    }
    #[must_use]
    pub fn hedge_delay_seconds(&self) -> u32 {
        self.hedge_delay_seconds.clamp(1, 30)
    }
    #[must_use]
    pub fn max_candidates(&self) -> u32 {
        self.max_candidates.clamp(1, 10)
    }
    #[must_use]
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts.clamp(1, 200)
    }
    #[must_use]
    pub fn stall_negative_marking_ttl_minutes(&self) -> u32 {
        self.stall_negative_marking_minutes.clamp(1, 1440)
    }
    #[must_use]
    pub fn candidate_negative_cache_ttl_minutes(&self) -> u32 {
        self.candidate_negative_cache_minutes.clamp(1, 1440)
    }
    #[must_use]
    pub fn resolution_cache_ttl_hours(&self) -> u32 {
        self.resolution_cache_ttl_hours.clamp(1, 720)
    }
}

/// v1 `grab.stall-failover-*` — abort a stalled play candidate and try the next one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Grab {
    /// v1 default true.
    pub stall_failover_enabled: bool,
    /// v1 default 2, clamp 2..=60.
    pub stall_failover_window_seconds: u32,
    /// v1 default 5, clamp 5..=120.
    pub stall_failover_ceiling_seconds: u32,
}

impl Default for Grab {
    fn default() -> Self {
        Self {
            stall_failover_enabled: true,
            stall_failover_window_seconds: 2,
            stall_failover_ceiling_seconds: 5,
        }
    }
}

impl Grab {
    #[must_use]
    pub fn stall_failover_window_seconds(&self) -> u32 {
        self.stall_failover_window_seconds.clamp(2, 60)
    }
    #[must_use]
    pub fn stall_failover_ceiling_seconds(&self) -> u32 {
        self.stall_failover_ceiling_seconds.clamp(5, 120)
    }
}

/// v1 `warden.*` — the dead-release list (Phase 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct Warden {
    /// v1 default 2 (>= 1).
    pub quorum: u32,
    /// v1 default true.
    pub hide_dead: bool,
    /// v1 default true.
    pub backbone_scope: bool,
    /// v1 default 2,000,000 (> 0).
    pub max_source_entries: u32,
}

impl Default for Warden {
    fn default() -> Self {
        Self {
            quorum: 2,
            hide_dead: true,
            backbone_scope: true,
            max_source_entries: 2_000_000,
        }
    }
}

impl Warden {
    #[must_use]
    pub fn quorum(&self) -> i64 {
        i64::from(self.quorum.max(1))
    }
    #[must_use]
    pub fn max_source_entries(&self) -> i64 {
        i64::from(self.max_source_entries.max(1))
    }
}

/// v1 `watchtower.*` — automatic series/movie grabs (Phase 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "v1 watchtower.* keys map 1:1 onto these fields"
)]
#[serde(rename_all = "camelCase", default)]
pub struct Watchtower {
    pub enabled: bool,
    pub profile_token: String,
    /// `watchdog` (default) | `largest`.
    pub ranking: String,
    /// v1 default 524288000 (500 MiB).
    pub size_floor_bytes: i64,
    /// v1 default 0 = no ceiling.
    pub size_ceiling_bytes: i64,
    pub min_grabs: u32,
    /// v1 default 2, clamp 1..=5.
    pub shortlist_depth: u32,
    /// v1 default 3, clamp 1..=10.
    pub grab_cap_per_resolve: u32,
    /// v1 default 3, clamp 1..=20.
    pub verify_sample_count: u32,
    /// v1 default 10, clamp 2..=120.
    pub verify_timeout_seconds: u32,
    /// v1 default 100, clamp 1..=100000.
    pub active_set_cap: u32,
    /// v1 default 3, clamp 1..=16.
    pub resolve_concurrency: u32,
    /// v1 default 60.
    pub daily_resolve_budget: u32,
    /// v1 default 3600, clamp 60..=86400.
    pub sync_interval_seconds: u32,
    /// v1 default: watchtower list source byte cap.
    pub list_source_max_response_bytes: u64,
    /// v1 keep-fresh bounds.
    pub keepfresh_base_seconds: u32,
    pub keepfresh_max_seconds: u32,
    /// v1 default 3, clamp 1..=100.
    pub series_recent_count: u32,
    /// v1 default 50, clamp 0..=1000.
    pub series_max_episodes: u32,
    /// `newest` (default) | `oldest`.
    pub series_cap_keep: String,
    pub season_bundles: bool,
    pub season_bundle_fallback: bool,
    pub season_bundle_fallback_scope: String,
    pub season_bundle_fallback_recent_count: u32,
    pub season_bundle_fallback_max_episodes: u32,
    pub auto_throughput: bool,
    pub verbose_logging: bool,
    /// Global series scope: `latest-season` (default) | `all-aired` | `first-season` | `recent` | `off`.
    pub series_scope: String,
    /// v1 default 21600 (6 h), clamp 600..=604800.
    pub unavailable_retry_seconds: u32,
}

impl Default for Watchtower {
    fn default() -> Self {
        Self {
            enabled: false,
            profile_token: String::new(),
            ranking: "watchdog".to_owned(),
            size_floor_bytes: 524_288_000,
            size_ceiling_bytes: 0,
            min_grabs: 0,
            shortlist_depth: 2,
            grab_cap_per_resolve: 3,
            verify_sample_count: 3,
            verify_timeout_seconds: 10,
            active_set_cap: 100,
            resolve_concurrency: 3,
            daily_resolve_budget: 60,
            sync_interval_seconds: 3600,
            list_source_max_response_bytes: 16 * 1024 * 1024,
            keepfresh_base_seconds: 3600,
            keepfresh_max_seconds: 86400,
            series_recent_count: 3,
            series_max_episodes: 50,
            series_cap_keep: "newest".to_owned(),
            season_bundles: true,
            season_bundle_fallback: false,
            season_bundle_fallback_scope: "recent".to_owned(),
            season_bundle_fallback_recent_count: 2,
            season_bundle_fallback_max_episodes: 50,
            auto_throughput: false,
            verbose_logging: false,
            series_scope: "latest-season".to_owned(),
            unavailable_retry_seconds: 21_600,
        }
    }
}

impl Watchtower {
    #[must_use]
    pub fn ranking(&self) -> &str {
        if self.ranking == "largest" {
            "largest"
        } else {
            "watchdog"
        }
    }
    #[must_use]
    pub fn shortlist_depth(&self) -> u32 {
        self.shortlist_depth.clamp(1, 5)
    }
    #[must_use]
    pub fn grab_cap_per_resolve(&self) -> u32 {
        self.grab_cap_per_resolve.clamp(1, 10)
    }
    #[must_use]
    pub fn verify_sample_count(&self) -> u32 {
        self.verify_sample_count.clamp(1, 20)
    }
    #[must_use]
    pub fn verify_timeout_seconds(&self) -> u32 {
        self.verify_timeout_seconds.clamp(2, 120)
    }
    #[must_use]
    pub fn active_set_cap(&self) -> u32 {
        self.active_set_cap.clamp(1, 100_000)
    }
    #[must_use]
    pub fn resolve_concurrency(&self) -> u32 {
        self.resolve_concurrency.clamp(1, 16)
    }
    #[must_use]
    pub fn sync_interval_seconds(&self) -> u32 {
        self.sync_interval_seconds.clamp(60, 86_400)
    }
    #[must_use]
    pub fn series_recent_count(&self) -> u32 {
        self.series_recent_count.clamp(1, 100)
    }
    #[must_use]
    pub fn series_max_episodes(&self) -> u32 {
        self.series_max_episodes.clamp(0, 1000)
    }
    #[must_use]
    pub fn series_cap_keep(&self) -> &str {
        if self.series_cap_keep == "oldest" {
            "oldest"
        } else {
            "newest"
        }
    }
    #[must_use]
    pub fn season_bundle_fallback_recent_count(&self) -> u32 {
        self.season_bundle_fallback_recent_count.clamp(1, 100)
    }
    #[must_use]
    pub fn season_bundle_fallback_max_episodes(&self) -> u32 {
        self.season_bundle_fallback_max_episodes.clamp(1, 1000)
    }
    #[must_use]
    pub fn series_scope(&self) -> &str {
        match self.series_scope.as_str() {
            "all-aired" | "first-season" | "recent" | "latest-season" | "off" => &self.series_scope,
            _ => "latest-season",
        }
    }
    #[must_use]
    pub fn unavailable_retry_seconds(&self) -> u32 {
        self.unavailable_retry_seconds.clamp(600, 604_800)
    }
    #[must_use]
    pub fn season_bundle_fallback_scope(&self) -> &str {
        match self.season_bundle_fallback_scope.as_str() {
            "all" | "recent" | "unwatched" => &self.season_bundle_fallback_scope,
            _ => "recent",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Settings {
    pub sab: Sab,
    pub auth: Auth,
    pub webdav: WebDav,
    pub rclone_rc: RcloneRc,
    pub mount: Mount,
    pub performance: Performance,
    pub providers: Vec<Provider>,
    pub arr_instances: Vec<ArrInstance>,
    pub arr: ArrOptions,
    pub health: Health,
    pub queue: Queue,
    pub imports: Imports,
    pub metrics: Metrics,
    pub network: Network,
    pub proxy: Proxy,
    pub repair: Repair,
    pub maintenance: Maintenance,
    /// v1-fork search settings: exclude patterns and user agents (`search.*`, `api.*-user-agent`).
    pub search: Search,
    /// v1 `indexers.instances` — the Newznab/Prowlarr indexer list.
    pub indexers: IndexerConfig,
    /// v1 `profiles.instances` — token-addressable search profiles.
    pub profiles: ProfileConfig,
    /// v1 `prowlarr.*` — Prowlarr pull-sync connection and status.
    pub prowlarr: Prowlarr,
    /// v1 `preflight.*` — background candidate verification.
    pub preflight: Preflight,
    /// v1 `play.*` — play/failover path.
    pub play: Play,
    /// v1 `grab.stall-failover-*` — stalled-candidate failover during play.
    pub grab: Grab,
    /// v1 `warden.*` — dead-release list.
    pub warden: Warden,
    /// v1 `watchtower.*` — automatic grabs.
    pub watchtower: Watchtower,
    /// Experiments switched on, by name; they apply after a restart.
    #[serde(deserialize_with = "known_experiments")]
    pub experiments: Vec<String>,
}

/// An unfinished change users may switch on while it is measured.
#[derive(Debug, Serialize, ToSchema)]
pub struct Experiment {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

pub(crate) const IDLE_MEMORY_TRIM: &str = "idle-memory-trim";

pub const EXPERIMENTS: [Experiment; 4] = [
    Experiment {
        name: IDLE_MEMORY_TRIM,
        title: "Release unused memory when idle",
        description: "On Linux, return unused memory to the OS after two minutes without playback or other work. The log shows the observed drop in process memory in MB. Keeps the segment cache. The next playback may take slightly longer to start.",
    },
    Experiment {
        name: "par2-repair",
        title: "Repair original articles with PAR2",
        description: "Repairs damaged files with their PAR2 recovery data when Arr replacement cannot, and checks degraded files before they fail. Repairs are stored on disk. Kernel and throughput validation is still pending; back up your config before enabling. Applies after restart.",
    },
    Experiment {
        name: "win-floor",
        title: "Deeper read-ahead for large articles",
        description: "Files posted in articles of 2.1 MB or more may keep two articles per connection in flight, so one stream can use every connection. Faster sequential reads in testing; jumping backwards may start slower.",
    },
    Experiment {
        name: "weight-large",
        title: "Large articles use a whole connection",
        description: "Articles of 2.1 MB or more count as a full connection's pipeline, so a deep pipeline cannot stack them on one connection. Matters most with a high pipeline depth.",
    },
];

/// Keeps names from [`EXPERIMENTS`] once each; others (retired, or a typo) are dropped.
fn known_experiments<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let names = Vec::<String>::deserialize(d)?;
    Ok(EXPERIMENTS
        .iter()
        .filter(|e| names.iter().any(|n| n == e.name))
        .map(|e| e.name.to_owned())
        .collect())
}

/// Maintenance jobs and the daily window they run in (`infinidysk-lab:docs/maintenance-policy.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Maintenance {
    pub window: MaintenanceWindow,
    /// Jobs that run in the window, once each per window.
    #[serde(deserialize_with = "known_jobs")]
    pub scheduled: Vec<MaintenanceJob>,
    pub backup: BackupSettings,
}

impl Default for Maintenance {
    fn default() -> Self {
        Self {
            window: MaintenanceWindow::default(),
            scheduled: vec![MaintenanceJob::RemoveDeadLinks],
            backup: BackupSettings::default(),
        }
    }
}

/// Where backups go and how many the backup job keeps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct BackupSettings {
    /// Absolute folder; empty means `backups` beside the database.
    pub dir: String,
    /// Scheduled and manual backups kept; pinned, uploaded and added ones are extra.
    pub keep: u16,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            dir: String::new(),
            keep: 3,
        }
    }
}

pub const BACKUP_KEEP: RangeInclusive<u16> = 1..=100;

/// Daily, in server time, as `HH:MM`. An end at or before the start ends the next day.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct MaintenanceWindow {
    pub start: String,
    pub end: String,
}

impl Default for MaintenanceWindow {
    fn default() -> Self {
        Self {
            start: "03:00".to_owned(),
            end: "06:00".to_owned(),
        }
    }
}

impl MaintenanceWindow {
    /// `(start, end)` in minutes after midnight; `None` when either is not `HH:MM` or
    /// they are equal.
    #[must_use]
    pub fn minutes(&self) -> Option<(u16, u16)> {
        let parse = |s: &str| {
            let (h, m) = s.split_once(':')?;
            if h.len() != 2 || m.len() != 2 {
                return None;
            }
            let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
            (h < 24 && m < 60).then_some(h * 60 + m)
        };
        let (start, end) = (parse(&self.start)?, parse(&self.end)?);
        (start != end).then_some((start, end))
    }
}

/// A maintenance job, in the order the window runs them.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum MaintenanceJob {
    /// Writes a checked copy of the SQLite library database and prunes old copies.
    BackupDatabase,
    /// Deletes downloads nothing in the library folders points to.
    RemoveUnlinked,
    /// Removes library links and STRM files whose download is gone, and has the Arr
    /// search for it again.
    RemoveDeadLinks,
    /// Rewrites the SQLite databases without their unused space; always scheduled.
    CompactDatabase,
}

impl MaintenanceJob {
    pub const ALL: [Self; 4] = [
        Self::BackupDatabase,
        Self::RemoveUnlinked,
        Self::RemoveDeadLinks,
        Self::CompactDatabase,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::BackupDatabase => "backup-database",
            Self::RemoveUnlinked => "remove-unlinked",
            Self::RemoveDeadLinks => "remove-dead-links",
            Self::CompactDatabase => "compact-database",
        }
    }

    /// Runs in every window; not part of the `scheduled` setting.
    #[must_use]
    pub const fn always_scheduled(self) -> bool {
        matches!(self, Self::CompactDatabase)
    }

    #[must_use]
    pub const fn has_dry_run(self) -> bool {
        matches!(self, Self::RemoveUnlinked | Self::RemoveDeadLinks)
    }
}

/// Drops job ids this build does not know (stored by a newer one) and jobs that are
/// always scheduled.
fn known_jobs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<MaintenanceJob>, D::Error> {
    let ids = Vec::<Value>::deserialize(d)?;
    Ok(ids
        .into_iter()
        .filter_map(|id| serde_json::from_value::<MaintenanceJob>(id).ok())
        .filter(|job| !job.always_scheduled())
        .collect())
}

/// How finished jobs reach the Arrs (`infinidysk-lab:docs/strm-support-proposal.md` §4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Imports {
    pub strategy: ImportStrategy,
    /// STRM mode: where each job's STRM files are written for the Arrs to import.
    pub strm_dir: String,
    /// Signs STRM playback URLs, one key per item.
    pub strm_key: Secret,
    /// Folders holding the Arrs' libraries, walked when pointers change form.
    pub library_dirs: Vec<LibraryDir>,
    /// Exact chmod for handoff files and the folders made for them, as 3-4 octal digits
    /// (e.g. `"664"`, `"2775"`). Unset: the process umask decides, as 1.x did. Reported
    /// to SAB clients as `misc.permissions`.
    pub handoff_permissions: Option<String>,
}

impl Imports {
    /// [`Self::handoff_permissions`] parsed as an octal mode, `None` when unset or invalid.
    /// Parsed straight into `mode_t`, which is `u16` on macOS and `u32` elsewhere.
    #[must_use]
    pub fn handoff_mode(&self) -> Option<Mode> {
        nix::libc::mode_t::from_str_radix(self.handoff_permissions.as_deref()?.trim(), 8)
            .ok()
            .map(Mode::from_bits_truncate)
    }
}

/// A folder of an Arr's library.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(from = "StoredLibraryDir")]
pub struct LibraryDir {
    /// As seen from here.
    pub path: String,
    /// As the Arrs see it; empty when that is `path`.
    pub arr_path: String,
}

/// Builds before `arr_path` stored a bare path string.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredLibraryDir {
    Path(String),
    Dir {
        #[serde(default)]
        path: String,
        #[serde(default)]
        arr_path: String,
    },
}

impl From<StoredLibraryDir> for LibraryDir {
    fn from(stored: StoredLibraryDir) -> Self {
        match stored {
            StoredLibraryDir::Path(path) => Self {
                path,
                arr_path: String::new(),
            },
            StoredLibraryDir::Dir { path, arr_path } => Self { path, arr_path },
        }
    }
}

impl LibraryDir {
    #[must_use]
    pub fn arr_path(&self) -> &str {
        if self.arr_path.is_empty() {
            &self.path
        } else {
            &self.arr_path
        }
    }
}

impl Default for Imports {
    fn default() -> Self {
        Self {
            strategy: ImportStrategy::default(),
            strm_dir: "/data/completed-downloads".to_owned(),
            strm_key: Secret::default(),
            library_dirs: Vec::new(),
            handoff_permissions: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImportStrategy {
    /// Symlinks into the mount, for Plex and anything else on the mount host.
    #[default]
    Symlinks,
    /// STRM files with playback URLs, for Emby, Jellyfin and Kodi without the mount.
    Strm,
}

/// Import queue choices; everything else is fixed by `infinidysk-lab:docs/import-policy.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Queue {
    /// Jobs importing at once, 1 to 10.
    pub concurrency: u8,
    /// Filename globs (`*`, `?`) dropped from imports on top of the built-in list.
    pub extra_blocklist: Vec<String>,
    /// Days an NZB is kept after nothing uses it anymore; 0 deletes it at once.
    pub nzb_retention_days: u32,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            concurrency: 2,
            extra_blocklist: Vec::new(),
            nzb_retention_days: 30,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Network {
    /// Reverse proxies (addresses or CIDR ranges) whose `X-Forwarded-For` names the
    /// client; every local range by default.
    pub trusted_proxies: Vec<String>,
    /// URL Arrs and browsers use to reach this app, such as the reverse-proxy address;
    /// empty when not set.
    pub public_url: String,
    /// Cap on the total download speed from all providers together; `None` for no
    /// limit. Serving files to players is not limited.
    pub download_limit_mb_per_s: Option<u32>,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            trusted_proxies: [
                "127.0.0.0/8",
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "169.254.0.0/16",
                "::1/128",
                "fc00::/7",
                "fe80::/10",
            ]
            .map(str::to_owned)
            .into(),
            public_url: String::new(),
            download_limit_mb_per_s: None,
        }
    }
}

/// The process-wide outbound SOCKS5 proxy, ported from the v1 fork's
/// `general.socks5-proxy` / `general.socks5-proxy-bypass` settings. All non-local outbound
/// traffic — NNTP provider connections and HTTP — is tunneled through it; loopback
/// addresses, single-label Docker service names and the bypass entries always connect
/// directly. Read per connection, so saving applies to new connections.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Proxy {
    /// `socks5://host:port` (`socks5h://` is accepted and normalized); empty disables the
    /// proxy.
    pub socks5: String,
    /// Comma/whitespace-separated exceptions that connect directly: `*`, a host name (also
    /// covering its subdomains), a domain suffix (`.lan` or `*.lan`), an IP address, or a
    /// CIDR range.
    pub socks5_bypass: String,
}

/// Read-ahead PAR2 repair, ported from the v1 fork's `repair.readahead.*` settings. When
/// enabled (and the `par2-repair` experiment is on), an active stream widens its prefetch
/// window to measured-bitrate × window ahead of the read head — capped by the in-flight
/// article RAM budget — so missing or corrupt articles are discovered and repaired before
/// playback reaches them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Repair {
    /// Widen the prefetch window of active streams for ahead-of-playback repair.
    pub readahead_enabled: bool,
    /// Read-ahead window in seconds.
    pub readahead_window_seconds: u32,
}

impl Default for Repair {
    fn default() -> Self {
        Self {
            readahead_enabled: false,
            readahead_window_seconds: infinidysk_stream::readahead::DEFAULT_WINDOW_SECONDS,
        }
    }
}

/// Health monitoring choices; everything else is fixed by `infinidysk-lab:docs/health-policy.md`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools, reason = "independent user toggles")]
pub struct Health {
    /// Run routine checks. Rechecks, playback reports and repairs run regardless.
    pub enabled: bool,
    /// Share of segments each health check probes, 5 to 100; at least 5 segments.
    pub check_percent: u8,
    /// Share of segments an import probes, 5 to 100; at least 5 segments.
    pub import_check_percent: u8,
    /// Ask the Arr that imported a broken file for a replacement.
    pub auto_repair: bool,
    /// Keep serving files with a few small gaps instead of replacing them.
    pub degraded_tolerance: bool,
    /// What happens to a broken file not imported by an Arr.
    pub unowned_action: UnownedAction,
    /// Analyze each good media file once with ffprobe, when it is installed.
    pub media_probe: bool,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            enabled: true,
            check_percent: infinidysk_core::media::SAMPLE_PERCENT,
            import_check_percent: infinidysk_core::media::SAMPLE_PERCENT,
            auto_repair: true,
            degraded_tolerance: true,
            unowned_action: UnownedAction::Delete,
            media_probe: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnownedAction {
    #[default]
    Delete,
    ActionNeeded,
}

/// How we work with the Arr instances; the rules are fixed by `infinidysk-lab:docs/import-policy.md` §10.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct ArrOptions {
    /// Clear downloads whose import is stuck for a known reason from the Arr queues.
    pub queue_management: bool,
}

impl Default for ArrOptions {
    fn default() -> Self {
        Self {
            queue_management: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Metrics {
    /// Serve Prometheus metrics at `/metrics`, behind the admin API key
    /// (`Authorization: Bearer <key>`).
    pub prometheus: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Sab {
    pub api_key: Secret,
    pub categories: Vec<String>,
    /// Category for NZBs submitted without one, or with `*`.
    pub manual_category: String,
}

impl Default for Sab {
    fn default() -> Self {
        Self {
            api_key: Secret::default(),
            categories: ["audio", "software", "tv", "movies"]
                .map(str::to_owned)
                .into(),
            manual_category: "uncategorized".to_owned(),
        }
    }
}

impl Sab {
    /// Categories offered to download clients: the manual category first.
    pub fn categories(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.manual_category.as_str())
            .chain(self.categories.iter().map(String::as_str))
            .map(str::trim)
            .filter(|c| !c.is_empty())
    }
}

/// Sign-in choices and admin API secrets; the admin login itself lives in the accounts table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Auth {
    /// `X-Api-Key` for scripts calling the admin API; separate from the SAB key.
    pub api_key: Secret,
    /// Signs UI session cookies; changing it logs everyone out.
    pub session_key: Secret,
    /// Who must sign in to the web UI and admin API.
    pub mode: AuthMode,
    /// Sign in with the admin username and password. Turning it off takes effect only
    /// while SSO is set up.
    pub local_login: bool,
    /// Single sign-on through one OIDC provider.
    pub oidc: Oidc,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            api_key: Secret::default(),
            session_key: Secret::default(),
            mode: AuthMode::default(),
            local_login: true,
            oidc: Oidc::default(),
        }
    }
}

impl Auth {
    /// Username and password sign-in is accepted; always while SSO is off.
    #[must_use]
    pub fn password_login(&self) -> bool {
        self.local_login || !self.oidc.enabled()
    }
}

/// SSO through one OIDC provider; on while `issuer` is set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Oidc {
    /// Issuer URL exactly as the provider publishes it; empty turns SSO off.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Secret,
    /// Callback registered at the provider. Empty means `network.public_url`, else the
    /// address the browser used, plus `/auth/oidc/callback`.
    pub redirect_uri: String,
    /// Space-separated; `openid` is always requested.
    pub scopes: String,
    /// ID-token claim shown as the username; then `preferred_username`, `email`, `sub`.
    pub username_claim: String,
    /// Shown on the sign-in page as "Sign in with <name>"; empty shows `SSO`.
    pub display_name: String,
}

impl Default for Oidc {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            client_id: String::new(),
            client_secret: Secret::default(),
            redirect_uri: String::new(),
            scopes: "openid profile email".to_owned(),
            username_claim: "preferred_username".to_owned(),
            display_name: String::new(),
        }
    }
}

impl Oidc {
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.issuer.is_empty()
    }

    /// The provider name the sign-in page shows.
    #[must_use]
    pub fn display_name(&self) -> &str {
        if self.display_name.is_empty() {
            "SSO"
        } else {
            &self.display_name
        }
    }

    /// Trims the fields; checks them only while SSO is on, so clearing the issuer alone
    /// always turns SSO off.
    pub(crate) fn validate(&mut self) -> Result<(), Error> {
        self.issuer = self.issuer.trim().to_owned();
        self.client_id = self.client_id.trim().to_owned();
        self.redirect_uri = self.redirect_uri.trim().to_owned();
        self.username_claim = self.username_claim.trim().to_owned();
        self.display_name = self
            .display_name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        self.scopes = self.scopes.split_whitespace().collect::<Vec<_>>().join(" ");
        if self.display_name.chars().count() > MAX_DISPLAY_NAME
            || self.display_name.chars().any(char::is_control)
        {
            return Err(invalid(
                "auth.oidc.display_name",
                format!("Display name must be plain text of at most {MAX_DISPLAY_NAME} characters"),
            ));
        }
        if !self.enabled() {
            return Ok(());
        }
        if !issuer_ok(&self.issuer) {
            return Err(invalid(
                "auth.oidc.issuer",
                format!(
                    "Issuer {:?} must be an https:// address without a query (http:// only on this machine)",
                    self.issuer
                ),
            ));
        }
        if self.client_id.is_empty() {
            return Err(invalid(
                "auth.oidc.client_id",
                "SSO needs the client ID from your identity provider".to_owned(),
            ));
        }
        if self.client_secret.0.is_empty() {
            return Err(invalid(
                "auth.oidc.client_secret",
                "SSO needs the client secret from your identity provider".to_owned(),
            ));
        }
        if !self.redirect_uri.is_empty() && !redirect_ok(&self.redirect_uri) {
            return Err(invalid(
                "auth.oidc.redirect_uri",
                format!(
                    "Redirect URI {:?} must be an http:// or https:// address without a fragment",
                    self.redirect_uri
                ),
            ));
        }
        Ok(())
    }
}

/// Fits the sign-in button on a phone screen.
const MAX_DISPLAY_NAME: usize = 40;

/// `https://`, or `http://` to this machine ([`crate::oidc::secure`]), with no query or
/// fragment. The trailing slash stays: discovery compares the issuer exactly.
fn issuer_ok(issuer: &str) -> bool {
    reqwest::Url::parse(issuer).is_ok_and(|u| {
        crate::oidc::secure(u.scheme(), u.host_str().unwrap_or_default())
            && u.query().is_none()
            && u.fragment().is_none()
    })
}

fn redirect_ok(uri: &str) -> bool {
    reqwest::Url::parse(uri).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.host_str().is_some() && u.fragment().is_none()
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    Required,
    /// Clients on loopback, private or link-local addresses skip sign-in.
    Local,
    Disabled,
}

/// `WebDAV` login, separate from the UI login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct WebDav {
    pub user: String,
    pub password: Secret,
    /// `downloadKey` that `/view` links must carry.
    pub download_key: Secret,
    /// Seconds a streamed response may go unconsumed before the read is closed and its
    /// session leaves the active reads; 0 disables. v1 `usenet.streaming-write-timeout-seconds`.
    pub idle_timeout_seconds: u32,
}

impl Default for WebDav {
    fn default() -> Self {
        Self {
            user: "admin".to_owned(),
            password: Secret::default(),
            download_key: Secret::default(),
            idle_timeout_seconds: 60,
        }
    }
}

/// The RC server of an external rclone mount of the `WebDAV` root, told to forget cached
/// directory listings when they change. Applies after a restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct RcloneRc {
    pub enabled: bool,
    /// Base URL of the RC server: `https://`, or `http://` on loopback.
    pub url: String,
    /// The mounted VFS as rclone names it (e.g. `library:`); empty when it serves one.
    pub fs: String,
    pub user: String,
    pub password: Secret,
}

impl RcloneRc {
    /// The `vfs/forget` endpoint below [`Self::url`].
    pub(crate) fn forget_url(&self) -> Result<reqwest::Url, Error> {
        let bad = || {
            invalid(
                "rclone_rc.url",
                "Enter the RC base URL: HTTPS, or HTTP on loopback, without credentials, \
                 query, or fragment"
                    .to_owned(),
            )
        };
        let mut url = reqwest::Url::parse(&self.url).map_err(|_| bad())?;
        let host = url.host_str().ok_or_else(bad)?;
        if !crate::oidc::secure(url.scheme(), host)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(bad());
        }
        url.path_segments_mut()
            .map_err(|()| bad())?
            .pop_if_empty()
            .push("vfs")
            .push("forget");
        Ok(url)
    }

    fn validate(&mut self) -> Result<(), Error> {
        for field in [&mut self.url, &mut self.fs, &mut self.user] {
            *field = field.trim().to_owned();
        }
        if !self.url.is_empty() {
            self.forget_url()?;
        }
        if !self.enabled {
            return Ok(());
        }
        if self.url.is_empty() {
            self.forget_url()?;
        }
        if self.user.contains(':') {
            return Err(invalid(
                "rclone_rc.user",
                "RC username must not contain a colon".to_owned(),
            ));
        }
        if self.user.is_empty() != self.password.0.is_empty() {
            let path = if self.user.is_empty() {
                "rclone_rc.user"
            } else {
                "rclone_rc.password"
            };
            return Err(invalid(
                path,
                "Enter both an RC username and password, or neither".to_owned(),
            ));
        }
        Ok(())
    }

    /// Scheme, host and port of a parseable RC URL.
    fn origin(url: &str) -> Option<reqwest::Url> {
        let mut url = reqwest::Url::parse(url.trim()).ok()?;
        url.set_path("");
        url.set_query(None);
        url.set_fragment(None);
        Some(url)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Mount {
    /// Where the tree is mounted, as Sonarr/Radarr see it; completed downloads are reported
    /// under `<dir>/completed-symlinks`.
    pub dir: String,
    /// Serve the built-in FUSE mount at `dir`. Off leaves `dir` to an external mount.
    pub fuse: bool,
    /// Media files on the mount report 1970 dates, matching an rclone mount that used
    /// `--no-modtime`, so media servers that stored those dates do not rescan.
    pub epoch_times: bool,
}

impl Default for Mount {
    fn default() -> Self {
        Self {
            dir: "/mnt/infinidysk".to_owned(),
            fuse: true,
            epoch_times: false,
        }
    }
}

/// Streaming and cache tuning: a preset, plus overrides (`None` takes the preset's value).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Performance {
    pub preset: Preset,
    /// Bytes each open file fetches ahead of the reader.
    pub read_ahead_mb: Option<u32>,
    pub memory_cache_enabled: Option<bool>,
    pub memory_cache_mb: Option<u32>,
    pub disk_cache_enabled: Option<bool>,
    pub disk_cache_dir: Option<String>,
    pub disk_cache_gb: Option<u32>,
    pub disk_cache_max_age_hours: Option<u32>,
    /// Decoded bytes all open files together may hold in flight.
    pub read_ahead_memory_mb: Option<u32>,
    pub fuse_max_readahead_kb: Option<u32>,
    pub fuse_max_background: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    #[default]
    Balanced,
    MaxPlayback,
    LowResources,
}

/// Concrete performance values: a preset with its overrides applied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct Tuning {
    pub read_ahead_mb: u32,
    pub memory_cache_enabled: bool,
    pub memory_cache_mb: u32,
    pub disk_cache_enabled: bool,
    pub disk_cache_dir: String,
    pub disk_cache_gb: u32,
    pub disk_cache_max_age_hours: u32,
    pub read_ahead_memory_mb: u32,
    pub fuse_max_readahead_kb: u32,
    pub fuse_max_background: u16,
}

impl Preset {
    pub const ALL: [Self; 3] = [Self::Balanced, Self::MaxPlayback, Self::LowResources];

    #[must_use]
    pub fn tuning(self) -> Tuning {
        // Read-ahead memory = read-ahead x streams at full depth (10, ~13, 5).
        let (read_ahead_mb, memory_cache_mb, read_ahead_memory_mb, fuse_kb, fuse_background) =
            match self {
                Self::Balanced => (60, 600, 600, 5000, 64),
                Self::MaxPlayback => (150, 1250, 2000, 10_000, 128),
                Self::LowResources => (20, 150, 100, 1250, 32),
            };
        Tuning {
            read_ahead_mb,
            memory_cache_enabled: true,
            memory_cache_mb,
            disk_cache_enabled: false,
            disk_cache_dir: DEFAULT_CACHE_DIR.to_owned(),
            disk_cache_gb: 25,
            disk_cache_max_age_hours: 168,
            read_ahead_memory_mb,
            fuse_max_readahead_kb: fuse_kb,
            fuse_max_background: fuse_background,
        }
    }
}

impl Performance {
    #[must_use]
    pub fn effective(&self) -> Tuning {
        let base = self.preset.tuning();
        Tuning {
            read_ahead_mb: self.read_ahead_mb.unwrap_or(base.read_ahead_mb),
            memory_cache_enabled: self
                .memory_cache_enabled
                .unwrap_or(base.memory_cache_enabled),
            memory_cache_mb: self.memory_cache_mb.unwrap_or(base.memory_cache_mb),
            disk_cache_enabled: self.disk_cache_enabled.unwrap_or(base.disk_cache_enabled),
            disk_cache_dir: self.disk_cache_dir.clone().unwrap_or(base.disk_cache_dir),
            disk_cache_gb: self.disk_cache_gb.unwrap_or(base.disk_cache_gb),
            disk_cache_max_age_hours: self
                .disk_cache_max_age_hours
                .unwrap_or(base.disk_cache_max_age_hours),
            read_ahead_memory_mb: self
                .read_ahead_memory_mb
                .unwrap_or(base.read_ahead_memory_mb),
            fuse_max_readahead_kb: self
                .fuse_max_readahead_kb
                .unwrap_or(base.fuse_max_readahead_kb),
            fuse_max_background: self.fuse_max_background.unwrap_or(base.fuse_max_background),
        }
    }

    fn reset_invalid(&mut self) {
        let p = self.preset.tuning();
        reset_override(
            "performance.read_ahead_mb",
            &mut self.read_ahead_mb,
            &READ_AHEAD_MB,
            p.read_ahead_mb,
        );
        reset_override(
            "performance.memory_cache_mb",
            &mut self.memory_cache_mb,
            &MEMORY_CACHE_MB,
            p.memory_cache_mb,
        );
        reset_override(
            "performance.disk_cache_gb",
            &mut self.disk_cache_gb,
            &DISK_CACHE_GB,
            p.disk_cache_gb,
        );
        reset_override(
            "performance.disk_cache_max_age_hours",
            &mut self.disk_cache_max_age_hours,
            &DISK_CACHE_MAX_AGE_HOURS,
            p.disk_cache_max_age_hours,
        );
        reset_override(
            "performance.read_ahead_memory_mb",
            &mut self.read_ahead_memory_mb,
            &READ_AHEAD_MEMORY_MB,
            p.read_ahead_memory_mb,
        );
        reset_override(
            "performance.fuse_max_readahead_kb",
            &mut self.fuse_max_readahead_kb,
            &FUSE_MAX_READAHEAD_KB,
            p.fuse_max_readahead_kb,
        );
        reset_override(
            "performance.fuse_max_background",
            &mut self.fuse_max_background,
            &FUSE_MAX_BACKGROUND,
            p.fuse_max_background,
        );
    }

    fn validate(&self, mount_dir: &str, default_cache_dir: &str) -> Result<(), Error> {
        let t = self.effective();
        check(
            "performance.read_ahead_mb",
            "Read-ahead",
            t.read_ahead_mb,
            &READ_AHEAD_MB,
        )?;
        check(
            "performance.memory_cache_mb",
            "Memory cache size",
            t.memory_cache_mb,
            &MEMORY_CACHE_MB,
        )?;
        check(
            "performance.disk_cache_gb",
            "Disk cache size",
            t.disk_cache_gb,
            &DISK_CACHE_GB,
        )?;
        check(
            "performance.disk_cache_max_age_hours",
            "Disk cache max age",
            t.disk_cache_max_age_hours,
            &DISK_CACHE_MAX_AGE_HOURS,
        )?;
        check(
            "performance.read_ahead_memory_mb",
            "Read-ahead memory",
            t.read_ahead_memory_mb,
            &READ_AHEAD_MEMORY_MB,
        )?;
        check(
            "performance.fuse_max_readahead_kb",
            "FUSE max readahead",
            t.fuse_max_readahead_kb,
            &FUSE_MAX_READAHEAD_KB,
        )?;
        check(
            "performance.fuse_max_background",
            "FUSE max background",
            t.fuse_max_background,
            &FUSE_MAX_BACKGROUND,
        )?;
        let cache_dir = self.disk_cache_dir.as_deref().unwrap_or(default_cache_dir);
        let dir = std::path::Path::new(cache_dir);
        if !dir.is_absolute() {
            return Err(invalid(
                "performance.disk_cache_dir",
                format!("Disk cache dir {cache_dir:?} must be an absolute path"),
            ));
        }
        if dir.starts_with(mount_dir) {
            return Err(invalid(
                "performance.disk_cache_dir",
                format!("Disk cache dir {cache_dir:?} must be outside the mount dir"),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct Provider {
    /// Stable across edits, so masked secrets can be matched back.
    pub id: Uuid,
    pub name: String,
    /// Catalog profile id. When set, `port` and `backbone` come from the profile server
    /// named by `hostname` (its first server when empty).
    pub profile: Option<String>,
    pub hostname: String,
    pub port: u16,
    pub tls: bool,
    pub username: String,
    pub password: Secret,
    pub max_connections: u16,
    /// BODY requests in flight per connection, `1..=16`; Auto-Tune Pipeline sets it.
    pub pipeline_depth: u8,
    /// Same number = one pool; higher numbers are asked only after lower ones miss.
    pub priority: u32,
    /// Catalog backbone id; a custom provider without one is its own group.
    pub backbone: Option<String>,
    /// Download cap; once used up, requests go to other providers until it resets.
    pub quota: Option<Quota>,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Quota {
    /// Decimal gigabytes (10^9 bytes), as providers sell them.
    pub gb: u32,
    pub period: QuotaPeriod,
    /// Monthly: the day of the month usage resets, `1..=31`, at 00:00 UTC; shorter
    /// months reset on their last day.
    #[serde(default = "first_day")]
    pub reset_day: u8,
}

const fn first_day() -> u8 {
    1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum QuotaPeriod {
    Monthly,
    /// A prepaid block: resets only by hand.
    Block,
}

impl Default for Provider {
    fn default() -> Self {
        Self {
            id: Uuid::now_v7(),
            name: String::new(),
            profile: None,
            hostname: String::new(),
            port: 563,
            tls: true,
            username: String::new(),
            password: Secret::default(),
            max_connections: 10,
            pipeline_depth: 4,
            priority: 0,
            backbone: None,
            quota: None,
            enabled: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ArrInstance {
    #[serde(default = "Uuid::now_v7")]
    pub id: Uuid,
    pub name: String,
    pub profile: ArrProfile,
    pub url: String,
    pub api_key: Secret,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

const fn enabled() -> bool {
    true
}

/// Arr-specific behavior hangs off this, so new Arr types are additive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ArrProfile {
    Sonarr,
    Radarr,
    Lidarr,
    Chaptarr,
    Whisparr,
    Sportarr,
}

impl Settings {
    /// A copy safe to return from the API or log: every non-empty secret is [`MASK`].
    #[must_use]
    pub fn masked(&self) -> Self {
        let mut out = self.clone();
        out.sab.api_key = out.sab.api_key.masked();
        out.auth.api_key = out.auth.api_key.masked();
        out.auth.session_key = out.auth.session_key.masked();
        out.auth.oidc.client_secret = out.auth.oidc.client_secret.masked();
        out.webdav.password = out.webdav.password.masked();
        out.webdav.download_key = out.webdav.download_key.masked();
        out.rclone_rc.password = out.rclone_rc.password.masked();
        out.imports.strm_key = out.imports.strm_key.masked();
        for p in &mut out.providers {
            p.password = p.password.masked();
        }
        for a in &mut out.arr_instances {
            a.api_key = a.api_key.masked();
        }
        out
    }

    /// Replaces [`MASK`] placeholders with the secrets in `current`.
    fn restore_secrets(&mut self, current: &Self) {
        self.sab.api_key.restore(&current.sab.api_key);
        self.auth.api_key.restore(&current.auth.api_key);
        self.auth.session_key.restore(&current.auth.session_key);
        self.auth
            .oidc
            .client_secret
            .restore(&current.auth.oidc.client_secret);
        self.webdav.password.restore(&current.webdav.password);
        self.webdav
            .download_key
            .restore(&current.webdav.download_key);
        self.rclone_rc.password.restore(&current.rclone_rc.password);
        self.imports.strm_key.restore(&current.imports.strm_key);
        for p in &mut self.providers {
            if let Some(c) = current.providers.iter().find(|c| c.id == p.id) {
                p.password.restore(&c.password);
            }
        }
        for a in &mut self.arr_instances {
            if let Some(c) = current.arr_instances.iter().find(|c| c.id == a.id) {
                a.api_key.restore(&c.api_key);
            }
        }
    }

    /// On load: resets out-of-range numbers and a blank manual category to their defaults,
    /// so a value an older build or a hand edit stored does not stop startup.
    fn reset_invalid(&mut self) {
        let d = Self::default();
        reset(
            "health.check_percent",
            &mut self.health.check_percent,
            &CHECK_PERCENT,
            d.health.check_percent,
        );
        reset(
            "health.import_check_percent",
            &mut self.health.import_check_percent,
            &CHECK_PERCENT,
            d.health.import_check_percent,
        );
        reset(
            "queue.concurrency",
            &mut self.queue.concurrency,
            &QUEUE_CONCURRENCY,
            d.queue.concurrency,
        );
        reset(
            "queue.nzb_retention_days",
            &mut self.queue.nzb_retention_days,
            &NZB_RETENTION_DAYS,
            d.queue.nzb_retention_days,
        );
        reset(
            "repair.readahead_window_seconds",
            &mut self.repair.readahead_window_seconds,
            &infinidysk_stream::readahead::WINDOW_SECONDS_RANGE,
            d.repair.readahead_window_seconds,
        );
        if self.sab.manual_category.trim().is_empty() {
            reset_warning(
                "sab.manual_category",
                &format!("{:?}", self.sab.manual_category),
                &d.sab.manual_category,
            );
            self.sab.manual_category = d.sab.manual_category;
        }
        if self.maintenance.window.minutes().is_none() {
            let w = &self.maintenance.window;
            reset_warning(
                "maintenance.window",
                &format!("{}-{}", w.start, w.end),
                &format!(
                    "{}-{}",
                    d.maintenance.window.start, d.maintenance.window.end
                ),
            );
            self.maintenance.window = d.maintenance.window;
        }
        reset(
            "maintenance.backup.keep",
            &mut self.maintenance.backup.keep,
            &BACKUP_KEEP,
            d.maintenance.backup.keep,
        );
        if !PROWLARR_SYNC_INTERVAL_MINUTES.contains(&self.prowlarr.sync_interval_minutes) {
            reset_warning(
                "prowlarr.sync_interval_minutes",
                &self.prowlarr.sync_interval_minutes,
                &DEFAULT_PROWLARR_SYNC_INTERVAL_MINUTES,
            );
            self.prowlarr.sync_interval_minutes = DEFAULT_PROWLARR_SYNC_INTERVAL_MINUTES;
        }
        self.performance.reset_invalid();
        if let Some(limit) = self.network.download_limit_mb_per_s
            && !DOWNLOAD_LIMIT_MB_PER_S.contains(&limit)
        {
            reset_warning("network.download_limit_mb_per_s", &limit, &"no limit");
            self.network.download_limit_mb_per_s = None;
        }
        let pd = Provider::default();
        for p in &mut self.providers {
            let f = format!("providers[{}].", p.label());
            reset(
                &format!("{f}max_connections"),
                &mut p.max_connections,
                &MAX_CONNECTIONS,
                pd.max_connections,
            );
            reset(
                &format!("{f}pipeline_depth"),
                &mut p.pipeline_depth,
                &PIPELINE_DEPTH,
                pd.pipeline_depth,
            );
            reset(&format!("{f}port"), &mut p.port, &PORT, pd.port);
            if let Some(q) = p.quota
                && !QUOTA_GB.contains(&q.gb)
            {
                reset_warning(&format!("{f}quota.gb"), &q.gb, &"no quota");
                p.quota = None;
            }
            if let Some(q) = &mut p.quota {
                reset(
                    &format!("{f}quota.reset_day"),
                    &mut q.reset_day,
                    &RESET_DAY,
                    first_day(),
                );
            }
        }
    }

    /// Checks every field and fills the provider fields that come from the catalog.
    pub fn validate(&mut self) -> Result<(), Error> {
        self.validate_in(DEFAULT_CACHE_DIR)
    }

    /// [`Self::validate`], with the disk cache in `default_cache_dir` when none is saved.
    pub(crate) fn validate_in(&mut self, default_cache_dir: &str) -> Result<(), Error> {
        if !self.mount.dir.starts_with('/') {
            return Err(invalid(
                "mount.dir",
                format!("Mount dir {:?} must be an absolute path", self.mount.dir),
            ));
        }
        // A mount over the config folder would hide its files once it mounts.
        if let Some(config_dir) = std::path::Path::new(default_cache_dir).parent()
            && config_dir.starts_with(&self.mount.dir)
        {
            return Err(invalid(
                "mount.dir",
                format!(
                    "Mount dir {:?} must not contain the config folder {}",
                    self.mount.dir,
                    config_dir.display()
                ),
            ));
        }
        if self.webdav.user.trim().is_empty() {
            return Err(invalid(
                "webdav.user",
                "WebDAV user must not be empty".to_owned(),
            ));
        }
        check(
            "webdav.idle_timeout_seconds",
            "Idle timeout seconds",
            self.webdav.idle_timeout_seconds,
            &WEBDAV_IDLE_TIMEOUT_SECONDS,
        )?;
        self.rclone_rc.validate()?;
        if self.sab.manual_category.trim().is_empty() {
            return Err(invalid(
                "sab.manual_category",
                "Default category must not be empty".to_owned(),
            ));
        }
        check(
            "health.check_percent",
            "Check percent",
            self.health.check_percent,
            &CHECK_PERCENT,
        )?;
        check(
            "health.import_check_percent",
            "Import check percent",
            self.health.import_check_percent,
            &CHECK_PERCENT,
        )?;
        check(
            "queue.concurrency",
            "Concurrency",
            self.queue.concurrency,
            &QUEUE_CONCURRENCY,
        )?;
        check(
            "queue.nzb_retention_days",
            "NZB retention days",
            self.queue.nzb_retention_days,
            &NZB_RETENTION_DAYS,
        )?;
        self.queue.extra_blocklist = self
            .queue
            .extra_blocklist
            .iter()
            .map(|g| g.trim().to_owned())
            .filter(|g| !g.is_empty())
            .collect();
        self.performance
            .validate(&self.mount.dir, default_cache_dir)?;
        if self.maintenance.window.minutes().is_none() {
            return Err(invalid(
                "maintenance.window",
                "The window needs a start and a different end, each HH:MM".to_owned(),
            ));
        }
        self.maintenance.scheduled.sort_unstable();
        self.maintenance.scheduled.dedup();
        check(
            "maintenance.backup.keep",
            "Copies to keep",
            self.maintenance.backup.keep,
            &BACKUP_KEEP,
        )?;
        let backup_dir = self.maintenance.backup.dir.trim();
        if !backup_dir.is_empty() {
            if !backup_dir.starts_with('/') {
                return Err(invalid(
                    "maintenance.backup.dir",
                    format!("Backup folder {backup_dir:?} must be an absolute path"),
                ));
            }
            if std::path::Path::new(backup_dir).starts_with(&self.mount.dir) {
                return Err(invalid(
                    "maintenance.backup.dir",
                    format!("Backup folder {backup_dir:?} must be outside the mount"),
                ));
            }
        }
        self.maintenance.backup.dir = backup_dir.to_owned();
        let strm_dir = self.imports.strm_dir.trim();
        if !strm_dir.starts_with('/') {
            return Err(invalid(
                "imports.strm_dir",
                format!("STRM directory {strm_dir:?} must be an absolute path"),
            ));
        }
        self.imports.strm_dir = strm_dir.to_owned();
        if let Some(perms) = self.imports.handoff_permissions.take() {
            let trimmed = perms.trim();
            let valid = (3..=4).contains(&trimmed.len())
                && trimmed.bytes().all(|b| b.is_ascii_digit() && b <= b'7')
                && u32::from_str_radix(trimmed, 8).is_ok_and(|m| m <= 0o7777);
            if !valid {
                return Err(invalid(
                    "imports.handoff_permissions",
                    format!(
                        "Handoff permissions {perms:?} must be 3-4 octal digits (e.g. \"664\")"
                    ),
                ));
            }
            self.imports.handoff_permissions = Some(trimmed.to_owned());
        }
        self.imports
            .library_dirs
            .retain(|d| !d.path.trim().is_empty());
        for dir in &mut self.imports.library_dirs {
            for path in [&mut dir.path, &mut dir.arr_path] {
                let trimmed = path.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('/') {
                    return Err(invalid(
                        "imports.library_dirs",
                        format!("Library folder {trimmed:?} must be an absolute path"),
                    ));
                }
                *path = if trimmed == "/" {
                    trimmed.to_owned()
                } else {
                    trimmed.trim_end_matches('/').to_owned()
                };
            }
        }
        let inside = |a: &str, b: &str| std::path::Path::new(a).starts_with(b);
        // The mount and the STRM directory may sit inside a library folder (one `/data`
        // for everything); library walks skip them.
        for (i, dir) in self.imports.library_dirs.iter().enumerate() {
            let path = dir.path.as_str();
            let clash = [
                (self.mount.dir.as_str(), "is inside the mount"),
                (
                    self.imports.strm_dir.as_str(),
                    "is inside the completed STRM directory",
                ),
            ]
            .into_iter()
            .find(|(other, _)| inside(path, other))
            .map(|(_, what)| what)
            .or_else(|| {
                self.imports
                    .library_dirs
                    .iter()
                    .enumerate()
                    .any(|(j, o)| i != j && (inside(path, &o.path) || inside(&o.path, path)))
                    .then_some("overlaps another library folder")
            });
            if let Some(what) = clash {
                return Err(invalid(
                    "imports.library_dirs",
                    format!("Library folder {path:?} {what}"),
                ));
            }
        }
        self.network
            .trusted_proxies
            .retain(|p| !p.trim().is_empty());
        for proxy in &mut self.network.trusted_proxies {
            if infinidysk_webdav::parse_proxy(proxy).is_none() {
                return Err(invalid(
                    "network.trusted_proxies",
                    format!("Trusted proxy {proxy:?} is not an IP address or CIDR range"),
                ));
            }
            *proxy = proxy.trim().to_owned();
        }
        let url = self.network.public_url.trim().trim_end_matches('/');
        let host = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"));
        if !url.is_empty()
            && host.is_none_or(|h| {
                h.is_empty()
                    || h.starts_with('/')
                    || h.contains(|c: char| c.is_whitespace() || c == '?' || c == '#')
            })
        {
            return Err(invalid(
                "network.public_url",
                format!(
                    "Public URL {url:?} must be an http:// or https:// address without a query"
                ),
            ));
        }
        self.network.public_url = url.to_owned();
        let socks5 = self.proxy.socks5.trim();
        if !socks5.is_empty() && infinidysk_proxy::Socks5Url::parse(Some(socks5)).is_none() {
            return Err(invalid(
                "proxy.socks5",
                "Config value for 'proxy.socks5' must be a socks5://host:port URL (socks5h:// \
                 is accepted) without credentials, query, or fragment"
                    .to_owned(),
            ));
        }
        for entry in infinidysk_proxy::BypassMatcher::split_entries(Some(&self.proxy.socks5_bypass))
        {
            if !infinidysk_proxy::BypassMatcher::is_valid_entry(entry) {
                return Err(invalid(
                    "proxy.socks5_bypass",
                    format!(
                        "Config value for 'proxy.socks5_bypass' contains an invalid proxy \
                         exception '{entry}': expected a host name, a domain suffix \
                         (.example.com or *.example.com), an IP address, a CIDR range, or '*'"
                    ),
                ));
            }
        }
        check(
            "repair.readahead_window_seconds",
            "Read-ahead window seconds",
            self.repair.readahead_window_seconds,
            &infinidysk_stream::readahead::WINDOW_SECONDS_RANGE,
        )?;
        self.auth.oidc.validate()?;
        if let Some(limit) = self.network.download_limit_mb_per_s {
            check(
                "network.download_limit_mb_per_s",
                "Download limit MB/s",
                limit,
                &DOWNLOAD_LIMIT_MB_PER_S,
            )?;
        }
        if self.imports.strategy == ImportStrategy::Strm && self.network.public_url.is_empty() {
            return Err(invalid(
                "imports.strategy",
                "STRM files need a public URL, the address media servers play them from".to_owned(),
            ));
        }
        let mut ids = HashSet::new();
        for p in &mut self.providers {
            if !ids.insert(p.id) {
                return Err(invalid(
                    "providers",
                    format!("Duplicate provider id {}", p.id),
                ));
            }
            p.resolve()?;
        }
        ids.clear();
        for a in &self.arr_instances {
            if !ids.insert(a.id) {
                return Err(invalid(
                    "arr_instances",
                    format!("Duplicate Arr instance id {}", a.id),
                ));
            }
            a.validate()?;
        }
        Ok(())
    }
}

impl Provider {
    fn label(&self) -> &str {
        [&self.name, &self.hostname]
            .into_iter()
            .find(|s| !s.is_empty())
            .or(self.profile.as_ref())
            .map_or("", String::as_str)
    }

    pub(crate) fn resolve(&mut self) -> Result<(), Error> {
        const P: &str = "providers";
        let label = self.label().to_owned();
        check(
            P,
            &format!("Provider {label}: max connections"),
            self.max_connections,
            &MAX_CONNECTIONS,
        )?;
        check(
            P,
            &format!("Provider {label}: pipeline depth"),
            self.pipeline_depth,
            &PIPELINE_DEPTH,
        )?;
        if let Some(quota) = &self.quota {
            check(
                P,
                &format!("Provider {label}: quota GB"),
                quota.gb,
                &QUOTA_GB,
            )?;
            check(
                P,
                &format!("Provider {label}: quota reset day"),
                quota.reset_day,
                &RESET_DAY,
            )?;
        }
        if let Some(id) = &self.profile {
            let profile = catalog::profile(id).ok_or_else(|| {
                invalid(
                    P,
                    format!("Provider {label}: unknown catalog profile {id:?}"),
                )
            })?;
            let server = if self.hostname.is_empty() {
                profile.servers.first()
            } else {
                profile
                    .servers
                    .iter()
                    .find(|s| s.hostname.eq_ignore_ascii_case(&self.hostname))
            }
            .ok_or_else(|| {
                invalid(
                    P,
                    format!(
                        "Provider {label}: {} is not a {} server",
                        self.hostname, profile.name
                    ),
                )
            })?;
            server.hostname.clone_into(&mut self.hostname);
            self.port = server.port;
            self.backbone = Some(server.backbone.to_owned());
            if self.name.is_empty() {
                profile.name.clone_into(&mut self.name);
            }
        } else {
            if self.hostname.is_empty()
                || self
                    .hostname
                    .contains(|c: char| c.is_whitespace() || c == '/')
            {
                return Err(invalid(
                    P,
                    format!("Provider {label}: invalid hostname {:?}", self.hostname),
                ));
            }
            check(P, &format!("Provider {label}: port"), self.port, &PORT)?;
            if let Some(b) = &self.backbone
                && catalog::backbone(b).is_none()
            {
                return Err(invalid(
                    P,
                    format!(
                        "Provider {label}: unknown backbone {b:?}; pick one from the catalog or none"
                    ),
                ));
            }
            if self.name.is_empty() {
                self.name.clone_from(&self.hostname);
            }
        }
        Ok(())
    }
}

impl ArrInstance {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        const A: &str = "arr_instances";
        let label = &self.name;
        if label.trim().is_empty() {
            return Err(invalid(A, "Arr instance name must not be empty".to_owned()));
        }
        let rest = self
            .url
            .strip_prefix("https://")
            .or_else(|| self.url.strip_prefix("http://"));
        if rest.is_none_or(str::is_empty) {
            return Err(invalid(
                A,
                format!("Arr instance {label}: URL must start with http:// or https://"),
            ));
        }
        if self.api_key.0.is_empty() {
            return Err(invalid(
                A,
                format!("Arr instance {label}: API key is required"),
            ));
        }
        Ok(())
    }
}

struct Override {
    path: Vec<String>,
    value: Value,
}

/// Effective settings plus the layers they came from.
pub struct ConfigStore {
    db: Db,
    stored: Map<String, Value>,
    env: Vec<Override>,
    settings: Settings,
    /// The disk cache folder when none is saved; validation checks it against the mount.
    default_cache_dir: String,
    /// Wakes subscribers (e.g. the Prowlarr sync loop) after a successful save.
    changed: Arc<tokio::sync::Notify>,
}

impl ConfigStore {
    /// Loads stored settings and overlays `vars` (normally `std::env::vars()`).
    pub async fn load(
        db: Db,
        vars: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, Error> {
        Self::load_in(db, vars, DEFAULT_CACHE_DIR.to_owned()).await
    }

    /// [`Self::load`], checking the disk cache in `default_cache_dir` when none is saved.
    pub(crate) async fn load_in(
        db: Db,
        vars: impl IntoIterator<Item = (String, String)>,
        default_cache_dir: String,
    ) -> Result<Self, Error> {
        let defaults = serde_json::to_value(Settings::default())?;
        let mut env: Vec<Override> = vars
            .into_iter()
            .filter_map(|(key, raw)| {
                let raw_path: Vec<String> = key
                    .strip_prefix(ENV_PREFIX)?
                    .split("__")
                    .map(str::to_ascii_lowercase)
                    .collect();
                // Sections serialized camelCase (play.*, preflight.*, …) are addressed by
                // snake_case env segments: retry with each segment in camelCase.
                let path = if lookup(&defaults, &raw_path).is_some() {
                    raw_path
                } else {
                    let camel: Vec<String> = raw_path.iter().map(|s| snake_to_camel(s)).collect();
                    if lookup(&defaults, &camel).is_some() {
                        camel
                    } else {
                        raw_path
                    }
                };
                let Some(default) = lookup(&defaults, &path) else {
                    tracing::warn!(key, "ignoring unknown config environment variable");
                    return None;
                };
                // Strings stay verbatim so a numeric password is not parsed as a number.
                let value = if default.is_string() {
                    Value::String(raw)
                } else {
                    serde_json::from_str(&raw).unwrap_or(Value::String(raw))
                };
                Some(Override { path, value })
            })
            .collect();

        let mut stored = Map::new();
        for (key, value) in db.config().await? {
            stored.insert(key, serde_json::from_str(&value)?);
        }
        let settings = effective(&stored, &env, &default_cache_dir)?;
        publish_proxy(&settings);
        // Freeze generated ids and catalog fields so env-defined lists round-trip through saves.
        let resolved = serde_json::to_value(&settings)?;
        for o in &mut env {
            if let Some(v) = lookup(&resolved, &o.path) {
                o.value = v.clone();
            }
        }
        Ok(Self {
            db,
            stored,
            env,
            settings,
            default_cache_dir,
            changed: Arc::new(tokio::sync::Notify::new()),
        })
    }

    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Fills every empty server secret (API keys, session key, `WebDAV` password and
    /// download key, STRM key) with a random one and stores it.
    pub async fn generate_missing_secrets(&mut self) -> Result<(), Error> {
        let mut next = self.settings.clone();
        let mut changed = false;
        for secret in [
            &mut next.sab.api_key,
            &mut next.auth.api_key,
            &mut next.auth.session_key,
            &mut next.webdav.password,
            &mut next.webdav.download_key,
            &mut next.imports.strm_key,
        ] {
            if secret.0.is_empty() {
                secret.0 = Uuid::new_v4().simple().to_string();
                changed = true;
            }
        }
        if changed {
            // An unchanged RC password is not entered, so it binds no env URL.
            if !next.rclone_rc.password.0.is_empty() {
                next.rclone_rc.password = Secret(MASK.to_owned());
            }
            self.save(next).await?;
        }
        Ok(())
    }

    /// Dotted paths fixed by environment variables.
    pub fn locked(&self) -> impl Iterator<Item = String> + '_ {
        self.env.iter().map(|o| o.path.join("."))
    }

    fn env_sets(&self, path: [&str; 2]) -> bool {
        env_sets(&self.env, path)
    }

    /// Whether an `INFINIDYSK__` variable pins this settings path (v1 `IsEnvironmentManaged`).
    pub(crate) fn env_managed(&self, path: [&str; 2]) -> bool {
        self.env_sets(path)
    }

    /// Wakes on the next successful [`Self::save`] (v1 `ConfigManager.OnConfigChanged`).
    pub(crate) fn changed(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.changed)
    }

    /// The RC password about to be stored stays with the server it was stored for, also
    /// when environment variables hide either of them.
    fn check_stored_rc(&self, tree: &Value, password_entered: bool) -> Result<(), Error> {
        let stored = Value::Object(self.stored.clone());
        let text = |tree: &Value, path: [&str; 2]| {
            let path = path.map(str::to_owned);
            lookup(tree, &path)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let password = text(tree, RC_PASSWORD);
        let replaced = password_entered && !self.env_sets(RC_PASSWORD);
        if password.is_empty()
            || replaced
            || password != text(&stored, RC_PASSWORD)
            || RcloneRc::origin(&text(tree, RC_URL)) == RcloneRc::origin(&text(&stored, RC_URL))
        {
            return Ok(());
        }
        Err(if self.env_sets(RC_PASSWORD) {
            invalid(
                "rclone_rc.url",
                "INFINIDYSK__RCLONE_RC__PASSWORD hides a saved RC password that belongs to \
                 the saved RC URL. Keep that URL, or remove the variable and clear the \
                 saved password first"
                    .to_owned(),
            )
        } else {
            invalid(
                "rclone_rc.password",
                "The RC URL points somewhere new: enter its password, or clear it".to_owned(),
            )
        })
    }

    /// Validates and persists `new`. [`MASK`] secrets keep their current value.
    pub async fn save(&mut self, mut new: Settings) -> Result<(), Error> {
        let (rc, current_rc) = (&new.rclone_rc, &self.settings.rclone_rc);
        let rc_password_entered = rc.password.0 != MASK && !rc.password.0.is_empty();
        // An env password goes wherever the URL points, however the request sends it.
        if (rc.password.0 == MASK || self.env_sets(RC_PASSWORD))
            && !current_rc.password.0.is_empty()
            && !rc.url.trim().is_empty()
            && RcloneRc::origin(&rc.url) != RcloneRc::origin(&current_rc.url)
        {
            return Err(if self.env_sets(RC_PASSWORD) {
                invalid(
                    "rclone_rc.url",
                    "The RC password comes from INFINIDYSK__RCLONE_RC__PASSWORD, so it \
                     only goes to the current RC server. Set INFINIDYSK__RCLONE_RC__URL \
                     to point it somewhere new"
                        .to_owned(),
                )
            } else {
                invalid(
                    "rclone_rc.password",
                    "The RC URL points somewhere new: enter its password, or clear it".to_owned(),
                )
            });
        }
        new.restore_secrets(&self.settings);
        new.validate_in(&self.default_cache_dir)?;
        let mut tree = serde_json::to_value(&new)?;
        let current = serde_json::to_value(&self.settings)?;
        let stored = Value::Object(self.stored.clone());
        for o in &self.env {
            if lookup(&tree, &o.path) != lookup(&current, &o.path) {
                return Err(Error::Locked(o.path.join(".")));
            }
            match lookup(&stored, &o.path) {
                Some(v) => set(&mut tree, &o.path, v.clone()),
                None => remove(&mut tree, &o.path),
            }
        }
        // A password entered for an env-set URL is stored with that URL, so a later
        // startup sends it only there.
        if rc_password_entered && self.env_sets(RC_URL) && !self.env_sets(RC_PASSWORD) {
            let path = RC_URL.map(str::to_owned);
            set(&mut tree, &path, Value::String(new.rclone_rc.url.clone()));
        }
        self.check_stored_rc(&tree, rc_password_entered)?;

        let defaults = serde_json::to_value(Settings::default())?;
        let mut next = Map::new();
        if let Value::Object(sections) = tree {
            for (key, value) in sections {
                if let Some(v) = diff(value, defaults.get(&key)) {
                    next.insert(key, v);
                }
            }
        }
        let set: Vec<(&str, String)> = next
            .iter()
            .filter(|(k, v)| self.stored.get(*k) != Some(v))
            .map(|(k, v)| (k.as_str(), v.to_string()))
            .collect();
        let set: Vec<(&str, &str)> = set.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // Keys the current build does not know stay, so a downgrade does not lose them.
        let remove: Vec<&str> = self
            .stored
            .keys()
            .filter(|k| defaults.get(k).is_some() && !next.contains_key(*k))
            .map(String::as_str)
            .collect();
        self.db.write_config(&set, &remove).await?;

        for (k, v) in &self.stored {
            if defaults.get(k).is_none() {
                next.insert(k.clone(), v.clone());
            }
        }
        self.settings = effective(&next, &self.env, &self.default_cache_dir)?;
        publish_proxy(&self.settings);
        self.stored = next;
        self.changed.notify_waiters();
        Ok(())
    }
}

/// Publishes the outbound proxy settings to the process-wide state that connections read
/// per connect, so a save applies to new connections.
fn publish_proxy(settings: &Settings) {
    infinidysk_proxy::set_global(infinidysk_proxy::OutboundProxy::new(
        Some(settings.proxy.socks5.as_str()),
        Some(settings.proxy.socks5_bypass.as_str()),
    ));
}

fn effective(
    stored: &Map<String, Value>,
    env: &[Override],
    default_cache_dir: &str,
) -> Result<Settings, Error> {
    let mut tree = serde_json::to_value(Settings::default())?;
    merge(&mut tree, Value::Object(stored.clone()));
    for o in env {
        set(&mut tree, &o.path, o.value.clone());
    }
    reject_readarr(&tree)?;
    let mut settings: Settings = serde_json::from_value(tree)?;
    settings.reset_invalid();
    settings.validate_in(default_cache_dir)?;
    check_rc_url_override(stored, env, &settings.rclone_rc)?;
    Ok(settings)
}

const RC_URL: [&str; 2] = ["rclone_rc", "url"];
const RC_PASSWORD: [&str; 2] = ["rclone_rc", "password"];

fn env_sets(env: &[Override], path: [&str; 2]) -> bool {
    env.iter().any(|o| o.path == path)
}

/// The saved RC password belongs to the saved RC URL; an env URL for another server
/// stops startup instead of sending it there.
fn check_rc_url_override(
    stored: &Map<String, Value>,
    env: &[Override],
    rc: &RcloneRc,
) -> Result<(), Error> {
    if !env_sets(env, RC_URL) || env_sets(env, RC_PASSWORD) || rc.password.0.is_empty() {
        return Ok(());
    }
    let saved = stored
        .get("rclone_rc")
        .and_then(|section| section.get("url"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if RcloneRc::origin(saved) == RcloneRc::origin(&rc.url) {
        return Ok(());
    }
    Err(invalid(
        "rclone_rc.url",
        "INFINIDYSK__RCLONE_RC__URL points to another server than the saved RC URL, so \
         the saved RC password is not sent there. Set INFINIDYSK__RCLONE_RC__PASSWORD \
         too, or remove INFINIDYSK__RCLONE_RC__URL and clear the saved password"
            .to_owned(),
    ))
}

/// Readarr support was dropped; an instance left from before stops startup with what
/// to do, not a parse error.
fn reject_readarr(tree: &Value) -> Result<(), Error> {
    let Some(arr) = tree["arr_instances"]
        .as_array()
        .and_then(|arrs| arrs.iter().find(|a| a["profile"] == "readarr"))
    else {
        return Ok(());
    };
    Err(invalid(
        "arr_instances",
        format!(
            "Arr instance \"{}\" is a Readarr, which infinidysk no longer supports. Remove \
             it first: in the previous version under Settings > Automation > Arr \
             instances, or from INFINIDYSK__ARR_INSTANCES. Then upgrade and add Chaptarr.",
            arr["name"].as_str().unwrap_or_default()
        ),
    ))
}

/// `watchdog_enabled` → `watchdogEnabled`, for env overrides of camelCase sections.
fn snake_to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for c in s.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn lookup<'a>(tree: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(tree, |node, key| node.get(key))
}

fn set(tree: &mut Value, path: &[String], value: Value) {
    let mut node = tree;
    for key in path {
        if !node.is_object() {
            *node = Value::Object(Map::new());
        }
        let Value::Object(map) = node else { return };
        node = map.entry(key.clone()).or_insert(Value::Null);
    }
    *node = value;
}

fn remove(tree: &mut Value, path: &[String]) {
    if let Some((last, parents)) = path.split_last()
        && let Some(Value::Object(map)) =
            parents.iter().try_fold(tree, |node, key| node.get_mut(key))
    {
        map.remove(last);
    }
}

/// Objects merge key by key; anything else replaces.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(slot) => merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

/// The parts of `value` that differ from `default`; lists compare whole.
fn diff(value: Value, default: Option<&Value>) -> Option<Value> {
    match (value, default) {
        (Value::Object(map), Some(Value::Object(defaults))) => {
            let out: Map<String, Value> = map
                .into_iter()
                .filter_map(|(k, v)| {
                    let d = defaults.get(&k);
                    diff(v, d).map(|v| (k, v))
                })
                .collect();
            (!out.is_empty()).then_some(Value::Object(out))
        }
        (value, Some(default)) if &value == default => None,
        (value, _) => Some(value),
    }
}
