// The duplicate finder: files alike under a folder, in groups; the copies
// picked go to the shared folder's recycle bin, and one copy of each group
// always stays.

import { api } from "./api.js";
import { t, count, errText } from "./i18n.js";
import { esc, int, size, dateTime, joinPath, parentOf, baseName } from "./format.js";
import { svg } from "./icons.js";
import { dialog, toast, errorToast, confirmBox } from "./ui.js";
import * as transfers from "./transfers.js";

// groups drawn at most
const SHOWN = 1000;
// the options last used, offered again
let last = { by: "content", min: "1048576" };

export function findDuplicates(app, root) {
  const dlg = dialog(
    `
    <div class="dlg-head"><div class="dlg-icon">${svg("copy")}</div><h2>${esc(t("files.dupes.duplicates_in_x", { x: baseName(root) || root }))}</h2></div>
    <div class="dlg-body dup">
      <div class="dup-opts">
        <label class="field inline"><span>${esc(t("files.dupes.compare"))}</span><select class="input d-by">
          <option value="content">${esc(t("files.dupes.by_content"))}</option><option value="name">${esc(t("files.dupes.by_name"))}</option></select></label>
        <label class="field inline"><span>${esc(t("files.dupes.minimum_size"))}</span><select class="input d-min">
          <option value="1">${esc(t("files.dupes.any_size"))}</option><option value="1048576">${size(1048576)}</option><option value="10485760">${size(10485760)}</option><option value="104857600">${size(104857600)}</option><option value="1073741824">${size(1073741824)}</option></select></label>
        <button type="button" class="btn primary d-go">${esc(t("files.dupes.find"))}</button>
      </div>
      <div class="dup-prog" hidden><div class="dup-phase" role="status"></div><div class="pbar thin"><div class="fill"></div></div></div>
      <div class="dup-res" hidden>
        <div class="dup-sum"></div>
        <div class="dup-bar"><span class="dup-pick-l">${esc(t("files.dupes.select"))}</span>
          <button type="button" class="btn small" data-pick="old">${esc(t("files.dupes.all_but_oldest"))}</button>
          <button type="button" class="btn small" data-pick="new">${esc(t("files.dupes.all_but_newest"))}</button>
          <button type="button" class="btn small" data-pick="short">${esc(t("files.dupes.all_but_shortest"))}</button>
          <button type="button" class="btn small" data-pick="none">${esc(t("files.dupes.none"))}</button></div>
        <div class="dup-list"></div>
      </div>
    </div>
    <div class="dlg-foot"><span class="dup-sel hint"></span><span style="flex:1"></span><button type="button" class="btn d-close">${esc(t("files.common.close"))}</button><button type="button" class="btn danger d-del" disabled>${esc(t("files.common.move_recycle_bin"))}</button></div>`,
    { wide: true, cls: "dupes-dlg" },
  );
  const q = (s) => dlg.querySelector(s);
  q(".d-by").value = last.by;
  q(".d-min").value = last.min;
  let controller = null;
  let groups = [];
  // compared by name and size only: listed, never picked for deletion (nothing was read)
  let listOnly = false;
  const picked = new Set();
  dlg.addEventListener("close", () => controller?.abort());
  q(".d-close").onclick = () => dlg.close();

  const full = (rel) => joinPath(root, rel);
  const selBytes = () => groups.reduce((a, g) => a + g.size * g.items.filter((i) => picked.has(i.path)).length, 0);
  const updateFoot = () => {
    q(".dup-sel").textContent = picked.size ? t("files.dupes.n_selected_s", { n: int(picked.size), s: size(selBytes()) }) : "";
    q(".d-del").disabled = !picked.size || listOnly;
    q(".d-del").hidden = listOnly;
  };
  const render = () => {
    const shown = groups.slice(0, SHOWN);
    q(".dup-list").innerHTML =
      shown
        .map(
          (g, gi) => `<div class="dup-group"><div class="dup-gh">${esc(t("files.dupes.n_copies_of_s", { n: int(g.items.length), s: size(g.size) }))}</div>
      ${g.items.map((it) => `<label class="dup-item" data-g="${gi}" title="${esc(app.displayPath(full(it.path)))}"><input type="checkbox" data-p="${esc(it.path)}"${picked.has(it.path) ? " checked" : ""}${listOnly ? " hidden disabled" : ""}><span class="dup-path">${esc(app.displayPath(full(it.path)))}</span><span class="dup-date">${esc(dateTime(it.mtime))}</span><button type="button" class="icon-btn dup-show" title="${esc(t("files.dupes.show_in_folder"))}" aria-label="${esc(t("files.dupes.show_in_folder"))}">${svg("forward")}</button></label>`).join("")}</div>`,
        )
        .join("") + (groups.length > SHOWN ? `<div class="hint">${esc(t("files.common.n_more", { n: int(groups.length - SHOWN) }))}</div>` : "");
    updateFoot();
  };
  q(".dup-list").addEventListener("change", (e) => {
    const box = e.target.closest("input[type=checkbox]");
    if (!box) return;
    const g = groups[+box.closest(".dup-item").dataset.g];
    if (box.checked && g.items.every((i) => i.path === box.dataset.p || picked.has(i.path))) {
      // one copy of each group stays
      box.checked = false;
      return toast(t("files.dupes.one_copy_stays"));
    }
    if (box.checked) picked.add(box.dataset.p);
    else picked.delete(box.dataset.p);
    updateFoot();
  });
  q(".dup-list").addEventListener("click", (e) => {
    const b = e.target.closest(".dup-show");
    if (!b) return;
    e.preventDefault();
    const p = full(b.closest(".dup-item").querySelector("input").dataset.p);
    dlg.close();
    app.pane.go(parentOf(p), { select: p });
  });
  dlg.querySelectorAll("[data-pick]").forEach(
    (b) =>
      (b.onclick = () => {
        picked.clear();
        if (b.dataset.pick !== "none" && !listOnly) {
          const order = {
            old: (x, y) => x.mtime - y.mtime,
            new: (x, y) => y.mtime - x.mtime,
            short: (x, y) => x.path.length - y.path.length || x.path.localeCompare(y.path),
          }[b.dataset.pick];
          // the groups shown only: what is not on screen is never picked unseen
          for (const g of groups.slice(0, SHOWN)) {
            const keep = [...g.items].sort(order)[0];
            for (const it of g.items) if (it !== keep) picked.add(it.path);
          }
        }
        render();
      }),
  );
  q(".d-del").onclick = async () => {
    const list = [...picked];
    // a shared folder without a recycle bin would lose them for good: said so
    let bin = false;
    try {
      bin = !!(await api.list(root)).recycle;
    } catch (err) {
      return errorToast(err);
    }
    const n = count(list.length, "files.common.count.items", int);
    const ok = await confirmBox({
      title: bin ? t("files.actions.move_recycle_bin") : t("files.actions.delete_permanently"),
      message: bin
        ? esc(t("files.actions.n_go_shared_folder_recycle", { n }))
        : `<b style="color:var(--crit)">${esc(t("files.actions.n_deleted_permanently_cannot_restored", { n }))}</b><br>${esc(t("files.actions.recycle_bin_shared_folder_off"))}`,
      items: list.map((p) => esc(p)),
      ok: bin ? t("files.common.move_recycle_bin") : t("files.common.delete"),
      danger: !bin,
      icon: "trash",
    });
    if (!ok) return;
    dlg.close();
    // the NAS checks again before it deletes: each file still as found, and never the last
    // copy of its group still as found
    const seen = (it, g) => ({ size: g.size, mtime: it.mtime });
    const touched = groups.filter((g) => g.items.some((it) => picked.has(it.path)));
    transfers.start({
      op: "delete",
      sources: list.map(full),
      permanent: !bin,
      expect: touched.flatMap((g) => g.items.filter((it) => picked.has(it.path)).map((it) => ({ path: full(it.path), seen: seen(it, g) }))),
      keep: touched.map((g) => g.items.map((it) => ({ path: full(it.path), seen: seen(it, g) }))),
    });
  };

  q(".d-go").onclick = async () => {
    last = { by: q(".d-by").value, min: q(".d-min").value };
    controller?.abort();
    const mine = (controller = new AbortController());
    groups = [];
    picked.clear();
    q(".dup-res").hidden = true;
    q(".dup-prog").hidden = false;
    q(".d-go").disabled = true;
    const bar = q(".dup-prog .pbar");
    const phase = q(".dup-phase");
    bar.classList.add("indeterminate");
    phase.textContent = t("files.dupes.reading_folders");
    let result = null;
    try {
      let failed = null;
      await api.duplicates(
        root,
        last.by,
        last.min,
        (line) => {
          if (line.phase === "scan") phase.textContent = t("files.dupes.reading_folders_n", { n: int(line.files) });
          if (line.phase === "hash") {
            bar.classList.remove("indeterminate");
            bar.style.setProperty("--p", `${line.total ? Math.min(100, (line.done / line.total) * 100) : 0}%`);
            phase.textContent = t("files.dupes.comparing_a_of_b", { a: size(line.done), b: size(line.total) });
          }
          if (line.groups) result = line;
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
        q(".dup-prog").hidden = true;
        q(".d-go").disabled = false;
      }
    }
    if (!result || !dlg.open) return;
    groups = result.groups;
    listOnly = last.by === "name";
    const wasted = groups.reduce((a, g) => a + g.size * (g.items.length - 1), 0);
    q(".dup-res").hidden = false;
    q(".dup-sum").textContent = groups.length
      ? t("files.dupes.groups_g_files_n_s", { g: count(groups.length, "files.dupes.count.groups", int), n: int(result.scanned), s: size(wasted) }) + (result.more ? " · " + t("files.dupes.biggest_only") : "")
      : t("files.dupes.none_among_n", { n: int(result.scanned) });
    const notes = [];
    if (listOnly && groups.length) notes.push(t("files.dupes.by_name_list_only"));
    if (result.skipped) notes.push(t("files.dupes.n_odd_names_skipped", { n: int(result.skipped) }));
    if (notes.length) q(".dup-sum").textContent += " · " + notes.join(" · ");
    q(".dup-bar").hidden = listOnly;
    dlg.querySelectorAll("[data-pick]").forEach((b) => (b.disabled = !groups.length));
    render();
  };
}
