# PC backup in rynas: ABB-style, bare metal, btrbk or restic

**In short:** rynas gets a centrally managed **PC backup** in the style of Active Backup for Business: devices paired with the NAS, plans pushed from the NAS, a console with the state of every device, file browsing, and **bare-metal restore** from a rynas recovery environment. The **data engine is chosen per device by the user**: **btrbk** (btrfs send/receive, recommended when the PC has btrfs) or **restic/rustic** (any filesystem, any OS restic supports). Bare metal comes from a **system layout manifest** saved next to the data, not from a block image. This refines D-SNAP ("PC backups, ex btrbk-nas") and D-BACKUP; decision D-PCBACKUP.

Written 2026-11-06 from Marco's request ("voglio un backup in stile abb che permetta anche il bare metal. Btrbk quando possibile e restic/rustic negli altri casi. Ma sceglie l'utente") and from his own disaster-recovery practice, documented in his notes: btrbk + `restore_btrfs_system`, `abb-agent`/`abb-restore`, and the retired rustic full-system restore.

---

## 1. What the maintainer's own recovery tools already prove

| | btrbk + `restore_btrfs_system` | abb-agent + abb-restore (ABB) | rustic full system (retired) |
|---|---|---|---|
| Data | btrfs snapshots `@`, `@home`, `@games` received on the NAS | block image of the whole disk, changed-block tracking | files of `/` in a restic repository |
| Layout | **`sfdisk -d` dump inside the `@` snapshot** (`.bootbackup/part_table_*`, written by a pacman hook) and **ESP copies** (`/boot` `_pre`/`_post`) | in the image (partitions, PARTUUIDs, bootloader) | none: partitions and subvolumes recreated by hand |
| Boot media | Arch ISO **netbooted with iPXE from the NAS** (router TFTP → NAS HTTP), USB stick as fallback | any live system + a static `abb-restore` binary | Arch ISO + rustic |
| Restore steps | disk chosen by **model + serial** (never `nvmeX`), sfdisk, grow the last partition, mkfs, `btrfs receive`, rw snapshot, recreate excluded subvolumes, copy ESP, rewrite PARTUUID in fstab and loader entries, `bootctl install` in chroot | guided TUI: server, task, version, target disk, whole device / system volumes / single volume, `--exclude-subvolume`, `--grow`; `abb-agent-fixup` recreates excluded subvolumes at first boot | mkfs, subvolumes, `rustic restore latest:/ /mnt --glob '!/etc/fstab'`, `bootctl install`, `mkinitcpio -P` |
| Strengths | per-subvolume, per-date choice; additive restore of `@home`/`@games`; dry-run that exercises the fixups; guard "target disk in use"; handles GUIDs already present on another attached disk | **one-step bare metal**; whole device or single volumes; agent-side scheduling (`on_boot`, `on_boot_delay`, interval, `only_if` gateway+MAC for laptops, `wait_locks`), hooks, `status` with progress | any filesystem |
| Weak spots | never tested on real hardware; root ssh key in clear on a USB disk; hook copy in `/etc` drifts from the repo | depends on a proprietary server (no ABB on Debian); `node changed after generation read` races at boot | manual layout, slow restore of many small files |

**Conclusion:** bare metal needs three things beyond the data — the **layout** (partitions, filesystems, subvolumes, mount options, ESP, bootloader, encryption), a **boot environment** with network, and a **restore tool** that replays them safely. The data can be btrbk snapshots or restic snapshots: Marco's btrbk script already does bare metal without any image.

## 2. Other products (checked 2026-11-06)

- **Active Backup for Business:** the target experience — pairing, templates (schedule, retention, excludes), console per device, file portal, bare-metal media builder, block-level incrementals. Not available on Debian.
- **UrBackup** (server 2.5.38, 2026-08-30; open source): file **and image** backups for Windows, Linux file backups, web console, bootable restore media for images. The reference for **Windows bare metal** in open source; its Linux image backup is limited (dattobd/LVM snapshots).
- **Relax-and-Recover (ReaR):** the reference for layout capture and recovery media on Linux (`disklayout.conf`: partitions, md, LVM, LUKS, btrfs subvolumes; recovery ISO/PXE; layout recreation with UUID mapping). **No restic workflow**: `BACKUP=EXTERNAL` with restic commands. btrfs single-device only (2.9). Good ideas to copy, too heavy to embed.
- **Veeam Agent Free:** Windows/Linux image to an SMB share, recovery media. Proprietary; usable with a plain rynas share.
- **Backrest / Zerobyte:** restic UIs — scheduling, retention, browse, hooks. No bare metal.

## 3. Design

### 3.1 Roles

- **NAS (rynas, `backup` plugin, page "PC backup"):** devices, plans, receiving, retention and maintenance, console, file browsing, recovery media.
- **PC (`rynas-agent`, new small Rust binary, static musl, packages for Debian/Ubuntu/Fedora/Arch + AUR):** pairing, local snapshots, sending, layout manifest, status reports. Lessons from abb-agent built in: `on_boot` + `on_boot_delay`, interval, `only_if` (gateway IP **and MAC**, so a laptop backs up only on the home LAN), `wait_locks`, retry with backoff on btrfs generation races, a `status` command with progress, **configuration owned by the NAS** (no `.pacnew` traps: the local file only holds the pairing).
- Windows/macOS: restic engine only, phase 3, files only (see §3.6).

