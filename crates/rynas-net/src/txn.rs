//! The transaction engine: staged settings → validated plan → trial with timed confirmation
//! → confirmed (new last known good) or rolled back (verified). Also the boot guard, the
//! post-boot watchdog, the reset-button gesture and the ifupdown migration, which all reuse
//! the same two primitives: [`Engine::activate`] (make the system run a revision) and
//! [`Engine::settle`] (verify and repair the actual state).
//!
//! Every writer takes the store lock, so rynasd, the rollback timer (`rynasctl net rollback`),
//! the boot guard and the watchdog never interleave.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rynas_api_types::network::{
    InterfaceConfig, Ipv4Method, LinkKind, NetBackend, NetInterface, NetRevision,
    NetRollbackReport, NetTrial, NetworkState, ReadOnlyCode, ReadOnlyReason, RescueLinkLocal,
    TrialKind,
};
use rynas_api_types::notify::{EventState, NewEvent, NotifyCategory, NotifySeverity};
use rynas_config::FileKind;
use rynas_config::files::{rename_synced, render_final, sha256_hex, write_synced};

use crate::bond;
use crate::ifupdown::{self, Interfaces};
use crate::inventory::{self, Inventory, Link};
use crate::networkd::{self, NetworkFile};
use crate::os::{NetOs, QUICK, SLOW};
use crate::rescue::{self, Press};
use crate::resolv;
use crate::store::{BootAdopt, FileState, Marker, Observed, RevKind, Revision, Services, Store};
use crate::validate::{self, OtherPort};
use crate::{
    BOOT_CHECK_S, INTERFACES, INTERFACES_BACKUP, NETWORKD_DIR, NetPaths, ROLLBACK_UNIT, RYNASCTL,
    TIMEOUT_DEFAULT_S, TIMEOUT_MAX_S, TIMEOUT_MIN_S, reason,
};

