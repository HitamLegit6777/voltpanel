# Security Policy

## Supported versions

Security fixes are provided for the latest tagged release and the default branch.

## Reporting a vulnerability

Use GitHub's **Report a vulnerability** / private Security Advisory feature for this repository.

Include:

- Affected commit/version
- Component (`voltpanel`, `voltd`, isolation, API, UI, installer)
- Reproduction steps or proof of concept
- Impact and attacker prerequisites
- Relevant logs/config with secrets removed
- Proposed mitigation if known

Do not open public issues for active sandbox escapes, authentication bypasses, arbitrary host file access, secret disclosure or node-protocol compromise.

## Response targets

- Initial acknowledgement: 72 hours
- Severity assessment: 7 days
- Critical mitigation target: as soon as safely validated

These are best-effort targets for a community project, not a service-level agreement.

## Scope

High-priority security boundaries include:

- Workload namespace/cgroup/UID/network isolation
- Privileged node file operations and symlink handling
- Panel↔node enrollment/signatures/replay protection
- Authentication, sessions, API keys and subuser permissions
- Transfers, snapshots and extraction
- Installer/systemd/config permissions

See [docs/SECURITY.md](docs/SECURITY.md) for the architecture and operational model.
