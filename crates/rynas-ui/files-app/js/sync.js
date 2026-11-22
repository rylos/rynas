// Synchronizing the folders of the two panes: the two trees compared on the
// NAS, every difference shown with what will be done about it, and only
// then the copies (and, for a mirror, the removals, into the shared folder's
// recycle bin).

import { api } from "./api.js";
import { t, count, errText } from "./i18n.js";
import { esc, int, size, dateTime, joinPath } from "./format.js";
import { svg } from "./icons.js";
import { dialog, errorToast, confirmBox, toast } from "./ui.js";
import * as transfers from "./transfers.js";

// differences drawn at most
const SHOWN = 2000;
let lastMode = "update";

/**
 * Whether an action is ticked at once: a mirror also offers to copy over what is newer on
 * the right or the same date with another size, but leaves those for the user to tick.
 */
export function preset(d, mode) {
  const act = action(d, mode);
  if (!act) return false;
  return !(mode === "mirror" && (d.what === "newer_b" || d.what === "size"));
}

/** What a mode does with a difference: "a2b", "b2a", "del" (from b), or "" (left alone, for the user). */
export function action(d, mode) {
  if (d.what === "kind" || d.what === "unreadable") return "";
  if (mode === "update") return d.what === "a" || d.what === "newer_a" ? "a2b" : "";
  if (mode === "mirror") return d.what === "b" ? "del" : "a2b";
  // both ways: the newer wins; the same date with another size is for the user to decide
  if (d.what === "a" || d.what === "newer_a") return "a2b";
  if (d.what === "b" || d.what === "newer_b") return "b2a";
  return "";
}

