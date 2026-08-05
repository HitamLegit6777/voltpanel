#!/usr/bin/env bash
set -Eeuo pipefail
REPO=${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}
DATA_DIR=${VOLTPANEL_DATA_DIR:-/var/lib/voltpanel}
BACKUP_DIR=${VOLTPANEL_BACKUP_DIR:-/var/backups/voltpanel}
arch(){ case "$(uname -m)" in x86_64|amd64) echo linux-amd64;; aarch64|arm64) echo linux-arm64;; *) echo "unsupported architecture" >&2; exit 1;; esac; }
require_root(){ [[ $EUID -eq 0 ]] || { echo "Run as root." >&2; exit 1; }; }
case ${1:-help} in
  status) systemctl status voltpanel --no-pager; curl -fsS http://127.0.0.1:8080/api/system/health || true;;
  logs) journalctl -u voltpanel -f;;
  backup) require_root; install -d -m700 "$BACKUP_DIR"; sqlite3 "$DATA_DIR/voltpanel.db" ".backup '$BACKUP_DIR/panel-$(date +%F-%H%M%S).db'"; tar -C "$DATA_DIR" -czf "$BACKUP_DIR/files-$(date +%F-%H%M%S).tar.gz" servers backups eggs websites; echo "Backup stored in $BACKUP_DIR";;
  upgrade) require_root; tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT; curl -fL "https://github.com/$REPO/releases/latest/download/voltpanel-$(arch)" -o "$tmp"; chmod +x "$tmp"; "$0" backup; systemctl stop voltpanel; install -m755 "$tmp" /usr/local/bin/voltpanel; systemctl start voltpanel; systemctl --no-pager --full status voltpanel;;
  doctor) command -v bwrap; command -v setpriv; command -v nft; command -v ip; test -f /sys/fs/cgroup/cgroup.controllers; systemctl is-active voltpanel; stat -c '%a %n' /etc/voltpanel/config.toml "$DATA_DIR/voltpanel.db"; journalctl -u voltpanel -n 30 --no-pager;;
  uninstall) require_root; [[ ${2:-} == --purge ]] || { echo "Use: $0 uninstall --purge"; exit 1; }; systemctl disable --now voltpanel || true; rm -f /etc/systemd/system/voltpanel.service /usr/local/bin/voltpanel /usr/local/sbin/voltpanel-manage; rm -rf /etc/voltpanel "$DATA_DIR"; systemctl daemon-reload;;
  *) echo "Usage: $0 {status|logs|backup|upgrade|doctor|uninstall --purge}";;
esac
