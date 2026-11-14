//! The sidebar of the signed-in user (issue #11): order of the entries and of their groups,
//! hidden entries, pinned pages. Stored on the NAS for each user (`/api/v1/prefs/sidebar`,
//! [`rynas_api_types::ui_prefs`]), so it follows the account across browsers; merged on load
//! with the default of this release ([`ui_prefs::merge`]): entries of a later release show up
//! in their default place. Entries the system hides (Docker while it is not installed,
//! [`nav_shown`]) stay hidden whatever the layout says.
//!
//! Edited in place, like the dashboard's Customize mode: "Customize sidebar" turns the entries
//! into rows with a drag handle (pointer drag, or the arrow keys on the focused handle), a
//! hide button (pins: edit and unpin), a separator with its own handle per group, the tray of
//! hidden entries, "Pin this page", Reset and Done. Every change is saved shortly after it is
//! made. The phone's bottom bar takes its first entries from the same layout ([`bottom_ids`]).

use std::rc::Rc;

use leptos::html::Ul;
use leptos::prelude::*;
use leptos_router::hooks::use_location;
use rynas_api_types::ui_prefs::{self, MAX_GROUPS, MAX_PINS, PIN_LABEL_MAX, SidebarLayout, SidebarPin, SidebarPrefs};
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, KeyboardEvent, PointerEvent};

use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::reorder;
use crate::shell::{NAV, nav_shown};
use crate::ui::{Tone, verb_name};

const URL: &str = "/api/v1/prefs/sidebar";
/// Entries of the phone's bottom bar before its Menu button.
pub const BOTTOM_ENTRIES: usize = 3;

/// The icons a pin may use: [`PIN_ICONS`] names, their drawing and their accessible name.
pub const PIN_ICON_UI: &[(&str, &str, &str)] = &[
    ("pin", icons::PIN, "nav.icon.pin"),
    ("hard-drive", icons::HARD_DRIVE, "nav.icon.hard_drive"),
    ("database", icons::DATABASE, "nav.icon.database"),
    ("layers", icons::LAYERS, "nav.icon.layers"),
    ("folder", icons::FOLDER, "nav.icon.folder"),
    ("folder-open", icons::FOLDER_OPEN, "nav.icon.folder_open"),
    ("share", icons::SHARE_2, "nav.icon.share"),
    ("camera", icons::CAMERA, "nav.icon.camera"),
    ("archive-restore", icons::ARCHIVE_RESTORE, "nav.icon.archive"),
    ("users", icons::USERS, "nav.icon.users"),
    ("user", icons::USER, "nav.icon.user"),
    ("server", icons::SERVER, "nav.icon.server"),
    ("container", icons::CONTAINER, "nav.icon.container"),
    ("box", icons::BOX, "nav.icon.box"),
    ("puzzle", icons::PUZZLE, "nav.icon.puzzle"),
    ("network", icons::NETWORK, "nav.icon.network"),
    ("link", icons::LINK, "nav.icon.link"),
    ("cpu", icons::CPU, "nav.icon.cpu"),
    ("memory-stick", icons::MEMORY_STICK, "nav.icon.memory"),
    ("activity", icons::ACTIVITY, "nav.icon.activity"),
    ("settings", icons::SETTINGS, "nav.icon.settings"),
    ("calendar-clock", icons::CALENDAR_CLOCK, "nav.icon.calendar"),
    ("clock", icons::CLOCK, "nav.icon.clock"),
    ("history", icons::HISTORY, "nav.icon.history"),
    ("download", icons::DOWNLOAD, "nav.icon.download"),
    ("bell", icons::BELL, "nav.icon.bell"),
    ("shield-check", icons::SHIELD_CHECK, "nav.icon.shield"),
    ("key", icons::KEY, "nav.icon.key"),
    ("lock", icons::LOCK, "nav.icon.lock"),
    ("usb", icons::USB, "nav.icon.usb"),
    ("disc", icons::DISC, "nav.icon.disc"),
    ("file-text", icons::FILE_TEXT, "nav.icon.file"),
    ("life-buoy", icons::LIFE_BUOY, "nav.icon.life_buoy"),
    ("power", icons::POWER, "nav.icon.power"),
];

/// The drawing of a pin icon (the pin itself for an unknown name).
pub fn pin_icon(name: &str) -> &'static str {
    PIN_ICON_UI.iter().find(|i| i.0 == name).map_or(icons::PIN, |i| i.1)
}

/// The pin icon name of a drawing, if it is one of [`PIN_ICON_UI`].
fn icon_name(svg: &str) -> Option<&'static str> {
    PIN_ICON_UI.iter().find(|i| i.1 == svg).map(|i| i.0)
}

/// The path part of an entry's address (without the query).
fn route(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

/// How well an entry at `path` matches the page at `pathname`: the length of its route when
/// the page is the route or below it, `None` otherwise (`/` matches only itself).
pub fn match_len(path: &str, pathname: &str) -> Option<usize> {
    let r = route(path);
    let hit = if r == "/" { pathname == "/" } else { pathname == r || pathname.strip_prefix(r).is_some_and(|rest| rest.starts_with('/')) };
    hit.then_some(r.len())
}

/// Whether the system shows entry `id` now (built-in entries only: Docker while installed).
pub fn shown(id: &str) -> bool {
    NAV.iter().find(|n| n.id == id).is_none_or(|n| nav_shown(n.path))
}

/// The entry of the current page: of the entries on screen, the one whose route matches
/// `pathname` best (a pinned System › Scheduled tasks wins over System).
pub fn current(layout: &SidebarLayout, pathname: &str, shown: &dyn Fn(&str) -> bool) -> Option<String> {
    layout
        .order()
        .filter(|id| !layout.hidden.iter().any(|h| h == id) && shown(id))
        .filter_map(|id| path_of(layout, id).and_then(|p| match_len(&p, pathname)).map(|n| (n, id)))
        .max_by_key(|(n, _)| *n)
        .map(|(_, id)| id.to_string())
}

/// The first `n` entries on screen, for the phone's bottom bar.
pub fn bottom_ids(layout: &SidebarLayout, shown: &dyn Fn(&str) -> bool, n: usize) -> Vec<String> {
    layout.order().filter(|id| !layout.hidden.iter().any(|h| h == id) && shown(id)).take(n).map(str::to_string).collect()
}

fn path_of(layout: &SidebarLayout, id: &str) -> Option<String> {
    NAV.iter().find(|n| n.id == id).map(|n| n.path.to_string()).or_else(|| layout.pin(id).map(|p| p.path.clone()))
}

/// What an entry shows: built-in (a dictionary key) or pinned (the user's name).
#[derive(Clone, Debug, PartialEq)]
pub struct EntryView {
    pub path: String,
    pub icon: &'static str,
    label: Result<&'static str, String>,
    pub pin: bool,
}

impl EntryView {
    pub fn label(&self) -> String {
        match &self.label {
            Ok(key) => t(key).to_string(),
            Err(text) => text.clone(),
        }
    }
}

pub fn entry(layout: &SidebarLayout, id: &str) -> Option<EntryView> {
    if let Some(n) = NAV.iter().find(|n| n.id == id) {
        return Some(EntryView { path: n.path.to_string(), icon: n.icon, label: Ok(n.key), pin: false });
    }
    layout.pin(id).map(|p| EntryView { path: p.path.clone(), icon: pin_icon(&p.icon), label: Err(p.label.clone()), pin: true })
}

/// A row of the list: the separator of a group (with its handle while editing) or an entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Row {
    Sep(u32),
    Entry(String),
}

