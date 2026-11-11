# Deduplication of shared folders (oans)

Status: design 2026-11-10, answers of the owner 2026-11-11 (§5), implemented 2026-11-11
(§6). Refines decision row D-DEDUP
(`../phase0/05-decisions.md`) and `../research/encryption-compression-dedup.md` §3, whose
engine choice it keeps; corrections to that research are marked "(changed)". Nothing here
depends on the hardware: it works on any Debian machine with a btrfs volume (or an XFS
folder with reflink).

Offline batch deduplication finds files and extents with the same content and makes them
share one copy on disk through the kernel's `FIDEDUPERANGE` ioctl. The kernel locks the
pages and compares the bytes itself before it shares anything, so a bug in the tool can miss
duplicates but cannot corrupt data.

## 1. Engine, alternatives, packaging

### 1.1 Facts (oans v1.14.1, 2026-10-19; checked 2026-11-10)

| Topic | Fact | Consequence for rynas |
|---|---|---|
| Project | `martinus/oans`, a duperemove fork, GPL-2.0-only, started 2026-07-14, one maintainer, ~31 stars, 26 releases in 3 months, fast-moving | Pin a version and test it in CI before every bump; keep an engine trait |
| Release assets | `oans-X.Y.Z-linux-x86_64.tar.gz` + `.sha256` only: no signature, no .deb, no arm64. Dynamic binary built on Ubuntu 22.04 (glibc 2.35); needs `libglib2.0-0 libsqlite3-0 libblkid1 libmount1 libuuid1` | Build it ourselves (§1.4) |
| Build | `libglib2.0-dev libsqlite3-dev libxxhash-dev uuid-dev libmount-dev libblkid-dev libbsd-dev`; `make VERSION=vX.Y.Z` (otherwise "oans unknown"); `make install` also creates a `duperemove` symlink | Do not ship the symlink: it would shadow Debian's duperemove |
| Modes | Without `-d` it only hashes and reports duplicates (the only estimate mode). `-r` recursive. A run covers **one filesystem**: a root on another filesystem gives exit 2 | Estimate = the same run without `-d`; one run per filesystem |
| Hashfile | SQLite (+ `-wal`/`-shm`). It stores the job: roots (realpath), `-r`, `-d`, sizes, `--skip-zeroes`, `--skip-readonly-subvols`, `--dedupe-options`, excludes. `oans --hashfile=F` with no paths replays the stored job and **the stored values beat the command line**, `-d` included. A run **with** paths replaces the whole stored job. A format change in a new release rebuilds the file and drops the job | rynas never replays: every run passes paths and every option, so the hashfile is only a cache of hashes |
| Concurrency | No process lock, only SQLite `busy_timeout` 30 s. **Two writers (two `-d`, or `-d` and `-R`) can corrupt it.** Report modes (`--stats`, `--history`, `--json`, `-L`) are read-only and allowed during a run | rynas serializes every writer of a hashfile (§3.7) |
| Options | `-b` 128K default (4K..1M, fixed by the hashfile), `-B` 1024, `-m`/`--min-filesize` 1, `--max-filesize`, `--skip-zeroes`, `--exclude` (gitignore syntax, repeatable, **no `!`**, leading `/` = absolute path, `\` escapes), `--dedupe-options=same,partial,only_whole_files` (`same` on, the others off), `--skip-readonly-subvols` (**off** by default), `--io-threads` (auto, ≤ 8), `--cpu-threads` (CPUs, ≤ 8), `-q` | Defaults in §3.3 |
| Progress | `--progress=json`: NDJSON on **stderr**, ~1 line/s, mixed with diagnostics; phases `scanning` (`files_examined`, `files_to_hash`), `hashing` (`files`, `files_total`, `bytes`, `bytes_total`, `bytes_per_sec`, `eta_sec`), `dedupe` (`groups`, `groups_total`, `work_done_bytes`, `work_total_bytes`, `reclaimed_bytes`, `eta_sec`), final `{"event":"done",…,"reclaimed_bytes"}`. Unknown fields are omitted | Parse leniently, skip non-JSON lines |
| Report | `--json`: `block_size`, `files_tracked`, `logical_bytes`, `dup_groups`, `dup_files`, `reclaimable_logical_bytes`, `runs`, `reclaimed_total_bytes`, `files_hashed`, `last_run_ts`, `scan_configured_dedupe`, `readonly_subvols_skipped_last_run`, `scan_skipped_last_run{permission,unreadable,path_too_long,unsupported_fs}`, `scan_skipped_errors_total` | Read once after each run and stored in rynas state (never on page load: the hashfile lives on the data disks) |
| Exit | 0 done · 1 fatal · 2 done on less than asked (root missing, unmounted, other filesystem) · 130/143 SIGINT/SIGTERM, work saved, run not in `--history`. Per-file skips and "Not deduped" do not change the exit code | §3.8 |
| Interruption | Commits every 10 s; big files resume from checkpoints; SIGKILL loses the uncommitted batch. Upstream unit: `TimeoutStopSec=5min` | Stop = SIGINT with a 5 min grace, never rynas's usual SIGTERM + 3 s SIGKILL |
| Upstream unit | `oans@.service` root, `Nice=19`, `IOSchedulingClass=idle`, `NoNewPrivileges`; `oans@.timer` weekly | Not installed: rynas schedules runs itself |
| Size | 190k files / 10.5 GiB: peak RSS ~120 MiB, hashfile ~40 MiB; `partial` with `-b 4K`: ~8 GiB of block hashes per TiB | `partial` off by default; memory cap per run |
| Speed | Re-run on an already deduped tree (2.07 M files, 230 GiB, NVMe): ~92 s vs ~11 min for duperemove 0.15.2. No NAS-class numbers published; field note in the research: 2.7 TiB share, 250 GiB reclaimed in 8 h 26 min on a Celeron J3455 | Shown as an example, never promised |

### 1.2 Alternatives

| Engine | For | Against | Verdict |
|---|---|---|---|
| **oans** | Incremental (hashfile), clean SIGINT/resume (needed for time windows), JSON progress and report, exit 2 for partial coverage, gitignore excludes, XFS too | Young, one maintainer, no .deb, no signed releases | **Chosen** |
| duperemove (Debian) | In the archive; same model and ioctl | Debian 13 ships 0.11.2 (2022); 0.15.2 only in forky/sid. No JSON progress or report, no exit 2, snapshot exclusion did not work in the field (29 TB analysed instead of 2.9), much slower re-runs | Not a fallback in the first phase (changed): kept as a possible second `DedupEngine` once Debian 14 ships 0.15 |
| bees | Block level, whole filesystem, mature (v0.11, May 2026) | Daemon always running: no time windows, no disk spin-down; cannot be limited to a folder; hash table in RAM (128 MB per TB of unique data, more with compression); deduping into existing snapshots grows metadata until they expire; needs `--workaround-btrfs-send`; a balance invalidates its table; not in Debian (RFP #1103389) | No |

### 1.3 License

rynas is AGPL-3.0-or-later (D-LIC; the research still says BSL: outdated). oans is
GPL-2.0-only, which cannot be linked into AGPL-3.0 code. rynas only executes the `oans`
binary as a separate program with command-line arguments and reads its output: that is
aggregation, not a combined work, and needs no license change. Rules: never vendor, link or
port oans code into a rynas crate; parsers are written from the documented output formats.
The `rynas-oans` package carries upstream's GPL-2.0 text in its `copyright` file, and the
corresponding source (the pinned upstream tarball plus `packaging/oans/`) is published next
to the binary package (GPL-2.0 §3(a)).

### 1.4 Packaging plan

Same pattern as `packaging/caddy/` (D-PROXY):

- `packaging/oans/versions.env`: `OANS_VERSION`, the tag's commit SHA, the SHA-256 of the
  upstream source tarball, the builder image by digest (`debian:trixie@sha256:…`),
  `PKG_REVISION`. One change of the file = one package release.
- `packaging/oans/build.sh`: builds in that image from the verified tarball, `make
  VERSION=v$OANS_VERSION`, `-O2 -march=x86-64-v2` (like the rynas binaries; never
  `-march=native`), stripped, `SOURCE_DATE_EPOCH` from git; output
  `target/oans/rynas-oans_<ver>-<rev>_amd64.deb` + `.sha256`. arm64 later from the same
  script.
- Package `rynas-oans`: binary at **`/usr/lib/rynas/oans/oans`** (not `/usr/bin`: no clash
  with a user's own oans or Debian's duperemove, no `duperemove` symlink, no systemd units),
  man page under `/usr/share/man/man8/rynas-oans.8`. Depends on the trixie libraries above.
- `packaging/oans/check-upstream.sh` in CI: a new upstream release opens a bump; the bump
  runs `packaging/test-dedup.sh` (§4.4) before it is published.
- Installed **on demand**, like gocryptfs: rynas does not depend on it; "Enable
  deduplication" offers the install (apt through `rynas_tasks::Outside`, task
  `apt.install`). `GET /storage/dedup` reports `engine: {installed, version, supported}`;
  rynas refuses a version outside the range it was tested with (`engine_unsupported`).
- Until the apt repository exists: the same `.deb` is attached to rynas releases and
  installed from a local path with its SHA-256 compiled into rynas. Downloading upstream's
  tarball at run time is not done (no signature; the checksum comes from the same release
  page).

## 2. Impact on every rynas feature

"Must do" items are requirements for the implementation.

| Feature | Interaction | Risk | rynas must do |
|---|---|---|---|
| Snapshots (rynas-btrfs, btrbk) | Snapshots live in `/<vol>/.snapshots/<job>/` outside the share path; `#snapshot` inside a share is a read-only bind mount of them. oans scans read-only subvolumes by default and can dedupe into them | Deduping live files against snapshot extents means deleting old snapshots frees less; deduping inside snapshots grows metadata and breaks btrbk's incremental parents | Always `--skip-readonly-subvols`; always exclude `<root>/#snapshot/`; roots are share paths only, never the volume root. UI: on a share with snapshots, "the space comes back when the snapshots taken before this run expire: about <date>" (from the job's retention) |
| Snapshot replication (btrbk send, local and SSH) | Send and dedupe exclude each other on the same root: dedupe gets `EAGAIN` ("cannot deduplicate to root N while send operations are using it"). Send emits clone commands for shared extents with fewer than `SEND_MAX_EXTENT_REFS` (1024) references, otherwise full data; deduping between two snapshots can make the next incremental **bigger** | Failed dedupes, slow or large first send after a big dedup, target no longer deduplicated | Per-filesystem lock (§3.7): a send preempts dedup (SIGINT, resume later). On the first enable of a replicated share: warning, the duration and size of the next transfer recorded and compared in the job status; "deduplicate the target too" offered for local replicas (read-only: needs a separate decision, §5) |
| Restic backup (rynas-backup) | restic reads files from a snapshot and dedups by content itself; NAS-side dedup does not change repository size. restic detects changes by ctime/mtime | If dedupe changed ctime, every backup would re-read every deduped file (no extra data, much more IO) | Verify on 6.12 that `FIDEDUPERANGE` leaves mtime/ctime unchanged (btrfs passes `no_time_update` for dedupe; XFS to check) and keep it as a test in `test-dedup.sh`. Backup and dedup on the same volume never run together (lock). Local restic repositories are always excluded (unique packs, nothing to find) |
| PC backup (btrbk receive) | Received snapshots are read-only subvolumes | Same as snapshots | Covered by `--skip-readonly-subvols`; receive folders never offered as roots |
| Share size column (folders.rs: qgroup `referenced` or a walk summing `st_blocks`) | A walk counts every shared extent in every file (logical size). qgroup *referenced* counts an extent once per subvolume but in every subvolume that reaches it; *exclusive* is what deleting the folder frees, and dedup moves bytes from exclusive to shared | Sizes of shares add up to more than the volume's usage; users think dedup "did nothing" | Label the column "Used" with the note "data shared with other folders or snapshots is counted in each"; never sum share sizes as volume usage; show dedup savings per volume (oans) and per share (compsize after the run, §3.9). No change needed in the size code itself |
| btrfs quotas | Classic qgroups walk backrefs at every commit; many shared extents slow commits (minutes of 100 % CPU reported). Simple quotas (6.7+) charge an extent forever to the subvolume that wrote it first | Volume stalls during or after dedup; squota numbers do not drop in the deduped share | Qgroups stay off by default (D-SNAP). Enabling dedup on a volume with classic qgroups on shows a warning ("commits can become slow") |
| Recycle bins (`#recycle`) | Deleted files sit there until cleanup | Wasted hashing on files about to go | Exclude `<root>/#recycle/` by default (absolute pattern: a pattern starting with `#` would be a gitignore comment) |
| Encrypted folders (gocryptfs, D-CRYPT E1) | The share path is a FUSE mount (another filesystem: exit 2); the ciphertext has a random file ID per file, so nothing matches | Useless runs, false "partial" alarms | Dedup switch disabled with a reason; `.rynas-crypt` always excluded. LUKS volumes: transparent, no restriction |
| NoCOW shares (`chattr +C`) | Both files must have the same COW/checksum state (mixed pairs: `EINVAL`); a write into a shared NOCOW extent is forced COW once; the kernel flushes the inode first | Fragmentation and write latency for VM images, databases, recordings | Switch disabled on NoCOW shares; a NoCOW file found inside a COW share is skipped by the kernel (counted in "not deduped") |
| Compression (`compress=zstd:1`, per-share `none`) | Dedupe compares decompressed data, works on compressed extents; "Reclaimed" is logical; an extent that is only partly shared frees nothing | Overstated savings | Show oans' figure as "logical"; the real figure from compsize (Referenced − Uncompressed = dedup/reflink savings). Order: compress first, dedup after |
| Defrag / recompression (not in rynas yet) | `btrfs filesystem defragment` breaks reflinks: undoes dedup and snapshot sharing | Space use multiplies | Any future defrag/recompress action warns on dedup-enabled shares and resets their hashfile entries (`-R`) |
| Scrub (scheduler `Scrub`, volume page) | No known conflict, but both read the whole disk | IO contention, both take much longer | Lock: never together on one filesystem; scrub preempts dedup |
| Balance / grow / replace / convert (rynas-storage) | Relocation must find every reference: many shared extents make balance slow; a balance makes block hashes stale; `LOGICAL_INO` next to dedupe could hang old kernels | Long balances, wasted hashes | Storage operations on a filesystem take the lock and preempt dedup; after a balance the next run is a full re-hash (hashfile reset), told in the history |
| Disk spin-down (power, `asleep`) | Even an incremental run walks and stats every file (metadata IO) and reads changed files; the hashfile is on the volume | Disks woken at the start of each window and kept awake while it runs | Runs only inside the window; option "only if the disks are already awake" (uses the same `asleep` check as the size column); status and history come from rynas state, never from the hashfile |
| Power schedule shutdown (`check_tasks`) | A dedup run is a rynas task | A long dedup blocking a scheduled shutdown | Dedup does not count as busy for `check_tasks`: it is stopped with SIGINT before poweroff and resumes in the next window |
| Docker and apps | `/<vol>/docker/engine` and `containerd` are subvolumes outside the shares; stacks folders may be shared; live databases change constantly | Latency spikes while the kernel locks a busy file; hashing data that changes every minute | Always exclude the engine and containerd roots and `/<vol>/docker/.snapshots`; a share holding stacks gets a note; default `--min-filesize` skips most small DB files |
| Files app copies | Copies inside one btrfs filesystem are reflinks (already shared; oans 1.14.1 skips them); copies across volumes are real copies | none | Nothing; in-progress `.<name>.rynas-part` and `.rynas-aside` files are excluded |
| Tasks system | Runs are rynasd tasks (UPID, log, cancel) | Cancel would SIGKILL after 3 s | Stop = SIGINT, 5 min grace (§3.6); progress from `--progress=json` |
| Notifications | New events | Noise | §3.9 |
| Removable / USB volumes | Not shares; usually exFAT/ext4 | none | Not offered (later, if users ask, for btrfs/XFS USB disks with a rynas hashfile on the disk itself) |
| XFS | rynas volumes are btrfs (D-STO), but a share may be an existing folder on XFS. oans supports XFS with `reflink=1` (default since xfsprogs 5.1); ext4: no reflink | — | Offered on XFS only when `xfs_info` shows `reflink=1`; no snapshot logic there; hashfile under `/var/lib/rynas/dedup/` (no `@rynas-cache`) |
| System volume (`/srv/rynas/system`, D-ROOTVOL) | No `@rynas-cache`; often on flash | Hashfile writes on flash | Offered when it is btrfs; hashfile in `/var/lib/rynas/dedup/`; note about flash writes |
| Hardware modules (Synology, QNAP) | Data migrated from other NAS systems carries thumbnail/metadata folders (`@eaDir`, `.@__thumb`, …) | Hashing thousands of tiny thumbnails | Default excludes come from the core list plus a module extension point `dedup_excludes()` (D-MODULE); the core list is §5 Q4 |
| Config backup / revisions (rynas-config) | The `dedup` key is ordinary config; hashfiles are cache | none | Hashfiles never exported in `.rynasbak`; restore on another machine starts with empty hashfiles |

## 3. Settings and behaviour

### 3.1 Model

- **Settings are per shared folder** (the switch lives with the share); **the job is per
  filesystem**: one oans run per volume whose roots are the enabled shares on it. This finds
  duplicates **across** shares (the common case: the same files in "projects" and
  "archive") and matches oans' one-filesystem-per-run rule. One hashfile per volume.
- **rynas config is the source of truth**, the hashfile is a cache. Every run passes the
  full command (paths, every option, every exclude), so a lost, corrupt or rebuilt hashfile
  only costs a full re-hash. A share removed from the set is dropped with `oans -R <path>`
  inside the lock before the next run.

### 3.2 Where things live

- Hashfile: `/<vol>/@rynas-cache/dedup/volume.hash` (+ `-wal`, `-shm`): the volume's
  existing NOCOW cache subvolume (not snapshotted, not replicated). XFS folders, the
  system volume and volumes without `@rynas-cache`: `/var/lib/rynas/dedup/hash/<key>.hash`
  (a folder of its own: the run's sandbox makes only it writable, not rynasd's state). Reset
  only when the engine reports a damaged database (SQLite "database disk image is
  malformed" / "file is not a database") in two runs in a row; other failures (a timeout,
  memory, a missing binary) never reset it.
- rynas state: `/var/lib/rynas/dedup/<key>.json` (`<key>`: the job key, a volume label or
  `uuid.<uuid>`): last `--json` report, the roots the hashfile holds, the history
  (one record per run: start, end, outcome, exit code, reclaimed, files hashed, skipped
  counts, preempted by), the last estimate. Read by the UI; written only by rynasd.

### 3.3 Options exposed

| Setting | Where | Default | Notes |
|---|---|---|---|
| Deduplicate this folder | share | off | Disabled with a reason for encrypted, NoCOW, non-btrfs/XFS, XFS without reflink, folders on another filesystem than their volume |
| Extra excludes | share | empty | gitignore patterns relative to the share, turned into absolute `--exclude` by rynas; `!` refused |
| Window (7 × 24 grid, `week_grid.rs`) | volume | every night 01:00–06:00 | 168-char value like the other grids; runs never start outside it |
| Only if disks are awake | volume | on | §5 Q5 |
| Minimum file size | volume (advanced) | 64 KiB | `-m`; below that savings are noise and DB/thumbnail churn dominates |
| Block size | volume (advanced) | 128 KiB | `-b`; changing it resets the hashfile (told before saving) |
| Partial matches | volume (advanced) | off | `--dedupe-options=partial`; warning: CPU and hashfile size |
| Skip all-zero blocks | volume (advanced) | on | `--skip-zeroes` (sparse/preallocated files) |
| CPU / IO threads | volume (advanced) | CPU 2, IO automatic | `--cpu-threads`, `--io-threads` |
| Memory limit | volume (advanced) | 1 GiB (`MemoryHigh` 768 MiB) | Transient unit limits |

Not exposed: `--skip-readonly-subvols` (always on), `same` (always on), `only_whole_files`
(off), `-B`, replay mode, `-r` (always).

Always-on excludes (not editable, shown read-only): `<root>/#snapshot/`, `<root>/#recycle/`
(per share "Include the recycle bin": §5 Q3), `.snapshots/`, `@rynas-cache/`, `.replicas/`,
`.rynas-crypt/`, the Docker engine/containerd roots, local restic repositories of backup jobs,
`.*.rynas-part`, `.*.rynas-aside`, plus the names the hardware modules add (§5 Q4: the
Synology module `@eaDir/`, `@tmp/`, `.SynologyWorkingDirectory/`; the QNAP module
`.@__thumb/`, `.@__qini/`, `@Recycle/`; a module gives them only on its own hardware; the
core list names no other system).

### 3.4 Estimate before enabling

"Estimate" runs the same command **without `-d`** into the volume's hashfile (task
`storage.dedup-estimate`, same window/lock rules, "run now" asks until when), then reads
`--json`: "duplicates found: X GiB logical in N files (Y % of Z)", plus, per share with
snapshots, the expected date the space comes back. The hashes are reused by the first real
run (a run with paths replaces the stored job, `-d` included), so the estimate is not lost
work. Enabling a share without an estimate is allowed; the UI recommends it.

