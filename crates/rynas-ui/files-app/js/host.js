// The file manager inside the rynas shell: one shadow root for the whole
// life of the page, and the bridge to the Leptos side.
//
// The shell renders <rynas-files> on the Files page; that element hosts
// a single <div> with the shadow root (created once, see rynas-files.js),
// so the tabs, the transfers and the editor survive a visit to another
// page. Everything the app draws goes under `host.box` (the root of the
// app inside the shadow root): dialogs, menus, toasts, drag ghosts.
//
// Events to the shell go on `window` as "rynas-files:<name>":
// - "unauth": a request was refused with 401, the session is gone;
// - "navigate" {path}: open a page of the shell (shares, permissions);
// - "toast" {text, kind}: a message while the file manager is not shown;
// - "location" {path}: the folder of the current pane (for the address bar).

export const host = {
  /** The shadow root. */
  root: null,
  /** The app's root element inside the shadow root (carries data-theme, data-size…). */
  box: null,
  /** The element holding the shadow root (moved into each <rynas-files>). */
  el: null,
  /** The session's CSRF token, set by the shell (property `csrf` of <rynas-files>). */
  csrf: "",
};

/** Appends an element to the app (dialogs, menus, overlays): inside the shadow root. */
export function mount(el) {
  host.box.append(el);
  return el;
}

/** The focused element inside the file manager (document.activeElement stops at the host). */
export function activeElement() {
  return host.root?.activeElement || null;
}

/** True while the Files page shows the app. */
export function connected() {
  return !!host.el?.isConnected;
}

/** An event for the rynas shell. */
export function emit(name, detail = {}) {
  window.dispatchEvent(new CustomEvent(`rynas-files:${name}`, { detail }));
}

/** Opens a page of the rynas shell (client-side navigation, the session stays). */
export function navigate(path) {
  emit("navigate", { path });
}

/** The element an event really started on (outside the shadow root `e.target` is the host). */
export function realTarget(e) {
  return (e.composedPath && e.composedPath()[0]) || e.target;
}

/**
 * Places a fixed overlay (menu, picker) at viewport coordinates: the app root is the
 * containing block of fixed elements (a size container), so they are offset by its corner.
 */
export function place(el, x, y) {
  const o = host.box.getBoundingClientRect();
  el.style.left = x - o.left + "px";
  el.style.top = y - o.top + "px";
}
