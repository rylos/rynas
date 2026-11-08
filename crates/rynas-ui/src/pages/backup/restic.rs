//! Backup to another NAS, a disk or the cloud (D-BACKUP, `/api/v1/backup/restic/…`):
//! encrypted, deduplicated, versioned restic-format repositories.
//!
//! - The job list: destination, schedule and next run, last result, repository size and
//!   versions; Back up now, versions, integrity check, edit, delete.
//! - The job editor (one drawer, in the order of the questions): what (shares, the rynas
//!   configuration, other folders), where (destination type and its fields, a connection
//!   test), when, how many versions to keep, the encryption password, options.
//! - A generated repository password is shown once, with the warning that without it the
//!   backup cannot be read; the dialog closes only after "I have saved it".
//! - Versions: the snapshots of the repository, their folders, restore of files and folders
//!   to the original place or to another folder, download of one file.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::backup::{
    BackupJob, BackupJobCreated, BackupJobRequest, BackupJobView, BackupSource, CheckRequest, Destination, DestinationSecrets, Maintenance,
    NodeEntry, RestoreRequest, RestoreTarget, Retention, SnapshotEntry, SshKeyCreated, TestOutcome, TestRequest, TestResult,
};
use rynas_api_types::scheduler::TaskTrigger;
use rynas_api_types::{ShareView, TaskStatus};

use crate::api;
use crate::form::{In, area_field, number_field, save_bar, segmented, select_field, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::scheduler::{TriggerEdit, run_status, trigger_text};
use crate::tasks;
use crate::toast::{saved_applied, saved_pending, toast};
use crate::ui::{self, Confirm, DlgKind, EmptyState, Remote, Tone, confirm, dialog, drawer};

const JOBS: &str = "/api/v1/backup/restic/jobs";

fn job_url(name: &str, rest: &str) -> String {
    format!("{JOBS}/{}{rest}", api::encode(name))
}

/// `[a-z0-9][a-z0-9-]{0,31}`.
fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 32 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn kind_key(d: &Destination) -> &'static str {
    match d {
        Destination::Local { .. } => "bk.dest.local",
        Destination::Rest { .. } => "bk.dest.rest",
        Destination::Sftp { .. } => "bk.dest.sftp",
        Destination::S3 { .. } => "bk.dest.s3",
        Destination::B2 { .. } => "bk.dest.b2",
        Destination::Rclone { .. } => "bk.dest.rclone",
    }
}

/// "7 d · 4 w · 12 m".
fn retention_text(r: &Retention) -> String {
    let parts: Vec<String> = [(r.last, "bk.u.last"), (r.hourly, "bk.u.h"), (r.daily, "bk.u.d"), (r.weekly, "bk.u.w"), (r.monthly, "bk.u.m"), (r.yearly, "bk.u.y")]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, k)| format!("{n}\u{a0}{}", t(k)))
        .collect();
    parts.join(" · ")
}

fn source_text(s: &BackupSource) -> String {
    match s {
        BackupSource::Share { name } => name.clone(),
        BackupSource::Folder { path } => path.clone(),
        BackupSource::Config => t("bk.src.config").to_string(),
    }
}

pub fn create_button(edit: RwSignal<Option<Option<BackupJobView>>>) -> AnyView {
    view! {
        <button class="btn primary" on:click=move |_| edit.set(Some(None))>
            <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("bk.create")}</span>
        </button>
    }
    .into_any()
}

/// The tab: the job list and its dialogs.
pub fn panel(edit: RwSignal<Option<Option<BackupJobView>>>) -> AnyView {
    let jobs = Remote::<Vec<BackupJobView>>::with_config(JOBS);
    let del = RwSignal::new(None::<String>);
    let versions = RwSignal::new(None::<String>);
    let check = RwSignal::new(None::<String>);
    let shown_password = RwSignal::new(None::<String>);
    let table = move || {
        if let Some(v) = jobs.gate(4) {
            return v;
        }
        let rows = jobs.data.get().unwrap_or_default();
        if rows.is_empty() {
            return view! {
                <EmptyState icon=icons::ARCHIVE_RESTORE title=Signal::derive(|| t("bk.empty").to_string()) text=Signal::derive(|| t("bk.empty_text").to_string())>
                    <button class="btn primary" on:click=move |_| edit.set(Some(None))><Icon icon=icons::PLUS size=16 />{tr("bk.create")}</button>
                </EmptyState>
            }
            .into_any();
        }
        let offsite = rows.iter().any(|v| v.job.destination.offsite());
        view! {
            {(!offsite).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <span>{tr("bk.no_offsite")}</span> }.into_any()))}
            {ui::table("", &["bk.col.job", "bk.col.where", "sch.col.schedule", "sch.col.last", "bk.col.size", ""], rows.into_iter().map(|v| job_row(v, jobs, edit, del, versions, check)).collect::<Vec<_>>())}
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
                title: tf("bk.delete", &[("name", &name)]),
                text: t("bk.delete_text").to_string(),
                action: "delete",
                danger: true,
                // the repository password goes with the job: without a copy the versions left
                // at the destination cannot be read
                extra: Some(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <span>{tr("bk.delete_password")}</span> }.into_any())),
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    busy.set(true);
                    let n = n.clone();
                    spawn_local(async move {
                        let r = api::delete_empty(&job_url(&n, "")).await;
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
        <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("bk.help")}</p>
        {table}
        {deleter}
        {move || edit.get().map(|v| job_editor(v, shown_password, Callback::new(move |_| edit.set(None))))}
        {move || versions.get().map(|n| versions_drawer(n, Callback::new(move |_| versions.set(None))))}
        {move || check.get().map(|n| check_dialog(n, jobs, Callback::new(move |_| check.set(None))))}
        {move || shown_password.get().map(|pw| password_dialog(pw, Callback::new(move |_| shown_password.set(None))))}
    }
    .into_any()
}

