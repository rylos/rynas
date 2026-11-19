//! Volume detail: usage, RAID layout, scrub, encryption (unlock / lock, recovery key, LUKS
//! header backup), subvolumes and the md tuning; grow, replace and scrub start from here.
//!
//! Every change follows the same flow: form → plan from `POST /storage/plan/*` → typed
//! confirmation (volume name) when it erases a disk → the task, followed until it ends.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use rynas_api_types::storage::{
    DiskSummary, FlexBlocker, FlexLayout, FlexPending, MdArray, ScrubState, ScrubStatus, MemberRole, RaidLevel, RecoveryKey, StoragePlan, SubvolumeInfo, VolumeSettings,
    VolumeSettingsState,
};
use rynas_api_types::{TaskInfo, TaskStarted, TaskStatus};
use serde_json::{Value, json};

use crate::tasks::TaskRun;
use super::plan::{plan_view, tier_list, tier_map, tolerance_text};
use super::{
    Sto, Vol, array_health, disk_title, free_disks, health, level_key, loading, md_level, md_level_text, pct_of,
    serial_of, stale_notice, sync_row, use_refresh, volumes,
};
use crate::api;
use crate::form::{SaveBar, form_error, save_bar_with, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, EmptyState, ErrorBox, Meter, Modal, Notice, Skeleton, Status, Tone, WidgetHead, kv_row, typed_confirm, usage_tone};

// ------------------------------------------------------------------ flow: plan → confirm → task

#[derive(Clone, PartialEq)]
pub enum Stage {
    Form,
    Plan(StoragePlan),
    Running(String),
    Done(TaskInfo),
}

/// POST a planner request; the answer becomes the shown plan.
pub fn fetch_plan(url: &'static str, body: Value, stage: RwSignal<Stage>, busy: RwSignal<bool>, error: RwSignal<Option<String>>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = api::post::<_, StoragePlan>(url, Some(&body)).await;
        let _ = busy.try_set(false);
        match r {
            Ok(p) => {
                let _ = stage.try_set(Stage::Plan(p));
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
    });
}

/// POST an operation that answers 202 `TaskStarted` (also DELETE with `delete`).
pub fn start_task(url: String, body: Option<Value>, delete: bool, stage: RwSignal<Stage>, busy: RwSignal<bool>, error: RwSignal<Option<String>>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = if delete { api::delete::<TaskStarted>(&url).await } else { api::post::<_, TaskStarted>(&url, body.as_ref()).await };
        let _ = busy.try_set(false);
        match r {
            Ok(t) => {
                let _ = stage.try_set(Stage::Running(t.task_id));
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
    });
}

/// The stages after the form of a storage dialog: the plan with its confirmation, the running
/// task, the outcome. [`Flow::body`] goes in the dialog's body, [`Flow::foot`] in its action
/// row (`Modal`'s `foot`), so the buttons stay in sight however long the plan is.
#[derive(Clone, Copy)]
pub struct Flow {
    pub stage: RwSignal<Stage>,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    /// The name typed to confirm a plan that destroys data.
    typed: RwSignal<String>,
    pass: RwSignal<String>,
    confirm_name: StoredValue<Option<String>>,
    need_pass: bool,
    exec_key: &'static str,
    /// The disks the form chose (their serials go with the request already): any other disk
    /// the plan erases is confirmed by typing its serial number.
    own: Signal<Vec<String>>,
    /// The serial numbers typed for those other disks: (disk, text).
    serials: RwSignal<Vec<(String, String)>>,
    /// Gets the typed passphrase (empty when none is asked) and the serial numbers of the
    /// other disks the plan erases.
    exec: Callback<(String, Vec<String>)>,
    on_close: Callback<()>,
}

impl Flow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stage: RwSignal<Stage>,
        confirm_name: Option<String>,
        need_pass: bool,
        exec_key: &'static str,
        busy: RwSignal<bool>,
        error: RwSignal<Option<String>>,
        exec: Callback<(String, Vec<String>)>,
        on_close: Callback<()>,
    ) -> Self {
        Flow {
            stage,
            busy,
            error,
            typed: RwSignal::new(String::new()),
            pass: RwSignal::new(String::new()),
            confirm_name: StoredValue::new(confirm_name),
            need_pass,
            exec_key,
            own: Signal::stored(Vec::new()),
            serials: RwSignal::new(Vec::new()),
            exec,
            on_close,
        }
    }

    /// The disks the form chose: other disks the plan erases need their serial typed.
    pub fn with_own(self, own: Signal<Vec<String>>) -> Self {
        Flow { own, ..self }
    }

    fn need_typed(self, p: &StoragePlan) -> bool {
        p.steps.iter().any(|s| !s.destroys.is_empty()) && self.confirm_name.with_value(Option::is_some)
    }

    /// The other disks the plan erases (not chosen in the form), as the inventory has them.
    fn others(self, p: &StoragePlan) -> Vec<DiskSummary> {
        if self.confirm_name.with_value(Option::is_none) {
            return Vec::new();
        }
        let s = super::sto();
        let own = self.own.get_untracked();
        let mut v: Vec<DiskSummary> = Vec::new();
        for name in erased_disk_names(p) {
            if own.contains(&name) || v.iter().any(|d| d.name == name) {
                continue;
            }
            if let Some(d) = untrack(|| s.disk(&name)) {
                v.push(d);
            }
        }
        v
    }

    /// The body of the current stage after the form (nothing on the form).
    pub fn body(self) -> AnyView {
        let Flow { stage, error, typed, pass, confirm_name, need_pass, serials, .. } = self;
        let s = super::sto();
        match stage.get() {
            Stage::Form => ().into_any(),
            Stage::Plan(p) => {
                let need_typed = self.need_typed(&p);
                let others = self.others(&p);
                let erased = if need_typed { erased_disk_names(&p) } else { Vec::new() };
                view! {
                    {plan_view(&p)}
                    {(p.valid && !erased.is_empty()).then(|| erase_box(&erased))}
                    {p.valid.then(|| others.into_iter().map(|d| serial_confirm(d, serials)).collect_view())}
                    {(p.valid && need_typed).then(|| typed_confirm(confirm_name.get_value().unwrap_or_default(), typed))}
                    {(p.valid && need_pass).then(|| view! {
                        <label class="field">
                            <span class="field-label">{tr("sto.passphrase")}</span>
                            <input class="input" type="password" autocomplete="off" prop:value=move || pass.get() on:input=move |ev| pass.set(event_target_value(&ev)) />
                        </label>
                    })}
                    {form_error(error)}
                }
                .into_any()
            }
            Stage::Running(id) => view! {
                <TaskRun id=id on_done=Callback::new(move |t: TaskInfo| {
                    s.load();
                    stage.set(Stage::Done(t));
                }) />
            }
            .into_any(),
            Stage::Done(t) => done_note(&t),
        }
    }

    /// The action row of the current stage after the form: Back and the action on the plan,
    /// nothing while the task runs, Close on the outcome.
    pub fn foot(self) -> AnyView {
        let Flow { stage, busy, typed, pass, confirm_name, need_pass, exec_key, exec, on_close, serials, .. } = self;
        match stage.get() {
            Stage::Plan(p) => {
                let valid = p.valid;
                let destructive = p.steps.iter().any(|s| !s.destroys.is_empty());
                let need_typed = self.need_typed(&p);
                // (disk, serial) of the other disks the plan erases
                let others: Vec<(String, String)> = self.others(&p).iter().map(|d| (d.name.clone(), serial_of(d))).collect();
                let others = StoredValue::new(others);
                let serials_ok = move || {
                    others.with_value(|o| {
                        serials.with(|typed| {
                            o.iter().all(|(n, want)| typed.iter().any(|(d, t)| d == n && t.trim() == want))
                        })
                    })
                };
                let ok = move || {
                    valid
                        && (!need_typed || confirm_name.with_value(|n| n.as_deref() == Some(typed.get().trim())))
                        && (!need_pass || pass.with(|p| p.chars().count() >= 8))
                        && serials_ok()
                };
                view! {
                    <button class="btn ghost" type="button" on:click=move |_| stage.set(Stage::Form)>{tr("back")}</button>
                    <button
                        class=if destructive { "btn danger" } else { "btn primary" }
                        type="button"
                        disabled=move || !ok() || busy.get()
                        on:click=move |_| exec.run((pass.get_untracked(), others.with_value(|o| o.iter().map(|(_, s)| s.clone()).collect())))
                    >
                        {spinner(busy)}{tr(exec_key)}
                    </button>
                }
                .into_any()
            }
            Stage::Done(_) => ui::close_foot(on_close),
            Stage::Form | Stage::Running(_) => ().into_any(),
        }
    }
}

