//! Tiny i18n: English (default, source language and fallback) and Italian, plus
//! number/byte/duration formatting.
//!
//! The language lives in a signal, so `t()` called inside a reactive closure re-renders when the
//! user switches language. Preference: localStorage `rynas.lang` ("en", "it" or absent = browser:
//! Italian when the browser asks for it, English otherwise).
//!
//! Italian wording uses the common NAS vocabulary (archiviazione, cartella condivisa, volume,
//! integro/degradato…); no product or app names of other systems (test `no_other_products`).

use std::cell::Cell;

use leptos::prelude::*;

use crate::prefs::{storage_get, storage_set};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    It,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::It => "it",
        }
    }
}

thread_local! {
    static LANG: Cell<Option<RwSignal<Lang>>> = const { Cell::new(None) };
}

fn sig() -> RwSignal<Lang> {
    LANG.with(|c| match c.get() {
        Some(s) => s,
        None => {
            let s = RwSignal::new(Lang::En);
            c.set(Some(s));
            s
        }
    })
}

/// Italian when the browser's preferred language is Italian, English for everything else.
fn browser_lang() -> Lang {
    lang_for_tag(&window().navigator().language().unwrap_or_default())
}

fn lang_for_tag(tag: &str) -> Lang {
    if tag.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("it")) { Lang::It } else { Lang::En }
}

pub fn init() {
    let lang = match storage_get("rynas.lang").as_deref() {
        Some("en") => Lang::En,
        Some("it") => Lang::It,
        _ => browser_lang(),
    };
    sig().set(lang);
    apply_html_lang(lang);
}

fn apply_html_lang(lang: Lang) {
    if let Some(el) = document().document_element() {
        let _ = el.set_attribute("lang", lang.code());
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
    sig().set(l);
    apply_html_lang(l);
    if l == Lang::It {
        // English meanwhile; the texts switch when the file is there
        load_italian(italian_arrived);
    }
}

/// Stored preference: `None` means "automatic".
pub fn lang_pref() -> Option<Lang> {
    match storage_get("rynas.lang").as_deref() {
        Some("en") => Some(Lang::En),
        Some("it") => Some(Lang::It),
        _ => None,
    }
}

/// `"key" => "English", "Italiano";` — both texts are required (rule G8), even when they are
/// the same word, so a forgotten translation is a compile error.
///
/// Keys follow `<area>.<thing>` (`sto.scrub`, `net.dns`); words shared by every area are
/// bare (`cancel`, `save`, `edit_named`, `run_now`) or in a shared group (`col.*` column
/// headers, `health.*`, `pending.*`, `task.st.*`). One key per text: a test fails on two keys
/// with the same English and Italian text of more than one word.
///
/// The core table is below; a hardware module has its own table in its folder
/// (`crate::modules::<module>::i18n`, same macro, D-MODULE), read together with this one.
///
/// Only English is compiled into the interface: the Italian column (30 KiB brotli of wasm) is
/// the file `assets/i18n/it.txt` (served as `/i18n/it.txt`), generated from these tables by
/// the test `italian_file_is_current` and fetched when the language is Italian. The test
/// build keeps the Italian column in memory, so every test sees both languages.
macro_rules! dict {
    ($blob:ident, $lens:ident, $it:ident; $($k:literal => $en:literal, $itx:literal;)*) => {
        /// Every key and English text back to back with their byte lengths: much smaller in
        /// the wasm than a `match` over 3 000 strings or a table of string pointers.
        pub(crate) static $blob: &str = concat!($($k, $en),*);
        pub(crate) static $lens: &[u16] = &[$($k.len() as u16, $en.len() as u16),*];
        /// The Italian column (test build only; the interface fetches `/i18n/it.txt`).
        #[cfg(test)]
        pub(crate) static $it: &[&str] = &[$($itx),*];
    };
}
pub(crate) use dict;

/// The tables: the core one first, then the modules'.
fn tables() -> [(&'static str, &'static [u16]); 3] {
    [(BLOB, LENS), (crate::modules::synology::i18n::BLOB, crate::modules::synology::i18n::LENS), (crate::modules::qnap::i18n::BLOB, crate::modules::qnap::i18n::LENS)]
}

/// The Italian columns of [`tables`], in the same order (test build).
#[cfg(test)]
fn italian_tables() -> [&'static [&'static str]; 3] {
    [IT, crate::modules::synology::i18n::IT, crate::modules::qnap::i18n::IT]
}

/// Start of every text of every table (and the end), the entries (table, index) sorted by
/// key, built on first use, and the Italian texts once they are there.
struct Index {
    blobs: [&'static str; 3],
    starts: [Vec<u32>; 3],
    by_key: Vec<(u8, u16)>,
    italian: std::cell::RefCell<[Vec<&'static str>; 3]>,
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
        #[cfg(test)]
        let italian = italian_tables().map(<[&str]>::to_vec);
        #[cfg(not(test))]
        let italian = [Vec::new(), Vec::new(), Vec::new()];
        let mut idx = Index { blobs: tables.map(|t| t.0), starts, by_key, italian: std::cell::RefCell::new(italian) };
        let mut by_key = std::mem::take(&mut idx.by_key);
        by_key.sort_unstable_by_key(|&(t, e)| idx.text(t as usize, e as usize * 2));
        idx.by_key = by_key;
        idx
    };
    /// 0 not asked, 1 being fetched, 2 there.
    static ITALIAN: Cell<u8> = const { Cell::new(0) };
}

/// The text of `key`; Italian falls back to English while (or where) the Italian file has
/// no text for it.
pub(crate) fn lookup(key: &str, lang: Lang) -> Option<&'static str> {
    INDEX.with(|idx| {
        let (t, e) = idx.find(key)?;
        if lang == Lang::It
            && let Some(s) = idx.italian.borrow()[t].get(e).copied().filter(|s| !s.is_empty())
        {
            return Some(s);
        }
        Some(idx.text(t, e * 2 + 1))
    })
}

/// Reads the Italian file (`key<TAB>text` lines); unknown keys are skipped, missing ones stay
/// English. Returns the number of texts taken.
fn install_italian(file: String) -> usize {
    let file: &'static str = Box::leak(file.into_boxed_str());
    INDEX.with(|idx| {
        let mut it = idx.italian.borrow_mut();
        for (t, v) in it.iter_mut().enumerate() {
            v.clear();
            v.resize((idx.starts[t].len() - 1) / 2, "");
        }
        let mut n = 0;
        for line in file.lines() {
            if let Some((k, text)) = line.split_once('\t')
                && let Some((t, e)) = idx.find(k)
            {
                it[t][e] = text;
                n += 1;
            }
        }
        n
    })
}

/// How long the boot screen waits for the Italian file before the interface shows in English
/// (it switches to Italian as soon as the file arrives).
const ITALIAN_WAIT_MS: u64 = 1200;

/// The Italian file: the request `index.html` started while the wasm was loading
/// (`window.__rynasIt`), or a new one.
async fn fetch_italian() -> Option<String> {
    use wasm_bindgen::JsCast;
    let w = window();
    let early = js_sys::Reflect::get(&w, &"__rynasIt".into()).ok().and_then(|p| p.dyn_into::<js_sys::Promise>().ok());
    if early.is_some() {
        let _ = js_sys::Reflect::delete_property(&w, &"__rynasIt".into());
    }
    let text = match early {
        Some(p) => wasm_bindgen_futures::JsFuture::from(p).await.ok(),
        None => {
            let resp = wasm_bindgen_futures::JsFuture::from(w.fetch_with_str("/i18n/it.txt")).await.ok()?;
            let resp: web_sys::Response = resp.dyn_into().ok()?;
            if !resp.ok() {
                return None;
            }
            wasm_bindgen_futures::JsFuture::from(resp.text().ok()?).await.ok()
        }
    };
    text?.as_string().filter(|s| !s.is_empty())
}

/// Fetches the Italian texts once; `done` runs when they are there (or the fetch failed).
fn load_italian(done: impl FnOnce() + 'static) {
    if ITALIAN.with(Cell::get) != 0 {
        return;
    }
    ITALIAN.with(|c| c.set(1));
    // before the mount: the reactive executor is not set up yet
    wasm_bindgen_futures::spawn_local(async move {
        let n = fetch_italian().await.map_or(0, install_italian);
        // none: a later switch to Italian tries again
        ITALIAN.with(|c| c.set(if n > 0 { 2 } else { 0 }));
        done();
    });
}

/// The Italian texts arrived after the interface rendered: render its texts again.
fn italian_arrived() {
    if ITALIAN.with(Cell::get) == 2 && sig().get_untracked() == Lang::It {
        sig().update(|_| {});
    }
}

/// Runs `mount` once the texts of the language are there: at once in English, after the
/// Italian file (at most [`ITALIAN_WAIT_MS`]) in Italian.
pub fn when_ready(mount: fn()) {
    if sig().get_untracked() != Lang::It || ITALIAN.with(Cell::get) == 2 {
        mount();
        return;
    }
    let mounted = std::rc::Rc::new(Cell::new(false));
    let m = mounted.clone();
    load_italian(move || {
        if m.replace(true) {
            italian_arrived();
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
        std::time::Duration::from_millis(ITALIAN_WAIT_MS),
    );
}

/// The text of `key` in `lang`, for the tests of the pages.
#[cfg(test)]
pub fn text_in(lang: Lang, key: &str) -> Option<&'static str> {
    lookup(key, lang)
}

/// Every entry as (key, English, Italian), for the tests.
#[cfg(test)]
pub(crate) fn entries() -> Vec<(&'static str, &'static str, &'static str)> {
    INDEX.with(|idx| {
        let it = idx.italian.borrow();
        (0..idx.starts.len())
            .flat_map(|t| (0..(idx.starts[t].len() - 1) / 2).map(move |e| (t, e)))
            .map(|(t, e)| (idx.text(t, e * 2), idx.text(t, e * 2 + 1), it[t][e]))
            .collect()
    })
}

/// The Italian file as the interface fetches it: one `key<TAB>text` line per entry.
#[cfg(test)]
pub(crate) fn italian_file() -> String {
    let mut out = String::new();
    for (k, _, it) in entries() {
        out.push_str(k);
        out.push('\t');
        out.push_str(it);
        out.push('\n');
    }
    out
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

dict! { BLOB, LENS, IT;
    // navigation
    "nav.dashboard" => "Dashboard", "Dashboard";
    "nav.storage" => "Storage", "Archiviazione";
    "nav.files" => "Files", "File";
    "nav.shares" => "Shared Folders", "Cartelle condivise";
    "nav.users" => "Users & Groups", "Utenti e gruppi";
    "nav.services" => "Services", "Servizi";
    "nav.apps" => "Apps & Plugins", "App e plugin";
    "nav.hardware" => "Hardware", "Hardware";
    "nav.system" => "System", "Sistema";
    "nav.menu" => "Menu", "Menu";
    "nav.main" => "Main navigation", "Navigazione principale";
    "nav.collapse" => "Collapse sidebar", "Comprimi barra laterale";
    "nav.expand" => "Expand sidebar", "Espandi barra laterale";
    "nav.project" => "The project on GitHub", "Il progetto su GitHub";
    "nav.notifications" => "Notifications", "Notifiche";
    // app bar
    "bar.search" => "Search or run a command…", "Cerca o esegui un comando…";
    "bar.theme" => "Theme", "Tema";
    "bar.user" => "Account menu", "Menu account";
    "bar.open_menu" => "Open menu", "Apri menu";
    "bar.demo" => "Demo data", "Dati demo";
    "bar.demo_hint" => "The API is not responding: showing demo data", "L'API non risponde: vengono mostrati dati dimostrativi";
    "bar.live" => "Live", "In tempo reale";
    "bar.offline" => "Offline", "Non connesso";
    "bar.connecting" => "Connecting…", "Connessione…";
    "bar.logout" => "Sign out", "Esci";
    "bar.prefs" => "Preferences", "Preferenze";
    // notifications
    "notif.empty" => "You're all caught up", "Nessuna notifica";
    "notif.empty_text" => "Hardware alerts and system events will show up here.", "Qui compariranno gli avvisi hardware e gli eventi di sistema.";
    "notif.mark_read" => "Mark all as read", "Segna tutte come lette";
    "notif.unread" => "unread", "non lette";
    "notif.since" => "since", "dalle";
    "notif.fan_fail" => "Fan failure", "Guasto ventola";
    "notif.fan_fail_text" => "{label} is not spinning or reports an alarm.", "{label} non gira o segnala un allarme.";
    "notif.fan_fail_any" => "The hardware provider reports a fan failure.", "Il provider hardware segnala un guasto alle ventole.";
    "notif.temp_warn" => "High temperature", "Temperatura elevata";
    "notif.temp_crit" => "Critical temperature", "Temperatura critica";
    "notif.temp_text" => "{label} is at {value} (threshold {limit}).", "{label} è a {value} (soglia {limit}).";
    "notif.array_degraded" => "RAID array degraded", "Array RAID degradato";
    "notif.array_degraded_text" => "{name} ({level}) is running without redundancy. Replace the failed disk as soon as possible.", "{name} ({level}) è senza ridondanza. Sostituisci il disco guasto il prima possibile.";
    "notif.array_failed" => "RAID array failed", "Array RAID guasto";
    "notif.array_failed_text" => "{name} ({level}) is inactive or has failed.", "{name} ({level}) è inattivo o guasto.";
    "notif.array_sync" => "RAID resync in progress", "Sincronizzazione RAID in corso";
    "notif.array_sync_text" => "{name} ({level}): {pct} complete.", "{name} ({level}): {pct} completato.";
    "notif.volume_full" => "Volume almost full", "Volume quasi pieno";
    "notif.volume_full_text" => "{mount} is {pct} full, {free} free.", "{mount} è pieno al {pct}, {free} liberi.";
    "nt.src.storage" => "Storage monitor", "Monitoraggio storage";
    "nt.src.hardware" => "Hardware monitor", "Monitoraggio hardware";
    "nt.src.auth" => "Sign-in", "Accesso";
    "nt.src.task" => "Tasks", "Attività";
    "nt.src.config" => "Configuration", "Configurazione";
    "nt.src.snapshots" => "Snapshots", "Snapshot";
    "nt.src.power" => "Power", "Alimentazione";
    "nt.src.network" => "Network", "Rete";
    "nt.src.notifications" => "Notifications", "Notifiche";
    // server notification texts by event code: `nev.<code>.title|body` with `{field}`
    // placeholders; `…_2`/`…_3` are variants for optional (empty) fields, most specific
    // first; `nev.<code>.<field>.<value>` translates a field value of a closed set. A missing
    // key or field shows the server's English text (rynas_api_types::notify::event_text).
    // Code table: docs/phase1/plan.md, "Event codes". rynasd reads these lines for the
    // Italian channel messages, so keep one entry per line.
    // hardware
    "nev.hw.fan_stopped.title" => "Fan {fan} stopped", "Ventola {fan} ferma";
    "nev.hw.fan_running.title" => "Fan {fan} is running again", "La ventola {fan} gira di nuovo";
    "nev.hw.fan_failure.title" => "Fan failure", "Guasto alle ventole";
    "nev.hw.fan_failure.body" => "The hardware controller reports a fan failure: check the fans before the disks overheat.", "Il controller hardware segnala un guasto alle ventole: controllale prima che i dischi si surriscaldino.";
    "nev.hw.fans_ok.title" => "Fans back to normal", "Ventole tornate alla normalità";
    "nev.hw.monitor_stale.title" => "Hardware monitor not updating", "Il monitoraggio hardware non si aggiorna";
    "nev.hw.monitor_stale.body" => "The hardware provider ({provider}) has not written its status for {age_s} s: fan and temperature alarms are blind.", "Il provider hardware ({provider}) non aggiorna il suo stato da {age_s} s: gli allarmi di ventole e temperature non funzionano.";
    "nev.hw.monitor_ok.title" => "Hardware monitor updating again", "Il monitoraggio hardware si aggiorna di nuovo";
    "nev.hw.temp_high.title" => "{label} temperature {celsius} °C", "Temperatura {label} {celsius} °C";
    "nev.hw.temp_high.body" => "{label} is at {celsius} °C (warning {warn_c} °C, critical {crit_c} °C).", "{label} è a {celsius} °C (attenzione {warn_c} °C, critico {crit_c} °C).";
    "nev.hw.temp_ok.title" => "{label} temperature back to normal ({celsius} °C)", "Temperatura {label} tornata normale ({celsius} °C)";
    "nev.hw.thermal_shutdown.title" => "Thermal shutdown imminent: {label} {celsius} °C", "Spegnimento termico imminente: {label} {celsius} °C";
    "nev.hw.thermal_shutdown.body" => "{label} is at {celsius} °C; the hardware controller powers the NAS off at {shutdown_c} °C (when thermal shutdown is on). Check the fans and the airflow.", "{label} è a {celsius} °C; il controller hardware spegne il NAS a {shutdown_c} °C (se lo spegnimento termico è attivo). Controlla le ventole e il flusso d'aria.";
    "nev.hw.thermal_ok.title" => "{label} cooled down ({celsius} °C)", "{label} si è raffreddato ({celsius} °C)";
    // storage
    "nev.storage.disk_warning.title" => "Disk {disk_label} needs attention", "Il disco {disk_label} richiede attenzione";
    "nev.storage.disk_failing.title" => "Disk {disk_label} failing", "Disco {disk_label} in avaria";
    "nev.storage.disk_ok.title" => "Disk {disk_label} healthy again", "Disco {disk_label} di nuovo integro";
    "nev.storage.raid_degraded.title" => "RAID {array} degraded", "RAID {array} degradato";
    "nev.storage.raid_degraded.body" => "{array} ({level}) is running without redundancy: {degraded} of {raid_disks} members missing, state \"{state}\". Members: {members}.", "{array} ({level}) è senza ridondanza: mancano {degraded} membri su {raid_disks}, stato \"{state}\". Membri: {members}.";
    "nev.storage.raid_degraded.body_2" => "{array} ({level}) is running without redundancy: {degraded} of {raid_disks} members missing, state \"{state}\".", "{array} ({level}) è senza ridondanza: mancano {degraded} membri su {raid_disks}, stato \"{state}\".";
    "nev.storage.raid_failed.title" => "RAID {array} failed", "RAID {array} guasto";
    "nev.storage.raid_failed.body" => "{array} ({level}) has failed: {degraded} of {raid_disks} members missing, state \"{state}\". Members: {members}.", "{array} ({level}) è guasto: mancano {degraded} membri su {raid_disks}, stato \"{state}\". Membri: {members}.";
    "nev.storage.raid_failed.body_2" => "{array} ({level}) has failed: {degraded} of {raid_disks} members missing, state \"{state}\".", "{array} ({level}) è guasto: mancano {degraded} membri su {raid_disks}, stato \"{state}\".";
    "nev.storage.raid_inactive.title" => "RAID {array} inactive", "RAID {array} inattivo";
    "nev.storage.raid_inactive.body" => "{array} ({level}) is not running, state \"{state}\".", "{array} ({level}) non è in funzione, stato \"{state}\".";
    "nev.storage.raid_ok.title" => "RAID {array} healthy again", "RAID {array} di nuovo integro";
    "nev.storage.btrfs_errors.title" => "Volume {volume}: device errors on {device}", "Volume {volume}: errori sul dispositivo {device}";
    "nev.storage.btrfs_errors.body" => "Device {device} of volume {volume}: write {write_io} read {read_io} flush {flush_io} corruption {corruption} generation {generation} errors.", "Dispositivo {device} del volume {volume}: errori di scrittura {write_io}, lettura {read_io}, flush {flush_io}, corruzione {corruption}, generazione {generation}.";
    "nev.storage.btrfs_errors_cleared.title" => "Volume {volume}: device error counters cleared", "Volume {volume}: contatori di errore del dispositivo azzerati";
    "nev.storage.btrfs_missing.title" => "Volume {volume}: device {devid} missing", "Volume {volume}: dispositivo {devid} mancante";
    "nev.storage.btrfs_back.title" => "Volume {volume}: device {devid} back", "Volume {volume}: dispositivo {devid} di nuovo presente";
    "nev.storage.volume_full.title" => "Volume {volume} {used_pct}% full", "Volume {volume}: {used_pct}% occupato";
    "nev.storage.volume_full.body" => "{volume} uses {used_gib} GiB, about {free_gib} GiB free ({used_pct}%).", "{volume} usa {used_gib} GiB, circa {free_gib} GiB liberi ({used_pct}%).";
    "nev.storage.volume_ok.title" => "Volume {volume} back under {limit_pct}% ({used_pct}%)", "Volume {volume} di nuovo sotto soglia: {used_pct}% (limite {limit_pct}%)";
    // tasks and configuration (`{type}` shows the task name of `task.<type>`)
    "nev.task.failed.title" => "Task failed: {type}", "Attività non riuscita: {type}";
    "nev.task.succeeded.title" => "Task succeeded: {type}", "Attività riuscita: {type}";
    "nev.config.apply_failed.title" => "Configuration apply failed", "Applicazione della configurazione non riuscita";
    "nev.config.apply_ok.title" => "Configuration apply succeeded", "Applicazione della configurazione riuscita";
    "nev.sched.task_failed.title" => "Task {name} failed", "Attività {name} non riuscita";
    "nev.sched.task_failed.body" => "Result: {result}. See the task history for the output.", "Esito: {result}. L'output è nella cronologia dell'attività.";
    "nev.sched.task_ok.title" => "Task {name} succeeded", "Attività {name} riuscita";
    "nev.sched.task_ok.body" => "Result: {result}. See the task history for the output.", "Esito: {result}. L'output è nella cronologia dell'attività.";
    "nev.sched.task_finished.title" => "Task {name} finished", "Attività {name} completata";
    "nev.sched.task_finished.body" => "Result: {result}. See the task history for the output.", "Esito: {result}. L'output è nella cronologia dell'attività.";
    "nev.snapshot.job_failed.title" => "Snapshot job {name} failed", "Job di snapshot {name} non riuscito";
    "nev.snapshot.job_failed.body" => "Result: {result}. See the task history for the output.", "Esito: {result}. L'output è nella cronologia dell'attività.";
    "nev.snapshot.job_ok.title" => "Snapshot job {name} succeeded", "Job di snapshot {name} riuscito";
    "nev.snapshot.job_ok.body" => "Result: {result}. See the task history for the output.", "Esito: {result}. L'output è nella cronologia dell'attività.";
    "nev.snapshot.job_finished.title" => "Snapshot job {name} finished", "Job di snapshot {name} completato";
    "nev.snapshot.job_finished.body" => "Result: {result}. See the task history for the output.", "Esito: {result}. L'output è nella cronologia dell'attività.";
    // security
    "nev.auth.lockout.title" => "Login locked for {user} after failed attempts", "Accesso bloccato per {user} dopo tentativi falliti";
    "nev.auth.lockout.body" => "Too many failed logins for {user} from {ip}: further attempts are refused for {retry_after_s} s.", "Troppi accessi falliti per {user} da {ip}: altri tentativi vengono rifiutati per {retry_after_s} s.";
    "nev.auth.totp_enabled.title" => "Two-factor authentication enabled for {user}", "Autenticazione a due fattori attivata per {user}";
    "nev.auth.totp_disabled.title" => "Two-factor authentication disabled for {user}", "Autenticazione a due fattori disattivata per {user}";
    // power
    "nev.power.shutdown_skipped.title" => "Scheduled shutdown {rule} skipped", "Spegnimento pianificato {rule} saltato";
    "nev.power.shutdown_skipped.body" => "The NAS stayed on: {reasons}.", "Il NAS è rimasto acceso: {reasons}.";
    "nev.power.shutdown.title" => "Scheduled shutdown {rule}", "Spegnimento pianificato {rule}";
    "nev.power.shutdown.body" => "The NAS is shutting down as scheduled.", "Il NAS si sta spegnendo come pianificato.";
    // network
    "nev.net.confirmed.title" => "Network change confirmed (revision {revision})", "Modifica di rete confermata (revisione {revision})";
    "nev.net.confirmed.body" => "Interfaces: {interfaces}", "Interfacce: {interfaces}";
    "nev.net.rolled_back.title" => "Network change rolled back ({reason}): revision {revision} restored", "Modifica di rete annullata ({reason}): ripristinata la revisione {revision}";
    "nev.net.rolled_back.body" => "The previous network settings run again and were verified (revision {revision}).", "Le impostazioni di rete precedenti sono di nuovo attive e verificate (revisione {revision}).";
    "nev.net.rollback_unverified.title" => "Network rollback ({reason}) could not be verified", "Annullamento della modifica di rete ({reason}) non verificato";
    "nev.net.rollback_unverified.body" => "Still wrong: {problems}. Rescue: the link-local address of the port or the reset button.", "Ancora non funziona: {problems}. Soccorso: l'indirizzo link-local della porta o il pulsante di reset.";
    "nev.net.adopted.title" => "Network migrated to rynas (systemd-networkd)", "Rete migrata a rynas (systemd-networkd)";
    "nev.net.adopted.body" => "The migration done at boot came up with a routable address and working name resolution, and is kept.", "La migrazione eseguita all'avvio ha ottenuto un indirizzo instradabile e una risoluzione dei nomi funzionante, e viene mantenuta.";
    "nev.net.boot_dns_failed.title" => "Network migration at boot undone: names did not resolve", "Migrazione di rete all'avvio annullata: i nomi non si risolvevano";
    "nev.net.boot_dns_failed.body" => "systemd-networkd brought a routable address up, but no DNS server of /etc/resolv.conf answered; ifupdown (revision {revision}) is restored.", "systemd-networkd ha ottenuto un indirizzo instradabile, ma nessun server DNS di /etc/resolv.conf ha risposto; è stato ripristinato ifupdown (revisione {revision}).";
    "nev.net.dns_repaired.title" => "Name resolution repaired", "Risoluzione dei nomi riparata";
    "nev.net.dns_repaired.body" => "No DNS server was configured while systemd-networkd ran the network; {detail}.", "Nessun server DNS era configurato mentre systemd-networkd gestiva la rete; {detail}.";
    "nev.net.bond_member_down.title" => "{bond}: {member} lost its link", "{bond}: {member} ha perso il link";
    "nev.net.bond_member_down.body" => "The bond counted {failures} link failures on {member}; the other members carry the traffic. Check the cable and the switch port.", "Il bond ha contato {failures} interruzioni del link su {member}; il traffico passa dagli altri membri. Controlla il cavo e la porta dello switch.";
    "nev.net.bond_active_changed.title" => "{bond}: {to} carries the traffic now", "{bond}: ora il traffico passa da {to}";
    "nev.net.bond_active_changed.body" => "The bond moved from {from} to {to}.", "Il bond è passato da {from} a {to}.";
    "nev.net.bond_lacp_lost.title" => "{bond}: the LACP group with the switch is broken", "{bond}: il gruppo LACP con lo switch non funziona";
    "nev.net.bond_lacp_lost.body" => "No LACP partner answers, or the members ended up in different aggregators: check the LACP group on the switch.", "Nessun partner LACP risponde, oppure i membri sono finiti in aggregatori diversi: controlla il gruppo LACP sullo switch.";
    "nev.net.boot_undone.title" => "Unconfirmed network change undone at boot: revision {revision} restored", "Modifica di rete non confermata annullata all'avvio: ripristinata la revisione {revision}";
    "nev.net.boot_undone.body" => "The system restarted while revision {tested} was being tested.", "Il sistema si è riavviato mentre la revisione {tested} era in prova.";
    "nev.net.watchdog_restored.title" => "Network restored to the last known good revision {revision}", "Rete riportata all'ultima revisione funzionante ({revision})";
    "nev.net.watchdog_restored.body" => "After boot no port had a routable address; the last known good settings fixed it.", "Dopo l'avvio nessuna porta aveva un indirizzo instradabile; le ultime impostazioni funzionanti hanno risolto il problema.";
    "nev.net.rescue.title" => "Network rescue configuration applied", "Configurazione di rete di emergenza applicata";
    "nev.net.rescue.body" => "No port had a routable address after boot. Every port is on DHCP with link-local; the previous settings are revision {revision}.", "Dopo l'avvio nessuna porta aveva un indirizzo instradabile. Tutte le porte sono in DHCP con link-local; le impostazioni precedenti sono la revisione {revision}.";
    "nev.net.reset.title" => "Factory network restored with the reset button", "Rete di fabbrica ripristinata con il pulsante di reset";
    "nev.net.reset.body" => "Every port is on DHCP with link-local; bonds, VLANs and bridges are suspended. The previous settings are revision {revision} (Network → revisions → restore).", "Tutte le porte sono in DHCP con link-local; bond, VLAN e bridge sono sospesi. Le impostazioni precedenti sono la revisione {revision} (Rete → revisioni → ripristina).";
    // theme / density / language
    "theme.auto" => "System", "Sistema";
    "theme.light" => "Light", "Chiaro";
    "theme.dark" => "Dark", "Scuro";
    "density" => "Density", "Densità";
    "density.comfortable" => "Comfortable", "Comoda";
    "density.compact" => "Compact", "Compatta";
    "language" => "Language", "Lingua";
    "language.auto" => "Browser default", "Come il browser";
    "lang.it" => "Italiano", "Italiano";
    "lang.en" => "English", "English";
    "skip" => "Skip to content", "Vai al contenuto";
    // palette
    "pal.placeholder" => "Search pages and actions…", "Cerca pagine e azioni…";
    "pal.pages" => "Pages", "Pagine";
    "pal.empty" => "No matches", "Nessun risultato";
    "pal.hint_nav" => "navigate", "per spostarti";
    "pal.hint_open" => "open", "per aprire";
    "pal.hint_close" => "close", "per chiudere";
    "pal.go" => "Go to", "Vai a";
    "pal.theme_light" => "Switch to light theme", "Passa al tema chiaro";
    "pal.theme_dark" => "Switch to dark theme", "Passa al tema scuro";
    "pal.theme_auto" => "Use system theme", "Usa il tema di sistema";
    "pal.density_comfortable" => "Comfortable density", "Densità comoda";
    "pal.density_compact" => "Compact density", "Densità compatta";
    "pal.lang_it" => "Language: Italian", "Lingua: italiano";
    "pal.lang_en" => "Language: English", "Lingua: inglese";
    "pal.toggle_sidebar" => "Toggle sidebar", "Mostra/nascondi barra laterale";
    "pal.toggle_tasks" => "Toggle task panel", "Mostra/nascondi pannello attività";
    "pal.edit_layout" => "Customize dashboard", "Personalizza la dashboard";
    "pal.notifications" => "Show notifications", "Mostra notifiche";
    // tasks / pending
    "tasks.title" => "Tasks", "Attività";
    "tasks.none" => "No running tasks", "Nessuna attività in corso";
    "tasks.none_text" => "Long-running operations such as scrubs, resyncs and snapshots will appear here with progress and logs.", "Le operazioni lunghe come scrub, risincronizzazioni e snapshot compariranno qui con avanzamento e log.";
    "tasks.running_1" => "1 running", "1 in corso";
    "tasks.running_n" => "{n} running", "{n} in corso";
    "tasks.last" => "Last", "Ultima";
    "tasks.col_task" => "Task", "Attività";
    "tasks.col_user" => "User", "Utente";
    "tasks.col_start" => "Started", "Avvio";
    "tasks.col_duration" => "Duration", "Durata";
    "tasks.log" => "Task log", "Log dell'attività";
    "tasks.cancel" => "Stop", "Interrompi";
    "tasks.close_log" => "Close log", "Chiudi log";
    "tasks.background" => "You can close this window: the task keeps running.", "Puoi chiudere questa finestra: l'attività continua.";
    "task.failed" => "Failed", "Non riuscita";
    "tech_details" => "Technical details:", "Dettagli tecnici:";
    "task.st.ok" => "OK", "OK";
    "task.st.error" => "Error", "Errore";
    "task.st.cancelled" => "Stopped", "Interrotta";
    "task.st.running" => "Running", "In corso";
    "task.st.queued" => "Queued", "In coda";
    "task.config.apply" => "Apply configuration", "Applicazione configurazione";
    "task.zram.apply" => "Apply memory settings", "Applicazione impostazioni memoria";
    "task.time.apply" => "Apply date and time settings", "Applicazione impostazioni data e ora";
    "task.time.set" => "Set the date and time", "Impostazione di data e ora";
    "task.time.sync" => "Synchronize the clock", "Sincronizzazione dell'orologio";
    "time.title" => "Date and time", "Data e ora";
    "time.subtitle" => "Time zone and clock synchronization", "Fuso orario e sincronizzazione dell'orologio";
    "time.open_text" => "Time zone, time servers and the clock's state.", "Fuso orario, server dell'ora e stato dell'orologio.";
    "time.settings" => "Settings", "Impostazioni";
    "time.zone_search" => "Search a city or region", "Cerca una città o regione";
    "time.zone_help" => "Running apps keep the previous zone until they restart.", "Le app in esecuzione mantengono il fuso precedente fino al riavvio.";
    "time.sync" => "Synchronize automatically", "Sincronizza automaticamente";
    "time.sync_help" => "Certificates, two-factor codes and schedules need the right time.", "Certificati, codici a due fattori e pianificazioni richiedono l'ora esatta.";
    "time.servers" => "Time servers", "Server dell'ora";
    "time.servers_help" => "One per line, at most 8. Empty: the system's default servers.", "Uno per riga, al massimo 8. Vuoto: i server predefiniti del sistema.";
    "time.defaults" => "Default servers: {list}.", "Server predefiniti: {list}.";
    "time.dhcp" => "From DHCP, also used: {list}.", "Da DHCP, usati anch'essi: {list}.";
    "time.dhcp_first" => "From DHCP, asked first: {list}.", "Da DHCP, interrogati per primi: {list}.";
    "time.dhcp_replaced" => "Servers from DHCP ({list}) are not used while your own servers are listed.", "I server da DHCP ({list}) non vengono usati finché sono elencati server propri.";
    "time.servers_ignored" => "The time service's configuration does not read the folder these servers are written to, so they are not used: add the line \"sourcedir /etc/chrony/sources.d\" to it.", "La configurazione del servizio dell'ora non legge la cartella in cui vengono scritti questi server, quindi non sono usati: aggiungici la riga \"sourcedir /etc/chrony/sources.d\".";
    "time.other_daemon" => "Time synchronization is managed by another service, outside rynas: its settings are not changed here.", "La sincronizzazione dell'ora è gestita da un altro servizio, fuori da rynas: le sue impostazioni non si cambiano qui.";
    "time.also_own" => "The servers of the time service's own configuration stay in use too.", "Restano in uso anche i server della configurazione del servizio dell'ora.";
    "time.will_install" => "The time service will be installed from the distribution's archive.", "Il servizio dell'ora verrà installato dall'archivio della distribuzione.";
    "time.apply" => "Apply", "Applica";
    "time.saved" => "Date and time settings applied", "Impostazioni di data e ora applicate";
    "time.status" => "Clock", "Orologio";
    "time.state" => "State", "Stato";
    "time.st_off" => "Manual", "Manuale";
    "time.st_synced" => "Synchronized", "Sincronizzato";
    "time.st_unsynced" => "Not synchronized", "Non sincronizzato";
    "time.source" => "Server", "Server";
    "time.offset" => "Offset", "Scarto";
    "time.last" => "Last sync", "Ultima sincronizzazione";
    "time.poll" => "Checked every", "Controllo ogni";
    "time.sync_now" => "Sync now", "Sincronizza ora";
    "time.synced" => "The clock is synchronized", "L'orologio è sincronizzato";
    "time.manual" => "Set manually", "Impostazione manuale";
    "time.manual_label" => "Date and time (system time zone)", "Data e ora (fuso orario del sistema)";
    "time.browser" => "Use this browser's time", "Usa l'ora di questo browser";
    "time.set" => "Set", "Imposta";
    "time.set_done" => "Date and time set", "Data e ora impostate";
    "time.local_rtc" => "The hardware clock keeps local time: daylight saving changes can shift it by an hour.", "L'orologio hardware è in ora locale: i cambi dell'ora legale possono spostarlo di un'ora.";
    "time.rtc_utc" => "Keep it in UTC", "Tienilo in UTC";
    "nev.system.clock_unsynced.title" => "The clock is not synchronized", "L'orologio non è sincronizzato";
    "nev.system.clock_unsynced.body" => "No time server has answered for {minutes} minutes. Certificates, two-factor codes, schedules and snapshots need the right time: check the network and the time servers.", "Nessun server dell'ora risponde da {minutes} minuti. Certificati, codici a due fattori, pianificazioni e snapshot richiedono l'ora esatta: controlla la rete e i server dell'ora.";
    "nev.system.clock_synced.title" => "The clock is synchronized again", "L'orologio è di nuovo sincronizzato";
    "nev.system.clock_drift.title" => "The clock is drifting", "L'orologio sta andando alla deriva";
    "nev.system.clock_drift.body" => "The clock is {offset} away from the time server and is not catching up. Check the time servers.", "L'orologio è a {offset} dal server dell'ora e non recupera. Controlla i server dell'ora.";
    "nev.system.clock_in_step.title" => "The clock is in step again", "L'orologio è di nuovo allineato";
    "nev.system.clock_sync_off.title" => "Automatic time sync was turned off", "La sincronizzazione automatica dell'ora è stata disattivata";
    "mem.title" => "Memory", "Memoria";
    "mem.subtitle" => "Compressed swap in RAM", "Swap compresso in RAM";
    "mem.open_text" => "zram: compressed swap in RAM, with values advised for this system.", "zram: swap compresso in RAM, con i valori consigliati per questo sistema.";
    "mem.status" => "Status", "Stato";
    "mem.state" => "zram", "zram";
    "mem.on" => "On", "Attiva";
    "mem.off" => "Off", "Spenta";
    "mem.size" => "Size", "Dimensione";
    "mem.algorithm" => "Compression", "Compressione";
    "mem.data" => "Data held", "Dati contenuti";
    "mem.ram_used" => "RAM used", "RAM usata";
    "mem.disk_swaps" => "Swap on disk", "Swap su disco";
    "mem.no_disk_swap" => "None.", "Nessuno.";
    "mem.disk_line" => "{size}, {used} used, priority {priority}, {kind}", "{size}, {used} usati, priorità {priority}, {kind}";
    "mem.disk_hdd" => "hard disk", "disco meccanico";
    "mem.disk_ssd" => "SSD", "SSD";
    "mem.disk_file" => "swap file", "file di swap";
    "mem.disk_help" => "zram has the highest priority: the disks only take what no longer fits in it.", "zram ha la priorità più alta: i dischi ricevono solo ciò che non ci sta più.";
    "mem.zram" => "zram", "zram";
    "mem.zram_help" => "Keeps memory pages compressed in RAM instead of writing them to disk, so a system with little memory stays responsive. It works together with swap on disk.", "Tiene le pagine di memoria compresse in RAM invece di scriverle su disco, così un sistema con poca memoria resta reattivo. Funziona insieme allo swap su disco.";
    "mem.enable" => "Use zram", "Usa zram";
    "mem.advice" => "Advised here: {size}, {algorithm}, swappiness {swappiness}, page-cluster {cluster}.", "Consigliati qui: {size}, {algorithm}, swappiness {swappiness}, page-cluster {cluster}.";
    "mem.use_advice" => "Use these values", "Usa questi valori";
    "mem.size_mib" => "Size (MiB)", "Dimensione (MiB)";
    "mem.size_help" => "Data it can hold before compression; it takes RAM only for what it holds.", "Dati che può contenere prima della compressione; occupa RAM solo per ciò che contiene.";
    "mem.algorithm_help" => "zstd saves more RAM, lz4 uses less CPU.", "zstd fa risparmiare più RAM, lz4 usa meno CPU.";
    "mem.swappiness" => "Swappiness", "Swappiness";
    "mem.swappiness_help" => "0-200: how willingly memory goes to swap rather than dropping file cache. Above 100 suits swap in RAM.", "0-200: quanto volentieri la memoria va in swap invece di scartare la cache dei file. Oltre 100 è adatto allo swap in RAM.";
    "mem.cluster" => "Page-cluster", "Page-cluster";
    "mem.cluster_help" => "0-5: pages read together from swap, as a power of two. 0 suits swap in RAM.", "0-5: pagine lette insieme dallo swap, come potenza di due. 0 è adatto allo swap in RAM.";
    "mem.will_install" => "Applying installs the systemd-zram-generator package from the Debian archive.", "Applicando verrà installato il pacchetto systemd-zram-generator dall'archivio Debian.";
    "mem.apply" => "Apply", "Applica";
    "mem.saved" => "Memory settings saved", "Impostazioni della memoria salvate";
    "mem.pending_restart" => "The saved values are not in use yet. Press Apply to try again, or restart the system.", "I valori salvati non sono ancora in uso. Premi Applica per riprovare, oppure riavvia il sistema.";
    "mem.zswap" => "zswap is on as well: two compression layers on the same memory. Turn zswap off.", "Anche zswap è attivo: due livelli di compressione sulla stessa memoria. Disattiva zswap.";
    "mem.unsupported" => "The running kernel has no zram.", "Il kernel in uso non ha zram.";
    "mem.external" => "This zram device was set up outside rynas, so it is only shown here.", "Questo dispositivo zram è stato configurato fuori da rynas, quindi qui viene solo mostrato.";
    "task.config.overwrite" => "Restore managed files", "Ripristino file gestiti";
    "task.storage.volume-create" => "Create volume", "Creazione volume";
    "task.storage.volume-grow" => "Expand volume", "Espansione volume";
    "task.storage.volume-replace" => "Replace disk", "Sostituzione disco";
    "task.storage.scrub" => "Data scrubbing", "Pulitura dati";
    "task.storage.scrub-stop" => "Stop data scrubbing", "Arresto pulitura dati";
    "task.storage.unlock" => "Unlock volume", "Sblocco volume";
    "task.storage.lock" => "Lock volume", "Blocco volume";
    "task.storage.subvolume-create" => "Create subvolume", "Creazione sottovolume";
    "task.storage.subvolume-delete" => "Delete subvolume", "Eliminazione sottovolume";
    "task.storage.subvolume-settings" => "Subvolume settings", "Impostazioni sottovolume";
    "task.snapshot.run" => "Take snapshots", "Creazione snapshot";
    "task.snapshot.restore" => "Restore from snapshot", "Ripristino da snapshot";
    "task.snapshot.rollback" => "Roll back to snapshot", "Ritorno a snapshot";
    "task.snapshot.delete" => "Delete snapshot", "Eliminazione snapshot";
    "task.network.apply" => "Apply network settings", "Applicazione impostazioni di rete";
    "task.network.rollback" => "Roll back network settings", "Annullamento impostazioni di rete";
    "task.network.restore" => "Restore network settings", "Ripristino impostazioni di rete";
    "task.network.rescue" => "Network rescue", "Ripristino di emergenza della rete";
    "task.network.adopt" => "Adopt network configuration", "Adozione configurazione di rete";
    "task.power.shutdown" => "Shut down", "Spegnimento";
    "task.user.password" => "Set password", "Impostazione password";
    "task.smb.recycle-cleanup" => "Empty recycle bins", "Svuotamento cestini";
    "task.homes.move" => "Move home folders", "Spostamento cartelle home";
    "task.homes.move-finish" => "Delete old home folder", "Eliminazione vecchia cartella home";
    "task.homes.move-undo" => "Undo home move", "Annullamento spostamento home";
    "task.btrfs.scrub" => "Data scrubbing", "Pulitura dati";
    "task.smart.test" => "SMART test", "Test SMART";
    "back" => "Back", "Indietro";
    "cancel" => "Cancel", "Annulla";
    "pending.applying" => "Applying configuration…", "Applicazione della configurazione…";
    "pending.applied" => "Configuration applied.", "Configurazione applicata.";
    "pending.failed" => "Apply failed, nothing was changed.", "Applicazione non riuscita, non è stato cambiato nulla.";
    "pending.show_log" => "Show log", "Mostra log";
    "pending.conflict" => "Conflict", "Conflitto";
    "pending.conflict_text" => "Someone applied a newer value after you staged a change marked Conflict. Discard your changes and make them again.", "Qualcuno ha applicato un valore più recente dopo che hai preparato una modifica segnata come Conflitto. Annulla le modifiche e rifalle.";
    "pending.discard" => "Discard", "Annulla modifiche";
    "pending.discard_title" => "Discard pending changes?", "Annullare le modifiche in sospeso?";
    "pending.discard_text" => "Your {n} staged changes will be dropped. The running configuration is not touched.", "Le {n} modifiche preparate verranno eliminate. La configurazione in uso non viene toccata.";
    "pending.review_title" => "Pending changes", "Modifiche in sospeso";
    "pending.staged" => "Staged", "Preparata il";
    "pending.comment" => "Comment (optional)", "Commento (facoltativo)";
    "pending.comment_hint" => "Why this change? Stored with the revision.", "Perché questa modifica? Viene salvato con la revisione.";
    "diff.empty" => "empty", "vuoto";
    "diff.none" => "No differences.", "Nessuna differenza.";
    "diff.added" => "Added", "Aggiunto";
    "diff.removed" => "Removed", "Rimosso";
    "diff.changed" => "Changed", "Modificato";
    "diff.whole" => "(whole setting)", "(intera impostazione)";
    "diff.field" => "Setting", "Impostazione";
    "diff.kind" => "Change", "Modifica";
    "diff.value" => "Value", "Valore";
    "diff.to" => "becomes", "diventa";
    "cfg.title" => "Configuration", "Configurazione";
    "cfg.subtitle" => "Applied revisions and changes made outside rynas", "Revisioni applicate e modifiche fatte fuori da rynas";
    "cfg.open" => "Revisions and drift", "Revisioni e differenze";
    "cfg.open_text" => "Every apply is a revision you can inspect and restore. rynas also checks that the files it manages were not changed by hand.", "Ogni applicazione è una revisione che puoi esaminare e ripristinare. rynas controlla anche che i file che gestisce non siano stati modificati a mano.";
    "cfg.refresh" => "Refresh", "Aggiorna";
    "cfg.drift" => "Changes outside rynas", "Modifiche fuori da rynas";
    "cfg.drift_text" => "These files or settings no longer match what rynas applied. Import keeps the system's version as a pending change; Restore writes the applied configuration again.", "Questi file o impostazioni non corrispondono più a ciò che rynas ha applicato. Importa tiene la versione del sistema come modifica in sospeso; Ripristina riscrive la configurazione applicata.";
    "cfg.checked" => "{n} managed files checked.", "{n} file gestiti controllati.";
    "cfg.no_drift" => "Everything matches", "Tutto corrisponde";
    "cfg.drift_modified" => "Edited", "Modificato";
    "cfg.drift_missing" => "Missing", "Mancante";
    "cfg.state_missing" => "Missing on the system", "Mancante nel sistema";
    "cfg.state_unmanaged" => "Created outside rynas", "Creato fuori da rynas";
    "cfg.state_mismatch" => "Different", "Diverso";
    "cfg.state_missing_samba" => "No SMB account", "Senza account SMB";
    "cfg.state_missing_passwd" => "SMB account without user", "Account SMB senza utente";
    "cfg.sd.samba_without_user" => "The SMB account {name} has no system user.", "L'account SMB {name} non ha un utente di sistema.";
    "cfg.sd.user_unmanaged" => "{name} (UID {id}) was created outside rynas.", "{name} (UID {id}) è stato creato fuori da rynas.";
    "cfg.sd.user_missing" => "{name} is in the configuration but not on the system.", "{name} è nella configurazione ma non nel sistema.";
    "cfg.sd.user_mismatch" => "The account settings of {name} differ from the configuration.", "Le impostazioni dell'account {name} sono diverse dalla configurazione.";
    "cfg.sd.user_without_samba" => "{name} has SMB access but no SMB account: set the password again.", "{name} ha accesso SMB ma nessun account SMB: imposta di nuovo la password.";
    "cfg.sd.group_missing" => "The group {name} is in the configuration but not on the system.", "Il gruppo {name} è nella configurazione ma non nel sistema.";
    "cfg.sd.group_unmanaged" => "The group {name} (GID {id}) was created outside rynas.", "Il gruppo {name} (GID {id}) è stato creato fuori da rynas.";
    "cfg.sd.share_path_missing" => "{path} does not exist any more.", "{path} non esiste più.";
    "cfg.sd.share_acl_differs" => "The permissions of {path} differ (group membership changed or edited by hand). Missing: {missing}. Unexpected: {extra}.", "I permessi di {path} sono diversi (appartenenza ai gruppi cambiata o modifica a mano). Mancanti: {missing}. Inattesi: {extra}.";
    "cfg.sd.upsn_missing" => "The network UPS server accounts are missing from {path}.", "Gli account del server UPS di rete mancano da {path}.";
    "cfg.sd.upsn_changed" => "The network UPS server accounts in {path} differ from the configuration (edited by hand or by another tool).", "Gli account del server UPS di rete in {path} sono diversi dalla configurazione (modifica a mano o da un altro strumento).";
    "cfg.col_what" => "File or object", "File o oggetto";
    "cfg.col_key" => "Setting", "Impostazione";
    "cfg.col_state" => "State", "Stato";
    "col.actions" => "Actions", "Azioni";
    "cfg.import" => "Import", "Importa";
    "cfg.import_hint" => "Keep the system's version: it becomes a pending change to review and apply", "Tieni la versione del sistema: diventa una modifica in sospeso da rivedere e applicare";
    "cfg.overwrite" => "Restore", "Ripristina";
    "cfg.overwrite_hint" => "Write the applied configuration again, dropping the changes made outside rynas", "Riscrivi la configurazione applicata, scartando le modifiche fatte fuori da rynas";
    "cfg.overwrite_title" => "Restore the applied configuration?", "Ripristinare la configurazione applicata?";
    "cfg.overwrite_text" => "The changes made outside rynas to {key} will be lost:", "Le modifiche fatte fuori da rynas a {key} andranno perse:";
    "cfg.imported" => "{key} imported.", "{key} importato.";
    "cfg.overwritten" => "{key} restored to the applied configuration.", "{key} riportato alla configurazione applicata.";
    "cfg.forbidden_import" => "Only administrators can import this setting.", "Solo gli amministratori possono importare questa impostazione.";
    "cfg.forbidden_overwrite" => "Only administrators can restore this setting.", "Solo gli amministratori possono ripristinare questa impostazione.";
    "cfg.forbidden_restore" => "Only administrators can restore this revision: it changes privileged settings.", "Solo gli amministratori possono ripristinare questa revisione: cambia impostazioni riservate.";
    "cfg.staged_1" => "1 change is waiting in the pending bar.", "1 modifica attende nella barra delle modifiche in sospeso.";
    "cfg.staged_n" => "{n} changes are waiting in the pending bar.", "{n} modifiche attendono nella barra delle modifiche in sospeso.";
    "cfg.revisions" => "Revisions", "Revisioni";
    "cfg.no_revisions" => "Nothing has been applied yet.", "Non è stato ancora applicato nulla.";
    "cfg.col_rev" => "Revision", "Revisione";
    "cfg.col_date" => "Date", "Data";
    "cfg.col_author" => "Author", "Autore";
    "cfg.col_changes" => "Changed", "Modificato";
    "cfg.col_comment" => "Comment", "Commento";
    "cfg.current" => "Current", "Attuale";
    "cfg.restored_from" => "restores #{n}", "ripristina #{n}";
    "cfg.restore" => "Restore", "Ripristina";
    "cfg.restore_title" => "Restore this revision?", "Ripristinare questa revisione?";
    "cfg.restore_text" => "The settings of revision #{n} are staged as pending changes. Nothing changes until you review and apply them.", "Le impostazioni della revisione #{n} vengono preparate come modifiche in sospeso. Non cambia nulla finché non le rivedi e applichi.";
    "cfg.restored" => "Revision #{n} staged.", "Revisione #{n} preparata.";
    "cfg.restore_noop" => "Revision #{n} matches the current configuration: nothing to restore.", "La revisione #{n} corrisponde alla configurazione attuale: niente da ripristinare.";
    "cfg.files" => "Files written", "File scritti";
    "cfg.key.system.hostname" => "Host name", "Nome host";
    "cfg.key.users" => "Users and groups", "Utenti e gruppi";
    "cfg.key.shares" => "Shared folders", "Cartelle condivise";
    "cfg.key.homes" => "User homes", "Home utenti";
    "cfg.key.snapshots" => "Snapshots", "Snapshot";
    "cfg.key.scheduler" => "Scheduled tasks", "Attività pianificate";
    "cfg.key.power.schedule" => "Power schedule", "Accensione e spegnimento pianificati";
    "cfg.key.network.rescue" => "Network rescue", "Ripristino di emergenza della rete";
    "cfg.key.storage.volumes" => "Volumes", "Volumi";
    "cfg.key.storage.disks" => "Disk settings", "Impostazioni dischi";
    "pending.changes" => "pending changes", "modifiche in sospeso";
    "pending.one" => "pending change", "modifica in sospeso";
    "pending.review" => "Review", "Rivedi";
    "pending.stale" => "The pending changes could not be refreshed: the count may be out of date.", "Impossibile aggiornare le modifiche in sospeso: il numero potrebbe non essere aggiornato.";
    "pending.apply" => "Apply", "Applica";
    // dashboard
    "dash.title" => "Dashboard", "Dashboard";
    "dash.subtitle" => "Real-time system overview", "Panoramica del sistema in tempo reale";
    "dash.edit" => "Customize", "Personalizza";
    "dash.done" => "Done", "Fine";
    "dash.reset" => "Reset layout", "Ripristina layout";
    "dash.editing" => "Editing layout", "Modifica layout";
    "dash.edit_hint" => "Drag widgets to reorder them, or focus a handle and use the arrow keys. Changes are saved in this browser.", "Trascina i widget per riordinarli, oppure seleziona una maniglia e usa le frecce. Le modifiche sono salvate in questo browser.";
    "dash.hidden" => "Hidden widgets", "Widget nascosti";
    "dash.hide" => "Hide widget", "Nascondi widget";
    "dash.move" => "Move widget", "Sposta widget";
    "dash.size" => "Widget size", "Dimensione widget";
    "dash.size.S" => "Small", "Piccolo";
    "dash.size.M" => "Medium", "Medio";
    "dash.size.L" => "Large", "Grande";
    "dash.moved" => "{widget}: position {pos} of {count}", "{widget}: posizione {pos} di {count}";
    "dash.all_hidden" => "All widgets are hidden", "Tutti i widget sono nascosti";
    "dash.all_hidden_text" => "Choose Customize to bring some back.", "Scegli Personalizza per mostrarne di nuovo qualcuno.";
    "w.system" => "System", "Sistema";
    "w.cpu" => "CPU", "CPU";
    "w.memory" => "Memory", "Memoria";
    "w.network" => "Network", "Rete";
    "w.disk_io" => "Disk I/O", "I/O dischi";
    "w.volumes" => "Volumes", "Volumi";
    "w.health" => "Storage health", "Stato dello storage";
    "w.hardware" => "Hardware", "Hardware";
    "f.model" => "Model", "Modello";
    "f.os" => "Operating system", "Sistema operativo";
    "f.kernel" => "Kernel", "Kernel";
    "f.uptime" => "Uptime", "Tempo di attività";
    "f.cpu" => "Processor", "Processore";
    "f.threads" => "threads", "thread";
    "f.load" => "Load average", "Carico medio";
    "f.version" => "rynas version", "Versione rynas";
    "f.host" => "Hostname", "Nome host";
    "m.used" => "Used", "In uso";
    "m.cache" => "Cache", "Cache";
    "m.free" => "Free", "Libera";
    "m.swap" => "Swap", "Swap";
    "m.of" => "of", "di";
    "m.zram_detail" => "Compressed swap in RAM: {used} of RAM used, {saved} saved, {size} capacity", "Swap compresso in RAM: {used} di RAM usati, {saved} risparmiati, capacità {size}";
    "n.rx" => "Download", "Ricezione";
    "n.tx" => "Upload", "Invio";
    "n.down" => "Disconnected", "Scollegata";
    "n.none" => "No network interfaces", "Nessuna interfaccia di rete";
    "d.read" => "Read", "Lettura";
    "d.write" => "Write", "Scrittura";
    "d.busy" => "utilization", "utilizzo";
    "v.free" => "free", "liberi";
    "v.none" => "No volumes", "Nessun volume";
    "v.none_text" => "No mounted file systems were found.", "Non è stato trovato alcun file system montato.";
    "v.network" => "{n} network mounts", "{n} mount di rete";
    "v.more" => "{n} more in Storage", "Altri {n} in Archiviazione";
    "v.removable" => "{n} on USB or removable drives", "{n} su dischi USB o rimovibili";
    "v.removable_hint" => "Not NAS volumes: they do not count for the storage state.", "Non sono volumi del NAS: non contano per lo stato dello storage.";
    "h.sub_ok" => "No problems detected", "Nessun problema rilevato";
    "h.sub_sync" => "Redundancy is being rebuilt", "Ridondanza in ricostruzione";
    "h.sub_warn" => "Check the details in Storage", "Controlla i dettagli in Archiviazione";
    "v.arrays" => "RAID arrays", "Array RAID";
    "h.disks" => "Disks", "Dischi";
    "h.arrays" => "Arrays", "Array";
    "h.volumes" => "Volumes", "Volumi";
    "h.asleep" => "in standby", "in standby";
    "h.ok" => "All storage is healthy", "Tutto lo storage è integro";
    "h.syncing" => "A RAID array is resyncing", "Un array RAID è in sincronizzazione";
    "h.warn" => "Attention needed", "Richiede attenzione";
    "h.err" => "Storage failure", "Guasto dello storage";
    "h.fullest" => "Fullest volume", "Volume più pieno";
    "h.no_arrays" => "no RAID", "nessun RAID";
    "per_thread" => "Per thread", "Per thread";
    "last_min" => "Last {n} min", "Ultimi {n} min";
    // hardware
    "hw.fans" => "Fans", "Ventole";
    "hw.temps" => "Temperatures", "Temperature";
    "hw.led" => "Status LED", "LED di stato";
    "hw.events" => "Events", "Eventi";
    "hw.duty" => "Fan duty", "Velocità ventole";
    "hw.fan_fail" => "Fan failure detected", "Rilevato un guasto alle ventole";
    "hw.alarm" => "Alarm", "Allarme";
    "hw.provider" => "Provider", "Provider";
    "hw.stale" => "Stale data", "Dati non aggiornati";
    "hw.updated" => "Last update", "Ultimo aggiornamento";
    "hw.ago" => "ago", "fa";
    "hw.none_title" => "No hardware provider", "Nessun provider hardware";
    "hw.none_text" => "This machine does not expose fans, LEDs or drive bays through a hardware provider. Sensor temperatures are still shown on the dashboard.", "Questa macchina non espone ventole, LED o alloggiamenti tramite un provider hardware. Le temperature dei sensori restano visibili nella dashboard.";
    "hw.no_events" => "No recent events", "Nessun evento recente";
    "hw.subtitle" => "Fans, temperatures, status LED and platform events", "Ventole, temperature, LED di stato ed eventi della piattaforma";
    "led.green" => "Green", "Verde";
    "led.green-blink" => "Blinking green", "Verde lampeggiante";
    "led.orange" => "Orange", "Arancione";
    "led.orange-blink" => "Blinking orange", "Arancione lampeggiante";
    "led.off" => "Off", "Spento";
    "hw.st_normal" => "Normal", "Stato normale";
    "hw.st_starting" => "Starting up", "In avvio";
    "hw.st_attention" => "Needs attention", "Attenzione";
    "hw.st_fault" => "Fault", "Guasto";
    "hw.fan_n" => "Fan {n}", "Ventola {n}";
    "hw.tab_overview" => "Overview", "Panoramica";
    "ps.title" => "Power schedule", "Pianificazione alimentazione";
    "last_boot" => "Last boot", "Ultimo avvio";
    "hw.power_led" => "Power LED", "LED di alimentazione";
    "hw.led_status_short" => "Status", "Status";
    "hw.led_power_short" => "Power", "Power";
    "hw.bay_no_power" => "Disk present, bay without power", "Disco presente, alloggiamento senza alimentazione";
    "hw.temp_last" => "Last reading before standby (the disk is not woken up)", "Ultima lettura prima dello standby (il disco non viene risvegliato)";
    "hw.identify" => "Identify", "Identifica";
    "hw.identifying" => "Blinking…", "Lampeggia…";
    "hw.identify_on" => "The LED of bay {n} blinks for {s} seconds.", "Il LED dell'alloggiamento {n} lampeggia per {s} secondi.";
    "hw.v_range" => "Between {min} and {max}.", "Tra {min} e {max}.";
    "reset_button" => "Reset button", "Pulsante di reset";
    "ps.help" => "Weekly rules that start or shut down the NAS. Startup uses the RTC wake alarm, set before every shutdown; shutdowns check the system first.", "Regole settimanali che accendono o spengono il NAS. L'accensione usa la sveglia RTC, impostata prima di ogni spegnimento; gli spegnimenti controllano prima il sistema.";
    "add_rule" => "Add rule", "Aggiungi regola";
    "ps.edit" => "Edit rule", "Modifica regola";
    "ps.none_title" => "No power schedule", "Nessuna pianificazione";
    "ps.none_text" => "Add a rule to start or shut down the NAS on chosen days and times.", "Aggiungi una regola per accendere o spegnere il NAS in giorni e orari scelti.";
    // UPS (Hardware › UPS)
    "ups.tab" => "UPS", "UPS";
    "task.ups.test" => "UPS battery test", "Test batteria UPS";
    "task.ups.detect" => "UPS detection", "Rilevamento UPS";
    "task.ups.install" => "Install NUT", "Installazione di NUT";
    "task.ups.update-nut" => "Update NUT", "Aggiornamento di NUT";
    "ups.install" => "Install NUT", "Installa NUT";
    "ups.installed" => "NUT installed. Set up the UPS with UPS setup.", "NUT installato. Configura l'UPS con Configurazione UPS.";
    "ups.missing_text" => "NUT, the UPS service, is not installed on this NAS. Install it here (Debian package nut), then set up the UPS with UPS setup below.", "NUT, il servizio UPS, non è installato su questo NAS. Installalo da qui (pacchetto Debian nut), poi configura l'UPS con Configurazione UPS qui sotto.";
    "ups.status" => "UPS status", "Stato UPS";
    "ups.st_unreachable" => "UPS service not reachable", "Servizio UPS non raggiungibile";
    "ups.st_none" => "No UPS", "Nessun UPS";
    "ups.st_fsd" => "Forced shutdown", "Arresto forzato";
    "ups.st_critical" => "Critical: on battery, battery low", "Critico: a batteria, batteria scarica";
    "ups.st_low" => "Battery low", "Batteria scarica";
    "ups.st_battery" => "On battery", "A batteria";
    "ups.st_replace" => "Replace the battery", "Sostituire la batteria";
    "ups.st_online" => "Online", "In linea";
    "ups.st_unknown" => "State unknown", "Stato sconosciuto";
    "ups.f_ol" => "OL · mains power", "OL · rete elettrica";
    "ups.f_ob" => "OB · on battery", "OB · a batteria";
    "ups.f_lb" => "LB · low battery", "LB · batteria scarica";
    "ups.f_chrg" => "CHRG · charging", "CHRG · in carica";
    "ups.f_dischrg" => "DISCHRG · discharging", "DISCHRG · in scarica";
    "ups.f_rb" => "RB · replace battery", "RB · sostituire batteria";
    "ups.f_fsd" => "FSD · forced shutdown", "FSD · arresto forzato";
    "ups.f_bypass" => "BYPASS · on bypass", "BYPASS · in bypass";
    "ups.f_cal" => "CAL · calibrating", "CAL · calibrazione";
    "ups.f_over" => "OVER · overloaded", "OVER · sovraccarico";
    "ups.none_title" => "No UPS connected", "Nessun UPS collegato";
    "ups.none_text" => "The UPS service runs but reports no UPS. Connect the UPS and set it up with UPS setup below.", "Il servizio UPS è attivo ma non segnala alcun UPS. Collega l'UPS e configuralo con Configurazione UPS qui sotto.";
    "ups.down_title" => "UPS service not running", "Servizio UPS non attivo";
    "ups.down_text" => "Nothing answers on the NUT port of this NAS (127.0.0.1:3493). Set up the UPS with UPS setup below.", "Nessuna risposta sulla porta NUT di questo NAS (127.0.0.1:3493). Configura l'UPS con Configurazione UPS qui sotto.";
    "ups.charge" => "Battery charge", "Carica batteria";
    "ups.runtime" => "Runtime left", "Autonomia residua";
    "ups.load" => "Load", "Carico";
    "ups.input" => "Input voltage", "Tensione in ingresso";
    "ups.output" => "Output voltage", "Tensione in uscita";
    "ups.manufacturer" => "Manufacturer", "Produttore";
    "ups.beeper" => "Beeper", "Segnale acustico";
    "ups.beeper_on" => "On", "Attivo";
    "ups.beeper_off" => "Off", "Disattivato";
    "ups.beeper_muted" => "Muted", "Silenziato";
    "ups.battery_date" => "Battery date", "Data batteria";
    "ups.driver" => "NUT name and driver", "Nome NUT e driver";
    "ups.safemode" => "Power failure", "Interruzione di corrente";
    "ups.lb_always_nut" => "NUT's upsmon always shuts the system down when the UPS reports low battery (LB) or is critical, and rynas stops its tasks first. The timer below only adds an earlier shutdown.", "upsmon di NUT spegne sempre il sistema quando l'UPS segnala batteria scarica (LB) o è critico, e rynas prima ferma le sue attività. Il timer qui sotto aggiunge solo uno spegnimento anticipato.";
    "ups.need_nut" => "The shutdown policy and the battery tests use NUT: install it with Install NUT in UPS status above, then set up the UPS.", "La politica di spegnimento e i test batteria usano NUT: installalo con Installa NUT in Stato UPS qui sopra, poi configura l'UPS.";
    "ups.timer_nut" => "Shut down after a time on battery", "Spegni dopo un tempo a batteria";
    "ups.timer_help_nut" => "rynas stops its tasks, then asks NUT's upsmon to shut the system down (upsmon -c fsd), with the UPS power-off configured in NUT.", "rynas ferma le sue attività, poi chiede a upsmon di NUT di spegnere il sistema (upsmon -c fsd), con lo spegnimento dell'UPS configurato in NUT.";
    "ups.minutes" => "Minutes on battery", "Minuti a batteria";
    "ups.minutes_help" => "1 to 1440 minutes.", "Da 1 a 1440 minuti.";
    "ups.pre_timeout" => "Time to stop apps and tasks (s)", "Tempo per fermare app e attività (s)";
    "ups.pre_timeout_help_nut" => "Before the shutdown, rynas stops its running tasks and scheduled jobs within this time (10 to 900 s).", "Prima dello spegnimento, rynas ferma le attività in corso e quelle pianificate entro questo tempo (da 10 a 900 s).";
    "ups.pending_note" => "Saved, not applied yet: the UPS uses the applied settings.", "Salvato, non ancora applicato: l'UPS usa le impostazioni applicate.";
    "ups.v_minutes" => "Enter whole minutes from 1 to 1440.", "Inserisci minuti interi da 1 a 1440.";
    "ups.v_pre" => "Enter seconds from 10 to 900.", "Inserisci secondi da 10 a 900.";
    "ups.test" => "Battery test", "Test batteria";
    "ups.last_result" => "Last result", "Ultimo risultato";
    "ups.no_result" => "Not reported by the UPS", "Non segnalato dall'UPS";
    "ups.tr.passed" => "Completed: passed", "Completato: superato";
    "ups.tr.warning" => "Completed with a warning", "Completato con un avviso";
    "ups.tr.error" => "Completed: failed", "Completato: non superato";
    "ups.tr.aborted" => "Aborted", "Interrotto";
    "ups.tr.in_progress" => "Test in progress", "Test in corso";
    "ups.tr.none" => "No test run yet", "Nessun test eseguito";
    "ups.tr.scheduled" => "Test scheduled", "Test programmato";
    "ups.test_done" => "Battery test finished.", "Test batteria terminato.";
    "ups.test_unsupported" => "This UPS offers no battery test through NUT.", "Questo UPS non offre test della batteria tramite NUT.";
    "ups.test_online" => "A test can start only while the UPS is on mains power.", "Un test può partire solo mentre l'UPS è alimentato dalla rete.";
    "ups.test_lb_nut" => "During a test, low battery still shuts the system down.", "Durante un test, la batteria scarica spegne comunque il sistema.";
    "ups.quick" => "Quick test", "Test rapido";
    "ups.deep" => "Deep test", "Test approfondito";
    "ups.stop" => "Stop test", "Ferma test";
    "ups.deep_title" => "Start a deep battery test?", "Avviare un test approfondito della batteria?";
    "ups.deep_text_nut" => "The UPS runs on battery until it is almost empty, which can take an hour. If the battery reaches low battery (LB), NUT shuts the system down: start it only when someone is present to stop the test.", "L'UPS funziona a batteria fino a scaricarla quasi del tutto, anche per un'ora. Se la batteria arriva a batteria scarica (LB), NUT spegne il sistema: avvialo solo con qualcuno presente per fermare il test.";
    "ups.deep_start" => "Start deep test", "Avvia test approfondito";
    "ups.schedule_hint" => "To test on a schedule, add a scheduled task that runs as root:", "Per un test pianificato, aggiungi un'attività pianificata eseguita come root:";
    "ups.events" => "Recent UPS events", "Eventi UPS recenti";
    "ups.no_events" => "No UPS events yet.", "Ancora nessun evento UPS.";
    "ups.device" => "Driver and connection", "Driver e collegamento";
    "upsw.title" => "UPS setup", "Configurazione UPS";
    "upsw.intro" => "No UPS reports to this NAS. rynas can look for a UPS on USB, test which NUT driver reads it and set it up.", "Nessun UPS comunica con questo NAS. rynas può cercare un UPS su USB, provare quale driver NUT lo legge e configurarlo.";
    "upsw.need_nut" => "Install NUT with Install NUT in UPS status above to set up a UPS here.", "Installa NUT con Installa NUT in Stato UPS qui sopra per configurare qui un UPS.";
    "upsw.name" => "NUT name", "Nome NUT";
    "upsw.name_help" => "The name of the UPS in NUT (lowercase letters, digits, - and _).", "Il nome dell'UPS in NUT (lettere minuscole, cifre, - e _).";
    "upsw.change" => "Change UPS", "Cambia UPS";
    "upsw.start" => "Set up UPS", "Configura UPS";
    "upsw.s_find" => "Find", "Cerca";
    "upsw.s_test" => "Test", "Prova";
    "upsw.s_review" => "Review", "Riepilogo";
    "upsw.s_done" => "Done", "Fatto";
    "upsw.find_text" => "Connect the UPS to this NAS. The detection lists the UPSes on USB and tries the matching NUT drivers one by one until one reads the UPS status. For a serial or network UPS, enter the driver by hand.", "Collega l'UPS a questo NAS. Il rilevamento elenca gli UPS su USB e prova uno alla volta i driver NUT adatti finché uno legge lo stato dell'UPS. Per un UPS seriale o di rete, inserisci il driver a mano.";
    "upsw.stop_drivers" => "Stop the running UPS driver during the test", "Ferma il driver UPS in funzione durante la prova";
    "upsw.stop_drivers_help" => "A driver already holds the UPS: it is stopped while the drivers are tested and started again afterwards. The UPS is not monitored meanwhile.", "Un driver occupa già l'UPS: viene fermato mentre si provano i driver e riavviato dopo. Nel frattempo l'UPS non è sorvegliato.";
    "upsw.detect" => "Detect USB UPS", "Rileva UPS USB";
    "upsw.manual" => "Enter by hand", "Inserisci a mano";
    "upsw.testing" => "Each driver gets up to 45 seconds to answer.", "Ogni driver ha fino a 45 secondi per rispondere.";
    "upsw.no_report" => "No detection result yet.", "Ancora nessun risultato del rilevamento.";
    "upsw.busy" => "A UPS driver is running and holds the device: nothing was tested. Allow stopping it and detect again.", "Un driver UPS è in funzione e occupa il dispositivo: non è stato provato nulla. Consenti di fermarlo e rileva di nuovo.";
    "upsw.need_newer" => "No driver read this UPS. Its known setting needs NUT {need} or newer and this NAS has NUT {have}: install a newer NUT on this system, then detect again.", "Nessun driver ha letto questo UPS. La sua impostazione nota richiede NUT {need} o successivo e questo NAS ha NUT {have}: installa un NUT più recente su questo sistema, poi rileva di nuovo.";
    "upsw.need_newer_update" => "No driver read this UPS. Its known setting needs NUT {need} or newer and this NAS has NUT {have}. rynas can update NUT to {v} ({source}), then you detect again.", "Nessun driver ha letto questo UPS. La sua impostazione nota richiede NUT {need} o successivo e questo NAS ha NUT {have}. rynas può aggiornare NUT alla {v} ({source}), poi rilevi di nuovo.";
    "upsw.need_newer_files" => "No driver read this UPS. Its known setting needs NUT {need} or newer and this NAS has NUT {have}. rynas has NUT {v} for this system, attached to the rynas release: copy its packages (nut-server, nut-client, libupsclient7, libnutscan4) to {dir}, then detect again to update.", "Nessun driver ha letto questo UPS. La sua impostazione nota richiede NUT {need} o successivo e questo NAS ha NUT {have}. rynas ha NUT {v} per questo sistema, allegato alla release di rynas: copia i suoi pacchetti (nut-server, nut-client, libupsclient7, libnutscan4) in {dir}, poi rileva di nuovo per aggiornare.";
    "upsw.nut_src_apt" => "from the package sources of this system", "dalle sorgenti dei pacchetti di questo sistema";
    "upsw.nut_src_backports" => "from the Debian backports configured on this system", "dai backports di Debian configurati su questo sistema";
    "upsw.nut_src_rynas" => "rynas's build of Debian's NUT package, downloaded from the rynas release if it is not already on this NAS", "la build di rynas del pacchetto NUT di Debian, scaricata dalla release di rynas se non è già su questo NAS";
    "upsw.update_nut" => "Update NUT to {v}", "Aggiorna NUT alla {v}";
    "upsw.updating" => "apt installs the new NUT packages; your NUT configuration files are kept.", "apt installa i nuovi pacchetti di NUT; i tuoi file di configurazione di NUT restano.";
    "upsw.updated" => "NUT {v} is installed: detect the UPS again.", "NUT {v} è installato: rileva di nuovo l'UPS.";
    "upsw.driver_output" => "Driver output", "Output del driver";
    "upsw.no_usb" => "No UPS found on USB. Check the cable, or enter a serial or network UPS by hand.", "Nessun UPS trovato su USB. Controlla il cavo, oppure inserisci a mano un UPS seriale o di rete.";
    "upsw.none_answered" => "No driver read the UPS status. The last lines of each driver are below: try another driver or other options by hand.", "Nessun driver ha letto lo stato dell'UPS. Qui sotto le ultime righe di ogni driver: prova a mano un altro driver o altre opzioni.";
    "upsw.found" => "Found: {model}, status {status}, charge {charge}.", "Trovato: {model}, stato {status}, carica {charge}.";
    "upsw.detect_again" => "Detect again", "Rileva di nuovo";
    "upsw.usb" => "On USB", "Su USB";
    "upsw.nut_version" => "NUT version on this NAS: {v}", "Versione di NUT su questo NAS: {v}";
    "upsw.candidates" => "Drivers tried", "Driver provati";
    "upsw.min_nut" => "Needs NUT {v} or newer", "Richiede NUT {v} o successivo";
    "upsw.p_ok" => "Answers", "Risponde";
    "upsw.p_failed" => "No answer", "Nessuna risposta";
    "upsw.p_not_run" => "Not tried", "Non provato";
    "upsw.p_busy" => "Device in use", "Dispositivo in uso";
    "upsw.p_newer" => "Needs a newer NUT", "Serve un NUT più recente";
    "upsw.p_no_driver" => "Driver not installed", "Driver non installato";
    "upsw.src_known" => "Known setting for this model", "Impostazione nota per questo modello";
    "upsw.src_scanner" => "Suggested by the USB scan", "Suggerito dalla scansione USB";
    "upsw.src_generic" => "Generic driver", "Driver generico";
    "upsw.src_manual" => "Entered by hand", "Inserito a mano";
    "upsw.manual_text" => "Pick the driver that NUT's hardware list names for the UPS. USB: port auto (add vendorid and productid when several UPSes are connected). Serial: the device, such as /dev/ttyS0. Network (SNMP): the address, with options such as community=public.", "Scegli il driver che l'elenco hardware di NUT indica per l'UPS. USB: porta auto (aggiungi vendorid e productid se sono collegati più UPS). Seriale: il dispositivo, come /dev/ttyS0. Rete (SNMP): l'indirizzo, con opzioni come community=public.";
    "upsw.driver" => "Driver", "Driver";
    "upsw.pick_driver" => "Choose a driver", "Scegli un driver";
    "upsw.driver_help" => "The NUT drivers installed on this NAS.", "I driver NUT installati su questo NAS.";
    "upsw.driver_for" => "Listed by NUT for: {list}", "Indicato da NUT per: {list}";
    "upsw.port" => "Port", "Porta";
    "upsw.port_help" => "auto for USB, a serial device such as /dev/ttyS0, or a network address.", "auto per USB, un dispositivo seriale come /dev/ttyS0, oppure un indirizzo di rete.";
    "upsw.options" => "Driver options", "Opzioni del driver";
    "upsw.options_help" => "key=value separated by spaces; a word alone is a flag (norating).", "chiave=valore separate da spazi; una parola da sola è un'opzione senza valore (norating).";
    "upsw.skip_test" => "Continue without a test", "Continua senza prova";
    "upsw.test" => "Test this setting", "Prova questa impostazione";
    "upsw.untested" => "This setting was not tested: if the driver cannot start, the apply fails and the previous setup comes back.", "Questa impostazione non è stata provata: se il driver non parte, l'applicazione fallisce e torna la configurazione precedente.";
    "upsw.conflict" => "NUT's ups.conf has a UPS section that rynas does not manage and that would use the same device. rynas changes it only if you take it over: the section is then removed (a backup is kept).", "Il file ups.conf di NUT ha una sezione UPS che rynas non gestisce e che userebbe lo stesso dispositivo. rynas la modifica solo se la prendi in carico: la sezione viene allora rimossa (resta una copia di sicurezza).";
    "upsw.take_over" => "Take over and remove the section [{name}]", "Prendi in carico e rimuovi la sezione [{name}]";
    "upsw.removed" => "Lines removed:", "Righe rimosse:";
    "upsw.commands" => "Then rynas runs:", "Poi rynas esegue:";
    "upsw.apply" => "Apply", "Applica";
    "upsw.waiting" => "Waiting for the UPS to report…", "In attesa che l'UPS comunichi…";
    "upsw.applied" => "The UPS is set up: NUT now runs its driver, upsd and upsmon.", "L'UPS è configurato: ora NUT esegue il suo driver, upsd e upsmon.";
    // NUT network server
    "upsn.title" => "Network UPS server", "Server UPS di rete";
    "upsn.intro" => "Other machines on the network read this UPS on port 3493 and shut down when its battery runs out: servers and PCs with NUT's upsmon, home automation, dashboards.", "Gli altri computer in rete leggono questo UPS sulla porta 3493 e si spengono quando la sua batteria si esaurisce: server e PC con upsmon di NUT, domotica, pannelli di controllo.";
    "upsn.on" => "On", "Attivo";
    "upsn.off" => "Off", "Spento";
    "upsn.pending" => "Changes not applied yet", "Modifiche non ancora applicate";
    "upsn.need_nut" => "The network UPS server needs NUT: install it with Install NUT in UPS status above.", "Il server UPS di rete richiede NUT: installalo con Installa NUT in Stato UPS qui sopra.";
    "upsn.module" => "The system layer of this hardware runs NUT with its own files: the network UPS server cannot be set up here.", "Su questo hardware NUT è gestito dal livello di sistema con i suoi file: qui il server UPS di rete non si può configurare.";
    "upsn.listen_all" => "All addresses of the NAS", "Tutti gli indirizzi del NAS";
    "upsn.listen_chosen" => "Chosen addresses", "Indirizzi scelti";
    "upsn.listen_help" => "The NAS itself (localhost) is always included.", "Il NAS stesso (localhost) è sempre incluso.";
    "upsn.listen_bad" => "{addr} is not an address of this NAS: untick it or choose one of the addresses above.", "{addr} non è un indirizzo di questo NAS: toglilo o scegli uno degli indirizzi qui sopra.";
    "upsn.allow" => "Allowed hosts", "Host ammessi";
    "upsn.allow_any" => "Any host that reaches the NAS", "Qualsiasi host che raggiunge il NAS";
    "upsn.filter" => "Host filter", "Filtro degli host";
    "upsn.filter_on" => "Working", "Attivo";
    "upsn.filter_off" => "Not working", "Non funzionante";
    "upsn.pb.server_inactive" => "The server is on but upsd (nut-server) is not running: no client can connect. Set up the UPS first, or look at the service log.", "Il server è attivo ma upsd (nut-server) non è in esecuzione: nessun client può collegarsi. Configura prima l'UPS o guarda il log del servizio.";
    "upsn.pb.filter_inactive" => "The allowed-hosts filter does not work on this system (the kernel does not apply systemd's IP filter): every host that reaches port 3493 can connect. Limit port 3493 with a firewall.", "Il filtro degli host ammessi non funziona su questo sistema (il kernel non applica il filtro IP di systemd): ogni host che raggiunge la porta 3493 può collegarsi. Limita la porta 3493 con un firewall.";
    "upsn.pb.other" => "The server reports a problem this interface does not know: update rynas.", "Il server segnala un problema che questa interfaccia non conosce: aggiorna rynas.";
    "upsn.allow_help" => "One IP address or subnet per line (192.168.1.20, 192.168.1.0/24, fd00::/64). Empty: no limit. Other hosts are dropped by the system before they reach NUT. Containers on this NAS (Home Assistant in a bridge network, for example) connect from their Docker bridge subnet: allow that subnet too.", "Un indirizzo IP o una sottorete per riga (192.168.1.20, 192.168.1.0/24, fd00::/64). Vuoto: nessun limite. Gli altri host vengono scartati dal sistema prima di arrivare a NUT. I container di questo NAS (per esempio Home Assistant in una rete bridge) si collegano dalla sottorete del loro bridge Docker: ammetti anche quella.";
    "upsn.allow_ph" => "e.g. 192.168.1.0/24", "es. 192.168.1.0/24";
    "upsn.allow_lan" => "Allow the local network ({nets})", "Ammetti la rete locale ({nets})";
    "upsn.address" => "Address", "Indirizzo";
    "upsn.ups_name" => "UPS name", "Nome dell'UPS";
    "upsn.open_warn" => "No allowed hosts are set: every device that reaches port 3493 can read the UPS status. Limit it to your network in the settings.", "Nessun host ammesso impostato: ogni dispositivo che raggiunge la porta 3493 può leggere lo stato dell'UPS. Limitalo alla tua rete nelle impostazioni.";
    "upsn.accounts" => "Client accounts", "Account dei client";
    "upsn.no_accounts" => "No accounts yet. Add one for each machine that must shut down with this UPS, and one for home automation.", "Ancora nessun account. Aggiungine uno per ogni computer che deve spegnersi con questo UPS e uno per la domotica.";
    "upsn.settings" => "Settings", "Impostazioni";
    "upsn.settings_title" => "Network UPS server settings", "Impostazioni del server UPS di rete";
    "upsn.add" => "Add account", "Aggiungi account";
    "upsn.add_title" => "New client account", "Nuovo account client";
    "upsn.user_help" => "a-z, 0-9, _ and -, at most 32 characters.", "a-z, 0-9, _ e -, al massimo 32 caratteri.";
    "upsn.user_required" => "Enter a user name.", "Inserisci un nome utente.";
    "upsn.user_bad" => "Use 1 to 32 characters a-z, 0-9, _ and - (rynas and rynas-upsmon are taken by rynas).", "Usa da 1 a 32 caratteri a-z, 0-9, _ e - (rynas e rynas-upsmon sono usati da rynas).";
    "upsn.user_taken" => "An account with this name already exists.", "Esiste già un account con questo nome.";
    "upsn.role" => "Role", "Ruolo";
    "upsn.role_secondary" => "Shuts down with the UPS", "Si spegne con l'UPS";
    "upsn.role_monitor" => "Reads only", "Solo lettura";
    "upsn.role_secondary_help" => "For another machine's upsmon (secondary): it shuts that machine down when this UPS reaches low battery, before the NAS.", "Per upsmon di un altro computer (secondario): spegne quel computer quando questo UPS arriva a batteria scarica, prima del NAS.";
    "upsn.role_monitor_help" => "For home automation and dashboards: it logs in and reads the values, it cannot run commands.", "Per domotica e pannelli: accede e legge i valori, non può eseguire comandi.";
    "upsn.note" => "Note", "Nota";
    "upsn.own_password" => "Choose the password", "Scegli la password";
    "upsn.own_password_help" => "Off: rynas generates a strong one and shows it once.", "Disattivato: rynas ne genera una robusta e la mostra una volta.";
    "upsn.password" => "Password", "Password";
    "upsn.password_help" => "12 to 64 characters: letters, digits and . _ ~ @ % + = : , - (no spaces, quotes or #).", "Da 12 a 64 caratteri: lettere, cifre e . _ ~ @ % + = : , - (niente spazi, virgolette o #).";
    "upsn.password_bad" => "The password needs 12 to 64 characters among letters, digits and . _ ~ @ % + = : , -", "La password richiede da 12 a 64 caratteri tra lettere, cifre e . _ ~ @ % + = : , -";
    "upsn.create" => "Create", "Crea";
    "upsn.connect" => "Connect", "Collega";
    "upsn.connect_title" => "Connect with the account {name}", "Collegamento con l'account {name}";
    "upsn.remove" => "Remove", "Rimuovi";
    "upsn.remove_title" => "Remove the account {name}?", "Rimuovere l'account {name}?";
    "upsn.remove_text" => "The machine that uses it can no longer log in and will not shut down with this UPS.", "Il computer che lo usa non potrà più accedere e non si spegnerà con questo UPS.";
    "upsn.regen_title" => "New password for {name}?", "Nuova password per {name}?";
    "upsn.regen_text" => "The current password stops working: enter the new one on the machine that uses this account.", "La password attuale smette di funzionare: inserisci quella nuova sul computer che usa questo account.";
    "upsn.enable" => "Accept connections from other machines", "Accetta connessioni da altri computer";
    "upsn.enable_help" => "upsd accepts connections from other machines on port 3493.", "upsd accetta connessioni da altri computer sulla porta 3493.";
    "upsn.read_note" => "NUT lets anyone who connects read the UPS values without a password; accounts decide who logs in to follow it for a shutdown. Allowed hosts limit who connects at all.", "NUT permette a chiunque si colleghi di leggere i valori dell'UPS senza password; gli account decidono chi accede per seguirlo e spegnersi. Gli host ammessi limitano chi può collegarsi.";
    "upsn.show_changes" => "Show the changes", "Mostra le modifiche";
    "upsn.file_removed" => "The file is removed.", "Il file viene rimosso.";
    "upsn.shown_once" => "This password is shown only now: copy it to the client. Later you can only make a new one.", "Questa password viene mostrata solo ora: copiala nel client. In seguito potrai solo crearne una nuova.";
    "upsn.for_ha" => "Home automation and dashboards", "Domotica e pannelli";
    "upsn.for_ha_help" => "Enter these values in the NUT integration of your home automation (Home Assistant: Settings, Devices and services, Network UPS Tools).", "Inserisci questi valori nell'integrazione NUT della tua domotica (Home Assistant: Impostazioni, Dispositivi e servizi, Network UPS Tools).";
    "upsn.host" => "Host", "Host";
    "upsn.port" => "Port", "Porta";
    "upsn.for_upsmon" => "Another Linux machine (upsmon)", "Un altro computer Linux (upsmon)";
    "upsn.for_upsmon_help" => "Add this line to /etc/nut/upsmon.conf on that machine, set MODE=netclient in its nut.conf and restart nut-monitor.", "Aggiungi questa riga a /etc/nut/upsmon.conf su quel computer, imposta MODE=netclient nel suo nut.conf e riavvia nut-monitor.";
    "upsn.for_upsmon_help_pw" => "Add this line to /etc/nut/upsmon.conf on that machine with the account's password in place of PASSWORD, set MODE=netclient in its nut.conf and restart nut-monitor.", "Aggiungi questa riga a /etc/nut/upsmon.conf su quel computer, con la password dell'account al posto di PASSWORD, imposta MODE=netclient nel suo nut.conf e riavvia nut-monitor.";
    "upsn.monitor_note" => "A read-only account cannot shut a machine down: for upsmon add an account that shuts down with the UPS.", "Un account in sola lettura non può spegnere un computer: per upsmon aggiungi un account che si spegne con l'UPS.";
    "upsn.copy" => "Copy", "Copia";
    "upsn.allow_none" => "None: only this NAS", "Nessuno: solo questo NAS";
    "upsn.allow_required" => "Enter at least one allowed host or subnet.", "Inserisci almeno un host o una sottorete ammessi.";
    "cfg.key.ups.netserver" => "Network UPS server", "Server UPS di rete";
    "ps.col_on" => "Enabled", "Attiva";
    "ps.col_action" => "Action", "Azione";
    "ps.col_busy" => "If busy", "Se occupato";
    "ps.startup" => "Startup", "Accensione";
    "ps.shutdown" => "Shutdown", "Spegnimento";
    "ps.every_day" => "Every day", "Tutti i giorni";
    "ps.enabled" => "Rule enabled", "Regola attiva";
    "ps.time_help" => "Local time of the NAS.", "Ora locale del NAS.";
    "ps.checks" => "Before shutting down, check", "Prima di spegnere, controlla";
    "ps.checks_help" => "The NAS is busy when one of these checks finds work in progress.", "Il NAS è occupato quando uno di questi controlli trova lavoro in corso.";
    "ps.check_storage" => "RAID resync or scrub running", "Risincronizzazione RAID o scrub in corso";
    "ps.check_tasks" => "Other tasks running (snapshots, backups, apply)", "Altre attività in corso (snapshot, backup, applicazione)";
    "ps.check_smb" => "SMB clients with open files", "Client SMB con file aperti";
    "ps.when_busy" => "When busy", "Se occupato";
    "ps.busy_wait" => "Wait", "Attendi";
    "ps.busy_skip" => "Skip this shutdown", "Salta questo spegnimento";
    "ps.busy_force" => "Shut down anyway", "Spegni comunque";
    "ps.busy_wait_n" => "Wait up to {n} min", "Attendi fino a {n} min";
    "ps.max_wait" => "Wait up to (minutes)", "Attendi fino a (minuti)";
    "ps.max_wait_help" => "Checked again every minute; then the shutdown is skipped (1–720).", "Ricontrollato ogni minuto; poi lo spegnimento viene saltato (1–720).";
    "ps.v_time" => "Choose a time.", "Scegli un orario.";
    "ps.v_days" => "Choose at least one day.", "Scegli almeno un giorno.";
    "ps.v_same" => "A startup and a shutdown at the same time ({when}).", "Un'accensione e uno spegnimento alla stessa ora ({when}).";
    "ps.v_close" => "The shutdown at {down} comes less than 10 minutes after the startup at {up}.", "Lo spegnimento delle {down} arriva meno di 10 minuti dopo l'accensione delle {up}.";
    "ps.delete_title" => "Delete this rule?", "Eliminare questa regola?";
    "ps.delete_text" => "The rule is removed from the power schedule when the pending changes are applied.", "La regola viene tolta dalla pianificazione quando si applicano le modifiche in sospeso.";
    "ps.next" => "Next power events", "Prossimi eventi di alimentazione";
    "ps.next_startup" => "Next startup", "Prossima accensione";
    "ps.next_shutdown" => "Next shutdown", "Prossimo spegnimento";
    "ps.none" => "None scheduled", "Nessuna pianificata";
    "ps.today" => "today", "oggi";
    "ps.tomorrow" => "tomorrow", "domani";
    "ps.pending_note" => "These times are those of the applied schedule: pending changes count after they are applied.", "Questi orari sono quelli della pianificazione applicata: le modifiche in sospeso valgono dopo l'applicazione.";
    "ps.time_zone" => "Time zone", "Fuso orario";
    "ps.wake_alarm" => "RTC wake alarm", "Sveglia RTC";
    "ps.not_armed" => "Not set", "Non impostata";
    "ps.arm_error" => "The wake alarm could not be set.", "Non è stato possibile impostare la sveglia.";
    "ps.ww.no_rtc" => "No real-time clock found: the NAS cannot start itself on schedule.", "Nessun orologio di sistema trovato: il NAS non può accendersi da solo in base alla pianificazione.";
    "ps.ww.no_wake_alarm" => "The clock {rtc} has no wake alarm: the NAS cannot start itself on schedule.", "L'orologio {rtc} non ha una sveglia: il NAS non può accendersi da solo in base alla pianificazione.";
    "ps.wake_unsupported" => "Not supported here", "Non supportata qui";
    "ps.wake_unsupported_help" => "This machine has no RTC wake alarm: startup rules cannot start it. Shutdown rules still work.", "Questa macchina non ha una sveglia RTC: le regole di accensione non possono avviarla. Le regole di spegnimento funzionano comunque.";
    "ps.prev_clean" => "Previous shutdown clean", "Spegnimento precedente pulito";
    "ps.clean_yes" => "Yes", "Sì";
    "ps.clean_no" => "No (power loss or crash)", "No (mancanza di corrente o blocco)";
    "ps.cause_scheduled" => "Scheduled startup", "Accensione pianificata";
    "ps.cause_watchdog" => "Watchdog reset", "Reset del watchdog";
    "ps.cause_unknown" => "Button, Wake-on-LAN or power return", "Pulsante, Wake-on-LAN o ritorno corrente";
    // storage
    "sto.title" => "Storage", "Archiviazione";
    "sto.subtitle" => "Drives, RAID arrays and volumes", "Dischi, array RAID e volumi";
    "sto.chassis" => "Chassis", "Chassis";
    "sto.disks" => "Drives", "Dischi";
    "sto.filesystems" => "Volumes", "Volumi";
    "sto.bay" => "Bay", "Alloggiamento";
    "sto.empty_bay" => "Empty", "Vuoto";
    "sto.bays" => "Drive bays", "Alloggiamenti dei dischi";
    "sto.m2" => "M.2", "M.2";
    "sto.m2_slots" => "M.2 NVMe slots", "Slot M.2 NVMe";
    "sto.asleep" => "Standby", "In standby";
    "sto.active" => "Active", "Attivo";
    "health.unknown" => "Unknown", "Sconosciuto";
    "sto.not_powered" => "Powered off", "Spento";
    "sto.name" => "Name", "Nome";
    "sto.model" => "Model", "Modello";
    "sto.serial" => "Serial number", "Numero di serie";
    "sto.size" => "Capacity", "Capacità";
    "sto.type" => "Type", "Tipo";
    "sto.members_n" => "members", "membri";
    "sto.level" => "Level", "Livello";
    "sto.mount" => "Mount point", "Punto di montaggio";
    "sto.fs" => "File system", "File system";
    "sto.usage" => "Usage", "Utilizzo";
    "sto.ssd" => "SSD", "SSD";
    "sto.hdd" => "HDD", "HDD";
    "sto.no_disks" => "No drives detected", "Nessun disco rilevato";
    "sto.generic" => "Detected drives", "Dischi rilevati";
    "sto.sync" => "Resync", "Sincronizzazione";
    // storage manager (pages/storage)
    "confirm.type_name" => "Type {name} to confirm", "Digita {name} per confermare";
    "plan.destructive" => "Erases data", "Cancella dati";
    "plan.erases" => "Erases {dev}", "Cancella {dev}";
    "plan.protection" => "Redundancy", "Ridondanza";
    "plan.raw" => "Raw capacity", "Capacità grezza";
    "plan.steps" => "{n} steps", "{n} passaggi";
    "plan.tol0" => "No drive may fail", "Nessun disco può guastarsi";
    "plan.tol1" => "1 drive may fail", "Può guastarsi 1 disco";
    "plan.toln" => "{n} drives may fail", "Possono guastarsi {n} dischi";
    "plan.tolerance" => "Fault tolerance", "Tolleranza ai guasti";
    "plan.usable" => "Usable", "Utilizzabile";
    "plan.wasted" => "Unused", "Non utilizzato";
    "plan.k.array" => "Array", "Array";
    "plan.k.filesystem" => "File system", "File system";
    "plan.k.filesystem_device" => "File system device", "Dispositivo del file system";
    "plan.k.from" => "From", "Da";
    "plan.k.to" => "To", "A";
    "plan.k.level" => "Level", "Livello";
    "plan.k.mapper" => "Encrypted device", "Dispositivo cifrato";
    "plan.k.md" => "RAID device", "Dispositivo RAID";
    "plan.k.mountpoint" => "Mount point", "Punto di montaggio";
    "plan.k.old" => "Replaced member", "Membro sostituito";
    "plan.k.raid_devices" => "RAID members", "Membri RAID";
    // storage plan texts by code: errors (plan.e.<PlanIssueCode>), warnings
    // (plan.w.<PlanWarningCode>), steps (plan.s.<PlanStepKind>); {subject}, {other}, {items},
    // {size}, {n} and {level} come from the plan
    "plan.e.invalid_label" => "Invalid name: use 1-16 lowercase letters, digits or dashes, starting with a letter.", "Nome non valido: usa da 1 a 16 lettere minuscole, cifre o trattini, iniziando con una lettera.";
    "plan.e.label_taken" => "A volume, array or mount point with this name already exists.", "Esiste già un volume, un array o un punto di montaggio con questo nome.";
    "plan.e.disk_selected_twice" => "{subject}: selected twice", "{subject}: selezionato due volte";
    "plan.e.disk_count" => "Wrong number of drives for this RAID level", "Numero di dischi non adatto a questo livello RAID";
    "plan.e.no_disks" => "Select at least one drive", "Seleziona almeno un disco";
    "plan.e.no_such_disk" => "{subject}: drive not found", "{subject}: disco non trovato";
    "plan.e.system_disk" => "{subject}: holds the operating system", "{subject}: contiene il sistema operativo";
    "plan.e.disk_in_use" => "{subject}: in use by an array, a volume or a mounted file system", "{subject}: in uso da un array, un volume o un file system montato";
    "plan.e.disk_failing" => "{subject}: SMART reports that the drive is failing", "{subject}: secondo SMART il disco si sta guastando";
    "plan.e.disk_too_small" => "{subject}: too small", "{subject}: troppo piccolo";
    "plan.e.no_such_array" => "{subject}: array not found", "{subject}: array non trovato";
    "plan.e.array_not_healthy" => "{subject} is not healthy: repair it first", "{subject} non è integro: va prima riparato";
    "plan.e.array_busy" => "{subject} is busy (resync, check or reshape): wait for it to finish", "{subject} è occupato (risincronizzazione, controllo o rimodellamento): attendi che finisca";
    "plan.e.level_unsupported" => "{subject}: rynas cannot grow this RAID level", "{subject}: rynas non può espandere questo livello RAID";
    "plan.e.level_change_unsupported" => "This level change is not supported (supported: add mirrors, expand RAID 5/6, 2-drive RAID 1 to RAID 5, RAID 5 to RAID 6)", "Questo cambio di livello non è supportato (supportati: aggiunta di mirror, espansione di RAID 5/6, da RAID 1 a 2 dischi a RAID 5, da RAID 5 a RAID 6)";
    "plan.e.not_a_member" => "{subject} is not a member of the array", "{subject} non è un membro dell'array";
    "plan.e.no_redundancy" => "{subject} has no redundancy: a member cannot be replaced", "{subject} non ha ridondanza: un membro non si può sostituire";
    "plan.e.no_healthy_member" => "{subject}: no healthy member left to rebuild from", "{subject}: nessun membro integro da cui ricostruire";
    "plan.e.no_such_filesystem" => "{subject}: btrfs file system not found", "{subject}: file system btrfs non trovato";
    "plan.e.not_mounted" => "{subject}: not mounted, it cannot be scrubbed", "{subject}: non montato, la pulizia dati non è possibile";
    "plan.e.scrub_running" => "A scrub is already running on this file system", "Una pulizia dati è già in corso su questo file system";
    "plan.w.data_erased" => "{subject}: the data on it will be erased ({items})", "{subject}: i dati presenti verranno cancellati ({items})";
    "plan.w.smart_warnings" => "{subject}: SMART warnings ({items})", "{subject}: avvisi SMART ({items})";
    "plan.w.no_smart_data" => "{subject}: no SMART data yet", "{subject}: ancora nessun dato SMART";
    "plan.w.removable_disk" => "{subject}: USB or removable drive: unplugging it breaks the volume", "{subject}: disco USB o rimovibile: se viene scollegato, il volume smette di funzionare";
    "plan.w.mixed_sizes" => "Drives of different sizes: each one is used up to the size of the smallest, {size} stay unused", "Dischi di dimensioni diverse: ognuno viene usato fino alla dimensione del più piccolo, {size} restano inutilizzati";
    "plan.w.mixed_kinds" => "Mixed drive types (HDD, SSD, NVMe): the array runs at the speed of the slowest", "Tipi di disco misti (HDD, SSD, NVMe): l'array va alla velocità del più lento";
    "plan.w.smr_raid" => "SMR drives ({items}): a rebuild can take days or fail; prefer CMR drives for RAID", "Dischi SMR ({items}): una ricostruzione può durare giorni o non riuscire; per il RAID meglio dischi CMR";
    "plan.w.smr_slow_writes" => "SMR drives ({items}): long writes are slow", "Dischi SMR ({items}): le scritture prolungate sono lente";
    "plan.w.single_no_redundancy" => "No redundancy: if the drive fails, the volume is lost (a mirror drive can be added later)", "Nessuna ridondanza: se il disco si guasta, il volume va perso (un disco mirror si può aggiungere in seguito)";
    "plan.w.raid0_no_redundancy" => "RAID 0 has no redundancy: any failed drive loses the whole volume", "RAID 0 non ha ridondanza: un qualsiasi disco guasto fa perdere l'intero volume";
    "plan.w.raid5_rebuild" => "RAID 5 with large or many HDDs: a second failure during the long rebuild loses the volume; consider RAID 6", "RAID 5 con HDD grandi o numerosi: un secondo guasto durante la lunga ricostruzione fa perdere il volume; valuta il RAID 6";
    "plan.w.encrypted" => "Encrypted: after every restart the volume stays locked until it is unlocked in rynas; keep the recovery key and the header backup outside the NAS", "Crittografato: dopo ogni riavvio il volume resta bloccato finché non lo sblocchi da rynas; conserva la chiave di ripristino e il backup dell'intestazione fuori dal NAS";
    "plan.w.compression_off" => "Compression off: zstd level 1 costs little CPU and usually saves space", "Compressione disattivata: zstd livello 1 usa poca CPU e di solito fa risparmiare spazio";
    "plan.w.ssd_trim_off" => "SSD with encryption: TRIM is off because it reveals which blocks are free; enable 'discard' in crypttab if that is acceptable", "SSD con crittografia: il TRIM è disattivato perché rivela quali blocchi sono liberi; attiva 'discard' in crypttab se è accettabile";
    "plan.w.disk_larger_than_members" => "{subject} is larger than the members: the extra space stays unused until every member is replaced", "{subject} è più grande dei membri: lo spazio in più resta inutilizzato finché non vengono sostituiti tutti i membri";
    "plan.w.grow_filesystem_by_hand" => "No mounted btrfs on this array: grow the file system by hand", "Nessun btrfs montato su questo array: espandi il file system a mano";
    "plan.w.spare_member" => "{subject} is a spare: it can simply be removed", "{subject} è una riserva: basta rimuoverla";
    "plan.w.smr_slow_rebuild" => "{subject}: SMR drive, the rebuild may be very slow", "{subject}: disco SMR, la ricostruzione può essere molto lenta";
    "plan.w.other_partitions" => "{subject} has {n} partitions: only the RAID members are rebuilt, copy the others (EFI, swap) separately", "{subject} ha {n} partizioni: vengono ricostruiti solo i membri RAID, copia a parte le altre (EFI, swap)";
    "plan.w.array_not_healthy" => "{subject} is not healthy: without redundancy a scrub finds errors but cannot repair them", "{subject} non è integro: senza ridondanza la pulizia dati trova gli errori ma non può ripararli";
    "plan.w.scrub_cost" => "A scrub reads every allocated block: it takes hours on large HDD volumes and wakes sleeping drives; it never runs together with replication, balance or deduplication on the same file system", "La pulizia dati legge ogni blocco allocato: su grandi volumi HDD dura ore e risveglia i dischi in standby; non viene mai eseguita insieme a replica, bilanciamento o deduplicazione sullo stesso file system";
    "plan.s.wipe_signatures" => "Erase the signatures on {subject}", "Cancella le firme su {subject}";
    "plan.s.clear_partition_table" => "Clear the partition table of {subject}", "Azzera la tabella delle partizioni di {subject}";
    "plan.s.create_partition" => "Create one RAID partition on {subject}, leaving 128 MiB free at the end", "Crea una partizione RAID su {subject}, lasciando 128 MiB liberi alla fine";
    "plan.s.wait_udev" => "Wait for the partitions to appear", "Attendi che compaiano le partizioni";
    "plan.s.create_array" => "Create the RAID array ({level}, drives: {n})", "Crea l'array RAID ({level}, dischi: {n})";
    "plan.s.record_array" => "Record the array so it is assembled at boot", "Registra l'array perché venga assemblato all'avvio";
    "plan.s.luks_format" => "Format the array as LUKS2 (Argon2id, 256 MiB of memory, 4 KiB sectors)", "Formatta l'array come LUKS2 (Argon2id, 256 MiB di memoria, settori da 4 KiB)";
    "plan.s.add_recovery_key" => "Add a recovery key (shown once: save it before going on)", "Aggiungi una chiave di ripristino (mostrata una volta: salvala prima di proseguire)";
    "plan.s.backup_luks_header" => "Back up the LUKS header (offered for download, then removed from the NAS)", "Salva l'intestazione LUKS (offerta da scaricare, poi rimossa dal NAS)";
    "plan.s.unlock_volume" => "Unlock the new volume", "Sblocca il nuovo volume";
    "plan.s.declare_crypttab" => "Declare the encrypted volume, locked at boot", "Dichiara il volume crittografato, bloccato all'avvio";
    "plan.s.create_filesystem" => "Create the btrfs file system (data single, metadata DUP)", "Crea il file system btrfs (dati single, metadati DUP)";
    "plan.s.mount_at_boot" => "Mount the volume at boot", "Monta il volume all'avvio";
    "plan.s.create_mount_point" => "Create the mount point {subject}", "Crea il punto di montaggio {subject}";
    "plan.s.mount_volume" => "Mount the volume at {subject}", "Monta il volume in {subject}";
    "plan.s.create_snapshot_subvolume" => "Create the snapshot subvolume (hidden from shares)", "Crea il sottovolume degli snapshot (nascosto alle cartelle condivise)";
    "plan.s.create_cache_subvolume" => "Create the cache subvolume (excluded from snapshots)", "Crea il sottovolume della cache (escluso dagli snapshot)";
    "plan.s.disable_cow_cache" => "Turn off copy-on-write on the cache subvolume", "Disattiva il copy-on-write sul sottovolume della cache";
    "plan.s.mirror_to_raid5" => "Convert the 2-drive mirror to RAID 5 (instant, same data layout)", "Converti il mirror a 2 dischi in RAID 5 (immediato, stessa disposizione dei dati)";
    "plan.s.add_spares" => "Add the new partitions as spares", "Aggiungi le nuove partizioni come riserve";
    "plan.s.reshape" => "Reshape the array onto the new drives (hours to days; the volume stays online)", "Rimodella l'array sui nuovi dischi (da ore a giorni; il volume resta disponibile)";
    "plan.s.spares_to_mirrors" => "Turn the spares into active mirrors (resync)", "Trasforma le riserve in mirror attivi (risincronizzazione)";
    "plan.s.wait_reshape" => "Wait for the reshape to finish", "Attendi la fine del rimodellamento";
    "plan.s.wait_resync" => "Wait for the resync to finish", "Attendi la fine della risincronizzazione";
    "plan.s.grow_luks" => "Grow the encrypted device to the new array size", "Espandi il dispositivo cifrato alla nuova dimensione dell'array";
    "plan.s.grow_filesystem" => "Grow the file system to the new size", "Espandi il file system alla nuova dimensione";
    "plan.s.copy_partition_table" => "Copy the partition table of {other} to {subject}", "Copia la tabella delle partizioni di {other} su {subject}";
    "plan.s.randomize_guids" => "Give {subject} its own partition IDs", "Assegna a {subject} identificativi di partizione propri";
    "plan.s.add_spare" => "Add the new partition as a spare", "Aggiungi la nuova partizione come riserva";
    "plan.s.replace_copy" => "Copy {subject} onto the new drive while it stays in the array", "Copia {subject} sul nuovo disco mentre resta nell'array";
    "plan.s.wait_copy" => "Wait for the copy to finish", "Attendi la fine della copia";
    "plan.s.remove_replaced" => "Remove {subject} (marked faulty by the replacement)", "Rimuovi {subject} (segnato come guasto dalla sostituzione)";
    "plan.s.mark_failed" => "Mark {subject} as failed", "Segna {subject} come guasto";
    "plan.s.remove_member" => "Remove {subject} from the array", "Rimuovi {subject} dall'array";
    "plan.s.add_for_rebuild" => "Add the new partition: the rebuild starts", "Aggiungi la nuova partizione: parte la ricostruzione";
    "plan.s.wait_rebuild" => "Wait for the rebuild to finish", "Attendi la fine della ricostruzione";
    "plan.s.scrub" => "Verify every checksum and repair from a good copy", "Verifica ogni checksum e ripara da una copia integra";
    "plan.s.check_array" => "Check the parity or mirrors of {subject} (read-only)", "Controlla la parità o i mirror di {subject} (sola lettura)";
    "plan.s.wait_check" => "Wait for the check of {subject}, then read the mismatch count", "Attendi la fine del controllo di {subject}, poi leggi il numero di discrepanze";
    "sto.tab.overview" => "Overview", "Panoramica";
    "sto.create" => "Create volume", "Crea volume";
    "sto.refresh_failed" => "Couldn't refresh the storage data.", "Impossibile aggiornare i dati di archiviazione.";
    "sto.no_volumes" => "No volumes yet", "Ancora nessun volume";
    "sto.no_volumes_text" => "Create a volume on the free drives to start storing data.", "Crea un volume sui dischi liberi per iniziare ad archiviare dati.";
    "sto.free_disks" => "Drives available: {n}. Use them for a new volume or to grow one.", "Dischi disponibili: {n}. Usali per un nuovo volume o per espanderne uno.";
    "wiz.erase_chip" => "Will be erased", "Verrà cancellato";
    "wiz.will_erase" => "Existing data will be erased: {what}", "I dati esistenti verranno cancellati: {what}";
    "sto.n_disks" => "Drives: {n}", "Dischi: {n}";
    "sto.used_n" => "Used: {n}", "Usato: {n}";
    "sto.encrypted" => "Encrypted", "Crittografato";
    "sto.encryption" => "Encryption", "Crittografia";
    "sto.locked" => "Locked", "Bloccato";
    "sto.unlocked" => "Unlocked", "Sbloccato";
    "sto.locked_text" => "Encrypted volume: unlock it with the passphrase or the recovery key to use its data.", "Volume crittografato: sbloccalo con la passphrase o la chiave di ripristino per usarne i dati.";
    "sto.crypt_text" => "LUKS2 encryption. Keep the recovery key and a header backup outside the NAS.", "Crittografia LUKS2. Conserva la chiave di ripristino e un backup dell'intestazione fuori dal NAS.";
    "sto.unlock" => "Unlock", "Sblocca";
    "sto.lock" => "Lock", "Blocca";
    "sto.unlock_title" => "Unlock {name}", "Sblocca {name}";
    "sto.unlock_text" => "The volume is mounted again and the shares on it come back.", "Il volume viene rimontato e le cartelle condivise su di esso tornano disponibili.";
    "sto.pass_or_key" => "Passphrase or recovery key", "Passphrase o chiave di ripristino";
    "sto.lock_title" => "Lock {name}", "Blocca {name}";
    "sto.lock_text" => "The volume is unmounted and its key removed from memory. Shares and apps on it stop until it is unlocked again.", "Il volume viene smontato e la sua chiave tolta dalla memoria. Cartelle condivise e app su di esso si fermano fino al prossimo sblocco.";
    "sto.passphrase" => "Passphrase", "Passphrase";
    "sto.stale" => "Cached", "In cache";
    "sto.stale_hint" => "A drive below is in standby: details come from the last reading, so it isn't woken up.", "Un disco sottostante è in standby: i dettagli vengono dall'ultima lettura, per non risvegliarlo.";
    "sto.scrub_running" => "Scrubbing", "Pulizia dati in corso";
    "health.healthy" => "Healthy", "Integro";
    "sto.h.warning" => "Warning", "Avviso";
    "sto.h.failing" => "Failing", "In avaria";
    "sto.h.no_smart" => "Not available", "Non disponibile";
    "sto.h.no_smart_hint" => "This device has no SMART (or its USB adapter does not pass it on): its health cannot be read.", "Questo dispositivo non ha SMART (o l'adattatore USB non lo trasmette): non se ne può leggere lo stato di salute.";
    "health.degraded" => "Degraded", "Degradato";
    "sto.a.failed" => "Crashed", "Arrestato";
    "sto.a.inactive" => "Inactive", "Inattivo";
    "sto.p.idle" => "Idle", "Inattivo";
    "sto.p.sleep" => "Sleep", "Sospensione";
    "sto.nvme" => "NVMe", "NVMe";
    "sto.lvl.single" => "Single drive", "Disco singolo";
    "sto.lvl.raid0" => "RAID 0", "RAID 0";
    "sto.lvl.raid1" => "RAID 1", "RAID 1";
    "sto.lvl.raid5" => "RAID 5", "RAID 5";
    "sto.lvl.raid6" => "RAID 6", "RAID 6";
    "sto.lvl.raid10" => "RAID 10", "RAID 10";
    "sto.sync.resync" => "Resync", "Sincronizzazione";
    "sto.sync.recovery" => "Rebuilding", "Ricostruzione";
    "sto.sync.check" => "Parity check", "Controllo parità";
    "sto.sync.repair" => "Repair", "Riparazione";
    "sto.sync.reshape" => "Reshaping", "Rimodellamento";
    "sto.sync.waiting" => "Waiting", "In attesa";
    "sto.drive" => "Drive", "Disco";
    "sto.temp" => "Temperature", "Temperatura";
    "sto.temp_short" => "Temp.", "Temp.";
    "sto.health" => "Health", "Integrità";
    "sto.used_by" => "Used by", "Usato da";
    "sto.unused" => "Not in use", "Non in uso";
    "sto.smr_hint" => "Shingled (SMR) drive: slow rewrites, rebuilds of RAID 5/6 can take days.", "Disco shingled (SMR): riscritture lente, la ricostruzione di RAID 5/6 può durare giorni.";
    "sto.smart" => "S.M.A.R.T.", "S.M.A.R.T.";
    "sto.settings" => "Settings", "Impostazioni";
    "sto.standby_note" => "This drive is in standby. Its details are not read automatically, so it isn't woken up.", "Questo disco è in standby. I dettagli non vengono letti in automatico, per non risvegliarlo.";
    "sto.read_now" => "Read now", "Leggi ora";
    "sto.wwn" => "WWN", "WWN";
    "sto.firmware" => "Firmware", "Firmware";
    "sto.rpm" => "Rotation speed", "Velocità di rotazione";
    "sto.poh" => "Power-on time", "Tempo di accensione";
    "sto.smart_age" => "S.M.A.R.T. read", "Lettura S.M.A.R.T.";
    "sto.ago" => "{t} ago", "{t} fa";
    "sto.partitions" => "Partitions", "Partizioni";
    "sto.no_smart" => "No S.M.A.R.T. data for this drive.", "Nessun dato S.M.A.R.T. per questo disco.";
    "sto.smart_passed" => "Passed", "Superato";
    "sto.smart_failed" => "Failed", "Non superato";
    "sto.smart_verdict" => "Drive self-assessment", "Autovalutazione del disco";
    "sto.power_cycles" => "Power cycles", "Cicli di accensione";
    "sto.life" => "Life remaining", "Vita residua";
    "sto.written" => "Data written", "Dati scritti";
    "sto.error_log" => "Error log entries", "Voci del registro errori";
    "sto.selftest_running" => "Self-test running", "Autotest in corso";
    "sto.pct_left" => "{p}% left", "{p}% rimanente";
    "sto.read_at" => "Read at", "Letto il";
    "sto.nvme_health" => "NVMe health", "Integrità NVMe";
    "sto.nv_spare" => "Available spare", "Riserva disponibile";
    "sto.nv_used" => "Endurance used", "Resistenza consumata";
    "sto.nv_media" => "Media errors", "Errori del supporto";
    "sto.nv_unsafe" => "Unsafe shutdowns", "Spegnimenti non sicuri";
    "sto.nv_warn" => "Minutes over warning / critical temperature", "Minuti oltre temperatura di avviso / critica";
    "sto.nv_crit" => "Critical warning flags", "Flag di avviso critico";
    "sto.attributes" => "Attributes", "Attributi";
    "sto.a_value" => "Value", "Valore";
    "sto.a_worst" => "Worst", "Peggiore";
    "sto.a_thresh" => "Threshold", "Soglia";
    "sto.a_raw" => "Raw", "Grezzo";
    "sto.a_failing" => "Failing now", "In errore ora";
    "sto.a_failed_past" => "Failed in the past", "In errore in passato";
    "sto.a_watch" => "Watch", "Da controllare";
    "sto.a_ok" => "OK", "OK";
    "sto.selftests" => "Self-test log", "Registro autotest";
    "sto.at_hours" => "At hour", "All'ora";
    "sto.recommended" => "Recommended for this drive type", "Consigliate per questo tipo di disco";
    "sto.chk_ok" => "As recommended", "Come consigliato";
    "sto.chk_differs" => "Differs", "Diversa";
    "sto.chk_na" => "Not applicable", "Non applicabile";
    "sto.current" => "Current", "Attuale";
    "sto.recommended_v" => "recommended", "consigliato";
    "sto.overrides" => "Settings of this drive", "Impostazioni di questo disco";
    "sto.pending_note" => "A change is staged: apply it from the pending changes.", "C'è una modifica in sospeso: applicala dalle modifiche in sospeso.";
    "sto.kernel_now" => "In use now: scheduler {s}, read-ahead {r} KiB.", "In uso ora: scheduler {s}, read-ahead {r} KiB.";
    "sto.scheduler" => "I/O scheduler", "Scheduler I/O";
    "sto.default_v" => "Default ({v})", "Predefinito ({v})";
    "sto.read_ahead" => "Read-ahead (KiB)", "Read-ahead (KiB)";
    "sto.ra_invalid" => "Read-ahead must be a number from 0 to 65536.", "Il read-ahead deve essere un numero da 0 a 65536.";
    "sto.standby" => "Spin down after", "Standby dopo";
    "sto.standby_hint" => "Off unless you choose it: spinning down pays only for disks idle for hours, such as archive or backup disks.", "Spento finché non lo scegli: fermare il disco conviene solo se resta inattivo per ore, come un disco di archivio o di backup.";
    "sto.hdd_only" => "Hard disks only.", "Solo dischi rigidi.";
    "sto.not_set" => "Not set (leave the drive alone)", "Non impostato (lascia il disco com'è)";
    "sto.never" => "Never", "Mai";
    "sto.apm" => "Power management (APM)", "Gestione energetica (APM)";
    // labels of the recommended-settings checks by `SettingCheck.key` (scheduler, read-ahead, APM and
    // temperature reuse sto.scheduler, sto.read_ahead, sto.apm, sto.temp)
    "sto.chk.write_cache" => "Write cache", "Cache di scrittura";
    "sto.chk.trim" => "TRIM", "TRIM";
    "sto.chk.alpm" => "SATA link power (ALPM)", "Risparmio energetico del collegamento SATA (ALPM)";
    "sto.chk.apst" => "NVMe power states (APST)", "Stati di alimentazione NVMe (APST)";
    "sto.apm_254" => "254 · Recommended: no head parking, no spin-down by the drive", "254 · Consigliato: niente parcheggio delle testine né standby deciso dal disco";
    "sto.apm_255" => "255 · APM off (also fine)", "255 · APM disattivato (va bene anche così)";
    "sto.apm_128" => "128 · Saves some power, no spin-down by the drive", "128 · Un po' di risparmio, niente standby deciso dal disco";
    "sto.apm_127" => "127 · The drive may park its heads and spin down by itself", "127 · Il disco può parcheggiare le testine e fermarsi da solo";
    "sto.apm_1" => "1 · Most power saving: frequent head parking, wears the drive", "1 · Massimo risparmio: parcheggi frequenti delle testine, usura il disco";
    "sto.reset_defaults" => "Use defaults", "Usa predefiniti";
    "sto.done_ok" => "Completed.", "Completato.";
    "sto.done_err" => "The operation failed.", "L'operazione non è riuscita.";
    "sto.vol_missing" => "Volume not found", "Volume non trovato";
    "sto.vol_missing_text" => "It may have been renamed or removed.", "Potrebbe essere stato rinominato o rimosso.";
    "sto.back_storage" => "Back to Storage", "Torna ad Archiviazione";
    "sto.scrub" => "Data scrubbing", "Pulizia dati";
    "sto.grow" => "Add drives", "Aggiungi dischi";
    "sto.replace" => "Replace drive", "Sostituisci disco";
    "sto.unmanaged" => "This volume wasn't created by rynas: it is shown, but not changed here.", "Questo volume non è stato creato da rynas: viene mostrato, ma non modificato qui.";
    "sto.used" => "Used", "Usato";
    "sto.free" => "Free (estimated)", "Libero (stimato)";
    "sto.of" => "of {n}", "di {n}";
    "sto.profiles" => "Profiles", "Profili";
    "sto.prof.data" => "Data", "Dati";
    "sto.prof.metadata" => "Metadata", "Metadati";
    "sto.compression" => "Compression", "Compressione";
    "sto.quota" => "Quota", "Quota";
    "sto.off" => "Off", "Disattivata";
    "sto.raid" => "RAID", "RAID";
    "sto.md_state" => "md state", "Stato md";
    "sto.array" => "Array", "Array";
    "sto.mismatch" => "Mismatched blocks (last check)", "Blocchi non corrispondenti (ultimo controllo)";
    "sto.r.active" => "Active", "Attivo";
    "sto.r.spare" => "Spare", "Riserva";
    "sto.r.faulty" => "Faulty", "Guasto";
    "sto.r.rebuilding" => "Rebuilding", "In ricostruzione";
    "sto.r.journal" => "Journal", "Journal";
    "sto.missing_members" => "Missing members: {n}", "Membri mancanti: {n}";
    "sto.scrub_errors" => "Errors found", "Errori trovati";
    "sto.scrub_finished" => "No errors", "Nessun errore";
    "sto.scrub_aborted" => "Interrupted", "Interrotta";
    "never_run" => "Never run", "Mai eseguita";
    "sto.scrub_text" => "Reads every block, checks the checksums and repairs from the redundant copy; the RAID parity check verifies the array too.", "Legge ogni blocco, verifica i checksum e ripara dalla copia ridondante; il controllo di parità verifica anche l'array RAID.";
    "sto.started" => "Started", "Avviata";
    "sto.duration" => "Duration", "Durata";
    "sto.progress" => "Progress", "Avanzamento";
    "sto.scrub_result" => "Result", "Risultato";
    "sto.scrub_stop" => "Stop scrubbing", "Interrompi pulizia";
    "run_now" => "Run now", "Esegui ora";
    "sto.scrub_title" => "Data scrubbing on {name}", "Pulizia dati su {name}";
    "sto.scrub_start" => "Start scrubbing", "Avvia pulizia";
    "sto.md_check" => "Also check the RAID parity", "Controlla anche la parità RAID";
    "sto.review_plan" => "Review plan", "Rivedi il piano";
    "sto.hdr_saved" => "Header backup downloaded. Keep it outside the NAS, then delete it from the server.", "Backup dell'intestazione scaricato. Conservalo fuori dal NAS, poi eliminalo dal server.";
    "sto.hdr_none" => "No header backup is waiting on the server (already deleted, or made before the last restart).", "Nessun backup dell'intestazione in attesa sul server (già eliminato o precedente all'ultimo riavvio).";
    "sto.hdr_deleted" => "Header backup deleted from the server.", "Backup dell'intestazione eliminato dal server.";
    "sto.hdr_download" => "Download header backup", "Scarica backup intestazione";
    "sto.hdr_delete" => "Delete from server", "Elimina dal server";
    "sto.rk_reveal" => "Show recovery key", "Mostra chiave di ripristino";
    "sto.rk_title" => "Recovery key of {name}", "Chiave di ripristino di {name}";
    "sto.rk_text" => "The recovery key opens the volume when the passphrase is lost. It can be shown only once, within an hour of the creation.", "La chiave di ripristino apre il volume se la passphrase va persa. Può essere mostrata una sola volta, entro un'ora dalla creazione.";
    "sto.rk_gone" => "The recovery key is no longer available: it was already shown or more than an hour has passed.", "La chiave di ripristino non è più disponibile: è già stata mostrata o è passata più di un'ora.";
    "sto.rk_once" => "This is the only time the key is shown. Store it somewhere safe outside the NAS.", "È l'unica volta che la chiave viene mostrata. Conservala in un posto sicuro fuori dal NAS.";
    "sto.rk_stored" => "I have stored the recovery key", "Ho conservato la chiave di ripristino";
    "sto.copy" => "Copy", "Copia";
    "sto.download" => "Download", "Scarica";
    "sto.done" => "Done", "Fine";
    "sto.no_free" => "No drive available: add a drive that is not in use.", "Nessun disco disponibile: aggiungi un disco non in uso.";
    "sto.keep_level" => "Keep {l}", "Mantieni {l}";
    "sto.grow_title" => "Add drives to {name}", "Aggiungi dischi a {name}";
    "sto.grow_text" => "The new drives join the array (more capacity or more redundancy); a level change reshapes it, which can take many hours.", "I nuovi dischi entrano nell'array (più capacità o più ridondanza); un cambio di livello lo rimodella e può richiedere molte ore.";
    "sto.replace_title" => "Replace a drive of {name}", "Sostituisci un disco di {name}";
    "sto.replace_text" => "The new drive is rebuilt from the others; the old one leaves the array when the rebuild ends.", "Il nuovo disco viene ricostruito dagli altri; quello vecchio esce dall'array a ricostruzione finita.";
    "sto.replace_old" => "Drive to replace", "Disco da sostituire";
    "sto.replace_new" => "New drive", "Nuovo disco";
    "sto.replace_go" => "Replace", "Sostituisci";
    "sto.subvolumes" => "Subvolumes", "Sottovolumi";
    "sto.sub_create" => "Create subvolume", "Crea sottovolume";
    "sto.sub_none" => "No subvolumes yet. Shared folders get their own when they are created.", "Ancora nessun sottovolume. Le cartelle condivise ne ricevono uno quando vengono create.";
    "sto.path" => "Path", "Percorso";
    "sto.readonly" => "Read-only", "Sola lettura";
    "sto.inherit" => "Volume default", "Predefinita del volume";
    "sto.system" => "System", "Sistema";
    "sto.yes" => "Yes", "Sì";
    "sto.no" => "No", "No";
    "sto.sub_invalid" => "Use letters, digits, dot, underscore or dash (up to 64), starting with a letter or digit.", "Usa lettere, cifre, punto, trattino basso o trattino (fino a 64), iniziando con una lettera o una cifra.";
    "edit_named" => "Edit {name}", "Modifica {name}";
    "sto.sub_name_hint" => "Letters, digits, . _ - (up to 64).", "Lettere, cifre, . _ - (fino a 64).";
    "sto.sub_compress" => "Compression (zstd)", "Compressione (zstd)";
    "sto.sub_compress_hint" => "Turn it off for data that is already compressed: video, photos, backups, VM images.", "Disattivala per dati già compressi: video, foto, backup, immagini di VM.";
    "sto.sub_delete" => "Delete {name}", "Elimina {name}";
    "sto.sub_delete_text" => "The subvolume and every file in it are deleted. This cannot be undone.", "Il sottovolume e tutti i file che contiene vengono eliminati. L'operazione non può essere annullata.";
    "sto.tuning" => "RAID tuning", "Ottimizzazione RAID";
    "sto.stripe_text" => "A larger stripe cache speeds up RAID 5/6 writes and rebuilds at the cost of memory.", "Una stripe cache più grande accelera scritture e ricostruzioni RAID 5/6 a spese della memoria.";
    "sto.proposal" => "rynas proposal", "Proposta di rynas";
    "sto.stripe" => "Stripe cache (entries)", "Stripe cache (voci)";
    "sto.kernel_default" => "Kernel default (256)", "Predefinito del kernel (256)";
    "sto.use_proposal" => "{n} (proposal)", "{n} (proposta)";
    "wiz.subtitle" => "Drives, RAID level and options, with the plan shown before anything changes", "Dischi, livello RAID e opzioni, con il piano mostrato prima di ogni modifica";
    "wiz.steps" => "Steps", "Passaggi";
    "wiz.s_disks" => "Drives", "Dischi";
    "wiz.s_level" => "RAID level", "Livello RAID";
    "wiz.s_options" => "Options", "Opzioni";
    "wiz.s_review" => "Review", "Riepilogo";
    "wiz.next" => "Next", "Avanti";
    "wiz.pick_disks" => "Choose the drives", "Scegli i dischi";
    "wiz.pick_text" => "Only drives without data are listed. Everything on the chosen drives will be erased.", "Sono elencati solo i dischi senza dati. Tutto ciò che c'è sui dischi scelti verrà cancellato.";
    "wiz.in_use" => "Drives in use, not listed: {n}.", "Dischi in uso, non elencati: {n}.";
    "wiz.selected" => "Selected: {n} · {size} raw", "Selezionati: {n} · {size} grezzi";
    "wiz.pick_level" => "Choose the RAID level", "Scegli il livello RAID";
    "wiz.recommended" => "Recommended", "Consigliato";
    "wiz.min_single" => "Exactly 1 drive", "Esattamente 1 disco";
    "wiz.min2" => "Needs 2 or more drives", "Servono almeno 2 dischi";
    "wiz.min3" => "Needs 3 or more drives", "Servono almeno 3 dischi";
    "wiz.min4" => "Needs 4 or more drives", "Servono almeno 4 dischi";
    "wiz.min_even4" => "Needs an even number of drives, 4 or more", "Serve un numero pari di dischi, almeno 4";
    "wiz.d_single" => "One drive, no redundancy; a mirror can be added later.", "Un disco, senza ridondanza; un mirror si può aggiungere in seguito.";
    "wiz.d_raid0" => "Striping for speed: one failed drive loses everything.", "Striping per velocità: un disco guasto fa perdere tutto.";
    "wiz.d_raid1" => "Mirror: every drive holds a full copy.", "Mirror: ogni disco contiene una copia completa.";
    "wiz.d_raid5" => "Single parity: survives one failed drive.", "Parità singola: sopravvive a un disco guasto.";
    "wiz.d_raid6" => "Double parity: survives two failed drives.", "Doppia parità: sopravvive a due dischi guasti.";
    "wiz.d_raid10" => "Striped mirrors: fast, survives one failure per mirror.", "Mirror in striping: veloce, sopravvive a un guasto per mirror.";
    "wiz.label" => "Volume name", "Nome del volume";
    "wiz.label_hint" => "Mounted at {path}.", "Montato in {path}.";
    "wiz.label_invalid" => "Use 1-16 lowercase letters, digits or dashes, starting with a letter.", "Usa da 1 a 16 lettere minuscole, cifre o trattini, iniziando con una lettera.";
    "wiz.label_taken" => "A volume with this name already exists.", "Esiste già un volume con questo nome.";
    "wiz.compress" => "Compression (zstd, level 1)", "Compressione (zstd, livello 1)";
    "wiz.compress_hint" => "Saves space with almost no CPU cost; already compressed files are skipped. It can be turned off per subvolume.", "Risparmia spazio con un costo di CPU quasi nullo; i file già compressi vengono saltati. Si può disattivare per singolo sottovolume.";
    "wiz.encrypt" => "Encrypt the volume (LUKS2)", "Crittografa il volume (LUKS2)";
    "wiz.encrypt_hint" => "The volume must be unlocked after every restart. A recovery key and a header backup are created: keep both outside the NAS.", "Il volume va sbloccato dopo ogni riavvio. Vengono creati una chiave di ripristino e un backup dell'intestazione: conservali entrambi fuori dal NAS.";
    "wiz.pass_again" => "Repeat the passphrase", "Ripeti la passphrase";
    "wiz.pass_hint" => "8 to 512 characters. Without it (or the recovery key) the data cannot be read.", "Da 8 a 512 caratteri. Senza di essa (o la chiave di ripristino) i dati non sono leggibili.";
    "wiz.pass_len" => "The passphrase needs 8 to 512 characters.", "La passphrase deve avere da 8 a 512 caratteri.";
    "wiz.pass_mismatch" => "The passphrases don't match.", "Le passphrase non coincidono.";
    "wiz.erase_title" => "All data on these drives will be erased", "Tutti i dati su questi dischi verranno cancellati";
    "wiz.create_go" => "Erase drives and create", "Cancella i dischi e crea";
    "wiz.creating" => "Creating {name}", "Creazione di {name}";
    "wiz.created" => "Volume {name} created.", "Volume {name} creato.";
    "wiz.secure_title" => "Secure the encryption now", "Metti al sicuro la crittografia ora";
    "wiz.secure_text" => "Save the recovery key (shown once, for one hour) and the LUKS header backup outside the NAS.", "Salva fuori dal NAS la chiave di ripristino (mostrata una volta, per un'ora) e il backup dell'intestazione LUKS.";
    "wiz.open_volume" => "Open the volume", "Apri il volume";
    "wiz.back_review" => "Back to the review", "Torna al riepilogo";
    // status words
    "health.failed" => "Failed", "Guasto";
    "health.syncing" => "Resyncing", "In sincronizzazione";
    // generic
    "loading" => "Loading…", "Caricamento…";
    "error.load" => "Couldn't load data", "Impossibile caricare i dati";
    "soon" => "Coming soon", "In arrivo";
    "soon.plugins" => "Plugins that add pages, widgets and commands through a declarative manifest.", "Plugin che aggiungono pagine, widget e comandi tramite un manifest dichiarativo.";
    "soon.system" => "Network, software updates, notifications, system logs and a web terminal are on the way.", "Rete, aggiornamenti software, notifiche, log di sistema e terminale web sono in arrivo.";
    "sys.subtitle" => "System information and interface preferences", "Informazioni di sistema e preferenze dell'interfaccia";
    "sys.info" => "System information", "Informazioni di sistema";
    "sys.prefs" => "Interface", "Interfaccia";
    "notfound" => "Page not found", "Pagina non trovata";
    "notfound.text" => "This address doesn't match any section.", "Questo indirizzo non corrisponde a nessuna sezione.";
    "back_home" => "Back to Dashboard", "Torna alla dashboard";
    "close" => "Close", "Chiudi";
    "copied" => "Copied", "Copiato";
    "role.admin" => "Administrator", "Amministratore";
    "nav.security" => "Security", "Sicurezza";
    // sign-in
    "login.sign_in" => "Sign in", "Accedi";
    "username" => "User name", "Nome utente";
    "login.password" => "Password", "Password";
    "login.caps" => "Caps Lock is on", "Il Bloc Maiusc è attivo";
    "login.signing_in" => "Signing in…", "Accesso in corso…";
    "login.verify" => "Verify", "Verifica";
    "login.wait" => "Try again in {t}", "Riprova tra {t}";
    "login.totp_text" => "Enter the 6-digit code from your authenticator app.", "Inserisci il codice a 6 cifre dell'app di autenticazione.";
    "login.recovery_text" => "Enter one of the recovery codes you saved when you turned on two-factor authentication. Each code works once.", "Inserisci uno dei codici di recupero salvati quando hai attivato la verifica in due passaggi. Ogni codice vale una volta sola.";
    "login.totp_code" => "Authentication code", "Codice di autenticazione";
    "login.recovery_code" => "Recovery code", "Codice di recupero";
    "login.use_recovery" => "Use a recovery code", "Usa un codice di recupero";
    "login.use_app" => "Use the authenticator app", "Usa l'app di autenticazione";
    "login.expired" => "Your session has ended. Sign in again to continue where you left off.", "La sessione è scaduta. Accedi di nuovo per riprendere da dove eri rimasto.";
    "login.signed_out" => "You have signed out.", "Sei uscito.";
    "login.renew" => "Sign in again to start a new session.", "Accedi di nuovo per aprire una nuova sessione.";
    "login.foot" => "Use your system account (administrators only).", "Usa il tuo account di sistema (solo amministratori).";
    "login.empty" => "Enter your user name and password.", "Inserisci nome utente e password.";
    "err.invalid_credentials" => "Wrong user name or password.", "Nome utente o password errati.";
    "err.invalid_totp" => "That code isn't valid. Check the time on your device and try a new code.", "Codice non valido. Controlla l'ora del dispositivo e prova un codice nuovo.";
    "err.rate_limited" => "Too many failed attempts.", "Troppi tentativi falliti.";
    "err.auth_unavailable" => "The authentication service is unavailable. Check that rynasd is running.", "Il servizio di autenticazione non è disponibile. Controlla che rynasd sia in esecuzione.";
    "err.network" => "Can't reach the server. Check your connection.", "Server non raggiungibile. Controlla la connessione.";
    "err.csrf" => "Your session changed in another tab. Reload the page.", "La sessione è cambiata in un'altra scheda. Ricarica la pagina.";
    "err.backend_unavailable" => "The rynas service is not answering. Check that rynasd is running.", "Il servizio rynas non risponde. Controlla che rynasd sia in esecuzione.";
    "err.timeout" => "The NAS took too long to answer.", "Il NAS ha impiegato troppo a rispondere.";
    "err.bad_gateway" => "The rynas service sent an invalid answer.", "Il servizio rynas ha inviato una risposta non valida.";
    "err.internal" => "Internal error on the NAS. Details are in the system log.", "Errore interno del NAS. I dettagli sono nel log di sistema.";
    "err.forbidden" => "Your account can't do this.", "Il tuo account non può farlo.";
    "err.not_found" => "Not found.", "Non trovato.";
    "err.retry" => "Retry", "Riprova";
    "err.offline_title" => "Can't reach the NAS", "NAS non raggiungibile";
    "err.offline_text" => "The web service is not answering. It may be restarting, or the network may be down. rynas keeps retrying.", "Il servizio web non risponde. Potrebbe essere in riavvio, oppure la rete non è disponibile. rynas continua a riprovare.";
    "err.unauthenticated" => "Your session has ended.", "La sessione è scaduta.";
    "err.bad_request" => "The request was not valid.", "La richiesta non era valida.";
    "err.invalid_value" => "A value is not valid.", "Un valore non è valido.";
    "err.nothing_pending" => "There are no pending changes.", "Non ci sono modifiche in sospeso.";
    "err.apply_group_split" => "This change includes a deletion (such as a folder's), which is applied only together with all the pending changes: review it and apply it from the pending changes bar.", "Questa modifica comprende un'eliminazione (per esempio di una cartella), che si applica solo insieme a tutte le modifiche in sospeso: controllala e applicala dalla barra delle modifiche in sospeso.";
    "err.conflict" => "Something changed meanwhile. Reload and try again.", "Nel frattempo qualcosa è cambiato. Ricarica e riprova.";
    "err.busy" => "Another operation is running. Try again shortly.", "È in corso un'altra operazione. Riprova tra poco.";
    "err.task_finished" => "The task has already finished.", "L'attività è già terminata.";
    "err.already_exists" => "It already exists.", "Esiste già.";
    "err.confirmation_required" => "This needs an explicit confirmation.", "Serve una conferma esplicita.";
    "err.not_applied" => "The change was not applied.", "La modifica non è stata applicata.";
    "err.method_not_allowed" => "Operation not supported.", "Operazione non supportata.";
    "err.payload_too_large" => "The data sent is too large.", "I dati inviati sono troppo grandi.";
    "err.unsupported_media_type" => "Unsupported data format.", "Formato dei dati non supportato.";
    "err.permission_denied" => "You don't have permission for this item.", "Non hai i permessi per questo elemento.";
    "err.insufficient_storage" => "The volume is full or your quota is used up.", "Il volume è pieno o la tua quota è esaurita.";
    "err.read_only" => "This item is read-only.", "Questo elemento è di sola lettura.";
    "err.share_root" => "A shared folder can't be changed from here.", "Una cartella condivisa non si modifica da qui.";
    "err.outside_share" => "This link leads out of the shared folder: it isn't followed.", "Questo collegamento porta fuori dalla cartella condivisa: non viene seguito.";
    "err.password_required" => "The archive needs its password.", "L'archivio richiede la sua password.";
    "err.tool_missing" => "A program this feature needs is not installed.", "Un programma necessario a questa funzione non è installato.";
    "err.remote_unreachable" => "The remote server can't be reached.", "Il server remoto non è raggiungibile.";
    "err.remote_auth_failed" => "The remote server refused the login.", "Il server remoto ha rifiutato l'accesso.";
    "err.not_available" => "This is not available on this system.", "Non è disponibile su questo sistema.";
    "err.docker_not_installed" => "Docker is not installed.", "Docker non è installato.";
    "err.docker_unavailable" => "Docker is installed but its service does not answer.", "Docker è installato ma il suo servizio non risponde.";
    "err.containers_running" => "Stop the running containers first.", "Ferma prima i container in esecuzione.";
    "err.stack_not_managed" => "This stack is managed outside rynas: its files cannot be changed here.", "Questo stack è gestito fuori da rynas: i suoi file non si possono modificare qui.";
    "err.host_key_changed" => "The server's key has changed since the first connection.", "La chiave del server è cambiata dalla prima connessione.";
    "err.generic" => "Something went wrong. Try again.", "Qualcosa è andato storto. Riprova.";
    "session.expires" => "Your session expires in {n} min", "La sessione scade tra {n} min";
    "session.expires_1" => "Your session expires in 1 min", "La sessione scade tra 1 min";
    "session.renew" => "Sign in again", "Accedi di nuovo";
    // shared words of the configuration pages
    "save" => "Save", "Salva";
    "create" => "Create", "Crea";
    "edit" => "Edit", "Modifica";
    "delete" => "Delete", "Elimina";
    "reset" => "Reset", "Ripristina";
    "off" => "Off", "Disattivato";
    "advanced" => "Advanced", "Avanzate";
    "col.status" => "Status", "Stato";
    "table.no_match" => "Nothing matches the search.", "Nessun risultato per la ricerca.";
    "form.check" => "Check the highlighted fields.", "Controlla i campi evidenziati.";
    "form.changed_elsewhere" => "These settings were changed elsewhere while you were editing.", "Queste impostazioni sono state modificate altrove mentre le stavi modificando.";
    "form.reload" => "Discard my edits and reload", "Scarta le mie modifiche e ricarica";
    "toast.saved_pending" => "Saved, pending apply", "Salvato, in attesa di applicazione";
    "pending.add" => "Pending: new", "In attesa: nuovo";
    "pending.change" => "Pending: changed", "In attesa: modificato";
    "pending.delete" => "Pending: removal", "In attesa: eliminazione";
    "v.account_name" => "Lowercase letters, digits, - and _; starts with a letter or _; up to 32 characters.", "Lettere minuscole, cifre, - e _; inizia con una lettera o _; fino a 32 caratteri.";
    "v.full_name" => "The full name cannot contain : or ,", "Il nome completo non può contenere : o ,";
    "v.email" => "Not a valid email address.", "Indirizzo email non valido.";
    "v.date" => "Use the format YYYY-MM-DD.", "Usa il formato AAAA-MM-GG.";
    "v.share_name" => "Letters, digits, . - and _; starts with a letter or digit; up to 64 characters.", "Lettere, cifre, . - e _; inizia con una lettera o una cifra; fino a 64 caratteri.";
    "v.path" => "An absolute path such as /srv/data, without .. or //.", "Un percorso assoluto come /srv/data, senza .. o //.";
    "v.client" => "A host name, IP address, subnet (192.0.2.0/24), wildcard (*.lan) or *.", "Un nome host, indirizzo IP, sottorete (192.0.2.0/24), carattere jolly (*.lan) o *.";
    "v.workgroup" => "1–15 characters, no spaces or \\ / : * ? \" < > | ,", "1–15 caratteri, senza spazi né \\ / : * ? \" < > | ,";
    "v.threads" => "Between 1 and 256.", "Tra 1 e 256.";
    "v.port" => "A port between 1 and 65535.", "Una porta tra 1 e 65535.";
    // users and groups
    "users.subtitle" => "Local accounts for file sharing and sign-in", "Account locali per la condivisione di file e l'accesso";
    "users.tab_users" => "Users", "Utenti";
    "users.tab_groups" => "Groups", "Gruppi";
    "users.search" => "Search users", "Cerca utenti";
    "users.create" => "Create user", "Crea utente";
    "users.edit_title" => "Edit user {name}", "Modifica utente {name}";
    "users.empty" => "No users yet", "Ancora nessun utente";
    "users.empty_text" => "Create an account for each person who uses the NAS, then give it access to shared folders.", "Crea un account per ogni persona che usa il NAS, poi dagli accesso alle cartelle condivise.";
    "users.name" => "Name", "Nome";
    "users.name_help" => "Used to sign in; cannot be changed later.", "Serve per accedere; non si può cambiare in seguito.";
    "users.full_name" => "Full name", "Nome completo";
    "users.email" => "Email", "Email";
    "users.email_help" => "For notifications to this user.", "Per le notifiche a questo utente.";
    "users.description" => "Description", "Descrizione";
    "users.groups" => "Groups", "Gruppi";
    "users.no_groups" => "No groups yet: create them in the Groups tab.", "Ancora nessun gruppo: creali nella scheda Gruppi.";
    "users.access" => "Access", "Accesso";
    "users.admin_chip" => "Admin", "Admin";
    "users.sec_info" => "User information", "Informazioni utente";
    "users.sec_access" => "Access", "Accesso";
    "users.sec_password" => "Password", "Password";
    "users.admin" => "Administrator", "Amministratore";
    "users.admin_help" => "Can sign in to rynas and change every setting.", "Può accedere a rynas e cambiare ogni impostazione.";
    "users.smb" => "SMB access", "Accesso SMB";
    "users.smb_help" => "Can open shared folders from Windows, macOS and Linux.", "Può aprire le cartelle condivise da Windows, macOS e Linux.";
    "users.shell" => "SSH access", "Accesso SSH";
    "users.shell_help" => "Off by default. SFTP gives file transfer only.", "Disattivato di default. SFTP consente solo il trasferimento di file.";
    "users.shell_none" => "No SSH", "Nessun SSH";
    "users.shell_sftp" => "SFTP only", "Solo SFTP";
    "users.shell_shell" => "Shell", "Shell";
    "users.disabled" => "Disable account", "Disattiva account";
    "users.disabled_help" => "The user can't sign in or open shared folders; nothing is deleted.", "L'utente non può accedere né aprire le cartelle condivise; non viene eliminato nulla.";
    "users.expires" => "Expires on", "Scade il";
    "users.expires_help" => "The account stops working on this day. Empty: never.", "L'account smette di funzionare da questo giorno. Vuoto: mai.";
    "users.set_password" => "Set password", "Imposta password";
    "users.st_active" => "Active", "Attivo";
    "users.st_disabled" => "Disabled", "Disattivato";
    "users.st_expired" => "Expired", "Scaduto";
    "users.st_password" => "Needs a password", "Serve una password";
    "users.st_unmanaged" => "Not managed", "Non gestito";
    "users.st_missing" => "Missing on the system", "Assente nel sistema";
    "users.delete_title" => "Delete user {name}?", "Eliminare l'utente {name}?";
    "users.delete_text" => "The account and its access to shared folders are removed when you apply the changes. Files in shared folders stay.", "L'account e i suoi accessi alle cartelle condivise vengono rimossi quando applichi le modifiche. I file nelle cartelle condivise restano.";
    "users.remove_home" => "Also delete the home folder", "Elimina anche la cartella home";
    "users.remove_home_help" => "Its files cannot be recovered after applying.", "I suoi file non si possono recuperare dopo l'applicazione.";
    "users.tab_homes" => "User homes", "Home utenti";
    "users.home" => "Home folder", "Cartella home";
    "homes.intro" => "Each user gets a private home folder, on a data volume or on the system disk, chosen per user.", "Ogni utente ha una cartella home privata, su un volume dati o sul disco di sistema, scelta per ciascun utente.";
    "homes.enabled" => "Home folders on a volume", "Cartelle home su un volume";
    "homes.enabled_help" => "New users get their home in the homes folder of the default volume, unless you choose another place for them. Turning it off moves and deletes nothing.", "I nuovi utenti hanno la home nella cartella homes del volume predefinito, se non scegli un altro posto per loro. Disattivandolo non si sposta né si elimina nulla.";
    "homes.volume" => "Default volume", "Volume predefinito";
    "homes.volume_help" => "New homes go in {path}: one btrfs subvolume per user on btrfs, a plain folder otherwise. Changing it moves no existing home.", "Le nuove home vanno in {path}: un sottovolume btrfs per utente su btrfs, altrimenti una cartella normale. Cambiarlo non sposta le home esistenti.";
    "homes.volume_opt" => "{path} ({fs}), {free} free", "{path} ({fs}), {free} liberi";
    "homes.no_volumes" => "No data volume is mounted: create one in Storage first.", "Nessun volume dati montato: creane prima uno in Archiviazione.";
    "homes.path" => "Home path", "Percorso home";
    "homes.path_help" => "Accounts use this path, one link per user: it stays the same when a home moves to another volume.", "Gli account usano questo percorso, un collegamento per utente: resta uguale quando una home passa a un altro volume.";
    "homes.smb" => "Share home folders over SMB", "Condividi le cartelle home via SMB";
    "homes.smb_help" => "Each user sees a share named after them with their home; nobody else can open it.", "Ogni utente vede una condivisione con il proprio nome che contiene la sua home; nessun altro può aprirla.";
    "homes.recycle" => "Recycle bin in home folders", "Cestino nelle cartelle home";
    "homes.recycle_help" => "Files deleted over SMB go to #recycle in the home.", "I file eliminati via SMB finiscono in #recycle nella home.";
    "homes.off_note" => "Off: new users get their home in /home on the system disk. Homes already on a volume keep working and can still move.", "Disattivato: i nuovi utenti hanno la home in /home sul disco di sistema. Le home già su un volume continuano a funzionare e si possono ancora spostare.";
    "homes.list" => "Home folders", "Cartelle home";
    "homes.location" => "Location", "Posizione";
    "homes.loc_volume" => "Volume", "Volume";
    "homes.loc_system" => "System disk", "Disco di sistema";
    "homes.loc_other" => "Other folder", "Altra cartella";
    "homes.loc_missing" => "Missing", "Mancante";
    "homes.kept" => "Old folder kept as {path}", "Vecchia cartella conservata come {path}";
    "homes.move" => "Move home", "Sposta home";
    "homes.move_all" => "Move all", "Sposta tutte";
    "homes.move_needs" => "Turn on home folders on a volume and apply the changes to move homes there. Moving to the system disk is always possible.", "Attiva le cartelle home su un volume e applica le modifiche per potervi spostare le home. Lo spostamento sul disco di sistema è sempre possibile.";
    "homes.move_title_one" => "Move the home of {name}", "Spostare la home di {name}";
    "homes.move_title_all" => "Move home folders", "Spostare le cartelle home";
    "homes.move_text" => "Nothing moves by itself: choose where, check what happens, then start the move.", "Niente si sposta da solo: scegli dove, controlla cosa succede, poi avvia lo spostamento.";
    "homes.move_steps" => "The files are copied with permissions, ACLs and attributes and checked; then the account points at the new home. The old folder is kept, renamed, until you confirm. The user must stay signed out and disconnected meanwhile.", "I file vengono copiati con permessi, ACL e attributi e verificati; poi l'account punta alla nuova home. La vecchia cartella resta, rinominata, finché non confermi. Nel frattempo l'utente deve restare disconnesso.";
    "homes.from" => "Current home", "Home attuale";
    "homes.to" => "New home", "Nuova home";
    "homes.target" => "Files go to", "I file vanno in";
    "homes.size" => "Space used", "Spazio usato";
    "homes.avail" => "Free at the destination", "Libero nella destinazione";
    "homes.kind" => "Created as", "Creata come";
    "homes.kind_subvolume" => "btrfs subvolume", "Sottovolume btrfs";
    "homes.kind_folder" => "Plain folder", "Cartella normale";
    "homes.ssh_note" => "SSH keys (.ssh/authorized_keys) move with the home.", "Le chiavi SSH (.ssh/authorized_keys) si spostano con la home.";
    "homes.blocked" => "Cannot move now:", "Non si può spostare ora:";
    "homes.move_btn" => "Start move", "Avvia spostamento";
    "homes.moved" => "Home folders moved: confirm each move once everything works.", "Cartelle home spostate: conferma ogni spostamento quando tutto funziona.";
    "homes.none" => "No home to move.", "Nessuna home da spostare.";
    "homes.batch_total" => "{n} homes, {size} in all; {free} free at the destination.", "{n} home, {size} in tutto; {free} liberi nella destinazione.";
    "homes.batch_no_space" => "{n} homes, {size} in all: they do not fit together in the {free} free at the destination. Move fewer at a time or free some space.", "{n} home, {size} in tutto: insieme non entrano nei {free} liberi della destinazione. Spostane meno alla volta o libera spazio.";
    "homes.batch_partial" => "Some sizes are only partly measured: before copying, the move measures every home in full and stops if they do not fit.", "Alcune dimensioni sono misurate solo in parte: prima di copiare, lo spostamento misura per intero ogni home e si ferma se non entrano.";
    "homes.finish" => "Confirm move", "Conferma spostamento";
    "homes.finish_title" => "Confirm the move of {name}?", "Confermare lo spostamento di {name}?";
    "homes.finish_text" => "The old folder {path} is deleted for good.", "La vecchia cartella {path} viene eliminata definitivamente.";
    "homes.finish_btn" => "Delete old folder", "Elimina vecchia cartella";
    "homes.finished" => "Old home folder of {name} deleted.", "Vecchia cartella home di {name} eliminata.";
    "homes.undo" => "Undo move", "Annulla spostamento";
    "homes.undo_title" => "Undo the move of {name}?", "Annullare lo spostamento di {name}?";
    "homes.undo_text" => "The old folder goes back to {path} and is the home again. The copy is kept aside as a hidden folder next to it.", "La vecchia cartella torna in {path} e torna a essere la home. La copia resta da parte come cartella nascosta accanto.";
    "homes.undone" => "Move of {name} undone.", "Spostamento di {name} annullato.";
    "homes.b.service_off" => "home folders on a volume are off or not applied yet", "le cartelle home su un volume sono disattivate o non ancora applicate";
    "homes.b.not_local" => "not a local user (uid {detail})", "non è un utente locale (uid {detail})";
    "homes.b.already_moved" => "the home is already there", "la home è già lì";
    "homes.b.source_missing" => "the home {detail} does not exist", "la home {detail} non esiste";
    "homes.b.source_not_plain" => "the home is a link or contains a mount ({detail})", "la home è un collegamento o contiene un mount ({detail})";
    "homes.b.target_exists" => "{detail} exists already", "{detail} esiste già";
    "homes.b.processes" => "the user has running processes ({detail})", "l'utente ha processi in esecuzione ({detail})";
    "homes.b.smb_sessions" => "the user is connected over SMB ({detail} open sessions)", "l'utente è connesso via SMB ({detail} sessioni aperte)";
    "homes.b.smb_session_one" => "the user is connected over SMB (1 open session)", "l'utente è connesso via SMB (1 sessione aperta)";
    "homes.b.share_inside" => "shared folder {detail} uses the home", "la cartella condivisa {detail} usa la home";
    "homes.b.no_space" => "not enough free space at the destination (needs {detail})", "spazio libero insufficiente nella destinazione (servono {detail})";
    "homes.b.no_rsync" => "rsync is not installed", "rsync non è installato";
    "homes.b.move_waiting" => "a previous move waits for confirmation", "uno spostamento precedente attende conferma";
    "homes.dest" => "Move to", "Sposta in";
    "homes.dest_help" => "A volume (its homes folder) or the system disk (/home).", "Un volume (la sua cartella homes) o il disco di sistema (/home).";
    "homes.dest_system" => "System disk (/home)", "Disco di sistema (/home)";
    "homes.dest_here" => "Where it is now ({path})", "Dove si trova ora ({path})";
    "homes.source" => "Current folder", "Cartella attuale";
    "homes.b.target_invalid" => "{detail} is not a volume that can hold homes", "{detail} non è un volume che può contenere home";
    "users.home_place" => "Home location", "Posizione della home";
    "users.home_place_help" => "The homes folder of a volume, or /home on the system disk.", "La cartella homes di un volume, oppure /home sul disco di sistema.";
    "users.home_move_help" => "Choosing another place moves the home after saving: you see the plan first, then the files are copied and checked.", "Scegliendo un altro posto la home si sposta dopo il salvataggio: prima vedi il piano, poi i file vengono copiati e verificati.";
    // Users adopt / UID change
    "users.adopt" => "Adopt", "Adotta";
    "users.adopt_all" => "Adopt all", "Adotta tutti";
    "users.adopt_banner" => "{n} accounts on this system were created outside rynas. Adopt them to manage their password, groups and access: they are not recreated and their files do not change.", "{n} account di questo sistema sono stati creati fuori da rynas. Adottali per gestirne password, gruppi e accessi: non vengono ricreati e i loro file non cambiano.";
    "users.adopt_banner_one" => "One account on this system was created outside rynas. Adopt it to manage its password, groups and access: it is not recreated and its files do not change.", "Un account di questo sistema è stato creato fuori da rynas. Adottalo per gestirne password, gruppi e accessi: non viene ricreato e i suoi file non cambiano.";
    "users.adopted" => "Accounts adopted", "Account adottati";
    "users.unmanaged_note" => "Created outside rynas. Saving adopts the account as it is: it is not recreated and its files do not change.", "Creato fuori da rynas. Salvando, l'account viene adottato così com'è: non viene ricreato e i suoi file non cambiano.";
    "users.uid" => "User ID (UID)", "ID utente (UID)";
    "users.uid_help" => "Empty: the next free one. Set it to match the owner of files copied from another system.", "Vuoto: il primo libero. Impostalo per farlo coincidere con il proprietario di file copiati da un altro sistema.";
    "users.uid_range" => "Use a number from 1000 to 59999.", "Usa un numero da 1000 a 59999.";
    "ids.change_uid" => "Change UID…", "Cambia UID…";
    "ids.change_gid" => "Change GID…", "Cambia GID…";
    "ids.reown" => "Take over files…", "Acquisisci file…";
    "ids.title_uid" => "Change the UID of {name}", "Cambia l'UID di {name}";
    "ids.title_gid" => "Change the GID of {name}", "Cambia il GID di {name}";
    "ids.title_reown" => "Give files to {name}", "Assegna file a {name}";
    "ids.text_change" => "The account gets the new ID, then the files owned by the old one in the chosen folders follow. Check first: nothing changes until you start.", "L'account riceve il nuovo ID, poi i file del vecchio ID nelle cartelle scelte lo seguono. Prima verifica: non cambia nulla finché non avvii.";
    "ids.text_reown" => "Files owned by an ID that no account has, such as data copied from another system with its numeric owners, become this account's. Check first: nothing changes until you start.", "I file di un ID che nessun account possiede, come i dati copiati da un altro sistema con i proprietari numerici, diventano di questo account. Prima verifica: non cambia nulla finché non avvii.";
    "ids.new_uid" => "New UID", "Nuovo UID";
    "ids.new_gid" => "New GID", "Nuovo GID";
    "ids.from_id" => "Files now owned by ID", "File ora dell'ID";
    "ids.id_help_change" => "{min}–{max}, not used by another account. Now: {id}.", "{min}–{max}, non usato da un altro account. Ora: {id}.";
    "ids.id_help_reown" => "1000 or more, not used by any account.", "1000 o più, non usato da nessun account.";
    "ids.folders" => "Folders", "Cartelle";
    "ids.folders_help" => "The data volumes and the home, unless you choose. Read-only snapshots and other file systems inside are skipped; links are not followed.", "I volumi dati e la home, se non scegli altro. Snapshot in sola lettura e altri file system al loro interno vengono saltati; i collegamenti non vengono seguiti.";
    "ids.add_folder" => "Add folder", "Aggiungi cartella";
    "ids.other_folder" => "Another folder", "Altra cartella";
    "ids.acls" => "Also change ACL entries", "Cambia anche le voci ACL";
    "ids.acls_help" => "Permissions that access lists give to this ID.", "I permessi che gli elenchi di accesso danno a questo ID.";
    "ids.check" => "Check", "Verifica";
    "ids.start_change" => "Change ID", "Cambia ID";
    "ids.start_reown" => "Change owner", "Cambia proprietario";
    "ids.scanned" => "Entries checked", "Elementi controllati";
    "ids.at_least" => "at least {n}", "almeno {n}";
    "ids.owners" => "Owners that change", "Proprietari che cambiano";
    "ids.groups" => "Groups that change", "Gruppi che cambiano";
    "ids.acl_entries" => "ACL entries that change", "Voci ACL che cambiano";
    "ids.acl_conflicts" => "ACL entries lost", "Voci ACL perse";
    "ids.acl_conflicts_text" => "On these files the new ID already has its own ACL entry: it is kept as it is and the old ID's entry is dropped, so the permissions it gave are lost. Check them after the change.", "Su questi file il nuovo ID ha già una sua voce ACL: resta com'è e la voce del vecchio ID viene tolta, quindi i permessi che dava si perdono. Controllali dopo la modifica.";
    "ids.errors" => "Unreadable entries", "Elementi non leggibili";
    "ids.readonly" => "Read-only snapshots, skipped (they keep the old ID):", "Snapshot in sola lettura, saltati (mantengono il vecchio ID):";
    "ids.other_fs" => "Other file systems, skipped:", "Altri file system, saltati:";
    "ids.walked" => "Folders checked:", "Cartelle controllate:";
    "ids.retired" => "IDs retired by earlier changes (never given to new accounts):", "ID ritirati da modifiche precedenti (mai dati a nuovi account):";
    "ids.retired_item" => "{id}: {name}, now {new}", "{id}: {name}, ora {new}";
    "ids.target_retired" => "ID {id} was retired when {name} changed ID: files still under it (snapshots, folders left out then) may belong to {name}.", "L'ID {id} è stato ritirato quando {name} ha cambiato ID: i file che lo hanno ancora (snapshot, cartelle escluse allora) potrebbero essere di {name}.";
    "ids.resumes" => "This completes the previous ID change of this account: files of ID {from} become {to}'s, with their setuid and setgid bits kept.", "Questo completa la modifica di ID precedente di questo account: i file dell'ID {from} passano a {to}, mantenendo i bit setuid e setgid.";
    "ids.complete_text" => "This account had ID {id} before an earlier change: files that still have it can follow.", "Questo account aveva l'ID {id} prima di una modifica precedente: i file che lo hanno ancora possono seguirlo.";
    "ids.complete" => "Complete: take the files from ID {id}", "Completa: prendi i file dall'ID {id}";
    "ids.partial" => "The check stopped at its time limit: the numbers are lower bounds and the change goes through everything.", "La verifica si è fermata al limite di tempo: i numeri sono minimi e la modifica passa comunque tutto.";
    "ids.after" => "Not changed: crontabs, systemd user units and files outside these folders. The change runs as a task you can cancel; after a cancel the rest can be completed from here.", "Non vengono cambiati: crontab, unità systemd dell'utente e file fuori da queste cartelle. La modifica gira come attività annullabile; dopo un annullamento il resto si completa da qui.";
    "ids.done" => "Ownership changed for {name}", "Proprietà cambiata per {name}";
    "ids.b_not_local" => "Not a local account (1000–59999): root and system accounts never change.", "Non è un account locale (1000–59999): root e gli account di sistema non cambiano mai.";
    "ids.b_unmanaged" => "Created outside rynas: adopt it first.", "Creato fuori da rynas: prima adottalo.";
    "ids.b_out_of_range" => "ID out of range: {d}.", "ID fuori intervallo: {d}.";
    "ids.b_same_id" => "That is the current ID.", "È l'ID attuale.";
    "ids.b_id_in_use" => "The ID belongs to {d}.", "L'ID appartiene a {d}.";
    "ids.b_processes" => "The user has running processes ({d}): sign them out first.", "L'utente ha processi in esecuzione ({d}): prima disconnettilo.";
    "ids.b_processes_group" => "Processes are running with this group ({d}): stop them first.", "Ci sono processi in esecuzione con questo gruppo ({d}): prima fermali.";
    "ids.b_smb_sessions" => "The user is connected over SMB ({d} sessions): disconnect them first.", "L'utente è connesso via SMB ({d} sessioni): prima disconnettilo.";
    "ids.b_smb_sessions_group" => "Members of the group are connected over SMB ({d} sessions): disconnect them first.", "Alcuni membri del gruppo sono connessi via SMB ({d} sessioni): prima disconnettili.";
    "ids.b_path_invalid" => "Folder missing or outside the data folders: {d}", "Cartella assente o fuori dalle cartelle dati: {d}";
    "ids.b_bad_request" => "Enter an ID.", "Inserisci un ID.";
    "ids.b_other" => "Not available on this system.", "Non disponibile su questo sistema.";
    "homes.b.other" => "another reason", "un altro motivo";
    "groups.search" => "Search groups", "Cerca gruppi";
    "groups.create" => "Create group", "Crea gruppo";
    "groups.edit_title" => "Edit group {name}", "Modifica gruppo {name}";
    "groups.empty" => "No groups yet", "Ancora nessun gruppo";
    "groups.empty_text" => "Groups give the same permissions to several users at once.", "I gruppi danno le stesse autorizzazioni a più utenti insieme.";
    "groups.name" => "Group name", "Nome gruppo";
    "groups.gid" => "GID", "GID";
    "smart.col_id" => "ID", "ID";
    "groups.name_help" => "Cannot be changed later. System groups (sudo, staff, users…) are not managed here.", "Non si può cambiare in seguito. I gruppi di sistema (sudo, staff, users…) non si gestiscono qui.";
    "groups.members" => "Members", "Membri";
    "groups.no_members" => "No members", "Nessun membro";
    "groups.no_users" => "No users yet.", "Ancora nessun utente.";
    "groups.delete_title" => "Delete group {name}?", "Eliminare il gruppo {name}?";
    "groups.delete_text" => "Members lose the permissions they had through this group when you apply the changes. The users stay.", "I membri perdono le autorizzazioni ricevute tramite questo gruppo quando applichi le modifiche. Gli utenti restano.";
    "pw.new" => "New password", "Nuova password";
    "pw.confirm" => "Confirm password", "Conferma password";
    "pw.rules" => "8 to 256 characters, not the user name. A long phrase is easier to remember and harder to guess.", "Da 8 a 256 caratteri, diversa dal nome utente. Una frase lunga è più facile da ricordare e più difficile da indovinare.";
    "pw.too_short" => "At least 8 characters.", "Almeno 8 caratteri.";
    "pw.too_long" => "At most 256 characters.", "Al massimo 256 caratteri.";
    "pw.control" => "Control characters are not allowed.", "I caratteri di controllo non sono ammessi.";
    "pw.is_user" => "The password cannot be the user name.", "La password non può essere il nome utente.";
    "pw.mismatch" => "The passwords don't match.", "Le password non coincidono.";
    "pw.s0" => "Too short", "Troppo corta";
    "pw.s1" => "Weak", "Debole";
    "pw.s2" => "Fair", "Discreta";
    "pw.s3" => "Good", "Buona";
    "pw.s4" => "Strong", "Forte";
    "pw.done" => "Password of {name} changed.", "Password di {name} cambiata.";
    "pw.after_apply" => "Set the password after applying the changes: the account must exist first.", "Imposta la password dopo aver applicato le modifiche: prima l'account deve esistere.";
    "pw.not_applied" => "This user is created when you apply the pending changes; set the password afterwards.", "Questo utente viene creato quando applichi le modifiche in attesa; imposta la password dopo.";
    // shared folders
    "shares.subtitle" => "Folders shared over SMB and NFS, and who can open them", "Cartelle condivise via SMB e NFS, e chi può aprirle";
    "shares.search" => "Search shared folders", "Cerca cartelle condivise";
    "shares.create" => "Create shared folder", "Crea cartella condivisa";
    "shares.edit_title" => "Edit shared folder {name}", "Modifica cartella condivisa {name}";
    "shares.editor" => "Shared folder settings", "Impostazioni della cartella condivisa";
    "shares.empty" => "No shared folders yet", "Ancora nessuna cartella condivisa";
    "shares.empty_text" => "Create a shared folder on a volume, then choose who can read or change its files.", "Crea una cartella condivisa su un volume, poi scegli chi può leggerne o modificarne i file.";
    "shares.name" => "Name", "Nome";
    "shares.name_help" => "The name clients see; cannot be changed later.", "Il nome che vedono i client; non si può cambiare in seguito.";
    "shares.location" => "Location", "Posizione";
    "shares.volume" => "Volume", "Volume";
    "shares.size_of_volume" => "{pct} of {volume}", "{pct} di {volume}";
    "shares.size_bytes" => "{n} bytes", "{n} byte";
    "shares.size_capacity" => "{volume}: {capacity}", "{volume}: {capacity}";
    "shares.size_exact" => "Exact (btrfs quota)", "Esatta (quota btrfs)";
    "shares.size_as_of" => "As of {when}", "Al {when}";
    "shares.protocols" => "Protocols", "Protocolli";
    "shares.no_protocol" => "Not shared", "Non condivisa";
    "shares.recycle" => "Recycle bin", "Cestino";
    "shares.recycle_days" => "On · {n} d", "Attivo · {n} g";
    "shares.recycle_keep" => "On · kept", "Attivo · conservato";
    "shares.subvolume" => "Subvolume", "Sottovolume";
    "shares.permissions" => "Permissions", "Autorizzazioni";
    "shares.st_ready" => "Ready", "Pronta";
    "shares.st_missing" => "Folder missing", "Cartella assente";
    "shares.st_on_apply" => "Created on apply", "Creata all'applicazione";
    "shares.st_warnings" => "Warnings", "Avvisi";
    "shares.st_root_only" => "Root only", "Solo root";
    "shares.delete_title" => "Delete shared folder {name}?", "Eliminare la cartella condivisa {name}?";
    "shares.delete_text" => "The share and its permissions are removed when you apply the changes. The folder and its files stay on the volume: you will find it under Unshared folders.", "La condivisione e le sue autorizzazioni vengono rimosse quando applichi le modifiche. La cartella e i suoi file restano sul volume: la trovi in Cartelle non condivise.";
    "shares.tab_general" => "General", "Generale";
    "shares.tab_smb" => "SMB", "SMB";
    "shares.tab_nfs" => "NFS", "NFS";
    "shares.loc_subvolume" => "New subvolume", "Nuovo sottovolume";
    "shares.loc_folder" => "Existing folder", "Cartella esistente";
    "shares.no_volumes" => "No volume found. Create one in Storage, or share an existing folder.", "Nessun volume trovato. Creane uno in Storage, oppure condividi una cartella esistente.";
    "shares.subvolume_help" => "Created as {path} when you apply the changes (own snapshots and statistics).", "Creato come {path} quando applichi le modifiche (snapshot e statistiche propri).";
    "shares.compression" => "Compression", "Compressione";
    "shares.compression_help" => "Turn off for video, photos and backups that are already compressed.", "Disattivala per video, foto e backup già compressi.";
    "shares.path" => "Path", "Percorso";
    "shares.path_help" => "An existing folder on a local filesystem, ideally a btrfs subvolume.", "Una cartella esistente su un filesystem locale, meglio se un sottovolume btrfs.";
    "shares.path_edit_help" => "Changing the path gives NFS clients a new export id: they must mount again.", "Cambiare il percorso assegna ai client NFS un nuovo id di esportazione: dovranno montarla di nuovo.";
    "shares.smb_on" => "Share over SMB", "Condividi via SMB";
    "shares.smb_on_help" => "Windows, macOS and Linux file sharing.", "Condivisione file per Windows, macOS e Linux.";
    "shares.preset" => "Purpose", "Scopo";
    "shares.preset_help" => "Sets the SMB options that suit the use.", "Imposta le opzioni SMB adatte all'uso.";
    "shares.preset_standard" => "Standard", "Standard";
    "shares.preset_multi" => "Also shared over NFS", "Condivisa anche via NFS";
    "shares.preset_tm" => "Time Machine backups", "Backup Time Machine";
    "shares.preset_huge" => "Folders with 100,000+ files", "Cartelle con oltre 100.000 file";
    "shares.tm_size" => "Time Machine size limit", "Limite di spazio Time Machine";
    "shares.tm_size_help" => "For example 500G or 1T. Empty: no limit.", "Per esempio 500G o 1T. Vuoto: nessun limite.";
    "shares.browseable" => "Visible in the network", "Visibile in rete";
    "shares.browseable_help" => "Listed when browsing the NAS; hidden shares open by name only.", "Elencata sfogliando il NAS; quelle nascoste si aprono solo per nome.";
    "shares.recycle_on_help" => "Files deleted over SMB go to #recycle instead of disappearing.", "I file eliminati via SMB finiscono in #recycle invece di sparire.";
    "shares.retention" => "Empty after (days)", "Svuota dopo (giorni)";
    "shares.retention_help" => "The daily cleanup removes older items; 0 keeps them.", "La pulizia giornaliera rimuove gli elementi più vecchi; 0 li conserva.";
    "shares.hide_dot" => "Hide dot files", "Nascondi i file che iniziano con il punto";
    "shares.encrypt" => "Require encryption", "Richiedi la crittografia";
    "shares.shadow" => "Previous versions", "Versioni precedenti";
    "shares.shadow_help" => "Show snapshots in the Windows \"Previous versions\" tab.", "Mostra gli snapshot nella scheda \"Versioni precedenti\" di Windows.";
    "shares.snap_prefix" => "Snapshot name prefix", "Prefisso del nome degli snapshot";
    "shares.snap_prefix_help" => "Empty: the last part of the share path.", "Vuoto: l'ultima parte del percorso della condivisione.";
    "shares.veto" => "Hidden and blocked names", "Nomi nascosti e bloccati";
    "shares.veto_help" => "One pattern per line, for example @eaDir.", "Uno schema per riga, per esempio @eaDir.";
    "shares.extra" => "Extra SMB options", "Opzioni SMB aggiuntive";
    "shares.nfs_on" => "Export over NFS", "Esporta via NFS";
    "shares.nfs_on_help" => "For Linux and other Unix clients. Clients need the same user ids.", "Per client Linux e altri Unix. I client devono avere gli stessi id utente.";
    "nfs.rules" => "Client rules", "Regole dei client";
    "nfs.no_rules" => "No client can mount this folder yet: add a rule.", "Nessun client può ancora montare questa cartella: aggiungi una regola.";
    "nfs.no_rules_short" => "No clients", "Nessun client";
    "nfs.remove_rule" => "Remove rule", "Rimuovi regola";
    "nfs.client" => "Client", "Client";
    "nfs.client_help" => "Host, IP, subnet, *.lan or *", "Host, IP, sottorete, *.lan o *";
    "nfs.access" => "Access", "Accesso";
    "nfs.squash" => "Squash", "Mappatura utenti";
    "nfs.squash_help" => "Which client users act as the anonymous user.", "Quali utenti del client agiscono come utente anonimo.";
    "nfs.squash_root" => "Map root to anonymous", "Mappa root su anonimo";
    "nfs.squash_all" => "Map all users to anonymous", "Mappa tutti gli utenti su anonimo";
    "nfs.squash_none" => "No mapping (root stays root)", "Nessuna mappatura (root resta root)";
    "nfs.anonuid" => "Anonymous uid", "uid anonimo";
    "nfs.anongid" => "Anonymous gid", "gid anonimo";
    "nfs.sync" => "Synchronous writes", "Scritture sincrone";
    "nfs.sync_help" => "Safe default; asynchronous is faster but can lose data on a power cut.", "Scelta sicura; asincrono è più veloce ma può perdere dati in caso di blackout.";
    "nfs.crossmnt" => "Include nested mounts", "Includi i mount annidati";
    "nfs.crossmnt_help" => "Also export subvolumes mounted below this folder.", "Esporta anche i sottovolumi montati sotto questa cartella.";
    "perm.show" => "Show", "Mostra";
    "perm.search" => "Search", "Cerca";
    "perm.user" => "User", "Utente";
    "perm.group" => "Group", "Gruppo";
    "not_set" => "Not set", "Non impostato";
    "perm.ro" => "Read only", "Sola lettura";
    "perm.rw" => "Read & write", "Lettura e scrittura";
    "perm.deny" => "No access", "Nessun accesso";
    "perm.nobody" => "Nobody to list.", "Nessuno da elencare.";
    "perm.empty_help" => "No permissions set: rynas leaves the folder's current permissions alone.", "Nessuna autorizzazione impostata: rynas lascia invariate le autorizzazioni attuali della cartella.";
    "perm.admins" => "rynas administrators", "Amministratori di rynas";
    "perm.root_only_help" => "No permissions: the new folder belongs to root and nobody else will be able to use it. Give at least one user or group access.", "Nessuna autorizzazione: la nuova cartella appartiene a root e nessun altro potrà usarla. Dai accesso ad almeno un utente o gruppo.";
    "perm.keep_own" => "Keep the folder's own permissions", "Mantieni i permessi della cartella";
    "perm.set_now" => "Set permissions", "Imposta le autorizzazioni";
    "perm.empty_confirm_title" => "Save without permissions?", "Salvare senza autorizzazioni?";
    "perm.empty_confirm_text" => "{path} belongs to root and gives nobody else access: without permissions nobody but root will be able to use it, not even over SMB, NFS or the file manager.", "{path} appartiene a root e non dà accesso a nessun altro: senza autorizzazioni nessuno potrà usarla tranne root, nemmeno via SMB, NFS o dal file manager.";
    "perm.empty_confirm_ok" => "Save for root only", "Salva solo per root";
    "perm.rule_help" => "Only listed users and groups get in. No access wins over Read & write, which wins over Read only.", "Entrano solo gli utenti e i gruppi elencati. Nessun accesso prevale su Lettura e scrittura, che prevale su Sola lettura.";
    "eff.title" => "Effective permissions", "Autorizzazioni effettive";
    "eff.help" => "What a user or group really gets, counting the groups the user belongs to.", "Cosa ottiene davvero un utente o un gruppo, contando i gruppi di cui l'utente fa parte.";
    "eff.principal" => "User or group", "Utente o gruppo";
    "eff.choose" => "Choose…", "Scegli…";
    "eff.unsaved" => "This shows the saved permissions; save to check your changes.", "Mostra le autorizzazioni salvate; salva per verificare le modifiche.";
    "eff.save_first" => "Save the shared folder to check effective permissions.", "Salva la cartella condivisa per verificare le autorizzazioni effettive.";
    "eff.none" => "No access (not set)", "Nessun accesso (non impostato)";
    "eff.no_source" => "No entry of the matrix applies.", "Nessuna voce della tabella si applica.";
    "eff.decisive" => "Sets the result", "Determina il risultato";
    "eff.rule" => "No access wins over Read & write, which wins over Read only.", "Nessun accesso prevale su Lettura e scrittura, che prevale su Sola lettura.";
    // shared folders: deleting with the data, unshared folders, snapshot folder, remarks
    // (shares.w.<ShareWarningCode>, shares.use.<FolderUsageKind>)
    "shares.delete_all" => "Also delete the folder, its files and snapshots", "Elimina anche la cartella, i file e gli snapshot";
    "shares.delete_all_text" => "The share, its folder with every file, its recycle bin and its snapshots are deleted when you apply the changes.", "La condivisione, la cartella con tutti i file, il cestino e gli snapshot vengono eliminati quando applichi le modifiche.";
    "shares.delete_all_btn" => "Delete everything", "Elimina tutto";
    "shares.del_folder" => "Folder", "Cartella";
    "shares.del_space" => "Space freed", "Spazio liberato";
    "shares.del_snapshots" => "Snapshots", "Snapshot";
    "shares.del_recycle_yes" => "Deleted too", "Eliminato anche questo";
    "shares.del_recycle_no" => "None", "Nessuno";
    "shares.del_used" => "Also in use by:", "In uso anche da:";
    "shares.del_blocked" => "It cannot be deleted now:", "Ora non si può eliminare:";
    "shares.del_irreversible" => "This cannot be undone: the files and the snapshots cannot be recovered.", "L'operazione è irreversibile: i file e gli snapshot non si potranno recuperare.";
    "shares.del_no_rollback" => "The folder is deleted when you apply the pending changes from the bar, at the end of the apply. If a later step of that apply fails and the previous settings are put back, the share comes back but its files and snapshots do not.", "La cartella viene eliminata quando applichi le modifiche in sospeso dalla barra, alla fine dell'applicazione. Se un passo successivo fallisce e le impostazioni precedenti vengono ripristinate, la condivisione torna ma i suoi file e snapshot no.";
    "shares.del_staged" => "Saved: the folder and its snapshots are deleted when you apply the changes", "Salvato: la cartella e i suoi snapshot vengono eliminati quando applichi le modifiche";
    "shares.size_min" => "at least {size}", "almeno {size}";
    "shares.use.nfs_export" => "the NFS export of {name} (removed with the share)", "l'esportazione NFS di {name} (rimossa con la condivisione)";
    "shares.use.share" => "the shared folder {name}", "la cartella condivisa {name}";
    "shares.use.snapshot_job" => "the snapshot job {name}", "il job di snapshot {name}";
    "shares.use.task" => "the scheduled task {name}", "l'attività pianificata {name}";
    "shares.use.container" => "the container {name}", "il container {name}";
    "shares.use.not_on_volume" => "{name} is not directly on a volume: only folders at the top of a volume can be deleted here", "{name} non è direttamente su un volume: qui si possono eliminare solo le cartelle al primo livello di un volume";
    "shares.use.other" => "{name}", "{name}";
    "shares.fdel_title" => "Delete folder {name} permanently?", "Eliminare definitivamente la cartella {name}?";
    "shares.fdel_text" => "The folder with every file, its recycle bin and its snapshots are deleted now.", "La cartella con tutti i file, il cestino e gli snapshot vengono eliminati subito.";
    "shares.fdel_done" => "Folder {name} deleted", "Cartella {name} eliminata";
    "shares.unshared" => "Unshared folders", "Cartelle non condivise";
    "shares.unshared_help" => "Folders on the volumes that no shared folder uses, for example after a share was deleted.", "Cartelle sui volumi che nessuna cartella condivisa usa, per esempio dopo l'eliminazione di una condivisione.";
    "shares.unshared_none" => "Every folder on the volumes is shared.", "Tutte le cartelle dei volumi sono condivise.";
    "shares.uf_folder" => "Folder", "Cartella";
    "shares.size" => "Size", "Dimensione";
    "shares.size_measuring" => "Measuring…", "Misurazione in corso…";
    "shares.size_asleep" => "Not measured: the disks are asleep", "Non misurata: i dischi sono in standby";
    "shares.size_asleep_as_of" => "As of {when} (the disks are asleep)", "Al {when} (i dischi sono in standby)";
    "shares.size_failed" => "Size not available", "Dimensione non disponibile";
    "shares.removed_at" => "Share removed", "Condivisione rimossa";
    "shares.was_shared_as" => "was shared as {name}", "era condivisa come {name}";
    "shares.share_again" => "Share again", "Condividi di nuovo";
    "shares.delete_forever" => "Delete permanently", "Elimina definitivamente";
    "shares.show_snapshots" => "Show snapshots in the folder", "Mostra gli snapshot nella cartella";
    "shares.show_snapshots_help" => "A read-only #snapshot folder at the top of the share, with one folder per snapshot named after its date and time. It needs a snapshot job for this shared folder.", "Una cartella #snapshot di sola lettura al primo livello della condivisione, con una cartella per ogni snapshot che ne riporta data e ora. Serve un job di snapshot per questa cartella condivisa.";
    "shares.w.subvolume_on_apply" => "{path} will be created as a subvolume of volume {volume} when you apply the changes.", "{path} verrà creata come sottovolume del volume {volume} quando applichi le modifiche.";
    "shares.w.subvolume_on_apply_nc" => "{path} will be created as a subvolume of volume {volume}, without compression, when you apply the changes.", "{path} verrà creata come sottovolume del volume {volume}, senza compressione, quando applichi le modifiche.";
    "shares.w.not_subvolume" => "{path} is not a btrfs subvolume: snapshots, previous versions and quotas need one.", "{path} non è un sottovolume btrfs: snapshot, versioni precedenti e quote ne richiedono uno.";
    "shares.w.extra_overrides" => "The extra SMB option {key} replaces a setting managed here.", "L'opzione SMB aggiuntiva {key} sostituisce un'impostazione gestita qui.";
    "shares.w.nfs_uids" => "NFS trusts the user IDs of the clients: they must use the same IDs as this server.", "NFS si fida degli ID utente dei client: devono usare gli stessi ID di questo server.";
    "shares.w.nfs_off" => "NFS is off in the file services settings.", "NFS è disattivato nelle impostazioni dei servizi file.";
    "shares.w.smb_off" => "SMB is off in the file services settings.", "SMB è disattivato nelle impostazioni dei servizi file.";
    "shares.w.tm_no_limit" => "Time Machine without a size limit can fill the volume.", "Time Machine senza un limite di dimensione può riempire il volume.";
    "shares.w.multi_no_nfs" => "The multi-protocol preset turns off client caching; it is meant for folders also exported over NFS.", "Il profilo multiprotocollo disattiva la cache dei client; è pensato per cartelle esportate anche via NFS.";
    "shares.w.no_permissions" => "No permissions set: the folder keeps its own permissions.", "Nessuna autorizzazione impostata: la cartella mantiene i propri permessi.";
    "shares.w.root_only" => "No permissions, and {path} belongs to root with no access for others: nobody but root can use this shared folder. Give users or groups access in its permissions.", "Nessuna autorizzazione e {path} appartiene a root senza accesso per gli altri: nessuno tranne root può usare questa cartella condivisa. Dai accesso a utenti o gruppi nelle sue autorizzazioni.";
    "shares.w.all_denied" => "Every entry denies access: nobody can open this folder.", "Ogni voce nega l'accesso: nessuno può aprire questa cartella.";
    "shares.w.snapfolder_no_job" => "The snapshot folder appears once this shared folder has a snapshot job.", "La cartella degli snapshot compare quando questa cartella condivisa ha un job di snapshot.";
    "shares.w.snapfolder_nfs" => "NFS clients see the snapshot folder only with Include nested mounts on (NFS tab).", "I client NFS vedono la cartella degli snapshot solo con Includi i mount annidati attivo (scheda NFS).";
    // NOCOW folders
    "shares.nocow" => "No copy-on-write", "Senza copy-on-write";
    "shares.nocow_help" => "For video recordings, databases and virtual machine images: files are rewritten in place instead of copied, so they do not fragment.", "Per registrazioni video, database e immagini di macchine virtuali: i file vengono riscritti sul posto invece di essere copiati, così non si frammentano.";
    "shares.nocow_help_existing" => "Files are copied on every change and protected by checksums. No copy-on-write can be turned on while the folder is empty.", "I file vengono copiati a ogni modifica e protetti da checksum. La scrittura senza copy-on-write si può attivare finché la cartella è vuota.";
    "shares.nocow_is_on" => "New files in this folder are rewritten in place, without checksums or compression.", "I nuovi file di questa cartella vengono riscritti sul posto, senza checksum né compressione.";
    "shares.nocow_cost" => "What the files of this folder give up:", "Cosa perdono i file di questa cartella:";
    "shares.nocow_t_sum" => "checksums: damaged data is not detected by a scrub, and a mirror cannot repair it from the other copy", "i checksum: i dati danneggiati non vengono rilevati dalla verifica (scrub) e un mirror non può ripararli dall'altra copia";
    "shares.nocow_t_comp" => "compression", "la compressione";
    "shares.nocow_t_snap" => "space with snapshots: they still work, but every block rewritten after a snapshot takes new space as long as the snapshot is kept", "spazio con gli snapshot: funzionano ancora, ma ogni blocco riscritto dopo uno snapshot occupa nuovo spazio finché lo snapshot resta";
    "shares.nocow_snap_hint" => "Snapshot jobs on this folder work, but rewritten data costs space: keep few snapshots with a short retention, or none.", "I job di snapshot su questa cartella funzionano, ma i dati riscritti occupano spazio: tieni pochi snapshot con una conservazione breve, oppure nessuno.";
    "shares.nocow_turn_on" => "Write without copy-on-write", "Scrivi senza copy-on-write";
    "shares.nocow_turn_off" => "Back to copy-on-write", "Torna al copy-on-write";
    "shares.nocow_on_title" => "No copy-on-write for {name}?", "Senza copy-on-write per {name}?";
    "shares.nocow_off_title" => "Copy-on-write again for {name}?", "Di nuovo copy-on-write per {name}?";
    "shares.nocow_on_text" => "New files in this folder will be rewritten in place. Possible only while the folder is empty.", "I nuovi file di questa cartella verranno riscritti sul posto. Possibile solo finché la cartella è vuota.";
    "shares.nocow_off_text" => "Files created from now on get copy-on-write, checksums and compression again. Files already in the folder stay without copy-on-write until they are copied.", "I file creati da adesso in poi riavranno copy-on-write, checksum e compressione. I file già presenti restano senza copy-on-write finché non vengono copiati.";
    "shares.nocow_done_on" => "{name}: new files are written without copy-on-write.", "{name}: i nuovi file vengono scritti senza copy-on-write.";
    "shares.nocow_done_off" => "{name}: new files get copy-on-write again.", "{name}: i nuovi file riavranno il copy-on-write.";
    "shares.nocow_move" => "Folder not empty? Move the files into a new folder", "La cartella non è vuota? Sposta i file in una cartella nuova";
    "shares.nocow_move_1" => "Create a new shared folder with No copy-on-write on.", "Crea una nuova cartella condivisa con Senza copy-on-write attivo.";
    "shares.nocow_move_2" => "Stop the apps and clients that write to the old folder.", "Ferma le app e i client che scrivono nella vecchia cartella.";
    "shares.nocow_move_3" => "Copy the files with a full copy, not a clone, from a shell: `rsync -aHAX /old/ /new/` (or `cp -a --reflink=never`).", "Copia i file con una copia completa, non un clone, da una shell: `rsync -aHAX /vecchia/ /nuova/` (oppure `cp -a --reflink=never`).";
    "shares.nocow_move_4" => "Check the copy, point the apps at the new folder, then delete the old one.", "Controlla la copia, fai puntare le app alla nuova cartella, poi elimina la vecchia.";
    "shares.nocow_move_space" => "Until the old folder is deleted the files take twice the space.", "Finché la vecchia cartella non viene eliminata, i file occupano il doppio dello spazio.";
    "shares.w.subvolume_on_apply_nocow" => "{path} will be created as a subvolume of volume {volume}, without copy-on-write, when you apply the changes.", "{path} verrà creata come sottovolume del volume {volume}, senza copy-on-write, quando applichi le modifiche.";
    "shares.w.nocow_snapshots" => "{path} has no copy-on-write and a snapshot job: snapshots work, but data rewritten after a snapshot takes new space while the snapshot is kept.", "{path} è senza copy-on-write e ha un job di snapshot: gli snapshot funzionano, ma i dati riscritti dopo uno snapshot occupano nuovo spazio finché lo snapshot resta.";
    "task.shares.nocow" => "Change copy-on-write", "Modifica del copy-on-write";
    "err.folder_not_empty" => "The folder is not empty.", "La cartella non è vuota.";
    "apps.w.nocow_hint" => "On btrfs this folder is created without copy-on-write: files are rewritten in place, without checksums or compression.", "Su btrfs questa cartella viene creata senza copy-on-write: i file vengono riscritti sul posto, senza checksum né compressione.";
    "task.shares.folder-delete" => "Delete folder", "Eliminazione cartella";
    // encrypted shared folders (D-CRYPT E1)
    "crypt.encrypt" => "Encrypt this folder", "Crittografa questa cartella";
    "crypt.encrypt_help" => "Files are stored encrypted and readable only while the folder is unlocked with its password. Chosen now: an existing folder cannot be encrypted later.", "I file sono salvati crittografati e leggibili solo mentre la cartella è sbloccata con la sua password. Si sceglie ora: una cartella esistente non si può crittografare dopo.";
    "crypt.experimental" => "Experimental. An encrypted folder:", "Sperimentale. Una cartella crittografata:";
    "crypt.l_speed" => "is slower than a plain folder, most with many small files;", "è più lenta di una cartella normale, soprattutto con molti file piccoli;";
    "crypt.l_space" => "gains nothing from compression or deduplication;", "non guadagna nulla da compressione o deduplicazione;";
    "crypt.l_snap" => "keeps snapshots, replicas and backups encrypted: restoring needs the password or the recovery key;", "mantiene crittografati snapshot, repliche e backup: per ripristinare servono la password o la chiave di ripristino;";
    "crypt.l_apps" => "is an empty, read-only folder for SMB, NFS and apps while it is locked (after every restart unless it unlocks automatically); containers started meanwhile must be restarted after the unlock;", "è una cartella vuota e in sola lettura per SMB, NFS e app mentre è bloccata (dopo ogni riavvio, salvo sblocco automatico); i container avviati nel frattempo vanno riavviati dopo lo sblocco;";
    "crypt.l_convert" => "to encrypt existing files, copy them into it.", "per crittografare file esistenti, copiali al suo interno.";
    "crypt.created_help" => "Created as {path} when you press Create, with its encrypted data in a hidden folder next to it.", "Creata come {path} quando premi Crea, con i dati crittografati in una cartella nascosta accanto.";
    "crypt.not_installed" => "Encrypted folders need gocryptfs, a package of the Debian archive.", "Le cartelle crittografate richiedono gocryptfs, un pacchetto dell'archivio Debian.";
    "crypt.install" => "Install gocryptfs", "Installa gocryptfs";
    "crypt.password" => "Folder password", "Password della cartella";
    "crypt.password_help" => "8 to 256 characters. Without it or the recovery key the files cannot be read by anyone, rynas included.", "Da 8 a 256 caratteri. Senza la password o la chiave di ripristino nessuno può leggere i file, rynas compreso.";
    "crypt.auto" => "Unlock automatically after a restart", "Sblocca automaticamente dopo un riavvio";
    "crypt.auto_help" => "The password is sealed with this machine's host key, a file on the system disk (no TPM2 found). It protects data disks that leave the NAS without the system disk; it does not protect against someone who takes the whole NAS or has administrator access.", "La password viene sigillata con la chiave host di questa macchina, un file sul disco di sistema (nessun TPM2 trovato). Protegge i dischi dati che lasciano il NAS senza il disco di sistema; non protegge da chi prende l'intero NAS o ha accesso da amministratore.";
    "crypt.auto_help_tpm" => "The password is sealed with this machine's TPM2 and its host key on the system disk: the data disks cannot be read on another machine. It does not protect against someone who takes the whole NAS and starts it, or has administrator access.", "La password viene sigillata con il TPM2 di questa macchina e la sua chiave host sul disco di sistema: i dischi dati non si leggono su un'altra macchina. Non protegge da chi prende l'intero NAS e lo avvia, o ha accesso da amministratore.";
    "crypt.rk_text" => "The recovery key sets a new password when the current one is lost; while you know the password it can be shown again from the folder's settings. Without both, the files are lost for good.", "La chiave di ripristino imposta una nuova password quando quella attuale va persa; finché conosci la password puoi rivederla dalle impostazioni della cartella. Senza entrambe, i file sono persi per sempre.";
    "crypt.unlocked" => "Unlocked", "Sbloccata";
    "crypt.locked" => "Locked", "Bloccata";
    "crypt.missing" => "Encrypted data missing", "Dati crittografati assenti";
    "crypt.encrypted" => "encrypted", "crittografata";
    "crypt.unlock_text" => "While unlocked, the files are readable through SMB, NFS, apps and the file manager, with the folder's permissions.", "Da sbloccata, i file sono leggibili da SMB, NFS, app e gestore file, con i permessi della cartella.";
    "crypt.unlock" => "Unlock", "Sblocca";
    "crypt.unlocked_toast" => "{name} is unlocked.", "{name} è sbloccata.";
    "crypt.lock_title" => "Lock {name}?", "Bloccare {name}?";
    "crypt.lock_text" => "SMB connections to this folder are closed, then it is unmounted. It is not locked while files are open or running containers use it: stop them first. Apps and containers started while it is locked see an empty folder until they are restarted after the next unlock.", "Le connessioni SMB a questa cartella vengono chiuse, poi viene smontata. Non viene bloccata finché ci sono file aperti o container in esecuzione che la usano: fermali prima. App e container avviati mentre è bloccata vedono una cartella vuota finché non vengono riavviati dopo il prossimo sblocco.";
    "crypt.lock" => "Lock", "Blocca";
    "crypt.locked_toast" => "{name} is locked.", "{name} è bloccata.";
    "crypt.busy" => "The folder is in use: close the open files and stop the apps using it, then try again.", "La cartella è in uso: chiudi i file aperti e ferma le app che la usano, poi riprova.";
    "crypt.title" => "Encrypted folder", "Cartella crittografata";
    "crypt.existing_help" => "Snapshots, replicas and backups of this folder hold its encrypted data, stored here:", "Snapshot, repliche e backup di questa cartella contengono i suoi dati crittografati, salvati qui:";
    "crypt.pw_title" => "Password of {name}", "Password di {name}";
    "crypt.pw_with" => "Prove it with", "Conferma con";
    "crypt.pw_current" => "Current password", "Password attuale";
    "crypt.pw_recovery" => "Recovery key", "Chiave di ripristino";
    "crypt.pw_recovery_help" => "64 hexadecimal digits, as shown at the creation (dashes and spaces are ignored).", "64 cifre esadecimali, come mostrate alla creazione (trattini e spazi vengono ignorati).";
    "crypt.pw_change" => "Change password", "Cambia password";
    "crypt.pw_done" => "{name}: new password set.", "{name}: nuova password impostata.";
    "crypt.auto_title" => "Unlock {name} automatically", "Sblocca {name} automaticamente";
    "crypt.in_use" => "Still in use", "Ancora in uso";
    "crypt.too_old" => "gocryptfs {version} is too old: encrypted folders need version 2.5 or newer (Debian 13 and later ship it).", "gocryptfs {version} è troppo vecchio: le cartelle crittografate richiedono la versione 2.5 o successiva (inclusa da Debian 13 in poi).";
    "crypt.rk_later" => "The folder was created but its recovery key could not be fetched: show it from the folder's settings with its password", "La cartella è stata creata ma non è stato possibile ottenere la chiave di ripristino: mostrala dalle impostazioni della cartella con la sua password";
    "crypt.stale_text" => "Unlocked. These running containers were started while the folder was locked and still see an empty folder: restart them to give them the files.", "Sbloccata. Questi container in esecuzione sono stati avviati mentre la cartella era bloccata e vedono ancora una cartella vuota: riavviali per dar loro i file.";
    "crypt.later" => "Later", "Più tardi";
    "crypt.restart" => "Restart containers", "Riavvia i container";
    "crypt.restarted" => "Containers restarted.", "Container riavviati.";
    "crypt.auto_off" => "Automatic unlock turned off: the new password could not be sealed", "Sblocco automatico disattivato: non è stato possibile sigillare la nuova password";
    "crypt.reveal_text" => "The recovery key is read from the folder with its password. Keep it outside the NAS.", "La chiave di ripristino viene letta dalla cartella con la sua password. Conservala fuori dal NAS.";
    "crypt.rk_keep" => "Store it somewhere safe outside the NAS: with it anyone can set a new password for this folder.", "Conservala in un posto sicuro fuori dal NAS: con essa chiunque può impostare una nuova password per questa cartella.";
    "shares.w.crypt_locked" => "{path} is encrypted and locked: SMB and NFS clients cannot connect; apps and containers started while it is locked see an empty folder and must be restarted after it is unlocked.", "{path} è crittografata e bloccata: i client SMB e NFS non possono connettersi; app e container avviati mentre è bloccata vedono una cartella vuota e vanno riavviati dopo lo sblocco.";
    "err.crypt_wrong_password" => "Wrong password or recovery key", "Password o chiave di ripristino sbagliata";
    "err.crypt_busy" => "The encrypted folder is in use.", "La cartella crittografata è in uso.";
    "err.crypt_not_installed" => "gocryptfs is not installed.", "gocryptfs non è installato.";
    "err.ssh_lockout" => "With this change no administrator could log in over SSH.", "Con questa modifica nessun amministratore potrebbe più entrare via SSH.";
    "task.crypt.create" => "Create encrypted folder", "Creazione cartella crittografata";
    "task.crypt.install" => "Install gocryptfs", "Installazione di gocryptfs";
    // file services
    "svc.title" => "File Services", "Servizi file";
    "svc.subtitle" => "SMB and NFS settings for every shared folder", "Impostazioni SMB e NFS per tutte le cartelle condivise";
    "svc.smb" => "SMB", "SMB";
    "svc.nfs" => "NFS", "NFS";
    "svc.running" => "Running", "In esecuzione";
    "svc.stopped" => "Stopped", "Fermo";
    "svc.off" => "Off", "Disattivato";
    "svc.exports" => "NFS exports", "Esportazioni NFS";
    "svc.no_exports" => "No shared folder is exported over NFS.", "Nessuna cartella condivisa è esportata via NFS.";
    "svc.exports_help" => "Exports and their client rules are edited in each shared folder (NFS tab).", "Le esportazioni e le regole dei client si modificano in ogni cartella condivisa (scheda NFS).";
    // SSH service
    "ssh.title" => "SSH", "SSH";
    "ssh.subtitle" => "Terminal and SFTP access to this NAS", "Accesso da terminale e SFTP a questo NAS";
    "ssh.open_text" => "Terminal and SFTP access: port, password login, who may log in, keys.", "Accesso da terminale e SFTP: porta, accesso con password, chi può entrare, chiavi.";
    "ssh.settings" => "Settings", "Impostazioni";
    "ssh.running_on" => "Running on {ports}", "In esecuzione su {ports}";
    "ssh.enabled" => "SSH service", "Servizio SSH";
    "ssh.enabled_help" => "Accounts with SSH access log in with a terminal or SFTP.", "Gli account con accesso SSH entrano con un terminale o in SFTP.";
    "ssh.port" => "Port", "Porta";
    "ssh.port_help" => "22 is the standard; another port only cuts the noise of scanners.", "22 è lo standard; un'altra porta riduce solo il rumore degli scanner.";
    "ssh.root" => "Root login", "Accesso di root";
    "ssh.root_help" => "Better: administrators log in with their own account and use sudo.", "Meglio: gli amministratori entrano con il proprio account e usano sudo.";
    "ssh.root_no" => "No", "No";
    "ssh.root_keys" => "Keys only", "Solo con chiave";
    "ssh.root_yes" => "Yes, also with password", "Sì, anche con password";
    "ssh.password" => "Password login", "Accesso con password";
    "ssh.password_help" => "Off: keys only, the safest choice. At least one administrator needs a key.", "Disattivato: solo chiavi, la scelta più sicura. Almeno un amministratore deve avere una chiave.";
    "ssh.allow" => "Who may log in", "Chi può entrare";
    "ssh.allow_everyone" => "Every account", "Ogni account";
    "ssh.allow_listed" => "Only accounts with SSH access", "Solo account con accesso SSH";
    "ssh.allow_everyone_help" => "Every account with a login shell can log in.", "Ogni account con una shell di login può entrare.";
    "ssh.allow_listed_help" => "Accounts with Shell or SFTP in Users, the PC backup and certificate accounts, and the groups below.", "Gli account con Shell o SFTP in Utenti, gli account del backup dei PC e dei certificati, e i gruppi qui sotto.";
    "ssh.allow_groups" => "More groups", "Altri gruppi";
    "ssh.allow_groups_help" => "Comma separated, e.g. sudo for accounts made outside rynas.", "Separati da virgola, ad es. sudo per gli account creati fuori da rynas.";
    "ssh.who" => "Accounts", "Account";
    "ssh.admin" => "administrator", "amministratore";
    "ssh.level_shell" => "shell", "shell";
    "ssh.level_sftp" => "SFTP only", "solo SFTP";
    "ssh.level_none" => "no SSH", "niente SSH";
    "ssh.n_keys" => "keys: {n}", "chiavi: {n}";
    "ssh.keys_unread" => "keys not read (disk asleep)", "chiavi non lette (disco in standby)";
    "ssh.tcp" => "Port forwarding", "Inoltro delle porte";
    "ssh.tcp_help" => "Tunnels through SSH (ssh -L, -R, SOCKS). Leave off unless needed.", "Tunnel attraverso SSH (ssh -L, -R, SOCKS). Lascialo spento se non serve.";
    "ssh.agent" => "Agent forwarding", "Inoltro dell'agente SSH";
    "ssh.agent_help" => "Sessions may use the keys of the computer that connects.", "Le sessioni possono usare le chiavi del computer che si collega.";
    "ssh.x11" => "X11 forwarding", "Inoltro X11";
    "ssh.x11_help" => "Graphical programs over SSH; rarely needed on a NAS.", "Programmi grafici via SSH; raramente utile su un NAS.";
    "ssh.lockout" => "With these settings no administrator could log in over SSH. Add a key or allow an administrator first.", "Con queste impostazioni nessun amministratore potrebbe entrare via SSH. Prima aggiungi una chiave o consenti un amministratore.";
    "ssh.trial_note" => "Access changes are tested: try a new SSH connection, then keep them here, or they go back by themselves.", "Le modifiche all'accesso vengono provate: apri una nuova connessione SSH, poi confermale qui, altrimenti tornano indietro da sole.";
    "ssh.trial_title" => "Test the new SSH settings", "Prova le nuove impostazioni SSH";
    "ssh.trial_text" => "Connect from a new terminal with the command below. If it works, keep the settings; if not, they go back by themselves. Open sessions stay connected.", "Collegati da un nuovo terminale con il comando qui sotto. Se funziona, mantieni le impostazioni; altrimenti tornano indietro da sole. Le sessioni aperte restano collegate.";
    "ssh.trial_other" => "{user} is testing new SSH settings: they go back by themselves unless confirmed.", "{user} sta provando nuove impostazioni SSH: tornano indietro da sole se non vengono confermate.";
    "ssh.left_sr" => "{n} seconds left", "{n} secondi rimasti";
    "ssh.copy" => "Copy", "Copia";
    "ssh.keep" => "Keep", "Mantieni";
    "ssh.undo" => "Undo now", "Annulla ora";
    "ssh.kept" => "SSH settings kept.", "Impostazioni SSH mantenute.";
    "ssh.undone" => "The previous SSH settings are back.", "Sono tornate le impostazioni SSH precedenti.";
    "ssh.saved" => "SSH settings saved.", "Impostazioni SSH salvate.";
    "ssh.host_keys" => "Host keys", "Chiavi dell'host";
    "ssh.host_keys_help" => "On the first connection your computer shows one of these: check that it matches before answering yes.", "Alla prima connessione il computer ne mostra una: controlla che corrisponda prima di rispondere sì.";
    "ssh.no_host_keys" => "No host keys yet: they are made when openssh-server is installed.", "Ancora nessuna chiave dell'host: vengono create all'installazione di openssh-server.";
    "ssh.logins" => "Logins, last 24 hours", "Accessi, ultime 24 ore";
    "ssh.logins_ok" => "{n} logins", "{n} accessi";
    "ssh.logins_failed" => "{n} failed", "{n} falliti";
    "ssh.logins_once" => "once", "una volta";
    "ssh.logins_recent" => "From the last {n} entries", "Dagli ultimi {n} eventi";
    "ssh.logins_timeline" => "Timeline", "Cronologia";
    "ssh.logins_grouped" => "Grouped", "Raggruppati";
    "ssh.logins_last" => "last {when}", "ultimo {when}";
    "ssh.logins_top" => "Most failures from: {list}", "Più tentativi falliti da: {list}";
    "ssh.logins_since" => "Too many entries today: counted since {when}", "Troppe voci oggi: conteggio dal {when}";
    "ssh.login_ok" => "logged in", "entrato";
    "ssh.login_failed" => "failed", "fallito";
    "ssh.login_invalid" => "unknown account", "account sconosciuto";
    "ssh.refresh" => "Refresh", "Aggiorna";
    "ssh.keys" => "Authorized keys", "Chiavi autorizzate";
    "ssh.account" => "Account", "Account";
    "ssh.no_accounts" => "No account can use SSH: give one Shell or SFTP access in Users.", "Nessun account può usare SSH: dai a uno l'accesso Shell o SFTP in Utenti.";
    "ssh.no_keys" => "No keys: this account logs in with its password only.", "Nessuna chiave: questo account entra solo con la password.";
    "ssh.key_options" => "with options", "con opzioni";
    "ssh.key_weak" => "weak", "debole";
    "ssh.key_remove" => "Remove key", "Rimuovi chiave";
    "ssh.key_new" => "Public key", "Chiave pubblica";
    "ssh.key_new_help" => "The line of the .pub file, e.g. made with ssh-keygen -t ed25519.", "La riga del file .pub, ad es. creata con ssh-keygen -t ed25519.";
    "ssh.key_add" => "Add key", "Aggiungi chiave";
    "ssh.key_added" => "Key added.", "Chiave aggiunta.";
    "ssh.key_removed" => "Key removed.", "Chiave rimossa.";
    "ssh.key_remove_title" => "Remove the key {name}?", "Rimuovere la chiave {name}?";
    "ssh.key_remove_text" => "Whoever uses this key can no longer log in as {user}.", "Chi usa questa chiave non potrà più entrare come {user}.";
    "ssh.other_lines" => "The file also has {n} lines that are not keys (comments or other entries): they stay as they are and are not managed here.", "Il file ha anche {n} righe che non sono chiavi (commenti o altre voci): restano come sono e non si gestiscono da qui.";
    "ssh.p.not_installed" => "The SSH server is not installed: `apt install openssh-server`.", "Il server SSH non è installato: `apt install openssh-server`.";
    "ssh.p.not_included" => "`/etc/ssh/sshd_config` does not include `sshd_config.d/*.conf`: these settings are not read. Put that line back.", "`/etc/ssh/sshd_config` non include `sshd_config.d/*.conf`: queste impostazioni non vengono lette. Rimetti quella riga.";
    "ssh.p.overridden" => "`{file}` also sets `{key}`: its value counts instead of this page's.", "Anche `{file}` imposta `{key}`: vale il suo valore invece di quello di questa pagina.";
    "ssh.p.extra_ports" => "sshd also listens on {ports} (set in another file).", "sshd ascolta anche su {ports} (impostata in un altro file).";
    "ssh.p.drift" => "The settings file was changed by hand: saving writes it again.", "Il file delle impostazioni è stato modificato a mano: salvando viene riscritto.";
    "ssh.p.socket" => "SSH starts on demand (`ssh.socket`): saving switches it to the `ssh.service` daemon.", "SSH parte su richiesta (`ssh.socket`): salvando passa al demone `ssh.service`.";
    "ssh.p.not_running" => "The SSH service is on but not running.", "Il servizio SSH è attivo ma non in esecuzione.";
    "smb.enabled" => "Enable SMB", "Attiva SMB";
    "smb.enabled_help" => "File sharing for Windows, macOS and Linux.", "Condivisione file per Windows, macOS e Linux.";
    "smb.workgroup" => "Workgroup", "Gruppo di lavoro";
    "smb.workgroup_help" => "Usually WORKGROUP.", "Di solito WORKGROUP.";
    "smb.min_protocol" => "Minimum protocol", "Protocollo minimo";
    "smb.min_protocol_help" => "SMB1 is never offered.", "SMB1 non viene mai offerto.";
    "smb.proto_smb2" => "SMB 2.1 (Windows 7+)", "SMB 2.1 (Windows 7+)";
    "smb.proto_smb3" => "SMB 3.0 (Windows 8+)", "SMB 3.0 (Windows 8+)";
    "smb.proto_smb311" => "SMB 3.1.1 (Windows 10+)", "SMB 3.1.1 (Windows 10+)";
    "smb.server_string" => "Description", "Descrizione";
    "smb.server_string_help" => "Shown next to the NAS name in some clients.", "Mostrata accanto al nome del NAS in alcuni client.";
    "smb.wsdd" => "Windows network discovery", "Rilevamento di rete Windows";
    "smb.wsdd_help" => "The NAS appears under Network in Windows Explorer (WS-Discovery).", "Il NAS compare in Rete in Esplora file di Windows (WS-Discovery).";
    "smb.mdns" => "Bonjour announcements", "Annunci Bonjour";
    "smb.mdns_help" => "The NAS appears in the macOS Finder and on Linux (mDNS).", "Il NAS compare nel Finder di macOS e su Linux (mDNS).";
    "smb.encrypt" => "Require encryption on every share", "Richiedi la crittografia su ogni condivisione";
    "smb.encrypt_help" => "Clients without SMB 3 encryption cannot connect.", "I client senza crittografia SMB 3 non possono connettersi.";
    "smb.netbios" => "NetBIOS name service", "Servizio nomi NetBIOS";
    "smb.netbios_help" => "Only for very old clients; off by default.", "Solo per client molto vecchi; disattivato di default.";
    "smb.interfaces" => "Listen on", "In ascolto su";
    "smb.interfaces_help" => "Interfaces or addresses, comma separated. Empty: all.", "Interfacce o indirizzi, separati da virgole. Vuoto: tutti.";
    "smb.extra" => "Extra global options", "Opzioni globali aggiuntive";
    "smb.extra_help" => "key = value lines, checked with testparm when applying.", "Righe chiave = valore, verificate con testparm all'applicazione.";
    "recycle.title" => "Empty recycle bins", "Svuota i cestini";
    "recycle.help" => "Runs now what the daily cleanup does: removes items deleted longer ago than the retention.", "Esegue subito la pulizia giornaliera: rimuove gli elementi eliminati da più tempo della conservazione.";
    "shares.folder" => "Shared folder", "Cartella condivisa";
    "recycle.all" => "All with a recycle bin", "Tutte quelle con il cestino";
    "recycle.days" => "Older than (days)", "Più vecchi di (giorni)";
    "recycle.days_help" => "Empty: each folder's retention.", "Vuoto: la conservazione di ogni cartella.";
    "recycle.run" => "Empty now", "Svuota ora";
    "recycle.done" => "Recycle bins emptied.", "Cestini svuotati.";
    "nfs.enabled" => "Enable NFS", "Attiva NFS";
    "nfs.enabled_help" => "NFS 4.1 and 4.2 for Linux and Unix clients.", "NFS 4.1 e 4.2 per client Linux e Unix.";
    "nfs.v3" => "Also allow NFSv3", "Consenti anche NFSv3";
    "nfs.v3_help" => "For older clients; needs rpcbind (port 111).", "Per client più vecchi; richiede rpcbind (porta 111).";
    "nfs.threads" => "Server threads", "Thread del server";
    "nfs.threads_help" => "8 suits most homes and small offices.", "8 va bene per la maggior parte delle case e dei piccoli uffici.";
    "nfs.mountd" => "mountd port", "Porta mountd";
    "nfs.statd" => "statd port", "Porta statd";
    "nfs.lockd" => "lockd port", "Porta lockd";
    "nfs.ports_help" => "Fixed ports, so a firewall can allow NFSv3.", "Porte fisse, così un firewall può consentire NFSv3.";
    // security page
    "sec.subtitle" => "Your account, session and two-factor authentication", "Account, sessione e verifica in due passaggi";
    "sec.account" => "Account", "Account";
    "sec.user" => "User", "Utente";
    "sec.role" => "Role", "Ruolo";
    "sec.realm" => "Account type", "Tipo di account";
    "sec.realm_pam" => "System user", "Utente di sistema";
    "sec.realm_rynas" => "rynas user", "Utente rynas";
    "sec.since" => "Signed in", "Accesso";
    "sec.until" => "Session ends", "Fine sessione";
    "sec.idle" => "Idle timeout", "Timeout di inattività";
    "sec.idle_min" => "{n} min", "{n} min";
    "sec.totp" => "Two-factor authentication", "Verifica in due passaggi";
    "sec.2fa_on" => "2FA on", "2FA attiva";
    "sec.on" => "On", "Attiva";
    "sec.off" => "Off", "Disattivata";
    "sec.off_text" => "Protect your account with a code from an authenticator app (Aegis, 2FAS, Google Authenticator, 1Password…) in addition to your password.", "Proteggi l'account con un codice di un'app di autenticazione (Aegis, 2FAS, Google Authenticator, 1Password…) oltre alla password.";
    "sec.on_text" => "Signing in asks for a code from your authenticator app. Keep your recovery codes somewhere safe.", "L'accesso chiede un codice dell'app di autenticazione. Conserva i codici di recupero in un posto sicuro.";
    "sec.other_sessions" => "Turning it on signs you out everywhere else: other browsers and devices will have to sign in again.", "Attivandola esci da tutte le altre sessioni: altri browser e dispositivi dovranno accedere di nuovo.";
    "sec.enable" => "Set up", "Configura";
    "sec.step_scan" => "Scan the QR code", "Scansiona il codice QR";
    "sec.step_scan_text" => "Open your authenticator app and add an account by scanning this code.", "Apri l'app di autenticazione e aggiungi un account scansionando questo codice.";
    "sec.qr_label" => "QR code with the two-factor authentication secret", "Codice QR con il segreto della verifica in due passaggi";
    "sec.manual" => "Can't scan it? Enter this key:", "Non riesci a scansionarlo? Inserisci questa chiave:";
    "sec.copy_secret" => "Copy key", "Copia chiave";
    "sec.step_code" => "Enter the code the app shows", "Inserisci il codice mostrato dall'app";
    "sec.activate" => "Turn on", "Attiva";
    "sec.enabled_now" => "Two-factor authentication is on.", "La verifica in due passaggi è attiva.";
    "sec.codes_title" => "Recovery codes", "Codici di recupero";
    "sec.codes_text" => "Use one of these codes to sign in if you lose your phone. Each code works once. They are shown only now: save them somewhere safe.", "Usa uno di questi codici per accedere se perdi il telefono. Ogni codice vale una volta sola. Vengono mostrati solo ora: salvali in un posto sicuro.";
    "sec.codes_file_head" => "rynas recovery codes for {user} on {host}. Each code works once.", "Codici di recupero rynas per {user} su {host}. Ogni codice vale una volta sola.";
    "sec.copy_codes" => "Copy", "Copia";
    "sec.download" => "Download .txt", "Scarica .txt";
    "sec.saved" => "I saved my recovery codes", "Ho salvato i codici di recupero";
    "sec.done" => "Done", "Fatto";
    "sec.disable" => "Turn off", "Disattiva";
    "sec.disable_text" => "Enter a code from your authenticator app or a recovery code to turn off two-factor authentication.", "Inserisci un codice dell'app di autenticazione o un codice di recupero per disattivare la verifica in due passaggi.";
    "sec.code_or_recovery" => "Authentication or recovery code", "Codice di autenticazione o di recupero";
    "sec.demo" => "Not available with demo data: there is no account to protect.", "Non disponibile con i dati demo: non c'è un account da proteggere.";
    "err.totp_already_enabled" => "Two-factor authentication is already on.", "La verifica in due passaggi è già attiva.";
    "err.totp_not_enrolled" => "The setup has expired. Start again.", "La configurazione è scaduta. Ricomincia.";
    // notifications, task scheduler, snapshots (P3)
    "open" => "Open", "Apri";
    "day.1" => "Mon", "Lun";
    "day.2" => "Tue", "Mar";
    "day.3" => "Wed", "Mer";
    "day.4" => "Thu", "Gio";
    "day.5" => "Fri", "Ven";
    "day.6" => "Sat", "Sab";
    "day.7" => "Sun", "Dom";
    "day.long.1" => "Monday", "Lunedì";
    "day.long.2" => "Tuesday", "Martedì";
    "day.long.3" => "Wednesday", "Mercoledì";
    "day.long.4" => "Thursday", "Giovedì";
    "day.long.5" => "Friday", "Venerdì";
    "day.long.6" => "Saturday", "Sabato";
    "day.long.7" => "Sunday", "Domenica";
    // weekly schedule grid (week_grid.rs)
    "wg.level" => "Level to apply", "Livello da applicare";
    "wg.hours" => "Hours", "Ore";
    "wg.cell" => "{day} {from}–{to}: {level}", "{day} {from}–{to}: {level}";
    "wg.fill_day" => "{day}: all hours to {level}", "{day}: tutte le ore a {level}";
    "wg.fill_hour" => "{from}–{to} every day to {level}", "{from}–{to} di ogni giorno a {level}";
    "wg.all_week" => "All week: {level}", "Tutta la settimana: {level}";
    "wg.invalid" => "The saved schedule could not be read: it shows as {level} until you save it.", "La pianificazione salvata non era leggibile: appare come {level} finché non la salvi.";
    "wg.help" => "Pick a level, then click or drag over the hours; a day or an hour heading fills its row or column. Touch: tap an hour to apply the level, hold it a moment and then drag to fill a rectangle, swipe to scroll the page. Keyboard: arrows move, Space applies the level, Shift+Space fills the day, Shift+Enter fills the hour on every day, 1–{n} pick the level, Esc cancels a drag.", "Scegli un livello, poi fai clic o trascina sulle ore; l'intestazione di un giorno o di un'ora riempie la sua riga o colonna. Al tocco: tocca un'ora per applicare il livello, tienila premuta un attimo e poi trascina per riempire un rettangolo, scorri per spostare la pagina. Da tastiera: le frecce spostano, Spazio applica il livello, Maiusc+Spazio riempie il giorno, Maiusc+Invio riempie l'ora in tutti i giorni, 1–{n} scelgono il livello, Esc annulla un trascinamento.";
    "wgd.title" => "Weekly schedule grid", "Griglia settimanale";
    "wgd.subtitle" => "Component preview, demo build only", "Anteprima del componente, solo nella build demo";
    "wgd.led" => "LED brightness by hour", "Luminosità dei LED per ora";
    "wgd.window" => "Backup window", "Finestra di backup";
    "wgd.bandwidth" => "Bandwidth by hour", "Banda per ora";
    "wgd.preview" => "Compact previews", "Anteprime compatte";
    "wgd.normal" => "Normal", "Normale";
    "wgd.reduced" => "Reduced", "Ridotto";
    "wgd.off" => "Off", "Spento";
    "wgd.allowed" => "Run", "Esegui";
    "wgd.paused" => "Paused", "In pausa";
    "wgd.unlimited" => "Unlimited", "Illimitata";
    "wgd.limited" => "Limited", "Limitata";
    "wgd.slow" => "Slow", "Lenta";
    "nav.snapshots" => "Snapshots", "Snapshot";
    "nt.center" => "Notification center", "Centro notifiche";
    "nt.subtitle" => "Events from storage, hardware, tasks and services, with acknowledgement and silences", "Eventi di storage, hardware, attività e servizi, con presa visione e silenziamenti";
    "nt.settings" => "Notification settings", "Impostazioni notifiche";
    "nt.settings_title" => "Notifications", "Notifiche";
    "nt.settings_subtitle" => "Where notifications go (email, ntfy, Telegram, Gotify, webhook) and which events reach them", "Dove vanno le notifiche (email, ntfy, Telegram, Gotify, webhook) e quali eventi le raggiungono";
    "nt.open_text" => "Delivery targets with a test message, rules by severity and category, quiet hours and silences.", "Destinazioni con messaggio di prova, regole per gravità e categoria, ore di silenzio e silenziamenti.";
    "nt.view_all" => "All notifications", "Tutte le notifiche";
    "nt.load_failed" => "Couldn't load the notifications.", "Impossibile caricare le notifiche.";
    "nt.ack" => "Acknowledge", "Presa visione";
    "nt.ack_all" => "Acknowledge all", "Presa visione di tutte";
    "nt.acked_n" => "{n} notifications acknowledged", "Presa visione di {n} notifiche";
    "nt.acked" => "Acknowledged", "Vista";
    "nt.resolved" => "Resolved", "Risolta";
    "nt.active" => "Active", "Attiva";
    "nt.silenced" => "Silenced", "Silenziata";
    "nt.silence" => "Silence", "Silenzia";
    "nt.unsilence" => "Remove silence", "Rimuovi silenziamento";
    "nt.details" => "Details", "Dettagli";
    "nt.times" => "{n} times", "{n} volte";
    "nt.load_more" => "Load more", "Carica altri";
    "nt.none" => "No notifications", "Nessuna notifica";
    "nt.none_text" => "Nothing matches these filters. Events appear here as soon as rynas records them.", "Nessuna corrispondenza con questi filtri. Gli eventi compaiono qui appena rynas li registra.";
    "nt.col.severity" => "Severity", "Gravità";
    "nt.col.event" => "Event", "Evento";
    "nt.col.category" => "Category", "Categoria";
    "nt.col.when" => "When", "Quando";
    "nt.col.name" => "Name", "Nome";
    "nt.col.kind" => "Type", "Tipo";
    "nt.col.delivery" => "Sends to", "Invia a";
    "nt.col.matches" => "Matches", "Corrisponde a";
    "nt.col.sends_to" => "Targets", "Destinazioni";
    "nt.col.until" => "Until", "Fino a";
    "nt.col.created" => "Created", "Creato";
    "nt.f.any_sev" => "Any severity", "Qualsiasi gravità";
    "nt.f.notice" => "Notice and above", "Avviso e superiori";
    "nt.f.warning" => "Warning and above", "Attenzione e superiori";
    "nt.f.error" => "Error and above", "Errore e superiori";
    "nt.f.any_cat" => "Any category", "Qualsiasi categoria";
    "nt.f.unacked" => "Not acknowledged", "Da vedere";
    "nt.f.all" => "All", "Tutte";
    "nt.f.acked" => "Acknowledged", "Viste";
    "nt.sev.info" => "Info", "Informazione";
    "nt.sev.notice" => "Notice", "Avviso";
    "nt.sev.warning" => "Warning", "Attenzione";
    "nt.sev.error" => "Error", "Errore";
    "nt.sev.critical" => "Critical", "Critico";
    "nt.cat.storage" => "Storage", "Storage";
    "nt.cat.hardware" => "Hardware", "Hardware";
    "nt.cat.ups" => "UPS", "UPS";
    "nt.cat.system" => "System", "Sistema";
    "nt.cat.security" => "Security", "Sicurezza";
    "nt.cat.update" => "Updates", "Aggiornamenti";
    "nt.cat.backup" => "Backup", "Backup";
    "nt.cat.app" => "Apps", "App";
    "nt.cat.task" => "Tasks", "Attività";
    "nt.source" => "Source", "Origine";
    "nt.key" => "Key", "Chiave";
    "nt.key_help" => "End it with * to silence every key that starts with it.", "Termina con * per silenziare tutte le chiavi che iniziano così.";
    "nt.key_required" => "Enter a key.", "Inserisci una chiave.";
    "nt.first" => "First seen", "Prima volta";
    "nt.last" => "Last seen", "Ultima volta";
    "nt.fields" => "Fields", "Campi";
    "nt.silence_title" => "Silence notifications", "Silenzia le notifiche";
    "nt.silence_text" => "Events with this key are still recorded but not sent, and reminders stop. Critical events are silenced too.", "Gli eventi con questa chiave restano registrati ma non vengono inviati, e i promemoria si fermano. Vengono silenziati anche gli eventi critici.";
    "nt.silence_for" => "For", "Per";
    "nt.for_1h" => "1 hour", "1 ora";
    "nt.for_8h" => "8 hours", "8 ore";
    "nt.for_1d" => "1 day", "1 giorno";
    "nt.for_7d" => "7 days", "7 giorni";
    "nt.for_ever" => "Until removed", "Fino alla rimozione";
    "nt.silenced_ok" => "Silence added", "Silenziamento aggiunto";
    "nt.tab.targets" => "Targets", "Destinazioni";
    "nt.tab.rules" => "Rules", "Regole";
    "nt.tab.silences" => "Silences", "Silenziamenti";
    "nt.targets_help" => "A target is where messages go. Secrets (passwords, tokens) are write-only: they are never shown again.", "Una destinazione è dove vanno i messaggi. I segreti (password, token) si possono solo scrivere: non vengono più mostrati.";
    "nt.rules_help" => "A rule picks events by severity, category, source or field and sends them to targets. Each target gets an event once, even when several rules match.", "Una regola sceglie gli eventi per gravità, categoria, origine o campo e li invia alle destinazioni. Ogni destinazione riceve un evento una volta sola, anche se più regole corrispondono.";
    "nt.silences_help" => "Silenced keys are recorded but not delivered. Use * at the end to silence a whole family of events.", "Le chiavi silenziate vengono registrate ma non inviate. Usa * alla fine per silenziare un'intera famiglia di eventi.";
    "nt.add_target" => "Add target", "Aggiungi destinazione";
    "nt.add_silence" => "Add silence", "Aggiungi silenziamento";
    "nt.no_targets" => "No targets", "Nessuna destinazione";
    "nt.no_targets_text" => "Add an email account, an ntfy topic or a Telegram chat to receive notifications away from the NAS.", "Aggiungi un account email, un argomento ntfy o una chat Telegram per ricevere le notifiche lontano dal NAS.";
    "nt.no_rules" => "No rules", "Nessuna regola";
    "nt.no_rules_text" => "Without rules, events are only listed in the notification center.", "Senza regole gli eventi compaiono solo nel centro notifiche.";
    "nt.no_silences" => "No silences", "Nessun silenziamento";
    "nt.no_silences_text" => "Silence a noisy event from the notification center or add a key here.", "Silenzia un evento ripetitivo dal centro notifiche o aggiungi qui una chiave.";
    "nt.delete_target" => "Delete target {name}?", "Eliminare la destinazione {name}?";
    "nt.delete_target_text" => "Messages waiting for this target are dropped.", "I messaggi in attesa per questa destinazione vengono scartati.";
    "nt.target_in_use" => "A rule still sends to this target: remove it from the rule first.", "Una regola invia ancora a questa destinazione: toglila prima dalla regola.";
    "nt.delete_rule" => "Delete rule {name}?", "Eliminare la regola {name}?";
    "nt.delete_rule_text" => "Events it matched are no longer sent to its targets.", "Gli eventi che selezionava non vengono più inviati alle sue destinazioni.";
    "nt.target_saved" => "Target {name} saved", "Destinazione {name} salvata";
    "nt.rule_saved" => "Rule {name} saved", "Regola {name} salvata";
    "nt.test" => "Send test message", "Invia messaggio di prova";
    "nt.test_ok" => "Test message sent to {name} ({ms} ms)", "Messaggio di prova inviato a {name} ({ms} ms)";
    "nt.test_failed" => "Test message to {name} failed:", "Messaggio di prova a {name} non riuscito:";
    "nt.enabled" => "Enabled", "Attiva";
    "nt.disabled" => "Disabled", "Disattivata";
    "nt.failing" => "Failing", "In errore";
    "nt.delivering" => "Working", "Funzionante";
    "nt.never_used" => "Not used yet", "Non ancora usata";
    "nt.last_ok" => "Last delivery {when}", "Ultimo invio {when}";
    "nt.queued" => "{n} queued", "{n} in coda";
    "nt.quiet" => "Quiet hours", "Ore di silenzio";
    "nt.inverted" => "Inverted", "Invertita";
    "nt.kind.smtp" => "Email (SMTP)", "Email (SMTP)";
    "nt.kind.ntfy" => "ntfy", "ntfy";
    "nt.kind.telegram" => "Telegram", "Telegram";
    "nt.kind.gotify" => "Gotify", "Gotify";
    "nt.kind.webhook" => "Webhook", "Webhook";
    "nt.kind.ui" => "Web interface", "Interfaccia web";
    "nt.kind.ui_help" => "Events are always listed in the notification center: as a rule target this means no external delivery.", "Gli eventi compaiono sempre nel centro notifiche: come destinazione di una regola significa nessun invio esterno.";
    "nt.sec.starttls" => "STARTTLS (port 587)", "STARTTLS (porta 587)";
    "nt.sec.tls" => "TLS (port 465)", "TLS (porta 465)";
    "nt.sec.none" => "None (local relay only)", "Nessuna (solo relay locale)";
    "nt.m.post" => "POST", "POST";
    "nt.m.put" => "PUT", "PUT";
    "nt.m.get" => "GET", "GET";
    "nt.mode.all" => "All conditions", "Tutte le condizioni";
    "nt.mode.any" => "Any condition", "Almeno una condizione";
    "nt.secret.password" => "Password", "Password";
    "nt.secret.token" => "Access token", "Token di accesso";
    "nt.secret.bot_token" => "Bot token", "Token del bot";
    "nt.secret_name" => "Secret name", "Nome del segreto";
    "nt.secret_set" => "Set — type to replace", "Impostato — scrivi per sostituirlo";
    "nt.secret_remove" => "Remove", "Rimuovi";
    "nt.f.enabled" => "Enabled", "Attiva";
    "nt.f.language" => "Message language", "Lingua dei messaggi";
    "nt.f.language_help" => "Subjects, severity names and dates of the messages.", "Oggetti, nomi delle gravità e date dei messaggi.";
    "nt.f.connection" => "Connection", "Connessione";
    "nt.f.host" => "SMTP server", "Server SMTP";
    "nt.f.port" => "Port", "Porta";
    "nt.f.security" => "Encryption", "Crittografia";
    "nt.f.from" => "Sender address", "Indirizzo del mittente";
    "nt.f.to" => "Recipients", "Destinatari";
    "nt.f.to_help" => "One address per line, or separated by commas.", "Un indirizzo per riga, oppure separati da virgole.";
    "nt.f.server" => "Server", "Server";
    "nt.f.topic" => "Topic", "Argomento";
    "nt.f.topic_help" => "Anyone who knows a topic on a public server can read it: pick a long, random name.", "Chiunque conosca un argomento su un server pubblico può leggerlo: scegli un nome lungo e casuale.";
    "nt.f.ntfy_user_help" => "Only with password authentication; leave empty to use an access token.", "Solo con autenticazione a password; lascia vuoto per usare un token di accesso.";
    "nt.f.chat_id" => "Chat ID", "ID della chat";
    "nt.f.chat_id_help" => "The numeric id of the user, group or channel the bot writes to.", "L'id numerico dell'utente, del gruppo o del canale in cui scrive il bot.";
    "nt.f.api_url" => "Bot API address", "Indirizzo della Bot API";
    "nt.f.url" => "URL", "URL";
    "nt.f.url_help" => "A template: {{ secrets.name }} inserts a secret.", "Un modello: {{ secrets.nome }} inserisce un segreto.";
    "nt.f.method" => "Method", "Metodo";
    "nt.f.headers" => "Headers", "Intestazioni";
    "nt.f.header_name" => "Header", "Intestazione";
    "nt.f.header_value" => "Value", "Valore";
    "nt.f.add_header" => "Add header", "Aggiungi intestazione";
    "nt.f.body" => "Body", "Corpo";
    "nt.f.body_help" => "Minijinja template with subject, message, severity, events, hostname and secrets. Empty: the default JSON document.", "Modello minijinja con subject, message, severity, events, hostname e secrets. Vuoto: il documento JSON predefinito.";
    "nt.f.secrets" => "Secrets", "Segreti";
    "nt.f.secrets_help" => "Values used as {{ secrets.name }} in the URL, headers and body. They are never shown again.", "Valori usati come {{ secrets.nome }} in URL, intestazioni e corpo. Non vengono più mostrati.";
    "nt.f.add_secret" => "Add secret", "Aggiungi segreto";
    "nt.f.when" => "Which events", "Quali eventi";
    "nt.f.min_severity" => "At least", "Almeno";
    "nt.f.mode" => "Combine conditions", "Combina le condizioni";
    "nt.f.mode_help" => "Only the conditions that are set count.", "Contano solo le condizioni impostate.";
    "nt.f.categories_help" => "None selected: any category.", "Nessuna selezionata: qualsiasi categoria.";
    "nt.f.source_help" => "Regular expression on the event source (storage, scheduler, plugin:…). Empty: any.", "Espressione regolare sull'origine dell'evento (storage, scheduler, plugin:…). Vuoto: qualsiasi.";
    "nt.f.field" => "Field", "Campo";
    "nt.f.value" => "Value", "Valore";
    "nt.f.regex" => "Regex", "Regex";
    "nt.f.add_field" => "Add field condition", "Aggiungi condizione su campo";
    "nt.f.invert" => "Invert", "Inverti";
    "nt.f.invert_help" => "Send the events that do NOT match the conditions.", "Invia gli eventi che NON corrispondono alle condizioni.";
    "nt.f.deliver" => "Delivery", "Invio";
    "nt.f.group" => "Group events for", "Raggruppa gli eventi per";
    "nt.f.group_help" => "Events within this window go out as one message.", "Gli eventi in questo intervallo partono in un solo messaggio.";
    "nt.f.repeat" => "Remind every", "Ricorda ogni";
    "nt.f.repeat_help" => "While a problem stays active and unacknowledged.", "Finché un problema resta attivo e non visto.";
    "nt.f.quiet_help" => "Non-critical events are held during these windows and sent when they end. Critical events never wait.", "Gli eventi non critici vengono trattenuti in queste fasce e inviati alla fine. Gli eventi critici non aspettano mai.";
    "nt.f.days" => "Days", "Giorni";
    "nt.f.from_time" => "From", "Dalle";
    "nt.f.to_time" => "To", "Alle";
    "nt.f.add_quiet" => "Add quiet window", "Aggiungi fascia di silenzio";
    "nt.v.name" => "Enter a name.", "Inserisci un nome.";
    "nt.v.host" => "Enter the SMTP server.", "Inserisci il server SMTP.";
    "nt.v.port" => "The port must be a number from 1 to 65535.", "La porta deve essere un numero da 1 a 65535.";
    "nt.v.addresses" => "Enter the sender and at least one recipient.", "Inserisci il mittente e almeno un destinatario.";
    "nt.v.ntfy" => "Enter the server and the topic.", "Inserisci il server e l'argomento.";
    "nt.v.chat_id" => "Enter the chat ID.", "Inserisci l'ID della chat.";
    "nt.v.server" => "Enter the server address.", "Inserisci l'indirizzo del server.";
    "nt.v.url" => "Enter the URL.", "Inserisci l'URL.";
    "nt.v.targets" => "Pick at least one target.", "Scegli almeno una destinazione.";
    "sch.title" => "Scheduled tasks", "Attività pianificate";
    "sch.subtitle" => "Scheduled commands, scripts and maintenance, run by systemd timers", "Comandi, script e manutenzione pianificati, eseguiti dai timer di systemd";
    "sch.open_text" => "Scheduled commands and scripts, scrubs, SMART tests, recycle bin cleanup, with run history.", "Comandi e script pianificati, scrub, test SMART, pulizia del cestino, con cronologia delle esecuzioni.";
    "sch.create" => "Create task", "Crea attività";
    "sch.search" => "Search tasks", "Cerca attività";
    "sch.tab.tasks" => "Tasks", "Attività";
    "sch.tab.timers" => "System timers", "Timer di sistema";
    "sch.tab.cron" => "Cron entries", "Voci cron";
    "sch.empty" => "No scheduled tasks", "Nessuna attività pianificata";
    "sch.empty_text" => "Run a command or a script on a schedule, at boot or on demand, and get notified when it fails.", "Esegui un comando o uno script secondo una pianificazione, all'avvio o su richiesta, con notifica in caso di errore.";
    "sch.col.enabled" => "Enabled", "Attiva";
    "sch.col.task" => "Task", "Attività";
    "sch.col.action" => "Action", "Azione";
    "sch.col.schedule" => "Schedule", "Pianificazione";
    "sch.col.last" => "Last run", "Ultima esecuzione";
    "sch.col.started" => "Started", "Avvio";
    "sch.col.duration" => "Duration", "Durata";
    "sch.col.result" => "Result", "Risultato";
    "sch.col.timer" => "Timer", "Timer";
    "sch.col.next" => "Next run", "Prossima esecuzione";
    "sch.col.source" => "File", "File";
    "sch.next" => "Next: {when}", "Prossima: {when}";
    "sch.running" => "Running", "In esecuzione";
    "sch.started" => "{name}: running…", "{name}: in esecuzione…";
    "sch.finished_ok" => "{name}: completed", "{name}: completata";
    "sch.finished_err" => "{name}: failed ({why})", "{name}: non riuscita ({why})";
    "snap.run.started" => "Taking a snapshot of {name}…", "Snapshot di {name} in corso…";
    "snap.run.ok" => "Snapshot of {name} created", "Snapshot di {name} creato";
    "snap.run.failed" => "Snapshot of {name} failed: {why}", "Snapshot di {name} non riuscito: {why}";
    "sch.not_applied" => "Apply the pending changes first: the task does not exist on the system yet.", "Applica prima le modifiche in sospeso: l'attività non esiste ancora sul sistema.";
    "sch.history" => "Run history", "Cronologia esecuzioni";
    "sch.history_title" => "History of {name}", "Cronologia di {name}";
    "sch.no_runs" => "No runs yet", "Nessuna esecuzione";
    "sch.no_runs_text" => "Runs appear here with their result and output.", "Le esecuzioni compaiono qui con risultato e output.";
    "sch.show_output" => "Show output", "Mostra output";
    "sch.output" => "Output", "Output";
    "sch.no_output" => "(no output)", "(nessun output)";
    "sch.truncated" => "Only the last {size} of the output are kept.", "Dell'output vengono conservati solo gli ultimi {size}.";
    "sch.run.ok" => "Succeeded", "Riuscita";
    "sch.run.failed" => "Failed", "Non riuscita";
    "sch.run.timeout" => "Timed out", "Tempo scaduto";
    "sch.run.exit" => "Failed (exit {code})", "Non riuscita (uscita {code})";
    "sch.run.signal" => "Killed ({sig})", "Interrotta ({sig})";
    "sch.delete_title" => "Delete task {name}?", "Eliminare l'attività {name}?";
    "sch.delete_text" => "Its timer and units are removed when the changes are applied. The run history stays.", "Il timer e le unità vengono rimossi all'applicazione delle modifiche. La cronologia resta.";
    "sch.delete_job_task" => "This task belongs to a snapshot job or a power schedule: change or delete it there.", "Questa attività appartiene a un job di snapshot o a una pianificazione di alimentazione: modificala o eliminala lì.";
    "sch.sec.general" => "General", "Generale";
    "sch.sec.action" => "Action", "Azione";
    "sch.sec.schedule" => "Schedule", "Pianificazione";
    "sch.sec.notify" => "Notifications", "Notifiche";
    "sch.f.name" => "Name", "Nome";
    "sch.f.id" => "ID", "ID";
    "sch.f.id_help" => "Lowercase letters, digits and dashes; the units are rynas-task-<id>. It cannot change later.", "Lettere minuscole, cifre e trattini; le unità sono rynas-task-<id>. Non si può cambiare in seguito.";
    "sch.f.description" => "Description", "Descrizione";
    "sch.f.enabled" => "Enabled", "Attiva";
    "sch.f.enabled_help" => "A disabled task keeps its settings and can still be run by hand.", "Un'attività disattivata conserva le impostazioni e si può ancora eseguire a mano.";
    "sch.f.command" => "Command", "Comando";
    "sch.f.command_help" => "One line, run by /bin/sh.", "Una riga, eseguita da /bin/sh.";
    "sch.f.script" => "Script", "Script";
    "sch.f.script_help" => "The #! line picks the interpreter (default /bin/sh). rynas stores it so that only root can change it.", "La riga #! sceglie l'interprete (predefinito /bin/sh). rynas lo conserva in modo che solo root possa modificarlo.";
    "sch.f.user" => "Run as", "Esegui come";
    "sch.f.user_help" => "root (administrators only) or a local user.", "root (solo amministratori) o un utente locale.";
    "sch.f.filesystem" => "Volume", "Volume";
    "sch.f.filesystem_help" => "Mount point of the btrfs volume to scrub.", "Punto di mount del volume btrfs da verificare.";
    "sch.f.disk" => "Disk", "Disco";
    "sch.f.test" => "Test", "Test";
    "sch.f.share_help" => "Empty: every shared folder with a recycle bin.", "Vuoto: tutte le cartelle condivise con cestino.";
    "sch.f.days" => "Keep for (days)", "Conserva per (giorni)";
    "sch.f.days_help" => "Empty: the folder's own retention.", "Vuoto: la conservazione della cartella.";
    "sch.f.trigger" => "Run", "Esecuzione";
    "sch.f.repeat" => "Repeat", "Ripeti";
    "sch.f.time" => "Time", "Ora";
    "sch.f.every_min" => "Every (minutes)", "Ogni (minuti)";
    "sch.f.minute" => "Minute", "Minuto";
    "sch.f.minute_help" => "Minute past every hour.", "Minuto di ogni ora.";
    "sch.f.weekdays" => "Days", "Giorni";
    "sch.f.dom" => "Day of the month", "Giorno del mese";
    "sch.f.dom_help" => "Months without this day are skipped.", "I mesi senza questo giorno vengono saltati.";
    "sch.f.expression" => "systemd calendar expression", "Espressione di calendario systemd";
    "sch.f.expression_help" => "OnCalendar= syntax, e.g. Mon..Fri *-*-* 02:30:00 or *-*-1,15 04:00:00.", "Sintassi OnCalendar=, es. Mon..Fri *-*-* 02:30:00 o *-*-1,15 04:00:00.";
    "sch.f.cron" => "Cron expression", "Espressione cron";
    "sch.f.cron_help" => "Five fields (minute hour day month weekday) or @daily, @weekly…; converted to a systemd calendar.", "Cinque campi (minuto ora giorno mese giorno-settimana) o @daily, @weekly…; convertita in un calendario systemd.";
    "sch.f.persistent" => "Catch up missed runs", "Recupera le esecuzioni perse";
    "sch.f.persistent_help" => "A run missed while the NAS was off happens at the next boot.", "Un'esecuzione persa mentre il NAS era spento avviene al successivo avvio.";
    "sch.f.random" => "Random delay (seconds)", "Ritardo casuale (secondi)";
    "sch.f.random_help" => "Spreads tasks that start at the same time.", "Distribuisce le attività che partono alla stessa ora.";
    "sch.f.boot_delay" => "Delay after boot (seconds)", "Ritardo dopo l'avvio (secondi)";
    "sch.f.boot_delay_help" => "Counted from the moment systemd started.", "Contato dal momento in cui systemd è partito.";
    "sch.f.notify" => "Send a notification", "Invia una notifica";
    "sch.f.notify_output" => "Attach the output", "Allega l'output";
    "sch.f.notify_output_help" => "The last lines of the output go with the notification.", "Le ultime righe dell'output accompagnano la notifica.";
    "sch.f.timeout" => "Time limit (s)", "Limite di tempo (s)";
    "sch.f.timeout_help" => "The run is stopped after this long. Empty: no limit.", "L'esecuzione viene fermata dopo questo tempo. Vuoto: nessun limite.";
    "sch.f.nice" => "CPU priority (nice)", "Priorità CPU (nice)";
    "sch.f.nice_help" => "-20 to 19; empty: 10 for commands and scripts.", "Da -20 a 19; vuoto: 10 per comandi e script.";
    "sch.f.io_weight" => "Disk priority", "Priorità disco";
    "sch.f.io_weight_help" => "1 to 10000; empty: 100.", "Da 1 a 10000; vuoto: 100.";
    "sch.notify.on_failure" => "When it fails", "In caso di errore";
    "sch.notify.always" => "After every run", "Dopo ogni esecuzione";
    "sch.notify.never" => "Never", "Mai";
    "sch.trig.calendar" => "On a schedule", "Pianificata";
    "sch.trig.boot" => "At boot", "All'avvio";
    "sch.trig.manual" => "Manually", "Manuale";
    "sch.trig.manual_help" => "No timer: the task runs only with Run now.", "Nessun timer: l'attività parte solo con Esegui ora.";
    "sch.trig.boot_text" => "At boot (+{s} s)", "All'avvio (+{s} s)";
    "sch.rep.minutes" => "Every few minutes", "Ogni tot minuti";
    "sch.rep.hourly" => "Every hour", "Ogni ora";
    "sch.rep.daily" => "Every day", "Ogni giorno";
    "sch.rep.weekly" => "Every week", "Ogni settimana";
    "sch.rep.monthly" => "Every month", "Ogni mese";
    "sch.rep.custom" => "Custom (systemd)", "Personalizzata (systemd)";
    "sch.rep.cron" => "Cron syntax", "Sintassi cron";
    "sch.desc.minutes" => "Every {n} min", "Ogni {n} min";
    "sch.desc.hourly" => "Hourly at :{m}", "Ogni ora ai :{m}";
    "sch.desc.daily" => "Daily at {time}", "Ogni giorno alle {time}";
    "sch.desc.weekly" => "{days} at {time}", "{days} alle {time}";
    "sch.desc.monthly" => "Monthly on day {d} at {time}", "Ogni mese il giorno {d} alle {time}";
    "sch.next_runs" => "Next runs", "Prossime esecuzioni";
    "sch.normalized" => "systemd reads it as", "systemd la interpreta come";
    "sch.checking" => "Checking…", "Verifica…";
    "sch.act.command" => "Command", "Comando";
    "sch.act.script" => "Script", "Script";
    "sch.act.snapshot" => "Snapshot job", "Job di snapshot";
    "sch.act.scrub" => "Volume scrub", "Scrub del volume";
    "sch.act.smart_test" => "SMART test", "Test SMART";
    "sch.act.recycle_cleanup" => "Recycle bin cleanup", "Pulizia del cestino";
    "sch.act.power_shutdown" => "Scheduled shutdown", "Spegnimento pianificato";
    "sch.act.backup" => "Backup job", "Job di backup";
    "sch.act.dedup" => "Deduplication", "Deduplicazione";
    "sch.managed_storage" => "Managed in Storage: change it there.", "Gestita in Archiviazione: modificala lì.";
    // Deduplication (D-DEDUP)
    "dd.title" => "Deduplication", "Deduplicazione";
    "dd.short" => "Deduplicated", "Deduplicata";
    "dd.text" => "Finds identical data in the enabled shared folders and keeps one copy on disk. Runs only in its time window.", "Trova dati identici nelle cartelle condivise abilitate e ne tiene una sola copia su disco. Lavora solo nella sua fascia oraria.";
    "dd.fs_name" => "Filesystem {id}", "Filesystem {id}";
    "dd.st.off" => "Off", "Disattivata";
    "dd.st.on" => "On", "Attiva";
    "dd.st.paused" => "Paused", "In pausa";
    "dd.st.running" => "Running", "In corso";
    "dd.st.estimating" => "Estimating", "Stima in corso";
    "dd.st.waiting" => "Waiting", "In attesa";
    "dd.engine_missing" => "The deduplication engine (oans) is not installed.", "Il motore di deduplicazione (oans) non è installato.";
    "dd.engine_unavailable" => "The deduplication engine (package rynas-oans) is not installed and no source has it: add the rynas package source, or put the package from the rynas release in /var/lib/rynas/packages.", "Il motore di deduplicazione (pacchetto rynas-oans) non è installato e nessuna sorgente lo offre: aggiungi la sorgente dei pacchetti rynas, o metti il pacchetto della release di rynas in /var/lib/rynas/packages.";
    "dd.engine_unsupported" => "oans {version} is outside the versions rynas was tested with ({min} to {max}): runs are refused.", "oans {version} è fuori dalle versioni provate con rynas (da {min} a {max}): le esecuzioni sono rifiutate.";
    "dd.install" => "Install", "Installa";
    "dd.waiting_for" => "Waiting for {task} to finish", "In attesa che finisca {task}";
    "dd.starting" => "Starting…", "Avvio…";
    "dd.until" => "until {when}", "fino a {when}";
    "dd.ends" => "Stops", "Si ferma";
    "dd.stopping" => "Stopping: the work done is being saved (up to 5 minutes).", "Arresto: il lavoro fatto viene salvato (fino a 5 minuti).";
    "dd.ph.scanning" => "Looking for files", "Ricerca dei file";
    "dd.ph.hashing" => "Reading files", "Lettura dei file";
    "dd.ph.dedupe" => "Sharing duplicates", "Condivisione dei duplicati";
    "dd.eta" => "{time} left", "mancano {time}";
    "dd.freed" => "{size} freed", "{size} liberati";
    "dd.nothing_freed" => "nothing to free", "niente da liberare";
    "dd.checked" => "Checked", "Controllati";
    "dd.checked_line" => "{files} files, {size} · {dups}", "{files} file, {size} · {dups}";
    "dd.no_dups" => "no duplicates", "nessun duplicato";
    "dd.dups" => "{files} duplicated files", "{files} file duplicati";
    "dd.window" => "Time window", "Fascia oraria";
    "dd.window_help" => "Runs start at the beginning of each block and stop at its end; the next block goes on from there.", "Le esecuzioni partono all'inizio di ogni blocco e si fermano alla sua fine; il blocco successivo riprende da lì.";
    "dd.window_empty" => "Choose at least one hour.", "Scegli almeno un'ora.";
    "dd.grid_on" => "May run", "Può lavorare";
    "dd.grid_off" => "Not running", "Ferma";
    "dd.none" => "None yet", "Ancora nessuna";
    "dd.total" => "Freed in total", "Liberato in totale";
    "dd.left" => "Still to free", "Ancora da liberare";
    "dd.next" => "Next window", "Prossima fascia";
    "dd.estimate_result" => "Estimate", "Stima";
    "dd.estimate_line" => "{size} duplicated in {files} of {scanned} files scanned ({pct} of {total})", "{size} duplicati in {files} di {scanned} file analizzati ({pct} di {total})";
    "dd.logical_note" => "Sizes count data before compression; snapshots taken earlier keep their copy until they expire.", "Le dimensioni contano i dati prima della compressione; gli snapshot precedenti tengono la loro copia finché non scadono.";
    "dd.out.done" => "Done", "Completata";
    "dd.out.window_ended" => "Window ended", "Fascia finita";
    "dd.out.preempted" => "Gave way", "Ha ceduto il passo";
    "dd.out.stopped" => "Stopped", "Fermata";
    "dd.out.partial" => "Partial", "Parziale";
    "dd.out.failed" => "Failed", "Non riuscita";
    "dd.out.skipped" => "Skipped", "Saltata";
    "dd.block.encrypted" => "Encrypted folders cannot be deduplicated.", "Le cartelle crittografate non si possono deduplicare.";
    "dd.block.nocow" => "Folders without copy-on-write cannot be deduplicated.", "Le cartelle senza copy-on-write non si possono deduplicare.";
    "dd.block.filesystem" => "Only btrfs and XFS folders can be deduplicated.", "Si possono deduplicare solo cartelle btrfs e XFS.";
    "dd.block.xfs_no_reflink" => "This XFS filesystem was made without reflink.", "Questo filesystem XFS è stato creato senza reflink.";
    "dd.block.other_fs" => "The folder is on another filesystem than its volume.", "La cartella è su un filesystem diverso dal suo volume.";
    "dd.block.not_mounted" => "The folder or its filesystem is not available.", "La cartella o il suo filesystem non è disponibile.";
    "dd.warn.qgroups" => "btrfs quotas are on: with many shared extents, writes can stall for a while.", "Le quote btrfs sono attive: con molti extent condivisi le scritture possono bloccarsi per un po'.";
    "dd.warn.replicated" => "A replicated folder is included: the next replication can take longer or send more.", "È inclusa una cartella replicata: la prossima replica può durare di più o inviare di più.";
    "dd.warn.no_shares" => "No shared folder is enabled: turn it on in a folder's settings.", "Nessuna cartella condivisa abilitata: attivala nelle impostazioni di una cartella.";
    "dd.warn.blocked" => "Some enabled folders cannot be deduplicated and are left out.", "Alcune cartelle abilitate non si possono deduplicare e restano fuori.";
    "dd.warn.flash" => "The system disk is often flash memory: the index is written there.", "Il disco di sistema è spesso memoria flash: l'indice viene scritto lì.";
    "dd.estimate" => "Estimate", "Stima";
    "dd.estimate_title" => "Estimate on {name}", "Stima su {name}";
    "dd.estimate_text" => "Reads the folders and counts the duplicates without changing anything. Its work is reused by the first run.", "Legge le cartelle e conta i duplicati senza cambiare nulla. Il lavoro viene riusato dalla prima esecuzione.";
    "dd.run_title" => "Deduplicate {name} now", "Deduplica {name} ora";
    "dd.run_text" => "Runs now with low priority. Other work on the volume (scrub, replication, backup) goes first.", "Parte ora con bassa priorità. Altre attività sul volume (scrub, replica, backup) hanno la precedenza.";
    "dd.until_label" => "Stop", "Fermati";
    "dd.until_help" => "The work done is kept; the next window goes on.", "Il lavoro fatto resta; la prossima fascia continua.";
    "dd.until_window" => "At the end of the window", "Alla fine della fascia";
    "dd.h1" => "After 1 hour", "Dopo 1 ora";
    "dd.h2" => "After 2 hours", "Dopo 2 ore";
    "dd.h4" => "After 4 hours", "Dopo 4 ore";
    "dd.h8" => "After 8 hours", "Dopo 8 ore";
    "dd.h24" => "After 24 hours", "Dopo 24 ore";
    "dd.snapshots_note" => "Space used by older snapshots comes back only when they expire.", "Lo spazio degli snapshot più vecchi torna solo quando scadono.";
    "dd.start" => "Start", "Avvia";
    "dd.stop" => "Stop", "Ferma";
    "dd.history" => "History", "Cronologia";
    "dd.history_title" => "Deduplication history of {name}", "Cronologia della deduplicazione di {name}";
    "dd.by" => "for {task}", "per {task}";
    "dd.skipped_files" => "{n} files could not be read", "{n} file non leggibili";
    "dd.settings" => "Settings", "Impostazioni";
    "dd.settings_title" => "Deduplication of {name}", "Deduplicazione di {name}";
    "dd.enabled" => "Scheduled runs", "Esecuzioni pianificate";
    "dd.enabled_help" => "Off: runs only when started by hand.", "Disattivato: lavora solo se avviata a mano.";
    "dd.only_awake" => "Only if the disks are already awake", "Solo se i dischi sono già attivi";
    "dd.only_awake_help" => "Skips a window instead of waking disks in standby.", "Salta una fascia invece di svegliare i dischi in standby.";
    "dd.advanced" => "Advanced", "Avanzate";
    "dd.min_size" => "Smallest file", "File più piccolo";
    "dd.min_size_help" => "Smaller files are skipped: they free little and change often.", "I file più piccoli vengono saltati: liberano poco e cambiano spesso.";
    "dd.block" => "Block size", "Dimensione del blocco";
    "dd.block_help" => "Most volumes keep the default.", "La maggior parte dei volumi tiene il valore predefinito.";
    "dd.block_changed" => "A new block size makes the next run read everything again.", "Una nuova dimensione del blocco fa rileggere tutto alla prossima esecuzione.";
    "dd.partial" => "Partial matches", "Corrispondenze parziali";
    "dd.partial_help" => "Also shares parts of files. Much more CPU and a bigger index.", "Condivide anche parti di file. Molta più CPU e un indice più grande.";
    "dd.zeroes" => "Skip empty blocks", "Salta i blocchi vuoti";
    "dd.zeroes_help" => "Faster on disk images and preallocated files.", "Più veloce su immagini disco e file preallocati.";
    "dd.cpu" => "CPU threads", "Thread CPU";
    "dd.io" => "Disk threads", "Thread disco";
    "dd.io_help" => "Automatic adapts to the disks.", "Automatico si adatta ai dischi.";
    "dd.memory" => "Memory limit", "Limite di memoria";
    "dd.auto" => "Automatic", "Automatico";
    "dd.n1" => "1", "1";
    "dd.n2" => "2", "2";
    "dd.n4" => "4", "4";
    "dd.n8" => "8", "8";
    "dd.mem.512" => "512 MiB", "512 MiB";
    "dd.mem.1024" => "1 GiB", "1 GiB";
    "dd.mem.2048" => "2 GiB", "2 GiB";
    "dd.mem.4096" => "4 GiB", "4 GiB";
    "dd.sz.4k" => "4 KiB", "4 KiB";
    "dd.sz.16k" => "16 KiB", "16 KiB";
    "dd.sz.32k" => "32 KiB", "32 KiB";
    "dd.sz.64k" => "64 KiB", "64 KiB";
    "dd.sz.128k" => "128 KiB", "128 KiB";
    "dd.sz.256k" => "256 KiB", "256 KiB";
    "dd.sz.512k" => "512 KiB", "512 KiB";
    "dd.sz.1m" => "1 MiB", "1 MiB";
    "dd.sz.4m" => "4 MiB", "4 MiB";
    "dd.always_excluded" => "Never deduplicated", "Mai deduplicati";
    "dd.reset" => "Reset index", "Azzera l'indice";
    "dd.reset_text" => "The index of known data is deleted: the next run reads everything again. The data is not touched.", "L'indice dei dati noti viene eliminato: la prossima esecuzione rilegge tutto. I dati non vengono toccati.";
    "dd.reset_done" => "Index reset", "Indice azzerato";
    "dd.turn_off_text" => "Deduplication of {name} stops. Space already freed stays free.", "La deduplicazione di {name} si ferma. Lo spazio già liberato resta libero.";
    "dd.share_switch" => "Deduplicate this folder", "Deduplica questa cartella";
    "dd.share_switch_help" => "Identical data here and in the other enabled folders of the volume is kept once.", "I dati identici qui e nelle altre cartelle abilitate del volume vengono tenuti una volta sola.";
    "dd.share_job_off" => "Deduplication of this volume is off.", "La deduplicazione di questo volume è disattivata.";
    "dd.include_recycle" => "Include the recycle bin", "Includi il cestino";
    "dd.include_recycle_help" => "Off: files waiting in #recycle are skipped.", "Disattivato: i file in attesa in #recycle vengono saltati.";
    "dd.excludes" => "Skip", "Salta";
    "dd.excludes_help" => "One pattern per line, relative to the folder (cache/, *.iso).", "Un modello per riga, relativo alla cartella (cache/, *.iso).";
    "task.storage.dedup" => "Deduplication", "Deduplicazione";
    "task.storage.dedup-estimate" => "Deduplication estimate", "Stima della deduplicazione";
    "task.storage.dedup-reset" => "Reset the deduplication index", "Azzeramento dell'indice di deduplicazione";
    "task.dedup.install" => "Install the deduplication engine", "Installazione del motore di deduplicazione";
    "nev.dedup.ok.title" => "Deduplication of {volume} done: {reclaimed} freed", "Deduplicazione di {volume} completata: {reclaimed} liberati";
    "nev.dedup.partial.title" => "Deduplication of {volume} covered only part of its folders", "La deduplicazione di {volume} ha coperto solo parte delle cartelle";
    "nev.dedup.failed.title" => "Deduplication of {volume} failed", "Deduplicazione di {volume} non riuscita";
    "shares.size_shared" => "Data shared with other folders or snapshots is counted in each.", "I dati condivisi con altre cartelle o snapshot sono contati in ciascuna.";
    "sch.smart.short" => "Short", "Breve";
    "sch.smart.long" => "Extended", "Esteso";
    "sch.all_shares" => "All shared folders", "Tutte le cartelle condivise";
    "sch.target" => "Target", "Destinazione";
    "sch.internal_help" => "A maintenance task keeps its action: only the schedule and the options can change.", "Un'attività di manutenzione mantiene la sua azione: si possono cambiare solo pianificazione e opzioni.";
    "sch.timers_help" => "systemd timers that rynas did not create, read only.", "Timer di systemd non creati da rynas, in sola lettura.";
    "sch.cron_help" => "Crontab lines found on the system, read only. A line with an equivalent schedule can become a rynas task (remove the cron line afterwards).", "Righe crontab trovate sul sistema, in sola lettura. Una riga con una pianificazione equivalente può diventare un'attività rynas (poi rimuovi la riga cron).";
    "sch.no_timers" => "No other timers", "Nessun altro timer";
    "sch.no_cron" => "No cron entries", "Nessuna voce cron";
    "sch.convert" => "Create a task from this line", "Crea un'attività da questa riga";
    "sch.v.id" => "Use lowercase letters, digits and dashes (at most 48), starting with a letter or digit.", "Usa lettere minuscole, cifre e trattini (al massimo 48), iniziando con una lettera o una cifra.";
    "sch.v.action" => "Pick an action.", "Scegli un'azione.";
    "sch.v.command" => "Enter the command.", "Inserisci il comando.";
    "sch.v.script" => "Enter the script.", "Inserisci lo script.";
    "sch.v.filesystem" => "Enter the volume's mount point (an absolute path).", "Inserisci il punto di mount del volume (un percorso assoluto).";
    "sch.v.disk" => "Enter the disk (sda, nvme0n1).", "Inserisci il disco (sda, nvme0n1).";
    "sch.v.days" => "The days must be a number.", "I giorni devono essere un numero.";
    "sch.v.delay" => "The delay must be a number of seconds.", "Il ritardo deve essere un numero di secondi.";
    "sch.v.calendar" => "The schedule is not valid.", "La pianificazione non è valida.";
    "snap.title" => "Snapshots", "Snapshot";
    "snap.subtitle" => "Point-in-time copies of shared folders, file restore and rollback", "Copie puntuali delle cartelle condivise, ripristino di file e rollback";
    "snap.tab.snapshots" => "Snapshots", "Snapshot";
    "snap.tab.jobs" => "Snapshot jobs", "Job di snapshot";
    "snap.tab.system" => "System restore points", "Punti di ripristino del sistema";
    "snap.create_job" => "Create snapshot job", "Crea job di snapshot";
    "snap.all_jobs" => "All jobs", "Tutti i job";
    "snap.refresh" => "Refresh", "Aggiorna";
    "snap.none" => "No snapshots", "Nessuno snapshot";
    "snap.none_text" => "Snapshots appear here once a job has run. Jobs are applied with the pending changes.", "Gli snapshot compaiono qui dopo la prima esecuzione di un job. I job vengono applicati con le modifiche in sospeso.";
    "snap.col.snapshot" => "Snapshot", "Snapshot";
    "snap.col.job" => "Job", "Job";
    "snap.col.created" => "Taken", "Creato";
    "snap.col.retention" => "Retention", "Conservazione";
    "snap.col.name" => "Name", "Nome";
    "snap.col.size" => "Size", "Dimensione";
    "snap.col.modified" => "Modified", "Modificato";
    "snap.col.mode" => "Permissions", "Permessi";
    "snap.system_point" => "System restore point", "Punto di ripristino del sistema";
    "snap.share_of" => "Shared folder {share}", "Cartella condivisa {share}";
    "snap.readonly" => "Read-only", "Sola lettura";
    "snap.writable" => "Writable", "Scrivibile";
    "snap.latest" => "Latest", "Più recente";
    "snap.latest_at" => "Latest {when}", "Ultimo {when}";
    "snap.reason" => "Reason: {r}", "Motivo: {r}";
    "snap.why.apt" => "Before package changes", "Prima di modifiche ai pacchetti";
    "snap.why.manual" => "Taken by hand", "Creato a mano";
    "snap.why.before_rollback" => "System before a rollback", "Sistema prima di un rollback";
    "snap.browse" => "Browse", "Sfoglia";
    "snap.browse_title" => "Browse {id}", "Sfoglia {id}";
    "snap.path" => "Folder", "Cartella";
    "snap.up" => "Parent folder", "Cartella superiore";
    "snap.empty_dir" => "This folder is empty.", "Questa cartella è vuota.";
    "snap.truncated" => "More than 10,000 entries: only the first ones by name are listed.", "Più di 10.000 elementi: vengono elencati solo i primi per nome.";
    "snap.restore" => "Restore", "Ripristina";
    "snap.restore_this" => "Restore this folder", "Ripristina questa cartella";
    "snap.restore_file" => "Restore {path}?", "Ripristinare {path}?";
    "snap.restore_folder" => "Restore folder {path}?", "Ripristinare la cartella {path}?";
    "snap.restore_text" => "The version in snapshot {id} goes back to the shared folder. Nothing is overwritten.", "La versione dello snapshot {id} torna nella cartella condivisa. Nulla viene sovrascritto.";
    "snap.restore_mode" => "Where", "Dove";
    "snap.mode.copy" => "Next to the current one", "Accanto all'attuale";
    "snap.mode.replace" => "In place", "Al suo posto";
    "snap.mode.copy_help" => "Restored as \"name (restored date)\" next to the current version.", "Ripristinato come \"nome (restored data)\" accanto alla versione attuale.";
    "snap.mode.replace_help" => "The current version is first renamed to name.rynas-before-restore-<time>, then the snapshot's version takes its place.", "La versione attuale viene prima rinominata in nome.rynas-before-restore-<ora>, poi la versione dello snapshot prende il suo posto.";
    "snap.restored" => "{path} restored", "{path} ripristinato";
    "snap.rollback" => "Roll back", "Rollback";
    "snap.rollback_system" => "Roll the system back (reboot needed)", "Ripristina il sistema (serve un riavvio)";
    "snap.rollback_title" => "Roll back to {id}?", "Rollback a {id}?";
    "snap.rollback_text" => "The whole shared folder goes back to this snapshot. Its current content is kept, renamed <folder>.rynas-rollback-<time>, until you delete it.", "L'intera cartella condivisa torna a questo snapshot. Il contenuto attuale viene conservato, rinominato <cartella>.rynas-rollback-<ora>, finché non lo elimini.";
    "snap.rollback_nfs" => "NFS clients must mount the folder again afterwards.", "I client NFS devono rimontare la cartella dopo l'operazione.";
    "snap.rolled_back" => "Rolled back to {id}", "Rollback a {id} eseguito";
    "snap.root_title" => "Roll the system back to {id}?", "Ripristinare il sistema a {id}?";
    "snap.root_text" => "At the next reboot the system starts from this restore point: packages, kernel and settings go back to that moment. Shared folders and data volumes are not touched.", "Al prossimo riavvio il sistema parte da questo punto di ripristino: pacchetti, kernel e impostazioni tornano a quel momento. Cartelle condivise e volumi dati non vengono toccati.";
    "snap.root_kernel" => "Kernel after the reboot", "Kernel dopo il riavvio";
    "snap.root_running" => "Kernel running now", "Kernel in esecuzione ora";
    "snap.root_steps" => "What happens", "Cosa succede";
    "snap.root_step1" => "The restore point is mounted read-only and its kernel is checked.", "Il punto di ripristino viene montato in sola lettura e il suo kernel viene controllato.";
    "snap.root_step2" => "The boot entry is pointed at that kernel. If that is refused, nothing changes.", "La voce di avvio viene puntata su quel kernel. Se l'operazione viene rifiutata, non cambia nulla.";
    "snap.root_step3" => "The current system is kept as a new restore point (reason: before rollback), then the restore point becomes the system subvolume (@rootfs).", "Il sistema attuale viene conservato come nuovo punto di ripristino (motivo: before rollback), poi il punto di ripristino diventa il subvolume di sistema (@rootfs).";
    "snap.root_step4" => "You reboot the NAS to start the restored system.", "Riavvii il NAS per avviare il sistema ripristinato.";
    "snap.root_reboot" => "A reboot is needed to finish. Until then the current system keeps running.", "Per finire serve un riavvio. Fino ad allora il sistema attuale continua a funzionare.";
    "snap.root_done" => "At the next reboot the system starts from {id} (kernel {kernel}). Reboot now to finish the rollback.", "Al prossimo riavvio il sistema parte da {id} (kernel {kernel}). Riavvia ora per completare il ripristino.";
    "snap.root_refused_title" => "{id} cannot be restored from here", "{id} non si può ripristinare da qui";
    "snap.refusal.no_boot_layer" => "This system cannot be rolled back from the web interface: the restore point is there, going back to it is a console procedure.", "Questo sistema non si può ripristinare dall'interfaccia web: il punto di ripristino c'è, tornarci è una procedura da console.";
    "snap.refusal.no_kernel" => "The restore point has no kernel image.", "Il punto di ripristino non contiene un'immagine del kernel.";
    "snap.refusal.no_modules" => "The restore point lacks the modules of its kernel (/lib/modules): it could not boot.", "Al punto di ripristino mancano i moduli del suo kernel (/lib/modules): non potrebbe avviarsi.";
    "snap.refusal.not_readonly" => "Not a read-only snapshot.", "Non è uno snapshot in sola lettura.";
    "snap.delete_title" => "Delete snapshot {id}?", "Eliminare lo snapshot {id}?";
    "snap.delete_text" => "Its files can no longer be restored. This cannot be undone.", "I suoi file non si potranno più ripristinare. L'operazione non si può annullare.";
    "snap.deleted" => "Snapshot {id} deleted", "Snapshot {id} eliminato";
    "snap.del.latest" => "The newest snapshot of a job is kept: it is the base of the next one", "Lo snapshot più recente di un job viene conservato: è la base del successivo";
    "snap.del.writable" => "Not a read-only snapshot: left alone", "Non è uno snapshot in sola lettura: non viene toccato";
    "snap.del.not_subvolume" => "Not a subvolume: left alone", "Non è un subvolume: non viene toccato";
    "snap.jobs_help" => "Each job snapshots one shared folder or subvolume on its schedule and keeps hourly, daily, weekly, monthly and yearly snapshots. A shared folder with SMB also gets Windows \"Previous versions\".", "Ogni job crea snapshot di una cartella condivisa o di un subvolume secondo la sua pianificazione e conserva snapshot orari, giornalieri, settimanali, mensili e annuali. Una cartella condivisa con SMB ottiene anche le \"Versioni precedenti\" di Windows.";
    "snap.no_jobs" => "No snapshot jobs", "Nessun job di snapshot";
    "snap.no_jobs_text" => "Protect a shared folder against deleted files and ransomware with hourly snapshots.", "Proteggi una cartella condivisa da file eliminati e ransomware con snapshot orari.";
    "snap.take_now" => "Take a snapshot now", "Crea uno snapshot ora";
    "snap.show_snapshots" => "Show the snapshots of this job", "Mostra gli snapshot di questo job";
    "snap.delete_job" => "Delete snapshot job {name}?", "Eliminare il job di snapshot {name}?";
    "snap.delete_job_text" => "Its schedule and the SMB previous versions go with it. Snapshots already on disk stay.", "Con il job se ne vanno la pianificazione e le versioni precedenti SMB. Gli snapshot già su disco restano.";
    "snap.ret.none" => "Latest only", "Solo il più recente";
    "snap.u.h" => "hourly", "orari";
    "snap.u.d" => "daily", "giornalieri";
    "snap.u.w" => "weekly", "settimanali";
    "snap.u.m" => "monthly", "mensili";
    "snap.u.y" => "yearly", "annuali";
    "snap.preset.standard" => "Standard", "Standard";
    "snap.preset.light" => "Light", "Leggera";
    "snap.preset.long" => "Long", "Lunga";
    "snap.preset.custom" => "Custom", "Personalizzata";
    "snap.preset_keeps" => "Keeps {counts}; everything younger than {min} stays.", "Conserva {counts}; tutto ciò che ha meno di {min} resta.";
    "snap.preview" => "Preview what would be deleted", "Anteprima di cosa verrebbe eliminato";
    "snap.preview_counts" => "Now this retention keeps {keep} snapshots and deletes {delete}.", "Ora questa conservazione mantiene {keep} snapshot e ne elimina {delete}.";
    "snap.preview_empty" => "There are no snapshots of this job yet.", "Non ci sono ancora snapshot di questo job.";
    "snap.sec.source" => "What to protect", "Cosa proteggere";
    "snap.f.name" => "Job name", "Nome del job";
    "snap.f.name_help" => "Also the snapshot name prefix (name.YYYYMMDDTHHMM). It cannot change later.", "È anche il prefisso dei nomi degli snapshot (nome.AAAAMMGGTHHMM). Non si può cambiare in seguito.";
    "snap.f.source" => "Source", "Origine";
    "snap.src.path" => "Subvolume path", "Percorso del subvolume";
    "snap.f.pick_share" => "Choose a shared folder", "Scegli una cartella condivisa";
    "snap.f.share_help" => "Its folder must be a btrfs subvolume.", "La sua cartella deve essere un subvolume btrfs.";
    "snap.f.subvolume" => "Subvolume", "Subvolume";
    "snap.f.subvolume_help" => "Absolute path of a btrfs subvolume.", "Percorso assoluto di un subvolume btrfs.";
    "snap.f.dir" => "Snapshot folder", "Cartella degli snapshot";
    "snap.f.dir_help" => "On the same volume. Empty: .snapshots/<job> next to the subvolume.", "Sullo stesso volume. Vuoto: .snapshots/<job> accanto al subvolume.";
    "snap.f.enabled" => "Take snapshots on schedule", "Crea snapshot secondo la pianificazione";
    "snap.f.enabled_help" => "Off: snapshots are taken only with Take a snapshot now.", "Disattivato: gli snapshot si creano solo con Crea uno snapshot ora.";
    "snap.f.preset" => "Keep", "Conserva";
    "snap.f.hourly" => "Hourly", "Orari";
    "snap.f.daily" => "Daily", "Giornalieri";
    "snap.f.weekly" => "Weekly", "Settimanali";
    "snap.f.monthly" => "Monthly", "Mensili";
    "snap.f.yearly" => "Yearly", "Annuali";
    "snap.f.min" => "Always keep", "Conserva sempre";
    "snap.f.min_help" => "latest, all, or an age such as 1d, 12h, 2w.", "latest, all, o un'età come 1d, 12h, 2w.";
    "snap.v.name" => "Use letters, digits, _ and - (at most 48), starting with a letter or digit.", "Usa lettere, cifre, _ e - (al massimo 48), iniziando con una lettera o una cifra.";
    "snap.v.source" => "Choose a shared folder or enter an absolute subvolume path.", "Scegli una cartella condivisa o inserisci il percorso assoluto di un subvolume.";
    "snap.system_help" => "Before apt installs or upgrades packages, rynas takes a read-only snapshot of the system. Where the boot setup allows it, the system can go back to one of them from the snapshot list (kernel included, reboot needed); elsewhere it is a console procedure.", "Prima che apt installi o aggiorni pacchetti, rynas crea uno snapshot in sola lettura del sistema. Dove l'avvio lo consente, il sistema può tornare a uno di essi dalla lista degli snapshot (kernel compreso, serve un riavvio); altrove è una procedura da console.";
    "snap.system_enabled" => "Take restore points before updates", "Crea punti di ripristino prima degli aggiornamenti";
    "snap.system_enabled_help" => "Requires a btrfs root filesystem.", "Richiede un filesystem di root btrfs.";
    "snap.system_keep" => "Restore points kept", "Punti di ripristino conservati";
    "snap.system_keep_help" => "The oldest beyond this number are deleted after a new one.", "I più vecchi oltre questo numero vengono eliminati dopo uno nuovo.";
    "snap.system_prefix" => "Name prefix", "Prefisso del nome";
    // Snapshot replication
    "snap.tab.replication" => "Replication", "Replica";
    "sch.act.replicate" => "Snapshot replication", "Replica degli snapshot";
    "task.replication.run" => "Replicate snapshots", "Replica degli snapshot";
    "task.replication.restore" => "Restore from a replica", "Ripristino da una replica";
    "task.replication.promote" => "Promote a replica", "Promozione di una replica";
    "nev.replication.failed.title" => "Replication {name} failed", "Replica {name} non riuscita";
    "nev.replication.ok.title" => "Replication {name} works again", "La replica {name} funziona di nuovo";
    "repl.intro" => "A second copy of a job's snapshots on another btrfs volume or another server, with its own retention. Snapshots on the same disks are not a backup.", "Una seconda copia degli snapshot di un job su un altro volume btrfs o su un altro server, con una conservazione propria. Gli snapshot sugli stessi dischi non sono un backup.";
    "repl.add" => "Add replica target", "Aggiungi destinazione di replica";
    "repl.none" => "No replica targets", "Nessuna destinazione di replica";
    "repl.none_text" => "Add a target to keep a copy of the snapshots on another volume or server.", "Aggiungi una destinazione per tenere una copia degli snapshot su un altro volume o server.";
    "repl.none_no_jobs" => "Create a snapshot job first: replication copies its snapshots.", "Crea prima un job di snapshot: la replica ne copia gli snapshot.";
    "repl.col.target" => "Target", "Destinazione";
    "repl.col.when" => "When", "Quando";
    "repl.col.last" => "Last transfer", "Ultimo trasferimento";
    "repl.col.replica" => "Replica", "Replica";
    "repl.after_snapshots" => "After each snapshot", "Dopo ogni snapshot";
    "repl.held_one" => "{n} snapshot kept on the source until the target is reachable again", "{n} snapshot tenuto sull'origine finché la destinazione non torna raggiungibile";
    "repl.held_many" => "{n} snapshots kept on the source until the target is reachable again", "{n} snapshot tenuti sull'origine finché la destinazione non torna raggiungibile";
    "repl.key.line_needs_path" => "Fill in the folder on the target above: the authorized_keys line limits the key to that folder.", "Compila sopra la cartella sulla destinazione: la riga di authorized_keys limita la chiave a quella cartella.";
    "repl.never" => "Never", "Mai";
    "repl.sent_one" => "{n} snapshot sent", "{n} snapshot inviato";
    "repl.sent_many" => "{n} snapshots sent", "{n} snapshot inviati";
    "repl.off" => "Off", "Disattivata";
    "repl.failing" => "Failing", "Non riuscita";
    "repl.ok" => "Replicated", "Replicata";
    "repl.waiting" => "Not run yet", "Non ancora eseguita";
    "repl.run" => "Replicate now", "Replica ora";
    "repl.run_done" => "Replication finished", "Replica completata";
    "repl.replicas" => "Replicas", "Repliche";
    "repl.replicas_remote" => "Replicas on another server are browsed on that server", "Le repliche su un altro server si consultano su quel server";
    "repl.replicas_title" => "Replicas of {job} on {id}", "Repliche di {job} su {id}";
    "repl.replicas_text" => "Read-only copies in {path}. Restore copies a file or folder back next to the original; Promote makes a writable copy as a new folder on that volume.", "Copie in sola lettura in {path}. Ripristina copia un file o una cartella accanto all'originale; Promuovi crea una copia scrivibile come nuova cartella su quel volume.";
    "repl.no_replicas" => "No replicas yet: they appear after the first transfer.", "Ancora nessuna replica: compaiono dopo il primo trasferimento.";
    "repl.incomplete" => "Interrupted transfer", "Trasferimento interrotto";
    "repl.back" => "Back to the replicas", "Torna alle repliche";
    "repl.restore_title" => "Restore {path} from the replica?", "Ripristinare {path} dalla replica?";
    "repl.restore_text" => "A copy goes next to the original in the shared folder, with the restore date in its name. Nothing is overwritten.", "Una copia viene messa accanto all'originale nella cartella condivisa, con la data del ripristino nel nome. Nulla viene sovrascritto.";
    "repl.promote" => "Promote", "Promuovi";
    "repl.promote_title" => "Promote {name}?", "Promuovere {name}?";
    "repl.promote_text" => "Creates a writable copy of this replica as a new folder at the top of the target volume, ready to be shared. The replica itself stays read-only, so later transfers continue.", "Crea una copia scrivibile di questa replica come nuova cartella nella radice del volume di destinazione, pronta da condividere. La replica resta in sola lettura, quindi i trasferimenti successivi continuano.";
    "repl.promoted" => "Folder {folder} created", "Cartella {folder} creata";
    "repl.delete_title" => "Remove the target {id} of {job}?", "Rimuovere la destinazione {id} di {job}?";
    "repl.delete_text" => "Replication to this target stops. The replicas already there are kept: delete them on the target if you no longer need them.", "La replica verso questa destinazione si ferma. Le repliche già presenti restano: eliminale sulla destinazione se non servono più.";
    "repl.sec.target" => "Target", "Destinazione";
    "repl.sec.key" => "Access to the server", "Accesso al server";
    "repl.sec.retention" => "Retention on the target", "Conservazione sulla destinazione";
    "repl.sec.transfer" => "Transfer", "Trasferimento";
    "repl.f.job_help" => "The snapshots of this job are copied.", "Vengono copiati gli snapshot di questo job.";
    "repl.f.id" => "Target name", "Nome della destinazione";
    "repl.f.id_help" => "Lowercase letters, digits and '-', for example hdd or offsite.", "Lettere minuscole, cifre e '-', per esempio hdd o offsite.";
    "repl.f.kind" => "Where", "Dove";
    "repl.kind.local" => "Another volume", "Un altro volume";
    "repl.kind.ssh" => "Another server (SSH)", "Un altro server (SSH)";
    "repl.f.host" => "Server", "Server";
    "repl.f.port" => "Port", "Porta";
    "repl.f.user" => "User", "Utente";
    "repl.f.user_help" => "On the server; root, or a user allowed to run btrfs with sudo.", "Sul server; root, o un utente che può eseguire btrfs con sudo.";
    "repl.f.path" => "Folder", "Cartella";
    "repl.f.path_local_help" => "On another btrfs volume of this NAS; created at the first transfer. Not on the volume of the source.", "Su un altro volume btrfs di questo NAS; viene creata al primo trasferimento. Non sul volume dell'origine.";
    "repl.f.path_remote_help" => "On a btrfs filesystem of the server. It must exist: create it there (for example btrfs subvolume create).", "Su un filesystem btrfs del server. Deve esistere: creala lì (per esempio btrfs subvolume create).";
    "repl.f.sudo" => "Run btrfs with sudo", "Esegui btrfs con sudo";
    "repl.f.sudo_help" => "For a user other than root.", "Per un utente diverso da root.";
    "repl.f.host_keys" => "Server identity", "Identità del server";
    "repl.f.host_keys_none" => "Not confirmed yet: transfers are refused until the server's key is confirmed.", "Non ancora confermata: i trasferimenti sono rifiutati finché la chiave del server non è confermata.";
    "repl.f.scan" => "Read the server's key", "Leggi la chiave del server";
    "repl.f.scan_check" => "Compare these fingerprints with the output of `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` on the server before trusting them.", "Confronta queste impronte con l'output di `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` sul server prima di fidarti.";
    "repl.f.scan_none" => "The server answered no key.", "Il server non ha restituito alcuna chiave.";
    "repl.f.trust" => "Trust these keys", "Fidati di queste chiavi";
    "repl.f.enabled" => "Replication on", "Replica attiva";
    "repl.f.after" => "Run after each snapshot", "Esegui dopo ogni snapshot";
    "repl.f.after_help" => "Off: the target has its own schedule (for example every night).", "Disattivato: la destinazione ha una pianificazione propria (per esempio ogni notte).";
    "repl.f.retention_help" => "Independent of the snapshots on the source. Only replicas made by rynas are ever deleted on the target.", "Indipendente dagli snapshot sull'origine. Sulla destinazione vengono eliminate solo le repliche create da rynas.";
    "repl.f.compress" => "Compression on the network", "Compressione in rete";
    "repl.compress.no" => "None", "Nessuna";
    "repl.compress.zstd" => "zstd", "zstd";
    "repl.compress.lz4" => "lz4", "lz4";
    "repl.compress.gzip" => "gzip", "gzip";
    "repl.f.rate" => "Bandwidth limit (KiB/s)", "Limite di banda (KiB/s)";
    "repl.f.rate_help" => "Empty or 0: no limit.", "Vuoto o 0: nessun limite.";
    "repl.f.folder_help" => "At the top of the target volume; it must not exist.", "Nella radice del volume di destinazione; non deve esistere.";
    "repl.key.none" => "rynas connects with its own key pair, used only for replication.", "rynas si collega con una propria coppia di chiavi, usata solo per la replica.";
    "repl.key.create" => "Create the key", "Crea la chiave";
    "repl.key.public" => "Public key of this NAS", "Chiave pubblica di questo NAS";
    "repl.key.line" => "Line for the server", "Riga per il server";
    "repl.key.line_help" => "Add it to `{file}` on the server: the key can then only receive and prune replicas in this folder.", "Aggiungila a `{file}` sul server: la chiave potrà solo ricevere e sfoltire repliche in questa cartella.";
    "repl.key.filter_version" => "The server needs btrbk 0.32.7 or later: older `ssh_filter_btrbk.sh` versions let a restricted key run any command.", "Il server deve avere btrbk 0.32.7 o successivo: le versioni precedenti di `ssh_filter_btrbk.sh` permettono a una chiave limitata di eseguire qualsiasi comando.";
    "repl.key.sudo_help" => "On the server, allow {user} to run btrfs without a password, for example `{user} ALL=(root) NOPASSWD: /usr/bin/btrfs` in a file of `/etc/sudoers.d`.", "Sul server, consenti a {user} di eseguire btrfs senza password, per esempio `{user} ALL=(root) NOPASSWD: /usr/bin/btrfs` in un file di `/etc/sudoers.d`.";
    "repl.copy" => "Copy", "Copia";
    "repl.copied" => "Copied", "Copiato";
    "repl.hint.ssh_key" => "The server refuses the key: add the line for the server to its `authorized_keys`.", "Il server rifiuta la chiave: aggiungi la riga per il server al suo `authorized_keys`.";
    "repl.hint.host_key" => "The server's identity is not confirmed or has changed: read and confirm its key again.", "L'identità del server non è confermata o è cambiata: leggi e conferma di nuovo la sua chiave.";
    "repl.hint.unreachable" => "The server cannot be reached: check the name, the port and that it is on.", "Il server non è raggiungibile: controlla nome, porta e che sia acceso.";
    "repl.hint.filter" => "The server's command filter refused a command: check the folder in the line and that its btrbk is 0.32.7 or later.", "Il filtro dei comandi del server ha rifiutato un comando: controlla la cartella nella riga e che il suo btrbk sia 0.32.7 o successivo.";
    "repl.hint.target_missing" => "The target folder does not exist: create it on the target (`btrfs subvolume create`).", "La cartella di destinazione non esiste: creala sulla destinazione (`btrfs subvolume create`).";
    "repl.hint.not_btrfs" => "The target is not on btrfs: replication needs btrfs on both sides.", "La destinazione non è su btrfs: la replica richiede btrfs da entrambe le parti.";
    "repl.hint.no_space" => "The target is full: free space there or keep fewer replicas.", "La destinazione è piena: libera spazio o conserva meno repliche.";
    "repl.hint.busy" => "Another run of this job was still going: the next run tries again.", "Un'altra esecuzione di questo job era ancora in corso: la prossima esecuzione riprova.";
    "repl.hint.btrbk" => "btrbk is missing or too old on this NAS or on the server.", "btrbk manca o è troppo vecchio su questo NAS o sul server.";
    "repl.hint.other" => "See the task history for the full output.", "L'output completo è nella cronologia delle attività.";
    "repl.warn.not_btrfs" => "Not on btrfs: replication needs a btrfs target.", "Non su btrfs: la replica richiede una destinazione btrfs.";
    "repl.warn.same_fs" => "On the volume of the source: not a second copy.", "Sul volume dell'origine: non è una seconda copia.";
    "repl.warn.not_mounted" => "No volume is mounted there: mount it before the next transfer.", "Lì non è montato alcun volume: montalo prima del prossimo trasferimento.";
    "repl.warn.no_host_key" => "Server identity not confirmed.", "Identità del server non confermata.";
    "repl.v.id" => "Use 1-24 lowercase letters, digits and '-'.", "Usa da 1 a 24 lettere minuscole, cifre e '-'.";
    "repl.v.path" => "Enter the absolute path of the target folder.", "Inserisci il percorso assoluto di una cartella.";
    "repl.v.host" => "Enter the server name or address.", "Inserisci il nome o l'indirizzo del server.";
    "repl.v.port" => "Enter a port from 1 to 65535.", "Inserisci una porta da 1 a 65535.";
    "repl.v.rate" => "Enter a number of KiB/s.", "Inserisci un numero di KiB/s.";
    // Backup (restic): Backup › Another NAS or cloud (bk.*) and its task names
    "nav.backup" => "Backup", "Backup";
    "bk.title" => "Backup", "Backup";
    "bk.subtitle" => "Encrypted, versioned copies of this NAS elsewhere, and the backups PCs keep here", "Copie cifrate e con versioni di questo NAS altrove, e i backup che i PC tengono qui";
    "bk.tab.restic" => "Another NAS or cloud", "Altro NAS o cloud";
    "bk.help" => "Each job copies the chosen folders into an encrypted repository. Only what changed is sent, and every run is a version you can restore. The repository opens with restic or rustic on any computer, with its password.", "Ogni job copia le cartelle scelte in un repository cifrato. Viene inviato solo ciò che è cambiato e ogni esecuzione è una versione ripristinabile. Il repository si apre con restic o rustic su qualsiasi computer, con la sua password.";
    "bk.create" => "New backup job", "Nuovo job di backup";
    "bk.crypt_note" => "Encrypted shares are backed up as their encrypted data (the folder shown next to them), locked or not: a restore gives that folder back, readable with the folder's password or recovery key.", "Le cartelle condivise crittografate vengono salvate come dati crittografati (la cartella indicata accanto), bloccate o no: un ripristino restituisce quella cartella, leggibile con la password o la chiave di ripristino della cartella.";
    "bk.empty" => "No backup jobs yet", "Ancora nessun job di backup";
    "bk.empty_text" => "Create a job to copy shared folders, Docker app data or the rynas configuration to another NAS, a USB disk or a cloud storage.", "Crea un job per copiare cartelle condivise, dati delle app Docker o la configurazione di rynas su un altro NAS, un disco USB o uno spazio cloud.";
    "bk.no_offsite" => "No job copies data off this NAS: a fire or a theft would take the backups too. Add a job to another NAS or to the cloud.", "Nessun job copia i dati fuori da questo NAS: un incendio o un furto si porterebbero via anche i backup. Aggiungi un job verso un altro NAS o il cloud.";
    "bk.col.job" => "Job and sources", "Job e origini";
    "bk.col.where" => "Destination", "Destinazione";
    "bk.col.size" => "Repository", "Repository";
    "bk.dest.local" => "USB or local disk", "Disco USB o locale";
    "bk.dest.rest" => "REST server", "Server REST";
    "bk.dest.sftp" => "SFTP", "SFTP";
    "bk.dest.s3" => "S3 compatible", "Compatibile S3";
    "bk.dest.b2" => "Backblaze B2", "Backblaze B2";
    "bk.dest.rclone" => "rclone (Google Drive, OneDrive…)", "rclone (Google Drive, OneDrive…)";
    "bk.u.last" => "latest", "ultime";
    "bk.u.h" => "h", "h";
    "bk.u.d" => "d", "g";
    "bk.u.w" => "w", "s";
    "bk.u.m" => "m", "m";
    "bk.u.y" => "y", "a";
    "bk.src.config" => "rynas configuration", "Configurazione di rynas";
    "bk.src.config_help" => "Settings, users, shares and jobs, encrypted with the repository password.", "Impostazioni, utenti, cartelle condivise e job, cifrati con la password del repository.";
    "bk.failed" => "Failed", "Non riuscito";
    "bk.never" => "Not run yet", "Mai eseguito";
    "bk.paused" => "Paused", "In pausa";
    "bk.versions_n" => "{n} versions", "{n} versioni";
    "bk.check.bad_at" => "Check failed {when}", "Verifica non riuscita {when}";
    "bk.run" => "Back up now", "Esegui backup ora";
    "bk.run.started" => "Backup {name} started", "Backup {name} avviato";
    "bk.run.ok" => "Backup {name} completed", "Backup {name} completato";
    "bk.run.failed" => "Backup {name} failed: {why}", "Backup {name} non riuscito: {why}";
    "bk.versions" => "Versions and restore", "Versioni e ripristino";
    "bk.versions_of" => "Versions of {name}", "Versioni di {name}";
    "bk.check" => "Check integrity", "Verifica integrità";
    "bk.check.title" => "Check {name}", "Verifica di {name}";
    "bk.check.text" => "The check reads the repository's index and structure; reading the data back too finds damaged files at the destination, and takes longer and costs traffic.", "La verifica legge l'indice e la struttura del repository; rileggere anche i dati trova i file danneggiati nella destinazione, ma richiede più tempo e traffico.";
    "bk.check.read" => "Data read back", "Dati riletti";
    "bk.check.structure" => "Structure only", "Solo struttura";
    "bk.check.five" => "5 % of the data", "Il 5 % dei dati";
    "bk.check.all" => "All the data", "Tutti i dati";
    "bk.check.started" => "Check of {name} started", "Verifica di {name} avviata";
    "bk.check.done" => "Check of {name}: no errors", "Verifica di {name}: nessun errore";
    "bk.delete" => "Delete job {name}?", "Eliminare il job {name}?";
    "bk.delete_text" => "The job and its schedule are removed. The repository and its versions stay at the destination.", "Il job e la sua pianificazione vengono rimossi. Il repository e le sue versioni restano nella destinazione.";
    "bk.sec.what" => "What to back up", "Cosa salvare";
    "bk.sec.where" => "Where", "Dove";
    "bk.sec.encryption" => "Encryption", "Cifratura";
    "bk.sec.versions" => "Versions to keep", "Versioni da conservare";
    "bk.f.name_help" => "Lowercase letters, digits and -, up to 32.", "Lettere minuscole, cifre e -, fino a 32.";
    "bk.f.description" => "Description", "Descrizione";
    "bk.f.folders" => "Other folders", "Altre cartelle";
    "bk.f.folders_help" => "One absolute path per line, e.g. the Docker apps folder.", "Un percorso assoluto per riga, ad esempio la cartella delle app Docker.";
    "bk.f.dest" => "Destination type", "Tipo di destinazione";
    "bk.f.path" => "Folder", "Cartella";
    "bk.f.path_help" => "A folder on a USB disk or another volume; it is created when missing.", "Una cartella su un disco USB o un altro volume; viene creata se manca.";
    "bk.f.path_on" => "On a volume", "Su un volume";
    "bk.f.same_disk" => "This folder is on the system disk: the backup protects against deleted or encrypted files, not against a failure of that disk. Keep another copy elsewhere.", "Questa cartella è sul disco di sistema: il backup protegge da file cancellati o cifrati, non da un guasto di quel disco. Tieni un'altra copia altrove.";
    "bk.f.url" => "Server address", "Indirizzo del server";
    "bk.f.url_help" => "A restic REST server (rest-server), also on another NAS. Start it with --append-only so that this NAS can add versions but never delete them.", "Un server REST di restic (rest-server), anche su un altro NAS. Avvialo con --append-only perché questo NAS possa aggiungere versioni ma mai cancellarle.";
    "bk.f.user" => "User", "Utente";
    "bk.f.password" => "Password", "Password";
    "bk.f.secret_help" => "Stored encrypted on this NAS, never shown again.", "Conservata cifrata su questo NAS, non viene più mostrata.";
    "bk.f.secret_kept" => "A secret is stored: leave empty to keep it.", "C'è un segreto salvato: lascia vuoto per mantenerlo.";
    "bk.f.host" => "Server", "Server";
    "bk.f.port" => "Port", "Porta";
    "bk.f.folder" => "Folder on the server", "Cartella sul server";
    "bk.f.ssh_key" => "Key of this NAS", "Chiave di questo NAS";
    "bk.f.ssh_key_help" => "rynas logs in with a key pair: generate it, then add the public key to the server's ~/.ssh/authorized_keys.", "rynas accede con una coppia di chiavi: generala, poi aggiungi la chiave pubblica a ~/.ssh/authorized_keys del server.";
    "bk.f.ssh_key_paste" => "Add this line to ~/.ssh/authorized_keys of the user on the server. At the first connection the server's key is remembered.", "Aggiungi questa riga a ~/.ssh/authorized_keys dell'utente sul server. Alla prima connessione la chiave del server viene memorizzata.";
    "bk.f.ssh_generate" => "Generate key pair", "Genera coppia di chiavi";
    "bk.f.endpoint" => "Endpoint", "Endpoint";
    "bk.f.endpoint_help" => "Empty for AWS; the service's S3 address otherwise (MinIO, Wasabi, Backblaze S3…).", "Vuoto per AWS; altrimenti l'indirizzo S3 del servizio (MinIO, Wasabi, Backblaze S3…).";
    "bk.f.bucket" => "Bucket", "Bucket";
    "bk.f.bucket_id" => "Bucket ID", "ID del bucket";
    "bk.f.region" => "Region", "Regione";
    "bk.f.prefix" => "Folder in the bucket", "Cartella nel bucket";
    "bk.f.access_key" => "Access key", "Chiave di accesso";
    "bk.f.secret_key" => "Secret key", "Chiave segreta";
    "bk.f.key_id" => "Application key ID", "ID della chiave applicazione";
    "bk.f.app_key" => "Application key", "Chiave applicazione";
    "bk.f.remote" => "rclone remote", "Remote di rclone";
    "bk.f.remote_help" => "name:folder of a remote in the configuration below (rclone must be installed).", "nome:cartella di un remote della configurazione qui sotto (rclone deve essere installato).";
    "bk.f.rclone_config" => "rclone configuration", "Configurazione di rclone";
    "bk.f.rclone_help" => "The remote's section of rclone.conf, with its token: create it with rclone config on a computer with a browser. Stored encrypted.", "La sezione del remote di rclone.conf, con il suo token: creala con rclone config su un computer con un browser. Conservata cifrata.";
    "bk.f.rclone_kept" => "A configuration is stored: leave empty to keep it.", "C'è una configurazione salvata: lascia vuoto per mantenerla.";
    "bk.test" => "Test connection", "Prova connessione";
    "bk.test.empty" => "Reachable: the repository will be created at the first run.", "Raggiungibile: il repository verrà creato alla prima esecuzione.";
    "bk.test.opens" => "Reachable: an existing repository opens with this password ({n} versions).", "Raggiungibile: un repository esistente si apre con questa password ({n} versioni).";
    "bk.test.locked" => "A repository is already there: give its password to use it.", "C'è già un repository: indica la sua password per usarlo.";
    "bk.test.failed" => "Not reachable", "Non raggiungibile";
    "bk.f.pw_mode" => "Repository password", "Password del repository";
    "bk.pw.generate" => "Generate a strong one", "Generane una robusta";
    "bk.pw.own" => "Use my own", "Usa la mia";
    "bk.pw.generate_help" => "rynas creates a random password and shows it once after saving.", "rynas crea una password casuale e la mostra una sola volta dopo il salvataggio.";
    "bk.pw.kept" => "Stored encrypted on this NAS. It cannot change: it is the key of the versions already in the repository (for another password, create a new job).", "Conservata cifrata su questo NAS. Non si può cambiare: è la chiave delle versioni già nel repository (per un'altra password crea un nuovo job).";
    "bk.f.dest_change" => "Changing the destination asks for its secrets again (passwords, keys, rclone configuration): the stored ones belong to the old destination.", "Cambiare destinazione richiede di nuovo i suoi segreti (password, chiavi, configurazione rclone): quelli salvati appartengono alla destinazione precedente.";
    "bk.f.ssh_replace" => "A key pair exists. A new one replaces it: the server's authorized_keys must then get the new public key.", "Esiste già una coppia di chiavi. Una nuova la sostituisce: in authorized_keys del server va poi messa la nuova chiave pubblica.";
    "bk.f.ssh_replace_go" => "Replace the key", "Sostituisci la chiave";
    "bk.delete_password" => "Without the repository password the versions left at the destination cannot be read. Keep a copy of it if you may need them.", "Senza la password del repository le versioni rimaste nella destinazione non si possono leggere. Tienine una copia se potrebbero servirti.";
    "bk.interrupted" => "Interrupted", "Interrotto";
    "bk.interrupted_text" => "{when} · the service stopped during the run; the system was put back as before", "{when} · il servizio si è fermato durante l'esecuzione; il sistema è stato rimesso com'era";
    "bk.warnings_n" => "Warnings of the last run: {n}", "Avvisi dell'ultima esecuzione: {n}";
    "bk.all_hosts" => "Show other NAS", "Mostra altri NAS";
    "bk.all_hosts_help" => "Also the versions other systems saved in the same repository.", "Anche le versioni salvate da altri sistemi nello stesso repository.";
    "bk.restore.original_confirm" => "I understand that files at the original place are replaced", "Ho capito che i file nella posizione originale vengono sostituiti";
    "bk.restore.overwrite" => "Overwrite and restore", "Sovrascrivi e ripristina";
    "bk.f.repo_password" => "Password", "Password del repository cifrato";
    "bk.f.repo_password_help" => "At least 8 characters; the password of an existing repository to add to it.", "Almeno 8 caratteri; la password di un repository esistente per aggiungervi versioni.";
    "bk.pw.title" => "Save the repository password", "Salva la password del repository";
    "bk.pw.warning" => "Without this password the backup cannot be restored, not even by you: if this NAS is lost, it is the only key to your data. Keep it outside this NAS (password manager, printed copy).", "Senza questa password il backup non si può ripristinare, nemmeno da te: se questo NAS va perso, è l'unica chiave dei tuoi dati. Conservala fuori da questo NAS (gestore di password, copia stampata).";
    "bk.pw.download" => "Download as a file", "Scarica come file";
    "bk.pw.file_note" => "rynas backup repository password (restic format). Keep it safe.", "Password del repository di backup di rynas (formato restic). Conservala al sicuro.";
    "bk.pw.saved" => "I have saved the password", "Ho salvato la password";
    "bk.pw.done" => "Done", "Fatto";
    "bk.copy" => "Copy", "Copia";
    "bk.copied" => "Copied to the clipboard", "Copiato negli appunti";
    "bk.f.enabled" => "Run on schedule", "Esegui secondo la pianificazione";
    "bk.f.enabled_help" => "Off: only Back up now runs it.", "Disattivato: parte solo con Esegui backup ora.";
    "bk.f.keep_help" => "After each run, older versions beyond these counts are forgotten; their space is freed by the next cleanup.", "Dopo ogni esecuzione le versioni più vecchie oltre questi numeri vengono dimenticate; il loro spazio viene liberato dalla pulizia successiva.";
    "bk.f.keep_last" => "Latest", "Ultime";
    "bk.f.keep_hourly" => "Hourly", "Orarie";
    "bk.f.keep_daily" => "Daily", "Giornaliere";
    "bk.f.keep_weekly" => "Weekly", "Settimanali";
    "bk.f.keep_monthly" => "Monthly", "Mensili";
    "bk.f.keep_yearly" => "Yearly", "Annuali";
    "bk.f.smart" => "Use the recommended counts", "Usa i valori consigliati";
    "bk.f.use_snapshot" => "Read from a snapshot", "Leggi da uno snapshot";
    "bk.f.use_snapshot_help" => "Each folder that is a btrfs subvolume is read from a temporary read-only snapshot: files changing during the backup (databases) stay consistent.", "Ogni cartella che è un subvolume btrfs viene letta da uno snapshot temporaneo in sola lettura: i file che cambiano durante il backup (database) restano coerenti.";
    "bk.f.stop_stacks" => "Docker stacks to stop", "Stack Docker da fermare";
    "bk.f.stop_stacks_help" => "Comma-separated; stopped while the snapshot is taken (or during the whole backup without one), then started again.", "Separati da virgole; fermati mentre viene creato lo snapshot (o per tutto il backup senza snapshot), poi riavviati.";
    "bk.f.excludes" => "Exclude", "Escludi";
    "bk.f.excludes_help" => "One pattern per line. Recycle bins, snapshot folders and folders with a CACHEDIR.TAG or .nobackup file are always left out.", "Un modello per riga. Cestini, cartelle degli snapshot e cartelle con un file CACHEDIR.TAG o .nobackup sono sempre esclusi.";
    "bk.f.bandwidth" => "Upload limit (KiB/s, 0 none)", "Limite di invio (KiB/s, 0 nessuno)";
    "bk.f.prune_days" => "Clean up every (days)", "Pulizia ogni (giorni)";
    "bk.f.check_days" => "Check every (days, 0 never)", "Verifica ogni (giorni, 0 mai)";
    "bk.f.read_pct" => "Data read by each check (%)", "Dati letti da ogni verifica (%)";
    "bk.f.maintenance_help" => "The upload limit applies to S3, B2, SFTP and rclone. Clean-ups and checks run after a backup, one at a time per repository.", "Il limite di invio vale per S3, B2, SFTP e rclone. Pulizie e verifiche partono dopo un backup, una alla volta per repository.";
    "bk.v.name" => "Give the job a name: lowercase letters, digits and -.", "Dai un nome al job: lettere minuscole, cifre e -.";
    "bk.v.folder" => "Other folders must be absolute paths.", "Le altre cartelle devono essere percorsi assoluti.";
    "bk.v.sources" => "Choose at least one thing to back up.", "Scegli almeno una cosa da salvare.";
    "bk.v.retention" => "Keep at least one kind of version.", "Conserva almeno un tipo di versione.";
    "bk.v.password" => "The repository password needs at least 8 characters.", "La password del repository richiede almeno 8 caratteri.";
    "bk.v.restore_folder" => "Enter an absolute folder path.", "Inserisci il percorso assoluto di una cartella.";
    "bk.no_versions" => "No versions yet", "Ancora nessuna versione";
    "bk.no_versions_text" => "The first run of the job creates the first version.", "La prima esecuzione del job crea la prima versione.";
    "bk.all_versions" => "All versions", "Tutte le versioni";
    "bk.up" => "Up", "Su";
    "bk.pick" => "Select", "Seleziona";
    "bk.download" => "Download", "Scarica";
    "bk.restore" => "Restore", "Ripristina";
    "bk.restore_all" => "Restore this version", "Ripristina questa versione";
    "bk.restore_n" => "Restore {n} selected", "Ripristina {n} selezionati";
    "bk.restore.to" => "Restore to", "Ripristina in";
    "bk.restore.original" => "Original place", "Posizione originale";
    "bk.restore.folder" => "Another folder", "Un'altra cartella";
    "bk.restore.folder_path" => "Target folder", "Cartella di destinazione";
    "bk.restore.folder_help" => "The files go below it with their full path, e.g. /volume1/restore/volume1/docs.", "I file vanno al suo interno con il percorso completo, ad esempio /volume1/restore/volume1/docs.";
    "bk.restore.original_warn" => "Files that changed since this version are overwritten; newer files are kept.", "I file cambiati dopo questa versione vengono sovrascritti; i file più nuovi restano.";
    "bk.restore.all_text" => "Everything in this version is restored.", "Viene ripristinato tutto il contenuto di questa versione.";
    "bk.restore.n_text" => "{n} selected items are restored with their folders.", "Vengono ripristinati {n} elementi selezionati con le loro cartelle.";
    "bk.restore.started" => "Restore started", "Ripristino avviato";
    "bk.restore.done" => "Restore completed", "Ripristino completato";
    "task.backup.run" => "Backup", "Backup";
    "task.backup.check" => "Backup integrity check", "Verifica integrità del backup";
    "task.backup.restore" => "Restore from backup", "Ripristino da backup";
    // network page
    "nav.network" => "Network", "Rete";
    "net.title" => "Network", "Rete";
    "net.subtitle" => "Interfaces, addresses and rescue paths. Every change is tested and rolled back unless you confirm it.", "Interfacce, indirizzi e accessi di emergenza. Ogni modifica viene provata e annullata se non la confermi.";
    "net.tab_ifaces" => "Interfaces", "Interfacce";
    "net.tab_revisions" => "Revisions", "Revisioni";
    "net.tab_rescue" => "Rescue access", "Accesso di emergenza";
    "net.tab_tools" => "Diagnostics", "Diagnostica";
    "net.summary" => "Overview", "Riepilogo";
    "net.services" => "Network services", "Servizi di rete";
    "net.hostname" => "Host name", "Nome host";
    "net.mgmt" => "This session comes through", "Questa sessione arriva da";
    "net.mgmt_unknown" => "Unknown (local or proxied connection)", "Sconosciuta (connessione locale o tramite proxy)";
    "net.gateway" => "Default gateway", "Gateway predefinito";
    "net.dns" => "DNS servers", "Server DNS";
    "net.search" => "Search domains", "Domini di ricerca";
    "net.lkg" => "Last known good revision", "Ultima revisione funzionante";
    "net.lkg_none" => "None yet", "Nessuna per ora";
    "net.svc_networkd" => "systemd-networkd", "systemd-networkd";
    "net.svc_resolved" => "systemd-resolved", "systemd-resolved";
    "net.svc_ifupdown" => "ifupdown (networking)", "ifupdown (networking)";
    "net.svc_nm" => "NetworkManager", "NetworkManager";
    "net.svc_avahi" => "Avahi (mDNS)", "Avahi (mDNS)";
    "net.running" => "Running", "In esecuzione";
    "net.not_running" => "Not running", "Non attivo";
    "net.reason.timeout" => "not confirmed in time", "non confermata in tempo";
    "net.reason.manual" => "rolled back by hand", "annullata a mano";
    "net.reason.verify_failed" => "the new settings did not come up", "le nuove impostazioni non si sono attivate";
    "net.reason.boot_guard" => "a test was interrupted by a reboot", "una prova è stata interrotta da un riavvio";
    "net.reason.boot_check" => "no routable address after the boot", "nessun indirizzo instradabile dopo l'avvio";
    "net.reason.watchdog" => "the network was unreachable after the boot", "la rete non era raggiungibile dopo l'avvio";
    "net.reason.other" => "automatic", "automatico";
    "net.rolled_back_at" => "Network settings rolled back on {date} ({reason}): revision #{rev} is active again.", "Impostazioni di rete annullate il {date} ({reason}): è di nuovo attiva la revisione #{rev}.";
    "net.rollback_unverified" => "The restored state could not be verified: check the interfaces.", "Non è stato possibile verificare lo stato ripristinato: controlla le interfacce.";
    "net.adopt_title" => "Network configured by ifupdown", "Rete configurata da ifupdown";
    "net.adopt_text" => "/etc/network/interfaces configures the ports, so they are read-only here. Let rynas manage them to edit them safely (same addresses, with a timed test).", "Le porte sono configurate da /etc/network/interfaces e qui sono in sola lettura. Affidale a rynas per modificarle in sicurezza (stessi indirizzi, con prova a tempo).";
    "net.adopt_btn" => "Let rynas manage…", "Affida a rynas…";
    "net.adopt_dialog_text" => "rynas writes systemd-networkd files with the same addresses, keeps the original as interfaces.rynas-bak and switches from ifupdown to systemd-networkd. Bonds, bridges, VLANs and static IPv6 are not migrated yet.", "rynas scrive i file di systemd-networkd con gli stessi indirizzi, conserva l'originale come interfaces.rynas-bak e passa da ifupdown a systemd-networkd. Bond, bridge, VLAN e IPv6 statico non vengono ancora migrati.";
    "net.adopt_when" => "When", "Quando";
    "net.adopt_next_boot" => "At the next reboot (recommended)", "Al prossimo riavvio (consigliato)";
    "net.adopt_next_boot_help" => "Safest when you are connected over the network: if no port has a routable address 3 minutes after the boot, ifupdown comes back.", "La scelta più sicura se sei collegato via rete: se 3 minuti dopo l'avvio nessuna porta ha un indirizzo instradabile, torna ifupdown.";
    "net.adopt_now" => "Now, with a timed test", "Adesso, con prova a tempo";
    "net.adopt_now_help" => "The network restarts now; confirm within the time limit or ifupdown comes back.", "La rete si riavvia subito; conferma entro il tempo limite o torna ifupdown.";
    "net.adopt_go" => "Let rynas manage", "Affida a rynas";
    "net.adopt_scheduled" => "Migration planned for the next reboot: {ifaces}.", "Migrazione programmata al prossimo riavvio: {ifaces}.";
    "net.pending_title" => "Network changes not applied yet", "Modifiche di rete non ancora applicate";
    "net.pending_text" => "Staged on {ifaces}. Nothing changes until you apply them.", "Preparate su {ifaces}. Non cambia nulla finché non le applichi.";
    "net.review_apply" => "Review and apply…", "Rivedi e applica…";
    "net.discard_title" => "Discard the network changes?", "Scartare le modifiche di rete?";
    "net.discard_text" => "The staged settings of every interface are dropped. The running network does not change.", "Le impostazioni preparate di tutte le interfacce vengono eliminate. La rete in uso non cambia.";
    "net.discarded" => "Network changes discarded", "Modifiche di rete scartate";
    "net.apply_title" => "Apply network settings", "Applica le impostazioni di rete";
    "net.timeout" => "Time to confirm", "Tempo per confermare";
    "net.timeout_help" => "Without a confirmation in this time the previous settings come back by themselves.", "Senza conferma entro questo tempo tornano da sole le impostazioni precedenti.";
    "net.seconds" => "{n} s", "{n} s";
    "net.mgmt_touched" => "You are connected through {iface}. After Apply this page may lose the connection: open the NAS at {addrs} and confirm there.", "Sei collegato tramite {iface}. Dopo Applica questa pagina potrebbe perdere la connessione: apri il NAS su {addrs} e conferma da lì.";
    "net.mgmt_touched_dhcp" => "You are connected through {iface}. After Apply the address may change (DHCP): reconnect through the host name, the link-local address or the address your router assigns, and confirm there.", "Sei collegato tramite {iface}. Dopo Applica l'indirizzo potrebbe cambiare (DHCP): ricollegati con il nome host, l'indirizzo link-local o l'indirizzo assegnato dal router e conferma da lì.";
    "net.mgmt_unknown_apply" => "rynas cannot tell which interface this session uses (local or proxied connection), so the timed test is mandatory.", "rynas non sa quale interfaccia usa questa sessione (connessione locale o tramite proxy), quindi la prova a tempo è obbligatoria.";
    "net.skip_test" => "Apply without the timed test", "Applica senza prova a tempo";
    "net.skip_test_help" => "Only for interfaces this session does not use. There is no automatic rollback.", "Solo per interfacce che questa sessione non usa. Non c'è annullamento automatico.";
    "net.test_mandatory" => "The timed test cannot be skipped: the change touches the connection you are using.", "La prova a tempo non si può saltare: la modifica tocca la connessione che stai usando.";
    "net.apply_now" => "Apply now", "Applica ora";
    "net.apply_test" => "Apply with a {n} s test", "Applica con prova di {n} s";
    "net.trial_title" => "Confirm the new network settings", "Conferma le nuove impostazioni di rete";
    "net.trial_text" => "Confirm within {n} s or they are rolled back to revision {rev}.", "Conferma entro {n} s o si torna alla revisione {rev}.";
    "net.trial_deadline" => "Time is up", "Tempo scaduto";
    "net.trial_deadline_text" => "Not confirmed: revision {rev} is being restored.", "Non confermate: si sta ripristinando la revisione {rev}.";
    "net.trial_applying" => "Applying network settings", "Applicazione delle impostazioni di rete";
    "net.trial_applying_text" => "Applied without a timed test. This takes a few seconds.", "Applicate senza prova a tempo. Servono pochi secondi.";
    "net.trial_starting" => "Checking the new addresses and writing the configuration…", "Controllo dei nuovi indirizzi e scrittura della configurazione…";
    "net.trial_who" => "Started by {user} on {ifaces}.", "Avviata da {user} su {ifaces}.";
    "net.previous" => "the previous settings", "le impostazioni precedenti";
    "net.left_sr" => "About {n} seconds left to confirm.", "Circa {n} secondi per confermare.";
    "net.confirm" => "Keep these settings", "Mantieni queste impostazioni";
    "net.rollback_now" => "Roll back now", "Annulla ora";
    "net.no_token" => "Confirm from the browser where the change was applied, or roll it back here.", "Conferma dal browser in cui è stata applicata la modifica, oppure annullala da qui.";
    "net.confirmed" => "Network settings kept: revision #{rev} is the last known good.", "Impostazioni di rete mantenute: la revisione #{rev} è l'ultima funzionante.";
    "net.confirm_conflict" => "The confirmation must come over the new network: open the NAS at one of the new addresses below and confirm there.", "La conferma deve arrivare dalla nuova rete: apri il NAS su uno dei nuovi indirizzi qui sotto e conferma da lì.";
    "net.confirm_token" => "This confirmation link is not valid for the running test.", "Questo link di conferma non vale per la prova in corso.";
    "net.kept" => "The new network settings are kept.", "Le nuove impostazioni di rete sono state mantenute.";
    "net.applied" => "Network settings applied.", "Impostazioni di rete applicate.";
    "net.not_kept" => "The new network settings were not kept: the previous ones were restored.", "Le nuove impostazioni di rete non sono state mantenute: sono state ripristinate le precedenti.";
    "net.lost_title" => "Connection lost.", "Connessione persa.";
    "net.lost_text" => "Expected if the address of this page changed. This page keeps trying by itself; meanwhile open the NAS at a new address and confirm there.", "È normale se l'indirizzo di questa pagina è cambiato. La pagina continua a riprovare da sola; intanto apri il NAS su un nuovo indirizzo e conferma da lì.";
    "net.lost_after" => "Not confirmed in time: the previous settings are coming back. This page reconnects by itself, or reopen {url}.", "Non confermato in tempo: stanno tornando le impostazioni precedenti. La pagina si ricollega da sola, oppure riapri {url}.";
    "net.cands_title" => "Where the NAS answers after the change:", "Dove risponde il NAS dopo la modifica:";
    "net.cands_text" => "Each link opens the confirmation page (sign in again if asked).", "Ogni link apre la pagina di conferma (accedi di nuovo se richiesto).";
    "net.boot_title" => "Migration planned for the next reboot", "Migrazione programmata al prossimo riavvio";
    "net.boot_text" => "At the next boot systemd-networkd takes over {ifaces}. If no port has a routable address 3 minutes later, ifupdown comes back by itself.", "Al prossimo avvio systemd-networkd prende in carico {ifaces}. Se dopo 3 minuti nessuna porta ha un indirizzo instradabile, torna da solo ifupdown.";
    "net.boot_cancel" => "Cancel the migration", "Annulla la migrazione";
    "net.boot_who" => "Planned by {user} on {when}.", "Programmata da {user} il {when}.";
    "net.boot_cancelled" => "The migration planned for the next reboot is cancelled.", "La migrazione programmata al prossimo riavvio è annullata.";
    "net.booted_title" => "Network migrated at this boot", "Rete migrata a questo avvio";
    "net.booted_text" => "systemd-networkd now manages {ifaces}. When the countdown ends the migration stays if a port has a routable address; otherwise ifupdown comes back.", "Ora {ifaces} è gestita da systemd-networkd. Allo scadere del tempo la migrazione resta se una porta ha un indirizzo instradabile; altrimenti torna ifupdown.";
    "net.booted_text_n" => "systemd-networkd now manages {ifaces}. When the countdown ends the migration stays if a port has a routable address; otherwise ifupdown comes back.", "Ora {ifaces} sono gestite da systemd-networkd. Allo scadere del tempo la migrazione resta se una porta ha un indirizzo instradabile; altrimenti torna ifupdown.";
    "net.booted_left_sr" => "About {n} seconds before the migration stays or ifupdown comes back.", "Circa {n} secondi prima che la migrazione resti o torni ifupdown.";
    "net.booted_planned" => "{user} planned it for the next reboot from the Network page.", "{user} l'aveva programmata al prossimo riavvio dalla pagina Rete.";
    "net.booted_keep" => "Keep it now", "Tienila subito";
    "net.booted_back" => "Go back to ifupdown", "Torna a ifupdown";
    "net.col_iface" => "Interface", "Interfaccia";
    "net.col_speed" => "Speed", "Velocità";
    "net.col_mac" => "MAC address", "Indirizzo MAC";
    "net.col_addresses" => "Addresses", "Indirizzi";
    "net.col_config" => "Configuration", "Configurazione";
    "net.col_kind" => "Type", "Tipo";
    "net.col_ifaces" => "Interfaces", "Interfacce";
    "net.k.ethernet" => "Ethernet", "Ethernet";
    "net.k.bond" => "Bond", "Bond";
    "net.k.bridge" => "Bridge", "Bridge";
    "net.k.vlan" => "VLAN", "VLAN";
    "net.k.loopback" => "Loopback", "Loopback";
    "net.k.wireless" => "Wireless", "Wireless";
    "net.k.virtual" => "Virtual", "Virtuale";
    "net.k.other" => "Other", "Altro";
    "net.b.networkd" => "systemd-networkd", "systemd-networkd";
    "net.b.ifupdown" => "ifupdown", "ifupdown";
    "net.b.nm" => "NetworkManager", "NetworkManager";
    "net.b.none" => "Not configured", "Non configurata";
    "net.m.dhcp" => "DHCP", "DHCP";
    "net.m.static" => "Static", "Statico";
    "net.m.disabled" => "Off (link-local only)", "Disattivato (solo link-local)";
    "net.st_up" => "Connected", "Connessa";
    "net.st_dormant" => "Cable, not up", "Cavo presente, non attiva";
    "net.st_no_cable" => "No cable", "Nessun cavo";
    "net.st_down" => "Down", "Disattivata";
    "net.st_unknown" => "Unknown", "Sconosciuto";
    "net.half_duplex" => "half duplex", "half duplex";
    "net.dynamic" => "dynamic", "dinamico";
    "net.configured" => "configured", "configurato";
    "net.no_address" => "No address", "Nessun indirizzo";
    "net.read_only" => "Read-only", "Sola lettura";
    "net.this_connection" => "This connection", "Questa connessione";
    "net.this_connection_help" => "You reach rynas through this interface: changes to it are always tested with a timed confirmation.", "Raggiungi rynas tramite questa interfaccia: le sue modifiche vengono sempre provate con conferma a tempo.";
    "net.show_virtual" => "Show virtual interfaces", "Mostra interfacce virtuali";
    "net.hidden_virtual" => "{n} virtual interfaces hidden", "{n} interfacce virtuali nascoste";
    "net.none_text" => "The system reports no network interface rynas can show.", "Il sistema non segnala interfacce di rete da mostrare.";
    "net.details_title" => "Details of {name}", "Dettagli di {name}";
    "net.read_only_text" => "This interface is read-only in rynas.", "Questa interfaccia è in sola lettura in rynas.";
    "net.ro.loopback" => "The loopback interface cannot be configured.", "L'interfaccia di loopback non è configurabile.";
    "net.ro.enslaved" => "It is a member of {master}: change it from the bond.", "È un membro di {master}: modificalo dal bond.";
    "net.ro.not_ethernet" => "Only Ethernet ports and the bonds rynas made can be configured in this version.", "In questa versione si possono configurare solo le porte Ethernet e i bond creati da rynas.";
    "net.ro.ifupdown" => "Configured by ifupdown (/etc/network/interfaces): use Let rynas manage first.", "Configurata da ifupdown (/etc/network/interfaces): usa prima Affida a rynas.";
    "net.ro.network_manager" => "Managed by NetworkManager: rynas does not change it.", "Gestita da NetworkManager: rynas non la modifica.";
    "net.ro.no_networkd" => "systemd-networkd is not running: rynas configures the interfaces through it.", "systemd-networkd non è in esecuzione: rynas configura le interfacce tramite questo servizio.";
    "net.mgmt_edit" => "You are connected through this interface. The change is tested with a timed confirmation; if its address changes you will reconnect at the new one.", "Sei collegato tramite questa interfaccia. La modifica viene provata con conferma a tempo; se cambia l'indirizzo ti ricollegherai a quello nuovo.";
    "net.has_pending" => "This interface already has a staged change: you are editing it.", "Questa interfaccia ha già una modifica preparata: la stai modificando.";
    "net.sec_ipv4" => "IPv4", "IPv4";
    "net.sec_diff" => "Changes", "Modifiche";
    "net.sec_details" => "Details", "Dettagli";
    "net.sec_applied" => "Applied settings", "Impostazioni applicate";
    "net.sec_ifupdown" => "ifupdown configuration (read-only)", "Configurazione ifupdown (sola lettura)";
    "net.f_method" => "IPv4 address", "Indirizzo IPv4";
    "net.f_addresses" => "Addresses", "Indirizzi";
    "net.f_gateway" => "Gateway", "Gateway";
    "net.f_mtu" => "MTU", "MTU";
    "net.method_help" => "Every port also keeps a link-local rescue address.", "Ogni porta conserva anche un indirizzo link-local di emergenza.";
    "net.addresses_help" => "One per line, with the prefix (192.0.2.10/24).", "Uno per riga, con il prefisso (192.0.2.10/24).";
    "net.gateway_help" => "Optional; must be inside one of the addresses' subnets.", "Facoltativo; deve stare in una delle sottoreti degli indirizzi.";
    "net.dns_help" => "One per line. With DHCP they are added to the servers the lease gives.", "Uno per riga. Con DHCP si aggiungono a quelli forniti dal server DHCP.";
    "net.mtu_help" => "Empty: the driver default (usually 1500).", "Vuoto: il valore predefinito del driver (di solito 1500).";
    "net.mtu_help_max" => "Empty: the driver default (usually 1500). This port accepts up to {max}.", "Vuoto: il valore predefinito del driver (di solito 1500). Questa porta accetta fino a {max}.";
    "net.mtu_default" => "default", "predefinito";
    "net.max" => "max", "max";
    "net.stage" => "Stage change", "Prepara modifica";
    "net.staged" => "Change staged: apply it from the Network page", "Modifica preparata: applicala dalla pagina Rete";
    "net.revert" => "Drop staged change", "Elimina modifica preparata";
    "net.no_changes" => "No changes.", "Nessuna modifica.";
    "net.phase_note" => "IPv6 settings, VLANs and bridges are shown read-only for now.", "Per ora impostazioni IPv6, VLAN e bridge sono in sola lettura.";
    "net.driver" => "Driver", "Driver";
    "net.backend" => "Managed by", "Gestita da";
    "net.config_file" => "Configuration file", "File di configurazione";
    "net.link_local" => "Link-local rescue address", "Indirizzo link-local di emergenza";
    "net.dns_live" => "DNS of this link", "DNS di questo collegamento";
    "net.ifu_method" => "Method", "Metodo";
    "net.ifu_bond" => "Bond members", "Membri del bond";
    "net.ifu_bridge" => "Bridge ports", "Porte del bridge";
    "net.ifu_vlan" => "VLAN parent", "Interfaccia padre VLAN";
    "net.ifu_file" => "File", "File";
    "net.bond_create" => "Create bond", "Crea bond";
    "net.bond_create_title" => "New bond {name}", "Nuovo bond {name}";
    "net.no_ports_free" => "No free Ethernet port for a bond.", "Nessuna porta Ethernet libera per un bond.";
    "net.f_members" => "Member ports", "Porte membro";
    "net.members_help" => "Physical ports that work as one link. The bond takes the address settings below; the ports lose their own.", "Porte fisiche che lavorano come un solo collegamento. Il bond prende le impostazioni di indirizzo qui sotto; le porte perdono le proprie.";
    "net.f_bond_mode" => "Bond mode", "Modalità del bond";
    "net.bm.ab" => "Fault tolerance (active-backup)", "Tolleranza ai guasti (active-backup)";
    "net.bm.alb" => "Adaptive load balancing (balance-alb)", "Bilanciamento adattivo del carico (balance-alb)";
    "net.bm.lacp" => "LACP (802.3ad)", "LACP (802.3ad)";
    "net.bm.xor" => "Static link aggregation (balance-xor)", "Aggregazione statica (balance-xor)";
    "net.bm.tlb" => "Adaptive transmit load balancing (balance-tlb)", "Bilanciamento adattivo in uscita (balance-tlb)";
    "net.bm.rr" => "Round robin (balance-rr)", "Round robin (balance-rr)";
    "net.bm.bc" => "Broadcast (every port)", "Broadcast (tutte le porte)";
    "net.bq.ab" => "Works with any switch. One port carries the traffic; another takes over when it fails.", "Funziona con qualsiasi switch. Una porta porta il traffico; un'altra subentra se si guasta.";
    "net.bq.alb" => "Works with any switch. Spreads the traffic over the ports, also the incoming traffic per client.", "Funziona con qualsiasi switch. Distribuisce il traffico sulle porte, anche quello in entrata per client.";
    "net.bq.lacp" => "Needs a managed switch with a LACP group on the same ports. Faster for many clients, not for a single transfer.", "Richiede uno switch gestito con un gruppo LACP sulle stesse porte. Più veloce con molti client, non per un singolo trasferimento.";
    "net.bq.xor" => "Needs a switch with a static link aggregation group on the same ports.", "Richiede uno switch con un gruppo di aggregazione statico sulle stesse porte.";
    "net.bq.tlb" => "Works with any switch. Balances the outgoing traffic only.", "Funziona con qualsiasi switch. Bilancia solo il traffico in uscita.";
    "net.bq.legacy" => "Kept as it was configured; not offered for new bonds.", "Mantenuta come era configurata; non proposta per i nuovi bond.";
    "net.lacp_warn" => "Configure the LACP group on the switch first. Without it no partner answers within about 35 seconds and the change is rolled back.", "Configura prima il gruppo LACP sullo switch. Senza, nessun partner risponde entro circa 35 secondi e la modifica viene annullata.";
    "net.f_primary" => "Preferred port", "Porta preferita";
    "net.primary_none" => "None (the first that works)", "Nessuna (la prima che funziona)";
    "net.primary_help" => "Carries the traffic whenever it has link; choose the faster port.", "Porta il traffico ogni volta che ha il link; scegli la porta più veloce.";
    "net.f_hash" => "Transmit hash", "Hash di trasmissione";
    "net.hash_help" => "How connections are spread over the ports. Layer 3+4 spreads better; layer 2+3 suits every switch.", "Come le connessioni vengono distribuite sulle porte. Livello 3+4 distribuisce meglio; livello 2+3 va bene con ogni switch.";
    "net.bh.l23" => "Layer 2+3 (MAC and IP)", "Livello 2+3 (MAC e IP)";
    "net.bh.l34" => "Layer 3+4 (IP and port)", "Livello 3+4 (IP e porta)";
    "net.bh.l2" => "Layer 2 (MAC)", "Livello 2 (MAC)";
    "net.mixed_speeds" => "The ports run at different speeds. With fault tolerance choose the faster one as preferred port.", "Le porte vanno a velocità diverse. Con la tolleranza ai guasti scegli la più veloce come porta preferita.";
    "net.v_bond_members" => "Choose at least one port.", "Scegli almeno una porta.";
    "net.bond_delete" => "Delete bond", "Elimina bond";
    "net.bond_deleted" => "Deletion staged: apply it from the Network page", "Eliminazione preparata: applicala dalla pagina Rete";
    "net.bond_dropped" => "The new bond is no longer staged", "Il nuovo bond non è più preparato";
    "net.bond_removed" => "To be deleted", "Da eliminare";
    "net.bond_removed_text" => "This bond is staged for deletion. When applied, the port with its MAC address takes its address settings and the other ports stay without IPv4.", "Questo bond è preparato per l'eliminazione. Quando la applichi, la porta con il suo indirizzo MAC prende le sue impostazioni di indirizzo e le altre porte restano senza IPv4.";
    "net.bond_members" => "Members: {list}", "Membri: {list}";
    "net.member_of" => "Member of {master}", "Membro di {master}";
    "net.member_active" => "active", "attiva";
    "net.member_standby" => "standby", "in riserva";
    "net.member_down" => "no link", "senza link";
    "net.member_failures" => "{n} link failures", "{n} interruzioni del link";
    "net.lacp_partner" => "LACP partner", "Partner LACP";
    "net.lacp_none" => "None: the switch does not answer", "Nessuno: lo switch non risponde";
    "net.lacp_bad" => "No LACP partner", "Nessun partner LACP";
    "net.st_new" => "Not created yet", "Non ancora creato";
    "net.v_address" => "Use host addresses with a prefix of 8–32 (192.0.2.10/24).", "Usa indirizzi host con prefisso 8–32 (192.0.2.10/24).";
    "net.v_addresses_none" => "A static configuration needs at least one address.", "Una configurazione statica richiede almeno un indirizzo.";
    "net.v_gateway" => "Enter an IPv4 address.", "Inserisci un indirizzo IPv4.";
    "net.v_gateway_subnet" => "The gateway must be inside an address's subnet and differ from it.", "Il gateway deve stare nella sottorete di un indirizzo ed essere diverso da esso.";
    "net.v_dns" => "Enter IP addresses (IPv4 or IPv6), not names.", "Inserisci indirizzi IP (IPv4 o IPv6), non nomi.";
    "net.v_mtu" => "MTU out of range for this port.", "MTU fuori dall'intervallo accettato da questa porta.";
    "net.v_host" => "Enter a host name or address, without spaces.", "Inserisci un nome host o un indirizzo, senza spazi.";
    "net.v_count" => "1 to 10.", "Da 1 a 10.";
    "net.v_size" => "0 to 65000 bytes.", "Da 0 a 65000 byte.";
    "net.revs_intro" => "Every change keeps a snapshot of the network before it. Restoring a revision is tested like an apply.", "Ogni modifica conserva una copia della rete precedente. Il ripristino di una revisione viene provato come un'applicazione.";
    "net.revs_empty" => "No revisions yet", "Ancora nessuna revisione";
    "net.revs_empty_text" => "They appear with the first applied change.", "Compaiono con la prima modifica applicata.";
    "net.rk.snapshot" => "Snapshot before a change", "Copia prima di una modifica";
    "net.rk.apply" => "Applied", "Applicata";
    "net.rk.adopt" => "Adopted from ifupdown", "Adottata da ifupdown";
    "net.rk.rescue" => "Rescue settings", "Impostazioni di emergenza";
    "net.rk.restore" => "Restored", "Ripristinata";
    "net.rk.other" => "Other", "Altro";
    "net.rev_lkg" => "Last known good", "Ultima funzionante";
    "net.rev_good" => "Good", "Funzionante";
    "net.rev_unconfirmed" => "Not confirmed", "Non confermata";
    "net.restore_rev" => "Restore revision #{rev}", "Ripristina la revisione #{rev}";
    "net.restore_text" => "The network of {date} comes back with a timed test: confirm it from the restored network or it is rolled back.", "Torna la rete del {date} con una prova a tempo: confermala dalla rete ripristinata o viene annullata.";
    "net.rescue_intro" => "Ways in when the normal address does not work. They stay on whatever the interface settings are.", "Vie d'accesso quando l'indirizzo normale non funziona. Restano attive qualunque siano le impostazioni delle interfacce.";
    "net.mdns" => "Local name (mDNS)", "Nome locale (mDNS)";
    "net.mdns_text" => "Computers on the same network find the NAS by this name, even when its address changes.", "I computer della stessa rete trovano il NAS con questo nome, anche se cambia indirizzo.";
    "net.avahi_on" => "Avahi running", "Avahi attivo";
    "net.avahi_off" => "Avahi not running", "Avahi non attivo";
    "net.mdns_service" => "Announce rynas on the network", "Annuncia rynas nella rete";
    "net.mdns_service_help" => "Publishes the web interface over Bonjour/mDNS (_rynas._tcp, _http._tcp). Applied with the pending changes.", "Pubblica l'interfaccia web via Bonjour/mDNS (_rynas._tcp, _http._tcp). Si applica con le modifiche in sospeso.";
    "net.ll_title" => "Link-local addresses", "Indirizzi link-local";
    "net.ll_text" => "Always on every port and derived from the MAC address: connect a computer straight to the port with a cable and open this address.", "Sempre attivi su ogni porta e ricavati dall'indirizzo MAC: collega un computer direttamente alla porta con un cavo e apri questo indirizzo.";
    "net.ll_none" => "No port has a MAC address.", "Nessuna porta ha un indirizzo MAC.";
    "net.ll_active" => "Active", "Attivo";
    "net.ll_inactive" => "Not on the port now", "Non presente ora";
    "net.tools_intro" => "Run from the NAS itself. 20 runs per minute.", "Eseguiti dal NAS stesso. 20 esecuzioni al minuto.";
    "net.d_tool" => "Tool", "Strumento";
    "net.d_ping" => "Ping", "Ping";
    "net.d_trace" => "Traceroute", "Traceroute";
    "net.d_dns" => "DNS lookup", "Ricerca DNS";
    "net.d_host" => "Host", "Host";
    "net.d_name" => "Name", "Nome";
    "net.d_record" => "Record type", "Tipo di record";
    "net.d_count" => "Packets", "Pacchetti";
    "net.d_size" => "Payload size (bytes)", "Dimensione dati (byte)";
    "net.d_iface" => "From interface", "Dall'interfaccia";
    "net.d_any" => "Any", "Qualsiasi";
    "net.d_df" => "Don't fragment", "Non frammentare";
    "net.d_df_help" => "With a payload size, tests the path MTU (1472 bytes = MTU 1500).", "Con una dimensione dati verifica l'MTU del percorso (1472 byte = MTU 1500).";
    "net.d_server" => "DNS server", "Server DNS";
    "net.d_server_help" => "Optional: ask this server instead of the system resolver.", "Facoltativo: interroga questo server invece del resolver di sistema.";
    "net.d_run" => "Run", "Esegui";
    "net.d_ok" => "Succeeded", "Riuscito";
    "net.d_failed" => "Failed", "Non riuscito";
    "net.d_exit" => "exit code", "codice di uscita";
    "net.d_output" => "Output", "Output";
    "net.rate_limited" => "Too many runs: try again in {n} s.", "Troppe esecuzioni: riprova tra {n} s.";
    "net.h_hop" => "Hop", "Salto";
    "net.h_host" => "Host", "Host";
    "net.h_loss" => "Loss", "Perdita";
    "net.h_avg" => "Avg ms", "Media ms";
    "net.h_best" => "Best ms", "Min ms";
    "net.h_worst" => "Worst ms", "Max ms";
    // Tailscale (Network › Remote access, ts.*) and its task names
    "ts.title" => "Remote access", "Accesso remoto";
    "ts.subtitle" => "Reach this NAS from anywhere through Tailscale, without opening ports", "Raggiungi questo NAS da ovunque con Tailscale, senza aprire porte";
    "ts.open_text" => "Tailscale: install it, log the NAS in to your tailnet and choose what it shares.", "Tailscale: installalo, collega il NAS alla tua tailnet e scegli cosa condivide.";
    "ts.install_title" => "Tailscale", "Tailscale";
    "ts.install_text" => "Tailscale puts this NAS on your private network (tailnet): your devices reach it from anywhere at its own address, with no port forwarding. rynas installs it from Tailscale's official package repository.", "Tailscale mette questo NAS nella tua rete privata (tailnet): i tuoi dispositivi lo raggiungono da ovunque al suo indirizzo, senza inoltro di porte. rynas lo installa dal repository ufficiale dei pacchetti di Tailscale.";
    "ts.install" => "Install Tailscale", "Installa Tailscale";
    "ts.block.unsupported_os" => "Tailscale has no package repository for this operating system.", "Tailscale non ha un repository di pacchetti per questo sistema operativo.";
    "ts.block.unknown_release" => "This system's release has no code name, so the right repository cannot be chosen.", "La versione di questo sistema non ha un nome in codice, quindi non si può scegliere il repository giusto.";
    "ts.not_running" => "Tailscale is installed but its service is not running.", "Tailscale è installato ma il suo servizio non è in esecuzione.";
    "ts.login_title" => "Log in to your tailnet", "Accedi alla tua tailnet";
    "ts.login_text" => "Log in with the browser, or paste an auth key made in the admin console. A tagged key keeps the NAS logged in without key expiry.", "Accedi con il browser, oppure incolla una chiave di autenticazione creata nella console di amministrazione. Una chiave con tag mantiene il NAS collegato senza scadenza della chiave.";
    "ts.auth_key" => "Auth key (optional)", "Chiave di autenticazione (facoltativa)";
    "ts.auth_key_help" => "It is never shown again or written to the logs.", "Non viene più mostrata né scritta nei log.";
    "ts.hostname" => "Device name", "Nome del dispositivo";
    "ts.hostname_help" => "Empty: the NAS's host name.", "Vuoto: il nome host del NAS.";
    "ts.login_server" => "Coordination server", "Server di coordinamento";
    "ts.login_server_help" => "Only for a self-hosted coordination server; leave it empty otherwise.", "Solo per un server di coordinamento ospitato in proprio; altrimenti lascialo vuoto.";
    "ts.login" => "Log in", "Accedi";
    "ts.login_url_text" => "Open this address on any device and approve this NAS:", "Apri questo indirizzo su qualsiasi dispositivo e approva questo NAS:";
    "ts.use_key" => "Use an auth key instead", "Usa invece una chiave di autenticazione";
    "ts.use_url" => "Back to the login address", "Torna all'indirizzo di accesso";
    "ts.open_login" => "Open the login page", "Apri la pagina di accesso";
    "ts.login_wait" => "Waiting for the login address…", "In attesa dell'indirizzo di accesso…";
    "ts.machine_auth" => "Logged in: an administrator of the tailnet must approve this device in the admin console.", "Accesso fatto: un amministratore della tailnet deve approvare questo dispositivo nella console di amministrazione.";
    "ts.status_title" => "Status", "Stato";
    "ts.state" => "State", "Stato del collegamento";
    "ts.st.running" => "Connected", "Connesso";
    "ts.st.stopped" => "Switched off", "Spento";
    "ts.st.needs_login" => "Logged out", "Disconnesso";
    "ts.st.needs_machine_auth" => "Waiting for approval", "In attesa di approvazione";
    "ts.st.starting" => "Starting", "Avvio in corso";
    "ts.st.no_state" => "Not set up", "Non configurato";
    "ts.st.unavailable" => "Service not running", "Servizio non in esecuzione";
    "ts.dns_name" => "Name in the tailnet", "Nome nella tailnet";
    "ts.addresses" => "Tailscale addresses", "Indirizzi Tailscale";
    "ts.tailnet" => "Tailnet", "Tailnet";
    "ts.peers" => "Other devices", "Altri dispositivi";
    "ts.peers_val" => "{online} online of {total}", "{online} in linea su {total}";
    "ts.key_expiry" => "Key expires", "Scadenza della chiave";
    "ts.key_never" => "Never (expiry disabled for this device)", "Mai (scadenza disattivata per questo dispositivo)";
    "ts.key_soon" => "The key expires on {date}. Renew it, or disable key expiry for this device in the admin console, or the NAS drops off the tailnet.", "La chiave scade il {date}. Rinnovala, oppure disattiva la scadenza per questo dispositivo nella console di amministrazione, altrimenti il NAS esce dalla tailnet.";
    "ts.version" => "Version", "Versione installata";
    "ts.update" => "{v} available with the system updates", "{v} disponibile con gli aggiornamenti di sistema";
    "ts.offline" => "Not connected to the coordination server.", "Non connesso al server di coordinamento.";
    "ts.health" => "Tailscale reports:", "Tailscale segnala:";
    "ts.options_title" => "Options", "Opzioni";
    "ts.opt.accept_routes" => "Use routes from other devices", "Usa le rotte degli altri dispositivi";
    "ts.opt.accept_routes_help" => "Off on a server: a route to this NAS's own network through the tailnet can make the local network unreachable.", "Spento su un server: una rotta verso la rete di questo NAS attraverso la tailnet può rendere irraggiungibile la rete locale.";
    "ts.opt.accept_dns" => "Use the tailnet's DNS settings", "Usa le impostazioni DNS della tailnet";
    "ts.opt.accept_dns_help" => "Off by default: the NAS keeps its local DNS servers. On: tailnet names resolve on the NAS too, and its resolver configuration changes.", "Spento di norma: il NAS tiene i suoi server DNS locali. Acceso: i nomi della tailnet si risolvono anche sul NAS e la sua configurazione DNS cambia.";
    "ts.opt.exit_node" => "Offer this NAS as an exit node", "Offri questo NAS come nodo di uscita";
    "ts.opt.exit_node_help" => "Devices of the tailnet may send all their internet traffic through this NAS, once approved in the admin console.", "I dispositivi della tailnet possono far passare tutto il loro traffico internet da questo NAS, dopo l'approvazione nella console di amministrazione.";
    "ts.opt.routes" => "Share the local network", "Condividi la rete locale";
    "ts.opt.routes_help" => "Devices of the tailnet reach these subnets through this NAS, once approved in the admin console. Everything in them becomes reachable from the tailnet: limit it with the tailnet's access rules.", "I dispositivi della tailnet raggiungono queste sottoreti attraverso questo NAS, dopo l'approvazione nella console di amministrazione. Tutto ciò che contengono diventa raggiungibile dalla tailnet: limitalo con le regole di accesso della tailnet.";
    "ts.opt.no_lan" => "No IPv4 subnet found on the network interfaces.", "Nessuna sottorete IPv4 trovata sulle interfacce di rete.";
    "ts.opt.ssh" => "Tailscale SSH", "SSH di Tailscale";
    "ts.opt.ssh_help" => "Off by default. On: the tailnet's access rules decide who may log in over SSH, not the accounts of this NAS.", "Spento di norma. Acceso: decidono le regole di accesso della tailnet chi può entrare via SSH, non gli account di questo NAS.";
    "ts.routes_pending" => "Waiting for approval in the admin console: {routes}", "In attesa di approvazione nella console di amministrazione: {routes}";
    "ts.confirm_title" => "Change the Tailscale options?", "Cambiare le opzioni di Tailscale?";
    "ts.confirm_text" => "The change is tested: if this page cannot reach the NAS again within 90 seconds, the previous options come back by themselves.", "La modifica viene provata: se questa pagina non raggiunge di nuovo il NAS entro 90 secondi, tornano da sole le opzioni precedenti.";
    "ts.via_warning" => "This page is open through Tailscale: this can end your session. Keep another way in ready (local network, emergency address).", "Questa pagina è aperta tramite Tailscale: questo può chiudere la sessione. Tieni pronto un altro accesso (rete locale, indirizzo di emergenza).";
    "ts.saved" => "Tailscale options saved", "Opzioni di Tailscale salvate";
    "ts.confirm_failed" => "The NAS could not confirm the change: the previous options come back by themselves within 90 seconds.", "Il NAS non ha potuto confermare la modifica: le opzioni precedenti tornano da sole entro 90 secondi.";
    "ts.pending_confirm" => "A change of the Tailscale options waits for confirmation: if it is not confirmed, the previous options come back within 90 seconds.", "Una modifica delle opzioni di Tailscale attende conferma: se non viene confermata, le opzioni precedenti tornano entro 90 secondi.";
    "ts.save_after_confirm" => "New changes can be saved once the pending one is confirmed or reverted.", "Si possono salvare nuove modifiche quando quella in attesa è confermata o annullata.";
    "ts.ch.routes_added" => "Share: {routes}", "Condividi: {routes}";
    "ts.ch.routes_removed" => "Stop sharing: {routes}", "Smetti di condividere: {routes}";
    "ts.keep" => "Keep the new options", "Tieni le nuove opzioni";
    "ts.logout" => "Log out", "Esci dalla tailnet";
    "ts.logout_title" => "Log this NAS out of the tailnet?", "Scollegare questo NAS dalla tailnet?";
    "ts.logout_text" => "The NAS leaves the tailnet and its key is expired. Logging in again needs the browser or a new auth key.", "Il NAS esce dalla tailnet e la sua chiave scade. Per rientrare servono il browser o una nuova chiave di autenticazione.";
    "ts.uninstall" => "Uninstall", "Disinstalla";
    "ts.uninstall_title" => "Uninstall Tailscale?", "Disinstallare Tailscale?";
    "ts.uninstall_text" => "The package, its repository and the forwarding settings rynas wrote are removed.", "Vengono rimossi il pacchetto, il suo repository e le impostazioni di inoltro scritte da rynas.";
    "ts.purge" => "Also forget this device: a new install joins the tailnet as a new device", "Dimentica anche questo dispositivo: una nuova installazione entra nella tailnet come nuovo dispositivo";
    "ts.remove_title" => "Remove Tailscale", "Rimuovi Tailscale";
    "ts.wa_on" => "Tailscale is connected ({name}): “This network and VPN” also admits the devices of the tailnet.", "Tailscale è connesso ({name}): «Questa rete e VPN» ammette anche i dispositivi della tailnet.";
    "ts.wa_off" => "Tailscale is not connected: “This network and VPN” admits no tailnet device.", "Tailscale non è connesso: «Questa rete e VPN» non ammette dispositivi della tailnet.";
    "err.tailscale_not_installed" => "Tailscale is not installed.", "Tailscale non è installato.";
    "task.tailscale.install" => "Tailscale install", "Installazione di Tailscale";
    "task.tailscale.uninstall" => "Tailscale removal", "Rimozione di Tailscale";
    "task.tailscale.login" => "Tailscale login", "Accesso a Tailscale";
    "task.tailscale.logout" => "Tailscale logout", "Uscita da Tailscale";
    "task.tailscale.options" => "Tailscale options", "Opzioni Tailscale";
    "task.tailscale.revert" => "Tailscale options restored", "Ripristino opzioni Tailscale";
    // ---- System › Updates (upd.*) and its task names
    "task.updates.check" => "Check for updates", "Controllo aggiornamenti";
    "task.updates.backports" => "Debian backports setting", "Impostazione dei backports di Debian";
    "task.updates.install" => "Install updates", "Installazione aggiornamenti";
    "task.updates.autoremove" => "Remove unused packages", "Rimozione pacchetti inutilizzati";
    "task.updates.changelog" => "Package changelog", "Changelog del pacchetto";
    "upd.title" => "Updates", "Aggiornamenti";
    "upd.subtitle" => "Debian packages, automatic updates and the boot kernel.", "Pacchetti Debian, aggiornamenti automatici e kernel di avvio.";
    "upd.open_text" => "Install Debian updates, set automatic updates and check the kernel.", "Installa gli aggiornamenti Debian, imposta quelli automatici e controlla il kernel.";
    "upd.check" => "Check now", "Controlla ora";
    "upd.never" => "Never", "Mai";
    "upd.no_apt" => "No package manager", "Nessun gestore di pacchetti";
    "upd.no_apt_text" => "apt is not installed on this machine.", "apt non è installato su questa macchina.";
    "upd.status" => "Status", "Stato";
    "upd.uptodate" => "Up to date", "Aggiornato";
    "upd.available" => "{n} updates available", "{n} aggiornamenti disponibili";
    "upd.security_n" => "{n} security", "{n} di sicurezza";
    "upd.reboot_required" => "Restart required.", "Riavvio necessario.";
    "upd.reboot_pkgs" => "Asked by: {p}.", "Richiesto da: {p}.";
    "upd.sim_error" => "apt could not compute the updates.", "apt non è riuscito a calcolare gli aggiornamenti.";
    "upd.task_running" => "An update task is running.", "È in corso un'attività di aggiornamento.";
    "upd.removals_note" => "Installing everything would remove: {p}.", "Installare tutto rimuoverebbe: {p}.";
    "upd.held" => "Kept back", "Trattenuti";
    "upd.lists_updated" => "Last checked", "Ultimo controllo";
    "upd.read_at" => "Read at", "Letto alle";
    "upd.install_all" => "Install all", "Installa tutto";
    "upd.install_security" => "Install security updates", "Installa aggiornamenti di sicurezza";
    "upd.install_selected" => "Install selected ({n})", "Installa selezionati ({n})";
    "upd.autoremove_n" => "Clean up ({n})", "Pulisci ({n})";
    "upd.packages" => "Packages", "Pacchetti";
    "upd.none" => "No updates waiting.", "Nessun aggiornamento in attesa.";
    "upd.security" => "Security", "Sicurezza";
    "upd.new_pkg" => "New", "Nuovo";
    "upd.col_select" => "Select", "Seleziona";
    "upd.select_named" => "Select {name}", "Seleziona {name}";
    "upd.col_pkg" => "Package", "Pacchetto";
    "upd.col_current" => "Installed", "Installata";
    "upd.col_new" => "New version", "Nuova versione";
    "upd.col_origin" => "Origin", "Origine";
    "upd.changelog_of" => "Changelog of {name}", "Changelog di {name}";
    "upd.confirm_all" => "Install all updates?", "Installare tutti gli aggiornamenti?";
    "upd.confirm_some" => "Install {n} updates?", "Installare {n} aggiornamenti?";
    "upd.confirm_text" => "apt runs without questions and keeps your changes to configuration files.", "apt viene eseguito senza domande e mantiene le tue modifiche ai file di configurazione.";
    "upd.confirm_text_rm" => "apt runs without questions and keeps your changes to configuration files. Some packages will be removed.", "apt viene eseguito senza domande e mantiene le tue modifiche ai file di configurazione. Alcuni pacchetti verranno rimossi.";
    "upd.will_remove" => "These packages will be removed:", "Questi pacchetti verranno rimossi:";
    "upd.install" => "Install", "Installa";
    "upd.autoremove_title" => "Remove unused packages?", "Rimuovere i pacchetti inutilizzati?";
    "upd.autoremove_text" => "apt autoremove removes packages that nothing depends on any more.", "apt autoremove rimuove i pacchetti da cui non dipende più nulla.";
    "upd.autoremove_warn" => "Check the list: it may include the previous kernel or packages the system needs to boot.", "Controlla l'elenco: può includere il kernel precedente o pacchetti che servono all'avvio del sistema.";
    "upd.remove" => "Remove", "Rimuovi";
    "upd.auto" => "Automatic updates", "Aggiornamenti automatici";
    "upd.bp_title" => "Debian backports", "Backports di Debian";
    "upd.bp" => "Use Debian backports", "Usa i backports di Debian";
    "upd.bp_help" => "Backports are newer packages rebuilt for this Debian release. With them on, nothing is installed or upgraded from them unless asked (apt priority 100). rynas uses them for NUT when a UPS needs a newer one.", "I backports sono pacchetti più recenti ricompilati per questa versione di Debian. Attivandoli non viene installato né aggiornato nulla da lì se non lo si chiede (priorità apt 100). rynas li usa per NUT quando un UPS ne richiede uno più recente.";
    "upd.bp_system" => "Configured by the system", "Configurati dal sistema";
    "upd.no_unattended" => "unattended-upgrades is not installed: automatic updates will not run.", "unattended-upgrades non è installato: gli aggiornamenti automatici non verranno eseguiti.";
    "upd.mode" => "Install automatically", "Installa automaticamente";
    "upd.mode_help" => "Package lists are refreshed daily in every mode, for the notification. Packages are never removed automatically.", "Gli elenchi dei pacchetti vengono aggiornati ogni giorno in ogni modalità, per la notifica. I pacchetti non vengono mai rimossi automaticamente.";
    "upd.mode_off" => "Off", "Disattivato";
    "upd.mode_security" => "Security only", "Solo sicurezza";
    "upd.mode_all" => "All Debian updates", "Tutti gli aggiornamenti Debian";
    "upd.start" => "Window start", "Inizio finestra";
    "upd.start_help" => "Local time.", "Ora locale.";
    "upd.window" => "Window length (minutes)", "Durata finestra (minuti)";
    "upd.window_help" => "The start is spread randomly within the window.", "L'avvio è distribuito a caso nella finestra.";
    "upd.reboot" => "Restart automatically", "Riavvia automaticamente";
    "upd.reboot_help" => "Only when an installed update requires it.", "Solo quando un aggiornamento installato lo richiede.";
    "upd.reboot_time" => "Restart at", "Riavvia alle";
    "upd.reboot_time_help" => "Local time, after the updates.", "Ora locale, dopo gli aggiornamenti.";
    "upd.pending_note" => "Saved changes take effect after you apply them.", "Le modifiche salvate hanno effetto dopo averle applicate.";
    "upd.kernel" => "Kernel and boot", "Kernel e avvio";
    "upd.refresh_kernel" => "Read the kernel and boot state again", "Rileggi lo stato del kernel e dell'avvio";
    "upd.running_kernel" => "Running kernel", "Kernel in uso";
    "upd.installed_kernels" => "Installed kernels", "Kernel installati";
    "upd.kernel_newer" => "Kernel {k} is installed but not running: restart to use it.", "Il kernel {k} è installato ma non in uso: riavvia per usarlo.";
    "upd.kernel_mismatch" => "The boot entry has kernel {k}, not the running one.", "La voce di avvio ha il kernel {k}, non quello in uso.";
    "upd.boot_outdated" => "The boot entries are outdated.", "Le voci di avvio non sono aggiornate.";
    "upd.boot_error" => "The boot state could not be read.", "Non è stato possibile leggere lo stato dell'avvio.";
    "upd.boot_default" => "Default entry", "Voce predefinita";
    "upd.boot_fallback" => "Fallback", "Riserva";
    "upd.boot_entries" => "Boot entries", "Voci di avvio";
    "upd.boot_running" => "Booted from", "Avviato da";
    "upd.boot_kernel" => "Kernel on the boot device", "Kernel sul dispositivo di avvio";
    "upd.boot_kernel_old" => "Previous kernel", "Kernel precedente";
    "upd.trial" => "Kernel trial", "Prova del kernel";
    "upd.trial_none" => "None", "Nessuna";
    "upd.trial_armed" => "Armed", "Pronta";
    "upd.trial_trying" => "Trying", "In prova";
    "upd.boot_free" => "Free space on the boot device", "Spazio libero sul dispositivo di avvio";
    // ---- Save in dialogs and editors: stage + apply only that change (config::save_apply)
    "toast.saved_applied" => "Saved and applied", "Salvato e applicato";
    "cfg.applying_now" => "Applying the change…", "Applicazione della modifica…";
    "cfg.apply_kept" => "The previous settings are back in place; the change stays in the pending changes.", "Sono tornate le impostazioni precedenti; la modifica resta tra quelle in sospeso.";
    "cfg.apply_log" => "Show the log", "Mostra il registro";
    "cfg.key.ups.safemode" => "UPS shutdown policy", "Criterio di spegnimento con l'UPS";
    "cfg.key.ups.device" => "UPS driver and connection", "Driver e collegamento dell'UPS";
    "cfg.key.updates.auto" => "Automatic updates", "Aggiornamenti automatici";
    // ---- Disk checks (SettingCheck codes) and SMART health reasons (HealthReason codes)
    "sto.chk_crit" => "Critical", "Critica";
    "sto.chk.head_parking" => "Head parking", "Parcheggio delle testine";
    "sto.cv.on" => "On", "Attiva";
    "sto.cv.off" => "Off", "Disattivata";
    "sto.cv.disabled" => "Disabled", "Disattivato";
    "sto.cv.enabled" => "Enabled", "Attivo";
    "sto.cv.unknown" => "Unknown", "Sconosciuto";
    "sto.cv.apm_254_off" => "254 or off", "254 o disattivato";
    "sto.cv.trim_timer_discard" => "fstrim.timer + discard mount option", "fstrim.timer + opzione di montaggio discard";
    "sto.cv.trim_timer" => "fstrim.timer", "fstrim.timer";
    "sto.cv.trim_discard" => "discard mount option", "opzione di montaggio discard";
    "sto.cv.trim_none" => "None", "Nessuno";
    "sto.cv.trim_weekly" => "fstrim.timer every week", "fstrim.timer ogni settimana";
    "sto.cv.apst_enabled" => "Enabled (max latency {n} µs)", "Attivo (latenza massima {n} µs)";
    "sto.cv.temp" => "{n}", "{n}";
    "sto.cv.temp_below" => "below {n}", "sotto {n}";
    "sto.cv.ra_default" => "kernel default (128-{n})", "predefinito del kernel (128-{n})";
    "sto.cv.parking_rate" => "{n} per hour", "{n} all'ora";
    "sto.cv.parking_max" => "at most {n} per hour", "al massimo {n} all'ora";
    "sto.cn.sched_nvme" => "NVMe drives have deep hardware queues: no scheduler is fastest.", "Gli NVMe hanno code hardware profonde: senza scheduler sono più veloci.";
    "sto.cn.sched_hdd" => "mq-deadline keeps throughput on RAID; bfq only helps desktop interactivity.", "mq-deadline mantiene le prestazioni in RAID; bfq aiuta solo la reattività di un desktop.";
    "sto.cn.sched_flash" => "A single queue: mq-deadline and none perform the same.", "Una sola coda: mq-deadline e none si equivalgono.";
    "sto.cn.ra_stacked" => "btrfs and md read ahead on their own device: the disk's value does not matter.", "btrfs e md fanno read-ahead sul proprio dispositivo: il valore del disco non conta.";
    "sto.cn.ra_hdd" => "The kernel's default suits spinning disks; very large values only waste cache.", "Il valore predefinito del kernel va bene per i dischi rotativi; valori molto grandi sprecano solo cache.";
    "sto.cn.ra_flash" => "Flash needs little read-ahead; more only wastes cache.", "La memoria flash richiede poco read-ahead; di più spreca solo cache.";
    "sto.cn.wcache" => "Leave it on: the filesystem's flushes keep it safe, off costs a lot of speed.", "Lasciala attiva: i flush del filesystem la rendono sicura, disattivarla costa molta velocità.";
    "sto.cn.apm" => "Levels up to 127 let the drive park its heads and spin down by itself.", "I livelli fino a 127 lasciano che il disco parcheggi le testine e si fermi da solo.";
    "sto.cn.apm_none" => "The drive has no APM: nothing to set.", "Il disco non ha APM: non c'è niente da impostare.";
    "sto.cn.parking" => "Frequent head unloads wear the drive: raise APM or the drive's idle timers.", "Parcheggi frequenti delle testine usurano il disco: alza l'APM o i timer di inattività del disco.";
    "sto.cn.trim_both" => "Async discard does the work, the weekly timer is a cheap safety net.", "Il discard asincrono fa il lavoro, il timer settimanale è una rete di sicurezza che costa poco.";
    "sto.cn.trim_timer" => "Weekly batched TRIM.", "TRIM settimanale a blocchi.";
    "sto.cn.trim_discard" => "btrfs discard=async (the default since Linux 6.2) trims continuously.", "btrfs discard=async (predefinito da Linux 6.2) esegue il TRIM di continuo.";
    "sto.cn.trim_none" => "Without TRIM the SSD slows down and wears faster: enable fstrim.timer.", "Senza TRIM l'SSD rallenta e si usura prima: attiva fstrim.timer.";
    "sto.cn.trim_unsupported" => "The device does not accept discards.", "Il dispositivo non accetta i discard.";
    "sto.cn.trim_parity_raid" => "md RAID5/6 does not pass TRIM through (devices_handle_discard_safely is off).", "md RAID5/6 non fa passare il TRIM (devices_handle_discard_safely è disattivato).";
    "sto.cn.trim_crypt" => "The encrypted volume does not pass TRIM through: leave 10-20% of a new SSD unpartitioned.", "Il volume cifrato non fa passare il TRIM: lascia non partizionato il 10-20% di un SSD nuovo.";
    "sto.cn.alpm" => "Saves about 1 W per SSD with no measurable cost.", "Fa risparmiare circa 1 W per SSD senza costi misurabili.";
    "sto.cn.apst" => "Disable it only on models with known APST firmware bugs.", "Disattivalo solo sui modelli con bug noti del firmware APST.";
    "sto.cn.temp_hdd" => "Failures rise with heat: keep hard disks below 50 °C, never above 60 °C.", "I guasti aumentano col calore: tieni i dischi rigidi sotto i 50 °C, mai sopra i 60 °C.";
    "sto.cn.temp_nvme" => "The drive slows itself down above its warning temperature.", "Il disco rallenta da solo sopra la sua temperatura di avviso.";
    "sto.cn.temp_other" => "Keep air flowing over the drive.", "Mantieni un flusso d'aria sul disco.";
    "sto.reason.smart_failed" => "The drive reports its SMART overall health as FAILED.", "Il disco segnala lo stato SMART complessivo FAILED.";
    "sto.reason.attribute_failing" => "{name} is below its failure threshold.", "{name} è sotto la soglia di guasto.";
    "sto.reason.attribute_failed_past" => "{name} was below its failure threshold in the past.", "{name} è stato sotto la soglia di guasto in passato.";
    "sto.reason.reallocated_sectors" => "{n} reallocated sectors.", "{n} settori riallocati.";
    "sto.reason.pending_sectors" => "{n} sectors waiting for reallocation (unreadable).", "{n} settori in attesa di riallocazione (illeggibili).";
    "sto.reason.offline_uncorrectable" => "{n} offline uncorrectable sectors.", "{n} settori non correggibili offline.";
    "sto.reason.reported_uncorrect" => "{n} errors that ECC could not correct.", "{n} errori che l'ECC non ha potuto correggere.";
    "sto.reason.command_timeouts" => "{n} command timeouts (often power or cable).", "{n} timeout dei comandi (spesso alimentazione o cavo).";
    "sto.reason.crc_errors" => "{n} interface CRC errors: check the cable or the bay connector.", "{n} errori CRC dell'interfaccia: controlla il cavo o il connettore dell'alloggiamento.";
    "sto.reason.load_cycles" => "{n} head load cycles (rated 300,000-600,000): aggressive head parking.", "{n} cicli di carico delle testine (nominali 300.000-600.000): parcheggio aggressivo.";
    "sto.reason.old_disk" => "{n} years powered on.", "{n} anni di accensione.";
    "sto.reason.temperature" => "Temperature {n}.", "Temperatura {n}.";
    "sto.reason.nvme_spare_low" => "Available spare {n} (threshold {limit}).", "Riserva disponibile {n} (soglia {limit}).";
    "sto.reason.nvme_temperature" => "Temperature out of range.", "Temperatura fuori intervallo.";
    "sto.reason.nvme_reliability" => "Reliability degraded by media or internal errors.", "Affidabilità ridotta da errori del supporto o interni.";
    "sto.reason.nvme_read_only" => "The media is read-only.", "Il supporto è in sola lettura.";
    "sto.reason.nvme_backup_failed" => "The volatile memory backup device failed.", "Il dispositivo di backup della memoria volatile è guasto.";
    "sto.reason.media_errors" => "{n} media and data integrity errors.", "{n} errori del supporto e di integrità dei dati.";
    "sto.reason.wear_out" => "Rated endurance used up (100% or more).", "Resistenza nominale esaurita (100% o più).";
    "sto.reason.wear" => "{n} of the rated endurance left.", "Resta il {n} della resistenza nominale.";
    "sto.reason.self_test_failed" => "Last self-test ({name}): {detail}.", "Ultimo autotest ({name}): {detail}.";
    "sto.reason.error_log" => "{n} errors in the drive's error log.", "{n} errori nel registro errori del disco.";
    // SMART counters marked as seen (Storage › Drives › drive)
    "sto.ack" => "Mark as seen", "Segna come visto";
    "sto.ack_undo" => "Undo", "Annulla";
    "sto.ack_seen" => "seen {d}", "visto il {d}";
    "sto.ack_hint" => "The warning comes back if the count grows.", "L'avviso torna se il conteggio aumenta.";
    "sto.ack_done" => "Warnings marked as seen", "Avvisi segnati come visti";
    "sto.ack_undone" => "Warnings count again", "Gli avvisi contano di nuovo";
    "sto.ack_grew" => "Some counts grew since this page was loaded: those still warn", "Alcuni conteggi sono aumentati da quando hai aperto la pagina: quelli restano in avviso";
    // removable drives (app bar menu, Hardware › Removable drives)
    "rm.title" => "Removable drives", "Unità rimovibili";
    "rm.count_one" => "1 drive", "1 unità";
    "rm.count" => "{n} drives", "{n} unità";
    "rm.mounted_one" => "1 mounted", "1 montata";
    "rm.mounted" => "{n} mounted", "{n} montate";
    "rm.empty" => "No removable drives", "Nessuna unità rimovibile";
    "rm.empty_text" => "Plug in a USB drive or an SD card.", "Collega un'unità USB o una scheda SD.";
    "rm.mount_all" => "Mount all", "Monta tutto";
    "rm.remove_all" => "Remove all safely", "Rimuovi tutto in sicurezza";
    "rm.refresh" => "Read the drives again", "Rileggi le unità";
    "rm.settings" => "Removable drive settings", "Impostazioni delle unità rimovibili";
    "rm.drives_now" => "Plugged in now", "Collegate ora";
    "rm.remove" => "Remove safely", "Rimuovi in sicurezza";
    "rm.hint.power_off" => "Unmounts everything, locks encrypted volumes and powers the drive off", "Smonta tutto, blocca i volumi cifrati e spegne l'unità";
    "rm.hint.eject" => "Unmounts everything, locks encrypted volumes and ejects the medium", "Smonta tutto, blocca i volumi cifrati ed espelle il supporto";
    "rm.hint.release" => "Unmounts everything and locks encrypted volumes: then the card can be pulled out", "Smonta tutto e blocca i volumi cifrati: poi la scheda si può estrarre";
    "rm.bus.sd" => "SD card", "Scheda SD";
    "rm.bus.optical" => "Optical drive", "Unità ottica";
    "rm.bus.other" => "Removable drive", "Unità rimovibile";
    "rm.no_media" => "No medium", "Nessun supporto";
    "rm.read_only" => "read-only", "sola lettura";
    "rm.writing" => "Writing…", "Scrittura…";
    "rm.pending.mount" => "Mounting…", "Montaggio…";
    "rm.pending.unmount" => "Unmounting…", "Smontaggio…";
    "rm.pending.unlock" => "Unlocking…", "Sblocco…";
    "rm.pending.lock" => "Locking…", "Blocco…";
    "rm.pending.remove" => "Preparing for removal…", "Preparazione alla rimozione…";
    "rm.untitled" => "Untitled", "Senza nome";
    "rm.locked" => "Locked", "Bloccato";
    "rm.unlocked" => "Unlocked", "Sbloccato";
    "rm.encrypted" => "Encrypted: unlock it with its passphrase", "Cifrato: sbloccalo con la sua passphrase";
    "rm.not_mounted" => "Not mounted", "Non montato";
    "rm.unsupported" => "No driver for this filesystem is installed", "Nessun driver installato per questo file system";
    "rm.free" => "{free} free of {total}", "{free} liberi su {total}";
    "rm.open" => "Open in Files", "Apri in File";
    "rm.copy" => "Copy the mount path", "Copia il percorso di montaggio";
    "rm.copied" => "Path copied", "Percorso copiato";
    "rm.mount" => "Mount", "Monta";
    "rm.unmount" => "Unmount", "Smonta";
    "rm.unlock" => "Unlock", "Sblocca";
    "rm.lock" => "Lock", "Blocca";
    "rm.shared" => "Shared on the network as {name}", "Condiviso in rete come {name}";
    "rm.safe" => "{name} can be removed safely", "{name} si può rimuovere in sicurezza";
    "rm.safe_all" => "Every drive can be removed safely", "Tutte le unità si possono rimuovere in sicurezza";
    "rm.busy_by" => "In use by: {list}. Close the files and windows that use it, then try again.", "In uso da: {list}. Chiudi i file e le finestre che la usano, poi riprova.";
    "rm.tip_mounted" => "mounted at {path}", "montato in {path}";
    "rm.tip_not_mounted" => "not mounted", "non montato";
    "rm.set.where" => "Volumes are mounted below {root}, in a folder named after their label.", "I volumi vengono montati sotto {root}, in una cartella con il nome della loro etichetta.";
    "rm.set.auto_mount" => "Mount automatically", "Monta automaticamente";
    "rm.set.auto_mount_help" => "Mount the volumes of a drive as soon as it is plugged in, and at startup. Encrypted volumes still wait for their passphrase.", "Monta i volumi di un'unità appena viene collegata, e all'avvio. I volumi cifrati attendono comunque la loro passphrase.";
    "rm.set.read_only" => "Mount read-only", "Monta in sola lettura";
    "rm.set.read_only_help" => "Nothing on a plugged-in drive can be changed (safe for imports).", "Niente su un'unità collegata può essere modificato (sicuro per le importazioni).";
    "rm.set.notify" => "Notify when a drive can be removed", "Notifica quando un'unità si può rimuovere";
    "rm.set.notify_help" => "A drive unplugged while it was still mounted is always notified.", "Un'unità scollegata mentre era ancora montata viene sempre notificata.";
    "rm.set.group" => "Access group", "Gruppo con accesso";
    "rm.set.group_help" => "Its members may use the network shares and the files of FAT, exFAT and NTFS drives (rynas-admin: administrators; users: every account).", "I suoi membri possono usare le condivisioni di rete e i file delle unità FAT, exFAT e NTFS (rynas-admin: amministratori; users: tutti gli account).";
    "rm.set.share" => "Share mounted drives on the network (SMB)", "Condividi in rete le unità montate (SMB)";
    "rm.set.share_help" => "Each mounted volume becomes a private share named usb-<name>, only for the access group. It is removed, and its clients disconnected, before the volume is unmounted.", "Ogni volume montato diventa una condivisione privata chiamata usb-<nome>, solo per il gruppo con accesso. Viene rimossa, e i suoi client disconnessi, prima di smontare il volume.";
    "rm.set.access" => "Network access", "Accesso dalla rete";
    "rm.set.rw" => "Read and write", "Lettura e scrittura";
    "rm.set.no_smb" => "Samba is not installed: drives cannot be shared on the network.", "Samba non è installato: le unità non si possono condividere in rete.";
    "task.removable.mount" => "Mount removable volume", "Montaggio volume rimovibile";
    "task.removable.mount-all" => "Mount removable volumes", "Montaggio volumi rimovibili";
    "task.removable.unmount" => "Unmount removable volume", "Smontaggio volume rimovibile";
    "task.removable.unlock" => "Unlock removable volume", "Sblocco volume rimovibile";
    "task.removable.lock" => "Lock removable volume", "Blocco volume rimovibile";
    "task.removable.remove" => "Remove drive safely", "Rimozione sicura dell'unità";
    "task.removable.remove-all" => "Remove all drives safely", "Rimozione sicura di tutte le unità";
    "cfg.key.removable" => "Removable drives", "Unità rimovibili";
    "nt.src.removable" => "Removable drive monitor", "Monitoraggio unità rimovibili";
    "nev.removable.unplugged.title" => "{drive} was unplugged while mounted", "{drive} è stata scollegata mentre era montata";
    "nev.removable.unplugged.body" => "Files being written to {mount_points} may be lost or damaged. Remove drives safely before unplugging them.", "I file in scrittura su {mount_points} potrebbero essere persi o danneggiati. Rimuovi le unità in sicurezza prima di scollegarle.";
    "nev.removable.safe_to_remove.title" => "{drive} can be removed safely", "{drive} si può rimuovere in sicurezza";
    "nev.removable.ntfs_read_only.title" => "{volume} mounted read-only", "{volume} montato in sola lettura";
    "nev.removable.ntfs_read_only.body" => "Windows did not close this NTFS volume cleanly (Fast Startup, hibernation or an unsafe unplug), so it was mounted read-only. Shut Windows down completely or run chkdsk, then plug it in again to write to it.", "Windows non ha chiuso correttamente questo volume NTFS (avvio rapido, ibernazione o una rimozione non sicura), quindi è stato montato in sola lettura. Spegni Windows completamente o esegui chkdsk, poi ricollegalo per poterci scrivere.";
    "task.docker.install" => "Install Docker", "Installazione di Docker";
    "task.docker.migrate" => "Move Docker to a volume", "Spostamento di Docker su un volume";
    "task.docker.container.start" => "Start container", "Avvio del container";
    "task.docker.container.stop" => "Stop container", "Arresto del container";
    "task.docker.container.restart" => "Restart container", "Riavvio del container";
    "task.docker.stack.up" => "Start stack", "Avvio dello stack";
    "task.docker.stack.start" => "Start stack containers", "Avvio dei container dello stack";
    "task.docker.stack.stop" => "Stop stack", "Arresto dello stack";
    "task.docker.stack.restart" => "Restart stack", "Riavvio dello stack";
    "task.docker.stack.down" => "Take stack down", "Rimozione dei container dello stack";
    "task.docker.stack.pull" => "Download stack images", "Scaricamento delle immagini dello stack";
    "task.docker.stack.update" => "Update stack", "Aggiornamento dello stack";
    "task.docker.stack.rollback" => "Roll stack back", "Ripristino dello stack";
    "task.docker.stack.remove" => "Remove stack", "Rimozione dello stack";
    "task.docker.update-check" => "Check image updates", "Controllo aggiornamenti delle immagini";
    "task.docker.auto-update" => "Automatic stack update", "Aggiornamento automatico dello stack";
    "nev.docker.unhealthy.title" => "Container {container} is unhealthy", "Il container {container} non è in salute";
    "nev.docker.unhealthy.body" => "Its health check fails. Look at its logs on the Docker page.", "Il suo controllo di salute fallisce. Guarda i suoi log nella pagina Docker.";
    "nev.docker.recovered.title" => "Container {container} is healthy again", "Il container {container} è di nuovo in salute";
    "nev.docker.oom.title" => "Container {container} ran out of memory", "Il container {container} ha esaurito la memoria";
    "nev.docker.oom.body" => "The kernel stopped a process of the container because it used up its memory.", "Il kernel ha fermato un processo del container perché ha esaurito la sua memoria.";
    "nev.docker.crash_loop.title" => "Container {container} keeps restarting", "Il container {container} continua a riavviarsi";
    "nev.docker.crash_loop.body" => "{restarts} restarts in {minutes} minutes. Look at its logs on the Docker page.", "{restarts} riavvii in {minutes} minuti. Guarda i suoi log nella pagina Docker.";
    "nev.docker.crash_loop_over.title" => "Container {container} stopped restarting", "Il container {container} ha smesso di riavviarsi";
    "nev.docker.disk_full.title" => "Docker storage {used_pct}% full", "Spazio di Docker pieno al {used_pct}%";
    "nev.docker.disk_full.body" => "The filesystem of {path} is almost full: images and containers may stop working. Remove unused images or move Docker to a larger volume.", "Il filesystem di {path} è quasi pieno: immagini e container potrebbero smettere di funzionare. Rimuovi le immagini inutilizzate o sposta Docker su un volume più grande.";
    "nev.docker.disk_ok.title" => "Docker storage back under {limit_pct}%", "Spazio di Docker di nuovo sotto il {limit_pct}%";
    "nev.docker.update_available.title" => "Image update available: {image}", "Aggiornamento disponibile per l'immagine {image}";
    "nev.docker.update_available.body" => "Used by {stacks}. Update from the Docker page (a snapshot is taken first).", "Usata da {stacks}. Aggiorna dalla pagina Docker (prima viene fatto uno snapshot).";
    "nev.docker.update_available.body_2" => "A newer image is in the registry. Update from the Docker page.", "Nel registro c'è un'immagine più recente. Aggiorna dalla pagina Docker.";
    "nev.docker.update_done.title" => "Image {image} is up to date", "L'immagine {image} è aggiornata";
    "nev.docker.auto_updated.title" => "Stack {stack} updated automatically", "Stack {stack} aggiornato automaticamente";
    "nev.docker.auto_updated.body" => "New version: {change}.", "Nuova versione: {change}.";
    "nev.docker.auto_updated.body_2" => "The new images run and the stack is healthy.", "Le nuove immagini sono in esecuzione e lo stack è in salute.";
    "nev.docker.auto_rolled_back.title" => "Automatic update of {stack} rolled back", "Aggiornamento automatico di {stack} annullato";
    "nev.docker.auto_rolled_back.body" => "The new version failed: {reason}. The stack runs the previous version again.", "La nuova versione non ha funzionato: {reason}. Lo stack è tornato alla versione precedente.";
    "nev.docker.auto_held.title" => "Update of {stack} needs you", "L'aggiornamento di {stack} richiede il tuo intervento";
    "nev.docker.auto_held.body" => "{reason}. Update it by hand from the Docker page.", "{reason}. Aggiornalo a mano dalla pagina Docker.";
    "nev.docker.auto_failed.title" => "Automatic update of {stack} failed", "Aggiornamento automatico di {stack} fallito";
    "nev.docker.auto_failed.body" => "{reason}. Look at the task log on the Docker page.", "{reason}. Guarda il log dell'attività nella pagina Docker.";
    // signed online app catalogue
    "nev.apps.catalog_updates.title" => "App updates available: {apps}", "Aggiornamenti disponibili per le app: {apps}";
    "nev.apps.catalog_updates.body" => "The app catalogue was updated. Review and update the apps on the Apps & Plugins page (a snapshot is taken first).", "Il catalogo delle app è stato aggiornato. Controlla e aggiorna le app nella pagina App e plugin (prima viene fatto uno snapshot).";
    "nev.apps.catalog_refused.title" => "Online app catalogue refused", "Catalogo online delle app rifiutato";
    "nev.apps.catalog_refused.body" => "rynas keeps the catalogue it has. The downloaded catalogue failed verification: {reason}", "rynas tiene il catalogo che ha. Il catalogo scaricato non ha superato la verifica: {reason}";
    "nev.apps.catalog_ok.title" => "Online app catalogue verified again", "Catalogo online delle app di nuovo verificato";
    // Docker page, widget and module card (wave D2)
    "nav.docker" => "Docker", "Docker";
    "w.docker" => "Docker", "Docker";
    "cfg.key.docker" => "Docker engine and stacks", "Motore Docker e stack";
    "nt.src.docker" => "Docker monitor", "Monitoraggio Docker";
    "on" => "On", "Attivato";
    "dk.title" => "Docker", "Docker";
    "dk.subtitle" => "Containers, compose stacks, images and volumes", "Container, stack compose, immagini e volumi";
    "dk.tab.overview" => "Overview", "Panoramica";
    "dk.tab.containers" => "Containers", "Container";
    "dk.tab.stacks" => "Stacks", "Stack";
    "dk.tab.images" => "Images & volumes", "Immagini e volumi";
    "dk.module" => "Docker — containers and compose stacks", "Docker — container e stack compose";
    "dk.module_text" => "Runs apps in containers. rynas installs Docker Engine from Docker's own repository (or adopts the one already installed), keeps its data on a volume and gives every compose stack its own folder, with a snapshot before each update.", "Esegue app in container. rynas installa Docker Engine dal repository di Docker (o adotta quello già installato), tiene i suoi dati su un volume e dà a ogni stack compose una cartella propria, con uno snapshot prima di ogni aggiornamento.";
    "dk.installed" => "Installed", "Installato";
    "dk.inst.volume" => "Volume for Docker", "Volume per Docker";
    "dk.inst.volume_help" => "Images, containers and stacks go to <volume>/docker. You can move them later.", "Immagini, container e stack vanno in <volume>/docker. Puoi spostarli in seguito.";
    "dk.inst.system_disk" => "Docker's default folder (choose a volume later)", "Cartella predefinita di Docker (scegli un volume più tardi)";
    "dk.inst.step_repo" => "Adds Docker's apt repository, pinned to the current major version", "Aggiunge il repository apt di Docker, fissato alla versione principale attuale";
    "dk.inst.step_engine" => "Installs Docker Engine, the compose and buildx plugins", "Installa Docker Engine e i plugin compose e buildx";
    "dk.inst.step_user" => "Creates the docker-apps user that owns the app folders", "Crea l'utente docker-apps proprietario delle cartelle delle app";
    "dk.inst.go" => "Install Docker", "Installa Docker";
    "dk.block.installed" => "Docker is already installed: rynas adopts it.", "Docker è già installato: rynas lo adotta.";
    "dk.block.not_debian" => "Docker can be installed from here only on Debian.", "Da qui Docker si può installare solo su Debian.";
    "dk.block.unsupported_release" => "This Debian release is not supported by Docker's repository.", "Questa versione di Debian non è supportata dal repository di Docker.";
    "dk.block.unsupported_arch" => "This processor architecture is not supported by Docker's repository.", "Questa architettura del processore non è supportata dal repository di Docker.";
    "dk.task_running" => "Docker task in progress", "Operazione Docker in corso";
    "dk.done" => "{task}: done", "{task}: completato";
    "dk.open_web" => "Open", "Apri";
    "dk.engine" => "Engine", "Motore";
    "dk.eng.running" => "Running", "Attivo";
    "dk.eng.down" => "Not answering", "Non risponde";
    "dk.eng.version" => "Version", "Versione";
    "dk.eng.version_api" => "{v} (API {api}, oldest client API {min})", "{v} (API {api}, API minima dei client {min})";
    "dk.eng.source" => "Installed from", "Installato da";
    "dk.src.docker_ce" => "Docker's repository (docker-ce)", "Repository di Docker (docker-ce)";
    "dk.src.docker_io" => "Debian (docker.io)", "Debian (docker.io)";
    "dk.src.other" => "Other source", "Altra origine";
    "dk.eng.compose" => "Compose plugin", "Plugin compose";
    "dk.eng.buildx" => "Buildx plugin", "Plugin buildx";
    "dk.eng.storage" => "Storage driver", "Driver di archiviazione";
    "dk.eng.containerd_store" => "containerd image store", "archivio immagini containerd";
    "dk.eng.data_root" => "Data folder", "Cartella dei dati";
    "dk.eng.cgroup" => "Cgroup driver", "Driver cgroup";
    "dk.eng.logging" => "Log driver", "Driver dei log";
    "dk.eng.live_restore" => "Containers survive engine restarts", "I container sopravvivono al riavvio del motore";
    "dk.eng.counts" => "Contents", "Contenuto";
    "dk.eng.counts_v" => "{n} containers ({run} running, {stop} stopped) · {img} images", "{n} container ({run} in esecuzione, {stop} fermi) · {img} immagini";
    "dk.warn.daemon_down" => "Docker is installed but its service does not answer. Start docker.service, or look at its journal.", "Docker è installato ma il suo servizio non risponde. Avvia docker.service o guarda il suo journal.";
    "dk.warn.compose_missing" => "The docker compose plugin is missing: stacks cannot be started or checked.", "Manca il plugin docker compose: gli stack non si possono avviare né controllare.";
    "dk.warn.data_root_on_system" => "Docker keeps its data in its default folder, outside the volumes. Choose a volume in the settings below (the system volume too), or move the data.", "Docker tiene i suoi dati nella sua cartella predefinita, fuori dai volumi. Scegli un volume nelle impostazioni qui sotto (anche quello di sistema) o sposta i dati.";
    "dk.warn.data_root_almost_full" => "The filesystem of Docker's data is almost full.", "Il filesystem dei dati di Docker è quasi pieno.";
    "dk.warn.not_pinned" => "Docker's packages are not pinned to their major version: a system update could bring a new major release.", "I pacchetti di Docker non sono fissati alla loro versione principale: un aggiornamento di sistema potrebbe portare una nuova versione principale.";
    "dk.warn.engine_too_old" => "This Docker Engine is too old for current apps and managers (API older than 1.44). Update it.", "Questo Docker Engine è troppo vecchio per le app e i gestori attuali (API precedente alla 1.44). Aggiornalo.";
    "dk.disk" => "Disk usage", "Spazio su disco";
    "dk.disk.fs" => "Filesystem of the data folder", "Filesystem della cartella dei dati";
    "dk.disk.used_of" => "{used} of {total} · {free} free", "{used} di {total} · {free} liberi";
    "dk.disk.images" => "Images", "Immagini";
    "dk.disk.reclaimable" => "Images not in use", "Immagini non usate";
    "dk.disk.containers" => "Container layers", "Livelli dei container";
    "dk.disk.volumes" => "Named volumes", "Volumi con nome";
    "dk.disk.build_cache" => "Build cache", "Cache di build";
    "dk.managers" => "Docker managers", "Gestori Docker";
    "dk.manager_root" => "A Docker manager has root privileges on this system and its own login: protect it with a strong password and do not expose it to the internet.", "Un gestore Docker ha i privilegi di root su questo sistema e un proprio accesso: proteggilo con una password robusta e non esporlo su internet.";
    "dk.col.manager" => "Manager", "Gestore";
    "dk.set.title" => "Engine settings", "Impostazioni del motore";
    "dk.set.text" => "Saving applies the change right away; a new volume restarts Docker (only while no container runs).", "Il salvataggio applica subito la modifica; un nuovo volume riavvia Docker (solo se nessun container è in esecuzione).";
    "dk.set.volume" => "Volume", "Volume";
    "dk.set.volume_help" => "Holds the engine data, containerd and the stacks (<volume>/docker).", "Contiene i dati del motore, containerd e gli stack (<volume>/docker).";
    "dk.set.volume_none" => "None (rynas leaves the engine configuration alone)", "Nessuno (rynas non tocca la configurazione del motore)";
    "dk.set.manager" => "Preferred manager", "Gestore preferito";
    "dk.set.manager_help" => "The manager the Apps & Plugins page offers first.", "Il gestore che la pagina App e plugin propone per primo.";
    "apps.proxy_err" => "The web access state could not be read ({err}): apps are offered on the network only.", "Non è stato possibile leggere lo stato dell'accesso web ({err}): le app vengono proposte solo sulla rete.";
    "dk.set.mgr_none" => "None", "Nessuno";
    "dk.set.mgr_arcane" => "Arcane", "Arcane";
    "dk.set.mgr_dockhand" => "Dockhand", "Dockhand";
    "dk.set.mgr_portainer" => "Portainer CE", "Portainer CE";
    "dk.set.mgr_dockge" => "Dockge", "Dockge";
    "dk.set.mgr_komodo" => "Komodo", "Komodo";
    "dk.set.pool" => "Container networks", "Reti dei container";
    "dk.set.pool_help" => "Address range for the networks of the stacks, used only when Docker has none of its own.", "Intervallo di indirizzi per le reti degli stack, usato solo se Docker non ne ha già uno.";
    "dk.set.pool_invalid" => "Enter a network such as 10.200.0.0/16.", "Inserisci una rete come 10.200.0.0/16.";
    "dk.set.pool_size" => "Network size (prefix)", "Dimensione delle reti (prefisso)";
    "dk.set.pool_size_help" => "24 gives each network 254 addresses.", "24 dà a ogni rete 254 indirizzi.";
    "dk.set.snapshot" => "Snapshot before updates", "Snapshot prima degli aggiornamenti";
    "dk.set.snapshot_help" => "A read-only snapshot of the stack folder, for a rollback.", "Uno snapshot in sola lettura della cartella dello stack, per un ripristino.";
    "dk.set.stop" => "Stop the stack for the snapshot", "Ferma lo stack per lo snapshot";
    "dk.set.stop_help" => "Recommended for databases: the files are consistent while nothing writes.", "Consigliato per i database: i file sono coerenti mentre nessuno scrive.";
    "dk.set.online_catalog" => "Download app catalogue updates", "Scarica gli aggiornamenti del catalogo delle app";
    "dk.set.files_app_data" => "Show app data in Files", "Mostra i dati delle app nell'app File";
    "dk.set.files_app_data_help" => "Administrators see a folder per app and can edit its files; owners stay as the app expects. A wrong change can break an app.", "Gli amministratori vedono una cartella per app e possono modificarne i file; i proprietari restano quelli che l'app si aspetta. Una modifica sbagliata può rompere un'app.";
    "dk.set.online_catalog_help" => "Once a day rynas fetches the signed app catalogue, so new apps and app versions arrive without waiting for a rynas update. A catalogue that fails verification is never used.", "Una volta al giorno rynas scarica il catalogo firmato delle app, così nuove app e nuove versioni arrivano senza aspettare un aggiornamento di rynas. Un catalogo che non supera la verifica non viene mai usato.";
    "dk.set.keep" => "Snapshots kept per stack", "Snapshot conservati per stack";
    "dk.set.full" => "Disk full warning (%)", "Avviso di disco pieno (%)";
    "dk.set.full_help" => "A notification when the data folder's filesystem is this full.", "Una notifica quando il filesystem della cartella dei dati è pieno a questo livello.";
    "dk.set.restarts" => "Restarts that make a crash loop", "Riavvii che indicano un ciclo di crash";
    "dk.set.restarts_help" => "A notification when a container restarts this often within the window below.", "Una notifica quando un container si riavvia così spesso nell'intervallo qui sotto.";
    "dk.set.window" => "Crash loop window (minutes)", "Intervallo del ciclo di crash (minuti)";
    "dk.plan.title" => "Move Docker to the volume", "Sposta Docker sul volume";
    "dk.plan.text" => "Applying the new volume changes these files and runs these commands.", "Applicare il nuovo volume modifica questi file ed esegue questi comandi.";
    "dk.plan.files" => "Files", "File";
    "dk.plan.create" => "Folders created", "Cartelle create";
    "dk.plan.commands" => "Commands", "Comandi";
    "dk.plan.restart" => "Docker and containerd restart. Existing images and containers stay in the old folder: use Move data to bring them along.", "Docker e containerd si riavviano. Le immagini e i container esistenti restano nella vecchia cartella: usa Sposta i dati per portarli con sé.";
    "dk.plan.go" => "Save and apply", "Salva e applica";
    "dk.mig.title" => "Move data to a volume", "Sposta i dati su un volume";
    "dk.mig.text" => "Copies images, containers and containerd's data to <volume>/docker and starts Docker there. The old folders are kept until you delete them.", "Copia immagini, container e dati di containerd in <volume>/docker e avvia Docker lì. Le vecchie cartelle restano finché non le elimini.";
    "dk.mig.volume" => "Destination volume", "Volume di destinazione";
    "dk.mig.running" => "{n} containers are running: stop them first, the move needs Docker stopped.", "{n} container sono in esecuzione: fermali prima, lo spostamento richiede Docker fermo.";
    "dk.mig.go" => "Move data", "Sposta i dati";
    "dk.mig.confirm" => "Docker stops during the copy and starts again on {vol}.", "Docker si ferma durante la copia e riparte su {vol}.";
    "dk.vol_free" => "{free} free", "{free} liberi";
    // the volumes something can be put on, the system volume included (D-ROOTVOL)
    "place.system" => "System", "Sistema";
    "place.system_note" => "The system volume is the folder {path} on the operating system's disk. It shares that disk's space: if it fills up, the system can stop working, and a failure of that disk takes this data with it.", "Il volume di sistema è la cartella {path} sul disco del sistema operativo. Ne condivide lo spazio: se si riempie, il sistema può smettere di funzionare, e un guasto di quel disco si porta via anche questi dati.";
    "place.not_btrfs" => "The system disk is not btrfs: a plain folder is made, without subvolume, snapshots or NOCOW.", "Il disco di sistema non è btrfs: viene creata una cartella normale, senza sottovolume, snapshot o NOCOW.";
    "place.unavailable" => "{name} cannot be used now: {why}", "{name} non si può usare ora: {why}";
    "place.unavailable_short" => "not available", "non disponibile";
    "place.low_space" => "Little free space on {name}: {free} left.", "Poco spazio libero su {name}: restano {free}.";
    "dk.col.name" => "Name", "Nome";
    "dk.col.image" => "Image", "Immagine";
    "dk.col.images" => "Images", "Immagini";
    "dk.col.ports" => "Ports", "Porte";
    "dk.col.cpu" => "CPU", "CPU";
    "dk.col.mem" => "Memory", "Memoria";
    "dk.col.stack" => "Stack", "Stack";
    "dk.col.running" => "Running", "In esecuzione";
    "dk.col.created" => "Created", "Creato";
    "dk.col.size" => "Size", "Dimensione";
    "dk.col.volume" => "Volume", "Volume";
    "dk.c.search" => "Search containers", "Cerca container";
    "dk.c.none" => "No containers", "Nessun container";
    "dk.c.none_text" => "Containers of every stack and manager show up here.", "Qui compaiono i container di ogni stack e gestore.";
    "dk.c.no_match" => "No container matches the filters.", "Nessun container corrisponde ai filtri.";
    "dk.f.label" => "Show", "Mostra";
    "dk.f.all" => "All", "Tutti";
    "dk.f.running" => "Running", "In esecuzione";
    "dk.f.stopped" => "Stopped", "Fermi";
    "dk.f.unhealthy" => "Unhealthy", "Non in salute";
    "dk.st.running" => "Running", "In esecuzione";
    "dk.st.restarting" => "Restarting", "In riavvio";
    "dk.st.paused" => "Paused", "In pausa";
    "dk.st.created" => "Created", "Creato";
    "dk.st.exited" => "Stopped", "Fermo";
    "dk.st.dead" => "Dead", "Bloccato";
    "dk.st.stopping" => "Stopping", "In arresto";
    "dk.st.unknown" => "Unknown", "Sconosciuto";
    "dk.h.healthy" => "Healthy", "In salute";
    "dk.h.starting" => "Starting", "In avvio";
    "dk.h.unhealthy" => "Unhealthy", "Non in salute";
    "dk.a.start" => "Start", "Avvia";
    "dk.a.stop" => "Stop", "Ferma";
    "dk.a.restart" => "Restart", "Riavvia";
    "dk.a.logs" => "Logs", "Log";
    "dk.a.details" => "Details", "Dettagli";
    "dk.a.up" => "Up", "Avvia (up)";
    "dk.a.pull" => "Pull images", "Scarica le immagini";
    "dk.a.update" => "Update…", "Aggiorna…";
    "dk.a.down" => "Down", "Rimuovi i container (down)";
    "dk.a.edit" => "Edit files", "Modifica i file";
    "dk.a.remove" => "Remove…", "Rimuovi…";
    "dk.rm.go" => "Remove", "Rimuovi";
    "dk.log.title" => "Logs", "Log";
    "dk.log.follow" => "Follow", "Segui";
    "dk.log.stamps" => "Timestamps", "Orari";
    "dk.log.following" => "Live", "In diretta";
    "dk.log.ended" => "The container stopped logging", "Il container ha smesso di scrivere log";
    "dk.log.paused" => "Paused", "In pausa";
    "dk.log.lost" => "Connection lost", "Connessione persa";
    "dk.log.download" => "Download", "Scarica";
    "dk.upd.available" => "Update available", "Aggiornamento disponibile";
    "dk.upd.check" => "Check for updates", "Controlla aggiornamenti";
    "dk.upd.never" => "Images never checked", "Immagini mai controllate";
    "dk.upd.checked" => "Checked {when}", "Controllate {when}";
    "dk.upd.stack" => "Newer images in the registry: {images}", "Immagini più recenti nel registro: {images}";
    "dk.upd.title" => "Update {name}", "Aggiorna {name}";
    "dk.upd.text" => "Downloads the newer images and recreates the containers. If the update fails, the stack goes back to the snapshot.", "Scarica le immagini più recenti e ricrea i container. Se l'aggiornamento fallisce, lo stack torna allo snapshot.";
    "dk.upd.snapshot" => "Take a snapshot first", "Fai prima uno snapshot";
    "dk.upd.snapshot_help" => "Needed for the rollback.", "Serve per il ripristino.";
    "dk.upd.stop" => "Stop the stack during the snapshot", "Ferma lo stack durante lo snapshot";
    "dk.upd.next" => "next check {when}", "prossimo controllo {when}";
    "dk.au.title" => "Automatic image updates", "Aggiornamenti automatici delle immagini";
    "dk.au.text" => "Images are checked every day. Stacks that take automatic updates are updated one at a time inside this daily window: snapshot first, health check after, rollback when the new version fails. Turn them on per stack in Stacks, in the stack's details.", "Le immagini vengono controllate ogni giorno. Gli stack con gli aggiornamenti automatici vengono aggiornati uno alla volta dentro questa finestra giornaliera: prima lo snapshot, poi il controllo di salute, e il ripristino se la nuova versione non funziona. Attivali per ogni stack in Stack, nei dettagli dello stack.";
    "dk.au.hour" => "Window start (hour)", "Inizio della finestra (ora)";
    "dk.au.minute" => "Window start (minute)", "Inizio della finestra (minuto)";
    "dk.au.window" => "Window length (minutes)", "Durata della finestra (minuti)";
    "dk.au.window_help" => "Updates start only inside the window.", "Gli aggiornamenti partono solo dentro la finestra.";
    "dk.au.chip" => "Automatic", "Automatico";
    "dk.au.stack" => "Automatic updates of this stack", "Aggiornamenti automatici di questo stack";
    "dk.au.enable" => "Update automatically", "Aggiorna automaticamente";
    "dk.au.enable_help" => "In the daily window: download, snapshot, start, health check, rollback when it fails. A notification tells you how it went.", "Nella finestra giornaliera: scaricamento, snapshot, avvio, controllo di salute, ripristino se fallisce. Una notifica ti dice com'è andata.";
    "dk.au.policy" => "Versions allowed", "Versioni ammesse";
    "dk.au.policy_help" => "Which version jumps an automatic update may take. Pre-releases and other image variants are never taken.", "Quali salti di versione può fare un aggiornamento automatico. Le versioni di prova e le altre varianti dell'immagine non vengono mai prese.";
    "dk.au.p_default" => "Default (catalogue or tag)", "Predefinite (catalogo o tag)";
    "dk.au.p_patch" => "Patch releases (1.4.2 → 1.4.3)", "Solo correzioni (1.4.2 → 1.4.3)";
    "dk.au.p_minor" => "Minor releases (1.4 → 1.5; 0.x: patches)", "Versioni minori (1.4 → 1.5; 0.x: correzioni)";
    "dk.au.p_calver" => "Same month (2026.11.0 → 2026.11.1)", "Stesso mese (2026.11.0 → 2026.11.1)";
    "dk.au.p_notify" => "Never: notify only", "Mai: solo notifica";
    "dk.au.next" => "Next window: {when}", "Prossima finestra: {when}";
    "dk.au.pending" => "The Docker settings have a staged change of the engine volume: apply or discard it first.", "Le impostazioni di Docker hanno in sospeso un cambio del volume del motore: applicalo o scartalo prima.";
    "dk.au.v_hold" => "Update {what}: notification only ({why}).", "Aggiornamento {what}: solo notifica ({why}).";
    "dk.au.v_off" => "Update {what}: automatic updates are off for this stack.", "Aggiornamento {what}: gli aggiornamenti automatici sono spenti per questo stack.";
    "dk.au.v_auto" => "Update {what}: installed in the next window.", "Aggiornamento {what}: verrà installato nella prossima finestra.";
    "dk.au.v_label" => "Update {what}: after the download the image version label decides (allowed jumps only).", "Aggiornamento {what}: dopo lo scaricamento decide l'etichetta di versione dell'immagine (solo salti ammessi).";
    "dk.au.r_major" => "new major version", "nuova versione principale";
    "dk.au.r_minor" => "new minor version", "nuova versione minore";
    "dk.au.r_month" => "new calendar month", "nuovo mese di calendario";
    "dk.au.r_pre" => "pre-release version", "versione di prova";
    "dk.au.r_variant" => "another image variant", "un'altra variante dell'immagine";
    "dk.au.r_downgrade" => "older version", "versione più vecchia";
    "dk.au.r_unknown" => "version not readable", "versione non leggibile";
    "dk.au.r_policy" => "the policy is notify only", "la regola è solo notifica";
    "dk.au.r_review" => "the new entry needs answers or is a major change", "la nuova voce chiede risposte o è un cambio importante";
    "dk.au.r_managed" => "rynas does not manage the files of this stack", "rynas non gestisce i file di questo stack";
    "dk.au.o_updated" => "Updated automatically {when}.", "Aggiornato automaticamente {when}.";
    "dk.au.o_rolled_back" => "Automatic update of {when} rolled back:", "Aggiornamento automatico di {when} annullato:";
    "dk.au.o_held" => "Automatic update of {when} not installed:", "Aggiornamento automatico di {when} non installato:";
    "dk.au.o_failed" => "Automatic update of {when} failed:", "Aggiornamento automatico di {when} fallito:";
    "dk.g.rynas" => "Managed by rynas", "Gestiti da rynas";
    "dk.g.portainer" => "Managed by Portainer", "Gestiti da Portainer";
    "dk.g.untracked" => "Other compose projects", "Altri progetti compose";
    "dk.m.rynas" => "rynas", "rynas";
    "dk.m.portainer" => "Portainer", "Portainer";
    "dk.m.untracked" => "Not managed by rynas", "Non gestito da rynas";
    "dk.ss.running" => "Running", "Attivo";
    "dk.ss.partial" => "Partly running", "Attivo in parte";
    "dk.ss.stopped" => "Stopped", "Fermo";
    "dk.ss.down" => "Not started", "Non avviato";
    "dk.s.search" => "Search stacks", "Cerca stack";
    "dk.s.create" => "Create stack", "Crea stack";
    "dk.s.none" => "No stacks", "Nessuno stack";
    "dk.s.none_text" => "Create a stack from a compose file, or install an app.", "Crea uno stack da un file compose o installa un'app.";
    "dk.s.unhealthy_n" => "{n} unhealthy", "{n} non in salute";
    "dk.s.title" => "Stack {name}", "Stack {name}";
    "dk.s.folder" => "Folder", "Cartella";
    "dk.s.files" => "Compose files", "File compose";
    "dk.s.readonly" => "This stack is managed outside rynas: here it can only be started, stopped and restarted.", "Questo stack è gestito fuori da rynas: qui si può solo avviare, fermare e riavviare.";
    "dk.s.readonly_adopt" => "This stack is managed outside rynas: here it can be started, stopped and restarted, or adopted to be managed fully.", "Questo stack è gestito fuori da rynas: qui si può avviare, fermare e riavviare, oppure adottare per gestirlo del tutto.";
    "dk.s.no_folder" => "The stack's folder is missing: it can only be started, stopped and restarted.", "Manca la cartella dello stack: si può solo avviare, fermare e riavviare.";
    // Docker adopt
    "dk.ad.adopt" => "Adopt", "Adotta";
    "dk.ad.title" => "Adopt {name}", "Adotta {name}";
    "dk.ad.text" => "rynas will manage this stack in its current folder: edit, start, stop, update. Nothing is moved or rewritten.", "rynas gestirà questo stack nella sua cartella attuale: modifica, avvio, arresto, aggiornamenti. Niente viene spostato o riscritto.";
    "dk.ad.text_ro" => "Portainer keeps managing these files: rynas shows them, starts and stops the stack and checks its images, but does not edit them.", "Portainer continua a gestire questi file: rynas li mostra, avvia e ferma lo stack e controlla le sue immagini, ma non li modifica.";
    "dk.ad.env" => "Environment files", "File di ambiente";
    "dk.ad.env_default" => ".env of the folder", ".env della cartella";
    "dk.ad.dir" => "Folder on this system", "Cartella su questo sistema";
    "dk.ad.dir_help" => "The folder with the compose files, as this system sees it.", "La cartella con i file compose, come la vede questo sistema.";
    "dk.ad.profiles" => "Profiles to start", "Profili da avviare";
    "dk.ad.profiles_help" => "Names separated by commas; services with other profiles stay off.", "Nomi separati da virgole; i servizi con altri profili restano spenti.";
    "dk.ad.done" => "{name} is now managed by rynas", "{name} ora è gestito da rynas";
    "dk.ad.w_other" => "{manager} can still edit these files: rynas notices changes made outside.", "{manager} può ancora modificare questi file: rynas si accorge delle modifiche fatte altrove.";
    "dk.ad.w_portainer" => "The files are in Portainer's data: adopted read-only.", "I file sono nei dati di Portainer: adottato in sola lettura.";
    "dk.ad.w_name" => "The files say name: {file}; rynas runs the stack as {name}.", "I file dicono name: {file}; rynas avvia lo stack come {name}.";
    "dk.ad.w_in_use" => "Containers of another folder already use the name {name}: choose another name.", "Container di un'altra cartella usano già il nome {name}: scegline un altro.";
    "dk.ad.w_networks" => "External networks must exist before starting: {networks}", "Le reti esterne devono esistere prima dell'avvio: {networks}";
    "dk.ad.w_profiles" => "Some services start only with a profile: {profiles}", "Alcuni servizi partono solo con un profilo: {profiles}";
    "dk.ad.w_include" => "The files use include: the stack runs, but its files are not edited here.", "I file usano include: lo stack funziona, ma i suoi file non si modificano qui.";
    "dk.ad.w_outside" => "A compose or environment file is outside the folder: the stack runs, but its files are not edited here.", "Un file compose o di ambiente è fuori dalla cartella: lo stack funziona, ma i suoi file non si modificano qui.";
    "dk.ad.w_missing" => "The compose files were not found on this system: give the folder where they are.", "I file compose non sono stati trovati su questo sistema: indica la cartella dove si trovano.";
    "dk.ad.changed" => "The files changed outside rynas since it last read or wrote them.", "I file sono cambiati fuori da rynas dall'ultima lettura o scrittura.";
    "dk.ad.ro_note" => "Portainer manages these files: here the stack can be started, stopped and restarted, its files viewed and its images checked.", "Portainer gestisce questi file: qui lo stack si può avviare, fermare e riavviare, vederne i file e controllarne le immagini.";
    "dk.ad.chip" => "Adopted", "Adottato";
    "dk.ad.move" => "Move to stacks folder", "Sposta nella cartella stack";
    "dk.ad.move_title" => "Move {name}", "Sposta {name}";
    "dk.ad.move_text" => "The stack stops, its whole folder ({from}) is copied with its data to {to}, the bind mounts are checked, and it starts again. The old folder is kept with a .moved name until you delete it.", "Lo stack si ferma, l'intera cartella ({from}) viene copiata con i dati in {to}, i bind mount vengono controllati e lo stack riparte. La vecchia cartella resta con un nome .moved finché non la elimini.";
    "dk.ad.move_free" => "Free space on the target", "Spazio libero nella destinazione";
    "dk.ad.move_copy" => "Copy", "Copia";
    "dk.ad.move_reflink" => "Clone on the same btrfs volume (reflink): quick, no extra space", "Clone sullo stesso volume btrfs (reflink): rapido, senza spazio in più";
    "dk.ad.move_full" => "Full copy of every file: needs as much free space as the folder", "Copia completa di ogni file: serve tanto spazio libero quanto la cartella";
    "dk.ad.move_down" => "Stack stopped", "Stack fermo";
    "dk.ad.move_down_short" => "Seconds: the copy only clones the files", "Pochi secondi: la copia clona soltanto i file";
    "dk.ad.move_down_long" => "For the whole copy: minutes or more with large app data", "Per tutta la copia: minuti o più con dati dell'app grandi";
    "dk.ad.release" => "Release", "Rilascia";
    "dk.ad.release_title" => "Release {name}?", "Rilasciare {name}?";
    "dk.ad.release_text" => "rynas stops managing this stack. Its files and containers stay as they are.", "rynas smette di gestire questo stack. I suoi file e i container restano come sono.";
    "dk.ad.released" => "Stack released", "Stack rilasciato";
    "dk.ad.ed_ro" => "These files are not edited here (read-only, outside the folder or with include:).", "Questi file non si modificano qui (sola lettura, fuori dalla cartella o con include:).";
    "dk.ad.ed_others" => "The stack also uses: {files}", "Lo stack usa anche: {files}";
    "dk.im.button" => "Import folder", "Importa cartella";
    "dk.im.title" => "Import a folder", "Importa una cartella";
    "dk.im.text" => "Register a folder with compose files (for example restored from a backup) as a stack, where it is.", "Registra come stack una cartella con file compose (per esempio ripristinata da un backup), dove si trova.";
    "dk.im.dir_help" => "Absolute path, such as /volume1/docker/app.", "Percorso assoluto, come /volume1/docker/app.";
    "dk.im.name_help" => "The compose project name; the folder keeps its name.", "Il nome del progetto compose; la cartella mantiene il suo nome.";
    "dk.im.check" => "Check", "Controlla";
    "dk.im.import" => "Import", "Importa";
    "dk.s.containers" => "Containers", "Container";
    "dk.s.no_containers" => "No containers: start the stack with Up.", "Nessun container: avvia lo stack con Avvia (up).";
    "dk.snap.title" => "Snapshots", "Snapshot";
    "dk.snap.take" => "Take snapshot", "Fai uno snapshot";
    "dk.snap.taken" => "Snapshot taken", "Snapshot fatto";
    "dk.snap.none" => "No snapshots yet: one is taken before every update.", "Ancora nessuno snapshot: se ne fa uno prima di ogni aggiornamento.";
    "dk.snap.not_subvolume" => "The stack folder is not a btrfs subvolume: no snapshots and no rollback.", "La cartella dello stack non è un subvolume btrfs: niente snapshot e niente ripristino.";
    "dk.snap.r_manual" => "Taken by hand", "Fatto a mano";
    "dk.snap.r_update" => "Before an update", "Prima di un aggiornamento";
    "dk.snap.r_pull" => "Before a pull", "Prima di uno scaricamento";
    "dk.snap.r_rollback" => "Before a rollback", "Prima di un ripristino";
    "dk.snap.rollback" => "Roll back", "Ripristina";
    "dk.snap.rollback_text" => "The stack is taken down, its folder goes back to the snapshot of {when} with the images of that time, then it starts again. The current state is kept as a snapshot.", "Lo stack viene fermato, la sua cartella torna allo snapshot del {when} con le immagini di allora, poi riparte. Lo stato attuale resta come snapshot.";
    "dk.snap.delete" => "Delete snapshot", "Elimina snapshot";
    "dk.snap.delete_text" => "The snapshot of {when} is deleted: a rollback to it is no longer possible.", "Lo snapshot del {when} viene eliminato: non sarà più possibile ripristinarlo.";
    "dk.rm.title" => "Remove {name}", "Rimuovi {name}";
    "dk.rm.text" => "Stops the stack and removes its containers. Named volumes are kept.", "Ferma lo stack e rimuove i suoi container. I volumi con nome restano.";
    "dk.rm.files" => "Also delete the stack folder (compose file, .env, app data) and its snapshots", "Elimina anche la cartella dello stack (file compose, .env, dati dell'app) e i suoi snapshot";
    "dk.rm.adopted" => "The stack folder, its files and app data stay where they are. To stop managing the stack without removing its containers, use Release.", "La cartella dello stack, i suoi file e i dati dell'app restano dove sono. Per smettere di gestire lo stack senza rimuoverne i container, usa Rilascia.";
    "dk.ed.name" => "Stack name", "Nome dello stack";
    "dk.ed.name_help" => "Lowercase letters, digits, - and _: also the folder name.", "Lettere minuscole, cifre, - e _: è anche il nome della cartella.";
    "dk.ed.name_invalid" => "Use 1-63 lowercase letters, digits, - or _, starting with a letter or digit.", "Usa da 1 a 63 lettere minuscole, cifre, - o _, iniziando con una lettera o una cifra.";
    "dk.ed.volume" => "Volume", "Volume";
    "dk.ed.compose" => "compose.yaml", "compose.yaml";
    "dk.ed.env" => ".env", ".env";
    "dk.ed.env_help" => "Variables for the compose file (NAME=value per line). Kept readable by root only.", "Variabili per il file compose (NOME=valore per riga). Leggibile solo da root.";
    "dk.ed.start" => "Start the stack after creating it", "Avvia lo stack dopo averlo creato";
    "dk.ad.start_import" => "Start the stack after importing it", "Avvia lo stack dopo averlo importato";
    "dk.ed.validate" => "Check", "Controlla";
    "dk.ed.created" => "Stack created", "Stack creato";
    "dk.ed.saved" => "Files saved: use Up to apply them", "File salvati: usa Avvia (up) per applicarli";
    "dk.val.bad" => "docker compose refuses the file.", "docker compose rifiuta il file.";
    "dk.val.ok" => "The file is valid. Services: {services}. Published ports: {ports}.", "Il file è valido. Servizi: {services}. Porte pubblicate: {ports}.";
    "dk.val.ports" => "Ports already used by other containers: {ports}", "Porte già usate da altri container: {ports}";
    "dk.val.outside" => "{path} ({service}) is outside the volume: it is not in the stack's snapshots and the folder is not created for you.", "{path} ({service}) è fuori dal volume: non è negli snapshot dello stack e la cartella non viene creata per te.";
    "dk.val.create" => "Folders created before the start, owned by docker-apps: {paths}", "Cartelle create prima dell'avvio, di proprietà di docker-apps: {paths}";
    "dk.i.title" => "Images", "Immagini";
    "dk.i.total" => "{n} · {size}", "{n} · {size}";
    "dk.i.none" => "No images", "Nessuna immagine";
    "dk.i.untagged" => "No tag", "Senza tag";
    "dk.i.used_by" => "Used by {n}", "Usata da {n}";
    "dk.i.unused" => "Not used", "Non usata";
    "dk.i.dangling" => "Dangling", "Orfana";
    "dk.v.title" => "Volumes", "Volumi";
    "dk.v.none" => "No named volumes", "Nessun volume con nome";
    "dk.w.open" => "Open Docker", "Apri Docker";
    "dk.w.running" => "Containers running", "Container in esecuzione";
    "dk.w.cpu" => "CPU of all containers", "CPU di tutti i container";
    "dk.w.top" => "Busiest containers", "Container più attivi";
    "dk.w.none" => "No container is running.", "Nessun container in esecuzione.";
    "dk.w.stopped" => "Docker engine stopped", "Motore Docker fermo";
    "dk.w.stopped_text" => "Docker is installed but its engine does not answer. Open Docker to see why.", "Docker è installato ma il motore non risponde. Apri Docker per vederne il motivo.";
    // ---- Apps & Plugins (D-APPCAT, wave A2)
    "yes" => "Yes", "Sì";
    "no" => "No", "No";
    "task.apps.install" => "Install app", "Installazione dell'app";
    "task.apps.update" => "Update app", "Aggiornamento dell'app";
    "task.apps.rollback" => "Roll back app", "Ripristino dell'app";
    "task.apps.uninstall" => "Uninstall app", "Disinstallazione dell'app";
    "task.apps.proxy" => "Move app behind the proxy", "Spostamento dell'app dietro il proxy";
    "task.apps.auto-update" => "Automatic app update", "Aggiornamento automatico dell'app";
    "task.apps.catalog-update" => "Check the app catalogue", "Controllo del catalogo delle app";
    "apps.subtitle" => "Curated apps in containers and the modules that add features to rynas", "App selezionate in container e i moduli che aggiungono funzioni a rynas";
    "apps.docker_first" => "Apps run in Docker containers: install Docker first, then any app of the catalogue installs in a few steps.", "Le app girano in container Docker: installa prima Docker, poi ogni app del catalogo si installa in pochi passaggi.";
    "apps.plg.docker_above" => "The Docker module is at the top of this page: install it before any app.", "Il modulo Docker è in cima a questa pagina: installalo prima di qualsiasi app.";
    "apps.tab.featured" =>"Featured", "In evidenza";
    "apps.tab.apps" => "Apps", "App";
    "apps.tab.plugins" => "Plugins", "Plugin";
    "apps.tab.installed" => "Installed", "Installate";
    "apps.tab.updates" => "Updates", "Aggiornamenti";
    "apps.updates_waiting" => "Installed apps with an update: {n}.", "App installate con un aggiornamento: {n}.";
    "apps.see_updates" => "See the updates", "Vedi gli aggiornamenti";
    "apps.feat.fit" => "Recommended apps", "App consigliate";
    "apps.feat.ram" => "Chosen for {ram} of RAM and the {arch} architecture.", "Scelte per {ram} di RAM e l'architettura {arch}.";
    "apps.feat.arch" => "Chosen for the {arch} architecture.", "Scelte per l'architettura {arch}.";
    "apps.feat.none" => "Every app that fits is already installed.", "Tutte le app adatte sono già installate.";
    "apps.feat.managers" => "Docker managers", "Gestori di Docker";
    "apps.feat.managers_text" => "A full container manager next to rynas, on the same stacks folder. It gets the Docker socket, so it has root privileges and its own login.", "Un gestore di container completo accanto a rynas, sulla stessa cartella degli stack. Riceve il socket di Docker, quindi ha privilegi di root e un proprio login.";
    "apps.browse_all" => "Browse all apps", "Sfoglia tutte le app";
    "apps.empty" => "The catalogue is empty", "Il catalogo è vuoto";
    "apps.empty_text" => "No app could be loaded. Reinstall the catalogue package or check the entries listed below.", "Nessuna app è stata caricata. Reinstalla il pacchetto del catalogo o controlla le voci elencate sotto.";
    "apps.issues" => "Entries not loaded: {n}", "Voci non caricate: {n}";
    "apps.count" => "Shown: {n}", "Mostrate: {n}";
    "apps.no_match" => "No app matches", "Nessuna app corrisponde";
    "apps.no_match_text" => "Try another word or category.", "Prova un'altra parola o categoria.";
    "apps.search" => "Search apps", "Cerca app";
    "apps.category" => "Category", "Categoria";
    "apps.cat.all" => "All categories", "Tutte le categorie";
    "apps.cat.management" => "Management", "Gestione";
    "apps.cat.media" => "Media", "Multimedia";
    "apps.cat.photos" => "Photos", "Foto";
    "apps.cat.files" => "Files", "File";
    "apps.cat.documents" => "Documents", "Documenti";
    "apps.cat.productivity" => "Productivity", "Produttività";
    "apps.cat.security" => "Security", "Sicurezza";
    "apps.cat.home_automation" => "Home automation", "Domotica";
    "apps.cat.surveillance" => "Video surveillance", "Videosorveglianza";
    "apps.cat.monitoring" => "Monitoring", "Monitoraggio";
    "apps.cat.network" => "Network", "Rete";
    "apps.cat.sync" => "Sync", "Sincronizzazione";
    "apps.cat.development" => "Development", "Sviluppo";
    "apps.cat.backup" => "Backup", "Backup";
    "apps.cat.ai" => "AI", "IA";
    "apps.af.non_free" => "Not free software", "Software non libero";
    "apps.af.non_free_text" => "The app or a part of it is not free software: its license limits use, changes or sharing.", "L'app o una sua parte non è software libero: la licenza limita uso, modifiche o condivisione.";
    "apps.af.high_resources" => "Heavy", "Pesante";
    "apps.af.high_resources_text" => "Needs more than 2 GB of RAM to run well.", "Ha bisogno di più di 2 GB di RAM per funzionare bene.";
    "apps.af.needs_gpu" => "Needs a GPU", "Richiede una GPU";
    "apps.af.needs_gpu_text" => "Needs a graphics processor or another accelerator.", "Ha bisogno di un processore grafico o di un altro acceleratore.";
    "apps.af.telemetry" => "Telemetry", "Telemetria";
    "apps.af.telemetry_text" => "Sends usage data to its developers by default.", "Invia dati d'uso ai suoi sviluppatori in modo predefinito.";
    "apps.af.unmaintained" => "Unmaintained", "Non mantenuta";
    "apps.af.unmaintained_text" => "No new release upstream for a long time: security fixes may not come.", "Nessuna nuova versione da molto tempo: le correzioni di sicurezza potrebbero non arrivare.";
    "apps.af.requires_https" => "Needs HTTPS", "Richiede HTTPS";
    "apps.af.requires_https_text" => "Its clients refuse plain HTTP: publish it through web access or another TLS proxy.", "I suoi client rifiutano HTTP semplice: pubblicala tramite l'accesso web o un altro proxy TLS.";
    "apps.af.root_access" => "Root access", "Accesso root";
    "apps.af.root_access_text" => "Gets root-level access to the NAS (the Docker socket, privileged mode or host devices).", "Ottiene un accesso di livello root al NAS (socket di Docker, modalità privilegiata o dispositivi dell'host).";
    "apps.ram.ok" => "Enough RAM", "RAM sufficiente";
    "apps.ram.warn" => "Low RAM: {have} of {need} advised", "Poca RAM: {have} su {need} consigliati";
    "apps.ram.blocked" => "Not enough RAM: {have} of {need}", "RAM insufficiente: {have} su {need}";
    "apps.ram.unknown" => "RAM not checked", "RAM non verificata";
    "apps.ram.min" => "Minimum RAM", "RAM minima";
    "apps.ram.recommended" => "Recommended RAM", "RAM consigliata";
    "apps.block.docker" => "Install Docker first", "Installa prima Docker";
    "apps.block.volume" => "Docker has no volume yet: choose one in the Docker settings", "Docker non ha ancora un volume: sceglilo nelle impostazioni di Docker";
    "apps.block.arch" => "Not available for this processor", "Non disponibile per questo processore";
    "apps.block.rynas" => "Needs a newer rynas", "Richiede un rynas più recente";
    "apps.block.ram" => "Not enough RAM to install", "RAM insufficiente per l'installazione";
    "apps.block.reverse_proxy" => "This proxy needs ports 80 and 443: switch web access to external proxy mode first.", "Questo proxy richiede le porte 80 e 443: prima passa l'accesso web alla modalità proxy esterno.";
    "apps.block.to_mode" => "Change the mode", "Cambia la modalità";
    "apps.installed" => "Installed", "Installata";
    "apps.version" => "Version {v}", "Versione {v}";
    "apps.version_pkg" => "Version {v} · package {pkg}", "Versione {v} · pacchetto {pkg}";
    "apps.license" => "License", "Licenza";
    "apps.details" => "Details", "Dettagli";
    "apps.install" => "Install", "Installa";
    "apps.manage" => "Manage", "Gestisci";
    "apps.update" => "Update", "Aggiorna";
    "apps.homepage" => "Website", "Sito web";
    "apps.source" => "Source code", "Codice sorgente";
    "apps.changelog" => "Release notes", "Note di rilascio";
    "apps.screenshot" => "Screenshot", "Schermata";
    "apps.maintainers" => "Catalogue maintainers", "Manutentori nel catalogo";
    "apps.d.manager" => "Docker manager", "Gestore di Docker";
    "apps.d.antifeatures" => "Things to know", "Cose da sapere";
    "apps.d.resources" => "Resources", "Risorse";
    "apps.d.ports" => "Ports", "Porte";
    "apps.d.folders" => "Data folders", "Cartelle dei dati";
    "apps.d.folders_text" => "Created in the app's stack folder, so its snapshots cover them; single folders can go on another volume.", "Create nella cartella dello stack dell'app, così le sue istantanee le includono; singole cartelle possono andare su un altro volume.";
    "apps.d.shares" => "Shared folders", "Cartelle condivise";
    "apps.d.secrets" => "Generated secrets", "Segreti generati";
    "apps.d.volumes" => "Disposable volumes (not in snapshots)", "Volumi usa e getta (non nelle istantanee)";
    "apps.secret.shown" => "shown once after the install", "mostrato una volta dopo l'installazione";
    "apps.gpu" => "Graphics", "Grafica";
    "apps.gpu.optional" => "Optional, for acceleration", "Facoltativa, per l'accelerazione";
    "apps.gpu.required" => "Required", "Necessaria";
    "apps.cpu" => "Processor features", "Funzioni del processore";
    "apps.network" => "Network", "Rete";
    "apps.host_network" => "Host network", "Rete dell'host";
    "apps.col.port" => "Port", "Porta";
    "apps.col.purpose" => "Purpose", "Uso";
    "apps.col.reach" => "Reachable", "Raggiungibile";
    "apps.col.folder" => "Folder", "Cartella";
    "apps.col.kind" => "Kind", "Tipo";
    "apps.col.access" => "Access", "Accesso";
    "apps.col.app" => "App", "App";
    "apps.col.version" => "Version", "Versione";
    "apps.col.service" => "Service", "Servizio";
    "apps.col.change" => "Change", "Modifica";
    "apps.col.images" => "Image", "Immagine";
    "apps.port.proxied" => "Web, can go behind the proxy", "Web, può stare dietro il proxy";
    "apps.port.fixed" => "Fixed", "Fissa";
    "apps.intent.config" => "Settings", "Impostazioni";
    "apps.intent.data" => "Data", "Dati";
    "apps.intent.db" => "Database", "Database";
    "apps.intent.media" => "Media files", "Contenuti multimediali";
    "apps.intent.cache" => "Cache", "Cache";
    "apps.ssd.ok" => "SSD helps", "Meglio su SSD";
    "apps.ssd.preferred" => "Best on SSD", "Ideale su SSD";
    "apps.share.required" => "Required", "Obbligatoria";
    "apps.web.port" => "Port and path", "Porta e percorso";
    "apps.web.auth" => "Login", "Accesso";
    "apps.web.auth_app" => "The app has its own login", "L'app ha un proprio login";
    "apps.web.auth_none" => "No login of its own: keep it on the LAN", "Nessun login proprio: tienila sulla LAN";
    "apps.web.auth_proxy" => "Trusts the login of the proxy", "Si fida del login del proxy";
    "apps.web.public" => "Internet exposure", "Esposizione su Internet";
    "apps.web.public_ok" => "Suitable", "Adatta";
    "apps.web.public_warn" => "Possible, with care", "Possibile, con attenzione";
    "apps.web.public_never" => "Never", "Mai";
    "apps.web.body" => "Largest upload", "Caricamento massimo";
    "apps.web.timeout" => "Response time limit", "Tempo massimo di risposta";
    "apps.web.websocket" => "Live updates (WebSocket)", "Aggiornamenti in tempo reale (WebSocket)";
    "apps.web.subpath" => "Works under a path", "Funziona sotto un percorso";
    "apps.web.streaming" => "Streaming and large uploads", "Streaming e caricamenti grandi";
    "apps.w.title" => "Install {name}", "Installa {name}";
    "apps.w.s_options" => "Options", "Opzioni";
    "apps.w.s_folders" => "Folders and shares", "Cartelle e condivisioni";
    "apps.w.s_network" => "Network", "Rete";
    "apps.w.s_access" => "Access", "Accesso";
    "apps.w.stack_help" => "The folder and the compose project of the app; it cannot change later.", "La cartella e il progetto compose dell'app; non si può cambiare in seguito.";
    "apps.w.stack_dir" => "The app goes in {path}.", "L'app va in {path}.";
    "apps.w.no_folders" => "This app needs no folders or shared folders.", "Questa app non ha bisogno di cartelle né di cartelle condivise.";
    "apps.w.folders_text" => "By default every folder is in the stack folder. Pick a volume to place one elsewhere, for example a database on an SSD.", "Per impostazione predefinita ogni cartella sta nella cartella dello stack. Scegli un volume per metterne una altrove, per esempio un database su un SSD.";
    "apps.w.in_stack" => "In the stack folder: {path}", "Nella cartella dello stack: {path}";
    "apps.w.on_volume" => "On {vol}: {path}", "Su {vol}: {path}";
    "apps.w.on_volume_suggested" => "On {vol} (suggested): {path}", "Su {vol} (consigliato): {path}";
    "apps.w.ssd_hint" => "A database answers much faster on an SSD volume.", "Un database risponde molto più in fretta su un volume SSD.";
    "apps.w.shares_text" => "Folders the app reads or writes, with the access shown. Nothing is moved or copied.", "Cartelle che l'app legge o scrive, con l'accesso indicato. Nulla viene spostato o copiato.";
    "apps.w.no_share" => "Not mapped", "Non collegata";
    "apps.w.no_share_fallback" => "Not mapped (uses {path} in the stack folder)", "Non collegata (usa {path} nella cartella dello stack)";
    "apps.w.no_shares_yet" => "There are no shared folders yet.", "Non ci sono ancora cartelle condivise.";
    "apps.w.to_shares" => "Create one", "Creane una";
    "apps.w.host_network" => "The app uses the host network: its ports are fixed and only listed here.", "L'app usa la rete dell'host: le sue porte sono fisse e qui sono solo elencate.";
    "apps.w.no_ports" => "The app publishes no ports.", "L'app non pubblica porte.";
    "apps.w.port_used" => "Port {port} is in use", "La porta {port} è occupata";
    "apps.w.port_fixed_used" => "Port {port} is in use and this app cannot move it.", "La porta {port} è occupata e questa app non può spostarla.";
    "apps.w.use_port" => "Use {port}", "Usa {port}";
    "apps.w.check_again" => "Check again", "Controlla di nuovo";
    "apps.w.errors" => "Fix these answers first:", "Correggi prima queste risposte:";
    "apps.w.folder" => "Folder", "Cartella";
    "apps.w.version" => "Catalogue version", "Versione del catalogo";
    "apps.w.address" => "Address", "Indirizzo";
    "apps.w.create" => "Folders to create", "Cartelle da creare";
    "apps.w.existing" => "Folders already there", "Cartelle già presenti";
    "apps.w.shares_used" => "Shared folders used", "Cartelle condivise usate";
    "apps.w.compose" => "compose.yaml that will be written", "compose.yaml che verrà scritto";
    "apps.w.env" => ".env that will be written (secrets hidden)", ".env che verrà scritto (segreti nascosti)";
    "apps.w.installing" => "Installing as stack {stack} in {path}.", "Installazione come stack {stack} in {path}.";
    "apps.w.installed" => "The app is installed and starting.", "L'app è installata e si sta avviando.";
    "apps.w.failed" => "The install did not finish: the log above says why and how to go on.", "L'installazione non è terminata: il log qui sopra dice perché e come proseguire.";
    "apps.w.secrets" => "Initial credentials", "Credenziali iniziali";
    "apps.w.save_now" => "Save it now, for example in a password manager: it is shown only this once.", "Salvala ora, per esempio in un gestore di password: viene mostrata solo questa volta.";
    "apps.w.copy" => "Copy", "Copia";
    "apps.w.close_secrets" => "The credentials will not be shown again.", "Le credenziali non verranno più mostrate.";
    "apps.w.close_anyway" => "Close anyway", "Chiudi comunque";
    "apps.warn.docker_not_installed" => "Docker is not installed: this is a preview, the install is refused.", "Docker non è installato: questa è un'anteprima, l'installazione viene rifiutata.";
    "apps.warn.no_volume" => "Docker has no volume yet: the install is refused.", "Docker non ha ancora un volume: l'installazione viene rifiutata.";
    "apps.warn.engine_unreachable" => "Docker did not answer: container ports were not checked.", "Docker non ha risposto: le porte dei container non sono state controllate.";
    "apps.warn.ram_below_recommended" => "This NAS has less RAM than the app recommends: it may be slow.", "Questo NAS ha meno RAM di quella consigliata dall'app: potrebbe essere lenta.";
    "apps.warn.published_on_lan" => "The web port opens on the LAN, without the web access proxy in front.", "La porta web si apre sulla LAN, senza il proxy dell'accesso web davanti.";
    "apps.warn.proxy_inactive" => "The web port listens only on the NAS itself while web access is not active: the app is unreachable until web access runs.", "La porta web ascolta solo sul NAS finché l'accesso web non è attivo: l'app è irraggiungibile finché l'accesso web non funziona.";
    "apps.fe.invalid_value" => "Not a valid value", "Valore non valido";
    "apps.fe.missing_value" => "Required", "Obbligatorio";
    "apps.fe.unknown_value" => "Not a question of this app", "Non è una domanda di questa app";
    "apps.fe.unsafe_value" => "Contains a line break, a single quote or a NUL character", "Contiene un a capo, un apice singolo o un carattere NUL";
    "apps.fe.share_missing" => "The shared folder does not exist", "La cartella condivisa non esiste";
    "apps.fe.bad_path" => "Not a folder on a mounted volume", "Non è una cartella su un volume montato";
    "apps.fe.bad_port" => "This port cannot be used", "Questa porta non si può usare";
    "apps.fe.bad_stack" => "Not a valid stack name, or the stack folder exists", "Nome dello stack non valido, oppure la cartella dello stack esiste";
    "apps.bind.lan" => "LAN", "LAN";
    "apps.bind.proxy" => "Web access only", "Solo accesso web";
    "apps.inst.none" => "No app installed", "Nessuna app installata";
    "apps.inst.none_text" => "Apps installed from the catalogue show up here, with their state and updates.", "Le app installate dal catalogo compaiono qui, con stato e aggiornamenti.";
    "apps.inst.no_docker" => "Docker is not installed", "Docker non è installato";
    "apps.inst.engine_down" => "Docker not answering", "Docker non risponde";
    "apps.inst.left" => "No longer in the catalogue", "Non più nel catalogo";
    "apps.inst.left_text" => "This app left the catalogue: it keeps running, but gets no updates from it. Converting it to a custom stack makes that explicit.", "Questa app è uscita dal catalogo: continua a funzionare, ma non riceve più aggiornamenti. Convertirla in uno stack personalizzato lo rende esplicito.";
    "apps.inst.on_lan" => "The app is published on the LAN directly. Move it behind the proxy to reach it only through web access, with HTTPS.", "L'app è pubblicata direttamente sulla LAN. Spostala dietro il proxy per raggiungerla solo tramite l'accesso web, con HTTPS.";
    "apps.inst.catalog" => "Catalogue", "Catalogo";
    "apps.inst.title" => "App {name}", "App {name}";
    "apps.inst.no_snaps" => "No snapshot yet: the first update takes one.", "Ancora nessuna istantanea: il primo aggiornamento ne crea una.";
    "apps.in_docker" => "Show in Docker", "Mostra in Docker";
    "apps.proxy" => "Move behind the proxy", "Sposta dietro il proxy";
    "apps.proxy_text" => "The app's web port will listen only on the NAS itself, and web access gives the app its own HTTPS address. The stack restarts.", "La porta web dell'app ascolterà solo sul NAS e l'accesso web darà all'app un proprio indirizzo HTTPS. Lo stack si riavvia.";
    "apps.w.access_how" => "How the app is reached", "Come si raggiunge l'app";
    "apps.w.access_proxy" => "Through web access (recommended)", "Tramite l'accesso web (consigliato)";
    "apps.w.access_proxy_help" => "HTTPS at the app's own address, only for the devices web access allows. Its port stays closed on the LAN.", "HTTPS all'indirizzo proprio dell'app, solo per i dispositivi ammessi dall'accesso web. La sua porta resta chiusa sulla LAN.";
    "apps.w.access_lan" => "Directly on the LAN port", "Direttamente sulla porta LAN";
    "apps.w.access_lan_help" => "Port {port} opens on every address of the NAS, without HTTPS and without the access rules.", "La porta {port} si apre su ogni indirizzo del NAS, senza HTTPS e senza le regole di accesso.";
    "apps.w.access_url_pending" => "The address is shown once the plan is ready.", "L'indirizzo compare quando il piano è pronto.";
    "apps.w.access_level" => "Who may connect (this network by default) can be changed later in", "Chi può collegarsi (di default questa rete) si cambia in seguito in";
    "apps.w.access_inactive" => "Web access is not active, so the app opens directly on its LAN port: {url}. With web access on, apps get their own HTTPS address.", "L'accesso web non è attivo, quindi l'app si apre direttamente sulla sua porta LAN: {url}. Con l'accesso web attivo le app hanno un proprio indirizzo HTTPS.";
    "apps.w.access_enable" => "Turn on web access", "Attiva l'accesso web";
    "apps.proxy_url" => "New address: {url}", "Nuovo indirizzo: {url}";
    "apps.proxy_more" => "Who may connect and the checks of the address are on the page", "Chi può collegarsi e le verifiche dell'indirizzo sono nella pagina";
    "wa.title" => "Web access", "Accesso web";
    "wa.subtitle" => "HTTPS addresses for the interface and the apps, and who may reach them", "Indirizzi HTTPS per l'interfaccia e le app, e chi può raggiungerli";
    "wa.open_text" => "Addresses of the interface and the apps, access levels, certificate", "Indirizzi dell'interfaccia e delle app, livelli di accesso, certificato";
    "wa.tab.overview" => "Overview", "Panoramica";
    "wa.tab.routes" => "App addresses", "Indirizzi delle app";
    "wa.eng.title" => "Proxy engine", "Motore del proxy";
    "wa.eng.state" => "State", "Stato";
    "wa.eng.not_installed" => "Not installed", "Non installato";
    "wa.eng.running" => "Running", "In esecuzione";
    "wa.eng.out_of_sync" => "Running an older configuration", "Usa una configurazione vecchia";
    "wa.eng.in_sync" => "Up to date", "Aggiornata";
    "wa.eng.sync" => "Configuration", "Configurazione";
    "wa.eng.last_load" => "Last loaded", "Ultimo caricamento";
    "wa.eng.never" => "Never", "Mai";
    "wa.eng.sync_now" => "Check and load again", "Controlla e ricarica";
    "wa.eng.last_error" => "The last load failed:", "L'ultimo caricamento non è riuscito:";
    "wa.eng.missing_text" => "The rynas-caddy package is not installed. The settings below are kept and used as soon as it is.", "Il pacchetto rynas-caddy non è installato. Le impostazioni qui sotto vengono conservate e usate appena lo è.";
    "wa.ui.title" => "This interface", "Questa interfaccia";
    "wa.ui.urls" => "Addresses", "Indirizzi";
    "wa.ui.external" => "Only the emergency address (external proxy mode)", "Solo l'indirizzo di emergenza (modalità proxy esterno)";
    "wa.ui.access" => "Who may open the interface", "Chi può aprire l'interfaccia";
    "wa.ui.ports" => "Ports", "Porte";
    "wa.ui.ports_val" => "HTTPS {https}, HTTP {http} (redirects to HTTPS)", "HTTPS {https}, HTTP {http} (rimanda a HTTPS)";
    "wa.ui.lan" => "Network ranges", "Intervalli della rete";
    "wa.ui.vpn" => "VPN ranges", "Intervalli VPN";
    "wa.ui.emergency" => "Emergency address, always served by rynas itself and never through the proxy. Keep it as a bookmark:", "Indirizzo di emergenza, servito sempre da rynas e mai tramite il proxy. Tienilo nei preferiti:";
    "wa.set.title" => "Settings", "Impostazioni";
    "wa.mode" => "Mode", "Modalità";
    "wa.mode.builtin" => "Built-in proxy", "Proxy integrato";
    "wa.mode.external" => "External proxy", "Proxy esterno";
    "wa.mode.builtin_help" => "rynas runs the proxy on ports 80 and 443: the interface and the apps get HTTPS addresses with access rules.", "rynas usa il proxy sulle porte 80 e 443: l'interfaccia e le app hanno indirizzi HTTPS con regole di accesso.";
    "wa.mode.external_help" => "Ports 80 and 443 stay free for a proxy you install yourself as an app. The built-in proxy stops, apps keep only their LAN ports and the interface is reached at the emergency address.", "Le porte 80 e 443 restano libere per un proxy che installi tu come app. Il proxy integrato si ferma, le app tengono solo le porte LAN e l'interfaccia si raggiunge all'indirizzo di emergenza.";
    "wa.set.local_names" => "Local names (.local)", "Nomi locali (.local)";
    "wa.set.local_names_help" => "Each app gets a name like app-nas.local, announced on the network with mDNS.", "Ogni app riceve un nome come app-nas.local, annunciato in rete con mDNS.";
    "wa.set.h3" => "HTTP/3", "HTTP/3";
    "wa.set.h3_help" => "Faster connections over UDP port 443 for browsers that support it.", "Connessioni più rapide sulla porta UDP 443 per i browser che lo supportano.";
    "wa.access.lan" => "This network", "Questa rete";
    "wa.access.lan_vpn" => "This network and VPN", "Questa rete e VPN";
    "wa.access.public" => "Everyone on the internet", "Chiunque su internet";
    "wa.access.lan_help" => "Devices on the networks the NAS is connected to.", "I dispositivi delle reti a cui è collegato il NAS.";
    "wa.access.lan_vpn_help" => "Also devices connected through Tailscale or WireGuard.", "Anche i dispositivi collegati tramite Tailscale o WireGuard.";
    "wa.access.public_help" => "Arrives later, with its own checklist and confirmation.", "Arriva più avanti, con una lista di controlli e una conferma dedicate.";
    "wa.soon" => "Coming later", "In arrivo";
    "wa.change.title" => "Change how the interface is reached?", "Cambiare come si raggiunge l'interfaccia?";
    "wa.change.timed" => "After saving, open the interface at its new address and confirm there within 2 minutes. Without a confirmation the previous settings come back by themselves. The emergency address {url} keeps working.", "Dopo il salvataggio, apri l'interfaccia al nuovo indirizzo e conferma lì entro 2 minuti. Senza conferma tornano da sole le impostazioni precedenti. L'indirizzo di emergenza {url} continua a funzionare.";
    "wa.change.external" => "The built-in proxy stops: the interface's .local addresses and the apps' HTTPS addresses stop working. Use the emergency address {url} from now on.", "Il proxy integrato si ferma: gli indirizzi .local dell'interfaccia e gli indirizzi HTTPS delle app smettono di funzionare. Da ora usa l'indirizzo di emergenza {url}.";
    "wa.confirm.title" => "Confirm the new address", "Conferma il nuovo indirizzo";
    "wa.confirm.text" => "The interface's own address changed. Open it and confirm there within {s} s.", "L'indirizzo dell'interfaccia è cambiato. Aprilo e conferma lì entro {s} s.";
    "wa.confirm.text_here" => "This page is open at the new address: confirm it within {s} s.", "Questa pagina è aperta al nuovo indirizzo: confermalo entro {s} s.";
    "wa.confirm.expired" => "Time is up: the previous settings are being restored…", "Tempo scaduto: si ripristinano le impostazioni precedenti…";
    "wa.confirm.rollback" => "Without a confirmation the previous settings come back by themselves when the time runs out. Port {port} (the emergency address) is never affected.", "Senza conferma le impostazioni precedenti tornano da sole allo scadere del tempo. La porta {port} (l'indirizzo di emergenza) non cambia mai.";
    "wa.confirm.open" => "Open {name}", "Apri {name}";
    "wa.confirm.go" => "Confirm", "Conferma";
    "wa.confirm.done" => "New address confirmed.", "Nuovo indirizzo confermato.";
    "wa.confirm.names_help" => "If the .local name does not open on this device (no mDNS), use the other one: it needs the NAS name in your router's DNS.", "Se il nome .local non si apre su questo dispositivo (niente mDNS), usa l'altro: richiede il nome del NAS nel DNS del router.";
    "wa.confirm.rolled_back" => "Not confirmed in time: the previous settings are back.", "Non confermato in tempo: sono tornate le impostazioni precedenti.";
    "wa.rolled.title" => "Change undone", "Modifica annullata";
    "wa.col.app" => "App", "App";
    "wa.col.address" => "Address", "Indirizzo";
    "wa.col.access" => "Who may connect", "Chi può collegarsi";
    "wa.kind.app" => "App", "App";
    "wa.kind.stack" => "Stack", "Stack";
    "wa.kind.custom" => "Other device or service", "Altro dispositivo o servizio";
    "wa.route.on" => "Published", "Pubblicato";
    "wa.route.off" => "Turned off", "Spento";
    "wa.route.test" => "Test", "Verifica";
    "wa.route.test_named" => "Test {name}", "Verifica {name}";
    "wa.route.title" => "Address of {name}", "Indirizzo di {name}";
    "wa.route.gone" => "This address no longer exists.", "Questo indirizzo non esiste più.";
    "wa.route.names" => "Names", "Nomi";
    "wa.route.upstream" => "Forwarded to", "Inoltrato a";
    "wa.route.kind" => "Kind", "Tipo";
    "wa.route.login" => "Login", "Login";
    "wa.route.headers" => "Security headers", "Intestazioni di sicurezza";
    "wa.route.enabled" => "Address on", "Indirizzo attivo";
    "wa.route.enabled_help" => "Off: the proxy closes connections to this name; the app keeps running.", "Spento: il proxy chiude le connessioni a questo nome; l'app resta in esecuzione.";
    "wa.route.auth_none" => "This app has no login of its own, so it stays limited to this network.", "Questa app non ha un login proprio, quindi resta limitata a questa rete.";
    // Web access routes on ports / custom
    "wa.add.button" => "Add an address", "Aggiungi un indirizzo";
    "wa.add.title" => "New address", "Nuovo indirizzo";
    "wa.add.intro" => "Forward a name, a port or both to another device on this network or to a service rynas did not install.", "Inoltra un nome, una porta o entrambi a un altro dispositivo di questa rete o a un servizio non installato da rynas.";
    "wa.sec.where" => "Where it answers", "Dove risponde";
    "wa.sec.forward" => "Forwarding", "Inoltro";
    "wa.f.label" => "Name", "Nome";
    "wa.f.label_help" => "Letters, digits and '-'. It starts the address, e.g. router-nas.local.", "Lettere, cifre e '-'. È l'inizio dell'indirizzo, per esempio router-nas.local.";
    "wa.f.label_bad" => "Use 1-40 characters a-z, 0-9 and '-', not at the start or end.", "Usa da 1 a 40 caratteri a-z, 0-9 e '-', non all'inizio o alla fine.";
    "wa.f.label_required" => "Enter a name for the address.", "Inserisci un nome per l'indirizzo.";
    "wa.f.upstream" => "Forward to", "Inoltra a";
    "wa.f.upstream_help" => "http:// or https://, then the device's address or name, and its port when it is not the standard one (80, 443).", "http:// o https://, poi l'indirizzo o il nome del dispositivo, e la sua porta se non è quella standard (80, 443).";
    "wa.f.upstream_bad" => "Write it as http://address:port or https://address:port (the port may be left out for 80 and 443), without a path.", "Scrivilo come http://indirizzo:porta o https://indirizzo:porta (la porta si può omettere per 80 e 443), senza percorso.";
    "wa.f.upstream_required" => "Enter the address of the device or service.", "Inserisci l'indirizzo del dispositivo o servizio.";
    "wa.f.skip_verify" => "Accept the device's own certificate", "Accetta il certificato del dispositivo";
    "wa.f.skip_verify_help" => "For devices with a self-signed certificate. The connection stays encrypted, but the device is not verified.", "Per i dispositivi con un certificato autofirmato. La connessione resta cifrata, ma il dispositivo non viene verificato.";
    "wa.f.skip_warn" => "Turn this on only for a device you know: whoever takes its address on the network could read the traffic.", "Attivalo solo per un dispositivo che conosci: chi prende il suo indirizzo in rete potrebbe leggere il traffico.";
    "wa.f.upstream_host" => "Send the device's own host name", "Invia il nome host del dispositivo";
    "wa.f.upstream_host_help" => "For devices that answer only to their own name or address. Off: the device sees the name the browser used.", "Per i dispositivi che rispondono solo al proprio nome o indirizzo. Spento: il dispositivo vede il nome usato dal browser.";
    "wa.f.prefix" => "Path prefix (optional)", "Prefisso del percorso (facoltativo)";
    "wa.f.prefix_help" => "Only requests below this path are forwarded, unchanged, e.g. /grafana. Several services can then share one port.", "Vengono inoltrate solo le richieste sotto questo percorso, invariate, per esempio /grafana. Così più servizi possono condividere una porta.";
    "wa.f.websocket" => "WebSockets and streaming pass through without extra settings.", "WebSocket e streaming passano senza impostazioni aggiuntive.";
    "wa.f.ports" => "Extra ports (optional)", "Porte aggiuntive (facoltative)";
    "wa.f.ports_help" => "HTTPS ports this address also listens on, separated by commas, e.g. 8444. Useful when your router forwards a specific port.", "Porte HTTPS su cui l'indirizzo ascolta anche, separate da virgole, per esempio 8444. Utile quando il router inoltra una porta precisa.";
    "wa.f.ports_bad" => "Use port numbers from 1024 to 65535, separated by commas.", "Usa numeri di porta da 1024 a 65535, separati da virgole.";
    "wa.f.main_port" => "Also on the standard HTTPS port", "Anche sulla porta HTTPS standard";
    "wa.f.main_port_help" => "Off: the address answers only on its extra ports.", "Spento: l'indirizzo risponde solo sulle sue porte aggiuntive.";
    "wa.f.nas" => "Also on this NAS's names", "Anche sui nomi di questo NAS";
    "wa.f.nas_help" => "On the extra ports the address also answers at the NAS's own names, e.g. https://nas.example.com:8444/.", "Sulle porte aggiuntive l'indirizzo risponde anche ai nomi del NAS, per esempio https://nas.example.com:8444/.";
    "wa.route.remove" => "Remove address", "Rimuovi indirizzo";
    "wa.route.remove_q" => "Remove this address? The device or service itself is not touched.", "Rimuovere questo indirizzo? Il dispositivo o servizio non viene toccato.";
    "wa.route.remove_title" => "Remove this address", "Rimuovi questo indirizzo";
    "wa.route.remove_text" => "The NAS stops answering on this address. The device or service itself is not touched.", "Il NAS smette di rispondere a questo indirizzo. Il dispositivo o servizio non viene toccato.";
    "wa.check.port_listening" => "Extra port listening", "Porta aggiuntiva in ascolto";
    "wa.fix.port_listening" => "The proxy does not answer on this port. Check that no other program uses it, then save again.", "Il proxy non risponde su questa porta. Controlla che nessun altro programma la usi, poi salva di nuovo.";
    "wa.auth.app" => "The app's own login", "Il login proprio dell'app";
    "wa.auth.none" => "None", "Nessuno";
    "wa.headers.default" => "Standard (no framing by other sites)", "Standard (niente incorporamento da altri siti)";
    "wa.headers.relaxed" => "Relaxed (framing allowed)", "Permissive (incorporamento ammesso)";
    "wa.routes.empty" => "No app addresses yet", "Ancora nessun indirizzo di app";
    "wa.routes.empty_text" => "Apps installed through web access get an address here; installed apps can move behind the proxy from their page.", "Le app installate tramite l'accesso web ricevono qui un indirizzo; quelle già installate si spostano dietro il proxy dalla loro scheda.";
    "wa.routes.external" => "External proxy mode: these addresses are not served until the built-in proxy is back on.", "Modalità proxy esterno: questi indirizzi non sono serviti finché il proxy integrato non torna attivo.";
    "wa.mdns" => "A .local name also needs mDNS on the device that opens it. Windows 10 and later, macOS, iOS, Android 12 and later and most Linux desktops have it; elsewhere use the emergency address or a DNS entry on your router.", "Un nome .local richiede mDNS anche sul dispositivo che lo apre. Windows 10 e successivi, macOS, iOS, Android 12 e successivi e la maggior parte dei desktop Linux lo hanno; altrove usa l'indirizzo di emergenza o una voce DNS sul router.";
    "wa.prob.name_too_long" => "The name is longer than 63 characters: shorten the app's stack name.", "Il nome supera i 63 caratteri: accorcia il nome dello stack dell'app.";
    "wa.prob.local_names_off" => "Local names are off: this app has no address.", "I nomi locali sono spenti: quest'app non ha un indirizzo.";
    "wa.prob.disabled" => "The address is turned off.", "L'indirizzo è spento.";
    "wa.prob.name_collision" => "Another device on the network already uses this name.", "Un altro dispositivo in rete usa già questo nome.";
    "wa.prob.name_unpublished" => "The name could not be announced: Avahi is not running or refused it.", "Non è stato possibile annunciare il nome: Avahi non è in esecuzione o lo ha rifiutato.";
    "wa.test.title" => "Checks", "Verifiche";
    "wa.test.text" => "Runs every step from the name to the app, on the NAS, and says what to fix.", "Controlla ogni passo dal nome all'app, sul NAS, e dice cosa correggere.";
    "wa.test.at" => "Checked {when}", "Verificato {when}";
    "wa.cs.ok" => "OK", "OK";
    "wa.cs.warn" => "Warning", "Avviso";
    "wa.cs.fail" => "Failed", "Non riuscita";
    "wa.cs.skipped" => "Skipped", "Saltata";
    "wa.check.local_name" => "Name announced on the network", "Nome annunciato in rete";
    "wa.check.dns" => "Name resolves on the NAS", "Il nome si risolve sul NAS";
    "wa.check.proxy_running" => "Proxy answering", "Il proxy risponde";
    "wa.check.route_loaded" => "Address loaded in the proxy", "Indirizzo caricato nel proxy";
    "wa.check.https_listening" => "HTTPS port open", "Porta HTTPS aperta";
    "wa.check.certificate" => "Certificate covers the name", "Il certificato copre il nome";
    "wa.check.upstream" => "App answering", "L'app risponde";
    "wa.fix.local_name" => "Another device may use this name, or Avahi is not running. Rename the device or the app's stack, then use Check and load again.", "Un altro dispositivo potrebbe usare questo nome, oppure Avahi non è in esecuzione. Rinomina il dispositivo o lo stack dell'app, poi usa Controlla e ricarica.";
    "wa.fix.dns" => "The NAS cannot resolve the name. Check that Avahi runs and that local names are on.", "Il NAS non risolve il nome. Controlla che Avahi sia in esecuzione e che i nomi locali siano attivi.";
    "wa.fix.proxy_running" => "The proxy does not answer. Use Check and load again; if it stays down, look at the log of the rynas-caddy service.", "Il proxy non risponde. Usa Controlla e ricarica; se resta fermo, guarda il registro del servizio rynas-caddy.";
    "wa.fix.route_loaded" => "The running proxy does not have this address yet. Use Check and load again.", "Il proxy in esecuzione non ha ancora questo indirizzo. Usa Controlla e ricarica.";
    "wa.fix.https_listening" => "Nothing answers on the HTTPS port: another program may hold port 443, or the proxy is stopped.", "Sulla porta HTTPS non risponde nulla: un altro programma potrebbe occupare la porta 443, o il proxy è fermo.";
    "wa.fix.certificate" => "The certificate does not cover this name or has expired. Use Check and load again to issue a new one.", "Il certificato non copre questo nome o è scaduto. Usa Controlla e ricarica per emetterne uno nuovo.";
    "wa.fix.upstream" => "The app does not answer on its port. Check on the Docker page that its containers run and finished starting.", "L'app non risponde sulla sua porta. Controlla nella pagina Docker che i suoi container siano in esecuzione e abbiano finito di avviarsi.";
    "tls.title" => "Certificate", "Certificato";
    "tls.open_text" => "The HTTPS certificate of the interface and how devices trust this NAS", "Il certificato HTTPS dell'interfaccia e come i dispositivi si fidano di questo NAS";
    "tls.active" => "Active certificate", "Certificato attivo";
    "tls.source.local" => "Local certificate", "Certificato locale";
    "tls.source.imported" => "Imported certificate", "Certificato importato";
    "tls.https_port" => "Also used by the emergency port {port}", "Usato anche dalla porta di emergenza {port}";
    "tls.subject" => "Subject", "Soggetto";
    "tls.issuer" => "Issued by", "Emesso da";
    "tls.names" => "Names", "Nomi coperti";
    "tls.addresses" => "Addresses", "Indirizzi coperti";
    "tls.valid_from" => "Valid from", "Valido dal";
    "tls.valid_until" => "Valid until", "Valido fino al";
    "tls.days_left" => "{n} days left", "mancano {n} giorni";
    "tls.fingerprint" => "SHA-256 fingerprint", "Impronta SHA-256";
    "tls.key_type" => "Key", "Chiave";
    "tls.chain" => "Intermediate certificates", "Certificati intermedi";
    "tls.local_ca" => "Trust this NAS", "Fidati di questo NAS";
    "tls.no_ca" => "There is no local certificate authority yet.", "Non c'è ancora un'autorità di certificazione locale.";
    "tls.download_ca" => "Download the local CA", "Scarica la CA locale";
    "tls.trust_help" => "Browsers warn about this NAS until a device trusts its local certificate authority. Install it once per device: it covers the interface and every app address, also after a renewal.", "I browser avvisano su questo NAS finché un dispositivo non si fida della sua autorità di certificazione locale. Installala una volta per dispositivo: copre l'interfaccia e tutti gli indirizzi delle app, anche dopo un rinnovo.";
    "tls.trust_check" => "Before installing, compare the fingerprint above with the one your device shows.", "Prima di installarla, confronta l'impronta qui sopra con quella mostrata dal dispositivo.";
    "tls.os.windows" => "Windows", "Windows";
    "tls.os.macos" => "macOS", "macOS";
    "tls.os.ios" => "iPhone and iPad", "iPhone e iPad";
    "tls.os.android" => "Android", "Android";
    "tls.os.linux" => "Linux", "Linux";
    "tls.os.firefox" => "Firefox", "Firefox";
    "tls.trust.windows" => "Open the downloaded file and choose Install Certificate | Store location: Local Machine | Place all certificates in the following store: Trusted Root Certification Authorities | Finish, then restart the browser", "Apri il file scaricato e scegli Installa certificato | Percorso archivio: Computer locale | Colloca tutti i certificati nel seguente archivio: Autorità di certificazione radice attendibili | Fine, poi riavvia il browser";
    "tls.trust.macos" => "Open the downloaded file: Keychain Access adds it | Double-click the certificate (rynas local CA) | Trust: When using this certificate, Always Trust | Close the window and confirm with your password", "Apri il file scaricato: Accesso Portachiavi lo aggiunge | Fai doppio clic sul certificato (rynas local CA) | Fidati: Quando si usa questo certificato, Fidati sempre | Chiudi la finestra e conferma con la password";
    "tls.trust.ios" => "Download the file in Safari and allow the profile | Settings, General, VPN and Device Management: install the downloaded profile | Settings, General, About, Certificate Trust Settings: turn on full trust for rynas local CA", "Scarica il file con Safari e consenti il profilo | Impostazioni, Generali, VPN e gestione dispositivi: installa il profilo scaricato | Impostazioni, Generali, Info, Impostazioni certificati attendibili: attiva l'attendibilità completa per rynas local CA";
    "tls.trust.android" => "Settings, Security and privacy, More security settings, Encryption and credentials (names vary by maker) | Install a certificate, CA certificate, Install anyway | Pick the downloaded file. Browsers trust it; some apps use only their own list", "Impostazioni, Sicurezza e privacy, Altre impostazioni di sicurezza, Crittografia e credenziali (i nomi variano per produttore) | Installa un certificato, Certificato CA, Installa comunque | Scegli il file scaricato. I browser lo accettano; alcune app usano solo il proprio elenco";
    "tls.trust.linux" => "Debian, Ubuntu: sudo cp rynas-local-ca.crt /usr/local/share/ca-certificates/ && sudo update-ca-certificates | Fedora, Arch: sudo trust anchor rynas-local-ca.crt | Chrome on Linux (version 134 and later) keeps its own list: open chrome://certificate-manager, Local certificates, Custom, Installed by you, then Import next to Trusted certificates", "Debian, Ubuntu: sudo cp rynas-local-ca.crt /usr/local/share/ca-certificates/ && sudo update-ca-certificates | Fedora, Arch: sudo trust anchor rynas-local-ca.crt | Chrome su Linux (dalla versione 134) ha un elenco proprio: apri chrome://certificate-manager, Certificati locali, Personalizzati, Installati da te, poi Importa accanto a Certificati attendibili";
    "tls.trust.firefox" => "Settings, Privacy and Security, Certificates, View Certificates | Authorities, Import: pick the downloaded file | Tick Trust this CA to identify websites", "Impostazioni, Privacy e sicurezza, Certificati, Mostra certificati | Autorità, Importa: scegli il file scaricato | Spunta Considera attendibile questa CA per l'identificazione di siti web";
    "tls.import" => "Import for the emergency port", "Importa per la porta di emergenza";
    "tls.import.certificate" => "Certificate (PEM)", "Certificato (PEM)";
    "tls.import.private_key" => "Private key (PEM)", "Chiave privata (PEM)";
    "tls.import.hint" => "Paste or load the full chain, server certificate first, and its private key without a password.", "Incolla o carica la catena completa, con il certificato del server per primo, e la sua chiave privata senza password.";
    "load_file" => "Load from a file", "Carica da un file";
    "tls.use_local" => "Use the local certificate", "Usa il certificato locale";
    "tls.use_local.confirm" => "The imported certificate is set aside and the local one, from this NAS's own authority, is served again.", "Il certificato importato viene messo da parte e torna in uso quello locale, dell'autorità di questo NAS.";
    "tls.warn.expires_soon" => "The certificate expires in less than 30 days.", "Il certificato scade fra meno di 30 giorni.";
    "tls.warn.expired" => "The certificate has expired: browsers refuse it.", "Il certificato è scaduto: i browser lo rifiutano.";
    "tls.warn.names_not_covered" => "Some names and addresses of this NAS are not in the certificate.", "Alcuni nomi e indirizzi di questo NAS non sono nel certificato.";
    "tls.warn.chain_incomplete" => "The intermediate certificates are missing: some devices will not trust it. Import the full chain.", "Mancano i certificati intermedi: alcuni dispositivi non si fideranno. Importa la catena completa.";
    "tls.missing_names" => "Not covered:", "Non coperti:";
    "tls.reload_note" => "The page reconnects with the new certificate; a browser that trusted the old one may ask again.", "La pagina si ricollega con il nuovo certificato; un browser che si fidava del precedente potrebbe chiedere di nuovo.";
    "wa.tab.domains" => "Domains", "Domini";
    "wa.prob.domain_name_too_long" => "The name under the domain would be longer than 253 characters: shorten the app's label or the domain.", "Il nome sotto il dominio supererebbe i 253 caratteri: accorcia l'etichetta dell'app o il dominio.";
    "wa.check.domain_dns" => "Domain name resolves", "Il nome del dominio si risolve";
    "wa.check.domain_certificate" => "Domain certificate", "Certificato del dominio";
    "wa.fix.domain_dns" => "The domain name does not point to this NAS. Open the domain on the Domains tab and run its checks.", "Il nome del dominio non punta a questo NAS. Apri il dominio nella scheda Domini ed esegui i suoi controlli.";
    "wa.fix.domain_certificate" => "The domain certificate is missing or not valid. Open the domain on the Domains tab to see why.", "Il certificato del dominio manca o non è valido. Apri il dominio nella scheda Domini per vedere perché.";
    "dom.title" => "Own domains", "Domini propri";
    "dom.intro" => "Each app gets <app>.<domain> with a trusted certificate from Let's Encrypt, on every device of the network.", "Ogni app riceve <app>.<dominio> con un certificato riconosciuto di Let's Encrypt, su ogni dispositivo della rete.";
    "dom.add" => "Add a domain", "Aggiungi un dominio";
    "dom.add_go" => "Add and apply", "Aggiungi e applica";
    "dom.open" => "Open", "Apri";
    "dom.check_named" => "Check {name}", "Controlla {name}";
    "dom.empty" => "No own domain yet", "Ancora nessun dominio proprio";
    "dom.empty_text" => "With a domain you own (ideally a subdomain such as nas.example.com) the apps open with a trusted certificate, phones and mobile apps included, without opening any port.", "Con un dominio tuo (meglio un sottodominio come nas.example.com) le app si aprono con un certificato riconosciuto, anche da telefoni e app mobili, senza aprire alcuna porta.";
    "dom.external" => "Another reverse proxy serves ports 80 and 443: no certificate is requested here. The LAN records are still written.", "Un altro reverse proxy gestisce le porte 80 e 443: qui non viene richiesto alcun certificato. I record LAN vengono comunque scritti.";
    "dom.col.domain" => "Domain", "Dominio";
    "dom.col.records" => "DNS records", "Record DNS";
    "dom.col.cert" => "Certificate", "Certificato";
    "dom.state.ok" => "Working", "Funziona";
    "dom.gone" => "This domain no longer exists.", "Questo dominio non esiste più.";
    "dom.prob.credential_missing" => "The provider's access values are missing or cannot be read (for example after a restore without its key): enter them again.", "I valori di accesso del provider mancano o non si possono leggere (per esempio dopo un ripristino senza la sua chiave): inseriscili di nuovo.";
    "dom.prob.records_failed" => "The DNS records could not be written. rynas tries again every 10 minutes.", "Non è stato possibile scrivere i record DNS. rynas riprova ogni 10 minuti.";
    "dom.prob.records_manual" => "Some DNS records must be added by hand at the provider: they are listed below.", "Alcuni record DNS vanno aggiunti a mano presso il provider: sono elencati qui sotto.";
    "dom.prob.records_conflict" => "A record rynas did not write holds other values: take the name over below, or fix it at the provider.", "Un record non scritto da rynas contiene altri valori: prendi in carico il nome qui sotto, o correggilo presso il provider.";
    "dom.prob.no_lan_address" => "This NAS has no LAN address to put in the records.", "Questo NAS non ha un indirizzo LAN da mettere nei record.";
    "dom.prob.staging" => "Test certificates are on: browsers do not trust them.", "I certificati di prova sono attivi: i browser non li riconoscono.";
    "dom.ps.credential_missing" => "Token missing", "Token mancante";
    "dom.ps.records_failed" => "Records not written", "Record non scritti";
    "dom.ps.records_manual" => "Records to add by hand", "Record da aggiungere a mano";
    "dom.ps.records_conflict" => "Record to take over", "Record da prendere in carico";
    "dom.ps.no_lan_address" => "No LAN address", "Nessun indirizzo LAN";
    "dom.ps.staging" => "Test certificates on", "Certificati di prova attivi";
    "dom.fix.enter_values" => "Enter the values again", "Inserisci di nuovo i valori";
    "dom.fix.no_staging" => "Use trusted certificates", "Usa certificati riconosciuti";
    "dom.rec.not_yet" => "Not written yet", "Non ancora scritti";
    "dom.rec.off" => "Not managed", "Non gestiti";
    "dom.rec.failed" => "{n} failed", "{n} non riusciti";
    "dom.rec.conflicts" => "{n} to take over", "{n} da prendere in carico";
    "dom.rec.manual" => "To add by hand", "Da aggiungere a mano";
    "dom.rec.ok" => "Up to date", "Aggiornati";
    "dom.rec.text" => "rynas points the domain and every name below it to this NAS's LAN address, at the provider, and keeps them current.", "rynas fa puntare il dominio e ogni nome sotto di esso all'indirizzo LAN di questo NAS, presso il provider, e li tiene aggiornati.";
    "dom.rec.not_yet_text" => "The records are written right after the first apply.", "I record vengono scritti subito dopo la prima applicazione.";
    "dom.rec.off_text" => "rynas does not manage the records of this domain: point them to this NAS yourself.", "rynas non gestisce i record di questo dominio: falli puntare tu a questo NAS.";
    "dom.rec.none" => "No records to write.", "Nessun record da scrivere.";
    "dom.rec.manual_text" => "Add these records at your DNS provider; rynas cannot write them there.", "Aggiungi questi record presso il tuo provider DNS; rynas non può scriverli lì.";
    "dom.rec.write" => "Write records now", "Scrivi ora i record";
    "dom.rec.at" => "Last written {when}", "Ultima scrittura {when}";
    "dom.cert.title" => "Certificates", "Certificati";
    "dom.cert.none" => "No certificate yet: it is requested after the apply and usually arrives within a minute.", "Ancora nessun certificato: viene richiesto dopo l'applicazione e di solito arriva entro un minuto.";
    "dom.test.text" => "Checks the records, name resolution on this NAS and on the internet, router protection against DNS rebinding and the certificate.", "Controlla i record, la risoluzione del nome su questo NAS e su internet, la protezione del router contro il DNS rebinding e il certificato.";
    "dom.ui_urls" => "Interface", "Interfaccia";
    "dom.cert_names" => "Certificate names", "Nomi del certificato";
    "dom.provider" => "DNS provider", "Provider DNS";
    "dom.challenge" => "Validation", "Convalida";
    "dom.settings" => "Settings", "Impostazioni";
    "dom.remove" => "Remove domain", "Rimuovi dominio";
    "dom.remove_go" => "Remove", "Rimuovi";
    "dom.remove_text" => "The names of this domain stop working. rynas deletes the records it wrote, only while they still hold its values; the credential stays.", "I nomi di questo dominio smettono di funzionare. rynas elimina i record che ha scritto, solo se contengono ancora i suoi valori; la credenziale resta.";
    "dom.take.title" => "Take over {name}?", "Prendere in carico {name}?";
    "dom.take.text" => "This record was not written by rynas and now holds {now}. rynas replaces it with {wanted} and keeps it current from now on.", "Questo record non è stato scritto da rynas e ora contiene {now}. rynas lo sostituisce con {wanted} e da ora lo tiene aggiornato.";
    "dom.form.intro" => "Four steps: the provider that hosts the domain's DNS, an access token, the domain, then a preview of what changes. Nothing is written before you add it.", "Quattro passi: il provider che gestisce il DNS del dominio, un token di accesso, il dominio, poi un'anteprima di cosa cambia. Nulla viene scritto prima di aggiungerlo.";
    "dom.step.token" => "Access to the provider", "Accesso al provider";
    "dom.step.domain" => "The domain and how it is validated", "Il dominio e come viene convalidato";
    "dom.step.records" => "What changes", "Cosa cambia";
    "dom.p.records" => "rynas writes the LAN records", "rynas scrive i record LAN";
    "dom.p.manual" => "LAN records added by hand", "Record LAN da aggiungere a mano";
    "dom.p.per_name" => "One record per name", "Un record per nome";
    "dom.p.unscoped" => "Tokens of this provider cannot be limited to one domain. Prefer CNAME delegation, so the token never touches the main zone.", "I token di questo provider non si possono limitare a un dominio. Preferisci la delega CNAME, così il token non tocca mai la zona principale.";
    "dom.p.duckdns" => "DuckDNS keeps one address per name: rynas sets it to this NAS's LAN address on every sync, replacing a public address another updater keeps there.", "DuckDNS tiene un solo indirizzo per nome: rynas lo imposta all'indirizzo LAN di questo NAS a ogni sincronizzazione, sostituendo un indirizzo pubblico tenuto lì da un altro aggiornatore.";
    "dom.f.optional" => "{name} (optional)", "{name} (facoltativo)";
    "dom.f.never_shown" => "Values are stored encrypted and never shown again.", "I valori vengono conservati cifrati e non vengono più mostrati.";
    "dom.name" => "Domain", "Dominio";
    "dom.err.name" => "This domain cannot be used", "Questo dominio non si può usare";
    "dom.name_help" => "A name you own, ideally a subdomain used only for this NAS. DuckDNS: <name>.duckdns.org.", "Un nome tuo, meglio un sottodominio usato solo per questo NAS. DuckDNS: <nome>.duckdns.org.";
    "dom.ch.dns01" => "DNS (recommended)", "DNS (consigliata)";
    "dom.ch.cname_delegation" => "CNAME delegation", "Delega CNAME";
    "dom.ch.http01" => "HTTP", "HTTP";
    "dom.ch.dns01_help" => "The provider's API proves the domain is yours. One wildcard certificate, no open port.", "L'API del provider dimostra che il dominio è tuo. Un solo certificato wildcard, nessuna porta aperta.";
    "dom.ch.cname_delegation_help" => "_acme-challenge of the domain points by CNAME to another zone that the token can write. The main zone stays out of reach; its records are added by hand.", "_acme-challenge del dominio punta con un CNAME a un'altra zona che il token può scrivere. La zona principale resta fuori portata; i suoi record vanno aggiunti a mano.";
    "dom.ch.http01_help" => "Port 80 must be reachable from the internet. One certificate per name, and app names become public in the certificate logs.", "La porta 80 deve essere raggiungibile da internet. Un certificato per nome, e i nomi delle app diventano pubblici nei registri dei certificati.";
    "dom.delegate_to" => "CNAME target", "Destinazione del CNAME";
    "dom.delegate_to_help" => "The name _acme-challenge.<domain> points to (with acme-dns, the account's full domain).", "Il nome a cui punta _acme-challenge.<dominio> (con acme-dns, il dominio completo dell'account).";
    "dom.wildcard" => "Wildcard certificate", "Certificato wildcard";
    "dom.wildcard_help" => "One certificate for *.<domain> and <domain>: app names stay out of the public certificate logs.", "Un certificato per *.<dominio> e <dominio>: i nomi delle app restano fuori dai registri pubblici dei certificati.";
    "dom.lan_record" => "Write the LAN records", "Scrivi i record LAN";
    "dom.lan_record_help" => "<domain> and *.<domain> point to this NAS's LAN address, so every device finds the apps.", "<dominio> e *.<dominio> puntano all'indirizzo LAN di questo NAS, così ogni dispositivo trova le app.";
    "dom.lan_ipv6" => "Also IPv6 records", "Anche record IPv6";
    "dom.lan_ipv6_help" => "Only with a stable IPv6 address on the LAN interface.", "Solo con un indirizzo IPv6 stabile sull'interfaccia LAN.";
    "dom.ui" => "Interface on the domain itself", "Interfaccia sul dominio stesso";
    "dom.ui_help" => "https://<domain> opens this interface.", "https://<dominio> apre questa interfaccia.";
    "dom.enabled" => "Active", "Attivo";
    "dom.enabled_help" => "Turned off, the names stop working and rynas deletes the records it wrote.", "Se disattivato, i nomi smettono di funzionare e rynas elimina i record che ha scritto.";
    "dom.plan.text" => "rynas reads the provider (nothing is written) and lists the records the save would create or change.", "rynas legge il provider (nulla viene scritto) ed elenca i record che il salvataggio creerebbe o cambierebbe.";
    "dom.plan.go" => "Preview the DNS changes", "Anteprima delle modifiche DNS";
    "dom.plan.conflict" => "Some names already hold records rynas did not write. They stay as they are unless you tick Replace.", "Alcuni nomi hanno già record non scritti da rynas. Restano come sono, a meno che tu non spunti Sostituisci.";
    "dom.cred.title" => "Provider credentials", "Credenziali dei provider";
    "dom.cred.add" => "Add a credential", "Aggiungi una credenziale";
    "dom.cred.edit" => "Edit credential", "Modifica credenziale";
    "dom.cred.empty" => "None yet: the first one is created with a domain.", "Ancora nessuna: la prima viene creata con un dominio.";
    "dom.cred.stored" => "Stored: {fields}", "Conservati: {fields}";
    "dom.cred.no_values" => "No values stored", "Nessun valore conservato";
    "dom.cred.used" => "Domains using it: {n}", "Domini che la usano: {n}";
    "dom.cred.in_use" => "A domain uses it", "È usata da un dominio";
    "dom.cred.remove" => "Remove credential", "Rimuovi credenziale";
    "dom.cred.remove_text" => "The stored values are deleted from this NAS. The token stays valid at the provider: revoke it there too.", "I valori conservati vengono eliminati da questo NAS. Il token resta valido presso il provider: revocalo anche lì.";
    "dom.cred.label" => "Name (optional)", "Nome (facoltativo)";
    "dom.cred.label_help" => "To recognize it in the list, for example \"token for example.com\".", "Per riconoscerla nell'elenco, per esempio \"token per example.com\".";
    "dom.cred.keep" => "Leave the fields empty to keep the stored values; to change them, enter them all again.", "Lascia vuoti i campi per tenere i valori conservati; per cambiarli, inseriscili di nuovo tutti.";
    "dom.cred.pick" => "Credential", "Credenziale";
    "dom.cred.new" => "New credential", "Nuova credenziale";
    "dom.cred.replace" => "Enter new values", "Inserisci nuovi valori";
    "dom.acme.title" => "Let's Encrypt account", "Account Let's Encrypt";
    "dom.acme.text" => "Shared by every domain. Certificates renew on their own, well before they expire.", "Comune a tutti i domini. I certificati si rinnovano da soli, molto prima della scadenza.";
    "dom.acme.email" => "Contact e-mail", "E-mail di contatto";
    "dom.acme.email_help" => "Optional: the certificate authority may write about problems with the account.", "Facoltativa: l'autorità di certificazione può scrivere in caso di problemi con l'account.";
    "dom.acme.staging" => "Test certificates", "Certificati di prova";
    "dom.acme.staging_help" => "For trying a setup without hitting the rate limits. Browsers do not trust them: turn it off afterwards.", "Per provare una configurazione senza raggiungere i limiti di richieste. I browser non li riconoscono: disattivalo dopo.";
    "dom.acme.profile" => "Certificate profile", "Profilo del certificato";
    "dom.acme.profile_default" => "Default of the authority", "Predefinito dell'autorità";
    "dom.acme.profile_tlsserver" => "TLS server (shorter, web only)", "Server TLS (più breve, solo web)";
    "dom.acme.profile_classic" => "Classic", "Classico";
    "dom.check.dns_record" => "DNS records written", "Record DNS scritti";
    "dom.check.dns_nas" => "Name resolves on this NAS", "Il nome si risolve su questo NAS";
    "dom.check.dns_public" => "Name resolves on the internet", "Il nome si risolve su internet";
    "dom.check.rebind" => "Router lets the answer through", "Il router lascia passare la risposta";
    "dom.check.certificate" => "Certificate obtained", "Certificato ottenuto";
    "dom.check.renewal" => "Renewal", "Rinnovo";
    "dom.check.hairpin" => "Name points to the LAN", "Il nome punta alla LAN";
    "dom.fix.dns_record" => "Look at the DNS records above: write them again, take over a name, or add the listed ones by hand.", "Guarda i record DNS qui sopra: scrivili di nuovo, prendi in carico un nome o aggiungi a mano quelli elencati.";
    "dom.fix.dns_nas" => "This NAS does not resolve the name to its LAN address yet. Changes at the provider can take a few minutes: try again shortly.", "Questo NAS non risolve ancora il nome nel suo indirizzo LAN. Le modifiche presso il provider possono richiedere qualche minuto: riprova fra poco.";
    "dom.fix.dns_public" => "Public resolvers do not see the record yet, or outbound DNS is blocked on this network.", "I resolver pubblici non vedono ancora il record, oppure il DNS in uscita è bloccato su questa rete.";
    "dom.fix.rebind" => "The router drops public answers that point to private addresses (DNS rebinding protection). Allow this domain there; the detail says how for common routers.", "Il router scarta le risposte pubbliche che puntano a indirizzi privati (protezione dal DNS rebinding). Consenti lì questo dominio; il dettaglio spiega come per i router più comuni.";
    "dom.fix.certificate" => "The certificate is not there yet. Check the token's permissions and the engine's error in the detail; test certificates are not trusted.", "Il certificato non c'è ancora. Controlla i permessi del token e l'errore del motore nel dettaglio; i certificati di prova non sono riconosciuti.";
    "dom.fix.renewal" => "Renewal is late or failing: usually the token was revoked or lost its permissions. Enter new values for the credential.", "Il rinnovo è in ritardo o fallisce: di solito il token è stato revocato o ha perso i permessi. Inserisci nuovi valori per la credenziale.";
    "dom.fix.hairpin" => "The name points to a public address: devices of the network then depend on the router forwarding back to the NAS. Turn on Write the LAN records.", "Il nome punta a un indirizzo pubblico: i dispositivi della rete dipendono allora dal router che rimanda al NAS. Attiva Scrivi i record LAN.";
    "dns.col.name" => "Name", "Nome";
    "dns.col.type" => "Type", "Tipo";
    "dns.col.action" => "Change", "Modifica";
    "dns.col.value" => "Value", "Valore";
    "dns.act.keep" => "Already right", "Già corretto";
    "dns.act.create" => "Create", "Da creare";
    "dns.act.update" => "Update", "Da aggiornare";
    "dns.act.delete" => "Delete", "Da eliminare";
    "dns.act.manual" => "Add by hand", "Da aggiungere a mano";
    "dns.act.unknown" => "Not known", "Non noto";
    "dns.act.conflict" => "Not written by rynas", "Non scritto da rynas";
    "dns.act.failed" => "Failed", "Non riuscito";
    "dns.copy" => "Copy", "Copia";
    "dns.copy_all" => "Copy all", "Copia tutti";
    "dns.take_over" => "Replace", "Sostituisci";
    "dns.take_over_btn" => "Take over", "Prendi in carico";
    "dns.f.api_token" => "API token", "Token API";
    "dns.f.token" => "Token", "Token";
    "dns.f.api_key" => "API key", "Chiave API";
    "dns.f.api_secret_key" => "Secret API key", "Chiave API segreta";
    "dns.f.endpoint" => "API endpoint", "Endpoint API";
    "dns.f.application_key" => "Application key", "Chiave dell'applicazione";
    "dns.f.application_secret" => "Application secret", "Segreto dell'applicazione";
    "dns.f.consumer_key" => "Consumer key", "Chiave consumer";
    "dns.f.bearer_token" => "Personal access token", "Token di accesso personale";
    "dns.f.access_key_id" => "Access key ID", "ID della chiave di accesso";
    "dns.f.secret_access_key" => "Secret access key", "Chiave di accesso segreta";
    "dns.f.region" => "Region", "Regione";
    "dns.f.auth_token" => "API access token", "Token di accesso API";
    "dns.f.server" => "Primary DNS server", "Server DNS primario";
    "dns.f.key_name" => "TSIG key name", "Nome della chiave TSIG";
    "dns.f.key_alg" => "TSIG algorithm", "Algoritmo TSIG";
    "dns.f.key" => "TSIG key", "Chiave TSIG";
    "dns.f.server_url" => "Server URL", "URL del server";
    "dns.f.username" => "Account user name", "Nome utente dell'account";
    "dns.f.subdomain" => "Subdomain", "Sottodominio";
    "dns.f.password" => "Password", "Password";
    "cert.inv.title" => "All certificates", "Tutti i certificati";
    "cert.inv.text" => "Every certificate this NAS serves. Trouble with one becomes a notification.", "Ogni certificato servito da questo NAS. Un problema con uno di essi diventa una notifica.";
    "cert.inv.at" => "Read {when}", "Letto {when}";
    "cert.col.cert" => "Certificate", "Certificato";
    "cert.col.renew" => "Renews from", "Rinnovo dal";
    "cert.kind.local_ca" => "Local authority", "Autorità locale";
    "cert.kind.interface" => "Interface (emergency port)", "Interfaccia (porta di emergenza)";
    "cert.kind.proxy_local" => "Local names", "Nomi locali";
    "cert.kind.acme" => "Let's Encrypt", "Let's Encrypt";
    "cert.kind.imported" => "Imported", "Importato";
    "wa.import.title" => "Imported certificates", "Certificati importati";
    "wa.import.add" => "Import a certificate", "Importa un certificato";
    "wa.import.intro" => "A certificate made elsewhere (a router's ACME client, a company authority, a bought wildcard) serves the interface and app addresses it covers.", "Un certificato creato altrove (il client ACME di un router, un'autorità aziendale, un wildcard acquistato) serve gli indirizzi dell'interfaccia e delle app che copre.";
    "wa.import.empty" => "No imported certificate. Addresses use the Let's Encrypt or the local certificate.", "Nessun certificato importato. Gli indirizzi usano il certificato Let's Encrypt o quello locale.";
    "wa.import.id" => "Name (short id)", "Nome (id breve)";
    "wa.import.id_help" => "Lowercase letters, digits and dashes, at most 32. It also names the watched folder.", "Lettere minuscole, cifre e trattini, al massimo 32. Dà il nome anche alla cartella controllata.";
    "wa.import.label" => "Description", "Descrizione";
    "wa.import.source" => "How the files arrive", "Come arrivano i file";
    "wa.import.source.upload" => "Upload the files", "Carica i file";
    "wa.import.source.folder" => "Another tool pushes them into a folder", "Un altro programma li deposita in una cartella";
    "wa.import.certificate" => "Certificate chain (PEM: server certificate first, then intermediates)", "Catena del certificato (PEM: prima il certificato del server, poi gli intermedi)";
    "wa.import.private_key" => "Private key (PEM, unencrypted)", "Chiave privata (PEM, non cifrata)";
    "wa.import.inspect" => "Check", "Verifica";
    "wa.import.covers" => "Takes over these addresses", "Usato per questi indirizzi";
    "wa.import.covers_none" => "It covers none of the addresses this NAS serves", "Non copre nessuno degli indirizzi serviti da questo NAS";
    "wa.import.shadowed" => "Served by another imported certificate", "Serviti da un altro certificato importato";
    "wa.import.replaces_acme" => "Replaces the Let's Encrypt certificate for", "Sostituisce il certificato Let's Encrypt per";
    "wa.import.folder" => "Watched folder", "Cartella controllata";
    "wa.import.folder_help" => "Write fullchain.pem and privkey.pem here; rynas checks them and reloads within seconds. Members of the group rynas-cert-drop can write here.", "Scrivi qui fullchain.pem e privkey.pem: rynas li controlla e ricarica in pochi secondi. Possono scrivere qui i membri del gruppo rynas-cert-drop.";
    "wa.import.folder_ssh" => "Example: a push account over SSH (acme.sh)", "Esempio: un account di invio via SSH (acme.sh)";
    "wa.import.ssh_note" => "These lines are not applied by rynas: the SSH server configuration stays yours. Replace <name> with the certificate's name.", "Queste righe non vengono applicate da rynas: la configurazione del server SSH resta tua. Sostituisci <name> con il nome del certificato.";
    "wa.import.check_now" => "Check the folder now", "Controlla ora la cartella";
    "wa.import.waiting" => "Waiting for the files", "In attesa dei file";
    "wa.import.last_error" => "Last files refused", "Ultimi file rifiutati";
    "wa.import.kept" => "The previous certificate stays in use.", "Resta in uso il certificato precedente.";
    "wa.import.imported_at" => "Taken on", "Importato il";
    "wa.import.replace" => "Replace the files", "Sostituisci i file";
    "wa.import.saved" => "Certificate saved; the proxy uses it within seconds.", "Certificato salvato; il proxy lo usa entro pochi secondi.";
    "wa.import.delete" => "Remove", "Rimuovi";
    "wa.import.delete_confirm" => "Its addresses go back to the Let's Encrypt or local certificate.", "I suoi indirizzi tornano al certificato Let's Encrypt o locale.";
    "wa.import.not_renewed" => "rynas does not renew imported certificates.", "rynas non rinnova i certificati importati.";
    "nev.web_access.cert_import_failed.title" => "Certificate files for {name} were refused", "I file del certificato per {name} sono stati rifiutati";
    "nev.web_access.cert_import_failed.body" => "The previous certificate stays in use. {error}", "Resta in uso il certificato precedente. {error}";
    "dom.step.provider" => "Who hosts the domain's DNS", "Chi gestisce il DNS del dominio";
    "dns.now_label" => "Now:", "Ora:";
    "dns.why.failed" => "The provider refused the change; rynas tries again in 10 minutes.", "Il provider ha rifiutato la modifica; rynas riprova fra 10 minuti.";
    "dns.why.conflict" => "Holds values rynas did not write: nothing is written until you take the name over.", "Contiene valori non scritti da rynas: nulla viene scritto finché non prendi in carico il nome.";
    "dns.why.conflict_unwanted" => "No longer needed, but changed since rynas wrote it: left as it is and no longer managed.", "Non serve più, ma è cambiato da quando rynas lo ha scritto: resta com'è e non è più gestito.";
    "dns.why.unknown" => "The provider could not be read.", "Non è stato possibile leggere il provider.";
    "dns.hint.cloudflare" => "Create an API token (My Profile → API Tokens) with the permission Zone → DNS → Edit, limited to this one zone (Zone Resources: Include → Specific zone). Never the Global API Key.", "Crea un token API (My Profile → API Tokens) con il permesso Zone → DNS → Edit, limitato a questa sola zona (Zone Resources: Include → Specific zone). Mai la Global API Key.";
    "dns.hint.desec" => "Create a token whose policy allows writes only to this domain (optionally only to *.<domain>, <domain> and _acme-challenge.<domain>), optionally limited to your public address.", "Crea un token la cui policy consenta scritture solo su questo dominio (facoltativamente solo su *.<dominio>, <dominio> e _acme-challenge.<dominio>), facoltativamente limitato al tuo indirizzo pubblico.";
    "dns.hint.hetzner" => "Use a Hetzner Console (Cloud API) token with Read & Write permission; tokens of the old DNS console no longer work. A token covers a whole project: keep the zone in a project of its own.", "Usa un token della Hetzner Console (Cloud API) con permesso Read & Write; i token della vecchia console DNS non funzionano più. Un token vale per tutto un progetto: tieni la zona in un progetto a parte.";
    "dns.hint.porkbun" => "Create an API key, turn on API access for this domain only (Domain Management → Details → API Access) and limit the key to DNS if offered.", "Crea una chiave API, attiva l'accesso API solo per questo dominio (Domain Management → Details → API Access) e limita la chiave al DNS se possibile.";
    "dns.hint.ovh" => "Create an application and a consumer key whose rights allow only GET/POST/PUT/DELETE on /domain/zone/<zone>/* (and GET /domain/zone).", "Crea un'applicazione e una consumer key i cui diritti consentano solo GET/POST/PUT/DELETE su /domain/zone/<zona>/* (e GET /domain/zone).";
    "dns.hint.duckdns" => "The account token covers all your DuckDNS names. The domain is <name>.duckdns.org and every name below it gets the same address.", "Il token dell'account vale per tutti i tuoi nomi DuckDNS. Il dominio è <nome>.duckdns.org e ogni nome sotto di esso riceve lo stesso indirizzo.";
    "dns.hint.gandi" => "Create a personal access token limited to this domain with the permission to manage its technical configuration.", "Crea un token di accesso personale limitato a questo dominio con il permesso di gestirne la configurazione tecnica.";
    "dns.hint.route53" => "Create an IAM user whose policy allows route53:ChangeResourceRecordSets and ListResourceRecordSets on this one hosted zone, plus route53:ListHostedZonesByName and GetChange.", "Crea un utente IAM la cui policy consenta route53:ChangeResourceRecordSets e ListResourceRecordSets su questa sola hosted zone, più route53:ListHostedZonesByName e GetChange.";
    "dns.hint.digitalocean" => "Tokens cannot be limited to one domain: prefer CNAME delegation of _acme-challenge to a zone used only for that.", "I token non si possono limitare a un dominio: preferisci la delega CNAME di _acme-challenge a una zona usata solo per questo.";
    "dns.hint.rfc2136" => "Use a TSIG key whose update policy allows only TXT records of _acme-challenge.<domain>.", "Usa una chiave TSIG la cui update policy consenta solo i record TXT di _acme-challenge.<dominio>.";
    "dns.hint.acmedns" => "Register an account on the acme-dns server and add the CNAME _acme-challenge.<domain> → <fulldomain> once. The account can change only that one TXT record.", "Registra un account sul server acme-dns e aggiungi una volta il CNAME _acme-challenge.<dominio> → <fulldomain>. L'account può cambiare solo quel record TXT.";
    "wa.import.names" => "Names it may carry", "Nomi ammessi";
    "wa.import.names_help" => "A pushed certificate is used only for exactly these names; leave empty to take the names of the first certificate pushed.", "Un certificato depositato viene usato solo per esattamente questi nomi; lascia vuoto per prendere i nomi del primo certificato depositato.";
    "wa.import.names_why" => "Recommended: every account that can push into one folder can push into all of them, so the names decide what a push may change. A certificate for other names waits for your approval.", "Consigliato: ogni account che può depositare in una cartella può farlo in tutte, quindi sono i nomi a decidere cosa può cambiare un deposito. Un certificato per altri nomi attende la tua approvazione.";
    "wa.import.problem.refused" => "The last files were refused", "Gli ultimi file sono stati rifiutati";
    "wa.import.problem.names_changed" => "A new certificate for other names is waiting for your approval", "Un nuovo certificato per altri nomi attende la tua approvazione";
    "wa.import.problem.not_yet_valid" => "A new certificate starts {when}; the current one stays in use until then", "Un nuovo certificato entra in vigore {when}; fino ad allora resta in uso quello attuale";
    "wa.import.problem.group_missing" => "The group rynas-cert-drop is missing: no account can write into the folder. Reinstall the rynas package.", "Manca il gruppo rynas-cert-drop: nessun account può scrivere nella cartella. Reinstalla il pacchetto rynas.";
    "wa.import.pending_names" => "Names of the new certificate", "Nomi del nuovo certificato";
    "wa.import.current_names" => "Names in use", "Nomi in uso";
    "wa.import.accept" => "Accept the new names", "Accetta i nuovi nomi";
    "wa.import.accept_confirm" => "The new certificate takes over these addresses at once. Accept only if you expected the change.", "Il nuovo certificato prende subito questi indirizzi. Accetta solo se ti aspettavi il cambiamento.";
    "wa.import.private_ca" => "Self-signed or from a private authority: devices trust it only once that authority is installed on them.", "Autofirmato o di un'autorità privata: i dispositivi lo riconoscono solo dopo averci installato quell'autorità.";
    "wa.import.code.account" => "On the NAS: the push account (once, as root)", "Sul NAS: l'account di invio (una volta, come root)";
    "wa.import.code.sshd" => "On the NAS: add to /etc/ssh/sshd_config, then reload ssh", "Sul NAS: aggiungi a /etc/ssh/sshd_config, poi ricarica ssh";
    "wa.import.code.acme" => "On the router or machine running acme.sh", "Sul router o sulla macchina che esegue acme.sh";
    "wa.import.err.certificate" => "The certificate chain was refused", "La catena del certificato è stata rifiutata";
    "wa.import.err.private_key" => "The private key was refused", "La chiave privata è stata rifiutata";
    "nev.web_access.cert_import_names_changed.title" => "The certificate pushed for {name} is for other names", "Il certificato depositato per {name} è per altri nomi";
    "nev.web_access.cert_import_names_changed.body" => "It is for {names}. The previous certificate stays in use until you accept the new names on Network › Web access › Certificate.", "È per {names}. Resta in uso il certificato precedente finché non accetti i nuovi nomi in Rete › Accesso web › Certificato.";
    "nev.web_access.cert_import_ok.title" => "Certificate files for {name} are fine again", "I file del certificato per {name} sono di nuovo a posto";
    "cert.staging" => "Test", "Prova";
    "cert.st.ok" => "Valid", "Valido";
    "cert.st.expires_soon" => "Expires soon", "Scade presto";
    "cert.st.expired" => "Expired", "Scaduto";
    "cert.st.renewal_overdue" => "Renewal failing", "Rinnovo non riuscito";
    "cert.st.pending" => "Being obtained", "In arrivo";
    "cert.st.failed" => "Not obtained", "Non ottenuto";
    "nev.web_access.cert_expires_soon.title" => "Certificate for {name} expires soon", "Il certificato per {name} scade presto";
    "nev.web_access.cert_expires_soon.body" => "It expires within 14 days. Open Network › Web access › Certificate to see why it was not renewed.", "Scade entro 14 giorni. Apri Rete › Accesso web › Certificato per vedere perché non è stato rinnovato.";
    "nev.web_access.cert_renewal_overdue.title" => "Certificate for {name} is not renewing", "Il certificato per {name} non si rinnova";
    "nev.web_access.cert_renewal_overdue.body" => "Renewal is past due. Check the domain's token on Network › Web access › Domains.", "Il rinnovo è in ritardo. Controlla il token del dominio in Rete › Accesso web › Domini.";
    "nev.web_access.cert_failed.title" => "Certificate for {name} could not be obtained", "Non è stato possibile ottenere il certificato per {name}";
    "nev.web_access.cert_failed.body" => "Open the domain on Network › Web access › Domains and run its checks.", "Apri il dominio in Rete › Accesso web › Domini ed esegui i suoi controlli.";
    "nev.web_access.cert_expired.title" => "Certificate for {name} has expired", "Il certificato per {name} è scaduto";
    "nev.web_access.cert_expired.body" => "Browsers refuse the names it covers. Open Network › Web access › Domains to fix it.", "I browser rifiutano i nomi che copre. Apri Rete › Accesso web › Domini per correggerlo.";
    "nev.web_access.cert_ok.title" => "Certificate for {name} is fine again", "Il certificato per {name} è di nuovo a posto";
    "docker.install_incomplete" => "A previous Docker install did not finish: {n} packages are half-installed.", "Un'installazione precedente di Docker non è terminata: {n} pacchetti sono installati a metà.";
    "docker.install_incomplete_hint" => "Finishing repairs them and completes the install.", "Il completamento li ripara e termina l'installazione.";
    "docker.install_unfinished" => "The Docker install rynas started is missing its last steps (the version pin or the docker-apps account).", "All'installazione di Docker avviata da rynas mancano gli ultimi passi (il blocco della versione o l'account docker-apps).";
    "docker.install_finish" => "Finish the Docker install", "Completa l'installazione di Docker";
    "apps.convert" => "Convert to custom stack…", "Converti in stack personalizzato…";
    "apps.convert_go" => "Convert", "Converti";
    "apps.conv.title" => "Convert {name} to a custom stack", "Converti {name} in uno stack personalizzato";
    "apps.conv.text" => "The app becomes an ordinary compose stack that you manage yourself.", "L'app diventa un normale stack compose che gestisci tu.";
    "apps.conv.p1" => "Containers, data and files stay as they are; nothing restarts.", "Container, dati e file restano come sono; nulla si riavvia.";
    "apps.conv.p2" => "It leaves this list: no more catalogue updates, migrations or app rollbacks.", "Esce da questo elenco: niente più aggiornamenti del catalogo, migrazioni o ripristini dell'app.";
    "apps.conv.p3" => "The Docker page keeps editing, updating and snapshotting it. This cannot be undone from here.", "La pagina Docker continua a modificarlo, aggiornarlo e farne istantanee. Da qui non si può annullare.";
    "apps.conv.done" => "Converted to a custom stack.", "Convertita in stack personalizzato.";
    "apps.uninstall" => "Uninstall…", "Disinstalla…";
    "apps.uninstall_go" => "Uninstall", "Disinstalla";
    "apps.un.title" => "Uninstall {name}", "Disinstalla {name}";
    "apps.un.text" => "The containers are removed. The stack folder, the data and the snapshots stay on disk, so the app can be started again later.", "I container vengono rimossi. La cartella dello stack, i dati e le istantanee restano sul disco, così l'app si può riavviare in seguito.";
    "apps.un.delete" => "Also delete the data", "Elimina anche i dati";
    "apps.un.delete_text" => "Deletes the stack folder, its snapshots and the folders the app placed elsewhere. Shared folders are never touched.", "Elimina la cartella dello stack, le sue istantanee e le cartelle che l'app ha messo altrove. Le cartelle condivise non vengono mai toccate.";
    "apps.rb.text" => "The app goes back to the snapshot of {when}: files, data in the stack folder and image versions. A snapshot of the current state is taken first.", "L'app torna all'istantanea del {when}: file, dati nella cartella dello stack e versioni delle immagini. Prima viene creata un'istantanea dello stato attuale.";
    "apps.upd.app_version" => "App version", "Versione dell'app";
    "apps.upd.pkg_version" => "Catalogue package", "Pacchetto del catalogo";
    "apps.upd.images" => "Image changes", "Modifiche alle immagini";
    "apps.upd.migrations" => "Setting migrations", "Migrazioni delle impostazioni";
    "apps.upd.migrations_text" => "The stored answers are adapted for versions {list}.", "Le risposte salvate vengono adattate per le versioni {list}.";
    "apps.upd.answers" => "New questions", "Nuove domande";
    "apps.upd.answers_text" => "This version asks for values the app did not have before.", "Questa versione chiede valori che l'app prima non aveva.";
    "apps.upd.snapshot_help" => "Taken before anything changes; a failed update goes back to it by itself, and Roll back restores it later.", "Creato prima di ogni modifica; un aggiornamento fallito ci torna da solo e Ripristina lo riporta in seguito.";
    "apps.upd.no_snapshot" => "The stack folder is not a btrfs subvolume: no snapshot is taken. A failed update puts the previous files back.", "La cartella dello stack non è un sottovolume btrfs: nessuna istantanea. Un aggiornamento fallito rimette i file precedenti.";
    "apps.upd.up_to_date" => "The app is up to date.", "L'app è aggiornata.";
    "apps.upd.downgrade" => "The catalogue has an older version than the installed one: the update is refused.", "Il catalogo ha una versione più vecchia di quella installata: l'aggiornamento viene rifiutato.";
    "apps.upd.none" => "Everything is up to date", "Tutto è aggiornato";
    "apps.upd.none_text" => "New catalogue versions of the installed apps show up here.", "Le nuove versioni del catalogo per le app installate compaiono qui.";
    "apps.upd.count" => "Updates available: {n}", "Aggiornamenti disponibili: {n}";
    "apps.src.title" => "Catalogue source", "Origine del catalogo";
    "apps.src.checked" => "App catalogue checked", "Catalogo delle app controllato";
    "apps.src.source" => "In use", "In uso";
    "apps.src.online" => "Online catalogue {serial} ({n} newer entries)", "Catalogo online {serial} ({n} voci più recenti)";
    "apps.src.shipped_only" => "Shipped with rynas", "Fornito con rynas";
    "apps.src.shipped" => "Shipped catalogue serial", "Numero del catalogo fornito";
    "apps.src.published" => "Published", "Pubblicato";
    "apps.src.last_check" => "Last check", "Ultimo controllo";
    "apps.src.never" => "Never", "Mai";
    "apps.src.no_key" => "Online catalogue updates are not available in this build yet: the catalogue comes with rynas updates.", "Gli aggiornamenti online del catalogo non sono ancora disponibili in questa versione: il catalogo arriva con gli aggiornamenti di rynas.";
    "apps.src.off" => "Online catalogue updates are turned off in the Docker settings.", "Gli aggiornamenti online del catalogo sono disattivati nelle impostazioni di Docker.";
    "apps.src.expired" => "The online catalogue has not been renewed in time: rynas keeps using it, but no newer catalogue arrived.", "Il catalogo online non è stato rinnovato in tempo: rynas continua a usarlo, ma non è arrivato un catalogo più recente.";
    "apps.src.error" => "The last check failed; the catalogue in use stays:", "L'ultimo controllo non è riuscito; il catalogo in uso resta:";
    "apps.upd.pick" => "Select", "Seleziona";
    "apps.upd.selected" => "Update selected", "Aggiorna le selezionate";
    "apps.upd.many_text" => "One after the other, each with a snapshot first when its folder allows it: {names}.", "Una dopo l'altra, ciascuna con un'istantanea prima quando la sua cartella lo consente: {names}.";
    "apps.upd.skipped" => "Need answers, update them one by one: {names}", "Richiedono risposte, aggiornale una per una: {names}";
    "apps.upd.failed" => "Not updated: {names}", "Non aggiornate: {names}";
    "apps.upd.all_done" => "The selected apps are updated.", "Le app selezionate sono aggiornate.";
    "apps.chg.added" => "Added", "Aggiunta";
    "apps.chg.removed" => "Removed", "Rimossa";
    "apps.chg.digest" => "Rebuilt image", "Immagine ricostruita";
    "sf.advanced" => "Advanced options", "Opzioni avanzate";
    "sf.optional" => "(optional)", "(facoltativo)";
    "sf.none" => "None", "Nessuno";
    "sf.no_questions" => "This app asks no questions.", "Questa app non fa domande.";
    "sf.when_on" => "When on:", "Se attivo:";
    "sf.when_off" => "When off:", "Se disattivo:";
    "sf.err.required" => "Required.", "Obbligatorio.";
    "sf.err.enum" => "Choose one of the listed values.", "Scegli uno dei valori elencati.";
    "sf.err.integer" => "Enter a whole number.", "Inserisci un numero intero.";
    "sf.err.min" => "At least {n}.", "Almeno {n}.";
    "sf.err.max" => "At most {n}.", "Al massimo {n}.";
    "sf.err.min_len" => "At least {n} characters.", "Almeno {n} caratteri.";
    "sf.err.max_len" => "At most {n} characters.", "Al massimo {n} caratteri.";
    "sf.err.pattern" => "Not in the expected format.", "Non è nel formato previsto.";
    "sf.err.hostname" => "Enter a host name, e.g. nas.example.lan.", "Inserisci un nome host, ad es. nas.example.lan.";
    "sf.err.uri" => "Enter an address starting with http:// or https://.", "Inserisci un indirizzo che inizia con http:// o https://.";
    "sf.err.email" => "Enter an e-mail address.", "Inserisci un indirizzo e-mail.";
    // PC backup receive (Backup › PCs, bk.*) and its tasks and events
    "bk.tab.pcs" => "PCs", "PC";
    "bk.intro" => "Linux PCs with btrfs send their snapshots here with btrbk over SSH. Each PC gets its own folder and its own key, and its key can only receive into that folder.", "I PC Linux con btrfs mandano qui i loro snapshot con btrbk via SSH. Ogni PC ha la sua cartella e la sua chiave, e la sua chiave può solo ricevere in quella cartella.";
    "bkpc.empty" => "No PCs yet", "Ancora nessun PC";
    "bkpc.empty_text" => "Add a PC to get a folder and the lines for its btrbk configuration, or adopt a folder that already holds received snapshots.", "Aggiungi un PC per avere una cartella e le righe per la sua configurazione di btrbk, oppure adotta una cartella che contiene già snapshot ricevuti.";
    "bk.add" => "Add a PC", "Aggiungi un PC";
    "bk.add_title" => "New backup target", "Nuova destinazione di backup";
    "bk.adopt" => "Adopt a folder", "Adotta una cartella";
    "bkpc.create" => "Create", "Crea";
    "bk.open" => "Open", "Apri";
    "bk.col.pc" => "PC", "PC";
    "bk.col.folder" => "Folder", "Cartella";
    "bk.col.snapshots" => "Snapshots", "Snapshot";
    "bk.col.space" => "Space", "Spazio";
    "bk.col.subvolume" => "Subvolume", "Sottovolume";
    "bk.col.newest" => "Newest", "Più recente";
    "bk.col.snapshot" => "Snapshot name", "Nome dello snapshot";
    "bk.col.received" => "Received", "Ricevuto";
    "bk.st.late" => "Late", "In ritardo";
    "bk.st.waiting" => "Waiting for the first backup", "In attesa del primo backup";
    "bk.st.missing" => "Folder missing", "Cartella mancante";
    "bk.problem.sshd_missing" => "No SSH server is installed or running: PCs cannot send. Install openssh-server.", "Nessun server SSH installato o attivo: i PC non possono inviare. Installa openssh-server.";
    "bk.problem.sshd_allow_list" => "The SSH server only lets in the users of AllowUsers/AllowGroups: add {subject} there, or PCs are refused.", "Il server SSH accetta solo gli utenti di AllowUsers/AllowGroups: aggiungi {subject}, altrimenti i PC vengono rifiutati.";
    "bk.problem.sudo_missing" => "sudo is not installed: the backup account runs btrfs through it. Install sudo.", "sudo non è installato: l'account di backup esegue btrfs tramite sudo. Installa sudo.";
    "bk.problem.btrfs_missing" => "btrfs-progs is not installed.", "btrfs-progs non è installato.";
    "bk.problem.filter_missing" => "The command filter {subject} is missing: reinstall rynas.", "Il filtro dei comandi {subject} manca: reinstalla rynas.";
    "bk.mode" => "Where the snapshots go", "Dove vanno gli snapshot";
    "bk.mode.new" => "New folder", "Nuova cartella";
    "bk.mode.new_help" => "A new btrfs subvolume, readable only by administrators.", "Un nuovo sottovolume btrfs, leggibile solo dagli amministratori.";
    "bk.mode.adopt_help" => "A folder with snapshots a PC already sent (for example copied from another NAS). Nothing inside changes, so the PC goes on with incremental sends.", "Una cartella con snapshot che un PC ha già inviato (per esempio copiata da un altro NAS). Dentro non cambia niente, così il PC continua con gli invii incrementali.";
    "bk.f.device" => "PC name", "Nome del PC";
    "bk.f.device_help" => "Lowercase letters, digits and -, at most 32.", "Lettere minuscole, cifre e -, al massimo 32.";
    "bkpc.f.path" => "Folder on the NAS", "Cartella sul NAS";
    "bkpc.f.path_help" => "On a btrfs volume. Letters, digits and _ @ + . - only.", "Su un volume btrfs. Solo lettere, cifre e _ @ + . -.";
    "bk.f.key" => "Public key of the PC", "Chiave pubblica del PC";
    "bk.f.key_help" => "The .pub file made on the PC (the steps are in the PC's panel). You can add it later.", "Il file .pub creato sul PC (i passaggi sono nel pannello del PC). Puoi aggiungerla dopo.";
    "bk.f.alert" => "Alert after days without backups", "Avviso dopo giorni senza backup";
    "bk.f.alert_help" => "A notification when nothing arrives for this many days; 0 turns it off.", "Una notifica quando non arriva niente per questi giorni; 0 la disattiva.";
    "bk.f.rules" => "Deleting old snapshots", "Eliminazione dei vecchi snapshot";
    "bk.f.allowed" => "The PC may delete old snapshots here", "Il PC può eliminare qui i vecchi snapshot";
    "bk.f.allowed_help" => "btrbk deletes what its target_preserve no longer keeps. Off: the PC can only add, and old snapshots stay until you delete them.", "btrbk elimina quello che il suo target_preserve non tiene più. Spento: il PC può solo aggiungere, e i vecchi snapshot restano finché non li elimini tu.";
    "bk.f.keep" => "Always keep the newest", "Tieni sempre i più recenti";
    "bkpc.f.keep_help" => "Per subvolume, whatever the PC asks.", "Per sottovolume, qualunque cosa chieda il PC.";
    "bk.f.min_age" => "Never delete before (days)", "Non eliminare prima di (giorni)";
    "bk.f.min_age_help" => "A snapshot younger than this stays even if the PC asks to delete it.", "Uno snapshot più giovane di così resta anche se il PC chiede di eliminarlo.";
    "bk.f.rules_note" => "These rules protect the backups from a PC that is broken or in the wrong hands: a deletion they refuse shows up as an error in btrbk's log on the PC. Keep them below the PC's target_preserve_min to avoid those errors.", "Queste regole proteggono i backup da un PC guasto o in mani sbagliate: un'eliminazione rifiutata compare come errore nel log di btrbk sul PC. Tienile sotto il target_preserve_min del PC per evitare quegli errori.";
    "bk.f.destination" => "Copy into", "Copia in";
    "bk.f.destination_help" => "An existing folder on the NAS. The copied files get the owner of that folder; set-user/group-ID bits and security attributes are dropped. Nothing is overwritten.", "Una cartella esistente sul NAS. I file copiati prendono il proprietario di quella cartella; i bit set-user/group-ID e gli attributi di sicurezza vengono tolti. Niente viene sovrascritto.";
    "bk.adopt.check" => "Check the folder", "Controlla la cartella";
    "bk.adopt.found" => "{n} received snapshots found.", "Trovati {n} snapshot ricevuti.";
    "bk.adopt.none" => "No btrbk snapshots in this folder. It can be adopted anyway and the PC starts with a full send.", "Nessuno snapshot di btrbk in questa cartella. Si può adottare lo stesso e il PC parte con un invio completo.";
    "bk.adopt.incomplete" => "Interrupted receives: {n}. btrbk deletes them before it sends those snapshots again.", "Ricezioni interrotte: {n}. btrbk le elimina prima di inviare di nuovo quegli snapshot.";
    "bk.adopt.other" => "Other entries left alone: {n}.", "Altri elementi lasciati come sono: {n}.";
    "bk.adopt.perm_change" => "Adopting changes owner, group and permissions of the folder itself (not of its content):", "Adottarla cambia proprietario, gruppo e permessi della cartella stessa (non del contenuto):";
    "bk.adopt.perm_confirm" => "Change owner and permissions of the folder", "Cambia proprietario e permessi della cartella";
    "bk.adopt.not_subvolume" => "The folder is not a subvolume of its own: it works, but its space is not measured apart from the volume.", "La cartella non è un sottovolume a sé: funziona, ma il suo spazio non si misura separatamente dal volume.";
    "bk.added" => "{device} is ready", "{device} è pronto";
    "bk.saved" => "PC settings saved", "Impostazioni del PC salvate";
    "bk.settings" => "Settings", "Impostazioni";
    "bk.settings_title" => "Settings of {device}", "Impostazioni di {device}";
    "bk.remove" => "Remove", "Rimuovi";
    "bk.remove_title" => "Remove {device}?", "Rimuovere {device}?";
    "bk.remove_text" => "The PC's key stops working. The folder and its snapshots stay: delete them in Files when you no longer need them, or adopt the folder again later.", "La chiave del PC smette di funzionare. La cartella e i suoi snapshot restano: eliminali in File quando non servono più, oppure adotta di nuovo la cartella più avanti.";
    "bk.removed" => "{device} removed, its snapshots kept", "{device} rimosso, i suoi snapshot restano";
    "bk.measure" => "Measure space", "Misura lo spazio";
    "bk.measured" => "Space measured", "Spazio misurato";
    "bk.usage_text" => "{excl} only in these snapshots, {total} in all (measured {when})", "{excl} solo in questi snapshot, {total} in tutto (misurato il {when})";
    "bk.usage_none" => "Not measured yet", "Non ancora misurato";
    "bk.rules.off" => "Only the NAS deletes", "Elimina solo il NAS";
    "bk.rules.on" => "The PC deletes; the newest {keep} stay, none younger than {days} days", "Elimina il PC; restano i {keep} più recenti, nessuno più giovane di {days} giorni";
    "bk.rules.on_one" => "The PC deletes; the newest stays, none younger than {days} days", "Elimina il PC; resta il più recente, nessuno più giovane di {days} giorni";
    "bk.rules.on_keep" => "The PC deletes; the newest {keep} always stay", "Elimina il PC; i {keep} più recenti restano sempre";
    "bk.rules.on_keep_one" => "The PC deletes; the newest always stays", "Elimina il PC; il più recente resta sempre";
    "bk.rules.on_days" => "The PC deletes; none younger than {days} days", "Elimina il PC; nessuno più giovane di {days} giorni";
    "bk.rules.on_free" => "The PC deletes as its btrbk decides", "Elimina il PC secondo il suo btrbk";
    "bk.alert_off" => "Off", "Disattivato";
    "bk.alert_days" => "After {n} days", "Dopo {n} giorni";
    "bk.last" => "Last received", "Ultimo ricevuto";
    "bk.incomplete" => "Interrupted receives", "Ricezioni interrotte";
    "bk.key" => "Key", "Chiave";
    "bk.adopted" => "Origin", "Origine";
    "bk.adopted_yes" => "Adopted folder", "Cartella adottata";
    "bk.no_key" => "No key yet: the PC cannot connect. Add its public key in Settings.", "Ancora nessuna chiave: il PC non può collegarsi. Aggiungi la sua chiave pubblica in Impostazioni.";
    "bk.late_note" => "Nothing received for more than {days} days.", "Niente ricevuto da più di {days} giorni.";
    "bk.steps" => "Set up the PC", "Configura il PC";
    "bk.steps_intro" => "On the PC, with sudo (the commands below use it): make a key for btrbk, add the lines to /etc/btrbk/btrbk.conf (change volume and subvolumes to the PC's layout), then connect once to accept the NAS key and try a dry run.", "Sul PC, con sudo (i comandi qui sotto lo usano): crea una chiave per btrbk, aggiungi le righe a /etc/btrbk/btrbk.conf (cambia volume e sottovolumi secondo il PC), poi collegati una volta per accettare la chiave del NAS e prova una simulazione.";
    "bk.step.key" => "1. Key of the PC", "1. Chiave del PC";
    "bk.step.config" => "2. btrbk.conf", "2. btrbk.conf";
    "bk.step.test" => "3. First connection and test", "3. Prima connessione e prova";
    "bk.step.line" => "Line written on the NAS (authorized_keys)", "Riga scritta sul NAS (authorized_keys)";
    "bk.steps_note" => "The key only runs the rynas filter: btrfs receive into this folder, deleting this PC's snapshots under the rules above, and the read-only commands btrbk needs. No shell, no other folder.", "La chiave esegue solo il filtro di rynas: btrfs receive in questa cartella, eliminazione degli snapshot di questo PC secondo le regole sopra e i comandi di sola lettura che servono a btrbk. Niente shell, nessun'altra cartella.";
    "bk.snapshots" => "Received snapshots", "Snapshot ricevuti";
    "bk.no_snapshots" => "Nothing received yet.", "Ancora niente ricevuto.";
    "bk.snap.received" => "Complete", "Completo";
    "bk.snap.protected" => "Kept by the NAS", "Tenuto dal NAS";
    "bk.snap.incomplete" => "Interrupted", "Interrotto";
    "bk.snap.not_subvolume" => "Not a subvolume", "Non è un sottovolume";
    "bk.browse" => "Browse", "Sfoglia";
    "bk.browse_note" => "Read only: what the PC sent, as it was at that moment.", "Sola lettura: quello che il PC ha inviato, com'era in quel momento.";
    "bkpc.copy" => "Copy to the NAS", "Copia sul NAS";
    "bk.copy_this" => "Copy this folder", "Copia questa cartella";
    "bk.copy_text" => "{path} is copied with its folders into the folder you choose.", "{path} viene copiato con le sue cartelle nella cartella che scegli.";
    "bkpc.copied" => "{path} copied", "{path} copiato";
    "task.pcbackup.create" => "Add a PC backup target", "Aggiunta di una destinazione di backup per PC";
    "task.pcbackup.update" => "PC backup settings", "Impostazioni del backup di un PC";
    "task.pcbackup.remove" => "Remove a PC backup target", "Rimozione di una destinazione di backup per PC";
    "task.pcbackup.usage" => "Measure a PC's backups", "Misura dei backup di un PC";
    "task.pcbackup.copy_out" => "Copy from a PC backup", "Copia da un backup di PC";
    "nev.pcbackup.late.title" => "No backup from {device} for {days} days", "Nessun backup da {device} da {days} giorni";
    "nev.pcbackup.late.body" => "Nothing received for {days} days (alert after {alert_days}).", "Niente ricevuto da {days} giorni (avviso dopo {alert_days}).";
    "nev.pcbackup.resumed.title" => "Backups from {device} arrive again", "I backup da {device} arrivano di nuovo";
    "nev.pcbackup.resumed.body" => "{device} sent a new snapshot.", "{device} ha inviato un nuovo snapshot.";
    // Restart / shut down (user menu, updates page, overlay; sysp.*) and its tasks and events
    "sysp.restart" => "Restart", "Riavvia";
    "sysp.shutdown" => "Shut down", "Spegni";
    "sysp.restart_now" => "Restart now", "Riavvia ora";
    "sysp.restart_title" => "Restart the NAS?", "Riavviare il NAS?";
    "sysp.restart_text" => "Every service stops and starts again: file shares, apps and this page are unavailable for a few minutes.", "Tutti i servizi si fermano e ripartono: condivisioni, app e questa pagina non sono disponibili per qualche minuto.";
    "sysp.shutdown_title" => "Shut down the NAS?", "Spegnere il NAS?";
    "sysp.shutdown_text" => "The NAS powers off. It starts again with its power button, Wake-on-LAN or a startup rule of the power schedule.", "Il NAS si spegne. Si riaccende con il pulsante di accensione, con Wake-on-LAN o con una regola di accensione della pianificazione.";
    "sysp.when" => "When", "Quando";
    "sysp.now" => "Now", "Ora";
    "sysp.in_1" => "In 1 minute", "Tra 1 minuto";
    "sysp.in_5" => "In 5 minutes", "Tra 5 minuti";
    "sysp.checking" => "Checking what is running…", "Controllo di ciò che è in corso…";
    "sysp.check_failed" => "Could not check what is running.", "Impossibile controllare ciò che è in corso.";
    "sysp.no_warnings" => "Nothing is running that would be interrupted.", "Non c'è niente in corso che verrebbe interrotto.";
    "sysp.warnings" => "This would interrupt:", "Verrebbe interrotto:";
    "sysp.anyway" => "You can go on anyway: interrupted work is not resumed by itself.", "Puoi procedere comunque: il lavoro interrotto non riprende da solo.";
    "sysp.w.raid_busy" => "{subject}: RAID {op}{pct} (it goes on after the start, from the beginning if interrupted)", "{subject}: RAID {op}{pct} (prosegue dopo l'avvio, da capo se interrotto)";
    "sysp.w.scrub_running" => "{subject} in progress", "{subject} in corso";
    "sysp.w.task_running" => "Task in progress: {subject}", "Attività in corso: {subject}";
    "sysp.w.scheduler_task_running" => "Scheduled task in progress: {subject}", "Attività pianificata in corso: {subject}";
    "sysp.w.smb_open_files" => "{n} files open over SMB", "{n} file aperti via SMB";
    "sysp.w.apt_running" => "Package manager running ({subject}): stopping it may leave packages half installed", "Gestore dei pacchetti in esecuzione ({subject}): fermarlo può lasciare pacchetti installati a metà";
    "sysp.w.ups_on_battery" => "The UPS runs on battery: the NAS may not start again until mains power is back", "L'UPS è a batteria: il NAS potrebbe non ripartire finché non torna la corrente";
    "sysp.w.boot_trial" => "A new kernel starts on trial at the next boot; if it fails, the previous kernel comes back by itself", "Al prossimo avvio parte in prova un nuovo kernel; se non funziona, torna da solo il kernel precedente";
    "sysp.op.resync" => "resync", "sincronizzazione";
    "sysp.op.recovery" => "rebuild", "ricostruzione";
    "sysp.op.reshape" => "reshape", "rimodellamento";
    "sysp.op.check" => "check", "verifica";
    "sysp.restart_scheduled" => "Restart scheduled.", "Riavvio pianificato.";
    "sysp.shutdown_scheduled" => "Shut down scheduled.", "Spegnimento pianificato.";
    "sysp.restart_failed" => "The restart did not start.", "Il riavvio non è partito.";
    "sysp.shutdown_failed" => "The shut down did not start.", "Lo spegnimento non è partito.";
    "sysp.pending_restart" => "Restart scheduled: {at}.", "Riavvio pianificato: {at}.";
    "sysp.pending_restart_by" => "Restart scheduled: {at} (asked by {user}).", "Riavvio pianificato: {at} (richiesto da {user}).";
    "sysp.pending_shutdown" => "Shut down scheduled: {at}.", "Spegnimento pianificato: {at}.";
    "sysp.pending_shutdown_by" => "Shut down scheduled: {at} (asked by {user}).", "Spegnimento pianificato: {at} (richiesto da {user}).";
    "sysp.cancel" => "Cancel restart or shut down", "Annulla riavvio o spegnimento";
    "sysp.cancelled" => "Cancelled: the NAS stays on.", "Annullato: il NAS resta acceso.";
    "sysp.restarting" => "Restarting…", "Riavvio in corso…";
    "sysp.shutting_down" => "Shutting down…", "Spegnimento in corso…";
    "sysp.going_text" => "Waiting for the NAS to stop.", "In attesa che il NAS si fermi.";
    "sysp.coming_text" => "Waiting for the NAS to come back. The sign-in page opens by itself.", "In attesa che il NAS torni disponibile. La pagina di accesso si apre da sola.";
    "sysp.going_off_text" => "Waiting for the NAS to power off.", "In attesa che il NAS si spenga.";
    "sysp.off_title" => "The NAS is off", "Il NAS è spento";
    "sysp.off_text" => "You can close this page. Once the NAS is on again, reload it to sign in.", "Puoi chiudere questa pagina. Quando il NAS è di nuovo acceso, ricaricala per accedere.";
    "sysp.reload" => "Reload page", "Ricarica la pagina";
    "task.system.restart" => "Restart the NAS", "Riavvio del NAS";
    "task.system.shutdown" => "Shut down the NAS", "Spegnimento del NAS";
    "nev.power.manual_restart.title" => "NAS restarting by {user}", "Riavvio del NAS richiesto da {user}";
    "nev.power.manual_restart.body" => "{user} restarts the NAS (delay {delay_min} min).", "{user} riavvia il NAS (ritardo {delay_min} min).";
    "nev.power.manual_shutdown.title" => "NAS shutting down by {user}", "Spegnimento del NAS richiesto da {user}";
    "nev.power.manual_shutdown.body" => "{user} shuts the NAS down (delay {delay_min} min).", "{user} spegne il NAS (ritardo {delay_min} min).";
    "nev.power.manual_cancelled.title" => "Restart or shut down cancelled by {user}", "Riavvio o spegnimento annullato da {user}";
    "nev.power.manual_cancelled.body" => "The NAS stays on.", "Il NAS resta acceso.";
}

// ---------------------------------------------------------------- formatting

fn sep() -> char {
    if lang() == Lang::It { ',' } else { '.' }
}

/// Fixed decimals with the locale's decimal separator.
pub fn num(v: f64, decimals: usize) -> String {
    let s = format!("{v:.decimals$}");
    if sep() == ',' { s.replace('.', ",") } else { s }
}

/// A whole count with the locale's thousands separator: 1,234,567 / 1.234.567.
pub fn count(n: u64) -> String {
    group(n, if sep() == ',' { '.' } else { ',' })
}

fn group(n: u64, mark: char) -> String {
    let d = n.to_string();
    let mut out = String::with_capacity(d.len() + d.len() / 3);
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.push(mark);
        }
        out.push(c);
    }
    out
}

/// Up to 3 significant digits: 1.23, 12.3, 123.
fn sig3(v: f64) -> String {
    let d = if v >= 100.0 {
        0
    } else if v >= 10.0 {
        1
    } else {
        2
    };
    num(v, d)
}

/// 1536 -> "1,50 KiB".
pub fn bytes(n: u64) -> String {
    const U: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{} {}", sig3(v), U[i])
}

/// Disk sizes the way vendors print them (decimal): 4000787030016 -> "4,0 TB".
pub fn bytes_si(n: u64) -> String {
    const U: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{} {}", num(v, if v >= 100.0 { 0 } else { 1 }), U[i])
    }
}

/// Byte rate: "12,3 MiB/s".
pub fn rate(bps: u64) -> String {
    format!("{}/s", bytes(bps))
}

/// Network rate in bits: "940 Mbit/s".
pub fn bitrate(bytes_per_s: u64) -> String {
    const U: [&str; 5] = ["bit/s", "kbit/s", "Mbit/s", "Gbit/s", "Tbit/s"];
    let mut v = bytes_per_s as f64 * 8.0;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", v as u64, U[0])
    } else {
        format!("{} {}", sig3(v), U[i])
    }
}

/// Link speed of a port from Mbit/s: "1 Gbit", "2,5 Gbit", "100 Mbit".
pub fn link_speed(m: u32) -> String {
    if m >= 1000 { format!("{} Gbit", num(m as f64 / 1000.0, if m.is_multiple_of(1000) { 0 } else { 1 })) } else { format!("{m} Mbit") }
}

pub fn pct(v: f64) -> String {
    format!("{}%", num(v, if v < 10.0 { 1 } else { 0 }))
}

pub fn celsius(v: f32) -> String {
    format!("{}°C", num(v as f64, 0))
}

/// Date and time in the current language, e.g. "Nov 5, 2026, 12:54 AM" / "5 nov 2026, 00:54".
pub fn datetime(ms: u64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&opts, &"dateStyle".into(), &"medium".into());
    let _ = js_sys::Reflect::set(&opts, &"timeStyle".into(), &"short".into());
    d.to_locale_string(lang().code(), &opts).as_string().unwrap_or_default()
}

/// Date in the current language, e.g. "Nov 5, 2026" / "5 nov 2026".
pub fn date(ms: u64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&opts, &"dateStyle".into(), &"medium".into());
    d.to_locale_date_string(lang().code(), &opts).as_string().unwrap_or_default()
}

/// Time of day for today's moments, date and time otherwise (lists of events).
pub fn short_when(ms: u64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let now = js_sys::Date::new_0();
    if d.to_date_string() == now.to_date_string() { time_hm(ms as f64) } else { datetime(ms) }
}

/// Time of day "14:05" in the current language.
pub fn time_hm(ms: f64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms));
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&opts, &"timeStyle".into(), &"short".into());
    d.to_locale_time_string_with_options(lang().code(), &opts).as_string().unwrap_or_default()
}

/// Uptime-style duration: "12 g 4 h 7 min" / "12d 4h 7m".
pub fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    let it = lang() == Lang::It;
    let (ud, uh, um) = if it { (" g", " h", " min") } else { ("d", "h", "m") };
    if d > 0 {
        format!("{d}{ud} {h}{uh} {m}{um}")
    } else if h > 0 {
        format!("{h}{uh} {m}{um}")
    } else if m > 0 {
        format!("{m}{um}")
    } else {
        format!("{secs} s")
    }
}

/// Elapsed time with seconds (task durations): "45 s", "3 min 05 s", "1 h 02 min" /
/// "45s", "3m 05s", "1h 02m".
pub fn elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let it = lang() == Lang::It;
    if h > 0 {
        if it { format!("{h} h {m:02} min") } else { format!("{h}h {m:02}m") }
    } else if m > 0 {
        if it { format!("{m} min {s:02} s") } else { format!("{m}m {s:02}s") }
    } else if it {
        format!("{s} s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_en() {
        assert_eq!(elapsed(7), "7s");
        assert_eq!(elapsed(185), "3m 05s");
        assert_eq!(elapsed(3720), "1h 02m");
    }

    #[test]
    fn browser_language() {
        assert_eq!(lang_for_tag("it-IT"), Lang::It);
        assert_eq!(lang_for_tag("it"), Lang::It);
        assert_eq!(lang_for_tag("en-US"), Lang::En);
        assert_eq!(lang_for_tag("de"), Lang::En);
        assert_eq!(lang_for_tag(""), Lang::En);
    }

    #[test]
    fn counts_are_grouped() {
        assert_eq!(group(0, ','), "0");
        assert_eq!(group(999, ','), "999");
        assert_eq!(group(1000, ','), "1,000");
        assert_eq!(group(1_234_567, '.'), "1.234.567");
    }

    #[test]
    fn english_is_the_fallback() {
        // entries without an Italian text use the English one
        assert_eq!(lookup("nav.dashboard", Lang::It), Some("Dashboard"));
        assert_eq!(lookup("nav.files", Lang::En), Some("Files"));
        assert_eq!(lookup("nav.files", Lang::It), Some("File"));
        assert_eq!(lookup("no.such.key", Lang::En), None);
    }

    #[test]
    fn no_duplicate_texts() {
        // keys whose text mirrors a server-side name (task types, configuration keys, plan
        // summary fields, server event texts by code) may repeat a UI text
        let exempt = |k: &str| k.starts_with("task.") || k.starts_with("cfg.key.") || k.starts_with("plan.k.") || k.starts_with("nev.");
        let mut seen = std::collections::HashMap::new();
        let mut dups = Vec::new();
        for (k, en, it) in entries() {
            if exempt(k) || !en.contains(' ') {
                continue;
            }
            if let Some(prev) = seen.insert((en, it), k) {
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
        for (k, en, it) in entries().into_iter().filter(|e| !PENDING.contains(&e.0) && !FORMAT_SOURCE.contains(&e.0)) {
            for text in [en, it] {
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
        let core: Vec<(&str, &str, &str)> = {
            let mut at = 0usize;
            let mut v = Vec::new();
            for (e, it) in LENS.chunks(2).zip(IT) {
                let k = &BLOB[at..at + e[0] as usize];
                let en = &BLOB[at + e[0] as usize..at + (e[0] + e[1]) as usize];
                at += (e[0] + e[1]) as usize;
                v.push((k, en, *it));
            }
            v
        };
        assert!(core.len() > 1000);
        let hits: Vec<_> = core
            .iter()
            .filter(|(k, en, it)| {
                k.starts_with("syn.") || [en, it].iter().any(|t| { let l = t.to_lowercase(); l.contains("synology") || l.contains("synodeb") || l.contains("synohwd") })
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
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("i18n.rs") {
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
                    if is_key_shape(k) && !not_a_key(k) && !keys.contains(k) && !cfg_key {
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

    /// The Italian file the interface fetches (`assets/i18n/it.txt`) matches the tables. When
    /// it does not, the test writes the current one and fails: commit the new file.
    #[test]
    fn italian_file_is_current() {
        for (k, en, it) in entries() {
            assert!(!k.contains(['\t', '\n']) && !it.contains(['\t', '\n']) && !en.contains('\n'), "tab or newline in {k}");
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/i18n/it.txt");
        let want = italian_file();
        if std::fs::read_to_string(path).ok().as_deref() != Some(want.as_str()) {
            std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/i18n")).unwrap();
            std::fs::write(path, &want).unwrap();
            panic!("assets/i18n/it.txt was out of date and has been rewritten: run the tests again and commit it");
        }
        // the file read back gives every Italian text
        let n = install_italian(want);
        assert_eq!(n, entries().len());
        assert_eq!(lookup("nav.files", Lang::It), Some("File"));
    }

    #[test]
    fn keys_are_unique() {
        let mut k: Vec<&str> = entries().into_iter().map(|e| e.0).collect();
        k.sort_unstable();
        let n = k.len();
        k.dedup();
        assert_eq!(n, k.len(), "duplicate i18n keys");
    }

    /// Event texts: the Italian text uses the same placeholders as the English one (a
    /// placeholder only in one language would hide or invent a field).
    #[test]
    fn event_texts_have_the_same_placeholders() {
        fn names(s: &str) -> Vec<&str> {
            let mut v: Vec<&str> = s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|x| x.0)).collect();
            v.sort_unstable();
            v
        }
        let bad: Vec<_> = entries().into_iter().filter(|(k, en, it)| k.starts_with("nev.") && names(en) != names(it)).map(|e| e.0).collect();
        assert!(bad.is_empty(), "placeholders differ: {bad:?}");
    }
}
