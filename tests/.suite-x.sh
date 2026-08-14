#!/usr/bin/env bash
# Roundtrip test for voltpanel-manage backup/restore/doctor --bundle and
# voltd-manage workload guards. Runs entirely in a temp dir with stubbed
# systemctl/journalctl/curl and a fake voltpanel/voltd binary in PATH.
#
# Usage: sudo bash tests/ops-restore.test.sh
set -Eeuo pipefail
cd "$(dirname "$0")/.." || exit 1

FAILS=0
ok()   { printf 'ok:   %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1"; FAILS=$((FAILS + 1)); }

[[ ${EUID:-$(id -u)} -eq 0 ]] || { echo 'SKIP: ops-restore tests need root (chown of restored db)' >&2; exit 0; }
command -v sqlite3 >/dev/null || { echo 'SKIP: sqlite3 required' >&2; exit 0; }
command -v tar >/dev/null || { echo 'SKIP: tar required' >&2; exit 0; }

# --- syntax gate ------------------------------------------------------------
for f in scripts/manage-panel.sh scripts/manage-node.sh scripts/lib/common.sh tests/ops-restore.test.sh; do
  bash -n "$f" && ok "bash -n $f" || fail "bash -n $f"
done
if command -v shellcheck >/dev/null; then
  shellcheck scripts/manage-panel.sh scripts/manage-node.sh scripts/lib/common.sh \
    && ok 'shellcheck scripts' || fail 'shellcheck scripts'
fi

TMP=$(mktemp -d)
trap 'cp -a "$TMP" /tmp/suite-debug-preserved 2>/dev/null || true' EXIT
BIN=$TMP/bin; DATA=$TMP/var/lib/voltpanel; CFG=$TMP/etc/voltpanel; BK=$TMP/var/backups/voltpanel
mkdir -p "$BIN" "$DATA/servers/s1" "$DATA/backups" "$DATA/blueprints" "$DATA/websites/w1" "$DATA/datalab" "$CFG/tls" "$TMP/etc/voltpanel-node"

# --- stubs ------------------------------------------------------------------
cat > "$BIN/systemctl" <<'EOF'
#!/usr/bin/env bash
# test stub: service "active" when VOLT_SERVICE_ACTIVE=1; workloads when VOLT_SCOPES=1
case "${1:-}" in
  is-active) [[ "${VOLT_SERVICE_ACTIVE:-0}" == 1 ]] && exit 0 || exit 3 ;;
  stop) echo "stub: stop ${2:-}" ;;
  start) echo "stub: start ${2:-}" ;;
  status) echo "stub: ${2:-unit} active (running)" ; exit 0 ;;
  show)
    if [[ "$*" == *--value* ]]; then echo "/system.slice/voltd.service"
    else echo "ControlGroup=/system.slice/voltd.service"; fi ;;
  list-units)
    [[ "${VOLT_SCOPES:-0}" == 1 ]] && echo "vp-000000000001-1.scope running active" ;;
  *) echo "stub: systemctl $*" ;;
esac
EOF
cat > "$BIN/journalctl" <<'EOF'
#!/usr/bin/env bash
echo 'info: voltpanel started'
echo 'warn: login failed for admin'
echo 'debug: token=supersecretlogvalue'
echo 'info: workload secret = hunter2'
EOF
cat > "$BIN/curl" <<'EOF'
#!/usr/bin/env bash
echo 'curl stub: download refused' >&2
touch "${VOLT_CURL_MARKER:-/tmp/voltpanel-curl-called}"
exit 22
EOF
cat > "$BIN/voltpanel" <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
  --version|version) echo "voltpanel 0.0.0-test" ;;
  check-config) echo "valid config: ${3:-}" ; exit 0 ;;
  *) exit 0 ;;
esac
EOF
cat > "$BIN/voltd" <<'EOF'
#!/usr/bin/env bash
case "${1:-}" in
  --version|version) echo "voltd 0.0.0-test" ;;
  check-config) echo "valid config: ${3:-}" ; exit 0 ;;
  *) exit 0 ;;
