//! Users & Groups › User homes: the user homes setting (default volume for new users, SMB
//! home folders with their recycle bin), where each local user's home is, and the explicit
//! home move to any volume or to the system disk (plan with what blocks it, task, then
//! confirm or undo). Saving applies the `homes` and `shares` keys together.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{
    HOME_SYSTEM, HOMES_LINK, HomeLocation, HomeMoveBlocker, HomeMoveBlockerCode as B, HomeMovePlan, HomeMoveRecord, HomeMoveRequest, HomesSettings, HomesState,
    TaskStarted, TaskStatus, UserHome,
};

use crate::api;
use crate::config::keys;
use crate::form::{Draft, form_error, lens_bool, spinner, stale_notice, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::{saved_applied, toast};
use crate::ui::{self, Confirm, DlgKind, Remote, Skeleton, Tone, confirm, dialog};

/// The tab's content.
pub fn homes_panel(state: Remote<HomesState>) -> AnyView {
    view! {
        {move || state.gate(6).unwrap_or_else(|| {
            // built once: the draft follows the server's settings (form::Draft)
            let s = state.data.get_untracked().unwrap_or_default();
            view! {
                {settings_form(s.settings, state)}
                {homes_list(state)}
            }
            .into_any()
        })}
    }
    .into_any()
}

fn settings_form(init: HomesSettings, state: Remote<HomesState>) -> AnyView {
    let draft = Draft::new(init, move || state.data.with(|s| s.as_ref().map(|s| s.settings.clone())));
    let d = draft.d;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (enabled, set_enabled) = lens_bool(d, |s| s.enabled, |s, v| s.enabled = v);
    let (smb, set_smb) = lens_bool(d, |s| s.smb.enabled, |s, v| s.smb.enabled = v);
    let (rec, set_rec) = lens_bool(d, |s| s.smb.recycle.enabled, |s, v| s.smb.recycle.enabled = v);
    let volumes = move || state.data.with(|s| s.as_ref().map(|s| s.volumes.clone()).unwrap_or_default());
    // the first volume once the service is turned on without one
    Effect::new(move |_| {
        if enabled.get()
            && d.with_untracked(|s| s.volume.is_empty())
            && let Some(v) = volumes().first()
        {
            let p = v.path.clone();
            d.update(|s| s.volume = p);
        }
    });
    let dirty = draft.dirty();
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
    });
    let save = move || {
        busy.set(true);
        error.set(None);
        let body = d.get_untracked();
        spawn_local(async move {
            match api::put::<_, HomesState>("/api/v1/services/homes", &body).await {
                Ok(_) => {
                    if crate::config::apply_now(&[keys::HOMES, keys::SHARES], error).await {
                        saved_applied();
                        state.reload();
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    };
    let vid = crate::form::fid();
    let volume_field = move || {
        let vols = volumes();
        if vols.is_empty() {
            return view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("homes.no_volumes")}</p> }.into_any();
        }
        let id = vid.clone();
        view! {
            <div class="field">
                <label class="field-label" for=id.clone()>{tr("homes.volume")}</label>
                <select id=id class="input" on:change=move |ev| {
                    let v = event_target_value(&ev);
                    d.update(|s| s.volume = v);
                }>
                    {vols.into_iter().map(|v| {
                        let p2 = v.path.clone();
                        let text = tf("homes.volume_opt", &[("path", &v.path), ("fs", &v.fs_type), ("free", &i18n::bytes(v.avail_bytes))]);
                        view! { <option value=v.path.clone() selected=move || d.with(|s| s.volume == p2)>{text}</option> }
                    }).collect_view()}
                </select>
                <span class="field-help">{move || {
                    let v = d.with(|s| s.volume.clone());
                    tf("homes.volume_help", &[("path", &format!("{}/homes", v.trim_end_matches('/')))])
                }}</span>
            </div>
        }
        .into_any()
    };
    view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save(); }>
            <p class="field-help">{tr("homes.intro")}</p>
            {switch("homes.enabled", Some("homes.enabled_help"), enabled, set_enabled)}
            <div class="form" hidden=move || !enabled.get()>
                {volume_field}
                <dl class="kvs">
                    {ui::kv_row("homes.path", Signal::stored(format!("{HOMES_LINK}/<user>")), "mono")}
                </dl>
                <p class="field-help">{tr("homes.path_help")}</p>
                {switch("homes.smb", Some("homes.smb_help"), smb, set_smb)}
                <div class="form" hidden=move || !smb.get()>
                    {switch("homes.recycle", Some("homes.recycle_help"), rec, set_rec)}
                </div>
            </div>
            <p class="sec-note" hidden=move || enabled.get()><Icon icon=icons::INFO size=16 />{tr("homes.off_note")}</p>
            {stale_notice(draft.stale, reset)}
            {form_error(error)}
            <div class="sec-actions">
                <button type="submit" class="btn primary" disabled=move || busy.get() || !dirty.get()>{spinner(busy)}{tr("save")}</button>
                <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
            </div>
        </form>
    }
    .into_any()
}

