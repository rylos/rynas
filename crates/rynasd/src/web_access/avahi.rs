//! Local app names (`<label>-<host>.local`) published through Avahi's D-Bus API
//! (`docs/phase2/docker-apps.md` §5.3, `docs/research/web-access.md` §3.2).
//!
//! **Why not `/etc/avahi/hosts`.** A static hosts entry publishes the address record *and*
//! the reverse PTR record of the address. Every app name points to the NAS's own address,
//! whose PTR the host name already owns, so Avahi refuses each entry ("Static host name
//! …: avahi_server_add_address failure: Local name collision"; seen on Debian trixie with
//! avahi-daemon 0.8-16; https://pi3g.com/?p=9174,
//! https://lists.freedesktop.org/archives/avahi/2009-April/001625.html). The hosts file has
//! no flag to skip the PTR; the D-Bus API does: `EntryGroup.AddAddress` with
//! [`AVAHI_PUBLISH_NO_REVERSE`].
//!
//! - **One entry group per name**, all owned by rynasd's own system-bus connection (Avahi
//!   frees them when the connection goes away). Avahi reports collisions per group
//!   (`StateChanged` → `COLLISION`), so a name another device holds only takes itself down.
//! - **No loop.** A collided or failed name stays reported (route problem `name_collision` /
//!   `name_unpublished`, check `local_name`) and is retried only when its address changes,
//!   when Avahi restarts, or on `POST /web-access/sync` ([`LocalNames::retry`]).
//! - **Avahi restarts** (`NameOwnerChanged` of `org.freedesktop.Avahi`) and server
//!   re-registrations (host name change, `Server.StateChanged`) republish everything once
//!   the server is `RUNNING` again.
//! - **IPv4 only**: local names are plain A records of the primary IPv4 address (research
//!   §3.2); the own-domain mode (W2) handles AAAA.
//! - Without Avahi on the bus the names are simply unavailable (diagnostics say so);
//!   nothing else in web access depends on them.
//!
//! The record set ([`records`]) and the state machine ([`Names`], behind the [`Publisher`]
//! trait) are pure; [`LocalNames`] is the handle the service keeps, and its driver
//! ([`LocalNames::spawn`]) talks zbus.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::web_access::{ProxyMode, WebAccessConfig};
use tokio::sync::{Notify, watch};

use super::model::HostFacts;

pub const AVAHI: &str = "org.freedesktop.Avahi";
pub const SERVER: &str = "org.freedesktop.Avahi.Server";
pub const ENTRY_GROUP: &str = "org.freedesktop.Avahi.EntryGroup";
/// `AvahiIfIndex`: every interface Avahi uses (`avahi-common/address.h`).
pub const AVAHI_IF_UNSPEC: i32 = -1;
/// `AvahiProtocol`: answer over mDNS on IPv4 and IPv6 (`avahi-common/address.h`).
pub const AVAHI_PROTO_UNSPEC: i32 = -1;
/// `AvahiPublishFlags`: "For address records: don't create a reverse (PTR) entry"
/// (`avahi-common/defs.h`: `AVAHI_PUBLISH_NO_REVERSE = 16`,
/// https://github.com/avahi/avahi/blob/master/avahi-common/defs.h).
pub const AVAHI_PUBLISH_NO_REVERSE: u32 = 16;
/// `AvahiServerState` (`avahi-common/defs.h`): INVALID 0, REGISTERING 1, RUNNING 2,
/// COLLISION 3, FAILURE 4.
pub const SERVER_RUNNING: i32 = 2;
/// `AvahiEntryGroupState` (`avahi-common/defs.h`).
pub const GROUP_UNCOMMITED: i32 = 0;
pub const GROUP_REGISTERING: i32 = 1;
pub const GROUP_ESTABLISHED: i32 = 2;
pub const GROUP_COLLISION: i32 = 3;
pub const GROUP_FAILURE: i32 = 4;
/// Deadline of one D-Bus call to Avahi.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The names to publish: every route's local name → the primary IPv4 address. Empty in
/// external mode, with local names off, or without an IPv4 address.
pub fn records(cfg: &WebAccessConfig, facts: &HostFacts) -> BTreeMap<String, IpAddr> {
    if cfg.settings.mode == ProxyMode::External || !cfg.settings.local_names {
        return BTreeMap::new();
    }
    let Some(addr) = facts.primary_v4 else {
        return BTreeMap::new();
    };
    super::resolve(cfg, facts)
        .routes
        .into_iter()
        .flat_map(|r| r.names)
        .map(|n| (n, IpAddr::V4(addr)))
        .collect()
}

