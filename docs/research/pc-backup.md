# PC backup in rynas: our own ABB server and agent, for every system

**In short:** rynas gets its own **PC backup server** and a cross-platform **`rynas-agent`**, centrally managed in the style of Active Backup for Business, with **bare-metal restore**. Each device uses one engine, chosen by the user:

- **image**: block-level images of the used blocks with incrementals, our own engine grown from abb-agent;
- **btrbk**: btrfs send/receive;
- **files**: restic.

All three keep their data on the NAS, show up in one console, can be browsed in the file manager, and restore through one recovery environment.

This decision is D-PCBACKUP. It refines D-SNAP ("PC backups, ex btrbk-nas") and D-BACKUP.

Marco's requests, 2026-11-06/07:
- "voglio un backup in stile abb che permetta anche il bare metal. Btrbk quando possibile e restic/rustic negli altri casi. Ma sceglie l'utente";
- "una versione migliorata di abb server e abb client che va su tutti i sistemi. Fatta a modo nostro".

Sources: Marco's disaster-recovery notes and projects (§1), plus two rounds of web research done on 2026-11-07 (§2, sources at the end). **[U]** marks a claim that could not be verified.

---

## 1. What Marco's own tools already prove

| | btrbk + `restore_btrfs_system` | abb-agent + abb-restore | rustic full system (retired) |
|---|---|---|---|
| Data | btrfs snapshots received on the NAS | block image of the used blocks, incrementals without a kernel driver | files of `/` in a restic repository |
| Layout | `sfdisk -d` dump inside the `@` snapshot (written by a pacman hook) and ESP copies | inside the image | rebuilt by hand |
| Boot media | Arch ISO netbooted with iPXE from the NAS | any live system + the static `abb-restore` | Arch ISO + rustic |
| Strengths | choice of subvolume and date; additive restore; dry run that tries the fixups; disk picked by model + serial; "disk in use" guard | **one-step bare metal**; whole device / system / single volume; scheduling (`on_boot`, `on_boot_delay`, `only_if` gateway+MAC, `wait_locks`), hooks, `status` | works on any filesystem |
| Weak spots | never tested on real hardware; root key in clear on a USB disk; the hook copy drifts from the repo | needs a proprietary server; races with boot-time writes; `.pacnew`/`.pacsave` overwrote the config | manual layout, slow restore |

**abb-agent code that can be reused** (Rust; Marco is its only author, 124 + 44 commits, licence GPL-3.0-or-later):

- **`btrfs-image`** (about 3,500 lines):
  - read-only snapshots, with a capture step that avoids block-reuse races;
  - a map of the blocks in use;
  - per-block fingerprints taken from the btrfs checksums, which also catch writes to NOCOW files;
  - every block checked against its checksum before it is sent;
  - excluded subvolumes edited out of the image.
- **`ext-image`**:
  - used blocks from the group bitmaps;
  - incrementals by comparing the inode tables with the previous run;
  - a brief FIFREEZE while the metadata is read.
- **`abb-restore`**: GPT handling, restore plan, TUI, guards.
- **`abb-agent`**:
  - scheduler, `only_if`, hooks;
  - an 8-thread read/verify/LZ4 pipeline (about 2.2 GB/s);
  - fixup after restore;
  - file restore that does not follow symbolic links (`O_NOFOLLOW`).

Moving this code into rynas (AGPL-3.0-or-later since 2026-11-07) is **Marco's licensing decision**: GPL-3.0-or-later code can be combined with it, GPL-2.0-only code cannot. The copies already published stay GPL.

## 2. What the others do (checked 2026-11-07)

- **Synology ABB**:
  - one image per disk with an interval table;
  - deduplication in a shared pool, versions kept as btrfs snapshots;
  - files browsed through a FUSE image plus `losetup`/mount;
  - Linux snapshots through synosnap, a fork of elastio-snap. The fork was archived on 2025-01-02, more evidence that out-of-tree block drivers are fragile.
- **Proxmox Backup Server**:
  - **images in a fixed index** (`.fidx`, 4 MiB chunks) and files in a dynamic index (`.didx`);
  - chunks named by SHA-256, zstd, optionally AES-256-GCM;
  - dirty bitmaps map straight onto chunks;
  - an HTTP/2 backup protocol;
  - **files of an image browsed inside a micro-VM**, so the host never mounts filesystems it cannot trust;
  - no Windows client, no bare-metal agent for hosts ("on the roadmap"); AGPL.
  - We take the format ideas and none of the code.
