# Backup in rynas: rustic, restic and what to take from Backrest

**In short:** use **rustic_core as a library**, in a **separate worker**, with the **standard restic repository** as the reference format and **Debian's restic** as a safety net. Two conditions: **rynas must serialise maintenance per repository** and **cap the memory used by prune and check**. The facts behind the choice follow.

---

## 1. rustic: status checked in November 2026

| Item | Fact |
|---|---|
| CLI version | **0.11.4** (2026-08-18). 5 releases in 2026 (Feb, Mar, Apr, Jun, Aug). 0.11.0 had breaking changes |
| `rustic_core` | **0.13.0** (2026-08-16). A new minor version every 2–3 months (0.8 → 0.13 since September 2025). MSRV **1.91**. The README says: *"in an early development stage and its API is subject to change"* |
| Maintainers | Effectively one: **aawsome** (1228 commits on rustic, 195 on core), then simonsan (188/162). The rest are bots and occasional contributors. Regular commits, last on 2026-10-31 |
| Production readiness | The README says neither "beta" nor "production ready"; it calls restic *"older and more mature"*. The FAQ admits: *"Error handling is not perfect yet"* |
| License | Apache-2.0 OR MIT: compatible with rynas' AGPL-3.0-or-later |
| Debian | **Not packaged** (missing from trixie, forky and sid). Official signed static musl binaries for x86_64 and aarch64 |
| Format | *"identical repository format… access every repository with restic as well as with rustic"*. Supports v2 (compression) |

**Serious open issues to know before choosing:**

- **#1946 (P-critical, open):** two **overlapping prunes** with a forget in between can delete packs a later backup uses, leaving snapshots that cannot be restored. Confirmed by aawsome, fix under review (rustic_core PR #639). Only parallel prunes are affected.
- **#1731 (open):** when the `local` backend (for us, the USB disk) **runs out of space**, truncated index files remain and check, repair, prune and restore all fail. In a documented case **`restic repair index`** fixed the repository, which then worked again with rustic: concrete evidence that restic as a safety net makes sense. 0.11.3 already partly fixed writes on the local backend.
- **#1905:** a false alarm, retracted by the author.

**Useful features:**
- TOML configuration with profiles;
- `run-before/after/failed/finally` hooks at global, repository, backup and source level;
- **lock-free**, two-phase prune with `--keep-delete` at 23h;
- hot/cold repositories with warm-up;
- OpenDAL backends (S3, B2, Azure, GCS, WebDAV, built-in SFTP, OneDrive…) and rclone;
- `copy` to several destinations, `webdav` server to browse snapshots, `repoinfo`, Prometheus and OpenTelemetry metrics;
- snapshot delete protection;
- append-only option in the repository, enforced by the client: **not** a defence against an attacker holding the credentials.