### 3.5 Schedule

- Task `TaskAction::Dedup { volume }` in the scheduler key, created, changed and removed
  with the volume's dedup settings (like `Backup { job }`); shown in Scheduler as managed by
  Storage. Its timer fires at the start of each contiguous block of the window; the run's
  deadline is the end of that block.
- Each run: check engine version → take the filesystem lock (§3.7) → optional awake check →
  `-R` for removed roots → `oans -d -r …` until done or deadline → `--json` → record →
  notify → schedule the compsize report (§3.9).
- "Run now" outside the window asks for an end time (default: end of the next window block,
  max 24 h); it never runs without a deadline.

### 3.6 Execution

The process runs in a transient unit (the `rynas-backup` runner pattern, `systemd-run --wait
--pipe`): `Nice=19`, `IOSchedulingClass=idle`, `CPUWeight=20`, `MemoryHigh`/`MemoryMax`,
`KillSignal=SIGINT`, `TimeoutStopSec=5min`, `RuntimeMaxSec=` = deadline, `BindsTo=rynasd`,
`ProtectSystem=strict` with `ReadWritePaths=` = the roots and the hashfile directory (a
read-only mount namespace would make every dedupe fail with `EROFS`),
`CapabilityBoundingSet=CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_FOWNER` (read every file;
dedupe needs write permission or ownership of the destination; to verify, §4.4),
`NoNewPrivileges`, `PrivateNetwork`. `IOSchedulingClass=idle` works only with BFQ; on
mq-deadline the run is limited by the window and the thread counts (research §3 caveat).

