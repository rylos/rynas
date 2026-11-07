//! The install wizard: Options (the entry's `form.json` through [`SchemaForm`], the stack
//! name), Folders & shares, Network, Review, then the install task and the secrets shown once.
//!
//! Every Next asks the server for a plan (`POST /apps/catalog/{id}/plan`, no side effects):
//! errors of the current step's fields keep the step, the others wait for their step. The
//! Review shows the compose file and `.env` that will be written (secrets masked).

use std::collections::BTreeMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::apps::{AppInstallPlan, AppInstallRequest, AppInstallStarted, FieldErrorKind, FolderIntent, FolderSsd, PlanWarning, ShareAccess};
use rynas_api_types::docker::{DockerSettingsState, valid_stack_name};
use rynas_api_types::ShareView;

use super::catalog::{intent_text, notes_view, ssd_key};
use super::{AppDetail, apps, ram_pill};
use crate::api::{self, DETAIL_SEP};
use crate::form::{In, form_error, spinner, text_field};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::schema_form::{self, Errors, SchemaForm, Values};
use crate::tasks::TaskRun;
use crate::ui::{self, DlgKind, Remote, Tone};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Options,
    Folders,
    Network,
    Access,
    Review,
}

fn step_key(s: Step) -> &'static str {
    match s {
        Step::Options => "apps.w.s_options",
        Step::Folders => "apps.w.s_folders",
        Step::Network => "apps.w.s_network",
        Step::Access => "apps.w.s_access",
        Step::Review => "wiz.s_review",
    }
}

pub(super) fn kind_key(k: FieldErrorKind) -> &'static str {
    use FieldErrorKind as K;
    match k {
        K::InvalidValue => "apps.fe.invalid_value",
        K::MissingValue => "apps.fe.missing_value",
        K::UnknownValue => "apps.fe.unknown_value",
        K::UnsafeValue => "apps.fe.unsafe_value",
        K::ShareMissing => "apps.fe.share_missing",
        K::BadPath => "apps.fe.bad_path",
        K::BadPort => "apps.fe.bad_port",
        K::BadStack => "apps.fe.bad_stack",
    }
}

fn warning_key(w: PlanWarning) -> &'static str {
    match w {
        PlanWarning::DockerNotInstalled => "apps.warn.docker_not_installed",
        PlanWarning::NoVolume => "apps.warn.no_volume",
        PlanWarning::EngineUnreachable => "apps.warn.engine_unreachable",
        PlanWarning::RamBelowRecommended => "apps.warn.ram_below_recommended",
        PlanWarning::PublishedOnLan => "apps.warn.published_on_lan",
        PlanWarning::ProxyInactive => "apps.warn.proxy_inactive",
    }
}

/// The wizard's state (all signals: copied into every closure).
#[derive(Clone, Copy)]
struct W {
    id: StoredValue<String>,
    detail: Remote<AppDetail>,
    settings: Remote<DockerSettingsState>,
    shares_list: Remote<Vec<ShareView>>,
    step: RwSignal<usize>,
    stack: RwSignal<String>,
    values: RwSignal<Values>,
    shares: RwSignal<BTreeMap<String, String>>,
    folders: RwSignal<BTreeMap<String, String>>,
    ports: RwSignal<BTreeMap<String, u16>>,
    /// `None`: the server's default (§7); `Some` once the administrator chose.
    publish: RwSignal<Option<bool>>,
    plan: RwSignal<Option<AppInstallPlan>>,
    planning: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    reveal: RwSignal<bool>,
    started: RwSignal<Option<AppInstallStarted>>,
    done: RwSignal<Option<TaskStatus>>,
    ask_close: RwSignal<bool>,
}

impl W {
    /// The Access step exists for apps with a web interface on their own ports.
    fn steps(self) -> Vec<Step> {
        let access = self.detail.data.with(|d| d.as_ref().is_some_and(|d| d.web.is_some() && !d.host_network && !d.card.reverse_proxy));
        let mut v = vec![Step::Options, Step::Folders, Step::Network];
        if access {
            v.push(Step::Access);
        }
        v.push(Step::Review);
        v
    }

    fn cur(self) -> Step {
        let s = self.steps();
        s[self.step.get().min(s.len() - 1)]
    }

