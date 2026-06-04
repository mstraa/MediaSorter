#!/usr/bin/env bash
set -Eeuo pipefail

APP_NAME="TvSorter"
DEFAULT_REPO="https://github.com/mstraa/MediaSorter.git"
DEFAULT_BRANCH="main"

CTID=""
HOSTNAME="tvsorter"
STORAGE="${TVSORTER_STORAGE:-prompt}"
TEMPLATE_STORAGE="${TVSORTER_TEMPLATE_STORAGE:-prompt}"
TEMPLATE=""
BRIDGE="vmbr0"
IP_CONFIG="dhcp"
GATEWAY=""
CORES="4"
MEMORY="4096"
SWAP="1024"
DISK="14"
START="1"
REPO_URL="${TVSORTER_REPO_URL:-$DEFAULT_REPO}"
REPO_BRANCH="${TVSORTER_REPO_BRANCH:-$DEFAULT_BRANCH}"
SSH_PUBLIC_KEY=""
ROOT_PASSWORD=""
DRY_RUN="0"
declare -a MOUNTS=()

usage() {
  cat <<'USAGE'
Create a privileged Proxmox LXC and install TvSorter (Rust backend + React UI).

Run this script from the Proxmox VE host as root.

The container builds TvSorter from source: it installs the Rust toolchain
(rustup) and Node.js, then compiles the release binary. Compilation is
RAM/CPU/disk intensive, so the defaults are larger than a Python install.

Required:
  --ctid ID                    LXC container ID, for example 120

Common options:
  --hostname NAME              Container hostname (default: tvsorter)
  --storage NAME|auto|prompt   Root disk storage (default: prompt when interactive, auto otherwise)
  --template-storage NAME|prompt
                               Template storage (default: prompt when interactive, local otherwise)
  --bridge NAME                Network bridge (default: vmbr0)
  --ip dhcp|CIDR               IP config, for example dhcp or 192.168.1.50/24 (default: dhcp)
  --gateway IP                 Gateway for static IP configs
  --cores N                    CPU cores (default: 4)
  --memory MiB                 RAM in MiB (default: 4096)
  --swap MiB                   Swap in MiB (default: 1024)
  --disk GiB                   Root disk size in GiB (default: 14)
  --repo URL                   Git repo to install (default: https://github.com/mstraa/MediaSorter.git)
  --branch NAME                Git branch to install (default: main)
  --ssh-public-key PATH        SSH public key for root login
  --root-password PASSWORD     Root password for the container
  --mount HOST:CT              Bind mount, repeatable. Example: /tank/media:/mnt/media
  --no-start                   Create and install, then stop the container
  --dry-run                    Print planned settings without changing Proxmox
  -h, --help                   Show this help

Examples:
  scripts/create-proxmox-lxc.sh \
    --ctid 120 \
    --mount /tank/downloads:/mnt/downloads \
    --mount /tank/media/TV:/mnt/media/TV \
    --mount /tank/media/Anime:/mnt/media/Anime \
    --mount /tank/media/Films:/mnt/media/Films
USAGE
}

log() { printf '[%s] %s\n' "$APP_NAME" "$*"; }
die() { printf '[%s] ERROR: %s\n' "$APP_NAME" "$*" >&2; exit 1; }
require_cmd() { command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"; }

list_storage_ids() { pvesm status 2>/dev/null | awk 'NR > 1 {print $1}'; }
list_container_storage_ids() {
  pvesm status --content rootdir 2>/dev/null | awk 'NR > 1 {print $1}'
  pvesm status --content images 2>/dev/null | awk 'NR > 1 {print $1}'
}
list_template_storage_ids() { pvesm status --content vztmpl 2>/dev/null | awk 'NR > 1 {print $1}'; }
storage_exists() { pvesm status --storage "$1" >/dev/null 2>&1; }
storage_supports_container_rootfs() {
  pvesm status --storage "$1" --content rootdir >/dev/null 2>&1 \
    || pvesm status --storage "$1" --content images >/dev/null 2>&1
}

choose_root_storage() {
  local selected=""
  selected="$(list_container_storage_ids | awk 'NF && !seen[$1]++ {print $1; exit}')"
  [[ -n "$selected" ]] || selected="$(list_storage_ids | awk 'NF {print $1; exit}')"
  [[ -n "$selected" ]] || die "No Proxmox storage found"
  printf '%s\n' "$selected"
}

choose_template_storage() {
  local selected=""
  selected="$(list_template_storage_ids | awk 'NF && !seen[$1]++ {print $1; exit}')"
  [[ -n "$selected" ]] || selected="$(list_storage_ids | awk 'NF {print $1; exit}')"
  [[ -n "$selected" ]] || die "No Proxmox storage found"
  printf '%s\n' "$selected"
}

prompt_storage_choice() {
  local title="$1"; shift
  local -a options=("$@")
  local choice index
  [[ "${#options[@]}" -gt 0 ]] || die "No storage choices available for: $title"
  printf '\n%s\n' "$title" >&2
  for index in "${!options[@]}"; do
    printf '  %s) %s\n' "$((index + 1))" "${options[$index]}" >&2
  done
  while true; do
    read -r -p "Select storage [1]: " choice
    choice="${choice:-1}"
    if [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= ${#options[@]} )); then
      printf '%s\n' "${options[$((choice - 1))]}"
      return 0
    fi
    printf 'Invalid choice: %s\n' "$choice" >&2
  done
}

resolve_root_storage() {
  local storage="$1"; local -a choices=()
  if [[ "$storage" == "prompt" ]]; then
    if [[ -t 0 ]]; then
      while IFS= read -r item; do [[ -n "$item" ]] && choices+=("$item"); done < <(list_container_storage_ids | awk 'NF && !seen[$1]++')
      [[ "${#choices[@]}" -eq 0 ]] && while IFS= read -r item; do [[ -n "$item" ]] && choices+=("$item"); done < <(list_storage_ids | awk 'NF && !seen[$1]++')
      prompt_storage_choice "Select root disk storage" "${choices[@]}"
    else
      choose_root_storage
    fi
  elif [[ "$storage" == "auto" ]]; then
    choose_root_storage
  else
    printf '%s\n' "$storage"
  fi
}

resolve_template_storage() {
  local storage="$1"; local -a choices=()
  if [[ "$storage" == "prompt" ]]; then
    if [[ -t 0 ]]; then
      while IFS= read -r item; do [[ -n "$item" ]] && choices+=("$item"); done < <(list_template_storage_ids | awk 'NF && !seen[$1]++')
      [[ "${#choices[@]}" -eq 0 ]] && while IFS= read -r item; do [[ -n "$item" ]] && choices+=("$item"); done < <(list_storage_ids | awk 'NF && !seen[$1]++')
      prompt_storage_choice "Select template storage" "${choices[@]}"
    else
      printf 'local\n'
    fi
  else
    printf '%s\n' "$storage"
  fi
}

available_storage_message() {
  local all container
  all="$(list_storage_ids | paste -sd ', ' -)"
  container="$(list_container_storage_ids | awk 'NF && !seen[$1]++' | paste -sd ', ' -)"
  printf 'Available storage: %s. Container-capable candidates: %s' "${all:-none}" "${container:-unknown}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ctid) CTID="${2:-}"; shift 2;;
    --hostname) HOSTNAME="${2:-}"; shift 2;;
    --storage) STORAGE="${2:-}"; shift 2;;
    --template-storage) TEMPLATE_STORAGE="${2:-}"; shift 2;;
    --template) TEMPLATE="${2:-}"; shift 2;;
    --bridge) BRIDGE="${2:-}"; shift 2;;
    --ip) IP_CONFIG="${2:-}"; shift 2;;
    --gateway) GATEWAY="${2:-}"; shift 2;;
    --cores) CORES="${2:-}"; shift 2;;
    --memory) MEMORY="${2:-}"; shift 2;;
    --swap) SWAP="${2:-}"; shift 2;;
    --disk) DISK="${2:-}"; shift 2;;
    --repo) REPO_URL="${2:-}"; shift 2;;
    --branch) REPO_BRANCH="${2:-}"; shift 2;;
    --ssh-public-key) SSH_PUBLIC_KEY="${2:-}"; shift 2;;
    --root-password) ROOT_PASSWORD="${2:-}"; shift 2;;
    --mount) MOUNTS+=("${2:-}"); shift 2;;
    --no-start) START="0"; shift;;
    --dry-run) DRY_RUN="1"; shift;;
    -h|--help) usage; exit 0;;
    *) die "Unknown option: $1";;
  esac
