//! HTTPS certificate of the web interface (wave W0, D-PROXY: `rynas-web` keeps its own port
//! `:8443` with rustls, never routed through the reverse proxy, and reuses the active
//! certificate).
//!
//! rynasd is the only writer. Its private store, `<state>/tls/` (0700 root), holds:
//! - `ca.crt`, `ca.key`: the local CA ([`rynas_tls::generate_ca`]), made once;
//! - `local.crt`, `local.key`: the leaf it issued, re-issued when the host name or an
//!   address is missing from it or 30 days before it expires ([`rynas_tls::needs_reissue`]);
//! - `imported.crt`, `imported.key`: the administrator's certificate, when imported;
//! - `active`: `local` or `imported`;
//! - `nas-id`: this NAS' identity for the PC agents (32 hex digits, made once);
//! - `agent.crt`, `agent.key`: the PC agents' leaf, issued by the local CA for the single
//!   name `<nas-id>.rynas.invalid` (whatever is active for the interface), re-issued with a
//!   new CA or before it expires, never for a host name or an address;
//! - `ca-next.crt`, `ca-next.key`: the next local CA, made ahead and announced to the agents
//!   in the last two years of the current one ([`TlsSvc::agent_trust`]);
//! - `retired/`: CA files replaced by a rotation (never deleted).
//!
//! The active pair is published for `rynas-web` in `/etc/rynas/tls/` (0750 root:rynas):
//! `ui.crt` (chain, 0644), `ui.key` (0640 root:rynas), `agent.crt` / `agent.key` (same
//! modes; served only to clients asking for the agents' name) and `ca.crt` (0644). rynas-web
//! reloads them without a restart. The key is written before the chain, each file atomically
//! (temporary file + rename), and rynas-web only takes a pair whose key matches.
//!
//! Why `/etc/rynas/tls` and not `/var/lib/rynas`: systemd resets `StateDirectory=` to the
//! unit's `StateDirectoryMode=0700 root` at every start of rynasd, so rynas-web (user rynas)
//! cannot enter `/var/lib/rynas`. `/etc` is where Cockpit (`/etc/cockpit/ws-certs.d`) and
//! Proxmox (`/etc/pve/local/pveproxy-ssl.pem`) keep their UI certificates too.
//!
//! The future certificate store of the web-access module (W1/W2: ACME wildcard, Caddy) adds
//! sources here and publishes through the same files.

use std::collections::BTreeSet;
use std::io;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rynas_api_types::error_codes;
use rynas_api_types::pcagent::AgentTrust;
use rynas_api_types::tls::{TlsImportRequest, TlsSource, TlsStatus};
use rynas_tls::{Names, PemPair};

use crate::error::Error;

type Res<T> = Result<T, Error>;

/// Where the active pair is published for rynas-web, below the configuration root.
pub const PUBLISH_DIR: &str = "etc/rynas/tls";

/// How often the names (host name, addresses) are compared with the local leaf.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(120);

/// Interfaces whose addresses never go into the certificate: container and VM bridges, veth
/// pairs (they come and go with every Docker network and would re-issue the leaf for nothing).
const SKIPPED_IFACES: &[&str] = &[
    "docker", "br-", "veth", "virbr", "lxcbr", "lxdbr", "cni", "podman", "flannel", "cali", "vnet",
    "kube",
];

/// How long the local certificate keeps the names of the previous host name after a rename
/// (the next issue after that leaves them out).
pub const FORMER_NAMES_MS: i64 = 86_400_000;

const DAY_MS: i64 = 86_400_000;
/// The local CA is replaced when it has less than this left (it is made for
/// [`rynas_tls::CA_DAYS`]).
pub const CA_SWITCH_DAYS: i64 = 365;
/// From this long before its end the next CA is made and announced to the PC agents.
pub const CA_ANNOUNCE_DAYS: i64 = 2 * 365;

/// `<store>/host`: the host name the local certificate was last issued for, and the one
/// before it while its names are kept ([`FORMER_NAMES_MS`]).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct HostRecord {
    host: String,
    #[serde(default)]
    former: String,
    #[serde(default)]
    former_until_ms: i64,
}

/// The names probe: host name and interface addresses.
pub type Probe = Box<dyn Fn() -> (String, Vec<IpAddr>) + Send + Sync>;

/// Whether the system clock is synchronized (NTP).
pub type ClockCheck = Box<dyn Fn() -> bool + Send + Sync>;

pub struct TlsSvc {
    store: PathBuf,
    publish: PathBuf,
    /// Group that may read the published key (`rynas`), when rynasd runs as root.
    gid: Option<u32>,
    probe: Probe,
    /// Whether the system clock is synchronized: asked only before a CA rotation.
    synced: ClockCheck,
    lock: Mutex<()>,
}

pub type TlsState = Option<Arc<TlsSvc>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Active {
    Local,
    Imported,
}

impl TlsSvc {
    /// `store`: the private store (`<state>/tls`); `publish`: the folder rynas-web reads
    /// (`<root>/etc/rynas/tls`); `group`: owner group of the published key (only applied as
    /// root).
    pub fn new(store: PathBuf, publish: PathBuf, group: Option<&str>, probe: Probe) -> Arc<Self> {
        Self::with_clock(store, publish, group, probe, Self::system_clock())
    }

    /// [`Self::new`] with the clock check `synced` (tests).
    pub fn with_clock(
        store: PathBuf,
        publish: PathBuf,
        group: Option<&str>,
        probe: Probe,
        synced: ClockCheck,
    ) -> Arc<Self> {
        let gid = group
            .filter(|_| nix::unistd::Uid::effective().is_root())
            .and_then(|g| nix::unistd::Group::from_name(g).ok().flatten())
            .map(|g| g.gid.as_raw());
        Arc::new(Self {
            store,
            publish,
            gid,
            probe,
            synced,
            lock: Mutex::new(()),
        })
    }

