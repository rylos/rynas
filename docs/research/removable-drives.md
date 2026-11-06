# Removable drives: research and decisions

Date: 2026-11-06. Scope: USB disks and sticks, USB card readers, SD cards in a built-in reader,
optical drives and anything the kernel flags as removable, managed from an icon in the app bar
(next to the notifications bell) and from Hardware › Removable drives. Behaviour modelled on
the maintainer's desktop "USB Manager" plugin, adapted to a headless NAS whose root daemon
mounts drives itself (no udisks2).

Sources (read 2026-11-06): udisks2 `data/builtin_mount_options.conf`, `src/udiskslinuxdrive.c`,
`udiskslinuxfilesystem.c`, Debian `udisks2` rules (trixie); Unassigned Devices plugin source
(`include/lib.php`, SMB and common settings pages); OpenMediaVault `globals.inc`, filesystem
docs, `usbbackup` unit template; the vendor help pages of two appliance NAS systems (external
devices, eject, shared folders); kernel `fs/ntfs3/super.c` (v6.12) and ntfs3 docs; Debian 6.12
kernel config; `systemd-mount(1)`, `udevil.conf`, `usbmount.conf`.

## What the reference systems do

| Topic | Appliance NAS A | Appliance NAS B | Plugin-based NAS | Debian NAS distro | udisks2 (desktop) |
|---|---|---|---|---|---|
| Mount location | fixed root, per-device folder numbered by plug order | fixed root, numbered device folders (change after reboot) | `/mnt/disks/<label>` (vendor+model or serial without a label), editable per device, stored by serial | `/srv/dev-disk-by-uuid-<uuid>`, in fstab with `nofail` | `/media/<user>/<label>`, else UUID; clashes get `1`, `2`… |
| Auto-mount on plug | always | always | off by default (global switch, per-device flag) | no (admin mounts once, fstab keeps it) | daemon never; the desktop session asks |
| Network share | always a shared folder, admin-set permissions, re-applied on reconnect | a shared folder per partition, normal permissions | per-partition toggle; public / private / hidden modes | none automatic (create a shared folder on it) | n/a |
| Mount options | — | execute bit removed on FAT/exFAT/NTFS/HFS+ | `relatime,nodev,nosuid`, `umask=000` on non-POSIX | `defaults,nofail` | always `nodev,nosuid`; vfat `shortname=mixed,utf8,flush`, exfat `iocharset=utf8,errors=remount-ro`, ext `errors=remount-ro`, ntfs3 first then ntfs-3g `windows_names` |
| Busy unmount | — | — | always lazy (`umount -l`): never "busy" | — | refuses (DEVICE_BUSY), the desktop lists the processes |
| Dirty NTFS / fsck | — | — | "Check filesystem": read-only check, then repair on request | — | — |
| Encryption | none for USB | — | LUKS open/close (passphrase in a temp file, shredded) | — | LUKS1/2 |
| Safe removal | unmount ("eject"); the share disappears | unmount | unmount | — | PowerOff: refuse if the drive or a sibling LUN is busy, fsync, SYNCHRONIZE CACHE + START STOP UNIT (failures logged), write `1` to the USB device's `remove`; Eject: `eject` |

Kernel facts (Debian 13, kernel 6.12): exFAT and ntfs3 are in-kernel modules
(`CONFIG_EXFAT_FS=m`, `CONFIG_NTFS3_FS=m`); ntfs3 refuses a read-write mount of a volume that
Windows left dirty ("volume is dirty and \"force\" flag is not set!") or hibernated / Fast
Startup ("Can't mount rw!"), read-only works. `ntfs-3g` (2022.10.3) is the FUSE fallback and
brings `ntfsfix`.

## Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| R1 | Discovery | sysfs + `/run/udev/data` + `/proc/self/mountinfo` + `/proc/swaps`, read every 2 s by a dedicated thread in rynasd, full scan only when a cheap signature changes (device list, mount table, sizes, udev entry times) | the plugin's proven approach: no daemon dependency, a few file reads per tick, a dead device can only block that thread (guidelines S7) |
| R2 | What is removable | USB bus, SD (`mmcblk` type `SD`, not eMMC), `sr*`, `removable=1`; never: system mount points, swap, RAID/LVM members, filesystems in `/etc/fstab`, `UDISKS_IGNORE`/`UDISKS_SYSTEM`, the USB boot flash of some appliances (USB ids `f400:f400`) | a NAS must never offer to power off a disk it runs on |
| R3 | Mount root | `/media/<name>` (FHS place for removable media), one system-wide root | readable paths for shares, scripts, the file manager; numbered folders change with plug order |
| R4 | Names | the label cleaned like udisks (`/`, quotes, backslash, control characters, a leading dot → `_`; 64 characters), else the UUID, else the kernel name; clashes get `-2`, `-3`…; the name chosen for a filesystem UUID is remembered (`/var/lib/rynas/removable/names.json`, 256 entries, least recently used dropped) | the same drive comes back at the same path (stable shares and backup targets) |
| R5 | Auto-mount | **on** by default (setting), at plug-in and at boot (first reading within 5 minutes of the system start); never for LUKS; a volume unmounted by hand stays unmounted until it is plugged in again | a headless NAS has no desktop session to ask; both appliance systems mount at once; mounting exposes nothing by itself (sharing is off, R7). The desktop plugin's default (off) suits a laptop where any stick should not be touched |
| R6 | Options | `nosuid,nodev,noexec,noatime` everywhere. FAT/exFAT/NTFS: `uid=0,gid=<access group>,fmask=0117,dmask=0007` (+ vfat `utf8=1,shortname=mixed,flush,errors=remount-ro`, exfat `iocharset=utf8,errors=remount-ro`, ntfs3 `iocharset=utf8,windows_names`, ntfs-3g `windows_names`); ext2/3/4 `errors=remount-ro`; btrfs/xfs/f2fs the common set; HFS+, ISO 9660, UDF read-only (`uid`, `gid`, group-readable modes) | nothing on a stick runs on the NAS (`noexec`, as the appliances and usbmount/udevil do); no access-time writes on flash; non-POSIX files belong to one group instead of world-writable `umask=000` |
| R7 | Network sharing | **off** by default; when on, every mounted volume is an SMB share `usb-<name>`, private (`valid users = @<access group>`, no guest), read-write unless set read-only; FAT/exFAT/NTFS without streams/DOS attributes; written to `/etc/samba/rynas-removable.conf`, included at the end of `smb.conf`; removed and its clients disconnected (`smbcontrol smbd close-share`) before the volume is unmounted | never expose a random stick; private like the appliance default (administrators); a connected SMB client must not keep a drive busy |
| R8 | Access group | one setting, default `rynas-admin` (administrators; `users` = every account rynas creates): owner group of non-POSIX files and the share's `valid users` | the file manager runs as the requesting user: the same group rule applies to files and shares |
| R9 | Busy | refuse (never `umount -l`), name the processes that hold the volume: `/proc/<pid>/{cwd,root,exe,fd/*,maps}` compared by device number (`GET …/holders`, also in the task error) | a lazy unmount before a power-off loses data |
| R10 | Safe removal | as udisks2 without udisks: release every volume (unmount, `cryptsetup close`) of the drive and of its sibling LUNs, `fsync`+`BLKFLSBUF`, SYNCHRONIZE CACHE + START STOP UNIT through `SG_IO` (failures logged), `1` to the USB device's `remove`; optical `CDROMEJECT`; other removable media START STOP UNIT with eject; SD card in a built-in reader: release only | what the drive supports, the plugin's single "Remove safely" |
| R11 | Mounting from rynasd | transient units through `systemd-mount --no-ask-password --collect --fsck=no` (bound to the device: an unplug stops the unit) and `systemd-mount --umount`; plain `mount`/`umount` only when systemd is not PID 1 | rynasd runs in its own mount namespace with a seccomp filter without `mount(2)`: mounts must happen in PID 1's namespace (same rule as the storage volumes) |
| R12 | Dirty NTFS | try ntfs3 read-write; on its dirty/hibernated refusal mount read-only and notify (`removable.ntfs_read_only`: run chkdsk or shut Windows down fully); never `force` | data safety; a later "clear dirty flag" (`ntfsfix -d`) and a filesystem check are open items |
| R13 | LUKS | unlock with a passphrase typed in the menu (password field, never stored, never logged, sent once on cryptsetup's standard input), then mount; lock = unmount + close; mapper `luks-<uuid>` | the plugin's flow; secrets rules of the storage write side |
| R14 | Notifications | `removable.unplugged` (warning) when a mounted drive disappears without a safe removal; `removable.safe_to_remove` (info, setting, on by default); `removable.ntfs_read_only` (warning); failed actions are failed tasks; plugging in is not notified | the plugin's "notify only what needs attention" |
| R15 | Actions | immediate (not staged), each a rynasd task `removable.<verb>`; the request is checked against a fresh reading first (404, 409 `conflict`, 409 `busy`); settings are the configuration key `removable` (Save applies it) | guidelines A7 (system changes are tasks), A19 for settings |
| R16 | Dependencies | none required; Recommends `cryptsetup-bin` (LUKS), `ntfs-3g` (fallback NTFS driver, `ntfsfix`), `exfatprogs`, `dosfstools`; a missing driver shows "no driver installed" on the volume, a missing cryptsetup refuses unlock with 409 | generic Debian, degrade gracefully |

Open items: per-device overrides (auto-mount, read-only, share, mount name), filesystem check
and repair (read-only check first), "clear NTFS dirty flag", formatting, USB copy button on
appliances that have one (a hardware plugin feature), APFS (no in-kernel driver).

## File manager integration

The file manager lists mounted removable volumes next to the shared folders and ejects them:

- Rust: `rynas_removable::mounts(&drives) -> Vec<RemovableMount>` and
  `rynas_removable::Removable::safely_remove(&drives, drive, &ctx)`; in rynasd
  `rynasd::removable::start_remove(&svc, user, drive)` starts the task `removable.remove`.
- HTTP: `GET /api/v1/removable/mounts` → `[RemovableMount {removable: true, device, drive,
  device_name, label, fs_type, mount_point, read_only, remove_method}]`; a files route
  `POST /api/v1/files/eject {share}` maps the entry to `drive` and calls
  `POST /api/v1/removable/drives/{drive}/remove` (202 `TaskStarted`; 409 `busy` names the
  holders, `GET /api/v1/removable/volumes/{device}/holders`).
- The removable menu's Open button navigates to `/files/<mount point>` (`/files/media/STICK`).
