#!/usr/bin/env bash
set -Eeuo pipefail

REPO=${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}
CONFIG_PATH=${VOLTD_CONFIG:-/etc/voltpanel-node/voltd.toml}
SCRIPT_PATH=${BASH_SOURCE[0]:-$0}
SCRIPT_DIR=$(cd -- "$(dirname -- "$SCRIPT_PATH")" && pwd)

# Prefer the shared helper library (repo checkout, or installed copy managed by
# install-node.sh); fall back to self-contained copies when running standalone.
if [[ -f "$SCRIPT_DIR/lib/common.sh" ]]; then
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/lib/common.sh"
elif [[ -f /usr/share/voltpanel-node/common.sh ]]; then
  # shellcheck disable=SC1091
  source /usr/share/voltpanel-node/common.sh
else
  # Minimal self-contained fallback so the manager still runs when neither
  # the repo checkout nor the installed copy of lib/common.sh is present.
  require_root() { [[ ${EUID:-$(id -u)} -eq 0 ]] || { printf 'Run as root.\n' >&2; exit 1; }; }
  # The purge safety guard lives only in lib/common.sh. Never purge without
  # it: silently skipping the check would make `uninstall --purge` dangerous.
  unsafe_purge_path() { printf 'Missing lib/common.sh; refusing to purge without the safety guard.\n' >&2; exit 1; }
  die() { printf '[error] %s\n' "$*" >&2; exit 1; }
  warn() { printf '[warn] %s\n' "$*" >&2; }
  log() { printf '[voltpanel] %s\n' "$*"; }
  ok() { printf '[ok] %s\n' "$*"; }
  tui_available() { false; }
  tui_yesno() { return 1; }
  redact_config() {
    sed -E \
      -e 's/([[:space:]]*[A-Za-z0-9_.-]*(secret|password|passwd|token|api[_-]?key|private[_-]?key|sig(nature)?)[A-Za-z0-9_.-]*[[:space:]]*=[[:space:]]*).*/\1"REDACTED"/I' \
      -e 's/(secret|password|passwd|token|api[_-]?key|private[_-]?key|sig(nature)?)=[^[:space:]"]+/\1="REDACTED"/Ig'
  }
  sqlite_user_version() {
    local db=$1
    [[ -r "$db" ]] || { printf '0'; return 1; }
    sqlite3 -readonly "$db" 'PRAGMA user_version;' 2>/dev/null || { printf '0'; return 1; }
  }
 fi