    /// The running system's clock check: what the Date and time page shows as synchronized
    /// (timedated's NTPSynchronized, chrony's leap status).
    pub fn system_clock() -> ClockCheck {
        Box::new(|| crate::clock::Clock::new(PathBuf::from("/"), false).state().synchronized)
    }

    /// The probe of the running system: kernel host name, `ip -j addr show`.
    pub fn system_probe() -> Probe {
        Box::new(|| {
            let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let addrs = std::process::Command::new("ip")
                .args(["-j", "addr", "show"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| parse_ip_addrs(&String::from_utf8_lossy(&o.stdout)))
                .unwrap_or_default();
            (host, addrs)
        })
    }

    fn names(&self) -> Names {
        let (host, addrs) = (self.probe)();
        rynas_tls::names(&host, &addrs)
    }

    /// The DNS names of the previous host name for [`FORMER_NAMES_MS`] after a rename, kept
    /// in the local certificate: the page opened at the old name (still cached by browsers
    /// and resolvers, still in the address bar) keeps a valid certificate while it moves to
    /// the new name. The host the names were last issued for is kept in `<store>/host`.
    fn former_names(&self, host: &str, now: i64) -> io::Result<BTreeSet<String>> {
        let p = self.path("host");
        let mut rec: HostRecord = std::fs::read_to_string(&p)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if rec.host != host {
            if !rec.host.is_empty() {
                rec.former = std::mem::take(&mut rec.host);
                rec.former_until_ms = now + FORMER_NAMES_MS;
            }
            rec.host = host.to_string();
            let json = serde_json::to_vec(&rec).map_err(io::Error::other)?;
            write_atomic(&p, &json, 0o600, None)?;
        }
        if rec.former.is_empty() || rec.former == host || rec.former_until_ms <= now {
            return Ok(BTreeSet::new());
        }
        Ok(rynas_tls::names(&rec.former, &[])
            .dns
            .into_iter()
            .filter(|d| d != "localhost")
            .collect())
    }

    fn path(&self, name: &str) -> PathBuf {
        self.store.join(name)
    }

    fn read_pair(&self, stem: &str) -> Option<PemPair> {
        let chain = std::fs::read_to_string(self.path(&format!("{stem}.crt"))).ok()?;
        let key = std::fs::read_to_string(self.path(&format!("{stem}.key"))).ok()?;
        Some(PemPair { chain, key })
    }

    fn write_pair(&self, stem: &str, p: &PemPair) -> io::Result<()> {
        write_atomic(
            &self.path(&format!("{stem}.key")),
            p.key.as_bytes(),
            0o600,
            None,
        )?;
        write_atomic(
            &self.path(&format!("{stem}.crt")),
            p.chain.as_bytes(),
            0o600,
            None,
        )
    }

    fn active(&self) -> Active {
        match std::fs::read_to_string(self.path("active")) {
            Ok(s) if s.trim() == "imported" => Active::Imported,
            _ => Active::Local,
        }
    }

    fn set_active(&self, a: Active) -> io::Result<()> {
        let v = match a {
            Active::Local => "local\n",
            Active::Imported => "imported\n",
        };
        write_atomic(&self.path("active"), v.as_bytes(), 0o600, None)
    }

    fn dirs(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.store)?;
        std::fs::set_permissions(&self.store, std::fs::Permissions::from_mode(0o700))?;
        std::fs::create_dir_all(&self.publish)?;
        // `/etc/rynas` may have been made under rynasd's UMask=0077 (btrbk configurations):
        // rynas-web must be able to pass through it
        if let Some(parent) = self.publish.parent() {
            let m = std::fs::metadata(parent)?.permissions().mode() & 0o7777;
            if m & 0o011 != 0o011 {
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(m | 0o711))?;
            }
        }
        std::fs::set_permissions(&self.publish, std::fs::Permissions::from_mode(0o750))?;
        if let Some(g) = self.gid {
            chown_group(&self.publish, g)?;
        }
        Ok(())
    }

    /// The local CA: the stored one while it has more than [`CA_SWITCH_DAYS`] left, else the
    /// next one. From [`CA_ANNOUNCE_DAYS`] before its end a next CA is made ahead
    /// (`ca-next.*`) and announced to the PC agents ([`Self::agent_trust`]); the switch takes
    /// that one, so an agent that came by in that year already trusts it. An agent away for
    /// longer is refused with a clear error and is paired again.
    ///
    /// A stored CA is never replaced because of a wrong clock: one that looks not yet valid
    /// (a clock behind: an RTC reset before NTP) is kept, and the next CA and the switch wait
    /// for a synchronized clock. Replaced CA files are kept in `retired/`, never deleted.
    fn ca(&self, host: &str, now: i64) -> io::Result<(PemPair, bool)> {
        let first = |p: &PemPair| -> Option<rynas_api_types::tls::TlsCertificate> {
            rynas_tls::key_der(&p.key).ok()?;
            rynas_tls::inspect(&p.chain).ok()?.into_iter().next()
        };
        let left_more_than = |p: &PemPair, days: i64| first(p).is_some_and(|i| i.not_after_ms - now > days * DAY_MS);
        if let Some(p) = self.read_pair("ca")
            && let Some(info) = first(&p)
        {
            // A CA that looks not yet valid means a clock behind (an RTC reset before NTP),
            // never a reason to replace it: every agent would have to pair again.
            if info.not_before_ms > now {
                tracing::warn!(
                    "tls: the local CA starts in the future: the clock is behind, keeping it"
                );
                return Ok((p, false));
            }
            if left_more_than(&p, CA_ANNOUNCE_DAYS) {
                return Ok((p, false));
            }
            // Rotation only with a synchronized clock: one ahead would rotate early (and a
            // CA made then would start in the future for the right time).
            if !(self.synced)() {
                tracing::warn!(
                    "tls: the local CA ends in {} days, but the clock is not synchronized: no rotation yet",
                    (info.not_after_ms - now) / DAY_MS
                );
                return Ok((p, false));
            }
            if left_more_than(&p, CA_SWITCH_DAYS) {
                if self
                    .read_pair("ca-next")
                    .is_none_or(|n| !left_more_than(&n, CA_ANNOUNCE_DAYS))
                {
                    self.retire("ca-next", now)?;
                    let next = rynas_tls::generate_ca(host, now).map_err(io::Error::other)?;
                    self.write_pair("ca-next", &next)?;
                    tracing::info!(
                        "tls: next local CA {} made ahead of the switch",
                        rynas_tls::inspect(&next.chain)
                            .map(|i| i[0].fingerprint_sha256.clone())
                            .unwrap_or_default()
                    );
                }
                return Ok((p, false));
            }
        }
        // the switch (less than CA_SWITCH_DAYS left, by a synchronized clock), or no usable CA
        // at all (first start): the announced next CA when there is one
        let ca = match self
            .read_pair("ca-next")
            .filter(|n| left_more_than(n, CA_SWITCH_DAYS))
        {
            Some(next) => next,
            None => {
                self.retire("ca-next", now)?;
                rynas_tls::generate_ca(host, now).map_err(io::Error::other)?
            }
        };
        self.retire("ca", now)?;
        self.write_pair("ca", &ca)?;
        let _ = std::fs::remove_file(self.path("ca-next.crt"));
        let _ = std::fs::remove_file(self.path("ca-next.key"));
        tracing::info!(
            "tls: new local CA {}",
            rynas_tls::inspect(&ca.chain)
                .map(|i| i[0].fingerprint_sha256.clone())
                .unwrap_or_default()
        );
        Ok((ca, true))
    }

    /// Moves `<stem>.crt` / `<stem>.key`, when there, to `retired/<stem>-<now>.*` in the
    /// store: a CA key is never deleted (a rotation or a mistake can be undone by hand).
    fn retire(&self, stem: &str, now: i64) -> io::Result<()> {
        let dir = self.path("retired");
        for ext in ["key", "crt"] {
            let from = self.path(&format!("{stem}.{ext}"));
            if std::fs::symlink_metadata(&from).is_err() {
                continue;
            }
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::rename(&from, dir.join(format!("{stem}-{now}.{ext}")))?;
        }
        Ok(())
    }

    /// This NAS' identity for its PC agents (`<store>/nas-id`, 32 hex digits, made once and
    /// never changed: it outlives CA rotations and host names). Call with the lock held.
    fn nas_id_locked(&self) -> io::Result<String> {
        let p = self.path("nas-id");
        if let Ok(s) = std::fs::read_to_string(&p)
            && rynas_api_types::pcagent::valid_nas_id(s.trim())
        {
            return Ok(s.trim().to_string());
        }
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|e| io::Error::other(e.to_string()))?;
        let id: String = b.iter().map(|x| format!("{x:02x}")).collect();
        write_atomic(&p, format!("{id}\n").as_bytes(), 0o600, None)?;
        Ok(id)
    }

    /// The stored leaf `stem` when it can be served as it is: readable, its key matching,
    /// issued by `ca`, covering `want` and not about to expire.
    fn current_leaf(&self, stem: &str, ca: &PemPair, want: &Names, now: i64) -> Option<PemPair> {
        let p = self.read_pair(stem)?;
        let info = rynas_tls::inspect(&p.chain).ok()?;
        let ca_fp = rynas_tls::inspect(&ca.chain).ok()?.remove(0).fingerprint_sha256;
        (rynas_tls::certified_key(&p).is_ok()
            && info.get(1).is_some_and(|c| c.fingerprint_sha256 == ca_fp)
            && !rynas_tls::needs_reissue(&info[0], want, now))
        .then_some(p)
    }

    /// What PC agents trust of this NAS ([`AgentTrust`]): the identity, the local CA (and the
    /// next one ahead of a switch), and the addresses of the HTTPS port `port`: the host name,
    /// its mDNS name, then the interface addresses (no loopback).
    pub fn agent_trust(&self, port: u16) -> io::Result<AgentTrust> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let nas_id = self.nas_id_locked()?;
        let mut cas = Vec::new();
        for stem in ["ca", "ca-next"] {
            if let Some(p) = self.read_pair(stem)
                && rynas_tls::chain_der(&p.chain).is_ok_and(|d| d.len() == 1)
            {
                cas.push(p.chain);
            }
        }
        if cas.is_empty() {
            return Err(io::Error::other("no local CA yet"));
        }
        let (host, addrs) = (self.probe)();
        Ok(AgentTrust {
            nas_id,
            cas,
            addresses: agent_addresses(&host, &addrs, port),
        })
    }

    /// Makes sure a valid pair is active and published: creates the local CA and leaf on the
    /// first start, re-issues the leaf when names are missing or it is about to expire, falls
    /// back to the local leaf when the imported pair is unreadable. `force_local` makes the
    /// local leaf active, `reissue` issues a new one in any case. Returns whether the published
    /// files changed.
    pub fn ensure(&self, force_local: bool, reissue: bool) -> io::Result<bool> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.dirs()?;
        let now = rynas_tls::now();
        let (probed, addrs) = (self.probe)();
        let names = rynas_tls::names(&probed, &addrs);
        // what the local leaf covers: these names, and the previous host's for a while
        let mut want = names.clone();
        want.dns.extend(self.former_names(&probed, now)?);
        let host = names
            .dns
            .iter()
            .find(|d| *d != "localhost" && !d.ends_with(".local") && !d.contains('.'))
            .cloned()
            .unwrap_or_else(|| "nas".into());
        let (ca, new_ca) = self.ca(&host, now)?;
        let mut active = if force_local {
            Active::Local
        } else {
            self.active()
        };
        let mut pair = None;
        if active == Active::Imported {
            match self.read_pair("imported") {
                Some(p) if rynas_tls::certified_key(&p).is_ok() => pair = Some(p),
                _ => {
                    tracing::warn!(
                        "tls: the imported certificate is unreadable, using the local one"
                    );
                    active = Active::Local;
                }
            }
        }
        let pair = match pair {
            Some(p) => p,
            None => {
                let current = self.current_leaf("local", &ca, &want, now);
                match current {
                    Some(p) if !reissue && !new_ca => p,
                    _ => {
                        let leaf =
                            rynas_tls::issue_leaf(&ca, &want, now).map_err(io::Error::other)?;
                        self.write_pair("local", &leaf)?;
                        tracing::info!("tls: new certificate for {}", want.all().join(", "));
                        leaf
                    }
                }
            }
        };
        // the PC agents' leaf: the NAS identity as its only name, always from the local CA
        // (also while an imported certificate is active), served for that name only (SNI)
        let nas_id = self.nas_id_locked()?;
        let agent_names = Names {
            dns: [rynas_api_types::pcagent::agent_tls_name(&nas_id)].into(),
            ips: BTreeSet::new(),
        };
        let agent = match self.current_leaf("agent", &ca, &agent_names, now) {
            Some(p) if !new_ca => p,
            _ => {
                let leaf =
                    rynas_tls::issue_agent_leaf(&ca, &nas_id, now).map_err(io::Error::other)?;
                self.write_pair("agent", &leaf)?;
                tracing::info!("tls: new certificate for the PC agents");
                leaf
            }
        };
        if active != self.active() || !self.path("active").exists() {
            self.set_active(active)?;
        }
        self.publish(&pair, &agent, &ca)
    }

    /// The local CA (made when missing): the web-access module issues the certificate of
    /// the proxy's local names from it, so one trusted root covers `:8443` and every
    /// `<app>-<host>.local` (W1).
    pub fn local_ca(&self) -> io::Result<PemPair> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.dirs()?;
        let names = self.names();
        let host = names
            .dns
            .iter()
            .find(|d| *d != "localhost" && !d.ends_with(".local") && !d.contains('.'))
            .cloned()
            .unwrap_or_else(|| "nas".into());
        self.ca(&host, rynas_tls::now()).map(|(p, _)| p)
    }

    /// Writes the published files that differ from `pair`, `agent` / `ca`.
    fn publish(&self, pair: &PemPair, agent: &PemPair, ca: &PemPair) -> io::Result<bool> {
        // each key first: rynas-web only takes a pair whose key matches the chain
        let files: [(&str, &str, u32); 5] = [
            ("ui.key", &pair.key, 0o640),
            ("ui.crt", &pair.chain, 0o644),
            ("agent.key", &agent.key, 0o640),
            ("agent.crt", &agent.chain, 0o644),
            ("ca.crt", &ca.chain, 0o644),
        ];
        let mut changed = false;
        for (name, data, mode) in files {
            let p = self.publish.join(name);
            let same = std::fs::read(&p).is_ok_and(|old| old == data.as_bytes());
            if !same {
                write_atomic(&p, data.as_bytes(), mode, self.gid)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    /// The active certificate and its details.
    pub fn status(&self) -> Res<TlsStatus> {
        let source = self.active();
        let stem = match source {
            Active::Local => "local",
            Active::Imported => "imported",
        };
        let pair = self
            .read_pair(stem)
            .ok_or_else(|| Error::not_found("no certificate yet"))?;
        let mut chain = rynas_tls::inspect(&pair.chain).map_err(Error::internal)?;
        let local_ca = self
            .read_pair("ca")
            .and_then(|p| rynas_tls::inspect(&p.chain).ok())
            .map(|mut v| v.remove(0));
        let now = rynas_tls::now();
        let missing = rynas_tls::missing_names(&chain[0], &self.names());
        let warnings = rynas_tls::warnings(&chain, &missing, now);
        let certificate = chain.remove(0);
        Ok(TlsStatus {
            source: match source {
                Active::Local => TlsSource::Local,
                Active::Imported => TlsSource::Imported,
            },
            certificate,
            chain,
            local_ca,
            warnings,
            missing_names: missing,
        })
    }

    /// Checks and activates an administrator's certificate.
    pub fn import(&self, req: &TlsImportRequest) -> Res<TlsStatus> {
        let imp = rynas_tls::validate_import(&req.certificate, &req.private_key, rynas_tls::now())
            .map_err(|e| Error::invalid(e.field, e.message))?;
        {
            let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            self.dirs().map_err(Error::internal)?;
            self.write_pair("imported", &imp.pair)
                .map_err(Error::internal)?;
            self.set_active(Active::Imported).map_err(Error::internal)?;
        }
        tracing::info!(
            "tls: imported certificate {} ({}) is active",
            imp.chain[0].subject,
            imp.chain[0].fingerprint_sha256
        );
        self.ensure(false, false).map_err(Error::internal)?;
        self.status()
    }

    /// Makes the local certificate active again (re-issued when names are missing).
    pub fn use_local(&self, reissue: bool) -> Res<TlsStatus> {
        self.ensure(true, reissue).map_err(Error::internal)?;
        self.status()
    }

    /// The local CA certificate, PEM.
    pub fn ca_pem(&self) -> Option<String> {
        self.read_pair("ca").map(|p| p.chain)
    }
}

/// The group of a folder rynasd made in a root-owned parent (`/etc/rynas/tls`).
fn chown_group(p: &Path, gid: u32) -> io::Result<()> {
    nix::unistd::chown(p, None, Some(nix::unistd::Gid::from_raw(gid))).map_err(io::Error::from)
}

/// Writes `data` to `path` through a temporary file in the same folder and a rename, with
/// `mode` (and group `gid`) set on the descriptor before any byte is written
/// ([`rynas_docker::safefs::write_at`]: `O_EXCL|O_NOFOLLOW`, `fchown`/`fchmod`, `renameat`).
/// The folder itself is trusted (root-owned parents); for a folder another user can write,
/// open it with [`rynas_docker::safefs`] and call `write_at` on it.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32, gid: Option<u32>) -> io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid path"));
    };
    let dir = rynas_docker::safefs::open_start(parent)?;
    // uid u32::MAX (-1): fchown keeps the owner, only the group changes
    rynas_docker::safefs::write_at(&dir, name, data, mode, gid.map(|g| (u32::MAX, g)))
}

