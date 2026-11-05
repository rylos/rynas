# Quality review — 2026-11-06, wave D

Scope: `git diff ab97a3e..782ef09` (generic NUT UPS backend + `rynas-nut`, coded storage plan texts,
UI neutrality sweep, shares: `#snapshot` folder / delete with data / unshared folders, shared chassis,
Save = only the dialog's keys (`ApplyRequest.keys`), disk tuning, bounded rynasd shutdown,
notifications outside-click / resolved icon). Rules `G*`, `A*` in [`guidelines.md`](guidelines.md).
Time box ~30 min (overran to ~55 min, mostly builds on a cold worktree target).

## Measurements

| Check | Result |
|---|---|
| `cargo test --workspace` | 563 passed, 0 failed (+1 new test, see fix 1) |
| `cargo clippy --workspace --all-targets -D warnings` | clean |
| `cargo clippy -p rynas-ui --target wasm32` (+ `--features demo`) | clean / clean |
| wasm brotli -q 11 | **923 359 B = 902 KiB** (budget 1 200 KiB) |
| rynasd RSS at rest (`FAKE_UPS=ob`, NUT watcher running, after browsing) | 17.5 MB fresh, 18.9 MB after the audit (< 20 MB) |
| rynas-web RSS at rest | 6.4 MB |
| `scroll_audit.py` G7, `/storage,/hardware,/shares,/notifications`, en/it × 1280/1024/390, 48 dialog views | **0** sideways scroll (page and inner elements) |
| Raw i18n keys in it (probe on `/shares`, `/storage`, UPS tab, `/notifications`) | none |
| `GENERIC=1 FAKE_UPS=ol`: Synology/DSM/synodeb wording on `/hardware` (all tabs), `/storage`, `/shares`, `/dashboard`, `/network` | none; no Synology tab (G8d ok) |

`audit.py --quick` **could not run**: `tools/ui-audit/audit.js`, which `audit.py` loads at line 35,
is not in the repository (not in any commit, not in the main checkout). Replaced by the probes above
plus screenshots (it light: shares, storage, UPS on battery; generic UPS on line).

## Findings

### High

**H1 — Folder deletion followed symlinks into another subvolume (fixed).**
`HostShares::probe` (`crates/rynas-services/src/host.rs:188`) uses `std::fs::metadata`, which follows
symlinks. A `/<volume>/<name>` that is a symlink to a btrfs subvolume (another share, another volume)
therefore came out as `exists && is_dir && subvolume`: the plan allowed it
(`folders.rs` `plan`, `allowed`), `unshared_path` (`crates/rynasd/src/services.rs`, folder delete)
accepted it, and `remove_folder` ran `btrfs subvolume delete --recursive /<volume>/<name>`, which
resolves the path (btrfs-progs `realpath`) and deletes the **link's target**. The same path is
reachable through "delete share with data", since share paths are not checked for symlinks
(`validate::share_path`). `list_dir` (no symlink follow) keeps symlinks out of the Unshared list,
but the API takes any name. Fix: `PathInfo.symlink`; the plan is not `allowed` for a symlink and
`remove_folder` refuses before any command (test `a_symlinked_folder_is_never_deleted`, also on the
real filesystem).

### Medium

