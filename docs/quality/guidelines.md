# rynas — quality guidelines

Standing rules for everyone who writes code in this repository. They cover **API consistency, GUI
consistency, speed and memory**. Every rule is meant to be checkable in review; "MUST" rules block a
merge, "SHOULD" rules need a written reason to break. The reference machine is a **Synology DS920+**
(Celeron J4125, 4 cores, 4 GB RAM, HDDs that spin down). When in doubt, measure on that box.

Budgets come from `docs/phase1/plan.md`; design tokens from `docs/phase0/02-ui.md`; stack choices from
`docs/phase0/05-decisions.md`. Reviews against these rules live next to this file
(`review-YYYY-MM-DD.md`).

---

## 1. API

### 1.1 Paths and versions
- A1 MUST: every public endpoint lives under `/api/v1`. A breaking change (removed field, changed
  meaning, changed status code) needs `/api/v2` for that resource; adding an optional field is not breaking.
- A2 MUST: paths are **lowercase, plural nouns, kebab-case** for multi-word segments
  (`/storage/disks`, `/config/revisions`, `/shares/smb`, future `/smart-tests`). Path parameters are
  identifiers, never free text: `{name}` (kernel names, `sda`), `{id}` (opaque ids, UPID), `{n}` (integers).
- A3 MUST: singletons (one object per system) use a singular noun under their owner
  (`/system/hostname`, `/system/time`).
- A4 MUST: actions that do not map to CRUD are `POST /<collection>/{id}/<verb>` or
  `POST /<area>/<verb>` with a verb in kebab-case (`/config/apply`, `/tasks/{id}/cancel`,
  `/config/drift/{key}/import`). Read-only computations with a body (planners, validators) use
  `POST /<area>/plan/<what>` and MUST have no side effects.
- A5 MUST: rynasd's internal routes mirror the public ones 1:1 with `/v1` instead of `/api/v1`
  (`/api/v1/storage/disks` ⇄ `/v1/storage/disks`). Internal-only routes (`/v1/authenticate`,
  `/v1/totp/{user}/…`, `/v1/users/{user}/groups`) are never exposed. A new public endpoint backed
  by rynasd is added to both routers and to the table in `docs/phase1/plan.md` in the same commit.
- A6 MUST: one forwarding mechanism from rynas-web to rynasd (see review: today there are three). The
  forwarder adds `x-rynas-user` to **every** call, including reads, so rynasd can audit and authorize.

### 1.2 Methods and status codes
| Situation | Method | Status | Body |
|---|---|---|---|
| Read | GET (also HEAD) | 200 | resource |
| Create | POST on the collection | 201 + `Location` | created resource |
| Replace / stage a config object | PUT (idempotent: same body twice = same state) | 200 | new state |
| Partial update | PATCH (JSON merge patch) — only if really needed | 200 | new state |
| Delete | DELETE (idempotent: deleting a missing object is 404, not 500) | 204 | — |
| Start a long operation | POST | 202 | `TaskStarted {task_id}` |
| Action with nothing to return | POST | 204 | — |

- A7 MUST: anything that can take more than **2 s** on the J4125 or that changes the system is a
  task (202 + `TaskStarted`), never a long-held HTTP request.
- A8 MUST: every non-2xx answer from `/api/**` — including 404 for unknown paths, 405, 413, 415, query
  string and JSON rejections, timeouts — has `Content-Type: application/json` and an `ApiError` body.
  No axum default text bodies: wrap `Query`, `Json`, `Path` extractors (or install a rejection mapper).
- A9 MUST: status codes by meaning:
  `400 bad_request` (malformed request: JSON, query, path syntax), `400 invalid_value` (well-formed
  but semantically invalid input), `401 unauthenticated|invalid_credentials|totp_required|invalid_totp`,
  `403 csrf|forbidden`, `404 not_found`, `405 method_not_allowed`, `409 conflict|busy|nothing_pending|task_finished|…`,
  `413 payload_too_large`, `429 rate_limited` + `Retry-After`, `500 internal`, `502 bad_gateway`
  (rynasd answered garbage), `503 backend_unavailable` (rynasd or a provider down),
  `504 timeout` (rynasd did not answer in time).

### 1.3 Errors
- A10 MUST: error body = `ApiError { code, message, field? }`. `code` is machine-readable
  `snake_case`; `message` is English, for logs and tooltips, and is **never parsed** by the UI. The UI
  translates `code` (and `field`) through i18n. `field` (skipped when `None`) names the request field
  a validation error is about, spelled as in the JSON body (`email`, `workgroup`, `smb.veto_files`);
  set it wherever the field is known: domain validators return `rynas_config::Invalid::at(field, msg)`,
  rynasd handlers `Error::invalid(field, msg)` (or `.field(name)` on another error), `rynas-net`
  `NetError::InvalidField`.
- A11 MUST: every `code` is a constant in `rynas_api_types::error_codes`; no string literals for codes
  anywhere else (grep: `"[a-z_]+"` next to `StatusCode::` outside `error_codes` must be empty).
  Adding a code = adding the constant + a line in the table of `docs/phase1/plan.md` + an i18n key
  `err.<code>` in en and it.
- A12 MUST: one code per meaning across areas. "rynasd unreachable" is `backend_unavailable`
  everywhere (not `storage_unavailable` for storage only); "provider down" is `backend_unavailable`
  with a message naming the provider.
- A13 SHOULD: messages do not leak internals (serde paths, file paths, argv); log those server-side.

### 1.4 Bodies, lists, units
- A14 MUST: JSON field names `snake_case`; enum values `snake_case` (`#[serde(rename_all = "snake_case")]`,
  not `lowercase`, so multi-word variants stay readable); optional fields are `null`-able, never
  sentinel values (`-1`, `""`).
- A15 MUST: units in the field name, one convention per quantity:

| Quantity | Type / unit | Suffix | Example |
|---|---|---|---|
| Point in time | `u64` ms since the Unix epoch, UTC | `_ms` | `start_ms`, `read_ms` → prefer `read_at_ms` for clarity |
| Duration | `u64` seconds (ms only for sub-second) | `_s` / `_ms` | `uptime_s`, `duration_s` |
| Size, capacity | `u64` bytes | `_bytes` (new fields); legacy `size` = bytes | `usable_bytes` |
| Throughput | `u64` **bytes** per second | `_bytes_s` | `read_bytes_s` (never `bps`, which reads as bits) |
| Link speed | `u32` Mbit/s | `_mbps` | `speed_mbps` |
| Percentage | `f32` 0..=100 | `_pct` | `life_remaining_pct` |
| Temperature | `f32` °C | `_c` | `temperature_c` |
| Counters | `u64` | plural noun | `reallocated_sectors` |

  Times as text ("YYYY-MM-DD HH:MM:SS", local time) are not allowed in API types; convert at the edge.
  Tuples (`Vec<(String, String)>`) are not allowed in API types: they serialize as arrays; use a struct.
- A16 MUST: one type per concept. A disk is `DiskSummary`, an array `MdArray`, a health state one of
  two enums (`HealthVerdict` for devices, `ArrayHealth` for redundancy). Free-text `String` fields for
  closed sets (`power_state: String`, `level: String`) are replaced by the shared enums.
- A17 MUST: lists are bare JSON arrays when bounded by the system (disks, arrays, shares, users).
  History lists (tasks, revisions, logs, notifications) take `?limit=` (default 50, max 500, the same
  for every list) and `?before=<cursor>` (the `id` or `ts_ms` of the last item), newest first; the next
  cursor is the last item itself. Filters are query parameters named after the field (`?status=running`,
  `?type=config.apply`). Sorting is fixed per endpoint and documented.
- A18 SHOULD: an object that may be stale says so (`stale: true` + `read_at_ms`), never silently.

### 1.5 Pending changes, apply, tasks, events
- A19 MUST: every configuration domain follows the step-6 model: `GET /<domain>` → `{applied, pending?, …}`,
  `PUT /<domain>` stages (200 + new state), apply/discard only through `/config/apply` and
  `/config/discard`. No endpoint writes system configuration directly.
- A19b MUST: one rule for when a change takes effect. **A form inside a dialog or a
  single-object editor** (one user, one share, one task, one disk, the SMB settings card)
  has a primary **Save** that stages the change and applies **only the keys that form
  writes**: `POST /config/apply` with `keys` (every key that endpoint can stage; keys with
  nothing staged are ignored, so listing a superset is safe). The other staged keys of the
  user stay in the pending bar, untouched. The apply task's progress shows inside the form,
  a failure keeps the dialog open with the task's error and the note that the previous
  settings are back in place (the pipeline rolled back) while the change stays pending.
  **Row actions in lists** (enable/disable switches, deletes from a table) and page-level
  work that is meant to be reviewed together still only stage: the global pending bar
  applies them. Network changes keep their own trial apply (`/network/apply`). Keys are
  whole domain documents: applying `users` from a user dialog also applies other staged
  changes of `users`; drift detection, conflicts (only among the selected keys) and rollback
  are those of a full apply. After an apply, state that views cache and that the apply has
  just changed is read again before the task ends (`Ops::after_apply`: the disk settings
  re-read APM and write cache with `smartctl -n standby -g apm -g wcache`, never waking a
  sleeping disk), so a client that reloads on the task's end sees the new values.
- A20 MUST: long work is a `rynas-tasks` task with a UPID, progress, log and cancel. Clients follow it
  through `/tasks/stream`; polling `/tasks/{id}` is a fallback only.
- A21 MUST: SSE event names are `snake_case` verbs or nouns (`sample`, `started`, `updated`,
  `finished`), data is one JSON object of a type in `rynas-api-types`, serialized **once** per event
  (not once per subscriber). A lagged subscriber gets an explicit `resync` event (the client then
  re-reads the collection), never a silent gap. Keep-alive comment every 15 s.
- A22 SHOULD (before phase 1 ends): one multiplexed `GET /api/v1/events` stream per tab
  (`event: metrics.sample`, `event: task.updated`, `event: notification`…), because HTTP/1.1 browsers
  allow 6 connections per origin and every extra stream per tab steals one from fetches.

### 1.6 Sessions, CSRF, timeouts
- A23 MUST: every endpoint except `auth/login` and `auth/session` requires the session middleware;
  every non-GET/HEAD/OPTIONS requires `X-CSRF-Token`. Unknown `/api/**` paths answer 401 without a
  session (also outside `/api/v1`, so `/api/v2/x` does not reveal what exists).
- A24 MUST: SSE and other long responses end when their session ends.
- A24b MUST: only user-initiated requests renew the idle timeout. Streams never do; requests the
  client makes on its own carry `X-Rynas-Background: 1` (`api::send` sets it when the user has not
  typed, clicked or scrolled for 60 s), so a new poller needs no extra code to respect it.
- A25 MUST: each upstream call has an explicit timeout; a timeout maps to `504 timeout`, an unreachable
  rynasd to `503 backend_unavailable`. Server-side work that outlives its client is cancelled or bounded.
- A26 MUST: API responses are `Cache-Control: no-store`; static hashed assets are `immutable`.

---

## 2. GUI

### 2.1 One component per pattern
Pages compose these and nothing else. A new pattern is added to `src/ui.rs` (or its own module)
first, reviewed, then used.

