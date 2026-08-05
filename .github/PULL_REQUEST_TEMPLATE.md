## Problem

Describe the problem and user impact.

## Changes

- 

## Security implications

Describe changes to authentication, permissions, node protocol, isolation, files, networking, migrations, or state handling. Write `None` only after checking.

## Verification

```text
Commands and observed results
```

## Compatibility and migration

- Database migration:
- Config change:
- Local/remote behavior:
- Rollback:

## UI changes

Attach desktop and mobile screenshots when applicable.

## Checklist

- [ ] `cargo test --all-targets`
- [ ] `cargo build --release --bins`
- [ ] `node --check static/js/icons.js`
- [ ] `node --check static/js/app.js`
- [ ] `shellcheck scripts/*.sh scripts/lib/*.sh`
- [ ] No secrets, databases, logs, binaries or runtime data included
- [ ] Documentation updated
- [ ] Local and remote behavior verified
