//! Storage Manager: overview (volumes, chassis), drives (SMART, settings), the create-volume
//! wizard and the volume detail (subvolumes, encryption, grow / replace / scrub).
//!
//! Data comes from the privileged inventory (`/storage/tree|arrays|filesystems|disks`, one
//! rynasd snapshot shared for 5 s). It lives in one store, so a page change shows cached data
//! at once; drives, arrays and tree are read again every 30 s while a storage page is visible,
//! the filesystems (btrfs usage) every 5 min, everything after every task. Disk details (SMART)
//! are read on demand only, and never automatically for a disk in standby (S6).
//! Every change is planned first (`POST /storage/plan/*`), shown, confirmed, then run as a task.

pub mod dedup;
pub mod disks;
pub mod plan;
pub mod volume;
pub mod wizard;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{Bay, M2Slot};
use rynas_api_types::storage::{
    ScrubState,
    ArrayHealth, BtrfsFilesystem, DiskKind, DiskSummary, HealthVerdict, MdArray, NodeKind, PowerState, RaidLevel, StorageTree,
    TreeNode, VolumeTree,
};

use crate::api;
use crate::chassis;
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::live::use_live;
use crate::ui::{self, EmptyState, ErrorBox, Meter, PageHeader, Skeleton, Status, Tone, usage_tone};

// ------------------------------------------------------------------ store

/// The storage inventory as last read (each list keeps its previous value while reloading).
#[derive(Clone, Copy)]
pub struct Sto {
    pub tree: RwSignal<Option<StorageTree>>,
    pub arrays: RwSignal<Option<Vec<MdArray>>>,
    pub fss: RwSignal<Option<Vec<BtrfsFilesystem>>>,
    pub disks: RwSignal<Option<Vec<DiskSummary>>>,
    /// Error code of the last round, when one of its reads failed.
    pub err: RwSignal<Option<String>>,
    busy: RwSignal<bool>,
}

thread_local! {
    static STO: Cell<Option<Sto>> = const { Cell::new(None) };
    /// Mounted storage pages (see [`pages_open`]).
    static OPEN: Cell<u32> = const { Cell::new(0) };
}

/// The store, created on first use under the app's root owner (it outlives the pages).
pub fn sto() -> Sto {
    STO.with(|c| {
        if let Some(s) = c.get() {
            return s;
        }
        let mut owner = Owner::current().expect("storage store needs a reactive owner");
        while let Some(p) = owner.parent() {
            owner = p;
        }
        let s = owner.with(|| Sto {
            tree: RwSignal::new(None),
            arrays: RwSignal::new(None),
            fss: RwSignal::new(None),
            disks: RwSignal::new(None),
            err: RwSignal::new(None),
            busy: RwSignal::new(false),
        });
        c.set(Some(s));
        s
    })
}

/// One read of the store, not generic: `apply` parses the body into its signal (the JSON parse
/// is the only per-type code, wasm size).
fn fetch(url: &'static str, apply: fn(Sto, &str) -> bool, left: Rc<Cell<u8>>, round_err: Rc<Cell<Option<String>>>, s: Sto) {
    spawn_local(async move {
        match api::get_text(url).await {
            Ok(text) => {
                if !apply(s, &text) {
                    round_err.set(Some("decode".into()));
                }
            }
            Err(e) if e.status() != 401 => round_err.set(Some(e.code().to_string())),
            Err(_) => {}
        }
        left.set(left.get() - 1);
        if left.get() == 0 {
            s.err.set(round_err.take());
            s.busy.set(false);
        }
    });
}

/// Set `sig` from `text` when it parses and differs (no re-render for an equal answer).
fn put<T: serde::de::DeserializeOwned + PartialEq + Send + Sync + 'static>(sig: RwSignal<Option<T>>, text: &str) -> bool {
    match crate::api::decode::<T>(text) {
        Ok(v) => {
            if sig.with_untracked(|cur| cur.as_ref() != Some(&v)) {
                sig.set(Some(v));
            }
            true
        }
        Err(_) => false,
    }
}

