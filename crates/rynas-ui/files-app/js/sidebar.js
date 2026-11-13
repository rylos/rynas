// The sidebar: favourites, shared folders by volume, remote connections;
// reordering them, their menus, ejecting USB drives. Methods of App (main.js).

import { api } from "./api.js";
import { sharePermissions, shareDescription } from "./perms.js";
import { prefs, save } from "./prefs.js";
import { t } from "./i18n.js";
import { esc, size, baseName, isDirLike } from "./format.js";
import { svg, glyph, shareGlyph, folderGlyphHTML } from "./icons.js";
import { $, toast, errorToast, openMenu, confirmBox, promptBox, copyText } from "./ui.js";
import * as transfers from "./transfers.js";
import { pickIcon } from "./iconpick.js";
import { isRemote, connId, editConnection, renameConnection, parseAddress, serverAddress, startPath } from "./conns.js";
import { APPS, APPS_PATH, setAppFolders, appFolders } from "./appdata.js";
import { BACKUPS, BACKUPS_PATH, setBackupFolders, backupFolders, isBackupTop, pathLabel } from "./backups.js";

// a shared folder on a removable drive: `removable` from the server, or the
// volume names of USB and eSATA drives on volumes migrated as they are
// (volumeUSB1, volumeSATA1…)
const REMOVABLE = /^volume(USB|SATA)\d/i;
const isRemovable = (s) => s.removable === true || REMOVABLE.test(s.volume || "");

// a moment within which the shared folders just read are not read again
const SHARES_FRESH_MS = 1500;