impl Row {
    fn key(&self) -> String {
        match self {
            Row::Sep(k) => format!("~{k}"),
            Row::Entry(id) => id.clone(),
        }
    }
}

/// The layout being shown and edited, with a stable key per group (for the separators and
/// their drag; not stored).
#[derive(Clone, Debug, PartialEq)]
pub struct Board {
    pub layout: SidebarLayout,
    pub keys: Vec<u32>,
    next: u32,
}

impl Board {
    pub fn new(layout: SidebarLayout) -> Self {
        let n = layout.groups.len() as u32;
        Board { layout, keys: (0..n).collect(), next: n }
    }

    fn find(&self, id: &str) -> Option<(usize, usize)> {
        self.layout.groups.iter().enumerate().find_map(|(g, v)| v.iter().position(|e| e == id).map(|i| (g, i)))
    }

    pub fn group_key_of(&self, id: &str) -> Option<u32> {
        self.find(id).map(|(g, _)| self.keys[g])
    }

    fn group_of_key(&self, key: u32) -> Option<usize> {
        self.keys.iter().position(|k| *k == key)
    }

    fn new_key(&mut self) -> u32 {
        self.next += 1;
        self.next - 1
    }

    /// Drops empty groups with their keys.
    fn tidy(&mut self) {
        let mut g = 0;
        while g < self.layout.groups.len() {
            if self.layout.groups[g].is_empty() {
                self.layout.groups.remove(g);
                self.keys.remove(g);
            } else {
                g += 1;
            }
        }
    }

    pub fn is_hidden(&self, id: &str) -> bool {
        self.layout.hidden.iter().any(|h| h == id)
    }

    pub fn set_hidden(&mut self, id: &str, hidden: bool) -> bool {
        if self.is_hidden(id) == hidden || self.find(id).is_none() {
            return false;
        }
        if hidden {
            self.layout.hidden.push(id.to_string());
        } else {
            self.layout.hidden.retain(|h| h != id);
        }
        true
    }

    /// One step up (`delta` < 0) or down among the entries `vis` accepts: past the first or
    /// last entry of a group it goes to the end of the group above or the start of the one
    /// below; past the very first or last it starts a group of its own.
    pub fn move_entry(&mut self, id: &str, delta: i32, vis: &dyn Fn(&str) -> bool) -> bool {
        let Some((g, i)) = self.find(id) else { return false };
        let alone = self.layout.groups[g].iter().filter(|e| vis(e)).count() <= 1;
        let can_add = self.layout.groups.len() < MAX_GROUPS;
        let groups = &mut self.layout.groups;
        if delta < 0 {
            if let Some(j) = (0..i).rev().find(|&j| vis(&groups[g][j])) {
                let e = groups[g].remove(i);
                groups[g].insert(j, e);
            } else if let Some(pg) = (0..g).rev().find(|&pg| groups[pg].iter().any(|e| vis(e))) {
                let e = groups[g].remove(i);
                groups[pg].push(e);
            } else if !alone && can_add {
                let e = groups[g].remove(i);
                groups.insert(0, vec![e]);
                let k = self.new_key();
                self.keys.insert(0, k);
            } else {
                return false;
            }
        } else if let Some(j) = (i + 1..groups[g].len()).find(|&j| vis(&groups[g][j])) {
            let e = groups[g].remove(i);
            groups[g].insert(j, e);
        } else if let Some(ng) = (g + 1..groups.len()).find(|&ng| groups[ng].iter().any(|e| vis(e))) {
            let e = groups[g].remove(i);
            groups[ng].insert(0, e);
        } else if !alone && can_add {
            let e = groups[g].remove(i);
            groups.push(vec![e]);
            let k = self.new_key();
            self.keys.push(k);
        } else {
            return false;
        }
        self.tidy();
        true
    }

    /// Drag: `id` goes where `target` is (before it when coming from a group below… after it
    /// when coming from below in the same group or from a group below).
    pub fn move_to(&mut self, id: &str, target: &str) -> bool {
        if id == target {
            return false;
        }
        let (Some((g, i)), Some((tg, ti))) = (self.find(id), self.find(target)) else { return false };
        let e = self.layout.groups[g].remove(i);
        let at = if g == tg { ti } else if g > tg { ti + 1 } else { ti };
        let at = at.min(self.layout.groups[tg].len());
        self.layout.groups[tg].insert(at, e);
        self.tidy();
        true
    }

    /// One step for the group `key`, past the next group with entries `vis` accepts.
    pub fn move_group(&mut self, key: u32, delta: i32, vis: &dyn Fn(&str) -> bool) -> bool {
        let Some(g) = self.group_of_key(key) else { return false };
        let has = |v: &Vec<String>| v.iter().any(|e| vis(e));
        let to = if delta < 0 {
            (0..g).rev().find(|&x| has(&self.layout.groups[x]))
        } else {
            (g + 1..self.layout.groups.len()).find(|&x| has(&self.layout.groups[x]))
        };
        let Some(to) = to else { return false };
        let grp = self.layout.groups.remove(g);
        let k = self.keys.remove(g);
        self.layout.groups.insert(to, grp);
        self.keys.insert(to, k);
        true
    }

    /// Drag of a group: it goes where the group `target` is.
    pub fn move_group_to(&mut self, key: u32, target: u32) -> bool {
        let (Some(g), Some(to)) = (self.group_of_key(key), self.group_of_key(target)) else { return false };
        if g == to {
            return false;
        }
        let grp = self.layout.groups.remove(g);
        let k = self.keys.remove(g);
        self.layout.groups.insert(to, grp);
        self.keys.insert(to, k);
        true
    }

    /// A free pin id (`p1`, `p2`…).
    pub fn next_pin_id(&self) -> String {
        (1..).map(|n| format!("p{n}")).find(|id| self.layout.pin(id).is_none()).unwrap_or_default()
    }

    /// Pins a page at the end of the last group.
    pub fn add_pin(&mut self, path: String, label: String, icon: String) -> Option<String> {
        if self.layout.pins.len() >= MAX_PINS {
            return None;
        }
        let id = self.next_pin_id();
        self.layout.pins.push(SidebarPin { id: id.clone(), path, label, icon });
        match self.layout.groups.last_mut() {
            Some(g) => g.push(id.clone()),
            None => {
                self.layout.groups.push(vec![id.clone()]);
                let k = self.new_key();
                self.keys.push(k);
            }
        }
        Some(id)
    }

