#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
if [[ -f "$SCRIPT_DIR/lib/common.sh" ]]; then
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/lib/common.sh"
else
  TMP_COMMON=$(mktemp); trap 'rm -f "$TMP_COMMON"' EXIT
  curl -fsSL "${VOLTPANEL_RAW:-https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main}/scripts/lib/common.sh" -o "$TMP_COMMON"
  # shellcheck disable=SC1090
  source "$TMP_COMMON"
fi

PANEL_URL=""; TOKEN=""; PUBLIC_URL=""; DOMAIN=""; EMAIL=""; LISTEN=""; DATA_DIR=/var/lib/voltd; CONFIG_DIR=/etc/voltpanel-node; ALLOW_HTTP=0; NO_CADDY=0
while (($#)); do
  case "$1" in
    --panel) PANEL_URL=${2:?}; shift 2;;
    --token) TOKEN=${2:?}; shift 2;;
    --public-url) PUBLIC_URL=${2:?}; shift 2;;
    --domain) DOMAIN=${2:?}; shift 2;;
    --email) EMAIL=${2:?}; shift 2;;
    --listen) LISTEN=${2:?}; shift 2;;
    --data-dir) DATA_DIR=${2:?}; shift 2;;
    --allow-http) ALLOW_HTTP=1; shift;;
    --no-caddy) NO_CADDY=1; shift;;
    --version) VOLTPANEL_VERSION=${2:?}; shift 2;;
    --dry-run) DRY_RUN=1; shift;;
    --help|-h) cat <<'EOF'
VoltPanel node installer

Usage: sudo ./install-node.sh --panel URL --token TOKEN [options]
  --domain node.example.com   HTTPS node endpoint through Caddy
  --public-url URL            Explicit URL stored in the panel
  --listen ADDRESS            Daemon listen address
  --allow-http                Permit plain HTTP enrollment on trusted LAN
  --no-caddy                  Do not configure Caddy
  --data-dir PATH             Node data directory (default /var/lib/voltd)
  --version VERSION           Release tag (default latest)
  --dry-run                   Print actions without modifying the host
EOF
      exit 0;;
    *) die "Unknown argument: $1";;
  esac
done
export VOLTPANEL_VERSION

require_root; require_systemd; load_os
[[ -n "$PANEL_URL" ]] || die "--panel is required"
[[ -n "$TOKEN" ]] || die "--token is required"
validate_url "$PANEL_URL"
[[ -z "$DOMAIN" ]] || validate_domain "$DOMAIN"

if [[ -n "$DOMAIN" && "$NO_CADDY" == 0 ]]; then
  LISTEN=${LISTEN:-127.0.0.1:8081}
  PUBLIC_URL=${PUBLIC_URL:-https://$DOMAIN}
else
  LISTEN=${LISTEN:-0.0.0.0:8081}
  if [[ -z "$PUBLIC_URL" ]]; then
    IP=$(ip -4 route get 1.1.1.1 | awk '{for(i=1;i<=NF;i++)if($i=="src"){print $(i+1);exit}}')
    PUBLIC_URL="http://${IP:-127.0.0.1}:${LISTEN##*:}"
  fi
fi
validate_url "$PUBLIC_URL"

if [[ $PANEL_URL != https://* && "$PANEL_URL" != http://127.0.0.1* && "$PANEL_URL" != http://localhost* && "$ALLOW_HTTP" != 1 ]]; then
  die "Panel enrollment must use HTTPS. On a trusted private LAN pass --allow-http explicitly."
fi

install_packages
install_binary voltd
run install -d -m 0700 "$DATA_DIR" "$DATA_DIR/servers" "$DATA_DIR/logs" "$DATA_DIR/meta" "$CONFIG_DIR"

JOIN_ARGS=(join "$PANEL_URL" "$TOKEN" --public-url "$PUBLIC_URL" --listen "$LISTEN" --data "$DATA_DIR" --config "$CONFIG_DIR/voltd.toml" --no-start)
[[ "$ALLOW_HTTP" == 1 ]] && JOIN_ARGS+=(--allow-http)
if [[ "$DRY_RUN" == 1 ]]; then log "[dry-run] /usr/local/bin/voltd ${JOIN_ARGS[*]}"; else /usr/local/bin/voltd "${JOIN_ARGS[@]}"; fi

write_file /etc/systemd/system/voltd.service 0644 <<EOF
[Unit]
Description=VoltPanel node daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
Group=root
WorkingDirectory=$DATA_DIR
Environment=VOLTD_CONFIG=$CONFIG_DIR/voltd.toml
Environment=RUST_LOG=info
ExecStart=/usr/local/bin/voltd serve --config $CONFIG_DIR/voltd.toml
Restart=on-failure
RestartSec=3
TimeoutStopSec=45
UMask=0077
Delegate=yes
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
ReadWritePaths=$DATA_DIR /run/voltpanel /sys/fs/cgroup
CapabilityBoundingSet=CAP_CHOWN CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_ADMIN CAP_NET_ADMIN CAP_NET_RAW CAP_KILL
AmbientCapabilities=CAP_CHOWN CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_ADMIN CAP_NET_ADMIN CAP_NET_RAW CAP_KILL
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
LockPersonality=yes

[Install]
WantedBy=multi-user.target
EOF

if [[ -f "$SCRIPT_DIR/manage-node.sh" ]]; then run install -m0755 "$SCRIPT_DIR/manage-node.sh" /usr/local/sbin/voltd-manage
elif [[ "$DRY_RUN" == 1 ]]; then log "[dry-run] download voltd-manage"
else curl -fsSL "$VOLTPANEL_RAW/scripts/manage-node.sh" -o /usr/local/sbin/voltd-manage; chmod 0755 /usr/local/sbin/voltd-manage
fi

if [[ -n "$DOMAIN" && "$NO_CADDY" == 0 ]]; then
  install_caddy
  TLS_LINE=""; [[ -z "$EMAIL" ]] || TLS_LINE="    tls $EMAIL"
  write_file /etc/caddy/conf.d/voltpanel-node.caddy 0644 <<EOF
$DOMAIN {
$TLS_LINE
    encode zstd gzip
    reverse_proxy 127.0.0.1:8081
    header {
        Strict-Transport-Security "max-age=31536000; includeSubDomains; preload"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
    }
}
EOF
  if [[ "$DRY_RUN" == 1 ]]; then log "[dry-run] ensure Caddyfile imports /etc/caddy/conf.d/*.caddy"
  else
    install -d -m755 /etc/caddy/conf.d
    touch /etc/caddy/Caddyfile
    grep -Fq 'import /etc/caddy/conf.d/*.caddy' /etc/caddy/Caddyfile || { cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.pre-voltpanel; printf '\nimport /etc/caddy/conf.d/*.caddy\n' >> /etc/caddy/Caddyfile; }
  fi
  run systemctl enable --now caddy
  run systemctl reload caddy
fi

systemctl_reload_start voltd
firewall_hint node
ok "VoltPanel node installed and enrolled"
printf '\n  Panel:      %s\n  Node URL:   %s\n  Config:     %s/voltd.toml\n  Data:       %s\n\n' "$PANEL_URL" "$PUBLIC_URL" "$CONFIG_DIR" "$DATA_DIR"
log "Run 'voltd-manage doctor' for diagnostics."