### 3.2 Engine per device, chosen by the user

The pairing wizard inspects the PC (agent report) and proposes:

| | btrbk | restic/rustic |
|---|---|---|
| Requires | btrfs on the PC, data in subvolumes | nothing (any FS; Linux first) |
| Transfer | incremental `btrfs send`, exact copy (xattrs, ACLs, reflinks, sparse files) | dedup + compression, encrypted at rest |
| NAS side | received subvolumes in `/<volume>/.receive/<device>/` (D-SNAP layout), one subvolume per device for quota | rest-server repository per device in `/<volume>/.restic/<device>/`, `--private-repos --append-only` |
| Encryption at rest | volume LUKS (D-CRYPT) | restic key (+ volume LUKS) |
| Browse | snapshots are folders: file manager directly | restic snapshot browser in the file manager (D-BACKUP) |
| Bare metal | yes (§3.4) | yes (§3.4) |
| Recommended | **when the PC has btrfs** | otherwise, or when encryption-at-rest per device or a cloud copy is wanted |

The user can override the proposal; a device can also use **both** (e.g. btrbk for `@`/`@home`, restic for an ext4 data disk). Switching engine later starts a new chain (full first transfer), said in the UI.

### 3.3 Who drives: NAS-driven like ABB

- **btrbk, pull mode (default):** the NAS runs btrbk against `ssh://pc` with `ssh_filter_btrbk.sh --source --restrict-path` on the PC (installed by the agent). Retention (`target_preserve`, GFS presets) and schedules live on the NAS; **the PC holds no credential that can delete backups** (ransomware on the PC cannot touch them). The agent announces "I'm up, on the right network, idle" and the NAS starts the job: this fixes laptops that are off at schedule time. Local snapshot retention on the PC is set by the NAS too.
- **btrbk, push mode (option):** for PCs the NAS cannot reach (behind NAT, roaming): the agent runs btrbk with a NAS target restricted by `ssh_filter_btrbk.sh --target`.
- **restic:** always push (restic is client-side). The agent gets a per-device rest-server credential; append-only; **forget/prune/check only on the NAS** under the per-repository maintenance lock (D-BACKUP), with a rynas key added to the repository (consequence written in the UI: whoever owns the NAS can read the backups, as with ABB).
- **btrbk ≥ 0.32.7** required on both sides (fix in `ssh_filter_btrbk.sh`); trixie needs a backport.

### 3.4 Bare metal without images: the layout manifest

At every backup the agent writes a **`rynas-layout.json`** (plus raw dumps) next to the data — inside the `@` snapshot for btrbk (like `.bootbackup/` today, but written by the agent, not by a pacman hook that drifts), as a separate snapshot tag `layout` for restic:

- disks: model, serial, size, logical/physical sector size;
- partition tables: `sfdisk --dump` (GPT GUIDs, PARTUUIDs, types, names);
- filesystems: type, label, UUID, creation options (`btrfs` features, `mkfs.vfat` volume id);
- btrfs: subvolume tree, default subvolume, which subvolumes are backed up and which are recreated empty (`@log`, `@pkg`, caches), mount options from fstab;
- md/LVM/LUKS: layout; for LUKS the header backup **encrypted with the device's recovery key**, never in clear;
- **ESP**: a tar of its content (FAT is outside btrfs and restic sees it as plain files anyway);
- bootloader: systemd-boot / GRUB / rEFInd / UKI, kernel command line, Secure Boot state;
- fstab, crypttab, `/etc/kernel/cmdline`, initramfs generator (mkinitcpio, dracut, initramfs-tools).

**Recovery environment** (`rynas-recovery`): a small Debian live image with the agent's restore mode (`rynas-agent restore`, static binary, TUI wizard like abb-restore):