    fn request(self) -> AppInstallRequest {
        let form = self.detail.data.with_untracked(|d| d.as_ref().map(|d| schema_form::answers(&d.form, &self.values.get_untracked())).unwrap_or_default());
        let stack = self.stack.get_untracked();
        AppInstallRequest {
            stack: (!stack.is_empty()).then_some(stack),
            form,
            shares: self.shares.get_untracked().into_iter().filter(|(_, v)| !v.is_empty()).collect(),
            folders: self.folders.get_untracked().into_iter().filter(|(_, v)| !v.is_empty()).collect(),
            ports: self.ports.get_untracked(),
            publish_lan: self.publish.get_untracked(),
            start: true,
        }
    }

    /// Server errors by field (translated kind + the server's detail).
    fn server_errors(self) -> Errors {
        self.plan.with(|p| p.as_ref().map(|p| p.errors.iter().map(|e| (e.field.clone(), format!("{}{DETAIL_SEP}{}", t(kind_key(e.kind)), e.message))).collect()).unwrap_or_default())
    }

    /// The fields a step owns.
    fn owns(self, s: Step, field: &str) -> bool {
        self.detail.data.with_untracked(|d| {
            let Some(d) = d else { return false };
            match s {
                Step::Options => field == "stack" || d.form.has(field),
                Step::Folders => d.folders.iter().any(|f| f.env == field) || d.shares.iter().any(|f| f.env == field),
                Step::Network => d.card.ports.iter().any(|p| p.env == field),
                Step::Access | Step::Review => false,
            }
        })
    }

    /// Plans the current answers; `then` runs with the plan when it arrived.
    fn replan(self, then: Option<Callback<()>>) {
        self.planning.set(true);
        self.error.set(None);
        let body = self.request();
        let url = format!("/api/v1/apps/catalog/{}/plan", api::encode(&self.id.get_value()));
        spawn_local(async move {
            let r = api::post::<_, AppInstallPlan>(&url, Some(&body)).await;
            let _ = self.planning.try_set(false);
            match r {
                Ok(p) => {
                    let _ = self.plan.try_set(Some(p));
                    if let Some(cb) = then {
                        cb.run(());
                    }
                }
                Err(e) => {
                    let _ = self.error.try_set(Some(e.text()));
                }
            }
        });
    }

    /// Next: client checks of the step, then the plan; the step's server errors keep it.
    fn next(self) {
        let s = self.cur();
        if s == Step::Options {
            let client = self.detail.data.with_untracked(|d| d.as_ref().map(|d| schema_form::errors(&d.form, &self.values.get_untracked(), None, schema_form::js_pattern)).unwrap_or_default());
            let stack_bad = self.stack.with_untracked(|v| !v.is_empty() && !valid_stack_name(v));
            if !client.is_empty() || stack_bad {
                self.reveal.set(true);
                return;
            }
        }
        self.replan(Some(Callback::new(move |_| {
            let blocked = self.plan.with_untracked(|p| {
                p.as_ref().is_some_and(|p| p.errors.iter().any(|e| self.owns(s, &e.field)) || (s == Step::Network && p.port_conflicts.iter().any(|c| c.relocatable)))
            });
            if blocked {
                self.reveal.set(true);
            } else {
                self.reveal.set(false);
                let n = self.steps().len();
                self.step.update(|i| *i = (*i + 1).min(n - 1));
            }
        })));
    }

    fn install(self) {
        self.planning.set(true);
        self.error.set(None);
        let body = self.request();
        let url = format!("/api/v1/apps/catalog/{}/install", api::encode(&self.id.get_value()));
        spawn_local(async move {
            let r = api::post::<_, AppInstallStarted>(&url, Some(&body)).await;
            let _ = self.planning.try_set(false);
            match r {
                Ok(s) => {
                    crate::tasks::tasks().reload();
                    let _ = self.started.try_set(Some(s));
                }
                Err(e) => {
                    let _ = self.error.try_set(Some(e.text()));
                    // the refusal may name a field: plan again to show it in place
                    self.replan(None);
                }
            }
        });
    }
}

