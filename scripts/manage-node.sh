#!/usr/bin/env bash
set -Eeuo pipefail

REPO=${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}
CONFIG_PATH=${VOLTD_CONFIG:-/etc/voltpanel-node/voltd.toml}

arch() {
  case "$(uname -m)" in
    x86_64|amd64) printf 'linux-amd64' ;;
    aarch64|arm64) printf 'linux-arm64' ;;
    *) printf 'unsupported architecture\n' >&2; exit 1 ;;
  esac
}
require_root() { [[ $EUID -eq 0 ]] || { printf 'Run as root.\n' >&2; exit 1; }; }
config_value() {
  local key=$1
  awk -F= -v key="$key" '$1 ~ "^[[:space:]]*" key "[[:space:]]*$" { value=$2; sub(/^[[:space:]]*/, "", value); sub(/[[:space:]]*$/, "", value); gsub(/^"|"$/, "", value); print value; exit }' "$CONFIG_PATH"
}
download_release() {
  local version=$1 output=$2 sums=$3 asset base expected actual identity
  asset="voltd-$(arch)"
  if [[ "$version" == latest ]]; then base="https://github.com/$REPO/releases/latest/download"; else base="https://github.com/$REPO/releases/download/$version"; fi
  curl --fail --location --retry 3 --connect-timeout 15 "$base/$asset" -o "$output"
  curl --fail --location --retry 3 --connect-timeout 15 "$base/SHA256SUMS" -o "$sums"
  expected=$(awk -v asset="$asset" '$2==asset{print $1}' "$sums")
  actual=$(sha256sum "$output" | awk '{print $1}')
  [[ -n "$expected" && "$expected" == "$actual" ]] || { printf 'Checksum mismatch for %s\n' "$asset" >&2; exit 1; }
  chmod 0755 "$output"
  identity=$(timeout 5 "$output" --version 2>&1) || { printf 'Downloaded binary failed version check\n' >&2; exit 1; }
  [[ "$identity" == "voltd "* ]] || { printf 'Unexpected binary identity: %s\n' "$identity" >&2; exit 1; }
  printf 'Downloaded %s\n' "$identity"
}
upgrade() {
  require_root
  local version=${1:-latest} temp sums previous
  temp=$(mktemp); sums=$(mktemp); previous=$(mktemp)
  trap 'rm -f "$temp" "$sums" "$previous"' RETURN
  download_release "$version" "$temp" "$sums"
  "$temp" check-config --config "$CONFIG_PATH"
  cp --preserve=mode,ownership,timestamps /usr/local/bin/voltd "$previous"
  systemctl stop voltd
  install -m755 "$temp" /usr/local/bin/voltd
  if ! systemctl start voltd || ! systemctl is-active --quiet voltd; then
    printf 'Upgrade failed; restoring previous binary.\n' >&2
    install -m755 "$previous" /usr/local/bin/voltd
    systemctl restart voltd
    exit 1
  fi
  systemctl --no-pager --full status voltd
}
doctor() {
  local failures=0 cmd listen scheme
  for cmd in bwrap setpriv nft ip; do if command -v "$cmd" >/dev/null; then printf 'ok: %s\n' "$cmd"; else printf 'missing: %s\n' "$cmd"; failures=$((failures+1)); fi; done
  if [[ -f /sys/fs/cgroup/cgroup.controllers ]]; then printf 'ok: cgroup v2\n'; else printf 'missing: cgroup v2\n'; failures=$((failures+1)); fi
  if systemctl is-active --quiet voltd; then printf 'ok: service\n'; else printf 'failed: service\n'; failures=$((failures+1)); fi
  [[ -r "$CONFIG_PATH" ]] || { printf 'missing: %s\n' "$CONFIG_PATH"; failures=$((failures+1)); }
  if [[ -r "$CONFIG_PATH" ]]; then
    stat -c '%a %n' "$CONFIG_PATH" 2>/dev/null || failures=$((failures+1))
    listen=$(config_value listen); scheme=https; [[ $(config_value plaintext) == true ]] && scheme=http
    if curl -kfsS "$scheme://127.0.0.1:${listen##*:}/v1/health" >/dev/null; then printf 'ok: local health endpoint\n'; else printf 'failed: local health endpoint\n'; failures=$((failures+1)); fi
  fi
  journalctl -u voltd -n 30 --no-pager || true
  return "$failures"
}

case ${1:-help} in
  status) systemctl status voltd --no-pager || true ;;
  logs) journalctl -u voltd -f ;;
  upgrade) upgrade "${2:-latest}" ;;
  doctor) doctor ;;
  uninstall) require_root; [[ ${2:-} == --purge ]] || { printf 'Use: %s uninstall --purge\n' "$0"; exit 1; }; data=$(config_value data_dir); systemctl disable --now voltd || true; rm -f /etc/systemd/system/voltd.service /usr/local/bin/voltd /usr/local/sbin/voltd-manage; rm -rf /etc/voltpanel-node "$data"; systemctl daemon-reload ;;
  *) printf 'Usage: %s {status|logs|upgrade [VERSION]|doctor|uninstall --purge}\n' "$0" ;;
esac
