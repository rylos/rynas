//! Layout A: app bar, sidebar (drawer < 1024 px, bottom bar < 768 px), routed content,
//! pending-changes bar and the Proxmox-style task panel.

use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::hooks::{use_location, use_query_map};
use leptos_router::path;

use crate::auth::{Auth, BootScreen, LoginScreen, Phase, SessionNotice, auth, role_key};
use crate::i18n::{t, tr};

use crate::icons::{self, Icon};
use crate::config::{Config, PendingBar};
use crate::live::{Conn, Live, use_live};
use crate::tasks::{TaskPanel, Tasks};
use crate::notify::{Bell, Notes};
use crate::pages;
use crate::palette::Palette;
use crate::prefs::{ThemePref, Ui, use_ui};

pub struct NavItem {
    pub path: &'static str,
    pub key: &'static str,
    pub icon: &'static str,
    /// Starts a new visual group in the sidebar.
    pub group: bool,
}

/// Whether a navigation entry exists now: Docker only while it is installed or being
/// installed (nothing of it on systems without Docker).
pub fn nav_shown(path: &str) -> bool {
    path != "/docker" || crate::docker::docker().present()
}

pub const NAV: [NavItem; 13] = [
    NavItem { path: "/", key: "nav.dashboard", icon: icons::LAYOUT_DASHBOARD, group: false },
    NavItem { path: "/storage", key: "nav.storage", icon: icons::HARD_DRIVE, group: true },
    NavItem { path: "/files", key: "nav.files", icon: icons::FOLDER, group: false },
    NavItem { path: "/shares", key: "nav.shares", icon: icons::SHARE_2, group: false },
    NavItem { path: "/snapshots", key: "nav.snapshots", icon: icons::CAMERA, group: false },
    NavItem { path: "/backup", key: "nav.backup", icon: icons::ARCHIVE_RESTORE, group: false },
    NavItem { path: "/users", key: "nav.users", icon: icons::USERS, group: true },
    NavItem { path: "/services", key: "nav.services", icon: icons::SERVER, group: false },
    NavItem { path: "/docker", key: "nav.docker", icon: icons::CONTAINER, group: false },
    NavItem { path: "/apps", key: "nav.apps", icon: icons::PUZZLE, group: false },
    NavItem { path: "/network", key: "nav.network", icon: icons::NETWORK, group: true },
    NavItem { path: "/hardware", key: "nav.hardware", icon: icons::CPU, group: false },
    NavItem { path: "/system", key: "nav.system", icon: icons::SETTINGS, group: false },
];