pub fn wizard(id: String) -> AnyView {
    let a = apps();
    let w = W {
        id: StoredValue::new(id.clone()),
        detail: Remote::new(format!("/api/v1/apps/catalog/{}", api::encode(&id))),
        settings: Remote::new("/api/v1/docker/settings"),
        shares_list: Remote::new("/api/v1/shares"),
        step: RwSignal::new(0),
        stack: RwSignal::new(id.clone()),
        values: RwSignal::new(Values::new()),
        shares: RwSignal::new(BTreeMap::new()),
        folders: RwSignal::new(BTreeMap::new()),
        ports: RwSignal::new(BTreeMap::new()),
        publish: RwSignal::new(None),
        plan: RwSignal::new(None),
        planning: RwSignal::new(false),
        error: RwSignal::new(None),
        reveal: RwSignal::new(false),
        started: RwSignal::new(None),
        done: RwSignal::new(None),
        ask_close: RwSignal::new(false),
    };
    // the form's defaults once the entry arrived
    Effect::new(move |init: Option<bool>| {
        if init == Some(true) {
            return true;
        }
        let Some(defs) = w.detail.data.with(|d| d.as_ref().map(|d| schema_form::defaults(&d.form))) else { return false };
        w.values.set(defs);
        true
    });
    let name = Memo::new(move |_| w.detail.data.with(|d| d.as_ref().map(|d| d.card.name.clone())).unwrap_or_default());
    let title = Signal::derive(move || tf("apps.w.title", &[("name", &name.get())]));
    let has_secrets = move || w.started.with(|s| s.as_ref().is_some_and(|s| !s.secrets.is_empty()));
    let close = Callback::new(move |_| {
        if has_secrets() && !w.ask_close.get_untracked() {
            w.ask_close.set(true);
        } else {
            a.wizard.set(None);
        }
    });
    let body = move || wizard_body(w);
    let foot = move || wizard_foot(w, a, close);
    ui::dialog(DlgKind::Wide, title, close, view! { {body} }.into_any(), Some(view! { {foot} }.into_any()))
}

/// The wizard's steps and the current one (a function of its own: smaller wasm than a
/// closure inlined in the dialog).
fn wizard_body(w: W) -> AnyView {
    if w.started.with(Option::is_some) {
        return running(w);
    }
    let Some(_) = w.detail.data.with(|d| d.as_ref().map(|_| ())) else {
        return w.detail.view(8, || ().into_any());
    };
    let cur = w.cur();
    view! {
        <ol class="stepper app-steps" aria-label=tr("wiz.steps")>
            {w.steps().into_iter().enumerate().map(|(i, s)| view! {
                <li class=move || { let c = w.step.get(); if c == i { "current" } else if c > i { "done" } else { "" } }
                    aria-current=move || (w.step.get() == i).then_some("step")>
                    <span class="step-num">{move || if w.step.get() > i { view! { <Icon icon=icons::CHECK size=12 /> }.into_any() } else { (i + 1).to_string().into_any() }}</span>
                    <span class="step-label">{tr(step_key(s))}</span>
                </li>
            }).collect_view()}
        </ol>
        <div class="wizard app-wizard">
            {match cur {
                Step::Options => options(w),
                Step::Folders => folders(w),
                Step::Network => network(w),
                Step::Access => access(w),
                Step::Review => review(w),
            }}
        </div>
    }
    .into_any()
}

/// The wizard's buttons: Back/Next/Install, or Close/Manage once the install started.
fn wizard_foot(w: W, a: super::Apps, close: Callback<()>) -> AnyView {
    if w.started.with(Option::is_some) {
        let stack = w.started.with(|s| s.as_ref().map(|s| s.stack.clone())).unwrap_or_default();
        let finished = w.done.with(Option::is_some);
        return view! {
            {move || w.ask_close.get().then(|| view! { <span class="small foot-note warn-text">{tr("apps.w.close_secrets")}</span> })}
            <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr(if w.ask_close.get() { "apps.w.close_anyway" } else { "close" })}</button>
            {finished.then(|| view! {
                <button type="button" class="btn primary" on:click=move |_| { a.wizard.set(None); a.tab.set(3); a.manage.set(Some(stack.clone())); }>{tr("apps.manage")}</button>
            })}
        }
        .into_any();
    }
    let last = w.step.get() + 1 == w.steps().len();
    let first = w.step.get() == 0;
    let ok = w.plan.with(|p| p.as_ref().is_some_and(|p| p.ok));
    view! {
        {form_error(w.error)}
        <button type="button" class="btn ghost" on:click=move |_| if first { a.wizard.set(None) } else { w.reveal.set(false); w.step.update(|i| *i -= 1) }>
            {tr(if first { "cancel" } else { "back" })}
        </button>
        {if last {
            view! {
                <button type="button" class="btn primary" disabled=move || w.planning.get() || !ok || !a.can_install() on:click=move |_| w.install()>
                    {spinner(w.planning)}<Icon icon=icons::DOWNLOAD size=16 />{tr("apps.install")}
                </button>
            }
            .into_any()
        } else {
            view! {
                <button type="button" class="btn primary" disabled=move || w.planning.get() || w.detail.data.with(Option::is_none) on:click=move |_| w.next()>
                    {spinner(w.planning)}{tr("wiz.next")}
                </button>
            }
            .into_any()
        }}
    }
    .into_any()
}

