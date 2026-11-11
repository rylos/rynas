//! Hardware › UPS › setup wizard (`/api/v1/ups/device`, task `ups.detect`,
//! `/api/v1/ups/drivers`, `/api/v1/ups/plan/device`): find the UPS (USB scan and test runs of
//! the candidate drivers, or a setting entered by hand: any NUT driver, serial port, network
//! address, extra options), choose, review what changes, apply (`ups.device`), then the live
//! status. The card offers it when no UPS reports, and "Change UPS" otherwise.
//!
//! Driver output and option names are NUT's own (English, monospace). With a hardware module's
//! UPS layer the wizard's texts about who applies the setting come from the module
//! (`crate::modules::ups_key`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::ups::*;

use super::ups::state_of;
use crate::api;
use crate::modules::{self, UpsSlot};
use crate::form::{form_error, spinner, switch};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::live::use_live;
use crate::tasks::TaskRun;
use crate::ui::{self, Modal, Remote, Skeleton, Status, Tone, WidgetHead};

const DEVICE: &str = "/api/v1/ups/device";
const DETECT: &str = "/api/v1/ups/detect";
const DRIVERS: &str = "/api/v1/ups/drivers";
const PLAN: &str = "/api/v1/ups/plan/device";
const UPDATE_NUT: &str = "/api/v1/ups/update-nut";
const KEY: &str = crate::config::keys::UPS_DEVICE;

/// `driver · port · k=v …` of a setting.
pub fn summary(d: &UpsDevice) -> String {
    let mut s = format!("{} · {}", d.driver, d.port);
    let o = options_text(&d.options);
    if !o.is_empty() {
        s.push_str(" · ");
        s.push_str(&o);
    }
    s
}

/// Options as typed in the form: `key=value` or a bare flag, separated by spaces.
pub fn options_text(o: &[UpsOption]) -> String {
    o.iter()
        .map(|o| match &o.value {
            Some(v) => format!("{}={v}", o.key),
            None => o.key.clone(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The form's options text back (spaces, commas or new lines between options).
pub fn parse_options(s: &str) -> Vec<UpsOption> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .map(|w| match w.split_once('=') {
            Some((k, v)) => UpsOption { key: k.trim().to_ascii_lowercase(), value: Some(v.trim().to_string()) },
            None => UpsOption { key: w.to_ascii_lowercase(), value: None },
        })
        .collect()
}

/// Tone and i18n key of a test-run outcome.
pub fn probe_label(s: UpsProbeState) -> (Tone, &'static str) {
    match s {
        UpsProbeState::Ok => (Tone::Ok, "upsw.p_ok"),
        UpsProbeState::Failed => (Tone::Err, "upsw.p_failed"),
        UpsProbeState::NotRun => (Tone::Unknown, "upsw.p_not_run"),
        UpsProbeState::Busy => (Tone::Warn, "upsw.p_busy"),
        UpsProbeState::NeedsNewerNut => (Tone::Warn, "upsw.p_newer"),
        UpsProbeState::NoDriver => (Tone::Warn, "upsw.p_no_driver"),
    }
}

pub fn source_key(s: UpsCandidateSource) -> &'static str {
    match s {
        UpsCandidateSource::Known => "upsw.src_known",
        UpsCandidateSource::Scanner => "upsw.src_scanner",
        UpsCandidateSource::Generic => "upsw.src_generic",
        UpsCandidateSource::Manual => "upsw.src_manual",
    }
}

/// The note of a detection where nothing answered: a newer NUT needed, a busy device, no UPS
/// on USB, or nothing worked.
pub fn failure_key(r: &UpsDetectReport) -> Option<&'static str> {
    if r.found.is_some() {
        None
    } else if r.busy {
        Some("upsw.busy")
    } else if r.candidates.iter().any(|c| c.probe.state == UpsProbeState::NeedsNewerNut) {
        Some("upsw.need_newer")
    } else if r.candidates.is_empty() {
        Some("upsw.no_usb")
    } else {
        Some("upsw.none_answered")
    }
}

/// The text of a "needs a newer NUT" note: the update rynas offers, the packages it misses,
/// or a plain "install a newer NUT" (a module's layer, or nothing rynas can install).
pub fn newer_key(update: Option<&UpsNutUpdate>) -> &'static str {
    match update {
        Some(UpsNutUpdate { source: Some(_), .. }) => "upsw.need_newer_update",
        Some(_) => "upsw.need_newer_files",
        None => "upsw.need_newer",
    }
}

