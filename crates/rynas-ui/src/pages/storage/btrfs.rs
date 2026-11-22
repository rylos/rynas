//! A btrfs volume of several devices (its own RAID profile, not md): the devices, what is
//! missing, and the repairs: mount it degraded when it stayed unmounted, replace the
//! missing device, remove it, convert the chunks written with a weaker profile back; plus
//! the per-volume setting of the degraded mount after boot (`docs/phase2/btrfs-degraded.md`).
//!
//! The repairs follow the storage flow: review the plan (`dry_run`), confirm, follow the
//! task. Only the replace erases a drive, so only it asks for the typed name and serial.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::storage::{
    BtrfsBlocker, BtrfsDevice, BtrfsDeviceErrors, BtrfsFilesystem, BtrfsRepair, BtrfsSettings, BtrfsSettingsState, ScrubState, StoragePlan,
};
use serde_json::{Value, json};

use super::volume::{Flow, Stage, start_task};
use super::{Vol, disk_title, free_disks};
use crate::api;
use crate::form::{SaveBar, form_error, save_bar_with, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, ErrorBox, Modal, Skeleton, Status, Tone, WidgetHead, kv_row};

/// A repair started from the devices card.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repair {
    Mount,
    Replace,
    Remove,
    Convert,
    /// The data scrub a device that came back needs (the volume page's scrub dialog).
    Scrub,
}

fn url(uuid: &str, what: &str) -> String {
    format!("/api/v1/storage/btrfs/{}/{what}", api::encode(uuid))
}

/// A btrfs filesystem of several devices, or one with something to repair.
pub fn multi_device(fs: &BtrfsFilesystem) -> bool {
    fs.devices.len() > 1 || fs.repair.is_some()
}

/// The whole disk (among the volume's `disks`) a device is or is a partition of:
/// `/dev/sdb1` → `sdb`, `/dev/nvme0n1p2` → `nvme0n1`.
pub fn disk_of(dev: &str, disks: &[String]) -> Option<String> {
    let p = dev.trim_start_matches("/dev/");
    disks
        .iter()
        .filter(|d| {
            p == d.as_str()
                || p.strip_prefix(d.as_str()).is_some_and(|rest| {
                    let n = rest.strip_prefix('p').unwrap_or(rest);
                    !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
                })
        })
        .max_by_key(|d| d.len())
        .cloned()
}

/// The text of a blocker.
fn blocker_key(b: BtrfsBlocker) -> &'static str {
    match b {
        BtrfsBlocker::Incomplete => "sto.bt.blocker.incomplete",
        BtrfsBlocker::TooFewDevices => "sto.bt.blocker.too_few_devices",
        BtrfsBlocker::NoSpace => "sto.bt.blocker.no_space",
        BtrfsBlocker::SplitBrain => "sto.bt.blocker.split_brain",
        BtrfsBlocker::Disabled => "sto.bt.blocker.disabled",
        BtrfsBlocker::NoDevices => "sto.bt.blocker.no_devices",
        BtrfsBlocker::Asleep => "sto.bt.blocker.asleep",
        BtrfsBlocker::Other => "sto.bt.blocker.other",
    }
}

/// "data raid1, metadata raid1c3": the strongest profile of each kind in use.
fn profiles(fs: &BtrfsFilesystem) -> String {
    let mut v: Vec<String> = Vec::new();
    for kind in ["data", "metadata"] {
        if let Some(p) = fs.profiles.iter().find(|p| p.kind == kind) {
            v.push(format!("{} {}", t(if kind == "data" { "sto.bt.data" } else { "sto.bt.metadata" }), p.profile));
        }
    }
    v.join(", ")
}

/// The state of the devices as (tone, key).
fn state(r: Option<&BtrfsRepair>) -> (Tone, &'static str) {
    match r {
        Some(r) if !r.mounted => (Tone::Err, "rm.not_mounted"),
        Some(r) if !r.lost.is_empty() => (Tone::Err, "health.failed"),
        Some(r) if r.degraded() => (Tone::Warn, "health.degraded"),
        Some(r) if r.scrub_needed => (Tone::Warn, "sto.h.warning"),
        Some(r) if !r.convert.is_empty() => (Tone::Warn, "sto.h.warning"),
        _ => (Tone::Ok, "health.healthy"),
    }
}

