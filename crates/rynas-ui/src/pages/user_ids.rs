//! UID/GID change and ownership remap dialog (`POST /api/v1/ids/plan`, then the task
//! `POST /api/v1/ids/remap`): an id, the folders, a dry run that counts what changes, and only
//! then the change.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{IdBlocker, IdBlockerCode, IdKind, IdRemapPlan, IdRemapRequest, LOCAL_ID_MAX, LOCAL_ID_MIN, TaskStarted, TaskStatus};

use crate::api;
use crate::form::{In, form_error, spinner, switch, text_field};
use crate::i18n::{t, tf, tr};
use crate::icons;
use crate::toast::toast;
use crate::ui::{self, DlgKind, Skeleton, Tone, dialog};

/// What the dialog is for: the account, its id now, and whether its id changes (`false`:
/// files of another id are given to it).
#[derive(Clone, Debug, PartialEq)]
pub struct IdAsk {
    pub kind: IdKind,
    pub name: String,
    pub current: u32,
    pub change: bool,
}

fn blocker_text(b: &IdBlocker) -> String {
    let d = b.detail.as_str();
    match b.code {
        IdBlockerCode::NotLocal => t("ids.b_not_local").to_string(),
        IdBlockerCode::Unmanaged => t("ids.b_unmanaged").to_string(),
        IdBlockerCode::OutOfRange => tf("ids.b_out_of_range", &[("d", d)]),
        IdBlockerCode::SameId => t("ids.b_same_id").to_string(),
        IdBlockerCode::IdInUse => tf("ids.b_id_in_use", &[("d", d)]),
        IdBlockerCode::Processes => tf("ids.b_processes", &[("d", d)]),
        IdBlockerCode::SmbSessions => tf("ids.b_smb_sessions", &[("d", d)]),
        IdBlockerCode::PathInvalid => tf("ids.b_path_invalid", &[("d", d)]),
        IdBlockerCode::BadRequest => t("ids.b_bad_request").to_string(),
        IdBlockerCode::Other => t("ids.b_other").to_string(),
    }
}

fn count(n: u64, partial: bool) -> String {
    let s = crate::i18n::num(n as f64, 0);
    if partial { tf("ids.at_least", &[("n", &s)]) } else { s }
}

fn plan_view(p: &IdRemapPlan) -> AnyView {
    if !p.allowed {
        let list: Vec<String> = p.blockers.iter().map(blocker_text).collect();
        return ui::note(Tone::Err, icons::CIRCLE_X, view! {
            <ul class="plain-list id-paths">{list.into_iter().map(|x| view! { <li>{x}</li> }).collect_view()}</ul>
        }
        .into_any());
    }
    let Some(c) = p.scan.clone() else {
        return ().into_any();
    };
    let part = c.partial;
    let changed_key = if p.kind == IdKind::User { "ids.owners" } else { "ids.groups" };
    let changed = if p.kind == IdKind::User { c.owners } else { c.groups };
    let paths = |v: Vec<String>, key: &'static str| {
        (!v.is_empty()).then(|| {
            ui::note(Tone::Info, icons::INFO, view! {
                <span>{tr(key)}</span>
                <ul class="plain-list id-paths mono">{v.into_iter().map(|x| view! { <li>{x}</li> }).collect_view()}</ul>
            }
            .into_any())
        })
    };
    view! {
        <dl class="kvs">
            {ui::kv_row("ids.scanned", Signal::stored(count(c.scanned, part)), "tnum")}
            {ui::kv_row(changed_key, Signal::stored(count(changed, part)), "tnum")}
            {ui::kv_row("ids.acl_entries", Signal::stored(count(c.acl_entries, part)), "tnum")}
            {(c.errors > 0).then(|| ui::kv_row("ids.errors", Signal::stored(count(c.errors, part)), "tnum"))}
        </dl>
        {part.then(|| ui::note(Tone::Info, icons::TIMER, view! { <span>{tr("ids.partial")}</span> }.into_any()))}
        {paths(c.readonly, "ids.readonly")}
        {paths(c.other_fs, "ids.other_fs")}
    }
    .into_any()
}

