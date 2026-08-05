# Operations

## Services

```bash
systemctl status voltpanel
systemctl status voltd
journalctl -u voltpanel -f
journalctl -u voltd -f
```

Management wrappers:

```bash
voltpanel-manage status|logs|doctor|backup|upgrade
voltd-manage status|logs|doctor|upgrade
```

## Health checks

Panel health:

```bash
curl -fsS http://127.0.0.1:8080/api/system/health
```

Authenticated administrators can inspect `/api/system/isolation` for namespace/cgroup capabilities.

Node health is signed and normally queried through **Admin → Nodes → Test**.

## Backup the panel

SQLite uses WAL mode. Do not copy only `voltpanel.db` while running.

Use:

```bash
sudo voltpanel-manage backup
```

This uses SQLite's online backup command and archives server-related panel files.

Store backups outside the panel host when possible.

## Server backups

Server backups are SHA-256 verified. Remote backups use a node snapshot and store the resulting archive in the panel backup directory.

Before restore:

- Stop the workload
- Verify available disk
- Confirm the backup checksum
- Keep a second known-good backup

## Logs

Panel and node logs go to journald:

```bash
journalctl -u voltpanel --since today
journalctl -u voltd --since today
```

Per-server console logs are under the configured log directory.

## Resource operations

- Memory and CPU changes apply to the next workload start/restart
- Port allocation is node-scoped and transactional
- Transfers validate target capacity and port conflicts
- Maintenance mode prevents placement
- `schedulable=false` prevents automatic placement without disabling management

## Monitoring recommendations

Alert on:

- Node heartbeat older than 45 seconds
- Memory above 85%
- Disk above 85%
- Repeated crash/restart count
- OOM events
- Snapshot/transfer failures
- Isolation health not secure

## First response to node failure

1. Check `voltd-manage status`
2. Run `voltd-manage doctor`
3. Verify panel↔node HTTPS and DNS
4. Check clock synchronization
5. Inspect disk/memory pressure
6. Check nftables and veth interfaces
7. Do not regenerate enrollment tokens unless secret recovery is required

## Service hardening

Installed units use:

- `Delegate=yes`
- Restricted capability bounding set
- `NoNewPrivileges=yes`
- `ProtectSystem=strict`
- `ProtectHome=yes`
- `PrivateTmp=yes`
- UMask 0077

Do not remove cgroup delegation; workload limits and cleanup depend on it.