    pub fn update_pin(&mut self, id: &str, path: String, label: String, icon: String) -> bool {
        let Some(p) = self.layout.pins.iter_mut().find(|p| p.id == id) else { return false };
        *p = SidebarPin { id: id.to_string(), path, label, icon };
        true
    }

    pub fn remove_pin(&mut self, id: &str) -> bool {
        if self.layout.pin(id).is_none() {
            return false;
        }
        self.layout.pins.retain(|p| p.id != id);
        self.layout.hidden.retain(|h| h != id);
        for g in &mut self.layout.groups {
            g.retain(|e| e != id);
        }
        self.tidy();
        true
    }

    /// Whether a pin already opens `path`.
    pub fn pinned(&self, path: &str) -> bool {
        self.layout.pins.iter().any(|p| p.path == path)
    }

    /// The rows on screen: groups with at least one entry `vis` accepts, a separator between
    /// them (while editing, one above every group, the handle that moves it, when there is
    /// more than one group).
    pub fn rows(&self, editing: bool, vis: &dyn Fn(&str) -> bool) -> Vec<Row> {
        let groups: Vec<(u32, Vec<&String>)> = self
            .layout
            .groups
            .iter()
            .zip(&self.keys)
            .map(|(g, k)| (*k, g.iter().filter(|e| vis(e)).collect::<Vec<_>>()))
            .filter(|(_, v)| !v.is_empty())
            .collect();
        let many = groups.len() > 1;
        let mut out = Vec::new();
        for (n, (k, entries)) in groups.into_iter().enumerate() {
            if (editing && many) || (!editing && n > 0) {
                out.push(Row::Sep(k));
            }
            out.extend(entries.into_iter().map(|e| Row::Entry(e.clone())));
        }
        out
    }

    /// 1-based number of the group `key` among the groups on screen.
    pub fn group_number(&self, key: u32, vis: &dyn Fn(&str) -> bool) -> usize {
        self.layout
            .groups
            .iter()
            .zip(&self.keys)
            .filter(|(g, _)| g.iter().any(|e| vis(e)))
            .position(|(_, k)| *k == key)
            .map_or(0, |p| p + 1)
    }

    /// (group number, position in it) of `id` among the entries on screen, 1-based.
    pub fn place(&self, id: &str, vis: &dyn Fn(&str) -> bool) -> (usize, usize) {
        let Some((g, _)) = self.find(id) else { return (0, 0) };
        let pos = self.layout.groups[g].iter().filter(|e| vis(e)).position(|e| e == id).map_or(0, |p| p + 1);
        (self.group_number(self.keys[g], vis), pos)
    }
}

/// What the pin dialog edits: a new pin of the current page, or a pin by id.
#[derive(Clone, Debug, PartialEq)]
pub enum PinOpen {
    New,
    Edit(String),
}

/// The sidebar state the shell shares through context.
///
/// Saving rules: nothing is written before the user's layout has been read (`loaded`: a failed
/// read is retried, and editing waits for it), so a default board can never overwrite a saved
/// layout; one save at a time, then the latest board if it changed meanwhile; a pending change
/// is sent before sign-out ([`flush`]) and when the page goes away (`pagehide`, `keepalive`).
#[derive(Clone, Copy)]
pub struct NavLayout {
    pub board: RwSignal<Board>,
    /// The user's layout has been read (or there is none): editing and saving may start.
    pub loaded: RwSignal<bool>,
    pub editing: RwSignal<bool>,
    pub pin_dialog: RwSignal<Option<PinOpen>>,
    pub reset_ask: RwSignal<bool>,
    pub dragging: RwSignal<Option<String>>,
    /// Text for the aria-live region (keyboard moves).
    pub announce: RwSignal<String>,
    pub list: NodeRef<Ul>,
    /// Bumped by every change and by sign-out: a delayed save runs only for the latest change.
    generation: StoredValue<u32>,
    /// Bumped by sign-out (and a new load): an answer of an older load is dropped.
    epoch: StoredValue<u32>,
    /// A save is on its way; `again`: the board changed meanwhile, send it after.
    in_flight: StoredValue<bool>,
    again: StoredValue<bool>,
}

thread_local! {
    /// The layout not saved yet (`None`: nothing pending): what [`flush`] and `pagehide` send.
    static PENDING: std::cell::RefCell<Option<SidebarLayout>> = const { std::cell::RefCell::new(None) };
}

fn set_pending(l: Option<SidebarLayout>) {
    PENDING.with(|p| *p.borrow_mut() = l);
}

fn take_pending() -> Option<SidebarLayout> {
    PENDING.with(|p| p.borrow_mut().take())
}

pub fn nav_layout() -> NavLayout {
    expect_context::<NavLayout>()
}

/// Entries on screen while editing: shown by the system and not hidden.
fn vis_of(b: &Board) -> impl Fn(&str) -> bool + '_ {
    move |id: &str| shown(id) && !b.is_hidden(id)
}

async fn send(layout: &SidebarLayout) -> Result<(), crate::api::ApiErr> {
    if layout.is_default() {
        crate::api::delete_empty(URL).await
    } else {
        crate::api::put::<_, SidebarPrefs>(URL, layout).await.map(|_| ())
    }
}

/// Sends a change not saved yet and waits for the answer (before sign-out).
pub async fn flush() {
    if let Some(l) = take_pending()
        && let Err(e) = send(&l).await
    {
        leptos::logging::warn!("sidebar layout not saved before sign-out: {}", e.text());
    }
}

/// The page goes away: a change not saved yet leaves with a `keepalive` request (the browser
/// finishes it after the page is gone).
fn flush_keepalive() {
    use wasm_bindgen::JsValue;
    let Some(l) = take_pending() else { return };
    let Some(token) = crate::auth::csrf_token() else { return };
    let init = web_sys::RequestInit::new();
    // (no typed setter in this web-sys: the plain property of the init dictionary)
    let _ = js_sys::Reflect::set(&init, &JsValue::from_str("keepalive"), &JsValue::TRUE);
    let Ok(headers) = web_sys::Headers::new() else { return };
    let _ = headers.set("Accept", "application/json");
    let _ = headers.set(rynas_api_types::CSRF_HEADER, &token);
    if l.is_default() {
        init.set_method("DELETE");
    } else {
        let Ok(body) = serde_json::to_string(&l) else { return };
        init.set_method("PUT");
        let _ = headers.set("Content-Type", "application/json");
        init.set_body(&JsValue::from_str(&body));
    }
    init.set_headers(&headers);
    let _ = window().fetch_with_str_and_init(URL, &init);
}