/// `<address> <name>` lines (the plan's preview).
pub fn lines(records: &BTreeMap<String, IpAddr>) -> String {
    records.iter().map(|(n, a)| format!("{a} {n}\n")).collect()
}

// ---------------------------------------------------------------------------------------------
// state machine
// ---------------------------------------------------------------------------------------------

/// A failed publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// The name is taken (on this host or on the network).
    Collision(String),
    /// Avahi refused it for another reason.
    Failed(String),
    /// Avahi (or the bus) did not answer.
    Gone(String),
}

/// What publishes one name in its own entry group (Avahi over D-Bus, or a fake in tests).
pub trait Publisher {
    /// `EntryGroupNew`, `AddAddress`, `Commit`: the entry group's object path.
    fn publish(
        &mut self,
        name: &str,
        addr: IpAddr,
    ) -> impl Future<Output = Result<String, PublishError>> + Send;
    /// `EntryGroup.Free` (best effort).
    fn withdraw(&mut self, group: &str) -> impl Future<Output = ()> + Send;
}

/// One name's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameState {
    /// Committed, Avahi probes the name.
    Registering,
    /// Published.
    Established,
    /// Another device (or this host) uses the name.
    Collision(String),
    /// Avahi refused it.
    Failed(String),
}

/// Whether Avahi can be reached.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Availability {
    /// The driver is not running (tests, `--dry-system`) or has not checked yet.
    #[default]
    Unknown,
    /// Avahi is on the bus and its server is running.
    Up,
    /// Avahi is on the bus, its server is (re)registering its host name.
    Starting,
    /// Not on the bus (not installed, stopped) or the bus is unreachable.
    Down(String),
}

/// What diagnostics and the state show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NamesReport {
    pub avahi: Availability,
    pub names: BTreeMap<String, (IpAddr, NameState)>,
}