impl Sto {
    /// Read the inventory again (at most one round at a time): drives, arrays and the block
    /// tree, and with `fs` also the filesystems (btrfs usage and subvolumes: rynasd reads
    /// those from the data volumes, so they are not refreshed on every tick).
    pub fn refresh(self, fs: bool) {
        if self.busy.get_untracked() {
            return;
        }
        self.busy.set(true);
        let left = Rc::new(Cell::new(if fs { 4u8 } else { 3 }));
        let err = Rc::new(Cell::new(None));
        fetch("/api/v1/storage/tree", |s, t| put(s.tree, t), left.clone(), err.clone(), self);
        fetch("/api/v1/storage/arrays", |s, t| put(s.arrays, t), left.clone(), err.clone(), self);
        fetch("/api/v1/storage/disks", |s, t| put(s.disks, t), left.clone(), err.clone(), self);
        if fs {
            fetch("/api/v1/storage/filesystems", |s, t| put(s.fss, t), left, err, self);
        }
    }

    /// Read everything again (page open, Retry, after a task).
    pub fn load(self) {
        self.refresh(true);
    }

    /// Read the drives again now, even while a round runs (that round may carry the answer
    /// from before a change to one disk).
    pub fn reload_disks(self) {
        spawn_local(async move {
            if let Ok(text) = api::get_text("/api/v1/storage/disks").await {
                put(self.disks, &text);
            }
        });
    }

    /// Everything needed by the overview has arrived at least once.
    pub fn ready(self) -> bool {
        self.tree.with(Option::is_some) && self.arrays.with(Option::is_some) && self.disks.with(Option::is_some)
    }

    pub fn disk(self, name: &str) -> Option<DiskSummary> {
        self.disks.with(|d| d.as_ref().and_then(|d| d.iter().find(|d| d.name == name).cloned()))
    }
}

/// Storage pages open (the shell pauses its own `/storage` poll meanwhile).
pub fn pages_open() -> bool {
    OPEN.with(Cell::get) > 0
}

/// Drives, arrays and tree every 30 s, the filesystems every 5 min, while a storage page is
/// mounted and the tab visible; everything right away on mount. Disk details (SMART) are never
/// read on a timer.
pub fn use_refresh() -> Sto {
    let s = sto();
    s.load();
    OPEN.with(|c| c.set(c.get() + 1));
    let ticks = StoredValue::new(0u32);
    crate::live::poll(Duration::from_secs(30), move || {
        ticks.update_value(|t| *t += 1);
        s.refresh(ticks.get_value().is_multiple_of(10));
    });
    on_cleanup(move || OPEN.with(|c| c.set(c.get().saturating_sub(1))));
    s
}

// ------------------------------------------------------------------ view model

/// A volume as the pages show it: the filesystem with the md array and disks below, or a
/// locked LUKS container.
#[derive(Clone, PartialEq, Default)]
pub struct Vol {
    /// Volume label: route id and the name typed to confirm destructive actions (the
    /// filesystem UUID when there is no label).
    pub id: String,
    /// Created the rynas way (md array + valid label): the write actions apply.
    pub managed: bool,
    pub locked: bool,
    pub encrypted: bool,
    pub fstype: String,
    pub mountpoint: Option<String>,
    pub size: u64,
    pub used: u64,
    pub health: HealthVerdict,
    pub md: Option<MdArray>,
    pub fs: Option<BtrfsFilesystem>,
    /// Whole disks below, by kernel name.
    pub disks: Vec<String>,
}

pub fn valid_label(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 16
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn walk<'a>(n: &'a TreeNode, out: &mut Vec<&'a TreeNode>) {
    out.push(n);
    for c in &n.children {
        walk(c, out);
    }
}

/// Label of an md array from its superblock name ("host:vol1" → "vol1").
fn md_label(a: &MdArray) -> Option<String> {
    a.md_name.as_deref().map(|n| n.rsplit(':').next().unwrap_or(n).to_string())
}

