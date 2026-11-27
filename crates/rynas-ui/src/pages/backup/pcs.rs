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
//! - The drawer, titled "<pc> — <description>": one status card (with rynas-agent its
//!   status, live progress, last good run, schedule and agent, Back up now / Cancel, or the
//!   code waiting for it; with btrbk the folder's status and last receive), then one note
//!   per problem; the snapshots with Measure space (the folder's space and the snapshot
//!   sizes) and Delete old now on their title line, folded by run (newest first) or by
//!   subvolume, labelled by the time in their names, with sizes, Browse and Delete; the
//!   agent's last runs; folded at the end "Restore on an empty disk" (its state and first
//!   reason on the folded line: the manifest's subvolumes, disks and boot copies, a restore
//!   code and the agent's commands), "Folder and rules" (path, space, retention, alert, last
//!   cleanup, key) and for btrbk "Set up the PC" (key, `btrbk.conf`, first connection, the
//!   manifest drop-in; open while they are the only way forward). Remove, Settings and
//!   Close in the footer.
//! - Settings: one dialog per PC: the folder's (also the switch between the senders) and,
//!   with rynas-agent, its schedule, the subvolumes it leaves out and Revoke.
//! - Browse: read-only folders inside a snapshot; Copy out puts a file or folder into a NAS
//!   folder (nothing is overwritten).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::pcbackup::*;
use rynas_api_types::snapshots::{Retention, RetentionPreset};
use rynas_api_types::TaskStatus;

use crate::api;
use crate::form::{In, SaveBar, area_field, form_error, number_field, save_bar_with, segmented, spinner, spinner_when, switch, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use rynas_api_types::pcagent::{PcDevice, PcMethod};
use crate::pages::snapshots::retention_summary;
use crate::tasks;
use crate::toast::toast;
use crate::ui::{self, Confirm, DlgKind, EmptyState, Folds, Remote, Tone, code_block, kv, kv_act, when};

const URL: &str = "/api/v1/pc-backups/targets";
/// The file manager's folder of the PCs' backups (`/@backups/<device>/<snapshot>`, read only,
/// administrators).
const BACKUPS_SHARE: &str = "@backups";

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

/// Who starts the measure that follows a new backup on the NAS (`rynasd::pc_backup`).
const AUTO_MEASURE_USER: &str = "rynas";

/// Task `id` has not finished yet as the task panel's store knows it (its events keep it
/// current; a task the store does not hold is taken as running: the server named it so).
fn task_running(id: &str) -> bool {
    tasks::tasks().list.with(|l| l.iter().find(|t| t.id == id).is_none_or(|t| !t.status.is_finished()))
}

/// A space measure of the PC runs now.
fn measure_running(m: &PcMeasure) -> bool {
    m.task.as_deref().is_some_and(task_running)
}

/// How far measure task `id` is: "34 % · about 20 min left" (the server weighs its progress
/// by the time each part should take; the store follows it by events), else "".
fn measure_progress(id: &str) -> String {
    let Some((pct, start)) = tasks::tasks().list.with(|l| l.iter().find(|t| t.id == id).and_then(|t| Some((t.progress?, t.start_ms)))) else {
        return String::new();
    };
    let pct = u64::from(pct.clamp(1, 99));
    let elapsed = (js_sys::Date::now() as u64).saturating_sub(start) / 1000;
    let left = elapsed * (100 - pct) / pct;
    tf("bkpc.measure_left", &[("pct", &pct.to_string()), ("time", &rough_time(left))])
}

/// An expected duration in whole minutes: "45 min", "2 h 10" (at least a minute).
fn rough_time(secs: u64) -> String {
    let min = secs.div_ceil(60).max(1);
    if min >= 60 { i18n::elapsed(min * 60) } else { i18n::approx(min * 60) }
}

/// The PC's own size from its manifest: "523 GiB used of 930 GiB".
fn pc_size_text(z: &PcSourceSize) -> String {
    tf("pcb.pc_size_text", &[("used", &i18n::bytes(z.used_bytes)), ("size", &i18n::bytes(z.size_bytes))])
}

/// "measured 2 d ago": when a space figure was read (shown while backups came or went since).
fn measured_ago(at_ms: u64) -> String {
    let now = js_sys::Date::now() as u64;
    tf("bk.usage_measured", &[("ago", &i18n::ago(now.saturating_sub(at_ms) / 1000))])
}

/// How long an agent's run took, in seconds (`None` when it does not say).
pub(super) fn run_secs(h: &rynas_api_types::pcagent::PcRun) -> Option<u64> {
    (h.ended_ms > h.started_ms).then(|| (h.ended_ms - h.started_ms) / 1000)
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

/// The folder's one status, the same words in the list and the drawer: a running backup is
/// "Backing up" with rynas-agent (the PC drives it) and "Receiving now" with btrbk (the NAS
/// only receives); a btrbk folder without a key waits for it whatever it holds.
fn folder_status(p: &PcTarget) -> AnyView {
    if p.receiving {
        return ui::status(Tone::Info, t(if p.agent { "pca.st.running" } else { "bkpc.receiving" }).to_string());
    }
    if !p.agent && p.public_key.is_empty() {
        return ui::status(Tone::Warn, t("pcb.st.no_key").to_string());
    }
    state_status(p.state)
}

/// Without a live agent nothing tells whether the PC is on: once nothing arrived for the
/// folder's alert time (three days when off) a neutral "Not seen for…" chip, never a late one.
fn quiet_chip(p: &PcTarget) -> Option<AnyView> {
    let last = p.last_received_ms?;
    let off = (js_sys::Date::now() as u64).saturating_sub(last);
    let hours = match p.alert_after_hours() {
        0 => super::agent::not_seen_after_h(0, 0),
        h => u64::from(h),
    };
    (!p.receiving && p.state == ReceiveState::Ok && off >= hours * 3_600_000).then(|| super::agent::not_seen_view(off))
}

/// The civil time in a btrbk snapshot name (`<prefix>.YYYYMMDD`, `…THHMM`,
/// `…THHMMSS±hhmm`, each with an optional `_N`): prefix, year, month, day, hour and minute
/// when the format has them, and the `_N` suffix. The PC's clock, shown as written (no time
/// zone conversion). `None` for any other name.
fn snapshot_stamp(name: &str) -> Option<Stamp<'_>> {
    let (prefix, ts) = split_long_snapshot(name)?;
    let (stamp, n) = match ts.split_once('_') {
        Some((s, n)) => (s, Some(n)),
        None => (ts, None),
    };
    let base = &stamp[..stamp.find(['+', '-']).unwrap_or(stamp.len())];
    let num = |r: std::ops::Range<usize>| base.get(r).and_then(|s| s.parse::<u32>().ok());
    let date = [num(0..4)?, num(4..6)?, num(6..8)?];
    let hm = if base.len() >= 13 { Some((num(9..11)?, num(11..13)?)) } else { None };
    Some(Stamp { prefix, date, hm, n })
}

/// The parts of a btrbk snapshot name ([`snapshot_stamp`]).
#[derive(Debug, PartialEq)]
struct Stamp<'a> {
    prefix: &'a str,
    /// Year, month, day.
    date: [u32; 3],
    /// Hour and minute (not in `short` names).
    hm: Option<(u32, u32)>,
    /// The `_N` suffix.
    n: Option<&'a str>,
}

/// A snapshot name's time in the current language ("13 Nov 2026, 15:43"; the date alone for
/// btrbk's `short` names), the `_N` suffix as "#N"; `None` when the name is not btrbk's.
pub(super) fn stamp_text(name: &str) -> Option<String> {
    let Stamp { date: [y, mo, d], hm, n, .. } = snapshot_stamp(name)?;
    let text = match hm {
        Some((h, mi)) => {
            let at = js_sys::Date::new_with_year_month_day_hr_min(y, mo as i32 - 1, d as i32, h as i32, mi as i32);
            i18n::datetime(at.get_time() as u64)
        }
        None => i18n::date(js_sys::Date::new_with_year_month_day(y, mo as i32 - 1, d as i32).get_time() as u64),
    };
    Some(match n {
        Some(n) => format!("{text} #{n}"),
        None => text,
    })
}

