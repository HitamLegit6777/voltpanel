# Contributing to VoltPanel

Thank you for improving VoltPanel. Contributions are welcome: bug fixes, security hardening, installers, documentation, UI/UX, eggs, platform support and new hosting features.

## Before opening work

1. Search existing issues and pull requests.
2. For large architectural changes, open a design issue first.
3. Security vulnerabilities must use private GitHub Security Advisories, not public issues.
4. Keep VoltPanel free, self-hosted and Docker-free by default.

## Development setup

Requirements:

- Rust 1.80+
- Linux with cgroup v2
- `bubblewrap`, `setpriv`, `ip`, `nft`, `systemd-run`
- Node.js only for `node --check` on frontend JavaScript
- ShellCheck for installer scripts

```bash
git clone https://github.com/HitamLegit6777/voltpanel.git
cd voltpanel
cargo test
cargo build --bins
node --check static/js/icons.js
node --check static/js/app.js
shellcheck scripts/*.sh scripts/lib/*.sh
```

Run locally:

```bash
cp config.example.toml config.toml
VOLTPANEL_ADMIN_PASSWORD='development-password' cargo run --bin voltpanel
```

Run a test node:

```bash
cargo run --bin voltd -- join http://127.0.0.1:8080 TOKEN \
  --public-url http://127.0.0.1:8081 \
  --listen 127.0.0.1:8081 \
  --data /tmp/voltd-dev \
  --config /tmp/voltd-dev.toml \
  --no-start
cargo run --bin voltd -- serve --config /tmp/voltd-dev.toml
```

## Engineering expectations

- Correctness and security before abstraction.
- Workload launch must fail closed when isolation cannot be configured.
- Never weaken namespace, capability, UID, cgroup, path or network isolation silently.
- Local and remote servers must preserve API behavior parity.
- Use transactions for multi-table state changes.
- Reject traversal and symlink escapes in privileged file operations.
- Avoid allocations, copies and polling when a stream/cache is appropriate.
- Do not introduce paid-service requirements.

## Frontend expectations

- Vanilla JavaScript/CSS/SVG; avoid large UI frameworks unless discussed first.
- No emoji used as interface icons.
- No native `alert`, `confirm` or `prompt` dialogs.
- Keyboard navigation and accessible names are required.
- Verify 1440px, 900px and 390px layouts.
- New async views need loading, empty, error and success states.

## Tests

Every security or behavior fix should include a regression test. Important areas:

- Sandbox escape attempts
- UID/port collision safety
- HMAC replay and tampering
- Transfer rollback
- Local/remote API parity
- Migration upgrades
- Permission enforcement

Run:

```bash
cargo test --all-targets
cargo build --release --bins
```

## Pull requests

A PR should include:

- Problem and user impact
- Design/implementation summary
- Security implications
- Verification commands and results
- Screenshots for visual changes
- Migration and rollback notes where relevant

Keep commits focused. Do not include runtime data, secrets, databases, logs or generated binaries.

## Egg contributions

Provide:

- Upstream project URL and license
- Supported versions
- Startup command
- Variables and validation rules
- Sandboxed install script that does not modify the host OS
- Stop command
- Minimum resources

## License

By contributing, you agree that your contribution is licensed under the MIT License.
