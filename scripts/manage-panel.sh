#!/usr/bin/env bash
set -Eeuo pipefail

REPO=${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}
CONFIG_PATH=${VOLTPANEL_CONFIG:-/etc/voltpanel/config.toml}
BACKUP_DIR=${VOLTPANEL_BACKUP_DIR:-/var/backups/voltpanel}
SCRIPT_PATH=${BASH_SOURCE[0]:-$0}
SCRIPT_DIR=$(cd -- "$(dirname -- "$SCRIPT_PATH")" && pwd)

# Prefer the shared helper library (repo checkout, or installed copy managed by
# install-panel.sh); fall back to self-contained copies when running standalone.
if [[ -f "$SCRIPT_DIR/lib/common.sh" ]]; then
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/lib/common.sh"
elif [[ -f /usr/share/voltpanel/common.sh ]]; then
  # shellcheck disable=SC1091
  source /usr/share/voltpanel/common.sh
else
  require_root() { [[ ${EUID:-$(id -u)} -eq 0 ]] || { printf 'Run as root.\n' >&2; exit 1; }; }
  # The purge safety guard lives only in lib/common.sh. Never purge without
  # it: silently skipping the check would make `uninstall --purge` dangerous.
  unsafe_purge_path() { printf 'Missing lib/common.sh; refusing to purge without the safety guard.\n' >&2; exit 1; }
fi

arch() {
  case "$(uname -m)" in
    x86_64|amd64) printf 'linux-amd64' ;;
    aarch64|arm64) printf 'linux-arm64' ;;
    *) printf 'unsupported architecture\n' >&2; exit 1 ;;
  esac
}
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