done

[[ -n "$CTID" ]] || die "--ctid is required"
[[ "$CTID" =~ ^[0-9]+$ ]] || die "--ctid must be numeric"
[[ -n "$HOSTNAME" ]] || die "--hostname cannot be empty"
[[ -n "$REPO_URL" ]] || die "--repo cannot be empty"
[[ -n "$REPO_BRANCH" ]] || die "--branch cannot be empty"

for mount in "${MOUNTS[@]}"; do
  [[ "$mount" == *:* ]] || die "Invalid --mount value '$mount'. Expected HOST:CT"
  host_path="${mount%%:*}"; ct_path="${mount#*:}"
  [[ -n "$host_path" && -n "$ct_path" ]] || die "Invalid --mount value '$mount'. Expected HOST:CT"
  [[ -e "$host_path" ]] || die "Host mount path does not exist: $host_path"
  [[ "$ct_path" == /* ]] || die "Container mount path must be absolute: $ct_path"
done

if [[ "$DRY_RUN" == "1" ]]; then
  cat <<EOF
Planned TvSorter LXC:
  CTID:             $CTID
  Hostname:         $HOSTNAME
  Privileged:       yes
  Storage:          $STORAGE
  Template storage: $TEMPLATE_STORAGE
  Template:         ${TEMPLATE:-auto Debian standard}
  Bridge:           $BRIDGE
  IP:               $IP_CONFIG
  Gateway:          ${GATEWAY:-none}
  CPU/RAM/Swap:     ${CORES} cores / ${MEMORY} MiB / ${SWAP} MiB
  Disk:             ${DISK} GiB
  Repo:             $REPO_URL
  Branch:           $REPO_BRANCH
  Mounts:           ${MOUNTS[*]:-none}
EOF
  exit 0
fi

[[ "${EUID:-$(id -u)}" -eq 0 ]] || die "Run this script as root on the Proxmox VE host"
require_cmd pct
require_cmd pveam
require_cmd pvesm

pct status "$CTID" >/dev/null 2>&1 && die "CTID $CTID already exists"
STORAGE="$(resolve_root_storage "$STORAGE")"
log "Using root storage: $STORAGE"
storage_exists "$STORAGE" || die "Storage not found: $STORAGE. $(available_storage_message)"
storage_supports_container_rootfs "$STORAGE" \
  || log "Warning: could not confirm storage '$STORAGE' supports container root disks. Continuing."

TEMPLATE_STORAGE="$(resolve_template_storage "$TEMPLATE_STORAGE")"
log "Using template storage: $TEMPLATE_STORAGE"
storage_exists "$TEMPLATE_STORAGE" || die "Template storage not found: $TEMPLATE_STORAGE. $(available_storage_message)"

if [[ -z "$TEMPLATE" ]]; then
  log "Finding latest Debian standard LXC template"
  pveam update >/dev/null
  TEMPLATE="$(pveam available --section system | awk '/debian-[0-9]+-standard_[^ ]+_amd64\.tar\.(zst|xz|gz)/ {print $2}' | sort -V | tail -n 1)"
  [[ -n "$TEMPLATE" ]] || die "Could not find a Debian standard template via pveam"
fi

if ! pveam list "$TEMPLATE_STORAGE" | awk '{print $1}' | grep -qx "${TEMPLATE_STORAGE}:vztmpl/${TEMPLATE}"; then
  log "Downloading template $TEMPLATE to $TEMPLATE_STORAGE"
  pveam download "$TEMPLATE_STORAGE" "$TEMPLATE"
fi

NET0="name=eth0,bridge=${BRIDGE},ip=${IP_CONFIG}"
if [[ -n "$GATEWAY" && "$IP_CONFIG" != "dhcp" ]]; then
  NET0="${NET0},gw=${GATEWAY}"
fi

create_args=(
  "$CTID"
  "${TEMPLATE_STORAGE}:vztmpl/${TEMPLATE}"
  --hostname "$HOSTNAME"
  --ostype debian
  --unprivileged 0
  --features nesting=1
  --cores "$CORES"
  --memory "$MEMORY"
  --swap "$SWAP"
  --rootfs "${STORAGE}:${DISK}"
  --net0 "$NET0"
  --onboot 1
  --tags "tvsorter;media"
)

[[ -n "$SSH_PUBLIC_KEY" ]] && { [[ -f "$SSH_PUBLIC_KEY" ]] || die "SSH public key not found: $SSH_PUBLIC_KEY"; create_args+=(--ssh-public-keys "$SSH_PUBLIC_KEY"); }
[[ -n "$ROOT_PASSWORD" ]] && create_args+=(--password "$ROOT_PASSWORD")

log "Creating privileged LXC $CTID"
pct create "${create_args[@]}"

mount_index=0
for mount in "${MOUNTS[@]}"; do
  host_path="${mount%%:*}"; ct_path="${mount#*:}"
  log "Adding bind mount mp${mount_index}: $host_path -> $ct_path"
  pct set "$CTID" -mp"${mount_index}" "${host_path},mp=${ct_path}"
  mount_index=$((mount_index + 1))
done

log "Starting LXC $CTID"
pct start "$CTID"

log "Waiting for container startup"
for _ in $(seq 1 60); do
  pct exec "$CTID" -- test -x /bin/bash >/dev/null 2>&1 && break
  sleep 1
done

install_script="$(mktemp)"
cleanup() { rm -f "$install_script"; }
trap cleanup EXIT

cat >"$install_script" <<'INSTALL'
#!/usr/bin/env bash
set -Eeuo pipefail

repo_url="$1"
repo_branch="$2"
update_url="$3"
build_url="$4"

export DEBIAN_FRONTEND=noninteractive
export RUSTUP_HOME=/opt/rust/rustup
export CARGO_HOME=/opt/rust/cargo

configure_autologin() {
  for unit in console-getty.service container-getty@1.service getty@tty1.service; do
    install -d "/etc/systemd/system/${unit}.d"
  done
  cat >/etc/systemd/system/console-getty.service.d/override.conf <<'UNIT'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear --keep-baud console 115200,38400,9600 $TERM
UNIT
  cat >/etc/systemd/system/container-getty@1.service.d/override.conf <<'UNIT'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear --keep-baud tty%I 115200,38400,9600 $TERM
UNIT
  cat >/etc/systemd/system/getty@tty1.service.d/override.conf <<'UNIT'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin root --noclear %I $TERM
UNIT
}

restart_getty_units() {
  systemctl daemon-reload
  for unit in console-getty.service container-getty@1.service getty@tty1.service; do
    if systemctl list-unit-files "$unit" --no-legend 2>/dev/null | grep -q "$unit"; then
      systemctl restart "$unit" || true
    fi
  done
}

apt-get update
apt-get install -y ca-certificates curl git build-essential pkg-config ffmpeg

# Node.js 20 (Vite requires Node 18+).
if ! command -v node >/dev/null 2>&1; then
  curl -fsSL https://deb.nodesource.com/setup_20.x | bash -
  apt-get install -y nodejs
fi

# Rust toolchain via rustup (Debian's packaged rustc is too old for our deps).
if [[ ! -x "${CARGO_HOME}/bin/cargo" ]]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
fi

if ! id tvsorter >/dev/null 2>&1; then
  useradd --system --home /var/lib/tvsorter --create-home --shell /usr/sbin/nologin tvsorter
fi
install -d -o tvsorter -g tvsorter /var/lib/tvsorter

if [[ ! -d /opt/tvsorter/.git ]]; then
  rm -rf /opt/tvsorter
  git clone --branch "$repo_branch" --depth 1 "$repo_url" /opt/tvsorter
else
  git -C /opt/tvsorter fetch origin "$repo_branch"
  git -C /opt/tvsorter checkout "$repo_branch"
  git -C /opt/tvsorter reset --hard "origin/${repo_branch}"
fi

# Build helper (frontend + release binary).
curl -fsSL "$build_url" -o /usr/local/lib/tvsorter-build.sh
# shellcheck disable=SC1091
source /usr/local/lib/tvsorter-build.sh
build_app /opt/tvsorter

install -m 0755 /opt/tvsorter/scripts/tvsorter-access.sh /usr/local/bin/tvsorter-access
curl -fsSL "$update_url" -o /usr/local/bin/update-tvsorter
chmod 0755 /usr/local/bin/update-tvsorter
ln -sf /usr/local/bin/update-tvsorter /usr/local/bin/update

configure_autologin

cat >/etc/systemd/system/tvsorter.service <<'UNIT'
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
UNIT

systemctl daemon-reload
restart_getty_units
systemctl enable --now tvsorter
INSTALL

log "Installing TvSorter inside LXC $CTID (this compiles from source and can take several minutes)"
pct push "$CTID" "$install_script" /root/install-tvsorter.sh -perms 0755
UPDATE_URL="https://raw.githubusercontent.com/mstraa/MediaSorter/${REPO_BRANCH}/scripts/update-tvsorter.sh"
BUILD_URL="https://raw.githubusercontent.com/mstraa/MediaSorter/${REPO_BRANCH}/scripts/build-app.sh"
pct exec "$CTID" -- /root/install-tvsorter.sh "$REPO_URL" "$REPO_BRANCH" "$UPDATE_URL" "$BUILD_URL"

[[ "$START" == "0" ]] && { log "Stopping LXC $CTID because --no-start was requested"; pct stop "$CTID"; }

ip_output="$(pct exec "$CTID" -- hostname -I 2>/dev/null || true)"
log "Done"
log "Container: $CTID ($HOSTNAME)"
log "Service: pct exec $CTID -- systemctl status tvsorter"
if [[ -n "$ip_output" ]]; then
  log "Open: http://${ip_output%% *}:8080"
else
  log "Open the container summary in Proxmox to find its IP, then browse to port 8080"
fi