// ------------------------------------------------------------------ steps

fn options(w: W) -> AnyView {
    let Some(d) = w.detail.data.get_untracked() else { return ().into_any() };
    let server = Signal::derive(move || w.server_errors());
    let stack_err = Signal::derive(move || {
        if let Some(e) = server.with(|s| s.get("stack").cloned()) {
            return Some(e);
        }
        w.stack.with(|v| (!v.is_empty() && !valid_stack_name(v)).then(|| t("dk.ed.name_invalid").to_string()))
    });
    let form = SchemaForm {
        form: StoredValue::new(d.form.clone()),
        overlays: StoredValue::new(d.overlays.clone()),
        values: w.values,
        server,
        reveal: w.reveal.into(),
        only: None,
    };
    view! {
        <div class="form">
            {text_field("dk.ed.name", w.stack.into(), Callback::new(move |v: String| { w.stack.set(v.trim().to_lowercase()); w.plan.set(None); }), stack_err,
                In { mono: true, help: Some("apps.w.stack_help"), maxlength: 63, ..In::default() })}
        </div>
        {form.view()}
    }
    .into_any()
}

/// `<vol path>/docker/appdata/<stack>/<path>`.
fn placement(vol_path: &str, stack: &str, path: &str) -> String {
    format!("{}/docker/appdata/{stack}/{path}", vol_path.trim_end_matches('/'))
}

