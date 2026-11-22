// Renaming many items at once (F2 on a selection): a name made of pieces
// ([N] the old name, [C] a counter, [D] the date…), a find and replace, a
// case, with every new name shown before anything changes.

import { api } from "./api.js";
import { t, count, errText } from "./i18n.js";
import { esc, int, baseName, parentOf, joinPath, isDirLike } from "./format.js";
import { svg } from "./icons.js";
import { dialog, toast, errorToast } from "./ui.js";
import { isRemote } from "./conns.js";
import { moveFolderIcons } from "./iconpick.js";
import { changed as appChanged } from "./appdata.js";
import { recordRename } from "./undo.js";

const DEFAULTS = { name: "[N]", ext: "[E]", find: "", repl: "", regex: false, matchCase: false, kase: "keep", start: 1, step: 1, digits: 2 };
// the options last used, offered again
let last = { ...DEFAULTS };
// rows of the preview drawn at most
const PREVIEW = 500;
// items one folder renames at once at most (the NAS's own limit, fsops::MAX_RENAMES)
const MAX_BATCH = 10000;

const pad = (n, d) => String(n).padStart(d, "0");
const bytes = (s) => new TextEncoder().encode(s).length;

/**
 * `name` as HTML with what differs from `other` marked: the common start and end plain, the
 * part in between in `<del>` (the old name) or `<ins>` (the new one), or a small `.gap` mark
 * where the other name has text this one has not. Everything escaped: names are user data.
 */
export function marked(name, other, tag) {
  if (name === other) return esc(name);
  const [a, b] = [[...name], [...other]];
  let pre = 0;
  while (pre < a.length && pre < b.length && a[pre] === b[pre]) pre++;
  let suf = 0;
  while (suf < a.length - pre && suf < b.length - pre && a[a.length - 1 - suf] === b[b.length - 1 - suf]) suf++;
  const mid = a.slice(pre, a.length - suf).join("");
  const part = mid ? `<${tag}>${esc(mid)}</${tag}>` : `<span class="gap" aria-hidden="true"></span>`;
  return esc(a.slice(0, pre).join("")) + part + esc(a.slice(a.length - suf).join(""));
}

/** [stem, extension] of an item's name; a folder or a dot file has no extension. */
function splitName(e) {
  const i = e.name.lastIndexOf(".");
  if (isDirLike(e) || i <= 0) return [e.name, ""];
  return [e.name.slice(0, i), e.name.slice(i + 1)];
}

/** Fills a mask: [N] name, [E] extension, [C] counter, [D] date, [T] time, [P] folder; [N2-5] characters 2 to 5. */
function fill(mask, e, i, o) {
  const [stem, ext] = splitName(e);
  const d = new Date((e.mtime || 0) * 1000);
  return mask.replace(/\[(N|E|C|D|T|P)(\d+)?(?:-(\d+))?\]/gi, (all, k, a, b) => {
    k = k.toUpperCase();
    let v;
    if (k === "N") v = stem;
    else if (k === "E") v = ext;
    else if (k === "C") v = pad(o.start + i * o.step, o.digits);
    else if (k === "D") v = `${d.getFullYear()}-${pad(d.getMonth() + 1, 2)}-${pad(d.getDate(), 2)}`;
    else if (k === "T") v = `${pad(d.getHours(), 2)}.${pad(d.getMinutes(), 2)}.${pad(d.getSeconds(), 2)}`;
    else v = baseName(parentOf(e.path));
    if (a && (k === "N" || k === "E" || k === "P")) v = [...v].slice(Math.max(0, +a - 1), b ? +b : +a).join("");
    return v;
  });
}

