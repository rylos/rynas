//! Snapshot replication (tab of Snapshots): replica targets of the snapshot jobs on another
//! btrfs volume or another host over SSH, with their own retention; status per target,
//! "Replicate now", the replication key and the `authorized_keys` line for remote targets,
//! host key confirmation, and for local targets the replicas with browse, restore of a file
//! or folder and promote (a writable copy as a new folder). Changes stage pending changes
//! (keys `snapshots` and `scheduler`) and are applied at once from the editor.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::replication::{
    HostKey, HostKeyRequest, PromoteRequest, ReplicaDestination, ReplicaHint, ReplicaListing, ReplicaRequest, ReplicaRestoreRequest, ReplicaSnapshot, ReplicaTarget, ReplicaView,
    ReplicaWarning, ReplicationKey, StreamCompress, default_target_retention,
};
use rynas_api_types::scheduler::TaskTrigger;
use rynas_api_types::snapshots::{EntryKind, Retention, SnapshotJobView};
use rynas_api_types::{StagedChanges, TaskStatus};

use crate::api;
use crate::form::{In, lens, number_field, save_bar, segmented, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::scheduler::{TriggerEdit, trigger_text};
use crate::pages::snapshots::retention_text;
use crate::tasks;
use crate::toast::toast;
use crate::ui::{self, Confirm, EmptyState, ErrorBox, Remote, Skeleton, Tone, confirm, drawer};

const KEYS: [&str; 2] = [crate::config::keys::SNAPSHOTS, crate::config::keys::SCHEDULER];
const FILTER: &str = "/usr/share/btrbk/scripts/ssh_filter_btrbk.sh";

/// `[a-z0-9][a-z0-9-]{0,23}`.
pub fn valid_target_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty() && b.len() <= 24 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The `authorized_keys` line of a remote target (same text as the server's).
pub fn authorized_keys_line(public_key: &str, path: &str, sudo: bool) -> String {
    if public_key.is_empty() {
        return String::new();
    }
    let sudo = if sudo { " --sudo" } else { "" };
    format!("command=\"{FILTER} --target --delete --restrict-path {path}{sudo} --log\",restrict {}", public_key.trim())
}

fn hint_key(h: ReplicaHint) -> &'static str {
    match h {
        ReplicaHint::SshKey => "repl.hint.ssh_key",
        ReplicaHint::HostKey => "repl.hint.host_key",
        ReplicaHint::Unreachable => "repl.hint.unreachable",
        ReplicaHint::Filter => "repl.hint.filter",
        ReplicaHint::TargetMissing => "repl.hint.target_missing",
        ReplicaHint::NotBtrfs => "repl.hint.not_btrfs",
        ReplicaHint::NoSpace => "repl.hint.no_space",
        ReplicaHint::Busy => "repl.hint.busy",
        ReplicaHint::Btrbk => "repl.hint.btrbk",
        ReplicaHint::Other => "repl.hint.other",
    }
}

fn warning_key(w: ReplicaWarning) -> &'static str {
    match w {
        ReplicaWarning::NotBtrfs => "repl.warn.not_btrfs",
        ReplicaWarning::SameFilesystem => "repl.warn.same_fs",
        ReplicaWarning::NotMounted => "repl.warn.not_mounted",
        ReplicaWarning::NoHostKey => "repl.warn.no_host_key",
    }
}

fn copy_text(s: String) {
    if crate::ui::copy_text(&s) {
        toast(Tone::Ok, t("repl.copied").to_string());
    }
}

/// A long value shown whole (wrapped, selectable) with a copy button.
fn copy_box(label: &'static str, value: String) -> AnyView {
    let v2 = value.clone();
    view! {
        <div class="field">
            <span class="field-label">{tr(label)}</span>
            <div class="repl-copy">
                <code class="secret mono">{value}</code>
                <button type="button" class="icon-btn sm" title=tr("repl.copy") aria-label=ui::verb_name("repl.copy", t(label)) on:click=move |_| copy_text(v2.clone())>
                    <Icon icon=icons::COPY size=16 />
                </button>
            </div>
        </div>
    }
    .into_any()
}

/// A host key as key type over its `SHA256:` fingerprint (two lines: the fingerprint never
/// pushes the drawer sideways on a phone).
fn key_item(kind: String, fingerprint: String) -> AnyView {
    view! {
        <li class="repl-key">
            <span class="muted">{kind}</span>
            <span class="mono repl-fp">{fingerprint}</span>
        </li>
    }
    .into_any()
}

/// A confirmed `known_hosts` line shown by its fingerprint (the raw line when it holds no
/// readable key).
fn fp_item(line: &str) -> AnyView {
    match crate::sshfp::of_line(line) {
        Some((kind, fp)) => key_item(kind, fp),
        None => view! { <li class="mono repl-fp">{line.to_string()}</li> }.into_any(),
    }
}

