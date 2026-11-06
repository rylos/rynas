//! Removable drives in the app bar, next to the bell: a USB glyph with the number of drives,
//! hidden while nothing is plugged in. Its tone says what matters: accent while a volume is
//! mounted, warning while a drive is written to or an action runs, plain otherwise; the
//! tooltip lists the drives with their volumes and mount points.
//!
//! The popover has one card per drive (name, capacity, USB link speed, **Remove safely**,
//! whose tooltip says what it does for that drive) and one row per volume (label,
//! filesystem, mount point, usage bar; Open in the file manager, copy the path, Mount /
//! Unmount, Unlock with an inline passphrase, Lock). The header has Mount all, Remove all
//! safely, Refresh and the settings. Data comes from `removable.state` events
//! ([`crate::live`]); actions are rynasd tasks followed to their end.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::removable::{
    DriveBus, RemovableAction, RemovableDrive, RemovableVolume, RemoveMethod, VolumeHolder, usb_speed_name,
};
use rynas_api_types::TaskStatus;
use rynas_api_types::removable::UnlockVolumeRequest;

use crate::api;
use crate::form::{In, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::live::use_live;
use crate::prefs::use_ui;
use crate::toast::toast;
use crate::ui::{EmptyState, Meter, Tone, usage_tone};

const BASE: &str = "/api/v1/removable";

/// The icon of a drive.
pub fn drive_icon(d: &RemovableDrive) -> &'static str {
    match d.bus {
        DriveBus::Sd => icons::SD_CARD,
        DriveBus::Optical => icons::DISC,
        _ => icons::USB,
    }
}

/// "2 drives" / "1 drive".
fn count_text(n: usize) -> String {
    if n == 1 { t("rm.count_one").to_string() } else { tf("rm.count", &[("n", &n.to_string())]) }
}

fn mounted_text(n: usize) -> String {
    if n == 1 { t("rm.mounted_one").to_string() } else { tf("rm.mounted", &[("n", &n.to_string())]) }
}

fn remove_hint(m: RemoveMethod) -> &'static str {
    match m {
        RemoveMethod::PowerOff => "rm.hint.power_off",
        RemoveMethod::Eject => "rm.hint.eject",
        RemoveMethod::Release => "rm.hint.release",
    }
}

fn pending_key(a: RemovableAction) -> &'static str {
    match a {
        RemovableAction::Mount => "rm.pending.mount",
        RemovableAction::Unmount => "rm.pending.unmount",
        RemovableAction::Unlock => "rm.pending.unlock",
        RemovableAction::Lock => "rm.pending.lock",
        RemovableAction::Remove => "rm.pending.remove",
    }
}

/// The file manager's route for a mount point: the file manager lists a mounted volume as a
/// shared folder named after the mount point's last component (`/media/STICK` → `/files/STICK`).
pub fn files_href(mount_point: &str) -> String {
    let last = mount_point.split('/').rfind(|s| !s.is_empty()).unwrap_or("");
    format!("/files/{}", pct(last))
}

/// RFC 3986 percent-encoding of a path segment (everything but the unreserved characters).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Size, link and state of a drive: "64,0 GB · USB 3 · 5 Gbps".
fn drive_details(d: &RemovableDrive) -> String {
    let mut parts = vec![if d.no_media { t("rm.no_media").to_string() } else { i18n::bytes_si(d.size_bytes) }];
    match (d.bus, d.usb.as_ref().and_then(|u| usb_speed_name(u.speed_mbps))) {
        (_, Some(s)) => parts.push(s),
        (DriveBus::Usb, None) => parts.push("USB".into()),
        (DriveBus::Sd, None) => parts.push(t("rm.bus.sd").into()),
        (DriveBus::Optical, None) => parts.push(t("rm.bus.optical").into()),
        (DriveBus::Other, None) => parts.push(t("rm.bus.other").into()),
    }
    if d.read_only {
        parts.push(t("rm.read_only").into());
    }
    parts.join(" · ")
}