impl NavLayout {
    pub fn new() -> Self {
        let nav = NavLayout {
            board: RwSignal::new(Board::new(SidebarLayout::default_layout())),
            loaded: RwSignal::new(false),
            editing: RwSignal::new(false),
            pin_dialog: RwSignal::new(None),
            reset_ask: RwSignal::new(false),
            dragging: RwSignal::new(None),
            announce: RwSignal::new(String::new()),
            list: NodeRef::new(),
            generation: StoredValue::new(0),
            epoch: StoredValue::new(0),
            in_flight: StoredValue::new(false),
            again: StoredValue::new(false),
        };
        let _ = window_event_listener(leptos::ev::pagehide, |_| flush_keepalive());
        nav
    }

    /// The user's layout from the NAS (after sign-in); merged with this release's default.
    /// A failed read is tried again (2 s, then up to every 60 s) while the same session lasts:
    /// until then the sidebar shows the default and cannot be edited.
    pub fn load(self) {
        self.epoch.update_value(|e| *e += 1);
        let epoch = self.epoch.get_value();
        leptos::task::spawn_local(async move {
            let mut wait = 2_000u32;
            loop {
                let r = crate::api::get::<SidebarPrefs>(URL).await;
                // signed out, or another load started meanwhile: this answer is not for now
                if self.epoch.try_get_value() != Some(epoch) {
                    return;
                }
                match r {
                    Ok(p) => {
                        self.board.set(Board::new(ui_prefs::merge(p.layout.as_ref())));
                        self.loaded.set(true);
                        return;
                    }
                    Err(e) => leptos::logging::warn!("sidebar layout not loaded: {}", e.text()),
                }
                crate::api::sleep(wait).await;
                if self.epoch.try_get_value() != Some(epoch) {
                    return;
                }
                wait = (wait * 2).min(60_000);
            }
        });
    }

    /// Signed out: back to the default, nothing open, nothing pending.
    pub fn clear(self) {
        self.generation.update_value(|g| *g += 1);
        self.epoch.update_value(|e| *e += 1);
        self.again.set_value(false);
        set_pending(None);
        self.loaded.set(false);
        self.editing.set(false);
        self.pin_dialog.set(None);
        self.reset_ask.set(false);
        self.board.set(Board::new(SidebarLayout::default_layout()));
    }

    /// Enters the editing mode (once the layout is there) and focuses the first handle.
    pub fn start_editing(self) {
        if !self.loaded.get_untracked() {
            return;
        }
        self.editing.set(true);
        request_animation_frame(|| {
            request_animation_frame(|| {
                if let Ok(Some(el)) = document().query_selector(".sidebar .nav .drag-handle")
                    && let Ok(el) = el.dyn_into::<HtmlElement>()
                {
                    let _ = el.focus();
                }
            })
        });
    }

    /// "Customize sidebar" from anywhere: on a narrow screen the sidebar is the drawer, so it
    /// opens; a collapsed sidebar shows its names while editing (and collapses again after).
    pub fn open_editor(self, ui: crate::prefs::Ui) {
        if !self.loaded.get_untracked() {
            return;
        }
        let narrow = window().match_media("(max-width: 1023px)").ok().flatten().is_some_and(|m| m.matches());
        if narrow {
            ui.drawer_open.set(true);
        }
        self.start_editing();
    }

    /// Applies `f` and, when it changed something and `save`, saves shortly after; `save:
    /// false` only shows it (a drag in progress).
    fn change_with(self, save: bool, f: impl FnOnce(&mut Board) -> bool) -> bool {
        if !self.loaded.get_untracked() {
            return false;
        }
        let mut b = self.board.get_untracked();
        if !f(&mut b) {
            return false;
        }
        self.board.set(b);
        if save {
            self.mark_changed();
        }
        true
    }

    /// The board changed: it is pending, saved half a second after the last change.
    fn mark_changed(self) {
        set_pending(Some(self.board.with_untracked(|b| b.layout.clone())));
        self.generation.update_value(|g| *g += 1);
        let mine = self.generation.get_value();
        set_timeout(
            move || {
                if self.generation.try_get_value() == Some(mine) {
                    self.save_now();
                }
            },
            std::time::Duration::from_millis(500),
        );
    }

    /// Saves the pending change now, one request at a time (the latest board goes after the
    /// one on its way). Nothing in the demo or before the layout was read.
    pub fn save_now(self) {
        if !self.loaded.get_untracked() || crate::auth::auth().phase.get_untracked() != crate::auth::Phase::Ready {
            return;
        }
        if self.in_flight.get_value() {
            self.again.set_value(true);
            return;
        }
        let Some(layout) = take_pending() else { return };
        self.in_flight.set_value(true);
        let epoch = self.epoch.get_value();
        leptos::task::spawn_local(async move {
            let r = send(&layout).await;
            if self.in_flight.try_set_value(false).is_some() {
                // the shell is gone
                return;
            }
            if self.epoch.try_get_value() != Some(epoch) {
                return;
            }
            if let Err(e) = r {
                // kept pending (unless a newer change is): Done or the next change tries again
                PENDING.with(|p| {
                    p.borrow_mut().get_or_insert(layout);
                });
                crate::toast::toast(Tone::Err, tf("nav.save_failed", &[("error", &e.text())]));
            }
            if self.again.try_get_value() == Some(true) {
                self.again.set_value(false);
                self.save_now();
            }
        });
    }

    /// Back to the default layout, saved at once.
    pub fn reset(self) {
        if !self.loaded.get_untracked() {
            return;
        }
        let container = self.container();
        reorder::flip(container, "data-nav", None, move || self.board.set(Board::new(SidebarLayout::default_layout())));
        set_pending(Some(SidebarLayout::default_layout()));
        self.generation.update_value(|g| *g += 1);
        self.save_now();
    }

    /// Ends the editing mode, saving a change still pending; the focus goes back to the
    /// Customize button.
    pub fn done(self) {
        self.editing.set(false);
        self.generation.update_value(|g| *g += 1);
        self.save_now();
        request_animation_frame(|| {
            if let Ok(Some(el)) = document().query_selector(".sidebar .customize-btn")
                && let Ok(el) = el.dyn_into::<HtmlElement>()
            {
                let _ = el.focus();
            }
        });
    }

    fn container(self) -> Option<HtmlElement> {
        self.list.get_untracked().map(|u| u.unchecked_into::<HtmlElement>())
    }

    /// A change with the FLIP animation of the rows.
    fn animate(self, skip: Option<String>, f: impl FnOnce(&mut Board) -> bool + 'static) -> bool {
        self.animate_with(skip, true, f)
    }

    fn animate_with(self, skip: Option<String>, save: bool, f: impl FnOnce(&mut Board) -> bool + 'static) -> bool {
        let mut changed = false;
        let changed_ref = &mut changed;
        reorder::flip(self.container(), "data-nav", skip, move || *changed_ref = self.change_with(save, f));
        changed
    }