export const sidebarMethods = {
  /** The name a volume is shown with: the one given to it (the system volume's reserved
   * label "system" translated), else "Volume 1" from its mount; `plain` is the latter. */
  volumeTitle(vol) {
    if (vol === "home") return { title: t("Personale"), plain: t("Personale") };
    const plain = vol.replace("volume", t("Volume") + " ").replace("USB", "USB ");
    const given = this.volumeNames?.[vol];
    if (given === "system") return { title: t("Sistema"), plain: t("Sistema") };
    return { title: (prefs.volumeNames && given) || plain, plain };
  },

  applyShares(data) {
    this.shares = data.shares || [];
    // the apps' folders (administrators, when the Docker setting shows them)
    setAppFolders(data.apps);
    // the PCs' backups (administrators, when PCs send btrfs snapshots)
    setBackupFolders(data.backups);
    this.user = data.user || this.user;
    this.volumeNames = data.volumes || {};
    this.hostname = data.hostname || "";
    this.sharesAt = Date.now();
    this.renderSidebar();
  },

  /** Reads the shared folders again (one request at a time); throws on failure. */
  fetchShares() {
    this.sharesReq ??= api
      .shares()
      .then((data) => {
        this.applyShares(data);
        return this.shares;
      })
      .finally(() => (this.sharesReq = null));
    return this.sharesReq;
  },

  /** The shared folders, for the pane at the top: those just read, else asked again. */
  async freshShares() {
    if (this.sharesAt && Date.now() - this.sharesAt < SHARES_FRESH_MS) return this.shares;
    return this.fetchShares();
  },

  async loadShares() {
    try {
      await this.fetchShares();
    } catch (e) {
      // a 401 has its banner already
      if (e.status === 403) this.adminOnly();
    }
  },

  applyConns(data) {
    this.conns = data.conns || [];
    this.connKey = data.key || null;
    this.renderSidebar();
    this.allPanes().forEach((p) => isRemote(p.path) && p.renderChrome());
  },

  async loadConns() {
    try {
      this.applyConns(await api.conns());
    } catch {}
  },

  connOf(path) {
    const id = connId(path);
    return id ? this.conns.find((c) => c.id === id) || null : null;
  },

  /** The last part of a path; a connection's root goes by its name. */
  pathName(path) {
    if (path === APPS_PATH) return t("Dati delle app");
    if (path === BACKUPS_PATH) return t("Backups");
    // a PC's snapshot by its date (the tab's tooltip keeps the real path)
    const snapshot = pathLabel(path);
    if (snapshot) return snapshot;
    return isRemote(path) && path.split("/").filter(Boolean).length === 1 ? this.connName(path) : baseName(path);
  },

  connName(path) {
    return this.connOf(path)?.name || t("Connessione eliminata");
  },

  /** A path as people read it: a server's own address for a remote one. */
  displayPath(path) {
    const c = this.connOf(path);
    return c ? serverAddress(c, path, "smb") : path;
  },

  async addConnection(draft = {}) {
    const saved = await editConnection(draft, this.connKey);
    if (!saved) return null;
    await this.loadConns();
    return saved;
  },

  async editConn(conn) {
    const saved = await editConnection(conn, this.connKey);
    if (saved) await this.loadConns();
  },

  async renameConn(conn) {
    const name = await promptBox({ title: t("Rinomina connessione"), label: t("Nome nella barra laterale"), value: conn.name, ok: t("Rinomina"), validate: (v) => (!v.trim() ? t("Il nome non può essere vuoto") : null) });
    if (!name || name.trim() === conn.name) return;
    try {
      await renameConnection(conn, name.trim());
      await this.loadConns();
    } catch (e) {
      errorToast(e);
    }
  },

  async deleteConn(conn) {
    const ok = await confirmBox({ title: t("Eliminare la connessione?"), message: t("«{n}» sparisce dalla barra laterale, con la password salvata. I file sul server restano dove sono.", { n: esc(conn.name) }), ok: t("Elimina"), danger: true });
    if (!ok) return;
    try {
      await api.deleteConn(conn.id);
      await this.loadConns();
      for (const p of this.allPanes()) if (connId(p.path) === conn.id) p.go("/");
    } catch (e) {
      errorToast(e);
    }
  },

  async moveConn(from, to) {
    const ids = this.conns.map((c) => c.id);
    to = Math.max(0, Math.min(ids.length - 1, to));
    if (from === to) return;
    const [moved] = ids.splice(from, 1);
    ids.splice(to, 0, moved);
    this.conns.sort((a, b) => ids.indexOf(a.id) - ids.indexOf(b.id));
    this.renderSidebar();
    try {
      await api.orderConns(ids);
    } catch (e) {
      errorToast(e);
    }
  },

  connMenu(conn, x, y) {
    const i = this.conns.indexOf(conn);
    const path = startPath(conn);
    const fav = (prefs.favorites || []).some((f) => f.path === path);
    openMenu(x, y, [
      { title: conn.host },
      { label: t("Apri"), icon: "forward", action: () => this.pane.go(path) },
      { label: t("Apri in una nuova scheda"), icon: "plus", action: () => this.newTab(path) },
      this.isSplit() && { label: t("Apri nell'altro pannello"), icon: "split", action: () => this.other().go(path) },
      conn.kind === "smb" && conn.path && { label: t("Cartelle condivise del server"), icon: "server", action: () => this.pane.go("/:" + conn.id) },
      "-",
      { label: t("Rinomina…"), icon: "rename", action: () => this.renameConn(conn) },
      { label: t("Modifica…"), icon: "settings", action: () => this.editConn(conn) },
      i > 0 && { label: t("Sposta su"), icon: "up", action: () => this.moveConn(i, i - 1) },
      i < this.conns.length - 1 && { label: t("Sposta giù"), icon: "up", action: () => this.moveConn(i, i + 1) },
      "-",
      fav ? { label: t("Togli dai preferiti"), icon: "star", action: () => this.toggleFavorite(path) } : { label: t("Aggiungi ai preferiti"), icon: "star", action: () => this.toggleFavorite(path) },
      { label: t("Copia indirizzo"), icon: "link", action: () => copyText(serverAddress(conn, path, "smb")) },
      "-",
      { label: t("Elimina connessione"), icon: "trash", danger: true, action: () => this.deleteConn(conn) },
    ]);
  },

  /**
   * An address typed in the path bar: \\server\share, smb://, sftp://,
   * the NAS itself, /volume1/share… A server with no connection yet opens
   * the dialog to add one.
   */
  async goAddress(text, pane = this.pane) {
    const v = text.trim();
    if (isRemote(v)) return pane.go(v);
    const a = /^(\\\\|\/\/|smb:|cifs:|sftp:|ssh:)/i.test(v) ? parseAddress(v) : null;
    if (!a) return pane.go(v.replace(/\\/g, "/").replace(/^\/volume\d+/, ""));
    const host = a.host.toLowerCase();
    const short = (h) => h.split(".")[0];
    const nas = [location.hostname, this.hostname].filter(Boolean).map((h) => h.toLowerCase());
    if (a.kind !== "sftp" && nas.some((n) => n === host || short(n) === short(host))) return pane.go("/" + a.path);
    const conn = this.conns.find((c) => (c.host.toLowerCase() === host || short(c.host.toLowerCase()) === short(host)) && (!a.kind || c.kind === a.kind) && (!a.port || !c.port || c.port === a.port));
    if (conn) return pane.go("/:" + conn.id + (a.path ? "/" + a.path : ""));
    const saved = await this.addConnection({ kind: a.kind || "smb", host: v });
    if (saved) pane.go(startPath(saved));
  },

  renderSidebar() {
    const el = $("#sidebar");
    const cur = this.pane?.path || "/";
    const item = (path, name, gl, sub = "", extra = "", fav = null, gone = false) => {
      const active = cur === path || (path !== "/" && cur.startsWith(path + "/") && !extra.includes("nested"));
      const favAttrs = fav === null ? "" : ` data-fav="${fav}" draggable="true"`;
      return `<button class="side-item ${active ? "active" : ""} ${gone ? "gone" : ""}" data-path="${esc(path)}"${favAttrs}><i class="gi">${gl}</i><span class="si-text"><span class="si-name">${esc(name)}</span>${sub}</span>${extra.replace("nested", "")}</button>`;
    };
    let html = "";
    const favs = prefs.favorites || [];
    if (prefs.showFavorites) {
      html += `<div class="side-title fav-title">${t("Preferiti")}</div>`;
      // a favourite on a shared folder that is gone (a USB drive unplugged)
      const there = new Set(this.shares.map((s) => s.name));
      // …or an app's folder no longer shown (the app removed, the setting off)
      const appGone = (p) => !appFolders().length || (p !== APPS_PATH && !appFolders().some((a) => a.name === p.split("/")[2]));
      // …or a PC's backup no longer there
      const backupGone = (p) => !backupFolders().length || (p !== BACKUPS_PATH && !backupFolders().some((b) => b.name === p.split("/")[2]));
      const top = (f) => f.path.split("/")[1];
      const gone = (f) => (isRemote(f.path) ? !this.connOf(f.path) : top(f) === APPS ? appGone(f.path) : top(f) === BACKUPS ? backupGone(f.path) : this.shares.length && !there.has(top(f)));
      if (favs.length) html += favs.map((f, i) => item(f.path, f.name || baseName(f.path), f.path.split("/").length === 2 && isRemote(f.path) ? svg("server") : folderGlyphHTML(f.path, baseName(f.path), glyph({ kind: "d", name: baseName(f.path) })), gone(f) ? `<span class="si-sub">${t("non disponibile")}</span>` : "", "nested" + (gone(f) ? "" : this.ejectButton(f.path)), i, gone(f))).join("");
      else html += `<div class="hint" style="padding:2px 10px 6px">${t("Ctrl+D aggiunge la cartella aperta")}</div>`;
    }
    const volumes = new Map();
    for (const s of this.shares) {
      const v = s.home ? "home" : s.volume;
      if (!volumes.has(v)) volumes.set(v, []);
      volumes.get(v).push(s);
    }
    for (const vol of this.volumeOrder([...volumes.keys()])) {
      const list = volumes.get(vol);
      const first = list[0];
      const used = first.total ? 1 - first.free / first.total : 0;
      // the name given to the volume, with the plain one as the tooltip
      const { title, plain } = this.volumeTitle(vol);
      html += `<div class="side-title vol-title" data-vol="${esc(vol)}" draggable="true"${title !== plain ? ` title="${esc(plain)}"` : ""}><span>${esc(title)}</span>${first.total && vol !== "home" ? `<span title="${t("{f} liberi di {t}", { f: size(first.free), t: size(first.total) })}">${size(first.free)}</span>` : ""}</div>`;
      if (first.total && vol !== "home") html += `<div class="vol-bar ${used > 0.95 ? "full" : used > 0.85 ? "high" : ""}" style="margin:0 10px 6px"><div style="width:${(used * 100).toFixed(1)}%"></div></div>`;
      for (const s of list) {
        const badges = (s.encrypted ? `<span class="si-lock" title="${t("Cartella crittografata")}">${svg("lock")}</span>` : "") + (s.hidden ? `<span class="si-badge" title="${t("Nascosta in Rete")}">${t("nasc.")}</span>` : "") + (s.holds_backup ? `<span class="si-badge" title="${esc(t("Si sovrappone alla cartella dei backup del PC {pc}: in sola lettura finché i backup non hanno una cartella propria", { pc: s.holds_backup }))}">${t("sola lettura")}</span>` : "") + this.ejectButton("/" + s.name);
        html += item("/" + s.name, s.home ? t("Home ({u})", { u: this.user }) : s.name, folderGlyphHTML("/" + s.name, s.name, s.home ? shareGlyph("home") : shareGlyph(s.name)), s.comment ? `<span class="si-sub">${esc(s.comment)}</span>` : "", badges);
      }
    }
    const apps = appFolders();
    if (apps.length) {
      html += `<div class="side-title"><span>${t("App")}</span></div>`;
      html += item(APPS_PATH, t("Dati delle app"), svg("box"), `<span class="si-sub">${t("{n} cartelle", { n: apps.length })}</span>`);
    }
    const backups = backupFolders();
    if (backups.length) {
      // one entry, no section title of its own (it would say "Backups" twice): a rule sets it apart
      html += `<div class="side-sep"></div>`;
      // the same icon as its tab
      html += item(BACKUPS_PATH, t("Backups"), folderGlyphHTML(BACKUPS_PATH, t("Backups"), glyph({ kind: "d", name: t("Backups") })), `<span class="si-sub">${t("sola lettura")}</span>`);
    }
    html += `<div class="side-title conn-title"><span>${t("Connessioni")}</span><button class="side-add" title="${t("Nuova connessione")}">${svg("plus")}</button></div>`;
    if (this.conns.length) {
      html += this.conns
        .map((c, i) => {
          const path = startPath(c);
          const active = connId(cur) === c.id;
          return `<button class="side-item ${active ? "active" : ""}" data-path="${esc(path)}" data-conn="${i}" draggable="true"><i class="gi">${svg("server")}</i><span class="si-text"><span class="si-name">${esc(c.name)}</span><span class="si-sub">${esc((c.kind === "sftp" ? "SFTP · " : "") + c.host)}</span></span></button>`;
        })
        .join("");
    } else html += `<div class="hint" style="padding:2px 10px 6px">${t("Server SMB o SFTP: \\\\server nel percorso, o il +")}</div>`;
    el.innerHTML = html;
    el.querySelector(".side-add").onclick = async () => {
      const saved = await this.addConnection();
      if (saved) this.pane.go(startPath(saved));
    };
    this.bindConnDrag(el);
    el.querySelectorAll(".si-eject").forEach((b) => {
      b.onclick = (e) => {
        e.stopPropagation();
        this.eject(b.dataset.share);
      };
    });
    el.querySelectorAll(".side-item").forEach((b) => {
      b.onclick = () => this.pane.go(b.dataset.path);
      b.onauxclick = (e) => e.button === 1 && this.newTab(b.dataset.path, false);
      b.oncontextmenu = (e) => {
        e.preventDefault();
        if (b.dataset.conn !== undefined) return this.connMenu(this.conns[+b.dataset.conn], e.clientX, e.clientY);
        this.sideMenu(b.dataset.path, e.clientX, e.clientY, b.dataset.fav === undefined ? null : +b.dataset.fav);
      };
      this.makeDropTarget(b, () => b.dataset.path);
    });
    this.bindFavoriteDrag(el);
    this.bindVolumeDrag(el);
  },

  /**
   * The groups of the sidebar: the order chosen (dragging a title,
   * or its menu), the others after it in the natural one: personal home,
   * volume1, volume2…, then USB and the rest.
   */
  volumeOrder(vols) {
    const rank = (v) => {
      if (v === "home") return [0, 0];
      const m = v.match(/^volume(\d+)$/);
      if (m) return [1, +m[1]];
      const u = v.match(/^volumeUSB(\d+)$/);
      if (u) return [2, +u[1]];
      return [3, 0];
    };
    const natural = [...vols].sort((a, b) => {
      const [x, y] = [rank(a), rank(b)];
      return x[0] - y[0] || x[1] - y[1] || a.localeCompare(b);
    });
    const saved = (prefs.volumeOrder || []).filter((v) => vols.includes(v));
    return [...saved, ...natural.filter((v) => !saved.includes(v))];
  },

  moveVolume(vol, to) {
    const order = this.volumeOrder([...new Set(this.shares.map((s) => (s.home ? "home" : s.volume)))]);
    const from = order.indexOf(vol);
    if (from < 0) return;
    to = Math.max(0, Math.min(order.length - 1, to));
    if (from === to) return;
    order.splice(from, 1);
    order.splice(to, 0, vol);
    save({ volumeOrder: order });
  },

  /**
   * Items of the sidebar that reorder by dragging (favourites, connections,
   * volume groups): a line shows where the item lands, `move(from, to)`
   * gets the indexes in `items`.
   */
  bindReorder(items, type, move) {
    const clear = () => items.forEach((x) => x.classList.remove("fav-before", "fav-after"));
    items.forEach((b, i) => {
      b.addEventListener("dragstart", (e) => {
        e.dataTransfer.setData(type, String(i));
        e.dataTransfer.effectAllowed = "move";
      });
      b.addEventListener("dragover", (e) => {
        if (!e.dataTransfer.types.includes(type)) return;
        e.preventDefault();
        e.stopPropagation();
        const r = b.getBoundingClientRect();
        clear();
        b.classList.add(e.clientY < r.top + r.height / 2 ? "fav-before" : "fav-after");
      });
      b.addEventListener("dragleave", () => b.classList.remove("fav-before", "fav-after"));
      b.addEventListener("drop", (e) => {
        if (!e.dataTransfer.types.includes(type)) return;
        e.preventDefault();
        e.stopPropagation();
        const after = b.classList.contains("fav-after");
        clear();
        const from = +e.dataTransfer.getData(type);
        let to = i + (after ? 1 : 0);
        if (from < to) to--;
        move(from, to);
      });
      b.addEventListener("dragend", clear);
    });
  },

  bindVolumeDrag(el) {
    const titles = [...el.querySelectorAll(".vol-title")];
    this.bindReorder(titles, "application/x-rynas-files-vol", (from, to) => titles[from] && this.moveVolume(titles[from].dataset.vol, to));
    titles.forEach((b, i) => {
      b.oncontextmenu = (e) => {
        e.preventDefault();
        openMenu(e.clientX, e.clientY, [
          i > 0 && { label: t("Sposta su"), icon: "up", action: () => this.moveVolume(b.dataset.vol, i - 1) },
          i < titles.length - 1 && { label: t("Sposta giù"), icon: "up", action: () => this.moveVolume(b.dataset.vol, i + 1) },
          (prefs.volumeOrder || []).length > 0 && "-",
          (prefs.volumeOrder || []).length > 0 && { label: t("Ordine predefinito"), icon: "refresh", action: () => save({ volumeOrder: [] }) },
          ...(() => {
            const share = this.shares.find((s) => s.volume === b.dataset.vol && isRemovable(s));
            return share ? ["-", { label: t("Espelli"), icon: "eject", action: () => this.eject(share.name) }] : [];
          })(),
        ]);
      };
    });
  },

  /** Connections reorder by dragging, like the favourites. */
  bindConnDrag(el) {
    this.bindReorder([...el.querySelectorAll("[data-conn]")], "application/x-rynas-files-conn", (from, to) => this.moveConn(from, to));
  },

  /** Favourites reorder by dragging; a folder dropped on their title becomes one. */
  bindFavoriteDrag(el) {
    this.bindReorder([...el.querySelectorAll("[data-fav]")], "application/x-rynas-files-fav", (from, to) => this.moveFavorite(from, to));
    const title = el.querySelector(".fav-title");
    if (!title) return;
    title.addEventListener("dragover", (e) => {
      if (!e.dataTransfer.types.includes("application/x-rynas-files")) return;
      e.preventDefault();
      title.classList.add("drop");
    });
    title.addEventListener("dragleave", () => title.classList.remove("drop"));
    title.addEventListener("drop", (e) => {
      title.classList.remove("drop");
      const raw = e.dataTransfer.getData("application/x-rynas-files");
      if (!raw) return;
      e.preventDefault();
      const known = new Set((prefs.favorites || []).map((f) => f.path));
      const dirs = JSON.parse(raw).paths.filter((p) => !known.has(p) && this.allPanes().some((pane) => pane.visible.some((x) => x.path === p && isDirLike(x))));
      if (dirs.length) save({ favorites: [...(prefs.favorites || []), ...dirs.map((p) => ({ path: p, name: baseName(p) }))] });
    });
  },

  moveFavorite(from, to) {
    const favs = [...(prefs.favorites || [])];
    if (from < 0 || from >= favs.length) return;
    to = Math.max(0, Math.min(favs.length - 1, to));
    if (from === to) return;
    const [moved] = favs.splice(from, 1);
    favs.splice(to, 0, moved);
    save({ favorites: favs });
  },

  sideMenu(path, x, y, favIndex = null) {
    const fav = (prefs.favorites || []).some((f) => f.path === path);
    const share = this.shares.find((s) => "/" + s.name === path);
    const last = (prefs.favorites || []).length - 1;
    openMenu(x, y, [
      favIndex !== null && favIndex > 0 && { label: t("Sposta su"), icon: "up", action: () => this.moveFavorite(favIndex, favIndex - 1) },
      favIndex !== null && favIndex < last && { label: t("Sposta giù"), icon: "up", action: () => this.moveFavorite(favIndex, favIndex + 1) },
      favIndex !== null && (favIndex > 0 || favIndex < last) && "-",
      { label: t("Apri"), icon: "forward", action: () => this.pane.go(path) },
      { label: t("Apri in una nuova scheda"), icon: "plus", action: () => this.newTab(path) },
      this.isSplit() && { label: t("Apri nell'altro pannello"), icon: "split", action: () => this.other().go(path) },
      share?.recycle && { label: t("Apri il Cestino"), icon: "trash", action: () => this.pane.go(path + "/#recycle") },
      share?.recycle && { label: t("Svuota il Cestino"), icon: "trash", danger: true, action: () => this.emptyBin(path + "/#recycle") },
      !isRemote(path) && !isBackupTop(path) && { label: t("Analisi spazio"), icon: "pie", action: () => this.analyseSpace(path) },
      "-",
      fav ? { label: t("Togli dai preferiti"), icon: "star", action: () => this.toggleFavorite(path) } : { label: t("Aggiungi ai preferiti"), icon: "star", action: () => this.toggleFavorite(path) },
      { label: t("Copia percorso"), icon: "link", action: () => copyText(this.displayPath(path)) },
      !(isRemote(path) && path.split("/").length === 2) && { label: t("Icona…"), icon: "palette", action: () => pickIcon({ x, y }, path) },
      share && !share.home && share.volume && !this.removableShare(path) && { label: t("Descrizione…"), icon: "rename", action: () => shareDescription(share) },
      share && !share.home && share.volume && !this.removableShare(path) && { label: t("Permessi…"), icon: "shield", action: () => sharePermissions(share) },
      this.removableShare(path) && "-",
      this.removableShare(path) && { label: t("Espelli"), icon: "eject", action: () => this.eject(this.removableShare(path).name) },
    ]);
  },

  /** The shared folder on a USB or eSATA drive that holds `path`, if any. */
  removableShare(path) {
    if (!path || isRemote(path)) return null;
    const name = path.split("/")[1];
    return this.shares.find((s) => s.name === name && isRemovable(s)) || null;
  },

  ejectButton(path) {
    const share = this.removableShare(path);
    return share ? `<span class="si-eject" role="button" data-share="${esc(share.name)}" title="${t("Espelli")}">${svg("eject")}</span>` : "";
  },

  /**
   * Ejects the drive of a shared folder (route `eject` of the files API:
   * unmounts every partition of the drive and powers it off). The shared
   * folders of the same drive go with it.
   */
  async eject(shareName) {
    const share = this.shares.find((s) => s.name === shareName);
    if (!share) return toast(t("Il dispositivo di «{s}» non risulta collegato", { s: shareName }), { kind: "error" });
    // the other shared folders of the same drive (`device` from the server, else the volume)
    const same = (s) => (share.device ? s.device === share.device : s.volume === share.volume);
    const names = this.shares.filter((s) => isRemovable(s) && same(s)).map((s) => s.name);
    const inUse = (p) => names.some((n) => p === "/" + n || p?.startsWith("/" + n + "/"));
    if (transfers.all().some((j) => !transfers.finished(j) && [...(j.info.sources || []), j.info.dest].some(inUse))) {
      return toast(t("Ci sono trasferimenti in corso su «{s}»: aspetta che finiscano o annullali", { s: shareName }), { kind: "error" });
    }
    const title = share.device_name || shareName;
    const ok = await confirmBox({
      title: t("Espellere «{d}»?", { d: title }),
      message: t("Le cartelle condivise del dispositivo non saranno disponibili finché non lo ricolleghi."),
      items: names.map((n) => `<i class="gi">${shareGlyph(n)}</i>${esc(n)}`),
      ok: t("Espelli"),
      icon: "eject",
    });
    if (!ok) return;
    try {
      await api.eject(shareName);
    } catch (e) {
      return toast(t("Impossibile espellere «{d}» ({e}): forse è ancora in uso", { d: title, e: e.message }), { kind: "error", timeout: 9000 });
    }
    toast(t("«{d}» espulso: si può scollegare", { d: title }), { kind: "ok" });
    for (const p of this.allPanes()) if (inUse(p.path)) p.go("/");
    await this.loadShares();
  },

  toggleFavorite(path = this.pane.path) {
    if (path === "/") return;
    let favs = [...(prefs.favorites || [])];
    if (favs.some((f) => f.path === path)) {
      favs = favs.filter((f) => f.path !== path);
      toast(t("Tolta dai preferiti"), { timeout: 1500 });
    } else {
      favs.push({ path, name: this.pathName(path) });
      toast(t("Aggiunta ai preferiti"), { kind: "ok", timeout: 1500 });
    }
    save({ favorites: favs });
  },
};
