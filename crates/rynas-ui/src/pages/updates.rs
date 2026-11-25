//! System › Updates: Debian packages (apt), automatic updates (unattended-upgrades) and the
//! kernel card (with the boot device of a hardware module's boot layer, where there is one).
//!
//! - The list is `GET /api/v1/updates` (rynasd's cached apt simulation: cheap to load).
//! - "Check now", "Install…", "Clean up…" and the changelogs are rynasd tasks, followed in a
//!   dialog with their log; the list is read again when they end.
//! - Packages a full upgrade would remove are listed in the confirmation and sent back by
//!   name (`confirm_removals`); autoremove is never offered without its list.
//! - Automatic updates are a configuration key (`updates.auto`): staged, then applied from
//!   the pending-changes bar.
//! - The kernel card reads `GET /api/v1/updates/kernel` (cached for hours by rynasd, a manual
//!   refresh reads the boot device again).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::updates::*;

use crate::api;
use crate::form::{form_error, lens, lens_bool, number_field, segmented, spinner, stale_notice, switch, Draft};
use crate::i18n::{self, t, tf, tn, tr};
use crate::icons::{self, Icon};
use crate::schedule::window::{hhmm, parse_hhmm, time_field};
use crate::schedule::window::Spread;
use crate::schedule::WeekWindowEditor;
use crate::tasks::TaskRun;
use crate::toast::saved_applied;
use crate::ui::{self, cell, confirm, row, status, table, text, Confirm, DlgKind, EmptyState, PageHeader, Remote, Tone, WidgetHead};

const URL: &str = "/api/v1/updates";
const AUTO: &str = "/api/v1/updates/auto";
const KERNEL: &str = "/api/v1/updates/kernel";
const BACKPORTS: &str = "/api/v1/updates/backports";
const CHANNEL: &str = "/api/v1/updates/kernel/channel";
const POLICY: &str = "/api/v1/updates/kernel/policy";
const MODES: &[(&str, &str)] = &[("off", "upd.mode_off"), ("security", "upd.mode_security"), ("all", "upd.mode_all")];

/// What the confirmation dialog asks.
#[derive(Clone)]
enum Ask {
    /// Install these packages (empty: all, a full upgrade) — removals listed.
    Install { packages: Vec<String>, removals: Vec<String> },
    Autoremove(Vec<String>),
    /// Install the newer kernel of the series (the version shown).
    Kernel { version: String, release: String },
    /// Switch to another kernel series (`older`: its metapackages replace the newer ones).
    Series { id: String, name: String, older: bool },
}

/// A task followed in the dialog: title and id.
type Run = Option<(String, String)>;

#[component]
pub fn Updates() -> impl IntoView {
    let st = Remote::<UpdatesStatus>::new(URL);
    let auto = Remote::<AutoUpdatesState>::with_config(AUTO);
    let kernel = Remote::<KernelStatus>::new(KERNEL);
    let backports = Remote::<BackportsState>::new(BACKPORTS);
    let channel = Remote::<KernelChannelState>::new(CHANNEL);
    let kpolicy = Remote::<KernelPolicyState>::with_config(POLICY);
    #[cfg(feature = "demo")]
    demo::fill(st, auto, kernel, channel, kpolicy);
    // a task ended: the list, the kernels and the boot state are read again
    let done = Callback::new(move |_| {
        st.reload();
        channel.reload();
        kernel.reload();
    });
    let selected = RwSignal::new(Vec::<String>::new());
    let run = RwSignal::new(None::<(String, String)>);
    let ask = RwSignal::new(None::<Ask>);
    let starting = RwSignal::new(false);
    let start_err = RwSignal::new(None::<(Tone, String)>);
    let busy = Signal::derive(move || starting.get() || st.data.with(|s| s.as_ref().is_some_and(|s| s.running_task.is_some())));

    let start = move |url: String, body: Option<String>, title: String| {
        starting.set(true);
        start_err.set(None);
        spawn_local(async move {
            let r = api::start_task(&url, body).await;
            starting.set(false);
            match r {
                Ok(id) => run.set(Some((title, id))),
                Err(e) => start_err.set(Some((Tone::Err, e.text()))),
            }
        });
    };
    let check = move |_| start(format!("{URL}/check"), None, t("task.updates.check").to_string());

    view! {
        <div class="page">
            <PageHeader icon=icons::DOWNLOAD title="upd.title" subtitle="upd.subtitle">
                <a class="btn ghost" href="/system"><Icon icon=icons::ARROW_LEFT size=16 /><span class="btn-label">{tr("nav.system")}</span></a>
                <button class="btn primary" on:click=check disabled=move || busy.get() || !st.ready()>
                    {spinner(starting)}<Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("upd.check")}</span>
                </button>
            </PageHeader>
            <ui::Notice msg=start_err />
            <div class="grid">
                {summary_card(st, selected, ask, busy)}
                {packages_card(st, selected, busy, start)}
                {auto_card(auto)}
                {backports_card(backports, run, busy)}
                {kernel_card(kernel)}
                {kernel_channel_card(channel, kpolicy, ask, busy)}
            </div>
            {move || ask.get().map(|a| ask_dialog(a, ask, start))}
            {move || run.get().map(|(title, id)| run_dialog(title, id, run, done))}
        </div>
    }
}

fn ms_text(ms: Option<u64>) -> String {
    ms.map(i18n::datetime).unwrap_or_else(|| t("upd.never").to_string())
}


fn pkg_list(names: &[String]) -> AnyView {
    view! { <ul class="upd-list">{names.iter().map(|n| view! { <li class="mono">{n.clone()}</li> }).collect_view()}</ul> }.into_any()
}

