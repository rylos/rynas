# PC backup in rynas: a restic repository per PC, our own agent, bare-metal restore

**In short:** rynas keeps **one restic-format repository per PC**, served append-only by rynas itself. Only the NAS runs retention, prune and checks. A Rust **`rynas-agent`** built on `rustic_core` backs up the PC's files from a consistent point in time. With every version it also stores a **layout manifest**: disks, partitions, filesystems, boot loader and kernel command line. The same agent binary does the **bare-metal restore from any live Linux**, through a TUI, with no recovery ISO. Single files are restored from the rynas web UI.

Marco's request, 2026-11-11: "voglio una soluzione completa che permetta anche il bare metal restore. le idee le puoi prendere da synology active backup for business. vorrei questo: rynas gestisce un archivio restic/rustic, il pc invia tutto il necessario (come l'agent synology o rustic/restic bastano?)". He added these constraints the same day:
- the agent works on ext2/3/4 too, as abb-agent does;
- no recovery USB/ISO: the agent binary (AUR, GitHub releases) is enough, started from any live Linux;
- a TUI in the agent that, in the first version, does **only bare-metal restore** (PC → version → target disk → everything);
- single files come back from the web UI.

This **replaces the design of 2026-11-07** (D-PCBACKUP: three engines image/btrbk/files, `rynas-recovery` ISO, netboot). What already ships stays as it is: btrbk receiving (§10, and §7 for its future). Web research was done on 2026-11-07 and 2026-11-11, and the VM tests on 2026-11-11 (§3). **[U]** marks a claim that could not be verified.

> **Marco, 2026-11-11, after review:** (1) repositories are per **backup group**: one group per PC by default (isolation), several PCs may share a group to deduplicate across them (they can then read each other's data; prune memory grows with the group's size). (2) **btrbk is an equal alternative**, not a secondary engine: a PC chooses native btrfs snapshots (btrbk, recommended when the root is btrfs) or file backup (restic); both are first-class in the UI and in the restore TUI. Where the text below says "per PC" or "maintenance only", these two points win.

---

## 1. Are restic or rustic enough on their own? No

They store files well: dedup, encryption, versions, checks. A bare-metal restore needs six more things:

1. **A consistent point in time.** On btrfs and LVM a snapshot gives one. On **ext4 without LVM there is no snapshot**, and a plain walk of a live system corrupted a SQLite database in the test (§3.1).
2. **The disk layout.** GPT with its GUIDs, partition types, md/LUKS/LVM, filesystem UUIDs, labels and options, btrfs subvolumes and the default subvolume.
3. **What is not a file.** The BIOS boot partition; an ESP that cannot be frozen (vfat refuses FIFREEZE, §3.1).
4. **Boot repair after the restore.** Initramfs, boot loader, the NVRAM entry or the removable path, and the **kernel command line**. The test lost it: `kernel-install` in a chroot reads the *live system's* `/proc/cmdline` (§3.3).
5. **Pairing, schedule, progress and "late" alerts.** Every PC needs these, and they are what makes it a product (ABB) rather than a cron job.
6. **Ransomware resistance.** restic clients may delete their own snapshots by default. Kopia server and Backrest multihost have the same weakness. Here the server must refuse that, per PC.

So: **restic/rustic as the storage engine, plus our own agent around it.** ReaR does this for Linux, but its restic support is "BACKUP=EXTERNAL, unsupported". ABB and Veeam do it with block images and kernel drivers.

## 2. Comparison (checked 2026-11-07/11)