fn vol_from(layers: &[TreeNode], arrays: &[MdArray]) -> (Option<MdArray>, Vec<String>) {
    let mut nodes = Vec::new();
    for l in layers {
        walk(l, &mut nodes);
    }
    let md = nodes.iter().find(|n| n.kind == NodeKind::Md).and_then(|n| arrays.iter().find(|a| a.name == n.name)).cloned();
    let mut disks: Vec<String> = nodes.iter().filter(|n| n.kind == NodeKind::Disk).map(|n| n.name.clone()).collect();
    disks.sort();
    disks.dedup();
    (md, disks)
}

/// The volumes of the inventory (tracked: call inside a reactive closure).
pub fn volumes(s: Sto) -> Vec<Vol> {
    let arrays = s.arrays.get().unwrap_or_default();
    let fss = s.fss.get().unwrap_or_default();
    let Some(tree) = s.tree.get() else { return Vec::new() };
    let mut out = Vec::new();
    for v in &tree.volumes {
        out.push(vol_of_tree(v, &arrays, &fss));
    }
    for l in &tree.locked {
        let (md, disks) = vol_from(&l.children, &arrays);
        let id = md.as_ref().and_then(md_label).unwrap_or_else(|| l.name.clone());
        out.push(Vol {
            managed: md.is_some() && valid_label(&id),
            id,
            locked: true,
            encrypted: true,
            size: l.size,
            health: l.health,
            md,
            disks,
            ..Default::default()
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn vol_of_tree(v: &VolumeTree, arrays: &[MdArray], fss: &[BtrfsFilesystem]) -> Vol {
    let (md, disks) = vol_from(&v.layers, arrays);
    let id = v.label.clone().filter(|l| !l.is_empty()).or_else(|| md.as_ref().and_then(md_label)).unwrap_or_else(|| v.id.clone());
    let fs = fss.iter().find(|f| f.uuid == v.id).cloned();
    Vol {
        managed: md.is_some() && valid_label(&id) && v.fstype == "btrfs",
        id,
        locked: false,
        encrypted: v.encrypted,
        fstype: v.fstype.clone(),
        mountpoint: v.mountpoint.clone(),
        size: fs.as_ref().map(|f| f.usable_size()).filter(|s| *s > 0).unwrap_or(v.size),
        used: v.used,
        health: v.health,
        md,
        fs,
        disks,
    }
}

/// Disks a new volume (or a grow / replace) may use: nothing on them, not removable.
pub fn free_disks(s: Sto) -> Vec<DiskSummary> {
    // leftovers (old partitions, an unassembled DSM array) are erased by the plan with a warning;
    // only live use and the system disk exclude a drive
    s.disks.get().unwrap_or_default().into_iter().filter(|d| !d.in_use && !d.system && !d.removable).collect()
}

// ------------------------------------------------------------------ labels and tones

pub fn health(h: HealthVerdict) -> (Tone, &'static str) {
    match h {
        HealthVerdict::Healthy => (Tone::Ok, "health.healthy"),
        HealthVerdict::Warning => (Tone::Warn, "sto.h.warning"),
        HealthVerdict::Failing => (Tone::Err, "sto.h.failing"),
        HealthVerdict::Unknown => (Tone::Unknown, "health.unknown"),
    }
}

/// A disk's health in words: "Not available" for a device without SMART, where "Unknown"
/// would suggest a reading still to come.
pub fn disk_health(d: &DiskSummary) -> (Tone, &'static str) {
    match d.health {
        HealthVerdict::Unknown if d.smart_unsupported => (Tone::Unknown, "sto.h.no_smart"),
        h => health(h),
    }
}

pub fn array_health(h: ArrayHealth) -> (Tone, &'static str) {
    match h {
        ArrayHealth::Healthy => (Tone::Ok, "health.healthy"),
        ArrayHealth::Degraded => (Tone::Warn, "health.degraded"),
        ArrayHealth::Failed => (Tone::Err, "sto.a.failed"),
        ArrayHealth::Inactive => (Tone::Unknown, "sto.a.inactive"),
        ArrayHealth::Unknown => (Tone::Unknown, "health.unknown"),
    }
}

pub fn power(p: PowerState) -> (Tone, &'static str) {
    match p {
        PowerState::Active => (Tone::Ok, "sto.active"),
        PowerState::Idle => (Tone::Ok, "sto.p.idle"),
        PowerState::Standby => (Tone::Info, "sto.asleep"),
        PowerState::Sleep => (Tone::Info, "sto.p.sleep"),
        PowerState::Unknown => (Tone::Unknown, "health.unknown"),
    }
}

pub fn asleep(p: PowerState) -> bool {
    matches!(p, PowerState::Standby | PowerState::Sleep)
}

pub fn kind_key(k: DiskKind) -> &'static str {
    match k {
        DiskKind::Hdd => "sto.hdd",
        DiskKind::Ssd => "sto.ssd",
        DiskKind::Nvme => "sto.nvme",
        DiskKind::Unknown => "health.unknown",
    }
}

pub fn level_key(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Single => "sto.lvl.single",
        RaidLevel::Raid0 => "sto.lvl.raid0",
        RaidLevel::Raid1 => "sto.lvl.raid1",
        RaidLevel::Raid5 => "sto.lvl.raid5",
        RaidLevel::Raid6 => "sto.lvl.raid6",
        RaidLevel::Raid10 => "sto.lvl.raid10",
    }
}

/// The md level of an array as a [`RaidLevel`] (a one-member RAID1 is "single").
pub fn md_level(a: &MdArray) -> Option<RaidLevel> {
    Some(match a.level.as_str() {
        "raid1" if a.raid_disks <= 1 => RaidLevel::Single,
        "raid0" => RaidLevel::Raid0,
        "raid1" => RaidLevel::Raid1,
        "raid5" => RaidLevel::Raid5,
        "raid6" => RaidLevel::Raid6,
        "raid10" => RaidLevel::Raid10,
        _ => return None,
    })
}

pub fn md_level_text(a: &MdArray) -> String {
    md_level(a).map(|l| t(level_key(l)).to_string()).unwrap_or_else(|| a.level.to_uppercase())
}

pub fn pct_of(used: u64, size: u64) -> f64 {
    if size == 0 { 0.0 } else { used as f64 * 100.0 / size as f64 }
}

/// Serial a destructive request names a disk by (its kernel name when it has none).
pub fn serial_of(d: &DiskSummary) -> String {
    if d.serial.trim().is_empty() { d.name.clone() } else { d.serial.clone() }
}

// ------------------------------------------------------------------ shared pieces

/// Storage section tabs: Overview | Drives.
#[component]
pub fn SubNav() -> impl IntoView {
    let loc = leptos_router::hooks::use_location();
    let item = move |href: &'static str, key: &'static str, icon: &'static str| {
        let current = move || {
            let p = loc.pathname.get();
            let on = if href == "/storage" { p == "/storage" || p.starts_with("/storage/volumes") } else { p.starts_with(href) };
            on.then_some("page")
        };
        view! { <a class="subnav-item" href=href aria-current=current><Icon icon=icon size=16 />{tr(key)}</a> }
    };
    view! {
        <nav class="subnav" aria-label=tr("sto.title")>
            {item("/storage", "sto.tab.overview", icons::LAYOUT_DASHBOARD)}
            {item("/storage/disks", "sto.disks", icons::HARD_DRIVE)}
        </nav>
    }
}

/// Skeleton while the first round is loading, error box with Retry when it failed.
pub fn loading(s: Sto, lines: usize) -> AnyView {
    view! {
        <div class="card">
            {move || match s.err.get() {
                Some(code) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| s.load()) /> }.into_any(),
                None => view! { <Skeleton lines=lines /> }.into_any(),
            }}
        </div>
    }
    .into_any()
}

