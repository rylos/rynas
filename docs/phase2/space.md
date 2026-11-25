# Space: what takes room and how to reclaim it (D-SPACE)

Status: phase A (accounting, page, previews, deletions) and phase B (guardrails) implemented
2026-11-24. Code: `crates/rynasd/src/space/`, `crates/rynas-btrfs/src/extents.rs`, types in
`crates/rynas-api-types/src/space.rs`, UI `crates/rynas-ui/src/pages/storage/space.rs`.

## 1. Why

A NAS on btrfs fills up in ways the share sizes do not show: snapshots of every job, app stack
snapshots, system restore points, PC backup folders, replicas kept for a target that is gone,
copies left by rollbacks and restores, recycle bins nobody empties, Docker layers, the journal.
Before this, rynas had a "volume full" notification at 80/90/95 % and nothing that said *what*
took the room or what could go without risk. Several sources were never cleaned at all (see §4).

## 2. What the best NAS software does (summary of the research)

- **Smart snapshot space management of a NAS system**: when free space falls below a trigger,
  the oldest snapshots go first until free space is back above a second level (hysteresis);
  snapshots marked "keep permanently" are never removed.
- **snapper**: per-config algorithms (number, timeline, empty pre/post pairs), `SPACE_LIMIT`
  and `FREE_LIMIT`, snapshots marked important kept longer. It computes "freed if deleted" for a
  set with a level-1 qgroup that contains the set.
- **A desktop backup tool**: thins old backups automatically, never the newest.
- **A desktop OS's storage cleanup**: per-category ages (recycle bin, downloads, temporary
  files).
- **A NAS system's storage analyzer**: history and trend of volume usage, but it ignores
  snapshots, which is the main complaint of its users.
- **Docker** never removes named volumes by itself; dangling images and build cache grow until
  someone prunes them.
- **journald** defaults to `SystemMaxUse` = 10 % of the filesystem capped at 4 GiB.

btrfs facts that shape the design:

- the "exclusive" bytes of one snapshot cannot be summed over a set: data shared only among the
  members of the set is "shared" in each of them;
- what deleting a **set** frees is the exclusive size of a level-1 qgroup containing exactly
  that set, or an extent walk over the set;
- `btrfs filesystem du -s <paths…>` reports per argument ("set shared" is the union of the
  shared extents inside that one argument), and it is slow (minutes on HDDs with many
  subvolumes);
- qgroups slow down transaction commits and balance with many snapshots (to be avoided above
  ~100 snapshots per source) and simple quotas (squota) cannot answer "freed if deleted";
- freed space appears asynchronously (the cleaner thread); `btrfs subvolume sync` waits for it.

## 3. Decisions

| Topic | Decision |
|---|---|
| Place in the UI | No new sidebar entry: a **Space** tab in Storage (`/storage/space`), next to Overview and Drives. Links from Snapshots, Docker and Backup. |
| Default guardrail | **Notify only.** Automatic thinning is opt-in per volume ("Free space automatically"). |
| "Freed if removed" | rynas's own extent walk (§5.2); qgroup level-1 numbers only when quotas are already on, full and consistent and a level-1 group matches the set exactly. rynas never enables quotas. |
| Scan | Daily and on demand, in a thread at idle I/O priority and nice 19; volumes whose disks sleep are left for later; 45 min budget per volume. |
| Deletions | Always a task after an explicit selection, discovery runs again first, any protected path refuses the whole request; the task ends with `btrfs subvolume sync`. |
| Protections | Never: the newest snapshot of a job, stack or the restore points; what a replication target still needs (its latest replica and every newer snapshot; all of them while an enabled target has none or its state file is missing or unreadable); the newest complete PC backup per subvolume; the restore point the system runs from; a pinned snapshot; a writable subvolume. |
| Pins | A persisted set of paths (`<state>/space/pins.json`). Honoured by Space, the thinning, stack pruning, restore point pruning, share job runs (btrbk `--preserve-snapshots`, rynas prunes the rest) and PC backups (the NAS's retention, and the PC's own deletes through the key line's `--pinned` names). |
| Identical app snapshots | The stack folder's subvolume generation is not newer than the transaction that created the snapshot (`generation <= otransid`, btrbk's "onchange" rule) **and** the images the snapshot recorded are the ones the stack runs now (§5.3). |
| Navigation | "Security" becomes a sidebar entry after Users; the command palette (Ctrl+K) indexes inner tabs and sections (static deep-link table with i18n labels). |

