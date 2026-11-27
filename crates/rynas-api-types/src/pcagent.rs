//! PC backup with `rynas-agent`, file method (D-PCBACKUP, 2026-11-11): restic-format
//! repositories served by rynas with per-PC rules, one per **backup group** (by default one
//! group per PC; PCs in one group deduplicate against each other and can read each other's
//! data), and a layout manifest for bare-metal restore. The other method, btrfs snapshots
//! received with btrbk, is [`crate::pcbackup`].
//! API in `docs/phase1/plan.md` "API v1: PC backup".
//!
//! The same types are the wire format of the console (`/api/v1/pcbackup/…`), of the agent
//! (`/api/v1/pcbackup/agent/…`) and of the manifest the agent writes to
//! `/.rynas/manifest.json` on the PC.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::backup::Retention;

/// Where the manifest lives on the PC (inside the root filesystem, so every version has it).
pub const MANIFEST_PATH: &str = "/.rynas/manifest.json";
/// The manifest format (one schema for both PC methods: the file method fills
/// [`Manifest::filesystems`] and friends, the btrfs method adds [`Manifest::btrbk`]).
pub const MANIFEST_FORMAT: u32 = 1;

/// Mount points (and what lies below them) the btrfs method never sends: caches, logs, swap,
/// rebuildable container storage. The agent skips them; an interface does not count them as
/// left out ([`Manifest::unsent_mounts`]).
pub const NOT_SENT_MOUNTS: &[&str] = &["/var/log", "/var/cache", "/var/tmp", "/tmp", "/swap", "/var/swap", "/var/lib/docker", "/var/lib/containers", "/srv/swap"];
/// The folder of the agent's own snapshots, inside each snapshotted subvolume.
pub const AGENT_SNAP_DIR: &str = ".rynas-snap";

/// `rel` (a path from a top level or below a mount) holds the components `seq` in a row:
/// `docker/btrfs` matches `var/lib/docker/btrfs/…` and `home/me/.local/share/docker/btrfs/…`.
fn has_components(rel: &str, seq: &[&str]) -> bool {
    // `@docker` (a subvolume of its own, mounted at /var/lib/docker) counts as `docker`
    let c: Vec<&str> = rel.split('/').map(|x| x.trim_start_matches('@')).collect();
    c.windows(seq.len()).any(|w| w == seq)
}

/// Container engines' storage, wherever it is (the system's and rootless engines' under a
/// home: `~/.local/share/docker/btrfs`, `~/.local/share/containers/storage`), and Incus' and
/// LXD's storage pools: layers and instances the engine manages, never PC data.
pub fn container_storage(rel: &str) -> bool {
    has_components(rel, &["docker", "btrfs"])
        || has_components(rel, &["containers", "storage"])
        || has_components(rel, &["incus", "storage-pools"])
        || has_components(rel, &["lxd", "storage-pools"])
}

/// Subvolumes (a path from a top level or below a mount) never backed up: snapshot folders
/// (snapper, btrbk, Timeshift, the agent's own) and container storage.
pub fn excluded_subvolume(rel: &str) -> bool {
    let last = rel.rsplit('/').next().unwrap_or(rel);
    // snapper's folder, and anything inside it
    rel.split('/').any(|c| c == ".snapshots")
        || last == AGENT_SNAP_DIR
        // btrbk's local snapshots (the usual folder names and the agent's default)
        || rel.split('/').any(|c| c == "btrbk_snapshots" || c == ".btrbk_snapshots")
        || rel.contains("timeshift-btrfs")
        || container_storage(rel)
        || rel.starts_with(AGENT_SNAP_DIR)
        || rel.contains(&format!("/{AGENT_SNAP_DIR}/"))
}

/// Why the btrfs method deliberately does not send the subvolume `subvol` (its path from the
/// top level) mounted at `mount`, a choice the user cannot change (`None`: it is sent). One
/// rule for the agent's layout and for [`Manifest::unsent_mounts`], so an interface never
/// calls a PC partly backed up for what is left out on purpose.
pub fn not_sent_reason(mount: &str, subvol: &str) -> Option<&'static str> {
    let subvol = subvol.trim_matches('/');
    let under = |p: &str| mount == p || mount.starts_with(&format!("{p}/"));
    if NOT_SENT_MOUNTS.iter().any(|p| under(p)) {
        Some("cache, logs or rebuildable data")
    } else if mount.rsplit('/').next() == Some(".snapshots") || excluded_subvolume(subvol) {
        Some("snapshot folder")
    } else if mount.chars().any(|c| c.is_whitespace() || c == '#' || c == '"' || c == '\'') || subvol.chars().any(char::is_whitespace) {
        Some("a name btrbk.conf cannot hold")
    } else {
        None
    }
}
/// The `Accept` value of the restic REST protocol v2 listings.
pub const REST_V2: &str = "application/vnd.x.restic.rest.v2";
/// Longest manifest the NAS keeps (display only).
pub const MAX_MANIFEST: usize = 1 << 20;
/// Longest `changed_during` list in a report.
pub const MAX_CHANGED: usize = 200;

// ------------------------------------------------------------------ plans and settings

/// How filesystems without a snapshot (ext2/3/4, XFS without LVM) are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FreezeMode {
    /// Read while in use; files changed during the backup are listed.
    #[default]
    Hot,
    /// Frozen for the whole run, thawed at `freeze_max_secs` at the latest (then hot).
    Freeze,
}

/// What the NAS tells the agent to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcPlan {
    /// Hours between two backups (1–168).
    pub interval_hours: u32,
    /// Minutes after the agent starts before a due backup runs.
    #[serde(default)]
    pub start_delay_min: u32,
    /// Exclude globs, on top of the agent's own (caches, `/tmp`, swap files).
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub freeze: FreezeMode,
    /// Longest freeze in seconds (5–3600).
    #[serde(default = "default_freeze_max")]
    pub freeze_max_secs: u32,
    /// No scheduled backups ("Back up now" still runs). In the plan an agent receives it is
    /// also set while every scheduled task of the NAS is paused (Schedule › Pause all).
    #[serde(default)]
    pub paused: bool,
    /// Allowed start window (S6 of `docs/research/scheduling.md`): 168 cells `0`/`1`, Sunday
    /// first (`window[day * 24 + hour]`), in the NAS's local time like every weekly grid of
    /// rynas (the agent receives the NAS's zone in [`AgentPlan::tz`]). A due backup starts
    /// only while it is open (a run already going is not stopped at its end). `None`: any
    /// time. Agents older than 0.8.20 ignore it and start whenever a backup is due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Backups at set times instead of every `interval_hours`: a systemd calendar expression
    /// (`*-*-* 02:00:00`, `Mon..Fri 21:00`), in the NAS's time zone. Due at its first time after
    /// the last good run, so a time missed while the PC was off is caught up once,
    /// `start_delay_min` after the agent starts (like `Persistent=true`). `None`: every
    /// `interval_hours`. Agents older than 0.8.21 ignore it and keep the interval the NAS sends
    /// them instead (about the calendar's spacing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<String>,
    /// Also one backup at every start of the PC (`start_delay_min` after it; a restart of the
    /// agent alone is not a start), however recent the last one, on top of the interval or the
    /// calendar (after it, the interval counts from that backup; a calendar's next time still
    /// runs). Off by default. Agents older than 0.8.21 ignore it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub at_boot: bool,
    /// The NAS wakes the PC (its linked Wake-on-LAN device, `crate::wol`) when a scheduled
    /// backup is due inside the window and the PC is off; the agent then starts it at once.
    /// Off by default; ignored without a linked device.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wake_for_backup: bool,
    /// What the agent does after a backup the NAS woke the PC for, when nobody used the PC
    /// meanwhile. Agents older than 0.8.26 ignore it.
    #[serde(default, skip_serializing_if = "AfterWake::is_stay")]
    pub after_wake: AfterWake,
}

/// After a backup the NAS woke the PC for ([`PcPlan::after_wake`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AfterWake {
    /// Leave the PC on.
    #[default]
    Stay,
    Suspend,
    Poweroff,
    /// A value of a newer NAS: treated as [`AfterWake::Stay`].
    #[serde(other)]
    Unknown,
}

impl AfterWake {
    /// Stay on (also an unknown value of a newer NAS).
    pub fn is_stay(&self) -> bool {
        matches!(self, AfterWake::Stay | AfterWake::Unknown)
    }
}

/// What an agent can do to its PC's power on request ([`AgentCommand::Sleep`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SleepMode {
    Suspend,
    Hibernate,
    Poweroff,
    /// A mode of a newer NAS or agent: never carried out, dropped from facts.
    #[serde(other)]
    Unknown,
}

/// One network card of the PC, as its agent reports it ([`AgentFacts`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentNic {
    /// Kernel name (`enp3s0`).
    pub name: String,
    /// `aa:bb:cc:dd:ee:ff`, lower case.
    pub mac: String,
    /// IPv4 addresses with prefix (`192.0.2.20/24`).
    #[serde(default)]
    pub ipv4: Vec<String>,
    /// Wi-Fi card (`/sys/class/net/<if>/wireless` or `phy80211`).
    #[serde(default)]
    pub wireless: bool,
    /// Carrier up.
    #[serde(default)]
    pub up: bool,
    /// `ethtool`: the card supports magic packets (`Supports Wake-on` holds `g`); `None`
    /// unknown (no ethtool, virtual card).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wol_supported: Option<bool>,
    /// `ethtool`: `Wake-on` holds `g`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wol_enabled: Option<bool>,
    /// The agent turns magic-packet wake on for this card at every start
    /// ([`AgentCommand::EnableWol`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wol_kept: bool,
}

/// `POST` [`AGENT_FACTS`] (device token): the PC's facts, sent by agents from 0.8.26 at start
/// and every 6 hours when they changed. An older NAS answers 404: the agent stops sending
/// until it restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentFacts {
    pub agent_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_btrfs: Option<bool>,
    /// Physical cards (no loopback, bridges, tunnels, containers).
    #[serde(default)]
    pub nics: Vec<AgentNic>,
    /// What [`AgentCommand::Sleep`] can do here (`systemctl can-suspend` & co.).
    #[serde(default)]
    pub sleep: Vec<SleepMode>,
}

pub const AGENT_FACTS: &str = "/api/v1/pcbackup/agent/facts";

fn default_freeze_max() -> u32 {
    60
}

impl Default for PcPlan {
    fn default() -> Self {
        PcPlan {
            interval_hours: 24,
            start_delay_min: 10,
            excludes: Vec::new(),
            freeze: FreezeMode::Hot,
            freeze_max_secs: default_freeze_max(),
            paused: false,
            window: None,
            calendar: None,
            at_boot: false,
            wake_for_backup: false,
            after_wake: AfterWake::Stay,
        }
    }
}

/// Settings of one PC (`PUT /api/v1/pcbackup/devices/{device}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]

pub struct PcDeviceSettings {
    #[serde(default)]
    pub description: String,
    pub plan: PcPlan,
    pub retention: Retention,
    /// No version younger than this is ever forgotten.
    #[serde(default)]
    pub min_age_days: u32,
    /// Late after this many hours without a good version (0: never).
    #[serde(default = "default_alert")]
    pub alert_hours: u32,
    /// Critical after this many hours.
    #[serde(default = "default_critical")]
    pub critical_hours: u32,
    /// Files into the group's repository, or btrfs snapshots sent to the PC's folder of
    /// the btrfs method (`crate::pcbackup::PcTarget` with `agent`, same name). With
    /// `btrfs`, `retention`, `min_age_days` and the alert hours of the files method do not
    /// apply: the folder's NAS-side retention and alerts do.
    #[serde(default)]
    pub method: PcMethod,
}

/// How `rynas-agent` backs a PC up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PcMethod {
    /// Files into a restic-format repository on the NAS (any filesystem).
    #[default]
    Files,
    /// Read-only btrfs snapshots of the PC's subvolumes, sent with `btrfs send` over HTTPS
    /// and received into the PC's folder (btrbk-style names, `docs/research/pc-backup.md`
    /// §11.9).
    Btrfs,
}

fn default_alert() -> u32 {
    26
}

fn default_critical() -> u32 {
    72
}

impl Default for PcDeviceSettings {
    fn default() -> Self {
        PcDeviceSettings {
            description: String::new(),
            plan: PcPlan::default(),
            retention: Retention::SMART,
            min_age_days: 7,
            alert_hours: default_alert(),
            critical_hours: default_critical(),
            method: PcMethod::Files,
        }
    }
}

/// Settings of the feature (`PUT /api/v1/pcbackup/settings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcAgentSettings {
    /// The folder that holds one repository per backup group; empty: not set up yet.
    #[serde(default)]
    pub folder: String,
    /// Quota of new groups (0: none).
    #[serde(default)]
    pub default_quota_gib: u32,
    /// Hour (0–23, local time) of the daily retention, prune and check.
    #[serde(default = "default_hour")]
    pub maintenance_hour: u8,
    /// Share of the NAS' memory (10–80 %) a maintenance may use: the worker's unit gets it
    /// as `MemoryMax`, and a prune whose estimate does not fit is postponed.
    #[serde(default = "default_memory_pct")]
    pub maintenance_memory_pct: u8,
    /// Average chunk of new groups' repositories; a repository keeps its own for good.
    #[serde(default)]
    pub chunking: PcChunking,
    /// Where restore requests (a restore TUI asking for an approval from a phone) may come
    /// from: IP addresses or CIDR ranges (`203.0.113.0/24`, `2001:db8::/32`; `0.0.0.0/0` and
    /// `::/0` for anywhere). Empty: private and LAN ranges, Tailscale, loopback
    /// ([`RESTORE_DEFAULT_SOURCES`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restore_request_sources: Vec<String>,
    /// Restore requests over plain HTTP are accepted (no certificate binds their compare
    /// code; approvers see a red warning). Off by default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restore_requests_over_http: bool,
}

/// Where restore requests may come from when [`PcAgentSettings::restore_request_sources`] is
/// empty: private IPv4 (RFC 1918), Tailscale and carrier-grade NAT (100.64.0.0/10),
/// link-local, loopback, IPv6 unique-local (Tailscale's too) and link-local.
pub const RESTORE_DEFAULT_SOURCES: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "100.64.0.0/10",
    "169.254.0.0/16",
    "127.0.0.0/8",
    "fc00::/7",
    "fe80::/10",
    "::1/128",
];

/// One IP range (`a.b.c.d/n`, `x::/n`, or a single address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpRange {
    pub addr: std::net::IpAddr,
    pub prefix: u8,
}

impl IpRange {
    pub fn parse(s: &str) -> Option<IpRange> {
        let s = s.trim();
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: std::net::IpAddr = a.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => p.parse::<u8>().ok().filter(|n| *n <= max)?,
            Some(_) => return None,
            None => max,
        };
        Some(IpRange { addr, prefix })
    }

    /// `ip` is inside (an IPv4-mapped IPv6 address counts as its IPv4 one).
    pub fn contains(&self, ip: std::net::IpAddr) -> bool {
        use std::net::IpAddr;
        let ip = match ip {
            IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let m = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                u32::from(a) & m == u32::from(b) & m
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let m = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                u128::from(a) & m == u128::from(b) & m
            }
            _ => false,
        }
    }
}

