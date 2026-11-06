// <rynas-files>: the rynas file manager as a Web Component (decision D-FM:
// the SynoFile UI ported first, rewritten in Leptos piece by piece later).
//
// The Leptos shell loads this module only when the Files page opens, so the
// file manager costs nothing to the rest of the interface. It renders
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
const PRELOAD = ["api", "prefs", "i18n", "format", "icons", "ui", "transfers", "settings", "iconpick", "tabs", "sidebar", "actions", "dnd", "menus", "keys", "health", "eza", "pane", "perms", "conns", "archive", "uploads", "preview", "space", "fuzzy", "heic", "zoom", "lang/en"];

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

/** The shadow root and the app, made once; resolves with main.js's module once the styles are in. */
function start() {
  if (started) return started;
  for (const m of PRELOAD) {
    const l = document.createElement("link");
    l.rel = "modulepreload";
    l.href = new URL(`js/${m}.js`, BASE).href;
    document.head.append(l);
  }
  // @font-face does not work inside a shadow root (Chrome): the icon font goes to the document
  if (window.FontFace && document.fonts) {
    const font = new FontFace("eza-icons", `url(${new URL("fonts/eza-icons.woff2", BASE).href})`, { display: "block" });
    document.fonts.add(font);
    font.load().catch(() => {});
  }
  const el = document.createElement("div");
  el.className = "rynas-files-host";
  const root = el.attachShadow({ mode: "open" });
  const css = document.createElement("link");
  css.rel = "stylesheet";
  css.href = new URL("app.css", BASE).href;
  const box = document.createElement("div");
  box.className = "sf";
  box.dataset.theme = shellTheme();
  box.innerHTML = CHROME;
  // nothing shows before the styles: no flash of unstyled chrome
  box.hidden = true;
  root.append(css, box);
  Object.assign(host, { el, root, box });
  started = new Promise((resolve) => (css.onload = css.onerror = resolve)).then(() => {
    box.hidden = false;
    return import("./js/main.js");
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
    ready.then(() => this.open());
  }

  attributeChangedCallback() {
    if (this.isConnected && started) started.then(() => this.open());
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
    started.then((m) => m.app.openFromShell(path));
  }
}

if (!customElements.get("rynas-files")) customElements.define("rynas-files", RynasFiles);
