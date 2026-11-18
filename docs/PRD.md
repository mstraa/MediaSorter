# TvSorter V2 PRD

## Overview

TvSorter is a LAN-only web application intended to run inside a privileged LXC container.
It helps curate TV shows, anime, and films from mounted input folders into clean,
Plex/Jellyfin-friendly output libraries without modifying the original files.

V2 reimplements the original Python/FastAPI app with a Rust backend and a TypeScript/React
frontend while preserving the product behavior.

The app lets a user browse mounted input folders, select files or folders, identify the
show and episode from public metadata sources, manually correct matches when needed, then
hardlink or copy the selected media into a curated output folder using a consistent naming
structure.

## Goals

- Provide a web UI for manually controlled TV/anime/film imports.
- Keep source files untouched.
- Allow per-import choice between hardlink and copy.
- Support separate manually configured output roots for TV, Anime, and Film.
- Persist imported library state across app restarts.
- Allow users to see already imported files from the output folders.
- Use non-login public metadata APIs.
- Use English metadata.
- Support normal season/episode naming for anime and TV.
- Ignore subtitles for the MVP.

## Non-Goals

- No automatic deletion, moving, or renaming of source files. *(Superseded: a
  `move` action shipped after the MVP. It is opt-in per import — the default is
  still `copy`, which leaves sources untouched — and it is the only action that
  removes a source file.)*
- No user login or authentication in the MVP.
- No subtitle import in the MVP.
- No automatic daemon-style watch/import workflow in the MVP.
- No dependency on paid or logged-in metadata APIs in the MVP.

## Runtime Environment

- Runs inside a privileged LXC container.
- Input and output folders are mounted into the LXC by the host.
- The web app is available only on the LAN.
- The app process should run as a non-root user where possible.
- Existing host media permissions should not be changed by TvSorter; the LXC service identity
  should be matched to the existing writable UID/GID or group when mounts are shared.
- Hardlinks only work when source and destination are on the same filesystem/device. If
  hardlinking fails, the UI must explain the failure and let the user copy instead.

## Stack

- **Backend:** Rust, axum (HTTP) + tokio (async runtime).
- **Database:** SQLite via rusqlite (single connection behind a mutex; short synchronous critical sections).
- **Metadata HTTP:** reqwest (rustls TLS).
- **Frontend:** React + TypeScript, built with Vite, embedded into the binary via rust-embed.
- **Media inspection:** ffprobe (ffmpeg) for the resolution-based quality fallback.
- **Service management:** systemd inside the LXC. Built from source (Rust + Node) on install/update.

## Metadata Providers

- **TV:** TVMaze public API (no key). Show search and episode list.
- **Anime:** Jikan public API (no key, MyAnimeList data). Requests throttled to avoid 429s.
- **Film:** IMDb-style public suggestion endpoint first, Wikidata as a fallback (descriptive user agent).

Provider calls are cached in SQLite and de-duplicated during batch matching. A repeated show
reuses one search and one episode-list lookup. If a provider rate-limits or fails, the
filename-parsed fallback remains available for manual correction.

Future optional providers (require keys): TMDB, TheTVDB, AniList.

## User Configuration

The Settings UI configures: one or more input roots, one TV output root, one Anime output
root, one Film output root, and a copy speed limit (Mo/s, default 15). It shows read/write
permission checks for configured roots and provides a server-side folder picker.

## Import Workflow

1. Open the web UI.
2. Select an input root and browse folders.
3. Select one or more files or folders (folders expand recursively into video files).
4. Choose media type: TV, Anime, or Film.
5. The app parses each filename for show title, year, season, episode, and quality, and runs
   the matching provider lookup (TVMaze / Jikan / IMDb+Wikidata). When the filename has no
   quality tag, ffprobe fills it from the video resolution.
6. The app shows proposed matches; the user may override any field, re-search providers and
   apply a result to one or all rows, or load the provider episode list and pick an episode.
7. The app previews destination paths.
8. The user chooses hardlink, copy, or test, and a conflict policy.
9. The app performs the import as a background job with progress and cancellation, then records
   it in SQLite and shows it in Library/History.

If a running import is cancelled, TvSorter stops the current copy, removes any partial
destination file, aborts remaining queued items, and keeps already completed imports recorded.

## Naming Format

TV and Anime (separate output roots, same structure):

```text
Show Name (Year)/Season XX/Show Name (Year) - SXXEYY - Episode Name - Quality.ext
```

Film files are written directly under the Film output root:

```text
Film Name (Year) - Quality.ext
```

## Quality Detection

1. Filename tags such as `720p`, `1080p`, or `2160p`.
2. ffprobe resolution fallback (maps stream height to the nearest tier).
3. `Unknown` when quality cannot be determined.

## Import Actions & Conflict Handling