fn volume_name(v: &RemovableVolume) -> String {
    v.display_label().map(str::to_string).unwrap_or_else(|| t("rm.untitled").to_string())
}

/// The tooltip of the app bar button.
fn tooltip(drives: &[RemovableDrive]) -> String {
    let mut lines = vec![t("rm.title").to_string()];
    for d in drives {
        let mut l = format!("{} · {}", d.title, if d.no_media { t("rm.no_media").to_string() } else { i18n::bytes_si(d.size_bytes) });
        for v in &d.volumes {
            let state = match &v.mount_point {
                Some(p) => tf("rm.tip_mounted", &[("path", p)]),
                None => t("rm.tip_not_mounted").to_string(),
            };
            l.push_str(&format!(" · {}: {state}", volume_name(v)));
        }
        lines.push(l);
    }
    lines.join("\n")
}

/// What the panel reports after an action: an error line (translated, with details).
#[derive(Clone, Copy)]
struct Panel {
    error: RwSignal<Option<String>>,
    unlocking: RwSignal<Option<String>>,
    pass: RwSignal<String>,
}

/// "In use by: smbd (pid 812, alice)…" for the mounted volumes `vols`, else the error.
async fn busy_text(vols: Vec<String>, fallback: String) -> String {
    let mut all: Vec<VolumeHolder> = Vec::new();
    for v in vols {
        if let Ok(h) = api::get::<Vec<VolumeHolder>>(&format!("{BASE}/volumes/{}/holders", api::encode(&v))).await {
            all.extend(h);
        }
    }
    if all.is_empty() {
        return fallback;
    }
    let list: Vec<String> = all
        .iter()
        .map(|h| if h.user.is_empty() { format!("{} ({})", h.command, h.pid) } else { format!("{} ({}, {})", h.command, h.pid, h.user) })
        .collect();
    tf("rm.busy_by", &[("list", &list.join(", "))])
}

/// Runs an action (a rynasd task) and reports its failure in the panel; `busy_vols` are the
/// volumes whose holders explain a `busy` refusal; `done` runs after a success.
fn act(p: Panel, url: String, body: Option<String>, busy_vols: Vec<String>, done: Option<Box<dyn FnOnce() + 'static>>) {
    p.error.set(None);
    spawn_local(async move {
        match crate::tasks::run_task(&url, body).await {
            Ok(info) if info.status == TaskStatus::Ok => {
                if let Some(f) = done {
                    f();
                }
            }
            Ok(info) => p.error.set(Some(crate::tasks::task_error(&info))),
            Err(e) if e.code() == rynas_api_types::error_codes::BUSY && !busy_vols.is_empty() => {
                let msg = busy_text(busy_vols, e.text()).await;
                p.error.set(Some(msg));
            }
            Err(e) => p.error.set(Some(e.text())),
        }
    });
}

/// The app bar button with its popover.
#[component]
pub fn UsbButton() -> impl IntoView {
    let ui = use_ui();
    let live = use_live();
    let open = ui.usb_open;
    let drives = Memo::new(move |_| live.removable.with(|r| r.as_ref().map(|r| r.drives.clone()).unwrap_or_default()));
    let count = Memo::new(move |_| drives.with(Vec::len));
    let mounted = Memo::new(move |_| drives.with(|d| d.iter().map(RemovableDrive::mounted).sum::<usize>()));
    let busy = Memo::new(move |_| live.removable.with(|r| r.as_ref().is_some_and(|r| r.busy())));
    let tone = move || {
        if busy.get() {
            "warn"
        } else if mounted.get() > 0 {
            "on"
        } else {
            ""
        }
    };
    let label = move || format!("{} ({})", t("rm.title"), count_text(count.get()));
    let tip = move || drives.with(|d| tooltip(d));
    // the popover closes when the last drive goes
    Effect::new(move || {
        if count.get() == 0 && open.get_untracked() {
            open.set(false);
        }
    });
    let glyph = move || drives.with(|d| if d.len() == 1 { drive_icon(&d[0]) } else { icons::USB });
    view! {
        <Show when=move || { count.get() > 0 }>
            <div class="pop-anchor">
                <button
                    class=move || format!("icon-btn usb-btn {}", tone())
                    aria-label=label
                    title=tip
                    aria-haspopup="dialog"
                    aria-expanded=move || open.get().to_string()
                    on:click=move |_| {
                        ui.notif_open.set(false);
                        ui.user_menu_open.set(false);
                        open.update(|o| *o = !*o);
                    }
                >
                    {move || view! { <Icon icon=glyph() /> }}
                    <Show when=move || { count.get() > 1 }>
                        <span class=move || format!("badge usb-badge {}", tone()) aria-hidden="true">{move || count.get()}</span>
                    </Show>
                </button>
                <Show when=move || open.get()>
                    <UsbPanel />
                </Show>
            </div>
        </Show>
    }
}

