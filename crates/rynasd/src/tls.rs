//! HTTPS certificate of the web interface (wave W0, D-PROXY: `rynas-web` keeps its own port
//! `:8443` with rustls, never routed through the reverse proxy, and reuses the active
//! certificate).
//!
//! rynasd is the only writer. Its private store, `<state>/tls/` (0700 root), holds:
//! - `ca.crt`, `ca.key`: the local CA ([`rynas_tls::generate_ca`]), made once;
//! - `local.crt`, `local.key`: the leaf it issued, re-issued when the host name or an
//!   address is missing from it or 30 days before it expires ([`rynas_tls::needs_reissue`]);
//! - `imported.crt`, `imported.key`: the administrator's certificate, when imported;
//! - `active`: `local` or `imported`.
//!
//! The active pair is published for `rynas-web` in `/etc/rynas/tls/` (0750 root:rynas):
//! `ui.crt` (chain, 0644), `ui.key` (0640 root:rynas) and `ca.crt` (0644). rynas-web
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

use std::io::{self, Write};
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rynas_api_types::error_codes;
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

/// The names probe: host name and interface addresses.
pub type Probe = Box<dyn Fn() -> (String, Vec<IpAddr>) + Send + Sync>;

pub struct TlsSvc {
    store: PathBuf,
    publish: PathBuf,
    /// Group that may read the published key (`rynas`), when rynasd runs as root.
    gid: Option<u32>,
    probe: Probe,
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
        let gid = group
            .filter(|_| nix::unistd::Uid::effective().is_root())
            .and_then(|g| nix::unistd::Group::from_name(g).ok().flatten())
            .map(|g| g.gid.as_raw());
        Arc::new(Self {
            store,
            publish,
            gid,
            probe,
            lock: Mutex::new(()),
        })
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

    /// The local CA: the stored one while it is valid for another year, else a new one.
    fn ca(&self, host: &str, now: i64) -> io::Result<(PemPair, bool)> {
        if let Some(p) = self.read_pair("ca")
            && let Ok(info) = rynas_tls::inspect(&p.chain)
            && info[0].not_after_ms - now > 365 * 86_400_000
            && rynas_tls::key_der(&p.key).is_ok()
        {
            return Ok((p, false));
        }
        let ca = rynas_tls::generate_ca(host, now).map_err(io::Error::other)?;
        self.write_pair("ca", &ca)?;
        tracing::info!(
            "tls: new local CA {}",
            rynas_tls::inspect(&ca.chain)
                .map(|i| i[0].fingerprint_sha256.clone())
                .unwrap_or_default()
        );
        Ok((ca, true))
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
        let names = self.names();
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
                let current = self.read_pair("local").filter(|p| {
                    let Ok(info) = rynas_tls::inspect(&p.chain) else {
                        return false;
                    };
                    rynas_tls::certified_key(p).is_ok()
                        && info.get(1).is_some_and(|c| {
                            rynas_tls::inspect(&ca.chain)
                                .is_ok_and(|ci| ci[0].fingerprint_sha256 == c.fingerprint_sha256)
                        })
                        && !rynas_tls::needs_reissue(&info[0], &names, now)
                });
                match current {
                    Some(p) if !reissue && !new_ca => p,
                    _ => {
                        let leaf =
                            rynas_tls::issue_leaf(&ca, &names, now).map_err(io::Error::other)?;
                        self.write_pair("local", &leaf)?;
                        tracing::info!("tls: new certificate for {}", names.all().join(", "));
                        leaf
                    }
                }
            }
        };
        if active != self.active() || !self.path("active").exists() {
            self.set_active(active)?;
        }
        self.publish(&pair, &ca)
    }

    /// Writes the published files that differ from `pair` / `ca`.
    fn publish(&self, pair: &PemPair, ca: &PemPair) -> io::Result<bool> {
        // the key first: rynas-web only takes a pair whose key matches the chain
        let files: [(&str, &str, u32); 3] = [
            ("ui.key", &pair.key, 0o640),
            ("ui.crt", &pair.chain, 0o644),
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

fn chown_group(p: &Path, gid: u32) -> io::Result<()> {
    nix::unistd::chown(p, None, Some(nix::unistd::Gid::from_raw(gid))).map_err(io::Error::from)
}

/// Writes `data` to `path` through a temporary file in the same folder and a rename, with
/// `mode` (and group `gid`) set before the rename.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32, gid: Option<u32>) -> io::Result<()> {
    let tmp = path.with_extension("tmp~");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    if let Some(g) = gid {
        chown_group(&tmp, g)?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    drop(f);
    std::fs::rename(&tmp, path)
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

/// Ensures the certificate at start, then re-checks the names every [`CHECK_INTERVAL`].
pub fn spawn_watch(svc: Arc<TlsSvc>) {
    tokio::spawn(async move {
        loop {
            let s = svc.clone();
            match tokio::task::spawn_blocking(move || s.ensure(false, false)).await {
                Ok(Ok(true)) => tracing::info!("tls: published certificate updated"),
                Ok(Ok(false)) => {}
                Ok(Err(e)) => tracing::error!("tls: {e}"),
                Err(e) => tracing::error!("tls: {e}"),
            }
            tokio::time::sleep(CHECK_INTERVAL).await;
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

async fn import(
    State(st): State<TlsState>,
    body: Result<Json<TlsImportRequest>, JsonRejection>,
) -> Res<Json<TlsStatus>> {
    let s = svc(&st)?;
    let Json(req) = body?;
    blocking(move || s.import(&req)).await.map(Json)
}

async fn local(State(st): State<TlsState>) -> Res<Json<TlsStatus>> {
    let s = svc(&st)?;
    blocking(move || s.use_local(false)).await.map(Json)
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

pub fn router(st: TlsState) -> Router {
    Router::new()
        .route("/v1/system/tls", get(get_status))
        .route("/v1/system/tls/import", post(import))
        .route("/v1/system/tls/local", post(local))
        .route("/v1/system/tls/ca", get(ca))
        .with_state(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc_in(dir: &Path, host: &'static str, addrs: Vec<IpAddr>) -> Arc<TlsSvc> {
        TlsSvc::new(
            dir.join("state/tls"),
            dir.join("etc/rynas/tls"),
            None,
            Box::new(move || (host.to_string(), addrs.clone())),
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
}
