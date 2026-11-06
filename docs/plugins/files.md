# Plugin `files` — file manager

Decision D-FM (`../phase0/05-decisions.md`): the SynoFile backend extracted into rynas, the
SynoFile interface first ported as a Web Component, then rewritten in Leptos piece by piece. This
file has two parts kept apart to ease merging: the backend (routes, worker, transfers as tasks)
and the UI below.

## UI

Status (2026-11-06): phase 1 done in the UI. The SynoFile interface (vanilla ES modules,
~12.5k lines, CodeMirror 6, libheif) runs inside rynas as the Web Component `<rynas-files>` on the
Files page; it needs the files API (`/api/v1/files/*`) from the backend part to show data.

### Where things are

| What | Where |
|---|---|
| The component (ported code) | `crates/rynas-ui/files-app/`: `rynas-files.js` (custom element, shadow root, bootstrap), `js/` (the app), `app.css`, `fonts/eza-icons.woff2`, `vendor/codemirror` (MIT), `vendor/libheif` (LGPL-3.0, unmodified), `js/syntax` (from micro, MIT) |
| Build and serving | trunk copies `files-app/` into `dist/files-app/` (`<link data-trunk rel="copy-dir">` in `index.html`); rynas-web serves it like the rest of the UI (`--ui-dir`), the .deb ships it precompressed (`packaging/build-deb.sh` compresses every `.js`/`.css`/`.wasm`) |
| Leptos page | `crates/rynas-ui/src/pages/files.rs`: routes `/files` and `/files/*path`, the lazy script load, the bridge of window events |
| Checks | `crates/rynas-ui/src/files_check.rs` (texts translated, no other product named, only it/en, no undeclared sideways scroll); `tools/ui-audit/files_mock.py` (fake files API for screenshots) |

### How it is mounted

- **Lazy**: the wasm bundle does not grow. `pages/files.rs` adds
  `<script type="module" src="/files-app/rynas-files.js">` the first time the Files page opens
  (skeleton while it loads, an error box with Retry if it fails). `rynas-files.js` preloads the
  app's modules (`modulepreload`) so they come in parallel, not one import level at a time.
