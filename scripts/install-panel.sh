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
# A failed fresh install rolls back the local state it created; a rerun
# (EXISTING=1) never rolls back, so existing config and credentials survive.
ROLLBACK_ENABLED=0
rollback_panel() {
  local rc=$?
  [[ -n "${CONFIG_TMP:-}" && "$DRY_RUN" != 1 ]] && rm -f "$CONFIG_TMP"
  [[ -n "${TMP_COMMON:-}" ]] && rm -f "$TMP_COMMON"
  [[ "$ROLLBACK_ENABLED" == 1 ]] || return 0
  warn "Install failed; removing the partial panel install. Re-run to start fresh."
  run rm -f "$CONFIG_DIR/config.toml" "$CONFIG_DIR/first-run.env" /etc/systemd/system/voltpanel.service /usr/local/sbin/voltpanel-manage /usr/share/voltpanel/common.sh
  if systemctl is-active --quiet voltpanel 2>/dev/null; then run systemctl disable --now voltpanel >/dev/null 2>&1 || true; fi
  run systemctl daemon-reload
  cleanup_proxy_artifacts panel
  exit "$rc"
}
trap 'rollback_panel' EXIT

DOMAIN=""; IP_ADDRESS=""; EMAIL=""; TLS_MODE=""; CF_CERT=""; CF_KEY=""; PORT=8080; PORT_SET=0; PUBLIC=0; LISTEN=""; DATA_DIR=/var/lib/voltpanel; CONFIG_DIR=/etc/voltpanel; INTERACTIVE=auto; TLS_INTENT=0; CONFIG_TMP=""; ACTION=""; PURGE=0; RESET_USER="admin"
printf -v ACTION '%s' deploy
ARG_COUNT=$#
set_action() {
  local requested=$1
  [[ "$ACTION" == deploy || "$ACTION" == "$requested" ]] || die "Choose only one action: reinstall, uninstall, or reset-password"
  ACTION=$requested
}

while (($#)); do
  case "$1" in
    --reinstall) set_action reinstall; shift;;
    --uninstall) set_action uninstall; shift;;
    --purge) PURGE=1; shift;;
    --reset-password)
      set_action reset-password
      if (($# > 1)) && [[ "$2" != --* ]]; then RESET_USER=$2; shift 2; else shift; fi
      ;;
    --domain) DOMAIN=${2:?}; TLS_INTENT=1; shift 2;;
    --ip-address) IP_ADDRESS=${2:?}; TLS_INTENT=1; shift 2;;
    --email) EMAIL=${2:?}; shift 2;;
    --tls) TLS_MODE=${2:?}; TLS_INTENT=1; shift 2;;
    --cloudflare-cert) CF_CERT=${2:?}; TLS_INTENT=1; shift 2;;
    --cloudflare-key) CF_KEY=${2:?}; TLS_INTENT=1; shift 2;;
    --port) PORT=${2:?}; PORT_SET=1; shift 2;;
    --listen) LISTEN=${2:?}; shift 2;;
    --public) PUBLIC=1; TLS_INTENT=1; shift;;
    --no-caddy) TLS_MODE=none; TLS_INTENT=1; shift;;
    --non-interactive) INTERACTIVE=0; shift;;
    --data-dir) DATA_DIR=${2:?}; shift 2;;
    --version) VOLTPANEL_VERSION=${2:?}; shift 2;;
    --dry-run) DRY_RUN=1; shift;;
    --help|-h) cat <<'EOF'
VoltPanel panel installer

Usage: sudo ./install-panel.sh [options]
  --reinstall                 Reinstall binary/service; preserve config and data
  --uninstall                 Remove application; preserve config and data
  --uninstall --purge         Remove application, config, and data permanently
  --reset-password [USERNAME] Reset a user password (default admin)
  --domain panel.example.com  Public panel domain
  --email admin@example.com   ACME contact email
  --tls MODE                  caddy, certbot, certbot-ip, cloudflare, or none
  --ip-address IP             Public IPv4 or IPv6 for certbot-ip
  --cloudflare-cert PATH      Cloudflare Origin Certificate PEM
  --cloudflare-key PATH       Cloudflare Origin private key
  --public                    Listen directly on 0.0.0.0 (no TLS only)
  --port PORT                 Internal/direct panel port (default 8080)
  --listen ADDRESS            Explicit listen address (overrides --port)
  --no-caddy                  Alias for --tls none
  --non-interactive           Disable the terminal wizard
  --data-dir PATH             Data directory (default /var/lib/voltpanel)
  --version VERSION           Release tag (default latest)
  --dry-run                   Print actions without modifying the host
