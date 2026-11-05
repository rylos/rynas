//! Snapshots (D-SNAP; DSM Snapshot Replication, TrueNAS periodic snapshots, Proxmox): btrbk
//! snapshot jobs with GFS retention and a prune preview, the snapshot list per job or share
//! with browse and file/folder restore, whole-subvolume rollback (typed confirmation), chain-safe
//! delete, and the system restore points taken before apt runs. Job and system settings stage
//! pending changes; restore, rollback and delete are tasks.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::scheduler::{TaskTrigger, TaskView};
use rynas_api_types::snapshots::{
    EntryKind, RestoreMode, RestoreRequest, Retention, RetentionPreset, RetentionPreview, RetentionPreviewRequest, RollbackRequest, SnapshotDeletion,
    SnapshotEntry, SnapshotInfo, SnapshotJob, SnapshotJobRequest, SnapshotJobView, SnapshotListing, SystemSnapshots,
};
use rynas_api_types::{ShareView, TaskStarted, TaskStatus};

use crate::api;
use crate::form::{Draft, In, stale_notice, lens, lens_bool, number_field, save_bar, segmented, switch, tabs, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::scheduler::{TriggerEdit, run_status, trigger_text};
use crate::tasks;
use crate::toast::{saved_pending, toast};
use crate::ui::{self, Confirm, EmptyState, ErrorBox, PageHeader, Remote, Skeleton, Tone, confirm, drawer};

/// `[A-Za-z0-9][A-Za-z0-9_-]{0,47}`.
pub fn valid_job_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 48 && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

/// GFS counts in short form: "24 h · 14 d · 8 w · 12 m".
pub fn retention_text(r: &Retention) -> String {
    let parts: Vec<String> = [(r.hourly, "snap.u.h"), (r.daily, "snap.u.d"), (r.weekly, "snap.u.w"), (r.monthly, "snap.u.m"), (r.yearly, "snap.u.y")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, k)| format!("{n} {}", t(k)))
        .collect();
    if parts.is_empty() { t("snap.ret.none").to_string() } else { parts.join(" · ") }
}

fn preset_code(p: Option<RetentionPreset>) -> &'static str {
    match p {
        Some(RetentionPreset::Standard) => "standard",
        Some(RetentionPreset::Light) => "light",
        Some(RetentionPreset::Long) => "long",
        None => "custom",
    }
}

/// Why a snapshot may not be deleted (None: it may).
fn deletion_reason(d: SnapshotDeletion) -> Option<&'static str> {
    match d {
        SnapshotDeletion::Allowed => None,
        SnapshotDeletion::Latest => Some("snap.del.latest"),
        SnapshotDeletion::Writable => Some("snap.del.writable"),
        SnapshotDeletion::NotSubvolume => Some("snap.del.not_subvolume"),
    }
}

/// Run a task started by `start` and report its end in a toast; `done` runs afterwards.
fn follow_toast(start: impl std::future::Future<Output = Result<String, api::ApiErr>> + 'static, ok: String, busy: RwSignal<bool>, error: RwSignal<Option<String>>, done: Callback<()>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = match start.await {
            Ok(id) => tasks::follow(&id).await,
            Err(e) => Err(e),
        };
        busy.set(false);
        match r {
            Ok(task) if task.status == TaskStatus::Ok => {
                toast(Tone::Ok, ok);
                done.run(());
            }
            Ok(task) => {
                error.set(Some(tasks::task_error(&task)));
                toast(Tone::Err, tasks::task_error(&task));
                done.run(());
            }
            Err(e) => error.set(Some(e.text())),
        }
    });
}

#[component]
pub fn Snapshots() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    let jobs = Remote::<Vec<SnapshotJobView>>::with_config("/api/v1/snapshots/jobs");
    let system = Remote::<SystemSnapshots>::with_config("/api/v1/snapshots/system");
    let filter = RwSignal::new(String::new());
    let edit = RwSignal::new(None::<Option<SnapshotJobView>>);
    view! {
        <div class="page">
            <PageHeader icon=icons::CAMERA title="snap.title" subtitle="snap.subtitle">
                <button class="btn primary" on:click=move |_| edit.set(Some(None))>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("snap.create_job")}</span>
                </button>
            </PageHeader>
            <section class="card widget w-12">
                {tabs("snp", &["snap.tab.snapshots", "snap.tab.jobs", "snap.tab.system"], tab, "snap.title")}
                <div role="tabpanel" id="snp-p0" aria-labelledby="snp-t0" hidden=move || tab.get() != 0>{snapshots_panel(jobs, system, filter)}</div>
                <div role="tabpanel" id="snp-p1" aria-labelledby="snp-t1" hidden=move || tab.get() != 1>{jobs_panel(jobs, edit, filter, tab)}</div>
                <div role="tabpanel" id="snp-p2" aria-labelledby="snp-t2" hidden=move || tab.get() != 2>{system_panel(system)}</div>
            </section>
            {move || edit.get().map(|v| job_editor(v, Callback::new(move |_| edit.set(None))))}
        </div>
    }
}

// ------------------------------------------------------------------ snapshot list

