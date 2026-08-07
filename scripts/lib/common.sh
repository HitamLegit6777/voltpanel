#!/usr/bin/env bash
set -Eeuo pipefail

VOLTPANEL_REPO="${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}"
VOLTPANEL_VERSION="${VOLTPANEL_VERSION:-latest}"
VOLTPANEL_GITHUB="https://github.com/${VOLTPANEL_REPO}"
export VOLTPANEL_RAW="https://raw.githubusercontent.com/${VOLTPANEL_REPO}/main"
COLOR="${NO_COLOR:-}"
DRY_RUN="${DRY_RUN:-0}"

if [[ -t 1 && -z "$COLOR" ]]; then
  C_BLUE=$'\033[1;34m'; C_GREEN=$'\033[1;32m'; C_YELLOW=$'\033[1;33m'; C_RED=$'\033[1;31m'; C_RESET=$'\033[0m'
else
  C_BLUE=""; C_GREEN=""; C_YELLOW=""; C_RED=""; C_RESET=""
fi

log() { printf '%s[voltpanel]%s %s\n' "$C_BLUE" "$C_RESET" "$*"; }
ok() { printf '%s[ok]%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%s[warn]%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
die() { printf '%s[error]%s %s\n' "$C_RED" "$C_RESET" "$*" >&2; exit 1; }

run() {
  if [[ "$DRY_RUN" == "1" ]]; then printf '[dry-run]'; printf ' %q' "$@"; printf '\n'; else "$@"; fi
}

require_root() { [[ ${EUID:-$(id -u)} -eq 0 ]] || die "Run as root (sudo)."; }
require_systemd() { command -v systemctl >/dev/null || die "systemd is required."; [[ -d /run/systemd/system ]] || die "systemd is not running."; }

tui_available() { [[ -r /dev/tty && -w /dev/tty && "${TERM:-dumb}" != "dumb" ]]; }

tui_title() {
  local title=$1
  printf '\033[2J\033[H\033[1;34m%s\033[0m\n\n' "$title" > /dev/tty
}

tui_note() { printf '\033[1;33m%s\033[0m\n' "$1" > /dev/tty; }

tui_input() {
  local prompt=$1 default=${2:-} value
  if [[ -n "$default" ]]; then
    printf '%s \033[2m[%s]\033[0m: ' "$prompt" "$default" > /dev/tty
  else
    printf '%s: ' "$prompt" > /dev/tty
  fi
  IFS= read -r value < /dev/tty || die "Terminal input closed."
  printf '%s' "${value:-$default}"
}

tui_secret() {
  local prompt=$1 value
  printf '%s: ' "$prompt" > /dev/tty
  IFS= read -r -s value < /dev/tty || die "Terminal input closed."
  printf '\n' > /dev/tty
  printf '%s' "$value"
}

tui_menu() {
  local prompt=$1 default=$2; shift 2
  local -a values=() labels=()
  local index=1 choice
  while (($#)); do values+=("$1"); labels+=("$2"); shift 2; done
  printf '%s\n' "$prompt" > /dev/tty
  for ((index=0; index<${#values[@]}; index++)); do
    if [[ "${values[index]}" == "$default" ]]; then
      printf '  \033[1;32m%d)\033[0m %s \033[2m(default)\033[0m\n' "$((index + 1))" "${labels[index]}" > /dev/tty
    else
      printf '  %d) %s\n' "$((index + 1))" "${labels[index]}" > /dev/tty
    fi
  done
  while true; do
    printf 'Choice: ' > /dev/tty
    IFS= read -r choice < /dev/tty || die "Terminal input closed."
    [[ -n "$choice" ]] || { printf '%s' "$default"; return; }
    if [[ "$choice" =~ ^[0-9]+$ ]] && ((choice >= 1 && choice <= ${#values[@]})); then
      printf '%s' "${values[choice - 1]}"; return
    fi
    printf '\033[1;31mEnter a number from 1 to %d.\033[0m\n' "${#values[@]}" > /dev/tty
  done
}

tui_yesno() {
  local prompt=$1 default=${2:-yes} answer suffix='Y/n'
  [[ "$default" == no ]] && suffix='y/N'
  while true; do
    printf '%s [%s]: ' "$prompt" "$suffix" > /dev/tty
    IFS= read -r answer < /dev/tty || die "Terminal input closed."
    answer=${answer:-$default}
    case "${answer,,}" in y|yes) return 0;; n|no) return 1;; esac
  done
}

tui_pause() { printf '\nPress Enter to install, or Ctrl+C to cancel...' > /dev/tty; IFS= read -r _ < /dev/tty; printf '\n' > /dev/tty; }
validate_ip() { [[ $1 =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ || $1 == *:* ]] || die "Invalid IP address: $1"; }
validate_port() {
  if [[ ! $1 =~ ^[0-9]+$ ]] || ((10#$1 < 1 || 10#$1 > 65535)); then die "Invalid port: $1 (expected 1-65535)"; fi
}

load_os() {
  [[ -r /etc/os-release ]] || die "Cannot detect Linux distribution."
  # shellcheck disable=SC1091
  source /etc/os-release
  OS_ID="${ID:-unknown}"
  OS_LIKE="${ID_LIKE:-}"
  case " $OS_ID $OS_LIKE " in
    *" debian "*|*" ubuntu "*) PKG_FAMILY=debian ;;
    *" fedora "*|*" rhel "*|*" centos "*|*" rocky "*|*" almalinux "*) PKG_FAMILY=rhel ;;
    *" arch "*) PKG_FAMILY=arch ;;
    *) die "Unsupported distribution: ${PRETTY_NAME:-$OS_ID}. Supported: Debian/Ubuntu, Fedora/RHEL/Rocky/Alma, Arch." ;;
  esac
}

install_packages() {
  local packages=(curl ca-certificates bubblewrap util-linux iproute2 nftables sqlite3 tar gzip openssl)
  case "$PKG_FAMILY" in
    debian) run apt-get update -y; run env DEBIAN_FRONTEND=noninteractive apt-get install -y "${packages[@]}" ;;
    rhel) run dnf install -y curl ca-certificates bubblewrap util-linux iproute nftables sqlite tar gzip openssl ;;
    arch) run pacman -Sy --noconfirm curl ca-certificates bubblewrap util-linux iproute2 nftables sqlite tar gzip openssl ;;
  esac
}

arch_asset() {
  case "$(uname -m)" in
    x86_64|amd64) printf 'linux-amd64' ;;
    aarch64|arm64) printf 'linux-arm64' ;;
    *) die "Unsupported CPU architecture: $(uname -m)" ;;
  esac
}