fn folders(w: W) -> AnyView {
    let Some(d) = w.detail.data.get_untracked() else { return ().into_any() };
    let server = Signal::derive(move || w.server_errors());
    let stacks_dir = apps().cat.data.with_untracked(|c| c.as_ref().and_then(|c| c.stacks_dir.clone())).unwrap_or_default();
    let stack_dir = Signal::derive(move || format!("{stacks_dir}/{}", w.stack.get()));
    let folder_rows = d
        .folders
        .iter()
        .map(|f| {
            let env = StoredValue::new(f.env.clone());
            let path = StoredValue::new(f.path.clone());
            let suggested = Memo::new(move |_| w.plan.with(|p| p.as_ref().and_then(|p| p.folders.iter().find(|x| x.env == env.get_value()).and_then(|x| x.suggested.clone()))));
            let hint = (f.intent == FolderIntent::Db || f.ssd == FolderSsd::Preferred).then_some("apps.w.ssd_hint");
            let options = move || {
                let stack = w.stack.get();
                let cur = w.folders.with(|m| m.get(&env.get_value()).cloned()).unwrap_or_default();
                let sug = suggested.get();
                let vols = w.settings.data.with(|s| s.as_ref().map(|s| s.volumes.clone()).unwrap_or_default());
                let mut v = vec![view! { <option value="" selected=cur.is_empty()>{tf("apps.w.in_stack", &[("path", &format!("{}/{}", stack_dir.get(), path.get_value()))])}</option> }.into_any()];
                for vol in vols {
                    let p = placement(&vol.path, &stack, &path.get_value());
                    let label = if sug.as_deref() == Some(p.as_str()) { tf("apps.w.on_volume_suggested", &[("vol", &vol.label), ("path", &p)]) } else { tf("apps.w.on_volume", &[("vol", &vol.label), ("path", &p)]) };
                    let sel = cur == p;
                    v.push(view! { <option value=p selected=sel>{label}</option> }.into_any());
                }
                v
            };
            let fid = crate::form::fid();
            let err = Signal::derive(move || server.with(|s| s.get(&env.get_value()).cloned()));
            view! {
                <div class="field app-folder">
                    <label class="field-label" for=fid.clone()>
                        <span class="mono">{format!("./{}", f.path)}</span>" "
                        <span class="chip">{intent_text(f.intent)}</span>
                        {ssd_key(f.ssd).map(|k| view! { " "<span class="chip accent">{tr(k)}</span> })}
                    </label>
                    <select id=fid class="input" on:change=move |ev| { let v = event_target_value(&ev); w.folders.update(|m| { m.insert(env.get_value(), v); }); }>{options}</select>
                    <span class="field-help" lang="en">{f.description.clone()}</span>
                    {hint.map(|k| view! { <span class="field-help app-hint"><Icon icon=icons::INFO size=12 />{tr(k)}</span> })}
                    <span class="field-error" aria-live="polite">{move || err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{ui::err_view(e)} })}</span>
                </div>
            }
            .into_any()
        })
        .collect_view();
    let share_rows = d
        .shares
        .iter()
        .map(|s| {
            let env = StoredValue::new(s.env.clone());
            let required = s.required;
            let fallback = s.fallback.clone();
            let options = move || {
                let cur = w.shares.with(|m| m.get(&env.get_value()).cloned()).unwrap_or_default();
                let list = w.shares_list.data.with(|l| l.as_ref().map(|l| l.iter().map(|s| (s.share.name.clone(), s.share.path.clone())).collect::<Vec<_>>()).unwrap_or_default());
                let mut v: Vec<AnyView> = Vec::new();
                let empty_label = match (&fallback, required) {
                    (_, true) => t("snap.f.pick_share").to_string(),
                    (Some(f), false) => tf("apps.w.no_share_fallback", &[("path", &format!("./{f}"))]),
                    (None, false) => t("apps.w.no_share").to_string(),
                };
                v.push(view! { <option value="" selected=cur.is_empty() disabled=required>{empty_label}</option> }.into_any());
                for (n, p) in list {
                    let sel = cur == p;
                    v.push(view! { <option value=p.clone() selected=sel>{format!("{n} ({p})")}</option> }.into_any());
                }
                v
            };
            let fid = crate::form::fid();
            let err = Signal::derive(move || server.with(|s| s.get(&env.get_value()).cloned()));
            view! {
                <div class="field">
                    <label class="field-label" for=fid.clone()>
                        <span lang="en">{s.label.clone()}</span>" "
                        <span class="chip">{tr(if s.access == ShareAccess::Rw { "rm.set.rw" } else { "perm.ro" })}</span>
                        {(!required).then(|| view! { " "<span class="muted small">{tr("sf.optional")}</span> })}
                    </label>
                    <select id=fid class="input" on:change=move |ev| { let v = event_target_value(&ev); w.shares.update(|m| { m.insert(env.get_value(), v); }); }>{options}</select>
                    <span class="field-help" lang="en">{s.description.clone()}</span>
                    <span class="field-error" aria-live="polite">{move || err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{ui::err_view(e)} })}</span>
                </div>
            }
            .into_any()
        })
        .collect_view();
    let no_shares = Memo::new(move |_| w.shares_list.data.with(|l| l.as_ref().is_some_and(Vec::is_empty)));
    let has_shares = !d.shares.is_empty();
    view! {
        {ui::note(Tone::Info, icons::FOLDER, view! { {move || tf("apps.w.stack_dir", &[("path", &stack_dir.get())])} }.into_any())}
        {if d.folders.is_empty() && d.shares.is_empty() {
            view! { <p class="muted small">{tr("apps.w.no_folders")}</p> }.into_any()
        } else {
            view! {
                {(!d.folders.is_empty()).then(|| view! { <fieldset class="sf-group"><legend class="mini-title">{tr("apps.d.folders")}</legend><p class="small muted">{tr("apps.w.folders_text")}</p><div class="form">{folder_rows}</div></fieldset> })}
                {has_shares.then(|| view! {
                    <fieldset class="sf-group">
                        <legend class="mini-title">{tr("apps.d.shares")}</legend>
                        <p class="small muted">{tr("apps.w.shares_text")}</p>
                        {move || no_shares.get().then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("apps.w.no_shares_yet")}" "<a href="/shares">{tr("apps.w.to_shares")}</a> }.into_any()))}
                        <div class="form">{share_rows}</div>
                    </fieldset>
                })}
            }
            .into_any()
        }}
    }
    .into_any()
}