fn snapshots_panel(jobs: Remote<Vec<SnapshotJobView>>, system: Remote<SystemSnapshots>, filter: RwSignal<String>) -> AnyView {
    let items = RwSignal::new(None::<Vec<SnapshotInfo>>);
    let err = RwSignal::new(None::<String>);
    let browse = RwSignal::new(None::<SnapshotInfo>);
    let rollback = RwSignal::new(None::<SnapshotInfo>);
    let del = RwSignal::new(None::<SnapshotInfo>);
    let load = move || {
        let f = filter.get_untracked();
        let url = if f.is_empty() { "/api/v1/snapshots".to_string() } else { format!("/api/v1/snapshots?job={}", api::encode(&f)) };
        spawn_local(async move {
            match api::get::<Vec<SnapshotInfo>>(&url).await {
                Ok(l) => {
                    items.set(Some(l));
                    err.set(None);
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => err.set(Some(e.code().to_string())),
            }
        });
    };
    Effect::new(move |_| {
        filter.track();
        crate::config::generation();
        load();
    });
    let reload = Callback::new(move |_| load());
    let sys_prefix = move || system.data.with(|s| s.as_ref().map(|s| s.prefix.clone()));
    let job_options = move || {
        let cur = filter.get();
        let mut names: Vec<String> = jobs.data.with(|l| l.iter().flatten().map(|j| j.job.name.clone()).collect());
        if let Some(p) = sys_prefix() {
            names.push(p);
        }
        if !cur.is_empty() && !names.contains(&cur) {
            names.push(cur.clone());
        }
        let all_sel = cur.is_empty();
        view! {
            <option value="" selected=all_sel>{tr("snap.all_jobs")}</option>
            {names.into_iter().map(|n| { let sel = n == cur; let label = n.clone(); view! { <option value=n selected=sel>{label}</option> } }).collect_view()}
        }
    };
    let table = move || {
        if items.with(Option::is_none) {
            return match err.get() {
                Some(code) => view! { <ErrorBox code=code on_retry=reload /> }.into_any(),
                None => view! { <Skeleton lines=5 /> }.into_any(),
            };
        }
        let rows = items.get().unwrap_or_default();
        if rows.is_empty() {
            return view! { <EmptyState icon=icons::CAMERA title=Signal::derive(|| t("snap.none").to_string()) text=Signal::derive(|| t("snap.none_text").to_string()) /> }.into_any();
        }
        let prefix = sys_prefix();
        view! {
            {ui::table("", &["snap.col.snapshot", "snap.col.job", "snap.col.created", "col.status", ""], rows.into_iter().map(|s| {
                            let is_system = s.reason.is_some() || prefix.as_deref() == Some(s.job.as_str());
                            let why_not = deletion_reason(s.deletion);
                            let (s1, s2, s3) = (s.clone(), s.clone(), s.clone());
                            let can_browse = s.readonly;
                            let can_rollback = !is_system && s.deletion != SnapshotDeletion::NotSubvolume;
                            ui::row(vec![
                                ui::cell("snap.col.snapshot", "", view! {
                                    <div class="cell-main">
                                        <button class="link-strong mono" disabled=!can_browse on:click=move |_| browse.set(Some(s1.clone()))>{s.id.clone()}</button>
                                        <span class="cell-sub mono ellipsis-cell">{s.path.clone()}</span>
                                    </div>
                                }
                                .into_any()),
                                ui::cell("snap.col.job", "", view! {
                                    <div class="cell-main">
                                        <span>{if is_system { t("snap.system_point").to_string() } else { s.job.clone() }}</span>
                                        {s.share.clone().map(|sh| view! { <span class="cell-sub">{tf("snap.share_of", &[("share", &sh)])}</span> })}
                                    </div>
                                }
                                .into_any()),
                                ui::cell("snap.col.created", "tnum", view! { {i18n::datetime(s.created_ms)} }.into_any()),
                                ui::cell("col.status", "", view! {
                                    <div class="chips">
                                        {if s.readonly { view! { <span class="chip"><Icon icon=icons::LOCK size=12 />{tr("snap.readonly")}</span> }.into_any() } else { ui::status(Tone::Warn, t("snap.writable").to_string()) }}
                                        {(s.deletion == SnapshotDeletion::Latest).then(|| view! { <span class="chip accent">{tr("snap.latest")}</span> })}
                                        {s.reason.clone().map(|r| view! { <span class="chip">{tf("snap.reason", &[("r", &r)])}</span> })}
                                    </div>
                                }
                                .into_any()),
                                ui::actions(view! {
                                    <button class="icon-btn sm" title=tr("snap.browse") aria-label=format!("{}: {}", t("snap.browse"), s.id) disabled=!can_browse on:click=move |_| browse.set(Some(s2.clone()))>
                                        <Icon icon=icons::FOLDER_OPEN size=16 />
                                    </button>
                                    <button class="icon-btn sm" title=move || if is_system { t("snap.rollback_system") } else { t("snap.rollback") }
                                        aria-label=format!("{}: {}", t("snap.rollback"), s.id) disabled=!can_rollback on:click=move |_| rollback.set(Some(s3.clone()))>
                                        <Icon icon=icons::ROTATE_CCW size=16 />
                                    </button>
                                    <button class="icon-btn sm danger" title=move || why_not.map(t).unwrap_or_else(|| t("delete"))
                                        aria-label=format!("{}: {}", t("delete"), s.id) disabled=why_not.is_some() on:click=move |_| del.set(Some(s.clone()))>
                                        <Icon icon=icons::TRASH_2 size=16 />
                                    </button>
                                }
                                .into_any()),
                            ])
                        }).collect::<Vec<_>>())}
        }
        .into_any()
    };
    let rollback_dialog = move || {
        rollback.get().map(|s| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| rollback.set(None));
            let id = s.id.clone();
            let id2 = s.id.clone();
            confirm(Confirm {
                title: tf("snap.rollback_title", &[("id", &s.id)]),
                text: t("snap.rollback_text").to_string(),
                action: "snap.rollback",
                danger: true,
                extra: Some(view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("snap.rollback_nfs")}</p> }.into_any()),
                typed: Signal::stored(Some(id.clone())),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    let body = serde_json::to_string(&RollbackRequest { confirm: id2.clone() }).ok();
                    let url = format!("/api/v1/snapshots/{}/rollback", api::encode(&id2));
                    follow_toast(
                        async move { api::start_task(&url, body).await },
                        tf("snap.rolled_back", &[("id", &id)]),
                        busy,
                        error,
                        Callback::new(move |_| {
                            close.run(());
                            reload.run(());
                        }),
                    );
                }),
                on_cancel: close,
            })
        })
    };
    let delete_dialog = move || {
        del.get().map(|s| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            let id = s.id.clone();
            confirm(Confirm {
                title: tf("snap.delete_title", &[("id", &s.id)]),
                text: t("snap.delete_text").to_string(),
                action: "delete",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    let url = format!("/api/v1/snapshots/{0}?confirm={0}", api::encode(&id));
                    follow_toast(
                        async move { api::delete::<TaskStarted>(&url).await.map(|t| t.task_id) },
                        tf("snap.deleted", &[("id", &id)]),
                        busy,
                        error,
                        Callback::new(move |_| {
                            close.run(());
                            reload.run(());
                        }),
                    );
                }),
                on_cancel: close,
            })
        })
    };
    view! {
        <div class="toolbar">
            <select class="input filter" aria-label=tr("snap.col.job") on:change=move |ev| filter.set(event_target_value(&ev))>{job_options}</select>
            <button class="btn ghost" on:click=move |_| load()><Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("snap.refresh")}</span></button>
        </div>
        {table}
        {move || browse.get().map(|s| browse_drawer(s, Callback::new(move |_| browse.set(None))))}
        {rollback_dialog}
        {delete_dialog}
    }
    .into_any()
}