    /// The hooks of a drag: moves are only shown while it lasts; a drop that changed the board
    /// is saved, Esc puts the board back and saves nothing.
    fn drag_hooks(self, dragged: String, over: impl Fn(String) + 'static) -> reorder::DragHooks {
        let original = self.board.get_untracked();
        let canceled = StoredValue::new(false);
        self.dragging.set(Some(dragged));
        reorder::DragHooks {
            over: Rc::new(over),
            cancel: Rc::new({
                let original = original.clone();
                move || {
                    canceled.set_value(true);
                    let original = original.clone();
                    reorder::flip(self.container(), "data-nav", None, move || self.board.set(original));
                }
            }),
            end: Rc::new(move || {
                self.dragging.try_set(None);
                if canceled.try_get_value() == Some(false) && self.board.try_with_untracked(|b| *b != original) == Some(true) {
                    self.mark_changed();
                }
            }),
        }
    }

    /// The handle to focus after the row `id` goes away: the next row's, else the previous.
    fn focus_neighbor_of(self, id: &str) {
        let rows = self.board.with_untracked(|b| b.rows(true, &vis_of(b)));
        let ids: Vec<String> = rows.into_iter().filter_map(|r| if let Row::Entry(e) = r { Some(e) } else { None }).collect();
        let at = ids.iter().position(|e| e == id);
        let next = at.and_then(|i| ids.get(i + 1).or_else(|| i.checked_sub(1).and_then(|p| ids.get(p)))).cloned();
        request_animation_frame(move || match next {
            Some(n) => reorder::focus_handle("data-nav", &n),
            None => {
                if let Ok(Some(el)) = document().query_selector(".nav-edit .btn.primary")
                    && let Ok(el) = el.dyn_into::<HtmlElement>()
                {
                    let _ = el.focus();
                }
            }
        });
    }
}

/// The entries of the sidebar (`.nav`): links, or the rows of the editing mode.
#[component]
pub fn NavList() -> impl IntoView {
    let nav = nav_layout();
    let location = use_location();
    let rows = Memo::new(move |_| {
        let editing = nav.editing.get();
        nav.board.with(|b| b.rows(editing, &vis_of(b)))
    });
    let current = Memo::new(move |_| {
        let p = location.pathname.get();
        nav.board.with(|b| current(&b.layout, &p, &shown))
    });
    view! {
        <div class="sr-only" aria-live="polite">{move || nav.announce.get()}</div>
        <ul class="nav" class:editing=move || nav.editing.get() node_ref=nav.list>
            <For each=move || rows.get() key=Row::key let:row>
                {match row {
                    Row::Sep(k) => sep_row(nav, k),
                    Row::Entry(id) => entry_row(nav, id, current),
                }}
            </For>
        </ul>
    }
}

fn sep_row(nav: NavLayout, key: u32) -> AnyView {
    let number = move || nav.board.with(|b| b.group_number(key, &vis_of(b))).to_string();
    let label = move || tf("nav.group", &[("n", &number())]);
    let on_key = move |e: KeyboardEvent| {
        let delta = match e.key().as_str() {
            "ArrowUp" | "ArrowLeft" => -1,
            "ArrowDown" | "ArrowRight" => 1,
            _ => return,
        };
        e.prevent_default();
        let moved = nav.animate(None, move |b| {
            let hidden = b.layout.hidden.clone();
            b.move_group(key, delta, &|id: &str| shown(id) && !hidden.iter().any(|h| h == id))
        });
        if moved {
            let count = nav.board.with_untracked(|b| b.rows(true, &vis_of(b)).iter().filter(|r| matches!(r, Row::Sep(_))).count());
            nav.announce.set(tf("nav.group_moved", &[("pos", &number()), ("count", &count.to_string())]));
            let id = format!("~{key}");
            request_animation_frame(move || reorder::focus_handle("data-sep", &id));
        }
    };
    let start = move |e: PointerEvent| {
        if e.button() != 0 {
            return;
        }
        let hooks = nav.drag_hooks(format!("~{key}"), move |target: String| {
            let target = target.trim_start_matches('~').parse::<u32>().ok().or_else(|| nav.board.with_untracked(|b| b.group_key_of(&target)));
            let Some(target) = target else { return };
            nav.animate_with(Some(format!("~{key}")), false, move |b| b.move_group_to(key, target));
        });
        reorder::start_drag(nav.container(), "data-nav", format!("~{key}"), e, true, hooks);
    };
    view! {
        <Show
            when=move || nav.editing.get()
            fallback=|| view! { <li class="nav-sep" role="separator"></li> }
        >
            <li class="nav-sep edit" data-nav=format!("~{key}") data-sep=format!("~{key}") class:dragging=move || nav.dragging.get().as_deref() == Some(format!("~{key}").as_str())>
                <button
                    type="button"
                    class="tool drag-handle"
                    aria-label=move || tf("nav.move_group", &[("n", &number())])
                    aria-describedby="nav-hint"
                    title=move || tf("nav.move_group", &[("n", &number())])
                    on:pointerdown=start
                    on:keydown=on_key
                >
                    <Icon icon=icons::GRIP_VERTICAL size=16 />
                </button>
                <span class="nav-sep-label">{label}</span>
            </li>
        </Show>
    }
    .into_any()
}