/// Whether a restore request from `ip` is accepted with these `sources` (empty: the
/// defaults).
pub fn restore_source_allowed(sources: &[String], ip: std::net::IpAddr) -> bool {
    if sources.is_empty() {
        RESTORE_DEFAULT_SOURCES.iter().filter_map(|s| IpRange::parse(s)).any(|r| r.contains(ip))
    } else {
        sources.iter().filter_map(|s| IpRange::parse(s)).any(|r| r.contains(ip))
    }
}

/// The same defaults as a document without the fields (a derived default gave 0 for the
/// hour and the share, which the settings then refused to save back).
impl Default for PcAgentSettings {
    fn default() -> Self {
        PcAgentSettings {
            folder: String::new(),
            default_quota_gib: 0,
            maintenance_hour: default_hour(),
            maintenance_memory_pct: default_memory_pct(),
            chunking: PcChunking::default(),
            restore_request_sources: Vec::new(),
            restore_requests_over_http: false,
        }
    }
}

impl PcAgentSettings {
    /// The share of memory a maintenance gets: an unset one (0, written by older versions) is
    /// the default.
    pub fn effective_memory_pct(&self) -> u8 {
        if self.maintenance_memory_pct == 0 { default_memory_pct() } else { self.maintenance_memory_pct }
    }
}

/// Average chunk of a new group's repository. Smaller chunks deduplicate a little better;
/// larger ones mean half the blobs, so about half the memory of prune and check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PcChunking {
    /// 1 MiB with 8 GiB of memory or more, 2 MiB below.
    #[default]
    Auto,
    /// 1 MiB (512 KiB–8 MiB), restic's own.
    Small,
    /// 2 MiB (1–16 MiB).
    Large,
}

/// Memory from which [`PcChunking::Auto`] picks 1 MiB chunks.
pub const AUTO_SMALL_CHUNKS_FROM: u64 = 8 << 30;

impl PcChunking {
    /// The average chunk in MiB for a NAS with `mem_total` bytes of memory.
    pub fn mib(self, mem_total: u64) -> u8 {
        match self {
            Self::Small => 1,
            Self::Large => 2,
            Self::Auto if mem_total >= AUTO_SMALL_CHUNKS_FROM => 1,
            Self::Auto => 2,
        }
    }
}

fn default_hour() -> u8 {
    4
}

fn default_memory_pct() -> u8 {
    40
}

// ------------------------------------------------------------------ console views

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PcState {
    /// Paired, no version yet.
    #[default]
    Waiting,
    Ok,
    Running,
    Late,
    Critical,
    /// The last run failed (and the last good one is not late yet).
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    #[default]
    Preparing,
    /// Snapshots, freezes, the manifest.
    Snapshot,
    /// Reading and uploading.
    Uploading,
    Finishing,
}

/// Live progress of a run (from the agent every 2 s).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcProgress {
    pub run_id: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: u64,
    #[serde(default)]
    pub files_done: u64,
    #[serde(default)]
    pub files_total: u64,
    /// Bytes per second over the last reports.
    #[serde(default)]
    pub speed_bps: u64,
    /// Free text (English) for older interfaces; [`Self::btrfs`] says the same in fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The btrfs method: the subvolume being sent now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs: Option<BtrfsStep>,
    pub updated_ms: u64,
}

/// What the btrfs method does or did with one subvolume, in fields, so an interface can say
/// it in its own language (the English `message`/`note` stays for older ones).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrfsStep {
    /// The subvolume's snapshot name prefix (`@home`).
    pub subvolume: String,
    /// The snapshot sent (`@home.20261112T0300`).
    pub snapshot: String,
    /// The local parent of an incremental send; `None`: a full send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Why it failed (finished steps only): one of [`btrfs_step_error`], or a NAS error code
    /// (`busy`, `already_exists`, `parent_missing`, `invalid_value`, `payload_too_large`…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// The error as it came (English), next to the code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Agent-side error codes of a [`BtrfsStep`].
pub mod btrfs_step_error {
    /// The local read-only snapshot could not be taken.
    pub const SNAPSHOT: &str = "snapshot";
    /// The connection to the NAS failed (or `btrfs send` itself).
    pub const TRANSPORT: &str = "transport";
    /// Cancel was asked for.
    pub const CANCELLED: &str = "cancelled";
}

/// How one filesystem was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMethod {
    /// btrfs read-only snapshot.
    BtrfsSnapshot,
    /// LVM snapshot mounted read-only.
    LvmSnapshot,
    /// Frozen for the whole run.
    Frozen,
    /// Frozen, thawed by the watchdog before the end (then read hot).
    FrozenThawed,
    /// Read while in use.
    Hot,
    /// Copied as files (the ESP, vfat).
    Files,
    /// Not in the backup.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capture {
    pub path: String,
    pub fstype: String,
    pub method: CaptureMethod,
    /// Free text (English) for older interfaces; [`Self::note_code`] says the same in fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Why this filesystem was read the way it was, for the interface to put into words
    /// (agents from 0.8.21; older ones send only [`Self::note`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_code: Option<CaptureNote>,
    /// The btrfs method: what was sent for this subvolume, in fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs: Option<BtrfsStep>,
}

/// Why a filesystem was not read from a snapshot (or not at all), in fields
/// ([`Capture::note_code`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum CaptureNote {
    /// The volume group has too little free space for a snapshot of the volume: read in use.
    LvmNoSpace { vg: String, lv: String },
    /// An LVM snapshot failed (the tool's own message): read in use.
    LvmFailed { error: String },
    /// A btrfs snapshot failed: read in use.
    BtrfsFailed { error: String },
    /// Its mount point is missing in the parent filesystem: left out.
    MountPointMissing,
    /// The filesystem refused to freeze: read in use.
    FreezeRefused { error: String },
    /// A code this NAS does not know (a newer agent): the interface shows [`Capture::note`],
    /// and the report is still accepted.
    #[serde(other)]
    Other,
}

impl CaptureNote {
    /// The English sentence older interfaces show ([`Capture::note`]).
    pub fn text(&self) -> String {
        match self {
            CaptureNote::LvmNoSpace { vg, lv } => format!("volume group {vg} has too little free space for a snapshot of {lv}"),
            CaptureNote::LvmFailed { error } | CaptureNote::BtrfsFailed { error } => error.clone(),
            CaptureNote::MountPointMissing => "its mount point is missing in the parent filesystem".into(),
            CaptureNote::FreezeRefused { error } => format!("freeze refused: {error}"),
            CaptureNote::Other => String::new(),
        }
    }
}

/// One run of the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcRun {
    pub run_id: String,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub files: u64,
    #[serde(default)]
    pub bytes_read: u64,
    #[serde(default)]
    pub bytes_added: u64,
    /// Files changed while they were read (up to [`MAX_CHANGED`]).
    #[serde(default)]
    pub changed_during: Vec<String>,
    #[serde(default)]
    pub changed_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub captures: Vec<Capture>,
    /// A bare-metal restore from the live system (not a backup).
    #[serde(default)]
    pub restore: bool,
    /// How the run started (S6: `timer` on schedule, `retry` after a failed run, `manual`
    /// for "Back up now"); absent from older agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<crate::scheduler::RunTrigger>,
    /// Not a run: a scheduled start the agent skipped (S6: `paused`, or `window` when a
    /// backup came due outside the allowed window and waits for it), sent only to a NAS that
    /// gave the plan a shared trigger ([`AgentPlan::trigger`]). Recorded in the scheduler's
    /// history only; the PC's own history, its last good run and its alerts stay as they
    /// are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<crate::scheduler::SkipReason>,
}

/// Why a PC is (not) ready for a bare-metal restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyReason {
    NoVersion,
    NoManifest,
    /// The newest manifest is older than the newest version by more than a day.
    ManifestStale,
    /// A mounted filesystem was skipped.
    VolumeSkipped,
    LoaderUnknown,
    /// No partition table could be read.
    NoPartitionTable,
    /// The group has no recovery kit yet.
    NoRecoveryKit,
    /// md, or LVM the restore does not recreate (thin pools, snapshots, raid/mirror/striped
    /// or cache volumes): recreated by hand. Linear LVM is recreated (agent 0.8.12).
    ManualLayout,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BareMetalReady {
    pub ready: bool,
    #[serde(default)]
    pub reasons: Vec<ReadyReason>,
    /// With [`ReadyReason::ManualLayout`]: which volume group, volume or physical volume, and
    /// why (technical lines, English).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcMaintenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ms: Option<u64>,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub freed_bytes: u64,
    #[serde(default)]
    pub removed_versions: u32,
    /// Memory the next prune is estimated to need (from the index: blobs × bytes per entry).
    #[serde(default)]
    pub estimate_bytes: u64,
    /// Blobs in the index when it was last counted.
    #[serde(default)]
    pub blobs: u64,
    /// Why the last scheduled maintenance did not run (over the memory budget, PC busy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postponed: Option<String>,
}

/// A command waiting for the agent's next poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentCommand {
    BackupNow { id: u64 },
    Cancel { id: u64 },
    /// Suspend, hibernate or shut the PC down (refused by the agent while a backup runs).
    /// Sent only to agents from 0.8.26: older ones cannot read it.
    Sleep { id: u64, mode: SleepMode },
    /// Turn magic-packet wake on for these cards now and at every agent start (`ethtool -s
    /// <if> wol g`). Agents from 0.8.26 only.
    EnableWol { id: u64, nics: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcDevice {
    pub device: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub agent_version: String,
    /// The PC's root filesystem is btrfs, as its agent last said (`None`: an older agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_btrfs: Option<bool>,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_ms: Option<u64>,
    /// `Late` and `Critical` only when the PC was online when a backup was due (the agent
    /// would have started it) and none ran; a PC that is off stays as its last backup left it.
    pub state: PcState,
    /// When the next scheduled backup is due, as the agent counts it from the last good one
    /// (without the delay after the PC starts): in the past when one was missed while the PC
    /// was off (it catches up soon after the PC starts), else the next start inside the
    /// allowed window. `None` while paused (also by Pause all) or revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_ms: Option<u64>,
    /// When the PC's interval counts from: the last good run's start (agents from 0.8.24),
    /// else its end, as its agent counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_from_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<PcRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<PcProgress>,
    /// Its backup group (the repository).
    pub group: String,
    /// Bytes this PC uploaded into the group's repository (new data only, before prunes).
    #[serde(default)]
    pub uploaded_bytes: u64,
    #[serde(default)]
    pub versions: u32,
    pub settings: PcDeviceSettings,
    #[serde(default)]
    pub bare_metal: BareMetalReady,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<AgentCommand>,
    /// When `command` was queued: still waiting for the PC while `last_seen_ms` is older.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_at_ms: Option<u64>,
    /// Revoked: no token, no key; the versions stay readable until the group is deleted.
    #[serde(default)]
    pub revoked: bool,
}

/// Settings of a backup group (`PUT /api/v1/pcbackup/groups/{group}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcGroupSettings {
    #[serde(default)]
    pub description: String,
    /// Size limit of the repository (0: none).
    #[serde(default)]
    pub quota_gib: u32,
}

/// A backup group: one repository, one or more PCs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcGroup {
    pub group: String,
    pub settings: PcGroupSettings,
    /// The repository folder on the NAS.
    pub repository: String,
    pub created_ms: u64,
    /// PCs that write here (revoked ones included).
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub used_bytes: u64,
    #[serde(default)]
    pub maintenance: PcMaintenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_kit_ms: Option<u64>,
    /// Average chunk of its repository in MiB, fixed when it was created.
    #[serde(default)]
    pub chunk_mib: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcDeviceDetail {
    pub device: PcDevice,
    #[serde(default)]
    pub history: Vec<PcRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingView {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub expires_ms: u64,
    /// A pairing for the btrfs method into this folder (no group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PcBackupOverview {
    pub settings: PcAgentSettings,
    pub groups: Vec<PcGroup>,
    pub devices: Vec<PcDevice>,
    #[serde(default)]
    pub pairings: Vec<PairingView>,
    /// Memory a maintenance may use on this NAS (`maintenance_memory_pct` of the RAM).
    #[serde(default)]
    pub memory_budget_bytes: u64,
    /// SHA-256 of the certificate the web interface serves (hex, `:` separated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Every certificate this NAS presents to clients (the interface's own, and through the
    /// built-in proxy its `.local` leaf, imported pairs, ACME certificates), with the names
    /// they cover: the restore TUI pins the one served for the address it uses, and a
    /// restore request is accepted with any of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub served_certificates: Vec<ServedCertificate>,
    #[serde(default)]
    pub dry: bool,
}

/// [`PcBackupOverview::served_certificates`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ServedCertificate {
    /// SHA-256 of the DER (`AA:BB:…`).
    pub fingerprint: String,
    /// DNS names and IP addresses of its SAN (`*.example.org` wildcards as written).
    #[serde(default)]
    pub names: Vec<String>,
}

impl ServedCertificate {
    /// Whether it covers `host` (a DNS name, case-insensitive, one-label wildcards; or an
    /// IP address).
    pub fn covers(&self, host: &str) -> bool {
        let h = host.trim().trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
        self.names.iter().any(|n| {
            let n = n.to_ascii_lowercase();
            if let Some(rest) = n.strip_prefix("*.") {
                h.split_once('.').is_some_and(|(first, tail)| !first.is_empty() && tail == rest)
            } else {
                n == h
            }
        })
    }
}

/// `POST /api/v1/pcbackup/pairings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingRequest {
    #[serde(default)]
    pub description: String,
    /// Join this existing group; `None`: a new group of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Quota of a new group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_gib: Option<u32>,
    /// Pair a PC for the btrfs method into this folder of the btrfs method (a
    /// `crate::pcbackup::PcTarget` with `agent`): the device takes the folder's name, gets
    /// [`PcMethod::Btrfs`] and no repository (`group` and `quota_gib` are ignored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairingCreated {
    pub id: String,
    pub code: String,
    pub expires_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// The command to run on the PC, with this NAS' address filled in by the browser.
    pub command: String,
}

/// The recovery kit of a group: enough to open its repository with stock restic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RecoveryKit {
    pub group: String,
    #[serde(default)]
    pub description: String,
    /// The PCs whose versions it opens.
    #[serde(default)]
    pub devices: Vec<String>,
    pub repository: String,
    pub password: String,
    pub created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreCodeCreated {
    pub code: String,
    pub expires_ms: u64,
    pub device: String,
}