/// The devices of a multi-device btrfs volume and its repairs.
pub fn devices_card(v: &Vol, open: Callback<Repair>, selected: RwSignal<Option<String>>) -> AnyView {
    let Some(fs) = v.fs.clone() else { return ().into_any() };
    let s = super::sto();
    let r = fs.repair.clone();
    let (tone, key) = state(r.as_ref());
    let total = r.as_ref().map(|r| r.total_devices).filter(|n| *n > 0).unwrap_or(fs.devices.len() as u32);
    let present = fs.devices.iter().filter(|d| !d.missing).count();
    let btn = move |what: Repair, icon: &'static str, key: &'static str, primary: bool| {
        view! {
            <button class=if primary { "btn primary sm" } else { "btn ghost sm" } type="button" on:click=move |_| open.run(what)>
                <Icon icon=icon size=14 />{tr(key)}
            </button>
        }
    };
    let notes = r.clone().map(|r| {
        let mut out: Vec<AnyView> = Vec::new();
        if r.system && r.degraded() {
            out.push(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("sto.bt.system")} }.into_any()));
        }
        if !r.mounted {
            let text = if r.total_devices > 0 {
                tf("sto.bt.unmounted", &[("n", &r.missing_count.to_string()), ("total", &r.total_devices.to_string())])
            } else {
                t("sto.bt.unmounted_some").to_string()
            };
            let why = r.mount.filter(|b| *b != BtrfsBlocker::Disabled);
            out.push(ui::note(Tone::Err, icons::CIRCLE_X, view! {
                <span>{text}" "{match why { Some(b) => t(blocker_key(b)).to_string(), None => t("sto.bt.mount_hint").to_string() }}</span>
            }.into_any()));
        } else if !r.lost.is_empty() {
            out.push(ui::note(Tone::Err, icons::CIRCLE_X, view! { {tf("sto.bt.lost", &[("items", &r.lost.join(", "))])} }.into_any()));
        } else if r.degraded() {
            out.push(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tf("sto.bt.degraded", &[("n", &r.missing_count.to_string())])} }.into_any()));
            if let Some(b) = r.remove {
                out.push(view! { <p class="muted small">{tr("sto.bt.remove_blocked")}" "{tr(blocker_key(b))}</p> }.into_any());
            }
        } else if r.scrub_needed {
            out.push(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("sto.bt.scrub_needed")} }.into_any()));
        }
        if r.mounted && !r.degraded() && !r.convert.is_empty() {
            let items: Vec<String> = r.convert.iter().map(|c| format!("{} {}", c.kind, c.from.join("/"))).collect();
            out.push(ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tf("sto.bt.weak", &[("items", &items.join(", "))])} }.into_any()));
        }
        out.collect_view()
    });
    let actions = r.clone().map(|r| {
        let mount = !r.mounted && matches!(r.mount, None | Some(BtrfsBlocker::Disabled));
        let degraded = r.mounted && r.degraded() && r.lost.is_empty();
        let remove = degraded && r.remove.is_none();
        let convert = r.mounted && !r.degraded() && !r.convert.is_empty();
        let scrub = r.mounted && !r.degraded() && r.scrub_needed;
        (mount || degraded || convert || scrub).then(|| view! {
            <div class="card-actions">
                {mount.then(|| btn(Repair::Mount, icons::PLAY, "sto.bt.mount", true))}
                {degraded.then(|| btn(Repair::Replace, icons::REFRESH_CW, "sto.bt.replace", true))}
                {remove.then(|| btn(Repair::Remove, icons::UNPLUG, "sto.bt.remove", false))}
                {convert.then(|| btn(Repair::Convert, icons::LAYERS, "sto.bt.convert", true))}
                {scrub.then(|| btn(Repair::Scrub, icons::SHIELD_CHECK, "sto.scrub", true))}
            </div>
        })
    });
    let missing_ids: Vec<u64> = {
        let mut v: Vec<u64> = fs.devices.iter().filter(|d| d.missing).map(|d| d.devid).collect();
        if let Some(r) = &r {
            v.extend(r.missing.iter().filter(|id| !fs.devices.iter().any(|d| d.devid == **id)));
        }
        v
    };
    let unknown_missing = r.as_ref().map(|r| (r.missing_count as usize).saturating_sub(missing_ids.len())).unwrap_or(0);
    let prof = profiles(&fs);
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::LAYERS title="sto.bt.title"><Status tone=tone text=t(key).to_string() /></WidgetHead>
            <dl class="kvs">
                {(!prof.is_empty()).then(|| kv_row("sto.bt.profiles", Signal::stored(prof.clone()), ""))}
                {kv_row("sto.bt.devices", Signal::stored(tf("sto.bt.present", &[("present", &present.to_string()), ("total", &total.to_string())])), "tnum")}
            </dl>
            <ul class="members-list">
                {fs.devices.iter().filter(|d| !d.missing).map(|d| {
                    let disk = disk_of(&d.path, &v.disks);
                    let info = disk.as_ref().and_then(|n| s.disk(n));
                    let d2 = disk.clone();
                    let name = d.path.trim_start_matches("/dev/").to_string();
                    view! {
                        <li>
                            <button class="cell-btn" type="button" disabled=d2.is_none() on:click=move |_| selected.set(d2.clone())>
                                <span class="mono strong">{name}</span>
                                <span class="muted small ellipsis">{info.map(|x| disk_title(&x)).unwrap_or_else(|| tf("sto.bt.devid", &[("n", &d.devid.to_string())]))}</span>
                            </button>
                            <span class="row-end"><Status tone=Tone::Ok text=t("sto.r.active").to_string() /></span>
                        </li>
                    }
                }).collect_view()}
                {missing_ids.into_iter().map(|id| view! {
                    <li>
                        <span class="strong">{tf("sto.bt.devid", &[("n", &id.to_string())])}</span>
                        <span class="row-end"><Status tone=Tone::Err text=t("sto.bt.missing").to_string() /></span>
                    </li>
                }).collect_view()}
                {(unknown_missing > 0).then(|| view! {
                    <li class="warn tone-text small"><Icon icon=icons::TRIANGLE_ALERT size=14 />{tf("sto.missing_members", &[("n", &unknown_missing.to_string())])}</li>
                })}
            </ul>
            {notes}
            {actions}
        </section>
    }
    .into_any()
}