/// The disks a plan erases (step `destroys` that name a whole disk of the inventory), in plan
/// order.
fn erased_disk_names(p: &StoragePlan) -> Vec<String> {
    let s = super::sto();
    let mut v: Vec<String> = Vec::new();
    for d in p.steps.iter().flat_map(|st| st.destroys.iter()) {
        if !v.contains(d) && untrack(|| s.disk(d)).is_some() {
            v.push(d.clone());
        }
    }
    v
}

/// "Everything on these drives will be erased": each drive with model, serial and size (the
/// parts it has).
pub fn erase_box(names: &[String]) -> AnyView {
    let s = super::sto();
    view! {
        <div class="erase-box" role="note">
            <strong><Icon icon=icons::TRIANGLE_ALERT size=16 />{tr("wiz.erase_title")}</strong>
            <ul>
                {names.iter().map(|n| {
                    let d = untrack(|| s.disk(n));
                    let facts = d.map(|d| {
                        [d.model.trim().to_string(), d.serial.trim().to_string(), i18n::bytes_si(d.size)]
                            .into_iter()
                            .filter(|x| !x.is_empty())
                            .collect::<Vec<_>>()
                            .join(" · ")
                    });
                    view! { <li><span class="mono strong">{n.clone()}</span>" "<span class="muted small">{facts.unwrap_or_default()}</span></li> }
                }).collect_view()}
            </ul>
        </div>
    }
    .into_any()
}

/// The serial number of a disk the plan erases besides the chosen ones, typed to confirm.
fn serial_confirm(d: DiskSummary, serials: RwSignal<Vec<(String, String)>>) -> AnyView {
    let name = d.name.clone();
    let n2 = name.clone();
    // a disk without a serial number (a virtual disk) is confirmed by its name
    let prompt = if d.serial.trim().is_empty() {
        tf("confirm.type_name", &[("name", &name)])
    } else {
        tf("sto.type_serial", &[("disk", &name)])
    };
    view! {
        <label class="field confirm-field">
            <span class="field-label">{prompt}</span>
            <input
                class="input mono"
                autocomplete="off"
                autocapitalize="off"
                spellcheck="false"
                prop:value=move || serials.with(|v| v.iter().find(|(x, _)| *x == name).map(|(_, t)| t.clone()).unwrap_or_default())
                on:input=move |ev| {
                    let text = event_target_value(&ev);
                    serials.update(|v| {
                        v.retain(|(x, _)| *x != n2);
                        v.push((n2.clone(), text));
                    });
                }
            />
        </label>
    }
    .into_any()
}

/// The outcome of a storage task.
fn done_note(t: &TaskInfo) -> AnyView {
    let ok = t.status == TaskStatus::Ok;
    let (tone, key) = if ok { (Tone::Ok, "sto.done_ok") } else { (Tone::Err, "sto.done_err") };
    let err = t.error.clone();
    view! {
        {ui::note(tone, tone.icon(), view! { {tr(key)} }.into_any())}
        {err.map(|e| view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{ui::tech_detail(e)}</p> })}
    }
    .into_any()
}

fn vol_url(id: &str, what: &str) -> String {
    format!("/api/v1/storage/volumes/{}/{what}", api::encode(id))
}

// ------------------------------------------------------------------ page

/// Parts of the page that keep their own state (forms, downloads, lists): rebuilt only when
/// these change, not on every inventory refresh.
#[derive(Clone, PartialEq)]
struct Stable {
    id: String,
    locked: bool,
    managed: bool,
    encrypted: bool,
    subvolumes: bool,
    raid56: bool,
    /// A btrfs data volume of several devices: the UUID its degraded-mount setting is for.
    btrfs_boot: Option<String>,
}

/// How often the page reads a running scrub's progress.
const SCRUB_POLL_MS: u32 = 4000;