function changeCase(s, kase) {
  if (kase === "lower") return s.toLowerCase();
  if (kase === "upper") return s.toUpperCase();
  if (kase === "title") return s.toLowerCase().replace(/(^|[\s._\-([])(\p{L})/gu, (m, sep, c) => sep + c.toUpperCase());
  return s;
}

/** Each item's new name with the options `o`; throws on a broken regular expression. */
export function newNames(list, o) {
  let re = null;
  if (o.find) re = new RegExp(o.regex ? o.find : o.find.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), o.matchCase ? "g" : "gi");
  return list.map((e, i) => {
    const stem = fill(o.name, e, i, o);
    const ext = isDirLike(e) ? "" : fill(o.ext, e, i, o);
    let name = ext ? `${stem}.${ext}` : stem;
    // plain text is put in as it is: "$&" and "$1" mean something in a regular expression only
    if (re) name = o.regex ? name.replace(re, o.repl) : name.replace(re, () => o.repl);
    // Title Case is for the name, never the extension (".Txt"); lower and upper case both
    const dot = name.lastIndexOf(".");
    if (o.kase === "title" && !isDirLike(e) && dot > 0) return (changeCase(name.slice(0, dot), o.kase) + name.slice(dot)).trim();
    return changeCase(name, o.kase).trim();
  });
}

/**
 * Renames [old path, new name] pairs: on the NAS one request per folder,
 * swaps and shifts included; on a remote server one by one. Resolves the
 * pairs done and the errors ("name: reason").
 */
export async function renameMany(pairs, { shallowFirst = false } = {}) {
  const done = [];
  const errors = [];
  const recovered = [];
  const byDir = new Map();
  for (const [path, name] of pairs) {
    const dir = parentOf(path);
    if (!byDir.has(dir)) byDir.set(dir, []);
    byDir.get(dir).push([path, name]);
  }
  // the deepest folders first, so a folder renamed with things inside it does not move them
  // away first; an undo (shallowFirst) the other way round, a folder takes its name back first
  const depth = (p) => p.split("/").length;
  const dirs = [...byDir].sort(([a], [b]) => (shallowFirst ? depth(a) - depth(b) : depth(b) - depth(a)));
  for (const [dir, list] of dirs) {
    if (isRemote(dir)) {
      for (const [path, name] of list) {
        try {
          await api.rename(path, name);
          done.push([path, name]);
        } catch (err) {
          errors.push(`${baseName(path)}: ${err.message}`);
        }
      }
      continue;
    }
    try {
      let answer = null;
      await api.renames(dir, list.map(([p, n]) => [baseName(p), n]), (line) => {
        if (line.results) answer = line;
      });
      if (!answer) throw new Error(t("files.pane.incomplete_answer"));
      for (const r of answer.results) {
        // a failed item that could not go back under its own name says where it is
        const where = r.kept_as ? ` (${t("files.rename.kept_as_n", { n: r.kept_as })})` : "";
        if (r.error) errors.push(`${r.from}: ${errText(r.code) || r.error}${where}`);
        else done.push([joinPath(dir, r.from), r.to]);
      }
      // items an earlier interrupted batch had left hidden, put back
      for (const r of answer.recovered || []) if (r.name && r.name !== r.parked) recovered.push(r.name);
    } catch (err) {
      errors.push(err.message);
    }
  }
  for (const [path, name] of done) {
    moveFolderIcons(path, joinPath(parentOf(path), name));
    appChanged(path);
  }
  if (recovered.length) toast(t("files.rename.recovered_n", { n: recovered.slice(0, 3).join(", ") + (recovered.length > 3 ? "…" : "") }), { timeout: 8000 });
  return { done, errors };
}

/** The batch rename dialog for `list`, items of `pane`. */
export function batchRename(app, pane, list) {
  const tokens = [
    ["[N]", "files.rename.tok_name"], ["[N1-3]", "files.rename.tok_chars"], ["[E]", "files.rename.tok_ext"], ["[C]", "files.rename.tok_counter"],
    ["[D]", "files.rename.tok_date"], ["[T]", "files.rename.tok_time"], ["[P]", "files.rename.tok_folder"],
  ];
  const dlg = dialog(
    `
    <form method="dialog">
      <div class="dlg-head"><div class="dlg-icon">${svg("rename")}</div><h2>${esc(t("files.rename.rename_n", { n: count(list.length, "files.common.count.items", int) }))}</h2></div>
      <div class="dlg-body ren">
        <div class="ren-grid">
          <label class="field"><span>${esc(t("files.common.name"))}</span><input type="text" class="r-name" spellcheck="false" autocomplete="off"></label>
          <label class="field"><span>${esc(t("files.rename.extension"))}</span><input type="text" class="r-ext" spellcheck="false" autocomplete="off"></label>
        </div>
        <div class="ren-tokens">${tokens.map(([k, l]) => `<button type="button" class="chip-btn" data-k="${k}" title="${esc(t(l))}" aria-label="${esc(`${k} ${t(l)}`)}">${k}</button>`).join("")}</div>
        <div class="ren-grid">
          <label class="field"><span>${esc(t("files.rename.find"))}</span><input type="text" class="r-find" spellcheck="false" autocomplete="off"></label>
          <label class="field"><span>${esc(t("files.rename.replace_with"))}</span><input type="text" class="r-repl" spellcheck="false" autocomplete="off"></label>
        </div>
        <div class="ren-row">
          <label class="check"><input type="checkbox" class="r-regex"> ${esc(t("files.select.regex"))}</label>
          <label class="check"><input type="checkbox" class="r-mcase"> ${esc(t("files.rename.match_case"))}</label>
        </div>
        <div class="ren-row">
          <label class="field inline"><span>${esc(t("files.rename.case"))}</span><select class="input r-case">
            <option value="keep">${esc(t("files.rename.case_keep"))}</option><option value="lower">${esc(t("files.rename.case_lower"))}</option><option value="upper">${esc(t("files.rename.case_upper"))}</option><option value="title">${esc(t("files.rename.case_title"))}</option></select></label>
          <label class="field inline"><span>${esc(t("files.rename.counter_from"))}</span><input type="number" class="input r-start" min="0"></label>
          <label class="field inline"><span>${esc(t("files.rename.step"))}</span><input type="number" class="input r-step" min="1"></label>
          <label class="field inline"><span>${esc(t("files.rename.digits"))}</span><input type="number" class="input r-digits" min="1" max="9"></label>
        </div>
        <div class="ren-head"><span>${esc(t("files.rename.before"))}</span><span></span><span>${esc(t("files.rename.after"))}</span></div>
        <div class="ren-list"></div>
        <div class="hint ren-sum" role="status"></div>
      </div>
      <div class="dlg-foot"><button type="button" class="btn r-reset">${esc(t("files.rename.reset"))}</button><span style="flex:1"></span><button type="button" class="btn cancel">${esc(t("files.common.cancel"))}</button><button class="btn primary">${esc(t("files.common.rename"))}</button></div>
    </form>`,
    { wide: true, cls: "rename-dlg" },
  );
  const q = (s) => dlg.querySelector(s);
  const fields = {
    name: q(".r-name"), ext: q(".r-ext"), find: q(".r-find"), repl: q(".r-repl"), regex: q(".r-regex"), matchCase: q(".r-mcase"),
    kase: q(".r-case"), start: q(".r-start"), step: q(".r-step"), digits: q(".r-digits"),
  };
  const put = (o) => {
    for (const [k, el] of Object.entries(fields)) {
      if (el.type === "checkbox") el.checked = !!o[k];
      else el.value = o[k];
    }
  };
  const read = () => {
    const o = {};
    for (const [k, el] of Object.entries(fields)) o[k] = el.type === "checkbox" ? el.checked : el.type === "number" ? Math.max(0, Math.floor(+el.value) || 0) : el.value;
    o.step = o.step || 1;
    o.digits = Math.min(9, Math.max(1, o.digits || 1));
    return o;
  };
  put(last);
  // the token chips write into the name or the extension field last focused
  let focused = fields.name;
  for (const el of [fields.name, fields.ext]) el.addEventListener("focus", () => (focused = el));
  const listEl = q(".ren-list");
  const sum = q(".ren-sum");
  const ok = q(".btn.primary");
  let plan = [];
  const update = () => {
    let names;
    sum.classList.remove("err");
    try {
      names = newNames(list, read());
    } catch {
      sum.textContent = t("files.select.invalid_regex");
      sum.classList.add("err");
      listEl.innerHTML = "";
      ok.disabled = true;
      return;
    }
    // what a new name may clash with: another new name, or a name that stays in its folder
    const olds = new Set(list.map((e) => e.path));
    const staying = new Set(pane.visible.filter((e) => !olds.has(e.path)).map((e) => e.path));
    const tally = new Map();
    names.forEach((n, i) => {
      const k = joinPath(parentOf(list[i].path), n);
      tally.set(k, (tally.get(k) || 0) + 1);
    });
    plan = list.map((e, i) => {
      const n = names[i];
      const k = joinPath(parentOf(e.path), n);
      let problem = "";
      if (!n || n === "." || n === ".." || n.includes("/") || n.includes("\0")) problem = t("files.rename.invalid_name");
      else if (bytes(n) > 255) problem = t("files.rename.too_long");
      else if (tally.get(k) > 1) problem = t("files.rename.duplicate");
      else if (n !== e.name && staying.has(k)) problem = t("files.rename.exists");
      // a remote server renames one by one: a name another item still has is taken
      else if (isRemote(e.path) && n !== e.name && olds.has(k)) problem = t("files.rename.remote_swap");
      return { e, name: n, problem, same: n === e.name };
    });
    const shown = plan.slice(0, PREVIEW);
    // items of several folders (opened in place): each says which folder it is in
    const spread = new Set(list.map((e) => parentOf(e.path))).size > 1;
    const relDir = (e) => (parentOf(e.path) === pane.path ? "." : parentOf(e.path).slice(pane.path === "/" ? 1 : pane.path.length + 1));
    const where = (e) => (spread ? `<span class="ren-dir">${esc(relDir(e))}/</span>` : "");
    listEl.innerHTML =
      shown
        .map(
          (p) =>
            `<div class="ren-item ${p.problem ? "bad" : p.same ? "same" : ""}"><span title="${esc(p.e.path)}">${where(p.e)}${marked(p.e.name, p.name, "del")}</span><span>→</span><span title="${esc(p.problem ? `${p.name} · ${p.problem}` : p.name)}">${marked(p.name, p.e.name, "ins")}${p.problem ? ` <em>${esc(p.problem)}</em>` : ""}</span></div>`,
        )
        .join("") +
      (plan.length > shown.length ? `<div class="ren-item same"><span>… ${esc(t("files.common.n_more", { n: int(plan.length - shown.length) }))}</span></div>` : "");
    // one folder renames up to MAX_BATCH items at once
    const perDir = new Map();
    for (const e of list) perDir.set(parentOf(e.path), (perDir.get(parentOf(e.path)) || 0) + 1);
    if ([...perDir.values()].some((n) => n > MAX_BATCH)) {
      sum.textContent = t("files.rename.too_many_n", { n: int(MAX_BATCH) });
      sum.classList.add("err");
      ok.disabled = true;
      return;
    }
    const bad = plan.filter((p) => p.problem).length;
    const changing = plan.filter((p) => !p.same && !p.problem).length;
    sum.textContent = bad ? t("files.rename.n_to_fix", { n: int(bad) }) : changing ? count(changing, "files.rename.count.changes", int) : t("files.rename.nothing_changes");
    if (bad) sum.classList.add("err");
    ok.disabled = !!bad || !changing;
  };
  dlg.querySelectorAll(".ren-tokens button").forEach(
    (b) =>
      (b.onclick = () => {
        const el = focused;
        const [a, z] = [el.selectionStart ?? el.value.length, el.selectionEnd ?? el.value.length];
        el.value = el.value.slice(0, a) + b.dataset.k + el.value.slice(z);
        el.focus();
        el.setSelectionRange(a + b.dataset.k.length, a + b.dataset.k.length);
        update();
      }),
  );
  dlg.querySelectorAll("input, select").forEach((el) => el.addEventListener("input", update));
  q(".r-reset").onclick = () => {
    put(DEFAULTS);
    update();
  };
  q(".cancel").onclick = () => dlg.close();
  update();
  fields.name.focus();
  fields.name.select();
  q("form").onsubmit = async (ev) => {
    ev.preventDefault();
    if (ok.disabled) return;
    last = read();
    ok.disabled = true;
    const pairs = plan.filter((p) => !p.same).map((p) => [p.e.path, p.name]);
    const { done, errors } = await renameMany(pairs);
    dlg.close();
    if (done.length) recordRename(done, t("files.rename.rename_n", { n: count(done.length, "files.common.count.items", int) }));
    app.renamedPaths(done);
    app.refreshFolders([...new Set(pairs.map(([p]) => parentOf(p)))]);
    if (pane.search) pane.reload();
    if (errors.length) errorToast(new Error(t("files.rename.n_not_renamed", { n: int(errors.length), e: errors.slice(0, 3).join("; ") + (errors.length > 3 ? "…" : "") })));
    else toast(count(done.length, "files.rename.count.renamed", int));
  };
}
