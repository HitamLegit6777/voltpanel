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
  SCHEMA_VERSION_MAX=20
  sqlite_user_version() {
    local db=$1
    [[ -r "$db" ]] || { printf '0'; return 1; }
    sqlite3 -readonly "$db" 'PRAGMA user_version;' 2>/dev/null || { printf '0'; return 1; }
  }
  sqlite_integrity_ok() {
    local db=$1
    [[ -r "$db" ]] || return 1
    [[ "$(sqlite3 -readonly "$db" 'PRAGMA integrity_check;' 2>/dev/null)" == ok ]]
  }
  confirm_or_force() {
    local force=$1 prompt=$2
    [[ "$force" == 1 ]] && return 0
    tui_available || die "$prompt (non-interactive shell; pass --force to skip this prompt)"
    tui_yesno "$prompt"
  }
  unique_stamp() {
    local stamp=$1 dir=${2:-$BACKUP_DIR} n=1
    while [[ -e "$dir/panel-$stamp.tar.gz" ]]; do stamp="${1}-${n}"; n=$((n + 1)); done
    printf '%s' "$stamp"
  }
  redact_config() {
    sed -E \
      -e 's/([[:space:]]*[A-Za-z0-9_.-]*(secret|password|passwd|token|api[_-]?key|private[_-]?key|master[_-]?key|sig(nature)?)[A-Za-z0-9_.-]*[[:space:]]*=[[:space:]]*).*/\1"REDACTED"/I' \
      -e 's/(secret|password|passwd|token|api[_-]?key|private[_-]?key|master[_-]?key|sig(nature)?)=[^[:space:]"]+/\1="REDACTED"/Ig'
  }
  prune_backups() {
    local dir=$1 keep=$2 f
    local -a old=()
    while IFS= read -r f; do [[ -n "$f" ]] && old+=("$f"); done < <(
      find "$dir" -maxdepth 1 -name 'panel-*.tar.gz' -printf '%T@ %p\n' 2>/dev/null \
        | sort -nr | awk -v keep="$keep" 'NR > keep { $1=""; sub(/^ /, ""); print }'
    )
    for f in "${old[@]}"; do
      printf 'pruning old backup: %s\n' "$(basename "$f")"
      rm -f -- "$f" "${f%.tar.gz}.sha256"
    done
  }
  validate_archive() {
    local archive=$1 line type found=0
    while IFS= read -r line; do
      [[ -n "$line" ]] || continue
      found=1
      while [[ "$line" == */ ]]; do line=${line%/}; done
      case "$line" in
        manifest.json|voltpanel.db|servers|backups|blueprints|websites|datalab|config) ;;
        servers/*|backups/*|blueprints/*|websites/*|datalab/*|config/*) ;;
        *) printf 'archive contains unexpected entry: %s\n' "$line" >&2; return 1 ;;
      esac
      [[ "$line" != *..* && "$line" != /* && "$line" != ./* ]] || {
        printf 'unsafe path in archive: %s\n' "$line" >&2
        return 1
      }
    done < <(tar -tzf "$archive" 2>/dev/null) || {
      printf 'unreadable archive: %s\n' "$archive" >&2
      return 1
    }
    (( found == 1 )) || { printf 'empty archive: %s\n' "$archive" >&2; return 1; }
    tar -tzf "$archive" 2>/dev/null | grep -qx manifest.json || {
      printf 'archive lacks manifest.json: %s\n' "$archive" >&2
      return 1
    }
    # Second pass for member types and permissions: only regular files and
    # directories may be restored. Symlinks are refused (a symlink member
    # followed by a member through it could redirect extraction outside the
    # restore root), and regular members carrying setuid/setgid/sticky or
    # world-writable modes are refused (privilege escalation on install).
    while IFS= read -r line; do
      [[ -n "$line" ]] || continue
      type=${line:0:1}
      case "$type" in
        -|d) ;;
        *) printf 'archive member is not a regular file or directory (type %s)\n' "$type" >&2; return 1 ;;
      esac
      if [[ "$type" == - ]]; then
        # tar -tv mode field: "-rwxr-xr-x" — setuid at 3, setgid at 6,
        # world-writable at 8, sticky at 9.
        perms=${line:0:10}
        if [[ "${perms:3:1}" == [sS] || "${perms:6:1}" == [sS] || "${perms:8:1}" == w || "${perms:9:1}" == [tT] ]]; then
          printf 'archive member has unsafe permissions: %s\n' "$line" >&2
          return 1
        fi
      fi
    done < <(tar -tvzf --quoting-style=shell "$archive" 2>/dev/null)
  }
  manifest_value() {
    local key=$1 manifest=$2 value
    value=$(printf '%s\n' "$manifest" | grep -F "\"$key\"" | head -n1)
    [[ -n "$value" ]] || { printf ''; return 1; }
    value=${value#*:}
    printf '%s' "$value" | tr -d '[:space:]"'
  }
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
  local data config_dir stamp archive staging sums_file manifest sym
  data=$(data_dir); [[ -n "$data" ]] || { printf 'Cannot read general.data_dir from %s\n' "$CONFIG_PATH" >&2; exit 1; }
  config_dir=$(dirname "$CONFIG_PATH")
  # A symlink anywhere in the trees that would be archived (data dirs, config
  # dir) produces a symlink member, which restore refuses — fail now instead
  # of shipping an archive that can never be restored.
  sym=$(find "$data" "$config_dir" -type l -print -quit 2>/dev/null || true)
  [[ -z "$sym" ]] || { printf 'Refusing backup: symlink in backup tree: %s\n' "$sym" >&2; exit 1; }
  install -d -m700 "$BACKUP_DIR"
  stamp=$(unique_stamp "$(date +%F-%H%M%S)")
  archive="$BACKUP_DIR/panel-$stamp.tar.gz"
  staging=$(mktemp -d)
  # Clear the trap inside the handler so it cannot re-fire when a caller
  # returns (staging is out of scope by then and set -u would abort).
  trap 'rm -rf "$staging"; trap - RETURN' RETURN
  if [[ -f "$data/voltpanel.db" ]]; then
    sqlite3 "$data/voltpanel.db" ".backup '$staging/voltpanel.db'" \
      || { printf 'SQLite online backup failed\n' >&2; exit 1; }
  else
    printf 'warning: no database at %s, skipping\n' "$data/voltpanel.db" >&2
  fi
  # Config tree (config.toml, tls/, first-run.env) staged under config/.
  # Symlinks were already refused by the guard above.
  if [[ -d "$config_dir" ]]; then
    cp -a "$config_dir/." "$staging/config/"
  else
    printf 'warning: no config directory at %s, skipping\n' "$config_dir" >&2
  fi
  local -a data_names=() entries=()
  # The database is archived from staging — a consistent .backup snapshot —
  # never from the live file, so the archived bytes match the staged checksum.
  [[ -f "$staging/voltpanel.db" ]] && entries+=(voltpanel.db)
  local name
  for name in servers backups blueprints websites datalab; do
    if [[ -e "$data/$name" ]]; then data_names+=("$name"); entries+=("$name"); fi
  done
  [[ -d "$staging/config" ]] && entries+=(config)
  if (( ${#entries[@]} == 0 )); then
    printf 'Nothing to back up (no database, data dirs or config found).\n' >&2
    exit 1
  fi
  # Per-file SHA-256 manifest, computed from the exact sources being archived.
  # Restore re-verifies every listed file before installing anything.
  sums_file="$staging/checksums.txt"
  : > "$sums_file"
  (
    cd "$staging" || exit 1
    [[ -f voltpanel.db ]] && sha256sum voltpanel.db
    if [[ -d config ]]; then
      cd config || exit 1
      find . -type f -print0 | sort -z | xargs -0 -r sha256sum \
        | sed 's|^\([0-9a-f]*\)  \.\(/.*\)$|\1  config\2|'
    fi
  ) >> "$sums_file"
  # Data dirs are archived straight from the live tree, so their checksums
  # come from the same source.
  (
    cd "$data" || exit 1
    for name in servers backups blueprints websites datalab; do
      [[ -d "$name" ]] && find "$name" -type f -print0 | sort -z | xargs -0 -r sha256sum
    done
    exit 0
  ) >> "$sums_file"
  manifest="$staging/manifest.json"
  {
    printf '{\n'
    printf '  "format_version": 1,\n'
    printf '  "tool": "voltpanel-manage backup",\n'
    printf '  "created_at": "%s",\n' "$(date -Is)"
    printf '  "schema_version": %s,\n' "$(sqlite_user_version "$staging/voltpanel.db" 2>/dev/null || printf 0)"
    printf '  "data_dir": "%s",\n' "$data"
    printf '  "config_dir": "%s",\n' "$config_dir"
    printf '  "entries": ['
    local first=1 e line hash path
    for e in "${entries[@]}"; do
      [[ "$first" == 1 ]] || printf ', '
      printf '"%s"' "$e"
      first=0
    done
    printf '],\n'
    printf '  "checksums": {\n'
    first=1
    while IFS= read -r line; do
      [[ -n "$line" ]] || continue
      hash=${line%% *}
      path=${line#*  }
      [[ "$first" == 1 ]] || printf ',\n'
      printf '    "%s": "sha256:%s"' "$path" "$hash"
      first=0
    done < "$sums_file"
    printf '\n  }\n'
    printf '}\n'
  } > "$manifest"
  local -a tar_sources=(-C "$staging" manifest.json)
  [[ -f "$staging/voltpanel.db" ]] && tar_sources+=(-C "$staging" voltpanel.db)
  (( ${#data_names[@]} )) && tar_sources+=(-C "$data" "${data_names[@]}")
  [[ -d "$staging/config" ]] && tar_sources+=(-C "$staging" config)
  tar -czf "$archive" "${tar_sources[@]}" || { printf 'could not create archive\n' >&2; exit 1; }
  (cd "$BACKUP_DIR" && sha256sum "panel-$stamp.tar.gz") > "$archive.sha256"
  prune_backups "$BACKUP_DIR" 10
  printf 'Backup stored: %s\n' "$archive"
  printf 'Checksum:      %s.sha256\n' "$archive"
}

# Snapshot the current database, data dirs and config into $dest so a failed
# restore can roll back every file it is about to touch. list.txt records the
# exact file set so rollback can also DELETE files a partial restore added.
snapshot_state() {
  local dest=$1 data=$2 config_dir=$3
  local -a names=()
  local n
  for n in voltpanel.db voltpanel.db-wal voltpanel.db-shm servers backups blueprints websites datalab; do
    [[ -e "$data/$n" ]] && names+=("$n")
  done
  if (( ${#names[@]} )); then
    tar -C "$data" -czf "$dest/data.tar.gz" "${names[@]}" || return 1
  fi
  if [[ -d "$config_dir" ]]; then
    tar -C "$(dirname "$config_dir")" -czf "$dest/config.tar.gz" "$(basename "$config_dir")" || return 1
  fi
  {
    [[ -f "$data/voltpanel.db" ]] && printf '%s\n' "$data/voltpanel.db"
    for n in "$data/voltpanel.db-wal" "$data/voltpanel.db-shm"; do
      [[ -e "$n" ]] && printf '%s\n' "$n"
    done
    for n in servers backups blueprints websites datalab; do
      [[ -d "$data/$n" ]] && find "$data/$n" -type f
    done
    [[ -d "$config_dir" ]] && find "$config_dir" -type f
  } > "$dest/list.txt"
}

rollback_state() {
  local dest=$1 data=$2 config_dir=$3 f
  if [[ -f "$dest/data.tar.gz" ]]; then
    tar -C "$data" -xzf "$dest/data.tar.gz" || return 1
  fi
  if [[ -f "$dest/config.tar.gz" ]]; then
    tar -C "$(dirname "$config_dir")" -xzf "$dest/config.tar.gz" || return 1
  fi
  [[ -f "$dest/list.txt" ]] || return 0
  # Delete every file the failed restore added that was not part of the
  # snapshot (tar extraction alone cannot remove files).
  while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    grep -Fqx "$f" "$dest/list.txt" || rm -f -- "$f"
  done < <(
    for n in servers backups blueprints websites datalab; do
      [[ -d "$data/$n" ]] && find "$data/$n" -type f
    done
    [[ -f "$data/voltpanel.db" ]] && printf '%s\n' "$data/voltpanel.db"
    [[ -d "$config_dir" ]] && find "$config_dir" -type f
  )
}


# Re-verify the staged extraction against the manifest checksums. Every
# checksum entry must match its extracted file, and every regular archive
# member (except manifest.json) must be listed in the checksums — an unlisted
# file was not part of the snapshot and must not be installed. Returns
# nonzero on the first mismatch.
verify_staged() {
  local staging=$1 manifest=$2 fail=0 hash key actual line f
  local -A expected=()
  while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    hash=${line%% *}
    key=${line#*  }
    expected[$key]=$hash
    actual=$(sha256sum "$staging/$key" 2>/dev/null | awk '{print $1}') || actual=''
    if [[ "$actual" != "$hash" ]]; then
      printf 'checksum mismatch: %s\n' "$key" >&2
      fail=1
    fi
  done < <(printf '%s\n' "$manifest" | sed -n 's/[[:space:]]*"\([^"]*\)"[[:space:]]*:[[:space:]]*"sha256:\([0-9a-f]*\)".*/\2  \1/p')
  while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    key=${f#"$staging"/}
    [[ "$key" == manifest.json ]] && continue
    [[ -n "${expected[$key]+x}" ]] || {
      printf 'archive member missing from manifest checksums: %s\n' "$key" >&2
      fail=1
    }
  done < <(find "$staging" -type f -print)
  return "$fail"
}

