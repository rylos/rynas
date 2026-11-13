//! Backup › PCs: btrfs snapshots: the PCs' read-only btrfs snapshots received on the NAS
//! (D-PCBACKUP, `docs/research/pc-backup.md` §10–§11). Two senders, one folder model:
//! `rynas-agent` over HTTPS (recommended: no SSH, the NAS owns retention) or btrbk over SSH
//! (legacy, manual set-up). Snapshot names are btrbk's either way, so a PC can switch.
//!
//! - The card lists each PC with how it sends, its freshness, open problems, snapshots,
//!   space and whether the snapshots are enough for a restore on an empty disk; SSH setup
//!   problems show above it.
//! - "Add a PC": rynas-agent (a new PC, paired from the drawer, or an already paired file PC
//!   moved here) or btrbk; a new subvolume folder, or an existing folder of received
//!   snapshots (Check shows what it holds, nothing inside changes). Alert, retention (the
//!   NAS with GFS counts, or for btrbk the PC within the NAS floor); the btrbk key.
//! - The drawer: problems; with rynas-agent its status, progress and last run (or the code
//!   waiting for it, or Make a code), then the folder under its own title; one row of
//!   buttons (Back up now / Cancel, Measure, Delete old snapshots now, Settings, Remove); the
//!   agent's runs; the full-restore check with the manifest's subvolumes, disks and boot
//!   copies; "Restore bare metal" (a restore code and the agent's commands); for btrbk the folded manual steps (key, `btrbk.conf`, first connection, the
//!   manifest drop-in; open while they are the only way forward) then the first-backup
//!   check; the snapshots by subvolume or by run, with sizes, Browse and Delete.
//! - Settings: one dialog per PC: the folder's (also the switch between the senders) and,
//!   with rynas-agent, its schedule, the subvolumes it leaves out and Revoke.
//! - Browse: read-only folders inside a snapshot; Copy out puts a file or folder into a NAS
//!   folder (nothing is overwritten).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::pcbackup::*;
use rynas_api_types::snapshots::{EntryKind, Retention, RetentionPreset, SnapshotEntry, SnapshotListing};
use rynas_api_types::TaskStatus;

use crate::api;
use crate::form::{In, area_field, form_error, number_field, segmented, select_field, spinner, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::pages::security::copy_button_for;
use rynas_api_types::pcagent::{PcDevice, PcMethod};
use crate::pages::snapshots::{mode_text, retention_text};
use crate::tasks;
use crate::toast::toast;
use crate::ui::{self, Confirm, DlgKind, EmptyState, ErrorBox, Remote, Skeleton, Tone};

const URL: &str = "/api/v1/pc-backups/targets";

#[derive(Clone, Copy)]
struct Pcs {
    list: Remote<Vec<PcTarget>>,
    setup: Remote<ReceiveSetup>,
    /// Device of the open drawer.
    open: RwSignal<Option<String>>,
    /// The add dialog (`Some(adopt)`), or the settings of a device.
    add: RwSignal<Option<bool>>,
    /// The folder, and the agent paired under its name (its schedule is in the same dialog).
    edit: RwSignal<Option<(PcTarget, Option<PcDevice>)>>,
    /// Revoke the drawer's agent (asked from Settings, confirmed in the drawer).
    ask_revoke: RwSignal<bool>,
    /// Bumped when Settings saved: the open drawer reads its PC again.
    saved: RwSignal<u32>,
}

fn target_url(device: &str) -> String {
    format!("{URL}/{}", api::encode(device))
}

fn state_status(s: ReceiveState) -> AnyView {
    let (tone, k) = match s {
        ReceiveState::Ok => (Tone::Ok, "upd.uptodate"),
        ReceiveState::Late => (Tone::Warn, "bk.st.late"),
        ReceiveState::Waiting => (Tone::Info, "bk.st.waiting"),
        ReceiveState::Missing => (Tone::Err, "bk.st.missing"),
    };
    ui::status(tone, t(k).to_string())
}

fn when(ms: Option<u64>) -> String {
    ms.map(i18n::datetime).unwrap_or_else(|| "—".into())
}

fn kv(label: &'static str, value: AnyView) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd>{value}</dd></div> }.into_any()
}

/// A titled command block with its own Copy button (named after the title for screen
/// readers); nothing when the text is empty.
fn code_block(title: &'static str, text: String) -> Option<AnyView> {
    if text.trim().is_empty() {
        return None;
    }
    let aria = tf("pcb.copy_of", &[("what", t(title))]);
    Some(
        view! {
            <h4 class="small"><strong>{tr(title)}</strong></h4>
            <pre class="app-code">{text.clone()}</pre>
            <div class="sec-actions">{copy_button_for("dns.copy", aria, text)}</div>
        }
        .into_any(),
    )
}

fn problem_text(p: &SetupProblem) -> String {
    let k = match p.code {
        SetupProblemCode::SshdMissing => "bk.problem.sshd_missing",
        SetupProblemCode::SshdAllowList => "bk.problem.sshd_allow_list",
        SetupProblemCode::SudoMissing => "bk.problem.sudo_missing",
        SetupProblemCode::BtrfsMissing => "bk.problem.btrfs_missing",
        SetupProblemCode::FilterMissing => "bk.problem.filter_missing",
    };
    tf(k, &[("subject", p.subject.as_deref().unwrap_or(""))])
}

/// One open problem of a PC, as a sentence.
fn issue_text(i: &PcIssue) -> String {
    let subject = i.subject.as_deref().unwrap_or("");
    let since = when(i.since_ms);
    let k = match i.code {
        PcIssueCode::Late => "bkpc.issue.late",
        PcIssueCode::SubvolumeLate if i.since_ms.is_none() => "bkpc.issue.subvolume_never",
        PcIssueCode::SubvolumeLate => "bkpc.issue.subvolume_late",
        PcIssueCode::ReceiveFailed => "bkpc.issue.receive_failed",
        PcIssueCode::ReceiveStuck => "bkpc.issue.receive_stuck",
        PcIssueCode::LowSpace => "bkpc.issue.low_space",
    };
    let b = |v: Option<u64>| i18n::bytes(v.unwrap_or(0));
    tf(k, &[("subject", subject), ("when", &since), ("free", &b(i.free_bytes)), ("needed", &b(i.needed_bytes))])
}

/// The first problem in a few words (the list).
fn issue_short(i: &PcIssue) -> &'static str {
    match i.code {
        PcIssueCode::Late => "bk.st.late",
        PcIssueCode::SubvolumeLate => "bkpc.short.subvolume_late",
        PcIssueCode::ReceiveFailed => "bkpc.short.receive_failed",
        PcIssueCode::ReceiveStuck => "bkpc.short.receive_stuck",
        PcIssueCode::LowSpace => "bkpc.short.low_space",
    }
}

/// `agent`: sent by rynas-agent (it writes the manifest itself; btrbk needs the drop-in of
/// the manual steps).
fn gap_text(g: &BareMetalGap, agent: bool) -> String {
    let k = match g.code {
        BareMetalCode::NoSnapshots => "bkpc.bm.no_snapshots",
        BareMetalCode::NoManifest if !agent => "pcb.bm.no_manifest_btrbk",
        BareMetalCode::NoManifest => "bkpc.bm.no_manifest",
        BareMetalCode::ManifestInvalid => "bkpc.bm.manifest_invalid",
        BareMetalCode::ManifestUnsupported => "bkpc.bm.manifest_unsupported",
        BareMetalCode::NoRoot => "bkpc.bm.no_root",
        BareMetalCode::SubvolumeMissing => "bkpc.bm.subvolume_missing",
        BareMetalCode::LoaderUnknown => "bkpc.bm.loader_unknown",
        BareMetalCode::ManifestStale => "bkpc.bm.manifest_stale",
        BareMetalCode::LatestRunIncomplete => "bkpc.bm.latest_run_incomplete",
    };
    tf(k, &[("subject", g.subject.as_deref().unwrap_or(""))])
}

fn bm_status(b: &BareMetal) -> AnyView {
    if b.ready {
        ui::status(Tone::Ok, t("bkpc.bm.ready").to_string())
    } else {
        ui::status(Tone::Info, t("bkpc.bm.not_ready").to_string())
    }
}

/// The PC's disks from its manifest: name, model, serial and size, then each partition
/// indented below (a left-aligned block; sizes never break).
fn disk_items(disks: &[rynas_api_types::pcagent::Disk]) -> impl IntoView + use<> {
    disks
        .iter()
        .map(|d| {
            let mut head = vec![d.name.clone()];
            if !d.model.is_empty() {
                head.push(d.model.clone());
            }
            if !d.serial.is_empty() {
                head.push(tf("bkpc.bm.serial", &[("serial", &d.serial)]));
            }
            let parts = d
                .table
                .iter()
                .flat_map(|t| t.partitions.iter())
                .map(|p| {
                    let size = i18n::bytes(p.size_lba.saturating_mul(u64::from(d.logical_sector)));
                    let name = (!p.name.is_empty()).then(|| format!(" · {}", p.name));
                    view! { <li>{tf("bkpc.bm.part", &[("n", &p.number.to_string())])}" · "<span class="nowrap tnum">{size}</span>{name}</li> }
                })
                .collect_view();
            view! {
                <li>
                    <span class="mono">{head.join(" · ")}</span>" · "<span class="nowrap tnum">{i18n::bytes(d.size_bytes)}</span>
                    <ul class="bkpc-sub">{parts}</ul>
                </li>
            }
        })
        .collect_view()
}

/// The boot loader as the manifest names it, as people write it.
fn loader_label(raw: &str) -> String {
    match raw {
        "grub" => "GRUB".into(),
        "systemd_boot" => "systemd-boot".into(),
        "refind" => "rEFInd".into(),
        "unknown" | "" => t("pcb.loader_unknown").to_string(),
        other => other.replace('_', "-"),
    }
}

