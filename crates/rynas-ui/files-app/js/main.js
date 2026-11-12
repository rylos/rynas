// The rynas file manager (ported from SynoFile): the application around the
// panes. App holds the state of the page and starts it; its methods live by
// subject in tabs.js, sidebar.js, actions.js, dnd.js, menus.js, keys.js and
// health.js. rynas-files.js mounts it in the shell and imports this module.

import { api } from "./api.js";
import { prefs, save, loadLocal, loadRemote, onChange } from "./prefs.js";
import { t, loadTable, onLangChange } from "./i18n.js";
import { emit } from "./host.js";
import { baseName, parentOf, joinPath } from "./format.js";
import { svg } from "./icons.js";
import { $, toast } from "./ui.js";
import * as transfers from "./transfers.js";
import { openSettings } from "./settings.js";
import { moveFolderIcons } from "./iconpick.js";
import { tabMethods } from "./tabs.js";
import { sidebarMethods } from "./sidebar.js";
import { actionMethods } from "./actions.js";
import { dndMethods } from "./dnd.js";
import { menuMethods } from "./menus.js";
import { keyMethods } from "./keys.js";
import { healthMethods } from "./health.js";
import { changed as appChanged } from "./appdata.js";

// what each preference redraws when it changes (prefsChanged)
const QUIET_PREFS = new Set(["tabs", "folderViews", "sort", "view", "sidebar", "singleClick", "vimKeys", "confirmDelete", "conflict", "limit", "openDialog", "notify", "startIn", "newTabIn", "startFolder", "perFolder"]);
const RESORT_PREFS = new Set(["showHidden", "foldersFirst", "hiddenLast"]);
const REPAINT_PREFS = new Set(["columns", "binarySizes", "relativeDates", "todayHighlight", "llColumns", "icons", "colors", "size", "density", "folderIcons", "info"]);
const SIDEBAR_PREFS = new Set(["favorites", "showFavorites", "volumeNames", "volumeOrder", "folderIcons"]);

class App {
  constructor() {
    this.tabs = [];
    this.current = null;
    this.closed = [];
    this.clipboard = null;
    this.shares = [];
    this.user = "";
    this.conns = [];
    this.connKey = null;
    this.hostname = "";
  }

  async start() {
    loadLocal();
    this.translateChrome();
    this.popOutButton();
    $("#tab-new").innerHTML = svg("plus");
    $("#tab-new").onclick = () => this.newTab(this.newTabPath());
    $("#open-settings").innerHTML = svg("settings");
    $("#open-settings").onclick = () => openSettings();
    addEventListener("rynas-files:auth-lost", () => this.needLogin());
    addEventListener("rynas-files:auth", () => this.sessionBack());
    onLangChange(() => this.languageChanged());
    await this.boot();
  }

  /** What needs the server; run again by "Retry" when the files API did not answer. */
  async boot() {
    if (this.started || this.booting) return;
    this.booting = true;
    const init = await this.loadInit().finally(() => (this.booting = false));
    if (!init) return;
    this.clearBanner("unavailable");
    await loadRemote(init.prefs);
    onChange((keys) => this.prefsChanged(keys));
    if (init.shares) this.applyShares(init.shares);
    if (init.conns) this.applyConns(init.conns);
    this.restoreTabs();
    this.bindKeys();
    this.bindGlobalDrop();
    this.bindUploadInputs();
    transfers.init(this);
    if (!init.shares) this.loadShares();
    if (!init.conns) this.loadConns();
    if (init.whoami) this.checkHealth(init.whoami);
    this.started = true;
    for (const m of this.pendingOpen || []) this.openFromShell(m.path, m.select);
    this.pendingOpen = null;
  }

  /**
   * The shell asks for a folder (the address /files/<path>, or another page
   * linking here): a tab showing it, or a new one.
   */
  openFromShell(path, select) {
    if (!path) return;
    if (!this.started) return void (this.pendingOpen ||= []).push({ path, select });
    const p = "/" + path.replace(/^\/+|\/+$/g, "");
    const shown = this.tabs.find((tab) => tab.panes.some((pane) => pane.path === p));
    if (shown) {
      this.switchTab(shown);
      if (select) this.selectWhenReady(shown, select);
    } else this.openAt(p, select);
  }

  /** The shell switched language: what is drawn now is drawn again in the new one. */
  languageChanged() {
    this.translateChrome();
    this.renderTabs();
    this.renderSidebar();
    for (const tab of this.tabs) for (const p of tab.panes) if (tab === this.current) p.render();
    transfers.renderAll?.();
  }