// ------------------------------------------------------------------ the agent's calls

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairRequest {
    pub code: String,
    pub hostname: String,
    #[serde(default)]
    pub machine_id: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub agent_version: String,
    /// The PC's root filesystem is btrfs (sent at pairing, so a PC that never ran is known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_btrfs: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]

pub struct AgentPlan {
    /// Changes with every new plan or command.
    pub rev: u64,
    pub plan: PcPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<AgentCommand>,
    /// What a backup run does (an older agent ignores it and runs the files method).
    #[serde(default)]
    pub method: PcMethod,
    /// The schedule in the shared model (S6): [`TaskTrigger::Interval`] from the plan's
    /// `interval_hours` and `start_delay_min`, which the NAS keeps sending for older agents
    /// (R5). An agent that finds it follows it with the rules of `rynas-schedule` (the same
    /// as the NAS's overview), the plan's allowed window included, and reports the starts it
    /// skips; absent (an older NAS): the two plan fields, no window, no skipped starts
    /// reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<crate::scheduler::TaskTrigger>,
    /// The NAS's time zone (IANA name): the zone of the allowed window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    /// The same zone as a POSIX TZ rule (`CET-1CEST,M3.5.0,M10.5.0/3`, from the zone file):
    /// used when the name is absent (a NAS whose `/etc/localtime` is a copy, or `TZ` set to a
    /// rule) or unknown on the PC (no zone database). Neither: the PC's own zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz_posix: Option<String>,
    /// The NAS woke the PC for a due backup at this time (ms, [`PcPlan::wake_for_backup`]):
    /// the agent starts the due backup without the start delay and afterwards does
    /// [`PcPlan::after_wake`] when nobody used the PC. Cleared by the next report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub woken_ms: Option<u64>,
}

impl AgentPlan {
    /// The schedule an agent follows: every so many hours from the last good run, the first
    /// so many minutes after the agent started, and the allowed window. The shared trigger
    /// when the NAS sent one (an interval), else the plan's two old fields without a window
    /// (an older NAS).
    pub fn schedule(&self) -> AgentSchedule {
        match &self.trigger {
            Some(crate::scheduler::TaskTrigger::Interval { hours, after_boot_min }) => AgentSchedule {
                every_h: (*hours).max(1),
                delay_min: *after_boot_min,
                window: self.plan.window.clone().filter(|w| crate::scheduler::week_window_valid(w)),
                calendar: None,
                at_boot: self.plan.at_boot,
                shared: true,
            },
            // set times (0.8.21): the delay after start is the plan's, `interval_hours` the
            // calendar's spacing (what an older agent follows, and the cap of the retries)
            Some(crate::scheduler::TaskTrigger::Calendar { on_calendar, .. }) if !on_calendar.trim().is_empty() => AgentSchedule {
                every_h: self.plan.interval_hours.max(1),
                delay_min: self.plan.start_delay_min,
                window: self.plan.window.clone().filter(|w| crate::scheduler::week_window_valid(w)),
                calendar: Some(on_calendar.trim().to_string()),
                at_boot: self.plan.at_boot,
                shared: true,
            },
            _ => AgentSchedule {
                every_h: self.plan.interval_hours.max(1),
                delay_min: self.plan.start_delay_min,
                window: None,
                calendar: None,
                at_boot: false,
                shared: false,
            },
        }
    }
}

/// What [`AgentPlan::schedule`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSchedule {
    pub every_h: u32,
    pub delay_min: u32,
    /// The allowed start window (a valid one), when set.
    pub window: Option<String>,
    /// Set times: a calendar expression (instead of every `every_h`).
    pub calendar: Option<String>,
    /// Also one backup at every start of the PC (its boot, not the agent's restart).
    pub at_boot: bool,
    /// From the shared trigger (a NAS that knows skipped-start reports).
    pub shared: bool,
}

/// The first agent version that follows [`PcPlan::window`] (S6).
pub const WINDOW_AGENT: (u32, u32, u32) = (0, 8, 20);

/// The agent of this version follows the allowed window: a known version older than
/// [`WINDOW_AGENT`] starts whenever a backup is due (the overview draws it so, the settings
/// warn); an unknown one is taken as current.
pub fn agent_follows_window(agent_version: &str) -> bool {
    agent_at_least(agent_version, WINDOW_AGENT)
}

/// The first agent version that follows [`PcPlan::calendar`].
pub const CALENDAR_AGENT: (u32, u32, u32) = (0, 8, 21);

/// The agent of this version follows a calendar plan: an older known one runs every
/// `interval_hours` (the NAS sends it the calendar's spacing); an unknown one is taken as
/// current.
pub fn agent_follows_calendar(agent_version: &str) -> bool {
    agent_at_least(agent_version, CALENDAR_AGENT)
}

/// The first agent version that counts its interval from the start of the last good run
/// (older ones count from its end, so their start time drifts by each run's length).
pub const START_ANCHORED_AGENT: (u32, u32, u32) = (0, 8, 24);

/// The agent of this version counts its interval from the last good run's start (the NAS
/// judges an older one by its end, as that agent counts); an unknown one is taken as current.
pub fn agent_counts_from_start(agent_version: &str) -> bool {
    agent_at_least(agent_version, START_ANCHORED_AGENT)
}

fn agent_at_least(agent_version: &str, min: (u32, u32, u32)) -> bool {
    let mut p = agent_version.trim().trim_start_matches('v').split(['.', '-', '+']);
    let mut num = || p.next().and_then(|x| x.parse::<u32>().ok());
    match (num(), num(), num()) {
        (Some(a), Some(b), Some(c)) => (a, b, c) >= min,
        _ => true,
    }
}

/// `GET /api/v1/pcbackup/agent/btrfs/snapshots` (device token, btrfs method): what the PC's
/// folder holds, so the agent picks the parent of each incremental send (the newest local
/// snapshot whose UUID, or received UUID, is a complete snapshot's `received_uuid` here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentBtrfsListing {
    /// Every entry with a btrbk snapshot name, newest first.
    pub snapshots: Vec<AgentBtrfsSnapshot>,
    /// A receive of this PC is running now.
    #[serde(default)]
    pub receiving: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentBtrfsSnapshot {
    pub name: String,
    /// The PC-side UUID it was sent from (`None`: interrupted or not a received subvolume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_uuid: Option<String>,
    /// Read-only with a received UUID: usable as a parent.
    pub complete: bool,
}

/// Answer of `POST /api/v1/pcbackup/agent/btrfs/receive`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentBtrfsReceived {
    pub name: String,
    pub received_uuid: String,
    /// Bytes of the (zstd-compressed) request body the NAS read.
    #[serde(default)]
    pub bytes: u64,
}

/// The btrfs method's listing (see [`AgentBtrfsListing`]).
pub const AGENT_BTRFS_SNAPSHOTS: &str = "/api/v1/pcbackup/agent/btrfs/snapshots";
/// The btrfs method's upload: `POST …?snapshot=<name.YYYYMMDDTHHMM[_N]>&parent=<name|none>`,
/// body = `btrfs send` stream compressed with zstd (`Content-Type: application/zstd`,
/// chunked). One receive per PC at a time. The stream must be a v1 stream (`btrfs send
/// --proto 1`). Refusals (409) carry the error code: `busy` (this PC's folder, or too many
/// PCs at once), `already_exists` (the snapshot is there), [`PARENT_MISSING`] (send in full).
pub const AGENT_BTRFS_RECEIVE: &str = "/api/v1/pcbackup/agent/btrfs/receive";
/// Error code of a receive whose parent is not a complete snapshot on the NAS (any more):
/// the agent sends the same snapshot again in full.
pub const PARENT_MISSING: &str = "parent_missing";
/// zstd window of the agent's stream (2 MiB, level 3's own); the NAS decodes windows up to
/// [`AGENT_ZSTD_WINDOW_LOG_MAX`] (8 MiB) and refuses larger ones (decoder memory).
pub const AGENT_ZSTD_WINDOW_LOG: u32 = 21;
pub const AGENT_ZSTD_WINDOW_LOG_MAX: u32 = 23;
/// The agent's local snapshot folder for the btrfs method when the PC has no btrbk
/// `snapshot_dir` of its own (btrbk's usual name, so btrbk could take the chain over).
pub const AGENT_SNAPSHOT_DIR: &str = "/.btrbk_snapshots";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PairResponse {
    pub device: String,
    /// The repository path is `/api/v1/pcbackup/repo/<group>/`.
    pub group: String,
    pub token: String,
    pub repo_password: String,
    #[serde(default)]
    pub nas_name: String,
    pub plan: AgentPlan,
    /// What the agent pins instead of the certificate it saw (`None`: a NAS without its local
    /// certificate authority, e.g. a dry system; the agent keeps the certificate pin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<AgentTrust>,
}

/// What an agent trusts of its NAS (`PairResponse::trust`, and [`AGENT_TRUST`] on every
/// contact): the NAS' **local certificate authority** and a stable identity, instead of the
/// exact certificate seen at pairing. The NAS' own HTTPS port serves, to a client that asks
/// for the name [`agent_tls_name`] (SNI), a certificate issued by that authority for that
/// name only; the agent checks the chain up to the pinned authority (validity included) and
/// the name. A new host name, a new address or a re-issued certificate change none of that,
/// so PCs never need pairing again for them; a certificate of another NAS (another
/// authority, or another identity) is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentTrust {
    /// This NAS' identity: 32 lower-case hex digits, made once ([`valid_nas_id`]).
    pub nas_id: String,
    /// The local authority's certificates (PEM, one each): the current one, and ahead of a
    /// rotation the next one too, so agents carry it before the switch.
    pub cas: Vec<String>,
    /// Where the NAS' own HTTPS port answers (`https://<name or address>:<port>`), host
    /// names first: the agent tries them in order when its address stops answering.
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// `GET` (device token): this NAS' [`AgentTrust`] now. 404 on a NAS that has none (older
/// versions, dry systems).
pub const AGENT_TRUST: &str = "/api/v1/pcbackup/agent/trust";
/// The name an agent asks the NAS' HTTPS port for (`<nas id>` + this): a reserved top-level
/// domain (RFC 6761), so it can never be a real name of anything.
pub const AGENT_TLS_SUFFIX: &str = ".rynas.invalid";
/// Most CA certificates an agent keeps (current and next, with room to spare).
pub const AGENT_MAX_CAS: usize = 4;
/// Most addresses an agent keeps.
pub const AGENT_MAX_ADDRESSES: usize = 16;

/// The TLS name of the NAS `nas_id` for its agents.
pub fn agent_tls_name(nas_id: &str) -> String {
    format!("{nas_id}{AGENT_TLS_SUFFIX}")
}

/// A NAS identity: 32 lower-case hex digits.
pub fn valid_nas_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentStatus {
    pub run_id: String,
    pub phase: AgentPhase,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: u64,
    #[serde(default)]
    pub files_done: u64,
    #[serde(default)]
    pub files_total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs: Option<BtrfsStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentReport {
    #[serde(flatten)]
    pub run: PcRun,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
    #[serde(default)]
    pub agent_version: String,
    /// The PC's root filesystem is btrfs (the btrfs method can serve it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_btrfs: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreLogin {
    pub code: String,
}

/// The restore TUI's other login: an administrator with TOTP turned on (an account without
/// TOTP is refused). Checked by rynas-web, which then asks rynasd for a [`RestoreChoice`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreAdminLogin {
    pub username: String,
    pub password: String,
    pub totp: String,
}

/// The PCs an administrator may restore, each with a one-time restore code of its own (5
/// minutes; using one ends the others). The TUI logs in with the chosen PC's code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreChoice {
    pub devices: Vec<RestoreChoiceDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreChoiceDevice {
    pub device: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hostname: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreDevice {
    pub device: String,
    /// The file method's backup group (empty: the PC has no file backups).
    pub group: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hostname: String,
    /// Opens the session's own key of the group's repository (empty without a group).
    pub repo_password: String,
    /// The PC has a folder of btrfs snapshots on the NAS (the agent's btrfs method, or btrbk
    /// over SSH): the restore can read it through [`RESTORE_BTRFS`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub btrfs: bool,
}

impl RestoreDevice {
    /// The PC has file backups on the NAS (a backup group).
    pub fn has_files(&self) -> bool {
        !self.group.is_empty()
    }
}

/// The btrfs routes of a restore session (bearer = the session token), below
/// `/api/v1/pcbackup/agent/restore/btrfs/<device>/`:
/// - `snapshots`: [`RestoreBtrfsListing`];
/// - `manifest?snapshot=<name>`: `/.rynas/manifest.json` of that snapshot, as the PC wrote it
///   (JSON, at most [`MAX_MANIFEST`]; 404 `not_found` when the snapshot has none);
/// - `legacy?snapshot=<name>`: [`RestoreLegacy`], what an older restore script left inside a
///   root snapshot without manifest;
/// - `send?snapshot=<name>`: `btrfs send` (full) of a complete read-only snapshot of the
///   PC's folder compressed with zstd (`application/zstd`), one stream per PC at a time.
pub const RESTORE_BTRFS: &str = "/api/v1/pcbackup/agent/restore/btrfs";

/// `GET …/restore/btrfs/<device>/snapshots`: the PC's received snapshots and its runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreBtrfsListing {
    pub device: String,
    /// Fed by `rynas-agent` (otherwise btrbk over SSH).
    #[serde(default)]
    pub agent: bool,
    /// Every snapshot, newest first, with its subvolume (`group`), state and size when known.
    pub snapshots: Vec<crate::pcbackup::ReceivedSnapshot>,
    /// The snapshots grouped by the timestamp of their names, newest first.
    pub runs: Vec<crate::pcbackup::PcRun>,
    /// The snapshot whose manifest the NAS reads for the bare-metal check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_snapshot: Option<String>,
}

/// A text file of a root snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LegacyFile {
    /// Its name inside the folder it was found in.
    pub name: String,
    pub text: String,
}

/// `GET …/restore/btrfs/<device>/legacy?snapshot=`: a root snapshot written before the
/// manifest existed (an older restore script's hook): `/etc/fstab`, os-release, partition
/// table dumps (`sfdisk --dump` text found in `/.bootbackup/`, whatever their names) and the
/// boot folder copies in `/.bootbackup/` (folders ending in `_post`, newest last).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreLegacy {
    pub snapshot: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fstab: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_release: Option<String>,
    #[serde(default)]
    pub partition_dumps: Vec<LegacyFile>,
    #[serde(default)]
    pub boot_backups: Vec<String>,
}

/// Folder of the older restore script's copies inside the root subvolume.
pub const LEGACY_BOOT_DIR: &str = ".bootbackup";
/// Largest text file read for [`RestoreLegacy`].
pub const LEGACY_FILE_MAX: usize = 64 << 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreSession {
    pub token: String,
    pub expires_ms: u64,
    pub devices: Vec<RestoreDevice>,
}

// ------------------------------------------------------------------ restore approved from a phone

/// `POST /api/v1/pcbackup/agent/restore-requests` (no session): the restore TUI asks an
/// administrator to approve a bare-metal restore (`docs/phase2/pc-restore-approval.md`).
pub const AGENT_RESTORE_REQUESTS: &str = "/api/v1/pcbackup/agent/restore-requests";
/// A restore request waits this long for a decision.
pub const RESTORE_REQUEST_TTL_MS: u64 = 10 * 60 * 1000;
/// Longest wait of the TUI's poll (`?wait=` seconds).
pub const RESTORE_REQUEST_POLL_WAIT_S: u64 = 25;
/// Longest PC label a requester may claim (characters, after trimming).
pub const RESTORE_REQUEST_MAX_PC: usize = 64;
/// `kind` of the push action and of the QR scheme of a restore request.
pub const PUSH_ACTION_PC_RESTORE: &str = "pc_restore";