/// The order candidates are shown in: the one that answered first, then the others by
/// outcome (needing a newer NUT before failures, untried last), the tried order otherwise.
pub fn shown_order(r: &UpsDetectReport) -> Vec<usize> {
    let rank = |i: usize| {
        if r.found == Some(i) {
            return 0;
        }
        match r.candidates[i].probe.state {
            UpsProbeState::Ok => 1,
            UpsProbeState::NeedsNewerNut => 2,
            UpsProbeState::Failed | UpsProbeState::Busy => 3,
            UpsProbeState::NoDriver => 4,
            UpsProbeState::NotRun => 5,
        }
    };
    let mut order: Vec<usize> = (0..r.candidates.len()).collect();
    order.sort_by_key(|&i| rank(i));
    order
}

// ------------------------------------------------------------------ the card

/// `installed` changes when NUT was just installed from the status card: the state is read again.
pub fn card(installed: ReadSignal<u32>) -> AnyView {
    let live = use_live();
    let state = Remote::<UpsDeviceState>::with_config(DEVICE);
    Effect::new(move |prev: Option<u32>| {
        let n = installed.get();
        if prev.is_some_and(|p| p != n) {
            state.reload();
        }
        n
    });
    #[cfg(feature = "demo")]
    if crate::auth::auth().phase.get_untracked() == crate::auth::Phase::Demo {
        state.data.set(Some(crate::demo::ups_device()));
    }
    let open = RwSignal::new(false);
    let connected = Memo::new(move |_| live.ups.with(|u| u.as_ref().is_some_and(|u| u.link == UpsLink::Connected)));
    let body = move || {
        if let Some(v) = state.gate(2) {
            return v;
        }
        let st = state.data.get().unwrap_or_default();
        match st.blocked {
            // the status card and the policy card already ask for NUT
            Some(UpsSetupBlock::NutMissing) => return view! { <p class="muted">{t("upsw.need_nut")}</p> }.into_any(),
            Some(UpsSetupBlock::ModuleTooOld) => {
                return ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t(modules::ups_key(UpsSlot::SetupTooOld))} }.into_any());
            }
            None => {}
        }
        let current = st.pending.clone().or(st.applied.clone());
        view! {
            {move || (!connected.get()).then(|| view! { <p>{t("upsw.intro")}</p> })}
            {current.map(|c| view! {
                <dl class="kvs">
                    <div class="kv"><dt>{tr("ups.device")}</dt><dd class="mono">{summary(&c.device)}</dd></div>
                    <div class="kv"><dt>{tr("upsw.name")}</dt><dd class="mono">{c.name}</dd></div>
                </dl>
            })}
            <div class="btn-row">
                {move || if connected.get() {
                    view! { <button type="button" class="btn ghost" on:click=move |_| open.set(true)><Icon icon=icons::SETTINGS size=16 />{tr("upsw.change")}</button> }.into_any()
                } else {
                    view! { <button type="button" class="btn primary" on:click=move |_| open.set(true)><Icon icon=icons::SEARCH size=16 />{tr("upsw.start")}</button> }.into_any()
                }}
            </div>
        }
        .into_any()
    };
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::USB title="upsw.title" />
            {body}
            {move || open.get().then(|| view! {
                <Wizard state=state on_close=Callback::new(move |_| { open.set(false); state.reload(); }) />
            })}
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ the wizard

#[derive(Clone, PartialEq)]
enum Step {
    Start,
    Running(String),
    Results,
    /// The NUT update task.
    Updating(String),
    Manual,
    Review,
    Done,
}

impl Step {
    /// Position in the stepper (1-based).
    fn n(&self) -> u8 {
        match self {
            Step::Start | Step::Manual => 1,
            Step::Running(_) | Step::Results | Step::Updating(_) => 2,
            Step::Review => 3,
            Step::Done => 4,
        }
    }
}

/// The manual entry form.
#[derive(Clone, PartialEq, Default)]
struct Manual {
    driver: String,
    port: String,
    options: String,
}