release_url() {
  local binary=$1 arch version_path
  arch=$(arch_asset)
  if [[ "$VOLTPANEL_VERSION" == "latest" ]]; then version_path=latest/download; else version_path="download/${VOLTPANEL_VERSION}"; fi
  printf '%s/releases/%s/%s-%s' "$VOLTPANEL_GITHUB" "$version_path" "$binary" "$arch"
}

release_base_url() {
  local version_path
  if [[ "$VOLTPANEL_VERSION" == "latest" ]]; then version_path=latest/download; else version_path="download/${VOLTPANEL_VERSION}"; fi
  printf '%s/releases/%s' "$VOLTPANEL_GITHUB" "$version_path"
}

install_binary() {
  local binary=$1 target="/usr/local/bin/$1" url temp checksums asset expected actual
  url=$(release_url "$binary"); asset="${binary}-$(arch_asset)"; temp=$(mktemp); checksums=$(mktemp)
  log "Downloading $binary from $url"
  if [[ "$DRY_RUN" == "1" ]]; then log "[dry-run] verify SHA256SUMS and install $binary -> $target"; rm -f "$temp" "$checksums"; return; fi
  curl --fail --location --retry 3 --connect-timeout 15 "$url" -o "$temp" || die "Binary download failed. Check the release and architecture."
  curl --fail --location --retry 3 --connect-timeout 15 "$(release_base_url)/SHA256SUMS" -o "$checksums" || die "Checksum download failed."
  expected=$(awk -v asset="$asset" '$2==asset{print $1}' "$checksums"); [[ -n "$expected" ]] || die "No checksum published for $asset"
  actual=$(sha256sum "$temp" | awk '{print $1}'); [[ "$actual" == "$expected" ]] || die "Checksum mismatch for $asset"
  install -m 0755 "$temp" "$target"; rm -f "$temp" "$checksums"
}
random_secret() { openssl rand -base64 "${1:-36}" | tr -d '\n'; }
validate_domain() { [[ $1 =~ ^([A-Za-z0-9-]+\.)+[A-Za-z]{2,}$ ]] || die "Invalid domain: $1"; }
validate_url() { [[ $1 =~ ^https?://[^[:space:]]+$ ]] || die "Invalid URL: $1"; }

install_caddy() {
  if command -v caddy >/dev/null; then return; fi
  log "Installing Caddy"
  case "$PKG_FAMILY" in
    debian)
      run apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gpg
      if [[ "$DRY_RUN" != "1" ]]; then
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' > /etc/apt/sources.list.d/caddy-stable.list
      fi
      run apt-get update -y; run apt-get install -y caddy ;;
    rhel) run dnf install -y 'dnf-command(copr)'; run dnf copr enable -y @caddy/caddy; run dnf install -y caddy ;;
    arch) run pacman -S --noconfirm caddy ;;
  esac
}

