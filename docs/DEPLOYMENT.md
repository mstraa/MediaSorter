# LXC Deployment

TvSorter V2 runs inside a privileged LXC container. Unlike the Python version, the container
**builds TvSorter from source**: it installs the Rust toolchain (rustup) and Node.js, compiles
the release binary, and runs it under systemd. The frontend bundle is embedded into the binary,
so there is nothing else to serve at runtime.

## Automated Proxmox Creation

From the Proxmox VE host, run:

```sh
bash -c "$(curl -fsSL https://raw.githubusercontent.com/mstraa/TvSorterV2/main/scripts/create-proxmox-lxc.sh)" -- \
  --ctid 120 \
  --mount /tank/downloads:/mnt/downloads \
  --mount /tank/media/TV:/mnt/media/TV \
  --mount /tank/media/Anime:/mnt/media/Anime \
  --mount /tank/media/Films:/mnt/media/Films
```

> The default repo `github.com/mstraa/TvSorterV2` is a placeholder — pass `--repo <your remote>`
> (and `--branch`) to install from your fork.

The script:

- Creates a privileged Debian LXC (defaults: 4 cores / 4096 MiB / 14 GiB disk to accommodate the Rust build).
- Downloads a Debian standard template through `pveam` when needed.
- Adds any requested bind mounts.
- Installs build tooling: `build-essential`, `pkg-config`, `ffmpeg`, Node.js 20, and rustup.
- Clones the repo, builds the frontend (`npm ci && npm run build`) and the release binary (`cargo build --release`).
- Configures the Proxmox LXC console to autologin as root.
- Creates and starts the `tvsorter.service` systemd unit.

Use `scripts/create-proxmox-lxc.sh --help` for all options. When run interactively, the script
prompts for root disk and template storage; `--storage auto` picks the first container-capable
storage. Run `pvesm status` on the host to inspect choices.

### Build resource notes

Compiling the dependency tree (axum, reqwest/rustls, rusqlite with bundled SQLite, etc.) is
RAM- and disk-hungry. If the build is OOM-killed, raise `--memory` (6144+) or add swap. The
target directory plus cargo registry plus `node_modules` typically need several GiB, hence the
14 GiB default disk.

## Updating In The LXC

The install places `/usr/local/bin/update`, which pulls the latest GitHub `main`, rebuilds the
frontend and the Rust binary, reapplies the console autologin overrides, and restarts
`tvsorter.service`:

```sh
update
```

Because the binary is recompiled, an update takes several minutes.

## Manual Package Install

```sh
apt update
apt install -y build-essential pkg-config ffmpeg git curl
curl -fsSL https://deb.nodesource.com/setup_20.x | bash - && apt install -y nodejs
curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
useradd --system --home /var/lib/tvsorter --create-home --shell /usr/sbin/nologin tvsorter
```

Clone to `/opt/tvsorter`, then build:

```sh
cd /opt/tvsorter
(cd frontend && npm ci && npm run build)
~/.cargo/bin/cargo build --release
install -d -o tvsorter -g tvsorter /var/lib/tvsorter
```

## Mounts

Mount input and output folders into the LXC before starting the service. Example container paths:

```text
/mnt/downloads
/mnt/media/TV
/mnt/media/Anime
/mnt/media/Films
```

The service user needs read access to input roots and write access to TV/Anime/Film output roots.

If the mounted folders are managed by another LXC, do not change their ownership or mode. Match
TvSorter to the existing numeric identity that can already write:

```sh
stat -c '%u:%g %A %n' /mnt/data/Movies
tvsorter-access --path /mnt/data/Movies --mode group   # group-writable mount
tvsorter-access --path /mnt/data/Movies --mode owner   # only owner can write
```

`tvsorter-access` changes only local LXC users/groups, the systemd service override, and
`/var/lib/tvsorter` ownership. It does not `chown`/`chmod` the mounted media folder.

Hardlinks require source and output on the same filesystem. If they are on different devices,
use copy.

### One mount for the Duplicates module

Hardlinks fail with `EXDEV` between two LXC mount points **even when both come from the same
ZFS dataset**: each `mpN` bind mount is its own mount, and the kernel refuses links across
mounts. For the Duplicates module (and for hardlink imports), mount the whole data dataset
once and point the dedupe roots at sub-folders of that single mount:

```text
# /etc/pve/lxc/<id>.conf
mp0: /ColdGooStore/data,mp=/mnt/data
```

Dedupe roots would then be `/mnt/data/Torrents`, `/mnt/data/Tv Show`, `/mnt/data/Films`, …
Settings › Duplicates shows each root's `st_dev` and warns when they differ.

The LXC is privileged, so files the app creates belong to root on the host. The Duplicates
module creates no data (a hardlink shares the existing inode, owner included), but its scan
database `dedupe.db` sits next to `tvsorter.db` in the data directory.

## systemd Unit

`/etc/systemd/system/tvsorter.service`:

```ini
[Unit]
Description=TvSorter
After=network-online.target
Wants=network-online.target

[Service]
User=tvsorter
Group=tvsorter
WorkingDirectory=/opt/tvsorter
Environment=TVSORTER_DATA_DIR=/var/lib/tvsorter
Environment=TVSORTER_HOST=0.0.0.0
Environment=TVSORTER_PORT=8080
ExecStart=/opt/tvsorter/target/release/tvsorter
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

```sh
systemctl daemon-reload
systemctl enable --now tvsorter
```
