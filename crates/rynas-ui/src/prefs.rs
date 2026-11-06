//! User preferences (theme, density, sidebar) and transient UI state.
//!
//! Theme and density are applied on `<html>` as `data-theme` / `data-density`; the inline script
//! in `index.html` does the same before any CSS loads, so there is no white flash.

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

pub fn storage_get(key: &str) -> Option<String> {
    window().local_storage().ok().flatten()?.get_item(key).ok().flatten()
}

pub fn storage_set(key: &str, value: &str) {
    if let Some(s) = window().local_storage().ok().flatten() {
        let _ = if value.is_empty() { s.remove_item(key) } else { s.set_item(key, value) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePref {
    Auto,
    Light,
    Dark,
}

impl ThemePref {
    fn code(self) -> &'static str {
        match self {
            ThemePref::Auto => "auto",
            ThemePref::Light => "light",
            ThemePref::Dark => "dark",
        }
    }
    pub fn next(self) -> Self {
        match self {
            ThemePref::Auto => ThemePref::Light,
            ThemePref::Light => ThemePref::Dark,
            ThemePref::Dark => ThemePref::Auto,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Density {
    Comfortable,
    Compact,
}

/// Everything the shell shares through context. All fields are `Copy` signals.
#[derive(Clone, Copy)]
pub struct Ui {
    pub theme: RwSignal<ThemePref>,
    /// Effective theme: true when dark is showing (resolves `Auto`).
    pub dark: RwSignal<bool>,
    pub density: RwSignal<Density>,
    pub sidebar_collapsed: RwSignal<bool>,
    pub drawer_open: RwSignal<bool>,
    pub palette_open: RwSignal<bool>,
    pub tasks_open: RwSignal<bool>,
    pub user_menu_open: RwSignal<bool>,
    /// Dashboard "Customize" mode (widgets can be moved, resized and hidden).
    pub dash_edit: RwSignal<bool>,
    pub notif_open: RwSignal<bool>,
    /// The removable drives popover of the app bar.
    pub usb_open: RwSignal<bool>,
    /// Bumped when the layout width may have changed (resize, sidebar toggle): charts redraw.
    pub layout_tick: RwSignal<u32>,
}

fn system_dark() -> bool {
    window()
        .match_media("(prefers-color-scheme: dark)")
        .ok()
        .flatten()
        .map(|m| m.matches())
        .unwrap_or(false)
}

impl Ui {
    pub fn new() -> Self {
        let theme = match storage_get("rynas.theme").as_deref() {
            Some("light") => ThemePref::Light,
            Some("dark") => ThemePref::Dark,
            _ => ThemePref::Auto,
        };
        let density = match storage_get("rynas.density").as_deref() {
            Some("compact") => Density::Compact,
            _ => Density::Comfortable,
        };
        let ui = Ui {
            theme: RwSignal::new(theme),
            dark: RwSignal::new(false),
            density: RwSignal::new(density),
            sidebar_collapsed: RwSignal::new(storage_get("rynas.sidebar").as_deref() == Some("collapsed")),
            drawer_open: RwSignal::new(false),
            palette_open: RwSignal::new(false),
            tasks_open: RwSignal::new(false),
            user_menu_open: RwSignal::new(false),
            dash_edit: RwSignal::new(false),
            notif_open: RwSignal::new(false),
            usb_open: RwSignal::new(false),
            layout_tick: RwSignal::new(0),
        };
        ui.install();
        ui
    }

    fn install(self) {
        let root = document().document_element().expect("html element");
        // theme
        let r = root.clone();
        let dark = self.dark;
        Effect::new(move || {
            let pref = self.theme.get();
            let is_dark = match pref {
                ThemePref::Auto => system_dark(),
                ThemePref::Light => false,
                ThemePref::Dark => true,
            };
            let _ = r.set_attribute("data-theme", if is_dark { "dark" } else { "light" });
            // the inline boot style only covers the first paint
            let _ = r.remove_attribute("style");
            storage_set("rynas.theme", if pref == ThemePref::Auto { "" } else { pref.code() });
            dark.set(is_dark);
        });
        // follow the OS while on Auto
        if let Ok(Some(mq)) = window().match_media("(prefers-color-scheme: dark)") {
            let theme = self.theme;
            let cb = Closure::<dyn Fn()>::new(move || {
                if theme.get_untracked() == ThemePref::Auto {
                    theme.set(ThemePref::Auto);
                }
            });
            let _ = mq.add_event_listener_with_callback("change", cb.as_ref().unchecked_ref());
            cb.forget();
        }
        // density
        let r = root;
        Effect::new(move || {
            let compact = self.density.get() == Density::Compact;
            let _ = r.set_attribute("data-density", if compact { "compact" } else { "comfortable" });
            storage_set("rynas.density", if compact { "compact" } else { "" });
        });
        Effect::new(move || {
            let c = self.sidebar_collapsed.get();
            storage_set("rynas.sidebar", if c { "collapsed" } else { "" });
        });
        let tick = self.layout_tick;
        let _ = window_event_listener(leptos::ev::resize, move |_| tick.update(|t| *t += 1));
    }

    /// Redraw charts once the sidebar/drawer transition has finished.
    pub fn relayout_soon(self) {
        let tick = self.layout_tick;
        set_timeout(move || tick.update(|t| *t += 1), std::time::Duration::from_millis(220));
    }
}

pub fn use_ui() -> Ui {
    expect_context::<Ui>()
}
