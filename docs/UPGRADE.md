# Upgrade and uninstall

## Upgrade the panel

```bash
sudo voltpanel-manage upgrade
```

The command:

1. Downloads the latest binary
2. Creates a panel database/files backup
3. Stops the service
4. Replaces the binary
5. Starts the service
6. Runs migrations automatically during startup

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

Running workloads are tied to the daemon lifecycle. Plan a maintenance window or transfer workloads first.

## Manual rollback

Before every upgrade, retain:

- Previous `voltpanel`/`voltd` binary
- `/etc/voltpanel/config.toml`
- `/etc/voltpanel-node/voltd.toml`
- SQLite online backup
- Workload/snapshot backups

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

## Uninstall panel

Create a final backup, then:

```bash
sudo voltpanel-manage backup
sudo voltpanel-manage uninstall --purge
```

`--purge` removes config and data. Without a separate backup, deletion is permanent.

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

Normal daemon cleanup removes these automatically.