// ------------------------------------------------------------------ summary

fn summary_card(st: Remote<UpdatesStatus>, selected: RwSignal<Vec<String>>, ask: RwSignal<Option<Ask>>, busy: Signal<bool>) -> AnyView {
    let body = move || {
        if let Some(v) = st.gate(4) {
            return v;
        }
        let s = st.data.get().unwrap_or_default();
        if !s.apt {
            return view! {
                <EmptyState icon=icons::BOX title=Signal::derive(|| t("upd.no_apt").to_string()) text=Signal::derive(|| t("upd.no_apt_text").to_string()) />
            }
            .into_any();
        }
        let n = s.packages.len();
        let sec = s.security_count();
        let pill = if n == 0 {
            status(Tone::Ok, t("upd.uptodate").to_string())
        } else {
            status(if sec > 0 { Tone::Warn } else { Tone::Info }, tf("upd.available", &[("n", &n.to_string())]))
        };
        let sec_pill = (sec > 0).then(|| status(Tone::Err, tf("upd.security_n", &[("n", &sec.to_string())])));
        let reboot = s.reboot_required.then(|| {
            let pk = if s.reboot_packages.is_empty() { String::new() } else { tf("upd.reboot_pkgs", &[("p", &s.reboot_packages.join(", "))]) };
            ui::note_class(Tone::Warn, icons::ROTATE_CCW, "sysp-note", view! { <strong>{t("upd.reboot_required")}</strong>" "{pk}" "{crate::power::restart_button()} }.into_any())
        });
        let error = s.error.clone().map(|e| ui::note(Tone::Err, icons::CIRCLE_X, view! { {t("upd.sim_error")}<span class="muted small" lang="en">" "{e}</span> }.into_any()));
        let running = s.running_task.clone().map(|id| {
            ui::note(
                Tone::Info,
                icons::ACTIVITY,
                view! {
                    {t("upd.task_running")}" "
                    <button type="button" class="link-btn" on:click=move |_| crate::tasks::tasks().show(&id)>{tr("pending.show_log")}</button>
                }
                .into_any(),
            )
        });
        let removals = (!s.removals.is_empty()).then(|| {
            ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tf("upd.removals_note", &[("p", &s.removals.join(", "))])} }.into_any())
        });
        // the kernel metapackages the kernel policy holds are kept back by choice
        let other_held: Vec<String> = s.held.iter().filter(|h| !s.kernel_held.contains(h)).cloned().collect();
        let held = (!other_held.is_empty()).then(|| ui::kv("upd.held", text(other_held.join(", "))));
        let kernel_held = (!s.kernel_held.is_empty()).then(|| {
            ui::kv("upd.kernel_held", view! { <span class="mono">{s.kernel_held.join(", ")}</span><span class="muted small">" "{t("upd.kernel_held_help")}</span> }.into_any())
        });
        let sec_names: Vec<String> = s.packages.iter().filter(|p| p.security).map(|p| p.name.clone()).collect();
        let all_removals = s.removals.clone();
        let auto_rm = s.autoremovable.clone();
        let auto_n = auto_rm.len();
        view! {
            <div class="chips upd-head">{pill}{sec_pill}</div>
            {running}
            {error}
            {reboot}
            {removals}
            <dl class="kvs">
                {ui::kv("upd.lists_updated", text(ms_text(s.lists_updated_ms)))}
                {ui::kv("upd.read_at", text(i18n::datetime(s.read_at_ms)))}
                {held}
                {kernel_held}
            </dl>
            <div class="card-actions">
                <button class="btn primary" disabled=move || busy.get() || n == 0
                    on:click=move |_| ask.set(Some(Ask::Install { packages: Vec::new(), removals: all_removals.clone() }))>
                    <Icon icon=icons::DOWNLOAD size=16 />{tr("upd.install_all")}
                </button>
                {(sec > 0).then(|| {
                    let names = sec_names.clone();
                    view! {
                        <button class="btn ghost" disabled=move || busy.get()
                            on:click=move |_| ask.set(Some(Ask::Install { packages: names.clone(), removals: Vec::new() }))>
                            <Icon icon=icons::SHIELD_CHECK size=16 />{tr("upd.install_security")}
                        </button>
                    }
                })}
                <button class="btn ghost" disabled=move || busy.get() || selected.with(Vec::is_empty)
                    on:click=move |_| ask.set(Some(Ask::Install { packages: selected.get_untracked(), removals: Vec::new() }))>
                    {move || tf("upd.install_selected", &[("n", &selected.with(Vec::len).to_string())])}
                </button>
                {(auto_n > 0).then(|| {
                    let names = auto_rm.clone();
                    view! {
                        <button class="btn ghost" disabled=move || busy.get() on:click=move |_| ask.set(Some(Ask::Autoremove(names.clone())))>
                            <Icon icon=icons::TRASH_2 size=16 />{tf("upd.autoremove_n", &[("n", &auto_n.to_string())])}
                        </button>
                    }
                })}
            </div>
        }
        .into_any()
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::SHIELD_CHECK title="upd.status" />
            {body}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ packages

