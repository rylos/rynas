//! Command palette (Ctrl+K): fuzzy search over pages and actions, keyboard first.

use leptos::html::Input;
use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

use crate::i18n::{self, Lang, t, tr};
use crate::icons::{self, Icon};
use crate::prefs::{Density, ThemePref, use_ui};
use crate::shell::NAV;

#[derive(Clone)]
enum Action {
    Go(&'static str),
    /// A pinned page of the sidebar (its address).
    Open(String),
    EditSidebar,
    PinPage,
    Theme(ThemePref),
    Density(Density),
    Lang(Lang),
    ToggleSidebar,
    ToggleTasks,
    EditLayout,
    Notifications,
    SignOut,
}

#[derive(Clone)]
struct Entry {
    key: &'static str,
    /// The text instead of `key` (a pinned page's name).
    text: Option<String>,
    icon: &'static str,
    action: Action,
    /// Shortcut hint shown on the right.
    hint: &'static str,
}

impl Entry {
    fn label(&self) -> String {
        self.text.clone().unwrap_or_else(|| t(self.key).to_string())
    }
}

/// The icon of the page at `route` in the palette, if it lists it.
pub fn page_icon(route: &str) -> Option<&'static str> {
    entries().into_iter().find(|e| matches!(e.action, Action::Go(p) if p == route)).map(|e| e.icon)
}

fn entries() -> Vec<Entry> {
    let mut v: Vec<Entry> = NAV
        .iter()
        .map(|n| Entry { text: None, key: n.key, icon: n.icon, action: Action::Go(n.path), hint: "" })
        .collect();
    v.extend([
        Entry { text: None, key: "nav.security", icon: icons::SHIELD_CHECK, action: Action::Go("/security"), hint: "" },
        Entry { text: None, key: "wa.title", icon: icons::SHIELD_CHECK, action: Action::Go("/network/web-access"), hint: "" },
        Entry { text: None, key: "ts.title", icon: icons::LINK, action: Action::Go("/network/remote-access"), hint: "" },
        Entry { text: None, key: "ssh.title", icon: icons::COMMAND, action: Action::Go("/services/ssh"), hint: "" },
        Entry { text: None, key: "mem.title", icon: icons::MEMORY_STICK, action: Action::Go("/system/memory"), hint: "" },
        Entry { text: None, key: "proc.title", icon: icons::ACTIVITY, action: Action::Go("/system/processes"), hint: "" },
        Entry { text: None, key: "cfg.title", icon: icons::HISTORY, action: Action::Go("/system/configuration"), hint: "" },
        Entry { text: None, key: "sch.title", icon: icons::CALENDAR_CLOCK, action: Action::Go("/system/scheduler"), hint: "" },
        Entry { text: None, key: "upd.title", icon: icons::DOWNLOAD, action: Action::Go("/system/updates"), hint: "" },
        Entry { text: None, key: "nt.settings_title", icon: icons::BELL_RING, action: Action::Go("/system/notifications"), hint: "" },
        Entry { text: None, key: "nt.center", icon: icons::BELL, action: Action::Go("/notifications"), hint: "" },
        Entry { text: None, key: "bar.logout", icon: icons::LOG_OUT, action: Action::SignOut, hint: "" },
        Entry { text: None, key: "pal.edit_layout", icon: icons::SETTINGS, action: Action::EditLayout, hint: "" },
        Entry { text: None, key: "nav.customize", icon: icons::PENCIL, action: Action::EditSidebar, hint: "" },
        Entry { text: None, key: "pal.pin_page", icon: icons::PIN, action: Action::PinPage, hint: "" },
        Entry { text: None, key: "pal.notifications", icon: icons::BELL, action: Action::Notifications, hint: "" },
        Entry { text: None, key: "pal.theme_light", icon: icons::SUN, action: Action::Theme(ThemePref::Light), hint: "" },
        Entry { text: None, key: "pal.theme_dark", icon: icons::MOON, action: Action::Theme(ThemePref::Dark), hint: "" },
        Entry { text: None, key: "pal.theme_auto", icon: icons::MONITOR, action: Action::Theme(ThemePref::Auto), hint: "" },
        Entry { text: None, key: "pal.density_comfortable", icon: icons::ROWS_3, action: Action::Density(Density::Comfortable), hint: "" },
        Entry { text: None, key: "pal.density_compact", icon: icons::ROWS_3, action: Action::Density(Density::Compact), hint: "" },
        Entry { text: None, key: "pal.toggle_sidebar", icon: icons::PANEL_LEFT, action: Action::ToggleSidebar, hint: "" },
        Entry { text: None, key: "pal.toggle_tasks", icon: icons::LIST_CHECKS, action: Action::ToggleTasks, hint: "" },
    ]);
    // one entry per language of the table, named as the language names itself
    v.extend(Lang::all().map(|l| Entry { text: Some(i18n::tf("pal.lang", &[("name", l.name())])), key: "pal.lang", icon: icons::LANGUAGES, action: Action::Lang(l), hint: "" }));
    v
}