#[component]
pub fn App() -> impl IntoView {
    let ui = Ui::new();
    provide_context(ui);
    let auth = Auth::new();
    let live = Live::new();
    provide_context(live);
    let tasks = Tasks::new(ui.tasks_open, Signal::derive(move || live.conn.get() == Conn::Live));
    let config = Config::new();
    live.start();
    provide_context(Notes::new(live));

    // data and stream follow the session
    Effect::new(move |prev: Option<bool>| {
        let ready = auth.phase.get() == Phase::Ready;
        if ready && prev != Some(true) {
            live.resume();
        } else if !ready && prev == Some(true) {
            live.suspend();
            config.clear();
            tasks.clear();
            ui.user_menu_open.set(false);
            ui.palette_open.set(false);
            ui.notif_open.set(false);
            ui.usb_open.set(false);
        }
        ready
    });

    // global shortcuts
    let _ = window_event_listener(leptos::ev::keydown, move |e| {
        if !matches!(auth.phase.get_untracked(), Phase::Ready | Phase::Demo) {
            return;
        }
        let k = e.key();
        if (e.ctrl_key() || e.meta_key()) && k.eq_ignore_ascii_case("k") {
            e.prevent_default();
            ui.palette_open.update(|o| *o = !*o);
        } else if k == "Escape" {
            ui.drawer_open.set(false);
            ui.user_menu_open.set(false);
            ui.notif_open.set(false);
            ui.usb_open.set(false);
        }
    });

    // a press outside the bell and the user menu closes them (a fixed scrim cannot do it: the
    // app bar's backdrop-filter makes it the containing block, so the scrim covered only the bar)
    let _ = window_event_listener(leptos::ev::pointerdown, move |e| {
        if !(ui.user_menu_open.get_untracked() || ui.notif_open.get_untracked() || ui.usb_open.get_untracked()) {
            return;
        }
        let inside = e
            .target()
            .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
            .and_then(|el| el.closest(".pop-anchor").ok().flatten())
            .is_some();
        if !inside {
            ui.user_menu_open.set(false);
            ui.notif_open.set(false);
            ui.usb_open.set(false);
        }
    });

    let locked = move || auth.phase.get() == Phase::Locked;
    // the shell appears with the first session (or demo) and then stays mounted, covered by the
    // login screen while signed out, so the current route and page survive an expired session
    let shell = move || auth.shell_mounted.get() || auth.phase.get() == Phase::Demo;

    view! {
        <Show when=shell>
            <div class="shell-root" inert=locked aria-hidden=move || locked().then_some("true")>
                <Router>
                    <Shell />
                </Router>
            </div>
        </Show>
        <Show when=locked>
            <LoginScreen />
        </Show>
        <Show when=move || auth.phase.get() == Phase::Checking>
            <BootScreen />
        </Show>
    }
}

#[component]
fn Shell() -> impl IntoView {
    let ui = use_ui();
    let location = use_location();
    let query = use_query_map();
    pages::files::bridge();
    // close the drawer after navigating
    Effect::new(move || {
        location.pathname.track();
        ui.drawer_open.set(false);
    });
    // the file manager in a window of its own (`/files…?window`): no bars around it
    let bare = move || location.pathname.with(|p| p == "/files" || p.starts_with("/files/")) && query.with(|q| q.get("window").is_some());
    let app_class = move || {
        let mut c = String::from("app");
        if bare() {
            c.push_str(" bare");
        }
        if ui.sidebar_collapsed.get() {
            c.push_str(" collapsed");
        }
        if ui.drawer_open.get() {
            c.push_str(" drawer-open");
        }
        if ui.tasks_open.get() {
            c.push_str(" tasks-open");
        }
        c
    };
    let dock = NodeRef::<leptos::html::Div>::new();
    track_dock_height(dock);

    view! {
        <a class="skip" href="#main">{tr("skip")}</a>
        <div class=app_class>
            <AppBar />
            <Sidebar />
            <div class="scrim" on:click=move |_| ui.drawer_open.set(false)></div>
            <main id="main" class="main" tabindex="-1">
                <Routes fallback=|| view! { <pages::NotFound /> }>
                    <Route path=path!("/") view=pages::dashboard::Dashboard />
                    <Route path=path!("/storage") view=pages::storage::Storage />
                    <Route path=path!("/storage/disks") view=pages::storage::disks::Disks />
                    <Route path=path!("/storage/create") view=pages::storage::wizard::CreateVolume />
                    <Route path=path!("/storage/volumes/:name") view=pages::storage::volume::VolumePage />
                    <Route path=path!("/network") view=pages::network::Network />
                    <Route path=path!("/network/web-access") view=pages::web_access::WebAccess />
                    <Route path=path!("/network/remote-access") view=pages::network::remote::RemoteAccess />
                    <Route path=path!("/hardware") view=pages::hardware::Hardware />
                    <Route path=path!("/system") view=pages::system::System />
                    <Route path=path!("/system/configuration") view=pages::config::Configuration />
                    <Route path=path!("/system/memory") view=pages::memory::Memory />
                    <Route path=path!("/system/processes") view=pages::processes::Processes />
                    <Route path=path!("/system/notifications") view=pages::notifications::NotificationSettings />
                    <Route path=path!("/system/scheduler") view=pages::scheduler::Scheduler />
                    <Route path=path!("/system/time") view=pages::time::TimeSettings />
                    <Route path=path!("/system/updates") view=pages::updates::Updates />
                    <Route path=path!("/notifications") view=pages::notifications::NotificationsPage />
                    <Route path=path!("/snapshots") view=pages::snapshots::Snapshots />
                    <Route path=path!("/backup") view=pages::backup::BackupPage />
                    <Route path=path!("/security") view=pages::security::Security />
                    <Route path=path!("/files") view=pages::files::Files />
                    <Route path=path!("/files/*path") view=pages::files::Files />
                    <Route path=path!("/shares") view=pages::shares::Shares />
                    <Route path=path!("/users") view=pages::users::Users />
                    <Route path=path!("/services") view=pages::services::FileServices />
                    <Route path=path!("/services/ssh") view=pages::ssh::SshPage />
                    <Route path=path!("/apps") view=pages::apps::AppsPage />
                    <Route path=path!("/plugins") view=pages::apps::AppsPage />
                    <Route path=path!("/docker") view=pages::docker::DockerPage />
                    // dev-only component showcase (demo build; "not found" otherwise)
                    <Route path=path!("/dev/week-grid") view=crate::week_grid::Showcase />
                </Routes>
            </main>
            <div class="dock" node_ref=dock>
                <crate::power::PendingPower />
                <PendingBar />
                <TaskPanel />
            </div>
            <BottomBar />
        </div>
        <SessionNotice />
        <crate::toast::Toasts />
        <Palette />
        <crate::power::PowerHost />
    }
}