fn job_row(
    v: BackupJobView,
    jobs: Remote<Vec<BackupJobView>>,
    edit: RwSignal<Option<Option<BackupJobView>>>,
    del: RwSignal<Option<String>>,
    versions: RwSignal<Option<String>>,
    check: RwSignal<Option<String>>,
) -> AnyView {
    let name = v.job.name.clone();
    let deleting = v.pending == Some(rynas_api_types::PendingOp::Delete);
    let not_applied = v.pending == Some(rynas_api_types::PendingOp::Add);
    let running = RwSignal::new(v.running_task.is_some());
    let n2 = name.clone();
    let run = move |_| {
        running.set(true);
        let n = n2.clone();
        spawn_local(async move {
            let r = api::start_task(&job_url(&n, "/run"), None).await;
            match r {
                Ok(id) => {
                    toast(Tone::Info, tf("bk.run.started", &[("name", &n)]));
                    let done = tasks::follow(&id).await;
                    running.try_set(false);
                    match done {
                        Ok(task) if task.status == TaskStatus::Ok => toast(Tone::Ok, tf("bk.run.ok", &[("name", &n)])),
                        Ok(task) => toast(Tone::Err, tf("bk.run.failed", &[("name", &n), ("why", &tasks::task_error(&task))])),
                        Err(_) => {}
                    }
                    jobs.reload();
                }
                Err(e) => {
                    running.set(false);
                    toast(Tone::Err, e.text());
                }
            }
        });
    };
    let st = v.status.clone();
    let warnings = st.last_warnings.clone();
    let last = match (&v.last_run, &st.last_error) {
        // rynasd stopped in the middle of a run and put the system back at its next start
        (_, Some(err)) if err.starts_with("interrupted") => {
            let when = st.last_end_ms.map(i18n::short_when).unwrap_or_default();
            view! { <div class="cell-main">{ui::status(Tone::Warn, t("bk.interrupted").to_string())}<span class="cell-sub bk-wrap" title=err.clone()>{tf("bk.interrupted_text", &[("when", &when)])}</span></div> }.into_any()
        }
        (_, Some(err)) if st.last_end_ms.is_some_and(|e| Some(e) != st.last_ok_ms) => {
            let when = st.last_end_ms.map(i18n::short_when).unwrap_or_default();
            view! { <div class="cell-main">{ui::status(Tone::Err, t("bk.failed").to_string())}<span class="cell-sub bk-err" title=err.clone()>{format!("{when} · {err}")}</span></div> }.into_any()
        }
        (Some(r), _) => {
            let (tone, text) = run_status(r);
            view! { <div class="cell-main">{ui::status(tone, text)}<span class="cell-sub tnum">{i18n::short_when(r.end_ms)}</span></div> }.into_any()
        }
        (None, _) => match st.last_ok_ms {
            Some(ms) => view! { <div class="cell-main">{ui::status(Tone::Ok, t("sch.run.ok").to_string())}<span class="cell-sub tnum">{i18n::short_when(ms)}</span></div> }.into_any(),
            None => view! { <span class="muted">{tr("bk.never")}</span> }.into_any(),
        },
    };
    let check_line = st.last_check_ms.map(|ms| {
        let ok = st.last_check_ok.unwrap_or(false);
        tf(if ok { "wa.test.at" } else { "bk.check.bad_at" }, &[("when", &i18n::short_when(ms))])
    });
    let sources = v.job.sources.iter().map(source_text).collect::<Vec<_>>().join(", ");
    let size = st.repository_bytes.map(i18n::bytes).unwrap_or_else(|| "—".into());
    let v_edit = v.clone();
    let (n3, n4, n5, n6, n7) = (name.clone(), name.clone(), name.clone(), name.clone(), name.clone());
    ui::row_class(
        vec![
            ui::cell("bk.col.job", "", view! {
                <div class="cell-main">
                    <button class="link-strong mono" disabled=deleting on:click={ let v = v.clone(); move |_| edit.set(Some(Some(v.clone()))) }>{name.clone()}</button>
                    <span class="cell-sub bk-wrap">{sources}</span>
                </div>
            }
            .into_any()),
            ui::cell("bk.col.where", "", view! {
                <div class="cell-main">
                    <span>{tr(kind_key(&v.job.destination))}</span>
                    <span class="cell-sub mono bk-wrap">{v.job.destination.summary()}</span>
                </div>
            }
            .into_any()),
            ui::cell("sch.col.schedule", "", view! {
                <div class="cell-main">
                    <span>{if v.enabled { v.schedule.as_ref().map(trigger_text).unwrap_or_else(|| "—".into()) } else { t("bk.paused").to_string() }}</span>
                    <span class="cell-sub tnum">{v.next_run_ms.filter(|_| v.enabled).map(|m| tf("sch.next", &[("when", &i18n::short_when(m))]))}</span>
                </div>
            }
            .into_any()),
            ui::cell("sch.col.last", "", view! {
                <div class="cell-main">
                    <div class="chips">{last}{ui::pending_status(v.pending)}</div>
                    {(!warnings.is_empty()).then(|| view! {
                        <span class="cell-sub warn-text bk-wrap">{tf("bk.warnings_n", &[("n", &warnings.len().to_string())])}</span>
                        <span class="cell-sub bk-wrap">{ui::tech_detail(warnings.join("; "))}</span>
                    })}
                </div>
            }
            .into_any()),
            ui::cell("bk.col.size", "", view! {
                <div class="cell-main">
                    <span class="tnum">{size}</span>
                    <span class="cell-sub tnum">{tf("bk.versions_n", &[("n", &st.snapshots.to_string())])}</span>
                    <span class="cell-sub tnum">{retention_text(&v.job.retention)}</span>
                    {check_line.map(|c| view! { <span class="cell-sub">{c}</span> })}
                </div>
            }
            .into_any()),
            ui::actions(view! {
                <button class="icon-btn sm" title=tr("bk.run") aria-label=format!("{}: {}", t("bk.run"), n3)
                    disabled=move || running.get() || not_applied || deleting on:click=run>
                    {move || if running.get() { view! { <span class="btn-spinner" aria-hidden="true"></span> }.into_any() } else { view! { <Icon icon=icons::PLAY size=16 /> }.into_any() }}
                </button>
                <button class="icon-btn sm" title=tr("bk.versions") aria-label=format!("{}: {}", t("bk.versions"), n4) disabled=not_applied
                    on:click=move |_| versions.set(Some(n4.clone()))>
                    <Icon icon=icons::HISTORY size=16 />
                </button>
                <button class="icon-btn sm" title=tr("bk.check") aria-label=format!("{}: {}", t("bk.check"), n5) disabled=move || not_applied || running.get()
                    on:click=move |_| check.set(Some(n5.clone()))>
                    <Icon icon=icons::SHIELD_CHECK size=16 />
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

// ------------------------------------------------------------------ editor

const DESTS: &[(&str, &str)] = &[
    ("local", "bk.dest.local"),
    ("rest", "bk.dest.rest"),
    ("sftp", "bk.dest.sftp"),
    ("s3", "bk.dest.s3"),
    ("b2", "bk.dest.b2"),
    ("rclone", "bk.dest.rclone"),
];
const PW_MODES: &[(&str, &str)] = &[("generate", "bk.pw.generate"), ("own", "bk.pw.own")];

/// The editor's fields, one signal each.
#[derive(Clone, Copy)]
struct Form {
    name: RwSignal<String>,
    description: RwSignal<String>,
    shares: RwSignal<Vec<String>>,
    config: RwSignal<bool>,
    folders: RwSignal<String>,
    kind: RwSignal<String>,
    path: RwSignal<String>,
    url: RwSignal<String>,
    user: RwSignal<String>,
    host: RwSignal<String>,
    port: RwSignal<String>,
    folder: RwSignal<String>,
    endpoint: RwSignal<String>,
    region: RwSignal<String>,
    bucket: RwSignal<String>,
    bucket_id: RwSignal<String>,
    prefix: RwSignal<String>,
    key_id: RwSignal<String>,
    remote: RwSignal<String>,
    secret: RwSignal<String>,
    rclone_config: RwSignal<String>,
    public_key: RwSignal<String>,
    pw_mode: RwSignal<String>,
    password: RwSignal<String>,
    keep: [RwSignal<String>; 6],
    use_snapshot: RwSignal<bool>,
    stop_stacks: RwSignal<String>,
    excludes: RwSignal<String>,
    bandwidth: RwSignal<String>,
    prune_days: RwSignal<String>,
    check_days: RwSignal<String>,
    read_pct: RwSignal<String>,
    enabled: RwSignal<bool>,
}

fn s(v: impl Into<String>) -> RwSignal<String> {
    RwSignal::new(v.into())
}

impl Form {
    fn from(v: Option<&BackupJobView>) -> Self {
        let j = v.map(|v| v.job.clone());
        let r = j.as_ref().map(|j| j.retention).unwrap_or_default();
        let m = j.as_ref().map(|j| j.maintenance).unwrap_or_default();
        let f = Form {
            name: s(j.as_ref().map(|j| j.name.clone()).unwrap_or_default()),
            description: s(j.as_ref().map(|j| j.description.clone()).unwrap_or_default()),
            shares: RwSignal::new(j.as_ref().map(|j| j.sources.iter().filter_map(|s| if let BackupSource::Share { name } = s { Some(name.clone()) } else { None }).collect()).unwrap_or_default()),
            config: RwSignal::new(j.as_ref().is_none_or(|j| j.sources.contains(&BackupSource::Config))),
            folders: s(j
                .as_ref()
                .map(|j| j.sources.iter().filter_map(|s| if let BackupSource::Folder { path } = s { Some(path.clone()) } else { None }).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default()),
            kind: s("local"),
            path: s(""),
            url: s(""),
            user: s(""),
            host: s(""),
            port: s("22"),
            folder: s(""),
            endpoint: s(""),
            region: s(""),
            bucket: s(""),
            bucket_id: s(""),
            prefix: s(""),
            key_id: s(""),
            remote: s(""),
            secret: s(""),
            rclone_config: s(""),
            public_key: s(j.as_ref().map(|j| j.ssh_public_key.clone()).unwrap_or_default()),
            pw_mode: s("generate"),
            password: s(""),
            keep: [r.last, r.hourly, r.daily, r.weekly, r.monthly, r.yearly].map(|n| s(n.to_string())),
            use_snapshot: RwSignal::new(j.as_ref().is_none_or(|j| j.use_snapshot)),
            stop_stacks: s(j.as_ref().map(|j| j.stop_stacks.join(", ")).unwrap_or_default()),
            excludes: s(j.as_ref().map(|j| j.excludes.join("\n")).unwrap_or_default()),
            bandwidth: s(j.as_ref().map(|j| j.bandwidth_kib.to_string()).unwrap_or_else(|| "0".into())),
            prune_days: s(m.prune_every_days.to_string()),
            check_days: s(m.check_every_days.to_string()),
            read_pct: s(m.check_read_percent.to_string()),
            enabled: RwSignal::new(v.is_none_or(|v| v.enabled)),
        };
        if let Some(j) = &j {
            match &j.destination {
                Destination::Local { path } => f.path.set(path.clone()),
                Destination::Rest { url, user } => {
                    f.kind.set("rest".into());
                    f.url.set(url.clone());
                    f.user.set(user.clone());
                }
                Destination::Sftp { host, port, user, path } => {
                    f.kind.set("sftp".into());
                    f.host.set(host.clone());
                    f.port.set(port.to_string());
                    f.user.set(user.clone());
                    f.folder.set(path.clone());
                }
                Destination::S3 { endpoint, region, bucket, prefix, access_key_id } => {
                    f.kind.set("s3".into());
                    f.endpoint.set(endpoint.clone());
                    f.region.set(region.clone());
                    f.bucket.set(bucket.clone());
                    f.prefix.set(prefix.clone());
                    f.key_id.set(access_key_id.clone());
                }
                Destination::B2 { bucket, bucket_id, prefix, key_id } => {
                    f.kind.set("b2".into());
                    f.bucket.set(bucket.clone());
                    f.bucket_id.set(bucket_id.clone());
                    f.prefix.set(prefix.clone());
                    f.key_id.set(key_id.clone());
                }
                Destination::Rclone { remote } => {
                    f.kind.set("rclone".into());
                    f.remote.set(remote.clone());
                }
            }
        }
        f
    }

    fn destination(&self) -> Destination {
        let g = |s: RwSignal<String>| s.get_untracked().trim().to_string();
        match self.kind.get_untracked().as_str() {
            "rest" => Destination::Rest { url: g(self.url), user: g(self.user) },
            "sftp" => Destination::Sftp { host: g(self.host), port: g(self.port).parse().unwrap_or(22), user: g(self.user), path: g(self.folder) },
            "s3" => Destination::S3 { endpoint: g(self.endpoint), region: g(self.region), bucket: g(self.bucket), prefix: g(self.prefix), access_key_id: g(self.key_id) },
            "b2" => Destination::B2 { bucket: g(self.bucket), bucket_id: g(self.bucket_id), prefix: g(self.prefix), key_id: g(self.key_id) },
            "rclone" => Destination::Rclone { remote: g(self.remote) },
            _ => Destination::Local { path: g(self.path) },
        }
    }

    fn secrets(&self) -> DestinationSecrets {
        DestinationSecrets { password: self.secret.get_untracked(), rclone_config: self.rclone_config.get_untracked() }
    }

    fn job(&self) -> Result<BackupJob, &'static str> {
        let name = self.name.get_untracked().trim().to_string();
        if !valid_name(&name) {
            return Err("bk.v.name");
        }
        let mut sources: Vec<BackupSource> = self.shares.get_untracked().into_iter().map(|name| BackupSource::Share { name }).collect();
        for l in self.folders.get_untracked().lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !l.starts_with('/') {
                return Err("bk.v.folder");
            }
            sources.push(BackupSource::Folder { path: l.to_string() });
        }
        if self.config.get_untracked() {
            sources.push(BackupSource::Config);
        }
        if sources.is_empty() {
            return Err("bk.v.sources");
        }
        let n = |s: RwSignal<String>| s.get_untracked().trim().parse::<u32>().unwrap_or(0);
        let k = self.keep;
        let retention = Retention { last: n(k[0]), hourly: n(k[1]), daily: n(k[2]), weekly: n(k[3]), monthly: n(k[4]), yearly: n(k[5]) };
        if retention.is_empty() {
            return Err("bk.v.retention");
        }
        let list = |s: RwSignal<String>, sep: char| s.get_untracked().split(sep).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
        Ok(BackupJob {
            name,
            description: self.description.get_untracked().trim().to_string(),
            sources,
            destination: self.destination(),
            retention,
            maintenance: Maintenance {
                prune_every_days: n(self.prune_days).max(1),
                check_every_days: n(self.check_days),
                check_read_percent: n(self.read_pct).min(100) as u8,
            },
            excludes: list(self.excludes, '\n'),
            use_snapshot: self.use_snapshot.get_untracked(),
            stop_stacks: list(self.stop_stacks, ','),
            bandwidth_kib: n(self.bandwidth),
            sealed: None,
            ssh_public_key: self.public_key.get_untracked().trim().to_string(),
        })
    }
}

fn field(label: &'static str, sig: RwSignal<String>, o: In) -> AnyView {
    text_field(label, sig.into(), Callback::new(move |v| sig.set(v)), Signal::derive(|| None::<String>), o)
}

/// Shortcuts below a local folder: a segmented choice of the volumes (the system volume
/// included, D-ROOTVOL) filling `<volume folder>/backup`, and the same-disk note when the
/// folder is in the system volume.
fn place_picks(path: RwSignal<String>) -> AnyView {
    use rynas_api_types::storage::{SYSTEM_VOLUME_DIR, VolumePlace};
    let places = Remote::<Vec<VolumePlace>>::new(crate::places::PATH);
    let in_system = move || path.with(|p| p.strip_prefix(SYSTEM_VOLUME_DIR).is_some_and(|r| r.starts_with('/')));
    let on = Callback::new(move |v: VolumePlace| path.with(|p| p.strip_prefix(v.path.as_str()).is_some_and(|r| r.starts_with('/'))));
    let pick = Callback::new(move |v: VolumePlace| path.set(format!("{}/backup", v.path)));
    view! {
        {move || places.data.with(|p| p.as_ref().filter(|p| !p.is_empty()).map(|p| crate::places::picker("bk.f.path_on", p.clone(), on, pick)))}
        {move || in_system().then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("bk.f.same_disk").to_string())))}
    }
    .into_any()
}

fn job_editor(init: Option<BackupJobView>, shown_password: RwSignal<Option<String>>, close: Callback<()>) -> AnyView {
    let is_new = init.is_none();
    let f = Form::from(init.as_ref());
    let has_secret = init.as_ref().is_some_and(|v| v.has_destination_secret);
    let has_rclone = init.as_ref().is_some_and(|v| v.has_rclone_config);
    let shares = Remote::<Vec<ShareView>>::with_config("/api/v1/shares");
    let trig = TriggerEdit::new(&init.as_ref().and_then(|v| v.schedule.clone()).unwrap_or(TaskTrigger::Calendar {
        on_calendar: "*-*-* 02:00:00".into(),
        cron: None,
        persistent: true,
        randomized_delay_s: 600,
    }));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let testing = RwSignal::new(false);
    let tested = RwSignal::new(None::<TestResult>);
    let keygen = RwSignal::new(false);
    let orig_name = init.as_ref().map(|v| v.job.name.clone());

    let share_list = move || {
        // an encrypted share is backed up as its encrypted data: that folder is shown
        let names: Vec<(String, String)> = shares.data.with(|l| {
            l.iter()
                .flatten()
                .map(|s| {
                    let p = s.status.crypt.as_ref().filter(|c| s.share.encrypted && !c.cipher_dir.is_empty()).map(|c| c.cipher_dir.clone());
                    (s.share.name.clone(), p.unwrap_or_else(|| s.share.path.clone()))
                })
                .collect()
        });
        if names.is_empty() {
            return view! { <p class="field-help">{tr("apps.w.no_shares_yet")}</p> }.into_any();
        }
        let any_crypt = shares.data.with(|l| l.iter().flatten().any(|s| s.share.encrypted));
        view! {
            {any_crypt.then(|| view! { <p class="field-help">{tr("bk.crypt_note")}</p> })}
            <fieldset class="bk-checks">
                <legend class="field-label">{tr("apps.d.shares")}</legend>
                {names.into_iter().map(|(n, p)| {
                    let n2 = n.clone();
                    let checked = move || f.shares.with(|l| l.contains(&n2));
                    let n3 = n.clone();
                    view! {
                        <label class="check">
                            <input type="checkbox" prop:checked=checked on:change=move |ev| {
                                let on = event_target_checked(&ev);
                                f.shares.update(|l| { l.retain(|x| x != &n3); if on { l.push(n3.clone()); } });
                            } />
                            <span>{n.clone()}</span><span class="muted mono small bk-wrap">{p}</span>
                        </label>
                    }
                }).collect_view()}
            </fieldset>
        }
        .into_any()
    };

    // a new pair replaces the key already on the server's authorized_keys: asked first
    let regen_ask = RwSignal::new(false);
    let gen_key = move |_| {
        if !f.public_key.with_untracked(String::is_empty) && !regen_ask.get_untracked() {
            regen_ask.set(true);
            return;
        }
        regen_ask.set(false);
        keygen.set(true);
        spawn_local(async move {
            let r = api::post::<(), SshKeyCreated>("/api/v1/backup/restic/ssh-key", None).await;
            keygen.set(false);
            match r {
                Ok(k) => f.public_key.set(k.public_key),
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let copy_key = move |_| {
        let k = f.public_key.get_untracked();
        let _ = window().navigator().clipboard().write_text(&k);
        toast(Tone::Ok, t("bk.copied").to_string());
    };
    let secret_help = if has_secret { "bk.f.secret_kept" } else { "bk.f.secret_help" };
    let dest_fields = move || match f.kind.get().as_str() {
        "rest" => view! {
            {field("bk.f.url", f.url, In { mono: true, placeholder: "https://nas2.lan:8000/nas1", help: Some("bk.f.url_help"), ..In::default() })}
            <div class="form-row">
                {field("bk.f.user", f.user, In { autocomplete: "off", ..In::default() })}
                {field("bk.f.password", f.secret, In { kind: "password", autocomplete: "new-password", help: Some(secret_help), ..In::default() })}
            </div>
        }
        .into_any(),
        "sftp" => view! {
            <div class="form-row">
                {field("bk.f.host", f.host, In { mono: true, placeholder: "192.168.1.20", ..In::default() })}
                {field("bk.f.port", f.port, In { kind: "number", ..In::default() })}
            </div>
            <div class="form-row">
                {field("bk.f.user", f.user, In::default())}
                {field("bk.f.folder", f.folder, In { mono: true, placeholder: "/volume1/backup/nas1", ..In::default() })}
            </div>
            <div class="field">
                <span class="field-label">{tr("bk.f.ssh_key")}</span>
                {move || {
                    let k = f.public_key.get();
                    if k.is_empty() {
                        view! { <span class="field-help">{tr("bk.f.ssh_key_help")}</span> }.into_any()
                    } else {
                        view! { <code class="secret">{k}</code><span class="field-help">{tr("bk.f.ssh_key_paste")}</span> }.into_any()
                    }
                }}
                <div class="btn-row">
                    <button type="button" class="btn ghost sm" disabled=move || keygen.get() on:click=gen_key>
                        {crate::form::spinner(keygen)}<Icon icon=icons::KEY size=14 />{tr("bk.f.ssh_generate")}
                    </button>
                    <button type="button" class="btn ghost sm" disabled=move || f.public_key.with(String::is_empty) on:click=copy_key>
                        <Icon icon=icons::COPY size=14 />{tr("bk.copy")}
                    </button>
                </div>
                <Show when=move || regen_ask.get()>
                    {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! {
                        <div class="cell-main">
                            <span>{tr("bk.f.ssh_replace")}</span>
                            <div class="btn-row">
                                <button type="button" class="btn danger sm" on:click=gen_key>{tr("bk.f.ssh_replace_go")}</button>
                                <button type="button" class="btn ghost sm" on:click=move |_| regen_ask.set(false)>{tr("cancel")}</button>
                            </div>
                        </div>
                    }.into_any())}
                </Show>
            </div>
        }
        .into_any(),
        "s3" => view! {
            {field("bk.f.endpoint", f.endpoint, In { mono: true, placeholder: "https://s3.example.com", help: Some("bk.f.endpoint_help"), ..In::default() })}
            <div class="form-row">
                {field("bk.f.bucket", f.bucket, In { mono: true, ..In::default() })}
                {field("bk.f.region", f.region, In { mono: true, placeholder: "us-east-1", ..In::default() })}
            </div>
            {field("bk.f.prefix", f.prefix, In { mono: true, placeholder: "nas1", ..In::default() })}
            <div class="form-row">
                {field("bk.f.access_key", f.key_id, In { mono: true, ..In::default() })}
                {field("bk.f.secret_key", f.secret, In { kind: "password", autocomplete: "new-password", help: Some(secret_help), ..In::default() })}
            </div>
        }
        .into_any(),
        "b2" => view! {
            <div class="form-row">
                {field("bk.f.bucket", f.bucket, In { mono: true, ..In::default() })}
                {field("bk.f.bucket_id", f.bucket_id, In { mono: true, ..In::default() })}
            </div>
            {field("bk.f.prefix", f.prefix, In { mono: true, placeholder: "nas1", ..In::default() })}
            <div class="form-row">
                {field("bk.f.key_id", f.key_id, In { mono: true, ..In::default() })}
                {field("bk.f.app_key", f.secret, In { kind: "password", autocomplete: "new-password", help: Some(secret_help), ..In::default() })}
            </div>
        }
        .into_any(),
        "rclone" => view! {
            {field("bk.f.remote", f.remote, In { mono: true, placeholder: "gdrive:backup/nas1", help: Some("bk.f.remote_help"), ..In::default() })}
            {area_field("bk.f.rclone_config", f.rclone_config.into(), Callback::new(move |v| f.rclone_config.set(v)), Some(if has_rclone { "bk.f.rclone_kept" } else { "bk.f.rclone_help" }), "[gdrive]\ntype = drive\ntoken = {…}")}
        }
        .into_any(),
        _ => view! {
            {field("bk.f.path", f.path, In { mono: true, placeholder: "/media/usb1/backup", help: Some("bk.f.path_help"), ..In::default() })}
            {place_picks(f.path)}
        }
        .into_any(),
    };

    let job_name = orig_name.clone();
    let run_test = move |_| {
        let dest = f.destination();
        let mut secrets = f.secrets();
        if let Destination::Sftp { .. } = dest {
            secrets.password = f.public_key.get_untracked();
        }
        let req = TestRequest {
            destination: dest,
            secrets,
            job: job_name.clone(),
            repository_password: if f.pw_mode.get_untracked() == "own" { f.password.get_untracked() } else { String::new() },
        };
        testing.set(true);
        tested.set(None);
        spawn_local(async move {
            let r = api::post::<_, TestResult>("/api/v1/backup/restic/test", Some(&req)).await;
            testing.set(false);
            match r {
                Ok(r) => tested.set(Some(r)),
                Err(e) => tested.set(Some(TestResult { outcome: TestOutcome::Failed, message: e.text(), snapshots: None })),
            }
        });
    };
    let test_view = move || {
        tested.get().map(|r| {
            let (tone, text) = match r.outcome {
                TestOutcome::Empty => (Tone::Ok, t("bk.test.empty").to_string()),
                TestOutcome::Opens => (Tone::Ok, tf("bk.test.opens", &[("n", &r.snapshots.unwrap_or(0).to_string())])),
                TestOutcome::Locked => (Tone::Warn, t("bk.test.locked").to_string()),
                TestOutcome::Failed => (Tone::Err, t("bk.test.failed").to_string()),
            };
            // a working test's message is for the user (the SFTP host key fingerprint, pinned
            // when the job is saved); a failure's is the tool's words
            let failed = r.outcome == TestOutcome::Failed;
            view! {
                <div class="bk-test" aria-live="polite">
                    {ui::status(tone, text)}
                    {(!r.message.is_empty() && failed).then(|| ui::tech_detail(r.message.clone()))}
                    {(!r.message.is_empty() && !failed).then(|| view! { <p class="small bk-wrap">{r.message.clone()}</p> })}
                </div>
            }
        })
    };

    let save = Callback::new(move |_| {
        let job = match f.job() {
            Ok(j) => j,
            Err(k) => {
                error.set(Some(t(k).to_string()));
                return;
            }
        };
        let schedule = match trig.trigger() {
            Ok(s) => s,
            Err(k) => {
                error.set(Some(t(k).to_string()));
                return;
            }
        };
        let own = f.pw_mode.get_untracked() == "own";
        let password = f.password.get_untracked();
        if is_new && own && password.chars().count() < 8 {
            error.set(Some(t("bk.v.password").to_string()));
            return;
        }
        let req = BackupJobRequest {
            job,
            secrets: f.secrets(),
            repository_password: if own { password } else { String::new() },
            schedule: Some(schedule),
            enabled: Some(f.enabled.get_untracked()),
            notify: None,
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = if is_new {
                api::post::<_, BackupJobCreated>(JOBS, Some(&req)).await.map(|c| c.repository_password)
            } else {
                api::put::<_, serde_json::Value>(&job_url(&req.job.name, ""), &req).await.map(|_| None)
            };
            match r {
                Ok(pw) => {
                    // shown once: kept by the page even when the apply below fails
                    if let Some(pw) = pw {
                        shown_password.set(Some(pw));
                    }
                    let keys = [crate::config::keys::BACKUP, crate::config::keys::SCHEDULER];
                    if crate::config::apply_now(&keys, error).await {
                        saved_applied();
                        close.run(());
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    });

    let title = {
        let n = f.name.get_untracked();
        Signal::derive(move || if is_new { t("bk.create").to_string() } else { tf("edit_named", &[("name", &n)]) })
    };
    let k = f.keep;
    let num = |label: &'static str, sig: RwSignal<String>, max: i64| number_field(label, None, sig.into(), Callback::new(move |v| sig.set(v)), 0, max);
    let smart = move |_| {
        let r = Retention::SMART;
        for (s, v) in k.iter().zip([r.last, r.hourly, r.daily, r.weekly, r.monthly, r.yearly]) {
            s.set(v.to_string());
        }
    };
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            <h3 class="form-section">{tr("bk.sec.what")}</h3>
            {is_new.then(|| field("snap.f.name", f.name, In { mono: true, help: Some("bk.f.name_help"), maxlength: 32, ..In::default() }))}
            {field("bk.f.description", f.description, In { maxlength: 200, ..In::default() })}
            {share_list}
            {switch("bk.src.config", Some("bk.src.config_help"), f.config.into(), Callback::new(move |v| f.config.set(v)))}
            {area_field("bk.f.folders", f.folders.into(), Callback::new(move |v| f.folders.set(v)), Some("bk.f.folders_help"), "/volume1/docker")}

            <h3 class="form-section">{tr("bk.sec.where")}</h3>
            {select_field("bk.f.dest", None, DESTS, f.kind.into(), Callback::new(move |v| { f.kind.set(v); tested.set(None); }))}
            {(!is_new).then(|| view! { <p class="field-help">{tr("bk.f.dest_change")}</p> })}
            {dest_fields}
            <div class="btn-row">
                <button type="button" class="btn ghost sm" disabled=move || testing.get() on:click=run_test>
                    {crate::form::spinner(testing)}<Icon icon=icons::UNPLUG size=14 />{tr("bk.test")}
                </button>
            </div>
            {test_view}

            <h3 class="form-section">{tr("bk.sec.encryption")}</h3>
            {if is_new {
                view! {
                    {segmented("bk.f.pw_mode", PW_MODES, f.pw_mode.into(), Callback::new(move |v| f.pw_mode.set(v)))}
                    {move || if f.pw_mode.get() == "own" {
                        field("bk.f.repo_password", f.password, In { kind: "password", autocomplete: "new-password", help: Some("bk.f.repo_password_help"), ..In::default() })
                    } else {
                        view! { <p class="field-help">{tr("bk.pw.generate_help")}</p> }.into_any()
                    }}
                }
                .into_any()
            } else {
                // the key of the repository's versions: it cannot change (400)
                field("bk.f.repo_password", RwSignal::new("••••••••".to_string()), In { kind: "password", disabled: true, help: Some("bk.pw.kept"), ..In::default() })
            }}

            <h3 class="form-section">{tr("sch.sec.schedule")}</h3>
            {switch("bk.f.enabled", Some("bk.f.enabled_help"), f.enabled.into(), Callback::new(move |v| f.enabled.set(v)))}
            {trig.view(true)}

            <h3 class="form-section">{tr("bk.sec.versions")}</h3>
            <p class="field-help">{tr("bk.f.keep_help")}</p>
            <div class="form-row three">
                {num("bk.f.keep_last", k[0], 10000)}
                {num("bk.f.keep_hourly", k[1], 10000)}
                {num("bk.f.keep_daily", k[2], 10000)}
                {num("bk.f.keep_weekly", k[3], 10000)}
                {num("bk.f.keep_monthly", k[4], 10000)}
                {num("bk.f.keep_yearly", k[5], 10000)}
            </div>
            <div><button type="button" class="btn ghost sm" on:click=smart>{tr("bk.f.smart")}</button></div>

            <details class="advanced">
                <summary>{tr("advanced")}</summary>
                {switch("bk.f.use_snapshot", Some("bk.f.use_snapshot_help"), f.use_snapshot.into(), Callback::new(move |v| f.use_snapshot.set(v)))}
                {field("bk.f.stop_stacks", f.stop_stacks, In { mono: true, help: Some("bk.f.stop_stacks_help"), ..In::default() })}
                {area_field("bk.f.excludes", f.excludes.into(), Callback::new(move |v| f.excludes.set(v)), Some("bk.f.excludes_help"), "*.tmp\n**/cache")}
                <div class="form-row three">
                    {num("bk.f.bandwidth", f.bandwidth, 10_000_000)}
                    {num("bk.f.prune_days", f.prune_days, 365)}
                    {num("bk.f.check_days", f.check_days, 365)}
                    {num("bk.f.read_pct", f.read_pct, 100)}
                </div>
                <p class="field-help">{tr("bk.f.maintenance_help")}</p>
            </details>
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

/// The generated repository password, shown once.
fn password_dialog(pw: String, close: Callback<()>) -> AnyView {
    let saved = RwSignal::new(false);
    let p2 = pw.clone();
    let copy = move |_| {
        let _ = window().navigator().clipboard().write_text(&p2);
        toast(Tone::Ok, t("bk.copied").to_string());
    };
    let p3 = pw.clone();
    let download = move |_| api::save_text("rynas-backup-password.txt", &format!("{}\n\n{p3}\n", t("bk.pw.file_note")));
    let body = view! {
        {ui::note(Tone::Err, icons::TRIANGLE_ALERT, view! { <span>{tr("bk.pw.warning")}</span> }.into_any())}
        <code class="secret rk" aria-label=tr("bk.f.repo_password")>{pw.clone()}</code>
        <div class="btn-row">
            <button type="button" class="btn ghost sm" on:click=copy><Icon icon=icons::COPY size=14 />{tr("bk.copy")}</button>
            <button type="button" class="btn ghost sm" on:click=download><Icon icon=icons::DOWNLOAD size=14 />{tr("bk.pw.download")}</button>
        </div>
        <label class="check">
            <input type="checkbox" prop:checked=move || saved.get() on:change=move |ev| saved.set(event_target_checked(&ev)) />
            <span>{tr("bk.pw.saved")}</span>
        </label>
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn primary" disabled=move || !saved.get() on:click=move |_| close.run(())>{tr("bk.pw.done")}</button>
    }
    .into_any();
    // closing without the confirmation is refused: Esc and the backdrop do nothing
    dialog(DlgKind::Alert, Signal::derive(|| t("bk.pw.title").to_string()), Callback::new(move |_| if saved.get_untracked() { close.run(()) }), body, Some(foot))
}

// ------------------------------------------------------------------ check

const READS: &[(&str, &str)] = &[("0", "bk.check.structure"), ("5", "bk.check.five"), ("100", "bk.check.all")];

fn check_dialog(name: String, jobs: Remote<Vec<BackupJobView>>, close: Callback<()>) -> AnyView {
    let read = RwSignal::new("0".to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let n = name.clone();
    let go = move |_| {
        let pct: u8 = read.get_untracked().parse().unwrap_or(0);
        let n = n.clone();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let body = serde_json::to_string(&CheckRequest { read_percent: pct }).ok();
            match api::start_task(&job_url(&n, "/check"), body).await {
                Ok(id) => {
                    toast(Tone::Info, tf("bk.check.started", &[("name", &n)]));
                    close.run(());
                    let done = tasks::follow(&id).await;
                    match done {
                        Ok(t2) if t2.status == TaskStatus::Ok => toast(Tone::Ok, tf("bk.check.done", &[("name", &n)])),
                        Ok(t2) => toast(Tone::Err, tasks::task_error(&t2)),
                        Err(_) => {}
                    }
                    jobs.reload();
                }
                Err(e) => {
                    busy.set(false);
                    error.set(Some(e.text()));
                }
            }
        });
    };
    let body = view! {
        <p class="dialog-text">{tr("bk.check.text")}</p>
        {segmented("bk.check.read", READS, read.into(), Callback::new(move |v| read.set(v)))}
        {crate::form::form_error(error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() on:click=go>{crate::form::spinner(busy)}{tr("bk.check")}</button>
    }
    .into_any();
    dialog(DlgKind::Normal, Signal::derive(move || tf("bk.check.title", &[("name", &name)])), close, body, Some(foot))
}

// ------------------------------------------------------------------ versions and restore

fn versions_drawer(name: String, close: Callback<()>) -> AnyView {
    // only this NAS' versions unless asked (?all_hosts=true: a repository shared with
    // another NAS)
    let all_hosts = RwSignal::new(false);
    let open = RwSignal::new(None::<SnapshotEntry>);
    let picked = RwSignal::new(Vec::<String>::new());
    let restore = RwSignal::new(false);
    let n0 = name.clone();
    let list = move || {
        let all = all_hosts.get();
        let url = format!("{}{}", job_url(&n0, "/snapshots"), if all { "?all_hosts=true" } else { "" });
        let snaps = Remote::<Vec<SnapshotEntry>>::new(url);
        let rows = move || {
            if let Some(v) = snaps.gate(5) {
                return v;
            }
            let rows = snaps.data.get().unwrap_or_default();
            if rows.is_empty() {
                return view! { <EmptyState icon=icons::HISTORY title=Signal::derive(|| t("bk.no_versions").to_string()) text=Signal::derive(|| t("bk.no_versions_text").to_string()) /> }.into_any();
            }
            view! {
                <ul class="bk-list">
                    {rows.into_iter().map(|s| {
                        let s2 = s.clone();
                        let mut sub = s.paths.join(", ");
                        if let Some(b) = s.bytes {
                            sub = format!("{sub} · {}", i18n::bytes(b));
                        }
                        if all {
                            sub = format!("{} · {sub}", s.hostname);
                        }
                        view! {
                            <li>
                                <button class="bk-item" on:click=move |_| { picked.set(Vec::new()); open.set(Some(s2.clone())); }>
                                    <Icon icon=icons::HISTORY size=16 />
                                    <span class="cell-main">
                                        <span class="tnum">{i18n::short_when(s.time_ms)}<span class="muted mono small">{format!(" · {}", s.id.get(..8).unwrap_or(&s.id))}</span></span>
                                        <span class="cell-sub bk-wrap">{sub}</span>
                                    </span>
                                    <Icon icon=icons::CHEVRON_RIGHT size=16 />
                                </button>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            }
            .into_any()
        };
        view! {
            {switch("bk.all_hosts", Some("bk.all_hosts_help"), all_hosts.into(), Callback::new(move |v| all_hosts.set(v)))}
            {rows}
        }
        .into_any()
    };
    let n = name.clone();
    let body = view! {
        {move || match open.get() {
            None => list(),
            Some(s) => browser(n.clone(), s, picked, Callback::new(move |_| open.set(None))),
        }}
    }
    .into_any();
    let n2 = name.clone();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button>
        {move || open.get().is_some().then(|| view! {
            <button type="button" class="btn primary" on:click=move |_| restore.set(true)>
                <Icon icon=icons::ARCHIVE_RESTORE size=16 />
                {move || if picked.with(Vec::is_empty) { t("bk.restore_all").to_string() } else { tf("bk.restore_n", &[("n", &picked.with(Vec::len).to_string())]) }}
            </button>
        })}
        {move || restore.get().then(|| {
            let id = open.with_untracked(|s| s.as_ref().map(|s| s.id.clone()).unwrap_or_default());
            restore_dialog(n2.clone(), id, picked.get_untracked(), Callback::new(move |_| restore.set(false)))
        })}
    }
    .into_any();
    drawer(Signal::derive(move || tf("bk.versions_of", &[("name", &name)])), close, body, foot)
}

/// One snapshot: its folders, a selection (`picked`, restored from the drawer's footer) and
/// downloads.
fn browser(name: String, snap: SnapshotEntry, picked: RwSignal<Vec<String>>, back: Callback<()>) -> AnyView {
    let root = if snap.paths.len() == 1 { snap.paths[0].clone() } else { "/".to_string() };
    let path = RwSignal::new(root.clone());
    let entries = RwSignal::new(None::<Result<Vec<NodeEntry>, String>>);
    let (n, id) = (name.clone(), snap.id.clone());
    Effect::new(move |_| {
        let p = path.get();
        let url = format!("{}?snapshot={}&path={}", job_url(&n, "/browse"), api::encode(&id), api::encode(&p));
        entries.set(None);
        spawn_local(async move {
            let r = api::get::<Vec<NodeEntry>>(&url).await.map_err(|e| e.text());
            entries.set(Some(r));
        });
    });
    let (n2, id2) = (name.clone(), snap.id.clone());
    let rows = move || match entries.get() {
        None => view! { <crate::ui::Skeleton lines=5 /> }.into_any(),
        Some(Err(e)) => ui::err_view(e),
        Some(Ok(list)) if list.is_empty() => view! { <p class="muted">{tr("snap.empty_dir")}</p> }.into_any(),
        Some(Ok(list)) => view! {
            <ul class="bk-list">
                {list.into_iter().map(|e| {
                    let p = e.path.clone();
                    let p2 = p.clone();
                    let p3 = p.clone();
                    let checked = move || picked.with(|l| l.contains(&p2));
                    let dl = (!e.dir).then(|| {
                        let url = format!("{}?snapshot={}&path={}", job_url(&n2, "/download"), api::encode(&id2), api::encode(&p));
                        let fname = e.name.clone();
                        view! {
                            <button class="icon-btn sm" title=tr("bk.download") aria-label=format!("{}: {}", t("bk.download"), e.name)
                                on:click=move |_| { let (u, f) = (url.clone(), fname.clone()); spawn_local(async move { if let Err(err) = api::download(&u, &f).await { toast(Tone::Err, err.text()); } }); }>
                                <Icon icon=icons::DOWNLOAD size=16 />
                            </button>
                        }
                    });
                    let open_dir = e.dir.then(|| {
                        let p4 = p.clone();
                        view! {
                            <button class="link-btn bk-wrap" on:click=move |_| path.set(p4.clone())>{e.name.clone()}</button>
                        }
                    });
                    view! {
                        <li class="bk-entry">
                            <input type="checkbox" aria-label=format!("{}: {}", t("bk.pick"), e.name) prop:checked=checked on:change=move |ev| {
                                let on = event_target_checked(&ev);
                                picked.update(|l| { l.retain(|x| x != &p3); if on { l.push(p3.clone()); } });
                            } />
                            <Icon icon=if e.dir { icons::FOLDER } else { icons::FILE } size=16 />
                            {match open_dir { Some(v) => v.into_any(), None => view! { <span class="bk-wrap">{e.name.clone()}</span> }.into_any() }}
                            <span class="muted small tnum bk-size">{(!e.dir).then(|| i18n::bytes(e.size))}</span>
                            {dl}
                        </li>
                    }
                }).collect_view()}
            </ul>
        }
        .into_any(),
    };
    let r2 = root.clone();
    let up = move |_| {
        let p = path.get_untracked();
        if p != r2 && p != "/" {
            let parent = p.rsplit_once('/').map(|(a, _)| if a.is_empty() { "/".to_string() } else { a.to_string() }).unwrap_or_else(|| "/".into());
            path.set(parent);
        }
    };
    let r3 = root.clone();
    view! {
        <div class="bk-browser">
            <div class="btn-row">
                <button type="button" class="btn ghost sm" on:click=move |_| back.run(())><Icon icon=icons::ARROW_LEFT size=14 />{tr("bk.all_versions")}</button>
                <button type="button" class="btn ghost sm" disabled=move || path.with(|p| *p == r3 || p == "/") on:click=up><Icon icon=icons::ARROW_UP size=14 />{tr("bk.up")}</button>
            </div>
            <p class="strong">{i18n::short_when(snap.time_ms)}</p>
            <p class="mono small bk-wrap">{move || path.get()}</p>
            {rows}
        </div>
    }
    .into_any()
}

const TARGETS: &[(&str, &str)] = &[("original", "bk.restore.original"), ("folder", "bk.restore.folder")];

fn restore_dialog(name: String, snapshot: String, paths: Vec<String>, close: Callback<()>) -> AnyView {
    let target = RwSignal::new("folder".to_string());
    let folder = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // the original place is overwritten: an explicit confirmation (?confirm=true)
    let overwrite_ok = RwSignal::new(false);
    let original = move || target.get() == "original";
    let count = paths.len();
    let go = move |_| {
        let to_original = target.get_untracked() == "original";
        let t2 = if to_original {
            RestoreTarget::Original
        } else {
            let f = folder.get_untracked().trim().to_string();
            if !f.starts_with('/') {
                error.set(Some(t("bk.v.restore_folder").to_string()));
                return;
            }
            RestoreTarget::Folder { path: f }
        };
        let req = RestoreRequest { snapshot: snapshot.clone(), paths: paths.clone(), target: t2 };
        let url = format!("{}{}", job_url(&name, "/restore"), if to_original { "?confirm=true" } else { "" });
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::start_task(&url, serde_json::to_string(&req).ok()).await {
                Ok(id) => {
                    toast(Tone::Info, t("bk.restore.started").to_string());
                    close.run(());
                    match tasks::follow(&id).await {
                        Ok(t2) if t2.status == TaskStatus::Ok => toast(Tone::Ok, t("bk.restore.done").to_string()),
                        Ok(t2) => toast(Tone::Err, tasks::task_error(&t2)),
                        Err(_) => {}
                    }
                }
                Err(e) => {
                    busy.set(false);
                    error.set(Some(e.text()));
                }
            }
        });
    };
    let body = view! {
        <p class="dialog-text">{if count == 0 { t("bk.restore.all_text").to_string() } else { tf("bk.restore.n_text", &[("n", &count.to_string())]) }}</p>
        {segmented("bk.restore.to", TARGETS, target.into(), Callback::new(move |v| { overwrite_ok.set(false); target.set(v); }))}
        {move || if original() {
            view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <span>{tr("bk.restore.original_warn")}</span> }.into_any())}
                <label class="check">
                    <input type="checkbox" prop:checked=move || overwrite_ok.get() on:change=move |ev| overwrite_ok.set(event_target_checked(&ev)) />
                    <span>{tr("bk.restore.original_confirm")}</span>
                </label>
            }
            .into_any()
        } else {
            field("bk.restore.folder_path", folder, In { mono: true, placeholder: "/volume1/restore", help: Some("bk.restore.folder_help"), ..In::default() })
        }}
        {crate::form::form_error(error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class=move || if original() { "btn danger" } else { "btn primary" } disabled=move || busy.get() || (original() && !overwrite_ok.get()) on:click=go>
            {crate::form::spinner(busy)}{move || if original() { t("bk.restore.overwrite") } else { t("bk.restore") }}
        </button>
    }
    .into_any();
    dialog(DlgKind::Normal, Signal::derive(|| t("bk.restore").to_string()), close, body, Some(foot))
}
