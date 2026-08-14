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
voltpanel-manage status|logs|doctor|backup|restore|upgrade
voltd-manage status|logs|doctor|upgrade
```

`voltpanel-manage restore ARCHIVE [--force]` restores a panel backup archive
created by `backup`; `doctor --bundle PATH` writes a redacted diagnostics
archive (see below).

## Health checks

Panel health is checked using the listener configured in `/etc/voltpanel/config.toml`:

```bash
sudo voltpanel-manage status
```

Authenticated administrators can inspect `/api/system/isolation` for namespace/cgroup capabilities.

Node health is signed and normally queried through **Control Center → Fabric → Test**.

## Backup the panel

SQLite uses WAL mode. Do not copy only `voltpanel.db` while running.

Use:

```bash
sudo voltpanel-manage backup
```

This writes `panel-<timestamp>.tar.gz` (plus `panel-<timestamp>.sha256`) into
`/var/backups/voltpanel` (override with `$VOLTPANEL_BACKUP_DIR`). Each archive
contains:

- `voltpanel.db` — SQLite **online backup** artifact, a consistent snapshot
  even while the service is running
- `servers/`, `backups/`, `blueprints/`, `websites/`, `datalab/` — panel data
  directories present on the host
- `config/` — the full config directory (`config.toml`, `tls/`, `first-run.env`)
- `manifest.json` — `format_version`, the SQLite `user_version`, and a per-file
  SHA-256 checksum map used to verify the archive before any restore

The `.sha256` file holds the archive's own checksum. The 10 newest backups are
retained; older ones are pruned automatically. Store backups outside the panel
host when possible.

## Restore the panel

```bash
sudo voltpanel-manage restore /var/backups/voltpanel/panel-2026-08-10-161500.tar.gz
sudo voltpanel-manage restore /var/backups/voltpanel/panel-2026-08-10-161500.tar.gz --force
```

Restore validates the archive structure first (no absolute paths, `..`
traversal, symlink/hardlink members, or unknown top-level entries), checks the
archive checksum against its `.sha256` companion when present, and refuses
archives whose schema is newer than the installed binary supports. It then
stops the service (after a confirmation prompt; `--force` skips the prompt),
snapshots the current state, re-verifies every extracted file against the
manifest checksums, restores the database and directories, runs `check-config`
and `PRAGMA integrity_check`, and starts the service. Any failure before the
service is confirmed healthy rolls the previous state back automatically.

Restoring overwrites the current database, data directories, and config, and
removes files added since the backup. Create a fresh backup first, and stop
local workloads beforehand (or pass `--force`). Archives created by older
`backup` versions (`panel-*.db` / `files-*.tar.gz`) cannot be restored with
this command; keep them until the panel has produced new-format backups.

## Site gateway (host-routed vhosts)

VoltPanel can publish website records (per-workspace domains, static roots or
reverse proxies) through an embedded gateway on its own listener, configured
under `[sites]`:

```toml
[sites]
listen = "0.0.0.0:8081"   # unset/absent = gateway disabled
trusted_proxies = []      # CIDRs allowed to set X-Forwarded-Proto
```

The gateway resolves each request's `Host` header against enabled website
records and either serves the site's static root or reverse-proxies its
upstream. Point your DNS/Caddy/Traefik at this listener, not at the admin
panel's `web.listen`.

Security model:


- **Static roots** are pinned under `paths.website_dir/server_<id>`; the URL
  path is joined with a zip-slip-safe joiner that rejects `..` escapes and
  symlinks, so a site can never read outside its own root. Symlinked files
  inside a root are not served.
- **Reverse-proxy upstreams** are SSRF-gated at request time: the target
  must resolve to a loopback or private (RFC 1918 / IPv6 ULA) address.
  Public-IP upstreams are refused with 502 — the gateway is not an open
  proxy to the internet or to cloud metadata endpoints. Redirects are never
  followed. Keep local workloads on loopback/private addresses.
- **`force_https`** sites are redirected with 308 unless the request is
  HTTPS. The gateway terminates plain HTTP, so "HTTPS" means the socket peer
  is inside `sites.trusted_proxies` and the request carries
  `X-Forwarded-Proto: https`. With no trusted proxies configured the header
  is never trusted and every plain request redirects.
- `GET /__volt/health` is answered before host dispatch and works for any
  `Host` header — use it for load-balancer health checks.

Startup behavior: a bind error on a *configured* `sites.listen` aborts panel
startup (fail fast — fix the config). Once bound, a runtime serve failure is
logged and the panel keeps running. On shutdown the gateway drains
concurrently with the panel's HTTP connections.

Site records are managed per workspace through the panel UI/API
(`/api/servers/{id}/sites`); the gateway picks up create/update/enable
changes immediately, and only `enabled` sites are routed.


## Server backups

Server backups are SHA-256 verified. Remote backups use a node snapshot and store the resulting archive in the panel backup directory.

Before restore:
- Stop the workload
- Verify available disk
- Confirm the backup checksum
- Keep a second known-good backup

## Diagnostics bundle

```bash
sudo voltpanel-manage doctor --bundle ./voltpanel-diag.tar.gz
sudo voltd-manage doctor --bundle ./voltd-diag.tar.gz
```

The bundle is a `tar.gz` written with mode `0600` containing one
`diagnostics.txt` with: redacted config, service status, binary version,
database integrity and schema version (panel), workload count and isolation
checks (node), disk/memory usage, and the last 200 log lines. Secret-bearing
values (`secret`, `password`, `token`, `key`, `signature`) are masked in both
config and logs before the bundle is packed.

The node refuses to create a bundle while workloads are running, since live
workload output can leak into the logs; pass `--force` after stopping or
draining workloads to override.

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
### `panel.alert` — panel self-health

The panel watches its own subsystems and fires a global `panel.alert` event
when a condition starts. Events are **edge-triggered**: a kind is emitted once
when it transitions from inactive to active, stays silent while the condition
persists, and is re-armed after it recovers — recovery is not an event, so the
condition clears by the alert's absence. The event carries `server_id: null`
(global scope; any global webhook subscribed to it receives it) and is
evaluated every 30 seconds alongside telemetry sampling.

| `kind` | Condition |
| --- | --- |
| `pool.saturated` | DB pool connections at the configured max |
| `mirror.degraded` | Mirror enabled but a mirror operation failed this process lifetime |
| `webhooks.backlog` | More than 50 pending webhook deliveries |
| `schedules.backlog` | More than 10 pending schedule runs |

Subscribe with `panel.alert`, the `panel.*` group wildcard, or `*`. The payload
carries `event`, `kind`, `timestamp`, and `server_id` (always `null`).

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
7. Regenerate enrollment only for deliberate secret recovery: it revokes the current secret immediately and requires `voltd join` before the node can reconnect

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
