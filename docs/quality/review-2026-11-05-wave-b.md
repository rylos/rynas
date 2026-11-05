# Quality review — 2026-11-05, wave B (after the UI consolidation)

Scope: `main` at `2181e0b`, i.e. the whole product after the UI consolidation pass (`faaf668` …
`8962714`) and the backend fixes of wave A (`099752c`). Axes: GUI coherence, API coherence, speed,
memory, plus the open point of the previous pass (a full page load that hangs in headless Chromium).
Rules `A*`, `G*`, `S*`, `M*` are in [`guidelines.md`](guidelines.md); the previous review is
[`review-2026-11-05-wave-a.md`](review-2026-11-05-wave-a.md). Line numbers are those of `2181e0b`
unless a finding says "after the fix".

## Measurements

Release builds from a private target dir, development workstation (8-core Ryzen, 16 threads).
`rynasd --dry-system` with a private socket and state dir, `rynas-web --users-file` on a localhost
port, headless Chromium driven over CDP.

### UI bundle (`trunk build --release`, brotli -q 11)
| File | Wave A after the pass | Now (after the fixes below) | Budget |
|---|---|---|---|
| `rynas-ui_bg.wasm` | 3 121 445 B raw, 828 842 B brotli | 3 121 915 B raw, **829 019 B brotli (809.6 KiB)** | ≤ 900 KiB — ok, **90 %** used |
| JS glue | 41 431 B, 8 113 B brotli | 41 431 B, 8 113 B brotli | ≤ 12 KiB — ok |
| CSS | 84 839 B, 13 970 B brotli | 84 997 B, 13 994 B brotli | CSS + fonts 53.6 KiB ≤ 60 KiB — ok |
| fonts (Inter + JetBrains Mono, latin) | 40 600 B | 40 600 B | |

### First paint (dashboard, precompressed assets, localhost)
| CPU | Load | First contentful paint | Page header in the DOM | First data rendered |
|---|---|---|---|---|
| 1× | cold cache | 116 ms | 76 ms | 101 ms |
| 1× | warm | 96–112 ms | 60–68 ms | 79–93 ms |
| 4× slowdown | cold | 312 ms | 222 ms | 304 ms |
| 4× slowdown | warm | 296 ms | 196–221 ms | 285–304 ms |

First paint is at the 300 ms budget only with a 4× CPU slowdown; interactive (< 800 ms) holds.

### Backend (RSS from `/proc/<pid>/status`, PSS from `smaps`)
| Process / state | VmRSS | RssAnon | PSS | Threads | fds |
|---|---|---|---|---|---|
| rynas-web, idle after login | 5.2 MB | 1.1 MB | 3.0 MB | 2 | 10 |
| rynas-web, 3 SSE clients + 30 s of mixed GETs (208 764 requests, 4 workers) | 6.9 MB | 2.1 MB | 4.7 MB | 6 | 10 |
| rynas-web, 20 `/events` streams open | 7.9 MB | 3.0 MB | 5.6 MB | 6 | 71 |
| rynas-web, 3 s after the 20 streams closed | 7.9 MB | 3.0 MB | 5.6 MB | 2 | **10** |
| rynasd, idle | 10.4 MB | 2.9 MB | 7.2 MB | 1 | 19 |
| rynasd, 20 `/events` streams open (2 upstream connections each) | 14.3 MB | 3.7 MB | 11.1 MB | 5 | 59 |
| rynasd, 3 s after the 20 streams closed | 14.3 MB | 3.7 MB | 11.1 MB | 1 | **19** |

Both daemons stay under 20 MB at every step; the extra rynasd RSS after the streams is file-backed
pages of its binary (anon +0.8 MB). File descriptors return exactly to the idle count: no stream,
upstream connection or task is left behind. `/events` ends at the next sample after logout (A24).
Idle CPU over 30 s: 1 tick (rynas-web), 0 (rynasd). Every GET of the UI answers in p50 0.53–0.76 ms,
p99 ≤ 1.6 ms under 4 parallel clients.

## The "8th reload" hang — cause and fix

**Verified, cause found, fixed (`9c3b26d`).** The server was not at fault: killing a client of
`/api/v1/events` frees its TCP connection and both upstream connections to rynasd within a second
(fd counts above). Plain reloads (`Page.navigate`, `Page.reload`) of the same or of different pages
never hung in 12–24 rounds. What hangs is a mix of full loads and **back/forward navigation**: on the
12th step of such a walk the page stopped at `readyState = interactive` with `/api/v1/auth/session`
pending for more than 20 s, with all six connections to the origin busy.

The pages were in Chromium's **back/forward cache**: a cached page keeps its open `EventSource`, so
each cached page holds one of the six HTTP/1.1 connections the browser allows per origin. Counting
the rynasd sockets (2 per open `/events`) on an A → B → back → forward walk shows it directly: two
streams with the old UI (the visible page and the cached one), one with the fix. With
`--disable-features=BackForwardCache` the same walk never hung, which confirms the cause.