write_file() {
  local path=$1 mode=$2
  if [[ "$DRY_RUN" == "1" ]]; then log "[dry-run] write $path ($mode)"; cat >/dev/null; return; fi
  install -d -m 0755 "$(dirname "$path")"
  cat > "$path"
  chmod "$mode" "$path"
}

firewall_hint() {
  local role=$1
  if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q '^Status: active'; then
    if [[ "$role" == panel ]]; then
      run ufw allow 80/tcp; run ufw allow 443/tcp; run ufw allow 20000:30000/tcp; run ufw allow 20000:30000/udp
    else
      warn "UFW is active. Allow the node HTTPS endpoint and allocated game ports from trusted sources."
    fi
  fi
}

install_nginx_certbot() {
  log "Installing Nginx and Certbot"
  case "$PKG_FAMILY" in
    debian) run env DEBIAN_FRONTEND=noninteractive apt-get install -y nginx certbot python3-certbot-nginx ;;
    rhel) run dnf install -y nginx certbot python3-certbot-nginx ;;
    arch) run pacman -S --noconfirm nginx certbot certbot-nginx ;;
  esac
}

install_certbot_ip() {
  log "Installing Nginx and Certbot with IP certificate support"
  case "$PKG_FAMILY" in
    debian) run env DEBIAN_FRONTEND=noninteractive apt-get install -y nginx python3 python3-venv ;;
    rhel) run dnf install -y nginx python3 python3-pip ;;
    arch) run pacman -S --noconfirm nginx python python-pip ;;
  esac
  run python3 -m venv /opt/voltpanel-certbot
  run /opt/voltpanel-certbot/bin/pip install --upgrade 'certbot>=5.4'
  run ln -sfn /opt/voltpanel-certbot/bin/certbot /usr/local/bin/certbot
}

require_certbot_ip_support() {
  if [[ "$DRY_RUN" == "1" ]]; then log "[dry-run] require Certbot >= 5.4"; return; fi
  local version major minor
  version=$(certbot --version 2>&1 | awk '{print $2}')
  major=${version%%.*}; minor=${version#*.}; minor=${minor%%.*}
  [[ "$major" =~ ^[0-9]+$ && "$minor" =~ ^[0-9]+$ ]] || die "Cannot determine Certbot version: $version"
  ((major > 5 || (major == 5 && minor >= 4))) || die "IP certificates require Certbot 5.4 or newer; installed: $version"
}

configure_caddy_import() {
  if [[ "$DRY_RUN" == "1" ]]; then log "[dry-run] ensure Caddyfile imports /etc/caddy/conf.d/*.caddy"; return; fi
  install -d -m 0755 /etc/caddy/conf.d
  touch /etc/caddy/Caddyfile
  grep -Fq 'import /etc/caddy/conf.d/*.caddy' /etc/caddy/Caddyfile || {
    cp /etc/caddy/Caddyfile /etc/caddy/Caddyfile.pre-voltpanel
    printf '\nimport /etc/caddy/conf.d/*.caddy\n' >> /etc/caddy/Caddyfile
  }
}

configure_certbot_proxy() {
  local name=$1 domain=$2 upstream=$3 email=$4
  install_nginx_certbot
  write_file "/etc/nginx/conf.d/voltpanel-${name}.conf" 0644 <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name $domain;
    client_max_body_size 256m;

    location / {
        proxy_pass http://$upstream;
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_set_header Upgrade \$http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;
    }
}
EOF
  run nginx -t
  run systemctl enable --now nginx
  run systemctl reload nginx
  local certbot_args=(--nginx --non-interactive --agree-tos --redirect -d "$domain")
  if [[ -n "$email" ]]; then certbot_args+=(--email "$email"); else certbot_args+=(--register-unsafely-without-email); fi
  run certbot "${certbot_args[@]}"
}