| Pattern | Component | Notes |
|---|---|---|
| Page header | `PageHeader` | icon + title + subtitle + optional actions slot |
| Card / widget | `card widget w-N` with `WidgetHead` | one class set; no ad-hoc `card section` |
| Table | `ui::table` + `row` / `row_class` / `row_flag` + `cell` / `actions` | non-generic helpers (one compiled copy, wasm size); heads are i18n keys (`#key` numeric, `""` actions); `data-label` for the card layout < 768 px; only special widgets (permission matrix, expandable detail rows) write `<table>` themselves |
| Form field | `form::text_field`, `select_field`, `segmented`, `switch`, `number_field`, `area_field` | label, control, help, error (client rule or `ApiError.field`), `aria-describedby`; the error shows once the field has been left (polite live region) |
| Settings draft | `form::Draft` + `form::stale_notice` | a settings form built once; a clean draft follows the server, an edited one shows "changed elsewhere — reload" |
| Status pill | `Status` | tone + icon + text; never hand-written `<span class="status …">` markup |
| Empty state | `EmptyState` (page or tab), `ui::empty_line` (a section) | icon, title, text; an action only when the page header or tab bar does not already have it (never two primaries for the same thing) |
| Loading | `Skeleton` | always inside the card it replaces, same height |
| Fetch + states | `ui::Remote` (`new`, `with_config`), `ui::loading`, `ui::Pending` (shared `live::Res`) | skeleton → content \| `ErrorBox` with Retry; data set only when the answer changed |
| Error state | `ErrorBox` (an error code), `ui::error_box_text` (a message from `ApiErr::text`) | i18n text, Retry button; replaces the skeleton when a fetch fails; never a bare `note(Tone::Err)` or `err_view` for a failed read |
| Error text | `ApiErr::text()` + `ui::err_view` | translated code; the server's English message only as a smaller detail (`lang="en"`), never the main text |
| Dialog | `ui::dialog` (`Modal` component with `foot`, `drawer`, `drawer_wide`) | native `<dialog>` with `showModal` (top layer, inert page, focus to the first field, restore on close, Esc), kinds normal / wide / drawer / wide drawer (`DlgKind::WideDrawer`, tables) / alert; the buttons always go in the footer (`foot`), never in a hand-written `.dialog-foot` inside the body (test `dialog_buttons_go_in_the_footer`); a dialog with stages computes its footer per stage (`storage::volume::Flow`) |
| Confirmation | `ui::confirm` + `ui::Confirm` | Cancel focused first, busy + error inside, optional extra content; the title is a question ("Delete user {name}?"), the buttons [Cancel] [the same verb]; type the name (`typed`) only when user data is destroyed for good (rollback, a folder deleted with its data, an app's data), not for removing a configuration, a hash database or one snapshot; no inline "are you sure" rows in forms |
| Short confirmation | `toast::toast` | finished work ("saved, pending apply", "task finished/failed"); `aria-live="polite"` (assertive for errors), paused on hover/focus |
| Action outcome | `ui::Notice` | a result the user must read (restore, drift, LUKS header); tone + icon + text, dismissable, `role=status`/`alert` |
| Static note | `ui::note` | a warning or hint next to what it concerns; not dismissable |
| Form error | `form::form_error` | errors inside a form or dialog (`role="alert"`) |
| Pending changes bar | `config::PendingBar` | data from `/config/pending`; pages call `config::changed()` after staging (row actions, page-level work) |
| Save of a dialog / editor | `config::apply_now(&[config::keys::…], error)` | after the form's PUT/POST: applies only those keys (A19b), progress in the form's `form_error` box (`config::apply_line`), `toast::saved_applied` and close on success, error + "previous settings back, change stays pending" on failure; the button says **Save**/**Create**, never "Stage" |
| Chassis drawing | `chassis::chassis` (`src/chassis.rs`) | bays side by side and M.2 slots below, rows keyed by slot with a memo per value (a poll rewrites only what changed); provider parts optional (status LED, identify, disk LEDs off); light and dark `--chassis-*`/`--bay-*`/`--led-*` tokens |
| Task progress | `tasks::TaskPanel`, `tasks::TaskRun`, `tasks::LogTail`, `tasks::run_and_report` | `task.*` events of `/events`; a button that starts a task uses `tasks::run_and_report` (busy, refusal in the dialog's `form_error`, outcome in a toast, then `done`) or `tasks::run_task` / `api::start_task` + `tasks::follow`; no page polls a task |
| Status drawer | `ui::detail_drawer` + `ui::Danger` (`ui::drawer_foot` when the footer changes) | the PC backup drawer is the model: one `.status-card` on top (state, `.status-meta` line, the lifecycle actions: Run now/Cancel, Up/Stop…), sections with their own actions on the title row (`.sec-head`), rare parts folded (`ui::fold` in `.folds`), footer = the danger action on the left (`foot-start`), secondary actions (Settings, Edit) on the right, Close (ghost) last |
| Editor footer | `form::save_bar` / `form::save_bar_with(SaveBar { … })` | the form error (or in the body), Cancel (ghost, off while busy), an optional secondary ghost button (Validate, Test), the action on the right (primary or danger, spinner, optional icon, `form` to submit a `<form>`); never hand-written Cancel/Save rows |
| Close | `ui::close_foot` | a dialog that only shows something ends with Close, ghost, bottom right; never primary (test `close_is_never_primary`) |
| Fold | `ui::fold` / `ui::fold_with`, `ui::Folds` | advanced options, history, logs, rarely needed parts; one `.fold` class (chevron + `.fold-title`); `Folds` keeps the open state while a poll rebuilds the content |
| Tag | `ui::chip(Tone, …)` | a short tag next to a name (SMR, weak key, erases); `.chip.ok/.warn/.err/.info`; a state is a `Status` pill |
| Key/value | `ui::kv` (any view), `ui::kv_text` (text + class), `ui::kv_row` (signal) | no page-local `fn kv` |
| Missing value, dates | `ui::DASH`, `ui::when(Option<ms>)` | never a literal "—" |
| Copy | `ui::copy_button`, `ui::copy_button_for` (own accessible name), `ui::code_block` (titled command + Copy), `ui::copy_text` | works on plain-HTTP LAN addresses (no direct Clipboard API calls) |
| Row action | `ui::icon_btn` + `ui::verb_name` | icon-only buttons named "<Verb> <name>" (tooltip and `aria-label`), `danger` for delete/remove (test `row_actions_are_named_and_toned`); the row's name is the link that opens its drawer |
| Spinner | `form::spinner` (a `RwSignal`), `form::spinner_when` (any condition), `form::spinner_icon` (in place of the icon) | no hand-written `.btn-spinner` |
| Live data | `live::poll(every, tick)`, `live::poll_while(ms, busy, reload)` | only while signed in and the tab is visible (`live::should_poll`), stopped with the page; data that changes only through the UI is not polled |

- G1 MUST: no `inner_html` with markup built from strings, except the static icon SVGs in `icons.rs`.
  Wasm size is handled with shared non-generic helpers, not by bypassing components.
- G1b MUST: buttons. One primary action per view: page-wide actions in the page header, the
  action of a tab in that tab's bar (the header shows only the open tab's action), never repeated
  by an empty state. Destructive actions are `danger` and sit on the left of a footer or as a
  danger icon in a row. Refresh buttons only where the data is not live, as a ghost icon button
  in the tab bar. Every button has a variant (`primary`, `ghost`, `danger`); "…" only when a
  dialog asking for more input follows, never for a plain confirmation. One settings icon
  (`icons::SETTINGS`).
- G2 MUST: every page has the same three states: skeleton → content | error state. A failed fetch is
  never silent (`if let Ok(...)` that drops the error is a review blocker in pages).

### 2.2 Tokens, type, spacing, icons
- G3 MUST: colours only through tokens (`var(--…)`), defined once for light and dark in `:root` /
  `[data-theme=dark]`. Hex/rgb literals outside the token block are not allowed (check:
  `awk` out the token block, then `grep -nE '#[0-9a-f]{3,8}|rgba?\('` must be empty). Physical
  hardware drawings (chassis, bay LEDs) use `--chassis-*`/`--led-*` tokens.
- G4 MUST: font sizes from `--fs-12 … --fs-32` only; spacing from the 4 px scale tokens
  (`--s-1`=4px, `--s-2`=8px, `--s-3`, `--s-4`, `--s-6`, `--s-8`, `--s-12`); radius `--r-xs|sm|md|lg|xl|pill`; control heights `--input-h` / `--ctl-sm` (they follow the density).
  No 2/3/5/6/7/10 px paddings or gaps (`--s-half` = 2 px exists only for the optical alignment of an
  icon with a text line). Checked by the `style_check` tests of rynas-ui (balanced blocks, no colour
  literal outside the token blocks, no px spacing or font size off the scale, no rule set twice, no literal radius or 36/28 px control height).
- G5 MUST: icons from the Lucide set in `icons.rs` (Tabler only where Lucide has none); sizes 12/14/16/18/22.
- G6 MUST: state = icon + text + colour (`Tone`), never colour alone.
- G7 MUST: nothing scrolls sideways: not the page and not any element inside it (tables, dialogs,
  dialog tabs, matrices, cards), in en and it, at 390, 768, 1024 and 1280 px. Tables wrap text to fit
  their card (only numbers, actions and status pills stay on one line); a matrix uses a fixed layout
  with wrapping headers. Check every route and every dialog tab for elements with
  `overflow-x: auto|scroll` and `scrollWidth > clientWidth`, not only `documentElement`. The only
  exceptions are content that is wide by nature and labelled as such (log viewers, code blocks).
  A CSS rule that allows horizontal scrolling must be listed with its reason in the `style_check`
  test `horizontal_scroll_containers_are_listed`. Drawings (chassis, bays) wrap instead of scrolling.

### 2.3 Text and formatting
- G7 MUST: no user-visible literal in views; everything through `t`/`tr`/`tf`. Product names and
  key caps (`rynas`, `Ctrl K`, `Esc`) are the only exceptions.
- G8 MUST: every key has an explicit Italian text (the dictionary macro requires both), and
  no case transformation of translated text (`to_lowercase()` on `t(..)`): add a separate key.
- G8b MUST: one key per text and one prefix per area. Keys are `<area>.<thing>` (`sto.scrub`,
  `net.dns`, `shares.folder`); words every area uses are bare (`cancel`, `back`, `save`, `edit`,
  `edit_named`, `run_now`, `not_set`) or in a shared group (`col.*` column headers, `health.*`,
  `pending.*`, `task.st.*`, `task.<type>` task names). Before adding a key, search the dictionary for
  the text. The test `no_duplicate_texts` fails on two keys with the same English and Italian text
  of more than one word.
- G9 MUST: numbers, bytes, rates, percentages, temperatures, durations and **dates** only through
  `i18n.rs` helpers (`num`, `count`, `bytes`, `bytes_si`, `rate`, `bitrate`, `pct`, `celsius`,
  `duration`, `elapsed`, `approx`, `ago`, `datetime`, `date`, `time_hm`, `short_when`, `hm`/`hm_text`,
  `wall_when`, `date_with`). Raw device capacity (disks, arrays) uses `bytes_si` (matches the drive
  label); filesystem/volume usage and memory use `bytes` (IEC). The same quantity is shown with the same
  helper on every page.
- G9a MUST: one locale for the whole interface (core, modules, File app). `i18n::locale()` is the UI
  language with the region of the browser's own tag when the languages match (`en-GB`, `en-US`,
  `it-CH`), else `en-US` / `it-IT`; it is published on `<html lang>`, and the time format preference
  (System › Preferences, "Time format": automatic / 24-hour / 12-hour = `hourCycle` `h23`/`h12`,
  localStorage `rynas.hour`) on `<html data-hour-cycle>`. The File app reads both (`i18n.js` `LOCALE`,
  `HOUR_CYCLE`, `format.js` `dateFormat`); never `Lang::code()`, `undefined` or a hard-coded
  `"en-GB"` as a locale, never `getHours()` padding by hand. Times typed in forms stay stored as
  `HH:MM`; they are shown through `i18n::hm`/`hm_text`. Server wall-clock strings without a zone
  ("2026-11-04 08:02:51") go through `i18n::wall_when`.
- G9b MUST: separators, percent sign and plural categories come from `Intl` (`Intl.NumberFormat`,
  `Intl.PluralRules`) for the locale; unit words and joining templates are dictionary keys (`dur.*`,
  `fmt.ago` "{time} ago", `fmt.of` "{part} of {total}"), never concatenated (`format!("{} {}", d,
  t("ago"))`) and never chosen by `lang() == Lang::It`. The File app uses the same texts (test
  `files_formats_match_core`). A ratio is `i18n::of` / `bytes_of` ("1.50 GiB of 4.00 GiB"), not
  "X / Y".
- G9c MUST: counted texts use `i18n::tn(key, n, args)`: the form `key.<category>` for the CLDR category
  of `n` (`key.one`, `key.few`, `key.many`…), `key.other` required (test `plural_keys_have_other`),
  `{n}` filled with the grouped count; no `if n == 1 { "x_one" }` pairs. The File app's `count()` picks
  its form by the same category.
- G9d MUST: sizes have three significant digits and move to the next unit before four digits ("1.00
  GiB", never "1022 MiB"); decimal units are `kB`, `MB`, `GB`. Server texts (task logs, errors, event
  fields) use `rynas_api_types::units::human_bytes` (binary units, `.`); the interface localizes the
  decimal mark of such event fields. Better still, an API or event carries the number and the client
  formats it.

- G8e MUST: server texts that users read (check notes, recommendations, health reasons, plan
  warnings) travel with a stable code and their values (`note_code`, `CheckValue`,
  `HealthReason.args`, `PlanWarning.args`); the UI translates `<area>.<kind>.<code>` and fills
  the values with the `i18n.rs` helpers, and shows the English text (`lang="en"`) only for a
  code it does not know. A test reads the server source and fails on a code without a key.
- G8c MUST: no comparison with other products in user-visible text ("like DSM", "DSM's default",
  "as in Windows Server"…): describe what rynas does. Reviewers grep the dictionary for product names.
- G8d MUST: hardware/platform-specific screens, tabs, hints and texts (Synology, synodeb, synohwd,
  synodeb-ups, synodeb-dom) appear only when that plugin/backend is present on the machine; a generic
  machine sees neither the screen nor a "not a Synology"/"needs synodeb" placeholder.

#### Glossary and text style (G8f)
Every text of the core, the modules, the File app and the server's event texts uses these words.
Before adding a text, check the glossary; a new concept gets a row here first.

| Concept | English | Italian |
|---|---|---|
| this machine | NAS (never "server") | NAS (mai "server") |
| a remote host (replication, backup, SMB/SFTP connection) | server | server |
| the File app | Files | File (never "file manager", "gestore file", "app File") |
| shared folder (the object) | shared folder | cartella condivisa |
| network export of it | share (SMB/NFS share) | condivisione |
| physical device | drive | disco |
| removable device | removable drive | unità rimovibile |
| disk of the OS | system disk | disco di sistema |
| btrfs subvolume | subvolume | sottovolume (never "subvolume") |
| snapshot | snapshot | snapshot (never "istantanea"; masculine: lo/gli snapshot) |
| snapshot job | job | job |
| background operation | task | attività |
| scheduled command | scheduled task | attività pianificata |
| replication | replication | replica |
| get files back | restore | ripristina / ripristino |
| go back to a version | roll back / rollback | button "Torna alla versione precedente", task "Ritorno alla versione precedente", text "ritorno alla versione precedente" (never "ripristino" or "rollback") |
| back to defaults | reset | reimposta |
| out of the configuration, data stays | remove | rimuovi |
| data destroyed | delete | elimina |
| destroyed without recycle bin | delete permanently (never "for good") | elimina definitivamente |
| wipe a drive | erase | cancella |
| recycle bin | recycle bin (lowercase) | cestino (minuscolo) |
| changes not applied yet | pending changes (never "staged") | modifiche in sospeso (never "preparate", "in attesa") |
| make them effective | apply (a verb only, never "the apply") | applica |
| drop pending or edited changes | discard | scarta ("Scarta le modifiche") |
| close without doing anything | cancel | annulla (only this meaning) |
| undo | undo | annulla l'ultima modifica |
| server-side configuration | settings (Tailscale too, never "options") | impostazioni |
| browser-side | preferences | preferenze |
| try a setting | test | prova |
| check integrity | check | verifica |
| btrfs/RAID scrub | data scrub | pulizia dati |
| retention | retention | conservazione |
| permissions | permissions | permessi (never "autorizzazioni") |
| encryption at rest (LUKS, encrypted folders) | encrypted / encryption | cifrato / cifratura (crittografia only for SMB and TLS) |
| web UI session | sign in / sign out | accedi / esci |
| hardware source (synohwd, a module) | hardware source | sorgente hardware (never "provider") |
| beeper | beeper | cicalino |
| a page as an entry of its own in the sidebar | pin / unpin, pinned page | fissa / stacca, pagina fissata |
| the side navigation | sidebar | barra laterale |

- Statuses are one word each, the same everywhere: Running "In esecuzione", Healthy "Integro",
  Warning "Avviso", Notice "Informazione", Failing "In avaria", Done "Completato/a", Never run "Mai
  eseguito/a", Canceled "Annullata".
- English is **American** (catalog, canceled, color, favorite, behavior, recognized, license, gray,
  minimize) and in **sentence case**, navigation and page titles included ("Shared folders", "Users
  and groups", "Apps and plugins").
- Buttons: Italian imperative ("Crea", "Sposta"), never an infinitive; "Create <object>" for a new
  thing, "Add <object>" to connect something that exists, no article ("Add PC", "Create backup
  job"); "Run now" (`run_now`) has the play icon and the subject in the tooltip / `aria-label`
  (`ui::verb_name`); "Done" is "Fatto"; page forms are [Reset][Save] ("Reimposta", "Salva").
- Titles: imperative ("Sposta le cartelle home"); a confirmation title is a question ("Delete user
  {name}?", "Eliminare l'utente {name}?") and never reuses the button's key.
- Task labels: English imperative ("Install Tailscale", "Roll back a stack"), Italian noun phrase
  ("Installazione di Tailscale").
- Success messages (toasts, short outcomes) end without a period.
- Quotes: “…” in English, «…» in Italian; never straight quotes around words.
- Technical names stay out of the main text: no "units"/"timers"/"systemd" where "schedule" says it.

### 2.4 Accessibility
- G10 MUST: every control has an accessible name (label, `aria-label` via `tr`), focus is visible
  (`:focus-visible` token outline), dialogs trap focus and restore it on close, `Esc` closes overlays.
- G11 MUST: live regions — toasts `aria-live="polite"`, form errors `role="alert"`, field errors
  `aria-live="polite"` (shown after the field is left), connection state
  `role="status"`. Charts have `role="img"` and a text `aria-label` with the current value.
- G12 MUST: text contrast ≥ 4.5:1, UI borders/icons ≥ 3:1 in both themes (tokens in `02-ui.md` are
  already corrected; new tokens need their ratio in the PR).

#### Dark theme palette (Marco 2026-11-06)
The dark theme uses **SynoFile's dark palette** (`synofile/ui/app.css`), not Tokyo Night storm:
neutral graphite layers and near-white text, for readability and so the file manager looks the
same inside rynas as on its own. Roles are unchanged; values are SynoFile's, or derived from it
where SynoFile's own value is below AA (its `--faint` #667085 is 3.3:1 on its surface, so
`--text-subtle` is its `--muted` #98a2b3 and `--text-muted` a lighter #b0b8c4; `--border-strong`
is lifted to #727b90 for 3:1 input borders; `--accent-2`/`--danger` slightly lighter than
#8b7cf6/#f06b6b so they stay ≥ 4.5:1 on hovered rows and tinted chips). The light theme is the
Tokyo Night Light palette of `02-ui.md`, unchanged.

| Layer | Token | Dark |
|---|---|---|
| page | `--bg` / `--bg-sunken` (inputs, code) | #14171c / #0f1216 |
| sidebar / app bar | `--surface-nav` / `--surface-appbar` | #111418 / #171a20 |
| cards, dialogs | `--surface` / `--surface-2` / `--surface-striped` | #1c2027 / #222731 / #191d23 |
| hover, raised controls | `--surface-hover`, `--surface-3` | #2a303c |
| borders | `--border` / `--border-raised` / `--border-strong` | #2e3440 / #3b4252 / #727b90 |
| text | `--text` / `--text-muted` / `--text-subtle` / `--text-disabled` | #e4e7ec / #b0b8c4 / #98a2b3 / #667085 |
| accent | `--accent` / `--accent-2` / `--on-accent` | #5b9cf5 / #978af7 / #14171c |
| states | `--info` / `--success` / `--warning` / `--danger` | #56c2e6 / #3cc47c / #e7a93a / #f37575 |
| charts | `--c1`…`--c5` | accent, accent-2, info, success, warning |
| file manager | `--row-sel`, `--eza-*` | #203253; eza's colours = SynoFile's terminal palette (#89b4fa dirs, #a6e3a1 files…), grey and system folders lifted to ≥ 4.5:1 |
| chassis | `--chassis` / `--bay` / `--bay-2` | #101318 / #2f3541 / #232832 |

Contrast (WCAG 2.x ratio, text on background; ≥ 4.5 required, `--border-strong` ≥ 3):

| Token | Dark | on `bg` #14171c | on `bg-sunken` #0f1216 | on `surface` #1c2027 | on `surface-2` #222731 | on `surface-hover / surface-3` #2a303c | on `row-sel` #203253 |
|---|---|---|---|---|---|---|---|
| `--text` | #e4e7ec | 14.5 | 15.1 | 13.2 | 12.1 | 10.7 | 10.3 |
| `--text-muted` | #b0b8c4 | 9.0 | 9.4 | 8.2 | 7.5 | 6.6 | 6.4 |
| `--text-subtle` | #98a2b3 | 7.0 | 7.3 | 6.3 | 5.8 | 5.1 | 5.0 |
| `--accent` | #5b9cf5 | 6.4 | 6.7 | 5.8 | 5.4 | 4.7 | 4.6 |
| `--accent-2` | #978af7 | 6.2 | 6.5 | 5.7 | 5.2 | 4.6 | 4.4 |
| `--info` | #56c2e6 | 8.8 | 9.2 | 8.0 | 7.3 | 6.5 | 6.2 |
| `--success` | #3cc47c | 8.0 | 8.4 | 7.3 | 6.7 | 5.9 | 5.7 |
| `--warning` | #e7a93a | 8.7 | 9.1 | 7.9 | 7.2 | 6.4 | 6.2 |
| `--danger` | #f37575 | 6.5 | 6.8 | 5.9 | 5.4 | 4.8 | 4.6 |
| `--border-strong` (UI) | #727b90 | 4.2 | 4.4 | 3.9 | 3.5 | 3.1 | 3.0 |
| `--eza-blue` | #89b4fa | 8.5 | 8.9 | 7.8 | 7.1 | 6.3 | 6.1 |
| `--eza-green` | #a6e3a1 | 12.1 | 12.6 | 11.0 | 10.1 | 8.9 | 8.6 |
| `--eza-yellow` | #f9e2af | 14.1 | 14.8 | 12.9 | 11.8 | 10.4 | 10.1 |
| `--eza-red` | #da8199 | 6.5 | 6.8 | 5.9 | 5.4 | 4.8 | 4.6 |
| `--eza-purple` | #f5c2e7 | 11.8 | 12.3 | 10.7 | 9.8 | 8.7 | 8.4 |
| `--eza-cyan` | #89dceb | 11.5 | 12.1 | 10.5 | 9.6 | 8.5 | 8.2 |
| `--eza-white` | #bac2de | 10.1 | 10.6 | 9.2 | 8.5 | 7.5 | 7.2 |
| `--eza-gray` | #949bae | 6.5 | 6.8 | 5.9 | 5.4 | 4.8 | 4.6 |
| `--eza-sys` | #959cb2 | 6.6 | 6.9 | 6.0 | 5.5 | 4.8 | 4.7 |

`--accent-2` on `--row-sel` (4.4) does not occur: no accent-2 text on a selected file row. Filled
badges and buttons put `--on-accent` #14171c on the fill: 6.4 on accent, 8.0 success, 8.7 warning,
6.5 danger. Chassis: `--chassis-text-3` 4.8 on a bay tile, LEDs ≥ 4.5 on the tile, the bay
outline on hover 4.4 on the case. A new dark token needs its row here.
- G13 MUST: honour `prefers-reduced-motion` (no chart slide, no skeleton shimmer).

### 2.5 Real data only
- G14 MUST: release builds never show fabricated data. The demo data set is a separate cargo feature,
  **off by default**, enabled only for `trunk serve`/screenshots; an unreachable API shows the offline state.

---

## 3. Speed

### 3.1 Budgets
| What | Budget | How to measure |
|---|---|---|
| UI wasm | ≤ 2 000 KiB brotli -q 11 (raised from 350 KiB, then 900 KiB, on 2026-11-05; to 1 500 KiB on 2026-11-07; to 2 000 KiB on 2026-11-11, Marco: it is what the browser downloads once, cached, not NAS memory) | `trunk build --release` then `brotli -q 11 -c dist/*.wasm \| wc -c` |
| JS glue | ≤ 12 KiB brotli | same on `dist/*.js` |
| CSS + preloaded fonts | ≤ 60 KiB brotli | same on `dist/*.css` + `dist/fonts/*.woff2` |
| First paint (LAN) | < 300 ms | Chrome DevTools, "Fast 3G" off, CPU 4× slowdown ≈ J4125-class client is not the issue; server on the DS920+ |
| Interactive | < 800 ms | Performance panel: wasm instantiate + first data render |
| Page change | < 50 ms | Performance panel, cached data |
| API GET (in-memory data) | p50 < 5 ms, p99 < 20 ms on J4125 | `curl -w '%{time_total}'` loop, or `scripts` in the review |
| API GET (runs a tool: lsblk, mdadm, btrfs) | p50 < 150 ms on J4125, cached ≥ 10 s | same |
| Idle backend | < 1% CPU per process on J4125 | `/proc/<pid>/stat` utime+stime over 60 s |

The wasm budget includes the default features of the release build. Track sizes in every UI PR
(before/after line in the description). Timings: `tools/ui-audit/perf.py` (startup, page changes,
idle main-thread work, the Files page, per endpoint; CPU 4× and 6× + network throttling, cold and warm);
a change that claims speed shows its numbers with `--compare`.

How the wasm is built (A/B measured 2026-11-11 on the same code, brotli -q 11): LLVM `opt-level = "z"`
for the UI crate and its dependencies (profile `wasm-release`; "s" for the UI crate gave +16 %), then
`wasm-opt -O2 --low-memory-unused` (`index.html`). `-Oz` was the largest after brotli (its shrinking
passes leave less for the compressor): `-O2` is 1–2 % smaller and runs the optimizations meant for
speed. `--low-memory-unused` is safe because rustc lays the stack out first (stack pointer and
data at 1 MiB): the low KiB is reached only by a stack overflow. JSON goes through `api::decode`
only (never `serde_json::from_str` in the UI): one reader frame for every type, and structs read
as objects only (`json::Objects`), which drops the array path serde derives for each struct.

Only the English texts are compiled into the wasm (2026-11-07): the Italian column of the
dictionaries is `crates/rynas-ui/assets/i18n/it.txt` (served as `/i18n/it.txt`, `key<TAB>text` lines,
about 44 KiB brotli; a key the file lacks stays English), written by the test `i18n::tests::italian_file_is_current` (after a text change it fails
once and rewrites the file: commit it). When the language is Italian, `index.html` starts the
download while the wasm loads; the boot screen waits for it at most 1.2 s, then the interface
shows in English and switches to Italian when the file arrives.

### 3.2 Rules
- S1 MUST: live data through SSE; polling only for data without an event source, ≥ 5 s, paused when
  `document.hidden`, and stopped when no visible component needs it. Hardware status comes from
  `hardware.status` on `/events` (no `/hardware` polling); `/config/pending` is polled compact
  (`?detail=1` only for the review dialog).
- S2 MUST: charts on `<canvas>`; series data and sizes read once per sample (or layout change),
  theme colors once per theme (`getComputedStyle` forces a style recalculation of the page), and
  nothing read from the DOM inside the animation loop: `clientWidth` in a frame forces a layout.
  Live charts scroll continuously by timestamp (decided 2026-11-05 by Marco: smooth, not per sample) at
  30–60 fps, starting 1.2 s after the chart appears (the first page is still loading then), paused in
  hidden tabs and when the chart leaves the page, static with reduced motion.
  Other animations ≤ 200 ms (the plan's 120–200 ms rule).
- S2b MUST: anything that animates for long (skeletons, the live indicator) animates `transform` and
  `opacity` only, in its own layer. `box-shadow`, `background-position` and the like repaint on the
  main thread every frame; on the app bar that also repaints what the blur behind it covers.
- S3 MUST: fine-grained signals: a sample updates only the text nodes that show it; `Memo` for derived
  values that feed DOM attributes; no `Vec` clones of the whole history per tick.
- S4 SHOULD: features over ~20 KiB brotli (file manager, terminal, QR/TOTP enrolment, charts of long
  ranges) are lazy-loaded (separate wasm module or Web Component) once the plugin loader exists.
- S5 MUST: static assets precompressed at build time (`.br` q11 + `.gz` -9) and served as is; dynamic
  compression only for API JSON and only with gzip or brotli quality ≤ 4 and window ≤ 18 (`lgwin`), or
  not at all for bodies < 1 KiB. Fonts and other unhashed assets get a hash in the name or
  `max-age=86400`.
- S6 MUST: never wake a disk: no SMART, `hdparm`, `blkid`, `btrfs filesystem show` (device scan) or
  directory walk of a data volume on a timer; power state via ATA CHECK POWER MODE; anything that may
  touch a sleeping disk is on demand and cached (see `docs/phase1/plan.md`, storage rules).
- S7 MUST: the async runtime never blocks: file, process and D-Bus work goes to `spawn_blocking` or
  a dedicated thread, **with a timeout that frees the thread**. Calls that can hang forever (statvfs on
  network filesystems, reads on hung devices) run on a dedicated small pool or a child process, never on
  the shared blocking pool that also serves static files.
- S8 MUST: results of external tools are cached with an explicit TTL and a single-flight guard
  (concurrent requests share one run), and an endpoint read at every start of the interface runs
  none (`GET /system/power?pending_only=true`: the expensive warnings only for the dialog).
- S9 MUST: every byte of the first screen earns its place. A signed-in page reload starts the first
  screen's API calls from `index.html` while the wasm is loading (`window.__rynasEarly`, taken once by
  `src/api.rs`, read in full by the page, used only if successful and within 10 s of the page's time
  origin, otherwise asked again); `/metrics/history` carries per-thread and temperature data on its newest point only.
- S10 MUST: assets a build can change are named after their content and served immutable (trunk
  hashes; `hash-files-app.sh` renames `files-app/` the same way), `index.html` is `no-cache` + `ETag`.
  No service worker: the interface is tied to the server's version, and a stale worker is the failure
  this rule prevents (see the measurements in `tools/ui-audit/perf.py`).

---

## 4. Memory

### 4.1 Budgets (RSS on the DS920+, steady state)
| Process | Idle | Busy (UI open, 3 SSE clients, a task running) | systemd limit |
|---|---|---|---|
| rynas-web | < 12 MB | < 20 MB | `MemoryHigh=48M`, `MemoryMax=96M` |
| rynasd | < 12 MB | < 24 MB (+ children) | leaf `daemon/` 96M/256M (children: mdadm, smartctl, btrfs), leaf `workers/` 192M/384M (file manager workers), unit `MemoryHigh=288M`, `MemoryMax=640M` (`rynasd::cgroup`) |
| each plugin process | < 8 MB | < 16 MB | `MemoryMax=64M` unless the plugin documents more |
| task worker units (backup, dedup, AV) | — | documented per task | `MemoryMax` in the transient unit |
| UI wasm heap | < 16 MB after 1 h on the dashboard | | — |

### 4.2 Rules
- M1 MUST: every collection that grows with input has a hard cap and an eviction rule, written next to
  its definition: maps keyed by user, IP, session, disk, task; caches; history rings; log buffers.
  Caps today: sessions (per user + global), rate-limit tables 10 000, metrics ring 3 600, task history
  500 / 30 days, task log 1 MiB, UI history 600.
- M2 MUST: channels are bounded (`broadcast(16)`, `mpsc(n)`); slow consumers lag and resync (A21).
  No `unbounded_channel`.
- M3 MUST: bodies are streamed or capped: request bodies ≤ 1 MiB unless the endpoint documents more;
  subprocess stdout/stderr captured with a cap (1 MiB) — one runner (`rynas_tasks::cmd::Cmd`) for every
  external command; upstream (rynasd) answers read with a cap.
- M4 MUST: long-lived per-sample structures avoid per-item heap strings: intern names
  (`Arc<str>` or an index into a series table) in rings that hold thousands of samples.
- M5 MUST (both daemons, at the top of `main`): `mallopt(M_ARENA_MAX, 2)`, `mallopt(M_MMAP_THRESHOLD, 256 KiB)`
  (a fixed threshold disables glibc's dynamic raise, which otherwise keeps every large transient buffer —
  argon2's 19 MiB, a brotli window, a 1 MiB log chunk — resident forever) and `M_TRIM_THRESHOLD` 1 MiB.
  Alternative: `GLIBC_TUNABLES=glibc.malloc.mmap_threshold=262144:glibc.malloc.trim_threshold=1048576:glibc.malloc.arena_max=2`
  in the units. No jemalloc/mimalloc unless a measurement shows a win on the J4125.
- M6 MUST: `current_thread` runtime, `max_blocking_threads` sized and documented, `thread_stack_size`
  512 KiB where deep recursion is impossible; big transient buffers (argon2, compression) are bounded by
  a semaphore.
- M7 MUST: systemd units carry `MemoryHigh`/`MemoryMax`, `TasksMax` and, for background work,
  `CPUWeight=`/`IOWeight=`/`Nice=`.
- M8 MUST (UI): closures registered on long-lived JS objects are dropped when the object is replaced
  (keep the `Closure` next to the `EventSource`; no `forget()` in code that can run more than once).

### 4.3 How to measure
```
# steady-state RSS, split anon/file, threads
grep -E 'VmRSS|VmHWM|RssAnon|RssFile|Threads' /proc/$(pidof rynas-web)/status
# proportional set size (shared libs split fairly)
awk '/^Pss:/{s+=$2} END{print s" kB"}' /proc/$(pidof rynasd)/smaps
smem -k -P 'rynas'                       # if installed
# where the heap is: mapping sizes
awk '/^[0-9a-f]+-/{n=$6} /^Rss:/{r[n]+=$2} END{for(k in r) if(r[k]>256) print r[k], k}' /proc/<pid>/smaps | sort -rn
# allocations over time (dev box)
heaptrack ./target/release/rynas-web --listen 127.0.0.1:18080 ...   # then heaptrack_gui
# CPU over 60 s (ticks → %)
a=$(awk '{print $14+$15}' /proc/<pid>/stat); sleep 60; b=$(awk '{print $14+$15}' /proc/<pid>/stat); echo "($b-$a)/60" | python3 -c 'print(eval(input()))'
```
Always report: idle after 1 min, after 1 login, after 5 parallel logins, with 3/20 SSE clients, after a
minute of mixed GETs, and after 1 h (full metrics ring). Run the release binaries from a private target
dir, `rynasd --dry-system` with a temp `--state-dir`, `rynas-web --users-file` on a free localhost port.

## 5. Verification

The root workspace does not cover the crates with their own workspace and lock file
(`crates/rynas-files-worker`, `crates/rynas-backup-worker`, `crates/rynas-agent`, listed in the root
`exclude`): `cargo test --workspace` and clippy skip them. Run them on their own, with
`CARGO_BUILD_JOBS=2` and a target folder on disk:
```
cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
cargo test -p rynas-ui && cargo clippy -p rynas-ui --target wasm32-unknown-unknown -- -D warnings
for c in rynas-files-worker rynas-backup-worker rynas-agent; do
  cargo clippy --manifest-path crates/$c/Cargo.toml --all-targets -- -D warnings &&
  cargo test --manifest-path crates/$c/Cargo.toml || break
done
```
`italian_file_is_current` (rynas-ui) rewrites `assets/i18n/it.txt` when it is stale: run it twice.
