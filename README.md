# MediaSorter

MediaSorter (formerly *TvSorter*) is a LAN-only web app for curating mounted media
files into clean output libraries by hardlinking, copying, or moving selected
TV, anime, films, and music. It pairs a **Rust backend** with a
**TypeScript/React frontend**.

- **Backend:** Rust (axum + tokio), SQLite via rusqlite, reqwest for metadata providers.
- **Frontend:** React + TypeScript built with Vite, embedded into the Rust binary.
- **Deployment:** privileged Proxmox LXC, built from source (Rust + Node) and run under systemd, or as an optional native desktop app (Tauri).

> **Naming note:** the binary, environment variables, and internal identifiers
> still use the legacy `tvsorter` name.

## Building locally

### Prerequisites

- A recent **stable Rust** toolchain (via [rustup](https://rustup.rs))
- **Node.js 18+**
- **ffmpeg/ffprobe** on `PATH` (optional but recommended — used for the video
  quality fallback and for reading music tags)

### Production build

The frontend bundle is embedded into the Rust binary, so build the frontend first:

```sh
# 1. Build the frontend (gets embedded into the binary)
cd frontend && npm ci && npm run build && cd ..

# 2. Build the backend
cargo build --release

# 3. Run it
./target/release/tvsorter
```

Then open `http://127.0.0.1:8080`, configure input/output folders in Settings,
and start browsing and importing.

### Development mode

Run the backend (serves the JSON API on port 8080):

```sh
TVSORTER_DATA_DIR=.local-data cargo run
```

In a second terminal, run the Vite dev server (proxies `/api` and `/health` to the backend):

```sh
cd frontend
npm install
npm run dev
```

Open the Vite URL (default `http://127.0.0.1:5173`).

### Tests

```sh
cargo test                     # backend unit tests
cd frontend && npm run build   # type-check + bundle the frontend
```

## Features

- Browse one or more configured input roots from the web UI, with a refresh
  button to re-read the current folder.
- Select individual files or whole folders; folders are expanded recursively and
  only supported video/audio files are imported.
- Sort **TV, Anime, Film, and Music** into separate output roots.
- Parse titles, years, season/episode numbers (including anime version suffixes
  like `05v2`), episode titles, and quality tags from filenames.
- **ffprobe resolution fallback** fills the quality when no quality tag is present in the filename.
- **Music sorting:** read artist/album/year tags with ffprobe, fall back to the
  `Artist/Album` folder convention, then to filename parsing; files keep their
  original names under `Artist/Album (Year)/`.
- Look up TV metadata with TVMaze, anime metadata with Jikan, and film metadata with IMDb-style public suggestions plus Wikidata fallback.
- Cache provider responses in SQLite and de-duplicate metadata lookups during batch matching.
- Manually correct every match field before import (title, year, season, episode, episode title, quality, provider, provider ID).
- **Match exclusion:** include/exclude individual items or whole folders/albums
  from the match queue before importing.
- Search metadata again from the match queue and apply a selected result to one row or every row in the current batch.
- **Provider episode dropdown:** load a show's episode list and pick the exact episode to fill season/number/title.
- Multi-season anime matching falls back to matching by episode number when the season does not line up with the provider entry.
- Preview destination paths before importing.
- Import by **copy** (default), **hardlink**, **move**, or **test**/preview-only mode.
- Handle destination conflicts with skip, replace, keep-both indexing, or fail.
- Run imports as background jobs with current item, total progress, current-file
  copy progress, whole-job cancellation, and per-item cancellation.
- Remove partial destination files when a copy is cancelled.
- Limit copy speed from Settings; the default is 15 Mo/s, and `0` disables the limit.
- Keep source files untouched (except in move mode).
- **Origin xattr:** copied/moved files get a `user.tvsorter.origin` extended
  attribute recording the source path relative to the input root (Unix only,
  best-effort).
- Persist settings, input roots, import history, library state, provider cache, and manual source status overrides in SQLite.
- Show latest source status in Browse, filter Browse by status, and manually mark selected files/folders as auto, no status, imported, failed, skipped, preview, or conflict.
- Import History page with an output-folder rescan that discovers existing media (video and audio) and marks missing files.
- Clear finished import jobs from the Imports page.
- Pick input and output folders from a server-side folder browser in Settings.
- Show read/write permission checks for configured roots.
- Toggle light/dark theme in the browser.
- Expose `GET /health` for service health checks.
- Provide Proxmox LXC creation, update, and media-mount access helper scripts.

## Import Workflow

1. Open Settings and configure input roots plus TV, Anime, Film, and Music output roots.
2. Use Browse to choose an input root, navigate folders, select files or folders, choose the media type, and click **Match Selected**.
3. Review the Match Queue, adjust metadata if needed, exclude unwanted items or folders, optionally search providers again or load the episode list, and preview destination paths.
4. Choose copy, hardlink, move, or test and select a conflict policy.
5. Start the import. The progress dialog shows batch and copy progress and can cancel the whole job or single items.
6. Review Import Results, then use History to inspect persisted output state.

## Naming

TV and Anime:

```text
Show Name (Year)/Season XX/Show Name (Year) - SXXEYY - Episode Name - Quality.ext
```

Film:

```text
Film Name (Year) - Quality.ext
```

Music (original filenames are preserved; the year is omitted when unknown):

```text
Artist/Album (Year)/Original Filename.ext
```

Supported video extensions are `.avi`, `.m2ts`, `.m4v`, `.mkv`, `.mov`, `.mp4`, `.mpeg`, `.mpg`, `.ts`, `.webm`, and `.wmv`.

Supported audio extensions are `.mp3`, `.flac`, `.m4a`, `.aac`, `.ogg`, `.opus`, `.wav`, `.wma`, and `.alac`.

## Desktop app (Tauri)

The primary deployment target is the Proxmox LXC server below. As an **optional,
standalone deliverable**, MediaSorter can also be packaged as a native desktop
app for macOS, Windows, and Linux.

It uses an **embedded-server** design: the desktop app starts the *same* axum
server on a loopback port (`127.0.0.1:<random>`) and shows it in a native
webview. No HTTP handler is rewritten — the server and the desktop app share one
codebase (the `tvsorter` library crate). The desktop crate (`src-tauri/`) is a
separate Cargo crate, so the LXC build (`cargo build --release` at the repo root)
never compiles Tauri or pulls in its system webview dependencies.

Requires the Tauri CLI (`cargo install tauri-cli --version '^2'`) plus the
platform build prerequisites (see <https://tauri.app/start/prerequisites/>):
WebKitGTK on Linux, Xcode Command Line Tools on macOS, MSVC + WebView2 on Windows.

```sh
scripts/build-desktop.sh        # bundle for the current OS
# or directly:
cargo tauri build               # artifacts in src-tauri/target/release/bundle/
cargo tauri dev                 # run the desktop app for development
```

Cross-OS bundles are produced by building on each target OS (macOS → `.app`/`.dmg`,
Windows → `.msi`/`.exe`, Linux → `.deb`/`.AppImage`). Release bundles are also
built by CI on `v*` tags (`.github/workflows/release-desktop.yml`).

> **Note:** the app browses the *local* machine's filesystem. The folder-picker
> roots and hardlink semantics are tuned for a Linux media server; on a desktop
> Mac/Windows the relevant media usually lives on a NAS, so the desktop app is
> most useful pointed at locally-mounted media.

## Configuration

Environment variables:

- `TVSORTER_DATA_DIR`: directory for SQLite data, default `~/.local/share/tvsorter`
- `TVSORTER_DATABASE`: explicit SQLite database path
- `TVSORTER_HOST`: service host, default `0.0.0.0`
- `TVSORTER_PORT`: service port, default `8080`

Runtime settings saved through the UI:

- Input roots, one path per line.
- TV output root.
- Anime output root.
- Film output root.
- Music output root.
- Copy speed limit in Mo/s.

See [docs/PRD.md](docs/PRD.md) and [DEV.md](DEV.md) before development work.

## Proxmox LXC

Run this from the Proxmox VE host to create a privileged Debian LXC and build MediaSorter from source.
Because the container compiles Rust, the defaults are larger than the Python version
(4 cores / 4096 MiB / 14 GiB disk).

```sh
bash -c "$(curl -fsSL https://raw.githubusercontent.com/mstraa/MediaSorter/main/scripts/create-proxmox-lxc.sh)" -- \
  --ctid 120 \
  --mount /tank/downloads:/mnt/downloads \
  --mount /tank/media/TV:/mnt/media/TV \
  --mount /tank/media/Anime:/mnt/media/Anime \
  --mount /tank/media/Films:/mnt/media/Films \
  --mount /tank/media/Music:/mnt/media/Music
```

> To install from a fork, pass `--repo https://github.com/you/MediaSorter.git`.

The script prompts for root disk and template storage when run interactively. Use `--help` to see static IP, SSH key, storage, and sizing options.

The LXC console is configured to autologin as root, matching common Proxmox helper-script containers.

Inside the LXC, update MediaSorter to the latest GitHub `main` (this re-runs the frontend and Rust build) with:

```sh
update
```

If output mounts are shared with another LXC, keep the existing media permissions and match MediaSorter to the mount identity instead:

```sh
stat -c '%u:%g %A %n' /mnt/data/Movies
tvsorter-access --path /mnt/data/Movies --mode group
```

Use `--mode owner` instead if the mount is writable only by its owner.
