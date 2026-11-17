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
      // the other way round: a folder takes its name back before what is inside it
      const { errors } = await renameMany(done, { shallowFirst: true });
      app.renamedPaths(done);
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

/** Whether another step on the stack already counts `name` in `dest` as made by its job. */
const claimed = (dest, name) => stack.some((s) => s.claims?.has(`${dest}\n${name}`));

/**
 * A move or a copy just started as job `id`. `fresh`: the names the
 * destination did not hold before. Only those are undone, and only while
 * each is still the very item the job left there (its inode, size and date
 * recorded when the job ended: `jobFinished`): an item that was already
 * there, replaced or kept, or put there since by someone else, is never
 * moved back or thrown away. Two transfers into one folder never both
 * claim a name.
 */
export function recordTransfer(id, op, sources, dest, fresh) {
  if (id == null || !fresh) return;
  const items = sources.filter((p) => fresh.has(baseName(p)) && !claimed(dest, baseName(p))).map((p) => [p, joinPath(dest, baseName(p))]);
  if (!items.length) return;
  const n = count(items.length, "files.common.count.items", int);
  const d = baseName(dest) || dest;
  const names = items.map(([, now]) => baseName(now));
  const step = {
    label: op === "move" ? t("files.undo.move_n_d", { n, d }) : t("files.undo.copy_n_d", { n, d }),
    claims: new Set(names.map((name) => `${dest}\n${name}`)),
    job: id,
    // name -> {ino, size, mtime, dir} of what the job left, once it ended
    made: null,
    record: () =>
      (step.made ??= api.exists(dest, names).then(
        (r) => new Map(r.taken.map((x) => [x.name, x])),
        () => null,
      )),
    run: async () => {
      const running = transfers.all().find((j) => j.id === id);
      if (running && !transfers.finished(running)) throw notYet(t("files.undo.transfer_still_running"));
      const made = await (step.made || Promise.resolve(null));
      if (!made) throw new Error(t("files.undo.not_recorded"));
      // what is still exactly what the job left (a failed or skipped item was never there)
      const now = new Map((await api.exists(dest, names)).taken.map((x) => [x.name, x]));
      const same = (a, b) => a && b && a.ino === b.ino && a.dir === b.dir && (a.dir || (a.size === b.size && a.mtime === b.mtime));
      const left = items.filter(([, p]) => same(made.get(baseName(p)), now.get(baseName(p))));
      if (!left.length) throw new Error(t("files.undo.nothing_left_n", { n: d }));
      if (op === "move") {
        // back where each came from, one move per folder of origin
        const byParent = new Map();
        for (const [from, p] of left) {
          const parent = parentOf(from);
          if (!byParent.has(parent)) byParent.set(parent, []);
          byParent.get(parent).push(p);
        }
        for (const [parent, list] of byParent) await job({ op: "move", sources: list, dest: parent });
        return;
      }
      // a copy: its new items go to the recycle bin, after asking
      const copies = left.map(([, p]) => p);
      const recycle = !!(await api.list(dest)).recycle;
      const ok = await confirmBox({
        title: t("files.undo.undo_copy_ask"),
        message: recycle
          ? esc(count(copies.length, "files.actions.count.go_recycle", int))
          : `<b style="color:var(--crit)">${esc(count(copies.length, "files.actions.count.deleted_permanently", int))}</b><br>${esc(t("files.actions.recycle_bin_shared_folder_off"))}`,
        items: copies.map((p) => esc(baseName(p))),
        ok: recycle ? t("files.common.move_recycle_bin") : t("files.common.delete"),
        danger: !recycle,
        icon: "trash",
      });
      if (!ok) throw DECLINED;
      // the NAS checks once more that each is still as recorded before it deletes
      const expect = copies.map((p) => {
        const m = made.get(baseName(p));
        return { path: p, seen: { size: m.size, mtime: m.mtime, dir: m.dir } };
      });
      await job({ op: "delete", sources: copies, permanent: !recycle, expect });
    },
  };
  push(step);
  // a quick job may have ended already, before its step was kept
  const ended = transfers.all().find((j) => j.id === id);
  if (ended && transfers.finished(ended)) step.record();
}

/** A transfer job ended (main.js refreshAfter): its step records what it left, once. */
export function jobFinished(job) {
  for (const step of stack) if (step.job === job.id && step.record) step.record();
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