Actions: `hardlink`, `copy`, `test` (preview-only). Source files are never modified.

Conflict modes when the destination exists: `skip` (default), `replace`, `index` (keep both with
a `(2)` suffix), `fail`.

## Persistence

SQLite stores settings, input roots, import history, library state, provider cache, and manual
source status overrides. Each import records source path/size/mtime/device/inode, output path,
media type, provider and show ID, show title/year, season/episode/episode title, quality, action,
conflict policy, result, and timestamp. An output rescan reconciles files added/removed outside
the app.

## Web UI Pages

- **Duplicates:** scan status and controls (walk and hashing progress, speed, time left),
  duplicate groups sorted by recoverable space with a per-folder-pair summary and filters by
  pair, folder and size, selection with a per-group kept file and exclusions, Simulate and
  Replace with hardlinks (confirmation dialog), live job progress, Unused torrents, History of
  every simulated or performed replacement.

- **Settings:** input/output roots, copy speed limit, folder picker, permission checks.
- **Browse:** input-root navigation, human-readable sizes (e.g. `1,34 Go`), multi-select,
  recursive folder expansion, sticky controls, per-source status badges and filter, manual
  status overrides, clickable rows, dark theme toggle.
- **Match Queue:** parsed data, provider candidates, manual correction, re-search with
  apply-to-row/all, episode dropdown, quality correction, wrapped long paths.
- **Import Preview:** source, destination, action, conflict status.
- **Import Results:** per-item state with color coding and a state filter; permission failures
  show actionable output-mount guidance.
- **Long Operations:** indeterminate indicator after 2s; determinate background import progress
  with current filename, item counts, byte-level copy progress, and percentage; copy throttling
  per the configured speed limit.
- **Library:** imported files grouped by type, present/missing indicator, output rescan.
- **History/Logs:** completed, skipped, and failed imports with error messages.

## API Surface

`GET /health`, `GET/PUT /api/settings`, `GET /api/browse`, `POST /api/match`, `POST /api/preview`,
`POST /api/import-jobs`, `GET /api/import-jobs/:id`, `POST /api/import-jobs/:id/cancel`,
`GET /api/import-jobs/:id/results`, `GET /api/library`, `POST /api/library/rescan`,
`GET /api/history`, `GET /api/search`, `GET /api/episodes`, `POST /api/source-status`,
`GET /api/folders`. All non-API routes serve the embedded SPA (client-side routing fallback).

## Duplicates (hardlink dedupe)

Target: a ZFS raidz of 5400 rpm HDDs (~110 random-read IOPS each) holding several TB and
hundreds of thousands of files, where Sonarr already hardlinks `Torrents/` into the library but
many full copies remain (Torrents ↔ media folders, `Films` ↔ `Movies`, `Tv` ↔ `Tv Show`).
Priority is data safety, then not saturating the pool; an overnight scan is acceptable.

Goals:

1. Scan configurable dedupe roots (separate from input roots) robustly, resumably and throttled.
2. Detect files with identical content on different inodes of the same filesystem.
3. After explicit validation, replace duplicates with hardlinks to one inode to free space.
4. List files under the torrent folders with `nlink == 1` (no longer used by the library).

Delivery is phased; each phase is reviewed before the next:

- **Phase 1 (done):** settings, `dedupe.db`, metadata scan, Unused torrents. Read-only.
- **Phase 2 (done):** partial then full (BLAKE3) hashing, duplicate groups, read-only Duplicates list.
- **Phase 3 (done):** simulation, hardlink replacement with re-verification, journal, crash recovery.

### Settings (Settings › Duplicates)

Dedupe roots (one per line; all must share one `st_dev`, otherwise the UI says so), torrent
folders (inside the roots; guessed from roots named `Torrents` until saved), excluded patterns
(default `Torrent-work/`, `*.!qB`, `*.part`, `*.tmp`, `.Trash-*`, `.ms-dedupe-*`), minimum size
(default 50 Mio), minimum age since modification (default 60 min), walk limit in stat calls/s
(default 200), hash read limit in Mo/s (default 40, `0` = unlimited), optional automatic scan
window (`01:00-07:00`), preferred owner of the kept inode (default `100000:100000`).

### Metadata scan (phase 1)

- Single thread, no symlink following, never crosses into another `st_dev`, honours exclusions.
  I/O class IDLE via `ioprio_set` (best effort; on ZFS the throttle is the real guard).
- Records path, dev, ino, size, mtime, ctime, nlink, uid, gid, mode per regular file into
  `dedupe.db` (a separate SQLite file next to `tvsorter.db`, WAL, its own connection, so the
  scan never blocks the UI or imports).
- Resumable: the directory frontier is persisted; a directory leaves the queue in the same
  transaction (≤ ~1,000 rows or 2 s) that records its files. Stop, crash or restart resume
  from the last commit. Changing the roots abandons the unfinished run.