| Product | What is stored | Bare metal | Agent model | Dedup / encryption | Licence |
|---|---|---|---|---|---|
| **Synology ABB** (PC / physical server) | disk images; incrementals via a snapshot/CBT driver on Linux, VSS on Windows | Windows: recovery media built **on the PC** from the local ADK/WinPE or `Winre.wim`. Linux: a prebuilt ISO | agent paired with the NAS; schedule, retention and templates in the console; file-restore portal; trial boot in VMM with a video | global dedup on btrfs; TLS in transit | proprietary, free with Synology |
| **Veeam Agent** (Linux/Windows) | volume or file images; blksnap module on Linux (not in mainline, last series v7 in 2024-02) | Linux: ISO built on the machine (unsigned) or Veeam's ISO. Windows: media **based on the local WinRE**, drivers added | standalone or managed by VBR | dedup, AES | proprietary (free edition) |
| **UrBackup** 2.5.38 | Windows NTFS images (VHD/raw), files for Windows/Linux/FreeBSD; Linux images through dattobd | restore CD, Debian trixie since 2.6.0 | client pulls from the server | file dedup, hardlinks/reflinks | AGPL-3.0+ (Windows CBT driver paid) |
| **Proxmox Backup Client** 4.2 | `.pxar` file archives, `.img` block archives | none documented for hosts | CLI client to PBS | SHA-256 chunks, AES-GCM | AGPL-3.0 |
| **ReaR** 2.9 | layout (`disklayout.conf`) + any file backup | builds its own rescue ISO; `diskrestore.sh` can be read; migration mode when disks differ | runs on the host | depends on the backup method (BORG native, restic only EXTERNAL) | GPL-3.0 |
| **Clonezilla** 3.3 | offline partclone images | its own live system | none (boot it) | none / optional | GPL |
| **Borg / Vorta** 1.4 | files | none | SSH client; **append-only per SSH key** (`borg serve --append-only`, 2.0: `--permissions`) | CDC dedup, AES/chacha | BSD-3 / Vorta GPL-3 |
| **restic** 0.19.1 | files; Windows VSS with `--use-fs-snapshot` since 0.11 | none | CLI only, **not a library** (Go `internal/`) | CDC dedup, AES-256 + Poly1305, zstd (repo v2) | BSD-2 |
| **rustic** 0.11.4 / **`rustic_core` 0.13** | files, restic format | none | CLI; `rustic_core` is a documented library | as restic; **lock-free**, never honours restic locks; no VSS (#1412 open) | Apache-2.0 OR MIT |
| **rest-server** 0.14.0 | serves a restic REST v2 repository | n/a | HTTP + htpasswd | n/a. `--append-only` and `--max-size` apply to the **whole server**; `--private-repos` = first path part is the user | BSD-2 |
| **abb-agent** 0.2.17 (Marco) | ABB disk images; btrfs and ext2/3/4, no kernel module | `abb-restore`: a static binary + TUI, **from any live Linux** | ABB agent | ABB's | GPL-3.0-or-later |

What we take: ABB's console, pairing, NAS-owned retention, portal file restore and "verified" state. ReaR's manifest and readable restore plan. Borg's per-client append-only. abb-agent's live-system restore (no ISO), TUI, freeze handling and GPT code. What we leave: kernel drivers, WinPE, our own image formats.

## 3. Tests in VMs (2026-11-11)

Throwaway qemu/KVM VMs on a developer PC: UEFI (OVMF), user-mode network, everything in the session scratchpad, deleted afterwards. **Source:** Debian 13 genericcloud, one 4 GiB disk (GPT: p1 ext4 `/` with no LVM, p14 BIOS boot, p15 ESP vfat), restic 0.18.0. The repository sat on a second virtual disk. **Live system:** a Debian 12 genericcloud VM (restic 0.14.0), chosen for its different UUIDs. **Target:** an empty **3 GiB** disk, smaller than the source. abb-agent 0.2.17 came from its release tarball.

```sh
qemu-img create -f qcow2 -F qcow2 -b debian-13-genericcloud-amd64.qcow2 src.qcow2 4G
qemu-system-x86_64 -enable-kvm -cpu host -m 2048 -smp 2 \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/x64/OVMF_CODE.4m.fd \
  -drive if=pflash,format=raw,file=src.vars \
  -drive file=src.qcow2,if=none,id=d0 -device virtio-blk-pci,drive=d0,serial=src-disk0 \
  -nic user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:2221-:22 \
  -smbios "type=1,serial=ds=nocloud-net;s=http://10.0.2.2:8765/src/" -display none -daemonize
```

(cloud-init seeds come from `python3 -m http.server`. Debian 12's cloud-init needs `ds=nocloud-net`, not `ds=nocloud`.)

### 3.1 Consistency of ext4 `/` without LVM

The writer loop updated two files `a`/`b` with the same counter, a 200 kB blob, and a SQLite database in WAL mode.

| Method | Result |
|---|---|
| `restic backup --one-file-system / /boot/efi`, live (1.9 s, 896 MiB) | **SQLite "database disk image is malformed"**, `a=8740 b=8739`, blob truncated to 81920 bytes |
| `fsfreeze -f /` for the whole run; `TMPDIR=/dev/shm restic backup --no-cache …` (repo on another filesystem) | **frozen 9.3 s** for 869 MiB; SQLite `integrity_check` ok, `a=b`, blob whole |
| abb-agent's way: `abb-agent bench vda` with the writer running | `/: ext, 1.1 GiB used, frozen 0.12s to read 29.5 MiB of metadata, files read while in use` |
| `fsfreeze -f /boot/efi` | `Operation not supported` (vfat) |
| `e2image` while `/` was frozen | **hung until thawed** (193 s): a frozen root allows no tool that writes to it, which matches abb-agent's own rules |

Conclusions:
- abb-agent's brief freeze makes the **filesystem structure** consistent. File data is still read live, so a file written during the backup can be mixed, as abb-agent's README says.
- In a file backup the structure is rebuilt by `mkfs` at restore time anyway. What matters is **per-file consistency**, and only a snapshot or a freeze held for the whole read gives that.

### 3.2 Layout capture and recreation on a smaller disk

On the source the test captured `sfdisk --json`, `lsblk -J -b -O`, `blkid -o export`, `findmnt -J --real`, `dumpe2fs -h`, `/proc/cmdline` and a `dd` of the BIOS boot partition, all in the snapshot. On the live system:

```sh
# GPT with the same label-id, partition numbers, types and PARTUUIDs; the last partition takes what is left
label: gpt
label-id: 1E2B8F6D-3643-4D8D-AAC5-AB7EDEBECACC
first-lba: 2048
/dev/vdc14 : start=2048, size=6144, type=21686148-…, uuid=2BDA7513-…
/dev/vdc15 : start=8192, size=253952, type=C12A7328-…, uuid=0397CC02-…
/dev/vdc1 : start=262144, type=4F68BCE3-…, uuid=3612A9BF-…
wipefs -a /dev/vdc; sfdisk /dev/vdc < layout.sfdisk
mkfs.ext4 -F -U 2d5cddec-8777-4aef-9b8d-1d7093e60982 /dev/vdc1
mkfs.vfat -i 91BCA784 /dev/vdc15
dd if=vda14.bios-boot.raw of=/dev/vdc14
mount /dev/vdc1 /mnt; mount /dev/vdc15 /mnt/boot/efi
restic --no-lock restore <snapshot> --target /mnt        # 4 s; then mkdir proc sys dev run tmp
```

`fstab` and `root=` use PARTUUIDs, so nothing had to be rewritten. 4 GiB → 3 GiB worked: a file restore can shrink as long as the data fits, which `abb-restore` cannot do. restic 0.14 refused a read-only repository until it was given `--no-lock`. rustic is lock-free, so this is one more reason for the agent to embed its own engine instead of relying on the live system's restic.

### 3.3 Boot loader reinstall and boot

```sh
for d in dev proc sys run; do mount --rbind /$d /mnt/$d; done
chroot /mnt sh -c 'grub-install --target=x86_64-efi --efi-directory=/boot/efi \
    --bootloader-id=debian --force-extra-removable && update-grub && update-initramfs -u'
```

- **GRUB:** the target disk booted alone, with **fresh NVRAM**, through `\EFI\BOOT\BOOTX64.EFI`: `systemctl is-system-running` = running, no failed units, SQLite ok.
- **The rbind of `/sys` exposes the live system's efivarfs.** `grub-install` and `bootctl` therefore wrote NVRAM boot entries. On real hardware this is wanted, because it is the machine being restored. In the test it made the live VM boot the restored disk on its next start.
- **systemd-boot** on an **emptied ESP**: `apt install systemd-boot; bootctl --esp-path=/boot/efi install; kernel-install add <kver> /boot/vmlinuz-<kver>` booted systemd-boot, which then **dropped to the initramfs shell**: `ALERT! PARTUUID=180a0c80-… does not exist`. `kernel-install` had taken the live system's `/proc/cmdline`. After writing the source's command line from the manifest to `/etc/kernel/cmdline` first, the VM booted (`Current Boot Loader: systemd-boot 257.13-1~deb13u1`, running).

Rule taken: **the manifest stores the kernel command line, and the restore writes `/etc/kernel/cmdline` (and checks `GRUB_CMDLINE_LINUX`) before any boot-loader tool runs.**

## 4. Architecture

### 4.1 NAS side

- **Repositories:** one restic repository (v2, zstd) per PC at `<volume>/pcbackup/<device>/`, a btrfs subvolume when the volume is btrfs. The NAS creates it with `rustic_core`; the PC never runs `init`.
- **Served by rynas, not rest-server.** rynas serves the restic REST v2 protocol (config, keys, index, snapshots, data, locks: GET/HEAD/POST/DELETE) inside its own HTTPS API, `/api/v1/pcbackup/repo/<device>/`.
  - rest-server's append-only mode and quota apply to the whole server, it has its own htpasswd, and it knows nothing about prunes. Every per-PC rule would need a proxy in front of it anyway.
  - The protocol is small: a few hundred lines in axum, tested against restic and rustic.
  - rest-server (BSD-2) stays the fallback if this ever proves wrong.
- **Per-device rules in that handler:**
  - a bearer token per device, which reaches only its own path;
  - **append-only**: no DELETE except the device's own lock files, no overwrite, `config` and `keys` read-only;
  - a **quota** per device, counted at POST time;
  - **writes refused with 423 while the NAS prunes that repository** (rustic ignores locks);
  - optional rate limiting.
- **Retention, prune and verification run only on the NAS,** in the existing `rynas-backup-worker` (D-BACKUP):
  - GFS / smart retention per plan, with a forget preview;
  - prune (never `--instant-delete`), then `check` and a rotating `read-data` subset;
  - a per-repository maintenance lock;
  - **a minimum age**: no version younger than N days is ever forgotten.
- **Second layer:** read-only btrfs snapshots of the `pcbackup` subvolume (D-SNAP). The restic copy of the NAS elsewhere (D-BACKUP) can include the PC repositories, with `restic copy` or a copy of the folder.
- **Console** (Backup › PC backup), one card per PC:
  - last good version and next run;
  - state: ok, late after 26 h, critical after 72 h;
  - size and quota, speed, live progress, last error, files changed during the backup;
  - the **"Bare-metal ready"** check: recent manifest, every volume covered, boot loader known, restore code and recovery kit available.

  Templates as in ABB: schedule, retention, excludes. Notifications only when the state changes (`pcbackup.*`, as today).

### 4.2 Pairing, auth, revocation

1. In the UI, **Add PC**: rynas shows a one-time pairing code (15 min) and the NAS certificate's SHA-256 fingerprint.
2. On the PC: `sudo rynas-agent pair https://nas:5443 <code>`. The agent prints the fingerprint and the NAS name and pins the certificate.
3. The NAS creates the repository and a device record, and returns:
   - a random 256-bit **device token** (the NAS keeps only its hash; the PC stores it at `/var/lib/rynas-agent/` 0600 root);
   - the **repository password of a restic key file of its own** (`key add`).
4. **Revocation:** delete the device in the UI. Its token stops working at once and its key file is removed.
   - Limit to state in the UI: restic key files all wrap one master key, so a stolen copy of a PC's key can still decrypt data it can *read*. With the token gone it can read nothing.
   - Rotating the master key means a new repository (`restic copy`).
5. **Restore access** from a live system, where the PC's own token is gone with its disk: a **restore code** made in the UI. It is one-time, valid for 1 hour, **read-only**, and covers one device, or every device after re-authentication. Or an administrator login with TOTP in the TUI. Over pinned TLS the NAS hands the TUI the repository password for that session.
6. A restored PC carries its old token back, so backups go on without pairing again. A *second* machine restored from the same backup (a clone) is noticed by machine-id plus hardware, and the console asks whether to pair it as a new PC.

### 4.3 The agent (`rynas-agent`, Rust)

- **One binary for every role:** the service, `pair`, `backup`, `status [-w]`, and `restore` (the TUI).
  - Engine: `rustic_core` (Apache-2.0/MIT), the same as `rynas-backup-worker`, so both ends share one format and one code base.
  - Linux: a static musl binary (x86_64, aarch64), signed, in the GitHub releases and the AUR (`rynas-agent-bin`, `rynas-agent`), with a systemd unit. A .deb/.rpm via cargo-deb / cargo-generate-rpm costs little and can follow. Windows: §5.
- **Plans come from the NAS:** schedule, sources and excludes. The only local state is the pairing, so no local configuration file can be overwritten by a package manager (a `.pacnew` once broke abb-agent).
- **Scheduler,** taken from abb-agent:
  - after boot with a delay, every N hours, retries with backoff;
  - `only_if` gateway IP + MAC (laptops), `wait_locks`;
  - a `systemd-inhibit` sleep lock while running;
  - "Back up now" and "Cancel" from the console, through a long poll.
- **Consistent point in time, per filesystem:**

  | Filesystem | Method |
  |---|---|
  | btrfs | read-only snapshot of every subvolume under one run folder (abb-agent's `btrfs-image` snapshot code), backed up with `as_path` = the original path, then removed |
  | LVM (thick or thin) | `lvcreate -s` / thin snapshot, mounted read-only; when the VG has no free extents, fall back to the next row |
  | ext2/3/4, XFS, no LVM | **abb-agent's freeze machinery**, in two modes. (a) default: files read live with **change detection**: ctime/size/mtime before and after each file, one re-read, then the file is listed as "changed during backup" in the version and on the card. Pre-backup hooks cover databases. This is abb-agent's own guarantee for file data. (b) opt-in **"freeze up to N s"**: the filesystem stays frozen for the whole run, with cache and temp in `/run`, nothing written to it, and a watchdog that thaws at N s and goes on in mode (a). §3.1 measured 9.3 s for a 0.9 GiB full backup; incrementals read only changed files |
  | vfat ESP | copied as files (small), never frozen |
  | BIOS boot partition, unknown filesystems | raw copy, unmounted only, stored as a file in the version |

  Freeze safety comes from abb-agent: thaw on every signal and on exit, a list of frozen mounts in `/run` thawed by the next start, and no logging to a frozen filesystem.
- **Layout manifest** `/.rynas/manifest.json`, stored in every version:
  - disks by model, serial, `by-id`, size and sector size; the GPT (label-id, every entry with type, PARTUUID, name and attributes);
  - md (`mdadm --detail --export`), LVM (`vgs/lvs --reportformat json`);
  - **LUKS parameters only** (cipher, pbkdf, UUID, label, token types), **never the header**;
  - filesystems: type, UUID, label, features (from abb-agent's superblock readers), btrfs subvolumes, the default subvolume and the subvolumes excluded or recreated empty;
  - fstab, crypttab, `/proc/cmdline`, `/etc/kernel/cmdline`, boot mode, loader (GRUB, systemd-boot, shim, UKI, rEFInd), initramfs generator, os-release.
- **Progress:** every 2 s to `/api/v1/pcbackup/agent/status`: phase, bytes, files, speed, ETA. At the end, a summary with the counts and the changed-during-backup list.
- **Privileges:** root (block devices, FIFREEZE, snapshots), with a systemd sandbox where possible (`ProtectHome=read-only` is impossible for a backup agent, but `ProtectKernelModules`, `NoNewPrivileges` and the rest are fine).

### 4.4 Bare-metal restore: the agent from any live Linux

There is no ISO. The user boots any live system in the same firmware mode (Arch ISO, SystemRescue, Debian/Ubuntu/Fedora live), with Secure Boot left on: the agent is a userspace binary. The PC page shows two commands:

```sh
curl -fLO https://nas:5443/api/v1/pcbackup/agent/rynas-agent-x86_64   # or from the GitHub release
sha256sum rynas-agent-x86_64     # compare with the value on the page
sudo ./rynas-agent restore
```

**TUI, version 1: bare metal only** (Marco, 2026-11-11). It is built on `abb-restore`'s ratatui flow:
1. Network and NAS address; the fingerprint is shown and confirmed.
2. Restore code, or administrator + TOTP.
3. PC, then version (date, size, "bare-metal ready").
4. Target disk(s), by model, serial and size, mapped from the manifest:
   - the "disk in use" guard;
   - a smaller disk is refused only when the data does not fit;
   - typing the disk name confirms the wipe.
5. Summary with the generated plan (the commands, ReaR-style, readable).
6. Progress, then reboot.

What the agent does:
1. **Preflight:** checks for `mkfs.*`, `cryptsetup`, `lvm` and `mdadm` as the manifest needs them, and names the package to install on the live system when one is missing. GPT and the restic engine are inside the agent.
2. **GPT:** the same label-id, numbers, types, PARTUUIDs, names and attributes (abb-restore's `gpt.rs`). The last partition grows or shrinks; fixed partitions keep their size.
3. **md / LUKS / LVM:** LUKS is created with a passphrase typed in the TUI and the original UUID, so crypttab keeps working. TPM2/FIDO2 are enrolled again by the user, and the TUI says so.
4. **Filesystems with the original UUIDs and labels** when no other attached disk has them; otherwise new ones, and fstab, crypttab, the loader entries and the cmdline are rewritten. btrfs subvolumes and the default subvolume are recreated, excluded subvolumes empty.
5. **Data:** a `rustic_core` restore that keeps owners, modes, xattrs (`security.capability`, SELinux), ACLs, hard links and sparse files. NOCOW folders get `chattr +C` before their content. Raw items are written back.
6. **Chroot** (`--rbind` dev/proc/sys/run):
   - write `/etc/kernel/cmdline` from the manifest (§3.3);
   - regenerate the initramfs (`update-initramfs -u -k all` / `mkinitcpio -P` / `dracut --regenerate-all`), which also covers new hardware;
   - reinstall the loader (`grub-install` + `grub-mkconfig`, or `bootctl install` + `kernel-install add` for each kernel; shim when it was there);
   - add the **removable fallback** `\EFI\BOOT\BOOTX64.EFI` (as abb-restore does) on top of the NVRAM entry written through efivarfs;
   - BIOS: `grub-install --target=i386-pc` plus the BIOS boot partition;
   - `/.autorelabel` when SELinux is enforcing.
7. **Report** to the NAS: the restore appears in the PC's history.

Later TUI phases: single files/folders, restore of one volume, "restore into a folder".

### 4.5 File restore from the rynas web UI

Built on the D-BACKUP restic browser, which already exists (browse, restore, download through the worker):
- pick a PC and a version, then browse;
- **download one file**, streamed. Today's 64 MiB `MAX_DOWNLOAD` becomes a stream from the worker;
- **download a folder or a selection as a zip**, streamed and not compressed (store), with zip64;
- **restore into a NAS folder**, with the §10 copy-out rules: owned by the destination's owner, no set-id bits, no `security.*`/ACL xattrs, never overwrite;
- **restore to the PC (phase 2):** the agent writes under a temporary name and renames into place with `O_NOFOLLOW` (abb-agent's `filerestore.rs`), with skip or overwrite, and reports progress.

### 4.6 Security and keys

- **Who holds the keys:**
  - the PC, its own key file;
  - the **NAS, a key in every repository**: retention, prune, check, web browsing and restore codes need it;
  - the administrator, a **recovery kit** shown once per PC: the repository password and path, enough for stock `restic` on any machine (print it or put it in Bitwarden).

  The UI states that whoever owns the NAS can read the backups, as with ABB. Zero-knowledge would move prune and browsing to the PC; it is decision 3.
- **Ransomware:**
  - a stolen or infected PC can add data, not delete or rewrite it (append-only per token);
  - it can fill the quota, which raises an alert;
  - the minimum-age rule and the btrfs snapshots of `pcbackup` protect against a bad retention change;
  - a compromised NAS loses everything, which is why the off-NAS copy (D-BACKUP) matters.
- **Never:** credentials in a file on a USB stick; the LUKS header in the backup; a device token in the URL; a pairing code that is not one-time.

### 4.7 Memory of the NAS-side maintenance (implemented 2026-11-11)

Marco's request: size the maintenance to the machine's RAM.

- **Engine.** Prune, check and forget run in `rynas-backup-worker` (`rustic_core`), never the restic binary. rustic loads only the index a command needs (forget reads only the snapshots). Nowhere in rynas does the restic binary run, so no GOGC/GOMEMLIMIT is needed.
- **Budget.** `maintenance_memory_pct` (10–80 %, default 40) of MemTotal, at least 256 MiB. The worker runs in its transient unit with `MemoryMax` = the budget and `MemoryHigh` = 3/4 of it, plus `Nice=19`, idle I/O and the same sandbox as the other worker units (the dedup units follow the same pattern).
- **Estimate before running.** Counting the blobs with the worker would cost as much memory as the prune itself (measured below), so the NAS reads only the **size of the index files**: about 40 bytes per blob on disk (zstd-compressed JSON; the 32-byte blob ids do not compress). Need = 32 MiB + blobs × 250 bytes. A prune and check that would not fit are **postponed**, and the reason shows on the group (forget still runs). The console shows each group's estimate against the budget. "Join a group" warns when one more PC would push the group past it.
- **New repositories** (`init_pc` in the worker) use larger packs: data packs of 64 MiB that grow with the repository up to 512 MiB (restic: 16 MiB; rustic's default: 32 MiB without a limit). They also use a **2 MiB average chunk** (min 1, max 16) instead of restic's fixed 1 MiB (512 KiB–8 MiB). **Revised 2026-11-12 (Marco):** both sizes are available. The feature's settings offer *Automatic* (the default: 1 MiB from 8 GiB of NAS memory, otherwise 2 MiB), *1 MiB* or *2 MiB*. Each group records the size it was created with (groups made earlier: 2 MiB), and its row shows it. That means about half the data blobs, and so half the index in memory, for files over 1 MiB. Files under 1 MiB are one blob either way. The price is less dedup inside large files that change in small places: a changed region rewrites about 2 MiB instead of 1. Stock restic still reads these repositories (blobs are content-addressed, and it ignores the extra config fields); backups it writes into one would be chunked with its own defaults. **Existing repositories never change chunking**, because that would split their dedup in two.

Measured on a developer PC, 2026-11-11 (`rynas-backup-worker` release build; restic 0.18.1 wrote N one-blob files twice, the second time without 1000 of them; peak RSS from `getrusage`):

| blobs | index on disk | index_info | forget | prune | check (structure) | check + 1/12 data |
|---|---|---|---|---|---|---|
| 300 000 | — | 72 MiB | 40 MiB | 77 MiB | 79 MiB | 97 MiB |
| 1 200 000 | 46 MiB (40.4 B/blob) | 237 MiB | 40 MiB | 247 MiB | 282 MiB | 271 MiB |

The slope is about 198 bytes per blob for prune and 236 for check; the base is about 20 MiB. A 4 GB DS920+ with the default 40 % budget (≈1.6 GB) therefore maintains a group of about 6 million blobs: roughly 6–12 TB of PC data at 1–2 MiB per blob.

## 5. Phases

| Phase | Content | Rough size | Main risks |
|---|---|---|---|
| **1. MVP: Linux files + layout + BMR** | NAS: device registry, pairing, REST v2 handler (append-only, quota, prune gate), console, retention via the existing worker, web browse/zip/copy-out. Agent: pairing, scheduler, btrfs/LVM snapshots, ext/XFS freeze modes, manifest, progress, AUR + release. TUI bare-metal restore with GRUB and systemd-boot, GPT/ext4/btrfs/vfat, then LUKS/LVM/md | **L**: about 12–16k new lines (3–4k reused from abb-agent/abb-restore), plus a VM test matrix (Debian, Arch, Fedora; GRUB, systemd-boot, UKI; ext4, btrfs, LUKS) | ext4 without LVM consistency (§3.1); `rustic_core` restore fidelity (xattrs, ACLs, hard links, sparse) to measure against restic; the chroot fixups per distro; restores on real hardware |
| **2. Windows file backup** | Windows service (windows-service), **VSS made by the agent**, files into the same kind of repository, MSI installer (WiX), signed binaries; web restore unchanged; restore to the PC through the agent | **M**: 5–7k lines + signing and CI | rustic on Windows (no VSS, open restore bugs #635/#1986) → decision 5; code signing and AV false positives; VSS edge cases (timeouts, writers) |
| **3. Windows BMR** | at the same VSS moment, a **used-cluster image of each NTFS volume** (`FSCTL_GET_VOLUME_BITMAP`), stored as a file in the version, plus ESP/MSR/recovery partitions; restored **by the same agent from a Linux live system**: GPT with the same GUIDs (BCD keeps working), clusters written, `ntfsresize` for other sizes | **M–L**: 5–8k lines | different hardware (storage drivers: `storahci`/`stornvme` start type in the offline SYSTEM hive) **[U]**; BitLocker volumes come back decrypted; a cluster image doubles what the PC reads, though dedup limits what it sends **[U]**; no WinPE means no Windows-side repair tools |
| later | TUI single-file restore; scheduled restore test in a VM (ABB's "verification"); fast incrementals from abb-agent's inode-table comparison instead of a stat walk; MBR disks; macOS **out of scope** (signed system volume, APFS snapshots, notarized .app, Full Disk Access) | | |

Why no WinPE: Microsoft gives no redistribution licence for WinPE images. Vendors build the media on the customer's PC from the installed ADK (ABB) or from the local `Winre.wim` (Veeam, ABB manual mode). The Linux live system plus our agent avoids that, and it also keeps one restore code path. If phase 3 shows that Windows needs Windows tools, the fallback is a "create recovery media" button in the Windows agent that builds from the PC's own WinRE, never media we ship.

## 6. Reuse from abb-agent

abb-agent is **GPL-3.0-or-later**, workspace-wide. It has 133 commits: 132 are Marco's, and one external commit (#3, 4 lines in `abb-agent/src/stream.rs`) is in ABB-protocol code that is not reused. Marco can relicense his code to AGPL-3.0-or-later. Even without relicensing, GPL-3.0 §13 allows combining it with rynas' AGPL-3.0-or-later code.

| Piece | Reuse |
|---|---|
| `abb-agent/scheduler.rs`, `control.rs` | the scheduler (boot, interval, retries, `only_if`, `wait_locks`) and clean stops on signals |
| `abb-agent/volume.rs` | `Freeze` (FIFREEZE/FITHAW, thaw on drop, leftovers in `/run`, live-path checks) |
| `abb-agent/disk.rs`, `ui.rs` | disk discovery by stable ids and `system`; the terminal progress bar |
| `abb-agent/filerestore.rs` | restore to the PC (phase 2): temporary name + rename, `O_NOFOLLOW` |
| `btrfs-image` (snapshot/capture parts) | read-only snapshots of every subvolume, cleanup, subvolume enumeration and exclusion rules |
| `ext-image` | superblock and feature parsing for the manifest; later, inode-table comparison for fast incrementals |
| `abb-restore/tui.rs`, `plan.rs`, `gpt.rs`, parts of `restore.rs` | TUI flow, disk mapping and guards, GPT writer (same GUIDs, backup GPT moved, last partition resized), EFI fallback copy |
| packaging | static musl build, signed releases, AUR PKGBUILDs (`-bin` and source) |
| not reused | `abb-proto` (Synology protocol), the block-image streaming and CBT (files are stored, not images) |

## 7. The current btrbk "PC" tab

It **stays as it is**, renamed "PC snapshots (btrbk)". It is implemented, hardened by a security review, and useful for btrfs PCs that want readable snapshots on the NAS. It gets maintenance only. The plans for it in the earlier design are **dropped**:
- pull mode with a filter shipped by the agent;
- the btrbk-nas console port;
- bare metal through btrbk.

**Revised 2026-11-11:** bare metal through btrbk comes back as a source of the same agent TUI, see §11. The new restic-based "PC backup" tab becomes the recommended path, and the wizard says so. A PC may use both. Rewritten D-PCBACKUP text follows Marco's answers below.

## 8. Decisions for Marco

1. **Storage: files (proposed) or images in the restic repository.** Files give web browsing for free, restore onto any disk size (shrink included, §3.2), and an emergency exit with stock restic. abb-agent-style images would give the fastest bare metal and exact filesystems, but browsing would mean parsing untrusted images on the NAS. *Recommendation: files; keep images as a later option only if restore times on real PCs disappoint.*
2. **ext/XFS without LVM:** default "live + change detection" with opt-in "freeze up to N s" (proposed), or freeze by default with a short cap (30 s?). *Recommendation: live by default on desktops, freeze offered in the wizard for servers; suggest btrfs or LVM in the UI.*
3. **NAS key in every repository** (proposed: NAS-side retention, web restore, restore codes) or zero-knowledge (prune and browsing through the PC, no web restore while the PC is off). *Recommendation: NAS key + recovery kit.*
4. **Serve REST v2 from rynas** (proposed) or run rest-server behind a rynas proxy. *Recommendation: rynas; rest-server's global append-only and quota do not fit.*
5. **Windows file engine:** `rustic_core` + our own VSS (one engine everywhere) or a bundled `restic.exe` (BSD-2, mature VSS and Windows metadata) driven by the agent. *Recommendation: decide at the start of phase 2 with a restore-fidelity test; lean towards restic.exe until rustic closes #635/#1986.*
6. **Licence of the reused abb-agent code:** relicense to AGPL-3.0-or-later (simplest, one licence in the repository) or keep those files GPL-3.0-or-later. *Recommendation: relicense; Marco is the author of all reused code.*
7. **Repository per PC (proposed) or shared with cross-PC dedup** (as ABB's global dedup). A shared repository cannot be append-only per PC, has one master key for all, and needs the 25 GB RSS prune seen on 5.7 TiB. *Recommendation: per PC.*
8. **Agent distribution:** AUR + GitHub releases only (Marco's minimum), or also .deb/.rpm and an apt repository later. *Recommendation: AUR + releases in phase 1, .deb/.rpm files in the release (cheap); a repository when rynas has one (D-APPCAT).*
9. **Code signing for Windows** (phase 2): a paid certificate, or Azure Trusted Signing. *Recommendation: decide before phase 2; unsigned installers trigger SmartScreen and AV.*

## 9. Mistakes to avoid

1. Recognising disks by kernel name.
2. Trusting the live system's `/proc/cmdline` in a chroot (§3.3).
3. Writing to a frozen filesystem, logs and caches included (§3.1).
4. Prune with writes open, or from two places.
5. Credentials on removable media.
6. Saving the LUKS header.
7. A local agent configuration the package manager can overwrite.
8. Depending on the live system's restic (0.14 on Debian 12).
9. Never booting a restored machine: the VM test matrix runs before each agent release.

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
- Pull mode and the btrbk-nas console port, once planned as next steps, are dropped (§7, 2026-11-11).
- Space per device through qgroups is not done: `du` on demand only.

Sources (2026-11-08): https://github.com/digint/btrbk/blob/v0.32.7/ssh_filter_btrbk.sh · https://github.com/digint/btrbk/blob/master/doc/ssh_filter_btrbk.1.asciidoc · https://github.com/digint/btrbk#setting-up-ssh · https://github.com/digint/btrbk/blob/master/doc/btrbk.conf.5.asciidoc · https://security-tracker.debian.org/tracker/CVE-2026-62943 · https://btrfs.readthedocs.io/en/latest/btrfs-receive.html · https://forums.urbackup.org/t/ho-do-you-add-clients/12848/6 · https://kb.synology.com/en-ca/DSM/tutorial/How_back_up_PC_physical_server_with_ABB · https://truenas.com/docs/core/coretutorials/systemconfiguration/configuringsshconnections/

## 11. btrbk PCs: stronger receive and bare-metal restore

> **Marco, 2026-11-11, decisions on §11:** all seven recommendations accepted. On retention and transport: for new btrfs PCs **the agent does everything** (read-only snapshots, parent choice, `btrfs send` of the system's btrfs-progs over HTTPS to rynas, a few local snapshots kept as parents); rynas runs the NAS-side retention. Snapshot names stay btrbk-compatible so a PC can switch either way without a new full send. PCs already set up with btrbk over SSH (such as the maintainer's home PC) keep working as today; the agent restores both.

**In short:** Marco's request (2026-11-11) brings **bare metal through btrbk** back. §7 had dropped it. The btrbk receive of §10 stays as it is. The plan adds four things:
- a **layout manifest that travels inside the root snapshot**;
- NAS-side views, retention and readiness checks;
- a **btrbk source in the same `rynas-agent` restore TUI** that phase 1 builds for restic;
- an HTTPS stream from rynas, so nothing has to be kept by hand on a USB stick.

The logic comes from Marco's `restore_btrfs_system` (MIT; https://github.com/rylos/restore_btrfs_system, read 2026-11-11). B1 (the NAS side) is implemented, see §11.8; B2 (the agent-native btrfs method: the agent snapshots and sends over HTTPS, no btrbk needed) in §11.9; B3 (the restore source) in §11.10; B4 (the VM matrix) is not. **[U]** marks claims still to verify in a VM.

### 11.1 What `restore_btrfs_system` does (and what we take)

It is two bash front ends with the same logic (`restore_btrfs_system.sh` CLI, `restore_tui.sh` dialog), plus a pacman hook and a read-only test (`test_restore.sh`). Its job is to restore an Arch btrfs PC from btrbk snapshots on a Synology NAS.

| Area | What it does | Taken? |
|---|---|---|
| **Modes** | `--standard` (@ + @home), `--full` (+ @games), `--system-only` (format, @ only), additive `--home-only` / `--games-only` (no format, @ must already be in the pool), interactive, TUI "Custom" checklist | yes, generalised from the manifest (§11.4) |
| **Snapshot choice** | newest per subvolume by default; `--snapshot-root/home/games TS` checked against `YYYYMMDDTHHMM`; menu lists from the NAS; override flags that a mode ignores raise a warning | yes, plus a "same run" set (the same btrbk timestamp across subvolumes) |
| **Subvolume rules** | @games is mounted inside @home: formatting with @games requires @home; additive @games requires @home in the pool | yes, derived from fstab nesting instead of hard-coded names |
| **Disk identity** | NVMe list with size, model and **serial**; the summary repeats model + serial ("nvmeX changed between two boots on 2026-10-26"); the disk holding the script is marked but still selectable; the script's own disk is mounted by **PARTUUID** | yes; disks from the manifest by model + serial + size |
| **Safety before wiping** | root required; `flock` lock; NAS ping + persistent SSH; **every snapshot checked on the NAS before formatting**; **disk-in-use guard** (mounts or swap, `MOUNTPOINTS` with a `MOUNTPOINT` fallback); **empty partition dump refused** (`test -s`: `sfdisk < empty` returns 0); disk-too-small warning computed from the dump; partition table and `/boot` backup **checked before the wipe**; typed `yes` in every mode (`--yes` = 5 s countdown) | all of it, except the countdown |
| **Partitioning** | `sfdisk` dump kept inside @ by a pacman hook (atomic tmp + `mv`); keeps label-id and PARTUUIDs; **if another attached disk already has those GUIDs**, they are stripped and new ones generated; `udevadm settle` + polling for the device nodes | yes; the GPT is written by the agent (phase 1 `gpt.rs`), from the manifest |
| **Data** | `ssh nas btrfs send` piped (`pv` for progress) into `btrfs receive` into `btrbk_snapshots/`, then `btrfs subvolume snapshot` writable as `@`, `@home`…; existing subvolumes renamed `_backup_<ts>`, never deleted; after the receive, checks that the subvolume exists with a non-zero generation | yes; this is also btrbk's documented restore. The received snapshot stays as the parent of the next incremental (btrbk README) |
| **Recreated empty** | `@log`, `@pkg` | yes, from the manifest's "excluded" list |
| **Boot** | `/boot` (the ESP, kernels included on Arch) from the newest `.bootbackup/*_post` copy inside the snapshot; `arch-chroot bootctl install`, **non-fatal** (BIOS-booted live system, no efivars); the README says "boot the live system in UEFI mode" | partly; regeneration comes first, the ESP copy is the fallback (§11.4 step 7) |
| **fstab/loader fix** | rewrites old→new PARTUUID in fstab and `loader/entries`; comments out non-restored subvolumes and uncomments restored ones (only lines it commented, `^#\s*PARTUUID=`); **sanity check on active lines**; old PARTUUID still in entries = failure | yes, with an explicit marker comment instead of a regex |
| **End** | reboot offered **only** when the PARTUUID fix is verified and the boot files came back; the log is copied to the target's `/var/log`, or to the script's disk when the run fails earlier; **dry-run** runs the real fstab regex on a copy with mock IDs and shows the diff | yes; the log also goes to the NAS |
| **Not taken** | SSH key in `keys/id_rsa` on the stick, `root@NAS`, `StrictHostKeyChecking=no`, CIFS with a password, fixed IP/paths/user, NVMe only, `pacman -Sy` of dependencies, hard-coded subvolume names | §9 mistakes 1 and 5 |

### 11.2 NAS side: what to add to Backup › PCs

1. **Received snapshots per PC.** A table grouped by subvolume. Each row shows the btrbk timestamp, the NAS receive time (`otime`) and its size. Sizes come from the existing `du` task, run per snapshot: exclusive bytes (what deleting it frees) and referenced bytes (what a restore transfers). A **run view** groups by timestamp, so "everything from 2026-11-10 03:00" is one row. Incomplete receives get their own badge and a delete action.
2. **NAS-side retention (optional per PC).** Today btrbk's `target_preserve` on the PC deletes through the filter, and the NAS only adds a floor (keep-last N, min-age). The plan adds a **"NAS owns retention"** mode with GFS presets (D-SNAP's preview code: what would go, and how much it frees). In this mode the PC's key gets no `--delete` and its `target_preserve` is `no`. A NAS task then deletes through `rynas-btrfs`'s chain-safe delete, which always keeps the newest complete snapshot of each subvolume, the parent of the next incremental. That gives ABB-like ransomware resistance: a PC's key can only add. **Recommended as the default for new PCs.**
3. **Health and alerts** (on top of `pcbackup.late`/`resumed`):
   - **missing subvolume**: a group that stops while the others go on, for example `@home` and not `@`;
   - **stuck interrupted receive** older than 1 day;
   - **manifest stale**: older than its root snapshot by more than N days, or one that lists a subvolume the NAS never received;
   - **volume free space** below the size of the last full set;
   - **"Bare-metal ready"**: the manifest is present and parsed, every non-excluded subvolume has a snapshot, the boot loader is known, and a restore code can be made. One state per PC with a reason, the same check as the restic card (§4.1).

   Notifications fire only when the state changes, as today.
4. **Layout manifest from the snapshot.** The PC keeps `/.rynas/manifest.json` inside its root subvolume (§11.3), so each `@` snapshot carries the layout *of that moment*. This is the coherence argument of `restore_btrfs_system`'s hook. The NAS reads it from received snapshots:
   - descriptor walk, `O_NOFOLLOW`, size cap 1 MiB, parsed as untrusted input, cached by snapshot UUID;
   - shown as disks, partitions, subvolumes and boot loader on the PC page. No hand-kept table like the README's.
5. **Setup helper.** The existing panel already prints the key commands, a `btrbk.conf` and a test. Three additions:
   - *(superseded by §11.9: no btrbk on new PCs)* `rynas-agent btrbk-setup --nas <url> --device <d>` (one command, shown filled in) reads the PC's real layout (top-level mount, subvolumes, which are snapshot-worthy, the existing `snapshot_dir`) and writes `/etc/btrbk/btrbk.conf`. It shows a diff when one exists and never overwrites silently. It also creates the key and installs the manifest hook (§11.3). For PCs without the agent, the panel keeps the hand-written snippet, now generated from the subvolume list the user ticks;
   - the timer: the distro's `btrbk.timer`, with a `btrbk.service.d/rynas.conf` drop-in that runs `ExecStartPre=rynas-agent manifest`;
   - a **first-run check** on the NAS, "first snapshot of each subvolume received", which turns the card green.
6. **Restore path.** The PC page gets two parts:
   - "Restore bare metal", with the same two commands as §4.4 (download the agent, `sudo ./rynas-agent restore`) and a **restore code** button (one-time, 1 h, read-only, one device);
   - file restore and copy-out, already in place (§10).

   The NAS serves the data over **HTTPS**:
   - `GET /api/v1/pc-backups/targets/{device}/manifest?snapshot=`;
   - `GET …/send?snapshot=<name>[&parent=<name>]`, a `btrfs send` stream compressed with zstd. It is allowed only for read-only, received, complete snapshots directly in the device folder. The same `verify_root` walk applies, it is audited, and one stream per device runs at a time;
   - authentication: a restore-code session or an administrator with TOTP, as in §4.2 step 5.

   No SSH key, no sshd and no NAS root access are needed on the live system.

### 11.3 PC side without a daemon: the manifest hook

btrbk has no pre/post hooks (README), so the manifest is written by `rynas-agent manifest`. It writes to a temporary file in `/.rynas/` and renames it into place, as the hook does. It runs from three places:
- the btrbk drop-in above, so every snapshot run refreshes it first;
- a package-manager hook for kernel and boot changes: pacman `.hook`, apt `DPkg::Post-Invoke`, dnf `post-transaction-actions`;
- once at setup.

Content: the §4.3 manifest (disks by model, serial and size; GPT with GUIDs; LUKS parameters, never the header; filesystems with UUIDs and labels; subvolumes with mount points, the default subvolume, NOCOW and excluded ones; fstab, crypttab, `/proc/cmdline`, `/etc/kernel/cmdline`, boot mode, loader, initramfs generator, os-release), plus a btrbk block: the `snapshot_dir`, and the btrbk name → subvolume path → mount point map.

**ESP copy:** when the ESP (or a separate non-btrfs `/boot`, as Fedora uses) is not inside a snapshotted subvolume, the hook also copies its files to `/.rynas/boot/<mount>/`. It uses tmp + rename, keeps one copy, and records the size in the manifest.

A **PC without the agent** keeps working as today, with no manifest. The restore TUI then asks for the layout through a guided step (one disk, ESP + btrfs, sizes) and shows a "not bare-metal ready" badge.

### 11.4 Bare-metal restore in the agent TUI (btrbk source)

**Fit with phase 1.** The restic TUI already does network, NAS, fingerprint and code, then PC → version → target disks → plan → progress → reboot. Its restore source is a trait. The btrbk source implements it. **This part is shared:** GPT, LUKS/LVM, mkfs with the original IDs, the chroot and boot fixes, and the report. **Only these differ:**
- `list_versions`: btrbk runs, grouped by timestamp, with a per-subvolume override;
- `manifest(version)`: from `…/manifest`;
- **data**: per subvolume, a stream received into the pool. The restic source restores files into mounted filesystems instead.

The trait needs a data step that can say "I fill a btrfs pool" or "I fill mounted filesystems". If phase 1's trait is file-only, this is the one change to ask for.

Flow:
1. **Connect:** NAS URL, pinned fingerprint, restore code (as restic).
2. **PC → btrbk run** (date, sizes, ready badge) → **mode:**
   - *Whole PC*: format and restore every non-excluded subvolume;
   - *System only*;
   - *Custom* checklist;
   - *Add subvolumes* (additive, no format; needs the target's btrfs pool with its root subvolume, mounted with `subvolid=5`).

   Each subvolume can take another snapshot from a list.
3. **Rules from the manifest:** a subvolume mounted under another selected one (like @games in @home) needs its parent when formatting, or the parent already in the pool when adding. At least one subvolume must be chosen.
4. **Target disk(s)** by model, serial and size, mapped to the manifest's disks; a disk with the same serial is suggested. Guards:
   - the disk-in-use guard, with no override;
   - firmware mode of the live system = the manifest's boot mode (UEFI vs BIOS), refused otherwise, because `restore_btrfs_system`'s README rule becomes a check;
   - "fits" = the sum of the chosen snapshots' referenced bytes plus 10 % ≤ the new btrfs partition. A smaller disk works, because send/receive does not care about the old geometry;
   - the original GUIDs/UUIDs are reused only when no attached disk has them.
5. **Preflight, nothing written yet:**
   - each chosen snapshot exists, is complete and read-only (one NAS call);
   - the manifest parses;
   - the ESP copy is there, or regeneration is possible;
   - `btrfs`, `mkfs.vfat`, `cryptsetup` are present as needed (the package name is printed when one is missing);
   - free RAM is enough for buffers.
6. **Plan + typed confirmation:** the readable plan (ReaR-style), the disk's model and serial again, "WILL BE ERASED", and the disk name typed to confirm. `--dry-run` runs every step that only reads, and runs the fstab, entry and cmdline rewrites on copies with mock IDs, showing the diffs (`restore_btrfs_system`'s dry-run idea).
7. **Execute:**
   1. GPT and filesystems through the shared code: `mkfs.btrfs -U <fs uuid>`, `mkfs.vfat -i`; wait with `udevadm settle` + polling;
   2. mount the top level; make `snapshot_dir`; for each subvolume, receive the stream with the btrbk name (progress from the byte count against the known size); verify `received_uuid`, read-only and a non-zero generation; then `btrfs subvolume snapshot` to the original path (an existing one is renamed `_rynas_old_<ts>`, never deleted);
   3. recreate the excluded subvolumes empty, with NOCOW where the manifest says so; recreate the swapfile; restore the default subvolume;
   4. mount everything as fstab says;
   5. **boot**, the shared §4.4 chroot step: `/etc/kernel/cmdline` from the manifest; kernels from `/usr/lib/modules/<kver>/vmlinuz` (Arch, Fedora) or `/boot` (Debian); initramfs regenerated (`mkinitcpio -P` / `update-initramfs -u -k all` / `dracut --regenerate-all`); loader reinstalled (`bootctl install` + `kernel-install`, or `grub-install` + `grub-mkconfig`) with the removable fallback. When regeneration fails, the ESP copy from `/.rynas/boot/` is laid down (Arch's case in `restore_btrfs_system`), and a loader-install failure stays non-fatal when the copied EFI files are there;
   6. **ID fix** only when new IDs were generated: fstab, crypttab, loader entries, `/etc/kernel/cmdline`, `GRUB_CMDLINE_LINUX`. Lines of subvolumes that were not restored are commented with `# rynas-restore:` and uncommented by a later *Add*. Then a sanity check: the new root ID on an **active** fstab line, and no old ID left in the entries or the cmdline.
8. **End:** the reboot is offered only when the ID check, the boot files and the loader are all good; otherwise the TUI says exactly what to check. The log goes to the target's `/var/log/rynas-restore-<ts>.log` **and to the NAS**, where it shows in the PC's history. The next btrbk run on the restored PC continues incrementally from the received snapshots (btrbk README) **[U]: to be tested, including PARTUUID-only vs fs-UUID changes**.

Manual fallback, documented on the PC page: with NAS administrator access, `ssh admin@nas sudo btrfs send … | btrfs receive …` and btrbk's own restore steps. This replaces the old hand-kept table.

### 11.5 Arch-specific vs generic

| Stays specific (data-driven, not code) | Becomes generic |
|---|---|
| subvolume names (`@`, `@home`, `@games`, `@log`, `@pkg`) → read from the manifest | presets → "whole / system / custom / add" over the manifest's subvolumes |
| `/.bootbackup/*_post` from an Arch hook → read only for adopted old snapshots (legacy, the newest `_post`) | `/.rynas/boot/` ESP copy + regeneration with the distro's tools |
| `arch-chroot` → our own `--rbind` of dev/proc/sys/run | chroot step shared with restic |
| `bootctl` only, PARTUUID only | systemd-boot, GRUB (Debian/Fedora/Arch), UKI; UUID=, PARTUUID=, LABEL= |
| `pacman -Sy` of tools on the live system | preflight names the package for apt/dnf/pacman; GPT and stream code in the agent |
| NVMe only, Samsung table name `part_table_samsung` | any block device by stable id; GPT from the manifest (MBR later, as §5) |
| Synology `btrfs send --without-syno-features` | Debian `btrfs send` on the NAS (the flag is Synology-only) |

The legacy reader means Marco's existing home PC chain (adopted from the Synology into rynas as in §10.2) restores with no new manifest. In that case the TUI uses `.bootbackup/part_table_samsung` as the GPT source and warns that it is legacy.

### 11.6 Phases and rough sizes

| Phase | Content | Size |
|---|---|---|
| **B1: NAS views and retention** | snapshot table with per-snapshot `du`, run view, health states and the new alerts, "NAS owns retention" with GFS preview and chain-safe delete, setup snippet from ticked subvolumes | **M**: ~2–3k lines (Rust + UI), reuses D-SNAP preview/delete |
| **B2: manifest and setup** | `rynas-agent manifest` and `btrbk-setup` (the shared manifest code from phase 1 + btrbk block + ESP copy), package hooks, btrbk drop-in; NAS manifest reader and "bare-metal ready" | **S–M**: ~1.5–2k lines; depends on phase 1's manifest module |
| **B3: restore source** | `…/send` and `…/manifest` endpoints under restore codes; btrbk source in the agent (versions, pool receive, writable snapshots, excluded/default subvolumes); modes and rules; legacy `.bootbackup` reader; dry-run on copies | **M**: ~2.5–3.5k lines; needs phase 1's TUI, GPT and chroot steps |
| **B4: VM matrix** | Arch + systemd-boot (ESP = /boot), Debian + GRUB, Fedora (separate ext4 /boot), LUKS root; same disk / smaller disk / GUID conflict; first incremental after restore | part of the phase 1 VM matrix (§9 rule 9) |

Order: B1 can start now; it does not touch the agent. B2 and B3 wait for phase 1's manifest and TUI.

### 11.7 Decisions for Marco

1. **Bring back "bare metal through btrbk"** (reverses part of §7) and make the btrbk tab a first-class engine next to restic in the wizard, instead of "maintenance only". *Recommendation: yes; btrfs PCs get the fastest exact restore and readable snapshots on the NAS.*
2. **Transport for restore:** HTTPS stream from rynas under a restore code (proposed), or a second SSH key line with a `--send` mode in `rynas-btrbk-filter`. *Recommendation: HTTPS: one auth path with restic, no key on the live system; the filter keeps its "a PC never reads back" rule.*
3. **Retention owner for new PCs:** NAS (GFS, the PC can only add) or btrbk's `target_preserve` with the NAS floor (today). *Recommendation: NAS by default, PC-side kept for adopted chains.*
4. **Boot files:** regeneration only, or regeneration + the ESP copy in `/.rynas/boot/` as fallback. *Recommendation: both; the copy is small and saved Arch restores without network.*
5. **Manifest writer:** require `rynas-agent` on btrbk PCs (manifest, setup, later status), or also offer a tiny shell hook for PCs without it. *Recommendation: agent only; the bash version would be a second code path to keep in step, and the guided layout step covers PCs without it.*
6. **Legacy support for `restore_btrfs_system` snapshots** (`.bootbackup`, `part_table_samsung`). *Recommendation: read-only support in B3, removed once that PC has manifests in all kept snapshots.*
7. **What happens to `restore_btrfs_system`:** keep it as Marco's independent fallback until B4 passes on that PC's real hardware, then archive it with a pointer to rynas. *Recommendation: as stated; never delete a working restore path before the new one has booted real hardware.*

Sources (2026-11-11): https://github.com/rylos/restore_btrfs_system (README, scripts, hook, tests) · https://github.com/digint/btrbk#restoring-backups · btrfs-send(8)/btrfs-receive(8) as in §10.

### 11.8 Implemented: B1, the NAS side (2026-11-11)

For PCs that already push with btrbk over SSH (§10). Code: `crates/rynasd/src/pc_backup.rs` (logic and routes), new types in `rynas_api_types::pcbackup`, UI `crates/rynas-ui/src/pages/backup/pcs.rs` (Backup › "PCs: btrfs snapshots", presented as a method of its own next to the files engine). API in `../phase1/plan.md`.

- **Snapshot table.** Each snapshot carries `parent` (below) and `size` {referenced, exclusive}. The drawer groups them by subvolume or by **run** (`PcRun`: the snapshots with one btrbk timestamp, `_N` suffix ignored, with the expected subvolumes it lacks and the summed sizes).
- **Sizes, measured cost** (loop image, 3 received snapshots of 20 000 files + 80 MiB, `packaging/test-pcbackup-loop.sh`):
  - quota groups on (non-simple, not marked inconsistent): read live from `/sys/fs/btrfs/<fsid>/qgroups/0_<subvolid>/{referenced,exclusive}`, two small reads per snapshot, 0.1 ms for the folder, no command and no walk;
  - otherwise **Measure sizes** (task `pcbackup.sizes`): `ionice -c3 nice -n19 btrfs filesystem du -s --raw` per snapshot, one at a time, cancellable, 0.21–0.26 s per 20 000-file snapshot here (warm cache): it reads every file's extents, so a 1 M-file PC on HDDs takes minutes per snapshot. Each snapshot is measured **once** (results kept by subvolume UUID; received snapshots are read-only so the referenced size never changes; the exclusive one grows as neighbours go, `?again=true` measures again). No measure ever runs per request.
- **NAS-side retention** (`retention.owner: nas`, Marco's decision 3: the default for new PCs, `pc` for adopted folders):
  - the key line gets `--delete-interrupted` instead of `--delete`: the filter accepts `btrfs subvolume delete` only for interrupted receives (btrbk removes its own "garbled" target before sending again). The same now applies to `delete.allowed: false`, which until now refused even those, against §10.2;
  - the GFS counts are the snapshot jobs' `Retention` (presets and custom, `min` as btrbk's `*_preserve_min`), computed in Rust by btrbk's rules: per subvolume, the first snapshot of each hour/day/week (from Sunday)/month/year within that many of them, by the time in the name (the PC's clock, as btrbk counts), in the NAS's local calendar;
  - **the parent is protected** whatever the counts say: per subvolume the newest complete snapshot by the NAS-side `otime` (names more than a day in the future do not count, so a junk name cannot displace it) is never deleted, by the hourly pass or by hand (`DELETE …/snapshots/{name}` answers 409). On top, the floor of `delete` (`keep_last`, at least 1, and `min_age_days`, both on `otime`), as for deletes the PC asks for; future names are never deleted on the NAS's own; interrupted receives go after an hour with no receive running;
  - each pass (hourly after the alerts' check, or "Delete old snapshots now", task `pcbackup.prune`) takes the filter's own per-device `flock` on the folder (opened by `verify_root`: walked from `/` without symlinks, then checked to be the same inode as the path), lists it again and deletes with `BTRFS_IOC_SNAP_DESTROY` relative to that descriptor, at most 50 per pass; a running receive skips the pass (`last_prune.error`);
  - `POST /pc-backups/plan/retention` previews keep/delete with a reason per snapshot and "frees at least" (the sum of the exclusive sizes, when known);
  - the PC's `btrbk.conf` snippet says `target_preserve_min all` / `target_preserve no` in this mode.
- **Alerts** (one notification per condition, fired once, resolved when it is over, keys `pcbackup:<kind>:<device>`): `pcbackup.late` (now in hours: `alert_hours`, 0 = `alert_days` × 24), `pcbackup.subvolume_late` (an expected subvolume older than the alert time while the PC is not late; "expected" = the manifest's subvolumes, else every received one, minus `ignored_groups`), `pcbackup.receive_failed` (an interrupted receive older than 10 min with nothing receiving: the lock is free) and `pcbackup.receive_stuck` (receiving for more than 24 h), `pcbackup.low_space` (free space below the referenced size of one more full set of parents, or below 5 % of the volume while sizes are unknown).
- **"Ready for bare metal"** (`bare_metal`): a manifest in the newest snapshot of some subvolume (the root names first), read through descriptors (`O_NOFOLLOW` on every step, regular file, at most 1 MiB, parsed as untrusted JSON, cached by subvolume UUID), `format: 1`, a subvolume mounted at `/`, every listed subvolume with a complete snapshot, and a boot loader. Warnings: manifest written more than 7 days before its snapshot, newest run lacking a listed subvolume. PCs without `rynas-agent` show `no_manifest`.
- **Manifest fields B1 reads** (B2 must write at least these): `format` (1), `written_at_ms`, `boot.loader`, `btrbk.subvolumes[] {name (the btrbk name), path, mount}`; other fields (`subvolumes[]` with `excluded`, disks, GPT…) are ignored by B1.
- **Tests:** unit tests of the plan, runs, manifest and alerts in `pc_backup.rs`, API tests on the memory host (`tests/pc_backup.rs`), and `tests/pc_backup_host.rs` (`#[ignore]`, root) on a real loop-mounted btrfs folder: sysfs quota sizes with and without quotas, measure, a symlinked manifest refused (`ELOOP`), the lock shared with the filter, delete under the lock.
- **Open:** the GFS buckets use the NAS's time zone for names without an offset (as `name.rs` reads them), so a PC in another zone shifts by those hours; one shared account still serves every PC (§10.3).

### 11.9 Implemented: B2, the agent-native btrfs method (2026-11-12)

The owner decided on 2026-11-12: weigh whether btrbk is needed at all or the agent alone, avoiding a dependency on btrbk; the naming standards to follow are snapper's, Timeshift's and btrbk's. The first cut of B2 (`rynas-agent btrbk-setup`, which wrote `btrbk.conf`, an SSH key, a btrbk drop-in and package-manager hooks) is replaced: new btrfs PCs need **only `rynas-agent`**, paired like the file method, and send over HTTPS. btrbk over SSH (§10, B1) stays for PCs already set up that way.

**Why no btrbk dependency.** What btrbk did for us on the PC was four things: read-only snapshots, the parent choice, `btrfs send`, local retention. The agent already makes read-only snapshots (file method), already has a paired, pinned HTTPS channel, a scheduler, progress and reports. Keeping btrbk meant a second scheduler (btrbk.timer), a second transport (SSH key, account, sudoers, filter), a config file rynas had to write and diff, and package hooks to keep the manifest fresh. Without it: one binary, one pairing, one schedule, "Back up now" and live progress like the file method, and the manifest is written right before the snapshots, so no hook is needed.

**Naming standards** (snapper, Timeshift, btrbk), checked 2026-11-12:
- **btrbk** `<name>.YYYYMMDDTHHMM[_N]` (the `long` format) is what goes to the NAS, always: B1's runs, retention, alerts and bare-metal check work unchanged, and a chain can move between btrbk and the agent in both directions without a new full send (same names, same `received_uuid` chain).
- **snapper** (`.snapshots/<N>/snapshot`) is not used as the source. `btrfs send` names the received subvolume after the source's basename, so every snapper snapshot would arrive as `snapshot`; renaming on the NAS breaks the rule that a receive goes only into the device folder itself (§10.2) and btrbk interoperability. snapper's cleanup algorithms would also delete a parent unless the snapshot is taken out of every cleanup (then the agent owns it anyway), and snapper usually covers only `/`.
- **Timeshift** btrfs snapshots are **writable** by design (so they can be booted, `timeshift-btrfs/snapshots/<date>/@`); `btrfs send` needs read-only ones, and every one is named `@` or `@home`. Not usable as a source.
- So the agent takes **its own read-only snapshots with btrbk names**, in btrbk's `snapshot_dir` from `/etc/btrbk/btrbk.conf` when the PC has one on the same filesystem (a PC switching from btrbk keeps its local parents), otherwise `/.btrbk_snapshots` (btrbk's usual name; `rynas_api_types::pcagent::AGENT_SNAPSHOT_DIR`). The file method already leaves that folder, `.snapshots` and Timeshift's out.

**PC side** (`crates/rynas-agent`: `send.rs`, `bootcopy.rs`, `btrbk.rs`, `manifest.rs`, `http.rs`, `btrfs.rs`):
- The NAS's plan carries `method` (`PcMethod::Files | Btrfs`); `backup::run` dispatches. A run takes `flock` on `/.rynas/.lock` (also taken by `rynas-agent manifest` and the file method), and refuses while `btrbk.timer` is active (two senders and two pruners on one chain).
- Subvolumes: the mounted subvolumes of the root filesystem (the B2 layout rules: no hard-coded names; cache, log, swap, container and snapshot folders not sent), minus `plan.excludes` entries equal to a mount point. Name: btrbk.conf's `snapshot_name` when there is one, else the subvolume's basename (`@`, `@home`, `root`); duplicates take their path with `_`.
- Order: the **manifest first** (`/.rynas/manifest.json`, with the btrbk-shaped block `agent: true`, the snapshot folder and name → path → mount, plus the boot copies), so the root snapshot carries it; then one timestamp for the run, a read-only snapshot per subvolume (`_N` when the name is taken locally or on the NAS).
- Parent: `GET …/agent/btrfs/snapshots` once; the newest local read-only snapshot of that name (by creation time, then name: names are local time and can go backwards when summer time ends) whose `received_uuid`, or own UUID when it has none (what `btrfs send -p` writes in the stream), is the `received_uuid` of a complete snapshot of the same subvolume on the NAS. The request names the parent by the **NAS' name** of that copy (the names can differ). Otherwise a full send.
- Stream: `btrfs send --proto 1 [-p parent]` (the PC's btrfs-progs; `--proto` only when `btrfs send --help` lists it, older versions send v1 anyway) → zstd level 3 with a 2 MiB window (`AGENT_ZSTD_WINDOW_LOG`) → chunked `POST …/agent/btrfs/receive` on a fresh connection, the answer awaited up to 1 h after the last byte. A body that fails is never terminated cleanly (the NAS sees a broken stream); an early error answer is read. 409 `parent_missing` (a retention pass removed the parent meanwhile) → the same snapshot again in full. Whatever else goes wrong (a refusal, a connection broken after the NAS had finished, a Cancel), the NAS' listing decides before the new local snapshot is deleted: complete there → kept as the next parent.
- Cancel: SIGTERM stops the send at the next read, and every socket read waits in 1 s slices that look at the flag (`http::cancel_on`), so the final wait for the NAS' answer stops within a second instead of an hour.
- Plan excludes that are not a subvolume's mount point (a path inside a sent subvolume, a pattern) cannot be applied by a snapshot: they are reported on the root's capture line, not dropped silently.
- Local retention: after a good send the new snapshot is the next parent; older ones of that name beyond the newest **2** (creation-time order) go, never the snapshot just sent, never the parent it was sent on, never names that are not `<name>.<long ts>`.
- Boot copies leave out systemd-boot's `loader/random-seed`; a crypttab line that opens a device with a key file is reported in the manifest notices (a bare-metal restore needs that key, and an initramfs that embeds it carries it into the snapshots and boot copies).
- Report: `PcRun` with the run timestamp, one `Capture` per subvolume (full / incremental from X / error), raw and compressed bytes; progress every 2 s as for files.
- Hidden test command (root, never against `/` in tests): `rynas-agent btrfs-send --nas URL --token T [--fingerprint FP] [--allow-http] --snapshot-dir DIR --subvol NAME=PATH …`.
- **Removed:** `btrbk-setup`, the btrbk drop-in it installed, the pacman/apt/dnf/kernel-install hooks, the generated `btrbk.conf`, the SSH key. **Kept for legacy btrbk PCs:** `rynas-agent manifest` (btrbk block from `btrbk.conf`, `agent: false`); the btrbk panel shows a one-line systemd drop-in (`ExecStartPre=-/usr/bin/rynas-agent manifest --quiet`) to paste, never installed by rynas (`PcTargetDetail.manifest_hook`).
- Review fixes on what stays: the ESP behind an automount is triggered by opening it (`O_DIRECTORY`), an autofs entry never counts as the filesystem; an empty source never replaces a good copy; 2 GiB cap and a free-space check; only changed files are written (temporary file, fsync, rename each; removed files deleted; mode, owner and mtime kept), so there is never a moment without a copy; a failed copy keeps the previous `BootCopy` record with `stale: true` and `error`, and its folder is not pruned; reading `btrbk.conf` tells "missing" from "unreadable or not UTF-8"; `#` starts a comment after any whitespace.

**NAS side** (`rynasd::pc_backup`, `rynasd::pc_agent`, `rynas_btrfs::send_stream`, `rynas_web::pc_agent`):
- **One device model.** The PC is a paired agent device (token, plan, progress, history, Back up now/Cancel: phase 1) with `settings.method = btrfs`, and its snapshots live in a B1 folder of the same name with `agent: true` (`PcTarget.agent`). B1's snapshot table, runs, NAS-owned retention with the protected parent, sizes, alerts and bare-metal check apply as they are. An agent folder has no key line, no sudoers line and no SSH account (`0700 root:root`); its retention owner is always `nas` (the agent never deletes on the NAS). New PC: create the folder with `agent: true`, then a pairing with `btrfs_target` (device = folder name, no repository: `group` empty). Existing file PC: create the folder with its name, then `PUT …/devices/{d}` with `method: btrfs`. A btrbk folder switches to the agent with `PUT …/targets/{d}` `{agent: true}` (the key line goes, snapshots stay, the chain goes on) and back with `{agent: false, public_key}`.
- **Receive** (`PcBackup::agent_receive`), the protections of the SSH filter and more: device token → method btrfs → the agent folder the PC is **bound to** (`Dev.btrfs_folder` = the folder's `created_at_ms`, set at pairing or when the btrfs method is chosen; a folder removed and made again under the same name refuses the old token, 409; removing an agent folder revokes a btrfs-only PC at once and moves a PC with a repository back to the files method); names in btrbk `long` form (`rynas_api_types::pcbackup::split_long_snapshot`, shared with the agent) and not dated more than a day ahead, the parent of the same subvolume; at most **2 receives at once on the NAS** (`MAX_RECEIVES`, all PCs; 409 `busy` otherwise); the folder walked from `/` without symlinks and locked with the filter's per-device `flock` (one receive at a time, no prune pass meanwhile, busy → 409 `busy`); the snapshot must not exist (409 `already_exists`), the parent must be complete here (409 `parent_missing`); a free-space reserve of max(1 GiB, 2 % of the volume): the decompressed stream stops when it would cross the room measured at the start, and the free space is read again every 64 MiB of stream, so parallel receives (or anything else) filling the volume stop it too (413 up front when there is no room, 422 when the stream crosses it); idle body for 10 min → stopped; below 64 KiB/s on average after the first 15 min → stopped (`RECEIVE_MIN_RATE`; a trickling PC would hold its lock and a slot for the unit's 48 h); zstd windows above 8 MiB refused (decoder memory).
- **The stream guard** (`rynas_btrfs::send_stream::Guard`) reads **every** command whole (≤ 64 KiB, the v1 send buffer) before handing it on: stream version 1 only (v2 `FALLOCATE`, `ENCODED_WRITE` & co. refused: a 170-byte v2 stream could allocate 150 MiB); each command's CRC32C checked (seed 0, no final inversion, as btrfs computes it); only v1 command types (`UPDATE_EXTENT`, a `--no-data` stream, refused); each attribute a v1 type and present **once** (`btrfs receive` uses the last copy of a repeated attribute, so a check that reads the first could be fooled: `PATH` twice created a second subvolume, `CLONE_UUID` twice bypassed the parent check); the first command creates exactly the announced name (`SUBVOL` for full, `SNAPSHOT` with the parent's `received_uuid` as clone UUID for incremental) with a UUID; no second `SUBVOL`/`SNAPSHOT`; `PATH`, `PATH_TO`, `CLONE_PATH` and a hard link's `PATH_LINK` relative, without `.`/`..`/empty components or NUL (`btrfs receive` does not resolve `..`); `CLONE` only from the incoming subvolume or the announced parent (by UUID); nothing after `END`. A symlink's target is content and passes; set-id bits and device nodes pass (system snapshots hold them).
- **Staging**: each receive gets a fresh empty `<folder>/.incoming/<random>/` (`0700 root`, made by rynasd on the locked folder's descriptor) and `btrfs receive --chroot` runs there, so a symlink followed by a later command (`SYMLINK up → /`, then `MKFILE up/x`: no `..`, the guard cannot see it) lands in the staging folder, never next to the PC's other snapshots, which are not reachable by path at all. Afterwards the staging folder must hold **exactly one entry**: the announced subvolume, read-only, its `received_uuid` = the stream's UUID, its `parent_uuid` = the NAS copy of the announced parent (`check_staged`). Then it moves into the folder: `renameat2(RENAME_NOREPLACE)` with the read-only flag lifted for the moment of the move by `BTRFS_IOC_SUBVOL_SETFLAGS` (a read-only directory cannot change parent, `EROFS`; the received UUID stays, checked). Anything else, and every leftover, is removed with the staging folder (subvolumes deleted whole, files and folders unlinked without following links); the staging folders are also cleared before each receive and when rynasd starts.
- **Receive unit** (`receive_unit_properties`): `btrfs receive -e --chroot <staging>/` is never run inside rynasd (its seccomp filter has no `chroot` and its bounding set no `CAP_SYS_CHROOT`); it runs in a transient unit `rynas-pcreceive-<device>` (one name per device: systemd refuses a second one even after a rynasd restart) with `ProtectSystem=strict` + `ReadWritePaths=<folder>` (the whole device folder: a clone needs source and target on one mount, `FICLONERANGE` refuses across bind mounts), `InaccessiblePaths=-<every other device folder>`, `PrivateNetwork`, `PrivateIPC`, `NoNewPrivileges`, `RestrictAddressFamilies=AF_UNIX`, `CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_CHROOT CAP_CHOWN CAP_FOWNER CAP_FSETID CAP_DAC_OVERRIDE CAP_MKNOD` (`CAP_DAC_READ_SEARCH` dropped: same result without it on a loop test; `CAP_FSETID` kept so a write into another user's set-id file keeps its bits), `SystemCallFilter=@system-service chroot mknod mknodat`, `DevicePolicy=closed` with `DeviceAllow=char-* m` and `block-* m` (device nodes can be made, never opened: a `WRITE` into a block node of the NAS' own disk fails with `EPERM`), `ProtectKernelModules/Tunables/Logs`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `ProtectProc=invisible`, `ProcSubset=pid`, `RestrictNamespaces`, `RestrictRealtime`, `LockPersonality`, `MemoryDenyWriteExecute`, `UMask=0077`, `MemoryMax=512M`, `TasksMax=16`, `CPUQuota=100%`, idle-ish I/O, `RuntimeMaxSec=48h`. Not set, on purpose: `PrivateTmp` (a folder below `/tmp` would vanish), `PrivateDevices` (drops `CAP_MKNOD`), `RestrictSUIDSGID` (system snapshots carry set-id files).
- **Where a parent can come from**: `btrfs receive` opens the mount holding its target *before* it chroots and looks parents and clone sources up by UUID across the whole filesystem. In the unit that mount is the bind mount `ReadWritePaths` makes of the device folder, and btrfs-progs refuses sources outside that mount's subvolume ("parent subvol is not reachable from inside the root subvol"; 7.3 says the same for clone sources). That holds because the folder is a subvolume and its own mount in the unit; it is not a promise of btrfs (older btrfs-progs did not check clone sources), hence the guard's UUID rule, the `parent_uuid` check and `InaccessiblePaths`. An agent folder must be a subvolume: with a plain folder the bind mount makes `btrfs receive` lose its parents ("try to remount this btrfs filesystem with fs tree"), so adopting a plain folder for the agent and receiving into one are refused.
- `GET …/agent/btrfs/snapshots`: every snapshot name with `received_uuid` and `complete`, and whether a receive runs.
- rynas-web passes both routes through as streams (no body limit for the receive; rynasd keeps the reserve; deadline 48 h for the answer head).
- The files method's late/critical alerts are skipped for btrfs PCs (the folder's alerts cover them); revoke of a btrfs-only PC removes no key file; restore codes and the TUI's PC list left btrfs-only PCs out until B3 (§11.10), which takes them in.
- The manifest reader caps display strings at 256 characters, drops control characters and layout entries that are not `name`/absolute mount/plain path (`SHOWN_MAX`).

**Tests** (2026-11-12): agent 41 unit tests, clippy clean; rynas-btrfs lib 74 (new `send_stream` 3, IPv6 target URL); rynasd `pc_` unit 27 (new: names, receive sandbox, bounded manifest strings), API `tests/pc_backup.rs` 13 (new 2), the e2e test 1 (`#[ignore]`, run as root once: passed); rynas-api-types 28 + 2; rynas-web 46 + routes; rynas-ui 124, wasm clippy clean; clippy `-D warnings` on rynasd, rynas-web, rynas-btrfs, rynas-api-types. Covered: rynas-btrfs `send_stream` (good streams pass byte for byte; another name, a path below, full vs incremental, another parent, not a stream, bad version, second subvolume, data after END, a second stream, no END, an oversized command all refused; a refused command never reaches the receive); rynasd API on the memory host (`tests/pc_backup.rs`: agent folder without key or account, pairing into it, token, listing, full then incremental, already received, bad names, foreign parent, missing parent, busy, mismatched or broken streams removed at once, no room, method change refused without a repository, no file restore code, switch to btrbk and back with the chain intact, revoke); agent unit tests (parent choice by received UUID, names and `_N`, keep policy never deleting the parent, duplicates, conf parsing, boot copies, stale record, lock, chunked upload, the subvolume-info ioctl layout); an end-to-end run (`crates/rynasd/tests/pc_btrfs_e2e.rs`, root, a throwaway loop btrfs): the agent's `btrfs-send` against rynasd's routes on 127.0.0.1, the real receive in its transient unit, full then incremental (`@data.<ts>` then `_1`), each NAS copy's `received_uuid` = the PC snapshot's UUID, folder `0700`. rynas-web's pass-through was not in that run.

**Security review, second pass (2026-11-12/13).** An independent review found, with hand-made streams: a repeated attribute fooling the guard (`PATH` twice → a second subvolume; `CLONE_UUID` twice → the parent check bypassed), clone sources and parents looked up by UUID across the filesystem, `..` and symlinks reaching the PC's other snapshots from the chroot, a 170-byte v2 stream allocating 150 MiB, a partial check after the receive, a shared free-space check for parallel receives, a trickling PC holding its lock for 48 h, an unbounded zstd window, devices and folders linked only by name, and on the agent a parent sent by its local name, a local snapshot deleted after a receive that had in fact finished, and a Cancel that waited an hour. All are fixed as described above. Evidence on a loop btrfs (kernel 7.3, btrfs-progs 7.3), through the whole path (agent → HTTP → guard → unit → staging check → move), in `tests/pc_btrfs_e2e.rs`: full then incremental (with `--proto 1`) received, moved, `received_uuid` chain right, no `.incoming` left; hostile streams each refused with nothing left in the folder: symlink escape ("the receive left 2 entries"), rename of an older snapshot through a symlink ("No such file or directory": not reachable from the staging folder), repeated `PATH`, `..`, a v2 stream, `MKNOD` of the test's own loop device then `WRITE` ("cannot open …: Operation not permitted"). By hand in the same unit: a `SNAPSHOT` or `CLONE` from another folder's subvolume refused by btrfs-progs ("not reachable from inside the root subvol", "unreachable from nas/pc"), with and without `InaccessiblePaths`; a plain-directory folder losing its parent; the read-only flag flip keeping `received_uuid` and later incrementals working; a set-id file, a read-only folder of another user and an overlayfs whiteout received without `CAP_DAC_READ_SEARCH`.

**Not verified:** the whole flow on a real PC install (Arch, Debian, Fedora) with the service and the UI; `btrbk.timer` detection on every distro; a switch of a real btrbk chain to the agent; very large streams (hours) through rynas-web; the UI at 390 px in a browser. All of it belongs to B4's VM matrix.

**Nested subvolumes without a mount (2026-11-13, after B4).** B4 lost data: `@home/games`, made inside `@home` with no fstab line, was never sent (a btrfs snapshot is not recursive: the snapshot of `@home` holds only an empty folder where `games` is), and the restore made it again empty. Now each run also looks **inside every sent subvolume** (`btrfs subvolume list -o` on its mount and on each child, 8 levels; `send::nested_candidates`, `btrfs::nested_checked`) and sends every nested subvolume as **a group of its own**, after its parent:
- **Left out, quietly** (a rule, not a failure): the agent's and btrbk.conf's snapshot folders, snapper's `.snapshots` and anything inside it, Timeshift's, container engines' storage wherever it is (`docker/btrfs`, `containers/storage`, also rootless engines' under a home, and Incus/LXD `storage-pools`: the engines manage and back up those themselves), the agent's `.rynas-snap` (`btrfs::excluded_subvolume`), cache/log paths (`btrbk::NOT_SENT`), snapshots received onto the PC (a received UUID), and whatever lies inside one left out (none of these is listed further down). A subvolume nested in a sent one but mounted elsewhere is a mounted subvolume like any other.
- **Left out, said on the run's root line** (never silent): a listing that failed ("the subvolumes nested in … could not be listed (…); any of them is not sent"; a nested subvolume whose own listing fails is still sent, and so are its siblings: `btrfs::nested_checked` keeps going), a nested subvolume under another mount inside its parent (a tmpfs over its folder: the path leads to that mount, not to the subvolume; found in the mount table), a read-only nested subvolume that was not received ("a snapshot?"), a path or name that cannot be used in a snapshot name, a nested one whose parent the plan leaves out.
- **The plan's excludes** apply to a nested subvolume by its place (`/home/games`) or by a folder above it (`/home/me/vms` leaves out `/home/me/vms/win10`), so "Subvolumes not sent" in the UI works for them too (the UI maps a group to its place, `BtrbkLayout::place_of`).
- **Names: stable, from the path** (changed 2026-11-13 after the final review). A nested subvolume is sent as its **whole path from the top level with `/` as `_`** (`@home/games` → `@home_games`, `send::nested_name`), unless btrbk.conf has a `subvolume` section for that path, whose `snapshot_name` wins (the chain goes on). The first cut used the basename, btrbk's default (btrbk source, `$default_snapshot_name =~ s/^.*\///`; `btrbk.conf(5)`: "Defaults to `<subvolume-name>`"), with the path as a fallback when the basename was taken: that name depended on which other subvolumes existed in that run, so a nested subvolume could change group between runs (the next send incremental against another subvolume's snapshot, retention mixing groups). btrbk has no recursion: a nested subvolume it sends always has its own section, so the agent takes btrbk's name exactly when there is a btrbk chain to continue; otherwise the name only has to be unique and never change, and the path is both. A name that is not allowed (`valid_snapshot_prefix`: ASCII letters, digits, `_-.@`, at most 64 bytes; `Giochi_è`, `vm+win`, a path longer than that) or is already a mounted subvolume's is a **note** on the run's root line and that subvolume is not sent; only a mounted subvolume without a usable name fails the run. Snapshots go to the same snapshot folder.
- **Parent check**: a local snapshot is a parent only when it comes from the subvolume being sent (`send::taken_of`: its `parent_uuid` is the source's UUID, or the source was made from it, as a restore does), besides the NAS' `received_uuid` match; a name alone never makes a parent.
- **Manifest**: the nested entry of `btrbk.subvolumes` has no `mount`, and `nested_in` (the parent's btrbk name) + `inside` (its path inside the parent, `games`): `{"name":"@home_games","path":"@home/games","nested_in":"@home","inside":"games"}`. Older readers see one more subvolume without a mount.
- **NAS**: nothing special is needed. The nested group is one more group in `btrbk.subvolumes`: it is expected in each run (`lacks`), alerted when late, needed for "ready for bare metal", and kept by the same per-group GFS retention with its protected parent. The manifest reader keeps a nested layout entry only with a valid parent name, no mount and a relative `inside`.
- Tests: `send::nested_subvolumes_without_a_mount_are_sent_as_their_own_groups` (rules, notes, parents at every depth, names, manifest fields, a parent left out), `rynasd pc_backup::nested_subvolumes_in_the_manifest`, `rynas-api-types place_of_nested_subvolumes`; the restore side in §11.10.

**Throughput (measured 2026-11-13, a developer PC with NVMe → DS920+ with one HDD, 1 GbE, through rynas-web TLS):** first full send of `/`, 108 GB raw / 44.7 GB on the wire in 31 min (58 MB/s raw average); the next run, incremental, 17 KB in 1 s. Stage by stage: `btrfs send` alone 458 MB/s, zstd level 3 on one thread over 200 MB/s of output, raw TCP 111 MiB/s, plain `btrfs send | ssh | btrfs receive` 86 MB/s: no stage was saturated, each waited on the next through 64 KiB pipes. Fix: `readahead::ReadAhead` (crates/rynas-btrfs, shared with the agent by `#[path]`), a thread reading 32 × 1 MiB ahead, between `btrfs send` and the compression (agent) and between the stream check and `btrfs receive` (NAS), pipes grown to 1 MiB (`F_SETPIPE_SZ`), 1 MiB writes into `btrfs receive`, 128 network frames queued instead of 16. Same full send, first 10 minutes: 30.3 GiB → 53 GiB (49 → 92 MiB/s raw, 24 → 42 MiB/s on the wire); the HDD receive is the limit now. Compression stays always on: on a LAN it neither helps nor costs (the wire carries 42 of 111 MiB/s), on Wi-Fi, a VPN or the internet it cuts the traffic 2-3 times. **Parents in any btrbk format:** local snapshots and NAS copies named with btrbk's `short`, `long` or `long-iso` timestamps (with `_N`) are parents of the agent's sends, so a btrbk chain moves to the agent without a full send; the agent's own new snapshots stay `long`. **Empty nested subvolumes** (systemd's `/var/lib/machines` and `/var/lib/portables` until something is put there) are not sent: the restored parent keeps an empty folder, which `systemd-nspawn`/`portablectl` use as is; once one holds something it is sent as its own group. **A restored snapshot whose received UUID differs from the NAS copy's** stays a warning: the stream was already checked against its own UUID before the move (an error there), so the data is right and the only cost is one full send next time. **Open:** one shared SSH account still serves btrbk PCs (§10.3); a first full send of a large PC over a slow link can still exceed the unit's 48 h (do the first one on the LAN); the free-space checks read `statvfs`, which lags behind delayed allocation by a few seconds of writes (the 1 GiB / 2 % reserve absorbs it).

### 11.10 Implemented: B3, bare-metal restore from btrfs snapshots (2026-11-13)

The restore source of §11.4 for both kinds of btrfs folder: PCs of the agent's btrfs method (§11.9) and folders fed by btrbk over SSH (§10, including adopted chains). API in `../phase1/plan.md` ("restore/btrfs").

**NAS** (`rynasd::pc_agent`, `rynasd::pc_backup`, passed through by `rynas_web::pc_agent`):
- **Restore codes and the administrator login** now cover a PC with file backups, a folder of btrfs snapshots, or both; a btrbk folder needs no paired PC record (`device` = the folder's name). The session lists the PC with `group` (may be empty) and `btrfs`; a session without a group gets no repository key. Every btrfs route checks that the session covers that very PC (403 otherwise) and keeps the session alive; the report ends it (a btrbk folder has no PC history: audit log and log file only).
- `…/snapshots`: B1's view of the folder for the restore (every snapshot with its subvolume, completeness, read-only state and referenced size when known; the runs by timestamp; the manifest's snapshot).
- `…/manifest?snapshot=`: B1's descriptor-walk reader (`O_NOFOLLOW` each step, ≤ 1 MiB), checked to be JSON of format 1, sent as the PC wrote it. Only complete snapshots directly in the folder.
- `…/legacy?snapshot=` (§11.5): `/etc/fstab`, os-release, and in `/.bootbackup/` the GPT dumps (any small text file whose content is a `sfdisk --dump` of a GPT disk: no file name is hard-coded) and the folders ending in `_post`. No symlink followed, each file ≤ 64 KiB.
- `…/send?snapshot=[&parent=]`: `btrfs send [--proto 1] [-p parent] | zstd` (level 3, 2 MiB window, the agent's own settings). Only complete read-only received subvolumes directly in the folder (checked again by ioctl on the folder's descriptor), the parent of the same subvolume. The folder is walked from `/` without symlinks (`verify_root`) and locked **shared**: a receive or a retention pass (both exclusive) cannot start while it reads, and the UI does not show "receiving". One stream per PC, at most 2 on the NAS (`MAX_SENDS`). The send runs in a transient unit `rynas-pcsend-<device>` that can only read (`send_unit_properties`: `ProtectSystem=strict` with no writable path, `ProtectHome=read-only`, the other device folders inaccessible, no network, `DevicePolicy=closed`, bounding set `CAP_SYS_ADMIN CAP_DAC_READ_SEARCH`, `@system-service`, `RestrictSUIDSGID`, `MemoryDenyWriteExecute`, 256 MiB, 8 tasks, one CPU, idle-ish I/O, 48 h). When the client goes away (the body's channel closes) the reader stops, `systemctl stop` ends the unit and the process is killed; a send that fails after the head ends the body with an error, never with a clean end. Start and end are audited.

**Agent** (`crates/rynas-agent/src/restore/`: `btrfs.rs` new, `legacy.rs` new, changes in `exec.rs`, `plan.rs`, `mod.rs`, `tui.rs`, `source.rs`; `http.rs` streamed GET; `btrfs.rs` ioctls):
- The `RestoreSource` trait already had the pool step (`Fill::BtrfsPool`); it gained `skipped_subvolumes`, `boot_copies`, `describe` and `drain_log`, all with defaults (the restic source is unchanged).
- **Versions** are B1's runs with at least one complete snapshot; a subvolume missing from the chosen run takes its newest complete snapshot not newer than the run (said in the plan), and any subvolume can take another snapshot (`--subvol NAME=SNAPSHOT`, `o` in the TUI's checklist).
- **Modes** over the manifest's `btrbk.subvolumes`: whole PC; system only (`/` and subvolumes mounted under `/usr`, `/var`, `/opt`, `/boot`, `/etc`; home folders, `/root` included, are user data as in Timeshift, `/srv` site data); custom; add (no format, into the PC's existing pool on a device the user picks, mounted `subvolid=5`). **Rules from the mount points**: formatting needs the subvolume mounted at `/`; a subvolume mounted inside another needs that one too (restored with it, or, adding, already in the pool, checked on the device read-only); at least one.
- **Guards**: the disk-in-use guard (no override, also for the pool of *add*), firmware mode = the manifest's, "fits" = the chosen snapshots' referenced bytes plus 10 % (at least 256 MiB, the plan's own rule) on the new btrfs partition (a smaller disk is fine), GUIDs/UUIDs reused only when no other attached disk has them (the shared plan).
- **Preflight**: the shared tools check plus `btrfs` (package name printed), the boot files (a copy in the snapshot or a known initramfs tool; a stale copy is named), at least 256 MiB of free memory; the snapshots are complete by construction (from the listing); the manifest parses.
- **Plan and dry run**: the shared plan (GPT, mkfs with the original IDs) with the source's lines (each receive, what is not restored, where the received snapshots stay); typed confirmation of the disk name (of the device for *add*). `--dry-run` (CLI and TUI) prints the plan and runs the real text fixes (`rewrite_ids`, `comment_unrestored`) on copies of fstab, `/etc/kernel/cmdline`, `GRUB_CMDLINE_LINUX` and a loader entry's `options` from the manifest, with every ID replaced by a made-up one (`mock_ids`), shown as diffs. The restore code is used up by the login all the same.
- **Execute** (§11.4 step 7): GPT and mkfs through the shared code; each chosen snapshot streamed from the NAS through the NAS' own stream guard (`rynas_btrfs::send_stream`, shared as a source file: exactly the announced snapshot, v1, paths inside it, clones only from itself) into `btrfs receive -e --chroot` in its own empty staging folder at the pool's top level (`.rynas-restore-incoming/<n>/`); the folder must then hold exactly that subvolume, read-only, with the stream's UUID as `received_uuid` and a non-zero generation. Then a writable snapshot at the subvolume's path (an existing one renamed `<path>_rynas_old_<time>`, never deleted; the empty folder a nested subvolume leaves in its parent's snapshot is replaced). The received snapshots move into the PC's snapshot folder (`snapshot_dir` mapped to the pool through the root filesystem's mounts: `/.btrbk_snapshots` → `@/.btrbk_snapshots`; `btrbk_snapshots` at the top level when unknown) with the read-only flag lifted only for the move by the ioctl, so `received_uuid` stays. Then the excluded subvolumes made empty (NOCOW by `FS_IOC_SETFLAGS` where the manifest says so; container layers and other tools' snapshots are not made), the default subvolume, the tree mounted as fstab says (subvolumes not restored are not mounted), the boot copies laid on their mount points (`/.rynas/boot/…`, or the legacy `_post` copy) **before** the shared boot step, so the kernels are there: the step then regenerates the initramfs and reinstalls the loader, and where it cannot the copy stands; swap files made again (NOCOW, `fallocate`, `mkswap -U`). Text fixes: fstab lines of subvolumes not restored commented with `# rynas-restore: ` (an *add* later takes the mark away for the subvolumes it adds), old IDs replaced when new ones were made, then the checks: the restored root's UUID or PARTUUID on the active fstab line of `/`, no old ID left in the loader entries, `/etc/kernel/cmdline` and `/etc/default/grub`, and EFI files on the ESP. A failed check is recorded (`Log::problems`): the report says so, and the TUI does not offer the reboot. `/etc/kernel/cmdline` is written only when the command line is known; a manifest's unknown loader or initramfs tool is found in the restored system. The log goes to `/var/log/rynas-restore-<ts>.log` and to the NAS.
- **The next backup is incremental.** The kernel sends a received subvolume with its `received_uuid` as the stream's UUID, so the restored snapshot carries the NAS copy's `received_uuid`; B2's parent choice (`send.rs`, local `received_uuid` = a complete NAS snapshot's) finds it. Checked end to end (below).
- **Legacy reader** (`legacy.rs`, §11.5, decision 6): a run without manifest is read from its own files: the newest GPT dump gives the disk (label-id, PARTUUIDs, types, names, attributes); without one, the guided layout (one disk, an ESP of `--esp-mib` MiB, default 1024, btrfs on the rest, with the PARTUUIDs fstab names). fstab gives the filesystems' UUIDs, the mount points and subvolumes (the btrbk names matched to fstab's subvolumes by name), the snapshot folder below a `subvolid=5` mount; the newest `_post` folder becomes the ESP's content; loader and initramfs tool are found in the restored system. The plan says "legacy" and where each part came from.
- **TUI**: after the login, the method when the PC has both; runs (date, size, complete or what is missing, "legacy" when there is no manifest); mode; a checklist with per-subvolume snapshots for custom and add; the target disk (or the btrfs device for add); the plan. Scripted: `--method files|btrfs --version <run>|latest --mode … --subvol NAME[=SNAPSHOT] --add-to /dev/X --esp-mib N --no-nvram`. **EFI entries**: written into the firmware of the machine running the restore (the restored PC) like an installer does, except with `--no-nvram` (the disk goes into another PC, as `grub-install --no-nvram`/`bootctl --no-variables`) or when a target disk is on USB; the removable path EFI/BOOT boots the disk either way. Hidden test command `rynas-agent btrfs-restore-pool --nas … --code … --pool <mounted top level>`: only the pool step, into a throwaway filesystem.

**UI**: the drawer of Backup › "PCs: btrfs snapshots" has "Restore bare metal" for every folder with snapshots (agent or btrbk): the restore-code button and the two commands (download the agent, `sudo ./rynas-agent-x86_64 restore --nas …`).

**Tests** (2026-11-13): agent 63 unit tests (new: modes, nesting and snapshot choice, snapshot folder in the pool, legacy GPT dump and fstab, guided layout, commented and uncommented fstab lines, ID rewrites and the checks, dry run with mock IDs), clippy clean; rynasd lib `pc_` 31 (new: one send per PC and the global cap, the send unit's properties, dump detection), API `tests/pc_backup.rs` 15 (new: a btrbk folder without PC record: code, login, listing, scope 401/403, manifest, legacy, send full and on a parent, refusals, busy folder, report ending the session, the administrator's choice), all rynasd tests green; rynas-web 46 + routes (the four routes pass through); rynas-ui 124, wasm clippy clean; clippy `-D warnings` on rynasd, rynas-web, rynas-btrfs, rynas-api-types. End to end as root on two loop btrfs filesystems (kernel 7.3, btrfs-progs 7.3), `tests/pc_btrfs_restore_e2e.rs` (`#[ignore]`): `btrfs-send` of a fake root with a manifest into rynasd's folder, a restore code, `btrfs-restore-pool` through the real send route (shared lock, read-only unit, guard, chroot receive) into the second filesystem: `@data` writable with the data and no `received_uuid`, the received snapshot read-only in `@data/.btrbk_snapshots` with the NAS copy's `received_uuid` and a generation, `@data`'s parent = it, the excluded `@cache` made with NOCOW, no staging left, the unit gone; then `btrfs-send` from the restored pool went **incremental** on the restored snapshot (`@data.<ts>_1`: a 64 MiB stream for 64 MiB of new data, not the whole subvolume); the legacy route on the real snapshot (a dump found by content, the `_post` folder, a symlink not followed); a send cancelled after the first chunk: the unit stops within seconds and the PC's slot is free again. The B2 e2e test still passes.

**Review fixes** (2026-11-13, independent correctness and quality reviews of the first cut):
- **Subvolume IDs** (genfstab/archinstall write `subvolid=N,subvol=/@`): a restore makes every subvolume anew, so the IDs change and the kernel refuses the line (or mounts another subvolume). Every fstab line (matched to its filesystem by the **old** spec) and every `rootflags=` (`/etc/kernel/cmdline`, `GRUB_CMDLINE_LINUX`, loader entries) loses `subvolid=` ≠ 5; a line with the ID alone gets `subvol=/<path>` from the manifest's subvolume IDs. This runs before the lines of subvolumes not restored are commented (so ID-only lines are commented too) and before the ID rewrite, in the real run and the dry run alike (`plan::fix_fstab`, `plan::fix_cmdline`; the boot step's cmdline uses the same). The checks report any `subvolid=` ≠ 5 left.
- **The snapshot folder inside `@`**: the received root holds an empty folder where each older snapshot was; such a folder makes way, so an older snapshot of another subvolume still lands in the snapshot folder and the next backup of it stays incremental (e2e below).
- **Only the root's filesystem**: `BtrbkSubvolume.fs_uuid` (written by the agent from the mount table; empty in older manifests = the root's) keeps subvolumes of another btrfs filesystem out (said, their fstab lines stay, that filesystem is not touched); naming one in custom/add is refused. Legacy: a group no fstab line of the root's filesystem names is kept only when fstab mounts no other btrfs filesystem.
- **Paths**: every manifest path (subvolumes, mount points, snapshot folder, boot copies, ESP, swap files) must be plain (no `..`, NUL); refused otherwise. Paths in the pool go through no symlink; paths in the restored system are resolved **inside** the target (a symlink, absolute or not, is followed as in a chroot: NixOS' `/etc/fstab -> /etc/static/fstab` works, nothing reaches the live system) for mounts, the text fixes, the boot copies, swap files, `/etc/kernel/cmdline` and the log (both methods).
- **Staging per run** (`.rynas-restore-incoming/<btrbk timestamp>/<n>`): a failed run deletes what it received; leftovers of an earlier run are said, never in the way. An existing subvolume is set aside with `RENAME_NOREPLACE` as `<path>_rynas_old_<btrbk timestamp>[_N]`.
- **Add**: only into the PC's own filesystem (UUID = the manifest root's; `--other-filesystem` to override); the look at the device mounts it `ro,rescue=nologreplay` (also for a dry run).
- **Legacy `_post` copy**: laid where fstab mounts the ESP; a copy with kernels (`vmlinuz-*`, `initramfs-*`) goes only onto `/boot` (Arch with the ESP at `/boot`, the old hook's layout); onto an ESP at `/boot/efi` or `/efi` it is skipped with a warning.
- **NAS send**: only full streams (the unused `parent` query is gone); a client that takes nothing for 10 minutes, or whose session ended (keepalive check every 5 minutes), stops the send, which frees the shared lock and the slot; each send has its own unit name (`rynas-pcsend-<device>-<random>`), so a retry right after a cancel does not meet the old unit. The receive and the send units share one property base and one launcher. A new restore code replaces the PC's earlier ones.
- **Small ones**: the quoted `UUID="…"` root line passes the check; the agent's chunked reader checks each chunk's CRLF and the trailer; an unused `--subvol X=SNAP` is said; the agent shares rynas-btrfs' `ioctl.rs` as a source file (descriptor-based move of a read-only subvolume); one fstab parser; one path-prefix rule (`under`: `/home` does not hold `/homework`, nested subvolumes of a skipped one are commented too); the TUI says "filesystem" (not "pool"), names the real place of the restore code (Backup → PCs → the PC → Restore code), readiness notes as sentences, internal states as one "internal error" message.
- **Tests** (2026-11-13): agent 82 unit tests (new: paths and in-root symlinks, fstab parsing and subvolume ID fixes, nested commented lines, the quoted root line, boot cmdline mapping, mount-point prefixes, snapshot fallbacks, other filesystems and unused overrides, manifest path checks, sfdisk variants, legacy groups of other filesystems, swap partition, nested names), clippy clean; rynasd, rynas-web, rynas-btrfs, rynas-api-types 559 tests green (API `tests/pc_backup.rs`: a new code replaces the earlier one, full send only), clippy `-D warnings` clean; lock files unchanged. As root on two loop btrfs filesystems: the B2 e2e test, and both restore e2e tests (`--test-threads=1`); the new one: `@` and `@home` with the snapshot folder inside `@`, two runs, the older `@home` restored into `@/.btrbk_snapshots` with the NAS copy's received UUID, no staging left, the restored fstab without `subvolid=` mounting both lines (the PC's old line refused by the kernel), and the next backup of `@home` incremental on the restored snapshot. Finding: `btrfs send` leaves out the empty folders nested snapshots leave in a local snapshot (kernel and btrfs-progs 7.3), so the collision the review feared does not happen with received snapshots; the restore makes way for such a folder anyway.

**Not verified (B4, VMs):** the whole restore on real installs (GPT, mkfs, the boot step with the copies, Arch + systemd-boot with ESP = `/boot`, Debian + GRUB, Fedora with a separate `/boot`, LUKS root), the TUI on a real terminal, *add* on a real pool, a real legacy chain (its dump and `_post` copies), a smaller disk and a GUID conflict, firmware-mode refusal, very large streams through rynas-web over TLS, the UI at 390 px in a browser.

**Fixes after B4** (2026-11-13):
- **Nested subvolumes without a mount** (sent since this date, §11.9): the manifest entry with `nested_in`/`inside` is a subvolume like the others, placed at its **path from the top level** (`@home/games`) **after its parent** (the chosen ones go parents first, by path depth), where the parent's snapshot left an empty folder (removed) or nothing; a non-empty folder or an existing subvolume there is set aside `_rynas_old_<ts>`, never deleted. Its received snapshot goes to the snapshot folder like the others, so its next backup is incremental. The nesting rules gained the **path rule**: a subvolume nested by path in another one needs that one too, at every level, in *custom* and *add* (in *add* the parent may already be in the pool); *whole* takes everything; *system* takes a nested one when its place is a system path (`/var/lib/machines` inside `@` goes with `/`, `/home/games` does not). A nested entry whose parent the manifest does not list is refused. The plan and the log say "not mounted, inside /home/games"; the TUI's checklist says the same (`where_text`). `finish_pool` no longer makes such a subvolume empty (it exists, received).
- **GPT disk GUID** (`label-id`): the "in use" check now also reads the GPT of every other attached disk (`gpt::disk_guid_of`, primary or backup header) besides `/dev/disk/by-uuid` and `by-partuuid`, so a restore next to the original disk gets a new disk GUID (the plan says so) instead of a second disk with the same one.
- **Private mount namespace**: `restore` (TUI and scripted) starts with `unshare(CLONE_NEWNS)` + `mount --make-rprivate /` (`restore::private_namespace`, the file method's `private_mounts`), so the restore's mounts never propagate into the namespaces of systemd-udevd, -resolved, -logind on the live system; `umount -R` then really frees the pool and `cryptsetup close` works. Every tool the restore runs inherits it; it ends with the process.
- **ESP options**: a FAT filesystem is mounted with the PC's masks from its fstab line (`umask`/`fmask`/`dmask`/`uid`/`gid`, `fstab::fat_options`), `fmask=0077,dmask=0077` when the line names none, so `bootctl` no longer warns about a world-readable random seed.
- **os-prober**: `update-grub`/`grub-mkconfig`/`grub2-mkconfig` run with `GRUB_DISABLE_OS_PROBER=true` in their environment only (`exec::grub_mkconfig`); the target's `/etc/default/grub` is not changed (an explicit `GRUB_DISABLE_OS_PROBER=false` there still wins).
- **Adding a snapshot that is already there** (found by the second B4 run, §11.11): when the snapshot folder already holds the same snapshot (a copy received earlier, same received UUID, or the PC's own original, whose UUID is the NAS copy's received UUID), that one stays the parent and the copy just received is deleted with its staging folder; only a different snapshot under the same name keeps the received copy in `.rynas-restore-incoming` (said). The B4 note "fixed on main" was wrong: the copy stayed every time.
- **Log noise**: the final cleanup unmounts only the helper folders that are mounted (no more "umount …/probe: not mounted", "…/add: not mounted").
- **Tests** (2026-11-13): agent 86 unit tests (new: nested discovery and names, nested selection rules, the disk GUID conflict in the plan, FAT options), clippy clean; rynasd, rynas-web, rynas-btrfs, rynas-api-types 562 green (new: nested entries in the manifest, `place_of`), clippy `-D warnings` clean; rynas-ui 124, wasm clippy clean; lock files unchanged. As root on two loop btrfs filesystems: the B2 e2e test and the three restore e2e tests; the new one (`nested_subvolume_without_a_mount`): `@nhome/games` without a mount sent as `games`, the parent's NAS snapshot without its data, the NAS layout with `nested_in`/`inside` and nothing missing, *custom* without the parent refused before anything is written, the whole restore placing a writable `@nhome/games` with its file and its received snapshot in `@n/.btrbk_snapshots`, the subvolume deleted and *added* back (the duplicate copy removed, no staging), the next backup of `games` incremental.
- **UI**: the restore-code note said that codes made earlier stay valid until they expire; since the review fixes a new code replaces the PC's earlier ones, and the note (both panels, `pca.restore_code_valid`, en + it) says so.

**Final review fixes** (2026-11-13, independent review of the whole B2–B4 change):
- **Nested discovery keeps going**: a nested subvolume whose own listing fails no longer drops every other nested subvolume of that mount (`btrfs::nested_checked` notes the child and goes on; the subvolume itself is still sent, its listing error is a note); places under `btrbk::NOT_SENT` and places under another mount inside the parent are never looked into; a nested subvolume under such a mount (a tmpfs over its folder) is a note, never snapshotted through the path (that would have sent the mount's content under the hidden subvolume's name).
- **Names**: nested subvolumes take their whole path (`@home_games`, §11.9 "Names"), stable between runs; a name that is not allowed or taken is a note, so a `Giochi_è` or a 70-byte path no longer fails every run. `pick_parent` also checks the parent came from the subvolume being sent (`taken_of`). *Add* makes the writable subvolume from the snapshot already in the snapshot folder when it is the same one (the copy just received is removed), so its next backup stays incremental under that check (found by the e2e test).
- **Excludes**: a folder the plan leaves out takes the nested subvolumes inside it along. **Container storage** anywhere (`docker/btrfs`, `containers/storage`, rootless engines under a home, Incus/LXD `storage-pools`) is left out and not made again by a restore.
- **os-prober**: `GRUB_DISABLE_OS_PROBER=false` in the target's `/etc/default/grub` (sourced after the environment) won; now the target's `/etc/grub.d/30_os-prober` is also made non-executable for the `grub-mkconfig` run and its mode put back.
- **crypttab** `keyfile-offset=`/`keyfile-size=` go to `luksAddKey` as `--new-keyfile-offset`/`--new-keyfile-size` (a value that is not a number: a problem, no key added). An *add* retried in the same minute gets its own staging folder (`<ts>_N`).
- **Manifest**: a btrbk.conf subvolume gets its filesystem UUID from the device it was located on (`manifest_block`), so one not mounted on a second filesystem is no longer taken for the root's. The restore's fstab check uses the shared fstab parser.
- **Simpler**: `safe_rel`/`under` live in `mounts.rs` (the backup side no longer imports the restore module; `restore::paths` re-exports them); the nested walk uses `safe_rel`; the TUI's place text is `restore::btrfs::where_text`. New hidden command `btrfs-nested --root DIR --subvol PATH=PLACE` (tests): what a run would send of the nested subvolumes, with names and notes.
- **Tests** (2026-11-13): agent 93 unit tests (new: a failing child and a hidden place, nested names that are notes and btrbk.conf names, an excluded folder, container storage anywhere, the parent's origin incl. after a restore, crypttab key offsets, the same-minute staging folder), clippy `-D warnings` clean, static musl build; rynasd + rynas-btrfs 433 green, clippy clean; lock files unchanged. As root on two loop btrfs filesystems: the B2 e2e test and the three restore e2e tests; `nested_subvolume_without_a_mount` now runs the agent's own discovery on the real filesystem (`games` sent as `@nhome_games`; `vm+win` a name note, a read-only snapshot a note, `over/inner` under a tmpfs a "hides it" note; only games sent) and its next backup after *add* is incremental.

**Open:** a received snapshot's `received_uuid` differing from the NAS copy's is reported, not fatal (the next backup of it is then full); the restore always sends full streams; *system only* is a fixed list of system mount points; the guided layout has no swap partition; a dry run uses up the restore code, as for the file method.

### 11.11 B4 results: the bare-metal restore VM matrix (2026-11-13)

Setup (a developer PC, at most 2 VMs of 2 GiB, OVMF, user-mode network): the four source PCs of `~/vm-scratch/vms/b4` (README there), a Debian 12 live VM with an empty target disk, the static musl `rynas-agent` of this commit, and as the NAS rynasd's router with the real `HostReceive` (receive and send units, root) on a 16 GiB loop btrfs, served on loopback (the agent with `--allow-http`, reached at 10.0.2.2). Scripted mode throughout (`restore --method btrfs --mode … --disk /dev/vdb --confirm vdb`, `--dry-run` first); the TUI was not driven.

| PC | B2 full + incremental | dry run | restore | boot of the restored disk alone, fresh NVRAM | markers, fstab, swap | next backup |
|---|---|---|---|---|---|---|
| Arch, systemd-boot, ESP = `/boot` | ok (958 MB full in 32 s, 22 KB incr.) | ok | ok, 12 s | ok (`BOOTX64.EFI` fallback, `bootctl` entry) | ok; swapfile made again and active | incremental (33 MB, parent = the restored snapshot) |
| Debian 13, GRUB, `/boot` in `@` | ok (489 MB, 12 s) | ok | ok, 15 s | ok | ok (no swap in this image) | incremental |
| Fedora 45, separate ext4 `/boot`, shim | ok (3 subvolumes, 1.2 GB, 55 s) | ok | ok, 28 s (`grub2-install` refuses on Fedora EFI: non-fatal, the copied shim/GRUB stand) | ok (SELinux relabel, then running) | ok (zram swap) | incremental |
| Debian 13, LUKS2 + btrfs, key file in crypttab | ok | ok | ok, 29–39 s | **failed** before the fix below (initramfs never unlocked), ok after it | ok; swapfile active | incremental |

Extra cases: **smaller disk** (Arch 6 → 4 GiB) together with the **GUID conflict** (original disk attached): new fs UUIDs and PARTUUIDs, fstab, `/etc/kernel/cmdline` and the loader entry rewritten, boots alone and with the original attached, next backup incremental. **Add subvolumes** (Arch, `@home` deleted from the restored pool): `@home` back with its files; the received copy stays in `.rynas-restore-incoming` when the same snapshot is already in the snapshot folder (known, fixed on main). **Firmware mode**: the live VM booted with SeaBIOS is refused at preflight ("the PC booted in UEFI mode, this live system in BIOS"). **Legacy reader**: a folder of debian-grub snapshots without manifest, with `/.bootbackup/vda.sfdisk` and a `*_post` ESP copy, received on the NAS: GPT from the dump, fstab IDs, the `_post` copy on `/boot/efi`, loader and initramfs tool found in the restored system; boots.

Fixed in this phase: the musl release build did not link (`renameat2` has no musl wrapper: raw syscall now); the receive unit lacked `CAP_SETFCAP`, so any real root with file capabilities (Arch's `newgidmap`) failed with EPERM; a LUKS container opened by a crypttab key file now gets that file (from the restored system) as a second key.

Still open from B4: unmounted nested subvolumes (`@home/games`) are not backed up, so their data is lost on restore (made empty, as §11.9 says); the disk GUID conflict is never detected (only `/dev/disk/by-uuid` and `by-partuuid` are looked at), so the restored disk shares the original's GPT label-id; mounts made by the restore propagate into the live system's service mount namespaces (systemd-udevd, -resolved, -logind…), so after `umount -R` the pool stays mounted there and `cryptsetup close` fails (a private mount namespace for the restore would avoid it); the ESP is mounted without its fstab options during the restore (`bootctl` warns about a world-readable random seed); `grub-mkconfig` in the chroot lets os-prober add the live system; the boot step writes EFI entries into the NVRAM of the machine running the live system (intended on real hardware). All but the last are fixed since (§11.9, §11.10 "Fixes after B4"); the second run below checks them.

**Second run** (2026-11-13, same setup, the code of this change: the review fixes of §11.10, the nested subvolumes of §11.9, the fixes after B4; static musl agent; harness `~/vm-scratch/b4nas/` with `matrix.sh`/`cycle.sh`/`addmode.sh`/`legacy.sh`, NVRAM of the live VM reset before each restore, the restored disk booted alone with a fresh NVRAM):

| PC | B2 full + incremental | nested `games` sent | restore (dry run first) | boot alone, fresh NVRAM | markers incl. `/home/games` | next backup |
|---|---|---|---|---|---|---|
| Arch, systemd-boot, ESP = `/boot` | ok (959 MB in 33 s; 23 KB) | yes, plus `machines` and `portables` (nested in `@`) | ok, 11 s; ESP mounted `fmask=0077,dmask=0077`, no random-seed warning | ok | all 6 ok; swapfile active | incremental (33 MB) |
| Debian 13, GRUB, `/boot` in `@` | ok (489 MB in 14 s; 7 KB) | yes | ok, 11 s; `update-grub` with `GRUB_DISABLE_OS_PROBER=true`: no live-system entry | ok | all 6 ok | incremental (48 MB) |
| Fedora 45, ext4 `/boot`, shim | ok (3 subvolumes, 1.2 GB in 37 s) | yes | ok, 25 s (`grub2-install` refuses on Fedora EFI, the copied shim/GRUB stand, as before) | ok | all 6 ok | incremental |
| Debian 13, LUKS2 + btrfs, key file | ok (495 MB in 17 s) | yes | ok, 31 s; key file added as second key; **`cryptsetup close` succeeded** (private mount namespace) | ok, unattended | all 6 ok; swapfile active | incremental (62 MB) |

Extra cases: **smaller disk + GUID conflict** (Arch 6 → 4 GiB, the original disk attached): the plan said "the disk GUID is in use on another disk: a new one is made"; the restored disk's PTUUID differs from the original's (and the PARTUUIDs and both filesystem UUIDs are new); fstab, `/etc/kernel/cmdline` and the loader entry rewritten; boots with the original attached (root from the restored disk) and alone; markers incl. `games`; next backup incremental. **Add** (on that restored disk, `@home` and `@home/games` deleted): `--subvol games` alone refused ("games is nested inside @home … restore @home too"); `--subvol @home --subvol games` brought both back with their files, `games` a subvolume again; first try left the received copies in `.rynas-restore-incoming` because the same snapshots were already in the snapshot folder (**bug, fixed**: §11.10), after the fix "it stays the parent, the copy just received is removed" and no staging left. **Legacy** (debian-grub's snapshots without manifest, a GPT dump and a `_post` copy): GPT from the dump (label-id kept: no other disk had it), the `_post` copy on `/boot/efi` mounted with fstab's `umask=0077`, GRUB and update-initramfs found, boots, markers ok; `/home/games` is absent, as expected for an old chain that never sent it. No "umount … not mounted" lines in any log.

Bugs found by this run: the duplicate received copy in *add* (above, fixed and covered by the e2e test). The harness's `matrix.sh check` left the restored VM running, which made the next PC's steps fail at once (one VM at a time); it now stops it.

**Third run (da2bdcd)** (2026-11-13, a quick confirmation after the final review fixes: path-based nested names, the parent_uuid check in the parent choice, Add then incremental, the os-prober hook, crypttab key offset/size; same harness and rules, a fresh 16 GiB loop btrfs as the NAS volume). "Incremental" was checked on the NAS: every received snapshot after the first has a btrfs Parent UUID equal to the previous one's UUID.

| PC | full + incremental | restore (dry run first) | boot alone, fresh NVRAM | markers incl. `/home/games` | next backup |
|---|---|---|---|---|---|
| Arch, systemd-boot | ok (959 MB in 30 s; 23 KB in 0.9 s); nested names `@home_games`, `@_var_lib_machines`, `@_var_lib_portables` | ok, 11 s; ESP mounted `fmask=0077,dmask=0077` | ok | all 6 ok; swapfile active | incremental (33 MB, all 5 subvolumes) |
| Arch, Add (`@home` + `@home_games` deleted on the restored disk) | — | `--subvol @home_games` alone refused ("nested inside @home … restore @home too"); both together ok, no staging left, "it stays the parent, the copy just received is removed" | ok | all 6 ok | incremental for all 5, `@home` and `@home_games` with the Add-received snapshots as parents |
| Debian 13, LUKS2 + btrfs, key file | ok (495 MB in 17 s; 10 KB) | ok, 31 s; key file added as second key; `cryptsetup close` ok | ok, unattended | all 6 ok; swapfile active | incremental (62 MB) |
| Debian 13, GRUB | ok (489 MB in 10 s) | ok, 11 s; `30_os-prober` not executable during `update-grub` (and executable again after) | ok | all 6 ok | incremental (48 MB) |

grub.cfg of the restored debian-grub has no os-prober entry (only "Debian GNU/Linux" and its submenu); the image has no os-prober package, so this shows the hook is put back, not that it kept an entry out. Found: in scripted mode a LUKS restore without `--passphrase-file` erases and partitions the disk before it fails ("a LUKS container needs a passphrase", `restore/exec.rs:439`); the dry run does not flag it either. Checking it in `run_execute` (`restore/mod.rs:279`) before `exec::execute` would fail at once. The harness's `cycle.sh` had dropped the `EXTRA="--passphrase-file /root/pass"` the earlier LUKS runs used.

Not verified in either run: the TUI on a real terminal, very large streams through rynas-web over TLS, the UI at 390 px in a browser, real hardware NVRAM.

## 12. Implemented: the file method, phase 1 (2026-11-11)

What runs (API in `../phase1/plan.md` "API v1: PC backup, file method"):

- **NAS** (`rynasd::pc_agent`, `rynas_web::pc_agent`):
  - restic REST v2 served per backup group, with append-only, quota, the 423 gate and token/session rules (§4.1);
  - pairing, revocation, restore codes, recovery kits, the daily maintenance in the memory budget (§4.7), alerts;
  - streamed downloads and zips: the worker writes to its stdout, events go to its stderr;
  - the console tab "PCs: file backup".
- **Worker** (`rynas-backup-worker`): `init_pc` (§4.7 profile), `add_key`/`remove_key`, `index_info`, `stream` (one file or a stored zip64), and a minimum age for `forget`.
- **Agent** (`crates/rynas-agent`, its own workspace; a static musl binary of 6.6 MB, `packaging/agent/build.sh`):
  - pairing with the certificate pinned by its SHA-256;
  - its own small HTTP/1.1 client: rustls with a pin verifier and keep-alive, so no CA store is needed on a live system;
  - a `rustic_core` backend over the REST protocol, waiting out 423;
  - the service with a long poll, Back up now and Cancel, and a sleep inhibitor;
  - the backup run itself: a private mount namespace in which every source is bound read-only at its mount point and saved as `/` in one version. btrfs gets read-only snapshots of the mounted and nested subvolumes (snapper `.snapshots`, Timeshift and container engines excluded); LVM gets snapshots; ext/XFS are read hot with the changed files listed, or frozen with a watchdog; the ESP is copied as files;
  - the manifest, progress and reports.
- **Restore** (`rynas-agent restore`):
  - a TUI and a scripted mode over the same engine (`restore/exec.rs`): GPT with the same GUIDs, LUKS with the same UUID, mkfs with the original IDs (FAT16/32 kept), btrfs subvolumes and the default one;
  - files restored with `rustic_core` (numeric owners, xattrs, hard links);
  - ID rewrite when another disk has the IDs; a chroot that writes `/etc/kernel/cmdline` first, then the initramfs, GRUB (EFI/BIOS) or systemd-boot with `kernel-install` for machine-id entries, and the removable fallback;
  - a report to the NAS. The `RestoreSource` trait has the btrfs pool step (§11.4).

**Tests on 2026-11-11** (a developer PC; at most 2 VMs at a time, 2 GiB RAM each, OVMF, user-mode network; the NAS was a local `rynasd --dry-system` + `rynas-web` on 127.0.0.1 with its real certificate on :18651):

| Test | Result |
|---|---|
| stock restic 0.18.1 as the client of the REST handler | backup and snapshots ok; `forget` refused (append-only); `key add` refused; no token → 401; a restore session reads and cannot write (403); stock restic opens the folder with the recovery kit; download of a 50 MB file identical; the zip checks with `unzip -t`; maintenance refused with `busy` within 10 min of an upload; a revoked token → 401 |
| Debian 13 genericcloud (ext4 `/` without LVM, ESP FAT16, GRUB + shim, UEFI), SQLite writer running | pairing with the pinned certificate; first backup read hot: 22 953 files, 1.46 GB in 40 s, 8 files listed as changed while read (the SQLite files and the writer's counters); second backup frozen: 6.5 s, nothing changed; manifest correct (UUIDs, PARTUUIDs, GPT, loader, initramfs tool) |
| btrfs on a loop device, `@data` with a nested subvolume and a snapper `.snapshots` | read-only snapshots, the nested subvolume's files in the version, `.snapshots` empty, snapshots removed after the run, manifest subvolumes right |
| service: "Back up now" from the console | picked up by the long poll at once; progress (phase, bytes) visible in the console; version in 6 s |
| bare-metal restore from a Debian 12 live VM onto an empty **3 GiB** disk (source 4 GiB), scripted mode | preflight named the missing `dosfstools`; GPT, mkfs with the original UUIDs, 1.4 GB restored, initramfs, `grub-install`, `update-grub`, removable fallback: 32 s in all |
| boot of the restored disk alone, fresh NVRAM | `running`, no failed units; SQLite `integrity_check` ok and both counters equal (the frozen version); a 20 MB file's SHA-256 ok; `cap_net_raw` on ping kept; a hard link kept |

**Not tested in a VM yet:** systemd-boot and UKI, LUKS, LVM snapshots, XFS, Arch and Fedora, the TUI itself (the scripted mode runs the same engine), BIOS boot. LVM and md layouts are refused by the restore of this version (`manual_layout`).

**Open:**
- the administrator + TOTP login (2026-11-12, Marco: "si") is only in the TUI, not in the scripted mode. rynas-web checks it like a web login (same rate limit; an account without TOTP is refused), opens no session, and rynasd gives one-time codes for every PC: 5 minutes, one batch, and using one ends the others;
- a dry run uses up the restore code;
- the agent binary is not served by the NAS yet (the commands point to the GitHub release);
- the btrfs method's source in the TUI (B3) is in since 2026-11-13 (§11.10).

## Sources

Checked 2026-11-11 unless noted.

- restic: https://github.com/restic/restic/releases/tag/v0.19.1 · https://restic.readthedocs.io/en/stable/040_backup.html · https://restic.readthedocs.io/en/stable/REST_backend.html · https://pkg.go.dev/github.com/restic/restic
- rest-server: https://github.com/restic/rest-server/releases/tag/v0.14.0 · https://github.com/restic/rest-server/blob/master/handlers.go · https://github.com/restic/rest-server/blob/master/repo/repo.go
- rustic: https://docs.rs/rustic_core · https://rustic.cli.rs/docs/FAQ.html · https://rustic.cli.rs/docs/comparison-restic.html · https://github.com/rustic-rs/rustic/issues/1412
- Synology ABB: https://www.synology.com/en-global/dsm/7.4/software_spec/abb · https://kb.synology.com/en-in/DSM/help/ActiveBackup/activebackup_business_physicalserver_tasklist?version=7 · https://global.download.synology.com/download/Document/Software/AdminGuide/Package/ActiveBackup/All/enu/ABB_Recovery_Media_Creation_Guide_enu.pdf
- Veeam: https://helpcenter.veeam.com/docs/agentforlinux/userguide/system_requirements.html · https://helpcenter.veeam.com/docs/agentforlinux/userguide/val_first_steps_iso.html · https://helpcenter.veeam.com/docs/agentforwindows/userguide/recovery_media.html · https://github.com/veeam/blksnap
- UrBackup: https://www.urbackup.org/download.html · https://wwwold.urbackup.org/restore_changelog.html · https://www.urbackup.org/features.html
- PBS client: https://pbs.proxmox.com/docs/backup-client.html
- ReaR: https://github.com/rear/rear/releases · https://relax-and-recover.org/rear-user-guide/scenarios/restic.html · https://github.com/rear/rear/blob/master/doc/user-guide/06-layout-configuration.md
- Clonezilla: https://clonezilla.org/downloads/stable/release-notes.php · Borg: https://borgbackup.readthedocs.io/en/2.0.0b20/usage/serve.html
- Windows: https://learn.microsoft.com/en-us/windows/win32/vss/security-considerations-for-requestors · https://learn.microsoft.com/windows-hardware/drivers/dashboard/code-signing-attestation
- fsfreeze: https://man7.org/linux/man-pages/man8/fsfreeze.8.html · Debian cloud images: https://cloud.debian.org/images/cloud/
- abb-agent: https://github.com/rylos/abb-agent (README, 0.2.17) · Marco's notes: restore_btrfs_system, btrbk-nas