#[component]
fn Wizard(state: Remote<UpsDeviceState>, on_close: Callback<()>) -> impl IntoView {
    let live = use_live();
    let st = state.data.get_untracked().unwrap_or_default();
    let module = st.backend == UpsBackend::Module;
    let step = RwSignal::new(Step::Start);
    let stop_drivers = RwSignal::new(st.drivers_running);
    let report = RwSignal::new(None::<UpsDetectReport>);
    let chosen = RwSignal::new(None::<UpsDevice>);
    let tested = RwSignal::new(true);
    let current = st.pending.clone().or(st.applied.clone());
    let name = RwSignal::new(current.as_ref().map_or_else(|| UPS_DEVICE_NAME.to_string(), |c| c.name.clone()));
    let take_over = RwSignal::new(Vec::<String>::new());
    let manual = RwSignal::new(current.as_ref().map_or_else(
        || Manual { port: "auto".into(), ..Default::default() },
        |c| Manual { driver: c.device.driver.clone(), port: c.device.port.clone(), options: options_text(&c.device.options) },
    ));
    let plan = RwSignal::new(None::<Result<UpsSetupPlan, String>>);
    let drivers = RwSignal::new(None::<Vec<NutDriver>>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // the NUT version just installed by the update (shown on the first step)
    let updated = RwSignal::new(None::<String>);

    let update_nut = move || {
        error.set(None);
        busy.set(true);
        spawn_local(async move {
            let r = api::start_task(UPDATE_NUT, None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(id) => {
                    let _ = step.try_set(Step::Updating(id));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let nut_updated = Callback::new(move |task: rynas_api_types::TaskInfo| {
        spawn_local(async move {
            let ok = task.status == TaskStatus::Ok;
            match api::get::<UpsDeviceState>(DEVICE).await {
                Ok(s) => {
                    if ok {
                        let _ = updated.try_set(s.nut_version.clone());
                        let _ = report.try_set(None);
                        let _ = chosen.try_set(None);
                    }
                    state.data.set(Some(s));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            if ok {
                let _ = step.try_set(Step::Start);
            } else {
                let _ = error.try_set(Some(crate::tasks::task_error(&task)));
                let _ = step.try_set(Step::Results);
            }
        });
    });

    let detect = move |device: Option<UpsDevice>| {
        error.set(None);
        updated.set(None);
        busy.set(true);
        let req = UpsDetectRequest { device, stop_drivers: stop_drivers.get_untracked() };
        spawn_local(async move {
            let r = api::start_task(DETECT, serde_json::to_string(&req).ok()).await;
            let _ = busy.try_set(false);
            match r {
                Ok(id) => {
                    let _ = step.try_set(Step::Running(id));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let detected = Callback::new(move |task: rynas_api_types::TaskInfo| {
        spawn_local(async move {
            let r = api::get::<UpsDeviceState>(DEVICE).await;
            match r {
                Ok(s) => {
                    let rep = s.detect.clone();
                    let found = rep.as_ref().and_then(|r| r.found.map(|i| r.candidates[i].device.clone()));
                    let _ = report.try_set(rep);
                    let _ = chosen.try_set(found);
                    let _ = tested.try_set(true);
                    state.data.set(Some(s));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            if task.status != TaskStatus::Ok {
                let _ = error.try_set(Some(crate::tasks::task_error(&task)));
            }
            let _ = step.try_set(Step::Results);
        });
    });
    let load_plan = move || {
        let Some(device) = chosen.get_untracked() else { return };
        let body = UpsDeviceConfig { name: name.get_untracked().trim().to_string(), device, take_over: take_over.get_untracked() };
        plan.set(None);
        spawn_local(async move {
            let r = api::post::<_, UpsSetupPlan>(PLAN, Some(&body)).await.map_err(|e| e.text());
            let _ = plan.try_set(Some(r));
        });
    };
    let open_manual = move || {
        error.set(None);
        if drivers.with_untracked(Option::is_none) {
            spawn_local(async move {
                if let Ok(d) = api::get::<Vec<NutDriver>>(DRIVERS).await {
                    let _ = drivers.try_set(Some(d));
                }
            });
        }
        step.set(Step::Manual);
    };
    let manual_device = move || {
        manual.with_untracked(|m| UpsDevice { driver: m.driver.trim().to_string(), port: m.port.trim().to_string(), options: parse_options(&m.options) })
    };
    let to_review = move || {
        error.set(None);
        take_over.set(Vec::new());
        step.set(Step::Review);
        load_plan();
    };
    let apply = move || {
        let Some(device) = chosen.get_untracked() else { return };
        let body = UpsDeviceConfig { name: name.get_untracked().trim().to_string(), device, take_over: take_over.get_untracked() };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::put::<_, UpsDeviceState>(DEVICE, &body).await {
                Ok(_) => {
                    if crate::config::apply_now(&[KEY], error).await {
                        let _ = step.try_set(Step::Done);
                        // the driver needs a few seconds before upsd reports it
                        for _ in 0..6 {
                            live.refresh_ups();
                            api::sleep(3000).await;
                        }
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            let _ = busy.try_set(false);
        });
    };

    let steps = ["upsw.s_find", "upsw.s_test", "upsw.s_review", "upsw.s_done"];
    let stepper = move || {
        let cur = step.with(Step::n);
        view! {
            <ol class="stepper" aria-label=tr("wiz.steps")>
                {steps.iter().enumerate().map(|(i, k)| {
                    let n = i as u8 + 1;
                    view! {
                        <li class={if cur == n { "current" } else if cur > n { "done" } else { "" }} aria-current={(cur == n).then_some("step")}>
                            <span class="step-num">{if cur > n { view! { <Icon icon=icons::CHECK size=12 /> }.into_any() } else { n.to_string().into_any() }}</span>
                            <span class="step-label">{tr(k)}</span>
                        </li>
                    }
                }).collect_view()}
            </ol>
        }
    };

    let body = move || match step.get() {
        Step::Start => view! {
            {move || updated.get().map(|v| ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! { {tf("upsw.updated", &[("v", &v)])} }.into_any()))}
            <p>{t(if module { modules::ups_key(UpsSlot::SetupFind) } else { "upsw.find_text" })}</p>
            {st.drivers_running.then(|| switch("upsw.stop_drivers", Some("upsw.stop_drivers_help"), stop_drivers.into(), Callback::new(move |v| stop_drivers.set(v))))}
            <div class="btn-row">
                <button type="button" class="btn primary" disabled=move || busy.get() on:click=move |_| detect(None)>{spinner(busy)}<Icon icon=icons::SEARCH size=16 />{tr("upsw.detect")}</button>
                <button type="button" class="btn ghost" on:click=move |_| open_manual()>{tr("upsw.manual")}</button>
            </div>
        }
        .into_any(),
        Step::Running(id) => view! {
            <p class="muted small">{t("upsw.testing")}</p>
            <TaskRun id=id on_done=detected />
        }
        .into_any(),
        Step::Results => {
            let update = state.data.with_untracked(|s| s.as_ref().and_then(|s| s.nut_update.clone()));
            results(report, chosen, stop_drivers, Callback::new(move |_| detect(None)), update, busy, Callback::new(move |_| update_nut())).into_any()
        }
        Step::Updating(id) => view! {
            <p class="muted small">{t("upsw.updating")}</p>
            <TaskRun id=id on_done=nut_updated />
        }
        .into_any(),
        Step::Manual => manual_form(manual, drivers).into_any(),
        Step::Review => review(name, take_over, plan, chosen, tested, module, Callback::new(move |_| load_plan())).into_any(),
        Step::Done => done().into_any(),
    };

    let foot = move || {
        let s = step.get();
        let back = match s {
            Step::Results | Step::Manual => Some(Step::Start),
            Step::Review => Some(if report.with_untracked(Option::is_some) { Step::Results } else { Step::Manual }),
            _ => None,
        };
        view! {
            {form_error(error)}
            <div class="wizard-foot">
                {match back {
                    Some(b) => view! { <button type="button" class="btn ghost" on:click=move |_| { error.set(None); step.set(b.clone()); }><Icon icon=icons::ARROW_LEFT size=14 />{tr("back")}</button> }.into_any(),
                    None => view! { <button type="button" class="btn ghost" on:click=move |_| on_close.run(())>{tr(if s == Step::Done { "close" } else { "cancel" })}</button> }.into_any(),
                }}
                {match s {
                    Step::Results => view! {
                        <span class="btn-row">
                            <button type="button" class="btn ghost" on:click=move |_| open_manual()>{tr("upsw.manual")}</button>
                            <button type="button" class="btn primary" disabled=move || chosen.with(Option::is_none) on:click=move |_| to_review()>{tr("wiz.next")}<Icon icon=icons::CHEVRON_RIGHT size=14 /></button>
                        </span>
                    }
                    .into_any(),
                    Step::Manual => {
                        let filled = move || manual.with(|m| !m.driver.trim().is_empty() && !m.port.trim().is_empty());
                        view! {
                            <span class="btn-row">
                                <button type="button" class="btn ghost" disabled=move || !filled() on:click=move |_| { chosen.set(Some(manual_device())); tested.set(false); report.set(None); to_review(); }>{tr("upsw.skip_test")}</button>
                                <button type="button" class="btn primary" disabled=move || !filled() || busy.get() on:click=move |_| detect(Some(manual_device()))>{spinner(busy)}{tr("upsw.test")}</button>
                            </span>
                        }
                        .into_any()
                    }
                    Step::Review => view! {
                        <button type="button" class="btn primary" disabled=move || busy.get() || !plan.with(|p| matches!(p, Some(Ok(p)) if p.conflicts.is_empty())) on:click=move |_| apply()>{spinner(busy)}{tr("upsw.apply")}</button>
                    }
                    .into_any(),
                    _ => ().into_any(),
                }}
            </div>
        }
    };

    view! {
        <Modal title=Signal::derive(|| t("upsw.title").to_string()) on_close=on_close wide=true>
            <div class="wizard">
                {stepper}
                {body}
                {foot}
            </div>
        </Modal>
    }
}

#[allow(clippy::too_many_arguments)]
fn results(
    report: RwSignal<Option<UpsDetectReport>>,
    chosen: RwSignal<Option<UpsDevice>>,
    stop_drivers: RwSignal<bool>,
    again: Callback<()>,
    update: Option<UpsNutUpdate>,
    busy: RwSignal<bool>,
    on_update: Callback<()>,
) -> impl IntoView {
    let Some(r) = report.get_untracked() else {
        return view! { <p class="muted">{t("upsw.no_report")}</p> }.into_any();
    };
    let note = failure_key(&r).map(|k| {
        let have = r.nut_version.clone().unwrap_or_else(|| "?".into());
        let need = r.candidates.iter().find_map(|c| (c.probe.state == UpsProbeState::NeedsNewerNut).then(|| c.min_nut.clone()).flatten()).unwrap_or_default();
        let newer = k == "upsw.need_newer";
        let k = if newer { newer_key(update.as_ref()) } else { k };
        let v = update.as_ref().map(|u| u.version.clone()).unwrap_or_default();
        let dir = update.as_ref().map(|u| u.dir.clone()).unwrap_or_default();
        let source = match update.as_ref().and_then(|u| u.source) {
            Some(UpsNutSource::Apt) => t("upsw.nut_src_apt"),
            Some(UpsNutSource::Rynas) => t("upsw.nut_src_rynas"),
            None => "",
        };
        let can_update = newer && update.as_ref().is_some_and(|u| u.source.is_some()) && super::ups::is_admin();
        let label = tf("upsw.update_nut", &[("v", &v)]);
        ui::note(if k == "upsw.busy" { Tone::Warn } else { Tone::Err }, icons::TRIANGLE_ALERT, view! {
            <div class="ups-note">
                <p>{tf(k, &[("have", &have), ("need", &need), ("v", &v), ("source", source), ("dir", &dir)])}</p>
                {can_update.then(|| view! {
                    <div class="btn-row">
                        <button type="button" class="btn primary sm" disabled=move || busy.get() on:click=move |_| on_update.run(())>
                            {spinner(busy)}<Icon icon=icons::DOWNLOAD size=14 />{label}
                        </button>
                    </div>
                })}
            </div>
        }.into_any())
    });
    let found = r.found.map(|i| r.candidates[i].probe.clone());
    let usb = r.usb.clone();
    let order = shown_order(&r);
    view! {
        {found.map(|p| ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! {
            {tf("upsw.found", &[("model", &p.model.clone().or(p.manufacturer.clone()).unwrap_or_else(|| "UPS".into())), ("status", p.status.as_deref().unwrap_or("?")), ("charge", &p.battery_charge_pct.map_or("?".to_string(), |c| format!("{c} %")))])}
        }.into_any()))}
        {note}
        {r.busy.then(|| view! {
            {switch("upsw.stop_drivers", Some("upsw.stop_drivers_help"), stop_drivers.into(), Callback::new(move |v| stop_drivers.set(v)))}
            <div class="btn-row"><button type="button" class="btn primary" disabled=move || !stop_drivers.get() on:click=move |_| again.run(())>{tr("upsw.detect_again")}</button></div>
        })}
        {(!usb.is_empty()).then(|| view! {
            <dl class="kvs">
                {usb.into_iter().map(|u| view! {
                    <div class="kv"><dt>{tr("upsw.usb")}</dt><dd><span class="mono">{format!("{}:{}", u.vendor_id, u.product_id)}</span>" "{[u.vendor, u.product].into_iter().flatten().collect::<Vec<_>>().join(" ")}</dd></div>
                }).collect_view()}
            </dl>
        })}
        {r.nut_version.clone().map(|v| view! { <p class="muted small">{tf("upsw.nut_version", &[("v", &v)])}</p> })}
        <h3 class="section-title">{tr("upsw.candidates")}</h3>
        <div class="ups-cands" role="radiogroup" aria-label=tr("upsw.candidates")>
            {order.into_iter().map(|i| {
                let c = r.candidates[i].clone();
                let ok = c.probe.state == UpsProbeState::Ok;
                let dev = c.device.clone();
                let dev2 = c.device.clone();
                let selected = Memo::new(move |_| chosen.with(|x| x.as_ref() == Some(&dev2)));
                let (tone, key) = probe_label(c.probe.state);
                let tail = c.probe.log_tail.join("\n");
                let opts = format!("{} {}", c.device.port, options_text(&c.device.options));
                let id = crate::form::fid();
                let for_id = id.clone();
                view! {
                    <div class="ups-cand" class:selected=move || selected.get() class:idle=c.probe.state == UpsProbeState::NotRun>
                        <label class="ups-cand-head" for=for_id>
                            <input id=id type="radio" name="ups-candidate" disabled=!ok
                                prop:checked=move || selected.get()
                                on:change=move |_| chosen.set(Some(dev.clone())) />
                            <strong class="mono">{c.device.driver.clone()}</strong>
                            <Status tone=tone text=t(key).to_string() />
                        </label>
                        <p class="muted small">
                            {tr(source_key(c.source))}
                            {c.min_nut.clone().filter(|_| c.probe.state == UpsProbeState::NeedsNewerNut).map(|v| format!(" · {}", tf("upsw.min_nut", &[("v", &v)])))}
                        </p>
                        <p class="ups-cand-opts mono small">{opts}</p>
                        {(!tail.is_empty()).then(|| view! {
                            <details class="ups-cand-log">
                                <summary><Icon icon=icons::CHEVRON_RIGHT size=14 />{tr("upsw.driver_output")}</summary>
                                <div class="pre-text log-text" lang="en">{tail}</div>
                            </details>
                        })}
                    </div>
                }
            }).collect_view()}
        </div>
    }
    .into_any()
}

fn manual_form(manual: RwSignal<Manual>, drivers: RwSignal<Option<Vec<NutDriver>>>) -> impl IntoView {
    let id_d = crate::form::fid();
    let id_p = crate::form::fid();
    let id_o = crate::form::fid();
    let hint = move || {
        let d = manual.with(|m| m.driver.clone());
        drivers.with(|l| l.as_ref().and_then(|l| l.iter().find(|x| x.name == d)).map(|x| x.manufacturers.join(", "))).filter(|s| !s.is_empty())
    };
    view! {
        <p class="muted small">{t("upsw.manual_text")}</p>
        <div class="field">
            <label class="field-label" for=id_d.clone()>{tr("upsw.driver")}</label>
            <select id=id_d class="input mono" on:change=move |ev| { let v = event_target_value(&ev); manual.update(|m| m.driver = v); }>
                <option value="" selected=move || manual.with(|m| m.driver.is_empty())>{tr("upsw.pick_driver")}</option>
                {move || drivers.get().unwrap_or_default().into_iter().map(|d| {
                    let n = d.name.clone();
                    let n2 = d.name.clone();
                    view! { <option value=n selected=move || manual.with(|m| m.driver == n2)>{d.name}</option> }
                }).collect_view()}
            </select>
            <span class="field-help">{move || hint().map(|h| tf("upsw.driver_for", &[("list", &h)])).unwrap_or_else(|| t("upsw.driver_help").to_string())}</span>
        </div>
        <div class="field">
            <label class="field-label" for=id_p.clone()>{tr("upsw.port")}</label>
            <input id=id_p class="input mono" type="text" spellcheck="false" autocomplete="off" maxlength="128"
                prop:value=move || manual.with(|m| m.port.clone())
                on:input=move |ev| { let v = event_target_value(&ev); manual.update(|m| m.port = v); } />
            <span class="field-help">{tr("upsw.port_help")}</span>
        </div>
        <div class="field">
            <label class="field-label" for=id_o.clone()>{tr("upsw.options")}</label>
            <textarea id=id_o class="input area mono" rows="3" spellcheck="false"
                prop:value=move || manual.with(|m| m.options.clone())
                on:input=move |ev| { let v = event_target_value(&ev); manual.update(|m| m.options = v); }></textarea>
            <span class="field-help">{tr("upsw.options_help")}</span>
        </div>
    }
}

fn review(
    name: RwSignal<String>,
    take_over: RwSignal<Vec<String>>,
    plan: RwSignal<Option<Result<UpsSetupPlan, String>>>,
    chosen: RwSignal<Option<UpsDevice>>,
    tested: RwSignal<bool>,
    module: bool,
    reload: Callback<()>,
) -> impl IntoView {
    let id_n = crate::form::fid();
    let shown = move || {
        let Some(p) = plan.get() else {
            return view! { <Skeleton lines=4 /> }.into_any();
        };
        let p = match p {
            Ok(p) => p,
            Err(e) => return view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{ui::err_view(e)}</p> }.into_any(),
        };
        let taken = take_over.get_untracked();
        view! {
            {(!p.conflicts.is_empty() || !taken.is_empty()).then(|| {
                let names: Vec<String> = p.conflicts.iter().chain(taken.iter()).cloned().collect();
                view! {
                    {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upsw.conflict")} }.into_any())}
                    {names.into_iter().map(|n| {
                        let n2 = n.clone();
                        let on = Signal::derive(move || take_over.with(|t| t.contains(&n2)));
                        let label = tf("upsw.take_over", &[("name", &n)]);
                        view! {
                            <label class="switch-row">
                                <input type="checkbox" role="switch" class="switch" prop:checked=move || on.get() on:change=move |ev| {
                                    let v = event_target_checked(&ev);
                                    let n = n.clone();
                                    take_over.update(|t| { t.retain(|x| x != &n); if v { t.push(n); } });
                                    reload.run(());
                                } />
                                <span class="switch-text"><span class="switch-label">{label}</span></span>
                            </label>
                        }
                    }).collect_view()}
                }
            })}
            {p.files.into_iter().map(|f| view! {
                <h3 class="section-title mono">{f.path}</h3>
                {(!f.removed.is_empty()).then(|| view! {
                    <p class="muted small">{t("upsw.removed")}</p>
                    <span class="pre-text log-text diff-old">{f.removed.join("\n")}</span>
                })}
                <span class="pre-text log-text">{f.after}</span>
            }).collect_view()}
            <p class="muted small">{t(if module { modules::ups_key(UpsSlot::SetupCommands) } else { "upsw.commands" })}</p>
            <span class="pre-text log-text">{p.commands.join("\n")}</span>
        }
        .into_any()
    };
    view! {
        <dl class="kvs">
            <div class="kv"><dt>{tr("ups.device")}</dt><dd class="mono">{move || chosen.get().map(|d| summary(&d)).unwrap_or_default()}</dd></div>
        </dl>
        {move || (!tested.get()).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {t("upsw.untested")} }.into_any()))}
        {(!module).then(|| view! {
            <div class="field">
                <label class="field-label" for=id_n.clone()>{tr("upsw.name")}</label>
                <input id=id_n class="input mono" type="text" spellcheck="false" autocomplete="off" maxlength="32"
                    prop:value=move || name.get()
                    on:input=move |ev| name.set(event_target_value(&ev))
                    on:change=move |_| reload.run(()) />
                <span class="field-help">{tr("upsw.name_help")}</span>
            </div>
        })}
        {shown}
    }
}

fn done() -> impl IntoView {
    let live = use_live();
    let st = move || {
        live.ups.with(|u| match u {
            Some(u) if u.link == UpsLink::Connected => {
                let (tone, key) = state_of(u);
                view! { <Status tone=tone text=t(key).to_string() /> }.into_any()
            }
            _ => view! { <span class="muted">{t("upsw.waiting")}</span> }.into_any(),
        })
    };
    view! {
        {ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! { {t("upsw.applied")} }.into_any())}
        <dl class="kvs"><div class="kv"><dt>{tr("ups.status")}</dt><dd>{st}</dd></div></dl>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n;

    #[test]
    fn options_round_trip() {
        let o = parse_options("subdriver=armac protocol=megatec\nnorating, pollfreq=30");
        assert_eq!(o.len(), 4);
        assert_eq!(o[2], UpsOption { key: "norating".into(), value: None });
        assert_eq!(options_text(&o), "subdriver=armac protocol=megatec norating pollfreq=30");
        let d = UpsDevice { driver: "nutdrv_qx".into(), port: "auto".into(), options: o };
        assert_eq!(summary(&d), "nutdrv_qx · auto · subdriver=armac protocol=megatec norating pollfreq=30");
    }

    #[test]
    fn labels_exist_and_failures_explain() {
        for s in [UpsProbeState::Ok, UpsProbeState::Failed, UpsProbeState::NotRun, UpsProbeState::Busy, UpsProbeState::NeedsNewerNut, UpsProbeState::NoDriver] {
            assert!(i18n::try_t(probe_label(s).1).is_some());
        }
        for s in [UpsCandidateSource::Known, UpsCandidateSource::Scanner, UpsCandidateSource::Generic, UpsCandidateSource::Manual] {
            assert!(i18n::try_t(source_key(s)).is_some());
        }
        let cand = |state| UpsCandidate {
            device: UpsDevice { driver: "x".into(), port: "auto".into(), options: vec![] },
            source: UpsCandidateSource::Known,
            min_nut: Some("2.8.5".into()),
            probe: UpsProbe { state, ..Default::default() },
        };
        let mut r = UpsDetectReport { candidates: vec![cand(UpsProbeState::NeedsNewerNut), cand(UpsProbeState::Failed)], ..Default::default() };
        assert_eq!(failure_key(&r), Some("upsw.need_newer"));
        let text = i18n::text_in(i18n::Lang::En, "upsw.need_newer").unwrap();
        assert!(text.contains("{need}") && text.contains("{have}"));
        r.candidates.remove(0);
        assert_eq!(failure_key(&r), Some("upsw.none_answered"));
        r.found = Some(0);
        assert_eq!(failure_key(&r), None);
        assert_eq!(failure_key(&UpsDetectReport::default()), Some("upsw.no_usb"));
    }

    #[test]
    fn newer_nut_notes_and_candidate_order() {
        let up = |source| UpsNutUpdate { version: "2.8.5".into(), source, dir: "/var/lib/rynas/packages".into() };
        assert_eq!(newer_key(None), "upsw.need_newer");
        assert_eq!(newer_key(Some(&up(Some(UpsNutSource::Rynas)))), "upsw.need_newer_update");
        assert_eq!(newer_key(Some(&up(None))), "upsw.need_newer_files");
        for k in ["upsw.need_newer", "upsw.need_newer_update", "upsw.need_newer_files"] {
            for lang in [i18n::Lang::En, i18n::Lang::It] {
                let s = i18n::text_in(lang, k).unwrap();
                assert!(s.contains("{need}") && s.contains("{have}"), "{k}");
                if k != "upsw.need_newer" {
                    assert!(s.contains("{v}"), "{k}");
                }
            }
        }
        assert!(i18n::text_in(i18n::Lang::En, "upsw.need_newer_files").unwrap().contains("{dir}"));
        assert!(i18n::text_in(i18n::Lang::It, "upsw.need_newer_update").unwrap().contains("{source}"));
        for k in ["upsw.update_nut", "upsw.updated", "upsw.updating", "upsw.driver_output", "upsw.nut_src_apt", "upsw.nut_src_rynas", "task.ups.update-nut"] {
            assert!(i18n::try_t(k).is_some(), "{k}");
        }
        let cand = |state| UpsCandidate {
            device: UpsDevice { driver: "x".into(), port: "auto".into(), options: vec![] },
            source: UpsCandidateSource::Generic,
            min_nut: None,
            probe: UpsProbe { state, ..Default::default() },
        };
        let mut r = UpsDetectReport {
            candidates: vec![cand(UpsProbeState::NeedsNewerNut), cand(UpsProbeState::Failed), cand(UpsProbeState::Ok), cand(UpsProbeState::NotRun)],
            found: Some(2),
            ..Default::default()
        };
        assert_eq!(shown_order(&r), [2, 0, 1, 3]);
        r.found = None;
        r.candidates[2].probe.state = UpsProbeState::Failed;
        assert_eq!(shown_order(&r), [0, 1, 2, 3], "tried order among equals");
    }

    /// Texts a plain Debian user sees name no other system and no module platform (the
    /// module's own texts are `syn.*`, in its table).
    #[test]
    fn neutral_without_a_module() {
        for lang in [i18n::Lang::En, i18n::Lang::It] {
            for (k, en, _) in i18n::entries().into_iter().filter(|e| e.0.starts_with("upsw.")) {
                let s = i18n::text_in(lang, k).unwrap_or(en);
                assert!(!s.to_ascii_lowercase().contains("synodeb"), "{k}: {s}");
                assert!(!s.to_ascii_lowercase().contains("backports"), "{k}: {s}");
            }
        }
    }
}