/// Subsequence fuzzy score (higher is better), None when `q` does not match.
/// Bonus for matches at word starts and for consecutive characters.
pub fn fuzzy(q: &str, text: &str) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut ti = 0;
    let mut prev: Option<usize> = None;
    for qc in q.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let mut found = None;
        while ti < t.len() {
            if t[ti] == qc {
                found = Some(ti);
                break;
            }
            ti += 1;
        }
        let i = found?;
        score += 1;
        if i == 0 || !t[i - 1].is_alphanumeric() {
            score += 8;
        }
        if prev.is_some_and(|p| p + 1 == i) {
            score += 5;
        }
        score -= (i as i32 - prev.map_or(0, |p| p as i32 + 1)).min(4);
        prev = Some(i);
        ti = i + 1;
    }
    Some(score)
}

#[component]
pub fn Palette() -> impl IntoView {
    let ui = use_ui();
    let navigate = use_navigate();
    let query = RwSignal::new(String::new());
    let active = RwSignal::new(0usize);
    let input = NodeRef::<Input>::new();
    let nav = crate::sidebar::nav_layout();
    // the pages and actions, with the pinned pages of the sidebar after the sidebar's pages
    // (hidden entries stay here: the palette always reaches every page)
    let all = Signal::derive(move || {
        let mut v = entries();
        let pins: Vec<Entry> = nav.board.with(|b| {
            b.layout
                .pins
                .iter()
                .map(|p| Entry { key: "", text: Some(p.label.clone()), icon: crate::sidebar::pin_icon(&p.icon), action: Action::Open(p.path.clone()), hint: "" })
                .collect()
        });
        let at = NAV.len().min(v.len());
        v.splice(at..at, pins);
        v
    });

    let results = Memo::new(move |_| {
        i18n::lang();
        let q = query.get();
        let mut r: Vec<(i32, usize)> = all.with(|es| {
            es.iter()
                .enumerate()
                .filter(|(_, e)| !matches!(e.action, Action::Go(p) if !crate::shell::nav_shown(p)))
                .filter_map(|(i, e)| {
                    let label = e.label();
                    let extra = match e.action {
                        Action::Go(_) | Action::Open(_) => t("pal.go").to_string(),
                        // a language is also found by its English name and code ("italian", "it")
                        Action::Lang(l) => format!("language {} {}", l.english(), l.code()),
                        _ => String::new(),
                    };
                    fuzzy(&q, &label).or_else(|| fuzzy(&q, &format!("{extra} {label}"))).map(|s| (s, i))
                })
                .collect()
        });
        if !q.is_empty() {
            r.sort_by_key(|e| std::cmp::Reverse(e.0));
            // drop scattered subsequence hits when there are good matches
            if let Some(&(best, _)) = r.first() {
                r.retain(|e| e.0 * 2 >= best);
            }
        }
        r.into_iter().map(|(_, i)| i).collect::<Vec<_>>()
    });

    // reset and focus on open
    Effect::new(move || {
        if ui.palette_open.get() {
            query.set(String::new());
            active.set(0);
            request_animation_frame(move || {
                if let Some(i) = input.get_untracked() {
                    let _ = i.focus();
                }
            });
        }
    });

    let run = move |idx: usize| {
        let Some(e) = all.with_untracked(|es| es.get(idx).cloned()) else { return };
        ui.palette_open.set(false);
        match e.action {
            Action::Go(p) => navigate(p, Default::default()),
            Action::Open(p) => navigate(&p, Default::default()),
            Action::EditSidebar => {
                nav.open_editor(ui);
            }
            Action::PinPage => {
                if nav.loaded.get_untracked() {
                    nav.pin_dialog.set(Some(crate::sidebar::PinOpen::New));
                }
            }
            Action::Theme(th) => ui.theme.set(th),
            Action::Density(d) => ui.density.set(d),
            Action::Lang(l) => i18n::set_lang(Some(l)),
            Action::ToggleSidebar => {
                ui.sidebar_collapsed.update(|c| *c = !*c);
                ui.relayout_soon();
            }
            Action::ToggleTasks => ui.tasks_open.update(|o| *o = !*o),
            Action::EditLayout => {
                navigate("/", Default::default());
                ui.dash_edit.set(true);
            }
            Action::Notifications => ui.notif_open.set(true),
            Action::SignOut => {
                let a = crate::auth::auth();
                if a.is_ready() {
                    a.sign_out();
                }
            }
        }
    };
    let run = StoredValue::new(run);

    let on_key = move |ev: leptos::ev::KeyboardEvent| {
        let n = results.with(Vec::len);
        match ev.key().as_str() {
            "ArrowDown" => {
                ev.prevent_default();
                if n > 0 {
                    active.update(|a| *a = (*a + 1) % n);
                }
            }
            "ArrowUp" => {
                ev.prevent_default();
                if n > 0 {
                    active.update(|a| *a = (*a + n - 1) % n);
                }
            }
            "Home" => active.set(0),
            "End" => active.set(n.saturating_sub(1)),
            "Enter" => {
                ev.prevent_default();
                if let Some(&idx) = results.with(|r| r.get(active.get_untracked()).copied()).as_ref() {
                    run.with_value(|f| f(idx));
                }
            }
            "Escape" | "Tab" => {
                ev.prevent_default();
                ui.palette_open.set(false);
            }
            _ => {}
        }
    };

    // keep the active row visible
    Effect::new(move || {
        let a = active.get();
        if ui.palette_open.get_untracked()
            && let Some(el) = document().get_element_by_id(&format!("pal-{a}"))
        {
            el.scroll_into_view_with_bool(false);
        }
    });

    // a native modal <dialog> like every other overlay (top layer, inert page behind)
    // (an effect, not `NodeRef::on_load`: that runs its closure once and panics when the `Show`
    // mounts the dialog a second time)
    let dlg = NodeRef::<leptos::html::Dialog>::new();
    Effect::new(move || {
        if let Some(d) = dlg.get()
            && !d.open()
        {
            let _ = d.show_modal();
        }
    });
    view! {
        <Show when=move || ui.palette_open.get()>
            <dialog
                class="palette"
                node_ref=dlg
                aria-label=tr("bar.search")
                on:cancel=move |e: web_sys::Event| {
                    e.prevent_default();
                    ui.palette_open.set(false);
                }
                on:click=move |e: web_sys::MouseEvent| {
                    if e.target() == e.current_target() {
                        ui.palette_open.set(false);
                    }
                }
            >
                <div class="pal-input">
                    <Icon icon=icons::SEARCH size=18 />
                    <input
                        node_ref=input
                        type="text"
                        role="combobox"
                        aria-expanded="true"
                        aria-controls="pal-list"
                        aria-autocomplete="list"
                        aria-activedescendant=move || format!("pal-{}", active.get())
                        placeholder=tr("pal.placeholder")
                        spellcheck="false"
                        autocomplete="off"
                        prop:value=move || query.get()
                        on:input=move |ev| {
                            query.set(event_target_value(&ev));
                            active.set(0);
                        }
                        on:keydown=on_key
                    />
                    <kbd>"Esc"</kbd>
                </div>
                <ul id="pal-list" class="pal-list" role="listbox">
                    {move || {
                        let rs = results.get();
                        if rs.is_empty() {
                            return view! { <li class="pal-empty">{t("pal.empty")}</li> }.into_any();
                        }
                        let mut last_group = None;
                        rs.into_iter()
                            .enumerate()
                            .map(|(pos, idx)| {
                                let e = all.with_untracked(|es| es[idx].clone());
                                let is_page = matches!(e.action, Action::Go(_) | Action::Open(_));
                                let header = (last_group != Some(is_page)).then(|| {
                                    last_group = Some(is_page);
                                    view! { <li class="pal-group" role="presentation">{t(if is_page { "pal.pages" } else { "col.actions" })}</li> }
                                });
                                view! {
                                    {header}
                                    <li
                                        id=format!("pal-{pos}")
                                        class="pal-item"
                                        role="option"
                                        aria-selected=move || (active.get() == pos).to_string()
                                        on:mousemove=move |_| if active.get_untracked() != pos { active.set(pos) }
                                        on:click=move |_| run.with_value(|f| f(idx))
                                    >
                                        <Icon icon=e.icon size=16 />
                                        <span class="pal-label">{e.label()}</span>
                                        {is_page.then(|| view! { <span class="pal-kind"><Icon icon=icons::CHEVRON_RIGHT size=14 /></span> })}
                                        {(!e.hint.is_empty()).then(|| view! { <kbd>{e.hint}</kbd> })}
                                    </li>
                                }
                            })
                            .collect_view()
                            .into_any()
                    }}
                </ul>
                <div class="pal-foot">
                    <span><kbd>"↑"</kbd><kbd>"↓"</kbd>" "{tr("pal.hint_nav")}</span>
                    <span><kbd>"↵"</kbd>" "{tr("pal.hint_open")}</span>
                    <span><kbd>"Esc"</kbd>" "{tr("pal.hint_close")}</span>
                </div>
            </dialog>
        </Show>
    }
}
