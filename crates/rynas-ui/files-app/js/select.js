// Selecting by a pattern (+ and -), and the differences between the two
// panes (Ctrl+Shift+D).

import { t, count } from "./i18n.js";
import { int, isDirLike, esc, parentOf } from "./format.js";
import { svg } from "./icons.js";
import { dialog, toast } from "./ui.js";

// the last pattern and options, offered again next time
let last = { text: "", regex: false, files: false };

/**
 * A test for names: wildcards (`*` any text, `?` one character, `[abc]` one
 * of these, `[!abc]` none of these), several separated by `;`, or one
 * regular expression; case is ignored. Null when the text is empty; throws
 * on a broken regular expression.
 */
export function matcher(text, regex) {
  text = text.trim();
  if (!text) return null;
  if (regex) {
    const re = new RegExp(text, "i");
    return (name) => re.test(name);
  }
  const parts = text
    .split(";")
    .map((p) => p.trim())
    .filter(Boolean)
    .map((p) => {
      let src = "";
      for (let i = 0; i < p.length; i++) {
        const c = p[i];
        if (c === "*") src += ".*";
        else if (c === "?") src += ".";
        else if (c === "[") {
          // a "]" right after "[" (or "[!") belongs to the set
          const end = p.indexOf("]", i + (p[i + 1] === "!" ? 3 : 2));
          if (end < 0) src += "\\[";
          else {
            let body = p.slice(i + 1, end);
            const negate = body[0] === "!";
            if (negate) body = body.slice(1);
            src += "[" + (negate ? "^" : "") + body.replace(/[\\\]^]/g, "\\$&") + "]";
            i = end;
          }
        } else src += c.replace(/[.+^${}()|\\\]]/g, "\\$&");
      }
      return new RegExp(`^${src}$`, "is");
    });
  return parts.length ? (name) => parts.some((re) => re.test(name)) : null;
}

/** The pane's items a test picks (files only, when asked). */
const hits = (pane, test, filesOnly) => pane.visible.filter((e) => (!filesOnly || !isDirLike(e)) && test(e.name));

/** "add" the matches to the selection, "remove" them from it, or select "only" them. */
export function selectByPattern(pane, mode = "add") {
  const dlg = dialog(
    `
    <form method="dialog">
      <div class="dlg-head"><div class="dlg-icon">${svg("filter")}</div><h2>${esc(mode === "remove" ? t("files.select.deselect_pattern") : t("files.select.select_pattern"))}</h2></div>
      <div class="dlg-body">
        <label class="field"><span>${esc(t("files.select.pattern"))}</span><input type="text" class="pat" value="${esc(last.text)}" placeholder="*.mkv; *.srt" spellcheck="false" autocomplete="off"></label>
        <div class="hint">${esc(t("files.select.pattern_hint"))}</div>
        <label class="check"><input type="checkbox" class="re" ${last.regex ? "checked" : ""}> ${esc(t("files.select.regex"))}</label>
        <label class="check"><input type="checkbox" class="files" ${last.files ? "checked" : ""}> ${esc(t("files.select.files_only"))}</label>
        <div class="seg sel-mode" role="radiogroup">
          <button type="button" role="radio" data-m="add">${esc(t("files.select.add"))}</button><button type="button" role="radio" data-m="only">${esc(t("files.select.only_these"))}</button><button type="button" role="radio" data-m="remove">${esc(t("files.select.remove"))}</button>
        </div>
        <div class="hint found" role="status"></div>
      </div>
      <div class="dlg-foot"><button type="button" class="btn cancel">${esc(t("files.common.cancel"))}</button><button class="btn primary"></button></div>
    </form>`,
    { cls: "select-dlg" },
  );
  const pat = dlg.querySelector(".pat");
  const re = dlg.querySelector(".re");
  const files = dlg.querySelector(".files");
  const found = dlg.querySelector(".found");
  const ok = dlg.querySelector(".btn.primary");
  const setMode = (m) => {
    mode = m;
    dlg.querySelectorAll(".sel-mode button").forEach((b) => {
      b.classList.toggle("on", b.dataset.m === m);
      b.setAttribute("aria-checked", String(b.dataset.m === m));
    });
    ok.textContent = m === "remove" ? t("files.select.deselect") : t("files.select.select");
  };
  dlg.querySelectorAll(".sel-mode button").forEach((b) => (b.onclick = () => setMode(b.dataset.m)));
  setMode(mode);
  let test = null;
  const update = () => {
    found.classList.remove("err");
    try {
      test = matcher(pat.value, re.checked);
    } catch {
      test = null;
      found.textContent = t("files.select.invalid_regex");
      found.classList.add("err");
      ok.disabled = true;
      return;
    }
    found.textContent = test ? count(hits(pane, test, files.checked).length, "files.select.count.matches", int) : "";
    ok.disabled = !test;
  };
  pat.oninput = re.onchange = files.onchange = update;
  update();
  pat.focus();
  pat.select();
  dlg.querySelector(".cancel").onclick = () => dlg.close();
  dlg.querySelector("form").onsubmit = () => {
    if (!test) return;
    last = { text: pat.value, regex: re.checked, files: files.checked };
    const picked = hits(pane, test, files.checked).map((e) => e.path);
    if (mode === "only") pane.sel = new Set(picked);
    else if (mode === "add") picked.forEach((p) => pane.sel.add(p));
    else picked.forEach((p) => pane.sel.delete(p));
    pane.render();
  };
}

/**
 * In both panes, selects what the other one lacks or has different: a file
 * of another size or date (two seconds of slack, as FAT and SMB keep), a
 * folder only when missing. Returns how many were selected on each side.
 */
export function selectDifferences(a, b) {
  // the items of the folder shown, not those of folders opened inside it
  const top = (pane) => pane.visible.filter((e) => parentOf(e.path) === pane.path);
  const index = (pane) => new Map(top(pane).map((e) => [e.name.toLowerCase(), e]));
  const ia = index(a);
  const ib = index(b);
  const differs = (e, other) => {
    if (!other) return true;
    if (isDirLike(e) !== isDirLike(other)) return true;
    if (isDirLike(e)) return false;
    return e.size !== other.size || Math.abs((e.mtime || 0) - (other.mtime || 0)) > 2;
  };
  const pick = (pane, theirs) => {
    pane.sel = new Set(top(pane).filter((e) => differs(e, theirs.get(e.name.toLowerCase()))).map((e) => e.path));
    pane.render();
    return pane.sel.size;
  };
  const na = pick(a, ib);
  const nb = pick(b, ia);
  toast(na + nb ? t("files.select.differences_a_b", { a: int(na), b: int(nb) }) : t("files.select.same_items"));
  return [na, nb];
}
