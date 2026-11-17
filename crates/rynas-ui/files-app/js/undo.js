// Undo (Ctrl+Z): the last renames, new folders, moves and copies, newest
// first, 30 steps at most. What a deletion threw away is in the shared
// folder's recycle bin instead.

import { api } from "./api.js";
import { t, count } from "./i18n.js";
import { int, esc, baseName, parentOf, joinPath } from "./format.js";
import { toast, errorToast, confirmBox } from "./ui.js";
import * as transfers from "./transfers.js";
import { renameMany } from "./rename.js";

const MAX = 30;
const stack = [];
let app = null;
// thrown by a step: the user said no (the step goes back on the stack)
const DECLINED = Symbol("declined");
// thrown by a step: the reason is already on screen (transfers.start shows its own errors)
const SHOWN = Symbol("shown");
/** A refusal that keeps the step on the stack, to be tried again later. */
const notYet = (message) => Object.assign(new Error(message), { keep: true });

export function initUndo(theApp) {
  app = theApp;
}

/** The label of what Ctrl+Z would undo, or null. */
export function undoLabel() {
  return stack.length ? stack[stack.length - 1].label : null;
}

function push(step) {
  stack.push(step);
  if (stack.length > MAX) stack.shift();
}

/** A transfer job for an undo; its id, or SHOWN thrown when it did not start. */
async function job(request) {
  const id = await transfers.start(request);
  if (id == null) throw SHOWN;
  return id;
}

/** `pairs`: [old path, new name] of items just renamed, all in one step (`label`: its name in the menu). */
export function recordRename(pairs, label) {
  if (!pairs.length) return;
  const done = pairs.map(([from, name]) => [joinPath(parentOf(from), name), baseName(from)]);
  push({
    label: label || t("files.undo.rename_a_b", { a: baseName(pairs[0][0]), b: pairs[0][1] }),
    run: async () => {
      // all back at once: swaps and shifts of names undo as they were done
      const { errors } = await renameMany(done);
      app.refreshFolders([...new Set(done.map(([p]) => parentOf(p)))]);
      if (errors.length) throw new Error(errors.slice(0, 3).join("; "));
    },
  });
}

/** A folder just made: undone only while it is still empty. */
export function recordNewFolder(path) {
  push({
    label: t("files.undo.new_folder_n", { n: baseName(path) }),
    run: async () => {
      const { entries } = await api.list(path);
      // whatever was put in it since is never thrown away
      if (entries.length) throw new Error(t("files.undo.folder_n_not_empty", { n: baseName(path) }));
      await job({ op: "delete", sources: [path], permanent: true });
    },
  });
}

/**
 * A move or a copy just started as job `id`. `fresh`: the names the
 * destination did not hold before (only those are undone: an item that was
 * already there, replaced or kept, is never moved back or thrown away).
 */
export function recordTransfer(id, op, sources, dest, fresh) {
  if (id == null || !fresh) return;
  const items = sources.filter((p) => fresh.has(baseName(p))).map((p) => [p, joinPath(dest, baseName(p))]);
  if (!items.length) return;
  const n = count(items.length, "files.common.count.items", int);
  const d = baseName(dest) || dest;
  push({
    label: op === "move" ? t("files.undo.move_n_d", { n, d }) : t("files.undo.copy_n_d", { n, d }),
    run: async () => {
      const running = transfers.all().find((j) => j.id === id);
      if (running && !transfers.finished(running)) throw notYet(t("files.undo.transfer_still_running"));
      // what the job left in the destination (a failed or skipped item is not there)
      const there = new Set((await api.exists(dest, items.map(([, now]) => baseName(now)))).taken.map((x) => x.name));
      const left = items.filter(([, now]) => there.has(baseName(now)));
      if (!left.length) throw new Error(t("files.undo.nothing_left_n", { n: d }));
      if (op === "move") {
        // back where each came from, one move per folder of origin
        const byParent = new Map();
        for (const [from, now] of left) {
          const parent = parentOf(from);
          if (!byParent.has(parent)) byParent.set(parent, []);
          byParent.get(parent).push(now);
        }
        for (const [parent, list] of byParent) await job({ op: "move", sources: list, dest: parent });
        return;
      }
      // a copy: its new items go to the recycle bin, after asking
      const copies = left.map(([, now]) => now);
      const recycle = !!(await api.list(dest)).recycle;
      const many = count(copies.length, "files.common.count.items", int);
      const ok = await confirmBox({
        title: t("files.undo.undo_copy_ask"),
        message: recycle
          ? esc(t("files.actions.n_go_shared_folder_recycle", { n: many }))
          : `<b style="color:var(--crit)">${esc(t("files.actions.n_deleted_permanently_cannot_restored", { n: many }))}</b><br>${esc(t("files.actions.recycle_bin_shared_folder_off"))}`,
        items: copies.map((p) => esc(baseName(p))),
        ok: recycle ? t("files.common.move_recycle_bin") : t("files.common.delete"),
        danger: !recycle,
        icon: "trash",
      });
      if (!ok) throw DECLINED;
      await job({ op: "delete", sources: copies, permanent: !recycle });
    },
  });
}

/** Undoes the last step. */
export async function undo() {
  const step = stack.pop();
  if (!step) return toast(t("files.undo.nothing_to_undo"));
  try {
    await step.run();
    toast(t("files.undo.undone_s", { s: step.label }));
  } catch (err) {
    // the user said no, or the job is still running: it can be tried again
    if (err === DECLINED || err?.keep) stack.push(step);
    if (err !== DECLINED && err !== SHOWN) errorToast(err);
  }
}