- **UrBackup** (server 2.5.38 of 2026-08-30, restore media 2.6.0, AGPL):
  - Windows NTFS images stored as VHD, or as raw reflink copies on btrfs/ZFS;
  - the Windows change-block tracking (CBT) driver is a **paid** add-on;
  - Linux images go through dattobd, a kernel module that needs free space and has no support listed for 6.x kernels;
  - a macOS client for files.
- **Veeam Agent**: a CBT driver, proprietary.
- **Rescuezilla / Clonezilla**: images taken offline only, after a reboot.
- **wimlib** `wimcapture --snapshot`: captures a running Windows through VSS. **[U]** whether the image boots after `wimapply`.
- **ReaR 2.9** (2025-01):
  - `disklayout.conf` with `disk`, `part` (with PARTUUID), `raidarray`, `lvm*`, `crypt`, `fs` (UUID, label, btrfs features) and the btrfs subvolume lines;
  - it **generates a `diskrestore.sh` script you can read and edit**, shows a disk-mapping table, and keeps UUIDs when the hardware is the same;
  - **LUKS: a new passphrase at restore time, the header is not restored**;
  - no restic workflow.
- **Backrest multihost** (experimental): pairing tokens; the server owns forget, prune and check.
- **Kopia repository server**: one user per `user@host`; **clients never hold the repository password**, but by default they can delete their own snapshots.
- **rest-server 0.14**: `--append-only` applies to the whole server, not per user.
- **restic 0.19.1**:
  - Windows VSS with `--use-fs-snapshot`;
  - on macOS it records the snapshot's mount path (#2092 open), and there is no APFS support (#3714).