/// Run a task and toast its end.
fn run_task(url: String, body: Option<String>, ok: String, busy: RwSignal<bool>, error: RwSignal<Option<String>>, done: Callback<()>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = match api::start_task(&url, body).await {
            Ok(id) => tasks::follow(&id).await,
            Err(e) => Err(e),
        };
        let _ = busy.try_set(false);
        match r {
            Ok(task) if task.status == TaskStatus::Ok => {
                toast(Tone::Ok, ok);
                done.try_run(());
            }
            Ok(task) => {
                let _ = error.try_set(Some(tasks::task_error(&task)));
                toast(Tone::Err, tasks::task_error(&task));
                done.try_run(());
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
    });
}

// ------------------------------------------------------------------ panel

/// The tab's action in the page header: "Add replica target" (needs a snapshot job).
pub fn add_button(jobs: Remote<Vec<SnapshotJobView>>, edit: RwSignal<Option<Option<ReplicaView>>>) -> AnyView {
    let no_jobs = move || jobs.data.with(|l| l.as_ref().is_some_and(Vec::is_empty));
    view! {
        <button class="btn primary" disabled=no_jobs on:click=move |_| edit.set(Some(None))>
            <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("repl.add")}</span>
        </button>
    }
    .into_any()
}

pub fn panel(jobs: Remote<Vec<SnapshotJobView>>, edit: RwSignal<Option<Option<ReplicaView>>>) -> AnyView {
    let list = Remote::<Vec<ReplicaView>>::with_config("/api/v1/snapshots/replication");
    let replicas = RwSignal::new(None::<ReplicaView>);
    let del = RwSignal::new(None::<ReplicaView>);
    let running = RwSignal::new(None::<String>);
    let reload = Callback::new(move |_| list.reload());
    let no_jobs = move || jobs.data.with(|l| l.as_ref().is_some_and(Vec::is_empty));
    let table = move || {
        if let Some(v) = list.gate(4) {
            return v;
        }
        let rows = list.data.get().unwrap_or_default();
        if rows.is_empty() {
            let text = if no_jobs() { "repl.none_no_jobs" } else { "repl.none_text" };
            return view! { <EmptyState icon=icons::FOLDER_SYNC title=Signal::derive(|| t("repl.none").to_string()) text=Signal::derive(move || t(text).to_string()) /> }.into_any();
        }
        ui::table("", &["repl.col.target", "snap.col.job", "repl.col.when", "repl.col.last", "col.status", ""], rows.into_iter().map(|v| target_row(v, edit, replicas, del, running, reload)).collect::<Vec<_>>())
    };
    let delete_dialog = move || {
        del.get().map(|v| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            let url = format!("/api/v1/snapshots/replication/{}/{}", api::encode(&v.job), api::encode(&v.target.id));
            confirm(Confirm {
                title: tf("repl.delete_title", &[("id", &v.target.id), ("job", &v.job)]),
                text: t("repl.delete_text").to_string(),
                action: "delete",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    let url = url.clone();
                    busy.set(true);
                    spawn_local(async move {
                        match api::delete::<StagedChanges>(&url).await {
                            Ok(_) => {
                                if crate::config::apply_now(&KEYS, error).await {
                                    crate::toast::saved_applied();
                                    close.run(());
                                    reload.run(());
                                }
                            }
                            Err(e) => error.set(Some(e.text())),
                        }
                        let _ = busy.try_set(false);
                    });
                }),
                on_cancel: close,
            })
        })
    };
    view! {
        <div class="toolbar">
            <p class="muted small repl-intro">{tr("repl.intro")}</p>
            <button class="btn ghost" on:click=move |_| list.reload()><Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("snap.refresh")}</span></button>
        </div>
        {table}
        {move || edit.get().map(|v| editor(v, jobs, Callback::new(move |_| { edit.set(None); list.reload(); })))}
        {move || replicas.get().map(|v| replicas_drawer(v, Callback::new(move |_| replicas.set(None))))}
        {delete_dialog}
    }
    .into_any()
}

