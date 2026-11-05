# rynas research: malware scanning and monitoring

Scope: proposals for (1) antivirus, malware and ransomware protection and (2) monitoring, history and statistics. Target is a DS920+ (Celeron J4125, 4–8 GB RAM, root on flash) running Debian 13 (synodeb). This builds on D-TASKS, D-NOTIFY, D-SNAP, D-DEDUP, D-SYSTUNE, D-FILESVC and the metrics design in `docs/phase0/research/02-architecture.md` §5.

---

## 1. Antivirus, malware and ransomware

### 1.1 What others do

| Product | Engine | Modes | Quarantine | Notes |
|---|---|---|---|---|
| **DSM AntiVirus Essential** (free) | ClamAV | scheduled plans per target, weekly idle scan by default (Sat ~01:00), option to scan only new or modified files, manual scan | yes, with a log | needs ≥1 GB RAM; skips files >2,048 MB and read-only shares |
| **DSM AntiVirus by McAfee** (paid) | McAfee | same UI, adds real-time scanning | yes | only on some models; still listed for DSM 7.2–7.4 |
| **DSM 7.2 Immutable Snapshots / WriteOnce** | — | snapshots locked against a compromised admin; WORM shares | — | Synology presents these as its ransomware defence; some models only |
| **QNAP** | Malware Remover with Ransomware Guard (QuTS hero h6) | detects behaviour: mass encryption, mass rename, unusual I/O | blocks the process | Security Center (QTS 5.2, beta) reacts to unusual file activity: switches to read-only, takes a snapshot, **pauses snapshot schedules** so bad snapshots cannot push good ones out |
| **TrueNAS** | none built in | ClamAV was only a community app with no UI | — | the vendor relies on immutable ZFS snapshots and replication |
| **OMV** | `openmediavault-clamav` plugin | scheduled jobs per shared folder, with mail; on-access via clamonacc | quarantine option | a thin wrapper over Debian packages |
| **Unraid** | ClamAV container (Community Applications) | manual or scheduled | — | community-maintained, not built in |

The shared pattern is ClamAV with scheduled scans and quarantine. Real-time scanning is paid (DSM) or best-effort (OMV). Vendors put their ransomware defence in **snapshots** and **behaviour detection**, not in signature scanning.

### 1.2 ClamAV on Debian 13: facts that matter

- **Version.** Trixie ships `clamav 1.4.3+dfsg-1`, kept current through stable updates. Upstream released 1.5.0 in October 2025 and 1.5.1 on 2025-10-16 (CVD `.sign` verification, FIPS mode, memory-stats fixes).
- **Memory.**
  - clamd keeps every signature in RAM. Field reports put it at about **1–1.5 GB resident** with the official databases, and the figure grows as databases grow.
  - During a reload it builds a second engine before swapping, so for a while it needs roughly **double**. `ConcurrentDatabaseReload no` avoids that, but scans block during the reload.
  - OOM kills on 4 GB hosts are a known pattern (mailcow, IPFire). The official Docker docs suggest giving the container 4 GB.
  - **On a 4 GB NAS, an always-on clamd is not acceptable as a default.**
- **freshclam.** Run it on a timer or as the Debian daemon at default frequency. The CDN rate-limits clients that update too often. Turn off `NotifyClamd` when no clamd is running.
- **clamonacc (on-access).**
  - Uses fanotify (kernel ≥3.8). Blocking (`OnAccessPrevention`) needs `CONFIG_FANOTIFY_ACCESS_PERMISSIONS`; without it the mode is notify-only.
  - Recursive watching (DDD) uses inotify watches. The default 8192 must be raised (docs suggest 524288), which costs kernel memory on large trees.
  - `OnAccessMountPath` avoids inotify but cannot block or exclude paths.
  - Blocking on broad paths "can lock up the system". clamonacc also needs a resident clamd.
  - Nothing in the docs makes btrfs or mdadm special. The limits are watch count, queue depth and the RAM held by clamd.
- **Samba `vfs_virusfilter`** (already in `samba-vfs-modules`).
  - Backends: clamav (via the clamd socket), sophos, fsav.
  - Defaults: `scan on open = yes`, `scan on close = no`, `max file size` 100 MB, `infected file action = nothing|quarantine|rename|delete`, a quarantine directory that keeps the tree, a result cache (100 entries, 10 s), `block access on error = no`, and an `infected file command` hook.
  - It is SMB-only and also needs a resident clamd.
- **Detection value.** ClamAV is good hygiene: it catches known malware and Office/PDF droppers and meets compliance checkboxes. It is weak against new ransomware. Treat it as one layer, not the defence.

### 1.3 Alternatives

- **YARA.** ClamAV already loads YARA rules. **YARA-X** (VirusTotal, Rust) is a native option for user or organisation rule packs with low memory use; it does not replace a signature feed.
- **Sophos and ESET for Linux.** Commercial and on-access. `vfs_virusfilter` supports Sophos (SAVDI) directly. This is a possible business plugin, not core.
- **rspamd and mail AV.** Not relevant to the NAS core.