Fix (`live.rs:218`, after the fix): `pagehide` closes the stream, `pageshow` with `persisted = true`
calls `Live::resume` (re-reads system, history, tasks, notifications, pending changes, storage and
hardware, then reopens the stream). After the fix: 24 back/forward/full-load steps without a stall,
exactly one `/events` stream at any time, pages restored from the cache show live data again.

## Findings

### Critical

None open. The one Critical found in this pass is fixed:

**C1 — The Drives page showed raw i18n keys.** `pages/storage/disks.rs:55` and `:214` used the
table heads `#st.bay` and `#st.size`; the `st.*` → `sto.*` rename of the consolidation (M10 of wave A)
missed them because the `#` prefix (numeric column) hid them from the search. Visible in every
language as "st.bay", "st.size". Fixed in `6a56ebe` (`#sto.bay`, `#sto.size`), plus a new test
`i18n::tests::used_keys_exist` that scans the sources for key-shaped literals (also `#`-prefixed)
and fails on any that is not in the dictionary; a short allowlist covers the literals of the same
shape that are not keys (`rynas.*` storage keys, event names, `fuse.*`, sample host names).

### Major

**M1 — Sideways page scroll at tablet widths (fixed, `c2d3fc8`).** Two causes, found by the
390/768/900/1024/1280 px walk:
- `style/main.css:197-265` (app bar) had no rule between 768 and 1023 px: in Italian the host chip
  wrapped to two lines and the user menu was pushed past the right edge (`scrollWidth` 826 px in
  a 785 px viewport) on every page. Fix: `.conn-text, .user-name { display: none }` inside the
  `max-width: 1023px` block, host chip `nowrap` with an ellipsis (`max-width: 200px`).
- `style/main.css:765` `.table-wrap` scrolled the table but was not its containing block, so the
  absolutely positioned `.sr-only` "Actions" header of a wide table (Network interfaces, 853 px
  wide in a 751 px card) stuck out and scrolled the whole page. Fix: `position: relative` on
  `.table-wrap` (helps every table with an actions column).
After the fix: no horizontal overflow on any of the 18 routes in en/it, light/dark, 390, 768, 900,
1024 and 1280 px.

**M2 — Chart animations over budget (fixed, `a2cdb99`).** `chart.rs:233` animated each new sample
for 450 ms, i.e. charts were in motion almost half of the time (one sample per second), with one
canvas redraw per frame per chart; `style/main.css:597` `.thread-fill` transitioned 450 ms. S2 says
≤ 200 ms. Fix: `var(--t-med)` (180 ms) for the thread bars. The chart part is superseded by the
continuous time-based scrolling merged in parallel (`1688440`, S2 updated).

**M3 — No cap on event streams per session (M1 of the guidelines).** `rynas-web/src/events.rs:104`
opens, for each `GET /api/v1/events`, one TCP stream and two upstream connections to rynasd
(`follow`, `:193`), with no limit per session or in total. A buggy client, a page left in a cache
(see the hang above) or a script with a stolen cookie can hold hundreds of rynasd connections and
hyper tasks. Fix: a small per-session counter (an `Arc<AtomicUsize>` in the session entry, or a
`HashMap<SessionId, usize>` capped like the session table) incremented in `events`/`metrics_stream`
and decremented by a guard dropped with the stream; above 8 per session answer `429 rate_limited`
with `Retry-After: 5`. A global cap (e.g. 256 streams) as a second line. Owner: backend.
**Fixed** (6 per session, the oldest closed; 128 in total, then 429): see
[`backend-2026-11-05.md`](backend-2026-11-05.md).

**M4 — Wasm at 90 % of the budget (carried from wave A, M14).** 809.6 KiB of 900 KiB brotli; the
consolidation removed duplicates but no feature weight. The remaining phase-1 pages will cross the
budget. Fix: plan the S4 lazy split now (network diagnostics, notification rules, scheduler editor,
Synology fan curves are the largest page modules after storage), and keep the size line in every UI
PR. Owner: ui-shell.

**M5 — Backend items of wave A still open (A6 table, A13, A14, A16).** Unchanged since the last
review:
- m17: `POST` of users, groups and shares answers 201 without `Location` (`rynasd/src/services.rs:300`,
  `:559`, `:913`); also the scheduler (`rynasd/src/scheduler.rs:290`) and snapshot schedules
  (`rynasd/src/snapshots.rs:340`). Notifications do it right (`rynasd/src/notifications.rs:92-94`):
  copy that.
- m19 / A13: framework rejections pass serde text to the client as `message`
  (`rynas-web/src/error.rs:88-95`), e.g. `GET /api/v1/tasks?limit=abc` →
  `"Failed to deserialize query string: limit: invalid digit found in string"`. Fix: generic
  message per code ("invalid query string"), the original text to `tracing::debug!`.
- m20 / A14, A16: `UserStatus.badges: Vec<String>` with a kebab-case value `smb-password-reset`
  (`rynas-api-types/src/services.rs:144-147`); scrub state compared as a string in the UI
  (`pages/storage/mod.rs:507`, `pages/storage/volume.rs:380`: `status == "running"`). Fix: enums with
  `rename_all = "snake_case"`.