fn location_key(l: HomeLocation) -> (&'static str, Tone) {
    match l {
        HomeLocation::Volume => ("homes.loc_volume", Tone::Ok),
        HomeLocation::System => ("homes.loc_system", Tone::Unknown),
        HomeLocation::Other => ("homes.loc_other", Tone::Unknown),
        HomeLocation::Missing => ("homes.loc_missing", Tone::Warn),
    }
}

/// Where a home is, as a destination value: a volume's mount point, `system`, or empty
/// (another folder, a missing one).
pub fn place_of(h: &UserHome) -> String {
    match h.location {
        HomeLocation::Volume => h.volume.clone(),
        HomeLocation::System => HOME_SYSTEM.to_string(),
        HomeLocation::Missing if !h.volume.is_empty() => h.volume.clone(),
        _ => String::new(),
    }
}

/// The places a home can go: the volumes while the service is applied, and the system disk.
/// `(value, text)`.
pub fn places(s: &HomesState) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = if s.active {
        s.volumes
            .iter()
            .map(|v| (v.path.clone(), tf("homes.volume_opt", &[("path", &v.path), ("fs", &v.fs_type), ("free", &i18n::bytes(v.avail_bytes))])))
            .collect()
    } else {
        Vec::new()
    };
    v.push((HOME_SYSTEM.to_string(), t("homes.dest_system").to_string()));
    v
}

/// A labelled select over `(value, text)` options.
pub fn place_select(label: &'static str, help: Option<&'static str>, opts: Vec<(String, String)>, value: Signal<String>, set: Callback<String>) -> AnyView {
    let id = crate::form::fid();
    let hid = help.map(|_| format!("{id}h"));
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr(label)}</label>
            <select id=id class="input" aria-describedby=hid.clone() on:change=move |ev| set.run(event_target_value(&ev))>
                {opts
                    .into_iter()
                    .map(|(v, text)| {
                        let v2 = v.clone();
                        view! { <option value=v selected=move || value.with(|x| *x == v2)>{text}</option> }
                    })
                    .collect_view()}
            </select>
            {help.zip(hid).map(|(h, id)| view! { <span class="field-help" id=id>{tr(h)}</span> })}
        </div>
    }
    .into_any()
}

/// The destination offered first for a home now at `here`: the default volume, else the
/// system disk, else the first other place.
fn suggested(s: &HomesState, here: &str) -> String {
    let opts = places(s);
    let default = s.settings.volume.clone();
    if s.active && !default.is_empty() && default != here && opts.iter().any(|(v, _)| *v == default) {
        return default;
    }
    if here != HOME_SYSTEM {
        return HOME_SYSTEM.to_string();
    }
    opts.into_iter().map(|(v, _)| v).find(|v| v != here).unwrap_or_default()
}

/// Move dialog request: users (empty: every home that can move) and the destination.
#[derive(Clone)]
pub struct MoveAsk {
    pub users: Vec<String>,
    pub to: String,
}