/// Addresses of `ip -j addr show` that belong in the certificate: up interfaces except
/// loopback and container bridges, no temporary (privacy) or deprecated IPv6 addresses (they
/// rotate and would re-issue the leaf every day).
pub fn parse_ip_addrs(json: &str) -> Vec<IpAddr> {
    let Ok(serde_json::Value::Array(links)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for l in &links {
        let name = l["ifname"].as_str().unwrap_or("");
        if l["link_type"] == "loopback" || SKIPPED_IFACES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        for a in l["addr_info"].as_array().into_iter().flatten() {
            if a["temporary"] == true || a["deprecated"] == true {
                continue;
            }
            if let Some(ip) = a["local"].as_str().and_then(|s| s.parse::<IpAddr>().ok()) {
                out.push(ip);
            }
        }
    }
    out
}

/// The addresses PC agents may reach the HTTPS port `port` at: the host name (the full one
/// when it has dots, then its first label), its mDNS name, then the interface addresses
/// (IPv4 first; no loopback, no IPv6 link-local).
pub fn agent_addresses(host: &str, addrs: &[IpAddr], port: u16) -> Vec<String> {
    let names = rynas_tls::names(host, addrs);
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let first = host.split('.').next().unwrap_or_default().to_string();
    let mut out: Vec<String> = Vec::new();
    for d in [host.clone(), first.clone(), format!("{first}.local")] {
        if names.dns.contains(&d) && d != "localhost" && !out.contains(&d) {
            out.push(d);
        }
    }
    out.extend(
        names
            .ips
            .iter()
            .filter(|i| !i.is_loopback())
            .map(|i| match i {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{v6}]"),
            }),
    );
    out.into_iter()
        .take(rynas_api_types::pcagent::AGENT_MAX_ADDRESSES)
        .map(|h| format!("https://{h}:{port}"))
        .collect()
}