- m15: client validators still copy rynasd's rules by hand.
- m18: guideline table vs. staging deletes (200 + `StagedChanges`) not reconciled.
Owner: backend.
**Fixed after this review**: m17, m19, m20 and m15 (server side; the UI still has to switch to
`rynas_api_types::validate`), plus M11 of wave A (ETag/304 and the `config.pending` event); m18
is still open. See [`backend-2026-11-05.md`](backend-2026-11-05.md).

### minor

| # | File:line | Problem | Fix | Owner |
|---|---|---|---|---|
| m1 | `pages/hardware/synology.rs:526` (`curve_svg`, `:142-190`) | the fan-curve chart is SVG markup built with `format!` and inserted with `inner_html` (G1 allows only the icon SVGs); the content is numeric and safe, but it is the only exception | draw on `<canvas>` like the other charts, or build the SVG with `view!` elements; if kept, list it as an exception in G1 | ui-hardware |
| m2 | `pages/hardware/synology.rs:470`, `:483` | `format!("{d}%")` and `format!("{r} rpm")` (G9: percentages through `i18n::pct`; the unit is a visible literal, G7) | `i18n::pct(d)`, an `hw.rpm` key with `{n}` | ui-hardware |
| m3 | `chart.rs:208-226` | every sample allocates a `Vec<Vec<f32>>` copy of the visible history and clones the `Vec<Series>` (S2/S3: reuse the buffers) | keep the series buffers in a `StoredValue`, refill in place; borrow the series instead of `get_value()` | ui-shell |
| m4 | `rynas-web/src/api.rs:107`, `rynas-web/src/proxy.rs:44` | `/api/v1/metrics/stream` and `/api/v1/tasks/stream` are still public but no client uses them since `/events` (A22); each is one more way to hold a connection | remove them (with the plan table, `docs/phase1/plan.md:99`, `:193`) or mark them deprecated for scripts | backend |
| m5 | `style/main.css:1171-1174` | between 768 and 1279 px the System widget (`w-4` → span 6) sits alone on a half row because the CPU widget (`w-8` → span 12) does not fit next to it: an empty half row on the default dashboard | `grid-auto-flow: row dense` on the dashboard grid, or default sizes that pair up at span 6 | ui-shell |
| m6 | `pages/users.rs:207` | an empty e-mail shows an empty cell (labelled "Email" in the 390 px card layout) instead of the "not set" dash used elsewhere | `not_set` text (muted) when `None`/empty | ui-users |
| m7 | `rynasd` idle RSS | 10.4 MB idle but 14.3 MB after a burst of streams (file-backed pages of the binary, anon only 3.7 MB); the guideline idle target is < 12 MB, the 20 MB ceiling holds | none needed now; re-measure on the DS920+, where the page cache is under more pressure | backend |
| m8 | `crates/rynas-ui/dist` in development | without the packaging step the UI is served gzip-4 on the fly (3 MB wasm → ~1 MB per cold load); only `build-deb.sh` writes the `.br`/`.gz` siblings | let a `trunk` post-build hook (or `cargo xtask ui`) write them, so dev and package measure the same | ui-shell |

## Checked and fine

- **Event streams**: one multiplexed `/events` per tab; server-side teardown within a second of a
  client hang-up and at the next sample after logout; upstream rynasd connections freed with the
  stream; `resync` handled for every stream; keep-alive 15 s; no `forget()` in code that runs more
  than once (`prefs.rs:132` runs once at start).
- **Polling**: only `/storage` (30 s, paused on storage pages), `/config/pending` (30 s, compact),
  the storage pages' drives/arrays/tree (30 s) and filesystems (5 min) while one is mounted,
  scheduler (15 s while the page is open), Synology status and fan curves (10 s while that tab is
  active); every poller skips its work while `document.hidden`; no `/hardware` polling.
  Background requests carry `X-Rynas-Background`.
- **API**: every error answer seen is JSON with a registered code (405, 404 unknown path, 400 query);
  history lists clamp `?limit=` to 500 (`rynasd/src/ops.rs:259-260`); CSRF on every write.
- **GUI**: all 18 routes in en/it × light/dark × 390/768/900/1024/1280 px: no console errors, no
  error boxes, no skeleton left after 2.5 s, no raw keys (the two literal hits are a host-name
  placeholder and the configuration-key tooltip of the revision chips, both intended).

## Fixed in this pass

| # | Commit | Fix |
|---|---|---|
| hang | `9c3b26d` | `EventSource` closed on `pagehide`, everything re-read and the stream reopened on a bfcache restore (`live.rs`) |
| C1 | `6a56ebe` | `#st.bay`/`#st.size` → `#sto.bay`/`#sto.size`; test `used_keys_exist` |
| M1 | `c2d3fc8` | app bar at 768–1023 px; `.table-wrap` as containing block of `.sr-only` |
| M2 | `a2cdb99` | chart animation 200 ms, thread bars `--t-med` |

Verified before the final commit: `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo clippy -p rynas-ui --target wasm32-unknown-unknown -- -D warnings`, `cargo test --workspace`
(rynas-ui: 50 tests), `trunk build --release`, and the browser walks above.
