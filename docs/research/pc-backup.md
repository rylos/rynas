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

Throwaway qemu/KVM VMs on pc-work: UEFI (OVMF), user-mode network, everything in the session scratchpad, deleted afterwards. **Source:** Debian 13 genericcloud, one 4 GiB disk (GPT: p1 ext4 `/` with no LVM, p14 BIOS boot, p15 ESP vfat), restic 0.18.0. The repository sat on a second virtual disk. **Live system:** a Debian 12 genericcloud VM (restic 0.14.0), chosen for its different UUIDs. **Target:** an empty **3 GiB** disk, smaller than the source. abb-agent 0.2.17 came from its release tarball.

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

> **Marco, 2026-11-11, decisions on §11:** all seven recommendations accepted. On retention and transport: for new btrfs PCs **the agent does everything** (read-only snapshots, parent choice, `btrfs send` of the system's btrfs-progs over HTTPS to rynas, a few local snapshots kept as parents); rynas runs the NAS-side retention. Snapshot names stay btrbk-compatible so a PC can switch either way without a new full send. PCs already set up with btrbk over SSH (such as PC-casa) keep working as today; the agent restores both.

**In short:** Marco's request (2026-11-11) brings **bare metal through btrbk** back. §7 had dropped it. The btrbk receive of §10 stays as it is. The plan adds four things:
- a **layout manifest that travels inside the root snapshot**;
- NAS-side views, retention and readiness checks;
- a **btrbk source in the same `rynas-agent` restore TUI** that phase 1 builds for restic;
- an HTTPS stream from rynas, so nothing has to be kept by hand on a USB stick.

The logic comes from Marco's `restore_btrfs_system` (MIT; https://github.com/rylos/restore_btrfs_system, read 2026-11-11). Nothing here is implemented yet. **[U]** marks claims still to verify in a VM.

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
   - `rynas-agent btrbk-setup --nas <url> --device <d>` (one command, shown filled in) reads the PC's real layout (top-level mount, subvolumes, which are snapshot-worthy, the existing `snapshot_dir`) and writes `/etc/btrbk/btrbk.conf`. It shows a diff when one exists and never overwrites silently. It also creates the key and installs the manifest hook (§11.3). For PCs without the agent, the panel keeps the hand-written snippet, now generated from the subvolume list the user ticks;
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

The legacy reader means Marco's existing PC-casa chain (adopted from the Synology into rynas as in §10.2) restores with no new manifest. In that case the TUI uses `.bootbackup/part_table_samsung` as the GPT source and warns that it is legacy.

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
6. **Legacy support for `restore_btrfs_system` snapshots** (`.bootbackup`, `part_table_samsung`). *Recommendation: read-only support in B3, removed once PC-casa has manifests in all kept snapshots.*
7. **What happens to `restore_btrfs_system`:** keep it as Marco's independent fallback until B4 passes on PC-casa's hardware, then archive it with a pointer to rynas. *Recommendation: as stated; never delete a working restore path before the new one has booted real hardware.*

Sources (2026-11-11): https://github.com/rylos/restore_btrfs_system (README, scripts, hook, tests) · https://github.com/digint/btrbk#restoring-backups · btrfs-send(8)/btrfs-receive(8) as in §10.

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