#[component]
fn UsbPanel() -> impl IntoView {
    let ui = use_ui();
    let live = use_live();
    let p = Panel { error: RwSignal::new(None), unlocking: RwSignal::new(None), pass: RwSignal::new(String::new()) };
    let drives = Memo::new(move |_| live.removable.with(|r| r.as_ref().map(|r| r.drives.clone()).unwrap_or_default()));
    // a volume that went away or was unlocked elsewhere drops its passphrase prompt
    Effect::new(move || {
        let still = p.unlocking.get().is_some_and(|u| drives.with(|l| l.iter().flat_map(|d| &d.volumes).any(|v| v.name == u && v.luks && !v.unlocked)));
        if !still && p.unlocking.get_untracked().is_some() {
            p.unlocking.set(None);
            p.pass.set(String::new());
        }
    });
    on_cleanup(move || p.pass.set(String::new()));
    let subtitle = move || {
        let (n, m) = drives.with(|d| (d.len(), d.iter().map(RemovableDrive::mounted).sum::<usize>()));
        if m > 0 { format!("{} · {}", count_text(n), mounted_text(m)) } else { count_text(n) }
    };
    let any_unmounted = move || drives.with(|d| d.iter().flat_map(|d| &d.volumes).any(|v| !v.luks && v.supported && v.mount_point.is_none() && v.pending.is_none()));
    let mount_all = move |_| act(p, format!("{BASE}/mount-all"), None, Vec::new(), None);
    let remove_all = move |_| {
        let vols: Vec<String> = drives.with(|d| d.iter().flat_map(|d| &d.volumes).filter(|v| v.mount_point.is_some()).map(|v| v.name.clone()).collect());
        act(p, format!("{BASE}/remove-all"), None, vols, Some(Box::new(|| toast(Tone::Ok, t("rm.safe_all").to_string()))));
    };
    let refresh = move |_| {
        spawn_local(async move {
            match api::post::<(), rynas_api_types::removable::RemovableState>(&format!("{BASE}/refresh"), None).await {
                Ok(r) => live.set_removable(r),
                Err(e) => p.error.set(Some(e.text())),
            }
        });
    };
    view! {
        <div class="popover usb-pop" role="dialog" aria-label=tr("rm.title")>
            <div class="usb-head">
                <span class="usb-head-icon"><Icon icon=icons::USB size=18 /></span>
                <div class="usb-head-text">
                    <span class="notif-title">{tr("rm.title")}</span>
                    <span class="muted small">{subtitle}</span>
                </div>
                <Show when=any_unmounted>
                    <button class="icon-btn sm" title=tr("rm.mount_all") aria-label=tr("rm.mount_all") on:click=mount_all>
                        <Icon icon=icons::FOLDER_OPEN size=16 />
                    </button>
                </Show>
                <button class="icon-btn sm" title=tr("rm.remove_all") aria-label=tr("rm.remove_all") on:click=remove_all>
                    <Icon icon=icons::UNPLUG size=16 />
                </button>
                <button class="icon-btn sm" title=tr("rm.refresh") aria-label=tr("rm.refresh") on:click=refresh>
                    <Icon icon=icons::REFRESH_CW size=16 />
                </button>
                <a class="icon-btn sm" href="/hardware#removable" title=tr("rm.settings") aria-label=tr("rm.settings") on:click=move |_| ui.usb_open.set(false)>
                    <Icon icon=icons::SETTINGS size=16 />
                </a>
            </div>
            {move || p.error.get().map(|e| view! {
                <div class="notif-err" role="alert">
                    <Icon icon=icons::TRIANGLE_ALERT size=14 />
                    {crate::ui::err_view(e)}
                </div>
            })}
            <Show
                when=move || drives.with(|d| !d.is_empty())
                fallback=|| view! {
                    <EmptyState
                        icon=icons::USB
                        title=Signal::derive(|| t("rm.empty").to_string())
                        text=Signal::derive(|| t("rm.empty_text").to_string())
                    />
                }
            >
                <ul class="usb-list">
                    <For each=move || drives.with(|d| d.iter().map(|d| d.name.clone()).collect::<Vec<_>>()) key=|n| n.clone() let:name>
                        {drive_card(drives, name, p)}
                    </For>
                </ul>
            </Show>
        </div>
    }
}

