//! Network (P4, decision D-NET): inventory, staged interface settings, the transaction with
//! timed confirmation and automatic rollback, rescue paths, ifupdown adoption, diagnostics.
//!
//! Phase 1 edits only interfaces that systemd-networkd manages (IPv4 static/DHCP, gateway,
//! DNS, MTU); everything else is shown read-only.

use serde::{Deserialize, Serialize};

/// Who configures an interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetBackend {
    Networkd,
    Ifupdown,
    NetworkManager,
    /// Nothing (or something rynas does not know, e.g. a container runtime).
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    #[default]
    Ethernet,
    Bond,
    Bridge,
    Vlan,
    Loopback,
    Wireless,
    /// veth, dummy, tun, docker bridges' peers...
    Virtual,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Duplex {
    Full,
    Half,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IpFamily {
    #[default]
    Ipv4,
    Ipv6,
}

/// An address on an interface now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IpAddress {
    pub address: String,
    pub prefix_len: u8,
    pub family: IpFamily,
    /// `global`, `link`, `host`, `site`.
    pub scope: String,
    /// Leased (DHCP, SLAAC) rather than configured.
    pub dynamic: bool,
}

/// How an interface gets its IPv4 address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Ipv4Method {
    #[default]
    Dhcp,
    Static,
    /// No IPv4 address: only the always-on link-local rescue address.
    Disabled,
}

/// The settings rynas manages for one interface (`PUT /api/v1/network/interfaces/{name}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InterfaceConfig {
    #[serde(default)]
    pub ipv4: Ipv4Method,
    /// `a.b.c.d/nn`, static only (at least one).
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Default gateway, static only; must be inside one of `addresses`.
    #[serde(default)]
    pub gateway: Option<String>,
    /// DNS servers of this link (static; with DHCP they are added to the leased ones).
    #[serde(default)]
    pub dns: Vec<String>,
    /// `None`: the driver default.
    #[serde(default)]
    pub mtu: Option<u32>,
}

/// One `iface` stanza of `/etc/network/interfaces` (read-only view).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IfupdownStanza {
    /// `inet` or `inet6`.
    pub family: String,
    /// `static`, `dhcp`, `manual`, `loopback`, `auto` (inet6)...
    pub method: String,
    /// `auto <name>` present.
    pub auto: bool,
    /// `allow-hotplug <name>` present.
    pub hotplug: bool,
    /// `a.b.c.d/nn` (from `address` + `netmask` when needed).
    pub address: Option<String>,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
    pub dns_search: Vec<String>,
    pub mtu: Option<u32>,
    pub bond_slaves: Vec<String>,
    pub bond_mode: Option<String>,
    pub bridge_ports: Vec<String>,
    pub vlan_raw_device: Option<String>,
    /// File the stanza comes from (`source` includes are followed).
    pub file: String,
    /// Options rynas does not interpret (shown as they are).
    pub other: Vec<NetOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetOption {
    pub name: String,
    pub value: String,
}

/// Why an interface cannot be edited ([`NetInterface::read_only`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadOnlyReason {
    pub code: ReadOnlyCode,
    /// The bond or bridge the interface is enslaved to (`enslaved`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<String>,
}

/// Stable code of a [`ReadOnlyReason`]; unknown codes of a newer server read as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnlyCode {
    /// The loopback interface.
    Loopback,
    /// A port of the bond or bridge `master`.
    Enslaved,
    /// Not an Ethernet port (Wi-Fi, VLAN, virtual...).
    NotEthernet,
    /// Configured by ifupdown: "let rynas manage" first.
    Ifupdown,
    /// Managed by NetworkManager.
    NetworkManager,
    /// systemd-networkd is not running.
    NoNetworkd,
    #[serde(other)]
    Other,
}

/// `GET /api/v1/network/interfaces`: one interface with its live state and its settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NetInterface {
    pub name: String,
    pub index: u32,
    pub kind: LinkKind,
    /// A real port (has a device behind it), not a virtual link.
    pub physical: bool,
    pub mac: Option<String>,
    pub mtu: u32,
    /// Largest MTU the driver accepts.
    pub max_mtu: Option<u32>,
    /// Kernel operational state: `up`, `down`, `dormant`, `unknown`...
    pub oper_state: String,
    /// `None` when the interface is down (the kernel cannot tell).
    pub carrier: Option<bool>,
    pub speed_mbps: Option<u32>,
    pub duplex: Option<Duplex>,
    pub driver: Option<String>,
    pub addresses: Vec<IpAddress>,
    /// DNS servers of this link (systemd-networkd state).
    pub dns: Vec<String>,
    pub backend: NetBackend,
    /// Configured by a file rynas wrote.
    pub managed_by_rynas: bool,
    /// Can be changed with `PUT /network/interfaces/{name}`.
    pub editable: bool,
    /// Why not, when `editable` is false (English, for logs and the CLI); the UI translates
    /// `read_only`.
    pub read_only_reason: Option<String>,
    /// The same reason as `read_only_reason`, as a code with the value its text names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<ReadOnlyReason>,
    /// The networkd `.network` file that applies to it, if any.
    pub config_file: Option<String>,
    /// What rynas (or the foreign networkd file) configures now.
    pub applied: Option<InterfaceConfig>,
    /// The session user's staged change.
    pub pending: Option<InterfaceConfig>,
    /// ifupdown stanzas for this interface (read-only until adopted).
    pub ifupdown: Vec<IfupdownStanza>,
    /// Deterministic IPv4 link-local address derived from the MAC (rescue path).
    pub link_local_v4: Option<String>,
    /// The current session reaches rynas through this interface.
    pub management: bool,
}

