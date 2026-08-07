#!/usr/bin/env bash
set -Eeuo pipefail

REPO=${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}
CONFIG_PATH=${VOLTPANEL_CONFIG:-/etc/voltpanel/config.toml}
BACKUP_DIR=${VOLTPANEL_BACKUP_DIR:-/var/backups/voltpanel}

arch() {
  case "$(uname -m)" in
    x86_64|amd64) printf 'linux-amd64' ;;
    aarch64|arm64) printf 'linux-arm64' ;;
    *) printf 'unsupported architecture\n' >&2; exit 1 ;;
  esac
}
require_root() { [[ $EUID -eq 0 ]] || { printf 'Run as root.\n' >&2; exit 1; }; }
config_value() {
  local section=$1 key=$2
  awk -F= -v section="$section" -v key="$key" '
    /^\[/ { current=$0; gsub(/[[:space:]\[\]]/, "", current); next }
    current == section && $1 ~ "^[[:space:]]*" key "[[:space:]]*$" {
      value=$2; sub(/^[[:space:]]*/, "", value); sub(/[[:space:]]*$/, "", value); gsub(/^"|"$/, "", value); print value; exit
    }
  ' "$CONFIG_PATH"
}
data_dir() { config_value general data_dir; }
health_url() {
  local listen port scheme=http
  listen=$(config_value web listen); port=${listen##*:}; port=${port%]}
  [[ $(config_value web tls_self_signed) == true ]] && scheme=https
  printf '%s://127.0.0.1:%s/' "$scheme" "$port"
}
backup() {
  require_root
  local data stamp archive entries=()
  data=$(data_dir); [[ -n "$data" ]] || { printf 'Cannot read general.data_dir from %s\n' "$CONFIG_PATH" >&2; exit 1; }
  stamp=$(date +%F-%H%M%S); install -d -m700 "$BACKUP_DIR"
  sqlite3 "$data/voltpanel.db" ".backup '$BACKUP_DIR/panel-$stamp.db'"
  local name
  for name in servers backups blueprints websites; do [[ -e "$data/$name" ]] && entries+=("$name"); done
  if ((${#entries[@]})); then archive="$BACKUP_DIR/files-$stamp.tar.gz"; tar -C "$data" -czf "$archive" "${entries[@]}"; fi
  printf 'Backup stored in %s\n' "$BACKUP_DIR"
}
download_release() {
  local binary=$1 version=$2 output=$3 sums=$4 asset base expected actual identity
  asset="$binary-$(arch)"
  if [[ "$version" == latest ]]; then base="https://github.com/$REPO/releases/latest/download"; else base="https://github.com/$REPO/releases/download/$version"; fi
  curl --fail --location --retry 3 --connect-timeout 15 "$base/$asset" -o "$output"
  curl --fail --location --retry 3 --connect-timeout 15 "$base/SHA256SUMS" -o "$sums"
  expected=$(awk -v asset="$asset" '$2==asset{print $1}' "$sums")
  actual=$(sha256sum "$output" | awk '{print $1}')
  [[ -n "$expected" && "$expected" == "$actual" ]] || { printf 'Checksum mismatch for %s\n' "$asset" >&2; exit 1; }
  chmod 0755 "$output"
  identity=$(timeout 5 "$output" --version 2>&1) || { printf 'Downloaded binary failed version check\n' >&2; exit 1; }
  [[ "$identity" == "voltpanel "* ]] || { printf 'Unexpected binary identity: %s\n' "$identity" >&2; exit 1; }
  printf 'Downloaded %s\n' "$identity"
}
upgrade() {
  require_root
  local version=${1:-latest} temp sums previous
  temp=$(mktemp); sums=$(mktemp); previous=$(mktemp)
  trap 'rm -f "$temp" "$sums" "$previous"' RETURN
  download_release voltpanel "$version" "$temp" "$sums"
  VOLTPANEL_CONFIG="$CONFIG_PATH" "$temp" check-config --config "$CONFIG_PATH"
  backup
  cp --preserve=mode,ownership,timestamps /usr/local/bin/voltpanel "$previous"
  systemctl stop voltpanel
  install -m755 "$temp" /usr/local/bin/voltpanel
  if ! systemctl start voltpanel || ! systemctl is-active --quiet voltpanel; then
    printf 'Upgrade failed; restoring previous binary.\n' >&2
    install -m755 "$previous" /usr/local/bin/voltpanel
    systemctl restart voltpanel
    exit 1
  fi
  systemctl --no-pager --full status voltpanel
}
doctor() {
  local failures=0 cmd url curl_args=(-fsS)
  for cmd in bwrap setpriv nft ip sqlite3; do if command -v "$cmd" >/dev/null; then printf 'ok: %s\n' "$cmd"; else printf 'missing: %s\n' "$cmd"; failures=$((failures+1)); fi; done
  if [[ -f /sys/fs/cgroup/cgroup.controllers ]]; then printf 'ok: cgroup v2\n'; else printf 'missing: cgroup v2\n'; failures=$((failures+1)); fi
  if systemctl is-active --quiet voltpanel; then printf 'ok: service\n'; else printf 'failed: service\n'; failures=$((failures+1)); fi
  [[ -r "$CONFIG_PATH" ]] || { printf 'missing: %s\n' "$CONFIG_PATH"; failures=$((failures+1)); }
  if [[ -r "$CONFIG_PATH" ]]; then
    data=$(data_dir)
    stat -c '%a %n' "$CONFIG_PATH" "$data/voltpanel.db" 2>/dev/null || failures=$((failures+1))
    [[ $(sqlite3 "$data/voltpanel.db" 'PRAGMA quick_check;' 2>/dev/null) == ok ]] || { printf 'failed: SQLite quick_check\n'; failures=$((failures+1)); }
    [[ $(config_value web tls_self_signed) == true ]] && curl_args+=(-k)
    url=$(health_url); if curl "${curl_args[@]}" "$url" >/dev/null; then printf 'ok: local HTTP endpoint\n'; else printf 'failed: %s\n' "$url"; failures=$((failures+1)); fi
  fi
  journalctl -u voltpanel -n 30 --no-pager || true
  return "$failures"
}

case ${1:-help} in
  status) systemctl status voltpanel --no-pager || true; if [[ -r "$CONFIG_PATH" ]]; then args=(-fsS); [[ $(config_value web tls_self_signed) == true ]] && args+=(-k); curl "${args[@]}" "$(health_url)" || true; fi ;;
  logs) journalctl -u voltpanel -f ;;
  backup) backup ;;
  upgrade) upgrade "${2:-latest}" ;;
  doctor) doctor ;;
  uninstall) require_root; [[ ${2:-} == --purge ]] || { printf 'Use: %s uninstall --purge\n' "$0"; exit 1; }; systemctl disable --now voltpanel || true; rm -f /etc/systemd/system/voltpanel.service /usr/local/bin/voltpanel /usr/local/sbin/voltpanel-manage; rm -rf /etc/voltpanel "$(data_dir)"; systemctl daemon-reload ;;
  *) printf 'Usage: %s {status|logs|backup|upgrade [VERSION]|doctor|uninstall --purge}\n' "$0" ;;
esac
