// Numbers, sizes, dates and durations in the interface's locale and time format
// (i18n.js LOCALE, HOUR_CYCLE), with the same rules and unit words as the core's
// formatting (rynas-ui i18n.rs: bytes, duration, elapsed, ago, of, pct; the test
// files_formats_match_core keeps the texts equal).
import { LOCALE, HOUR_CYCLE, t, count } from "./i18n.js";

export const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

// made on first use and again when the locale or the time format changes
let made = null;
function intl() {
  const key = `${LOCALE}|${HOUR_CYCLE}`;
  if (made?.key === key) return made;
  const fixed = (d) => new Intl.NumberFormat(LOCALE, { minimumFractionDigits: d, maximumFractionDigits: d });
  made = {
    key,
    nf: [fixed(0), fixed(1), fixed(2)],
    pct: [0, 1].map((d) => new Intl.NumberFormat(LOCALE, { style: "percent", minimumFractionDigits: d, maximumFractionDigits: d })),
    collator: new Intl.Collator(LOCALE, { numeric: true, sensitivity: "base" }),
    dates: new Map(),
  };
  return made;
}
export const int = (n) => intl().nf[0].format(n || 0);

/** Fixed decimals in the interface's locale. */
export const num = (v, decimals = 0) => intl().nf[Math.min(2, decimals)].format(v || 0);

/** A fraction (0–1) as a percentage: one decimal below 10 % ("4.5%", "45%"), like the core's pct. */
export const pct = (x) => intl().pct[x < 0.1 ? 1 : 0].format(x || 0);

/**
 * An Intl.DateTimeFormat in the interface's locale, made once per option set;
 * the time format preference is added when the options show a time.
 */
export function dateFormat(opts) {
  const { dates } = intl();
  const k = JSON.stringify(opts);
  if (!dates.has(k)) {
    const withTime = "timeStyle" in opts || "hour" in opts;
    dates.set(k, new Intl.DateTimeFormat(LOCALE, withTime && HOUR_CYCLE ? { ...opts, hourCycle: HOUR_CYCLE } : opts));
  }
  return dates.get(k);
}

let binary = true;
export function setBinary(b) {
  binary = b;
}
/** Sizes in 1024ⁿ (KiB…) rather than 1000ⁿ (kB…). */
export const isBinary = () => binary;

const UNITS_BIN = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
const UNITS_DEC = ["B", "kB", "MB", "GB", "TB", "PB"];

/** Three significant digits (decided on the rounded value), like the core's sig3. */
const sig3 = (v) => intl().nf[v < 9.995 ? 2 : v < 99.95 ? 1 : 0].format(v);

/**
 * [number text, unit, magnitude index], three significant digits: the next
 * unit as soon as the number would need four ("1.00 GiB", never "1022 MiB").
 */
export function sizeParts(bytes) {
  const base = binary ? 1024 : 1000;
  const units = binary ? UNITS_BIN : UNITS_DEC;
  const n = bytes || 0;
  if (n < 1000) return [intl().nf[0].format(n), units[0], 0];
  let v = n;
  let u = 0;
  while (v >= 999.5 && u < units.length - 1) {
    v /= base;
    u++;
  }
  return [sig3(v), units[u], u];
}

export function size(bytes) {
  const [n, u] = sizeParts(bytes);
  return `${n} ${u}`;
}

export function speed(bps) {
  return `${size(bps)}/s`;
}

/** "X of Y": a part of a whole (core: i18n::of). */
export const of = (part, total) => t("{part} di {total}", { part, total });

/** The date column: day, month and time this half year, day, month and year before. */
export function ezaDate(sec) {
  if (!sec) return "-";
  const d = new Date(sec * 1000);
  const recent = Math.abs(Date.now() - d) < 180 * 86400 * 1000;
  return dateFormat(recent ? { day: "numeric", month: "short", hour: "numeric", minute: "2-digit" } : { day: "numeric", month: "short", year: "numeric" }).format(d);
}

/** Date and time with seconds: "13 nov 2026, 17:05:09" / "Nov 13, 2026, 5:05:09 PM". */
export function dateTime(sec) {
  if (!sec) return "-";
  return dateFormat({ dateStyle: "medium", timeStyle: "medium" }).format(new Date(sec * 1000));
}

/** Time of day: "17:05" / "5:05 PM". */
export function clock(ms) {
  return dateFormat({ timeStyle: "short" }).format(new Date(ms));
}

/** The largest unit of a duration in seconds: "42 s", "5 min", "3 h", "2 g" (core: i18n::approx). */
export function approx(secs) {
  const s = Math.max(0, Math.floor(secs));
  if (s >= 86400) return t("{d} g", { d: int(Math.floor(s / 86400)) });
  if (s >= 3600) return t("{h} h", { h: Math.floor(s / 3600) });
  if (s >= 60) return t("{m} min", { m: Math.floor(s / 60) });
  return t("{s} s", { s });
}

/** "5 min fa" / "5 min ago" (core: i18n::ago). */
export const ago = (secs) => t("{time} fa", { time: approx(secs) });

export function relative(sec) {
  if (!sec) return "-";
  const diff = Date.now() / 1000 - sec;
  if (diff < 60) return t("adesso");
  if (diff < 86400 * 7) return ago(diff);
  return ezaDate(sec);
}

/** 1 h 02 min, 3 min 05 s, 42 s (core: i18n::elapsed). */
export function duration(ms) {
  if (ms == null || !isFinite(ms)) return "—";
  let s = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(s / 3600);
  s -= h * 3600;
  const m = Math.floor(s / 60);
  s -= m * 60;
  if (h) return t("{h} h {m} min", { h: int(h), m: pad(m) });
  if (m) return t("{m} min {s} s", { m, s: pad(s) });
  return t("{s} s", { s });
}

const pad = (n) => String(n).padStart(2, "0");

export function shortDuration(ms) {
  let s = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  s = s % 60;
  return h ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/** The mode column the way `ls -l` and eza print it. */
export function modeString(kind, mode) {
  const t = kind === "d" ? "d" : kind === "l" ? "l" : kind === "o" ? "s" : ".";
  const bits = [
    [0o400, "r"], [0o200, "w"], [0o100, "x"],
    [0o040, "r"], [0o020, "w"], [0o010, "x"],
    [0o004, "r"], [0o002, "w"], [0o001, "x"],
  ];
  let out = t;
  bits.forEach(([bit, ch], i) => {
    let c = mode & bit ? ch : "-";
    if (ch === "x") {
      const special = i === 2 ? 0o4000 : i === 5 ? 0o2000 : 0o1000;
      if (mode & special) c = mode & bit ? (i === 8 ? "t" : "s") : i === 8 ? "T" : "S";
    }
    out += c;
  });
  return out;
}

export function plural(n, one, many) {
  return count(n, one, many, int);
}

export function joinPath(dir, name) {
  return dir.replace(/\/+$/, "") + "/" + name;
}

export function parentOf(path) {
  const parts = path.split("/").filter(Boolean);
  parts.pop();
  return "/" + parts.join("/");
}

export function baseName(path) {
  const parts = path.split("/").filter(Boolean);
  return parts[parts.length - 1] || "";
}

export function ext(name) {
  const i = name.lastIndexOf(".");
  return i > 0 ? name.slice(i + 1).toLowerCase() : "";
}

/** A folder, or a link to one: what opens on a double click. */
export const isDirLike = (e) => e.kind === "d" || !!e.link_dir;

/** Natural order: "file2" before "file10", accents and case folded. */
export const collator = { compare: (a, b) => intl().collator.compare(a, b) };
