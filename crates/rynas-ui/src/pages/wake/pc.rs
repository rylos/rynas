//! Wake-on-LAN on the PC backup page: Wake (and Sleep) next to a PC's actions when a device is
//! linked to it ([`WolDevice::pc_device`]), and the wake settings of its backups
//! ([`PcPlan::wake_for_backup`], [`PcPlan::after_wake`]).

use leptos::prelude::*;
use rynas_api_types::pcagent::{AfterWake, PcPlan};

use super::{Wol, sleep_menu, wake_button, wake_line};
use crate::form::{segmented, switch};
use crate::i18n::{t, tr};
use crate::icons;
use crate::ui::{self, Tone};

/// The Wake-on-LAN state of a PC's panel: the overview, read once (`start`) and followed while
/// a wake runs.
#[derive(Clone, Copy)]
pub struct PcWake {
    w: Wol,
    pc: StoredValue<String>,
}

impl PcWake {
    /// Nothing is read until [`PcWake::start`].
    pub fn new(pc: String) -> Self {
        PcWake { w: Wol::new(false, false), pc: StoredValue::new(pc) }
    }

    pub fn start(self) {
        if !super::demo() {
            self.w.data.reload();
        }
    }

    /// A device is linked to the PC (tracked; `false` until read).
    pub fn linked(self) -> bool {
        self.w.of_pc(&self.pc.get_value()).is_some()
    }

    /// Wake and Power in a row of their own (nothing without a linked device), with the
    /// progress under it: built once by the panel, outside what its polls rebuild (an open
    /// Power menu stays open).
    pub fn row(self) -> AnyView {
        let id = Memo::new(move |_| self.w.of_pc(&self.pc.get_value()).map(|v| v.device.id));
        view! {
            {move || id.get().map(|id| view! { <div class="sec-actions">{wake_button(self.w, id.clone())}{sleep_menu(self.w, id)}</div> })}
            {self.line()}
        }
        .into_any()
    }

    /// The power commands the linked PC's agent takes, `None` without a linked agent that
    /// takes them (tracked).
    fn modes(self) -> Option<Vec<rynas_api_types::pcagent::SleepMode>> {
        self.w.of_pc(&self.pc.get_value()).filter(|v| v.agent.as_ref().is_some_and(|a| a.power_commands)).map(|v| super::power_modes(&v))
    }

    /// The progress of a wake (under the action row).
    pub fn line(self) -> AnyView {
        let id = Memo::new(move |_| self.w.of_pc(&self.pc.get_value()).map(|v| v.device.id));
        let nav = StoredValue::new_local(leptos_router::hooks::use_navigate());
        (move || {
            id.get().map(|id| {
                let to = id.clone();
                let open = Callback::new(move |_| {
                    nav.with_value(|nav| nav(&format!("/wake?device={}", crate::api::encode(&to)), Default::default()));
                });
                wake_line(self.w, id, Some(open))
            })
        })
        .into_any()
    }
}

const AFTER: &[(&str, &str)] = &[("stay", "wol.after.stay"), ("suspend", "wol.sleep.suspend"), ("poweroff", "sysp.shutdown")];
const AFTER_SUSPEND: &[(&str, &str)] = &[("stay", "wol.after.stay"), ("suspend", "wol.sleep.suspend")];
const AFTER_OFF: &[(&str, &str)] = &[("stay", "wol.after.stay"), ("poweroff", "sysp.shutdown")];
const AFTER_STAY: &[(&str, &str)] = &[("stay", "wol.after.stay")];

/// "Wake the PC for scheduled backups" and what to do after such a backup, in a PC's backup
/// settings.
#[derive(Clone, Copy)]
pub struct WakeSettings {
    wake: RwSignal<bool>,
    after: RwSignal<String>,
    pcw: PcWake,
}

impl WakeSettings {
    pub fn new(plan: &PcPlan, pc: &str) -> Self {
        let after = match plan.after_wake {
            AfterWake::Stay | AfterWake::Unknown => "stay",
            AfterWake::Suspend => "suspend",
            AfterWake::Poweroff => "poweroff",
        };
        let pcw = PcWake::new(pc.to_string());
        pcw.start();
        WakeSettings { wake: RwSignal::new(plan.wake_for_backup), after: RwSignal::new(after.to_string()), pcw }
    }

    pub fn apply(self, plan: &mut PcPlan) {
        plan.wake_for_backup = self.wake.get_untracked();
        plan.after_wake = match self.after.get_untracked().as_str() {
            "suspend" => AfterWake::Suspend,
            "poweroff" => AfterWake::Poweroff,
            _ => AfterWake::Stay,
        };
    }

    pub fn view(self) -> AnyView {
        let s = self;
        let ready = move || s.pcw.w.data.ready();
        let pc = s.pcw.pc;
        // the choices the linked agent can carry out (all of them while that is unknown)
        let opts = Memo::new(move |_| {
            use rynas_api_types::pcagent::SleepMode;
            match s.pcw.modes() {
                None => AFTER,
                Some(m) => match (m.contains(&SleepMode::Suspend), m.contains(&SleepMode::Poweroff)) {
                    (true, true) => AFTER,
                    (true, false) => AFTER_SUSPEND,
                    (false, true) => AFTER_OFF,
                    (false, false) => AFTER_STAY,
                },
            }
        });
        view! {
            <h3 class="mini-title">{tr("wol.pc.title")}</h3>
            {switch("wol.pc.wake", Some("wol.pc.wake_help"), s.wake.into(), Callback::new(move |v| s.wake.set(v)))}
            <Show when=move || s.wake.get()>
                {move || {
                    let o = opts.get();
                    // a choice the agent cannot do any more goes back to "Stay on"
                    if !o.iter().any(|(v, _)| s.after.with_untracked(|a| a == v)) {
                        s.after.set("stay".into());
                    }
                    segmented("wol.pc.after", o, s.after.into(), Callback::new(move |v| s.after.set(v)))
                }}
                <p class="field-help">{tr("wol.pc.after_help")}</p>
            </Show>
            // in a new tab: the settings being edited here stay as they are
            {move || (ready() && !s.pcw.linked()).then(|| ui::note(Tone::Info, icons::POWER, view! {
                <span>{t("wol.pc.not_linked")}" "</span>
                <a href=format!("/wake?add={}", crate::api::encode(&pc.get_value())) target="_blank" rel="noopener">{tr("wol.pc.add_link")}</a>
                <span class="small muted">" "{tr("wol.pc.add_hint")}</span>
            }.into_any()))}
        }
        .into_any()
    }
}
