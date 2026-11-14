// Remote connections: SMB and SFTP servers in the sidebar, under names of
// the user's choice. A connection's folders are the paths "/:<id>/…".

import { api } from "./api.js";
import { t } from "./i18n.js";
import { esc } from "./format.js";
import { svg } from "./icons.js";
import { dialog, copyText, toast } from "./ui.js";

export const isRemote = (path) => typeof path === "string" && path.startsWith("/:");

/** The connection id of a remote path, or "". */
export const connId = (path) => (isRemote(path) ? path.slice(2).split("/")[0] : "");

/**
 * A server address as people write it: \\nas\share\folder, //nas/share,
 * smb://user@nas/share, sftp://user@host:2222/path, or a bare host.
 */
export function parseAddress(text) {
  let v = (text || "").trim();
  if (!v) return null;
  let kind = null;
  const scheme = v.match(/^(smb|cifs|sftp|ssh):\/\//i);
  if (scheme) {
    kind = /^s(ftp|sh)$/i.test(scheme[1]) ? "sftp" : "smb";
    v = v.slice(scheme[0].length);
  } else if (/^(\\\\|\/\/)/.test(v)) {
    kind = "smb";
  }
  v = v.replace(/\\/g, "/").replace(/^\/+/, "");
  const slash = v.indexOf("/");
  let auth = slash < 0 ? v : v.slice(0, slash);
  const path = slash < 0 ? "" : v.slice(slash + 1).replace(/\/+$/, "");
  let user = "";
  let domain = "";
  const at = auth.lastIndexOf("@");
  if (at >= 0) {
    user = decodeURIComponent(auth.slice(0, at));
    auth = auth.slice(at + 1);
    // smb://DOMAIN;user@host
    const semi = user.indexOf(";");
    if (semi >= 0) [domain, user] = [user.slice(0, semi), user.slice(semi + 1)];
  }
  let host = auth;
  let port = 0;
  const m = auth.match(/^(.*):(\d+)$/);
  if (m) [host, port] = [m[1], +m[2]];
  if (!host) return null;
  return { kind, host, port, user, domain, path };
}

/** How a remote path is written for the server's own clients. */
export function serverAddress(conn, path, style) {
  const rest = path.split("/").filter(Boolean).slice(1);
  const port = conn.port ? `:${conn.port}` : "";
  if (conn.kind === "sftp") return `sftp://${conn.user ? encodeURIComponent(conn.user) + "@" : ""}${conn.host}${port}/${rest.map(encodeURIComponent).join("/")}`;
  if (style === "url") return `smb://${conn.host}${port}/${rest.map(encodeURIComponent).join("/")}`;
  return `\\\\${conn.host}${rest.length ? "\\" + rest.join("\\") : ""}`;
}

/** Where a connection opens. */
export function startPath(conn) {
  return "/:" + conn.id + (conn.path ? "/" + conn.path.split("/").filter(Boolean).join("/") : "");
}

const fieldsHTML = (c) => `
  <div class="conn-kind seg">
    <button type="button" data-kind="smb" class="${c.kind !== "sftp" ? "on" : ""}">SMB</button>
    <button type="button" data-kind="sftp" class="${c.kind === "sftp" ? "on" : ""}">SFTP</button>
  </div>
  <label class="field"><span>${t("files.conns.server_address")}</span><input type="text" name="host" value="${esc(c.host || "")}" placeholder="${t("files.conns.e_g_nas_nas_folder")}" spellcheck="false" autocomplete="off"></label>
  <div class="conn-row">
    <label class="field"><span>${t("files.common.name_in_sidebar")}</span><input type="text" name="name" value="${esc(c.name || "")}" spellcheck="false" autocomplete="off"></label>
    <label class="field conn-port"><span>${t("files.conns.port")}</span><input type="number" name="port" min="0" max="65535" value="${c.port || ""}" placeholder="${c.kind === "sftp" ? 22 : 445}"></label>
  </div>
  <label class="field smb-only"><span>${t("files.conns.domain")}</span><input type="text" name="domain" value="${esc(c.domain || "")}" placeholder="${t("files.conns.optional_e_g_company")}" spellcheck="false" autocomplete="off"></label>
  <label class="field"><span>${t("files.conns.user")}</span><input type="text" name="user" value="${esc(c.user || "")}" spellcheck="false" autocomplete="off"></label>
  <div class="field sftp-only"><span>${t("files.conns.login")}</span>
    <div class="seg conn-auth">
      <button type="button" data-auth="password" class="${c.auth !== "key" ? "on" : ""}">${t("files.common.password")}</button>
      <button type="button" data-auth="key" class="${c.auth === "key" ? "on" : ""}">${t("files.conns.rynas_key")}</button>
      <button type="button" data-auth="own" class="${c.auth === "own" ? "on" : ""}">${t("files.conns.my_key")}</button>
    </div>
  </div>
  <label class="field pw"><span>${t("files.common.password")}</span><input type="password" name="password" value="" placeholder="${c.has_password ? t("files.conns.saved_leave_empty_keep") : ""}" autocomplete="new-password"></label>
  <div class="field key-box"><span>${t("files.conns.public_key_add_ssh_authorized")}</span>
    <div class="conn-key"><code class="kv"></code><button type="button" class="btn small kcopy">${t("files.common.copy")}</button><button type="button" class="btn small kmake">${t("files.conns.create_key")}</button></div>
  </div>
  <div class="field own-box"><span>${t("files.conns.private_key_openssh_pem")}</span>
    ${c.key_info ? `<div class="hint conn-keyinfo">${t("files.conns.loaded_k", { k: esc(c.key_info) })}</div>` : ""}
    <textarea name="private_key" rows="3" spellcheck="false" autocomplete="off" placeholder="${c.key_info ? t("files.conns.empty_keep_one_loaded") : "-----BEGIN OPENSSH PRIVATE KEY-----"}"></textarea>
    <div class="conn-key"><button type="button" class="btn small kfile">${t("files.conns.load_file")}</button><input type="file" class="kpick" hidden>
      <input type="password" name="passphrase" placeholder="${t("files.conns.passphrase_key_one")}" autocomplete="new-password"></div>
  </div>
  <label class="field"><span>${t("files.conns.starting_folder")}</span><input type="text" name="path" value="${esc(c.path && c.kind === "sftp" ? "/" + c.path : c.path || "")}" placeholder="${c.kind === "sftp" ? t("files.conns.empty_user_home") : t("files.conns.empty_list_shared_folders")}" spellcheck="false" autocomplete="off"></label>
  ${c.host_key ? `<div class="hint conn-hostkey">${t("files.conns.server_key_k", { k: esc(c.host_key) })} <label><input type="checkbox" name="forget_key"> ${t("files.conns.forget_changed")}</label></div>` : ""}
  <div class="hint conn-result"></div>`;

/**
 * Adds or edits a connection. `conn`: the saved one, or a draft from an
 * address typed in the path bar. Resolves to the saved connection, or null.
 */
export function editConnection(conn = {}, key = null) {
  return new Promise((resolve) => {
    const isNew = !conn.id;
    let kind = conn.kind || "smb";
    let auth = conn.auth || "password";
    let publicKey = key;
    const dlg = dialog(
      `<form method="dialog" class="conn-form">
        <div class="dlg-head"><div class="dlg-icon">${svg("server")}</div><h2>${isNew ? t("files.common.new_connection") : t("files.conns.edit_connection")}</h2></div>
        <div class="dlg-body">${fieldsHTML({ ...conn, kind, auth })}</div>
        <div class="dlg-foot"><button type="button" class="btn test">${t("files.conns.test")}</button><span style="flex:1"></span><button type="button" class="btn cancel">${t("files.common.cancel")}</button><button class="btn primary">${t("files.common.save")}</button></div>
      </form>`,
      { cls: "conn-dlg" },
    );
    const f = (n) => dlg.querySelector(`[name="${n}"]`);
    const result = dlg.querySelector(".conn-result");
    let saved = null;
    let nameTouched = !isNew;
    f("name").addEventListener("input", () => (nameTouched = true));

    const refresh = () => {
      dlg.querySelectorAll(".conn-kind button").forEach((b) => b.classList.toggle("on", b.dataset.kind === kind));
      dlg.querySelectorAll(".conn-auth button").forEach((b) => b.classList.toggle("on", b.dataset.auth === auth));
      dlg.querySelectorAll(".smb-only").forEach((x) => (x.hidden = kind !== "smb"));
      dlg.querySelectorAll(".sftp-only").forEach((x) => (x.hidden = kind !== "sftp"));
      const byKey = kind === "sftp" && auth === "key";
      const own = kind === "sftp" && auth === "own";
      dlg.querySelector(".pw").hidden = byKey || own;
      dlg.querySelector(".key-box").hidden = !byKey;
      dlg.querySelector(".own-box").hidden = !own;
      dlg.querySelector(".kv").textContent = publicKey || t("files.conns.no_key_yet_create_copy");
      dlg.querySelector(".kcopy").hidden = !publicKey;
      dlg.querySelector(".kmake").hidden = !!publicKey;
      f("port").placeholder = kind === "sftp" ? "22" : "445";
      f("path").placeholder = kind === "sftp" ? t("files.conns.empty_user_home") : t("files.conns.empty_list_shared_folders");
    };
    refresh();

    dlg.querySelectorAll(".conn-kind button").forEach((b) => (b.onclick = () => ((kind = b.dataset.kind), refresh())));
    dlg.querySelectorAll(".conn-auth button").forEach((b) => (b.onclick = () => ((auth = b.dataset.auth), refresh())));
    dlg.querySelector(".kcopy").onclick = () => copyText(publicKey);
    const pick = dlg.querySelector(".kpick");
    dlg.querySelector(".kfile").onclick = () => pick.click();
    pick.onchange = async () => {
      const file = pick.files[0];
      if (!file) return;
      if (file.size > 64 * 1024) return (result.textContent = t("files.conns.not_private_key_file_too"));
      f("private_key").value = (await file.text()).trim();
      pick.value = "";
    };
    dlg.querySelector(".kmake").onclick = async () => {
      try {
        publicKey = (await api.makeKey()).key;
        refresh();
      } catch (e) {
        result.textContent = e.message;
      }
    };

    // an address pasted whole fills the other fields
    const host = f("host");
    const spread = () => {
      const a = parseAddress(host.value);
      if (!a) return;
      if (a.kind) kind = a.kind;
      host.value = a.host;
      if (a.port) f("port").value = a.port;
      if (a.user) f("user").value = a.user;
      if (a.domain) f("domain").value = a.domain;
      if (a.path) f("path").value = a.path;
      if (!nameTouched || !f("name").value) f("name").value = a.path && kind === "smb" ? a.path.split("/").pop() : a.host;
      refresh();
    };
    host.addEventListener("change", spread);
    host.addEventListener("paste", () => setTimeout(spread));
    if (isNew && conn.host) spread();
    (isNew ? host : f("name")).focus();

    const payload = () => ({
      id: conn.id || "",
      name: f("name").value.trim() || host.value.trim(),
      kind,
      host: host.value.trim(),
      port: +f("port").value || 0,
      domain: kind === "smb" ? f("domain").value.trim() : "",
      user: f("user").value.trim(),
      auth: kind === "sftp" ? auth : "password",
      path: f("path").value.trim(),
      password: auth === "own" && kind === "sftp" ? "" : f("password").value,
      private_key: kind === "sftp" && auth === "own" ? f("private_key").value : "",
      passphrase: kind === "sftp" && auth === "own" ? f("passphrase").value : "",
      forget_key: !!f("forget_key")?.checked,
    });
    const busy = (on) => dlg.querySelectorAll(".dlg-foot .btn").forEach((b) => (b.disabled = on));

    dlg.querySelector(".test").onclick = async () => {
      result.style.color = "";
      result.textContent = t("files.conns.connecting");
      busy(true);
      try {
        const r = await api.testConn(payload());
        result.style.color = "var(--ok)";
        result.textContent = t("files.conns.connected_in_ms_ms_n", { ms: r.ms, n: r.items, p: "/" + r.path });
      } catch (e) {
        result.style.color = "var(--crit)";
        result.textContent = e.message;
      } finally {
        busy(false);
      }
    };
    dlg.querySelector(".cancel").onclick = () => dlg.close();
    dlg.querySelector("form").onsubmit = async (e) => {
      e.preventDefault();
      busy(true);
      try {
        saved = (await api.saveConn(payload())).conn;
        dlg.close();
      } catch (err) {
        result.style.color = "var(--crit)";
        result.textContent = err.message;
      } finally {
        busy(false);
      }
    };
    dlg.addEventListener("close", () => resolve(saved));
  });
}

/** A connection under a new name only. */
export async function renameConnection(conn, name) {
  const { conn: saved } = await api.saveConn({ ...conn, name, password: "" });
  toast(t("files.conns.connection_renamed"), { kind: "ok", timeout: 1500 });
  return saved;
}
