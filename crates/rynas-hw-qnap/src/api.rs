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
    #[default]
    Log,
    /// Recorded and sent as a notification.
    Notify,
    /// Not even recorded.
    Ignore,
}

/// `qnap.hw`, the module's configuration key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QnapSettings {
    /// Whether rynas may drive the LED lines at all. On a model not verified on hardware it
    /// also needs the separate confirmation of `POST confirm-mapping`.
    pub led_control: bool,
    pub status_led: StatusLed,
    /// Brightness of the front LEDs: the vendor's three levels, 0, 50 or 100 percent.
    pub led_brightness: u8,
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
            led_brightness: 100,
            disk_leds: true,
            usb_led: UsbLed::Off,
            // only listed until the model is verified: an unstable line must not notify anyone
            reset_button: ButtonAction::Log,
            usb_copy_button: ButtonAction::Log,
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
    /// `status_led`, `led_brightness`, `disk_leds`, `usb_led`, `reset_button`,
    /// `usb_copy_button`, `buzzer`.
    pub id: String,
    pub available: bool,
    /// Why not, as a stable code the interface translates (empty when available):
    /// `driver_missing`, `wrong_direction`, `mapping_unconfirmed`, `led_control_off`,
    /// `write_failed`, `no_speaker`, `not_fitted` (the case has no such LED).
    pub reason: String,
}

/// A bay and the state its error LED shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BayLed {
    pub bay: u32,
    /// Kernel name of the disk (`sda`), empty for an empty bay.
    pub device: String,
    /// What the disk's state asks of the LED: `off`, `on` (failure), `blink` (warning).
    pub led: String,
    /// rynas has written this bay's line (control on and confirmed, line usable). It says what
    /// was commanded, not that the LED visibly changed: see `gpio_active` of the power recovery
    /// state.
    pub driven: bool,
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
    /// What the user must type to confirm the mapping: `HS-251`.
    pub short_model: String,
    /// The line mapping of this model was checked on real hardware. False: it comes from
    /// the vendor's configuration and the driver's source only.
    pub verified: bool,
    /// The user confirmed the mapping on this machine (always true on a verified model).
    pub mapping_confirmed: bool,
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

/// `POST /api/v1/modules/qnap/confirm-mapping`: the user typed the model's name after the
/// first-boot procedure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ConfirmMapping {
    pub typed: String,
}

/// What the machine does when mains power comes back after an outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerRecovery {
    /// Back to the state before the outage.
    LastState,
    /// Always start.
    PowerOn,
    /// Stay off.
    PowerOff,
}

/// `GET /api/v1/modules/qnap/power-recovery`: read from the hardware at this moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PowerRecoveryState {
    /// `None`: the register holds a value the vendor's code gives no meaning to.
    pub mode: Option<PowerRecovery>,
    /// Bits 2:1 of register 0xF4, logical device 0x0A.
    pub sio_bits: u8,
    /// CMOS byte 0x61, the BIOS's copy.
    pub cmos: u8,
    /// EuP is on: the vendor's interface disables this setting then, and so does rynas.
    pub eup: bool,
    /// The GPIO logical device of the Super I/O is activated (LDN 6, register 0x30).
    pub gpio_active: bool,
}

/// `PUT /api/v1/modules/qnap/power-recovery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerRecoveryRequest {
    pub mode: PowerRecovery,
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
