# Gaps found by a real DSM → Debian migration

A household NAS running DSM (Synology DS923+) is being moved to Debian with synodeb and rynas.
Its inventory of DSM services showed what rynas still lacks. Every item is generic: it helps
anyone leaving DSM, TrueNAS or a hand-made Debian setup. 2026-11-07.

## Already covered

md RAID and btrfs volumes, snapshots, SMB, NFS v3 with per-host restrictions, scheduled scrub
and RAID checks, SMART alerts, notifications for unhealthy containers, local UPS (NUT, installable from the UPS page:
`../phase1/plan.md`, "API v1: UPS"), the
synodeb hardware screens, the app catalogue, Docker stacks next to other managers.

## Missing, in order

| Phase | Item | Notes | Status (2026-11-18, 0.8.21) |
|---|---|---|---|
| A | Own-domain certificates in the UI (DNS-01) | backend done (W2, §5.4); the domains page is missing | done: Web access › Domains (guided add with DNS preview, records, certificate inventory) |
| A | Import an external certificate | upload, plus a watched folder an external tool (acme.sh on a router, a corporate CA) pushes to; reload without downtime | done: Web access › Imported certificates (upload or watched folder, `/web-access/imports`), the proxy reloads in place |
| B | Tailscale | install and guided login; VPN ranges already feed the access rules (design: `remote-access.md`) | done: `rynas-tailscale`, Network › Remote access (install, login with URL/QR or key, options, logout, uninstall) |
| B | NUT server on the LAN | upsd reachable by other hosts (e.g. Home Assistant), with its own users | done: `ups.netserver` (LISTEN addresses, client accounts, allowed hosts), Hardware › UPS |
| B | Proxy on host:port | routes like `nas.example.com:8443 → app`, besides one name per app on 443 | done: extra HTTPS ports per route (`ports`, `main_port`), path prefixes for shared ports |
| C | Adopt existing compose stacks | take over a project made by hand or by another manager without rewriting it | done: Docker › adopt in place, import a folder, release, move (`docker_adopt`) |
| C | No-CoW subvolume | done 2026-11-07 (D-NOCOW): per-folder switch at creation, state and on/off for empty folders, `nocow` app folders (Frigate) | done |
| C | Existing users and UID changes | adopt Linux users with their UIDs; change a UID with a file ownership remap | done: adopt accounts, UID/GID change with ownership remap (`/v1/ids/plan`, `/v1/ids/remap`), UID dialog on Users & Groups |
| D | Backup to another NAS and to the cloud | restic/rustic repositories; rest-server, SFTP, rclone remotes | done: `rynas-backup` + worker (local, rest-server, SFTP, object storage, rclone), Backup page |
| D | Receive btrbk from PCs | keep existing incremental chains (btrfs receive) | done: Backup › PCs (`pc_backup`, `rynas-btrbk-filter`), adopt an existing folder of received snapshots |
| D | Local snapshot replication | send/receive between volumes (NVMe → HDD) with retention | done: Snapshots › Replication, targets on another volume or over SSH, retention, browse/restore/promote |
| E | Encrypted folder | done 2026-11-08 (D-CRYPT E1, `encrypted-folders.md`): gocryptfs folder per share, recovery key, lock/unlock, password change, optional automatic unlock; volume LUKS exists | done |
| E | Synology Photos album import into Immich | one-time import from the `synofoto` database dump | not started |
| — | Firewall (nftables) | lower priority when the router filters | not started (fail2ban for the login and systemd IP filters for upsd only) |
| — | Degraded btrfs boot | document booting with a dead mirror member (`-o degraded`) | done after 0.8.21 (`btrfs-degraded.md`): md arrays since 0.8.11; a btrfs volume of several devices left unmounted by a missing device is listed and mounted degraded after boot when every chunk has a copy (per-volume setting, on by default; split-brain guard), replace / remove missing / restore redundancy as tasks; a root filesystem gets the `rootflags=degraded` guidance |

Phase A comes first because a certificate renewal date drives the migration.

Status on 2026-11-18 (release 0.8.21): phases A to D and the encrypted folder are done; still
open are the photo album import and the firewall; the degraded btrfs boot is done after 0.8.21.
