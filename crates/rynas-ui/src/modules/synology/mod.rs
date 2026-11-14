//! The Synology module of the web interface (D-MODULE): the Hardware › Synology tab
//! ([`page`]), its presence check ([`present`]), its texts ([`i18n`]: the tab, synodeb's event
//! codes, the UPS texts of synodeb's layer) and the reset button status of Network › Rescue
//! ([`reset_button`]). Nothing here is shown on other hardware.

pub mod i18n;
mod led;
mod page;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::network::ResetButton;

use super::UpsSlot;
use crate::i18n::{t, tr};
use crate::icons::{self, Icon};
use crate::live::use_live;
use crate::ui::{self, Tone};

pub use page::panel;

/// Label key of the Hardware tab.
pub const TAB: &str = "syn.tab";

/// The module is present: the live provider is synohwd's, or (synohwd stopped, no status yet)
/// the module's settings answer. Probed once per visit, only when the provider does not
/// already tell. A memo: the hardware status changes with every reading, and a derived signal
/// would build the whole tab again each time.
pub fn present() -> Memo<bool> {
    let hw = use_live().hardware;
    let probed = RwSignal::new(false);
    let asked = StoredValue::new(false);
    Effect::new(move || {
        let Some(provider) = hw.with(|h| h.as_ref().map(|h| h.provider.clone())) else { return };
        if provider.as_deref() == Some("synology") || asked.get_value() {
            return;
        }
        asked.set_value(true);
        spawn_local(async move {
            let ok = crate::api::get_text("/api/v1/modules/synology/settings").await.is_ok();
            probed.set(ok);
        });
    });
    Memo::new(move |_| probed.get() || hw.with(|h| h.as_ref().is_some_and(|h| h.provider.as_deref() == Some("synology"))))
}

/// synodeb's UPS layer: Safe Mode instead of a shutdown, synodeb applies the driver choice.
pub fn ups_key(slot: UpsSlot) -> &'static str {
    match slot {
        UpsSlot::NoneText => "syn.ups_none",
        UpsSlot::DownText => "syn.ups_down",
        UpsSlot::LbAlways => "syn.ups_lb_always",
        UpsSlot::Timer => "syn.ups_timer",
        UpsSlot::TimerHelp => "syn.ups_timer_help",
        UpsSlot::PreTimeoutHelp => "syn.ups_pre_timeout_help",
        UpsSlot::TestLb => "syn.ups_test_lb",
        UpsSlot::DeepText => "syn.ups_deep_text",
        UpsSlot::SetupTooOld => "syn.upsw_too_old",
        UpsSlot::SetupFind => "syn.upsw_find_text",
        UpsSlot::SetupCommands => "syn.upsw_commands",
        UpsSlot::NetTooOld => "syn.upsn_too_old",
        UpsSlot::NetListen => "syn.upsn_listen",
        UpsSlot::NetAllowHelp => "syn.upsn_allow_help",
        UpsSlot::NetFilter => "syn.upsn_filter",
        UpsSlot::NetFilterInactive => "syn.upsn_filter_inactive",
        UpsSlot::NetUnreadable => "syn.upsn_unreadable",
        UpsSlot::NetPrimary => "syn.upsn_primary",
        UpsSlot::NetManual => "syn.upsn_manual",
        UpsSlot::NetOutside => "syn.upsn_outside",
        UpsSlot::NetRoleMonitorHelp => "syn.upsn_role_monitor_help",
        UpsSlot::NetUserHelp => "syn.upsn_user_help",
        UpsSlot::NetUserBad => "syn.upsn_user_bad",
        UpsSlot::NetCommands => "syn.upsn_commands",
    }
}

/// synodeb's user names for its network UPS server: `[a-z][a-z0-9_]{0,31}`, not `monuser` or
/// `admin` (synodeb checks the same).
pub fn net_user_ok(u: &str) -> bool {
    let b = u.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        && !["monuser", "admin"].contains(&u)
}

/// Every synodeb sender (`synodeb:<source>`) is one neutral label.
pub fn source_label(source: &str) -> Option<&'static str> {
    source.starts_with("synodeb:").then(|| t("nt.src.synodeb"))
}

/// The reset button's network rescue (a `synology.hw` setting, shown read-only on Network ›
/// Rescue where the hardware has the button).
pub fn reset_button(r: &ResetButton) -> AnyView {
    let (tone, key) = if r.configured { (Tone::Ok, "syn.reset_on") } else { (Tone::Warn, "syn.reset_off") };
    let pending = r.pending.map(|p| ui::status(Tone::Info, t(if p { "syn.reset_pending_on" } else { "syn.reset_pending_off" }).to_string()));
    view! {
        <div class="chips">{ui::status(tone, t(key).to_string())}{pending}</div>
        <p class="muted small">{tr("syn.reset_text")}</p>
        <p class="sec-note"><Icon icon=icons::LOCK size=16 /><span>{tr("syn.reset_owner")}" "<a class="link-btn" href="/hardware">{tr("syn.reset_owner_link")}</a></span></p>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{self as i18n, Lang};

    #[test]
    fn every_slot_has_a_text() {
        use UpsSlot::*;
        for s in [
            NoneText, DownText, LbAlways, Timer, TimerHelp, PreTimeoutHelp, TestLb, DeepText, SetupTooOld, SetupFind, SetupCommands, NetTooOld, NetListen,
            NetAllowHelp, NetFilter, NetFilterInactive, NetUnreadable, NetPrimary, NetManual, NetOutside, NetRoleMonitorHelp, NetUserHelp, NetUserBad,
            NetCommands,
        ] {
            assert!(i18n::text_in(Lang::EN, ups_key(s)).is_some(), "{s:?}");
        }
        assert!(source_label("smartd").is_none());
        assert!(net_user_ok("pc_1") && !net_user_ok("pc-1") && !net_user_ok("1pc") && !net_user_ok("monuser"));
    }
}
