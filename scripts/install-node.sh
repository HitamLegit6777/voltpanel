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

PANEL_URL=""; TOKEN=${VOLTD_TOKEN:-}; PUBLIC_URL=""; DOMAIN=""; IP_ADDRESS=""; EMAIL=""; TLS_MODE=""; CF_CERT=""; CF_KEY=""; PORT=8081; PORT_SET=0; LISTEN=""; DATA_DIR=/var/lib/voltd; CONFIG_DIR=/etc/voltpanel-node; ALLOW_HTTP=0; OUTBOUND_ONLY=0; ENABLE_ADMIN_TERMINAL=0; INTERACTIVE=auto
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
    --port) PORT=${2:?}; PORT_SET=1; shift 2;;
    --listen) LISTEN=${2:?}; shift 2;;
    --data-dir) DATA_DIR=${2:?}; shift 2;;
    --allow-http) ALLOW_HTTP=1; shift;;
    --outbound-only) OUTBOUND_ONLY=1; shift;;
    --enable-admin-terminal) ENABLE_ADMIN_TERMINAL=1; shift;;
    --no-caddy) TLS_MODE=none; shift;;
    --non-interactive) INTERACTIVE=0; shift;;
    --version) VOLTPANEL_VERSION=${2:?}; shift 2;;
    --dry-run) DRY_RUN=1; shift;;
    --help|-h) cat <<'EOF'
VoltPanel execution agent installer

Usage: sudo ./install-node.sh [options]
  --panel URL                 Panel URL
  --token TOKEN               One-time enrollment token
  --outbound-only              Outbound command channel only; no public node endpoint
  --enable-admin-terminal      Allow audited root troubleshooting commands from panel admins
  --domain agent.example.com  Public node domain
  --tls MODE                  caddy, certbot, certbot-ip, cloudflare, or none
  --ip-address IP             Public IPv4 or IPv6 for certbot-ip
  --cloudflare-cert PATH      Cloudflare Origin Certificate PEM
  --cloudflare-key PATH       Cloudflare Origin private key
  --public-url URL            Explicit node URL stored in the panel
  --port PORT                 Internal/direct node port (default 8081)
  --listen ADDRESS            Agent listen address (mutually exclusive with --port; plaintext defaults to 127.0.0.1:PORT, 0.0.0.0:PORT with --allow-http)
  --allow-http                Permit --tls none only for loopback-local development (the panel refuses plaintext enrollment)
  --no-caddy                  Alias for --tls none
  --non-interactive           Disable the terminal wizard
  --data-dir PATH             Node data directory (default /var/lib/voltd)
  --version VERSION           Release tag (default latest)
  --dry-run                   Print actions without modifying the host

  Proxied TLS modes (caddy, certbot, certbot-ip, cloudflare) keep the agent
  on a plaintext loopback origin behind the TLS-terminating proxy, so the
  agent cannot self-present the endpoint certificate the panel dials. For
  those deployments, seed the node's expected_fingerprint (the strict
  64-hex SHA-256 fingerprint of the endpoint certificate) via the panel
  UI/API before the first enrollment; the first enrollment and every
  re-enrollment must then present exactly that fingerprint.
EOF
      exit 0;;
    *) die "Unknown argument: $1";;
  esac
done
if [[ -n "$PANEL_URL" && "$DRY_RUN" != 1 ]]; then
  PANEL_META=$(curl -fsSL --connect-timeout 15 "${PANEL_URL%/}/api/meta") \
    || die "Cannot read panel capability manifest"
  PANEL_VERSION=$(printf '%s' "$PANEL_META" | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)
  [[ -n "$PANEL_VERSION" ]] || die "Panel returned invalid metadata JSON"
  if [[ "$OUTBOUND_ONLY" == 1 ]]; then
    printf '%s' "$PANEL_META" | grep -Eq '"outbound_channel"[[:space:]]*:[[:space:]]*true' \
      || die "Panel does not advertise outbound node channels"
    if [[ "$ENABLE_ADMIN_TERMINAL" == 1 ]]; then
      printf '%s' "$PANEL_META" | grep -Eq '"admin_terminal"[[:space:]]*:[[:space:]]*true' \
        || die "Panel does not support the admin terminal protocol"
    fi
  fi
  if [[ "${VOLTPANEL_VERSION:-latest}" == latest ]]; then
    VOLTPANEL_VERSION="v${PANEL_VERSION#v}"
  fi