### 3.7 One run per filesystem, never next to other heavy work

A new rynasd **filesystem lock** keyed by the filesystem's superblock `major:minor` (none
existed: `storage_lock` serializes storage API calls, btrbk has per-job lockfiles only).
Holders: dedup, dedup estimate, `oans -R` and the hashfile reset; scrub (manual and
scheduled), replication transfers, restic backups, volume grow and replace, locking an
encrypted volume, subvolume delete. rynas has no balance or profile conversion action yet:
they take the lock when they come. The lock lives in rynasd's memory: at start rynasd stops
the runs a previous rynasd left (`systemctl stop 'rynas-dedup-*'`, SIGINT and their time to
save) before anything takes it.
Rules:

- dedup is the only **preemptible** holder: any other holder asking for the lock sends it
  SIGINT, waits for its exit (≤ 5 min grace), then proceeds; the dedup run is recorded as
  "stopped for <task>" and re-queued if its window is still open;
- dedup asking while another holder runs waits until it ends or the window closes (then
  "skipped: <task> was running");
- never two oans processes on one hashfile (the lock is the only guard: oans has none).

### 3.8 Outcomes

| Exit | Meaning | Task | Notification |
|---|---|---|---|
| 0 | Done | ok | `dedup.ok` (info) |
| 130/143 at deadline | Window ended, work saved | ok, "continues in the next window" | none |
| 130/143 preempted | Stopped for another task | ok, "stopped for <task>" | none |
| 2 | Done on less than asked (a share missing, unmounted or on another filesystem) | warning, names the roots | `dedup.partial` (warning) |
| 1 | Fatal (options, hashfile unreadable, DB write failed) | failed | `dedup.failed` (error); a second consecutive failure with a hashfile error resets the hashfile |
| `EAGAIN` lines | A send was running (should not happen with the lock) | counted | none |