/// A failed refresh while older data is still shown: a slim notice with Retry.
pub fn stale_notice(s: Sto) -> impl IntoView {
    move || {
        s.err.get().map(|code| {
            view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <span>{tr("sto.refresh_failed")}" "{i18n::err_text(&code)}</span>
                    <button class="btn ghost sm row-end" on:click=move |_| s.load()>{tr("err.retry")}</button> }.into_any())}
            }
        })
    }
}

// ------------------------------------------------------------------ overview page

#[component]
pub fn Storage() -> impl IntoView {
    let s = use_refresh();
    let live = use_live();
    let bays = Memo::new(move |_| live.hardware.with(|h| h.as_ref().map(|h| h.bays.clone()).unwrap_or_default()));
    let m2 = Memo::new(move |_| live.hardware.with(|h| h.as_ref().map(|h| h.m2.clone()).unwrap_or_default()));
    let model = Memo::new(move |_| live.hardware.with(|h| h.as_ref().map(|h| h.model.clone()).unwrap_or_default()));
    let status_led = Memo::new(move |_| live.hardware.with(|h| h.as_ref().and_then(|h| h.status_led.clone())));
    let has_bays = Memo::new(move |_| bays.with(|b| !b.is_empty()));
    let vols = Memo::new(move |_| volumes(s));
    let free = Memo::new(move |_| free_disks(s).len());
    let selected = RwSignal::new(None::<String>);
    let unlock = RwSignal::new(None::<String>);

    view! {
        <div class="page">
            <PageHeader icon=icons::HARD_DRIVE title="sto.title" subtitle="sto.subtitle">
                <a class="btn primary" href="/storage/create" aria-disabled=move || (free.get() == 0).then_some("true") title=move || (free.get() == 0).then(|| t("sto.no_free").to_string())>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("sto.create")}</span>
                </a>
            </PageHeader>
            <SubNav />
            <Show when=move || s.ready() fallback=move || loading(s, 6)>
                {stale_notice(s)}
                <section class="card widget w-12" style="--i:0">
                    <h2 class="section-title"><Icon icon=icons::DATABASE size=16 />{tr("sto.filesystems")}</h2>
                    {move || {
                        let v = vols.get();
                        if v.is_empty() {
                            view! {
                                <EmptyState icon=icons::DATABASE title=Signal::derive(|| t("sto.no_volumes").to_string()) text=Signal::derive(|| t("sto.no_volumes_text").to_string())>
                                    <a class="btn primary" href="/storage/create"><Icon icon=icons::PLUS size=16 />{tr("sto.create")}</a>
                                </EmptyState>
                            }
                            .into_any()
                        } else {
                            view! { <div class="vol-grid">{v.into_iter().map(|v| vol_card(v, unlock)).collect_view()}</div> }.into_any()
                        }
                    }}
                    {move || (free.get() > 0 && !vols.with(Vec::is_empty)).then(|| view! {
                        <p class="muted small free-note">
                            <Icon icon=icons::INFO size=14 />
                            {move || tf("sto.free_disks", &[("n", &free.get().to_string())])}
                        </p>
                    })}
                </section>
                <section class="card widget w-12" style="--i:1">
                    <h2 class="section-title">
                        <Icon icon=icons::BOX size=16 />
                        {move || if has_bays.get() { t("sto.chassis") } else { t("sto.generic") }}
                    </h2>
                    // built once per kind: the chassis follows the bays and drives by itself
                    {move || if has_bays.get() {
                        chassis_view(s, bays, m2, model, status_led, selected)
                    } else {
                        (move || disk_grid(s.disks.get().unwrap_or_default(), selected)).into_any()
                    }}
                </section>
                // deduplication of filesystems without a volume page (system volume, other folders)
                <dedup::OtherDedupJobs volumes=Signal::derive(move || vols.with(|v| v.iter().map(|x| x.id.clone()).collect::<Vec<_>>())) />
            </Show>
            {move || selected.get().map(|name| view! { <disks::DiskDrawer name=name on_close=Callback::new(move |_| selected.set(None)) /> })}
            {move || unlock.get().map(|id| view! { <volume::UnlockDialog id=id on_close=Callback::new(move |_| unlock.set(None)) /> })}
        </div>
    }
}