fn packages_card(
    st: Remote<UpdatesStatus>,
    selected: RwSignal<Vec<String>>,
    busy: Signal<bool>,
    start: impl Fn(String, Option<String>, String) + Copy + Send + Sync + 'static,
) -> AnyView {
    // selections of packages that are gone (installed meanwhile) are dropped
    Effect::new(move |_| {
        st.data.with(|s| {
            if let Some(s) = s {
                let keep: Vec<String> = selected.get_untracked().into_iter().filter(|n| s.packages.iter().any(|p| &p.name == n)).collect();
                if keep.len() != selected.with_untracked(Vec::len) {
                    selected.set(keep);
                }
            }
        })
    });
    let body = move || {
        if let Some(v) = st.gate(6) {
            return v;
        }
        let s = st.data.get().unwrap_or_default();
        if s.packages.is_empty() {
            return view! { <p class="muted small table-none">{tr("upd.none")}</p> }.into_any();
        }
        let rows = s
            .packages
            .iter()
            .map(|p| {
                let name = p.name.clone();
                let (n1, n2, n3) = (name.clone(), name.clone(), name.clone());
                let checked = Signal::derive(move || selected.with(|v| v.contains(&n1)));
                let toggle = move |on: bool| {
                    selected.update(|v| {
                        v.retain(|x| x != &n2);
                        if on {
                            v.push(n2.clone());
                        }
                    })
                };
                let kind = if p.security {
                    Some(status(Tone::Err, t("upd.security").to_string()))
                } else if p.current.is_none() {
                    Some(status(Tone::Info, t("upd.new_pkg").to_string()))
                } else {
                    None
                };
                let selectable = p.current.is_some();
                row(vec![
                    cell(
                        "upd.col_select",
                        "",
                        view! {
                            <input type="checkbox" class="check-box" aria-label=tf("upd.select_named", &[("name", &name)])
                                prop:checked=checked disabled=!selectable
                                on:change=move |ev| toggle(event_target_checked(&ev)) />
                        }
                        .into_any(),
                    ),
                    cell("upd.col_pkg", "", view! { <div class="cell-main"><span class="mono">{name.clone()}</span>{kind}</div> }.into_any()),
                    cell("upd.col_current", "mono small", text(p.current.clone().unwrap_or_else(|| ui::DASH.into()))),
                    cell("upd.col_new", "mono small", text(p.candidate.clone())),
                    cell("upd.col_origin", "muted small", text(p.origin.clone())),
                    cell(
                        "",
                        "num",
                        view! {
                            <div class="row-actions">
                                <button class="icon-btn sm" disabled=move || busy.get() aria-label=tf("upd.changelog_of", &[("name", &n3)]) title=tf("upd.changelog_of", &[("name", &n3)])
                                    on:click={
                                        let n = n3.clone();
                                        move |_| start(format!("{URL}/packages/{}/changelog", api::encode(&n)), None, tf("upd.changelog_of", &[("name", &n)]))
                                    }>
                                    <Icon icon=icons::FILE_TEXT size=16 />
                                </button>
                            </div>
                        }
                        .into_any(),
                    ),
                ])
            })
            .collect();
        table("upd-table", &["upd.col_select", "upd.col_pkg", "upd.col_current", "upd.col_new", "upd.col_origin", ""], rows)
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::BOX title="upd.packages" />
            {body}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ dialogs

fn ask_dialog(a: Ask, ask: RwSignal<Option<Ask>>, start: impl Fn(String, Option<String>, String) + Copy + Send + Sync + 'static) -> AnyView {
    let close = Callback::new(move |_| ask.set(None));
    let (title, textv, action, danger, extra, url, body, task) = match a {
        Ask::Install { packages, removals } => {
            let all = packages.is_empty();
            let title = if all { t("upd.confirm_all").to_string() } else { tf("upd.confirm_some", &[("n", &packages.len().to_string())]) };
            let textv = if removals.is_empty() { t("upd.confirm_text").to_string() } else { t("upd.confirm_text_rm").to_string() };
            let extra = view! {
                {(!packages.is_empty()).then(|| pkg_list(&packages))}
                {(!removals.is_empty()).then(|| view! {
                    {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <strong>{t("upd.will_remove")}</strong> }.into_any())}
                    {pkg_list(&removals)}
                })}
            }
            .into_any();
            let body = serde_json::to_string(&InstallRequest { packages, confirm_removals: removals.clone() }).unwrap_or_default();
            (title, textv, "upd.install", !removals.is_empty(), extra, format!("{URL}/install"), body, t("task.updates.install").to_string())
        }
        Ask::Autoremove(packages) => {
            let extra = view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upd.autoremove_warn")} }.into_any())}
                {pkg_list(&packages)}
            }
            .into_any();
            let body = serde_json::to_string(&AutoremoveRequest { packages }).unwrap_or_default();
            (t("upd.autoremove_title").to_string(), t("upd.autoremove_text").to_string(), "upd.remove", true, extra, format!("{URL}/autoremove"), body, t("task.updates.autoremove").to_string())
        }
        Ask::Kernel { version, release } => {
            let body = serde_json::to_string(&KernelInstallRequest { version }).unwrap_or_default();
            (
                tf("kch.confirm_title", &[("k", &release)]),
                t("kch.confirm_text").to_string(),
                "kch.install",
                false,
                ().into_any(),
                "/api/v1/updates/kernel/install".to_string(),
                body,
                t("task.updates.kernel").to_string(),
            )
        }
        Ask::Series { id, name, older } => {
            let body = serde_json::to_string(&KernelSeriesRequest { series: id }).unwrap_or_default();
            (
                tf("kch.series_title", &[("series", &name)]),
                t(if older { "kch.series_text_back" } else { "kch.series_text" }).to_string(),
                "kch.switch_action",
                older,
                ().into_any(),
                "/api/v1/updates/kernel/series".to_string(),
                body,
                t("task.updates.kernel-series").to_string(),
            )
        }
    };
    confirm(Confirm {
        title,
        text: textv,
        action,
        danger,
        extra: Some(extra),
        typed: Signal::stored(None),
        busy: RwSignal::new(false),
        error: RwSignal::new(None),
        on_confirm: Callback::new(move |_| {
            ask.set(None);
            start(url.clone(), Some(body.clone()), task.clone());
        }),
        on_cancel: close,
    })
}