/// Keeps `--dock-h` on the root element equal to the dock's height (pending changes bar,
/// tasks bar, pending power action), so the toasts always sit just above it whatever it holds.
fn track_dock_height(dock: NodeRef<leptos::html::Div>) {
    use wasm_bindgen::{closure::Closure, JsCast};
    Effect::new(move |done: Option<bool>| {
        if done == Some(true) {
            return true;
        }
        let Some(el) = dock.get() else { return false };
        let set = |h: f64| {
            if let Some(root) = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.document_element())
                .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = root.style().set_property("--dock-h", &format!("{}px", h.ceil()));
            }
        };
        set(el.get_bounding_client_rect().height());
        let cb = Closure::<dyn Fn(js_sys::Array)>::new(move |entries: js_sys::Array| {
            if let Some(e) = entries.get(0).dyn_ref::<web_sys::ResizeObserverEntry>() {
                set(e.target().get_bounding_client_rect().height());
            }
        });
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&el);
            // one observer for the app's whole life
            std::mem::forget(ro);
        }
        cb.forget();
        true
    });
}

#[component]
fn AppBar() -> impl IntoView {
    let ui = use_ui();
    let live = use_live();
    let host = move || live.system.with(|s| s.as_ref().map(|s| s.hostname.clone()).unwrap_or_else(|| "…".into()));

    let conn = move || {
        let (cls, key) = match live.conn.get() {
            Conn::Live => ("conn live", "bar.live"),
            Conn::Connecting => ("conn connecting", "bar.connecting"),
            Conn::Offline => ("conn offline", "bar.offline"),
            Conn::Demo => ("conn demo", "bar.demo"),
        };
        let demo = live.conn.get() == Conn::Demo;
        view! {
            <span class=cls role="status" title=move || if demo { t("bar.demo_hint") } else { t(key) }>
                <span class="conn-dot" aria-hidden="true"></span>
                <span class="conn-text">{move || t(key)}</span>
            </span>
        }
    };

    let theme_icon = move || match ui.theme.get() {
        ThemePref::Auto => icons::MONITOR,
        ThemePref::Light => icons::SUN,
        ThemePref::Dark => icons::MOON,
    };
    let theme_label = move || {
        let cur = match ui.theme.get() {
            ThemePref::Auto => t("theme.auto"),
            ThemePref::Light => t("theme.light"),
            ThemePref::Dark => t("theme.dark"),
        };
        format!("{}: {cur}", t("bar.theme"))
    };

    view! {
        <header class="appbar">
            <button class="icon-btn only-drawer" aria-label=tr("bar.open_menu") on:click=move |_| ui.drawer_open.update(|o| *o = !*o)>
                <Icon icon=icons::MENU />
            </button>
            <a class="brand" href="/" aria-label="rynas">
                {crate::auth::logo()}
                <span class="brand-name">"rynas"</span>
            </a>
            <span class="host">
                <Icon icon=icons::SERVER size=14 />
                <span>{host}</span>
            </span>
            {conn}
            <button class="search" on:click=move |_| ui.palette_open.set(true) aria-haspopup="dialog">
                <Icon icon=icons::SEARCH size=16 />
                <span class="search-text">{tr("bar.search")}</span>
                <kbd>"Ctrl K"</kbd>
            </button>
            <div class="bar-actions">
                <crate::removable::UsbButton />
                <Bell />
                <button class="icon-btn" aria-label=theme_label title=theme_label on:click=move |_| ui.theme.update(|th| *th = th.next())>
                    {move || view! { <Icon icon=theme_icon() /> }}
                </button>
                <UserMenu />
            </div>
        </header>
    }
}