// ------------------------------------------------------------------ browse and restore

/// `rwxr-xr-x` of permission bits.
pub fn mode_text(mode: u32, kind: EntryKind) -> String {
    let mut s = String::with_capacity(10);
    s.push(match kind {
        EntryKind::Dir => 'd',
        EntryKind::Symlink => 'l',
        EntryKind::Other => '?',
        EntryKind::File => '-',
    });
    for shift in [6, 3, 0] {
        let b = (mode >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn browse_drawer(s: SnapshotInfo, close: Callback<()>) -> AnyView {
    let id = StoredValue::new(s.id.clone());
    let path = RwSignal::new(String::new());
    let listing = RwSignal::new(None::<SnapshotListing>);
    let err = RwSignal::new(None::<String>);
    let restore = RwSignal::new(None::<(String, bool)>);
    let load = move || {
        let url = format!("/api/v1/snapshots/{}/browse?path={}", api::encode(&id.get_value()), api::encode(&path.get_untracked()));
        listing.set(None);
        spawn_local(async move {
            match api::get::<SnapshotListing>(&url).await {
                Ok(l) => {
                    err.set(None);
                    listing.set(Some(l));
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => err.set(Some(e.code().to_string())),
            }
        });
    };
    Effect::new(move |_| {
        path.track();
        load();
    });
    let crumbs = move || {
        let p = path.get();
        let mut acc = String::new();
        let mut v = vec![view! {
            <li><button class="link-btn mono" on:click=move |_| path.set(String::new())><Icon icon=icons::CAMERA size=14 />{id.get_value()}</button></li>
        }
        .into_any()];
        for seg in p.split('/').filter(|s| !s.is_empty()) {
            acc = join(&acc, seg);
            let target = acc.clone();
            let seg = seg.to_string();
            v.push(view! { <li><Icon icon=icons::CHEVRON_RIGHT size=12 /><button class="link-btn mono" on:click=move |_| path.set(target.clone())>{seg}</button></li> }.into_any());
        }
        view! { <nav aria-label=tr("snap.path")><ol class="crumbs">{v}</ol></nav> }
    };
    let up = move |_| path.update(|p| *p = p.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default());
    let entries = move || {
        if listing.with(Option::is_none) {
            return match err.get() {
                Some(code) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
                None => view! { <Skeleton lines=6 /> }.into_any(),
            };
        }
        let l = listing.get().unwrap_or_default();
        if l.entries.is_empty() {
            return view! { <p class="muted table-none">{tr("snap.empty_dir")}</p> }.into_any();
        }
        let dir = l.path.clone();
        view! {
            {l.truncated.then(|| view! { {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("snap.truncated")} }.into_any())} })}
            {ui::table("", &["snap.col.name", "#snap.col.size", "snap.col.modified", "snap.col.mode", ""], l.entries.into_iter().map(|e| entry_row(e, &dir, path, restore)).collect::<Vec<_>>())}
        }
        .into_any()
    };
    let restore_dialog = move || {
        restore.get().map(|(rel, is_dir)| {
            let mode = RwSignal::new("copy".to_string());
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let cancel = Callback::new(move |_| restore.set(None));
            let rel2 = rel.clone();
            const MODES: &[(&str, &str)] = &[("copy", "snap.mode.copy"), ("replace", "snap.mode.replace")];
            let extra = view! {
                {segmented("snap.restore_mode", MODES, mode.into(), Callback::new(move |v| mode.set(v)))}
                <p class="field-help">{move || if mode.get() == "replace" { t("snap.mode.replace_help") } else { t("snap.mode.copy_help") }}</p>
            }
            .into_any();
            confirm(Confirm {
                title: tf(if is_dir { "snap.restore_folder" } else { "snap.restore_file" }, &[("path", &rel)]),
                text: tf("snap.restore_text", &[("id", &id.get_value())]),
                action: "snap.restore",
                danger: false,
                extra: Some(extra),
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    let m = if mode.get_untracked() == "replace" { RestoreMode::Replace } else { RestoreMode::Copy };
                    let body = serde_json::to_string(&RestoreRequest { path: rel2.clone(), mode: m }).ok();
                    let url = format!("/api/v1/snapshots/{}/restore", api::encode(&id.get_value()));
                    follow_toast(async move { api::start_task(&url, body).await }, tf("snap.restored", &[("path", &rel2)]), busy, error, Callback::new(move |_| restore.set(None)));
                }),
                on_cancel: cancel,
            })
        })
    };
    let body = view! {
        <div class="toolbar browse-bar">
            <button class="icon-btn sm" title=tr("snap.up") aria-label=tr("snap.up") disabled=move || path.with(String::is_empty) on:click=up>
                <Icon icon=icons::ARROW_UP size=16 />
            </button>
            {crumbs}
            <button class="btn ghost sm" disabled=move || path.with(String::is_empty) on:click=move |_| restore.set(Some((path.get_untracked(), true)))>
                <Icon icon=icons::ARCHIVE_RESTORE size=14 />{tr("snap.restore_this")}
            </button>
        </div>
        {entries}
        {restore_dialog}
    }
    .into_any();
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    let title = Signal::stored(tf("snap.browse_title", &[("id", &s.id)]));
    view! { <div class="wide-drawer">{drawer(title, close, body, foot)}</div> }.into_any()
}

