# Security and isolation

## Threat model

Workload scripts are treated as hostile. A server must not:

- Read or modify the node host
- Read or modify another server
- Reach the panel/node/LAN over the network
- Escape resource limits
- Gain privilege through setuid binaries or Linux capabilities
- Reuse another server's port on the same node

## Isolation boundary

Each workload uses:

| Control | Enforcement |
|---|---|
| Filesystem | bubblewrap mount namespace; host runtimes read-only; only `/home/container`, `/tmp`, `/run` writable |
| Process visibility | PID namespace |
| IPC | IPC namespace |
| Hostname | UTS namespace |
| Network | Network namespace, veth and nftables |
| Identity | Host-local UID/GID allocation scans existing workload roots and probes to a free ID; root mode 0700 |
| Privilege | `no_new_privs`, empty effective/bounding/inheritable/ambient capabilities |
| Memory | cgroup v2 `MemoryMax`/`memory.max`; swap disabled |
| CPU | `CPUQuota`/`cpu.max` |
| Process count | `TasksMax`/`pids.max` |
| Cleanup | cgroup/scope kill, nft table and veth removal, orphan cleanup at startup |

Launch is fail-closed: missing isolation dependencies cause the workload start to fail rather than run unsandboxed.

## Verified security properties

Regression tests and live probes verify:

```text
CapEff:     0000000000000000
CapBnd:     0000000000000000
NoNewPrivs: 1
setuid(0):  PermissionError
```

Host and peer paths are unavailable:

```text
/etc/shadow                 blocked
/root                       unavailable
/sys/fs/cgroup              unavailable
/home/container/../peer     unavailable
```

Network behavior:

```text
Allocated inbound port      reachable
New node/panel/LAN access   blocked
Established replies         allowed
Public internet egress      allowed
```

Remote file operations reject lexical traversal and every existing symlink component. Snapshot restore rejects symlink/hardlink entries.

## Panel↔node transport

HMAC signing protects integrity and authentication, not confidentiality. Use HTTPS for every production panel and node URL.

For direct HTTPS, `voltd` terminates TLS with a self-signed certificate and the panel pins the SHA-256 fingerprint captured during enrollment. Installer-managed Caddy/Nginx modes terminate public TLS at the reverse proxy and keep the `voltd` origin on loopback. Enrollment requires TLS end to end: the panel refuses plaintext transport (403) and enrollments without a pinned fingerprint (400), so `--allow-http` no longer permits plaintext enrollment — it only fits loopback-local development.

## Secrets

- Passwords: Argon2id
- Sessions/API keys: hashed before storage
- TOTP/execution-agent secrets: protected by 0600 SQLite/config files
- Data/config directories: 0700
- Process/service umask: 0077
- Enrollment token: one-time use

Never commit:

- `config.toml`
- `voltd.toml`
- SQLite database/WAL files
- server data or backups

The provided `.gitignore` excludes these files.

## Squad delegated managers

Squads grant every member their role preset on every grouped server at once.
Manager authority — root admins, the squad creator, and Manager-preset members —
delegates horizontally, mirroring the subuser precedent:

- A manager may add members or change roles only to equal-or-lower roles: the
  subuser anti-escalation rule refuses to mint (or remove) a role whose
  capabilities the manager does not hold on the squad.
- A manager may assign or un-assign a server only when they already hold access
  to it. Without this gate, granting the server would mint the Manager preset on
  any panel server with zero prior access; root admins pass automatically.
- Creation is admin-only; renaming is manager-scoped.
- Outsiders get a minimal `{id, name, my_role: null}` view of a squad — no
  member or server rosters — so sequential squad-id enumeration cannot leak
  usernames or server names.
- Deletion requires the squad creator or a root admin; managers (even
  Manager-preset members) cannot delete a squad they manage.

## Reverse proxy headers

The installer-generated Caddy configs (panel and node vhosts) set:

- HSTS (`Strict-Transport-Security`) on both panel and node vhosts
- `X-Content-Type-Options: nosniff` on both
- `X-Frame-Options: DENY` on both
- Referrer policy on the panel vhost

VoltPanel also emits frame/content/referrer headers directly.

## Reporting vulnerabilities

Use GitHub Security Advisories for private disclosure. Include:

- Affected version/commit
- Reproduction steps
- Impact
- Suggested mitigation if known

Do not publish a working escape before maintainers have released a fix.

## Limitations

- Hard disk byte quotas require filesystem project-quota integration and are not yet enforced per server. Disk capacity is used for placement and monitored.
- SFTP is not yet embedded.
