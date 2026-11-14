//! Tiny i18n: English (the source language, built into the interface and the fallback) and
//! every language of the table `assets/i18n/languages.txt` (docs/i18n.md), plus
//! number/byte/date/duration formatting in one locale for the whole interface.
//!
//! The language lives in a signal, so `t()` called inside a reactive closure re-renders when the
//! user switches language. Preference: localStorage `rynas.lang` (a code of the table, or absent
//! = the browser's: the first of its languages the table has, English otherwise; a stored code
//! the table no longer has counts as absent). The locale (`en-GB`, `en-US`, `it-IT`…) comes from
//! the language and the browser's own tags ([`locale`]); the time format preference is
//! localStorage `rynas.hour` ("h23", "h12" or absent = the locale's).
//!
//! Only English is compiled in. The other languages' texts are the files
//! `assets/i18n/<code>/<part>.txt` (`key<TAB>text`, one per part: core, modules, File app),
//! bundled per language at build time into `/i18n/<code>.txt` (`i18n-bundle.sh`, the parts the
//! wasm reads) and fetched when the language is chosen. The test build reads every file, so
//! every test sees every language.
//!
//! Italian wording uses the common NAS vocabulary (archiviazione, cartella condivisa, volume,
//! integro/degradato…); no product or app names of other systems (test `no_other_products`).

use std::cell::{Cell, RefCell};

use leptos::prelude::*;

use crate::prefs::{storage_get, storage_set};

/// One language of the table `assets/i18n/languages.txt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Language {
    /// BCP 47 tag, also the folder of its translation files.
    pub code: String,
    /// As the language names itself (shown so in every language).
    pub name: String,
    /// Dates and numbers when the browser names no region of this language.
    pub locale: String,
    /// CLDR plural categories of whole numbers: the forms its `key.<category>` texts need.
    pub plurals: Vec<String>,
    /// Its name in English (the command palette finds it by this name too).
    pub english: String,
}

/// The table shipped with the interface.
const LANGUAGES: &str = include_str!("../assets/i18n/languages.txt");

/// The `key<TAB>text` lines of a translation file: lines starting with `#` (comments) and
/// lines without a tab are skipped.
pub(crate) fn translation_lines(file: &str) -> impl Iterator<Item = (&str, &str)> {
    file.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once('\t'))
}

/// Reads a language table: `code<TAB>native name<TAB>default locale<TAB>plural categories<TAB>
/// English name`.
pub(crate) fn parse_languages(text: &str) -> Vec<Language> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t').map(str::trim);
            let (code, name, locale) = (f.next()?, f.next()?, f.next()?);
            let mut plurals: Vec<String> = f.next().unwrap_or("").split_whitespace().map(String::from).collect();
            if !plurals.iter().any(|p| p == "other") {
                plurals.push("other".into());
            }
            let english = f.next().filter(|e| !e.is_empty()).unwrap_or(name).to_string();
            (!code.is_empty() && !name.is_empty() && !locale.is_empty()).then(|| Language { code: code.into(), name: name.into(), locale: locale.into(), plurals, english })
        })
        .collect()
}

/// The languages of the interface, English (the source) first.
pub fn languages() -> Vec<Language> {
    parse_languages(LANGUAGES)
}

/// The table [`Lang`] indexes. The test build adds the made-up languages of
/// `tests/fixtures/i18n/languages.txt`: the mechanism works for any language of the table.
fn table() -> &'static [Language] {
    thread_local! {
        static TABLE: &'static [Language] = {
            #[allow(unused_mut)]
            let mut t = languages();
            #[cfg(test)]
            t.extend(parse_languages(include_str!("../tests/fixtures/i18n/languages.txt")));
            t.truncate(256);
            Box::leak(t.into_boxed_slice())
        };
    }
    TABLE.with(|t| *t)
}

/// A language of the table (its row; English is the first).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Lang(u8);

impl Lang {
    /// English: the source language and the fallback of every text.
    pub const EN: Lang = Lang(0);

    fn info(self) -> &'static Language {
        &table()[usize::from(self.0)]
    }

    /// Its code (`en`, `it`): stored preferences, file names, `<html data-lang>`.
    pub fn code(self) -> &'static str {
        &self.info().code
    }

    /// Its own name ("English", "Italiano").
    pub fn name(self) -> &'static str {
        &self.info().name
    }

    /// Its name in English ("Italian").
    pub fn english(self) -> &'static str {
        &self.info().english
    }

    /// The language of a code of the table (case does not matter).
    pub fn from_code(code: &str) -> Option<Lang> {
        let code = code.trim();
        table().iter().position(|l| l.code.eq_ignore_ascii_case(code)).map(|i| Lang(i as u8))
    }

    /// Every language of the table, English first.
    pub fn all() -> impl Iterator<Item = Lang> {
        (0..table().len()).map(|i| Lang(i as u8))
    }
}

thread_local! {
    static LANG: Cell<Option<RwSignal<Lang>>> = const { Cell::new(None) };
}

fn sig() -> RwSignal<Lang> {
    LANG.with(|c| match c.get() {
        Some(s) => s,
        None => {
            let s = RwSignal::new(Lang::EN);
            c.set(Some(s));
            s
        }
    })
}

/// The first subtag of a language tag, lowercase (`it-CH` → `it`).
fn primary(tag: &str) -> String {
    tag.trim().split('-').next().unwrap_or("").to_ascii_lowercase()
}

/// The language of the table a browser tag asks for: the same code (`pt-BR`), else the
/// language of that code (`it-CH` → `it`), else one of the same language (`pt` → `pt-BR`).
fn lang_for_tag(tag: &str) -> Option<Lang> {
    let p = primary(tag);
    if p.is_empty() {
        return None;
    }
    Lang::from_code(tag).or_else(|| Lang::from_code(&p)).or_else(|| Lang::all().find(|l| primary(l.code()) == p))
}

/// The first of the browser's languages (most preferred first) the table has, else English.
fn lang_for_tags(tags: &[String]) -> Lang {
    tags.iter().find_map(|t| lang_for_tag(t)).unwrap_or(Lang::EN)
}

fn browser_lang() -> Lang {
    lang_for_tags(&browser_tags())
}

/// The stored preference when the table has it.
fn stored_lang() -> Option<Lang> {
    storage_get("rynas.lang").filter(|s| !s.is_empty()).and_then(|s| Lang::from_code(&s))
}

pub fn init() {
    let lang = stored_lang().unwrap_or_else(browser_lang);
    hour_sig().set(HourCycle::from_code(&storage_get("rynas.hour").unwrap_or_default()));
    publish(lang);
    sig().set(lang);
}

/// Works out the locale of `lang` and publishes the language (`<html data-lang>`, a code of the
/// table), the time format preference (`<html data-hour-cycle>`, absent = the locale's) and the
/// locale (`<html lang>`), where the File app reads them. Also remembers which texts
/// `index.html` should fetch early on the next visit (`rynas.lang.texts`).
fn publish(lang: Lang) {
    let loc = locale_for(lang, &browser_tags());
    LOCALE.with(|l| *l.borrow_mut() = loc.clone());
    // "en" too (never removed): only a first visit falls back to `rynas.lang`
    storage_set("rynas.lang.texts", lang.code());
    if let Some(el) = document().document_element() {
        // language and hour cycle first: the File app follows all three on the change of `lang`
        let _ = el.set_attribute("data-lang", lang.code());
        let _ = match hour_sig().get_untracked() {
            HourCycle::Auto => el.remove_attribute("data-hour-cycle"),
            h => el.set_attribute("data-hour-cycle", h.code()),
        };
        let _ = el.set_attribute("lang", &loc);
    }
}

/// Current language: tracked inside reactive closures, a plain read elsewhere (static labels).
pub fn lang() -> Lang {
    if leptos::reactive::graph::Observer::get().is_some() {
        sig().get()
    } else {
        sig().get_untracked()
    }
}

/// `None` = follow the browser.
pub fn set_lang(lang: Option<Lang>) {
    let l = lang.unwrap_or_else(browser_lang);
    storage_set("rynas.lang", lang.map(Lang::code).unwrap_or(""));
    // the locale before the signal: what re-renders formats with the new one
    publish(l);
    sig().set(l);
    // English meanwhile; the texts switch when the file is there
    load_texts(l, move || texts_arrived(l));
}

/// Stored preference: `None` means "automatic" (also for a code the table no longer has).
pub fn lang_pref() -> Option<Lang> {
    stored_lang()
}

/// `"key" => "English";` — the source text of every key. The other languages' texts are the
/// translation files `assets/i18n/<code>/<part>.txt` (docs/i18n.md); the test
/// `translations_are_complete` lists, per language, the keys missing there, the stale ones,
/// the placeholders that differ and the plural forms the language needs.
///
/// Keys follow `<area>.<thing>` (`sto.scrub`, `net.dns`); words shared by every area are
/// bare (`cancel`, `save`, `edit_named`, `run_now`) or in a shared group (`col.*` column
/// headers, `health.*`, `pending.*`, `task.st.*`). One key per text: a test fails on two keys
/// with the same text of more than one word in every language. A text with plural forms is
/// `key.one` / `key.other` (`tn`), `other` required.
///
/// The core table is below; a hardware module has its own table in its folder
/// (`crate::modules::<module>::i18n`, same macro, D-MODULE) and its own translation file
/// (`module-<module>.txt`), read together with this one.
macro_rules! dict {
    ($blob:ident, $lens:ident; $($k:literal => $en:literal;)*) => {
        /// Every key and English text back to back with their byte lengths: much smaller in
        /// the wasm than a `match` over 3 000 strings or a table of string pointers.
        pub(crate) static $blob: &str = concat!($($k, $en),*);
        pub(crate) static $lens: &[u16] = &[$($k.len() as u16, $en.len() as u16),*];
    };
}
pub(crate) use dict;

/// The tables: the core one first, then the modules'.
fn tables() -> [(&'static str, &'static [u16]); 3] {
    [(BLOB, LENS), (crate::modules::synology::i18n::BLOB, crate::modules::synology::i18n::LENS), (crate::modules::qnap::i18n::BLOB, crate::modules::qnap::i18n::LENS)]
}

/// The translation file (part) of each of [`tables`], in the same order. The build bundles
/// them per language into `/i18n/<code>.txt` (`i18n-bundle.sh`: every part of a language but
/// `files` and `notify`); the test build reads them one by one.
#[cfg(test)]
pub(crate) const PARTS: [&str; 3] = ["core", "module-synology", "module-qnap"];

/// Start of every text of every table (and the end), the entries (table, index) sorted by
/// key, built on first use, and the other languages' texts once they are there.
struct Index {
    blobs: [&'static str; 3],
    starts: [Vec<u32>; 3],
    by_key: Vec<(u8, u16)>,
    /// `[language][table][entry]`, "" = not translated (the English text shows).
    translated: RefCell<Vec<[Vec<&'static str>; 3]>>,
    /// `[language]`: plural forms English does not have (`key.few`, `key.many`…).
    forms: RefCell<Vec<std::collections::HashMap<&'static str, &'static str>>>,
}

impl Index {
    fn text(&self, t: usize, i: usize) -> &'static str {
        &self.blobs[t][self.starts[t][i] as usize..self.starts[t][i + 1] as usize]
    }

    /// (table, entry) of `key`.
    fn find(&self, key: &str) -> Option<(usize, usize)> {
        let at = self.by_key.binary_search_by(|&(t, e)| self.text(t as usize, e as usize * 2).cmp(key)).ok()?;
        let (t, e) = self.by_key[at];
        Some((t as usize, e as usize))
    }

    /// Takes the texts of a translation file of `lang` (`key<TAB>text` lines, any part);
    /// unknown keys are skipped, missing ones stay English. Returns the number of texts taken.
    fn install(&self, lang: Lang, file: &'static str) -> usize {
        let mut tr = self.translated.borrow_mut();
        let Some(tables) = tr.get_mut(usize::from(lang.0)) else { return 0 };
        for (t, v) in tables.iter_mut().enumerate() {
            if v.is_empty() {
                v.resize((self.starts[t].len() - 1) / 2, "");
            }
        }
        let mut forms = self.forms.borrow_mut();
        let mut n = 0;
        for (k, text) in translation_lines(file) {
            if let Some((t, e)) = self.find(k) {
                tables[t][e] = text;
                n += 1;
            } else if let Some((base, cat)) = k.rsplit_once('.')
                && matches!(cat, "zero" | "two" | "few" | "many")
                && self.find(&format!("{base}.one")).is_some()
                && self.find(&format!("{base}.other")).is_some()
                && let Some(f) = forms.get_mut(usize::from(lang.0))
            {
                f.insert(k, text);
                n += 1;
            }
        }
        n
    }
}

/// A translation file read from the repository (test build): the shipped ones, then the
/// made-up languages of the fixtures.
#[cfg(test)]
fn test_file(code: &str, part: &str) -> Option<&'static str> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = [root.join("assets/i18n"), root.join("tests/fixtures/i18n")].iter().find_map(|d| std::fs::read_to_string(d.join(code).join(format!("{part}.txt"))).ok())?;
    Some(Box::leak(text.into_boxed_str()))
}

thread_local! {
    static INDEX: Index = {
        let tables = tables();
        let starts = tables.map(|(_, lens)| {
            let mut starts = Vec::with_capacity(lens.len() + 1);
            let mut at = 0u32;
            starts.push(0);
            for &l in lens {
                at += u32::from(l);
                starts.push(at);
            }
            starts
        });
        let by_key = tables.iter().enumerate().flat_map(|(t, (_, lens))| (0..(lens.len() / 2) as u16).map(move |e| (t as u8, e))).collect();
        let translated = Lang::all().map(|_| [Vec::new(), Vec::new(), Vec::new()]).collect();
        let forms = Lang::all().map(|_| std::collections::HashMap::new()).collect();
        let mut idx = Index { blobs: tables.map(|t| t.0), starts, by_key, translated: RefCell::new(translated), forms: RefCell::new(forms) };
        let mut by_key = std::mem::take(&mut idx.by_key);
        by_key.sort_unstable_by_key(|&(t, e)| idx.text(t as usize, e as usize * 2));
        idx.by_key = by_key;
        #[cfg(test)]
        for l in Lang::all().skip(1) {
            for part in PARTS {
                if let Some(f) = test_file(l.code(), part) {
                    idx.install(l, f);
                }
            }
        }
        idx
    };
    /// Per language: 0 not asked, 1 being fetched, 2 there.
    static LOADED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// The text of `key`; a language falls back to English while (or where) its file has no text
/// for it.
pub(crate) fn lookup(key: &str, lang: Lang) -> Option<&'static str> {
    INDEX.with(|idx| {
        let Some((t, e)) = idx.find(key) else {
            // a plural form only the language has
            return if lang == Lang::EN { None } else { idx.forms.borrow().get(usize::from(lang.0)).and_then(|f| f.get(key).copied()) };
        };
        if lang != Lang::EN
            && let Some(s) = idx.translated.borrow().get(usize::from(lang.0)).and_then(|x| x[t].get(e).copied()).filter(|s| !s.is_empty())
        {
            return Some(s);
        }
        Some(idx.text(t, e * 2 + 1))
    })
}

fn load_state(lang: Lang) -> u8 {
    LOADED.with(|l| l.borrow().get(usize::from(lang.0)).copied().unwrap_or(0))
}

fn set_load_state(lang: Lang, s: u8) {
    LOADED.with(|l| {
        let mut l = l.borrow_mut();
        let i = usize::from(lang.0);
        if l.len() <= i {
            l.resize(i + 1, 0);
        }
        l[i] = s;
    });
}

/// How long the boot screen waits for the texts of the language before the interface shows in
/// English (it switches as soon as the file arrives).
const TEXTS_WAIT_MS: u64 = 1200;

/// The texts of `lang`: the request `index.html` started while the wasm was loading
/// (`window.__rynasTexts = { code, text }`) when it asked for this language, or a new one.
async fn fetch_texts(lang: Lang) -> Option<String> {
    use wasm_bindgen::JsCast;
    let w = window();
    let key = wasm_bindgen::JsValue::from_str("__rynasTexts");
    let mut early = None;
    if let Some(e) = js_sys::Reflect::get(&w, &key).ok().filter(|v| v.is_object()) {
        let _ = js_sys::Reflect::delete_property(&w, &key);
        if js_sys::Reflect::get(&e, &"code".into()).ok().and_then(|c| c.as_string()).as_deref() == Some(lang.code()) {
            early = js_sys::Reflect::get(&e, &"text".into()).ok().and_then(|p| p.dyn_into::<js_sys::Promise>().ok());
        }
    }
    let text = match early {
        Some(p) => wasm_bindgen_futures::JsFuture::from(p).await.ok(),
        None => {
            let resp = wasm_bindgen_futures::JsFuture::from(w.fetch_with_str(&format!("/i18n/{}.txt", lang.code()))).await.ok()?;
            let resp: web_sys::Response = resp.dyn_into().ok()?;
            // a translation is plain text: anything else (an error page) is no texts
            let plain = resp.headers().get("content-type").ok().flatten().is_some_and(|t| t.starts_with("text/plain"));
            if !resp.ok() || !plain {
                return None;
            }
            wasm_bindgen_futures::JsFuture::from(resp.text().ok()?).await.ok()
        }
    };
    text?.as_string().filter(|s| !s.is_empty())
}

/// Fetches the texts of `lang` once (nothing for English); `done` runs when they are there (or
/// the fetch failed).
fn load_texts(lang: Lang, done: impl FnOnce() + 'static) {
    if lang == Lang::EN || load_state(lang) != 0 {
        return;
    }
    set_load_state(lang, 1);
    // before the mount: the reactive executor is not set up yet
    wasm_bindgen_futures::spawn_local(async move {
        let n = match fetch_texts(lang).await {
            Some(f) => INDEX.with(|idx| idx.install(lang, Box::leak(f.into_boxed_str()))),
            None => 0,
        };
        // none: a later switch to the language tries again
        set_load_state(lang, if n > 0 { 2 } else { 0 });
        done();
    });
}

/// The texts of `lang` arrived after the interface rendered: render its texts again.
fn texts_arrived(lang: Lang) {
    if load_state(lang) == 2 && sig().get_untracked() == lang {
        sig().update(|_| {});
    }
}

/// Runs `mount` once the texts of the language are there: at once in English, after its file
/// (at most [`TEXTS_WAIT_MS`]) in another language.
pub fn when_ready(mount: fn()) {
    let lang = sig().get_untracked();
    if lang == Lang::EN || load_state(lang) == 2 {
        mount();
        return;
    }
    let mounted = std::rc::Rc::new(Cell::new(false));
    let m = mounted.clone();
    load_texts(lang, move || {
        if m.replace(true) {
            texts_arrived(lang);
        } else {
            mount();
        }
    });
    set_timeout(
        move || {
            if !mounted.replace(true) {
                mount();
            }
        },
        std::time::Duration::from_millis(TEXTS_WAIT_MS),
    );
}

/// The text of `key` in `lang`, for the tests of the pages.
#[cfg(test)]
pub fn text_in(lang: Lang, key: &str) -> Option<&'static str> {
    lookup(key, lang)
}

/// The entries (key, English) of each table with its part name, for the tests.
#[cfg(test)]
pub(crate) fn parts() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    INDEX.with(|idx| (0..idx.starts.len()).map(|t| (PARTS[t], (0..(idx.starts[t].len() - 1) / 2).map(|e| (idx.text(t, e * 2), idx.text(t, e * 2 + 1))).collect())).collect())
}

/// Every entry as (key, English), for the tests.
#[cfg(test)]
pub(crate) fn entries() -> Vec<(&'static str, &'static str)> {
    parts().into_iter().flat_map(|p| p.1).collect()
}

/// Every text of `key`: English, then each language that translates it.
#[cfg(test)]
pub(crate) fn texts_of(key: &str) -> Vec<&'static str> {
    Lang::all().filter_map(|l| lookup(key, l)).collect()
}

/// Translate `key` in the current language (tracked). Unknown keys come back verbatim, so a
/// missing entry is visible but never breaks the page.
pub fn t(key: &'static str) -> &'static str {
    lookup(key, lang()).unwrap_or(key)
}

/// Reactive translated text for a view (child or attribute): `{tr("key")}`. Every call returns
/// the same closure type, so the rendering code is compiled once instead of once per call site
/// (a `move || t("key")` at each site costs about 1 KB of wasm).
pub fn tr(key: &'static str) -> impl Fn() -> &'static str + Copy + Send + Sync + 'static {
    move || t(key)
}

/// Text for an `ApiError.code` (`err.<code>`), generic message for unknown codes.
pub fn err_text(code: &str) -> &'static str {
    lookup(&format!("err.{code}"), lang()).unwrap_or_else(|| t("err.generic"))
}

/// Like `t` for keys built at runtime (e.g. `led.{state}`): None when the key does not exist.
pub fn try_t(key: &str) -> Option<&'static str> {
    lookup(key, lang())
}

/// `t(key)` with `{name}` placeholders replaced, e.g. `tf("n.temp", &[("label", "CPU")])`.
pub fn tf(key: &'static str, args: &[(&str, &str)]) -> String {
    let mut s = t(key).to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

dict! { BLOB, LENS;
    // navigation
    "nav.dashboard" => "Dashboard";
    "nav.storage" => "Storage";
    "nav.files" => "Files";
    "nav.shares" => "Shared folders";
    "nav.users" => "Users and groups";
    "nav.services" => "Services";
    "nav.apps" => "Apps and plugins";
    "nav.hardware" => "Hardware";
    "nav.system" => "System";
    "nav.menu" => "Menu";
    "nav.main" => "Main navigation";
    "nav.collapse" => "Collapse sidebar";
    "nav.expand" => "Expand sidebar";
    "nav.project" => "The project on GitHub";
    "nav.notifications" => "Notifications";
    // app bar
    "bar.search" => "Search or run a command…";
    "bar.theme" => "Theme";
    "bar.user" => "Account menu";
    "bar.open_menu" => "Open menu";
    "bar.demo" => "Demo data";
    "bar.demo_hint" => "The API is not responding: showing demo data";
    "bar.live" => "Live";
    "bar.offline" => "Offline";
    "bar.connecting" => "Connecting…";
    "bar.logout" => "Sign out";
    "bar.prefs" => "Preferences";
    // notifications
    "notif.empty" => "You're all caught up";
    "notif.empty_text" => "Hardware alerts and system events will show up here.";
    "notif.mark_read" => "Mark all as read";
    "notif.unread" => "unread";
    "notif.since" => "since";
    "notif.fan_fail" => "Fan failure";
    "notif.fan_fail_text" => "{label} is not spinning or reports an alarm.";
    "notif.fan_fail_any" => "The hardware source reports a fan failure.";
    "notif.temp_warn" => "High temperature";
    "notif.temp_crit" => "Critical temperature";
    "notif.temp_text" => "{label} is at {value} (threshold {limit}).";
    "notif.array_degraded" => "RAID array degraded";
    "notif.array_degraded_text" => "{name} ({level}) is running without redundancy. Replace the failed disk as soon as possible.";
    "notif.array_failed" => "RAID array failed";
    "notif.array_failed_text" => "{name} ({level}) is inactive or has failed.";
    "notif.array_sync" => "RAID resync in progress";
    "notif.array_sync_text" => "{name} ({level}): {pct} complete.";
    "notif.volume_full" => "Volume almost full";
    "notif.volume_full_text" => "{mount} is {pct} full, {free} free.";
    "nt.src.storage" => "Storage monitor";
    "nt.src.hardware" => "Hardware monitor";
    "nt.src.auth" => "Sign-in";
    "nt.src.task" => "Tasks";
    "nt.src.config" => "Configuration";
    "nt.src.snapshots" => "Snapshots";
    "nt.src.power" => "Power";
    "nt.src.network" => "Network";
    "nt.src.notifications" => "Notifications";
    // server notification texts by event code: `nev.<code>.title|body` with `{field}`
    // placeholders; `…_2`/`…_3` are variants for optional (empty) fields, most specific
    // first; `nev.<code>.<field>.<value>` translates a field value of a closed set. A missing
    // key or field shows the server's English text (rynas_api_types::notify::event_text).
    // Code table: docs/phase1/plan.md, "Event codes". rynasd reads these lines for the
    // Italian channel messages, so keep one entry per line.
    // hardware
    "nev.hw.fan_stopped.title" => "Fan {fan} stopped";
    "nev.hw.fan_running.title" => "Fan {fan} is running again";
    "nev.hw.fan_failure.title" => "Fan failure";
    "nev.hw.fan_failure.body" => "The hardware controller reports a fan failure: check the fans before the disks overheat.";
    "nev.hw.fans_ok.title" => "Fans back to normal";
    "nev.hw.monitor_stale.title" => "Hardware monitor not updating";
    "nev.hw.monitor_stale.body" => "The hardware source ({provider}) has not written its status for {age_s} s: fan and temperature alarms do not work.";
    "nev.hw.monitor_ok.title" => "Hardware monitor updating again";
    "nev.hw.temp_high.title" => "{label} temperature {celsius} °C";
    "nev.hw.temp_high.body" => "{label} is at {celsius} °C (warning {warn_c} °C, critical {crit_c} °C).";
    "nev.hw.temp_ok.title" => "{label} temperature back to normal ({celsius} °C)";
    "nev.hw.thermal_shutdown.title" => "Thermal shutdown imminent: {label} {celsius} °C";
    "nev.hw.thermal_shutdown.body" => "{label} is at {celsius} °C; the hardware controller powers the NAS off at {shutdown_c} °C (when thermal shutdown is on). Check the fans and the airflow.";
    "nev.hw.thermal_ok.title" => "{label} cooled down ({celsius} °C)";
    // storage
    "nev.storage.disk_warning.title" => "Disk {disk_label} needs attention";
    "nev.storage.disk_failing.title" => "Disk {disk_label} failing";
    "nev.storage.disk_ok.title" => "Disk {disk_label} healthy again";
    "nev.storage.raid_degraded.title" => "RAID {array} degraded";
    "nev.storage.raid_degraded.body" => "{array} ({level}) is running without redundancy: {degraded} of {raid_disks} members missing, state “{state}”. Members: {members}.";
    "nev.storage.raid_degraded.body_2" => "{array} ({level}) is running without redundancy: {degraded} of {raid_disks} members missing, state “{state}”.";
    "nev.storage.raid_failed.title" => "RAID {array} failed";
    "nev.storage.raid_failed.body" => "{array} ({level}) has failed: {degraded} of {raid_disks} members missing, state “{state}”. Members: {members}.";
    "nev.storage.raid_failed.body_2" => "{array} ({level}) has failed: {degraded} of {raid_disks} members missing, state “{state}”.";
    "nev.storage.raid_inactive.title" => "RAID {array} inactive";
    "nev.storage.raid_inactive.body" => "{array} ({level}) is not running, state “{state}”.";
    "nev.storage.raid_ok.title" => "RAID {array} healthy again";
    "nev.storage.btrfs_errors.title" => "Volume {volume}: device errors on {device}";
    "nev.storage.btrfs_errors.body" => "Device {device} of volume {volume}: write {write_io} read {read_io} flush {flush_io} corruption {corruption} generation {generation} errors.";
    "nev.storage.btrfs_errors_cleared.title" => "Volume {volume}: device error counters cleared";
    "nev.storage.btrfs_missing.title" => "Volume {volume}: device {devid} missing";
    "nev.storage.btrfs_back.title" => "Volume {volume}: device {devid} back";
    "nev.storage.volume_full.title" => "Volume {volume} {used_pct}% full";
    "nev.storage.volume_full.body" => "{volume} uses {used_gib} GiB, about {free_gib} GiB free ({used_pct}%).";
    "nev.storage.volume_ok.title" => "Volume {volume} back under {limit_pct}% ({used_pct}%)";
    // tasks and configuration (`{type}` shows the task name of `task.<type>`)
    "nev.task.failed.title" => "Task failed: {type}";
    "nev.task.succeeded.title" => "Task succeeded: {type}";
    "nev.config.apply_failed.title" => "Applying the configuration failed";
    "nev.config.apply_ok.title" => "Configuration apply succeeded";
    "nev.sched.task_failed.title" => "Task {name} failed";
    "nev.sched.task_failed.body" => "Result: {result}. See the task history for the output.";
    "nev.sched.task_ok.title" => "Task {name} succeeded";
    "nev.sched.task_ok.body" => "Result: {result}. See the task history for the output.";
    "nev.sched.task_finished.title" => "Task {name} finished";
    "nev.sched.task_finished.body" => "Result: {result}. See the task history for the output.";
    "nev.snapshot.job_failed.title" => "Snapshot job {name} failed";
    "nev.snapshot.job_failed.body" => "Result: {result}. See the task history for the output.";
    "nev.snapshot.job_ok.title" => "Snapshot job {name} succeeded";
    "nev.snapshot.job_ok.body" => "Result: {result}. See the task history for the output.";
    "nev.snapshot.job_finished.title" => "Snapshot job {name} finished";
    "nev.snapshot.job_finished.body" => "Result: {result}. See the task history for the output.";
    // security
    "nev.auth.lockout.title" => "Sign-in locked for {user} after failed attempts";
    "nev.auth.lockout.body" => "Too many failed logins for {user} from {ip}: further attempts are refused for {retry_after_s} s.";
    "nev.auth.totp_enabled.title" => "Two-factor authentication enabled for {user}";
    "nev.auth.totp_disabled.title" => "Two-factor authentication disabled for {user}";
    // power
    "nev.power.shutdown_skipped.title" => "Scheduled shutdown {rule} skipped";
    "nev.power.shutdown_skipped.body" => "The NAS stayed on: {reasons}.";
    "nev.power.shutdown.title" => "Scheduled shutdown {rule}";
    "nev.power.shutdown.body" => "The NAS is shutting down as scheduled.";
    // network
    "nev.net.confirmed.title" => "Network change confirmed (revision {revision})";
    "nev.net.confirmed.body" => "Interfaces: {interfaces}";
    "nev.net.rolled_back.title" => "Network change rolled back ({reason}): revision {revision} restored";
    "nev.net.rolled_back.body" => "The previous network settings run again and were verified (revision {revision}).";
    "nev.net.rollback_unverified.title" => "Network rollback ({reason}) could not be verified";
    "nev.net.rollback_unverified.body" => "Still wrong: {problems}. Rescue: the link-local address of the port or the reset button.";
    "nev.net.adopted.title" => "Network migrated to rynas (systemd-networkd)";
    "nev.net.adopted.body" => "The migration done at boot came up with a routable address and working name resolution, and is kept.";
    "nev.net.boot_dns_failed.title" => "Network migration at boot undone: names did not resolve";
    "nev.net.boot_dns_failed.body" => "systemd-networkd brought a routable address up, but no DNS server of /etc/resolv.conf answered; ifupdown (revision {revision}) is restored.";
    "nev.net.dns_repaired.title" => "Name resolution repaired";
    "nev.net.dns_repaired.body" => "No DNS server was configured while systemd-networkd ran the network; {detail}.";
    "nev.net.bond_member_down.title" => "{bond}: {member} lost its link";
    "nev.net.bond_member_down.body" => "The bond counted {failures} link failures on {member}; the other members carry the traffic. Check the cable and the switch port.";
    "nev.net.bond_active_changed.title" => "{bond}: {to} carries the traffic now";
    "nev.net.bond_active_changed.body" => "The bond moved from {from} to {to}.";
    "nev.net.bond_lacp_lost.title" => "{bond}: the LACP group with the switch is broken";
    "nev.net.bond_lacp_lost.body" => "No LACP partner answers, or the members ended up in different aggregators: check the LACP group on the switch.";
    "nev.net.boot_undone.title" => "Unconfirmed network change undone at boot: revision {revision} restored";
    "nev.net.boot_undone.body" => "The system restarted while revision {tested} was being tested.";
    "nev.net.watchdog_restored.title" => "Network restored to the last known good revision {revision}";
    "nev.net.watchdog_restored.body" => "After boot no port had a routable address; the last known good settings fixed it.";
    "nev.net.rescue.title" => "Network rescue configuration applied";
    "nev.net.rescue.body" => "No port had a routable address after boot. Every port is on DHCP with link-local; the previous settings are revision {revision}.";
    "nev.net.reset.title" => "Factory network restored with the reset button";
    "nev.net.reset.body" => "Every port is on DHCP with link-local; bonds, VLANs and bridges are suspended. The previous settings are revision {revision} (Network › Revisions).";
    // theme / density / language
    "theme.auto" => "System";
    "theme.light" => "Light";
    "theme.dark" => "Dark";
    "density" => "Density";
    "density.comfortable" => "Comfortable";
    "density.compact" => "Compact";
    "language" => "Language";
    "language.auto" => "Browser default";
    "skip" => "Skip to content";
    // palette
    "pal.placeholder" => "Search pages and actions…";
    "pal.pages" => "Pages";
    "pal.empty" => "No matches";
    "pal.hint_nav" => "navigate";
    "pal.hint_open" => "open";
    "pal.hint_close" => "close";
    "pal.go" => "Go to";
    "pal.theme_light" => "Switch to light theme";
    "pal.theme_dark" => "Switch to dark theme";
    "pal.theme_auto" => "Use system theme";
    "pal.density_comfortable" => "Comfortable density";
    "pal.density_compact" => "Compact density";
    "pal.lang" => "Language: {name}";
    "pal.toggle_sidebar" => "Toggle sidebar";
    "pal.toggle_tasks" => "Toggle task panel";
    "pal.edit_layout" => "Customize dashboard";
    "pal.notifications" => "Show notifications";
    // tasks / pending
    "tasks.title" => "Tasks";
    "tasks.none" => "No running tasks";
    "tasks.none_text" => "Long-running operations such as scrubs, resyncs and snapshots will appear here with progress and logs.";
    "tasks.running_1" => "1 running";
    "tasks.running_n" => "{n} running";
    "tasks.last" => "Last";
    "tasks.col_task" => "Task";
    "tasks.col_user" => "User";
    "tasks.col_start" => "Started";
    "tasks.col_duration" => "Duration";
    "tasks.log" => "Task log";
    "tasks.cancel" => "Stop";
    "tasks.close_log" => "Close log";
    "tasks.background" => "You can close this window: the task keeps running.";
    "task.failed" => "Failed";
    "tech_details" => "Technical details:";
    "task.st.ok" => "OK";
    "task.st.error" => "Error";
    "task.st.cancelled" => "Canceled";
    "task.st.running" => "Running";
    "task.st.queued" => "Queued";
    "task.config.apply" => "Apply configuration";
    "task.zram.apply" => "Apply memory settings";
    "task.time.apply" => "Apply date and time settings";
    "task.time.set" => "Set the date and time";
    "task.time.sync" => "Synchronize the clock";
    "time.title" => "Date and time";
    "time.subtitle" => "Time zone and clock synchronization";
    "time.open_text" => "Time zone, time servers and the clock's state.";
    "time.settings" => "Settings";
    "time.zone_search" => "Search a city or region";
    "time.zone_help" => "Running apps keep the previous zone until they restart.";
    "time.sync" => "Synchronize automatically";
    "time.sync_help" => "Certificates, two-factor codes and schedules need the right time.";
    "time.servers" => "Time servers";
    "time.servers_help" => "One per line, at most 8. Empty: the system's default servers.";
    "time.defaults" => "Default servers: {list}.";
    "time.dhcp" => "From DHCP, also used: {list}.";
    "time.dhcp_first" => "From DHCP, asked first: {list}.";
    "time.dhcp_replaced" => "Servers from DHCP ({list}) are not used while your own servers are listed.";
    "time.servers_ignored" => "The time service's configuration does not read the folder these servers are written to, so they are not used: add the line “sourcedir /etc/chrony/sources.d” to it.";
    "time.other_daemon" => "Time synchronization is managed by another service, outside rynas: its settings are not changed here.";
    "time.also_own" => "The servers of the time service's own configuration stay in use too.";
    "time.will_install" => "The time service will be installed from the distribution's archive.";
    "time.apply" => "Apply";
    "time.saved" => "Date and time settings applied";
    "time.status" => "Clock";
    "time.state" => "State";
    "time.st_off" => "Manual";
    "time.st_synced" => "Synchronized";
    "time.st_unsynced" => "Not synchronized";
    "time.source" => "Server";
    "time.offset" => "Offset";
    "time.last" => "Last sync";
    "time.poll" => "Checked every";
    "time.sync_now" => "Sync now";
    "time.synced" => "The clock is synchronized";
    "time.manual" => "Set manually";
    "time.manual_label" => "Date and time (system time zone)";
    "time.browser" => "Use this browser's time";
    "time.set" => "Set";
    "time.set_done" => "Date and time set";
    "time.local_rtc" => "The hardware clock keeps local time: daylight saving changes can shift it by an hour.";
    "time.rtc_utc" => "Keep it in UTC";
    "nev.system.clock_unsynced.title" => "The clock is not synchronized";
    "nev.system.clock_unsynced.body" => "No time server has answered for {minutes} minutes. Certificates, two-factor codes, schedules and snapshots need the right time: check the network and the time servers.";
    "nev.system.clock_synced.title" => "The clock is synchronized again";
    "nev.updates.available.title" => "{updates} updates available, {security} for security";
    "nev.updates.available.title_2" => "{updates} updates available";
    "nev.updates.available.body" => "Packages: {packages}.";
    "nev.updates.up_to_date.title" => "The system is up to date";
    "nev.ups.on_battery.title" => "UPS on battery";
    "nev.ups.on_battery.body" => "Mains power is gone. The system shuts down after {minutes} min on battery, or earlier on low battery.";
    "nev.ups.on_battery.body_2" => "Mains power is gone. The system shuts down on low battery.";
    "nev.ups.power_back.title" => "UPS back on mains power";
    "nev.ups.power_back.body" => "Mains power is back.";
    "nev.ups.upsmon_failed.title" => "UPS: upsmon did not take the shutdown request";
    "nev.ups.upsmon_failed.body" => "upsmon -c fsd: {error}. rynas powers the system off itself.";
    "nev.ups.shutdown.title" => "UPS shutdown: {reason}";
    "nev.ups.shutdown.reason.fsd" => "forced by the UPS server";
    "nev.ups.shutdown.reason.lowbatt" => "battery low";
    "nev.ups.shutdown.reason.waittimeup" => "time on battery is over";
    "nev.ups.shutdown.reason.nocomm" => "the UPS does not answer";
    "nev.ups.shutdown.reason.shutdowncmd" => "shutdown command";
    "nev.ups.shutdown.body" => "The system shuts down on UPS power: rynas stops its running and scheduled tasks first.";
    "nev.crypt.auto_unlock_failed.title" => "Encrypted folder {share} was not unlocked automatically";
    "nev.crypt.auto_unlock_failed.body" => "{path} stays locked: {error}. Unlock it with its password in Shared folders, then restart the apps and containers that use it.";
    "nev.system.config_refresh_failed.title" => "Settings not brought up to date after the update";
    "nev.system.config_refresh_failed.body" => "rynas {version} writes the files of {keys} differently, but writing them failed and the previous files stay. Open the setting and save it again to see the error.";
    "nev.system.clock_drift.title" => "The clock is drifting";
    "nev.system.clock_drift.body" => "The clock is {offset} away from the time server and is not catching up. Check the time servers.";
    "nev.system.clock_in_step.title" => "The clock is in step again";
    "nev.system.clock_sync_off.title" => "Automatic time sync was turned off";
    "proc.title" => "Processes";
    "proc.subtitle" => "What runs on the system, grouped by service, app and session";
    "proc.open_text" => "Every process with its CPU, memory and disk use; restart a service, change a priority or stop a process.";
    "proc.search" => "Filter by name, command, user or app";
    "proc.view" => "View";
    "proc.view_groups" => "Groups";
    "proc.view_list" => "List";
    "proc.view_tree" => "Tree";
    "proc.sort" => "Sort by";
    "proc.sort_cpu" => "CPU";
    "proc.sort_mem" => "Memory used";
    "proc.sort_read" => "Disk reads";
    "proc.sort_write" => "Disk writes";
    "proc.sort_age" => "Oldest first";
    "proc.sort_name" => "Name";
    "proc.summary" => "{n} processes · CPU {cpu} of {cores} cores · {mem} of memory in processes";
    "proc.none" => "No process matches the filter.";
    "proc.col.name" => "Name";
    "proc.col.user" => "User";
    "proc.col.cpu" => "CPU";
    "proc.col.memory" => "Memory";
    "proc.col.disk" => "Disk I/O";
    "proc.read" => "Read";
    "proc.write" => "Write";
    "proc.g.session" => "Session of {user}";
    "proc.g.user" => "Services of {user}";
    "proc.g.kernel" => "Kernel";
    "proc.k.service" => "System service";
    "proc.k.container" => "Container";
    "proc.k.app" => "App";
    "proc.k.session" => "Login session";
    "proc.k.user" => "User services";
    "proc.k.kernel" => "Kernel threads";
    "proc.k.system" => "System";
    "proc.st.running" => "Running";
    "proc.st.sleeping" => "Waiting";
    "proc.st.disk_wait" => "Waiting for the disk";
    "proc.st.stopped" => "Stopped";
    "proc.st.zombie" => "Ended, not yet collected";
    "proc.st.idle" => "Idle";
    "proc.st.other" => "Other";
    "proc.cpu_graph" => "CPU of this process";
    "proc.mem_graph" => "Memory of this process";
    "proc.cpu_aria" => "CPU of the process: {v}";
    "proc.mem_aria" => "Memory of the process: {v}";
    "proc.f.group" => "Belongs to";
    "proc.f.rss" => "Memory (resident)";
    "proc.f.pss" => "Memory (own share)";
    "proc.f.swap" => "In swap";
    "proc.f.io_total" => "Read / written since start";
    "proc.f.threads" => "Threads";
    "proc.f.nice" => "Nice";
    "proc.f.parent" => "Parent process";
    "proc.f.started" => "Started";
    "proc.f.ports" => "Listening on";
    "proc.f.exe" => "Program";
    "proc.f.cgroup" => "Control group";
    "proc.f.cmdline" => "Command line";
    "proc.io_normal" => "Normal";
    "proc.io_idle" => "Only when the disk is free";
    "proc.io_be" => "Normal, level {level}";
    "proc.io_rt" => "Real time, level {level}";
    "proc.ended" => "This process has ended.";
    "proc.protected" => "The system or rynas needs this process: it cannot be stopped or changed from here.";
    "proc.in_service" => "This process belongs to the service {name}: if it is stopped, the system starts it again. Restart the service instead.";
    "proc.in_container" => "This process runs in the container {name}. Restarting the container is usually the right fix.";
    "proc.restart_svc" => "Restart the service";
    "proc.restart_ct" => "Restart the container";
    "proc.restart_svc_title" => "Restart {name}?";
    "proc.restart_ct_title" => "Restart the container {name}?";
    "proc.restart_svc_text" => "The service stops and starts again. Whoever is using it is disconnected for a moment.";
    "proc.restart_docker_text" => "Docker stops and starts again: every container and its app stops too and comes back after a minute or two.";
    "proc.restart_ct_text" => "The container stops and starts again; its app is unreachable for a moment.";
    "proc.restarted" => "Restarted";
    "proc.priority" => "Priority";
    "proc.cpu_prio" => "CPU priority";
    "proc.io_prio" => "Disk access";
    "proc.prio_high" => "High";
    "proc.prio_above" => "Above normal";
    "proc.prio_normal" => "Normal";
    "proc.prio_low" => "Low";
    "proc.prio_lowest" => "Lowest";
    "proc.prio_custom" => "Now set by hand to nice {nice}.";
    "proc.prio_apply" => "Change priority";
    "proc.prio_done" => "Priority changed";
    "proc.stop" => "Stop the process";
    "proc.stop_help" => "Terminate asks the process to close; Kill ends it at once, without saving anything.";
    "proc.term" => "Terminate";
    "proc.kill" => "Kill";
    "proc.term_title" => "Terminate {name}?";
    "proc.kill_title" => "Kill {name}?";
    "proc.term_text" => "The process is asked to close. Work it has not saved may be lost.";
    "proc.kill_text" => "The process ends at once, without closing its files. Use it only when Terminate does not work.";
    "proc.signal_sent" => "Sent to the process";
    "task.processes.restart-service" => "Restart a service";
    "err.process_protected" => "The system or rynas needs this process: it cannot be stopped or changed.";
    "mem.title" => "Memory";
    "mem.subtitle" => "Compressed swap in RAM";
    "mem.open_text" => "zram: compressed swap in RAM, with values advised for this system.";
    "mem.status" => "Status";
    "mem.state" => "zram";
    "mem.on" => "On";
    "mem.off" => "Off";
    "mem.size" => "Size";
    "mem.algorithm" => "Compression";
    "mem.data" => "Data held";
    "mem.ram_used" => "RAM used";
    "mem.disk_swaps" => "Swap on disk";
    "mem.no_disk_swap" => "None.";
    "mem.disk_line" => "{size}, {used} used, priority {priority}, {kind}";
    "mem.disk_hdd" => "hard disk";
    "mem.disk_ssd" => "SSD";
    "mem.disk_file" => "swap file";
    "mem.disk_help" => "zram has the highest priority: the disks only take what no longer fits in it.";
    "mem.zram" => "zram";
    "mem.zram_help" => "Keeps memory pages compressed in RAM instead of writing them to disk, so a system with little memory stays responsive. It works together with swap on disk.";
    "mem.enable" => "Use zram";
    "mem.advice" => "Advised here: {size}, {algorithm}, swappiness {swappiness}, page-cluster {cluster}.";
    "mem.use_advice" => "Use these values";
    "mem.size_mib" => "Size (MiB)";
    "mem.size_help" => "Data it can hold before compression; it takes RAM only for what it holds.";
    "mem.algorithm_help" => "zstd saves more RAM, lz4 uses less CPU.";
    "mem.swappiness" => "Swappiness";
    "mem.swappiness_help" => "0-200: how willingly memory goes to swap rather than dropping file cache. Above 100 suits swap in RAM.";
    "mem.cluster" => "Page-cluster";
    "mem.cluster_help" => "0-5: pages read together from swap, as a power of two. 0 suits swap in RAM.";
    "mem.will_install" => "Applying installs the systemd-zram-generator package from the Debian archive.";
    "mem.apply" => "Apply";
    "mem.saved" => "Memory settings saved";
    "mem.pending_restart" => "The saved values are not in use yet. Press Apply to try again, or restart the system.";
    "mem.zswap" => "zswap is on as well: two compression layers on the same memory. Turn zswap off.";
    "mem.unsupported" => "The running kernel has no zram.";
    "mem.external" => "This zram device was set up outside rynas, so it is only shown here.";
    "task.config.overwrite" => "Restore managed files";
    "task.storage.volume-create" => "Create volume";
    "task.storage.volume-grow" => "Expand volume";
    "task.storage.volume-replace" => "Replace disk";
    "task.storage.scrub" => "Scrub data";
    "task.storage.scrub-stop" => "Stop the data scrub";
    "task.storage.unlock" => "Unlock volume";
    "task.storage.lock" => "Lock volume";
    "task.storage.subvolume-create" => "Create subvolume";
    "task.storage.subvolume-delete" => "Delete subvolume";
    "task.storage.subvolume-settings" => "Change subvolume settings";
    "task.snapshot.run" => "Take snapshots";
    "task.snapshot.restore" => "Restore from snapshot";
    "task.snapshot.rollback" => "Roll back to a snapshot";
    "task.snapshot.delete" => "Delete snapshot";
    "task.network.apply" => "Apply network settings";
    "task.network.rollback" => "Roll back network settings";
    "task.network.restore" => "Restore network settings";
    "task.network.rescue" => "Rescue the network";
    "task.network.adopt" => "Adopt network configuration";
    "task.power.shutdown" => "Shut down";
    "task.user.password" => "Set password";
    "task.smb.recycle-cleanup" => "Empty recycle bins";
    "task.homes.move" => "Move home folders";
    "task.homes.move-finish" => "Delete old home folder";
    "task.homes.move-undo" => "Undo home move";
    "task.btrfs.scrub" => "Scrub data";
    "task.smart.test" => "Run a SMART test";
    "back" => "Back";
    "cancel" => "Cancel";
    "pending.applying" => "Applying configuration…";
    "pending.applied" => "Configuration applied";
    "pending.failed" => "Applying failed, nothing was changed";
    "pending.show_log" => "Show log";
    "pending.conflict" => "Conflict";
    "pending.conflict_text" => "Someone applied a newer value after you made a change marked “Conflict”. Discard your changes and make them again.";
    "pending.discard" => "Discard changes";
    "pending.discard_title" => "Discard pending changes?";
    "pending.discard_text" => "Your {n} pending changes will be discarded. The running configuration is not touched.";
    "pending.review_title" => "Pending changes";
    "pending.staged" => "Changed {when}";
    "pending.comment" => "Comment (optional)";
    "pending.comment_hint" => "Why this change? Stored with the revision.";
    "diff.empty" => "empty";
    "diff.none" => "No differences.";
    "diff.added" => "Added";
    "diff.removed" => "Removed";
    "diff.changed" => "Changed";
    "diff.whole" => "(whole setting)";
    "diff.field" => "Setting";
    "diff.kind" => "Change";
    "diff.value" => "Value";
    "diff.to" => "becomes";
    "cfg.title" => "Configuration";
    "cfg.subtitle" => "Applied revisions and changes made outside rynas";
    "cfg.open" => "Revisions and drift";
    "cfg.open_text" => "Every applied change is a revision you can inspect and restore. rynas also checks that the files it manages were not changed by hand.";
    "cfg.refresh" => "Refresh";
    "cfg.drift" => "Changes outside rynas";
    "cfg.drift_text" => "These files or settings no longer match what rynas applied. Import keeps the system's version as a pending change; Restore writes the applied configuration again.";
    "cfg.checked" => "{n} managed files checked";
    "cfg.no_drift" => "Everything matches";
    "cfg.drift_modified" => "Edited";
    "cfg.drift_missing" => "Missing";
    "cfg.state_missing" => "Missing on the system";
    "cfg.state_unmanaged" => "Created outside rynas";
    "cfg.state_mismatch" => "Different";
    "cfg.state_missing_samba" => "No SMB account";
    "cfg.state_missing_passwd" => "SMB account without user";
    "cfg.sd.samba_without_user" => "The SMB account {name} has no system user.";
    "cfg.sd.user_unmanaged" => "{name} (UID {id}) was created outside rynas.";
    "cfg.sd.user_missing" => "{name} is in the configuration but not on the system.";
    "cfg.sd.user_mismatch" => "The account settings of {name} differ from the configuration.";
    "cfg.sd.user_without_samba" => "{name} has SMB access but no SMB account: set the password again.";
    "cfg.sd.group_missing" => "The group {name} is in the configuration but not on the system.";
    "cfg.sd.group_unmanaged" => "The group {name} (GID {id}) was created outside rynas.";
    "cfg.sd.share_path_missing" => "{path} does not exist any more.";
    "cfg.sd.share_acl_differs" => "The permissions of {path} differ (group membership changed or edited by hand). Missing: {missing}. Unexpected: {extra}.";
    "cfg.sd.upsn_missing" => "The network UPS server accounts are missing from {path}.";
    "cfg.sd.upsn_changed" => "The network UPS server accounts in {path} differ from the configuration (edited by hand or by another tool).";
    "cfg.col_what" => "File or object";
    "cfg.col_key" => "Setting";
    "cfg.col_state" => "State";
    "col.actions" => "Actions";
    "cfg.import" => "Import";
    "cfg.import_hint" => "Keep the system's version: it becomes a pending change to review and apply";
    "cfg.overwrite" => "Restore";
    "cfg.overwrite_hint" => "Write the applied configuration again, dropping the changes made outside rynas";
    "cfg.overwrite_title" => "Restore the applied configuration?";
    "cfg.overwrite_text" => "The changes made outside rynas to {key} will be lost:";
    "cfg.imported" => "{key} imported";
    "cfg.overwritten" => "{key} restored to the applied configuration.";
    "cfg.forbidden_import" => "Only administrators can import this setting.";
    "cfg.forbidden_overwrite" => "Only administrators can restore this setting.";
    "cfg.forbidden_restore" => "Only administrators can restore this revision: it changes privileged settings.";
    "cfg.staged_1" => "1 change is waiting in the pending bar.";
    "cfg.staged_n" => "{n} changes are waiting in the pending bar.";
    "cfg.revisions" => "Revisions";
    "cfg.no_revisions" => "Nothing has been applied yet.";
    "cfg.col_rev" => "Revision";
    "cfg.col_date" => "Date";
    "cfg.col_author" => "Author";
    "cfg.col_changes" => "Changed";
    "cfg.col_comment" => "Comment";
    "cfg.current" => "Current";
    "cfg.restored_from" => "restores #{n}";
    "cfg.restore" => "Restore";
    "cfg.restore_title" => "Restore this revision?";
    "cfg.restore_text" => "The settings of revision #{n} become pending changes. Nothing changes until you review and apply them.";
    "cfg.restored" => "Revision #{n} added to the pending changes";
    "cfg.restore_noop" => "Revision #{n} matches the current configuration: nothing to restore.";
    "cfg.files" => "Files written";
    "cfg.key.system.hostname" => "Host name";
    "cfg.key.users" => "Users and groups";
    "cfg.key.shares" => "Shared folders";
    "cfg.key.homes" => "User homes";
    "cfg.key.snapshots" => "Snapshots";
    "cfg.key.scheduler" => "Scheduled tasks";
    "cfg.key.power.schedule" => "Power schedule";
    "cfg.key.network.rescue" => "Network rescue";
    "cfg.key.storage.volumes" => "Volumes";
    "cfg.key.storage.disks" => "Disk settings";
    "pending.changes.other" => "pending changes";
    "pending.changes.one" => "pending change";
    "pending.review" => "Review";
    "pending.stale" => "The pending changes could not be refreshed: the count may be out of date.";
    "pending.apply" => "Apply";
    // dashboard
    "dash.title" => "Dashboard";
    "dash.subtitle" => "Real-time system overview";
    "dash.edit" => "Customize";
    "dash.done" => "Done";
    "dash.reset" => "Reset layout";
    "dash.editing" => "Editing layout";
    "dash.edit_hint" => "Drag widgets to reorder them, or focus a handle and use the arrow keys. Changes are saved in this browser.";
    "dash.hidden" => "Hidden widgets";
    "dash.hide" => "Hide widget";
    "dash.move" => "Move widget";
    "dash.size" => "Widget size";
    "dash.size.S" => "Small";
    "dash.size.M" => "Medium";
    "dash.size.L" => "Large";
    "dash.moved" => "{widget}: position {pos} of {count}";
    "dash.all_hidden" => "All widgets are hidden";
    "dash.all_hidden_text" => "Choose Customize to bring some back.";
    // sidebar layout (per user, on the NAS)
    "nav.customize" => "Customize sidebar";
    "nav.editing" => "Customizing the sidebar";
    "nav.edit_hint" => "Drag entries and groups to reorder them, or focus a handle and use the arrow keys. Saved on the NAS for your account.";
    "nav.hidden" => "Hidden entries";
    "nav.hide" => "Hide";
    "nav.show" => "Show";
    "nav.move" => "Move";
    "nav.unpin" => "Unpin";
    "nav.group" => "Group {n}";
    "nav.move_group" => "Move group {n}";
    "nav.moved" => "{entry}: group {group}, position {pos}";
    "nav.group_moved" => "Group moved to position {pos} of {count}";
    "nav.pin_page" => "Pin this page";
    "nav.pinned_already" => "This page is already pinned";
    "nav.pins_full" => "You can pin up to {n} pages";
    "nav.pin_title" => "Pin page to the sidebar";
    "nav.pin_edit_title" => "Edit pinned page";
    "nav.pin" => "Pin";
    "nav.pin_name" => "Name";
    "nav.pin_name_bad" => "Enter a name of up to {n} characters";
    "nav.pin_path" => "Page";
    "nav.pin_path_help" => "The address of the page in rynas, starting with /";
    "nav.pin_path_bad" => "Enter an address of rynas that starts with /";
    "nav.pin_icon" => "Icon";
    "nav.reset_ask" => "Reset the sidebar?";
    "nav.reset_text" => "The default order comes back, hidden entries are shown again and pinned pages are removed.";
    "nav.save_failed" => "Couldn't save the sidebar: {error}";
    "nav.icon.pin" => "Pin";
    "nav.icon.hard_drive" => "Drive";
    "nav.icon.database" => "Database";
    "nav.icon.layers" => "Layers";
    "nav.icon.folder" => "Folder";
    "nav.icon.folder_open" => "Open folder";
    "nav.icon.share" => "Share";
    "nav.icon.camera" => "Camera";
    "nav.icon.archive" => "Archive";
    "nav.icon.users" => "People";
    "nav.icon.user" => "Person";
    "nav.icon.server" => "Server";
    "nav.icon.container" => "Container";
    "nav.icon.box" => "Box";
    "nav.icon.puzzle" => "Puzzle piece";
    "nav.icon.network" => "Network";
    "nav.icon.link" => "Link";
    "nav.icon.cpu" => "Processor";
    "nav.icon.memory" => "Memory";
    "nav.icon.activity" => "Activity";
    "nav.icon.settings" => "Gear";
    "nav.icon.calendar" => "Calendar";
    "nav.icon.clock" => "Clock";
    "nav.icon.history" => "History";
    "nav.icon.download" => "Download";
    "nav.icon.bell" => "Bell";
    "nav.icon.shield" => "Shield";
    "nav.icon.key" => "Key";
    "nav.icon.lock" => "Lock";
    "nav.icon.usb" => "USB";
    "nav.icon.disc" => "Disc";
    "nav.icon.file" => "Document";
    "nav.icon.life_buoy" => "Life buoy";
    "nav.icon.power" => "Power";
    "pal.pin_page" => "Pin this page to the sidebar";
    "sys.sidebar" => "Sidebar";
    "sys.sidebar_help" => "Order of the entries, hidden entries and pinned pages, saved on the NAS for your account.";
    "w.system" => "System";
    "w.cpu" => "CPU";
    "w.memory" => "Memory";
    "w.network" => "Network";
    "w.disk_io" => "Disk I/O";
    "w.volumes" => "Volumes";
    "w.health" => "Storage health";
    "w.hardware" => "Hardware";
    "f.model" => "Model";
    "f.os" => "Operating system";
    "f.kernel" => "Kernel";
    "f.uptime" => "Uptime";
    "f.cpu" => "Processor";
    "f.threads" => "threads";
    "f.load" => "Load average";
    "f.version" => "rynas version";
    "f.host" => "Hostname";
    "m.used" => "Used";
    "m.cache" => "Cache";
    "m.free" => "Free";
    "m.swap" => "Swap";
    "m.of" => "of";
    "m.zram_detail" => "Compressed swap in RAM: {used} of RAM used, {saved} saved, {size} capacity";
    "n.rx" => "Download";
    "n.tx" => "Upload";
    "n.down" => "Disconnected";
    "n.none" => "No network interfaces";
    "d.read" => "Read";
    "d.write" => "Write";
    "d.busy" => "utilization";
    "v.free" => "free";
    "v.none" => "No volumes";
    "v.none_text" => "No mounted file systems were found.";
    "v.network" => "{n} network mounts";
    "v.more" => "{n} more in Storage";
    "v.removable" => "{n} on USB or removable drives";
    "v.removable_hint" => "Not NAS volumes: they do not count for the storage state.";
    "h.sub_ok" => "No problems detected";
    "h.sub_sync" => "Redundancy is being rebuilt";
    "h.sub_warn" => "Check the details in Storage";
    "v.arrays" => "RAID arrays";
    "h.disks" => "Drives";
    "h.arrays" => "Arrays";
    "h.volumes" => "Volumes";
    "h.asleep" => "in standby";
    "h.ok" => "All storage is healthy";
    "h.syncing" => "A RAID array is resyncing";
    "h.warn" => "Attention needed";
    "h.err" => "Storage failure";
    "h.fullest" => "Fullest volume";
    "h.no_arrays" => "no RAID";
    "per_thread" => "Per thread";
    "last_min" => "Last {n} min";
    // hardware
    "hw.fans" => "Fans";
    "hw.temps" => "Temperatures";
    "hw.led" => "Status LED";
    "hw.events" => "Events";
    "hw.duty" => "Fan duty";
    "hw.fan_fail" => "Fan failure detected";
    "hw.alarm" => "Alarm";
    "hw.provider" => "Hardware source";
    "hw.test_beep" => "Test beep";
    "hw.stale" => "Stale data";
    "hw.updated" => "Last update";
    "hw.none_title" => "No hardware source";
    "hw.none_text" => "This machine does not expose fans, LEDs or drive bays through a hardware source. Sensor temperatures are still shown on the dashboard.";
    "hw.no_events" => "No recent events";
    "hw.subtitle" => "Fans, temperatures, status LED and platform events";
    "led.green" => "Green";
    "led.green-blink" => "Blinking green";
    "led.orange" => "Orange";
    "led.orange-blink" => "Blinking orange";
    "led.off" => "Off";
    "hw.st_normal" => "Normal";
    "hw.st_starting" => "Starting up";
    "hw.st_attention" => "Needs attention";
    "hw.st_fault" => "Fault";
    "hw.fan_n" => "Fan {n}";
    "hw.tab_overview" => "Overview";
    "ps.title" => "Power schedule";
    "last_boot" => "Last boot";
    "hw.power_led" => "Power LED";
    "hw.led_status_short" => "Status";
    "hw.led_power_short" => "Power";
    "hw.bay_no_power" => "Disk present, bay without power";
    "hw.temp_last" => "Last reading before standby (the disk is not woken up)";
    "hw.identify" => "Identify";
    "hw.identifying" => "Blinking…";
    "hw.identify_on" => "The LED of bay {n} blinks for {s} seconds";
    "hw.v_range" => "Between {min} and {max}.";
    "reset_button" => "Reset button";
    "ps.help" => "Weekly rules that start or shut down the NAS. Startup uses the RTC wake alarm, set before every shutdown; shutdowns check the system first.";
    "add_rule" => "Add rule";
    "ps.edit" => "Edit rule";
    "ps.none_title" => "No power schedule";
    "ps.none_text" => "Add a rule to start or shut down the NAS on chosen days and times.";
    // UPS (Hardware › UPS)
    "ups.tab" => "UPS";
    "task.ups.test" => "Test the UPS battery";
    "task.ups.detect" => "Detect the UPS";
    "task.ups.install" => "Install NUT";
    "task.ups.update-nut" => "Update NUT";
    "ups.install" => "Install NUT";
    "ups.installed" => "NUT installed. Set up the UPS with UPS setup.";
    "ups.missing_text" => "NUT, the UPS service, is not installed on this NAS. Install it here (Debian package nut), then set up the UPS with UPS setup below.";
    "ups.status" => "UPS status";
    "ups.st_unreachable" => "UPS service not reachable";
    "ups.st_none" => "No UPS";
    "ups.st_fsd" => "Forced shutdown";
    "ups.st_critical" => "Critical: on battery, battery low";
    "ups.st_low" => "Battery low";
    "ups.st_battery" => "On battery";
    "ups.st_replace" => "Replace the battery";
    "ups.st_online" => "Online";
    "ups.st_unknown" => "State unknown";
    "ups.f_ol" => "OL · mains power";
    "ups.f_ob" => "OB · on battery";
    "ups.f_lb" => "LB · low battery";
    "ups.f_chrg" => "CHRG · charging";
    "ups.f_dischrg" => "DISCHRG · discharging";
    "ups.f_rb" => "RB · replace battery";
    "ups.f_fsd" => "FSD · forced shutdown";
    "ups.f_bypass" => "BYPASS · on bypass";
    "ups.f_cal" => "CAL · calibrating";
    "ups.f_over" => "OVER · overloaded";
    "ups.none_title" => "No UPS connected";
    "ups.none_text" => "The UPS service runs but reports no UPS. Connect the UPS and set it up with UPS setup below.";
    "ups.down_title" => "UPS service not running";
    "ups.down_text" => "Nothing answers on the NUT port of this NAS (127.0.0.1:3493). Set up the UPS with UPS setup below.";
    "ups.charge" => "Battery charge";
    "ups.runtime" => "Runtime left";
    "ups.load" => "Load";
    "ups.input" => "Input voltage";
    "ups.output" => "Output voltage";
    "ups.manufacturer" => "Manufacturer";
    "ups.beeper" => "Beeper";
    "ups.beeper_on" => "On";
    "ups.beeper_off" => "Off";
    "ups.beeper_muted" => "Muted";
    "ups.battery_date" => "Battery date";
    "ups.driver" => "NUT name and driver";
    "ups.safemode" => "Power failure";
    "ups.lb_always_nut" => "NUT's upsmon always shuts the system down when the UPS reports low battery (LB) or is critical, and rynas stops its tasks first. The timer below only adds an earlier shutdown.";
    "ups.need_nut" => "The shutdown policy and the battery tests use NUT: install it with Install NUT in UPS status above, then set up the UPS.";
    "ups.timer_nut" => "Shut down after a time on battery";
    "ups.timer_help_nut" => "rynas stops its tasks, then asks NUT's upsmon to shut the system down (upsmon -c fsd), with the UPS power-off configured in NUT.";
    "ups.minutes" => "Minutes on battery";
    "ups.minutes_help" => "1 to 1440 minutes.";
    "ups.pre_timeout" => "Time to stop apps and tasks (s)";
    "ups.pre_timeout_help_nut" => "Before the shutdown, rynas stops its running tasks and scheduled jobs within this time (10 to 900 s).";
    "ups.pending_note" => "Saved, not applied yet: the UPS uses the applied settings.";
    "ups.v_minutes" => "Enter whole minutes from 1 to 1440.";
    "ups.v_pre" => "Enter seconds from 10 to 900.";
    "ups.test" => "Battery test";
    "ups.last_result" => "Last result";
    "ups.no_result" => "Not reported by the UPS";
    "ups.tr.passed" => "Completed: passed";
    "ups.tr.warning" => "Completed with a warning";
    "ups.tr.error" => "Completed: failed";
    "ups.tr.aborted" => "Aborted";
    "ups.tr.in_progress" => "Test in progress";
    "ups.tr.none" => "No test run yet";
    "ups.tr.scheduled" => "Test scheduled";
    "ups.test_done" => "Battery test finished";
    "ups.test_unsupported" => "This UPS offers no battery test through NUT.";
    "ups.test_online" => "A test can start only while the UPS is on mains power.";
    "ups.test_lb_nut" => "During a test, low battery still shuts the system down.";
    "ups.quick" => "Quick test";
    "ups.deep" => "Deep test";
    "ups.stop" => "Stop test";
    "ups.deep_title" => "Start a deep battery test?";
    "ups.deep_text_nut" => "The UPS runs on battery until it is almost empty, which can take an hour. If the battery reaches low battery (LB), NUT shuts the system down: start it only when someone is present to stop the test.";
    "ups.deep_start" => "Start deep test";
    "ups.schedule_hint" => "To test on a schedule, add a scheduled task that runs as root:";
    "ups.events" => "Recent UPS events";
    "ups.no_events" => "No UPS events yet.";
    "ups.device" => "Driver and connection";
    "upsw.title" => "UPS setup";
    "upsw.intro" => "No UPS reports to this NAS. rynas can look for a UPS on USB, test which NUT driver reads it and set it up.";
    "upsw.need_nut" => "Install NUT with Install NUT in UPS status above to set up a UPS here.";
    "upsw.name" => "NUT name";
    "upsw.name_help" => "The name of the UPS in NUT (lowercase letters, digits, - and _).";
    "upsw.change" => "Change UPS";
    "upsw.start" => "Set up UPS";
    "upsw.s_find" => "Find";
    "upsw.s_test" => "Test";
    "upsw.s_review" => "Review";
    "upsw.s_done" => "Done";
    "upsw.find_text" => "Connect the UPS to this NAS. The detection lists the UPSes on USB and tries the matching NUT drivers one by one until one reads the UPS status. For a serial or network UPS, enter the driver by hand.";
    "upsw.stop_drivers" => "Stop the running UPS driver during the test";
    "upsw.stop_drivers_help" => "A driver already holds the UPS: it is stopped while the drivers are tested and started again afterwards. The UPS is not monitored meanwhile.";
    "upsw.detect" => "Detect USB UPS";
    "upsw.manual" => "Enter by hand";
    "upsw.testing" => "Each driver gets up to 45 seconds to answer.";
    "upsw.no_report" => "No detection result yet.";
    "upsw.busy" => "A UPS driver is running and holds the device: nothing was tested. Allow stopping it and detect again.";
    "upsw.need_newer" => "No driver read this UPS. Its known setting needs NUT {need} or newer and this NAS has NUT {have}: install a newer NUT on this system, then detect again.";
    "upsw.need_newer_update" => "No driver read this UPS. Its known setting needs NUT {need} or newer and this NAS has NUT {have}. rynas can update NUT to {v} ({source}), then you detect again.";
    "upsw.need_newer_files" => "No driver read this UPS. Its known setting needs NUT {need} or newer and this NAS has NUT {have}. rynas has NUT {v} for this system, attached to the rynas release: copy its packages (nut-server, nut-client, libupsclient7, libnutscan4) to {dir}, then detect again to update.";
    "upsw.nut_src_apt" => "from the package sources of this system";
    "upsw.nut_src_backports" => "from the Debian backports configured on this system";
    "upsw.nut_src_rynas" => "rynas's build of Debian's NUT package, downloaded from the rynas release if it is not already on this NAS";
    "upsw.update_nut" => "Update NUT to {v}";
    "upsw.updating" => "apt installs the new NUT packages; your NUT configuration files are kept.";
    "upsw.updated" => "NUT {v} is installed: detect the UPS again";
    "upsw.driver_output" => "Driver output";
    "upsw.no_usb" => "No UPS found on USB. Check the cable, or enter a serial or network UPS by hand.";
    "upsw.none_answered" => "No driver read the UPS status. The last lines of each driver are below: try another driver or other options by hand.";
    "upsw.found" => "Found: {model}, status {status}, charge {charge}.";
    "upsw.detect_again" => "Detect again";
    "upsw.usb" => "On USB";
    "upsw.nut_version" => "NUT version on this NAS: {v}";
    "upsw.candidates" => "Drivers tried";
    "upsw.min_nut" => "Needs NUT {v} or newer";
    "upsw.p_ok" => "Answers";
    "upsw.p_failed" => "No answer";
    "upsw.p_not_run" => "Not tried";
    "upsw.p_busy" => "Device in use";
    "upsw.p_newer" => "Needs a newer NUT";
    "upsw.p_no_driver" => "Driver not installed";
    "upsw.src_known" => "Known setting for this model";
    "upsw.src_scanner" => "Suggested by the USB scan";
    "upsw.src_generic" => "Generic driver";
    "upsw.src_manual" => "Entered by hand";
    "upsw.manual_text" => "Pick the driver that NUT's hardware list names for the UPS. USB: port auto (add vendorid and productid when several UPSes are connected). Serial: the device, such as /dev/ttyS0. Network (SNMP): the address, with options such as community=public.";
    "upsw.driver" => "Driver";
    "upsw.pick_driver" => "Choose a driver";
    "upsw.driver_help" => "The NUT drivers installed on this NAS.";
    "upsw.driver_for" => "Listed by NUT for: {list}";
    "upsw.port" => "Port";
    "upsw.port_help" => "auto for USB, a serial device such as /dev/ttyS0, or a network address.";
    "upsw.options" => "Driver options";
    "upsw.options_help" => "key=value separated by spaces; a word alone is a flag (norating).";
    "upsw.skip_test" => "Continue without a test";
    "upsw.test" => "Test this setting";
    "upsw.untested" => "This setting was not tested: if the driver cannot start, applying fails and the previous setup comes back.";
    "upsw.conflict" => "NUT's ups.conf has a UPS section that rynas does not manage and that would use the same device. rynas changes it only if you take it over: the section is then removed (a backup is kept).";
    "upsw.take_over" => "Take over and remove the section [{name}]";
    "upsw.removed" => "Lines removed:";
    "upsw.commands" => "Then rynas runs:";
    "upsw.apply" => "Apply";
    "upsw.waiting" => "Waiting for the UPS to report…";
    "upsw.applied" => "The UPS is set up: NUT now runs its driver, upsd and upsmon.";
    // NUT network server
    "upsn.title" => "Network UPS server";
    "upsn.intro" => "Other machines on the network read this UPS on port 3493 and shut down when its battery runs out: servers and PCs with NUT's upsmon, home automation, dashboards.";
    "upsn.on" => "On";
    "upsn.off" => "Off";
    "upsn.pending" => "Changes not applied yet";
    "upsn.need_nut" => "The network UPS server needs NUT: install it with Install NUT in UPS status above.";
    "upsn.module" => "The system layer of this hardware runs NUT with its own files: the network UPS server cannot be set up here.";
    "upsn.listen_all" => "All addresses of the NAS";
    "upsn.listen_chosen" => "Chosen addresses";
    "upsn.listen_help" => "The NAS itself (localhost) is always included.";
    "upsn.listen_bad" => "{addr} is not an address of this NAS: untick it or choose one of the addresses above.";
    "upsn.allow" => "Allowed hosts";
    "upsn.allow_any" => "Any host that reaches the NAS";
    "upsn.filter" => "Host filter";
    "upsn.filter_on" => "Working";
    "upsn.filter_off" => "Not working";
    "upsn.pb.server_inactive" => "The server is on but upsd (nut-server) is not running: no client can connect. Set up the UPS first, or look at the service log.";
    "upsn.pb.filter_inactive" => "The allowed-hosts filter does not work on this system (the kernel does not apply systemd's IP filter): every host that reaches port 3493 can connect. Limit port 3493 with a firewall.";
    "upsn.pb.other" => "The server reports a problem this interface does not know: update rynas.";
    "upsn.allow_help" => "One IP address or subnet per line (192.168.1.20, 192.168.1.0/24, fd00::/64). Empty: no limit. Other hosts are dropped by the system before they reach NUT. Containers on this NAS (Home Assistant in a bridge network, for example) connect from their Docker bridge subnet: allow that subnet too.";
    "upsn.allow_ph" => "e.g. 192.168.1.0/24";
    "upsn.allow_lan" => "Allow the local network ({nets})";
    "upsn.address" => "Address";
    "upsn.ups_name" => "UPS name";
    "upsn.open_warn" => "No allowed hosts are set: every device that reaches port 3493 can read the UPS status. Limit it to your network in the settings.";
    "upsn.accounts" => "Client accounts";
    "upsn.no_accounts" => "No accounts yet. Add one for each machine that must shut down with this UPS, and one for home automation.";
    "upsn.settings" => "Settings";
    "upsn.settings_title" => "Network UPS server settings";
    "upsn.add" => "Add account";
    "upsn.add_title" => "New client account";
    "upsn.user_help" => "a-z, 0-9, _ and -, at most 32 characters.";
    "upsn.user_required" => "Enter a user name.";
    "upsn.user_bad" => "Use 1 to 32 characters a-z, 0-9, _ and - (rynas and rynas-upsmon are taken by rynas).";
    "upsn.user_taken" => "An account with this name already exists.";
    "upsn.role" => "Role";
    "upsn.role_secondary" => "Shuts down with the UPS";
    "upsn.role_monitor" => "Reads only";
    "upsn.role_secondary_help" => "For another machine's upsmon (secondary): it shuts that machine down when this UPS reaches low battery, before the NAS.";
    "upsn.role_monitor_help" => "For home automation and dashboards: it logs in and reads the values, it cannot run commands.";
    "upsn.note" => "Note";
    "upsn.own_password" => "Choose the password";
    "upsn.own_password_help" => "Off: rynas generates a strong one and shows it once.";
    "upsn.password" => "Password";
    "upsn.password_help" => "12 to 64 characters: letters, digits and . _ ~ @ % + = : , - (no spaces, quotes or #).";
    "upsn.password_bad" => "The password needs 12 to 64 characters among letters, digits and . _ ~ @ % + = : , -";
    "upsn.create" => "Create";
    "upsn.connect" => "Connect";
    "upsn.connect_title" => "Connect with the account {name}";
    "upsn.remove" => "Remove";
    "upsn.remove_title" => "Remove the account {name}?";
    "upsn.remove_text" => "The machine that uses it can no longer log in and will not shut down with this UPS.";
    "upsn.regen_title" => "New password for {name}?";
    "upsn.regen_text" => "The current password stops working: enter the new one on the machine that uses this account.";
    "upsn.enable" => "Accept connections from other machines";
    "upsn.enable_help" => "upsd accepts connections from other machines on port 3493.";
    "upsn.read_note" => "NUT lets anyone who connects read the UPS values without a password; accounts decide who logs in to follow it for a shutdown. Allowed hosts limit who connects at all.";
    "upsn.show_changes" => "Show the changes";
    "upsn.file_removed" => "The file is removed.";
    "upsn.shown_once" => "This password is shown only now: copy it to the client. Later you can only make a new one.";
    "upsn.for_ha" => "Home automation and dashboards";
    "upsn.for_ha_help" => "Enter these values in the NUT integration of your home automation (Home Assistant: Settings, Devices and services, Network UPS Tools).";
    "upsn.host" => "Host";
    "upsn.port" => "Port";
    "upsn.for_upsmon" => "Another Linux machine (upsmon)";
    "upsn.for_upsmon_help" => "Add this line to /etc/nut/upsmon.conf on that machine, set MODE=netclient in its nut.conf and restart nut-monitor.";
    "upsn.for_upsmon_help_pw" => "Add this line to /etc/nut/upsmon.conf on that machine with the account's password in place of PASSWORD, set MODE=netclient in its nut.conf and restart nut-monitor.";
    "upsn.monitor_note" => "A read-only account cannot shut a machine down: for upsmon add an account that shuts down with the UPS.";
    "upsn.copy" => "Copy";
    "upsn.allow_none" => "None: only this NAS";
    "upsn.allow_required" => "Enter at least one allowed host or subnet.";
    "cfg.key.ups.netserver" => "Network UPS server";
    "ps.col_on" => "Enabled";
    "ps.col_action" => "Action";
    "ps.col_busy" => "If busy";
    "ps.startup" => "Startup";
    "ps.shutdown" => "Shutdown";
    "ps.enabled" => "Rule enabled";
    "ps.time_help" => "Local time of the NAS.";
    "ps.checks" => "Before shutting down, check";
    "ps.checks_help" => "The NAS is busy when one of these checks finds work in progress.";
    "ps.check_storage" => "RAID resync or scrub running";
    "ps.check_tasks" => "Other tasks running (snapshots, backups, apply)";
    "ps.check_smb" => "SMB clients with open files";
    "ps.when_busy" => "When busy";
    "ps.busy_wait" => "Wait";
    "ps.busy_skip" => "Skip this shutdown";
    "ps.busy_force" => "Shut down anyway";
    "ps.busy_wait_n" => "Wait up to {n} min";
    "ps.max_wait" => "Wait up to (minutes)";
    "ps.max_wait_help" => "Checked again every minute; then the shutdown is skipped (1–720).";
    "ps.v_time" => "Choose a time.";
    "ps.v_days" => "Choose at least one day.";
    "ps.v_same" => "A startup and a shutdown at the same time ({when}).";
    "ps.v_close" => "The shutdown at {down} comes less than 10 minutes after the startup at {up}.";
    "ps.delete_title" => "Delete this rule?";
    "ps.delete_text" => "The rule is removed from the power schedule when the pending changes are applied.";
    "ps.next" => "Next power events";
    "ps.next_startup" => "Next startup";
    "ps.next_shutdown" => "Next shutdown";
    "ps.none" => "None scheduled";
    "ps.today" => "today";
    "ps.tomorrow" => "tomorrow";
    "ps.pending_note" => "These times are those of the applied schedule: pending changes count after they are applied.";
    "ps.time_zone" => "Time zone";
    "ps.wake_alarm" => "RTC wake alarm";
    "ps.not_armed" => "Not set";
    "ps.arm_error" => "The wake alarm could not be set.";
    "ps.ww.no_rtc" => "No real-time clock found: the NAS cannot start itself on schedule.";
    "ps.ww.no_wake_alarm" => "The clock {rtc} has no wake alarm: the NAS cannot start itself on schedule.";
    "ps.wake_unsupported" => "Not supported here";
    "ps.wake_unsupported_help" => "This machine has no RTC wake alarm: startup rules cannot start it. Shutdown rules still work.";
    "ps.prev_clean" => "Previous shutdown clean";
    "ps.clean_yes" => "Yes";
    "ps.clean_no" => "No (power loss or crash)";
    "ps.cause_scheduled" => "Scheduled startup";
    "ps.cause_watchdog" => "Watchdog reset";
    "ps.cause_unknown" => "Button, Wake-on-LAN or power return";
    // storage
    "sto.title" => "Storage";
    "sto.subtitle" => "Drives, RAID arrays and volumes";
    "sto.chassis" => "Chassis";
    "sto.disks" => "Drives";
    "sto.filesystems" => "Volumes";
    "sto.bay" => "Bay";
    "sto.empty_bay" => "Empty";
    "sto.bays" => "Drive bays";
    "sto.m2" => "M.2";
    "sto.m2_slots" => "M.2 NVMe slots";
    "sto.asleep" => "Standby";
    "sto.active" => "Active";
    "health.unknown" => "Unknown";
    "sto.not_powered" => "Powered off";
    "sto.name" => "Name";
    "sto.model" => "Model";
    "sto.serial" => "Serial number";
    "sto.size" => "Capacity";
    "sto.type" => "Type";
    "sto.members_n" => "members";
    "sto.level" => "Level";
    "sto.mount" => "Mount point";
    "sto.fs" => "File system";
    "sto.usage" => "Usage";
    "sto.ssd" => "SSD";
    "sto.hdd" => "HDD";
    "sto.no_disks" => "No drives detected";
    "sto.generic" => "Detected drives";
    "sto.sync" => "Resync";
    // storage manager (pages/storage)
    "confirm.type_name" => "Type {name} to confirm";
    "plan.destructive" => "Erases data";
    "plan.erases" => "Erases {dev}";
    "plan.protection" => "Redundancy";
    "plan.raw" => "Raw capacity";
    "plan.steps" => "{n} steps";
    "plan.tol0" => "No drive may fail";
    "plan.tol1" => "1 drive may fail";
    "plan.toln" => "{n} drives may fail";
    "plan.tolerance" => "Fault tolerance";
    "plan.usable" => "Usable";
    "plan.wasted" => "Unused";
    "plan.k.array" => "Array";
    "plan.k.filesystem" => "File system";
    "plan.k.filesystem_device" => "File system device";
    "plan.k.from" => "From";
    "plan.k.to" => "To";
    "plan.k.level" => "Level";
    "plan.k.mapper" => "Encrypted device";
    "plan.k.md" => "RAID device";
    "plan.k.mountpoint" => "Mount point";
    "plan.k.old" => "Replaced member";
    "plan.k.raid_devices" => "RAID members";
    // storage plan texts by code: errors (plan.e.<PlanIssueCode>), warnings
    // (plan.w.<PlanWarningCode>), steps (plan.s.<PlanStepKind>); {subject}, {other}, {items},
    // {size}, {n} and {level} come from the plan
    "plan.e.invalid_label" => "Invalid name: use 1-16 lowercase letters, digits or dashes, starting with a letter.";
    "plan.e.label_taken" => "A volume, array or mount point with this name already exists.";
    "plan.e.disk_selected_twice" => "{subject}: selected twice";
    "plan.e.disk_count" => "Wrong number of drives for this RAID level";
    "plan.e.no_disks" => "Select at least one drive";
    "plan.e.no_such_disk" => "{subject}: drive not found";
    "plan.e.system_disk" => "{subject}: holds the operating system";
    "plan.e.disk_in_use" => "{subject}: in use by an array, a volume or a mounted file system";
    "plan.e.disk_failing" => "{subject}: SMART reports that the drive is failing";
    "plan.e.disk_too_small" => "{subject}: too small";
    "plan.e.no_such_array" => "{subject}: array not found";
    "plan.e.array_not_healthy" => "{subject} is not healthy: repair it first";
    "plan.e.array_busy" => "{subject} is busy (resync, check or reshape): wait for it to finish";
    "plan.e.level_unsupported" => "{subject}: rynas cannot grow this RAID level";
    "plan.e.level_change_unsupported" => "This level change is not supported (supported: add mirrors, expand RAID 5/6, 2-drive RAID 1 to RAID 5, RAID 5 to RAID 6)";
    "plan.e.not_a_member" => "{subject} is not a member of the array";
    "plan.e.no_redundancy" => "{subject} has no redundancy: a member cannot be replaced";
    "plan.e.no_healthy_member" => "{subject}: no healthy member left to rebuild from";
    "plan.e.no_such_filesystem" => "{subject}: btrfs file system not found";
    "plan.e.not_mounted" => "{subject}: not mounted, it cannot be scrubbed";
    "plan.e.scrub_running" => "A scrub is already running on this file system";
    "plan.w.data_erased" => "{subject}: the data on it will be erased ({items})";
    "plan.w.smart_warnings" => "{subject}: SMART warnings ({items})";
    "plan.w.no_smart_data" => "{subject}: no SMART data yet";
    "plan.w.removable_disk" => "{subject}: USB or removable drive: unplugging it breaks the volume";
    "plan.w.mixed_sizes" => "Drives of different sizes: each one is used up to the size of the smallest, {size} stay unused";
    "plan.w.mixed_kinds" => "Mixed drive types (HDD, SSD, NVMe): the array runs at the speed of the slowest";
    "plan.w.smr_raid" => "SMR drives ({items}): a rebuild can take days or fail; prefer CMR drives for RAID";
    "plan.w.smr_slow_writes" => "SMR drives ({items}): long writes are slow";
    "plan.w.single_no_redundancy" => "No redundancy: if the drive fails, the volume is lost (a mirror drive can be added later)";
    "plan.w.raid0_no_redundancy" => "RAID 0 has no redundancy: any failed drive loses the whole volume";
    "plan.w.raid5_rebuild" => "RAID 5 with large or many HDDs: a second failure during the long rebuild loses the volume; consider RAID 6";
    "plan.w.encrypted" => "Encrypted: after every restart the volume stays locked until it is unlocked in rynas; keep the recovery key and the header backup outside the NAS";
    "plan.w.compression_off" => "Compression off: zstd level 1 costs little CPU and usually saves space";
    "plan.w.ssd_trim_off" => "SSD with encryption: TRIM is off because it reveals which blocks are free; enable “discard” in crypttab if that is acceptable";
    "plan.w.disk_larger_than_members" => "{subject} is larger than the members: the extra space stays unused until every member is replaced";
    "plan.w.grow_filesystem_by_hand" => "No mounted btrfs on this array: grow the file system by hand";
    "plan.w.spare_member" => "{subject} is a spare: it can simply be removed";
    "plan.w.smr_slow_rebuild" => "{subject}: SMR drive, the rebuild may be very slow";
    "plan.w.other_partitions" => "{subject} has {n} partitions: only the RAID members are rebuilt, copy the others (EFI, swap) separately";
    "plan.w.array_not_healthy" => "{subject} is not healthy: without redundancy a scrub finds errors but cannot repair them";
    "plan.w.scrub_cost" => "A scrub reads every allocated block: it takes hours on large HDD volumes and wakes sleeping drives; it never runs together with replication, balance or deduplication on the same file system";
    "plan.s.wipe_signatures" => "Erase the signatures on {subject}";
    "plan.s.clear_partition_table" => "Clear the partition table of {subject}";
    "plan.s.create_partition" => "Create one RAID partition on {subject}, leaving 128 MiB free at the end";
    "plan.s.wait_udev" => "Wait for the partitions to appear";
    "plan.s.create_array" => "Create the RAID array ({level}, drives: {n})";
    "plan.s.record_array" => "Record the array so it is assembled at boot";
    "plan.s.luks_format" => "Format the array as LUKS2 (Argon2id, 256 MiB of memory, 4 KiB sectors)";
    "plan.s.add_recovery_key" => "Add a recovery key (shown once: save it before going on)";
    "plan.s.backup_luks_header" => "Back up the LUKS header (offered for download, then removed from the NAS)";
    "plan.s.unlock_volume" => "Unlock the new volume";
    "plan.s.declare_crypttab" => "Declare the encrypted volume, locked at boot";
    "plan.s.create_filesystem" => "Create the btrfs file system (data single, metadata DUP)";
    "plan.s.mount_at_boot" => "Mount the volume at boot";
    "plan.s.create_mount_point" => "Create the mount point {subject}";
    "plan.s.mount_volume" => "Mount the volume at {subject}";
    "plan.s.create_snapshot_subvolume" => "Create the snapshot subvolume (hidden from shares)";
    "plan.s.create_cache_subvolume" => "Create the cache subvolume (excluded from snapshots)";
    "plan.s.disable_cow_cache" => "Turn off copy-on-write on the cache subvolume";
    "plan.s.mirror_to_raid5" => "Convert the 2-drive mirror to RAID 5 (instant, same data layout)";
    "plan.s.add_spares" => "Add the new partitions as spares";
    "plan.s.reshape" => "Reshape the array onto the new drives (hours to days; the volume stays online)";
    "plan.s.spares_to_mirrors" => "Turn the spares into active mirrors (resync)";
    "plan.s.wait_reshape" => "Wait for the reshape to finish";
    "plan.s.wait_resync" => "Wait for the resync to finish";
    "plan.s.grow_luks" => "Grow the encrypted device to the new array size";
    "plan.s.grow_filesystem" => "Grow the file system to the new size";
    "plan.s.copy_partition_table" => "Copy the partition table of {other} to {subject}";
    "plan.s.randomize_guids" => "Give {subject} its own partition IDs";
    "plan.s.add_spare" => "Add the new partition as a spare";
    "plan.s.replace_copy" => "Copy {subject} onto the new drive while it stays in the array";
    "plan.s.wait_copy" => "Wait for the copy to finish";
    "plan.s.remove_replaced" => "Remove {subject} (marked faulty by the replacement)";
    "plan.s.mark_failed" => "Mark {subject} as failed";
    "plan.s.remove_member" => "Remove {subject} from the array";
    "plan.s.add_for_rebuild" => "Add the new partition: the rebuild starts";
    "plan.s.wait_rebuild" => "Wait for the rebuild to finish";
    "plan.s.scrub" => "Verify every checksum and repair from a good copy";
    "plan.s.check_array" => "Check the parity or mirrors of {subject} (read-only)";
    "plan.s.wait_check" => "Wait for the check of {subject}, then read the mismatch count";
    "sto.tab.overview" => "Overview";
    "sto.create" => "Create volume";
    "sto.refresh_failed" => "Couldn't refresh the storage data.";
    "sto.no_volumes" => "No volumes yet";
    "sto.no_volumes_text" => "Create a volume on the free drives to start storing data.";
    "sto.free_disks" => "Drives available: {n}. Use them for a new volume or to grow one.";
    "wiz.erase_chip" => "Will be erased";
    "wiz.will_erase" => "Existing data will be erased: {what}";
    "sto.n_disks" => "Drives: {n}";
    "sto.used_n" => "Used: {n}";
    "sto.encrypted" => "Encrypted";
    "sto.encryption" => "Encryption";
    "sto.locked" => "Locked";
    "sto.unlocked" => "Unlocked";
    "sto.locked_text" => "Encrypted volume: unlock it with the passphrase or the recovery key to use its data.";
    "sto.crypt_text" => "LUKS2 encryption. Keep the recovery key and a header backup outside the NAS.";
    "sto.unlock" => "Unlock";
    "sto.lock" => "Lock";
    "sto.unlock_title" => "Unlock {name}";
    "sto.unlock_text" => "The volume is mounted again and the shares on it come back.";
    "sto.pass_or_key" => "Passphrase or recovery key";
    "sto.lock_title" => "Lock the volume {name}?";
    "sto.lock_text" => "The volume is unmounted and its key removed from memory. Shared folders and apps on it stop until it is unlocked again.";
    "sto.passphrase" => "Passphrase";
    "sto.stale" => "Cached";
    "sto.stale_hint" => "A drive below is in standby: details come from the last reading, so it isn't woken up.";
    "sto.scrub_running" => "Scrubbing";
    "health.healthy" => "Healthy";
    "sto.h.warning" => "Warning";
    "sto.h.failing" => "Failing";
    "sto.h.no_smart" => "Not available";
    "sto.h.no_smart_hint" => "This device has no SMART (or its USB adapter does not pass it on): its health cannot be read.";
    "health.degraded" => "Degraded";
    "sto.a.failed" => "Crashed";
    "sto.a.inactive" => "Inactive";
    "sto.p.idle" => "Idle";
    "sto.p.sleep" => "Sleep";
    "sto.nvme" => "NVMe";
    "sto.lvl.single" => "Single drive";
    "sto.lvl.raid0" => "RAID 0";
    "sto.lvl.raid1" => "RAID 1";
    "sto.lvl.raid5" => "RAID 5";
    "sto.lvl.raid6" => "RAID 6";
    "sto.lvl.raid10" => "RAID 10";
    "sto.sync.resync" => "Resync";
    "sto.sync.recovery" => "Rebuilding";
    "sto.sync.check" => "Parity check";
    "sto.sync.repair" => "Repair";
    "sto.sync.reshape" => "Reshaping";
    "sto.sync.waiting" => "Waiting";
    "sto.drive" => "Drive";
    "sto.temp" => "Temperature";
    "sto.temp_short" => "Temp.";
    "sto.health" => "Health";
    "sto.used_by" => "Used by";
    "sto.unused" => "Not in use";
    "sto.smr_hint" => "Shingled (SMR) drive: slow rewrites, rebuilds of RAID 5/6 can take days.";
    "sto.smart" => "S.M.A.R.T.";
    "sto.settings" => "Settings";
    "sto.standby_note" => "This drive is in standby. Its details are not read automatically, so it isn't woken up.";
    "sto.read_now" => "Read now";
    "sto.wwn" => "WWN";
    "sto.firmware" => "Firmware";
    "sto.rpm" => "Rotation speed";
    "sto.poh" => "Power-on time";
    "sto.smart_age" => "S.M.A.R.T. read";
    "sto.ago" => "{t} ago";
    "sto.partitions" => "Partitions";
    "sto.no_smart" => "No S.M.A.R.T. data for this drive.";
    "sto.smart_passed" => "Passed";
    "sto.smart_failed" => "Failed";
    "sto.smart_verdict" => "Drive self-assessment";
    "sto.power_cycles" => "Power cycles";
    "sto.life" => "Life remaining";
    "sto.written" => "Data written";
    "sto.error_log" => "Error log entries";
    "sto.selftest_running" => "Self-test running";
    "sto.pct_left" => "{p}% left";
    "sto.read_at" => "Read at";
    "sto.nvme_health" => "NVMe health";
    "sto.nv_spare" => "Available spare";
    "sto.nv_used" => "Endurance used";
    "sto.nv_media" => "Media errors";
    "sto.nv_unsafe" => "Unsafe shutdowns";
    "sto.nv_warn" => "Minutes over warning / critical temperature";
    "sto.nv_crit" => "Critical warning flags";
    "sto.attributes" => "Attributes";
    "sto.a_value" => "Value";
    "sto.a_worst" => "Worst";
    "sto.a_thresh" => "Threshold";
    "sto.a_raw" => "Raw";
    "sto.a_failing" => "Failing now";
    "sto.a_failed_past" => "Failed in the past";
    "sto.a_watch" => "Watch";
    "sto.a_ok" => "OK";
    "sto.selftests" => "Self-test log";
    "sto.at_hours" => "At hour";
    "sto.recommended" => "Recommended for this drive type";
    "sto.chk_ok" => "As recommended";
    "sto.chk_differs" => "Differs";
    "sto.chk_na" => "Not applicable";
    "sto.current" => "Current";
    "sto.recommended_v" => "recommended";
    "sto.overrides" => "Settings of this drive";
    "sto.pending_note" => "A change is pending: apply it from the pending changes.";
    "sto.kernel_now" => "In use now: scheduler {s}, read-ahead {r} KiB.";
    "sto.scheduler" => "I/O scheduler";
    "sto.default_v" => "Default ({v})";
    "sto.read_ahead" => "Read-ahead (KiB)";
    "sto.ra_invalid" => "Read-ahead must be a number from 0 to 65536.";
    "sto.standby" => "Spin down after";
    "sto.standby_hint" => "Off unless you choose it: spinning down pays only for disks idle for hours, such as archive or backup disks.";
    "sto.hdd_only" => "Hard disks only.";
    "sto.not_set" => "Not set (leave the drive alone)";
    "sto.never" => "Never";
    "sto.apm" => "Power management (APM)";
    // labels of the recommended-settings checks by `SettingCheck.key` (scheduler, read-ahead, APM and
    // temperature reuse sto.scheduler, sto.read_ahead, sto.apm, sto.temp)
    "sto.chk.write_cache" => "Write cache";
    "sto.chk.trim" => "TRIM";
    "sto.chk.alpm" => "SATA link power (ALPM)";
    "sto.chk.apst" => "NVMe power states (APST)";
    "sto.apm_254" => "254 · Recommended: no head parking, no spin-down by the drive";
    "sto.apm_255" => "255 · APM off (also fine)";
    "sto.apm_128" => "128 · Saves some power, no spin-down by the drive";
    "sto.apm_127" => "127 · The drive may park its heads and spin down by itself";
    "sto.apm_1" => "1 · Most power saving: frequent head parking, wears the drive";
    "sto.reset_defaults" => "Use defaults";
    "sto.done_ok" => "Completed";
    "sto.done_err" => "The operation failed.";
    "sto.vol_missing" => "Volume not found";
    "sto.vol_missing_text" => "It may have been renamed or removed.";
    "sto.back_storage" => "Back to Storage";
    "sto.scrub" => "Data scrub";
    "sto.grow" => "Add drives";
    "sto.replace" => "Replace drive";
    "sto.unmanaged" => "This volume wasn't created by rynas: it is shown, but not changed here.";
    "sto.used" => "Used";
    "sto.free" => "Free (estimated)";
    "sto.of" => "of {n}";
    "sto.profiles" => "Profiles";
    "sto.prof.data" => "Data";
    "sto.prof.metadata" => "Metadata";
    "sto.compression" => "Compression";
    "sto.quota" => "Quota";
    "sto.off" => "Off";
    "sto.raid" => "RAID";
    "sto.md_state" => "md state";
    "sto.array" => "Array";
    "sto.mismatch" => "Mismatched blocks (last check)";
    "sto.r.active" => "Active";
    "sto.r.spare" => "Spare";
    "sto.r.faulty" => "Faulty";
    "sto.r.rebuilding" => "Rebuilding";
    "sto.r.journal" => "Journal";
    "sto.missing_members" => "Missing members: {n}";
    "sto.scrub_errors" => "Errors found";
    "sto.scrub_finished" => "No errors";
    "sto.scrub_aborted" => "Interrupted";
    "never_run" => "Never run";
    "sto.scrub_text" => "Reads every block, checks the checksums and repairs from the redundant copy; the RAID parity check verifies the array too.";
    "sto.started" => "Started";
    "sto.duration" => "Duration";
    "sto.progress" => "Progress";
    "sto.scrub_result" => "Result";
    "sto.scrub_stop" => "Stop the scrub";
    "run_now" => "Run now";
    "sto.scrub_title" => "Data scrub on {name}";
    "sto.scrub_start" => "Start the scrub";
    "sto.md_check" => "Also check the RAID parity";
    "sto.review_plan" => "Review plan";
    "sto.hdr_saved" => "Header backup downloaded. Keep it outside the NAS, then delete the copy left on the NAS.";
    "sto.hdr_none" => "No header backup is waiting on the NAS (already deleted, or made before the last restart).";
    "sto.hdr_deleted" => "Header backup deleted from the NAS";
    "sto.hdr_download" => "Download header backup";
    "sto.hdr_delete" => "Delete from the NAS";
    "sto.rk_reveal" => "Show recovery key";
    "sto.rk_title" => "Recovery key of {name}";
    "sto.rk_text" => "The recovery key opens the volume when the passphrase is lost. It can be shown only once, within an hour of the creation.";
    "sto.rk_gone" => "The recovery key is no longer available: it was already shown or more than an hour has passed.";
    "sto.rk_once" => "This is the only time the key is shown. Store it somewhere safe outside the NAS.";
    "sto.rk_stored" => "I have stored the recovery key";
    "sto.copy" => "Copy";
    "sto.download" => "Download";
    "sto.done" => "Done";
    "sto.no_free" => "No drive available: add a drive that is not in use.";
    "sto.keep_level" => "Keep {l}";
    "sto.grow_title" => "Add drives to {name}";
    "sto.grow_text" => "The new drives join the array (more capacity or more redundancy); a level change reshapes it, which can take many hours.";
    "sto.replace_title" => "Replace a drive of {name}";
    "sto.replace_text" => "The new drive is rebuilt from the others; the old one leaves the array when the rebuild ends.";
    "sto.replace_old" => "Drive to replace";
    "sto.replace_new" => "New drive";
    "sto.replace_go" => "Replace";
    "sto.subvolumes" => "Subvolumes";
    "sto.sub_create" => "Create subvolume";
    "sto.sub_none" => "No subvolumes yet. Shared folders get their own when they are created.";
    "sto.path" => "Path";
    "sto.readonly" => "Read-only";
    "sto.inherit" => "Volume default";
    "sto.system" => "System";
    "sto.yes" => "Yes";
    "sto.no" => "No";
    "sto.sub_invalid" => "Use letters, digits, dot, underscore or dash (up to 64), starting with a letter or digit.";
    "edit_named" => "Edit {name}";
    "sto.sub_name_hint" => "Letters, digits, . _ - (up to 64).";
    "sto.sub_compress" => "Compression (zstd)";
    "sto.sub_compress_hint" => "Turn it off for data that is already compressed: video, photos, backups, VM images.";
    "sto.sub_delete" => "Delete {name}";
    "sto.sub_delete_text" => "The subvolume and every file in it are deleted. This cannot be undone.";
    "sto.tuning" => "RAID tuning";
    "sto.stripe_text" => "A larger stripe cache speeds up RAID 5/6 writes and rebuilds at the cost of memory.";
    "sto.proposal" => "rynas proposal";
    "sto.stripe" => "Stripe cache (entries)";
    "sto.kernel_default" => "Kernel default (256)";
    "sto.use_proposal" => "{n} (proposal)";
    "wiz.subtitle" => "Drives, RAID level and options, with the plan shown before anything changes";
    "wiz.steps" => "Steps";
    "wiz.s_disks" => "Drives";
    "wiz.s_level" => "RAID level";
    "wiz.s_options" => "Options";
    "wiz.s_review" => "Review";
    "wiz.next" => "Next";
    "wiz.pick_disks" => "Choose the drives";
    "wiz.pick_text" => "Only drives without data are listed. Everything on the chosen drives will be erased.";
    "wiz.in_use" => "Drives in use, not listed: {n}.";
    "wiz.selected" => "Selected: {n} · {size} raw";
    "wiz.pick_level" => "Choose the RAID level";
    "wiz.recommended" => "Recommended";
    "wiz.min_single" => "Exactly 1 drive";
    "wiz.min2" => "Needs 2 or more drives";
    "wiz.min3" => "Needs 3 or more drives";
    "wiz.min4" => "Needs 4 or more drives";
    "wiz.min_even4" => "Needs an even number of drives, 4 or more";
    "wiz.d_single" => "One drive, no redundancy; a mirror can be added later.";
    "wiz.d_raid0" => "Striping for speed: one failed drive loses everything.";
    "wiz.d_raid1" => "Mirror: every drive holds a full copy.";
    "wiz.d_raid5" => "Single parity: survives one failed drive.";
    "wiz.d_raid6" => "Double parity: survives two failed drives.";
    "wiz.d_raid10" => "Striped mirrors: fast, survives one failure per mirror.";
    "wiz.label" => "Volume name";
    "wiz.label_hint" => "Mounted at {path}.";
    "wiz.label_invalid" => "Use 1-16 lowercase letters, digits or dashes, starting with a letter.";
    "wiz.label_taken" => "A volume with this name already exists.";
    "wiz.compress" => "Compression (zstd, level 1)";
    "wiz.compress_hint" => "Saves space with almost no CPU cost; already compressed files are skipped. It can be turned off per subvolume.";
    "wiz.encrypt" => "Encrypt the volume (LUKS2)";
    "wiz.encrypt_hint" => "The volume must be unlocked after every restart. A recovery key and a header backup are created: keep both outside the NAS.";
    "wiz.pass_again" => "Repeat the passphrase";
    "wiz.pass_hint" => "8 to 512 characters. Without it (or the recovery key) the data cannot be read.";
    "wiz.pass_len" => "The passphrase needs 8 to 512 characters.";
    "wiz.pass_mismatch" => "The passphrases don't match.";
    "wiz.erase_title" => "All data on these drives will be erased";
    "wiz.create_go" => "Erase drives and create";
    "wiz.creating" => "Creating {name}";
    "wiz.created" => "Volume {name} created";
    "wiz.secure_title" => "Secure the encryption now";
    "wiz.secure_text" => "Save the recovery key (shown once, for one hour) and the LUKS header backup outside the NAS.";
    "wiz.open_volume" => "Open the volume";
    "wiz.back_review" => "Back to the review";
    // status words
    "health.failed" => "Failed";
    "health.syncing" => "Resyncing";
    // generic
    "loading" => "Loading…";
    "error.load" => "Couldn't load data";
    "soon" => "Coming soon";
    "soon.plugins" => "Plugins that add pages, widgets and commands through a declarative manifest.";
    "soon.system" => "System logs and a web terminal are on the way.";
    "sys.subtitle" => "System information and interface preferences";
    "sys.info" => "System information";
    "sys.prefs" => "Interface";
    "notfound" => "Page not found";
    "notfound.text" => "This address doesn't match any section.";
    "back_home" => "Back to Dashboard";
    "close" => "Close";
    "copied" => "Copied";
    "role.admin" => "Administrator";
    "nav.security" => "Security";
    // sign-in
    "login.sign_in" => "Sign in";
    "username" => "User name";
    "login.password" => "Password";
    "login.caps" => "Caps Lock is on";
    "login.signing_in" => "Signing in…";
    "login.verify" => "Verify";
    "login.wait" => "Try again in {t}";
    "login.totp_text" => "Enter the 6-digit code from your authenticator app.";
    "login.recovery_text" => "Enter one of the recovery codes you saved when you turned on two-factor authentication. Each code works once.";
    "login.totp_code" => "Authentication code";
    "login.recovery_code" => "Recovery code";
    "login.use_recovery" => "Use a recovery code";
    "login.use_app" => "Use the authenticator app";
    "login.expired" => "Your session has ended. Sign in again to continue where you left off.";
    "login.signed_out" => "You have signed out.";
    "login.renew" => "Sign in again to start a new session.";
    "login.foot" => "Use your system account (administrators only).";
    "login.empty" => "Enter your user name and password.";
    "err.invalid_credentials" => "Wrong user name or password.";
    "err.invalid_totp" => "That code isn't valid. Check the time on your device and try a new code.";
    "err.rate_limited" => "Too many failed attempts.";
    "err.auth_unavailable" => "The authentication service is unavailable. Check that rynasd is running.";
    "err.network" => "Can't reach the NAS. Check your connection.";
    "err.csrf" => "Your session changed in another tab. Reload the page.";
    "err.backend_unavailable" => "The rynas service is not answering. Check that rynasd is running.";
    "err.timeout" => "The NAS took too long to answer.";
    "err.bad_gateway" => "The rynas service sent an invalid answer.";
    "err.internal" => "Internal error on the NAS. Details are in the system log.";
    "err.forbidden" => "Your account can't do this.";
    "err.not_found" => "Not found.";
    "err.retry" => "Retry";
    "err.offline_title" => "Can't reach the NAS";
    "err.offline_text" => "The web service is not answering. It may be restarting, or the network may be down. rynas keeps retrying.";
    "err.unauthenticated" => "Your session has ended.";
    "err.bad_request" => "The request was not valid.";
    "err.invalid_value" => "A value is not valid.";
    "err.nothing_pending" => "There are no pending changes.";
    "err.apply_group_split" => "This change includes a deletion (such as a folder's), which is applied only together with all the pending changes: review it and apply it from the pending changes bar.";
    "err.conflict" => "Something changed meanwhile. Reload and try again.";
    "err.busy" => "Another operation is running. Try again shortly.";
    "err.task_finished" => "The task has already finished.";
    "err.already_exists" => "It already exists.";
    "err.confirmation_required" => "This needs an explicit confirmation.";
    "err.not_applied" => "The change was not applied.";
    "err.method_not_allowed" => "Operation not supported.";
    "err.payload_too_large" => "The data sent is too large.";
    "err.unsupported_media_type" => "Unsupported data format.";
    "err.permission_denied" => "You don't have permission for this item.";
    "err.insufficient_storage" => "The volume is full or your quota is used up.";
    "err.read_only" => "This item is read-only.";
    "err.share_root" => "A shared folder can't be changed from here.";
    "err.outside_share" => "This link leads out of the shared folder: it isn't followed.";
    "err.password_required" => "The archive needs its password.";
    "err.tool_missing" => "A program this feature needs is not installed.";
    "err.remote_unreachable" => "The remote server can't be reached.";
    "err.remote_auth_failed" => "The remote server refused the login.";
    "err.not_available" => "This is not available on this system.";
    "err.docker_not_installed" => "Docker is not installed.";
    "err.docker_unavailable" => "Docker is installed but its service does not answer.";
    "err.containers_running" => "Stop the running containers first.";
    "err.stack_not_managed" => "This stack is managed outside rynas: its files cannot be changed here.";
    "err.host_key_changed" => "The server's key has changed since the first connection.";
    "err.generic" => "Something went wrong. Try again.";
    "session.expires" => "Your session expires in {n} min";
    "session.expires_1" => "Your session expires in 1 min";
    "session.renew" => "Sign in again";
    // shared words of the configuration pages
    "save" => "Save";
    "create" => "Create";
    "edit" => "Edit";
    "delete" => "Delete";
    "reset" => "Reset";
    "off" => "Off";
    "advanced" => "Advanced";
    "col.status" => "Status";
    "table.no_match" => "Nothing matches the search.";
    "form.check" => "Check the highlighted fields.";
    "form.changed_elsewhere" => "These settings were changed elsewhere while you were editing.";
    "form.reload" => "Discard my edits and reload";
    "toast.saved_pending" => "Saved, pending apply";
    "pending.add" => "Pending: new";
    "pending.change" => "Pending: changed";
    "pending.delete" => "Pending: removal";
    "v.account_name" => "Lowercase letters, digits, - and _; starts with a letter or _; up to 32 characters.";
    "v.full_name" => "The full name cannot contain : or ,";
    "v.email" => "Not a valid email address.";
    "v.date" => "Use the format YYYY-MM-DD.";
    "v.share_name" => "Letters, digits, . - and _; starts with a letter or digit; up to 64 characters.";
    "v.path" => "An absolute path such as /srv/data, without .. or //.";
    "v.client" => "A host name, IP address, subnet (192.0.2.0/24), wildcard (*.lan) or *.";
    "v.workgroup" => "1–15 characters, no spaces or \\ / : * ? \" < > | ,";
    "v.threads" => "Between 1 and 256.";
    "v.port" => "A port between 1 and 65535.";
    // users and groups
    "users.subtitle" => "Local accounts for file sharing and sign-in";
    "users.tab_users" => "Users";
    "users.tab_groups" => "Groups";
    "users.search" => "Search users";
    "users.create" => "Create user";
    "users.edit_title" => "Edit user {name}";
    "users.empty" => "No users yet";
    "users.empty_text" => "Create an account for each person who uses the NAS, then give it access to shared folders.";
    "users.name" => "Name";
    "users.name_help" => "Used to sign in; cannot be changed later.";
    "users.full_name" => "Full name";
    "users.email" => "Email";
    "users.email_help" => "For notifications to this user.";
    "users.description" => "Description";
    "users.groups" => "Groups";
    "users.no_groups" => "No groups yet: create them in the Groups tab.";
    "users.access" => "Access";
    "users.admin_chip" => "Admin";
    "users.sec_info" => "User information";
    "users.sec_access" => "Access";
    "users.sec_password" => "Password";
    "users.admin" => "Administrator";
    "users.admin_help" => "Can sign in to rynas and change every setting.";
    "users.smb" => "SMB access";
    "users.smb_help" => "Can open shared folders from Windows, macOS and Linux.";
    "users.shell" => "SSH access";
    "users.shell_help" => "Off by default. SFTP gives file transfer only.";
    "users.shell_none" => "No SSH";
    "users.shell_sftp" => "SFTP only";
    "users.shell_shell" => "Shell";
    "users.disabled" => "Disable account";
    "users.disabled_help" => "The user can't sign in or open shared folders; nothing is deleted.";
    "users.expires" => "Expires on";
    "users.expires_help" => "The account stops working on this day. Empty: never.";
    "users.set_password" => "Set password";
    "users.st_active" => "Active";
    "users.st_disabled" => "Disabled";
    "users.st_expired" => "Expired";
    "users.st_password" => "Needs a password";
    "users.st_unmanaged" => "Not managed";
    "users.st_missing" => "Missing on the system";
    "users.delete_title" => "Delete user {name}?";
    "users.delete_text" => "The account and its access to shared folders are removed when you apply the changes. Files in shared folders stay.";
    "users.remove_home" => "Also delete the home folder";
    "users.remove_home_help" => "Its files cannot be recovered after applying.";
    "users.tab_homes" => "User homes";
    "users.home" => "Home folder";
    "homes.intro" => "Each user gets a private home folder, on a data volume or on the system disk, chosen per user.";
    "homes.enabled" => "Home folders on a volume";
    "homes.enabled_help" => "New users get their home in the homes folder of the default volume, unless you choose another place for them. Turning it off moves and deletes nothing.";
    "homes.volume" => "Default volume";
    "homes.volume_help" => "New homes go in {path}: one btrfs subvolume per user on btrfs, a plain folder otherwise. Changing it moves no existing home.";
    "homes.volume_opt" => "{path} ({fs}), {free} free";
    "homes.no_volumes" => "No data volume is mounted: create one in Storage first.";
    "homes.path" => "Home path";
    "homes.path_help" => "Accounts use this path, one link per user: it stays the same when a home moves to another volume.";
    "homes.smb" => "Share home folders over SMB";
    "homes.smb_help" => "Each user sees a share named after them with their home; nobody else can open it.";
    "homes.recycle" => "Recycle bin in home folders";
    "homes.recycle_help" => "Files deleted over SMB go to #recycle in the home.";
    "homes.off_note" => "Off: new users get their home in /home on the system disk. Homes already on a volume keep working and can still move.";
    "homes.list" => "Home folders";
    "homes.location" => "Location";
    "homes.loc_volume" => "Volume";
    "homes.loc_system" => "System disk";
    "homes.loc_other" => "Other folder";
    "homes.loc_missing" => "Missing";
    "homes.kept" => "Old folder kept as {path}";
    "homes.move" => "Move home";
    "homes.move_all" => "Move all";
    "homes.move_needs" => "Turn on home folders on a volume and apply the changes to move homes there. Moving to the system disk is always possible.";
    "homes.move_title_one" => "Move the home of {name}";
    "homes.move_title_all" => "Move home folders";
    "homes.move_text" => "Nothing moves by itself: choose where, check what happens, then start the move.";
    "homes.move_steps" => "The files are copied with permissions, ACLs and attributes and checked; then the account points at the new home. The old folder is kept, renamed, until you confirm. The user must stay signed out and disconnected meanwhile.";
    "homes.from" => "Current home";
    "homes.to" => "New home";
    "homes.target" => "Files go to";
    "homes.size" => "Space used";
    "homes.avail" => "Free at the destination";
    "homes.kind" => "Created as";
    "homes.kind_subvolume" => "btrfs subvolume";
    "homes.kind_folder" => "Plain folder";
    "homes.ssh_note" => "SSH keys (.ssh/authorized_keys) move with the home.";
    "homes.blocked" => "Cannot move now:";
    "homes.move_btn" => "Start move";
    "homes.moved" => "Home folders moved: confirm each move once everything works.";
    "homes.none" => "No home to move.";
    "homes.batch_total" => "{n} homes, {size} in all; {free} free at the destination.";
    "homes.batch_no_space" => "{n} homes, {size} in all: they do not fit together in the {free} free at the destination. Move fewer at a time or free some space.";
    "homes.batch_partial" => "Some sizes are only partly measured: before copying, the move measures every home in full and stops if they do not fit.";
    "homes.finish" => "Confirm move";
    "homes.finish_title" => "Confirm the move of {name}?";
    "homes.finish_text" => "The old folder {path} is deleted permanently.";
    "homes.finish_btn" => "Delete old folder";
    "homes.finished" => "Old home folder of {name} deleted";
    "homes.undo" => "Undo move";
    "homes.undo_title" => "Undo the move of {name}?";
    "homes.undo_text" => "The old folder goes back to {path} and is the home again. The copy is kept aside as a hidden folder next to it.";
    "homes.undone" => "Move of {name} undone";
    "homes.b.service_off" => "home folders on a volume are off or not applied yet";
    "homes.b.not_local" => "not a local user (uid {detail})";
    "homes.b.already_moved" => "the home is already there";
    "homes.b.source_missing" => "the home {detail} does not exist";
    "homes.b.source_not_plain" => "the home is a link or contains a mount ({detail})";
    "homes.b.target_exists" => "{detail} exists already";
    "homes.b.processes" => "the user has running processes ({detail})";
    "homes.b.smb_sessions" => "the user is connected over SMB ({detail} open sessions)";
    "homes.b.smb_session_one" => "the user is connected over SMB (1 open session)";
    "homes.b.share_inside" => "shared folder {detail} uses the home";
    "homes.b.no_space" => "not enough free space at the destination (needs {detail})";
    "homes.b.no_rsync" => "rsync is not installed";
    "homes.b.move_waiting" => "a previous move waits for confirmation";
    "homes.dest" => "Move to";
    "homes.dest_help" => "A volume (its homes folder) or the system disk (/home).";
    "homes.dest_system" => "System disk (/home)";
    "homes.dest_here" => "Where it is now ({path})";
    "homes.source" => "Current folder";
    "homes.b.target_invalid" => "{detail} is not a volume that can hold homes";
    "users.home_place" => "Home location";
    "users.home_place_help" => "The homes folder of a volume, or /home on the system disk.";
    "users.home_move_help" => "Choosing another place moves the home after saving: you see the plan first, then the files are copied and checked.";
    // Users adopt / UID change
    "users.adopt" => "Adopt";
    "users.adopt_all" => "Adopt all";
    "users.adopt_banner.other" => "{n} accounts on this system were created outside rynas. Adopt them to manage their password, groups and access: they are not recreated and their files do not change.";
    "users.adopt_banner.one" => "One account on this system was created outside rynas. Adopt it to manage its password, groups and access: it is not recreated and its files do not change.";
    "users.adopted" => "Accounts adopted";
    "users.unmanaged_note" => "Created outside rynas. Saving adopts the account as it is: it is not recreated and its files do not change.";
    "users.uid" => "User ID (UID)";
    "users.uid_help" => "Empty: the next free one. Set it to match the owner of files copied from another system.";
    "users.uid_range" => "Use a number from 1000 to 59999.";
    "ids.change_uid" => "Change UID…";
    "ids.change_gid" => "Change GID…";
    "ids.reown" => "Take over files…";
    "ids.title_uid" => "Change the UID of {name}";
    "ids.title_gid" => "Change the GID of {name}";
    "ids.title_reown" => "Give files to {name}";
    "ids.text_change" => "The account gets the new ID, then the files owned by the old one in the chosen folders follow. Check first: nothing changes until you start.";
    "ids.text_reown" => "Files owned by an ID that no account has, such as data copied from another system with its numeric owners, become this account's. Check first: nothing changes until you start.";
    "ids.new_uid" => "New UID";
    "ids.new_gid" => "New GID";
    "ids.from_id" => "Files now owned by ID";
    "ids.id_help_change" => "{min}–{max}, not used by another account. Now: {id}.";
    "ids.id_help_reown" => "1000 or more, not used by any account.";
    "ids.folders" => "Folders";
    "ids.folders_help" => "The data volumes and the home, unless you choose. Read-only snapshots and other file systems inside are skipped; links are not followed.";
    "ids.add_folder" => "Add folder";
    "ids.other_folder" => "Another folder";
    "ids.acls" => "Also change ACL entries";
    "ids.acls_help" => "Permissions that access lists give to this ID.";
    "ids.check" => "Check";
    "ids.start_change" => "Change ID";
    "ids.start_reown" => "Change owner";
    "ids.scanned" => "Entries checked";
    "ids.at_least" => "at least {n}";
    "ids.owners" => "Owners that change";
    "ids.groups" => "Groups that change";
    "ids.acl_entries" => "ACL entries that change";
    "ids.acl_conflicts" => "ACL entries lost";
    "ids.acl_conflicts_text" => "On these files the new ID already has its own ACL entry: it is kept as it is and the old ID's entry is dropped, so the permissions it gave are lost. Check them after the change.";
    "ids.errors" => "Unreadable entries";
    "ids.readonly" => "Read-only snapshots, skipped (they keep the old ID):";
    "ids.other_fs" => "Other file systems, skipped:";
    "ids.walked" => "Folders checked:";
    "ids.retired" => "IDs retired by earlier changes (never given to new accounts):";
    "ids.retired_item" => "{id}: {name}, now {new}";
    "ids.target_retired" => "ID {id} was retired when {name} changed ID: files still under it (snapshots, folders left out then) may belong to {name}.";
    "ids.resumes" => "This completes the previous ID change of this account: files of ID {from} become {to}'s, with their setuid and setgid bits kept.";
    "ids.complete_text" => "This account had ID {id} before an earlier change: files that still have it can follow.";
    "ids.complete" => "Complete: take the files from ID {id}";
    "ids.partial" => "The check stopped at its time limit: the numbers are lower bounds and the change goes through everything.";
    "ids.after" => "Not changed: the user's own schedules (crontab, user services) and files outside these folders. The change runs as a task you can cancel; after a cancel the rest can be completed from here.";
    "ids.done" => "Ownership changed for {name}";
    "ids.b_not_local" => "Not a local account (1000–59999): root and system accounts never change.";
    "ids.b_unmanaged" => "Created outside rynas: adopt it first.";
    "ids.b_out_of_range" => "ID out of range: {d}.";
    "ids.b_same_id" => "That is the current ID.";
    "ids.b_id_in_use" => "The ID belongs to {d}.";
    "ids.b_processes" => "The user has running processes ({d}): sign them out first.";
    "ids.b_processes_group" => "Processes are running with this group ({d}): stop them first.";
    "ids.b_smb_sessions" => "The user is connected over SMB ({d} sessions): disconnect them first.";
    "ids.b_smb_sessions_group" => "Members of the group are connected over SMB ({d} sessions): disconnect them first.";
    "ids.b_path_invalid" => "Folder missing or outside the data folders: {d}";
    "ids.b_bad_request" => "Enter an ID.";
    "ids.b_other" => "Not available on this system.";
    "homes.b.other" => "another reason";
    "groups.search" => "Search groups";
    "groups.create" => "Create group";
    "groups.edit_title" => "Edit group {name}";
    "groups.empty" => "No groups yet";
    "groups.empty_text" => "Groups give the same permissions to several users at once.";
    "groups.name" => "Group name";
    "groups.gid" => "GID";
    "smart.col_id" => "ID";
    "groups.name_help" => "Cannot be changed later. System groups (sudo, staff, users…) are not managed here.";
    "groups.members" => "Members";
    "groups.no_members" => "No members";
    "groups.no_users" => "No users yet.";
    "groups.delete_title" => "Delete group {name}?";
    "groups.delete_text" => "Members lose the permissions they had through this group when you apply the changes. The users stay.";
    "pw.new" => "New password";
    "pw.confirm" => "Confirm password";
    "pw.rules" => "8 to 256 characters, not the user name. A long phrase is easier to remember and harder to guess.";
    "pw.too_short" => "At least 8 characters.";
    "pw.too_long" => "At most 256 characters.";
    "pw.control" => "Control characters are not allowed.";
    "pw.is_user" => "The password cannot be the user name.";
    "pw.mismatch" => "The passwords don't match.";
    "pw.s0" => "Too short";
    "pw.s1" => "Weak";
    "pw.s2" => "Fair";
    "pw.s3" => "Good";
    "pw.s4" => "Strong";
    "pw.done" => "Password of {name} changed";
    "pw.after_apply" => "Set the password after applying the changes: the account must exist first.";
    "pw.not_applied" => "This user is created when you apply the pending changes; set the password afterwards.";
    // shared folders
    "shares.subtitle" => "Folders shared over SMB and NFS, and who can open them";
    "shares.search" => "Search shared folders";
    "shares.create" => "Create shared folder";
    "shares.edit_title" => "Edit shared folder {name}";
    "shares.editor" => "Shared folder settings";
    "shares.empty" => "No shared folders yet";
    "shares.empty_text" => "Create a shared folder on a volume, then choose who can read or change its files.";
    "shares.name" => "Name";
    "shares.name_help" => "The name clients see; cannot be changed later.";
    "shares.location" => "Location";
    "shares.volume" => "Volume";
    "shares.size_of_volume" => "{pct} of {volume}";
    "shares.size_bytes" => "{n} bytes";
    "shares.size_capacity" => "{volume}: {capacity}";
    "shares.size_exact" => "Exact (btrfs quota)";
    "shares.size_as_of" => "As of {when}";
    "shares.protocols" => "Protocols";
    "shares.no_protocol" => "Not shared";
    "shares.recycle" => "Recycle bin";
    "shares.recycle_days" => "On · {n} d";
    "shares.recycle_keep" => "On · kept";
    "shares.subvolume" => "Subvolume";
    "shares.permissions" => "Permissions";
    "shares.st_ready" => "Ready";
    "shares.st_missing" => "Folder missing";
    "shares.st_on_apply" => "Created on apply";
    "shares.st_warnings" => "Warnings";
    "shares.st_root_only" => "Root only";
    "shares.delete_title" => "Delete shared folder {name}?";
    "shares.delete_text" => "The shared folder and its permissions are removed when you apply the changes. The folder and its files stay on the volume: you will find it under Unshared folders.";
    "shares.tab_general" => "General";
    "shares.tab_smb" => "SMB";
    "shares.tab_nfs" => "NFS";
    "shares.loc_subvolume" => "New subvolume";
    "shares.loc_folder" => "Existing folder";
    "shares.no_volumes" => "No volume found. Create one in Storage, or share an existing folder.";
    "shares.subvolume_help" => "Created as {path} when you apply the changes (own snapshots and statistics).";
    "shares.compression" => "Compression";
    "shares.compression_help" => "Turn off for video, photos and backups that are already compressed.";
    "shares.path" => "Path";
    "shares.path_help" => "An existing folder on a local filesystem, ideally a btrfs subvolume.";
    "shares.path_edit_help" => "Changing the path gives NFS clients a new export id: they must mount again.";
    "shares.smb_on" => "Share over SMB";
    "shares.smb_on_help" => "Windows, macOS and Linux file sharing.";
    "shares.preset" => "Purpose";
    "shares.preset_help" => "Sets the SMB options that suit the use.";
    "shares.preset_standard" => "Standard";
    "shares.preset_multi" => "Also shared over NFS";
    "shares.preset_tm" => "Time Machine backups";
    "shares.preset_huge" => "Folders with 100,000+ files";
    "shares.tm_size" => "Time Machine size limit";
    "shares.tm_size_help" => "For example 500G or 1T. Empty: no limit.";
    "shares.browseable" => "Visible in the network";
    "shares.browseable_help" => "Listed when browsing the NAS; hidden shares open by name only.";
    "shares.recycle_on_help" => "Files deleted over SMB go to #recycle instead of disappearing.";
    "shares.retention" => "Empty after (days)";
    "shares.retention_help" => "The daily cleanup removes older items; 0 keeps them.";
    "shares.hide_dot" => "Hide dot files";
    "shares.encrypt" => "Require encryption";
    "shares.shadow" => "Previous versions";
    "shares.shadow_help" => "Show snapshots in the Windows “Previous versions” tab.";
    "shares.snap_prefix" => "Snapshot name prefix";
    "shares.snap_prefix_help" => "Empty: the last part of the shared folder's path.";
    "shares.veto" => "Hidden and blocked names";
    "shares.veto_help" => "One pattern per line, for example @eaDir.";
    "shares.extra" => "Extra SMB options";
    "shares.nfs_on" => "Export over NFS";
    "shares.nfs_on_help" => "For Linux and other Unix clients. Clients need the same user ids.";
    "nfs.rules" => "Client rules";
    "nfs.no_rules" => "No client can mount this folder yet: add a rule.";
    "nfs.no_rules_short" => "No clients";
    "nfs.remove_rule" => "Remove rule";
    "nfs.client" => "Client";
    "nfs.client_help" => "Host, IP, subnet, *.lan or *";
    "nfs.access" => "Access";
    "nfs.squash" => "Squash";
    "nfs.squash_help" => "Which client users act as the anonymous user.";
    "nfs.squash_root" => "Map root to anonymous";
    "nfs.squash_all" => "Map all users to anonymous";
    "nfs.squash_none" => "No mapping (root stays root)";
    "nfs.anonuid" => "Anonymous uid";
    "nfs.anongid" => "Anonymous gid";
    "nfs.sync" => "Synchronous writes";
    "nfs.sync_help" => "Safe default; asynchronous is faster but can lose data on a power cut.";
    "nfs.crossmnt" => "Include nested mounts";
    "nfs.crossmnt_help" => "Also export subvolumes mounted below this folder.";
    "perm.show" => "Show";
    "perm.search" => "Search";
    "perm.user" => "User";
    "perm.group" => "Group";
    "not_set" => "Not set";
    "perm.ro" => "Read only";
    "perm.rw" => "Read & write";
    "perm.deny" => "No access";
    "perm.nobody" => "Nobody to list.";
    "perm.empty_help" => "No permissions set: rynas leaves the folder's current permissions alone.";
    "perm.admins" => "rynas administrators";
    "perm.root_only_help" => "No permissions: the new folder belongs to root and nobody else will be able to use it. Give at least one user or group access.";
    "perm.keep_own" => "Keep the folder's own permissions";
    "perm.set_now" => "Set permissions";
    "perm.empty_confirm_title" => "Save without permissions?";
    "perm.empty_confirm_text" => "{path} belongs to root and gives nobody else access: without permissions nobody but root will be able to use it, not even over SMB, NFS or in Files.";
    "perm.empty_confirm_ok" => "Save for root only";
    "perm.rule_help" => "Only listed users and groups get in. No access wins over Read & write, which wins over Read only.";
    "eff.title" => "Effective permissions";
    "eff.help" => "What a user or group really gets, counting the groups the user belongs to.";
    "eff.principal" => "User or group";
    "eff.choose" => "Choose…";
    "eff.unsaved" => "This shows the saved permissions; save to check your changes.";
    "eff.save_first" => "Save the shared folder to check effective permissions.";
    "eff.none" => "No access (not set)";
    "eff.no_source" => "No entry of the matrix applies.";
    "eff.decisive" => "Sets the result";
    "eff.rule" => "No access wins over Read & write, which wins over Read only.";
    // shared folders: deleting with the data, unshared folders, snapshot folder, remarks
    // (shares.w.<ShareWarningCode>, shares.use.<FolderUsageKind>)
    "shares.delete_all" => "Also delete the folder, its files and snapshots";
    "shares.delete_all_text" => "The shared folder, with every file, its recycle bin and its snapshots, is deleted when you apply the changes.";
    "shares.delete_all_btn" => "Delete everything";
    "shares.del_folder" => "Folder";
    "shares.del_space" => "Space freed";
    "shares.del_snapshots" => "Snapshots";
    "shares.del_recycle_yes" => "Deleted too";
    "shares.del_recycle_no" => "None";
    "shares.del_used" => "Also in use by:";
    "shares.del_blocked" => "It cannot be deleted now:";
    "shares.del_irreversible" => "This cannot be undone: the files and the snapshots cannot be recovered.";
    "shares.del_no_rollback" => "The folder is deleted at the end, when you apply the pending changes from the bar. If a later step fails and the previous settings are put back, the shared folder comes back but its files and snapshots do not.";
    "shares.del_staged" => "Saved: the folder and its snapshots are deleted when you apply the changes";
    "shares.size_min" => "at least {size}";
    "shares.use.nfs_export" => "the NFS export of {name} (removed with the share)";
    "shares.use.share" => "the shared folder {name}";
    "shares.use.snapshot_job" => "the snapshot job {name}";
    "shares.use.task" => "the scheduled task {name}";
    "shares.use.container" => "the container {name}";
    "shares.use.not_on_volume" => "{name} is not directly on a volume: only folders at the top of a volume can be deleted here";
    "shares.use.other" => "{name}";
    "shares.fdel_title" => "Delete folder {name} permanently?";
    "shares.fdel_text" => "The folder with every file, its recycle bin and its snapshots are deleted now.";
    "shares.fdel_done" => "Folder {name} deleted";
    "shares.unshared" => "Unshared folders";
    "shares.unshared_help" => "Folders on the volumes that no shared folder uses, for example after a share was deleted.";
    "shares.unshared_none" => "Every folder on the volumes is shared.";
    "shares.uf_folder" => "Folder";
    "shares.size" => "Size";
    "shares.size_measuring" => "Measuring…";
    "shares.size_asleep" => "Not measured: the disks are asleep";
    "shares.size_asleep_as_of" => "As of {when} (the disks are asleep)";
    "shares.size_failed" => "Size not available";
    "shares.removed_at" => "Shared folder removed";
    "shares.was_shared_as" => "was shared as {name}";
    "shares.share_again" => "Share again";
    "shares.delete_forever" => "Delete permanently";
    "shares.show_snapshots" => "Show snapshots in the folder";
    "shares.show_snapshots_help" => "A read-only #snapshot folder at the top of the share, with one folder per snapshot named after its date and time. It needs a snapshot job for this shared folder.";
    "shares.w.subvolume_on_apply" => "{path} will be created as a subvolume of volume {volume} when you apply the changes.";
    "shares.w.subvolume_on_apply_nc" => "{path} will be created as a subvolume of volume {volume}, without compression, when you apply the changes.";
    "shares.w.not_subvolume" => "{path} is not a btrfs subvolume: snapshots, previous versions and quotas need one.";
    "shares.w.extra_overrides" => "The extra SMB option {key} replaces a setting managed here.";
    "shares.w.nfs_uids" => "NFS trusts the user IDs of the clients: they must use the same IDs as this NAS.";
    "shares.w.nfs_off" => "NFS is off in the file services settings.";
    "shares.w.smb_off" => "SMB is off in the file services settings.";
    "shares.w.tm_no_limit" => "Time Machine without a size limit can fill the volume.";
    "shares.w.multi_no_nfs" => "The multi-protocol preset turns off client caching; it is meant for folders also exported over NFS.";
    "shares.w.no_permissions" => "No permissions set: the folder keeps its own permissions.";
    "shares.w.root_only" => "No permissions, and {path} belongs to root with no access for others: nobody but root can use this shared folder. Give users or groups access in its permissions.";
    "shares.w.all_denied" => "Every entry denies access: nobody can open this folder.";
    "shares.w.snapfolder_no_job" => "The snapshot folder appears once this shared folder has a snapshot job.";
    "shares.w.snapfolder_nfs" => "NFS clients see the snapshot folder only with Include nested mounts on (NFS tab).";
    // NOCOW folders
    "shares.nocow" => "No copy-on-write";
    "shares.nocow_help" => "For video recordings, databases and virtual machine images: files are rewritten in place instead of copied, so they do not fragment.";
    "shares.nocow_help_existing" => "Files are copied on every change and protected by checksums. No copy-on-write can be turned on while the folder is empty.";
    "shares.nocow_is_on" => "New files in this folder are rewritten in place, without checksums or compression.";
    "shares.nocow_cost" => "What the files of this folder give up:";
    "shares.nocow_t_sum" => "checksums: damaged data is not detected by a scrub, and a mirror cannot repair it from the other copy";
    "shares.nocow_t_comp" => "compression";
    "shares.nocow_t_snap" => "space with snapshots: they still work, but every block rewritten after a snapshot takes new space as long as the snapshot is kept";
    "shares.nocow_snap_hint" => "Snapshot jobs on this folder work, but rewritten data costs space: keep few snapshots with a short retention, or none.";
    "shares.nocow_turn_on" => "Write without copy-on-write";
    "shares.nocow_turn_off" => "Back to copy-on-write";
    "shares.nocow_on_title" => "No copy-on-write for {name}?";
    "shares.nocow_off_title" => "Copy-on-write again for {name}?";
    "shares.nocow_on_text" => "New files in this folder will be rewritten in place. Possible only while the folder is empty.";
    "shares.nocow_off_text" => "Files created from now on get copy-on-write, checksums and compression again. Files already in the folder stay without copy-on-write until they are copied.";
    "shares.nocow_done_on" => "{name}: new files are written without copy-on-write";
    "shares.nocow_done_off" => "{name}: new files get copy-on-write again";
    "shares.nocow_move" => "Folder not empty? Move the files into a new folder";
    "shares.nocow_move_1" => "Create a new shared folder with No copy-on-write on.";
    "shares.nocow_move_2" => "Stop the apps and clients that write to the old folder.";
    "shares.nocow_move_3" => "Copy the files with a full copy, not a clone, from a shell: `rsync -aHAX /old/ /new/` (or `cp -a --reflink=never`).";
    "shares.nocow_move_4" => "Check the copy, point the apps at the new folder, then delete the old one.";
    "shares.nocow_move_space" => "Until the old folder is deleted the files take twice the space.";
    "shares.w.subvolume_on_apply_nocow" => "{path} will be created as a subvolume of volume {volume}, without copy-on-write, when you apply the changes.";
    "shares.w.nocow_snapshots" => "{path} has no copy-on-write and a snapshot job: snapshots work, but data rewritten after a snapshot takes new space while the snapshot is kept.";
    "task.shares.nocow" => "Change copy-on-write";
    "err.folder_not_empty" => "The folder is not empty.";
    "apps.w.nocow_hint" => "On btrfs this folder is created without copy-on-write: files are rewritten in place, without checksums or compression.";
    "task.shares.folder-delete" => "Delete folder";
    // encrypted shared folders (D-CRYPT E1)
    "crypt.encrypt" => "Encrypt this folder";
    "crypt.encrypt_help" => "Files are stored encrypted and readable only while the folder is unlocked with its password. Chosen now: an existing folder cannot be encrypted later.";
    "crypt.experimental" => "Experimental. An encrypted folder:";
    "crypt.l_speed" => "is slower than a plain folder, most with many small files;";
    "crypt.l_space" => "gains nothing from compression or deduplication;";
    "crypt.l_snap" => "keeps snapshots, replicas and backups encrypted: restoring needs the password or the recovery key;";
    "crypt.l_apps" => "is an empty, read-only folder for SMB, NFS and apps while it is locked (after every restart unless it unlocks automatically); containers started meanwhile must be restarted after the unlock;";
    "crypt.l_convert" => "to encrypt existing files, copy them into it.";
    "crypt.created_help" => "Created as {path} when you press Create, with its encrypted data in a hidden folder next to it.";
    "crypt.not_installed" => "Encrypted folders need gocryptfs, a package of the Debian archive.";
    "crypt.install" => "Install gocryptfs";
    "crypt.password" => "Folder password";
    "crypt.password_help" => "8 to 256 characters. Without it or the recovery key the files cannot be read by anyone, rynas included.";
    "crypt.auto" => "Unlock automatically after a restart";
    "crypt.auto_help" => "The password is sealed with this machine's host key, a file on the system disk (no TPM2 found). It protects data disks that leave the NAS without the system disk; it does not protect against someone who takes the whole NAS or has administrator access.";
    "crypt.auto_help_tpm" => "The password is sealed with this machine's TPM2 and its host key on the system disk: the data disks cannot be read on another machine. It does not protect against someone who takes the whole NAS and starts it, or has administrator access.";
    "crypt.rk_text" => "The recovery key sets a new password when the current one is lost; while you know the password it can be shown again from the folder's settings. Without both, the files are lost permanently.";
    "crypt.unlocked" => "Unlocked";
    "crypt.locked" => "Locked";
    "crypt.missing" => "Encrypted data missing";
    "crypt.encrypted" => "encrypted";
    "crypt.unlock_text" => "While unlocked, the files are readable through SMB, NFS, apps and Files, with the folder's permissions.";
    "crypt.unlock" => "Unlock";
    "crypt.unlocked_toast" => "{name} is unlocked";
    "crypt.lock_title" => "Lock {name}?";
    "crypt.lock_text" => "SMB connections to this folder are closed, then it is unmounted. It is not locked while files are open or running containers use it: stop them first. Apps and containers started while it is locked see an empty folder until they are restarted after the next unlock.";
    "crypt.lock" => "Lock";
    "crypt.locked_toast" => "{name} is locked";
    "crypt.busy" => "The folder is in use: close the open files and stop the apps using it, then try again.";
    "crypt.title" => "Encrypted folder";
    "crypt.existing_help" => "Snapshots, replicas and backups of this folder hold its encrypted data, stored here:";
    "crypt.pw_title" => "Password of {name}";
    "crypt.pw_with" => "Prove it with";
    "crypt.pw_current" => "Current password";
    "crypt.pw_recovery" => "Recovery key";
    "crypt.pw_recovery_help" => "64 hexadecimal digits, as shown at the creation (dashes and spaces are ignored).";
    "crypt.pw_change" => "Change password";
    "crypt.pw_done" => "{name}: new password set";
    "crypt.auto_title" => "Unlock {name} automatically";
    "crypt.in_use" => "Still in use";
    "crypt.too_old" => "gocryptfs {version} is too old: encrypted folders need version 2.5 or newer (Debian 13 and later ship it).";
    "crypt.rk_later" => "The folder was created but its recovery key could not be fetched: show it from the folder's settings with its password";
    "crypt.stale_text" => "Unlocked. These running containers were started while the folder was locked and still see an empty folder: restart them to give them the files.";
    "crypt.later" => "Later";
    "crypt.restart" => "Restart containers";
    "crypt.restarted" => "Containers restarted";
    "crypt.auto_off" => "Automatic unlock turned off: the new password could not be sealed";
    "crypt.reveal_text" => "The recovery key is read from the folder with its password. Keep it outside the NAS.";
    "crypt.rk_keep" => "Store it somewhere safe outside the NAS: with it anyone can set a new password for this folder.";
    "shares.w.pc_backup_folder" => "{path} holds, is or lies inside the backup folder of the PC {key}: it stays read-only in File until the PC's backups move to a folder of their own.";
    "shares.w.crypt_locked" => "{path} is encrypted and locked: SMB and NFS clients cannot connect; apps and containers started while it is locked see an empty folder and must be restarted after it is unlocked.";
    "err.crypt_wrong_password" => "Wrong password or recovery key";
    "err.crypt_busy" => "The encrypted folder is in use.";
    "err.crypt_not_installed" => "gocryptfs is not installed.";
    "err.ssh_lockout" => "With this change no administrator could log in over SSH.";
    "task.crypt.create" => "Create an encrypted folder";
    "task.crypt.install" => "Install gocryptfs";
    // file services
    "svc.title" => "File services";
    "svc.subtitle" => "SMB and NFS settings for every shared folder";
    "svc.smb" => "SMB";
    "svc.nfs" => "NFS";
    "svc.running" => "Running";
    "svc.stopped" => "Stopped";
    "svc.off" => "Off";
    "svc.exports" => "NFS exports";
    "svc.no_exports" => "No shared folder is exported over NFS.";
    "svc.exports_help" => "Exports and their client rules are edited in each shared folder (NFS tab).";
    // SSH service
    "ssh.title" => "SSH";
    "ssh.subtitle" => "Terminal and SFTP access to this NAS";
    "ssh.open_text" => "Terminal and SFTP access: port, password login, who may log in, keys.";
    "ssh.settings" => "Settings";
    "ssh.running_on" => "Running on {ports}";
    "ssh.enabled" => "SSH service";
    "ssh.enabled_help" => "Accounts with SSH access log in with a terminal or SFTP.";
    "ssh.port" => "Port";
    "ssh.port_help" => "22 is the standard; another port only cuts the noise of scanners.";
    "ssh.root" => "Root login";
    "ssh.root_help" => "Better: administrators log in with their own account and use sudo.";
    "ssh.root_no" => "No";
    "ssh.root_keys" => "Keys only";
    "ssh.root_yes" => "Yes, also with password";
    "ssh.password" => "Password login";
    "ssh.password_help" => "Off: keys only, the safest choice. At least one administrator needs a key.";
    "ssh.allow" => "Who may log in";
    "ssh.allow_everyone" => "Every account";
    "ssh.allow_listed" => "Only accounts with SSH access";
    "ssh.allow_everyone_help" => "Every account with a login shell can log in.";
    "ssh.allow_listed_help" => "Accounts with Shell or SFTP in Users, the PC backup and certificate accounts, and the groups below.";
    "ssh.allow_groups" => "More groups";
    "ssh.allow_groups_help" => "Comma separated, e.g. sudo for accounts made outside rynas.";
    "ssh.who" => "Accounts";
    "ssh.admin" => "administrator";
    "ssh.level_shell" => "shell";
    "ssh.level_sftp" => "SFTP only";
    "ssh.level_none" => "no SSH";
    "ssh.n_keys" => "keys: {n}";
    "ssh.keys_unread" => "keys not read (disk asleep)";
    "ssh.tcp" => "Port forwarding";
    "ssh.tcp_help" => "Tunnels through SSH (ssh -L, -R, SOCKS). Leave off unless needed.";
    "ssh.agent" => "Agent forwarding";
    "ssh.agent_help" => "Sessions may use the keys of the computer that connects.";
    "ssh.x11" => "X11 forwarding";
    "ssh.x11_help" => "Graphical programs over SSH; rarely needed on a NAS.";
    "ssh.lockout" => "With these settings no administrator could log in over SSH. Add a key or allow an administrator first.";
    "ssh.trial_note" => "Access changes are tested: try a new SSH connection, then keep them here, or they go back by themselves.";
    "ssh.trial_title" => "Test the new SSH settings";
    "ssh.trial_text" => "Connect from a new terminal with the command below. If it works, keep the settings; if not, they go back by themselves. Open sessions stay connected.";
    "ssh.trial_other" => "{user} is testing new SSH settings: they go back by themselves unless confirmed.";
    "ssh.left_sr" => "{n} seconds left";
    "ssh.copy" => "Copy";
    "ssh.keep" => "Keep";
    "ssh.kept" => "SSH settings kept";
    "ssh.undone" => "The previous SSH settings are back";
    "ssh.saved" => "SSH settings saved";
    "ssh.host_keys" => "Host keys";
    "ssh.host_keys_help" => "On the first connection your computer shows one of these: check that it matches before answering yes.";
    "ssh.no_host_keys" => "No host keys yet: they are made when openssh-server is installed.";
    "ssh.logins" => "Logins, last 24 hours";
    "ssh.logins_ok" => "{n} logins";
    "ssh.logins_failed" => "{n} failed";
    "ssh.logins_once" => "once";
    "ssh.logins_recent" => "From the last {n} entries";
    "ssh.logins_timeline" => "Timeline";
    "ssh.logins_grouped" => "Grouped";
    "ssh.logins_last" => "last {when}";
    "ssh.logins_top" => "Most failures from: {list}";
    "ssh.logins_since" => "Too many entries today: counted since {when}";
    "ssh.login_ok" => "logged in";
    "ssh.login_failed" => "failed";
    "ssh.login_invalid" => "unknown account";
    "ssh.refresh" => "Refresh";
    "ssh.keys" => "Authorized keys";
    "ssh.account" => "Account";
    "ssh.no_accounts" => "No account can use SSH: give one Shell or SFTP access in Users.";
    "ssh.no_keys" => "No keys: this account logs in with its password only.";
    "ssh.key_options" => "with options";
    "ssh.key_weak" => "weak";
    "ssh.key_remove" => "Remove key";
    "ssh.key_new" => "Public key";
    "ssh.key_new_help" => "The line of the .pub file, e.g. made with ssh-keygen -t ed25519.";
    "ssh.key_add" => "Add key";
    "ssh.key_added" => "Key added";
    "ssh.key_removed" => "Key removed";
    "ssh.key_remove_title" => "Remove the key {name}?";
    "ssh.key_remove_text" => "Whoever uses this key can no longer log in as {user}.";
    "ssh.other_lines" => "The file also has {n} lines that are not keys (comments or other entries): they stay as they are and are not managed here.";
    "ssh.p.not_installed" => "The SSH server is not installed: `apt install openssh-server`.";
    "ssh.p.not_included" => "`/etc/ssh/sshd_config` does not include `sshd_config.d/*.conf`: these settings are not read. Put that line back.";
    "ssh.p.overridden" => "`{file}` also sets `{key}`: its value counts instead of this page's.";
    "ssh.p.extra_ports" => "sshd also listens on {ports} (set in another file).";
    "ssh.p.drift" => "The settings file was changed by hand: saving writes it again.";
    "ssh.p.socket" => "SSH starts on demand (`ssh.socket`): saving switches it to the `ssh.service` daemon.";
    "ssh.p.not_running" => "The SSH service is on but not running.";
    "smb.enabled" => "Enable SMB";
    "smb.enabled_help" => "File sharing for Windows, macOS and Linux.";
    "smb.workgroup" => "Workgroup";
    "smb.workgroup_help" => "Usually WORKGROUP.";
    "smb.min_protocol" => "Minimum protocol";
    "smb.min_protocol_help" => "SMB1 is never offered.";
    "smb.proto_smb2" => "SMB 2.1 (Windows 7+)";
    "smb.proto_smb3" => "SMB 3.0 (Windows 8+)";
    "smb.proto_smb311" => "SMB 3.1.1 (Windows 10+)";
    "smb.server_string" => "Description";
    "smb.server_string_help" => "Shown next to the NAS name in some clients.";
    "smb.wsdd" => "Windows network discovery";
    "smb.wsdd_help" => "The NAS appears under Network in Windows Explorer (WS-Discovery).";
    "smb.mdns" => "Bonjour announcements";
    "smb.mdns_help" => "The NAS appears in the macOS Finder and on Linux (mDNS).";
    "smb.encrypt" => "Require encryption on every share";
    "smb.encrypt_help" => "Clients without SMB 3 encryption cannot connect.";
    "smb.netbios" => "NetBIOS name service";
    "smb.netbios_help" => "Only for very old clients; off by default.";
    "smb.interfaces" => "Listen on";
    "smb.interfaces_help" => "Interfaces or addresses, comma separated. Empty: all.";
    "smb.extra" => "Extra global options";
    "smb.extra_help" => "key = value lines, checked with testparm when applying.";
    "recycle.title" => "Empty recycle bins";
    "recycle.help" => "Runs now what the daily cleanup does: removes items deleted longer ago than the retention.";
    "shares.folder" => "Shared folder";
    "recycle.all" => "All with a recycle bin";
    "recycle.days" => "Older than (days)";
    "recycle.days_help" => "Empty: each folder's retention.";
    "recycle.run" => "Empty now";
    "recycle.done" => "Recycle bins emptied";
    "nfs.enabled" => "Enable NFS";
    "nfs.enabled_help" => "NFS 4.1 and 4.2 for Linux and Unix clients.";
    "nfs.v3" => "Also allow NFSv3";
    "nfs.v3_help" => "For older clients; needs rpcbind (port 111).";
    "nfs.threads" => "Server threads";
    "nfs.threads_help" => "8 suits most homes and small offices.";
    "nfs.mountd" => "mountd port";
    "nfs.statd" => "statd port";
    "nfs.lockd" => "lockd port";
    "nfs.ports_help" => "Fixed ports, so a firewall can allow NFSv3.";
    // security page
    "sec.subtitle" => "Your account, session and two-factor authentication";
    "sec.account" => "Account";
    "sec.user" => "User";
    "sec.role" => "Role";
    "sec.realm" => "Account type";
    "sec.realm_pam" => "System user";
    "sec.realm_rynas" => "rynas user";
    "sec.since" => "Signed in";
    "sec.until" => "Session ends";
    "sec.idle" => "Idle timeout";
    "sec.idle_min" => "{n} min";
    "sec.totp" => "Two-factor authentication";
    "sec.2fa_on" => "2FA on";
    "sec.on" => "On";
    "sec.off" => "Off";
    "sec.off_text" => "Protect your account with a code from an authenticator app (Aegis, 2FAS, Google Authenticator, 1Password…) in addition to your password.";
    "sec.on_text" => "Signing in asks for a code from your authenticator app. Keep your recovery codes somewhere safe.";
    "sec.other_sessions" => "Turning it on signs you out everywhere else: other browsers and devices will have to sign in again.";
    "sec.enable" => "Set up";
    "sec.step_scan" => "Scan the QR code";
    "sec.step_scan_text" => "Open your authenticator app and add an account by scanning this code.";
    "sec.qr_label" => "QR code with the two-factor authentication secret";
    "sec.manual" => "Can't scan it? Enter this key:";
    "sec.copy_secret" => "Copy key";
    "sec.step_code" => "Enter the code the app shows";
    "sec.activate" => "Turn on";
    "sec.enabled_now" => "Two-factor authentication is on.";
    "sec.codes_title" => "Recovery codes";
    "sec.codes_text" => "Use one of these codes to sign in if you lose your phone. Each code works once. They are shown only now: save them somewhere safe.";
    "sec.codes_file_head" => "rynas recovery codes for {user} on {host}. Each code works once.";
    "sec.copy_codes" => "Copy";
    "sec.download" => "Download .txt";
    "sec.saved" => "I saved my recovery codes";
    "sec.done" => "Done";
    "sec.disable" => "Turn off";
    "sec.disable_text" => "Enter a code from your authenticator app or a recovery code to turn off two-factor authentication.";
    "sec.code_or_recovery" => "Authentication or recovery code";
    "sec.demo" => "Not available with demo data: there is no account to protect.";
    "err.totp_already_enabled" => "Two-factor authentication is already on.";
    "err.totp_not_enrolled" => "The setup has expired. Start again.";
    // notifications, task scheduler, snapshots (P3)
    "open" => "Open";
    "day.1" => "Mon";
    "day.2" => "Tue";
    "day.3" => "Wed";
    "day.4" => "Thu";
    "day.5" => "Fri";
    "day.6" => "Sat";
    "day.7" => "Sun";
    "day.long.1" => "Monday";
    "day.long.2" => "Tuesday";
    "day.long.3" => "Wednesday";
    "day.long.4" => "Thursday";
    "day.long.5" => "Friday";
    "day.long.6" => "Saturday";
    "day.long.7" => "Sunday";
    // weekly schedule grid (week_grid.rs)
    "wg.level" => "Level to apply";
    "wg.hours" => "Hours";
    "wg.cell" => "{day} {from}–{to}: {level}";
    "wg.fill_day" => "{day}: all hours to {level}";
    "wg.fill_hour" => "{from}–{to} every day to {level}";
    "wg.all_week" => "All week: {level}";
    "wg.invalid" => "The saved schedule could not be read: it shows as {level} until you save it.";
    "wg.help" => "Pick a level, then click or drag over the hours; a day or an hour heading fills its row or column. Touch: tap an hour to apply the level, hold it a moment and then drag to fill a rectangle, swipe to scroll the page. Keyboard: arrows move, Space applies the level, Shift+Space fills the day, Shift+Enter fills the hour on every day, 1–{n} pick the level, Esc cancels a drag.";
    "wgd.title" => "Weekly schedule grid";
    "wgd.subtitle" => "Component preview, demo build only";
    "wgd.led" => "LED brightness by hour";
    "wgd.window" => "Backup window";
    "wgd.bandwidth" => "Bandwidth by hour";
    "wgd.preview" => "Compact previews";
    "wgd.normal" => "Normal";
    "wgd.reduced" => "Reduced";
    "wgd.off" => "Off";
    "wgd.allowed" => "Run";
    "wgd.paused" => "Paused";
    "wgd.unlimited" => "Unlimited";
    "wgd.limited" => "Limited";
    "wgd.slow" => "Slow";
    "nav.snapshots" => "Snapshots";
    "nt.center" => "Notification center";
    "nt.subtitle" => "Events from storage, hardware, tasks and services, with acknowledgement and silences";
    "nt.settings" => "Notification settings";
    "nt.settings_title" => "Notifications";
    "nt.settings_subtitle" => "Where notifications go (email, ntfy, Telegram, Gotify, webhook) and which events reach them";
    "nt.open_text" => "Delivery targets with a test message, rules by severity and category, quiet hours and silences.";
    "nt.view_all" => "All notifications";
    "nt.load_failed" => "Couldn't load the notifications.";
    "nt.ack" => "Acknowledge";
    "nt.ack_all" => "Acknowledge all";
    "nt.acked_n" => "{n} notifications acknowledged";
    "nt.acked" => "Acknowledged";
    "nt.resolved" => "Resolved";
    "nt.resolved_at" => "Resolved {when}";
    "nt.active" => "Active";
    "nt.silenced" => "Silenced";
    "nt.silence" => "Silence";
    "nt.unsilence" => "Remove silence";
    "nt.details" => "Details";
    "nt.times" => "{n} times";
    "nt.load_more" => "Load more";
    "nt.none" => "No notifications";
    "nt.none_text" => "Nothing matches these filters. Events appear here as soon as rynas records them.";
    "nt.col.severity" => "Severity";
    "nt.col.event" => "Event";
    "nt.col.category" => "Category";
    "nt.col.when" => "When";
    "nt.col.name" => "Name";
    "nt.col.kind" => "Type";
    "nt.col.delivery" => "Sends to";
    "nt.col.matches" => "Matches";
    "nt.col.sends_to" => "Targets";
    "nt.col.until" => "Until";
    "nt.col.created" => "Created";
    "nt.f.any_sev" => "Any severity";
    "nt.f.notice" => "Notice and above";
    "nt.f.warning" => "Warning and above";
    "nt.f.error" => "Error and above";
    "nt.f.any_cat" => "Any category";
    "nt.f.unacked" => "Not acknowledged";
    "nt.f.all" => "All";
    "nt.f.acked" => "Acknowledged";
    "nt.sev.info" => "Info";
    "nt.sev.notice" => "Notice";
    "nt.sev.warning" => "Warning";
    "nt.sev.error" => "Error";
    "nt.sev.critical" => "Critical";
    "nt.cat.storage" => "Storage";
    "nt.cat.hardware" => "Hardware";
    "nt.cat.ups" => "UPS";
    "nt.cat.system" => "System";
    "nt.cat.security" => "Security";
    "nt.cat.update" => "Updates";
    "nt.cat.backup" => "Backup";
    "nt.cat.app" => "Apps";
    "nt.cat.task" => "Tasks";
    "nt.source" => "Source";
    "nt.key" => "Key";
    "nt.key_help" => "End it with * to silence every key that starts with it.";
    "nt.key_required" => "Enter a key.";
    "nt.first" => "First seen";
    "nt.last" => "Last seen";
    "nt.fields" => "Fields";
    "nt.silence_title" => "Silence these notifications?";
    "nt.silence_text" => "Events with this key are still recorded but not sent, and reminders stop. Critical events are silenced too.";
    "nt.silence_for" => "For";
    "nt.for_1h" => "1 hour";
    "nt.for_8h" => "8 hours";
    "nt.for_1d" => "1 day";
    "nt.for_7d" => "7 days";
    "nt.for_ever" => "Until removed";
    "nt.silenced_ok" => "Silence added";
    "nt.tab.targets" => "Targets";
    "nt.tab.rules" => "Rules";
    "nt.tab.silences" => "Silences";
    "nt.targets_help" => "A target is where messages go. Secrets (passwords, tokens) are write-only: they are never shown again.";
    "nt.rules_help" => "A rule picks events by severity, category, source or field and sends them to targets. Each target gets an event once, even when several rules match.";
    "nt.silences_help" => "Silenced keys are recorded but not delivered. Use * at the end to silence a whole family of events.";
    "nt.add_target" => "Add target";
    "nt.add_silence" => "Add silence";
    "nt.no_targets" => "No targets";
    "nt.no_targets_text" => "Add an email account, an ntfy topic or a Telegram chat to receive notifications away from the NAS.";
    "nt.no_rules" => "No rules";
    "nt.no_rules_text" => "Without rules, events are only listed in the notification center.";
    "nt.no_silences" => "No silences";
    "nt.no_silences_text" => "Silence a noisy event from the notification center or add a key here.";
    "nt.delete_target" => "Delete target {name}?";
    "nt.delete_target_text" => "Messages waiting for this target are dropped.";
    "nt.target_in_use" => "A rule still sends to this target: remove it from the rule first.";
    "nt.delete_rule" => "Delete rule {name}?";
    "nt.delete_rule_text" => "Events it matched are no longer sent to its targets.";
    "nt.target_saved" => "Target {name} saved";
    "nt.rule_saved" => "Rule {name} saved";
    "nt.test" => "Send test message";
    "nt.test_ok" => "Test message sent to {name} ({ms} ms)";
    "nt.test_failed" => "Test message to {name} failed:";
    "nt.enabled" => "Enabled";
    "nt.disabled" => "Disabled";
    "nt.failing" => "Failing";
    "nt.delivering" => "Working";
    "nt.never_used" => "Not used yet";
    "nt.last_ok" => "Last delivery {when}";
    "nt.queued" => "{n} queued";
    "nt.quiet" => "Quiet hours";
    "nt.inverted" => "Inverted";
    "nt.kind.smtp" => "Email (SMTP)";
    "nt.kind.ntfy" => "ntfy";
    "nt.kind.telegram" => "Telegram";
    "nt.kind.gotify" => "Gotify";
    "nt.kind.webhook" => "Webhook";
    "nt.kind.ui" => "Web interface";
    "nt.kind.ui_help" => "Events are always listed in the notification center: as a rule target this means no external delivery.";
    "nt.sec.starttls" => "STARTTLS (port 587)";
    "nt.sec.tls" => "TLS (port 465)";
    "nt.sec.none" => "None (local relay only)";
    "nt.m.post" => "POST";
    "nt.m.put" => "PUT";
    "nt.m.get" => "GET";
    "nt.mode.all" => "All conditions";
    "nt.mode.any" => "Any condition";
    "nt.secret.password" => "Password";
    "nt.secret.token" => "Access token";
    "nt.secret.bot_token" => "Bot token";
    "nt.secret_name" => "Secret name";
    "nt.secret_set" => "Set — type to replace";
    "nt.secret_remove" => "Remove";
    "nt.f.enabled" => "Enabled";
    "nt.f.language" => "Message language";
    "nt.f.language_help" => "Subjects, severity names and dates of the messages.";
    "nt.f.connection" => "Connection";
    "nt.f.host" => "SMTP server";
    "nt.f.port" => "Port";
    "nt.f.security" => "Encryption";
    "nt.f.from" => "Sender address";
    "nt.f.to" => "Recipients";
    "nt.f.to_help" => "One address per line, or separated by commas.";
    "nt.f.server" => "Server";
    "nt.f.topic" => "Topic";
    "nt.f.topic_help" => "Anyone who knows a topic on a public server can read it: pick a long, random name.";
    "nt.f.ntfy_user_help" => "Only with password authentication; leave empty to use an access token.";
    "nt.f.chat_id" => "Chat ID";
    "nt.f.chat_id_help" => "The numeric id of the user, group or channel the bot writes to.";
    "nt.f.api_url" => "Bot API address";
    "nt.f.url" => "URL";
    "nt.f.url_help" => "A template: {{ secrets.name }} inserts a secret.";
    "nt.f.method" => "Method";
    "nt.f.headers" => "Headers";
    "nt.f.header_name" => "Header";
    "nt.f.header_value" => "Value";
    "nt.f.add_header" => "Add header";
    "nt.f.body" => "Body";
    "nt.f.body_help" => "Minijinja template with subject, message, severity, events, hostname and secrets. Empty: the default JSON document.";
    "nt.f.secrets" => "Secrets";
    "nt.f.secrets_help" => "Values used as {{ secrets.name }} in the URL, headers and body. They are never shown again.";
    "nt.f.add_secret" => "Add secret";
    "nt.f.when" => "Which events";
    "nt.f.min_severity" => "At least";
    "nt.f.mode" => "Combine conditions";
    "nt.f.mode_help" => "Only the conditions that are set count.";
    "nt.f.categories_help" => "None selected: any category.";
    "nt.f.source_help" => "Regular expression on the event source (storage, scheduler, plugin:…). Empty: any.";
    "nt.f.field" => "Field";
    "nt.f.value" => "Value";
    "nt.f.regex" => "Regex";
    "nt.f.add_field" => "Add field condition";
    "nt.f.invert" => "Invert";
    "nt.f.invert_help" => "Send the events that do NOT match the conditions.";
    "nt.f.deliver" => "Delivery";
    "nt.f.group" => "Group events for";
    "nt.f.group_help" => "Events within this window go out as one message.";
    "nt.f.repeat" => "Remind every";
    "nt.f.repeat_help" => "While a problem stays active and unacknowledged.";
    "nt.f.quiet_help" => "Non-critical events are held during these windows and sent when they end. Critical events never wait.";
    "nt.f.days" => "Days";
    "nt.f.from_time" => "From";
    "nt.f.to_time" => "To";
    "nt.f.add_quiet" => "Add quiet window";
    "nt.v.name" => "Enter a name.";
    "nt.v.host" => "Enter the SMTP server.";
    "nt.v.port" => "The port must be a number from 1 to 65535.";
    "nt.v.addresses" => "Enter the sender and at least one recipient.";
    "nt.v.ntfy" => "Enter the server and the topic.";
    "nt.v.chat_id" => "Enter the chat ID.";
    "nt.v.server" => "Enter the server address.";
    "nt.v.url" => "Enter the URL.";
    "nt.v.targets" => "Pick at least one target.";
    "sch.title" => "Scheduled tasks";
    "sch.subtitle" => "Scheduled commands, scripts and maintenance";
    "sch.open_text" => "Scheduled commands and scripts, scrubs, SMART tests, recycle bin cleanup, with run history.";
    "sch.create" => "Create task";
    "sch.search" => "Search tasks";
    "sch.tab.tasks" => "Tasks";
    "sch.tab.timers" => "System timers";
    "sch.tab.cron" => "Cron entries";
    "sch.empty" => "No scheduled tasks";
    "sch.empty_text" => "Run a command or a script on a schedule, at boot or on demand, and get notified when it fails.";
    "sch.col.enabled" => "Enabled";
    "sch.col.task" => "Task";
    "sch.col.action" => "Action";
    "sch.col.schedule" => "Schedule";
    "sch.col.last" => "Last run";
    "sch.col.started" => "Started";
    "sch.col.duration" => "Duration";
    "sch.col.result" => "Result";
    "sch.col.timer" => "Timer";
    "sch.col.next" => "Next run";
    "sch.col.source" => "File";
    "sch.next" => "Next: {when}";
    "sch.running" => "Running";
    "sch.started" => "{name}: running…";
    "sch.finished_ok" => "{name}: completed";
    "sch.finished_err" => "{name}: failed ({why})";
    "snap.run.started" => "Taking a snapshot of {name}…";
    "snap.run.ok" => "Snapshot of {name} created";
    "snap.run.failed" => "Snapshot of {name} failed: {why}";
    "sch.not_applied" => "Apply the pending changes first: the task does not exist on the system yet.";
    "sch.history" => "Run history";
    "sch.history_title" => "History of {name}";
    "sch.no_runs" => "No runs yet";
    "sch.no_runs_text" => "Runs appear here with their result and output.";
    "sch.show_output" => "Show output";
    "sch.output" => "Output";
    "sch.no_output" => "(no output)";
    "sch.truncated" => "Only the last {size} of the output are kept.";
    "sch.live" => "Live output";
    "sch.live.follow" => "Follow";
    "sch.live.connecting" => "Connecting";
    "sch.live.following" => "Live";
    "sch.live.ended" => "The run ended";
    "sch.live.idle" => "Nothing is running";
    "sch.live.failed" => "The output cannot be read: {why}";
    "sch.live.waiting" => "Waiting for output…";
    "sch.live.cr_note" => "A line that rewrites itself (rsync -P, curl) shows its last state when it ends.";
    "sch.live.dropped" => "Earlier lines are not shown here: the history keeps the end of the output.";
    "sch.run.ok" => "Succeeded";
    "sch.run.failed" => "Failed";
    "sch.run.timeout" => "Timed out";
    "sch.run.exit" => "Failed (exit {code})";
    "sch.run.signal" => "Killed ({sig})";
    "sch.delete_title" => "Delete task {name}?";
    "sch.delete_text" => "Its schedule is removed when the changes are applied. The run history stays.";
    "sch.delete_job_task" => "This task belongs to a snapshot, replication or backup job, or to the power schedule: change or delete it there.";
    "sch.sec.general" => "General";
    "sch.sec.action" => "Action";
    "sch.sec.schedule" => "Schedule";
    "sch.sec.notify" => "Notifications";
    "sch.f.name" => "Name";
    "sch.f.id" => "ID";
    "sch.f.id_help" => "Lowercase letters, digits and dashes. It cannot change later.";
    "sch.f.description" => "Description";
    "sch.f.enabled" => "Enabled";
    "sch.f.enabled_help" => "A disabled task keeps its settings and can still be run by hand.";
    "sch.f.command" => "Command";
    "sch.f.command_help" => "One line, run by /bin/sh.";
    "sch.f.script" => "Script";
    "sch.f.script_help" => "The #! line picks the interpreter (default /bin/sh). rynas stores it so that only root can change it.";
    "sch.f.user" => "Run as";
    "sch.f.user_help" => "root (administrators only) or a local user.";
    "sch.f.filesystem" => "Volume";
    "sch.f.filesystem_help" => "Mount point of the btrfs volume to scrub.";
    "sch.f.disk" => "Disk";
    "sch.f.test" => "Test";
    "sch.f.share_help" => "Empty: every shared folder with a recycle bin.";
    "sch.f.days" => "Keep for (days)";
    "sch.f.days_help" => "Empty: the folder's own retention.";
    "sch.f.trigger" => "Run";
    "sch.f.repeat" => "Repeat";
    "sch.f.time" => "Time";
    "sch.f.every_min" => "Every (minutes)";
    "sch.f.minute" => "Minute";
    "sch.f.minute_help" => "Minute past every hour.";
    "sch.f.weekdays" => "Days";
    "sch.f.dom" => "Day of the month";
    "sch.f.dom_help" => "Months without this day are skipped.";
    "sch.f.expression" => "systemd calendar expression";
    "sch.f.expression_help" => "OnCalendar= syntax, e.g. Mon..Fri *-*-* 02:30:00 or *-*-1,15 04:00:00.";
    "sch.f.cron" => "Cron expression";
    "sch.f.cron_help" => "Five fields (minute hour day month weekday) or @daily, @weekly…; converted to a systemd calendar.";
    "sch.f.persistent" => "Catch up missed runs";
    "sch.f.persistent_help" => "A run missed while the NAS was off happens at the next boot.";
    "sch.f.random" => "Random delay (seconds)";
    "sch.f.random_help" => "Spreads tasks that start at the same time.";
    "sch.f.boot_delay" => "Delay after boot (seconds)";
    "sch.f.boot_delay_help" => "Counted from the moment systemd started.";
    "sch.f.notify" => "Send a notification";
    "sch.f.notify_output" => "Attach the output";
    "sch.f.notify_output_help" => "The last lines of the output go with the notification.";
    "sch.f.timeout" => "Time limit (s)";
    "sch.f.timeout_help" => "The run is stopped after this long. Empty: no limit.";
    "sch.f.nice" => "CPU priority (nice)";
    "sch.f.nice_help" => "-20 to 19; empty: 10 for commands and scripts.";
    "sch.f.io_weight" => "Disk priority";
    "sch.f.io_weight_help" => "1 to 10000; empty: 100.";
    "sch.notify.on_failure" => "When it fails";
    "sch.notify.always" => "After every run";
    "sch.notify.never" => "Never";
    "sch.trig.calendar" => "On a schedule";
    "sch.trig.boot" => "At boot";
    "sch.trig.manual" => "Manually";
    "sch.trig.manual_help" => "No timer: the task runs only with Run now.";
    "sch.trig.boot_text" => "At boot (+{s} s)";
    "sch.rep.minutes" => "Every few minutes";
    "sch.rep.hourly" => "Every hour";
    "sch.rep.daily" => "Every day";
    "sch.rep.weekly" => "Every week";
    "sch.rep.monthly" => "Every month";
    "sch.rep.custom" => "Custom (systemd)";
    "sch.rep.cron" => "Cron syntax";
    "sch.desc.minutes" => "Every {n} min";
    "sch.desc.hourly" => "Hourly at :{m}";
    "sch.desc.daily" => "Daily at {time}";
    "sch.desc.weekly" => "{days} at {time}";
    "sch.desc.monthly" => "Monthly on day {d} at {time}";
    "sch.next_runs" => "Next runs";
    "sch.normalized" => "systemd reads it as";
    "sch.checking" => "Checking…";
    "sch.act.command" => "Command";
    "sch.act.script" => "Script";
    "sch.act.snapshot" => "Snapshot job";
    "sch.act.scrub" => "Volume data scrub";
    "sch.act.smart_test" => "SMART test";
    "sch.act.recycle_cleanup" => "Recycle bin cleanup";
    "sch.act.power_shutdown" => "Scheduled shutdown";
    "sch.act.backup" => "Backup job";
    "sch.act.dedup" => "Deduplication";
    "sch.managed_storage" => "Managed in Storage: change it there.";
    // Deduplication (D-DEDUP)
    "dd.title" => "Deduplication";
    "dd.short" => "Deduplicated";
    "dd.text" => "Finds identical data in the enabled shared folders and keeps one copy on disk. Runs only in its time window.";
    "dd.fs_name" => "Filesystem {id}";
    "dd.st.off" => "Off";
    "dd.st.on" => "On";
    "dd.st.paused" => "Paused";
    "dd.st.running" => "Running";
    "dd.st.estimating" => "Estimating";
    "dd.st.waiting" => "Waiting";
    "dd.engine_missing" => "The deduplication engine (oans) is not installed.";
    "dd.engine_unavailable" => "The deduplication engine (package rynas-oans) is not installed and no package source has it: put the package from the rynas release in /var/lib/rynas/packages.";
    "dd.engine_unsupported" => "oans {version} is outside the versions rynas was tested with ({min} to {max}): runs are refused.";
    "dd.install" => "Install";
    "dd.waiting_for" => "Waiting for {task} to finish";
    "dd.starting" => "Starting…";
    "dd.until" => "until {when}";
    "dd.ends" => "Stops";
    "dd.stopping" => "Stopping: the work done is being saved (up to 5 minutes).";
    "dd.ph.scanning" => "Looking for files";
    "dd.ph.hashing" => "Reading files";
    "dd.ph.dedupe" => "Sharing duplicates";
    "dd.eta" => "{time} left";
    "dd.freed" => "{size} freed";
    "dd.nothing_freed" => "nothing to free";
    "dd.checked" => "Checked";
    "dd.checked_line" => "{files} files, {size}";
    "dd.window" => "Time window";
    "dd.window_help" => "Runs start at the beginning of each block and stop at its end; the next block goes on from there.";
    "dd.window_empty" => "Choose at least one hour.";
    "dd.grid_on" => "May run";
    "dd.grid_off" => "Not running";
    "dd.none" => "None yet";
    "dd.total" => "Freed in total";
    "dd.next" => "Next window";
    "dd.estimate_result" => "Estimate";
    "dd.estimate_line" => "{size} duplicated in {files} of {scanned} files scanned ({pct} of {total})";
    "dd.logical_note" => "Sizes count data before compression; snapshots taken earlier keep their copy until they expire.";
    "dd.out.done" => "Done";
    "dd.out.window_ended" => "Window ended";
    "dd.out.preempted" => "Gave way";
    "dd.out.stopped" => "Stopped";
    "dd.out.partial" => "Partial";
    "dd.out.failed" => "Failed";
    "dd.out.skipped" => "Skipped";
    "dd.block.encrypted" => "Encrypted folders cannot be deduplicated.";
    "dd.block.nocow" => "Folders without copy-on-write cannot be deduplicated.";
    "dd.block.filesystem" => "Only btrfs and XFS folders can be deduplicated.";
    "dd.block.xfs_no_reflink" => "This XFS filesystem was made without reflink.";
    "dd.block.other_fs" => "The folder is on another filesystem than its volume.";
    "dd.block.not_mounted" => "The folder or its filesystem is not available.";
    "dd.warn.qgroups" => "btrfs quotas are on: with many shared extents, writes can stall for a while.";
    "dd.warn.replicated" => "A replicated folder is included: the next replication can take longer or send more.";
    "dd.warn.no_shares" => "No shared folder is enabled: turn it on in a folder's settings.";
    "dd.warn.blocked" => "Some enabled folders cannot be deduplicated and are left out.";
    "dd.warn.flash" => "The system disk is often flash memory: the index is written there.";
    "dd.estimate" => "Estimate";
    "dd.estimate_title" => "Estimate on {name}";
    "dd.estimate_text" => "Reads the folders and counts the duplicates without changing anything. Its work is reused by the first run.";
    "dd.run_title" => "Deduplicate {name} now";
    "dd.run_text" => "Runs now with low priority. Other work on the volume (scrub, replication, backup) goes first.";
    "dd.until_label" => "Stop";
    "dd.until_help" => "The work done is kept; the next window goes on.";
    "dd.until_window" => "At the end of the window";
    "dd.h1" => "After 1 hour";
    "dd.h2" => "After 2 hours";
    "dd.h4" => "After 4 hours";
    "dd.h8" => "After 8 hours";
    "dd.h24" => "After 24 hours";
    "dd.snapshots_note" => "Space used by older snapshots comes back only when they expire.";
    "dd.start" => "Start";
    "dd.stop" => "Stop";
    "dd.history" => "History";
    "dd.history_title" => "Deduplication history of {name}";
    "dd.by" => "for {task}";
    "dd.skipped_files" => "{n} files could not be read";
    "dd.settings" => "Settings";
    "dd.settings_title" => "Deduplication of {name}";
    "dd.enabled" => "Scheduled runs";
    "dd.enabled_help" => "Off: runs only when started by hand.";
    "dd.only_awake" => "Only if the disks are already awake";
    "dd.only_awake_help" => "Skips a window instead of waking disks in standby.";
    "dd.advanced" => "Advanced";
    "dd.min_size" => "Smallest file";
    "dd.min_size_help" => "Smaller files are skipped: they free little and change often.";
    "dd.block" => "Block size";
    "dd.block_help" => "Most volumes keep the default.";
    "dd.block_changed" => "A new block size makes the next run read everything again.";
    "dd.partial" => "Partial matches";
    "dd.partial_help" => "Also shares parts of files. Much more CPU and a bigger index.";
    "dd.zeroes" => "Skip empty blocks";
    "dd.zeroes_help" => "Faster on disk images and preallocated files.";
    "dd.cpu" => "CPU threads";
    "dd.io" => "Disk threads";
    "dd.io_help" => "Automatic adapts to the disks.";
    "dd.memory" => "Memory limit";
    "dd.auto" => "Automatic";
    "dd.n1" => "1";
    "dd.n2" => "2";
    "dd.n4" => "4";
    "dd.n8" => "8";
    "dd.mem.512" => "512 MiB";
    "dd.mem.1024" => "1 GiB";
    "dd.mem.2048" => "2 GiB";
    "dd.mem.4096" => "4 GiB";
    "dd.sz.4k" => "4 KiB";
    "dd.sz.16k" => "16 KiB";
    "dd.sz.32k" => "32 KiB";
    "dd.sz.64k" => "64 KiB";
    "dd.sz.128k" => "128 KiB";
    "dd.sz.256k" => "256 KiB";
    "dd.sz.512k" => "512 KiB";
    "dd.sz.1m" => "1 MiB";
    "dd.sz.4m" => "4 MiB";
    "dd.always_excluded" => "Never deduplicated";
    "dd.reset" => "Reset index";
    "dd.reset_title" => "Reset the index?";
    "dd.turn_off_title" => "Turn off deduplication of {name}?";
    "dd.reset_text" => "The index of known data is deleted: the next run reads everything again. The data is not touched.";
    "dd.reset_done" => "Index reset";
    "dd.turn_off_text" => "Deduplication of {name} stops. Space already freed stays free.";
    "dd.share_switch" => "Deduplicate this folder";
    "dd.share_switch_help" => "Identical data here and in the other enabled folders of the volume is kept once.";
    "dd.share_job_off" => "Deduplication of this volume is off.";
    "dd.include_recycle" => "Include the recycle bin";
    "dd.include_recycle_help" => "Off: files waiting in #recycle are skipped.";
    "dd.excludes" => "Skip";
    "dd.excludes_help" => "One pattern per line, relative to the folder (cache/, *.iso).";
    "task.storage.dedup" => "Deduplicate";
    "task.storage.dedup-estimate" => "Estimate deduplication";
    "task.storage.dedup-reset" => "Reset the deduplication index";
    "task.dedup.install" => "Install the deduplication engine";
    "nev.dedup.ok.title" => "Deduplication of {volume} done: {reclaimed} freed";
    "nev.dedup.partial.title" => "Deduplication of {volume} covered only part of its folders";
    "nev.dedup.failed.title" => "Deduplication of {volume} failed";
    "shares.size_shared" => "Includes data shared with other folders or snapshots (copies, deduplication): deleting the folder frees only the part that is not shared.";
    "sch.smart.short" => "Short";
    "sch.smart.long" => "Extended";
    "sch.all_shares" => "All shared folders";
    "sch.target" => "Target";
    "sch.internal_help" => "A maintenance task keeps its action: only the schedule and the options can change.";
    "sch.timers_help" => "systemd timers that rynas did not create, read only.";
    "sch.cron_help" => "Crontab lines found on the system, read only. A line with an equivalent schedule can become a rynas task (remove the cron line afterwards).";
    "sch.no_timers" => "No other timers";
    "sch.no_cron" => "No cron entries";
    "sch.convert" => "Create a task from this line";
    "sch.v.id" => "Use lowercase letters, digits and dashes (at most 48), starting with a letter or digit.";
    "sch.v.action" => "Pick an action.";
    "sch.v.command" => "Enter the command.";
    "sch.v.script" => "Enter the script.";
    "sch.v.filesystem" => "Enter the volume's mount point (an absolute path).";
    "sch.v.disk" => "Enter the disk (sda, nvme0n1).";
    "sch.v.days" => "The days must be a number.";
    "sch.v.delay" => "The delay must be a number of seconds.";
    "sch.v.calendar" => "The schedule is not valid.";
    "snap.title" => "Snapshots";
    "snap.subtitle" => "Point-in-time copies of shared folders, file restore and rollback";
    "snap.tab.snapshots" => "Snapshots";
    "snap.tab.jobs" => "Snapshot jobs";
    "snap.tab.system" => "System restore points";
    "snap.create_job" => "Create snapshot job";
    "snap.all_jobs" => "All jobs";
    "snap.refresh" => "Refresh";
    "snap.none" => "No snapshots";
    "snap.none_text" => "Snapshots appear here once a job has run. Jobs are applied with the pending changes.";
    "snap.col.snapshot" => "Snapshot";
    "snap.col.job" => "Job";
    "snap.col.created" => "Taken";
    "snap.col.retention" => "Retention";
    "snap.col.name" => "Name";
    "snap.col.size" => "Size";
    "snap.col.modified" => "Modified";
    "snap.col.mode" => "Permissions";
    "snap.system_point" => "System restore point";
    "snap.share_of" => "Shared folder {share}";
    "snap.readonly" => "Read-only";
    "snap.writable" => "Writable";
    "snap.latest" => "Latest";
    "snap.latest_at" => "Latest {when}";
    "snap.reason" => "Reason: {r}";
    "snap.why.apt" => "Before package changes";
    "snap.why.manual" => "Taken by hand";
    "snap.why.before_rollback" => "System before a rollback";
    "snap.browse" => "Browse";
    "snap.browse_title" => "Browse {id}";
    "snap.path" => "Folder";
    "snap.up" => "Parent folder";
    "snap.empty_dir" => "This folder is empty.";
    "snap.truncated" => "More than 10,000 entries: only the first ones by name are listed.";
    "snap.restore" => "Restore";
    "snap.restore_this" => "Restore this folder";
    "snap.restore_file" => "Restore {path}?";
    "snap.restore_folder" => "Restore folder {path}?";
    "snap.restore_text" => "The version in snapshot {id} goes back to the shared folder. Nothing is overwritten.";
    "snap.restore_mode" => "Where";
    "snap.mode.copy" => "Next to the current one";
    "snap.mode.replace" => "In place";
    "snap.mode.copy_help" => "Restored as “name (restored date)” next to the current version.";
    "snap.mode.replace_help" => "The current version is first renamed to name.rynas-before-restore-<time>, then the snapshot's version takes its place.";
    "snap.restored" => "{path} restored";
    "snap.rollback" => "Roll back";
    "snap.rollback_system" => "Roll the system back (reboot needed)";
    "snap.rollback_title" => "Roll back to {id}?";
    "snap.rollback_text" => "The whole shared folder goes back to this snapshot. Its current content is kept, renamed <folder>.rynas-rollback-<time>, until you delete it.";
    "snap.rollback_nfs" => "NFS clients must mount the folder again afterwards.";
    "snap.rolled_back" => "Rolled back to {id}";
    "snap.root_title" => "Roll the system back to {id}?";
    "snap.root_text" => "At the next reboot the system starts from this restore point: packages, kernel and settings go back to that moment. Shared folders and data volumes are not touched.";
    "snap.root_kernel" => "Kernel after the reboot";
    "snap.root_running" => "Kernel running now";
    "snap.root_steps" => "What happens";
    "snap.root_step1" => "The restore point is mounted read-only and its kernel is checked.";
    "snap.root_step2" => "The boot entry is pointed at that kernel. If that is refused, nothing changes.";
    "snap.root_step3" => "The current system is kept as a new restore point (“System before a rollback”), then the restore point becomes the system subvolume (@rootfs).";
    "snap.root_step4" => "You reboot the NAS to start the restored system.";
    "snap.root_reboot" => "A reboot is needed to finish. Until then the current system keeps running.";
    "snap.root_done" => "At the next reboot the system starts from {id} (kernel {kernel}). Reboot now to finish the rollback.";
    "snap.root_refused_title" => "{id} cannot be restored from here";
    "snap.refusal.no_boot_layer" => "This system cannot be rolled back from the web interface: the restore point is there, going back to it is a console procedure.";
    "snap.refusal.no_kernel" => "The restore point has no kernel image.";
    "snap.refusal.no_modules" => "The restore point lacks the modules of its kernel (/lib/modules): it could not boot.";
    "snap.refusal.not_readonly" => "Not a read-only snapshot.";
    "snap.delete_title" => "Delete snapshot {id}?";
    "snap.delete_text" => "Its files can no longer be restored. This cannot be undone.";
    "snap.deleted" => "Snapshot {id} deleted";
    "snap.del.latest" => "The newest snapshot of a job is kept: it is the base of the next one";
    "snap.del.writable" => "Not a read-only snapshot: left alone";
    "snap.del.not_subvolume" => "Not a subvolume: left alone";
    "snap.jobs_help" => "Each job snapshots one shared folder or subvolume on its schedule and keeps hourly, daily, weekly, monthly and yearly snapshots. A shared folder with SMB also gets Windows “Previous versions”.";
    "snap.no_jobs" => "No snapshot jobs";
    "snap.no_jobs_text" => "Protect a shared folder against deleted files and ransomware with hourly snapshots.";
    "snap.show_snapshots" => "Show the snapshots of this job";
    "snap.delete_job" => "Delete snapshot job {name}?";
    "snap.delete_job_text" => "Its schedule and the SMB previous versions go with it. Snapshots already on disk stay.";
    "snap.ret.none" => "Latest only";
    "snap.u.h" => "hourly";
    "snap.u.d" => "daily";
    "snap.u.w" => "weekly";
    "snap.u.m" => "monthly";
    "snap.u.y" => "yearly";
    "snap.preset.standard" => "Standard";
    "snap.preset.light" => "Light";
    "snap.preset.long" => "Long";
    "snap.preset.custom" => "Custom";
    "snap.preset_keeps" => "Keeps {counts}; everything younger than {min} stays.";
    "snap.preview" => "Preview what would be deleted";
    "snap.preview_counts" => "Now this retention keeps {keep} snapshots and deletes {delete}.";
    "snap.preview_empty" => "There are no snapshots of this job yet.";
    "snap.sec.source" => "What to protect";
    "snap.f.name" => "Job name";
    "snap.f.name_help" => "Also the snapshot name prefix (name.YYYYMMDDTHHMM). It cannot change later.";
    "snap.f.source" => "Source";
    "snap.src.path" => "Subvolume path";
    "snap.f.pick_share" => "Choose a shared folder";
    "snap.f.share_help" => "Its folder must be a btrfs subvolume.";
    "snap.f.subvolume" => "Subvolume";
    "snap.f.subvolume_help" => "Absolute path of a btrfs subvolume.";
    "snap.f.dir" => "Snapshot folder";
    "snap.f.dir_help" => "On the same volume. Empty: .snapshots/<job> next to the subvolume.";
    "snap.f.enabled" => "Take snapshots on schedule";
    "snap.f.enabled_help" => "Off: snapshots are taken only with Take a snapshot now.";
    "snap.f.preset" => "Keep";
    "snap.f.hourly" => "Hourly";
    "snap.f.daily" => "Daily";
    "snap.f.weekly" => "Weekly";
    "snap.f.monthly" => "Monthly";
    "snap.f.yearly" => "Yearly";
    "snap.f.min" => "Always keep";
    "snap.f.min_help" => "latest, all, or an age such as 1d, 12h, 2w.";
    "snap.v.name" => "Use letters, digits, _ and - (at most 48), starting with a letter or digit.";
    "snap.v.source" => "Choose a shared folder or enter an absolute subvolume path.";
    "snap.system_help" => "Before apt installs or upgrades packages, rynas takes a read-only snapshot of the system. Where the boot setup allows it, the system can go back to one of them from the snapshot list (kernel included, reboot needed); elsewhere it is a console procedure.";
    "snap.system_enabled" => "Take restore points before updates";
    "snap.system_enabled_help" => "Requires a btrfs root filesystem.";
    "snap.system_keep" => "Restore points kept";
    "snap.system_keep_help" => "The oldest beyond this number are deleted after a new one.";
    "snap.system_prefix" => "Name prefix";
    // Snapshot replication
    "snap.tab.replication" => "Replication";
    "sch.act.replicate" => "Snapshot replication";
    "task.replication.run" => "Replicate snapshots";
    "task.replication.restore" => "Restore from a replica";
    "task.replication.promote" => "Promote a replica";
    "nev.replication.failed.title" => "Replication {name} failed";
    "nev.replication.ok.title" => "Replication {name} works again";
    "repl.intro" => "A second copy of a job's snapshots on another btrfs volume or another server, with its own retention. Snapshots on the same disks are not a backup.";
    "repl.add" => "Add replica target";
    "repl.none" => "No replica targets";
    "repl.none_text" => "Add a target to keep a copy of the snapshots on another volume or server.";
    "repl.none_no_jobs" => "Create a snapshot job first: replication copies its snapshots.";
    "repl.col.target" => "Target";
    "repl.col.when" => "When";
    "repl.col.last" => "Last transfer";
    "repl.col.replica" => "Replica";
    "repl.after_snapshots" => "After each snapshot";
    "repl.held.one" => "{n} snapshot kept on the source until the target is reachable again";
    "repl.held.other" => "{n} snapshots kept on the source until the target is reachable again";
    "repl.key.line_needs_path" => "Fill in the folder on the target above: the authorized_keys line limits the key to that folder.";
    "repl.never" => "Never";
    "repl.sent.one" => "{n} snapshot sent";
    "repl.sent.other" => "{n} snapshots sent";
    "repl.off" => "Off";
    "repl.failing" => "Failing";
    "repl.ok" => "Replicated";
    "repl.run_done" => "Replication finished";
    "repl.replicas" => "Replicas";
    "repl.replicas_remote" => "Replicas on another server are browsed on that server";
    "repl.replicas_title" => "Replicas of {job} on {id}";
    "repl.replicas_text" => "Read-only copies in {path}. Restore copies a file or folder back next to the original; Promote makes a writable copy as a new folder on that volume.";
    "repl.no_replicas" => "No replicas yet: they appear after the first transfer.";
    "repl.incomplete" => "Interrupted transfer";
    "repl.back" => "Back to the replicas";
    "repl.restore_title" => "Restore {path} from the replica?";
    "repl.restore_text" => "A copy goes next to the original in the shared folder, with the restore date in its name. Nothing is overwritten.";
    "repl.promote" => "Promote";
    "repl.promote_title" => "Promote {name}?";
    "repl.promote_text" => "Creates a writable copy of this replica as a new folder at the top of the target volume, ready to be shared. The replica itself stays read-only, so later transfers continue.";
    "repl.promoted" => "Folder {folder} created";
    "repl.delete_title" => "Remove the target {id} of {job}?";
    "repl.delete_text" => "Replication to this target stops. The replicas already there are kept: delete them on the target if you no longer need them.";
    "repl.sec.target" => "Target";
    "repl.sec.key" => "Access to the server";
    "repl.sec.retention" => "Retention on the target";
    "repl.sec.transfer" => "Transfer";
    "repl.f.job_help" => "The snapshots of this job are copied.";
    "repl.f.id" => "Target name";
    "repl.f.id_help" => "Lowercase letters, digits and “-”, for example hdd or offsite.";
    "repl.f.kind" => "Where";
    "repl.kind.local" => "Another volume";
    "repl.kind.ssh" => "Another server (SSH)";
    "repl.f.host" => "Server";
    "repl.f.port" => "Port";
    "repl.f.user" => "User";
    "repl.f.user_help" => "On the server; root, or a user allowed to run btrfs with sudo.";
    "repl.f.path" => "Folder";
    "repl.f.path_local_help" => "On another btrfs volume of this NAS; created at the first transfer. Not on the volume of the source.";
    "repl.f.path_remote_help" => "On a btrfs filesystem of the server. It must exist: create it there (for example btrfs subvolume create).";
    "repl.f.sudo" => "Run btrfs with sudo";
    "repl.f.sudo_help" => "For a user other than root.";
    "repl.f.host_keys" => "Server identity";
    "repl.f.host_keys_none" => "Not confirmed yet: transfers are refused until the server's key is confirmed.";
    "repl.f.scan" => "Read the server's key";
    "repl.f.scan_check" => "Compare these fingerprints with the output of `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the server before trusting them.";
    "repl.f.scan_none" => "The server answered no key.";
    "repl.f.trust" => "Trust these keys";
    "repl.f.enabled" => "Replication on";
    "repl.f.after" => "Run after each snapshot";
    "repl.f.after_help" => "Off: the target has its own schedule (for example every night).";
    "repl.f.retention_help" => "Independent of the snapshots on the source. Only replicas made by rynas are ever deleted on the target.";
    "repl.f.compress" => "Compression on the network";
    "repl.compress.no" => "None";
    "repl.compress.zstd" => "zstd";
    "repl.compress.lz4" => "lz4";
    "repl.compress.gzip" => "gzip";
    "repl.f.rate" => "Bandwidth limit (KiB/s)";
    "repl.f.rate_help" => "Empty or 0: no limit.";
    "repl.f.folder_help" => "At the top of the target volume; it must not exist.";
    "repl.key.none" => "rynas connects with its own key pair, used only for replication.";
    "repl.key.create" => "Create the key";
    "repl.key.public" => "Public key of this NAS";
    "repl.key.line" => "Line for the server";
    "repl.key.line_help" => "Add it to `{file}` on the server: the key can then only receive and prune replicas in this folder.";
    "repl.key.filter_version" => "The server needs btrbk 0.32.7 or later: older `ssh_filter_btrbk.sh` versions let a restricted key run any command.";
    "repl.key.sudo_help" => "On the server, allow {user} to run btrfs without a password, for example `{user} ALL=(root) NOPASSWD: /usr/bin/btrfs` in a file of `/etc/sudoers.d`.";
    "repl.copy" => "Copy";
    "repl.copied" => "Copied";
    "repl.hint.ssh_key" => "The server refuses the key: add the line for the server to its `authorized_keys`.";
    "repl.hint.host_key" => "The server's identity is not confirmed or has changed: read and confirm its key again.";
    "repl.hint.unreachable" => "The server cannot be reached: check the name, the port and that it is on.";
    "repl.hint.filter" => "The server's command filter refused a command: check the folder in the line and that its btrbk is 0.32.7 or later.";
    "repl.hint.target_missing" => "The target folder does not exist: create it on the target (`btrfs subvolume create`).";
    "repl.hint.not_btrfs" => "The target is not on btrfs: replication needs btrfs on both sides.";
    "repl.hint.no_space" => "The target is full: free space there or keep fewer replicas.";
    "repl.hint.busy" => "Another run of this job was still going: the next run tries again.";
    "repl.hint.btrbk" => "btrbk is missing or too old on this NAS or on the server.";
    "repl.hint.other" => "See the task history for the full output.";
    "repl.warn.not_btrfs" => "Not on btrfs: replication needs a btrfs target.";
    "repl.warn.same_fs" => "On the volume of the source: not a second copy.";
    "repl.warn.not_mounted" => "No volume is mounted there: mount it before the next transfer.";
    "repl.warn.no_host_key" => "Server identity not confirmed.";
    "repl.v.id" => "Use 1-24 lowercase letters, digits and “-”.";
    "repl.v.path" => "Enter the absolute path of the target folder.";
    "repl.v.host" => "Enter the server name or address.";
    "repl.v.port" => "Enter a port from 1 to 65535.";
    "repl.v.rate" => "Enter a number of KiB/s.";
    // Backup (restic): Backup › Another NAS or cloud (bk.*) and its task names
    "nav.backup" => "Backup";
    "bk.title" => "Backup";
    "bk.subtitle" => "Encrypted, versioned copies of this NAS elsewhere, and the backups PCs keep here";
    "bk.tab.restic" => "Another NAS or cloud";
    "bk.help" => "Each job copies the chosen folders into an encrypted repository. Only what changed is sent, and every run is a version you can restore. The repository opens with restic or rustic on any computer, with its password.";
    "bk.create" => "Create backup job";
    "bk.crypt_note" => "Encrypted shares are backed up as their encrypted data (the folder shown next to them), locked or not: a restore gives that folder back, readable with the folder's password or recovery key.";
    "bk.empty" => "No backup jobs yet";
    "bk.empty_text" => "Create a job to copy shared folders, Docker app data or the rynas configuration to another NAS, a USB disk or a cloud storage.";
    "bk.no_offsite" => "No job copies data off this NAS: a fire or a theft would take the backups too. Add a job to another NAS or to the cloud.";
    "bk.col.job" => "Job and sources";
    "bk.col.where" => "Destination";
    "bk.col.size" => "Repository";
    "bk.dest.local" => "USB or local disk";
    "bk.dest.rest" => "REST server";
    "bk.dest.sftp" => "SFTP";
    "bk.dest.s3" => "S3 compatible";
    "bk.dest.b2" => "Backblaze B2";
    "bk.dest.rclone" => "rclone (Google Drive, OneDrive…)";
    "bk.u.last" => "latest";
    "bk.u.h" => "h";
    "bk.u.d" => "d";
    "bk.u.w" => "w";
    "bk.u.m" => "m";
    "bk.u.y" => "y";
    "bk.src.config" => "rynas configuration";
    "bk.src.config_help" => "Settings, users, shares and jobs, encrypted with the repository password.";
    "bk.failed" => "Failed";
    "bk.never" => "Never run";
    "bk.paused" => "Paused";
    "bk.versions_n" => "{n} versions";
    "bk.check.bad_at" => "Check failed {when}";
    "bk.run.started" => "Backup {name} started";
    "bk.run.ok" => "Backup {name} completed";
    "bk.run.failed" => "Backup {name} failed: {why}";
    "bk.versions" => "Versions and restore";
    "bk.versions_of" => "Versions of {name}";
    "bk.check" => "Check integrity";
    "bk.check.title" => "Check {name}";
    "bk.check.text" => "The check reads the repository's index and structure; reading the data back too finds damaged files at the destination, and takes longer and costs traffic.";
    "bk.check.read" => "Data read back";
    "bk.check.structure" => "Structure only";
    "bk.check.five" => "5 % of the data";
    "bk.check.all" => "All the data";
    "bk.check.started" => "Check of {name} started";
    "bk.check.done" => "Check of {name}: no errors";
    "bk.delete" => "Delete job {name}?";
    "bk.delete_text" => "The job and its schedule are removed. The repository and its versions stay at the destination.";
    "bk.sec.what" => "What to back up";
    "bk.sec.where" => "Where";
    "bk.sec.encryption" => "Encryption";
    "bk.sec.versions" => "Versions to keep";
    "bk.f.name_help" => "Lowercase letters, digits and -, up to 32.";
    "bk.f.description" => "Description";
    "bk.f.folders" => "Other folders";
    "bk.f.folders_help" => "One absolute path per line, e.g. the Docker apps folder.";
    "bk.f.dest" => "Destination type";
    "bk.f.path" => "Folder";
    "bk.f.path_help" => "A folder on a USB disk or another volume; it is created when missing.";
    "bk.f.path_on" => "On a volume";
    "bk.f.same_disk" => "This folder is on the system disk: the backup protects against deleted or encrypted files, not against a failure of that disk. Keep another copy elsewhere.";
    "bk.f.url" => "Server address";
    "bk.f.url_help" => "A restic REST server (rest-server), also on another NAS. Start it with --append-only so that this NAS can add versions but never delete them.";
    "bk.f.user" => "User";
    "bk.f.password" => "Password";
    "bk.f.secret_help" => "Stored encrypted on this NAS, never shown again.";
    "bk.f.secret_kept" => "A secret is stored: leave empty to keep it.";
    "bk.f.host" => "Server";
    "bk.f.port" => "Port";
    "bk.f.folder" => "Folder on the server";
    "bk.f.ssh_key" => "Key of this NAS";
    "bk.f.ssh_key_help" => "rynas logs in with a key pair: generate it, then add the public key to the server's ~/.ssh/authorized_keys.";
    "bk.f.ssh_key_paste" => "Add this line to ~/.ssh/authorized_keys of the user on the server. At the first connection the server's key is remembered.";
    "bk.f.ssh_generate" => "Generate key pair";
    "bk.f.endpoint" => "Endpoint";
    "bk.f.endpoint_help" => "Empty for AWS; the service's S3 address otherwise (MinIO, Wasabi, Backblaze S3…).";
    "bk.f.bucket" => "Bucket";
    "bk.f.bucket_id" => "Bucket ID";
    "bk.f.region" => "Region";
    "bk.f.prefix" => "Folder in the bucket";
    "bk.f.access_key" => "Access key";
    "bk.f.secret_key" => "Secret key";
    "bk.f.key_id" => "Application key ID";
    "bk.f.app_key" => "Application key";
    "bk.f.remote" => "rclone remote";
    "bk.f.remote_help" => "name:folder of a remote in the configuration below (rclone must be installed).";
    "bk.f.rclone_config" => "rclone configuration";
    "bk.f.rclone_help" => "The remote's section of rclone.conf, with its token: create it with rclone config on a computer with a browser. Stored encrypted.";
    "bk.f.rclone_kept" => "A configuration is stored: leave empty to keep it.";
    "bk.test" => "Test connection";
    "bk.test.empty" => "Reachable: the repository will be created at the first run.";
    "bk.test.opens" => "Reachable: an existing repository opens with this password ({n} versions).";
    "bk.test.locked" => "A repository is already there: give its password to use it.";
    "bk.test.failed" => "Not reachable";
    "bk.f.pw_mode" => "Repository password";
    "bk.pw.generate" => "Generate a strong one";
    "bk.pw.own" => "Use my own";
    "bk.pw.generate_help" => "rynas creates a random password and shows it once after saving.";
    "bk.pw.kept" => "Stored encrypted on this NAS. It cannot change: it is the key of the versions already in the repository (for another password, create a new job).";
    "bk.f.dest_change" => "Changing the destination asks for its secrets again (passwords, keys, rclone configuration): the stored ones belong to the old destination.";
    "bk.f.ssh_replace" => "A key pair exists. A new one replaces it: the server's authorized_keys must then get the new public key.";
    "bk.f.ssh_replace_go" => "Replace the key";
    "bk.delete_password" => "Without the repository password the versions left at the destination cannot be read. Keep a copy of it if you may need them.";
    "bk.interrupted" => "Interrupted";
    "bk.interrupted_text" => "{when} · the service stopped during the run; the system was put back as before";
    "bk.warnings_n" => "Warnings of the last run: {n}";
    "bk.all_hosts" => "Show other NAS";
    "bk.all_hosts_help" => "Also the versions other systems saved in the same repository.";
    "bk.restore.original_confirm" => "I understand that files at the original place are replaced";
    "bk.restore.overwrite" => "Overwrite and restore";
    "bk.f.repo_password" => "Password";
    "bk.f.repo_password_help" => "At least 8 characters; the password of an existing repository to add to it.";
    "bk.pw.title" => "Save the repository password";
    "bk.pw.warning" => "Without this password the backup cannot be restored, not even by you: if this NAS is lost, it is the only key to your data. Keep it outside this NAS (password manager, printed copy).";
    "bk.pw.download" => "Download as a file";
    "bk.pw.file_note" => "rynas backup repository password (restic format). Keep it safe.";
    "bk.pw.saved" => "I have saved the password";
    "bk.pw.done" => "Done";
    "bk.copy" => "Copy";
    "bk.copied" => "Copied to the clipboard";
    "bk.f.enabled" => "Run on schedule";
    "bk.f.enabled_help" => "Off: only Back up now runs it.";
    "bk.f.keep_help" => "After each run, older versions beyond these counts are forgotten; their space is freed by the next cleanup.";
    "bk.f.keep_last" => "Latest";
    "bk.f.keep_hourly" => "Hourly";
    "bk.f.keep_daily" => "Daily";
    "bk.f.keep_weekly" => "Weekly";
    "bk.f.keep_monthly" => "Monthly";
    "bk.f.keep_yearly" => "Yearly";
    "bk.f.smart" => "Use the recommended counts";
    "bk.f.use_snapshot" => "Read from a snapshot";
    "bk.f.use_snapshot_help" => "Each folder that is a btrfs subvolume is read from a temporary read-only snapshot: files changing during the backup (databases) stay consistent.";
    "bk.f.stop_stacks" => "Docker stacks to stop";
    "bk.f.stop_stacks_help" => "Comma-separated; stopped while the snapshot is taken (or during the whole backup without one), then started again.";
    "bk.f.excludes" => "Exclude";
    "bk.f.excludes_help" => "One pattern per line. Recycle bins, snapshot folders and folders with a CACHEDIR.TAG or .nobackup file are always left out.";
    "bk.f.bandwidth" => "Upload limit (KiB/s, 0 none)";
    "bk.f.prune_days" => "Clean up every (days)";
    "bk.f.check_days" => "Check every (days, 0 never)";
    "bk.f.read_pct" => "Data read by each check (%)";
    "bk.f.maintenance_help" => "The upload limit applies to S3, B2, SFTP and rclone. Clean-ups and checks run after a backup, one at a time per repository.";
    "bk.v.name" => "Give the job a name: lowercase letters, digits and -.";
    "bk.v.folder" => "Other folders must be absolute paths.";
    "bk.v.sources" => "Choose at least one thing to back up.";
    "bk.v.retention" => "Keep at least one kind of version.";
    "bk.v.password" => "The repository password needs at least 8 characters.";
    "bk.v.restore_folder" => "Enter an absolute folder path.";
    "bk.no_versions" => "No versions yet";
    "bk.no_versions_text" => "The first run of the job creates the first version.";
    "bk.all_versions" => "All versions";
    "bk.up" => "Up";
    "bk.pick" => "Select";
    "bk.download" => "Download";
    "bk.restore" => "Restore";
    "bk.restore_all" => "Restore this version";
    "bk.restore_n" => "Restore {n} selected";
    "bk.restore.to" => "Restore to";
    "bk.restore.original" => "Original place";
    "bk.restore.folder" => "Another folder";
    "bk.restore.folder_path" => "Target folder";
    "bk.restore.folder_help" => "The files go below it with their full path, e.g. /volume1/restore/volume1/docs.";
    "bk.restore.original_warn" => "Files that changed since this version are overwritten; newer files are kept.";
    "bk.restore.all_text" => "Everything in this version is restored.";
    "bk.restore.n_text" => "{n} selected items are restored with their folders.";
    "bk.restore.started" => "Restore started";
    "bk.restore.done" => "Restore completed";
    "task.backup.run" => "Back up";
    "task.backup.check" => "Check backup integrity";
    "task.backup.restore" => "Restore from backup";
    // network page
    "nav.network" => "Network";
    "net.title" => "Network";
    "net.subtitle" => "Interfaces, addresses and rescue paths. Every change is tested and rolled back unless you confirm it.";
    "net.tab_ifaces" => "Interfaces";
    "net.tab_revisions" => "Revisions";
    "net.tab_rescue" => "Rescue access";
    "net.tab_tools" => "Diagnostics";
    "net.summary" => "Overview";
    "net.services" => "Network services";
    "net.hostname" => "Host name";
    "net.mgmt" => "This session comes through";
    "net.mgmt_unknown" => "Unknown (local or proxied connection)";
    "net.gateway" => "Default gateway";
    "net.dns" => "DNS servers";
    "net.search" => "Search domains";
    "net.lkg" => "Last known good revision";
    "net.lkg_none" => "None yet";
    "net.svc_networkd" => "systemd-networkd";
    "net.svc_resolved" => "systemd-resolved";
    "net.svc_ifupdown" => "ifupdown (networking)";
    "net.svc_nm" => "NetworkManager";
    "net.svc_avahi" => "Avahi (mDNS)";
    "net.running" => "Running";
    "net.not_running" => "Not running";
    "net.reason.timeout" => "not confirmed in time";
    "net.reason.manual" => "rolled back by hand";
    "net.reason.verify_failed" => "the new settings did not come up";
    "net.reason.boot_guard" => "a test was interrupted by a reboot";
    "net.reason.boot_check" => "no routable address after the boot";
    "net.reason.watchdog" => "the network was unreachable after the boot";
    "net.reason.other" => "automatic";
    "net.rolled_back_at" => "Network settings rolled back on {date} ({reason}): revision #{rev} is active again.";
    "net.rollback_unverified" => "The restored state could not be verified: check the interfaces.";
    "net.adopt_title" => "Network configured by ifupdown";
    "net.adopt_ask" => "Let rynas manage the network?";
    "net.adopt_text" => "/etc/network/interfaces configures the ports, so they are read-only here. Let rynas manage them to edit them safely (same addresses, with a timed test).";
    "net.adopt_btn" => "Let rynas manage…";
    "net.adopt_dialog_text" => "rynas writes systemd-networkd files with the same addresses, keeps the original as interfaces.rynas-bak and switches from ifupdown to systemd-networkd. Bonds come along; bridges, VLANs and static IPv6 are not migrated yet.";
    "net.adopt_when" => "When";
    "net.adopt_next_boot" => "At the next reboot (recommended)";
    "net.adopt_next_boot_help" => "Safest when you are connected over the network: if no port has a routable address 3 minutes after the boot, ifupdown comes back.";
    "net.adopt_now" => "Now, with a timed test";
    "net.adopt_now_help" => "The network restarts now; confirm within the time limit or ifupdown comes back.";
    "net.adopt_go" => "Let rynas manage";
    "net.adopt_scheduled" => "Migration planned for the next reboot: {ifaces}.";
    "net.pending_title" => "Network changes not applied yet";
    "net.pending_text" => "Pending on {ifaces}. Nothing changes until you apply them.";
    "net.review_apply" => "Review and apply…";
    "net.discard_title" => "Discard the network changes?";
    "net.discard_text" => "The pending settings of every interface are discarded. The running network does not change.";
    "net.discarded" => "Network changes discarded";
    "net.apply_title" => "Apply network settings";
    "net.timeout" => "Time to confirm";
    "net.timeout_help" => "Without a confirmation in this time the previous settings come back by themselves.";
    "net.seconds" => "{n} s";
    "net.mgmt_touched" => "You are connected through {iface}. After Apply this page may lose the connection: open the NAS at {addrs} and confirm there.";
    "net.mgmt_touched_dhcp" => "You are connected through {iface}. After Apply the address may change (DHCP): reconnect through the host name, the link-local address or the address your router assigns, and confirm there.";
    "net.mgmt_unknown_apply" => "rynas cannot tell which interface this session uses (local or proxied connection), so the timed test is mandatory.";
    "net.skip_test" => "Apply without the timed test";
    "net.skip_test_help" => "Only for interfaces this session does not use. There is no automatic rollback.";
    "net.test_mandatory" => "The timed test cannot be skipped: the change touches the connection you are using.";
    "net.apply_now" => "Apply now";
    "net.apply_test" => "Apply with a {n} s test";
    "net.trial_title" => "Confirm the new network settings";
    "net.trial_text" => "Confirm within {n} s or they are rolled back to revision {rev}.";
    "net.trial_deadline" => "Time is up";
    "net.trial_deadline_text" => "Not confirmed: revision {rev} is being restored.";
    "net.trial_applying" => "Applying network settings";
    "net.trial_applying_text" => "Applied without a timed test. This takes a few seconds.";
    "net.trial_starting" => "Checking the new addresses and writing the configuration…";
    "net.trial_who" => "Started by {user} on {ifaces}.";
    "net.previous" => "the previous settings";
    "net.left_sr" => "About {n} seconds left to confirm.";
    "net.confirm" => "Keep these settings";
    "rollback_now" => "Roll back now";
    "net.no_token" => "Confirm from the browser where the change was applied, or roll it back here.";
    "net.confirmed" => "Network settings kept: revision #{rev} is the last known good";
    "net.confirm_conflict" => "The confirmation must come over the new network: open the NAS at one of the new addresses below and confirm there.";
    "net.confirm_token" => "This confirmation link is not valid for the running test.";
    "net.kept" => "The new network settings are kept.";
    "net.applied" => "Network settings applied";
    "net.not_kept" => "The new network settings were not kept: the previous ones were restored.";
    "net.lost_title" => "Connection lost.";
    "net.lost_text" => "Expected if the address of this page changed. This page keeps trying by itself; meanwhile open the NAS at a new address and confirm there.";
    "net.lost_after" => "Not confirmed in time: the previous settings are coming back. This page reconnects by itself, or reopen {url}.";
    "net.cands_title" => "Where the NAS answers after the change:";
    "net.cands_text" => "Each link opens the confirmation page (sign in again if asked).";
    "net.boot_title" => "Migration planned for the next reboot";
    "net.boot_text" => "At the next boot systemd-networkd takes over {ifaces}. If no port has a routable address 3 minutes later, ifupdown comes back by itself.";
    "net.boot_cancel" => "Cancel the migration";
    "net.boot_who" => "Planned by {user} on {when}.";
    "net.boot_cancelled" => "The migration planned for the next reboot is canceled.";
    "net.booted_title" => "Network migrated at this boot";
    "net.booted_text" => "systemd-networkd now manages {ifaces}. When the countdown ends the migration stays if a port has a routable address; otherwise ifupdown comes back.";
    "net.booted_text_n" => "systemd-networkd now manages {ifaces}. When the countdown ends the migration stays if a port has a routable address; otherwise ifupdown comes back.";
    "net.booted_left_sr" => "About {n} seconds before the migration stays or ifupdown comes back.";
    "net.booted_planned" => "{user} planned it for the next reboot from the Network page.";
    "net.booted_keep" => "Keep it now";
    "net.booted_back" => "Go back to ifupdown";
    "net.col_iface" => "Interface";
    "net.col_speed" => "Speed";
    "net.col_mac" => "MAC address";
    "net.col_addresses" => "Addresses";
    "net.col_config" => "Configuration";
    "net.col_kind" => "Type";
    "net.col_ifaces" => "Interfaces";
    "net.k.ethernet" => "Ethernet";
    "net.k.bond" => "Bond";
    "net.k.bridge" => "Bridge";
    "net.k.vlan" => "VLAN";
    "net.k.loopback" => "Loopback";
    "net.k.wireless" => "Wireless";
    "net.k.virtual" => "Virtual";
    "net.k.other" => "Other";
    "net.b.networkd" => "systemd-networkd";
    "net.b.ifupdown" => "ifupdown";
    "net.b.nm" => "NetworkManager";
    "net.b.none" => "Not configured";
    "net.m.dhcp" => "DHCP";
    "net.m.static" => "Static";
    "net.m.disabled" => "Off (link-local only)";
    "net.st_up" => "Connected";
    "net.st_dormant" => "Cable, not up";
    "net.st_no_cable" => "No cable";
    "net.st_down" => "Down";
    "net.st_unknown" => "Unknown";
    "net.half_duplex" => "half duplex";
    "net.dynamic" => "dynamic";
    "net.configured" => "configured";
    "net.no_address" => "No address";
    "net.read_only" => "Read-only";
    "net.this_connection" => "This connection";
    "net.this_connection_help" => "You reach rynas through this interface: changes to it are always tested with a timed confirmation.";
    "net.show_virtual" => "Show virtual interfaces";
    "net.hidden_virtual" => "{n} virtual interfaces hidden";
    "net.none_text" => "The system reports no network interface rynas can show.";
    "net.details_title" => "Details of {name}";
    "net.read_only_text" => "This interface is read-only in rynas.";
    "net.ro.loopback" => "The loopback interface cannot be configured.";
    "net.ro.enslaved" => "It is a member of {master}: change it from the bond.";
    "net.ro.not_ethernet" => "Only Ethernet ports and the bonds rynas made can be configured in this version.";
    "net.ro.ifupdown" => "Configured by ifupdown (/etc/network/interfaces): use Let rynas manage first.";
    "net.ro.network_manager" => "Managed by NetworkManager: rynas does not change it.";
    "net.ro.no_networkd" => "systemd-networkd is not running: rynas configures the interfaces through it.";
    "net.mgmt_edit" => "You are connected through this interface. The change is tested with a timed confirmation; if its address changes you will reconnect at the new one.";
    "net.has_pending" => "This interface already has a pending change: you are editing it.";
    "net.sec_ipv4" => "IPv4";
    "net.sec_diff" => "Changes";
    "net.sec_details" => "Details";
    "net.sec_applied" => "Applied settings";
    "net.sec_ifupdown" => "ifupdown configuration (read-only)";
    "net.f_method" => "IPv4 address";
    "net.f_addresses" => "Addresses";
    "net.f_gateway" => "Gateway";
    "net.f_mtu" => "MTU";
    "net.method_help" => "Every port also keeps a link-local rescue address.";
    "net.addresses_help" => "One per line, with the prefix (192.0.2.10/24).";
    "net.gateway_help" => "Optional; must be inside one of the addresses' subnets.";
    "net.dns_help" => "One per line. With DHCP they are added to the servers the lease gives.";
    "net.mtu_help" => "Empty: the driver default (usually 1500).";
    "net.mtu_help_max" => "Empty: the driver default (usually 1500). This port accepts up to {max}.";
    "net.mtu_default" => "default";
    "net.max" => "max";
    "net.stage" => "Stage change";
    "net.staged" => "Change pending: apply it from the Network page";
    "net.revert" => "Discard the pending change";
    "net.no_changes" => "No changes.";
    "net.phase_note" => "IPv6 settings, VLANs and bridges are shown read-only for now.";
    "net.driver" => "Driver";
    "net.backend" => "Managed by";
    "net.config_file" => "Configuration file";
    "net.link_local" => "Link-local rescue address";
    "net.dns_live" => "DNS of this link";
    "net.ifu_method" => "Method";
    "net.ifu_bond" => "Bond members";
    "net.ifu_bridge" => "Bridge ports";
    "net.ifu_vlan" => "VLAN parent";
    "net.ifu_file" => "File";
    "net.bond_create" => "Create bond";
    "net.bond_create_title" => "New bond {name}";
    "net.no_ports_free" => "No free Ethernet port for a bond.";
    "net.f_members" => "Member ports";
    "net.members_help" => "Physical ports that work as one link. The bond takes the address settings below; the ports lose their own.";
    "net.f_bond_mode" => "Bond mode";
    "net.bm.ab" => "Fault tolerance (active-backup)";
    "net.bm.alb" => "Adaptive load balancing (balance-alb)";
    "net.bm.lacp" => "LACP (802.3ad)";
    "net.bm.xor" => "Static link aggregation (balance-xor)";
    "net.bm.tlb" => "Adaptive transmit load balancing (balance-tlb)";
    "net.bm.rr" => "Round robin (balance-rr)";
    "net.bm.bc" => "Broadcast (every port)";
    "net.bq.ab" => "Works with any switch. One port carries the traffic; another takes over when it fails.";
    "net.bq.alb" => "Works with any switch. Spreads the traffic over the ports, also the incoming traffic per client.";
    "net.bq.lacp" => "Needs a managed switch with a LACP group on the same ports. Faster for many clients, not for a single transfer.";
    "net.bq.xor" => "Needs a switch with a static link aggregation group on the same ports.";
    "net.bq.tlb" => "Works with any switch. Balances the outgoing traffic only.";
    "net.bq.legacy" => "Kept as it was configured; not offered for new bonds.";
    "net.lacp_warn" => "Configure the LACP group on the switch first. Without it no partner answers within about 35 seconds and the change is rolled back.";
    "net.f_primary" => "Preferred port";
    "net.primary_none" => "None (the first that works)";
    "net.primary_help" => "Carries the traffic whenever it has link; choose the faster port.";
    "net.f_hash" => "Transmit hash";
    "net.hash_help" => "How connections are spread over the ports. Layer 3+4 spreads better; layer 2+3 suits every switch.";
    "net.bh.l23" => "Layer 2+3 (MAC and IP)";
    "net.bh.l34" => "Layer 3+4 (IP and port)";
    "net.bh.l2" => "Layer 2 (MAC)";
    "net.mixed_speeds" => "The ports run at different speeds. With fault tolerance choose the faster one as preferred port.";
    "net.v_bond_members" => "Choose at least one port.";
    "net.bond_delete" => "Delete bond";
    "net.bond_deleted" => "Deletion pending: apply it from the Network page";
    "net.bond_dropped" => "The new bond is no longer pending";
    "net.bond_removed" => "To be deleted";
    "net.bond_removed_text" => "This bond is pending deletion. When applied, the port with its MAC address takes its address settings and the other ports stay without IPv4.";
    "net.bond_members" => "Members: {list}";
    "net.member_of" => "Member of {master}";
    "net.member_active" => "active";
    "net.member_standby" => "standby";
    "net.member_down" => "no link";
    "net.member_failures" => "{n} link failures";
    "net.lacp_partner" => "LACP partner";
    "net.lacp_none" => "None: the switch does not answer";
    "net.lacp_bad" => "No LACP partner";
    "net.st_new" => "Not created yet";
    "net.v_address" => "Use host addresses with a prefix of 8–32 (192.0.2.10/24).";
    "net.v_addresses_none" => "A static configuration needs at least one address.";
    "net.v_gateway" => "Enter an IPv4 address.";
    "net.v_gateway_subnet" => "The gateway must be inside an address's subnet and differ from it.";
    "net.v_dns" => "Enter IP addresses (IPv4 or IPv6), not names.";
    "net.v_mtu" => "MTU out of range for this port.";
    "net.v_host" => "Enter a host name or address, without spaces.";
    "net.v_count" => "1 to 10.";
    "net.v_size" => "0 to 65000 bytes.";
    "net.revs_intro" => "Every change keeps a copy of the network settings before it. Restoring a revision is tested like any other change.";
    "net.revs_empty" => "No revisions yet";
    "net.revs_empty_text" => "They appear with the first applied change.";
    "net.rk.snapshot" => "Snapshot before a change";
    "net.rk.apply" => "Applied";
    "net.rk.adopt" => "Adopted from ifupdown";
    "net.rk.rescue" => "Rescue settings";
    "net.rk.restore" => "Restored";
    "net.rk.other" => "Other";
    "net.rev_lkg" => "Last known good";
    "net.rev_good" => "Good";
    "net.rev_unconfirmed" => "Not confirmed";
    "net.restore_rev" => "Restore revision #{rev}";
    "net.restore_rev_title" => "Restore revision #{rev}?";
    "net.restore_text" => "The network of {date} comes back with a timed test: confirm it from the restored network or it is rolled back.";
    "net.rescue_intro" => "Ways in when the normal address does not work. They stay on whatever the interface settings are.";
    "net.mdns" => "Local name (mDNS)";
    "net.mdns_text" => "Computers on the same network find the NAS by this name, even when its address changes.";
    "net.avahi_on" => "Avahi running";
    "net.avahi_off" => "Avahi not running";
    "net.mdns_service" => "Announce rynas on the network";
    "net.mdns_service_help" => "Publishes the web interface over Bonjour/mDNS (_rynas._tcp, _http._tcp). Applied with the pending changes.";
    "net.ll_title" => "Link-local addresses";
    "net.ll_text" => "Always on every port and derived from the MAC address: connect a computer straight to the port with a cable and open this address.";
    "net.ll_none" => "No port has a MAC address.";
    "net.ll_active" => "Active";
    "net.ll_inactive" => "Not on the port now";
    "net.tools_intro" => "Run from the NAS itself. 20 runs per minute.";
    "net.d_tool" => "Tool";
    "net.d_ping" => "Ping";
    "net.d_trace" => "Traceroute";
    "net.d_dns" => "DNS lookup";
    "net.d_host" => "Host";
    "net.d_name" => "Name";
    "net.d_record" => "Record type";
    "net.d_count" => "Packets";
    "net.d_size" => "Payload size (bytes)";
    "net.d_iface" => "From interface";
    "net.d_any" => "Any";
    "net.d_df" => "Don't fragment";
    "net.d_df_help" => "With a payload size, tests the path MTU (1472 bytes = MTU 1500).";
    "net.d_server" => "DNS server";
    "net.d_server_help" => "Optional: ask this server instead of the system resolver.";
    "net.d_run" => "Run";
    "net.d_ok" => "Succeeded";
    "net.d_failed" => "Failed";
    "net.d_exit" => "exit code";
    "net.d_output" => "Output";
    "net.rate_limited" => "Too many runs: try again in {n} s.";
    "net.h_hop" => "Hop";
    "net.h_host" => "Host";
    "net.h_loss" => "Loss";
    "net.h_avg" => "Avg ms";
    "net.h_best" => "Best ms";
    "net.h_worst" => "Worst ms";
    // Tailscale (Network › Remote access, ts.*) and its task names
    "ts.title" => "Remote access";
    "ts.subtitle" => "Reach this NAS from anywhere through Tailscale, without opening ports";
    "ts.open_text" => "Tailscale: install it, log the NAS in to your tailnet and choose what it shares.";
    "ts.install_title" => "Tailscale";
    "ts.install_text" => "Tailscale puts this NAS on your private network (tailnet): your devices reach it from anywhere at its own address, with no port forwarding. rynas installs it from Tailscale's official package repository.";
    "ts.install" => "Install Tailscale";
    "ts.block.unsupported_os" => "Tailscale has no package repository for this operating system.";
    "ts.block.unknown_release" => "This system's release has no code name, so the right repository cannot be chosen.";
    "ts.not_running" => "Tailscale is installed but its service is not running.";
    "ts.login_title" => "Log in to your tailnet";
    "ts.login_text" => "Log in with the browser, or paste an auth key made in the admin console. A tagged key keeps the NAS logged in without key expiry.";
    "ts.auth_key" => "Auth key (optional)";
    "ts.auth_key_help" => "It is never shown again or written to the logs.";
    "ts.hostname" => "Device name";
    "ts.hostname_help" => "Empty: the NAS's host name.";
    "ts.login_server" => "Coordination server";
    "ts.login_server_help" => "Only for a self-hosted coordination server; leave it empty otherwise.";
    "ts.login" => "Log in";
    "ts.login_url_text" => "Open this address on any device and approve this NAS:";
    "ts.use_key" => "Use an auth key instead";
    "ts.use_url" => "Back to the login address";
    "ts.open_login" => "Open the login page";
    "ts.login_wait" => "Waiting for the login address…";
    "ts.machine_auth" => "Logged in: an administrator of the tailnet must approve this device in the admin console.";
    "ts.status_title" => "Status";
    "ts.state" => "State";
    "ts.st.running" => "Connected";
    "ts.st.stopped" => "Switched off";
    "ts.st.needs_login" => "Logged out";
    "ts.st.needs_machine_auth" => "Waiting for approval";
    "ts.st.starting" => "Starting";
    "ts.st.no_state" => "Not set up";
    "ts.st.unavailable" => "Service not running";
    "ts.dns_name" => "Name in the tailnet";
    "ts.addresses" => "Tailscale addresses";
    "ts.tailnet" => "Tailnet";
    "ts.peers" => "Other devices";
    "ts.peers_val" => "{online} online of {total}";
    "ts.key_expiry" => "Key expires";
    "ts.key_never" => "Never (expiry disabled for this device)";
    "ts.key_soon" => "The key expires on {date}. Renew it, or disable key expiry for this device in the admin console, or the NAS drops off the tailnet.";
    "ts.version" => "Version";
    "ts.update" => "{v} available with the system updates";
    "ts.offline" => "Not connected to the coordination server.";
    "ts.health" => "Tailscale reports:";
    "ts.options_title" => "Settings";
    "ts.opt.accept_routes" => "Use routes from other devices";
    "ts.opt.accept_routes_help" => "Off by default: a route to this NAS's own network through the tailnet can make the local network unreachable.";
    "ts.opt.accept_dns" => "Use the tailnet's DNS settings";
    "ts.opt.accept_dns_help" => "Off by default: the NAS keeps its local DNS servers. On: tailnet names resolve on the NAS too, and its resolver configuration changes.";
    "ts.opt.exit_node" => "Offer this NAS as an exit node";
    "ts.opt.exit_node_help" => "Devices of the tailnet may send all their internet traffic through this NAS, once approved in the admin console.";
    "ts.opt.routes" => "Share the local network";
    "ts.opt.routes_help" => "Devices of the tailnet reach these subnets through this NAS, once approved in the admin console. Everything in them becomes reachable from the tailnet: limit it with the tailnet's access rules.";
    "ts.opt.no_lan" => "No IPv4 subnet found on the network interfaces.";
    "ts.opt.ssh" => "Tailscale SSH";
    "ts.opt.ssh_help" => "Off by default. On: the tailnet's access rules decide who may log in over SSH, not the accounts of this NAS.";
    "ts.routes_pending" => "Waiting for approval in the admin console: {routes}";
    "ts.confirm_title" => "Change the Tailscale settings?";
    "ts.confirm_text" => "The change is tested: if this page cannot reach the NAS again within 90 seconds, the previous settings come back by themselves.";
    "ts.via_warning" => "This page is open through Tailscale: this can end your session. Keep another way in ready (local network, emergency address).";
    "ts.saved" => "Tailscale settings saved";
    "ts.confirm_failed" => "The NAS could not confirm the change: the previous settings come back by themselves within 90 seconds.";
    "ts.pending_confirm" => "A change of the Tailscale settings waits for confirmation: if it is not confirmed, the previous settings come back within 90 seconds.";
    "ts.save_after_confirm" => "New changes can be saved once the pending one is confirmed or reverted.";
    "ts.ch.routes_added" => "Share: {routes}";
    "ts.ch.routes_removed" => "Stop sharing: {routes}";
    "ts.keep" => "Keep the new settings";
    "ts.logout" => "Log out";
    "ts.logout_title" => "Log this NAS out of the tailnet?";
    "ts.logout_text" => "The NAS leaves the tailnet and its key is expired. Logging in again needs the browser or a new auth key.";
    "ts.uninstall" => "Uninstall";
    "ts.uninstall_title" => "Uninstall Tailscale?";
    "ts.uninstall_text" => "The package, its repository and the forwarding settings rynas wrote are removed.";
    "ts.purge" => "Also forget this device: a new install joins the tailnet as a new device";
    "ts.remove_title" => "Remove Tailscale";
    "ts.wa_on" => "Tailscale is connected ({name}): “This network and VPN” also admits the devices of the tailnet.";
    "ts.wa_off" => "Tailscale is not connected: “This network and VPN” admits no tailnet device.";
    "err.tailscale_not_installed" => "Tailscale is not installed.";
    "task.tailscale.install" => "Install Tailscale";
    "task.tailscale.uninstall" => "Uninstall Tailscale";
    "task.tailscale.login" => "Log in to Tailscale";
    "task.tailscale.logout" => "Log out of Tailscale";
    "task.tailscale.options" => "Change Tailscale settings";
    "task.tailscale.revert" => "Roll back Tailscale settings";
    // ---- System › Updates (upd.*) and its task names
    "task.updates.check" => "Check for updates";
    "task.updates.backports" => "Change the Debian backports setting";
    "task.updates.install" => "Install updates";
    "task.updates.autoremove" => "Remove unused packages";
    "task.updates.changelog" => "Read a package changelog";
    "upd.title" => "Updates";
    "upd.subtitle" => "Debian packages, automatic updates and the boot kernel.";
    "upd.open_text" => "Install Debian updates, set automatic updates and check the kernel.";
    "upd.check" => "Check for updates";
    "upd.never" => "Never";
    "upd.no_apt" => "No package manager";
    "upd.no_apt_text" => "apt is not installed on this machine.";
    "upd.status" => "Status";
    "upd.uptodate" => "Up to date";
    "upd.available" => "{n} updates available";
    "upd.security_n" => "{n} security";
    "upd.reboot_required" => "Restart required.";
    "upd.reboot_pkgs" => "Asked by: {p}.";
    "upd.sim_error" => "apt could not compute the updates.";
    "upd.task_running" => "An update task is running.";
    "upd.removals_note" => "Installing everything would remove: {p}.";
    "upd.held" => "Kept back";
    "upd.lists_updated" => "Last checked";
    "upd.read_at" => "Read on";
    "upd.install_all" => "Install all";
    "upd.install_security" => "Install security updates";
    "upd.install_selected" => "Install selected ({n})";
    "upd.autoremove_n" => "Clean up ({n})";
    "upd.packages" => "Packages";
    "upd.none" => "No updates waiting.";
    "upd.security" => "Security";
    "upd.new_pkg" => "New";
    "upd.col_select" => "Select";
    "upd.select_named" => "Select {name}";
    "upd.col_pkg" => "Package";
    "upd.col_current" => "Installed";
    "upd.col_new" => "New version";
    "upd.col_origin" => "Origin";
    "upd.changelog_of" => "Changelog of {name}";
    "upd.confirm_all" => "Install all updates?";
    "upd.confirm_some" => "Install {n} updates?";
    "upd.confirm_text" => "apt runs without questions and keeps your changes to configuration files.";
    "upd.confirm_text_rm" => "apt runs without questions and keeps your changes to configuration files. Some packages will be removed.";
    "upd.will_remove" => "These packages will be removed:";
    "upd.install" => "Install";
    "upd.autoremove_title" => "Remove unused packages?";
    "upd.autoremove_text" => "apt autoremove removes packages that nothing depends on any more.";
    "upd.autoremove_warn" => "Check the list: it may include the previous kernel or packages the system needs to boot.";
    "upd.remove" => "Remove";
    "upd.auto" => "Automatic updates";
    "upd.bp_title" => "Debian backports";
    "upd.bp" => "Use Debian backports";
    "upd.bp_help" => "Backports are newer packages rebuilt for this Debian release. With them on, nothing is installed or upgraded from them unless asked (apt priority 100). rynas uses them for NUT when a UPS needs a newer one.";
    "upd.bp_system" => "Configured by the system";
    "upd.no_unattended" => "unattended-upgrades is not installed: automatic updates will not run.";
    "upd.mode" => "Install automatically";
    "upd.mode_help" => "Package lists are refreshed daily in every mode, for the notification. Packages are never removed automatically.";
    "upd.mode_off" => "Off";
    "upd.mode_security" => "Security only";
    "upd.mode_all" => "All Debian updates";
    "upd.start" => "Window start";
    "upd.start_help" => "Local time.";
    "upd.window" => "Window length (minutes)";
    "upd.window_help" => "The start is spread randomly within the window.";
    "upd.reboot" => "Restart automatically";
    "upd.reboot_help" => "Only when an installed update requires it.";
    "upd.reboot_time" => "Restart at";
    "upd.reboot_time_help" => "Local time, after the updates.";
    "upd.pending_note" => "Saved changes take effect after you apply them.";
    "upd.kernel" => "Kernel and boot";
    "upd.refresh_kernel" => "Read the kernel and boot state again";
    "upd.running_kernel" => "Running kernel";
    "upd.installed_kernels" => "Installed kernels";
    "upd.kernel_newer" => "Kernel {k} is installed but not running: restart to use it.";
    "upd.kernel_mismatch" => "The boot entry has kernel {k}, not the running one.";
    "upd.boot_outdated" => "The boot entries are outdated.";
    "upd.boot_error" => "The boot state could not be read.";
    "upd.boot_default" => "Default entry";
    "upd.boot_fallback" => "Fallback";
    "upd.boot_entries" => "Boot entries";
    "upd.boot_running" => "Booted from";
    "upd.boot_kernel" => "Kernel on the boot device";
    "upd.boot_kernel_old" => "Previous kernel";
    "upd.trial" => "Kernel trial";
    "upd.trial_none" => "None";
    "upd.trial_armed" => "Armed";
    "upd.trial_trying" => "Trying";
    "upd.boot_free" => "Free space on the boot device";
    // ---- Save in dialogs and editors: stage + apply only that change (config::save_apply)
    "toast.saved_applied" => "Saved and applied";
    "cfg.applying_now" => "Applying the change…";
    "cfg.apply_kept" => "The previous settings are back in place; the change stays in the pending changes.";
    "cfg.apply_log" => "Show the log";
    "cfg.key.ups.safemode" => "UPS shutdown policy";
    "cfg.key.ups.device" => "UPS driver and connection";
    "cfg.key.updates.auto" => "Automatic updates";
    // ---- Disk checks (SettingCheck codes) and SMART health reasons (HealthReason codes)
    "sto.chk_crit" => "Critical";
    "sto.chk.head_parking" => "Head parking";
    "sto.cv.on" => "On";
    "sto.cv.off" => "Off";
    "sto.cv.disabled" => "Disabled";
    "sto.cv.enabled" => "Enabled";
    "sto.cv.unknown" => "Unknown";
    "sto.cv.apm_254_off" => "254 or off";
    "sto.cv.trim_timer_discard" => "fstrim.timer + discard mount option";
    "sto.cv.trim_timer" => "fstrim.timer";
    "sto.cv.trim_discard" => "discard mount option";
    "sto.cv.trim_none" => "None";
    "sto.cv.trim_weekly" => "fstrim.timer every week";
    "sto.cv.apst_enabled" => "Enabled (max latency {n} µs)";
    "sto.cv.temp" => "{n}";
    "sto.cv.temp_below" => "below {n}";
    "sto.cv.ra_default" => "kernel default (128-{n})";
    "sto.cv.parking_rate" => "{n} per hour";
    "sto.cv.parking_max" => "at most {n} per hour";
    "sto.cn.sched_nvme" => "NVMe drives have deep hardware queues: no scheduler is fastest.";
    "sto.cn.sched_hdd" => "mq-deadline keeps throughput on RAID; bfq only helps desktop interactivity.";
    "sto.cn.sched_flash" => "A single queue: mq-deadline and none perform the same.";
    "sto.cn.ra_stacked" => "btrfs and md read ahead on their own device: the disk's value does not matter.";
    "sto.cn.ra_hdd" => "The kernel's default suits spinning disks; very large values only waste cache.";
    "sto.cn.ra_flash" => "Flash needs little read-ahead; more only wastes cache.";
    "sto.cn.wcache" => "Leave it on: the filesystem's flushes keep it safe, off costs a lot of speed.";
    "sto.cn.apm" => "Levels up to 127 let the drive park its heads and spin down by itself.";
    "sto.cn.apm_none" => "The drive has no APM: nothing to set.";
    "sto.cn.parking" => "Frequent head unloads wear the drive: raise APM or the drive's idle timers.";
    "sto.cn.trim_both" => "Async discard does the work, the weekly timer is a cheap safety net.";
    "sto.cn.trim_timer" => "Weekly batched TRIM.";
    "sto.cn.trim_discard" => "btrfs discard=async (the default since Linux 6.2) trims continuously.";
    "sto.cn.trim_none" => "Without TRIM the SSD slows down and wears faster: enable fstrim.timer.";
    "sto.cn.trim_unsupported" => "The device does not accept discards.";
    "sto.cn.trim_parity_raid" => "md RAID5/6 does not pass TRIM through (devices_handle_discard_safely is off).";
    "sto.cn.trim_crypt" => "The encrypted volume does not pass TRIM through: leave 10-20% of a new SSD unpartitioned.";
    "sto.cn.alpm" => "Saves about 1 W per SSD with no measurable cost.";
    "sto.cn.apst" => "Disable it only on models with known APST firmware bugs.";
    "sto.cn.temp_hdd" => "Failures rise with heat: keep hard disks below 50 °C, never above 60 °C.";
    "sto.cn.temp_nvme" => "The drive slows itself down above its warning temperature.";
    "sto.cn.temp_other" => "Keep air flowing over the drive.";
    "sto.reason.smart_failed" => "The drive reports its SMART overall health as FAILED.";
    "sto.reason.attribute_failing" => "{name} is below its failure threshold.";
    "sto.reason.attribute_failed_past" => "{name} was below its failure threshold in the past.";
    "sto.reason.reallocated_sectors" => "{n} reallocated sectors.";
    "sto.reason.pending_sectors" => "{n} sectors waiting for reallocation (unreadable).";
    "sto.reason.offline_uncorrectable" => "{n} offline uncorrectable sectors.";
    "sto.reason.reported_uncorrect" => "{n} errors that ECC could not correct.";
    "sto.reason.command_timeouts" => "{n} command timeouts (often power or cable).";
    "sto.reason.crc_errors" => "{n} interface CRC errors: check the cable or the bay connector.";
    "sto.reason.load_cycles" => "{n} head load cycles (rated 300,000-600,000): aggressive head parking.";
    "sto.reason.old_disk" => "{n} years powered on.";
    "sto.reason.temperature" => "Temperature {n}.";
    "sto.reason.nvme_spare_low" => "Available spare {n} (threshold {limit}).";
    "sto.reason.nvme_temperature" => "Temperature out of range.";
    "sto.reason.nvme_reliability" => "Reliability degraded by media or internal errors.";
    "sto.reason.nvme_read_only" => "The media is read-only.";
    "sto.reason.nvme_backup_failed" => "The volatile memory backup device failed.";
    "sto.reason.media_errors" => "{n} media and data integrity errors.";
    "sto.reason.wear_out" => "Rated endurance used up (100% or more).";
    "sto.reason.wear" => "{n} of the rated endurance left.";
    "sto.reason.self_test_failed" => "Last self-test ({name}): {detail}.";
    "sto.reason.error_log" => "{n} errors in the drive's error log.";
    // SMART counters marked as seen (Storage › Drives › drive)
    "sto.ack" => "Mark as seen";
    "sto.ack_undo" => "Mark as not seen";
    "sto.ack_seen" => "seen {d}";
    "sto.ack_hint" => "The warning comes back if the count grows.";
    "sto.ack_done" => "Warnings marked as seen";
    "sto.ack_undone" => "Warnings count again";
    "sto.ack_grew" => "Some counts grew since this page was loaded: those still warn";
    // removable drives (app bar menu, Hardware › Removable drives)
    "rm.title" => "Removable drives";
    "rm.count.one" => "{n} drive";
    "rm.count.other" => "{n} drives";
    "rm.mounted.one" => "{n} mounted";
    "rm.mounted.other" => "{n} mounted";
    "rm.empty" => "No removable drives";
    "rm.empty_text" => "Plug in a USB drive or an SD card.";
    "rm.mount_all" => "Mount all";
    "rm.remove_all" => "Remove all safely";
    "rm.refresh" => "Read the drives again";
    "rm.settings" => "Removable drive settings";
    "rm.drives_now" => "Plugged in now";
    "rm.remove" => "Remove safely";
    "rm.hint.power_off" => "Unmounts everything, locks encrypted volumes and powers the drive off";
    "rm.hint.eject" => "Unmounts everything, locks encrypted volumes and ejects the medium";
    "rm.hint.release" => "Unmounts everything and locks encrypted volumes: then the card can be pulled out";
    "rm.bus.sd" => "SD card";
    "rm.bus.optical" => "Optical drive";
    "rm.bus.other" => "Removable drive";
    "rm.no_media" => "No medium";
    "rm.read_only" => "read-only";
    "rm.writing" => "Writing…";
    "rm.pending.mount" => "Mounting…";
    "rm.pending.unmount" => "Unmounting…";
    "rm.pending.unlock" => "Unlocking…";
    "rm.pending.lock" => "Locking…";
    "rm.pending.remove" => "Preparing for removal…";
    "rm.untitled" => "Untitled";
    "rm.locked" => "Locked";
    "rm.unlocked" => "Unlocked";
    "rm.encrypted" => "Encrypted: unlock it with its passphrase";
    "rm.not_mounted" => "Not mounted";
    "rm.unsupported" => "No driver for this filesystem is installed";
    "rm.free" => "{free} free of {total}";
    "rm.open" => "Open in Files";
    "rm.copy" => "Copy the mount path";
    "rm.copied" => "Path copied";
    "rm.mount" => "Mount";
    "rm.unmount" => "Unmount";
    "rm.unlock" => "Unlock";
    "rm.lock" => "Lock";
    "rm.shared" => "Shared on the network as {name}";
    "rm.safe" => "{name} can be removed safely";
    "rm.safe_all" => "Every drive can be removed safely";
    "rm.busy_by" => "In use by: {list}. Close the files and windows that use it, then try again.";
    "rm.tip_mounted" => "mounted at {path}";
    "rm.tip_not_mounted" => "not mounted";
    "rm.set.where" => "Volumes are mounted below {root}, in a folder named after their label.";
    "rm.set.auto_mount" => "Mount automatically";
    "rm.set.auto_mount_help" => "Mount the volumes of a drive as soon as it is plugged in, and at startup. Encrypted volumes still wait for their passphrase.";
    "rm.set.read_only" => "Mount read-only";
    "rm.set.read_only_help" => "Nothing on a plugged-in drive can be changed (safe for imports).";
    "rm.set.notify" => "Notify when a drive can be removed";
    "rm.set.notify_help" => "A drive unplugged while it was still mounted is always notified.";
    "rm.set.group" => "Access group";
    "rm.set.group_help" => "Its members may use the network shares and the files of FAT, exFAT and NTFS drives (rynas-admin: administrators; users: every account).";
    "rm.set.share" => "Share mounted drives on the network (SMB)";
    "rm.set.share_help" => "Each mounted volume becomes a private share named usb-<name>, only for the access group. It is removed, and its clients disconnected, before the volume is unmounted.";
    "rm.set.access" => "Network access";
    "rm.set.rw" => "Read and write";
    "rm.set.no_smb" => "Samba is not installed: drives cannot be shared on the network.";
    "task.removable.mount" => "Mount removable volume";
    "task.removable.mount-all" => "Mount removable volumes";
    "task.removable.unmount" => "Unmount removable volume";
    "task.removable.unlock" => "Unlock removable volume";
    "task.removable.lock" => "Lock removable volume";
    "task.removable.remove" => "Remove drive safely";
    "task.removable.remove-all" => "Remove all drives safely";
    "cfg.key.removable" => "Removable drives";
    "nt.src.removable" => "Removable drive monitor";
    "nev.removable.unplugged.title" => "{drive} was unplugged while mounted";
    "nev.removable.unplugged.body" => "Files being written to {mount_points} may be lost or damaged. Remove drives safely before unplugging them.";
    "nev.removable.safe_to_remove.title" => "{drive} can be removed safely";
    "nev.removable.ntfs_read_only.title" => "{volume} mounted read-only";
    "nev.removable.ntfs_read_only.body" => "Windows did not close this NTFS volume cleanly (Fast Startup, hibernation or an unsafe unplug), so it was mounted read-only. Shut Windows down completely or run chkdsk, then plug it in again to write to it.";
    "task.docker.install" => "Install Docker";
    "task.docker.migrate" => "Move Docker to a volume";
    "task.docker.container.start" => "Start container";
    "task.docker.container.stop" => "Stop container";
    "task.docker.container.restart" => "Restart container";
    "task.docker.stack.up" => "Start stack";
    "task.docker.stack.start" => "Start stack containers";
    "task.docker.stack.stop" => "Stop stack";
    "task.docker.stack.restart" => "Restart stack";
    "task.docker.stack.down" => "Take stack down";
    "task.docker.stack.pull" => "Download stack images";
    "task.docker.stack.update" => "Update stack";
    "task.docker.stack.rollback" => "Roll back a stack";
    "task.docker.stack.remove" => "Remove stack";
    "task.docker.update-check" => "Check image updates";
    "task.docker.auto-update" => "Update a stack automatically";
    "nev.docker.unhealthy.title" => "Container {container} is unhealthy";
    "nev.docker.unhealthy.body" => "Its health check fails. Look at its logs on the Docker page.";
    "nev.docker.recovered.title" => "Container {container} is healthy again";
    "nev.docker.oom.title" => "Container {container} ran out of memory";
    "nev.docker.oom.body" => "The kernel stopped a process of the container because it used up its memory.";
    "nev.docker.crash_loop.title" => "Container {container} keeps restarting";
    "nev.docker.crash_loop.body" => "{restarts} restarts in {minutes} minutes. Look at its logs on the Docker page.";
    "nev.docker.crash_loop_over.title" => "Container {container} stopped restarting";
    "nev.docker.disk_full.title" => "Docker storage {used_pct}% full";
    "nev.docker.disk_full.body" => "The filesystem of {path} is almost full: images and containers may stop working. Remove unused images or move Docker to a larger volume.";
    "nev.docker.disk_ok.title" => "Docker storage back under {limit_pct}%";
    "nev.docker.update_available.title" => "Image update available: {image}";
    "nev.docker.update_available.body" => "Used by {stacks}. Update from the Docker page (a snapshot is taken first).";
    "nev.docker.update_available.body_2" => "A newer image is in the registry. Update from the Docker page.";
    "nev.docker.update_done.title" => "Image {image} is up to date";
    "nev.docker.auto_updated.title" => "Stack {stack} updated automatically";
    "nev.docker.auto_updated.body" => "New version: {change}.";
    "nev.docker.auto_updated.body_2" => "The new images run and the stack is healthy.";
    "nev.docker.auto_rolled_back.title" => "Automatic update of {stack} rolled back";
    "nev.docker.auto_rolled_back.body" => "The new version failed: {reason}. The stack runs the previous version again.";
    "nev.docker.auto_held.title" => "Update of {stack} needs you";
    "nev.docker.auto_held.body" => "{reason}. Update it by hand from the Docker page.";
    "nev.docker.auto_failed.title" => "Automatic update of {stack} failed";
    "nev.docker.auto_failed.body" => "{reason}. Look at the task log on the Docker page.";
    // signed online app catalog
    "nev.apps.catalog_updates.title" => "App updates available: {apps}";
    "nev.apps.catalog_updates.body" => "The app catalog was updated. Review and update the apps on the Apps and plugins page (a snapshot is taken first).";
    "nev.apps.catalog_refused.title" => "Online app catalog refused";
    "nev.apps.catalog_refused.body" => "rynas keeps the catalog it has. The downloaded catalog failed verification: {reason}";
    "nev.apps.catalog_ok.title" => "Online app catalog verified again";
    // Docker page, widget and module card (wave D2)
    "nav.docker" => "Docker";
    "w.docker" => "Docker";
    "cfg.key.docker" => "Docker engine and stacks";
    "nt.src.docker" => "Docker monitor";
    "on" => "On";
    "dk.title" => "Docker";
    "dk.subtitle" => "Containers, compose stacks, images and volumes";
    "dk.tab.overview" => "Overview";
    "dk.tab.containers" => "Containers";
    "dk.tab.stacks" => "Stacks";
    "dk.tab.images" => "Images & volumes";
    "dk.module" => "Docker — containers and compose stacks";
    "dk.module_text" => "Runs apps in containers. rynas installs Docker Engine from Docker's own repository (or adopts the one already installed), keeps its data on a volume and gives every compose stack its own folder, with a snapshot before each update.";
    "dk.installed" => "Installed";
    "dk.inst.volume" => "Volume for Docker";
    "dk.inst.volume_help" => "Images, containers and stacks go to <volume>/docker. You can move them later.";
    "dk.inst.system_disk" => "Docker's default folder (choose a volume later)";
    "dk.inst.step_repo" => "Adds Docker's apt repository, pinned to the current major version";
    "dk.inst.step_engine" => "Installs Docker Engine, the compose and buildx plugins";
    "dk.inst.step_user" => "Creates the docker-apps user that owns the app folders";
    "dk.inst.go" => "Install Docker";
    "dk.block.installed" => "Docker is already installed: rynas adopts it.";
    "dk.block.not_debian" => "Docker can be installed from here only on Debian.";
    "dk.block.unsupported_release" => "This Debian release is not supported by Docker's repository.";
    "dk.block.unsupported_arch" => "This processor architecture is not supported by Docker's repository.";
    "dk.task_running" => "Docker task in progress";
    "dk.done" => "{task}: done";
    "dk.open_web" => "Open";
    "dk.engine" => "Engine";
    "dk.eng.running" => "Running";
    "dk.eng.down" => "Not answering";
    "dk.eng.version" => "Version";
    "dk.eng.version_api" => "{v} (API {api}, oldest client API {min})";
    "dk.eng.source" => "Installed from";
    "dk.src.docker_ce" => "Docker's repository (docker-ce)";
    "dk.src.docker_io" => "Debian (docker.io)";
    "dk.src.other" => "Other source";
    "dk.eng.compose" => "Compose plugin";
    "dk.eng.buildx" => "Buildx plugin";
    "dk.eng.storage" => "Storage driver";
    "dk.eng.containerd_store" => "containerd image store";
    "dk.eng.data_root" => "Data folder";
    "dk.eng.cgroup" => "Cgroup driver";
    "dk.eng.logging" => "Log driver";
    "dk.eng.live_restore" => "Containers survive engine restarts";
    "dk.eng.counts" => "Contents";
    "dk.eng.counts_v" => "{n} containers ({run} running, {stop} stopped) · {img} images";
    "dk.warn.daemon_down" => "Docker is installed but its service does not answer. Start docker.service, or look at its journal.";
    "dk.warn.compose_missing" => "The docker compose plugin is missing: stacks cannot be started or checked.";
    "dk.warn.data_root_on_system" => "Docker keeps its data in its default folder, outside the volumes. Choose a volume in the settings below (the system volume too), or move the data.";
    "dk.warn.data_root_almost_full" => "The filesystem of Docker's data is almost full.";
    "dk.warn.not_pinned" => "Docker's packages are not pinned to their major version: a system update could bring a new major release.";
    "dk.warn.engine_too_old" => "This Docker Engine is too old for current apps and managers (API older than 1.44). Update it.";
    "dk.disk" => "Disk usage";
    "dk.disk.fs" => "Filesystem of the data folder";
    "dk.disk.used_of" => "{used} of {total} · {free} free";
    "dk.disk.images" => "Images";
    "dk.disk.reclaimable" => "Images not in use";
    "dk.disk.containers" => "Container layers";
    "dk.disk.volumes" => "Named volumes";
    "dk.disk.build_cache" => "Build cache";
    "dk.managers" => "Docker managers";
    "dk.manager_root" => "A Docker manager has root privileges on this system and its own login: protect it with a strong password and do not expose it to the internet.";
    "dk.col.manager" => "Manager";
    "dk.set.title" => "Engine settings";
    "dk.set.text" => "Saving applies the change right away; a new volume restarts Docker (only while no container runs).";
    "dk.set.volume" => "Volume";
    "dk.set.volume_help" => "Holds the engine data, containerd and the stacks (<volume>/docker).";
    "dk.set.volume_none" => "None (rynas leaves the engine configuration alone)";
    "dk.set.manager" => "Preferred manager";
    "dk.set.manager_help" => "The manager the Apps and plugins page offers first.";
    "apps.proxy_err" => "The web access state could not be read ({err}): apps are offered on the network only.";
    "dk.set.mgr_none" => "None";
    "dk.set.mgr_arcane" => "Arcane";
    "dk.set.mgr_dockhand" => "Dockhand";
    "dk.set.mgr_portainer" => "Portainer CE";
    "dk.set.mgr_dockge" => "Dockge";
    "dk.set.mgr_komodo" => "Komodo";
    "dk.set.pool" => "Container networks";
    "dk.set.pool_help" => "Address range for the networks of the stacks, used only when Docker has none of its own.";
    "dk.set.pool_invalid" => "Enter a network such as 10.200.0.0/16.";
    "dk.set.pool_size" => "Network size (prefix)";
    "dk.set.pool_size_help" => "24 gives each network 254 addresses.";
    "dk.set.snapshot" => "Snapshot before updates";
    "dk.set.snapshot_help" => "A read-only snapshot of the stack folder, for a rollback.";
    "dk.set.stop" => "Stop the stack for the snapshot";
    "dk.set.stop_help" => "Recommended for databases: the files are consistent while nothing writes.";
    "dk.set.online_catalog" => "Download app catalog updates";
    "dk.set.files_app_data" => "Show app data in Files";
    "dk.set.files_app_data_help" => "Administrators see a folder per app and can edit its files; owners stay as the app expects. A wrong change can break an app.";
    "dk.set.online_catalog_help" => "Once a day rynas fetches the signed app catalog, so new apps and app versions arrive without waiting for a rynas update. A catalog that fails verification is never used.";
    "dk.set.keep" => "Snapshots kept per stack";
    "dk.set.full" => "Disk full warning (%)";
    "dk.set.full_help" => "A notification when the data folder's filesystem is this full.";
    "dk.set.restarts" => "Restarts that make a crash loop";
    "dk.set.restarts_help" => "A notification when a container restarts this often within the window below.";
    "dk.set.window" => "Crash loop window (minutes)";
    "dk.plan.title" => "Move Docker to the volume?";
    "dk.plan.text" => "Applying the new volume changes these files and runs these commands.";
    "dk.plan.files" => "Files";
    "dk.plan.create" => "Folders created";
    "dk.plan.commands" => "Commands";
    "dk.plan.restart" => "Docker and containerd restart. Existing images and containers stay in the old folder: use Move data to bring them along.";
    "dk.plan.go" => "Save and apply";
    "dk.mig.title" => "Move data to a volume";
    "dk.mig.ask" => "Move Docker's data?";
    "dk.mig.text" => "Copies images, containers and containerd's data to <volume>/docker and starts Docker there. The old folders are kept until you delete them.";
    "dk.mig.volume" => "Destination volume";
    "dk.mig.running" => "{n} containers are running: stop them first, the move needs Docker stopped.";
    "dk.mig.go" => "Move data";
    "dk.mig.confirm" => "Docker stops during the copy and starts again on {vol}.";
    "dk.vol_free" => "{free} free";
    // the volumes something can be put on, the system volume included (D-ROOTVOL)
    "place.system" => "System";
    "place.system_note" => "The system volume is the folder {path} on the system disk. It shares that disk's space: if it fills up, the system can stop working, and a failure of that disk takes this data with it.";
    "place.not_btrfs" => "The system disk is not btrfs: a plain folder is made, without subvolume, snapshots or NOCOW.";
    "place.unavailable" => "{name} cannot be used now: {why}";
    "place.unavailable_short" => "not available";
    "place.low_space" => "Little free space on {name}: {free} left.";
    "dk.col.name" => "Name";
    "dk.col.image" => "Image";
    "dk.col.images" => "Images";
    "dk.col.ports" => "Ports";
    "dk.col.cpu" => "CPU";
    "dk.col.mem" => "Memory";
    "dk.col.stack" => "Stack";
    "dk.col.running" => "Running";
    "dk.col.created" => "Created";
    "dk.col.size" => "Size";
    "dk.col.volume" => "Volume";
    "dk.c.search" => "Search containers";
    "dk.c.none" => "No containers";
    "dk.c.none_text" => "Containers of every stack and manager show up here.";
    "dk.c.no_match" => "No container matches the filters.";
    "dk.f.label" => "Show";
    "dk.f.all" => "All";
    "dk.f.running" => "Running";
    "dk.f.stopped" => "Stopped";
    "dk.f.unhealthy" => "Unhealthy";
    "dk.st.running" => "Running";
    "dk.st.restarting" => "Restarting";
    "dk.st.paused" => "Paused";
    "dk.st.created" => "Created";
    "dk.st.exited" => "Stopped";
    "dk.st.dead" => "Dead";
    "dk.st.stopping" => "Stopping";
    "dk.st.unknown" => "Unknown";
    "dk.h.healthy" => "Healthy";
    "dk.h.starting" => "Starting";
    "dk.h.unhealthy" => "Unhealthy";
    "dk.a.start" => "Start";
    "dk.a.stop" => "Stop";
    "dk.a.restart" => "Restart";
    "dk.a.logs" => "Logs";
    "dk.a.details" => "Details";
    "dk.a.up" => "Up";
    "dk.a.pull" => "Pull images";
    "dk.a.update" => "Update…";
    "dk.a.down" => "Down";
    "dk.a.edit" => "Edit files";
    "dk.a.remove" => "Remove…";
    "dk.rm.go" => "Remove";
    "dk.log.title" => "Logs";
    "dk.log.follow" => "Follow";
    "dk.log.stamps" => "Timestamps";
    "dk.log.following" => "Live";
    "dk.log.ended" => "The container stopped logging";
    "dk.log.paused" => "Paused";
    "dk.log.lost" => "Connection lost";
    "dk.log.download" => "Download";
    "dk.upd.available" => "Update available";
    "dk.upd.never" => "Images never checked";
    "dk.upd.checked" => "Checked {when}";
    "dk.upd.stack" => "Newer images in the registry: {images}";
    "dk.upd.title" => "Update {name}";
    "dk.upd.ask" => "Update {name}?";
    "dk.upd.text" => "Downloads the newer images and recreates the containers. If the update fails, the stack goes back to the snapshot.";
    "dk.upd.snapshot" => "Take a snapshot first";
    "dk.upd.snapshot_help" => "Needed for the rollback.";
    "dk.upd.stop" => "Stop the stack during the snapshot";
    "dk.upd.next" => "next check {when}";
    "dk.au.title" => "Automatic image updates";
    "dk.au.text" => "Images are checked every day. Stacks that take automatic updates are updated one at a time inside this daily window: snapshot first, health check after, rollback when the new version fails. Turn them on per stack in Stacks, in the stack's details.";
    "dk.au.hour" => "Window start (hour)";
    "dk.au.minute" => "Window start (minute)";
    "dk.au.window" => "Window length (minutes)";
    "dk.au.window_help" => "Updates start only inside the window.";
    "dk.au.chip" => "Automatic";
    "dk.au.stack" => "Automatic updates of this stack";
    "dk.au.enable" => "Update automatically";
    "dk.au.enable_help" => "In the daily window: download, snapshot, start, health check, rollback when it fails. A notification tells you how it went.";
    "dk.au.policy" => "Versions allowed";
    "dk.au.policy_help" => "Which version jumps an automatic update may take. Pre-releases and other image variants are never taken.";
    "dk.au.p_default" => "Default (catalog or tag)";
    "dk.au.p_patch" => "Patch releases (1.4.2 → 1.4.3)";
    "dk.au.p_minor" => "Minor releases (1.4 → 1.5; 0.x: patches)";
    "dk.au.p_calver" => "Same month (2026.11.0 → 2026.11.1)";
    "dk.au.p_notify" => "Never: notify only";
    "dk.au.next" => "Next window: {when}";
    "dk.au.pending" => "The Docker settings have a pending change of the engine volume: apply or discard it first.";
    "dk.au.v_hold" => "Update {what}: notification only ({why}).";
    "dk.au.v_off" => "Update {what}: automatic updates are off for this stack.";
    "dk.au.v_auto" => "Update {what}: installed in the next window.";
    "dk.au.v_label" => "Update {what}: after the download the image version label decides (allowed jumps only).";
    "dk.au.r_major" => "new major version";
    "dk.au.r_minor" => "new minor version";
    "dk.au.r_month" => "new calendar month";
    "dk.au.r_pre" => "pre-release version";
    "dk.au.r_variant" => "another image variant";
    "dk.au.r_downgrade" => "older version";
    "dk.au.r_unknown" => "version not readable";
    "dk.au.r_policy" => "the policy is notify only";
    "dk.au.r_review" => "the new entry needs answers or is a major change";
    "dk.au.r_managed" => "rynas does not manage the files of this stack";
    "dk.au.o_updated" => "Updated automatically {when}.";
    "dk.au.o_rolled_back" => "Automatic update of {when} rolled back:";
    "dk.au.o_held" => "Automatic update of {when} not installed:";
    "dk.au.o_failed" => "Automatic update of {when} failed:";
    "dk.g.rynas" => "Managed by rynas";
    "dk.g.portainer" => "Managed by Portainer";
    "dk.g.untracked" => "Other compose projects";
    "dk.m.rynas" => "rynas";
    "dk.m.portainer" => "Portainer";
    "dk.m.untracked" => "Not managed by rynas";
    "dk.ss.running" => "Running";
    "dk.ss.partial" => "Partly running";
    "dk.ss.stopped" => "Stopped";
    "dk.ss.down" => "Not started";
    "dk.s.search" => "Search stacks";
    "dk.s.create" => "Create stack";
    "dk.s.none" => "No stacks";
    "dk.s.none_text" => "Create a stack from a compose file, or install an app.";
    "dk.s.unhealthy_n" => "{n} unhealthy";
    "dk.s.title" => "Stack {name}";
    "dk.s.folder" => "Folder";
    "dk.s.files" => "Compose files";
    "dk.s.readonly" => "This stack is managed outside rynas: here it can only be started, stopped and restarted.";
    "dk.s.readonly_adopt" => "This stack is managed outside rynas: here it can be started, stopped and restarted, or adopted to be managed fully.";
    "dk.s.no_folder" => "The stack's folder is missing: it can only be started, stopped and restarted.";
    // Docker adopt
    "dk.ad.adopt" => "Adopt";
    "dk.ad.title" => "Adopt {name}";
    "dk.ad.text" => "rynas will manage this stack in its current folder: edit, start, stop, update. Nothing is moved or rewritten.";
    "dk.ad.text_ro" => "Portainer keeps managing these files: rynas shows them, starts and stops the stack and checks its images, but does not edit them.";
    "dk.ad.env" => "Environment files";
    "dk.ad.env_default" => ".env of the folder";
    "dk.ad.dir" => "Folder on this system";
    "dk.ad.dir_help" => "The folder with the compose files, as this system sees it.";
    "dk.ad.profiles" => "Profiles to start";
    "dk.ad.profiles_help" => "Names separated by commas; services with other profiles stay off.";
    "dk.ad.done" => "{name} is now managed by rynas";
    "dk.ad.w_other" => "{manager} can still edit these files: rynas notices changes made outside.";
    "dk.ad.w_portainer" => "The files are in Portainer's data: adopted read-only.";
    "dk.ad.w_name" => "The files say name: {file}; rynas runs the stack as {name}.";
    "dk.ad.w_in_use" => "Containers of another folder already use the name {name}: choose another name.";
    "dk.ad.w_networks" => "External networks must exist before starting: {networks}";
    "dk.ad.w_profiles" => "Some services start only with a profile: {profiles}";
    "dk.ad.w_include" => "The files use include: the stack runs, but its files are not edited here.";
    "dk.ad.w_outside" => "A compose or environment file is outside the folder: the stack runs, but its files are not edited here.";
    "dk.ad.w_missing" => "The compose files were not found on this system: give the folder where they are.";
    "dk.ad.changed" => "The files changed outside rynas since it last read or wrote them.";
    "dk.ad.ro_note" => "Portainer manages these files: here the stack can be started, stopped and restarted, its files viewed and its images checked.";
    "dk.ad.chip" => "Adopted";
    "dk.ad.move" => "Move to stacks folder";
    "dk.ad.move_title" => "Move {name}?";
    "dk.ad.move_text" => "The stack stops, its whole folder ({from}) is copied with its data to {to}, the bind mounts are checked, and it starts again. The old folder is kept with a .moved name until you delete it.";
    "dk.ad.move_free" => "Free space on the target";
    "dk.ad.move_copy" => "Copy";
    "dk.ad.move_reflink" => "Clone on the same btrfs volume (reflink): quick, no extra space";
    "dk.ad.move_full" => "Full copy of every file: needs as much free space as the folder";
    "dk.ad.move_down" => "Stack stopped";
    "dk.ad.move_down_short" => "Seconds: the copy only clones the files";
    "dk.ad.move_down_long" => "For the whole copy: minutes or more with large app data";
    "dk.ad.release" => "Release";
    "dk.ad.release_title" => "Release {name}?";
    "dk.ad.release_text" => "rynas stops managing this stack. Its files and containers stay as they are.";
    "dk.ad.released" => "Stack released";
    "dk.ad.ed_ro" => "These files are not edited here (read-only, outside the folder or with include:).";
    "dk.ad.ed_others" => "The stack also uses: {files}";
    "dk.im.button" => "Import folder";
    "dk.im.title" => "Import a folder";
    "dk.im.text" => "Register a folder with compose files (for example restored from a backup) as a stack, where it is.";
    "dk.im.dir_help" => "Absolute path, such as /volume1/docker/app.";
    "dk.im.name_help" => "The compose project name; the folder keeps its name.";
    "dk.im.check" => "Check";
    "dk.im.import" => "Import";
    "dk.s.containers" => "Containers";
    "dk.s.no_containers" => "No containers: start the stack with Up.";
    "dk.snap.title" => "Snapshots";
    "dk.snap.take" => "Take a snapshot";
    "dk.snap.taken" => "Snapshot taken";
    "dk.snap.none" => "No snapshots yet: one is taken before every update.";
    "dk.snap.not_subvolume" => "The stack folder is not a btrfs subvolume: no snapshots and no rollback.";
    "dk.snap.r_manual" => "Taken by hand";
    "dk.snap.r_update" => "Before an update";
    "dk.snap.r_pull" => "Before a pull";
    "dk.snap.r_rollback" => "Before a rollback";
    "dk.snap.rollback_title" => "Roll back to the snapshot of {when}?";
    "dk.snap.rollback_text" => "The stack is taken down, its folder goes back to the snapshot of {when} with the images of that time, then it starts again. The current state is kept as a snapshot.";
    "dk.snap.delete" => "Delete the snapshot of {when}?";
    "dk.snap.delete_text" => "The snapshot of {when} is deleted: a rollback to it is no longer possible.";
    "dk.rm.title" => "Remove {name}?";
    "dk.rm.text" => "Stops the stack and removes its containers. Named volumes are kept.";
    "dk.rm.files" => "Also delete the stack folder (compose file, .env, app data) and its snapshots";
    "dk.rm.adopted" => "The stack folder, its files and app data stay where they are. To stop managing the stack without removing its containers, use Release.";
    "dk.ed.name" => "Stack name";
    "dk.ed.name_help" => "Lowercase letters, digits, - and _: also the folder name.";
    "dk.ed.name_invalid" => "Use 1-63 lowercase letters, digits, - or _, starting with a letter or digit.";
    "dk.ed.volume" => "Volume";
    "dk.ed.compose" => "compose.yaml";
    "dk.ed.env" => ".env";
    "dk.ed.env_help" => "Variables for the compose file (NAME=value per line). Kept readable by root only.";
    "dk.ed.start" => "Start the stack after creating it";
    "dk.ad.start_import" => "Start the stack after importing it";
    "dk.ed.validate" => "Check";
    "dk.ed.created" => "Stack created";
    "dk.ed.saved" => "Files saved: use Up to apply them";
    "dk.val.bad" => "docker compose refuses the file.";
    "dk.val.ok" => "The file is valid. Services: {services}. Published ports: {ports}.";
    "dk.val.ports" => "Ports already used by other containers: {ports}";
    "dk.val.outside" => "{path} ({service}) is outside the volume: it is not in the stack's snapshots and the folder is not created for you.";
    "dk.val.create" => "Folders created before the start, owned by docker-apps: {paths}";
    "dk.i.title" => "Images";
    "dk.i.total" => "{n} · {size}";
    "dk.i.none" => "No images";
    "dk.i.untagged" => "No tag";
    "dk.i.used_by" => "Used by {n}";
    "dk.i.unused" => "Not used";
    "dk.i.dangling" => "Dangling";
    "dk.v.title" => "Volumes";
    "dk.v.none" => "No named volumes";
    "dk.w.open" => "Open Docker";
    "dk.w.running" => "Containers running";
    "dk.w.cpu" => "CPU of all containers";
    "dk.w.top" => "Busiest containers";
    "dk.w.none" => "No container is running.";
    "dk.w.stopped" => "Docker engine stopped";
    "dk.w.stopped_text" => "Docker is installed but its engine does not answer. Open Docker to see why.";
    // ---- Apps & Plugins (D-APPCAT, wave A2)
    "yes" => "Yes";
    "no" => "No";
    "task.apps.install" => "Install app";
    "task.apps.update" => "Update app";
    "task.apps.rollback" => "Roll back an app";
    "task.apps.uninstall" => "Uninstall app";
    "task.apps.proxy" => "Move app behind the proxy";
    "task.apps.auto-update" => "Update an app automatically";
    "task.apps.catalog-update" => "Check the app catalog";
    "apps.subtitle" => "Curated apps in containers and the modules that add features to rynas";
    "apps.docker_first" => "Apps run in Docker containers: install Docker first, then any app of the catalog installs in a few steps.";
    "apps.plg.docker_above" => "The Docker module is at the top of this page: install it before any app.";
    "apps.tab.featured" =>"Featured";
    "apps.tab.apps" => "Apps";
    "apps.tab.plugins" => "Plugins";
    "apps.tab.installed" => "Installed";
    "apps.tab.updates" => "Updates";
    "apps.updates_waiting" => "Installed apps with an update: {n}.";
    "apps.see_updates" => "See the updates";
    "apps.feat.fit" => "Recommended apps";
    "apps.feat.ram" => "Chosen for {ram} of RAM and the {arch} architecture.";
    "apps.feat.arch" => "Chosen for the {arch} architecture.";
    "apps.feat.none" => "Every app that fits is already installed.";
    "apps.feat.managers" => "Docker managers";
    "apps.feat.managers_text" => "A full container manager next to rynas, on the same stacks folder. It gets the Docker socket, so it has root privileges and its own login.";
    "apps.browse_all" => "Browse all apps";
    "apps.empty" => "The catalog is empty";
    "apps.empty_text" => "No app could be loaded. Reinstall rynas or check the entries listed below.";
    "apps.issues" => "Entries not loaded: {n}";
    "apps.count" => "Shown: {n}";
    "apps.no_match" => "No app matches";
    "apps.no_match_text" => "Try another word or category.";
    "apps.search" => "Search apps";
    "apps.category" => "Category";
    "apps.cat.all" => "All categories";
    "apps.cat.management" => "Management";
    "apps.cat.media" => "Media";
    "apps.cat.photos" => "Photos";
    "apps.cat.files" => "Files";
    "apps.cat.documents" => "Documents";
    "apps.cat.productivity" => "Productivity";
    "apps.cat.security" => "Security";
    "apps.cat.home_automation" => "Home automation";
    "apps.cat.surveillance" => "Video surveillance";
    "apps.cat.monitoring" => "Monitoring";
    "apps.cat.network" => "Network";
    "apps.cat.sync" => "Sync";
    "apps.cat.development" => "Development";
    "apps.cat.backup" => "Backup";
    "apps.cat.ai" => "AI";
    "apps.af.non_free" => "Not free software";
    "apps.af.non_free_text" => "The app or a part of it is not free software: its license limits use, changes or sharing.";
    "apps.af.high_resources" => "Heavy";
    "apps.af.high_resources_text" => "Needs more than 2 GB of RAM to run well.";
    "apps.af.needs_gpu" => "Needs a GPU";
    "apps.af.needs_gpu_text" => "Needs a graphics processor or another accelerator.";
    "apps.af.telemetry" => "Telemetry";
    "apps.af.telemetry_text" => "Sends usage data to its developers by default.";
    "apps.af.unmaintained" => "Unmaintained";
    "apps.af.unmaintained_text" => "No new release upstream for a long time: security fixes may not come.";
    "apps.af.requires_https" => "Needs HTTPS";
    "apps.af.requires_https_text" => "Its clients refuse plain HTTP: publish it through web access or another TLS proxy.";
    "apps.af.root_access" => "Root access";
    "apps.af.root_access_text" => "Gets root-level access to the NAS (the Docker socket, privileged mode or host devices).";
    "apps.ram.ok" => "Enough RAM";
    "apps.ram.warn" => "Low RAM: {have} of {need} advised";
    "apps.ram.blocked" => "Not enough RAM: {have} of {need}";
    "apps.ram.unknown" => "RAM not checked";
    "apps.ram.min" => "Minimum RAM";
    "apps.ram.recommended" => "Recommended RAM";
    "apps.block.docker" => "Install Docker first";
    "apps.block.volume" => "Docker has no volume yet: choose one in the Docker settings";
    "apps.block.arch" => "Not available for this processor";
    "apps.block.rynas" => "Needs a newer rynas";
    "apps.block.ram" => "Not enough RAM to install";
    "apps.block.reverse_proxy" => "This proxy needs ports 80 and 443: switch web access to external proxy mode first.";
    "apps.block.to_mode" => "Change the mode";
    "apps.installed" => "Installed";
    "apps.version" => "Version {v}";
    "apps.version_pkg" => "Version {v} · package {pkg}";
    "apps.license" => "License";
    "apps.details" => "Details";
    "apps.install" => "Install";
    "apps.manage" => "Manage";
    "apps.update" => "Update";
    "apps.homepage" => "Website";
    "apps.source" => "Source code";
    "apps.changelog" => "Release notes";
    "apps.screenshot" => "Screenshot";
    "apps.maintainers" => "Catalog maintainers";
    "apps.d.manager" => "Docker manager";
    "apps.d.antifeatures" => "Things to know";
    "apps.d.resources" => "Resources";
    "apps.d.ports" => "Ports";
    "apps.d.folders" => "Data folders";
    "apps.d.folders_text" => "Created in the app's stack folder, so its snapshots cover them; single folders can go on another volume.";
    "apps.d.secrets" => "Generated secrets";
    "apps.d.volumes" => "Disposable volumes (not in snapshots)";
    "apps.secret.shown" => "shown once after the install";
    "apps.gpu" => "Graphics";
    "apps.gpu.optional" => "Optional, for acceleration";
    "apps.gpu.required" => "Required";
    "apps.cpu" => "Processor features";
    "apps.network" => "Network";
    "apps.host_network" => "Host network";
    "apps.col.port" => "Port";
    "apps.col.purpose" => "Purpose";
    "apps.col.reach" => "Reachable";
    "apps.col.folder" => "Folder";
    "apps.col.kind" => "Kind";
    "apps.col.access" => "Access";
    "apps.col.app" => "App";
    "apps.col.version" => "Version";
    "apps.col.service" => "Service";
    "apps.col.change" => "Change";
    "apps.col.images" => "Image";
    "apps.port.proxied" => "Web, can go behind the proxy";
    "apps.port.fixed" => "Fixed";
    "apps.intent.config" => "Settings";
    "apps.intent.data" => "Data";
    "apps.intent.db" => "Database";
    "apps.intent.media" => "Media files";
    "apps.intent.cache" => "Cache";
    "apps.ssd.ok" => "SSD helps";
    "apps.ssd.preferred" => "Best on SSD";
    "apps.share.required" => "Required";
    "apps.web.port" => "Port and path";
    "apps.web.auth" => "Login";
    "apps.web.auth_app" => "The app has its own login";
    "apps.web.auth_none" => "No login of its own: keep it on the LAN";
    "apps.web.auth_proxy" => "Trusts the login of the proxy";
    "apps.web.public" => "Internet exposure";
    "apps.web.public_ok" => "Suitable";
    "apps.web.public_warn" => "Possible, with care";
    "apps.web.public_never" => "Never";
    "apps.web.body" => "Largest upload";
    "apps.web.timeout" => "Response time limit";
    "apps.web.websocket" => "Live updates (WebSocket)";
    "apps.web.subpath" => "Works under a path";
    "apps.web.streaming" => "Streaming and large uploads";
    "apps.w.title" => "Install {name}";
    "apps.w.s_options" => "Options";
    "apps.w.s_folders" => "Folders and shares";
    "apps.w.s_network" => "Network";
    "apps.w.s_access" => "Access";
    "apps.w.stack_help" => "The folder and the compose project of the app; it cannot change later.";
    "apps.w.stack_dir" => "The app goes in {path}.";
    "apps.w.no_folders" => "This app needs no folders or shared folders.";
    "apps.w.folders_text" => "By default every folder is in the stack folder. Pick a volume to place one elsewhere, for example a database on an SSD.";
    "apps.w.in_stack" => "In the stack folder: {path}";
    "apps.w.on_volume" => "On {vol}: {path}";
    "apps.w.on_volume_suggested" => "On {vol} (suggested): {path}";
    "apps.w.ssd_hint" => "A database answers much faster on an SSD volume.";
    "apps.w.shares_text" => "Folders the app reads or writes, with the access shown. Nothing is moved or copied.";
    "apps.w.no_share" => "Not mapped";
    "apps.w.no_share_fallback" => "Not mapped (uses {path} in the stack folder)";
    "apps.w.no_shares_yet" => "There are no shared folders yet.";
    "apps.w.to_shares" => "Create one";
    "apps.w.host_network" => "The app uses the host network: its ports are fixed and only listed here.";
    "apps.w.no_ports" => "The app publishes no ports.";
    "apps.w.port_used" => "Port {port} is in use";
    "apps.w.port_fixed_used" => "Port {port} is in use and this app cannot move it.";
    "apps.w.use_port" => "Use {port}";
    "apps.w.check_again" => "Check again";
    "apps.w.errors" => "Fix these answers first:";
    "apps.w.folder" => "Folder";
    "apps.w.version" => "Catalog version";
    "apps.w.address" => "Address";
    "apps.w.create" => "Folders to create";
    "apps.w.existing" => "Folders already there";
    "apps.w.shares_used" => "Shared folders used";
    "apps.grant.read" => "read";
    "apps.grant.rw" => "read and write";
    "apps.grant.ok" => "The share already lets apps in ({access}); files and folders without it get it";
    "apps.grant.add" => "The apps' account (docker-apps) is added to the share's permissions with {access} access, also on the files already there";
    "apps.grant.raise" => "The apps' access to this share goes from read to read and write, also on the files already there";
    "apps.grant.files" => "Its permissions are not set by rynas: the apps' account (docker-apps) gets {access} access on its files and folders";
    "apps.grant.denied_short" => "The share denies apps: nothing is changed";
    "apps.grant.denied" => "{share} denies the apps' account (docker-apps): the app cannot use this folder. Change the share's permissions, or choose another folder.";
    "apps.w.compose" => "compose.yaml that will be written";
    "apps.w.env" => ".env that will be written (secrets hidden)";
    "apps.w.installing" => "Installing as stack {stack} in {path}.";
    "apps.w.installed" => "The app is installed and starting.";
    "apps.w.failed" => "The install did not finish: the log above says why and how to go on.";
    "apps.w.secrets" => "Initial credentials";
    "apps.w.save_now" => "Save it now, for example in a password manager: it is shown only this once.";
    "apps.w.copy" => "Copy";
    "apps.w.close_secrets" => "The credentials will not be shown again.";
    "apps.w.close_anyway" => "Close anyway";
    "apps.warn.docker_not_installed" => "Docker is not installed: this is a preview, the install is refused.";
    "apps.warn.no_volume" => "Docker has no volume yet: the install is refused.";
    "apps.warn.engine_unreachable" => "Docker did not answer: container ports were not checked.";
    "apps.warn.ram_below_recommended" => "This NAS has less RAM than the app recommends: it may be slow.";
    "apps.warn.published_on_lan" => "The web port opens on the LAN, without the web access proxy in front.";
    "apps.warn.proxy_inactive" => "The web port listens only on the NAS itself while web access is not active: the app is unreachable until web access runs.";
    "apps.fe.invalid_value" => "Not a valid value";
    "apps.fe.missing_value" => "Required";
    "apps.fe.unknown_value" => "Not a question of this app";
    "apps.fe.unsafe_value" => "Contains a line break, a single quote or a NUL character";
    "apps.fe.share_missing" => "The shared folder does not exist";
    "apps.fe.bad_path" => "Not a folder on a mounted volume";
    "apps.fe.bad_port" => "This port cannot be used";
    "apps.fe.bad_stack" => "Not a valid stack name, or the stack folder exists";
    "apps.bind.lan" => "LAN";
    "apps.bind.proxy" => "Web access only";
    "apps.inst.none" => "No app installed";
    "apps.inst.none_text" => "Apps installed from the catalog show up here, with their state and updates.";
    "apps.inst.no_docker" => "Docker is not installed";
    "apps.inst.engine_down" => "Docker not answering";
    "apps.inst.left" => "No longer in the catalog";
    "apps.inst.left_text" => "This app left the catalog: it keeps running, but gets no updates from it. Converting it to a custom stack makes that explicit.";
    "apps.inst.on_lan" => "The app is published on the LAN directly. Move it behind the proxy to reach it only through web access, with HTTPS.";
    "apps.inst.catalog" => "Catalog";
    "apps.inst.title" => "App {name}";
    "apps.inst.no_snaps" => "No snapshot yet: the first update takes one.";
    "apps.in_docker" => "Show in Docker";
    "apps.proxy" => "Move behind the proxy";
    "apps.proxy_title" => "Move the app behind the proxy?";
    "apps.proxy_text" => "The app's web port will listen only on the NAS itself, and web access gives the app its own HTTPS address. The stack restarts.";
    "apps.w.access_how" => "How the app is reached";
    "apps.w.access_proxy" => "Through web access (recommended)";
    "apps.w.access_proxy_help" => "HTTPS at the app's own address, only for the devices web access allows. Its port stays closed on the LAN.";
    "apps.w.access_lan" => "Directly on the LAN port";
    "apps.w.access_lan_help" => "Port {port} opens on every address of the NAS, without HTTPS and without the access rules.";
    "apps.w.access_url_pending" => "The address is shown once the plan is ready.";
    "apps.w.access_level" => "Who may connect (this network by default) can be changed later in";
    "apps.w.access_inactive" => "Web access is not active, so the app opens directly on its LAN port: {url}. With web access on, apps get their own HTTPS address.";
    "apps.w.access_enable" => "Turn on web access";
    "apps.proxy_url" => "New address: {url}";
    "apps.proxy_more" => "Who may connect and the checks of the address are on the page";
    "wa.title" => "Web access";
    "wa.subtitle" => "HTTPS addresses for the interface and the apps, and who may reach them";
    "wa.open_text" => "Addresses of the interface and the apps, access levels, certificate";
    "wa.tab.overview" => "Overview";
    "wa.tab.routes" => "App addresses";
    "wa.eng.title" => "Proxy engine";
    "wa.eng.state" => "State";
    "wa.eng.not_installed" => "Not installed";
    "wa.eng.running" => "Running";
    "wa.eng.out_of_sync" => "Running an older configuration";
    "wa.eng.in_sync" => "Up to date";
    "wa.eng.sync" => "Configuration";
    "wa.eng.last_load" => "Last loaded";
    "wa.eng.never" => "Never";
    "wa.eng.sync_now" => "Check and load again";
    "wa.eng.last_error" => "The last load failed:";
    "wa.eng.missing_text" => "The rynas-caddy package is not installed. The settings below are kept and used as soon as it is.";
    "wa.ui.title" => "This interface";
    "wa.ui.urls" => "Addresses";
    "wa.ui.external" => "Only the emergency address (external proxy mode)";
    "wa.ui.access" => "Who may open the interface";
    "wa.ui.ports" => "Ports";
    "wa.ui.ports_val" => "HTTPS {https}, HTTP {http} (redirects to HTTPS)";
    "wa.ui.lan" => "Network ranges";
    "wa.ui.vpn" => "VPN ranges";
    "wa.ui.emergency" => "Emergency address, always served by rynas itself and never through the proxy. Keep it as a bookmark:";
    "wa.set.title" => "Settings";
    "wa.mode" => "Mode";
    "wa.mode.builtin" => "Built-in proxy";
    "wa.mode.external" => "External proxy";
    "wa.mode.builtin_help" => "rynas runs the proxy on ports 80 and 443: the interface and the apps get HTTPS addresses with access rules.";
    "wa.mode.external_help" => "Ports 80 and 443 stay free for a proxy you install yourself as an app. The built-in proxy stops, apps keep only their LAN ports and the interface is reached at the emergency address.";
    "wa.set.local_names" => "Local names (.local)";
    "wa.set.local_names_help" => "Each app gets a name like app-nas.local, announced on the network with mDNS.";
    "wa.set.h3" => "HTTP/3";
    "wa.set.h3_help" => "Faster connections over UDP port 443 for browsers that support it.";
    "wa.access.lan" => "This network";
    "wa.access.lan_vpn" => "This network and VPN";
    "wa.access.public" => "Everyone on the internet";
    "wa.access.lan_help" => "Devices on the networks the NAS is connected to.";
    "wa.access.lan_vpn_help" => "Also devices connected through Tailscale or WireGuard.";
    "wa.access.public_help" => "Arrives later, with its own checklist and confirmation.";
    "wa.soon" => "Coming later";
    "wa.change.title" => "Change how the interface is reached?";
    "wa.change.timed" => "After saving, open the interface at its new address and confirm there within 2 minutes. Without a confirmation the previous settings come back by themselves. The emergency address {url} keeps working.";
    "wa.change.external" => "The built-in proxy stops: the interface's .local addresses and the apps' HTTPS addresses stop working. Use the emergency address {url} from now on.";
    "wa.confirm.title" => "Confirm the new address";
    "wa.confirm.text" => "The interface's own address changed. Open it and confirm there within {s} s.";
    "wa.confirm.text_here" => "This page is open at the new address: confirm it within {s} s.";
    "wa.confirm.expired" => "Time is up: the previous settings are being restored…";
    "wa.confirm.rollback" => "Without a confirmation the previous settings come back by themselves when the time runs out. Port {port} (the emergency address) is never affected.";
    "wa.confirm.open" => "Open {name}";
    "wa.confirm.go" => "Confirm";
    "wa.confirm.done" => "New address confirmed";
    "wa.confirm.names_help" => "If the .local name does not open on this device (no mDNS), use the other one: it needs the NAS name in your router's DNS.";
    "wa.confirm.rolled_back" => "Not confirmed in time: the previous settings are back.";
    "wa.rolled.title" => "Change undone";
    "wa.col.app" => "App";
    "wa.col.address" => "Address";
    "wa.col.access" => "Who may connect";
    "wa.kind.app" => "App";
    "wa.kind.stack" => "Stack";
    "wa.kind.custom" => "Other device or service";
    "wa.route.on" => "Published";
    "wa.route.off" => "Turned off";
    "wa.route.test" => "Test";
    "wa.route.test_named" => "Test {name}";
    "wa.route.title" => "Address of {name}";
    "wa.route.gone" => "This address no longer exists.";
    "wa.route.names" => "Names";
    "wa.route.upstream" => "Forwarded to";
    "wa.route.kind" => "Kind";
    "wa.route.login" => "Sign-in";
    "wa.route.headers" => "Security headers";
    "wa.route.enabled" => "Address on";
    "wa.route.enabled_help" => "Off: the proxy closes connections to this name; the app keeps running.";
    "wa.route.auth_none" => "This app has no login of its own, so it stays limited to this network.";
    // Web access routes on ports / custom
    "wa.add.button" => "Add address";
    "wa.add.title" => "New address";
    "wa.add.intro" => "Forward a name, a port or both to another device on this network or to a service rynas did not install.";
    "wa.sec.where" => "Where it answers";
    "wa.sec.forward" => "Forwarding";
    "wa.f.label" => "Name";
    "wa.f.label_help" => "Letters, digits and “-”. It starts the address, e.g. router-nas.local.";
    "wa.f.label_bad" => "Use 1-40 characters a-z, 0-9 and “-”, not at the start or end.";
    "wa.f.label_required" => "Enter a name for the address.";
    "wa.f.upstream" => "Forward to";
    "wa.f.upstream_help" => "http:// or https://, then the device's address or name, and its port when it is not the standard one (80, 443).";
    "wa.f.upstream_bad" => "Write it as http://address:port or https://address:port (the port may be left out for 80 and 443), without a path.";
    "wa.f.upstream_required" => "Enter the address of the device or service.";
    "wa.f.skip_verify" => "Accept the device's own certificate";
    "wa.f.skip_verify_help" => "For devices with a self-signed certificate. The connection stays encrypted, but the device is not verified.";
    "wa.f.skip_warn" => "Turn this on only for a device you know: whoever takes its address on the network could read the traffic.";
    "wa.f.upstream_host" => "Send the device's own host name";
    "wa.f.upstream_host_help" => "For devices that answer only to their own name or address. Off: the device sees the name the browser used.";
    "wa.f.prefix" => "Path prefix (optional)";
    "wa.f.prefix_help" => "Only requests below this path are forwarded, unchanged, e.g. /grafana. Several services can then share one port.";
    "wa.f.websocket" => "WebSockets and streaming pass through without extra settings.";
    "wa.f.ports" => "Extra ports (optional)";
    "wa.f.ports_help" => "HTTPS ports this address also listens on, separated by commas, e.g. 8444. Useful when your router forwards a specific port.";
    "wa.f.ports_bad" => "Use port numbers from 1024 to 65535, separated by commas.";
    "wa.f.main_port" => "Also on the standard HTTPS port";
    "wa.f.main_port_help" => "Off: the address answers only on its extra ports.";
    "wa.f.nas" => "Also on this NAS's names";
    "wa.f.nas_help" => "On the extra ports the address also answers at the NAS's own names, e.g. https://nas.example.com:8444/.";
    "wa.route.remove" => "Remove address";
    "wa.route.remove_q" => "Remove this address?";
    "wa.route.remove_text" => "The NAS stops answering on this address. The device or service itself is not touched.";
    "wa.check.port_listening" => "Extra port listening";
    "wa.fix.port_listening" => "The proxy does not answer on this port. Check that no other program uses it, then save again.";
    "wa.auth.app" => "The app's own login";
    "wa.auth.none" => "None";
    "wa.headers.default" => "Standard (no framing by other sites)";
    "wa.headers.relaxed" => "Relaxed (framing allowed)";
    "wa.routes.empty" => "No app addresses yet";
    "wa.routes.empty_text" => "Apps installed through web access get an address here; installed apps can move behind the proxy from their page.";
    "wa.routes.external" => "External proxy mode: these addresses are not served until the built-in proxy is back on.";
    "wa.mdns" => "A .local name also needs mDNS on the device that opens it. Windows 10 and later, macOS, iOS, Android 12 and later and most Linux desktops have it; elsewhere use the emergency address or a DNS entry on your router.";
    "wa.prob.name_too_long" => "The name is longer than 63 characters: shorten the app's stack name.";
    "wa.prob.local_names_off" => "Local names are off: this app has no address.";
    "wa.prob.disabled" => "The address is turned off.";
    "wa.prob.name_collision" => "Another device on the network already uses this name.";
    "wa.prob.name_unpublished" => "The name could not be announced: Avahi is not running or refused it.";
    "wa.test.title" => "Checks";
    "wa.test.text" => "Runs every step from the name to the app, on the NAS, and says what to fix.";
    "wa.test.at" => "Checked {when}";
    "wa.cs.ok" => "OK";
    "wa.cs.warn" => "Warning";
    "wa.cs.fail" => "Failed";
    "wa.cs.skipped" => "Skipped";
    "wa.check.local_name" => "Name announced on the network";
    "wa.check.dns" => "Name resolves on the NAS";
    "wa.check.proxy_running" => "Proxy answering";
    "wa.check.route_loaded" => "Address loaded in the proxy";
    "wa.check.https_listening" => "HTTPS port open";
    "wa.check.certificate" => "Certificate covers the name";
    "wa.check.upstream" => "App answering";
    "wa.fix.local_name" => "Another device may use this name, or Avahi is not running. Rename the device or the app's stack, then use Check and load again.";
    "wa.fix.dns" => "The NAS cannot resolve the name. Check that Avahi runs and that local names are on.";
    "wa.fix.proxy_running" => "The proxy does not answer. Use Check and load again; if it stays down, look at the log of the rynas-caddy service.";
    "wa.fix.route_loaded" => "The running proxy does not have this address yet. Use Check and load again.";
    "wa.fix.https_listening" => "Nothing answers on the HTTPS port: another program may hold port 443, or the proxy is stopped.";
    "wa.fix.certificate" => "The certificate does not cover this name or has expired. Use Check and load again to issue a new one.";
    "wa.fix.upstream" => "The app does not answer on its port. Check on the Docker page that its containers run and finished starting.";
    "tls.title" => "Certificate";
    "tls.open_text" => "The HTTPS certificate of the interface and how devices trust this NAS";
    "tls.active" => "Active certificate";
    "tls.source.local" => "Local certificate";
    "tls.source.imported" => "Imported certificate";
    "tls.https_port" => "Also used by the emergency port {port}";
    "tls.subject" => "Subject";
    "tls.issuer" => "Issued by";
    "tls.names" => "Names";
    "tls.addresses" => "Addresses";
    "tls.valid_from" => "Valid from";
    "tls.valid_until" => "Valid until";
    "tls.days_left" => "{n} days left";
    "tls.fingerprint" => "SHA-256 fingerprint";
    "tls.key_type" => "Key";
    "tls.chain" => "Intermediate certificates";
    "tls.local_ca" => "Trust this NAS";
    "tls.no_ca" => "There is no local certificate authority yet.";
    "tls.download_ca" => "Download the local CA";
    "tls.trust_help" => "Browsers warn about this NAS until a device trusts its local certificate authority. Install it once per device: it covers the interface and every app address, also after a renewal.";
    "tls.trust_check" => "Before installing, compare the fingerprint above with the one your device shows.";
    "tls.os.windows" => "Windows";
    "tls.os.macos" => "macOS";
    "tls.os.ios" => "iPhone and iPad";
    "tls.os.android" => "Android";
    "tls.os.linux" => "Linux";
    "tls.os.firefox" => "Firefox";
    "tls.trust.windows" => "Open the downloaded file and choose Install Certificate | Store location: Local Machine | Place all certificates in the following store: Trusted Root Certification Authorities | Finish, then restart the browser";
    "tls.trust.macos" => "Open the downloaded file: Keychain Access adds it | Double-click the certificate (rynas local CA) | Trust: When using this certificate, Always Trust | Close the window and confirm with your password";
    "tls.trust.ios" => "Download the file in Safari and allow the profile | Settings, General, VPN and Device Management: install the downloaded profile | Settings, General, About, Certificate Trust Settings: turn on full trust for rynas local CA";
    "tls.trust.android" => "Settings, Security and privacy, More security settings, Encryption and credentials (names vary by maker) | Install a certificate, CA certificate, Install anyway | Pick the downloaded file. Browsers trust it; some apps use only their own list";
    "tls.trust.linux" => "Debian, Ubuntu: sudo cp rynas-local-ca.crt /usr/local/share/ca-certificates/ && sudo update-ca-certificates | Fedora, Arch: sudo trust anchor rynas-local-ca.crt | Chrome on Linux (version 134 and later) keeps its own list: open chrome://certificate-manager, Local certificates, Custom, Installed by you, then Import next to Trusted certificates";
    "tls.trust.firefox" => "Settings, Privacy and Security, Certificates, View Certificates | Authorities, Import: pick the downloaded file | Tick Trust this CA to identify websites";
    "tls.import" => "Import for the emergency port";
    "tls.import.certificate" => "Certificate (PEM)";
    "tls.import.private_key" => "Private key (PEM)";
    "tls.import.hint" => "Paste or load the full chain, server certificate first, and its private key without a password.";
    "load_file" => "Load from a file";
    "tls.use_local" => "Use the local certificate";
    "tls.use_local_title" => "Use the local certificate?";
    "tls.use_local.confirm" => "The imported certificate is set aside and the local one, from this NAS's own authority, is served again.";
    "tls.warn.expires_soon" => "The certificate expires in less than 30 days.";
    "tls.warn.expired" => "The certificate has expired: browsers refuse it.";
    "tls.warn.names_not_covered" => "Some names and addresses of this NAS are not in the certificate.";
    "tls.warn.chain_incomplete" => "The intermediate certificates are missing: some devices will not trust it. Import the full chain.";
    "tls.missing_names" => "Not covered:";
    "tls.reload_note" => "The page reconnects with the new certificate; a browser that trusted the old one may ask again.";
    "wa.tab.domains" => "Domains";
    "wa.prob.domain_name_too_long" => "The name under the domain would be longer than 253 characters: shorten the app's label or the domain.";
    "wa.check.domain_dns" => "Domain name resolves";
    "wa.check.domain_certificate" => "Domain certificate";
    "wa.fix.domain_dns" => "The domain name does not point to this NAS. Open the domain on the Domains tab and run its checks.";
    "wa.fix.domain_certificate" => "The domain certificate is missing or not valid. Open the domain on the Domains tab to see why.";
    "dom.title" => "Own domains";
    "dom.intro" => "Each app gets <app>.<domain> with a trusted certificate from Let's Encrypt, on every device of the network.";
    "dom.add" => "Add domain";
    "dom.add_go" => "Add and apply";
    "dom.open" => "Open";
    "dom.check_named" => "Check {name}";
    "dom.empty" => "No own domain yet";
    "dom.empty_text" => "With a domain you own (ideally a subdomain such as nas.example.com) the apps open with a trusted certificate, phones and mobile apps included, without opening any port.";
    "dom.external" => "Another reverse proxy serves ports 80 and 443: no certificate is requested here. The LAN records are still written.";
    "dom.col.domain" => "Domain";
    "dom.col.records" => "DNS records";
    "dom.col.cert" => "Certificate";
    "dom.state.ok" => "Working";
    "dom.gone" => "This domain no longer exists.";
    "dom.prob.credential_missing" => "The provider's access values are missing or cannot be read (for example after a restore without its key): enter them again.";
    "dom.prob.records_failed" => "The DNS records could not be written. rynas tries again every 10 minutes.";
    "dom.prob.records_manual" => "Some DNS records must be added by hand at the provider: they are listed below.";
    "dom.prob.records_conflict" => "A record rynas did not write holds other values: take the name over below, or fix it at the provider.";
    "dom.prob.no_lan_address" => "This NAS has no LAN address to put in the records.";
    "dom.prob.staging" => "Test certificates are on: browsers do not trust them.";
    "dom.ps.credential_missing" => "Token missing";
    "dom.ps.records_failed" => "Records not written";
    "dom.ps.records_manual" => "Records to add by hand";
    "dom.ps.records_conflict" => "Record to take over";
    "dom.ps.no_lan_address" => "No LAN address";
    "dom.ps.staging" => "Test certificates on";
    "dom.fix.enter_values" => "Enter the values again";
    "dom.fix.no_staging" => "Use trusted certificates";
    "dom.rec.not_yet" => "Not written yet";
    "dom.rec.off" => "Not managed";
    "dom.rec.failed" => "{n} failed";
    "dom.rec.conflicts" => "{n} to take over";
    "dom.rec.manual" => "To add by hand";
    "dom.rec.ok" => "Up to date";
    "dom.rec.text" => "rynas points the domain and every name below it to this NAS's LAN address, at the provider, and keeps them current.";
    "dom.rec.not_yet_text" => "The records are written right after the first apply.";
    "dom.rec.off_text" => "rynas does not manage the records of this domain: point them to this NAS yourself.";
    "dom.rec.none" => "No records to write.";
    "dom.rec.manual_text" => "Add these records at your DNS provider; rynas cannot write them there.";
    "dom.rec.write" => "Write records now";
    "dom.rec.at" => "Last written {when}";
    "dom.cert.title" => "Certificates";
    "dom.cert.none" => "No certificate yet: it is requested once the change is applied and usually arrives within a minute.";
    "dom.test.text" => "Checks the records, name resolution on this NAS and on the internet, router protection against DNS rebinding and the certificate.";
    "dom.ui_urls" => "Interface";
    "dom.cert_names" => "Certificate names";
    "dom.provider" => "DNS provider";
    "dom.challenge" => "Validation";
    "dom.settings" => "Settings";
    "dom.remove" => "Remove domain";
    "dom.remove_title" => "Remove this domain?";
    "dom.remove_go" => "Remove";
    "dom.remove_text" => "The names of this domain stop working. rynas deletes the records it wrote, only while they still hold its values; the credential stays.";
    "dom.take.title" => "Take over {name}?";
    "dom.take.text" => "This record was not written by rynas and now holds {now}. rynas replaces it with {wanted} and keeps it current from now on.";
    "dom.form.intro" => "Four steps: the provider that hosts the domain's DNS, an access token, the domain, then a preview of what changes. Nothing is written before you add it.";
    "dom.step.token" => "Access to the provider";
    "dom.step.domain" => "The domain and how it is validated";
    "dom.step.records" => "What changes";
    "dom.p.records" => "rynas writes the LAN records";
    "dom.p.manual" => "LAN records added by hand";
    "dom.p.per_name" => "One record per name";
    "dom.p.unscoped" => "Tokens of this provider cannot be limited to one domain. Prefer CNAME delegation, so the token never touches the main zone.";
    "dom.p.duckdns" => "DuckDNS keeps one address per name: rynas sets it to this NAS's LAN address on every sync, replacing a public address another updater keeps there.";
    "dom.f.optional" => "{name} (optional)";
    "dom.f.never_shown" => "Values are stored encrypted and never shown again.";
    "dom.name" => "Domain";
    "dom.err.name" => "This domain cannot be used";
    "dom.name_help" => "A name you own, ideally a subdomain used only for this NAS. DuckDNS: <name>.duckdns.org.";
    "dom.ch.dns01" => "DNS (recommended)";
    "dom.ch.cname_delegation" => "CNAME delegation";
    "dom.ch.http01" => "HTTP";
    "dom.ch.dns01_help" => "The provider's API proves the domain is yours. One wildcard certificate, no open port.";
    "dom.ch.cname_delegation_help" => "_acme-challenge of the domain points by CNAME to another zone that the token can write. The main zone stays out of reach; its records are added by hand.";
    "dom.ch.http01_help" => "Port 80 must be reachable from the internet. One certificate per name, and app names become public in the certificate logs.";
    "dom.delegate_to" => "CNAME target";
    "dom.delegate_to_help" => "The name _acme-challenge.<domain> points to (with acme-dns, the account's full domain).";
    "dom.wildcard" => "Wildcard certificate";
    "dom.wildcard_help" => "One certificate for *.<domain> and <domain>: app names stay out of the public certificate logs.";
    "dom.lan_record" => "Write the LAN records";
    "dom.lan_record_help" => "<domain> and *.<domain> point to this NAS's LAN address, so every device finds the apps.";
    "dom.lan_ipv6" => "Also IPv6 records";
    "dom.lan_ipv6_help" => "Only with a stable IPv6 address on the LAN interface.";
    "dom.ui" => "Interface on the domain itself";
    "dom.ui_help" => "https://<domain> opens this interface.";
    "dom.enabled" => "Active";
    "dom.enabled_help" => "Turned off, the names stop working and rynas deletes the records it wrote.";
    "dom.plan.text" => "rynas reads the provider (nothing is written) and lists the records the save would create or change.";
    "dom.plan.go" => "Preview the DNS changes";
    "dom.plan.conflict" => "Some names already hold records rynas did not write. They stay as they are unless you tick Replace.";
    "dom.cred.title" => "Provider credentials";
    "dom.cred.add" => "Add credential";
    "dom.cred.edit" => "Edit credential";
    "dom.cred.empty" => "None yet: the first one is created with a domain.";
    "dom.cred.stored" => "Stored: {fields}";
    "dom.cred.no_values" => "No values stored";
    "dom.cred.used" => "Domains using it: {n}";
    "dom.cred.in_use" => "A domain uses it";
    "dom.cred.remove" => "Remove this credential?";
    "dom.cred.remove_text" => "The stored values are deleted from this NAS. The token stays valid at the provider: revoke it there too.";
    "dom.cred.label" => "Name (optional)";
    "dom.cred.label_help" => "To recognize it in the list, for example “token for example.com”.";
    "dom.cred.keep" => "Leave the fields empty to keep the stored values; to change them, enter them all again.";
    "dom.cred.pick" => "Credential";
    "dom.cred.new" => "New credential";
    "dom.cred.replace" => "Enter new values";
    "dom.acme.title" => "Let's Encrypt account";
    "dom.acme.text" => "Shared by every domain. Certificates renew on their own, well before they expire.";
    "dom.acme.email" => "Contact e-mail";
    "dom.acme.email_help" => "Optional: the certificate authority may write about problems with the account.";
    "dom.acme.staging" => "Test certificates";
    "dom.acme.staging_help" => "For trying a setup without hitting the rate limits. Browsers do not trust them: turn it off afterwards.";
    "dom.acme.profile" => "Certificate profile";
    "dom.acme.profile_default" => "Default of the authority";
    "dom.acme.profile_tlsserver" => "TLS server (shorter, web only)";
    "dom.acme.profile_classic" => "Classic";
    "dom.check.dns_record" => "DNS records written";
    "dom.check.dns_nas" => "Name resolves on this NAS";
    "dom.check.dns_public" => "Name resolves on the internet";
    "dom.check.rebind" => "Router lets the answer through";
    "dom.check.certificate" => "Certificate obtained";
    "dom.check.renewal" => "Renewal";
    "dom.check.hairpin" => "Name points to the LAN";
    "dom.fix.dns_record" => "Look at the DNS records above: write them again, take over a name, or add the listed ones by hand.";
    "dom.fix.dns_nas" => "This NAS does not resolve the name to its LAN address yet. Changes at the provider can take a few minutes: try again shortly.";
    "dom.fix.dns_public" => "Public resolvers do not see the record yet, or outbound DNS is blocked on this network.";
    "dom.fix.rebind" => "The router drops public answers that point to private addresses (DNS rebinding protection). Allow this domain there; the detail says how for common routers.";
    "dom.fix.certificate" => "The certificate is not there yet. Check the token's permissions and the engine's error in the detail; test certificates are not trusted.";
    "dom.fix.renewal" => "Renewal is late or failing: usually the token was revoked or lost its permissions. Enter new values for the credential.";
    "dom.fix.hairpin" => "The name points to a public address: devices of the network then depend on the router forwarding back to the NAS. Turn on Write the LAN records.";
    "dns.col.name" => "Name";
    "dns.col.type" => "Type";
    "dns.col.action" => "Change";
    "dns.col.value" => "Value";
    "dns.act.keep" => "Already right";
    "dns.act.create" => "Create";
    "dns.act.update" => "Update";
    "dns.act.delete" => "Delete";
    "dns.act.manual" => "Add by hand";
    "dns.act.unknown" => "Not known";
    "dns.act.conflict" => "Not written by rynas";
    "dns.act.failed" => "Failed";
    "dns.copy" => "Copy";
    "dns.copy_all" => "Copy all";
    "dns.take_over" => "Replace";
    "dns.take_over_btn" => "Take over";
    "dns.f.api_token" => "API token";
    "dns.f.token" => "Token";
    "dns.f.api_key" => "API key";
    "dns.f.api_secret_key" => "Secret API key";
    "dns.f.endpoint" => "API endpoint";
    "dns.f.application_key" => "Application key";
    "dns.f.application_secret" => "Application secret";
    "dns.f.consumer_key" => "Consumer key";
    "dns.f.bearer_token" => "Personal access token";
    "dns.f.access_key_id" => "Access key ID";
    "dns.f.secret_access_key" => "Secret access key";
    "dns.f.region" => "Region";
    "dns.f.auth_token" => "API access token";
    "dns.f.server" => "Primary DNS server";
    "dns.f.key_name" => "TSIG key name";
    "dns.f.key_alg" => "TSIG algorithm";
    "dns.f.key" => "TSIG key";
    "dns.f.server_url" => "Server URL";
    "dns.f.username" => "Account user name";
    "dns.f.subdomain" => "Subdomain";
    "dns.f.password" => "Password";
    "cert.inv.title" => "All certificates";
    "cert.inv.text" => "Every certificate this NAS serves. Trouble with one becomes a notification.";
    "cert.inv.at" => "Read {when}";
    "cert.col.cert" => "Certificate";
    "cert.col.renew" => "Renews from";
    "cert.kind.local_ca" => "Local authority";
    "cert.kind.interface" => "Interface (emergency port)";
    "cert.kind.proxy_local" => "Local names";
    "cert.kind.acme" => "Let's Encrypt";
    "cert.kind.imported" => "Imported";
    "wa.import.title" => "Imported certificates";
    "wa.import.add" => "Import a certificate";
    "wa.import.intro" => "A certificate made elsewhere (a router's ACME client, a company authority, a bought wildcard) serves the interface and app addresses it covers.";
    "wa.import.empty" => "No imported certificate. Addresses use the Let's Encrypt or the local certificate.";
    "wa.import.id" => "Name (short id)";
    "wa.import.id_help" => "Lowercase letters, digits and dashes, at most 32. It also names the watched folder.";
    "wa.import.label" => "Description";
    "wa.import.source" => "How the files arrive";
    "wa.import.source.upload" => "Upload the files";
    "wa.import.source.folder" => "Another tool pushes them into a folder";
    "wa.import.certificate" => "Certificate chain (PEM: server certificate first, then intermediates)";
    "wa.import.private_key" => "Private key (PEM, unencrypted)";
    "wa.import.inspect" => "Check";
    "wa.import.covers" => "Takes over these addresses";
    "wa.import.covers_none" => "It covers none of the addresses this NAS serves";
    "wa.import.shadowed" => "Served by another imported certificate";
    "wa.import.replaces_acme" => "Replaces the Let's Encrypt certificate for";
    "wa.import.folder" => "Watched folder";
    "wa.import.folder_help" => "Write fullchain.pem and privkey.pem here; rynas checks them and reloads within seconds. Members of the group rynas-cert-drop can write here.";
    "wa.import.folder_ssh" => "Example: a push account over SSH (acme.sh)";
    "wa.import.ssh_note" => "These lines are not applied by rynas: the SSH server configuration stays yours. Replace <name> with the certificate's name.";
    "wa.import.check_now" => "Check the folder now";
    "wa.import.waiting" => "Waiting for the files";
    "wa.import.last_error" => "Last files refused";
    "wa.import.kept" => "The previous certificate stays in use.";
    "wa.import.imported_at" => "Taken on";
    "wa.import.replace" => "Replace the files";
    "wa.import.saved" => "Certificate saved; the proxy uses it within seconds";
    "wa.import.delete" => "Remove";
    "wa.import.delete_title" => "Remove this certificate?";
    "wa.import.delete_confirm" => "Its addresses go back to the Let's Encrypt or local certificate.";
    "wa.import.not_renewed" => "rynas does not renew imported certificates.";
    "nev.web_access.cert_import_failed.title" => "Certificate files for {name} were refused";
    "nev.web_access.cert_import_failed.body" => "The previous certificate stays in use. {error}";
    "dom.step.provider" => "Who hosts the domain's DNS";
    "dns.now_label" => "Now:";
    "dns.why.failed" => "The provider refused the change; rynas tries again in 10 minutes.";
    "dns.why.conflict" => "Holds values rynas did not write: nothing is written until you take the name over.";
    "dns.why.conflict_unwanted" => "No longer needed, but changed since rynas wrote it: left as it is and no longer managed.";
    "dns.why.unknown" => "The provider could not be read.";
    "dns.hint.cloudflare" => "Create an API token (My Profile → API Tokens) with the permission Zone → DNS → Edit, limited to this one zone (Zone Resources: Include → Specific zone). Never the Global API Key.";
    "dns.hint.desec" => "Create a token whose policy allows writes only to this domain (optionally only to *.<domain>, <domain> and _acme-challenge.<domain>), optionally limited to your public address.";
    "dns.hint.hetzner" => "Use a Hetzner Console (Cloud API) token with Read & Write permission; tokens of the old DNS console no longer work. A token covers a whole project: keep the zone in a project of its own.";
    "dns.hint.porkbun" => "Create an API key, turn on API access for this domain only (Domain Management → Details → API Access) and limit the key to DNS if offered.";
    "dns.hint.ovh" => "Create an application and a consumer key whose rights allow only GET/POST/PUT/DELETE on /domain/zone/<zone>/* (and GET /domain/zone).";
    "dns.hint.duckdns" => "The account token covers all your DuckDNS names. The domain is <name>.duckdns.org and every name below it gets the same address.";
    "dns.hint.gandi" => "Create a personal access token limited to this domain with the permission to manage its technical configuration.";
    "dns.hint.route53" => "Create an IAM user whose policy allows route53:ChangeResourceRecordSets and ListResourceRecordSets on this one hosted zone, plus route53:ListHostedZonesByName and GetChange.";
    "dns.hint.digitalocean" => "Tokens cannot be limited to one domain: prefer CNAME delegation of _acme-challenge to a zone used only for that.";
    "dns.hint.rfc2136" => "Use a TSIG key whose update policy allows only TXT records of _acme-challenge.<domain>.";
    "dns.hint.acmedns" => "Register an account on the acme-dns server and add the CNAME _acme-challenge.<domain> → <fulldomain> once. The account can change only that one TXT record.";
    "wa.import.names" => "Names it may carry";
    "wa.import.names_help" => "A pushed certificate is used only for exactly these names; leave empty to take the names of the first certificate pushed.";
    "wa.import.names_why" => "Recommended: every account that can push into one folder can push into all of them, so the names decide what a push may change. A certificate for other names waits for your approval.";
    "wa.import.problem.refused" => "The last files were refused";
    "wa.import.problem.names_changed" => "A new certificate for other names is waiting for your approval";
    "wa.import.problem.not_yet_valid" => "A new certificate starts {when}; the current one stays in use until then";
    "wa.import.problem.group_missing" => "The group rynas-cert-drop is missing: no account can write into the folder. Reinstall the rynas package.";
    "wa.import.pending_names" => "Names of the new certificate";
    "wa.import.current_names" => "Names in use";
    "wa.import.accept" => "Accept the new names";
    "wa.import.accept_title" => "Accept the new names?";
    "wa.import.accept_confirm" => "The new certificate takes over these addresses at once. Accept only if you expected the change.";
    "wa.import.private_ca" => "Self-signed or from a private authority: devices trust it only once that authority is installed on them.";
    "wa.import.code.account" => "On the NAS: the push account (once, as root)";
    "wa.import.code.sshd" => "On the NAS: add to /etc/ssh/sshd_config, then reload ssh";
    "wa.import.code.acme" => "On the router or machine running acme.sh";
    "wa.import.err.certificate" => "The certificate chain was refused";
    "wa.import.err.private_key" => "The private key was refused";
    "nev.web_access.cert_import_names_changed.title" => "The certificate pushed for {name} is for other names";
    "nev.web_access.cert_import_names_changed.body" => "It is for {names}. The previous certificate stays in use until you accept the new names on Network › Web access › Certificate.";
    "nev.web_access.cert_import_ok.title" => "Certificate files for {name} are fine again";
    "cert.staging" => "Test";
    "cert.st.ok" => "Valid";
    "cert.st.expires_soon" => "Expires soon";
    "cert.st.expired" => "Expired";
    "cert.st.renewal_overdue" => "Renewal failing";
    "cert.st.pending" => "Being obtained";
    "cert.st.failed" => "Not obtained";
    "nev.web_access.cert_expires_soon.title" => "Certificate for {name} expires soon";
    "nev.web_access.cert_expires_soon.body" => "It expires within 14 days. Open Network › Web access › Certificate to see why it was not renewed.";
    "nev.web_access.cert_renewal_overdue.title" => "Certificate for {name} is not renewing";
    "nev.web_access.cert_renewal_overdue.body" => "Renewal is past due. Check the domain's token on Network › Web access › Domains.";
    "nev.web_access.cert_failed.title" => "Certificate for {name} could not be obtained";
    "nev.web_access.cert_failed.body" => "Open the domain on Network › Web access › Domains and run its checks.";
    "nev.web_access.cert_expired.title" => "Certificate for {name} has expired";
    "nev.web_access.cert_expired.body" => "Browsers refuse the names it covers. Open Network › Web access › Domains to fix it.";
    "nev.web_access.cert_ok.title" => "Certificate for {name} is fine again";
    "docker.install_incomplete" => "A previous Docker install did not finish: {n} packages are half-installed.";
    "docker.install_incomplete_hint" => "Finishing repairs them and completes the install.";
    "docker.install_unfinished" => "The Docker install rynas started is missing its last steps (the version pin or the docker-apps account).";
    "docker.install_finish" => "Finish the Docker install";
    "apps.convert" => "Convert to custom stack";
    "apps.convert_go" => "Convert";
    "apps.conv.title" => "Convert {name} to a custom stack?";
    "apps.conv.text" => "The app becomes an ordinary compose stack that you manage yourself.";
    "apps.conv.p1" => "Containers, data and files stay as they are; nothing restarts.";
    "apps.conv.p2" => "It leaves this list: no more catalog updates, migrations or app rollbacks.";
    "apps.conv.p3" => "The Docker page keeps editing, updating and snapshotting it. This cannot be undone from here.";
    "apps.conv.done" => "Converted to a custom stack";
    "apps.uninstall" => "Uninstall…";
    "apps.uninstall_go" => "Uninstall";
    "apps.un.title" => "Uninstall {name}?";
    "apps.un.text" => "The containers are removed. The stack folder, the data and the snapshots stay on disk, so the app can be started again later.";
    "apps.un.delete" => "Also delete the data";
    "apps.un.delete_text" => "Deletes the stack folder, its snapshots and the folders the app placed elsewhere. Shared folders are never touched.";
    "apps.rb.text" => "The app goes back to the snapshot of {when}: files, data in the stack folder and image versions. A snapshot of the current state is taken first.";
    "apps.upd.app_version" => "App version";
    "apps.upd.pkg_version" => "Catalog package";
    "apps.upd.images" => "Image changes";
    "apps.upd.migrations" => "Setting migrations";
    "apps.upd.migrations_text" => "The stored answers are adapted for versions {list}.";
    "apps.upd.answers" => "New questions";
    "apps.upd.answers_text" => "This version asks for values the app did not have before.";
    "apps.upd.snapshot_help" => "Taken before anything changes; a failed update goes back to it by itself, and “Roll back” returns to it later.";
    "apps.upd.no_snapshot" => "The stack folder is not a btrfs subvolume: no snapshot is taken. A failed update puts the previous files back.";
    "apps.upd.up_to_date" => "The app is up to date.";
    "apps.upd.downgrade" => "The catalog has an older version than the installed one: the update is refused.";
    "apps.upd.none" => "Everything is up to date";
    "apps.upd.none_text" => "New catalog versions of the installed apps show up here.";
    "apps.upd.count" => "Updates available: {n}";
    "apps.src.title" => "Catalog source";
    "apps.src.checked" => "App catalog checked";
    "apps.src.source" => "In use";
    "apps.src.online" => "Online catalog {serial} ({n} newer entries)";
    "apps.src.shipped_only" => "Shipped with rynas";
    "apps.src.shipped" => "Shipped catalog serial";
    "apps.src.published" => "Published";
    "apps.src.last_check" => "Last check";
    "apps.src.never" => "Never";
    "apps.src.no_key" => "Online catalog updates are not available in this build yet: the catalog comes with rynas updates.";
    "apps.src.off" => "Online catalog updates are turned off in the Docker settings.";
    "apps.src.expired" => "The online catalog has not been renewed in time: rynas keeps using it, but no newer catalog arrived.";
    "apps.src.error" => "The last check failed; the catalog in use stays:";
    "apps.upd.pick" => "Select";
    "apps.upd.selected" => "Update selected";
    "apps.upd.selected_title" => "Update the selected apps?";
    "apps.upd.many_text" => "One after the other, each with a snapshot first when its folder allows it: {names}.";
    "apps.upd.skipped" => "Need answers, update them one by one: {names}";
    "apps.upd.failed" => "Not updated: {names}";
    "apps.upd.all_done" => "The selected apps are updated";
    "apps.chg.added" => "Added";
    "apps.chg.removed" => "Removed";
    "apps.chg.digest" => "Rebuilt image";
    "sf.advanced" => "Advanced options";
    "sf.optional" => "(optional)";
    "sf.none" => "None";
    "sf.no_questions" => "This app asks no questions.";
    "sf.when_on" => "When on:";
    "sf.when_off" => "When off:";
    "sf.err.required" => "Required.";
    "sf.err.enum" => "Choose one of the listed values.";
    "sf.err.integer" => "Enter a whole number.";
    "sf.err.min" => "At least {n}.";
    "sf.err.max" => "At most {n}.";
    "sf.err.min_len" => "At least {n} characters.";
    "sf.err.max_len" => "At most {n} characters.";
    "sf.err.pattern" => "Not in the expected format.";
    "sf.err.hostname" => "Enter a host name, e.g. nas.example.lan.";
    "sf.err.uri" => "Enter an address starting with http:// or https://.";
    "sf.err.email" => "Enter an e-mail address.";
    // PC backup receive (Backup › PCs, bk.*) and its tasks and events
    "bk.tab.pcs" => "PCs: btrfs snapshots";
    "bk.intro" => "PCs with btrfs send read-only snapshots of their subvolumes here, only what changed since the last one. Each snapshot is a complete copy of the PC you can browse. rynas-agent sends them over HTTPS; PCs already set up with btrbk can keep sending over SSH.";
    "bkpc.empty" => "No PCs yet";
    "bkpc.empty_text" => "Add a PC: rynas-agent on the PC sends its snapshots here. A folder that already holds received snapshots can be adopted.";
    "bk.add" => "Add PC";
    "bk.adopt" => "Adopt a folder";
    "bkpc.create" => "Create";
    "bk.open" => "Open";
    "bk.col.pc" => "PC";
    "bk.col.folder" => "Folder";
    "bk.col.snapshots" => "Snapshots";
    "bk.col.space" => "Space";
    "bk.col.subvolume" => "Subvolume";
    "bk.col.newest" => "Newest";
    "bk.col.snapshot" => "Snapshot name";
    "bk.col.received" => "Received";
    "bk.st.late" => "Late";
    "bk.st.waiting" => "Waiting for the first backup";
    "bk.st.missing" => "Folder missing";
    "bk.problem.sshd_missing" => "No SSH server is installed or running: PCs cannot send. Install openssh-server.";
    "bk.problem.sshd_allow_list" => "The SSH server only lets in the users of AllowUsers/AllowGroups: add {subject} there, or PCs are refused.";
    "bk.problem.sudo_missing" => "sudo is not installed: the backup account runs btrfs through it. Install sudo.";
    "bk.problem.btrfs_missing" => "btrfs-progs is not installed.";
    "bk.problem.filter_missing" => "The command filter {subject} is missing: reinstall rynas.";
    "bk.mode" => "Where the snapshots go";
    "bk.mode.new" => "New folder";
    "bk.mode.new_help" => "A new btrfs subvolume, readable only by administrators.";
    "bk.mode.adopt_help" => "A folder with snapshots a PC already sent (for example copied from another NAS). Nothing inside changes, so the PC goes on with incremental sends.";
    "bk.f.device" => "PC name";
    "bk.f.device_help" => "Lowercase letters, digits and -, at most 32.";
    "bkpc.f.path" => "Folder on the NAS";
    "bkpc.f.path_help" => "On a btrfs volume. Letters, digits and _ @ + . - only.";
    "bk.f.key" => "Public key of the PC";
    "bk.f.key_help" => "The .pub file made on the PC (the steps are in the PC's panel). You can add it later.";
    "bk.f.rules" => "Deleting old snapshots";
    "bk.f.allowed" => "The PC may delete old snapshots here";
    "bk.f.allowed_help" => "btrbk deletes what its target_preserve no longer keeps. Off: the PC can only add, and old snapshots stay until you delete them.";
    "bk.f.keep" => "Always keep the newest";
    "bkpc.f.keep_help" => "Per subvolume, whatever the PC asks.";
    "bk.f.min_age" => "Never delete before (days)";
    "bk.f.min_age_help" => "A snapshot younger than this stays even if the PC asks to delete it.";
    "bk.f.rules_note" => "These rules protect the backups from a PC that is broken or in the wrong hands: a deletion they refuse shows up as an error in btrbk's log on the PC. Keep them below the PC's target_preserve_min to avoid those errors.";
    "bk.f.destination" => "Copy into";
    "bk.f.destination_help" => "An existing folder on the NAS. The copied files get the owner of that folder; set-user/group-ID bits and security attributes are dropped. Nothing is overwritten.";
    "bk.adopt.check" => "Check the folder";
    "bk.adopt.found" => "{n} received snapshots found.";
    "bk.adopt.none" => "No btrbk snapshots in this folder. It can be adopted anyway and the PC starts with a full send.";
    "bk.adopt.incomplete" => "Interrupted receives: {n}. btrbk deletes them before it sends those snapshots again.";
    "bk.adopt.other" => "Other entries left alone: {n}.";
    "bk.adopt.perm_change" => "Adopting changes owner, group and permissions of the folder itself (not of its content):";
    "bk.adopt.perm_confirm" => "Change owner and permissions of the folder";
    "bk.adopt.not_subvolume" => "The folder is not a subvolume of its own: it works, but its space is not measured apart from the volume.";
    "bk.added" => "{device} is ready";
    "bk.saved" => "PC settings saved";
    "bk.settings" => "Settings";
    "bk.settings_title" => "Settings of {device}";
    "bk.remove" => "Remove";
    "bk.remove_title" => "Remove {device}?";
    "bk.remove_text" => "The PC's key stops working. The folder and its snapshots stay: delete them in Files when you no longer need them, or adopt the folder again later.";
    "bk.removed" => "{device} removed, its snapshots kept";
    "bk.measure" => "Measure space";
    "bk.measured" => "Space measured";
    "bk.usage_text" => "{excl} only in these snapshots, {total} in all (measured {when})";
    "bk.usage_none" => "Not measured yet";
    "bk.rules.off" => "Only the NAS deletes";
    "bk.rules.on" => "Handled by the PC: the newest {keep} stay, none younger than {days} days";
    "bk.rules.on_one" => "Handled by the PC: the newest stays, none younger than {days} days";
    "bk.rules.on_keep" => "Handled by the PC: the newest {keep} always stay";
    "bk.rules.on_keep_one" => "Handled by the PC: the newest always stays";
    "bk.rules.on_days" => "Handled by the PC: none younger than {days} days";
    "bk.rules.on_free" => "Handled by the PC, as its btrbk decides";
    "bk.alert_off" => "Off";
    "bk.alert_days" => "After {n} days";
    "bk.incomplete" => "Interrupted receives";
    "bk.key" => "Key";
    "bk.adopted" => "Origin";
    "bk.adopted_yes" => "Adopted folder";
    "bk.no_key" => "No key yet: the PC cannot connect. Add its public key in Settings.";
    "bk.steps" => "Set up the PC";
    "bk.steps_intro" => "On the PC, with sudo (the commands below use it): make a key for btrbk, add the lines to /etc/btrbk/btrbk.conf (change volume and subvolumes to the PC's layout), then connect once to accept the NAS key and try a dry run.";
    "bk.step.key" => "1. Key of the PC";
    "bk.step.config" => "2. btrbk.conf";
    "bk.step.test" => "3. First connection and test";
    "bk.step.line" => "Line written on the NAS (authorized_keys)";
    "bk.steps_note" => "The key only runs the rynas filter: btrfs receive into this folder, deleting this PC's snapshots under the rules above, and the read-only commands btrbk needs. No shell, no other folder.";
    "bk.snapshots" => "Received snapshots";
    "bk.no_snapshots" => "Nothing received yet.";
    "bk.snap.protected" => "Kept by the NAS";
    "bk.snap.incomplete" => "Interrupted";
    "bk.snap.not_subvolume" => "Not a subvolume";
    "bk.browse" => "Browse";
    "bk.browse_note" => "Read only: what the PC sent, as it was at that moment.";
    "bkpc.copy" => "Copy to the NAS";
    "bk.copy_this" => "Copy this folder";
    "bk.copy_text" => "{path} is copied with its folders into the folder you choose.";
    "bkpc.copied" => "{path} copied";
    "bkpc.col.restore" => "Empty-disk restore";
    "bkpc.bm.ready" => "Ready";
    "bkpc.bm.disks" => "Disks";
    "bkpc.bm.serial" => "S/N {serial}";
    "bkpc.bm.part" => "Partition {n}";
    "bkpc.bm.boot_copies" => "Boot files copied";
    "bkpc.restore.intro" => "To restore, make a restore code (single use, valid for one hour, read-only, this PC only), boot any live Linux on the PC and run the agent below. The agent receives the chosen snapshots into a new btrfs filesystem, or adds subvolumes to the PC's existing one.";
    "bkpc.first_run" => "First backup";
    "bkpc.first_run_intro" => "The PC is set up once the first snapshot of each subvolume has arrived.";
    "bkpc.first_run_ok" => "Received";
    "bkpc.first_run_wait" => "Waiting";
    "bkpc.bm.not_ready" => "Not ready";
    "bkpc.bm.title" => "Restore on an empty disk";
    "bkpc.bm.manifest" => "Layout manifest";
    "bkpc.bm.loader" => "Boot loader";
    "bkpc.bm.subvolumes" => "Subvolumes needed";
    "bkpc.bm.no_snapshots" => "No complete snapshot yet.";
    "bkpc.bm.no_manifest" => "The newest snapshots carry no layout manifest (/.rynas/manifest.json). rynas-agent writes it before every run.";
    "bkpc.bm.manifest_invalid" => "The layout manifest cannot be read.";
    "bkpc.bm.manifest_unsupported" => "The layout manifest has format {subject}, which this NAS does not know yet.";
    "bkpc.bm.no_root" => "The manifest names no subvolume mounted at /.";
    "bkpc.bm.subvolume_missing" => "{subject} is needed for a restore but has no complete snapshot here.";
    "bkpc.bm.loader_unknown" => "The manifest does not name the boot loader.";
    "bkpc.bm.manifest_stale" => "The manifest is {subject} days older than the snapshot that carries it.";
    "bkpc.bm.latest_run_incomplete" => "The newest run lacks {subject}: a restore takes it from an older run.";
    "bkpc.bm.layout" => "Subvolumes";
    "bkpc.owner.agent_help" => "The NAS deletes old snapshots with these counts; rynas-agent can only add. The newest snapshot of each subvolume always stays: the next send starts from it.";
    "pcb.how" => "Sent by";
    "pcb.how.agent" => "rynas-agent (recommended)";
    "pcb.how.ssh" => "btrbk over SSH";
    "pcb.how.agent_help" => "rynas-agent on the PC takes the snapshots and sends them over HTTPS. No SSH, no btrbk: install the agent and pair it with a code.";
    "pcb.how.ssh_help" => "btrbk on the PC sends over SSH with a key of its own. For PCs already set up that way; you write the configuration by hand.";
    "pcb.via.agent" => "rynas-agent";
    "pcb.via.ssh" => "btrbk (SSH)";
    "pcb.f.pc" => "PC";
    "pcb.f.pc_new" => "A new PC (pair it next)";
    "pcb.f.pc_help" => "A PC already paired for file backup can move here: it keeps its name and switches to btrfs snapshots. Its saved file versions stay in their group.";
    "pcb.pair_after" => "Once the folder exists, its panel gives the pairing code for the PC.";
    "pcb.link_failed" => "The folder was created, but the PC could not be switched to it: {error}";
    "pcb.switch_to_agent" => "From now on rynas-agent sends to this folder: the btrbk key stops working. The snapshots stay, so the agent goes on from them; turn btrbk's timer off on the PC.";
    "pcb.switch_to_ssh" => "From now on btrbk sends to this folder over SSH with the key below; rynas-agent can no longer send here.";
    "pcb.ssh_recommend" => "rynas-agent is simpler (no SSH key, no configuration file): switch in Settings at any time, the snapshots stay.";
    "pcb.hook" => "Layout manifest before each btrbk run";
    "pcb.hook_intro" => "With rynas-agent installed, these commands add a drop-in to btrbk.service that writes the layout manifest before every btrbk run.";
    "pca.pairing_btrfs" => "btrfs snapshots ({name})";
    "pcb.copy_of" => "Copy: {what}";
    "pcb.loader_unknown" => "Unknown";
    "pcb.copy_skipped" => "Left out as key material: {files}";
    "pcb.copy_stale" => "The last copy failed, the previous one is kept: {error}";
    "pcb.first_run_so_far" => "Arrived so far: {names}";
    "pcb.first_run_no_manifest" => "Which subvolumes to expect comes with the layout manifest (rynas-agent writes it).";
    "pcb.agent.title" => "rynas-agent";
    "pcb.agent.unpaired" => "No rynas-agent is paired for this PC yet. Make a code, install the agent on the PC and run the pairing command: the PC takes this folder's name.";
    "pcb.agent.every_h" => "Every {n} h";
    "pcb.agent.last_run" => "Subvolumes of the last run";
    "pcb.st.no_key" => "Waiting for the key";
    "pcb.last_ok_on" => "Last successful {when}";
    "pcb.last_received_on" => "Last received {when}";
    "pcb.last_seen_on" => "Last contact {when}";
    "pcb.never" => "No backup yet";
    "pcb.agent_os" => "{os} {arch} · agent {v}";
    "pcb.size_help" => "About the sizes";
    "pcb.n_subvols" => "{n} subvolumes";
    "pcb.n_subvols1" => "1 subvolume";
    "pcb.n_snaps" => "{n} snapshots";
    "pcb.n_snaps1" => "1 snapshot";
    "pcb.show_all" => "Show all ({n})";
    "pcb.show_fewer" => "Show fewer";
    "pcb.folder_rules" => "Folder and rules";
    "pcb.other_snaps" => "Other snapshots";
    "pcb.agent.sent" => "Sent";
    "pcb.agent.revoked" => "The agent of this PC was revoked: it cannot send any more. The snapshots stay.";
    "pcb.agent.revoke_text" => "The PC's token stops working at once and its agent can no longer send. The folder and its snapshots stay.";
    "pca.copy_install" => "Copy the install command";
    "pca.copy_pair" => "Copy the pairing command";
    "pca.on_btrfs" => "PCs sending btrfs snapshots: {n}.";
    "pca.on_btrfs_open" => "Open their tab";
    "pca.phase.preparing" => "Preparing…";
    "pca.phase.snapshot" => "Snapshots…";
    "pca.phase.finishing" => "Finishing…";
    "pcb.note.full" => "{name}: whole copy (no earlier snapshot to start from)";
    "pcb.note.incr" => "{name}: changes since {parent}";
    "pcb.note.error" => "error: {error}";
    "pcb.agent.code_waiting" => "A code for this PC is valid until {until}. It is shown only when made: if you no longer have it, cancel it and make a new one.";
    "pcb.agent.revoke_help" => "Revoke: the PC can no longer send. The folder and its snapshots stay.";
    "pcb.f.not_sent" => "Subvolumes not sent";
    "pcb.f.not_sent_help" => "The PC stops sending them and the NAS no longer expects them. Their snapshots stay.";
    "pcb.f.other_mounts" => "Other mount points not sent";
    "pcb.f.other_mounts_help" => "One per line. Caches, logs and container data stay out anyway.";
    "pcb.f.pc_no_btrfs" => "{pc} — root not on btrfs";
    "pcb.bm.no_manifest_btrbk" => "The newest snapshots carry no layout manifest (/.rynas/manifest.json). Add the last step of “Set up btrbk by hand” below: it writes the manifest before every btrbk run.";
    "pcb.age.h1" => "{n} hour";
    "pcb.age.h" => "{n} hours";
    "pcb.age.d1" => "{n} day";
    "pcb.age.d" => "{n} days";
    "pcb.age.w1" => "{n} week";
    "pcb.age.w" => "{n} weeks";
    "pcb.age.m1" => "{n} month";
    "pcb.age.m" => "{n} months";
    "pcb.age.y1" => "{n} year";
    "pcb.age.y" => "{n} years";
    "bkpc.issue.late" => "Nothing new from this PC since {when}.";
    "bkpc.issue.subvolume_late" => "{subject}: nothing new since {when}, while the other subvolumes arrive.";
    "bkpc.issue.subvolume_never" => "{subject} is expected but was never received.";
    "bkpc.issue.receive_failed" => "A receive was interrupted ({subject}) and nothing is receiving now. The PC sends that snapshot again at its next run.";
    "bkpc.issue.receive_stuck" => "A receive has been running since {when} ({subject}).";
    "bkpc.issue.low_space" => "{free} free on the volume, {needed} wanted for another full set of snapshots.";
    "bkpc.short.subvolume_late" => "A subvolume stopped";
    "bkpc.short.receive_failed" => "Receive interrupted";
    "bkpc.short.receive_stuck" => "Receive stuck";
    "bkpc.short.low_space" => "Low space";
    "bkpc.issue.shared_folder" => "This folder is the shared folder {subject}, lies inside it or holds it: {subject} stays read-only in File until the PC's backups move to a folder of their own.";
    "bkpc.short.shared_folder" => "In a shared folder";
    "bkpc.f.owner" => "Who deletes old snapshots";
    "bkpc.owner.nas" => "This NAS";
    "bkpc.owner.pc" => "The PC";
    "bkpc.owner.nas_help" => "The PC can only add snapshots, so a stolen or broken PC cannot delete its backups. The NAS keeps what the counts below say for each subvolume, and always the newest snapshot (the base of the next incremental send). It checks every hour.";
    "bkpc.owner.pc_help" => "btrbk on the PC deletes what its target_preserve no longer keeps, within the NAS rules below.";
    "bkpc.f.keep_nas_help" => "Per subvolume, by when the NAS received them, whatever the counts say.";
    "bkpc.f.min_age_nas_help" => "Counted from when the NAS received the snapshot.";
    "bkpc.f.alert_hours" => "Alert after hours without backups";
    "bkpc.f.alert_hours_help" => "A notification when nothing arrives for this many hours, also when a single subvolume stops; 0 turns it off.";
    "bkpc.alert_hours" => "After {n} hours";
    "bkpc.alert" => "Alert when nothing arrives";
    "bkpc.f.ignored" => "Subvolumes the PC no longer sends";
    "bkpc.f.ignored_help" => "No alert for them and not expected in a full set. Their snapshots stay.";
    "bkpc.ret.nas" => "Handled by the NAS: it keeps {counts}, and everything younger than {min}";
    "bkpc.prune" => "Delete old now";
    "bkpc.pruned" => "Old snapshots deleted";
    "bkpc.last_prune" => "Last cleanup";
    "bkpc.last_prune_text" => "{when}: {n} deleted";
    "bkpc.preview_frees" => "Frees at least {bytes}.";
    "bkpc.free" => "Free on the volume";
    "bkpc.free_text" => "{free} of {total}";
    "bkpc.receiving" => "Receiving now";
    "bkpc.view" => "Group by";
    "bkpc.view.run" => "Backup run";
    "bkpc.size_text" => "{ref} · {excl} its own";
    "bkpc.sizes_quota" => "Sizes come live from the volume's quota groups.";
    "bkpc.sizes_note" => "Size: what the snapshot references (about what a restore transfers) · what only it holds (about what deleting it alone frees). Measuring reads every file's extents at low priority and can take a while; each snapshot is measured once.";
    "bkpc.snap.parent" => "Base of the next send";
    "bkpc.run.lacks" => "Lacks {names}";
    "bkpc.delete_snap" => "Delete snapshot";
    "bkpc.delete_snap_title" => "Delete {name}?";
    "bkpc.delete_snap_text" => "The snapshot is deleted from the NAS and its space is freed in the background. The other snapshots and the PC's next incremental send are not affected.";
    "bkpc.deleted_snap" => "{name} deleted";
    "task.pcbackup.create" => "Add a PC backup target";
    "task.pcbackup.update" => "Change PC backup settings";
    "task.pcbackup.remove" => "Remove a PC backup target";
    "task.pcbackup.usage" => "Measure a PC's backups";
    "task.pcbackup.copy_out" => "Copy from a PC backup";
    "task.pcbackup.prune" => "Delete old snapshots of a PC";
    "task.pcbackup.sizes" => "Measure the snapshots of a PC";
    "task.pcbackup.delete_snapshot" => "Delete a PC snapshot";
    "task.files.copy" => "Copy files";
    "task.files.move" => "Move files";
    "task.files.delete" => "Delete files";
    "task.files.compress" => "Compress files";
    "task.files.extract" => "Extract an archive";
    "task.files.cleanup" => "Clean up after a transfer";
    "nev.pcbackup.late.title" => "No backup from {device} for {days} days";
    "nev.pcbackup.late.title_2" => "No backup from {device} for {hours} hours";
    "nev.pcbackup.late.body" => "Nothing received for {days} days (alert after {alert_days}).";
    "nev.pcbackup.late.body_2" => "Nothing received for {hours} hours (alert after {alert_hours} hours).";
    "nev.pcbackup.subvolume_late.title" => "{subject} of {device} is not backed up any more";
    "nev.pcbackup.subvolume_late.body" => "{device} still sends other subvolumes, but nothing new of {subject}.";
    "nev.pcbackup.subvolume_resumed.title" => "{subject} of {device} is backed up again";
    "nev.pcbackup.receive_failed.title" => "A backup from {device} was interrupted";
    "nev.pcbackup.receive_failed.body" => "{subject} was not received completely and nothing is receiving now.";
    "nev.pcbackup.receive_stuck.title" => "A backup from {device} has been running for over a day";
    "nev.pcbackup.receive_stuck.body" => "{subject} has been receiving for more than a day.";
    "nev.pcbackup.receive_ok.title" => "Backups from {device} are received normally again";
    "nev.pcbackup.low_space.title" => "Little space left for the backups of {device}";
    "nev.pcbackup.low_space.body" => "{free_gib} GiB free, {needed_gib} GiB wanted for another full set of snapshots.";
    "nev.pcbackup.space_ok.title" => "Enough space again for the backups of {device}";
    "nev.pcbackup.resumed.title" => "Backups from {device} arrive again";
    "nev.pcbackup.revoked.title" => "{device} revoked";
    "nev.pcbackup.removed.title" => "{device} removed";
    "nev.pcbackup.resumed.body" => "{device} sent a new snapshot.";
    // Restart / shut down (user menu, updates page, overlay; sysp.*) and its tasks and events
    "sysp.restart" => "Restart";
    "sysp.shutdown" => "Shut down";
    "sysp.restart_now" => "Restart now";
    "sysp.restart_title" => "Restart the NAS?";
    "sysp.restart_text" => "Every service stops and starts again: file shares, apps and this page are unavailable for a few minutes.";
    "sysp.shutdown_title" => "Shut down the NAS?";
    "sysp.shutdown_text" => "The NAS powers off. It starts again with its power button, Wake-on-LAN or a startup rule of the power schedule.";
    "sysp.when" => "When";
    "sysp.now" => "Now";
    "sysp.in_1" => "In 1 minute";
    "sysp.in_5" => "In 5 minutes";
    "sysp.checking" => "Checking what is running…";
    "sysp.check_failed" => "Could not check what is running.";
    "sysp.no_warnings" => "Nothing is running that would be interrupted.";
    "sysp.warnings" => "This would interrupt:";
    "sysp.anyway" => "You can go on anyway: interrupted work is not resumed by itself.";
    "sysp.w.raid_busy" => "{subject}: RAID {op}{pct} (it goes on after the start, from the beginning if interrupted)";
    "sysp.w.scrub_running" => "{subject} in progress";
    "sysp.w.task_running" => "Task in progress: {subject}";
    "sysp.w.scheduler_task_running" => "Scheduled task in progress: {subject}";
    "sysp.w.smb_open_files" => "{n} files open over SMB";
    "sysp.w.apt_running" => "Package manager running ({subject}): stopping it may leave packages half installed";
    "sysp.w.ups_on_battery" => "The UPS runs on battery: the NAS may not start again until mains power is back";
    "sysp.w.boot_trial" => "A new kernel starts on trial at the next boot; if it fails, the previous kernel comes back by itself";
    "sysp.op.resync" => "resync";
    "sysp.op.recovery" => "rebuild";
    "sysp.op.reshape" => "reshape";
    "sysp.op.check" => "check";
    "sysp.restart_scheduled" => "Restart scheduled";
    "sysp.shutdown_scheduled" => "Shut down scheduled";
    "sysp.restart_failed" => "The restart did not start.";
    "sysp.shutdown_failed" => "The shut down did not start.";
    "sysp.pending_restart" => "Restart scheduled: {at}.";
    "sysp.pending_restart_by" => "Restart scheduled: {at} (asked by {user}).";
    "sysp.pending_shutdown" => "Shut down scheduled: {at}.";
    "sysp.pending_shutdown_by" => "Shut down scheduled: {at} (asked by {user}).";
    "sysp.cancel" => "Cancel restart or shut down";
    "sysp.cancelled" => "Canceled: the NAS stays on.";
    "sysp.restarting" => "Restarting…";
    "sysp.shutting_down" => "Shutting down…";
    "sysp.going_text" => "Waiting for the NAS to stop.";
    "sysp.coming_text" => "Waiting for the NAS to come back. The sign-in page opens by itself.";
    "sysp.going_off_text" => "Waiting for the NAS to power off.";
    "sysp.off_title" => "The NAS is off";
    "sysp.off_text" => "You can close this page. Once the NAS is on again, reload it to sign in.";
    "sysp.reload" => "Reload page";
    "task.system.restart" => "Restart the NAS";
    "task.system.shutdown" => "Shut down the NAS";
    "nev.power.manual_restart.title" => "NAS restarting by {user}";
    "nev.power.manual_restart.body" => "{user} restarts the NAS (delay {delay_min} min).";
    "nev.power.manual_shutdown.title" => "NAS shutting down by {user}";
    "nev.power.manual_shutdown.body" => "{user} shuts the NAS down (delay {delay_min} min).";
    "nev.power.manual_cancelled.title" => "Restart or shut down canceled by {user}";
    "nev.power.manual_cancelled.body" => "The NAS stays on.";
    // PC backup, file method (Backup › PC file backup, pca.*) and its tasks and events
    "bk.tab.agent" => "PCs: file backup";
    "pca.intro" => "PCs have two methods. btrfs snapshots: the NAS receives the PC's own snapshots (the tab before; the best fit for PCs with btrfs). File backup: rynas-agent on any Linux PC sends its files and its disk layout into a repository on the NAS, and the same program restores the whole PC from a live system.";
    "pca.settings" => "Settings";
    "pca.setup" => "Choose where the PC backups go";
    "pca.setup_text" => "A folder on a volume holds one repository per backup group. Choose it first, then add the PCs.";
    "pca.choose_folder" => "Choose the folder";
    "pca.empty_text" => "Add a PC: you get a one-time code and the command to run on it.";
    "pca.col.pc" => "PC";
    "pca.col.group" => "Group";
    "pca.col.versions" => "Versions";
    "pca.col.uploaded" => "Sent";
    "pca.col.pcs" => "PCs";
    "pca.col.size" => "Size";
    "pca.col.maintenance" => "Maintenance";
    "pca.col.expires" => "Valid until";
    "pca.col.when" => "When";
    "pca.col.what" => "What";
    "pca.col.files" => "Files";
    "pca.col.added" => "New data";
    "pca.st.running" => "Backing up";
    "pca.st.late" => "Late";
    "pca.st.critical" => "Much too late";
    "pca.st.failed" => "Failed";
    "pca.st.revoked" => "Revoked";
    "pca.pairings" => "Codes waiting for a PC";
    "pca.pairing_cancel" => "Cancel the code";
    "pca.groups" => "Backup groups";
    "pca.groups_note" => "Each group is one repository. Retention, prune and check run here on the NAS every night, within {budget} of memory.";
    "pca.maint.never" => "Not maintained yet";
    "pca.maint.ok" => "Maintained {when}";
    "pca.maint.failed" => "Maintenance failed {when}";
    "pca.maint.estimate" => "Needs about {mem} of memory";
    "pca.maint.run" => "Maintain now";
    "pca.maint.done" => "Group {group} maintained";
    "pca.kit" => "Recovery kit";
    "pca.kit_once" => "The password is shown only now. Print the kit or keep it in a password manager: with it and a copy of the folder, stock restic opens the backups on any machine.";
    "pca.kit_text" => "A new kit replaces the previous one: the old password stops working.";
    "pca.kit.repo" => "Repository";
    "pca.kit.password" => "Password";
    "pca.kit.print" => "Print";
    "pca.group_delete" => "Delete the group";
    "pca.group_delete_title" => "Delete group {group}?";
    "pca.group_delete_text" => "The repository and every version in it are deleted from the NAS. This cannot be undone.";
    "pca.group_deleted" => "Group {group} deleted";
    "pca.own_group" => "Own group";
    "pca.join_group" => "Join a group";
    "pca.own_help" => "The PC gets a repository of its own: no other PC can read its data.";
    "pca.join_help" => "Identical data of the PCs of one group is stored once.";
    "pca.join_warning" => "PCs of one group can read each other's data. Join only PCs of the same person, or machines with the same content.";
    "pca.join_memory" => "With one more PC the group's maintenance may need about {mem}, over the {budget} it may use: prune and check would wait. Raise the memory share in the settings, or give the PC its own group.";
    "pca.no_groups" => "No group yet: the first PC gets its own.";
    "pca.f.quota" => "Group quota (GiB)";
    "pca.f.quota_help" => "0: no limit. A PC over the quota cannot send more until versions are pruned.";
    "pca.make_code" => "Make the code";
    "pca.code" => "Pairing code";
    "pca.code_valid" => "The code works once, until {until}.";
    "pca.fingerprint" => "Certificate of this NAS";
    "pca.step.install" => "1. Install rynas-agent on the PC";
    "pca.step.pair" => "2. Pair it";
    "pca.step.note" => "The command carries the certificate's fingerprint: the agent pairs only with this NAS. Without it the agent shows the fingerprint to compare with the one above. Then start the service: systemctl enable --now rynas-agent.service. Packages for Arch, Debian and Fedora are in the releases.";
    "pca.f.folder" => "Folder of the repositories";
    "pca.f.folder_help" => "Inside a volume. Existing groups stay where they are.";
    "pca.f.default_quota" => "Quota of new groups (GiB)";
    "pca.f.hour" => "Maintenance hour";
    "pca.f.hour_help" => "0–23, every night.";
    "pca.f.memory" => "Memory share (%)";
    "pca.f.memory_help" => "Of the NAS' memory, for one maintenance. A prune that needs more waits.";
    "pca.f.chunking" => "Block size of new groups";
    "pca.f.chunking_help" => "1 MiB deduplicates a little better; 2 MiB needs about half the memory for maintenance. A group keeps the size it was created with.";
    "pca.chunk.auto" => "Automatic: 1 MiB with 8 GiB of memory or more, otherwise 2 MiB";
    "pca.chunk.small" => "1 MiB (better deduplication)";
    "pca.chunk.large" => "2 MiB (less memory)";
    "pca.chunk_line" => "Blocks of {mib} MiB";
    "pca.last_ok" => "Last good version";
    "pca.last_seen" => "Last contact";
    "pca.os" => "System";
    "pca.h.restore" => "Restore";
    "pca.ready.yes" => "Ready";
    "pca.ready.no_version" => "No version yet";
    "pca.ready.no_manifest" => "No disk layout received yet";
    "pca.ready.stale" => "The disk layout is older than the last version";
    "pca.ready.skipped" => "A mounted filesystem is not in the backup";
    "pca.ready.loader" => "The boot loader is unknown";
    "pca.ready.no_table" => "No partition table was read";
    "pca.ready.no_kit" => "The group has no recovery kit";
    "pca.ready.manual" => "LVM or md: recreated by hand before restoring";
    "pca.last_error" => "Last error";
    "pca.changed" => "Files changed while read";
    "pca.changed_n" => "{n} files";
    "pca.changed_note" => "These files were written while the backup read them: their copy may mix old and new content. Freezing the filesystem, a snapshot (btrfs, LVM) or a pre-backup dump avoids it.";
    "pca.how_read" => "How each filesystem was read";
    "pca.m.btrfs" => "btrfs snapshot";
    "pca.m.lvm" => "LVM snapshot";
    "pca.m.frozen" => "frozen";
    "pca.m.thawed" => "frozen, then read in use";
    "pca.m.hot" => "read in use";
    "pca.m.files" => "files";
    "pca.m.skipped" => "left out";
    "pca.cancel" => "Cancel";
    "pca.backup_sent" => "The PC starts at its next contact";
    "pca.cancel_sent" => "The PC stops at its next contact";
    "pca.restore_code" => "Restore code";
    "pca.restore_code_valid" => "Restore code made: single use, read-only, valid until {until}. It replaces the codes made earlier for this PC: they no longer work.";
    "pca.restore_not_ready" => "Not everything a restore on an empty disk needs is on the NAS yet (see the notes above): the restore may stop halfway or leave the PC unable to boot.";
    "pca.restore_nas" => "NAS address";
    "pca.restore_nas_local" => "This page is open on {host}, which the PC cannot reach: after --nas put the NAS' address on the network, for example https://192.0.2.10:8443.";
    "pca.restore_nas_note" => "The command uses the NAS' own HTTPS port and carries its certificate's fingerprint, so the agent connects only to this NAS without asking. If the PC reaches the NAS by another address, change it after --nas.";
    "pca.restore_nas_note_nofp" => "The command uses the NAS' own HTTPS port; the agent shows the certificate's fingerprint to confirm. If the PC reaches the NAS by another address, change it after --nas.";
    "pca.rstep.download" => "1. Download the agent";
    "pca.rstep.run" => "2. Start the restore";
    "pca.copy_download" => "Copy the download command";
    "pca.copy_restore" => "Copy the restore command";
    "pca.restore_code_shared" => "This PC shares its group with other PCs: the restore can read the files of every PC in the group while it runs.";
    "pca.h.ok" => "Completed";
    "pca.zip_too_long" => "Too many items selected for one zip: select fewer, or download their folder.";
    "pca.restore_steps" => "Boot any live Linux on the PC (in the same UEFI or BIOS mode), then run:";
    "pca.revoke" => "Revoke";
    "pca.revoke_title" => "Revoke {device}?";
    "pca.revoke_text" => "Its token stops working at once and its key file is removed. Its versions stay readable in the group until the group is deleted.";
    "pca.revoked" => "{device} revoked";
    "pca.revoked_note" => "This PC is revoked: it can send nothing; its versions can still be browsed and restored.";
    "pca.versions" => "Versions";
    "pca.no_versions" => "No version yet.";
    "pca.history" => "History";
    "pca.h.backup" => "Backup";
    "pca.f.schedule" => "Schedule";
    "pca.f.state" => "Backups";
    "pca.f.scheduled" => "On schedule";
    "pca.f.paused" => "Paused";
    "pca.f.interval" => "Every (hours)";
    "pca.f.interval_help" => "1–168.";
    "pca.f.delay" => "After start-up (minutes)";
    "pca.f.delay_help" => "A due backup waits this long after the PC starts.";
    "pca.f.consistency" => "Filesystems without snapshots";
    "pca.f.read" => "ext and XFS without LVM";
    "pca.f.hot" => "Read in use";
    "pca.f.freeze" => "Freeze";
    "pca.f.hot_help" => "The PC keeps working; files written meanwhile are listed with the version.";
    "pca.f.freeze_help" => "Every file is consistent, but programs that write wait until the end of the backup (at most the time below).";
    "pca.f.freeze_max" => "Longest freeze (seconds)";
    "pca.f.freeze_max_help" => "Then the filesystem thaws and the rest is read in use.";
    "pca.f.excludes" => "Excludes";
    "pca.f.excludes_help" => "One pattern per line, on top of caches, temporary files and swap files.";
    "pca.f.retention" => "Versions kept";
    "pca.f.last" => "Last";
    "pca.f.daily" => "Daily";
    "pca.f.weekly" => "Weekly";
    "pca.f.monthly" => "Monthly";
    "pca.f.yearly" => "Yearly";
    "pca.f.min_age" => "Never delete before (days)";
    "pca.f.min_age_help" => "Protects against a wrong retention change.";
    "pca.f.alerts" => "Alerts";
    "pca.f.alert" => "Late after (hours)";
    "pca.f.alert_help" => "0: no alerts.";
    "pca.f.critical" => "Much too late after (hours)";
    "pca.download" => "Download";
    "pca.zip_folder" => "Download this folder (zip)";
    "pca.zip_selected" => "Download {n} selected (zip)";
    "pca.browse_note" => "Files come straight from the backup; a zip is made while it downloads.";
    "pca.browse_title" => "{device}: version {id}";
    "task.pcbackup.revoke" => "Revoke a PC";
    "task.pcbackup.maintenance" => "Maintain a PC backup group";
    "task.pcbackup.delete-group" => "Delete a PC backup group";
    "nev.pcbackup.agent_late.title" => "No backup from {device} for {hours} hours";
    "nev.pcbackup.agent_late.body" => "The last good version of {device} is older than {hours} hours.";
    "nev.pcbackup.agent_critical.title" => "No backup from {device} for {hours} hours";
    "nev.pcbackup.agent_critical.body" => "{device} has had no good version for {hours} hours.";
    "nev.pcbackup.agent_resumed.title" => "Backups from {device} arrive again";
    "nev.pcbackup.agent_resumed.body" => "{device} has a new good version.";
    "nev.pcbackup.agent_failed.title" => "The backup of {device} failed";
    "nev.pcbackup.agent_failed.body" => "See the PC on the Backup page.";
    // formats: unit words and joining templates (the File app has the same texts, test
    // `files_formats_match_core`), and the time format preference
    "dur.dhm" => "{d} d {h} h {m} min";
    "dur.hm" => "{h} h {m} min";
    "dur.ms" => "{m} min {s} s";
    "dur.d" => "{d} d";
    "dur.h" => "{h} h";
    "dur.m" => "{m} min";
    "dur.s" => "{s} s";
    "fmt.ago" => "{time} ago";
    "fmt.of" => "{part} of {total}";
    "hour_cycle" => "Time format";
    "hour_cycle.auto" => "Automatic";
    "hour_cycle.h23" => "24-hour";
    "hour_cycle.h12" => "12-hour";
}

// ---------------------------------------------------------------- formatting
//
// One locale for the whole interface (core, modules and the File app): the interface's language
// with the region of the browser's own tag when the languages match (`en-GB`, `en-US`, `it-CH`),
// else `en-US` / `it-IT`. Dates and times add the time format preference (`hourCycle`); numbers
// take their separators from `Intl.NumberFormat`, plural forms their category from
// `Intl.PluralRules`. Unit words and joining templates are dictionary keys (`dur.*`, `fmt.*`),
// the same texts the File app uses (test `files_formats_match_core`).

/// The time format preference: the locale's own, or forced 24 / 12 hours.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HourCycle {
    Auto,
    H23,
    H12,
}

impl HourCycle {
    /// The `hourCycle` of `Intl.DateTimeFormat` (`""` = the locale's own).
    pub fn code(self) -> &'static str {
        match self {
            HourCycle::Auto => "",
            HourCycle::H23 => "h23",
            HourCycle::H12 => "h12",
        }
    }

    pub fn from_code(s: &str) -> Self {
        match s {
            "h23" => HourCycle::H23,
            "h12" => HourCycle::H12,
            _ => HourCycle::Auto,
        }
    }
}

thread_local! {
    static HOUR: Cell<Option<RwSignal<HourCycle>>> = const { Cell::new(None) };
    /// The locale of the current language (see [`locale_for`]).
    static LOCALE: RefCell<String> = RefCell::new(String::from("en-US"));
    /// Separators and plural rules of the locale, made once per locale.
    static NUMBERS: RefCell<Option<std::rc::Rc<Numbers>>> = const { RefCell::new(None) };
}

fn hour_sig() -> RwSignal<HourCycle> {
    HOUR.with(|c| match c.get() {
        Some(s) => s,
        None => {
            let s = RwSignal::new(HourCycle::Auto);
            c.set(Some(s));
            s
        }
    })
}

/// The time format preference: tracked inside reactive closures, like [`lang`].
pub fn hour_cycle() -> HourCycle {
    if leptos::reactive::graph::Observer::get().is_some() { hour_sig().get() } else { hour_sig().get_untracked() }
}

pub fn set_hour_cycle(h: HourCycle) {
    storage_set("rynas.hour", h.code());
    hour_sig().set(h);
    publish(sig().get_untracked());
}

fn default_locale(lang: Lang) -> &'static str {
    &lang.info().locale
}

/// The locale for `lang`: the first of the browser's tags (most preferred first) of the same
/// language that names a region or script (`en-GB`), else the language's default.
fn locale_for(lang: Lang, tags: &[String]) -> String {
    tags.iter()
        .map(|t| t.trim())
        .find(|t| {
            t.len() <= 35
                && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && t.split_once('-').is_some_and(|(l, rest)| l.eq_ignore_ascii_case(&primary(lang.code())) && rest.split('-').all(valid_subtag))
        })
        .map(str::to_string)
        .unwrap_or_else(|| default_locale(lang).to_string())
}

/// A BCP 47 subtag `Intl` accepts after the language: region (`GB`, `419`), script
/// (`Hans`) or variant (5–8 characters). `en-x` or `en-12` would make `Intl` throw.
fn valid_subtag(s: &str) -> bool {
    let alpha = s.chars().all(|c| c.is_ascii_alphabetic());
    let digits = s.chars().all(|c| c.is_ascii_digit());
    match s.len() {
        2 | 4 => alpha,
        3 => digits,
        5..=8 => s.chars().all(|c| c.is_ascii_alphanumeric()),
        _ => false,
    }
}

/// The browser's language tags, most preferred first.
fn browser_tags() -> Vec<String> {
    let nav = window().navigator();
    let mut tags: Vec<String> = nav.languages().iter().filter_map(|v| v.as_string()).collect();
    tags.extend(nav.language());
    tags
}

/// The interface's locale (`en-GB`, `it-IT`…), tracked like [`lang`].
pub fn locale() -> String {
    let lang = lang();
    if cfg!(test) {
        return default_locale(lang).to_string();
    }
    LOCALE.with(|l| l.borrow().clone())
}

/// `Date.toLocaleString` of `ms` in the interface's locale with `opts` (`Intl.DateTimeFormat`
/// options), plus the time format preference when a time is shown. For pages with their own
/// options (time zone of the NAS, full date…).
pub fn date_with(ms: f64, opts: &[(&str, &str)]) -> String {
    let o = js_sys::Object::new();
    for (k, v) in opts {
        let _ = js_sys::Reflect::set(&o, &(*k).into(), &(*v).into());
    }
    let h = hour_cycle();
    if h != HourCycle::Auto && opts.iter().any(|(k, _)| matches!(*k, "timeStyle" | "hour")) {
        let _ = js_sys::Reflect::set(&o, &"hourCycle".into(), &h.code().into());
    }
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms)).to_locale_string(&locale(), &o).as_string().unwrap_or_default()
}

/// Date and time, e.g. "Nov 5, 2026, 12:54 AM" / "5 nov 2026, 00:54".
pub fn datetime(ms: u64) -> String {
    date_with(ms as f64, &[("dateStyle", "medium"), ("timeStyle", "short")])
}

/// Date, e.g. "Nov 5, 2026" / "5 nov 2026".
pub fn date(ms: u64) -> String {
    date_with(ms as f64, &[("dateStyle", "medium")])
}

/// Time of day for today's moments, date and time otherwise (lists of events).
pub fn short_when(ms: u64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let now = js_sys::Date::new_0();
    if d.to_date_string() == now.to_date_string() { time_hm(ms as f64) } else { datetime(ms) }
}

/// Time of day of a moment: "2:05 PM" / "14:05", as the locale and the preference want.
pub fn time_hm(ms: f64) -> String {
    date_with(ms, &[("timeStyle", "short")])
}

/// Milliseconds since the epoch of a date and time read as UTC (proleptic Gregorian).
fn civil_ms(y: i64, mo: u32, d: u32, h: u32, mi: u32) -> f64 {
    // days from civil (H. Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((mo + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400_000 + i64::from(h * 60 + mi) * 60_000) as f64
}

/// A wall-clock time with no date or zone (a stored "07:30") as the interface writes times:
/// "7:30 AM" / "07:30".
pub fn hm(h: u32, m: u32) -> String {
    if cfg!(not(target_arch = "wasm32")) {
        // off the browser (tests of the pages): the stored form
        return format!("{:02}:{:02}", h % 24, m % 60);
    }
    date_with(civil_ms(2000, 1, 1, h % 24, m % 60), &[("timeZone", "UTC"), ("timeStyle", "short")])
}

/// [`hm`] of a stored `"HH:MM"` (seconds ignored); anything else comes back as it is.
pub fn hm_text(s: &str) -> String {
    match parse_hm(s) {
        Some((h, m)) => hm(h, m),
        None => s.to_string(),
    }
}

fn parse_hm(s: &str) -> Option<(u32, u32)> {
    let mut p = s.trim().split(':');
    let h: u32 = p.next()?.parse().ok()?;
    let m: u32 = p.next().filter(|m| m.len() == 2)?.parse().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

/// `"YYYY-MM-DD HH:MM[:SS]"` (also with `T`) as (year, month, day, hour, minute).
fn parse_wall(s: &str) -> Option<(i64, u32, u32, u32, u32)> {
    let (date, time) = s.trim().split_once([' ', 'T'])?;
    let mut d = date.split('-');
    let (y, mo, day): (i64, u32, u32) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    if d.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&day) {
        return None;
    }
    let (h, m) = parse_hm(time.get(..5)?)?;
    Some((y, mo, day, h, m))
}

/// A local wall-clock moment written by the server without a zone ("2026-11-04 08:02:51") as
/// [`short_when`] writes moments: the time today, date and time otherwise. Shown as written
/// (no zone conversion); text in any other shape comes back as it is.
pub fn wall_when(s: &str) -> String {
    let Some((y, mo, d, h, m)) = parse_wall(s) else { return s.to_string() };
    let now = js_sys::Date::new_0();
    if (i64::from(now.get_full_year()), now.get_month() + 1, now.get_date()) == (y, mo, d) {
        hm(h, m)
    } else {
        date_with(civil_ms(y, mo, d, h, m), &[("timeZone", "UTC"), ("dateStyle", "medium"), ("timeStyle", "short")])
    }
}

/// The decimal and grouping separators, the percent pattern and the plural rules of a locale.
struct Numbers {
    locale: String,
    decimal: String,
    group: String,
    /// Digits a number needs before it is grouped: 4 (`1,000`) or 5 (Italian `1000`, `12.345`).
    group_from: usize,
    /// `{}%`, `{} %`, `%{}`…
    percent: String,
    #[cfg(target_arch = "wasm32")]
    plural: js_sys::Intl::PluralRules,
}

impl Numbers {
    #[cfg(target_arch = "wasm32")]
    fn new(locale: String) -> Numbers {
        let locales = js_sys::Array::of1(&locale.as_str().into());
        let nf = js_sys::Intl::NumberFormat::new(&locales, &js_sys::Object::new());
        let (mut decimal, mut group) = (String::from("."), String::from(","));
        for part in nf.format_to_parts(1_234_567.5).iter() {
            let get = |k: &str| js_sys::Reflect::get(&part, &k.into()).ok().and_then(|v| v.as_string()).unwrap_or_default();
            match get("type").as_str() {
                "decimal" => decimal = get("value"),
                "group" => group = get("value"),
                _ => {}
            }
        }
        let fmt = |f: &js_sys::Intl::NumberFormat, v: f64| f.format().call1(f, &wasm_bindgen::JsValue::from_f64(v)).ok().and_then(|s| s.as_string()).unwrap_or_default();
        let group_from = if fmt(&nf, 1000.0).chars().count() > 4 { 4 } else { 5 };
        let opts = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&opts, &"style".into(), &"percent".into());
        let percent = fmt(&js_sys::Intl::NumberFormat::new(&locales, &opts), 0.5).replacen("50", "{}", 1);
        let percent = if percent.contains("{}") { percent } else { "{}%".into() };
        let plural = js_sys::Intl::PluralRules::new(&locales, &js_sys::Object::new());
        Numbers { locale, decimal, group, group_from, percent, plural }
    }

    /// Off the browser (tests): a stand-in for `Intl.NumberFormat` with the separators of the
    /// languages the tests use (English for the others).
    #[cfg(not(target_arch = "wasm32"))]
    fn new(locale: String) -> Numbers {
        const SEPARATORS: &[(&str, &str, &str, usize)] = &[("it", ",", ".", 5), ("pl", ",", "\u{a0}", 5)];
        let p = primary(&locale);
        let (decimal, group, group_from) = SEPARATORS.iter().find(|s| s.0 == p).map_or((".", ",", 4), |s| (s.1, s.2, s.3));
        Numbers { locale, decimal: decimal.into(), group: group.into(), group_from, percent: "{}%".into() }
    }

    /// The CLDR plural category of `n`: "one", "few", "many", "other"…
    #[cfg(target_arch = "wasm32")]
    fn category(&self, n: u64) -> String {
        self.plural.select(n as f64).as_string().unwrap_or_else(|| "other".into())
    }

    /// Off the browser (tests): a stand-in for `Intl.PluralRules` with the CLDR rules of whole
    /// numbers of the languages the tests use (Polish for the made-up third language).
    #[cfg(not(target_arch = "wasm32"))]
    fn category(&self, n: u64) -> String {
        let cat = match primary(&self.locale).as_str() {
            "pl" if n == 1 => "one",
            "pl" if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) => "few",
            "pl" => "many",
            _ if n == 1 => "one",
            _ => "other",
        };
        cat.into()
    }
}

fn numbers() -> std::rc::Rc<Numbers> {
    let loc = locale();
    NUMBERS.with(|n| {
        let mut n = n.borrow_mut();
        match n.as_ref() {
            Some(x) if x.locale == loc => x.clone(),
            _ => {
                let x = std::rc::Rc::new(Numbers::new(loc));
                *n = Some(x.clone());
                x
            }
        }
    })
}

/// The locale's decimal separator ("." / ",").
pub fn decimal_sep() -> String {
    numbers().decimal.clone()
}

/// Fixed decimals with the locale's decimal separator.
pub fn num(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    let n = numbers();
    if n.decimal == "." { s } else { s.replace('.', &n.decimal) }
}

/// A whole count with the locale's grouping, as `Intl.NumberFormat` writes it: 1,234,567 /
/// 1.234.567 (Italian groups from five digits: 1000, 12.345).
pub fn count(n: u64) -> String {
    let x = numbers();
    if n.to_string().len() < x.group_from { n.to_string() } else { group(n, &x.group) }
}

fn group(n: u64, mark: &str) -> String {
    let d = n.to_string();
    let mut out = String::with_capacity(d.len() + d.len() / 3 * mark.len());
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.push_str(mark);
        }
        out.push(c);
    }
    out
}

/// The text of `key` for the count `n`: `key.<category>` for the CLDR plural category of `n` in
/// the interface's locale (`key.one`, `key.few`, `key.many`…), else `key.other` (required for
/// every key with forms), else `key` itself. `{n}` becomes the grouped count, then `args` as in
/// [`tf`].
pub fn tn(key: &str, n: u64, args: &[(&str, &str)]) -> String {
    let cat = numbers().category(n);
    let text = try_t(&format!("{key}.{cat}")).or_else(|| try_t(&format!("{key}.other"))).or_else(|| try_t(key));
    let mut s = text.map_or_else(|| key.to_string(), str::to_string).replace("{n}", &count(n));
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// Three significant digits: 1.23, 12.3, 123 (decided on the rounded value: 9.996 is "10.0").
fn sig3(v: f64) -> String {
    let d = if v < 9.995 {
        2
    } else if v < 99.95 {
        1
    } else {
        0
    };
    num(v, d)
}

/// `n` bytes in `units` of `base` with three significant digits: the next unit as soon as the
/// number would need four digits ("1.00 GiB", never "1022 MiB").
fn scaled(n: u64, base: f64, units: &[&str]) -> String {
    if n < 1000 {
        return format!("{n} {}", units[0]);
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 999.5 && i < units.len() - 1 {
        v /= base;
        i += 1;
    }
    format!("{} {}", sig3(v), units[i])
}

/// Binary sizes: 1536 -> "1.50 KiB" (the File app's sizes follow the same rule).
pub fn bytes(n: u64) -> String {
    scaled(n, 1024.0, &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"])
}

/// Disk sizes the way vendors print them (decimal): 4000787030016 -> "4.00 TB".
pub fn bytes_si(n: u64) -> String {
    scaled(n, 1000.0, &["B", "kB", "MB", "GB", "TB", "PB", "EB"])
}

/// "X of Y" / "X di Y": a part of a whole, for every ratio the interface writes.
pub fn of(part: &str, total: &str) -> String {
    tf("fmt.of", &[("part", part), ("total", total)])
}

/// [`of`] for sizes: "1.50 GiB of 4.00 GiB".
pub fn bytes_of(part: u64, total: u64) -> String {
    of(&bytes(part), &bytes(total))
}

/// Byte rate: "12.3 MiB/s".
pub fn rate(bps: u64) -> String {
    format!("{}/s", bytes(bps))
}

/// Network rate in bits: "940 Mbit/s".
pub fn bitrate(bytes_per_s: u64) -> String {
    const U: [&str; 5] = ["bit/s", "kbit/s", "Mbit/s", "Gbit/s", "Tbit/s"];
    let mut v = bytes_per_s as f64 * 8.0;
    let mut i = 0;
    // 999.7 kbit/s would print as "1000 kbit/s": step up at what rounds to 1000, like `scaled`.
    let step = |i: usize| if i == 0 { 1000.0 } else { 999.5 };
    while v >= step(i) && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", v as u64, U[0])
    } else {
        format!("{} {}", sig3(v), U[i])
    }
}

/// Link speed of a port from Mbit/s: "1 Gbit", "2.5 Gbit", "100 Mbit".
pub fn link_speed(m: u32) -> String {
    if m >= 1000 { format!("{} Gbit", num(m as f64 / 1000.0, if m.is_multiple_of(1000) { 0 } else { 1 })) } else { format!("{m} Mbit") }
}

/// A percentage of 0–100: one decimal below 10 ("4.5%", "45%").
pub fn pct(v: f64) -> String {
    numbers().percent.replacen("{}", &num(v, if v < 10.0 { 1 } else { 0 }), 1)
}

pub fn celsius(v: f32) -> String {
    format!("{}°C", num(v as f64, 0))
}

/// Uptime-style duration: "12 d 4 h 7 min" / "12 g 4 h 7 min", "4 h 7 min", "7 min", "42 s".
pub fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    let (d, h, m) = (count(d), h.to_string(), m.to_string());
    if secs >= 86400 {
        tf("dur.dhm", &[("d", &d), ("h", &h), ("m", &m)])
    } else if secs >= 3600 {
        tf("dur.hm", &[("h", &h), ("m", &m)])
    } else if secs >= 60 {
        tf("dur.m", &[("m", &m)])
    } else {
        tf("dur.s", &[("s", &secs.to_string())])
    }
}

/// Elapsed time with seconds (task durations): "45 s", "3 min 05 s", "1 h 02 min".
pub fn elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if h > 0 {
        tf("dur.hm", &[("h", &count(h)), ("m", &format!("{m:02}"))])
    } else if m > 0 {
        tf("dur.ms", &[("m", &m.to_string()), ("s", &format!("{s:02}"))])
    } else {
        tf("dur.s", &[("s", &s.to_string())])
    }
}

/// "5 min ago" / "5 min fa": [`approx`] of the time since a moment.
pub fn ago(secs: u64) -> String {
    tf("fmt.ago", &[("time", &approx(secs))])
}

/// The largest unit of a duration: "42 s", "5 min", "3 h", "2 d" (relative times).
pub fn approx(secs: u64) -> String {
    if secs >= 86400 {
        tf("dur.d", &[("d", &count(secs / 86400))])
    } else if secs >= 3600 {
        tf("dur.h", &[("h", &(secs / 3600).to_string())])
    } else if secs >= 60 {
        tf("dur.m", &[("m", &(secs / 60).to_string())])
    } else {
        tf("dur.s", &[("s", &secs.to_string())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Italian, the shipped translation the tests read.
    fn it() -> Lang {
        Lang::from_code("it").expect("Italian in the language table")
    }

    /// Polish, the made-up third language of `tests/fixtures/i18n` (four plural forms).
    fn pl() -> Lang {
        Lang::from_code("pl").expect("the fixture language")
    }

    #[test]
    fn elapsed_and_duration() {
        sig().set(Lang::EN);
        assert_eq!(elapsed(7), "7 s");
        assert_eq!(elapsed(185), "3 min 05 s");
        assert_eq!(elapsed(3720), "1 h 02 min");
        assert_eq!(duration(42), "42 s");
        assert_eq!(duration(3 * 86400 + 4 * 3600 + 7 * 60), "3 d 4 h 7 min");
        assert_eq!(ago(300), "5 min ago");
        assert_eq!(ago(2 * 3600 + 59), "2 h ago");
        assert_eq!(approx(3 * 86400), "3 d");
        sig().set(it());
        assert_eq!(duration(3 * 86400 + 4 * 3600 + 7 * 60), "3 g 4 h 7 min");
        assert_eq!(ago(300), "5 min fa");
        assert_eq!(of("1", "3"), "1 di 3");
        sig().set(Lang::EN);
    }

    #[test]
    fn browser_language() {
        let tags = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(lang_for_tag("it-IT"), Some(it()));
        assert_eq!(lang_for_tag("IT"), Some(it()));
        assert_eq!(lang_for_tag("en-US"), Some(Lang::EN));
        assert_eq!(lang_for_tag("de"), None);
        assert_eq!(lang_for_tag(""), None);
        // the first of the browser's languages the table has
        assert_eq!(lang_for_tags(&tags(&["de-DE", "it-CH", "en"])), it());
        assert_eq!(lang_for_tags(&tags(&["de-DE", "fr"])), Lang::EN);
        assert_eq!(lang_for_tags(&tags(&[])), Lang::EN);
        // stored preferences: the codes of the table, anything else is "automatic"
        assert_eq!(Lang::from_code("en"), Some(Lang::EN));
        assert_eq!(Lang::from_code("it"), Some(it()));
        assert_eq!(Lang::from_code("xx"), None);
        assert_eq!(Lang::from_code(""), None);
        assert_eq!(it().name(), "Italiano");
        assert_eq!(it().english(), "Italian");
        assert_eq!(Lang::all().next(), Some(Lang::EN));
    }

    /// The shipped table: English first, each line complete, codes unique, `other` always a
    /// plural form.
    #[test]
    fn language_table_is_valid() {
        let l = languages();
        assert_eq!(l[0].code, "en");
        let lines = LANGUAGES.lines().filter(|x| !x.starts_with('#') && !x.trim().is_empty()).count();
        assert_eq!(l.len(), lines, "a line of languages.txt does not parse");
        let mut codes: Vec<&str> = l.iter().map(|x| x.code.as_str()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), l.len(), "duplicate language codes");
        for x in &l {
            assert!(x.code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && x.code.len() <= 12, "{}: not a language tag", x.code);
            assert!(x.locale.split('-').next() == Some(primary(&x.code).as_str()), "{}: locale {} of another language", x.code, x.locale);
            assert!(x.plurals.iter().all(|p| crate::i18n_check::CATEGORIES.contains(&p.as_str())), "{}: plural categories {:?}", x.code, x.plurals);
        }
        // the test build's table adds the fixture's made-up language after the shipped ones
        assert!(Lang::all().count() > l.len());
    }

    /// A third language needs only its files and its line in the table: the made-up Polish of
    /// the fixtures loads, falls back to English where it has no text and picks its four
    /// plural forms, with its own separators and locale.
    #[test]
    fn any_language_of_the_table_works() {
        let pl = pl();
        assert_eq!(pl.code(), "pl");
        assert_eq!(locale_for(pl, &[]), "pl-PL");
        assert_eq!(lookup("nav.files", pl), Some("Pliki"));
        assert_eq!(lookup("nav.storage", pl), Some("Storage"), "English where the file has no text");
        sig().set(pl);
        assert_eq!(tn("rm.count", 1, &[]), "1 dysk");
        assert_eq!(tn("rm.count", 3, &[]), "3 dyski");
        assert_eq!(tn("rm.count", 5, &[]), "5 dysków");
        assert_eq!(tn("rm.count", 12, &[]), "12 dysków");
        assert_eq!(tn("rm.count", 22, &[]), "22 dyski");
        assert_eq!(tn("rm.count", 12_345, &[]), "12\u{a0}345 dysków");
        assert_eq!(t("dur.d"), "{d} d");
        sig().set(Lang::EN);
        // a file read the way the interface reads `/i18n/<code>.txt`: comments and unknown keys skipped
        let n = INDEX.with(|idx| idx.install(pl, "# comment\nnav.files\tPliki\nno.such.key\tx\nno tab here\n"));
        assert_eq!(n, 1);
    }

    #[test]
    fn locale_from_language_and_browser() {
        let tags = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(locale_for(Lang::EN, &tags(&["en-GB", "en"])), "en-GB");
        assert_eq!(locale_for(Lang::EN, &tags(&["it-IT", "en-AU"])), "en-AU");
        assert_eq!(locale_for(Lang::EN, &tags(&["en"])), "en-US");
        assert_eq!(locale_for(Lang::EN, &tags(&["it-IT", "it"])), "en-US");
        assert_eq!(locale_for(it(), &tags(&["it-CH"])), "it-CH");
        assert_eq!(locale_for(it(), &tags(&["en-GB"])), "it-IT");
        assert_eq!(locale_for(it(), &tags(&[])), "it-IT");
        // never a malformed tag
        assert_eq!(locale_for(Lang::EN, &tags(&["en-", "en GB", "en-\"x"])), "en-US");
        assert_eq!(locale_for(Lang::EN, &tags(&["en-x", "en-1", "en-abc", "en-12", "en-GB-"])), "en-US");
        assert_eq!(locale_for(Lang::EN, &tags(&["en-419"])), "en-419");
        assert_eq!(locale_for(Lang::EN, &tags(&["en-Latn-GB"])), "en-Latn-GB");
        assert_eq!(HourCycle::from_code("h12"), HourCycle::H12);
        assert_eq!(HourCycle::from_code(""), HourCycle::Auto);
        assert_eq!(HourCycle::from_code(HourCycle::H23.code()), HourCycle::H23);
    }

    #[test]
    fn wall_times_parse() {
        assert_eq!(parse_wall("2026-11-04 08:02:51"), Some((2026, 11, 4, 8, 2)));
        assert_eq!(parse_wall("2026-11-04T17:05"), Some((2026, 11, 4, 17, 5)));
        assert_eq!(parse_wall("2026-13-04 08:02"), None);
        assert_eq!(parse_wall("yesterday"), None);
        assert_eq!(parse_hm("07:30"), Some((7, 30)));
        assert_eq!(parse_hm("7:30:00"), Some((7, 30)));
        assert_eq!(parse_hm("24:00"), None);
        assert_eq!(civil_ms(2000, 1, 1, 0, 0), 946_684_800_000.0);
        assert_eq!(civil_ms(2026, 11, 13, 17, 5), 1_794_589_500_000.0);
    }

    #[test]
    fn sizes_have_three_significant_digits() {
        sig().set(Lang::EN);
        assert_eq!(bytes(999), "999 B");
        assert_eq!(bytes(1536), "1.50 KiB");
        assert_eq!(bytes(1022 * 1024 * 1024), "1.00 GiB");
        assert_eq!(bytes(5_000_000_000), "4.66 GiB");
        assert_eq!(bytes(100 * 1024 * 1024), "100 MiB");
        assert_eq!(bytes((9.996 * 1024.0) as u64), "10.0 KiB");
        assert_eq!(bytes_si(4_000_787_030_016), "4.00 TB");
        assert_eq!(bytes_si(1500), "1.50 kB");
        assert_eq!(bytes_of(1536, 4096), "1.50 KiB of 4.00 KiB");
        sig().set(it());
        assert_eq!(bytes(1536), "1,50 KiB");
        assert_eq!(count(1_234_567), "1.234.567");
        assert_eq!(count(1000), "1000");
        assert_eq!(count(12_345), "12.345");
        assert_eq!(pct(4.5), "4,5%");
        sig().set(Lang::EN);
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(count(1000), "1,000");
        assert_eq!(pct(45.2), "45%");
    }

    #[test]
    fn plural_forms() {
        sig().set(Lang::EN);
        assert_eq!(tn("rm.count", 1, &[]), "1 drive");
        assert_eq!(tn("rm.count", 1200, &[]), "1,200 drives");
        assert_eq!(tn("pending.changes", 1, &[]), "pending change");
        assert_eq!(tn("pending.changes", 0, &[]), "pending changes");
        sig().set(it());
        assert_eq!(tn("repl.sent", 1, &[]), "1 snapshot inviato");
        assert_eq!(tn("repl.sent", 2, &[]), "2 snapshot inviati");
        sig().set(Lang::EN);
        // the base key when a text has no forms, the key itself when there is none
        assert_eq!(tn("nav.files", 3, &[]), "Files");
        assert_eq!(tn("no.such", 3, &[]), "no.such");
    }

    /// A text with plural forms has its `other` form (the fallback of every category), and no
    /// key with forms keeps a `_one` twin of the old scheme.
    #[test]
    fn plural_keys_have_other() {
        const CATS: &[&str] = &["zero", "one", "two", "few", "many"];
        let keys: std::collections::HashSet<&str> = entries().into_iter().map(|e| e.0).collect();
        let mut bad = Vec::new();
        for k in &keys {
            if let Some((base, cat)) = k.rsplit_once('.')
                && CATS.contains(&cat)
                && !keys.contains(format!("{base}.other").as_str())
            {
                bad.push(k.to_string());
            }
            if let Some(base) = k.strip_suffix(".other")
                && keys.contains(format!("{base}_one").as_str())
            {
                bad.push(format!("{base}_one"));
            }
        }
        assert!(bad.is_empty(), "plural forms without `other`: {bad:?}");
    }

    #[test]
    fn counts_are_grouped() {
        assert_eq!(group(0, ","), "0");
        assert_eq!(group(999, ","), "999");
        assert_eq!(group(1000, ","), "1,000");
        assert_eq!(group(1_234_567, "."), "1.234.567");
        assert_eq!(group(1_234_567, "\u{202f}"), "1\u{202f}234\u{202f}567");
    }

    #[test]
    fn english_is_the_fallback() {
        // entries without an Italian text use the English one
        assert_eq!(lookup("nav.dashboard", it()), Some("Dashboard"));
        assert_eq!(lookup("nav.files", Lang::EN), Some("Files"));
        assert_eq!(lookup("nav.files", it()), Some("File"));
        assert_eq!(lookup("no.such.key", Lang::EN), None);
    }

    #[test]
    fn no_duplicate_texts() {
        // keys whose text mirrors a server-side name (task types, configuration keys, plan
        // summary fields, server event texts by code) may repeat a UI text
        let exempt = |k: &str| k.starts_with("task.") || k.starts_with("cfg.key.") || k.starts_with("plan.k.") || k.starts_with("nev.");
        let mut seen = std::collections::HashMap::new();
        let mut dups = Vec::new();
        for (k, en) in entries() {
            if exempt(k) || !en.contains(' ') {
                continue;
            }
            if let Some(prev) = seen.insert(texts_of(k), k) {
                dups.push(format!("{prev} = {k}: {en}"));
            }
        }
        assert!(dups.is_empty(), "one key per text: {dups:#?}");
    }

    /// G8c: the UI describes what rynas does and never compares it with another NAS system.
    /// Platform names (Synology, synohwd, synodeb) are allowed only in a module's table
    /// (`crate::modules`): those texts are rendered only where that hardware is present (G8d,
    /// test `core_names_no_module`).
    #[test]
    fn no_other_products() {
        const PRODUCTS: &[&str] = &["dsm", "truenas", "unraid", "openmediavault", "omv", "qnap", "qts"];
        // app names of other systems, matched as phrases
        const APPS: &[&str] = &["storage manager", "task scheduler", "file station", "package center", "hyper backup", "control panel", "utilità di pianificazione"];
        // being reworded by the UPS work in progress: drop from here once it is
        const PENDING: &[&str] = &[];
        // the file format a schedule is imported from, named on the import button and its help
        // only (approved by the maintainer, 2026-11-08); never a comparison
        const FORMAT_SOURCE: &[&str] = &["syn.ledsch_import", "syn.ledsch_import_help"];
        let mut hits = Vec::new();
        for (k, _) in entries().into_iter().filter(|e| !PENDING.contains(&e.0) && !FORMAT_SOURCE.contains(&e.0)) {
            for text in texts_of(k) {
                let words = text.split(|c: char| !c.is_alphanumeric()).map(str::to_ascii_lowercase);
                if let Some(w) = words.into_iter().find(|w| PRODUCTS.contains(&w.as_str())) {
                    hits.push(format!("{k}: {w}: {text}"));
                }
                let low = text.to_lowercase();
                if let Some(a) = APPS.iter().find(|a| low.contains(*a)) {
                    hits.push(format!("{k}: {a}: {text}"));
                }
            }
        }
        assert!(hits.is_empty(), "texts naming another product: {hits:#?}");
    }

    /// D-MODULE: the core table names no hardware module's platform (those texts belong to
    /// the module's table), and the module's keys are not in the core table.
    #[test]
    fn core_names_no_module() {
        let core = parts().swap_remove(0).1;
        assert!(core.len() > 1000);
        let hits: Vec<_> = core
            .iter()
            .filter(|(k, _)| {
                k.starts_with("syn.") || texts_of(k).iter().any(|t| { let l = t.to_lowercase(); l.contains("synology") || l.contains("synodeb") || l.contains("synohwd") })
            })
            .map(|e| e.0)
            .collect();
        assert!(hits.is_empty(), "module texts in the core table: {hits:?}");
    }

    /// Every string literal of the sources that looks like a key (`area.thing`, also `#area.thing`
    /// for numeric table heads) exists in the dictionary, so a renamed key cannot leave a raw
    /// key on screen. Literals of the same shape that are not keys are listed in `not_a_key`.
    #[test]
    fn used_keys_exist() {
        fn is_key_shape(s: &str) -> bool {
            let mut parts = s.split('.');
            let first = parts.next().unwrap_or("");
            let rest: Vec<&str> = parts.collect();
            first.starts_with(|c: char| c.is_ascii_lowercase())
                && !rest.is_empty()
                && std::iter::once(first)
                    .chain(rest)
                    .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
        }
        // storage keys, event names, file system types, NUT instant commands, sample host
        // names, source file names
        let not_a_key = |s: &str| {
            s.starts_with("rynas.")
                || s.starts_with("test.battery.")
                || s.ends_with(".rs")
                || s.ends_with(".js")
                || s.ends_with(".css")
                || s.starts_with("fuse.")
                || s.contains("example")
                || matches!(s, "a.b" | "metrics.sample" | "hardware.status" | "task.started" | "task.updated" | "task.finished" | "removable.state" | "docker.stats" | "docker.changed")
        };
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).expect("src dir").flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("i18n.rs") && !p.ends_with("i18n_check.rs") && !p.ends_with("files_check.rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let keys: std::collections::HashSet<&str> = entries().into_iter().map(|e| e.0).collect();
        let mut missing = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).expect("source file");
            for (n, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                // odd pieces between quotes are the literals (no key contains a quote)
                for lit in line.split('"').skip(1).step_by(2) {
                    let k = lit.strip_prefix('#').unwrap_or(lit);
                    // configuration keys (`config::keys`) are named by their `cfg.key.*` label
                    let cfg_key = keys.contains(format!("cfg.key.{k}").as_str());
                    // a text with plural forms (`tn`) is named by its base key
                    let plural = keys.contains(format!("{k}.other").as_str());
                    if is_key_shape(k) && !not_a_key(k) && !keys.contains(k) && !cfg_key && !plural {
                        missing.push(format!("{}:{}: {k}", f.display(), n + 1));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "keys used but not in the dictionary: {missing:#?}");
    }

    /// Every code of the disk checks (notes, values) and of the SMART health reasons has a
    /// text: the codes are read from the rynas-storage source (the line after the opening
    /// parenthesis of a note tuple or a `reason(severity,` call, and `CheckValue::code/with`).
    #[test]
    fn every_disk_check_and_reason_code_has_a_text() {
        let keys: std::collections::HashSet<&str> = entries().into_iter().map(|e| e.0).collect();
        let snake = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        let lit = |s: &str| s.strip_prefix('"').and_then(|s| s.split('"').next()).map(str::to_string);
        let mut missing = Vec::new();
        let mut check = |key: String| {
            if !keys.contains(key.as_str()) {
                missing.push(key);
            }
        };
        let tuning = include_str!("../../rynas-storage/src/tuning.rs");
        let tuning = &tuning[..tuning.find("#[cfg(test)]").unwrap_or(tuning.len())];
        let (mut notes, mut values) = (0, 0);
        let mut prev = "";
        for line in tuning.lines().map(str::trim) {
            for pat in ["CheckValue::code(\"", "CheckValue::with(\""] {
                for part in line.split(pat).skip(1) {
                    let code = part.split('"').next().unwrap_or_default();
                    values += 1;
                    check(format!("sto.cv.{code}"));
                }
            }
            // a note: ("code", "text") on one line, or "code", on the line after "("
            let note = if let Some(rest) = line.strip_prefix("(\"").filter(|r| r.contains("\", \"")) {
                rest.split('"').next().map(str::to_string)
            } else if prev.ends_with('(') && !prev.ends_with("check(") && line.ends_with("\",") && !line.contains(' ') {
                lit(line)
            } else {
                line.strip_prefix("Some((\"").and_then(|r| r.split('"').next()).map(str::to_string)
            };
            if let Some(code) = note.filter(|c| snake(c)) {
                notes += 1;
                check(format!("sto.cn.{code}"));
            }
            prev = line;
        }
        assert!(notes > 15 && values > 10, "codes not found: {notes} notes, {values} values");
        let smart = include_str!("../../rynas-storage/src/smart.rs");
        let verdict = &smart[smart.find("pub fn verdict").expect("verdict")..];
        let verdict = &verdict[..verdict.find("\n}").expect("verdict end")];
        let mut reasons = 0;
        let mut prev = "";
        for line in verdict.lines().map(str::trim) {
            if matches!(prev, "Critical," | "Warning," | "Info," | "sev,")
                && let Some(code) = lit(line).filter(|c| snake(c))
            {
                reasons += 1;
                check(format!("sto.reason.{code}"));
            }
            prev = line;
        }
        assert!(reasons > 20, "reason codes not found: {reasons}");
        assert!(missing.is_empty(), "codes without a text: {missing:?}");
    }

    /// Every code of the storage planner (errors, warnings, step kinds) has a text: the
    /// variants are read from the API types source, so a new code without a key fails here.
    #[test]
    fn every_plan_code_has_a_text() {
        let src = include_str!("../../rynas-api-types/src/storage.rs");
        let keys: std::collections::HashSet<&str> = entries().into_iter().map(|e| e.0).collect();
        let mut missing = Vec::new();
        for (enum_name, prefix) in [("PlanIssueCode", "plan.e"), ("PlanWarningCode", "plan.w"), ("PlanStepKind", "plan.s")] {
            let start = src.find(&format!("pub enum {enum_name} {{")).expect(enum_name);
            let body = &src[start..start + src[start..].find("\n}").expect("enum end")];
            let mut n = 0;
            for line in body.lines().skip(1).map(str::trim) {
                let Some(variant) = line.strip_suffix(',') else { continue };
                if variant == "Other" || !variant.starts_with(|c: char| c.is_ascii_uppercase()) {
                    continue;
                }
                let mut snake = String::new();
                for (i, c) in variant.chars().enumerate() {
                    if c.is_ascii_uppercase() && i > 0 {
                        snake.push('_');
                    }
                    snake.push(c.to_ascii_lowercase());
                }
                n += 1;
                let key = format!("{prefix}.{snake}");
                if !keys.contains(key.as_str()) {
                    missing.push(key);
                }
            }
            assert!(n > 10, "{enum_name}: variants not found");
        }
        assert!(missing.is_empty(), "plan codes without a text: {missing:#?}");
    }

    /// The bundle the interface fetches (`/i18n/<code>.txt`, the parts back to back) gives
    /// every text of every part; no text has a tab or a newline.
    #[test]
    fn bundled_parts_read_back() {
        for (k, en) in entries() {
            assert!(!k.contains(['\t', '\n']) && !en.contains('\n'), "tab or newline in {k}");
        }
        for l in Lang::all().skip(1).filter(|l| languages().iter().any(|x| x.code == l.code())) {
            let bundle: &'static str = Box::leak(PARTS.iter().filter_map(|p| test_file(l.code(), p)).collect::<String>().into_boxed_str());
            // plural forms that English has no key for (Russian `few`/`many`, …) are taken too
            let extra = translation_lines(bundle)
                .filter(|(k, _)| matches!(k.rsplit_once('.').map(|(_, c)| c), Some("zero" | "two" | "few" | "many")))
                .count();
            let n = INDEX.with(|idx| idx.install(l, bundle));
            assert_eq!(n, entries().len() + extra, "{}: texts taken from the bundle", l.code());
        }
        assert_eq!(lookup("nav.files", it()), Some("File"));
    }

    #[test]
    fn keys_are_unique() {
        let mut k: Vec<&str> = entries().into_iter().map(|e| e.0).collect();
        k.sort_unstable();
        let n = k.len();
        k.dedup();
        assert_eq!(n, k.len(), "duplicate i18n keys");
    }

}
