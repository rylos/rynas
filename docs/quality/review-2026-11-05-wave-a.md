# Quality review — 2026-11-05, UI wave A

Scope: `main` at `f01d909`, the UI commits merged since the first review — `3d261df` (login, TOTP,
session, Security page), `343d278` (pending-changes bar, task panel, System → Configuration),
`50f3e84` (erase_components), `e842497` (Storage Manager pages), `64869be` (Users & Groups, Shared
Folders, File Services) — and the parts of `rynas-web`/`rynasd` they call. Rules `A*`, `G*`, `S*`,
`M*` are in [`guidelines.md`](guidelines.md); the previous review is [`review-2026-11-05.md`](review-2026-11-05.md).
Line numbers are those of this commit.

Areas (owners): **ui-shell** = `api.rs`, `ui.rs`, `auth.rs`, `live.rs`, `shell.rs`, `toast.rs`, base CSS;
**ui-config** = `config.rs`, `tasks.rs`, `pages/config.rs`; **ui-storage** = `pages/storage/*`;
**ui-users** = `form.rs`, `pages/users.rs`, `pages/shares.rs`, `pages/services.rs`;
**backend** = `rynas-api-types`, `rynas-web`, `rynasd`, `rynas-services`, `rynas-storage`.

## Measurements

Release builds, development workstation (8-core Ryzen, 16 threads). J4125 ≈ 3–4× slower per core.

### UI bundle (`trunk build --release`, brotli -q 11)
| File | Raw | Brotli | Budget | Previous review |
|---|---|---|---|---|
| `rynas-ui_bg.wasm` | 2 045 886 B | **567 535 B (554 KiB)** | ≤ 900 KiB — ok | 354.7 KiB |
| JS glue | 40 449 B | 7 942 B | ≤ 12 KiB — ok | 7 650 B |
| CSS | 78 532 B | 13 056 B | | 9 036 B |
| fonts (Inter + JetBrains Mono, latin) | 40 600 B | 40 610 B | CSS+fonts **52.4 KiB** ≤ 60 KiB — ok, 87 % used | 49.6 KiB |

Share of code by module (twiggy on the pre-`wasm-opt` module, names section excluded; the optimised
module scales roughly linearly): std/alloc/fmt 20 %, Leptos/tachys/reactive 14.7 %, **serde of
`rynas-api-types` 10.3 %**, rodata 6.3 %, `pages::storage::volume` 4.5 %, `pages::dashboard` 4.4 %,
`pages::storage` 3.0 %, `pages::shares` 2.8 %, `shell` 2.8 %, `pages::users` 2.4 %,
`pages::storage::disks` 2.3 %, `i18n` 2.1 % (`i18n::lookup` alone is the largest function: 51 KB),
`ui` 1.9 %, `wizard` 1.6 %, `form` 1.5 %, `api` 1.5 %, `security` 1.4 %, `services` 1.2 %,
`config` 1.1 %, `pages::config` 1.1 %, `tasks` 1.0 %. `pages::storage::fetch_into::<T>` exists in four
monomorphised copies of ~10 KB each.

### Backend (rynasd `--dry-system`, rynas-web `--users-file`, private state dir, 127.0.0.1)
| Process / state | VmRSS | RssAnon | PSS | Threads |
|---|---|---|---|---|
| rynas-web, start | 6.2 MB | 1.0 MB | — | 3 (VmHWM 24 MB: dummy argon2 at start, released) |
| rynas-web, after login | 6.4 MB | 1.1 MB | 3.7 MB | 2 |
| rynas-web, after 5 parallel failed logins + 3 SSE clients + 30 s of mixed GETs (210 742 requests, 4 workers) | **5.0 MB** | 2.1 MB | 2.5 MB | 2 |
| rynasd, first minute | 15.9–17.5 MB | 2.3–2.5 MB | 13.9 MB | 2–4 (file-backed pages of a 12 MB binary) |
| rynasd, after the load above | **9.9 MB** | 3.0 MB | 6.1 MB | 1 |

Memory budgets met (rynasd above 12 MB only during start-up, from file pages). The C1 fix of the first
review holds: five argon2 logins leave rynas-web at 5 MB. Idle CPU over 20 s after the load: 1 tick
(rynas-web), 0 (rynasd). Every endpoint the new pages use answers in p50 0.5–0.8 ms, p99 ≤ 1.4 ms
under 4 parallel clients (≈ 7 000 req/s); `storage/tree` cold 28 ms.