fn entry_row(e: SnapshotEntry, dir: &str, path: RwSignal<String>, restore: RwSignal<Option<(String, bool)>>) -> AnyView {
    let rel = join(dir, &e.name);
    let is_dir = e.kind == EntryKind::Dir;
    let icon = match e.kind {
        EntryKind::Dir => icons::FOLDER,
        EntryKind::Symlink => icons::LINK,
        _ => icons::FILE,
    };
    let rel_open = rel.clone();
    let name = e.name.clone();
    let restorable = matches!(e.kind, EntryKind::Dir | EntryKind::File | EntryKind::Symlink);
    let label = format!("{}: {}", t("snap.restore"), e.name);
    ui::row(vec![
        ui::cell("snap.col.name", "", view! {
            <span class="entry-name">
                <Icon icon=icon size=16 />
                {if is_dir {
                    view! { <button class="link-strong" on:click=move |_| path.set(rel_open.clone())>{name}</button> }.into_any()
                } else {
                    view! { <span>{name}</span> }.into_any()
                }}
                {e.link.clone().map(|l| view! { <span class="muted mono small">{format!("→ {l}")}</span> })}
            </span>
        }
        .into_any()),
        ui::cell("snap.col.size", "num tnum", view! { {(!is_dir).then(|| i18n::bytes(e.size_bytes))} }.into_any()),
        ui::cell("snap.col.modified", "tnum", view! { {i18n::datetime(e.mtime_ms)} }.into_any()),
        ui::cell("snap.col.mode", "mono small", view! { {format!("{} {}:{}", mode_text(e.mode, e.kind), e.uid, e.gid)} }.into_any()),
        ui::actions(view! {
            <button class="icon-btn sm" title=tr("snap.restore") aria-label=label disabled=!restorable on:click=move |_| restore.set(Some((rel.clone(), is_dir)))>
                <Icon icon=icons::ARCHIVE_RESTORE size=16 />
            </button>
        }
        .into_any()),
    ])
    .into_any()
}

// ------------------------------------------------------------------ jobs

