# Quality review 2026-11-06, wave F

Scope: `git diff 32f58b5..main` (c2e3c4b): user homes (per-user volume, move task, per-user
links), Synology page re-render fix, dashboard and transfer charts, config discard refresh, UPS
status card, UPS setup wizard (`rynasd/src/ups_setup.rs`, `rynas-nut/src/setup.rs`, UI
`ups_setup.rs`), module boundary (`rynas-module`, Synology module, `ui/src/modules/synology`),
USB icon/badge, files editor active line. Read-only; every finding checked against the code.

## Checks

- `cargo test -p rynas-services -p rynasd -p rynas-nut -p rynas-config -p rynas-module -p rynas-hw-synology`: 217 passed, 0 failed.
- `cargo test -p rynas-ui`: 85 passed; `cargo clippy -p rynas-ui --target wasm32-unknown-unknown -- -D warnings`: clean.
- `trunk build --release`: wasm 3 964 092 B raw, 1 021 651 B brotli (~998 KiB, limit 1200 KiB).
- Local UI instance not run (code reading only for the GUI findings).
- Module boundary: no synodeb/Synology text left in the core paths of `rynasd` UPS/homes, `rynas-nut`,
  `rynas-api-types`, nor in the core UI tables; the Synology module registers only on its hardware or
  synodeb's files. Plain Debian path looks complete.

## High

### H1. Home move: stale read-modify-write of `users` reverts a concurrent apply — FIXED
Fixed: `Config::apply_update(author, key, comment, log, |applied| …)` reads the applied value,
lets the caller edit it and applies it all inside the apply lock; `switch_home` (move and undo)
uses it. Test `apply_update_reads_and_applies_inside_the_apply_lock` starts an apply between the
read and the apply and checks it waits and is not reverted.

`crates/rynasd/src/services/homes.rs:242-260` (`switch_home`). The applied `users` document is read
outside the configuration's apply lock, edited, then passed to `Config::apply_value`, which takes the
lock and applies that whole document.
- Scenario: a move (or undo) reaches the switch while an admin's apply that changes `users` is running
  (or starts just before). `apply_value` waits for the lock, then applies the stale document: the
  users created by the other apply are removed again (`userdel`), edits are reverted. Same in
  `move_undo` (same function).
- Fix: do the read-modify-write under the apply lock (e.g. `Config::apply_with(key, |current| …)`
  that reads `current(key)` inside the lock), or compare the base revision and retry/fail on change.

## Medium

### M1. Copy target is user-owned and user-writable during the root rsync — FIXED
Fixed: the copy is made in `<parent>/.<user>.rynas-copy/<user>` (staging folder root 0700, copy
root 0700; rsync gives the copy's root the source's owner, the staging folder keeps the user
out), chowned to the user only after the verification, then renamed to the target; a leftover
staging folder is cleared first. Unit tests and `packaging/test-homes.sh` check it.