### 1.4 Ransomware: what actually works

1. **Snapshots that a compromised client cannot delete** (the real defence).
   - btrfs read-only snapshots outside the share tree are already invisible to SMB clients (D-SNAP).
   - To resist a compromised *admin*, as DSM's immutable snapshots do, add a **retention lock**: rynasd refuses to delete locked snapshots before the lock expires, even for an admin in the UI or API.
   - Add **pull replication** to a second box, so the source has no credentials to delete remote copies.
2. **Canary (honey) files.** Proven community approach (CanaryTek ransomware-samba-tools, ransom2ban, Univention with fail2ban):
   - rynas places decoy files at the top of each share; names must be customizable, because published defaults are easy to evade.
   - rynasd watches only those files with inotify or fanotify, which is close to free.
   - On modify, rename or delete:
     - emit a critical event;
     - optionally cut that client's SMB session (`smbcontrol … close-share` / `kill-client-ip`) and block its IP in nftables for N minutes;
     - take an emergency read-only snapshot of the share;
     - **pause snapshot pruning** for that volume (the QNAP idea).
3. **Mass-rename and known-extension detection.** `vfs_full_audit` logging only `renameat`/`unlinkat`, plus a per-user rate threshold and a known-extension list. Off by default because of log volume (D-SYSTUNE already flags full_audit as opt-in).
4. Entropy analysis of writes is too costly on a J4125. Skip it.

### 1.5 Proposal for rynas

**Antivirus plugin (`antivirus`, optional, not installed by default).** Thin layer over Debian `clamav`, `clamav-freshclam` and `clamdscan`, in the style of OMV.

- **Scan jobs are rynas tasks** (D-TASKS):
  - generated systemd units with `Nice=19`, `IOSchedulingClass=idle`, `CPUQuota` (default 200%, 2 of 4 cores), `MemoryMax`;
  - default calendar: weekly in the maintenance window;
  - never during scrub, resync or a dedup window (shared per-filesystem lock);
  - SIGINT at the window's end, resumable.
- **No resident clamd by default.**
  - Each job starts a **transient clamd inside its own unit** (`MaxThreads=2`, `ConcurrentDatabaseReload no`), runs `clamdscan --fdpass --multiscan` over the targets, and stops clamd when done.
  - The database load costs about 20–60 s per job, which is irrelevant for a weekly job. The 1+ GB is used only while the job runs.
  - Fallback is single-threaded `clamscan` when RAM is very tight.