EOF
      exit 0;;
    *) die "Unknown argument: $1";;
  esac
done
export VOLTPANEL_VERSION
# Resolve `latest` to the concrete release tag, then pin every further script
# fetch to that same tag instead of the main branch.
resolve_release_tag
refresh_raw_base

config_get() {
  local section=$1 key=$2
  [[ -r "$CONFIG_DIR/config.toml" ]] || return 0
  awk -F= -v section="$section" -v key="$key" '
    /^\[/ { current=$0; gsub(/[[:space:]\[\]]/, "", current); next }
    current == section && $1 ~ "^[[:space:]]*" key "[[:space:]]*$" {
      value=$2; sub(/^[[:space:]]*/, "", value); sub(/[[:space:]]*$/, "", value); gsub(/^"|"$/, "", value); print value; exit
    }
  ' "$CONFIG_DIR/config.toml"
}

# Older installers wrote keys the panel config schema no longer accepts
# (timezone, base_path, jwt_secret, userland). Config is now
# deny_unknown_fields, so strip them before check-config or the panel refuses
# to boot with its own generated config.
strip_dead_config_keys() {
  local file=$1
  if grep -Eq '^[[:space:]]*(timezone|base_path|jwt_secret|userland)[[:space:]]*=' "$file"; then
    sed -i -E '/^[[:space:]]*(timezone|base_path|jwt_secret|userland)[[:space:]]*=/d' "$file" \
      || warn "Could not rewrite $file; remove the obsolete keys manually"
    warn "Removed obsolete config keys (timezone, base_path, jwt_secret, userland) from $file"
  fi
}

uninstall_panel() {
  local data
  data=$(config_get general data_dir)
  data=${data:-/var/lib/voltpanel}
  if [[ "$PURGE" == 1 ]] && unsafe_purge_path "$data"; then
    die "Refusing to purge unsafe data path: ${data:-<empty>}"
  fi
  run systemctl disable --now voltpanel || true
  run rm -f /etc/systemd/system/voltpanel.service /usr/local/bin/voltpanel \
    /usr/local/sbin/voltpanel-manage /usr/share/voltpanel/common.sh
  cleanup_proxy_artifacts panel
  run systemctl daemon-reload
  if [[ "$PURGE" == 1 ]]; then
    run rm -rf -- "$CONFIG_DIR" "$data"
    ok "VoltPanel uninstalled; config and data purged"
  else
    ok "VoltPanel uninstalled; config and data preserved"
    log "Reinstall later with: sudo bash install-panel.sh --reinstall"
  fi
}

if [[ "$INTERACTIVE" == auto && "$ARG_COUNT" == 0 ]] && tui_available; then
  INTERACTIVE=1
else
  INTERACTIVE=0
fi
if [[ "$INTERACTIVE" == 1 && -f "$CONFIG_DIR/config.toml" ]]; then
  tui_title "VoltPanel Management"
  ACTION=$(tui_menu "Choose action" "reinstall" \
    reinstall "Reinstall binary and service (preserve config/data)" \
    reset-password "Reset a user password" \
    uninstall "Uninstall application (preserve config/data)" \
    purge "Uninstall and permanently purge config/data" \
    cancel "Cancel")
  case "$ACTION" in
    reset-password) RESET_USER=$(tui_input "Username" "admin") ;;
    uninstall) tui_yesno "Remove VoltPanel but preserve config and data?" no || exit 0 ;;
    purge)
      ACTION=uninstall; PURGE=1
      tui_yesno "Permanently delete VoltPanel config and all data?" no || exit 0
      ;;
    cancel) exit 0 ;;
  esac
