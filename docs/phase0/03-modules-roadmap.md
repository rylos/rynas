# rynas — Modules and roadmap (proposal)

Source: `research/01-features.md` (DSM/TrueNAS/OMV/Unraid matrix, ideas, mistakes to avoid).

## Modules

Core = always installed. Builtin = first-party plugin compiled into the core (same API as plugins).
Package = separate `rynas-plugin-<id>.deb`.

| Module | Type | Content | Inspiration |
|---|---|---|---|
| core | core | login, sessions, 2FA, ACL, dashboard, tasks, notifications, pending/apply, metrics, apt updates, logs | TrueNAS (API, jobs), OMV (apply), PBS (tasks) |
| storage | builtin | disks, SMART, mdadm, hidden LVM, btrfs (volumes, subvolumes, snapshots, scrub, quotas), visual chassis | DSM Storage Manager, DSM scrub |
| shares-smb | builtin | Samba: shares, permissions, previous versions (`vfs_btrfs`/`shadow_copy2`), recycle bin, "extra options", Time Machine | DSM, OMV |
| shares-nfs | builtin | NFS exports | |
| users | builtin | local users/groups, quotas, app access; single-screen wizard | DSM |
| files | builtin | **SynoFile file manager** | SynoFile |
| network | builtin | interfaces, IP, DNS, bond/VLAN/bridge, **test changes with rollback** | TrueNAS |
| synology | package | Synology hardware: fans, LEDs, beeper, buttons, bays, power schedule, WoL, restore after power loss (via synohwd) | DSM |
| nut | package | UPS (network client and server), Safe Mode | DSM |
| docker | package | compose stacks, catalog from git (schema → form), stack backup | TrueNAS apps, omv-compose, Unraid CA |
| backup | package | rsync, restic/rustic, rclone cloud, btrfs send/receive | Hyper Backup, TrueNAS Cloud Sync |
| firewall | package | per-service nftables, auto-block | DSM |
| webdav/ftp | package | | |
| zfs | package | ZFS (DKMS, separate CDDL license) | TrueNAS |
| directory | package | AD join, LDAP client, OIDC | DSM, Unraid 7.2 |
| vm | package | libvirt, LXC | Unraid |
| remote | package | Tailscale/WireGuard | Unraid Connect |
| business: multi-NAS, audit, advanced replication, advanced AD | business | see `04-license.md` | |

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

### Phase 2 — services and protection
docker, backup, nut, firewall, full networking, log center + metrics history, webdav/ftp,
generic power schedule (rtcwake), synohwd in Rust, third-party plugins, business repo.

### Phase 3 — pro level
zfs, iSCSI/NVMe-oF, directory (AD/LDAP/OIDC/passkey), vm, remote, multi-NAS,
first-boot wizard, migration from DSM (import shares and users), installation ISO.

## Mistakes to avoid (from the research)
- Silently overwriting hand-edited files → header + hash + "import/overwrite" warning.
- Architecture changes that break apps → standard compose, data in readable paths.
- Hiding raw data (DSM SMART) → "advanced" view always available.
- Fragile configuration DB → full export/import to a single file, secrets included (encrypted with a passphrase).
- Plugins that break at every major release → `api_version` in the manifest, compatibility check before upgrading.
- Inventing a custom array (Unraid) → standard mdadm/btrfs/ZFS.
- Surprise monetization → clear rules from day one, no nagging, no mandatory cloud.
