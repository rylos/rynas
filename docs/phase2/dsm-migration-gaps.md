# Gaps found by a real DSM → Debian migration

A household NAS running DSM (Synology DS923+) is being moved to Debian with synodeb and rynas.
Its inventory of DSM services showed what rynas still lacks. Every item is generic: it helps
anyone leaving DSM, TrueNAS or a hand-made Debian setup. 2026-11-07.

## Already covered

md RAID and btrfs volumes, snapshots, SMB, NFS v3 with per-host restrictions, scheduled scrub
and RAID checks, SMART alerts, notifications for unhealthy containers, local UPS (NUT), the
synodeb hardware screens, the app catalogue, Docker stacks next to other managers.

## Missing, in order

| Phase | Item | Notes |
|---|---|---|
| A | Own-domain certificates in the UI (DNS-01) | backend done (W2, §5.4); the domains page is missing |
| A | Import an external certificate | upload, plus a watched folder an external tool (acme.sh on a router, a corporate CA) pushes to; reload without downtime |
| B | Tailscale | install and guided login; VPN ranges already feed the access rules (design: `remote-access.md`) |
| B | NUT server on the LAN | upsd reachable by other hosts (e.g. Home Assistant), with its own users |
| B | Proxy on host:port | routes like `nas.example.com:8443 → app`, besides one name per app on 443 |
| C | Adopt existing compose stacks | take over a project made by hand or by another manager without rewriting it |
| C | No-CoW subvolume | `chattr +C` folders for video recordings and databases |
| C | Existing users and UID changes | adopt Linux users with their UIDs; change a UID with a file ownership remap |
| D | Backup to another NAS and to the cloud | restic/rustic repositories; rest-server, SFTP, rclone remotes |
| D | Receive btrbk from PCs | keep existing incremental chains (btrfs receive) |
| D | Local snapshot replication | send/receive between volumes (NVMe → HDD) with retention |
| E | Encrypted folder | per-folder encryption (gocryptfs) with unlock and mount; volume LUKS exists |
| E | Synology Photos album import into Immich | one-time import from the `synofoto` database dump |
| — | Firewall (nftables) | lower priority when the router filters |
| — | Degraded btrfs boot | document booting with a dead mirror member (`-o degraded`) |

Phase A comes first because a certificate renewal date drives the migration.