install_staged() {
  local staging=$1 data=$2 config_dir=$3
  install -d -m700 "$data"
  if [[ -f "$staging/voltpanel.db" ]]; then
    # Drop stale WAL/SHM first: the archived db is a complete online-backup
    # artifact and must not replay a leftover journal on top of itself.
    rm -f "$data/voltpanel.db-wal" "$data/voltpanel.db-shm"
    install -m600 -o root -g root "$staging/voltpanel.db" "$data/voltpanel.db"
  fi
  local name f
  for name in servers backups blueprints websites datalab; do
    [[ -d "$staging/$name" ]] || continue
    # Sync, not merge: drop destination files that are not in the archive so
    # the restore lands exactly on the backed-up state. The pre-restore
    # snapshot (rollback_state) covers these removals.
    if [[ -d "$data/$name" ]]; then
      while IFS= read -r f; do
        [[ -f "$staging/$name/${f#"$data/$name"/}" ]] || rm -f -- "$f"
      done < <(find "$data/$name" -type f)
    fi
    cp -a "$staging/$name" "$data/"
    # Normalize: data dirs 0750, data files 0640. Archive modes were already
    # stripped at extraction (--no-same-permissions); never preserve owners.
    find "$data/$name" -type d -exec chmod 750 {} +
    find "$data/$name" -type f -exec chmod 640 {} +
  done
  if [[ -d "$staging/config" ]]; then
    install -d -m700 "$config_dir"
    while IFS= read -r f; do
      [[ -f "$staging/config/${f#"$config_dir"/}" ]] || rm -f -- "$f"
    done < <(find "$config_dir" -type f)
    cp -a "$staging/config/." "$config_dir/"
    # Normalize: config dirs 0700, config files 0640.
    find "$config_dir" -type d -exec chmod 700 {} +
    find "$config_dir" -type f -exec chmod 640 {} +
  fi
}