fn homes_list(state: Remote<HomesState>) -> AnyView {
    let mv = RwSignal::new(None::<MoveAsk>);
    let fin = RwSignal::new(None::<HomeMoveRecord>);
    let undo = RwSignal::new(None::<HomeMoveRecord>);
    let active = move || state.data.with(|s| s.as_ref().is_some_and(|s| s.active && !s.pending));
    let rows = move || {
        let Some(s) = state.data.get() else {
            return view! { <p class="muted table-none">{tr("users.empty")}</p> }.into_any();
        };
        if s.homes.is_empty() {
            return view! { <p class="muted table-none">{tr("users.empty")}</p> }.into_any();
        }
        ui::table(
            "",
            &["users.name", "homes.location", "users.home", ""],
            s.homes
                .iter()
                .map(|h| {
                    let (key, tone) = location_key(h.location);
                    let name = h.user.clone();
                    let rec = h.moved.clone();
                    let rec2 = h.moved.clone();
                    let to = suggested(&s, &place_of(h));
                    let can = h.moved.is_none() && h.location != HomeLocation::Missing && !to.is_empty();
                    let n_label = format!("{} {}", t("homes.move"), h.user);
                    let place = match h.location {
                        HomeLocation::Volume | HomeLocation::Missing if !h.volume.is_empty() => Some(h.volume.clone()),
                        _ => None,
                    };
                    let sub = (h.folder != h.home).then(|| h.folder.clone());
                    ui::row(vec![
                        ui::cell("users.name", "", view! { <span class="mono">{h.user.clone()}</span> }.into_any()),
                        ui::cell(
                            "homes.location",
                            "",
                            view! {
                                <div class="chips">
                                    {ui::status(tone, t(key).to_string())}
                                    {place.map(|p| view! { <span class="chip mono">{p}</span> })}
                                    {h.subvolume.then(|| view! { <span class="chip">{tr("shares.subvolume")}</span> })}
                                </div>
                            }
                            .into_any(),
                        ),
                        ui::cell(
                            "users.home",
                            "path-cell",
                            view! {
                                <div class="cell-main">
                                    <span class="mono">{h.home.clone()}</span>
                                    {sub.map(|f| view! { <span class="cell-sub mono">{f}</span> })}
                                    {h.moved.as_ref().map(|m| view! { <span class="cell-sub">{tf("homes.kept", &[("path", &m.kept)])}</span> })}
                                </div>
                            }
                            .into_any(),
                        ),
                        ui::actions(
                            view! {
                                {rec.map(|r| {
                                    let r2 = r.clone();
                                    view! {
                                        <button class="icon-btn sm" title=tr("homes.finish") aria-label=format!("{} {}", t("homes.finish"), r.user) on:click=move |_| fin.set(Some(r.clone()))>
                                            <Icon icon=icons::CHECK size=16 />
                                        </button>
                                        <button class="icon-btn sm" title=tr("homes.undo") aria-label=format!("{} {}", t("homes.undo"), r2.user) on:click=move |_| undo.set(Some(r2.clone()))>
                                            <Icon icon=icons::ROTATE_CCW size=16 />
                                        </button>
                                    }
                                })}
                                {rec2.is_none().then(|| view! {
                                    <button class="icon-btn sm" title=tr("homes.move") aria-label=n_label disabled=!can on:click=move |_| mv.set(Some(MoveAsk { users: vec![name.clone()], to: to.clone() }))>
                                        <Icon icon=icons::FOLDER_SYNC size=16 />
                                    </button>
                                })}
                            }
                            .into_any(),
                        ),
                    ])
                })
                .collect(),
        )
    };
    let done = Callback::new(move |_| state.reload());
    let all_to = move || state.data.with(|s| s.as_ref().map(|s| s.settings.volume.clone()).unwrap_or_default());
    view! {
        <div class="subsection">
            <div class="toolbar">
                <h3 class="form-section"><Icon icon=icons::FOLDER size=16 />{tr("homes.list")}</h3>
                <button class="btn ghost" disabled=move || !active() on:click=move |_| mv.set(Some(MoveAsk { users: Vec::new(), to: all_to() }))>
                    <Icon icon=icons::FOLDER_SYNC size=16 /><span class="btn-label">{tr("homes.move_all")}</span>
                </button>
            </div>
            {move || (!active()).then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("homes.move_needs")}</p> })}
            {rows}
        </div>
        {move || mv.get().map(|ask| move_dialog(ask, state, Callback::new(move |_| mv.set(None)), done))}
        {move || fin.get().map(|r| finish_dialog(r, Callback::new(move |_| fin.set(None)), done))}
        {move || undo.get().map(|r| undo_dialog(r, Callback::new(move |_| undo.set(None)), done))}
    }
    .into_any()
}