/// A request not revealed ([`RestoreRequestReveal`]) within this long is dropped.
pub const RESTORE_REVEAL_WITHIN_MS: u64 = 60 * 1000;
/// Wrong codes typed by approvers before a request is denied by itself.
pub const RESTORE_MAX_WRONG_CODES: u8 = 3;

/// The body of a new restore request: only what the person at the dead PC says, and the
/// commitment to its secret. Nothing the NAS knows comes back before an administrator
/// approved (no PC list, no names, no code).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreRequestCreate {
    /// The PC as the person at it calls it (its host name, or the PC's name on the NAS);
    /// shown to the approver as a claim, matched against the NAS' PCs only on the NAS.
    pub pc: String,
    /// SHA-256 of the certificate the TUI sees (`AA:BB:…`). Over TLS it must be the NAS' own
    /// (else 409 `certificate_mismatch`); empty only over plain HTTP.
    #[serde(default)]
    pub fingerprint: String,
    /// `hex(SHA-256(restore_commit_input(secret)))`: the TUI's 256-bit secret committed
    /// before it learns the NAS' nonce ([`restore_commit_input`]).
    pub commit: String,
}

/// 201 of a new restore request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreRequestCreated {
    /// 64 random bits (16 hex digits): names the request in the QR code and to the approver.
    /// Not a secret (the poll token is); short so that the QR code fits a text console.
    pub id: String,
    /// The TUI's secret (256 bits, hex): needed to reveal, poll, withdraw and receive the
    /// approval. Never in the QR code, never shown.
    pub poll_token: String,
    /// Random (hex), made after the commitment arrived: with the TUI's secret and the
    /// certificate it gives the compare code ([`restore_compare_input`]).
    pub nonce: String,
    /// This NAS' identity for the app (`GET /api/v1/ping` `sid`): the QR code names it.
    pub sid: String,
    pub expires_ms: u64,
}

/// `POST /api/v1/pcbackup/agent/restore-requests/{id}/reveal` (poll token as bearer): the
/// secret behind the commitment. Only then do administrators see the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreRequestReveal {
    /// 64 hex digits.
    pub secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestoreRequestState {
    #[default]
    Pending,
    Approved,
    Denied,
    Expired,
    /// Withdrawn by the TUI (Esc).
    Canceled,
}

impl RestoreRequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Canceled => "canceled",
        }
    }
}

/// What an approval gives the TUI: a restore code exactly like the one made in the web
/// interface (one-time, read-only, this PC only, 1 hour), used with
/// `POST /api/v1/pcbackup/agent/restore/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreApproval {
    pub code: String,
    pub device: String,
    pub expires_ms: u64,
}

/// `GET /api/v1/pcbackup/agent/restore-requests/{id}?wait=<s>` with the poll token as bearer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreRequestPoll {
    pub state: RestoreRequestState,
    pub expires_ms: u64,
    /// Only in `approved`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<RestoreApproval>,
}

/// A pending restore request as administrators see it (`GET /api/v1/pcbackup/restore-requests`,
/// the event `pcbackup.restore_request`). The compare code is never in it: the approver types
/// the digits the PC's screen shows ([`RestoreRequestApprove::code`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreRequestView {
    pub id: String,
    /// The PC as the requester claimed it (not checked: show it as a claim).
    pub pc: String,
    /// The backup PC whose name or host name the claim matches when exactly one does
    /// (case-insensitive); `null`: the approver picks one. A suggestion from the claim, never
    /// a check: show it as such.
    pub device: Option<String>,
    /// The address the request came from, as the NAS saw it.
    pub from_ip: String,
    /// The request came over TLS (its compare code binds the NAS' certificate). `false`:
    /// plain HTTP, allowed only by [`PcAgentSettings::restore_requests_over_http`]; show a
    /// red warning.
    pub tls: bool,
    /// Other waiting requests for the same PC (the same match, or the same claim): warn
    /// clearly when above 0, only one of them can be the person you know.
    #[serde(default)]
    pub same_pc: u32,
    pub created_ms: u64,
    pub expires_ms: u64,
}

/// `POST /api/v1/pcbackup/restore-requests/{id}/approve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreRequestApprove {
    /// The six digits the PC's screen shows, typed by the approver (or read from the QR code
    /// on that screen); spaces ignored. A wrong code is 400 `code_mismatch`; after
    /// [`RESTORE_MAX_WRONG_CODES`] the request is denied. Missing: 400 with `field` `code`.
    #[serde(default)]
    pub code: String,
    /// The PC whose backups the restore may read; may be left out only when the request
    /// matched one ([`RestoreRequestView::device`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// `POST /api/v1/pcbackup/restore-requests/{id}/deny` (body optional).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreRequestDeny {
    /// Also refuse new requests from that address (its IPv6 /64) for an hour.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub block: bool,
}

/// The event `pcbackup.restore_resolved`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreRequestResolved {
    pub id: String,
    /// `approved`, `denied`, `expired` or `canceled`.
    pub outcome: RestoreRequestState,
}

/// `GET /api/v1/pcbackup/devices`: every PC the NAS can restore (file backups, a folder of
/// btrfs snapshots, or both), for the approver's choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestorablePc {
    pub device: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hostname: String,
    /// File backups (a backup group).
    pub files: bool,
    /// A folder of btrfs snapshots.
    pub btrfs: bool,
}

/// The text whose SHA-256 (hex) is the TUI's commitment to `secret`.
pub fn restore_commit_input(secret: &str) -> String {
    format!("rynas-restore-commit-v1\n{secret}")
}

/// The text whose SHA-256 gives a restore request's compare code (ZRTP-style short
/// authentication string): the TUI's secret (committed before the nonce existed), the NAS'
/// nonce and the certificate, as upper-case hex without separators (empty over plain HTTP).
/// The NAS uses its own certificate, the TUI the one it really saw: a party in the middle
/// has its own certificate, cannot change the secret after the commitment nor know it before
/// it chose the nonce it shows the TUI, so the two sets of digits differ but by chance
/// (1 in a million per request that reaches an approver).
pub fn restore_compare_input(secret: &str, nonce: &str, fingerprint: &str) -> String {
    let fp: String = fingerprint
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    format!("rynas-restore-v2\n{secret}\n{nonce}\n{fp}")
}

/// The six digits of a compare code from the SHA-256 of [`restore_compare_input`]: its first
/// four bytes, big-endian, modulo 1 000 000, zero-padded.
pub fn restore_compare_code(sha256: &[u8]) -> String {
    let b = [sha256.first(), sha256.get(1), sha256.get(2), sha256.get(3)].map(|x| x.copied().unwrap_or(0));
    format!("{:06}", u32::from_be_bytes(b) % 1_000_000)
}

/// `482 913`: a compare code as people read it.
pub fn restore_code_text(code: &str) -> String {
    if code.len() == 6 && code.is_ascii() {
        format!("{} {}", &code[..3], &code[3..])
    } else {
        code.to_string()
    }
}

/// What the restore TUI's QR code carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestoreQr {
    /// The NAS' `sid`.
    pub nas: String,
    /// The request's id.
    pub req: String,
    /// The compare code the TUI made (six digits).
    pub code: String,
}

impl RestoreQr {
    /// `rynas-restore:v1?nas=<sid>&req=<id>&code=<digits>` (every value is base64url, hex or
    /// digits: nothing to escape).
    pub fn link(&self) -> String {
        format!("rynas-restore:v1?nas={}&req={}&code={}", self.nas, self.req, self.code)
    }