fn vol_card(v: Vol, unlock: RwSignal<Option<String>>) -> AnyView {
    let (tone, key) = health(v.health);
    let level = v.md.as_ref().map(md_level_text);
    let href = format!("/storage/volumes/{}", api::encode(&v.id));
    if v.locked {
        let id = v.id.clone();
        return view! {
            <div class="vol-card locked">
                <div class="vol-card-top">
                    <span class="w-icon"><Icon icon=icons::KEY size=16 /></span>
                    <a class="vol-card-name" href=href>{v.id.clone()}</a>
                    <span class="row-end"><Status tone=Tone::Info text=t("sto.locked").to_string() /></span>
                </div>
                <div class="vol-card-tags">{level.map(|l| view! { <span class="chip accent">{l}</span> })}</div>
                <p class="muted small">{tr("sto.locked_text")}</p>
                <div class="vol-card-foot">
                    <span class="muted small tnum">{i18n::bytes_si(v.size)}</span>
                    {v.managed.then(|| view! {
                        <button class="btn primary sm row-end" on:click=move |_| unlock.set(Some(id.clone()))>
                            <Icon icon=icons::KEY size=14 />{tr("sto.unlock")}
                        </button>
                    })}
                </div>
            </div>
        }
        .into_any();
    }
    // without root some filesystems report no size: show what is known, no meter
    let sized = v.size > 0 && v.size >= v.used;
    let pct = pct_of(v.used, v.size);
    let utone = usage_tone(pct);
    let sync = v.md.as_ref().and_then(|a| a.sync.clone());
    let scrub_running = v.fs.as_ref().and_then(|f| f.scrub.as_ref()).is_some_and(|s| s.status == ScrubState::Running);
    let stale = v.fs.as_ref().is_some_and(|f| f.stale) || v.md.as_ref().is_some_and(|a| a.stale);
    view! {
        <a class=format!("vol-card {}", tone.class()) href=href>
            <div class="vol-card-top">
                <span class="w-icon"><Icon icon=icons::DATABASE size=16 /></span>
                <span class="vol-card-name">{v.id.clone()}</span>
                <span class="row-end"><Status tone=tone text=t(key).to_string() /></span>
            </div>
            // level and encryption on their own line: the name and the health pill never wrap
            <div class="vol-card-tags">
                {level.map(|l| view! { <span class="chip accent">{l}</span> })}
                {v.encrypted.then(|| view! { <span class="chip" title=t("sto.encrypted")><Icon icon=icons::KEY size=12 />{tr("sto.encrypted")}</span> })}
            </div>
            {if sized {
                view! {
                    <div class="vol-card-usage">
                        <span class="vol-card-pct tnum">{i18n::pct(pct)}</span>
                        <span class="muted small tnum">{i18n::bytes_of(v.used, v.size)}</span>
                    </div>
                    <Meter pct=pct tone=Signal::derive(move || utone) />
                }
                .into_any()
            } else {
                view! { <div class="vol-card-usage"><span class="muted small tnum">{tf("sto.used_n", &[("n", &i18n::bytes(v.used))])}</span></div> }.into_any()
            }}
            <div class="vol-card-foot muted small">
                <span class="mono">{v.mountpoint.clone().unwrap_or_default()}</span>
                {(!v.disks.is_empty()).then(|| view! { <span>{tf("sto.n_disks", &[("n", &v.disks.len().to_string())])}</span> })}
                {stale.then(|| view! { <span class="chip" title=t("sto.stale_hint")><Icon icon=icons::MOON_STAR size=12 />{tr("sto.stale")}</span> })}
            </div>
            {sync.map(|s| sync_row(&s))}
            {scrub_running.then(|| view! { <div class="small info tone-text"><span class="spin"><Icon icon=icons::REFRESH_CW size=12 /></span>" "{tr("sto.scrub_running")}</div> })}
        </a>
    }
    .into_any()
}

