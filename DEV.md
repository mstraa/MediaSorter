# TvSorter V2 Development Tracker

## Standing Rule

Before starting any development task, read:

1. `docs/PRD.md`
2. `DEV.md`

Keep this file updated as implementation progresses.

## What This Is

A from-scratch rewrite of the original Python/FastAPI TvSorter into:

- **Backend:** Rust (axum + tokio), SQLite via rusqlite, reqwest providers, rust-embed for the UI.
- **Frontend:** React + TypeScript (Vite), JSON API client, embedded into the binary for production.
- **Deploy:** privileged Proxmox LXC, built from source (Rust + Node) under systemd.

Feature target: full parity with V1 plus the documented V1 "Known Gaps" closed.

## Architecture

```
src/
  lib.rs         library crate: build_app/serve/spawn_embedded, CORS, tracing
  main.rs        server binary: thin wrapper over tvsorter::serve (LXC/systemd)
  config.rs      env config
  state.rs       AppState (config, db, providers, jobs), output-root helpers
  db.rs          rusqlite schema + queries (Arc<Mutex<Connection>>)
  models.rs      request/response DTOs
  error.rs       AppError -> JSON { detail }
  parser.rs      filename -> ParsedMedia
  naming.rs      destination path builders + sanitization
  filesystem.rs  safe browsing, recursive expansion, path-traversal guards
  ffprobe.rs     resolution -> quality fallback
  providers.rs   TVMaze / Jikan / IMDb / Wikidata + SQLite cache + throttle
  importer.rs    hardlink/copy/test, conflicts, progress, cancel, rate limit
  jobs.rs        background import job manager (spawn_blocking)
  library.rs     output rescan
  assets.rs      embedded SPA serving + fallback
  routes/mod.rs  all HTTP handlers + router
frontend/        React + TS SPA (pages: Browse, Match, Results, Imports, History, Settings)
src-tauri/       desktop app (Tauri) — embeds the server via tvsorter::spawn_embedded
scripts/         create-proxmox-lxc.sh, update-tvsorter.sh, tvsorter-access.sh, build-app.sh, build-desktop.sh
```

The server lives in the `tvsorter` library crate. Two front-ends reuse it:
- `src/main.rs` — long-lived server for the LXC / systemd deployment (priority).
- `src-tauri` — native desktop app (macOS/Windows/Linux). It calls
  `tvsorter::spawn_embedded()` to run the same axum server on a loopback port,
  then points a webview at it. `src-tauri` is a *separate* crate (not a workspace
  member), so the LXC build never compiles Tauri.

## Status

- [x] Project scaffold (Cargo crate + Vite React app + embedded assets).
- [x] Config + SQLite schema and persistence (settings, input roots, imports, library, cache, status overrides).
- [x] Filesystem browser with path-traversal protection and recursive video expansion.
- [x] Filename parser + Plex/Jellyfin naming + sanitization.
- [x] ffprobe quality fallback (gap closed).
- [x] Metadata providers (TVMaze, Jikan, IMDb suggestions, Wikidata) with SQLite cache, Jikan throttle, 429 retry.
- [x] Import engine: hardlink/copy/test, skip/replace/index/fail, copystat, rate limiting, cancellation + partial cleanup.
- [x] Background import jobs with byte-level progress, item counts, cancellation.
- [x] JSON API surface (settings, browse, match, preview, import jobs, library, history, search, episodes, source-status, folders, health).
- [x] React SPA: Browse, Match (manual search + apply-to-row/all + episode dropdown), Preview, Results (state filter), Library, History, Settings (folder picker), dark theme, delayed progress overlay.
- [x] Provider episode dropdown (gap closed).
- [x] Multi-season anime: episode-number fallback match when season differs (gap mitigated).
- [x] Desktop app (Tauri) via embedded server: src-tauri crate, shared codebase, LXC build untouched.
- [x] Proxmox LXC creation/update/access scripts adapted for Rust+Node build-from-source.
- [x] Backend unit tests (parser, naming, filesystem, formatting, ffprobe mapping, importer).
- [x] End-to-end smoke test verified (settings → browse → match w/ live TVMaze → import → results → library → history → preview conflict → source-status → folders → embedded SPA).

## Confirmed Decisions

- Frontend: React + Vite SPA on a JSON API.
- Deploy: compile in LXC from source (Rust toolchain + Node installed in container; `update` rebuilds).
- Repo: configurable placeholder `github.com/mstraa/TvSorterV2`, `--repo` override in scripts.
- Scope: V1 parity + close Known Gaps (ffprobe fallback, episode dropdown, multi-season anime).
- DB layer: rusqlite single connection behind a mutex (no sqlx, to keep in-LXC compilation simple).

## Known Gaps / Follow-ups

- Multi-season anime mapping is still heuristic (MyAnimeList models each season as a separate
  entry); the episode-number fallback covers single-cour entries but cross-season numbering is approximate.
- Optional API-key providers (TMDB/TheTVDB/AniList) are not implemented.
- No subtitle handling (intentional, per PRD).
- The SQLite connection is a single mutex-guarded handle; fine for single-user LAN use, revisit if concurrency grows.

## Verification

```sh
cargo test
cd frontend && npm run build
```