- Incremental: rows are upserted by path; a completed run marks unseen rows `gone_at`.
- Runs as a background job with pause, resume and stop (stop keeps progress), progress
  (folders, entries, files, speed, ETA from the previous full scan's folder count).
- Automatic scans start or resume inside the configured window (at most one new full scan per
  20 h) and stop, resumably, when it closes. Manual scans ignore the window.

### Duplicate detection (phase 2)

- Runs as the second stage of the same scan job (walk → quick hash → full hash → groups), with
  the same pause/resume/stop; a run is `completed` only after grouping. Stopping during hashing
  resumes straight into hashing, without walking again.
- Candidates: inodes sharing `(dev, size)` with another inode; paths already hardlinked together
  are one inode, so one candidate.
- Quick hash: BLAKE3 over the size plus 1 MiB at the head, middle and tail (files ≤ 3 MiB are
  read whole). Full hash (BLAKE3, 4 MiB sequential reads) only for inodes whose quick hash still
  collides. One file at a time, throttled to the configured Mo/s, `fstat` before and after each
  file to discard anything that changed since the walk.
- Hashes are stored per `(dev, ino)` with the size and mtime they were taken at and reused while
  those match, so an unchanged file is never read again. Unreadable files are recorded and not
  retried until they change.
- Groups: same dev, size and full hash, 2+ inodes; recoverable space = size × (inodes − 1);
  labelled by folder pair (`Films ↔ Movies`). The kept inode is shown: most hard links, then the
  preferred owner, then the oldest mtime. Groups whose owners differ carry a warning.

### API

`GET/PUT /api/dedupe/settings`, `POST /api/dedupe/scan` (`{ "fresh": bool }`),
`POST /api/dedupe/scan/pause|resume|cancel|clear`, `GET /api/dedupe/scan/status`,
`GET /api/dedupe/unused-torrents?limit&offset`,
`GET /api/dedupe/groups?limit&offset&pair&folder&min_size` (folder must be inside the dedupe
roots), `POST /api/dedupe/jobs` (`{ "mode": "simulate"|"apply", "groups": [{ "group_id",
"keeper_ino", "exclude_inos" }] }`), `GET /api/dedupe/jobs` (latest), `GET /api/dedupe/jobs/:id`,
`POST /api/dedupe/jobs/:id/cancel`, `GET /api/dedupe/history?limit&offset&mode`,
`GET /api/dedupe/space` (ZFS `usedbysnapshots` per dataset when the `zfs` command exists).

### Replacement (phase 3)

- The user selects groups, may pick another kept file per group and untick duplicates, then
  runs a **simulation** (no content read, nothing written) or a **replacement** behind a
  confirmation that states it is irreversible and requires an "I understand" tick.
- One job at a time; it never runs alongside a scan (each refuses while the other runs). Stop
  takes effect between two files, never in the middle of a replacement.
- Simulation re-checks each path (present, unchanged since the scan, folder writable) and notes
  owner/permission changes and inodes with links outside the selection (no space freed).
- Every simulated, replaced, skipped or failed path is journaled in `dedupe_ops` (before/after
  inode, size, owner, mode, kept path, hash, reason, freed bytes). The journal survives
  `scan/clear`. A replacement cannot be undone: the History tab says so.
- Crash safety: a `pending_links` row is committed before each `link`, removed after the
  `rename`. At startup, listed `.ms-dedupe-*.tmp` files that are regular files with
  `nlink ≥ 2` are removed; nothing else is touched.
- No automatic `chown`: linked paths share the kept inode's owner, mode, mtime and xattrs.

### Safety (all phases)

Nothing on disk changes without an explicit user action; simulation is the default. Before
each replacement: re-`stat` (same dev/ino/size/mtime as scanned), then a byte-for-byte compare
with the kept file; `link` to a `.ms-dedupe-<uuid>.tmp` beside the duplicate, `rename` over it
(the path is never missing), verify the inode, `fsync` the parent. `EXDEV`/`EPERM`/`EACCES` or
any change means skip and journal the reason, never a copy fallback. Leftover `.ms-dedupe-*.tmp`
links are removed at startup. Freed space only returns once ZFS snapshots holding the old inodes
expire, and files held open (seeding) release their old inode on close.

## Safety Requirements

- All file browsing is constrained to configured input roots; path traversal is prevented.
- Output writes are constrained to configured TV/Anime/Film output roots.
- Source files are never mutated.
- Existing output files are never overwritten without an explicit user conflict choice.
- Hardlink failures never silently fall back to copy.

## References

Borrows concepts from FileBot (action selection, conflict modes, query override, preview/test)
and mnamer (parsing, provider abstraction, configurable formats, no-overwrite behavior, cache/test).
