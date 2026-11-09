//! The module's API documents (`/api/v1/modules/qnap/...`), shared with the web interface.

use serde::{Deserialize, Serialize};

/// What the status LED shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StatusLed {
    /// Follows the state of the disks: green, red blinking on a warning, red on a failure.
    #[default]
    Auto,
    Green,
    GreenBlink,
    Red,
    RedBlink,
    /// Green and red in turn.
    Alternate,
    Off,
}

/// The USB LED (the vendor's modes: on, off, blinking).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UsbLed {
    #[default]
    Off,
    On,
    Blink,
}

/// What a button does when pressed. Never anything destructive: the choices end here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ButtonAction {
    /// Press recorded in the module's event list only.
    Log,
    /// Recorded and sent as a notification.
    #[default]
    Notify,
    /// Not even recorded.
    Ignore,
}

/// `qnap.hw`, the module's configuration key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QnapSettings {
    /// Whether rynas may drive the LED lines at all. Off until the model's mapping was checked
    /// on the machine (the first-boot procedure of `docs/plugins/qnap.md`).
    pub led_control: bool,
    pub status_led: StatusLed,
    /// The bay error LEDs follow the disks' state.
    pub disk_leds: bool,
    pub usb_led: UsbLed,
    pub reset_button: ButtonAction,
    pub usb_copy_button: ButtonAction,
    /// A failed disk sounds the buzzer, repeated until muted.
    pub beep_on_disk_failure: bool,
}

impl Default for QnapSettings {
    fn default() -> Self {
        Self {
            led_control: false,
            status_led: StatusLed::Auto,
            disk_leds: true,
            usb_led: UsbLed::Off,
            reset_button: ButtonAction::Notify,
            usb_copy_button: ButtonAction::Notify,
            beep_on_disk_failure: true,
        }
    }
}

/// `GET /api/v1/modules/qnap/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct QnapSettingsState {
    /// In force.
    pub applied: QnapSettings,
    /// This user's staged value, when it differs.
    pub pending: Option<QnapSettings>,
}

/// One hardware function and whether it can be used now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feature {
    /// `status_led`, `disk_leds`, `usb_led`, `reset_button`, `usb_copy_button`, `buzzer`,
    /// `temperatures`.
    pub id: String,
    pub available: bool,
    /// Why not, as a stable code the interface translates (empty when available):
    /// `driver_missing`, `wrong_direction`, `led_control_off`, `no_speaker`.
    pub reason: String,
}

/// A bay and the state its error LED shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BayLed {
    pub bay: u32,
    /// Kernel name of the disk (`sda`), empty for an empty bay.
    pub device: String,
    /// `off`, `on` (failure), `blink` (warning).
    pub led: String,
}

/// A button press.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonEvent {
    /// `reset` or `usb_copy`.
    pub button: String,
    /// Seconds since the epoch.
    pub at: u64,
    pub held_ms: u32,
}

/// `GET /api/v1/modules/qnap/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct QnapStatus {
    /// `QNAP HS-251`.
    pub model: String,
    /// The line mapping of this model was checked on real hardware. False: it comes from
    /// the vendor's configuration and the driver's source only.
    pub verified: bool,
    pub features: Vec<Feature>,
    /// What the status LED shows now: `green`, `green-blink`, `red`, `red-blink`,
    /// `alternate`, `off`, empty while rynas does not drive it.
    pub status_led: String,
    pub bays: Vec<BayLed>,
    /// `off`, `on`, `blink`, empty while rynas does not drive it.
    pub usb_led: String,
    /// Newest last.
    pub events: Vec<ButtonEvent>,
    /// The disk-failure alarm is sounding.
    pub alarm: bool,
}

/// The tone patterns of the vendor's buzzer modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BeepKind {
    #[default]
    Short,
    Long,
    /// Three short beeps, one second apart.
    ThreeShort,
    /// Three quick short beeps.
    RapidThree,
    TwoLong,
    ThreeLong,
    /// One long and two short.
    LongTwoShort,
}

/// `POST /api/v1/modules/qnap/beep`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BeepRequest {
    pub kind: BeepKind,
}

/// `POST /api/v1/modules/qnap/mute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MuteResult {
    pub stopped_beep: bool,
    /// Open hardware notifications acknowledged.
    pub acknowledged: u64,
}