/// md resync / recovery / check progress.
pub fn sync_row(s: &rynas_api_types::storage::SyncProgress) -> AnyView {
    let action = i18n::try_t(&format!("sto.sync.{}", s.action)).unwrap_or(t("sto.sync")).to_string();
    let pct = s.pct.unwrap_or(0.0) as f64;
    let eta = s.finish_s.map(i18n::duration);
    let waiting = s.waiting.is_some();
    view! {
        <div class="sync">
            <div class="sub-row small">
                <span>{action}</span>
                <span class="tnum">
                    {if waiting { t("sto.sync.waiting").to_string() } else { i18n::pct(pct) }}
                    {eta.map(|e| format!(" · {e}"))}
                </span>
            </div>
            <Meter pct=pct tone=Signal::derive(|| Tone::Info) />
        </div>
    }
    .into_any()
}

/// The shared chassis drawing with the provider's bays and M.2 slots and the inventory's drives.
fn chassis_view(s: Sto, bays: Memo<Vec<Bay>>, m2: Memo<Vec<M2Slot>>, model: Memo<String>, status_led: Memo<Option<String>>, selected: RwSignal<Option<String>>) -> AnyView {
    let bay_slots = Memo::new(move |_| {
        s.disks.with(|d| {
            let d = d.as_deref().unwrap_or(&[]);
            let mut v: Vec<_> = bays.with(|b| b.iter().map(|b| chassis::bay_slot(b.number, b.present, b.powered, b.asleep, b.celsius, b.device.clone(), d)).collect());
            v.sort_by_key(|x| x.key);
            v
        })
    });
    let m2_slots = Memo::new(move |_| s.disks.with(|d| m2.with(|m| chassis::m2_slots(m, d.as_deref().unwrap_or(&[])))));
    chassis::chassis(chassis::Chassis {
        model: model.into(),
        bays: bay_slots,
        m2: m2_slots,
        status_led: status_led.into(),
        on_select: Some(Callback::new(move |name| selected.set(Some(name)))),
        identify: None,
    })
}

