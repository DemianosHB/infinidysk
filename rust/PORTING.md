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
| Preferred audio languages per profile | profiles + watchdog fallback ordering | Domain logic ported (`crates/search`); config/profile wiring pending — needs the search/profile stack (Phase 2). |
| Stall negative marking (play fallback) | `play.stall-negative-marking-*`; watchdog/play fallback | Not started — needs the watchdog/play-fallback stack. |

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
preferred-languages bias (in progress — pure domain logic done in `crates/search`);
Phase 3 = Watchdog / Preflight / Warden / Watchtower + stall negative marking; then
release + provider failover; Phase 4 = UI port into `ui/`, extension of the one-time C#
`/config` importer (`crates/import`) for the fork keys, CI and images for the Rust
line.** Failover rule (decided): read-ahead PAR2 repair runs first; failover to another
release/provider only engages when the read-ahead repair did not fix the failure
(repair-first, failover-second). Release failover must work mid-stream: if the currently
streamed release errors at all providers, switch to another release during the running
stream (the swap mechanism lives in the vfs fence/`ReadVersion`/`LiveReader` machinery;
the candidate knowledge comes from Phase 2/3).

## v1 config key → Rust setting mapping

| v1 key (`NZBDAV_CONFIG__…`) | Rust setting (`INFINIDYSK__…`) |
|---|---|
| `GENERAL__SOCKS5_PROXY` | `PROXY__SOCKS5` |
| `GENERAL__SOCKS5_PROXY_BYPASS` | `PROXY__SOCKS5_BYPASS` |
| `REPAIR__READAHEAD__ENABLED` | `REPAIR__READAHEAD_ENABLED` |
| `REPAIR__READAHEAD__WINDOW_SECONDS` | `REPAIR__READAHEAD_WINDOW_SECONDS` |
| `PLAY__STALL_NEGATIVE_MARKING_*` | Phase 3 |
| profiles' preferred audio languages | Phase 2 |

The one-time importer (`crates/import`) reads the C# `/config`; it must be extended with
these mappings when the Rust line ships.