fn entry_row(nav: NavLayout, id: String, current: Memo<Option<String>>) -> AnyView {
    let id = StoredValue::new(id);
    let view_of = move || nav.board.with(|b| entry(&b.layout, &id.get_value()));
    let label = move || view_of().map(|v| v.label()).unwrap_or_default();
    let icon = move || view_of().map_or(icons::PIN, |v| v.icon);
    let path = move || view_of().map(|v| v.path).unwrap_or_default();
    let is_pin = move || view_of().is_some_and(|v| v.pin);
    let aria_current = move || (current.get().as_deref() == Some(id.get_value().as_str())).then_some("page");

    let on_key = move |e: KeyboardEvent| {
        let delta = match e.key().as_str() {
            "ArrowUp" | "ArrowLeft" => -1,
            "ArrowDown" | "ArrowRight" => 1,
            _ => return,
        };
        e.prevent_default();
        let me = id.get_value();
        let moved = nav.animate(None, {
            let me = me.clone();
            move |b| {
                let hidden = b.layout.hidden.clone();
                b.move_entry(&me, delta, &|x: &str| shown(x) && !hidden.iter().any(|h| h == x))
            }
        });
        if moved {
            let (g, p) = nav.board.with_untracked(|b| b.place(&me, &vis_of(b)));
            nav.announce.set(tf("nav.moved", &[("entry", &label()), ("group", &g.to_string()), ("pos", &p.to_string())]));
            request_animation_frame(move || reorder::focus_handle("data-nav", &me));
        }
    };
    let start = move |e: PointerEvent| {
        if e.button() != 0 {
            return;
        }
        let me = id.get_value();
        let hooks = nav.drag_hooks(me.clone(), {
            let me = me.clone();
            move |target: String| {
                // over a group's separator: to the start of that group
                let target = if let Some(k) = target.strip_prefix('~').and_then(|k| k.parse::<u32>().ok()) {
                    match nav.board.with_untracked(|b| b.group_of_key(k).and_then(|g| b.layout.groups[g].first().cloned())) {
                        Some(first) => first,
                        None => return,
                    }
                } else {
                    target
                };
                let me = me.clone();
                nav.animate_with(Some(me.clone()), false, move |b| b.move_to(&me, &target));
            }
        });
        reorder::start_drag(nav.container(), "data-nav", me, e, true, hooks);
    };
    let hide = move || {
        let me = id.get_value();
        nav.focus_neighbor_of(&me);
        nav.animate(None, move |b| b.set_hidden(&me, true));
    };
    let unpin = move || {
        let me = id.get_value();
        nav.focus_neighbor_of(&me);
        nav.animate(None, move |b| b.remove_pin(&me));
    };

    view! {
        <li data-nav=move || id.get_value() class:dragging=move || nav.dragging.get().as_deref() == Some(id.get_value().as_str())>
            <Show
                when=move || nav.editing.get()
                fallback=move || view! {
                    <a class="nav-item" href=path aria-current=aria_current title=label>
                        {move || view! { <Icon icon=icon() /> }}
                        <span class="nav-label">{label}</span>
                    </a>
                }
            >
                <div class="nav-item nav-edit-row">
                    <button
                        type="button"
                        class="tool drag-handle"
                        aria-label=move || verb_name("nav.move", &label())
                        aria-describedby="nav-hint"
                        title=move || verb_name("nav.move", &label())
                        on:pointerdown=start
                        on:keydown=on_key
                    >
                        <Icon icon=icons::GRIP_VERTICAL size=16 />
                    </button>
                    {move || view! { <Icon icon=icon() /> }}
                    <span class="nav-label">{label}</span>
                    <Show
                        when=is_pin
                        fallback=move || view! {
                            <button type="button" class="tool" aria-label=move || verb_name("nav.hide", &label()) title=move || verb_name("nav.hide", &label()) on:click=move |_| hide()>
                                <Icon icon=icons::EYE_OFF size=16 />
                            </button>
                        }
                    >
                        <button type="button" class="tool" aria-label=move || verb_name("edit", &label()) title=move || verb_name("edit", &label())
                            on:click=move |_| nav.pin_dialog.set(Some(PinOpen::Edit(id.get_value())))>
                            <Icon icon=icons::PENCIL size=16 />
                        </button>
                        <button type="button" class="tool danger" aria-label=move || verb_name("nav.unpin", &label()) title=move || verb_name("nav.unpin", &label()) on:click=move |_| unpin()>
                            <Icon icon=icons::PIN_OFF size=16 />
                        </button>
                    </Show>
                </div>
            </Show>
        </li>
    }
    .into_any()
}

/// The bottom of the sidebar while editing: hint, hidden entries, Pin this page, Reset, Done.
#[component]
pub fn EditBar() -> impl IntoView {
    let nav = nav_layout();
    let location = use_location();
    let hidden = Memo::new(move |_| nav.board.with(|b| b.layout.hidden.iter().filter(|h| shown(h)).cloned().collect::<Vec<_>>()));
    let here = move || format!("{}{}", location.pathname.get(), location.search.get().strip_prefix('?').map(|q| format!("?{q}")).unwrap_or_default());
    let pinned = move || nav.board.with(|b| b.pinned(&here()));
    let full = move || nav.board.with(|b| b.layout.pins.len() >= MAX_PINS);
    let pin_title = move || {
        if pinned() {
            t("nav.pinned_already").to_string()
        } else if full() {
            tf("nav.pins_full", &[("n", &MAX_PINS.to_string())])
        } else {
            t("nav.pin_page").to_string()
        }
    };
    view! {
        <div class="nav-edit" role="region" aria-label=tr("nav.editing")>
            <p id="nav-hint" class="edit-hint">{tr("nav.edit_hint")}</p>
            <Show when=move || !hidden.with(Vec::is_empty)>
                <div class="tray">
                    <span class="tray-label">{tr("nav.hidden")}</span>
                    <For each=move || hidden.get() key=|id| id.clone() let:h>
                        {
                            let h = StoredValue::new(h);
                            let name = move || nav.board.with(|b| entry(&b.layout, &h.get_value()).map(|e| e.label()).unwrap_or_default());
                            view! {
                                <button type="button" class="tray-item" aria-label=move || verb_name("nav.show", &name()) on:click=move |_| {
                                    let id = h.get_value();
                                    nav.animate(None, move |b| b.set_hidden(&id, false));
                                }>
                                    <Icon icon=icons::PLUS size=14 />{name}
                                </button>
                            }
                        }
                    </For>
                </div>
            </Show>
            <button type="button" class="btn ghost" disabled=move || pinned() || full() title=pin_title on:click=move |_| nav.pin_dialog.set(Some(PinOpen::New))>
                <Icon icon=icons::PIN size=16 />
                <span class="btn-label">{tr("nav.pin_page")}</span>
            </button>
            <div class="nav-edit-actions">
                <button type="button" class="btn ghost" on:click=move |_| nav.reset_ask.set(true)>
                    <Icon icon=icons::ROTATE_CCW size=16 />
                    <span class="btn-label">{tr("reset")}</span>
                </button>
                <button type="button" class="btn primary" on:click=move |_| nav.done()>
                    <Icon icon=icons::CHECK size=16 />{tr("dash.done")}
                </button>
            </div>
        </div>
    }
}

/// The name of the page on screen (its title), cut to what a pin may hold.
fn page_title(path: &str) -> String {
    let h1 = document()
        .query_selector("#main h1")
        .ok()
        .flatten()
        .and_then(|e| e.text_content())
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty());
    let text = h1.unwrap_or_else(|| route(path).rsplit('/').find(|s| !s.is_empty()).unwrap_or("rynas").to_string());
    text.chars().take(PIN_LABEL_MAX).collect()
}

/// The icon a new pin of `path` starts with: the one of its page in the palette, else of the
/// sidebar entry it belongs to, else the pin.
fn default_icon(layout: &SidebarLayout, path: &str) -> &'static str {
    crate::palette::page_icon(route(path))
        .and_then(icon_name)
        .or_else(|| {
            layout
                .order()
                .filter_map(|id| NAV.iter().find(|n| n.id == id))
                .filter_map(|n| match_len(n.path, route(path)).map(|l| (l, n.icon)))
                .max_by_key(|x| x.0)
                .and_then(|(_, svg)| icon_name(svg))
        })
        .unwrap_or("pin")
}