# Older installers wrote keys the panel config schema no longer accepts
# (timezone, base_path, jwt_secret, userland). Config is now
# deny_unknown_fields, so strip them before check-config or an upgrade
# refuses to validate the existing config.
strip_dead_config_keys() {
  local file=$1
  if grep -Eq '^[[:space:]]*(timezone|base_path|jwt_secret|userland)[[:space:]]*=' "$file"; then
    sed -i -E '/^[[:space:]]*(timezone|base_path|jwt_secret|userland)[[:space:]]*=/d' "$file" \
      || { printf 'Could not rewrite %s; remove the obsolete keys manually\n' "$file" >&2; return 1; }
    printf 'Removed obsolete config keys (timezone, base_path, jwt_secret, userland) from %s\n' "$file"
  fi
}
backup() {
  require_root
  local data stamp archive entries=()
  data=$(data_dir); [[ -n "$data" ]] || { printf 'Cannot read general.data_dir from %s\n' "$CONFIG_PATH" >&2; exit 1; }
  stamp=$(date +%F-%H%M%S); install -d -m700 "$BACKUP_DIR"
  if [[ -f "$data/voltpanel.db" ]]; then sqlite3 "$data/voltpanel.db" ".backup '$BACKUP_DIR/panel-$stamp.db'"; else printf 'warning: no database at %s, skipping\n' "$data/voltpanel.db" >&2; fi
  local name
  for name in servers backups blueprints websites; do [[ -e "$data/$name" ]] && entries+=("$name"); done
  if ((${#entries[@]})); then archive="$BACKUP_DIR/files-$stamp.tar.gz"; tar -C "$data" -czf "$archive" "${entries[@]}"; fi
  printf 'Backup stored in %s\n' "$BACKUP_DIR"
}
download_release() {
  local binary=$1 version=$2 output=$3 sums=$4 asset base expected actual identity status
  asset="$binary-$(arch)"
  if [[ "$version" == latest ]]; then base="https://github.com/$REPO/releases/latest/download"; else base="https://github.com/$REPO/releases/download/$version"; fi
  curl --fail --location --retry 3 --connect-timeout 15 "$base/$asset" -o "$output"
  curl --fail --location --retry 3 --connect-timeout 15 "$base/SHA256SUMS" -o "$sums"
  expected=$(awk -v asset="$asset" '$2==asset{print $1}' "$sums")
  actual=$(sha256sum "$output" | awk '{print $1}')
  [[ -n "$expected" && "$expected" == "$actual" ]] || { printf 'Checksum mismatch for %s\n' "$asset" >&2; exit 1; }
  chmod 0755 "$output"
  identity=$(timeout 5 "$output" --version 2>&1) || { status=$?; printf 'Downloaded binary failed version check (exit %s): %s\n' "$status" "${identity:-no output}" >&2; exit 1; }
  [[ "$identity" == "voltpanel "* ]] || { printf 'Unexpected binary identity: %s\n' "$identity" >&2; exit 1; }
  printf 'Downloaded %s\n' "$identity"
}
upgrade() {
  require_root
  local version=${1:-latest} temp sums previous
  temp=$(mktemp /usr/local/bin/.voltpanel.upgrade.XXXXXX); sums=$(mktemp); previous=$(mktemp)
  trap 'rm -f "$temp" "$sums" "$previous"' RETURN
  download_release voltpanel "$version" "$temp" "$sums"
  strip_dead_config_keys "$CONFIG_PATH"
  VOLTPANEL_CONFIG="$CONFIG_PATH" "$temp" check-config --config "$CONFIG_PATH"
  backup
  cp --preserve=mode,ownership,timestamps /usr/local/bin/voltpanel "$previous"
  systemctl stop voltpanel
  # Disable Restart=on-failure during the swap so a crash of the freshly
  # installed binary cannot make systemd relaunch it mid-rollback.
  systemctl set-property --runtime voltpanel Restart=no || true
  install -m755 "$temp" /usr/local/bin/voltpanel
  if ! systemctl start voltpanel || ! systemctl is-active --quiet voltpanel; then
    printf 'Upgrade failed; restoring previous binary.\n' >&2
    install -m755 "$previous" /usr/local/bin/voltpanel
    systemctl set-property --runtime voltpanel Restart=on-failure || true
    systemctl restart voltpanel || true
    return 1
  fi
  systemctl set-property --runtime voltpanel Restart=on-failure || true
  systemctl --no-pager --full status voltpanel
  return 0
}
reset_password() {
  require_root
  reset_panel_password "$CONFIG_PATH" "${1:-admin}"
}

doctor() {
  local failures=0 cmd url data curl_args=(-fsS)
  for cmd in bwrap setpriv nft ip sqlite3; do if command -v "$cmd" >/dev/null; then printf 'ok: %s\n' "$cmd"; else printf 'missing: %s\n' "$cmd"; failures=$((failures+1)); fi; done
  if [[ -f /sys/fs/cgroup/cgroup.controllers ]]; then printf 'ok: cgroup v2\n'; else printf 'missing: cgroup v2\n'; failures=$((failures+1)); fi
  if systemctl is-active --quiet voltpanel; then printf 'ok: service\n'; else printf 'failed: service\n'; failures=$((failures+1)); fi
  [[ -r "$CONFIG_PATH" ]] || { printf 'missing: %s\n' "$CONFIG_PATH"; failures=$((failures+1)); }
  if [[ -r "$CONFIG_PATH" ]]; then
    data=$(data_dir)
    if [[ -f "$data/voltpanel.db" ]]; then
      stat -c '%a %n' "$CONFIG_PATH" "$data/voltpanel.db" 2>/dev/null || failures=$((failures+1))
      [[ $(sqlite3 -readonly "$data/voltpanel.db" 'PRAGMA quick_check;' 2>/dev/null) == ok ]] || { printf 'failed: SQLite quick_check\n'; failures=$((failures+1)); }
    else
      printf 'missing: %s\n' "$data/voltpanel.db"; failures=$((failures+1))
    fi
    [[ $(config_value web tls_self_signed) == true ]] && curl_args+=(-k)
    url=$(health_url); if curl "${curl_args[@]}" "$url" >/dev/null; then printf 'ok: local HTTP endpoint\n'; else printf 'failed: %s\n' "$url"; failures=$((failures+1)); fi
  fi
  journalctl -u voltpanel -n 30 --no-pager || true
  return "$failures"
}
uninstall() {
  require_root
  [[ ${1:-} == --purge ]] || { printf 'Use: %s uninstall --purge\n' "$0"; exit 1; }
  local data
  data=$(data_dir 2>/dev/null) || data=''
  systemctl disable --now voltpanel || true
  rm -f /etc/systemd/system/voltpanel.service /usr/local/bin/voltpanel /usr/local/sbin/voltpanel-manage
  rm -rf /etc/voltpanel
  if [[ -n "$data" ]] && unsafe_purge_path "$data"; then
    printf 'Refusing to purge unsafe data path: %s\n' "$data" >&2
    exit 1
  fi
  [[ -n "$data" ]] && rm -rf -- "$data"
  if declare -F cleanup_proxy_artifacts >/dev/null; then
    cleanup_proxy_artifacts panel
  else
    rm -f /etc/caddy/conf.d/voltpanel-panel.caddy /etc/nginx/conf.d/voltpanel-panel.conf \
      /etc/systemd/system/voltpanel-certbot-panel.service /etc/systemd/system/voltpanel-certbot-panel.timer \
      /etc/letsencrypt/renewal-hooks/deploy/voltpanel-panel-nginx \
      /etc/voltpanel/tls/panel-cloudflare.pem /etc/voltpanel/tls/panel-cloudflare.key
  fi
  systemctl daemon-reload
}

case ${1:-help} in
  status) systemctl status voltpanel --no-pager || true; if [[ -r "$CONFIG_PATH" ]]; then args=(-fsS); [[ $(config_value web tls_self_signed) == true ]] && args+=(-k); curl "${args[@]}" "$(health_url)" || true; fi ;;
  logs) journalctl -u voltpanel -f ;;
  backup) backup ;;
  upgrade) upgrade "${2:-latest}" ;;
  doctor) doctor ;;
  reset-password) reset_password "${2:-admin}" ;;
  uninstall) uninstall "${2:-}" ;;
  *) printf 'Usage: %s {status|logs|backup|upgrade [VERSION]|reset-password [USERNAME]|doctor|uninstall --purge}\n' "$0" ;;
esac
