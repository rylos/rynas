# rynas: per-disk tuning values (D-SYSTUNE follow-up)

Research for `crates/rynas-storage/src/tuning.rs` (recommended vs current values) and the per-type
udev defaults in `crates/rynas-storage/src/domain.rs`. Target: Debian 13 (kernel 6.12, newer
via backports), md RAID (1.2) → optional LUKS2 → btrfs, or single disks; DS920+/DS923+ class
boxes and homelab servers. Supersedes the table in `system-reliability-tweaks.md` §6 where they
disagree. Product names below are for this document only, never for the UI.

## 1. What the NAS projects do

| Project | Setting | Value | Source |
|---|---|---|---|
| TrueNAS SCALE | APM | default `DISABLED` → runs `hdparm -B 255` (APM off); choices 1/64/127/128/192/254 | [disk.py](https://github.com/truenas/middleware/blob/master/src/middlewared/middlewared/plugins/disk.py), [power_management_linux.py](https://github.com/truenas/middleware/blob/master/src/middlewared/middlewared/plugins/disk_/power_management_linux.py) |
| TrueNAS SCALE | Standby | default `ALWAYS ON` (`hdparm -S 0`); choices 5 min-330 min; applied 60 s after boot | same files |
| TrueNAS SCALE | Temperature alert | **drive-reported** limit: hwmon `temp1_crit` (drivetemp = SCT limit, nvme = CCTEMP); alert when `temp >= crit` | [disk_class.py `temp()`](https://github.com/truenas/middleware/blob/master/src/middlewared/middlewared/utils/disks_/disk_class.py), [disk_temp.py](https://github.com/truenas/middleware/blob/master/src/middlewared/middlewared/alert/source/disk_temp.py) |
| TrueNAS SCALE | Scheduler / read-ahead / write cache | not managed (ZFS has its own I/O scheduler; kernel defaults) | code search in truenas/middleware: no scheduler or read-ahead handling |
| OpenMediaVault | APM / AAM / spindown / write cache | per disk, all default **off/disabled** in the form; applied with `smartctl --set=apm,…,standby,…,wcache,…` at boot and after resume | [20hdparm.sls](https://github.com/openmediavault/openmediavault/blob/master/deb/openmediavault/srv/salt/omv/deploy/smartmontools/20hdparm.sls), [disk-form-page.component.ts](https://github.com/openmediavault/openmediavault/blob/master/deb/openmediavault/workbench/src/app/pages/storage/disks/disk-form-page.component.ts) |
| OpenMediaVault | Write cache gotcha | saving the disk form with the default unchecked box **turns the write cache off** | same form (`writecache` default `false`) |
| Unraid | Temperature | HDD warn 45 / crit 55 °C; SSD warn 60 / crit 70 °C; **NVMe = WCTEMP / CCTEMP** from `nvme id-ctrl`; per-disk override | [default.cfg](https://github.com/unraid/webgui/blob/master/emhttp/plugins/dynamix/default.cfg), [Wrappers.php `get_nvme_info`](https://github.com/unraid/webgui/blob/master/emhttp/plugins/dynamix/include/Wrappers.php) |
| Unraid | Scheduler | "Auto" = leave as is (mq-deadline); mq-deadline/kyber/bfq/none selectable for all array disks | [DiskSettings.page](https://github.com/unraid/webgui/blob/master/emhttp/plugins/dynamix/DiskSettings.page), [helptext.txt](https://github.com/unraid/webgui/blob/master/emhttp/languages/en_US/helptext.txt) |
| Unraid | Spin-down | global "Default spin down delay": Never, 15 min … 9 h; per-disk override | DiskSettings.page |
| Unraid | md tunables | `md_num_stripes`, `md_queue_limit`, `md_sync_limit`, `md_write_method` (auto = read/modify/write), `nr_requests` "Auto" | DiskSettings.page; these belong to Unraid's own md driver, **not** Linux md: not transferable |
| DSM (DS923+, DSM 7.2, kernel 4.4, observed 2026-11-05) | Scheduler | HDD `cfq` (old kernel), NVMe `none`, md/dm `none` | `/sys/block/*/queue/scheduler` on the home NAS |
| DSM (same) | Read-ahead | members 192 KiB, md 192-256 KiB, one dm volume 2048 KiB | `/sys/block/*/queue/read_ahead_kb` |
| DSM (same) | RAID5 `stripe_cache_size` | **4096** (kernel default 256) | `/sys/block/md3/md/stripe_cache_size` |
| DSM (same) | Resync limits | 300000 / 600000 KB/s (kernel 1000 / 200000) | `/proc/sys/dev/raid/speed_limit_*` (may reflect the "resync faster" choice) |
| DSM (same) | Write-intent bitmap | none on all arrays | `md/bitmap/location` |
| DSM (same) | Write cache / APM | write cache **on**, APM **off**; spin-down is host-driven (`standby_force="yes"`) | `hdparm -W -B`, `/etc.defaults/synoinfo.conf` |
| DSM (same) | Hibernation | default `standbytimer="20"` min (here changed to 60); USB disks `usb_standbytimer="10"` | `/etc.defaults/synoinfo.conf`, `/etc/synoinfo.conf` |
| DSM (same) | Temperature | disk: fan steps at 41/46/50/53 °C (cool profile) or 46/52/55/58 °C, **shutdown at 61 °C**; M.2: fan full at 60-64 °C, **shutdown at 70 °C** | `/usr/syno/etc.defaults/scemd.xml` |
| DSM (same) | dirty memory | `vm.dirty_background_ratio=5`, `vm.dirty_ratio=20` | `sysctl` |
| QTS | Temperature alarm | separate HDD and SSD thresholds (defaults not documented); **TLER/ERC timer** for all disks | [QTS 5.0 disk global settings](https://docs.qnap.com/operating-system/qts/5.0.x/en-us/disk-device-global-settings-E1C221D2.html) |
| Proxmox VE, Rockstor, XigmaNAS | — | nothing beyond kernel defaults worth copying (Proxmox: no disk power UI; XigmaNAS is FreeBSD) | — |

**Best ideas to take:** APM off by default and standby off by default (TrueNAS, OMV, Unraid
"Never"); host-driven spin-down instead of trusting the drive timer (DSM); temperature limits
read from the drive itself, with a per-type fallback (TrueNAS, Unraid for NVMe); a larger RAID5
stripe cache (DSM: 4096); an ERC/TLER setting for array members (QTS). **Not to copy:** OMV's
write cache defaulting to off once a disk form is saved.

## 2. Kernel and vendor evidence

### I/O scheduler

- Kernel default: `mq-deadline` for single-hardware-queue devices (SATA/SAS/USB), `none` for
  multi-queue (NVMe) ([elevator.c `elevator_get_default`, v6.12](https://github.com/torvalds/linux/blob/v6.12/block/elevator.c)).
- md and dm (dm-crypt) are bio-based: they have **no scheduler**; only the member disks do.
  dm-crypt already hands writes to a dedicated thread that sorts them before submission.
- BFQ costs ~2 µs per request on a laptop CPU and caps IOPS ([bfq-iosched.rst](https://docs.kernel.org/block/bfq-iosched.html));
  it targets interactivity, not NAS throughput. Kyber is meant for fast multi-queue devices.
  NVMe benchmarks: `none` best overall ([Phoronix, 5.6](https://www.phoronix.com/review/linux-56-nvme)).
- Zoned (host-managed SMR) devices: since 6.10 zone write plugging lives in the block layer, so
  `none` is also safe there; `mq-deadline` stays the conservative pick.

### Read-ahead

- Device read-ahead (6.12): `max(2 × io_opt, 128 KiB)` ([blk-settings.c v6.12](https://github.com/torvalds/linux/blob/v6.12/block/blk-settings.c)).
  md RAID5/6 sets `io_opt = chunk × data disks`, so the md device already gets two full stripes
  ([raid5.c](https://github.com/torvalds/linux/blob/v6.12/drivers/md/raid5.c)); dm-crypt
  inherits `io_opt` from md.
- Since **6.17** a rotational disk without `io_opt` (every ATA HDD) gets `2 × max_sectors`
  (≈2.5 MiB) and the kernel "never decreases" a value raised by the user
  ([commit 459779d04ae8](https://github.com/torvalds/linux/commit/459779d04ae8)). A udev rule
  writing 2048 KiB therefore **lowers** the default on 6.17+.
- **btrfs ignores the device read-ahead for file data**: it reads through its own bdi with
  `ra = max(128 KiB × number of devices, 4 MiB)` ([disk-io.c](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/disk-io.c)),
  tunable at `/sys/fs/btrfs/<UUID>/bdi/read_ahead_kb` ([sysfs.c](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/sysfs.c)).
  md resync/scrub issues its own large I/O and does not use read-ahead either. In rynas' stack
  (md → LUKS → btrfs, or btrfs on a disk) the per-disk `read_ahead_kb` has no effect.

### Write cache and flushes

- btrfs relies on FLUSH/FUA (barriers on by default; `nobarrier` "will most certainly lead to a
  corrupted filesystem" on power loss) ([btrfs mount options](https://btrfs.readthedocs.io/en/latest/ch-mount-options.html));
  md and dm-crypt pass flushes through. So the volatile cache stays **on**, with or without a
  UPS; the UPS protects against the md RAID5/6 write hole and lost dirty page cache, not the
  drive cache. Real risk: USB bridges that drop FLUSH.

### Power: APM, standby, EPC, head parking

- `hdparm -B`: 1-127 permit spin-down, 128-254 do not, 255 disables APM; low values also make
  many drives unload heads aggressively. NAS drives often lack APM. Some drives only honour
  `-S` with APM ≤ 127, many ignore or reset `-S`, and drivetemp/smartd polling can reset the
  idle timer ([drivetemp.rst](https://docs.kernel.org/hwmon/drivetemp.html)).
- Seagate IronWolf/Exos use **EPC** (idle_a, idle_b = heads unloaded, idle_c = reduced rpm,
  standby_z = spun down); inspect with `openSeaChest_PowerControl --showEPCSettings`, disable
  idle_b/idle_c on 24/7 arrays if the load-cycle count climbs ([TrueNAS forum](https://forums.truenas.com/t/seagate-drives-power-transition-issue/61677),
  [man page](https://manpages.ubuntu.com/manpages/oracular/en/man8/openSeaChest_PowerControl.8.html)).
  WD Green/Blue/older Red: idle3 timer (8 s) parks heads, fix with `idle3ctl` (Debian `idle3-tools`).
  Debian 13 ships `hdparm` 9.65, `hd-idle` 1.21, `openseachest` 24.08, `idle3-tools`.
- Rated load/unload cycles: 600,000 (WD Red Plus, IronWolf) ([IronWolf datasheet](https://www.seagate.com/www-content/datasheets/pdfs/ironwolf-16tb-DS1904-19-2110US-en_US.pdf)).
  600k over a 5-year life ≈ 13 per power-on hour: a drive above ~10/h is parking too often.
- Spin-down pays only on disks idle for hours: a 3.5" HDD idles at ~4-5 W vs <1 W in standby
  (4 disks ≈ 140 kWh/year); each spin-up costs ~10 s latency and a start/stop cycle. With SMB
  browsing, Docker, logs or metrics on the array the disks wake many times a day, which costs
  more wear than it saves. Backblaze never spins down.

### Error recovery (ERC/TLER)

- Without ERC a drive retries a bad sector for up to ~2 min, the kernel SCSI timeout is 30 s,
  md kicks a healthy disk. Fix: `smartctl -l scterc,70,70` (7 s, NAS drives default), else
  `/sys/block/sdX/device/timeout = 180`; both reset at power cycle. SMR drives can stall >10 min:
  avoid SMR in RAID ([Linux RAID wiki, Timeout Mismatch](https://archive.kernel.org/oldwiki/raid.wiki.kernel.org/index.php/Timeout_Mismatch.html)).

### NVMe

- APST on by default, `nvme_core.default_ps_max_latency_us=100000`
  ([core.c](https://github.com/torvalds/linux/blob/master/drivers/nvme/host/core.c)); the
  kernel already quirks known-bad models (`NVME_QUIRK_NO_APST`, `NO_DEEPEST_PS`).
- `nr_requests` (1023) and the queue count are per hardware queue: leave them. Volatile write
  cache (feature 0x06) stays on; drives with power-loss protection report none.
- The nvme hwmon exposes **WCTEMP as `temp1_max` and CCTEMP as `temp1_crit`**
  ([hwmon.c](https://github.com/torvalds/linux/blob/master/drivers/nvme/host/hwmon.c)); typical
  WCTEMP 70-85 °C, CCTEMP 80-90 °C; the drive throttles above WCTEMP.

### TRIM

- btrfs `discard=async` is the default since 6.2 when every layer supports discard
  ([btrfs mount options](https://btrfs.readthedocs.io/en/latest/ch-mount-options.html)).
  Debian enables `fstrim.timer` on new installs (util-linux ≥ 2.35.1-2, [README.Debian](https://sources.debian.org/src/util-linux/2.41-5/debian/util-linux.README.Debian)).
  Running both is harmless: "fstrim.timer will do nothing if everything has been handled by
  discard=async" (Fedora/btrfs maintainers, [RH bug 2182228](https://bugzilla.redhat.com/show_bug.cgi?id=2182228)).
- md RAID0/1/10 pass discard; RAID4/5/6 only with `raid456.devices_handle_discard_safely=Y`
  (only for drives that return zeroes after TRIM). dm-crypt only with `allow-discards`
  (leaks free-space layout). Over-provisioning matters when TRIM cannot reach the SSD
  (parity RAID, LUKS without discard): leave 10-20% unpartitioned on new SSDs.

### Temperature

- HDD: Google 2007 found little correlation in the normal range, more failures below ~25 °C on
  young drives ([Pinheiro et al., FAST '07](https://www.usenix.org/legacy/events/fast07/tech/full_papers/pinheiro/pinheiro.pdf));
  Backblaze 2014 no fleet-wide effect ([blog](https://www.backblaze.com/blog/hard-drive-temperature-does-it-matter/));
  a 2025 reanalysis of 443k Backblaze drives estimates +2.1% failures per °C (≈+23% per 10 °C)
  ([summary](https://www.techspot.com/news/113499-new-study-finds-hgst-wd-hard-drives-fail.html)).
  Vendors: IronWolf "sustained case temperatures above 60 °C" not recommended; WD Red Plus
  0-65 °C operating.
- SATA SSD: operating range typically 0-70 °C. NVMe: use WCTEMP/CCTEMP.
- drivetemp exposes SCT limits as `temp1_max` (max recommended continuous) and `temp1_crit`.

### md

- `stripe_cache_size`: default 256, range 17-32768, RAM = entries × 4 KiB × members
  ([md.rst](https://docs.kernel.org/admin-guide/md.html)); 4096 is what DSM ships.
  `group_thread_cnt` (RAID5 worker threads, default 0) helps flash arrays only.
- Resync limits: 1000 / 200000 KB/s system-wide (`dev.raid.speed_limit_min/max`), per array
  `md/sync_speed_min/max` ([md.c](https://github.com/torvalds/linux/blob/master/drivers/md/md.c)).
  200 MB/s caps NVMe/SSD rebuilds.
- Write-intent bitmap: mdadm 4.4 (Debian 13) **no longer adds one automatically**, it only
  reserves space; internal bitmap chunk default 64 MiB ([mdadm(8) trixie](https://manpages.debian.org/trixie/mdadm/mdadm.8.en.html)).
  Lockless bitmap `llbitmap` arrived in 6.18. PPL (RAID5 write-hole fix) excludes a bitmap.

### dm-crypt

- `--perf-no_read_workqueue --perf-no_write_workqueue` (cryptsetup ≥ 2.3.4) help SSD/NVMe
  latency, mixed results, no HDD benefit ([cryptsetup #862](https://gitlab.com/cryptsetup/cryptsetup/-/issues/862)).
  LUKS2 4096-byte sectors (cryptsetup ≥ 2.4 picks them automatically) lower CPU cost.

### Dirty memory

- Kernel defaults `dirty_background_ratio=10`, `dirty_ratio=20`: with 32 GB RAM up to 6 GB of
  dirty data, i.e. a minute of writeback at 1 GbE speed. Block-layer writeback throttling
  (`wbt_lat_usec`, 75 ms on HDD) softens it. Byte limits (256 MiB / 1 GiB) are a reasonable
  option, moderate evidence: offer, don't check.

## 3. Recommendations per disk type

### 7200/5400 rpm CMR HDD

| Setting | Recommended | Why | Source |
|---|---|---|---|
| Scheduler | `mq-deadline` (kernel default); `none` acceptable | read priority and merging with negligible CPU; bfq/kyber do not fit | elevator.c, bfq-iosched.rst |
| Scheduler on md/dm device | n/a (bio-based) | only members schedule | — |
| Read-ahead | kernel default; **n/a** under btrfs or md | btrfs uses its own 4 MiB bdi read-ahead; 6.17+ already sets ~2.5 MiB | disk-io.c, commit 459779d04ae8 |
| Write cache | on | btrfs flushes make it safe; off costs a lot of write speed | btrfs docs |
| APM | **254 or off (255), or unsupported** | no drive-initiated spin-down or aggressive head unloads; ≤127 only together with a chosen spin-down on drives that need it | TrueNAS, OMV, DSM |
| Standby | **off** by default; opt-in ≥ 30 min (60 min suggested) for archive/backup disks | wear and wake-ups outweigh the watts on busy volumes | TrueNAS/Unraid defaults |
| Spin-down method | host-driven (idle in `/proc/diskstats`, then STANDBY IMMEDIATE / START STOP UNIT, like hd-idle) | works when the drive ignores `-S` and through USB bridges | DSM `standby_force`, hd-idle |
| EPC / idle3 | show EPC timers; warn if load-cycle rate > 10/h or count > 300k | head parking wears drives; 600k rated | IronWolf datasheet |
| ERC | `scterc 7.0 s` for RAID members; if unsupported, kernel timeout 180 s | avoids md dropping a healthy disk | Linux RAID wiki |
| Temperature | ok < 45, **warn ≥ 50, crit ≥ 60 °C** (lower if the drive's `temp1_max`/`temp1_crit` are lower) | DSM shuts down at 61, Seagate: not above 60 sustained; Unraid 45/55 | scemd.xml, datasheets |

### SMR HDD (drive-managed)

As CMR, plus: warn when an SMR drive is (or would become) an md RAID member (rebuilds of days,
stalls > 10 min, timeouts). Detection: `queue/zoned` = host-aware/host-managed, else a model
list (e.g. WD Red EFAX 2-6 TB). md does not support host-managed zoned devices; btrfs zoned
mode does (single disk only).

### SATA SSD

| Setting | Recommended | Why | Source |
|---|---|---|---|
| Scheduler | `mq-deadline` or `none` (both ok) | single NCQ queue; difference is noise | elevator.c |
| Read-ahead | kernel default; n/a under btrfs/md | as HDD | disk-io.c |
| Write cache | on | same as HDD | btrfs docs |
| ALPM | `med_power_with_dipm` | ~1 W saved, no measurable cost | current rule |
| TRIM | `discard=async` (btrfs default) and/or `fstrim.timer`: both = ok | fstrim is a no-op after async discard | RH 2182228 |
| TRIM blocked by stack | parity RAID or LUKS without `allow-discards` → n/a, suggest over-provisioning | TRIM cannot reach the device | md.rst |
| ERC | as HDD when the SSD reports SCT ERC | same failure mode, rarer | Linux RAID wiki |
| Temperature | ok < 50, **warn ≥ 60, crit ≥ 70 °C** (or drive limits) | operating max is usually 70 | Unraid, datasheets |

### NVMe

| Setting | Recommended | Why | Source |
|---|---|---|---|
| Scheduler | `none` | multi-queue, any scheduler adds overhead | Phoronix, elevator.c |
| Read-ahead | kernel default; n/a under btrfs | — | disk-io.c |
| `nr_requests` / queues | default | per-queue depth already large | — |
| Write cache | on (or not reported = PLP) | flushes are honoured | — |
| APST | enabled (`default_ps_max_latency_us` > 0) | kernel quirks bad models; disable per model only | core.c |
| TRIM | as SATA SSD | — | — |
| Temperature | **warn ≥ WCTEMP (`temp1_max`), crit ≥ CCTEMP (`temp1_crit`)**; fallback 70 / 80 °C; also report thermal-throttle counters (SMART "Warning/Critical Comp. Temperature Time") | the drive's own throttling point | hwmon.c, Unraid, DSM M.2 70 °C |
| dm-crypt | optional `no_read/write_workqueue` | latency gain on fast devices, mixed results | cryptsetup #862 |

### USB-attached disks

| Setting | Recommended | Why |
|---|---|---|
| Driver | prefer `uas`; note `usb-storage` | queueing and TRIM need UAS |
| Type detection | do not trust `queue/rotational` (bridges often report 1 for SSDs); use SMART rotation rate via `-d sat` | wrong type → wrong recommendations |
| Scheduler | `mq-deadline` (single queue) | — |
| APM / `-S` | often not passed by the bridge; use host-driven spin-down; many bridges spin down by themselves (DSM: 10 min for USB) | — |
| Write cache | on, but warn against USB disks as md members (bridges that drop FLUSH, resets) | btrfs corruption after power loss |
| Temperature | per underlying type, often unreadable → Unknown, not an error | — |

### md arrays

| Setting | Recommended | Why | Source |
|---|---|---|---|
| `stripe_cache_size` (RAID4/5/6) | 4096 on HDD arrays when RAM allows (current 2%-of-RAM proposal is fine) | much faster sequential writes; DSM uses 4096 | md.rst, DSM |
| `group_thread_cnt` | 0 on HDD; 2-4 on SSD/NVMe RAID5/6 | parity work on several CPUs | md.rst |
| `sync_speed_max` | default on HDD; raise (e.g. 2000000) on SSD/NVMe arrays | 200 MB/s caps flash rebuilds | md.c |
| `sync_speed_min` | default 1000 | md backs off for user I/O | md.c |
| Bitmap | internal, chunk ≥ 64 MiB, pass `--bitmap=internal` explicitly (mdadm 4.4 no longer adds it) | resync after a crash in minutes instead of hours | mdadm(8) |

## 4. Changes for tuning.rs

Concrete differences from the current rules (and the per-type udev defaults in `domain.rs`):

1. **APM check is inverted.** Today `apm <= 127` is Ok and `> 127` Differs. Recommend
   "254 or off": Ok for 128-255, disabled or unsupported; Differs for ≤ 127 unless the disk has
   a spin-down override. Update the label/note ("levels ≤ 127 let the drive park heads and spin
   down by itself").
2. **Read-ahead check → NotApplicable** when the disk is an md member or holds btrfs (btrfs
   uses its own bdi read-ahead of ≥ 4 MiB). Optionally move the check to the volume
   (`/sys/fs/btrfs/<UUID>/bdi/read_ahead_kb`, recommend ≥ 4096). For other filesystems keep a
   loose range.
3. **Stop writing `read_ahead_kb` in the HDD udev default** (`type_defaults`: 2048). On 6.17+
   it lowers the kernel's own ~2.5 MiB value and it has no effect under btrfs. Drop it for SSD
   (256) and NVMe (128) too, or write it only as a per-disk override.
4. **SATA SSD scheduler: accept `none` and `mq-deadline`** as Ok (today only `mq-deadline`).
5. **Temperature thresholds** (`smart::temp_limits`): HDD 55/65 → **50/60**; SSD 70/80 →
   **60/70**; NVMe 75/85 → **WCTEMP/CCTEMP from hwmon `temp1_max`/`temp1_crit`**, fallback
   70/80. For ATA, use drivetemp `temp1_max`/`temp1_crit` when lower than the table. Add a
   crit status (today only "below warn").
6. **TRIM: online discard + fstrim.timer → Ok** (info: "async discard does the work, the
   timer is a cheap safety net"), not Differs.
7. **TRIM blocked by LUKS**: also NotApplicable (with an over-provisioning note) when the
   top device has `discard_max_bytes` = 0 because crypttab lacks `discard`; today only parity
   RAID is handled. Read `discard_max_bytes` of the top md/dm device, not only the disk.
8. **New check: ERC/TLER** for HDD/SSD md members: `smartctl -l scterc` = 70,70 → Ok; drive
   without SCT ERC and `device/timeout` < 180 → Differs ("a slow sector read can make the
   array drop this disk"). Apply later via udev (`smartctl -l scterc,70,70` or timeout 180).
9. **New check: head parking** for HDD: SMART 193 Load_Cycle_Count / 9 Power_On_Hours > 10/h
   or count > 300k → Differs, with a note pointing to APM/EPC/idle3.
10. **New check: standby** (informational): show the configured spin-down; recommended "off"
    for array members; note when the drive is a known `-S` ignorer → host-driven spin-down.
11. **New warning: SMR drive in md RAID** (zoned attribute or model list).
12. **USB transport**: skip APM/standby checks that need ATA passthrough when SAT is not
    available; show "uas"/"usb-storage"; do not classify by `rotational` alone.
13. **Array checks** (volume page, outside tuning.rs): `group_thread_cnt` for flash RAID5/6,
    `sync_speed_max` for flash arrays, bitmap present (`md/bitmap/location` ≠ none) on
    redundant arrays; create arrays with `--bitmap=internal`.
14. Unchanged and confirmed: NVMe `none`; HDD `mq-deadline`; write cache on; ALPM
    `med_power_with_dipm` for SATA SSD; APST enabled; stripe cache proposal.
