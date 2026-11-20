//! Wake-on-LAN and device power (`/api/v1/wol`, `docs/research/wake-on-lan.md`).
//!
//! The devices live in the config key `wol` ([`WolConfig`]); their status and wake history are
//! runtime state of rynasd. Sleep and shutdown go only through a paired PC agent.

use serde::{Deserialize, Serialize};

use crate::pcagent::{AgentNic, SleepMode};

/// Config key of [`WolConfig`].
pub const WOL_KEY: &str = "wol";

/// Default wake timeout (seconds).
pub const DEFAULT_WAKE_TIMEOUT_S: u32 = 180;

/// The config key `wol`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WolConfig {
    #[serde(default)]
    pub devices: Vec<WolDevice>,
}

/// What a device is (icon and wording only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WolKind {
    #[default]
    Desktop,
    Laptop,
    Server,
    Media,
    Other,
}

/// One device that can be woken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WolDevice {
    /// Stable id (`[a-z0-9-]`, 1–32), made by the server from the name when created empty.
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: WolKind,
    /// MAC addresses (`aa:bb:cc:dd:ee:ff`, lower case), the one to wake first. Every one
    /// receives the packet. At least one.
    pub macs: Vec<String>,
    /// Last known IPv4 address or host name: the status check and the choice of the interface.
    /// Refreshed from the agent and the neighbor table when the device has none set by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// NAS interface to send on; `None`: the one whose subnet holds `host`, else all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iface: Option<String>,
    /// UDP port of the magic packet (9 by default; 7 and 0 are common too).
    #[serde(default = "default_port")]
    pub port: u16,
    /// SecureOn password, 6 bytes as `aa:bb:cc:dd:ee:ff` (or 4 as `a.b.c.d`). It travels in
    /// clear text: it only stops a stray packet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secureon: Option<String>,
    /// TCP ports whose answer means "on" (a device that drops ping: 22, 445, 3389…).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
    /// Seconds to wait for the device after a wake (30–900).
    #[serde(default = "default_timeout")]
    pub wake_timeout_s: u32,
    /// The PC backup device (paired agent) this is: status from the agent, sleep and shutdown
    /// through it, MACs and IP from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pc_device: Option<String>,
    /// Users (not administrators, who always can) who may see and wake it, and put it to
    /// sleep when it has an agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

fn default_port() -> u16 {
    9
}

fn default_timeout() -> u32 {
    DEFAULT_WAKE_TIMEOUT_S
}

impl Default for WolDevice {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: WolKind::default(),
            macs: Vec::new(),
            host: None,
            iface: None,
            port: default_port(),
            secureon: None,
            ports: Vec::new(),
            wake_timeout_s: default_timeout(),
            pc_device: None,
            users: Vec::new(),
            notes: String::new(),
        }
    }
}

/// A device's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WolStatus {
    Online,
    Offline,
    /// A wake is running.
    Waking,
    /// Nothing to check (no host and no agent).
    #[default]
    Unknown,
}

/// What said the device is on (strongest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WolProbe {
    /// The paired agent is in contact.
    Agent,
    /// A TCP port of [`WolDevice::ports`] answered.
    Port,
    Ping,
    /// The neighbor table holds it as reachable.
    Neighbor,
}

/// Why a device may not wake (shown in its panel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WolWarning {
    /// The MAC to wake is a Wi-Fi card (most cannot be woken).
    Wireless,
    /// The agent says the wired card does not support magic packets.
    Unsupported,
    /// The agent says Wake-on-LAN is turned off on the card ("Enable on this PC").
    Disabled,
    /// No NAS interface is on the device's subnet: the packet goes out on every interface and
    /// a router does not forward it.
    NoRoute,
    /// The MAC is locally administered (often a randomized Wi-Fi MAC that changes).
    RandomMac,
}

/// How a wake ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WolWakeEnd {
    /// The device answered.
    Online,
    /// It was already on.
    AlreadyOn,
    /// It did not answer within the timeout.
    Timeout,
    /// The packet could not be sent (no interface, socket error).
    SendFailed,
    /// No status check is possible: the packet went out, nothing confirms it.
    Unconfirmed,
}

/// Who started a wake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WolTrigger {
    User { name: String },
    /// A scheduled PC backup was due (wake for backups).
    Backup,
}

/// One wake of the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WolWake {
    pub at_ms: u64,
    pub by: WolTrigger,
    pub end: WolWakeEnd,
    /// Seconds until the device answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<u32>,
    /// Interfaces the packet went out on.
    #[serde(default)]
    pub ifaces: Vec<String>,
    /// Task of the wake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upid: Option<String>,
}