fn target_row(
    v: ReplicaView,
    edit: RwSignal<Option<Option<ReplicaView>>>,
    replicas: RwSignal<Option<ReplicaView>>,
    del: RwSignal<Option<ReplicaView>>,
    running: RwSignal<Option<String>>,
    reload: Callback<()>,
) -> AnyView {
    let key = format!("{}/{}", v.job, v.target.id);
    let local = v.target.destination.is_local();
    let s = v.status.clone();
    let when = if v.target.after_snapshots { t("repl.after_snapshots").to_string() } else { v.schedule.as_ref().map(trigger_text).unwrap_or_default() };
    // same wording and format as the Backup page; none for a paused target
    let next = v.next_run_ms.filter(|_| v.target.enabled).map(|n| tf("sch.next", &[("when", &i18n::short_when(n))]));
    let last = match s.last_ok_ms {
        Some(ms) => {
            let n = s.transferred.len();
            let sent = i18n::tn("repl.sent", n as u64, &[]);
            let sub = match s.duration_s {
                Some(d) => format!("{sent} · {}", i18n::duration(d)),
                None => sent,
            };
            ui::two_lines(i18n::short_when(ms).into_any(), Some(sub), "cell-sub")
        }
        None => view! { <span class="muted">{tr("repl.never")}</span> }.into_any(),
    };
    let status = {
        let mut chips: Vec<AnyView> = Vec::new();
        if !v.target.enabled {
            chips.push(ui::status(Tone::Unknown, t("repl.off").to_string()));
        } else if s.failing {
            chips.push(ui::status(Tone::Err, t("repl.failing").to_string()));
        } else if s.last_ok_ms.is_some() {
            chips.push(ui::status(Tone::Ok, t("repl.ok").to_string()));
        } else {
            chips.push(ui::status(Tone::Unknown, t("never_run").to_string()));
        }
        if let Some(p) = ui::pending_status(v.pending) {
            chips.push(p);
        }
        // one explanation: the fix when it is known, else btrbk's or ssh's words; the
        // configuration warnings only while nothing is failing
        let hint = s.failing.then(|| s.hint.unwrap_or(ReplicaHint::Other));
        let known = hint.filter(|h| *h != ReplicaHint::Other || s.error.is_none());
        let err = s.error.clone().filter(|_| s.failing && known.is_none());
        let held = s.held_snapshots;
        let warnings = if s.failing { Vec::new() } else { v.warnings.clone() };
        view! {
            <div class="cell-main" title=s.error.clone().unwrap_or_default()>
                <div class="chips">{chips}</div>
                {known.map(|h| view! { <span class="cell-sub">{ui::text_code(t(hint_key(h)))}</span> })}
                {err.map(|e| view! { <span class="cell-sub mono small danger-text bk-wrap">{e}</span> })}
                {(held > 0).then(|| view! { <span class="cell-sub warn-text">{i18n::tn("repl.held", u64::from(held), &[])}</span> })}
                {warnings.into_iter().map(|w| view! { <span class="cell-sub warn-text">{tr(warning_key(w))}</span> }).collect_view()}
            </div>
        }
        .into_any()
    };
    let (v1, v2, v3) = (v.clone(), v.clone(), v.clone());
    let (job, id) = (v.job.clone(), v.target.id.clone());
    let busy_key = key.clone();
    let is_busy = move || running.get().as_deref() == Some(busy_key.as_str());
    let run = move |_| {
        let url = format!("/api/v1/snapshots/replication/{}/{}/run", api::encode(&job), api::encode(&id));
        let key = key.clone();
        running.set(Some(key));
        let busy = RwSignal::new(true);
        let error = RwSignal::new(None::<String>);
        run_task(url, None, t("repl.run_done").to_string(), busy, error, Callback::new(move |_| {
            running.set(None);
            reload.run(());
        }));
        Effect::new(move |_| {
            if !busy.get() {
                running.set(None);
            }
        });
    };
    let applied = v.pending != Some(rynas_api_types::PendingOp::Add);
    let can_run = v.target.enabled && applied;
    let icon = if local { icons::HARD_DRIVE } else { icons::SERVER };
    ui::row_class(
        vec![
            ui::cell("repl.col.target", "", view! {
                <div class="cell-main">
                    <span class="entry-name"><Icon icon=icon size=16 /><span class="strong">{v.target.id.clone()}</span></span>
                    <span class="cell-sub mono">{v.url.clone()}</span>
                </div>
            }
            .into_any()),
            ui::cell("snap.col.job", "", ui::two_lines(v.job.clone().into_any(), v.share.clone().map(|sh| tf("snap.share_of", &[("share", &sh)])), "cell-sub")),
            ui::cell("repl.col.when", "", ui::two_lines(when.into_any(), next, "cell-sub")),
            ui::cell("repl.col.last", "tnum", last),
            ui::cell("col.status", "", status),
            ui::actions(view! {
                <button class="icon-btn sm" title=tr("run_now") aria-label=ui::verb_name("run_now", &v.target.id) disabled=move || !can_run || is_busy() on:click=run>
                    <Icon icon=icons::PLAY size=16 />
                </button>
                <button class="icon-btn sm" title=move || if local { t("repl.replicas") } else { t("repl.replicas_remote") } aria-label=ui::verb_name("repl.replicas", &v.target.id) disabled=!local || !applied on:click=move |_| replicas.set(Some(v1.clone()))>
                    <Icon icon=icons::FOLDER_OPEN size=16 />
                </button>
                <button class="icon-btn sm" title=tr("edit") aria-label=ui::verb_name("edit", &v.target.id) on:click=move |_| edit.set(Some(Some(v2.clone())))>
                    <Icon icon=icons::PENCIL size=16 />
                </button>
                <button class="icon-btn sm danger" title=tr("delete") aria-label=ui::verb_name("delete", &v.target.id) on:click=move |_| del.set(Some(v3.clone()))>
                    <Icon icon=icons::TRASH_2 size=16 />
                </button>
            }
            .into_any()),
        ],
        if !v.target.enabled { "muted-row" } else { "" },
    )
}

// ------------------------------------------------------------------ editor