# Live workload count for the voltd agent: non-empty workload cgroups under
# the delegated cgroup root plus systemd vp-*.scope units. Both are what the
# agent itself treats as "running" (cleanup refuses to touch a live cgroup).
node_running_workloads() {
  local count=0 cg_root scope_file
  if command -v systemctl >/dev/null && systemctl is-active --quiet voltd 2>/dev/null; then
    cg_root=$(systemctl show voltd -p ControlGroup --value 2>/dev/null)
    cg_root=${cg_root:-/system.slice/voltd.service}
    for scope_file in "/sys/fs/cgroup${cg_root}"/voltpanel/*/cgroup.procs; do
      [[ -e "$scope_file" && -s "$scope_file" ]] && count=$((count + 1))
    done
    while read -r scope_file; do
      [[ -n "$scope_file" ]] && count=$((count + 1))
    done < <(systemctl list-units --all --plain --no-legend 'vp-*.scope' 2>/dev/null | awk '$1 ~ /^vp-.*\.scope$/ {print $1}')
  fi
  printf '%s' "$count"
}

arch() {
  case "$(uname -m)" in
    x86_64|amd64) printf 'linux-amd64' ;;
    aarch64|arm64) printf 'linux-arm64' ;;
    *) printf 'unsupported architecture\n' >&2; exit 1 ;;
  esac
}
config_value() {
  local key=$1
  [[ -r "$CONFIG_PATH" ]] || return 0
  awk -F= -v key="$key" '$1 ~ "^[[:space:]]*" key "[[:space:]]*$" { value=$2; sub(/^[[:space:]]*/, "", value); sub(/[[:space:]]*$/, "", value); gsub(/^"|"$/, "", value); print value; exit }' "$CONFIG_PATH"
}
download_release() {
  local version=$1 output=$2 sums=$3 asset base expected actual identity status
  asset="voltd-$(arch)"
  if [[ "$version" == latest ]]; then base="https://github.com/$REPO/releases/latest/download"; else base="https://github.com/$REPO/releases/download/$version"; fi
  curl --fail --location --retry 3 --connect-timeout 15 "$base/$asset" -o "$output"
  curl --fail --location --retry 3 --connect-timeout 15 "$base/SHA256SUMS" -o "$sums"
  expected=$(awk -v asset="$asset" '$2==asset{print $1}' "$sums")
  actual=$(sha256sum "$output" | awk '{print $1}')
  [[ -n "$expected" && "$expected" == "$actual" ]] || { printf 'Checksum mismatch for %s\n' "$asset" >&2; exit 1; }
  chmod 0755 "$output"
  identity=$(timeout 5 "$output" --version 2>&1) || { status=$?; printf 'Downloaded binary failed version check (exit %s): %s\n' "$status" "${identity:-no output}" >&2; exit 1; }
  [[ "$identity" == "voltd "* ]] || { printf 'Unexpected binary identity: %s\n' "$identity" >&2; exit 1; }
  printf 'Downloaded %s\n' "$identity"
}
upgrade() {
  require_root
  local force=0 version=${1:-latest}
  if [[ "$version" == --force ]]; then force=1; version=${2:-latest}; fi
  local running
  running=$(node_running_workloads)
  if (( running > 0 )) && [[ "$force" == 0 ]]; then
    printf 'voltd is running %s workload(s); refusing to upgrade while workloads are live.\n' "$running" >&2
    printf 'Drain or stop them first, or pass --force to upgrade anyway (workloads will be stopped).\n' >&2
    return 1
  fi
  local temp sums previous
  temp=$(mktemp /usr/local/bin/.voltd.upgrade.XXXXXX); sums=$(mktemp); previous=$(mktemp)
  trap 'rm -f "$temp" "$sums" "$previous"' RETURN
  download_release "$version" "$temp" "$sums"
  "$temp" check-config --config "$CONFIG_PATH"
  cp --preserve=mode,ownership,timestamps /usr/local/bin/voltd "$previous"
  systemctl stop voltd
  # Disable Restart=on-failure during the swap so a crash of the freshly
  # installed binary cannot make systemd relaunch it mid-rollback.
  systemctl set-property --runtime voltd Restart=no || true
  install -m755 "$temp" /usr/local/bin/voltd
  if ! systemctl start voltd || ! systemctl is-active --quiet voltd; then
    printf 'Upgrade failed; restoring previous binary.\n' >&2
    install -m755 "$previous" /usr/local/bin/voltd
    systemctl set-property --runtime voltd Restart=on-failure || true
    systemctl restart voltd
    exit 1
  fi
  systemctl set-property --runtime voltd Restart=on-failure || true
  systemctl --no-pager --full status voltd
}
# Signed GET /v1/health: the agent requires HMAC auth on every route, so a bare
# curl can never confirm readiness. The manager signs with the config secret.
agent_health_check() {
  local listen=$1 node_id secret scheme port host payload ts nonce sig
  node_id=$(config_value node_id)
  secret=$(config_value secret)
  [[ -n "$node_id" && -n "$secret" ]] || return 1
  scheme=https; [[ $(config_value plaintext) == true ]] && scheme=http
  port=${listen##*:}
  [[ "$port" =~ ^[0-9]+$ ]] || return 1
  host=127.0.0.1; [[ "$listen" == \[* ]] && host='[::1]'
  ts=$(date +%s)
  nonce=$(openssl rand -hex 16)
  payload="GET
/v1/health
$ts
$nonce
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
  sig=$(printf '%s' "$payload" | openssl dgst -sha256 -hmac "$secret" 2>/dev/null | awk '{print $NF}')
  [[ -n "$sig" ]] || return 1
  curl -kfsS -o /dev/null \
    -H "x-volt-node: $node_id" \
    -H "x-volt-timestamp: $ts" \
    -H "x-volt-nonce: $nonce" \
    -H "x-volt-signature: $sig" \
    "$scheme://$host:$port/v1/health"
}
build_node_bundle() {
  local bundle=$1 dir
  [[ -d "$bundle" ]] && { printf 'bundle path must be a file: %s\n' "$bundle" >&2; return 1; }
  dir=$(mktemp -d)
  # Clear the trap inside the handler so it cannot re-fire when the CALLER
  # returns (dir is out of scope by then and set -u would abort).
  trap 'rm -rf "$dir"; trap - RETURN' RETURN
  {
    printf 'voltd diagnostics bundle\ncreated_at: %s\nhostname: %s\n' "$(date -Is)" "$(hostname 2>/dev/null || uname -n)"
    printf '\n== version ==\n'
    (command -v voltd >/dev/null && voltd --version 2>&1 || printf 'voltd binary not found\n') || true
    printf '\n== config (redacted) ==\n'
    if [[ -r "$CONFIG_PATH" ]]; then redact_config < "$CONFIG_PATH" || true; else printf 'missing: %s\n' "$CONFIG_PATH"; fi
    printf '\n== service ==\n'
    systemctl --no-pager --full status voltd 2>&1 || true
    printf '\n== workloads ==\n'
    printf 'running: %s\n' "$(node_running_workloads)"
    printf '\n== isolation ==\n'
    if [[ -f /sys/fs/cgroup/cgroup.controllers ]]; then printf 'cgroup v2: present\n'; else printf 'cgroup v2: absent\n'; fi
    for cmd in bwrap setpriv nft ip; do
      if command -v "$cmd" >/dev/null; then printf 'ok: %s\n' "$cmd"; else printf 'missing: %s\n' "$cmd"; fi
    done
    printf '\n== resources ==\n'
    df -h 2>/dev/null || true
    printf '\n'
    free -h 2>/dev/null || true
    printf '\n'
    uptime 2>/dev/null || true
    printf '\n== logs (redacted, tail 200) ==\n'
    journalctl -u voltd -n 200 --no-pager 2>/dev/null | redact_config || true
  } > "$dir/diagnostics.txt"
  install -d -m700 "$(dirname "$bundle")"
  tar -C "$dir" -czf "$bundle" diagnostics.txt || return 1
  chmod 600 "$bundle"
  printf 'Diagnostics bundle written to %s (mode 0600)\n' "$bundle"
}

doctor() {
  local bundle='' force=0
  if [[ ${1:-} == --bundle ]]; then
    bundle=${2:-}
    if [[ -z "$bundle" ]]; then printf 'Usage: %s doctor --bundle PATH [--force]\n' "$0" >&2; return 1; fi
    [[ ${3:-} == --force ]] && force=1
  elif [[ ${1:-} == --force ]]; then
    force=1
  fi
  if [[ -n "$bundle" ]]; then
    local running
    running=$(node_running_workloads)
    if (( running > 0 )) && [[ "$force" == 0 ]]; then
      printf 'voltd is running %s workload(s); a bundle may capture live workload output.\n' "$running" >&2
      printf 'Stop or drain them first, or pass --force to create the bundle anyway.\n' >&2
      return 1
    fi
    build_node_bundle "$bundle"
    return $?
  fi
  local failures=0 cmd listen
  for cmd in bwrap setpriv nft ip; do if command -v "$cmd" >/dev/null; then printf 'ok: %s\n' "$cmd"; else printf 'missing: %s\n' "$cmd"; failures=$((failures+1)); fi; done
  if [[ -f /sys/fs/cgroup/cgroup.controllers ]]; then printf 'ok: cgroup v2\n'; else printf 'missing: cgroup v2\n'; failures=$((failures+1)); fi
  if systemctl is-active --quiet voltd; then printf 'ok: service\n'; else printf 'failed: service\n'; failures=$((failures+1)); fi
  [[ -r "$CONFIG_PATH" ]] || { printf 'missing: %s\n' "$CONFIG_PATH"; failures=$((failures+1)); }
  if [[ -r "$CONFIG_PATH" ]]; then
    stat -c '%a %n' "$CONFIG_PATH" 2>/dev/null || failures=$((failures+1))
    listen=$(config_value listen)
    if agent_health_check "$listen" >/dev/null; then printf 'ok: local health endpoint\n'; else printf 'failed: local health endpoint\n'; failures=$((failures+1)); fi
  fi
  journalctl -u voltd -n 30 --no-pager || true
  return "$failures"
}

case ${1:-help} in
  status) systemctl status voltd --no-pager || true ;;
  logs) journalctl -u voltd -f ;;
  upgrade) upgrade "${2:-}" "${3:-}" ;;
  doctor) doctor "${2:-}" "${3:-}" "${4:-}" ;;
  uninstall)
    require_root
    [[ ${2:-} == --purge ]] || { printf 'Use: %s uninstall --purge\n' "$0"; exit 1; }
    data=$(config_value data_dir)
    [[ -n "$data" ]] || data=/var/lib/voltd
    systemctl disable --now voltd >/dev/null 2>&1 || true
    rm -f /etc/systemd/system/voltd.service /usr/local/bin/voltd /usr/local/sbin/voltd-manage /usr/share/voltpanel-node/common.sh
    rm -rf -- /etc/voltpanel-node
    if [[ -n "$data" ]] && unsafe_purge_path "$data"; then
      printf 'Refusing to purge unsafe data path: %s\n' "$data" >&2
      exit 1
    fi
    [[ -n "$data" ]] && rm -rf -- "$data"
    if declare -F cleanup_proxy_artifacts >/dev/null; then
      cleanup_proxy_artifacts node
    else
      rm -f /etc/caddy/conf.d/voltpanel-node.caddy /etc/nginx/conf.d/voltpanel-node.conf \
        /etc/systemd/system/voltpanel-certbot-node.service /etc/systemd/system/voltpanel-certbot-node.timer \
        /etc/letsencrypt/renewal-hooks/deploy/voltpanel-node-nginx \
        /etc/voltpanel/tls/node-cloudflare.pem /etc/voltpanel/tls/node-cloudflare.key
    fi
    systemctl daemon-reload
    ;;
  *) printf 'Usage: %s {status|logs|upgrade [--force] [VERSION]|doctor [--bundle PATH] [--force]|uninstall --purge}\n' "$0" ;;
esac