/// The paired agent of a device, as it last reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WolAgent {
    pub agent_version: String,
    #[serde(default)]
    pub nics: Vec<AgentNic>,
    /// What it can do on request (empty: an agent older than 0.8.26).
    #[serde(default)]
    pub sleep: Vec<SleepMode>,
    /// A backup is running (sleep is refused).
    #[serde(default)]
    pub busy: bool,
    /// A sleep or enable command waits for the agent's next contact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
}

/// One device with its state, as the caller may see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WolDeviceView {
    pub device: WolDevice,
    pub status: WolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<WolProbe>,
    /// Since when the status holds (ms), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// The IPv4 address the device was last seen with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// The interface a wake would use (`None`: all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_iface: Option<String>,
    /// The running wake's task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waking: Option<String>,
    /// Newest first, at most 20.
    #[serde(default)]
    pub history: Vec<WolWake>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<WolAgent>,
    #[serde(default)]
    pub warnings: Vec<WolWarning>,
    /// The caller may edit and delete it (administrator).
    #[serde(default)]
    pub can_manage: bool,
}

/// A NAS interface a wake can go out on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WolIface {
    pub name: String,
    /// IPv4 addresses with prefix (`192.0.2.10/24`).
    #[serde(default)]
    pub ipv4: Vec<String>,
    pub up: bool,
}

/// `GET /api/v1/wol`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WolOverview {
    pub devices: Vec<WolDeviceView>,
    /// Empty for non-administrators.
    #[serde(default)]
    pub ifaces: Vec<WolIface>,
}

/// Where a candidate of "Add device" was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WolSource {
    /// A paired PC agent reported this card.
    Agent,
    /// The NAS's neighbor table.
    Network,
}

/// `GET /api/v1/wol/discover`: one card that could become a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WolCandidate {
    pub mac: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// Reverse DNS or the agent's host name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// NAS interface it was seen on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iface: Option<String>,
    pub source: WolSource,
    /// The PC backup device (agent source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pc_device: Option<String>,
    /// The agent's card facts (agent source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nic: Option<AgentNic>,
    /// Id of the device that already has this MAC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known: Option<String>,
}

/// `POST /api/v1/wol/devices/{id}/wake` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WolWakeStarted {
    pub upid: String,
    /// A wake was already running: this is its task.
    #[serde(default)]
    pub joined: bool,
}

/// `POST /api/v1/wol/devices/{id}/sleep`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WolSleepRequest {
    pub mode: SleepMode,
}

/// Normalizes a MAC (`AA-BB-CC-DD-EE-FF`, `aabb.ccdd.eeff`, `aabbccddeeff`) to
/// `aa:bb:cc:dd:ee:ff`; `None` when it is not 6 bytes, all zero, broadcast or multicast.
pub fn normalize_mac(s: &str) -> Option<String> {
    let hex: String = s.chars().filter(|c| !matches!(c, ':' | '-' | '.' | ' ')).collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Vec<u8> = (0..6).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0)).collect();
    if bytes.iter().all(|b| *b == 0) || bytes[0] & 1 == 1 {
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"))
}

/// A locally administered MAC (second bit of the first byte), often randomized.
pub fn mac_is_local(mac: &str) -> bool {
    mac.get(0..2).and_then(|h| u8::from_str_radix(h, 16).ok()).is_some_and(|b| b & 2 == 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs() {
        assert_eq!(normalize_mac("AA-BB-CC-DD-EE-F0").as_deref(), Some("aa:bb:cc:dd:ee:f0"));
        assert_eq!(normalize_mac("aabb.ccdd.eef0").as_deref(), Some("aa:bb:cc:dd:ee:f0"));
        assert_eq!(normalize_mac("00:00:00:00:00:00"), None);
        assert_eq!(normalize_mac("ff:ff:ff:ff:ff:ff"), None);
        assert_eq!(normalize_mac("01:00:5e:00:00:01"), None);
        assert_eq!(normalize_mac("aa:bb:cc"), None);
        assert!(mac_is_local("da:00:00:00:00:01"));
        assert!(!mac_is_local("a8:a1:59:00:00:01"));
    }

    #[test]
    fn device_defaults() {
        let d: WolDevice = serde_json::from_str(r#"{"name":"pc","macs":["aa:bb:cc:dd:ee:f0"]}"#).unwrap();
        assert_eq!(d.port, 9);
        assert_eq!(d.wake_timeout_s, DEFAULT_WAKE_TIMEOUT_S);
    }
}