fi
export VOLTPANEL_VERSION
resolve_release_tag
refresh_raw_base

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
    none "No TLS — loopback-local dev only (panel refuses plaintext enrollment)")
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
  PORT=$(tui_input "Node port" "$PORT")
  DATA_DIR=$(tui_input "Node data directory" "$DATA_DIR")
  printf '\n  Panel:    %s\n  TLS mode: %s\n  Address:  %s\n  Port:     %s\n  Data:     %s\n' "$PANEL_URL" "$TLS_MODE" "${DOMAIN:-${IP_ADDRESS:-(none)}}" "$PORT" "$DATA_DIR" > /dev/tty
  tui_pause
fi

if [[ "$OUTBOUND_ONLY" == 1 ]]; then
  TLS_MODE=none
  LISTEN=127.0.0.1:8081
  PUBLIC_URL=""
fi
TLS_MODE=${TLS_MODE:-$([[ -n "$DOMAIN" ]] && printf caddy || printf none)}
case "$TLS_MODE" in caddy|certbot|certbot-ip|cloudflare|none) ;; *) die "Invalid --tls mode: $TLS_MODE";; esac
[[ "$TLS_MODE" == none || "$TLS_MODE" == certbot-ip || -n "$DOMAIN" ]] || die "--domain is required for TLS mode $TLS_MODE"
[[ "$TLS_MODE" != certbot-ip || -n "$IP_ADDRESS" ]] || die "certbot-ip mode requires --ip-address"
[[ "$TLS_MODE" != cloudflare || (-n "$CF_CERT" && -n "$CF_KEY") ]] || die "Cloudflare mode requires --cloudflare-cert and --cloudflare-key"
validate_port "$PORT"
[[ -z "$LISTEN" || "$PORT_SET" != 1 ]] || die "Use either --listen or --port, not both"

if [[ "$DRY_RUN" != 1 ]]; then require_root; require_systemd; fi
load_os
[[ -n "$PANEL_URL" ]] || die "--panel is required"
[[ -n "$TOKEN" || "$DRY_RUN" == 1 ]] || die "--token is required"
validate_url "$PANEL_URL"
[[ -z "$DOMAIN" ]] || validate_domain "$DOMAIN"; [[ -z "$IP_ADDRESS" ]] || validate_ip "$IP_ADDRESS"
[[ "$TLS_MODE" != cloudflare || -r "$CF_CERT" ]] || die "Cloudflare Origin Certificate not readable: $CF_CERT"
[[ "$TLS_MODE" != cloudflare || -r "$CF_KEY" ]] || die "Cloudflare Origin private key not readable: $CF_KEY"