/// The drive cards with their own error line (the settings page shows the same list).
pub fn drives_view() -> AnyView {
    let live = use_live();
    let p = Panel { error: RwSignal::new(None), unlocking: RwSignal::new(None), pass: RwSignal::new(String::new()) };
    on_cleanup(move || p.pass.set(String::new()));
    let drives = Memo::new(move |_| live.removable.with(|r| r.as_ref().map(|r| r.drives.clone()).unwrap_or_default()));
    view! {
        {crate::form::form_error(p.error)}
        <Show
            when=move || drives.with(|d| !d.is_empty())
            fallback=|| view! {
                <EmptyState
                    icon=icons::USB
                    title=Signal::derive(|| t("rm.empty").to_string())
                    text=Signal::derive(|| t("rm.empty_text").to_string())
                />
            }
        >
            <ul class="usb-list page-list">
                <For each=move || drives.with(|d| d.iter().map(|d| d.name.clone()).collect::<Vec<_>>()) key=|n| n.clone() let:name>
                    {drive_card(drives, name, p)}
                </For>
            </ul>
        </Show>
    }
    .into_any()
}

fn drive_card(drives: Memo<Vec<RemovableDrive>>, name: String, p: Panel) -> AnyView {
    let n2 = name.clone();
    let d = Memo::new(move |_| drives.with(|l| l.iter().find(|d| d.name == n2).cloned()));
    let get = move || d.get().unwrap_or_default();
    let pending = move || d.with(|d| d.as_ref().and_then(|d| d.pending));
    let subtitle = move || {
        let d = get();
        match d.pending {
            Some(a) => t(pending_key(a)).to_string(),
            None if d.busy => format!("{} · {}", drive_details(&d), t("rm.writing")),
            None => drive_details(&d),
        }
    };
    let working = move || d.with(|d| d.as_ref().is_some_and(|d| d.pending.is_some() || d.busy));
    let full_title = move || {
        let d = get();
        if d.serial.is_empty() { d.title } else { format!("{} · {}", d.title, d.serial) }
    };
    let remove = {
        let name = name.clone();
        move |_| {
            let d = get();
            let vols: Vec<String> = d.volumes.iter().filter(|v| v.mount_point.is_some()).map(|v| v.name.clone()).collect();
            let title = d.title.clone();
            act(
                p,
                format!("{BASE}/drives/{}/remove", api::encode(&name)),
                None,
                vols,
                Some(Box::new(move || toast(Tone::Ok, tf("rm.safe", &[("name", &title)])))),
            );
        }
    };
    let vol_names = move || d.with(|d| d.as_ref().map(|d| d.volumes.iter().map(|v| v.name.clone()).collect::<Vec<_>>()).unwrap_or_default());
    view! {
        <li class=move || if pending() == Some(RemovableAction::Remove) { "usb-drive dim" } else { "usb-drive" }>
            <div class="usb-drive-head">
                <span class="usb-drive-icon">{move || view! { <Icon icon=drive_icon(&get()) size=18 /> }}</span>
                <div class="usb-drive-text" title=full_title>
                    <strong class="ellipsis">{move || get().title}</strong>
                    <span class=move || if working() { "small tone-text warn" } else { "small muted" }>{subtitle}</span>
                </div>
                <button
                    class="btn ghost sm usb-remove"
                    title=move || t(remove_hint(get().remove_method))
                    disabled=move || pending().is_some()
                    on:click=remove
                >
                    {move || view! { <Icon icon=if get().remove_method == RemoveMethod::Eject { icons::EJECT } else { icons::UNPLUG } size=16 /> }}
                    {tr("rm.remove")}
                </button>
            </div>
            <For each=vol_names key=|n| n.clone() let:vname>
                {volume_row(d, vname, p)}
            </For>
        </li>
    }
    .into_any()
}