fn jobs_panel(jobs: Remote<Vec<SnapshotJobView>>, edit: RwSignal<Option<Option<SnapshotJobView>>>, filter: RwSignal<String>, tab: RwSignal<usize>) -> AnyView {
    let del = RwSignal::new(None::<String>);
    let table = move || {
        if let Some(v) = jobs.gate(4) {
            return v;
        }
        let rows = jobs.data.get().unwrap_or_default();
        if rows.is_empty() {
            return view! {
                <EmptyState icon=icons::CAMERA title=Signal::derive(|| t("snap.no_jobs").to_string()) text=Signal::derive(|| t("snap.no_jobs_text").to_string())>
                    <button class="btn primary" on:click=move |_| edit.set(Some(None))><Icon icon=icons::PLUS size=16 />{tr("snap.create_job")}</button>
                </EmptyState>
            }
            .into_any();
        }
        view! {
            {ui::table("", &["sch.col.enabled", "snap.col.job", "sch.col.schedule", "snap.col.retention", "snap.tab.snapshots", "sch.col.last", ""], rows.into_iter().map(|v| job_row(v, jobs, edit, del, filter, tab)).collect::<Vec<_>>())}
        }
        .into_any()
    };
    let deleter = move || {
        del.get().map(|name| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            let n = name.clone();
            confirm(Confirm {
                title: tf("snap.delete_job", &[("name", &name)]),
                text: t("snap.delete_job_text").to_string(),
                action: "delete",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    busy.set(true);
                    let n = n.clone();
                    spawn_local(async move {
                        let r = api::delete_empty(&format!("/api/v1/snapshots/jobs/{}", api::encode(&n))).await;
                        busy.set(false);
                        match r {
                            Ok(()) => {
                                saved_pending();
                                crate::config::changed();
                                close.run(());
                            }
                            Err(e) => error.set(Some(e.text())),
                        }
                    });
                }),
                on_cancel: close,
            })
        })
    };
    view! {
        <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("snap.jobs_help")}</p>
        {table}
        {deleter}
    }
    .into_any()
}