if [[ "$OUTBOUND_ONLY" == 1 ]]; then
  [[ "$PANEL_URL" == https://* || "$PANEL_URL" =~ ^http://(127\.0\.0\.1|\[::1\]|localhost)([:/]|$) ]] \
    || die "Outbound-only agent enrollment requires an HTTPS panel URL"
fi
# Plaintext enrollment is refused by the panel: the enrollment endpoint
# requires positively-TLS transport (403 otherwise) and a presented
# certificate fingerprint (400 without one), so a plaintext agent cannot
# enroll. The v16 operator-seeded expected_fingerprint path lets an operator
# declare the endpoint certificate fingerprint for proxy-fronted agents (see
# --help), but a plaintext agent still presents no fingerprint of its own, so
# --tls none remains meaningful only for loopback-local development, and only
# with --allow-http plus an explicit warning.
if [[ "$TLS_MODE" == none && "$OUTBOUND_ONLY" != 1 ]]; then
  if [[ "$ALLOW_HTTP" != 1 || ! "$PANEL_URL" =~ ^http://(127\.0\.0\.1|\[::1\]|localhost)([:/]|$) ]]; then
    die "Plaintext inbound enrollment is only supported for loopback development; use --outbound-only for production agents."
  fi
fi

if [[ "$TLS_MODE" != none ]]; then
  LISTEN=${LISTEN:-127.0.0.1:$PORT}
  PUBLIC_URL=${PUBLIC_URL:-https://${DOMAIN:-$(host_for_url "$IP_ADDRESS")}}
else
  # Plaintext agent API: default to loopback-only. Exposing it on every
  # interface is a deliberate choice that needs --listen plus the
  # --allow-http opt-in (enforced below and by `voltd join`).
  if [[ -z "$LISTEN" ]]; then
    if [[ "$ALLOW_HTTP" == 1 ]]; then LISTEN="0.0.0.0:$PORT"; else LISTEN="127.0.0.1:$PORT"; fi
  fi
  if [[ -z "$PUBLIC_URL" && "$OUTBOUND_ONLY" != 1 ]]; then
    PUBLIC_URL="http://127.0.0.1:${LISTEN##*:}"
  fi
fi
if [[ "$TLS_MODE" == none && "$LISTEN" != 127.0.0.1:* && "$LISTEN" != \[::1\]:* ]]; then
  if [[ "$ALLOW_HTTP" != 1 ]]; then
    die "Refusing to expose the plaintext agent API on all interfaces ($LISTEN). Use --listen 127.0.0.1:$PORT for loopback-only, or pass --allow-http to explicitly opt in to a non-loopback plaintext bind on a trusted network."
  fi
  warn "Plaintext agent API on $LISTEN: control traffic is unencrypted and the panel cannot pin a certificate fingerprint. Only use this on a trusted network."
fi
if [[ "$TLS_MODE" != none && "$LISTEN" != 127.0.0.1:* && "$LISTEN" != \[::1\]:* ]]; then
  die "TLS proxy origin must listen on loopback (127.0.0.1 or [::1])"
fi
[[ "$OUTBOUND_ONLY" == 1 || -n "$PUBLIC_URL" ]] || die "node public URL is required outside outbound-only mode"
[[ "$OUTBOUND_ONLY" == 1 ]] || validate_url "$PUBLIC_URL"

if [[ "$PANEL_URL" != https://* && "$ALLOW_HTTP" != 1 ]]; then
  if [[ "$PANEL_URL" =~ ^http://(127\.0\.0\.1|\[::1\]|localhost)([:/]|$) ]]; then :; else
    die "Panel enrollment must use HTTPS. On a trusted private LAN pass --allow-http explicitly."
  fi
fi

install_packages
if [[ "$OUTBOUND_ONLY" == 1 && "$DRY_RUN" != 1 ]]; then
  PANEL_AGENT=$(mktemp)
  PANEL_HEADERS=$(mktemp)
  ARCH_HEADER=$(arch_asset)
  curl -fsSL --connect-timeout 15 -D "$PANEL_HEADERS" \
    -H "x-volt-enrollment-token: $TOKEN" -H "x-volt-arch: $ARCH_HEADER" \
    "${PANEL_URL%/}/api/system/agent-binary" -o "$PANEL_AGENT" \
    || { rm -f "$PANEL_AGENT" "$PANEL_HEADERS"; die "Compatible agent download from panel failed"; }
  EXPECTED=$(sed -n 's/^x-volt-sha256:[[:space:]]*//Ip' "$PANEL_HEADERS" | tr -d '\r' | head -n1)
  ACTUAL=$(sha256sum "$PANEL_AGENT" | awk '{print $1}')
  [[ -n "$EXPECTED" && "$ACTUAL" == "$EXPECTED" ]] \
    || { rm -f "$PANEL_AGENT" "$PANEL_HEADERS"; die "Panel agent checksum mismatch"; }
  chmod 0755 "$PANEL_AGENT"
  OUTPUT=$(timeout 5 "$PANEL_AGENT" --version) || die "Panel agent binary failed its identity check"
  [[ "$OUTPUT" == voltd\ * ]] || die "Panel returned an incompatible agent binary"
  run install -m0755 "$PANEL_AGENT" /usr/local/bin/voltd
  rm -f "$PANEL_AGENT" "$PANEL_HEADERS"
else
  install_binary voltd
fi
run install -d -m 0700 "$DATA_DIR" "$DATA_DIR/servers" "$DATA_DIR/logs" "$DATA_DIR/meta" "$CONFIG_DIR"

ROLLBACK_ENABLED=0
NODE_EXISTING=0; [[ -f "$CONFIG_DIR/voltd.toml" ]] && NODE_EXISTING=1
rollback_node() {
  local rc=$?
  [[ -n "${TMP_COMMON:-}" ]] && rm -f "$TMP_COMMON"
  [[ "$ROLLBACK_ENABLED" == 1 ]] || return 0
  warn "Enrollment succeeded but later steps failed; removing the partial install. Re-run with a fresh enrollment token."
  run rm -f "$CONFIG_DIR/voltd.toml" /etc/systemd/system/voltd.service /usr/local/sbin/voltd-manage /usr/share/voltpanel-node/common.sh
  if systemctl is-active --quiet voltd 2>/dev/null; then run systemctl disable --now voltd >/dev/null 2>&1 || true; fi
  run systemctl daemon-reload
  cleanup_proxy_artifacts node
  exit "$rc"
}

# The enrollment token is sensitive: hand it to `voltd join` via VOLTD_TOKEN
# so it never appears in argv (process listings, audit logs), only in the
# child's environment. `voltd join` accepts the env fallback when argv has no
# token (src/bin/voltd.rs).
JOIN_ARGS=(join "$PANEL_URL" --public-url "$PUBLIC_URL" --listen "$LISTEN" --data "$DATA_DIR" --config "$CONFIG_DIR/voltd.toml" --no-start --plaintext)
[[ "$ENABLE_ADMIN_TERMINAL" == 1 ]] && JOIN_ARGS+=(--enable-admin-terminal)
# --plaintext is passed only when the agent itself will serve plaintext. The
# agent serves plaintext whenever it terminates no TLS of its own — which is
# every mode this installer supports: --tls none binds raw http, and the
# proxied TLS modes (caddy/certbot/certbot-ip/cloudflare) keep the agent as a
# loopback http origin behind the TLS-terminating proxy (the proxy templates
# dial http://). A future direct-TLS mode (the agent terminates TLS itself)
# must NOT pass --plaintext.
if [[ "$OUTBOUND_ONLY" != 1 ]]; then
  case "$TLS_MODE" in none|caddy|certbot|certbot-ip|cloudflare) JOIN_ARGS+=(--plaintext) ;; esac
fi
[[ "$ALLOW_HTTP" == 1 ]] && JOIN_ARGS+=(--allow-http)
trap 'rollback_node' EXIT
if [[ "$DRY_RUN" == 1 ]]; then
  log "[dry-run] enroll: VOLTD_TOKEN=<redacted> /usr/local/bin/voltd ${JOIN_ARGS[*]}"
  log "[dry-run] validate $CONFIG_DIR/voltd.toml with voltd"
else
  VOLTD_TOKEN="$TOKEN" /usr/local/bin/voltd "${JOIN_ARGS[@]}"
  [[ "$NODE_EXISTING" == 0 ]] && ROLLBACK_ENABLED=1
  /usr/local/bin/voltd check-config --config "$CONFIG_DIR/voltd.toml"
fi

write_file /etc/systemd/system/voltd.service 0644 <<EOF
[Unit]
Description=VoltPanel execution agent
Documentation=https://github.com/HitamLegit6777/voltpanel
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=5

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
RuntimeDirectory=voltpanel
RuntimeDirectoryMode=0750
Delegate=yes
NoNewPrivileges=yes
PrivateDevices=yes
PrivateTmp=yes
ProtectHome=yes
ProtectKernelModules=yes
ProtectSystem=strict
ReadWritePaths=$DATA_DIR $CONFIG_DIR /run/voltpanel /sys/fs/cgroup
# CAP_SYS_ADMIN is REQUIRED: bwrap needs it to mount the sandbox filesystem
# (tmpfs /dev, bind mounts, netns/cgroup setup). Do not remove it.
CapabilityBoundingSet=CAP_CHOWN CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_ADMIN CAP_NET_ADMIN CAP_NET_RAW CAP_KILL
AmbientCapabilities=CAP_CHOWN CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_ADMIN CAP_NET_ADMIN CAP_NET_RAW CAP_KILL
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
EOF

if [[ -f "$SCRIPT_DIR/manage-node.sh" ]]; then run install -m0755 "$SCRIPT_DIR/manage-node.sh" /usr/local/sbin/voltd-manage
elif [[ "$DRY_RUN" == 1 ]]; then log "[dry-run] download voltd-manage"
else curl -fsSL "$VOLTPANEL_RAW/scripts/manage-node.sh" -o /usr/local/sbin/voltd-manage; chmod 0755 /usr/local/sbin/voltd-manage
fi
if [[ -f "$SCRIPT_DIR/lib/common.sh" ]]; then run install -D -m 0644 "$SCRIPT_DIR/lib/common.sh" /usr/share/voltpanel-node/common.sh
elif [[ "$DRY_RUN" == 1 ]]; then log "[dry-run] install common.sh -> /usr/share/voltpanel-node/common.sh"
else run install -d -m 0755 /usr/share/voltpanel-node; curl -fsSL "$VOLTPANEL_RAW/scripts/lib/common.sh" -o /usr/share/voltpanel-node/common.sh; chmod 0644 /usr/share/voltpanel-node/common.sh
fi

if [[ "$OUTBOUND_ONLY" == 1 ]]; then
  cleanup_proxy_artifacts node
  systemctl_reload_start voltd
  ROLLBACK_ENABLED=0
  ok "VoltPanel outbound-only node installed and enrolled"
  printf '\n  Panel: %s\n  Identity: outbound command channel\n  Config: %s/voltd.toml\n  Data: %s\n\n' "$PANEL_URL" "$CONFIG_DIR" "$DATA_DIR"
  exit 0
fi
UPSTREAM=$(proxy_upstream "$LISTEN")
cleanup_proxy_artifacts node
case "$TLS_MODE" in
  caddy)
    install_caddy
    TLS_LINE=""; [[ -z "$EMAIL" ]] || TLS_LINE="    tls $EMAIL"
    write_file /etc/caddy/conf.d/voltpanel-node.caddy 0644 <<EOF
$DOMAIN {
$TLS_LINE
    encode zstd gzip
    reverse_proxy $UPSTREAM
    header {
        Strict-Transport-Security "max-age=31536000; includeSubDomains"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        Referrer-Policy "strict-origin-when-cross-origin"
    }
}
EOF
    configure_caddy_import
    run caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
    if systemctl is-active --quiet caddy 2>/dev/null; then run systemctl reload caddy; else run systemctl enable --now caddy; fi
    ;;
  certbot) configure_certbot_proxy node "$DOMAIN" "$UPSTREAM" "$EMAIL" ;;
  certbot-ip) configure_certbot_ip_proxy node "$IP_ADDRESS" "$UPSTREAM" "$EMAIL" ;;
  cloudflare) configure_cloudflare_proxy node "$DOMAIN" "$UPSTREAM" "$CF_CERT" "$CF_KEY" ;;
esac

# The agent requires HMAC-signed requests on every route, so readiness can only
# be confirmed by signing with the config secret (node_health_probe in common.sh).
systemctl_reload_start voltd node_health_probe
firewall_hint node "$TLS_MODE"
ROLLBACK_ENABLED=0
ok "VoltPanel node installed and enrolled"
printf '\n  Panel:      %s\n  Node URL:   %s\n  Config:     %s/voltd.toml\n  Data:       %s\n\n' "$PANEL_URL" "$PUBLIC_URL" "$CONFIG_DIR" "$DATA_DIR"
log "Run 'voltd-manage doctor' for diagnostics."