fn network(w: W) -> AnyView {
    let host_network = w.detail.data.with_untracked(|d| d.as_ref().is_some_and(|d| d.host_network));
    let server = Signal::derive(move || w.server_errors());
    let content = move || {
        let Some(p) = w.plan.get() else { return ui::loading(Signal::stored(None), Callback::new(move |_| w.replan(None)), 4) };
        let rows = p
            .ports
            .iter()
            .map(|pp| {
                let env = StoredValue::new(pp.env.clone());
                let relocatable = w.detail.data.with_untracked(|d| d.as_ref().and_then(|d| d.card.ports.iter().find(|x| x.env == pp.env).map(|x| x.relocatable))).unwrap_or(false) && !host_network;
                let conflict = p.port_conflicts.iter().find(|c| c.env == pp.env).cloned();
                let port_cell = if relocatable {
                    let val = Signal::derive(move || w.ports.with(|m| m.get(&env.get_value()).copied()).map(|v| v.to_string()).unwrap_or_else(|| w.plan.with_untracked(|p| p.as_ref().and_then(|p| p.ports.iter().find(|x| x.env == env.get_value()).map(|x| x.port.to_string()))).unwrap_or_default()));
                    let label = format!("{}: {}", t("apps.col.port"), pp.name);
                    view! {
                        <input class="input tnum app-port" type="number" min="1" max="65535" inputmode="numeric" aria-label=label prop:value=val
                            on:change=move |ev| {
                                if let Ok(v) = event_target_value(&ev).trim().parse::<u16>() {
                                    w.ports.update(|m| { m.insert(env.get_value(), v); });
                                    w.replan(None);
                                }
                            } />
                    }
                    .into_any()
                } else {
                    view! { <span class="tnum">{pp.port}</span>" "<span class="chip">{tr("apps.port.fixed")}</span> }.into_any()
                };
                let err = Signal::derive(move || server.with(|s| s.get(&env.get_value()).cloned()));
                let conflict_view = conflict.map(|c| {
                    let fix = c.suggestion.filter(|_| c.relocatable).map(|s| view! {
                        <button type="button" class="btn ghost sm" on:click=move |_| { w.ports.update(|m| { m.insert(env.get_value(), s); }); w.replan(None); }>{tf("apps.w.use_port", &[("port", &s.to_string())])}</button>
                    });
                    view! {
                        <div class="app-conflict">
                            {ui::status(Tone::Err, tf("apps.w.port_used", &[("port", &c.port.to_string())]))}
                            {fix}
                        </div>
                    }
                });
                let proto = if pp.protocol == rynas_api_types::apps::PortProtocol::Udp { "udp" } else { "tcp" };
                let reach = match pp.bind.as_deref() {
                    Some("127.0.0.1") => t("apps.bind.proxy").to_string(),
                    Some(_) => t("apps.bind.lan").to_string(),
                    None => t("apps.host_network").to_string(),
                };
                ui::row(vec![
                    ui::cell("apps.col.purpose", "wrap", view! { <div class="cell-main"><span lang="en">{pp.purpose.clone()}</span><span class="cell-sub mono">{format!("{} · {proto}", pp.env)}</span></div> }.into_any()),
                    ui::cell("apps.col.port", "", view! {
                        <div class="cell-main">
                            <div class="chips">{port_cell}</div>
                            {conflict_view}
                            <span class="field-error" aria-live="polite">{move || err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{ui::err_view(e)} })}</span>
                        </div>
                    }.into_any()),
                    ui::cell("apps.col.reach", "", ui::text(reach)),
                ])
            })
            .collect::<Vec<_>>();
        view! {
            {if host_network { Some(ui::note(Tone::Info, icons::INFO, ui::text(t("apps.w.host_network").to_string()))) } else { None }}
            {if rows.is_empty() { view! { <p class="muted small">{tr("apps.w.no_ports")}</p> }.into_any() } else { ui::table("app-ports", &["apps.col.purpose", "apps.col.port", "apps.col.reach"], rows) }}
            <div class="sec-actions">
                <button type="button" class="btn ghost sm" disabled=move || w.planning.get() on:click=move |_| w.replan(None)>{spinner(w.planning)}<Icon icon=icons::REFRESH_CW size=14 />{tr("apps.w.check_again")}</button>
            </div>
        }
        .into_any()
    };
    view! { {content} }.into_any()
}