#[component]
pub fn VolumePage() -> impl IntoView {
    let s = use_refresh();
    let params = use_params_map();
    let id = Memo::new(move |_| params.with(|p| p.get("name").unwrap_or_default()));
    let vol = Memo::new(move |_| {
        let id = id.get();
        volumes(s).into_iter().find(|v| v.id == id)
    });
    let stable = Memo::new(move |_| {
        vol.with(|v| {
            v.as_ref().map(|v| Stable {
                id: v.id.clone(),
                locked: v.locked,
                managed: v.managed,
                encrypted: v.encrypted,
                subvolumes: v.managed && !v.locked && v.fstype == "btrfs",
                raid56: v.managed
                    && !v.locked
                    && !v.tiered
                    && v.md.as_ref().and_then(md_level).is_some_and(|l| matches!(l, RaidLevel::Raid5 | RaidLevel::Raid6)),
                // the root filesystem is mounted by the boot loader's command line, not by rynas
                btrfs_boot: v
                    .fs
                    .as_ref()
                    .filter(|f| super::btrfs::multi_device(f) && !f.mountpoints.iter().any(|m| m == "/"))
                    .filter(|f| f.repair.as_ref().is_none_or(|r| !r.system))
                    .map(|f| f.uuid.clone()),
            })
        })
    });
    // dialogs and the drive drawer live here, so a refresh of the data never closes them
    let modal = RwSignal::new(Open::None);
    let selected = RwSignal::new(None::<String>);
    let close = Callback::new(move |_| modal.set(Open::None));
    let current = move || vol.get_untracked().unwrap_or_default();
    // a scrub of this volume runs, or a scrub task was just started (btrfs may still say
    // "never" for a second): the scrub card follows it, then everything is read again once
    // (the volume's health, the device error counters)
    let scrubbing = move || {
        vol.with(|v| v.as_ref().and_then(|v| v.fs.as_ref()).and_then(|f| f.scrub.as_ref()).is_some_and(|x| x.status == ScrubState::Running))
            || crate::tasks::tasks().list.with(|l| l.iter().any(|t| t.kind.strip_prefix("storage.") == Some("scrub") && !t.status.is_finished()))
    };
    crate::live::poll_while(SCRUB_POLL_MS, scrubbing, move || s.reload_fss());
    let was_scrubbing = StoredValue::new(false);
    Effect::new(move |_| {
        let now = scrubbing();
        if was_scrubbing.get_value() && !now {
            s.load();
        }
        was_scrubbing.set_value(now);
    });
    view! {
        <div class="page">
            <Show when=move || s.ready() fallback=move || loading(s, 8)>
                {move || match vol.get() {
                    None => view! {
                        <div class="card soon">
                            <EmptyState icon=icons::DATABASE title=Signal::derive(|| t("sto.vol_missing").to_string()) text=Signal::derive(|| t("sto.vol_missing_text").to_string())>
                                <a class="btn ghost" href="/storage">{tr("sto.back_storage")}</a>
                            </EmptyState>
                        </div>
                    }
                    .into_any(),
                    Some(v) => volume_view(v, modal, selected),
                }}
                {move || stable.get().map(|st| view! {
                    <div class="grid vol-detail">
                        {st.encrypted.then(|| crypt_card(&st.id, st.locked, st.managed, modal))}
                        {st.raid56.then(|| view! { <StripeCache id=st.id.clone() /> })}
                        {st.subvolumes.then(|| view! { <Subvolumes id=st.id.clone() /> })}
                        {st.btrfs_boot.clone().map(|uuid| view! { <super::btrfs::DegradedSetting uuid=uuid /> })}
                    </div>
                })}
            </Show>
            {move || match modal.get() {
                Open::None => ().into_any(),
                Open::Scrub => view! { <ScrubDialog vol=current() on_close=close /> }.into_any(),
                Open::Grow => view! { <GrowDialog vol=current() on_close=close /> }.into_any(),
                Open::Replace => view! { <ReplaceDialog vol=current() on_close=close /> }.into_any(),
                Open::Complete => view! { <CompleteDialog vol=current() on_close=close /> }.into_any(),
                Open::Unlock => view! { <UnlockDialog id=current().id on_close=close /> }.into_any(),
                Open::Lock => view! { <LockDialog id=current().id on_close=close /> }.into_any(),
                Open::Reveal => view! { <RevealDialog id=current().id on_close=close /> }.into_any(),
                Open::Btrfs(what) => view! { <super::btrfs::RepairDialog vol=current() what=what on_close=close /> }.into_any(),
                Open::ResetErrors => view! { <super::btrfs::ResetErrorsDialog vol=current() on_close=close /> }.into_any(),
            }}
            {move || selected.get().map(|name| view! { <super::disks::DiskDrawer name=name on_close=Callback::new(move |_| selected.set(None)) /> })}
        </div>
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    None,
    Scrub,
    Grow,
    Replace,
    /// Complete the interrupted growth of a flexible RAID.
    Complete,
    Unlock,
    Lock,
    Reveal,
    /// A repair of a btrfs volume of several devices.
    Btrfs(super::btrfs::Repair),
    /// Reset the device error counters of the btrfs volume.
    ResetErrors,
}

fn volume_view(vol: Vol, modal: RwSignal<Open>, selected: RwSignal<Option<String>>) -> AnyView {
    let s = super::sto();
    let (tone, key) = health(vol.health);
    let level = vol.level_text();
    let sub = [
        vol.mountpoint.clone(),
        // a non-breaking hyphen: "1‑drive tolerance" stays together on a narrow screen
        level.clone().map(|l| l.replace('-', "\u{2011}")),
        (vol.size > 0).then(|| if vol.locked { i18n::bytes_si(vol.size) } else { i18n::bytes(vol.size) }),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let actions = vol.managed && !vol.locked;
    let has_md = vol.md.is_some();
    let btn = move |m: Open, icon: &'static str, key: &'static str| {
        view! { <button class="btn ghost" type="button" on:click=move |_| modal.set(m)><Icon icon=icon size=16 /><span class="btn-label">{tr(key)}</span></button> }
    };
    let flex = vol.flex.clone().filter(|_| !vol.locked);
    let pending = flex.as_ref().is_some_and(|f| !f.pending.is_empty()) && actions;
    let blockers = flex.as_ref().map(blocker_texts).unwrap_or_default();
    // a btrfs volume left unmounted by a missing device: no usage, no scrub
    let waiting = vol.fs.as_ref().and_then(|f| f.repair.as_ref()).is_some_and(|r| !r.mounted);
    view! {
        <header class="page-head">
            <a class="icon-btn" href="/storage" aria-label=tr("sto.back_storage")><Icon icon=icons::ARROW_LEFT size=18 /></a>
            <div class="page-icon"><Icon icon=if vol.locked { icons::KEY } else { icons::DATABASE } size=20 /></div>
            <div class="grow">
                <h1 class="head-line">{vol.name.clone()}<Status tone=tone text=t(key).to_string() /></h1>
                // each part wraps as a whole ("1-drive tolerance" never split at its hyphen)
                <p class="page-sub sub-parts">{sub.into_iter().map(|x| view! { <span>{x}</span> }).collect_view()}</p>
            </div>
            {actions.then(|| view! {
                <div class="page-actions">
                    {btn(Open::Scrub, icons::SHIELD_CHECK, "sto.scrub")}
                    {has_md.then(|| btn(Open::Grow, icons::PLUS, "sto.grow"))}
                    {has_md.then(|| btn(Open::Replace, icons::REFRESH_CW, "sto.replace"))}
                </div>
            })}
        </header>
        {stale_notice(s)}
        // a btrfs volume of several devices made elsewhere still gets its repairs (devices card)
        {(!vol.managed && !vol.fs.as_ref().is_some_and(super::btrfs::multi_device)).then(|| view! { {ui::note(Tone::Info, icons::INFO, view! { {tr("sto.unmanaged")} }.into_any())} })}
        {pending.then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! {
            <span>{tr("sto.flex_pending")}</span>
            <button class="btn primary sm row-end" type="button" on:click=move |_| modal.set(Open::Complete)>{tr("sto.complete")}</button>
        }.into_any()))}
        {(!blockers.is_empty()).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! {
            <span>
                {tr("sto.flex_blocked")}" "
                {blockers.into_iter().enumerate().map(|(i, (text, en))| view! {
                    {(i > 0).then_some("; ")}
                    {if en { view! { <span lang="en">{text}</span> }.into_any() } else { view! { <span>{text}</span> }.into_any() }}
                }).collect_view()}
            </span>
        }.into_any()))}
        <div class="grid vol-detail">
            {(!vol.locked && !waiting).then(|| usage_card(&vol))}
            {match flex {
                Some(f) => flex_card(f, vol.disks.clone(), selected),
                None => vol.md.clone().map(|a| raid_card(a, selected)).into_any(),
            }}
            {vol.fs.as_ref().filter(|f| super::btrfs::multi_device(f)).map(|_| {
                super::btrfs::devices_card(&vol, Callback::new(move |r| modal.set(if r == super::btrfs::Repair::Scrub { Open::Scrub } else { Open::Btrfs(r) })), selected)
            })}
            {(!vol.locked && !waiting && vol.fs.is_some()).then(|| scrub_card(&vol, modal))}
            {(!vol.locked).then(|| super::btrfs::errors_card(&vol, Callback::new(move |_| modal.set(Open::ResetErrors)), selected))}
            {(!vol.locked && vol.mountpoint.is_some()).then(|| view! { <super::dedup::DedupCard key=vol.id.clone() /> })}
        </div>
    }
    .into_any()
}

/// Why rynas cannot change a flexible RAID, translated from the codes: (text, English). A
/// server without codes, or a code this build has no text for, gives its English text.
fn blocker_texts(f: &FlexLayout) -> Vec<(String, bool)> {
    let coded = f.blocker_items.len() == f.blockers.len();
    f.blockers
        .iter()
        .enumerate()
        .map(|(i, en)| match f.blocker_items.get(i).filter(|_| coded).and_then(blocker_text) {
            Some(t) => (t, false),
            None => (en.clone(), true),
        })
        .collect()
}

fn blocker_text(b: &FlexBlocker) -> Option<String> {
    let code = serde_json::to_string(&b.kind).ok()?.trim_matches('"').to_string();
    let mut s = i18n::try_t(&format!("sto.block.{code}"))?.to_string();
    for (k, v) in [("{subject}", &b.subject), ("{other}", &b.other)] {
        if s.contains(k) {
            s = s.replace(k, v.as_deref()?);
        }
    }
    Some(s)
}

/// A flexible RAID: level and size, the drive map, every tier with its health and sync, the
/// drives (each opens its drawer).
fn flex_card(f: FlexLayout, disks: Vec<String>, selected: RwSignal<Option<String>>) -> AnyView {
    let s = super::sto();
    let (tone, key) = array_health(f.health());
    let level = t(f.level().map(level_key).unwrap_or("sto.flex")).to_string();
    let unused: u64 = f.unused.iter().map(|(_, b)| *b).sum();
    let tiers_n = f.tiers.len() as u64;
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::LAYERS title="sto.raid">
                {f.stale.then(|| view! { <span class="chip" title=t("sto.stale_hint")><Icon icon=icons::MOON_STAR size=12 />{tr("sto.stale")}</span> })}
                <Status tone=tone text=t(key).to_string() />
            </WidgetHead>
            <dl class="kvs">
                {kv_row("sto.level", sig(level), "")}
                {ui::kv_text("plan.tolerance", tolerance_text(f.tolerance), "")}
                {ui::kv_text("sto.tiers_label", i18n::tn("sto.tiers", tiers_n, &[]), "tnum")}
                {(f.size > 0).then(|| kv_row("sto.size", sig(i18n::bytes_si(f.size)), "tnum"))}
                {(unused > 0).then(|| kv_row("sto.unused_space", sig(i18n::bytes_si(unused)), "tnum"))}
                {kv_row("sto.lvm", sig(format!("{}/{}", f.vg, f.lv)), "mono")}
            </dl>
            {tier_map(&f.tiers, Some(&f.unused), &disks)}
            {tier_list(&f.tiers, true)}
            <div class="mini-title">{tr("sto.disks")}</div>
            <ul class="members-list">
                {disks.into_iter().map(|d| {
                    let info = untrack(|| s.disk(&d));
                    let d2 = d.clone();
                    view! {
                        <li>
                            <button class="cell-btn" type="button" on:click=move |_| selected.set(Some(d2.clone()))>
                                <span class="mono strong">{d.clone()}</span>
                                <span class="muted small ellipsis">{info.map(|x| disk_title(&x)).unwrap_or_default()}</span>
                            </button>
                        </li>
                    }
                }).collect_view()}
            </ul>
        </section>
    }
    .into_any()
}

fn sig(v: String) -> Signal<String> {
    Signal::derive(move || v.clone())
}

