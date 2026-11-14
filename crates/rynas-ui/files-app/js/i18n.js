// The File app speaks the language of the rynas shell. English is the source
// language, in js/lang/en.js (stable keys, files.<area>.<name>); every other
// language of the shell's table (/i18n/languages.txt) has its texts in
// /i18n/<code>/files.txt, one key<TAB>text line per entry (docs/i18n.md),
// fetched when that language is chosen. A key without a translation shows the
// English text. The tests in rynas-ui (files_check.rs) keep the keys of the
// code, en.js and every translation in step.

import EN from "./lang/en.js";

// One locale for the whole interface: the shell writes the language on
// <html data-lang> (a code of its table), the locale on <html lang> ("en-GB",
// "it-IT"…, from the language and the browser's own tags) and the time format
// preference on <html data-hour-cycle> ("h23", "h12", absent = the locale's).
// Without the shell: the same rules from the browser and the table (rynas-ui
// i18n::lang_for_tag, locale_for).
// Subtags Intl accepts: region (GB, 419), script (Hans), variant (5–8); "en-x" would throw.
const TAG = /^[A-Za-z]{2,3}(-([A-Za-z]{2}|[0-9]{3}|[A-Za-z]{4}|[A-Za-z0-9]{5,8}))+$/;
const CODE = /^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$/;

/** The language table without the shell: [{ code, locale }], English alone until it is read. */
let table = [{ code: "en", locale: "en-US" }];
const browserTags = () => [...(navigator.languages || []), navigator.language || ""].filter(Boolean);

function localeFor(lang) {
  const own = table.find((l) => l.code === lang);
  const prefix = lang.split("-")[0].toLowerCase();
  return browserTags().find((t) => TAG.test(t) && t.split("-")[0].toLowerCase() === prefix) || own?.locale || "en-US";
}
/** The language of a browser without the shell: a code of the table (exact, else the primary subtag), else English. */
function browserLang() {
  for (const tag of browserTags()) {
    const low = tag.toLowerCase();
    const hit = table.find((l) => l.code.toLowerCase() === low) || table.find((l) => l.code.toLowerCase() === low.split("-")[0]);
    if (hit) return hit.code;
  }
  return "en";
}
function shellLang() {
  const code = String(document.documentElement.dataset.lang || "");
  return CODE.test(code) ? code : browserLang();
}
function shellLocale() {
  const tag = String(document.documentElement.lang || "");
  if (TAG.test(tag) && document.documentElement.dataset.lang) return tag;
  return localeFor(LANG);
}
const shellHourCycle = () => (["h23", "h12"].includes(document.documentElement.dataset.hourCycle) ? document.documentElement.dataset.hourCycle : "");

/** The texts' language: a code of the shell's table ("en", "it"…). */
export let LANG = shellLang();
/** For Intl: dates, numbers, plural forms, sorting. */
export let LOCALE = shellLocale();
/** The time format preference: "h23", "h12" or "" (the locale's). */
export let HOUR_CYCLE = shellHourCycle();

/** The texts of LANG (key → text); empty for English. */
let texts = {};
let textsLang = "en";

/** Reads a translation file: key<TAB>text lines, "#" comments. */
function parse(file) {
  const out = {};
  for (const line of file.split("\n")) {
    const i = line.indexOf("\t");
    if (i > 0 && !line.startsWith("#")) out[line.slice(0, i)] = line.slice(i + 1).replace(/\r$/, "");
  }
  return out;
}

/** A text file (`text/plain`), "" when it is missing or anything else came back. */
async function fetchText(url) {
  try {
    const res = await fetch(url, { credentials: "same-origin" });
    const plain = (res.headers.get("content-type") || "").startsWith("text/plain");
    return res.ok && plain ? await res.text() : "";
  } catch {
    return "";
  }
}

/** How long a render waits for the texts of the language before it shows English (as the shell). */
const WAIT_MS = 1200;
/** Bumped by every load: an answer that comes back after a later switch is dropped. */
let loading = 0;

/**
 * The texts of `lang` (English: none to fetch). True when they are the ones in use; false when
 * the fetch failed (English meanwhile, tried again at the next switch to the language) or a
 * later switch made them useless.
 */
async function load(lang) {
  const seq = ++loading;
  if (lang === textsLang) return true;
  if (lang === "en") {
    texts = {};
    textsLang = "en";
    return true;
  }
  const file = await fetchText(`/i18n/${encodeURIComponent(lang)}/files.txt`);
  if (seq !== loading || shellLang() !== lang || !file) return false;
  texts = parse(file);
  textsLang = lang;
  return true;
}

const listeners = new Set();
/** fn(): the shell switched language, locale or time format, or the texts arrived. */
export function onLangChange(fn) {
  listeners.add(fn);
}
const notify = () => listeners.forEach((fn) => fn());

/**
 * Loads the texts of `lang`: resolves when they are there or after WAIT_MS (the interface then
 * shows English, and renders again when they arrive).
 */
function loadBounded(lang) {
  return new Promise((resolve) => {
    let waited = false;
    const timer = setTimeout(() => {
      waited = true;
      resolve();
    }, WAIT_MS);
    load(lang).then((ok) => {
      if (!waited) {
        clearTimeout(timer);
        resolve();
      } else if (ok) notify();
    });
  });
}

