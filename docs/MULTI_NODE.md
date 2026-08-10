# Execution Fabric architecture and operations

## Components

VoltPanel separates the control plane from workload execution:

- `voltpanel`: users, UI, authorization, SQLite, placement, schedules, audit, API
- `voltd`: isolated process execution, resources, network, files, console and snapshots

Execution agents can be in different locations as long as panel↔node HTTPS connectivity exists.

## Enrollment

1. Attach an agent in **Control Center → Fabric**
2. Copy the one-time token
3. Run `install-node.sh` or `voltd join`
4. The node receives its UUID and signing secret
5. Heartbeats update node status and capacity every 15 seconds

Enrollment tokens are single-use. Enrollment requires TLS end to end: the panel refuses enrollment over plaintext transport (403) and refuses enrollments without a pinned certificate fingerprint (400), so `--allow-http` no longer permits plaintext enrollment.

## Signed protocol

Every panel↔node request contains:

```text
NODE_ID
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

Responses are signed the same way. The agent echoes the request's nonce and signs

```text
NODE_ID
METHOD
PATH
STATUS
NONCE
SHA256(BODY)
```

appending `signature` and the echoed `nonce` to the envelope; the panel verifies a response before trusting its contents. An agent predating response signing returns an unsigned envelope, which the panel accepts with a per-node warning until the fleet is upgraded. A signature that is present but malformed, echoes the wrong nonce, or fails the MAC is rejected outright.

Keep NTP active on every machine:

```bash
timedatectl status
```

## Placement

Automatic placement filters agents by:

- Online/enrolled/enabled state
- Maintenance and schedulable flags
- Location
- Required tags
- Available memory and disk capacity
- Agent-scoped endpoint availability

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

## Transfer between agents

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

## Secret rotation and re-enrollment

Both **Rotate secret** and **Generate enrollment token** revoke the current shared secret immediately, mark the node unenrolled, and return a new single-use enrollment token. Existing HMAC requests stop authenticating until the agent completes enrollment again. Rotation keeps the node's pinned TLS fingerprint.

For a systemd-managed agent:

1. Stop new provisioning and place the node in maintenance mode.
2. Rotate the secret or generate a new enrollment token in **Control Center → Fabric**; retain the returned token.
3. Run `sudo systemctl stop voltd` on the node.
4. Run `voltd join PANEL_URL TOKEN --public-url NODE_URL --no-start` with the node's existing listen, data, and config options. Enrollment requires TLS (the panel refuses plaintext transport and fingerprint-less enrollments), so re-enroll with the node's existing certificate material; re-enrolling with the SAME fingerprint is accepted.
5. Run `sudo systemctl start voltd`, test connectivity, then leave maintenance mode.

Successful enrollment mints the agent's new shared secret and returns the new enrollment token. Re-enrolling with a DIFFERENT fingerprint than the pinned one is refused — delete and recreate the node to change it. Reusing the token is rejected.

## Capacity planning

Recommended headroom:

- Keep memory utilization below 80%
- Keep 15–20% disk free
- Avoid sustained CPU above 80%
- Reserve panel/node service overhead separately
- Use tags for workloads requiring specific runtimes or storage
