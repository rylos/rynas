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
