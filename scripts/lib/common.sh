#!/usr/bin/env bash
set -Eeuo pipefail

VOLTPANEL_REPO="${VOLTPANEL_REPO:-HitamLegit6777/voltpanel}"
VOLTPANEL_VERSION="${VOLTPANEL_VERSION:-latest}"
VOLTPANEL_GITHUB="https://github.com/${VOLTPANEL_REPO}"
refresh_raw_base() {
  if [[ -z "${VOLTPANEL_VERSION:-}" || "$VOLTPANEL_VERSION" == latest ]]; then
    VOLTPANEL_RAW="https://raw.githubusercontent.com/${VOLTPANEL_REPO}/main"
  else
    VOLTPANEL_RAW="https://raw.githubusercontent.com/${VOLTPANEL_REPO}/${VOLTPANEL_VERSION}"
  fi
  export VOLTPANEL_RAW
}
refresh_raw_base
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
# Resolve the floating `latest` marker to the concrete release tag so the
# binary and the helper scripts fetched alongside it always come from the SAME
# release instead of drifting between a tag and the main branch. Idempotent:
# installers call it again after parsing --version; it is a no-op once the
# version is pinned.
resolve_release_tag() {
  [[ "${VOLTPANEL_VERSION:-latest}" == latest ]] || return 0
  if [[ "$DRY_RUN" == "1" ]]; then log "[dry-run] resolve latest release tag for $VOLTPANEL_REPO"; return 0; fi
  local tag
  tag=$(curl -fsSL --retry 3 --connect-timeout 15 "https://api.github.com/repos/${VOLTPANEL_REPO}/releases/latest" 2>/dev/null \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)
  [[ -n "$tag" ]] || die "Cannot resolve the latest release tag for $VOLTPANEL_REPO"
  VOLTPANEL_VERSION=$tag
  export VOLTPANEL_VERSION
  refresh_raw_base
}


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
valid_ipv4() {
  local a=$1 b
  [[ "$a" =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$ ]] || return 1
  for b in "${BASH_REMATCH[@]:1}"; do ((10#${b} <= 255)) || return 1; done
  return 0
}

valid_ipv6() {
  local addr=$1 left right n hex oct
  [[ -n "$addr" && "$addr" == *:* ]] || return 1
  [[ "$addr" == *[!0-9A-Fa-f:.]* ]] && return 1
  [[ "$addr" == *%* ]] && return 1
  [[ "$addr" =~ ^: && "$addr" != ::* ]] && return 1
  [[ "$addr" =~ :$ && "$addr" != *:: ]] && return 1
  local stripped=${addr//::/}
  (( (${#addr} - ${#stripped}) / 2 > 1 )) && return 1
  if [[ "$addr" == *.* ]]; then
    if [[ "$addr" =~ (.*):([0-9]{1,3}(\.[0-9]{1,3}){3})$ ]]; then
      local octets=${BASH_REMATCH[2]}
      IFS=. read -r -a octs <<<"$octets"
      for oct in "${octs[@]}"; do ((10#$oct <= 255)) || return 1; done
      addr="${BASH_REMATCH[1]}:0:0"
    else
      return 1
    fi
  fi
  if [[ "$addr" == *::* ]]; then
    left="${addr%%::*}"; right="${addr##*::}"
  else
    left="$addr"; right=""
  fi
  n=0
  if [[ -n "$left" ]]; then
    IFS=':' read -r -a parts <<<"$left"
    for hex in "${parts[@]}"; do [[ "$hex" =~ ^[0-9A-Fa-f]{1,4}$ ]] || return 1; n=$((n + 1)); done
  fi
  if [[ -n "$right" ]]; then
    IFS=':' read -r -a parts <<<"$right"
    for hex in "${parts[@]}"; do [[ "$hex" =~ ^[0-9A-Fa-f]{1,4}$ ]] || return 1; n=$((n + 1)); done
  fi
  if [[ "$addr" == *::* ]]; then
    ((n <= 7)) || return 1
  else
    ((n == 8)) || return 1
  fi
  return 0
}

validate_ip() {
  local addr=$1 len
  # Strip surrounding brackets (bracketed IPv6 literal form) before checking.
  if [[ "${addr#\[}" != "$addr" && "${addr%\]}" != "$addr" ]]; then
    len=${#addr}
    addr=${addr:1:len-2}
  fi
  valid_ipv4 "$addr" || valid_ipv6 "$addr" || die "Invalid IP address: $1"
}

# Render a host (domain or raw IP) in URL/nginx host form: IPv6 gets brackets.
host_for_url() {
  local h=$1
  if [[ "${h#\[}" != "$h" && "${h%\]}" != "$h" ]]; then printf '%s' "$h"
  elif [[ "$h" == *:* ]]; then printf '[%s]' "$h"
  else printf '%s' "$h"; fi
}
validate_port() {
  if [[ ! $1 =~ ^[0-9]+$ ]] || ((10#$1 < 1 || 10#$1 > 65535)); then die "Invalid port: $1 (expected 1-65535)"; fi
}

validate_listen() {
  local listen=$1 host port
  [[ "$listen" == *:* ]] || die "Invalid listen address: $listen (expected HOST:PORT)"
  host=${listen%:*}; port=${listen##*:}
  validate_port "$port"
  if [[ "$host" == *:* && ( "${host#\[}" == "$host" || "${host%\]}" == "$host" ) ]]; then
    die "Invalid listen address: IPv6 hosts must be bracketed, e.g. [::1]:$port"
  fi
  validate_ip "$host"
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
  local binary=$1 target="/usr/local/bin/$1" url temp checksums asset expected actual output status
  url=$(release_url "$binary"); asset="${binary}-$(arch_asset)"
  log "Downloading $binary from $url"
  if [[ "$DRY_RUN" == "1" ]]; then log "[dry-run] verify SHA256SUMS, binary identity, and install $binary -> $target"; return; fi
  # Identity checks execute the artifact. Stage it beside the final target
  # because /tmp is commonly mounted noexec on hardened hosts.
  temp=$(mktemp "/usr/local/bin/.${binary}.install.XXXXXX")
  checksums=$(mktemp)
  curl --fail --location --retry 3 --connect-timeout 15 "$url" -o "$temp" || { rm -f "$temp" "$checksums"; die "Binary download failed. Check the release and architecture."; }
  curl --fail --location --retry 3 --connect-timeout 15 "$(release_base_url)/SHA256SUMS" -o "$checksums" || { rm -f "$temp" "$checksums"; die "Checksum download failed."; }
  expected=$(awk -v asset="$asset" '$2==asset{print $1}' "$checksums"); [[ -n "$expected" ]] || { rm -f "$temp" "$checksums"; die "No checksum published for $asset"; }
  actual=$(sha256sum "$temp" | awk '{print $1}'); [[ "$actual" == "$expected" ]] || { rm -f "$temp" "$checksums"; die "Checksum mismatch for $asset"; }
  chmod 0755 "$temp"
  output=$(timeout 5 "$temp" --version 2>&1) || { status=$?; rm -f "$temp" "$checksums"; die "$asset failed its version check (exit $status): ${output:-no output}"; }
  [[ "$output" == "$binary "* ]] || { rm -f "$temp" "$checksums"; die "Downloaded asset is not a compatible $binary binary: $output"; }
  chown 0:0 "$temp"
  chmod 0755 "$temp"
  mv -f "$temp" "$target"
  rm -f "$checksums"
  ok "Installed $output"
}

reset_panel_password() {
  local config=${1:-/etc/voltpanel/config.toml} username=${2:-admin} password confirm
  [[ "$DRY_RUN" == 1 ]] && { log "[dry-run] reset password for $username using $config"; return; }
  [[ -r "$config" ]] || die "Missing panel config: $config"
  [[ -x /usr/local/bin/voltpanel ]] || die "Missing panel binary: /usr/local/bin/voltpanel"
  [[ -r /dev/tty && -w /dev/tty ]] || die "An interactive terminal is required."
  IFS= read -r -s -p "New password for $username: " password < /dev/tty
  printf '\n' > /dev/tty
  IFS= read -r -s -p "Confirm new password: " confirm < /dev/tty
  printf '\n' > /dev/tty
  [[ "$password" == "$confirm" ]] || die "Passwords do not match."
  printf '%s' "$password" | VOLTPANEL_CONFIG="$config" \
    /usr/local/bin/voltpanel reset-password "$username" --password-stdin
  unset password confirm
}

proxy_upstream() {
  local listen=$1 port=${1##*:}
  validate_port "$port"
  case "$listen" in
    0.0.0.0:*) printf '127.0.0.1:%s' "$port" ;;
    \[::\]:*) printf '[::1]:%s' "$port" ;;
    \[*\]:*) printf '%s' "$listen" ;;
    *) printf '%s' "$listen" ;;
  esac
}
random_secret() { openssl rand -base64 "${1:-36}" | tr -d '\n'; }
validate_domain() { [[ $1 =~ ^([A-Za-z0-9-]+\.)+[A-Za-z]{2,}$ ]] || die "Invalid domain: $1"; }
validate_url() {
  local url=$1 scheme rest hostport host port tail colons
  [[ "$url" =~ ^https?://.+$ ]] || die "Invalid URL: $url"
  scheme=${url%%://*}
  rest=${url#*://}
  hostport=${rest%%/*}
  [[ -n "$hostport" && "$hostport" != *[[:space:]]* && "$hostport" != *@* ]] || die "Invalid URL: $url"
  if [[ "$hostport" == \[*\]* ]]; then
    host=${hostport%%\]*}; host=${host#\[}
    valid_ipv6 "$host" || die "Invalid IPv6 address in URL: $url"
    tail=${hostport#*\]}
    if [[ -n "$tail" ]]; then
      [[ "$tail" =~ ^:[0-9]+$ ]] || die "Invalid port in URL: $url"
      port=${tail#:}; validate_port "$port"
    fi
  elif [[ "$hostport" == *:* ]]; then
    colons=${hostport//[^:]/}
    if (( ${#colons} > 1 )); then die "Invalid URL: IPv6 addresses must be bracketed, e.g. https://[$hostport]/ ($url)"; fi
    host=${hostport%:*}; port=${hostport#*:}
    validate_port "$port"
    if valid_ipv4 "$host" || valid_hostname "$host"; then :; else die "Invalid host in URL: $url"; fi
  else
    host=$hostport
    if valid_ipv4 "$host" || valid_hostname "$host"; then :; else die "Invalid host in URL: $url"; fi
  fi
}
valid_hostname() {
  [[ $1 =~ ^([A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)*[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$ && ${#1} -le 253 ]]
}

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
unsafe_purge_path() {
  local path=$1 root canonical
  # Canonicalize so `..`, `//`, trailing slashes and `/.` tricks cannot smuggle
  # rm -rf past the guard. -s keeps symlinks unexpanded so protected names
  # like /bin (a symlink to /usr/bin on merged-usr distros) stay exact.
  # Fail closed: if realpath (coreutils) is missing or errors, `canonical`
  # would stay unnormalized and a `..`-traversal path could slip past the
  # prefix guard below. No rm -rf proceeds without a canonical result.
  if [[ -n "$path" ]] && ! canonical=$(realpath -ms -- "$path" 2>/dev/null); then
    die "coreutils realpath is required to canonicalize '$path' before purge; refusing to continue"
  fi
  [[ -n "$canonical" && "$canonical" != / ]] || return 0
  # Home and root's home are never safe, nor is anything under them.
  case "$canonical" in
    /home|/home/*|/root|/root/*) return 0 ;;
  esac
  # Protected system roots plus the default data-dir parent (/var/lib). A path
  # is unsafe when it is one of these or an ancestor of one: purging it would
  # take the system root with it. Exact-prefix containment only, so a real data
  # dir like /var/lib/voltpanel still purges cleanly.
  local roots=(/bin /boot /dev /etc /lib /lib64 /opt /proc /run /sbin /srv /sys /tmp /usr /var /var/lib)
  for root in "${roots[@]}"; do
    [[ "$canonical" == "$root" || "$root" == "$canonical"/* ]] && return 0
  done
  return 1
}

firewall_hint() {
  local role=$1 mode=${2:-}
  if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q '^Status: active'; then
    if [[ "$role" == panel ]]; then
      run ufw allow 80/tcp; run ufw allow 443/tcp
    elif [[ "$mode" != none ]]; then
      run ufw allow 80/tcp; run ufw allow 443/tcp
      warn "UFW is active. Allow the node's allocated game ports from trusted sources."
    else
      warn "UFW is active. Allow the node's allocated game ports from trusted sources, or run behind a reverse proxy."
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
  if [[ -d "/etc/letsencrypt/live/$domain" ]]; then
    log "Certificate for $domain already present; skipping issuance"
  else
    run certbot "${certbot_args[@]}"
  fi
}

configure_certbot_ip_proxy() {
  local name=$1 ip=$2 upstream=$3 email=$4 webroot=/var/lib/voltpanel/acme server_host
  server_host=$(host_for_url "$ip")
  validate_ip "$ip"
  install_certbot_ip
  require_certbot_ip_support
  run install -d -m 0755 "$webroot"
  write_file "/etc/nginx/conf.d/voltpanel-${name}.conf" 0644 <<EOF
server {
    listen 80;
    listen [::]:80;
    server_name $server_host;

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
    server_name $server_host;
    location ^~ /.well-known/acme-challenge/ { root $webroot; }
    location / { return 308 https://\$host\$request_uri; }
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;
    server_name $server_host;
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
}

systemctl_reload_start() {
  local service=$1 probe=${2:-}
  run systemctl daemon-reload
  run systemctl enable --now "$service"
  if [[ "$DRY_RUN" != "1" ]]; then
    local i code
    for ((i = 0; i < 30; i++)); do
      if systemctl is-active --quiet "$service" 2>/dev/null; then
        if [[ -n "$probe" ]]; then
          if declare -F "$probe" >/dev/null 2>&1; then
            "$probe" && return 0
          else
            code=$(curl -sS -o /dev/null -m 2 -w '%{http_code}' "$probe" 2>/dev/null) || { sleep 1; continue; }
            [[ "$code" =~ ^[0-9]{3}$ ]] && return 0
          fi
        else
          return 0
        fi
      fi
      sleep 1
    done
    systemctl status "$service" --no-pager || true
    journalctl -u "$service" -n 80 --no-pager || true
    die "$service failed to become ready"
  fi
}

# Signed GET /v1/health for a voltd agent. The agent requires HMAC auth on every
# route, so a bare curl can never confirm readiness; this signs with the shared
# secret from the agent config and reports the listener ready on ANY HTTP
# response (a 401/403 still proves the socket accepts). Reads flat TOML keys
# (node_id, secret, listen, plaintext) from the agent config file.
node_health_probe() {
  local config=${1:-} listen=${2:-} node_id secret plaintext scheme port host ts nonce sig payload code
  [[ -n "$config" ]] || config=${VOLTD_CONFIG:-/etc/voltpanel-node/voltd.toml}
  [[ -r "$config" ]] || return 1
  kv() {
    awk -F= -v key="$1" '$1 ~ "^[[:space:]]*" key "[[:space:]]*$" { value=$2; sub(/^[[:space:]]*/, "", value); sub(/[[:space:]]*$/, "", value); gsub(/^"|"$/, "", value); print value; exit }' "$config"
  }
  node_id=$(kv node_id); secret=$(kv secret); plaintext=$(kv plaintext)
  [[ -n "$node_id" && -n "$secret" ]] || return 1
  [[ -n "$listen" ]] || listen=$(kv listen)
  [[ -n "$listen" ]] || return 1
  port=${listen##*:}
  [[ "$port" =~ ^[0-9]+$ ]] || return 1
  host=127.0.0.1; [[ "$listen" == \[* ]] && host='[::1]'
  scheme=https; [[ "$plaintext" == true ]] && scheme=http
  ts=$(date +%s)
  nonce=$(openssl rand -hex 16)
  payload="GET
/v1/health
$ts
$nonce
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
  sig=$(printf '%s' "$payload" | openssl dgst -sha256 -hmac "$secret" 2>/dev/null | awk '{print $NF}')
  [[ -n "$sig" ]] || return 1
  code=$(curl -ksS -o /dev/null -m 3 -w '%{http_code}' \
    -H "x-volt-node: $node_id" \
    -H "x-volt-timestamp: $ts" \
    -H "x-volt-nonce: $nonce" \
    -H "x-volt-signature: $sig" \
    "$scheme://$host:$port/v1/health" 2>/dev/null) || return 1
  [[ "$code" =~ ^[0-9]{3}$ ]]
}

# Remove every stale reverse-proxy artifact belonging to a named install
# (caddy conf, nginx conf, certbot systemd unit/timer, renewal hook, Cloudflare
# TLS files) so re-running the installer with a different TLS mode leaves no
# orphaned site, timer, or key material behind. Idempotent; dry-run safe.
cleanup_proxy_artifacts() {
  local name=$1 u had_nginx='' had_caddy='' had_unit=''
  if [[ -e "/etc/nginx/conf.d/voltpanel-${name}.conf" ]]; then had_nginx=1; run rm -f "/etc/nginx/conf.d/voltpanel-${name}.conf"; fi
  if [[ -e "/etc/caddy/conf.d/voltpanel-${name}.caddy" ]]; then had_caddy=1; run rm -f "/etc/caddy/conf.d/voltpanel-${name}.caddy"; fi
  if [[ -e "/etc/letsencrypt/renewal-hooks/deploy/voltpanel-${name}-nginx" ]]; then run rm -f "/etc/letsencrypt/renewal-hooks/deploy/voltpanel-${name}-nginx"; fi
  if [[ -e "/etc/voltpanel/tls/${name}-cloudflare.pem" ]]; then run rm -f "/etc/voltpanel/tls/${name}-cloudflare.pem"; fi
  if [[ -e "/etc/voltpanel/tls/${name}-cloudflare.key" ]]; then run rm -f "/etc/voltpanel/tls/${name}-cloudflare.key"; fi
  for u in "voltpanel-certbot-${name}.service" "voltpanel-certbot-${name}.timer"; do
    if [[ -e "/etc/systemd/system/$u" ]]; then run rm -f "/etc/systemd/system/$u"; had_unit=1; fi
    if systemctl is-enabled --quiet "$u" 2>/dev/null; then run systemctl disable "$u" >/dev/null 2>&1 || true; had_unit=1; fi
  done
  [[ "$had_unit" == 1 ]] && run systemctl daemon-reload
  if [[ "$had_nginx" == 1 ]] && systemctl is-active --quiet nginx 2>/dev/null; then run nginx -t && run systemctl try-reload nginx; fi
  if [[ "$had_caddy" == 1 ]] && systemctl is-active --quiet caddy 2>/dev/null; then
    run caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null 2>&1 && run systemctl try-reload caddy
  fi
}

# ---------------------------------------------------------------------------
# Ops restore + diagnostics bundle helpers
# ---------------------------------------------------------------------------

# Ceiling of the SQLite schema ladder this release's binary supports. Restore
# refuses an archive whose manifest declares a newer user_version. UPDATE
# CONTRACT: bump to the final `PRAGMA user_version = N` in src/db.rs migrate()
# whenever a migration lands. backup/restore ship from the same release as the
# binary (install-panel.sh installs both), so this stays in lockstep with it.
SCHEMA_VERSION_MAX="${SCHEMA_VERSION_MAX:-20}"

# PRAGMA user_version of a SQLite file; prints 0 (and returns nonzero) when
# the file is absent or unreadable.
sqlite_user_version() {
  local db=$1
  [[ -r "$db" ]] || { printf '0'; return 1; }
  sqlite3 -readonly "$db" 'PRAGMA user_version;' 2>/dev/null || { printf '0'; return 1; }
}

sqlite_integrity_ok() {
  local db=$1
  [[ -r "$db" ]] || return 1
  [[ "$(sqlite3 -readonly "$db" 'PRAGMA integrity_check;' 2>/dev/null)" == ok ]]
}

# Mask secret-bearing TOML values on stdin (`key = "value"` lines whose key
# contains secret/password/token/key/signature/master-key). Values are
# replaced with "REDACTED"; keys stay visible so the diagnostics stay
# readable. The second expression covers inline `key=value` tokens.
redact_config() {
  sed -E \
    -e 's/([[:space:]]*[A-Za-z0-9_.-]*(secret|password|passwd|token|api[_-]?key|private[_-]?key|master[_-]?key|sig(nature)?)[A-Za-z0-9_.-]*[[:space:]]*=[[:space:]]*).*/\1"REDACTED"/I' \
    -e 's/(secret|password|passwd|token|api[_-]?key|private[_-]?key|master[_-]?key|sig(nature)?)=[^[:space:]"]+/\1="REDACTED"/Ig'
}

# Ask before a destructive step. --force bypasses the prompt; a non-interactive
# shell cannot confirm, so it fails instead of proceeding silently.
confirm_or_force() {
  local force=$1 prompt=$2
  if [[ "$force" == 1 ]]; then return 0; fi
  tui_available || die "$prompt (non-interactive shell; pass --force to skip this prompt)"
  tui_yesno "$prompt"
}

# Unique backup stamp: second-resolution date, numerically suffixed on
# collision so rapid repeated backups never overwrite each other.
unique_stamp() {
  local stamp=$1 dir=${2:-$BACKUP_DIR} n=1
  while [[ -e "$dir/panel-$stamp.tar.gz" ]]; do stamp="${1}-${n}"; n=$((n + 1)); done
  printf '%s' "$stamp"
}

# Keep the newest `keep` panel-*.tar.gz backups (plus their .sha256 files).
prune_backups() {
  local dir=$1 keep=$2 f
  local -a old=()
  while IFS= read -r f; do [[ -n "$f" ]] && old+=("$f"); done < <(
    find "$dir" -maxdepth 1 -name 'panel-*.tar.gz' -printf '%T@ %p\n' 2>/dev/null \
      | sort -nr | awk -v keep="$keep" 'NR > keep { $1=""; sub(/^ /, ""); print }'
  )
  for f in "${old[@]}"; do
    printf 'pruning old backup: %s\n' "$(basename "$f")"
    rm -f -- "$f" "${f%.tar.gz}.sha256"
  done
}

# Structural safety of a restore archive: reject absolute paths, `..`
# components, non-regular members (symlink/hardlink/device/fifo) and unknown
# top-level names, so extraction can never escape data_dir or config_dir.
# Archive layout is fixed by `voltpanel-manage backup`: manifest.json,
# voltpanel.db, servers/ backups/ blueprints/ websites/ datalab/, config/.
validate_archive() {
  local archive=$1 line type found=0
  while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    found=1
    while [[ "$line" == */ ]]; do line=${line%/}; done
    case "$line" in
      manifest.json|voltpanel.db|servers|backups|blueprints|websites|datalab|config) ;;
      servers/*|backups/*|blueprints/*|websites/*|datalab/*|config/*) ;;
      *)
        printf 'archive contains unexpected entry: %s\n' "$line" >&2
        return 1
        ;;
    esac
    [[ "$line" != *..* && "$line" != /* && "$line" != ./* ]] || {
      printf 'unsafe path in archive: %s\n' "$line" >&2
      return 1
    }
  done < <(tar -tzf "$archive" 2>/dev/null) || {
    printf 'unreadable archive: %s\n' "$archive" >&2
    return 1
  }
  (( found == 1 )) || { printf 'empty archive: %s\n' "$archive" >&2; return 1; }
  tar -tzf "$archive" 2>/dev/null | grep -qx manifest.json || {
    printf 'archive lacks manifest.json: %s\n' "$archive" >&2
    return 1
  }
  # Second pass for member types and permissions: only regular files and
  # directories may be restored. Symlinks are refused (a symlink member
  # followed by a member through it could redirect extraction outside the
  # restore root), and regular members carrying setuid/setgid/sticky or
  # world-writable modes are refused (privilege escalation on install).
  while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    type=${line:0:1}
    case "$type" in
      -|d) ;;
      *)
        printf 'archive member is not a regular file or directory (type %s)\n' "$type" >&2
        return 1
        ;;
    esac
    if [[ "$type" == - ]]; then
      # tar -tv mode field: "-rwxr-xr-x" — setuid at 3, setgid at 6,
      # world-writable at 8, sticky at 9.
      perms=${line:0:10}
      if [[ "${perms:3:1}" == [sS] || "${perms:6:1}" == [sS] || "${perms:8:1}" == w || "${perms:9:1}" == [tT] ]]; then
        printf 'archive member has unsafe permissions: %s\n' "$line" >&2
        return 1
      fi
    fi
  done < <(tar -tvzf --quoting-style=shell "$archive" 2>/dev/null)
}

# Read a value from a manifest.json (bare number or quoted string).
manifest_value() {
  local key=$1 manifest=$2 value
  value=$(printf '%s\n' "$manifest" | grep -F "\"$key\"" | head -n1)
  [[ -n "$value" ]] || { printf ''; return 1; }
  value=${value#*:}
  printf '%s' "$value" | tr -d '[:space:]",'
}
