# Review 2026-11-07 — wave R, GUI: Docker page, dashboard widget, Apps & Plugins

Scope: `git diff 0406ae4..main -- crates/rynas-ui tools/ui-audit` (32 files, +5 514 lines):
`pages/docker/*`, `pages/apps/*`, `schema_form.rs`, `markdown.rs`, `docker.rs`, the dashboard
widget, `form::code_field`, the shell/live/tasks hooks. Instance: `FAKE_DOCKER=1
tools/ui-audit/start.sh`, admin login `AUDIT_USER=$(id -un)`, build of `main` at `fba32cb`.
Read-only review; no code changed.

## Measurements

| What | Value | Budget |
|---|---|---|
| wasm brotli -q 11 | **1 208 764 B = 1 180.4 KiB** (19.6 KiB left) | 1 200 KiB |
| JS glue brotli | 8 430 B | 12 KiB |
| CSS brotli | 16 008 B | 60 KiB with fonts |
| wave F (2026-11-06) | ~998 KiB | — |

Heaviest new contributors (twiggy on the pre-`wasm-opt` module
`target/wasm32-unknown-unknown/release/rynas-ui.wasm`, raw bytes; the brotli share is about
0.19× of these, from 6.2 MB of code → 1.18 MB brotli):

| Item | raw | ≈ brotli |
|---|---|---|
| `pages::apps` (catalog, wizard, installed) | 210 KiB | ~40 KiB |
| `pages::docker` (overview, containers, stacks, images) | 170 KiB | ~32 KiB |
| serde `Deserialize` for `rynas_api_types::apps` | 127 KiB | ~24 KiB |
| serde for `docker` | 64 KiB | ~12 KiB |
| `schema_form` | 34 KiB | ~6 KiB |
| serde for `web_access` | 32 KiB | ~6 KiB |
| i18n: ~540 new key lines (en + it) | — | ~8 KiB |
| **Total of the wave** | | **~125-130 KiB** |

Largest single functions: `apps::catalog::detail_body` 18.9 KB, `apps::wizard::wizard::{closure#5}`
14.9 KB, `docker::Docker::refresh::{closure}` 14.7 KB (an inlined `DockerStatus` deserializer),
`apps::wizard::W::replan` 14.2 KB, `serde_json::from_str::<AppDetail>` 13.1 KB,
`docker::stacks::detail_drawer` 12.5 KB.

The whole Italian column of the dictionary (2 937 keys, 98.8 kB) is 30 002 B brotli on its own.

Browser checks (screenshots in `target/ui-audit/shots/r-*.png`):

| Check | Result |
|---|---|
| G7 overflow scan, `/docker` overview + containers at 1280/1024/390 (en) | none (`r-docker-ov-1280`, `r-docker-ct-{1280,1024,390}`) |
| G7, `/apps` featured, detail drawer, wizard step 1 at 390/1280 | none (`r-apps-390`, `r-apps-detail-390`, `r-apps-wiz-{390,1280}`) |
| `idle_dom.py 15` on `/`, `/docker`, `/docker:1`, `/docker:2`, `/apps` | ok: 0 elements removed, 0 added |
| Containers table DOM | 38.9 elements per row → ~1 950 for 50 containers (fine) |
| `scroll_audit.py` with `ROUTES=` docker/apps tabs | **hung twice** (no `TOTAL` after 15 and 10 min, nothing printed): not completed; see L6 |

Not covered within the time box: Italian at every width, the stacks editor and logs drawer
screenshots, the updates/rollback dialogs, `states.py`, `dash_shift.py` for the new widget.

## Findings

### High

**H1 — the containers table shows "—" for CPU and memory of almost every running container.**
`pages/docker/mod.rs:205` `container_stat` looks the id up in `DockerStats.top`, which is the
**top 5** only (`rynas-api-types/src/docker.rs:662`). In the fake set, 5 running containers, 1 has
numbers (`r-docker-ct-1280.png`: portainer, syncthing, uptime-kuma, homeassistant read "—"). With 50
containers, 45 columns of dashes look like broken data.
Fix: give `ContainerSummary` (or a `docker.stats` field `all: Vec<ContainerStat>`, ids + 2 numbers)
the per-container sample the sampler already computes, and keep `top` for the widget. Otherwise drop
the two columns and link the widget's top list.

**H2 — wasm at 1 180.4 KiB of 1 200 KiB: the next wave (W1 UI: Routes page, Access step) will
exceed the budget.** See "Wasm recommendation" below.

### Medium