fi

[[ "$PURGE" == 0 || "$ACTION" == uninstall ]] || die "--purge requires --uninstall"
if [[ "$ACTION" == reset-password ]]; then
  [[ "$DRY_RUN" == 1 ]] || require_root
  reset_panel_password "$CONFIG_DIR/config.toml" "$RESET_USER"
  exit 0
fi
if [[ "$ACTION" == uninstall ]]; then
  if [[ "$DRY_RUN" != 1 ]]; then require_root; require_systemd; fi
  uninstall_panel
  exit 0
fi
if [[ "$ACTION" == reinstall && ! -f "$CONFIG_DIR/config.toml" ]]; then
  die "Cannot reinstall: missing $CONFIG_DIR/config.toml; run a normal install first"
fi

if [[ "$INTERACTIVE" == 1 && "$ACTION" == deploy ]]; then
  TLS_INTENT=1
  tui_title "VoltPanel Panel Installer"
  TLS_MODE=$(tui_menu "Choose HTTPS mode" "caddy" \
    caddy "Caddy automatic HTTPS (recommended)" \
    certbot "Certbot + Nginx with a domain" \
    certbot-ip "Certbot + Nginx with a public IP" \
    cloudflare "Cloudflare Origin Certificate" \
    none "No reverse proxy / LAN only")
  if [[ "$TLS_MODE" != none ]]; then
    if [[ "$TLS_MODE" == certbot-ip ]]; then
      tui_note "IP certificates require Certbot 5.4+, are valid for about 6 days, and must renew automatically."
      IP_ADDRESS=$(tui_input "Public IP address" "$IP_ADDRESS")
    else
      DOMAIN=$(tui_input "Panel domain" "$DOMAIN")
    fi
    if [[ "$TLS_MODE" == caddy || "$TLS_MODE" == certbot || "$TLS_MODE" == certbot-ip ]]; then EMAIL=$(tui_input "ACME email (optional)" "$EMAIL"); fi
    if [[ "$TLS_MODE" == cloudflare ]]; then
      tui_note "Create an Origin Certificate in Cloudflare, download the PEM and private key to this host, and use Full (strict) SSL mode."
      CF_CERT=$(tui_input "Origin Certificate PEM path" "$CF_CERT")
      CF_KEY=$(tui_input "Origin private key path" "$CF_KEY")
    fi
  else
    PUBLIC=1
  fi
  PORT=$(tui_input "Panel port" "$PORT")
  DATA_DIR=$(tui_input "Data directory" "$DATA_DIR")
  printf '\n  TLS mode: %s\n  Address:  %s\n  Port:     %s\n  Data:     %s\n' "$TLS_MODE" "${DOMAIN:-${IP_ADDRESS:-(none)}}" "$PORT" "$DATA_DIR" > /dev/tty
  tui_pause
fi

TLS_MODE=${TLS_MODE:-$([[ -n "$DOMAIN" ]] && printf caddy || printf none)}
case "$TLS_MODE" in caddy|certbot|certbot-ip|cloudflare|none) ;; *) die "Invalid --tls mode: $TLS_MODE";; esac
[[ "$TLS_MODE" == none || "$TLS_MODE" == certbot-ip || -n "$DOMAIN" ]] || die "--domain is required for TLS mode $TLS_MODE"
[[ "$TLS_MODE" != certbot-ip || -n "$IP_ADDRESS" ]] || die "certbot-ip mode requires --ip-address"
[[ "$TLS_MODE" != cloudflare || (-n "$CF_CERT" && -n "$CF_KEY") ]] || die "Cloudflare mode requires --cloudflare-cert and --cloudflare-key"
if [[ "$TLS_MODE" == cloudflare ]]; then
  [[ -n "$CF_CERT" && -r "$CF_CERT" ]] || die "Cloudflare Origin Certificate not readable: ${CF_CERT:-<empty>}"
  [[ -n "$CF_KEY" && -r "$CF_KEY" ]] || die "Cloudflare Origin private key not readable: ${CF_KEY:-<empty>}"
fi
validate_port "$PORT"

