//! Folders of the shared folders page: deleting a share with its folder (plan, typed
//! confirmation), the folders of the volumes that no share uses ("Unshared folders") with
//! "Share again" and "Delete permanently", and the translated remarks of a share.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{
    FolderDeletePlan, FolderRef, FolderUsage, FolderUsageKind, Share, ShareDeletePlanRequest, ShareStatus, ShareWarning, ShareWarningCode, TaskStarted,
    TaskStatus, UnsharedFolder,
};

use crate::api;
use crate::form::{form_error, spinner, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::{saved_pending, toast};
use crate::ui::{self, DlgKind, Remote, Skeleton, Tone, WidgetHead, dialog, typed_confirm};

/// Translated text of a share remark (the server's English text for codes this build does
/// not know).
fn warning_text(w: &ShareWarning, english: Option<&String>) -> String {
    let a = &w.args;
    let key = match w.code {
        ShareWarningCode::SubvolumeOnApply => "shares.w.subvolume_on_apply",
        ShareWarningCode::SubvolumeOnApplyNoCompression => "shares.w.subvolume_on_apply_nc",
        ShareWarningCode::NotSubvolume => "shares.w.not_subvolume",
        ShareWarningCode::ExtraOverrides => "shares.w.extra_overrides",
        ShareWarningCode::NfsUids => "shares.w.nfs_uids",
        ShareWarningCode::NfsServiceOff => "shares.w.nfs_off",
        ShareWarningCode::SmbServiceOff => "shares.w.smb_off",
        ShareWarningCode::TimemachineNoLimit => "shares.w.tm_no_limit",
        ShareWarningCode::MultiprotocolWithoutNfs => "shares.w.multi_no_nfs",
        ShareWarningCode::NoPermissions => "shares.w.no_permissions",
        ShareWarningCode::AllDenied => "shares.w.all_denied",
        ShareWarningCode::SnapshotFolderNoJob => "shares.w.snapfolder_no_job",
        ShareWarningCode::SnapshotFolderNfs => "shares.w.snapfolder_nfs",
        ShareWarningCode::Other => return english.cloned().unwrap_or_default(),
    };
    tf(
        key,
        &[("path", a.path.as_deref().unwrap_or("")), ("volume", a.volume.as_deref().unwrap_or("")), ("key", a.key.as_deref().unwrap_or(""))],
    )
}

/// The remarks of a share in the UI language (older servers: their English texts).
pub fn warnings(st: &ShareStatus) -> Vec<String> {
    if st.warning_items.is_empty() {
        return st.warnings.clone();
    }
    st.warning_items.iter().enumerate().map(|(i, w)| warning_text(w, st.warnings.get(i))).collect()
}

fn usage_text(u: &FolderUsage) -> String {
    let key = match u.kind {
        FolderUsageKind::NfsExport => "shares.use.nfs_export",
        FolderUsageKind::Share => "shares.use.share",
        FolderUsageKind::SnapshotJob => "shares.use.snapshot_job",
        FolderUsageKind::ScheduledTask => "shares.use.task",
        FolderUsageKind::Container => "shares.use.container",
        FolderUsageKind::NotOnVolume => "shares.use.not_on_volume",
        FolderUsageKind::Other => "shares.use.other",
    };
    tf(key, &[("name", &u.name)])
}

fn size_text(bytes: u64, partial: bool) -> String {
    if partial { tf("shares.size_min", &[("size", &i18n::bytes(bytes))]) } else { i18n::bytes(bytes) }
}

fn list(items: Vec<String>) -> AnyView {
    view! { <ul class="plain-list">{items.into_iter().map(|x| view! { <li>{x}</li> }).collect_view()}</ul> }.into_any()
}

/// What a deletion removes, what else uses the folder and why it may be refused.
fn plan_view(p: &FolderDeletePlan) -> AnyView {
    let info: Vec<String> = p.usages.iter().filter(|u| !p.blockers.contains(u)).map(usage_text).collect();
    let blockers: Vec<String> = p.blockers.iter().map(usage_text).collect();
    let recycle = t(if p.recycle_bin { "shares.del_recycle_yes" } else { "shares.del_recycle_no" }).to_string();
    view! {
        <dl class="kvs">
            {ui::kv_row("shares.del_folder", Signal::stored(p.path.clone()), "mono")}
            {ui::kv_row("shares.del_space", Signal::stored(size_text(p.size_bytes, p.size_partial)), "tnum")}
            {ui::kv_row("shares.del_snapshots", Signal::stored(i18n::num(p.snapshots as f64, 0)), "tnum")}
            {ui::kv_row("shares.recycle", Signal::stored(recycle), "")}
        </dl>
        {(!info.is_empty()).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <span>{tr("shares.del_used")}</span>{list(info)} }.into_any()))}
        {(!blockers.is_empty()).then(|| ui::note(Tone::Err, icons::CIRCLE_X, view! { <span>{tr("shares.del_blocked")}</span>{list(blockers)} }.into_any()))}
        {p.allowed.then(|| ui::note(Tone::Err, icons::TRIANGLE_ALERT, view! { <span>{tr("shares.del_irreversible")}</span> }.into_any()))}
    }
    .into_any()
}