### Requests per page (steady state, one tab)
| Page | On open | Polling while open |
|---|---|---|
| every page (shell) | `/system`, `/metrics/history`, `/events` (SSE), `/tasks`, `/config/pending`, `/storage`, `/hardware` | `/hardware` **5 s**, `/storage` 30 s, `/config/pending` 30 s |
| Storage (overview, drives, volume) | + `/storage/tree`, `/arrays`, `/filesystems`, `/disks` | + those four every **15 s** |
| a running storage task | | + `/tasks/{id}` and `/tasks/{id}/log` every **1 s** (beside the SSE stream) |
| Users & Groups / Shares / File Services | + 2 / 3 / 3 collections | re-read on every configuration change |

## Findings

### Critical

**C1 — Two CSS rule sets named `.modal`: the review-and-apply dialog is mis-laid.** `style/main.css:948`
styles the native `<dialog class="modal">` of `ui::Modal` (from `343d278`); `style/main.css:1158` (from
`64869be`) styles the `<div class="modal">` of `form::confirm_dialog` and comes later, so it also hits
the native dialog: `position: fixed; top: 18vh; left: 50%; transform; padding: var(--s-6); display: flex;
width: 460px`. Result for `ReviewDialog` (`config.rs:339`) and the discard `ConfirmDialog` — the main
apply path of every configuration page — is a second padding around `.modal-box` (head border no longer
reaches the edges) and a dialog starting at 18 vh with `max-height: 86vh`, i.e. cut off below the fold
for long diffs. `.modal-title` and `.modal-actions` are also defined twice with different rules
(`:958/:1164`, `:961/:1166`). Found by cascade analysis; confirm with a screenshot. Fix: immediately
rename the users-wave classes (`.confirm`, `.confirm-title`…), then remove them with the dialog
consolidation below. Owner: ui-users (rename), ui-shell (consolidation).

**C2 — The UI keeps every session alive forever: the 30 min idle timeout never fires.**
`crates/rynas-web/src/auth.rs:103-106` renews the idle timeout on every request except `/stream` and
`/events`; the shell polls `/api/v1/hardware` every 5 s (`live.rs:264-270`), `/storage` and
`/config/pending` every 30 s (`live.rs:272-278`, `config.rs:82-89`), the storage pages four lists every
15 s (`pages/storage/mod.rs:125-131`). A visible tab left open on an unattended desk is therefore never
signed out until the 12 h absolute lifetime. Security property advertised on the Security page
(`sec.idle`, `pages/security.rs:354`) is not true. Fix: timer-driven requests send `X-Rynas-Background: 1`
(one flag in `api::send`, set by a `api::get_bg` used by every poller) and `require_session` calls
`get(token, false)` for them; only user-initiated requests touch. Add a test in rynas-web. Owners:
backend (middleware), ui-shell (`api.rs`, `live.rs`), ui-config, ui-storage (their pollers).

### Major

**M1 — Three dialog systems, two confirmation dialogs, three typed confirmations.** `ui::Modal`
(native `<dialog>`, `ui.rs:343`, used by ui-config), `ui::Dialog` (div + manual focus trap, also a drawer,
`ui.rs:485`, used by ui-storage) and `form::drawer`/`form::confirm_dialog` (div + another focus trap,
`form.rs:384/430`, used by ui-users); `ui::ConfirmDialog` (`ui.rs:406`) vs `form::confirm_dialog`;
`ConfirmDialog.confirm_word`, `form::Confirm.typed` and `ui::typed_confirm` (`ui.rs:563`) with two keys
(`confirm.type`, `confirm.type_name`). Two `FOCUSABLE` selectors, two opener-restore mechanisms, three
z-index layers (60/61, 70, toasts 70: a `Dialog` and a toast tie), three backdrop styles, drawer widths
520 px vs 640 px with different head paddings and close-icon sizes (18 vs 16). The div dialogs do not
make the page behind inert, so screen readers reach the shell behind them (G10). Fix: see the
consolidation plan. Owners: ui-shell (primitive), ui-storage and ui-users (ports).

**M2 — `ApiError.field` does not exist; the users/shares forms landed without it (A10).**
`crates/rynas-api-types/src/lib.rs:184` has only `code` and `message`; rynasd answers every validation
error as `400 invalid_value` without a field (verified: bad user name, bad e-mail, short password,
share name, workgroup). The UI already reads `field` (`api.rs:27`, `users.rs:77-85`), so server-side
rules always end in the form banner instead of under the field. Fix: add `#[serde(skip_serializing_if =
"Option::is_none")] field: Option<String>` to `ApiError`, an `Error::field(name)` builder in rynasd, set it
in the validators of `rynas-services` (users, groups, shares, smb, nfs) and `rynas-storage` (volume label,
subvolume), and use the same field names as the JSON body. Owner: backend.

