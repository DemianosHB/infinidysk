> [!WARNING]
> **This repository is the v1-only fork of InfiniDysk 1.x (C# / .NET 10), maintained by DemianosHB.** `v1` is the main line of development here, and 1.x is actively developed further in this fork. The 2.0 Rust rewrite lives upstream at [infinidysk/infinidysk](https://github.com/infinidysk/infinidysk) (`main`) and ships as separate images there; the fork's own Rust port of the v1 features (SOCKS5 proxy, read-ahead PAR2 repair, …) is developed under [`rust/`](rust/PORTING.md) on the upstream 2.x base — see [rust/PORTING.md](rust/PORTING.md).

> [!NOTE]
> **This repository is a community fork** of [infinidysk/infinidysk](https://github.com/infinidysk/infinidysk). It adds fork features on top of the upstream `v1` line (see [Fork features](#fork-features-demianoshb)) and publishes its own images to `ghcr.io/demianoshb/infinidysk`. Fork releases are tagged `vX.Y.Z-DemianosN`, e.g. [`v1.7.2-Demianos8`](https://github.com/DemianosHB/infinidysk/releases/tag/v1.7.2-Demianos8); pushes to `v1` additionally build the rolling `lang-dev` image.

> [!IMPORTANT] 
> InfiniDysk is designed as a drop-in replacement and upgrade from `nzbdav-dev/nzbdav v0.6.4`. Follow the [migration guide](https://www.infinidysk.com/getting-started/migration/) before switching an existing installation.
>
> Early adopters report **2x network throughput** and **seek times improved by up to 4x** compared with v0.6.4.
>
> `ghcr.io/demianoshb/infinidysk:latest`

<p align="center">
  <img src="docs/assets/logo.png" width="160" alt="InfiniDysk logo" />
</p>

<h1 align="center">InfiniDysk</h1>

<p align="center"><strong>The NzbDAV SuperFork</strong></p>
<p align="center">
  Stream NZBs directly from Usenet through a virtual filesystem — without downloading full media files first.
</p>

> [!NOTE]
> **NzbDAV is now InfiniDysk.** Docs, GitHub, and the Docker image have moved. If you still pull `ghcr.io/nzbdav/nzbdav`, switch to `ghcr.io/infinidysk/infinidysk` (same `/config` and mounts). Read the [rename FAQ](https://www.infinidysk.com/community/renaming-to-infinidysk/).

<img width="1024" height="601" alt="InfiniDysk overview dashboard" src="docs/assets/overview.png" />

<p align="center">
  <a href="https://github.com/DemianosHB/infinidysk/releases"><img alt="Latest release" src="https://img.shields.io/github/v/release/DemianosHB/infinidysk?filter=v1.*" /></a>
  <a href="https://github.com/DemianosHB/infinidysk/pkgs/container/infinidysk"><img alt="Docker image" src="https://img.shields.io/badge/ghcr.io-demianoshb%2Finfinidysk-blue?logo=docker&logoColor=white" /></a>
  <a href="https://github.com/DemianosHB/infinidysk/actions/workflows/v1-ci.yml?branch=v1"><img alt="CI status" src="https://img.shields.io/github/actions/workflow/status/DemianosHB/infinidysk/v1-ci.yml?branch=v1&label=CI" /></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/github/license/DemianosHB/infinidysk" /></a>
</p>

---

InfiniDysk combines a **WebDAV server** with a **SABnzbd-compatible API**. Sonarr, Radarr, and similar tools can use it as a drop-in download client, while Plex, Emby, Jellyfin, and other WebDAV clients stream content on demand from your Usenet providers.

Please add feature requests and bug reports for this v1 fork to [its issue tracker](https://github.com/DemianosHB/infinidysk/issues) (upstream tracker: [infinidysk/infinidysk/issues](https://github.com/infinidysk/infinidysk/issues)), or join our [Discord](https://discord.gg/DAya7W6QMa) to chat with us.

> After joining, use the channel and role selector to enable **InfiniDysk - SuperFork** for release notifications and development channels.

## Fork features (DemianosHB)

This fork develops the `v1` line further and adds the following on top of upstream `v1` (release scheme `vX.Y.Z-DemianosN`; the earliest fork releases used `v1.7.1-langN`, merged via [#3](https://github.com/DemianosHB/infinidysk/pull/3) and [#4](https://github.com/DemianosHB/infinidysk/pull/4)):

- **Preferred audio languages per profile** — an ordered language list (e.g. `de, en`) configurable in Settings → Profiles. It biases search result ordering and the watchdog play fallback queue (preferred language first, then quality). It is a bias, never a filter: releases whose language cannot be detected confidently are never demoted or hidden.
- **Optional process-wide SOCKS5 outbound proxy** — set `general.socks5-proxy` (`socks5://host:port`; `socks5h://` is accepted and normalized; ENV overlay `NZBDAV_CONFIG__GENERAL__SOCKS5_PROXY`) in Settings → General, with a "Test proxy" button. All non-local outbound traffic — NNTP provider connections and HTTP (indexers, metadata resolvers, rclone RC, Warden remote sources) — is tunneled through the proxy, e.g. a Gluetun container on the same Docker network. DNS is resolved by the proxy and TLS stays end-to-end between client and server. Loopback addresses and single-label Docker service names always connect directly, and per-indexer proxy settings keep their configured behavior. Empty or invalid values disable the proxy; the default is off. Internal services that must bypass the proxy are configurable via `general.socks5-proxy-bypass` (ENV overlay `NZBDAV_CONFIG__GENERAL__SOCKS5_PROXY_BYPASS`), a comma-separated list of host names (`sonarr.lan`, also covering its subdomains), domain suffixes (`.lan` or `*.lan`), IP addresses, CIDR ranges (`192.168.0.0/16`), or `*` for everything; the setting applies to NNTP and HTTP connections alike and takes effect for new connections after saving.
- **Stall negative marking (play fallback)** — Settings → Watchdog → "Stall failover": `play.stall-negative-marking-enabled` (default off) and `play.stall-negative-marking-minutes` (default 15, range 1–1440; ENV overlay `NZBDAV_CONFIG__PLAY__STALL_NEGATIVE_MARKING_*`). Candidates that abort on a serving stall are remembered, so a warm player retry skips them instead of re-stalling on the same winner. Marks are advisory only and fail open when every otherwise-eligible candidate is marked.
- **Read-ahead PAR2 repair** — Settings → Repairs → "PAR2 gap repair": `repair.readahead.enabled` (default off) and `repair.readahead.window-seconds` (default 45, range 5–600; ENV overlay `NZBDAV_CONFIG__REPAIR__READAHEAD__*`). Requires PAR2 gap repair (`repair.par2-enabled`). When on, an active stream widens its prefetch window to measured-bitrate × window ahead of the read head (capped by the article RAM budget), so missing or corrupt articles are discovered and PAR2-repaired *before* playback reaches them; streaming-triggered repairs jump the health-check repair queue. The player never waits for a repair — if the patch is not ready in time, the stock zero-fill / 503 + Retry-After behavior applies and the patch store (`/config/repair-segments`) heals later plays. See [PR #10](https://github.com/DemianosHB/infinidysk/pull/10).
- **Fork images and releases** — pushes to `v1` build the rolling `lang-dev` image, and tags `vX.Y.Z-DemianosN` build versioned images (multi-arch amd64/arm64), all published to `ghcr.io/demianoshb/infinidysk` (`vX.Y.Z-DemianosN` plus `latest`). The upstream release-please pipeline is disabled in this fork; releases are driven by tags. Upstream images remain at `ghcr.io/infinidysk/infinidysk`.

## Features

### Streaming and providers

- **Virtual WebDAV filesystem** — Mount, browse, stream, and seek through NZB content without downloading complete media files first.
- **Archive streaming** — Read RAR and 7z archives on demand, including password-protected content.
- **Fast, resilient NNTP** — Pipeline article requests and cascade across multiple providers with automatic failover and circuit breakers.
- **Provider routing and limits** — Separate provider-wide and article-transfer connection budgets, reserve burstable metadata capacity, set priorities and data caps, then Auto-tune transfers from the UI.
- **Container-aware gap handling** — Preserve MPEG-TS timing across missing article ranges to reduce playback desynchronization.

### Search and playback readiness

- **Built-in Newznab search** — Configure indexers, monitor API usage, search manually, and mount results.
- **Filters and adapters** — Apply regex or remote exclude lists and expose selected indexers through token-scoped Addon, Newznab, and JSON APIs.
- **Watchdog, Preflight, and Warden** — Verify candidates, warm likely results, retry failed releases, and remember known-dead releases.
- **Watchtower** — Keep wanted movies and episodes mapped to verified releases before playback begins.

### Automation and library management

- **SABnzbd-compatible queue** — Use add, queue, history, pause, resume, and speed-limit operations with configurable concurrent workers.
- **Sonarr and Radarr integration** — Import through Rclone symlinks or lightweight STRM files and optionally repair unhealthy content.
- **WebDAV management** — Browse, download, and delete eligible virtual filesystem items from the admin UI.
- **[Experimental AltMount migration](https://www.infinidysk.com/guides/altmount-migration/)** — Rebuild and import an existing AltMount library through a guided wizard that leaves its source metadata untouched.

### Operations and deployment

- **Live operations dashboard** — Track throughput, latency, errors, active reads, provider usage, failover saves, and indexer activity.
- **[Logs and diagnostics](https://www.infinidysk.com/configuration/support/)** — Filter and download logs, enable stream tracing from the UI, and generate redacted technical support packs.
- **[Backup and restore](https://www.infinidysk.com/configuration/backup/)** — Schedule database backups and download, upload, or restore them from the Settings UI.
- **[Headless configuration](https://www.infinidysk.com/configuration/headless/)** — Provision authoritative settings with `NZBDAV_CONFIG__...` environment variables.
- **[OIDC / SSO](https://www.infinidysk.com/configuration/oidc/)** — Authenticate browser sessions through standards-compliant identity providers such as Authentik, Authelia, and Keycloak.
- **[Prometheus metrics](https://www.infinidysk.com/operations/prometheus/)** — Scrape `/metrics` for streaming, seek latency, NNTP pools, circuit breakers, and process/runtime stats. Direct backend scrapes are anonymous unless `METRICS_REQUIRE_API_KEY=true`.
- **[PostgreSQL](https://www.infinidysk.com/operations/postgresql/)** — Optional external PostgreSQL for the main operational database on new installs. SQLite stays the default; there is no SQLite-to-PostgreSQL migration yet. Auxiliary stores (`metrics.sqlite`, `warden.db`) remain in `/config`.
- **[Admin API reference (Scalar)](https://www.infinidysk.com/configuration/environment-variables/)** — Opt-in Scalar explorer at `/scalar/` plus `/openapi/admin.json`, enabled with `ENABLE_API_DOCS=true`. Disabled by default in Docker.
- **Flexible hosting** — Run with Docker, prebuilt Linux archives, or DUMB, and build for reverse-proxy sub-path hosting when needed.

## Quick start

This fork ships a single multi-architecture Docker image to `ghcr.io/demianoshb/infinidysk`. Use `latest` for the newest fork release (versioned tags `vX.Y.Z-DemianosN`), or `lang-dev` to track the `v1` branch.

To try it without keeping any settings:

```bash
docker run --rm -it -p 3000:3000 ghcr.io/demianoshb/infinidysk:latest
```

This trial command is ephemeral: its settings are discarded when the container exits.

For a persistent setup, save the following as `compose.yml`:

```yaml
services:
  nzbdav:
    image: ghcr.io/demianoshb/infinidysk:latest
    container_name: nzbdav
    restart: unless-stopped
    healthcheck:
      test:
        - CMD-SHELL
        - curl -fsSL http://localhost:3000/healthz > /dev/null || exit 1
      interval: 30s
      retries: 3
      start_period: 60s
      timeout: 5s
    ports:
      - "3000:3000"
    environment:
      PUID: "1000"
      PGID: "1000"
      TZ: Etc/UTC
    volumes:
      - ./config:/config
```

Then run `docker compose up -d`, open `http://localhost:3000`, create your admin account, and configure your Usenet provider and WebDAV credentials under **Settings**.

> [!IMPORTANT]
> Port `3000` serves plain HTTP. If InfiniDysk will be reachable outside your trusted network, put it behind an HTTPS reverse proxy and do not expose the container port directly to the internet. WebDAV uses Basic authentication, so TLS is essential for remote access. When the proxy runs on the Docker host, bind the port to localhost with `127.0.0.1:3000:3000`.

Other supported setup paths:

- **Linux without Docker** — Follow the [prebuilt archive guide](https://www.infinidysk.com/getting-started/prebuilt-archives/) to run the x64 or ARM64 release bundle.
- **IPv6-only Docker hosts** — The fork image is published to GHCR only; if `ghcr.io` is not reachable over IPv6, use the upstream Docker Hub mirror `infinidysk/infinidysk`.
- **Batteries-included Arr stack** — Use InfiniDysk as a supported core module in [DUMB](https://dumbarr.com/services/core/nzbdav/).
- **Reverse-proxy sub-path** — Follow the [URL base guide](https://www.infinidysk.com/configuration/url-base/) to build an image for paths such as `/nzbdav`.

## Documentation

Full documentation is published at [www.infinidysk.com](https://www.infinidysk.com/). Start with the [getting started guide](https://www.infinidysk.com/getting-started/) for a production deployment.

- **Install and migrate** — Docker Compose, prebuilt Linux archives, first-run setup, upgrades from nzbdav-dev and community forks, and the [experimental AltMount migration](https://www.infinidysk.com/guides/altmount-migration/).
- **Configure** — Settings walkthroughs, [headless environment configuration](https://www.infinidysk.com/configuration/headless/), OIDC, backup and restore, URL base, and provider tuning.
- **Integrate** — Sonarr/Radarr automation, Rclone symlinks, STRM files, Plex, Emby, Jellyfin, and Stremio through AIOStreams.
- **Search and prepare** — Indexers, token-scoped search profiles, Watchdog, Preflight, Warden, and [Watchtower](https://www.infinidysk.com/features/watchtower/).
- **Operate and troubleshoot** — Health checks and repairs, live logs, stream tracing, support packs, performance tuning, [Prometheus metrics](https://www.infinidysk.com/operations/prometheus/), and [PostgreSQL](https://www.infinidysk.com/operations/postgresql/).
- **Compare** — [InfiniDysk, AltMount, and classic download clients](https://www.infinidysk.com/guides/compare/).

## Why another fork?

This project is a maintained fork of [nzbdav-dev/nzbdav](https://github.com/nzbdav-dev/nzbdav). We took ownership of the full Usenet streaming stack — NzbDAV, UsenetSharp, RapidYencSharp, rapidyenc, and SharpCompress — so playback, connection, archive, and decoding fixes can land in the right layer.

Read the full story on the [about page](https://www.infinidysk.com/community/about/).

### Special thanks

Special thanks to the forks and contributors whose ideas we consolidated:

- [@Nzbdav-dev](https://github.com/Nzbdav-dev)
- [@Pukabyte](https://github.com/Pukabyte)
- [@elfhosted](https://github.com/elfhosted)
- [@kha-kis](https://github.com/kha-kis)
- [@mrghxst](https://github.com/mrghxst)
- [@qooode](https://github.com/qooode)
- [@dgherman](https://github.com/dgherman)
- [@loambit](https://github.com/loambit)

## Development

This repository is v1-only: `v1` is the development line and the target of all pull requests. The project consists of a .NET 10 backend (WebDAV, Usenet streaming, SAB API) and a React Router 8 frontend (admin UI). See [CONTRIBUTING.md](CONTRIBUTING.md) for local development setup and [CHANGELOG.md](CHANGELOG.md) for release history. Source for the published documentation lives in [infinidysk/docs](https://github.com/infinidysk/docs). Local `run-backend.sh` enables the Scalar admin API reference at `/scalar/`; Docker requires `ENABLE_API_DOCS=true`.

## License

InfiniDysk is released under the [MIT License](LICENSE).

> [!NOTE]
> InfiniDysk is intended for use with legally obtained or public domain content only. The project maintainers do not condone piracy and will not provide support for users suspected of engaging in copyright infringement.