fn usage_card(v: &Vol) -> AnyView {
    let pct = pct_of(v.used, v.size);
    let tone = usage_tone(pct);
    let fs = v.fs.clone().unwrap_or_default();
    let free = if v.fs.is_some() { fs.free_estimated } else { v.size.saturating_sub(v.used) };
    let profiles = fs
        .profiles
        .iter()
        .filter(|p| p.kind != "system")
        .map(|p| format!("{} {}", i18n::try_t(&format!("sto.prof.{}", p.kind)).unwrap_or(&p.kind), p.profile.to_uppercase()))
        .collect::<Vec<_>>()
        .join(" · ");
    let quota = match fs.quota {
        rynas_api_types::storage::QuotaMode::None => t("sto.off").to_string(),
        rynas_api_types::storage::QuotaMode::Qgroup => "qgroups".into(),
        rynas_api_types::storage::QuotaMode::Squota => "squota".into(),
        rynas_api_types::storage::QuotaMode::Unknown => ui::DASH.into(),
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::DATABASE title="sto.usage" />
            {(v.size > 0 && v.size >= v.used).then(|| view! {
                <div class="kpi"><span class="kpi-num tnum">{i18n::pct(pct)}</span><span class="kpi-unit">{tf("sto.of", &[("n", &i18n::bytes(v.size))])}</span></div>
                <Meter pct=pct tone=Signal::derive(move || tone) />
            })}
            <dl class="kvs">
                {kv_row("sto.used", sig(i18n::bytes(v.used)), "tnum")}
                {kv_row("sto.free", sig(if v.size == 0 { ui::DASH.into() } else { i18n::bytes(free) }), "tnum")}
                {kv_row("sto.fs", sig(v.fstype.clone()), "")}
                {(!profiles.is_empty()).then(|| kv_row("sto.profiles", sig(profiles), ""))}
                {kv_row("sto.compression", sig(fs.compression.clone().unwrap_or_else(|| t("sto.off").to_string())), "mono")}
                {v.fs.is_some().then(|| kv_row("sto.quota", sig(quota), ""))}
                {kv_row("sto.mount", sig(v.mountpoint.clone().unwrap_or_else(|| ui::DASH.into())), "mono")}
            </dl>
        </section>
    }
    .into_any()
}

fn raid_card(a: MdArray, selected: RwSignal<Option<String>>) -> AnyView {
    let (tone, key) = array_health(a.health);
    let s = super::sto();
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::LAYERS title="sto.raid"><Status tone=tone text=t(key).to_string() /></WidgetHead>
            <dl class="kvs">
                {kv_row("sto.level", sig(md_level_text(&a)), "")}
                {kv_row("sto.md_state", sig(a.state.clone()), "")}
                {kv_row("sto.array", sig(a.md_name.clone().map(|n| format!("{} ({n})", a.name)).unwrap_or_else(|| a.name.clone())), "mono")}
                {kv_row("sto.size", sig(i18n::bytes_si(a.size)), "tnum")}
                {a.mismatch_cnt.map(|m| kv_row("sto.mismatch", sig(m.to_string()), "tnum"))}
            </dl>
            {a.sync.as_ref().map(sync_row)}
            <ul class="members-list">
                {a.members.iter().map(|m| {
                    let (rtone, rkey) = match m.role {
                        MemberRole::Active => (Tone::Ok, "sto.r.active"),
                        MemberRole::Spare => (Tone::Info, "sto.r.spare"),
                        MemberRole::Faulty => (Tone::Err, "sto.r.faulty"),
                        MemberRole::Rebuilding => (Tone::Info, "sto.r.rebuilding"),
                        MemberRole::Journal => (Tone::Info, "sto.r.journal"),
                        MemberRole::Unknown => (Tone::Unknown, "health.unknown"),
                    };
                    let disk = m.disk.clone().unwrap_or_else(|| m.device.clone());
                    let info = s.disk(&disk);
                    let d2 = disk.clone();
                    view! {
                        <li>
                            <button class="cell-btn" type="button" on:click=move |_| selected.set(Some(d2.clone()))>
                                <span class="mono strong">{m.device.clone()}</span>
                                <span class="muted small ellipsis">{info.map(|d| disk_title(&d)).unwrap_or(disk)}</span>
                            </button>
                            <span class="row-end"><Status tone=rtone text=t(rkey).to_string() /></span>
                        </li>
                    }
                }).collect_view()}
                {(a.degraded > 0).then(|| view! {
                    <li class="warn tone-text small"><Icon icon=icons::TRIANGLE_ALERT size=14 />{tf("sto.missing_members", &[("n", &a.degraded.to_string())])}</li>
                })}
            </ul>
        </section>
    }
    .into_any()
}

fn scrub_card(v: &Vol, modal: RwSignal<Open>) -> AnyView {
    // "never" carries btrfs's totals only: nothing to show but the state
    let sc = v.fs.as_ref().and_then(|f| f.scrub.clone()).filter(|s| s.status != ScrubState::Never);
    let running = sc.as_ref().is_some_and(|s| s.status == ScrubState::Running);
    let id = v.id.clone();
    let managed = v.managed;
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let stop = move |_| {
        start_task(vol_url(&id, "scrub"), Some(json!({"action": "stop", "md_check": false, "dry_run": false})), false, stage, busy, error);
    };
    let (tone, text) = match &sc {
        Some(s) => scrub_state(s),
        None => (Tone::Unknown, t("never_run").to_string()),
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SHIELD_CHECK title="sto.scrub"><Status tone=tone text=text /></WidgetHead>
            <p class="muted small">{tr("sto.scrub_text")}</p>
            {sc.map(|s| {
                let kinds = scrub_kinds(&s);
                view! {
                    <dl class="kvs">
                        {kv_row("sto.started", sig(s.started_ms.map(i18n::datetime).unwrap_or_else(|| ui::DASH.into())), "tnum")}
                        {kv_row("sto.duration", sig(s.duration_s.map(i18n::duration).unwrap_or_else(|| ui::DASH.into())), "tnum")}
                        {if running {
                            let pct = s.pct.map(|p| i18n::pct(p as f64)).unwrap_or_else(|| ui::DASH.into());
                            let done = match (s.scrubbed_bytes, s.total_bytes) {
                                (Some(d), Some(t)) => format!("{pct} · {}", i18n::bytes_of(d, t)),
                                _ => pct,
                            };
                            kv_row("sto.progress", sig(done), "tnum").into_any()
                        } else {
                            s.total_bytes.map(|b| kv_row("sto.scrub_checked", sig(i18n::bytes(b)), "tnum")).into_any()
                        }}
                        {kv_row("sto.scrub_result", sig(scrub_result(&s)), "")}
                        {(!kinds.is_empty()).then(|| kv_row("sto.scrub_kinds", sig(kinds), ""))}
                    </dl>
                }
            })}
            {managed.then(|| view! {
                <div class="card-actions">
                    {if running {
                        view! { <button class="btn ghost sm" type="button" disabled=move || busy.get() on:click=stop>{spinner(busy)}{tr("sto.scrub_stop")}</button> }.into_any()
                    } else {
                        view! { <button class="btn ghost sm" type="button" on:click=move |_| modal.set(Open::Scrub)><Icon icon=icons::PLAY size=14 />{tr("run_now")}</button> }.into_any()
                    }}
                </div>
            })}
            {form_error(error)}
            {move || match stage.get() {
                Stage::Running(id) => view! { <TaskRun id=id on_done=Callback::new(move |_| { super::sto().load(); stage.set(Stage::Form); }) /> }.into_any(),
                _ => ().into_any(),
            }}
        </section>
    }
    .into_any()
}

/// The state chip of a scrub that ran: errors that were all corrected are a warning (the data
/// is good again), errors left uncorrected an error.
pub fn scrub_state(s: &ScrubStatus) -> (Tone, String) {
    match s.status {
        ScrubState::Running => (Tone::Info, t("sto.scrub_running").to_string()),
        ScrubState::Aborted | ScrubState::Interrupted => (Tone::Warn, t("sto.scrub_aborted").to_string()),
        ScrubState::Finished if s.errors == 0 => (Tone::Ok, t("sto.scrub_finished").to_string()),
        ScrubState::Finished if s.all_corrected() => (Tone::Warn, i18n::tn("sto.scrub_corrected", s.errors, &[])),
        ScrubState::Finished => match s.uncorrectable.filter(|n| *n > 0) {
            Some(n) => (Tone::Err, i18n::tn("sto.scrub_uncorrectable", n, &[])),
            None => (Tone::Err, t("sto.scrub_errors").to_string()),
        },
        ScrubState::Never => (Tone::Unknown, t("never_run").to_string()),
        ScrubState::Unknown => (Tone::Unknown, t("health.unknown").to_string()),
    }
}