    /// The link read back; `None` for another scheme or version, a missing or malformed
    /// value. Unknown parameters are ignored (later versions may add some).
    pub fn parse(s: &str) -> Option<RestoreQr> {
        let q = s.trim().strip_prefix("rynas-restore:v1?")?;
        let mut out = RestoreQr::default();
        for kv in q.split('&') {
            let (k, v) = kv.split_once('=')?;
            let slot = match k {
                "nas" => &mut out.nas,
                "req" => &mut out.req,
                "code" => &mut out.code,
                _ => continue,
            };
            *slot = v.to_string();
        }
        let b64u = |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        let hex = |s: &str| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit());
        let digits = out.code.len() == 6 && out.code.bytes().all(|b| b.is_ascii_digit());
        (b64u(&out.nas) && hex(&out.req) && digits).then_some(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RestoreReport {
    pub device: String,
    pub version: String,
    pub ok: bool,
    #[serde(default)]
    pub message: String,
    /// The end of the restore log (up to 64 KiB).
    #[serde(default)]
    pub log: String,
}

/// One entry of a restic REST v2 listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestEntry {
    pub name: String,
    pub size: u64,
}

// ------------------------------------------------------------------ the layout manifest

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Loader {
    Grub,
    SystemdBoot,
    Refind,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InitramfsTool {
    UpdateInitramfs,
    Mkinitcpio,
    Dracut,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BootInfo {
    pub uefi: bool,
    pub loader: Loader,
    #[serde(default)]
    pub shim: bool,
    #[serde(default)]
    pub uki: bool,
    /// Mount point of the ESP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub esp: Option<String>,
    /// The loader's folder in `EFI/` (`debian`, `arch`, `fedora`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub efi_id: Option<String>,
    pub initramfs: InitramfsTool,
    /// `/proc/cmdline` at capture.
    #[serde(default)]
    pub cmdline: String,
    /// `/etc/kernel/cmdline`, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_cmdline: Option<String>,
    /// `GRUB_CMDLINE_LINUX` of `/etc/default/grub`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grub_cmdline: Option<String>,
    /// Installed kernel versions (`/usr/lib/modules`).
    #[serde(default)]
    pub kernels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Partition {
    pub number: u32,
    pub start_lba: u64,
    pub size_lba: u64,
    /// GPT type GUID (or `0x83`-style MBR type).
    pub type_guid: String,
    #[serde(default)]
    pub part_guid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub attrs: u64,
    /// Kernel name at capture (`nvme0n1p2`).
    #[serde(default)]
    pub dev: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PartTable {
    /// `gpt` or `dos`.
    pub kind: String,
    #[serde(default)]
    pub disk_guid: String,
    #[serde(default)]
    pub first_lba: u64,
    #[serde(default)]
    pub last_lba: u64,
    #[serde(default)]
    pub partitions: Vec<Partition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Disk {
    /// Kernel name at capture (`nvme0n1`); never used to find the disk again.
    pub name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub wwn: String,
    pub size_bytes: u64,
    #[serde(default = "default_sector")]
    pub logical_sector: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<PartTable>,
}

fn default_sector() -> u32 {
    512
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MountEntry {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subvol: Option<String>,
    #[serde(default)]
    pub options: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Subvolume {
    /// Path from the top level (`@home`, `@/var/lib/machines`).
    pub path: String,
    #[serde(default)]
    pub id: u64,
    /// Included in the backup (otherwise recreated empty).
    #[serde(default)]
    pub captured: bool,
    #[serde(default)]
    pub nocow: bool,
}

/// Where a filesystem sits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FsSource {
    /// A partition of a disk of the manifest (`disk` = its index in `disks`).
    Partition { disk: usize, number: u32 },
    /// An opened LUKS container (index in `luks`).
    Luks { luks: usize },
    /// An LVM logical volume.
    Lvm { vg: String, lv: String },
    /// An md array.
    Md { name: String },
    /// A whole disk (index in `disks`).
    Disk { disk: usize },
    #[default]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Filesystem {
    /// Device at capture (`/dev/nvme0n1p2`, `/dev/mapper/root`).
    pub device: String,
    pub source: FsSource,
    pub fstype: String,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub mounts: Vec<MountEntry>,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub used_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureMethod>,
    /// Superblock features (ext: `metadata_csum`, `64bit`…), for information.
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub subvolumes: Vec<Subvolume>,
    /// Default subvolume (btrfs), as a path from the top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_subvol: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Luks {
    /// The container's device (`/dev/nvme0n1p3`).
    pub device: String,
    pub source: FsSource,
    pub uuid: String,
    #[serde(default)]
    pub label: String,
    /// 1 or 2.
    pub version: u32,
    #[serde(default)]
    pub cipher: String,
    #[serde(default)]
    pub key_bits: u32,
    #[serde(default)]
    pub pbkdf: String,
    /// The mapper name it was open as.
    #[serde(default)]
    pub mapper: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Swap {
    /// `/dev/…` or a file path.
    pub path: String,
    #[serde(default)]
    pub file: bool,
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub source: FsSource,
}

/// The LVM layout of [`Manifest::lvm_report`], as the bare-metal restore reads it: LVM's
/// JSON report with the `lv` rows of `lvs` (one per segment when segment fields were asked)
/// and, from agent 0.8.12 on, the `vg` and `pv` rows of `vgs` and `pvs`. What an older agent
/// did not record is empty or 0 (UUIDs, extent size, PV sizes, segment types).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LvmLayout {
    pub vgs: Vec<LvmVg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LvmVg {
    pub name: String,
    pub uuid: String,
    /// LVM's system ID of the group (`vg_systemid`; empty: none, or not recorded).
    pub system_id: String,
    /// 0: not recorded.
    pub extent_bytes: u64,
    pub size_bytes: u64,
    pub free_bytes: u64,
    pub pvs: Vec<LvmPv>,
    /// In the report's order.
    pub lvs: Vec<LvmLv>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LvmPv {
    /// `/dev/nvme0n1p2`, `/dev/mapper/luks-…`.
    pub device: String,
    pub uuid: String,
    pub size_bytes: u64,
    pub pe_start_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LvmLv {
    pub name: String,
    pub uuid: String,
    pub size_bytes: u64,
    /// `lv_attr` (`-wi-ao----`; the first letter is the volume type).
    pub attr: String,
    /// The segment types (`linear`, `striped`, `thin`, `raid1`…), when recorded.
    pub segtypes: Vec<String>,
    /// The PVs it sits on, without the extent ranges.
    pub devices: Vec<String>,
    pub pool: String,
    pub origin: String,
}

/// A size of an LVM report taken with `--units b` (`49392123904B`; without the suffix too).
fn lvm_bytes(s: &str) -> u64 {
    s.trim().trim_end_matches(['B', 'b']).parse().unwrap_or(0)
}

impl LvmLayout {
    pub fn parse(report: &str) -> Result<LvmLayout, String> {
        let v: serde_json::Value = serde_json::from_str(report).map_err(|e| format!("the LVM report cannot be read: {e}"))?;
        let reports = v.get("report").and_then(|r| r.as_array()).ok_or("the LVM report has no \"report\" list")?;
        let rows = |key: &str| -> Vec<&serde_json::Map<String, serde_json::Value>> {
            reports
                .iter()
                .filter_map(|r| r.get(key).and_then(|a| a.as_array()))
                .flatten()
                .filter_map(|x| x.as_object())
                .collect()
        };
        let s = |row: &serde_json::Map<String, serde_json::Value>, k: &str| row.get(k).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
        let mut out = LvmLayout::default();
        fn vg_of<'a>(out: &'a mut LvmLayout, name: &str) -> &'a mut LvmVg {
            let i = match out.vgs.iter().position(|v| v.name == name) {
                Some(i) => i,
                None => {
                    out.vgs.push(LvmVg { name: name.to_string(), ..Default::default() });
                    out.vgs.len() - 1
                }
            };
            &mut out.vgs[i]
        }
        for r in rows("vg") {
            let name = s(r, "vg_name");
            if name.is_empty() {
                continue;
            }
            let vg = vg_of(&mut out, &name);
            vg.uuid = s(r, "vg_uuid");
            vg.system_id = s(r, "vg_systemid");
            vg.extent_bytes = lvm_bytes(&s(r, "vg_extent_size"));
            vg.size_bytes = lvm_bytes(&s(r, "vg_size"));
            vg.free_bytes = lvm_bytes(&s(r, "vg_free"));
        }
        for r in rows("pv") {
            let (name, device) = (s(r, "vg_name"), s(r, "pv_name"));
            if name.is_empty() || device.is_empty() {
                continue;
            }
            let vg = vg_of(&mut out, &name);
            if !vg.pvs.iter().any(|p| p.device == device) {
                vg.pvs.push(LvmPv { device, uuid: s(r, "pv_uuid"), size_bytes: lvm_bytes(&s(r, "pv_size")), pe_start_bytes: lvm_bytes(&s(r, "pe_start")) });
            }
        }
        // the agent's own snapshots of a backup that was interrupted (an older agent kept
        // them in the report): not part of the PC's layout, their extents are free space
        let mut freed: Vec<(String, String, u64)> = Vec::new();
        for r in rows("lv").into_iter().chain(rows("seg")) {
            let (name, lv_name) = (s(r, "vg_name"), s(r, "lv_name"));
            if name.is_empty() || lv_name.is_empty() {
                continue;
            }
            if lv_name.ends_with(LVM_OWN_SNAPSHOT_SUFFIX) {
                let seg = lvm_bytes(&s(r, "seg_size"));
                match freed.iter_mut().find(|(v, l, _)| *v == name && *l == lv_name) {
                    // one row per segment: their sizes add up; without segment sizes, the
                    // volume's size once
                    Some(f) if seg > 0 => f.2 += seg,
                    Some(_) => {}
                    None => freed.push((name, lv_name, if seg > 0 { seg } else { lvm_bytes(&s(r, "lv_size")) })),
                }
                continue;
            }
            let vg = vg_of(&mut out, &name);
            if vg.extent_bytes == 0 {
                vg.extent_bytes = lvm_bytes(&s(r, "vg_extent_size"));
            }
            let i = match vg.lvs.iter().position(|l| l.name == lv_name) {
                Some(i) => i,
                None => {
                    vg.lvs.push(LvmLv { name: lv_name, ..Default::default() });
                    vg.lvs.len() - 1
                }
            };
            let lv = &mut vg.lvs[i];
            for (field, key) in [(&mut lv.uuid, "lv_uuid"), (&mut lv.attr, "lv_attr"), (&mut lv.pool, "pool_lv"), (&mut lv.origin, "origin")] {
                if field.is_empty() {
                    *field = s(r, key);
                }
            }
            if lv.size_bytes == 0 {
                lv.size_bytes = lvm_bytes(&s(r, "lv_size"));
            }
            let seg = s(r, "segtype");
            if !seg.is_empty() {
                lv.segtypes.push(seg);
            }
            for d in s(r, "devices").split(',') {
                let d = d.split('(').next().unwrap_or("").trim();
                if !d.is_empty() && !lv.devices.iter().any(|x| x == d) {
                    lv.devices.push(d.to_string());
                }
            }
        }
        for (name, _, bytes) in freed {
            // a report without `vg` rows has no free space to add to (it comes from the PVs)
            if let Some(vg) = out.vgs.iter_mut().find(|v| v.name == name && v.size_bytes > 0) {
                vg.free_bytes += bytes;
            }
        }
        // an older report has no PV rows: the devices the volumes sit on
        for vg in &mut out.vgs {
            if vg.pvs.is_empty() {
                let devs: Vec<String> = vg.lvs.iter().flat_map(|l| l.devices.iter().cloned()).collect();
                for d in devs {
                    if !d.starts_with('[') && !vg.pvs.iter().any(|p| p.device == d) {
                        vg.pvs.push(LvmPv { device: d, ..Default::default() });
                    }
                }
            }
        }
        Ok(out)
    }

    /// What the restore does not recreate, one sentence per volume (`vg/lv: a thin pool`).
    /// Linear volumes are recreated; an older report without segment types is trusted for a
    /// volume on one device only (on several it could be striped).
    pub fn unsupported(&self) -> Vec<String> {
        let mut out = Vec::new();
        for vg in &self.vgs {
            for lv in &vg.lvs {
                let kind = match lv.attr.chars().next() {
                    None | Some('-') | Some('o') => None,
                    Some('t') => Some("a thin pool".to_string()),
                    Some('V') => Some("a thin volume".to_string()),
                    Some('s') | Some('S') => Some("a snapshot".to_string()),
                    Some('r') | Some('R') => Some("a raid volume".to_string()),
                    Some('m') | Some('M') => Some("a mirror".to_string()),
                    Some('C') => Some("a cache volume".to_string()),
                    Some('d') | Some('D') => Some("a VDO volume".to_string()),
                    Some(c) => Some(format!("a volume of type '{c}'")),
                };
                let kind = kind
                    .or_else(|| lv.segtypes.iter().find(|t| t.as_str() != "linear").map(|t| format!("{t} segments")))
                    .or_else(|| {
                        (lv.segtypes.is_empty() && lv.devices.len() > 1)
                            .then(|| "on several devices, and the backup's manifest does not say whether it is striped (a backup by a newer agent says)".to_string())
                    });
                if let Some(k) = kind {
                    out.push(format!("{}/{}: {k}", vg.name, lv.name));
                }
            }
        }
        out
    }
}

/// The name ending of the agent's own LVM snapshots while a backup reads a volume.
pub const LVM_OWN_SNAPSHOT_SUFFIX: &str = "-rynas-snap";

/// The kernel name of partition `n` of disk `disk` (`sda1`, `nvme0n1p1`, `mmcblk0p1`).
pub fn partition_name(disk: &str, n: u32) -> String {
    if disk.chars().last().is_some_and(|c| c.is_ascii_digit()) {
        format!("{disk}p{n}")
    } else {
        format!("{disk}{n}")
    }
}

/// What the bare-metal restore does with a volume group of the manifest
/// ([`Manifest::lvm_vg_check`]); the agent's plan and the NAS's readiness use the same rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LvmVgCheck {
    /// No physical volume on a restored disk (and not the root's group): left as it is.
    Elsewhere,
    /// Made again on the restored disk.
    Recreate,
    /// Not something the restore makes again (why), but it holds nothing of the backup: its
    /// partitions are made again, empty.
    SkipUnused(Vec<String>),
    /// Not something the restore makes again (why), and the backup has data on it.
    Refuse(Vec<String>),
}

/// Everything a bare-metal restore needs besides the files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Manifest {
    /// [`MANIFEST_FORMAT`]; a reader refuses a format it does not know.
    pub format: u32,
    pub written_at_ms: u64,
    #[serde(default)]
    pub agent_version: String,
    pub hostname: String,
    #[serde(default)]
    pub machine_id: String,
    #[serde(default)]
    pub arch: String,
    /// `ID`, `VERSION_ID`, `PRETTY_NAME`, `ID_LIKE` of os-release.
    #[serde(default)]
    pub os_release: BTreeMap<String, String>,
    pub boot: BootInfo,
    pub disks: Vec<Disk>,
    #[serde(default)]
    pub filesystems: Vec<Filesystem>,
    #[serde(default)]
    pub luks: Vec<Luks>,
    #[serde(default)]
    pub swap: Vec<Swap>,
    /// `vgs`/`lvs --reportformat json`, when LVM is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lvm_report: Option<String>,
    /// The active `filter = […]` and `global_filter = […]` settings of the PC's
    /// `/etc/lvm/lvm.conf` and `lvmlocal.conf`, as written there (they may name the old
    /// disks: the restore rewrites the names, the plan shows them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lvm_filters: Vec<String>,
    /// `mdadm --detail --export` of each array.
    #[serde(default)]
    pub md: Vec<String>,
    #[serde(default)]
    pub fstab: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crypttab: Option<String>,
    /// The btrfs method: btrbk's names of the subvolumes (`docs/research/pc-backup.md`
    /// §11.3). Absent on PCs of the file method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrbk: Option<BtrbkLayout>,
}

/// The btrfs method's block of the manifest (btrbk over SSH, or the agent itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]

pub struct BtrbkLayout {
    /// The local snapshot folder on the PC (btrbk's `snapshot_dir`, or the agent's).
    #[serde(default)]
    pub snapshot_dir: String,
    pub subvolumes: Vec<BtrbkSubvolume>,
    /// Boot filesystems outside every sent subvolume (the ESP, a separate non-btrfs
    /// `/boot`), copied as files into the root subvolume so its snapshots carry them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boot_copies: Vec<BootCopy>,
    /// Sent by `rynas-agent` itself over HTTPS (the btrfs method without btrbk); `false`:
    /// btrbk over SSH (`/etc/btrbk/btrbk.conf`). The names are btrbk's either way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent: bool,
}

/// One boot filesystem copied into `/.rynas/boot/<name>/` (one copy kept, changed files
/// replaced one by one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]

pub struct BootCopy {
    /// The mount point copied (`/boot/efi`).
    pub mount: String,
    /// Where the copy is on the PC (`/.rynas/boot/boot-efi`).
    pub dir: String,
    pub bytes: u64,
    pub files: u64,
    /// SHA-256 of the copied names, sizes and times: an unchanged tree is not copied again.
    #[serde(default)]
    pub signature: String,
    /// Files left out because they look like key material (`*.key`, `*keyfile*`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// The last copy failed: this record (and the copy on disk) is the previous good one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// Why the last copy failed (with `stale`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One subvolume the btrfs method sends: btrbk name → subvolume path → mount point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BtrbkSubvolume {
    /// btrbk's name (the prefix of its snapshots, `@home`).
    pub name: String,
    /// Path from the top level.
    #[serde(default)]
    pub path: String,
    /// Where it is mounted (`/home`); empty when not mounted.
    #[serde(default)]
    pub mount: String,
    /// UUID of the btrfs filesystem that holds it; empty: the one mounted at `/` (older
    /// manifests, and the agent's method, which sends only that filesystem's subvolumes).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fs_uuid: String,
    /// Not mounted: nested inside another sent subvolume and reached through it; the btrbk
    /// name of that parent (empty for a mounted subvolume).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nested_in: String,
    /// With `nested_in`: its path inside the parent (`games` for `@home/games`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub inside: String,
}

impl BtrbkLayout {
    /// Where `s` is on the PC: its mount point, or for a nested subvolume its parent's place
    /// joined with its path inside it (`/home/games`). `None` when the chain of parents is
    /// broken (a parent missing, a loop).
    pub fn place_of(&self, s: &BtrbkSubvolume) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        let mut cur = s;
        for _ in 0..32 {
            if cur.nested_in.is_empty() {
                if cur.mount.is_empty() {
                    return None;
                }
                let base = cur.mount.trim_end_matches('/');
                let rest: Vec<&str> = parts.iter().rev().map(|p| p.trim_matches('/')).filter(|p| !p.is_empty()).collect();
                return Some(if rest.is_empty() {
                    if base.is_empty() { "/".into() } else { base.to_string() }
                } else {
                    format!("{base}/{}", rest.join("/"))
                });
            }
            parts.push(&cur.inside);
            cur = self.subvolumes.iter().find(|p| p.name == cur.nested_in)?;
        }
        None
    }
}

impl Manifest {
    /// The filesystem mounted at `/`.
    pub fn root(&self) -> Option<&Filesystem> {
        self.filesystems
            .iter()
            .find(|f| f.mounts.iter().any(|m| m.path == "/"))
    }

    /// The btrfs method: subvolumes of the root filesystem mounted on the PC that the run
    /// writing this manifest does not send, other than those left out on purpose
    /// ([`not_sent_reason`], hidden by a later mount): `(mount point, subvolume path)`, so an
    /// interface never says "up to date" for a PC whose `/home` is not in the backup.
    /// Empty without the btrfs block.
    pub fn unsent_mounts(&self) -> Vec<(String, String)> {
        let (Some(block), Some(root)) = (self.btrbk.as_ref(), self.root()) else {
            return Vec::new();
        };
        let trim = |s: &str| s.trim_matches('/').to_string();
        let sent: Vec<String> = block
            .subvolumes
            .iter()
            .filter(|s| s.fs_uuid.is_empty() || s.fs_uuid == root.uuid)
            .map(|s| trim(&s.path))
            .collect();
        let mut out: Vec<(String, String)> = Vec::new();
        for (i, m) in root.mounts.iter().enumerate() {
            let Some(sv) = m.subvol.as_deref().map(trim) else {
                continue;
            };
            // a later mount on the same path (of this filesystem or of another) hides this
            // one: the agent does not send what it cannot reach
            let shadowed = root.mounts[i + 1..].iter().any(|o| o.path == m.path)
                || self
                    .filesystems
                    .iter()
                    .filter(|f| !std::ptr::eq(*f, root))
                    .any(|f| f.mounts.iter().any(|o| o.path == m.path));
            if sv.is_empty()
                || shadowed
                || not_sent_reason(&m.path, &sv).is_some()
                || sent.contains(&sv)
                || out.iter().any(|(_, s)| *s == sv)
            {
                continue;
            }
            out.push((m.path.clone(), sv));
        }
        out
    }

    /// The bare-metal readiness of a version with this manifest (without the NAS-side
    /// reasons: versions, recovery kit, age).
    pub fn ready_reasons(&self) -> Vec<ReadyReason> {
        let mut r = Vec::new();
        if self.disks.iter().all(|d| d.table.is_none()) {
            r.push(ReadyReason::NoPartitionTable);
        }
        if self.boot.loader == Loader::Unknown {
            r.push(ReadyReason::LoaderUnknown);
        }
        if self
            .filesystems
            .iter()
            .any(|f| !f.mounts.is_empty() && f.capture == Some(CaptureMethod::Skipped))
        {
            r.push(ReadyReason::VolumeSkipped);
        }
        if !self.md.is_empty()
            || self
                .filesystems
                .iter()
                .any(|f| matches!(f.source, FsSource::Md { .. }))
            || !self.lvm_unsupported().is_empty()
        {
            r.push(ReadyReason::ManualLayout);
        }
        r
    }

    pub fn lvm_unsupported(&self) -> Vec<String> {
        let used = self.lvm_report.is_some()
            || self.filesystems.iter().any(|f| matches!(f.source, FsSource::Lvm { .. }))
            || self.swap.iter().any(|s| matches!(s.source, FsSource::Lvm { .. }))
            || self.luks.iter().any(|l| matches!(l.source, FsSource::Lvm { .. }));
        if !used {
            return Vec::new();
        }
        let layout = match self.lvm_report.as_deref().map(LvmLayout::parse) {
            None => return vec!["the backup's manifest has no LVM report".into()],
            Some(Err(e)) => return vec![e],
            Some(Ok(l)) => l,
        };
        let Some(disk) = self.restore_disk() else {
            // no disk to restore (another ready reason says so): the volumes' kinds alone
            return layout.unsupported();
        };
        let mut out = Vec::new();
        for vg in &layout.vgs {
            if let LvmVgCheck::Refuse(why) = self.lvm_vg_check(vg, &|d| d == disk) {
                out.extend(why);
            }
        }
        out
    }

    /// Physical volumes the LVM report names by an alias (a udev symlink such as
    /// `/dev/gpt-auto-root` or `/dev/disk/by-…`, which older agents recorded as LVM printed
    /// it) are renamed to their partition (`/dev/nvme0n1p2`), in the PV rows and in the
    /// volumes' `devices`, when that is certain: exactly one partition of the manifest's disks
    /// has the PV's size (its data start plus its extents, within one extent and the 1 MiB
    /// alignment) and holds nothing else of the manifest (no filesystem, LUKS container or
    /// swap). Names already known (a partition, a LUKS container) stay; an alias that does
    /// not fit exactly one partition stays too (the restore then refuses the group, as before).
    /// Returns whether something was renamed.
    pub fn normalize_lvm_aliases(&mut self) -> bool {
        let Some(report) = self.lvm_report.clone() else { return false };
        let Ok(layout) = LvmLayout::parse(&report) else { return false };
        let known = |m: &Manifest, dev: &str| {
            m.disks.iter().any(|d| d.table.as_ref().is_some_and(|t| t.partitions.iter().any(|p| format!("/dev/{}", partition_name(&d.name, p.number)) == dev)))
                || m.luks_of_pv(dev).is_some()
        };
        // a partition already named as a PV (or picked for another alias) is never a candidate
        let mut taken: std::collections::HashSet<String> = layout.vgs.iter().flat_map(|v| v.pvs.iter().map(|p| p.device.clone())).collect();
        let mut renames: Vec<(String, String)> = Vec::new();
        for vg in &layout.vgs {
            for pv in &vg.pvs {
                if pv.device.is_empty() || known(self, &pv.device) || pv.size_bytes == 0 {
                    continue;
                }
                let need = pv.size_bytes + pv.pe_start_bytes;
                let slack = vg.extent_bytes.max(1 << 20) + (1 << 20);
                let mut fits: Vec<String> = Vec::new();
                for d in &self.disks {
                    let Some(t) = &d.table else { continue };
                    let sector = u64::from(if d.logical_sector == 0 { 512 } else { d.logical_sector });
                    for p in &t.partitions {
                        let bytes = p.size_lba * sector;
                        let dev = format!("/dev/{}", partition_name(&d.name, p.number));
                        if bytes >= need && bytes < need + slack && !taken.contains(&dev) && !self.partition_used(&dev) {
                            fits.push(dev);
                        }
                    }
                }
                if let [only] = fits.as_slice() {
                    taken.insert(only.clone());
                    renames.push((pv.device.clone(), only.clone()));
                }
            }
        }
        if renames.is_empty() {
            return false;
        }
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&report) else { return false };
        let rename = |s: &str| -> String {
            // `path` or `path(extent)`, comma separated
            s.split(',')
                .map(|part| {
                    let (path, rest) = part.split_once('(').map_or((part, String::new()), |(p, r)| (p, format!("({r}")));
                    let path = renames.iter().find(|(from, _)| from == path.trim()).map_or(path.to_string(), |(_, to)| to.clone());
                    format!("{path}{rest}")
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        for r in v.get_mut("report").and_then(|r| r.as_array_mut()).into_iter().flatten() {
            for (key, field) in [("pv", "pv_name"), ("lv", "devices")] {
                for row in r.get_mut(key).and_then(|x| x.as_array_mut()).into_iter().flatten() {
                    if let Some(s) = row.get(field).and_then(|x| x.as_str()).map(rename) {
                        row[field] = serde_json::Value::String(s);
                    }
                }
            }
        }
        self.lvm_report = Some(v.to_string());
        true
    }

    /// A partition (`/dev/…`) holds a filesystem, a LUKS container or swap of the manifest.
    fn partition_used(&self, dev: &str) -> bool {
        let on = |src: &FsSource| match src {
            FsSource::Partition { disk, number } => self.disks.get(*disk).is_some_and(|d| format!("/dev/{}", partition_name(&d.name, *number)) == dev),
            _ => false,
        };
        self.filesystems.iter().any(|f| on(&f.source)) || self.swap.iter().any(|s| on(&s.source)) || self.luks.iter().any(|l| on(&l.source) || l.device == dev)
    }

    /// The disk index of an LVM physical volume of the manifest: a partition of a disk with
    /// a partition table, or a LUKS container on one.
    pub fn lvm_pv_disk(&self, device: &str) -> Option<usize> {
        for (i, d) in self.disks.iter().enumerate() {
            if d.table.as_ref().is_some_and(|t| t.partitions.iter().any(|p| format!("/dev/{}", partition_name(&d.name, p.number)) == device)) {
                return Some(i);
            }
        }
        self.luks_of_pv(device).and_then(|l| match l.source {
            FsSource::Partition { disk, .. } => Some(disk),
            _ => None,
        })
    }

    /// The LUKS container a PV device names (`/dev/sda2` or `/dev/mapper/<name>`).
    fn luks_of_pv(&self, device: &str) -> Option<&Luks> {
        self.luks.iter().find(|l| l.device == device || (!l.mapper.is_empty() && format!("/dev/mapper/{}", l.mapper) == device))
    }

    /// The PC's disk the bare-metal restore makes again: the first with a partition table
    /// that holds the root filesystem (directly, in LUKS, or in an LVM group: the disk of the
    /// group's first PV), else the first with a table.
    pub fn restore_disk(&self) -> Option<usize> {
        let on_lvm = |vg: &str| {
            let layout = self.lvm_report.as_deref().and_then(|r| LvmLayout::parse(r).ok())?;
            let g = layout.vgs.into_iter().find(|g| g.name == vg)?;
            g.pvs.iter().find_map(|p| self.lvm_pv_disk(&p.device))
        };
        let disk_of = |s: &FsSource| match s {
            FsSource::Partition { disk, .. } => Some(*disk),
            FsSource::Luks { luks } => self.luks.get(*luks).and_then(|l| match &l.source {
                FsSource::Partition { disk, .. } => Some(*disk),
                FsSource::Lvm { vg, .. } => on_lvm(vg),
                _ => None,
            }),
            FsSource::Lvm { vg, .. } => on_lvm(vg),
            _ => None,
        };
        self.root()
            .and_then(|f| disk_of(&f.source))
            .filter(|d| self.disks.get(*d).is_some_and(|d| d.table.is_some()))
            .or_else(|| self.disks.iter().position(|d| d.table.is_some()))
    }

    /// The volume group `vg` holds something the backup restores: a filesystem, swap or a
    /// LUKS container on one of its volumes.
    pub fn lvm_vg_used(&self, vg: &str) -> bool {
        let on = |s: &FsSource| matches!(s, FsSource::Lvm { vg: v, .. } if v == vg);
        self.filesystems.iter().any(|f| on(&f.source)) || self.swap.iter().any(|s| on(&s.source)) || self.luks.iter().any(|l| on(&l.source))
    }

    /// What the bare-metal restore does with `vg` when it makes the disks `restored` again
    /// (one disk in this version): linear volumes on PVs that are partitions (or LUKS
    /// containers on partitions) of restored disks are made again; a group with a PV on a
    /// whole disk, on md, or on a disk that is not restored, or with volumes that are not
    /// linear, is refused when the backup has data on it and left out (its partitions made
    /// again empty) when not.
    pub fn lvm_vg_check(&self, vg: &LvmVg, restored: &dyn Fn(usize) -> bool) -> LvmVgCheck {
        let holds_root = self.root().is_some_and(|f| match &f.source {
            FsSource::Lvm { vg: v, .. } => *v == vg.name,
            FsSource::Luks { luks } => self.luks.get(*luks).is_some_and(|l| matches!(&l.source, FsSource::Lvm { vg: v, .. } if *v == vg.name)),
            _ => false,
        });
        let touches = vg.pvs.iter().any(|p| self.lvm_pv_disk(&p.device).is_some_and(restored));
        let mut why = Vec::new();
        let mut elsewhere = Vec::new();
        for p in &vg.pvs {
            let dev = p.device.as_str();
            if self.disks.iter().any(|d| format!("/dev/{}", d.name) == dev) {
                why.push(format!("{}: {dev} is a physical volume on a whole disk, without a partition table", vg.name));
            } else if dev.starts_with("/dev/md") {
                why.push(format!("{}: {dev} is a physical volume on md", vg.name));
            } else if let Some(l) = self.luks_of_pv(dev).filter(|l| !matches!(l.source, FsSource::Partition { .. })) {
                why.push(format!("{}: {dev} is a physical volume in the LUKS container {}, which is not on a partition", vg.name, l.device));
            } else {
                match self.lvm_pv_disk(dev) {
                    None => why.push(format!("{}: {dev} is not a partition or a LUKS container the manifest knows", vg.name)),
                    Some(d) if !restored(d) => elsewhere.push(dev.to_string()),
                    Some(_) => {}
                }
            }
        }
        if !touches && !holds_root {
            return LvmVgCheck::Elsewhere;
        }
        if !touches {
            why.push(format!("{}: holds the root filesystem, but none of its physical volumes is on the restored disk", vg.name));
        } else if !elsewhere.is_empty() {
            why.push(format!("{}: also has physical volumes on a disk that is not restored ({})", vg.name, elsewhere.join(", ")));
        }
        why.extend(LvmLayout { vgs: vec![vg.clone()] }.unsupported());
        if why.is_empty() {
            LvmVgCheck::Recreate
        } else if holds_root || self.lvm_vg_used(&vg.name) {
            LvmVgCheck::Refuse(why)
        } else {
            LvmVgCheck::SkipUnused(why)
        }
    }
}

/// The tag of every version of `device` (versions and retention are per PC inside a group).
pub fn device_tag(device: &str) -> String {
    format!("pc-{device}")
}

/// The tag every agent version carries.
pub const AGENT_TAG: &str = "rynas-pc";

/// A device name the NAS accepts: 1–32 of `[a-z0-9-]`, starting with a letter or digit.
pub fn valid_device(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 32
        && d.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !d.starts_with('-')
}

/// A device name from a host name: lower case, other characters as `-`, at most 32.
pub fn device_from_hostname(h: &str) -> String {
    let mut s: String = h
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '-' }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s: String = s.trim_matches('-').chars().take(32).collect();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "pc".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_qr_links_go_both_ways() {
        let q = RestoreQr {
            nas: "AbCdEfGhIjKlMnOpQrSt_-".into(),
            req: "0123456789abcdef".into(),
            code: "042913".into(),
        };
        let link = q.link();
        assert_eq!(link, "rynas-restore:v1?nas=AbCdEfGhIjKlMnOpQrSt_-&req=0123456789abcdef&code=042913");
        assert_eq!(RestoreQr::parse(&link), Some(q.clone()));
        // unknown parameters are ignored, the order does not matter
        assert_eq!(
            RestoreQr::parse("rynas-restore:v1?code=042913&x=1&req=0123456789abcdef&nas=AbCdEfGhIjKlMnOpQrSt_-"),
            Some(q)
        );
        for bad in [
            "rynas-restore:v2?nas=a&req=0123456789abcdef&code=042913",
            "rynas://pair?nas=a&req=0123456789abcdef&code=042913",
            "rynas-restore:v1?nas=a&req=0123456789abcdeg&code=042913",
            "rynas-restore:v1?nas=a&req=0123456789abcdef&code=42913",
            "rynas-restore:v1?nas=a%20b&req=0123456789abcdef&code=042913",
            "rynas-restore:v1?req=0123456789abcdef&code=042913",
        ] {
            assert_eq!(RestoreQr::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn served_certificates_cover_their_names() {
        let c = ServedCertificate { fingerprint: "AA".into(), names: vec!["*.nas.example.org".into(), "nas.local".into(), "192.168.1.5".into()] };
        for ok in ["files.nas.example.org", "NAS.local", "nas.local.", "192.168.1.5", "[192.168.1.5]"] {
            assert!(c.covers(ok), "{ok}");
        }
        for no in ["nas.example.org", "a.b.nas.example.org", "other.local", "192.168.1.6"] {
            assert!(!c.covers(no), "{no}");
        }
    }

    #[test]
    fn restore_sources() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        // the defaults: LAN, Tailscale, loopback, unique-local; not the Internet
        for ok in ["192.168.1.5", "10.1.2.3", "172.31.0.1", "100.101.102.103", "127.0.0.1", "fd7a:115c:a1e0::1", "fe80::1", "::ffff:192.168.0.9"] {
            assert!(restore_source_allowed(&[], ip(ok)), "{ok}");
        }
        for no in ["203.0.113.7", "8.8.8.8", "172.32.0.1", "2001:db8::1"] {
            assert!(!restore_source_allowed(&[], ip(no)), "{no}");
        }
        // an own list replaces the defaults
        let own = vec!["203.0.113.0/24".to_string(), "2001:db8::/32".into()];
        assert!(restore_source_allowed(&own, ip("203.0.113.7")));
        assert!(restore_source_allowed(&own, ip("2001:db8:1::5")));
        assert!(!restore_source_allowed(&own, ip("192.168.1.5")));
        assert!(restore_source_allowed(&["0.0.0.0/0".into()], ip("8.8.8.8")));
        for bad in ["10.0.0.0/33", "x", "10.0.0.0/", "::/129", "1.2.3.4/a"] {
            assert_eq!(IpRange::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn restore_compare_codes() {
        // the input names the version, the TUI's secret, the nonce and the certificate as
        // bare upper-case hex
        assert_eq!(restore_compare_input("5e", "ab12", "aa:bb:0c"), "rynas-restore-v2\n5e\nab12\nAABB0C");
        assert_eq!(restore_compare_input("5e", "ab12", ""), "rynas-restore-v2\n5e\nab12\n");
        assert_eq!(restore_commit_input("5e"), "rynas-restore-commit-v1\n5e");
        // first four bytes, big-endian, modulo a million, six digits
        assert_eq!(restore_compare_code(&[0, 0, 0, 7, 9, 9]), "000007");
        assert_eq!(restore_compare_code(&[0xff, 0xff, 0xff, 0xff]), "967295");
        assert_eq!(restore_code_text("042913"), "042 913");
        assert_eq!(restore_code_text("x"), "x");
    }

    #[test]
    fn capture_note_from_a_newer_agent_is_kept_as_other() {
        let n: CaptureNote = serde_json::from_str(r#"{"code":"something_new","detail":"x"}"#).unwrap();
        assert_eq!(n, CaptureNote::Other);
        let n: CaptureNote = serde_json::from_str(r#"{"code":"lvm_no_space","vg":"v","lv":"l"}"#).unwrap();
        assert_eq!(n, CaptureNote::LvmNoSpace { vg: "v".into(), lv: "l".into() });
    }

    #[test]
    fn device_names() {
        assert_eq!(device_from_hostname("Marco's Laptop.lan"), "marco-s-laptop-lan");
        assert_eq!(device_from_hostname("..."), "pc");
        assert!(valid_device("pc-1"));
        assert!(!valid_device("-pc"));
        assert!(!valid_device("PC"));
        assert!(!valid_device(&"a".repeat(33)));
    }

    #[test]
    fn agents_that_follow_the_window() {
        assert!(!agent_follows_window("0.8.19"));
        assert!(agent_follows_window("0.8.20"));
        assert!(agent_follows_window("v0.9.0-rc1"));
        assert!(agent_follows_window(""));
    }

    /// R5 of `docs/research/scheduling.md`: plans and reports between agents and NAS of
    /// different versions. Golden texts of 0.8.19 (before S6) and of S6.
    #[test]
    fn plans_and_reports_across_versions() {
        use crate::scheduler::{RunTrigger, SkipReason, TaskTrigger};
        // a plan of a 0.8.19 NAS (no trigger, no window, no zone): the old fields, no window
        let old_nas = r#"{"rev":7,"plan":{"interval_hours":6,"start_delay_min":15,"excludes":[],"freeze":"hot","freeze_max_secs":60,"paused":false},"method":"files"}"#;
        let p: AgentPlan = serde_json::from_str(old_nas).unwrap();
        assert_eq!(p.schedule(), AgentSchedule { every_h: 6, delay_min: 15, window: None, calendar: None, at_boot: false, shared: false });
        // a plan of an S6 NAS: what an older agent reads (it keeps the two old fields) and
        // what a new one follows (the trigger and the window)
        let window = format!("{}{}", "0".repeat(22), "1".repeat(146));
        let new = AgentPlan {
            rev: 8,
            plan: PcPlan { interval_hours: 6, start_delay_min: 15, window: Some(window.clone()), ..PcPlan::default() },
            command: None,
            method: PcMethod::Files,
            trigger: Some(TaskTrigger::Interval { hours: 6, after_boot_min: 15 }),
            tz: Some("Europe/Rome".into()),
            tz_posix: Some("CET-1CEST,M3.5.0,M10.5.0/3".into()),
            woken_ms: None,
        };
        let text = serde_json::to_string(&new).unwrap();
        let golden = format!(
            r#"{{"rev":8,"plan":{{"interval_hours":6,"start_delay_min":15,"excludes":[],"freeze":"hot","freeze_max_secs":60,"paused":false,"window":"{window}"}},"method":"files","trigger":{{"kind":"interval","hours":6,"after_boot_min":15}},"tz":"Europe/Rome","tz_posix":"CET-1CEST,M3.5.0,M10.5.0/3"}}"#
        );
        assert_eq!(text, golden);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!((v["plan"]["interval_hours"].as_u64(), v["plan"]["start_delay_min"].as_u64()), (Some(6), Some(15)));
        assert_eq!(serde_json::from_str::<AgentPlan>(&text).unwrap().schedule(), AgentSchedule { every_h: 6, delay_min: 15, window: Some(window), calendar: None, at_boot: false, shared: true });
        // a plan without a window serializes as before S6 but for the trigger and the zone
        let plain = AgentPlan { plan: PcPlan::default(), ..new.clone() };
        assert!(!serde_json::to_string(&plain).unwrap().contains("window"));
        // an invalid window is no window
        let bad = AgentPlan { plan: PcPlan { window: Some("1".into()), ..PcPlan::default() }, ..new };
        assert_eq!(bad.schedule().window, None);
        // a report of a 0.8.19 agent: no trigger, never skipped
        let old_agent = r#"{"run_id":"run-1","started_ms":1,"ended_ms":2,"ok":true,"files":3,"bytes_read":4,"bytes_added":5,"changed_during":[],"changed_count":0,"captures":[],"restore":false,"agent_version":"0.8.19"}"#;
        let r: AgentReport = serde_json::from_str(old_agent).unwrap();
        assert_eq!((r.run.trigger, r.run.skipped), (None, None));
        assert_eq!(serde_json::to_string(&r).unwrap(), old_agent);
        // a skipped start of a new agent: what a 0.8.19 NAS ignores (unknown fields)
        let skip = AgentReport {
            run: PcRun { run_id: "skip-9".into(), started_ms: 9, ended_ms: 9, trigger: Some(RunTrigger::Timer), skipped: Some(SkipReason::Window), ..PcRun::default() },
            agent_version: "0.8.20".into(),
            ..AgentReport::default()
        };
        let t = serde_json::to_string(&skip).unwrap();
        assert!(t.contains(r#""trigger":"timer""#) && t.contains(r#""skipped":"window""#), "{t}");
        assert_eq!(serde_json::from_str::<AgentReport>(&t).unwrap(), skip);
    }

    #[test]
    fn unsent_mounts_skip_the_sent_and_the_left_out_on_purpose() {
        let mount = |path: &str, sv: &str| MountEntry { path: path.into(), subvol: Some(sv.into()), options: String::new() };
        let sub = |name: &str, path: &str, mount: &str| BtrbkSubvolume { name: name.into(), path: path.into(), mount: mount.into(), ..Default::default() };
        let mut m = Manifest {
            filesystems: vec![Filesystem {
                fstype: "btrfs".into(),
                mounts: vec![mount("/", "/@"), mount("/home", "/@home"), mount("/var/log", "/@log"), mount("/.snapshots", "/@snapshots"), mount("/data", "/@data"), mount("/mnt/pool", "/")],
                ..Default::default()
            }],
            btrbk: Some(BtrbkLayout { subvolumes: vec![sub("@", "@", "/"), sub("@data", "@data", "/data")], ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(m.unsent_mounts(), vec![("/home".to_string(), "@home".to_string())]);
        m.btrbk = None;
        assert!(m.unsent_mounts().is_empty());
    }

    #[test]
    fn unsent_mounts_skip_what_the_agent_never_sends() {
        let mount = |path: &str, sv: &str| MountEntry { path: path.into(), subvol: Some(sv.into()), options: String::new() };
        let sub = |path: &str, mount: &str| BtrbkSubvolume { name: path.into(), path: path.into(), mount: mount.into(), ..Default::default() };
        let m = Manifest {
            filesystems: vec![
                Filesystem {
                    fstype: "btrfs".into(),
                    uuid: "root".into(),
                    mounts: vec![
                        mount("/", "/@"),
                        // names btrbk.conf cannot hold
                        mount("/srv/my files", "/@files"),
                        mount("/srv/a#b", "/@ab"),
                        mount("/srv/q", "/@with space"),
                        // container storage and the agent's snapshots
                        mount("/home/me/.local/share/containers/storage", "/@home/me/.local/share/containers/storage"),
                        mount("/var/lib/incus", "/@incus/storage-pools/default"),
                        mount("/x", "/@/.rynas-snap"),
                        // shadowed: by a later mount of the same filesystem, by another filesystem
                        mount("/opt", "/@opt-old"),
                        mount("/opt", "/@opt"),
                        mount("/data", "/@data-old"),
                        // really left out
                        mount("/home", "/@home"),
                    ],
                    ..Default::default()
                },
                Filesystem { fstype: "ext4".into(), uuid: "other".into(), mounts: vec![MountEntry { path: "/data".into(), subvol: None, options: String::new() }], ..Default::default() },
            ],
            btrbk: Some(BtrbkLayout { subvolumes: vec![sub("@", "/"), sub("@opt", "/opt")], ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(m.unsent_mounts(), vec![("/home".to_string(), "@home".to_string())]);
    }

    #[test]
    fn not_sent_reasons() {
        assert_eq!(not_sent_reason("/home", "@home"), None);
        assert_eq!(not_sent_reason("/var/log", "@log"), Some("cache, logs or rebuildable data"));
        assert_eq!(not_sent_reason("/var/lib/docker/x", "@x"), Some("cache, logs or rebuildable data"));
        assert_eq!(not_sent_reason("/.snapshots", "@snapshots"), Some("snapshot folder"));
        assert_eq!(not_sent_reason("/srv/d", "/@docker/btrfs"), Some("snapshot folder"));
        assert_eq!(not_sent_reason("/srv/my \"x\"", "@x"), Some("a name btrbk.conf cannot hold"));
        assert_eq!(not_sent_reason("/srv/x", "@my x"), Some("a name btrbk.conf cannot hold"));
    }

    #[test]
    fn report_flattens_the_run() {
        let r = AgentReport {
            run: PcRun {
                run_id: "r1".into(),
                ok: true,
                ..Default::default()
            },
            manifest: None,
            agent_version: "0.1.0".into(),
            root_btrfs: None,
        };
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["run_id"], "r1");
        let back: AgentReport = serde_json::from_value(j).unwrap();
        assert_eq!(back, r);
    }

    /// The fields the btrfs method's NAS side reads (`rynasd::pc_backup`): `format`,
    /// `written_at_ms`, `boot.loader`, `btrbk.subvolumes[].name` and `.mount`.
    #[test]
    fn manifest_shape_shared_by_both_methods() {
        let m = Manifest {
            format: MANIFEST_FORMAT,
            written_at_ms: 5,
            hostname: "pc".into(),
            boot: BootInfo { loader: Loader::SystemdBoot, ..Default::default() },
            btrbk: Some(BtrbkLayout {
                snapshot_dir: "/.snapshots".into(),
                subvolumes: vec![BtrbkSubvolume { name: "@".into(), path: "@".into(), mount: "/".into(), ..Default::default() }],
                boot_copies: Vec::new(),
                agent: false,
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["format"], 1);
        assert_eq!(v["written_at_ms"], 5);
        assert_eq!(v["boot"]["loader"], "systemd_boot");
        assert_eq!(v["btrbk"]["subvolumes"][0]["name"], "@");
        assert_eq!(v["btrbk"]["subvolumes"][0]["mount"], "/");
        let plain = serde_json::to_value(Manifest::default()).unwrap();
        assert!(plain.get("btrbk").is_none());
    }

    #[test]
    fn readiness() {
        let mut m = Manifest {
            disks: vec![Disk {
                name: "vda".into(),
                size_bytes: 1,
                logical_sector: 512,
                table: Some(PartTable::default()),
                ..Default::default()
            }],
            ..Default::default()
        };
        m.boot.loader = Loader::Grub;
        assert!(m.ready_reasons().is_empty());
        m.filesystems.push(Filesystem {
            fstype: "xfs".into(),
            mounts: vec![MountEntry { path: "/srv".into(), ..Default::default() }],
            capture: Some(CaptureMethod::Skipped),
            ..Default::default()
        });
        assert_eq!(m.ready_reasons(), vec![ReadyReason::VolumeSkipped]);
    }

    #[test]
    fn place_of_nested_subvolumes() {
        let sv = |name: &str, mount: &str, nested_in: &str, inside: &str| BtrbkSubvolume {
            name: name.into(),
            mount: mount.into(),
            nested_in: nested_in.into(),
            inside: inside.into(),
            ..Default::default()
        };
        let b = BtrbkLayout {
            subvolumes: vec![
                sv("@", "/", "", ""),
                sv("@home", "/home", "", ""),
                sv("games", "", "@home", "games"),
                sv("deep", "", "games", "x/deep"),
                sv("machines", "", "@", "var/lib/machines"),
                sv("orphan", "", "gone", "o"),
            ],
            ..Default::default()
        };
        let place = |n: &str| b.place_of(b.subvolumes.iter().find(|s| s.name == n).unwrap());
        assert_eq!(place("@").as_deref(), Some("/"));
        assert_eq!(place("games").as_deref(), Some("/home/games"));
        assert_eq!(place("deep").as_deref(), Some("/home/games/x/deep"));
        assert_eq!(place("machines").as_deref(), Some("/var/lib/machines"));
        assert_eq!(place("orphan"), None);
        // the manifest field names
        let v = serde_json::to_value(&b.subvolumes[2]).unwrap();
        assert_eq!(v["nested_in"], "@home");
        assert_eq!(v["inside"], "games");
        assert!(serde_json::to_value(&b.subvolumes[0]).unwrap().get("nested_in").is_none());
    }

    /// `lvs --reportformat json` as agents up to 0.8.11 recorded it.
    pub const OLD_LVS: &str = r#"{
      "report": [
          {
              "lv": [
                  {"vg_name":"ArchinstallVg", "lv_name":"home", "lv_size":"449360953344B", "lv_attr":"-wi-ao----", "pool_lv":"", "devices":"/dev/nvme0n1p2(11776)"},
                  {"vg_name":"ArchinstallVg", "lv_name":"root", "lv_size":"49392123904B", "lv_attr":"-wi-ao----", "pool_lv":"", "devices":"/dev/nvme0n1p2(0)"}
              ]
          }
      ]
      ,
      "log": [
      ]
  }"#;

    #[test]
    fn lvm_reports_old_and_new() {
        let l = LvmLayout::parse(OLD_LVS).unwrap();
        assert_eq!(l.vgs.len(), 1);
        let vg = &l.vgs[0];
        assert_eq!((vg.name.as_str(), vg.extent_bytes, vg.uuid.as_str()), ("ArchinstallVg", 0, ""));
        assert_eq!(vg.lvs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["home", "root"]);
        assert_eq!(vg.lvs[1].size_bytes, 46 << 30);
        assert_eq!(vg.pvs.len(), 1);
        assert_eq!(vg.pvs[0].device, "/dev/nvme0n1p2");
        assert!(l.unsupported().is_empty());
        // the newer report: vgs, pvs, and segment rows
        let new = r#"{"report":[{"lv":[
            {"vg_name":"vg0","lv_name":"root","lv_uuid":"aaaaaa-1111","lv_size":"1073741824B","lv_attr":"-wi-ao----","pool_lv":"","origin":"","segtype":"linear","devices":"/dev/sda2(0)"},
            {"vg_name":"vg0","lv_name":"root","lv_uuid":"aaaaaa-1111","lv_size":"1073741824B","lv_attr":"-wi-ao----","pool_lv":"","origin":"","segtype":"linear","devices":"/dev/sdb1(0)"},
            {"vg_name":"vg0","lv_name":"data","lv_uuid":"bbbbbb-2222","lv_size":"4194304B","lv_attr":"-wi-a-----","pool_lv":"","origin":"","segtype":"striped","devices":"/dev/sda2(256),/dev/sdb1(256)"}],
          "vg":[{"vg_name":"vg0","vg_uuid":"vvvvvv-0000","vg_extent_size":"4194304B","vg_size":"10737418240B","vg_free":"268435456B"}],
          "pv":[{"pv_name":"/dev/sda2","pv_uuid":"pppppp-1","pv_size":"5368709120B","pe_start":"1048576B","vg_name":"vg0"},{"pv_name":"/dev/sdb1","pv_uuid":"pppppp-2","pv_size":"5368709120B","pe_start":"1048576B","vg_name":"vg0"},{"pv_name":"/dev/sdc","pv_uuid":"x","pv_size":"1B","pe_start":"0B","vg_name":""}]}]}"#;
        let l = LvmLayout::parse(new).unwrap();
        let vg = &l.vgs[0];
        assert_eq!((vg.uuid.as_str(), vg.extent_bytes, vg.free_bytes), ("vvvvvv-0000", 4 << 20, 256 << 20));
        assert_eq!(vg.pvs.len(), 2, "a PV outside any VG is not part of the layout");
        assert_eq!(vg.lvs[0].devices, ["/dev/sda2", "/dev/sdb1"]);
        assert_eq!(vg.lvs[0].segtypes, ["linear", "linear"]);
        assert_eq!(l.unsupported(), ["vg0/data: striped segments"]);
        assert!(LvmLayout::parse("not json").is_err());
    }

    #[test]
    fn lvm_kinds_the_restore_refuses() {
        let row = |name: &str, attr: &str, extra: &str| format!(r#"{{"vg_name":"vg","lv_name":"{name}","lv_size":"1B","lv_attr":"{attr}","devices":"/dev/sda2(0)"{extra}}}"#);
        let rows = [row("pool", "twi-aotz--", ""), row("thin", "Vwi-aotz--", r#","pool_lv":"pool""#), row("snap", "swi-a-s---", r#","origin":"root""#), row("root", "owi-aos---", ""), row("r1", "rwi-a-r---", r#","segtype":"raid1""#), row("c", "Cwi-a-C---", "")];
        let l = LvmLayout::parse(&format!(r#"{{"report":[{{"lv":[{}]}}]}}"#, rows.join(","))).unwrap();
        let u = l.unsupported();
        assert_eq!(u, ["vg/pool: a thin pool", "vg/thin: a thin volume", "vg/snap: a snapshot", "vg/r1: a raid volume", "vg/c: a cache volume"]);
        // an old report cannot tell striped from linear on several devices
        let l = LvmLayout::parse(r#"{"report":[{"lv":[{"vg_name":"vg","lv_name":"x","lv_size":"1B","lv_attr":"-wi-a-----","devices":"/dev/sda2(0),/dev/sdb2(0)"}]}]}"#).unwrap();
        assert!(l.unsupported()[0].contains("striped"), "{:?}", l.unsupported());
    }

    #[test]
    fn linear_lvm_is_ready_md_and_thin_are_not() {
        let table = PartTable { partitions: vec![Partition { number: 1, ..Default::default() }, Partition { number: 2, ..Default::default() }], ..Default::default() };
        let mut m = Manifest { disks: vec![Disk { name: "nvme0n1".into(), table: Some(table), ..Default::default() }], boot: BootInfo { loader: Loader::SystemdBoot, ..Default::default() }, ..Default::default() };
        m.filesystems.push(Filesystem { source: FsSource::Lvm { vg: "ArchinstallVg".into(), lv: "root".into() }, mounts: vec![MountEntry { path: "/".into(), ..Default::default() }], ..Default::default() });
        assert!(m.ready_reasons().contains(&ReadyReason::ManualLayout), "LVM without a report");
        m.lvm_report = Some(OLD_LVS.into());
        assert!(!m.ready_reasons().contains(&ReadyReason::ManualLayout), "{:?}", m.ready_reasons());
        m.lvm_report = Some(OLD_LVS.replace("-wi-ao----", "Vwi-aotz--"));
        assert!(m.ready_reasons().contains(&ReadyReason::ManualLayout));
        m.lvm_report = Some(OLD_LVS.into());
        m.md.push("MD_LEVEL=raid1".into());
        assert!(m.ready_reasons().contains(&ReadyReason::ManualLayout));
    }

    #[test]
    fn lvm_report_without_interrupted_snapshots_with_system_id() {
        let r = r#"{"report":[{"lv":[
            {"vg_name":"vg","lv_name":"root","lv_size":"1073741824B","lv_attr":"owi-aos---","segtype":"linear","seg_size":"1073741824B","devices":"/dev/sda2(0)"},
            {"vg_name":"vg","lv_name":"root-rynas-snap","lv_size":"268435456B","lv_attr":"swi-a-s---","origin":"root","segtype":"linear","seg_size":"134217728B","devices":"/dev/sda2(256)"},
            {"vg_name":"vg","lv_name":"root-rynas-snap","lv_size":"268435456B","lv_attr":"swi-a-s---","origin":"root","segtype":"linear","seg_size":"134217728B","devices":"/dev/sda2(300)"}],
          "vg":[{"vg_name":"vg","vg_uuid":"u","vg_systemid":"pc1","vg_extent_size":"4194304B","vg_size":"2147483648B","vg_free":"0B"}]}]}"#;
        let l = LvmLayout::parse(r).unwrap();
        assert_eq!(l.vgs[0].lvs.len(), 1, "the interrupted snapshot is not a volume of the PC");
        assert_eq!(l.vgs[0].free_bytes, 256 << 20, "its two segments are free space again");
        assert_eq!(l.vgs[0].system_id, "pc1");
        assert!(l.unsupported().is_empty());
        // an old report (lv rows only, no segment sizes): left out, nothing to add to
        let l = LvmLayout::parse(&OLD_LVS.replace(r#""lv_name":"home""#, r#""lv_name":"root-rynas-snap", "lv_attr":"swi-a-s---""#)).unwrap();
        assert_eq!(l.vgs[0].lvs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["root"]);
        assert!(l.unsupported().is_empty());
    }

    /// Disk 0 (`sda`, the PC's system disk: ESP, PV of `sys`) and disk 1 (`sdb`, PV of
    /// `data`); root on `sys/root`.
    fn two_disk_lvm(report: &str) -> Manifest {
        let disk = |name: &str, n: u32| Disk {
            name: name.into(),
            table: Some(PartTable { kind: "gpt".into(), partitions: (1..=n).map(|number| Partition { number, ..Default::default() }).collect(), ..Default::default() }),
            ..Default::default()
        };
        let fs = |vg: &str, lv: &str, path: &str| Filesystem {
            source: FsSource::Lvm { vg: vg.into(), lv: lv.into() },
            fstype: "ext4".into(),
            mounts: vec![MountEntry { path: path.into(), ..Default::default() }],
            ..Default::default()
        };
        Manifest {
            disks: vec![disk("sda", 3), disk("sdb", 1)],
            boot: BootInfo { loader: Loader::SystemdBoot, ..Default::default() },
            filesystems: vec![fs("sys", "root", "/"), fs("data", "srv", "/srv")],
            lvm_report: Some(report.into()),
            ..Default::default()
        }
    }

    fn lv_row(vg: &str, lv: &str, attr: &str, dev: &str) -> String {
        format!(r#"{{"vg_name":"{vg}","lv_name":"{lv}","lv_size":"1073741824B","lv_attr":"{attr}","segtype":"linear","devices":"{dev}(0)"}}"#)
    }

    fn pv_row(vg: &str, dev: &str) -> String {
        format!(r#"{{"pv_name":"{dev}","pv_uuid":"","pv_size":"2147483648B","pe_start":"1048576B","vg_name":"{vg}"}}"#)
    }

    fn report(lvs: &[String], pvs: &[String]) -> String {
        format!(r#"{{"report":[{{"lv":[{}],"pv":[{}]}}]}}"#, lvs.join(","), pvs.join(","))
    }

    #[test]
    fn the_nas_and_the_agent_judge_lvm_groups_alike() {
        // the data group on the second disk: left as it is, ready
        let base = [lv_row("sys", "root", "-wi-ao----", "/dev/sda2"), lv_row("data", "srv", "-wi-ao----", "/dev/sdb1")];
        let m = two_disk_lvm(&report(&base, &[pv_row("sys", "/dev/sda2"), pv_row("data", "/dev/sdb1")]));
        assert_eq!(m.restore_disk(), Some(0));
        assert_eq!(m.lvm_pv_disk("/dev/sdb1"), Some(1));
        let l = LvmLayout::parse(m.lvm_report.as_deref().unwrap()).unwrap();
        let on0 = |d: usize| d == 0;
        assert_eq!(m.lvm_vg_check(&l.vgs[0], &on0), LvmVgCheck::Recreate);
        assert_eq!(m.lvm_vg_check(&l.vgs[1], &on0), LvmVgCheck::Elsewhere);
        assert!(m.lvm_unsupported().is_empty());
        assert!(!m.ready_reasons().contains(&ReadyReason::ManualLayout));
        // the system group also on the second disk: the NAS is not ready, the agent refuses
        let m = two_disk_lvm(&report(&base, &[pv_row("sys", "/dev/sda2"), pv_row("sys", "/dev/sdb1")]));
        let u = m.lvm_unsupported();
        assert!(u.len() == 1 && u[0].contains("sys: also has physical volumes on a disk that is not restored (/dev/sdb1)"), "{u:?}");
        assert!(m.ready_reasons().contains(&ReadyReason::ManualLayout));
        // a PV on a whole disk, on md
        let m = two_disk_lvm(&report(&base, &[pv_row("sys", "/dev/sda2"), pv_row("sys", "/dev/sdb"), pv_row("sys", "/dev/md0"), pv_row("data", "/dev/sdb1")]));
        let u = m.lvm_unsupported();
        assert!(u.iter().any(|x| x.contains("/dev/sdb is a physical volume on a whole disk")) && u.iter().any(|x| x.contains("/dev/md0 is a physical volume on md")), "{u:?}");
        // a thin pool of virtual machines on the system disk, nothing of the backup on it:
        // left out, ready
        let mut lvs = base.to_vec();
        lvs.push(lv_row("vms", "pool", "twi-aotz--", "/dev/sda3"));
        let m = two_disk_lvm(&report(&lvs, &[pv_row("sys", "/dev/sda2"), pv_row("vms", "/dev/sda3"), pv_row("data", "/dev/sdb1")]));
        let l = LvmLayout::parse(m.lvm_report.as_deref().unwrap()).unwrap();
        let vms = l.vgs.iter().find(|v| v.name == "vms").unwrap();
        assert!(matches!(m.lvm_vg_check(vms, &on0), LvmVgCheck::SkipUnused(w) if w[0].contains("vms/pool: a thin pool")));
        assert!(m.lvm_unsupported().is_empty(), "{:?}", m.lvm_unsupported());
        // the same pool with a filesystem of the backup on a thin volume: refused
        let mut lvs2 = lvs.clone();
        lvs2.push(lv_row("vms", "disk1", "Vwi-aotz--", "/dev/sda3"));
        let mut m = two_disk_lvm(&report(&lvs2, &[pv_row("sys", "/dev/sda2"), pv_row("vms", "/dev/sda3"), pv_row("data", "/dev/sdb1")]));
        m.filesystems.push(Filesystem { source: FsSource::Lvm { vg: "vms".into(), lv: "disk1".into() }, ..Default::default() });
        assert!(m.lvm_unsupported().iter().any(|x| x.contains("vms/disk1: a thin volume")), "{:?}", m.lvm_unsupported());
        // the root's group on a whole disk: the restore cannot make it
        let m = two_disk_lvm(&report(&base, &[pv_row("sys", "/dev/sdb"), pv_row("data", "/dev/sdb1")]));
        assert!(m.lvm_unsupported().iter().any(|x| x.contains("whole disk")), "{:?}", m.lvm_unsupported());
        assert_eq!(partition_name("nvme0n1", 2), "nvme0n1p2");
        assert_eq!(partition_name("sda", 2), "sda2");
    }

    /// A real report (Arch with systemd-gpt-auto: LVM names its PV by the udev symlink
    /// `/dev/gpt-auto-root`): renamed to the one partition of the PV's size, so the group is
    /// made again and the PC is ready; an alias that fits no partition, or two, stays (refused).
    #[test]
    fn a_pv_named_by_a_udev_alias() {
        const REAL: &str = r#"{"report":[{"lv":[{"devices":"/dev/gpt-auto-root(11776)","lv_attr":"-wi-ao----","lv_name":"home","lv_size":"449369341952B","segtype":"linear","stripes":"1","vg_name":"ArchinstallVg"},{"devices":"/dev/gpt-auto-root(0)","lv_attr":"-wi-ao----","lv_name":"root","lv_size":"49392123904B","segtype":"linear","stripes":"1","vg_name":"ArchinstallVg"}],"pv":[{"pe_start":"1048576B","pv_name":"/dev/gpt-auto-root","pv_size":"499029901312B","vg_name":"ArchinstallVg"}],"vg":[{"vg_extent_size":"4194304B","vg_free":"268435456B","vg_name":"ArchinstallVg","vg_size":"499029901312B"}]}]}"#;
        // nvme0n1: p1 the ESP (1 GiB, vfat), p2 the PV (the rest of a 500 GB disk)
        let pv_part = (499029901312u64 + 1048576 + 2 * 1048576) / 512;
        let manifest = |pv_lba: u64, extra: Option<u64>| {
            let mut parts = vec![
                Partition { number: 1, size_lba: (1 << 30) / 512, ..Default::default() },
                Partition { number: 2, size_lba: pv_lba, ..Default::default() },
            ];
            if let Some(lba) = extra {
                parts.push(Partition { number: 3, size_lba: lba, ..Default::default() });
            }
            Manifest {
                disks: vec![Disk { name: "nvme0n1".into(), logical_sector: 512, table: Some(PartTable { kind: "gpt".into(), partitions: parts, ..Default::default() }), ..Default::default() }],
                boot: BootInfo { loader: Loader::SystemdBoot, ..Default::default() },
                filesystems: vec![
                    Filesystem { source: FsSource::Partition { disk: 0, number: 1 }, fstype: "vfat".into(), mounts: vec![MountEntry { path: "/boot".into(), ..Default::default() }], ..Default::default() },
                    Filesystem { source: FsSource::Lvm { vg: "ArchinstallVg".into(), lv: "root".into() }, fstype: "ext4".into(), mounts: vec![MountEntry { path: "/".into(), ..Default::default() }], ..Default::default() },
                    Filesystem { source: FsSource::Lvm { vg: "ArchinstallVg".into(), lv: "home".into() }, fstype: "ext4".into(), mounts: vec![MountEntry { path: "/home".into(), ..Default::default() }], ..Default::default() },
                ],
                lvm_report: Some(REAL.into()),
                ..Default::default()
            }
        };
        // as the agent sent it: the alias is no partition, the group is refused
        let m = manifest(pv_part, None);
        assert!(m.ready_reasons().contains(&ReadyReason::ManualLayout));
        let mut m = m;
        assert!(m.normalize_lvm_aliases());
        let r = m.lvm_report.clone().unwrap();
        assert!(r.contains(r#""pv_name":"/dev/nvme0n1p2""#) && r.contains(r#""devices":"/dev/nvme0n1p2(11776)""#) && r.contains(r#""devices":"/dev/nvme0n1p2(0)""#), "{r}");
        assert!(!r.contains("gpt-auto"), "{r}");
        assert_eq!(m.restore_disk(), Some(0));
        assert!(m.lvm_unsupported().is_empty(), "{:?}", m.lvm_unsupported());
        assert!(!m.ready_reasons().contains(&ReadyReason::ManualLayout));
        // again: nothing more to rename
        assert!(!m.normalize_lvm_aliases());
        // two partitions of that size: not certain, left as it was
        let mut two = manifest(pv_part, Some(pv_part));
        assert!(!two.normalize_lvm_aliases());
        assert!(two.ready_reasons().contains(&ReadyReason::ManualLayout));
        // no partition of that size
        let mut none = manifest(pv_part / 2, None);
        assert!(!none.normalize_lvm_aliases());
    }
}
