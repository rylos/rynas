# rynas — Modules and roadmap (proposal)

Source: `research/01-features.md` (DSM/TrueNAS/OMV/Unraid matrix, ideas, mistakes to avoid).

## Modules

Core = always installed. Builtin = first-party plugin compiled into the core (same API as plugins).
Package = separate `rynas-plugin-<id>.deb`.

| Module | Type | Content | Inspiration | Status (2026-11-18, 0.8.21) |
|---|---|---|---|---|
| core | core | login, sessions, 2FA, ACL, dashboard, tasks, notifications, pending/apply, metrics, apt updates, logs | TrueNAS (API, jobs), OMV (apply), PBS (tasks) | done, built into rynasd/rynas-web (`rynas-auth`, `rynas-config`, `rynas-tasks`, `rynas-notify`, `rynas-updates`); partial: metrics history is the last hour in memory, no log center (audit lines go to the journal) |
| storage | builtin | disks, SMART, mdadm, hidden LVM, btrfs (volumes, subvolumes, snapshots, scrub, quotas), visual chassis | DSM Storage Manager, DSM scrub | done (`rynas-storage`, `rynas-btrfs`, Storage pages, md+LVM+btrfs flexible RAID, chassis view) |
| shares-smb | builtin | Samba: shares, permissions, previous versions (`vfs_btrfs`/`shadow_copy2`), recycle bin, "extra options", Time Machine | DSM, OMV | done (`rynas-services`, Shared Folders and File Services pages) |
| shares-nfs | builtin | NFS exports | | done (NFS v4.1/4.2, v3 optional, per-host rules) |
| users | builtin | local users/groups, quotas, app access; single-screen wizard | DSM | done (Users & Groups, btrfs quotas, adopt existing accounts, UID/GID remap) |
| files | builtin | **SynoFile file manager** | SynoFile | done (`rynas-files`, `rynas-files-worker`, File Manager page as a Web Component) |
| network | builtin | interfaces, IP, DNS, bond/VLAN/bridge, **test changes with rollback** | TrueNAS | partial: IP/DNS/bonds with timed confirmation, rescue, diagnostics done (`rynas-net`); VLANs and bridges are only shown, not created |
| synology | package | Synology hardware: fans, LEDs, beeper, buttons, bays, power schedule, WoL, restore after power loss (via synohwd) | DSM | done as a built-in hardware module (`rynas-hw-synology` behind `rynas-module`, D-MODULE), not a separate package; a second hardware module exists |
| nut | package | UPS (network client and server), Safe Mode | DSM | done in the core: local UPS setup, Safe Mode, network UPS server (`ups_netserver`); the NAS as a client of another UPS server is not in the UI |
| docker | package | compose stacks, catalog from git (schema → form), stack backup | TrueNAS apps, omv-compose, Unraid CA | done in the core (`rynas-docker`, `rynas-apps`, Docker and Apps pages, catalog from git, adopt/import stacks); missing: per-stack backup export |
| backup | package | rsync, restic/rustic, rclone cloud, btrfs send/receive | Hyper Backup, TrueNAS Cloud Sync | done (`rynas-backup` + `rynas-backup-worker` on rustic_core: local, rest-server, SFTP, object storage, rclone; btrfs send/receive as snapshot replication; PC backup); no plain rsync job |
| firewall | package | per-service nftables, auto-block | DSM | not started (only fail2ban for the login, systemd IP filters for upsd, fixed NFSv3 ports) |
| webdav/ftp | package | | | not started (SFTP exists through the SSH page) |
| zfs | package | ZFS (DKMS, separate CDDL license) | TrueNAS | not started |
| directory | package | AD join, LDAP client, OIDC | DSM, Unraid 7.2 | not started (local accounts only; group lookups go through NSS) |
| vm | package | libvirt, LXC | Unraid | not started |
| remote | package | Tailscale/WireGuard | Unraid Connect | partial: Tailscale done (`rynas-tailscale`, Network › Remote access); WireGuard subnets only feed the access rules, no WireGuard management |
| business: multi-NAS, audit, advanced replication, advanced AD | business | see `04-license.md` | | not started |

## File manager: porting SynoFile

| SynoFile (DSM) | rynas |
|---|---|
| DSM `authenticate.cgi`, `administrators` only | rynas session; access by role/ACL, including non-admin users on their own shares |
| CGI + musl binary | `files` plugin routes + `rynas-files-worker` worker |
| setresuid to the DSM user, `cap_setuid,cap_setgid,cap_dac_read_search` | same, to the logged-in Linux user (capabilities granted by the systemd unit, no manual `setup`) |
| transfer daemon (Unix socket, journal, max 2 jobs) | becomes a type of **rynas task** (UPID, task panel, SSE) |
| `synorecycle` | own recycle bin, `#recycle` compatible (the same one used by Samba `vfs_recycle`) |
| `space_meta.status` for volume names | names from `rynas-config` (storage) |
| Synology Photos `@eaDir` thumbnails | own cached thumbnails (+ `@eaDir` when present, useful on former DSM volumes) |
| ES modules UI + CodeMirror 6 | **decision D-FM** |