if [[ "$DRY_RUN" != 1 ]]; then require_root; require_systemd; fi
load_os
[[ -z "$DOMAIN" ]] || validate_domain "$DOMAIN"; [[ -z "$IP_ADDRESS" ]] || validate_ip "$IP_ADDRESS"
if [[ -z "$LISTEN" ]]; then
  if [[ "$TLS_MODE" != none ]]; then LISTEN="127.0.0.1:$PORT"
  elif [[ "$PUBLIC" == 1 ]]; then LISTEN="0.0.0.0:$PORT"
  else LISTEN="127.0.0.1:$PORT"; fi
elif [[ "$PORT_SET" == 1 ]]; then
  die "Use either --listen or --port, not both"
fi
validate_listen "$LISTEN"


# Detect a prior install so a rerun preserves its config and admin
# credentials instead of rotating them or printing a freshly invented password.
EXISTING=0; EFFECTIVE_LISTEN=$LISTEN; EFFECTIVE_DATA=$DATA_DIR
if [[ -f "$CONFIG_DIR/config.toml" ]]; then
  EXISTING=1
  local_listen=$(config_get web listen)
  local_data=$(config_get general data_dir)
  if [[ -n "$local_listen" && "$LISTEN" != "$local_listen" ]]; then
    warn "Rerun preserves the existing web.listen ($local_listen); ignoring $LISTEN"
  fi
  if [[ -n "$local_data" && "$DATA_DIR" != "$local_data" ]]; then
    warn "Rerun preserves the existing data_dir ($local_data); ignoring $DATA_DIR"
  fi
  EFFECTIVE_LISTEN=${local_listen:-$LISTEN}
  EFFECTIVE_DATA=${local_data:-$DATA_DIR}
fi
if [[ "$TLS_MODE" != none && "$EFFECTIVE_LISTEN" != 127.0.0.1:* && "$EFFECTIVE_LISTEN" != \[::1\]:* ]]; then
  die "TLS proxy origin must listen on loopback (127.0.0.1 or [::1])"
fi

install_packages
install_binary voltpanel
run install -d -m 0700 "$EFFECTIVE_DATA" "$EFFECTIVE_DATA/servers" "$EFFECTIVE_DATA/backups" "$EFFECTIVE_DATA/blueprints" "$EFFECTIVE_DATA/logs" "$EFFECTIVE_DATA/websites"
run install -d -m 0700 "$CONFIG_DIR"

ADMIN_PASSWORD=""
if [[ "$EXISTING" == 1 ]]; then
  # Rerun: never rotate admin credentials. Recover a pending first-run
  # credential only when the admin account has not been provisioned yet.
  if [[ -f "$CONFIG_DIR/first-run.env" ]]; then
    ADMIN_PASSWORD=$(sed -n 's/^VOLTPANEL_ADMIN_PASSWORD=//p' "$CONFIG_DIR/first-run.env" | head -n1)
  fi
else
  if [[ "$DRY_RUN" == 1 ]]; then
    # Dry-run never invents or prints real credentials.
    ADMIN_PASSWORD="<generated during the real install>"
  else
    ADMIN_PASSWORD=${VOLTPANEL_ADMIN_PASSWORD:-$(random_secret 24)}
  fi
fi

if [[ "$EXISTING" == 1 ]]; then
  # Rerun: migrate configs written by older installers that carried keys the
  # schema no longer accepts (timezone, base_path, jwt_secret, userland), then
  # leave the on-disk config otherwise untouched and validate it is still
  # well-formed before we touch services or proxies.
  if [[ "$DRY_RUN" == 1 ]]; then
    log "[dry-run] validate existing $CONFIG_DIR/config.toml with voltpanel"
  else
    strip_dead_config_keys "$CONFIG_DIR/config.toml"
    VOLTPANEL_CONFIG="$CONFIG_DIR/config.toml" /usr/local/bin/voltpanel check-config --config "$CONFIG_DIR/config.toml"
  fi
