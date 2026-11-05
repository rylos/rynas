# Quality review — 2026-11-05, wave C (GUI only)

Scope: `rynas-ui` at `c739254` (after the G7 rule), GUI only: coherence, layout, i18n, themes,
accessibility. No features, no other crate touched. Rules are in [`guidelines.md`](guidelines.md);
the previous passes are [`review-2026-11-05-wave-a.md`](review-2026-11-05-wave-a.md) and
[`review-2026-11-05-wave-b.md`](review-2026-11-05-wave-b.md). Line numbers are those of `c739254`.

The previous pass declared "no horizontal scroll" while the share-permission matrix had an inner
scrollbar, because only the page and the dialogs opened by primary buttons were checked, and only
the first tab of each dialog. This pass walked every reachable view and looked at the screenshots.

## Method

- Test instance: release `rynasd --dry-system --hw-root <fake DS920+ tree> --notify-socket ""
  --synodeb-notify-conf "" --storage-check-interval 0 --poweroff-command /bin/true` with its own
  socket and state dir under the worktree's `target/`, `rynas-web --users-file` on a localhost port,
  `--auth-state-dir`, `--ui-dir` = the release build. A second `rynas-web` without `rynasd` for the
  error states.
- Headless Chromium over CDP. For every language/theme, at 390 (mobile), 768 (mobile), 1024 and
  1280 px: every route (18 plus the volume page and the 404), **every page tab**, every dialog and
  drawer reachable from page buttons, row actions and cell buttons, **every tab of each dialog**,
  nested dialogs opened from a dialog body (confirmations), the volume wizard steps 1–4 (never the
  final Create), the command palette (open, filtered, no result), the app-bar popovers
  (notification centre, account menu), the tasks panel and a task log.
- In each view: elements with `overflow-x: auto|scroll` and `scrollWidth > clientWidth`
  (dialogs included), the page width, elements past the right edge, clipped boxes, text sticking out
  of its box horizontally or vertically, short labels squeezed into three or more lines, icon boxes
  squeezed out of their square, overlapping text of different elements (same layer), truncated
  labels (ellipsis, line clamp), raw i18n keys (text, `aria-label`, `title`, `placeholder`),
  English text in Italian mode (dictionary texts that differ in Italian, plus common English words),
  controls without an accessible name, overlays that leave the viewport, and the computed look of
  every button per role.
- Separate pass: Tab through 16 routes in light and dark checking the focus ring of every focused
  element; loading states with 3 s of network latency (10 routes, en/it, 390/1280); error states
  with `rynasd` down (16 routes, en/it, 390/1280).
- Screenshots of every page view and of every dialog view in Italian/light, and of every flagged
  view, looked at by eye (with full-page shots where content was below the fold).

### Coverage (final run, on the fixed build)

| Width | en views | it views |
|---|---|---|
| 390 | 159 | 160 |
| 768 | 159 | 160 |
| 1024 | 159 | 160 |
| 1280 | 159 | 160 |

**1 276 views**: per width and language, 117–118 in light (every page, page tab, dialog, dialog
tab, nested confirmation, wizard step, palette, popover, tasks panel) and 42 in dark (pages, page
tabs, wizard, palette, popovers, tasks panel; the dialogs in dark were not walked a second time,
since the theme does not change layout). Baseline before the fixes: 856 views (390 px complete, 768
px partly, all four language/theme combinations). Focus pass: 16 routes × 2 themes × 30 Tab stops,
no focused element without a visible ring. Loading: 40 views; error: 56 views.

## Findings and fixes

### Critical

**C1 — The command palette panicked on its second opening.** `palette.rs:210` used
`NodeRef::on_load` on a node ref declared outside the `Show` that mounts the `<dialog>`: `on_load`
runs its closure once, so the second mount hit `Option::unwrap()` on `None` in tachys
(`node_ref.rs:50`) and the palette never opened again (Ctrl K, search button). Fix: an `Effect` on
the node ref calls `showModal` on every mount. New test `source_check::node_ref_on_load_only_per_mount`.
Commit `8094abe`.

### Major

| # | File:line | Problem | Fix | Commit |
|---|---|---|---|---|
| M1 | `style/main.css:714-735` | **Sideways scroll (G7)**: the chassis drawing (fixed 150 px bays in one row, `.chassis-wrap { overflow-x: auto }`) scrolled inside its card at 390 px (Storage overview, Hardware › Synology: 748 px in 324 px) and at 768 px on the Synology tab | bays wrap (two per row on phones), model name across the top below 768 px, no scroll container | `cfe8c9c` |
| M2 | `style/main.css:1249`, `:1226` | **Nameless buttons (G10)**: below 768 px `.btn-label` and `.search-text` were `display: none`, leaving the search button and page actions (Create volume, Add rule, Create snapshot job, Refresh, Customize, back to System) with no accessible name | visually hidden instead of removed | `fb92652` |
| M3 | `style/main.css:373`, `ui.rs:172` | page header icon squeezed to a 22–38 px wide pill on 15 pages at 390 px (and Network at 768 px) | `flex: none` on page, widget and notification icon boxes; header text may wrap | `d6e0588` |
| M4 | `style/main.css:1115` | share-permission matrix as cards on phones: the level cells kept the 16 % table width, "Lettura e scrittura" on three lines | full-width cells below 768 px | `8997476` |
| M5 | `style/main.css:1083-1084` | System › Configuration link card: title and text squeezed into 80 px beside the long button at 390 px | text min 160 px, button wraps below | `db3ea42` |
| M6 | `pages/storage/disks.rs:417-423` | disk drawer › Settings: check labels were the server's English ("I/O scheduler", "Write cache", "SATA link power (ALPM)") in Italian | labels by `SettingCheck.key` from the dictionary; notes `lang="en"` | `c0af6c9` |

