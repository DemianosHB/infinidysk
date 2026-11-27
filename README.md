# infinidysk — Rust fork (DemianosHB)

This branch (`rust`) is a **fork of upstream [infinidysk](https://github.com/infinidysk/infinidysk) 2.x** (the Rust rewrite on upstream `main`) with the **v1-only features ported back on top in Rust**: indexer search and Prowlarr sync, search profiles and addon (Stremio) routes, Watchtower / Watchdog / Preflight / Warden, and more.

> [!IMPORTANT]
> **This fork is vibe-coded.** It is developed with AI coding agents (Mistral Vibe), not hand-written commit by commit. Expect the code, commits and review depth to differ from upstream — use it at your own risk and please report issues you find.

- Code, build and tests: [`rust/`](rust/README.md)
- Port plan and feature status: [`rust/PORTING.md`](rust/PORTING.md)
- Rules for AI agents working here: [`rust/AGENTS.md`](rust/AGENTS.md)
- Docker image of this fork's Rust build: `ghcr.io/demianoshb/infinidysk-rust` (built by the *Rust Docker build and push* workflow; kept separate from the v1 images)

The C# 1.x line of this fork lives on the [`v1`](https://github.com/DemianosHB/infinidysk/tree/v1) branch.

## Credits

Many thanks to the [infinidysk](https://github.com/infinidysk/infinidysk) team and all upstream contributors — this fork builds entirely on their 2.x Rust rewrite and the v1 line it grew out of, and would not exist without their work.