/// Ensures the certificate at start, then re-checks the names every [`CHECK_INTERVAL`] and
/// at once when an apply renames the host (`hostname`).
pub fn spawn_watch(svc: Arc<TlsSvc>, mut hostname: tokio::sync::watch::Receiver<String>) {
    tokio::spawn(async move {
        loop {
            let s = svc.clone();
            match tokio::task::spawn_blocking(move || s.ensure(false, false)).await {
                Ok(Ok(true)) => tracing::info!("tls: published certificate updated"),
                Ok(Ok(false)) => {}
                Ok(Err(e)) => tracing::error!("tls: {e}"),
                Err(e) => tracing::error!("tls: {e}"),
            }
            // a renamed host gets its certificate at once: the browser is sent to the new name
            tokio::select! {
                _ = tokio::time::sleep(CHECK_INTERVAL) => {}
                r = hostname.changed() => {
                    if r.is_err() {
                        // no more renames can come: back to the plain interval
                        tokio::time::sleep(CHECK_INTERVAL).await;
                    }
                }
            }
        }
    });
}

fn unavailable() -> Error {
    Error::new(
        StatusCode::SERVICE_UNAVAILABLE,
        error_codes::BACKEND_UNAVAILABLE,
        "certificates are not managed by this rynasd".into(),
    )
}

