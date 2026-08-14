# Upgrade and uninstall

## Upgrade the panel

```bash
sudo voltpanel-manage upgrade
```

The command downloads and verifies the release, validates the existing config with the new binary, creates a full panel backup (online SQLite backup, data directories including `datalab/`, and the config tree — see OPERATIONS.md), then replaces and restarts the service. If startup fails, it restores and restarts the previous binary automatically. Database migrations still require the matching database backup for a manual downgrade; `sudo voltpanel-manage restore ARCHIVE` performs a verified restore with automatic rollback.

Pin a specific release when required:

```bash
sudo voltpanel-manage upgrade v0.1.1
```

After upgrade:

```bash
sudo voltpanel-manage doctor
sudo journalctl -u voltpanel -n 100 --no-pager
```

## Upgrade a node

Mark the node non-schedulable and enter maintenance mode, then:

```bash
sudo voltd-manage upgrade
sudo voltd-manage doctor
```

`upgrade` refuses to run while workloads are live on the node, so the swap can
never interrupt a running server. Stop or drain the workloads first, or pass
`--force` to stop them as part of the upgrade:

```bash
sudo voltd-manage upgrade --force
```

The node manager validates its config before replacement and restores the
previous binary automatically if the upgraded service fails to start. A release
can be pinned with `sudo voltd-manage upgrade v0.1.1`.

For support, create a diagnostics bundle (refused while workloads run unless
`--force` is passed):

```bash
sudo voltd-manage doctor --bundle ./voltd-diag.tar.gz
```

## Manual rollback

Before every upgrade, retain:

- Previous `voltpanel`/`voltd` binary
- `/etc/voltpanel/config.toml`
- `/etc/voltpanel-node/voltd.toml`
- SQLite online backup
- Workload/snapshot backups

The same state can be restored from the backup archive created by the upgrade
itself with one command (validated, with automatic rollback on failure):

```bash
sudo voltpanel-manage restore /var/backups/voltpanel/panel-<timestamp>.tar.gz
```

Panel rollback:

```bash
sudo systemctl stop voltpanel
sudo install -m755 voltpanel.previous /usr/local/bin/voltpanel
sudo cp panel-backup.db /var/lib/voltpanel/voltpanel.db
sudo chown root:root /var/lib/voltpanel/voltpanel.db
sudo chmod 600 /var/lib/voltpanel/voltpanel.db
sudo systemctl start voltpanel
```

Database migrations are forward-only. Restoring the matching pre-upgrade database is required when rolling back across a schema migration.

## Installer management modes

The panel installer can also manage an existing installation:

```bash
sudo bash /tmp/install-panel.sh --reinstall
sudo bash /tmp/install-panel.sh --reset-password admin
sudo bash /tmp/install-panel.sh --uninstall
sudo bash /tmp/install-panel.sh --uninstall --purge
```

`--reinstall` replaces and restarts the binary/service while preserving configuration, database, workloads, and credentials. Plain `--uninstall` removes the application, service, management helper, and reverse-proxy artifacts but preserves configuration and data for a later reinstall. Only `--uninstall --purge` permanently removes configuration and the configured data directory.

## Uninstall panel with the management helper

The installed management helper retains its explicit purge workflow. Create a final backup, then:

```bash
sudo voltpanel-manage backup
sudo voltpanel-manage uninstall --purge
```

This removes config and data. Without a separate backup, deletion is permanent.

## Uninstall node

Transfer/delete workloads first:

```bash
sudo voltd-manage uninstall --purge
```

After uninstall inspect and remove stale firewall interfaces/tables only if they remain:

```bash
ip link show | grep '^.*vp'
sudo nft list tables | grep '^table ip vp'
```

Normal agent cleanup removes these automatically.