  /**
   * What the page needs to start, in one request: who is there,
   * preferences, shared folders and connections. A part missing is
   * undefined, and asked for later on its own; null: nothing to start.
   */
  async loadInit() {
    try {
      return await api.init();
    } catch (e) {
      if (e.status === 401) return null; // the shell's login screen is up already
      if (e.status === 403) {
        this.adminOnly();
        return null;
      }
      if (e.status === 0 || e.status >= 500 || e.status === 404) {
        this.unavailable(e);
        return null;
      }
    }
    // a NAS without the init route, or a hiccup: the requests one by one
    const [whoami, prefs] = await Promise.all([api.whoami().catch(() => undefined), api.prefs().catch(() => undefined)]);
    return { whoami, prefs };
  }

  get pane() {
    return this.current?.pane;
  }

  paneChanged(pane) {
    if (this.current?.panes.includes(pane)) {
      this.renderTabs();
      this.renderSidebar();
    }
    this.saveTabs();
    if (pane === this.pane) emit("location", { path: pane.path });
  }

  selectionChanged(pane) {
    pane.renderInfo();
  }

  rememberSort(sort, pane) {
    save({ sort: { ...sort } });
    this.rememberFolder(pane);
  }

  rememberView(view, pane) {
    save({ view });
    this.rememberFolder(pane);
    this.saveTabs();
  }

  /** With "remember for each folder": this folder's view and sort, the 500 latest. */
  rememberFolder(pane) {
    if (!prefs.perFolder || !pane || pane.path === "/") return;
    const all = { ...prefs.folderViews };
    delete all[pane.path];
    all[pane.path] = { view: pane.view, sort: { ...pane.sort } };
    const keys = Object.keys(all);
    for (const k of keys.slice(0, Math.max(0, keys.length - 500))) delete all[k];
    save({ folderViews: all });
  }


  /**
   * Preferences changed (`keys`, or null for any): only what they touch is
   * redrawn. The tabs remembered after each folder opened redraw nothing,
   * so a double click in progress is not lost.
   */
  prefsChanged(keys) {
    const known = (k) => QUIET_PREFS.has(k) || RESORT_PREFS.has(k) || REPAINT_PREFS.has(k) || SIDEBAR_PREFS.has(k);
    const all = !keys || keys.some((k) => !known(k));
    const any = (set) => all || keys.some((k) => set.has(k));
    const resort = any(RESORT_PREFS);
    if (resort || any(REPAINT_PREFS)) {
      // the other tabs draw themselves when shown again
      for (const tab of this.tabs)
        for (const p of tab.panes) {
          if (resort) p.refilter();
          if (tab === this.current) p.render();
        }
    }
    if (all || keys.includes("folderIcons")) this.renderTabs();
    if (any(SIDEBAR_PREFS)) this.renderSidebar();
  }

  toggleHidden() {
    save({ showHidden: !prefs.showHidden });
    toast(prefs.showHidden ? t("File nascosti visibili") : t("File nascosti non visibili"), { timeout: 1500 });
  }

  toggleInfo() {
    save({ info: !prefs.info });
  }

  allPanes() {
    return this.tabs.flatMap((t) => t.panes);
  }

  /** Reloads every pane showing one of these folders. */
  refreshFolders(paths) {
    const set = new Set(paths);
    for (const p of this.allPanes()) if (set.has(p.path)) p.reload();
  }

  refreshAfter(job) {
    if (job.op === "move" && job.state === "done" && !job.progress.errors) for (const p of job.info.sources) moveFolderIcons(p, joinPath(job.info.dest, baseName(p)));
    const dirs = new Set(job.info.sources.map(parentOf));
    // a deletion fills or empties a bin: the share's top shows which
    if (job.op === "delete") for (const p of job.info.sources) dirs.add("/" + p.split("/")[1]);
    if (job.info.dest) dirs.add(job.info.dest);
    this.refreshFolders([...dirs]);
    this.loadShares();
    // what the job changed in an app's folder: the destination, and the sources it removed
    if (job.state === "done") appChanged(job.op === "delete" || job.op === "move" ? job.info.sources : [], job.info.dest ? [job.info.dest] : []);
  }

  openPath(path) {
    this.pane.go(path);
  }
}

// the rest of App's methods, by subject
Object.assign(App.prototype, tabMethods, sidebarMethods, actionMethods, dndMethods, menuMethods, keyMethods, healthMethods);

await loadTable();
export const app = new App();
app.start();