/// A received snapshot as people read it: "@home · 13 Nov 2026, 15:43" (the raw name when it
/// is not btrbk's).
pub(super) fn snapshot_label(name: &str) -> String {
    match (snapshot_stamp(name), stamp_text(name)) {
        (Some(Stamp { prefix, .. }), Some(when)) => format!("{prefix} · {when}"),
        _ => name.to_string(),
    }
}

/// What a snapshot row of the drawer names: its subvolume (inside a run), its time (inside
/// a subvolume) or both (the snapshots outside any run).
#[derive(Clone, Copy, PartialEq)]
enum Shown {
    Subvolume,
    Time,
    Full,
}

/// A snapshot's state when it is more than "received": interrupted, not a subvolume, kept by
/// the rules (the base of the next send says so on its disabled bin instead).
fn snapshot_flag(s: &ReceivedSnapshot) -> Option<AnyView> {
    let (tone, k) = if s.incomplete {
        (Tone::Warn, "bk.snap.incomplete")
    } else if s.not_subvolume {
        (Tone::Err, "bk.snap.not_subvolume")
    } else if s.protected && !s.parent {
        (Tone::Info, "bk.snap.protected")
    } else {
        return None;
    };
    Some(ui::status(tone, t(k).to_string()))
}

/// Why the NAS keeps a snapshot at its next pass, and about until when the counts keep it
/// ("Kept: one per day · goes after 26 Nov 2026"), or that the pass takes it. `None` for an
/// interrupted receive (its state says it).
fn kept_text(i: &RetentionItem) -> Option<String> {
    let k = match (i.keep, i.reason) {
        (_, RetentionReason::Interrupted) => return None,
        (false, _) => "pcb.goes_next",
        (true, RetentionReason::Parent) => "pcb.kept.parent",
        (true, RetentionReason::Floor) => "pcb.kept.floor",
        (true, RetentionReason::Recent) => "pcb.kept.recent",
        (true, RetentionReason::Hourly) => "pcb.kept.hourly",
        (true, RetentionReason::Daily) => "pcb.kept.daily",
        (true, RetentionReason::Weekly) => "pcb.kept.weekly",
        (true, RetentionReason::Monthly) => "pcb.kept.monthly",
        (true, RetentionReason::Yearly) => "pcb.kept.yearly",
        (true, RetentionReason::Future) => "pcb.kept.future",
        (true, RetentionReason::Pinned) => "pcb.kept.pinned",
        (true, RetentionReason::Expired) => return None,
    };
    let text = t(k).to_string();
    Some(match i.until_ms.filter(|_| i.keep) {
        Some(u) => format!("{text} · {}", tf("pcb.goes_after", &[("when", &i18n::date(u))])),
        None => text,
    })
}

fn fold(folds: Folds, title: &'static str, open: bool, summary: AnyView, body: AnyView) -> AnyView {
    ui::fold(Some(folds), title, open, Some(summary), body)
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
        PcIssueCode::SpaceTight => "bkpc.issue.space_tight",
        PcIssueCode::SharedFolder => "bkpc.issue.shared_folder",
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
        PcIssueCode::SpaceTight => "bkpc.short.space_tight",
        PcIssueCode::SharedFolder => "bkpc.short.shared_folder",
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
        BareMetalCode::BootCopyStale => "bkpc.bm.boot_copy_stale",
        BareMetalCode::BootCopyMissing => "bkpc.bm.boot_copy_missing",
    };
    tf(k, &[("subject", g.subject.as_deref().unwrap_or(""))])
}