validate_restored() {
  local data=$1 ok=0 uv bin
  if [[ -f "$data/voltpanel.db" ]]; then
    sqlite_integrity_ok "$data/voltpanel.db" || {
      printf 'integrity_check failed on the restored database\n' >&2
      ok=1
    }
    uv=$(sqlite_user_version "$data/voltpanel.db") || { printf 'cannot read restored schema version\n' >&2; ok=1; }
    (( uv <= SCHEMA_VERSION_MAX )) || {
      printf 'restored database schema v%s is newer than this binary supports (v%s)\n' "$uv" "$SCHEMA_VERSION_MAX" >&2
      ok=1
    }
  fi
  bin=$(command -v voltpanel) || bin=/usr/local/bin/voltpanel
  if [[ -x "$bin" ]]; then
    VOLTPANEL_CONFIG="$CONFIG_PATH" "$bin" check-config --config "$CONFIG_PATH" >/dev/null 2>&1 || {
      printf 'check-config failed on the restored configuration\n' >&2
      ok=1
    }
  else
    printf 'warning: voltpanel binary not found; skipped check-config\n' >&2
  fi
  return "$ok"
}

restore() {
  require_root
  local archive=${1:-} force=0
  [[ ${2:-} == --force ]] && force=1
  if [[ -z "$archive" || ! -f "$archive" ]]; then
    printf 'Usage: %s restore ARCHIVE [--force]\n' "$0" >&2
    return 1
  fi
  local data config_dir manifest fmt schema
  data=$(data_dir); [[ -n "$data" ]] || { printf 'Cannot read general.data_dir from %s\n' "$CONFIG_PATH" >&2; return 1; }
  config_dir=$(dirname "$CONFIG_PATH")
  if [[ -f "$archive.sha256" ]]; then
    (cd "$(dirname "$archive")" && sha256sum -c --quiet "$(basename "$archive").sha256" >/dev/null 2>&1) || {
      printf 'Archive checksum verification failed: %s\n' "$archive" >&2
      return 1
    }
  else
    printf 'WARNING: no .sha256 sidecar for %s; relying on manifest checksums only\n' "$archive" >&2
  fi
  validate_archive "$archive" || { printf 'Archive failed structural validation: %s\n' "$archive" >&2; return 1; }
  manifest=$(tar -xOzf "$archive" manifest.json 2>/dev/null) || { printf 'Archive has no manifest.json: %s\n' "$archive" >&2; return 1; }
  fmt=$(manifest_value format_version "$manifest")
  schema=$(manifest_value schema_version "$manifest")
  [[ "$fmt" == 1 ]] || { printf 'Unsupported archive format_version: %s\n' "${fmt:-?}" >&2; return 1; }
  [[ "$schema" =~ ^[0-9]+$ ]] || { printf 'Archive manifest has no numeric schema_version\n' >&2; return 1; }
  if (( schema > SCHEMA_VERSION_MAX )); then
    printf 'Archive schema v%s is newer than this binary supports (v%s). Refusing to restore; upgrade the panel first.\n' \
      "$schema" "$SCHEMA_VERSION_MAX" >&2
    return 1
  fi
  if systemctl is-active --quiet voltpanel 2>/dev/null; then
    if [[ "$force" != 1 ]]; then
      tui_available || { printf 'voltpanel is running; stop it or pass --force (non-interactive shell).\n' >&2; return 1; }
      tui_yesno 'Stop voltpanel now and restore? (local workloads will stop)' || { printf 'Restore cancelled.\n'; return 1; }
    fi
    systemctl stop voltpanel || { printf 'Could not stop voltpanel; aborting restore.\n' >&2; return 1; }
  fi
  local staging rollback rc=0
  staging=$(mktemp -d); rollback=$(mktemp -d)
  if ! snapshot_state "$rollback" "$data" "$config_dir"; then
    printf 'Could not snapshot the current state; aborting restore.\n' >&2
    rc=1
  elif ! tar -xzf "$archive" --no-same-owner --no-same-permissions -C "$staging" 2>/dev/null; then
    printf 'Archive extraction failed.\n' >&2
    rc=1
  elif ! verify_staged "$staging" "$manifest"; then
    printf 'Checksum verification failed; nothing was installed.\n' >&2
    rc=1
  else
    install_staged "$staging" "$data" "$config_dir" || rc=1
    if (( rc == 0 )); then
      validate_restored "$data" || rc=1
    fi
    if (( rc == 0 )); then
      if ! systemctl start voltpanel 2>/dev/null || ! systemctl is-active --quiet voltpanel 2>/dev/null; then
        printf 'Restored files are in place but voltpanel failed to start.\n' >&2
        rc=1
      fi
    fi
  fi
  if (( rc != 0 )); then
    printf 'Restore failed; rolling back the previous state.\n' >&2
    rollback_state "$rollback" "$data" "$config_dir" || printf 'warning: rollback incomplete\n' >&2
    systemctl start voltpanel >/dev/null 2>&1 || true
    rm -rf "$staging" "$rollback"
    return 1
  fi
  rm -rf "$staging" "$rollback"
  printf 'Restore complete: %s\n' "$archive"
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
  local version=${1:-latest} temp sums previous cfg_backup
  temp=$(mktemp /usr/local/bin/.voltpanel.upgrade.XXXXXX); sums=$(mktemp); previous=$(mktemp); cfg_backup=$(mktemp)
  trap 'rm -f "$temp" "$sums" "$previous" "$cfg_backup"' RETURN
  download_release voltpanel "$version" "$temp" "$sums"
  # Snapshot the pre-upgrade state BEFORE touching the config: the backup must
  # preserve the exact running config (obsolete keys included) so a later
  # failure cannot lose the original.
  backup
  cp --preserve=mode,ownership,timestamps "$CONFIG_PATH" "$cfg_backup"
  strip_dead_config_keys "$CONFIG_PATH"
  if ! VOLTPANEL_CONFIG="$CONFIG_PATH" "$temp" check-config --config "$CONFIG_PATH"; then
    printf 'Config validation failed after removing obsolete keys; restoring the original config.\n' >&2
    cp --preserve=mode,ownership,timestamps "$cfg_backup" "$CONFIG_PATH"
    return 1
  fi
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

build_panel_bundle() {
  local bundle=$1 dir
  [[ -d "$bundle" ]] && { printf 'bundle path must be a file: %s\n' "$bundle" >&2; return 1; }
  dir=$(mktemp -d)
  # Clear the trap inside the handler so it cannot re-fire when the CALLER
  # returns (dir is out of scope by then and set -u would abort).
  trap 'rm -rf "$dir"; trap - RETURN' RETURN
  {
    printf 'voltpanel diagnostics bundle\ncreated_at: %s\nhostname: %s\n' "$(date -Is)" "$(hostname 2>/dev/null || uname -n)"
    printf '\n== version ==\n'
    (command -v voltpanel >/dev/null && voltpanel --version 2>&1 || printf 'voltpanel binary not found\n') || true
    printf '\n== config (redacted) ==\n'
    if [[ -r "$CONFIG_PATH" ]]; then redact_config < "$CONFIG_PATH" || true; else printf 'missing: %s\n' "$CONFIG_PATH"; fi
    printf '\n== service ==\n'
    systemctl --no-pager --full status voltpanel 2>&1 || true
    printf '\n== database ==\n'
    local data
    data=$(data_dir 2>/dev/null) || data=''
    if [[ -n "$data" && -f "$data/voltpanel.db" ]]; then
      printf 'integrity_check: %s\n' "$(sqlite3 -readonly "$data/voltpanel.db" 'PRAGMA integrity_check;' 2>/dev/null || printf 'unavailable')"
      printf 'schema_version: %s\n' "$(sqlite_user_version "$data/voltpanel.db" 2>/dev/null || printf 'unavailable')"
      printf 'size_bytes: %s\n' "$(stat -c %s "$data/voltpanel.db" 2>/dev/null || printf 'unavailable')"
      stat -c 'perms: %a owner: %U:%G' "$data/voltpanel.db" 2>/dev/null || true
    else
      printf 'missing: %s/voltpanel.db\n' "$data"
    fi
    printf '\n== resources ==\n'
    df -h 2>/dev/null || true
    printf '\n'
    free -h 2>/dev/null || true
    printf '\n'
    uptime 2>/dev/null || true
    printf '\n== logs (redacted, tail 200) ==\n'
    journalctl -u voltpanel -n 200 --no-pager 2>/dev/null | redact_config || true
  } > "$dir/diagnostics.txt"
  install -d -m700 "$(dirname "$bundle")"
  tar -C "$dir" -czf "$bundle" diagnostics.txt || return 1
  chmod 600 "$bundle"
  printf 'Diagnostics bundle written to %s (mode 0600)\n' "$bundle"
}

doctor() {
  local bundle=''
  if [[ ${1:-} == --bundle ]]; then
    bundle=${2:-}
    if [[ -z "$bundle" ]]; then printf 'Usage: %s doctor --bundle PATH\n' "$0" >&2; return 1; fi
    build_panel_bundle "$bundle"
    return $?
  fi
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
  restore) restore "${2:-}" "${3:-}" ;;
  upgrade) upgrade "${2:-latest}" ;;
  doctor) doctor "${2:-}" "${3:-}" ;;
  reset-password) reset_password "${2:-admin}" ;;
  uninstall) uninstall "${2:-}" ;;
  *) printf 'Usage: %s {status|logs|backup|restore ARCHIVE [--force]|upgrade [VERSION]|reset-password [USERNAME]|doctor [--bundle PATH]|uninstall --purge}\n' "$0" ;;
esac