#[component]
fn UserMenu() -> impl IntoView {
    let ui = use_ui();
    let a = auth();
    let open = ui.user_menu_open;
    let demo = move || a.phase.get() == Phase::Demo;
    let name = move || {
        if demo() {
            "demo".to_string()
        } else {
            a.session.with(|s| s.as_ref().map(|s| s.user.username.clone()).unwrap_or_default())
        }
    };
    let initial = move || name().chars().next().map(|c| c.to_ascii_uppercase().to_string()).unwrap_or_default();
    let role = move || a.session.with(|s| s.as_ref().map(|s| t(role_key(s.user.role))).unwrap_or_else(|| t("role.admin")));
    let totp = move || a.session.with(|s| s.as_ref().is_some_and(|s| s.totp_enabled));
    view! {
        <div class="pop-anchor">
            <button
                class="user-btn"
                aria-label=move || format!("{}: {}", t("bar.user"), name())
                aria-haspopup="menu"
                aria-expanded=move || open.get().to_string()
                on:click=move |_| {
                    ui.notif_open.set(false);
                    ui.usb_open.set(false);
                    open.update(|o| *o = !*o);
                }
            >
                <span class="avatar" aria-hidden="true">{initial}</span>
                <span class="user-name">{name}</span>
                <Icon icon=icons::CHEVRON_DOWN size=14 />
            </button>
            <Show when=move || open.get()>
                <div class="popover menu" role="menu">
                    <div class="menu-user">
                        <span class="avatar lg" aria-hidden="true">{initial}</span>
                        <div class="menu-user-text">
                            <strong class="ellipsis">{name}</strong>
                            <span class="muted small">
                                {role}
                                {move || totp().then(|| view! { " · " <span class="tone-text ok">{t("sec.2fa_on")}</span> })}
                            </span>
                        </div>
                    </div>
                    <div class="menu-sep"></div>
                    <a class="menu-item" role="menuitem" href="/security" on:click=move |_| open.set(false)>
                        <Icon icon=icons::SHIELD_CHECK size=16 />{tr("nav.security")}
                    </a>
                    <a class="menu-item" role="menuitem" href="/system" on:click=move |_| open.set(false)>
                        <Icon icon=icons::SETTINGS size=16 />{tr("bar.prefs")}
                    </a>
                    <button class="menu-item" role="menuitem" on:click=move |_| { open.set(false); ui.palette_open.set(true); }>
                        <Icon icon=icons::COMMAND size=16 />{tr("bar.search")}<kbd>"Ctrl K"</kbd>
                    </button>
                    <div class="menu-sep"></div>
                    <button class="menu-item" role="menuitem" disabled=demo on:click=move |_| { open.set(false); crate::power::ask(rynas_api_types::power::SystemPowerAction::Restart); }>
                        <Icon icon=icons::ROTATE_CCW size=16 />{tr("sysp.restart")}
                    </button>
                    <button class="menu-item" role="menuitem" disabled=demo on:click=move |_| { open.set(false); crate::power::ask(rynas_api_types::power::SystemPowerAction::Shutdown); }>
                        <Icon icon=icons::POWER size=16 />{tr("sysp.shutdown")}
                    </button>
                    <div class="menu-sep"></div>
                    <button class="menu-item" role="menuitem" disabled=demo on:click=move |_| { open.set(false); a.sign_out(); }>
                        <Icon icon=icons::LOG_OUT size=16 />{tr("bar.logout")}
                    </button>
                </div>
            </Show>
        </div>
    }
}