### 3.9 Reporting

- Volume: reclaimed this run and in total (logical, oans), duplicates still reclaimable
  (`reclaimable_logical_bytes`), files tracked, skipped counts, history table (last 52
  runs), next window.
- Real figures: after a successful run, a low-priority `compsize` task per enabled share
  (only while the window is open and the disks awake) stores Disk usage / Uncompressed /
  Referenced: per-share "saved by dedup and copies" = Referenced − Uncompressed. Package
  `btrfs-compsize` is recommended with `rynas-oans`.
- Share list: a small dedup indicator and the saved figure in the share's detail; the
  explanation of shared data in the size column (§2).
- Notifications (category `storage`): `dedup.ok` (oneshot, info, off in the default rule),
  `dedup.partial` (warning), `dedup.failed` (error), with volume, shares, reclaimed bytes
  and the outcome per root.

## 4. UI, API, tests

### 4.1 UI placement

- **Storage › volume detail**: a "Deduplication" card next to Scrub (both are volume
  maintenance on one filesystem): state, window summary, last run, reclaimed total, buttons
  Estimate / Run now… / Stop / History / Settings (window grid and advanced options in a
  dialog). Engine not installed: the card offers the install.
- **Shares › share editor, General tab**, next to compression and NoCOW (they decide
  together whether dedup is possible): "Deduplicate this folder" with its disabled reasons,
  extra excludes, the snapshot/replication warnings and a link to the volume card.
