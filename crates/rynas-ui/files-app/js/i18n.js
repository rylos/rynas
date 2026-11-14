// The interface speaks the language of the rynas shell: Italian or English,
// read from <html lang> (set by the shell from the user's choice) and
// followed when it changes. Strings are written in Italian in the code;
// ui/js/lang/en.js maps them to English. A string the table lacks shows
// the Italian original (the test files_texts_are_translated in rynas-ui
// keeps the table complete).

import EN from "./lang/en.js";

// One locale for the whole interface: the shell writes it on <html lang> ("en-GB",
// "it-IT"…, from the language and the browser's own tags) and the time format
// preference on <html data-hour-cycle> ("h23", "h12", absent = the locale's).
// Without the shell: the same rule from the browser (rynas-ui i18n::locale_for).
const TAG = /^[A-Za-z]{2,3}(-[A-Za-z0-9]{1,8})+$/;
function localeFor(lang) {
  const tags = [...(navigator.languages || []), navigator.language || ""];
  return tags.find((t) => TAG.test(t) && t.split("-")[0].toLowerCase() === lang) || (lang === "it" ? "it-IT" : "en-US");
}
function shellLocale() {
  const tag = String(document.documentElement.lang || "");
  if (TAG.test(tag)) return tag;
  const lang = String(tag || navigator.language || "en").toLowerCase().startsWith("it") ? "it" : "en";
  return localeFor(lang);
}
const shellHourCycle = () => (["h23", "h12"].includes(document.documentElement.dataset.hourCycle) ? document.documentElement.dataset.hourCycle : "");

/** For Intl: dates, numbers, plural forms, sorting. */
export let LOCALE = shellLocale();
/** "it" or "en": the texts' language. */
export let LANG = LOCALE.toLowerCase().startsWith("it") ? "it" : "en";
/** The time format preference: "h23", "h12" or "" (the locale's). */
export let HOUR_CYCLE = shellHourCycle();

const listeners = new Set();
/** fn(): the shell switched language, locale or time format. */
export function onLangChange(fn) {
  listeners.add(fn);
}

function follow() {
  const locale = shellLocale();
  const hc = shellHourCycle();
  if (locale === LOCALE && hc === HOUR_CYCLE) return;
  LOCALE = locale;
  LANG = LOCALE.toLowerCase().startsWith("it") ? "it" : "en";
  HOUR_CYCLE = hc;
  listeners.forEach((fn) => fn());
}
new MutationObserver(follow).observe(document.documentElement, { attributes: true, attributeFilter: ["lang", "data-hour-cycle"] });

/** Kept for the start sequence of main.js: the table is bundled, nothing to load. */
export async function loadTable() {}

// A key may carry a context after "|" ("Copia|op", "file|pl"): Italian
// shows what comes before it; the table translates each context apart.
const plain = (key) => key.split("|")[0];

function lookup(key) {
  if (LANG === "it") return plain(key);
  // an empty translation counts as missing
  return EN[key] || plain(key);
}

export function t(text, vars) {
  let out = lookup(text);
  // a plural table asked as a plain string: its general form
  if (typeof out === "object") out = out.other || out.many || Object.values(out)[0];
  if (vars) out = out.replace(/\{(\w+)\}/g, (m, k) => (k in vars ? vars[k] : m));
  return out;
}

/**
 * The text of an error code of the rynas API (ApiError.code), or "" when
 * the code has none here (the server's message is shown instead).
 */
const ERRORS = {
  unauthenticated: "Sessione scaduta: accedi di nuovo",
  csrf: "La sessione è cambiata in un'altra scheda: ricarica la pagina",
  forbidden: "Non hai il permesso per questa operazione",
  permission_denied: "Non hai il permesso per questa operazione",
  not_found: "Non trovato: forse è stato spostato o eliminato",
  exists: "Esiste già un elemento con questo nome",
  already_exists: "Esiste già un elemento con questo nome",
  conflict: "Il file è cambiato nel frattempo",
  busy: "Occupato: riprova tra poco",
  invalid_name: "Nome non valido",
  invalid_value: "Valore non valido",
  bad_request: "Richiesta non valida",
  read_only: "Sola lettura",
  payload_too_large: "Troppo grande",
  too_large: "Troppo grande",
  no_space: "Spazio esaurito sul volume",
  insufficient_storage: "Spazio esaurito sul volume",
  quota_exceeded: "Quota superata",
  rate_limited: "Troppe richieste: riprova tra poco",
  not_supported: "Non disponibile qui",
  remote_unreachable: "Connessione remota non raggiungibile",
  backend_unavailable: "Il servizio dei file non risponde",
  bad_gateway: "Il servizio dei file non risponde",
  timeout: "Il servizio dei file non ha risposto in tempo",
  internal: "Errore interno del server",
  network: "Server non raggiungibile",
  share_root: "Una cartella condivisa non si può spostare, rinominare o eliminare da qui",
  outside_share: "Questo collegamento porta fuori dalla cartella condivisa: non viene seguito",
  password_required: "Serve la password dell'archivio",
  tool_missing: "Manca il programma necessario sul NAS",
  remote_auth_failed: "Accesso al server remoto rifiutato",
  host_key_changed: "La chiave del server remoto è cambiata",
  unsupported_media_type: "Formato non supportato",
  not_available: "Non disponibile su questo sistema",
};

export function errText(code) {
  return code && ERRORS[code] ? t(ERRORS[code]) : "";
}

/** Every Italian key of the error table (for the tests of the texts). */
export const ERROR_KEYS = Object.values(ERRORS);

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
 * "1 file" / "3 file", "1 cartella" / "3 cartelle": the form of the CLDR
 * plural category of n, as the core's tn() picks `key.<category>`. The
 * Italian code passes its two forms; the English table may give a plain
 * text or an object of forms ({ one, few, many, other }, other required).
 * When Italian uses one word for both, the plural key is `word|pl`.
 */
export function count(n, one, many, fmt = (x) => x) {
  const form = pluralCategory(n);
  if (form === "one") return `${fmt(n)} ${t(one)}`;
  const key = one === many ? `${many}|pl` : many;
  const value = lookup(key);
  const word = typeof value === "object" ? value[form] || value.other : value;
  return `${fmt(n)} ${word}`;
}