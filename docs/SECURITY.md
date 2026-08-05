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
| Identity | Collision-free host UID/GID, root mode 0700 |
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

HMAC signing protects integrity and authentication, not confidentiality. Production deployments must use HTTPS for both panel and node URLs.

The installer configures Caddy automatically when a domain is supplied.

Plain HTTP enrollment is allowed only with explicit `--allow-http`; use it solely on a trusted private network.

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

## Reverse proxy headers

Caddy templates set:

- HSTS
- `X-Content-Type-Options: nosniff`
- `X-Frame-Options: DENY`
- Referrer policy

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
- Execution-agent traffic must be protected with TLS/reverse proxy; the agent does not terminate TLS directly.
