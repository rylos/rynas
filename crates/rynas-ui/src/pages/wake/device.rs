//! The panel of a device (status, Wake-on-LAN health, wake history, Settings, Remove) and
//! the device form shared by Settings and Add device.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::pcagent::{AgentNic, PcBackupOverview};
use rynas_api_types::UserView;
use rynas_api_types::wol::*;

use super::{Wol, URL, demo, dev_url, kind_icon, shown_step, sleep_menu, status_view, wake_button, wake_line, warning_text};
use crate::api;
use crate::form::{In, SaveBar, area_field, form_error, number_field, save_bar_with, segmented, select_field, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, Confirm, DlgKind, Remote, Tone};

// ------------------------------------------------------------------ the panel

fn probe_text(p: WolProbe) -> &'static str {
    match p {
        WolProbe::Agent => "wol.probe.agent",
        WolProbe::Port => "wol.probe.port",
        WolProbe::Ping => "wol.probe.ping",
        WolProbe::Neighbor => "wol.probe.neighbor",
    }
}

fn end_view(e: WolWakeEnd) -> AnyView {
    let (tone, k) = match e {
        WolWakeEnd::Online => (Tone::Ok, "wol.h.online"),
        WolWakeEnd::AlreadyOn => (Tone::Info, "wol.h.already_on"),
        WolWakeEnd::Timeout => (Tone::Warn, "wol.h.timeout"),
        WolWakeEnd::SendFailed => (Tone::Err, "wol.h.send_failed"),
        WolWakeEnd::Unconfirmed => (Tone::Info, "wol.h.unconfirmed"),
    };
    ui::status(tone, t(k).to_string())
}

/// A yes / no / unknown fact of the agent, as a pill.
fn fact(v: Option<bool>, yes: &'static str, no: &'static str) -> AnyView {
    match v {
        Some(true) => ui::status(Tone::Ok, t(yes).to_string()),
        Some(false) => ui::status(Tone::Warn, t(no).to_string()),
        None => ui::status(Tone::Unknown, t("wol.health.unknown").to_string()),
    }
}

/// The card a wake reaches: the one with the first MAC, else the first wired one.
fn wake_nic<'a>(nics: &'a [AgentNic], macs: &[String]) -> Option<&'a AgentNic> {
    macs.first().and_then(|m| nics.iter().find(|n| n.mac == *m)).or_else(|| nics.iter().find(|n| !n.wireless))
}

