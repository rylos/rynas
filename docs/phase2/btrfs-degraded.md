# A btrfs volume that lost a device

Status: implemented 2026-11-18 (after 0.8.21). Gap row: `dsm-migration-gaps.md` ("Degraded btrfs
boot").

rynas makes its own volumes as md arrays with a single-device btrfs on top; a missing drive there
is md's business, and since 0.8.11 an array left inactive after boot is started degraded
(`rynas_storage::recover`). A btrfs filesystem with its **own** RAID profile (several devices,
data and metadata `raid1`, `raid1c3`, `raid10`, `raid5/6`...) is adopted as a volume when it is
mounted at `/<label>`, and a root filesystem can be one too (a system on two NVMe drives in btrfs
`raid1`). Before this change such a filesystem with a dead member did not mount after boot, and
nothing said why.

## 1. Research

Sources: the btrfs documentation (btrfs.readthedocs.io: Volume management, `btrfs-replace(8)`,
`btrfs-device(8)`, `btrfs(5)` mount options, Status), `systemd.mount(5)`, the linux-btrfs and
systemd-devel archives and distribution trackers on degraded boot, read 2026-11-18. Claims marked
**[verified]** were checked on Debian 13 (btrfs-progs 6.14, systemd 257) with loop devices, in
`packaging/test-btrfs-degraded.sh` or while capturing its fixtures.

### 1.1 How btrfs behaves without a device

- **Mount.** A filesystem with a missing device mounts only with `-o degraded` ("allow mounts
  with fewer devices than the RAID profile constraints require"). Without it the kernel stops
  with `devid N uuid … is missing`, `failed to read the system array: -ENOENT` **[verified]**.
- **Read-write degraded mounts are checked per chunk.** Since kernel 4.14 a read-write degraded
  mount (or remount) goes through `btrfs_check_rw_degradable()`: every chunk that lost more
  devices than its profile tolerates makes the mount fail ("A read-write mount (or remount) may
  fail when there are too many devices missing, for example if a stripe member is completely
  missing from RAID0"). So a degraded read-write mount never runs on incomplete data; before
  4.14 a two-device raid1 got one read-write degraded mount only. Read-only degraded mounts are
  not checked that way and would return I/O errors on the missing data: rynas never uses them.
- **Writes while degraded.** "If a new chunk is going to be allocated and the existing devices
  cannot meet the requirements for the current profile then mount will fall back to other
  profiles like SINGLE"; the documentation recommends to "check and balance chunks with unwanted
  profiles, after a read-write degraded mount". Seen on a three-device `raid1c3` metadata
  filesystem mounted on two devices: new metadata and system chunks were written `single`, and
  were still there after the replace **[verified]**. The fix is a *soft* convert, which only
  touches chunks not already in the target profile:
  `btrfs balance start -dconvert=raid1,soft -mconvert=raid1c3,soft <mnt>` (system chunks follow
  the metadata filter; a different system profile needs `-sconvert` and `-f`) **[verified]**.
- **Seeing it.** `btrfs filesystem show` prints a mounted filesystem's missing device as
  `devid 3 size 0 used 0 path  MISSING` (empty path) and an unmounted one as
  `*** Some devices missing`; `btrfs device usage -b` prints it as `missing, ID: 3` with the
  chunks it held and its unallocated space (their sum is its size); `btrfs filesystem usage`
  has `Device missing:`; `btrfs device stats` names it `[devid:3]`; sysfs has
  `/sys/fs/btrfs/<uuid>/devinfo/<id>/missing` (kernel 5.9+) **[verified]**.
- **Replace.** `btrfs replace start <devid> <new> <mnt>`: "If the source device is disconnected
  from the system, you have to use the devid parameter format"; "the targetdev needs to be same
  size or larger than the srcdev"; then "the filesystem has to be resized to fully take
  advantage of a larger target device": `btrfs filesystem resize <devid>:max <mnt>`. Progress:
  `btrfs replace status -1` (`12.3% done, …`, `Started on …, finished on …`); it insists on
  reading or rebuilding all data and stops at an unreadable block.
- **Remove.** `btrfs device remove missing <mnt>` (once per missing device: `missing missing`)
  on a degraded mount: the chunks of the missing device are rebuilt on the others. "Device
  removal must satisfy the profile constraints": a two-device raid1 cannot go to one device
  (convert first), a raid1c3 needs three devices left, and the others need the free space.
- **No split-brain protection.** Two halves of a mirror each mounted degraded and written
  diverge for good; superblock generations differ but the kernel does not refuse the reunion. A
  distribution tracker on "automatic degraded raid for unattended boot" (open since 2022) names
  this the main obstacle and proposes a "has been mounted degraded" record that refuses a later
  mount on the other half. A device that only dropped out and comes back next to the newer
  ones is caught up by btrfs (generation checks on read, a scrub).

### 1.2 Boot: systemd, udev and Debian

- udev's `64-btrfs.rules` runs `btrfs ready` on every btrfs device and sets `SYSTEMD_READY=0`
  until all devices of the filesystem are there. The `/etc/fstab` mount unit `Requires=` its
  `.device` unit, so with a missing member it **waits** for a device that never becomes ready
  (until the job timeout, then fails); `degraded` in fstab or in `rootflags=` does not change
  that **[verified: the fstab unit stays queued behind the device job]**.
- A mount made outside the unit (`mount -o degraded …`, run in a transient service) stays: the
  unit has `Requires=` and `StopPropagatedFrom=` on the device, not `BindsTo=` (only
  `x-systemd.device-bound` adds that), and the device never had a stop job **[verified: the
  unit turns active and stays mounted]**.
- **Root.** The root is mounted by the initramfs before any of this. On Debian
  (initramfs-tools) the scripts scan the btrfs devices and mount with `rootflags`: with a member
  missing the boot ends in the initramfs shell. `rootflags=degraded` on the kernel command line
  (at the boot loader menu: `e` in GRUB or systemd-boot, add it to the `linux` line) mounts it
  degraded; from the shell, `mount -o degraded /dev/<member> /root` then `exit` does the same.
  Putting `degraded` permanently in the command line is discouraged: it would also mount a
  half that should not be mounted (the split brain above).

### 1.3 What NAS systems do

- A ZFS-based NAS imports a pool with a missing disk as DEGRADED by itself when every vdev
  still has its redundancy, alerts, and offers "replace"; a pool without enough replicas is not
  imported. ZFS resilvers a returning disk from transaction groups, so it has no split-brain
  problem to guard.
- Appliance systems built on md with a single-device btrfs (rynas' own volumes) start the array
  degraded and show a repair wizard: choose the new drive, rebuild, follow the progress.
- A NAS distribution built on btrfs native RAID shows "missing devices" on the pool, asks the
  administrator to add the `degraded` mount option, warns while it is set, offers "remove
  detached disks" and replace, and advises a balance afterwards.
- A NAS OS with btrfs pools for its cache is reported to mount a pool degraded when a member is
  missing and the profile allows it, and offers to replace or remove the member.

Best ideas kept: keep a volume available when no data is lost (the ZFS behaviour), because a
NAS that stays down until someone logs in is the worse failure for a household; let the kernel's
per-chunk check be the final word; record degraded mounts to refuse the split brain; one wizard
per repair (replace, remove, convert) with the plan shown first; tell the administrators loudly
each time.

## 2. Design

### 2.1 Detection

- **Mounted filesystems** (`btrfs filesystem show --mounted`): one of several devices gets the
  repair facts from `btrfs device usage -b` (cached with the other btrfs details, never read
  while a disk under it sleeps): `BtrfsFilesystem::repair` = missing devids, the size of the
  largest missing device (the smallest replacement), the chunk kinds without a copy (`lost`:
  a profile is lost when a missing device holds one of its chunks and the missing devices
  outnumber its tolerance: single/dup/raid0 0, raid1/raid10/raid5 1, raid1c3/raid6 2, raid1c4
  3), the weaker chunks to convert (per kind, the profiles weaker than the strongest one in
  use), whether the missing devices can be removed (no loss, the devices left at least the
  strongest profile's minimum, their unallocated space, less 256 MiB of working room each, at
  least what the missing ones held: a removal on loop devices without that room failed with
  ENOSPC; a new mirrored copy never goes where a copy is already, so with one device missing
  and as many left as copies each device left must take the chunks it has no copy of, total of
  those chunks minus what it holds, and with several missing only the `n - copies + 1`
  devices with the least room count), `degraded_mount` and `system` (mounted at `/`).
- **Configured volumes that did not mount**: `/etc/fstab` btrfs lines at `/<label>` (by `UUID=`,
  `LABEL=` or device), not mounted, whose members present (lsblk) are fewer than the
  superblock's `num_devices` (`btrfs inspect-internal dump-super`, read only while their disks
  are awake). They are listed with `mountpoint: None` and `repair.mounted == false`, the members
  present with devid and generation.
- **Dashboard** (rynas-web, unprivileged, no command): such a volume is an array in state
  "inactive" when its members carry `ID_BTRFS_READY=0` in the udev database, `/sys/fs/btrfs/<uuid>`
  does not exist and `/etc/fstab` mounts it at `/<label>`. Mounted degraded filesystems were
  already "degraded" (sysfs `devinfo/<id>/missing`).
- **Storage**: the tree marks an unmounted volume failing and a degraded one warning; the
  volume page has a "Devices" card for every btrfs volume of several devices (devices, profiles,
  what is missing or weak, the repair buttons, and the root filesystem's boot note).
- **Notifications**: `storage.btrfs_unmounted` (critical, until it is mounted),
  `storage.btrfs_missing` per devid (critical, existing), `storage.btrfs_weak_chunks` (warning,
  once no device is missing, until converted), `storage.btrfs_scrub_needed` (warning, every
  device back after a degraded mount and no clean scrub yet, see §2.2), and the events of the
  degraded mount below.

### 2.2 The degraded mount after boot

`rynasd::notifications::spawn_btrfs_degraded_mount`, the same rules as the md arrays: nothing
before 3 minutes of uptime, the members present unchanged for 2 minutes (a slow drive gets its
chance), only volumes first seen unmounted within 15 minutes of the boot (a volume unmounted on
purpose later, or a rynasd restarted at noon, is left alone), the volume's reservation against
any storage operation, read again under it. Then `mount -t btrfs -o <fstab options>,degraded,
device=<each member> <member> /<label>` in a transient unit (rynasd's sandbox forbids mounts,
and the fstab unit would wait for the device units). The kernel refuses when a chunk has no
complete copy; the notification says it (`storage.btrfs_mount_failed`), or says the volume runs
degraded on N of M devices (`storage.btrfs_mounted_degraded`).

**Split brain.** Every degraded mount (after boot or from the UI) is recorded in
`<state>/storage/btrfs-degraded.json` before it runs (the devids it ran on, their newest
superblock generation), and the record is put back as it was when the mount fails and nothing is
mounted, so a refused mount never blocks the other half. A later degraded mount whose devices
present include none of those is refused, after boot (`storage.btrfs_split_brain`) and from the
UI: those devices were the missing ones and hold older data. The file is written to a temporary
file of its own, flushed, renamed and its folder flushed, one writer at a time; a file that
exists but cannot be read stops every degraded mount (`storage.btrfs_guard_unreadable`) instead
of counting as "no record".

**A device that comes back.** When every device is present again, the ones that were missing
still lack the copies of what was written meanwhile (data written into existing mirrored chunks
while degraded has one copy only; new chunks are `single`). The btrfs documentation's answer is
a scrub (it rewrites the stale copies from the good ones, as corrected errors) and a soft convert
for the `single` chunks. So the record stays: the moment every device is back, the start of the
last scrub is noted; the record ends only when a scrub started after that finishes with no
uncorrectable error (corrected ones are the expected repair). Meanwhile the volume shows "run a
data scrub" with the Scrub button and `storage.btrfs_scrub_needed` warns, and if the devices
that stayed die before it, the returned ones are refused alone (split brain). A replace or a
removal that leaves nothing missing ends the record at once: the missing device was rebuilt
from the copies or is gone.

The returned device also keeps error counters (`btrfs device stats`, typically
`generation_errs` from the stale copies read before the scrub). btrfs keeps them until they
are reset, and any counter above zero keeps the volume at "Warning", also after the clean
scrub: the scrub proves the data good again, not the device. The volume page lists the devices
with counters above zero, says what each kind means and what the last scrub found, and offers
"Reset error counters" (`POST /api/v1/storage/btrfs/{uuid}/reset-errors`, confirmed, a task
running `btrfs device stats -z`): the reset is the administrator's acknowledgement.

Limit: a scrub repairs what it can verify by checksum. Files without checksums (`nodatacow`
mounts, folders with `chattr +C`, the NOCOW shared folders and app folders of rynas) are not
rewritten: on the device that came back their blocks written while it was away stay old, and
a read may return the stale copy. After a reunion, check or restore such files (or copy them
once, which writes both copies anew); a replace or removal instead rebuilds the device from
the good copies and is not affected.

To give up the newer half on purpose (it is lost for good), mount by hand:
`mount -o degraded /dev/<member> /<label>`, then replace, as root.

**Setting and default.** Per filesystem, Storage › the volume › "After boot": "Mount degraded
automatically after boot", **on by default** (`StorageTuning::btrfs_manual` lists the UUIDs where
it is off). Why on: the kernel's check already refuses a degraded read-write mount on incomplete
data, the record above refuses the split brain that makes btrfs upstream and distributions keep
it manual, it happens only for what the boot left unmounted, and a household NAS that waits for
an administrator after a drive dies is down for days; the ZFS-based systems keep a degraded pool
available the same way. Off, the volume waits and the page offers "Mount degraded". The root
filesystem is never mounted by rynas (the boot loader's command line decides), so it has no
such setting.

### 2.3 Repairs (tasks, `docs/phase1/plan.md` "API v1: storage")

| Route | Plan | Confirmation |
|---|---|---|
| `POST /storage/btrfs/{uuid}/mount-degraded` | `mount … -o degraded` (an unmounted configured volume; refused for a split brain) | the plan (not destructive) |
| `POST /storage/btrfs/{uuid}/replace` {devid, new_disk} | `wipefs -a` the new disk, `btrfs replace start <devid> /dev/<disk> <mnt>`, wait (`replace status`, progress), `resize <devid>:max`, convert what is weak then | typed volume name and the new disk's serial, typed on the plan; no drive is chosen in advance, the form shows the drive's model, serial and what is on it, and the plan warns about another btrfs on it (maybe the other half of a volume) |
| `POST /storage/btrfs/{uuid}/remove-missing` | `btrfs device remove missing… <mnt>`, convert what is weak then | the plan |
| `POST /storage/btrfs/{uuid}/convert` | the soft balance of the weaker kinds (only once no device is missing: chunks would be written weak again) | the plan |

`dry_run` answers with the plan. `btrfs replace start` returns before the kernel shows the replace:
"Never started" is accepted for the first minute of polls. A removal whose device usage is not
known (a drive asleep) is refused rather than planned blind. Checks before: a fresh inventory, the new disk not in use and
not the system disk, at least the size of the missing device (the kernel checks too), free for
the kernel (`O_EXCL`), no other operation on the disk or the volume. A replace needs a mounted
filesystem (mount it degraded first) and no lost chunks. The whole disk is used (a member that
was a partition is replaced by a disk: for a root filesystem, set up the boot partition and the
boot loader on the new disk separately; the page says so). A cancel stops a running replace or
balance in the kernel (`replace cancel`, `balance cancel`). The deduplication of the volume
gives way, as for a scrub.

After a repair the filesystem keeps the `degraded` option until it is mounted again (it has no
effect once every device is present); the next boot mounts it normally.

### 2.4 Root filesystem on several devices

Detected like any mounted filesystem (`repair.system`). The page says: until the missing device
is replaced, the NAS starts only with `rootflags=degraded` added at the boot loader (§1.2), and a
replacement disk gets no boot partition. rynas never edits the boot loader. Replace, remove and
convert work online on `/` the same way.

## 3. Tests

- Parsers and rules, on outputs captured from btrfs-progs 6.14 on loop devices
  (`crates/rynas-storage/src/btrfs_repair_tests.rs`, fixtures `btrfs-*degraded*`,
  `btrfs-device-usage-*`, `btrfs-dump-super.txt`): device usage, `show` of a missing device,
  superblock, lost profiles per tolerance, weaker chunks and the balance command, removal
  limits, fstab volumes left unmounted, the guard, the boot timing, the plans and their runner
  (the convert step reads the usage again).
- The dashboard's udev/fstab detection (`overview.rs`), the notifications (`rynas-notify`
  checks, rynasd events), the UI's state and device mapping.
- `packaging/test-btrfs-degraded.sh`: a privileged container on loop files only (no systemd,
  rynasd with `--storage-loop-disks`): unmounted volume listed, degraded mount, remove refused,
  too-small replacement refused, replace + resize + convert, split brain refused, normal mount
  with both halves and convert, record forgotten, remove missing on a three-device raid1.
  The 15-minute boot window cannot be reached in a container (the host has been up longer): the
  boot path's timing is covered by unit tests, its mount command by the same function the API
  uses.

## 4. Not done

- An md+btrfs volume of rynas is unaffected (md handles it).
- No automatic `rootflags=degraded`: deliberately left to the administrator at the boot menu.
- Profiles of an unmounted filesystem are not known before mounting (they live in the chunk
  tree): the kernel's check decides; the page cannot say beforehand whether data is lost.