- **rustic 0.11.4**:
  - binaries for Windows and macOS;
  - **no VSS** (#1412 open, PR #1833 closed without merging);
  - wrong restores on Windows (#635), Windows paths that cannot be restored elsewhere (#1986);
  - **it does not honour restic's locks**.
- **btrbk**:
  - **CVE-2026-62943** in `ssh_filter_btrbk.sh` (the regex had no end anchor), fixed in 0.32.7;
  - **trixie and sid ship 0.32.6-2, vulnerable, with no DSA**;
  - the filter has `--source`, `--target`, `--delete`, `--restrict-path`, `--sudo`;
  - a restored subvolume is made writable with `btrfs subvolume snapshot`, never `property set ro false`, which breaks later incrementals.
- **Secure Boot netboot**: Microsoft signed the iPXE shim (ipxe-16.1) in November 2025. The simplest path stays UEFI HTTP boot of Debian's signed shim, GRUB and kernel.
- **macOS**:
  - images are not realistic (signed system volume, Apple Silicon); Carbon Copy Cloner too now makes non-bootable backups and restores with Migration Assistant;
  - files are backed up from an APFS local snapshot (`tmutil localsnapshot`, mounted read-only against `/System/Volumes/Data`) by a root LaunchDaemon inside a signed .app that has Full Disk Access;
  - EDR products flag `mount_apfs`.

## 3. Design

### 3.1 Roles

- **NAS** (rynas, `backup` plugin, "PC backup" page):
  - devices, templates and plans;
  - storage and maintenance (append-only for devices; prune, forget and check only on the NAS);
  - console, file browsing, recovery media.
- **`rynas-agent`** (one Rust codebase; Linux, Windows, macOS):
  - pairing with a one-time token;
  - plans pushed by the NAS: no local configuration a package manager can overwrite, only the pairing;
  - consistent snapshot, change tracking, sending;
  - layout manifest, status and progress;
  - the abb-agent scheduling: `on_boot` + `on_boot_delay`, interval, `only_if` gateway IP **and MAC**, `wait_locks`, retry with backoff on boot-time races, a `systemd-inhibit` sleep lock while a backup runs.

### 3.2 Engines, chosen per device by the user

| | **image** (own) | **btrbk** | **files** (restic) |
|---|---|---|---|
| Systems | Linux btrfs, ext2/3/4 (XFS and LVM thin later); Windows NTFS (phase 3a) | Linux with btrfs | Linux, Windows, macOS |
| What it takes | used blocks of each partition + ESP + layout | btrfs subvolumes | files |
| Incrementals | changed chunks from the filesystem metadata, no kernel driver | `btrfs send -p` | dedup |
| Bare metal | yes, in one step | yes (§3.5) | yes on Linux (§3.5); macOS no |
| File browsing | from the image (§3.6) | snapshots are folders | restic browser |
| When to pick it | ABB replacement, fastest bare metal, Windows | btrfs PCs that also want readable snapshots on the NAS | macOS, unusual filesystems, file-level only |

The wizard reads the agent's report and proposes an engine: image for Linux and Windows, btrbk when the user wants subvolume snapshots, files for macOS. The user can change it. A device can combine engines, for example an image of the system disk plus files from a data disk. Changing engine starts a new chain, and the UI says so.

### 3.3 Image engine: storage and transfer

- **Storage, recommended: the restic repository format with fixed 1 MiB chunks, written through `rustic_core`.** It is the same engine, maintenance lock and browse path as D-BACKUP.
  - Each partition is stored as files of 64 GiB segments (`disk-<serial>/p2/000.img`), each a list of 1 MiB blobs.
  - Free or zero chunks point to one shared zero blob.
  - Each version also has a `device-spec.json` (the layout manifest).
  - Emergency exit: stock `restic restore` or `restic mount` rebuilds the raw partitions.
  - Repository v2 gives zstd, and encryption is built in.
  - **First spike:** check that `rustic_core` can write a file node from a list of blob IDs. **[U]**
  - **Fallback:** our own store in the style of PBS: a fixed index, chunks named by SHA-256, mark-and-sweep GC.
- **Transport:**
  - rynas serves the restic REST backend v2 itself inside its axum API (`/api/v1/pcbackup/repo/<device>/`), so there is no separate rest-server;
  - a token per device, **append-only enforced by rynas for each device**;
  - several HTTP/1.1 connections in parallel.
- **Repository creation:** the NAS creates every repository and gives the device its own key. The device never runs `init`.
- **Maintenance:**
  - only on the NAS, under the per-repository lock of D-BACKUP;
  - **rynas refuses that device's writes while a prune runs**, because rustic does not honour restic's locks;
  - a rynas key is added to each repository so the NAS can browse and maintain it. Consequence written in the UI: whoever owns the NAS can read the backups, as with ABB.
- **Pipeline in the agent:**
  1. snapshot;
  2. map of used blocks;
  3. dirty chunks from the fingerprints (64 KiB, grouped into 1 MiB);
  4. read, verify, zstd, encrypt;
  5. upload the blobs the server lacks;
  6. new tree and snapshot.

  If the local chunk table does not match the server's base version, the run is full (the abb-agent rule).
- **Change tracking per system, with no kernel driver:**
  - **btrfs:** generations + checksums (`btrfs-image`);
  - **ext:** inode table comparison (`ext-image`);
  - **XFS:** later; until then, hash all used blocks;
  - **LVM thin:** `thin_delta` **[U]**;
  - **Windows:**
    - VSS + `FSCTL_GET_VOLUME_BITMAP` for the used blocks;
    - first version: hash every used chunk;
    - then a comparison of the `$MFT` with the previous run (the NTFS counterpart of `ext-image`), with the USN journal as a shortcut when it is continuous;
    - **no CBT driver** (EV signing, attestation, risk of crashes).

### 3.4 btrbk and files engines

- **btrbk, pull mode (default):**
  - btrbk runs only on the NAS; the PC needs only btrfs-progs and **a filter shipped and pinned by `rynas-agent`** (the fixed regex of 0.32.7, or an allowlist in Rust), whatever the distro's btrbk version;
  - an unprivileged user with sudo, filter options `--source --delete -p <root> --sudo` (`--delete` lets the NAS prune the PC's local snapshots);
  - the NAS pins the PC's ssh host key;
  - the agent tells the NAS "I'm up, on the right network, idle" and the NAS starts the job; this suits laptops that are off at schedule time;
  - off the home LAN: push mode, or a reverse ssh tunnel opened by the agent;
  - stated threat: a compromised NAS can read every paired PC and delete its local snapshots.
- **btrbk, push mode:** the target on the NAS uses rynas' own copy of the filter.
- **Files:**
  - **restic** on Windows (VSS) and macOS, because rustic has no VSS and has the open Windows issues above;
  - on macOS: an APFS snapshot, then either a bind mount at the original paths or rustic with `--as-path`;
  - signed .app + LaunchDaemon + Full Disk Access profile.

### 3.5 Bare metal: manifest, recovery environment, restore

- **Layout manifest** (`device-spec.json`, with field names close to ReaR's records), written by the agent at every backup; in pull mode it is refreshed before the NAS takes the snapshot. It records:
  - disks by model, serial, size and sector size;
  - `sfdisk --dump`, PARTUUIDs, types;
  - filesystems: type, label, UUID, creation options;
  - btrfs subvolumes, the default subvolume, and which subvolumes are excluded or recreated empty (`@log`, `@pkg`, Docker, `/var/lib/machines`, Timeshift, `.snapshots`);
  - md/LVM;
  - **LUKS: cipher, pbkdf, label, UUID and token types, never the header**;
  - the ESP (inside the image, or as a tar for btrbk/files);
  - bootloader, cmdline, initramfs generator;
  - on Windows: BitLocker state.
- **`rynas-recovery`:**
  - a small Debian live image, started by **UEFI HTTP boot from the NAS** (Debian's signed shim → GRUB → kernel, so Secure Boot stays on), by iPXE through Microsoft's signed shim, or from a USB image downloaded from the UI with the NAS address and a **one-time token** (never credentials in clear);
  - it runs the agent's restore mode.
- **Restore:**
  1. NAS → device → version (date per subvolume for btrbk) → **mapping of the target disks by model + serial**, with the "disk in use" guard;
  2. like ReaR, a **restore script generated in advance, readable and editable**: that is the dry run, fstab and loader diff included;
  3. partitions (the last one grows on a larger disk);
  4. **original UUIDs when no attached disk has them**, otherwise new UUIDs and a rewrite of fstab, crypttab, loader entries and cmdline;
  5. data: image chunks streamed to the disk / `btrfs receive` + writable snapshots / `restic restore`;
  6. subvolumes recreated empty, ESP, bootloader + initramfs in chroot;
  7. LUKS recreated with a passphrase typed at restore time; TPM2/FIDO2 to enrol again.
- **Restore of Windows** (phase 3a):
  - GPT and NTFS blocks, with the same GUIDs so the BCD keeps working;
  - `ntfsresize` on a different disk size;
  - on different hardware: storahci/stornvme enabled in the offline registry with hivex, as virt-v2v does **[U]**;
  - BitLocker volumes come back **unencrypted**, and the user turns BitLocker on again (stated in the UI);
  - no WinPE shipped.
- Modes: whole device, system only, single volume or subvolume (additive), files only.
- **Restore test** (phase 3, virtualization plugin): restore into a VM on the NAS, boot it, take a screenshot, monthly. The console shows the date of the last test.

### 3.6 File browsing

- **btrbk:** snapshots are folders, so they open directly in the file manager.
- **files:** the restic browser of D-BACKUP.
- **image:**
  - a version is exposed as a read-only block device (NBD or FUSE over the segments);
  - filesystems that FUSE can read (ntfs-3g, fuse2fs) are opened that way;
  - **btrfs and the rest go through a micro-VM** (virtualization plugin), as PBS does: never mount untrusted images in the NAS kernel;
  - without that plugin, browsing goes through the device's agent.
- Everywhere: download a file or folder as tar/zip, restore into a share, restore to the device through its agent.

### 3.7 Console

- One card per device:
  - engine, last good backup, next run;
  - state (ok, late after 26 h, critical after 72 h, thresholds per device);
  - size, quota, speed, last error.
- btrbk details: freshness per subvolume, interrupted receives, next incremental parent, deletion that cannot break the chain (all from btrbk-nas).
- **"Bare metal ready"** check: recent manifest, ESP saved, bootloader known, recovery key stored, last restore test.
- Templates as in ABB: schedule, GFS retention, excludes. Notifications only when the state changes.

## 4. Marco's machines

All three run Arch on btrfs:

| Device | Today | With rynas |
|---|---|---|
| pc-casa | btrbk + ABB image | image (replaces ABB) and/or btrbk |
| pc-work | ABB image on vdsm, Timeshift | image |
| NB-Marco (Surface, ecryptfs home) | ABB image, only on the home LAN | image with `only_if` at home |

## 5. Phases

| Phase | Content |
|---|---|
| 2 | receiving btrbk backups (**done 2026-11-08, §10**) + console (btrbk-nas port); repositories per device served by rynas, append-only, maintenance on the NAS; manual pairing |
| 2b | `rynas-agent` for Linux (pairing, plans, image engine from abb-agent, btrbk pull, manifest); `rynas-recovery` and bare metal for Linux; restore tested in a VM |
| 3a | Windows: VSS, used-block bitmap, hashing then `$MFT` comparison; restore on the same hardware, then on different hardware |
| 3b | macOS: files with restic |
| 3 | scheduled restore tests in a VM, micro-VM browsing, XFS / LVM thin |

Until phase 3a, Windows images are documented with UrBackup as a curated app.

## 6. Main risks

- Windows restore on different hardware (BCD, storage drivers).
- BitLocker.
- Correctness of an NTFS parser written by us.
- Whether `rustic_core` exposes the low-level write API.
- Prune memory on repositories of several TB (the 25 GB RSS seen on 5.7 TiB): one repository per device, `MemoryMax`, repacking in stages.
- The Secure Boot chain for netboot.
- Mounting untrusted images in the kernel.
- The test burden: VM restore tests are mandatory before each release.

## 7. Mistakes to avoid

1. Recognising disks by kernel name.
2. A layout hook that drifts from the tool.
3. Credentials in clear on recovery media.
4. Deleting the last common parent.
5. Prune from two places, or with writes still open.
6. Backups at boot racing with boot-time writes.
7. A local configuration the package manager can overwrite.
8. `btrfs property set ro false` on a restored snapshot.
9. Saving the LUKS header in the backup.
10. Never testing the restore on real hardware.

## 8. Marco's decisions

1. **Licence** of the abb-agent crates in rynas: relicensed to AGPL-3.0-or-later like rynas, or kept as a separate GPL component. He is the only author.
2. **NAS key**: the NAS holds a key and can browse and run maintenance (recommended), or zero-knowledge, where the NAS sees nothing and maintenance and browsing go through the device.
3. Confirm: no Windows CBT driver.

Chosen by default, open to change:
- restic format with 1 MiB chunks;
- one repository per device;
- image engine as the default proposal on Linux and Windows.

## 9. Note for the next round (2026-11-07)

Marco: "il motore restic vince su tutti in velocità e deduplica". For the next round:
- **Deduplication:** agreed. Content-defined chunks, dedup across snapshots and across devices in a shared repository, zstd. This is why the image engine already stores into a restic-format repository: dedup is restic's in every case.
- **Speed:** it depends on what is measured.
  - Incremental backup of a large tree: restic still has to walk and `stat` every file, using the parent snapshot to skip unchanged files. `btrfs send` and changed-block tracking find the changes from the filesystem metadata without walking anything, so they win on millions of files.
  - Bare-metal restore: streaming an image is faster than recreating millions of files.
  - Prune memory on large repositories: the 25 GB RSS seen with rustic needs to be measured against restic.
- **Option to evaluate:** restic as the single storage and dedup layer for everything (files and images), with btrbk only where the user wants readable btrfs snapshots on the NAS. To measure on pc-casa / pc-work data before deciding.

## 10. Implemented: receiving btrbk backups (phase 2, 2026-11-08)

The first slice of phase 2: Linux PCs push btrbk snapshots to the NAS (`target ssh://nas/<path>`). UI: **Backup › PCs** ("Backup targets for PCs"); the Backup page has one tab per kind of backup, so the files engine can add its own.

### 10.1 What was checked first (2026-11-08)

- **btrbk 0.32.7 `ssh_filter_btrbk.sh`**:
  - what `--target` allows: `btrfs receive`, `mkdir`;
  - `--delete`: `btrfs subvolume delete`;
  - `--info`: `find-new`, `filesystem usage`;
  - always allowed: `btrfs subvolume show|list` (any path), `readlink`, `test -d`, `cat /proc/self/mountinfo|mounts`;
  - `--sudo` puts `sudo -n` in front of everything but `cat`;
  - `|` only for the decompressors (`zstd -d -c` …) and `mbuffer -v 1 -q …`.
- **The commands btrbk sends to a target**:
  - `sudo -n readlink -v -e '/p'`;
  - `cat /proc/self/mountinfo`;
  - `sudo -n btrfs subvolume show '/p'`;
  - `sudo -n btrfs subvolume list -a -c -u -q -R '/mnt'` (on the mount point, outside the target folder);
  - `[zstd -d -c |][mbuffer … |] sudo -n btrfs receive [--max-errors=0] '/p/'`;
  - `sudo -n btrfs subvolume delete [--commit-each] '/p/name.TS'`.
- **CVE-2026-62943**: 0.29.0–0.32.6 had no end anchor, so `<valid command> | anything` passed. Debian bookworm (0.32.5) and trixie/sid (0.32.6-2) are listed as vulnerable and unfixed.
- **Upstream setup**: root login with `PermitRootLogin prohibit-password` plus the filter is "the most straightforward". The alternative is a dedicated user with `backend_remote btrfs-progs-sudo` and NOPASSWD for btrfs.
- **`btrfs receive` and untrusted streams** (btrfs-receive(8), BUGS):
  - a crafted incremental stream can clone extents of any file **on the same filesystem**;
  - anyone with write access to the receive folder can change files while a receive runs;
  - `-C/--chroot` confines the receive process to the folder.
- **Enrollment UX**:
  - UrBackup "Add client" gives one ready-made command with the client's key built in;
  - Synology ABB shows the server's fingerprint on the client;
  - TrueNAS has a "semi-automatic" SSH connection that swaps keys for you, and a manual mode for other systems.

  Taken here: one panel per PC with every command filled in (key, `btrbk.conf`, first connection, dry run) and the NAS-side line shown.

### 10.2 Design

- **Account.** One locked system account, `rynas-pcbackup`:
  - shell `/bin/sh`, because sshd runs the forced command through it;
  - home `/var/lib/rynas-pcbackup`, owned by root with `.ssh/authorized_keys` 0644 root, so the account cannot change its own keys;
  - one line per PC, with the comment `rynas:<device>`.

  sshd_config is never edited. The setup card warns when `AllowUsers`/`AllowGroups` leave the account out, or when sshd, sudo, btrfs-progs or the filter is missing. It reads the port from `sshd -T` and puts it in the PC's URL.
- **Key line**: `restrict,command="/usr/libexec/rynas/rynas-btrbk-filter --device D --root /vol/backup/D [--delete [--keep-last N] [--min-age-days M]]" <key>`.
  - Each PC has its own key, so it can only touch its own folder.
  - The pasted comment is dropped, and options or a second line are refused.
  - The key type must match its blob. RSA keys need at least 2048 bits.
- **Our own filter in Rust**, chosen over shipping `ssh_filter_btrbk.sh`:
  - a tokenizer that takes only plain words, `'quoted'` words and `|`;
  - each stage is matched word by word and run **without a shell**, with absolute paths and a clean environment;
  - `btrfs receive` always gets `--chroot` and is accepted **only into the device folder itself** (btrbk always receives into the target folder). A destination below it is refused: `btrfs receive` resolves the path (`realpath()`) before it chroots, so `<root>/home.X/link`, where `link` is a symlink inside an earlier received snapshot, would write a root-owned subvolume anywhere on the volume (security review 2026-11-08);
  - at run time, before any btrfs command, the binary walks the folder from `/` with `O_DIRECTORY|O_NOFOLLOW` (a symlink anywhere on the way is refused) and refuses a folder on or inside a received or read-only subvolume (`verify_root`);
  - one receive at a time per device (`flock` on the folder); mbuffer's `-m` is clamped to 1 GiB or 25 %, `-s` to 16 MiB;
  - paths are normalized (no `..`, no empty parts). Receive, `readlink`, `test`, `subvolume show|list` and `filesystem usage` stay inside the device folder;
  - a delete always runs as `btrfs subvolume delete --commit-each` (the one form sudoers allows);
  - a non-UTF-8 `SSH_ORIGINAL_COMMAND` is refused; logged commands have control characters escaped;
  - deletes only for btrbk snapshot names directly in the folder;
  - every ACCEPT/REJECT goes to syslog (`auth`);
  - logging in without a command prints "the key of D works". That is the connection test of the setup steps.

  Tests cover the btrbk command shapes and the injection cases: `|`, `;`, `&&`, `$()`, backquotes, `>`, double quotes, newline, `..`, sibling folders, `-o` options, `sudo` without `-n`.
- **Root or non-root.** The filter runs as `rynas-pcbackup` and calls `sudo -n /usr/bin/btrfs`. `/etc/sudoers.d/rynas-pcbackup` (checked with `visudo -c` before the rename; `!requiretty, !use_pty, env_reset`) has **one line per device folder with fixed leading arguments**, exactly the forms the filter produces: `btrfs receive [--max-errors=0] --chroot <root>/`, `btrfs subvolume delete --commit-each <root>/*`, `btrfs subvolume show <root>[/*]`, `btrfs subvolume list [*] <root>[/*]`, `btrfs filesystem usage [*] <root>`. It is rewritten on every create and remove; with no device left the account, its home (keys) and the sudoers file are removed, and a removed device's folder goes back to `root:root` (no stale gid a later group could inherit). `postrm` removes the sudoers file on remove and the account on purge. `readlink`, `test` and `cat` run unprivileged.
  - Why not root: a hole in the filter then gives these btrfs forms on the PCs' folders, not a root shell, and root logins stay as the administrator set them.
  - Accepted limit: a sudoers `*` still matches `..` or a second path, so the filter's normalization and snapshot-name checks remain required; the per-folder lines only keep a filter bug from being root-equivalent. A root-owned wrapper as the only sudo entry was considered: it would need the same parsing again, as root, for no narrower result.
- **Folders.**
  - **New**: `btrfs subvolume create <path>`, `0750 root:rynas-pcbackup`. NAS users cannot read the PC's files, and the account can list names for the delete rules.
  - **Adopt**: an existing folder of received snapshots (for example copied from another NAS with `btrfs send/receive`, which keeps `received_uuid`). Only the folder's own owner and mode change; nothing inside is touched, so btrbk on the PC finds its parents and goes on incrementally.
  - Adopting is refused for a shared folder (or one holding shares), a user's home (or one holding homes) and system folders (`/etc`, `/usr`, `/var`, `/home`, `/opt`, `/tmp`, …). When the folder is not already `root:rynas-pcbackup 0750`, the plan returns `permission_change` (current uid/gid/mode and names, the new ones) and the create call needs `?confirm_permission_change=true`. The change is made on a descriptor (`fchown`/`fchmod`, no symlink followed).
  - "Check the folder" first lists subvolumes, groups, interrupted receives and other entries, and suggests a device name.
  - "New" refuses a folder that exists. Folders of two devices cannot overlap.
- **Status**, from ioctls on descriptors (`received_uuid`, `readonly`, `otime`), grouped by btrbk name:
  - snapshot count;
  - last receive (the NAS-side creation time of the newest complete snapshot);
  - interrupted receives: writable without a received UUID, as btrbk's "garbled";
  - folders with a snapshot name that are not subvolumes;
  - state: ok, late, waiting, or missing.
- **Space** on demand (task, `btrfs filesystem du -s --raw`): exclusive, total and shared bytes, kept with the record. Qgroups are not used, as everywhere in rynas.
- **Late alert**: per PC, `alert_days` (default 3, 0 = off). An hourly check opens `pcbackup.late` (firing, warning) once and closes it with `pcbackup.resumed` when a new snapshot arrives.
- **Retention.**
  - btrbk's `target_preserve` on the PC deletes through the filter (`--delete`, on by default).
  - The NAS adds safety rules, enforced in the filter on a fresh listing:
    - **keep the newest N** complete snapshots of each subvolume (default 1, so the parent of the next incremental always stays), newest by the NAS-side creation time (`otime`), not by the name the PC chose; snapshots named more than a day in the future never count among the N, so junk named `home.20991231T0000` cannot push the real ones out;
    - **never delete before M days**, counted from the NAS-side `otime` (the name only when it cannot be read);
    - or **no deletes at all**.
  - Interrupted receives can always be deleted (btrbk deletes them before sending again).
  - A refused delete is an error in btrbk's log on the PC, so the rules should sit below the PC's `target_preserve_min`.
  - This is basic ransomware protection: a stolen PC key cannot delete what the rules keep. It can still add snapshots and fill the volume (one receive at a time, but no size limit), so a dedicated volume, or a btrfs qgroup limit on the device folder when qgroups are on (`btrfs qgroup limit <size> <root>`, set by the administrator; rynas does not enable qgroups), is the answer to that.
- **Browse and copy out**:
  - read-only listing inside a received snapshot, as root (descriptor walk, symlinks shown never followed);
  - **Copy to the NAS** copies a file or folder into an existing NAS folder: reflink when possible, never overwrites (the snapshot restore code, `restore_foreign`). The PC's uids mean nothing on the NAS (and a setuid-root file or a file capability would mean too much), so the copy belongs to the destination folder's owner and group, loses the set-uid/set-gid bits and the `security.*`, `trusted.*` and POSIX ACL attributes; mode bits, `user.*` attributes and times are kept;
  - the file manager is not used because the folders are `0750` and the worker runs as the signed-in user.
- **Remove**: the key line goes; the folder and its snapshots stay (the dialog says so). Adopting the folder again later picks the chain back up.
- **Dry system and tests**: `MemoryReceive` (memory folders, keys, account) with `/volume1/backup/old-laptop` to adopt. No test touches a disk, sshd or the system's accounts.

### 10.3 Trade-offs and limits

- **Same-filesystem clones** (btrfs-receive BUGS): a malicious stream from a PC could reference extents of other files on the volume. The filter never lets a PC read anything back (no `send`, no `cat` of files), which limits this to the snapshot's own content seen by NAS administrators. A dedicated volume for PC backups removes it.
- The account is shared, so a hole in the filter would reach every device folder through sudo (but only those folders, with the fixed forms above). Per-device accounts would make the sudoers lines per account; the folder check would stay the same.
- **Real-btrbk test** (manual until automated): in a throwaway privileged container (`container=docker`, getty masked) with a loop-mounted btrfs image, install the .deb, add a device through the API, and from a second container with btrbk 0.32 push `btrbk run` to `ssh://nas/<root>`; check the ACCEPT lines in the auth log, then try by hand `ssh rynas-pcbackup@nas "btrfs receive '<root>/home.X/link'"` (REJECT), a future-named snapshot (`btrfs subvolume snapshot -r` named 2099, delete of a real one refused) and `sudo -l -U rynas-pcbackup` (only the per-folder lines). Outline: `packaging/test-pcbackup.sh`.
- Pull mode (btrbk on the NAS, `--source` on the PC through `rynas-agent`) and the console of §3.7 (next incremental parent, chain-safe NAS-side deletion with the PC's UUID list as in btrbk-nas) are the next steps.
- Space per device through qgroups is not done: `du` on demand only.

Sources (2026-11-08): https://github.com/digint/btrbk/blob/v0.32.7/ssh_filter_btrbk.sh · https://github.com/digint/btrbk/blob/master/doc/ssh_filter_btrbk.1.asciidoc · https://github.com/digint/btrbk#setting-up-ssh · https://github.com/digint/btrbk/blob/master/doc/btrbk.conf.5.asciidoc · https://security-tracker.debian.org/tracker/CVE-2026-62943 · https://btrfs.readthedocs.io/en/latest/btrfs-receive.html · https://forums.urbackup.org/t/ho-do-you-add-clients/12848/6 · https://kb.synology.com/en-ca/DSM/tutorial/How_back_up_PC_physical_server_with_ABB · https://truenas.com/docs/core/coretutorials/systemconfiguration/configuringsshconnections/

## Sources

- Marco's notes: Disaster Recovery PC-Casa (github.com/rylos/restore_btrfs_system), Disaster Recovery pc-work, abb-agent (github.com/rylos/abb-agent; local `~/dev/syno-abb`), btrbk-nas (github.com/rylos/btrbk-nas)
- https://pbs.proxmox.com/docs/technical-overview.html · https://pbs.proxmox.com/docs/file-formats.html · https://pbs.proxmox.com/docs/backup-protocol.html · https://forum.proxmox.com/threads/proxmox-backup-client.164363/
- https://urbackup.org/ · https://blog.urbackup.org/189/using-btrfs-on-linux-to-store-raw-image-files-in-an-efficient-manner · https://www.urbackup.org/CbtUserManual.pdf · https://github.com/datto/dattobd/releases
- https://github.com/rear/rear/blob/master/doc/user-guide/06-layout-configuration.md · https://relax-and-recover.org/rear-user-guide/scenarios/restic.html
- https://github.com/rustic-rs/rustic/releases · https://github.com/rustic-rs/rustic/issues/1412 · https://github.com/restic/restic/blob/master/doc/040_backup.rst
- https://security-tracker.debian.org/tracker/CVE-2026-62943 · https://github.com/digint/btrbk/blob/v0.32.7/ssh_filter_btrbk.sh · https://github.com/digint/btrbk#restoring-backups
- https://garethgeorge.github.io/backrest/docs/multihost · https://kopia.io/docs/repository-server/
- https://lists.ipxe.org/pipermail/ipxe-devel/2025-November/007651.html · https://ipxe.org/secboot
- https://learn.microsoft.com/windows-hardware/drivers/dashboard/code-signing-attestation
- https://arqbackup.com/documentation/arq7/English.lproj/permissionRequirements.html · https://forum.restic.net/t/utilizing-macos-local-apfs-snapshots-for-restic-backups/4232 · https://talk.tidbits.com/t/carbon-copy-cloner-restore-limitations/26822