/// Ready, ready with notes (a stale manifest or boot copy, a newest run that lacks a
/// subvolume: said in the panel), or not ready.
fn bm_status(b: &BareMetal) -> AnyView {
    match (b.ready, b.warnings.is_empty()) {
        (true, true) => ui::status(Tone::Ok, t("bkpc.bm.ready").to_string()),
        (true, false) => ui::status(Tone::Warn, t("bkpc.bm.ready_notes").to_string()),
        (false, _) => ui::status(Tone::Info, t("bkpc.bm.not_ready").to_string()),
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
    // mounted subvolumes and the ones nested in them are sent: any other unmounted one is
    // never expected
    let expected: Vec<&rynas_api_types::pcagent::BtrbkSubvolume> = b.layout.iter().filter(|s| !s.mount.is_empty() || !s.nested_in.is_empty()).collect();
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
    // everything there: nothing to say (the status and the snapshots say it)
    if expected.iter().all(|s| got(&s.name)) {
        return None;
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
        .unwrap_or_else(|| ui::DASH.into())
}

/// Who deletes old snapshots, in a sentence.
fn rules_text(p: &PcTarget) -> String {
    if p.retention.owner == RetentionOwner::Nas {
        return tf("bkpc.ret.nas", &[("rule", &retention_summary(&p.retention.gfs))]);
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
            Ok(task) if task.is_full_success() => {
                toast(Tone::Ok, ok);
                pcs.list.reload();
                // the page may have been left while the task ran
                done.try_run(());
            }
            // as run_and_report: the outcome in a toast (also when the dialog was closed
            // meanwhile), the dialog closed
            Ok(task) => {
                toast(tasks::task_tone(&task), tasks::task_error(&task));
                pcs.list.reload();
                done.try_run(());
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
        <button class="btn primary" on:click=move |_| add.set(Some(false))><Icon icon=icons::PLUS size=16 /><span class="btn-label keep">{tr("bk.add")}</span></button>
        <button class="btn ghost" on:click=move |_| add.set(Some(true))><Icon icon=icons::FOLDER_OPEN size=16 /><span class="btn-label keep">{tr("bk.adopt")}</span></button>
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
    // the agents' live progress: both lists read again every 5 s while one PC backs up and
    // the page is visible
    let agents = Remote::<rynas_api_types::pcagent::PcBackupOverview>::new("/api/v1/pcbackup");
    crate::live::poll_while(
        5000,
        move || {
            pcs.list.data.with(|l| l.as_ref().is_some_and(|l| l.iter().any(|p| p.receiving)))
                || agents.data.with(|o| o.as_ref().is_some_and(|o| o.devices.iter().any(|d| d.state == rynas_api_types::pcagent::PcState::Running)))
        },
        move || {
            pcs.list.reload();
            agents.reload();
        },
    );
    // the space measures the list names as running: when the task's end reaches the task
    // store (pushed by the event stream, nothing polled), the list and an open drawer read
    // the PC again and a toast says how it went (a failure always; a success only of a
    // measure someone started, not of the one that follows a backup)
    let told = StoredValue::new(std::collections::BTreeSet::<String>::new());
    // while this page is open it reports the measures itself: no global toast for them
    tasks::reports("pcbackup.usage");
    Effect::new(move |_| {
        let running: Vec<String> = pcs.list.data.with(|l| l.iter().flatten().filter_map(|p| p.measure.task.clone()).collect());
        if running.is_empty() {
            return;
        }
        let ended: Vec<rynas_api_types::TaskInfo> = tasks::tasks().list.with(|l| l.iter().filter(|x| x.status.is_finished() && running.contains(&x.id)).cloned().collect());
        for task in ended {
            let mut new = false;
            told.update_value(|s| new = s.insert(task.id.clone()));
            if !new {
                continue;
            }
            match task.status {
                TaskStatus::Ok if task.user == AUTO_MEASURE_USER => {}
                TaskStatus::Ok => toast(Tone::Ok, t("bk.measured").to_string()),
                TaskStatus::Cancelled => toast(Tone::Warn, tasks::task_error(&task)),
                _ => toast(Tone::Err, tasks::task_error(&task)),
            }
            pcs.list.reload();
            pcs.saved.update(|n| *n += 1);
        }
    });
    let content = move || {
        let Some(list) = pcs.list.data.get() else { return ().into_any() };
        // the paired PC of a folder fed by rynas-agent (its status and progress)
        let device_of = |dev: &str| agents.data.with(|o| o.as_ref().and_then(|o| o.devices.iter().find(|d| d.device == dev && d.settings.method == PcMethod::Btrfs).cloned()));
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
                // the space the backups take on disk (shared data once), as the drawer says it
                let space = p.usage.map(|u| i18n::bytes(u.on_disk_bytes())).unwrap_or_else(|| ui::DASH.into());
                let label = ui::verb_name("bk.open", &p.device);
                // the first problem other than "late" (the state already says that)
                let problem = p.issues.iter().find(|i| i.code != PcIssueCode::Late).map(|i| {
                    let tone = if i.code == PcIssueCode::LowSpace { Tone::Err } else { Tone::Warn };
                    ui::status(tone, t(issue_short(i)).to_string())
                });
                let paired = p.agent.then(|| device_of(&p.device)).flatten();
                // what the last backup sent (rynas-agent tells), under the space on disk
                let last_sent = paired.as_ref().and_then(|d| d.last_run.as_ref()).filter(|r| r.ok && r.bytes_added > 0).map(|r| tf("pcb.last_sent", &[("size", &i18n::bytes(r.bytes_added))]));
                // an older figure says when it was measured (snapshots came or went since)
                let stale = p.usage.filter(|_| p.usage_stale).map(|u| measured_ago(u.read_at_ms));
                // the PC's own size, as its manifest says (free: nothing measured)
                let pc_used = p.pc_size.map(|z| tf("pcb.pc_used_short", &[("size", &i18n::bytes(z.used_bytes))]));
                let space_sub = Some([stale, pc_used, last_sent].into_iter().flatten().collect::<Vec<_>>().join(" · ")).filter(|s| !s.is_empty());
                let mut cells = vec![
                    ui::cell("bk.col.pc", "wrap", ui::two_lines(view! { <strong>{p.device.clone()}</strong> }.into_any(), sub, "cell-sub small")),
                    ui::cell("col.status", "wrap", {
                        // with rynas-agent the PC's chip, the drawer's (agent::device_chip); a
                        // backup running: its progress instead of the last date
                        let progress = paired.as_ref().and_then(|d| d.progress.clone());
                        let (chip, not_seen, off) = match &paired {
                            _ if progress.is_some() => (ui::status(Tone::Info, t("pca.st.running").to_string()), None, None),
                            Some(d) => (super::agent::device_chip(d), super::agent::not_seen_chip(d), super::agent::off_note(d)),
                            None => (folder_status(p), quiet_chip(p), None),
                        };
                        // the chips on one line, then the progress, how long the PC has been
                        // off, or when the last backup arrived
                        view! {
                            <div class="cell-main">
                                <div class="chips">{chip}{not_seen}{problem}</div>
                                {match progress {
                                    Some(pr) => super::agent::progress_view(&pr),
                                    None => view! { <span class="cell-sub small">{off.unwrap_or_else(|| when(p.last_received_ms))}</span> }.into_any(),
                                }}
                            </div>
                        }
                        .into_any()
                    }),
                    // the backups (runs), as the drawer counts them, and the subvolumes they hold
                    ui::cell("pcb.backups", "wrap", ui::two_lines(view! { <span class="tnum">{i18n::count(u64::from(p.runs))}</span> }.into_any(), (!groups.is_empty()).then_some(groups), "cell-sub small mono")),
                    // on disk (measured), below it a measure running now, how the last one
                    // failed, or what the last backup sent (the line follows the task live)
                    ui::cell("pcb.col.on_disk", "num wrap", {
                        let task = p.measure.task.clone();
                        let failed = p.measure.error.clone();
                        let sub = move || {
                            if let Some(id) = task.as_deref().filter(|id| task_running(id)) {
                                let left = measure_progress(id);
                                Some(view! { <span class="cell-sub small">{tr("bkpc.measuring")}" "{left}</span> }.into_any())
                            } else if let Some(e) = failed.clone() {
                                Some(view! { <span class="cell-sub small bk-err" title=e>{tr("bkpc.measure_failed_short")}</span> }.into_any())
                            } else {
                                space_sub.clone().map(|s| view! { <span class="cell-sub small">{s}</span> }.into_any())
                            }
                        };
                        view! { <div class="cell-main"><span class="tnum">{space}</span>{sub}</div> }.into_any()
                    }),
                ];
                cells.push(ui::cell("bkpc.col.restore", "", bm_status(&p.bare_metal)));
                cells.push(ui::actions(view! { <button class="btn ghost sm" aria-label=label on:click=move |_| pcs.open.set(Some(dev.clone()))><Icon icon=icons::EYE size=14 />{tr("bk.open")}</button> }.into_any()));
                ui::row(cells)
            })
            .collect::<Vec<_>>();
        // the folder is in the panel; the same names as the file backups' tab
        const HEADS: &[&str] = &["bk.col.pc", "col.status", "pcb.backups", "#pcb.col.on_disk", "bkpc.col.restore", ""];
        ui::table("bk-table", HEADS, rows)
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
            // rynas-agent: a warning only when the PC was on while a backup was due; btrbk: a
            // notice, the NAS cannot tell a PC that is off from one whose backups stopped
            <Show
                when=move || agent.get()
                fallback=move || number_field("bkpc.f.alert_hours", Some("bkpc.f.alert_hours_help"), self.alert.into(), Callback::new(move |v| self.alert.set(v)), 0, 8760)
            >
                {number_field("bkpc.f.alert_hours_agent", Some("bkpc.f.alert_hours_agent_help"), self.alert.into(), Callback::new(move |v| self.alert.set(v)), 0, 8760)}
            </Show>
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
                    view! { <p class="field-help">{retention_summary(&r)}</p> }.into_any()
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
            // the folder was changed meanwhile: this is not its preview
            if path.try_with_untracked(|p| p.trim() == body.path) != Some(true) {
                return;
            }
            match r {
                Ok(p) => {
                    if device.try_with_untracked(String::is_empty) == Some(true)
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
        // an already paired PC moves to this folder in the same task, once the folder exists
        let link_device = is_agent && !linked.get_untracked().is_empty();
        let body = PcTargetCreate {
            link_device,
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
                // the folder exists but the paired PC was not moved to it (partial)
                Ok(task) if task.status == TaskStatus::Ok && task.partial.is_some() => {
                    let _ = busy.try_set(false);
                    let why = task.partial.clone().unwrap_or_default();
                    let _ = error.try_set(Some(tf("pcb.link_failed", &[("error", &why)])));
                    pcs.list.reload();
                }
                Ok(task) if task.status == TaskStatus::Ok => {
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
    let foot = save_bar_with(SaveBar {
        can_save: Signal::derive(ready),
        label: Signal::derive(move || if mode.get() == "adopt" { t("bk.adopt") } else { t("bkpc.create") }.to_string()),
        icon: Some(icons::PLUS),
        ..SaveBar::new(busy, "bkpc.create", close, Callback::new(save))
    });
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
    let sched = crate::schedule::interval::IntervalEditor::new(
        &plan,
        live.as_ref().and_then(|d| d.last_ok_ms),
        live.as_ref().and_then(|d| d.interval_from_ms),
        live.as_ref().map(|d| d.agent_version.as_str()).unwrap_or_default(),
    );
    let wake = has_agent.then(|| crate::pages::wake::pc::WakeSettings::new(&plan, &p.device));
    // a subvolume left out = its mount point in the agent's excludes and no alert for it on
    // the NAS; excludes that name no known subvolume stay as written
    // (a nested subvolume without a mount: its place inside its parent, `/home/games`)
    let whole = rynas_api_types::pcagent::BtrbkLayout { subvolumes: p.bare_metal.layout.clone(), ..Default::default() };
    let mounts: Vec<(String, String)> = p.bare_metal.layout.iter().filter_map(|s| whole.place_of(s).map(|at| (s.name.clone(), at.trim_end_matches('/').to_string()))).collect();
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
        if is_agent
            && has_agent
            && let Some(k) = sched.problem()
        {
            error.set(Some(t(k).to_string()));
            return;
        }
        let agent_settings = agent_body.clone().filter(|_| is_agent).map(|mut s| {
            s.description = description;
            sched.apply(&mut s.plan);
            if let Some(w) = wake {
                w.apply(&mut s.plan);
            }
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
    let schedule = has_agent.then(|| {
        view! {
            <Show when=move || agent.get()>
                <h3 class="mini-title">{tr("pca.f.schedule")}</h3>
                {sched.view()}
                {wake.map(|w| w.view())}
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
    let foot = save_bar_with(SaveBar::new(busy, "save", close, Callback::new(save)));
    ui::dialog(DlgKind::Normal, Signal::stored(tf("bk.settings_title", &[("device", &p.device)])), close, body, Some(foot))
}

// ------------------------------------------------------------------ drawer

/// The restore part of "Restore on an empty disk" for a PC of this folder (sent by
/// rynas-agent or by btrbk): a one-time restore code (one hour, read-only, this PC only) and
/// the commands to run on the PC from any live Linux; the agent receives the snapshots back
/// into a new btrfs filesystem (`docs/research/pc-backup.md` §11.2 point 6, §11.10).
/// `ready`: the snapshots are enough for a full rebuild (otherwise a warning above the button).
/// `code`: the code just made, with the NAS certificate's fingerprint (kept by the drawer,
/// so reading the PC again does not hide it).
fn restore_part(device: String, ready: bool, code: RwSignal<Option<(rynas_api_types::pcagent::RestoreCodeCreated, Option<String>)>>) -> AnyView {
    use rynas_api_types::pcagent::{PcBackupOverview, RestoreCodeCreated};
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let dev = StoredValue::new(device);
    let make = move |_| {
        let u = format!("/api/v1/pcbackup/devices/{}/restore-code", api::encode(&dev.get_value()));
        error.set(None);
        busy.set(true);
        spawn_local(async move {
            let r = api::post::<(), RestoreCodeCreated>(&u, None).await;
            // the NAS certificate's fingerprint, for the command and to compare by eye
            let fingerprint = match r {
                Ok(_) => api::get::<PcBackupOverview>("/api/v1/pcbackup").await.ok().and_then(|o| super::agent::pinned_fingerprint(&o)),
                Err(_) => None,
            };
            let _ = busy.try_set(false);
            match r {
                Ok(c) => code.set(Some((c, fingerprint))),
                Err(e) => error.set(Some(e.text())),
            }
        });
    };
    view! {
        <p class="small muted">{tr("bkpc.restore.intro")}</p>
        {(!ready).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("pca.restore_not_ready").to_string())))}
        {form_error(error)}
        <div class="sec-actions">
            <button class="btn ghost sm" disabled=move || busy.get() on:click=make>{spinner(busy)}<Icon icon=icons::LIFE_BUOY size=14 />{tr("bkpc.restore.make")}</button>
        </div>
        {move || code.get().map(|(c, fp)| super::agent::restore_code_view(&c, fp))}
    }
    .into_any()
}

fn drawer(pcs: Pcs, device: String) -> AnyView {
    use rynas_api_types::pcagent::PcRun as AgentRun;
    let close = Callback::new(move |_| pcs.open.set(None));
    let dev = StoredValue::new(device.clone());
    let host = window().location().hostname().unwrap_or_default();
    let detail = Remote::<PcTargetDetail>::new(format!("{}?host={}", target_url(&device), api::encode(&host)));
    pcs.ask_revoke.set(false);
    let ctl = super::agent::AgentCtl::new(device.clone(), pcs.ask_revoke, Callback::new(move |_| detail.reload()));
    let ask = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // one Measure: a task on the NAS for the folder and every snapshot; it goes on when the
    // drawer closes, the list shows it running and says how it ended (Pcs' watcher)
    let starting = RwSignal::new(false);
    let started = RwSignal::new(None::<String>);
    let measure_err = RwSignal::new(None::<String>);
    let measuring = Signal::derive(move || {
        starting.get()
            || started.with(|id| id.as_deref().is_some_and(task_running))
            || detail.data.with(|d| d.as_ref().is_some_and(|d| measure_running(&d.target.measure)))
    });
    // a measure that searches whole snapshots (the first one) says first about how long it
    // takes and goes only once confirmed; one that compares what changed starts at once
    let ask_measure = RwSignal::new(None::<PcMeasurePlan>);
    let start_measure = Callback::new(move |_: ()| {
        ask_measure.set(None);
        let base = target_url(&dev.get_value());
        starting.set(true);
        measure_err.set(None);
        spawn_local(async move {
            let r = api::start_task(&format!("{base}/usage"), None).await;
            let _ = starting.try_set(false);
            match r {
                Ok(id) => {
                    let _ = started.try_set(Some(id));
                    // the list names the running task: its watcher follows it from there
                    pcs.list.reload();
                }
                Err(e) => {
                    let _ = measure_err.try_set(Some(e.text()));
                }
            }
        });
    });
    let measure = move |_| match detail.data.with_untracked(|d| d.as_ref().and_then(|d| d.measure_plan)) {
        Some(p) if p.walks > 0 => ask_measure.set(Some(p)),
        _ => start_measure.run(()),
    };
    let remove = Callback::new(move |_| {
        run(pcs, ("DELETE", target_url(&dev.get_value()), None), tf("bk.removed", &[("device", &dev.get_value())]), (busy, error, None), Callback::new(move |_| {
            ask.set(false);
            pcs.open.set(None);
        }));
    });
    let pruning = RwSignal::new(false);
    let prune_err = RwSignal::new(None::<String>);
    // Delete old now: first what the saved rules delete and keep (the plan endpoint), then
    // the pass once confirmed
    let prune_ask = RwSignal::new(None::<PcRetentionPreview>);
    let prune = move |_| {
        let Some(tg) = detail.data.with_untracked(|d| d.as_ref().map(|d| d.target.clone())) else { return };
        pruning.set(true);
        prune_err.set(None);
        let body = PcRetentionPlanRequest { device: tg.device.clone(), retention: tg.retention.clone(), delete: tg.delete };
        spawn_local(async move {
            let r = api::post::<_, PcRetentionPreview>("/api/v1/pc-backups/plan/retention", Some(&body)).await;
            let _ = pruning.try_set(false);
            match r {
                Ok(p) if p.delete == 0 => toast(Tone::Info, t("bkpc.prune_nothing").to_string()),
                Ok(p) => {
                    let _ = prune_ask.try_set(Some(p));
                }
                Err(e) => {
                    let _ = prune_err.try_set(Some(e.text()));
                }
            }
        });
    };
    let do_prune = Callback::new(move |_| {
        run(pcs, ("POST", format!("{}/prune", target_url(&dev.get_value())), None), t("bkpc.pruned").to_string(), (pruning, prune_err, None), Callback::new(move |_| {
            prune_ask.set(None);
            detail.reload();
        }));
    });
    // the backups listed by run (newest first) or by subvolume (Folder and rules)
    let by_run = RwSignal::new("run".to_string());
    // kept while the PC is read again (a Measure, a poll): what is open, a restore code
    let folds = Folds::new();
    let restore_code = RwSignal::new(None);
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
    // the agent's runs (status, bytes sent) next to the NAS's: a memo, so the progress polls
    // do not rebuild the drawer
    let history = Memo::new(move |_| ctl.detail.data.with(|d| d.as_ref().map(|d| d.history.iter().filter(|h| !h.restore).cloned().collect::<Vec<AgentRun>>()).unwrap_or_default()));
    let settings = move || {
        if let Some(target) = detail.data.with_untracked(|d| d.as_ref().map(|d| d.target.clone())) {
            pcs.edit.set(Some((target, ctl.detail.data.with_untracked(|a| a.as_ref().map(|a| a.device.clone())))));
        }
    };
    let content = move || {
        let Some(d) = detail.data.get() else { return ().into_any() };
        let p = d.target.clone();
        let nas = p.retention.owner == RetentionOwner::Nas;
        let agent = p.agent;
        let device = dev.get_value();

        // ---- the description under the title, unless it only repeats the PC's name
        let desc = (!p.description.trim().is_empty() && p.description.trim().to_lowercase() != device.trim().to_lowercase()).then(|| view! { <p class="small muted pcb-desc">{p.description.clone()}</p> });

        // ---- the status card: with rynas-agent the agent's (status, progress, schedule,
        // Run now / Cancel); with btrbk the folder's status and last receive
        let status = if agent {
            let (state, last) = (p.clone(), p.last_received_ms);
            let folder = ViewFn::from(move || {
                view! {
                    {folder_status(&state)}
                    <span class="small muted">{last.map(|ms| tf("pcb.last_received_on", &[("when", &i18n::datetime(ms))]))}</span>
                }
                .into_any()
            });
            ctl.status(folder, Callback::new(move |_| settings()))
        } else {
            let last = match p.last_received_ms {
                Some(ms) => tf("pcb.last_received_on", &[("when", &i18n::datetime(ms))]),
                None => t("pcb.never").to_string(),
            };
            view! {
                <section class="status-card">
                    <div class="status-card-head">{folder_status(&p)}{quiet_chip(&p)}</div>
                    <p class="status-meta small"><span>{last}</span><span>{t("pcb.via.ssh")}</span></p>
                </section>
            }
            .into_any()
        };

        // ---- problems: one note each
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
        // the first-backup check while subvolumes are still awaited
        let check = first_run(&b, &d.list);

        // ---- snapshots
        let snaps: std::collections::HashMap<&str, &ReceivedSnapshot> = d.list.iter().map(|s| (s.name.as_str(), s)).collect();
        // with the NAS owning retention: the plan of its next pass, per snapshot
        let plan: std::collections::HashMap<&str, &RetentionItem> = d.plan.iter().map(|i| (i.name.as_str(), i)).collect();
        let browsable = |s: &ReceivedSnapshot| s.readonly && !s.not_subvolume;
        // a snapshot in the file manager (read only, copied out from there)
        let files_href = |name: &str| format!("/files/{}/{}/{}", api::encode(BACKUPS_SHARE), api::encode(&device), api::encode(name));
        // one row: the label (the subvolume inside a run, the time inside a subvolume, both
        // among the others), its state when it says more than "received", the size, Browse
        // and Delete
        let snap_row = |s: &ReceivedSnapshot, shown: Shown| {
            let name_del = s.name.clone();
            let text = match shown {
                Shown::Subvolume => s.group.clone(),
                Shown::Time => stamp_text(&s.name).unwrap_or_else(|| s.name.clone()),
                Shown::Full => snapshot_label(&s.name),
            };
            let label = format!("{}: {}", t("bk.browse"), snapshot_label(&s.name));
            let label_del = format!("{}: {}", t("bkpc.delete_snap"), snapshot_label(&s.name));
            let title = format!("{} · {} {}", s.name, t("bk.col.received"), when(s.received_at_ms));
            let deletable = !s.parent && !s.not_subvolume && !(s.incomplete && p.receiving);
            // why the bin is off: the next incremental backup starts from this one
            let del_tip = if s.parent { t("bkpc.snap.parent_tip").to_string() } else { t("bkpc.delete_snap").to_string() };
            let wrap_tip = del_tip.clone();
            let browse = if browsable(s) {
                view! { <a class="icon-btn sm" href=files_href(&s.name) title=tr("bk.browse") aria-label=label><Icon icon=icons::FOLDER_OPEN size=16 /></a> }.into_any()
            } else {
                view! { <button class="icon-btn sm" title=tr("bk.browse") aria-label=label disabled=true><Icon icon=icons::FOLDER_OPEN size=16 /></button> }.into_any()
            };
            let head = if shown == Shown::Subvolume { "bk.col.subvolume" } else { "bk.col.snapshot" };
            // why the NAS keeps it (its rules: the counts, the base of the next backup, the
            // floor) and about until when, or that the next cleanup takes it
            let kept = plan.get(s.name.as_str()).and_then(|i| kept_text(i));
            let flag = if kept.is_some() && s.protected && !s.incomplete && !s.not_subvolume { None } else { snapshot_flag(s) };
            ui::row(vec![
                ui::cell(head, "wrap", view! {
                    <div class="cell-main">
                        <div class="chips"><span class=if shown == Shown::Time { "small bk-wrap" } else { "mono small bk-wrap" } title=title>{text}</span>{flag}</div>
                        {kept.map(|k| view! { <span class="cell-sub small">{k}</span> })}
                    </div>
                }.into_any()),
                // the measured size, and below it what the agent sent for it (free)
                match s.sent_bytes {
                    Some(b) => ui::cell("snap.col.size", "tnum wrap", ui::two_lines(ui::text(size_text(s.size)), Some(tf("pcb.sent_size", &[("size", &i18n::bytes(b))])), "cell-sub small")),
                    None => ui::text_cell("snap.col.size", "tnum", size_text(s.size)),
                },
                ui::actions(view! {
                    {browse}
                    <span class="tip-wrap" title=wrap_tip>
                        <button class="icon-btn sm danger" title=del_tip aria-label=label_del disabled=!deletable on:click=move |_| { del_err.set(None); del.set(Some(name_del.clone())); }><Icon icon=icons::TRASH_2 size=16 /></button>
                    </span>
                }.into_any()),
            ])
        };
        // a folded row, open as the user left it (key: the run's timestamp, the subvolume, the
        // whole PC): its title and facts left, its state and the chevron right, so every row of
        // the drawer (restore, backups) starts at the same edge as the section headings
        let group = |key: String, title: AnyView, sub: String, flag: Option<AnyView>, body: AnyView| {
            let k = StoredValue::new(key.clone());
            view! {
                <details class="fold pcb-group" open=folds.open(key, false) on:toggle=move |ev| k.with_value(|k| folds.toggled(k, &ev))>
                    <summary>
                        <span class="pcb-row-main">{title}<span class="small muted tnum">{sub}</span></span>
                        <span class="pcb-row-end">{flag}<Icon icon=icons::CHEVRON_RIGHT size=14 /></span>
                    </summary>
                    <div class="pcb-group-body">{body}</div>
                </details>
            }
        };
        const RUN_HEADS: &[&str] = &["bk.col.subvolume", "snap.col.size", ""];
        const HEADS: &[&str] = &["bk.col.snapshot", "snap.col.size", ""];
        // the worst state of a group's snapshots, on its folded line
        let worst = |items: &[&ReceivedSnapshot]| {
            if items.iter().any(|s| s.not_subvolume) {
                Some(ui::status(Tone::Err, t("bk.snap.not_subvolume").to_string()))
            } else if items.iter().any(|s| s.incomplete) {
                Some(ui::status(Tone::Warn, t("bk.snap.incomplete").to_string()))
            } else {
                None
            }
        };
        let mut runs: Vec<&PcRun> = d.runs.iter().collect();
        runs.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.timestamp.cmp(&a.timestamp)));
        // what no run holds (not a subvolume, a name btrbk does not give): its own group at
        // the end of the runs
        let in_runs: std::collections::HashSet<&str> = runs.iter().flat_map(|r| r.snapshots.iter().map(String::as_str)).collect();
        let mut others: Vec<&ReceivedSnapshot> = d.list.iter().filter(|s| !in_runs.contains(s.name.as_str())).collect();
        others.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| a.name.cmp(&b.name)));
        let other_group = (!others.is_empty()).then(|| {
            let n = others.len();
            let sub = tf(if n == 1 { "pcb.n_snaps1" } else { "pcb.n_snaps" }, &[("n", &n.to_string())]);
            let flag = worst(&others);
            let rows = others.iter().map(|s| snap_row(s, Shown::Full)).collect();
            group("other".into(), view! { <span class="pcb-group-title">{tr("pcb.other_snaps")}</span> }.into_any(), sub, flag, ui::table("bk-snaps", HEADS, rows))
        });

        // ---- one list of backups: each run of the NAS with the agent's word on it (done,
        // failed, bytes sent), and the agent's failed runs that left nothing here
        let hist = history.get();
        let mut used = vec![false; hist.len()];
        let matched: Vec<Option<&AgentRun>> = runs
            .iter()
            .map(|r| {
                let i = hist.iter().enumerate().position(|(i, h)| !used[i] && h.snapshot.as_deref() == Some(r.timestamp.as_str())).or_else(|| {
                    // a run that names no snapshot (a failed one; btrfs sends report none):
                    // the one whose time falls in it (snapshot names have whole minutes)
                    let at = r.created_ms?;
                    hist.iter().enumerate().position(|(i, h)| !used[i] && at + 120_000 >= h.started_ms && at <= h.ended_ms.max(h.started_ms) + 60_000)
                })?;
                used[i] = true;
                Some(&hist[i])
            })
            .collect();
        enum Item<'a> {
            Run(&'a PcRun, Option<&'a AgentRun>),
            Failed(&'a AgentRun),
        }
        let mut items: Vec<(u64, Item)> = runs.iter().zip(matched).map(|(r, h)| (r.created_ms.or(h.map(|h| h.started_ms)).unwrap_or(0), Item::Run(r, h))).collect();
        items.extend(hist.iter().enumerate().filter(|(i, h)| !used[*i] && !h.ok).map(|(_, h)| (h.started_ms, Item::Failed(h))));
        items.sort_by_key(|i| std::cmp::Reverse(i.0));
        let n_backups = items.len();
        let by_runs = items
            .into_iter()
            .map(|(_, item)| match item {
                Item::Run(r, h) => {
                    let parts: Vec<&ReceivedSnapshot> = r.snapshots.iter().filter_map(|n| snaps.get(n.as_str()).copied()).collect();
                    let when_text = stamp_text(&format!("x.{}", r.timestamp)).unwrap_or_else(|| when(r.created_ms));
                    let mut sub = vec![tf(if parts.len() == 1 { "pcb.n_subvols1" } else { "pcb.n_subvols" }, &[("n", &parts.len().to_string())])];
                    if let Some(h) = h.filter(|h| h.bytes_added > 0) {
                        sub.push(tf("pcb.sent_size", &[("size", &i18n::bytes(h.bytes_added))]));
                    }
                    // how long the agent's run took
                    if let Some(d) = h.and_then(run_secs) {
                        sub.push(tf("pcb.took", &[("time", &i18n::elapsed(d))]));
                    }
                    // a run that brought some subvolumes and not others is partial, also when
                    // the agent's run failed on the missing one
                    let failed = h.is_some_and(|h| !h.ok);
                    let partial = !r.in_progress && !r.lacks.is_empty() && !r.incomplete && !parts.is_empty();
                    if partial {
                        sub.push(tf("bkpc.run.lacks", &[("names", &r.lacks.join(", "))]));
                    }
                    if let Some(x) = r.exclusive_bytes {
                        sub.push(i18n::bytes(x));
                    }
                    // the run the agent is still sending: what it lacks is on its way
                    let flag = if r.in_progress {
                        ui::status(Tone::Info, t("bkpc.run.running").to_string())
                    } else if partial {
                        // the pill stays short: the missing names are on the line below, the
                        // agent's error (when its run failed on one) in the body
                        ui::status(if failed { Tone::Warn } else { Tone::Info }, t("bkpc.run.partial").to_string())
                    } else if failed {
                        ui::status(Tone::Err, t("pca.st.failed").to_string())
                    } else if r.incomplete {
                        ui::status(Tone::Warn, t("bk.snap.incomplete").to_string())
                    } else {
                        ui::status(Tone::Ok, t("pca.h.ok").to_string())
                    };
                    let error = h.and_then(|h| h.error.clone()).map(|e| ui::note(Tone::Err, icons::TRIANGLE_ALERT, ui::breakable(&e, "small")));
                    let rows = parts.into_iter().map(|s| snap_row(s, Shown::Subvolume)).collect();
                    let body = view! { {error}{ui::table("bk-snaps", RUN_HEADS, rows)} }.into_any();
                    group(format!("run:{}", r.timestamp), view! { <span class="pcb-group-title tnum">{when_text}</span> }.into_any(), sub.join(" · "), Some(flag), body)
                }
                Item::Failed(h) => {
                    let body = ui::note(Tone::Err, icons::TRIANGLE_ALERT, ui::breakable(h.error.as_deref().unwrap_or_default(), "small"));
                    let sent = if h.bytes_added > 0 { tf("pcb.sent_size", &[("size", &i18n::bytes(h.bytes_added))]) } else { String::new() };
                    group(format!("failed:{}", h.run_id), view! { <span class="pcb-group-title tnum">{i18n::datetime(h.started_ms)}</span> }.into_any(), sent, Some(ui::status(Tone::Err, t("pca.st.failed").to_string())), body)
                }
            })
            .collect_view();
        let mut names: Vec<&str> = d.list.iter().map(|s| s.group.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        let by_groups = names
            .into_iter()
            .map(|g| {
                let mut items: Vec<&ReceivedSnapshot> = d.list.iter().filter(|s| s.group == g).collect();
                items.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then_with(|| b.name.cmp(&a.name)));
                let n = items.len();
                let mut sub = tf(if n == 1 { "pcb.n_snaps1" } else { "pcb.n_snaps" }, &[("n", &n.to_string())]);
                if let Some(t) = items.first().and_then(|s| stamp_text(&s.name)) {
                    sub = format!("{sub} · {t}");
                }
                let flag = worst(&items);
                let rows = items.into_iter().map(|s| snap_row(s, Shown::Time)).collect();
                group(format!("sv:{g}"), view! { <span class="pcb-group-title mono">{g.to_string()}</span> }.into_any(), sub, flag, ui::table("bk-snaps", HEADS, rows))
            })
            .collect_view();
        let count = {
            let n = u64::from(p.snapshots);
            // the backups held (the list's number too); failed attempts are rows, not backups
            let mut s = format!("{} · {}", i18n::tn("pcb.n_backups", u64::from(p.runs), &[]), tf(if n == 1 { "pcb.n_snaps1" } else { "pcb.n_snaps" }, &[("n", &i18n::count(n))]));
            if let Some(u) = p.usage {
                s = format!("{s} · {}", tf("bk.on_disk", &[("size", &i18n::bytes(u.on_disk_bytes()))]));
            }
            s
        };
        let has_snaps = !d.list.is_empty();
        let snapshots = view! {
            <div class="sec-head">
                <h3 class="mini-title">{tr("pcb.backups")}</h3>
                <span class="small muted tnum">{count}</span>
                // grouped by run or by subvolume: on the line of the list it changes
                {has_snaps.then(|| view! { <div class="sec-head-actions">{segmented("bkpc.view", VIEWS, by_run.into(), Callback::new(move |v| by_run.set(v)))}</div> })}
            </div>
            {if has_snaps || n_backups > 0 {
                view! {
                    <div class="pcb-groups" hidden=move || by_run.get() != "run">{by_runs}{other_group}</div>
                    <div class="pcb-groups" hidden=move || by_run.get() != "subvolume">{by_groups}</div>
                }
                .into_any()
            } else {
                ui::empty_line("bk.no_snapshots")
            }}
        };

        // ---- restore: a file (the newest backup in the file manager), or the whole PC on an
        // empty disk (the state and its first reason on the folded line; inside, the code and
        // its command, then the details)
        let mount_of = |g: &str| b.layout.iter().find(|l| l.name == g).map(|l| l.mount.clone());
        // the newest run's home, else its root, else any of its snapshots
        let latest = runs.iter().find_map(|r| {
            let ok: Vec<&ReceivedSnapshot> = r.snapshots.iter().filter_map(|n| snaps.get(n.as_str()).copied()).filter(|s| browsable(s) && !s.incomplete).collect();
            let at = |m: &str| ok.iter().find(|s| mount_of(&s.group).as_deref() == Some(m)).copied();
            at("/home").or_else(|| at("/")).or_else(|| ok.first().copied()).map(|s| s.name.clone())
        });
        let shown_gaps: Vec<&BareMetalGap> = b.missing.iter().chain(&b.warnings).filter(|g| g.code != BareMetalCode::NoSnapshots).collect();
        // btrbk without a manifest: the drop-in of the manual steps writes it
        let no_manifest = shown_gaps.iter().any(|g| g.code == BareMetalCode::NoManifest);
        // what stops a restore, else what a ready one should look at first
        let reason = if b.ready { b.warnings.first() } else { b.missing.first() }.map(|g| gap_text(g, agent));
        let gaps = shown_gaps.iter().map(|g| view! { <li>{gap_text(g, agent)}</li> }).collect_view();
        let layout = (!b.layout.is_empty()).then(|| {
            let whole = rynas_api_types::pcagent::BtrbkLayout { subvolumes: b.layout.clone(), ..Default::default() };
            let items = b.layout.iter().map(|s| {
                let at = match (s.mount.is_empty(), whole.place_of(s)) {
                    (false, _) => s.mount.clone(),
                    // nested in another sent subvolume, reached through it
                    (true, Some(place)) => format!("{place} ({})", t("rm.tip_not_mounted")),
                    (true, None) => t("rm.tip_not_mounted").to_string(),
                };
                view! { <li><span class="mono">{s.name.clone()}</span>" → "<span class="mono">{at}</span></li> }
            }).collect_view();
            view! { <h4 class="small"><strong>{tr("bkpc.bm.layout")}</strong></h4><ul class="small bkpc-list">{items}</ul> }
        });
        // one line per disk; serials and partitions in the details
        let disk_lines = (!b.disks.is_empty()).then(|| {
            let lines = b.disks.iter().map(|d| {
                let model = if d.model.is_empty() { d.name.clone() } else { d.model.clone() };
                view! { <li>{model}" · "<span class="nowrap tnum">{i18n::bytes(d.size_bytes)}</span></li> }
            }).collect_view();
            view! { <ul class="small bkpc-list">{lines}</ul> }
        });
        let disks = (!b.disks.is_empty()).then(|| view! { <h4 class="small"><strong>{tr("bkpc.bm.disks")}</strong></h4><ul class="small bkpc-list">{disk_items(&b.disks)}</ul> });
        let copies = (!b.boot_copies.is_empty()).then(|| view! { <h4 class="small"><strong>{tr("bkpc.bm.boot_copies")}</strong></h4><ul class="small bkpc-list">{boot_copy_items(&b.boot_copies)}</ul> });
        let facts = b.manifest_snapshot.is_some() || (b.layout.is_empty() && !b.subvolumes.is_empty()) || b.loader.is_some();
        let has_details = !shown_gaps.is_empty() || facts || !b.layout.is_empty() || !b.disks.is_empty() || !b.boot_copies.is_empty();
        let details = has_details.then(|| {
            ui::fold(
                Some(folds),
                "pcb.details",
                false,
                None,
                view! {
                    {(!shown_gaps.is_empty()).then(|| view! { <ul class="small bkpc-gaps">{gaps}</ul> })}
                    {facts.then(|| view! {
                        <dl class="kvs">
                            {b.manifest_snapshot.clone().map(|s| kv("bkpc.bm.manifest", ui::breakable(&s, "mono small")))}
                            {(b.layout.is_empty() && !b.subvolumes.is_empty()).then(|| kv("bkpc.bm.subvolumes", ui::breakable(&b.subvolumes.join(", "), "mono small")))}
                            {b.loader.clone().map(|l| kv("bkpc.bm.loader", ui::text(loader_label(&l))))}
                        </dl>
                    })}
                    {layout}
                    {disks}
                    {copies}
                }
                .into_any(),
            )
        });
        // the whole PC: a row like the backups, its state right, the first reason after the title
        let whole = group(
            "bkpc.bm.title".into(),
            view! { <span class="pcb-group-title">{tr("bkpc.bm.title")}</span> }.into_any(),
            reason.unwrap_or_default(),
            Some(bm_status(&b)),
            view! {
                {has_snaps.then(|| restore_part(device.clone(), b.ready, restore_code))}
                {disk_lines}
                {details}
            }
            .into_any(),
        );
        // Restore: two rows, text left and the action right
        let restore = view! {
            <div class="sec-head"><h3 class="mini-title">{tr("pcb.restore")}</h3></div>
            <div class="pcb-groups">
                <div class="pcb-group pcb-row">
                    <span class="pcb-row-main">
                        <span class="pcb-group-title">{tr("pcb.restore.file")}</span>
                        <span class="small muted">{tr("pcb.restore.file_text")}</span>
                    </span>
                    <span class="pcb-row-end">
                        {match latest {
                            Some(name) => view! { <a class="btn ghost sm" href=files_href(&name)><Icon icon=icons::FOLDER_OPEN size=14 />{tr("pcb.browse_restore")}</a> }.into_any(),
                            None => view! { <span class="small muted">{tr("bk.no_snapshots")}</span> }.into_any(),
                        }}
                    </span>
                </div>
                {whole}
            </div>
        };

        // ---- the folder and its rules: settings and facts that rarely change, with their
        // actions (Measure on the space, Delete old now on the rules)
        // on disk first (shared data once), then the data the backups hold and the measure's
        // age; snapshots that came or went since make it out of date
        let usage = p.usage.map(|u| {
            let ago = i18n::ago((js_sys::Date::now() as u64).saturating_sub(u.read_at_ms) / 1000);
            tf("bk.usage_text", &[
                ("disk", &i18n::bytes(u.on_disk_bytes())),
                ("data", &i18n::bytes(u.total_bytes)),
                ("backups", &i18n::tn("pcb.n_backups", u64::from(p.runs), &[])),
                ("n", &i18n::count(u64::from(p.runs))),
                ("ago", &ago),
            ])
        });
        let usage_view = match usage {
            Some(text) => {
                let title = p.usage.map(|u| tf("bk.usage_title", &[("excl", &i18n::bytes(u.exclusive_bytes)), ("shared", &i18n::bytes(u.shared_bytes)), ("when", &i18n::datetime(u.read_at_ms))])).unwrap_or_default();
                view! {
                    <span title=title>{text}</span>
                    {p.usage.filter(|_| p.usage_stale).map(|u| view! { <div class="small muted">{tf("bk.usage_stale", &[("ago", &i18n::ago((js_sys::Date::now() as u64).saturating_sub(u.read_at_ms) / 1000))])}</div> })}
                }
                .into_any()
            }
            None => ui::text(t("bk.usage_none").to_string()),
        };
        // before a measure that searches whole snapshots: about how long it takes (from the
        // speed of earlier measures on this NAS)
        let first_time = d.measure_plan.filter(|m| m.walks > 0).map(|m| tf("bkpc.measure_first", &[("time", &rough_time(m.estimate_s))]));
        // a measure running on the NAS (also one started elsewhere or after a backup), its
        // progress and time left, else how the last one failed
        let failed = p.measure.error.clone().map(|e| tf("bkpc.measure_failed", &[("when", &i18n::datetime(p.measure.error_at_ms.unwrap_or(0))), ("error", &e)]));
        let running_id = p.measure.task.clone();
        let usage_view = view! {
            {usage_view}
            {move || if measuring.get() {
                let left = started.get().or_else(|| running_id.clone()).map(|id| measure_progress(&id)).unwrap_or_default();
                Some(view! { <div class="small muted">{tr("bkpc.measuring_note")}" "{left}</div> }.into_any())
            } else if let Some(f) = failed.clone() {
                Some(view! { <div class="small bk-err">{f}</div> }.into_any())
            } else {
                first_time.clone().map(|f| view! { <div class="small muted">{f}</div> }.into_any())
            }}
        }
        .into_any();
        // the PC's own size (its manifest) and what each backup added: free, nothing measured
        let pc_size = match p.pc_size {
            Some(z) => {
                let at = z.at_ms.map(|ms| tf("pcb.pc_size_at", &[("when", &i18n::datetime(ms))])).unwrap_or_default();
                view! { <span>{pc_size_text(&z)}</span><div class="small muted">{at}</div> }.into_any()
            }
            None => ui::text(t("pcb.pc_size_none").to_string()),
        };
        let pc_size_help = view! {
            <span class="small muted">{tr("pcb.pc_size_help")}" "{tr(if agent { "pcb.added_agent" } else { "pcb.added_btrbk" })}</span>
        }
        .into_any();
        let free = p.free_bytes.zip(p.volume_bytes).map(|(f, v)| tf("bkpc.free_text", &[("free", &i18n::bytes(f)), ("total", &i18n::bytes(v))]));
        let alert = match p.alert_after_hours() {
            0 => t("bk.alert_off").to_string(),
            h if h % 24 == 0 => tf("bk.alert_days", &[("n", &(h / 24).to_string())]),
            h => tf("bkpc.alert_hours", &[("n", &h.to_string())]),
        };
        // the last real pass, then a pass postponed since (a receive was running: the next one
        // goes on, nothing failed)
        let real = p.last_prune.clone().map(|r| match r.error {
            Some(e) => format!("{}: {e}", i18n::datetime(r.at_ms)),
            None => tf("bkpc.last_prune_text", &[("when", &i18n::datetime(r.at_ms)), ("n", &r.deleted.to_string())]),
        });
        let postponed = p.prune_postponed_ms.map(|ms| tf("bkpc.prune_postponed", &[("when", &i18n::datetime(ms))]));
        let last_prune = match (real, postponed) {
            (Some(r), Some(q)) => Some(format!("{r} · {q}")),
            (r, q) => r.or(q),
        };
        let size_note = if d.sizes_from_quota { t("bkpc.sizes_quota") } else { t("bkpc.sizes_note") };
        const VIEWS: &[(&str, &str)] = &[("run", "bkpc.view.run"), ("subvolume", "bk.col.subvolume")];
        let rules = rules_text(&p);
        // a section like Restore and Backups: the measure and the prune buttons right on their
        // row, the long texts below over the whole width
        let measure_btn = view! { <button class="btn ghost sm" disabled=move || measuring.get() on:click=measure>{spinner_when(measuring)}<Icon icon=icons::DATABASE size=14 />{tr("bk.measure")}</button> }.into_any();
        let prune_btn = (nas && has_snaps).then(|| view! { <button class="btn ghost sm" disabled=move || pruning.get() on:click=prune>{spinner(pruning)}<Icon icon=icons::TRASH_2 size=14 />{tr("bkpc.prune")}</button> }.into_any());
        let folder = view! {
            <div class="sec-head"><h3 class="mini-title">{tr("pcb.folder_rules")}</h3></div>
            <dl class="kvs">
                {kv("bk.col.folder", ui::breakable(&p.path, "mono"))}
                {(p.incomplete > 0).then(|| kv("bk.incomplete", ui::text(i18n::count(u64::from(p.incomplete)))))}
                {free.map(|f| kv("bkpc.free", ui::text(f)))}
                {kv_act("pcb.pc_size", Some(pc_size), None, Some(pc_size_help))}
                {kv_act("bk.col.space", Some(usage_view), Some(measure_btn), has_snaps.then(|| view! { <span class="small muted">{size_note}</span> }.into_any()))}
                {kv_act("bk.f.rules", None, prune_btn, Some(ui::text(rules)))}
                {last_prune.filter(|_| nas).map(|l| kv("bkpc.last_prune", ui::text(l)))}
                // what the alert time does: with rynas-agent a warning only when the PC was on
                // while a backup was due; with btrbk a notice, nothing tells whether it is on
                {kv_act(
                    if agent { "bkpc.alert_agent" } else { "bkpc.alert" },
                    Some(ui::text(alert)),
                    None,
                    Some(view! { <span class="small muted">{tr(if agent { "bkpc.alert_agent_text" } else { "bkpc.alert_ssh_text" })}</span> }.into_any()),
                )}
                // a reinstalled PC: a new code that keeps folder, history and settings (in the
                // status card instead while the PC is unreachable)
                {agent.then_some(move || (ctl.away() == Some(false)).then(|| {
                    let again = view! { <button class="btn ghost sm" on:click=move |_| ctl.pair_again()><Icon icon=icons::KEY size=14 />{tr("pca.pair_again")}</button> }.into_any();
                    kv_act("pcb.pairing", None, Some(again), Some(view! { <span class="small muted">{tr("pca.pair_again_text")}</span> }.into_any()))
                }))}
                {p.key_fingerprint.clone().map(|fp| kv("bk.key", ui::breakable(&fp, "mono small")))}
                {p.adopted.then(|| kv("bk.adopted", ui::text(t("bk.adopted_yes").to_string())))}
            </dl>
            {form_error(measure_err)}
            {form_error(prune_err)}
        };

        // ---- btrbk: the manual steps, open while they are the only way forward (no key and
        // nothing received yet, or no manifest: its drop-in is one of them)
        let steps = (!agent).then(|| {
            let open = (p.public_key.is_empty() && d.list.is_empty()) || no_manifest;
            let hook = (!d.manifest_hook.trim().is_empty()).then(|| {
                view! {
                    <p class="small muted">{tr("pcb.hook_intro")}</p>
                    {code_block("pcb.hook", d.manifest_hook.clone())}
                }
            });
            fold(
                folds,
                "bk.steps",
                open,
                ().into_any(),
                view! {
                    <p class="small muted">{tr("pcb.ssh_recommend")}</p>
                    <p class="small muted">{tr("bk.steps_intro")}</p>
                    {code_block("bk.step.key", d.key_commands.clone())}
                    {code_block("bk.step.config", d.client_config.clone())}
                    {code_block("bk.step.test", d.test_commands.clone())}
                    {d.key_line.clone().and_then(|l| code_block("bk.step.line", l))}
                    {hook}
                    <p class="small muted">{tr("bk.steps_note")}</p>
                }
                .into_any(),
            )
        });

        view! {
            {desc}
            {status}
            {(p.public_key.is_empty() && !agent).then(|| ui::note(Tone::Warn, icons::KEY, ui::text(t("bk.no_key").to_string())))}
            {issues}
            {check}
            {restore}
            {snapshots}
            {folder}
            {steps.map(|s| view! { <div class="folds">{s}</div> })}
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
    // the PC's name; its description is the first line of the body
    let title = Signal::derive(move || dev.get_value());
    let body = view! { {move || detail.view(8, content)} }.into_any();
    let secondary = view! {
        <button type="button" class="btn ghost" disabled=move || detail.data.with(Option::is_none) on:click=move |_| settings()><Icon icon=icons::SETTINGS size=16 />{tr("bk.settings")}</button>
    }
    .into_any();
    let remove_btn = ui::Danger::new("bk.remove", Callback::new(move |_| {
        error.set(None);
        ask.set(true);
    }));
    view! {
        {ui::detail_drawer(title, close, true, body, Some(remove_btn), Some(secondary))}
        {ctl.confirm()}
        {move || ask.get().then(|| ui::confirm(Confirm { busy, error, ..Confirm::new(tf("bk.remove_title", &[("device", &dev.get_value())]), t("bk.remove_text").to_string(), "bk.remove", true, remove, Callback::new(move |_| ask.set(false))) }))}
        {move || ask_measure.get().map(|p| {
            let speed = t(if p.measured_here { "bkpc.measure_speed_here" } else { "bkpc.measure_speed_ref" });
            let text = tf("bkpc.measure_confirm_text", &[("tree", &i18n::bytes(p.walk_tree_bytes)), ("time", &rough_time(p.estimate_s)), ("speed", speed)]);
            ui::confirm(Confirm::new(t("bkpc.measure_confirm_title").to_string(), text, "bk.measure", false, start_measure, Callback::new(move |_| ask_measure.set(None))))
        })}
        {move || prune_ask.get().map(|p| {
            let keep = (p.items.len() as u32).saturating_sub(p.delete);
            let mut text = tf("snap.preview_counts", &[("keep", &keep.to_string()), ("delete", &p.delete.to_string())]);
            if let Some(b) = p.frees_at_least_bytes.filter(|b| *b > 0) {
                text = format!("{text} {}", tf("bkpc.preview_frees", &[("bytes", &i18n::bytes(b))]));
            }
            // one pass deletes at most so many: the hourly ones do the rest
            if p.per_pass > 0 && p.delete > p.per_pass {
                text = format!("{text} {}", tf("bkpc.prune_batch", &[("n", &p.per_pass.to_string())]));
            }
            ui::confirm(Confirm { busy: pruning, error: prune_err, ..Confirm::new(tf("bkpc.prune_title", &[("device", &dev.get_value())]), text, "bkpc.prune", true, do_prune, Callback::new(move |_| prune_ask.set(None))) })
        })}
        {move || del.get().map(|n| ui::confirm(Confirm { busy: del_busy, error: del_err, ..Confirm::new(tf("bkpc.delete_snap_title", &[("name", &snapshot_label(&n))]), t("bkpc.delete_snap_text").to_string(), "bkpc.delete_snap", true, delete_snap, Callback::new(move |_| del.set(None))) }))}
    }
    .into_any()
}

// ------------------------------------------------------------------ browse and copy out

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btrbk_names_are_read() {
        let s = |n| snapshot_stamp(n).map(|s| (s.prefix, s.date, s.hm, s.n));
        assert_eq!(s("@data.20261112"), Some(("@data", [2026, 11, 12], None, None)));
        assert_eq!(s("@.20261113T1543"), Some(("@", [2026, 11, 13], Some((15, 43)), None)));
        assert_eq!(s("@home.20261113T154301+0100_2"), Some(("@home", [2026, 11, 13], Some((15, 43)), Some("2"))));
        assert_eq!(s("x.20261110T0300"), Some(("x", [2026, 11, 10], Some((3, 0)), None)));
        assert_eq!(s("not-a-snapshot"), None);
        assert_eq!(s("@data.2026111"), None);
    }
}
