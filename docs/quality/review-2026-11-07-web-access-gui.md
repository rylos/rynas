# Review 2026-11-07 — web access W1/W2, GUI

Scope: `git diff 1626853..main -- crates/rynas-ui tools/ui-audit` (20 files, +1 549 lines):
`pages/web_access/{mod,cert}.rs`, the apps wizard Access step, "Move behind the proxy", the
Docker `install_incomplete` card, Featured / `reverse_proxy` on the Apps page. Instance:
`FAKE_DOCKER=1 FAKE_CADDY=1 tools/ui-audit/start.sh`, admin login `AUDIT_USER=$(id -un)`, build
of `main` at `648ae0a`. Read-only review; no code changed. A route was created by installing
Stirling-PDF through the wizard (dry system).

## Measurements

| What | Value | Budget |
|---|---|---|
| wasm brotli -q 11 | **1 206 734 B = 1 178.5 KiB** (21.5 KiB left) | 1 200 KiB |

Contributors of this wave (twiggy, raw bytes of the pre-`wasm-opt` module; brotli ≈ 0.19×):

| Item | raw | ≈ brotli |
|---|---|---|
| `pages::web_access` (overview, routes, drawer, banner) | 99 KiB | ~18 KiB |
| of which `pages::web_access::cert` | 24 KiB | ~4.5 KiB |
| `WaState` deserializer + `Remote<WaState>::reload` | 10 KiB | ~2 KiB |
| `TlsStatus` deserializer | 6 KiB | ~1 KiB |
| `apps::wizard::access` | 5.5 KiB | ~1 KiB |
| i18n, +178 English lines (Italian is the separate `it.txt`) | — | ~3 KiB |

Largest functions: `Remote<WaState>::reload` closure 7.8 KB, `cert::panel` closure 6.8 KB,
`route_drawer` body 6.4 KB, `Vec<RouteView>` deserializer 4.8 KB.

Browser checks (screenshots in `target/ui-audit/shots/`):

| Check | Result |
|---|---|
| G7 overview 1280/1024/390 (`wa-ov-*`) | no page or inner horizontal scroll |
| G7 App addresses 1280/1024/390 (`wa-routes-*`) | none |
| G7 route drawer with test results 1280/1024/390 (`wa-drawer-test-1280`, `wa-drawer-{1024,390}`) | none |
| G7 certificate, steps open, 1280/390 (`wa-cert-*`) | none |
| G7 import dialog 1280/1024/390 (`wa-import-*`) | none (but see M4) |
| Wizard Access step 1280/390 (`wa-wiz-access-*`) | none (but see M4) |
| Italian, 390 and 1024 (`it-wa-*`) | none; no English left |
| Timed confirmation: UI access → `lan_vpn`, Save, wait 130 s (`wa-change-dlg`, `wa-banner`, `wa-after-rollback`) | banner and rollback work; see H1, M1 |

Not covered within the time box: `idle_dom.py` on the new page (the code review below found no
polling), `FAKE_CADDY=refuse`, the external mode switch, the Docker `install_incomplete` card
in a browser.

## Findings

### High

**H1 — after a timed rollback the Settings form still shows the unconfirmed value.**
`web_access/mod.rs:419-421`: `settings_form` reads the state with `get_untracked` and is
mounted by `w.state.gate(6)`, which follows readiness only, so a reload never rebuilds it.
Seen: UI access set to "This network and VPN", not confirmed; after 120 s the summary card says
"This network" (applied value), but the radios still show "This network and VPN" with Save
enabled (`wa-after-rollback.png`). The page contradicts itself, and the admin may save the same
change again without knowing why it failed.
Fix: rebuild the form when `applied`/`pending` changes (key the gate closure on a memo of the
settings value), or reset `init` and the signals in an effect on `state.data` whenever the form
is not dirty.

### Medium

**M1 — a rollback happens silently.** The banner disappears and nothing says the change was
undone. Fix: when `confirm` goes from `Some` to `None` without the local Confirm, show a
warning toast or note ("Not confirmed in time: the previous settings are back"). The revision
text from rynasd already exists.

**M2 — the Confirm button appears on the emergency address, where the confirmation is always
refused.** `mod.rs:214` compares only `location.hostname` with `names`. On
`https://nas.local:8443/` (the address the page tells the admin to bookmark) `on_new` is true,
so the page shows Confirm, but rynasd accepts it only through the proxy (`web_access/mod.rs`
`confirm`: loopback local address on the plain port), so the admin gets 409 `conflict`.
Fix: also require `location.port` empty or equal to `settings.https_port`; otherwise show the
Open link.

**M3 — the banner can stay stuck at "0 s left".** `mod.rs:233` reloads the state once, 3 s after
the deadline. If the rollback apply (a Caddy load) takes longer, `confirm` is still in the answer,
the memo does not change, and the banner stays with a disabled button until F5. Fix: keep
reloading every 3 s while `left == 0` and `confirm` is still present (a few times at most), or
reload on the `config` revision event.

