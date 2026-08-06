#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]:-$0}
SCRIPT_DIR=$(cd -- "$(dirname -- "$SCRIPT_PATH")" && pwd)
if [[ -f "$SCRIPT_DIR/lib/common.sh" ]]; then
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/lib/common.sh"
else
  TMP_COMMON=$(mktemp); trap 'rm -f "$TMP_COMMON"' EXIT
  curl -fsSL "${VOLTPANEL_RAW:-https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main}/scripts/lib/common.sh" -o "$TMP_COMMON"
  # shellcheck disable=SC1090
  source "$TMP_COMMON"
fi

PANEL_URL=""; TOKEN=""; PUBLIC_URL=""; DOMAIN=""; IP_ADDRESS=""; EMAIL=""; TLS_MODE=""; CF_CERT=""; CF_KEY=""; LISTEN=""; DATA_DIR=/var/lib/voltd; CONFIG_DIR=/etc/voltpanel-node; ALLOW_HTTP=0; INTERACTIVE=auto
ARG_COUNT=$#
while (($#)); do
  case "$1" in
    --panel) PANEL_URL=${2:?}; shift 2;;
    --token) TOKEN=${2:?}; shift 2;;
    --public-url) PUBLIC_URL=${2:?}; shift 2;;
    --domain) DOMAIN=${2:?}; shift 2;;
    --ip-address) IP_ADDRESS=${2:?}; shift 2;;
    --email) EMAIL=${2:?}; shift 2;;
    --tls) TLS_MODE=${2:?}; shift 2;;
    --cloudflare-cert) CF_CERT=${2:?}; shift 2;;
    --cloudflare-key) CF_KEY=${2:?}; shift 2;;
    --listen) LISTEN=${2:?}; shift 2;;
    --data-dir) DATA_DIR=${2:?}; shift 2;;
    --allow-http) ALLOW_HTTP=1; shift;;
    --no-caddy) TLS_MODE=none; shift;;
    --non-interactive) INTERACTIVE=0; shift;;
    --version) VOLTPANEL_VERSION=${2:?}; shift 2;;
    --dry-run) DRY_RUN=1; shift;;
    --help|-h) cat <<'EOF'
VoltPanel execution agent installer

Usage: sudo ./install-node.sh [options]
  --panel URL                 Panel URL
  --token TOKEN               One-time enrollment token
  --domain agent.example.com  Public node domain
  --tls MODE                  caddy, certbot, certbot-ip, cloudflare, or none
  --ip-address IP             Public IPv4 or IPv6 for certbot-ip
  --cloudflare-cert PATH      Cloudflare Origin Certificate PEM
  --cloudflare-key PATH       Cloudflare Origin private key
  --public-url URL            Explicit node URL stored in the panel
  --listen ADDRESS            Agent listen address
  --allow-http                Permit plain HTTP enrollment on trusted LAN
  --no-caddy                  Alias for --tls none
  --non-interactive           Disable the terminal wizard
  --data-dir PATH             Node data directory (default /var/lib/voltd)
  --version VERSION           Release tag (default latest)
  --dry-run                   Print actions without modifying the host
EOF
      exit 0;;
    *) die "Unknown argument: $1";;
  esac
done
export VOLTPANEL_VERSION

