//! Tailscale (`/api/v1/tailscale...`), remote access of phase 2 (`docs/phase2/remote-access.md`).
//!
//! - [`TailscaleStatus`]: the package, the daemon and the node as rynasd reads them from
//!   tailscaled's LocalAPI (status and preferences); the only route that answers without
//!   Tailscale.
//! - [`TailscaleOptions`]: the few preferences rynas edits (`PUT /tailscale/options`, a task
//!   with a timed confirmation: [`TailscaleStatus::confirm`]).
//! - [`TailscaleLogin`]: the body of `POST /tailscale/login` (an optional auth key, never
//!   echoed back, never logged).
//!
//! Tailscale keeps its own state (`/var/lib/tailscale`): rynas stores no copy of it, so
//! these are not configuration keys of the store.

use serde::{Deserialize, Serialize};

/// Why rynas cannot install Tailscale here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TailscaleBlock {
    /// Not a Debian or Ubuntu system (no apt repository for it).
    UnsupportedOs,
    /// Debian/Ubuntu without a release codename in `/etc/os-release` (testing, sid).
    UnknownRelease,
}

/// tailscaled's backend state (`BackendState` of the LocalAPI status).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TailscaleState {
    /// The daemon does not answer on its socket (stopped, or not installed).
    #[default]
    Unavailable,
    NoState,
    /// Logged out, or never logged in: `auth_url` once a login was started.
    NeedsLogin,
    /// Logged in, waiting for an administrator to approve the device in the admin console.
    NeedsMachineAuth,
    /// Logged in, but switched off (`tailscale down`).
    Stopped,
    Starting,
    Running,
}

/// The preferences rynas shows and edits. Defaults are those of a server: nothing accepted
/// from the tailnet that could change the NAS's own routing or DNS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TailscaleOptions {
    /// Use subnet routes other nodes advertise (`--accept-routes`). Off: a route to the
    /// NAS's own LAN through the tailnet can make the LAN unreachable.
    #[serde(default)]
    pub accept_routes: bool,
    /// Use the tailnet's DNS settings (`--accept-dns`, MagicDNS names). Off on a server.
    #[serde(default)]
    pub accept_dns: bool,
    /// Offer this NAS as an exit node (`--advertise-exit-node`).
    #[serde(default)]
    pub exit_node: bool,
    /// Subnets of the NAS's LAN offered to the tailnet (`--advertise-routes`), CIDR.
    #[serde(default)]
    pub advertise_routes: Vec<String>,
    /// Tailscale SSH (`--ssh`): the tailnet policy decides who logs in, not the NAS.
    #[serde(default)]
    pub ssh: bool,
}

/// A change of options waiting for confirmation: without `POST /tailscale/options/confirm`
/// before `deadline_ms`, rynasd puts the previous options back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailscaleConfirm {
    pub deadline_ms: u64,
}

/// `GET /api/v1/tailscale`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TailscaleStatus {
    /// The package (or the binaries) is there.
    pub installed: bool,
    /// Why Install is not offered (`None` when it is, or when installed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_block: Option<TailscaleBlock>,
    /// tailscaled answers on its socket.
    pub running: bool,
    pub state: TailscaleState,
    /// Version of the daemon (`1.90.6`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// A newer release exists (from the daemon's own check).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_available: Option<String>,
    /// The URL to open to log this NAS in (only while a login waits for it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
    /// Name of the tailnet (`example.com`, `user@example.com`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<String>,
    /// MagicDNS name of the NAS, without the final dot (`nas.tail1234.ts.net`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_name: Option<String>,
    pub magic_dns: bool,
    /// The device name in the tailnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Tailscale addresses of the NAS (100.x.y.z, fd7a:115c:a1e0::…).
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Connected to the coordination server.
    pub online: bool,
    pub peers: u32,
    pub peers_online: u32,
    /// When the node key expires (`None`: expiry disabled for this device, or not logged in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_expiry_ms: Option<u64>,
    #[serde(default)]
    pub options: TailscaleOptions,
    /// Advertised routes the admin console has not approved yet (exit node: `0.0.0.0/0`).
    #[serde(default)]
    pub routes_pending_approval: Vec<String>,
    /// Subnets of the NAS's LAN interfaces (what "advertise this LAN" offers).
    #[serde(default)]
    pub lan_subnets: Vec<String>,
    /// tailscaled's own health warnings (English).
    #[serde(default)]
    pub health: Vec<String>,
    /// The request came over the tailnet: logout, uninstall and option changes may end
    /// this session.
    pub via_tailscale: bool,
    /// A running install, uninstall, login or option change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<TailscaleConfirm>,
}

/// `POST /api/v1/tailscale/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TailscaleLogin {
    /// A pre-generated auth key (`tskey-auth-…`): no browser needed. Written to a root-only
    /// file for the CLI (`--auth-key=file:`) and removed after; never logged or returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_key: Option<String>,
    /// Device name in the tailnet (default: the host name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Another coordination server (Headscale): `https://…`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_server: Option<String>,
}

/// `POST /api/v1/tailscale/uninstall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TailscaleUninstall {
    /// Also delete `/var/lib/tailscale` (the node identity): a new install is a new device.
    #[serde(default)]
    pub purge_state: bool,
}