else
  # Fresh install: validate the new config against a temp file before committing
  # it, so a faulty config is never left on the host.
  if [[ "$DRY_RUN" == 1 ]]; then CONFIG_TMP="$CONFIG_DIR/config.toml"; else CONFIG_TMP=$(mktemp); fi
  # A public-domain deployment (Caddy/Certbot/Cloudflare) is reached by a
  # hostname, so pin it in the strict allowlist. LAN-IP mode leaves the list
  # empty: the panel's derived mode accepts IP-literal Hosts out of the box.
  HOSTNAMES_LINE=""
  if [[ -n "$DOMAIN" ]]; then HOSTNAMES_LINE="hostnames = [\"$DOMAIN\"]"; fi
  write_file "$CONFIG_TMP" 0600 <<EOF
[general]
instance_name = "VoltPanel"
locale = "en"
data_dir = "$EFFECTIVE_DATA"
log_level = "info"

[web]
listen = "$EFFECTIVE_LISTEN"
session_ttl_hours = 24
max_body_mb = 64
$HOSTNAMES_LINE

[paths]
servers_dir = "servers"
backups_dir = "backups"
blueprints_dir = "blueprints"
logs_dir = "logs"
website_dir = "websites"

[limits]
default_memory_mb = 1024
default_disk_mb = 8192
default_cpu_percent = 100
max_memory_mb = 32768
max_servers_per_user = 32

[security]
argon2_cost = 3
argon2_mem_kib = 65536
rate_limit_per_min = 120
password_min_len = 10
allow_cross_server_dir = false

[features]
enable_backups = true
enable_databases = true
enable_schedules = true
enable_api_keys = true
enable_2fa = true
enable_websites = false
enable_audit_log = true
EOF
  if [[ "$DRY_RUN" == 1 ]]; then
    log "[dry-run] validate $CONFIG_DIR/config.toml with voltpanel"
  else
    VOLTPANEL_CONFIG="$CONFIG_TMP" /usr/local/bin/voltpanel check-config --config "$CONFIG_TMP"
    run install -m 0600 "$CONFIG_TMP" "$CONFIG_DIR/config.toml"
    ROLLBACK_ENABLED=1
  fi
fi

if [[ "$EXISTING" != 1 ]]; then
  write_file "$CONFIG_DIR/first-run.env" 0600 <<EOF
VOLTPANEL_ADMIN_PASSWORD=$ADMIN_PASSWORD
EOF
fi

write_file /etc/systemd/system/voltpanel.service 0644 <<EOF
[Unit]
Description=VoltPanel game hosting control plane
Documentation=https://github.com/HitamLegit6777/voltpanel
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=simple
User=root
Group=root
WorkingDirectory=$EFFECTIVE_DATA
Environment=VOLTPANEL_CONFIG=$CONFIG_DIR/config.toml
Environment=RUST_LOG=info
EnvironmentFile=-$CONFIG_DIR/first-run.env
ExecStart=/usr/local/bin/voltpanel
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
ProtectClock=yes
ProtectHome=yes
ProtectHostname=yes
ProtectKernelModules=yes
ProtectSystem=strict
ReadWritePaths=$EFFECTIVE_DATA $CONFIG_DIR /run/voltpanel /sys/fs/cgroup
# CAP_SYS_ADMIN is REQUIRED: local workloads spawn through ProcManager
# (bwrap -> setpriv tree), which needs it for sandbox mounts, cgroups and
# network namespaces. Do not remove it.
CapabilityBoundingSet=CAP_CHOWN CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_ADMIN CAP_NET_ADMIN CAP_NET_RAW CAP_KILL
AmbientCapabilities=CAP_CHOWN CAP_SETUID CAP_SETGID CAP_DAC_OVERRIDE CAP_FOWNER CAP_SYS_ADMIN CAP_NET_ADMIN CAP_NET_RAW CAP_KILL
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
EOF

