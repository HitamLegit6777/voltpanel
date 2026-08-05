#!/usr/bin/env bash
set -Eeuo pipefail

VOLTPANEL_REPO="${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}"
VOLTPANEL_VERSION="${VOLTPANEL_VERSION:-latest}"
VOLTPANEL_GITHUB="https://github.com/${VOLTPANEL_REPO}"
export VOLTPANEL_RAW="https://raw.githubusercontent.com/${VOLTPANEL_REPO}/main"
COLOR="${NO_COLOR:-}"
DRY_RUN="${DRY_RUN:-0}"

if [[ -t 1 && -z "$COLOR" ]]; then
  C_BLUE=$'\033[1;34m'; C_GREEN=$'\033[1;32m'; C_YELLOW=$'\033[1;33m'; C_RED=$'\033[1;31m'; C_RESET=$'\033[0m'
else
  C_BLUE=""; C_GREEN=""; C_YELLOW=""; C_RED=""; C_RESET=""
fi

log() { printf '%s[voltpanel]%s %s\n' "$C_BLUE" "$C_RESET" "$*"; }
ok() { printf '%s[ok]%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%s[warn]%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die() { printf '%s[error]%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; exit 1; }

run() {
  if [[ "$DRY_RUN" == "1" ]]; then printf '[dry-run]'; printf ' %q' "$@"; printf '\n'; else "$@"; fi
}

require_root() { [[ ${EUID:-$(id -u)} -eq 0 ]] || die "Run as root (sudo)."; }
require_systemd() { command -v systemctl >/dev/null || die "systemd is required."; [[ -d /run/systemd/system ]] || die "systemd is not running."; }

load_os() {
  [[ -r /etc/os-release ]] || die "Cannot detect Linux distribution."
  # shellcheck disable=SC1091
  source /etc/os-release
  OS_ID="${ID:-unknown}"
  OS_LIKE="${ID_LIKE:-}"
  case " $OS_ID $OS_LIKE " in
    *" debian "*|*" ubuntu "*) PKG_FAMILY=debian ;;
    *" fedora "*|*" rhel "*|*" centos "*|*" rocky "*|*" almalinux "*) PKG_FAMILY=rhel ;;
    *" arch "*) PKG_FAMILY=arch ;;
    *) die "Unsupported distribution: ${PRETTY_NAME:-$OS_ID}. Supported: Debian/Ubuntu, Fedora/RHEL/Rocky/Alma, Arch." ;;
  esac
}

install_packages() {
  local packages=(curl ca-certificates bubblewrap util-linux iproute2 nftables sqlite3 tar gzip openssl)
  case "$PKG_FAMILY" in
    debian) run apt-get update -y; run env DEBIAN_FRONTEND=noninteractive apt-get install -y "${packages[@]}" ;;
    rhel) run dnf install -y curl ca-certificates bubblewrap util-linux iproute nftables sqlite tar gzip openssl ;;
    arch) run pacman -Sy --noconfirm curl ca-certificates bubblewrap util-linux iproute2 nftables sqlite tar gzip openssl ;;
  esac
}

arch_asset() {
  case "$(uname -m)" in
    x86_64|amd64) printf 'linux-amd64' ;;
    aarch64|arm64) printf 'linux-arm64' ;;
    *) die "Unsupported CPU architecture: $(uname -m)" ;;
  esac
}

release_url() {
  local binary=$1 arch version_path
  arch=$(arch_asset)
  if [[ "$VOLTPANEL_VERSION" == "latest" ]]; then version_path=latest/download; else version_path="download/${VOLTPANEL_VERSION}"; fi
  printf '%s/releases/%s/%s-%s' "$VOLTPANEL_GITHUB" "$version_path" "$binary" "$arch"
}

release_base_url() {
  local version_path
  if [[ "$VOLTPANEL_VERSION" == "latest" ]]; then version_path=latest/download; else version_path="download/${VOLTPANEL_VERSION}"; fi
  printf '%s/releases/%s' "$VOLTPANEL_GITHUB" "$version_path"
}

install_binary() {
  local binary=$1 target="/usr/local/bin/$1" url temp checksums asset expected actual
  url=$(release_url "$binary"); asset="${binary}-$(arch_asset)"; temp=$(mktemp); checksums=$(mktemp)
  log "Downloading $binary from $url"
  if [[ "$DRY_RUN" == "1" ]]; then log "[dry-run] verify SHA256SUMS and install $binary -> $target"; rm -f "$temp" "$checksums"; return; fi
  curl --fail --location --retry 3 --connect-timeout 15 "$url" -o "$temp" || die "Binary download failed. Check the release and architecture."
  curl --fail --location --retry 3 --connect-timeout 15 "$(release_base_url)/SHA256SUMS" -o "$checksums" || die "Checksum download failed."
  expected=$(awk -v asset="$asset" '$2==asset{print $1}' "$checksums"); [[ -n "$expected" ]] || die "No checksum published for $asset"
  actual=$(sha256sum "$temp" | awk '{print $1}'); [[ "$actual" == "$expected" ]] || die "Checksum mismatch for $asset"
  install -m 0755 "$temp" "$target"; rm -f "$temp" "$checksums"
}
random_secret() { openssl rand -base64 "${1:-36}" | tr -d '\n'; }
validate_domain() { [[ $1 =~ ^([A-Za-z0-9-]+\.)+[A-Za-z]{2,}$ ]] || die "Invalid domain: $1"; }
validate_url() { [[ $1 =~ ^https?://[^[:space:]]+$ ]] || die "Invalid URL: $1"; }

install_caddy() {
  if command -v caddy >/dev/null; then return; fi
  log "Installing Caddy"
  case "$PKG_FAMILY" in
    debian)
      run apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gpg
      if [[ "$DRY_RUN" != "1" ]]; then
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' > /etc/apt/sources.list.d/caddy-stable.list
      fi
      run apt-get update -y; run apt-get install -y caddy ;;
    rhel) run dnf install -y 'dnf-command(copr)'; run dnf copr enable -y @caddy/caddy; run dnf install -y caddy ;;
    arch) run pacman -S --noconfirm caddy ;;
  esac
}

write_file() {
  local path=$1 mode=$2
  if [[ "$DRY_RUN" == "1" ]]; then log "[dry-run] write $path ($mode)"; cat >/dev/null; return; fi
  install -d -m 0755 "$(dirname "$path")"
  cat > "$path"
  chmod "$mode" "$path"
}

firewall_hint() {
  local role=$1
  if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q '^Status: active'; then
    if [[ "$role" == panel ]]; then
      run ufw allow 80/tcp; run ufw allow 443/tcp; run ufw allow 20000:30000/tcp; run ufw allow 20000:30000/udp
    else
      warn "UFW is active. Allow the node HTTPS endpoint and allocated game ports from trusted sources."
    fi
  fi
}

systemctl_reload_start() {
  local service=$1
  run systemctl daemon-reload
  run systemctl enable --now "$service"
  if [[ "$DRY_RUN" != "1" ]]; then
    sleep 2
    systemctl is-active --quiet "$service" || { systemctl status "$service" --no-pager || true; journalctl -u "$service" -n 80 --no-pager || true; die "$service failed to start"; }
  fi
}