/// Pin the current page / edit a pin: name, address, icon.
#[component]
pub fn PinDialog() -> impl IntoView {
    let nav = nav_layout();
    let location = use_location();
    view! {
        <Show when=move || nav.pin_dialog.with(Option::is_some)>
            {move || {
                let open = nav.pin_dialog.get_untracked().unwrap_or(PinOpen::New);
                let existing = match &open {
                    PinOpen::Edit(id) => nav.board.with_untracked(|b| b.layout.pin(id).cloned()),
                    PinOpen::New => None,
                };
                let (path0, label0, icon0) = match &existing {
                    Some(p) => (p.path.clone(), p.label.clone(), p.icon.clone()),
                    None => {
                        let search = location.search.get_untracked();
                        let path = format!("{}{}", location.pathname.get_untracked(), if search.is_empty() || search.starts_with('?') { search } else { format!("?{search}") });
                        let icon = nav.board.with_untracked(|b| default_icon(&b.layout, &path)).to_string();
                        (path.clone(), page_title(&path), icon)
                    }
                };
                pin_form(nav, open, path0, label0, icon0)
            }}
        </Show>
    }
}

fn pin_form(nav: NavLayout, open: PinOpen, path0: String, label0: String, icon0: String) -> AnyView {
    use crate::form::{In, SaveBar, save_bar_with, text_field};
    let path = RwSignal::new(path0);
    let label = RwSignal::new(label0);
    let icon = RwSignal::new(icon0);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let editing_id = match &open {
        PinOpen::Edit(id) => Some(id.clone()),
        PinOpen::New => None,
    };
    let label_err = Signal::derive(move || {
        let l = label.get();
        (l.trim().is_empty() || l.chars().count() > PIN_LABEL_MAX).then(|| tf("nav.pin_name_bad", &[("n", &PIN_LABEL_MAX.to_string())]))
    });
    let path_err = Signal::derive(move || (!ui_prefs::valid_pin_path(path.get().trim())).then(|| t("nav.pin_path_bad").to_string()));
    let can_save = Signal::derive(move || label_err.with(Option::is_none) && path_err.with(Option::is_none));
    let close = Callback::new(move |_| nav.pin_dialog.set(None));
    let editing_id = StoredValue::new(editing_id);
    let save = Callback::new(move |_| {
        if !can_save.get_untracked() {
            crate::ui::focus_invalid();
            return;
        }
        let (p, l, i) = (path.get_untracked().trim().to_string(), label.get_untracked().trim().to_string(), icon.get_untracked());
        let ok = match editing_id.get_value() {
            Some(id) => nav.animate(None, move |b| b.update_pin(&id, p, l, i)) || true,
            None => nav.animate(None, move |b| b.add_pin(p, l, i).is_some()),
        };
        if ok {
            nav.pin_dialog.set(None);
        } else {
            error.set(Some(tf("nav.pins_full", &[("n", &MAX_PINS.to_string())])));
        }
    });
    let is_new = matches!(open, PinOpen::New);
    let title = Signal::derive(move || t(if is_new { "nav.pin_title" } else { "nav.pin_edit_title" }).to_string());
    let foot = save_bar_with(SaveBar {
        can_save,
        error: Some(error),
        ..SaveBar::new(busy, if is_new { "nav.pin" } else { "save" }, close, save)
    });
    let icon_label = "nav-pin-icon";
    let body = view! {
        {text_field("nav.pin_name", label.into(), Callback::new(move |v| label.set(v)), label_err, In { maxlength: PIN_LABEL_MAX as u32, ..In::default() })}
        {text_field("nav.pin_path", path.into(), Callback::new(move |v| path.set(v)), path_err, In { mono: true, help: Some("nav.pin_path_help"), maxlength: ui_prefs::PIN_PATH_MAX as u32, ..In::default() })}
        <div class="field">
            <span class="field-label" id=icon_label>{tr("nav.pin_icon")}</span>
            {icon_picker(icon, icon_label)}
        </div>
    };
    crate::ui::dialog(crate::ui::DlgKind::Normal, title, close, body.into_any(), Some(foot))
}