Missing compared with restic: `repair packs`, `recover`, `restore --verify` and complete JSON output (#1953 still open).

**Interoperability, what to avoid:**
- Same format, but rustic **neither uses nor honours restic's locks**. The FAQ: *"never run `restic prune` against a repository a rustic command may be writing to"*.
- A custom chunker, non-default pack sizes and hot/cold leave the "openable with restic" perimeter. Avoid them, or verify them in CI.

**Memory, field data from a real homelab repository:**
- `rustic prune` with repack on a 5.7 TiB repository (28 million blobs) was killed by the OOM killer at **25 GB RSS**.
- `check --read-data-subset 2%` peaked at **8.3 GiB**.
- With 4 GB of RAM these are mandatory: staged `--max-repack`, frequent prunes (the backlog is what blows up memory) and `MemoryMax` in the unit.
- Other traps seen in practice:
  - sshd `MaxStartups` too low for many parallel SFTP connections;
  - the **share's recycle bin** keeping the packs deleted by prune;
  - **volume snapshots** on the destination keeping the space allocated;
  - `Persistent` timers firing at once when `OnCalendar` changes;
  - `--read-data-subset` also requiring `--read-data`.

## 2. restic as the reference

| | restic | rest-server |
|---|---|---|
| Version | **0.19.1** (2026-07-05). 0.18.0 added a chunker attack mitigation and S3 cold storage support | 0.14.0 (2025-05) |
| Debian trixie | **0.18.0** (forky and sid: 0.19.1) | `restic-rest-server` 0.13.0 |
| Maturity | Conservative project with several maintainers. Engine of Backrest and Zerobyte | `--append-only` prevents deletion and modification; `--private-repos` too |
| License | BSD-2 | BSD-2 |

## 3. Comparison with other products

| Product | Idea to take | Problem not to copy |
|---|---|---|
| **Hyper Backup** | Tasks with destination and versions. **Smart Recycle**: keeps the latest hourly, daily and weekly versions, or up to 7 custom duration/interval rules. Scheduled integrity check. **Hyper Backup Explorer** to restore without the NAS. App and configuration backup | Proprietary format |
| **Active Backup for Business** | Central console, global dedup, agents on PCs | Proprietary. For PCs rynas already has btrbk-nas |
| **Backrest** | see §4 | GPL-3, so no copied code. Repository password in clear text in `config.json` |
| **Zerobyte** | Modern UI on top of restic, remote sources (NFS, SMB, SFTP) | Still 0.x (v0.44.0, 2026-10-31) with many breaking releases, AGPL |
| **Kopia** | `before-snapshot-root` *actions* take a filesystem snapshot and the backup reads it. Quick/full maintenance with a **single owner** of the job | Format different from restic |
| **Borg / borgmatic / Vorta** | borgmatic runs backup, prune, compact and check in one cycle, with monitoring hooks | Borg 2.0 still beta (2.0.0b25, September 2026). 1.x format incompatible with 2 |
| **Duplicati** | Web UI, many destinations | History of corrupted local database and long rebuilds: not a reliability reference |
| **Proxmox Backup Server** | **Verify**, **Prune** and **GC** as separate jobs. Verify skips what was already verified and re-verifies after N days. Namespaces | Built for VMs and containers |

## 4. What rynas takes from Backrest

Backrest v1.14.1 (2026-07-12, 7.8k stars, GPL-3) bundles restic **0.19.1**.

**Data model**, from `proto/v1/config.proto`:
- **Repo**: URI, password, env, flags, `prunePolicy` (schedule + `maxUnusedBytes`), `checkPolicy` (schedule + subset or `structureOnly`), hooks, `autoUnlock`, `autoInitialize`, `guid` from `restic cat config`.
- **Plan**: repo, paths, excludes, iexcludes, schedule, retention, hooks, `backup_flags`, `skipIfUnchanged`.
- **Retention**: `keepLastN`, `keepAll`, or `TimeBucketedCounts` (hourly, daily, weekly, monthly, yearly, keepLastN).
- **Schedule**: disabled, cron with `L`, `#`, `W` extensions, or maxFrequencyDays/Hours. Clock UTC, local or relative to the last run.

**A common deployment problem:** Backrest on a NAS doing **only prune and check** of a repository whose backups are started by timers on a PC. The PC's lock and the NAS' prune do not see each other, and the backup → prune → check order holds only thanks to the schedule times. rynas must own the whole sequence per repository.

| Backrest | In rynas |
|---|---|
| Separate Repo, Plan and Policy (prune, check) | **Take** as Destination, Plan and Maintenance |
| Retention with hourly, daily, weekly, monthly, yearly bucket counts | **Take**, with a "Smart Recycle" preset and a preview of what gets deleted (`forget --dry-run`) |
| Hooks on `SNAPSHOT_START/END/ERROR/WARNING`, `PRUNE_*`, `CHECK_*`, `FORGET_*`, `ANY_ERROR` with `ON_ERROR_IGNORE/CANCEL/FATAL` | **Take** as typed events to D-NOTIFY (no per-plan webhooks) and command hooks with the same three error policies |
| Templates with `Plan`, `Repo`, `SnapshotId`, `SnapshotStats`, `Duration`, `Error` | **Take** the event fields |
| Operation history as a tree, log truncated at 32 KB with "View logs" | **Take**, but using the journal by invocation id (D-TASKS) and a typed summary |
| "Index snapshots" (import an existing repository) | **Take**: snapshots made by hand or by other hosts show up in the browser |
| Browse and restore from the UI, download | **Take**, inside the rynas file manager |
| Multihost (server/client, pairing tokens with TTL and max uses, permissions limited to repos or plans, shared repos) | **Phase 3, business**: multi-NAS console |
| Automatic restic download from GitHub with SHA256 check | **Do not take**: no binaries downloaded at runtime. Library pinned to a version + Debian package |
| Clear-text passwords in the configuration | **Avoid**: secrets encrypted per the D-CFGBAK/D-CRYPT model |

The most voted Backrest requests are already covered by the design:
- one plan to **several repositories** (#508);
- **restore to the original path** (#118);
- **parallel** execution (#221);
- **diff between snapshots** (#726);
- running as a non-root user (#476).

## 5. Proposal for the `backup` plugin

### Engine

**Recommendation:** `rustic_core` + `rustic_backend` behind a `BackupEngine` trait, run in the **`rynas-backup-worker`** binary: a separate process started by the D-TASKS `rynas-task-<id>.service` units, with `MemoryMax`, `Nice=19`, `IOSchedulingClass=idle` and `KillSignal=SIGINT`.

The worker sends typed progress and results to rynasd over JSON-RPC. A panic or an OOM does not take rynasd down: the isolation of an external binary, with a typed API instead of text to parse.

Why not restic alone: rustic uses fewer resources, is written in Rust like rynas, has hooks, OpenDAL backends and `copy` to several destinations. The risk is maturity, contained this way:

1. **One maintenance writer per repository.** rynas holds a per-repository lock: never two prunes at once, which closes #1946. Never `--instant-delete`. The lock must hold across NASes too, with a lease written in the destination or a declared owner as in Kopia.
2. **Mandatory "restic-compatible" profile:**
   - repository v2;
   - default chunker and pack sizes;
   - no hot/cold nor repository options, unless explicitly chosen with a warning;
   - a **CI test** that creates the repository with rustic_core and reads it with restic 0.18 (trixie's), and vice versa.
3. **Debian's restic as a safety net** (`Recommends: restic`):
   - a monthly `restic check`, always under rynas' lock;
   - a "Repair index (restic)" button;
   - in the disaster recovery guide, the line *"your backup opens with restic or rustic on any PC"*: the free equivalent of Hyper Backup Explorer.
4. **Dependency pinned to an exact version**, updated by hand after the test suite. MSRV to move from 1.90 to 1.91.
5. **Plan B:** if the rustic_core API changes too much, restic's CLI (`--json`) goes behind the same trait. Repositories, UI and plans stay the same.

### Model

- **Destination:**
  - types: USB (local backend, with the disk UUID and hot-plug detection), SFTP or rest-server on another NAS, S3, B2, Azure, GCS, WebDAV, rclone;
  - credentials and repository password encrypted;
  - **Test connection**, including a parallel SFTP test (`MaxStartups`), permissions and space;
  - warns if the destination share has the **recycle bin** or **snapshots** enabled.
- **Plan:**
  - sources = shared folders, docker stacks, rynas configuration;
  - **always from a read-only btrfs snapshot** made by `rynas-btrfs`, bind-mounted on a **fixed path** `/run/rynas/backup/<plan>/<share>`, so the parent stays stable (`as-path`);
  - excludes (cache presets, `#recycle`, `.snapshots`, `CACHEDIR.TAG`, `.nobackup`), time window, bandwidth limit, priority;
  - **several destinations**: backup to the first, then `copy` to the others.
- **Retention:**
  - Smart Recycle preset = `keep-hourly 24`, `keep-daily 30`, `keep-weekly 52`, `keep-monthly 24`, `keep-yearly 5`, or custom rules;
  - preview of what will be deleted;
  - "pinned" snapshots never deleted.
- **Maintenance per repository:**
  - forget after every backup;
  - **weekly prune** with `--max-repack` sized to the RAM (e.g. 2G with 4 GB);
  - weekly index-only **verify** and monthly `--read-data --read-data-subset` rotating over n/m, as PBS does: the whole repository is read in 12 months;
  - the backup → forget → prune → check sequence is driven by a queue, not by schedule times.
- **Restore:**
  - snapshot browser in the file manager;
  - restore in place, elsewhere, or download as zip/tar;
  - diff between two snapshots;
  - "Restore to a new share" for testing.
- **Configuration:** the default "rynas system" plan includes the `.rynasbak` (D-CFGBAK) and the stacks' compose files.
- **Immutability:**
  - rest-server `--append-only` on the remote NAS, with prune run **server-side** (remote rynas, "pull" and "hardened" model);
  - S3 Object Lock and versioning with retention longer than keep-delete: phase 3.
- **Keys:**
  - at init the **password is shown with an "I saved it" confirmation** and a recovery kit PDF;
  - the UI allows additional keys (`key add`), one per administrator.
- **Notifications (D-NOTIFY):**
  - typed events for backup ok/failed, plan overdue (no success for more than N for the schedule), verify failed, destination almost full;
  - firing/resolved only on state change;
  - tolerance on duration (the jitter lesson from healthchecks).
- **Dashboard:**
  - last backup and status per plan, next run;
  - repository space and dedup ratio;
  - "data not yet pruned";
  - **3-2-1** indicator: 3 copies, 2 media, 1 offsite, counting snapshots, btrbk and rustic.

**btrbk and rustic:** btrbk replicates btrfs to btrfs (NAS→NAS, btrfs USB, PC backup). rustic goes to everything that is not btrfs: cloud, SFTP, exFAT or ext4 USB. Both start from the same snapshot and share a per-filesystem lock (D-DEDUP).

```
Backup                                    [+ Plan] [+ Destination]
┌─ 3-2-1: ●●○  no offsite copy ──────────────── [Add cloud]────────┐
├──────────────┬──────────────┬─────────┬──────────┬───────────────┤
│ Plan         │ Destinations │ Last    │ Next     │ Status        │
│ Documents    │ USB-WD, B2   │ 02:10 ✓ │ 02:00    │ ● ok          │
│ Photos       │ NAS-office   │ yday ✗  │ 03:00    │ ● SFTP reset  │
│ System       │ B2           │ 01:00 ✓ │ 01:00    │ ● ok          │
├──────────────┴──────────────┴─────────┴──────────┴───────────────┤
│ Repository B2: 412 GiB · dedup 3.1× · to prune 18 GiB            │
│ Verify: 7/12 of the repo read · last check ok 11-01              │
│ [Browse snapshots] [Restore] [Verify now] [Recovery kit]         │
└──────────────────────────────────────────────────────────────────┘
```

### Phases

- **Phase 2:**
  - USB, SFTP, rest-server and S3/B2 destinations;
  - plans from snapshots, retention, maintenance queue, rotating verify;
  - browse and restore, System plan, notifications, dashboard, restic as safety net.
- **Phase 3 (business):**
  - multi-NAS in the style of Backrest's multihost;
  - central rest-server repository with `--private-repos`;
  - S3 Object Lock, compliance reports, scheduled automatic restore tests, cold storage (rustic hot/cold);
  - per-tenant quotas.

## 6. Mistakes to avoid

1. Overlapping prunes or `--instant-delete` with parallel access (#1946).
2. Mixing `restic prune` and rustic writes without rynas' lock.
3. A USB destination filling up: check space before the backup and keep a reserve (#1731).
4. Rare prunes with frequent backups: backlog builds up, then OOM. On 4 GB limit the RAM and repack in stages.
5. Recycle bin or snapshots enabled on the destination share, keeping deleted packs.
6. Repositories read from paths that change at every snapshot: the parent is lost and everything is re-read each time.
7. Clear-text repository password (Backrest does it, as do common `/etc/.restic.env` setups).
8. Write and delete credentials on the same machine that could be compromised.

## 7. Implemented design (first wave, 2026-11-08)

What is in the code now, and where it deviates from §5 (with the reason).

### Pieces

| Piece | Where | What |
|---|---|---|
| Worker | `crates/rynas-backup-worker` (own workspace and `Cargo.lock`, MSRV 1.91) | `rustic_core` 0.13.0 + `rustic_backend` 0.7.0, pinned exactly. One operation per process: a `WorkerRequest` (JSON) on stdin, `WorkerEvent` lines on stdout. Ops: `test`, `init`, `backup`, `forget`, `prune`, `check`, `snapshots`, `ls`, `restore`, `dump`, `stats` |
| Protocol and config types | `rynas-api-types::backup` | jobs, destinations, retention, maintenance, views, requests, worker wire types |
| Domain, runner, status | `crates/rynas-backup` (no rustic) | `backup` key (validate only, renders no file), destination → rustic backend string, worker launcher with streaming and cancel, `<state>/backup/<job>.json` |
| Daemon | `crates/rynasd/src/restic_backup.rs` | routes, secrets, run sequence, tasks `backup.run`, `backup.check`, `backup.restore`; scheduler action `TaskAction::Backup` (`rynasctl job run backup-<job>`) |
| UI | `crates/rynas-ui/src/pages/backup/{mod,restic}.rs` | page **Backup** (`/backup`) with tabs; this work is the tab *Another NAS or cloud* |

rustic is never linked into rynasd, rynas-web or the UI: they only speak the JSON protocol.

### Destinations (verified in `rustic_backend` 0.7.0)

`rustic_backend` offers four backend types: `local`, `rest`, `rclone` and `opendal:<service>`. Its default features already turn on OpenDAL with b2, s3, sftp, gdrive, onedrive, dropbox, webdav, azure, gcs and more ([choose.rs](https://github.com/rustic-rs/rustic_core/blob/main/crates/backend/src/choose.rs), [services](https://rustic.cli.rs/docs/commands/init/services.html)).

| rynas destination | rustic | Notes |
|---|---|---|
| USB or local disk | `local` | a folder |
| REST server (another NAS) | `rest:https://user:pw@host/path` | `rest-server --append-only` on the other side refuses deletes and overwrites: the "vault" model. `--private-repos` limits each user to `/user/…` ([rest-server](https://github.com/restic/rest-server)) |
| SFTP | `opendal:sftp` | key only (OpenDAL's SFTP has no password login). rynas generates an ed25519 pair (`ssh-keygen`), the public key goes into the server's `authorized_keys`; host key trusted on first use (`known_hosts_strategy=add`) |
| S3 compatible | `opendal:s3` | `disable_config_load=true`: never reads `~/.aws` |
| Backblaze B2 | `opendal:b2` | native API |
| rclone (Google Drive, OneDrive…) | `rclone:remote:path` | rustic starts `rclone serve restic --addr localhost:0` with a random user and password and talks REST to it (not `--stdio` as restic does); needs rclone ≥ 1.52.2 ([rclone.rs](https://github.com/rustic-rs/rustic_core/blob/main/crates/backend/src/rclone.rs)). The remote's `rclone.conf` section (with its OAuth token, made with `rclone config` on a PC with a browser) is a sealed secret, written to a 0600 file as `RCLONE_CONFIG` |

Bandwidth limit: OpenDAL `throttle` (S3, B2, SFTP) or `rclone serve --bwlimit`; none for local and REST.

### Secrets

- Repository password: generated (32 characters, ~190 bits, `getrandom`) or given. Shown **once** in the create answer; the UI keeps the dialog open until "I have saved the password", with copy and download.
- Every secret of a job (repository password, REST/S3/B2 secret, rclone configuration, SFTP private key) is sealed with rynasd's age key (`<state>/notify/secret.key`, as the UPS network passwords) into `BackupJob::sealed` (base64). Views never include it; a `.rynasbak` export carries the sealed blob, the key only with a passphrase.
- The worker gets secrets on stdin only; files a backend needs are 0600 in a private folder removed at exit; every line it prints has secret values (including a password inside a REST URL) replaced by `***`. Tested.

### Run sequence (`backup.run`)

1. `init` (creates the repository when missing; with an existing one the password must open it).
2. Stop the chosen Docker stacks (`docker ps --filter label=com.docker.compose.project=<stack>`, then `docker stop` / `docker start` of exactly those containers).
3. For each source that is a btrfs subvolume, a read-only snapshot `<parent>/.rynas-backup/<job>-<n>` (ioctl, no CLI); the stacks start again as soon as all snapshots exist.
4. One restic snapshot per source with `as_path` = the source's real path. **Deviation from §5**: no bind mount on `/run/rynas/backup/…`: `as_path` gives the same stable path (the parent is found, restores go back to the real place) without a mount namespace. Tags `rynas`, `rynas-job:<job>`.
5. `forget` with the job's retention, per path and only for snapshots tagged with this job.
6. `prune` when due (default every 7 days; never `--instant-delete`, packs removed after 23 h).
7. `check` when due (default every 7 days, reading `check_read_percent` % of the data, default 2 %). **Deviation**: a random subset per check instead of the n/12 rotation; the worker supports both (`ReadSubsetOption::IdSubSet`), the rotation needs the state of which slice is next and is left for the next wave.
8. `stats` → repository size in the job status. Temporary snapshots deleted, stacks restarted even when the backup fails.

Configuration source: the `.rynasbak` export, encrypted with the repository password, backed up as `/rynas-config`.

### Worker process (D-BACKUP)

- Production: `systemd-run --wait --pipe --collect --service-type=exec --unit=rynas-backup-<job>-<ts>-<n>` with `MemoryHigh=1536M`, `MemoryMax=2G` (`RYNAS_BACKUP_MEMORY=<high>:<max>`), `Nice=19`, `IOSchedulingClass=idle`, `CPUSchedulingPolicy=batch`, `KillSignal=SIGINT`, `TimeoutStopSec=60`, `PrivateTmp=yes`. Outside rynasd's 640 MB unit and its sandbox (restores write anywhere).
- Cancel: the task's cancel stops the unit (`systemctl stop`, SIGINT); `--dry-system`/tests: SIGINT to the child.
- **Resumable**: rustic saves the index of uploaded packs every 5 minutes (`MAX_AGE` in its indexer), so an interrupted backup does not upload that data again.
- **One worker operation per job at a time** (backup, check, restore): the per-repository maintenance lock of D-BACKUP inside one NAS. Reads (snapshots, browse, download, preview) run alongside, as the restic format allows.
- Progress: rustic's `ProgressBars` → `Step` and throttled `Progress` events → the task's log and percentage. rustic_core has no cancel API ([progress.rs](https://raw.githubusercontent.com/rustic-rs/rustic_core/main/crates/core/src/progress.rs)): ending the process is the cancel.

### API (`/api/v1/backup/restic/…`, every route administrator-only, reads too)

| Method | Path | |
|---|---|---|
| GET, POST | `/jobs` | list (`BackupJobView`), create (`BackupJobRequest` → `BackupJobCreated` with the generated password once); stages `backup` + `scheduler` |
| GET, PUT, DELETE | `/jobs/{name}` | the repository stays on delete |
| POST | `/test` | `TestResult`: `empty`, `opens` (n versions), `locked`, `failed` |
| POST | `/ssh-key` | a new pair for SFTP; the private key waits 1 h in memory for its job |
| POST | `/jobs/{name}/run`, `/check`, `/restore` | tasks (202) |
| GET | `/jobs/{name}/snapshots`, `/browse?snapshot=&path=`, `/download?snapshot=&path=`, `/preview-retention` | reads (110 s); downloads up to 64 MiB, larger files are restored to a folder |

Notifications: the job's scheduler task notifies on failure by default (category Backup, event family `backup.job`).

### UI

Ideas taken: the wizard order of Duplicati (what, where with "Test now", when, versions, encryption — [docs](https://docs.duplicati.com/getting-started/set-up-a-backup-in-the-ui)), Backrest's snapshot browser with restore and download ([backrest](https://github.com/garethgeorge/backrest)), TrueNAS TrueCloud's "use snapshot" option ([TrueCloud tasks](https://www.truenas.com/docs/scale/25.10/scaletutorials/dataprotection/truecloudtasks/)) — with full GFS retention where TrueCloud has only "keep last" ([request not accepted](https://forums.truenas.com/t/not-accepted-truecloud-backup-allow-to-keep-more-snapshots-per-week-per-month-per-year/26678)) — and the scheduled integrity check with data read-back of Hyper Backup ([spec](https://www.synology.com/dsm/7.3/software_spec/hyper_backup)). Kopia's per-path policies ([getting started](https://kopia.io/docs/getting-started/)) were left out: one job, one policy.

### Open for the next waves

- n/12 rotating read-data check, "data not yet pruned", 3-2-1 indicator counting btrbk replicas, recovery kit PDF, `key add` per administrator.
- Multi-destination `copy`, lease-based lock across NASes, restic CI interop test (rustic ↔ restic 0.18), monthly `restic check` safety net and "Repair index (restic)".
- Destination space check before the run (#1731), warning when the destination share has a recycle bin or snapshots.
- Browse in the file manager; snapshot diff; download of folders as an archive.

## Sources

- https://github.com/rustic-rs/rustic · /releases · issues [#1946](https://github.com/rustic-rs/rustic/issues/1946), [#1731](https://github.com/rustic-rs/rustic/issues/1731), [#1905](https://github.com/rustic-rs/rustic/issues/1905), [#1953](https://github.com/rustic-rs/rustic/issues/1953)
- https://github.com/rustic-rs/rustic_core · https://crates.io/crates/rustic_core
- https://rustic.cli.rs/docs/FAQ.html · https://rustic.cli.rs/docs/comparison-restic.html · https://github.com/rustic-rs/rustic/blob/main/config/README.md
- https://github.com/restic/restic/releases · https://github.com/restic/rest-server
- https://packages.debian.org/search?keywords=restic
- https://github.com/garethgeorge/backrest (releases, `proto/v1/config.proto`, `docs/src/docs/{operations,multihost}.md`) · https://garethgeorge.github.io/backrest/docs/hooks · https://garethgeorge.github.io/backrest/introduction/getting-started
- https://github.com/nicotsx/zerobyte · https://github.com/kopia/kopia · https://kopia.io/docs/advanced/actions/ · https://github.com/borgbackup/borg/releases
- https://pbs.proxmox.com/docs/maintenance.html · https://www.synology.com/en-us/dsm/7.4/software_spec/hyper_backup
