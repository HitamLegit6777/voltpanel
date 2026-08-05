#!/usr/bin/env bash
set -Eeuo pipefail
REPO=${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}
arch(){ case "$(uname -m)" in x86_64|amd64) echo linux-amd64;; aarch64|arm64) echo linux-arm64;; *) echo "unsupported architecture" >&2; exit 1;; esac; }
require_root(){ [[ $EUID -eq 0 ]] || { echo "Run as root." >&2; exit 1; }; }
case ${1:-help} in
  status) systemctl status voltd --no-pager;;
  logs) journalctl -u voltd -f;;
  upgrade) require_root; tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT; curl -fL "https://github.com/$REPO/releases/latest/download/voltd-$(arch)" -o "$tmp"; chmod +x "$tmp"; systemctl stop voltd; install -m755 "$tmp" /usr/local/bin/voltd; systemctl start voltd; systemctl --no-pager --full status voltd;;
  doctor) command -v bwrap; command -v setpriv; command -v nft; command -v ip; test -f /sys/fs/cgroup/cgroup.controllers; systemctl is-active voltd; stat -c '%a %n' /etc/voltpanel-node/voltd.toml; journalctl -u voltd -n 30 --no-pager;;
  uninstall) require_root; [[ ${2:-} == --purge ]] || { echo "Use: $0 uninstall --purge"; exit 1; }; systemctl disable --now voltd || true; rm -f /etc/systemd/system/voltd.service /usr/local/bin/voltd /usr/local/sbin/voltd-manage; rm -rf /etc/voltpanel-node /var/lib/voltd; systemctl daemon-reload;;
  *) echo "Usage: $0 {status|logs|upgrade|doctor|uninstall --purge}";;
esac