fn disk_line(d: &DiskSummary) -> String {
    format!("{} · {}", t(kind_key(d.kind)), i18n::bytes_si(d.size))
}

fn disk_grid(disks: Vec<DiskSummary>, selected: RwSignal<Option<String>>) -> AnyView {
    if disks.is_empty() {
        return ui::empty_line("sto.no_disks");
    }
    view! {
        <div class="disk-grid">
            {disks
                .into_iter()
                .map(|d| {
                    let (ptone, pkey) = power(d.power_state);
                    let (htone, hkey) = disk_health(&d);
                    let name = d.name.clone();
                    view! {
                        <button class="disk-card" type="button" on:click=move |_| selected.set(Some(name.clone()))>
                            <div class="disk-card-top">
                                <span class="w-icon"><Icon icon=icons::HARD_DRIVE size=16 /></span>
                                <span class="mono strong">{d.name.clone()}</span>
                                <span class="row-end tnum">{i18n::bytes_si(d.size)}</span>
                            </div>
                            <div class="small ellipsis" title=d.model.clone()>{d.model.clone()}</div>
                            <div class="disk-card-bottom">
                                {(d.health != HealthVerdict::Unknown).then(|| view! { <Status tone=htone text=t(hkey).to_string() /> })}
                                <Status tone=ptone text=t(pkey).to_string() />
                            </div>
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
    .into_any()
}

/// "M.2 1", or plain "M.2" when the provider does not know the physical slot.
pub fn m2_label(n: Option<u32>) -> String {
    match n {
        Some(n) => format!("{} {n}", t("sto.m2")),
        None => t("sto.m2").to_string(),
    }
}

/// Where a disk sits, for the Bay columns: "2" (bay 2), "M.2 1"; None when unknown.
pub fn slot_text(d: &DiskSummary) -> Option<String> {
    d.bay.map(|b| b.to_string()).or_else(|| d.m2.map(|m| m2_label(m.number)))
}

/// One-line description of a disk for pickers: "sdb · HDD · 4.0 TB · WDC WD40EFRX (bay 2)".
pub fn disk_title(d: &DiskSummary) -> String {
    let at = match (d.bay, d.m2) {
        (Some(b), _) => format!(" ({} {b})", t("sto.bay")),
        (None, Some(m)) => format!(" ({})", m2_label(m.number)),
        (None, None) => String::new(),
    };
    format!("{} · {} · {}{at}", d.name, disk_line(d), d.model)
}
