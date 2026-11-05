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
macro_rules! dict {
    ($($k:literal => $en:literal, $it:literal;)*) => {
        /// Every text back to back — key, English, Italian of each entry — with their byte
        /// lengths: much smaller in the wasm than a `match` over 1 800 strings or a table of
        /// string pointers.
        static BLOB: &str = concat!($($k, $en, $it),*);
        static LENS: &[u16] = &[$($k.len() as u16, $en.len() as u16, $it.len() as u16),*];
    };
}

/// Start of every text in [`BLOB`] (and the end) and the entries sorted by key, built on
/// first use.
struct Index {
    starts: Vec<u32>,
    by_key: Vec<u16>,
}

impl Index {
    fn text(&self, i: usize) -> &'static str {
        &BLOB[self.starts[i] as usize..self.starts[i + 1] as usize]
    }
}

thread_local! {
    static INDEX: Index = {
        let mut starts = Vec::with_capacity(LENS.len() + 1);
        let mut at = 0u32;
        starts.push(0);
        for &l in LENS {
            at += u32::from(l);
            starts.push(at);
        }
        let mut idx = Index { starts, by_key: (0..(LENS.len() / 3) as u16).collect() };
        let mut by_key = std::mem::take(&mut idx.by_key);
        by_key.sort_unstable_by_key(|&e| idx.text(e as usize * 3));
        idx.by_key = by_key;
        idx
    };
}

fn lookup(key: &str, lang: Lang) -> Option<&'static str> {
    INDEX.with(|idx| {
        let at = idx.by_key.binary_search_by(|&e| idx.text(e as usize * 3).cmp(key)).ok()?;
        let e = idx.by_key[at] as usize * 3;
        Some(idx.text(if lang == Lang::It { e + 2 } else { e + 1 }))
    })
}

/// The text of `key` in `lang`, for the tests of the pages.
#[cfg(test)]
pub fn text_in(lang: Lang, key: &str) -> Option<&'static str> {
    lookup(key, lang)
}