fn run_dialog(title: String, id: String, run: RwSignal<Run>, on_done: Callback<()>) -> AnyView {
    let close = Callback::new(move |_| run.set(None));
    let body = view! { <TaskRun id=id on_done=Callback::new(move |_| on_done.run(())) /> }.into_any();
    ui::dialog(DlgKind::Wide, Signal::stored(title), close, body, Some(ui::close_foot(close)))
}

// ------------------------------------------------------------------ Debian backports

/// The "Debian backports" switch (`/api/v1/updates/backports`): rynas's own sources file,
/// or "configured by the system" read-only; hidden where rynas cannot switch it.
fn backports_card(bp: Remote<BackportsState>, run: RwSignal<Run>, busy: Signal<bool>) -> AnyView {
    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // read again when the task dialog closes
    Effect::new(move |prev: Option<bool>| {
        let open = run.with(Option::is_some);
        if prev == Some(true) && !open {
            bp.reload();
        }
        open
    });
    let set = Callback::new(move |on: bool| {
        saving.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::put::<_, rynas_api_types::TaskStarted>(BACKPORTS, &BackportsRequest { enabled: on }).await;
            let _ = saving.try_set(false);
            match r {
                Ok(t) => run.set(Some((t_backports(), t.task_id))),
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                    bp.reload();
                }
            }
        });
    });
    let body = move || {
        if let Some(v) = bp.gate(2) {
            return v;
        }
        let s = bp.data.get().unwrap_or_default();
        let by_system = s.by_system;
        let value = Signal::derive(move || bp.data.with(|s| s.as_ref().is_some_and(|s| s.enabled)));
        view! {
            <p class="muted small">{t("upd.bp_help")}</p>
            {if by_system {
                view! { <dl class="kvs"><div class="kv"><dt>{tr("upd.bp")}</dt><dd>{status(Tone::Ok, t("upd.bp_system").to_string())}</dd></div></dl> }.into_any()
            } else {
                view! {
                    <fieldset class="sf-group" disabled=move || busy.get() || saving.get()>
                        {switch("upd.bp", None, value, set)}
                    </fieldset>
                }
                .into_any()
            }}
            {form_error(error)}
        }
        .into_any()
    };
    view! {
        {move || bp.data.with(|s| s.as_ref().is_none_or(|s| s.available)).then(|| view! {
            <section class="card widget w-6">
                <WidgetHead icon=icons::BOX title="upd.bp_title" />
                {body}
            </section>
        })}
    }
    .into_any()
}

fn t_backports() -> String {
    t("task.updates.backports").to_string()
}

// ------------------------------------------------------------------ automatic updates

fn mode_key(m: AutoMode) -> String {
    match m {
        AutoMode::Off => "off",
        AutoMode::Security => "security",
        AutoMode::All => "all",
    }
    .into()
}

fn auto_card(auto: Remote<AutoUpdatesState>) -> AnyView {
    let body = move || {
        if let Some(v) = auto.gate(6) {
            return v;
        }
        // built once: the draft follows the server (form::Draft)
        let s = auto.data.get_untracked().unwrap_or_default();
        auto_form(s, auto)
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::CALENDAR_CLOCK title="upd.auto" />
            {body}
        </section>
    }
    .into_any()
}