/// Access: through web access (loopback + a route, HTTPS at its own name) or directly on
/// the LAN port. Unset follows the server's default (web access when it is active, §7).
fn access(w: W) -> AnyView {
    let content = move || {
        let Some(p) = w.plan.get() else { return ui::loading(Signal::stored(None), Callback::new(move |_| w.replan(None)), 4) };
        let proxy_active = p.proxy_active;
        let web_port = p.ports.iter().find(|x| x.proxied).map(|x| x.port);
        let lan_url = web_port.map(|port| super::host_url(port, "/")).unwrap_or_default();
        let lan = Signal::derive(move || w.publish.get().unwrap_or(!proxy_active));
        if !proxy_active {
            return ui::note(Tone::Info, icons::NETWORK, view! {
                <span>{tf("apps.w.access_inactive", &[("url", &lan_url)])}" "<a href="/network/web-access">{tr("apps.w.access_enable")}</a></span>
            }.into_any());
        }
        let opt = move |on_lan: bool, label: &'static str, help: String, url: String| {
            view! {
                <label class="net-radio">
                    <input type="radio" name="app-access" prop:checked=move || lan.get() == on_lan on:change=move |_| { w.publish.set(Some(on_lan)); w.replan(None); } />
                    <span class="switch-text">
                        <span class="switch-label">{tr(label)}</span>
                        <span class="field-help">{help}</span>
                        <span class="mono small app-url">{url}</span>
                    </span>
                </label>
            }
        };
        let future = p.web_url.clone().or_else(|| super::proxy_url(&p.stack, &apps().proxy_host.get_untracked())).unwrap_or_else(|| t("apps.w.access_url_pending").to_string());
        let port = web_port.map(|p| p.to_string()).unwrap_or_default();
        view! {
            <fieldset class="fieldset">
                <legend class="field-label">{tr("apps.w.access_how")}</legend>
                <div class="wa-radios">
                    {opt(false, "apps.w.access_proxy", t("apps.w.access_proxy_help").to_string(), future)}
                    {opt(true, "apps.w.access_lan", tf("apps.w.access_lan_help", &[("port", &port)]), lan_url)}
                </div>
            </fieldset>
            {move || if lan.get() {
                ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("apps.warn.published_on_lan").to_string()))
            } else {
                view! { <p class="small muted">{tr("apps.w.access_level")}" "<a href="/network/web-access">{tr("wa.title")}</a></p> }.into_any()
            }}
        }
        .into_any()
    };
    view! { {content} }.into_any()
}