## 4. Inventory (what was never cleaned)

| Source | Where | Before | Now |
|---|---|---|---|
| Share job snapshots | `<parent>/.snapshots/<job>/<job>.<ts>` | btrbk GFS | listed and measured; deleting a job left its snapshots as orphans → **orphan source** + review item |
| App stack snapshots | `/<vol>/docker/.snapshots/<stack>/<id>` | pruned only before updates | **pruned after manual and before-rollback snapshots too** (never the one just taken, never the rollback target, never pinned) |
| Failed stack rollback | `stacks/.<stack>.rynas-old` | kept | review item |
| System restore points | `/.snapshots/rootfs/rootfs.<ts>` | hidden while disabled | **listed and deletable while disabled**; pinned ones never pruned |
| System rollback copy | top-level `@rootfs.rynas-rollback-<stamp>` | kept forever | review item, deleted by subvolume id after re-listing (never the running root) |
| Share rollback / restore copies | `<subvol>.rynas-rollback-<ts>`, `<name>.rynas-before-restore-<ts>` | kept, no API | review items (walk depth 4) |
| Syncthing adoption copy | `<folder>.before-adopt-<ms>` | kept | review item |
| Replicas | target folders | removed target left replicas | `.replicas/<x>` folders no configured target owns → "received replica", fully protected (§7); replicas another NAS sends over SSH into other folders are not recorded by the receiving side and show as "received, owner unknown" (§6) |
| PC backups | registered folders | own retention | measured; folders of received snapshots nobody owns → "received, owner unknown", newest of each line protected (§7) |
| Kernels | `linux-image-*` | never removed | review item (never running, newest of each series or held) |
| Recycle bins | `#recycle` of shares and homes | cleanup only from a manual task | **daily cleanup by default** honouring `retention_days` (0 = never), homes included: a change of behaviour on update, to be named in the release notes |
| Thumbnails | `/var/cache/rynas/files/thumbs/<uid>/` | no eviction | **LRU eviction to a cap** (default 2 GiB) + safe action |
| Journal | `/var/log/journal` | no limit from rynas | **journald drop-in** `SystemMaxUse` = min(2 GiB, 5 %) by default + safe action |
| App catalogues | `<state>/catalog/<serial>/` | kept | safe action keeps current and previous |
| Docker | engine data | manual prune of unused images | safe action (dangling images nothing needs + build cache), review (tagged unused images, named volumes no container uses and no known stack labels) |

## 5. Design

### 5.1 Accounting

`rynasd::space` scans each volume (the system volume is the root filesystem, walked from its
folder `/srv/rynas/system` only):

- totals from `statvfs`;
- **snapshot sources** (configured: share jobs, system restore points, app stacks, PC backup
  folders; found by a walk of depth 4 and at most 20 000 folders: orphaned job folders,
  unowned received snapshots), each with count, oldest, newest, "≈ freed if all removed",
  "freed at least", the method, growth against 7 days ago (daily history per source, 90 days,
  `<state>/space/history.json`), its own retention count and the budget of the settings;
- **categories**: snapshots, recycle bins (shares and homes on the volume), Docker (engine
  `system df` on the Docker volume), system (journal, apt cache, thumbnails, task logs; root
  only), leftovers, data (the rest);
- **safe actions** with dry-run numbers and the **review list**.

The answer is cached in `<state>/space/<volume>.json` and `<state>/space/_global.json`;
`GET /api/v1/space` reads them and the live totals.

### 5.2 How "freed if removed" is computed

`rynas_btrfs::extents::measure(set, keep)` reads the subvolume trees, not the files
(`measure_tree`):

1. `BTRFS_IOC_TREE_SEARCH_V2` (root privileges) over each subvolume's tree gives every
   `EXTENT_DATA` item: each file range and the data extent it points to (`disk_bytenr`,
   `disk_num_bytes`); inline data and holes are left out;
