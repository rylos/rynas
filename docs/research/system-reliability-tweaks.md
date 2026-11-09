# rynas: system reliability. Configuration backup, logs, flash, zram and tweaks

**Summary.** Treat the system disk as expendable. What matters is the configuration: the user reinstalls Debian, restores a rynas archive and is back up. This is the TrueNAS and Unraid model.

Logs and flash need an automatic profile for the system disk, with settings applied as drop-ins, never by editing package files. Automatic changes are few and reversible; everything else is proposed with an explanation. Many of the network and Samba tweaks circulating online are myths.

On synodeb one detail changes the priorities. The 128 MB DOM is only used for boot; the root lives on a disk in a bay (HDD or NVMe). **A root on an HDD never spins down if logs keep writing to it.** So the "logs in RAM" problem concerns HDDs too, not just USB sticks.

---

## 1. Configuration backup and restore

### How others do it

| Product | Format | Automatic | History and diff | Secrets | Lesson for rynas |
|---|---|---|---|---|---|
| **DSM** | `.dss` | yes, to Synology Account after **every change** in Control Panel; removed after 180 days without uploads | no (effectively the latest copy per NAS) | included | backup triggered by changes, not by a schedule ([KB](https://www.synology.com/en-us/knowledgebase/tutorials/511)) |
| **TrueNAS** | `.db` (+ `.tar` with the **secret seed**) | no; scheduling requires TrueCommand. Offers a save before every update | no | encrypted with the seed: without the seed, SMB passwords, ZFS keys and cloud/SSH credentials are lost | **the scenario not to repeat**: single archive, secrets encrypted with a passphrase ([doc](https://www.truenas.com/docs/scale/systemsettings/advanced/managesysconfig), [25.04](https://www.truenas.com/docs/scale/25.04/scaleuireference/systemsettings/updatescreensscale/)) |
| **OMV** | no official export | `openmediavault-backup` plugin (fsarchiver, dd, borg, rsync of the root); community `omv-regen` | no | — | imaging the root is clumsy; nobody has a proper configuration export |
| **Unraid** | `config/` folder on the USB stick | Connect Flash Backup to the cloud, configuration only, files under 10 MB | no | **excludes** passwords and WireGuard keys until encryption is available | separate configuration from binaries and logs ([doc](https://docs.unraid.net/unraid-connect/automated-flash-backup)) |
| **OPNsense** | `config.xml` | yes, one revision per change, configurable count | **yes: unified diff between any two revisions**, download, restore | optional export encryption | **the model to copy** ([doc](https://docs.opnsense.org/manual/backups.html)) |
| **Proxmox** | `/etc/pve` (pmxcfs `config.db`) | no official export; host backup with `proxmox-backup-client` | no | — | — |

### Proposal for rynas

**A. Revisions (phase 1, changesets already exist).** Every apply creates a revision with author, time, comment and diff. In the UI, *System → Configuration → History* offers two views:

- a **semantic** diff per key (e.g. `smb.share[media].read_only: false → true`);
- a diff of the **generated files** (`smb.conf`, `exports`…).

"Restore this revision" creates a new pending changeset: review the diff, then apply; history is never rewritten. Default retention: 200 revisions or 365 days.

**B. Export archive `rynas-<host>-<date>.rynasbak`** (tar + zstd):

- `manifest.json`: rynas version, DB schema version, plugins and their versions, hostname, hardware fingerprint (model, MACs, disk serials);
- `config.db`, taken with `VACUUM INTO` or the SQLite backup API: never copy the file while the WAL is active;
- `secrets.age`: Samba hashes (`pdbedit -e tdbsam:`), host TLS/SSH keys, TOTP secret, notification tokens, key files of encrypted volumes. Encrypted with **age**/scrypt (`age` crate) or XChaCha20-Poly1305 + Argon2id, with a passphrase. **Without a passphrase the export contains no secrets**, and the UI says so clearly, unlike TrueNAS;
- `system/`: NAS users and groups (UID ≥ 1000), `mdadm.conf`, `crypttab`, apt sources, `dpkg --get-selections` of the plugins;
- generated files are **not** needed, because they are regenerated from the DB. Only their hashes are stored, for anti-drift.

**C. Restore**, in two modes:

- **same machine**: everything is applied;
- **hardware migration**: a wizard that remaps network interfaces by role (not by MAC), matches volumes by UUID and skips hardware plugins that are absent (synology on a mini-PC).

Always through pending → diff → apply. DB schema migration runs forward with refinery.

**D. Scheduled backup (phase 2):**

- trigger **after every apply** (10 min debounce) plus a daily one if there were changes;
- destination `/<vol>/.rynas/config-backups/` (with retention), plus remote destinations from the `backup` plugin (SMB, SFTP, rclone, restic);
- forced export **before `apt upgrade`** of rynas, via a `DPkg::Pre-Invoke` hook (as TrueNAS does);
- notification if the backup fails or none has run for X days.

**E. Full system backup (phase 3, optional).**

- If the root is btrfs: snapshots before updates (snapper with apt hook), like TrueNAS boot environments. With synodeb's DOM boot, however, **booting from a snapshot is not trivial**: the snapshot serves for a rollback from a live or rescue system, not from GRUB.
- If the root is ext4: an image with `partclone`/`fsarchiver` on the data volume (OMV approach), proposed, not automatic.
- Recommendation: **reinstall + archive import** as the official path, the image as an extra.

---

## 2. Log management

**The facts on Debian.** Since bookworm rsyslog is no longer installed by default and only journald is used ([release notes 12](https://www.debian.org/releases/bookworm/amd64/release-notes.en.txt)). journald defaults (man `journald.conf`):

| Parameter | Default |
|---|---|
| `Storage` | `auto` (persistent if `/var/log/journal` exists) |
| `SystemMaxUse` | 10% of the filesystem, at most 4 GiB |
| `RuntimeMaxUse` | 10% of RAM, at most 4 GiB |
| `SyncIntervalSec` | 5 min (CRIT messages are written immediately) |
| `Compress` | yes |
| `RateLimitIntervalSec` / `RateLimitBurst` | 30 s / 10000 |

**Proposal, in layers:**

1. **Drop-in `/etc/systemd/journald.conf.d/50-rynas.conf`** generated from the profile:
   - normal disk: `SystemMaxUse=500M`, `MaxRetentionSec=3month`;
   - flash disk or HDD with spin-down: see §3.
2. **rynas audit** (logins, applies, file actions, sudo from the UI) in an SQLite `audit` table, plus a copy in the journal with structured fields (`RYNAS_USER=`, `RYNAS_ACTION=`). This is the equivalent of what DSM shows in Log Center.
3. **SMB audit**: `vfs_full_audit` per share, optional, only for modifying operations (`full_audit:success = mkdirat renameat unlinkat pwrite`, `facility = local5`), into the journal. On busy shares it produces a lot of logs: **disabled by default** and with a warning.
4. **UI viewer** (already planned): journal via `sd-journal` or `journalctl -o json --follow` over WebSocket, with filters by unit, priority, time range and text, plus export to `.txt` or `.json`.
5. **Remote forwarding (phase 2):**
   - RFC 5424 syslog via `systemd-journal-upload` (journal format) or rsyslog `omfwd` TCP/TLS. DSM Log Center, Graylog and Loki (via promtail/alloy) can receive it;
   - rynas **as a syslog server** for other devices: phase 3, with an rsyslog plugin (`imtcp`) writing to the data volume.
6. **From errors to notifications:** rules on `MESSAGE`/`PRIORITY<=3` and failed units (synodeb already notifies about failed services), kernel I/O errors, md degraded, btrfs `csum error` events. With rate limiting and grouping.

---

## 3. Small system disk, flash, or HDD that should sleep

| Tweak | Evidence | rynas |
|---|---|---|
| `/tmp` on tmpfs | **default in trixie**, at most 50% of RAM; `/tmp` cleaned after 10 days, `/var/tmp` after 30 ([release notes 13](https://www.debian.org/releases/trixie/release-notes.en.txt)) | show the status; on 4 GB propose `size=1G` via `systemctl edit tmp.mount` |
| volatile journal or "logs in RAM" | real: it is the main source of continuous writes | flash/spin-down profile: `Storage=volatile`, `RuntimeMaxUse=64M` (4 GB) or `128M`, **plus** periodic flush (below) |
| log2ram / folder2ram (OMV `openmediavault-flashmemory`) | proven: tmpfs with rsync to disk at shutdown and daily ([folder2ram](https://github.com/semool/folder2ram)) | **don't reinvent it**: propose `log2ram` (not in Debian, external repo) or an equivalent rynas unit doing `journalctl --flush`/rsync of `/var/log` hourly **and** at shutdown |
| rynas's own writes (metrics RRD, SQLite WAL) | **critical, and our fault**: an RRD written every 10 s on flash is enemy number one | RRD in RAM, flushed every 30 min and at shutdown (proxmox-rrd model); SQLite `journal_mode=WAL`, `synchronous=NORMAL`, no writes without changes |
| `noatime` | useful on **btrfs with snapshots** (atime updates create CoW metadata), marginal on ext4 (`relatime` is already the default) | propose `noatime` on a btrfs root and on btrfs volumes |
| `lazytime`, `commit=60` | small gain, risks more data loss on a crash | don't propose |
| no swap on USB stick/SD | real | detect swap on USB/SD and propose zram instead; on a real SSD swap is fine |
| fstrim | real for SSD/NVMe | see §6 |
| coredump | `systemd-coredump` is not installed by default | if present: `Storage=none` on the flash profile |
| smartd waking the disks | real | `DEVICESCAN -n standby,15,q` (skip the check if the disk is asleep, at most 15 times) |
| write monitoring | real: `/sys/block/*/stat` (field 7 = sectors written), TBW from SMART | "system disk writes per day" widget plus remaining-life estimate |

**Risk.** With a volatile journal, a crash or power loss loses the logs, exactly when they are needed most. Mitigations:

- `ForwardToSyslog`/remote upload if configured;
- hourly flush;
- `MaxLevelStore` unchanged, but `PRIORITY<=2` messages copied to disk immediately;
- `kernel.panic` + pstore/ramoops, if the platform supports it.

---

## 4. zram and memory

| Option | Notes | Choice |
|---|---|---|
| `systemd-zram-generator` (Debian) | upstream, config in `/etc/systemd/zram-generator.conf.d/`; default `min(ram/2, 4096)` ([repo](https://github.com/systemd/zram-generator), [example](https://github.com/systemd/zram-generator/blob/main/zram-generator.conf.example)) | **yes** |
| `zram-tools` (`zramswap`) | script, `/etc/default/zramswap` | no |
| zswap | compressed cache **in front of** on-disk swap; only makes sense with swap on SSD | alternative if swap on SSD already exists; **never together with zram** |

Drop-in `/etc/systemd/zram-generator.conf.d/50-rynas.conf`:

```ini
[zram0]
zram-size = min(ram / 2, 8192)
compression-algorithm = zstd      # lz4 if the CPU is weak or minimum latency is wanted
swap-priority = 100               # higher than any on-disk swap
```

and `/etc/sysctl.d/60-rynas-zram.conf`: `vm.swappiness = 150` (kernel documentation allows up to 200 for in-memory swap; Pop!_OS uses 180) and `vm.page-cluster = 0` (no readahead on zram).

- **zstd or lz4:** zstd compresses about 3:1 versus about 2:1, lz4 decompresses 2-3 times faster. On J4125 and R1600 zstd is acceptable because swapping on a NAS is rare. Combinations of compression algorithms are not documented for every kernel: to be verified during implementation.
- **OOM:** with 4 GB, `systemd-oomd` (separate package, requires swap and PSI) or `earlyoom`. Propose oomd only with 4-8 GB and Docker active.
- **ZFS ARC:** since OpenZFS 2.3 the Linux default is `max(RAM−1 GiB, 5/8 RAM)`, no longer 50% ([analysis](https://blog.thalheim.io/2025/10/17/zfs-ate-my-ram-understanding-the-arc-cache/)). With 4-8 GB and Docker, propose an explicit `zfs_arc_max` (e.g. 25-40% of RAM) and show the ARC on the dashboard.

---

## 5. Other tweaks: evidence or myth

| Tweak | Verdict | Detail |
|---|---|---|
| **`stripe_cache_size` md RAID5/6** | **strong evidence** | default 256, much faster sequential writes with 4096-8192. RAM used = value × 4 KiB × number of disks (8192 on 4 disks = 128 MiB). udev rule or unit per array; avoid on 4 GB |
| readahead on md and HDD | evidence for sequential reads | `blockdev --setra 4096` (2 MiB) on the md array; useful for streaming |
| `vm.dirty_bytes`/`dirty_background_bytes` | moderate evidence | with 16-32 GB, 20% dirty causes multi-GB stalls; propose `dirty_background_bytes=256M`, `dirty_bytes=1G` |
| I/O scheduler | see §6 | Debian's udev defaults are already good |
| TCP buffers for 10 GbE | **mostly a myth on the LAN**: BDP = 10 Gbit × 0.2 ms ≈ 250 KB, autotuning (max 6 MiB) is enough | only makes sense for WAN or remote replication |
| BBR | **myth on the LAN**, useful on lossy WANs | leave it alone |
| irqbalance | useful with multi-queue 10 GbE NICs | propose it if a NIC has more than 1 queue |
| CPU governor | `schedutil`/`powersave` with amd-pstate or intel_pstate is fine | `performance` is a waste on a NAS |
| tuned | overkill for us | no |
| Samba `socket options` | **myth**: the Samba docs say not to use it | never |
| Samba `aio read/write size` | already active by default | no |
| Samba multichannel | default `yes` since Samba 4.15 | show it, no tweak |
| `vfs_io_uring`, `use sendfile` | small gain | propose sendfile |
| btrfs `compress=zstd:1..3` | evidence: fewer writes and more space, low CPU cost | default `zstd:1` on new volumes |
| btrfs `compress-force` | risky on already-compressed content | no |
| btrfs `space_cache=v2` | already the default (free-space-tree in mkfs) | no tweak |
| btrfs `autodefrag` | harmful with VMs or DBs on SSD | no; see §6 |
| NFS threads | default 8 in `/etc/nfs.conf` | 16-32 with many clients |
| `mitigations=off`, `nobarrier`, cron `drop_caches`, `swappiness=0` | **myths or dangerous** | never |

---

## 6. Settings per disk type

| | HDD | SATA SSD | NVMe |
|---|---|---|---|
| **Scheduler** | `mq-deadline` (Debian default); `bfq` only for desktop interactivity, not throughput | `mq-deadline` | `none` (default); `kyber` niche |
| **readahead** | 1-4 MiB for streaming and RAID | 128-512 KiB | 128 KiB (default) |
| **nr_requests / NCQ** | default 64-256, NCQ 32: don't touch | default | default (multiple queues) |
| **write cache** (`hdparm -W`) | **leave it on**: flushes make it safe | on | on (`nvme get-feature -f 6`) |
| **TRIM** | — | weekly `fstrim.timer` (util-linux, check that it is enabled) | same |
| **btrfs** | `autodefrag` acceptable only for small randomly written files; `nossd` automatic | `ssd` detected automatically; `discard=async` is the **default since 6.2**, so it overlaps with fstrim.timer: keep only one ([Oracle](https://blogs.oracle.com/linux/post/btrfs-discard), [RH bug](https://bugzilla.redhat.com/show_bug.cgi?id=2182228)) | same |
| **TRIM on md** | — | RAID0/1/10 pass it through; **RAID5/6 do not**, unless `raid456.devices_handle_discard_safely=1` (**never** by default) | same |
| **TRIM on LUKS** | — | `discard` in `crypttab`: reveals free blocks; explicit choice | same |
| **Power** | `hdparm -B 127` (APM: allows spin-down), `-S 241` (30 min); many WD Red/IronWolf **ignore `-S`** → `hd-idle`. WD Green/Blue: idle3 (`idle3ctl`) parks heads every 8 s → raise it | SATA ALPM `med_power_with_dipm` | APST on; `nvme_core.default_ps_max_latency_us=0` **only** for known faulty models; ASPM from BIOS |
| **Temperature** | threshold 50-55 °C | 70 °C | throttling at 70-80 °C: show Composite and the sensors, warn on throttling |
| **SMART** | 5, 187, 188, 197, 198, 199 (CRC = cable), 193 Load_Cycle (limit 300-600k) | Percentage Used / Wear_Leveling (177/231/233, vendor-specific), TBW | Percentage Used, Available Spare, Media Errors, Critical Warning |
| **Over-provisioning** | — | 10% unpartitioned on heavily written consumer SSDs (cache, ZIL); weak evidence on modern drives | same |

**What wakes HDDs:**

- root or logs on that disk;
- atime;
- smartd without `-n standby`;
- badly done `hdparm -C`;
- `udisks2` polling;
- Docker or its data-root on the HDD volume;
- metrics that read SMART;
- indexers;
- scrub and mlocate cron jobs.

synodeb already handles staggered spin-up.

**SSD cache in front of HDDs.**

- btrfs does no tiering; bcachefs has left mainline and depends on DKMS: avoid.
- **lvmcache** (dm-cache, `writethrough`) for read caching, or `dm-writecache` for writes. This is the professional route and resembles DSM's SSD cache.
- bcache works but is poorly maintained.
- Writeback requires mirrored SSDs, just as DSM requires two SSDs for a read/write cache.
- Phase 3, LVM only.

**udev (`/usr/lib/udev/rules.d/60-rynas-disk.rules`, generated from the DB with per-disk overrides):**

```
ACTION=="add|change", KERNEL=="sd[a-z]", ATTR{queue/rotational}=="1", ATTR{queue/read_ahead_kb}="2048", RUN+="/usr/lib/rynas/hdd-power %k"
ACTION=="add|change", KERNEL=="sd[a-z]", ATTR{queue/rotational}=="0", ATTR{queue/scheduler}="mq-deadline"
ACTION=="add|change", KERNEL=="nvme[0-9]*n[0-9]", ATTR{queue/scheduler}="none"
ACTION=="add|change", KERNEL=="md[0-9]*", ATTR{md/stripe_cache_size}="%rynas_value"
```

**Per-disk UI:** detected type (HDD/SSD/NVMe); for each option the current value / type default / override, its impact in one line, and a "restore default" button.

---

## 7. System → Maintenance module

**Automatic profiles** (detected with `lsblk -o ROTA,TRAN,SIZE`, `/proc/meminfo`, `ethtool`):

- **Flash/small:** root on USB, SD or eMMC, or under 32 GB;
- **HDD-root:** root on a rotating disk, so spin-down is impossible without logs in RAM;
- **Standard:** root on SSD or NVMe;
- **RAM ≤ 4 GB / 8 GB / ≥16 GB.**

| When | What |
|---|---|
| **automatic at install** (drop-ins, removed on purge) | journal limit (`SystemMaxUse=500M`); rynas RRD and SQLite with minimal writes; smartd `-n standby`; udev with per-type defaults, matching Debian's |
| **proposed** (toggle with explanation) | volatile journal + hourly flush; noatime on btrfs; zram, in front of any swap on disk (see D-ZRAM); `stripe_cache_size`; `dirty_bytes`; HDD spin-down/hd-idle; idle3; choice of discard or fstrim; irqbalance; `zfs_arc_max`; oomd; SMB audit; remote syslog |
| **never** | `nobarrier`, write cache disabled, `mitigations=off`, `devices_handle_discard_safely`, compress-force on existing volumes, autodefrag on SSD, TCP/BBR tweaks on the LAN, Samba `socket options`, `drop_caches` |

**Mockup:**

```
System › Maintenance                      Profile: HDD-root · 8 GB · 4×HDD RAID5 · 1 GbE
┌─ Config backup ──────────────────────────────────────────────────────────────┐
│ Last: 2026-11-05 10:12 (after apply #148) ✓   Destination: /vol1/.rynas + SFTP │
│ [Export now…]  [Import…]  [History & diff ›]   Secret passphrase: set ✓       │
├─ Logs ──────────────────────────────────────────────────────────────────────┤
│ Journal: persistent, 812 MB / 500 MB limit ⚠   [ ] Logs in RAM (flush 1 h)    │
│   ↳ keeps the system disk spinning; RAM mode → lose ≤1 h on a crash           │
├─ Memory ────────────────────────────────────────────────────────────────────┤
│ zram: 4 GB zstd, used 210 MB (ratio 3.1)  ✓ auto     swappiness 150          │
├─ Disks ─────────────────────────────────────────────────────────────────────┤
│ sda HDD  sched mq-deadline  ra 2 MiB  spin-down 30 min [hd-idle]  wakeups 3/d │
│ nvme0 NVMe sched none  APST on  wear 4%  writes 18 GB/day  fstrim weekly ✓    │
│ md0 RAID5 stripe_cache 256 → [suggest 4096, +64 MiB RAM]                      │
└─────────────────────────────────────────────────────── [Apply pending (2)] ──┘
```

Every toggle goes through pending, diff and apply, and ends up in the revisions.

---

## 8. Phases

| Phase | Content |
|---|---|
| **1** | revisions with diff and restore via pending; manual export/import of the archive with age-encrypted secrets; safe automatic drop-ins; minimal-write RRD and SQLite; journal viewer; audit table; **read-only** Maintenance panel (profile, status, writes per day, SMART per type) |
| **2** | automatic backup after every apply and before upgrades, to the volume and remote destinations; hardware migration wizard; proposed toggles (logs in RAM, zram, noatime, spin-down, stripe cache, dirty, discard/fstrim); per-disk udev with overrides; remote syslog/Loki; notifications from journal errors |
| **3** | btrfs root snapshots before upgrades / partclone image; lvmcache/dm-writecache; rynas as a syslog server; SMB audit with reports; guided oomd and ARC tuning |

**Recommendation:**

- copy OPNsense for history and diff, DSM for backups triggered by every change, Unraid for separating configuration and binaries;
- avoid TrueNAS's mistake of a passphrase-protected single archive;
- for flash and logs adopt the log2ram/folder2ram model with periodic flush, not `volatile` alone;
- make rynas itself a good citizen of the system disk;
- everything else is proposed and explained, never imposed.

### Sources

- Synology, configuration backup: https://www.synology.com/en-us/knowledgebase/tutorials/511
- TrueNAS, configuration management: https://www.truenas.com/docs/scale/systemsettings/advanced/managesysconfig
- TrueNAS 25.04, update screens: https://www.truenas.com/docs/scale/25.04/scaleuireference/systemsettings/updatescreensscale/
- TrueNAS 26, backup: https://www.truenas.com/docs/scale/26/gettingstarted/configure/setupbackupscale/
- OPNsense, backups: https://docs.opnsense.org/manual/backups.html
- Unraid, automated flash backup: https://docs.unraid.net/unraid-connect/automated-flash-backup
- OMV on USB: https://openmediavault-docs.readthedocs.io/en/latest/installation/on_usb.html
- folder2ram: https://github.com/semool/folder2ram
- Debian 13 release notes: https://www.debian.org/releases/trixie/release-notes.en.txt
- Debian 12 release notes: https://www.debian.org/releases/bookworm/amd64/release-notes.en.txt
- zram-generator: https://github.com/systemd/zram-generator
- btrfs discard: https://blogs.oracle.com/linux/post/btrfs-discard
- Red Hat bug 2182228: https://bugzilla.redhat.com/show_bug.cgi?id=2182228
- Phoronix, btrfs async discard by default: https://www.phoronix.com/news/Btrfs-Async-Discard-Default
- ZFS ARC: https://blog.thalheim.io/2025/10/17/zfs-ate-my-ram-understanding-the-arc-cache/
- Netdata, ARC using all memory: https://www.netdata.cloud/guides/zfs/zfs-arc-using-all-memory/

Some values come from general technical knowledge, without a source verified in this research:

- Samba defaults (multichannel, socket options);
- md `stripe_cache_size` default;
- NFS threads;
- Pop!_OS swappiness.

They should be confirmed against the trixie man pages during implementation.