fn svc(st: &TlsState) -> Res<Arc<TlsSvc>> {
    st.clone().ok_or_else(unavailable)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(Error::internal)?
}

async fn get_status(State(st): State<TlsState>) -> Res<Json<TlsStatus>> {
    let s = svc(&st)?;
    blocking(move || s.status()).await.map(Json)
}

/// A line of the audit log.
fn audit(who: &str, action: &str, target: &str) {
    tracing::info!(target: "rynas::audit", "tls {action} {target} by {who}");
}

async fn import(
    State(st): State<TlsState>,
    hd: HeaderMap,
    body: Result<Json<TlsImportRequest>, JsonRejection>,
) -> Res<Json<TlsStatus>> {
    let s = svc(&st)?;
    // the router's gate checked the role; the user is needed for the audit line
    let who = crate::ops::user(&hd)?;
    let Json(req) = body?;
    let st = blocking(move || s.import(&req)).await?;
    audit(
        &who,
        "import",
        &format!(
            "{} ({})",
            st.certificate.subject, st.certificate.fingerprint_sha256
        ),
    );
    Ok(Json(st))
}

async fn local(State(st): State<TlsState>, hd: HeaderMap) -> Res<Json<TlsStatus>> {
    let s = svc(&st)?;
    let who = crate::ops::user(&hd)?;
    let st = blocking(move || s.use_local(false)).await?;
    audit(
        &who,
        "local",
        &format!(
            "{} ({})",
            st.certificate.subject, st.certificate.fingerprint_sha256
        ),
    );
    Ok(Json(st))
}

