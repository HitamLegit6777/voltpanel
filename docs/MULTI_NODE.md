# Multi-node architecture and operations

## Components

VoltPanel separates the control plane from workload execution:

- `voltpanel`: users, UI, authorization, SQLite, placement, schedules, audit, API
- `voltd`: isolated process execution, resources, network, files, console and snapshots

Nodes can be in different locations as long as panel↔node HTTPS connectivity exists.

## Enrollment

1. Attach an agent in **Control Center → Fabric**
2. Copy the one-time token
3. Run `install-node.sh` or `voltd join`
4. The node receives its UUID and signing secret
5. Heartbeats update node status and capacity every 15 seconds

Enrollment tokens are single-use. Non-loopback HTTP enrollment is refused unless `--allow-http` is explicit.

## Signed protocol

Every panel↔node request contains:

```text
METHOD
PATH
TIMESTAMP
NONCE
SHA256(BODY)
```

The canonical string is signed using HMAC-SHA256. Both sides reject:

- Signatures outside the 90-second clock window
- Reused nonces
- Modified bodies or paths
- Wrong node identity

Keep NTP active on every machine:

```bash
timedatectl status
```

## Placement

Automatic placement filters nodes by:

- Online/enrolled/enabled state
- Maintenance and schedulable flags
- Location
- Required tags
- Available memory and disk capacity
- Node-scoped port availability

Candidates are scored using CPU, memory utilization and running server count.

## Allocations

Ports are unique per node, not globally. This is valid:

```text
node-a :25565 → server A
node-b :25565 → server B
```

This is rejected:

```text
node-a :25565 → server A
node-a :25565 → server B
```

Every allocated port is exposed through the server's private veth/nft network. All other inbound ports are blocked.

## Workload network model

Each workload receives:

- Private network namespace
- Dedicated veth pair
- Deterministic private IPv4 `/30`
- nftables DNAT for allocated TCP/UDP ports
- Source NAT for internet egress
- Rules blocking new connections to node/LAN/private networks
- Established replies allowed

The node host cannot be reached from a workload. Outbound public internet remains available for game authentication and downloads.

## Transfer between nodes

Transfers are offline and integrity-checked:

1. Validate target status/capacity/ports
2. Stop the source workload
3. Create `tar.gz` snapshot
4. Calculate SHA-256
5. Provision target spec
6. Upload and verify snapshot
7. Atomically move allocation ownership
8. Delete source data
9. Restart on target if it was previously running

Failure rollback deletes target state and restarts the source when possible.

## Node maintenance

Before maintenance:

1. Set node `schedulable=false`
2. Set `maintenance=true`
3. Transfer or stop workloads
4. Upgrade and reboot
5. Run `voltd-manage doctor`
6. Re-enable scheduling

## Secret rotation

Rotating a secret invalidates the daemon's current secret. For safe rotation:

1. Stop new provisioning
2. Generate a new enrollment token
3. Run `voltd join ... --no-start` again on the node
4. Restart `voltd`
5. Test connectivity

## Capacity planning

Recommended headroom:

- Keep memory utilization below 80%
- Keep 15–20% disk free
- Avoid sustained CPU above 80%
- Reserve panel/node service overhead separately
- Use tags for workloads requiring specific runtimes or storage