pub(super) fn drawer(w: Wol, id: String) -> AnyView {
    let close = Callback::new(move |_| w.open.set(None));
    let sid = StoredValue::new(id.clone());
    let v = Memo::new(move |_| w.view_of(&sid.get_value()));
    let edit = RwSignal::new(false);
    let ask_remove = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let enabling = RwSignal::new(false);
    let enable = move |_| {
        if demo() {
            toast(Tone::Ok, t("wol.enable_sent").to_string());
            return;
        }
        enabling.set(true);
        error.set(None);
        let u = format!("{}/enable", dev_url(&sid.get_value()));
        spawn_local(async move {
            let r = api::post_empty::<()>(&u, None).await;
            let _ = enabling.try_set(false);
            match r {
                Ok(()) => {
                    toast(Tone::Ok, t("wol.enable_sent").to_string());
                    w.data.reload();
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let remove = Callback::new(move |_| {
        let Some(name) = v.with_untracked(|v| v.as_ref().map(|v| v.device.name.clone())) else { return };
        busy.set(true);
        error.set(None);
        let u = dev_url(&sid.get_value());
        spawn_local(async move {
            let r = api::delete_empty(&u).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => {
                    toast(Tone::Ok, tf("wol.removed", &[("name", &name)]));
                    w.open.set(None);
                    w.data.reload();
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    // the status card's facts follow the polls; its actions are built once, so a poll keeps
    // an open Power menu (and its confirmation) as it is
    let card_facts = move || {
        v.with(|v| {
            v.as_ref().map(|v| {
                let d = &v.device;
                let meta = [
                    v.since_ms.map(|s| tf("wol.since_when", &[("when", &i18n::datetime(s))])),
                    v.probe.map(|p| tf("wol.seen_by", &[("how", t(probe_text(p)))])),
                    v.ip.clone().or_else(|| d.host.clone()),
                    Some(match &v.send_iface {
                        Some(i) => tf("wol.sends_on", &[("iface", i)]),
                        None => t("wol.sends_all").to_string(),
                    }),
                ];
                let pending = v.agent.as_ref().is_some_and(|a| a.pending.is_some());
                view! {
                    <div class="status-card-head">
                        <span class="wol-kind" aria-hidden="true"><Icon icon=kind_icon(d.kind) size=18 /></span>
                        {status_view(v.status)}
                        {v.agent.is_some().then(|| ui::chip(Tone::Info, view! { {tr("wol.agent")} }.into_any()))}
                        {pending.then(|| ui::chip(Tone::Info, view! { <Icon icon=icons::CLOCK size=12 />{tr("pca.waiting_pc")} }.into_any()))}
                    </div>
                    <p class="status-meta small">{meta.into_iter().flatten().map(|s| view! { <span>{s}</span> }).collect_view()}</p>
                    <p class="status-meta small mono">{d.macs.iter().map(|m| view! { <span>{m.clone()}</span> }).collect_view()}</p>
                }
            })
        })
    };
    let card = view! {
        <Show when=move || v.with(Option::is_some)>
            <section class="status-card">
                {card_facts}
                <div class="sec-actions">{wake_button(w, sid.get_value())}{sleep_menu(w, sid.get_value())}</div>
                {wake_line(w, sid.get_value(), None)}
            </section>
        </Show>
    };
    let content = move || {
        let Some(v) = v.get() else { return ui::empty_line("wol.gone") };
        let d = &v.device;
        let health = v.agent.as_ref().map(|a| {
            let nic = wake_nic(&a.nics, &d.macs).cloned();
            let enabled = nic.as_ref().and_then(|n| n.wol_enabled);
            let card = match &nic {
                Some(n) if n.wireless => ui::status(Tone::Warn, tf("wol.health.wifi", &[("name", &n.name)])),
                Some(n) => ui::status(Tone::Ok, tf("wol.health.wired", &[("name", &n.name)])),
                None => ui::status(Tone::Unknown, t("wol.health.no_card").to_string()),
            };
            // only a wired card that says it supports magic packets, with an agent that takes it
            let can_enable = v.can_manage && a.power_commands && enabled == Some(false) && nic.as_ref().is_some_and(|n| !n.wireless && n.wol_supported == Some(true));
            let old_agent = !a.power_commands;
            view! {
                <div class="sec-head"><h3 class="mini-title">{tr("wol.health")}</h3></div>
                <dl class="kvs">
                    {ui::kv("wol.health.card", card)}
                    {ui::kv("wol.health.magic", fact(nic.as_ref().and_then(|n| n.wol_supported), "wol.health.supported", "wol.health.not_supported"))}
                    {ui::kv_act("wol.health.enabled", Some(fact(enabled, "wol.health.on", "wol.health.off")), can_enable.then(|| view! {
                        <button type="button" class="btn ghost sm" disabled=move || enabling.get() on:click=enable>{crate::form::spinner(enabling)}{tr("wol.enable")}</button>
                    }.into_any()), nic.as_ref().filter(|n| n.wol_kept).map(|_| view! { <span class="small muted">{tr("wol.health.kept")}</span> }.into_any()))}
                    {ui::kv_text("wol.health.agent", a.agent_version.clone(), "mono")}
                </dl>
                {old_agent.then(|| ui::note(Tone::Info, icons::INFO, ui::text(t("wol.health.old_agent").to_string())))}
            }
        });
        let warnings = v.warnings.iter().map(|x| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t(warning_text(*x)).to_string()))).collect_view();
        let history = v
            .history
            .iter()
            .take(20)
            .map(|h| {
                let by = match &h.by {
                    // the server leaves out other users' names for a user who is not an administrator
                    WolTrigger::User { name } if name.is_empty() => t("wol.by_other").to_string(),
                    WolTrigger::User { name } => name.clone(),
                    WolTrigger::Backup => t("wol.by_backup").to_string(),
                };
                ui::row(vec![
                    ui::text_cell("pca.col.when", "tnum", i18n::datetime(h.at_ms)),
                    ui::text_cell("wol.col.by", "wrap", by),
                    // a failed send says why (the step it kept), the other results speak for themselves
                    ui::cell("wol.col.result", "wrap", match shown_step(h.step.as_ref(), true).filter(|_| h.end == WolWakeEnd::SendFailed) {
                        Some(why) => view! { <div class="cell-main">{end_view(h.end)}<span class="cell-sub small">{why}</span></div> }.into_any(),
                        None => end_view(h.end),
                    }),
                    ui::text_cell("wol.col.secs", "num tnum", h.secs.map(|s| i18n::elapsed(u64::from(s))).unwrap_or_else(|| ui::DASH.to_string())),
                    ui::cell("wol.col.ifaces", "wrap", view! { <span class="mono small">{if h.ifaces.is_empty() { ui::DASH.to_string() } else { h.ifaces.join(", ") }}</span> }.into_any()),
                ])
            })
            .collect::<Vec<_>>();
        view! {
            {warnings}
            {health}
            {(!d.notes.is_empty()).then(|| view! { <p class="small wol-notes">{d.notes.clone()}</p> })}
            <div class="sec-head"><h3 class="mini-title">{tr("wol.history")}</h3></div>
            {if history.is_empty() {
                ui::empty_line("wol.no_history")
            } else {
                ui::table("wol-history", &["pca.col.when", "wol.col.by", "wol.col.result", "#wol.col.secs", "wol.col.ifaces"], history)
            }}
            {form_error(error)}
        }
        .into_any()
    };
    let manage = Signal::derive(move || v.with(|v| v.as_ref().is_some_and(|v| v.can_manage)));
    let body = view! { {card}{move || w.data.view(6, content)} }.into_any();
    let title = Signal::derive(move || v.with(|v| v.as_ref().map(|v| v.device.name.clone()).unwrap_or_default()));
    let settings = view! {
        <Show when=move || manage.get()>
            <button type="button" class="btn ghost" on:click=move |_| edit.set(true)><Icon icon=icons::SETTINGS size=16 />{tr("bk.settings")}</button>
        </Show>
    }
    .into_any();
    // administrators only; off until the device is read (a panel opened from a link)
    let danger = super::is_admin().then(|| ui::Danger {
        disabled: Signal::derive(move || !manage.get()),
        ..ui::Danger::new("wol.remove", Callback::new(move |_| {
            error.set(None);
            ask_remove.set(true);
        }))
    });
    view! {
        {ui::detail_drawer(title, close, true, body, danger, Some(settings))}
        {move || ask_remove.get().then(|| ui::confirm(Confirm { busy, error, ..Confirm::new(tf("wol.remove_title", &[("name", &title.get_untracked())]), t("wol.remove_text").to_string(), "wol.remove", true, remove, Callback::new(move |_| ask_remove.set(false))) }))}
        {move || edit.get().then(|| v.get_untracked().map(|v| settings_dialog(w, Some(v.device), Callback::new(move |_| edit.set(false)))))}
    }
    .into_any()
}

// ------------------------------------------------------------------ the form

const KINDS: &[(&str, &str)] = &[("desktop", "wol.kind.desktop"), ("laptop", "wol.kind.laptop"), ("server", "wol.kind.server"), ("media", "wol.kind.media"), ("other", "wol.kind.other")];

fn kind_code(k: WolKind) -> &'static str {
    match k {
        WolKind::Desktop => "desktop",
        WolKind::Laptop => "laptop",
        WolKind::Server => "server",
        WolKind::Media => "media",
        WolKind::Other => "other",
    }
}

fn kind_of(s: &str) -> WolKind {
    match s {
        "laptop" => WolKind::Laptop,
        "server" => WolKind::Server,
        "media" => WolKind::Media,
        "other" => WolKind::Other,
        _ => WolKind::Desktop,
    }
}

/// A SecureOn password: 6 bytes as a MAC or 4 as `a.b.c.d`.
fn secureon_ok(s: &str) -> bool {
    let hex: Vec<&str> = s.split([':', '-']).collect();
    let six = hex.len() == 6 && hex.iter().all(|h| h.len() == 2 && h.chars().all(|c| c.is_ascii_hexdigit()));
    let four = s.split('.').count() == 4 && s.split('.').all(|p| p.parse::<u8>().is_ok());
    six || four
}

/// The values of a device form.
#[derive(Clone, Copy)]
struct Form {
    id: StoredValue<String>,
    name: RwSignal<String>,
    kind: RwSignal<String>,
    macs: RwSignal<String>,
    host: RwSignal<String>,
    iface: RwSignal<String>,
    port: RwSignal<String>,
    secureon: RwSignal<String>,
    ports: RwSignal<String>,
    timeout: RwSignal<String>,
    pc: RwSignal<String>,
    users: RwSignal<Vec<String>>,
    notes: RwSignal<String>,
    name_err: RwSignal<Option<String>>,
    macs_err: RwSignal<Option<String>>,
    secureon_err: RwSignal<Option<String>>,
    ports_err: RwSignal<Option<String>>,
    host_err: RwSignal<Option<String>>,
    /// The server refused one of the fields without a text input (`iface`,
    /// `wake_timeout_s`, `pc_device`, `users`, `notes`, `port`): its name and why.
    other_err: RwSignal<Option<(String, String)>>,
    /// The Advanced fold is open (opened by an error inside it).
    adv_open: RwSignal<bool>,
}

/// A field of the Advanced fold.
fn advanced_field(f: &str) -> bool {
    matches!(f, "iface" | "port" | "secureon" | "ports" | "wake_timeout_s")
}

impl Form {
    fn new(d: &WolDevice) -> Self {
        Form {
            id: StoredValue::new(d.id.clone()),
            name: RwSignal::new(d.name.clone()),
            kind: RwSignal::new(kind_code(d.kind).to_string()),
            macs: RwSignal::new(d.macs.join(", ")),
            host: RwSignal::new(d.host.clone().unwrap_or_default()),
            iface: RwSignal::new(d.iface.clone().unwrap_or_default()),
            port: RwSignal::new(d.port.to_string()),
            secureon: RwSignal::new(d.secureon.clone().unwrap_or_default()),
            ports: RwSignal::new(d.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")),
            timeout: RwSignal::new(d.wake_timeout_s.to_string()),
            pc: RwSignal::new(d.pc_device.clone().unwrap_or_default()),
            users: RwSignal::new(d.users.clone()),
            notes: RwSignal::new(d.notes.clone()),
            name_err: RwSignal::new(None),
            macs_err: RwSignal::new(None),
            secureon_err: RwSignal::new(None),
            ports_err: RwSignal::new(None),
            host_err: RwSignal::new(None),
            other_err: RwSignal::new(None),
            adv_open: RwSignal::new(false),
        }
    }

    /// The error of a field without a text input, under it.
    fn field_note(self, field: &'static str) -> AnyView {
        (move || {
            self.other_err.with(|e| e.as_ref().filter(|(f, _)| f == field).map(|(_, m)| m.clone())).map(|m| {
                view! { <p class="field-error" role="alert"><Icon icon=icons::CIRCLE_X size=12 />{ui::err_view(m)}</p> }
            })
        })
        .into_any()
    }

    /// Puts a refusal of the server on its field; `false` when the field is not one of the
    /// form's (the text goes to the form's error then).
    fn server_error(self, field: Option<&str>, text: String) -> bool {
        let Some(f) = field else { return false };
        // `devices.3.macs` from a whole-configuration check: the last part
        let f = f.rsplit('.').next().unwrap_or(f);
        match f {
            "name" | "id" => self.name_err.set(Some(text)),
            "macs" => self.macs_err.set(Some(text)),
            "secureon" => self.secureon_err.set(Some(text)),
            "ports" => self.ports_err.set(Some(text)),
            "host" => self.host_err.set(Some(text)),
            "iface" | "port" | "wake_timeout_s" | "pc_device" | "users" | "notes" => self.other_err.set(Some((f.to_string(), text))),
            _ => return false,
        }
        if advanced_field(f) {
            self.adv_open.set(true);
        }
        ui::focus_invalid();
        true
    }

    /// Puts what a candidate knows into the form (Add device).
    fn take(self, c: &WolCandidate) {
        let mac = c.mac.clone();
        self.macs.update(|m| {
            if !m.split(',').any(|x| x.trim() == mac) {
                *m = if m.trim().is_empty() { mac } else { format!("{mac}, {m}") };
            }
        });
        if self.name.with_untracked(|n| n.trim().is_empty()) {
            self.name.set(c.pc_device.clone().or_else(|| c.name.clone()).unwrap_or_default());
        }
        if let Some(ip) = &c.ip {
            self.host.set(ip.clone());
        }
        if let Some(pc) = &c.pc_device {
            self.pc.set(pc.clone());
        }
        if c.nic.as_ref().is_some_and(|n| n.wireless) {
            self.kind.set("laptop".into());
        }
    }

    /// The device, or `None` with the errors shown.
    fn build(self) -> Option<WolDevice> {
        let text = |s: RwSignal<String>| s.get_untracked().trim().to_string();
        let opt = |s: RwSignal<String>| Some(text(s)).filter(|v| !v.is_empty());
        let num = |s: RwSignal<String>| text(s).parse::<u32>().ok();
        let mut ok = true;
        self.other_err.set(None);
        self.host_err.set(None);
        let name = text(self.name);
        self.name_err.set(name.is_empty().then(|| t("wol.f.name_missing").to_string()));
        ok &= !name.is_empty();
        let raw: Vec<String> = text(self.macs).split([',', ' ', '\n']).map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect();
        let macs: Vec<Option<String>> = raw.iter().map(|m| normalize_mac(m)).collect();
        let bad: Vec<&str> = raw.iter().zip(&macs).filter(|(_, n)| n.is_none()).map(|(r, _)| r.as_str()).collect();
        let mac_err = if raw.is_empty() {
            Some(t("wol.f.macs_missing").to_string())
        } else if !bad.is_empty() {
            Some(tf("wol.f.macs_bad", &[("list", &bad.join(", "))]))
        } else {
            None
        };
        ok &= mac_err.is_none();
        self.macs_err.set(mac_err);
        let mut list: Vec<String> = Vec::new();
        for m in macs.into_iter().flatten() {
            if !list.contains(&m) {
                list.push(m);
            }
        }
        let secureon = opt(self.secureon);
        let s_bad = secureon.as_deref().is_some_and(|s| !secureon_ok(s));
        self.secureon_err.set(s_bad.then(|| t("wol.f.secureon_bad").to_string()));
        ok &= !s_bad;
        let ports_raw: Vec<String> = text(self.ports).split([',', ' ']).map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect();
        let ports: Vec<u16> = ports_raw.iter().filter_map(|p| p.parse::<u16>().ok().filter(|p| *p > 0)).collect();
        let p_bad = ports.len() != ports_raw.len();
        self.ports_err.set(p_bad.then(|| t("wol.f.ports_bad").to_string()));
        ok &= !p_bad;
        if !ok {
            ui::focus_invalid();
            return None;
        }
        Some(WolDevice {
            id: self.id.get_value(),
            name,
            kind: kind_of(&self.kind.get_untracked()),
            macs: list,
            host: opt(self.host),
            iface: opt(self.iface),
            port: num(self.port).and_then(|p| u16::try_from(p).ok()).unwrap_or(9),
            secureon,
            ports,
            wake_timeout_s: num(self.timeout).unwrap_or(DEFAULT_WAKE_TIMEOUT_S).clamp(30, 900),
            pc_device: opt(self.pc),
            users: self.users.get_untracked(),
            notes: text(self.notes),
        })
    }

    fn view(self, w: Wol) -> AnyView {
        let f = self;
        let pcs = Remote::<PcBackupOverview>::idle("/api/v1/pcbackup");
        let users = Remote::<Vec<UserView>>::idle("/api/v1/users");
        if !demo() {
            pcs.reload();
            users.reload();
        }
        // the NAS's interfaces, not every poll of the overview
        let ifaces = Memo::new(move |_| w.data.data.with(|d| d.as_ref().map(|o| o.ifaces.clone()).unwrap_or_default()));
        let iface_select = move || {
            let ifaces = ifaces.get();
            let cur = f.iface.get_untracked();
            let known = cur.is_empty() || ifaces.iter().any(|i| i.name == cur);
            view! {
                <div class="field">
                    <label class="field-label" for="wol-iface">{tr("wol.f.iface")}</label>
                    <select id="wol-iface" class="input" aria-describedby="wol-iface-h" on:change=move |ev| f.iface.set(event_target_value(&ev))>
                        <option value="" selected=cur.is_empty()>{tr("wol.f.iface_auto")}</option>
                        {ifaces.into_iter().map(|i| {
                            let sel = i.name == cur;
                            let label = if i.ipv4.is_empty() { i.name.clone() } else { format!("{} ({})", i.name, i.ipv4.join(", ")) };
                            view! { <option value=i.name.clone() selected=sel>{label}</option> }
                        }).collect_view()}
                        {(!known).then(|| view! { <option value=cur.clone() selected=true>{cur.clone()}</option> })}
                    </select>
                    <span class="field-help" id="wol-iface-h">{tr("wol.f.iface_help")}</span>
                    {f.field_note("iface")}
                </div>
            }
        };
        let pc_select = move || {
            let list: Vec<(String, String)> = pcs.data.with(|d| d.as_ref().map(|o| o.devices.iter().filter(|p| !p.revoked).map(|p| (p.device.clone(), p.hostname.clone())).collect()).unwrap_or_default());
            let cur = f.pc.get_untracked();
            let known = cur.is_empty() || list.iter().any(|p| p.0 == cur);
            view! {
                <div class="field">
                    <label class="field-label" for="wol-pc">{tr("wol.f.pc")}</label>
                    <select id="wol-pc" class="input" aria-describedby="wol-pc-h" on:change=move |ev| f.pc.set(event_target_value(&ev))>
                        <option value="" selected=cur.is_empty()>{tr("wol.f.pc_none")}</option>
                        {list.into_iter().map(|(d, h)| {
                            let sel = d == cur;
                            let label = if h.is_empty() || h == d { d.clone() } else { format!("{d} ({h})") };
                            view! { <option value=d selected=sel>{label}</option> }
                        }).collect_view()}
                        {(!known).then(|| view! { <option value=cur.clone() selected=true>{cur.clone()}</option> })}
                    </select>
                    <span class="field-help" id="wol-pc-h">{tr("wol.f.pc_help")}</span>
                    {f.field_note("pc_device")}
                </div>
            }
        };
        let users_view = move || {
            let list: Vec<String> = users.data.with(|d| d.as_ref().map(|u| u.iter().filter(|u| !u.user.admin).map(|u| u.user.name.clone()).collect()).unwrap_or_default());
            // users named before that are gone stay listed, so a save keeps them visible
            let mut all = list;
            for u in f.users.get_untracked() {
                if !all.contains(&u) {
                    all.push(u);
                }
            }
            view! {
                <fieldset class="field wol-users">
                    <legend class="field-label">{tr("wol.f.users")}</legend>
                    {if all.is_empty() {
                        view! { <p class="field-help">{tr("wol.f.users_none")}</p> }.into_any()
                    } else {
                        view! {
                            <div class="check-grid">
                                {all.into_iter().map(|u| {
                                    let name = u.clone();
                                    let on = { let u = u.clone(); move || f.users.with(|l| l.contains(&u)) };
                                    view! {
                                        <label class="check">
                                            <input type="checkbox" prop:checked=on on:change=move |ev| {
                                                let c = event_target_checked(&ev);
                                                let n = name.clone();
                                                f.users.update(|l| if c { if !l.contains(&n) { l.push(n) } } else { l.retain(|x| *x != n) });
                                            } />
                                            <span>{u}</span>
                                        </label>
                                    }
                                }).collect_view()}
                            </div>
                            <p class="field-help">{tr("wol.f.users_help")}</p>
                        }.into_any()
                    }}
                    {f.field_note("users")}
                </fieldset>
            }
        };
        let advanced = view! {
            {iface_select}
            <div class="form-row">
                {number_field("wol.f.port", Some("wol.f.port_help"), f.port.into(), Callback::new(move |v| f.port.set(v)), 0, 65535)}
                {number_field("wol.f.timeout", Some("wol.f.timeout_help"), f.timeout.into(), Callback::new(move |v| f.timeout.set(v)), 30, 900)}
            </div>
            {f.field_note("port")}
            {f.field_note("wake_timeout_s")}
            {text_field("wol.f.ports", f.ports.into(), Callback::new(move |v| { f.ports_err.set(None); f.ports.set(v); }), f.ports_err.into(), In { placeholder: "22, 445", help: Some("wol.f.ports_help"), ..In::default() })}
            {text_field("wol.f.secureon", f.secureon.into(), Callback::new(move |v| { f.secureon_err.set(None); f.secureon.set(v); }), f.secureon_err.into(), In { mono: true, help: Some("wol.f.secureon_help"), maxlength: 17, ..In::default() })}
        }
        .into_any();
        view! {
            {text_field("wol.f.name", f.name.into(), Callback::new(move |v| { f.name_err.set(None); f.name.set(v); }), f.name_err.into(), In { maxlength: 64, ..In::default() })}
            {select_field("wol.f.kind", None, KINDS, f.kind.into(), Callback::new(move |v| f.kind.set(v)))}
            {text_field("wol.f.macs", f.macs.into(), Callback::new(move |v| { f.macs_err.set(None); f.macs.set(v); }), f.macs_err.into(), In { mono: true, placeholder: "a8:a1:59:00:00:01", help: Some("wol.f.macs_help"), maxlength: 200, ..In::default() })}
            {text_field("wol.f.host", f.host.into(), Callback::new(move |v| { f.host_err.set(None); f.host.set(v); }), f.host_err.into(), In { mono: true, placeholder: "192.0.2.20", help: Some("wol.f.host_help"), ..In::default() })}
            {pc_select}
            {users_view}
            {area_field("wol.f.notes", f.notes.into(), Callback::new(move |v| f.notes.set(v)), None, "")}
            {f.field_note("notes")}
            // opened by an error inside it, else as the user leaves it
            <details class="fold" open=move || f.adv_open.get() on:toggle=move |ev| {
                let open = event_target::<web_sys::Element>(&ev).has_attribute("open");
                if f.adv_open.get_untracked() != open {
                    f.adv_open.set(open);
                }
            }>
                <summary><Icon icon=icons::CHEVRON_RIGHT size=14 /><span class="fold-title">{tr("wol.f.advanced")}</span></summary>
                <div class="fold-body">{advanced}</div>
            </details>
        }
        .into_any()
    }
}

/// Settings of a device (`Some`) or the form of a new one (`None`, from Add device with
/// `init`).
fn save(w: Wol, f: Form, busy: RwSignal<bool>, error: RwSignal<Option<String>>, close: Callback<()>) {
    let Some(d) = f.build() else { return };
    if demo() {
        toast(Tone::Ok, tf("wol.saved", &[("name", &d.name)]));
        close.try_run(());
        return;
    }
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = if d.id.is_empty() { api::post::<_, WolDevice>(&format!("{URL}/devices"), Some(&d)).await } else { api::put::<_, WolDevice>(&dev_url(&d.id), &d).await };
        let _ = busy.try_set(false);
        match r {
            Ok(saved) => {
                toast(Tone::Ok, tf("wol.saved", &[("name", &saved.name)]));
                w.data.reload();
                close.try_run(());
            }
            Err(e) => {
                // a field the server refused shows its error there only
                if !f.server_error(e.field(), e.text()) {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        }
    });
}

fn settings_dialog(w: Wol, d: Option<WolDevice>, close: Callback<()>) -> AnyView {
    let d = d.unwrap_or_default();
    let f = Form::new(&d);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let body = view! { {f.view(w)}{form_error(error)} }.into_any();
    let foot = save_bar_with(SaveBar::new(busy, "save", close, Callback::new(move |_| save(w, f, busy, error, close))));
    ui::dialog(DlgKind::Normal, Signal::stored(tf("wol.settings_title", &[("name", &d.name)])), close, body, Some(foot))
}

// ------------------------------------------------------------------ Add device

const SOURCES: &[(&str, &str)] = &[("agents", "wol.src.agents"), ("network", "wol.src.network"), ("manual", "wol.src.manual")];

pub(super) fn add_dialog(w: Wol, init: WolDevice) -> AnyView {
    let close = Callback::new(move |_| w.add.set(None));
    let f = Form::new(&init);
    let cands = Remote::<Vec<WolCandidate>>::idle(format!("{URL}/discover"));
    #[cfg(feature = "demo")]
    if demo() {
        cands.data.set(Some(crate::demo::wol_candidates()));
    }
    if !demo() {
        cands.reload();
    }
    // a PC named by the link of its backup settings: straight to the form, its wired card
    // filled in as soon as the agent's cards are read
    let for_pc = init.pc_device.clone();
    let stage_form = RwSignal::new(for_pc.is_some());
    let source = RwSignal::new("agents".to_string());
    if let Some(pc) = for_pc {
        Effect::new(move |done: Option<bool>| {
            if done == Some(true) {
                return true;
            }
            cands.data.with(|c| {
                let c = c.as_ref()?;
                let best = c.iter().filter(|c| c.source == WolSource::Agent && c.pc_device.as_deref() == Some(pc.as_str()) && c.known.is_none()).min_by_key(|c| c.nic.as_ref().is_some_and(|n| n.wireless))?;
                f.take(best);
                Some(())
            });
            cands.data.with(Option::is_some)
        });
    }
    let pick = move |c: WolCandidate| {
        f.take(&c);
        stage_form.set(true);
    };
    let wol_state = |n: &AgentNic| match (n.wol_supported, n.wol_enabled) {
        (Some(false), _) => ui::status(Tone::Warn, t("wol.health.not_supported").to_string()),
        (_, Some(true)) => ui::status(Tone::Ok, t("wol.health.on").to_string()),
        (_, Some(false)) => ui::status(Tone::Warn, t("wol.health.off").to_string()),
        _ => ui::status(Tone::Unknown, t("wol.health.unknown").to_string()),
    };
    let list = move || {
        let src = source.get();
        if src == "manual" {
            return view! {
                <p class="small muted">{tr("wol.src.manual_text")}</p>
                <div class="sec-actions"><button type="button" class="btn primary sm" on:click=move |_| stage_form.set(true)><Icon icon=icons::PENCIL size=14 />{tr("wol.src.manual_go")}</button></div>
            }
            .into_any();
        }
        cands.view(3, move || {
            let want = if src == "agents" { WolSource::Agent } else { WolSource::Network };
            let all = cands.data.get().unwrap_or_default();
            let mine: Vec<WolCandidate> = all.into_iter().filter(|c| c.source == want).collect();
            if mine.is_empty() {
                return ui::empty_line(if want == WolSource::Agent { "wol.src.no_agents" } else { "wol.src.no_network" });
            }
            let rows = mine
                .into_iter()
                .map(|c| {
                    let known = c.known.is_some();
                    let label = ui::verb_name("wol.use", &c.mac);
                    let cc = c.clone();
                    let use_btn = view! { <button type="button" class="btn ghost sm" aria-label=label disabled=known on:click=move |_| pick(cc.clone())>{tr("wol.use")}</button> }.into_any();
                    let mac_cell = view! {
                        <div class="cell-main">
                            <span class="mono wol-mac">{c.mac.clone()}</span>
                            {known.then(|| ui::chip(Tone::Info, view! { {tr("wol.src.known")} }.into_any()))}
                        </div>
                    }
                    .into_any();
                    if want == WolSource::Agent {
                        let nic = c.nic.clone().unwrap_or_default();
                        // the card: its name and kind, then its MAC (one column less: no
                        // MAC split over two lines in the dialog)
                        let card = view! {
                            <div class="cell-main">
                                <span><span class="mono">{nic.name.clone()}</span>" · "{if nic.wireless { t("wol.wifi") } else { t("wol.wired") }}</span>
                                <span class="cell-sub small mono wol-mac">{c.mac.clone()}</span>
                                {known.then(|| ui::chip(Tone::Info, view! { {tr("wol.src.known")} }.into_any()))}
                            </div>
                        }
                        .into_any();
                        ui::row(vec![
                            ui::text_cell("wol.col.pc", "", c.pc_device.clone().or(c.name.clone()).unwrap_or_default()),
                            ui::cell("wol.col.card", "wrap", card),
                            ui::cell("wol.col.wol", "", wol_state(&nic)),
                            ui::actions(use_btn),
                        ])
                    } else {
                        let who = view! {
                            <div class="cell-main">
                                <span class="mono">{c.ip.clone().unwrap_or_else(|| ui::DASH.to_string())}</span>
                                {c.name.clone().map(|n| view! { <span class="cell-sub small">{n}</span> })}
                            </div>
                        }
                        .into_any();
                        ui::row(vec![
                            ui::cell("wol.col.address", "wrap", who),
                            ui::cell("net.col_mac", "wrap", mac_cell),
                            ui::cell("wol.col.iface", "", view! { <span class="mono small">{c.iface.clone().unwrap_or_else(|| ui::DASH.to_string())}</span> }.into_any()),
                            ui::actions(use_btn),
                        ])
                    }
                })
                .collect::<Vec<_>>();
            if want == WolSource::Agent {
                ui::table("wol-cands", &["wol.col.pc", "wol.col.card", "wol.col.wol", ""], rows)
            } else {
                view! {
                    <p class="small muted">{tr("wol.src.network_text")}</p>
                    {ui::table("wol-cands", &["wol.col.address", "net.col_mac", "wol.col.iface", ""], rows)}
                }
                .into_any()
            }
        })
    };
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let body = view! {
        {move || if stage_form.get() {
            view! { {f.view(w)}{form_error(error)} }.into_any()
        } else {
            view! {
                <p class="small muted">{tr("wol.add_intro")}</p>
                {segmented("wol.src", SOURCES, source.into(), Callback::new(move |v| source.set(v)))}
                {list}
            }
            .into_any()
        }}
    }
    .into_any();
    let foot = move || {
        if stage_form.get() {
            save_bar_with(SaveBar {
                icon: Some(icons::PLUS),
                secondary: Some(view! { <button type="button" class="btn ghost" on:click=move |_| stage_form.set(false)>{tr("back")}</button> }.into_any()),
                ..SaveBar::new(busy, "wol.add", close, Callback::new(move |_| save(w, f, busy, error, close)))
            })
        } else {
            ui::close_foot(close)
        }
    };
    ui::dialog(DlgKind::Normal, Signal::derive(move || t("wol.add").to_string()), close, body, Some(foot.into_any()))
}