/// What a deletion dialog deletes.
#[derive(Clone)]
pub enum Target {
    /// A share; its folder too when the user asks.
    Share(String),
    /// A folder without a share, for good.
    Folder(Box<UnsharedFolder>),
}

/// Delete a share (its folder stays, or goes with its snapshots after a plan and a typed
/// confirmation), or a folder without a share. `done` runs after a folder was deleted.
pub fn delete_dialog(target: Target, close: Callback<()>, done: Callback<()>) -> AnyView {
    let is_share = matches!(target, Target::Share(_));
    let name = match &target {
        Target::Share(n) => n.clone(),
        Target::Folder(f) => f.name.clone(),
    };
    let remove = RwSignal::new(!is_share);
    let plan = RwSignal::new(None::<Result<FolderDeletePlan, String>>);
    let typed_in = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let target = StoredValue::new(target);
    let fetch = move || {
        if plan.get_untracked().is_some() {
            return;
        }
        let tg = target.get_value();
        spawn_local(async move {
            let r = match &tg {
                Target::Share(n) => api::post::<_, FolderDeletePlan>("/api/v1/shares/plan/delete", Some(&ShareDeletePlanRequest { share: n.clone() })).await,
                Target::Folder(f) => {
                    api::post::<_, FolderDeletePlan>("/api/v1/folders/plan/delete", Some(&FolderRef { volume: f.volume.clone(), name: f.name.clone() })).await
                }
            };
            let _ = plan.try_set(Some(r.map_err(|e| e.text())));
        });
    };
    if !is_share {
        fetch();
    }
    let n_ok = name.clone();
    let ready = move || {
        !remove.get() || plan.with(|p| matches!(p, Some(Ok(p)) if p.allowed)) && typed_in.with(|x| x.trim() == n_ok)
    };
    let n_go = name.clone();
    let go = move || {
        busy.set(true);
        error.set(None);
        let n = n_go.clone();
        let tg = target.get_value();
        let all = remove.get_untracked();
        spawn_local(async move {
            match tg {
                Target::Share(_) => {
                    let mut url = format!("/api/v1/shares/{}", api::encode(&n));
                    if all {
                        url.push_str(&format!("?remove_folder=true&confirm={}", api::encode(&n)));
                    }
                    let r = api::delete_empty(&url).await;
                    busy.set(false);
                    match r {
                        Ok(()) => {
                            if all {
                                toast(Tone::Ok, t("shares.del_staged").to_string());
                            } else {
                                saved_pending();
                            }
                            crate::config::changed();
                            close.run(());
                        }
                        Err(e) => error.set(Some(e.text())),
                    }
                }
                Target::Folder(f) => {
                    let url = format!("/api/v1/folders/{}/{}?confirm={}", api::encode(&f.volume), api::encode(&f.name), api::encode(&f.name));
                    match api::delete::<TaskStarted>(&url).await {
                        Ok(st) => {
                            close.run(());
                            let r = crate::tasks::follow(&st.task_id).await;
                            match r {
                                Ok(task) if task.status == TaskStatus::Ok => toast(Tone::Ok, tf("shares.fdel_done", &[("name", &f.name)])),
                                Ok(task) => toast(Tone::Err, crate::tasks::task_error(&task)),
                                Err(e) => toast(Tone::Err, e.text()),
                            }
                            done.run(());
                        }
                        Err(e) => {
                            busy.set(false);
                            error.set(Some(e.text()));
                        }
                    }
                }
            }
        });
    };
    let title = if is_share { tf("shares.delete_title", &[("name", &name)]) } else { tf("shares.fdel_title", &[("name", &name)]) };
    let n_typed = name.clone();
    let body = view! {
        <p class="dialog-text">{move || t(if !is_share { "shares.fdel_text" } else if remove.get() { "shares.delete_all_text" } else { "shares.delete_text" })}</p>
        {is_share.then(|| switch("shares.delete_all", None, remove.into(), Callback::new(move |v| {
            remove.set(v);
            if v {
                fetch();
            }
        })))}
        {move || remove.get().then(|| match plan.get() {
            None => view! { <Skeleton lines=4 /> }.into_any(),
            Some(Err(e)) => ui::note(Tone::Err, icons::CIRCLE_X, view! { <span>{e}</span> }.into_any()),
            Some(Ok(p)) => view! { {plan_view(&p)} {p.allowed.then(|| typed_confirm(n_typed.clone(), typed_in))} }.into_any(),
        })}
        {form_error(error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" data-autofocus="" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn danger" disabled=move || busy.get() || !ready() on:click=move |_| go()>
            {spinner(busy)}
            {move || t(if remove.get() { "shares.delete_all_btn" } else { "delete" })}
        </button>
    }
    .into_any();
    dialog(DlgKind::Alert, Signal::stored(title), close, body, Some(foot))
}

/// The share to create when a folder is shared again: its previous settings, or its name
/// and path.
pub fn reshare(f: &UnsharedFolder) -> Share {
    match &f.previous {
        Some(s) => Share { path: f.path.clone(), subvolume: None, ..s.clone() },
        None => Share {
            name: if super::shares::valid_share_name(&f.name) { f.name.clone() } else { String::new() },
            path: f.path.clone(),
            ..Share::default()
        },
    }
}

/// Sizes of the unshared folders, measured after the list is on screen.
#[derive(Clone, Copy)]
struct Sizes {
    /// The list with measured sizes (`?sizes=true`).
    data: RwSignal<Option<Vec<UnsharedFolder>>>,
    measuring: RwSignal<bool>,
    err: RwSignal<Option<String>>,
}

/// "Unshared folders": the folders of the volumes that no share uses. The list comes first
/// with the sizes rynasd already knows; the others are measured afterwards (rynasd skips
/// volumes whose disks sleep), so the page never waits for a walk of the folders.
pub fn unshared_section(folders: Remote<Vec<UnsharedFolder>>, share_again: Callback<UnsharedFolder>, del: RwSignal<Option<UnsharedFolder>>) -> AnyView {
    let sizes = Sizes { data: RwSignal::new(None), measuring: RwSignal::new(false), err: RwSignal::new(None) };
    Effect::new(move |_| {
        if !folders.data.with(|d| d.as_ref().is_some_and(|v| !v.is_empty())) {
            return;
        }
        sizes.measuring.set(true);
        spawn_local(async move {
            let r = api::get::<Vec<UnsharedFolder>>("/api/v1/folders?sizes=true").await;
            let _ = sizes.measuring.try_set(false);
            match r {
                Ok(v) => {
                    let _ = sizes.err.try_set(None);
                    let _ = sizes.data.try_set(Some(v));
                }
                Err(e) => {
                    let _ = sizes.err.try_set(Some(e.text()));
                }
            }
        });
    });
    let content = move || {
        let rows: Vec<UnsharedFolder> = folders.data.with(|d| d.clone().unwrap_or_default());
        if rows.is_empty() {
            return view! { <p class="muted table-none">{tr("shares.unshared_none")}</p> }.into_any();
        }
        let measured = sizes.data.get().unwrap_or_default();
        let rows = rows
            .into_iter()
            .map(|mut f| {
                if let Some(m) = measured.iter().find(|m| m.path == f.path) {
                    f.size_bytes = m.size_bytes;
                    f.size_partial = m.size_partial;
                    f.size_at_ms = m.size_at_ms;
                    f.size_standby = m.size_standby;
                }
                folder_row(f, sizes, share_again, del)
            })
            .collect::<Vec<_>>();
        ui::table("", &["shares.uf_folder", "shares.location", "shares.size", "shares.del_snapshots", "shares.removed_at", ""], rows)
    };
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::FOLDER title="shares.unshared" />
            <p class="sec-note">{tr("shares.unshared_help")}</p>
            {folders.view(3, content)}
        </section>
    }
    .into_any()
}

fn folder_row(f: UnsharedFolder, sizes: Sizes, share_again: Callback<UnsharedFolder>, del: RwSignal<Option<UnsharedFolder>>) -> AnyView {
    let size = match f.size_bytes {
        Some(b) => size_text(b, f.size_partial),
        None => "—".to_string(),
    };
    // why the size is old or missing: disks asleep, still measuring, failed
    let size_note = match (f.size_standby, f.size_at_ms) {
        (true, Some(at)) => Some(tf("shares.size_asleep_as_of", &[("when", &i18n::datetime(at))])),
        (true, None) => Some(t("shares.size_asleep").to_string()),
        _ if f.size_bytes.is_none() && sizes.measuring.get() => Some(t("shares.size_measuring").to_string()),
        _ if f.size_bytes.is_none() => sizes.err.get(),
        _ => None,
    };
    let removed = f.removed_at_ms.map(i18n::datetime).unwrap_or_else(|| "—".into());
    let was = f.previous.as_ref().map(|s| tf("shares.was_shared_as", &[("name", &s.name)]));
    let (f1, f2) = (f.clone(), f.clone());
    let name = f.name.clone();
    ui::row(vec![
        ui::cell("shares.uf_folder", "", view! {
            <div class="cell-main">
                <strong>{f.name.clone()}</strong>
                <span class="cell-sub">{was}</span>
            </div>
        }
        .into_any()),
        ui::cell("shares.location", "path-cell", view! {
            <div class="cell-main">
                <span class="mono">{f.path.clone()}</span>
                <span class="cell-sub">{f.subvolume.then(|| t("shares.subvolume"))}</span>
            </div>
        }
        .into_any()),
        ui::cell("shares.size", "tnum", view! {
            <div class="cell-main">
                <span>{size}</span>
                <span class="cell-sub">{size_note}</span>
            </div>
        }
        .into_any()),
        ui::cell("shares.del_snapshots", "tnum", view! { {i18n::num(f.snapshots as f64, 0)} }.into_any()),
        ui::cell("shares.removed_at", "muted", view! { {removed} }.into_any()),
        ui::actions(view! {
            <button class="btn ghost sm" on:click=move |_| share_again.run(f1.clone())>
                <Icon icon=icons::SHARE_2 size=14 /><span class="btn-label">{tr("shares.share_again")}</span>
            </button>
            <button class="icon-btn sm danger" title=tr("shares.delete_forever") aria-label=format!("{} {}", t("shares.delete_forever"), name) on:click=move |_| del.set(Some(f2.clone()))>
                <Icon icon=icons::TRASH_2 size=16 />
            </button>
        }
        .into_any()),
    ])
}
