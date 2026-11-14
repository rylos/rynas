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
use crate::form::{fid, form_error, lens, lens_bool, number_field, segmented, spinner, stale_notice, switch, Draft};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::TaskRun;
use crate::toast::saved_applied;
use crate::ui::{self, cell, confirm, row, status, table, text, Confirm, DlgKind, EmptyState, PageHeader, Remote, Tone, WidgetHead};

const URL: &str = "/api/v1/updates";
const AUTO: &str = "/api/v1/updates/auto";
const KERNEL: &str = "/api/v1/updates/kernel";
const BACKPORTS: &str = "/api/v1/updates/backports";
const MODES: &[(&str, &str)] = &[("off", "upd.mode_off"), ("security", "upd.mode_security"), ("all", "upd.mode_all")];

/// What the confirmation dialog asks.
#[derive(Clone)]
enum Ask {
    /// Install these packages (empty: all, a full upgrade) — removals listed.
    Install { packages: Vec<String>, removals: Vec<String> },
    Autoremove(Vec<String>),
}

/// A task followed in the dialog: title and id.
type Run = Option<(String, String)>;

#[component]
pub fn Updates() -> impl IntoView {
    let st = Remote::<UpdatesStatus>::new(URL);
    let auto = Remote::<AutoUpdatesState>::with_config(AUTO);
    let kernel = Remote::<KernelStatus>::new(KERNEL);
    let backports = Remote::<BackportsState>::new(BACKPORTS);
    #[cfg(feature = "demo")]
    demo::fill(st, auto, kernel);
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
            </div>
            {move || ask.get().map(|a| ask_dialog(a, ask, start))}
            {move || run.get().map(|(title, id)| run_dialog(title, id, run, st))}
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
        let held = (!s.held.is_empty()).then(|| ui::kv("upd.held", text(s.held.join(", "))));
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

fn run_dialog(title: String, id: String, run: RwSignal<Run>, st: Remote<UpdatesStatus>) -> AnyView {
    let close = Callback::new(move |_| run.set(None));
    let body = view! { <TaskRun id=id on_done=Callback::new(move |_| st.reload()) /> }.into_any();
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

fn hhmm(h: u8, m: u8) -> String {
    format!("{h:02}:{m:02}")
}

fn parse_hhmm(v: &str) -> Option<(u8, u8)> {
    let (h, m) = v.trim().split_once(':')?;
    let (h, m) = (h.parse::<u8>().ok()?, m.get(..2)?.parse::<u8>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

fn time_field(label: &'static str, help: &'static str, value: Signal<String>, set: Callback<String>) -> AnyView {
    let id = fid();
    let hid = format!("{id}-h");
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <input id=id class="input tnum" type="time" required aria-describedby=hid.clone() prop:value=move || value.get() on:input=move |ev| set.run(event_target_value(&ev)) />
            <span class="field-help" id=hid>{tr(help)}</span>
        </div>
    }
    .into_any()
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
    let (start, set_start) = lens(d, |a| hhmm(a.hour, a.minute), |a, v| if let Some((h, m)) = parse_hhmm(&v) { (a.hour, a.minute) = (h, m) });
    let (window, set_window) = lens(d, |a| a.window_min.to_string(), |a, v| a.window_min = v.trim().parse().unwrap_or(0));
    let (reboot, set_reboot) = lens_bool(d, |a| a.reboot, |a, v| a.reboot = v);
    let (rtime, set_rtime) = lens(d, |a| hhmm(a.reboot_hour, a.reboot_minute), |a, v| if let Some((h, m)) = parse_hhmm(&v) { (a.reboot_hour, a.reboot_minute) = (h, m) });
    let off = move || d.with(|a| a.mode == AutoMode::Off);
    let invalid = move || d.with(|a| !(10..=360).contains(&a.window_min));
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
        error.set(None);
    });
    view! {
        {(!s.unattended_installed).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upd.no_unattended")} }.into_any()))}
        <form class="form" on:submit=move |ev| ev.prevent_default()>
            {segmented("upd.mode", MODES, mode, set_mode)}
            <p class="field-help">{tr("upd.mode_help")}</p>
            <div hidden=off>
                {time_field("upd.start", "upd.start_help", start, set_start)}
                {number_field("upd.window", Some("upd.window_help"), window, set_window, 10, 360)}
                {switch("upd.reboot", Some("upd.reboot_help"), reboot, set_reboot)}
                <div hidden=move || !reboot.get()>{time_field("upd.reboot_time", "upd.reboot_time_help", rtime, set_rtime)}</div>
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
        <section class="card widget w-6">
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

// ------------------------------------------------------------------ demo data

#[cfg(feature = "demo")]
mod demo {
    use super::*;
    use crate::live::{Conn, use_live};

    fn pkg(name: &str, cur: Option<&str>, new: &str, origin: &str, security: bool) -> UpdatePackage {
        UpdatePackage { name: name.into(), arch: "amd64".into(), current: cur.map(Into::into), candidate: new.into(), origin: origin.into(), security }
    }

    pub fn fill(st: Remote<UpdatesStatus>, auto: Remote<AutoUpdatesState>, kernel: Remote<KernelStatus>) {
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
