#!/usr/bin/env bash
set -Eeuo pipefail
REPO=${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}
arch(){ case "$(uname -m)" in x86_64|amd64) echo linux-amd64;; aarch64|arm64) echo linux-arm64;; *) echo "unsupported architecture" >&2; exit 1;; esac; }
require_root(){ [[ $EUID -eq 0 ]] || { echo "Run as root." >&2; exit 1; }; }
doctor(){ local failures=0 cmd; for cmd in bwrap setpriv nft ip; do if command -v "$cmd" >/dev/null; then echo "ok: $cmd"; else echo "missing: $cmd"; failures=$((failures+1)); fi; done; if [[ -f /sys/fs/cgroup/cgroup.controllers ]]; then echo "ok: cgroup v2"; else echo "missing: cgroup v2"; failures=$((failures+1)); fi; if systemctl is-active --quiet voltd; then echo "ok: service"; else echo "failed: service"; failures=$((failures+1)); fi; stat -c '%a %n' /etc/voltpanel-node/voltd.toml 2>/dev/null || failures=$((failures+1)); journalctl -u voltd -n 30 --no-pager || true; return "$failures"; }
case ${1:-help} in
  status) systemctl status voltd --no-pager || true;;
  logs) journalctl -u voltd -f;;
  upgrade) require_root; tmp=$(mktemp); sums=$(mktemp); trap 'rm -f "$tmp" "$sums"' EXIT; asset="voltd-$(arch)"; base="https://github.com/$REPO/releases/latest/download"; curl -fL "$base/$asset" -o "$tmp"; curl -fL "$base/SHA256SUMS" -o "$sums"; expected=$(awk -v a="$asset" '$2==a{print $1}' "$sums"); actual=$(sha256sum "$tmp"|awk '{print $1}'); [[ -n "$expected" && "$expected" == "$actual" ]] || { echo "checksum mismatch" >&2; exit 1; }; chmod +x "$tmp"; systemctl stop voltd; install -m755 "$tmp" /usr/local/bin/voltd; systemctl start voltd; systemctl --no-pager --full status voltd;;
  doctor) doctor;;
  uninstall) require_root; [[ ${2:-} == --purge ]] || { echo "Use: $0 uninstall --purge"; exit 1; }; systemctl disable --now voltd || true; rm -f /etc/systemd/system/voltd.service /usr/local/bin/voltd /usr/local/sbin/voltd-manage; rm -rf /etc/voltpanel-node /var/lib/voltd; systemctl daemon-reload;;
  *) echo "Usage: $0 {status|logs|upgrade|doctor|uninstall --purge}";;
esac
