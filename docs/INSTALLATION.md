# Installation

This guide covers production panel and node installation. The scripts support Debian/Ubuntu, Fedora/RHEL/Rocky/AlmaLinux and Arch Linux.

## Requirements

### Panel host

- Linux with systemd and cgroup v2
- Root/sudo access
- x86_64 or arm64
- 1 GB RAM minimum; 2 GB recommended
- A domain pointing to the host for automatic HTTPS

### Node host

- Linux with systemd and cgroup v2
- Root/sudo access
- `bubblewrap`, `setpriv`, `iproute2`, `nftables`
- Kernel namespaces enabled
- Enough CPU/RAM/disk for workloads
- One execution-agent domain or a private panel-reachable address

Check cgroup v2:

```bash
stat -fc %T /sys/fs/cgroup
# expected: cgroup2fs
```

## DNS preparation

Create DNS records before running domain installation:

```text
panel.example.com  A     PANEL_IPV4
node1.example.com  A     NODE1_IPV4
```

If IPv6 is reachable, add matching `AAAA` records. Ports 80 and 443 must reach Caddy for ACME certificate issuance.

## Install the panel

Download the script, then launch it from a real terminal to use the TUI wizard:

```bash
curl -fsSL https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main/scripts/install-panel.sh -o /tmp/install-panel.sh
sudo bash /tmp/install-panel.sh
```

The wizard offers Caddy automatic HTTPS, Certbot with Nginx for a domain, Certbot with Nginx for a public IP, a Cloudflare Origin Certificate, or LAN-only HTTP. The random admin password is printed once. On an existing installation, the wizard switches to a management menu for reinstall, password reset, safe uninstall, or full purge.

For a Cloudflare setup, create an Origin Certificate in Cloudflare, copy its PEM and private key to the host, select **Cloudflare Origin Certificate**, then set the zone SSL/TLS encryption mode to **Full (strict)**. Keep both origin key files private.

Public-IP certificates require a directly reachable IPv4/IPv6 and ports 80 and 443. The installer installs Certbot 5.4 or newer, requests Let's Encrypt's mandatory `shortlived` profile, configures Nginx manually, and checks renewal every 12 hours. IP certificates expire after approximately six days; do not disable the generated renewal timer.

### Unattended installer options

```text
--reinstall             Reinstall binary/service; preserve config and data
--uninstall             Remove application; preserve config and data
--uninstall --purge     Remove application, config, and data permanently
--reset-password [USER] Reset a password; default user is admin
--domain DOMAIN          Public panel domain
--email EMAIL            ACME contact email
--tls MODE               caddy, certbot, certbot-ip, cloudflare, or none
--ip-address IP          Public IPv4 or IPv6 for certbot-ip
--cloudflare-cert PATH   Cloudflare Origin Certificate PEM
--cloudflare-key PATH    Cloudflare Origin private key
--port PORT              Internal/direct panel port (default 8080)
--listen ADDRESS         Explicit origin listen address; overrides --port
--public                 Listen directly on 0.0.0.0 when TLS is disabled
--no-caddy               Alias for --tls none
--non-interactive        Disable the TUI wizard
--data-dir PATH          Panel data directory
--version VERSION        Install a release tag instead of latest
--dry-run                Print actions only
```

The TUI asks for the panel port. With TLS, this is the loopback origin port behind Caddy/Nginx; users still connect on HTTPS port 443. Without TLS, it is the public port. `--port` and `--listen` are mutually exclusive.

Example:

```bash
sudo bash /tmp/install-panel.sh --non-interactive \
  --tls certbot --domain panel.example.com --email admin@example.com
```

Public-IP example:

```bash
sudo bash /tmp/install-panel.sh --non-interactive \
  --tls certbot-ip --ip-address 203.0.113.10 --email admin@example.com
```

### Verify

```bash
sudo voltpanel-manage doctor
sudo voltpanel-manage status
sudo journalctl -u voltpanel -n 100 --no-pager
curl -I https://panel.example.com
```

## Install a node

Create an agent from **Control Center → Fabric** first and copy its enrollment token. Then launch the node wizard:

```bash
curl -fsSL https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main/scripts/install-node.sh -o /tmp/install-node.sh
sudo bash /tmp/install-node.sh
```

The wizard asks for the panel URL and enrollment token and provides the same five TLS modes as the panel installer.

Re-enrolling an installed systemd node requires a new token from **Control Center → Fabric**. Stop `voltd`, run `voltd join PANEL_URL TOKEN --public-url NODE_URL --no-start` with the existing node options, then start the service. Generating the token revokes the old shared secret immediately; the pinned TLS fingerprint is kept, so re-enrolling with the same certificate fingerprint is accepted (a different fingerprint is refused — delete and recreate the node to change it).

### Private LAN node

```bash
sudo bash /tmp/install-node.sh --non-interactive \
  --panel https://panel.example.com \
  --token TOKEN \
  --public-url https://node.example.com \
  --tls caddy
```

Enrollment requires TLS end to end: the panel must be reachable over positively-TLS transport — native panel TLS, or a trusted TLS-terminating proxy (Caddy/Nginx) in front of the panel — and the node agent must present a certificate fingerprint for the panel to pin. The panel refuses plaintext transport (403) and fingerprint-less enrollments (400), so a private-network node still enrolls over TLS. `--allow-http` no longer permits plaintext enrollment; it only fits loopback-local development.

### Execution-agent installer options

```text
--panel URL              Panel URL
--token TOKEN            One-time enrollment token
--domain DOMAIN          Public node domain
--tls MODE               caddy, certbot, certbot-ip, cloudflare, or none
--ip-address IP          Public IPv4 or IPv6 for certbot-ip
--cloudflare-cert PATH   Cloudflare Origin Certificate PEM
--cloudflare-key PATH    Cloudflare Origin private key
--public-url URL         URL stored by the panel
--port PORT              Internal/direct node port (default 8081)
--listen ADDRESS         voltd listen address; overrides --port
--allow-http             Permit --tls none only for loopback-local development (the panel refuses plaintext enrollment)
--no-caddy               Alias for --tls none
--non-interactive        Disable the TUI wizard
--data-dir PATH          Workload data path
--version VERSION        Release tag
--dry-run                Print actions only
```

The node TUI also asks for its port. Reverse proxies automatically target the selected port.

### Verify node

```bash
sudo voltd-manage doctor
sudo voltd-manage status
sudo journalctl -u voltd -n 100 --no-pager
```

The node should become online in the panel within 15 seconds.

## Firewall

Panel host with Caddy:

```bash
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
```

The game-port range (20000-30000) belongs on node hosts, where the panel allocates it per server; it is not needed on the panel host.

Keep 8080/8081 private when reverse-proxied. If a node origin is directly exposed, restrict 8081 to the panel IP.

Example nftables concept:

```nft
ip saddr PANEL_IP tcp dport 8081 accept
tcp dport 8081 drop
tcp dport 20000-30000 accept
udp dport 20000-30000 accept
```

Never flush an existing production firewall blindly. Merge rules with the host's existing policy.

## Build manually

```bash
git clone https://github.com/HitamLegit6777/voltpanel.git
cd voltpanel
cargo test
cargo build --release --bins
```

Install binaries:

```bash
sudo install -m755 target/release/voltpanel /usr/local/bin/voltpanel
sudo install -m755 target/release/voltd /usr/local/bin/voltd
```

Templates are available in `deploy/systemd/` and `deploy/caddy/`.