/// What the scrub found, in words: no errors, all corrected, or how many are left.
fn scrub_result(s: &ScrubStatus) -> String {
    if s.errors == 0 {
        return t("sto.scrub_no_errors").to_string();
    }
    if s.all_corrected() {
        return i18n::tn("sto.scrub_res_corrected", s.errors, &[]);
    }
    match (s.uncorrectable, s.corrected) {
        (Some(bad), Some(fixed)) => i18n::tn("sto.scrub_res_left", bad, &[("fixed", &i18n::count(fixed))]),
        _ => i18n::tn("sto.scrub_res_errors", s.errors, &[]),
    }
}

/// The errors by kind ("Checksum: 3, Read: 1"); a kind this build has no name for keeps
/// btrfs's word.
fn scrub_kinds(s: &ScrubStatus) -> String {
    s.error_kinds
        .iter()
        .map(|k| {
            let name = i18n::try_t(&format!("sto.scrub_kind.{}", k.kind)).map_or_else(|| k.kind.clone(), str::to_string);
            tf("sto.scrub_kind_n", &[("kind", &name), ("n", &i18n::count(k.count))])
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn crypt_card(id: &str, locked: bool, managed: bool, modal: RwSignal<Open>) -> AnyView {
    let id = StoredValue::new(id.to_string());
    let busy = RwSignal::new(false);
    let msg = RwSignal::new(None::<(Tone, String)>);
    let downloaded = RwSignal::new(false);
    let download = move |_| {
        busy.set(true);
        msg.set(None);
        spawn_local(async move {
            let id = id.get_value();
            let r = api::download(&vol_url(&id, "luks-header"), &format!("luks-header-{id}.img")).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => {
                    let _ = downloaded.try_set(true);
                    let _ = msg.try_set(Some((Tone::Ok, t("sto.hdr_saved").to_string())));
                }
                Err(e) if e.status() == 404 => {
                    let _ = msg.try_set(Some((Tone::Info, t("sto.hdr_none").to_string())));
                }
                Err(e) => {
                    let _ = msg.try_set(Some((Tone::Err, e.text())));
                }
            }
        });
    };
    let delete = move |_| {
        busy.set(true);
        spawn_local(async move {
            let r = api::delete_empty(&vol_url(&id.get_value(), "luks-header")).await;
            let _ = busy.try_set(false);
            let _ = downloaded.try_set(false);
            let _ = msg.try_set(Some(match r {
                Ok(()) => (Tone::Ok, t("sto.hdr_deleted").to_string()),
                Err(e) => (Tone::Err, e.text()),
            }));
        });
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::KEY title="sto.encryption">
                {if locked {
                    view! { <Status tone=Tone::Info text=t("sto.locked").to_string() /> }.into_any()
                } else {
                    view! { <Status tone=Tone::Ok text=t("sto.unlocked").to_string() /> }.into_any()
                }}
            </WidgetHead>
            <p class="muted small">{if locked { t("sto.locked_text") } else { t("sto.crypt_text") }}</p>
            {managed.then(|| view! {
                <div class="card-actions">
                    {if locked {
                        view! { <button class="btn primary sm" type="button" on:click=move |_| modal.set(Open::Unlock)><Icon icon=icons::KEY size=14 />{tr("sto.unlock")}</button> }.into_any()
                    } else {
                        view! { <button class="btn ghost sm" type="button" on:click=move |_| modal.set(Open::Lock)>{tr("sto.lock")}</button> }.into_any()
                    }}
                    <button class="btn ghost sm" type="button" on:click=move |_| modal.set(Open::Reveal)><Icon icon=icons::KEY size=14 />{tr("sto.rk_reveal")}</button>
                    <button class="btn ghost sm" type="button" disabled=move || busy.get() on:click=download><Icon icon=icons::DOWNLOAD size=14 />{tr("sto.hdr_download")}</button>
                    {move || downloaded.get().then(|| view! {
                        <button class="btn ghost sm" type="button" disabled=move || busy.get() on:click=delete>{tr("sto.hdr_delete")}</button>
                    })}
                </div>
            })}
            <Notice msg=msg />
        </section>
    }
    .into_any()
}

// ------------------------------------------------------------------ dialogs

#[component]
pub fn UnlockDialog(id: String, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let pass = RwSignal::new(String::new());
    let url = StoredValue::new(vol_url(&id, "unlock"));
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let p = pass.get_untracked();
        if p.is_empty() {
            return;
        }
        start_task(url.get_value(), Some(json!({ "passphrase": p })), false, stage, busy, error);
        pass.set(String::new());
    };
    let title = tf("sto.unlock_title", &[("name", &id)]);
    let flow = Flow::new(stage, None, false, "sto.unlock", busy, error, Callback::new(|_| {}), on_close);
    const FORM: &str = "sto-unlock";
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar {
            can_save: Signal::derive(move || !pass.with(String::is_empty)),
            form: Some(FORM),
            ..SaveBar::new(busy, "sto.unlock", on_close, Callback::new(|_| {}))
        }),
        _ => flow.foot(),
    };
    view! {
        <Modal title=title on_close=on_close foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    <form class="form" id=FORM on:submit=submit>
                        <p class="muted small">{tr("sto.unlock_text")}</p>
                        <label class="field">
                            <span class="field-label">{tr("sto.pass_or_key")}</span>
                            <input class="input" type="password" autocomplete="off" prop:value=move || pass.get() on:input=move |ev| pass.set(event_target_value(&ev)) />
                        </label>
                        {form_error(error)}
                    </form>
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}

#[component]
fn LockDialog(id: String, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let url = StoredValue::new(vol_url(&id, "lock"));
    let flow = Flow::new(stage, None, false, "sto.lock", busy, error, Callback::new(|_| {}), on_close);
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar::new(busy, "sto.lock", on_close, Callback::new(move |_| start_task(url.get_value(), None, false, stage, busy, error)))),
        _ => flow.foot(),
    };
    view! {
        <Modal title=tf("sto.lock_title", &[("name", &id)]) on_close=on_close foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p>{tr("sto.lock_text")}</p>
                    {form_error(error)}
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}


/// The recovery key of a volume created encrypted: handed out once by rynasd (kept in memory
/// for one hour after the creation). Asking for it needs the typed volume name.
#[component]
pub fn RevealDialog(id: String, on_close: Callback<()>) -> impl IntoView {
    let typed = RwSignal::new(String::new());
    let key = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let idv = StoredValue::new(id.clone());
    let reveal = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<(), RecoveryKey>(&vol_url(&idv.get_value(), "reveal-recovery-key"), None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(k) => {
                    let _ = key.try_set(Some(k.key));
                }
                Err(e) if e.status() == 404 => {
                    let _ = error.try_set(Some(t("sto.rk_gone").to_string()));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    // the key must not linger in memory after the dialog
    on_cleanup(move || {
        let _ = key.try_set(None);
    });
    let stored = RwSignal::new(false);
    let shown = Memo::new(move |_| key.with(Option::is_some));
    let foot = move || {
        if shown.get() {
            key_once_foot(stored, on_close)
        } else {
            save_bar_with(SaveBar {
                can_save: Signal::derive(move || idv.with_value(|i| typed.get().trim() == i)),
                ..SaveBar::new(busy, "sto.rk_reveal", on_close, Callback::new(reveal))
            })
        }
    };
    view! {
        <Modal title=tf("sto.rk_title", &[("name", &id)]) on_close=on_close foot=foot>
            {move || match key.get() {
                None => view! {
                    <p class="muted small">{tr("sto.rk_text")}</p>
                    {typed_confirm(idv.get_value(), typed)}
                    {form_error(error)}
                }
                .into_any(),
                Some(k) => key_once(k, format!("recovery-key-{}.txt", idv.get_value()), "sto.rk_once", stored),
            }}
        </Modal>
    }
}

/// A recovery key shown once: copy, download as `file`, and the "I stored it" check (`stored`)
/// that [`key_once_foot`] waits for (also for an encrypted shared folder). `warn`: the i18n key
/// of the warning above the key.
pub fn key_once(key: String, file: String, warn: &'static str, stored: RwSignal<bool>) -> AnyView {
    let k2 = key.clone();
    let k3 = key.clone();
    view! {
        {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr(warn)} }.into_any())}
        <code class="secret mono rk">{key}</code>
        <div class="card-actions">
            {ui::copy_button("sto.copy", k2)}
            <button class="btn ghost sm" type="button" on:click=move |_| api::save_text(&file, &k3)>
                <Icon icon=icons::DOWNLOAD size=14 />{tr("sto.download")}
            </button>
        </div>
        <label class="check"><input type="checkbox" prop:checked=move || stored.get() on:change=move |ev| stored.set(event_target_checked(&ev)) />{tr("sto.rk_stored")}</label>
    }
    .into_any()
}