/// The error counters of a device (`btrfs device stats`) above zero: (i18n kind, count).
fn counters(e: &BtrfsDeviceErrors) -> Vec<(&'static str, u64)> {
    [("write", e.write_io), ("read", e.read_io), ("flush", e.flush_io), ("corruption", e.corruption), ("generation", e.generation)]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// "Generation: 2, Read: 1".
fn counters_text(c: &[(&'static str, u64)]) -> String {
    c.iter()
        .map(|(k, n)| {
            let name = i18n::try_t(&format!("sto.err.k.{k}")).unwrap_or(k);
            tf("sto.scrub_kind_n", &[("kind", name), ("n", &i18n::count(*n))])
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The devices of a btrfs volume whose error counters are above zero (what keeps the volume
/// at "Warning"), what each kind means, what the last scrub says about the data, and the
/// reset of the counters. Nothing when every counter is zero.
pub fn errors_card(v: &Vol, reset: Callback<()>, selected: RwSignal<Option<String>>) -> AnyView {
    let Some(fs) = v.fs.clone() else { return ().into_any() };
    let bad: Vec<(BtrfsDevice, Vec<(&'static str, u64)>)> =
        fs.devices.iter().filter(|d| !d.missing).map(|d| (d.clone(), counters(&d.errors))).filter(|(_, c)| !c.is_empty()).collect();
    if bad.is_empty() {
        return ().into_any();
    }
    let s = super::sto();
    let mut kinds: Vec<&'static str> = bad.iter().flat_map(|(_, c)| c.iter().map(|(k, _)| *k)).collect();
    kinds.sort_by_key(|k| ["write", "read", "flush", "corruption", "generation"].iter().position(|x| x == k));
    kinds.dedup();
    let scrub = fs.scrub.clone().filter(|x| x.status == ScrubState::Finished);
    let hint = match &scrub {
        Some(x) if x.uncorrectable.unwrap_or(0) > 0 || (x.errors > 0 && !x.all_corrected()) => "sto.err.hint_bad",
        Some(_) => "sto.err.hint_clean",
        None => "sto.err.hint_scrub",
    };
    // administrators only (rynasd checks too); btrfs cannot reset a device it does not have
    let admin = crate::auth::auth().session.with_untracked(|x| x.as_ref().is_some_and(|x| x.user.role == rynas_api_types::Role::Admin));
    let missing = fs.devices.iter().any(|d| d.missing) || fs.repair.as_ref().is_some_and(|r| r.degraded());
    let mounted = fs.mountpoint.is_some() && admin;
    view! {
        <section class="card widget w-6">
            <WidgetHead icon=icons::TRIANGLE_ALERT title="sto.err.title"><Status tone=Tone::Warn text=t("sto.err.state").to_string() /></WidgetHead>
            <p class="muted small">{tr("sto.err.text")}</p>
            <ul class="members-list">
                {bad.into_iter().map(|(d, c)| {
                    let disk = disk_of(&d.path, &v.disks);
                    let info = disk.as_ref().and_then(|n| s.disk(n));
                    let d2 = disk.clone();
                    let name = d.path.trim_start_matches("/dev/").to_string();
                    view! {
                        <li>
                            <button class="cell-btn" type="button" disabled=d2.is_none() on:click=move |_| selected.set(d2.clone())>
                                <span class="mono strong">{name}</span>
                                <span class="muted small ellipsis">{info.map(|x| disk_title(&x)).unwrap_or_else(|| tf("sto.bt.devid", &[("n", &d.devid.to_string())]))}</span>
                                <span class="warn tone-text small">{counters_text(&c)}</span>
                            </button>
                        </li>
                    }
                }).collect_view()}
            </ul>
            {kinds.into_iter().map(|k| {
                let name = i18n::try_t(&format!("sto.err.k.{k}")).unwrap_or(k);
                let what = i18n::try_t(&format!("sto.err.d.{k}")).unwrap_or("");
                view! { <p class="muted small"><strong>{name}</strong>": "{what}</p> }
            }).collect_view()}
            {ui::note(Tone::Info, icons::INFO, view! { {tr(hint)} }.into_any())}
            {(mounted && missing).then(|| view! { <p class="muted small">{tr("sto.err.missing")}</p> })}
            {(mounted && !missing).then(|| view! {
                <div class="card-actions">
                    <button class="btn ghost sm" type="button" on:click=move |_| reset.run(())><Icon icon=icons::ROTATE_CCW size=14 />{tr("sto.err.reset")}</button>
                </div>
            })}
        </section>
    }
    .into_any()
}

/// Confirms and runs the reset of the error counters of a btrfs volume (`btrfs device stats
/// -z` as a task), then reads the storage again.
#[component]
pub fn ResetErrorsDialog(vol: Vol, on_close: Callback<()>) -> impl IntoView {
    let uuid = vol.fs.as_ref().map(|f| f.uuid.clone()).unwrap_or_default();
    let c = ui::Confirm::new(
        tf("sto.err.reset_title", &[("name", &vol.name)]),
        tf("sto.err.reset_text", &[("name", &vol.name)]),
        "sto.err.reset",
        false,
        Callback::new(|_| {}),
        on_close,
    );
    let (busy, error) = (c.busy, c.error);
    let go = Callback::new(move |_| {
        let url = url(&uuid, "reset-errors");
        crate::tasks::run_and_report(
            async move { api::start_task(&url, None).await },
            t("sto.err.reset_done").to_string(),
            busy,
            error,
            Callback::new(move |_| {
                super::sto().load();
                on_close.try_run(());
            }),
        );
    });
    ui::confirm(ui::Confirm { on_confirm: go, ..c })
}

/// A repair of a btrfs volume: its form (only the replace has one), the plan, the task.
#[component]
pub fn RepairDialog(vol: Vol, what: Repair, on_close: Callback<()>) -> impl IntoView {
    let s = super::sto();
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let fs = vol.fs.clone().unwrap_or_default();
    let r = fs.repair.clone().unwrap_or_default();
    let id = StoredValue::new(vol.id.clone());
    let (path, title, text, go) = match what {
        Repair::Mount => ("mount-degraded", "sto.bt.mount_title", "sto.bt.mount_text", "sto.bt.go_mount"),
        Repair::Replace => ("replace", "sto.bt.replace_title", "sto.bt.replace_text", "sto.bt.go_replace"),
        Repair::Remove => ("remove-missing", "sto.bt.remove_title", "sto.bt.remove_text", "sto.bt.go_remove"),
        // the scrub has its own dialog on the volume page; never opened here
        Repair::Convert | Repair::Scrub => ("convert", "sto.bt.convert_title", "sto.bt.convert_text", "sto.bt.go_convert"),
    };
    let target = StoredValue::new(url(&fs.uuid, path));
    // the replace: which missing device, onto which drive
    let missing: Vec<u64> = r.missing.clone();
    let devid = RwSignal::new(missing.first().copied().unwrap_or(0));
    // no drive chosen in advance: the administrator picks it, sees what is on it, and types
    // its serial number on the plan (it is erased)
    let new_disk = RwSignal::new(String::new());
    let free = free_disks(s);
    let body = move |dry: bool, serials: Vec<String>| -> Value {
        match what {
            Repair::Replace => {
                json!({"devid": devid.get_untracked(), "new_disk": new_disk.get_untracked(), "confirm": id.get_value(), "serials": serials, "dry_run": dry})
            }
            _ => json!({"confirm": id.get_value(), "dry_run": dry}),
        }
    };
    let review = move |_| {
        busy.set(true);
        error.set(None);
        let b = body(true, Vec::new());
        spawn_local(async move {
            let r = api::post::<_, StoragePlan>(&target.get_value(), Some(&b)).await;
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
    };
    // the serial numbers typed on the plan for the drive it erases
    let exec = move |(_, typed): (String, Vec<String>)| start_task(target.get_value(), Some(body(false, typed)), false, stage, busy, error);
    let replace = what == Repair::Replace;
    // only the replace erases a drive: the typed name, and the drive's serial typed too (it
    // is not one of the form's own drives, so the plan asks for it)
    let flow = Flow::new(stage, replace.then(|| id.get_value()), false, go, busy, error, Callback::new(exec), on_close);
    let chosen = move || new_disk.with(|n| s.disk(n));
    let free = StoredValue::new(free);
    let missing = StoredValue::new(missing);
    let foot = move || match stage.get() {
        Stage::Form => save_bar_with(SaveBar {
            can_save: Signal::derive(move || !replace || (!new_disk.with(String::is_empty) && devid.get() > 0)),
            ..SaveBar::new(busy, "sto.review_plan", on_close, Callback::new(review))
        }),
        _ => flow.foot(),
    };
    let size = r.missing_bytes;
    view! {
        <Modal title=tf(title, &[("name", &vol.id)]) on_close=on_close wide=replace foot=foot>
            {move || match stage.get() {
                Stage::Form => view! {
                    <p class="muted small">{tr(text)}</p>
                    {replace.then(|| view! {
                        <label class="field">
                            <span class="field-label">{tr("sto.bt.missing_device")}</span>
                            <select class="input" prop:value=move || devid.get().to_string() on:change=move |ev| devid.set(event_target_value(&ev).parse().unwrap_or(0))>
                                // `selected` too: after Back the form is built again, the value is set before the options exist
                                {missing.get_value().into_iter().map(|d| view! { <option value=d.to_string() selected=devid.get_untracked() == d>{tf("sto.bt.devid", &[("n", &d.to_string())])}</option> }).collect_view()}
                            </select>
                        </label>
                        <label class="field">
                            <span class="field-label">{tr("sto.replace_new")}</span>
                            <select class="input" prop:value=move || new_disk.get() on:change=move |ev| new_disk.set(event_target_value(&ev))>
                                <option value="">{tr("sto.bt.choose_disk")}</option>
                                {free.get_value().into_iter().map(|d| { let sel = new_disk.with_untracked(|n| *n == d.name); view! { <option value=d.name.clone() selected=sel>{disk_title(&d)}</option> } }).collect_view()}
                            </select>
                        </label>
                        {move || chosen().map(|d| {
                            // a drive without a serial number (a virtual disk) shows none
                            let facts = [d.model.trim().to_string(), d.serial.trim().to_string(), i18n::bytes_si(d.size)]
                                .into_iter()
                                .filter(|x| !x.is_empty())
                                .collect::<Vec<_>>()
                                .join(" · ");
                            view! {
                                <p class="muted small"><span class="mono strong">{d.name.clone()}</span>" "{facts}</p>
                                {(!d.used_by.is_empty()).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! {
                                    {tf("sto.bt.disk_holds", &[("items", &d.used_by.join(", "))])}
                                }.into_any()))}
                            }
                        })}
                        {(size > 0).then(|| view! { <p class="muted small">{tf("sto.bt.min_size", &[("size", &i18n::bytes_si(size))])}</p> })}
                        {free.with_value(Vec::is_empty).then(|| view! { <p class="muted small">{tr("sto.no_free")}</p> })}
                    })}
                    {form_error(error)}
                }
                .into_any(),
                _ => flow.body(),
            }}
        </Modal>
    }
}

/// The degraded mount after boot of one volume: on (the default) or off.
#[component]
pub fn DegradedSetting(uuid: String) -> impl IntoView {
    let target = StoredValue::new(url(&uuid, "settings"));
    let state = RwSignal::new(None::<Result<BtrfsSettingsState, String>>);
    let auto = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let fill = move |st: &BtrfsSettingsState| auto.set(st.pending.clone().unwrap_or_else(|| st.applied.clone()).auto_degraded);
    let load = move || {
        // also called after a save: the page may be gone by then
        let Some(u) = target.try_get_value() else { return };
        spawn_local(async move {
            let r = api::get::<BtrfsSettingsState>(&u).await;
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
        let body = BtrfsSettings { auto_degraded: auto.get_untracked() };
        spawn_local(async move {
            match api::put::<_, BtrfsSettingsState>(&target.get_value(), &body).await {
                Ok(st) => {
                    fill(&st);
                    let _ = state.try_set(Some(Ok(st)));
                    // Save applies at once (guidelines A19b)
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
            <WidgetHead icon=icons::SETTINGS title="sto.bt.boot" />
            {move || match state.get() {
                None => view! { <Skeleton lines=2 /> }.into_any(),
                Some(Err(code)) => view! { <ErrorBox code=code on_retry=Callback::new(move |_| load()) /> }.into_any(),
                Some(Ok(st)) => view! {
                    <p class="muted small">{tr("sto.bt.auto_text")}</p>
                    {st.pending.is_some().then(|| ui::note(Tone::Info, icons::HISTORY, view! { {tr("sto.pending_note")} }.into_any()))}
                    <label class="check"><input type="checkbox" prop:checked=move || auto.get() on:change=move |ev| auto.set(event_target_checked(&ev)) />{tr("sto.bt.auto")}</label>
                    {form_error(error)}
                    <div class="card-actions"><button class="btn primary sm" type="button" disabled=move || busy.get() on:click=save>{spinner(busy)}{tr("save")}</button></div>
                }
                .into_any(),
            }}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_to_disk() {
        let disks: Vec<String> = ["sdb", "sdb1x", "nvme0n1", "loop1"].map(String::from).into();
        assert_eq!(disk_of("/dev/sdb2", &disks).as_deref(), Some("sdb"));
        assert_eq!(disk_of("/dev/nvme0n1p2", &disks).as_deref(), Some("nvme0n1"));
        assert_eq!(disk_of("/dev/loop1", &disks).as_deref(), Some("loop1"));
        assert_eq!(disk_of("/dev/sdc1", &disks), None);
        assert_eq!(disk_of("/dev/mapper/x", &disks), None);
    }

    #[test]
    fn device_states() {
        let r = |mounted, missing_count, lost: &[&str], convert: bool| BtrfsRepair {
            mounted,
            missing_count,
            lost: lost.iter().map(|s| s.to_string()).collect(),
            convert: if convert { vec![Default::default()] } else { Vec::new() },
            ..Default::default()
        };
        assert_eq!(state(None), (Tone::Ok, "health.healthy"));
        assert_eq!(state(Some(&r(false, 1, &[], false))), (Tone::Err, "rm.not_mounted"));
        assert_eq!(state(Some(&r(true, 2, &["data raid1"], false))), (Tone::Err, "health.failed"));
        assert_eq!(state(Some(&r(true, 1, &[], true))), (Tone::Warn, "health.degraded"));
        assert_eq!(state(Some(&r(true, 0, &[], true))), (Tone::Warn, "sto.h.warning"));
    }
}