/// The dialog. `done` runs after the task (reload the lists).
pub fn id_dialog(ask: IdAsk, close: Callback<()>, done: Callback<()>) -> AnyView {
    let id = RwSignal::new(String::new());
    let acls = RwSignal::new(true);
    // folders: the suggested ones (from the first check) with a tick each, plus added ones
    let folders = RwSignal::new(Vec::<(String, bool)>::new());
    let extra = RwSignal::new(String::new());
    let plan = RwSignal::new(None::<Result<IdRemapPlan, String>>);
    let checking = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let ask2 = ask.clone();
    let request = move || -> Option<IdRemapRequest> {
        let n: u32 = id.with(|v| v.trim().parse().ok())?;
        let paths: Vec<String> = folders.with(|f| f.iter().filter(|(_, on)| *on).map(|(p, _)| p.clone()).collect());
        Some(IdRemapRequest {
            kind: ask2.kind,
            name: ask2.name.clone(),
            new_id: ask2.change.then_some(n),
            from_id: (!ask2.change).then_some(n),
            paths,
            acls: acls.get(),
            expected: None,
        })
    };
    // any change of the inputs makes the last check stale (not the folder list filled in by
    // the first check)
    let keep = StoredValue::new(false);
    Effect::new(move |_| {
        id.track();
        acls.track();
        folders.track();
        if keep.get_value() {
            keep.set_value(false);
        } else {
            plan.set(None);
        }
    });
    let req2 = request.clone();
    let check = move || {
        let Some(r) = req2() else {
            error.set(Some(t("ids.b_bad_request").to_string()));
            return;
        };
        error.set(None);
        checking.set(true);
        spawn_local(async move {
            let res = api::post::<_, IdRemapPlan>("/api/v1/ids/plan", Some(&r)).await;
            let _ = checking.try_set(false);
            if let Ok(p) = &res
                && folders.with_untracked(Vec::is_empty)
            {
                // first check: the suggested folders become the list (all ticked), without
                // marking this answer stale
                let list: Vec<(String, bool)> = p.roots.iter().map(|r| (r.clone(), true)).collect();
                keep.set_value(true);
                folders.set(list);
            }
            let _ = plan.try_set(Some(res.map_err(|e| e.text())));
        });
    };
    let ready = move || plan.with(|p| matches!(p, Some(Ok(p)) if p.allowed && p.scan.is_some()));
    let req3 = request.clone();
    let name = ask.name.clone();
    let start = move || {
        let Some(mut r) = req3() else { return };
        r.expected = plan.with_untracked(|p| match p {
            Some(Ok(p)) => p.scan.as_ref().filter(|c| !c.partial).map(|c| c.scanned),
            _ => None,
        });
        busy.set(true);
        error.set(None);
        let name = name.clone();
        spawn_local(async move {
            match api::post::<_, TaskStarted>("/api/v1/ids/remap", Some(&r)).await {
                Ok(st) => {
                    // a long walk: the dialog closes, the task panel shows the progress
                    close.run(());
                    match crate::tasks::follow(&st.task_id).await {
                        Ok(task) if task.status == TaskStatus::Ok => toast(Tone::Ok, tf("ids.done", &[("name", &name)])),
                        Ok(task) => toast(Tone::Err, crate::tasks::task_error(&task)),
                        Err(e) => toast(Tone::Err, e.text()),
                    }
                    crate::config::changed();
                    done.run(());
                }
                Err(e) => {
                    let _ = busy.try_set(false);
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let title = match (ask.change, ask.kind) {
        (true, IdKind::User) => tf("ids.title_uid", &[("name", &ask.name)]),
        (true, IdKind::Group) => tf("ids.title_gid", &[("name", &ask.name)]),
        (false, _) => tf("ids.title_reown", &[("name", &ask.name)]),
    };
    let (label, help) = match (ask.change, ask.kind) {
        (true, IdKind::User) => ("ids.new_uid", tf("ids.id_help_change", &[("min", &LOCAL_ID_MIN.to_string()), ("max", &LOCAL_ID_MAX.to_string()), ("id", &ask.current.to_string())])),
        (true, IdKind::Group) => ("ids.new_gid", tf("ids.id_help_change", &[("min", &LOCAL_ID_MIN.to_string()), ("max", &LOCAL_ID_MAX.to_string()), ("id", &ask.current.to_string())])),
        (false, _) => ("ids.from_id", t("ids.id_help_reown").to_string()),
    };
    let none = Signal::derive(|| None::<String>);
    let folder_list = move || {
        folders
            .get()
            .into_iter()
            .enumerate()
            .map(|(i, (p, on))| {
                view! {
                    <label class="check">
                        <input type="checkbox" prop:checked=on on:change=move |ev| {
                            let v = event_target_checked(&ev);
                            folders.update(|f| if let Some(x) = f.get_mut(i) { x.1 = v });
                        } />
                        <span class="mono id-paths">{p}</span>
                    </label>
                }
            })
            .collect_view()
    };
    let add = move || {
        let p = extra.get_untracked().trim().trim_end_matches('/').to_string();
        if p.starts_with('/') && !folders.with_untracked(|f| f.iter().any(|(x, _)| *x == p)) {
            folders.update(|f| f.push((p, true)));
            extra.set(String::new());
        }
    };
    let check2 = check.clone();
    let body = view! {
        <p class="dialog-text">{tr(if ask.change { "ids.text_change" } else { "ids.text_reown" })}</p>
        <form class="form" on:submit=move |ev| { ev.prevent_default(); check2(); }>
            {text_field(label, Signal::derive(move || id.get()), Callback::new(move |v: String| id.set(v.trim().to_string())), none, In { kind: "number", mono: true, maxlength: 10, ..In::default() })}
            <p class="field-help">{help}</p>
            <fieldset class="fieldset">
                <legend class="field-label">{tr("ids.folders")}</legend>
                <p class="field-help">{tr("ids.folders_help")}</p>
                <div class="id-folders">{folder_list}</div>
                <div class="toolbar">
                    {text_field("ids.add_folder", Signal::derive(move || extra.get()), Callback::new(move |v| extra.set(v)), none, In { mono: true, maxlength: 4096, ..In::default() })}
                    <button type="button" class="btn ghost" on:click=move |_| add()>{tr("ids.add_folder")}</button>
                </div>
            </fieldset>
            {switch("ids.acls", Some("ids.acls_help"), acls.into(), Callback::new(move |v| acls.set(v)))}
            <button type="submit" hidden=true></button>
        </form>
        {move || {
            if checking.get() {
                return view! { <Skeleton lines=4 /> }.into_any();
            }
            match plan.get() {
                None => ().into_any(),
                Some(Err(e)) => ui::note(Tone::Err, icons::CIRCLE_X, view! { <span>{e}</span> }.into_any()),
                Some(Ok(p)) => plan_view(&p),
            }
        }}
        {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <span>{tr("ids.after")}</span> }.into_any())}
        {form_error(error)}
    }
    .into_any();
    let change = ask.change;
    let foot = view! {
        <button type="button" class="btn ghost" data-autofocus="" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn ghost" disabled=move || checking.get() || busy.get() on:click=move |_| check()>
            {spinner(checking)}{tr("ids.check")}
        </button>
        <button type="button" class="btn danger" disabled=move || busy.get() || !ready() on:click=move |_| start()>
            {spinner(busy)}{tr(if change { "ids.start_change" } else { "ids.start_reown" })}
        </button>
    }
    .into_any();
    dialog(DlgKind::Wide, Signal::stored(title), close, body, Some(foot))
}