- **Shares list**: indicator only. **Scheduler**: the task listed read-only ("managed by
  Storage"). **Task panel**: progress (phase, bytes, ETA). No dashboard widget in this phase.

### 4.2 Config and API sketch

```rust
// services key, inside Share (lives and dies with the share)
pub struct ShareDedup { pub enabled: bool, pub excludes: Vec<String> }

// new key `dedup` (domain in rynas-storage or a small rynas-dedup crate)
pub struct DedupConfig { pub volumes: BTreeMap<String, VolumeDedup> }   // key: volume label
pub struct VolumeDedup {
    pub window: String,          // 168 chars, '1' = may run
    pub only_if_awake: bool,
    pub min_file_size: u64,      // 65536
    pub block_size: u32,         // 131072
    pub partial: bool,
    pub skip_zeroes: bool,
    pub cpu_threads: Option<u8>,
    pub io_threads: Option<u8>,
    pub memory_max_mib: u32,
}
```

| Method | Path | Notes |
|---|---|---|
| GET | `/api/v1/storage/dedup` | engine (installed, version, supported), per volume: settings, enabled shares, running UPID, last run, totals, next window, warnings |
| PUT | `/api/v1/storage/dedup/volumes/{vol}` | staged config (pending/apply like every key); block size change → `hashfile_reset` warning |
| POST | `/api/v1/storage/dedup/volumes/{vol}/estimate` | `{until}` → task `storage.dedup-estimate` |
| POST | `/api/v1/storage/dedup/volumes/{vol}/run` | `{until}` → task `storage.dedup` |
| POST | `/api/v1/storage/dedup/volumes/{vol}/stop` | SIGINT |
| GET | `/api/v1/storage/dedup/volumes/{vol}/history` | from rynas state |
| DELETE | `/api/v1/storage/dedup/volumes/{vol}/hashfile` | reset (lock held), typed confirmation |
| POST | `/api/v1/storage/dedup/install` | task `apt.install` of `rynas-oans` |

Admin only, checked in rynasd too. Engine behind `trait DedupEngine { fn command(&self,
job: &DedupJob) -> Cmd; fn parse_progress(line) ; fn parse_report(json) ; fn outcome(exit)
}` with `Oans` as the only implementation.

### 4.3 Wire-level details to get right

- Excludes are absolute (`/<vol>/<share>/#recycle/`): user patterns are prefixed with the
  root; patterns with `!` or `..` refused; one oans run with several roots shares one exclude
  list, so every exclude must be anchored.
- Paths are passed after `--`; share names are already restricted (`[A-Za-z0-9._-]`).
- Progress lines are capped in length and rate before they reach the task log.

### 4.4 Tests (never real disks)

- **Unit, pure**: argv golden tests from config (options, excludes, roots, unit properties);
  progress and `--json` parsers on fixtures; exit-code mapping; window → timer blocks and
  deadlines (jiff, DST); exclude anchoring and refusals; disabled reasons per share kind.
- **Fakes**: a `DedupHost` fake that plays scripted NDJSON and exit codes (0, 1, 2, 130 at
  deadline, 130 preempted, crash) to test task state, history, notifications and the
  hashfile reset; a fake filesystem lock with a fake clock for preemption and waiting; UI
  demo data for every card state.
- **Integration, `packaging/test-dedup.sh`**: a privileged container (with `container=docker`
  and getties masked) on a **loop-mounted image file**, btrfs and XFS (`reflink=1`): known
  duplicate sets → reclaimed bytes; read-only snapshot and `#snapshot` bind mount untouched;
  `#recycle`, NoCOW and gocryptfs paths skipped; mtime/ctime unchanged after dedupe; SIGINT
  then resume; exit 2 with an unmounted root; a btrbk send running → lock preempts;
  capability set sufficient. Runs on every `rynas-oans` bump. Fixtures for the parsers are
  captured from this run.

## 5. Answers of the owner (2026-11-11)

The owner approved the design and the proposed answer to every question:

1. **One hashfile per volume** (per filesystem), not per folder: duplicates are found across
   shares, one run per filesystem.
2. **Local replicas and PC-backup targets are not deduplicated** (read-only, skipped by
   `--skip-readonly-subvols`; no separate dedup of a replica in this phase).
3. **`#recycle` excluded by default**, included per share ("Include the recycle bin",
   `ShareDedup::include_recycle`).
4. **Metadata folders of other NAS systems only through the hardware modules**
   (`Module::dedup_excludes`): the core stays generic, the Synology and QNAP modules add
   their own names (§3.3).
5. **"Only if the disks are already awake" on by default.**
6. **Default window: every night 01:00–06:00.**
7. **The `rynas-oans` package is attached to rynas releases** until there is an apt
   repository (§6.3).
8. **duperemove fallback removed** (a second `DedupEngine` may come once Debian ships 0.15).
9. **XFS folders and the system volume included from the start.**
10. **D-DEDUP row updated** in `../phase0/05-decisions.md`.

## 6. Implementation (2026-11-11)

### 6.1 Where things are

| Part | Place |
|---|---|
| API types, share switch, key `dedup` | `crates/rynas-api-types/src/dedup.rs` (`ShareDedup` in `Share::dedup`, `DedupConfig`, views), `TaskAction::Dedup { volume }` |
| Domain, window, excludes, engine, state | `crates/rynas-dedup` (`DedupDomain`, `window`, `excludes`, `engine::{DedupEngine, Oans, unit_properties}`, `state`) |
| Filesystem lock | `crates/rynasd/src/fslock.rs` (`FsLocks::global()`, key = superblock `major:minor`) |
| Service, runs, API | `crates/rynasd/src/dedup/` (`DedupSvc`, `host::{HostDedup, MemoryDedup}`) |
| Lock holders | scheduled and manual scrub, replication transfers, restic backups, volume grow/replace |
| UI | `crates/rynas-ui/src/pages/storage/dedup.rs`: card in the volume detail next to Scrub (system volume and other filesystems: Storage page), share editor switch, Scheduler entry read-only |
| Package | `packaging/oans/` (`versions.env`, `build.sh`, `check-upstream.sh`, `debian/copyright`) |
| Engine test | `packaging/test-dedup.sh` (privileged container, loop images) |

The API paths are `/api/v1/storage/dedup/jobs/{key}/…` (`{key}`: a volume label, or
`uuid.<filesystem uuid>` for a folder on a filesystem that is no rynas volume) instead of
`/volumes/{vol}`; the reset is `POST …/reset` with the key typed again. Table in the module
documentation of `rynas_api_types::dedup`.

### 6.2 Decisions taken while building

- **Heavy holders do not wait for each other**, only deduplication gives way to them: making
  scrub, replication and backup exclusive among themselves would change what they did before
  (a 72-hour scrub would hold replication back).
- **The end of a window is told by the clock**: `systemd-run --wait` returns 1 (not oans'
  130) when `RuntimeMaxSec` stops the unit; rynasd records "window ended" when the deadline
  has come, "stopped" after a stop, "gave way" after a preemption, whatever the exit code.
- **The scheduler task** fires at the union of the block starts (one `OnCalendar=`
  expression: days × hours) with `Persistent=false`; a run started outside the window (or
  while the job runs) only logs why it does nothing. The Scheduler page shows it read-only
  (no edit, enable, delete or run now there).
- **Estimate** without enabled shares looks at every share that can be deduplicated; its
  `--json` figures are kept as the job's estimate.
- **Scheduled shutdown**: deduplication never counts as busy (`dedup::gives_way`); the unit
  is stopped with SIGINT (`BindsTo=rynasd.service`) and resumes in its next window.
- **Stops are never lost**: a stop or a preemption asked while a run gets ready (folder,
  `-R`) keeps the engine from starting; once it runs, the stop is asked again every 5 s
  until the client returns (a stop sent while systemd-run creates the unit finds none). The
  end of the window is the unit's own monotonic clock (`RuntimeMaxSec` from its start).
- **No run with less than 5 minutes** of its window left, also after giving way.
- **Roots the hashfile holds** are kept in the state, from runs and estimates alike; the
  next run forgets (`-R`) the ones no longer in the job and keeps them for another try when
  that fails.
- **Repeats collapse**: skips for the same reason and the same failure without an exit code
  (engine missing or untested at a scheduled run) are one history entry with one
  notification.
- **A removed job** (apply) stops its run and deletes its hashfile and state.
- **Engine versions**: 1.14.1 up to 1.14.x are accepted (`engine::MIN`, `MAX_MINOR`); others
  are refused with `engine_unsupported` until a bump passed `test-dedup.sh`.

### 6.3 Package and install

`packaging/oans/build.sh` builds `target/oans/rynas-oans_1.14.1-1_amd64.deb` reproducibly
in the pinned `debian:trixie` image (verified tarball, `-O2 -march=x86-64-v2`, stripped,
`SOURCE_DATE_EPOCH` of the last commit of `packaging/oans`). "Install" in the card runs
`apt-get install rynas-oans` when an apt source has it, otherwise the pinned `.deb` from
`/var/lib/rynas/packages/` when its SHA-256 is one compiled into rynasd
(`dedup::host::PINNED_DEBS`, updated with each package release). The build packages come
from snapshot.debian.org at a pinned timestamp and the tarball must be GitHub's archive of
the pinned tag commit (`git get-tar-commit-id`), so the package is the same file on every
machine. The release publishes the
`.deb`, its checksum and the upstream tarball (corresponding source).

### 6.4 Verified with `packaging/test-dedup.sh` (2026-11-11, kernel 7.3, Debian 13 userland)

Run under rynas's unit properties and command line on loop images: known duplicates shared
(btrfs and XFS `reflink=1`); **mtime and ctime unchanged by `FIDEDUPERANGE` on btrfs and on
XFS** (restic does not re-read deduplicated files); the read-only snapshot keeps its
generation and the `#snapshot` bind mount is not entered; `#recycle` and a no copy-on-write
folder skipped; another user's 0600 file deduplicated with the reduced capability set; the
window end (SIGINT) and the next run going on; exit 2 for a missing root; the estimate
stores no `-d`.

### 6.5 Not done yet / needs real hardware

- Real figures per share from `compsize` after a run (§3.9) and the expected date when space
  comes back from snapshot retention; the UI shows the logical figure and a note.
- The first replication after a big run: duration and size are not yet recorded and
  compared (the warning is shown).
- A balance does not reset the hashfile yet; a future defrag action must warn (§2).
- NAS-class timings (Celeron, HDDs, spin-down with "only if awake"), BFQ vs mq-deadline,
  memory use on 4 GB: on the hardware.
- UI demo data for the card states (the dry system plays scripted runs instead).