esac
EOF
chmod +x "$BIN"/*
export PATH="$BIN:$PATH"

# --- fixtures ---------------------------------------------------------------
cat > "$CFG/config.toml" <<EOF
[general]
data_dir = "$DATA"
[web]
listen = "127.0.0.1:8080"
tls_self_signed = false
[webhooks]
secret = "topsecret-config-value"
master_key = "topsecret-master-key"
echo 'tls material' > "$CFG/tls/server.pem"
echo 'marker file that must survive config restore' > "$CFG/backup-marker.txt"
cat > "$TMP/etc/voltpanel-node/voltd.toml" <<EOF
node_id = "test-node"
secret = "topsecret-node-value"
listen = "127.0.0.1:9000"
plaintext = true
data_dir = "$TMP/var/lib/voltd"
EOF

sqlite3 "$DATA/voltpanel.db" <<'SQL'
CREATE TABLE test (k TEXT PRIMARY KEY);
INSERT INTO test VALUES ('original');
PRAGMA user_version = 19;
SQL
echo 's1 content' > "$DATA/servers/s1/file.txt"
echo 'bp' > "$DATA/blueprints/bp.json"
echo 'w1' > "$DATA/websites/w1/index.html"
echo 'spaced name' > "$DATA/servers/s1/my file.txt"
echo 'lab' > "$DATA/datalab/lab.db"
echo 'b1' > "$DATA/backups/b1.zip"

ls -la "$CFG" > /tmp/suite-cfg-before.txt
ls -la "$CFG/tls" >> /tmp/suite-cfg-before.txt
export VOLTPANEL_CONFIG=$CFG/config.toml
export VOLTPANEL_BACKUP_DIR=$BK

# --- 1. backup --------------------------------------------------------------
bash scripts/manage-panel.sh backup >/dev/null
archive=$(ls "$BK"/panel-*.tar.gz | head -n1)
cp "$archive" "$TMP/badsum.tar.gz"
printf '0000000000000000000000000000000000000000000000000000000000000000  badsum.tar.gz\n' > "$TMP/badsum.tar.gz.sha256"
if bash scripts/manage-panel.sh restore "$TMP/badsum.tar.gz" --force >/dev/null 2>&1; then
  fail 'archive with wrong checksum refused'
else
  ok 'archive with wrong checksum refused'
fi
[[ -n "$archive" && -f "$archive" ]] && ok 'backup created archive' || fail 'backup created archive'
[[ -f "$archive.sha256" ]] && ok 'backup created .sha256' || fail 'backup created .sha256'
tar -xOzf "$archive" manifest.json | grep -q '"format_version": 1' && ok 'manifest format_version=1' || fail 'manifest format_version=1'
tar -xOzf "$archive" manifest.json | grep -q '"schema_version": 19' && ok 'manifest schema_version=19' || fail 'manifest schema_version=19'
tar -xOzf "$archive" manifest.json | grep -q '"config/config.toml"' && ok 'manifest has config checksums' || fail 'manifest has config checksums'
tar -tzf "$archive" | grep -q '^config/config.toml$' && ok 'archive contains config/config.toml' || fail 'archive contains config/config.toml'
tar -tzf "$archive" | grep -q 'my file.txt' && ok 'archive keeps space-named files' || fail 'archive keeps space-named files'
tar -tzf "$archive" | grep -q '^datalab/lab.db$' && ok 'archive contains datalab' || fail 'archive contains datalab'

tar -tzf "$archive" | grep -q '^servers/s1/file.txt$' && ok 'archive contains servers' || fail 'archive contains servers'

# --- 2. retention keeps 10 --------------------------------------------------
for _ in $(seq 1 12); do bash scripts/manage-panel.sh backup >/dev/null; done
count=$(find "$BK" -maxdepth 1 -name 'panel-*.tar.gz' | wc -l)
[[ "$count" == 10 ]] && ok "retention keeps 10 (got $count)" || fail "retention keeps 10 (got $count)"

# retention pruned the first archive; re-grab the newest surviving snapshot
# (created before the modify step below, so it still holds the original state)
archive=$(find "$BK" -maxdepth 1 -name 'panel-*.tar.gz' -printf '%T@ %p\n' | sort -nr | head -n1 | awk '{print $2}')
[[ -n "$archive" && -f "$archive" ]] || fail 'no surviving archive after retention'


# --- 3. modify live state ---------------------------------------------------
sqlite3 "$DATA/voltpanel.db" "INSERT INTO test VALUES ('modified');"
rm "$DATA/servers/s1/file.txt"
echo evil > "$DATA/servers/s1/evil.txt"
echo new > "$DATA/websites/w1/new.html"
sed -i 's/topsecret-config-value/TAMPERED/' "$CFG/config.toml"

# --- 4. restore (service active, --force skips the prompt) -------------------
VOLT_SERVICE_ACTIVE=1 bash scripts/manage-panel.sh restore "$archive" --force >/dev/null \
  && ok 'restore --force succeeded' || fail 'restore --force succeeded'
[[ "$(sqlite3 "$DATA/voltpanel.db" 'SELECT k FROM test;')" == original ]] \
  && ok 'restore rolled db back to snapshot' || fail 'restore rolled db back to snapshot'
[[ -f "$DATA/servers/s1/file.txt" ]] && ok 'restore brought file.txt back' || fail 'restore brought file.txt back'
[[ -f "$DATA/servers/s1/my file.txt" ]] && ok 'restore brought space-named file back' || fail 'restore brought space-named file back'
[[ ! -e "$DATA/servers/s1/evil.txt" ]] && ok 'restore removed post-backup file' || fail 'restore removed post-backup file'
[[ ! -e "$DATA/websites/w1/new.html" ]] && ok 'restore removed post-backup website file' || fail 'restore removed post-backup website file'
grep -q 'topsecret-config-value' "$CFG/config.toml" && ok 'restore reverted config secret' || fail 'restore reverted config secret'
[[ -f "$CFG/backup-marker.txt" ]] && ok 'config restore kept unrelated config files' || fail 'config restore kept unrelated config files'
[[ "$(stat -c %a "$DATA/voltpanel.db")" == 600 ]] && ok 'restored db mode 600' || fail 'restored db mode 600'
[[ "$(stat -c %a "$CFG/config.toml")" == 640 ]] && ok 'restored config mode 0640' || fail 'restored config mode 0640'
[[ "$(stat -c %a "$DATA/servers")" == 750 ]] && ok 'restored data dir mode 0750' || fail 'restored data dir mode 0750'
[[ "$(stat -c %a "$CFG")" == 700 ]] && ok 'restored config dir mode 0700' || fail 'restored config dir mode 0700'

# --- 5. non-interactive restore while active refuses without --force ----------
before=$(sqlite3 "$DATA/voltpanel.db" 'SELECT count(*) FROM test;')
if VOLT_SERVICE_ACTIVE=1 bash scripts/manage-panel.sh restore "$archive" </dev/null >/dev/null 2>&1; then
  fail 'restore without --force refused when active'
else
  ok 'restore without --force refused when active'
fi
after=$(sqlite3 "$DATA/voltpanel.db" 'SELECT count(*) FROM test;')
[[ "$before" == "$after" ]] && ok 'refused restore touched nothing' || fail 'refused restore touched nothing'

# --- 6. newer-schema archive refused ----------------------------------------
mkdir -p "$TMP/bad"
tar -xzf "$archive" -C "$TMP/bad"
sed -i 's/"schema_version": 19/"schema_version": 99/' "$TMP/bad/manifest.json"
tar -czf "$TMP/newer.tar.gz" -C "$TMP/bad" .
if bash scripts/manage-panel.sh restore "$TMP/newer.tar.gz" --force >/dev/null 2>&1; then
  fail 'newer-schema archive refused'
else
  ok 'newer-schema archive refused'
fi

# --- 7. traversal archives refused ------------------------------------------
mkdir -p "$TMP/evilroot"
echo pwn > "$TMP/evilroot/evil"
tar -czf "$TMP/traversal.tar.gz" -C "$TMP/evilroot" --transform='s|^evil$|../evil|' evil
if bash scripts/manage-panel.sh restore "$TMP/traversal.tar.gz" --force >/dev/null 2>&1; then
  fail 'traversal archive refused'
else
  ok 'traversal archive refused'
fi
tar -czf "$TMP/absolute.tar.gz" -C "$TMP/evilroot" --transform='s|^evil$|/etc/evil|' evil
if bash scripts/manage-panel.sh restore "$TMP/absolute.tar.gz" --force >/dev/null 2>&1; then
  fail 'absolute-path archive refused'
else
  ok 'absolute-path archive refused'
fi
# a symlink member must be refused as well
mkdir -p "$TMP/symroot"
ln -s /etc "$TMP/symroot/evil-link"
tar -czf "$TMP/symlink.tar.gz" -C "$TMP/symroot" evil-link
if bash scripts/manage-panel.sh restore "$TMP/symlink.tar.gz" --force >/dev/null 2>&1; then
  fail 'symlink-member archive refused'
else
  ok 'symlink-member archive refused'
fi

# --- 8. panel doctor --bundle ------------------------------------------------
OUT=$TMP/diag.tar.gz
bash scripts/manage-panel.sh doctor --bundle "$OUT" >/dev/null
[[ -f "$OUT" ]] && ok 'doctor bundle created' || fail 'doctor bundle created'
[[ "$(stat -c %a "$OUT")" == 600 ]] && ok 'doctor bundle mode 0600' || fail "doctor bundle mode 0600 (got $(stat -c %a "$OUT"))"
BODY=$TMP/diag.txt
tar -xzOf "$OUT" diagnostics.txt > "$BODY"
grep -q 'voltpanel 0.0.0-test' "$BODY" && ok 'bundle has version' || fail 'bundle has version'
grep -q 'integrity_check: ok' "$BODY" && ok 'bundle has integrity' || fail 'bundle has integrity'
grep -q 'schema_version: 19' "$BODY" && ok 'bundle has schema version' || fail 'bundle has schema version'
! grep -q 'topsecret-config-value' "$BODY" && ok 'bundle redacts config secret' || fail 'bundle redacts config secret'
! grep -q 'supersecretlogvalue' "$BODY" && ok 'bundle redacts log token' || fail 'bundle redacts log token'
! grep -q 'hunter2' "$BODY" && ok 'bundle redacts log secret' || fail 'bundle redacts log secret'
! grep -q 'topsecret-master-key' "$BODY" && ok 'bundle redacts master key' || fail 'bundle redacts master key'

# --- 9. node upgrade/doctor guards -------------------------------------------
export VOLTD_CONFIG=$TMP/etc/voltpanel-node/voltd.toml
VOLT_SERVICE_ACTIVE=1 VOLT_SCOPES=1 VOLT_CURL_MARKER=$TMP/curl-called bash scripts/manage-node.sh upgrade >/dev/null 2>&1 \
  && fail 'node upgrade refused with running workloads' || ok 'node upgrade refused with running workloads'
[[ ! -e "$TMP/curl-called" ]] && ok 'refused node upgrade did not download' || fail 'refused node upgrade did not download'
rm -f "$TMP/curl-called"
VOLT_SERVICE_ACTIVE=1 VOLT_SCOPES=1 VOLT_CURL_MARKER=$TMP/curl-called bash scripts/manage-node.sh upgrade --force >/dev/null 2>&1 \
  && fail 'node upgrade --force reached download (expected download failure)' || ok 'node upgrade --force proceeded past guard'
[[ -e "$TMP/curl-called" ]] && ok 'node upgrade --force attempted download' || fail 'node upgrade --force attempted download'

NODE_OUT=$TMP/node-diag.tar.gz
if VOLT_SERVICE_ACTIVE=1 VOLT_SCOPES=1 bash scripts/manage-node.sh doctor --bundle "$NODE_OUT" >/dev/null 2>&1; then
  fail 'node doctor bundle refused with running workloads'
else
  ok 'node doctor bundle refused with running workloads'
fi
[[ ! -e "$NODE_OUT" ]] && ok 'refused node bundle not created' || fail 'refused node bundle not created'
VOLT_SERVICE_ACTIVE=1 VOLT_SCOPES=1 bash scripts/manage-node.sh doctor --bundle "$NODE_OUT" --force >/dev/null
[[ -f "$NODE_OUT" ]] && ok 'node doctor bundle --force created' || fail 'node doctor bundle --force created'
NODE_BODY=$TMP/node-diag.txt
tar -xzOf "$NODE_OUT" diagnostics.txt > "$NODE_BODY"
! grep -q 'topsecret-node-value' "$NODE_BODY" && ok 'node bundle redacts secret' || fail 'node bundle redacts secret'
grep -q 'running: 1' "$NODE_BODY" && ok 'node bundle reports workload count' || fail 'node bundle reports workload count'
# --- 10. failed restore rolls back the pre-restore state --------------------
# Snapshot file set: after.txt + the 'second' row exist now; will-delete.txt
# exists only inside the archive (deleted from the live tree after its backup),
# so a failed restore must install it and then roll it back out again.
echo 'only in archive' > "$DATA/servers/s1/will-delete.txt"
bash scripts/manage-panel.sh backup >/dev/null
rollback_archive=$(find "$BK" -maxdepth 1 -name 'panel-*.tar.gz' -printf '%T@ %p\n' | sort -nr | head -n1 | awk '{print $2}')
rm "$DATA/servers/s1/will-delete.txt"
echo 'snapshot file, must survive rollback' > "$DATA/servers/s1/after.txt"
sqlite3 "$DATA/voltpanel.db" "INSERT INTO test VALUES ('second');"
cp "$BIN/voltpanel" "$BIN/voltpanel.good"
printf '#!/usr/bin/env bash\nexit 1\n' > "$BIN/voltpanel"   # check-config now fails
if VOLT_SERVICE_ACTIVE=1 bash scripts/manage-panel.sh restore "$rollback_archive" --force >/dev/null 2>&1; then
  fail 'failed restore reported failure'
else
  ok 'failed restore reported failure'
fi
mv "$BIN/voltpanel.good" "$BIN/voltpanel"
[[ -f "$DATA/servers/s1/after.txt" ]] && ok 'rollback kept pre-restore file' || fail 'rollback kept pre-restore file'
[[ ! -e "$DATA/servers/s1/will-delete.txt" ]] && ok 'rollback removed restore-added file' || fail 'rollback removed restore-added file'
[[ "$(sqlite3 "$DATA/voltpanel.db" 'SELECT count(*) FROM test;')" == 2 ]] \
  && ok 'rollback restored pre-restore db' || fail 'rollback restored pre-restore db'

# --- 11. upgrade ordering: backup runs before the config is stripped --------
UPG=$(sed -n '/^upgrade()/,/^}/p' scripts/manage-panel.sh)
UPG_BACKUP=$(printf '%s\n' "$UPG" | grep -n '^[[:space:]]*backup$' | head -n1 | cut -d: -f1)
UPG_STRIP=$(printf '%s\n' "$UPG" | grep -n 'strip_dead_config_keys' | head -n1 | cut -d: -f1)
[[ -n "$UPG_BACKUP" && -n "$UPG_STRIP" && "$UPG_BACKUP" -lt "$UPG_STRIP" ]] \
  && ok 'upgrade backs up before stripping config keys' || fail 'upgrade backs up before stripping config keys'
printf '%s\n' "$UPG" | grep -q 'restoring the original config' \
  && ok 'upgrade restores config on check-config failure' || fail 'upgrade restores config on check-config failure'

# --- 12. WAL-mode database: uncheckpointed row survives backup/restore ------
sqlite3 "$DATA/voltpanel.db" 'PRAGMA journal_mode=WAL;' >/dev/null
cat > "$TMP/wal-hold.sql" <<'SQL'
INSERT INTO test VALUES ('wal-row');
.shell sleep 10
SQL
# Hold the connection open so the row stays ONLY in the WAL file (SQLite
# checkpoints when the last connection closes). Backup must capture it.
sqlite3 "$DATA/voltpanel.db" < "$TMP/wal-hold.sql" &
WAL_HOLDER=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do
  [[ -f "$DATA/voltpanel.db-wal" ]] && break
  sleep 1
done
[[ -f "$DATA/voltpanel.db-wal" ]] && ok 'WAL file present with uncheckpointed row' || fail 'WAL file present with uncheckpointed row'
cp "$DATA/voltpanel.db" "$TMP/wal-naive.db"
[[ "$(sqlite3 "$TMP/wal-naive.db" 'SELECT count(*) FROM test WHERE k='"'"'wal-row'"'"';')" == 0 ]] \
  && ok 'row confirmed held only in WAL at backup time' || fail 'row confirmed held only in WAL at backup time'
bash scripts/manage-panel.sh backup >/dev/null
wait "$WAL_HOLDER"
wal_archive=$(find "$BK" -maxdepth 1 -name 'panel-*.tar.gz' -printf '%T@ %p\n' | sort -nr | head -n1 | awk '{print $2}')
sqlite3 "$DATA/voltpanel.db" "DELETE FROM test WHERE k='wal-row';"
VOLT_SERVICE_ACTIVE=1 bash scripts/manage-panel.sh restore "$wal_archive" --force >/dev/null \
  && ok 'WAL archive restore succeeded' || fail 'WAL archive restore succeeded'
[[ "$(sqlite3 "$DATA/voltpanel.db" 'SELECT count(*) FROM test WHERE k='"'"'wal-row'"'"';')" == 1 ]] \
  && ok 'uncheckpointed WAL row survived backup/restore' || fail 'uncheckpointed WAL row survived backup/restore'

# --- 13. crafted-mode archives: unsafe modes refused ------------------------
mkdir -p "$TMP/modebad"
tar -xzf "$wal_archive" -C "$TMP/modebad"
chmod 4755 "$TMP/modebad/servers/s1/file.txt"
tar -czf "$TMP/setuid.tar.gz" -C "$TMP/modebad" .
if bash scripts/manage-panel.sh restore "$TMP/setuid.tar.gz" --force >/dev/null 2>&1; then
  fail 'setuid archive member refused'
else
  ok 'setuid archive member refused'
fi
chmod 0666 "$TMP/modebad/servers/s1/file.txt"
tar -czf "$TMP/worldwritable.tar.gz" -C "$TMP/modebad" .
if bash scripts/manage-panel.sh restore "$TMP/worldwritable.tar.gz" --force >/dev/null 2>&1; then
  fail 'world-writable archive member refused'
else
  ok 'world-writable archive member refused'
fi

# --- 14. archive member missing from manifest checksums refused -------------
echo 'sneaked in' > "$TMP/modebad/servers/s1/unlisted.txt"
tar -czf "$TMP/unlisted.tar.gz" -C "$TMP/modebad" .
if bash scripts/manage-panel.sh restore "$TMP/unlisted.tar.gz" --force >/dev/null 2>&1; then
  fail 'archive member missing from manifest checksums refused'
else
  ok 'archive member missing from manifest checksums refused'
fi

# --- 15. symlink in backup tree fails backup --------------------------------
ln -s /etc/passwd "$DATA/servers/s1/evil-link"
if bash scripts/manage-panel.sh backup >/dev/null 2>&1; then
  fail 'backup with symlink refused'
else
  ok 'backup with symlink refused'
fi
rm -f "$DATA/servers/s1/evil-link"

echo
if (( FAILS == 0 )); then
  echo "ALL TESTS PASSED"
  exit 0
else
  echo "$FAILS TEST(S) FAILED"
  exit 1
fi