async fn ca(State(st): State<TlsState>) -> Res<Response> {
    let s = svc(&st)?;
    let pem = blocking(move || {
        s.ca_pem()
            .ok_or_else(|| Error::not_found("no local CA yet"))
    })
    .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-pem-file"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"rynas-local-ca.crt\"",
            ),
        ],
        pem,
    )
        .into_response())
}

/// The routes; every one needs an administrator (`admins`), reads included: replacing the
/// interface certificate is a man-in-the-middle primitive.
pub fn router(st: TlsState, admins: crate::roles::AdminPolicy) -> Router {
    Router::new()
        .route("/v1/system/tls", get(get_status))
        .route("/v1/system/tls/import", post(import))
        .route("/v1/system/tls/local", post(local))
        .route("/v1/system/tls/ca", get(ca))
        .with_state(st)
        .route_layer(axum::middleware::from_fn_with_state(
            crate::roles::AdminGate {
                policy: admins,
                what: "HTTPS certificate",
            },
            crate::roles::admin_only,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc_in(dir: &Path, host: &'static str, addrs: Vec<IpAddr>) -> Arc<TlsSvc> {
        svc_clock(dir, host, addrs, true)
    }

    fn svc_clock(dir: &Path, host: &'static str, addrs: Vec<IpAddr>, synced: bool) -> Arc<TlsSvc> {
        TlsSvc::with_clock(
            dir.join("state/tls"),
            dir.join("etc/rynas/tls"),
            None,
            Box::new(move || (host.to_string(), addrs.clone())),
            Box::new(move || synced),
        )
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn first_start_creates_ca_and_leaf_and_publishes_them() {
        let d = tempfile::tempdir().unwrap();
        let s = svc_in(d.path(), "nas", vec!["192.0.2.10".parse().unwrap()]);
        // /etc/rynas made earlier under UMask=0077 (btrbk configurations)
        std::fs::create_dir_all(d.path().join("etc/rynas")).unwrap();
        std::fs::set_permissions(
            d.path().join("etc/rynas"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(s.ensure(false, false).unwrap());
        assert_eq!(mode(&d.path().join("etc/rynas")), 0o711);
        let pubdir = d.path().join("etc/rynas/tls");
        assert_eq!(mode(&pubdir), 0o750);
        assert_eq!(mode(&pubdir.join("ui.key")), 0o640);
        assert_eq!(mode(&pubdir.join("ui.crt")), 0o644);
        assert_eq!(mode(&d.path().join("state/tls")), 0o700);
        assert_eq!(mode(&d.path().join("state/tls/ca.key")), 0o600);
        let st = s.status().unwrap();
        assert_eq!(st.source, TlsSource::Local);
        assert!(
            st.certificate
                .ip_addresses
                .contains(&"192.0.2.10".to_string())
        );
        assert!(st.certificate.dns_names.contains(&"nas.local".to_string()));
        assert_eq!(st.chain.len(), 1);
        assert_eq!(st.chain[0], st.local_ca.clone().unwrap());
        assert!(st.warnings.is_empty() && st.missing_names.is_empty());
        // nothing to do the second time
        assert!(!s.ensure(false, false).unwrap());
    }

    #[test]
    fn a_new_address_reissues_the_leaf_but_keeps_the_ca() {
        let d = tempfile::tempdir().unwrap();
        svc_in(d.path(), "nas", vec!["192.0.2.10".parse().unwrap()])
            .ensure(false, false)
            .unwrap();
        let ca_before = std::fs::read(d.path().join("etc/rynas/tls/ca.crt")).unwrap();
        let s = svc_in(
            d.path(),
            "nas",
            vec!["192.0.2.10".parse().unwrap(), "192.0.2.20".parse().unwrap()],
        );
        assert!(s.ensure(false, false).unwrap());
        let st = s.status().unwrap();
        assert!(
            st.certificate
                .ip_addresses
                .contains(&"192.0.2.20".to_string())
        );
        assert_eq!(
            std::fs::read(d.path().join("etc/rynas/tls/ca.crt")).unwrap(),
            ca_before
        );
    }

    #[test]
    fn a_renamed_host_keeps_its_old_name_in_the_leaf_for_a_while() {
        let d = tempfile::tempdir().unwrap();
        svc_in(d.path(), "nas", vec![]).ensure(false, false).unwrap();
        let s = svc_in(d.path(), "store", vec![]);
        assert!(s.ensure(false, false).unwrap());
        let st = s.status().unwrap();
        let dns = &st.certificate.dns_names;
        for n in ["store", "store.local", "nas", "nas.local"] {
            assert!(dns.contains(&n.to_string()), "{n} missing from {dns:?}");
        }
        assert!(st.missing_names.is_empty());
        // nothing more to do while the old names are kept
        assert!(!s.ensure(false, false).unwrap());
        // the transition is over: the next issue leaves the old name out
        let rec = d.path().join("state/tls/host");
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&rec).unwrap()).unwrap();
        assert_eq!((v["host"].as_str(), v["former"].as_str()), (Some("store"), Some("nas")));
        v["former_until_ms"] = serde_json::json!(1);
        std::fs::write(&rec, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(!s.ensure(false, false).unwrap(), "an extra name alone is no reason to reissue");
        assert!(s.ensure(false, true).unwrap());
        let dns = s.status().unwrap().certificate.dns_names;
        assert!(dns.contains(&"store.local".to_string()) && !dns.iter().any(|d| d.starts_with("nas")), "{dns:?}");
    }

    #[test]
    fn import_activates_and_local_switches_back() {
        let d = tempfile::tempdir().unwrap();
        let s = svc_in(d.path(), "nas", vec![]);
        s.ensure(false, false).unwrap();
        // an "imported" certificate from another CA
        let now = rynas_tls::now();
        let other = rynas_tls::generate_ca("other", now).unwrap();
        let leaf = rynas_tls::issue_leaf(&other, &rynas_tls::names("nas", &[]), now).unwrap();
        let req = TlsImportRequest {
            certificate: leaf.chain.clone(),
            private_key: leaf.key.clone(),
        };
        let st = s.import(&req).unwrap();
        assert_eq!(st.source, TlsSource::Imported);
        let served = std::fs::read_to_string(d.path().join("etc/rynas/tls/ui.crt")).unwrap();
        assert_eq!(served, leaf.chain);
        // the background check keeps it
        assert!(!s.ensure(false, false).unwrap());
        // a wrong key is refused with the field
        let bad = TlsImportRequest {
            certificate: leaf.chain.clone(),
            private_key: rynas_tls::generate_ca("x", now).unwrap().key,
        };
        let e = s.import(&bad).unwrap_err();
        assert_eq!(e.3.as_deref(), Some("private_key"));
        assert_eq!(s.status().unwrap().source, TlsSource::Imported);
        // back to the local one
        let st = s.use_local(false).unwrap();
        assert_eq!(st.source, TlsSource::Local);
        let served = std::fs::read_to_string(d.path().join("etc/rynas/tls/ui.crt")).unwrap();
        assert_ne!(served, leaf.chain);
    }

    #[test]
    fn ip_json_skips_loopback_bridges_and_temporary_addresses() {
        let json = r#"[
          {"ifname":"lo","link_type":"loopback","addr_info":[{"family":"inet","local":"127.0.0.1"}]},
          {"ifname":"enp1s0","link_type":"ether","addr_info":[
            {"family":"inet","local":"192.0.2.10","dynamic":true},
            {"family":"inet6","local":"2001:db8::10","dynamic":true,"mngtmpaddr":true},
            {"family":"inet6","local":"2001:db8::abcd","temporary":true,"dynamic":true},
            {"family":"inet6","local":"2001:db8::dead","deprecated":true},
            {"family":"inet6","local":"fe80::1","scope":"link"}]},
          {"ifname":"docker0","link_type":"ether","addr_info":[{"family":"inet","local":"172.17.0.1"}]},
          {"ifname":"br-1a2b","link_type":"ether","addr_info":[{"family":"inet","local":"172.18.0.1"}]},
          {"ifname":"tailscale0","link_type":"none","addr_info":[{"family":"inet","local":"100.64.0.5"}]}
        ]"#;
        let got: Vec<String> = parse_ip_addrs(json).iter().map(|i| i.to_string()).collect();
        assert_eq!(got, ["192.0.2.10", "2001:db8::10", "fe80::1", "100.64.0.5"]);
        assert!(parse_ip_addrs("garbage").is_empty());
    }

    #[test]
    fn the_agent_leaf_carries_the_nas_id_and_survives_renames_and_imports() {
        let d = tempfile::tempdir().unwrap();
        let s = svc_in(d.path(), "nas", vec!["192.0.2.10".parse().unwrap()]);
        s.ensure(false, false).unwrap();
        let pubdir = d.path().join("etc/rynas/tls");
        assert_eq!(mode(&pubdir.join("agent.key")), 0o640);
        let t = s.agent_trust(8443).unwrap();
        assert!(rynas_api_types::pcagent::valid_nas_id(&t.nas_id));
        assert_eq!(t.cas.len(), 1);
        assert_eq!(t.cas[0], std::fs::read_to_string(pubdir.join("ca.crt")).unwrap());
        assert_eq!(
            t.addresses,
            ["https://nas:8443", "https://nas.local:8443", "https://192.0.2.10:8443"]
        );
        let agent = std::fs::read_to_string(pubdir.join("agent.crt")).unwrap();
        let info = rynas_tls::inspect(&agent).unwrap();
        assert_eq!(
            info[0].dns_names,
            [rynas_api_types::pcagent::agent_tls_name(&t.nas_id)]
        );
        assert!(info[0].ip_addresses.is_empty());
        assert_eq!(info[1], s.status().unwrap().local_ca.unwrap());
        // a new name and address: the UI leaf changes, the agent leaf and the id do not
        let s = svc_in(d.path(), "store", vec!["192.0.2.99".parse().unwrap()]);
        assert!(s.ensure(false, false).unwrap());
        assert_eq!(std::fs::read_to_string(pubdir.join("agent.crt")).unwrap(), agent);
        let t2 = s.agent_trust(8443).unwrap();
        assert_eq!((t2.nas_id.as_str(), &t2.cas), (t.nas_id.as_str(), &t.cas));
        assert_eq!(t2.addresses[0], "https://store:8443");
        // an imported certificate for the interface: the agent leaf stays the local CA's
        let now = rynas_tls::now();
        let other = rynas_tls::generate_ca("other", now).unwrap();
        let leaf = rynas_tls::issue_leaf(&other, &rynas_tls::names("store", &[]), now).unwrap();
        s.import(&TlsImportRequest {
            certificate: leaf.chain,
            private_key: leaf.key,
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(pubdir.join("agent.crt")).unwrap(), agent);
    }

    #[test]
    fn the_next_ca_is_announced_a_year_ahead_and_taken_at_the_switch() {
        let d = tempfile::tempdir().unwrap();
        let s = svc_in(d.path(), "nas", vec![]);
        s.ensure(false, false).unwrap();
        let store = d.path().join("state/tls");
        // a CA with 500 days left: inside the announce window, not yet at the switch
        let now = rynas_tls::now();
        let old = rynas_tls::generate_ca("nas", now - (rynas_tls::CA_DAYS - 500) * DAY_MS).unwrap();
        std::fs::write(store.join("ca.crt"), &old.chain).unwrap();
        std::fs::write(store.join("ca.key"), &old.key).unwrap();
        s.ensure(false, false).unwrap();
        let t = s.agent_trust(8443).unwrap();
        assert_eq!(t.cas.len(), 2, "current and next");
        assert_eq!(t.cas[0], old.chain);
        let next = t.cas[1].clone();
        // the agent leaf is issued by the current CA until the switch
        let agent = std::fs::read_to_string(d.path().join("etc/rynas/tls/agent.crt")).unwrap();
        assert_eq!(
            rynas_tls::inspect(&agent).unwrap()[1].fingerprint_sha256,
            rynas_tls::inspect(&old.chain).unwrap()[0].fingerprint_sha256
        );
        // nothing new while the next one waits
        assert!(!s.ensure(false, false).unwrap());
        assert_eq!(s.agent_trust(8443).unwrap().cas[1], next);
        // the switch (less than a year left): the announced CA becomes the current one
        let old = rynas_tls::generate_ca("nas", now - (rynas_tls::CA_DAYS - 300) * DAY_MS).unwrap();
        std::fs::write(store.join("ca.crt"), &old.chain).unwrap();
        std::fs::write(store.join("ca.key"), &old.key).unwrap();
        assert!(s.ensure(false, false).unwrap());
        let t = s.agent_trust(8443).unwrap();
        assert_eq!(t.cas, std::slice::from_ref(&next));
        let agent = std::fs::read_to_string(d.path().join("etc/rynas/tls/agent.crt")).unwrap();
        assert_eq!(
            rynas_tls::inspect(&agent).unwrap()[1].fingerprint_sha256,
            rynas_tls::inspect(&next).unwrap()[0].fingerprint_sha256
        );
    }

    fn retired(store: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(store.join("retired"))
            .map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn a_clock_behind_or_unsynchronized_never_replaces_the_ca() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("state/tls");
        let now = rynas_tls::now();
        // made when the clock was right; now the RTC was reset and NTP has not answered yet:
        // the CA looks not yet valid
        let s = svc_clock(d.path(), "nas", vec![], false);
        s.ensure(false, false).unwrap();
        let ahead = rynas_tls::generate_ca("nas", now + 30 * DAY_MS).unwrap();
        std::fs::write(store.join("ca.crt"), &ahead.chain).unwrap();
        std::fs::write(store.join("ca.key"), &ahead.key).unwrap();
        s.ensure(false, false).unwrap();
        assert_eq!(std::fs::read_to_string(store.join("ca.key")).unwrap(), ahead.key);
        assert_eq!(s.agent_trust(8443).unwrap().cas, std::slice::from_ref(&ahead.chain));
        // even with a synchronized clock (the CA is not past its end)
        let s = svc_clock(d.path(), "nas", vec![], true);
        s.ensure(false, false).unwrap();
        assert_eq!(std::fs::read_to_string(store.join("ca.key")).unwrap(), ahead.key);
        // near its end (or past it) by an unsynchronized clock: no next CA, no switch
        let s = svc_clock(d.path(), "nas", vec![], false);
        for left in [500, 300, -10] {
            let old = rynas_tls::generate_ca("nas", now - (rynas_tls::CA_DAYS - left) * DAY_MS).unwrap();
            std::fs::write(store.join("ca.crt"), &old.chain).unwrap();
            std::fs::write(store.join("ca.key"), &old.key).unwrap();
            s.ensure(false, false).unwrap();
            assert_eq!(std::fs::read_to_string(store.join("ca.key")).unwrap(), old.key, "{left} days left");
            assert!(!store.join("ca-next.key").exists());
        }
        assert!(retired(&store).is_empty());
    }

    #[test]
    fn a_switch_keeps_the_old_ca_retired() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("state/tls");
        let s = svc_in(d.path(), "nas", vec![]);
        s.ensure(false, false).unwrap();
        let now = rynas_tls::now();
        let old = rynas_tls::generate_ca("nas", now - (rynas_tls::CA_DAYS - 300) * DAY_MS).unwrap();
        std::fs::write(store.join("ca.crt"), &old.chain).unwrap();
        std::fs::write(store.join("ca.key"), &old.key).unwrap();
        assert!(s.ensure(false, false).unwrap());
        assert_ne!(std::fs::read_to_string(store.join("ca.key")).unwrap(), old.key);
        let r = retired(&store);
        assert_eq!(r.len(), 2, "{r:?}");
        let key = r.iter().find(|n| n.starts_with("ca-") && n.ends_with(".key")).unwrap();
        assert_eq!(std::fs::read_to_string(store.join("retired").join(key)).unwrap(), old.key);
        assert_eq!(mode(&store.join("retired")), 0o700);
    }

    #[test]
    fn agent_addresses_names_first_no_loopback() {
        let a = agent_addresses(
            "nas.example.lan",
            &["2001:db8::5".parse().unwrap(), "192.0.2.5".parse().unwrap(), "fe80::1".parse().unwrap()],
            8443,
        );
        assert_eq!(
            a,
            [
                "https://nas.example.lan:8443",
                "https://nas:8443",
                "https://nas.local:8443",
                "https://192.0.2.5:8443",
                "https://[2001:db8::5]:8443"
            ]
        );
    }
}
