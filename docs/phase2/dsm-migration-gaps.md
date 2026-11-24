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

## Round 2: what still makes DSM feel better (2026-11-24)

The same household NAS (a DS923+: Ryzen R1600 with AVX2, no iGPU, 24 GB of RAM) was checked again
area by area against the DSM services in use. Already covered: Hyper Backup and Snapshot
Replication, UPS, notifications (Telegram, webhook, ntfy, Gotify, SMTP), SMB for media players,
the Task Scheduler, Tailscale, ABB (rynas-agent, Linux), no VMs in use. Snapshots before
updates with rollback already exist: apps and stacks (D1, `snapshot_before_update`, rollback on a
failed health check) and the system (`DPkg::Pre-Invoke` restore point, system rollback).

Chosen items, each to start with its own research note as usual:

| Id | Item | Shape | Effort |
|---|---|---|---|
| S1 | Syncthing integrated | catalogue app that adopts a running Syncthing container (config, device ID, folders kept; folder paths remapped to the new places), a page with folders, devices and last contact (REST API with the key from `config.xml`), notifications for `.sync-conflict-*` files, a device unseen for N days and a folder in error; versions come from the share's snapshots (no Syncthing versioning); a folder on an encrypted share is paused while the share is locked and resumed when it is unlocked (`PATCH /rest/config/folders/{id}` `{"paused": …}`), `.stfolder` stays as the second guard | M-L |
| S2 | Security check | a page like a security advisor: exposed ports and services, SSH password login and root login, administrators without 2FA, pending security updates, certificates near expiry, shares writable by everyone, guest access, default ports open to the Internet through the proxy; each finding with its fix in one click when rynas owns the setting, a link otherwise; a weekly run with a notification | M |
| S3 | Public file links | links from the File manager to a file or folder, with expiry, password and download limit, served by rynas-web on the web access domain; list and revoke; audit line per download | M |
| S4 | Ransomware guard | per share, compare the last two snapshots (btrfs `find-new` / send stream metadata): many files changed or renamed with new extensions, or many rewritten files with high entropy → warning, retention of older snapshots frozen until cleared | M |
| S5 | Home Assistant sensors | MQTT publisher with Home Assistant discovery (prefix `homeassistant`): volumes, disks (SMART, temperature), UPS, backups (last result), updates, CPU and memory; broker address and credentials sealed like the other secrets | S-M |
| S6 | Single sign-on for apps | forward auth in Caddy per route, with an OIDC gateway (oauth2-proxy or tinyauth) in front of pocket-id (pocket-id is an OIDC provider only), group checks per route | M |
| S7 | Torrent client | qBittorrent in the catalogue (WebUI, download folder on a share) | S |
| S8 | Frigate page | after a test with the real cameras: storage and retention on a NOCOW share, detector choice (OpenVINO on the R1600 CPU, a Hailo or Coral when there are more cameras), events from MQTT into notifications | M |

Not chosen for now: Photos album import, contacts and calendar, office, cloud sync, a log page,
search, a storage report, event scripts, static sites, mobile apps, a Windows agent.