/// The action row of [`key_once`]: Done, once "I stored it" is ticked.
pub fn key_once_foot(stored: RwSignal<bool>, on_done: Callback<()>) -> AnyView {
    view! { <button class="btn primary" type="button" disabled=move || !stored.get() on:click=move |_| on_done.run(())>{tr("sto.done")}</button> }.into_any()
}

#[component]
fn ScrubDialog(vol: Vol, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let md_check = RwSignal::new(vol.md.is_some());
    let has_md = vol.md.is_some();
    let uuid = vol.fs.as_ref().map(|f| f.uuid.clone()).unwrap_or_default();
    let exec_url = StoredValue::new(vol_url(&vol.id, "scrub"));
    let exec = move |_: (String, Vec<String>)| {
        start_task(exec_url.get_value(), Some(json!({"action": "start", "md_check": md_check.get_untracked(), "dry_run": false})), false, stage, busy, error)
    };
    let uuid = StoredValue::new(uuid);
    let flow = Flow::new(stage, None, false, "sto.scrub_start", busy, error, Callback::new(exec), on_close);
    let review = move |_| fetch_plan("/api/v1/storage/plan/scrub", json!({"filesystem": uuid.get_value(), "md_check": md_check.get_untracked()}), stage, busy, error);
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar::new(busy, "sto.review_plan", on_close, Callback::new(review))),
        _ => flow.foot(),
    };
    view! {
        <Modal title=tf("sto.scrub_title", &[("name", &vol.id)]) on_close=on_close foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p class="muted small">{tr("sto.scrub_text")}</p>
                    {has_md.then(|| view! {
                        <label class="check"><input type="checkbox" prop:checked=move || md_check.get() on:change=move |ev| md_check.set(event_target_checked(&ev)) />{tr("sto.md_check")}</label>
                    })}
                    {form_error(error)}
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}

