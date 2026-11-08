# Snapshots, replication and backup in rynas: research and proposal

## Summary

- **Use btrbk as the engine** for scheduled snapshots, retention and replication. rynas writes one btrbk configuration per job and runs it as one of its own tasks. Rust code handles the **read and restore** side (listing, space accounting, browser, deletion, pre-update snapshots) and is ported from btrbk-nas.
- Snapshots land on disk with **btrbk's naming and layout**. That way the `btrbk` CLI, btrbk-nas, the maintainer's own TUI tools and client PCs already backed up with btrbk understand them without any conversion.
- **Native, in the storage module:** data snapshots, system restore points before apt upgrades, browser and restore, Previous Versions over SMB.
- **`backup` plugin:** NAS→NAS or NAS→USB replication, receiving PC backups (ported btrbk-nas), restic to the cloud.
- **No classic qgroups enabled by default**: if quotas are needed, use squota.

---

## 1. What btrbk-nas and the maintainer's other projects do

**btrbk-nas** ([github.com/rylos/btrbk-nas](https://github.com/rylos/btrbk-nas), v0.5.0, MIT, Rust 2024, static musl binary, 4,300 lines; depends only on serde, chrono and libc) is a DSM 7 package that **does not take backups**: it displays the btrbk backups the NAS receives from PCs.
- One page per PC. For each subvolume: freshness (*late* after 26 h, *critical* after 72 h), number of snapshots, the oldest one, exclusive space (if qgroups are enabled).
- Anomalies: interrupted receives (btrbk calls them *garbled*: no `received_uuid`, or still writable) and folders with snapshot-style names that are not subvolumes.
- **Next incremental parent.** The PC writes `.btrbk-nas-source` (name, UUID and date of every local snapshot) with `btrbk-nas-push-uuids`, run as `ExecStopPost` of `btrbk.service`. Matching UUID ↔ `received_uuid` is exact. The NAS alone cannot rebuild the chain: the received `parent_uuid` points to no subvolume on the target.
- Deletion that does not break the chain: only snapshots older than the common parent, with a 5 s cancellable queue and a re-check under lock.
- Browsing and download (single file, or folder as `.tar`) inside read-only snapshots. Paths are opened by descriptor (`O_PATH|O_NOFOLLOW`, same device), so a file swap during the download cannot redirect it elsewhere.
- Hourly check daemon: notifications only on state changes, with a reminder every 24 h, on the DSM desktop and via webhook (ntfy, Telegram).
- Security: no root process, capabilities reduced to what each route needs.
- `src/btrfs.rs` reads metadata with the `TREE_SEARCH`/`INO_LOOKUP` ioctls and decodes ROOT_ITEM and qgroup items, because DSM runs on kernel 4.4.

**The maintainer's own PC restore tools** (MIT; Python and Rust): restore on the PC. The active subvolume is renamed `.BROKEN` with `rename(2)` (never `mv`) and the snapshot takes its place. They also do chain-aware cleanup and provide `post-change.d` hooks. **An iPXE-based bare-metal restore script** (shell) restores a PC from the NAS, booting via iPXE from the NAS.

**What to reuse in rynas.** MIT licence and the maintainer's own code, so compatible with both the BSL core and the Apache plugins.
- **To port** into a new `rynas-btrfs` crate: `btrfs.rs`, `snapshot.rs` (btrbk names, scanning, discovery), `report.rs` (freshness), `source.rs`, `browse.rs` (fd-walk and tar).
- **To drop:** `auth.rs`, `cgi.rs`, `caps.rs`, DSM notifications. rynas already has sessions, a root rynasd over a socket, and a notification system.
- On Debian (kernel ≥ 6.1) `BTRFS_IOC_GET_SUBVOL_INFO` can also be used, unprivileged.

In rynas, btrbk-nas becomes the **"PC backups"** tab of the `backup` plugin (see §4). It also does something it could not do on DSM: **it prepares the NAS side**. It creates the restricted ssh user, manages `authorized_keys` with `ssh_filter_btrbk.sh`, and creates the destination subvolume for each PC.

## 2. Comparison

### 2.1 Snapshots and retention

| | Scheduling | Retention | Naming | Notes |
|---|---|---|---|---|
| **Synology Snapshot Replication** | per shared folder or LUN | count/age, or **Smart Retention GFS** (hourly, daily, weekly, monthly, yearly: "the first of each of the last N weeks") | `GMT+01-2026.11.05-12.00.00` | **immutable** snapshots with a protection period (some models only); the latest replicated snapshot cannot be deleted |
| **TrueNAS 25.10** | Periodic Snapshot Task per dataset (recursive, exclusions) | lifetime (HOUR…YEAR, default WEEK); a snapshot created by several tasks lives until the longest expiry | strftime schema, default `auto-%Y-%m-%d_%H-%M`, %Y %m %d %H %M mandatory | incremental replication only sees snapshots matching **its own** naming schema (a known forum pitfall) |
| **btrbk 0.32.7** | external (timer or cron); `snapshot_create always/onchange/ondemand` | `snapshot_preserve 14d 8w *m`, `*_preserve_min 2d`, `preserve_day_of_week`, `preserve_hour_of_day`; different retention for source and target | `<subvol>.20261105T1200` (`long`), `long-iso` with offset to avoid DST ambiguity, `_N` on collision | `lockfile`, `transaction_log`, machine-readable `--format=raw` |
| **snapper** | timeline (hourly, daily, …) + pre/post | `TIMELINE_LIMIT_*`, `number` algorithm | `.snapshots/<N>/snapshot` + `info.xml` | the Debian package ships an apt hook (`80snapper`, to be verified on trixie: sources are from 2023); numeric names, so no direct shadow_copy2 (Samba has `vfs_snapper`) |
| **Timeshift** | hours/days/weeks/months | count per level | `timeshift-btrfs/snapshots/<date>/@` | `@`/`@home` layout **only**; no file exclusions; "not a backup" |

### 2.2 Replication and backup

| | Model | Strengths | Limits |
|---|---|---|---|
| **Synology SR** | block-level or snapshot replication to another Synology | **switchover** (planned reverse replication) and **failover** (promotion of the copy), replication on snapshot creation | Synology to Synology only |
| **Hyper Backup** | versions, dedup, multiple destinations, cloud | integrity check, Hyper Backup Explorer | proprietary format |
| **Active Backup for Business** | agents on PCs, global dedup, bare-metal restore | central console | proprietary |
| **TrueNAS Replication** | zfs send **push or pull** over SSH, keys managed by the UI, *SSH+netcat* on the LAN | wizard, independent retention on the target, resumable transfers | ZFS only |
| **btrbk** | incremental send/receive, ssh targets (also **pull**: btrbk runs on the target), `raw` target (compressed or gpg-encrypted file) | picks the parent itself, detects garbled receives, `stream_compress zstd`, `mbuffer`, `rate_limit`, `ssh_filter_btrbk.sh` | `raw` target still experimental; **0.32.7 (July 2026) fixes a security hole in `ssh_filter_btrbk.sh`** |
| **restic / rustic / borg / kopia** | file-level backup with dedup and encryption to S3, B2, SFTP, rest-server | restic is mature and packaged by Debian; rest-server in `--append-only` against ransomware; kopia has a GUI | rustic 0.11.x is compatible with the restic format but packagers label it **beta**, not production-ready |
| **Proxmox Backup Server** | dedup chunks, separate **verify job**, **prune** and **GC**, namespaces | separates logical retention (prune) from space reclamation (GC); scheduled verification | designed for VMs and containers |

**Best ideas:** GFS using DSM's wording (hourly, daily, weekly, monthly, yearly) translated into btrbk's `*_preserve`. A preview of what will be deleted: TrueNAS shows it, btrbk provides it with `prune -n`. The most recent replicated snapshot protected from deletion (DSM). Verify, prune and GC as separate jobs (PBS). Pull from the backup NAS (TrueNAS, btrbk). Switchover and failover (DSM). Notifications only on state changes (btrbk-nas).

## 3. Key technical aspects

**Atomicity.** A btrfs snapshot is atomic for a single subvolume, not across subvolumes. A docker stack with a database needs the "stop the stack during the snapshot" option (already in D-DOCKER) or a pause with `docker compose pause`.

**System vs data.** The synodeb root is on btrfs, in the `@rootfs` subvolume created by the Debian installer. `synodeb-dom` reads `subvol=` from fstab and writes `rootflags=subvol=@rootfs` into the DEBIAN entry on the DOM. The kernel and initrd live **on the DOM**, not in the snapshot. Four consequences follow:
1. **grub-btrfs cannot be used on synodeb**: the Synology GRUB lives on the DOM (128 MB), its configuration is fixed, and there is no console to choose from. On generic Debian with GRUB it may be an option.
2. **Rollback by rename**, as in the PC restore tools and Timeshift. From the filesystem top level, create a writable copy of the snapshot (`@rootfs.new`), then `@rootfs` → `@rootfs.broken-<ts>`, then `@rootfs.new` → `@rootfs`, then reboot. This way `rootflags` does not change and **the DOM does not need rewriting**.
3. **Kernel pitfall.** If the rolled-back apt run had installed a new kernel, the DOM has the new kernel while the restored root has the old `/lib/modules`. The rollback must also run `synodeb-dom default debian-old` (if DEBIAN_OLD has the same uname) or a fresh `synodeb-dom sync --root <snapshot>`. **To be agreed with synodeb.**
4. Phase 2 idea, to be validated: DEBIAN_OLD as a "fallback boot environment" (previous kernel with `rootflags=subvol=@rootfs.prev`), recovered with the same kernel trial mechanism synodeb already uses.

**What stays out of the root** (nested or separate subvolumes, as snapper and openSUSE do): `/var/log`, `/var/cache/apt`, `/tmp`, the docker data-root (on the data volume, D-DOCKER), swapfile.
**What stays in:** `/etc` and rynas's `config.db`. As with TrueNAS boot environments, configuration rolls back together with the binaries: a DB schema migration cannot be read by an older rynas.

**Previous Versions over SMB.** **`vfs_shadow_copy2`** plus btrbk names is all it takes:
```
vfs objects = shadow_copy2 btrfs
shadow:snapdir = /vol1/.snapshots/photos   # absolute path
shadow:basedir = /vol1/photos
shadow:format = photos.%Y%m%dT%H%M
shadow:localtime = yes                     # btrbk uses local time
shadow:sort = desc
```
`vfs_btrfs` adds server-side copy via cloning (instant copies from Windows) and snapshot creation via FSRVP. Classic mistake: a `shadow:format` that does not match the names leaves the Previous Versions tab empty. With the `long-iso` format, check that `%z` is parsed correctly. With `_N` the snapshot is not recognised (rare).

**qgroups.** Classic qgroups keep reference accounting on every write and on snapshot deletion. With many snapshots performance collapses (benchmarks from Boris Burkov's squota series, LWN). **Squotas** (`btrfs quota enable --simple`, kernel ≥ 6.7; trixie has 6.12) cost almost nothing. They have two drawbacks: they change the on-disk format incompatibly (older kernels or DSM can no longer mount the volume), and they cannot tell how much space deleting a snapshot would free (exclusive = shared). Choices:
- shared folder quotas with squota, enabled only on request;
- "space held by snapshots" computed on request as a task (`btrfs filesystem du -s`, or enabling qgroups only for the duration of the computation);
- **never classic qgroups enabled by default**.

## 4. Proposal for rynas

### 4.1 The engine: btrbk as engine, Rust for reading and restore

| Criterion | btrbk (generated configuration) | Native Rust (libbtrfsutil or ioctl) |
|---|---|---|
| Reliability | 10 years of production use: incremental parent, garbled, GFS retention, locking | everything to rewrite and test, precisely the most delicate part |
| On-disk format | btrbk's by construction | must be imitated by hand |
| Compatibility with the CLI, btrbk-nas, client PCs using btrbk | total: the user can run `btrbk -c /etc/rynas/btrbk/photos.conf list` | only if imitated well |
| Dependencies | Perl and the Debian `btrbk` package (require ≥ 0.32.7, possibly in the rynas repo) | none (`libbtrfsutil` 0.7.1 MIT, unchanged since 2024; or direct ioctls as in btrbk-nas) |
| Error handling | `--format=raw` output + `transaction_log` | complete |

**Decision:**
- **btrbk does:** scheduled snapshots, retention, send/receive (push, pull, raw), prune.
  - One `/etc/rynas/btrbk/<job>.conf` file per job, with a "generated by rynas" header and the hash (anti-drift rule already on the roadmap).
  - rynasd runs it with `btrbk -c <conf> run --format=raw` inside a task (UPID, SSE), scheduled by one systemd timer per job.
  - Retention preview with `btrbk -c <conf> prune -n`.
- **`rynas-btrfs` (Rust, from the btrbk-nas code) does:**
  - listing of subvolumes and snapshots, freshness, garbled detection;
  - browser and download, restore;
  - single deletion with chain check;
  - **pre-update** snapshots with a single ioctl (`BTRFS_IOC_SNAP_CREATE_V2` with the RDONLY flag), always with btrbk names (`@rootfs.20261105T1203`), so btrbk sees and prunes them.
- **No snapper and no Timeshift**: they would add a second world of names and retention. Their ideas are borrowed instead: apt pre/post, timeline, rollback by rename.

### 4.2 On-disk layout

```
<volume> (top level, mounted by rynasd at /run/rynas/top/vol1, subvolid=5)
├── photos/                    subvolume = shared folder (/vol1/photos)
├── docker/stacks/<stack>/     one subvolume per stack (D-DOCKER)
├── docker/engine/             data-root, excluded from snapshots and backup
├── .snapshots/                btrbk snapshot_dir, hidden from SMB/NFS
│   ├── photos/photos.20261105T1200
│   └── stacks/<stack>.20261105T0300
└── .receive/<pc>/             PC targets (backup plugin)
root (system disk): @rootfs, @var_log, @snapshots/rootfs/@rootfs.20261105T1203
```

### 4.3 Scope

| Feature | Where | Phase |
|---|---|---|
| Scheduled snapshots per shared folder, GFS presets + preview | storage (native) | 1 |
| Snapshot browser, restore of a file or folder (copy alongside, or in place with the current version saved first), rollback of the whole folder | storage + file manager | 1 |
| Previous Versions over SMB (shadow_copy2) | shares-smb | 1 |
| Restore point before apt (`DPkg::Pre-Invoke` hook → `rynasctl snapshot system --reason apt`, deduplicated if nothing changed) and rollback by rename, coordinated with synodeb-dom | core/updates + storage | 1 |
| Snapshot before updating a docker stack, and rollback | docker plugin | 2 |
| NAS→NAS / NAS→USB replication (btrbk push/pull, ssh keys, `ssh_filter_btrbk.sh`, zstd, rate limit) | backup plugin | 2 |
| Receiving PC backups (ported btrbk-nas + endpoint preparation) | backup plugin | 2 |
| restic to S3/B2/SFTP/rest-server, verify (`check --read-data-subset`), prune | backup plugin | 2 |
| Replication verify (`received_uuid`, read-only, target scrub), notifications on state changes | backup + core | 2 |
| Switchover and failover (promoting the copy: snapshot → writable subvolume + shared folder; reverse replication from the common parent) | business | 3 |
| "Locked" snapshots and **hardened** pull targets (the source NAS holds no credentials to the backup), S3 Object Lock, append-only rest-server | business | 3 |
| Compliance report (RPO met, last verify, PDF/CSV), multi-NAS replication dashboard | business | 3 |
| Bare-metal restore of PCs from the NAS (iPXE + script) | plugin | 3 |

### 4.4 UI

New sidebar entry: **Data protection**. Per volume and subvolume, the Snapshots tab stays in **Storage**. Previous versions are also available in the **file manager**.

```
Data protection
┌──────────────┬──────────────────────────────────────────────────────┐
│ Snapshots    │ Folder     Schedule         Retention        Last     │
│ Replication  │ photos     hourly           GFS 24h 14d 8w 12m  12:00 ●│
│ Cloud        │ docs       hourly           GFS 24h 14d 8w 12m  12:00 ●│
│ PC backups   │ stacks/*   before update    last 5              03:00 ●│
│ System       │ [+ New task]  [Retention preview]                      │
│  restore     │                                                       │
└──────────────┴──────────────────────────────────────────────────────┘
New task: Folders [photos ▾]  Every [1 h ▾]  Keep: hours 24 | days 14 |
weeks 8 | months 12 | years 0   Minimum: [2 d]   → "3 snapshots will be deleted"

System restore
 ● 2026-11-05 12:03  before apt (14 packages, kernel 6.12.48→6.12.57)  [Restore…]
 ○ 2026-11-01 03:00  weekly
 Restore = rename @rootfs → @rootfs.broken-…, reboot; kernel: DEBIAN_OLD

PC backups  (formerly btrbk-nas)
 desktop  @ ✓ 2 h  @home ✓ 2 h  parent: @home.20261105T1000  [Browse] [Prune]
 laptop   @ ⚠ 31 h  interrupted receive (1)                   [Details]

File manager → right click → Previous versions
 report.xlsx   today 12:00 · today 11:00 · yesterday · 2026-10-29   [Open] [Restore] [Download]
```

### 4.5 Mistakes to avoid
- Different naming schemas for snapshots and replication: in TrueNAS the replication no longer finds its base. In rynas btrbk always decides the name.
- Deleting the common parent: it breaks the incremental chain and the next transfer starts from scratch. It must be blocked, as DSM and btrbk-nas do.
- Classic qgroups enabled by default.
- A system rollback that changes `rootflags` or forgets the kernel on the DOM.
- Believing snapshots are a backup: Timeshift and the shadow_copy2 man page say so too. The UI must push towards a copy **off the NAS**.
- ssh targets with full root access: use `ssh_filter_btrbk.sh` (≥ 0.32.7) with `--target`/`--restrict-path`.
- Using rustic in production now: it is still beta.
- Promising "immutable" when root can remove `chattr +i`. Without a hardened pull target, say "protected", not WORM.

## 5. Snapshots inside the share, deleting a share with its data, folders without a share

Added 2026-11-05 for three requests of the maintainer.

**What others do.**
- *Snapshot folder in the share.* DSM shows a `#snapshot` folder at the share root when "make
  snapshots visible" is on (names `GMT+01-2026.11.05-12.00.00`, also visible over SMB); ZFS has the
  virtual `.zfs/snapshot/<name>` (`snapdir=hidden|visible`, mounted on access); NetApp ONTAP has
  `~snapshot`/`.snapshot`; QNAP `@Recently-Snapshot`; snapper puts `.snapshots/<n>/snapshot` inside
  the subvolume. Common traits: read-only, named after the snapshot, off or hidden by default,
  never part of the next snapshot.
- *Deleting a share.* DSM deletes the shared folder with its data (and its snapshots) after a
  warning; TrueNAS separates the share (deleting it keeps the dataset) from the dataset (deleting
  it asks to type its name and lists what depends on it: shares, snapshot and replication tasks).
  OMV asks whether to delete the content too.
- *Folders without a share.* TrueNAS lists every dataset, shared or not; DSM has no such list
  (a folder left by a deleted share is invisible until shared again).

**Choices for rynas.**
- **Snapshot folder = one read-only bind mount per share of the job's snapshot directory**
  (`<volume>/.snapshots/<job>` → `<share>/#snapshot`), as a managed systemd mount unit pulled in
  by `rynas-snapshot-folders.target`. Off by default (`show_snapshots`).
  - It follows btrbk by itself: new and pruned snapshots appear and disappear at once, no
    reconcile per snapshot. A bind mount *per snapshot* (readable names such as `2026-11-05 12.00`)
    was rejected: a mount pins the snapshot subvolume and the kernel refuses to delete a mounted
    subvolume (`EBUSY`, see the 2015 btrfs "prevent deletion of mounted subvolumes" patch), so
    retention would fail; it would also mean dozens of mounts per share. Names stay btrbk's
    (`photos.20261105T1200`: date and time, sorted chronologically), the same names previous
    versions use.
  - Read-only at the VFS level (`ro,nosuid,nodev,noexec`): clients cannot create, rename or
    delete anything; the mount point cannot be removed while mounted. The default veto list already
    hides `#snapshot`; it is not vetoed while the folder is shown, and `recycle:exclude_dir` keeps
    the recycle bin out of it.
  - No snapshots of snapshots and no double counting: a btrfs snapshot never crosses a mount (the
    snapshot holds an empty `#snapshot` directory), btrfs space accounting does not change, and
    rynas' own walks stay on one filesystem (folder sizes, `rm --one-file-system`). There is no
    file-level backup in rynas yet: btrbk send/receive works per subvolume and never sees the
    mount; future file-level backups (restic) must run with `--one-file-system` or exclude
    `#snapshot`.
  - Survives reboots (units in `/etc/systemd/system`, rynasd's private mount namespace never
    mounts anything itself); a share rollback unmounts it and mounts it again on the new
    subvolume.
  - SMB: yes. NFS: only when the export also exports the mounts below it (`crossmnt`), shown as a
    warning on the share.
- **Deleting a share with its data**: off by default in the delete dialog; when on, a plan
  (size of the folder, snapshots, recycle bin, what else uses it: NFS export, snapshot jobs,
  scheduled tasks, containers, nested shares), typed confirmation of the share name. It is staged
  with the share deletion (and the removal of the share's snapshot jobs) and runs at the end of
  the apply task, after the services stopped using the folder. Only folders directly on a rynas
  volume can be deleted: a share may point at `/home/alice` or `/srv`, which rynas never deletes.
- **Unshared folders**: the top-level folders of the rynas volumes no share uses (system names
  `.*`, `@*`, `#*`, `lost+found` left out), with size (bounded walk), snapshots, and when the share
  was removed. A plain share deletion records the share's settings, so "Share again" opens the
  create form with the previous name, permissions and settings. "Delete permanently" is a task
  with the same plan and typed confirmation.

## 6. Snapshot replication: design (phase 2)

Added 2026-11-08. A second copy of a snapshot job's snapshots on another btrfs volume of the
NAS (typical: system/apps on an NVMe RAID1 volume, copy on an HDD volume) or on another Linux
host over SSH, with its own retention. Decision D-REPL.

**What others do.**
- *DSM Snapshot Replication*: replication tasks per shared folder, schedule down to 5 minutes,
  Smart Retention (GFS), the latest replicated snapshot cannot be deleted, recovery as
  switchover, failover, re-protect and *test failover* (a writable copy on the destination).
- *TrueNAS replication tasks*: transport SSH, SSH+netcat or local; bound to periodic snapshot
  tasks, "Run Automatically" right after the snapshot task or its own schedule; destination
  retention "same as source", custom or none; "replicate from scratch" (destroys the
  destination, off by default); bandwidth limit, stream compression, "save pending snapshots"
  (snapshots not yet replicated are not pruned), retries; read-only policy of the destination.
- *btrbk*: `target send-receive <dir|ssh://host:port/dir>` per subvolume, `target_preserve` /
  `target_preserve_min` independent of `snapshot_preserve`, incremental from the latest common
  parent (full send otherwise), `stream_compress`, `rate_limit` (mbuffer), `ssh_identity`,
  `ssh_user`, `backend_remote btrfs-progs-sudo`; `snapshot` creates and prunes snapshots only,
  `resume` sends the missing backups only, `clean` deletes interrupted receives (no
  `received_uuid`); the latest snapshot and the latest snapshot/backup pair are always kept, and
  every snapshot is kept while a target is unreachable. On the remote side
  `ssh_filter_btrbk.sh --target --delete --restrict-path` pins the key; **before 0.32.7 the
  filter's allow-list had no end anchor (CVE-2026-62943)**, Debian trixie still ships 0.32.6.
- *btrfs send/receive*: a received subvolume carries `received_uuid` and must stay read-only;
  flipping it writable keeps (or, on newer kernels, clears with `-f`) the received data and
  breaks later incremental receives, so a writable copy is a new snapshot of it, never the
  replica itself.
- *OpenMediaVault* has per-folder btrfs snapshots without replication (scripts or btrsync
  around it); no Unraid equivalent was found.

**Choices for rynas.**
- **A target belongs to its snapshot job** (`SnapshotJob.replicas`, key `snapshots`), not a
  separate task kind: btrbk then sees the snapshots and the targets in one configuration
  (`/etc/rynas/btrbk/<job>.conf`), picks the parent itself, never prunes the pair the next
  incremental needs and keeps everything while a target is down (TrueNAS' "save pending
  snapshots" for free), and the names stay btrbk's on both sides (no naming-schema mismatch).
  Each target is a `target send-receive` section with `group replica-<id>`.
- **When**: after each snapshot run of the job (default, DSM/TrueNAS "run automatically"), or on
  its own schedule: scheduler task `replica-<job>-<id>`, action `replicate`, staged with the
  target. A job with targets runs `btrbk snapshot` (create + prune the source), then the targets
  that follow it; a transfer is `btrbk clean replica-<id>` (interrupted receives, btrbk-named
  only) then `btrbk resume --preserve-snapshots replica-<id>` (missing replicas, target
  retention). An interrupted transfer starts that snapshot again at the next run (btrfs send
  has no byte-level resume); everything already received stays. btrbk's lock is shared with
  the job: a transfer waits (20 s steps, 15 min) while the job runs.
- **Retention on the target**: GFS counts and minimum like the snapshots, default 14 daily,
  8 weekly, 12 monthly, 2 yearly, everything of the last day. btrbk only deletes, in the target
  directory, subvolumes matching its own `<job>.YYYYMMDDTHHMM` names: never what rynas did not
  create. There is no "replicate from scratch".
- **Local target**: a folder on another btrfs volume, suggested `/<volume>/.replicas/<job>`
  (mode 0700, created at the first run). Refused when on the filesystem of the source (same
  btrfs UUID: not a second copy) or not on btrfs (non-btrfs destinations are restic's, D2);
  the first run refuses to create it when no volume is mounted there (it would land on the
  system disk).
- **Remote target**: `ssh://user@host:port/path`, key-based with a dedicated ed25519 pair
  (`/etc/rynas/ssh/replication_ed25519`, created on request). rynas shows the public key and
  the exact `authorized_keys` line
  (`command="…/ssh_filter_btrbk.sh --target --delete --restrict-path <path> [--sudo] --log",restrict <key>`)
  and warns that the server needs btrbk ≥ 0.32.7. Host keys are read with `ssh-keyscan`, shown
  with their SHA256 fingerprints and stored only once confirmed; they go to rynas's own
  `/etc/rynas/ssh/known_hosts`. btrbk runs with `PATH=/etc/rynas/ssh/bin:…`, where `ssh` is a
  wrapper written by rynas: `/usr/bin/ssh -F none` with every option on the command line
  (`StrictHostKeyChecking=yes`, `UserKnownHostsFile=/etc/rynas/ssh/known_hosts`,
  `GlobalKnownHostsFile=none`, `IdentitiesOnly=yes`, `-i <key>`, `BatchMode`, no agent, proxy,
  forwarding or connection sharing, keepalives). The earlier
  `ssh_config.d/60-rynas-replication.conf` was dropped (security review 2026-11-08): ssh takes
  the first value it finds, so root's `~/.ssh/config` or an earlier `ssh_config.d` file could
  override it; with `-F none` no file is read and nothing else of root's ssh changes. The key
  folder is forced to 0700 and the key to 0600 at every use (a symlink is refused). The remote
  folder takes `[A-Za-z0-9_@+.-]` parts only: it goes into the remote `authorized_keys` command
  and into the regular expression of `--restrict-path`. Non-root users run btrfs through
  `sudo -n` (`backend_remote btrfs-progs-sudo`). Compression (`zstd`, `lz4`, `gzip`) only for
  remote targets; bandwidth limit for both (`rate_limit`, needs mbuffer).
- **Status per target** (`<state>/replication/<job>/<id>.json`): last success, last attempt,
  duration, replicas sent, newest replica, replicas pruned, last error with a hint (key refused,
  host key, unreachable, filter, missing folder, not btrfs, no space, busy, btrbk missing) that
  the UI turns into the fix; next run from the job's or the target's timer. Notifications on
  state changes only: `replication.failed` (firing) / `replication.ok` (resolved), key
  `replication:<job>:<id>`, category backup, fields `job`, `target`, `name`, `hint`,
  `held_snapshots`. Because btrbk keeps every source snapshot a target lacks, a target that
  stays down makes snapshots pile up on the source: the listing reports `held_snapshots` (job
  snapshots newer than the newest replica) and the failure notification says how many are
  kept and that they use more and more space. A failed
  transfer never fails the snapshot run it follows. Transferred bytes are not reported: btrbk's
  raw output has no size (open point).
- **Restore** (local targets): list the replicas (read-only, interrupted receives flagged),
  browse them (descriptor walk of `rynas-btrfs::browse`), copy a file or folder back next to
  the original in the job's live subvolume, and **promote**: a writable snapshot of a complete
  replica as a new folder at the top of the target's volume (typed confirmation), ready to be
  shared; the replica stays read-only so later transfers continue. Replicas on another host are
  browsed there (the filter allows no reads); switchover/failover with reverse replication stay
  phase 3.
- Reusable pieces (`rynas_btrfs::btrbk_ssh`): key pair, filter command, `authorized_keys` line,
  key scan, fingerprints, known_hosts lines, for the "NAS as a btrbk target" side too.

API: `GET/POST /api/v1/snapshots/replication`, `GET/PUT/DELETE …/{job}/{id}`,
`POST …/{job}/{id}/run` (task `replication.run`), `GET/POST …/ssh-key`, `POST …/host-keys`,
`GET …/{job}/{id}/snapshots`, `GET …/snapshots/{name}/browse?path=`,
`POST …/snapshots/{name}/restore` (task `replication.restore`),
`POST …/snapshots/{name}/promote` (task `replication.promote`). UI: Snapshots → Replication.

## Sources
- btrbk: [btrbk.conf(5)](https://digint.ch/btrbk/doc/btrbk.conf.5.html), [tags and release 0.32.7](https://github.com/digint/btrbk/tags)
- Synology: [Snapshot Replication spec](https://www.synology.com/dsm/software_spec/snapshot_replication), [Data Protection White Paper](https://cndl.synology.cn/download/Document/Software/WhitePaper/Firmware/DSM/All/enu/Synology_Data_Protection_White_Paper.pdf)
- TrueNAS 25.10: [Periodic Snapshot Tasks](https://www.truenas.com/docs/scale/25.10/scaletutorials/dataprotection/periodicsnapshottasksscale/), [API pool.snapshottask.create](https://api.truenas.com/v25.10/api_methods_pool.snapshottask.create.html), [forum Snapshot/Replication Basics](https://forums.truenas.com/t/snapshot-replication-basics/68171)
- qgroups and squota: [LWN 930797](https://lwn.net/Articles/930797), [LWN 944371](https://lwn.net/Articles/944371), [squota patch](https://patchwork.kernel.org/project/linux-btrfs/cover/cover.1690495785.git.boris@bur.io/)
- Samba: [vfs_shadow_copy2](https://manpages.debian.org/wheezy-backports/samba-vfs-modules/vfs_shadow_copy2.8.en.html), [OMV guide](https://forum.openmediavault.org/index.php/Thread/7331-GUIDE-Windows-Previous-Versions-and-Samba-Btrfs-Atomic-COW-Volume-Shadow-Copy/)
- Timeshift: [linuxmint/timeshift](https://github.com/linuxmint/timeshift/), [Fedora discussion](https://discussion.fedoraproject.org/t/cannot-configure-timeshift-with-btrfs/165118)
- snapper and apt: [snap-apt](https://github.com/pavinjosdev/snap-apt), [Salsa MR](https://salsa.debian.org/debian/snapper/-/merge_requests/2)
- Rust: [libbtrfsutil](https://docs.rs/crate/libbtrfsutil/0.1.0), [libbtrfsutil-sys](https://lib.rs/crates/libbtrfsutil-sys)
- rustic: [AltLinux](https://packages.altlinux.org/en/sisyphus/srpms/rustic/3360151391433197853), [OpenBSD](https://www.openbsdhandbook.com/packages/release/sysutils/rustic/)
- Snapshot folders and deletion: [btrfs "prevent deletion of mounted subvolumes" (LKML, 2015)](https://lore.kernel.org/lkml/20150401112242.GG6821@suse.cz/T/), [ZFS `snapdir` property](https://openzfs.github.io/openzfs-docs/man/master/7/zfsprops.7.html), [NetApp: snapshots hidden in ~snapshot](https://kb.netapp.com/on-prem/ontap/da/NAS/NAS-KBs/Snapshots_are_not_visible_inside_the_~snapshot_folder_because_they_are_marked_as_hidden), [QNAP @Recently-Snapshot](https://www.qnap.com/en/how-to/faq/con_show.php?cid=2343), [forum: DSM snapshot folder over SMB](https://www.synoforum.com/threads/hide-snapshot-folder.10074/)
- Replication: [btrbk(1)](https://digint.ch/btrbk/doc/btrbk.1.html), [ssh_filter_btrbk(1)](https://digint.ch/btrbk/doc/ssh_filter_btrbk.1.html), [CVE-2026-62943 (Debian tracker)](https://security-tracker.debian.org/tracker/CVE-2026-62943), [Red Hat bug 2538186](https://bugzilla.redhat.com/show_bug.cgi?id=2538186), [TrueNAS 25.10 replication screens](https://www.truenas.com/docs/scale/25.10/scaleuireference/dataprotection/replicationscreensscale/), [Synology Snapshot Replication 7.0 spec](https://www.synology.com/en-us/dsm/7.0/software_spec/snapshot_replication), [btrfs: received info on ro→rw switch (lore)](https://lore.kernel.org/all/20210908183312.GU3379@twin.jikos.cz/T/), [OMV 6.3 btrfs snapshots](https://www.openmediavault.org/?p=3404)
- Maintainer's projects: [btrbk-nas](https://github.com/rylos/btrbk-nas) (README, `src/btrfs.rs`), [synodeb](https://github.com/rylos/synodeb) (`pkg/synodeb/usr/sbin/synodeb-dom`)

**To be verified:**
- Synology and its product pages only state that immutable snapshots and GFS exist. How the two rules interact is not documented.
- snapper's apt hook on trixie has only been seen in 2023 sources.
- Rollback by rename with the kernel on the DOM comes from an analysis of the synodeb code, not yet tested.