- **One shadow root for the life of the page**: `<rynas-files>` hosts a single element that
  carries the shadow root. Leaving the Files page and coming back finds the same tabs, transfers,
  open editor and quick look. Styles are isolated both ways (rynas `.sidebar`, `.menu`, `.btn`
  and SynoFile's same class names never meet).
- **Deep links**: `/files/<share>/<folder>` (attribute `path`) opens that folder: the tab that
  shows it, or a new one. rynas-web answers `index.html` for `/files/…` even when the last segment
  has a dot (a folder `photos.2024`).
- **Own window**: the "open in a window of its own" button opens `/files/<current folder>?window`;
  with `?window` the shell hides its bars (`.app.bare`), so the window is the file manager only.
- **Theme**: rynas tokens. SynoFile's colour variables are mapped onto rynas's (`--muted` →
  `--text-muted`, `--crit` → `--danger`, eza's palette → `--danger/--success/--warning/--accent/
  --accent-2/--info`), and custom properties inherit through the shadow boundary, so light and
  dark follow the shell with no colour of their own (the editor's code colours and the dark quick
  look overlay keep theirs). `data-theme` on `<html>` is watched for the parts that need it.
- **Language**: only Italian and English, following `<html lang>` (set by the shell from the
  user's choice), switched live. The texts system is kept: Italian strings in the code, English in
  `js/lang/en.js`; the seven other SynoFile tables are not shipped.
- **Layout**: `.sf` (the app root) is a size container: SynoFile's breakpoints follow the room
  the shell leaves, not the window, and fixed overlays (context menu, transfers drawer, toasts,
  editor) stay inside the Files page; dialogs are in the top layer as before. The page fills the
  height under the app bar and above the dock (task panel) and, on phones, the bottom bar.
- **Keyboard**: the app's shortcuts listen on its shadow root, so they work only while the focus
  is in the file manager and never fight the shell's (Ctrl K, Esc, forms); the shell's Ctrl K
  still works from inside the file manager.

### API, session, errors

- Base `/api/v1/files/<route>` with SynoFile's route names and JSON shapes (`init`, `list`,
  `stat`, `jobs`, `jobs/new`, `upload`, `view`, `download`, `conns/*`, …).
- The rynas session cookie travels with every request; requests other than GET/HEAD carry
  `X-CSRF-Token` (the shell sets the property `csrf` on `<rynas-files>`, also again after a new
  login). Requests made while the user is away carry `X-Rynas-Background: 1` (A24b), so the
  transfers' polling does not keep an unattended session alive.
- A 401 fires `rynas-files:unauth`: the shell shows its login screen over the page (route and
  file manager state kept); after the new login every pane reloads.
- Errors: the server's stable `code` is translated (`i18n.js` `ERRORS`, it/en), falling back to
  the server's `message` (rynas) or `error` (SynoFile), then to the status. Both real status codes
  and SynoFile's `422 + X-Synofile-Status` are understood.
- `view` answers (PDF in an iframe, images, media) get from rynas-web
  `X-Frame-Options: SAMEORIGIN` and a CSP without `script-src` (`static_files::FILE_VIEW_CSP`):
  framed by rynas only, and an HTML/SVG file opened there cannot run scripts. The UI's CSP allows
  `blob:` images and media (HEIC previews decoded to JPEG in a worker).

What the UI expects from the backend beyond SynoFile's contract:

| Need | Detail |
|---|---|
| `POST eject` `{share}` | unmount and power off the removable drive of a shared folder (SynoFile asked DSM's own API); 404 shows "cannot eject" |
| share fields | `removable: true` for shares on USB/eSATA drives (else the volume names `volumeUSB1`/`volumeSATA1` are recognised), optional `device` (shares of the same drive) and `device_name` (shown in the eject dialog) |
| error `code`s | the ones in `i18n.js` `ERRORS`: `forbidden`, `permission_denied`, `not_found`, `exists`, `conflict` (editor), `invalid_name`, `read_only`, `no_space`, `quota_exceeded`, `payload_too_large`, `not_supported`, `remote_unreachable`, plus the generic rynas codes |
| upload chunks | 16 MiB per request (`uploads.js` `CHUNK`): above rynas-web's 1 MiB default body cap, the route must document its own |
| `whoami.daemon` | `false` shows "the transfer service is not running" |

### What changed from SynoFile

- DSM loader (`synofile.js`, `tray.js`, `index.html`, `api.cgi`, `texts/`, `config`, icons) replaced
  by `rynas-files.js`; no SynoToken, `login.cgi`, `SYNO.SDS.*`, iframe/opener lookups,
  `postMessage` open requests, stale-build reload or Login Portal paths.
- Session: rynas login instead of DSM's; the DSM banners (package setup, Package Center, admins
  only) became neutral ones (no access, service not installed/not answering with Retry,
  transfer service down).
- Theme toggle and theme preference removed (the shell's theme); the "DSM taskbar icon"
  preference removed; the version/author credits removed from the settings dialog.
- "Open in File Station" removed. "Description…" and "Permissions…" of a shared folder open the
  rynas share editor (`/shares?edit=<share>&tab=0|3`: General, or Permissions with the explained
  effective permission) instead of DSM dialogs; SynoFile's own permission dialog (DSM ACL API) is
  gone.
- USB eject through the files API instead of DSM's External Devices API.
- A transfer that ends while another rynas page is shown becomes a rynas toast; browser
  notifications say "rynas". Other windows of the file manager are nudged through
  `localStorage` to poll at once.
- `@eaDir` (thumbnails left by another system on migrated volumes) joins the protected, greyed
  system folders with `#recycle`, `#snapshot`, `@tmp`, `@sharebin`.
- Texts reworded without DSM terms (Recycle Bin of the shared folder, the names given to the
  volumes, …); localStorage keys `rynas.files.*`, drag types `application/x-rynas-files*`.
- Lists, dialogs and panels scroll vertically only (G7); the columns view and the text preview
  are the two listed exceptions, wide by nature.

### Plan: the Leptos rewrite, piece by piece

The component stays the file manager until each piece is replaced; the order goes from what other
pages reuse to what is most tied to the component's internals:

1. **Shell pieces in Leptos around the component** — breadcrumbs/path bar and the sidebar
   (shares, favourites, connections) as Leptos components talking to the component through
   attributes and events; rynas-styled toasts only (drop the component's).
2. **Transfers as rynas tasks** — once the backend turns transfer jobs into rynas tasks (UPID,
   SSE `task.*`), the task panel shows them and the transfers drawer/dialog (speed chart, ETA,
   conflicts, pause, limit, retry) is rewritten as a Leptos `TaskRun` view; the component's
   polling goes away.
3. **Dialogs** — settings, connection editor, folder picker, conflict and confirm dialogs on
   `ui::dialog`/`ui::confirm`/`form::*`, so they get the rynas form rules (field errors, i18n
   keys in `i18n.rs`).
4. **Quick look** — images (zoom/pan), video, audio, PDF, text, archives as a Leptos overlay;
   HEIC stays a JS worker (libheif) called from Rust.
5. **The pane** — list (eza style), grid and columns views, selection, keyboard (incl. Vim keys),
   fuzzy filter, type-ahead, drag and drop, virtualised rows. The biggest piece; done last
   among the views because everything else plugs into it.
6. **Editor** — CodeMirror 6 stays (a JS library), wrapped by a small Leptos component that loads
   it lazily; the micro-style chrome (status line, keys help) in Leptos.
7. **Space analysis** — rings on `<canvas>` with the `chart.rs` conventions.
8. When nothing is left in `files-app/js` but the editor bundle and libheif, drop the custom
   element and the shadow root; the texts move to `i18n.rs` keys (`files.*`).

Each step keeps the routes and JSON shapes of the API, so the backend is not touched by the rewrite.
