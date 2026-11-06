// The page and the server: banners (session, files service), the "own
// window" button, the strings of the chrome. Methods of App (main.js).

import { t } from "./i18n.js";
import { esc } from "./format.js";
import { svg } from "./icons.js";
import { $, h, toast } from "./ui.js";

export const healthMethods = {
  /**
   * A button that opens the file manager in a browser window of its own
   * (the /files page without the shell's bars), at the current folder.
   */
  popOutButton() {
    if ($("#pop-out") || new URLSearchParams(location.search).has("window")) return;
    const b = h(`<button class="icon-btn" id="pop-out"></button>`);
    b.innerHTML = svg("popout");
    b.title = t("Apri in una finestra separata");
    b.setAttribute("aria-label", b.title);
    b.onclick = () => {
      const w = Math.min(screen.availWidth, 1400);
      const ht = Math.min(screen.availHeight, 900);
      const path = this.pane?.path && this.pane.path !== "/" ? this.pane.path.split("/").map(encodeURIComponent).join("/") : "";
      const win = window.open(`/files${path}?window`, "rynas-files", `popup,width=${w},height=${ht}`);
      if (!win) toast(t("Il browser ha bloccato la finestra: consenti le finestre popup per questo sito"), { kind: "error" });
      else win.focus();
    };
    $("#open-settings")?.before(b);
  },

  checkHealth(me) {
    this.user = me.user || this.user;
    this.version = me.version;
    // `daemon` false: the transfer service is down (copies, moves, deletions wait)
    if (me.daemon === false) this.banner("daemon", t("Il servizio dei trasferimenti non è attivo: copie, spostamenti ed eliminazioni non possono partire."));
  },

  adminOnly() {
    this.banner("admin", t("Non hai accesso al file manager: chiedi a un amministratore."));
  },

  /** The files API does not answer (not installed yet, or down): a banner instead of empty panes. */
  unavailable(e) {
    const why = e?.status === 404 ? t("Il servizio dei file non è installato su questo sistema.") : e?.message || "";
    this.banner("unavailable", `<span>${esc(t("Il file manager non riesce a parlare con il server."))} ${esc(why)}</span> <button class="btn small retry">${esc(t("Riprova"))}</button>`, true);
    const retry = $('#banners [data-key="unavailable"] .retry');
    if (retry) retry.onclick = () => this.boot();
  },

  /**
   * The rynas session ended: the shell shows its login screen over the
   * page. Here only a note, in case the login is dismissed or delayed.
   */
  needLogin() {
    this.banner("login", t("Sessione scaduta: accedi di nuovo per continuare."));
  },

  /** The session came back (signed in again): everything is read again. */
  sessionBack() {
    this.clearBanner("login");
    if (!this.started) return void this.boot();
    this.loadShares?.();
    for (const p of this.allPanes?.() || []) if (this.current?.panes.includes(p)) p.reload();
  },

  /** A message over the panes, one for each `key`: shown again, it replaces the one there. */
  banner(key, html, raw = false) {
    let box = $("#banners");
    if (!box) {
      box = h(`<div class="banners" id="banners" role="status"></div>`);
      $(".body").prepend(box);
    }
    const b = h(`<div class="banner" data-key="${esc(key)}">${svg("alert")}<div>${raw ? html : esc(html)}</div></div>`);
    const old = box.querySelector(`[data-key="${CSS.escape(key)}"]`);
    if (old) old.replaceWith(b);
    else box.append(b);
  },

  clearBanner(key) {
    $(`#banners [data-key="${CSS.escape(key)}"]`)?.remove();
  },

  /** The strings of the chrome (the markup of rynas-files.js), in the interface's language. */
  translateChrome() {
    const set = (sel, fn) => {
      const el = $(sel);
      if (el) fn(el);
    };
    const label = (el, text) => {
      el.title = text;
      el.setAttribute("aria-label", text);
    };
    set("#tab-new", (el) => label(el, `${t("Nuova scheda")} (Ctrl+T)`));
    set("#open-settings", (el) => label(el, `${t("Impostazioni")} (Ctrl+,)`));
    set("#pop-out", (el) => label(el, t("Apri in una finestra separata")));
    set("#xfer-btn", (el) => label(el, t("Trasferimenti")));
    set("#sidebar", (el) => el.setAttribute("aria-label", t("Posizioni")));
    set("#tabs", (el) => el.setAttribute("aria-label", t("Schede")));
    set(".drawer-head > span", (el) => (el.textContent = t("Trasferimenti")));
    set("#xfer-clear", (el) => (el.textContent = t("Pulisci completati")));
    set("#xfer-drawer-close", (el) => label(el, t("Chiudi")));
    set("#drop-overlay > div", (el) => {
      const target = $("#drop-target") || h(`<b id="drop-target"></b>`);
      el.textContent = t("Rilascia per caricare in") + " ";
      el.append(target);
    });
  },
};