fn review(w: W) -> AnyView {
    let body = move || {
        let Some(p) = w.plan.get() else { return ui::loading(Signal::stored(None), Callback::new(move |_| w.replan(None)), 6) };
        let create: Vec<String> = p.folders.iter().filter(|f| !f.share && !f.exists).map(|f| f.path.clone()).collect();
        let existing: Vec<String> = p.folders.iter().filter(|f| !f.share && f.exists).map(|f| f.path.clone()).collect();
        let shares: Vec<String> = p.folders.iter().filter(|f| f.share).map(|f| f.path.clone()).collect();
        let errors = p
            .errors
            .iter()
            .map(|e| view! { <li><span class="mono">{e.field.clone()}</span>" — "{ui::err_view(format!("{}{DETAIL_SEP}{}", t(kind_key(e.kind)), e.message))}</li> })
            .collect_view();
        let has_errors = !p.errors.is_empty();
        let list = |items: Vec<String>| view! { <ul class="small app-list mono">{items.into_iter().map(|i| view! { <li>{i}</li> }).collect_view()}</ul> };
        let ports = p.ports.iter().map(|x| format!("{} → {} ({})", x.port, x.purpose, x.bind.as_deref().unwrap_or("host"))).collect::<Vec<_>>();
        view! {
            {has_errors.then(|| ui::note(Tone::Err, icons::CIRCLE_X, view! { {tr("apps.w.errors")}<ul class="small app-list">{errors}</ul> }.into_any()))}
            {p.port_conflicts.iter().filter(|c| !c.relocatable).map(|c| ui::note(Tone::Err, icons::CIRCLE_X, ui::text(tf("apps.w.port_fixed_used", &[("port", &c.port.to_string())])))).collect_view()}
            {p.warnings.iter().map(|w| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t(warning_key(*w)).to_string()))).collect_view()}
            {(!matches!(p.ram, rynas_api_types::apps::RamGate::Ok)).then(|| view! { <div class="chips">{ram_pill(p.ram)}</div> })}
            {notes_view(&p.notes)}
            <dl class="kvs">
                {ui::kv_row("dk.ed.name", Signal::stored(p.stack.clone()), "mono")}
                {ui::kv_row("apps.w.folder", Signal::stored(p.stack_dir.clone()), "mono")}
                {ui::kv_row("apps.w.version", Signal::stored(p.version.clone()), "mono")}
                {p.web_url.clone().map(|u| ui::kv_row("apps.w.address", Signal::stored(u), "mono"))}
            </dl>
            {(!create.is_empty()).then(|| view! { <h4 class="mini-title">{tr("apps.w.create")}</h4>{list(create)} })}
            {(!existing.is_empty()).then(|| view! { <h4 class="mini-title">{tr("apps.w.existing")}</h4>{list(existing)} })}
            {(!shares.is_empty()).then(|| view! { <h4 class="mini-title">{tr("apps.w.shares_used")}</h4>{list(shares)} })}
            {(!ports.is_empty()).then(|| view! { <h4 class="mini-title">{tr("apps.d.ports")}</h4>{list(ports)} })}
            {(!p.secrets.is_empty()).then(|| view! {
                <h4 class="mini-title">{tr("apps.d.secrets")}</h4>
                <ul class="small app-list">{p.secrets.iter().map(|s| view! {
                    <li><span class="mono">{s.env.clone()}</span>" ••••••••  "<span lang="en">{s.description.clone()}</span>{s.show.then(|| view! { " "<span class="chip">{tr("apps.secret.shown")}</span> })}</li>
                }).collect_view()}</ul>
            })}
            {(!p.images.is_empty()).then(|| view! { <h4 class="mini-title">{tr("dk.col.images")}</h4>{list(p.images.clone())} })}
            {(!p.compose.is_empty()).then(|| view! {
                <details class="app-preview" open>
                    <summary>{tr("apps.w.compose")}</summary>
                    <pre class="app-code" tabindex="0" aria-label=tr("apps.w.compose")>{p.compose.clone()}</pre>
                </details>
                <details class="app-preview">
                    <summary>{tr("apps.w.env")}</summary>
                    <pre class="app-code" tabindex="0" aria-label=tr("apps.w.env")>{p.env.clone()}</pre>
                </details>
            })}
        }
        .into_any()
    };
    view! { {body} }.into_any()
}

// ------------------------------------------------------------------ install

fn running(w: W) -> AnyView {
    let Some(s) = w.started.get_untracked() else { return ().into_any() };
    let secrets = s.secrets.clone();
    let a = apps();
    let on_done = Callback::new(move |tk: rynas_api_types::TaskInfo| {
        let _ = w.done.try_set(Some(tk.status));
        a.reload();
    });
    view! {
        <div class="wizard app-wizard">
            <p>{tf("apps.w.installing", &[("stack", &s.stack), ("path", &s.stack_dir)])}</p>
            {(!secrets.is_empty()).then(|| view! {
                <section class="app-secrets" aria-labelledby="app-secrets-t">
                    <h4 class="mini-title" id="app-secrets-t"><Icon icon=icons::KEY size=14 />{tr("apps.w.secrets")}</h4>
                    {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("apps.w.save_now").to_string()))}
                    {secrets.into_iter().map(|x| view! {
                        <div class="app-secret">
                            <div class="cell-main">
                                <span lang="en">{x.description.clone()}</span>
                                <span class="cell-sub mono">{x.env.clone()}</span>
                                <code class="app-secret-value">{x.value.clone()}</code>
                            </div>
                            {crate::pages::security::copy_button("apps.w.copy", x.value.clone())}
                        </div>
                    }).collect_view()}
                </section>
            })}
            <TaskRun id=s.task_id.clone() on_done=on_done />
            {move || w.done.get().map(|st| if st == TaskStatus::Ok {
                ui::note(Tone::Ok, icons::CIRCLE_CHECK, ui::text(t("apps.w.installed").to_string()))
            } else {
                ui::note(Tone::Err, icons::CIRCLE_X, ui::text(t("apps.w.failed").to_string()))
            })}
        </div>
    }
    .into_any()
}