#[derive(Debug)]
pub enum NetError {
    /// 400 `invalid_value`.
    Invalid(String),
    /// 400 `invalid_value` about one field of the request (`ApiError.field`).
    InvalidField(&'static str, String),
    /// 404.
    NotFound(String),
    /// 409 `conflict`.
    Conflict(String),
    /// 409 `nothing_pending`.
    NothingPending(String),
    /// The change was applied and rolled back (or could not be applied).
    Failed(String),
    Io(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Invalid(m)
            | NetError::InvalidField(_, m)
            | NetError::NotFound(m)
            | NetError::Conflict(m)
            | NetError::NothingPending(m)
            | NetError::Failed(m)
            | NetError::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for NetError {}

impl From<std::io::Error> for NetError {
    fn from(e: std::io::Error) -> Self {
        NetError::Io(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, NetError>;

/// How [`Engine::activate`] brings a revision into effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Reload and reconfigure now.
    Live,
    /// Early boot, before networkd and ifupdown start: files and unit enablement only.
    Boot,
}

/// Where a confirmation comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `rynasctl net confirm` on the console or over SSH.
    Cli,
    /// The web API, with the local address the session's connection arrived on.
    Web(Option<IpAddr>),
}

/// How the web interface is reached (candidate URLs, Avahi).
#[derive(Debug, Clone, Copy)]
pub struct WebInfo {
    pub port: u16,
    pub https: bool,
}

impl Default for WebInfo {
    fn default() -> Self {
        Self {
            port: 8080,
            https: false,
        }
    }
}

/// A validated change, ready for [`Engine::begin`].
#[derive(Debug, Clone)]
pub struct Plan {
    pub kind: TrialKind,
    /// The revision to run (not stored yet).
    pub target: Revision,
    pub interfaces: Vec<String>,
    pub management_touched: bool,
    pub timeout_s: u32,
    pub skip_test: bool,
    pub candidate_urls: Vec<String>,
    pub removed_addresses: Vec<String>,
    /// New static addresses to check with `arping -D`: (interface, address).
    pub new_addresses: Vec<(String, String)>,
}

/// What [`Engine::begin`] did.
#[derive(Debug, Clone)]
pub struct Begun {
    pub revision: u64,
    pub lkg: u64,
    pub deadline_ms: u64,
}

/// Health of one port, for the watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortHealth {
    pub name: String,
    pub carrier: bool,
    /// Has a global IPv4 address.
    pub routable: bool,
    pub gateway: Option<String>,
    /// `None`: no gateway to test.
    pub gateway_ok: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Healthy,
    /// No port has a cable: nothing to judge, nothing to do.
    NoCarrier,
    Unhealthy,
}

/// The watchdog's decision: healthy when a connected port has a routable address and either
/// no default gateway or a gateway that answers.
pub fn verdict(ports: &[PortHealth]) -> Verdict {
    if !ports.iter().any(|p| p.carrier) {
        return Verdict::NoCarrier;
    }
    if ports
        .iter()
        .any(|p| p.carrier && p.routable && p.gateway_ok != Some(false))
    {
        Verdict::Healthy
    } else {
        Verdict::Unhealthy
    }
}

/// What a revision expects of one interface.
#[derive(Debug, Clone, Default, PartialEq)]
struct Expect {
    addrs: Vec<String>,
    mtu: Option<u32>,
    gateway: Option<String>,
}

/// `TrialKind` as stored revision kinds and event keys.
fn rev_kind_name(k: RevKind) -> &'static str {
    match k {
        RevKind::Snapshot => "snapshot",
        RevKind::Apply => "apply",
        RevKind::Adopt => "adopt",
        RevKind::Rescue => "rescue",
        RevKind::Restore => "restore",
    }
}

/// A random confirmation token (128 bits, hex).
pub fn new_token() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the operating system random source failed");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A network notification: `code` and `fields` let clients translate the English title and
/// body (event codes `net.*` in `docs/phase1/plan.md`).
fn event(
    severity: NotifySeverity,
    key: &str,
    (code, fields): (&str, &[(&str, String)]),
    title: String,
    body: String,
) -> NewEvent {
    NewEvent {
        source: "network".into(),
        category: NotifyCategory::System,
        severity,
        key: Some(format!("network:{key}")),
        state: EventState::Oneshot,
        title,
        body,
        code: code.into(),
        fields: fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        ..Default::default()
    }
}

/// The settings rendered as a rynas file (with the managed header).
fn render_file(iface: &str, c: &InterfaceConfig, mac: Option<&str>) -> String {
    managed(&networkd::render_network(iface, c, mac))
}

/// A rynas file's text with the managed header.
fn managed(text: &str) -> String {
    render_final(&FileKind::Whole { header: true }, text, None).0
}

pub struct Engine {
    pub os: Arc<dyn NetOs>,
    pub store: Store,
    pub paths: NetPaths,
    pub web: WebInfo,
    /// The helper the rollback timer runs.
    pub rynasctl: String,
    /// How the machine signals the rescue paths (a hardware module's beeps and LEDs).
    pub feedback: Arc<dyn rescue::Feedback>,
    cache: Mutex<Option<(Instant, Arc<Inventory>)>>,
    /// The bonds as [`Engine::watch_bonds`] saw them last.
    bonds_seen: Mutex<BTreeMap<String, bond::Seen>>,
}

/// Inventory reads served from memory for this long (guidelines S8).
pub const INVENTORY_TTL: Duration = Duration::from_secs(2);

impl Engine {
    pub fn new(os: Arc<dyn NetOs>, paths: NetPaths) -> Self {
        Self {
            store: Store::new(paths.state.clone()),
            os,
            paths,
            web: WebInfo::default(),
            rynasctl: RYNASCTL.into(),
            feedback: Arc::new(rescue::NoFeedback),
            cache: Mutex::new(None),
            bonds_seen: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_web(mut self, web: WebInfo) -> Self {
        self.web = web;
        self
    }

    /// The machine's way to signal the rescue paths (none by default).
    pub fn with_feedback(mut self, f: Option<Arc<dyn rescue::Feedback>>) -> Self {
        if let Some(f) = f {
            self.feedback = f;
        }
        self
    }

    // -----------------------------------------------------------------------------------------
    // reading
    // -----------------------------------------------------------------------------------------

    /// A fresh inventory (bypasses the cache).
    pub fn inventory_fresh(&self) -> Result<Arc<Inventory>> {
        let inv = Arc::new(inventory::read(self.os.as_ref()).map_err(NetError::Io)?);
        *self.cache.lock().unwrap() = Some((Instant::now(), inv.clone()));
        Ok(inv)
    }

    /// The inventory, at most [`INVENTORY_TTL`] old.
    pub fn inventory(&self) -> Result<Arc<Inventory>> {
        if let Some((at, inv)) = self.cache.lock().unwrap().as_ref()
            && at.elapsed() < INVENTORY_TTL
        {
            return Ok(inv.clone());
        }
        self.inventory_fresh()
    }

    pub fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }

    /// Absolute paths of the rynas `.network` files on disk.
    fn rynas_files(&self) -> Vec<PathBuf> {
        let dir = self.os.path(Path::new(NETWORKD_DIR));
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| networkd::is_rynas_file(n))
                    .map(|n| Path::new(NETWORKD_DIR).join(n))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// What rynas configures now: its files on disk read back (a bond from its `.netdev` and
    /// its members' files).
    pub fn current_config(&self) -> BTreeMap<String, InterfaceConfig> {
        let mut out = BTreeMap::new();
        let mut netdevs = Vec::new();
        let mut members: Vec<bond::MemberFile> = Vec::new();
        for p in self.rynas_files() {
            let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let Some(text) = self.os.read_config(&p) else {
                continue;
            };
            if name.ends_with(".netdev") {
                if let Some(nd) = bond::parse_netdev(&text).filter(|n| n.kind == "bond") {
                    netdevs.push(nd);
                }
                continue;
            }
            let Some(iface) = networkd::iface_of_file(&name) else {
                continue;
            };
            let nf = NetworkFile::parse(&text);
            match &nf.bond {
                Some(b) => members.push((iface.to_string(), b.clone(), nf.primary_slave)),
                None => {
                    out.insert(iface.to_string(), nf.to_config());
                }
            }
        }
        bond::attach(&mut out, netdevs, &members);
        out
    }

    pub fn trial(&self) -> Result<Option<Marker>> {
        Ok(self.store.marker()?)
    }

    fn staged_for(&self, user: &str) -> Result<BTreeMap<String, InterfaceConfig>> {
        Ok(self.store.staged()?.remove(user).unwrap_or_default())
    }

    fn management_iface(inv: &Inventory, local: Option<IpAddr>) -> Option<String> {
        local
            .and_then(|ip| inv.iface_of_ip(&ip))
            .map(|l| l.name.clone())
    }

    /// Why an interface cannot be edited, `None` when it can.
    fn read_only(inv: &Inventory, l: &Link) -> Option<ReadOnlyReason> {
        let code = |code| Some(ReadOnlyReason { code, master: None });
        if l.kind == LinkKind::Loopback {
            return code(ReadOnlyCode::Loopback);
        }
        if let Some(m) = &l.master {
            return Some(ReadOnlyReason {
                code: ReadOnlyCode::Enslaved,
                master: Some(m.clone()),
            });
        }
        // a bond rynas made is edited like a port; other bonds stay read-only
        let rynas_bond = l.kind == LinkKind::Bond && l.rynas_file();
        if !(l.kind == LinkKind::Ethernet || inventory::is_port(l) || rynas_bond) {
            return code(ReadOnlyCode::NotEthernet);
        }
        match inv.backend_of(l) {
            NetBackend::Networkd => None,
            NetBackend::Ifupdown => code(ReadOnlyCode::Ifupdown),
            NetBackend::NetworkManager => code(ReadOnlyCode::NetworkManager),
            NetBackend::None if inv.services.networkd => None,
            NetBackend::None => code(ReadOnlyCode::NoNetworkd),
        }
    }

    /// The English text of a read-only reason (logs, CLI, conflict messages).
    fn read_only_text(r: &ReadOnlyReason) -> String {
        match r.code {
            ReadOnlyCode::Loopback => "the loopback interface is not configurable".into(),
            ReadOnlyCode::Enslaved => format!(
                "a member of {}: change it from there",
                r.master.as_deref().unwrap_or("?")
            ),
            ReadOnlyCode::NotEthernet => {
                "only Ethernet ports and the bonds rynas made are configurable in this version".into()
            }
            ReadOnlyCode::Ifupdown => {
                "configured by ifupdown (/etc/network/interfaces): use \"let rynas manage\" first"
                    .into()
            }
            ReadOnlyCode::NetworkManager => {
                "managed by NetworkManager: rynas does not change it".into()
            }
            ReadOnlyCode::NoNetworkd => {
                "systemd-networkd is not running: rynas configures interfaces through it".into()
            }
            ReadOnlyCode::Other => "not configurable".into(),
        }
    }

    fn read_only_reason(inv: &Inventory, l: &Link) -> Option<String> {
        Self::read_only(inv, l).map(|r| Self::read_only_text(&r))
    }

    /// `GET /network/interfaces`.
    pub fn interfaces(&self, user: &str, local: Option<IpAddr>) -> Result<Vec<NetInterface>> {
        let inv = self.inventory()?;
        let config = self.current_config();
        let staged = self.staged_for(user)?;
        let mgmt = Self::management_iface(&inv, local);
        let mut out = Vec::new();
        for l in inv.links.iter().filter(|l| l.kind != LinkKind::Loopback) {
            let backend = inv.backend_of(l);
            let managed = config.contains_key(&l.name);
            let applied = if managed {
                config.get(&l.name).cloned()
            } else if backend == NetBackend::Networkd {
                l.nd_file
                    .as_deref()
                    .and_then(|f| self.os.read_config(Path::new(f)))
                    .map(|t| NetworkFile::parse(&t).to_config())
            } else {
                None
            };
            let read_only = Self::read_only(&inv, l);
            out.push(NetInterface {
                name: l.name.clone(),
                index: l.index,
                kind: l.kind,
                physical: l.physical,
                mac: l.mac.clone(),
                mtu: l.mtu,
                max_mtu: l.max_mtu,
                oper_state: l.oper_state.clone(),
                carrier: l.carrier,
                speed_mbps: l.speed_mbps,
                duplex: l.duplex,
                driver: l.driver.clone(),
                addresses: l.addresses.clone(),
                dns: l.nd_dns.clone(),
                backend,
                managed_by_rynas: managed,
                editable: read_only.is_none(),
                read_only_reason: read_only.as_ref().map(Self::read_only_text),
                read_only,
                config_file: l.nd_file.clone(),
                applied,
                pending: staged.get(&l.name).cloned(),
                ifupdown: inv
                    .ifupdown
                    .as_ref()
                    .map(|i| i.of(&l.name))
                    .unwrap_or_default(),
                link_local_v4: l
                    .mac
                    .as_deref()
                    .filter(|_| inventory::is_port(l) || managed)
                    .and_then(networkd::link_local_for_mac)
                    .map(|ip| ip.to_string()),
                management: mgmt.as_deref() == Some(l.name.as_str()),
                bond: bond::status(&inv, l),
            });
        }
        // bonds the user is creating: not a link yet
        for (name, c) in staged.iter().filter(|(n, c)| c.bond.is_some() && inv.link(n).is_none()) {
            out.push(NetInterface {
                name: name.clone(),
                kind: LinkKind::Bond,
                backend: NetBackend::Networkd,
                editable: true,
                pending: Some(c.clone()),
                link_local_v4: c
                    .bond
                    .as_ref()
                    .and_then(|b| b.mac.as_deref())
                    .and_then(networkd::link_local_for_mac)
                    .map(|ip| ip.to_string()),
                ..Default::default()
            });
        }
        Ok(out)
    }

    /// `GET /network/state`.
    pub fn state(&self, user: &str, local: Option<IpAddr>) -> Result<NetworkState> {
        let inv = self.inventory()?;
        let st = self.store.state()?;
        Ok(NetworkState {
            hostname: self.hostname(),
            services: inv.services.clone(),
            routes: inv.routes.clone(),
            dns: inv.dns.clone(),
            trial: self.trial_view(st.lkg)?,
            lkg_revision: st.lkg,
            // a change confirmed after the rollback makes it history, also when an older rynas
            // confirmed it without clearing the notice
            last_rollback: self.store.last_rollback()?.map(|r| r.report).filter(|r| {
                !st.lkg
                    .and_then(|id| self.store.revision(id).ok().flatten())
                    .is_some_and(|lkg| lkg.id != r.restored_revision && lkg.ts_ms > r.ts_ms)
            }),
            pending: self.staged_for(user)?.into_keys().collect(),
            management_interface: Self::management_iface(&inv, local),
            ifupdown_adoptable: inv.ifupdown.as_ref().is_some_and(|i| !i.is_empty())
                && inv
                    .links
                    .iter()
                    .any(|l| inv.backend_of(l) == NetBackend::Ifupdown),
            timeout_default_s: TIMEOUT_DEFAULT_S,
            timeout_min_s: TIMEOUT_MIN_S,
            timeout_max_s: TIMEOUT_MAX_S,
            read_at_ms: inv.read_at_ms,
        })
    }

    /// The change under test, or the migration planned for the next boot (`deadline_ms` 0) so
    /// that every session sees it and can cancel it, not only the one that planned it.
    fn trial_view(&self, lkg: Option<u64>) -> Result<Option<NetTrial>> {
        if let Some(m) = self.store.marker()? {
            return Ok(Some(m.view()));
        }
        let Some(b) = self.store.boot_adopt()? else {
            return Ok(None);
        };
        let interfaces = self
            .store
            .revision(b.revision)?
            .map(|r| r.config.keys().cloned().collect())
            .unwrap_or_default();
        Ok(Some(NetTrial {
            kind: TrialKind::AdoptAtBoot,
            revision: b.revision,
            lkg_revision: lkg.unwrap_or_default(),
            started_ms: b.ts_ms,
            deadline_ms: 0,
            timeout_s: BOOT_CHECK_S,
            interfaces,
            management_touched: true,
            author: b.author,
        }))
    }

    pub fn hostname(&self) -> String {
        self.os
            .read_runtime("/proc/sys/kernel/hostname")
            .unwrap_or_else(|| "localhost".into())
    }

    /// The rescue addresses of the ports (and of the bonds, whose members have none).
    pub fn link_locals(&self) -> Result<Vec<RescueLinkLocal>> {
        let inv = self.inventory()?;
        Ok(inv
            .paths()
            .into_iter()
            .filter_map(|l| {
                let mac = l.mac.clone()?;
                let ll = networkd::link_local_for_mac(&mac)?.to_string();
                Some(RescueLinkLocal {
                    interface: l.name.clone(),
                    active: l.addresses.iter().any(|a| a.address == ll),
                    mac,
                    address: ll,
                })
            })
            .collect())
    }

    pub fn revisions(&self) -> Result<Vec<NetRevision>> {
        let lkg = self.store.state()?.lkg;
        let mut out = Vec::new();
        for id in self.store.revision_ids().into_iter().rev() {
            if let Some(r) = self.store.revision(id)? {
                out.push(NetRevision {
                    id,
                    ts_ms: r.ts_ms,
                    author: r.author,
                    comment: r.comment,
                    kind: rev_kind_name(r.kind).into(),
                    interfaces: r.config.keys().cloned().collect(),
                    backend: if r.services.ifupdown && !r.services.networkd {
                        NetBackend::Ifupdown
                    } else {
                        NetBackend::Networkd
                    },
                    good: r.good,
                    lkg: Some(id) == lkg,
                });
            }
        }
        Ok(out)
    }

    /// Events waiting for the notification engine.
    pub fn take_events(&self) -> Vec<NewEvent> {
        let _l = self.store.lock();
        self.store.take_events().unwrap_or_default()
    }

    fn emit(&self, e: NewEvent) {
        if let Err(err) = self.store.push_event(e) {
            tracing::warn!("network event not queued: {err}");
        }
    }

    // -----------------------------------------------------------------------------------------
    // staging
    // -----------------------------------------------------------------------------------------

    /// `PUT /network/interfaces/{name}`: stages the user's change of one interface (a bond,
    /// also a new one, when the settings carry `bond`).
    pub fn stage(&self, user: &str, iface: &str, c: &InterfaceConfig) -> Result<()> {
        self.stage_from(user, iface, c, None)
    }

    /// [`Engine::stage`] knowing the local address of the session: a new bond takes the MAC
    /// (and so the address) of the member the session comes through.
    pub fn stage_from(
        &self,
        user: &str,
        iface: &str,
        c: &InterfaceConfig,
        local: Option<IpAddr>,
    ) -> Result<()> {
        if c.bond.is_some() {
            return self.stage_bond(user, iface, c, local);
        }
        if self.current_config().get(iface).is_some_and(|x| x.bond.is_some()) {
            return Err(NetError::InvalidField(
                "bond",
                format!("{iface} is a bond: its settings include its members"),
            ));
        }
        if !networkd::valid_iface(iface) {
            return Err(NetError::NotFound(format!("no interface {iface}")));
        }
        let inv = self.inventory()?;
        let l = inv
            .link(iface)
            .ok_or_else(|| NetError::NotFound(format!("no interface {iface}")))?;
        if let Some(r) = Self::read_only_reason(&inv, l) {
            return Err(NetError::Conflict(format!("{iface}: {r}")));
        }
        let norm = validate::check_interface(iface, c, l.max_mtu)
            .map_err(|e| NetError::InvalidField(e.field, e.message))?;
        let _lock = self.store.lock()?;
        let mut all = self.store.staged()?;
        let mine = all.entry(user.to_string()).or_default();
        if self.current_config().get(iface) == Some(&norm) {
            mine.remove(iface);
        } else {
            mine.insert(iface.to_string(), norm);
        }
        if mine.is_empty() {
            all.remove(user);
        }
        self.store.set_staged(&all)?;
        Ok(())
    }

    /// Stages a new or changed bond: members checked against the ports, settings normalized
    /// (sorted members, the MAC of the port that holds the address, LACP hash). Ports that
    /// leave the bond are staged without an IPv4 address (link-local only).
    fn stage_bond(
        &self,
        user: &str,
        name: &str,
        c: &InterfaceConfig,
        local: Option<IpAddr>,
    ) -> Result<()> {
        let mut want = c.bond.clone().unwrap_or_default();
        if !bond::valid_name(name) {
            return Err(NetError::InvalidField(
                "name",
                format!("{name}: a bond is named bond0 to bond99"),
            ));
        }
        let inv = self.inventory()?;
        let current = self.current_config();
        let before = current.get(name).and_then(|x| x.bond.clone());
        if inv.link(name).is_some() && before.is_none() {
            return Err(NetError::Conflict(format!(
                "{name} exists and is not a bond rynas manages"
            )));
        }
        // a changed bond keeps its MAC (and so its address) unless told otherwise
        if want.mac.as_deref().is_none_or(|m| m.trim().is_empty()) {
            want.mac = before.as_ref().and_then(|b| b.mac.clone());
        }
        // the ports must be free for rynas (not ifupdown's, NetworkManager's...)
        for m in &want.members {
            let Some(l) = inv.link(m.trim()) else { continue };
            match Self::read_only(&inv, l) {
                None => {}
                Some(ReadOnlyReason {
                    code: ReadOnlyCode::Enslaved,
                    master: Some(x),
                }) if x == name => {}
                Some(ReadOnlyReason {
                    code: ReadOnlyCode::NotEthernet,
                    ..
                }) => {} // reported by bond::check
                Some(r) => {
                    return Err(NetError::InvalidField(
                        "bond.members",
                        format!("{m}: {}", Self::read_only_text(&r)),
                    ));
                }
            }
        }
        // the member the session comes through holds "the" address (two DHCP ports have two)
        let mgmt = Self::management_iface(&inv, local)
            .filter(|m| want.members.iter().any(|x| x.trim() == m.as_str()));
        let facts = |p: &str| {
            inv.link(p).map(|l| {
                let mut f = bond::PortFacts::of(l);
                if let Some(m) = &mgmt {
                    f.has_ipv4 = m == p;
                }
                f
            })
        };
        let min_mtu = want
            .members
            .iter()
            .filter_map(|m| inv.link(m.trim()).and_then(|l| l.max_mtu))
            .min();
        let ip = validate::check_interface(name, &InterfaceConfig { bond: None, ..c.clone() }, min_mtu)
            .map_err(|e| NetError::InvalidField(e.field, e.message))?;
        let _lock = self.store.lock()?;
        let mut all = self.store.staged()?;
        let mine = all.entry(user.to_string()).or_default();
        // members of the other bonds: those in force (unless this user removes them) and
        // those this user stages
        let mut others = current.clone();
        for (k, v) in mine.iter() {
            if v.removed {
                others.remove(k);
            } else {
                others.insert(k.clone(), v.clone());
            }
        }
        others.remove(name);
        let taken = bond::members(&others);
        let b = bond::check(name, &want, ip.mtu, &facts, &taken, false)
            .map_err(|e| NetError::InvalidField(e.field, e.message))?;
        for w in bond::warnings(name, &b, &facts) {
            tracing::info!("network: {w}");
        }
        let norm = InterfaceConfig {
            bond: Some(b.clone()),
            ..ip
        };
        // members carry no settings of their own
        mine.retain(|k, _| !b.members.contains(k));
        if current.get(name) == Some(&norm) {
            mine.remove(name);
        } else {
            mine.insert(name.to_string(), norm);
        }
        if let Some(old) = &before {
            let disabled = InterfaceConfig {
                ipv4: Ipv4Method::Disabled,
                ..Default::default()
            };
            if mine.contains_key(name) {
                for p in old.members.iter().filter(|p| !b.members.contains(p)) {
                    mine.entry(p.clone()).or_insert_with(|| disabled.clone());
                }
            } else {
                // back to the bond in force: what an earlier edit staged for its ports goes
                for p in &old.members {
                    mine.remove(p);
                }
            }
        }
        if mine.is_empty() {
            all.remove(user);
        }
        self.store.set_staged(&all)?;
        Ok(())
    }

    /// `DELETE /network/interfaces/{name}`: stages the deletion of a bond rynas manages (or
    /// drops a bond the user has only staged). The port with the bond's MAC takes the bond's
    /// settings (and its DHCP identity), the other members get none (link-local only).
    pub fn stage_remove(&self, user: &str, name: &str) -> Result<()> {
        let inv = self.inventory()?;
        let current = self.current_config();
        let _lock = self.store.lock()?;
        let mut all = self.store.staged()?;
        let mine = all.entry(user.to_string()).or_default();
        match current.get(name) {
            Some(c) if c.bond.is_some() => {
                let b = c.bond.clone().unwrap_or_default();
                let heir = b
                    .members
                    .iter()
                    .find(|m| {
                        inv.link(m)
                            .and_then(Link::perm_mac)
                            .zip(b.mac.as_deref())
                            .is_some_and(|(x, y)| x.eq_ignore_ascii_case(y))
                    })
                    .or(b.members.first())
                    .cloned();
                mine.insert(
                    name.to_string(),
                    InterfaceConfig {
                        removed: true,
                        ..Default::default()
                    },
                );
                for m in &b.members {
                    let cfg = if heir.as_deref() == Some(m.as_str()) {
                        InterfaceConfig {
                            bond: None,
                            ..c.clone()
                        }
                    } else {
                        InterfaceConfig {
                            ipv4: Ipv4Method::Disabled,
                            ..Default::default()
                        }
                    };
                    mine.insert(m.clone(), cfg);
                }
            }
            _ if mine.get(name).is_some_and(|c| c.bond.is_some()) => {
                mine.remove(name);
            }
            _ => {
                return Err(NetError::NotFound(format!(
                    "{name}: no bond rynas manages"
                )));
            }
        }
        if mine.is_empty() {
            all.remove(user);
        }
        self.store.set_staged(&all)?;
        Ok(())
    }

    pub fn discard(&self, user: &str) -> Result<()> {
        let _lock = self.store.lock()?;
        let mut all = self.store.staged()?;
        all.remove(user);
        self.store.set_staged(&all)?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // planning
    // -----------------------------------------------------------------------------------------

    fn timeout(t: Option<u32>) -> Result<u32> {
        let t = t.unwrap_or(TIMEOUT_DEFAULT_S);
        if !(TIMEOUT_MIN_S..=TIMEOUT_MAX_S).contains(&t) {
            return Err(NetError::Invalid(format!(
                "the confirmation timeout must be {TIMEOUT_MIN_S}-{TIMEOUT_MAX_S} s"
            )));
        }
        Ok(t)
    }

    /// The files of a networkd configuration: rynas files for `config` (a bond: its `.netdev`,
    /// its `.network` and one per member), removal of the rynas files it no longer has.
    fn networkd_files(
        &self,
        config: &BTreeMap<String, InterfaceConfig>,
        inv: &Inventory,
    ) -> Vec<FileState> {
        let mut files: Vec<FileState> = Vec::new();
        for (i, c) in config {
            match &c.bond {
                Some(b) => {
                    files.push(FileState::present(
                        bond::netdev_path(i),
                        managed(&bond::render_netdev(i, b)),
                    ));
                    files.push(FileState::present(
                        networkd::network_path(i),
                        managed(&bond::render_network(i, c)),
                    ));
                    for m in &b.members {
                        files.push(FileState::present(
                            networkd::network_path(m),
                            managed(&bond::render_member(m, i, b)),
                        ));
                    }
                }
                None => {
                    let mac = inv.link(i).and_then(|l| l.mac.clone());
                    files.push(FileState::present(
                        networkd::network_path(i),
                        render_file(i, c, mac.as_deref()),
                    ));
                }
            }
        }
        for p in self.rynas_files() {
            if !files.iter().any(|f| f.path == p) {
                files.push(FileState::absent(p));
            }
        }
        files
    }

    /// The DHCP identity follows the address: a new DHCP bond takes the identity of the port
    /// whose MAC it takes (the port's drop-in moves to the bond; without one, the identity
    /// networkd used for the port's lease is written down), and a removed bond gives its
    /// drop-in back to the port that takes its settings. So the server keeps giving the same
    /// address. Part of the revision, so a rollback puts the drop-ins back.
    fn bond_identity_files(
        &self,
        current: &BTreeMap<String, InterfaceConfig>,
        target: &BTreeMap<String, InterfaceConfig>,
        inv: &Inventory,
    ) -> Vec<FileState> {
        let mut files = Vec::new();
        let read = |p: &Path| self.os.read_config(p);
        let has_mac = |port: &str, mac: Option<&str>| {
            inv.link(port)
                .and_then(Link::perm_mac)
                .zip(mac)
                .is_some_and(|(x, y)| x.eq_ignore_ascii_case(y))
        };
        for (name, c) in target {
            let Some(b) = &c.bond else { continue };
            if c.ipv4 != Ipv4Method::Dhcp || current.get(name).is_some_and(|x| x.bond.is_some()) {
                continue;
            }
            let Some(src) = b.members.iter().find(|m| has_mac(m, b.mac.as_deref())) else {
                continue;
            };
            let src_path = networkd::dhcp_identity_path(src);
            let text = match read(&src_path) {
                Some(t) => {
                    files.push(FileState::absent(src_path));
                    Some(t)
                }
                None => inv.link(src).and_then(|l| {
                    let lease = self
                        .os
                        .read_runtime(&format!("/run/systemd/netif/leases/{}", l.index))?;
                    let (id, addr) = networkd::lease_identity(&lease)?;
                    Some(networkd::render_dhcp_identity(&id, "systemd-networkd", addr))
                }),
            };
            if let Some(t) = text {
                files.push(FileState::present(networkd::dhcp_identity_path(name), t));
            }
        }
        for (name, c) in current {
            let Some(b) = &c.bond else { continue };
            if target.get(name).is_some_and(|x| x.bond.is_some()) {
                continue;
            }
            let path = networkd::dhcp_identity_path(name);
            let Some(t) = read(&path) else { continue };
            files.push(FileState::absent(path));
            let dhcp = |m: &&String| target.get(*m).is_some_and(|x| x.ipv4 == Ipv4Method::Dhcp);
            let heir = b
                .members
                .iter()
                .filter(dhcp)
                .find(|m| has_mac(m, b.mac.as_deref()))
                .or_else(|| b.members.iter().find(dhcp));
            if let Some(h) = heir
                && read(&networkd::dhcp_identity_path(h)).is_none()
            {
                files.push(FileState::present(networkd::dhcp_identity_path(h), t));
            }
        }
        files
    }


    /// Drop-ins that keep, on the ports networkd takes over on DHCP, the client identifier the
    /// DHCP client of ifupdown sent and the address it had, so the server gives the same
    /// address and the page is not lost. ifupdown runs the first of dhclient, udhcpc, dhcpcd
    /// that is installed; a port whose previous identity cannot be read for sure (udhcpc, a
    /// `client` or `hwaddress` option in its stanza, a configuration the readers refuse)
    /// gets no drop-in (networkd's default), and comes back in the second list.
    fn dhcp_identity_files(
        &self,
        ifs: &Interfaces,
        config: &BTreeMap<String, InterfaceConfig>,
        inv: &Inventory,
    ) -> (Vec<FileState>, Vec<String>) {
        let exists = |p: &str| ["/usr/sbin/", "/sbin/"].iter().any(|d| self.os.path(&Path::new(d).join(p)).exists());
        let read = |p: &str| self.os.read_config(Path::new(p));
        let dhcp: Vec<&String> = config.iter().filter(|(_, c)| c.ipv4 == Ipv4Method::Dhcp).map(|(i, _)| i).collect();
        let client = if exists("dhclient") {
            "dhclient"
        } else if exists("udhcpc") {
            return (Vec::new(), dhcp.into_iter().cloned().collect());
        } else if exists("dhcpcd") {
            "dhcpcd"
        } else {
            return (Vec::new(), Vec::new());
        };
        let (mut files, mut unknown) = (Vec::new(), Vec::new());
        for iface in dhcp {
            let link = inv.link(iface);
            let mac = link.and_then(|l| l.mac.as_deref());
            // a bond's `hwaddress` is the MAC it keeps (the client saw that one)
            let is_bond = config.get(iface).is_some_and(|c| c.bond.is_some());
            let own_id = ifs.of(iface).iter().any(|s| {
                s.other
                    .iter()
                    .any(|o| o.name == "client" || (o.name == "hwaddress" && !is_bond))
            });
            let id = match client {
                _ if own_id => None,
                "dhclient" => networkd::dhclient_identity(read("/etc/dhcp/dhclient.conf").as_deref()),
                _ => read("/etc/dhcpcd.conf").and_then(|conf| {
                    let duid = read("/var/lib/dhcpcd/duid").or_else(|| read("/etc/dhcpcd.duid"));
                    networkd::dhcpcd_identity(&conf, duid.as_deref(), mac)
                }),
            };
            let Some(id) = id else {
                unknown.push(iface.clone());
                continue;
            };
            let leased = link.and_then(|l| {
                l.routable_v4().into_iter().find(|a| a.dynamic).and_then(|a| a.address.parse().ok())
            });
            files.push(FileState::present(
                networkd::dhcp_identity_path(iface),
                networkd::render_dhcp_identity(&id, client, leased),
            ));
        }
        (files, unknown)
    }

    /// The interfaces `managed` does not configure. Left out: the members of its bonds and any
    /// enslaved link (their addresses move to the bond), and links rynas configures now that
    /// the change drops (a bond it deletes).
    fn others(inv: &Inventory, managed: &BTreeMap<String, InterfaceConfig>) -> Vec<OtherPort> {
        let members = bond::members(managed);
        inv.links
            .iter()
            .filter(|l| {
                l.kind != LinkKind::Loopback
                    && !managed.contains_key(&l.name)
                    && !members.contains_key(&l.name)
                    && l.master.is_none()
                    && !l.rynas_file()
            })
            .map(|l| OtherPort {
                name: l.name.clone(),
                port: inventory::is_port(l),
                subnets: l
                    .static_v4()
                    .iter()
                    .filter_map(|a| a.parse().ok())
                    .collect(),
                has_ipv4: !l.routable_v4().is_empty(),
            })
            .collect()
    }

    fn candidate_urls(
        &self,
        inv: &Inventory,
        config: &BTreeMap<String, InterfaceConfig>,
        touched: &[String],
    ) -> Vec<String> {
        let scheme = if self.web.https { "https" } else { "http" };
        let port = self.web.port;
        let mut urls = Vec::new();
        for i in touched {
            if let Some(c) = config.get(i) {
                for a in &c.addresses {
                    let ip = a.split('/').next().unwrap_or(a);
                    urls.push(format!("{scheme}://{ip}:{port}/"));
                }
                // a DHCP bond keeps the identity, so the members' leased address comes back
                if let Some(b) = c.bond.as_ref().filter(|_| c.ipv4 == Ipv4Method::Dhcp) {
                    for m in &b.members {
                        for a in inv.link(m).map(|l| l.routable_v4()).unwrap_or_default() {
                            urls.push(format!("{scheme}://{}:{port}/", a.address));
                        }
                    }
                }
            }
        }
        urls.push(format!("{scheme}://{}.local:{port}/", self.hostname()));
        for i in touched {
            if let Some(ll) = inv
                .link(i)
                .and_then(|l| l.mac.as_deref())
                .and_then(networkd::link_local_for_mac)
            {
                urls.push(format!("{scheme}://{ll}:{port}/"));
            }
        }
        // addresses that stay (other interfaces)
        for l in &inv.links {
            if touched.contains(&l.name) || l.kind == LinkKind::Loopback {
                continue;
            }
            for a in l.routable_v4() {
                urls.push(format!("{scheme}://{}:{port}/", a.address));
            }
        }
        urls.dedup();
        urls
    }

    /// Common part of every plan: validation, touched interfaces, management path.
    #[allow(clippy::too_many_arguments)]
    fn finish_plan(
        &self,
        inv: &Inventory,
        kind: TrialKind,
        target: Revision,
        touched: Vec<String>,
        local: Option<IpAddr>,
        timeout_s: Option<u32>,
        skip_test: bool,
    ) -> Result<Plan> {
        let timeout_s = Self::timeout(timeout_s)?;
        let mgmt = Self::management_iface(inv, local);
        let management_touched = match &mgmt {
            Some(m) => touched.contains(m),
            // unknown (loopback, a proxy): assume the worst
            None => !touched.is_empty(),
        };
        if skip_test && management_touched {
            return Err(NetError::Invalid(match mgmt {
                Some(m) => format!(
                    "the change touches {m}, the interface this session comes through: the timed test cannot be skipped"
                ),
                None => {
                    "the interface of this session is unknown: the timed test cannot be skipped"
                        .into()
                }
            }));
        }
        let mut removed = Vec::new();
        let mut new_addresses = Vec::new();
        // an address that moves (a port's address to its new bond) is neither removed nor new
        let kept: BTreeSet<&String> = target.config.values().flat_map(|c| &c.addresses).collect();
        let held: BTreeSet<String> = inv.links.iter().flat_map(|l| l.static_v4()).collect();
        for i in &touched {
            let now: Vec<String> = inv.link(i).map(|l| l.static_v4()).unwrap_or_default();
            let want: Vec<String> = target
                .config
                .get(i)
                .map(|c| c.addresses.clone())
                .unwrap_or_default();
            removed.extend(
                now.iter()
                    .filter(|a| !want.contains(a) && !kept.contains(a))
                    .map(|a| a.split('/').next().unwrap_or(a).to_string()),
            );
            new_addresses.extend(
                want.iter()
                    .filter(|a| !now.contains(a) && !held.contains(*a))
                    .map(|a| (i.clone(), a.split('/').next().unwrap_or(a).to_string())),
            );
        }
        let candidate_urls = self.candidate_urls(inv, &target.config, &touched);
        Ok(Plan {
            kind,
            target,
            interfaces: touched,
            management_touched,
            timeout_s,
            skip_test,
            candidate_urls,
            removed_addresses: removed,
            new_addresses,
        })
    }

    /// `POST /network/apply`: the user's staged changes as a plan.
    pub fn plan_apply(
        &self,
        user: &str,
        author: &str,
        comment: &str,
        local: Option<IpAddr>,
        timeout_s: Option<u32>,
        skip_test: bool,
    ) -> Result<Plan> {
        let staged = self.staged_for(user)?;
        if staged.is_empty() {
            return Err(NetError::NothingPending("no staged network change".into()));
        }
        let inv = self.inventory_fresh()?;
        let current = self.current_config();
        let is_bond = |c: Option<&InterfaceConfig>| c.is_some_and(|c| c.bond.is_some());
        // the bonds this change makes, edits or removes: their members may be staged
        let bonds: BTreeSet<&String> = staged
            .iter()
            .filter(|(i, c)| c.bond.is_some() || c.removed || is_bond(current.get(*i)))
            .map(|(i, _)| i)
            .collect();
        for (i, c) in &staged {
            if c.bond.is_some() {
                continue; // checked with the ports below
            }
            if c.removed {
                if !is_bond(current.get(i)) {
                    return Err(NetError::Conflict(format!("{i}: no bond rynas manages")));
                }
                continue;
            }
            let l = inv
                .link(i)
                .ok_or_else(|| NetError::Conflict(format!("{i} is gone")))?;
            match Self::read_only(&inv, l) {
                None => {}
                Some(ReadOnlyReason {
                    code: ReadOnlyCode::Enslaved,
                    master: Some(m),
                }) if bonds.contains(&m) => {}
                Some(r) => {
                    return Err(NetError::Conflict(format!(
                        "{i}: {}",
                        Self::read_only_text(&r)
                    )));
                }
            }
        }
        let mut config = current.clone();
        for (i, c) in staged {
            if c.removed {
                config.remove(&i);
            } else {
                config.insert(i, c);
            }
        }
        // members carry no settings of their own: the bond writes their files
        let members = bond::members(&config);
        config.retain(|k, _| !members.contains_key(k));
        let mut touched: BTreeSet<String> = current
            .keys()
            .chain(config.keys())
            .filter(|k| current.get(*k) != config.get(*k))
            .cloned()
            .collect();
        let mut bond_touched = false;
        for k in touched.clone() {
            for side in [current.get(&k), config.get(&k)] {
                if let Some(b) = side.and_then(|c| c.bond.as_ref()) {
                    bond_touched = true;
                    touched.extend(b.members.iter().cloned());
                }
            }
        }
        if touched.is_empty() {
            return Err(NetError::NothingPending(
                "the staged settings are already applied".into(),
            ));
        }
        if skip_test && bond_touched {
            return Err(NetError::Invalid(
                "a bond change always runs the timed test: a switch that is not ready can cut the network".into(),
            ));
        }
        self.check_bonds(&mut config, &inv, false)?;
        let config = validate::check_config(
            &config,
            &|n| inv.link(n).and_then(|l| l.max_mtu),
            // a member the change frees is a port again
            &|n| inv.link(n).is_some_and(inventory::is_physical_port),
            &Self::others(&inv, &config),
        )
        .map_err(NetError::Invalid)?;
        let mut files = self.networkd_files(&config, &inv);
        files.extend(self.bond_identity_files(&current, &config, &inv));
        let target = Revision {
            ts_ms: self.os.now_ms(),
            author: author.into(),
            comment: comment.into(),
            kind: RevKind::Apply,
            files,
            services: Services {
                networkd: true,
                ifupdown: self.services_now(&inv).ifupdown,
            },
            config,
            ..Default::default()
        };
        self.finish_plan(
            &inv,
            TrialKind::Apply,
            target,
            touched.into_iter().collect(),
            local,
            timeout_s,
            skip_test,
        )
    }

    /// Checks and normalizes every bond of `config` against the ports now (`adopting`: the
    /// bonds come from ifupdown and exist already).
    fn check_bonds(
        &self,
        config: &mut BTreeMap<String, InterfaceConfig>,
        inv: &Inventory,
        adopting: bool,
    ) -> Result<()> {
        let facts = |p: &str| inv.link(p).map(bond::PortFacts::of);
        let names: Vec<String> = config
            .iter()
            .filter(|(_, c)| c.bond.is_some())
            .map(|(n, _)| n.clone())
            .collect();
        for name in names {
            if let Some(l) = inv.link(&name)
                && !(l.kind == LinkKind::Bond && (adopting || l.rynas_file()))
            {
                return Err(NetError::Conflict(format!(
                    "{name} exists and is not a bond rynas manages"
                )));
            }
            let mut others = config.clone();
            others.remove(&name);
            let taken = bond::members(&others);
            let c = config.get_mut(&name).expect("listed");
            let b = c.bond.clone().unwrap_or_default();
            let checked = bond::check(&name, &b, c.mtu, &facts, &taken, adopting)
                .map_err(|e| NetError::Invalid(e.message))?;
            c.bond = Some(checked);
        }
        Ok(())
    }

    /// The networkd configuration equivalent to ifupdown, as a migration target.
    fn adopt_target(&self, inv: &Inventory, author: &str) -> Result<(Revision, Vec<String>)> {
        let ifs: Interfaces = inv
            .ifupdown
            .clone()
            .filter(|i| !i.is_empty())
            .ok_or_else(|| NetError::Conflict("ifupdown configures no interface".into()))?;
        let mut config = ifupdown::to_networkd(&ifs).map_err(NetError::Invalid)?;
        // a bond keeps the MAC it runs with (and so its address and DHCP identity)
        for (name, c) in config.iter_mut() {
            if let Some(b) = c.bond.as_mut().filter(|b| b.mac.is_none()) {
                b.mac = inv.link(name).and_then(|l| l.mac.clone());
            }
        }
        // the DHCP ports keep the identity ifupdown's client had (only those ifupdown ran)
        let (identity, unknown) = self.dhcp_identity_files(&ifs, &config, inv);
        // keep what rynas already manages on other interfaces
        for (k, v) in self.current_config() {
            config.entry(k).or_insert(v);
        }
        self.check_bonds(&mut config, inv, true)?;
        let config = validate::check_config(
            &config,
            &|n| inv.link(n).and_then(|l| l.max_mtu),
            &|n| inv.is_port(n) || inv.link(n).is_none(),
            &Self::others(inv, &config),
        )
        .map_err(NetError::Invalid)?;
        let original = self
            .os
            .read_config(Path::new(INTERFACES))
            .unwrap_or_default();
        let mut files = self.networkd_files(&config, inv);
        files.extend(identity);
        files.push(FileState::present(
            INTERFACES,
            ifupdown::LOOPBACK_ONLY.into(),
        ));
        files.push(FileState::present(INTERFACES_BACKUP, original));
        let touched = ifs.names();
        let mut comment = String::from("ifupdown configuration handed to rynas (systemd-networkd)");
        if !unknown.is_empty() {
            comment.push_str(&format!(
                "; DHCP identity of the previous client not readable on {}: the server may give a new address",
                unknown.join(", ")
            ));
        }
        Ok((
            Revision {
                ts_ms: self.os.now_ms(),
                author: author.into(),
                comment,
                kind: RevKind::Adopt,
                files,
                services: Services {
                    networkd: true,
                    ifupdown: false,
                },
                config,
                ..Default::default()
            },
            touched,
        ))
    }

    /// `POST /network/adopt` with `mode: now`.
    pub fn plan_adopt(
        &self,
        author: &str,
        local: Option<IpAddr>,
        timeout_s: Option<u32>,
    ) -> Result<Plan> {
        let inv = self.inventory_fresh()?;
        let (target, touched) = self.adopt_target(&inv, author)?;
        self.finish_plan(
            &inv,
            TrialKind::Adopt,
            target,
            touched,
            local,
            timeout_s,
            false,
        )
    }

    /// `POST /network/revisions/{n}/restore`.
    pub fn plan_restore(
        &self,
        id: u64,
        author: &str,
        local: Option<IpAddr>,
        timeout_s: Option<u32>,
    ) -> Result<Plan> {
        let r = self
            .store
            .revision(id)?
            .ok_or_else(|| NetError::NotFound(format!("no network revision {id}")))?;
        let inv = self.inventory_fresh()?;
        let mut touched: BTreeSet<String> = r.config.keys().cloned().collect();
        touched.extend(r.observed.keys().cloned());
        touched.extend(self.current_config().into_keys());
        let touched: Vec<String> = touched
            .into_iter()
            .filter(|i| inv.link(i).is_some())
            .collect();
        let target = Revision {
            id: 0,
            ts_ms: self.os.now_ms(),
            author: author.into(),
            comment: format!("revision {id} restored"),
            kind: RevKind::Restore,
            good: false,
            ..r
        };
        self.finish_plan(
            &inv,
            TrialKind::Apply,
            target,
            touched,
            local,
            timeout_s,
            false,
        )
    }

    // -----------------------------------------------------------------------------------------
    // the transaction
    // -----------------------------------------------------------------------------------------

    /// Which backends the system runs: active now or enabled for the next boot (at boot the
    /// services have not started yet, so "active" alone would say neither).
    fn services_now(&self, inv: &Inventory) -> Services {
        let enabled = |unit: &str| {
            self.os
                .run(&["systemctl", "is-enabled", unit], QUICK)
                .is_ok_and(|o| o.stdout.trim() == "enabled")
        };
        Services {
            networkd: inv.services.networkd || enabled("systemd-networkd.service"),
            ifupdown: inv.services.ifupdown || enabled("networking.service"),
        }
    }

    /// The system now, for every path `target` governs (and every rynas file), with the
    /// observed state of the interfaces: the revision a rollback restores.
    fn snapshot(&self, target: &Revision, inv: &Inventory, author: &str) -> Revision {
        let mut paths: BTreeSet<PathBuf> = target.files.iter().map(|f| f.path.clone()).collect();
        paths.extend(self.rynas_files());
        paths.insert(PathBuf::from(INTERFACES));
        let files = paths
            .into_iter()
            .map(|p| match self.os.read_config(&p) {
                Some(c) => FileState::present(p, c),
                None => FileState::absent(p),
            })
            .collect();
        let observed = inv
            .links
            .iter()
            .filter(|l| l.kind != LinkKind::Loopback && l.master.is_none())
            .filter(|l| {
                inventory::is_port(l) || target.config.contains_key(&l.name) || l.networkd_managed()
            })
            .map(|l| {
                let gateway = inv
                    .routes
                    .iter()
                    .find(|r| {
                        r.destination == "default"
                            && r.interface == l.name
                            && r.family == rynas_api_types::network::IpFamily::Ipv4
                            && !matches!(r.protocol.as_deref(), Some("dhcp" | "ra"))
                    })
                    .and_then(|r| r.gateway.clone());
                (
                    l.name.clone(),
                    Observed {
                        mtu: l.mtu,
                        static_v4: l.static_v4(),
                        gateway,
                    },
                )
            })
            .collect();
        Revision {
            ts_ms: self.os.now_ms(),
            author: author.into(),
            comment: "the system before the change".into(),
            kind: RevKind::Snapshot,
            config: self.current_config(),
            files,
            services: self.services_now(inv),
            observed,
            good: true,
            ..Default::default()
        }
    }

    /// Static addresses and gateways of the ifupdown file a revision carries.
    fn ifupdown_static(rev: &Revision) -> BTreeMap<String, (Vec<String>, Option<String>)> {
        let mut out: BTreeMap<String, (Vec<String>, Option<String>)> = BTreeMap::new();
        if !(rev.services.ifupdown && !rev.services.networkd) {
            return out;
        }
        let Some(text) = rev
            .files
            .iter()
            .find(|f| f.path == Path::new(INTERFACES))
            .and_then(|f| f.content.as_deref())
        else {
            return out;
        };
        let mut ifs = Interfaces::default();
        ifupdown::parse_text(text, INTERFACES, &mut |_, _| Vec::new(), &mut ifs, 0);
        for (n, s) in &ifs.stanzas {
            if s.family == "inet" && s.method == "static" {
                let e = out.entry(n.clone()).or_default();
                e.0.extend(s.address.clone());
                if e.1.is_none() {
                    e.1 = s.gateway.clone();
                }
            }
        }
        out
    }

    fn expectations(rev: &Revision, other: &Revision) -> BTreeMap<String, Expect> {
        let mut out = BTreeMap::new();
        let ifup = Self::ifupdown_static(rev);
        let mut names: BTreeSet<&String> = rev.config.keys().collect();
        names.extend(rev.observed.keys());
        names.extend(ifup.keys());
        for n in names {
            let e = if rev.kind == RevKind::Snapshot
                && (rev.observed.contains_key(n) || ifup.contains_key(n))
            {
                let o = rev.observed.get(n);
                let (ia, ig) = ifup.get(n).cloned().unwrap_or_default();
                let mut addrs = o.map(|o| o.static_v4.clone()).unwrap_or_default();
                for a in ia {
                    if !addrs.contains(&a) {
                        addrs.push(a);
                    }
                }
                Expect {
                    addrs,
                    mtu: o.map(|o| o.mtu),
                    gateway: o.and_then(|o| o.gateway.clone()).or(ig),
                }
            } else if let Some(c) = rev.config.get(n) {
                Expect {
                    addrs: if c.ipv4 == Ipv4Method::Static {
                        c.addresses.clone()
                    } else {
                        Vec::new()
                    },
                    // no MTU in the file: the MTU it had before the change
                    mtu: c.mtu.or_else(|| other.observed.get(n).map(|o| o.mtu)),
                    gateway: c.gateway.clone(),
                }
            } else {
                continue;
            };
            out.insert(n.clone(), e);
        }
        out
    }

    /// Differences between the system and what `rev` wants (coming from `other`).
    fn verify(rev: &Revision, other: &Revision, inv: &Inventory) -> Vec<String> {
        let want = Self::expectations(rev, other);
        let before = Self::expectations(other, rev);
        let mut problems = Vec::new();
        for (n, e) in &want {
            let Some(l) = inv.link(n) else {
                if !e.addrs.is_empty() {
                    problems.push(format!("{n}: the interface is missing"));
                }
                continue;
            };
            // networkd configures addresses only on a link with carrier
            if l.carrier == Some(false) {
                continue;
            }
            let have: Vec<String> = l
                .routable_v4()
                .iter()
                .map(|a| format!("{}/{}", a.address, a.prefix_len))
                .collect();
            for a in &e.addrs {
                if !have.contains(a) {
                    problems.push(format!("{n}: address {a} is missing"));
                }
            }
            if let Some(old) = before.get(n) {
                for a in old.addrs.iter().filter(|a| !e.addrs.contains(a)) {
                    if l.static_v4().contains(a) {
                        problems.push(format!("{n}: old address {a} is still there"));
                    }
                }
            }
            if let Some(m) = e.mtu
                && l.mtu != m
            {
                problems.push(format!("{n}: MTU is {}, expected {m}", l.mtu));
            }
            if let Some(g) = &e.gateway
                && inv.default_gateway(n).as_deref() != Some(g.as_str())
            {
                problems.push(format!("{n}: no default route through {g}"));
            }
        }
        // bonds: kind, mode, members, MAC, the LACP partner; old bonds gone
        let to_ifupdown = rev.services.ifupdown && !rev.services.networkd;
        problems.extend(bond::verify(&rev.config, &other.config, to_ifupdown, inv));
        problems
    }

    /// Fixes what networkd leaves behind: MTUs it does not reset, old addresses (only once the
    /// new ones are up, never cutting the working path). When `restoring` a known good revision,
    /// missing addresses and default routes are also put back by hand.
    fn repair(
        &self,
        rev: &Revision,
        other: &Revision,
        inv: &Inventory,
        restoring: bool,
        log: &dyn Fn(&str),
    ) {
        let want = Self::expectations(rev, other);
        let before = Self::expectations(other, rev);
        for (n, e) in &want {
            let Some(l) = inv.link(n) else { continue };
            if let Some(m) = e.mtu
                && l.mtu != m
            {
                log(&format!("{n}: setting MTU {m} (was {})", l.mtu));
                let m = m.to_string();
                let _ = self
                    .os
                    .run(&["ip", "link", "set", "dev", n, "mtu", &m], QUICK);
            }
            let have: Vec<String> = l
                .routable_v4()
                .iter()
                .map(|a| format!("{}/{}", a.address, a.prefix_len))
                .collect();
            let missing: Vec<&String> = e.addrs.iter().filter(|a| !have.contains(a)).collect();
            if restoring && l.carrier != Some(false) {
                for a in &missing {
                    log(&format!("{n}: adding address {a} by hand"));
                    let _ = self.os.run(&["ip", "addr", "add", a, "dev", n], QUICK);
                }
                if let Some(g) = &e.gateway
                    && inv.default_gateway(n).as_deref() != Some(g.as_str())
                {
                    log(&format!(
                        "{n}: adding the default route through {g} by hand"
                    ));
                    let _ = self.os.run(
                        &["ip", "route", "replace", "default", "via", g, "dev", n],
                        QUICK,
                    );
                }
            }
            if !missing.is_empty() && !restoring {
                continue;
            }
            if let Some(old) = before.get(n) {
                for a in old.addrs.iter().filter(|a| !e.addrs.contains(a)) {
                    if l.static_v4().contains(a) {
                        log(&format!("{n}: removing old address {a}"));
                        let _ = self.os.run(&["ip", "addr", "del", a, "dev", n], QUICK);
                    }
                }
            }
        }
    }

    /// Verifies `rev` (polling up to 20 s, 35 s with a 802.3ad bond), repairing on the way;
    /// returns what is still wrong.
    fn settle(
        &self,
        rev: &Revision,
        other: &Revision,
        restoring: bool,
        log: &dyn Fn(&str),
    ) -> Vec<String> {
        if self.os.dry() {
            log("dry system: actual state not verified");
            return Vec::new();
        }
        let mut problems = Vec::new();
        // a LACP partner answers within one slow LACPDU period
        let rounds = if bond::has_lacp(&rev.config) {
            bond::LACP_WAIT_S
        } else {
            20
        };
        for i in 0..rounds {
            let inv = match self.inventory_fresh() {
                Ok(v) => v,
                Err(e) => {
                    problems = vec![e.to_string()];
                    self.os.sleep(Duration::from_secs(1));
                    continue;
                }
            };
            problems = Self::verify(rev, other, &inv);
            if problems.is_empty() {
                return problems;
            }
            if i == 2 || i == 8 {
                self.repair(rev, other, &inv, restoring, log);
            }
            if i == 8 && rev.services.networkd {
                let names: Vec<&str> = rev
                    .config
                    .keys()
                    .map(String::as_str)
                    .filter(|n| inv.link(n).is_some())
                    .collect();
                if !names.is_empty() {
                    let mut argv = vec!["networkctl", "reconfigure"];
                    argv.extend(names);
                    let _ = self.os.run(&argv, SLOW);
                }
            }
            self.os.sleep(Duration::from_secs(1));
        }
        problems
    }

    fn write_file(&self, f: &FileState) -> std::io::Result<()> {
        let real = self.os.path(&f.path);
        match &f.content {
            Some(c) => {
                if std::fs::read_to_string(&real).ok().as_deref() == Some(c.as_str()) {
                    return Ok(());
                }
                // a drop-in folder (`05-rynas-<port>.network.d`) may not exist yet
                if let Some(dir) = real.parent().filter(|d| !d.exists()) {
                    std::fs::create_dir_all(dir)?;
                }
                let tmp = rynas_config::files::candidate_path(&real);
                write_synced(&tmp, c.as_bytes(), f.mode)?;
                rename_synced(&tmp, &real)
            }
            None => match std::fs::remove_file(&real) {
                Ok(()) => {
                    if let Some(d) = real.parent().and_then(|d| std::fs::File::open(d).ok()) {
                        let _ = d.sync_all();
                    }
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        }
    }

    fn systemctl(&self, args: &[&str], log: &dyn Fn(&str)) {
        let mut argv = vec!["systemctl"];
        argv.extend(args);
        if let Err(e) = self.os.run_ok(&argv, SLOW) {
            log(&format!("warning: {e}"));
        }
    }

    fn unit_installed(&self, unit: &str) -> bool {
        [
            "/usr/lib/systemd/system",
            "/lib/systemd/system",
            "/etc/systemd/system",
        ]
        .iter()
        .any(|d| self.os.runtime_exists(&format!("{d}/{unit}")))
    }

    /// Makes the system run `rev`, coming from `from` (the revision running now).
    fn activate(
        &self,
        rev: &Revision,
        from: &Revision,
        mode: Mode,
        log: &dyn Fn(&str),
    ) -> Result<()> {
        let to_ifupdown = rev.services.ifupdown && !rev.services.networkd;
        let from_ifupdown = from.services.ifupdown && !from.services.networkd;
        let ifupdown_to_networkd = from_ifupdown && rev.services.networkd && !rev.services.ifupdown;
        let networkd_to_ifupdown = to_ifupdown && !from_ifupdown;
        let old_ifs = ifupdown::load(self.os.root())
            .map(|i| i.names())
            .unwrap_or_default();
        if mode == Mode::Live && ifupdown_to_networkd {
            // ifdown with the file that brought the interfaces up (clears ifupdown's state)
            log("stopping ifupdown (networking.service)");
            self.systemctl(&["stop", "networking.service"], log);
        }
        let mut writes = rev.files.clone();
        // rynas files the revision does not know (made later) go away
        for p in self.rynas_files() {
            if !rev.files.iter().any(|f| f.path == p) {
                writes.push(FileState::absent(p));
            }
        }
        // what networkd will not do by itself: worked out from the files before they change
        let teardown = if mode == Mode::Live {
            bond::teardown(&|p| self.os.read_config(p), &writes)
        } else {
            bond::Teardown::default()
        };
        for f in &writes {
            self.write_file(f)?;
        }
        log(&format!("files of revision {} written", rev.id));
        let nd_units = ["systemd-networkd.service", "systemd-networkd.socket"];
        if mode == Mode::Boot {
            // only what switches backend: a system running both keeps both
            if rev.services.networkd != from.services.networkd {
                for u in nd_units {
                    self.systemctl(
                        &[
                            if rev.services.networkd {
                                "enable"
                            } else {
                                "disable"
                            },
                            u,
                        ],
                        log,
                    );
                }
                if rev.services.networkd {
                    self.systemctl(&["start", "--no-block", "systemd-networkd.service"], log);
                }
            }
            if rev.services.ifupdown != from.services.ifupdown {
                self.systemctl(
                    &[
                        if rev.services.ifupdown {
                            "enable"
                        } else {
                            "disable"
                        },
                        "networking.service",
                    ],
                    log,
                );
                if rev.services.ifupdown {
                    self.systemctl(&["start", "--no-block", "networking.service"], log);
                }
            }
            return Ok(());
        }
        let links: Vec<String> = self
            .inventory_fresh()
            .map(|i| i.links.iter().map(|l| l.name.clone()).collect())
            .unwrap_or_default();
        let exists = |n: &String| links.contains(n);
        // networkd never frees a port nor deletes or re-kinds a netdev
        for p in teardown.release.iter().filter(|p| exists(p)) {
            log(&format!("{p}: leaving its bond"));
            if let Err(e) = self.os.run_ok(&["ip", "link", "set", "dev", p, "nomaster"], QUICK) {
                log(&format!("warning: {e}"));
            }
        }
        for b in teardown.delete.iter().filter(|b| exists(b)) {
            log(&format!("{b}: deleting the link (networkd does not change it in place)"));
            if let Err(e) = self.os.run_ok(&["ip", "link", "del", "dev", b], QUICK) {
                log(&format!("warning: {e}"));
            }
        }
        if ifupdown_to_networkd {
            for i in old_ifs.iter().filter(|i| exists(i)) {
                let _ = self
                    .os
                    .run(&["ip", "addr", "flush", "dev", i, "scope", "global"], QUICK);
            }
            self.systemctl(&["disable", "networking.service"], log);
            for u in nd_units {
                self.systemctl(&["enable", u], log);
            }
            self.systemctl(&["restart", "systemd-networkd.service"], log);
            if self.unit_installed("systemd-resolved.service") {
                self.systemctl(&["enable", "--now", "systemd-resolved.service"], log);
            } else {
                log(
                    "systemd-resolved is not installed: rynas writes /etc/resolv.conf from networkd's DNS servers",
                );
            }
        } else if networkd_to_ifupdown {
            log("handing the network back to ifupdown");
            self.systemctl(
                &[
                    "stop",
                    "systemd-networkd.service",
                    "systemd-networkd.socket",
                ],
                log,
            );
            for u in nd_units {
                self.systemctl(&["disable", u], log);
            }
            let mut names: BTreeSet<String> = ifupdown::load(self.os.root())
                .map(|i| i.names().into_iter().collect())
                .unwrap_or_default();
            names.extend(from.config.keys().cloned());
            for i in names.iter().filter(|i| exists(i)) {
                let _ = self.os.run(&["ifdown", "--force", i], QUICK);
                let _ = self
                    .os
                    .run(&["ip", "addr", "flush", "dev", i, "scope", "global"], QUICK);
            }
            self.systemctl(&["enable", "networking.service"], log);
            self.systemctl(&["restart", "networking.service"], log);
            return Ok(());
        }
        if rev.services.networkd {
            // reload reconfigures exactly the links whose matching file appeared, changed or
            // went away; an extra `reconfigure` would bounce their addresses a second time
            // (`settle` uses it only as a retry)
            if let Err(e) = self.os.run_ok(&["networkctl", "reload"], SLOW) {
                log(&format!("warning: {e}"));
            }
            log("networkd reloaded");
            // members of a bond made again: their file did not change, the reload left them
            let again: Vec<&str> = teardown
                .reconfigure
                .iter()
                .filter(|p| exists(p))
                .map(String::as_str)
                .collect();
            if !again.is_empty() {
                let mut argv = vec!["networkctl", "reconfigure"];
                argv.extend(again);
                if let Err(e) = self.os.run_ok(&argv, SLOW) {
                    log(&format!("warning: {e}"));
                }
            }
        }
        self.invalidate();
        Ok(())
    }

    fn arm_timer(&self, seconds: u32, reason: &str) -> Result<()> {
        let unit = format!("--unit={ROLLBACK_UNIT}");
        let timer = format!("{ROLLBACK_UNIT}.timer");
        let service = format!("{ROLLBACK_UNIT}.service");
        let _ = self.os.run(&["systemctl", "stop", &timer], QUICK);
        let _ = self
            .os
            .run(&["systemctl", "reset-failed", &timer, &service], QUICK);
        let on = format!("--on-active={seconds}s");
        self.os
            .run_ok(
                &[
                    "systemd-run",
                    &unit,
                    "--description=rynas: roll back an unconfirmed network change",
                    &on,
                    "--timer-property=AccuracySec=1s",
                    "--collect",
                    &self.rynasctl,
                    "net",
                    "rollback",
                    "--reason",
                    reason,
                ],
                QUICK,
            )
            .map_err(|e| NetError::Failed(format!("cannot arm the rollback timer: {e}")))?;
        Ok(())
    }

    fn disarm_timer(&self) {
        let timer = format!("{ROLLBACK_UNIT}.timer");
        let _ = self.os.run(&["systemctl", "stop", &timer], QUICK);
    }

    /// Checks the new addresses with `arping -D` (duplicate address detection).
    fn check_duplicates(&self, plan: &Plan, log: &dyn Fn(&str)) -> Result<()> {
        if self.os.dry() {
            return Ok(());
        }
        for (iface, ip) in &plan.new_addresses {
            match self.os.run(
                &["arping", "-D", "-q", "-c", "2", "-w", "3", "-I", iface, ip],
                QUICK,
            ) {
                Ok(o) if o.code == Some(0) => {}
                Ok(o) if o.code == Some(1) => {
                    return Err(NetError::Invalid(format!(
                        "{ip} is already used by another device on {iface}"
                    )));
                }
                Ok(_) | Err(_) => log(&format!(
                    "warning: could not check whether {ip} is free on {iface} (arping unavailable or no carrier)"
                )),
            }
        }
        Ok(())
    }

    /// Runs a plan: snapshot, marker, timer, files, reload, verification. With a token it
    /// returns while the change waits for its confirmation; without (skip_test) the change is
    /// final once verified. A failed verification rolls back at once.
    pub fn begin(
        &self,
        plan: &Plan,
        token: Option<&str>,
        task_id: Option<&str>,
        log: &dyn Fn(&str),
    ) -> Result<Begun> {
        let _lock = self.store.lock()?;
        if self.store.marker()?.is_some() {
            return Err(NetError::Conflict(
                "another network change is waiting for its confirmation".into(),
            ));
        }
        self.check_duplicates(plan, log)?;
        // whether names resolve now: a change must not take that away
        let dns_before = !self.os.dry() && self.dns_answers_now() == Some(true);
        let inv = self.inventory_fresh()?;
        let lkg =
            self.store
                .add_revision(self.snapshot(&plan.target, &inv, &plan.target.author))?;
        let target = self.store.add_revision(plan.target.clone())?;
        log(&format!(
            "last known good: revision {}; testing revision {}",
            lkg.id, target.id
        ));
        let now = self.os.now_ms();
        let deadline_ms = now + u64::from(plan.timeout_s) * 1000;
        if let Some(t) = token {
            self.store.set_marker(&Marker {
                kind: plan.kind,
                revision: target.id,
                lkg: lkg.id,
                started_ms: now,
                deadline_ms,
                timeout_s: plan.timeout_s,
                token_sha256: sha256_hex(t.as_bytes()),
                interfaces: plan.interfaces.clone(),
                management_touched: plan.management_touched,
                removed_addresses: plan.removed_addresses.clone(),
                author: plan.target.author.clone(),
                task_id: task_id.map(str::to_string),
            })?;
            // never change the network without the safety net
            if let Err(e) = self.arm_timer(plan.timeout_s, reason::TIMEOUT) {
                self.store.clear_marker()?;
                return Err(e);
            }
            log(&format!("rollback timer armed: {} s", plan.timeout_s));
        }
        if let Err(e) = self.activate(&target, &lkg, Mode::Live, log) {
            log(&format!("applying failed: {e}; rolling back"));
            return Err(self.fail_back(&lkg, &target, &e.to_string(), token.is_some(), log));
        }
        let problems = self.settle(&target, &lkg, false, log);
        if !problems.is_empty() {
            for p in &problems {
                log(&format!("verification: {p}"));
            }
            return Err(self.fail_back(&lkg, &target, &problems.join("; "), token.is_some(), log));
        }
        if let Err(why) = self.verify_dns(&target, dns_before, log) {
            log(&format!("verification: {why}"));
            return Err(self.fail_back(&lkg, &target, &why, token.is_some(), log));
        }
        log("verified: the interfaces run the new settings");
        if token.is_none() {
            self.commit(&target)?;
            log(&format!(
                "revision {} applied without the timed test",
                target.id
            ));
        }
        Ok(Begun {
            revision: target.id,
            lkg: lkg.id,
            deadline_ms,
        })
    }

    /// Restores `lkg` after a failed apply (lock held); returns the error to report.
    fn fail_back(
        &self,
        lkg: &Revision,
        target: &Revision,
        why: &str,
        armed: bool,
        log: &dyn Fn(&str),
    ) -> NetError {
        let report = self.restore_locked(lkg, target, reason::VERIFY_FAILED, log);
        if armed {
            self.disarm_timer();
        }
        let _ = self.store.clear_marker();
        NetError::Failed(format!(
            "the new settings did not come up ({why}); revision {} restored{}",
            lkg.id,
            if report.verified {
                " and verified"
            } else {
                ", but NOT verified: check the console"
            }
        ))
    }

    fn commit(&self, target: &Revision) -> Result<()> {
        let mut t = target.clone();
        t.good = true;
        self.store.update_revision(&t)?;
        self.store.set_lkg(t.id)?;
        // the staged changes this revision applied are done
        let mut staged = self.store.staged()?;
        for m in staged.values_mut() {
            m.retain(|i, c| {
                if c.removed {
                    t.config.get(i).is_some_and(|x| x.bond.is_some())
                } else {
                    t.config.get(i) != Some(c)
                }
            });
        }
        staged.retain(|_, m| !m.is_empty());
        self.store.set_staged(&staged)?;
        // a revision that works now: the notice about an earlier rollback is history (best
        // effort: the revision is already the last known good one)
        if let Err(e) = self.store.clear_last_rollback() {
            tracing::warn!("network: cannot clear the last rollback notice: {e}");
        }
        self.invalidate();
        Ok(())
    }

    /// `POST /network/confirm`: the change becomes the last known good revision.
    pub fn confirm(&self, token: &str, origin: Origin) -> Result<u64> {
        let _lock = self.store.lock()?;
        let m = self.store.marker()?.ok_or_else(|| {
            NetError::NothingPending("no network change is waiting for a confirmation".into())
        })?;
        // A migration done at boot has no browser that applied it, so no token: any
        // administrator who reaches the system over one of its addresses may keep it (the
        // address check below still applies). Otherwise it keeps itself at the deadline when a
        // port has a routable address.
        if m.kind != TrialKind::AdoptAtBoot && sha256_hex(token.as_bytes()) != m.token_sha256 {
            return Err(NetError::Invalid("wrong confirmation token".into()));
        }
        if self.os.now_ms() > m.deadline_ms {
            return Err(NetError::Conflict(
                "too late: the change is being rolled back".into(),
            ));
        }
        if let Origin::Web(local) = origin
            && (m.management_touched || !m.removed_addresses.is_empty())
        {
            let ip = local.ok_or_else(|| {
                NetError::Conflict(
                    "the confirmation must come over the new path (local address unknown)".into(),
                )
            })?;
            let ip = match ip {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
                v4 => v4,
            };
            if ip.is_loopback()
                || m.removed_addresses
                    .iter()
                    .any(|a| a.parse::<IpAddr>().ok() == Some(ip))
            {
                return Err(NetError::Conflict(format!(
                    "the confirmation came through {ip}, which does not prove the new settings work: open one of the new addresses"
                )));
            }
            let inv = self.inventory_fresh()?;
            if inv.iface_of_ip(&ip).is_none() {
                return Err(NetError::Conflict(format!(
                    "{ip} is not an address of this system any more"
                )));
            }
        }
        self.disarm_timer();
        let target = self
            .store
            .revision(m.revision)?
            .ok_or_else(|| NetError::Io(format!("revision {} is missing", m.revision)))?;
        self.commit(&target)?;
        self.store.clear_marker()?;
        self.emit(event(
            NotifySeverity::Info,
            "confirmed",
            (
                "net.confirmed",
                &[
                    ("revision", m.revision.to_string()),
                    ("interfaces", m.interfaces.join(", ")),
                ],
            ),
            format!("Network change confirmed (revision {})", m.revision),
            format!("Interfaces: {}", m.interfaces.join(", ")),
        ));
        Ok(m.revision)
    }

    /// Restores `rev` coming from `from` (lock held) and verifies it.
    fn restore_locked(
        &self,
        rev: &Revision,
        from: &Revision,
        reason: &str,
        log: &dyn Fn(&str),
    ) -> NetRollbackReport {
        log(&format!("restoring revision {} ({reason})", rev.id));
        let mut problems = Vec::new();
        if let Err(e) = self.activate(rev, from, Mode::Live, log) {
            problems.push(e.to_string());
        }
        problems.extend(self.settle(rev, from, true, log));
        for p in &problems {
            log(&format!("after the rollback: {p}"));
        }
        let report = NetRollbackReport {
            ts_ms: self.os.now_ms(),
            reason: reason.into(),
            restored_revision: rev.id,
            verified: problems.is_empty(),
            problems,
        };
        let _ = self.store.set_last_rollback(&report);
        let (sev, title, code) = if report.verified {
            (
                NotifySeverity::Warning,
                format!(
                    "Network change rolled back ({reason}): revision {} restored",
                    rev.id
                ),
                "net.rolled_back",
            )
        } else {
            (
                NotifySeverity::Critical,
                format!("Network rollback ({reason}) could not be verified"),
                "net.rollback_unverified",
            )
        };
        self.emit(event(
            sev,
            "rollback",
            (
                code,
                &[
                    ("reason", reason.to_string()),
                    ("revision", rev.id.to_string()),
                    ("problems", report.problems.join("; ")),
                ],
            ),
            title,
            if report.problems.is_empty() {
                format!("The previous network settings run again and were verified (revision {}).", rev.id)
            } else {
                format!("Still wrong: {}. Rescue: the link-local address of the port or the reset button.", report.problems.join("; "))
            },
        ));
        self.invalidate();
        report
    }

    /// Rolls the trial back (timer, API, watchdog). At its deadline a migration done at boot is
    /// confirmed instead when a port has a routable address and names resolve (a DNS server
    /// answers, when networkd learned any); asked by an administrator
    /// (`reason::MANUAL`) it is always rolled back. `Ok(None)`: nothing to roll back.
    pub fn rollback(&self, why: &str, log: &dyn Fn(&str)) -> Result<Option<NetRollbackReport>> {
        let _lock = self.store.lock()?;
        let Some(m) = self.store.marker()? else {
            if why == reason::MANUAL && self.store.boot_adopt()?.is_some() {
                self.store.clear_boot_adopt()?;
                log("the migration planned for the next boot is cancelled");
                return Ok(None);
            }
            return Ok(None);
        };
        let now = self.os.now_ms();
        if (why == reason::TIMEOUT || why == reason::BOOT_CHECK) && now + 5_000 < m.deadline_ms {
            // a timer left over from an older trial
            log("the trial's deadline is not reached yet: nothing to do");
            return Ok(None);
        }
        let target = self.store.revision(m.revision)?.unwrap_or_default();
        // only the timer keeps a migration done at boot; an administrator who asks for the
        // rollback gets ifupdown back
        if m.kind == TrialKind::AdoptAtBoot && why != reason::MANUAL {
            let inv = self.inventory_fresh()?;
            let routable = inv
                .paths()
                .iter()
                .any(|p| p.carrier == Some(true) && !p.routable_v4().is_empty());
            // names must resolve too when networkd learned DNS servers (or the file has some)
            let dns_ok = !routable || self.os.dry() || {
                let expected = inv.links.iter().any(|l| !l.nd_dns.is_empty());
                let mut ok = false;
                for _ in 0..10 {
                    self.invalidate();
                    self.sync_resolv(log);
                    match self.dns_answers_now() {
                        Some(true) => ok = true,
                        None if !expected => ok = true,
                        _ => {}
                    }
                    if ok {
                        break;
                    }
                    self.os.sleep(Duration::from_secs(1));
                }
                ok
            };
            if routable && !dns_ok {
                log("migration at boot: a port has a routable address but no DNS server answers");
                self.emit(event(
                    NotifySeverity::Warning,
                    "dns",
                    (
                        "net.boot_dns_failed",
                        &[("revision", m.lkg.to_string())],
                    ),
                    "Network migration at boot undone: names did not resolve".into(),
                    format!(
                        "systemd-networkd brought a routable address up, but no DNS server of {} answered; ifupdown (revision {}) is restored.",
                        resolv::RESOLV_CONF,
                        m.lkg
                    ),
                ));
            }
            if routable && dns_ok {
                self.commit(&target)?;
                self.store.clear_marker()?;
                self.emit(event(
                    NotifySeverity::Notice,
                    "adopted",
                    ("net.adopted", &[]),
                    "Network migrated to rynas (systemd-networkd)".into(),
                    "The migration done at boot came up with a routable address and working name resolution, and is kept."
                        .into(),
                ));
                log("migration at boot confirmed: a port has a routable address, names resolve");
                return Ok(None);
            }
        }
        let lkg = self
            .store
            .revision(m.lkg)?
            .ok_or_else(|| NetError::Io(format!("revision {} is missing", m.lkg)))?;
        let report = self.restore_locked(&lkg, &target, why, log);
        if why != reason::TIMEOUT && why != reason::BOOT_CHECK {
            self.disarm_timer();
        }
        self.store.clear_marker()?;
        Ok(Some(report))
    }

    /// Early boot, before networkd and ifupdown: an interrupted trial is undone, a planned
    /// migration is carried out (with its own deadline).
    pub fn boot_guard(&self, log: &dyn Fn(&str)) -> Result<String> {
        let _lock = self.store.lock()?;
        if let Some(m) = self.store.marker()? {
            let lkg = self
                .store
                .revision(m.lkg)?
                .ok_or_else(|| NetError::Io(format!("revision {} is missing", m.lkg)))?;
            let target = self.store.revision(m.revision)?.unwrap_or_default();
            self.activate(&lkg, &target, Mode::Boot, log)?;
            self.store.clear_marker()?;
            let report = NetRollbackReport {
                ts_ms: self.os.now_ms(),
                reason: reason::BOOT_GUARD.into(),
                restored_revision: lkg.id,
                verified: false,
                problems: vec![
                    "restored before networking started; verified by the watchdog after boot"
                        .into(),
                ],
            };
            self.store.set_last_rollback(&report)?;
            self.emit(event(
                NotifySeverity::Warning,
                "rollback",
                (
                    "net.boot_undone",
                    &[
                        ("revision", lkg.id.to_string()),
                        ("tested", m.revision.to_string()),
                    ],
                ),
                format!(
                    "Unconfirmed network change undone at boot: revision {} restored",
                    lkg.id
                ),
                format!(
                    "The system restarted while revision {} was being tested.",
                    m.revision
                ),
            ));
            return Ok(format!(
                "unconfirmed revision {} undone: revision {} restored",
                m.revision, lkg.id
            ));
        }
        if let Some(b) = self.store.boot_adopt()? {
            self.store.clear_boot_adopt()?;
            let target = self
                .store
                .revision(b.revision)?
                .ok_or_else(|| NetError::Io(format!("revision {} is missing", b.revision)))?;
            let inv = self.inventory_fresh().unwrap_or_default();
            let lkg = self
                .store
                .add_revision(self.snapshot(&target, &inv, &b.author))?;
            self.activate(&target, &lkg, Mode::Boot, log)?;
            let now = self.os.now_ms();
            self.store.set_marker(&Marker {
                kind: TrialKind::AdoptAtBoot,
                revision: target.id,
                lkg: lkg.id,
                started_ms: now,
                deadline_ms: now + u64::from(BOOT_CHECK_S) * 1000,
                timeout_s: BOOT_CHECK_S,
                token_sha256: sha256_hex(new_token().as_bytes()),
                interfaces: target.config.keys().cloned().collect(),
                management_touched: true,
                removed_addresses: Vec::new(),
                author: b.author,
                task_id: None,
            })?;
            if let Err(e) = self.arm_timer(BOOT_CHECK_S, reason::BOOT_CHECK) {
                // without the timer the watchdog (5 min after boot) is the safety net
                log(&format!("warning: {e}"));
            }
            return Ok(format!(
                "ifupdown migrated to networkd (revision {}); ifupdown comes back if in {BOOT_CHECK_S} s no port has a routable address or no DNS server answers",
                target.id
            ));
        }
        Ok("nothing to do".into())
    }

    /// `POST /network/adopt` with `mode: next_boot`.
    pub fn adopt_next_boot(&self, author: &str) -> Result<(u64, Vec<String>)> {
        let inv = self.inventory_fresh()?;
        let (target, touched) = self.adopt_target(&inv, author)?;
        let _lock = self.store.lock()?;
        if self.store.marker()?.is_some() {
            return Err(NetError::Conflict(
                "a network change is waiting for its confirmation".into(),
            ));
        }
        let t = self.store.add_revision(target)?;
        self.store.set_boot_adopt(&BootAdopt {
            revision: t.id,
            author: author.into(),
            ts_ms: self.os.now_ms(),
        })?;
        Ok((t.id, touched))
    }

    // -----------------------------------------------------------------------------------------
    // name resolution (`resolv`)
    // -----------------------------------------------------------------------------------------

    /// `/etc/resolv.conf`: its content (following a link) and the link target.
    fn read_resolv(&self) -> (Option<String>, Option<String>) {
        let real = self.os.path(Path::new(resolv::RESOLV_CONF));
        let link = std::fs::symlink_metadata(&real)
            .ok()
            .filter(|m| m.file_type().is_symlink())
            .and_then(|_| std::fs::read_link(&real).ok())
            .map(|t| t.to_string_lossy().into_owned());
        let content = match &link {
            // the target lives in /run (resolved), outside a test root
            Some(t) if t.starts_with('/') => self
                .os
                .read_runtime(t)
                .or_else(|| std::fs::read_to_string(&real).ok()),
            _ => std::fs::read_to_string(&real).ok(),
        };
        (content, link)
    }

    /// The servers glibc asks: those of `/etc/resolv.conf`, or resolved's upstream servers
    /// when the file points at its stub.
    fn effective_servers(&self) -> Vec<String> {
        let (content, _) = self.read_resolv();
        let servers = inventory::parse_resolv(content.as_deref().unwrap_or(""), "").servers;
        if servers.iter().all(|s| s == "127.0.0.53")
            && let Some(up) = self.os.read_runtime(resolv::RESOLVED_UPSTREAM)
        {
            let up = inventory::parse_resolv(&up, "").servers;
            if !up.is_empty() {
                return up;
            }
        }
        servers
    }

    /// `None`: no DNS server is configured; otherwise whether one of them answers.
    pub fn dns_answers_now(&self) -> Option<bool> {
        let servers = self.effective_servers();
        if servers.is_empty() {
            return None;
        }
        Some(
            servers
                .iter()
                .take(resolv::MAXNS)
                .any(|s| self.os.dns_answers(s)),
        )
    }

    /// Keeps `/etc/resolv.conf` working while rynas runs the network with networkd: the link
    /// to systemd-resolved's stub when resolved is active, otherwise a file with networkd's DNS
    /// servers. Leaves alone a file somebody else manages, and does nothing while ifupdown or
    /// NetworkManager runs. Returns what changed (rynasd calls it at start and every minute).
    pub fn sync_resolv(&self, log: &dyn Fn(&str)) -> Option<String> {
        let inv = self.inventory_fresh().ok()?;
        let s = &inv.services;
        // `networking.service` stays "active" (exited) for the rest of the boot in which rynas
        // took over: ifupdown only counts while its file still configures an interface.
        let ifupdown = s.ifupdown && inv.ifupdown.as_ref().is_some_and(|i| !i.is_empty());
        if !s.networkd || ifupdown || s.network_manager || self.rynas_files().is_empty() {
            return None;
        }
        let (content, link) = self.read_resolv();
        let owner = resolv::classify(content.as_deref(), link.as_deref());
        if !owner.replaceable() {
            return None;
        }
        let had_servers = !inventory::parse_resolv(content.as_deref().unwrap_or(""), "")
            .servers
            .is_empty();
        let real = self.os.path(Path::new(resolv::RESOLV_CONF));
        let what = if s.resolved {
            if !self.os.runtime_exists(resolv::RESOLVED_STUB) {
                return None;
            }
            let tmp = rynas_config::files::candidate_path(&real);
            let _ = std::fs::remove_file(&tmp);
            if let Err(e) = std::os::unix::fs::symlink(resolv::RESOLVED_STUB, &tmp)
                .and_then(|()| rename_synced(&tmp, &real))
            {
                log(&format!("warning: {}: {e}", resolv::RESOLV_CONF));
                return None;
            }
            format!(
                "{} now links to systemd-resolved ({})",
                resolv::RESOLV_CONF,
                resolv::RESOLVED_STUB
            )
        } else {
            let mut servers: Vec<String> = Vec::new();
            let mut search: Vec<String> = Vec::new();
            let mut links: Vec<&Link> = inv.links.iter().collect();
            links.sort_by_key(|l| l.index);
            for l in links {
                for a in l.nd_dns.iter().filter_map(|e| resolv::server_addr(e)) {
                    if !servers.contains(&a) {
                        servers.push(a);
                    }
                }
                for d in l.nd_domains.iter().map(|d| d.trim_start_matches('~')) {
                    if !d.is_empty() && d != "." && !search.iter().any(|x| x == d) {
                        search.push(d.to_string());
                    }
                }
            }
            if servers.is_empty() {
                // nothing better to offer: keep what is there
                return None;
            }
            let text = resolv::render(&servers, &search);
            if content.as_deref() == Some(text.as_str()) {
                return None;
            }
            if let Err(e) = self.write_file(&FileState::present(resolv::RESOLV_CONF, text)) {
                log(&format!("warning: {}: {e}", resolv::RESOLV_CONF));
                return None;
            }
            format!(
                "{} written with networkd's DNS servers: {}",
                resolv::RESOLV_CONF,
                servers
                    .iter()
                    .take(resolv::MAXNS)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        log(&what);
        if !had_servers {
            self.emit(event(
                NotifySeverity::Warning,
                "dns",
                ("net.dns_repaired", &[("detail", what.clone())]),
                "Name resolution repaired".into(),
                format!(
                    "No DNS server was configured while systemd-networkd ran the network; {what}."
                ),
            ));
        }
        Some(what)
    }

    /// After a change that leaves networkd running: `/etc/resolv.conf` in step, and a DNS
    /// server that answers (polling up to 15 s). Fails only when one answered before the
    /// change (a network without a reachable DNS server stays a valid choice).
    fn verify_dns(
        &self,
        target: &Revision,
        worked_before: bool,
        log: &dyn Fn(&str),
    ) -> std::result::Result<(), String> {
        if self.os.dry() || !target.services.networkd {
            return Ok(());
        }
        for _ in 0..15 {
            self.invalidate();
            self.sync_resolv(log);
            if self.dns_answers_now() == Some(true) {
                log("name resolution works: a DNS server answers");
                return Ok(());
            }
            self.os.sleep(Duration::from_secs(1));
        }
        if worked_before {
            return Err(format!(
                "name resolution stopped working: no DNS server of {} answers",
                resolv::RESOLV_CONF
            ));
        }
        log("warning: no DNS server answers (none did before the change either)");
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // watchdog and rescue
    // -----------------------------------------------------------------------------------------

    /// Health of every port now (pings the gateways of routable ports).
    pub fn port_health(&self) -> Result<Vec<PortHealth>> {
        let inv = self.inventory_fresh()?;
        Ok(inv
            .paths()
            .into_iter()
            .map(|l| {
                let routable = !l.routable_v4().is_empty();
                let gateway = inv.default_gateway(&l.name);
                let gateway_ok = match (&gateway, routable && l.carrier == Some(true)) {
                    (Some(g), true) => Some(
                        self.os
                            .run(
                                &["ping", "-n", "-q", "-c", "2", "-W", "2", "-I", &l.name, g],
                                QUICK,
                            )
                            .is_ok_and(|o| o.success()),
                    ),
                    _ => None,
                };
                PortHealth {
                    name: l.name.clone(),
                    carrier: l.carrier == Some(true),
                    routable,
                    gateway,
                    gateway_ok,
                }
            })
            .collect())
    }

    /// Looks at the bonds (rynasd, every minute) and notifies what got worse since the last
    /// look: a member lost its link or counted failures, the active member changed, a
    /// 802.3ad bond lost its partner or split into aggregators. The first look only learns.
    pub fn watch_bonds(&self) {
        let Ok(inv) = self.inventory_fresh() else {
            return;
        };
        let now: BTreeMap<String, bond::Seen> = inv
            .links
            .iter()
            .filter_map(|l| Some((l.name.clone(), bond::Seen::of(&bond::status(&inv, l)?))))
            .collect();
        let before = {
            let mut seen = self.bonds_seen.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *seen, now.clone())
        };
        for (name, s) in &now {
            let Some(b) = before.get(name) else { continue };
            for (code, fields) in bond::changes(name, b, s) {
                let f = |k: &str| {
                    fields
                        .iter()
                        .find(|(x, _)| *x == k)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default()
                };
                let (sev, title, body) = match code {
                    "net.bond_member_down" => (
                        NotifySeverity::Warning,
                        format!("{name}: {} lost its link", f("member")),
                        format!(
                            "The bond counted {} link failures on {}; the other members carry the traffic. Check the cable and the switch port.",
                            f("failures"),
                            f("member")
                        ),
                    ),
                    "net.bond_active_changed" => (
                        NotifySeverity::Info,
                        format!("{name}: {} carries the traffic now", f("to")),
                        format!("The bond moved from {} to {}.", f("from"), f("to")),
                    ),
                    _ => (
                        NotifySeverity::Warning,
                        format!("{name}: the LACP group with the switch is broken"),
                        "No LACP partner answers, or the members ended up in different aggregators: check the LACP group on the switch.".into(),
                    ),
                };
                let fields: Vec<(&str, String)> = fields.clone();
                self.emit(event(sev, &format!("bond:{name}"), (code, &fields), title, body));
            }
        }
    }

    /// The factory network: DHCP and link-local on every port (also the members of a bond),
    /// ifupdown and every netdev (bonds, VLANs, bridges, rynas's own too) put aside; the
    /// bonds are deleted by [`Engine::activate`] (their `.netdev` goes away).
    fn factory_target(&self, inv: &Inventory, author: &str, comment: &str) -> Revision {
        let config: BTreeMap<String, InterfaceConfig> = inv
            .physical_ports()
            .into_iter()
            .map(|l| (l.name.clone(), InterfaceConfig::default()))
            .collect();
        let mut files = self.networkd_files(&config, inv);
        // a bond's DHCP identity goes back to the port with its MAC
        files.extend(self.bond_identity_files(&self.current_config(), &config, inv));
        // foreign netdevs suspended (the snapshot keeps them)
        let dir = self.os.path(Path::new(NETWORKD_DIR));
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.ends_with(".netdev") {
                    files.push(FileState::absent(Path::new(NETWORKD_DIR).join(n)));
                }
            }
        }
        if inv.ifupdown.as_ref().is_some_and(|i| !i.is_empty()) {
            let original = self
                .os
                .read_config(Path::new(INTERFACES))
                .unwrap_or_default();
            files.push(FileState::present(
                INTERFACES,
                ifupdown::LOOPBACK_ONLY.into(),
            ));
            files.push(FileState::present(INTERFACES_BACKUP, original));
        }
        Revision {
            ts_ms: self.os.now_ms(),
            author: author.into(),
            comment: comment.into(),
            kind: RevKind::Rescue,
            config,
            files,
            services: Services {
                networkd: true,
                ifupdown: false,
            },
            good: true,
            ..Default::default()
        }
    }

    /// Applies the factory network at once (lock held); the previous state is kept as a
    /// revision that can be restored.
    fn factory_locked(
        &self,
        author: &str,
        comment: &str,
        log: &dyn Fn(&str),
    ) -> Result<(u64, u64)> {
        if self.store.marker()?.is_some() {
            self.disarm_timer();
            self.store.clear_marker()?;
            log("the change under test is dropped");
        }
        let inv = self.inventory_fresh()?;
        if inv.physical_ports().is_empty() {
            // never wipe a configuration for nothing
            return Err(NetError::Conflict(
                "no Ethernet port found: the factory network is not applied".into(),
            ));
        }
        let target = self.factory_target(&inv, author, comment);
        let before = self
            .store
            .add_revision(self.snapshot(&target, &inv, author))?;
        let target = self.store.add_revision(target)?;
        self.activate(&target, &before, Mode::Live, log)?;
        let problems = self.settle(&target, &before, false, log);
        for p in &problems {
            log(&format!("after the rescue configuration: {p}"));
        }
        self.commit(&target)?;
        Ok((before.id, target.id))
    }

    /// The post-boot watchdog (`rynas-net-watchdog.timer`, 5 minutes after boot).
    pub fn watchdog(&self, log: &dyn Fn(&str)) -> Result<String> {
        if self.store.marker()?.is_some() {
            return Ok("a change is being tested: its rollback timer is in charge".into());
        }
        match verdict(&self.port_health()?) {
            Verdict::Healthy => {
                return Ok("healthy: a connected port has a routable address".into());
            }
            Verdict::NoCarrier => return Ok("no port has a cable: nothing to check".into()),
            Verdict::Unhealthy => {
                log("no connected port has a routable address with a reachable gateway")
            }
        }
        // 1. the last known good revision, when the files differ from it
        let lkg_id = self.store.state()?.lkg;
        if let Some(lkg) = lkg_id.and_then(|id| self.store.revision(id).ok().flatten()) {
            let differs = lkg
                .files
                .iter()
                .any(|f| self.os.read_config(&f.path) != f.content);
            if differs {
                let _lock = self.store.lock()?;
                let inv = self.inventory_fresh()?;
                let now = self
                    .store
                    .add_revision(self.snapshot(&lkg, &inv, "watchdog"))?;
                self.activate(&lkg, &now, Mode::Live, log)?;
                let _ = self.settle(&lkg, &now, true, log);
                drop(_lock);
                for _ in 0..6 {
                    if verdict(&self.port_health()?) == Verdict::Healthy {
                        self.emit(event(
                            NotifySeverity::Warning,
                            "watchdog",
                            ("net.watchdog_restored", &[("revision", lkg.id.to_string())]),
                            format!("Network restored to the last known good revision {}", lkg.id),
                            "After boot no port had a routable address; the last known good settings fixed it.".into(),
                        ));
                        return Ok(format!("last known good revision {} restored", lkg.id));
                    }
                    self.os.sleep(Duration::from_secs(5));
                }
            }
        }
        // 2. the rescue configuration
        let _lock = self.store.lock()?;
        let (before, target) = self.factory_locked(
            "watchdog",
            "rescue configuration: no routable address after boot",
            log,
        )?;
        self.feedback.signal(rescue::Signal::RescueApplied);
        self.emit(event(
            NotifySeverity::Critical,
            "watchdog",
            ("net.rescue", &[("revision", before.to_string())]),
            "Network rescue configuration applied".into(),
            format!(
                "No port had a routable address after boot. Every port is on DHCP with link-local; the previous settings are revision {before}."
            ),
        ));
        Ok(format!(
            "rescue configuration applied (revision {target}); previous settings kept as revision {before}"
        ))
    }

    /// `rynasctl net reset-button`: one press of the reset button (a hardware module wires
    /// the button to it).
    pub fn reset_button(&self, log: &dyn Fn(&str)) -> Result<Press> {
        let state = self.paths.run.join("net-reset-press");
        let prev: Option<u64> = std::fs::read_to_string(&state)
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let now = self.os.now_ms();
        let press = rescue::classify(now, prev);
        match press {
            Press::Bounce => {}
            Press::First => {
                std::fs::create_dir_all(&self.paths.run)?;
                write_synced(&state, now.to_string().as_bytes(), 0o600)?;
                self.feedback.signal(rescue::Signal::PressAgain);
                log("reset button: press again within 10 s to restore the factory network");
            }
            Press::Second => {
                let _ = std::fs::remove_file(&state);
                self.feedback.signal(rescue::Signal::Restoring);
                let _lock = self.store.lock()?;
                let (before, target) = self.factory_locked(
                    "reset-button",
                    "factory network (reset button double press)",
                    log,
                )?;
                self.emit(event(
                    NotifySeverity::Warning,
                    "reset",
                    ("net.reset", &[("revision", before.to_string())]),
                    "Factory network restored with the reset button".into(),
                    format!(
                        "Every port is on DHCP with link-local; bonds, VLANs and bridges are suspended. The previous settings are revision {before} (Network → revisions → restore)."
                    ),
                ));
                log(&format!(
                    "factory network applied (revision {target}); previous settings kept as revision {before}"
                ));
            }
        }
        Ok(press)
    }
}

#[cfg(test)]
mod tests;
