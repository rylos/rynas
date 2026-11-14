// The PCs' backups: "/@backups/<pc>/…", the btrfs snapshots rynas received
// from the PCs, shown to administrators (rynasd decides who gets them, never
// this code). Read-only: the server refuses every write there, this module
// only spares the offer of actions that would be refused, and names the
// snapshots by their date.

import { t } from "./i18n.js";
import { dateFormat } from "./format.js";
import { toast } from "./ui.js";
import { readOnlyToast } from "./archive.js";
import { APPS_PATH } from "./appdata.js";

export const BACKUPS = "@backups";
export const BACKUPS_PATH = "/" + BACKUPS;

/** "/@backups" itself: the list of the PCs' folders. */
export const isBackupsRoot = (path) => path === BACKUPS_PATH;
/** "/@backups" or a path below it: nothing may be written there. */
export const atBackups = (path) => path === BACKUPS_PATH || (path || "").startsWith(BACKUPS_PATH + "/");
/** "/@backups/<pc>": one PC's folder, the snapshots it holds. */
export const isPcFolder = (path) => atBackups(path) && path.split("/").filter(Boolean).length === 2;
/**
 * "/@backups" or a PC's folder: what lies there is a whole backup or a whole
 * snapshot (summed up, mostly the same data again and again), never measured,
 * analysed or zipped from here.
 */
export const isBackupTop = (path) => isBackupsRoot(path) || isPcFolder(path);
/** The folder an entry's path lies in. */
const parentPath = (path) => path.slice(0, path.lastIndexOf("/")) || "/";
/** A PC's whole backup or a whole snapshot: an entry that [`isBackupTop`] holds, or such a folder itself. */
export const isWholeBackup = (path) => isBackupTop(path) || isBackupTop(parentPath(path));
/**
 * The apps' data and the PCs' backups are no SMB shares: they have no
 * network path (\\NAS\…) nor smb:// address.
 */
export const hasNoSmbPath = (path) => atBackups(path) || path === APPS_PATH || (path || "").startsWith(APPS_PATH + "/");

// the `backups` of the shares answer: [{ name }]
let folders = [];

export function setBackupFolders(list) {
  folders = list || [];
}

export const backupFolders = () => folders;

/** A pane where nothing may be written: an archive, or a read-only folder (the listing says so). */
export const isReadOnly = (pane) => !!pane.archive || !!pane.readOnly || atBackups(pane.path);

/** Tells why nothing can be written here. */
export function readOnlyHere(pane) {
  if (pane?.archive) return readOnlyToast();
  backupsToast();
}

export function backupsToast() {
  toast(t("I backup sono in sola lettura: per cambiare un file, copialo in una cartella condivisa"));
}

// <prefix>.YYYYMMDD[THHMM[SS][±hhmm]][_N]: the names btrbk and rynas-agent give
const SNAPSHOT = /^(.+)\.(\d{4})(\d{2})(\d{2})(?:T(\d{2})(\d{2})(\d{2})?([+-]\d{4})?)?(?:_(\d+))?$/;

/** The parts of a snapshot's name, or null when the name is not a snapshot's. */
export function parseSnapshot(name) {
  const m = SNAPSHOT.exec(name || "");
  if (!m) return null;
  const [, prefix, y, mo, d, h = "", mi = "", s = "", , n = ""] = m;
  if (+mo < 1 || +mo > 12 || +d < 1 || +d > 31 || (h && (+h > 23 || +mi > 59 || (s && +s > 59)))) return null;
  // the PC's own clock, as the name has it: no conversion
  const key = `${y}${mo}${d}${h || "00"}${mi || "00"}${s || "00"}${n.padStart(6, "0")}`;
  return { prefix, y: +y, mo: +mo, d: +d, h: h ? +h : null, mi: h ? +mi : null, n: n ? +n : 0, key };
}

// as the rest of the interface writes dates and times (rynas-ui i18n::datetime): the
// locale's medium date and short time with the time format preference
// ("13 nov 2026, 15:43" / "Nov 13, 2026, 3:43 PM"); UTC on both sides, so the
// date shows as the name writes it
const snapshotFormat = (withTime) => dateFormat(withTime ? { dateStyle: "medium", timeStyle: "short", timeZone: "UTC" } : { dateStyle: "medium", timeZone: "UTC" });

/**
 * What a snapshot folder is called on the screen, the date first so that cut
 * rows still differ: "13 nov 2026, 15:43 · @ (root)", " #2" for another one of
 * the same minute; null when the name is not a snapshot's.
 */
export function snapshotLabel(name) {
  const p = parseSnapshot(name);
  if (!p) return null;
  const when = snapshotFormat(p.h !== null).format(new Date(Date.UTC(p.y, p.mo - 1, p.d, p.h ?? 0, p.mi ?? 0)));
  const what = p.prefix === "@" ? `@ (${t("radice")})` : p.prefix;
  return `${when} · ${what}` + (p.n ? ` #${p.n}` : "");
}

/** The label of a path when it is a snapshot in a PC's folder ("/@backups/<pc>/<snapshot>"), else null. */
export function pathLabel(path) {
  return entryLabel({ kind: "d", name: path.slice(path.lastIndexOf("/") + 1), path });
}

/** The label of an entry ({ name, path, kind }) when it is a snapshot in a PC's folder, else null. */
export function entryLabel(e) {
  if (e.kind !== "d" || !e.path) return null;
  return isPcFolder(parentPath(e.path)) ? snapshotLabel(e.name) : null;
}

/**
 * The order by name in a PC's folder: snapshots newest first (`dir` 1, the
 * default), then anything else by name.
 */
export function compareSnapshots(a, b, byName) {
  const ka = parseSnapshot(a.name)?.key;
  const kb = parseSnapshot(b.name)?.key;
  if (ka && kb && ka !== kb) return ka < kb ? 1 : -1;
  if (!ka !== !kb) return ka ? -1 : 1;
  return byName(a, b);
}