const KINDS: &[(&str, &str)] = &[("local", "repl.kind.local"), ("ssh", "repl.kind.ssh")];
const COMPRESS: &[(&str, &str)] = &[("no", "repl.compress.no"), ("zstd", "repl.compress.zstd"), ("lz4", "repl.compress.lz4"), ("gzip", "repl.compress.gzip")];

/// The form's flat draft.
#[derive(Clone, Debug, PartialEq)]
struct Form {
    job: String,
    id: String,
    kind: String,
    path: String,
    host: String,
    port: String,
    user: String,
    sudo: bool,
    host_keys: Vec<String>,
    retention: Retention,
    compress: String,
    rate: String,
    enabled: bool,
    after: bool,
}

impl Form {
    fn of(t: &ReplicaTarget, job: &str) -> Self {
        let (kind, path, host, port, user, sudo, host_keys) = match &t.destination {
            ReplicaDestination::Local { path } => ("local", path.clone(), String::new(), "22".into(), "root".into(), false, Vec::new()),
            ReplicaDestination::Ssh { host, port, user, path, sudo, host_keys } => ("ssh", path.clone(), host.clone(), port.to_string(), user.clone(), *sudo, host_keys.clone()),
        };
        Form {
            job: job.into(),
            id: t.id.clone(),
            kind: kind.into(),
            path,
            host,
            port,
            user,
            sudo,
            host_keys,
            retention: t.retention.clone(),
            compress: t.compress_code().into(),
            rate: if t.rate_limit_kib > 0 { t.rate_limit_kib.to_string() } else { String::new() },
            enabled: t.enabled,
            after: t.after_snapshots,
        }
    }

    fn target(&self) -> Result<ReplicaTarget, &'static str> {
        if !valid_target_id(&self.id) {
            return Err("repl.v.id");
        }
        if !self.path.starts_with('/') || self.path == "/" {
            return Err("repl.v.path");
        }
        let destination = if self.kind == "local" {
            ReplicaDestination::Local { path: self.path.trim().into() }
        } else {
            if self.host.trim().is_empty() {
                return Err("repl.v.host");
            }
            let port: u16 = self.port.trim().parse().ok().filter(|p| *p > 0).ok_or("repl.v.port")?;
            ReplicaDestination::Ssh {
                host: self.host.trim().into(),
                port,
                user: if self.user.trim().is_empty() { "root".into() } else { self.user.trim().into() },
                path: self.path.trim().into(),
                sudo: self.sudo,
                host_keys: self.host_keys.clone(),
            }
        };
        let rate = match self.rate.trim() {
            "" => 0,
            r => r.parse().map_err(|_| "repl.v.rate")?,
        };
        Ok(ReplicaTarget {
            id: self.id.clone(),
            enabled: self.enabled,
            destination,
            retention: self.retention.clone(),
            compress: match self.compress.as_str() {
                "zstd" => StreamCompress::Zstd,
                "lz4" => StreamCompress::Lz4,
                "gzip" => StreamCompress::Gzip,
                _ => StreamCompress::No,
            },
            rate_limit_kib: rate,
            after_snapshots: self.after,
        })
    }
}

trait CompressCode {
    fn compress_code(&self) -> &'static str;
}

impl CompressCode for ReplicaTarget {
    fn compress_code(&self) -> &'static str {
        self.compress.btrbk()
    }
}

