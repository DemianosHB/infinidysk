//! Search profiles and search execution, ported from the C# v1 fork
//! (`backend/Services/SearchProfileService.cs`, `backend/Utils/FilenameMatcher.cs`,
//! `ProfileIdentityFilter.cs`, `ExcludePatternParser.cs`, `NewznabRateLimiter.cs`,
//! `IndexerHitTracker.cs`, `NzbResolutionCache.cs`, `backend/Services/*IdResolver.cs`).
//!
//! Phase 2 of the v1-fork port: everything needed to run a profile-scoped Newznab
//! search — per-indexer paging, rate limiting, hit tracking, exclude patterns,
//! strict identity matching, fallback queries, external-ID resolvers and the
//! play-token resolution cache. HTTP goes through the shared transport in
//! `infinidysk-indexer` (SOCKS5-aware). Watchtower/Warden/Preflight hooks are
//! wired in Phase 3.

pub mod cache;
pub mod exclude;
pub mod filename;
pub mod hits;
pub mod identity;
pub mod preferred;
pub mod ratelimit;

pub mod resolvers;
mod service;

pub use resolvers::{ResolverConfig, Resolvers};
pub use service::{SearchOutcome, SearchService, SearchSettings};
