//! Hardware modules of the web interface (D-MODULE, `docs/phase0/05-decisions.md`).
//!
//! The core pages work on any Debian and name no platform. A hardware module (Synology today)
//! brings its own pages, tabs and texts (its own dictionary table, read by `crate::i18n`),
//! all in its folder here, and they are shown only where the module answers: its provider in
//! the live hardware status, its endpoints, or a module backend in an API state. The core
//! pages reach a module only through the slots below.

pub mod synology;

/// Texts of the UPS page and wizard that belong to the module's UPS layer when the backend is
/// `UpsBackend::Module` (the core keys are NUT's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpsSlot {
    /// No UPS reported by the UPS service.
    NoneText,
    /// Nothing answers on the NUT port.
    DownText,
    /// What always happens on low battery.
    LbAlways,
    /// The timer switch.
    Timer,
    TimerHelp,
    /// The help of the time to stop work.
    PreTimeoutHelp,
    /// Low battery during a test.
    TestLb,
    /// The deep test's warning.
    DeepText,
    /// The layer cannot set up drivers.
    SetupTooOld,
    /// The wizard's first step.
    SetupFind,
    /// Above the commands of the plan.
    SetupCommands,
}

/// The text key of a UPS slot with a module's UPS layer (one module has one today; the API
/// will name the module when a second one brings a UPS layer).
pub fn ups_key(slot: UpsSlot) -> &'static str {
    synology::ups_key(slot)
}

/// The label of a notification source sent by a module's tools, if it is one.
pub fn source_label(source: &str) -> Option<&'static str> {
    synology::source_label(source)
}