/// A radio group of the pin icons; arrow keys move the choice (one tab stop).
fn icon_picker(icon: RwSignal<String>, labelled_by: &'static str) -> AnyView {
    let on_key = move |e: KeyboardEvent| {
        let n = PIN_ICON_UI.len() as i32;
        let at = PIN_ICON_UI.iter().position(|i| i.0 == icon.get_untracked()).unwrap_or(0) as i32;
        let to = match e.key().as_str() {
            "ArrowRight" | "ArrowDown" => (at + 1) % n,
            "ArrowLeft" | "ArrowUp" => (at + n - 1) % n,
            "Home" => 0,
            "End" => n - 1,
            _ => return,
        };
        e.prevent_default();
        let name = PIN_ICON_UI[to as usize].0;
        icon.set(name.to_string());
        if let Ok(Some(el)) = document().query_selector(&format!(".icon-pick [data-icon='{name}']"))
            && let Ok(el) = el.dyn_into::<HtmlElement>()
        {
            let _ = el.focus();
        }
    };
    view! {
        <div class="icon-pick" role="radiogroup" aria-labelledby=labelled_by on:keydown=on_key>
            {PIN_ICON_UI
                .iter()
                .map(|&(name, svg, key)| {
                    let on = move || icon.with(|i| i == name);
                    view! {
                        <button
                            type="button"
                            role="radio"
                            data-icon=name
                            class="icon-choice"
                            class:on=on
                            aria-checked=move || on().to_string()
                            tabindex=move || if on() { "0" } else { "-1" }
                            aria-label=tr(key)
                            title=tr(key)
                            on:click=move |_| icon.set(name.to_string())
                        >
                            <Icon icon=svg size=18 />
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
    .into_any()
}

/// "Reset the sidebar?" (pinned pages go too).
#[component]
pub fn ResetAsk() -> impl IntoView {
    let nav = nav_layout();
    view! {
        <Show when=move || nav.reset_ask.get()>
            {move || {
                let c = crate::ui::Confirm::new(
                    t("nav.reset_ask").to_string(),
                    t("nav.reset_text").to_string(),
                    "reset",
                    false,
                    Callback::new(move |_| {
                        nav.reset_ask.set(false);
                        nav.reset();
                    }),
                    Callback::new(move |_| nav.reset_ask.set(false)),
                );
                crate::ui::confirm(c)
            }}
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(_: &str) -> bool {
        true
    }

    fn flat(b: &Board) -> Vec<Vec<&str>> {
        b.layout.groups.iter().map(|g| g.iter().map(String::as_str).collect()).collect()
    }

    #[test]
    fn nav_matches_the_shared_default() {
        // every built-in entry of the API's default has a navigation item and back
        let ids: Vec<&str> = ui_prefs::SIDEBAR_DEFAULT.iter().flat_map(|g| g.iter().copied()).collect();
        let nav: Vec<&str> = NAV.iter().map(|n| n.id).collect();
        let mut a = ids.clone();
        let mut b = nav.clone();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
        // and the pin icons are the API's, in the same order
        let names: Vec<&str> = PIN_ICON_UI.iter().map(|i| i.0).collect();
        assert_eq!(names, ui_prefs::PIN_ICONS);
    }

    #[test]
    fn bottom_bar_takes_the_first_entries_on_screen() {
        let mut l = SidebarLayout::default_layout();
        assert_eq!(bottom_ids(&l, &all, BOTTOM_ENTRIES), ["dashboard", "storage", "files"]);
        // hidden entries and entries the system hides are skipped; pins count
        l.hidden.push("storage".into());
        let mut b = Board::new(l);
        b.add_pin("/system/scheduler".into(), "Scheduled tasks".into(), "calendar-clock".into());
        assert!(b.move_to("p1", "dashboard"));
        let no_files = |id: &str| id != "files";
        assert_eq!(bottom_ids(&b.layout, &no_files, BOTTOM_ENTRIES), ["dashboard", "p1", "shares"]);
        // Docker first but not installed: not in the bar
        assert!(b.move_to("docker", "p1"));
        let no_docker = |id: &str| id != "docker";
        assert_eq!(bottom_ids(&b.layout, &no_docker, 2), ["dashboard", "p1"]);
    }

    #[test]
    fn current_entry_is_the_best_match() {
        let mut b = Board::new(SidebarLayout::default_layout());
        assert_eq!(current(&b.layout, "/system/scheduler", &all).as_deref(), Some("system"));
        assert_eq!(current(&b.layout, "/", &all).as_deref(), Some("dashboard"));
        assert_eq!(current(&b.layout, "/storagex", &all), None);
        b.add_pin("/system/scheduler?tab=history".into(), "Scheduled tasks".into(), "calendar-clock".into());
        assert_eq!(current(&b.layout, "/system/scheduler", &all).as_deref(), Some("p1"));
        // a hidden entry is not highlighted
        b.set_hidden("p1", true);
        assert_eq!(current(&b.layout, "/system/scheduler", &all).as_deref(), Some("system"));
    }

    #[test]
    fn keyboard_moves_cross_groups() {
        let mut b = Board::new(SidebarLayout::default_layout());
        // first of group 2 up: end of group 1
        assert!(b.move_entry("storage", -1, &all));
        assert_eq!(flat(&b)[0], ["dashboard", "storage"]);
        // and down again: start of group 2
        assert!(b.move_entry("storage", 1, &all));
        assert_eq!(flat(&b)[1][0], "storage");
        // within a group
        assert!(b.move_entry("storage", 1, &all));
        assert_eq!(flat(&b)[1][..2], ["files", "storage"]);
        // the only entry of the first group cannot go up; its group goes away when it leaves
        assert!(!b.move_entry("dashboard", -1, &all));
        assert!(b.move_entry("dashboard", 1, &all));
        assert_eq!(b.layout.groups.len(), 3);
        assert_eq!(b.keys.len(), 3);
        assert_eq!(flat(&b)[0][0], "dashboard");
        // last of the last group, with others: a group of its own
        assert!(b.move_entry("system", 1, &all));
        assert_eq!(flat(&b).last().unwrap(), &["system"]);
        assert_eq!(b.keys.len(), b.layout.groups.len());
        assert!(b.layout.validate().is_ok());
    }

    #[test]
    fn keyboard_moves_skip_what_is_not_on_screen() {
        let mut b = Board::new(SidebarLayout::default_layout());
        let no_docker = |id: &str| id != "docker";
        // services → down skips Docker (not installed): after it, before apps
        assert!(b.move_entry("services", 1, &no_docker));
        assert_eq!(flat(&b)[2], ["users", "docker", "apps", "services"]);
    }

    #[test]
    fn drag_moves_and_groups() {
        let mut b = Board::new(SidebarLayout::default_layout());
        // down in the same group: after the target
        assert!(b.move_to("storage", "shares"));
        assert_eq!(flat(&b)[1], ["files", "shares", "storage", "snapshots", "backup"]);
        // down into another group: before the target
        assert!(b.move_to("storage", "users"));
        assert_eq!(flat(&b)[2][..2], ["storage", "users"]);
        // up into another group: after the target
        assert!(b.move_to("storage", "dashboard"));
        assert_eq!(flat(&b)[0], ["dashboard", "storage"]);
        // groups
        let (k0, k3) = (b.keys[0], b.keys[3]);
        assert!(b.move_group_to(k3, k0));
        assert_eq!(flat(&b)[0], ["network", "hardware", "system"]);
        assert!(b.move_group(k3, 1, &all));
        assert_eq!(flat(&b)[1], ["network", "hardware", "system"]);
        assert!(!b.move_group(b.keys[0], -1, &all));
        assert!(b.layout.validate().is_ok());
    }

    #[test]
    fn pins_and_rows() {
        let mut b = Board::new(SidebarLayout::default_layout());
        assert_eq!(b.add_pin("/storage/volumes/data".into(), "data".into(), "hard-drive".into()).as_deref(), Some("p1"));
        assert_eq!(b.add_pin("/docker".into(), "Docker".into(), "container".into()).as_deref(), Some("p2"));
        assert!(b.pinned("/docker"));
        assert!(b.remove_pin("p1"));
        assert_eq!(b.next_pin_id(), "p1");
        assert!(b.layout.validate().is_ok());
        for i in 0..MAX_PINS {
            b.add_pin(format!("/x{i}"), "x".into(), "pin".into());
        }
        assert_eq!(b.layout.pins.len(), MAX_PINS);
        assert!(b.add_pin("/y".into(), "y".into(), "pin".into()).is_none());

        // rows: a separator between groups; while editing one above each group
        let b = Board::new(SidebarLayout::default_layout());
        let rows = b.rows(false, &all);
        assert_eq!(rows.iter().filter(|r| matches!(r, Row::Sep(_))).count(), 3);
        assert_eq!(rows[0], Row::Entry("dashboard".into()));
        let rows = b.rows(true, &all);
        assert_eq!(rows.iter().filter(|r| matches!(r, Row::Sep(_))).count(), 4);
        // a group with nothing on screen has no separator
        let only_docker_group = |id: &str| !["users", "services", "docker", "apps"].contains(&id);
        assert_eq!(b.rows(false, &only_docker_group).iter().filter(|r| matches!(r, Row::Sep(_))).count(), 2);
        assert_eq!(b.group_number(b.keys[3], &only_docker_group), 3);
    }

    #[test]
    fn routes() {
        assert_eq!(match_len("/", "/"), Some(1));
        assert_eq!(match_len("/", "/storage"), None);
        assert_eq!(match_len("/storage", "/storage/volumes/a"), Some(8));
        assert_eq!(match_len("/storage", "/storagex"), None);
        assert_eq!(match_len("/docker?stack=web", "/docker"), Some(7));
    }
}