`crates/rynas-services/src/homes.rs:914-921` (`copy_home` → `make_dir(&plan.target, …, user.uid,
user.gid, HOME_MODE)`), then `sync` as root (`:1491`).
- Scenario: a local user with a shell logs in during the first pass (the live check runs only after
  it). The target path is predictable (`<volume>/homes/<user>` or `/home/<user>`) and already theirs:
  they can replace subfolders of the copy with symlinks (e.g. to `/etc`) while root's rsync writes
  into them (rsync's receiver is not hardened against a hostile writable destination). The move then
  aborts on the live check, but the writes already happened.
- Fix: create the target `root:root 0700` and only give it to the user in the final
  `set_owner_mode` (already there, after the verification). Same idea for `create_home` is not needed
  (no copy into it as root apart from `/etc/skel`, done before the link exists).

### M2. `upsmon.conf`: `MONITOR <name>@` drops the admin's remote MONITOR lines
`crates/rynasd/src/ups_setup.rs:642` with the new prefix rule in `crates/rynas-config/src/files.rs:179`.
- Scenario: default name `ups`; the machine already watches a UPS on another host
  (`MONITOR ups@nas.lan 1 monuser pw secondary`, a very common line). Applying the wizard drops it:
  this machine stops shutting down on the other UPS. Taken-over names have the same problem.
- Fix: drop only local monitors: `MONITOR <n>@localhost` and `MONITOR <n>@127.0.0.1` (and `@::1`),
  or match `<n>@host` where host is local.

### M3. "Move all": free space is checked per home, not for the whole batch — FIXED
Fixed: `homes::batch_fits` sums the homes that can move; `move_start` refuses (409) when they do
not fit together; the task measures every home again without the dialog's time limit and stops
before copying when a size is still partial (unknown) or the batch does not fit; each home is
re-planned against the free space of the moment before its copy. A size that cannot be read at
all is now partial, not 0. The dialog shows the total against the free space, disables the move
when it does not fit and warns when sizes are lower bounds.

`crates/rynas-services/src/homes.rs:815-818`; batch in `crates/rynasd/src/services/homes.rs:378ff`.
Also the size can be partial (1.5 s budget), so `need` can be underestimated.
- Scenario: ten homes that each fit move to one volume that cannot hold them all: the later copies
  fill the destination (btrfs at 100 % hurts every service on it) before rsync fails and the copy is
  removed.
- Fix: in `move_start`, sum the planned sizes for the destination and refuse (or plan only what fits);
  refuse a single move when `size_partial` and the margin is thin; re-check `avail` before each copy.

### M4. "Move all" plan blocks for up to 1.5 s per home — FIXED
Fixed: sizes measured by 4 workers with one deadline for the whole plan
(`PLAN_TOTAL_BUDGET`, 3 s; homes not measured by then show "at least"); subvolume homes use their
btrfs quota group when quotas are on (no walk).

`crates/rynasd/src/services/homes.rs:161-209` → `plan_move` → `tree_size(…, PLAN_SIZE_BUDGET)`.
- Scenario: 40 users: the plan request (UI "Sposta tutte") takes ~60 s and holds a blocking thread.
- Fix: a global budget for the whole plan (split across homes), or use `qgroup_size` on subvolume
  homes, or make the bulk plan a task.

### M5. Wizard test run stops the running UPS driver, also on battery
`crates/rynasd/src/ups_setup.rs:445-456` (`stop_drivers`).
- Scenario: the admin tests a new driver while the UPS is on battery: upsmon loses the UPS while OB,
  which NUT treats as critical (and the Synology layer's `nocomm` reason) → forced shutdown.
- Fix: refuse `stop_drivers` when the last reading is `OB`/`LB` (or when upsmon reports on battery)
  and say why in the wizard.

### M6. UPS card says "install NUT" when NUT is installed but not running
`crates/rynas-ui/src/pages/hardware/ups.rs:142-147` (`(_, false) => "ups.down_text"` for both `Nut`
and `NotInstalled`; text "Installa NUT (pacchetto Debian nut)").
- Scenario: plain Debian, `nut` installed with stock `MODE=none`: the status card says to install NUT
  while the setup card under it offers "Configura UPS".
- Fix: a separate key for `UpsBackend::Nut` ("NUT è installato ma upsd non risponde: configura l'UPS
  qui sotto").

### M7. Setup card ignores the admin's own `ups.conf` sections
`UpsDeviceState.foreign` (`crates/rynas-api-types/src/ups.rs:417`) is never shown in the UI.
- Scenario: a working hand-written `[myups]`: the card shows no current UPS and "Cambia UPS"; the
  section appears only as a conflict in the review step. Also `pending` is shown as current without a
  "pending" mark (`ups_setup.rs:126`).
- Fix: list foreign sections as "configured outside rynas"; mark a pending device.

## Low

- **L1. FIXED (mutex held across list+save in `MoveStore::put/remove`).** Move records: unlocked read-modify-write. `crates/rynas-services/src/homes.rs:1134-1150`
  (`MoveStore::put/remove`); moves, finishes and undos of different users run as parallel tasks. Two
  ending together can lose a record: the kept old home then has no finish/undo. Fix: a mutex around
  list+save (file-backed too), or one homes task at a time.
- **L2. FIXED (an existing folder not root's or writable by group/others is refused with the reason; new `HomesHost::owner_mode`).** Existing `<volume>/homes` reused as is. `crates/rynas-services/src/homes.rs:404`
  (`ensure_folder`): owner and mode of an existing folder (e.g. left by another system, `0777`) are not
  checked; users could rename others' homes. Fix: require root owner and no group/other write, or set
  `root 0711`.
- **L3. FIXED (`homes::waiting_folders` adds the folders of waiting moves).** Homes folder deletable while only kept homes are in it. `homes_folders`
  (`crates/rynasd/src/services/homes.rs:50`) counts folders with live links and the default one; a
  folder holding only `*.rynas-old` homes of moves waiting for confirmation can be deleted as an
  unshared folder, making the undo impossible. Fix: add the folders of `moves` records.
- **L4. ini section drop misses headers with a comment.** `crates/rynas-config/src/files.rs:163`
  compares the whole trimmed line: `[ups]  # main` is not dropped though `nut::sections` parsed and
  took it over → duplicate `[ups]` in `ups.conf`. Fix: compare the header up to `]`.
- **L5. Wizard plan race / focus.** `load_plan` (`crates/rynas-ui/src/pages/hardware/ups_setup.rs:248`)
  has no sequence guard: an older answer can show a plan different from what Apply sends. Switches are
  rebuilt behind a skeleton on every toggle (focus lost). Fix: seq counter as in the homes move dialog.
- **L6. Index panics.** `r.candidates[i]` (`ups_setup.rs:232`, `:401`) panics on an out-of-range
  `found`. Use `.get(i)`.
- **L7. Manual options cannot hold spaces/commas** (`parse_options`, `ups_setup.rs:55`), consistent
  with the backend's value charset; say so in `upsw.options_help`.
- **L8. Post-apply polling** (`ups_setup.rs:288`): 6 refreshes 3 s apart even after the wizard closes;
  stop at Connected or on unmount.
- **L9. FIXED (move dialog falls back to the first place offered; "Sposta tutte" starts from the suggested place; new-user editor sends the first place when the default volume is not offered; a home's current place is always an option; `places()` now uses `active && !pending` like the list).** Homes selects can show one value and send another: "Sposta tutte" `to = settings.volume`
  not among `places()` (unmounted volume); same in the new-user editor (`users.rs`). Fall back to the
  first option. `places()` (`user_homes.rs:153`) checks `active` only, the list `active && !pending`.
- **L10. Discard refresh flag** (`crates/rynas-ui/src/config.rs:101`, `:146`): `EXPECTED` can be
  consumed by an in-flight poll or stay set after a failed read; one external change then misses its
  reload. Tag it with a generation.
- **L11. Synology probe 404 on plain Debian**: `modules::synology::present()` requests
  `/api/v1/hardware/synology/settings` on every Hardware visit when the provider is not Synology; uses
  `.set` after an await. Skip when the provider is known; `try_set`.
- **L12. Wording** (homes parts FIXED: `homes.b.smb_session_one` / `homes.b.smb_sessions` "sessioni aperte", `homes.move_needs` reworded; `syn.ups_deep_text` left to the UPS work): `homes.b.smb_sessions` → "(1 sessioni)"; suggest "sessioni SMB aperte: {detail}".
  `syn.ups_deep_text` "Se la batteria arriva a batteria scarica (LB)" → "Se la carica scende al livello
  di batteria scarica (LB)". `homes.move_needs` repeats "su un volume".
- **L13. Transfer chart**: `speedCurve`'s `ver(tm)` (files-app `transfers.js`) scans samples for every
  grid point (O(points × samples)); advance an index as `at()` does.

## Checked, no issue

NUT option values limited to `[A-Za-z0-9._:/@+,=-]`, names/drivers to `[a-z0-9_-]` (no injection in
argv, `ups.conf` or the module's `--opt`); NUT files root:nut 0640 with chown on the candidate before
rename; upsd password 0600; home removal refuses symlinks and non-`.rynas-old` paths; `rm` with
`--one-file-system --preserve-root=all`; link switch atomic (rename) with rollback; failed copy removed;
first-layout migration built aside and swapped with `RENAME_EXCHANGE`; `[homes]` SMB section private
(`valid users = %S`); maintainer scripts remove the 0.1.0 conffile and only rynas's own hook link; UPS
status card and Synology tab no longer rebuild per reading; chart maths covered by tests.