export function syncFolders(app, paneA, paneB) {
  let a = paneA.path;
  let b = paneB.path;
  const dlg = dialog(
    `
    <div class="dlg-head"><div class="dlg-icon">${svg("refresh")}</div><h2>${esc(t("files.sync.sync_folders"))}</h2></div>
    <div class="dlg-body sync">
      <div class="sync-ends"><span class="s-a"></span><button type="button" class="icon-btn s-swap" title="${esc(t("files.sync.swap"))}" aria-label="${esc(t("files.sync.swap"))}">${svg("refresh")}</button><span class="s-b"></span></div>
      <div class="sync-opts">
        <div class="seg s-mode" role="radiogroup">
          <button type="button" role="radio" data-m="update" title="${esc(t("files.sync.update_hint"))}">${esc(t("files.sync.update"))}</button>
          <button type="button" role="radio" data-m="mirror" title="${esc(t("files.sync.mirror_hint"))}">${esc(t("files.sync.mirror"))}</button>
          <button type="button" role="radio" data-m="both" title="${esc(t("files.sync.both_hint"))}">${esc(t("files.sync.both"))}</button>
        </div>
        <button type="button" class="btn primary s-go">${esc(t("files.sync.compare"))}</button>
      </div>
      <div class="sync-mode-hint hint"></div>
      <div class="sync-prog hint" role="status" hidden></div>
      <div class="sync-res" hidden><div class="sync-sum"></div><div class="sync-list"></div></div>
    </div>
    <div class="dlg-foot"><span class="sync-sel hint"></span><span style="flex:1"></span><button type="button" class="btn s-close">${esc(t("files.common.close"))}</button><button type="button" class="btn primary s-run" disabled>${esc(t("files.sync.synchronize"))}</button></div>`,
    { wide: true, cls: "sync-dlg" },
  );
  const q = (s) => dlg.querySelector(s);
  let mode = lastMode;
  let diffs = [];
  let plan = [];
  let controller = null;
  dlg.addEventListener("close", () => controller?.abort());
  q(".s-close").onclick = () => dlg.close();
  const ends = () => {
    q(".s-a").textContent = app.displayPath(a);
    q(".s-a").title = app.displayPath(a);
    q(".s-b").textContent = app.displayPath(b);
    q(".s-b").title = app.displayPath(b);
  };
  ends();
  const HINT = { update: "files.sync.update_hint", mirror: "files.sync.mirror_hint", both: "files.sync.both_hint" };
  const setMode = (m) => {
    mode = m;
    dlg.querySelectorAll(".s-mode button").forEach((x) => {
      x.classList.toggle("on", x.dataset.m === m);
      x.setAttribute("aria-checked", String(x.dataset.m === m));
    });
    q(".sync-mode-hint").textContent = t(HINT[m]);
    if (diffs.length) build();
  };
  dlg.querySelectorAll(".s-mode button").forEach((x) => (x.onclick = () => setMode(x.dataset.m)));
  setMode(mode);
  q(".s-swap").onclick = () => {
    [a, b] = [b, a];
    // a comparison still running is of the sides as they were: its answer must not be shown
    controller?.abort();
    ends();
    diffs = [];
    plan = [];
    q(".sync-res").hidden = true;
    q(".s-run").disabled = true;
    q(".sync-sel").textContent = "";
  };

  const ARROW = { a2b: "→", b2a: "←", del: "✕", "": "·" };
  const WHAT = {
    a: "files.sync.only_left", b: "files.sync.only_right", newer_a: "files.sync.newer_left", newer_b: "files.sync.newer_right",
    size: "files.sync.same_date_other_size", kind: "files.sync.file_and_folder", unreadable: "files.sync.unreadable",
  };
  const side = (s, dir) => (s ? `${dir ? t("files.common.folder") : size(s.size)} · ${dateTime(s.mtime)}` : "—");
  const updateFoot = () => {
    const on = plan.filter((p) => p.on);
    const n = (k) => on.filter((p) => p.act === k).length;
    const parts = [];
    if (n("a2b")) parts.push(t("files.sync.n_to_right", { n: int(n("a2b")) }));
    if (n("b2a")) parts.push(t("files.sync.n_to_left", { n: int(n("b2a")) }));
    if (n("del")) parts.push(t("files.sync.n_off_right", { n: int(n("del")) }));
    q(".sync-sel").textContent = parts.join(" · ");
    q(".s-run").disabled = !on.length;
  };
  const build = () => {
    plan = diffs.map((d) => ({ d, act: action(d, mode), on: preset(d, mode) }));
    const shown = plan.slice(0, SHOWN);
    q(".sync-list").innerHTML =
      shown
        .map(
          (p, i) => `<label class="sync-item act-${p.act || "none"}" title="${esc(t(WHAT[p.d.what]))}">
        <input type="checkbox" data-i="${i}"${p.on ? " checked" : ""}${p.act ? "" : " disabled"}>
        <span class="sync-arrow" aria-label="${esc(t(WHAT[p.d.what]))}">${ARROW[p.act]}</span>
        <span class="sync-path" title="${esc(p.d.path)}${p.d.dir ? "/" : ""}">${esc(p.d.path)}${p.d.dir ? "/" : ""}</span>
        <span class="sync-side">${esc(side(p.d.a, p.d.dir))}</span><span class="sync-side">${esc(side(p.d.b, p.d.dir))}</span></label>`,
        )
        .join("") + (plan.length > shown.length ? `<div class="hint">${esc(t("files.common.n_more", { n: int(plan.length - shown.length) }))}</div>` : "");
    updateFoot();
  };
  q(".sync-list").addEventListener("change", (e) => {
    const box = e.target.closest("input[data-i]");
    if (!box) return;
    plan[+box.dataset.i].on = box.checked;
    updateFoot();
  });

  q(".s-go").onclick = async () => {
    lastMode = mode;
    controller?.abort();
    const mine = (controller = new AbortController());
    q(".sync-res").hidden = true;
    q(".s-run").disabled = true;
    const prog = q(".sync-prog");
    prog.hidden = false;
    prog.textContent = t("files.sync.comparing");
    q(".s-go").disabled = true;
    let result = null;
    try {
      let failed = null;
      await api.compare(
        a,
        b,
        (line) => {
          if (line.folders && !line.diffs) prog.textContent = t("files.sync.comparing_n", { n: int(line.folders) });
          if (line.diffs) result = line;
          if (line.error) failed = errText(line.code) || line.error;
        },
        mine.signal,
      );
      if (failed) throw new Error(failed);
      if (!result) throw new Error(t("files.pane.incomplete_answer"));
    } catch (err) {
      if (!mine.signal.aborted) errorToast(err);
      result = null;
    } finally {
      if (controller === mine) {
        prog.hidden = true;
        q(".s-go").disabled = false;
      }
    }
    if (!result || !dlg.open) return;
    diffs = result.diffs;
    q(".sync-res").hidden = false;
    q(".sync-sum").textContent = diffs.length ? count(diffs.length, "files.sync.count.differences", int) + (result.more ? " · " + t("files.sync.first_only") : "") : t("files.sync.folders_same");
    const notes = [];
    const unreadable = diffs.filter((d) => d.what === "unreadable").length;
    if (unreadable) notes.push(t("files.sync.n_unreadable", { n: int(unreadable) }));
    if (result.skipped) notes.push(t("files.dupes.n_odd_names_skipped", { n: int(result.skipped) }));
    if (notes.length) q(".sync-sum").textContent += " · " + notes.join(" · ");
    build();
  };

  q(".s-run").onclick = async () => {
    const on = plan.filter((p) => p.on);
    const pick = (k) => on.filter((p) => p.act === k).map((p) => p.d);
    const toB = pick("a2b");
    const toA = pick("b2a");
    const del = pick("del");
    if (!toB.length && !toA.length && !del.length) return toast(t("files.sync.nothing_to_do"));
    // the bins of both sides, asked now: a share without one loses what is replaced or removed
    let binA = false;
    let binB = false;
    try {
      [binA, binB] = await Promise.all([toA.length ? api.list(a).then((r) => !!r.recycle) : false, toB.length || del.length ? api.list(b).then((r) => !!r.recycle) : false]);
    } catch (err) {
      return errorToast(err);
    }
    // files replaced on each side: their old content goes to that side's bin, or is lost
    const overB = toB.filter((d) => d.b && !d.dir);
    const overA = toA.filter((d) => d.a && !d.dir);
    const lost = [...(binB ? [] : overB), ...(binA ? [] : overA)];
    const binned = [...(binB ? overB : []), ...(binA ? overA : [])];
    if (del.length || overB.length || overA.length) {
      const lines = [];
      if (binned.length) lines.push(esc(count(binned.length, "files.sync.count.replaced_go_recycle", int)));
      if (del.length && binB) lines.push(esc(count(del.length, "files.sync.count.extra_go_recycle", int)));
      const danger = lost.length > 0 || (del.length > 0 && !binB);
      if (lost.length) lines.push(`<b style="color:var(--crit)">${esc(count(lost.length, "files.sync.count.replaced_lost", int))}</b>`);
      if (del.length && !binB) lines.push(`<b style="color:var(--crit)">${esc(count(del.length, "files.actions.count.deleted_permanently", int))}</b>`);
      if (danger) lines.push(esc(t("files.actions.recycle_bin_shared_folder_off")));
      const ok = await confirmBox({
        title: t("files.sync.sync_ask"),
        message: lines.join("<br>"),
        items: [...del, ...overB, ...overA].map((d) => esc(d.path)),
        ok: t("files.sync.synchronize"),
        danger,
        icon: "refresh",
      });
      if (!ok) return;
    }
    dlg.close();
    // what the comparison saw of both sides: the NAS leaves alone what changed since
    const sideOf = (s, dir) => (s ? { size: s.size, mtime: s.mtime, dir } : null);
    const job = (list, src, dest, from, to, bin) => ({
      op: "sync",
      sources: [src],
      dest,
      members: list.map((d) => d.path),
      conflict: "overwrite",
      permanent: !bin,
      expect: list.map((d) => ({ path: d.path, seen: sideOf(d[from], d.dir) })),
      expect_dest: list.map((d) => ({ path: d.path, seen: sideOf(d[to], d.dir) })),
    });
    if (toB.length) await transfers.start(job(toB, a, b, "a", "b", binB));
    if (toA.length) await transfers.start(job(toA, b, a, "b", "a", binA));
    if (del.length) await transfers.start({ op: "delete", sources: del.map((d) => joinPath(b, d.path)), permanent: !binB, expect: del.map((d) => ({ path: joinPath(b, d.path), seen: sideOf(d.b, d.dir) })) });
  };
}