- **Incremental scans** (the DSM "new or modified files" option, done the btrfs way):
  - keep the btrfs generation of the last completed scan per share;
  - list changes with `btrfs subvolume find-new <subvol> <gen>` (or from the scan's snapshot) and scan only those files;
  - full scan monthly or on demand.
- **On-demand scans** from the file manager ("Scan with antivirus" on files or folders) and from the share page. They run as a one-shot task with progress in the task panel.
- **Quarantine.**
  - Root-only subvolume `/<vol>/@rynas-quarantine/`. Files keep their tree, ownership and ACLs are stripped, mode 000.
  - Index in SQLite: original path, owner, signature, SHA-256, time.
  - UI actions: restore (to the original or another path), delete, "report false positive" (adds the hash to `.fp`). Auto-purge after 30 days.
  - The UI warns that the file **is still present in N snapshots**.
- **Actions:** report only (default for the first scan), quarantine (default after that), delete (never the default).
- **Limits.** Respect ClamAV's `MaxFileSize`/`MaxScanSize` (defaults 100/400 MB). Expose them with a warning, as DSM does at 2 GB. Default excludes: `#recycle`, `.snapshots`, VM images, docker data-root, the Frigate recordings volume.
- **freshclam:** Debian default frequency, plus a check before each job. Health is shown in the plugin header (database age, last update).
- **Notifications** (D-NOTIFY) with typed, stateful events: `av.infected` (one key per file, grouped per job), `av.job_failed`, `av.db_stale` (more than 3 days).

**Real-time scanning ("Advanced", phase 3).** Two options, both needing a resident clamd:
- `vfs_virusfilter` on chosen SMB shares (scan on open, quarantine, cache), or
- clamonacc notify-only on chosen paths.

The UI enforces a **RAM gate**: disabled below 6 GB, with a warning and a measured clamd RSS between 6 and 8 GB. Prevention or blocking mode only on explicit opt-in.

**Ransomware protection (core, not the AV plugin; phase 2).**
- Canary files with response rules: notify, disconnect, block IP, emergency snapshot, pause pruning.
- Snapshot retention lock and pull-replication guidance (phase 2–3).
- full_audit mass-rename detection as an opt-in toggle (phase 3).

**Memory guidance shown in the plugin:**

| RAM | Allowed |
|---|---|
| 4 GB | scheduled and on-demand only, `MaxThreads=1–2`, not during backups or dedup |
| 8 GB | the same, plus optional real-time scanning on 1–3 shares |
| 16 GB+ | real-time scanning without a warning |

**Phases:** antivirus plugin and canaries in phase 2 (they need tasks, notifications and the file manager); real-time scanning, YARA-X rule packs and retention lock in phase 3; Sophos as a business plugin.

---

## 2. Monitoring and statistics

### 2.1 What others do

| Product | Live | History | Notable |
|---|---|---|---|
| **DSM Resource Monitor** | CPU, RAM, net, disk, volume, iSCSI, per process | toggle "Enable usage history", period comparison | performance alarms; connection list (users, services, accessed files) with **kick user**; per-service speed limits |
| **DSM Storage Analyzer** | — | scheduled report profiles emailed with a link | volume usage, top folders, **200 largest files**, file types, owners, modified files, **duplicates** (max 5,000, optionally matched by name/mtime), trends from past reports |
| **TrueNAS 25.04+** | — | Reporting: CPU, disk (incl. temperature), memory, network per interface, system, UPS, ZFS/ARC; persistent across upgrades; zoom 1 h to 6 months (v27 docs) | Netdata-based backend, but **the embedded Netdata UI was removed in 25.04 for security hardening** (Netdata is now a separate app); exporters push to Graphite |
| **OMV** | — | collectd + rrdcached, RRDtool PNG graphs | old but light |
| **Unraid** | dashboard tiles | little native history; community plugins | — |
| **Proxmox VE/PBS** | — | RRD per node, VM and storage; hour/day/week/month/year, average/max | PVE 9 moved to a new RRD format aligned with PBS, added **PSI pressure** metrics and a "decade" range; external metric servers (InfluxDB/Graphite) |
| **Netdata** | per second | dbengine tiers | rich but 100+ MB RAM and constant writes |
| **Prometheus + Grafana** | — | TSDB | standard for people who already run it; too heavy to host on the NAS |
| **Beszel** | — | hub + agent (agent ~10 MB) | simple multi-host view, optional SMART |
| **Glances** | per second | none | Python, interactive tool |
| **Scrutiny** | — | SMART history with failure thresholds based on Backblaze data | the model for SMART trends |

Ideas to take:
- From Proxmox: RRD tiers and range buttons, average/max, PSI.
- From DSM: connections with kick, history toggle, Storage Analyzer reports.
- From TrueNAS: don't embed a third-party root dashboard; offer an exporter instead.
- From Scrutiny: SMART trend thresholds.
- From QNAP and DSM: alarms that feed notifications.

### 2.2 Current state in rynas

`rynas-web/src/metrics/` already has:
- a 1 Hz collector on its own OS thread, with no process spawning;
- a 3,600-sample ring (one hour);
- SSE broadcast of pre-serialized JSON;
- `history` downsampling to ≤600 points;
- CPU (total and per thread), load, memory/cache/swap, network and disk rates with busy %, temperatures.

Missing: persistence, tiers beyond one hour, volume, container and share series, PSI, SMART history, and an alerting path.

### 2.3 Proposal: native core

**A. Multi-resolution RRD (proxmox-rrd model), in rynas-web.**
- The existing 1 s ring stays as the "live / 1 h" tier, in RAM only.
- Persistent tiers, fixed size, f32 average + f32 max per point (8 B):

| Tier | Points | Serves |
|---|---|---|
| 1 min × 24 h | 1,440 | 1 d |
| 5 min × 7 d | 2,016 | 1 w |
| 1 h × 62 d | 1,488 | 1 m |
| 1 d × 5 y | 1,825 | 1 y / 5 y |

  That is about 6,800 points per series, **~54 KB per series**.
- Series budget on a DS920+ with 4 HDD + 2 NVMe and ~10 containers is roughly **120–150 series, 7–8 MB of RAM**.
  - Host: CPU, load, mem/cache/swap/available, PSI cpu/mem/io.
  - Per disk: read/write/busy.
  - Per md array and volume: r/w and used bytes.
  - Per physical interface: rx/tx/errors (no veth).
  - Temperatures, UPS.
  - Per container: cpu/mem/io from cgroup v2, read directly with no Docker API.
  - Per share: used bytes, daily.
- **Low write rate.**
  - One file per tier, laid out **time-major**: a row is all series at one timestamp. A 30-minute flush then writes one contiguous block, about 30 KB for the 1-minute tier, instead of touching one page per series.
  - Flush every 30 min and at shutdown. Files `chattr +C` on btrfs, under `/var/lib/rynas/rrd/`.
  - Expected writes: **a few MB/day**, against hundreds of MB/day of journald, so negligible next to the D-SYSTUNE budget. A crash loses at most 30 minutes of coarse history, which Proxmox accepts too.
  - Series added or removed (new disk or container) live in a header map; removed series age out.
- **CPU.** Consolidation is a few hundred float operations per minute. Adding PSI (3 small files) and cgroup reads (~3 files per container) keeps the collector well under 1% of one J4125 core.
  - Expensive collectors run only while a page is subscribed: top processes (`/proc/*/stat` every 3 s) and open files.
- **API:** `GET /api/v1/metrics/history?series=…&range=1h|1d|1w|1m|1y&cf=avg|max`. The server picks the tier, so a response is always ≤ ~2,000 points. SSE keeps sending only subscribed series.

**B. Activity views** (data and actions through rynasd).
- **SMB:**
  - `smbstatus --json` gives sessions (user, client IP, protocol, encryption, signing), tree connects and open files with lock state;
  - **Disconnect** via `smbcontrol <pid> close-share <share>` or `smbcontrol smbd kill-client-ip <ip>`.
- **NFS:**
  - v4 clients come from `/proc/fs/nfsd/clients/*/info` and `…/states` (open files);
  - v3 shows only connected client IPs, from the socket table;
  - expire a v4 client through `…/ctl`.
- rynas web, SSH and SFTP sessions in the same list.
- Per-user bandwidth or IO: **not native** (Samba has no per-user counters). Show per-client-IP throughput only if conntrack accounting is enabled (opt-in, costs CPU). DSM-style speed limits are deferred: Samba cannot do them, so they would need `tc`.

**C. SMART, wear and temperature history** (rynasd, D-SYSTUNE).
- Read every 30 min with `smartctl -n standby` (never wake a disk); store in SQLite only when values change.
- HDD attributes kept: 5, 187, 188, 197, 198, 199, 194, power-on hours, load cycles. NVMe: percentage used, data units written, media errors, temperature, available spare.
- Show a trend per attribute; derive TB written per day and estimated remaining life (already planned).
- Scrutiny-style evaluation: **any increase** of 5, 197 or 198 is a warning; pending sectors >0 is critical; NVMe percentage used ≥80% is a warning.

**D. Alerts into the notification engine.**
- rynas-web evaluates rules on the RRD stream (condition + duration + hysteresis) and emits stateful events to rynasd. Firing and resolved transitions, deduplication and quiet hours already exist in D-NOTIFY.
- Defaults, all editable:
  - volume 80% warning, 90% error, 95% critical;
  - HDD >50 °C warning, >55 °C critical; NVMe >70 °C;
  - CPU >90% for 15 min;
  - available memory <10% for 10 min, or PSI memory `some avg60` >10%;
  - swap-in rate sustained;
  - NIC errors increasing;
  - SMART per C;
  - UPS from the nut plugin.

**E. Storage reports (Storage Analyzer model) as a scheduled task.**
- Task `storage-report`: idle IO class, Nice 19, per share or volume, weekly by default; also runnable from the file manager ("Analyze this folder").
- One `statx` walk that excludes snapshots and `#recycle`. Output:
  - top N folders and files (default 200);
  - type distribution by extension and MIME group;
  - age buckets;
  - per-owner usage (this is the "per-owner nightly report" from D-IDENTITY);
  - apparent vs allocated size (compression and reflinks).
- **Duplicates:**
  - reuse the **oans hash DB** where dedup is configured (D-DEDUP);
  - otherwise group by size, then a 64 KB partial hash, then a full BLAKE3 hash, with a cap like DSM's.
  - Link to "deduplicate with oans" instead of "delete".
- Reports are stored as compressed JSON, last 12 kept; **growth per share** is the delta between reports plus the daily used-bytes series from A.
- Email the summary with a link (DSM model). The file manager's space view reuses the same walker code.

**F. Optional exporter.**
- `/metrics` (OpenMetrics) served from the same in-memory store, **off by default**, bearer token, LAN-only by firewall profile.
- Names `rynas_*`, with labels device, volume, share, container, iface.
- Ship a **Grafana dashboard JSON** in `packaging/` and a Prometheus scrape example.
- No push exporters in core; a Graphite or InfluxDB push like TrueNAS and Proxmox could come later as a plugin.

**Delegate to apps (catalog, D-APPCAT), never embedded or root-proxied (TrueNAS lesson):** Netdata, Prometheus + Grafana, Beszel hub, Scrutiny, Uptime Kuma. Do not install node_exporter or Prometheus on the NAS by default (4 GB RAM).

### 2.4 UI sketches

```
Monitor ─ [Overview] [Disks] [Volumes] [Network] [Containers] [Processes] [Connections]
Range: (1h) 1d 1w 1m 1y   avg|max                         PSI cpu 2% mem 0% io 7%
┌ CPU % ──────────────────────────┐ ┌ Memory ───────────────────────────┐
│ ▁▂▂▃▅▃▂▁▁▂▇▅▂▁ max 94  avg 18   │ │ used ▆▆▆▆▇ cache ░░░ avail 1.2 GB │
└─────────────────────────────────┘ └───────────────────────────────────┘
┌ Disks (busy %)        sda ▂▂▅▂  sdb ▂▂▅▂  nvme0 ▁▁▂▁ ─ click → per-disk ┐
```

```
Connections                                          [Refresh 5s]
User   Client        Proto       Share     Open files  Since    
anna   10.0.0.21     SMB3.1.1 🔒  projects  12          09:14   [Files] [Disconnect]
host7  10.0.0.40     NFSv4.2     media     3           yesterday        [Expire]
```

```
Disk sdb  WDC WD40EFRX   ● Healthy   42 °C   POH 31,204
Attr 5 Reallocated   0 ───────────────  Attr 197 Pending 0 ──────────
Temp 1y  ▃▃▄▅▆▅▄▃   max 47 °C (Aug)     Written 1y: 9.8 TB
```

```
Storage report · share "projects" · 2026-11-02     Δ vs last week: +38 GB
Top folders  ▇▇▇▇ /renders 412G  ▅▅ /archive 201G …   Types: video 61% · cad 18% …
Duplicates: 1,284 files · 57 GB reclaimable   [Deduplicate with oans]
```

### 2.5 Phases

| Phase | Monitoring | Security |
|---|---|---|
| **1** | persistent RRD tiers for host, disk, net and volume; range selector; PSI; temperature and SMART history (rynasd); default thresholds → notifications; top processes | — |
| **2** | per-container and per-share series; Connections page (SMB/NFS with disconnect); storage-report task with duplicates through oans; `/metrics` + Grafana JSON; UPS history (nut plugin) | antivirus plugin (scheduled, incremental, on-demand, quarantine); canary files with response rules |
| **3** | per-client-IP stats (opt-in conntrack); external metric push plugin; speed limits through `tc` if there is demand | real-time scanning (vfs_virusfilter / clamonacc) behind the RAM gate; YARA-X packs; full_audit mass-rename detection; snapshot retention lock; Sophos business plugin |

---

## Sources

- Synology: [AntiVirus Essential spec](https://www.synology.com/en-us/dsm/7.2/software_spec/antivirus_essential), [AV Essential help](https://kb.synology.com/en-ro/DSM/help/AntiVirus/av_desc?version=7), [AntiVirus by McAfee](https://www.synology.com/en-us/dsm/7.4/software_spec/antivirus_mcafee), [DSM 7.2 immutable snapshots / WriteOnce](https://www.synology.com/en-uk/DSM72), [Resource Monitor](https://kb.synology.com/da-dk/DSM/help/DSM/ResourceMonitor/rsrcmonitor_desc?version=7), [System operations & monitoring 7.4](https://www.synology.com/en-global/dsm/7.4/software_spec/system_operations_monitoring), [Storage Analyzer spec](https://www.synology.com/en-us/dsm/7.4/software_spec/storage_analyzer), [Report profiles](https://kb.synology.com/da-dk/DSM/help/StorageAnalyzer/manage_profiles?version=7)
- QNAP: [Security Center](https://www.qnap.com/en/software/security-center), [Ransomware Guard FAQ](https://www.qnap.com/en/how-to/faq/article/what-is-ransomware-guard)
- TrueNAS: [Reporting screens 25.04](https://www.truenas.com/docs/scale/25.04/scaleuireference/reportingscreensscale/), [Reporting (v27)](https://www.truenas.com/docs/scale/27/reporting/reportingscreens/), [ClamAV community app thread](https://www.truenas.com/community/threads/question-about-clamav-usage.115104/)
- ClamAV: [On-Access docs](https://docs.clamav.net/manual/OnAccess.html), [Docker docs (RAM, reload)](https://docs.clamav.net/manual/Installing/Docker.html), [1.5.1 release](https://blog.clamav.net/2025/10/), [Debian src:clamav](https://packages.debian.org/src:clamav), [mailcow OOM issue](https://github.com/mailcow/mailcow-dockerized/issues/3761), [ApisCP clamd RAM thread](https://forums.apiscp.com/t/clamd-consuming-1434-mb-of-ram-and-the-usage-increasing/495)
- Samba: [vfs_virusfilter(8)](https://www.samba.org/samba/docs/current/man-html/vfs_virusfilter.8.html); ransomware on Samba: [Univention fail2ban](https://help.univention.com/t/cool-solution-ransomware-protection-with-fail2ban/21820), [CanaryTek tools](https://github.com/CanaryTek/ransomware-samba-tools)
- OMV: [ClamAV plugin how-to](https://nerdiy.de/en/howto-openmediavault-virus-scanner-configure-and-activate-clamav)
- Proxmox: [PVE 9 RRD format patches](https://lists.proxmox.com/pipermail/pve-devel/2025-July/072645.html)
- Beszel: [overview](https://selfhosted-sh.pages.dev/beszel-lightweight-server-monitoring)

To check before implementation:
- the exact `/proc/fs/nfsd/clients/*/ctl` expire semantics on the trixie kernel;
- `smbstatus --json` field names in Samba 4.22;
- measured clamd RSS with 1.4.3 and the current databases on the DS920+.

---

# rynas research addendum §3: hardware sensors and UPS monitoring

This section extends the report above. It plugs into §2.3: RRD tiers, alert rules into D-NOTIFY, and the Monitor UI.

## 3.1 Starting point in the code

- `rynas-web/src/metrics/temps.rs` already reads hwmon:
  - CPU via `k10temp` or `coretemp`;
  - NVMe;
  - NICs (atlantic, aqc, r8169, ixgbe, igb);
  - drivetemp, read only after recent I/O and at most every 30 s. This matches the kernel note that reading SCT temperature can reset some drives' spin-down timer.
- Bays come from `HardwareProvider` (synohwd).
- Fans, voltages, power, IPMI, thresholds and history are not covered yet.

## 3.2 What others do

| | Sensors | Fan control |
|---|---|---|
| **DSM** | fixed per model | fan modes (quiet, cool, full) through the platform |
| **TrueNAS** | CPU and disk temperatures in Reporting; IPMI settings for the BMC network only | **none built in**. Community scripts: mrstux hybrid CPU/HD zone controller, nas_fan_control PID variant, `truefan`, all using Supermicro `ipmitool raw` and run as post-init scripts |
| **Unraid** | Dynamix System Temp (lm-sensors), IPMI plugin | **Dynamix Auto Fan Control**: PWM driven by the hottest array disk, NVMe can be excluded; fork FanCtrl Plus 2 adds multi-group curves. Common forum failures: BIOS overrides PWM, headers without real PWM, disks reading 0 °C |
| **Proxmox / OMV** | lm-sensors via third-party add-ons | none |
| **CoolerControl** | hwmon, liquidctl, GPUs; v4.2 (April 2026) auto-detects devices | desktop-oriented daemon with its own UI; too much for a NAS core |
| **fancontrol / pwmconfig** (Debian lm-sensors) | — | interactive setup; config tied to `hwmonN` paths and checked with DEVPATH/DEVNAME; fragile after kernel or module changes |
| **thinkfan** | — | ThinkPad-specific |

## 3.3 Facts that drive the design

- **AMD `k10temp`:**
  - `temp1` is **Tctl**, the fan-control value. On some Zen 1/Zen+ parts it carries an offset (the driver's table has +20 °C for the Ryzen 7 1800X).
  - **Tdie** (`temp2`) is the real temperature where exported. `Tccd1..N` are per CCD.
  - Rule: display Tdie when present, else Tctl with a note; alarms use Tdie or max(Tccd); fan curves use Tctl.
- **Intel `coretemp`:** use "Package id 0" (the J4125 reports package and per-core values).
- **Super-I/O chips:**
  - `nct6775` (Nuvoton) is mainline and covers fans, PWM and voltages.
  - ITE `it87` in mainline lags; most current boards need the out-of-tree DKMS driver (frankcrawford/it87), and some IT8689 boards still cannot be controlled manually.
  - **Voltages are raw `inN` values.** Correct scaling needs per-board `compute` lines (lm-sensors `sensors.d`). Unscaled voltages cause false alarms.
- **drivetemp:** SCT, falling back to SMART; values in milli-°C; may reset spin-down timers on some models.
- **GPUs:** amdgpu hwmon gives edge, junction and mem temperatures, fan and power. i915/xe hwmon exists for discrete Intel cards only. NVIDIA only via `nvidia-smi` (optional).
- **CPU power:** RAPL energy (`/sys/class/powercap`) is root-only since CVE-2020-8694, so rynasd must provide it. It gives a cheap "package watts" series, useful on a J4125.
- **IPMI/BMC:**
  - **freeipmi** (`ipmi-sensors --output-sensor-state`, `ipmi-sel`, comma-separated output, SDR cache) parses more reliably than ipmitool. In-band access via `/dev/ipmi0` needs root, so it goes through rynasd.
  - BMCs are slow: poll every 30–60 s.
  - Supermicro: vendor fan *modes* (Standard, Full, Optimal, Heavy IO) and on X10/X11 per-zone duty (`raw 0x30 0x70 0x66 …`). Noctua-type fans need a duty floor of about 30%, or the BMC sees "fan failure" and goes to full speed.
  - Dell: manual fan control (`raw 0x30 0x30 …`) works up to iDRAC9 3.30.30.30 and is **revoked from 3.34.34.34** ("Insufficient privilege").
  - HPE iLO: no supported manual fan interface.

## 3.4 Proposal: provider model in `rynas-hw`

```
trait SensorProvider {           // in-process in rynas-web (unprivileged) or proxied by rynasd
  fn id() -> &str;               // "hwmon" | "ipmi" | "synology" | "gpu" | "rapl"
  fn discover() -> Vec<SensorDesc>;   // every 60 s and on udev events
  fn read() -> Vec<Reading>;          // at the provider's own period
  fn controls() -> Vec<FanControl>;   // empty = read-only
  fn events_since(cursor) -> Vec<HwEvent>; // IPMI SEL, intrusion, synohwd events
}
SensorDesc { id, provider, class: cpu|board|drive|nvme|nic|gpu|psu|fan|voltage|power|intrusion,
             unit, label, device (PCI/ATA/NVMe path), thresholds{warn,crit,min,max,source}, period }
```

**Providers:**

| Provider | Covers | Notes |
|---|---|---|
| `hwmon` | everything in `/sys/class/hwmon`, no libsensors dependency | |
| `drive` | drivetemp plus the SMART cache from rynasd | standby-aware, as today |
| `synology` | synohwd bays, fans, PSU, events | **owns** fans on Synology; rynas never writes PWM there |
| `ipmi` | sensors and SEL | only if `/dev/ipmi0` or `ipmi_si` is present; through rynasd |
| `rapl`, `gpu` | power, GPU sensors | optional |

**Naming:**
- Stable IDs never use the `hwmonN` number, which changes between boots. Form: `hwmon:<driver>:<device path>:<channel>`, e.g. `hwmon:k10temp:pci-0000:00:18.3:temp2`, or `ipmi:<SDR record name>`.
- Default label comes from `*_label` (Tctl, Composite, Package id 0) plus the class.
- The user can **rename, hide, or pin a sensor to the dashboard**; overrides live in config.db.
- An optional import of lm-sensors `sensors.d` labels and `compute` lines **enables voltages**. Voltages are hidden by default until scaled.

**Thresholds, in priority order:**
1. user override;
2. driver attributes `*_max`, `*_crit`, `*_min` and `*_alarm`, or IPMI SDR thresholds and sensor state;
3. defaults per class:

| Class | Warning | Critical |
|---|---|---|
| CPU | 85 °C | 95 °C |
| HDD | 50 °C | 55 °C |
| NVMe | 70 °C | driver crit |
| NIC | 90 °C | — |
| fan | — | 0 RPM while PWM >0 (stall) |
| intrusion | — | `intrusion0_alarm` set |

**History:**
- hwmon is read in the existing 1 Hz collector, which costs microseconds per file.
- RRD stores average + max for temperatures, fan RPM, PWM %, scaled voltages and watts: roughly **20–40 extra series, about 2 MB of RAM**, with no extra writes thanks to the time-major files of §2.3.
- IPMI and drive series use their own periods (30–60 s and SMART cadence) and fill the 1-minute tier.

**Alerts:**
- threshold rules with duration and hysteresis;
- fan stall is critical;
- IPMI sensor state Warning/Critical;
- **new SEL entries** become events: critical for PSU, ECC and fan-failure records, info for the rest;
- chassis intrusion.

All flow into D-NOTIFY with stateful keys (`hw.temp:<sensor id>`).

**Setup helpers:**
- A "Detect sensors" button runs `sensors-detect --auto` (Debian `lm-sensors`) in rynasd and shows the proposed modules.
- After confirmation it writes `/etc/modules-load.d/rynas-sensors.conf`.
- It **never installs DKMS drivers automatically**; for it87 boards it links to documentation.

**Fan control** (phase 3, generic hardware only):
- A small controller in rynasd, not fancontrol:
  - per-PWM curve from a source set: max HDD temperature from the standby-aware cache, where disks in standby drop out; CPU Tctl; NVMe;
  - hysteresis and minimum duty;
  - a calibration wizard that steps PWM and measures RPM and stall point, so it detects fake PWM headers.
- **Fail-safe:**
  - stale source, service stop or crash → restore `pwmN_enable` to the BIOS/automatic value (`ExecStopPost`), or 100%;
  - the controller refuses to start if `fancontrol` or `coolercontrold` is active, and refuses on Synology.
- **IPMI fan control:**
  - Supermicro vendor mode selection as a safe preset;
  - per-zone curves (the mrstux hybrid model) as "experimental";
  - Dell and HPE read-only, with an explanation.

**UI:**

```
Hardware ─ [Sensors] [Fans] [Power] [IPMI log]           Provider: hwmon · ipmi
Sensor              Value   24h            Min/Max    Warn/Crit  
CPU Tdie (k10temp)  41 °C   ▂▃▃▅▃▂▂▃       35/72      85/95   📌 ✎
NVMe nvme0 Comp.    46 °C   ▃▃▄▄▃▃▃▃       39/58      70/80      
Fan CPU_FAN         820 rpm ▃▃▃▃▅▃▃▃       780/1450   stall      
PSU1 (IPMI)         OK                                 SDR        
```

The IPMI log tab is a SEL table with severity, time and text, filters, "Clear SEL" (confirm) and export.

## 3.5 UPS monitoring (on top of D-UPS)

**Data source:**
- rynas-web speaks the **NUT TCP protocol** directly to localhost upsd (`LIST VAR`), with no `upsc` spawn and the same code for both backends (synodeb-ups and native).
- Poll every 5 s while on line and 1 s on battery. Do not enable `upslog`, which would be a second writer.

**RRD series** (average + max, tiers as in §2.3), recorded only when the UPS reports the variable:
- `battery.charge`, `battery.runtime`, `ups.load`, `input.voltage` (also min per bucket), `output.voltage`, `input.frequency`, `battery.voltage`, `ups.temperature`;
- watts: `ups.realpower`, or `ups.load × ups.realpower.nominal`.

**Event log** (SQLite `ups_events`):
- every `ups.status` transition: OL, OB, LB, CHRG, RB (replace battery), BYPASS, OVER, TRIM/BOOST (AVR active);
- policy actions (Safe Mode, shutdown, return), battery tests with `ups.test.result`, and NOCOMM.

**Outage statistics**, derived from the log:
- each OB→OL span gives duration, minimum charge, minimum runtime and estimated energy drawn;
- spans under 10 s are counted as **transfers or flickers**, not outages;
- per month: number of outages, total time on battery, longest outage, transfers per day, AVR activations, and daily input-voltage min/max. Frequent TRIM/BOOST points to poor mains, not the NAS.

**Battery health trend:**
- runtime reported at comparable load (in ±5% load bins) compared against the first 30 days of the battery's life;
- a drop of more than 30% is a warning ("battery aging"), together with `battery.date` or age and the test results.
- New battery: a "reset baseline" button.

**Alerts** (beyond the D-UPS events):
- RB;
- battery test failed;
- more than N transfers per day;
- overload;
- input voltage out of range for more than 1 min;
- runtime on line below the user threshold;
- NOCOMM for more than 5 min.

**Dashboard and graphs:**

```
┌ UPS  Eaton 5E 850 ● On line ───────────────┐
│ Charge 100%  Runtime 41 min  Load 18% 92 W  │
│ Input 229 V ▂▂▃▂▁▂▂  · last outage 3 d ago  │
└─────────────────────────────────────────────┘
```

- On battery: a global banner with a countdown to the configured Safe Mode or shutdown.
- **Monitor → UPS**: charts for charge, runtime, load, voltage and watts, plus an outage table and monthly statistics.
- **Annotation layer on every chart**: outages, reboots, updates and scrubs drawn as shaded bands or markers (the Grafana annotations idea), so a CPU or disk spike can be linked to its cause.
- The Prometheus endpoint exports `rynas_ups_*`.

## 3.6 Phases (this section)

| Phase | Work |
|---|---|
| **1** | hwmon provider with stable IDs, labels, rename, hide and pin; thresholds from driver attributes plus defaults; temperature and fan RRD history; stall and temperature alerts |
| **2** | IPMI read-only (sensors, SEL into events and the log view) via freeipmi in rynasd; "Detect sensors" wizard; RAPL and GPU power; lm-sensors `compute` import for voltages; UPS history, event log, outage statistics, battery trend and annotations (with the `nut` plugin) |
| **3** | generic PWM fan controller with calibration and fail-safe; Supermicro fan modes and experimental zone curves |

## Sources (addendum)

- Kernel docs: [k10temp](https://docs.kernel.org/6.9/hwmon/k10temp.html) (offset table in [k10temp.c](https://tangled.org/tjh.dev/kernel/blob/master/drivers/hwmon/k10temp.c)), [drivetemp](https://docs.kernel.org/6.9/hwmon/drivetemp.html)
- it87: [frankcrawford/it87](https://github.com/frankcrawford/it87), [Phoronix on the mainline driver](https://www.phoronix.com/news/IT87-Linux-Driver-Axing)
- CoolerControl: [v3.0](https://www.gamingonlinux.com/2025/10/coolercontrol-v3-0-0-brings-new-advanced-features-for-managing-hardware-on-linux), [v4.2](https://www.gamingonlinux.com/2026/04/coolercontrol-4-2-adds-auto-detection-of-new-devices-stress-testing-and-more/)
- Unraid: [Auto Fan Control issues](https://forums.unraid.net/topic/123968-fan-control-on-a-gigabyte-c246-wu/), [FanCtrl Plus 2](https://ca.unraid.net/apps/fanctrl-plus-2-08f0s880ywcl8k)
- TrueNAS community fan scripts: [mrstux hybrid_fan_control](https://github.com/mrstux/hybrid_fan_control), [nas_fan_control PID](https://github.com/khorton/nas_fan_control/blob/npeak/PID_fan_control.pl), [truefan](https://pypi.org/project/truefan/1.1.0/)
- Dell iDRAC fan control cutoff: [linux-poweredge](https://www.mail-archive.com/linux-poweredge@dell.com/msg05291.html), [guide](https://kovasky-github-io.pages.dev/blogs/fan_control)
- NUT: [upslog(8) 2.8.4](https://sources.debian.org/src/nut/2.8.4%2Breally-2/docs/man/upslog.8), [variable names](https://sources.debian.org/src/nut/2.7.2-4/data/cmdvartab)

To verify on hardware:
- Supermicro mode raw bytes per board generation;
- `ipmi_si` autodetection on Debian 13;
- RAPL availability on the J4125;
- which hwmon chips the DS920+ exposes besides coretemp (synohwd stays authoritative for fans).