fn blocker_text(b: &HomeMoveBlocker) -> String {
    let key = match b.code {
        B::ServiceOff => "homes.b.service_off",
        B::NotLocal => "homes.b.not_local",
        B::AlreadyMoved => "homes.b.already_moved",
        B::SourceMissing => "homes.b.source_missing",
        B::SourceNotPlain => "homes.b.source_not_plain",
        B::TargetExists => "homes.b.target_exists",
        B::TargetInvalid => "homes.b.target_invalid",
        B::Processes => "homes.b.processes",
        B::SmbSessions => "homes.b.smb_sessions",
        B::ShareInside => "homes.b.share_inside",
        B::NoSpace => "homes.b.no_space",
        B::NoRsync => "homes.b.no_rsync",
        B::MoveWaiting => "homes.b.move_waiting",
        B::Other => "homes.b.other",
    };
    let detail = match b.code {
        B::NoSpace => b.detail.parse::<u64>().map(i18n::bytes).unwrap_or_else(|_| b.detail.clone()),
        _ => b.detail.clone(),
    };
    tf(key, &[("detail", &detail)])
}

fn size_text(bytes: u64, partial: bool) -> String {
    if partial { tf("shares.size_min", &[("size", &i18n::bytes(bytes))]) } else { i18n::bytes(bytes) }
}

/// What moving one home does, and what blocks it.
fn plan_view(p: &HomeMovePlan) -> AnyView {
    let blockers: Vec<String> = p.blockers.iter().map(blocker_text).collect();
    let kind = t(if p.subvolume { "homes.kind_subvolume" } else { "homes.kind_folder" }).to_string();
    view! {
        <h3 class="form-section mono">{p.user.clone()}</h3>
        <dl class="kvs">
            {ui::kv_row("homes.from", Signal::stored(p.from.clone()), "mono")}
            {(p.source != p.from).then(|| ui::kv_row("homes.source", Signal::stored(p.source.clone()), "mono"))}
            {ui::kv_row("homes.to", Signal::stored(p.to.clone()), "mono")}
            {(p.target != p.to).then(|| ui::kv_row("homes.target", Signal::stored(p.target.clone()), "mono"))}
            {ui::kv_row("homes.kind", Signal::stored(kind), "")}
            {ui::kv_row("homes.size", Signal::stored(size_text(p.size_bytes, p.size_partial)), "tnum")}
            {ui::kv_row("homes.avail", Signal::stored(i18n::bytes(p.avail_bytes)), "tnum")}
        </dl>
        {p.ssh_keys.then(|| ui::note(Tone::Info, icons::KEY, view! { <span>{tr("homes.ssh_note")}</span> }.into_any()))}
        {(!blockers.is_empty()).then(|| ui::note(Tone::Err, icons::CIRCLE_X, view! {
            <span>{tr("homes.blocked")}</span>
            <ul class="plain-list">{blockers.into_iter().map(|x| view! { <li>{x}</li> }).collect_view()}</ul>
        }.into_any()))}
    }
    .into_any()
}

/// Starts a task and follows it to a toast.
fn run(url: String, body: Option<String>, ok: String, done: Callback<()>) {
    spawn_local(async move {
        let r = crate::tasks::run_task(&url, body).await;
        match r {
            Ok(task) if task.status == TaskStatus::Ok => toast(Tone::Ok, ok),
            Ok(task) => toast(Tone::Err, crate::tasks::task_error(&task)),
            Err(e) if e.status() == 401 => {}
            Err(e) => toast(Tone::Err, e.text()),
        }
        done.run(());
    });
}

