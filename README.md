# VoltPanel

VoltPanel is a free, self-hosted Linux workload platform for games, websites, bots, and application services. It is designed around an original operational model rather than reproducing another hosting panel's screens or terminology.

- `voltpanel` — control plane, Pulse UI, Flow engine, placement, identity, and state
- `voltd` — execution agent for isolated workloads, storage, terminal streams, telemetry, Vault snapshots, and transfers

The platform uses SQLite plus Linux-native isolation (`bubblewrap`, namespaces, unique UIDs, cgroup v2, veth, and nftables). Docker is not part of the runtime path.

## Native product model

VoltPanel's public language and workflows are its own:

- **Pulse** — live operational posture, workload health, capacity pressure, and Signals
- **Workspaces** — isolated environments owned by a person or team
- **Fabric** — the mesh of local and remote `voltd` execution agents
- **VoltSpec Blueprints** — portable launch plans, validated inputs, setup logic, and defaults
- **Flows** — scheduled or event-driven lifecycle, command, and snapshot pipelines
- **Vault** — verified snapshots, restore points, and workspace transfers
- **Data Lab** — workspace-scoped SQLite exploration with an authorizer sandbox
- **Observatory** — platform capacity, endpoint reservations, and security posture

Games are one workload type—not the shape of the product. Sites, bots, proxies, workers, and custom language runtimes use the same blueprint and fabric model.

## Highlights

- Execution Fabric with one-command agent enrollment
- Capacity-aware placement using agent tags, locations, health, and maintenance state
- Fail-closed workload isolation with cgroup v2 and private Linux namespaces
- Workspace-scoped endpoint reservations rather than generic server allocations
- Unified Terminal, Storage, Data Lab, Vault, Flow, and Signals experiences
- Portable VoltSpec blueprints with validated inputs and sandboxed setup plans
- Argon2id, TOTP, hashed sessions, scoped API credentials, and team permissions
- Original responsive Pulse UI, Blueprint Studio, agent mesh, and command palette
- No PHP, Redis, MySQL, Docker, or paid service required

## Supported systems

- Ubuntu 22.04/24.04
- Debian 11/12
- Fedora, RHEL, Rocky Linux and AlmaLinux
- Arch Linux
- Architectures: x86_64 and arm64
- systemd and cgroup v2 are required

## Interactive installation

Run the installer from a real terminal. With no arguments it opens a TUI wizard for the domain, storage path, and HTTPS mode:

```bash
curl -fsSL https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main/scripts/install-panel.sh -o /tmp/install-panel.sh
sudo bash /tmp/install-panel.sh
```

Available HTTPS modes:

- **Caddy automatic HTTPS** — recommended for a public hostname.
- **Certbot + Nginx (domain)** — provisions and renews a regular Let's Encrypt certificate.
- **Certbot + Nginx (public IP)** — provisions a publicly trusted certificate without a domain. It requires a directly reachable public IPv4/IPv6 and open ports 80/443. Certificates last about six days, so the installer configures renewal checks every 12 hours.
- **Cloudflare Origin Certificate** — uses a certificate and private key created in Cloudflare; set Cloudflare SSL/TLS mode to **Full (strict)**.
- **No reverse proxy** — intended only for a trusted LAN.

For unattended automation, pass `--non-interactive` with explicit options:

```bash
sudo bash /tmp/install-panel.sh --non-interactive \
  --tls certbot --domain panel.example.com --email admin@example.com
```

Public-IP certificate example:

```bash
sudo bash /tmp/install-panel.sh --non-interactive \
  --tls certbot-ip --ip-address 203.0.113.10 --email admin@example.com
```

The `certbot-ip` mode installs Certbot 5.4 or newer because older distro packages cannot request IP certificates. It uses Let's Encrypt's mandatory `shortlived` profile; do not disable its systemd renewal timer.

Cloudflare example:

```bash
sudo bash /tmp/install-panel.sh --non-interactive \
  --tls cloudflare --domain panel.example.com \
  --cloudflare-cert /root/panel-origin.pem \
  --cloudflare-key /root/panel-origin.key
```

The installer installs dependencies and the release binary, creates private data/configuration directories, generates secrets and a random first-admin password, installs a hardened systemd service, configures the selected TLS proxy, then starts VoltPanel. The initial password is printed once.