/// A route of the main table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetRoute {
    pub family: IpFamily,
    /// `default` or `a.b.c.d/nn`.
    pub destination: String,
    pub gateway: Option<String>,
    pub interface: String,
    pub metric: Option<u32>,
    /// `kernel`, `dhcp`, `static`, `boot`, `ra`...
    pub protocol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DnsState {
    pub servers: Vec<String>,
    pub search: Vec<String>,
    /// File read: `/run/systemd/resolve/resolv.conf` (resolved upstreams) or `/etc/resolv.conf`.
    pub source: String,
}

/// Which network services are active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetServices {
    pub networkd: bool,
    pub resolved: bool,
    /// `networking.service` (ifupdown).
    pub ifupdown: bool,
    pub network_manager: bool,
    pub avahi: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TrialKind {
    /// Interface settings applied with a timed test.
    #[default]
    Apply,
    /// ifupdown → networkd migration, live.
    Adopt,
    /// ifupdown → networkd migration done by the boot guard at the next boot.
    AdoptAtBoot,
}

/// A change waiting for its confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetTrial {
    pub kind: TrialKind,
    /// The revision under test.
    pub revision: u64,
    /// The last known good revision restored without confirmation.
    pub lkg_revision: u64,
    pub started_ms: u64,
    /// `0` for [`TrialKind::AdoptAtBoot`] before the reboot.
    pub deadline_ms: u64,
    pub timeout_s: u32,
    pub interfaces: Vec<String>,
    pub management_touched: bool,
    pub author: String,
}

/// What the last automatic or manual rollback did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetRollbackReport {
    pub ts_ms: u64,
    /// `timeout`, `manual`, `verify_failed`, `boot_guard`, `boot_check`, `watchdog`.
    pub reason: String,
    pub restored_revision: u64,
    /// The actual state matches the restored configuration.
    pub verified: bool,
    pub problems: Vec<String>,
}

/// `GET /api/v1/network/state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NetworkState {
    pub hostname: String,
    pub services: NetServices,
    pub routes: Vec<NetRoute>,
    pub dns: DnsState,
    pub trial: Option<NetTrial>,
    /// Revision that is restored when a trial is not confirmed.
    pub lkg_revision: Option<u64>,
    pub last_rollback: Option<NetRollbackReport>,
    /// Interfaces with a change staged by the session user.
    pub pending: Vec<String>,
    /// Interface the current session comes through (`None`: unknown, e.g. loopback).
    pub management_interface: Option<String>,
    /// `/etc/network/interfaces` configures ports: "let rynas manage" is offered.
    pub ifupdown_adoptable: bool,
    /// The default and allowed confirmation timeouts.
    pub timeout_default_s: u32,
    pub timeout_min_s: u32,
    pub timeout_max_s: u32,
    pub read_at_ms: u64,
}

/// `POST /api/v1/network/apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetApplyRequest {
    /// Seconds to confirm (60–600, default 120).
    #[serde(default)]
    pub timeout_s: Option<u32>,
    /// Apply without the timed test; refused when the change touches the interface of the
    /// current session (or that interface is unknown).
    #[serde(default)]
    pub skip_test: bool,
    #[serde(default)]
    pub comment: String,
}

/// Answer of `POST /api/v1/network/apply` and of a live adoption (202).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetApplyStarted {
    pub task_id: String,
    /// Give it back to `POST /network/confirm`; `None` when applied without the test.
    pub token: Option<String>,
    /// About when the automatic rollback happens (the change starts a moment after this
    /// answer; `GET /network/state` has the exact deadline once the trial runs).
    pub deadline_ms: u64,
    pub timeout_s: u32,
    /// Where the interface will answer after the change (new addresses, `host.local`,
    /// link-local), to probe and confirm from.
    pub candidate_urls: Vec<String>,
    pub management_touched: bool,
}

/// `POST /api/v1/network/confirm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetConfirmRequest {
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetConfirmed {
    /// The new last known good revision.
    pub revision: u64,
}

