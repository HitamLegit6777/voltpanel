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

Node health is signed and normally queried through **Control Center → Fabric → Test**.

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
- Endpoint reservation is agent-scoped and transactional
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

## Signals (outbound webhooks)

Signals POST a JSON envelope to an external URL when a subscribed event fires.

- Subscriptions match on event name, with `*` as a trailing wildcard (`backup.*`)
- A webhook with no `server_id` is global; a scoped one only fires for its workload
- The dispatcher claims due deliveries every 5 seconds, 50 per sweep
- `2xx` marks the delivery `delivered`; anything else reschedules with exponential
  backoff until the attempt cap, then `failed`
- The panel DB lock is never held across the outbound HTTP call

Each request carries:

| Header | Meaning |
| --- | --- |
| `X-VoltPanel-Event` | event name |
| `X-VoltPanel-Delivery` | delivery id, stable across retries |
| `X-VoltPanel-Timestamp` | unix seconds used in the signature |
| `X-VoltPanel-Signature` | `sha256=` HMAC over `<timestamp>.<body>` |

Verify with the webhook secret, which is returned **only** in the create response
and never re-exposed by the list endpoint:

```
expected = "sha256=" + hmac_sha256(secret, timestamp + "." + raw_body)
```

Use `POST /api/webhooks/{id}/test` to queue a `test.ping` straight to one webhook,
bypassing subscription matching, then read `GET /api/webhooks/{id}/deliveries` to
inspect attempt count, response code, and error text.

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