### minor

| # | File:line | Problem | Fix | Commit |
|---|---|---|---|---|
| m1 | `style/main.css:798` | segmented buttons had a fixed height: "Velocità massima" (fan mode, 768 px) stuck out of the control | `min-height`, grows with the label | `9ef25cd` |
| m2 | `style/main.css:665`, `:1365` | plan tiles cut "Può guastarsi 1 disco" to "Può guasta…" at 390 px | tiles ≥ 140 px and wrap; subtitle wraps | `dba0a8c`, `8f0983a` |
| m3 | `pages/storage/plan.rs:63,68,102`, `wizard.rs:374` | planner errors, warnings, step texts are English free text without `lang` | `lang="en"` | `dba0a8c`, `017832a` |
| m4 | `pages/network/mod.rs:565,573`, `edit.rs:85` | Read-only chip tooltip = server English only | translated sentence + reason as detail; drawer detail `lang="en"` | `7ab17bf` |
| m5 | `alerts.rs:37` | bay temperature alerts labelled "Bay N" in every language | `sto.bay` + number | `d22e611` |
| m6 | `pages/notifications.rs:699,1041,1098` | built-in target shown as "Web interface" in Italian (seeded name) | shown as `nt.kind.ui` while it keeps the seeded name | `8d41350` |
| m7 | `pages/storage/disks.rs:77,79` | Drives table: blank Temp. cell without a reading; used-by English without `lang` | dash; `lang="en"` | `a730502` |
| m8 | `style/main.css:585`, `:1403` | key/value values and wizard disk labels ended in an ellipsis with no tooltip ("Sconosciuta (connessione locale o…", mount points, the bay of a disk) | wrap | `8834b5f` |
| m9 | `style/main.css:520` | chips had a fixed 20 px height: the configuration-key chip "Ripristino di emergenza della rete" stuck out at 768 px | `min-height` | `017832a` |
| m10 | `pages/storage/disks.rs:63` | drive model wrapped to three lines at 768–1024 px | drive cell `min-width: 132px` | `017832a` |
| m11 | `shell.rs:188`, `style/main.css:1212,1226` | connection state was a colour dot only below 1024 px (G6) and not a live region (G11) | text kept unless live; `role="status"` | `fb92652` |

### Automated checks added

- `source_check::node_ref_on_load_only_per_mount` (C1 class).
- `style_check::horizontal_scroll_containers_are_listed`: every CSS rule that lets an element scroll
  sideways needs an entry with its reason (`.table-wrap`, `.task-list`, `.log`, `.subnav`,
  `.stepper`); G7 in the guidelines names it (M1 class).
- `i18n::used_keys_exist` ignores `.rs` file names (needed by the new source test).

## Checked and fine

- After the fixes, in all 1 276 views: no element scrolls sideways (dialog tabs, permission matrix,
  drawers, wizard, palette, tasks panel included), no page overflow, no element past the right
  edge, no overlay outside the viewport, no overlapping text, no nameless control, no raw key except
  two intended ones (the `example.org` placeholder, the configuration-key tooltip of revision chips).
- Buttons: one look per role in both themes; the only variations are intended (icon-only page
  actions with 12 px padding on phones, 36 px row actions in the phone card layout, disabled colour).
- Focus ring visible on every Tab stop of 16 routes in light and dark.
- Loading: skeletons inside their cards with 3 s latency; error: `ErrorBox` with Retry on every
  route with `rynasd` down, in both languages.
- Wasm: 831 950 B brotli -q 11 (812.5 KiB; 832 260 B before) — under 900 KiB. JS 8 200 B,
  CSS 14 230 B.
- `cargo test -p rynas-ui` (52 tests), clippy wasm32 `-D warnings` with and without `--features
  demo`, `trunk build --release`: pass.

## Open items

- **Backend (not UI):** English free text without a code reaches the UI in four places: planner
  errors/warnings/steps, `DiskSummary.used_by` ("mounted /home"), `SettingCheck.note`/`current`
  values, `NetInterface.read_only_reason`. They need codes (or enums) so the UI can translate them
  (A10/A16). The built-in notification target is seeded with an English name (`rynas-notify`
  `engine/db.rs:83`).
- Dark theme: dialogs were not walked a second time in dark (pages, tabs, wizard, palette,
  popovers and tasks panel were); the light walk covers their layout.
- The 768–1024 px Drives table is tight in Italian ("ALLOGGIAMENTO" header); verify m10 by eye.
- With `rynasd` down, primary actions (e.g. Create user) stay enabled next to the error box.
- Dialog forms were audited in their initial state; conditional fields that appear only after a
  choice (select/segmented values) were not toggled.
- The tooling (CDP walk, `audit.js`) lives outside the repo; worth adding as a dev tool when the
  repo gets a place for scripts.
