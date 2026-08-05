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

DOMAIN=""; EMAIL=""; NO_CADDY=0; PUBLIC=0; LISTEN=""; DATA_DIR=/var/lib/voltpanel; CONFIG_DIR=/etc/voltpanel
while (($#)); do
  case "$1" in
    --domain) DOMAIN=${2:?}; shift 2;;
    --email) EMAIL=${2:?}; shift 2;;
    --listen) LISTEN=${2:?}; shift 2;;
    --public) PUBLIC=1; shift;;
    --no-caddy) NO_CADDY=1; shift;;
    --data-dir) DATA_DIR=${2:?}; shift 2;;
    --version) VOLTPANEL_VERSION=${2:?}; shift 2;;
    --dry-run) DRY_RUN=1; shift;;
    --help|-h) cat <<'EOF'
VoltPanel panel installer

Usage: sudo ./install-panel.sh [options]
  --domain panel.example.com  Configure automatic HTTPS through Caddy
  --email admin@example.com   ACME contact email
  --public                    Listen directly on 0.0.0.0:8080 (not recommended)
  --listen ADDRESS            Explicit panel listen address
  --no-caddy                  Do not install/configure Caddy
  --data-dir PATH             Data directory (default /var/lib/voltpanel)
  --version VERSION           Release tag (default latest)
  --dry-run                   Print actions without modifying the host
EOF
      exit 0;;
    *) die "Unknown argument: $1";;
  esac
done
export VOLTPANEL_VERSION

require_root; require_systemd; load_os
[[ -z "$DOMAIN" ]] || validate_domain "$DOMAIN"
if [[ -z "$LISTEN" ]]; then
  if [[ -n "$DOMAIN" && "$NO_CADDY" == 0 ]]; then LISTEN=127.0.0.1:8080
  elif [[ "$PUBLIC" == 1 ]]; then LISTEN=0.0.0.0:8080
  else LISTEN=127.0.0.1:8080; fi
fi

install_packages
install_binary voltpanel
run install -d -m 0700 "$DATA_DIR" "$DATA_DIR/servers" "$DATA_DIR/backups" "$DATA_DIR/eggs" "$DATA_DIR/logs" "$DATA_DIR/websites"
run install -d -m 0700 "$CONFIG_DIR"

ADMIN_PASSWORD=${VOLTPANEL_ADMIN_PASSWORD:-$(random_secret 24)}
JWT_SECRET=$(random_secret 48)

write_file "$CONFIG_DIR/config.toml" 0600 <<EOF
[general]
instance_name = "VoltPanel"
locale = "en"
timezone = "UTC"
data_dir = "$DATA_DIR"
log_level = "info"

[web]
listen = "$LISTEN"
base_path = "/"
session_ttl_hours = 24
max_body_mb = 256

[paths]
servers_dir = "servers"
backups_dir = "backups"
eggs_dir = "eggs"
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
jwt_secret = "$JWT_SECRET"
rate_limit_per_min = 120
password_min_len = 10
userland = "nobody"
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

write_file "$CONFIG_DIR/first-run.env" 0600 <<EOF
VOLTPANEL_ADMIN_PASSWORD=$ADMIN_PASSWORD
EOF

write_file /etc/systemd/system/voltpanel.service 0644 <<EOF
[Unit]
Description=VoltPanel game hosting control plane
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
Group=root
WorkingDirectory=$DATA_DIR
Environment=VOLTPANEL_CONFIG=$CONFIG_DIR/config.toml
Environment=RUST_LOG=info
EnvironmentFile=-$CONFIG_DIR/first-run.env
ExecStart=/usr/local/bin/voltpanel
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

if [[ -f "$SCRIPT_DIR/manage-panel.sh" ]]; then run install -m0755 "$SCRIPT_DIR/manage-panel.sh" /usr/local/sbin/voltpanel-manage
elif [[ "$DRY_RUN" == 1 ]]; then log "[dry-run] download voltpanel-manage"
else curl -fsSL "$VOLTPANEL_RAW/scripts/manage-panel.sh" -o /usr/local/sbin/voltpanel-manage; chmod 0755 /usr/local/sbin/voltpanel-manage
fi

if [[ -n "$DOMAIN" && "$NO_CADDY" == 0 ]]; then
  install_caddy
  TLS_LINE=""; [[ -z "$EMAIL" ]] || TLS_LINE="    tls $EMAIL"
  write_file /etc/caddy/conf.d/voltpanel-panel.caddy 0644 <<EOF
$DOMAIN {
$TLS_LINE
    encode zstd gzip
    reverse_proxy 127.0.0.1:8080
    header {
        Strict-Transport-Security "max-age=31536000; includeSubDomains; preload"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        Referrer-Policy "strict-origin-when-cross-origin"
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

systemctl_reload_start voltpanel
if [[ "$DRY_RUN" != 1 ]]; then rm -f "$CONFIG_DIR/first-run.env"; fi
firewall_hint panel

URL="http://$LISTEN"; [[ -z "$DOMAIN" || "$NO_CADDY" == 1 ]] || URL="https://$DOMAIN"
ok "VoltPanel installed"
printf '\n  URL:      %s\n  Username: admin\n  Password: %s\n\n' "$URL" "$ADMIN_PASSWORD"
warn "Save this password now; it is not written to disk after first start. Change it immediately."
log "Run 'voltpanel-manage doctor' for diagnostics."