**M1 — whole-table rebuild on every list change.** `containers.rs:40-55` and `stacks.rs:76-100` rebuild
every row (`ui::table` from a `Vec<AnyView>`) when `list.data`, `updates.data` or the search text
changes. `docker.changed` arrives up to 1/s during a compose up (§5.1), so rows are rebuilt while the user
works: hover, open `title` tooltip and per-row `busy` (`containers.rs:65`, `stacks.rs:110`) are lost;
a button disabled by `busy` re-enables while its task still runs. (The fake engine sends no
`docker.changed`, so `idle_dom` cannot see it; found in code.)
Fix: a keyed `<For>` over ids with a per-row `Memo` of the summary (as in `top_row`,
`mod.rs:262`), filtering by toggling `hidden` instead of rebuilding; keep `busy` in a
`RwSignal<HashSet<String>>` at panel level.

**M2 — focus lost after a row action (G10).** Clicking Restart sets `disabled` on the focused button;
Chrome blurs it and focus goes to `<body>` (probe: `focus on: BODY` after Restart). Keyboard users must
tab from the top. Fix: `aria-disabled="true"` + ignore clicks while busy (keep it focusable), or move
focus to the task panel entry.

**M3 — duplicated fetches.** `/api/v1/docker/updates` is a separate `Remote` in Containers
(`containers.rs:37`) and Stacks (`stacks.rs:68`); both mounted tabs stay alive and both reload on every
`docker.changed`, as do images, volumes and `df`: up to 7 GETs per event per visited tab set.
`/docker/updates` only changes after `docker.update-check`. Fix: one page-level `Remote<UpdateReport>`
(context), reloaded only when a `docker.update-check` task ends; reload images/volumes/df on
`docker.changed` only while their tab is visible (or mark them stale and reload on tab show).

**M4 — dashboard widget stuck on its skeleton when the engine is stopped.** `mod.rs:230` shows
`Skeleton` until a `docker.stats` sample arrives; `has_docker` (`dashboard.rs`) checks `installed`
only. Docker installed but not running → a skeleton forever (G2: no error/empty state). Fix: when
`!docker().running()` render an `EmptyState`/`Status` "Engine stopped" with a link to `/docker`.

**M5 — log drawer copies the whole buffer on every line.** `containers.rs:166-180` appends to a
`String` of up to 256 KiB and `{move || text.get()}` (`:248`) clones it and replaces the `<pre>` text node
for each `log` event: a chatty container (100 lines/s) moves ~25 MB/s through wasm and the DOM.
Fix: batch lines per `requestAnimationFrame` and append text nodes (`pre.append_with_str_1`), trimming
the first child when over the limit.

**M6 — image references break at arbitrary characters.** `r-docker-ct-1280.png`:
`jellyfin/jellyfin:10.11.` / `2`, `ghcr.io/home-` / `assistant/home-` / `assistant:2026.11.1`. Fix:
render `repo` and `:tag` as two spans (tag in `.cell-sub` on its own line), or insert `<wbr>` after `/`
and before `:` and use `overflow-wrap: break-word` instead of `anywhere`.

### Low

- **L1** `apps.upd.snapshot_help` (i18n.rs:3170): "Creata … la riporta" treats *snapshot* as feminine;
  everywhere else it is masculine ("Uno snapshot", "Lo snapshot"). → "Creato … lo riporta".
- **L2** `sf.when_on` (i18n.rs:3190) "On adds:" / "Acceso aggiunge:" reads oddly; the catalogue
  description after it stays English in Italian (`r-apps-wiz-1280.png`). → "When on:" / "Se attivo:"
  and `lang="en"` on the catalogue text.
- **L3** `dk.set.manager_help` (i18n.rs:2770) says "the Apps page"; the page is "Apps & Plugins" /
  "App e plugin". Use the nav name.
- **L4** `/plugins` (shell.rs) opens the Apps page on Featured; an old link to Plugins should open
  tab 2 (`a.tab` initial value from the route).
- **L5** `apps/mod.rs:94` drops the `/web-access` error silently. Documented as intended (503 =
  inactive), but a 401/500 also reads "inactive". Map only 503 to inactive.
- **L6** `tools/ui-audit/scroll_audit.py` prints nothing until the end and hung on the docker/apps
  routes (probably a dialog opened by a primary button that never closes: the stack editor or the
  install wizard). Add `flush=True` progress per route and a per-step timeout.
- **L7** Catalogue screenshots/icons load with `loading="lazy"`; screenshots are external URLs
  (`catalog.rs:358`): the browser contacts third-party hosts when the drawer opens. Serve them through
  the API like the icon, or show them only on click.

Checked and fine: skeleton → content | `ErrorBox` through `Remote::view` everywhere; empty states on
every list; destructive actions use `ui::confirm` with typed name for data deletion (stack remove
`stacks.rs:365`, uninstall with data `installed.rs:518`); no `inner_html` (markdown builds nodes);
`.code-area` scroll listed in `style_check`; no product comparisons or Synology wording in the ~540
new keys (Portainer/Dockhand appear only as names); wizard values live in `W` signals, so Back/Next
keep them; secrets panel guarded by `ask_close`; errors through `e.text()` / `err.<code>`; no polling:
lists follow `docker.changed`, stats follow `docker.stats`, the log `EventSource` is closed on drawer
close (`on_cleanup`, `Drop`).

