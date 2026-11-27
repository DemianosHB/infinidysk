# infinidysk — Rust fork (DemianosHB)

This branch (`rust`) is a **fork of upstream [infinidysk](https://github.com/infinidysk/infinidysk) 2.x** (the Rust rewrite on upstream `main`) with the **v1-only features ported back on top in Rust**: indexer search and Prowlarr sync, search profiles and addon (Stremio) routes, Watchtower / Watchdog / Preflight / Warden, and more.

> [!IMPORTANT]
> **This fork is vibe-coded.** It is developed with AI coding agents (Mistral Vibe), not hand-written commit by commit. Expect the code, commits and review depth to differ from upstream — use it at your own risk and please report issues you find.

- Code, build and tests: [`rust/`](rust/README.md)
- Port plan and feature status: [`rust/PORTING.md`](rust/PORTING.md)
- Rules for AI agents working here: [`rust/AGENTS.md`](rust/AGENTS.md)
- Docker image of this fork's Rust build: `ghcr.io/demianoshb/infinidysk-rust` (built by the *Rust Docker build and push* workflow; kept separate from the v1 images)

The C# 1.x line of this fork lives on the [`v1`](https://github.com/DemianosHB/infinidysk/tree/v1) branch.

## Legal and responsible use

This software is a general-purpose, self-hosted tool: it streams whatever NZB content the Usenet providers and indexers you configure expose, using your own accounts. This repository hosts no content itself and does not provide, link to, or endorse any copyrighted material.

You are solely responsible for the content you access with it — only use it with content you have the legal right to download and stream in your jurisdiction. The maintainers do not condone piracy and will not support users suspected of engaging in copyright infringement.

If you believe material in this repository infringes your rights, please report it via [GitHub's DMCA takedown process](https://docs.github.com/en/site-policy/dmca-takedowns) or by opening an issue, and it will be acted on promptly.

## Credits

Many thanks to the [infinidysk](https://github.com/infinidysk/infinidysk) team and all upstream contributors — this fork builds entirely on their 2.x Rust rewrite and the v1 line it grew out of, and would not exist without their work.
