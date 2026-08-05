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
- One node API domain or a private panel-reachable address

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

```bash
curl -fsSL https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main/scripts/install-panel.sh \
  | sudo bash -s -- \
      --domain panel.example.com \
      --email admin@example.com
```

The random admin password is printed once. Save it and change it immediately.

### Installer options

```text
--domain DOMAIN       Install Caddy and enable automatic HTTPS
--email EMAIL         ACME contact email
--listen ADDRESS      Explicit origin listen address
--public              Listen directly on 0.0.0.0:8080
--no-caddy            Skip Caddy installation
--data-dir PATH       Panel data directory
--version VERSION     Install a release tag instead of latest
--dry-run             Print actions only
```

### Verify

```bash
sudo voltpanel-manage doctor
sudo voltpanel-manage status
sudo journalctl -u voltpanel -n 100 --no-pager
curl -I https://panel.example.com
```

## Install a node

Create a node from **Admin → Nodes** first. Copy its enrollment token.

```bash
curl -fsSL https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main/scripts/install-node.sh \
  | sudo bash -s -- \
      --panel https://panel.example.com \
      --token TOKEN \
      --domain node1.example.com \
      --email admin@example.com
```

### Private LAN node

```bash
curl -fsSL https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main/scripts/install-node.sh \
  | sudo bash -s -- \
      --panel http://192.168.1.10:8080 \
      --token TOKEN \
      --public-url http://192.168.1.11:8081 \
      --allow-http \
      --no-caddy
```

`--allow-http` must only be used on a trusted private network. HMAC protects integrity and authenticity, but only TLS protects console/file/snapshot confidentiality.

### Node installer options

```text
--panel URL           Panel URL (required)
--token TOKEN         One-time enrollment token (required)
--domain DOMAIN       Configure node HTTPS with Caddy
--public-url URL      URL stored by the panel
--listen ADDRESS      voltd listen address
--allow-http          Explicitly permit non-loopback HTTP enrollment
--no-caddy            Skip Caddy
--data-dir PATH       Workload data path
--version VERSION     Release tag
--dry-run             Print actions only
```

### Verify node

```bash
sudo voltd-manage doctor
sudo voltd-manage status
sudo journalctl -u voltd -n 100 --no-pager
```

The node should become online in the panel within 15 seconds.

## Firewall

With Caddy:

```bash
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw allow 20000:30000/tcp
sudo ufw allow 20000:30000/udp
```

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