async function follow() {
  const lang = shellLang();
  const locale = shellLocale();
  const hc = shellHourCycle();
  if (lang === LANG && locale === LOCALE && hc === HOUR_CYCLE) return;
  const changed = lang !== LANG;
  LANG = lang;
  LOCALE = locale;
  HOUR_CYCLE = hc;
  // the new texts first (at most WAIT_MS): what re-renders on the change shows them
  if (changed) await loadBounded(lang);
  // a later switch already rendered its own language
  if (LANG === lang) notify();
}
new MutationObserver(() => follow()).observe(document.documentElement, { attributes: true, attributeFilter: ["lang", "data-lang", "data-hour-cycle"] });

/** Awaited by main.js before the first render: the language table (without the shell) and the texts. */
export async function loadTable() {
  if (!document.documentElement.dataset.lang) {
    const file = await fetchText("/i18n/languages.txt");
    const rows = file.split("\n").filter((l) => l && !l.startsWith("#")).map((l) => l.split("\t"));
    const read = rows.filter((r) => r.length >= 3 && CODE.test(r[0])).map((r) => ({ code: r[0], locale: r[2] }));
    if (read.length) table = read;
    LANG = shellLang();
    LOCALE = shellLocale();
  }
  await loadBounded(LANG);
}

function lookup(key) {
  // an empty translation counts as missing; another language's texts never show
  return (textsLang === LANG && texts[key]) || EN[key] || key;
}

/** The text of `key` with its {name} placeholders replaced from `vars`. */
export function t(key, vars) {
  let out = lookup(key);
  if (vars) out = out.replace(/\{(\w+)\}/g, (m, k) => (k in vars ? vars[k] : m));
  return out;
}

/**
 * Key combinations written with the English key names ("Ctrl+Shift+Del / Enter",
 * "F3, Shift+F3") in the interface's language: every name with a files.key.* text.
 */
export function keyText(combo) {
  return String(combo).replace(/[^+/,\s]+/g, (k) => (EN[`files.key.${k.toLowerCase()}`] ? t(`files.key.${k.toLowerCase()}`) : k));
}

/**
 * The text of an error code of the rynas API (ApiError.code), or "" when
 * the code has none here (the server's message is shown instead).
 */
const ERRORS = {
  unauthenticated: "files.err.session_expired_sign_in_again",
  csrf: "files.err.session_changed_in_another_tab",
  forbidden: "files.err.not_allowed",
  permission_denied: "files.err.not_allowed",
  not_found: "files.err.not_found_may_been_moved",
  exists: "files.common.item_name_already_exists",
  already_exists: "files.common.item_name_already_exists",
  conflict: "files.err.file_changed_in_meantime",
  busy: "files.err.busy_try_again_shortly",
  invalid_name: "files.common.invalid_name",
  invalid_value: "files.err.invalid_value",
  bad_request: "files.err.invalid_request",
  read_only: "files.err.read_only",
  payload_too_large: "files.err.too_large",
  too_large: "files.err.too_large",
  no_space: "files.err.volume_full",
  insufficient_storage: "files.err.volume_full",
  quota_exceeded: "files.err.quota_exceeded",
  rate_limited: "files.err.too_many_requests_try_again",
  not_supported: "files.err.not_available_here",
  remote_unreachable: "files.err.remote_connection_unreachable",
  backend_unavailable: "files.err.file_service_not_answer",
  bad_gateway: "files.err.file_service_not_answer",
  timeout: "files.err.file_service_did_not_answer",
  internal: "files.err.internal_error_on_nas",
  network: "files.common.nas_unreachable",
  share_root: "files.err.shared_folder_cannot_moved_renamed",
  outside_share: "files.err.link_leads_out_shared_folder",
  password_required: "files.err.archive_needs_password",
  tool_missing: "files.err.program_needed_missing_on_nas",
  remote_auth_failed: "files.err.remote_server_refused_login",
  host_key_changed: "files.err.remote_server_key_changed",
  unsupported_media_type: "files.err.format_not_supported",
  not_available: "files.err.not_available_on_system",
};

export function errText(code) {
  return code && ERRORS[code] ? t(ERRORS[code]) : "";
}

// made once per locale: count() runs for every row of the status bar and info panel
let rules = null;
let rulesLocale = null;

/** The CLDR plural category of n in the interface's locale: "one", "few", "many", "other"… */
export function pluralCategory(n) {
  if (rulesLocale !== LOCALE) {
    rules = new Intl.PluralRules(LOCALE);
    rulesLocale = LOCALE;
  }
  return rules.select(n);
}

/**
 * "1 file" / "3 files": the form `key.<category>` of the CLDR plural category
 * of n (as the core's tn() picks it), else `key.other`, with {n} as fmt(n).
 */
export function count(n, key, fmt = (x) => x) {
  const cat = pluralCategory(n);
  const form = texts[`${key}.${cat}`] || texts[`${key}.other`] || EN[`${key}.${cat}`] || EN[`${key}.other`] || key;
  return form.replace(/\{n\}/g, fmt(n));
}
