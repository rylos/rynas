// <rynas-files>: the rynas file manager as a Web Component (decision D-FM:
// the SynoFile UI ported first, rewritten in Leptos piece by piece later).
//
// The Leptos shell loads this module a few seconds after sign-in (or when the
// Files page opens first), outside its wasm bundle; the app itself starts only
// when the page opens. It renders
//   <rynas-files path="share/folder"></rynas-files>
// and sets the property `csrf` to the session's CSRF token.
//
// The app lives in one shadow root for the whole life of the page (styles
// isolated both ways): <rynas-files> only hosts it, so leaving the page and
// coming back finds the same tabs, transfers and editor.

import { host } from "./js/host.js";
import { tokenChanged } from "./js/api.js";
import { shellTheme } from "./js/prefs.js";

const BASE = new URL(".", import.meta.url);

// what main.js imports at start, asked for at once instead of one level after another
const PRELOAD = ["api", "prefs", "i18n", "format", "icons", "ui", "transfers", "settings", "iconpick", "tabs", "sidebar", "appdata", "backups", "actions", "dnd", "menus", "keys", "select", "undo", "rename", "dupes", "sync", "health", "eza", "pane", "perms", "conns", "archive", "uploads", "preview", "space", "fuzzy", "heic", "zoom", "lang/en"];

const CHROME = `
<div class="app" id="app">
  <header class="tabbar" id="tabbar">
    <div class="tabs" id="tabs" role="tablist"></div>
    <button class="tab-new icon-btn" id="tab-new"></button>
    <div class="tabbar-actions">
      <button class="xfer-btn" id="xfer-btn">
        <svg class="ring" viewBox="0 0 36 36" aria-hidden="true"><circle class="ring-track" cx="18" cy="18" r="15"/><circle class="ring-fill" id="xfer-ring" cx="18" cy="18" r="15"/></svg>
        <span class="xfer-icon" id="xfer-icon"></span>
        <span class="xfer-count" id="xfer-count"></span>
      </button>
      <button class="icon-btn" id="open-settings"></button>
    </div>
  </header>
  <div class="body">
    <nav class="sidebar" id="sidebar"></nav>
    <main class="workspace" id="workspace"></main>
  </div>
</div>
<div class="xfer-drawer" id="xfer-drawer" hidden>
  <div class="drawer-head">
    <span></span>
    <div>
      <button class="btn small" id="xfer-clear"></button>
      <button class="icon-btn" id="xfer-drawer-close"></button>
    </div>
  </div>
  <div class="drawer-list" id="xfer-list"></div>
</div>
<div class="menu" id="menu" role="menu" hidden></div>
<div class="toasts" id="toasts" aria-live="polite"></div>
<div class="drop-overlay" id="drop-overlay" hidden><div><b id="drop-target"></b></div></div>
<input type="file" id="upload-files" multiple hidden>
<input type="file" id="upload-folder" webkitdirectory hidden>`;

let started = null;
// the <link rel=modulepreload> elements of this attempt, null before the first one
let preloads = null;
// the styles, asked for once: adopted, or a <link> in the shadow root (never rejects)
let styled = null;

/** main.js and its imports asked for at once (fetched and compiled), and the styles fetched. */
function preload() {
  if (preloads) return;
  preloads = [];
  for (const m of ["main", ...PRELOAD]) {
    const l = document.createElement("link");
    l.rel = "modulepreload";
    l.href = new URL(`js/${m}.js`, BASE).href;
    document.head.append(l);
    preloads.push(l);
  }
  // immutable: the browser keeps it for start()
  fetch(new URL("app.css", BASE).href).catch(() => {});
}

/** The shadow root and the chrome, made once. */
function build() {
  if (host.el) return;
  // @font-face does not work inside a shadow root (Chrome): the icon font goes to the document
  if (window.FontFace && document.fonts) {
    const font = new FontFace("eza-icons", `url(${new URL("fonts/eza-icons.woff2", BASE).href})`, { display: "block" });
    document.fonts.add(font);
    font.load().catch(() => {});
  }
  const el = document.createElement("div");
  el.className = "rynas-files-host";
  const root = el.attachShadow({ mode: "open" });
  const box = document.createElement("div");
  box.className = "sf";
  box.dataset.theme = shellTheme();
  box.innerHTML = CHROME;
  // nothing shows before the styles: no flash of unstyled chrome
  box.hidden = true;
  root.append(box);
  Object.assign(host, { el, root, box });
}

/** The shadow root and the app, made once; resolves with main.js's module once the styles are in. */
function start() {
  if (started) return started;
  preload();
  build();
  const { root, box } = host;
  const href = new URL("app.css", BASE).href;
  // A constructed stylesheet stays applied when the page is left and opened again (the host
  // element is moved out of and back into the document). A <link> in the shadow root is
  // loaded again on every reconnection and the chrome showed unstyled meanwhile.
  // app.css has no relative url(), so the sheet's base URL does not matter.
  styled ??= (
    "adoptedStyleSheets" in Document.prototype && "replace" in CSSStyleSheet.prototype
      ? fetch(href)
          .then((r) => (r.ok ? r.text() : Promise.reject(new Error(r.status))))
          .then((text) => new CSSStyleSheet().replace(text))
          .then((sheet) => {
            root.adoptedStyleSheets = [sheet];
          })
      : Promise.reject(new Error("no constructable stylesheets"))
  ).catch(
    () =>
      new Promise((resolve) => {
        const css = document.createElement("link");
        css.rel = "stylesheet";
        css.href = href;
        css.onload = resolve;
        // not there either: asked for again by the next start
        css.onerror = () => {
          css.remove();
          styled = null;
          resolve();
        };
        root.prepend(css);
      }),
  );
  const mine = styled.then(() => {
    box.hidden = false;
    return import("./js/main.js");
  });
  started = mine;
  // the app did not load (the NAS did not answer): the next opening of the page tries again,
  // preloads included
  mine.catch(() => {
    if (started !== mine) return;
    started = null;
    preloads?.forEach((l) => l.remove());
    preloads = null;
  });
  return started;
}

class RynasFiles extends HTMLElement {
  static observedAttributes = ["path"];

  connectedCallback() {
    // a value set by the shell before this module was loaded is an own property: take it
    if (Object.prototype.hasOwnProperty.call(this, "csrf")) {
      const v = this.csrf;
      delete this.csrf;
      this.csrf = v;
    }
    const ready = start();
    this.append(host.el);
    // a failed start is tried again at the next connect (start)
    ready.then(() => this.open(), () => {});
  }

  attributeChangedCallback() {
    if (this.isConnected && started) started.then(() => this.open(), () => {});
  }

  /** The session's CSRF token (the shell's `prop:csrf`). */
  set csrf(v) {
    if (!v || v === host.csrf) return;
    host.csrf = String(v);
    tokenChanged();
  }

  get csrf() {
    return host.csrf;
  }

  /** The folder of the address (/files/<path>), when there is one. */
  open() {
    const path = this.getAttribute("path");
    if (!path) return;
    started?.then((m) => m.app.openFromShell(path), () => {});
  }
}

if (!customElements.get("rynas-files")) customElements.define("rynas-files", RynasFiles);

// the shell loads this module ahead, some seconds after sign-in (pages/files.rs, warm): the
// rest follows when the browser is idle, so the Files page opens without waiting for it
if (window.requestIdleCallback) requestIdleCallback(preload, { timeout: 5000 });
else setTimeout(preload, 1000);