/// The destination picker, the plan of the move of `ask.users` (empty: every home that can
/// move) to it, then the task. Also opened from the user editor.
pub fn move_dialog(ask: MoveAsk, state: Remote<HomesState>, close: Callback<()>, done: Callback<()>) -> AnyView {
    let users = ask.users.clone();
    let to = RwSignal::new(ask.to.clone());
    let plans = RwSignal::new(None::<Result<Vec<HomeMovePlan>, String>>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // the plan follows the destination; an answer for an older choice is dropped
    let seq = StoredValue::new(0u32);
    let u2 = users.clone();
    Effect::new(move |_| {
        let req = HomeMoveRequest { users: u2.clone(), to: to.get() };
        let n = seq.get_value() + 1;
        seq.set_value(n);
        plans.set(None);
        spawn_local(async move {
            let r = api::post::<_, Vec<HomeMovePlan>>("/api/v1/services/homes/move/plan", Some(&req)).await;
            if seq.try_get_value() == Some(n) {
                let _ = plans.try_set(Some(r.map_err(|e| e.text())));
            }
        });
    });
    let any_allowed = move || plans.with(|p| matches!(p, Some(Ok(v)) if v.iter().any(|x| x.allowed)));
    let u3 = users.clone();
    let go = move || {
        busy.set(true);
        error.set(None);
        let b = HomeMoveRequest { users: u3.clone(), to: to.get_untracked() };
        spawn_local(async move {
            // the move can take long: the dialog closes, the task panel shows the progress
            match api::post::<_, TaskStarted>("/api/v1/services/homes/move", Some(&b)).await {
                Ok(st) => {
                    close.run(());
                    let r = crate::tasks::follow(&st.task_id).await;
                    match r {
                        Ok(task) if task.status == TaskStatus::Ok => toast(Tone::Ok, t("homes.moved").to_string()),
                        Ok(task) => toast(Tone::Err, crate::tasks::task_error(&task)),
                        Err(e) => toast(Tone::Err, e.text()),
                    }
                    done.run(());
                }
                Err(e) => {
                    let _ = busy.try_set(false);
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let title = match users.as_slice() {
        [one] => tf("homes.move_title_one", &[("name", one)]),
        _ => t("homes.move_title_all").to_string(),
    };
    let opts = state.data.with_untracked(|s| s.as_ref().map(places).unwrap_or_default());
    let body = view! {
        <p class="dialog-text">{tr("homes.move_text")}</p>
        {place_select("homes.dest", Some("homes.dest_help"), opts, to.into(), Callback::new(move |v| to.set(v)))}
        {move || match plans.get() {
            None => view! { <Skeleton lines=6 /> }.into_any(),
            Some(Err(e)) => ui::note(Tone::Err, icons::CIRCLE_X, view! { <span>{e}</span> }.into_any()),
            Some(Ok(v)) if v.is_empty() => view! { <p class="muted">{tr("homes.none")}</p> }.into_any(),
            Some(Ok(v)) => view! {
                {v.iter().map(plan_view).collect_view()}
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <span>{tr("homes.move_steps")}</span> }.into_any())}
            }
            .into_any(),
        }}
        {form_error(error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" data-autofocus="" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() || !any_allowed() on:click=move |_| go()>
            {spinner(busy)}
            {tr("homes.move_btn")}
        </button>
    }
    .into_any();
    dialog(DlgKind::Wide, Signal::stored(title), close, body, Some(foot))
}

fn finish_dialog(r: HomeMoveRecord, close: Callback<()>, done: Callback<()>) -> AnyView {
    let user = r.user.clone();
    let on_confirm = Callback::new(move |_| {
        close.run(());
        run(
            format!("/api/v1/services/homes/move/{}/finish", api::encode(&user)),
            None,
            tf("homes.finished", &[("name", &user)]),
            done,
        );
    });
    confirm(Confirm::new(
        tf("homes.finish_title", &[("name", &r.user)]),
        tf("homes.finish_text", &[("path", &r.kept)]),
        "homes.finish_btn",
        true,
        on_confirm,
        close,
    ))
}

fn undo_dialog(r: HomeMoveRecord, close: Callback<()>, done: Callback<()>) -> AnyView {
    let user = r.user.clone();
    let back = if r.from_folder.is_empty() { r.from.clone() } else { r.from_folder.clone() };
    let on_confirm = Callback::new(move |_| {
        close.run(());
        run(
            format!("/api/v1/services/homes/move/{}/undo", api::encode(&user)),
            None,
            tf("homes.undone", &[("name", &user)]),
            done,
        );
    });
    confirm(Confirm::new(
        tf("homes.undo_title", &[("name", &r.user)]),
        tf("homes.undo_text", &[("path", &back)]),
        "homes.undo",
        false,
        on_confirm,
        close,
    ))
}
