// Preferences: kept on the NAS per user (so they follow from one computer
// to another), with a local copy for a fast first paint. The theme is the
// rynas shell's (light/dark from <html data-theme>), not a preference here.

import { api } from "./api.js";
import { host } from "./host.js";
import { setBinary } from "./format.js";
import { setFolderIcons } from "./icons.js";

export const DEFAULTS = {
  icons: true,
  colors: true, // eza colours on names and icons
  greySystem: true, // system folders (#recycle, #snapshot…) in grey
  llColumns: true, // eza colours on permissions, sizes and dates
  size: "normal", // small, normal, large
  density: "normal", // compact, normal
  foldersFirst: true,
  showHidden: false,
  binarySizes: true,
  relativeDates: false,
  columns: { mode: true, size: true, mtime: true, kind: false },
  singleClick: false,
  vimKeys: false,
  confirmDelete: true,
  sidebar: true,
  info: false,
  view: "list",
  sort: { key: "name", dir: 1 },
  conflict: "ask",
  verify: false,
  limit: 0,
  openDialog: true,
  notify: true,
  favorites: [],
  folderIcons: { paths: {}, names: {} }, // icons given by hand: by path, or to every folder of a name
  tabs: null,
  perFolder: false, // view and sort remembered for each folder
  folderViews: {}, // path → { view, sort }
  hiddenLast: false,
  todayHighlight: false,
  startIn: "last", // last, shares, folder
  newTabIn: "current", // current, shares, folder
  startFolder: "",
  showFavorites: true, // the Favourites section of the sidebar
  volumeNames: true, // the names given to the volumes, else "Volume 1"
  volumeOrder: [], // the sidebar's groups in the order chosen by dragging
};

const LOCAL = "rynas.files.prefs";
export const prefs = structuredClone(DEFAULTS);
const listeners = new Set();

/** fn(keys): the keys that changed, or null when everything may have. */
export function onChange(fn) {
  listeners.add(fn);
}

function merge(saved) {
  for (const [k, v] of Object.entries(saved || {})) {
    if (k in DEFAULTS) prefs[k] = v && typeof v === "object" && !Array.isArray(v) && DEFAULTS[k] ? { ...DEFAULTS[k], ...v } : v;
  }
}

export function loadLocal() {
  try {
    merge(JSON.parse(localStorage.getItem(LOCAL) || "{}"));
  } catch {}
  apply();
}

/**
 * The preferences kept on the NAS: `remote` when they came already (the
 * init route), else asked for. Listeners hear about it when they differ
 * from the local copy the page started with.
 */
export async function loadRemote(remote) {
  const before = JSON.stringify(prefs);
  try {
    if (remote === undefined) remote = await api.prefs();
    if (remote && Object.keys(remote).length) merge(remote);
  } catch {}
  apply();
  if (JSON.stringify(prefs) !== before) {
    try {
      localStorage.setItem(LOCAL, JSON.stringify(prefs));
    } catch {}
    listeners.forEach((fn) => fn(null));
  }
}

let timer = 0;
/**
 * Changes and keeps preferences. Listeners hear only the keys whose value
 * really changed, so remembering the tabs after each folder opened does
 * not redraw the panes.
 */
export function save(patch) {
  const keys = [];
  for (const [k, v] of Object.entries(patch || {})) {
    // the same object back may have been changed in place: count it
    if ((v === prefs[k] && v && typeof v === "object") || JSON.stringify(prefs[k]) !== JSON.stringify(v)) keys.push(k);
    prefs[k] = v;
  }
  apply();
  try {
    localStorage.setItem(LOCAL, JSON.stringify(prefs));
  } catch {}
  clearTimeout(timer);
  timer = setTimeout(() => api.savePrefs(prefs).catch(() => {}), 700);
  if (keys.length || !patch) listeners.forEach((fn) => fn(patch ? keys : null));
}

/** The look of the page follows the preferences at once. */
export function apply() {
  setFolderIcons(prefs.folderIcons);
  const root = host.box;
  if (!root) return;
  root.dataset.theme = shellTheme();
  root.dataset.size = prefs.size;
  root.dataset.density = prefs.density;
  const app = root.querySelector("#app");
  if (app) {
    app.classList.toggle("no-icons", !prefs.icons);
    app.classList.toggle("no-colors", !prefs.colors);
    app.classList.toggle("grey-system", !!prefs.greySystem);
    app.classList.toggle("no-ll", !prefs.llColumns);
    app.classList.toggle("no-sidebar", !prefs.sidebar);
  }
  setBinary(prefs.binarySizes);
}

/** The theme the rynas shell shows: "light" or "dark". */
export function shellTheme() {
  return document.documentElement.dataset.theme === "dark" ? "dark" : "light";
}

// the shell switched theme (its toggle, or the system's while on "auto")
new MutationObserver(() => host.box && (host.box.dataset.theme = shellTheme())).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