/// The boot filesystems the PC copies into its root subvolume: mount, copy, size, the
/// files left out as key material, and a failed last copy (the previous one is kept).
fn boot_copy_items(copies: &[rynas_api_types::pcagent::BootCopy]) -> impl IntoView + use<> {
    copies
        .iter()
        .map(|c| {
            let skipped = (!c.skipped.is_empty()).then(|| view! { <li class="muted">{tf("pcb.copy_skipped", &[("files", &c.skipped.join(", "))])}</li> });
            let stale = c.stale.then(|| view! { <li class="warn-text">{tf("pcb.copy_stale", &[("error", c.error.as_deref().unwrap_or(""))])}</li> });
            view! {
                <li>
                    <span class="mono">{c.mount.clone()}</span>" → "<span class="mono">{c.dir.clone()}</span>" "<span class="nowrap tnum">{i18n::bytes(c.bytes)}</span>
                    {(skipped.is_some() || stale.is_some()).then(|| view! { <ul class="bkpc-sub">{stale}{skipped}</ul> })}
                </li>
            }
        })
        .collect_view()
}

fn first_run(b: &BareMetal, list: &[ReceivedSnapshot]) -> Option<AnyView> {
    let title = view! { <h4 class="small"><strong>{tr("bkpc.first_run")}</strong></h4> };
    // only mounted subvolumes are sent: one not mounted is never expected
    let expected: Vec<&rynas_api_types::pcagent::BtrbkSubvolume> = b.layout.iter().filter(|s| !s.mount.is_empty()).collect();
    if expected.is_empty() {
        let mut seen: Vec<String> = Vec::new();
        for s in list.iter().filter(|s| !s.incomplete && !s.not_subvolume) {
            if !seen.contains(&s.group) {
                seen.push(s.group.clone());
            }
        }
        if seen.is_empty() {
            return None;
        }
        return Some(
            view! {
                {title}
                <p class="small">{tf("pcb.first_run_so_far", &[("names", &seen.join(", "))])}</p>
                <p class="small muted">{tr("pcb.first_run_no_manifest")}</p>
            }
            .into_any(),
        );
    }
    let got = |name: &str| list.iter().any(|x| x.group == name && !x.incomplete && !x.not_subvolume);
    // everything there: one line, no list
    if expected.iter().all(|s| got(&s.name)) {
        return Some(view! { <div>{ui::status(Tone::Ok, t("pcb.first_run_all").to_string())}</div> }.into_any());
    }
    let rows = expected
        .iter()
        .map(|s| {
            let st = if got(&s.name) { ui::status(Tone::Ok, t("bkpc.first_run_ok").to_string()) } else { ui::status(Tone::Info, t("bkpc.first_run_wait").to_string()) };
            view! { <div class="kv"><dt class="mono">{s.name.clone()}</dt><dd>{st}</dd></div> }
        })
        .collect_view();
    Some(
        view! {
            {title}
            <p class="small muted">{tr("bkpc.first_run_intro")}</p>
            <dl class="kvs">{rows}</dl>
        }
        .into_any(),
    )
}

/// "12.0 GiB · 310 MiB its own".
fn size_text(z: Option<SnapshotSize>) -> String {
    z.map(|z| tf("bkpc.size_text", &[("ref", &i18n::bytes(z.referenced_bytes)), ("excl", &i18n::bytes(z.exclusive_bytes))]))
        .unwrap_or_else(|| "—".into())
}

/// A retention age as written in the settings ("1d", "12h", "2w") in words with the right
/// plural ("1 day", "12 hours"); anything else as written.
fn age_text(raw: &str) -> String {
    let s = raw.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let Ok(n) = num.parse::<u64>() else { return s.to_string() };
    let one = n == 1;
    let k = match unit {
        "h" if one => "pcb.age.h1",
        "h" => "pcb.age.h",
        "d" if one => "pcb.age.d1",
        "d" => "pcb.age.d",
        "w" if one => "pcb.age.w1",
        "w" => "pcb.age.w",
        "m" if one => "pcb.age.m1",
        "m" => "pcb.age.m",
        "y" if one => "pcb.age.y1",
        "y" => "pcb.age.y",
        _ => return s.to_string(),
    };
    tf(k, &[("n", &n.to_string())])
}

/// Who deletes old snapshots, in a sentence.
fn rules_text(p: &PcTarget) -> String {
    if p.retention.owner == RetentionOwner::Nas {
        let g = &p.retention.gfs;
        return tf("bkpc.ret.nas", &[("counts", &retention_text(g)), ("min", &age_text(&g.min))]);
    }
    if !p.delete.allowed {
        return t("bk.rules.off").to_string();
    }
    let one = p.delete.keep_last == 1;
    let k = match (p.delete.keep_last > 0, p.delete.min_age_days > 0) {
        (true, true) if one => "bk.rules.on_one",
        (true, true) => "bk.rules.on",
        (true, false) if one => "bk.rules.on_keep_one",
        (true, false) => "bk.rules.on_keep",
        (false, true) => "bk.rules.on_days",
        (false, false) => "bk.rules.on_free",
    };
    tf(k, &[("keep", &p.delete.keep_last.to_string()), ("days", &p.delete.min_age_days.to_string())])
}

/// Runs a task, toasts the outcome, reloads the list.
/// A refused field of the settings dialog is shown under that field (`fields`).
fn run(pcs: Pcs, (method, url, body): (&'static str, String, Option<String>), ok: String, (busy, error, fields): (RwSignal<bool>, RwSignal<Option<String>>, Option<Fields>), done: Callback<()>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let started = match method {
            "PUT" => api::put::<_, rynas_api_types::TaskStarted>(&url, &crate::api::decode::<serde_json::Value>(body.as_deref().unwrap_or("{}")).unwrap_or_default()).await.map(|t| t.task_id),
            "DELETE" => api::delete::<rynas_api_types::TaskStarted>(&url).await.map(|t| t.task_id),
            _ => api::start_task(&url, body).await,
        };
        let r = match started {
            Ok(id) => tasks::follow(&id).await,
            Err(e) => Err(e),
        };
        let _ = busy.try_set(false);
        match r {
            Ok(task) if task.status == TaskStatus::Ok => {
                toast(Tone::Ok, ok);
                pcs.list.reload();
                done.run(());
            }
            Ok(task) => {
                let _ = error.try_set(Some(tasks::task_error(&task)));
            }
            Err(e) => match (fields, e.field()) {
                (Some(f), Some("description")) => {
                    let _ = f.desc_err.try_set(Some(e.text()));
                }
                (Some(f), Some("public_key")) => {
                    let _ = f.key_err.try_set(Some(e.text()));
                }
                _ => {
                    let _ = error.try_set(Some(e.text()));
                }
            },
        }
    });
}

/// The tab's actions in the page header: add a PC, adopt a folder (`add` holds whether the
/// dialog opens on adopting).
pub fn header_buttons(add: RwSignal<Option<bool>>) -> AnyView {
    view! {
        <button class="btn primary" on:click=move |_| add.set(Some(false))><Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("bk.add")}</span></button>
        <button class="btn ghost" on:click=move |_| add.set(Some(true))><Icon icon=icons::FOLDER_OPEN size=16 /><span class="btn-label">{tr("bk.adopt")}</span></button>
    }
    .into_any()
}