2. the unit is the **whole extent**, deduplicated by its start: btrfs frees an extent only when
   no file range anywhere still references any part of it;
3. the distinct extents of the set go into one sorted list (8 bytes each); the members that
   **stay** (`keep`) are searched too and mark the extents of the list they reference;
4. freed = the bytes of the extents left unmarked.

No file is opened and no back reference is walked (what makes `btrfs filesystem du` and the
`FIEMAP` shared flag slow: on a J4125 with disks, `du -s` of one PC backup snapshot with 3.3
million files took more than 10 minutes), so each subvolume costs one pass over its metadata.

For a whole source the set is all its snapshots and `keep` is the live subvolume. For a
preview of a selection, `keep` adds, in each **line** of snapshots of the source (the name
before the time: `@` and `@home` of a PC are two lines; names without a time are one line), the
kept snapshot right before and right after every run of selected ones. A line is a linear
history, so an extent lives in a contiguous run of its snapshots: if neither neighbour of the
run references it, no farther snapshot does either.

Memory is bounded (8 Mi distinct extents for the set, under 128 MiB with the batch not yet
deduplicated, plus one bit per extent for the marks; more is "not measured") and so is time
(the scan budget per volume; 5 minutes for a preview, the task's cancel). A preview past its
time answers "not measured" (`timed_out_minutes`), never a partial number; the task log has a
line per subvolume searched.

Limits, shown as "≈": data the set also shares with subvolumes outside `keep` is counted as
freed — deduplicated or reflinked into another share, or, for a preview, reflinked or
deduplicated into a snapshot of the line that is not a neighbour of the selection (for example a
`btrfs receive` whose stream cloned from an older snapshot than its parent). These are
over-estimates. Not counted,
so the number is low by them: the metadata only the set holds (its own tree blocks and inline
data of small files). Sizes are on-disk (compressed extents count what they take). The tree
search knows no reference counts, so it gives no "at least" number.

When the tree search cannot be used at all (not root, a member that is not a subvolume top
directory, an old kernel), the older walk `measure_set` runs instead: `FS_IOC_FIEMAP` on every
regular file (like `du`), never following links or entering nested subvolumes; extents without
`FIEMAP_EXTENT_SHARED` are freed for sure ("at least"), the shared extents of the set are merged
into a physical range set `A`, those of `keep` clipped to `A` into `K`, and freed = exclusive +
|A| − |A ∩ K| (1 Mi ranges per range set, peak under 128 MiB). Its extra limits: an extent of
which the set and the kept members reference different, non-overlapping parts is counted
although btrfs frees an extent only when no part is referenced; lengths are logical
(compressed extents count their uncompressed size).

When quotas are on, full and consistent, the sum of the level-0 exclusive sizes is the "at
least" number, and a level-1 group whose children are exactly the set (snapper creates those)
gives the exact number without a walk (`btrfs qgroup show -c --raw`).

### 5.3 Identical app snapshots

A stack snapshot is identical to the stack folder now when the folder's subvolume generation
(the transaction of its last change) is not newer than the transaction that created the
snapshot (`generation <= otransid`). This is btrbk's `snapshot_create onchange` rule; it is
conservative (any change, even metadata, makes the folder "changed"), so it can only keep a
snapshot that could have gone, never remove one that differs. `btrfs subvolume find-new` was
rejected: it misses deletions and renames. The newest snapshot stays protected anyway.

The folder is only half of an app's rollback point: the snapshot's `<id>.json` records the
images (service and image id) the services ran. A snapshot whose recorded images differ from
the ones the stack runs now is the way back to the older images, not "identical"; when the
engine does not answer (images unknown) or the metadata is missing, nothing is identical.

### 5.4 Deleting

- `POST /api/v1/space/preview {paths}` → task `space.preview`; `GET /api/v1/space/previews/{id}`
  gives what can go, what is refused (with the protection) and the numbers for the set
  (or `timed_out_minutes` when measuring took longer than the preview's 5 minutes).
- `POST /api/v1/space/delete {paths, confirm}` (confirm = number of paths) → task
  `space.delete`: discovery runs again, any refused path refuses the whole request; share job and
  restore point snapshots are deleted under btrbk's job lock, PC backup snapshots through the PC
  backup service (its folder lock and rules), stack snapshots with their metadata; then
  `btrfs subvolume sync` on each volume touched ("being freed by the filesystem").
- `POST /api/v1/space/review/delete {ids, confirm}` → task `space.review-delete`: each id must
  still be in a fresh review list; folders are deleted by their exact verified path (never from a
  listing of children), a top-level system rollback copy by subvolume id after re-listing it.
- `POST /api/v1/space/actions/{kind}` → task `space.action`.

### 5.5 Guardrails (phase B)

Configuration key `space` (`SpaceSettings`, staged and applied like every domain):

- `space.low_free`: free space below `low_free_pct` (10 %) opens it, at or above `recover_pct`
  (15 %) it resolves (`space.free_ok`); the state survives restarts. Checked every 5 minutes.
- `space.snapshots_growth`: weekly, when a volume's snapshots grew more than `growth_gib`
  (50 GiB) or `growth_pct` (25 %) in 7 days, with the top three sources.
- **Free space automatically** (`auto_free`, per volume, off by default): while the volume is
  low, the oldest unprotected snapshots of share jobs, app stacks and restore points are
  deleted one at a time (sources over their budget first), with `btrfs subvolume sync` and a
  free-space reading after every deletion, until free space is back above `recover_pct` (at
  most 50 per pass, 15 minutes per pass). Each deletion takes the volume claim, discovers that
  volume only and gives the claim back right after, so replication, receives and snapshot runs
  get their turn between deletions; the day's count is written after every deletion. PC
  backups, orphans and replicas are never thinned automatically. Each deletion is logged; one
  `space.auto_freed` notification per pass lists them. A pass that finds nothing more to remove
  or fails opens one problem per volume (`space.auto_free_stuck`, fixed key, at most one per
  hour, resolved quietly when free space is back) and the next pass waits an hour.
- **Suspension on anomalous growth**: when the weekly growth check finds a volume's snapshots
  grew more than its limits **and** more than 3 times the median of the 4 weeks before (counted
  as at least 1 GiB), the automatic cleanup of that volume stops (`space.thin_suspended`): a
  mass change such as ransomware encrypting a share makes the newest snapshots large and the
  oldest ones the clean copies. The next weekly check that is not anomalous resumes it
  (`space.thin_resumed`).
- **Budgets** per source (`max_count`, `max_age_days`, `max_bytes`): shown in the sources table,
  "over budget" flagged; the thinning takes the excess first.
- **journald**: drop-in `/etc/systemd/journald.conf.d/50-rynas-space.conf`,
  `SystemMaxUse` = min(2 GiB, 5 % of the journal's filesystem), at least 64 MiB; a set value in
  MiB, or 0 for journald's own default. On a change journald is restarted (its streams survive
  in the file descriptor store) and the journal cut with `journalctl --vacuum-size`.
- **Recycle bins**: daily cleanup of the shares' and homes' `#recycle` honouring
  `retention_days` (on by default, `recycle_daily`).
- **Thumbnails**: daily LRU eviction to `thumbs_cap_mib` (2048).

### 5.6 API

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/space` | — | `SpaceOverview` {volumes: [`VolumeSpace`], safe_actions, review, scanning, scanned_at_ms?} |
| POST | `/api/v1/space/scan?volume=` | — | 202 `space.scan`; 409 `busy` while one runs |
| POST | `/api/v1/space/preview` | `SpacePreviewRequest` {paths} | 202 `space.preview` |
| GET | `/api/v1/space/previews/{task_id}` | — | `SpacePreview` {paths, refused, freed_bytes?, freed_at_least_bytes?, method, done, error?} |
| POST | `/api/v1/space/delete` | `SpaceDeleteRequest` {paths, confirm} | 202 `space.delete`; 400 `confirmation_required` |
| POST | `/api/v1/space/actions/{kind}` | — | 202 `space.action`; kinds `journal-vacuum`, `apt-clean`, `docker-dangling`, `recycle-bins`, `thumbnails`, `catalog-serials`, `identical-app-snapshots` |
| POST | `/api/v1/space/review/delete` | `ReviewDeleteRequest` {ids, confirm} | 202 `space.review-delete` |
| GET | `/api/v1/space/pins` | — | `PinnedSnapshots` {paths} |
| POST | `/api/v1/space/pin`, `/api/v1/space/unpin` | `PinRequest` {path} | 204; 400 `invalid_value` field `path` (not a read-only snapshot) |
| GET, PUT | `/api/v1/space/settings` | `SpaceSettings` | the effective settings; PUT stages key `space` |

Administrators only (also checked in rynasd). Event codes: `space.low_free`, `space.free_ok`,
`space.snapshots_growth`, `space.auto_freed`.

## 7. Data-safety review (2026-11-24)

- **Own prune of jobs with pins**: the dry run reads the job's configuration without its replica
  target sections; only `delete_snapshot` transactions whose `target_url` parent is exactly the
  job's snapshot folder count; never the latest replica of an enabled target nor anything newer
  (all of them while a target has none); a failed prune only warns, after the snapshot was taken
  and before the replication that follows.
- **Received snapshots without a configured owner** are "received, owner unknown" (rynas keeps no
  record of removed PCs or targets, so it never calls them that); the newest of each line
  (`<name>.<time>`) is kept; deleting is refused while a `btrfs receive` writes into the folder
  (a process `btrfs … receive` with an argument or working folder inside it). `.replicas/<name>`
  folders without a local target are replicas another NAS sends here: protected.
- **An unreadable `snapshots` or `shares` key** stops discovery (scan, previews, deletions fail)
  instead of making live job folders look deleted.
- **Leftovers** are never a configured share, job subvolume, app folder, PC or replica folder,
  nor a folder holding one; a plain folder is removed only when no subvolume (inode 256 or
  another device) or mount point is below it, checked again on the way down; restore copies that
  are files are listed too.
- **Pins** go only with their snapshot deleted by rynas, or when the volume is mounted and the
  snapshot's folder is there (on that filesystem) without it.
- **Automatic cleanup**: `btrfs subvolume sync` and a free-space reading after every deletion; a
  pass stops when 5 deletions freed less than 1 GiB together, then pauses 24 hours; at most 100
  deletions per volume and day; every pass is logged and notified with why it stopped.
- **Claims**: every Space deletion first asks the scheduler's lock table and is refused while a
  replication, receive, scrub, backup or snapshot run holds the volume, then holds the volume in
  the filesystem lock; app snapshots also take the stack's busy mark.
- **Extent walk**: each range set is capped (1 Mi ranges, peak under 128 MiB) and a measurement
  with any unreadable file is "not measured".
- **journald**: rynas reads every file journald reads, as journald does: `journald.conf` (or
  the vendor one in `/usr/lib/systemd/`), then the `*.conf` drop-ins of
  `/etc/systemd/journald.conf.d`, `/run/systemd/journald.conf.d`,
  `/usr/local/lib/systemd/journald.conf.d` and `/usr/lib/systemd/journald.conf.d` in lexical
  order of their names (the first folder hiding a name in the later ones), before and after
  rynas's own. An administrator's `SystemMaxUse=` anywhere in them wins: rynas writes no
  drop-in, the Space tab shows it; the journal action cuts to the value journald reads now and
  never below the administrator's, never to an unapplied default.
- **Docker volumes**: a volume labeled with the compose project of a stack rynas knows (its own
  stacks, adopted ones) is never in the review list; a removal reads `docker system df` again
  right before each `docker volume rm` and stops when the volume is in use or labeled
  meanwhile. Removing a volume deletes the app's data, which the review list says.
- **Identical app snapshots**: checked on a real btrfs (Debian 13, kernel 7.3): a read-only
  snapshot does not bump the source's generation, so `generation <= otransid` stays the rule.

## 6. Open items

- Leftovers deeper than the walk (depth 4) are not found; restore copies inside deep folders of
  a share are only seen once the restore task records them (not done).
- Removed-PC detection is by "received snapshots nobody owns"; a folder of received snapshots
  made by hand outside rynas shows up the same way (review only, never automatic). Replicas
  another NAS pushes over SSH outside `.replicas/` look the same: the receiving side keeps no
  record of them yet.
- The extent walk of a big live share is long on HDDs; the 45 min budget leaves the remaining
  sources "not measured" until the next scan.
