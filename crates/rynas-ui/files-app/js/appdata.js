// The apps' data folders: "/@apps/<folder>/…", shown to administrators when
// the Docker setting asks for it (rynasd decides who gets them, never this
// code). Their names, and after a change inside the folder of a running app,
// the offer to restart it: many apps read their files only when they start.

import { t } from "./i18n.js";
import { headers, errorOf, notifyUnauth } from "./api.js";
import { toast, errorToast } from "./ui.js";

export const APPS = "@apps";
export const APPS_PATH = "/" + APPS;

/** "/@apps" itself: the list of the apps' folders. */
export const isAppsRoot = (path) => path === APPS_PATH;
/** A path inside one app's folder (or the folder). */
export const inApps = (path) => path.startsWith(APPS_PATH + "/");

// folder name → { name, stack, app? } (the `apps` of the shares answer)
let folders = new Map();

export function setAppFolders(list) {
  folders = new Map((list || []).map((a) => [a.name, a]));
}

export const appFolders = () => [...folders.values()];

/** The stack whose folder holds `path`, else null. */
export function stackOf(path) {
  if (!inApps(path || "")) return null;
  return folders.get(path.split("/")[2])?.stack || null;
}

// one offer per stack in a while: a copy of many files is one change
const QUIET_MS = 20_000;
const offered = new Map();

async function rynas(path, method = "GET") {
  const res = await fetch(path, { method, credentials: "same-origin", headers: headers(method) });
  let data = null;
  try {
    data = await res.json();
  } catch {
    data = null;
  }
  if (res.status === 401) notifyUnauth();
  if (!res.ok) throw errorOf(res.status, data);
  return data;
}

/**
 * Something changed at these paths (saved, made, uploaded, renamed, deleted,
 * copied or moved there): when one is in the folder of an app that runs,
 * offers to restart that app.
 */
export async function changed(...paths) {
  const stacks = new Set(paths.flat().map(stackOf).filter(Boolean));
  for (const stack of stacks) {
    if (Date.now() - (offered.get(stack) || 0) < QUIET_MS) continue;
    offered.set(stack, Date.now());
    let running = 0;
    try {
      const d = await rynas(`/api/v1/docker/stacks/${encodeURIComponent(stack)}`);
      running = d?.summary?.running || 0;
    } catch {
      continue;
    }
    if (!running) continue;
    toast(t("L'app {s} potrebbe dover ripartire per vedere la modifica", { s: stack }), {
      action: t("Riavvia"),
      timeout: 12_000,
      onAction: () => restart(stack),
    });
  }
}

async function restart(stack) {
  try {
    await rynas(`/api/v1/docker/stacks/${encodeURIComponent(stack)}/restart`, "POST");
    toast(t("Riavvio di {s} avviato", { s: stack }), { kind: "ok" });
  } catch (e) {
    errorToast(e);
  }
}