/// Free disks as a checklist (grow) — shared look with the wizard.
pub fn disk_checklist(disks: Vec<rynas_api_types::storage::DiskSummary>, chosen: RwSignal<Vec<String>>) -> AnyView {
    if disks.is_empty() {
        return ui::empty_line("sto.no_free");
    }
    view! {
        <ul class="pick-list">
            {disks.into_iter().map(|d| {
                let name = d.name.clone();
                let n2 = name.clone();
                let (htone, hkey) = super::disk_health(&d);
                view! {
                    <li>
                        <label class="pick">
                            <input type="checkbox"
                                prop:checked=move || chosen.with(|c| c.contains(&name))
                                on:change=move |ev| {
                                    let on = event_target_checked(&ev);
                                    chosen.update(|c| { c.retain(|x| x != &n2); if on { c.push(n2.clone()); } });
                                } />
                            <span class="pick-main">
                                <span class="strong">{disk_title(&d)}</span>
                                <span class="muted small mono">{d.serial.clone()}</span>
                            </span>
                            {d.smr.then(|| view! { <span class="chip warn" title=t("sto.smr_hint")>"SMR"</span> })}
                            {(!d.used_by.is_empty()).then(|| {
                                let what = d.used_by.join(", ");
                                view! { <span class="chip warn" title=tf("wiz.will_erase", &[("what", &what)])>{tr("wiz.erase_chip")}</span> }
                            })}
                            <Status tone=htone text=t(hkey).to_string() />
                        </label>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

fn serials_for(s: Sto, names: &[String]) -> Vec<String> {
    names.iter().map(|n| s.disk(n).map(|d| serial_of(&d)).unwrap_or_else(|| n.clone())).collect()
}

const LEVELS: [RaidLevel; 6] = [RaidLevel::Single, RaidLevel::Raid1, RaidLevel::Raid5, RaidLevel::Raid6, RaidLevel::Raid10, RaidLevel::Raid0];

fn level_str(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Single => "single",
        RaidLevel::Raid0 => "raid0",
        RaidLevel::Raid1 => "raid1",
        RaidLevel::Raid5 => "raid5",
        RaidLevel::Raid6 => "raid6",
        RaidLevel::Raid10 => "raid10",
        RaidLevel::Flex1 => "flex1",
        RaidLevel::Flex2 => "flex2",
    }
}

pub fn level_of(s: &str) -> Option<RaidLevel> {
    LEVELS.into_iter().find(|l| level_str(*l) == s)
}

pub fn level_json(l: RaidLevel) -> Value {
    Value::String(level_str(l).into())
}

#[component]
fn GrowDialog(vol: Vol, on_close: Callback<()>) -> impl IntoView {
    let s = super::sto();
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let chosen = RwSignal::new(Vec::<String>::new());
    let level = RwSignal::new(String::new());
    let md = vol.md.clone().unwrap_or_default();
    let current = md_level(&md);
    let array = StoredValue::new(md.name.clone());
    let exec_url = StoredValue::new(vol_url(&vol.id, "grow"));
    let id = StoredValue::new(vol.id.clone());
    let lvl = move || level_of(&level.get_untracked());
    let exec = move |(pass, others): (String, Vec<String>)| {
        let disks = chosen.get_untracked();
        // the new disks' serials, then those of any other disk the plan erases
        let mut serials = serials_for(s, &disks);
        serials.extend(others);
        let mut body = json!({
            "disks": disks, "confirm": id.get_value(), "serials": serials, "dry_run": false,
        });
        if let Some(l) = lvl() {
            body["level"] = level_json(l);
        }
        if !pass.is_empty() {
            body["passphrase"] = Value::String(pass);
        }
        start_task(exec_url.get_value(), Some(body), false, stage, busy, error);
    };
    // a flexible RAID keeps its tolerance: the drives join its tiers, no level to choose
    let flex = vol.flex.is_some();
    let review = move |_| {
        let mut body = if flex {
            json!({ "array": "", "volume": id.get_value(), "disks": chosen.get_untracked() })
        } else {
            json!({ "array": array.get_value(), "disks": chosen.get_untracked() })
        };
        if let Some(l) = lvl() {
            body["level"] = level_json(l);
        }
        fetch_plan("/api/v1/storage/plan/grow", body, stage, busy, error);
    };
    let encrypted = vol.encrypted;
    let flow = Flow::new(stage, Some(id.get_value()), encrypted, "sto.grow", busy, error, Callback::new(exec), on_close)
        .with_own(chosen.into());
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar {
            can_save: Signal::derive(move || !chosen.with(Vec::is_empty)),
            ..SaveBar::new(busy, "sto.review_plan", on_close, Callback::new(review))
        }),
        _ => flow.foot(),
    };
    view! {
        <Modal title=tf("sto.grow_title", &[("name", &vol.id)]) on_close=on_close wide=true foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p class="muted small">{tr(if flex { "sto.grow_text_flex" } else { "sto.grow_text" })}</p>
                    {disk_checklist(free_disks(s), chosen)}
                    {(!flex).then(|| view! {
                    <label class="field">
                        <span class="field-label">{tr("sto.level")}</span>
                        <select class="input" prop:value=move || level.get() on:change=move |ev| level.set(event_target_value(&ev))>
                            <option value="">{tf("sto.keep_level", &[("l", current.map(|l| t(level_key(l))).unwrap_or(ui::DASH))])}</option>
                            {LEVELS.iter().filter(|l| Some(**l) != current && **l != RaidLevel::Single).map(|l| view! {
                                <option value=level_str(*l)>{tr(level_key(*l))}</option>
                            }).collect_view()}
                        </select>
                    </label>
                    })}
                    {form_error(error)}
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}

#[component]
fn ReplaceDialog(vol: Vol, on_close: Callback<()>) -> impl IntoView {
    let s = super::sto();
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let md = vol.md.clone().unwrap_or_default();
    // what can be replaced, (value, label), the likely one first: a flexible RAID by whole
    // drive (and "missing" for a drive gone from its degraded tiers), an array by member
    let options: Vec<(String, String)> = match &vol.flex {
        Some(f) => {
            let gone: Vec<String> = f.tiers.iter().flat_map(|x| x.missing.iter().cloned()).collect();
            let mut disks = vol.disks.clone();
            disks.extend(gone.iter().cloned());
            disks.sort();
            disks.dedup();
            disks.sort_by_key(|d| !gone.contains(d));
            let mut v: Vec<(String, String)> = disks
                .into_iter()
                .map(|d| {
                    let label = untrack(|| s.disk(&d)).map(|x| disk_title(&x)).unwrap_or_else(|| d.clone());
                    let label = if gone.contains(&d) { format!("{label} · {}", t("sto.r.faulty")) } else { label };
                    (d, label)
                })
                .collect();
            if f.tiers.iter().any(|x| x.degraded as usize > x.missing.len()) {
                v.insert(0, ("missing".to_string(), t("sto.replace_missing").to_string()));
            }
            v
        }
        None => {
            let mut members = md.members.clone();
            members.sort_by_key(|m| m.role != MemberRole::Faulty);
            members
                .into_iter()
                .map(|m| {
                    let role = format!("{:?}", m.role).to_lowercase();
                    let label = format!("{} · {}", m.device, i18n::try_t(&format!("sto.r.{role}")).unwrap_or(&role));
                    (m.device, label)
                })
                .collect()
        }
    };
    let flex = vol.flex.is_some();
    let old = RwSignal::new(options.first().map(|o| o.0.clone()).unwrap_or_default());
    let new_disk = RwSignal::new(String::new());
    let array = StoredValue::new(md.name.clone());
    let exec_url = StoredValue::new(vol_url(&vol.id, "replace"));
    let id = StoredValue::new(vol.id.clone());
    let free = free_disks(s);
    if let Some(d) = free.first() {
        new_disk.set(d.name.clone());
    }
    let exec = move |(pass, others): (String, Vec<String>)| {
        let nd = new_disk.get_untracked();
        // the new disk's serial, then the old disk's when the plan clears it
        let mut serials = serials_for(s, std::slice::from_ref(&nd));
        serials.extend(others);
        let mut body = json!({"old": old.get_untracked(), "new_disk": nd, "confirm": id.get_value(), "serials": serials, "dry_run": false});
        // an encrypted flexible RAID: a bigger drive grows the encrypted device too
        if !pass.is_empty() {
            body["passphrase"] = Value::String(pass);
        }
        start_task(exec_url.get_value(), Some(body), false, stage, busy, error);
    };
    let review = move |_| {
        let body = if flex {
            json!({"array": "", "volume": id.get_value(), "old": old.get_untracked(), "new_disk": new_disk.get_untracked()})
        } else {
            json!({"array": array.get_value(), "old": old.get_untracked(), "new_disk": new_disk.get_untracked()})
        };
        fetch_plan("/api/v1/storage/plan/replace", body, stage, busy, error);
    };
    let options = StoredValue::new(options);
    let free = StoredValue::new(free);
    let flow = Flow::new(stage, Some(id.get_value()), flex && vol.encrypted, "sto.replace_go", busy, error, Callback::new(exec), on_close)
        .with_own(Signal::derive(move || vec![new_disk.get()]));
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar {
            can_save: Signal::derive(move || !new_disk.with(String::is_empty) && !old.with(String::is_empty)),
            ..SaveBar::new(busy, "sto.review_plan", on_close, Callback::new(review))
        }),
        _ => flow.foot(),
    };
    view! {
        <Modal title=tf("sto.replace_title", &[("name", &vol.id)]) on_close=on_close wide=true foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p class="muted small">{tr(if flex { "sto.replace_text_flex" } else { "sto.replace_text" })}</p>
                    <label class="field">
                        <span class="field-label">{tr("sto.replace_old")}</span>
                        <select class="input" prop:value=move || old.get() on:change=move |ev| old.set(event_target_value(&ev))>
                            {options.get_value().into_iter().map(|(v, label)| view! { <option value=v>{label}</option> }).collect_view()}
                        </select>
                    </label>
                    <label class="field">
                        <span class="field-label">{tr("sto.replace_new")}</span>
                        <select class="input" prop:value=move || new_disk.get() on:change=move |ev| new_disk.set(event_target_value(&ev))>
                            {free.get_value().into_iter().map(|d| view! { <option value=d.name.clone()>{disk_title(&d)}</option> }).collect_view()}
                        </select>
                    </label>
                    {free.with_value(Vec::is_empty).then(|| view! { <p class="muted small">{tr("sto.no_free")}</p> })}
                    {form_error(error)}
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}

/// One unfinished item of a flexible RAID's growth in words.
fn pending_text(p: &FlexPending) -> String {
    let code = serde_json::to_string(&p.kind).unwrap_or_default().trim_matches('"').to_string();
    let text = i18n::try_t(&format!("sto.pend.{code}")).unwrap_or(t("sto.pend.other"));
    text.replace("{subject}", p.subject.as_deref().unwrap_or(ui::DASH))
}

/// Complete the interrupted growth of a flexible RAID: what is left, the plan, the task.
#[component]
fn CompleteDialog(vol: Vol, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let id = StoredValue::new(vol.id.clone());
    let pending = vol.flex.as_ref().map(|f| f.pending.clone()).unwrap_or_default();
    let exec = move |(pass, _): (String, Vec<String>)| {
        let mut body = json!({"dry_run": false});
        if !pass.is_empty() {
            body["passphrase"] = Value::String(pass);
        }
        start_task(vol_url(&id.get_value(), "complete"), Some(body), false, stage, busy, error);
    };
    let review = move |_| fetch_plan("/api/v1/storage/plan/complete", json!({"volume": id.get_value()}), stage, busy, error);
    let flow = Flow::new(stage, Some(id.get_value()), vol.encrypted, "sto.complete", busy, error, Callback::new(exec), on_close);
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar::new(busy, "sto.review_plan", on_close, Callback::new(review))),
        _ => flow.foot(),
    };
    let pending = StoredValue::new(pending);
    view! {
        <Modal title=tf("sto.complete_title", &[("name", &vol.id)]) on_close=on_close wide=true foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p class="muted small">{tr("sto.complete_text")}</p>
                    <ul class="plan-warnings">
                        {pending.get_value().iter().map(|p| view! { <li class="warn"><Icon icon=icons::TRIANGLE_ALERT size=14 /><span>{pending_text(p)}</span></li> }).collect_view()}
                    </ul>
                    {form_error(error)}
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}

// ------------------------------------------------------------------ subvolumes

#[derive(Clone, PartialEq)]
enum SubModal {
    None,
    Create,
    Edit(SubvolumeInfo),
    Delete(SubvolumeInfo),
}

#[component]
fn Subvolumes(id: String) -> impl IntoView {
    let url = StoredValue::new(vol_url(&id, "subvolumes"));
    let list = RwSignal::new(None::<Result<Vec<SubvolumeInfo>, String>>);
    let modal = RwSignal::new(SubModal::None);
    let load = move || {
        spawn_local(async move {
            let r = api::get::<Vec<SubvolumeInfo>>(&url.get_value()).await;
            let _ = list.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    };
    load();
    let close = Callback::new(move |_| {
        modal.set(SubModal::None);
        load();
    });
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::FOLDER title="sto.subvolumes">
                <button class="btn ghost sm" type="button" on:click=move |_| modal.set(SubModal::Create)><Icon icon=icons::PLUS size=14 />{tr("sto.sub_create")}</button>
            </WidgetHead>
            {move || match list.get() {
                None => view! { <Skeleton lines=3 /> }.into_any(),
                Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
                Some(Ok(v)) if v.is_empty() => ui::empty_line("sto.sub_none"),
                Some(Ok(v)) => view! {
                    {ui::table("", &["sto.name", "sto.path", "sto.compression", "sto.readonly", ""], v.into_iter().map(|sv| {
                                    let comp = match sv.compression.as_deref() { None => t("sto.inherit").to_string(), Some("none") => t("sto.off").to_string(), Some(c) => c.to_string() };
                                    let e = sv.clone();
                                    let d = sv.clone();
                                    ui::row(vec![
                                        ui::cell("sto.name", "strong", view! { {sv.name.clone()}{sv.system.then(|| view! { <span class="chip">{tr("sto.system")}</span> })} }.into_any()),
                                        ui::cell("sto.path", "mono muted", view! { {sv.path.clone()} }.into_any()),
                                        ui::cell("sto.compression", "", view! { {comp} }.into_any()),
                                        ui::cell("sto.readonly", "", view! { {if sv.readonly { t("sto.yes") } else { t("sto.no") }} }.into_any()),
                                        ui::actions(view! {
                                            {(!sv.system).then(|| view! {
                                                {ui::icon_btn(icons::PENCIL, ui::verb_name("edit", &sv.name), false, Signal::stored(false), Callback::new(move |_| modal.set(SubModal::Edit(e.clone()))))}
                                                {ui::icon_btn(icons::TRASH_2, ui::verb_name("delete", &sv.name), true, Signal::stored(false), Callback::new(move |_| modal.set(SubModal::Delete(d.clone()))))}
                                            })}
                                        }
                                        .into_any()),
                                    ])
                                }).collect::<Vec<_>>())}
                }
                .into_any(),
            }}
            {move || match modal.get() {
                SubModal::None => ().into_any(),
                SubModal::Create => view! { <SubvolDialog base=url.get_value() edit=None on_close=close /> }.into_any(),
                SubModal::Edit(sv) => view! { <SubvolDialog base=url.get_value() edit=Some(sv) on_close=close /> }.into_any(),
                SubModal::Delete(sv) => view! { <SubvolDelete base=url.get_value() sub=sv on_close=close /> }.into_any(),
            }}
        </section>
    }
}

fn valid_sub(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

#[component]
fn SubvolDialog(base: String, edit: Option<SubvolumeInfo>, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let creating = edit.is_none();
    let name = RwSignal::new(edit.as_ref().map(|e| e.name.clone()).unwrap_or_default());
    let comp = RwSignal::new(edit.as_ref().is_none_or(|e| e.compression.as_deref() != Some("none")));
    let ro = RwSignal::new(edit.as_ref().is_some_and(|e| e.readonly));
    let nocow = RwSignal::new(false);
    let base = StoredValue::new(base);
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let n = name.get_untracked();
        if !valid_sub(&n) {
            error.set(Some(t("sto.sub_invalid").to_string()));
            return;
        }
        let body = json!({"name": n, "compression": comp.get_untracked(), "readonly": ro.get_untracked(), "nocow": creating && nocow.get_untracked()});
        if creating {
            start_task(base.get_value(), Some(body), false, stage, busy, error);
        } else {
            // PUT answers 202 like POST: go through the generic path with the PUT method
            busy.set(true);
            error.set(None);
            let url = format!("{}/{}", base.get_value(), api::encode(&n));
            spawn_local(async move {
                let r = api::put::<_, TaskStarted>(&url, &body).await;
                let _ = busy.try_set(false);
                match r {
                    Ok(t) => {
                        let _ = stage.try_set(Stage::Running(t.task_id));
                    }
                    Err(e) => {
                        let _ = error.try_set(Some(e.text()));
                    }
                }
            });
        }
    };
    let title = if creating { t("sto.sub_create").to_string() } else { tf("edit_named", &[("name", &name.get_untracked())]) };
    let flow = Flow::new(stage, None, false, "save", busy, error, Callback::new(|_| {}), on_close);
    const FORM: &str = "sto-subvol";
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar { form: Some(FORM), ..SaveBar::new(busy, if creating { "create" } else { "save" }, on_close, Callback::new(|_| {})) }),
        _ => flow.foot(),
    };
    view! {
        <Modal title=title on_close=on_close foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    <form class="form" id=FORM on:submit=submit>
                        <label class="field">
                            <span class="field-label">{tr("sto.name")}</span>
                            <input class="input" disabled=!creating maxlength="64" autocomplete="off" spellcheck="false"
                                prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                            <span class="muted small">{tr("sto.sub_name_hint")}</span>
                        </label>
                        // the same switch and trade-offs as a new shared folder
                        {creating.then(|| view! {
                            {crate::form::switch("shares.nocow", Some("shares.nocow_help"), nocow.into(), Callback::new(move |v| nocow.set(v)))}
                            {move || nocow.get().then(crate::pages::share_nocow::costs_note)}
                        })}
                        <div class="form" hidden=move || nocow.get()>
                            <label class="check"><input type="checkbox" prop:checked=move || comp.get() on:change=move |ev| comp.set(event_target_checked(&ev)) />{tr("sto.sub_compress")}</label>
                            <p class="muted small">{tr("sto.sub_compress_hint")}</p>
                        </div>
                        <label class="check"><input type="checkbox" prop:checked=move || ro.get() on:change=move |ev| ro.set(event_target_checked(&ev)) />{tr("sto.readonly")}</label>
                        {form_error(error)}
                    </form>
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}

#[component]
fn SubvolDelete(base: String, sub: SubvolumeInfo, on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let typed = RwSignal::new(String::new());
    let name = StoredValue::new(sub.name.clone());
    let url = StoredValue::new(format!("{base}/{0}?confirm={0}", api::encode(&sub.name)));
    let flow = Flow::new(stage, None, false, "delete", busy, error, Callback::new(|_| {}), on_close);
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar {
            can_save: Signal::derive(move || name.with_value(|n| typed.get().trim() == n)),
            danger: Signal::stored(true),
            ..SaveBar::new(busy, "delete", on_close, Callback::new(move |_| start_task(url.get_value(), None, true, stage, busy, error)))
        }),
        _ => flow.foot(),
    };
    view! {
        <Modal title=tf("sto.sub_delete", &[("name", &sub.name)]) on_close=on_close foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    {ui::note(Tone::Err, icons::TRIANGLE_ALERT, view! { {tr("sto.sub_delete_text")} }.into_any())}
                    {typed_confirm(name.get_value(), typed)}
                    {form_error(error)}
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}

// ------------------------------------------------------------------ md tuning

#[component]
fn StripeCache(id: String) -> impl IntoView {
    let url = StoredValue::new(vol_url(&id, "settings"));
    let state = RwSignal::new(None::<Result<VolumeSettingsState, String>>);
    let choice = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let fill = move |st: &VolumeSettingsState| {
        let v = st.pending.clone().unwrap_or_else(|| st.applied.clone());
        choice.set(v.stripe_cache_size.map(|n| n.to_string()).unwrap_or_default());
    };
    let load = move || {
        spawn_local(async move {
            let r = api::get::<VolumeSettingsState>(&url.get_value()).await;
            if let Ok(st) = &r {
                fill(st);
            }
            let _ = state.try_set(Some(r.map_err(|e| e.code().to_string())));
        });
    };
    load();
    let save = move |_| {
        busy.set(true);
        error.set(None);
        let body = VolumeSettings { stripe_cache_size: choice.get_untracked().parse().ok() };
        spawn_local(async move {
            let r = api::put::<_, VolumeSettingsState>(&url.get_value(), &body).await;
            match r {
                Ok(st) => {
                    fill(&st);
                    let _ = state.try_set(Some(Ok(st)));
                    // Save applies at once (guidelines A19b); read the applied state back
                    if crate::config::apply_now(&[crate::config::keys::DISKS], error).await {
                        crate::toast::saved_applied();
                        load();
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    };
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::SETTINGS title="sto.tuning" />
            {move || match state.get() {
                None => view! { <Skeleton lines=3 /> }.into_any(),
                Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
                Some(Ok(st)) => {
                    let prop = st.proposed_stripe_cache_size;
                    let mem = st.proposed_memory_bytes;
                    view! {
                        <p class="muted small">{tr("sto.stripe_text")}</p>
                        <dl class="kvs">
                            {kv_row("sto.current", sig(st.current_stripe_cache_size.map(|n| n.to_string()).unwrap_or_else(|| ui::DASH.into())), "tnum")}
                            {prop.map(|p| kv_row("sto.proposal", sig(format!("{p} · {}", mem.map(i18n::bytes).unwrap_or_default())), "tnum"))}
                        </dl>
                        {st.pending.is_some().then(|| view! { {ui::note(Tone::Info, icons::HISTORY, view! { {tr("sto.pending_note")} }.into_any())} })}
                        <label class="field">
                            <span class="field-label">{tr("sto.stripe")}</span>
                            <select class="input" prop:value=move || choice.get() on:change=move |ev| choice.set(event_target_value(&ev))>
                                <option value="">{tr("sto.kernel_default")}</option>
                                {prop.map(|p| view! { <option value=p.to_string()>{tf("sto.use_proposal", &[("n", &p.to_string())])}</option> })}
                                {[512u32, 1024, 2048, 4096, 8192].into_iter().filter(|n| Some(*n) != prop).map(|n| view! { <option value=n.to_string()>{n.to_string()}</option> }).collect_view()}
                            </select>
                        </label>
                        {form_error(error)}
                        <div class="card-actions"><button class="btn primary sm" type="button" disabled=move || busy.get() on:click=save>{spinner(busy)}{tr("save")}</button></div>
                    }
                    .into_any()
                }
            }}
        </section>
    }
}