configure_certbot_ip_proxy() {
  local name=$1 ip=$2 upstream=$3 email=$4 webroot=/var/lib/voltpanel/acme
  validate_ip "$ip"
  install_certbot_ip
  require_certbot_ip_support
  run install -d -m 0755 "$webroot"
  write_file "/etc/nginx/conf.d/voltpanel-${name}.conf" 0644 <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name $ip;

    location ^~ /.well-known/acme-challenge/ { root $webroot; }
    location / { return 308 https://\$host\$request_uri; }
}
EOF
  run nginx -t
  run systemctl enable --now nginx
  run systemctl reload nginx
  local certbot_args=(certonly --webroot --webroot-path "$webroot" --preferred-profile shortlived --ip-address "$ip" --non-interactive --agree-tos)
  if [[ -n "$email" ]]; then certbot_args+=(--email "$email"); else certbot_args+=(--register-unsafely-without-email); fi
  run certbot "${certbot_args[@]}"
  write_file "/etc/nginx/conf.d/voltpanel-${name}.conf" 0644 <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name $ip;
    location ^~ /.well-known/acme-challenge/ { root $webroot; }
    location / { return 308 https://\$host\$request_uri; }
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name $ip;
    client_max_body_size 256m;
    ssl_certificate /etc/letsencrypt/live/$ip/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/$ip/privkey.pem;

    location / {
        proxy_pass http://$upstream;
        proxy_http_version 1.1;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_set_header Upgrade \$http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;
    }
}
EOF
  write_file "/etc/letsencrypt/renewal-hooks/deploy/voltpanel-${name}-nginx" 0755 <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
nginx -t
systemctl reload nginx
EOF
  run nginx -t
  run systemctl reload nginx
  write_file "/etc/systemd/system/voltpanel-certbot-${name}.service" 0644 <<EOF
[Unit]
Description=Renew VoltPanel $name IP certificate

[Service]
Type=oneshot
ExecStart=/usr/local/bin/certbot renew --quiet
EOF
  write_file "/etc/systemd/system/voltpanel-certbot-${name}.timer" 0644 <<EOF
[Unit]
Description=Renew VoltPanel $name IP certificate frequently

[Timer]
OnBootSec=15min
OnUnitActiveSec=12h
RandomizedDelaySec=30min
Persistent=true

[Install]
WantedBy=timers.target
EOF
  run systemctl daemon-reload
  run systemctl enable --now "voltpanel-certbot-${name}.timer"
}

configure_cloudflare_proxy() {
  local name=$1 domain=$2 upstream=$3 certificate=$4 private_key=$5
  [[ -r "$certificate" ]] || die "Cloudflare Origin Certificate not readable: $certificate"
  [[ -r "$private_key" ]] || die "Cloudflare Origin private key not readable: $private_key"
  install_caddy
  if [[ "$DRY_RUN" == "1" ]]; then
    log "[dry-run] install Cloudflare Origin Certificate and private key"
  else
    install -d -m 0750 -o root -g caddy /etc/voltpanel/tls
    install -m 0644 -o root -g caddy "$certificate" "/etc/voltpanel/tls/${name}-cloudflare.pem"
    install -m 0640 -o root -g caddy "$private_key" "/etc/voltpanel/tls/${name}-cloudflare.key"
  fi
  write_file "/etc/caddy/conf.d/voltpanel-${name}.caddy" 0644 <<EOF
$domain {
    tls /etc/voltpanel/tls/${name}-cloudflare.pem /etc/voltpanel/tls/${name}-cloudflare.key
    encode zstd gzip
    reverse_proxy $upstream
    header {
        Strict-Transport-Security "max-age=31536000; includeSubDomains; preload"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        Referrer-Policy "strict-origin-when-cross-origin"
    }
}
EOF
  configure_caddy_import
  run systemctl enable --now caddy
  run caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
  run systemctl reload caddy
}

systemctl_reload_start() {
  local service=$1
  run systemctl daemon-reload
  run systemctl enable --now "$service"
  if [[ "$DRY_RUN" != "1" ]]; then
    sleep 2
    systemctl is-active --quiet "$service" || { systemctl status "$service" --no-pager || true; journalctl -u "$service" -n 80 --no-pager || true; die "$service failed to start"; }
  fi
}