fn auto_form(s: AutoUpdatesState, auto: Remote<AutoUpdatesState>) -> AnyView {
    let init = s.pending.clone().unwrap_or_else(|| s.applied.clone());
    let draft = Draft::new(init, move || auto.data.with(|s| s.as_ref().map(|s| s.pending.clone().unwrap_or_else(|| s.applied.clone()))));
    let d = draft.d;
    let dirty = draft.dirty();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (mode, set_mode) = lens(
        d,
        |a| mode_key(a.mode),
        |a, v| {
            a.mode = match v.as_str() {
                "security" => AutoMode::Security,
                "all" => AutoMode::All,
                _ => AutoMode::Off,
            }
        },
    );
    // the window: the shared editor (the same texts as Docker's automatic updates)
    let win = d.with_untracked(|a| WeekWindowEditor::daily(a.hour, a.minute, a.window_min));
    let win_changed = Callback::new(move |_| {
        d.update(|a| match win.value() {
            Some((h, m, len)) => (a.hour, a.minute, a.window_min) = (h, m, len),
            // not a valid window: the save refuses it
            None => a.window_min = 0,
        })
    });
    let (reboot, set_reboot) = lens_bool(d, |a| a.reboot, |a, v| a.reboot = v);
    let (rtime, set_rtime) = lens(d, |a| hhmm(a.reboot_hour, a.reboot_minute), |a, v| if let Some((h, m)) = parse_hhmm(&v) { (a.reboot_hour, a.reboot_minute) = (h, m) });
    let off = move || d.with(|a| a.mode == AutoMode::Off);
    let invalid = move || win.value().is_none();
    let pending = Signal::derive(move || auto.data.with(|s| s.as_ref().is_some_and(|s| s.pending.is_some())));
    let save = move |_| {
        if invalid() {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = d.get_untracked();
        spawn_local(async move {
            let r = api::put::<_, AutoUpdatesState>(AUTO, &body).await;
            match r {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::UPDATES], error).await {
                        saved_applied();
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    };
    let reset = Callback::new(move |_| {
        draft.reset();
        d.with_untracked(|a| win.set(a.hour, a.minute, a.window_min));
        error.set(None);
    });
    view! {
        {(!s.unattended_installed).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upd.no_unattended")} }.into_any()))}
        <form class="form" on:submit=move |ev| ev.prevent_default()>
            {segmented("upd.mode", MODES, mode, set_mode)}
            <p class="field-help">{tr("upd.mode_help")}</p>
            <div hidden=off>
                {win.view(Spread::Whole, win_changed)}
                {switch("upd.reboot", Some("upd.reboot_help"), reboot, set_reboot)}
                <div hidden=move || !reboot.get()>{time_field("upd.reboot_time", Some("upd.reboot_time_help"), rtime, set_rtime)}</div>
            </div>
            {move || pending.get().then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("upd.pending_note")}</p> })}
            {stale_notice(draft.stale, reset)}
            {form_error(error)}
            <div class="card-actions">
                <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
                <button type="button" class="btn primary" disabled=move || busy.get() || !dirty.get() on:click=save>{spinner(busy)}{tr("save")}</button>
            </div>
        </form>
    }
    .into_any()
}

// ------------------------------------------------------------------ kernel / boot device

fn trial_status(t_: KernelTrial) -> AnyView {
    match t_ {
        KernelTrial::None => status(Tone::Ok, t("upd.trial_none").to_string()),
        KernelTrial::Armed => status(Tone::Info, t("upd.trial_armed").to_string()),
        KernelTrial::Trying => status(Tone::Warn, t("upd.trial_trying").to_string()),
    }
}

fn boot_kernel(k: &Option<BootKernel>) -> String {
    k.as_ref().and_then(|k| k.release.clone()).unwrap_or_else(|| ui::DASH.into())
}

fn kernel_card(kernel: Remote<KernelStatus>) -> AnyView {
    let refreshing = RwSignal::new(false);
    let refresh = move |_| {
        refreshing.set(true);
        spawn_local(async move {
            let r = api::get::<KernelStatus>(&format!("{KERNEL}?refresh=1")).await;
            refreshing.set(false);
            match r {
                Ok(k) => kernel.data.set(Some(k)),
                Err(e) => crate::toast::toast(Tone::Err, e.text()),
            }
        });
    };
    let body = move || {
        if let Some(v) = kernel.gate(6) {
            return v;
        }
        let k = kernel.data.get().unwrap_or_default();
        let newer = k.installed.first().filter(|n| **n != k.running).cloned();
        let dom = k.boot.clone();
        let mismatch = dom.as_ref().and_then(|d| d.kernel.as_ref()).and_then(|x| x.release.clone()).filter(|r| *r != k.running);
        view! {
            {newer.map(|n| ui::note(Tone::Warn, icons::ROTATE_CCW, view! { {tf("upd.kernel_newer", &[("k", &n)])} }.into_any()))}
            {mismatch.map(|r| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tf("upd.kernel_mismatch", &[("k", &r)])} }.into_any()))}
            {dom.as_ref().and_then(|d| d.outdated.clone()).map(|o| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upd.boot_outdated")}<span class="muted small" lang="en">" "{o}</span> }.into_any()))}
            {k.error.clone().map(|e| ui::note(Tone::Err, icons::CIRCLE_X, view! { {t("upd.boot_error")}<span class="muted small" lang="en">" "{e}</span> }.into_any()))}
            <dl class="kvs">
                {ui::kv("upd.running_kernel", view! { <span class="mono">{k.running.clone()}</span> }.into_any())}
                {ui::kv("upd.installed_kernels", view! { <span class="mono">{if k.installed.is_empty() { ui::DASH.to_string() } else { k.installed.join(", ") }}</span> }.into_any())}
                {dom.map(|d| view! {
                    {ui::kv("upd.boot_default", view! { <span class="mono">{d.default.clone().unwrap_or_else(|| ui::DASH.into())}</span> }.into_any())}
                    {ui::kv("upd.boot_fallback", view! { <span class="mono">{if d.fallback.is_empty() { ui::DASH.to_string() } else { d.fallback.join(" → ") }}</span> }.into_any())}
                    {ui::kv("upd.boot_entries", view! { <span class="mono">{d.entries.join(", ")}</span> }.into_any())}
                    {ui::kv("upd.boot_running", view! { <span class="mono">{d.running.clone().unwrap_or_else(|| ui::DASH.into())}</span> }.into_any())}
                    {ui::kv("upd.boot_kernel", view! { <span class="mono">{boot_kernel(&d.kernel)}</span> }.into_any())}
                    {ui::kv("upd.boot_kernel_old", view! { <span class="mono">{boot_kernel(&d.kernel_old)}</span> }.into_any())}
                    {ui::kv("upd.trial", trial_status(d.trial))}
                    {ui::kv("upd.boot_free", text(d.free_bytes.map(i18n::bytes).unwrap_or_else(|| ui::DASH.into())))}
                })}
                {ui::kv("upd.read_at", text(ms_text(k.read_at_ms)))}
            </dl>
        }
        .into_any()
    };
    view! {
        <section class="card widget w-6" id="kernel">
            <WidgetHead icon=icons::CPU title="upd.kernel">
                <button class="icon-btn sm" aria-label=tr("upd.refresh_kernel") title=tr("upd.refresh_kernel") disabled=move || refreshing.get() on:click=refresh>
                    <Icon icon=icons::REFRESH_CW size=16 />
                </button>
            </WidgetHead>
            {body}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ kernel updates

const KMODES: &[(&str, &str)] = &[
    ("system", "kch.mode_system"),
    ("manual", "kch.mode_manual"),
    ("notify", "kch.mode_notify"),
    ("auto", "kch.mode_auto"),
];

fn kmode_key(m: KernelMode) -> String {
    match m {
        KernelMode::System => "system",
        KernelMode::Manual => "manual",
        KernelMode::Notify => "notify",
        KernelMode::Auto => "auto",
    }
    .into()
}

/// A series' name: the known ids translated, with the upstream branch.
fn series_name(s: &KernelSeriesState) -> String {
    let name = match s.id.as_str() {
        "longterm" => t("kch.s.longterm").to_string(),
        "stable" => t("kch.s.stable").to_string(),
        "default" => t("kch.s.default").to_string(),
        other => other.to_string(),
    };
    match &s.branch {
        Some(b) => format!("{name} ({b})"),
        None => name,
    }
}

fn cve_list(ids: &[String], titles: &std::collections::BTreeMap<String, String>) -> AnyView {
    view! {
        <ul class="upd-list">
            {ids.iter().map(|c| {
                let url = format!("https://www.cve.org/CVERecord?id={}", api::encode(c));
                let title = titles.get(c).cloned();
                view! {
                    <li>
                        <a class="mono" href=url target="_blank" rel="noopener noreferrer">{c.clone()}</a>
                        {title.map(|t_| view! { <span class="muted small" lang="en">" "{t_}</span> })}
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

/// One newer kernel: date, change log, the publisher's changes, CVEs.
fn kernel_offer(k: &KernelOffer, titles: &std::collections::BTreeMap<String, String>, first: bool) -> AnyView {
    let date = k.published_ms.map(|ms| {
        if k.published_known { tf("kch.released", &[("date", &i18n::date(ms))]) } else { tf("kch.first_seen", &[("date", &i18n::date(ms))]) }
    });
    let chip = if k.available {
        status(Tone::Info, t("kch.installable").to_string())
    } else {
        status(Tone::Unknown, t("kch.not_available").to_string())
    };
    let cves = match &k.cves {
        Some(c) if c.is_empty() => Some(text(t("kch.no_cves").to_string())),
        Some(c) => Some(ui::fold_with(text(tn("kch.cves", c.len() as u64, &[])), false, cve_list(c, titles))),
        None => None,
    };
    let changes = if k.changes.is_empty() {
        text(t("kch.upstream_only").to_string())
    } else {
        view! { <ul class="upd-list">{k.changes.iter().map(|c| view! { <li lang="en">{c.clone()}</li> }).collect_view()}</ul> }.into_any()
    };
    let has_feed = k.changelog.is_some() || !k.changes.is_empty() || k.cves.is_some() || k.published_known;
    let title = view! { <span class="mono">{k.release.clone()}</span>" "{chip}{date.map(|d| view! { <span class="muted small">" "{d}</span> })} }.into_any();
    let body = view! {
        <dl class="kvs">
            {k.changelog.clone().map(|u| ui::kv("kch.changelog", view! { <a href=u target="_blank" rel="noopener noreferrer">{k.upstream.clone().unwrap_or_else(|| k.release.clone())}</a> }.into_any()))}
            {has_feed.then(|| ui::kv("kch.changes", changes))}
            {cves.map(|c| ui::kv("kch.cves_title", c))}
        </dl>
    }
    .into_any();
    ui::fold_with(title, first, body)
}

fn kernel_channel_card(channel: Remote<KernelChannelState>, policy: Remote<KernelPolicyState>, ask: RwSignal<Option<Ask>>, busy: Signal<bool>) -> AnyView {
    let refreshing = RwSignal::new(false);
    let refresh = move |_| {
        refreshing.set(true);
        spawn_local(async move {
            let r = api::get::<KernelChannelState>(&format!("{CHANNEL}?refresh=1")).await;
            refreshing.set(false);
            match r {
                Ok(k) => channel.data.set(Some(k)),
                Err(e) => crate::toast::toast(Tone::Err, e.text()),
            }
        });
    };
    let body = move || {
        if let Some(v) = channel.gate(6) {
            return v;
        }
        let s = channel.data.get().unwrap_or_default();
        let behind = s.newer.len();
        let cand = s.candidate().cloned();
        let cur = s.series.iter().find(|x| x.current).cloned();
        let others: Vec<KernelSeriesState> = s.series.iter().filter(|x| !x.current).cloned().collect();
        let cur_idx = s.series.iter().position(|x| x.current).unwrap_or(0);
        let total = s.cves();
        let head = if behind == 0 {
            status(Tone::Ok, t("kch.uptodate").to_string())
        } else {
            status(Tone::Info, tn("kch.behind", behind as u64, &[]))
        };
        let due = s.auto_due_ms.map(|ms| ui::note(Tone::Info, icons::CALENDAR_CLOCK, view! { {tf("kch.auto_due", &[("date", &i18n::datetime(ms))])} }.into_any()));
        let feed_err = s.feed_error.clone().map(|e| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("kch.feed_error")}<span class="muted small" lang="en">" "{e}</span> }.into_any()));
        let titles = s.cve_titles.clone();
        let offers = s.newer.iter().enumerate().map(|(i, k)| kernel_offer(k, &titles, i == 0)).collect_view();
        let install = cand.clone().map(|c| {
            let (v, r) = (c.version.clone(), c.release.clone());
            view! {
                <button class="btn primary" disabled=move || busy.get()
                    on:click=move |_| ask.set(Some(Ask::Kernel { version: v.clone(), release: r.clone() }))>
                    <Icon icon=icons::DOWNLOAD size=16 />{tr("kch.install")}
                </button>
            }
        });
        let switches = others
            .into_iter()
            .map(|o| {
                let idx = s.series.iter().position(|x| x.id == o.id).unwrap_or(0);
                let older = idx < cur_idx;
                let name = series_name(&o);
                let id = o.id.clone();
                let label = tf("kch.switch", &[("series", &name)]);
                view! {
                    <button class="btn ghost" disabled=move || busy.get()
                        on:click=move |_| ask.set(Some(Ask::Series { id: id.clone(), name: name.clone(), older }))>
                        {label}
                    </button>
                }
            })
            .collect_view();
        view! {
            <div class="chips upd-head">{head}</div>
            {feed_err}
            {due}
            <dl class="kvs">
                {ui::kv("kch.installed", view! { <span class="mono">{s.installed_release.clone().or(s.installed_version.clone()).unwrap_or_else(|| ui::DASH.into())}</span> }.into_any())}
                {cur.filter(|_| s.series.len() > 1).map(|c| ui::kv("kch.series", text(series_name(&c))))}
                {(!s.held.is_empty()).then(|| ui::kv("kch.held", view! { <span class="mono">{s.held.join(", ")}</span> }.into_any()))}
                {s.last_install_ms.map(|ms| ui::kv("kch.last_install", text(i18n::datetime(ms))))}
                {(behind > 0).then(|| ui::kv("kch.cves_total", text(match &total {
                    Some(c) => tn("kch.cves", c.len() as u64, &[]),
                    None => t("kch.cves_unknown").to_string(),
                })))}
            </dl>
            {(behind > 0).then(|| view! { <div class="kch-offers">{offers}</div> })}
            <div class="card-actions">{install}{switches}</div>
        }
        .into_any()
    };
    // built once: the draft follows the server (form::Draft)
    let policy_part = move || {
        if let Some(v) = policy.gate(3) {
            return v;
        }
        let s = policy.data.get_untracked().unwrap_or_default();
        kernel_policy_form(s, policy, channel)
    };
    view! {
        {move || channel.data.with(|s| s.as_ref().is_none_or(|s| s.supported)).then(|| view! {
            <section class="card widget w-12">
                <WidgetHead icon=icons::CPU title="kch.title">
                    <button class="icon-btn sm" aria-label=tr("kch.refresh") title=tr("kch.refresh") disabled=move || refreshing.get() on:click=refresh>
                        <Icon icon=icons::REFRESH_CW size=16 />
                    </button>
                </WidgetHead>
                {body}
                {policy_part}
            </section>
        })}
    }
    .into_any()
}

fn kernel_policy_form(s: KernelPolicyState, policy: Remote<KernelPolicyState>, channel: Remote<KernelChannelState>) -> AnyView {
    let init = s.pending.unwrap_or(s.applied);
    let draft = Draft::new(init, move || policy.data.with(|s| s.as_ref().map(|s| s.pending.unwrap_or(s.applied))));
    let d = draft.d;
    let dirty = draft.dirty();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (mode, set_mode) = lens(
        d,
        |p| kmode_key(p.mode),
        |p, v| {
            p.mode = match v.as_str() {
                "manual" => KernelMode::Manual,
                "notify" => KernelMode::Notify,
                "auto" => KernelMode::Auto,
                _ => KernelMode::System,
            }
        },
    );
    let (interval, set_interval) = lens(d, |p| p.interval_days.to_string(), |p, v| p.interval_days = v.trim().parse().unwrap_or(0));
    let (age, set_age) = lens(d, |p| p.min_age_days.to_string(), |p, v| p.min_age_days = v.trim().parse().unwrap_or(u16::MAX));
    let (reboot, set_reboot) = lens_bool(d, |p| p.reboot, |p, v| p.reboot = v);
    let m = move || d.with(|p| p.mode);
    let help = move || {
        t(match m() {
            KernelMode::System => "kch.help_system",
            KernelMode::Manual => "kch.help_manual",
            KernelMode::Notify => "kch.help_notify",
            KernelMode::Auto => "kch.help_auto",
        })
    };
    let invalid = move || d.with(|p| !(1..=365).contains(&p.interval_days) || p.min_age_days > 365);
    let pending = Signal::derive(move || policy.data.with(|s| s.as_ref().is_some_and(|s| s.pending.is_some())));
    let save = move |_| {
        if invalid() {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = d.get_untracked();
        spawn_local(async move {
            match api::put::<_, KernelPolicyState>(POLICY, &body).await {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::UPDATES_KERNEL], error).await {
                        saved_applied();
                        policy.reload();
                        channel.reload();
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    };
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
    });
    view! {
        <form class="form kch-form" on:submit=move |ev| ev.prevent_default()>
            {segmented("kch.mode", KMODES, mode, set_mode)}
            <p class="field-help">{help}</p>
            <div hidden=move || !matches!(m(), KernelMode::Notify | KernelMode::Auto)>
                {move || number_field("kch.interval", Some(if m() == KernelMode::Auto { "kch.interval_help_auto" } else { "kch.interval_help_notify" }), interval, set_interval, 1, 365)}
            </div>
            <div hidden=move || m() != KernelMode::Auto>
                {number_field("kch.min_age", Some("kch.min_age_help"), age, set_age, 0, 365)}
                {switch("kch.reboot", Some("kch.reboot_help"), reboot, set_reboot)}
                <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("kch.window_note")}</p>
            </div>
            {move || pending.get().then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{t("upd.pending_note")}</p> })}
            {stale_notice(draft.stale, reset)}
            {form_error(error)}
            <div class="card-actions">
                <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
                <button type="button" class="btn primary" disabled=move || busy.get() || !dirty.get() on:click=save>{spinner(busy)}{tr("save")}</button>
            </div>
        </form>
    }
    .into_any()
}

// ------------------------------------------------------------------ demo data

#[cfg(feature = "demo")]
mod demo {
    use super::*;
    use crate::live::{Conn, use_live};

    fn pkg(name: &str, cur: Option<&str>, new: &str, origin: &str, security: bool) -> UpdatePackage {
        UpdatePackage { name: name.into(), arch: "amd64".into(), current: cur.map(Into::into), candidate: new.into(), origin: origin.into(), security }
    }

    pub fn fill(st: Remote<UpdatesStatus>, auto: Remote<AutoUpdatesState>, kernel: Remote<KernelStatus>, channel: Remote<KernelChannelState>, kpolicy: Remote<KernelPolicyState>) {
        let live = use_live();
        Effect::new(move |_| {
            if live.conn.get() != Conn::Demo {
                return;
            }
            let now = js_sys::Date::now() as u64;
            let sec = "Debian-Security:13/stable-security";
            let st_ = "Debian:13.2/stable";
            st.data.set(Some(UpdatesStatus {
                apt: true,
                read_at_ms: now - 60_000,
                lists_updated_ms: Some(now - 3 * 3_600_000),
                packages: vec![
                    pkg("base-files", Some("13.8"), "13.8+deb13u2", st_, false),
                    pkg("libssl3t64", Some("3.5.1-1+deb13u1"), "3.5.4-1~deb13u1", sec, true),
                    pkg("linux-image-6.12.57+deb13-amd64", None, "6.12.57-1", sec, true),
                    pkg("openssh-server", Some("1:10.0p1-7"), "1:10.0p1-7+deb13u1", st_, false),
                    pkg("openssl", Some("3.5.1-1+deb13u1"), "3.5.4-1~deb13u1", sec, true),
                    pkg("samba", Some("2:4.22.3+dfsg-4"), "2:4.22.6+dfsg-0+deb13u1", st_, false),
                    pkg("tzdata", Some("2025b-4"), "2025b-4+deb13u1", "Debian:13.2/stable-updates", false),
                ],
                removals: vec!["libfuse2t64".into()],
                held: vec!["synodeb".into()],
                autoremovable: vec!["linux-image-6.12.38+deb13-amd64".into(), "synology-ds923".into()],
                reboot_required: true,
                reboot_packages: vec!["linux-image-amd64".into()],
                unattended_installed: true,
                ..Default::default()
            }));
            auto.data.set(Some(AutoUpdatesState { applied: AutoUpdates { mode: AutoMode::Security, ..Default::default() }, pending: None, unattended_installed: true }));
            kernel.data.set(Some(KernelStatus {
                running: "6.12.48-synodeb".into(),
                installed: vec!["6.12.48-synodeb".into(), "6.12.41-synodeb".into()],
                boot_layer: true,
                boot: Some(BootImage {
                    entries: vec!["DEBIAN".into(), "DEBIAN_OLD".into(), "SYNOLOGY_1".into(), "SYNOLOGY_2".into()],
                    default: Some("DEBIAN".into()),
                    fallback: vec!["DEBIAN_OLD".into(), "SYNOLOGY_1".into()],
                    trial: KernelTrial::None,
                    running: Some("DEBIAN".into()),
                    outdated: None,
                    kernel: Some(BootKernel { release: Some("6.12.48-synodeb".into()), version: None }),
                    kernel_old: Some(BootKernel { release: Some("6.12.41-synodeb".into()), version: None }),
                    free_bytes: Some(48_120 * 1024),
                }),
                read_at_ms: Some(now - 600_000),
                error: None,
            }));
            let day = 86_400_000;
            let policy = KernelPolicy { mode: KernelMode::Manual, ..Default::default() };
            kpolicy.data.set(Some(KernelPolicyState { applied: policy, pending: None }));
            let cves = |ids: &[&str]| Some(ids.iter().map(|s| s.to_string()).collect::<Vec<_>>());
            channel.data.set(Some(KernelChannelState {
                supported: true,
                module: true,
                series: vec![
                    KernelSeriesState { id: "longterm".into(), branch: Some("6.18".into()), image: "linux-image-synodeb-longterm".into(), installed: false, current: false },
                    KernelSeriesState { id: "stable".into(), branch: Some("7.3".into()), image: "linux-image-synodeb-stable".into(), installed: true, current: true },
                ],
                metapackages: vec!["linux-image-synodeb-stable".into()],
                held: vec!["linux-image-synodeb-stable".into()],
                installed_version: Some("7.3.4-11".into()),
                installed_release: Some("7.3.4-11-synodeb".into()),
                newer: vec![
                    KernelOffer {
                        release: "7.3.5-13-synodeb".into(),
                        version: "7.3.5-13".into(),
                        available: true,
                        published_ms: Some(now - day),
                        published_known: true,
                        upstream: Some("7.3.5".into()),
                        changelog: Some("https://cdn.kernel.org/pub/linux/kernel/v7.x/ChangeLog-7.3.5".into()),
                        changes: vec!["kernel: built with the upstream 7.3.5".into()],
                        cves: cves(&["CVE-2026-43001", "CVE-2026-43017"]),
                    },
                    KernelOffer {
                        release: "7.3.4-12-synodeb".into(),
                        version: "7.3.4-12".into(),
                        available: false,
                        published_ms: Some(now - 10 * day),
                        published_known: true,
                        upstream: Some("7.3.4".into()),
                        changelog: Some("https://cdn.kernel.org/pub/linux/kernel/v7.x/ChangeLog-7.3.4".into()),
                        changes: Vec::new(),
                        cves: cves(&[]),
                    },
                ],
                cve_titles: [("CVE-2026-43001".to_string(), "net: fix a use-after-free in the socket layer".to_string())].into_iter().collect(),
                feed_read_ms: Some(now - 3_600_000),
                policy,
                ..Default::default()
            }));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_hhmm("03:05"), Some((3, 5)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(hhmm(4, 0), "04:00");
        assert_eq!(mode_key(AutoMode::Security), "security");
    }
}