fn volume_row(d: Memo<Option<RemovableDrive>>, name: String, p: Panel) -> AnyView {
    let n2 = name.clone();
    let v = Memo::new(move |_| d.with(|d| d.as_ref().and_then(|d| d.volumes.iter().find(|v| v.name == n2).cloned())));
    let get = move || v.get().unwrap_or_default();
    let busy = move || v.with(|v| v.as_ref().and_then(|v| v.pending));
    let mounted = move || v.with(|v| v.as_ref().is_some_and(|v| v.mount_point.is_some()));
    let locked = move || v.with(|v| v.as_ref().is_some_and(|v| v.luks && !v.unlocked));
    let status = move || {
        let v = get();
        if let Some(a) = v.pending {
            t(pending_key(a)).to_string()
        } else if let Some(mp) = &v.mount_point {
            mp.clone()
        } else if v.luks && !v.unlocked {
            t("rm.encrypted").to_string()
        } else if !v.supported {
            t("rm.unsupported").to_string()
        } else {
            t("rm.not_mounted").to_string()
        }
    };
    let dot = move || {
        if mounted() {
            "usb-dot on"
        } else if locked() {
            "usb-dot locked"
        } else {
            "usb-dot"
        }
    };
    let fs = move || {
        let v = get();
        if v.luks { v.clear.as_ref().map(|c| c.fs_type.clone()).filter(|f| !f.is_empty()).unwrap_or_else(|| "LUKS".into()) } else { v.fs_type }
    };
    let usage = move || {
        let v = get();
        match (v.total_bytes, v.used_bytes, v.avail_bytes) {
            (Some(total), Some(used), Some(avail)) if v.mount_point.is_some() && total > 0 => {
                let pct = used as f64 * 100.0 / total as f64;
                Some(view! {
                    <Meter pct=Signal::derive(move || pct) tone=Signal::derive(move || usage_tone(pct)) />
                    <span class="small muted">{tf("rm.free", &[("free", &i18n::bytes(avail)), ("total", &i18n::bytes(total))])}</span>
                }
                .into_any())
            }
            _ if v.mount_point.is_none() => Some(view! { <span class="small muted">{i18n::bytes_si(v.size_bytes)}</span> }.into_any()),
            _ => None,
        }
    };
    let url = move |verb: &str| format!("{BASE}/volumes/{}/{verb}", api::encode(&get().name));
    let run = move |verb: &'static str| {
        let vols = if verb == "unmount" || verb == "lock" { vec![get().name] } else { Vec::new() };
        act(p, url(verb), None, vols, None);
    };
    let name_u = name.clone();
    let unlock_open = move || p.unlocking.with(|u| u.as_deref() == Some(name_u.as_str()));
    let submit = move || {
        let pass = p.pass.get_untracked();
        if pass.is_empty() {
            return;
        }
        let body = serde_json::to_string(&UnlockVolumeRequest { passphrase: pass, mount: true }).ok();
        // never kept: the field empties at once
        p.pass.set(String::new());
        p.unlocking.set(None);
        act(p, url("unlock"), body, Vec::new(), None);
    };
    let actions = move || {
        let v = get();
        let off = v.pending.is_some();
        let mut out: Vec<AnyView> = Vec::new();
        if let Some(mp) = v.mount_point.clone() {
            let href = files_href(&mp);
            out.push(view! {
                <a class="icon-btn sm" href=href title=tr("rm.open") aria-label=tr("rm.open") on:click=move |_| use_ui().usb_open.set(false)>
                    <Icon icon=icons::FOLDER_OPEN size=16 />
                </a>
            }.into_any());
            out.push(view! {
                <button class="icon-btn sm" title=tr("rm.copy") aria-label=tr("rm.copy") on:click=move |_| {
                    if crate::pages::security::copy_text(&mp) {
                        toast(Tone::Ok, t("rm.copied").to_string());
                    }
                }>
                    <Icon icon=icons::COPY size=16 />
                </button>
            }.into_any());
        }
        let btn = move |icon: &'static str, key: &'static str, verb: &'static str| {
            view! {
                <button class="icon-btn sm" title=tr(key) aria-label=tr(key) disabled=off on:click=move |_| run(verb)>
                    <Icon icon=icon size=16 />
                </button>
            }
            .into_any()
        };
        if v.luks {
            if !v.unlocked {
                let n = v.name.clone();
                out.push(view! {
                    <button class="icon-btn sm" title=tr("rm.unlock") aria-label=tr("rm.unlock") disabled=off on:click=move |_| {
                        p.pass.set(String::new());
                        p.unlocking.set(Some(n.clone()));
                    }>
                        <Icon icon=icons::LOCK_OPEN size=16 />
                    </button>
                }.into_any());
            } else {
                if v.mount_point.is_none() && v.supported {
                    out.push(btn(icons::PLAY, "rm.mount", "mount"));
                }
                out.push(btn(icons::LOCK, "rm.lock", "lock"));
            }
        } else if v.mount_point.is_some() {
            out.push(btn(icons::EJECT, "rm.unmount", "unmount"));
        } else if v.supported {
            out.push(btn(icons::PLAY, "rm.mount", "mount"));
        }
        out
    };
    view! {
        <div class="usb-vol">
            <div class="usb-vol-main">
                <span class=dot aria-hidden="true"></span>
                <div class="usb-vol-info">
                    <div class="usb-vol-title">
                        <strong class="ellipsis">{move || volume_name(&get())}</strong>
                        {move || (!fs().is_empty()).then(|| view! { <span class="chip">{fs()}</span> })}
                        {move || get().luks.then(|| view! { <span class="chip accent2">{if get().unlocked { t("rm.unlocked") } else { t("rm.locked") }}</span> })}
                        {move || get().read_only.then(|| view! { <span class="chip">{t("rm.read_only")}</span> })}
                        {move || get().share.map(|s| view! { <span class="chip accent" title=tf("rm.shared", &[("name", &s)])>{"SMB"}</span> })}
                    </div>
                    <span class=move || if busy().is_some() { "small tone-text warn usb-vol-status" } else { "small muted usb-vol-status" }>{status}</span>
                    {usage}
                </div>
                <div class="usb-vol-actions">{actions}</div>
            </div>
            <Show when=unlock_open.clone()>
                <form class="usb-unlock" on:submit=move |ev| { ev.prevent_default(); submit(); }>
                    {text_field(
                        "sto.passphrase",
                        Signal::derive(move || p.pass.get()),
                        Callback::new(move |s| p.pass.set(s)),
                        Signal::derive(|| None),
                        In { kind: "password", autocomplete: "off", maxlength: 512, ..In::default() },
                    )}
                    <div class="btn-row">
                        <button type="button" class="btn ghost sm" on:click=move |_| { p.pass.set(String::new()); p.unlocking.set(None); }>{tr("cancel")}</button>
                        <button type="submit" class="btn primary sm" disabled=move || p.pass.with(String::is_empty)>{tr("rm.unlock")}</button>
                    </div>
                </form>
            </Show>
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_manager_route() {
        assert_eq!(files_href("/media/My Stick"), "/files/My%20Stick");
        assert_eq!(files_href("/media/SANDISK"), "/files/SANDISK");
    }
}