**M4 — "drawers 15 px off at 390 px": real and global, but only with classic scrollbars.**
`main.css:1085-1086` (and `1553`) give `.modal`, `.modal.wide` and `.modal.drawer` `width: 100vw`
below 768 px. `100vw` includes the root scrollbar, the top layer box does not: with a page that
scrolls, the dialog is 390 px wide in a 375 px viewport (measured: import dialog `[0, 390]`
while `clientWidth` = 375). The wizard's close button is cut by the scrollbar
(`wa-wiz-access-390.png`). It hits every dialog and drawer, not only this page. It is invisible
on phones (overlay scrollbars) and when the page does not scroll (route drawer at 390:
`390/390`). Fix: `width: 100%` (or `inset-inline: 0; width: auto`) instead of `100vw`, or
`scrollbar-gutter: stable` plus `overflow: hidden` on `html` while a modal is open.

**M5 — the route drawer offers "This network and VPN" where rynasd refuses it.** For a route
with `auth: none` (`mod.rs:690`) the note says the app stays on this network, but
`access_radios` (`mod.rs:378`) still enables `lan_vpn`; saving returns 400 `invalid_value`
(`model.rs:487`), shown as a generic form error. Fix: pass a `lan_only` flag and disable
`lan_vpn` like `public`, with the note as its help.

