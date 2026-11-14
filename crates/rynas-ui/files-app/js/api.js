import { LANG, t, errText } from "./i18n.js";
import { host, emit } from "./host.js";

// Talking to the files API of rynas (/api/v1/files/<route>): the same
// routes and JSON shapes as SynoFile's api.cgi. The rynas session cookie
// goes with every request (HttpOnly, never seen here); requests that change
// something carry the session's CSRF token, which the shell hands to
// <rynas-files> (host.csrf).
//
// A 401 means the rynas session is over: the shell hears "rynas-files:unauth"
// and shows its login screen; the page hears "rynas-files:auth-lost" once and
// "rynas-files:auth" when a request works again (after the new login the
// shell sets a new token, which counts as the session being back).

const BASE = "/api/v1/files/";
const CSRF_HEADER = "X-CSRF-Token";
let unauthSent = false;

function sessionBack() {
  if (!unauthSent) return;
  unauthSent = false;
  dispatchEvent(new CustomEvent("rynas-files:auth"));
}

/** The shell set a new CSRF token: a new session (after signing in again). */
export function tokenChanged() {
  sessionBack();
}

/** The session is gone: tells the shell (login screen) and the page, once until it comes back. */
export function notifyUnauth() {
  if (unauthSent) return;
  unauthSent = true;
  emit("unauth");
  dispatchEvent(new CustomEvent("rynas-files:auth-lost"));
}

// Requests made while the user is away (no key, click or wheel for a minute:
// the transfers' polling, timers) carry X-Rynas-Background, so they do not
// renew the session's idle timeout (guidelines A24b), as the shell's own.
const USER_ACTIVE_MS = 60_000;
let lastInput = Date.now();
for (const ev of ["keydown", "pointerdown", "wheel"]) addEventListener(ev, () => (lastInput = Date.now()), { capture: true, passive: true });

/** Headers of a request with method `method` (the CSRF token on anything but GET/HEAD). */
export function headers(method = "GET") {
  const h = { Accept: "application/json" };
  if (method !== "GET" && method !== "HEAD" && host.csrf) h[CSRF_HEADER] = host.csrf;
  if (Date.now() - lastInput >= USER_ACTIVE_MS) h["X-Rynas-Background"] = "1";
  return h;
}

export class ApiError extends Error {
  constructor(status, message, code = "") {
    super(message);
    this.status = status;
    // the stable machine code of rynas's ApiError (`code`), when the server sent one
    this.code = code;
  }
}

/**
 * The error a failed answer carries: the server's `code` translated (it/en),
 * falling back to its message (`message` of rynas, `error` of SynoFile's
 * shape) and then to the status.
 */
export function errorOf(status, data) {
  const code = (data && typeof data.code === "string" && data.code) || "";
  const server = (data && (data.message || data.error)) || "";
  return new ApiError(status, errText(code) || server || t("errore {n}", { n: status }), code);
}

/** The real status: real codes from rynas, or the 422 + X-Synofile-Status of SynoFile's CGI. */
export function statusOf(res) {
  return +res.headers.get("X-Synofile-Status") || res.status;
}

export function url(route, params = {}) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null) q.set(k, v);
  q.set("lang", LANG);
  const s = q.toString();
  return BASE + route + (s ? "?" + s : "");
}

export async function call(route, opts = {}) {
  const { params, body, method, raw, signal } = opts;
  const verb = method || (body !== undefined ? "POST" : "GET");
  const options = { method: verb, credentials: "same-origin", headers: headers(verb), signal };
  if (body !== undefined) {
    if (raw) {
      options.body = body;
      options.headers["Content-Type"] = "application/octet-stream";
    } else {
      options.body = JSON.stringify(body);
      options.headers["Content-Type"] = "application/json";
    }
  }
  let res;
  try {
    res = await fetch(url(route, params), options);
  } catch (e) {
    if (e?.name === "AbortError") throw e;
    throw new ApiError(0, t("NAS non raggiungibile"), "network");
  }
  let data = null;
  try {
    data = await res.json();
  } catch {
    data = null;
  }
  const status = statusOf(res);
  if (status === 401) notifyUnauth();
  else if (res.ok) sessionBack();
  if (!res.ok) throw errorOf(status, data);
  return data;
}

export const api = {
  init: () => call("init"),
  whoami: () => call("whoami"),
  shares: () => call("shares"),
  list: (path) => call("list", { params: { path } }),
  stat: (path, signal) => call("stat", { params: { path }, signal }),
  media: (path, signal) => call("media", { params: { path }, signal }),
  usage: (paths) => call("usage", { body: { paths } }),
  space: (path) => call("space", { params: { path } }),
  mkdir: (path, name, parents) => call("mkdir", { params: { path, name, parents: parents ? 1 : undefined }, body: {} }),
  rename: (path, name) => call("rename", { params: { path, name }, body: {} }),
  search: (path, q, signal) => call("search", { params: { path, q }, signal }),
  text: (path) => call("text", { params: { path } }),
  editOpen: (path) => call("edit", { params: { path } }),
  editSave: (path, text, stamp, force) => call("edit/save", { params: { path, stamp, force: force ? 1 : undefined }, body: text, raw: true }),
  exists: (path, names) => call("exists", { params: { path }, body: names }),
  prefs: () => call("prefs"),
  savePrefs: (p) => call("prefs", { body: p }),
  jobs: (since) => call("jobs", { params: { since } }),
  newJob: (job) => call("jobs/new", { body: job }),
  control: (id, action, extra = {}) => call("jobs/control", { body: { id, action, ...extra } }),
  view: (path) => url("view", { path }),
  thumb: (path, big) => url("thumb", { path, size: big ? "big" : undefined }),
  download: (paths) => url("download", { paths: JSON.stringify(paths) }),
  conns: () => call("conns"),
  saveConn: (c) => call("conns/save", { body: c }),
  testConn: (c) => call("conns/test", { body: c }),
  deleteConn: (id) => call("conns/delete", { body: { id } }),
  orderConns: (ids) => call("conns/order", { body: ids }),
  makeKey: () => call("conns/key", { body: {} }),
  // removable drives (a route rynas adds to the SynoFile contract)
  eject: (share) => call("eject", { body: { share } }),
};