impl NamesReport {
    /// The route problem of `name`, if any: `name_collision`, or `name_unpublished` (Avahi
    /// unreachable, or a refusal). Nothing while the state is unknown or in progress.
    pub fn problem(&self, name: &str) -> Option<&'static str> {
        match (&self.avahi, self.names.get(name)) {
            (Availability::Down(_), _) => Some("name_unpublished"),
            (_, Some((_, NameState::Collision(_)))) => Some("name_collision"),
            (_, Some((_, NameState::Failed(_)))) => Some("name_unpublished"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    addr: IpAddr,
    group: Option<String>,
    state: NameState,
}

/// Desired names against what Avahi holds.
#[derive(Debug, Default)]
pub struct Names {
    desired: BTreeMap<String, IpAddr>,
    entries: BTreeMap<String, Entry>,
    avahi: Availability,
}

impl Names {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn report(&self) -> NamesReport {
        NamesReport {
            avahi: self.avahi.clone(),
            names: self
                .entries
                .iter()
                .map(|(n, e)| (n.clone(), (e.addr, e.state.clone())))
                .collect(),
        }
    }

    pub fn set_desired(&mut self, desired: BTreeMap<String, IpAddr>) {
        self.desired = desired;
    }

    /// Avahi appeared on the bus (a new daemon: the groups of the old one are gone) or left.
    pub fn on_owner(&mut self, present: bool) {
        self.entries.clear();
        self.avahi = if present {
            Availability::Starting
        } else {
            Availability::Down("avahi-daemon is not on the system bus".into())
        };
    }

    /// The bus or Avahi cannot be reached.
    pub fn on_down(&mut self, why: String) {
        self.entries.clear();
        self.avahi = Availability::Down(why);
    }

    /// `Server.StateChanged` (or `GetState`): publish once `RUNNING`; any other state
    /// (re-registering its own host name, collision, failure) resets our groups, which are
    /// added again when the server is running.
    pub async fn on_server_state<P: Publisher>(&mut self, state: i32, p: &mut P) {
        if state == SERVER_RUNNING {
            if self.avahi != Availability::Up {
                self.avahi = Availability::Up;
                self.withdraw_all(p).await;
            }
            self.reconcile(p).await;
        } else {
            self.withdraw_all(p).await;
            self.avahi = Availability::Starting;
        }
    }

    async fn withdraw_all<P: Publisher>(&mut self, p: &mut P) {
        for e in std::mem::take(&mut self.entries).into_values() {
            if let Some(g) = e.group {
                p.withdraw(&g).await;
            }
        }
    }

    /// `EntryGroup.StateChanged` of `group`.
    pub async fn on_group_state<P: Publisher>(
        &mut self,
        group: &str,
        state: i32,
        error: &str,
        p: &mut P,
    ) {
        let Some((name, e)) = self
            .entries
            .iter_mut()
            .find(|(_, e)| e.group.as_deref() == Some(group))
        else {
            return;
        };
        match state {
            GROUP_REGISTERING => e.state = NameState::Registering,
            GROUP_ESTABLISHED => {
                if e.state != NameState::Established {
                    tracing::info!("web access: {name} published through Avahi ({})", e.addr);
                }
                e.state = NameState::Established;
            }
            GROUP_COLLISION | GROUP_FAILURE => {
                let why = if !error.is_empty() {
                    error.to_string()
                } else if state == GROUP_COLLISION {
                    "another device on the network uses the name".to_string()
                } else {
                    "Avahi could not publish the name".to_string()
                };
                tracing::warn!("web access: {name} not published: {why}");
                e.state = if state == GROUP_COLLISION {
                    NameState::Collision(why)
                } else {
                    NameState::Failed(why)
                };
                // released; kept in the report and not retried (no loop)
                if let Some(g) = e.group.take() {
                    p.withdraw(&g).await;
                }
            }
            // GROUP_UNCOMMITED: a reset we did ourselves
            _ => {}
        }
    }

    /// Brings Avahi to the desired names: withdraws names that went away or moved,
    /// publishes the missing ones. Collided and failed names stay as they are.
    pub async fn reconcile<P: Publisher>(&mut self, p: &mut P) {
        if self.avahi != Availability::Up {
            return;
        }
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(n, e)| self.desired.get(*n) != Some(&e.addr))
            .map(|(n, _)| n.clone())
            .collect();
        for n in stale {
            if let Some(Entry { group: Some(g), .. }) = self.entries.remove(&n) {
                p.withdraw(&g).await;
            }
        }
        let missing: Vec<(String, IpAddr)> = self
            .desired
            .iter()
            .filter(|(n, _)| !self.entries.contains_key(*n))
            .map(|(n, a)| (n.clone(), *a))
            .collect();
        for (name, addr) in missing {
            let (group, state) = match p.publish(&name, addr).await {
                Ok(g) => (Some(g), NameState::Registering),
                Err(PublishError::Collision(m)) => {
                    tracing::warn!("web access: {name} not published: {m}");
                    (None, NameState::Collision(m))
                }
                Err(PublishError::Failed(m)) => {
                    tracing::warn!("web access: {name} not published: {m}");
                    (None, NameState::Failed(m))
                }
                Err(PublishError::Gone(m)) => {
                    tracing::warn!("web access: Avahi does not answer: {m}");
                    self.withdraw_all(p).await;
                    self.avahi = Availability::Down(m);
                    return;
                }
            };
            self.entries.insert(name, Entry { addr, group, state });
        }
    }

    /// Forgets collided and failed names and publishes them again.
    pub async fn retry<P: Publisher>(&mut self, p: &mut P) {
        self.entries
            .retain(|_, e| !matches!(e.state, NameState::Collision(_) | NameState::Failed(_)));
        self.reconcile(p).await;
    }
}

// ---------------------------------------------------------------------------------------------
// the handle and its driver
// ---------------------------------------------------------------------------------------------

/// The service's handle: the desired names go in, the report comes out. The driver runs
/// only after [`LocalNames::spawn`] (never under `--dry-system` or in tests).
pub struct LocalNames {
    desired: watch::Sender<BTreeMap<String, IpAddr>>,
    report: Arc<Mutex<NamesReport>>,
    retry: Arc<Notify>,
    started: AtomicBool,
}

impl Default for LocalNames {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalNames {
    pub fn new() -> Self {
        Self {
            desired: watch::Sender::new(BTreeMap::new()),
            report: Arc::new(Mutex::new(NamesReport::default())),
            retry: Arc::new(Notify::new()),
            started: AtomicBool::new(false),
        }
    }