Reuse: SynoFile is MIT and by the same author → the Rust code (transfers, reflink, archives, space,
SMB/SFTP, eza) is extracted into a `rynas-files-core` crate, which SynoFile itself could share.

## Roadmap

### Phase 0 — design (now)
Research, architecture, UI, license, decisions. Output: these documents + the decisions taken.

### Phase 1 — MVP "a DS923+ running Debian, managed from the browser"
1. Workspace skeleton, `rynas-web` + `rynasd`, PAM + TOTP login, basic ACL, tasks, SSE.
2. Leptos shell: Fusion light/dark theme, layout A, dashboard, command palette, task panel.
3. Plugin protocol v1 + `SchemaRenderer`; **synology** plugin read-only (fans, temperatures, bays), then read-write.
4. `rynas-config` with pending/apply/rollback and anti-drift.
5. **storage**: disks, SMART, mdadm + btrfs, scheduled snapshots, scrub.
6. **users** + **shares-smb** + **shares-nfs**.
7. **files** (SynoFile port; first: browsing, transfers, recycle bin, quick look).
8. Basic networking (IP/DNS with test changes), notifications (mail, ntfy, Telegram), apt updates.
9. `.deb` packages, signed community apt repo (aptly), install on synodeb and on a generic Debian VM.

Status (2026-11-18, release 0.8.21):

| # | Status |
|---|---|
| 1 | done (`rynas-web`, `rynasd`, `rynas-auth`: PAM + TOTP, roles, tasks, SSE) |
| 2 | done (`rynas-ui`: shell, light/dark theme, dashboard, command palette, task panel) |
| 3 | partial: the synology hardware is done read-write as a built-in module (`rynas-module`, `rynas-hw-synology`); `SchemaForm` renders the app catalogue forms; the plugin protocol v1 (out-of-tree plugins, manifest, `api_version` check) is not started |
| 4 | done (`rynas-config`: pending/apply, revisions, drift, `.rynasbak` export) |
| 5 | done (Storage pages, scheduled snapshots and scrub) |
| 6 | done |
| 7 | done (browsing, transfers, recycle bin, quick look in the File Manager page) |
| 8 | done (networking with timed confirmation, notifications by mail/ntfy/Telegram, apt updates) |
| 9 | partial: `.deb` built by the release workflow and published as GitHub release assets, installed on synodeb and on plain Debian; the signed apt repository (aptly) is not started |

### Phase 2 — services and protection
docker, backup, nut, firewall, full networking, log center + metrics history, webdav/ftp,
generic power schedule (rtcwake), synohwd in Rust, third-party plugins, business repo.

Status (2026-11-18): done: docker (with app catalogue and adoption of existing stacks), backup
(restic-format repositories, snapshot replication local and over SSH, PC backup), nut (local and
network server), generic power schedule (`rynas-power`, rtcwake), Tailscale, web access (Caddy,
own domains, imported certificates), dedup, encrypted folders. Partial: full networking (VLAN and
bridge creation missing); log center and metrics history (only the last hour of metrics in
memory, audit lines in the journal, SSH login list; no log page). Not started: firewall
(nftables), webdav/ftp, synohwd in Rust (rynas drives synodeb's synohwd), third-party plugins,
business repo.

### Phase 3 — pro level
zfs, iSCSI/NVMe-oF, directory (AD/LDAP/OIDC/passkey), vm, remote, multi-NAS,
first-boot wizard, migration from DSM (import shares and users), installation ISO.

Status (2026-11-18): remote is partial (Tailscale done, WireGuard management not started).
Not started: zfs, iSCSI/NVMe-oF, directory (AD/LDAP/OIDC/passkey), vm (libvirt/LXC), multi-NAS,
first-boot wizard, migration import from DSM (building blocks exist: adopting existing users with
their UIDs, existing compose stacks and existing btrbk folders), installation ISO.

## Mistakes to avoid (from the research)
- Silently overwriting hand-edited files → header + hash + "import/overwrite" warning.
- Architecture changes that break apps → standard compose, data in readable paths.
- Hiding raw data (DSM SMART) → "advanced" view always available.
- Fragile configuration DB → full export/import to a single file, secrets included (encrypted with a passphrase).
- Plugins that break at every major release → `api_version` in the manifest, compatibility check before upgrading.
- Inventing a custom array (Unraid) → standard mdadm/btrfs/ZFS.
- Surprise monetization → clear rules from day one, no nagging, no mandatory cloud.