if [[ "$INTERACTIVE" == auto && "$ARG_COUNT" == 0 ]] && tui_available; then INTERACTIVE=1; else INTERACTIVE=0; fi
if [[ "$INTERACTIVE" == 1 ]]; then
  tui_title "VoltPanel Node Installer"
  PANEL_URL=$(tui_input "Panel URL" "$PANEL_URL")
  TOKEN=$(tui_secret "Enrollment token")
  TLS_MODE=$(tui_menu "Choose node HTTPS mode" "caddy" \
    caddy "Caddy automatic HTTPS (recommended)" \
    certbot "Certbot + Nginx with a domain" \
    certbot-ip "Certbot + Nginx with a public IP" \
    cloudflare "Cloudflare Origin Certificate" \
    none "No reverse proxy / trusted LAN only")
  if [[ "$TLS_MODE" != none ]]; then
    if [[ "$TLS_MODE" == certbot-ip ]]; then
      tui_note "IP certificates require Certbot 5.4+, are valid for about 6 days, and must renew automatically."
      IP_ADDRESS=$(tui_input "Public node IP address" "$IP_ADDRESS")
    else
      DOMAIN=$(tui_input "Node domain" "$DOMAIN")
    fi
    if [[ "$TLS_MODE" == caddy || "$TLS_MODE" == certbot || "$TLS_MODE" == certbot-ip ]]; then EMAIL=$(tui_input "ACME email (optional)" "$EMAIL"); fi
    if [[ "$TLS_MODE" == cloudflare ]]; then
      tui_note "Create an Origin Certificate in Cloudflare and set this hostname to Full (strict)."
      CF_CERT=$(tui_input "Origin Certificate PEM path" "$CF_CERT")
      CF_KEY=$(tui_input "Origin private key path" "$CF_KEY")
    fi
  else
    ALLOW_HTTP=1
  fi
  DATA_DIR=$(tui_input "Node data directory" "$DATA_DIR")
  printf '\n  Panel:    %s\n  TLS mode: %s\n  Address:  %s\n  Data:     %s\n' "$PANEL_URL" "$TLS_MODE" "${DOMAIN:-${IP_ADDRESS:-(none)}}" "$DATA_DIR" > /dev/tty
  tui_pause
fi

TLS_MODE=${TLS_MODE:-$([[ -n "$DOMAIN" ]] && printf caddy || printf none)}
case "$TLS_MODE" in caddy|certbot|certbot-ip|cloudflare|none) ;; *) die "Invalid --tls mode: $TLS_MODE";; esac
[[ "$TLS_MODE" == none || "$TLS_MODE" == certbot-ip || -n "$DOMAIN" ]] || die "--domain is required for TLS mode $TLS_MODE"
[[ "$TLS_MODE" != certbot-ip || -n "$IP_ADDRESS" ]] || die "certbot-ip mode requires --ip-address"
[[ "$TLS_MODE" != cloudflare || (-n "$CF_CERT" && -n "$CF_KEY") ]] || die "Cloudflare mode requires --cloudflare-cert and --cloudflare-key"

require_root; require_systemd; load_os
[[ -n "$PANEL_URL" ]] || die "--panel is required"
[[ -n "$TOKEN" ]] || die "--token is required"
validate_url "$PANEL_URL"
[[ -z "$DOMAIN" ]] || validate_domain "$DOMAIN"; [[ -z "$IP_ADDRESS" ]] || validate_ip "$IP_ADDRESS"

if [[ "$TLS_MODE" != none ]]; then
  LISTEN=${LISTEN:-127.0.0.1:8081}
  PUBLIC_URL=${PUBLIC_URL:-https://${DOMAIN:-$IP_ADDRESS}}
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
Description=VoltPanel execution agent
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
ReadWritePaths=$DATA_DIR $CONFIG_DIR /run/voltpanel /sys/fs/cgroup
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

case "$TLS_MODE" in
  caddy)
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
    configure_caddy_import
    run systemctl enable --now caddy
    run caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
    run systemctl reload caddy
    ;;
  certbot) configure_certbot_proxy node "$DOMAIN" 127.0.0.1:8081 "$EMAIL" ;;
  certbot-ip) configure_certbot_ip_proxy node "$IP_ADDRESS" 127.0.0.1:8081 "$EMAIL" ;;
  cloudflare) configure_cloudflare_proxy node "$DOMAIN" 127.0.0.1:8081 "$CF_CERT" "$CF_KEY" ;;
esac

systemctl_reload_start voltd
firewall_hint node
ok "VoltPanel node installed and enrolled"
printf '\n  Panel:      %s\n  Node URL:   %s\n  Config:     %s/voltd.toml\n  Data:       %s\n\n' "$PANEL_URL" "$PUBLIC_URL" "$CONFIG_DIR" "$DATA_DIR"
log "Run 'voltd-manage doctor' for diagnostics."
