# Troubleshooting

## Panel will not start

```bash
sudo systemctl status voltpanel --no-pager
sudo journalctl -u voltpanel -n 100 --no-pager
sudo voltpanel-manage doctor
```

Common causes:

- Invalid TOML configuration
- Port 8080 already used
- Data/config permissions incorrect
- SQLite migration/integrity failure

Check ports:

```bash
sudo ss -ltnp | grep ':8080'
```

## Node remains offline

```bash
sudo voltd-manage doctor
sudo journalctl -u voltd -n 100 --no-pager
```

Verify:

- Node URL resolves from the panel
- TLS certificate is valid
- Caddy points to 127.0.0.1:8081
- Node clock is synchronized
- Enrollment secret matches
- Node is enabled

Test from panel host:

```bash
curl -I https://node1.example.com/v1/health
```

An unsigned response may be unauthorized; transport reachability is what matters.

## Workload refuses to start

VoltPanel fails closed when isolation cannot be configured.

```bash
command -v bwrap setpriv ip nft systemd-run
stat -fc %T /sys/fs/cgroup
systemctl show voltpanel -p Delegate
systemctl show voltd -p Delegate
```

Expected:

```text
cgroup2fs
Delegate=yes
```

Inspect the error in the panel notification/audit feed and service logs.

## Port conflict

Ports are unique per node. A conflict returns a 400 error and does not create the server.

Find endpoint reservations:

```bash
sqlite3 /var/lib/voltpanel/voltpanel.db \
  'SELECT servers.name,servers.node,allocations.port FROM allocations JOIN servers ON servers.id=allocations.server_id ORDER BY servers.node,allocations.port;'
```

Do not edit the DB manually unless the panel is stopped and a backup exists.

## Workload has no internet

Check veth and nftables:

```bash
ip link show | grep 'vp'
nft list tables | grep 'table ip vp'
sysctl net.ipv4.ip_forward
```

The policy intentionally blocks node/LAN/private ranges but permits public egress and established replies.

## Domain HTTPS fails

```bash
sudo systemctl status caddy --no-pager
sudo journalctl -u caddy -n 100 --no-pager
sudo caddy validate --config /etc/caddy/Caddyfile
```

Confirm DNS resolves to the correct public IP and ports 80/443 are reachable.

## Upload/transfer returns 413

Increase:

```toml
[web]
max_body_mb = 512
```

Restart panel. Node `max_upload_mb` is stored in `/etc/voltpanel-node/voltd.toml`; snapshot bodies are base64 and require headroom.

## File operation says symlink traversal rejected

Remote file APIs intentionally reject symlinks because the execution agent runs with host privileges. Replace the symlink with a real file/directory inside the server root.

## Lost admin password

Stop the panel, back up the DB, then use SQLite only as a last resort. Prefer creating a temporary recovery tool/build rather than deleting authentication rows. Do not replace Argon2 hashes with plaintext.

For a fresh installation, the one-time password is printed by the installer and the first-start journal:

```bash
sudo journalctl -u voltpanel | grep 'FIRST-RUN ADMIN CREDENTIAL'
```

Change it immediately after login.