fn editor(init: Option<ReplicaView>, jobs: Remote<Vec<SnapshotJobView>>, close: Callback<()>) -> AnyView {
    let is_new = init.is_none();
    let first_job = jobs.data.with_untracked(|l| l.iter().flatten().map(|j| j.job.name.clone()).next().unwrap_or_default());
    let start = match &init {
        Some(v) => Form::of(&v.target, &v.job),
        None => {
            let t = ReplicaTarget {
                id: String::new(),
                enabled: true,
                destination: ReplicaDestination::Local { path: String::new() },
                retention: default_target_retention(),
                compress: StreamCompress::Zstd,
                rate_limit_kib: 0,
                after_snapshots: true,
            };
            Form::of(&t, &first_job)
        }
    };
    let schedule = init.as_ref().and_then(|v| v.schedule.clone());
    let trig = TriggerEdit::new(&schedule.unwrap_or(TaskTrigger::Calendar { on_calendar: "daily".into(), cron: None, persistent: true, randomized_delay_s: 0 }));
    let d = RwSignal::new(start);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let key = RwSignal::new(None::<ReplicationKey>);
    let scanned = RwSignal::new(None::<Result<Vec<HostKey>, String>>);
    let scanning = RwSignal::new(false);
    let none = Signal::derive(|| None::<String>);
    spawn_local(async move {
        if let Ok(k) = api::get::<ReplicationKey>("/api/v1/snapshots/replication/ssh-key").await {
            let _ = key.try_set(Some(k));
        }
    });

    let (id, set_id) = lens(d, |f| f.id.clone(), |f, v| f.id = v.trim().to_string());
    let id_err = Signal::derive(move || {
        let v = id.get();
        (!v.is_empty() && !valid_target_id(&v)).then(|| t("repl.v.id").to_string())
    });
    let (kind, set_kind) = lens(d, |f| f.kind.clone(), |f, v| f.kind = v);
    let (path, set_path) = lens(d, |f| f.path.clone(), |f, v| f.path = v.trim().to_string());
    let (host, set_host) = lens(d, |f| f.host.clone(), |f, v| {
        if f.host != v.trim() {
            f.host_keys.clear();
        }
        f.host = v.trim().to_string()
    });
    let (port, set_port) = lens(d, |f| f.port.clone(), |f, v| {
        if f.port != v.trim() {
            f.host_keys.clear();
        }
        f.port = v.trim().to_string()
    });
    let (user, set_user) = lens(d, |f| f.user.clone(), |f, v| f.user = v.trim().to_string());
    let (compress, set_compress) = lens(d, |f| f.compress.clone(), |f, v| f.compress = v);
    let (rate, set_rate) = lens(d, |f| f.rate.clone(), |f, v| f.rate = v.trim().to_string());
    let count = |get: fn(&Retention) -> u32, set: fn(&mut Retention, u32)| {
        (Signal::derive(move || d.with(|f| get(&f.retention).to_string())), Callback::new(move |v: String| d.update(|f| set(&mut f.retention, v.trim().parse().unwrap_or(0)))))
    };
    let (dd, set_dd) = count(|r| r.daily, |r, v| r.daily = v);
    let (w, set_w) = count(|r| r.weekly, |r, v| r.weekly = v);
    let (m, set_m) = count(|r| r.monthly, |r, v| r.monthly = v);
    let (y, set_y) = count(|r| r.yearly, |r, v| r.yearly = v);
    let (h, set_h) = count(|r| r.hourly, |r, v| r.hourly = v);
    let (min, set_min) = lens(d, |f| f.retention.min.clone(), |f, v| f.retention.min = v.trim().to_string());
    let is_ssh = Memo::new(move |_| d.with(|f| f.kind == "ssh"));

    let job_select = move || {
        let cur = d.with(|f| f.job.clone());
        let names: Vec<String> = jobs.data.with(|l| l.iter().flatten().map(|j| j.job.name.clone()).collect());
        view! {
            <div class="field">
                <label class="field-label" for="repl-job">{tr("snap.col.job")}</label>
                <select id="repl-job" class="input" disabled=!is_new on:change=move |ev| { let v = event_target_value(&ev); d.update(|f| f.job = v); }>
                    {names.into_iter().map(|n| { let sel = n == cur; view! { <option value=n.clone() selected=sel>{n.clone()}</option> } }).collect_view()}
                </select>
                <span class="field-help">{tr("repl.f.job_help")}</span>
            </div>
        }
    };
    let scan = move |_| {
        let (host, port) = d.with_untracked(|f| (f.host.clone(), f.port.trim().parse::<u16>().unwrap_or(22)));
        if host.is_empty() {
            scanned.set(Some(Err(t("repl.v.host").to_string())));
            return;
        }
        scanning.set(true);
        spawn_local(async move {
            let r = api::post::<_, Vec<HostKey>>("/api/v1/snapshots/replication/host-keys", Some(&HostKeyRequest { host, port })).await;
            let _ = scanning.try_set(false);
            let _ = scanned.try_set(Some(r.map_err(|e| e.text())));
        });
    };
    let host_key_view = move || {
        let trusted = d.with(|f| f.host_keys.clone());
        let found = scanned.get();
        view! {
            <div class="field">
                <span class="field-label">{tr("repl.f.host_keys")}</span>
                {if trusted.is_empty() {
                    view! { <p class="field-help warn-text">{tr("repl.f.host_keys_none")}</p> }.into_any()
                } else {
                    view! { <ul class="plain-list small repl-keys">{trusted.iter().map(|k| fp_item(k)).collect_view()}</ul> }.into_any()
                }}
                <div>
                    <button type="button" class="btn ghost sm" disabled=move || scanning.get() on:click=scan>
                        {crate::form::spinner(scanning)}<Icon icon=icons::KEY size=14 />{tr("repl.f.scan")}
                    </button>
                </div>
                {found.map(|r| match r {
                    Ok(keys) if !keys.is_empty() => {
                        let lines: Vec<String> = keys.iter().map(|k| k.line.clone()).collect();
                        view! {
                            <div class="ret-preview" aria-live="polite">
                                <p class="small">{ui::text_code(t("repl.f.scan_check"))}</p>
                                <ul class="plain-list small repl-keys">{keys.into_iter().map(|k| key_item(k.line.split_whitespace().nth(1).unwrap_or("").to_string(), k.fingerprint)).collect_view()}</ul>
                                <div><button type="button" class="btn sm" on:click=move |_| { let l = lines.clone(); d.update(|f| f.host_keys = l); scanned.set(None); }>
                                    <Icon icon=icons::CHECK size=14 />{tr("repl.f.trust")}
                                </button></div>
                            </div>
                        }
                        .into_any()
                    }
                    Ok(_) => view! { <p class="form-error">{tr("repl.f.scan_none")}</p> }.into_any(),
                    Err(e) => view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{e}</p> }.into_any(),
                })}
            </div>
        }
    };
    let make_key = move |_| {
        spawn_local(async move {
            match api::post::<(), ReplicationKey>("/api/v1/snapshots/replication/ssh-key", None).await {
                Ok(k) => {
                    let _ = key.try_set(Some(k));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let key_view = move || {
        let k = key.get().unwrap_or_default();
        if k.public_key.is_empty() {
            return view! {
                <div class="field">
                    <p class="field-help">{tr("repl.key.none")}</p>
                    <div><button type="button" class="btn sm" on:click=make_key><Icon icon=icons::KEY size=14 />{tr("repl.key.create")}</button></div>
                </div>
            }
            .into_any();
        }
        let (p, sudo, user) = d.with(|f| (f.path.clone(), f.sudo, f.user.clone()));
        let home = if user == "root" || user.is_empty() { "/root".to_string() } else { format!("/home/{user}") };
        // the line restricts the key to the target folder: without it there is nothing to copy
        let line_view = if p.is_empty() {
            view! { <p class="field-help warn-text">{tr("repl.key.line_needs_path")}</p> }.into_any()
        } else {
            copy_box("repl.key.line", authorized_keys_line(&k.public_key, &p, sudo))
        };
        view! {
            {copy_box("repl.key.public", k.public_key.clone())}
            {(!k.fingerprint.is_empty()).then(|| view! { <p class="field-help mono repl-fp">{k.fingerprint.clone()}</p> })}
            {line_view}
            {(!p.is_empty()).then(|| view! { <p class="field-help">{ui::text_code(&tf("repl.key.line_help", &[("file", &format!("{home}/.ssh/authorized_keys"))]))}</p> })}
            {ui::note(Tone::Warn, icons::SHIELD_ALERT, view! { {ui::text_code(t("repl.key.filter_version"))} }.into_any())}
            {sudo.then(|| view! { <p class="field-help">{ui::text_code(&tf("repl.key.sudo_help", &[("user", &user)]))}</p> })}
        }
        .into_any()
    };

    let save = Callback::new(move |_| {
        let f = d.get_untracked();
        let target = match f.target() {
            Ok(t) => t,
            Err(k) => {
                error.set(Some(t(k).to_string()));
                return;
            }
        };
        let schedule = if f.after {
            None
        } else {
            match trig.trigger() {
                Ok(s) => Some(s),
                Err(k) => {
                    error.set(Some(t(k).to_string()));
                    return;
                }
            }
        };
        let req = ReplicaRequest { job: f.job.clone(), target, schedule };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = if is_new {
                api::post::<_, ReplicaView>("/api/v1/snapshots/replication", Some(&req)).await.map(|_| ())
            } else {
                api::put::<_, serde_json::Value>(&format!("/api/v1/snapshots/replication/{}/{}", api::encode(&req.job), api::encode(&req.target.id)), &req).await.map(|_| ())
            };
            match r {
                Ok(()) => {
                    if crate::config::apply_now(&KEYS, error).await {
                        crate::toast::saved_applied();
                        close.run(());
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    });

    let title = {
        let n = d.with_untracked(|f| f.id.clone());
        Signal::derive(move || if is_new { t("repl.add").to_string() } else { tf("edit_named", &[("name", &n)]) })
    };
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            <h3 class="form-section">{tr("repl.sec.target")}</h3>
            {job_select}
            {is_new.then(|| text_field("repl.f.id", id, set_id, id_err, In { mono: true, help: Some("repl.f.id_help"), maxlength: 24, ..In::default() }))}
            {segmented("repl.f.kind", KINDS, kind, set_kind)}
            {move || is_ssh.get().then(|| view! {
                <div class="form-row three">
                    {text_field("repl.f.host", host, set_host, none, In { mono: true, placeholder: "192.168.1.20", ..In::default() })}
                    {number_field("repl.f.port", None, port, set_port, 1, 65535)}
                    {text_field("repl.f.user", user, set_user, none, In { mono: true, help: Some("repl.f.user_help"), maxlength: 32, ..In::default() })}
                </div>
            })}
            {move || {
                let (help, placeholder) = if is_ssh.get() { ("repl.f.path_remote_help", "/srv/backup/replicas/docs") } else { ("repl.f.path_local_help", "/srv/vol2/.replicas/docs") };
                text_field("repl.f.path", path, set_path, none, In { mono: true, help: Some(help), placeholder, ..In::default() })
            }}
            {move || is_ssh.get().then(|| view! {
                {switch("repl.f.sudo", Some("repl.f.sudo_help"), Signal::derive(move || d.with(|f| f.sudo)), Callback::new(move |v| d.update(|f| f.sudo = v)))}
                {host_key_view}
                <h3 class="form-section">{tr("repl.sec.key")}</h3>
                {key_view}
            })}
            <h3 class="form-section">{tr("sch.sec.schedule")}</h3>
            {switch("repl.f.enabled", None, Signal::derive(move || d.with(|f| f.enabled)), Callback::new(move |v| d.update(|f| f.enabled = v)))}
            {switch("repl.f.after", Some("repl.f.after_help"), Signal::derive(move || d.with(|f| f.after)), Callback::new(move |v| d.update(|f| f.after = v)))}
            {move || (!d.with(|f| f.after)).then(|| trig.view(true))}
            <h3 class="form-section">{tr("repl.sec.retention")}</h3>
            <p class="field-help">{tr("repl.f.retention_help")}</p>
            <div class="form-row three">
                {number_field("snap.f.hourly", None, h, set_h, 0, 10000)}
                {number_field("snap.f.daily", None, dd, set_dd, 0, 10000)}
                {number_field("snap.f.weekly", None, w, set_w, 0, 10000)}
                {number_field("snap.f.monthly", None, m, set_m, 0, 10000)}
                {number_field("snap.f.yearly", None, y, set_y, 0, 1000)}
                {text_field("snap.f.min", min, set_min, none, In { mono: true, help: Some("snap.f.min_help"), ..In::default() })}
            </div>
            <p class="field-help">{move || tf("snap.preset_keeps", &[("counts", &d.with(|f| retention_text(&f.retention))), ("min", &d.with(|f| f.retention.min.clone()))])}</p>
            <h3 class="form-section">{tr("repl.sec.transfer")}</h3>
            {move || is_ssh.get().then(|| segmented("repl.f.compress", COMPRESS, compress, set_compress))}
            {number_field("repl.f.rate", Some("repl.f.rate_help"), rate, set_rate, 0, 10_000_000)}
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

// ------------------------------------------------------------------ replicas of a local target

fn replicas_drawer(v: ReplicaView, close: Callback<()>) -> AnyView {
    let base = StoredValue::new(format!("/api/v1/snapshots/replication/{}/{}", api::encode(&v.job), api::encode(&v.target.id)));
    let items = RwSignal::new(None::<Result<Vec<ReplicaSnapshot>, String>>);
    let open = RwSignal::new(None::<String>);
    let promote = RwSignal::new(None::<String>);
    let job = StoredValue::new(v.job.clone());
    let load = move || {
        let url = format!("{}/snapshots", base.get_value());
        spawn_local(async move {
            let r = api::get::<Vec<ReplicaSnapshot>>(&url).await;
            let _ = items.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    };
    load();
    let list = move || match items.get() {
        None => view! { <Skeleton lines=5 /> }.into_any(),
        Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
        Some(Ok(l)) if l.is_empty() => view! { <p class="muted table-none">{tr("repl.no_replicas")}</p> }.into_any(),
        Some(Ok(l)) => ui::table("", &["repl.col.replica", "snap.col.created", "col.status", ""], l.into_iter().map(|s| {
            let (n1, n2) = (s.name.clone(), s.name.clone());
            let ok = !s.incomplete;
            ui::row(vec![
                ui::cell("repl.col.replica", "", view! { <button class="link-strong mono" disabled=!ok on:click=move |_| open.set(Some(n1.clone()))>{s.name.clone()}</button> }.into_any()),
                ui::cell("snap.col.created", "tnum", i18n::datetime(s.created_ms).into_any()),
                ui::cell("col.status", "", if ok { view! { <span class="chip"><Icon icon=icons::LOCK size=12 />{tr("snap.readonly")}</span> }.into_any() } else { ui::status(Tone::Warn, t("repl.incomplete").to_string()) }),
                ui::actions(view! {
                    <button class="icon-btn sm" title=tr("snap.browse") aria-label=ui::verb_name("snap.browse", &s.name) disabled=!ok on:click={ let n = s.name.clone(); move |_| open.set(Some(n.clone())) }>
                        <Icon icon=icons::FOLDER_OPEN size=16 />
                    </button>
                    <button class="icon-btn sm" title=tr("repl.promote") aria-label=ui::verb_name("repl.promote", &s.name) disabled=!ok on:click=move |_| promote.set(Some(n2.clone()))>
                        <Icon icon=icons::FOLDER_PLUS size=16 />
                    </button>
                }
                .into_any()),
            ])
        }).collect::<Vec<_>>()),
    };
    let promote_dialog = move || {
        promote.get().map(|name| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let cancel = Callback::new(move |_| promote.set(None));
            let folder = RwSignal::new(format!("{}-restored", job.get_value()));
            let n2 = name.clone();
            let extra = view! {
                {text_field("bk.mode.new", folder.into(), Callback::new(move |v: String| folder.set(v.trim().to_string())), Signal::derive(|| None::<String>), In { mono: true, help: Some("repl.f.folder_help"), maxlength: 64, ..In::default() })}
            }
            .into_any();
            confirm(Confirm {
                title: tf("repl.promote_title", &[("name", &name)]),
                text: t("repl.promote_text").to_string(),
                action: "repl.promote",
                danger: false,
                extra: Some(extra),
                // not destructive (a new folder, the replica stays): a plain confirmation
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    let body = serde_json::to_string(&PromoteRequest { folder: folder.get_untracked(), confirm: n2.clone() }).ok();
                    let url = format!("{}/snapshots/{}/promote", base.get_value(), api::encode(&n2));
                    run_task(url, body, tf("repl.promoted", &[("folder", &folder.get_untracked())]), busy, error, Callback::new(move |_| promote.set(None)));
                }),
                on_cancel: cancel,
            })
        })
    };
    let body = view! {
        <p class="field-help">{tf("repl.replicas_text", &[("path", v.target.destination.path())])}</p>
        {list}
        {promote_dialog}
        {move || open.get().map(|n| browse_replica(base.get_value(), n, Callback::new(move |_| open.set(None))))}
    }
    .into_any();
    let foot = ui::close_foot(close);
    let title = Signal::stored(tf("repl.replicas_title", &[("id", &v.target.id), ("job", &v.job)]));
    view! { {ui::drawer_wide(title, close, body, foot)} }.into_any()
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

/// The content of one replica, read-only, with "Restore" (a copy next to the original).
fn browse_replica(base: String, name: String, close: Callback<()>) -> AnyView {
    let path = RwSignal::new(String::new());
    let listing = RwSignal::new(None::<Result<ReplicaListing, String>>);
    let restore = RwSignal::new(None::<String>);
    let base = StoredValue::new(base);
    let n = StoredValue::new(name.clone());
    Effect::new(move |_| {
        let url = format!("{}/snapshots/{}/browse?path={}", base.get_value(), api::encode(&n.get_value()), api::encode(&path.get()));
        listing.set(None);
        spawn_local(async move {
            let r = api::get::<ReplicaListing>(&url).await;
            let _ = listing.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    });
    let entries = move || match listing.get() {
        None => view! { <Skeleton lines=6 /> }.into_any(),
        Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| path.update(|_| ())) /> }.into_any(),
        Some(Ok(l)) if l.entries.is_empty() => view! { <p class="muted table-none">{tr("snap.empty_dir")}</p> }.into_any(),
        Some(Ok(l)) => {
            let dir = l.path.clone();
            ui::table("", &["snap.col.name", "#snap.col.size", "snap.col.modified", ""], l.entries.into_iter().map(|e| {
                let rel = join(&dir, &e.name);
                let is_dir = e.kind == EntryKind::Dir;
                let (r1, r2) = (rel.clone(), rel);
                let label = ui::verb_name("snap.restore", &e.name);
                ui::row(vec![
                    ui::cell("snap.col.name", "", view! {
                        <span class="entry-name">
                            <Icon icon=if is_dir { icons::FOLDER } else { icons::FILE } size=16 />
                            {if is_dir { view! { <button class="link-strong" on:click=move |_| path.set(r1.clone())>{e.name.clone()}</button> }.into_any() } else { view! { <span>{e.name.clone()}</span> }.into_any() }}
                        </span>
                    }
                    .into_any()),
                    ui::cell("snap.col.size", "num tnum", view! { {(!is_dir).then(|| i18n::bytes(e.size_bytes))} }.into_any()),
                    ui::cell("snap.col.modified", "tnum", i18n::datetime(e.mtime_ms).into_any()),
                    ui::actions(view! {
                        <button class="icon-btn sm" title=tr("snap.restore") aria-label=label disabled=!matches!(e.kind, EntryKind::Dir | EntryKind::File | EntryKind::Symlink) on:click=move |_| restore.set(Some(r2.clone()))>
                            <Icon icon=icons::ARCHIVE_RESTORE size=16 />
                        </button>
                    }
                    .into_any()),
                ])
            }).collect::<Vec<_>>())
        }
    };
    let restore_dialog = move || {
        restore.get().map(|rel| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let rel2 = rel.clone();
            confirm(Confirm {
                title: tf("repl.restore_title", &[("path", &rel)]),
                text: t("repl.restore_text").to_string(),
                action: "snap.restore",
                danger: false,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    let body = serde_json::to_string(&ReplicaRestoreRequest { path: rel2.clone() }).ok();
                    let url = format!("{}/snapshots/{}/restore", base.get_value(), api::encode(&n.get_value()));
                    run_task(url, body, tf("snap.restored", &[("path", &rel2)]), busy, error, Callback::new(move |_| restore.set(None)));
                }),
                on_cancel: Callback::new(move |_| restore.set(None)),
            })
        })
    };
    let up = move |_| path.update(|p| *p = p.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default());
    view! {
        <div class="repl-browse">
            <div class="toolbar browse-bar">
                <button class="icon-btn sm" title=tr("repl.back") aria-label=tr("repl.back") on:click=move |_| close.run(())><Icon icon=icons::ARROW_LEFT size=16 /></button>
                <button class="icon-btn sm" title=tr("snap.up") aria-label=tr("snap.up") disabled=move || path.with(String::is_empty) on:click=up><Icon icon=icons::ARROW_UP size=16 /></button>
                <span class="mono small ellipsis-cell">{move || if path.get().is_empty() { name.clone() } else { format!("{name}/{}", path.get()) }}</span>
            </div>
            {entries}
            {restore_dialog}
        </div>
    }
    .into_any()
}