**M1 — A dialog's Save can commit a staged "delete share with data" and split it from its jobs.**
`DELETE /v1/shares/{name}?remove_folder=true` stages three keys (`shares`, `snapshots`,
`scheduler`) for the pending bar. Any later dialog Save that applies `keys=["shares"]`
(`pages/shares.rs:302`, `pages/services.rs:120,247`) applies the whole `shares` document of the
changeset, including the staged deletion: the folder and its snapshots are deleted at once
(irreversible), while the snapshot job and its scheduler task stay pending and, until applied, keep
running against a folder that is gone. Suggested fix (server side, `ops.rs` `apply` /
`Config::apply_keys`): when `shares` is selected and its pending document adds `remove_folders`
entries, either refuse with a clear code ("a staged folder deletion is pending: apply or discard it
from the pending bar") or extend the selection with `snapshots` and `scheduler`. The first is
simpler and keeps the review step the user chose.

**M2 — Unshared-folders size walk on every page open and every config change.**
`pages/shares.rs:68` loads `/api/v1/folders` with `Remote::with_config`, so it reloads on each
open of `/shares` and on every pending-generation change. Each load runs `folders::unshared`
(`crates/rynas-services/src/folders.rs:68`): `tree_size` on every unshared folder (0.5 s each,
3 s total) — `lstat` of every file, which spins up disks in standby (the HDD standby setting of the
same wave is defeated by opening the page) and costs up to 3 s of a blocking thread per load.
Time-bounded: yes. Proposal: a size cache in `SharesDomain` keyed by path with the folder's
`mtime`/`ctime` and a TTL (e.g. 10 min), filled by the first walk; skip the walk (size `None`,
shown as "—") when the volume's disks are in standby (the storage power cache already knows);
reload the list only on `shares` applies, not every pending change.

### Low

- **L1** UPS "Last result" shows NUT's raw `ups.test.result` in English in the it UI ("Done and
  passed", `pages/hardware/ups.rs:414`). Map the standard NUT values (`Done and passed`,
  `Done and warning`, `Done and error`, `Aborted`, `In progress`, `No test initiated`) to keys,
  raw text as fallback.
- **L2** Folder deletion runs inside the `shares` apply (`shares.rs` `reconcile_removed`, last step)
  before `health`; if `health` then fails (smbd not active), the rollback restores the share config
  but not the data, and the share comes back on a missing path. Consider running the deletion after
  a successful health check (post-commit hook like `Ops::after_apply`), or document it.
- **L3** `tools/ui-audit/audit.js` missing from the repo (see above): `audit.py` is unusable for
  everyone but its author's checkout state. Commit it or drop `audit.py` from the README/playbook.

### Checked, no finding

- Typed confirmation is enforced server side for both paths (`confirm` must equal the name:
  share delete with folder, `DELETE /v1/folders/{volume}/{name}`); the plan is recomputed and
  `allowed` re-checked before the task / the staging.
- Path rules: only `/<volume-mount>/<name>` (`on_volume`), never a volume root (empty parent),
  no `..`/`.`/system names (`.`, `@`, `#`, `lost+found`), `rm -rf --one-file-system
  --preserve-root=all --`; `remove_folders` entries validated (`share_path`, no `/`, safe prefix).
- Order: ACLs → snapshot-folder units → SMB → NFS (`exportfs -ra`) → removed shares last; the
  `#snapshot` unit is stopped before snapshots and folder are deleted.
- `#snapshot` mount: `bind,ro,nosuid,nodev,noexec`, plain bind (not `rbind`), `ConditionPathIsDirectory`,
  stopped and its mount point `rmdir`-ed when the share or its view goes; managed files with marker
  (drift/rollback covered); size walks and `rm` stay on one filesystem so they skip it.
- NUT: `upsd.users` managed block (only `instcmds` for the three tests, no `upsmon`/`actions`),
  owner and mode of the file kept; secret `/etc/rynas/nut-upsd.secret` written 0600 via candidate +
  rename, never logged (task logs say only "added the rynas user"); duplicate unmanaged
  `[rynas]` user refused.
- `upsmon -c fsd` / `systemctl poweroff` fallback: guarded by `u.dry` (logs only), the watcher is
  not started under `--dry-system` unless `--upsd` names a test upsd; stop only once per outage,
  LB/FSD → stop work only (upsmon shuts down).
- `ApplyRequest.keys`: pending rows are deleted per applied key (`commit`), other keys stay
  pending; conflicts and `restored_from` only among the selected keys; same rollback pipeline.
- Disk settings re-read after apply: ATA only, cached disks only, never a sleeping disk
  (`-n standby`).

## Fixes in this review

| Commit | Fix |
|---|---|
| `shares: never delete a folder that is a symbolic link` | H1 |

## Not verified

- Save-in-dialog in the GUI (apply + close, error keeps the dialog open): the test instance has no
  shares and the time box was spent; covered only by `crates/rynas-web/tests/config.rs` and the
  `apply_now` code path read.
- `audit.py` clipped-text / English-in-it sweep in light + en dark (tool missing, L3).