/// The PCs tab.
pub fn panel(add: RwSignal<Option<bool>>) -> AnyView {
    let pcs = Pcs {
        list: Remote::new(URL),
        setup: Remote::new("/api/v1/pc-backups/setup"),
        open: RwSignal::new(None),
        add,
        edit: RwSignal::new(None),
        ask_revoke: RwSignal::new(false),
        saved: RwSignal::new(0),
    };
    let problems = move || {
        pcs.setup.data.with(|s| {
            s.as_ref().map(|s| {
                s.problems
                    .iter()
                    .map(|p| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(problem_text(p))))
                    .collect_view()
            })
        })
    };
    let content = move || {
        let Some(list) = pcs.list.data.get() else { return ().into_any() };
        if list.is_empty() {
            return view! {
                <EmptyState icon=icons::MONITOR title=Signal::derive(move || t("bkpc.empty").to_string()) text=Signal::derive(move || t("bkpc.empty_text").to_string())>
                    <div class="sec-actions">
                        <button class="btn primary" on:click=move |_| pcs.add.set(Some(false))><Icon icon=icons::PLUS size=16 />{tr("bk.add")}</button>
                        <button class="btn ghost" on:click=move |_| pcs.add.set(Some(true))><Icon icon=icons::FOLDER_OPEN size=16 />{tr("bk.adopt")}</button>
                    </div>
                </EmptyState>
            }
            .into_any();
        }
        let rows = list
            .iter()
            .map(|p| {
                let dev = p.device.clone();
                let groups = p.groups.iter().map(|g| g.name.clone()).collect::<Vec<_>>().join(", ");
                let via = t(if p.agent { "pcb.via.agent" } else { "pcb.via.ssh" });
                let sub = Some(if p.description.is_empty() { via.to_string() } else { format!("{} · {via}", p.description) });
                let space = p.usage.map(|u| i18n::bytes(u.exclusive_bytes)).unwrap_or_else(|| "—".into());
                let label = format!("{}: {}", t("bk.open"), p.device);
                // the first problem other than "late" (the state already says that)
                let problem = p.issues.iter().find(|i| i.code != PcIssueCode::Late).map(|i| {
                    let tone = if i.code == PcIssueCode::LowSpace { Tone::Err } else { Tone::Warn };
                    ui::status(tone, t(issue_short(i)).to_string())
                });
                ui::row(vec![
                    ui::cell("bk.col.pc", "wrap", ui::two_lines(view! { <strong>{p.device.clone()}</strong> }.into_any(), sub, "cell-sub small")),
                    ui::cell("bk.col.folder", "wrap", ui::breakable(&p.path, "mono small")),
                    ui::cell("col.status", "wrap", view! { <div class="cell-main">{state_status(p.state)}{problem}<span class="cell-sub small">{when(p.last_received_ms)}</span></div> }.into_any()),
                    ui::cell("bk.col.snapshots", "wrap", ui::two_lines(view! { <span class="tnum">{i18n::count(u64::from(p.snapshots))}</span> }.into_any(), (!groups.is_empty()).then_some(groups), "cell-sub small mono")),
                    ui::text_cell("bk.col.space", "num tnum", space),
                    ui::cell("bkpc.col.restore", "", bm_status(&p.bare_metal)),
                    ui::actions(view! { <button class="btn ghost sm" aria-label=label on:click=move |_| pcs.open.set(Some(dev.clone()))><Icon icon=icons::EYE size=14 />{tr("bk.open")}</button> }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        ui::table("bk-table", &["bk.col.pc", "bk.col.folder", "col.status", "bk.col.snapshots", "#bk.col.space", "bkpc.col.restore", ""], rows)
    };
    view! {
        <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("bk.intro")}</p>
        {problems}
        {move || pcs.list.view(4, content)}
        {move || pcs.open.get().map(|d| drawer(pcs, d))}
        {move || pcs.add.get().map(|adopt| add_dialog(pcs, adopt))}
        {move || pcs.edit.get().map(|(p, d)| settings_dialog(pcs, p, d))}
    }
    .into_any()
}

// ------------------------------------------------------------------ add and settings

const OWNERS: &[(&str, &str)] = &[("nas", "bkpc.owner.nas"), ("pc", "bkpc.owner.pc")];
const PRESETS: &[(&str, &str)] = &[("standard", "snap.preset.standard"), ("light", "snap.preset.light"), ("long", "snap.preset.long"), ("custom", "snap.preset.custom")];

/// Shared fields of the add and settings dialogs.
#[derive(Clone, Copy)]
struct Fields {
    description: RwSignal<String>,
    key: RwSignal<String>,
    /// Alert time in hours.
    alert: RwSignal<String>,
    allowed: RwSignal<bool>,
    keep: RwSignal<String>,
    min_age: RwSignal<String>,
    /// `nas` or `pc`.
    owner: RwSignal<String>,
    preset: RwSignal<String>,
    /// hourly, daily, weekly, monthly, yearly (custom preset).
    counts: [RwSignal<String>; 5],
    min: RwSignal<String>,
    ignored: RwSignal<Vec<String>>,
    key_err: RwSignal<Option<String>>,
    desc_err: RwSignal<Option<String>>,
}

impl Fields {
    /// `p`: the PC's settings; a new PC lets the NAS own retention, an adopted folder keeps
    /// btrbk on the PC deleting (its chain was set up that way).
    fn new(p: Option<&PcTarget>, adopt: bool) -> Self {
        let d = p.map(|p| p.delete).unwrap_or_default();
        let r = p.map(|p| p.retention.clone()).unwrap_or_else(|| PcRetention {
            owner: if adopt { RetentionOwner::Pc } else { RetentionOwner::Nas },
            gfs: Retention::default(),
        });
        let g = &r.gfs;
        let preset = match g.preset {
            Some(RetentionPreset::Standard) => "standard",
            Some(RetentionPreset::Light) => "light",
            Some(RetentionPreset::Long) => "long",
            None => "custom",
        };
        let s = |n: u32| RwSignal::new(n.to_string());
        Fields {
            description: RwSignal::new(p.map(|p| p.description.clone()).unwrap_or_default()),
            key: RwSignal::new(p.map(|p| p.public_key.clone()).unwrap_or_default()),
            alert: RwSignal::new(p.map(|p| p.alert_after_hours()).unwrap_or(default_alert() * 24).to_string()),
            allowed: RwSignal::new(d.allowed),
            keep: RwSignal::new(d.keep_last.to_string()),
            min_age: RwSignal::new(d.min_age_days.to_string()),
            owner: RwSignal::new(if r.owner == RetentionOwner::Nas { "nas" } else { "pc" }.to_string()),
            preset: RwSignal::new(preset.to_string()),
            counts: [s(g.hourly), s(g.daily), s(g.weekly), s(g.monthly), s(g.yearly)],
            min: RwSignal::new(g.min.clone()),
            ignored: RwSignal::new(p.map(|p| p.ignored_groups.clone()).unwrap_or_default()),
            key_err: RwSignal::new(None),
            desc_err: RwSignal::new(None),
        }
    }

    fn policy(self) -> DeletePolicy {
        let n = |s: RwSignal<String>| s.get_untracked().trim().parse::<u32>().unwrap_or(0);
        DeletePolicy { allowed: self.allowed.get_untracked(), keep_last: n(self.keep), min_age_days: n(self.min_age) }
    }

    fn alert_hours(self) -> u32 {
        self.alert.get_untracked().trim().parse().unwrap_or(0)
    }

    /// The days of older clients: the hours rounded up (the hours win on the NAS).
    fn alert_days(self) -> u32 {
        self.alert_hours().div_ceil(24)
    }

    fn retention(self) -> PcRetention {
        let owner = if self.owner.get_untracked() == "nas" { RetentionOwner::Nas } else { RetentionOwner::Pc };
        let gfs = match self.preset.get_untracked().as_str() {
            "standard" => RetentionPreset::Standard.retention(),
            "light" => RetentionPreset::Light.retention(),
            "long" => RetentionPreset::Long.retention(),
            _ => {
                let n = |i: usize| self.counts[i].get_untracked().trim().parse::<u32>().unwrap_or(0);
                Retention { preset: None, hourly: n(0), daily: n(1), weekly: n(2), monthly: n(3), yearly: n(4), min: self.min.get_untracked().trim().to_string() }
            }
        };
        PcRetention { owner, gfs }
    }

    /// `groups`: the PC's subvolumes (the "no longer sent" choice; none in the add dialog).
    /// `preview`: the preview of the settings dialog. `agent`: sent by rynas-agent (no key,
    /// the NAS always owns retention).
    /// `extra`: shown right after the subvolume choice (the agent's other excludes).
    fn view(self, groups: Vec<String>, extra: Option<AnyView>, preview: Option<AnyView>, agent: Signal<bool>) -> AnyView {
        let nas = move || agent.get() || self.owner.get() == "nas";
        let custom = move || self.preset.get() == "custom";
        let count = move |label: &'static str, i: usize| number_field(label, None, self.counts[i].into(), Callback::new(move |v| self.counts[i].set(v)), 0, 1000);
        let ignored = (!groups.is_empty()).then(|| {
            view! {
                <div class="field">
                    <span class="field-label">{move || if agent.get() { t("pcb.f.not_sent") } else { t("bkpc.f.ignored") }}</span>
                    {groups
                        .into_iter()
                        .map(|g| {
                            let g2 = g.clone();
                            let on = move || self.ignored.with(|v| v.contains(&g2));
                            let g3 = g.clone();
                            view! {
                                <label class="check">
                                    <input type="checkbox" prop:checked=on on:change=move |ev| {
                                        let c = event_target_checked(&ev);
                                        self.ignored.update(|v| { v.retain(|x| x != &g3); if c { v.push(g3.clone()); } });
                                    } />
                                    <span class="mono">{g}</span>
                                </label>
                            }
                        })
                        .collect_view()}
                    <span class="field-help">{move || if agent.get() { t("pcb.f.not_sent_help") } else { t("bkpc.f.ignored_help") }}</span>
                </div>
            }
        });
        view! {
            <Show when=move || !agent.get()>
                {area_field("bk.f.key", self.key.into(), Callback::new(move |v| { self.key_err.set(None); self.key.set(v); }), Some("bk.f.key_help"), "ssh-ed25519 AAAA… btrbk@pc")}
                {form_error(self.key_err)}
            </Show>
            {number_field("bkpc.f.alert_hours", Some("bkpc.f.alert_hours_help"), self.alert.into(), Callback::new(move |v| self.alert.set(v)), 0, 8760)}
            {ignored}
            {extra}
            <h3 class="mini-title">{tr("bk.f.rules")}</h3>
            <Show when=move || !agent.get()>
                {segmented("bkpc.f.owner", OWNERS, self.owner.into(), Callback::new(move |v| self.owner.set(v)))}
            </Show>
            <p class="field-help">{move || if agent.get() { t("bkpc.owner.agent_help") } else if nas() { t("bkpc.owner.nas_help") } else { t("bkpc.owner.pc_help") }}</p>
            <Show when=nas>
                {segmented("snap.f.preset", PRESETS, self.preset.into(), Callback::new(move |v| self.preset.set(v)))}
                {move || if custom() {
                    view! {
                        <div class="form-row three">
                            {count("snap.f.hourly", 0)}
                            {count("snap.f.daily", 1)}
                            {count("snap.f.weekly", 2)}
                            {count("snap.f.monthly", 3)}
                            {count("snap.f.yearly", 4)}
                            {text_field("snap.f.min", self.min.into(), Callback::new(move |v| self.min.set(v)), Signal::derive(|| None), In { mono: true, help: Some("snap.f.min_help"), ..In::default() })}
                        </div>
                    }
                    .into_any()
                } else {
                    let r = self.retention().gfs;
                    view! { <p class="field-help">{tf("snap.preset_keeps", &[("counts", &retention_text(&r)), ("min", &age_text(&r.min))])}</p> }.into_any()
                }}
            </Show>
            <Show when=move || !nas()>
                {switch("bk.f.allowed", Some("bk.f.allowed_help"), self.allowed.into(), Callback::new(move |v| self.allowed.set(v)))}
            </Show>
            <Show when=move || nas() || self.allowed.get()>
                <div class="form-row">
                    {move || number_field("bk.f.keep", Some(if nas() { "bkpc.f.keep_nas_help" } else { "bkpc.f.keep_help" }), self.keep.into(), Callback::new(move |v| self.keep.set(v)), 0, 1000)}
                    {move || number_field("bk.f.min_age", Some(if nas() { "bkpc.f.min_age_nas_help" } else { "bk.f.min_age_help" }), self.min_age.into(), Callback::new(move |v| self.min_age.set(v)), 0, 3650)}
                </div>
            </Show>
            {preview}
            <Show when=move || !nas()>
                <p class="small muted">{tr("bk.f.rules_note")}</p>
            </Show>
        }
        .into_any()
    }
}

/// What adopting changes on the folder itself: owner, group and mode before → after.
fn perm_view(c: &PermissionChange) -> AnyView {
    let owner = c.owner.clone().unwrap_or_else(|| c.uid.to_string());
    let group = c.group.clone().unwrap_or_else(|| c.gid.to_string());
    let before = format!("{owner}:{group} {:04o}", c.mode & 0o7777);
    let after = format!("{}:{} {:04o}", c.new_owner, c.new_group, c.new_mode & 0o7777);
    ui::note(
        Tone::Warn,
        icons::TRIANGLE_ALERT,
        view! {
            <div class="cell-main">
                <span>{tr("bk.adopt.perm_change")}</span>
                <span class="mono small bk-wrap">{before}" → "{after}</span>
            </div>
        }
        .into_any(),
    )
}

fn add_dialog(pcs: Pcs, adopt_first: bool) -> AnyView {
    let close = Callback::new(move |_| pcs.add.set(None));
    // how the PC sends: rynas-agent over HTTPS (recommended) or btrbk over SSH
    let how = RwSignal::new("agent".to_string());
    let agent = Signal::derive(move || how.get() == "agent");
    // with the agent: a PC to pair now ("" = a new one), or one already paired
    let linked = RwSignal::new(String::new());
    let paired = Remote::<rynas_api_types::pcagent::PcBackupOverview>::new("/api/v1/pcbackup");
    let linkable = move || {
        paired.data.with(|o| {
            o.as_ref()
                .map(|o| o.devices.iter().filter(|d| !d.revoked && d.settings.method == PcMethod::Files).cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        })
    };
    let mode = RwSignal::new(if adopt_first { "adopt" } else { "new" }.to_string());
    let device = RwSignal::new(String::new());
    let path = RwSignal::new(String::new());
    let path_touched = RwSignal::new(false);
    let f = Fields::new(None, adopt_first);
    if !adopt_first {
        f.owner.set("nas".into());
    }
    let dev_err = RwSignal::new(None::<String>);
    let path_err = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let checking = RwSignal::new(false);
    let preview = RwSignal::new(None::<AdoptPreview>);
    // adopting changes the folder's owner, group or mode: confirmed explicitly
    let perm_ok = RwSignal::new(false);
    let needs_perm = move || preview.with(|p| p.as_ref().is_some_and(|p| p.permission_change.is_some()));
    let base = move || pcs.setup.data.with(|s| s.as_ref().and_then(|s| s.volumes.first().cloned())).unwrap_or_else(|| "/volume1".into());
    let suggest = move |d: &str| if d.is_empty() { String::new() } else { format!("{}/backup/{d}", base()) };
    let route = move |e: api::ApiErr| match e.field() {
        Some("device") => dev_err.set(Some(e.text())),
        Some("path") => path_err.set(Some(e.text())),
        Some("public_key") => f.key_err.set(Some(e.text())),
        Some("description") => f.desc_err.set(Some(e.text())),
        _ => error.set(Some(e.text())),
    };
    let set_device = move |v: String| {
        dev_err.set(None);
        if !path_touched.get_untracked() && mode.get_untracked() == "new" {
            path.set(suggest(v.trim()));
        }
        device.set(v);
    };
    let check = move |_| {
        checking.set(true);
        path_err.set(None);
        error.set(None);
        preview.set(None);
        perm_ok.set(false);
        let body = AdoptRequest { path: path.get_untracked().trim().to_string() };
        spawn_local(async move {
            let r = api::post::<_, AdoptPreview>("/api/v1/pc-backups/plan/adopt", Some(&body)).await;
            let _ = checking.try_set(false);
            match r {
                Ok(p) => {
                    if device.get_untracked().is_empty()
                        && let Some(d) = &p.suggested_device
                    {
                        device.set(d.clone());
                    }
                    preview.set(Some(p));
                }
                Err(e) => route(e),
            }
        });
    };
    let save = move |_| {
        let adopt = mode.get_untracked() == "adopt";
        let is_agent = how.get_untracked() == "agent";
        // a linked PC keeps its name: the folder takes it
        let dev = match linked.get_untracked() {
            l if is_agent && !l.is_empty() => l,
            _ => device.get_untracked().trim().to_string(),
        };
        let mut retention = f.retention();
        if is_agent {
            retention.owner = RetentionOwner::Nas;
        }
        let body = PcTargetCreate {
            device: dev.clone(),
            description: f.description.get_untracked().trim().to_string(),
            path: path.get_untracked().trim().to_string(),
            adopt,
            public_key: if is_agent { String::new() } else { f.key.get_untracked().trim().to_string() },
            alert_days: f.alert_days(),
            delete: f.policy(),
            alert_hours: f.alert_hours(),
            retention,
            agent: is_agent,
        };
        // an already paired PC moves to this folder once it exists
        let link = if is_agent { paired.data.with_untracked(|o| o.as_ref().and_then(|o| o.devices.iter().find(|d| d.device == linked.get_untracked()).cloned())) } else { None };
        let ok = tf("bk.added", &[("device", &dev)]);
        let url = if adopt && preview.with_untracked(|p| p.as_ref().is_some_and(|p| p.permission_change.is_some())) { format!("{URL}?confirm_permission_change=true") } else { URL.to_string() };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let started = api::start_task(&url, serde_json::to_string(&body).ok()).await;
            let r = match started {
                Ok(id) => tasks::follow(&id).await,
                Err(e) => {
                    let _ = busy.try_set(false);
                    route(e);
                    return;
                }
            };
            match r {
                Ok(task) if task.status == TaskStatus::Ok => {
                    if let Some(d) = link {
                        let mut s = d.settings.clone();
                        s.method = PcMethod::Btrfs;
                        let u = format!("/api/v1/pcbackup/devices/{}", api::encode(&d.device));
                        if let Err(e) = api::put::<_, serde_json::Value>(&u, &s).await {
                            let _ = busy.try_set(false);
                            let _ = error.try_set(Some(tf("pcb.link_failed", &[("error", &e.text())])));
                            pcs.list.reload();
                            return;
                        }
                    }
                    let _ = busy.try_set(false);
                    toast(Tone::Ok, ok);
                    pcs.list.reload();
                    pcs.add.set(None);
                    pcs.open.set(Some(dev));
                }
                Ok(task) => {
                    let _ = busy.try_set(false);
                    let _ = error.try_set(Some(tasks::task_error(&task)));
                }
                Err(e) => {
                    let _ = busy.try_set(false);
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let preview_view = move || {
        preview.with(|p| {
            p.as_ref().map(|p| {
                let groups = p
                    .groups
                    .iter()
                    .map(|g| ui::row(vec![ui::text_cell("bk.col.subvolume", "mono", g.name.clone()), ui::text_cell("bk.col.snapshots", "num tnum", i18n::count(u64::from(g.snapshots))), ui::text_cell("bk.col.newest", "tnum", when(g.newest_ms))]))
                    .collect::<Vec<_>>();
                let incomplete = p.list.iter().filter(|s| s.incomplete).count() as u64;
                view! {
                    {if p.groups.is_empty() {
                        ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("bk.adopt.none").to_string()))
                    } else {
                        ui::note(Tone::Ok, icons::CIRCLE_CHECK, ui::text(tf("bk.adopt.found", &[("n", &i18n::count(p.list.len() as u64))])))
                    }}
                    {(!p.groups.is_empty()).then(|| ui::table("bk-groups", &["bk.col.subvolume", "#bk.col.snapshots", "bk.col.newest"], groups))}
                    {(incomplete > 0).then(|| view! { <p class="small muted">{tf("bk.adopt.incomplete", &[("n", &i18n::count(incomplete))])}</p> })}
                    {(p.other_entries > 0).then(|| view! { <p class="small muted">{tf("bk.adopt.other", &[("n", &i18n::count(u64::from(p.other_entries)))])}</p> })}
                    {(!p.subvolume).then(|| view! { <p class="small muted">{tr("bk.adopt.not_subvolume")}</p> })}
                    {p.permission_change.as_ref().map(perm_view)}
                }
                .into_any()
            })
        })
    };
    // the PC to connect: a new one (pairing code in the drawer) or an already paired one
    let pc_choice = move || {
        let list = linkable();
        (agent.get() && !list.is_empty()).then(|| {
            view! {
                <div class="field">
                    <label class="field-label" for="pcb-link">{tr("pcb.f.pc")}</label>
                    <select id="pcb-link" class="input" on:change=move |ev| {
                        let v = event_target_value(&ev);
                        if !v.is_empty() {
                            set_device(v.clone());
                        }
                        linked.set(v);
                    }>
                        <option value="" selected=move || linked.get().is_empty()>{tr("pcb.f.pc_new")}</option>
                        {list.iter().map(|d| {
                            let v = d.device.clone();
                            let v2 = v.clone();
                            let mut label = if d.hostname.is_empty() { v.clone() } else { format!("{} ({})", v, d.hostname) };
                            // the last run says how `/` was read: a root that is not btrfs cannot send snapshots
                            let not_btrfs = d.last_run.as_ref().and_then(|r| r.captures.iter().find(|c| c.path == "/")).is_some_and(|c| c.fstype != "btrfs");
                            if not_btrfs {
                                label = tf("pcb.f.pc_no_btrfs", &[("pc", &label)]);
                            }
                            view! { <option value=v selected=move || linked.get() == v2 disabled=not_btrfs>{label}</option> }
                        }).collect_view()}
                    </select>
                    <span class="field-help">{tr("pcb.f.pc_help")}</span>
                </div>
            }
        })
    };
    const HOWS: &[(&str, &str)] = &[("agent", "pcb.how.agent"), ("ssh", "pcb.how.ssh")];
    const MODES: &[(&str, &str)] = &[("new", "bk.mode.new"), ("adopt", "shares.loc_folder")];
    let body = view! {
        {segmented("pcb.how", HOWS, how.into(), Callback::new(move |v: String| {
            // the agent always lets the NAS delete; btrbk on an adopted chain keeps deleting
            if v == "agent" || mode.get_untracked() == "new" {
                f.owner.set("nas".into());
            } else {
                f.owner.set("pc".into());
            }
            if v != "agent" {
                linked.set(String::new());
            }
            how.set(v);
        }))}
        <p class="field-help">{move || if agent.get() { t("pcb.how.agent_help") } else { t("pcb.how.ssh_help") }}</p>
        {pc_choice}
        {segmented("bk.mode", MODES, mode.into(), Callback::new(move |v: String| {
            preview.set(None);
            path_err.set(None);
            // a new PC lets the NAS delete; an adopted chain keeps btrbk on the PC deleting
            f.owner.set(if v == "adopt" && !agent.get_untracked() { "pc" } else { "nas" }.to_string());
            mode.set(v);
        }))}
        <p class="field-help">{move || if mode.get() == "adopt" { t("bk.mode.adopt_help") } else { t("bk.mode.new_help") }}</p>
        <Show when=move || linked.with(String::is_empty)>
            {text_field("bk.f.device", device.into(), Callback::new(set_device), dev_err.into(), In { mono: true, maxlength: 32, help: Some("bk.f.device_help"), placeholder: "laptop", ..In::default() })}
        </Show>
        {text_field("bk.f.description", f.description.into(), Callback::new(move |v| { f.desc_err.set(None); f.description.set(v); }), f.desc_err.into(), In { maxlength: 120, ..In::default() })}
        {text_field("bkpc.f.path", path.into(), Callback::new(move |v| { path_err.set(None); path_touched.set(true); preview.set(None); path.set(v); }), path_err.into(), In { mono: true, maxlength: 512, help: Some("bkpc.f.path_help"), placeholder: "/volume1/backup/laptop", ..In::default() })}
        <Show when=move || mode.get() == "adopt">
            <div class="sec-actions">
                <button type="button" class="btn ghost sm" disabled=move || checking.get() || path.with(|p| p.trim().is_empty()) on:click=check>{spinner(checking)}<Icon icon=icons::SEARCH size=14 />{tr("bk.adopt.check")}</button>
            </div>
            {preview_view}
            <Show when=needs_perm>
                <label class="check">
                    <input type="checkbox" prop:checked=move || perm_ok.get() on:change=move |ev| perm_ok.set(event_target_checked(&ev)) />
                    <span>{tr("bk.adopt.perm_confirm")}</span>
                </label>
            </Show>
        </Show>
        {f.view(Vec::new(), None, None, agent)}
        <Show when=move || agent.get() && linked.get().is_empty()>
            <p class="small muted">{tr("pcb.pair_after")}</p>
        </Show>
        {form_error(error)}
    }
    .into_any();
    let ready = move || (!device.with(|d| d.trim().is_empty()) || !linked.with(String::is_empty)) && !path.with(|p| p.trim().is_empty()) && (mode.get() == "new" || (preview.with(Option::is_some) && (!needs_perm() || perm_ok.get())));
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() || !ready() on:click=save>{spinner(busy)}<Icon icon=icons::PLUS size=16 />{move || if mode.get() == "adopt" { t("bk.adopt") } else { t("bkpc.create") }}</button>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::derive(move || t("bk.add").to_string()), close, body, Some(foot))
}

fn settings_dialog(pcs: Pcs, p: PcTarget, agent_dev: Option<PcDevice>) -> AnyView {
    let close = Callback::new(move |_| pcs.edit.set(None));
    let f = Fields::new(Some(&p), p.adopted);
    let was_agent = p.agent;
    let how = RwSignal::new(if p.agent { "agent" } else { "ssh" }.to_string());
    let agent = Signal::derive(move || how.get() == "agent");
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let device = StoredValue::new(p.device.clone());
    // the agent paired under this name: its schedule and the subvolumes it leaves out are set
    // here too (one dialog per PC); a revoked agent has nothing left to set
    let live = agent_dev.filter(|d| p.agent && !d.revoked);
    let has_agent = live.is_some();
    let plan = live.as_ref().map(|d| d.settings.plan.clone()).unwrap_or_default();
    let interval = RwSignal::new(plan.interval_hours.to_string());
    let delay = RwSignal::new(plan.start_delay_min.to_string());
    let paused = RwSignal::new(if plan.paused { "paused" } else { "on" }.to_string());
    // a subvolume left out = its mount point in the agent's excludes and no alert for it on
    // the NAS; excludes that name no known subvolume stay as written
    let mounts: Vec<(String, String)> = p.bare_metal.layout.iter().filter(|s| !s.mount.is_empty()).map(|s| (s.name.clone(), s.mount.trim_end_matches('/').to_string())).collect();
    let norm = |e: &str| if e == "/" { "/".to_string() } else { e.trim_end_matches('/').to_string() };
    let mount_of = |m: &str| if m.is_empty() { "/".to_string() } else { m.to_string() };
    let mounts: Vec<(String, String)> = mounts.into_iter().map(|(n, m)| (n, mount_of(&m))).collect();
    if has_agent {
        let ex: Vec<String> = plan.excludes.iter().map(|e| norm(e)).collect();
        f.ignored.update(|v| {
            for (n, m) in &mounts {
                if ex.contains(m) && !v.contains(n) {
                    v.push(n.clone());
                }
            }
        });
    }
    let other = RwSignal::new(plan.excludes.iter().filter(|e| !mounts.iter().any(|(_, m)| *m == norm(e))).cloned().collect::<Vec<_>>().join("\n"));
    let show_other = has_agent && (mounts.is_empty() || !other.with_untracked(String::is_empty));
    let mounts = StoredValue::new(mounts);
    let agent_body = live.clone().map(|d| {
        let mut s = d.settings;
        s.plan.excludes.clear();
        s
    });
    let save = move |_| {
        let is_agent = how.get_untracked() == "agent";
        let mut retention = f.retention();
        if is_agent {
            retention.owner = RetentionOwner::Nas;
        }
        let description = f.description.get_untracked().trim().to_string();
        let body = PcTargetUpdate {
            description: description.clone(),
            public_key: if is_agent { String::new() } else { f.key.get_untracked().trim().to_string() },
            alert_days: f.alert_days(),
            delete: f.policy(),
            alert_hours: f.alert_hours(),
            retention: Some(retention),
            ignored_groups: Some(f.ignored.get_untracked()),
            agent: (is_agent != was_agent).then_some(is_agent),
        };
        let folder = ("PUT", target_url(&device.get_value()), serde_json::to_string(&body).ok());
        let done = Callback::new(move |_| {
            pcs.edit.set(None);
            pcs.saved.update(|n| *n += 1);
        });
        // the agent's schedule first (answers at once), then the folder (a task)
        let agent_settings = agent_body.clone().filter(|_| is_agent).map(|mut s| {
            let n = |s: RwSignal<String>| s.get_untracked().trim().parse::<u32>().unwrap_or(0);
            s.description = description;
            s.plan.interval_hours = n(interval);
            s.plan.start_delay_min = n(delay);
            s.plan.paused = paused.get_untracked() == "paused";
            let ignored = f.ignored.get_untracked();
            let mut ex: Vec<String> = other.get_untracked().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            for (n, m) in mounts.get_value() {
                if ignored.contains(&n) && !ex.iter().any(|e| norm(e) == m) {
                    ex.push(m);
                }
            }
            s.plan.excludes = ex;
            s
        });
        match agent_settings {
            None => run(pcs, folder, t("bk.saved").to_string(), (busy, error, Some(f)), done),
            Some(s) => {
                busy.set(true);
                error.set(None);
                let u = super::agent::dev_url(&device.get_value());
                spawn_local(async move {
                    match api::put::<_, PcDevice>(&u, &s).await {
                        Ok(_) => run(pcs, folder, t("bk.saved").to_string(), (busy, error, Some(f)), done),
                        Err(e) => {
                            let _ = busy.try_set(false);
                            let _ = error.try_set(Some(e.text()));
                        }
                    }
                });
            }
        }
    };
    // what the NAS would delete now with the settings in the form (nothing is saved)
    let previewing = RwSignal::new(false);
    let shown = RwSignal::new(None::<PcRetentionPreview>);
    let run_preview = move |_| {
        previewing.set(true);
        error.set(None);
        let mut retention = f.retention();
        if agent.get_untracked() {
            retention.owner = RetentionOwner::Nas;
        }
        let body = PcRetentionPlanRequest { device: device.get_value(), retention, delete: f.policy() };
        spawn_local(async move {
            let r = api::post::<_, PcRetentionPreview>("/api/v1/pc-backups/plan/retention", Some(&body)).await;
            let _ = previewing.try_set(false);
            match r {
                Ok(p) => {
                    let _ = shown.try_set(Some(p));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let preview = view! {
        <Show when=move || agent.get() || f.owner.get() == "nas">
            <div>
                <button type="button" class="btn ghost sm" disabled=move || previewing.get() on:click=run_preview>
                    {spinner(previewing)}<Icon icon=icons::EYE size=14 />{tr("snap.preview")}
                </button>
            </div>
            {move || shown.get().map(|p| {
                let keep = (p.items.len() as u32).saturating_sub(p.delete);
                let frees = p.frees_at_least_bytes.filter(|b| *b > 0).map(|b| tf("bkpc.preview_frees", &[("bytes", &i18n::bytes(b))]));
                view! { <p class="small">{tf("snap.preview_counts", &[("keep", &keep.to_string()), ("delete", &p.delete.to_string())])}" "{frees}</p> }
            })}
        </Show>
    }
    .into_any();
    let mut groups: std::collections::BTreeSet<String> = p.groups.iter().map(|g| g.name.clone()).chain(p.ignored_groups.iter().cloned()).collect();
    if has_agent {
        groups.extend(mounts.get_value().into_iter().map(|(n, _)| n));
    }
    let groups: Vec<String> = groups.into_iter().collect();
    const HOWS: &[(&str, &str)] = &[("agent", "pcb.how.agent"), ("ssh", "pcb.how.ssh")];
    const PAUSED: &[(&str, &str)] = &[("on", "pca.f.scheduled"), ("paused", "pca.f.paused")];
    let schedule = has_agent.then(|| {
        view! {
            <Show when=move || agent.get()>
                <h3 class="mini-title">{tr("pca.f.schedule")}</h3>
                {select_field("pca.f.state", None, PAUSED, paused.into(), Callback::new(move |v| paused.set(v)))}
                <div class="form-row">
                    {number_field("pca.f.interval", Some("pca.f.interval_help"), interval.into(), Callback::new(move |v| interval.set(v)), 1, 168)}
                    {number_field("pca.f.delay", Some("pca.f.delay_help"), delay.into(), Callback::new(move |v| delay.set(v)), 0, 1440)}
                </div>
            </Show>
        }
    });
    let other_view = show_other.then(|| {
        view! {
            <Show when=move || agent.get()>
                {area_field("pcb.f.other_mounts", other.into(), Callback::new(move |v| other.set(v)), Some("pcb.f.other_mounts_help"), "/srv/vm")}
            </Show>
        }
    });
    // Revoke: rare and final, so here rather than next to Back up now
    let revoke = has_agent.then(|| {
        view! {
            <Show when=move || was_agent && agent.get()>
                <h3 class="mini-title">{tr("pcb.agent.title")}</h3>
                <p class="small muted">{tr("pcb.agent.revoke_help")}</p>
                <div class="sec-actions">
                    <button type="button" class="btn danger sm" on:click=move |_| { pcs.edit.set(None); pcs.ask_revoke.set(true); }><Icon icon=icons::TRASH_2 size=14 />{tr("pca.revoke")}</button>
                </div>
            </Show>
        }
    });
    let body = view! {
        {text_field("bk.f.description", f.description.into(), Callback::new(move |v| { f.desc_err.set(None); f.description.set(v); }), f.desc_err.into(), In { maxlength: 120, ..In::default() })}
        {segmented("pcb.how", HOWS, how.into(), Callback::new(move |v: String| how.set(v)))}
        <p class="field-help">{move || match (was_agent, agent.get()) {
            (false, true) => t("pcb.switch_to_agent"),
            (true, false) => t("pcb.switch_to_ssh"),
            (_, true) => t("pcb.how.agent_help"),
            (_, false) => t("pcb.how.ssh_help"),
        }}</p>
        {schedule}
        {f.view(groups, other_view.map(IntoAny::into_any), Some(preview), agent)}
        {revoke}
        {form_error(error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() on:click=save>{spinner(busy)}{tr("save")}</button>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::stored(tf("bk.settings_title", &[("device", &p.device)])), close, body, Some(foot))
}

// ------------------------------------------------------------------ drawer

/// "Restore bare metal" for a PC of this folder (sent by rynas-agent or by btrbk): a
/// one-time restore code (one hour, read-only, this PC only) and the commands to run on the
/// PC from any live Linux; the agent receives the snapshots back into a new btrfs pool
/// (`docs/research/pc-backup.md` §11.2 point 6, §11.10).
fn restore_part(device: String) -> AnyView {
    use rynas_api_types::pcagent::RestoreCodeCreated;
    let code = RwSignal::new(None::<RestoreCodeCreated>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let dev = StoredValue::new(device);
    let make = move |_| {
        let u = format!("/api/v1/pcbackup/devices/{}/restore-code", api::encode(&dev.get_value()));
        error.set(None);
        busy.set(true);
        spawn_local(async move {
            let r = api::post::<(), RestoreCodeCreated>(&u, None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(c) => code.set(Some(c)),
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    let host = super::agent::origin();
    view! {
        <h3 class="mini-title">{tr("bkpc.restore.title")}</h3>
        <p class="small muted">{tr("bkpc.restore.intro")}</p>
        {form_error(error)}
        <div class="sec-actions">
            <button class="btn ghost sm" disabled=move || busy.get() on:click=make><Icon icon=icons::LIFE_BUOY size=14 />{tr("pca.restore_code")}</button>
        </div>
        {move || code.get().map(|c| {
            let cmd = format!("curl -fLO https://github.com/rylos/rynas/releases/latest/download/rynas-agent-x86_64\nchmod +x rynas-agent-x86_64\nsudo ./rynas-agent-x86_64 restore --nas {host}");
            view! {
                {ui::note(Tone::Ok, icons::LIFE_BUOY, ui::text(tf("pca.restore_code_valid", &[("code", &c.code), ("until", &i18n::datetime(c.expires_ms))])))}
                <p class="small muted">{tr("pca.restore_steps")}</p>
                <pre class="app-code">{cmd.clone()}</pre>
                <div class="sec-actions">{copy_button_for("dns.copy", t("bkpc.restore.copy").to_string(), cmd)}</div>
            }
        })}
    }
    .into_any()
}

fn drawer(pcs: Pcs, device: String) -> AnyView {
    let close = Callback::new(move |_| pcs.open.set(None));
    let dev = StoredValue::new(device.clone());
    let host = window().location().hostname().unwrap_or_default();
    let detail = Remote::<PcTargetDetail>::new(format!("{}?host={}", target_url(&device), api::encode(&host)));
    pcs.ask_revoke.set(false);
    let ctl = super::agent::AgentCtl::new(device.clone(), pcs.ask_revoke, Callback::new(move |_| detail.reload()));
    let browse = RwSignal::new(None::<String>);
    let ask = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let measuring = RwSignal::new(false);
    let measure_err = RwSignal::new(None::<String>);
    let measure = move |_| {
        run(pcs, ("POST", format!("{}/usage", target_url(&dev.get_value())), None), t("bk.measured").to_string(), (measuring, measure_err, None), Callback::new(move |_| detail.reload()));
    };
    let remove = Callback::new(move |_| {
        run(pcs, ("DELETE", target_url(&dev.get_value()), None), tf("bk.removed", &[("device", &dev.get_value())]), (busy, error, None), Callback::new(move |_| {
            ask.set(false);
            pcs.open.set(None);
        }));
    });
    // one busy flag and error line for the actions on snapshots (sizes, cleanup, delete)
    let working = RwSignal::new(false);
    let work_err = RwSignal::new(None::<String>);
    let reload = Callback::new(move |_| detail.reload());
    let sizes = move |_| run(pcs, ("POST", format!("{}/sizes", target_url(&dev.get_value())), None), t("bkpc.sizes_done").to_string(), (working, work_err, None), reload);
    let prune = move |_| run(pcs, ("POST", format!("{}/prune", target_url(&dev.get_value())), None), t("bkpc.pruned").to_string(), (working, work_err, None), reload);
    let by_run = RwSignal::new("subvolume".to_string());
    let del = RwSignal::new(None::<String>);
    let del_busy = RwSignal::new(false);
    let del_err = RwSignal::new(None::<String>);
    let delete_snap = Callback::new(move |_| {
        let Some(name) = del.get_untracked() else { return };
        let url = format!("{}/snapshots/{}", target_url(&dev.get_value()), api::encode(&name));
        run(pcs, ("DELETE", url, None), tf("bkpc.deleted_snap", &[("name", &name)]), (del_busy, del_err, None), Callback::new(move |_| {
            del.set(None);
            detail.reload();
        }));
    });
    let content = move || {
        let Some(d) = detail.data.get() else { return ().into_any() };
        let p = d.target.clone();
        let p_edit = p.clone();
        let nas = p.retention.owner == RetentionOwner::Nas;
        let usage = p.usage.map(|u| tf("bk.usage_text", &[("excl", &i18n::bytes(u.exclusive_bytes)), ("total", &i18n::bytes(u.total_bytes)), ("when", &i18n::datetime(u.read_at_ms))])).unwrap_or_else(|| t("bk.usage_none").to_string());
        let free = p.free_bytes.zip(p.volume_bytes).map(|(f, v)| tf("bkpc.free_text", &[("free", &i18n::bytes(f)), ("total", &i18n::bytes(v))]));
        let alert = match p.alert_after_hours() {
            0 => t("bk.alert_off").to_string(),
            h if h % 24 == 0 => tf("bk.alert_days", &[("n", &(h / 24).to_string())]),
            h => tf("bkpc.alert_hours", &[("n", &h.to_string())]),
        };
        let issues = p
            .issues
            .iter()
            .map(|i| {
                let (tone, icon) = match i.code {
                    PcIssueCode::LowSpace => (Tone::Err, icons::TRIANGLE_ALERT),
                    PcIssueCode::Late | PcIssueCode::SubvolumeLate => (Tone::Warn, icons::CLOCK),
                    _ => (Tone::Warn, icons::TRIANGLE_ALERT),
                };
                ui::note(tone, icon, ui::text(issue_text(i)))
            })
            .collect_view();
        let b = p.bare_metal.clone();
        // "no snapshots yet" is said once, by the snapshot list
        let shown_gaps: Vec<&BareMetalGap> = b.missing.iter().chain(&b.warnings).filter(|g| g.code != BareMetalCode::NoSnapshots).collect();
        let has_gaps = !shown_gaps.is_empty();
        // btrbk without a manifest: the drop-in of the manual steps writes it
        let no_manifest = shown_gaps.iter().any(|g| g.code == BareMetalCode::NoManifest);
        let agent = p.agent;
        let gaps = shown_gaps.into_iter().map(|g| view! { <li>{gap_text(g, agent)}</li> }).collect_view();
        let layout = (!b.layout.is_empty()).then(|| {
            let items = b.layout.iter().map(|s| {
                let at = if s.mount.is_empty() { t("rm.tip_not_mounted").to_string() } else { s.mount.clone() };
                view! { <li><span class="mono">{s.name.clone()}</span>" → "<span class="mono">{at}</span></li> }
            }).collect_view();
            view! { <h4 class="small"><strong>{tr("bkpc.bm.layout")}</strong></h4><ul class="small bkpc-list">{items}</ul> }
        });
        let disks = (!b.disks.is_empty()).then(|| view! { <h4 class="small"><strong>{tr("bkpc.bm.disks")}</strong></h4><ul class="small bkpc-list">{disk_items(&b.disks)}</ul> });
        let copies = (!b.boot_copies.is_empty()).then(|| view! { <h4 class="small"><strong>{tr("bkpc.bm.boot_copies")}</strong></h4><ul class="small bkpc-list">{boot_copy_items(&b.boot_copies)}</ul> });
        let bare = view! {
            <h3 class="mini-title">{tr("bkpc.bm.title")}</h3>
            <p class="small muted">{tr("bkpc.bm.intro")}</p>
            <dl class="kvs">
                {kv("col.status", bm_status(&b))}
                {b.manifest_snapshot.clone().map(|s| kv("bkpc.bm.manifest", ui::breakable(&s, "mono small")))}
                {(b.layout.is_empty() && !b.subvolumes.is_empty()).then(|| kv("bkpc.bm.subvolumes", ui::breakable(&b.subvolumes.join(", "), "mono small")))}
                {b.loader.clone().map(|l| kv("bkpc.bm.loader", ui::text(loader_label(&l))))}
            </dl>
            {has_gaps.then(|| view! { <ul class="small bkpc-gaps">{gaps}</ul> })}
            {layout}
            {disks}
            {copies}
        };
        let flag = |s: &ReceivedSnapshot| {
            if s.incomplete {
                ui::status(Tone::Warn, t("bk.snap.incomplete").to_string())
            } else if s.not_subvolume {
                ui::status(Tone::Err, t("bk.snap.not_subvolume").to_string())
            } else if s.parent {
                ui::status(Tone::Info, t("bkpc.snap.parent").to_string())
            } else if s.protected {
                ui::status(Tone::Info, t("bk.snap.protected").to_string())
            } else {
                ui::status(Tone::Ok, t("bk.snap.received").to_string())
            }
        };
        // by subvolume: grouped, newest first inside each
        let mut list = d.list.clone();
        list.sort_by(|a, b| a.group.cmp(&b.group).then_with(|| b.created_ms.cmp(&a.created_ms)));
        let snap_rows = list
            .iter()
            .map(|s| {
                let name = s.name.clone();
                let name_del = s.name.clone();
                let label = format!("{}: {}", t("bk.browse"), s.name);
                let label_del = format!("{}: {}", t("bkpc.delete_snap"), s.name);
                let can = s.readonly && !s.not_subvolume;
                let deletable = !s.parent && !s.not_subvolume && !(s.incomplete && p.receiving);
                ui::row(vec![
                    ui::cell("bk.col.snapshot", "wrap", ui::breakable(&s.name, "mono small")),
                    ui::text_cell("bk.col.received", "tnum", when(s.received_at_ms)),
                    ui::text_cell("snap.col.size", "tnum", size_text(s.size)),
                    ui::cell("col.status", "", flag(s)),
                    ui::actions(view! {
                        <button class="icon-btn sm" title=tr("bk.browse") aria-label=label disabled=!can on:click=move |_| browse.set(Some(name.clone()))><Icon icon=icons::FOLDER_OPEN size=16 /></button>
                        <button class="icon-btn sm" title=tr("bkpc.delete_snap") aria-label=label_del disabled=!deletable on:click=move |_| { del_err.set(None); del.set(Some(name_del.clone())); }><Icon icon=icons::TRASH_2 size=16 /></button>
                    }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        let run_rows = d
            .runs
            .iter()
            .map(|r| {
                let groups: Vec<String> = r.snapshots.iter().filter_map(|n| n.rsplit_once('.').map(|x| x.0.to_string())).collect();
                let size = match (r.referenced_bytes, r.exclusive_bytes) {
                    (Some(rb), Some(x)) => size_text(Some(SnapshotSize { referenced_bytes: rb, exclusive_bytes: x, ..Default::default() })),
                    _ => "—".into(),
                };
                let st = if r.incomplete {
                    ui::status(Tone::Warn, t("bk.snap.incomplete").to_string())
                } else if r.lacks.is_empty() {
                    ui::status(Tone::Ok, t("bkpc.run.full").to_string())
                } else {
                    ui::status(Tone::Info, tf("bkpc.run.lacks", &[("names", &r.lacks.join(", "))]))
                };
                ui::row(vec![
                    ui::text_cell("bkpc.view.run", "tnum", when(r.created_ms)),
                    ui::cell("bk.col.subvolume", "wrap", ui::breakable(&groups.join(", "), "mono small")),
                    ui::text_cell("snap.col.size", "tnum", size),
                    ui::cell("col.status", "wrap", st),
                ])
            })
            .collect::<Vec<_>>();
        let last_prune = p.last_prune.clone().map(|r| match r.error {
            Some(e) => format!("{}: {e}", i18n::datetime(r.at_ms)),
            None => tf("bkpc.last_prune_text", &[("when", &i18n::datetime(r.at_ms)), ("n", &r.deleted.to_string())]),
        });
        // how the PC is set up: nothing to do by hand with rynas-agent (its part is above);
        // with btrbk the commands, folded, then the first-backup check
        let check = first_run(&b, &d.list);
        // the manual steps open by themselves while they are the only way forward: no key
        // yet, or no manifest (its drop-in is one of them)
        let steps_open = (p.public_key.is_empty() && d.list.is_empty()) || no_manifest;
        let steps = if p.agent {
            check
        } else {
            let hook = (!d.manifest_hook.trim().is_empty()).then(|| {
                view! {
                    <p class="small muted">{tr("pcb.hook_intro")}</p>
                    {code_block("pcb.hook", d.manifest_hook.clone())}
                }
            });
            Some(
                view! {
                    <h3 class="mini-title">{tr("bk.steps")}</h3>
                    <p class="small muted">{tr("pcb.ssh_recommend")}</p>
                    <details class="tls-step" open=steps_open>
                        <summary>{tr("bkpc.manual_steps")}</summary>
                        <p class="small muted">{tr("bk.steps_intro")}</p>
                        {code_block("bk.step.key", d.key_commands.clone())}
                        {code_block("bk.step.config", d.client_config.clone())}
                        {code_block("bk.step.test", d.test_commands.clone())}
                        {d.key_line.clone().and_then(|l| code_block("bk.step.line", l))}
                        {hook}
                        <p class="small muted">{tr("bk.steps_note")}</p>
                    </details>
                    {check}
                }
                .into_any(),
            )
        };
        const VIEWS: &[(&str, &str)] = &[("subvolume", "bk.col.subvolume"), ("run", "bkpc.view.run")];
        view! {
            {(p.public_key.is_empty() && !p.agent).then(|| ui::note(Tone::Warn, icons::KEY, ui::text(t("bk.no_key").to_string())))}
            {issues}
            // with rynas-agent its block first: the PC's status, progress and schedule; the
            // folder below under its own title (one status, one date, one row of buttons)
            {agent.then(|| view! {
                {ctl.status()}
                <h3 class="mini-title">{tr("bkpc.f.path")}</h3>
            })}
            <dl class="kvs">
                {(!agent).then(|| kv("col.status", state_status(p.state)))}
                {kv("pcb.how", ui::text(t(if agent { "pcb.via.agent" } else { "pcb.via.ssh" }).to_string()))}
                {p.receiving.then(|| kv("bkpc.receiving", ui::status(Tone::Info, t("bkpc.receiving").to_string())))}
                {if agent {
                    let last = p.last_received_ms;
                    view! { {move || (!ctl.paired.get()).then(|| kv("bk.last", ui::text(when(last))))} }.into_any()
                } else {
                    kv("bk.last", ui::text(when(p.last_received_ms)))
                }}
                {kv("bk.col.snapshots", ui::text(i18n::count(u64::from(p.snapshots))))}
                {(p.incomplete > 0).then(|| kv("bk.incomplete", ui::text(i18n::count(u64::from(p.incomplete)))))}
                {kv("bk.col.folder", ui::breakable(&p.path, "mono"))}
                {free.map(|f| kv("bkpc.free", ui::text(f)))}
                {kv("bk.col.space", ui::text(usage))}
                {kv("bk.f.rules", ui::text(rules_text(&p)))}
                {last_prune.filter(|_| nas).map(|l| kv("bkpc.last_prune", ui::text(l)))}
                {kv("bkpc.alert", ui::text(alert))}
                {p.key_fingerprint.clone().map(|fp| kv("bk.key", ui::breakable(&fp, "mono small")))}
                {p.adopted.then(|| kv("bk.adopted", ui::text(t("bk.adopted_yes").to_string())))}
            </dl>
            {form_error(measure_err)}
            <div class="sec-actions">
                {agent.then(|| ctl.buttons())}
                <button class="btn ghost sm" disabled=move || measuring.get() on:click=measure>{spinner(measuring)}<Icon icon=icons::DATABASE size=14 />{tr("bk.measure")}</button>
                {nas.then(|| view! { <button class="btn ghost sm" disabled=move || working.get() on:click=prune><Icon icon=icons::TRASH_2 size=14 />{tr("bkpc.prune")}</button> })}
                <button class="btn ghost sm" on:click=move |_| pcs.edit.set(Some((p_edit.clone(), ctl.detail.data.with_untracked(|d| d.as_ref().map(|d| d.device.clone())))))><Icon icon=icons::SETTINGS size=14 />{tr("bk.settings")}</button>
                <button class="btn danger sm" on:click=move |_| { error.set(None); ask.set(true); }><Icon icon=icons::TRASH_2 size=14 />{tr("bk.remove")}</button>
            </div>
            {agent.then(|| ctl.history())}
            {bare}
            {(!d.list.is_empty()).then(|| restore_part(dev.get_value()))}
            {steps}
            <h3 class="mini-title">{tr("bk.snapshots")}</h3>
            {if snap_rows.is_empty() {
                view! { <p class="small muted">{tr("bk.no_snapshots")}</p> }.into_any()
            } else {
                view! {
                    {segmented("bkpc.view", VIEWS, by_run.into(), Callback::new(move |v| by_run.set(v)))}
                    <p class="small muted">{if d.sizes_from_quota { t("bkpc.sizes_quota") } else { t("bkpc.sizes_note") }}</p>
                    {(!d.sizes_from_quota).then(|| view! {
                        <div class="sec-actions">
                            <button class="btn ghost sm" disabled=move || working.get() on:click=sizes>{spinner(working)}<Icon icon=icons::DATABASE size=14 />{tr("bkpc.sizes")}</button>
                        </div>
                    })}
                    {form_error(work_err)}
                    <div hidden=move || by_run.get() != "subvolume">
                        {ui::table("bk-snaps", &["bk.col.snapshot", "bk.col.received", "snap.col.size", "col.status", ""], snap_rows)}
                    </div>
                    <div hidden=move || by_run.get() != "run">
                        {ui::table("bk-runs", &["bkpc.view.run", "bk.col.subvolume", "snap.col.size", "col.status"], run_rows)}
                    </div>
                }
                .into_any()
            }}
        }
        .into_any()
    };
    // the agent of a PC fed by rynas-agent: read once the folder says so (it polls on its own
    // while a run goes on)
    let is_agent = Memo::new(move |_| detail.data.with(|d| d.as_ref().map(|d| d.target.agent)));
    let started = StoredValue::new(false);
    Effect::new(move |_| {
        if is_agent.get() == Some(true) && !started.get_value() {
            started.set_value(true);
            ctl.start();
        }
    });
    // Settings saved: the folder and its agent again
    Effect::new(move |prev: Option<u32>| {
        let n = pcs.saved.get();
        if prev.is_some() {
            detail.reload();
            if started.get_value() {
                ctl.detail.reload();
            }
        }
        n
    });
    let body = view! { {move || detail.view(8, content)} }.into_any();
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    view! {
        <div class="wide-drawer">{ui::drawer(Signal::stored(device.clone()), close, body, foot)}</div>
        {ctl.confirm()}
        {move || ask.get().then(|| ui::confirm(Confirm { busy, error, ..Confirm::new(tf("bk.remove_title", &[("device", &dev.get_value())]), t("bk.remove_text").to_string(), "bk.remove", true, remove, Callback::new(move |_| ask.set(false))) }))}
        {move || browse.get().map(|s| browse_drawer(pcs, dev.get_value(), s, Callback::new(move |_| browse.set(None))))}
        {move || del.get().map(|n| ui::confirm(Confirm { busy: del_busy, error: del_err, ..Confirm::new(tf("bkpc.delete_snap_title", &[("name", &n)]), t("bkpc.delete_snap_text").to_string(), "bkpc.delete_snap", true, delete_snap, Callback::new(move |_| del.set(None))) }))}
    }
    .into_any()
}

// ------------------------------------------------------------------ browse and copy out

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn browse_drawer(pcs: Pcs, device: String, snapshot: String, close: Callback<()>) -> AnyView {
    let base = StoredValue::new((target_url(&device), snapshot.clone()));
    let path = RwSignal::new(String::new());
    let listing = RwSignal::new(None::<SnapshotListing>);
    let err = RwSignal::new(None::<String>);
    let copy = RwSignal::new(None::<String>);
    let load = move || {
        let (u, s) = base.get_value();
        let url = format!("{u}/browse?snapshot={}&path={}", api::encode(&s), api::encode(&path.get_untracked()));
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
            {l.truncated.then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("snap.truncated")} }.into_any()))}
            {ui::table("", &["snap.col.name", "#snap.col.size", "snap.col.modified", "snap.col.mode", ""], l.entries.into_iter().map(|e| entry_row(e, &dir, path, copy)).collect::<Vec<_>>())}
        }
        .into_any()
    };
    let shown = move || {
        let p = path.get();
        let s = base.with_value(|b| b.1.clone());
        if p.is_empty() { s } else { format!("{s}/{p}") }
    };
    let body = view! {
        <div class="toolbar browse-bar">
            <button class="icon-btn sm" title=tr("snap.up") aria-label=tr("snap.up") disabled=move || path.with(String::is_empty) on:click=up>
                <Icon icon=icons::ARROW_UP size=16 />
            </button>
            <span class="mono small bk-where">{shown}</span>
            <button class="btn ghost sm" disabled=move || path.with(String::is_empty) on:click=move |_| copy.set(Some(path.get_untracked()))>
                <Icon icon=icons::COPY size=14 />{tr("bk.copy_this")}
            </button>
        </div>
        <p class="small muted">{tr("bk.browse_note")}</p>
        {entries}
        {move || copy.get().map(|rel| copy_dialog(pcs, base.get_value(), rel, Callback::new(move |_| copy.set(None))))}
    }
    .into_any();
    let foot = view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any();
    let title = Signal::stored(tf("snap.browse_title", &[("id", &snapshot)]));
    view! { <div class="wide-drawer">{ui::drawer(title, close, body, foot)}</div> }.into_any()
}

fn entry_row(e: SnapshotEntry, dir: &str, path: RwSignal<String>, copy: RwSignal<Option<String>>) -> AnyView {
    let rel = join(dir, &e.name);
    let is_dir = e.kind == EntryKind::Dir;
    let icon = match e.kind {
        EntryKind::Dir => icons::FOLDER,
        EntryKind::Symlink => icons::LINK,
        _ => icons::FILE,
    };
    let rel_open = rel.clone();
    let name = e.name.clone();
    let copyable = matches!(e.kind, EntryKind::Dir | EntryKind::File | EntryKind::Symlink);
    let label = format!("{}: {}", t("bkpc.copy"), e.name);
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
            <button class="icon-btn sm" title=tr("bkpc.copy") aria-label=label disabled=!copyable on:click=move |_| copy.set(Some(rel.clone()))>
                <Icon icon=icons::COPY size=16 />
            </button>
        }
        .into_any()),
    ])
    .into_any()
}

fn copy_dialog(pcs: Pcs, base: (String, String), rel: String, close: Callback<()>) -> AnyView {
    let dest = RwSignal::new(String::new());
    let dest_err = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let rel2 = rel.clone();
    let go = move |_| {
        let body = CopyOutRequest { snapshot: base.1.clone(), path: rel2.clone(), destination: dest.get_untracked().trim().to_string() };
        let url = format!("{}/copy-out", base.0);
        let ok = tf("bkpc.copied", &[("path", &rel2)]);
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = match api::start_task(&url, serde_json::to_string(&body).ok()).await {
                Ok(id) => tasks::follow(&id).await,
                Err(e) => {
                    let _ = busy.try_set(false);
                    if e.field() == Some("destination") {
                        let _ = dest_err.try_set(Some(e.text()));
                    } else {
                        let _ = error.try_set(Some(e.text()));
                    }
                    return;
                }
            };
            let _ = busy.try_set(false);
            match r {
                Ok(task) if task.status == TaskStatus::Ok => {
                    toast(Tone::Ok, ok);
                    pcs.list.reload();
                    close.run(());
                }
                Ok(task) => {
                    let _ = error.try_set(Some(tasks::task_error(&task)));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let body = view! {
        <p class="small">{tf("bk.copy_text", &[("path", &rel)])}</p>
        {text_field("bk.f.destination", dest.into(), Callback::new(move |v| { dest_err.set(None); dest.set(v); }), dest_err.into(), In { mono: true, maxlength: 1024, help: Some("bk.f.destination_help"), placeholder: "/volume1/restore", ..In::default() })}
        {form_error(error)}
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() || dest.with(|d| d.trim().is_empty()) on:click=go>{spinner(busy)}<Icon icon=icons::COPY size=16 />{tr("bkpc.copy")}</button>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::stored(t("bkpc.copy").to_string()), close, body, Some(foot))
}