if [[ -f "$SCRIPT_DIR/manage-panel.sh" ]]; then run install -m0755 "$SCRIPT_DIR/manage-panel.sh" /usr/local/sbin/voltpanel-manage
elif [[ "$DRY_RUN" == 1 ]]; then log "[dry-run] download voltpanel-manage"
else curl -fsSL "$VOLTPANEL_RAW/scripts/manage-panel.sh" -o /usr/local/sbin/voltpanel-manage; chmod 0755 /usr/local/sbin/voltpanel-manage
fi
if [[ -f "$SCRIPT_DIR/lib/common.sh" ]]; then run install -D -m 0644 "$SCRIPT_DIR/lib/common.sh" /usr/share/voltpanel/common.sh
elif [[ "$DRY_RUN" == 1 ]]; then log "[dry-run] install common.sh -> /usr/share/voltpanel/common.sh"
else run install -d -m 0755 /usr/share/voltpanel; curl -fsSL "$VOLTPANEL_RAW/scripts/lib/common.sh" -o /usr/share/voltpanel/common.sh; chmod 0644 /usr/share/voltpanel/common.sh
fi

# On a rerun that does not ask for TLS changes, leave the existing reverse
# proxy exactly as it is. Otherwise drop stale artifacts from other TLS modes
# (cleanup_proxy_artifacts is shared with the node installer) and write the
# config for the requested mode.
if [[ "$EXISTING" != 1 || "$TLS_INTENT" == 1 ]]; then
  UPSTREAM=$(proxy_upstream "$EFFECTIVE_LISTEN")
  cleanup_proxy_artifacts panel
  case "$TLS_MODE" in
    caddy)
      install_caddy
      TLS_LINE=""; [[ -z "$EMAIL" ]] || TLS_LINE="    tls $EMAIL"
      write_file /etc/caddy/conf.d/voltpanel-panel.caddy 0644 <<EOF
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
    certbot) configure_certbot_proxy panel "$DOMAIN" "$UPSTREAM" "$EMAIL" ;;
    certbot-ip) configure_certbot_ip_proxy panel "$IP_ADDRESS" "$UPSTREAM" "$EMAIL" ;;
    cloudflare) configure_cloudflare_proxy panel "$DOMAIN" "$UPSTREAM" "$CF_CERT" "$CF_KEY" ;;
  esac
fi

systemctl_reload_start voltpanel
# The credential is transient: once the service has started successfully the
# env file must not linger on disk, and it must not survive a later step (e.g.
# the reinstall restart) failing after this start. Delete it immediately.
if [[ "$DRY_RUN" != 1 ]]; then rm -f "$CONFIG_DIR/first-run.env"; fi
if [[ "$ACTION" == reinstall ]]; then
  run systemctl restart voltpanel
fi
firewall_hint panel
ROLLBACK_ENABLED=0

URL="http://$EFFECTIVE_LISTEN"; [[ "$EFFECTIVE_LISTEN" != 0.0.0.0:* ]] || URL="http://$(hostname -I | awk '{print $1}'):${EFFECTIVE_LISTEN##*:}"; [[ "$TLS_MODE" == none ]] || URL="https://${DOMAIN:-$IP_ADDRESS}"
if [[ "$ACTION" == reinstall ]]; then
  ok "VoltPanel reinstalled; existing configuration, data, and admin credentials preserved"
  printf '\n  URL:      %s\n  Username: admin\n  Password: (preserved)\n\n' "$URL"
  log "Run 'voltpanel-manage doctor' for diagnostics."
elif [[ "$EXISTING" == 1 ]]; then
  ok "VoltPanel already installed; existing configuration and admin credentials preserved"
  printf '\n  URL:      %s\n  Username: admin\n' "$URL"
  if [[ -n "$ADMIN_PASSWORD" && "$DRY_RUN" != 1 ]]; then
    printf '  Password: %s\n\n' "$ADMIN_PASSWORD"
    warn "Recovered the pending first-run credential from $CONFIG_DIR/first-run.env; the admin account is provisioned on this start."
  else
    printf '  Password: (preserved; not stored or printed by the installer)\n\n'
    log "Run 'voltpanel-manage doctor' for diagnostics."
  fi
else
  ok "VoltPanel installed"
  printf '\n  URL:      %s\n  Username: admin\n  Password: %s\n\n' "$URL" "$ADMIN_PASSWORD"
  warn "Save this password now; it is not written to disk after first start. Change it immediately."
  log "Run 'voltpanel-manage doctor' for diagnostics."
fi