### LAN-only installation

```bash
sudo bash /tmp/install-panel.sh --non-interactive --tls none --public
```

Open `http://SERVER_IP:8080`. Public internet deployments should use HTTPS, not direct port 8080.

## Add a node

1. Open **Control Center → Fabric → Attach agent**.
2. Enter the node name, location and public URL.
3. Copy its one-time enrollment token.
4. Download and run the node wizard on the node host:

```bash
curl -fsSL https://raw.githubusercontent.com/HitamLegit6777/voltpanel/main/scripts/install-node.sh -o /tmp/install-node.sh
sudo bash /tmp/install-node.sh
```

The node wizard provides the same Caddy, domain Certbot, public-IP Certbot, Cloudflare, and LAN-only modes. For a trusted private LAN without TLS, the wizard enables HTTP enrollment explicitly; automated installs must pass `--allow-http`.


## Management commands

Panel:

```bash
sudo voltpanel-manage status
sudo voltpanel-manage logs
sudo voltpanel-manage doctor
sudo voltpanel-manage backup
sudo voltpanel-manage upgrade
```

Node:

```bash
sudo voltd-manage status
sudo voltd-manage logs
sudo voltd-manage doctor
sudo voltd-manage upgrade
```

## Build from source

```bash
git clone https://github.com/HitamLegit6777/voltpanel.git
cd voltpanel
cargo build --release --bins
cp config.example.toml config.toml
./target/release/voltpanel
```

A fresh database prints a random one-time `admin` password. To provide one for automated provisioning only:

```bash
VOLTPANEL_ADMIN_PASSWORD='temporary-strong-password' ./target/release/voltpanel
```

## Architecture

```mermaid
flowchart LR
  U[Browser / API client] --> P[voltpanel]
  P --> DB[(SQLite WAL)]
  P --> N1[voltd node A]
  P --> N2[voltd node B]
  N1 --> S1[Isolated server]
  N1 --> S2[Isolated server]
  N2 --> S3[Isolated server]
```

Panel↔node requests use HMAC-SHA256 signatures, body hashes, timestamps and one-time nonces. HTTPS provides transport confidentiality.

Each workload runs with:

- Private mount, PID, IPC, UTS and network namespaces
- Collision-free private host UID/GID
- Empty capability bounding set and `no_new_privs`
- Read-only runtime mounts and one writable server root
- Private veth and nftables policy
- Node/LAN access blocked, outbound internet allowed
- Only allocated ports exposed
- cgroup memory/CPU/PID limits and systemd delegated scopes

## Documentation

- [Installation](docs/INSTALLATION.md)
- [Execution Fabric operations](docs/MULTI_NODE.md)
- [Security and isolation](docs/SECURITY.md)
- [Operations and backups](docs/OPERATIONS.md)
- [Upgrade and uninstall](docs/UPGRADE.md)
- [Troubleshooting](docs/TROUBLESHOOTING.md)

## Default paths

| Component | Path |
|---|---|
| Control-plane binary | `/usr/local/bin/voltpanel` |
| Execution-agent binary | `/usr/local/bin/voltd` |
| Control-plane config | `/etc/voltpanel/config.toml` |
| Execution-agent config | `/etc/voltpanel-node/voltd.toml` |
| Control-plane data | `/var/lib/voltpanel` |
| Execution-agent data | `/var/lib/voltd` |
| Control-plane service | `voltpanel.service` |
| Execution-agent service | `voltd.service` |

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md), follow the [Code of Conduct](CODE_OF_CONDUCT.md), and use the GitHub issue/PR templates. Report vulnerabilities through private Security Advisories as described in [SECURITY.md](SECURITY.md).

## Ports

| Port | Purpose | Exposure |
|---|---|---|
| 80/443 | Caddy HTTP/HTTPS | Public when using a domain |
| 8080 | Control-plane origin | Loopback behind Caddy; LAN only otherwise |
| 8081 | Execution-agent API | Private control-plane reachability only |
| 20000–30000 | Suggested workspace endpoint range | Per provider policy |

## Security policy

Please report vulnerabilities privately through GitHub Security Advisories. Do not publish active exploit details before a fix is available.

## License

MIT. See [LICENSE](LICENSE).