- booted **over the network from the NAS** (rynas serves iPXE/HTTP boot; on the LAN with the router's DHCP options, or a tiny iPXE USB stick that chain-loads from the NAS) or from a **USB image downloaded from the rynas UI** with the NAS address and a one-time token baked in;
- steps: connect to the NAS → device → version (per subvolume/date, like `restore_btrfs_system --snapshot-root`) → target disk **by model + serial**, with the "disk in use" guard → **dry run** showing every command and the fstab/loader diff → recreate partitions (sfdisk; grow the last partition on a larger disk; shrink refused unless the data fits) → mkfs **with the original UUIDs** when no disk with those UUIDs is attached (no fstab rewrite needed), new UUIDs + rewrite of fstab/crypttab/loader entries/cmdline otherwise → data: `btrfs receive` + rw snapshots, or `restic restore` → recreate excluded subvolumes → ESP → chroot: bootloader install + initramfs regeneration → summary;
- modes: whole device, system only, single subvolume/volume (additive, like `--home-only`), files only;
- **restore test** (phase 3, needs the virtualization plugin): restore a device into a VM on the NAS and boot it, as Marco did by hand for abb-agent 0.2.2, scheduled monthly with a screenshot in the console.

### 3.5 Console (from btrbk-nas, ABB and Backrest)

- one card per device: engine, last good backup, next run, state (ok / late 26 h / critical 72 h, thresholds per device), size, quota, transfer rate, last error from the agent;
- btrbk: per-subvolume freshness, interrupted receives, next incremental parent, chain-safe deletion (all from btrbk-nas, ported to `rynas-btrfs`);
- restic: snapshots, repository size, dedup, maintenance state;
- **"Bare metal ready" check per device:** layout manifest present and recent, ESP saved, bootloader known, recovery key stored; red if any is missing;
- files: browse in the file manager, download file/folder as tar/zip, restore to a share;
- notifications (D-NOTIFY) on state change only;
- templates like ABB: schedule, retention preset, excludes (caches, `Downloads`, `Games`, `node_modules`, `CACHEDIR.TAG`, `.nobackup`), applied to new devices.

### 3.6 What stays out or later

- **Windows and macOS clients (phase 3, restic engine):** restic ships official Windows and macOS binaries and takes a **VSS snapshot on Windows** (`--use-fs-snapshot`); rustic also ships Windows and macOS binaries, but VSS support in rustic is **not verified** (to check before choosing it there). The Windows/macOS agent therefore drives **restic** by default (rustic only once VSS is confirmed). macOS: no snapshot in restic; the agent takes an APFS local snapshot (`tmutil localsnapshot`), mounts it read-only and backs it up; needs Full Disk Access. Files only: no bare metal on these systems.
- **Windows bare metal:** not in core. restic covers Windows files (VSS) in phase 3; for images, UrBackup as a curated app or Veeam Agent Free to an SMB share, documented.
- **Block-level images** of non-btrfs Linux systems: not planned; restic + layout manifest gives bare metal at file level.
- **Encrypted homes (ecryptfs):** backed up as encrypted files (same as ABB today on NB-Marco); restore gives them back as they were.

## 4. Coverage of Marco's machines

All three run Arch Linux on btrfs, so all can use btrbk:

| Device | Today | With rynas |
|---|---|---|
| pc-casa | btrbk push to the NAS + ABB image | btrbk (pull), bare metal from the manifest; replaces both |
| pc-work | ABB image on vdsm, Timeshift locally | btrbk (pull) to a rynas NAS; Timeshift's snapshots excluded or replaced by the agent's local snapshots |
| NB-Marco (Surface, Arch, ecryptfs home) | ABB image, only on the home LAN | btrbk (pull) triggered by the agent when on the home LAN (`only_if` gateway + MAC) |

Not covered by btrbk: non-btrfs or non-Linux devices (restic); Windows bare metal (§3.6).

## 5. Phases

- **Phase 2 (backup plugin):** receiving btrbk backups and the console (btrbk-nas port, D-SNAP); restic rest-server per device with server-side maintenance; manual pairing (ssh key / rest-server credential shown in the UI); file browsing.
- **Phase 2b:** `rynas-agent` (pairing, NAS-driven plans, pull trigger, layout manifest, status); `rynas-recovery` image with netboot and USB; bare-metal restore for btrbk first, then restic.
- **Phase 3:** restore test in a VM, Windows/macOS restic agent, multi-NAS, templates for many devices.

## 6. Mistakes to avoid

1. Recognising disks by kernel name (`nvme0n1`): the order changes between boots.
2. A layout hook that drifts from the tool (the pacman hook copy in `/etc`): the agent writes the manifest itself at every backup.
3. Credentials in clear on recovery media (the root key on the Kingston): one-time tokens, recovery key typed in.
4. Deleting the last common parent: the next btrbk transfer is full (btrbk-nas blocks it).
5. Restic prune from PC and NAS at once (#1946): maintenance only on the NAS.
6. Backups at boot racing with boot-time writes (abb-agent `node changed after the generation read`): boot delay and retry.
7. A local config file the package manager can overwrite (abb-agent `.pacnew`/`.pacsave`): plans come from the NAS.
8. Never testing the restore on real hardware: the console shows "last restore test" and nags.

## Sources

- Maintainer's notes: Disaster Recovery PC-Casa (btrbk + `restore_btrfs_system`, github.com/rylos/restore_btrfs_system), Disaster Recovery pc-work (ABB, retired rustic procedure), abb-agent (github.com/rylos/abb-agent), btrbk-nas (github.com/rylos/btrbk-nas)
- https://relax-and-recover.org/rear-user-guide/scenarios/restic.html · https://relax-and-recover.org/rear-user-guide/releasenotes/rear29.html
- https://urbackup.org/
- https://github.com/digint/btrbk (pull mode, `ssh_filter_btrbk.sh`, 0.32.7)
- `docs/research/snapshots-replication.md`, `docs/research/backup.md`