    /// The names to publish now (the driver follows; identical sets wake nothing).
    pub fn set(&self, records: BTreeMap<String, IpAddr>) {
        self.desired.send_if_modified(|cur| {
            if *cur == records {
                false
            } else {
                *cur = records;
                true
            }
        });
    }

    pub fn desired(&self) -> BTreeMap<String, IpAddr> {
        self.desired.borrow().clone()
    }

    pub fn report(&self) -> NamesReport {
        self.report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Tries collided and failed names again.
    pub fn retry(&self) {
        self.retry.notify_one();
    }

    /// Starts the driver (once; inside the tokio runtime).
    pub fn spawn(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let rx = self.desired.subscribe();
        let report = self.report.clone();
        let retry = self.retry.clone();
        tokio::spawn(drive(rx, report, retry));
    }
}

fn store(report: &Mutex<NamesReport>, names: &Names) {
    *report.lock().unwrap_or_else(|e| e.into_inner()) = names.report();
}

/// Connects to the system bus and runs sessions; reconnects with backoff.
async fn drive(
    mut rx: watch::Receiver<BTreeMap<String, IpAddr>>,
    report: Arc<Mutex<NamesReport>>,
    retry: Arc<Notify>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut names = Names::new();
        match zbus::Connection::system().await {
            Err(e) => names.on_down(format!("system bus: {e}")),
            Ok(conn) => {
                backoff = Duration::from_secs(1);
                let r = session(&conn, &mut names, &mut rx, &report, &retry).await;
                match r {
                    Ok(()) => return, // the service is gone
                    Err(e) => names.on_down(format!("system bus: {e}")),
                }
            }
        }
        tracing::warn!("web access: local names unavailable, retrying in {backoff:?}");
        store(&report, &names);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn session(
    conn: &zbus::Connection,
    names: &mut Names,
    rx: &mut watch::Receiver<BTreeMap<String, IpAddr>>,
    report: &Mutex<NamesReport>,
    retry: &Notify,
) -> zbus::Result<()> {
    use futures_util::StreamExt;
    use zbus::message::Type;

    let rule = |b: zbus::match_rule::Builder<'static>| b.msg_type(Type::Signal).build();
    let owners = rule(
        zbus::MatchRule::builder()
            .sender("org.freedesktop.DBus")
            .interface("org.freedesktop.DBus")
            .member("NameOwnerChanged")
            .arg(0, AVAHI),
    )?;
    // only Avahi's own signals: the bus matches the well-known name's current owner, and
    // each message's sender is compared with the owner again (review 2026-11-07, M5: any
    // local process could otherwise fake a COLLISION and take a name down)
    let groups = rule(
        zbus::MatchRule::builder()
            .sender(AVAHI)
            .interface(ENTRY_GROUP)
            .member("StateChanged"),
    )?;
    let servers = rule(
        zbus::MatchRule::builder()
            .sender(AVAHI)
            .interface(SERVER)
            .member("StateChanged"),
    )?;
    let mut owners = zbus::MessageStream::for_match_rule(owners, conn, None).await?;
    let mut groups = zbus::MessageStream::for_match_rule(groups, conn, None).await?;
    let mut servers = zbus::MessageStream::for_match_rule(servers, conn, None).await?;
    let mut p = ZbusPublisher { conn: conn.clone() };

    names.set_desired(rx.borrow_and_update().clone());
    let mut owner = name_owner(conn).await?;
    let present = owner.is_some();
    names.on_owner(present);
    if present {
        on_avahi_up(conn, names, &mut p).await;
    }
    store(report, names);
    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                names.set_desired(rx.borrow_and_update().clone());
                names.reconcile(&mut p).await;
            }
            () = retry.notified() => names.retry(&mut p).await,
            m = owners.next() => {
                let m = m.ok_or(zbus::Error::Failure("the bus closed".into()))??;
                let (_name, _old, new): (String, String, String) = m.body().deserialize()?;
                let present = !new.is_empty();
                owner = present.then_some(new);
                if present {
                    tracing::info!("web access: Avahi appeared on the bus, publishing the local names");
                } else {
                    tracing::warn!("web access: Avahi left the bus, local names are unavailable");
                }
                names.on_owner(present);
                if present {
                    on_avahi_up(conn, names, &mut p).await;
                }
            }
            m = groups.next() => {
                let m = m.ok_or(zbus::Error::Failure("the bus closed".into()))??;
                let path = m.header().path().map(|p| p.to_string()).unwrap_or_default();
                let sender = m.header().sender().map(|s| s.to_string());
                if from_owner(sender.as_deref(), owner.as_deref())
                    && let Ok((state, error)) = m.body().deserialize::<(i32, String)>()
                {
                    names.on_group_state(&path, state, &error, &mut p).await;
                }
            }
            m = servers.next() => {
                let m = m.ok_or(zbus::Error::Failure("the bus closed".into()))??;
                let sender = m.header().sender().map(|s| s.to_string());
                if from_owner(sender.as_deref(), owner.as_deref())
                    && let Ok((state, _)) = m.body().deserialize::<(i32, String)>()
                    && !matches!(names.avahi, Availability::Down(_))
                {
                    names.on_server_state(state, &mut p).await;
                }
            }
        }
        store(report, names);
    }
}

/// The unique bus name owning Avahi's well-known name (`None`: not on the bus).
async fn name_owner(conn: &zbus::Connection) -> zbus::Result<Option<String>> {
    let p = zbus::Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    match p.call::<_, _, String>("GetNameOwner", &(AVAHI,)).await {
        Ok(o) => Ok(Some(o)),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// A signal counts only when its sender is Avahi's current owner.
fn from_owner(sender: Option<&str>, owner: Option<&str>) -> bool {
    matches!((sender, owner), (Some(s), Some(o)) if s == o)
}

/// Avahi is on the bus: its server state decides when to publish.
async fn on_avahi_up(conn: &zbus::Connection, names: &mut Names, p: &mut ZbusPublisher) {
    let state = async {
        let s = zbus::Proxy::new(conn, AVAHI, "/", SERVER).await?;
        s.call::<_, _, i32>("GetState", &()).await
    };
    match tokio::time::timeout(CALL_TIMEOUT, state).await {
        Ok(Ok(state)) => names.on_server_state(state, p).await,
        Ok(Err(e)) => names.on_down(format!("Avahi: {e}")),
        Err(_) => names.on_down("Avahi did not answer".into()),
    }
}

/// [`Publisher`] over the system bus.
struct ZbusPublisher {
    conn: zbus::Connection,
}

fn classify(e: zbus::Error) -> PublishError {
    match e {
        zbus::Error::MethodError(name, detail, _) => {
            let msg = detail.unwrap_or_else(|| name.to_string());
            if name.as_str().starts_with("org.freedesktop.DBus.Error.") {
                PublishError::Gone(msg)
            } else if name.as_str().contains("Collision") || msg.contains("ollision") {
                PublishError::Collision(msg)
            } else {
                PublishError::Failed(msg)
            }
        }
        e => PublishError::Gone(e.to_string()),
    }
}

impl ZbusPublisher {
    async fn try_publish(&self, name: &str, addr: IpAddr) -> Result<String, PublishError> {
        let server = zbus::Proxy::new(&self.conn, AVAHI, "/", SERVER)
            .await
            .map_err(classify)?;
        let path: zbus::OwnedObjectPath =
            server.call("EntryGroupNew", &()).await.map_err(classify)?;
        let group = zbus::Proxy::new(&self.conn, AVAHI, path.clone(), ENTRY_GROUP)
            .await
            .map_err(classify)?;
        let r = async {
            group
                .call::<_, _, ()>(
                    "AddAddress",
                    &(
                        AVAHI_IF_UNSPEC,
                        AVAHI_PROTO_UNSPEC,
                        AVAHI_PUBLISH_NO_REVERSE,
                        name,
                        addr.to_string(),
                    ),
                )
                .await?;
            group.call::<_, _, ()>("Commit", &()).await
        }
        .await;
        match r {
            Ok(()) => Ok(path.to_string()),
            Err(e) => {
                let _ = group.call::<_, _, ()>("Free", &()).await;
                Err(classify(e))
            }
        }
    }
}

impl Publisher for ZbusPublisher {
    async fn publish(&mut self, name: &str, addr: IpAddr) -> Result<String, PublishError> {
        match tokio::time::timeout(CALL_TIMEOUT, self.try_publish(name, addr)).await {
            Ok(r) => r,
            Err(_) => Err(PublishError::Gone("Avahi did not answer".into())),
        }
    }

    async fn withdraw(&mut self, group: &str) {
        let free = async {
            let g = zbus::Proxy::new(&self.conn, AVAHI, group.to_string(), ENTRY_GROUP).await?;
            g.call::<_, _, ()>("Free", &()).await
        };
        if let Ok(Err(e)) = tokio::time::timeout(CALL_TIMEOUT, free).await {
            tracing::debug!("web access: freeing {group}: {e}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// migration from the hosts file
// ---------------------------------------------------------------------------------------------

/// `text` without rynas's old `web-access` block (`# BEGIN rynas web-access …` to
/// `# END rynas web-access`); `None` when there is none.
pub fn strip_hosts_block(text: &str) -> Option<String> {
    let begin = "# BEGIN rynas web-access ";
    let end = "# END rynas web-access";
    if !text.lines().any(|l| l.starts_with(begin)) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.lines() {
        if inside {
            if line.trim_end() == end {
                inside = false;
            }
            continue;
        }
        if line.starts_with(begin) {
            inside = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Review 2026-11-07 M5: signals count only from Avahi's current owner.
    #[test]
    fn signals_only_from_avahis_owner() {
        assert!(from_owner(Some(":1.7"), Some(":1.7")));
        assert!(!from_owner(Some(":1.99"), Some(":1.7")), "another process");
        assert!(!from_owner(None, Some(":1.7")));
        assert!(!from_owner(Some(":1.7"), None), "Avahi not on the bus");
    }

    #[derive(Default)]
    struct Fake {
        next: u32,
        live: BTreeSet<String>,
        published: Vec<(String, IpAddr)>,
        withdrawn: Vec<String>,
        collide: BTreeSet<String>,
        gone: bool,
    }

    impl Publisher for Fake {
        async fn publish(&mut self, name: &str, addr: IpAddr) -> Result<String, PublishError> {
            if self.gone {
                return Err(PublishError::Gone("no reply".into()));
            }
            if self.collide.contains(name) {
                return Err(PublishError::Collision("Local name collision".into()));
            }
            self.next += 1;
            let g = format!("/Client1/EntryGroup{}", self.next);
            self.live.insert(g.clone());
            self.published.push((name.into(), addr));
            Ok(g)
        }
        async fn withdraw(&mut self, group: &str) {
            self.live.remove(group);
            self.withdrawn.push(group.into());
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn want(pairs: &[(&str, &str)]) -> BTreeMap<String, IpAddr> {
        pairs.iter().map(|(n, a)| (n.to_string(), ip(a))).collect()
    }

    fn group_of(n: &Names, name: &str) -> String {
        n.entries[name].group.clone().unwrap()
    }

    async fn running(p: &mut Fake) -> Names {
        let mut n = Names::new();
        n.set_desired(want(&[
            ("immich-nas.local", "192.0.2.10"),
            ("kuma-nas.local", "192.0.2.10"),
        ]));
        n.on_owner(true);
        n.on_server_state(SERVER_RUNNING, p).await;
        n
    }

    #[tokio::test]
    async fn publishes_each_name_in_its_own_group_once_running() {
        let mut p = Fake::default();
        let mut n = Names::new();
        n.set_desired(want(&[("immich-nas.local", "192.0.2.10")]));
        // nothing before Avahi runs
        n.reconcile(&mut p).await;
        n.on_owner(true);
        n.on_server_state(1, &mut p).await;
        assert!(p.published.is_empty());
        assert_eq!(n.report().avahi, Availability::Starting);
        n.on_server_state(SERVER_RUNNING, &mut p).await;
        assert_eq!(
            p.published,
            vec![("immich-nas.local".into(), ip("192.0.2.10"))]
        );
        let g = group_of(&n, "immich-nas.local");
        n.on_group_state(&g, GROUP_ESTABLISHED, "", &mut p).await;
        let r = n.report();
        assert_eq!(r.avahi, Availability::Up);
        assert_eq!(r.names["immich-nas.local"].1, NameState::Established);
        assert_eq!(r.problem("immich-nas.local"), None);
        // the same set again: nothing
        n.reconcile(&mut p).await;
        assert_eq!(p.published.len(), 1);
    }

    #[tokio::test]
    async fn follows_address_and_route_changes() {
        let mut p = Fake::default();
        let mut n = running(&mut p).await;
        assert_eq!(p.live.len(), 2);
        let old = group_of(&n, "immich-nas.local");
        // the address moved and one route went away
        n.set_desired(want(&[("immich-nas.local", "192.0.2.20")]));
        n.reconcile(&mut p).await;
        assert!(p.withdrawn.contains(&old));
        assert_eq!(p.live.len(), 1);
        assert_eq!(
            p.published.last().unwrap(),
            &("immich-nas.local".to_string(), ip("192.0.2.20"))
        );
        // every route removed: nothing left in Avahi
        n.set_desired(BTreeMap::new());
        n.reconcile(&mut p).await;
        assert!(p.live.is_empty());
        assert!(n.report().names.is_empty());
    }

    #[tokio::test]
    async fn a_collision_takes_only_its_name_down_and_is_not_retried() {
        let mut p = Fake::default();
        p.collide.insert("kuma-nas.local".into());
        let mut n = running(&mut p).await;
        let r = n.report();
        assert!(matches!(
            r.names["kuma-nas.local"].1,
            NameState::Collision(_)
        ));
        assert_eq!(r.problem("kuma-nas.local"), Some("name_collision"));
        assert_eq!(r.names["immich-nas.local"].1, NameState::Registering);
        // a remote collision found while probing
        let g = group_of(&n, "immich-nas.local");
        n.on_group_state(&g, GROUP_COLLISION, "", &mut p).await;
        assert!(p.withdrawn.contains(&g));
        assert_eq!(
            n.report().problem("immich-nas.local"),
            Some("name_collision")
        );
        // no loop: the next reconcile leaves both alone
        let before = p.published.len();
        n.reconcile(&mut p).await;
        n.reconcile(&mut p).await;
        assert_eq!(p.published.len(), before);
        // an explicit retry, once the other device is gone
        p.collide.clear();
        n.retry(&mut p).await;
        assert_eq!(p.published.len(), before + 2);
        assert_eq!(n.report().problem("kuma-nas.local"), None);
    }

    #[tokio::test]
    async fn avahi_restart_and_server_reregistration_republish() {
        let mut p = Fake::default();
        let mut n = running(&mut p).await;
        // the daemon left: its groups died with it (nothing to free)
        p.live.clear();
        let freed = p.withdrawn.len();
        n.on_owner(false);
        assert_eq!(p.withdrawn.len(), freed);
        let r = n.report();
        assert!(matches!(r.avahi, Availability::Down(_)));
        assert_eq!(r.problem("immich-nas.local"), Some("name_unpublished"));
        n.reconcile(&mut p).await;
        assert_eq!(p.published.len(), 2);
        // back: published again once running
        n.on_owner(true);
        n.on_server_state(SERVER_RUNNING, &mut p).await;
        assert_eq!(p.published.len(), 4);
        // the host name re-registers: groups reset, then added again
        let live = p.live.clone();
        n.on_server_state(1, &mut p).await;
        assert!(live.iter().all(|g| p.withdrawn.contains(g)));
        n.on_server_state(SERVER_RUNNING, &mut p).await;
        assert_eq!(p.published.len(), 6);
        assert_eq!(p.live.len(), 2);
    }

    #[tokio::test]
    async fn avahi_not_answering_is_reported() {
        let mut p = Fake {
            gone: true,
            ..Default::default()
        };
        let n = running(&mut p).await;
        assert!(matches!(n.report().avahi, Availability::Down(_)));
        assert!(n.report().names.is_empty());
    }

    #[test]
    fn handle_keeps_the_latest_set() {
        let h = LocalNames::new();
        h.set(want(&[("a-nas.local", "192.0.2.10")]));
        h.set(want(&[("a-nas.local", "192.0.2.10")]));
        assert_eq!(h.desired().len(), 1);
        assert_eq!(h.report().avahi, Availability::Unknown);
        assert_eq!(h.report().problem("a-nas.local"), None);
    }

    #[test]
    fn old_hosts_block_is_stripped() {
        let text = "# admin\n192.0.2.99 printer.local\n# BEGIN rynas web-access — managed by rynas, do not edit (sha256:ab)\n192.0.2.10 kuma-nas.local\n# END rynas web-access\n10.0.0.1 x.local\n";
        assert_eq!(
            strip_hosts_block(text).unwrap(),
            "# admin\n192.0.2.99 printer.local\n10.0.0.1 x.local\n"
        );
        assert_eq!(strip_hosts_block("192.0.2.99 printer.local\n"), None);
    }

    /// Against the real system bus and Avahi (`cargo test -- --ignored real_avahi`): publishes
    /// `rynas-test-<pid>.local` on 192.0.2.1 for a few seconds.
    #[tokio::test]
    #[ignore = "needs avahi-daemon on the system bus"]
    async fn real_avahi() {
        let conn = zbus::Connection::system().await.unwrap();
        let mut p = ZbusPublisher { conn: conn.clone() };
        let name = format!("rynas-test-{}.local", std::process::id());
        let g = p.publish(&name, ip("192.0.2.1")).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let grp = zbus::Proxy::new(&conn, AVAHI, g.clone(), ENTRY_GROUP)
            .await
            .unwrap();
        let state: i32 = grp.call("GetState", &()).await.unwrap();
        assert_eq!(state, GROUP_ESTABLISHED);
        p.withdraw(&g).await;
    }
}