/// One rescue address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RescueLinkLocal {
    pub interface: String,
    pub mac: String,
    pub address: String,
    /// The address is on the interface now.
    pub active: bool,
}

/// Rescue-path settings (`PUT /api/v1/network/rescue`, staged like every configuration key).
///
/// The Synology reset button is not here: `/etc/synohwd.toml` has one writer, the
/// `synology.hw` key (`PUT /api/v1/hardware/synology/settings`, `reset_button:
/// network_rescue`); [`RescueInfo`] reflects it read-only. A PUT that still sends
/// `reset_button` is refused (400 `invalid_value`); stored values that have it (older
/// revisions) are read without it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescueSettings {
    /// Avahi service file (`_rynas._tcp`, `_https._tcp` or `_http._tcp`).
    #[serde(default = "yes")]
    pub mdns_service: bool,
}

fn yes() -> bool {
    true
}

impl Default for RescueSettings {
    fn default() -> Self {
        Self { mdns_service: true }
    }
}

/// `GET /api/v1/network/rescue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RescueInfo {
    /// `<host>.local`.
    pub mdns_name: String,
    /// avahi-daemon is running.
    pub mdns_active: bool,
    pub link_local: Vec<RescueLinkLocal>,
    /// A Synology (the `synology.hw` key exists: synohwd's settings are managed by rynas).
    pub synology: bool,
    /// Read-only reflection of `synology.hw`: its current `reset_button` is `network_rescue`
    /// (the double press restores the factory network). Changed with
    /// `PUT /api/v1/hardware/synology/settings`.
    pub reset_button_configured: bool,
    /// The session user has staged a different `synology.hw` reset button: the value it will
    /// have after `/config/apply` (`true` = network rescue).
    #[serde(default)]
    pub reset_button_pending: Option<bool>,
    pub settings: RescueSettings,
    pub pending: Option<RescueSettings>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdoptMode {
    /// Live, as a transaction with timed confirmation.
    #[default]
    Now,
    /// At the next boot; the boot guard restores ifupdown when no port has a routable
    /// address 3 minutes later. Recommended when connected remotely.
    NextBoot,
}

/// `POST /api/v1/network/adopt`: hand the ifupdown configuration to rynas (networkd).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetAdoptRequest {
    #[serde(default)]
    pub mode: AdoptMode,
    #[serde(default)]
    pub timeout_s: Option<u32>,
    /// Must be true (the migration changes how the whole network is configured).
    #[serde(default)]
    pub confirm: bool,
}

/// Answer of `POST /api/v1/network/adopt` with `mode: next_boot` (200).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetAdoptScheduled {
    pub revision: u64,
    /// Interfaces that will be configured by networkd after the reboot.
    pub interfaces: Vec<String>,
}

/// `GET /api/v1/network/revisions`: one stored network revision, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetRevision {
    pub id: u64,
    pub ts_ms: u64,
    pub author: String,
    pub comment: String,
    /// `snapshot` (the system right before a change), `apply`, `adopt`, `rescue`, `restore`.
    pub kind: String,
    /// Interfaces rynas configures in it.
    pub interfaces: Vec<String>,
    /// ifupdown or networkd.
    pub backend: NetBackend,
    /// Confirmed, or a snapshot of a running system.
    pub good: bool,
    /// The current last known good revision.
    pub lkg: bool,
}

/// `POST /api/v1/network/revisions/{n}/restore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetRestoreRequest {
    #[serde(default)]
    pub timeout_s: Option<u32>,
}

/// `POST /api/v1/network/diagnostics/ping`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PingRequest {
    /// Host name or address.
    pub host: String,
    /// 1–10, default 4.
    #[serde(default)]
    pub count: Option<u8>,
    /// Send from this interface.
    #[serde(default)]
    pub interface: Option<String>,
    /// Payload size (MTU tests with `dont_fragment`), 0–65000.
    #[serde(default)]
    pub size: Option<u32>,
    #[serde(default)]
    pub dont_fragment: bool,
}

/// `POST /api/v1/network/diagnostics/traceroute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TracerouteRequest {
    pub host: String,
}

/// `POST /api/v1/network/diagnostics/dns`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DnsLookupRequest {
    pub name: String,
    /// `A`, `AAAA`, `MX`, `TXT`, `CNAME`, `NS`, `PTR`, `SRV`; default `A`.
    #[serde(default)]
    pub record_type: Option<String>,
    /// Ask this server instead of the system resolver.
    #[serde(default)]
    pub server: Option<String>,
}

/// Result of a diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DiagnosticResult {
    /// `ping`, `mtr`, `traceroute`, `resolvectl`, `getent`.
    pub tool: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    /// What the tool printed (capped).
    pub output: String,
    pub duration_ms: u64,
    /// Structured output when the tool has one (`mtr --json`).
    pub report: Option<serde_json::Value>,
}