fn job_row(
    v: SnapshotJobView,
    jobs: Remote<Vec<SnapshotJobView>>,
    edit: RwSignal<Option<Option<SnapshotJobView>>>,
    del: RwSignal<Option<String>>,
    filter: RwSignal<String>,
    tab: RwSignal<usize>,
) -> AnyView {
    let name = v.job.name.clone();
    let task_id = v.task_id.clone();
    let deleting = v.pending == Some(rynas_api_types::PendingOp::Delete);
    let toggling = RwSignal::new(false);
    let running = RwSignal::new(false);
    let tid = task_id.clone();
    let toggle = move |on: bool| {
        toggling.set(true);
        let tid = tid.clone();
        spawn_local(async move {
            let r = api::post::<(), TaskView>(&format!("/api/v1/scheduler/tasks/{}/{}", api::encode(&tid), if on { "enable" } else { "disable" }), None).await;
            toggling.set(false);
            match r {
                Ok(_) => {
                    saved_pending();
                    crate::config::changed();
                }
                Err(e) => {
                    toast(Tone::Err, e.text());
                    jobs.reload();
                }
            }
        });
    };
    let (tid2, n2) = (task_id.clone(), name.clone());
    let run = move |_| {
        running.set(true);
        let (tid, n) = (tid2.clone(), n2.clone());
        spawn_local(async move {
            let r = api::post_empty::<()>(&format!("/api/v1/scheduler/tasks/{}/run", api::encode(&tid)), None).await;
            running.set(false);
            match r {
                Ok(()) => toast(Tone::Ok, tf("sch.started", &[("name", &n)])),
                Err(e) if e.code() == rynas_api_types::error_codes::NOT_APPLIED => toast(Tone::Err, t("sch.not_applied").to_string()),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };
    let source = v.job.share.clone().map(|s| tf("snap.share_of", &[("share", &s)])).unwrap_or_else(|| v.job.subvolume.clone());
    let last = v.last_run.as_ref().map(|r| {
        let (tone, text) = run_status(r);
        view! { <div class="cell-main">{ui::status(tone, text)}<span class="cell-sub tnum">{i18n::short_when(r.end_ms)}</span></div> }
    });
    let v_edit = v.clone();
    let (n3, n4, n5, n6, n7) = (name.clone(), name.clone(), name.clone(), name.clone(), name.clone());
    ui::row_class(
        vec![
            ui::cell("sch.col.enabled", "switch-col", view! {
                <input type="checkbox" role="switch" class="switch" aria-label=format!("{}: {}", t("sch.col.enabled"), n3)
                prop:checked=v.enabled disabled=move || toggling.get() || deleting on:change=move |ev| toggle(event_target_checked(&ev)) />
            }
            .into_any()),
            ui::cell("snap.col.job", "", view! {
                <div class="cell-main">
                    <button class="link-strong mono" disabled=deleting on:click={ let v = v.clone(); move |_| edit.set(Some(Some(v.clone()))) }>{name.clone()}</button>
                    <span class="cell-sub ellipsis-cell">{source}</span>
                </div>
            }
            .into_any()),
            ui::cell("sch.col.schedule", "", view! {
                <div class="cell-main">
                    <span>{v.schedule.as_ref().map(trigger_text).unwrap_or_else(|| "—".into())}</span>
                    <span class="cell-sub tnum">{v.next_run_ms.map(|m| tf("sch.next", &[("when", &i18n::short_when(m))]))}</span>
                </div>
            }
            .into_any()),
            ui::cell("snap.col.retention", "", view! {
                <div class="cell-main">
                    <span class="tnum">{retention_text(&v.job.retention)}</span>
                    <span class="cell-sub">{v.job.retention.preset.map(|p| t(match p { RetentionPreset::Standard => "snap.preset.standard", RetentionPreset::Light => "snap.preset.light", RetentionPreset::Long => "snap.preset.long" }))}</span>
                </div>
            }
            .into_any()),
            ui::cell("snap.tab.snapshots", "", view! {
                <div class="cell-main">
                    <button class="link-btn tnum" title=tr("snap.show_snapshots") on:click=move |_| { filter.set(n4.clone()); tab.set(0); }>{v.snapshots}</button>
                    <span class="cell-sub tnum">{v.latest_ms.map(|m| tf("snap.latest_at", &[("when", &i18n::short_when(m))]))}</span>
                </div>
            }
            .into_any()),
            ui::cell("sch.col.last", "", view! { <div class="chips">{last}{ui::pending_status(v.pending)}</div> }.into_any()),
            ui::actions(view! {
                <button class="icon-btn sm" title=tr("snap.take_now") aria-label=format!("{}: {}", t("snap.take_now"), n5)
                    disabled=move || running.get() || v.pending == Some(rynas_api_types::PendingOp::Add) on:click=run>
                    <Icon icon=icons::CAMERA size=16 />
                </button>
                <button class="icon-btn sm" title=tr("edit") aria-label=format!("{} {}", t("edit"), n6) disabled=deleting on:click=move |_| edit.set(Some(Some(v_edit.clone())))>
                    <Icon icon=icons::PENCIL size=16 />
                </button>
                <button class="icon-btn sm danger" title=tr("delete") aria-label=format!("{} {}", t("delete"), n7) disabled=deleting on:click=move |_| del.set(Some(name.clone()))>
                    <Icon icon=icons::TRASH_2 size=16 />
                </button>
            }
            .into_any()),
        ],
        if !v.enabled { "muted-row" } else { "" },
    )
}

const PRESETS: &[(&str, &str)] = &[("standard", "snap.preset.standard"), ("light", "snap.preset.light"), ("long", "snap.preset.long"), ("custom", "snap.preset.custom")];
const SOURCES: &[(&str, &str)] = &[("share", "shares.folder"), ("path", "snap.src.path")];

fn job_editor(init: Option<SnapshotJobView>, close: Callback<()>) -> AnyView {
    let is_new = init.is_none();
    let shares = Remote::<Vec<ShareView>>::with_config("/api/v1/shares");
    let (job, schedule, enabled) = match &init {
        Some(v) => (v.job.clone(), v.schedule.clone(), v.enabled),
        None => (
            SnapshotJob { name: String::new(), share: None, subvolume: String::new(), snapshot_dir: String::new(), retention: Retention::default() },
            None,
            true,
        ),
    };
    let trig = TriggerEdit::new(&schedule.unwrap_or(TaskTrigger::Calendar { on_calendar: "hourly".into(), cron: None, persistent: true, randomized_delay_s: 0 }));
    let source = RwSignal::new(if job.share.is_some() || is_new { "share" } else { "path" }.to_string());
    let d = RwSignal::new(job);
    let on = RwSignal::new(enabled);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let preview = RwSignal::new(None::<Result<RetentionPreview, String>>);
    let previewing = RwSignal::new(false);
    let none = Signal::derive(|| None::<String>);

    let (name, set_name) = lens(d, |j| j.name.clone(), |j, v| j.name = v.trim().to_string());
    let name_err = Signal::derive(move || {
        let v = name.get();
        (!v.is_empty() && !valid_job_name(&v)).then(|| t("snap.v.name").to_string())
    });
    let (subvol, set_subvol) = lens(d, |j| j.subvolume.clone(), |j, v| j.subvolume = v.trim().to_string());
    let (sdir, set_sdir) = lens(d, |j| j.snapshot_dir.clone(), |j, v| j.snapshot_dir = v.trim().to_string());
    let (preset, set_preset) = lens(
        d,
        |j| preset_code(j.retention.preset).to_string(),
        |j, v| {
            j.retention = match v.as_str() {
                "standard" => RetentionPreset::Standard.retention(),
                "light" => RetentionPreset::Light.retention(),
                "long" => RetentionPreset::Long.retention(),
                _ => Retention { preset: None, ..j.retention.clone() },
            }
        },
    );
    let custom = Memo::new(move |_| d.with(|j| j.retention.preset.is_none()));
    let count = |get: fn(&Retention) -> u32, set: fn(&mut Retention, u32)| {
        (Signal::derive(move || d.with(|j| get(&j.retention).to_string())), Callback::new(move |v: String| d.update(|j| set(&mut j.retention, v.trim().parse().unwrap_or(0)))))
    };
    let (min, set_min) = lens(d, |j| j.retention.min.clone(), |j, v| j.retention.min = v.trim().to_string());

    let share_select = move || {
        let cur = d.with(|j| j.share.clone().unwrap_or_default());
        let names: Vec<(String, String)> = shares.data.with(|l| l.iter().flatten().map(|s| (s.share.name.clone(), s.share.path.clone())).collect());
        view! {
            <div class="field">
                <label class="field-label" for="snap-share">{tr("shares.folder")}</label>
                <select id="snap-share" class="input" disabled=!is_new
                    on:change=move |ev| { let v = event_target_value(&ev); d.update(|j| j.share = (!v.is_empty()).then_some(v)); }>
                    <option value="" selected=cur.is_empty()>{tr("snap.f.pick_share")}</option>
                    {names.into_iter().map(|(n, p)| { let sel = n == cur; view! { <option value=n.clone() selected=sel>{format!("{n} — {p}")}</option> } }).collect_view()}
                </select>
                <span class="field-help">{tr("snap.f.share_help")}</span>
            </div>
        }
    };
    let src_fields = move || {
        if source.get() == "share" {
            share_select.into_any()
        } else {
            text_field("snap.f.subvolume", subvol, set_subvol, none, In { mono: true, help: Some("snap.f.subvolume_help"), placeholder: "/volume1/photos", disabled: !is_new, ..In::default() })
        }
    };
    let build = move || {
        let mut j = d.get_untracked();
        if source.get_untracked() == "share" {
            j.subvolume = String::new();
        } else {
            j.share = None;
        }
        j
    };
    let run_preview = move |_| {
        let j = build();
        previewing.set(true);
        spawn_local(async move {
            let r = api::post::<_, RetentionPreview>("/api/v1/snapshots/jobs/preview-retention", Some(&RetentionPreviewRequest { job: j })).await;
            previewing.set(false);
            preview.set(Some(r.map_err(|e| e.text())));
        });
    };
    // a changed retention makes the shown preview stale
    Effect::new(move |prev: Option<()>| {
        d.with(|j| j.retention.clone());
        if prev.is_some() {
            preview.set(None);
        }
    });
    let preview_view = move || {
        preview.get().map(|p| match p {
            Ok(p) => {
                let keep = p.keep.len();
                let gone = p.delete.clone();
                view! {
                    <div class="ret-preview" aria-live="polite">
                        <p class="strong">{tf("snap.preview_counts", &[("keep", &keep.to_string()), ("delete", &gone.len().to_string())])}</p>
                        {(!gone.is_empty()).then(|| view! {
                            <ul class="plain-list mono small danger-text">{gone.into_iter().map(|n| view! { <li>{n}</li> }).collect_view()}</ul>
                        })}
                        {(keep == 0 && p.delete.is_empty()).then(|| view! { <p class="muted small">{tr("snap.preview_empty")}</p> })}
                    </div>
                }
                .into_any()
            }
            Err(msg) => view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{msg}</p> }.into_any(),
        })
    };

    let save = Callback::new(move |_| {
        let j = build();
        if is_new && !valid_job_name(&j.name) {
            error.set(Some(t("snap.v.name").to_string()));
            return;
        }
        if j.share.is_none() && !j.subvolume.starts_with('/') {
            error.set(Some(t("snap.v.source").to_string()));
            return;
        }
        let schedule = match trig.trigger() {
            Ok(s) => s,
            Err(k) => {
                error.set(Some(t(k).to_string()));
                return;
            }
        };
        let req = SnapshotJobRequest { job: j, schedule: Some(schedule), enabled: Some(on.get_untracked()) };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = if is_new {
                api::post::<_, SnapshotJobView>("/api/v1/snapshots/jobs", Some(&req)).await.map(|_| ())
            } else {
                api::put::<_, serde_json::Value>(&format!("/api/v1/snapshots/jobs/{}", api::encode(&req.job.name)), &req).await.map(|_| ())
            };
            busy.set(false);
            match r {
                Ok(()) => {
                    saved_pending();
                    crate::config::changed();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });

    let title = {
        let n = d.with_untracked(|j| j.name.clone());
        Signal::derive(move || if is_new { t("snap.create_job").to_string() } else { tf("edit_named", &[("name", &n)]) })
    };
    let (h, set_h) = count(|r| r.hourly, |r, v| r.hourly = v);
    let (dd, set_dd) = count(|r| r.daily, |r, v| r.daily = v);
    let (w, set_w) = count(|r| r.weekly, |r, v| r.weekly = v);
    let (m, set_m) = count(|r| r.monthly, |r, v| r.monthly = v);
    let (y, set_y) = count(|r| r.yearly, |r, v| r.yearly = v);
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            <h3 class="form-section">{tr("snap.sec.source")}</h3>
            {is_new.then(|| text_field("snap.f.name", name, set_name, name_err, In { mono: true, help: Some("snap.f.name_help"), maxlength: 48, ..In::default() }))}
            {is_new.then(|| segmented("snap.f.source", SOURCES, source.into(), Callback::new(move |v| source.set(v))))}
            {src_fields}
            <details class="advanced">
                <summary>{tr("advanced")}</summary>
                {text_field("snap.f.dir", sdir, set_sdir, none, In { mono: true, help: Some("snap.f.dir_help"), disabled: !is_new, ..In::default() })}
            </details>
            <h3 class="form-section">{tr("sch.sec.schedule")}</h3>
            {switch("snap.f.enabled", Some("snap.f.enabled_help"), on.into(), Callback::new(move |v| on.set(v)))}
            {trig.view(true)}
            <h3 class="form-section">{tr("snap.col.retention")}</h3>
            {segmented("snap.f.preset", PRESETS, preset, set_preset)}
            {move || if custom.get() {
                view! {
                    <div class="form-row three">
                        {number_field("snap.f.hourly", None, h, set_h, 0, 10000)}
                        {number_field("snap.f.daily", None, dd, set_dd, 0, 10000)}
                        {number_field("snap.f.weekly", None, w, set_w, 0, 10000)}
                        {number_field("snap.f.monthly", None, m, set_m, 0, 10000)}
                        {number_field("snap.f.yearly", None, y, set_y, 0, 1000)}
                        {text_field("snap.f.min", min, set_min, none, In { mono: true, help: Some("snap.f.min_help"), ..In::default() })}
                    </div>
                }
                .into_any()
            } else {
                view! { <p class="field-help">{move || tf("snap.preset_keeps", &[("counts", &d.with(|j| retention_text(&j.retention))), ("min", &d.with(|j| j.retention.min.clone()))])}</p> }.into_any()
            }}
            <div>
                <button type="button" class="btn ghost sm" disabled=move || previewing.get() on:click=run_preview>
                    {crate::form::spinner(previewing)}<Icon icon=icons::EYE size=14 />{tr("snap.preview")}
                </button>
            </div>
            {preview_view}
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

// ------------------------------------------------------------------ system restore points

fn system_panel(system: Remote<SystemSnapshots>) -> AnyView {
    let form = move || {
        if let Some(v) = system.gate(4) {
            return v;
        }
        // built once: the draft follows the server (form::Draft)
        let draft = Draft::new(system.data.get_untracked().unwrap_or_default(), move || system.data.get());
        let d = draft.d;
        let busy = RwSignal::new(false);
        let error = RwSignal::new(None::<String>);
        let none = Signal::derive(|| None::<String>);
        let (enabled, set_enabled) = lens_bool(d, |s| s.enabled, |s, v| s.enabled = v);
        let (keep, set_keep) = lens(d, |s| s.keep.to_string(), |s, v| s.keep = v.trim().parse().unwrap_or(s.keep));
        let (sub, set_sub) = lens(d, |s| s.subvolume.clone(), |s, v| s.subvolume = v.trim().to_string());
        let (dir, set_dir) = lens(d, |s| s.snapshot_dir.clone(), |s, v| s.snapshot_dir = v.trim().to_string());
        let (prefix, set_prefix) = lens(d, |s| s.prefix.clone(), |s, v| s.prefix = v.trim().to_string());
        let changed = draft.dirty();
        let save = move |_| {
            let body = d.get_untracked();
            busy.set(true);
            error.set(None);
            spawn_local(async move {
                let r = api::put::<_, SystemSnapshots>("/api/v1/snapshots/system", &body).await;
                busy.set(false);
                match r {
                    Ok(_) => {
                        saved_pending();
                        crate::config::changed();
                    }
                    Err(e) => error.set(Some(e.text())),
                }
            });
        };
        view! {
            <form class="form narrow-form" on:submit=move |ev| ev.prevent_default()>
                <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("snap.system_help")}</p>
                {switch("snap.system_enabled", Some("snap.system_enabled_help"), enabled, set_enabled)}
                {number_field("snap.system_keep", Some("snap.system_keep_help"), keep, set_keep, 1, 100)}
                <details class="advanced">
                    <summary>{tr("advanced")}</summary>
                    {text_field("snap.f.subvolume", sub, set_sub, none, In { mono: true, ..In::default() })}
                    {text_field("snap.f.dir", dir, set_dir, none, In { mono: true, ..In::default() })}
                    {text_field("snap.system_prefix", prefix, set_prefix, none, In { mono: true, ..In::default() })}
                </details>
                {stale_notice(draft.stale, Callback::new(move |_| draft.reset()))}
                {crate::form::form_error(error)}
                <div class="sec-actions">
                    <button type="button" class="btn primary" disabled=move || busy.get() || !changed.get() on:click=save>
                        {crate::form::spinner(busy)}{tr("save")}
                    </button>
                </div>
            </form>
        }
        .into_any()
    };
    view! { {form} }.into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_names() {
        assert!(valid_job_name("photos"));
        assert!(valid_job_name("Bob_home-2"));
        assert!(!valid_job_name("_x"));
        assert!(!valid_job_name("a.b"));
        assert!(!valid_job_name(""));
    }

    #[test]
    fn modes() {
        assert_eq!(mode_text(0o755, EntryKind::Dir), "drwxr-xr-x");
        assert_eq!(mode_text(0o640, EntryKind::File), "-rw-r-----");
        assert_eq!(join("", "a"), "a");
        assert_eq!(join("a/b", "c"), "a/b/c");
    }

    #[test]
    fn presets() {
        assert_eq!(preset_code(Some(RetentionPreset::Light)), "light");
        assert_eq!(preset_code(None), "custom");
        assert_eq!(deletion_reason(SnapshotDeletion::Allowed), None);
    }
}