**M6 — the Chrome-on-Linux step is out of date.** `i18n.rs:3442`: "Settings, Privacy and
security, Security, Manage certificates, Authorities, Import". Since Chrome 134 this opens
`chrome://certificate-manager`: Local certificates › Custom › Installed by you › Trusted
certificates › Import (Chromium Root Store FAQ,
https://chromium.googlesource.com/chromium/src/+/main/net/data/ssl/chrome_root_store/faq.md;
an Ubuntu guide, https://docs.motorcortex.io/docs/getting-started/connect-your-pc/install-a-browser-certificate/ubuntu/).
Windows, macOS, iOS (VPN and Device Management, then About › Certificate Trust Settings),
Android, Debian/Fedora/Arch and Firefox match current docs. Fix: rewrite the Chrome line (en+it).

### Low

- **L1 — Italian "Accesso" means two things.** `wa.route.login` = "Accesso" (login) on a page
  called "Accesso web", next to "Chi può collegarsi". Fix: "Login" → "Accesso dell'app" /
  "Autenticazione" (`i18n.rs:3372`, also `wa.auth.app`).
- **L2 — "route" leaks into user text.** `wa.prob.local_names_off` (`i18n.rs:3386`): "this
  route has no address" / "questo percorso". Use "this app has no address" / "quest'app".
- **L3 — clumsy wizard sentence.** `apps.w.access_level` (`i18n.rs:3300`). Suggest "Who may
  connect (this network by default) can be changed later in Web access" / "Chi può collegarsi
  (di default questa rete) si cambia in seguito in Accesso web", with the link on the page name.
- **L4 — fixed 8443.** `tls.https_port` (`i18n.rs:3416`, `cert.rs:90`) hard-codes the port while
  the banner and the overview use `emergency_port`. Use `{port}`.
- **L5 — the route name is the entry id.** `route_name` (`mod.rs:502`) shows `stirling-pdf`,
  the drawer title "Address of stirling-pdf"; elsewhere the app is "Stirling-PDF". Send or look up
  the catalogue name.
- **L6 — the wizard stepper grows from 4 to 5 steps when the detail arrives**
  (`wizard.rs:97-98`, `steps()` false while `detail` loads). Count Access while loading too, or
  hold the stepper until the detail is in.
- **L7 — the Access step without web access is a page with only a note** (`wizard.rs`
  `access`, `!proxy_active`). Skip the step and show the note on Network instead.
- **L8 — `fe80::/10` breaks inside the address** (`wa-ov-1280.png`, "fe80 / ::/10"). `breakable`
  on the joined string; render each range as its own nowrap item.
- **L9 — the banner offers only `names[0]`** (the `.local` name). A client without mDNS cannot
  open it; offer the second name too.
- **L10 — the 1 s interval runs for the whole life of the page** (`mod.rs:166`). It only sets a
  signal while a confirmation waits, so the cost is low; it could be started from the banner.
- **Note (scope):** W2 (domains, certificate dashboard, `d04db82`) has no UI yet; the Certificate
  tab still shows only the W0 certificate.

## Consistency, idle

Components are the shared ones (PageHeader with back button, `WidgetHead`, `ui::table` with
card rows below 768 px, `ui::drawer` with Close in the foot, `ui::confirm`, `EmptyState`,
`ui::note`, `ui::status`), A19b saves (PUT, then apply `web_access` only), tabs mounted on first
visit, `?tab=certificate` deep link. No network polling: the state is read once, again after
an action, and on a config generation change. The routes table and the overview cards are
rebuilt on each of those reloads (not keyed); with a handful of routes this is fine.

Lockout from the UI: no path found. Ports are not editable, UI changes wait for a confirmation
through the proxy (an admin on a VPN who selects "This network" cannot confirm, so it rolls
back), `public` is disabled, external mode is explained with the emergency address first, and
`:8443` is never affected. The risks are the confusing states above (H1, M1-M3), not a lockout.

## Fix status (2026-11-07, UI fix wave)

| Finding | Status | How |
|---|---|---|
| H1 | fixed | the Settings form is keyed on a memo of the server's settings (`settings_of(current)`): a save, a timed rollback or another administrator's change builds it again; changes to other fields (routes, engine) do not |
| M1 | fixed | a confirmation that disappears while `applied.settings` changes back (and without this page's Confirm) is a rollback: warning toast "Not confirmed in time: the previous settings are back" and a "Change undone" note in the banner's place until closed or the next save; a confirmation done from the other address (settings stay) says nothing |
| M2 | fixed | Confirm only when the hostname is one of `confirm.names` *and* the port is the web access HTTPS port (empty = 443); on `:8443` the page shows the Open links, which carry `:<https_port>` when it is not 443 |
| M3 | fixed | after the deadline the state is read again with pauses of 2, 3, 5, 8, 13, 20 s while the same `confirm` is still present; the text says "Time is up: the previous settings are being restored…" instead of "0 s left" |
| M4 | fixed | `main.css`: `.modal`, `.modal.wide`, `.modal.drawer`, `.dialog.drawer` and the command palette use `100%` instead of `100vw` (desktop `calc(100% - …)` too); toasts, popovers and the offline card are not in the top layer / are shrink-to-fit and were left as they were |
| M5 | fixed | `access_radios(…, lan_only)`: for `auth: none` the `lan_vpn` choice is disabled and its help is the backend rule ("no login of its own, so it stays limited to this network"); the separate note is gone |
| M6 | fixed | Chrome on Linux 134+: `chrome://certificate-manager`, Local certificates, Custom, Installed by you, Import next to Trusted certificates (en + it); source added to `docker-apps.md` |
| L1 | fixed | Italian: "Accesso web" stays the feature; authentication says "login" (`wa.route.login` = "Login", `wa.auth.app` = "Il login proprio dell'app", `wa.route.auth_none`) |
| L2 | fixed | `wa.prob.local_names_off` says "this app" / "quest'app"; `apps.proxy_text` no longer says "route" / "percorso" |
| L3 | fixed | "Who may connect (this network by default) can be changed later in Web access." with the link on the page name (it: "… si cambia in seguito in Accesso web.") |
| L4 | fixed | the certificate chip uses `emergency_port` of the web access state (`tls.https_port` takes `{port}`) |
| L5 | fixed | the routes tab reads `/api/v1/apps/installed` once (stack and name only) and labels routes and the drawer title with the catalogue name; without Docker the stack name stays |
| L6 | fixed | the wizard counts its steps only when the entry *and* the web access state (`Apps::proxy_known`) are in, so the stepper is fixed from the first frame |
| L7 | fixed | no Access step without web access; the "web access is not active, the app opens on {url}" note moves to the Network step (the Access step keeps it only as a fallback if the plan disagrees) |
| L8 | fixed | address ranges render as one item each (`.wa-ranges`), breaking inside a range only when it alone is wider than the line |
| L9 | fixed | the banner offers an Open link for every name, plus a hint to use the plain name where mDNS is missing |
| L10 | fixed | the 1 s interval lives inside the banner and is cleared when the confirmation goes |
| Note (W2 UI) | not fixed | out of scope for this wave: W2 has no UI yet |

Verification (`FAKE_DOCKER=1 FAKE_CADDY=1 tools/ui-audit/start.sh 18640`, admin login):
- H1/M1/M3: UI access → "This network and VPN", Save, not confirmed; after 135 s the banner
  had become "Change undone — Not confirmed in time: the previous settings are back", the
  radios showed "This network" again and Save was disabled (`fx-banner.png`,
  `fx-after-rollback.png`). The banner offered both names (L9).
- M4: the install wizard at 390 px with a scrolling page: dialog right edge 375 = `clientWidth`
  (was 390); scroll_audit on `/network/web-access` (3 tabs), `/apps`, `/apps:1`, `/apps:3` at
  1280/1024/390, en and it: 0 findings, 102 dialog views.
- L5/L6: after installing Stirling-PDF the routes table and the drawer title say "Stirling-PDF";
  the stepper showed 5 steps from the first frame to Review.
- Screenshots `fx-{ov,routes,drawer,cert}-{en-light,it-dark}-{1280,1024,390}.png`: no
  horizontal scroll, `fe80::/10` whole, the chip says "porta di emergenza 8443" from the state.
- M2 and M5 are not reachable on this instance (no `:8443` through the browser, no catalogue
  entry with `auth = "none"`): checked by reading the code paths only.
- wasm brotli -q 11: 1 210 238 B = 1 181.9 KiB (+3.4 KiB: new texts, the rollback note, the
  installed-app names; 18.1 KiB left).