**M3 — SMB/NFS form drafts are thrown away by any configuration change.** `pages/services.rs:45-60`
rebuilds `smb_form`/`nfs_form` whenever `smb.data`/`nfs.data` is set; `form::Load` (`form.rs:54-60`)
re-reads on **every** `config_changed`, and `Load::reload` always sets the signal, even with an equal
value. Saving NFS, staging a user in another tab of the same session, or any finished `config.*` task
resets an SMB form being edited. Fix: in `Load::reload` set only when the value differs (`PartialEq`
on the API types, or compare the JSON text), and rebuild the form only when the *applied/pending* state
changed and the draft is clean (keep a `dirty` flag; show "changed on the server — reload?" otherwise).
Owner: ui-users.

**M4 — Storage tasks are polled every second next to the SSE stream.** `pages/storage/plan.rs:158`
uses `api::follow_task` (`api.rs:283`): `GET /tasks/{id}` + `GET /tasks/{id}/log` every 1 s for the whole
task (a scrub runs for hours), although `task.updated` events already update `tasks::Tasks` and
`tasks::follow` (`tasks.rs:129`) polls only every 10 s as a safety net. `follow_task` also builds the URL
without encoding the id, uses the header literal instead of `TASK_LOG_NEXT_OFFSET`, and `TaskRun`
duplicates the task panel's status mapping (`plan.rs:137` vs `tasks.rs:192-210`) and log view. Fix:
`TaskRun` = `api::start_task` + `tasks::follow` for the state, plus a shared `tasks::LogTail` component
(the panel's log view, polling the log only while visible, ≥ 2 s); delete `api::follow_task` and the
`sto.task.*` keys. Owners: ui-storage, ui-config (`LogTail`).

**M5 — Started tasks are left unattended: password and recycle-bin cleanup.** `pages/users.rs:445-452`
(`user.password`) and `pages/services.rs:175-180` (`smb.recycle-cleanup`) show "started" and never look
at the outcome; a failed `chpasswd`/`smbpasswd` is visible only if the user opens the task panel. Both
task kinds also lack `task.user.password` / `task.smb.recycle-cleanup` in `i18n.rs`, so the panel shows
the raw kind. Fix: `tasks::run_task` with a toast on the final state (`task_error` on failure), keys in
en+it. Owner: ui-users.

**M6 — Four ways to report an outcome, and the guidelines name the wrong module.** `toast::toast`
(users, shares, services), `ui::Notice` (System → Configuration), an ad-hoc `.banner` paragraph
(`pages/storage/volume.rs:489`) and `auth::form_error` (every form and dialog). `guidelines.md:143` says
Toast = `notify`, but `notify.rs` is the notification bell; the toast module is `toast.rs`. Fix: the rule
in the consolidation plan; correct the guidelines table. Owners: ui-shell (doc, rule), ui-storage (banner).

**M7 — Four error-text helpers; English server text is shown as UI text.** `i18n::err_text`,
`auth::error_text` (`auth.rs:231`), `pages/storage::err_msg` (`mod.rs:329`) and `form::write_error`
(`form.rs:98`), which appends `ApiErr.message` (English, "never shown as the main text" per A10) to the
translated text: an Italian user reads "Valore non valido. invalid workgroup \"…\": 1-15 letters…".
Fix: one `ApiErr::text()` = translated code (+ translated field once M2 lands); the English message goes
to `title`/a "details" disclosure only. Owners: ui-shell, ui-users, ui-storage.

**M8 — Two loaders plus two page-specific ones.** `ui::Remote` (+ `remote_state`, `ui.rs:222-285`),
`form::Load` + `form::gate` (`form.rs:40-96`, the same thing plus "reload on config change"),
`ui::Pending` for `live::Res`, and the storage store `Sto`/`fetch_into` (`pages/storage/mod.rs:75-108`, a
generic async block monomorphised four times, ~40 KB pre-opt). Fix: keep `Remote`; add
`Remote::with_config(url)` for the config-following variant; storage keeps its shared store but goes
through one non-generic fetch (`api::get_text` + `serde_json::from_str::<T>` at the edge). Owners:
ui-shell, ui-users, ui-storage.

**M9 — Tables are hand-written on four pages.** `users.rs:174/580`, `shares.rs:102`, `services.rs:280`,
`storage/disks.rs:56/238/314/356`, `storage/volume.rs:933` build `<table class="table">` with their own
headers and `data-label`s instead of `ui::table`/`cell`/`row` (G 2.1 MUST; also wasm size: one compiled
copy). `users.rs`/`shares.rs` also use the ad-hoc `card section` (no `.section` rule exists) instead of
`card widget w-12`. Fix: extend `ui::table` with an actions cell and a row class, port the tables.
Owners: ui-users, ui-storage.

**M10 — i18n: 74 duplicated entries and parallel prefixes.** Same en+it text under several keys
(`cancel`/`sto.cancel`/`login.cancel`, `back`/`sto.back`/`login.back`, `col.status`/`st.state`/
`tasks.col_status`, `task.st.running`/`sto.task.running`, `s.healthy`/`sto.h.healthy`/`sto.a.healthy`,
`col.actions`/`sto.actions`/`pal.actions`…); two prefixes for one concept (`pend.*` in users vs `pending.*`
in the bar, `st.*` vs `sto.*`, `task.st.*` vs `sto.task.*`). Coverage is otherwise good: every used key
exists, every key has Italian. Missing: `err.totp_required` (A11). Fix: a naming rule in the guidelines
(`<area>.<thing>`; shared words in a `common.*`/bare set — `cancel`, `back`, `close`, `col.*`, `status.*`,
`health.*`, `task.status.*`), then a mechanical rename; add a unit test that fails on duplicate en+it
pairs longer than one word. Owners: ui-shell (rule, test), all UI areas (rename).

**M11 — `/config/pending` sends whole domains twice every 30 s per tab.** Each change carries `base` and
`value` of the full domain plus the diff (`users` includes every user and group). With 200 accounts that
is ~70 KB per poll per tab, while the bar needs only the count, keys and conflicts. Fix: `ETag` (hash of
the changeset generation) + `If-None-Match` → 304, and `?summary=true` (key, staged_ms, conflict) for the
bar; the review dialog asks for the full form. Better: a `config.pending` event on `/events` and no
polling at all (S1). Owners: backend, ui-config.

**M12 — Storage pages: fan-out and possible disk wake-ups.** `use_refresh` (`pages/storage/mod.rs:121-134`)
reads four tool-backed lists every 15 s although they come from one rynasd inventory snapshot
(`rynas-storage/src/service.rs:473`); the shell also reads `/storage` every 30 s. The btrfs detail cache
is 60 s (`service.rs:94`), so while a Storage page is open rynasd runs `btrfs filesystem usage` and the
subvolume walk on every data volume once a minute — tree reads that can spin up an idle HDD array (S6).
Fix: one `GET /api/v1/storage/inventory` (tree, arrays, filesystems, disks from the same snapshot), poll
it every 30 s only on storage pages and stop the shell's `/storage` poll there; serve usage from
`statvfs` between explicit refreshes, run the subvolume walk only on the volume page or on demand. Owners:
ui-storage, backend.

**M13 — `/hardware` is polled every 5 s from every page.** `live.rs:264-270`; S1 requires polling to stop
when no visible component needs it. On a non-Synology box the answer is static. Fix: reference-count
consumers (dashboard hardware widget, Hardware page, alerts) and poll only while one is mounted; 10 s
off the Hardware page. Also the main driver of C2. Owner: ui-shell.

**M14 — Wasm grew 58 % in one wave (355 → 554 KiB brotli).** Within the new budget, but four pages cost
≈ 200 KiB; the remaining phase-1 pages (network, notifications, scheduler, snapshots, hardware settings)
would pass 900 KiB at this rate. Biggest levers seen in twiggy: serde of the API types (10 %),
generic fetch helpers (M8), `i18n::lookup` as one 51 KB `match` (a sorted static table + binary search is
smaller), and the duplicated dialog/table code (M1, M9). Fix: the consolidations, a size line in every
UI PR (rule already there, not followed in these commits), and plan the S4 lazy split now. Owner: ui-shell.

### minor

| # | File:line | Problem | Fix | Owner |
|---|---|---|---|---|
| m1 | `style/main.css:691`, `:1035` | `@keyframes blink` defined twice; the later one (task log caret, opacity 0) also drives the hardware LEDs, which now blink fully off instead of to 25 % | `led-blink` and `caret-blink` | ui-config |
| m2 | `style/main.css:797/1144`, `:273/1340/1178`, `:1152/1341` | duplicate keyframes `fade`/`fade-in`, `pop-in`/`dlg-in`/`modal-in`, `slide-in`/`drawer-in` | one each, removed with M1 | ui-shell |
| m3 | `api.rs:235/314`, `api.rs:240/258` | `encode` and `enc` (same function), `sleep` and `sleep_ms` | keep `encode`, `sleep`; delete the others | ui-shell |
| m4 | `pages/storage/disks.rs:129`, `volume.rs:151`, `wizard.rs:248`, `plan.rs:195` | path segments not percent-encoded (users/shares pages encode) | `api::encode` everywhere a name goes in a path | ui-storage |
| m5 | `pages/users.rs:33` | `pill` re-implements `ui::status` | use `ui::status` | ui-users |
| m6 | `config.rs:59`, `api.rs:115`; `users.rs:335`, `services.rs:122` | two names for one action (`config::changed`, `api::notify_config_changed`); pages use the one the guidelines do not name | keep `config::changed`, make the other private | ui-config, ui-users |
| m7 | `auth.rs:524/530` | `spinner`, `form_error` live in `auth.rs` but every page imports them | move to `form.rs` (or `ui.rs`) | ui-shell |
| m8 | `api.rs:268-275`, `pages/security.rs:40`, `pages/storage/volume.rs:566` | three Blob-download helpers; the object URL is revoked right after `click()` (Firefox/Safari may cancel the download) | one `api::save_blob(name, Blob)`, revoke in a `set_timeout` | ui-shell |
| m9 | `form.rs:160-162` | `.field-error` is `role="alert"` and the client rule runs on every keystroke once the field has text: assertive announcements while typing | `aria-live="polite"`, show rule errors after blur or submit | ui-users |
| m10 | `toast.rs:73-92` | the whole toast list is re-rendered on each change (existing toasts re-animate); no pause on hover/focus for the 4 s timeout (WCAG 2.2.1) | `<For>` keyed by id; pause timers on hover/focus | ui-shell |
| m11 | `config.rs:263` | progress formatted as `" {p}%"` (G9) | `i18n::pct` | ui-config |
| m12 | `config.rs:98-107` | a failed `/config/pending` read is dropped silently; the bar may show a stale count with no hint (G2/A18 spirit) | keep last value, mark stale (icon + tooltip) after two failures | ui-config |
| m13 | `pages/storage/plan.rs:77` vs `volume.rs:314` | planner "usable" in SI (`bytes_si`), the same volume afterwards in IEC (`bytes`): 8.0 TB becomes 7.3 TiB (G9) | volume capacity in one helper on both pages (IEC, the filesystem view), raw disk size stays SI | ui-storage |
| m14 | `pages/storage/disks.rs:317` | literal column header `"ID"` (G7) | `smart.col_id` | ui-storage |
| m15 | `users.rs:38-53`, `shares.rs:27-40`, `services.rs:21`, `storage/mod.rs:162`, `volume.rs:973` | client validators copy rynasd's rules by hand (drift already: workgroup is upper-cased server-side only) | a dependency-free `validate` module in `rynas-api-types` used by both sides | backend, ui-users |
| m16 | `api.rs:200-202` | doubled doc comment on `put` | delete one line | ui-shell |
| m17 | rynasd `POST /users`, `/groups` (and shares) | 201 without `Location` (A6 table) | add `Location: /api/v1/users/{name}` | backend |
| m18 | `docs/phase1/plan.md:386/389/394` vs `guidelines.md` A6 table | DELETE of users/groups/shares answers 200 `StagedChanges`, the table says 204; `POST /config/discard` with nothing staged answers 204, A9 lists `409 nothing_pending` | decide once (staging deletes return 200 + `StagedChanges` is reasonable) and update the guideline table; same for discard | backend |
| m19 | `PUT /services/nfs` | a missing field answers `bad_request` with the serde text "Failed to deserialize the JSON body…" (A13; also seen in the first review) | generic message, serde detail in the log | backend |
| m20 | `api_types::services::UserStatus.badges` (`services.rs:147`), `fs.scrub.status == "running"` (`pages/storage/mod.rs:478`) | closed sets as `String` (A16), `smb-password-reset` is kebab-case (A14) | enums, snake_case | backend |

## Consolidation plan for the duplicates

| Job | Keep | Remove / change | Why |
|---|---|---|---|
| Dialog / drawer | **`ui::Modal`** (native `<dialog>`, `showModal`: real inertness, focus trap and restore, Esc) + a `drawer: bool` prop (CSS `.modal.drawer`: right-aligned, full height, 560 px) | `ui::Dialog`, `OPENERS`, both `FOCUSABLE`, `form::drawer`, `form::trap_keys`, `form::manage_focus`; CSS `.overlay`, `.drawer*`, `.modal-root`, `.modal-backdrop`, `.dialog*`, the second `.modal` block | only the native element gives inert background for free and is the smallest code; ui-config already uses it |
| Confirmation | **`ui::ConfirmDialog`** extended with `busy`, `error`, `extra: Option<ChildrenFn>`, `typed: Option<Signal<String>>` (stays open while busy, shows the error) | `form::confirm_dialog`, `form::Confirm`, prop `confirm_word` | one API for "are you sure", features of the users-wave version kept |
| Typed confirmation input | **`ui::typed_confirm`**, used inside `ConfirmDialog` and the storage plan flow | the inline inputs in `ConfirmDialog` and `confirm_dialog`; key `confirm.type` (keep `confirm.type_name`) | one prompt text, one input style |
| Error state | **`ui::ErrorBox`** (component) with `ui::ErrorState` as its thin `live::Res` wrapper | — (both stay; document that `ErrorState` = `ErrorBox` + `live.refresh`) | they are not duplicates, the names just need the note |
| Error text | **`ApiErr::text()`** (in `api.rs`, via `i18n::err_text`) | `auth::error_text`, `storage::err_msg`, `form::write_error` (English detail moves to a tooltip) | M7 |
| Fetch + states | **`ui::Remote`** + `Remote::with_config` | `form::Load`, `form::gate`; storage `fetch_into` becomes a non-generic call | M8, wasm size |
| Task following | **`tasks::run_task`** / `api::start_task` + **`tasks::follow`** (SSE-driven) + new `tasks::LogTail` | `api::follow_task`, storage `status_of`, `sto.task.*` keys | M4 |
| Sleep | **`api::sleep(u32)`** | `api::sleep_ms` | |
| Path encoding | **`api::encode`** | `api::enc` | the documented one, used in more modules |
| Outcome feedback | **`toast::toast`** for short confirmations of finished work ("saved, pending apply", "task finished/failed"); **`ui::Notice`** for a page-level result the user must read (restore, drift actions, LUKS header, recovery key); **`form_error`** (moved to `form.rs`) for errors inside a form or dialog | `.banner` in `volume.rs:489` → `Notice`; fix `guidelines.md:143` (Toast = `toast::toast`) | M6 |
| Status pill | **`ui::Status` / `ui::status`** | `users::pill` | |
| Tables | **`ui::table` / `cell` / `row`** (+ actions cell, row class) | hand-written tables of M9 | G 2.1, wasm |
| Downloads | **`api::download`** + new `api::save_blob` | `security::download_text`, `volume::save_text` | m8 |
| Config generation | **`config::changed`** | public `api::notify_config_changed` | m6 |

Suggested order: C1 rename (minutes) → C2 → M2 (backend, unblocks inline errors) → dialog/confirm
consolidation (M1, removes m1/m2) → task following (M4, M5) → loaders/tables (M8, M9, measured with
twiggy) → i18n rename (M10) → polling (M11–M13).

## Fixed

UI consolidation pass of 2026-11-05 (one agent, branch of `08071b9`): the findings owned by the UI
areas, applied also to the pages added since the review (notifications, scheduler, snapshots,
network, hardware › Synology / power schedule). Backend findings (C2, M2, M11, M13 and the backend
part of M12) were fixed in `099752c`.

| # | Fix |
|---|---|
| C1 | One `.modal` rule set (the native `<dialog>`), the users-wave `.modal`, `.overlay`, `.drawer*`, `.dialog*`, `.modal-root/-backdrop` blocks removed; checked by the `style_check` tests (no selector set twice). |
| M1 | One dialog: `ui::dialog` on `<dialog>` + `showModal()` (top layer, inert page, Esc → `cancel`, focus to the first field, back to the opener on close, backdrop click only when the press started there), kinds normal / wide / drawer / alert; `Modal` (component) and `ui::drawer` on top; the command palette is a modal `<dialog>` too. One confirmation `ui::confirm` + `ui::Confirm` (Cancel focused, busy + error inside, extra content, reactive typed word through `ui::typed_confirm`). Removed: `ui::Dialog`, `ui::ConfirmDialog`, `form::drawer`, `form::confirm_dialog`, both focus traps and `FOCUSABLE`s, `OPENERS`, key `confirm.type`. |
| M3 | `ui::Remote` sets `data` only when the answer text changed, so an unrelated configuration change re-renders nothing; settings forms (SMB, NFS, Synology hardware, system restore points) are built once on a `form::Draft`: a clean draft follows the server, an edited one keeps the edits and shows "changed elsewhere — discard my edits and reload". Verified in the browser: an SMB workgroup draft survives saving the NFS card. |
| M4 | `api::follow_task` (1 s `GET /tasks/{id}` + log) removed; `tasks::TaskRun` (storage plans) reads the task from the store kept by `task.*` events, `tasks::follow` as fallback; `tasks::LogTail` is the one log view (task panel and pages), 2 s between empty reads, no reads while the tab is hidden; `sto.task.*` keys and `status_of` removed. |
| M5 | Password change and recycle-bin cleanup go through `tasks::run_task` and toast the final state (`task_error` on failure); `task.user.password`, `task.smb.recycle-cleanup`, `task.btrfs.scrub`, `task.smart.test` added in en+it. |
| M6 | Roles: `toast::toast` short confirmations of finished work; `ui::Notice` results the user must read (the LUKS-header `.banner` of the volume page is one now); `ui::note` static warnings and hints (every `.banner` and hand-written `.notice`); `form::form_error` errors inside a form or dialog. Guidelines table corrected (Toast = `toast::toast`). |
| M7 | One error text: `ApiErr::text()` = translated code + the server's English message as a detail that `ui::err_view` shows apart (smaller, `lang="en"`) in form errors, field errors, toasts and notices. Removed `auth::error_text`, `storage::err_msg`, `form::write_error`. |
| M8 | One loader: `ui::Remote` (`new`, `with_config`) with `gate()`/`view()` over `ui::loading`; `form::Load`/`form::gate` and `ErrorState` removed, `ui::Pending` uses `ui::loading`. The fetch is not generic (`api::get_text`, only the parse is per type); the storage store uses the same non-generic fetch. |
| M9 | Tables of users, groups, shares, file services, notifications (centre, targets, rules, silences), scheduler (tasks, history), snapshots, drives, partitions, SMART attributes and self-tests, subvolumes and revisions through `ui::table` / `row` / `row_class` / `row_flag` / `cell` / `actions`; `card section` → `card widget w-12`. Only the permission matrix and the revision detail row write `<table>`/`<tr>` themselves. |
| M10 | One key per text (1 821 → 1 764 entries; keys sharing a text 174 → 125, the rest single words used in different places or names mirrored from the server): shared words bare (`cancel`, `back`, `save`, `edit`, `edit_named`, `add_rule`, `run_now`, `not_set`…) or grouped (`col.*`, `health.*`, `pending.*`); `st.*` → `sto.*`, `pend.*` → `pending.*`; 24 unused keys removed. Test `no_duplicate_texts` fails on two keys with the same English and Italian text of more than one word; naming rule G8b in the guidelines. |
| M12 (UI) | Storage pages: drives, arrays and tree every 30 s (was 15 s), the filesystems (btrfs usage) every 5 min, everything on open, Retry and after tasks; the shell's `/storage` poll pauses while a storage page is open; disk details are still read only on demand and never automatically for a disk in standby. The single `/storage/inventory` endpoint stays a backend item. |
| M14 | See the size table below. `i18n::lookup` is a binary search over one concatenated string instead of a 1 800-arm `match`; generic fetch helpers and the dialog/table copies are gone. |
| m1, m2 | `@keyframes led-blink` (LEDs to 25 % again) and `caret-blink`; one `fade`, `pop-in`, `slide-in`. |
| m3 | `api::sleep_ms` and `api::enc` removed (`api::sleep`, `api::encode`). |
| m4 | Volume, subvolume and disk names percent-encoded in every API path. |
| m5 | `users::pill` → `ui::status`; `ui::pending_status` for staged objects. |
| m6 | `config::changed` / `config::generation` own the configuration generation; `api::notify_config_changed` and `api::config_changed` removed. |
| m7 | `spinner`, `form_error` live in `form.rs`. |
| m8 | `api::save_blob` / `api::save_text` (object URL revoked after a minute) replace `security::download_text` and `volume::save_text`; `api::download` uses it. |
| m9 | Field errors are `aria-live="polite"` and show once the field has been left (an error that arrives while the field is not focused shows at once). |
| m10 | Toasts keyed with `<For>` (existing ones do not re-animate), timers paused while the pointer or the focus is on the stack. |
| m11 | Apply progress through `i18n::pct`. |
| m12 | After two failed reads of `/config/pending` the bar shows a warning icon ("the count may be out of date"), keeping the last known set. |
| m13 | Usable capacity of a plan in IEC (`bytes`) like the volume afterwards; raw sizes SI. |
| m14 | SMART attribute ID and group GID headers through i18n (`smart.col_id`, `groups.gid`). |
| m16 | Doubled doc comment on `api::put` removed. |
| CSS | Tokens only (chassis, LEDs, badge text, dock shadow), spacing on the `--s-*` scale (`--s-half` only for icon alignment and the stacked usage bar), font sizes from `--fs-*`, one rule per selector, 10 unreferenced classes removed; enforced by the `style_check` tests (balanced blocks, colours, spacing/type scale, duplicates). |

Not changed: m15 (shared validators in `rynas-api-types`) and m17–m20 are backend items.

### UI bundle after the pass (`trunk build --release`, brotli -q 11)
| File | Before (`08071b9`) | After | Change |
|---|---|---|---|
| `rynas-ui_bg.wasm` | 3 186 647 B raw, **843 499 B brotli (823.7 KiB)** | 3 121 445 B raw, **828 842 B brotli (809.4 KiB)**, 90 % of the 900 KiB budget | −14 657 B brotli (−1.7 %) |
| JS glue | 41 821 B, 8 167 B brotli | 41 431 B, 8 113 B brotli | −54 B |
| CSS | 87 741 B, 14 379 B brotli | 84 839 B, 13 970 B brotli | −409 B |

Pre-`wasm-opt` module (twiggy, names section excluded): code + data 4 341 347 → 4 267 646 B (−73.7 KB). `i18n` code 106.6 → 18.1 KB (the dictionary is data now, rodata +36 KB), `form` 89.3 → 44.1 KB, `api` 73.4 → 41.7 KB, the four `storage::fetch_into` copies (~30 KB) and the per-type `Load::reload` futures gone; `ui` grew 83.0 → 116.8 KB with the shared dialog, confirmation, tables and notes. The serde code of `rynas-api-types` (16.5 %) and the remaining page markup are now the biggest levers (S4 lazy split); the consolidation removed duplicated code, not features, so the saving is modest.

Verified: `cargo check`/`clippy -D warnings` for wasm32 with and without `demo`, `cargo test -p rynas-ui`
(49 tests, among them the CSS and i18n checks), release build, and a headless-Chromium walk
through every page against `rynasd --dry-system` (fake DS920+) in light/dark, en/it, desktop and
390 px: no console errors (apart from the expected 401 of the session check before sign-in), no horizontal scroll, no error boxes; drawers, confirmations (typed word
with "remove home"), review dialog, apply followed through the task panel, Esc closing with the
focus back on the opener, field errors only after blur, SMB draft kept across an unrelated change.

Observed during the walk, also with the UI of `08071b9` (so not caused by this pass): in one headless
Chromium tab the eighth full page load in a row (`Page.navigate`) never completes, while client-side
navigation works; likely the HTTP/1.1 connection limit held by event streams of the previous loads
that the server has not closed yet. To check on the backend side (A24, `/events` teardown).

## What is good

- Every page follows skeleton → content | error with Retry; no `inner_html`, no `forget()` in new code,
  no hex colours or off-scale spacing in the new CSS (only `--qr-*`/`--led-*` tokens added).
- Forms are well wired: labels with `for`, `aria-describedby`, `aria-invalid`, tabs with roving
  tabindex and arrow keys, password strength + server rules checked before sending, typed confirmation
  for every destructive storage step, mobile `data-label`s on almost every cell.
- `api.rs` is the only HTTP client: CSRF only in memory, 401 → login without reload, 30 s timeout,
  `Retry-After` kept; the login flow handles `totp_required`, rate limiting and session renewal.
- The pending-changes bar, review dialog and task panel match the step-6 model end to end; the
  configuration apply follows its task through the event stream (`tasks::follow`), as A20 asks.
- Backend: all new endpoints answer JSON errors with registered codes, CSRF on every write, sub-ms
  latencies; memory budgets hold under load.