/// Every entry as (key, English, Italian), for the tests.
#[cfg(test)]
fn entries() -> Vec<(&'static str, &'static str, &'static str)> {
    INDEX.with(|idx| (0..LENS.len() / 3).map(|e| (idx.text(e * 3), idx.text(e * 3 + 1), idx.text(e * 3 + 2))).collect())
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

dict! {
    // navigation
    "nav.dashboard" => "Dashboard", "Dashboard";
    "nav.storage" => "Storage", "Archiviazione";
    "nav.files" => "Files", "File";
    "nav.shares" => "Shared Folders", "Cartelle condivise";
    "nav.users" => "Users & Groups", "Utenti e gruppi";
    "nav.services" => "Services", "Servizi";
    "nav.plugins" => "Plugins", "Plugin";
    "nav.hardware" => "Hardware", "Hardware";
    "nav.system" => "System", "Sistema";
    "nav.menu" => "Menu", "Menu";
    "nav.main" => "Main navigation", "Navigazione principale";
    "nav.collapse" => "Collapse sidebar", "Comprimi barra laterale";
    "nav.expand" => "Expand sidebar", "Espandi barra laterale";
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
    "task.st.ok" => "OK", "OK";
    "task.st.error" => "Error", "Errore";
    "task.st.cancelled" => "Stopped", "Interrotta";
    "task.st.running" => "Running", "In corso";
    "task.st.queued" => "Queued", "In coda";
    "task.config.apply" => "Apply configuration", "Applicazione configurazione";
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
    "cfg.key.snapshots" => "Snapshots", "Snapshot";
    "cfg.key.scheduler" => "Scheduled tasks", "Attività pianificate";
    "cfg.key.synology.hw" => "Hardware settings", "Impostazioni hardware";
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
    "hw.tab_synology" => "Synology", "Synology";
    "ps.title" => "Power schedule", "Pianificazione alimentazione";
    "syn.not_running" => "synohwd is not running: the status is not up to date and beeps or identify are unavailable. Settings can still be staged; applying them restarts synohwd.", "synohwd non è in esecuzione: lo stato non è aggiornato e i segnali acustici o l'identificazione non sono disponibili. Le impostazioni si possono comunque preparare; applicarle riavvia synohwd.";
    "syn.foreign" => "/etc/synohwd.toml has button commands rynas does not manage ({cmds}): applying these settings replaces them.", "/etc/synohwd.toml contiene comandi dei pulsanti non gestiti da rynas ({cmds}): applicare queste impostazioni li sostituisce.";
    "syn.platform" => "{bays} bays · {fans} fans", "{bays} alloggiamenti · {fans} ventole";
    "syn.settings" => "Settings", "Impostazioni";
    "syn.daemon" => "synohwd", "synohwd";
    "syn.running" => "Running", "In esecuzione";
    "syn.stopped" => "Not running", "Non in esecuzione";
    "last_boot" => "Last boot", "Ultimo avvio";
    "syn.power_led" => "Power LED", "LED di alimentazione";
    "syn.bay_no_power" => "Disk present, bay without power", "Disco presente, alloggiamento senza alimentazione";
    "syn.temp_last" => "Last reading before standby (the disk is not woken up)", "Ultima lettura prima dello standby (il disco non viene risvegliato)";
    "syn.identify" => "Identify", "Identifica";
    "syn.identifying" => "Blinking…", "Lampeggia…";
    "syn.identify_on" => "The LED of bay {n} blinks for {s} seconds.", "Il LED dell'alloggiamento {n} lampeggia per {s} secondi.";
    "syn.fan_mode" => "Fan speed mode", "Modalità velocità ventola";
    "syn.fan_low" => "Quiet", "Silenziosa";
    "syn.fan_high" => "Cool", "Raffreddamento";
    "syn.fan_full" => "Full speed", "Velocità massima";
    "syn.fan_low_help" => "Default: the fans follow the temperature curves and stay quiet.", "Predefinita: le ventole seguono le curve di temperatura e restano silenziose.";
    "syn.fan_high_help" => "Lower thresholds: the fans speed up earlier and the drives run cooler.", "Soglie più basse: le ventole accelerano prima e i dischi restano più freschi.";
    "syn.fan_full_help" => "The fans always run at 99%.", "Le ventole girano sempre al 99%.";
    "syn.full_note" => "Full speed has no curve: the fans run at 99%. The shutdown thresholds stay those of the quiet mode.", "La velocità massima non ha una curva: le ventole girano al 99%. Le soglie di spegnimento restano quelle della modalità silenziosa.";
    "syn.fan_ok" => "OK", "OK";
    "syn.fan_stopped" => "Stopped", "Ferma";
    "syn.fan_unknown" => "Not measured", "Non misurata";
    "syn.curve" => "Fan curve", "Curva delle ventole";
    "syn.curve_duty" => "Current speed", "Velocità attuale";
    "syn.curve_preview" => "Preview of the selected mode (not applied yet).", "Anteprima della modalità selezionata (non ancora applicata).";
    "syn.cls_disk" => "Disks", "Dischi";
    "syn.cls_nvme" => "NVMe", "NVMe";
    "syn.cls_cpu" => "CPU", "CPU";
    "syn.cls_nic" => "Network card", "Scheda di rete";
    "syn.fan_period" => "Adjust every (s)", "Regola ogni (s)";
    "syn.fan_period_help" => "Seconds between two fan adjustments (5–300).", "Secondi tra due regolazioni delle ventole (5–300).";
    "syn.fan_min_duty" => "Minimum speed (%)", "Velocità minima (%)";
    "syn.fan_min_duty_help" => "Floor under the curves (0–99).", "Minimo sotto le curve (0–99).";
    "syn.hysteresis" => "Hysteresis (°C)", "Isteresi (°C)";
    "syn.hysteresis_help" => "How far a sensor must cool below a step before the fans slow down (0–20).", "Di quanto un sensore deve scendere sotto un gradino prima che le ventole rallentino (0–20).";
    "syn.v.range" => "Between {min} and {max}.", "Tra {min} e {max}.";
    "syn.thermal" => "Thermal shutdown", "Spegnimento termico";
    "syn.thermal_on" => "Shut down when too hot", "Spegni in caso di surriscaldamento";
    "syn.thermal_help" => "synohwd powers the NAS off after two readings at or above the shutdown threshold of a sensor.", "synohwd spegne il NAS dopo due letture pari o superiori alla soglia di spegnimento di un sensore.";
    "syn.thermal_off" => "Thermal shutdown is off: disks, NVMe drives and the CPU can overheat and be damaged if a fan fails.", "Lo spegnimento termico è disattivato: dischi, unità NVMe e CPU possono surriscaldarsi e danneggiarsi se una ventola si guasta.";
    "syn.sensor" => "Sensor", "Sensore";
    "syn.now_limit" => "Now / shutdown at", "Ora / spegnimento a";
    "syn.leds_beeper" => "LEDs and beeper", "LED e cicalino";
    "syn.disk_leds" => "Disk LEDs", "LED dei dischi";
    "syn.disk_leds_help" => "Green when a disk is present, blinking on activity.", "Verde con il disco presente, lampeggiante durante l'attività.";
    "syn.brightness" => "LED brightness", "Luminosità dei LED";
    "syn.brightness_help" => "Shared by the disk LEDs; applied with the other settings.", "Comune a tutti i LED dei dischi; applicata con le altre impostazioni.";
    "syn.beep_start" => "Beep when the NAS has started", "Segnale acustico all'avvio del NAS";
    "syn.beep_error" => "Beep on hardware errors", "Segnale acustico in caso di errori hardware";
    "syn.beep_error_help" => "Fan failure and thermal shutdown.", "Guasto delle ventole e spegnimento termico.";
    "syn.test_beep" => "Test beep", "Prova segnale acustico";
    "syn.beep_sent" => "Beep sent.", "Segnale acustico inviato.";
    "syn.mute" => "Mute alarm", "Silenzia allarme";
    "syn.mute_help" => "synohwd's beeps are single; muting acknowledges the open hardware notifications so their reminders stop.", "I segnali di synohwd sono singoli; silenziare conferma le notifiche hardware aperte e ne ferma i promemoria.";
    "syn.muted" => "Alarm muted: {n} notifications acknowledged.", "Allarme silenziato: {n} notifiche confermate.";
    "syn.buttons" => "Buttons", "Pulsanti";
    "syn.power_button" => "Power button", "Pulsante di accensione";
    "syn.power_button_help" => "A clean shutdown stops tasks and services in order.", "Uno spegnimento pulito ferma attività e servizi in ordine.";
    "syn.pb_poweroff" => "Shut down cleanly", "Spegni in modo pulito";
    "syn.btn_log" => "Only record it in the events", "Registra solo negli eventi";
    "syn.btn_ignore" => "Ignore", "Ignora";
    "reset_button" => "Reset button", "Pulsante di reset";
    "syn.rb_rescue" => "Network rescue (double press)", "Ripristino di rete (doppia pressione)";
    "syn.rescue_help" => "Press the reset button twice within 10 seconds: the network goes back to DHCP with a safe firewall, so you can reach the NAS again after a wrong network setting. A single press does nothing.", "Premi il pulsante di reset due volte entro 10 secondi: la rete torna in DHCP con un firewall sicuro, così puoi raggiungere di nuovo il NAS dopo un'impostazione di rete sbagliata. Una sola pressione non fa nulla.";
    "syn.rescue_off" => "Without network rescue, a NAS with a wrong network setting can only be reached with a monitor or a serial console.", "Senza ripristino di rete, un NAS con un'impostazione di rete sbagliata si raggiunge solo con un monitor o una console seriale.";
    "syn.power" => "Power and disks", "Alimentazione e dischi";
    "syn.standby" => "Disk standby after (minutes)", "Standby dei dischi dopo (minuti)";
    "syn.standby_help" => "Idle time before the bay disks spin down; 0 = never (0–330).", "Inattività prima che i dischi degli alloggiamenti si fermino; 0 = mai (0–330).";
    "syn.ac_recovery" => "Restart after a power failure", "Riavvia dopo un'interruzione di corrente";
    "syn.ac_recovery_help" => "Power back on automatically when the power supply returns.", "Riaccendi automaticamente quando torna l'alimentazione.";
    "syn.wol" => "Wake-on-LAN", "Wake-on-LAN";
    "syn.wol_help" => "Start the NAS with a magic packet on any LAN port.", "Avvia il NAS con un magic packet su qualsiasi porta LAN.";
    "syn.macs" => "MAC addresses for Wake-on-LAN", "Indirizzi MAC per Wake-on-LAN";
    "syn.no_macs" => "No Ethernet ports found.", "Nessuna porta Ethernet trovata.";
    "syn.unsaved" => "Unsaved hardware settings", "Impostazioni hardware non salvate";
    "ps.help" => "Weekly rules that start or shut down the NAS. Startup uses the RTC wake alarm, set before every shutdown; shutdowns check the system first.", "Regole settimanali che accendono o spengono il NAS. L'accensione usa la sveglia RTC, impostata prima di ogni spegnimento; gli spegnimenti controllano prima il sistema.";
    "add_rule" => "Add rule", "Aggiungi regola";
    "ps.edit" => "Edit rule", "Modifica regola";
    "ps.none_title" => "No power schedule", "Nessuna pianificazione";
    "ps.none_text" => "Add a rule to start or shut down the NAS on chosen days and times.", "Aggiungi una regola per accendere o spegnere il NAS in giorni e orari scelti.";
    // UPS (Hardware › UPS)
    "ups.tab" => "UPS", "UPS";
    "task.ups.test" => "UPS battery test", "Test batteria UPS";
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
    "ups.none_text" => "The UPS service runs but reports no UPS. Connect the UPS over USB and add it to NUT's ups.conf (nut-scanner finds it).", "Il servizio UPS è attivo ma non segnala alcun UPS. Collega l'UPS via USB e aggiungilo a ups.conf di NUT (nut-scanner lo trova).";
    "ups.none_text_synodeb" => "The UPS service runs but reports no UPS. Connect a UPS over USB and set it up with synodeb-ups setup.", "Il servizio UPS è attivo ma non segnala alcun UPS. Collega un UPS via USB e configuralo con synodeb-ups setup.";
    "ups.down_title" => "UPS service not running", "Servizio UPS non attivo";
    "ups.down_text" => "Nothing answers on the NUT port of this NAS (127.0.0.1:3493). Install NUT (Debian package nut), set up the UPS and start nut-server.", "Nessuna risposta sulla porta NUT di questo NAS (127.0.0.1:3493). Installa NUT (pacchetto Debian nut), configura l'UPS e avvia nut-server.";
    "ups.down_text_synodeb" => "Nothing answers on the NUT port of this NAS (127.0.0.1:3493). Set up the UPS with synodeb-ups setup.", "Nessuna risposta sulla porta NUT di questo NAS (127.0.0.1:3493). Configura l'UPS con synodeb-ups setup.";
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
    "ups.lb_always" => "Safe Mode always starts when the UPS reports low battery (LB) or is critical (on battery and low battery), whichever comes first. The timer below only adds an earlier trigger.", "La modalità sicura parte sempre quando l'UPS segnala batteria scarica (LB) o è critico (a batteria e batteria scarica), qualunque cosa avvenga prima. Il timer qui sotto aggiunge solo un innesco anticipato.";
    "ups.lb_always_nut" => "NUT's upsmon always shuts the system down when the UPS reports low battery (LB) or is critical, and rynas stops its tasks first. The timer below only adds an earlier shutdown.", "upsmon di NUT spegne sempre il sistema quando l'UPS segnala batteria scarica (LB) o è critico, e rynas prima ferma le sue attività. Il timer qui sotto aggiunge solo uno spegnimento anticipato.";
    "ups.need_nut" => "Install NUT, the UPS service (Debian package nut), and set up the UPS: the shutdown policy and the battery tests use it.", "Installa NUT, il servizio UPS (pacchetto Debian nut), e configura l'UPS: la politica di spegnimento e i test batteria lo usano.";
    "ups.timer" => "Enter Safe Mode after a time on battery", "Entra in modalità sicura dopo un tempo a batteria";
    "ups.timer_help" => "Services stop, volumes are unmounted and disks spin down; the NAS restarts when power is back.", "I servizi si fermano, i volumi vengono smontati e i dischi si fermano; il NAS riparte al ritorno della corrente.";
    "ups.timer_nut" => "Shut down after a time on battery", "Spegni dopo un tempo a batteria";
    "ups.timer_help_nut" => "rynas stops its tasks, then asks NUT's upsmon to shut the system down (upsmon -c fsd), with the UPS power-off configured in NUT.", "rynas ferma le sue attività, poi chiede a upsmon di NUT di spegnere il sistema (upsmon -c fsd), con lo spegnimento dell'UPS configurato in NUT.";
    "ups.minutes" => "Minutes on battery", "Minuti a batteria";
    "ups.minutes_help" => "1 to 1440 minutes.", "Da 1 a 1440 minuti.";
    "ups.pre_timeout" => "Time to stop apps and tasks (s)", "Tempo per fermare app e attività (s)";
    "ups.pre_timeout_help" => "Before Safe Mode, rynas and other hooks stop their work within this time (10 to 900 s).", "Prima della modalità sicura, rynas e gli altri hook fermano il loro lavoro entro questo tempo (da 10 a 900 s).";
    "ups.pre_timeout_help_nut" => "Before the shutdown, rynas stops its running tasks and scheduled jobs within this time (10 to 900 s).", "Prima dello spegnimento, rynas ferma le attività in corso e quelle pianificate entro questo tempo (da 10 a 900 s).";
    "ups.pending_note" => "Saved, not applied yet: the UPS uses the applied settings.", "Salvato, non ancora applicato: l'UPS usa le impostazioni applicate.";
    "ups.v_minutes" => "Enter whole minutes from 1 to 1440.", "Inserisci minuti interi da 1 a 1440.";
    "ups.v_pre" => "Enter seconds from 10 to 900.", "Inserisci secondi da 10 a 900.";
    "ups.test" => "Battery test", "Test batteria";
    "ups.last_result" => "Last result", "Ultimo risultato";
    "ups.no_result" => "Not reported by the UPS", "Non segnalato dall'UPS";
    "ups.test_done" => "Battery test finished.", "Test batteria terminato.";
    "ups.test_unsupported" => "This UPS offers no battery test through NUT.", "Questo UPS non offre test della batteria tramite NUT.";
    "ups.test_online" => "A test can start only while the UPS is on mains power.", "Un test può partire solo mentre l'UPS è alimentato dalla rete.";
    "ups.test_lb" => "During a test, low battery still starts Safe Mode.", "Durante un test, la batteria scarica avvia comunque la modalità sicura.";
    "ups.test_lb_nut" => "During a test, low battery still shuts the system down.", "Durante un test, la batteria scarica spegne comunque il sistema.";
    "ups.quick" => "Quick test", "Test rapido";
    "ups.deep" => "Deep test", "Test approfondito";
    "ups.stop" => "Stop test", "Ferma test";
    "ups.deep_title" => "Start a deep battery test?", "Avviare un test approfondito della batteria?";
    "ups.deep_text" => "The UPS runs on battery until it is almost empty, which can take an hour. If the battery reaches low battery (LB), the NAS enters Safe Mode: start it only when someone is present to stop the test.", "L'UPS funziona a batteria fino a scaricarla quasi del tutto, anche per un'ora. Se la batteria arriva a batteria scarica (LB), il NAS entra in modalità sicura: avvialo solo con qualcuno presente per fermare il test.";
    "ups.deep_text_nut" => "The UPS runs on battery until it is almost empty, which can take an hour. If the battery reaches low battery (LB), NUT shuts the system down: start it only when someone is present to stop the test.", "L'UPS funziona a batteria fino a scaricarla quasi del tutto, anche per un'ora. Se la batteria arriva a batteria scarica (LB), NUT spegne il sistema: avvialo solo con qualcuno presente per fermare il test.";
    "ups.deep_start" => "Start deep test", "Avvia test approfondito";
    "ups.schedule_hint" => "To test on a schedule, add a scheduled task that runs as root:", "Per un test pianificato, aggiungi un'attività pianificata eseguita come root:";
    "ups.events" => "Recent UPS events", "Eventi UPS recenti";
    "ups.no_events" => "No UPS events yet.", "Ancora nessun evento UPS.";
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
    "ps.arm_error" => "The wake alarm could not be set: {e}", "Non è stato possibile impostare la sveglia: {e}";
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
    "soon.files" => "A dual-pane file manager modelled on SynoFile: tabs, previews, archives you can browse like folders, and a recycle bin.", "File manager a doppio pannello sul modello di SynoFile: schede, anteprime, archivi navigabili come cartelle e cestino.";
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
    "err.conflict" => "Something changed meanwhile. Reload and try again.", "Nel frattempo qualcosa è cambiato. Ricarica e riprova.";
    "err.busy" => "Another operation is running. Try again shortly.", "È in corso un'altra operazione. Riprova tra poco.";
    "err.task_finished" => "The task has already finished.", "L'attività è già terminata.";
    "err.already_exists" => "It already exists.", "Esiste già.";
    "err.confirmation_required" => "This needs an explicit confirmation.", "Serve una conferma esplicita.";
    "err.not_applied" => "The change was not applied.", "La modifica non è stata applicata.";
    "err.method_not_allowed" => "Operation not supported.", "Operazione non supportata.";
    "err.payload_too_large" => "The data sent is too large.", "I dati inviati sono troppo grandi.";
    "err.unsupported_media_type" => "Unsupported data format.", "Formato dei dati non supportato.";
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
    "shares.volume_usage" => "Volume usage", "Uso del volume";
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
    "shares.delete_title" => "Delete shared folder {name}?", "Eliminare la cartella condivisa {name}?";
    "shares.delete_text" => "The share and its permissions are removed when you apply the changes. The folder and its files stay on the volume: you will find it under Unshared folders.", "La condivisione e le sue autorizzazioni vengono rimosse quando applichi le modifiche. La cartella e i suoi file restano sul volume: la trovi in Cartelle non condivise.";
    "shares.tab_general" => "General", "Generale";
    "shares.tab_smb" => "SMB", "SMB";
    "shares.tab_nfs" => "NFS", "NFS";
    "shares.loc_subvolume" => "New subvolume", "Nuovo sottovolume";
    "shares.loc_folder" => "Existing folder", "Cartella esistente";
    "shares.no_volumes" => "No volume found. Create one in Storage, or share an existing folder.", "Nessun volume trovato. Creane uno in Storage, oppure condividi una cartella esistente.";
    "shares.free" => "free", "liberi";
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
    "shares.w.all_denied" => "Every entry denies access: nobody can open this folder.", "Ogni voce nega l'accesso: nessuno può aprire questa cartella.";
    "shares.w.snapfolder_no_job" => "The snapshot folder appears once this shared folder has a snapshot job.", "La cartella degli snapshot compare quando questa cartella condivisa ha un job di snapshot.";
    "shares.w.snapfolder_nfs" => "NFS clients see the snapshot folder only with Include nested mounts on (NFS tab).", "I client NFS vedono la cartella degli snapshot solo con Includi i mount annidati attivo (scheda NFS).";
    "task.shares.folder-delete" => "Delete folder", "Eliminazione cartella";
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
    "sch.started" => "{name} started", "{name} avviata";
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
    "snap.root_step2" => "synodeb-dom writes that kernel to the boot flash (DOM). If it refuses, nothing changes.", "synodeb-dom scrive quel kernel sulla flash di avvio (DOM). Se rifiuta, non cambia nulla.";
    "snap.root_step3" => "The current system is kept as a new restore point (reason: before rollback), then the restore point becomes the system subvolume (@rootfs).", "Il sistema attuale viene conservato come nuovo punto di ripristino (motivo: before rollback), poi il punto di ripristino diventa il subvolume di sistema (@rootfs).";
    "snap.root_step4" => "You reboot the NAS to start the restored system.", "Riavvii il NAS per avviare il sistema ripristinato.";
    "snap.root_reboot" => "A reboot is needed to finish. Until then the current system keeps running.", "Per finire serve un riavvio. Fino ad allora il sistema attuale continua a funzionare.";
    "snap.root_done" => "At the next reboot the system starts from {id} (kernel {kernel}). Reboot now to finish the rollback.", "Al prossimo riavvio il sistema parte da {id} (kernel {kernel}). Riavvia ora per completare il ripristino.";
    "snap.root_refused_title" => "{id} cannot be restored from here", "{id} non si può ripristinare da qui";
    "snap.refusal.not_synodeb" => "This system cannot be rolled back from the web interface: the restore point is there, going back to it is a console procedure.", "Questo sistema non si può ripristinare dall'interfaccia web: il punto di ripristino c'è, tornarci è una procedura da console.";
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
    "net.phase_note" => "IPv6 settings, bonds, VLANs and bridges are shown read-only for now.", "Per ora impostazioni IPv6, bond, VLAN e bridge sono in sola lettura.";
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
    "net.reset_on" => "Restores the network", "Ripristina la rete";
    "net.reset_off" => "Does not restore the network", "Non ripristina la rete";
    "net.reset_pending_on" => "Will restore the network after apply", "Ripristinerà la rete dopo l'applicazione";
    "net.reset_pending_off" => "Will stop restoring the network after apply", "Smetterà di ripristinare la rete dopo l'applicazione";
    "net.reset_text" => "Press it twice within 10 seconds: DHCP and link-local on every port, the previous settings kept as a revision.", "Premilo due volte entro 10 secondi: DHCP e link-local su ogni porta, le impostazioni precedenti conservate come revisione.";
    "net.reset_owner" => "Set in Hardware › Synology.", "Si imposta in Hardware › Synology.";
    "net.reset_owner_link" => "Open Hardware", "Apri Hardware";
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
    // ---- System › Updates (upd.*) and its task names
    "task.updates.check" => "Check for updates", "Controllo aggiornamenti";
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
    "upd.dom_outdated" => "The boot entries are outdated.", "Le voci di avvio non sono aggiornate.";
    "upd.dom_error" => "The boot state could not be read.", "Non è stato possibile leggere lo stato dell'avvio.";
    "upd.dom_default" => "Default entry", "Voce predefinita";
    "upd.dom_fallback" => "Fallback", "Riserva";
    "upd.dom_entries" => "Boot entries", "Voci di avvio";
    "upd.dom_running" => "Booted from", "Avviato da";
    "upd.dom_kernel" => "Kernel on the DOM", "Kernel sul DOM";
    "upd.dom_kernel_old" => "Previous kernel", "Kernel precedente";
    "upd.trial" => "Kernel trial", "Prova del kernel";
    "upd.trial_none" => "None", "Nessuna";
    "upd.trial_armed" => "Armed", "Pronta";
    "upd.trial_trying" => "Trying", "In prova";
    "upd.dom_free" => "Free space on the DOM", "Spazio libero sul DOM";
    // ---- Save in dialogs and editors: stage + apply only that change (config::save_apply)
    "toast.saved_applied" => "Saved and applied", "Salvato e applicato";
    "cfg.applying_now" => "Applying the change…", "Applicazione della modifica…";
    "cfg.apply_kept" => "The previous settings are back in place; the change stays in the pending changes.", "Sono tornate le impostazioni precedenti; la modifica resta tra quelle in sospeso.";
    "cfg.apply_log" => "Show the log", "Mostra il registro";
    "cfg.key.ups.safemode" => "UPS shutdown policy", "Criterio di spegnimento con l'UPS";
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
        // summary fields) may repeat a UI text
        let exempt = |k: &str| k.starts_with("task.") || k.starts_with("cfg.key.") || k.starts_with("plan.k.");
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
    /// Platform names (Synology, synohwd, synodeb) are allowed: their texts are rendered only
    /// where that hardware is present (G8d).
    #[test]
    fn no_other_products() {
        const PRODUCTS: &[&str] = &["dsm", "truenas", "unraid", "openmediavault", "omv", "qnap", "qts"];
        // app names of other systems, matched as phrases
        const APPS: &[&str] = &["storage manager", "task scheduler", "file station", "package center", "hyper backup", "control panel", "utilità di pianificazione"];
        // being reworded by the UPS work in progress: drop from here once it is
        const PENDING: &[&str] = &[];
        let mut hits = Vec::new();
        for (k, en, it) in entries().into_iter().filter(|e| !PENDING.contains(&e.0)) {
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
                || s.starts_with("fuse.")
                || s.contains("example")
                || matches!(s, "a.b" | "metrics.sample" | "hardware.status" | "task.started" | "task.updated" | "task.finished")
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

    #[test]
    fn keys_are_unique() {
        let mut k: Vec<&str> = entries().into_iter().map(|e| e.0).collect();
        k.sort_unstable();
        let n = k.len();
        k.dedup();
        assert_eq!(n, k.len(), "duplicate i18n keys");
    }
}
