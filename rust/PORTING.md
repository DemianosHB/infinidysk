# Porting the v1 fork to Rust (on the 2.x base)

This tree is the Rust port base of the DemianosHB fork: the 2.x workspace vendored from
[infinidysk/infinidysk](https://github.com/infinidysk/infinidysk) `main` at
`89db51738ba6517c72f7972fbc3f7d6d68c13008` (with the `rapidyenc` submodule content inlined at
its pinned commit, so the tree builds standalone). The fork's v1-only features are ported
into this workspace on top, one unit at a time. The C#/.NET code on branch `v1` is the
**specification**: read it and its tests as the behavior to reproduce, write idiomatic
Rust, no line-by-line ports. Behavior that must stay identical: the SABnzbd API
(`/api?mode=…`) and the WebDAV paths/semantics (`/view`, `/content`, `/nzbs`, `/.ids`,
`/completed-symlinks`).

Porting rules (per unit):

1. Port one unit at a time; a unit is done only when its checks pass: `cargo fmt --check`,
   `cargo clippy --all-targets -- -D warnings` on the touched crates, and the ported unit
   tests (`cargo test -p <crate>`). V1's tests are the characterization suite — port the
   cases, never weaken them.
2. No `unwrap`/`expect`/`panic!` in non-test code; return `Err` where the original throws.
   `#[expect(lint, reason = "…")]` instead of `#[allow]`. No `unsafe` outside FFI crates.
3. New crates live under `crates/<short-name>` (package `infinidysk-<name>`); dependencies
   point downward only. Follow the vendored `AGENTS.md` and `.agents/skills/`.
4. Never write media/release titles in code, tests, commits, or docs.

Build and test (from `rust/`):

```bash
cargo test -p infinidysk-proxy -p infinidysk-nntp -p infinidysk-stream
cargo test -p infinidysk --test config
cargo clippy --workspace --all-targets -- -D warnings   # full gate, or per PR CI
```

## Status of the v1-fork features

| Fork feature (v1, C#) | v1 keys / location | Status in this Rust port |
|---|---|---|
| SOCKS5 outbound proxy + bypass | `general.socks5-proxy`, `general.socks5-proxy-bypass`; `backend/Utils/OutboundProxy.cs`, `ProxyBypassMatcher.cs`, `libs/UsenetSharp/Clients/UsenetClient.Socks5.cs` | **Ported** — `crates/proxy` (URL parse, bypass matcher, local-destination rules, RFC 1928/1929 handshake), wired into the NNTP dial path (`crates/nntp`) and config (`proxy.socks5`, `proxy.socks5_bypass`). Indexer HTTP now goes through it too (`crates/indexer`, reqwest `socks` feature); remaining: other reqwest clients (e.g. NZB fetch). |
| Read-ahead PAR2 repair | `repair.readahead.enabled`, `repair.readahead.window-seconds`; `backend/Streams/MultiSegmentStream.cs` (`ComputeReadAheadWindowBytes`, `SampleDeliveryRate`), `backend/Services/Repair/Par2RepairTriggerSink.cs` | **Ported (first slice)** — `crates/stream/src/readahead.rs` (window bytes + delivery-rate EMA, v1 test cases ported), `SegmentReader::with_read_ahead` widens the prefetch window to rate × window capped by the article budget; config `repair.readahead_enabled` / `repair.readahead_window_seconds`, applied via `Vfs::set_read_ahead_repair`, gated on the `par2-repair` experiment (v1: `repair.par2-enabled`). Follow-up: streaming-triggered repairs jumping the health-check repair queue (v1 queue jump in `Par2RepairService`). |
| Search result domain logic (language detection, quality parse, result ordering) | `backend/Utils/AudioLanguagePreference.cs`, `SubtitlePreference.cs`, `ReleaseQuality.cs`, `ProfileResultSorter.cs`, `ProfileConfig.QualitySortMode` | **Ported (pure domain logic)** — new crate `crates/search` (`infinidysk-search`): `language` (token fold/alias, subtitle rank), `audio` (title-token detection with lookaround guards, indexer-attribute-wins, preferred-language rank bias), `quality` (resolution/source ranks), `sorter` (`QualitySortMode` + stable language→quality→grabs→size→posted ordering), `result` (`SearchResult`, the v1 `NewznabItem`). V1 test cases ported as characterization tests (37 tests). New deps flagged: `fancy-regex` (v1 parsing uses lookaround), `unicode-normalization` (v1 folds accents via NFD). |
| Newznab indexer HTTP client + SOCKS5 for indexer traffic | `backend/Clients/Indexers/NewznabClient.cs`, `backend/Utils/ProxyHttpClientPool.cs` | **Ported** — new crate `crates/indexer` (`infinidysk-indexer`): `/api` URL normalisation, `apikey`/`extended=1` prepended, RFC 3986 query escaping, one retry on the first transport failure, bounded body read (declared Content-Length checked before streaming; stream-count rejection at the limit), DTD-prohibited XML parse (quick-xml mini-DOM with namespace tracking), `<error>` protocol errors, controlled error messages that never leak URL/API key/body. HTTP via reqwest; when the Phase-1 process-wide SOCKS5 proxy is configured and the indexer host is not bypassed, requests tunnel as `socks5h://` (remote DNS), otherwise strict direct (`no_proxy`) — this wires indexer HTTP through the Phase-1 proxy (reqwest `socks` feature added). v1 tests ported against a loopback HTTP server (22 tests) plus a fake-SOCKS5 tunnel test and a loopback-bypass test. Documented deviations: hyper enforces declared Content-Length at the framing layer, so bytes beyond it are never delivered to the body (v1's raw-stream read could count them; over-limit rejection is covered by the chunked/undeclared paths); pubDate is RFC 2822 with the optional weekday stripped (jiff validates it, .NET did not). |
| Prowlarr pull-sync: API client, merge, per-indexer filter, config models | `backend/Clients/Prowlarr/ProwlarrClient.cs`, `backend/Services/ProwlarrIndexerSync.cs`, `backend/Services/IndexerResultFilter.cs`, `backend/Config/IndexerConfig.cs`, `ProfileConfig.cs` | **Ported (Phase 2.3)** — `crates/indexer`: shared HTTP transport `http.rs` (`build_client` routes through the Phase-1 SOCKS5 proxy as `socks5h://` unless bypassed, else strict direct; retry-once on transport errors; bounded body read; biased cancel-vs-timeout select), `prowlarr.rs` (`ProwlarrClient`: base-URL normalisation, `{base}/{id}/api`, `X-Api-Key`, UA `NzbDAV/{version}`, 15s timeout, no retry, 401/403 → rejected-key message, shape validation, JSON `null` → empty response, empty body → invalid JSON like STJ, timeout/cancel errors never leak the key), `models.rs` (`IndexerConfig`/`ConnectionDetails`/`ResultFilter` with per-indexer→global→default effective values and hard 16 MiB cap), `filter.rs` (fail-open drop rules + stable grabs-desc sort), `sync.rs` (`ProwlarrIndexerSync.Merge`: prowlarr-id ownership first-wins, reserved manual names, deterministic sort, tuning preservation, stale removal, profile rename/dedupe). `crates/search` gained `profile.rs` (`Profile`/`ProfileConfig`/`FallbackMode`, constant-time `find_by_token`, legacy migration) and serde on `QualitySortMode`. v1 tests ported (loopback HTTP server + raw header capture; 34 tests in the crate). |
| Search execution: per-indexer paging, rate limiter, hit tracker, exclude patterns, dedupe/sort, strict matching, fallback queries, IMDB/external-ID resolvers, play-token cache | `backend/Services/SearchProfileService.cs`, `NewznabRateLimiter.cs`, `IndexerHitTracker.cs`, `NzbResolutionCache.cs`, `TvdbIdResolver.cs`, `TmdbIdResolver.cs`, `ImdbTitleResolver.cs`, `ExternalIdResolver.cs`, `AnimeListMappingResolver.cs`; `backend/Utils/FilenameMatcher.cs`, `ProfileIdentityFilter.cs`, `ExcludePatternParser.cs` | **Ported (Phase 2.4)** — new crate `crates/profiles` (`infinidysk-profiles`): `filename` (head tokens, episode tags, title/year matching with fancy-regex lookarounds), `identity` (strict matching with canonical identity + 2-of-N head consensus fallback), `exclude` (bare/.NET + `/…/flags` JS-wrapper patterns, case-insensitive, dedup by key), `ratelimit` (per-indexer gate, refused try_wait consumes no budget), `hits` (search/download hit windows: fixed UTC reset hour or rolling 24h), `cache` (play tokens: one 16-hex token per candidate, shared ordered candidate list), `resolvers` (TVmaze/Wikidata/Kitsu + anime-lists dataset; keyless upstream APIs in v1, so no API keys — `ResolverConfig.enabled` is the offline switch; 24h/15min caches), `service` (`SearchService`: profile/indexer selection, 100-per-page paging capped at 50 pages with per-page hit re-check, per-indexer limits/UA/proxy/timeout, category overrides, fail-open per-indexer errors, dedupe by nzb_url first-wins, `sorter` ordering with preferred-audio bias, strict matching, Title/Broad fallback escalation, `verify_identity` guard, `build_imdb_query` incl. tvdb-/tmdb-/kitsu:/mal:/anilist: ids). Deviations: hit tracker and resolution cache are in-memory (v1 persists to EF tables — V2 `crates/db` wiring is a follow-up; prune is deterministic every 20th record instead of 5% random); exclude patterns have no 250 ms .NET match timeout (linear-time engine for non-lookaround patterns, short titles); anime-lists dataset is read whole (≤256 MiB) instead of streamed. Watchtower warming / Warden dead-hiding / preflight hooks are Phase 3 (`IsDeadHook`/`PreflightHook` slots exist). v1 characterization tests ported (67 tests incl. loopback-Newznab integration). |
| Search routes: `api/search-indexers`, `adapters/newznab/{token}/api`, `adapters/addon/{token}/manifest.json` | `backend/Api/Controllers/SearchIndexers`, `Profiles/Adapters/NewznabAdapterController.cs`, `ProfileManifestController.cs`, `ProfileAddonContracts.cs` | **Ported (Phase 2.5)** — `crates/infinidysk/src/search.rs`: `/api/search-indexers?q&limit` (q required, limit clamped to 1..=500 else 100; per-indexer hit-limit skip, rate limit, UA/proxy/timeout, strict-match title filter via `FilenameMatcher.Matches`, fail-open per-indexer statuses with elapsed ms, dedupe by `NzbUrl` first-wins, posted-desc ordering; auth = session/admin key or `apikey` query/`X-Api-Key` vs `sab.api_key`/`auth.api_key` like v1 `ApiKeyValidator`). Newznab adapter: `t=caps` advertises the max returnable limit, `t=movie&imdbid` / `t=tvsearch&imdbid&season&ep` run the profile search (`SearchByImdbAsync`) and return an RSS feed whose items link to `/api/search/{token}/nzb/{playToken}.nzb` (the NZB fetch route landed in Phase 2.6); unknown token / disabled adapter → 404; `Cache-Control: no-store`. Addon manifest: CORS (`*`, GET+OPTIONS, no-store), id `nzbdav.profile.{token}`, resources `["stream"]`, types movie/series, idPrefixes tt/tmdb/tvdb/kitsu/mal/anilist. Settings sections added (`Settings`): `search` (`exclude_patterns`, `user_agent`, `search_user_agent`, `resolvers_enabled`), `indexers` (`IndexerConfig`), `profiles` (`ProfileConfig`), `prowlarr` (`url`, `api_key`, `sync_enabled`, `sync_interval_minutes` clamped 5..=10080 default 60, persisted `sync_status`). Wired in `app.rs` via `search::router` + a `SearchService` whose settings snapshot is re-applied per request (`apply_settings`). Per-indexer/global HTTP proxy for indexer traffic is honored (`NewznabClient::with_proxy` → `http::build_client_with_proxy`). Route tests in `crates/infinidysk/tests/search.rs` (auth 401/400/200, dedupe+ordering, caps XML, movie feed with play tokens, manifest + CORS, adapter gating). |
| Preferred audio languages per profile | profiles + watchdog fallback ordering | Domain logic ported (`crates/search`); wired through the profile settings + sorter in Phase 2.5. |
| NZB proxy, JSON lookup, failover-order report | `backend/Api/Controllers/Profiles/Adapters/NzbProxyController.cs`, `JsonSearchController.cs`, `FailoverOrderController.cs`; `backend/Services/NzbFetchCoalescer.cs`, `PreferredOrderStore.cs`; `backend/Utils/ReleaseIdentity.cs`, `ProfileReleaseName.cs` | **Ported (Phase 2.6)** — `crates/infinidysk/src/search.rs`: `GET /api/search/{token}/nzb/{playToken}.nzb` (json|newznab adapter gate, play-token resolution, coalesced bounded fetch in `nzb_fetch.rs` — 50 MiB cap, 60 s fetch timeout under a 90 s hard cap, 5 min/256 MiB/64-entry success cache — per-indexer proxy + skip-TLS, `application/x-nzb` attachment with sanitized filename; cancel → empty 200, fetch failure → 502 like v1), `GET|OPTIONS /api/search/{token}/lookup` (type/id required → 400, series `id:s:e` rewrite, JSON results with `playUrl`), `POST|OPTIONS /adapters/addon/{token}/failover_order` (CORS POST, `{ok,total,matched,unmatched,titles}`, 400 on no ids). `crates/profiles/src/preferred.rs`: `PreferredOrderStore` (10 min TTL) + `ReleaseIdentity`/`WardenFingerprint` keys. The `adapters/addon/{token}/stream/{type}/{id}.json` and `play/{nzbToken}.mkv` routes need the Phase-3 stream-state/preflight/play services and land with Phase 3. |
| Prowlarr pull-sync service + status route | `backend/Services/ProwlarrSyncService.cs`, `backend/Api/Controllers/ProwlarrSync`, `backend/Config/ProwlarrSyncStatus.cs` | **Ported (Phase 2.6)** — `crates/infinidysk/src/prowlarr_sync.rs`: background loop (15 s startup delay, 30 s tick, gate = `Semaphore(1)`) that syncs on the interval since the persisted `lastAttemptAt` and on connection-config changes (observed via a `Notify` fired by `ConfigStore::save` + the tick; v1's `OnConfigChanged` event is therefore observed within at most one tick). `GET|POST /api/prowlarr-sync` behind the same API-key rule as v1 `BaseApiController`; POST returns the snapshot plus the indexer config JSON with `apiKey` masked and the profile config, only on success. Merge + env-managed guards (`INFINIDYSK__INDEXERS__INDEXERS` / `INFINIDYSK__PROFILES__PROFILES` via `ConfigStore::env_managed`) use `indexer::sync::merge`; results persist through `ConfigStore::save` (single write lock, connection re-check like v1) and are pushed into the `SearchService` via `apply_settings`. Failure attempts persist `lastAttemptAt`/`lastError` with zeroed counters. Route tests in `crates/infinidysk/tests/prowlarr_sync.rs` (merge + masked response + persistence, failure path, env-managed refusal, unconfigured). |
| Play routes (addon stream listing, play .mkv, hedged pre-verify/commit, queue enqueue, stall failover, watchdog entries) | `backend/Api/Controllers/Profiles/ProfileReadController.cs`, `ProfilePlayController.cs`; `play.stall-*`, `grab.stall-failover-*` | **Ported (Phase 3)** — `crates/infinidysk/src/play.rs` + `crates/db/src/play.rs`; stall negative marking in `CandidateNegativeCache` (`crates/infinidysk/src/preflight.rs`) |
| Stall negative marking (play fallback) | `play.stall-negative-marking-*`; watchdog/play fallback | **Ported (Phase 3)** — `play.rs` + `preflight::CandidateNegativeCache`; `INFINIDYSK__PLAY__STALL_NEGATIVE_MARKING_*` |
| Warden dead-list + sources | `backend/Services/Warden*`, `Api/Controllers/Warden*`; `warden.*` keys | **Ported (Phase 3)** — `crates/db/src/warden.rs` (migration 0013) + `crates/infinidysk/src/warden.rs` (`/api/get-warden`, `/api/warden-*`), full-trust + quorum with backbone scope; feeds `IsDeadHook` for search and watchtower |
| Preflight orchestrator + fast verifier + negative cache | `backend/Services/Preflight*`; `preflight.*` keys | **Ported (Phase 3)** — `crates/infinidysk/src/preflight.rs`: cache, session registry, coalesced NZB fetch, stat-verify with sample count/timeout, `PreflightHook` from search; wired into play commit loop |
| Watchdog entries + purge | `backend/Services/WatchdogLog.cs`, `WatchdogPurgeService.cs`, `Api/Controllers/*Watchdog*` | **Ported (Phase 3)** — `crates/db/src/watchdog.rs` (migration 0013) + play-route writes |
| Watchtower (list sources, expanders, resolve, keep-fresh, admin routes) | `backend/Services/Watchtower/*`, `backend/Api/Controllers/Watchtower/*`; `watchtower.*` keys; `WATCHTOWER__*` env | **Ported (Phase 3)** — `crates/db/src/watchtower.rs` (migration 0014: `list_sources`, `wanted_items`) + `crates/infinidysk/src/watchtower.rs`: `ListSourceEnumerator` (Stremio catalogs/URL lists/discovery), `EpisodeEnumerator` (TVmaze/Kitsu), background loop (source sync → expand → resolve → keep-fresh with backoff + daily budget), routes `/api/get-watchtower`, `/api/watchtower-mutate` (13 actions), `/api/watchtower-discover-catalogs`, API-key gated; `TryWarmCacheAsync` hooked into play. Deviations: `ActiveCycle`/CycleLifetime → plain 15-min per-cycle timeout; EF change-tracker detach → read-modify-write under `Db::write_permit`; benchmark-gate pause not wired; episode enumeration hits TVmaze/Kitsu directly (v1's Wikidata path for non-IMDb ids not ported — IMDb and `kitsu:` ids only); provenance stores the simple (hyphen-less) GUID form of source ids; `WtPointer`/shortlist JSON stays PascalCase like v1 STJ; raw content-id titles (no name from the source) are resolved to display titles via `ImdbTitleResolver` (TVmaze/Wikidata) on sync/expand/resolve/keep-fresh/manual add, with `SxxEyy` suffixes for episode ids |
| v1-fork web UI pages (Watchtower, Watchdog, Search, Warden/Indexers/Profiles/Prowlarr settings) | v1 `frontend/app/routes/{watchtower,watchdog,search}` + `settings/{watchtower,watchdog,warden,indexers,profiles}` | **Ported (Phase 4)** — `rust/ui/src/pages/watchtower.tsx`, `watchdog.tsx`, `search.tsx`, `warden.tsx`, `fork-settings.tsx` + `src/api/fork.ts` (typed fetch for the v1-style API-key-gated routes, which are not in the generated OpenAPI). Backend additions: `GET /api/get-watchdog-entries`, `POST /api/clear-watchdog-entries` (`crates/infinidysk/src/watchdog.rs`, provider nicknames resolved from current config like v1), `POST /api/v1/queue/from-url` (search page "Queue", like SAB `addurl`). Layout stays responsive: sidebar at `lg+`, hamburger sheet below; all new pages use the scrollable `Table` (overflow-x) and wrap forms in flex rows, so they work on phones. |


## v1 features upstream 2.x does not plan (the reason this fork ports them here)

Search, indexers and Prowlarr; Watchtower, Preflight and Warden; Addon routes — all
"not planned" in upstream 2.x (see its AGENTS.md scope rule and the feature comparison).
The v1 C# code on branch `v1` is the only specification for them:
`backend/Api/Controllers/SearchIndexers`, `Profiles`, `ProwlarrSync`,
`backend/Services/SearchProfileService.cs`, `ProwlarrIndexerSync.cs`,
`backend/Services/Preflight*`, `Warden*`, `Watchtower/`, and the addon routes under
`backend/Api/`. Rough size in v1 (including tests, overlapping): search/indexer ~33k
LOC, watchtower/preflight/warden ~18k LOC, addon ~2.6k LOC.

Planned order (decided with the maintainer): Phase 1 = SOCKS5 proxy + read-ahead PAR2
(done); **Phase 2 = search / indexers / Prowlarr / profiles + addon routes +
preferred-languages bias (in progress — 2.1 domain logic `crates/search`, 2.2 Newznab
client, 2.3 Prowlarr client/sync/filter/models, 2.4 search execution `crates/profiles`
done; remaining: 2.5 routes + Settings keys, 2.6 wiring + Prowlarr background sync);
Phase 3 = Watchdog / Preflight / Warden / Watchtower + stall negative marking (**done**); then
release + provider failover; Phase 4 = UI port into `ui/` (**fork pages done**), extension of the one-time C#
`/config` importer (`crates/import`) for the fork keys, CI and images for the Rust
line.** Failover rule (decided): read-ahead PAR2 repair runs first; failover to another
release/provider only engages when the read-ahead repair did not fix the failure
(repair-first, failover-second). Release failover must work mid-stream: if the currently
streamed release errors at all providers, switch to another release during the running
stream (the swap mechanism lives in the vfs fence/`ReadVersion`/`LiveReader` machinery;
the candidate knowledge comes from Phase 2/3).

## Upstream 2.x issues fixed in this fork

- [infinidysk#1969](https://github.com/infinidysk/infinidysk/issues/1969) — STRM handoffs
  are group-writable again: handoff files are created `0666` and folders `0777` so the
  process umask decides (1.x behavior, instead of the 2.x `0644`/`0755` regression), and
  the optional `imports.handoff_permissions` (3-4 octal digits, e.g. `"2775"`) chmods them
  exactly, whatever the umask. SAB clients see it as `misc.permissions` in `get_config`.

## v1 config key → Rust setting mapping

| v1 key (`NZBDAV_CONFIG__…`) | Rust setting (`INFINIDYSK__…`) |
|---|---|
| `GENERAL__SOCKS5_PROXY` | `PROXY__SOCKS5` |
| `GENERAL__SOCKS5_PROXY_BYPASS` | `PROXY__SOCKS5_BYPASS` |
| `REPAIR__READAHEAD__ENABLED` | `REPAIR__READAHEAD_ENABLED` |
| `REPAIR__READAHEAD__WINDOW_SECONDS` | `REPAIR__READAHEAD_WINDOW_SECONDS` |
| `PLAY__STALL_NEGATIVE_MARKING_*` | `INFINIDYSK__PLAY__STALL_NEGATIVE_MARKING_*` (Phase 3) |
| profiles' preferred audio languages | `INFINIDYSK__PROFILES__PROFILES[*].preferredAudioLanguages` (Phase 2) |
| `SEARCH__EXCLUDE_PATTERNS` (fallback `PLAY__EXCLUDE_PATTERNS`) | `INFINIDYSK__SEARCH__EXCLUDE_PATTERNS` |
| `API__USER_AGENT` / `API__SEARCH_USER_AGENT` | `INFINIDYSK__SEARCH__USER_AGENT` / `INFINIDYSK__SEARCH__SEARCH_USER_AGENT` |
| `INDEXERS__INSTANCES` | `INFINIDYSK__INDEXERS__INDEXERS` (plus `INDEXERS__*` scalars) |
| `PROFILES__INSTANCES` | `INFINIDYSK__PROFILES__PROFILES` |
| `PROWLARR__URL` / `PROWLARR__API_KEY` / `PROWLARR__SYNC_ENABLED` / `PROWLARR__SYNC_INTERVAL_MINUTES` / `PROWLARR__SYNC_STATUS` | `INFINIDYSK__PROWLARR__*` |

The one-time importer (`crates/import`) reads the C# `/config`; it must be extended with
these mappings when the Rust line ships.