## Wasm recommendation

1. **Lazy loading is not available with this toolchain today.** Leptos 0.8.5+ (we have 0.8.23)
   has `#[lazy]` and `#[lazy_route]`, backed by `wasm_split_helpers`, but the split itself is a
   post-link step done by **cargo-leptos** (`--split`); Trunk 0.22 has no wasm-split step and no
   issue/plan for one was found. `/usr/bin/wasm-split` on this host is Binaryen's tool, not the
   jbms/Leptos `wasm-split-cli`. Switching the UI build from Trunk to cargo-leptos (CSR) just for
   this is a bigger change than the gain warrants now; Docker + Apps would be the natural split
   (~125 KiB brotli, only admins with Docker use them). Revisit with Leptos 0.9 (lazy behind a
   `lazy` feature) or if Trunk gains the step. Sources:
   [Leptos releases](https://github.com/leptos-rs/leptos/releases),
   [wasm_split_helpers README](https://docs.rs/crate/wasm_split_helpers/0.2.0-rc.1/source/README.md),
   [wasm-bindgen #3939](https://github.com/rustwasm/wasm-bindgen/issues/3939),
   [Leptos 0.8.5 release notes (Renovate PR)](https://github.com/RustForWeb/shield/pull/143).
2. **Cuts of ≥ 60 KiB without a new toolchain:**
   - **Italian dictionary as a fetched asset (−29 KiB):** keep `en` in the blob, publish the `it`
   column as a hashed static file, fetch it at sign-in when the language is Italian (or on switch).
   Measured: the Italian column alone is 30 002 B brotli. Tests stay as they are (they read the
   source).
   - **Smaller wire types for the catalogue (−15-20 KiB):** serde for `apps` is the largest serde
   block (127 KiB raw, ~24 KiB brotli) and `from_str::<AppDetail>` alone is 13 KB. Have the list
   endpoint return a lean `AppCard` and deserialize the rest of `AppDetail` (`form`, `folders`,
   `overlays`) as `serde_json::Value` read by `SchemaForm`, which already walks JSON.
   - **One `DockerStatus` deserializer (−3 KiB):** `Docker::refresh::{closure}` inlines a 14.7 KB
   deserializer; mark the call `#[inline(never)]` through a shared `api::get_json` helper (pattern of
   the non-generic `ui::table`).
   - **Shared row builder for Docker/Apps tables (−8-10 KiB):** `containers::row`,
   `stacks::row`, `images::*`, `installed::*` each build their own cell closures; a non-generic
   `ui::text_cell(key, class, String)` for plain-text cells removes most of them (the M1 keyed rows
   change these anyway).
   - **Lighter `detail_body` and `wizard` closures (−5-8 KiB):** split the 19 KB / 15 KB monolithic
   view closures into `fn` helpers returning `AnyView` (erased types already on; the gain is from
   not monomorphizing nested `view!` tuples).
   Together ≈ 60-70 KiB, back near 1 110 KiB. If none of this is wanted before W1, raise the budget
   explicitly in `guidelines.md` §3.1 with the reason, as on 2026-11-05.

## Resolution (2026-11-07, UI fixes)

Branch `worktree-agent-aded93643058c000d`, main merged at `e33cd36` (backend fixes, `DockerStats.usage`).

### Wasm

| Build | wasm brotli -q 11 | Left of 1 200 KiB |
|---|---|---|
| Before (`fba32cb`, this review) | 1 208 764 B = 1 180.4 KiB | 19.6 KiB |
| After (this branch, main merged) | 1 171 686 B = 1 144.2 KiB | **55.8 KiB** |

−37.1 kB with the new code of this round included (keyed rows, logs buffer, dashboard state).
The 60 KiB headroom target is missed by 4 KiB: the rest of the review's list would need the
lean `AppCard` from the server (an API change) or a change of `RamGate` (internally tagged: the
most expensive serde shape), both outside the UI.

What was cut:
- **Italian dictionary as a fetched file** (−31.9 kB): only English is compiled in;
  `assets/i18n/it.txt` (`key<TAB>text`, 44 KiB brotli, served as `/i18n/it.txt`) is written by
  the test `italian_file_is_current`. `index.html` starts the download while the wasm loads when
  the language is Italian; the boot screen waits for it (at most 1.2 s, then English, switching
  when it arrives); a switch to Italian later fetches it on demand. Tests keep both columns in
  memory: every key still needs both texts (the macro), and the file is checked against them.
- **Install form as JSON** (−3.6 kB): `schema_form::Form` reads the `AppForm` wire shape from a
  `serde_json::Value` (a UI-local `AppDetail` with the same fields), a test checks it reads the
  same as the typed form serialized again.
- `/web-access` read through a two-field struct instead of the whole `WebAccessState`.
- One `DockerStatus` reader (`#[inline(never)]`, 9.4 KB instead of 14.7 KB inlined).
- Shared row helpers (`ui::text_cell`, `two_lines`, `breakable`, `keyed`, `row_hidden`,
  `table_body`; `row_btn`, `update_chip`, `image_ref`, `empty` in the Docker page); one non-generic
  keyed list (`ui::keyed`) also for the USB drives, volumes and UPS flags.
- `detail_body` and the wizard's body/foot split into functions.

### Findings

| # | State | What |
|---|---|---|
| H1 | fixed | The table reads `DockerStats.usage` (every running container, main `e33cd36`), `top` for older servers (`docker::usage_of`, unit test). With the fake engine only containers that borrow a host cgroup have numbers (this host: one), the rest show "—" as before: a property of the fake set |
| H2 | partly | 1 144.2 KiB, 55.8 KiB left (target 60): see above |
| M1 | fixed | Containers and stacks tables are keyed (`ui::keyed`), every cell follows a per-row `Memo`, the filter hides rows (`hidden`) instead of rebuilding; running actions are a panel-level `HashSet` (a rebuilt row stays busy). The stack drawer builds its action bar once per stack kind, so a refresh keeps its buttons |
| M2 | fixed | Row actions and the drawer's buttons use `aria-disabled` + ignored clicks (CSS `.icon-btn[aria-disabled]`), focus stays on the button |
| M3 | fixed | One update report per page (context), read again only when a `docker.*` task ends (`Docker::ended`); Docker settings once per page; lists follow `docker.changed` debounced (300 ms, `docker::follow`) and only while their tab is visible (a hidden tab reads once when shown). Apps: the catalogue follows task ends only, the installed list `docker.changed` (debounced) |
| M4 | fixed | Installed but not answering: "Docker engine stopped" empty state with the link (checked by stopping the fake engine); the dashboard counts it as loaded |
| M5 | fixed | Log lines are buffered and appended once per frame as one text node; the oldest nodes go over 256 KiB or 5 000 lines; Download reads the `<pre>` |
| M6 | fixed | `ui::breakable`: `<wbr>` after `/` and before `:`/`@` (`jellyfin/` `jellyfin:10.11.2`), still `anywhere` for a part wider than the cell |
| L1 | fixed | "Creato … lo riporta" |
| L2 | fixed | "When on:" / "Se attivo:", "When off:" / "Se disattivo:"; the overlay text stays `lang="en"` (catalogue texts are English) |
| L3 | fixed | "the Apps & Plugins page" / "la pagina App e plugin" |
| L4 | fixed | `/plugins` opens the Plugins section |
| L5 | fixed | Only 503 (and 401) mean "inactive"; another failure shows a warning note |
| L6 | fixed | `scroll_audit.py` prints one line per route and width (flush), has `ROUTE_TIMEOUT` per route, `LANGS`/`WIDTHS`, opens only the first visible row of each table and skips task buttons (`data-act`); it did not hang, it clicked every row action of every row (tasks, ~15 min per route). Exit 1 on findings or timeouts |
| L7 | fixed | Only `/api/...` screenshot URLs are shown (the API serves the catalogue's files); CSP `img-src 'self'` already blocked others |

Also found and fixed while checking: the stack drawer's Update / Remove / Rollback / delete
snapshot dialogs never opened when the drawer was built before its details arrived (the
dialogs closure returned before subscribing to `ask`).

### Checks of what the review could not cover

Instance `FAKE_DOCKER=1 tools/ui-audit/start.sh 18620`, `AUDIT_USER=$(id -un)`; screenshots
`target/ui-audit/shots/x-*.png`.
- Italian at 1280/768/390: containers, stacks, stack drawer, update and remove dialogs, stack
  editor, logs drawer, Apps, install wizard, dashboard: no element scrolls sideways, no raw key.
  The rollback dialog needs a stack folder that is a btrfs subvolume (the fake set has none).
- `dash_shift.py 4` with the Docker widget: shift score 0 (login, reload, navigation).
- `scroll_audit.py` on `/docker`, `/docker:1-3`, `/apps`, `/apps:1-4`, it+en, 1280/1024/390:
  0 findings, 162 dialog views, no timeout, about 10 minutes (it printed every route as it went).
- `states.py` (`ROUTES=/docker,/apps`, new `PARTS`/`ERR_BASE`/`ROUTES` options): focus visible on all 116 stops (light, dark); skeletons under 3 s latency (it, en, 390, 1280); with rynasd stopped both pages show the error state with Retry, no endless skeleton.
