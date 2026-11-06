// The interface speaks the language of the rynas shell: Italian or English,
// read from <html lang> (set by the shell from the user's choice) and
// followed when it changes. Strings are written in Italian in the code;
// ui/js/lang/en.js maps them to English. A string the table lacks shows
// the Italian original (the test files_texts_are_translated in rynas-ui
// keeps the table complete).

import EN from "./lang/en.js";

const shellLang = () => (String(document.documentElement.lang || navigator.language || "en").toLowerCase().startsWith("it") ? "it" : "en");

/** "it" or "en". */
export let LANG = shellLang();
/** For Intl: dates, numbers, sorting. */
export let LOCALE = LANG === "it" ? "it-IT" : "en-GB";

const listeners = new Set();
/** fn(): the shell switched language. */
export function onLangChange(fn) {
  listeners.add(fn);
}

function follow() {
  const next = shellLang();
  if (next === LANG) return;
  LANG = next;
  LOCALE = LANG === "it" ? "it-IT" : "en-GB";
  listeners.forEach((fn) => fn());
}
new MutationObserver(follow).observe(document.documentElement, { attributes: true, attributeFilter: ["lang"] });

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
};

export function errText(code) {
  return code && ERRORS[code] ? t(ERRORS[code]) : "";
}

/** Every Italian key of the error table (for the tests of the texts). */
export const ERROR_KEYS = Object.values(ERRORS);

// made once per language: count() runs for every row of the status bar and info panel
let rules = null;
let rulesLocale = null;

/**
 * "1 file" / "3 file", "1 cartella" / "3 cartelle". When Italian uses one
 * word for both, the plural key is `word|pl`.
 */
export function count(n, one, many, fmt = (x) => x) {
  if (rulesLocale !== LOCALE) {
    rules = new Intl.PluralRules(LOCALE);
    rulesLocale = LOCALE;
  }
  const form = rules.select(n);
  if (form === "one" || (LANG === "it" && n === 1)) return `${fmt(n)} ${t(one)}`;
  const key = one === many ? `${many}|pl` : many;
  const value = lookup(key);
  const word = typeof value === "object" ? value[form] || value.other || value.many : value;
  return `${fmt(n)} ${word}`;
}