#[component]
fn Sidebar() -> impl IntoView {
    let ui = use_ui();
    let location = use_location();
    let collapse_label = move || if ui.sidebar_collapsed.get() { t("nav.expand") } else { t("nav.collapse") };
    view! {
        <nav class="sidebar" aria-label=tr("nav.main")>
            <ul class="nav">
                {NAV
                    .iter()
                    .map(|item| {
                        let path = item.path;
                        let current = move || {
                            let p = location.pathname.get();
                            let on = if path == "/" { p == "/" } else { p.starts_with(path) };
                            on.then_some("page")
                        };
                        let key = item.key;
                        view! {
                            <li class:group=item.group hidden=move || !nav_shown(path)>
                                <a class="nav-item" href=path aria-current=current title=move || t(key)>
                                    <Icon icon=item.icon />
                                    <span class="nav-label">{move || t(key)}</span>
                                </a>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
            <div class="sidebar-about">
                <span class="about-text">{concat!("rynas ", env!("CARGO_PKG_VERSION"), " · Marco Ziliani")}</span>
                <a
                    class="icon-btn sm about-link"
                    href=env!("CARGO_PKG_REPOSITORY")
                    target="_blank"
                    rel="noopener noreferrer"
                    aria-label=tr("nav.project")
                    title=tr("nav.project")
                >
                    <Icon icon=icons::GITHUB size=16 />
                </a>
            </div>
            <button
                class="nav-item collapse-btn"
                aria-label=collapse_label
                title=collapse_label
                on:click=move |_| {
                    ui.sidebar_collapsed.update(|c| *c = !*c);
                    ui.relayout_soon();
                }
            >
                <Icon icon=icons::PANEL_LEFT />
                <span class="nav-label">{tr("nav.collapse")}</span>
            </button>
        </nav>
    }
}

#[component]
fn BottomBar() -> impl IntoView {
    let ui = use_ui();
    let location = use_location();
    let item = move |path: &'static str, key: &'static str, icon: &'static str| {
        let current = move || {
            let p = location.pathname.get();
            (if path == "/" { p == "/" } else { p.starts_with(path) }).then_some("page")
        };
        view! {
            <a class="bb-item" href=path aria-current=current>
                <Icon icon=icon size=20 />
                <span>{move || t(key)}</span>
            </a>
        }
    };
    view! {
        <nav class="bottombar" aria-label=tr("nav.main")>
            {item("/", "nav.dashboard", icons::LAYOUT_DASHBOARD)}
            {item("/storage", "nav.storage", icons::HARD_DRIVE)}
            {item("/hardware", "nav.hardware", icons::CPU)}
            <button class="bb-item" on:click=move |_| ui.drawer_open.update(|o| *o = !*o) aria-expanded=move || ui.drawer_open.get().to_string()>
                <Icon icon=icons::MENU size=20 />
                <span>{tr("nav.menu")}</span>
            </button>
        </nav>
    }
}
