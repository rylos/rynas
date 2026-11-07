//! The network UPS server (`/v1/ups/netserver`, `/v1/ups/plan/netserver`) and the
//! `ups.netserver` configuration domain: other machines (servers and PCs running upsmon,
//! home automation, dashboards) read this NAS's UPS from upsd on port 3493 and shut down on
//! its low battery. Design and sources: `docs/research/ups.md` ("Network UPS server").
//!
//! - **Files** (plain NUT, rynas blocks only): `LISTEN` lines in `/etc/nut/upsd.conf` (the
//!   admin's own `LISTEN` lines are set aside in the block while the server is on and put
//!   back when it is off or the key is removed, [`net::admin_listen`]), the client accounts
//!   in `upsd.users` (block `ups-netserver`), and a drop-in of `nut-server.service`
//!   (wait for the network with chosen addresses; systemd's per-service IP filter with
//!   allowed hosts). `upsd.users` already carries blocks of `ups.device` and of the battery
//!   tests and the engine keeps one owner per file, so the accounts block is written by
//!   [`NetServerDomain::reconcile`] (atomic, the file's owner kept and its mode capped at
//!   0640, written back with the old value on rollback, under the apply lock that the
//!   battery tests also take), like the battery test user. The drift report compares the
//!   block with the applied accounts ([`Applier::state_drift`]).
//! - **Removing the key** (restoring a revision from before it existed) writes the "off"
//!   files ([`Applier::render_removed`]): no `LISTEN` of rynas, no drop-in, no accounts.
//! - **Passwords** arrive write-only (`password`, or `generate`; a new account without
//!   either gets a generated one) and are sealed at once with rynasd's age key (the
//!   notification engine's `<state>/notify/secret.key`, which a passphrase-protected export
//!   carries): the configuration key, its revisions and answers never hold them in clear.
//!   The `PUT` answer returns the passwords of that request once.
//! - **Restart**: upsd reads `LISTEN` only at start, so an apply restarts
//!   `nut-server.service` when it runs and `upsd.conf` or the drop-in changed (upsmon on the
//!   NAS reconnects by itself), and only reloads it when only the accounts changed. Applied
//!   with `ups.device`, whose restart already read the new `upsd.conf`, upsd restarts once
//!   (again only for a changed drop-in, which needs a daemon-reload first). The health check
//!   wants upsd answering on the first `LISTEN` address (always on `127.0.0.1`).
//! - **A module's UPS layer** owns `/etc/nut`: rynas writes nothing there. A layer with a
//!   network UPS server of its own (`rynas_module::ups::NetServerLayer`) gets the setting
//!   through its tool instead ([`layer_apply`]): upsd listens on every address (no listen
//!   choice), allowed hosts are required (the layer's firewall, reported as `filter_active`),
//!   only new or changed passwords go to the tool, on its standard input. The state comes from
//!   the layer's own status (`UpsNetServerState::layer`). A layer without the server, or too
//!   old for it, is not offered ([`UpsNetBlock::Module`], [`UpsNetBlock::ModuleTooOld`]).

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use rynas_api_types::ups::*;
use rynas_api_types::{StateDrift, StateDriftKind, error_codes};
use rynas_config::files::{self, FileKind};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_module::UpsLayer;
use rynas_module::ups::{NetLayerRead, NetLayerStatus, NetServerLayer, Runner};
use rynas_notify::engine::secrets::SecretBox;
use rynas_nut::netserver as net;
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::ops::{Error, Res, user};
use crate::ups::{Ups, UpsState};
use crate::ups_setup::{DEVICE_KEY, NUT_USERS, foreign_sections};

pub const NETSERVER_KEY: &str = "ups.netserver";
pub const UPSD_CONF: &str = "/etc/nut/upsd.conf";
pub const DROPIN: &str = "/etc/systemd/system/nut-server.service.d/rynas-netserver.conf";
pub const SERVER_UNIT: &str = "nut-server.service";
const BLOCK: &str = net::BLOCK_ID;
const MASK: &str = "********";

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// rynasd's secret key (created on first use, folder 0700).
pub fn secret_box(dir: &Path) -> Result<SecretBox, String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("secret store: {e}"))?;
    SecretBox::open(dir).map_err(|e| format!("secret store: {e}"))
}

fn seal(sb: &SecretBox, pw: &str) -> Result<String, String> {
    let m = [("password".to_string(), pw.to_string())].into();
    sb.seal(&m).map(|s| b64().encode(s))
}

/// A client's password for `upsd.users`: opened, and checked again (it goes between double
/// quotes; [`net::safe_in_users`]).
fn open_password(sb: &SecretBox, c: &UpsNetClient) -> Result<String, String> {
    let pw = unseal(sb, c)?;
    if !net::safe_in_users(&pw) {
        return Err(format!(
            "the stored password of the account {} cannot go into {NUT_USERS} (a quote, \
             backslash or line break): generate a new one",
            c.name
        ));
    }
    Ok(pw)
}

fn unseal(sb: &SecretBox, c: &UpsNetClient) -> Result<String, String> {
    let sealed = c
        .sealed
        .as_deref()
        .ok_or_else(|| format!("the account {} has no stored password", c.name))?;
    let bytes = b64()
        .decode(sealed)
        .map_err(|e| format!("account {}: {e}", c.name))?;
    sb.open_sealed(&bytes)
        .ok()
        .and_then(|mut m| m.remove("password"))
        .ok_or_else(|| {
            format!(
                "the password of the account {} cannot be decrypted (restored without the key?): \
                 generate a new one",
                c.name
            )
        })
}

/// A copy for answers: no passwords, no envelopes.
pub fn for_answer(mut c: UpsNetServerConfig) -> UpsNetServerConfig {
    for cl in &mut c.clients {
        cl.password = None;
        cl.generate = false;
        cl.sealed = None;
    }
    c
}

/// Seals the passwords of an input (an envelope sent by a client is dropped first): the
/// admin's `password`, a generated one on `generate` or for a new account, else the envelope
/// of the same account in `previous` (staged value first, then the applied one). Returns the
/// passwords to show once.
pub fn seal_input(
    c: &mut UpsNetServerConfig,
    previous: &[&UpsNetServerConfig],
    sb: &SecretBox,
    generate: &dyn Fn() -> Result<String, String>,
) -> Result<Vec<UpsNetPassword>, Error> {
    let mut shown = Vec::new();
    for (i, cl) in c.clients.iter_mut().enumerate() {
        cl.sealed = None;
        let prev = previous
            .iter()
            .flat_map(|p| p.clients.iter())
            .find(|p| p.name == cl.name && p.sealed.is_some());
        let new_pw = match (cl.password.take(), cl.generate, prev) {
            (Some(p), _, _) => Some(p),
            (None, true, _) | (None, false, None) => Some(generate().map_err(Error::internal)?),
            (None, false, Some(p)) => {
                cl.sealed = p.sealed.clone();
                None
            }
        };
        cl.generate = false;
        if let Some(pw) = new_pw {
            cl.sealed = Some(
                seal(sb, &pw)
                    .map_err(|e| Error::invalid(format!("clients.{i}.password"), e))?,
            );
            shown.push(UpsNetPassword {
                name: cl.name.clone(),
                password: pw,
            });
        }
    }
    Ok(shown)
}

fn read(root: &Path, p: &str) -> Option<String> {
    std::fs::read_to_string(files::under_root(root, Path::new(p))).ok()
}

/// User names of the admin's own `upsd.users` sections.
pub fn foreign_users(root: &Path) -> Vec<String> {
    read(root, NUT_USERS)
        .map(|t| net::foreign_users(&t))
        .unwrap_or_default()
}

/// The files the engine writes for `c` (plain NUT); `upsd_conf` is the current file (the
/// admin's own `LISTEN` lines in it are kept, [`net::admin_listen`]).
pub fn nut_files(c: &UpsNetServerConfig, upsd_conf: Option<&str>) -> Vec<ManagedFile> {
    let saved = upsd_conf.map(net::admin_listen).unwrap_or_default();
    let mut upsd = ManagedFile::block(UPSD_CONF, BLOCK, net::render_upsd_conf(c, &saved))
        .mode(0o640)
        .group("nut");
    if let FileKind::Block { drop_outside, .. } = &mut upsd.kind {
        *drop_outside = net::upsd_conf_drops(c);
    }
    let dropin = match net::render_dropin(c) {
        Some(body) => ManagedFile::whole(DROPIN, body, true),
        None => ManagedFile::absent(DROPIN),
    };
    vec![upsd, dropin]
}

/// The accounts block of `upsd.users` for `c` (`None`: no block). `password` opens a client's
/// password (the real one on an apply, a mask in the plan).
pub fn users_block(
    c: &UpsNetServerConfig,
    password: &dyn Fn(&UpsNetClient) -> Result<String, String>,
) -> Result<Option<String>, String> {
    if !c.enabled || c.clients.is_empty() {
        return Ok(None);
    }
    let mut list = Vec::new();
    for cl in &c.clients {
        list.push((cl.name.clone(), cl.role, password(cl)?));
    }
    Ok(Some(net::render_users(&list)))
}

/// Writes (or removes) the accounts block of `upsd.users`, keeping the file's mode and owner.
/// Returns whether the file changed.
pub fn write_users(root: &Path, block: Option<&str>) -> Result<bool, String> {
    let path = files::under_root(root, Path::new(NUT_USERS));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // nothing to remove from a file that is not there
        Err(_) if block.is_none() => return Ok(false),
        Err(e) => return Err(format!("NUT is not installed or not readable ({NUT_USERS}: {e})")),
    };
    let kind = FileKind::Block {
        id: BLOCK.into(),
        drop_outside: Vec::new(),
    };
    let new = match block {
        Some(b) => {
            if files::current_hash(&kind, &text) == Some(files::sha256_hex(b.as_bytes())) {
                return Ok(false);
            }
            files::render_final(&kind, b, Some(&text)).0
        }
        None => match net::without_block(&text, BLOCK) {
            Some(t) => t,
            None => return Ok(false),
        },
    };
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(&path).map_err(|e| format!("{NUT_USERS}: {e}"))?;
    let cand = files::candidate_path(&path);
    // passwords: never readable by others, whatever the file had
    files::write_synced(&cand, new.as_bytes(), meta.mode() & 0o640)
        .and_then(|_| std::os::unix::fs::chown(&cand, Some(meta.uid()), Some(meta.gid())))
        .and_then(|_| files::rename_synced(&cand, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&cand);
            format!("{NUT_USERS}: {e}")
        })?;
    Ok(true)
}

/// Commands an apply runs after the files (also shown in the plan).
pub fn commands() -> Vec<String> {
    vec![
        "systemctl daemon-reload (when the drop-in changes)".into(),
        format!(
            "systemctl restart {SERVER_UNIT} (when it runs and upsd.conf or the drop-in \
             changes; only a reload when only the accounts change)"
        ),
    ]
}

/// Tells whether something accepts TCP connections on an address (upsd, after an apply).
pub type Probe = Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;

/// The real [`Probe`]: a TCP connect with a 2 s timeout.
pub fn tcp_probe() -> Probe {
    Arc::new(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok())
}

/// Whether the kernel applies the IP filter of the running `nut-server.service`
/// ([`net::filter_enforced`]; `None`: unknown).
pub fn filter_state(sys: &dyn rynas_config::System) -> Option<bool> {
    let cmd = Cmd::new("systemctl")
        .args(["show", "--value", "-p", "IPIngressPackets", SERVER_UNIT])
        .env_clear()
        .timeout(Duration::from_secs(10));
    sys.run(&cmd).ok().and_then(|o| net::filter_enforced(&o.stdout))
}

/// Refuses chosen listen addresses this NAS does not have (`addrs`, empty: unknown, no
/// check); one already in `kept` (the saved value) stays allowed, so a form can still be
/// saved while an interface is down.
pub fn check_listen_owned(
    listen: &[String],
    addrs: &[IpAddr],
    kept: &[String],
) -> Result<(), (String, String)> {
    if addrs.is_empty() {
        return Ok(());
    }
    for (i, a) in listen.iter().enumerate() {
        let ours = a.parse::<IpAddr>().is_ok_and(|ip| addrs.contains(&ip));
        if !ours && !kept.contains(a) {
            return Err((
                format!("listen.{i}"),
                "not an address of this NAS (see the list of its addresses)".into(),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// `ups.netserver`.
pub struct NetServerDomain {
    pub layer: Option<Arc<dyn UpsLayer>>,
    /// Folder of rynasd's secret key (`<state>/notify`).
    pub secret_dir: PathBuf,
    /// Checks that upsd answers after an apply ([`tcp_probe`]; `None` under `--dry-system`:
    /// no check).
    pub probe: Option<Probe>,
    /// Runs the layer's tool (exit codes kept: 2 means refused values); the host, or the dry
    /// runner under `--dry-system`.
    pub runner: Arc<dyn Runner>,
}

impl NetServerDomain {
    fn net_layer(&self) -> Option<&dyn NetServerLayer> {
        self.layer.as_deref().and_then(|l| l.netserver())
    }
    /// The accounts block `c` puts in `upsd.users`, with the real passwords.
    fn users_for(&self, c: &UpsNetServerConfig) -> Result<Option<String>, String> {
        let sb = if c.enabled && !c.clients.is_empty() {
            Some(secret_box(&self.secret_dir)?)
        } else {
            None
        };
        users_block(c, &|cl| match &sb {
            Some(sb) => open_password(sb, cl),
            None => Err("no secret store".into()),
        })
    }
}

fn config_of(v: &Value) -> Result<UpsNetServerConfig, Invalid> {
    serde_json::from_value(v.clone())
        .map_err(|e| Invalid::from(format!("invalid ups.netserver document: {e}")))
}

impl Applier for NetServerDomain {
    fn key(&self) -> &str {
        NETSERVER_KEY
    }

    /// After `ups.device` (45): upsd serves the UPS that sets up.
    fn order(&self) -> i32 {
        46
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let mut c = config_of(&value)?;
        net::normalize(&mut c, &[]).map_err(|(f, m)| Invalid::at(&f, m))?;
        if let Some(n) = self.net_layer() {
            c.listen.clear();
            n.check(&c).map_err(|(f, m)| Invalid::at(&f, m))?;
        }
        for (i, cl) in c.clients.iter().enumerate() {
            if cl.password.is_some() || cl.generate || cl.sealed.is_none() {
                return Err(Invalid::at(
                    format!("clients.{i}.password"),
                    "passwords are set through the network UPS server form",
                ));
            }
        }
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        if self.layer.is_some() {
            return Ok(Vec::new());
        }
        let c = config_of(value).map_err(|e| e.message)?;
        let foreign = foreign_users(ctx.root);
        if let Some(cl) = c.clients.iter().find(|cl| foreign.contains(&cl.name)) {
            return Err(format!(
                "{NUT_USERS} already has a user {} that rynas does not manage",
                cl.name
            ));
        }
        Ok(nut_files(&c, ctx.read(Path::new(UPSD_CONF)).as_deref()))
    }

    /// The "off" files: no `LISTEN` of rynas (the admin's own put back), no drop-in.
    fn render_removed(
        &self,
        old: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<Vec<ManagedFile>, String> {
        if self.layer.is_some() {
            return Ok(Vec::new());
        }
        let mut off = old
            .and_then(|v| config_of(v).ok())
            .unwrap_or_default();
        off.enabled = false;
        Ok(nut_files(&off, ctx.read(Path::new(UPSD_CONF)).as_deref()))
    }

    fn reconcile(
        &self,
        from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        if let Some(n) = self.net_layer() {
            return layer_apply(n, self.runner.as_ref(), &self.secret_dir, from, to, ctx.log);
        }
        if self.layer.is_some() {
            return Ok(());
        }
        // a removed key leaves no accounts
        let block = match to {
            Some(v) => self.users_for(&config_of(v).map_err(|e| e.message)?)?,
            None => None,
        };
        let users = write_users(ctx.root, block.as_deref())?;
        if users {
            (ctx.log)(&format!("updated the network UPS server accounts in {NUT_USERS}"));
        }
        let dropin = ctx.replaced(Path::new(DROPIN));
        let conf = ctx.replaced(Path::new(UPSD_CONF));
        if dropin {
            (ctx.log)("$ systemctl daemon-reload");
            ctx.system.daemon_reload()?;
        }
        if !(dropin || conf || users) {
            return Ok(());
        }
        if !ctx.system.unit_active(SERVER_UNIT).unwrap_or(false) {
            (ctx.log)(&format!(
                "{SERVER_UNIT} is not running: the settings take effect when it starts"
            ));
            return Ok(());
        }
        // ups.device of the same apply restarted upsd with every file in place, but before
        // the daemon-reload of a new drop-in
        if dropin || (conf && !ctx.restarted(SERVER_UNIT)) {
            (ctx.log)(&format!("$ systemctl restart {SERVER_UNIT}"));
            ctx.restart_unit(SERVER_UNIT)
        } else if users {
            (ctx.log)(&format!("$ systemctl reload {SERVER_UNIT}"));
            ctx.system.reload_or_restart_unit(SERVER_UNIT)
        } else {
            Ok(())
        }
    }

    /// upsd answers on the first `LISTEN` address (`127.0.0.1` in both layouts) when it
    /// runs; a filter the kernel does not apply is logged (the state reports it).
    fn health(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        if self.layer.is_some() {
            return Ok(());
        }
        let Some(c) = value.map(config_of).transpose().map_err(|e| e.message)? else {
            return Ok(());
        };
        if !c.enabled || !ctx.system.unit_active(SERVER_UNIT).unwrap_or(false) {
            return Ok(());
        }
        if !c.allow.is_empty() && filter_state(ctx.system) == Some(false) {
            (ctx.log)(
                "warning: the kernel does not apply systemd's IP filter here (no eBPF cgroup \
                 support): any host that reaches port 3493 can connect, limit it with a firewall",
            );
        }
        let Some(probe) = &self.probe else {
            return Ok(());
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], NUT_PORT));
        for i in 0..12 {
            if probe(addr) {
                return Ok(());
            }
            if i < 11 {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        Err(format!(
            "upsd does not answer on {addr} (its first LISTEN address): see journalctl -u {SERVER_UNIT}"
        ))
    }

    /// The accounts block of `upsd.users` against the applied accounts.
    fn state_drift(
        &self,
        applied: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<Vec<StateDrift>, String> {
        if let Some(n) = self.net_layer() {
            return Ok(layer_drift(n, self.runner.as_ref(), applied));
        }
        let Some(v) = applied.filter(|_| self.layer.is_none()) else {
            return Ok(Vec::new());
        };
        let want = self.users_for(&config_of(v).map_err(|e| e.message)?)?;
        let have = ctx
            .read(Path::new(NUT_USERS))
            .and_then(|t| rynas_nut::setup::block_of(&t, BLOCK));
        if want == have {
            return Ok(Vec::new());
        }
        let (kind, detail) = match (&want, &have) {
            (Some(_), None) => (
                StateDriftKind::Missing,
                format!("the network UPS server accounts are missing from {NUT_USERS}"),
            ),
            (None, _) => (
                StateDriftKind::Mismatch,
                format!("{NUT_USERS} has network UPS server accounts the configuration does not"),
            ),
            (Some(_), Some(_)) => (
                StateDriftKind::Mismatch,
                format!("the network UPS server accounts in {NUT_USERS} were changed"),
            ),
        };
        Ok(vec![StateDrift {
            key: NETSERVER_KEY.into(),
            object: format!("file:{NUT_USERS}#{BLOCK}"),
            kind,
            detail,
            detail_code: None,
            detail_args: rynas_api_types::StateDriftArgs {
                path: Some(NUT_USERS.into()),
                ..Default::default()
            },
        }])
    }
}

// ---------------------------------------------------------------------------------------------
// a module layer's server
// ---------------------------------------------------------------------------------------------

/// The layer's state of the server (its tool run through `runner`).
pub fn layer_read(n: &dyn NetServerLayer, runner: &dyn Runner) -> NetLayerRead {
    match runner.run(&n.status_cmd()) {
        Ok(o) => n.parse_status(&o),
        Err(e) => NetLayerRead::Unreadable(e),
    }
}

/// The accounts and roles the layer gets for `c`.
fn layer_users(c: &UpsNetServerConfig) -> Vec<(String, UpsClientRole)> {
    c.clients.iter().map(|cl| (cl.name.clone(), cl.role)).collect()
}

/// The command that applies `to` (`None` or off: the layer's `off`) and whether passwords go
/// with it. A password goes to the layer when the account is new or its envelope changed
/// since `from`, or when the layer does not have the account (an `off` removed it, or its
/// state is unreadable: then every password goes, the same value is harmless).
pub fn layer_command(
    n: &dyn NetServerLayer,
    from: Option<&UpsNetServerConfig>,
    to: Option<&UpsNetServerConfig>,
    have: Option<&NetLayerStatus>,
    password: &dyn Fn(&UpsNetClient) -> Result<String, String>,
) -> Result<Cmd, String> {
    let Some(c) = to.filter(|c| c.enabled) else {
        return Ok(n.off_cmd());
    };
    let mut pw = Vec::new();
    for cl in &c.clients {
        let same = from
            .filter(|f| f.enabled)
            .and_then(|f| f.clients.iter().find(|p| p.name == cl.name))
            .is_some_and(|p| p.sealed == cl.sealed);
        let known = have.is_some_and(|h| h.users.iter().any(|(u, _)| *u == cl.name));
        if !(same && known) {
            pw.push((cl.name.clone(), password(cl)?));
        }
    }
    Ok(n.set_cmd(&c.allow, &layer_users(c), &pw))
}

/// Applies `to` through the layer's tool: exit 0 applied, 2 values refused (the layer's
/// reason), anything else not applied. Passwords never reach the log.
pub fn layer_apply(
    n: &dyn NetServerLayer,
    runner: &dyn Runner,
    secret_dir: &Path,
    from: Option<&Value>,
    to: Option<&Value>,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let to = to.map(config_of).transpose().map_err(|e| e.message)?;
    let from = from.and_then(|v| config_of(v).ok());
    let on = to.as_ref().is_some_and(|c| c.enabled);
    let have = match on.then(|| layer_read(n, runner)) {
        Some(NetLayerRead::Status(s)) => Some(s),
        Some(NetLayerRead::Unsupported) => {
            return Err("the UPS layer has no network UPS server (update it)".into());
        }
        _ => None,
    };
    let sb = if on && to.as_ref().is_some_and(|c| !c.clients.is_empty()) {
        Some(secret_box(secret_dir)?)
    } else {
        None
    };
    let cmd = layer_command(n, from.as_ref(), to.as_ref(), have.as_ref(), &|cl| match &sb {
        Some(sb) => unseal(sb, cl),
        None => Err("no secret store".into()),
    })?;
    log(&format!("$ {}", cmd.display()));
    let out = runner.run(&cmd)?;
    let last = |t: &str| t.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
    match out.code {
        Some(0) => {
            let said = last(&out.stdout);
            if !said.is_empty() {
                log(&said);
            }
            Ok(())
        }
        Some(2) => Err(format!("the UPS layer refused the values: {}", last(&out.stderr))),
        c => Err(format!(
            "the UPS layer did not apply the network UPS server (exit code {}): {}",
            c.map_or("-".into(), |c| c.to_string()),
            last(&out.stderr)
        )),
    }
}

/// The layer's server against the applied value: on/off, allowed hosts, accounts.
fn layer_drift(n: &dyn NetServerLayer, runner: &dyn Runner, applied: Option<&Value>) -> Vec<StateDrift> {
    let Some(c) = applied.and_then(|v| config_of(v).ok()) else {
        return Vec::new();
    };
    let NetLayerRead::Status(s) = layer_read(n, runner) else {
        return Vec::new();
    };
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let mut users = layer_users(&c);
    let mut have = s.users.clone();
    users.sort_by(|a, b| a.0.cmp(&b.0));
    have.sort_by(|a, b| a.0.cmp(&b.0));
    let same = if c.enabled {
        s.enabled && sorted(c.allow.clone()) == sorted(s.allow.clone()) && users == have
    } else {
        !s.enabled
    };
    if same {
        return Vec::new();
    }
    vec![StateDrift {
        key: NETSERVER_KEY.into(),
        object: "ups-layer:netserver".into(),
        kind: StateDriftKind::Mismatch,
        detail: "the UPS layer's network UPS server differs from the applied setting (changed \
                 with its own tool)"
            .into(),
        detail_code: None,
        detail_args: Default::default(),
    }]
}

/// What applying `c` runs with a layer (passwords never shown).
pub fn layer_plan(n: &dyn NetServerLayer, c: &UpsNetServerConfig) -> UpsSetupPlan {
    let cmd = layer_command(n, None, Some(c), None, &|_| Ok(String::new()))
        .map(|c| c.display())
        .unwrap_or_default();
    let mut commands = vec![cmd];
    if c.enabled && !c.clients.is_empty() {
        commands.push("(passwords of new or changed accounts on its standard input)".into());
    }
    UpsSetupPlan {
        files: Vec::new(),
        commands,
        conflicts: Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// the plan
// ---------------------------------------------------------------------------------------------

/// What applying `c` does (no side effects; passwords masked).
pub fn plan(root: &Path, c: &UpsNetServerConfig) -> UpsSetupPlan {
    let mut out: Vec<UpsPlanFile> = nut_files(c, read(root, UPSD_CONF).as_deref())
        .into_iter()
        .map(|f| {
            let path = f.path.to_string_lossy().into_owned();
            let existing = read(root, &path);
            match &f.kind {
                FileKind::Block { id, drop_outside } => {
                    let removed = existing
                        .as_deref()
                        .map(|t| {
                            t.lines()
                                .filter(|l| {
                                    let first = l.split_whitespace().next().unwrap_or("");
                                    drop_outside.iter().any(|d| d == first)
                                        && rynas_nut::setup::block_of(t, id)
                                            .is_none_or(|b| !b.lines().any(|x| x == *l))
                                })
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    UpsPlanFile {
                        before: existing
                            .as_deref()
                            .and_then(|t| rynas_nut::setup::block_of(t, id)),
                        after: f.content,
                        removed,
                        path,
                    }
                }
                FileKind::Absent => UpsPlanFile {
                    before: existing,
                    after: String::new(),
                    removed: Vec::new(),
                    path,
                },
                FileKind::Whole { .. } => UpsPlanFile {
                    before: existing,
                    after: f.content,
                    removed: Vec::new(),
                    path,
                },
            }
        })
        // a drop-in that is not there and stays away is no change
        .filter(|f| !(f.before.is_none() && f.after.is_empty()))
        .collect();
    let users = users_block(c, &|_| Ok(MASK.into())).ok().flatten();
    let before = read(root, NUT_USERS).and_then(|t| rynas_nut::setup::block_of(&t, BLOCK));
    if users.is_some() || before.is_some() {
        out.insert(
            1,
            UpsPlanFile {
                path: NUT_USERS.into(),
                before: before.map(|b| mask_passwords(&b)),
                after: users.unwrap_or_default(),
                removed: Vec::new(),
            },
        );
    }
    UpsSetupPlan {
        files: out,
        commands: commands(),
        conflicts: Vec::new(),
    }
}

fn mask_passwords(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim_start().starts_with("password") {
                format!("\tpassword = \"{MASK}\"\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// endpoints
// ---------------------------------------------------------------------------------------------

pub fn router(st: UpsState) -> Router {
    Router::new()
        .route("/v1/ups/netserver", get(state_get).put(state_put))
        .route("/v1/ups/plan/netserver", post(plan_post))
        .with_state(st)
}

fn ups(st: &UpsState) -> Res<Arc<Ups>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "configuration and tasks are not enabled in this rynasd".into(),
        )
    })
}

async fn blocking<T: Send + 'static>(
    u: &Arc<Ups>,
    f: impl FnOnce(&Ups) -> Res<T> + Send + 'static,
) -> Res<T> {
    let u = u.clone();
    tokio::task::spawn_blocking(move || f(&u))
        .await
        .map_err(Error::internal)?
}

fn net_layer(u: &Ups) -> Option<&dyn NetServerLayer> {
    u.layer.as_deref().and_then(|l| l.netserver())
}

/// The layer's state of the server (`None`: plain NUT, or a layer without the server).
fn layer_state(u: &Ups) -> Option<NetLayerRead> {
    net_layer(u).map(|n| layer_read(n, u.setup.runner.as_ref()))
}

fn blocked(u: &Ups, layer: Option<&NetLayerRead>) -> Option<UpsNetBlock> {
    if u.layer.is_some() {
        match layer {
            None => Some(UpsNetBlock::Module),
            Some(NetLayerRead::Unsupported) => Some(UpsNetBlock::ModuleTooOld),
            Some(_) => None,
        }
    } else if u.backend() == UpsBackend::NotInstalled {
        Some(UpsNetBlock::NutMissing)
    } else {
        None
    }
}

fn parse_doc(v: Option<Value>) -> Res<Option<UpsNetServerConfig>> {
    v.map(|v| serde_json::from_value(v).map_err(Error::internal))
        .transpose()
}

/// `(applied, staged)` with envelopes (for carrying them forward).
fn docs(u: &Ups, owner: &str) -> Res<(Option<UpsNetServerConfig>, Option<UpsNetServerConfig>)> {
    let c = &u.ops.config;
    let applied = parse_doc(c.current(NETSERVER_KEY)?)?;
    let pending = match c.pending_value(owner, NETSERVER_KEY)? {
        Some(v) => parse_doc(v)?,
        None => None,
    };
    Ok((applied, pending))
}

/// The NUT name clients monitor: the wizard's UPS, else the first `ups.conf` section.
fn ups_name(u: &Ups) -> Option<String> {
    u.ops
        .config
        .current(DEVICE_KEY)
        .ok()
        .flatten()
        .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string))
        .or_else(|| {
            foreign_sections(&u.root).into_iter().next().map(|s| s.name)
        })
}

/// This NAS's addresses as listen choices, and the one of the default route.
pub fn addresses(f: &crate::web_access::model::HostFacts) -> (Vec<UpsNetAddress>, Option<String>) {
    let mut out = Vec::new();
    for ip in &f.addrs {
        if !net::valid_listen(&ip.to_string()) {
            continue;
        }
        let subnet = f
            .lan
            .iter()
            .find(|n| n.contains(*ip))
            .map(|n| n.to_string())
            .unwrap_or_default();
        out.push(UpsNetAddress {
            address: ip.to_string(),
            subnet,
        });
    }
    // IPv4 first
    out.sort_by_key(|a| a.address.parse::<IpAddr>().map(|i| i.is_ipv6()).unwrap_or(true));
    let host = f.primary_v4.map(|a| a.to_string()).or_else(|| out.first().map(|a| a.address.clone()));
    (out, host)
}

fn net_state(u: &Ups, owner: &str) -> Res<UpsNetServerState> {
    let (applied, pending) = docs(u, owner)?;
    let applied = applied.map(for_answer);
    let pending = pending.map(for_answer).filter(|p| Some(p) != applied.as_ref());
    let read = layer_state(u);
    let blocked = blocked(u, read.as_ref());
    let facts = if blocked.is_none() {
        (crate::web_access::WebAccessSvc::system_facts())()
    } else {
        Default::default()
    };
    let (addresses, host) = addresses(&facts);
    if let (None, Some(read)) = (blocked, read) {
        let current = pending.as_ref().or(applied.as_ref());
        let mut problems = Vec::new();
        let mut filter_active = None;
        let layer = match read {
            NetLayerRead::Status(s) => {
                if s.enabled {
                    filter_active = Some(s.firewall_active);
                    if !s.server_active {
                        problems.push(UpsNetProblem::ServerInactive);
                    }
                    if !s.firewall_active {
                        problems.push(UpsNetProblem::FilterInactive);
                    }
                }
                let ours: Vec<&str> =
                    current.map(|c| c.clients.iter().map(|cl| cl.name.as_str()).collect()).unwrap_or_default();
                UpsNetLayer {
                    enabled: s.enabled,
                    outside: s.users.iter().map(|(n, _)| n.clone()).filter(|n| !ours.contains(&n.as_str())).collect(),
                    listen: s.listen,
                    allow: s.allow,
                    primary_account: s.primary_account,
                    source: s.source,
                }
            }
            _ => {
                problems.push(UpsNetProblem::LayerUnreadable);
                UpsNetLayer::default()
            }
        };
        return Ok(UpsNetServerState {
            filter_active,
            problems,
            blocked,
            applied,
            pending,
            addresses,
            host,
            hostname: facts.host(),
            port: NUT_PORT,
            ups_name: ups_name(u),
            layer: Some(layer),
        });
    }
    let on = blocked.is_none() && applied.as_ref().is_some_and(|a| a.enabled);
    let sys = u.ops.config.system();
    let running = on && sys.unit_active(SERVER_UNIT).unwrap_or(false);
    let filter_active = applied
        .as_ref()
        .filter(|a| running && !a.allow.is_empty())
        .and_then(|_| filter_state(sys));
    let mut problems = Vec::new();
    if on && !running {
        problems.push(UpsNetProblem::ServerInactive);
    }
    if filter_active == Some(false) {
        problems.push(UpsNetProblem::FilterInactive);
    }
    Ok(UpsNetServerState {
        filter_active,
        problems,
        blocked,
        applied,
        pending,
        addresses,
        host,
        hostname: facts.host(),
        port: NUT_PORT,
        ups_name: if blocked.is_none() { ups_name(u) } else { None },
        layer: None,
    })
}

async fn state_get(State(st): State<UpsState>, hd: HeaderMap) -> Res<Json<UpsNetServerState>> {
    let (u, owner) = (ups(&st)?, user(&hd)?);
    Ok(Json(blocking(&u, move |u| net_state(u, &owner)).await?))
}

fn refuse_blocked(u: &Ups) -> Res<()> {
    match blocked(u, layer_state(u).as_ref()) {
        Some(b) => Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            match b {
                UpsNetBlock::NutMissing => "NUT is not installed (Debian package nut)",
                UpsNetBlock::Module => "the hardware module's UPS layer runs NUT here",
                UpsNetBlock::ModuleTooOld => {
                    "the hardware module's UPS layer is too old for the network UPS server: update it"
                }
            }
            .into(),
        )),
        None => Ok(()),
    }
}

async fn state_put(
    State(st): State<UpsState>,
    hd: HeaderMap,
    body: Result<Json<UpsNetServerConfig>, JsonRejection>,
) -> Res<Json<UpsNetServerSaved>> {
    let (u, owner) = (ups(&st)?, user(&hd)?);
    let Json(mut req) = body?;
    Ok(Json(
        blocking(&u, move |u| {
            refuse_blocked(u)?;
            let layer = net_layer(u);
            // a layer's /etc/nut is its own: its users are not rynas's concern
            let foreign = if layer.is_some() { Vec::new() } else { foreign_users(&u.root) };
            net::normalize(&mut req, &foreign).map_err(|(f, m)| Error::invalid(f, m))?;
            if let Some(n) = layer {
                req.listen.clear();
                n.check(&req).map_err(|(f, m)| Error::invalid(f, m))?;
            }
            let (applied, pending) = docs(u, &owner)?;
            let kept: Vec<String> = applied
                .iter()
                .chain(pending.iter())
                .flat_map(|c| c.listen.iter().cloned())
                .collect();
            if !req.listen.is_empty() {
                let facts = (crate::web_access::WebAccessSvc::system_facts())();
                check_listen_owned(
                    &req.listen,
                    &facts.addrs.iter().copied().collect::<Vec<_>>(),
                    &kept,
                )
                    .map_err(|(f, m)| Error::invalid(f, m))?;
            }
            let previous: Vec<&UpsNetServerConfig> =
                pending.iter().chain(applied.iter()).collect();
            let sb = secret_box(&u.net_secret_dir).map_err(Error::internal)?;
            let passwords = seal_input(&mut req, &previous, &sb, &|| {
                crate::ups::new_password().map_err(|e| e.to_string())
            })?;
            crate::services::stage(&u.ops.config, &owner, NETSERVER_KEY, &req)?;
            Ok(UpsNetServerSaved {
                state: net_state(u, &owner)?,
                passwords,
            })
        })
        .await?,
    ))
}

async fn plan_post(
    State(st): State<UpsState>,
    body: Result<Json<UpsNetServerConfig>, JsonRejection>,
) -> Res<Json<UpsSetupPlan>> {
    let u = ups(&st)?;
    let Json(mut req) = body?;
    net::normalize(&mut req, &[]).map_err(|(f, m)| Error::invalid(f, m))?;
    Ok(Json(
        blocking(&u, move |u| {
            refuse_blocked(u)?;
            if let Some(n) = net_layer(u) {
                req.listen.clear();
                n.check(&req).map_err(|(f, m)| Error::invalid(f, m))?;
                return Ok(layer_plan(n, &req));
            }
            Ok(plan(&u.root, &req))
        })
        .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nut_root() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let etc = d.path().join("etc/nut");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("nut.conf"), "MODE=standalone\n").unwrap();
        std::fs::write(
            etc.join("upsd.conf"),
            "LISTEN 127.0.0.1 3493\nMAXAGE 15\n",
        )
        .unwrap();
        std::fs::write(
            etc.join("upsd.users"),
            "[admin]\n\tpassword = mine\n\tactions = SET\n",
        )
        .unwrap();
        d
    }

    fn input() -> UpsNetServerConfig {
        UpsNetServerConfig {
            enabled: true,
            listen: vec![],
            allow: vec!["192.168.1.0/24".into()],
            clients: vec![
                UpsNetClient {
                    name: "pc-office".into(),
                    ..Default::default()
                },
                UpsNetClient {
                    name: "homeassistant".into(),
                    role: UpsClientRole::Monitor,
                    password: Some("Chosen.Password-1".into()),
                    ..Default::default()
                },
            ],
        }
    }

    fn open(
        root: &Path,
        secrets: &Path,
    ) -> (
        tempfile::TempDir,
        rynas_config::Config,
        Arc<rynas_config::MemorySystem>,
    ) {
        open_with(root, secrets, Some(Arc::new(|_| true)), Vec::new())
    }

    fn open_with(
        root: &Path,
        secrets: &Path,
        probe: Option<Probe>,
        more: Vec<Arc<dyn Applier>>,
    ) -> (
        tempfile::TempDir,
        rynas_config::Config,
        Arc<rynas_config::MemorySystem>,
    ) {
        let state = tempfile::tempdir().unwrap();
        let sys = Arc::new(rynas_config::MemorySystem::new("nas"));
        let mut domains: Vec<Arc<dyn Applier>> = vec![Arc::new(NetServerDomain {
            layer: None,
            secret_dir: secrets.to_path_buf(),
            probe,
            runner: Arc::new(crate::ups_setup::DryRunner::default()),
        })];
        domains.extend(more);
        let cfg = rynas_config::Config::open(
            rynas_config::ConfigOptions::new(state.path(), root),
            sys.clone(),
            domains,
        )
        .unwrap();
        (state, cfg, sys)
    }

    #[test]
    fn passwords_are_sealed_shown_once_and_carried_forward() {
        let keys = tempfile::tempdir().unwrap();
        let sb = secret_box(keys.path()).unwrap();
        let mut c = input();
        let shown = seal_input(&mut c, &[], &sb, &|| Ok("GeneratedPassword0123456789abcdef".into()))
            .unwrap();
        assert_eq!(
            shown.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["pc-office", "homeassistant"]
        );
        assert_eq!(shown[1].password, "Chosen.Password-1");
        assert!(c.clients.iter().all(|cl| cl.password.is_none() && cl.sealed.is_some()));
        let doc = serde_json::to_string(&c).unwrap();
        assert!(!doc.contains("Chosen.Password-1") && !doc.contains("GeneratedPassword"));
        assert_eq!(unseal(&sb, &c.clients[1]).unwrap(), "Chosen.Password-1");
        // a later save without passwords keeps them; generate makes a new one
        let mut next = for_answer(c.clone());
        next.clients[0].generate = true;
        let shown = seal_input(&mut next, &[&c], &sb, &|| Ok("AnotherGeneratedPassword987654321".into()))
            .unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(unseal(&sb, &next.clients[1]).unwrap(), "Chosen.Password-1");
        assert_eq!(
            unseal(&sb, &next.clients[0]).unwrap(),
            "AnotherGeneratedPassword987654321"
        );
        // a client cannot plant an envelope: it is replaced by the stored one
        let mut planted = for_answer(c.clone());
        planted.clients[1].sealed = Some("Zm9v".into());
        seal_input(&mut planted, &[&c], &sb, &|| Ok("x".repeat(32))).unwrap();
        assert_eq!(planted.clients[1].sealed, c.clients[1].sealed);
        assert!(for_answer(c).clients.iter().all(|cl| cl.sealed.is_none()));
    }

    #[test]
    fn apply_writes_listen_users_and_filter_then_turns_off() {
        let root = nut_root();
        let keys = tempfile::tempdir().unwrap();
        let sb = secret_box(keys.path()).unwrap();
        let (_s, cfg, sys) = open(root.path(), keys.path());
        let mut c = input();
        net::normalize(&mut c, &foreign_users(root.path())).unwrap();
        let shown = seal_input(&mut c, &[], &sb, &|| Ok("GeneratedPassword0123456789abcdef".into()))
            .unwrap();
        let p = plan(root.path(), &c);
        assert!(p.files.iter().all(|f| !f.after.contains("GeneratedPassword")));
        let conf = p.files.iter().find(|f| f.path == UPSD_CONF).unwrap();
        assert_eq!(conf.removed, ["LISTEN 127.0.0.1 3493"]);
        cfg.stage("admin", NETSERVER_KEY, Some(serde_json::to_value(&c).unwrap()))
            .unwrap();
        cfg.apply("admin", "admin", "ups", &|_| {}, &|| false).unwrap();
        let rd = |p: &str| std::fs::read_to_string(root.path().join(p)).unwrap();
        let upsd = rd("etc/nut/upsd.conf");
        assert!(upsd.contains("LISTEN 0.0.0.0 3493\nLISTEN :: 3493\n"), "{upsd}");
        assert!(upsd.contains("MAXAGE 15"));
        assert!(
            !upsd.lines().any(|l| l == "LISTEN 127.0.0.1 3493"),
            "the admin's LISTEN is set aside: {upsd}"
        );
        assert!(upsd.contains("\n# saved: LISTEN 127.0.0.1 3493\n"), "{upsd}");
        let users = rd("etc/nut/upsd.users");
        assert!(users.starts_with("[admin]\n"), "{users}");
        assert!(users.contains(&format!(
            "[pc-office]\n\tpassword = \"{}\"\n\tupsmon secondary\n",
            shown[0].password
        )));
        assert!(users.contains("[homeassistant]\n\tpassword = \"Chosen.Password-1\"\n# END"));
        let dropin = rd("etc/systemd/system/nut-server.service.d/rynas-netserver.conf");
        assert!(dropin.contains("IPAddressAllow=192.168.1.0/24"));
        assert!(dropin.contains("IPAccounting=yes\n"));
        let calls = std::mem::take(&mut sys.state().calls);
        assert!(calls.iter().any(|c| c.contains("daemon-reload")), "{calls:?}");
        assert!(calls.iter().any(|c| c.contains("restart nut-server.service")), "{calls:?}");
        // only the accounts change: a reload, no restart, no daemon-reload
        let mut fewer = c.clone();
        fewer.clients.pop();
        cfg.stage("admin", NETSERVER_KEY, Some(serde_json::to_value(&fewer).unwrap()))
            .unwrap();
        cfg.apply("admin", "admin", "ups", &|_| {}, &|| false).unwrap();
        let calls = std::mem::take(&mut sys.state().calls);
        assert_eq!(
            calls.iter().filter(|c| !c.contains("systemctl show")).collect::<Vec<_>>(),
            ["reload nut-server.service"]
        );
        assert!(!rd("etc/nut/upsd.users").contains("homeassistant"));
        // off: LISTEN lines, accounts and the filter go; the admin's LISTEN comes back
        let mut off = c.clone();
        off.enabled = false;
        cfg.stage("admin", NETSERVER_KEY, Some(serde_json::to_value(&off).unwrap()))
            .unwrap();
        cfg.apply("admin", "admin", "ups", &|_| {}, &|| false).unwrap();
        let upsd = rd("etc/nut/upsd.conf");
        assert!(!upsd.contains("LISTEN 0.0.0.0"));
        assert!(upsd.lines().any(|l| l == "LISTEN 127.0.0.1 3493"), "{upsd}");
        assert!(!upsd.contains("# saved:"));
        assert!(sys.state().calls.iter().any(|c| c.contains("restart nut-server.service")));
        let users = rd("etc/nut/upsd.users");
        assert_eq!(users, "[admin]\n\tpassword = mine\n\tactions = SET\n");
        assert!(!root
            .path()
            .join("etc/systemd/system/nut-server.service.d/rynas-netserver.conf")
            .exists());
    }

    fn sealed(keys: &Path, c: UpsNetServerConfig) -> UpsNetServerConfig {
        let sb = secret_box(keys).unwrap();
        let mut c = c;
        net::normalize(&mut c, &[]).unwrap();
        seal_input(&mut c, &[], &sb, &|| Ok("GeneratedPassword0123456789abcdef".into())).unwrap();
        c
    }

    fn apply_value(cfg: &rynas_config::Config, v: Option<&UpsNetServerConfig>) -> Result<u64, String> {
        cfg.stage("admin", NETSERVER_KEY, v.map(|v| serde_json::to_value(v).unwrap()))
            .map_err(|e| e.to_string())?;
        cfg.apply("admin", "admin", "ups", &|_| {}, &|| false)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn removing_the_key_closes_the_server() {
        let root = nut_root();
        let keys = tempfile::tempdir().unwrap();
        let (_s, cfg, sys) = open(root.path(), keys.path());
        let mut c = sealed(keys.path(), input());
        c.listen = vec!["192.168.1.10".into()];
        apply_value(&cfg, Some(&c)).unwrap();
        let rd = |p: &str| std::fs::read_to_string(root.path().join(p)).unwrap();
        assert!(rd("etc/nut/upsd.conf").contains("LISTEN 192.168.1.10 3493"));
        let dropin = rd("etc/systemd/system/nut-server.service.d/rynas-netserver.conf");
        assert!(dropin.contains("After=network-online.target"), "{dropin}");
        sys.state().calls.clear();
        // what restoring a revision from before the key existed stages
        apply_value(&cfg, None).unwrap();
        assert_eq!(cfg.applied(NETSERVER_KEY).unwrap(), None);
        let upsd = rd("etc/nut/upsd.conf");
        assert!(!upsd.contains("192.168.1.10") && !upsd.contains("::1"), "{upsd}");
        assert!(upsd.lines().any(|l| l == "LISTEN 127.0.0.1 3493"), "admin's back: {upsd}");
        assert_eq!(rd("etc/nut/upsd.users"), "[admin]\n\tpassword = mine\n\tactions = SET\n");
        assert!(!root
            .path()
            .join("etc/systemd/system/nut-server.service.d/rynas-netserver.conf")
            .exists());
        let calls = sys.state().calls.clone();
        assert!(calls.iter().any(|c| c.contains("daemon-reload")), "{calls:?}");
        assert!(calls.iter().any(|c| c.contains("restart nut-server.service")), "{calls:?}");
        assert!(cfg.drift().unwrap().entries.is_empty());
    }

    #[test]
    fn a_stored_password_with_a_quote_is_refused_and_rolled_back() {
        let root = nut_root();
        let keys = tempfile::tempdir().unwrap();
        let (_s, cfg, _sys) = open(root.path(), keys.path());
        let mut c = sealed(keys.path(), input());
        let sb = secret_box(keys.path()).unwrap();
        // an envelope from an older rynas or a tampered revision
        c.clients[0].sealed = Some(seal(&sb, "evil\"\n[admin2]\n\tactions = SET").unwrap());
        let before = std::fs::read_to_string(root.path().join("etc/nut/upsd.conf")).unwrap();
        let e = apply_value(&cfg, Some(&c)).unwrap_err();
        assert!(e.contains("cannot go into"), "{e}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("etc/nut/upsd.users")).unwrap(),
            "[admin]\n\tpassword = mine\n\tactions = SET\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("etc/nut/upsd.conf")).unwrap(),
            before,
            "rolled back"
        );
    }

    #[test]
    fn upsd_not_answering_rolls_back() {
        let root = nut_root();
        let keys = tempfile::tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let probe: Probe = Arc::new(move |a| {
            s2.lock().unwrap().push(a);
            false
        });
        let (_s, cfg, _sys) = open_with(root.path(), keys.path(), Some(probe), Vec::new());
        let e = apply_value(&cfg, Some(&sealed(keys.path(), input()))).unwrap_err();
        assert!(e.contains("does not answer on 127.0.0.1:3493"), "{e}");
        assert_eq!(seen.lock().unwrap()[0], SocketAddr::from(([127, 0, 0, 1], 3493)));
        assert_eq!(
            std::fs::read_to_string(root.path().join("etc/nut/upsd.conf")).unwrap(),
            "LISTEN 127.0.0.1 3493\nMAXAGE 15\n"
        );
        assert!(!std::fs::read_to_string(root.path().join("etc/nut/upsd.users"))
            .unwrap()
            .contains("pc-office"));
    }

    /// Restarts upsd like `ups.device` does (order 45, before `ups.netserver`).
    struct FakeDevice;

    impl Applier for FakeDevice {
        fn key(&self) -> &str {
            "ups.device"
        }
        fn order(&self) -> i32 {
            45
        }
        fn validate(&self, value: Value) -> Result<Value, Invalid> {
            Ok(value)
        }
        fn render(&self, _: &Value, _: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
            Ok(Vec::new())
        }
        fn reconcile(&self, _: Option<&Value>, _: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
            ctx.restart_unit(SERVER_UNIT)
        }
    }

    #[test]
    fn applied_with_the_device_upsd_restarts_once() {
        let root = nut_root();
        let keys = tempfile::tempdir().unwrap();
        let (_s, cfg, sys) =
            open_with(root.path(), keys.path(), Some(Arc::new(|_| true)), vec![Arc::new(FakeDevice)]);
        let mut c = sealed(keys.path(), input());
        c.listen = vec!["192.168.1.10".into()];
        apply_value(&cfg, Some(&c)).unwrap();
        sys.state().calls.clear();
        // a new address: upsd.conf changes, the drop-in does not
        c.listen.push("192.168.2.10".into());
        cfg.stage("admin", "ups.device", Some(serde_json::json!({"n": 1}))).unwrap();
        apply_value(&cfg, Some(&c)).unwrap();
        let calls = sys.state().calls.clone();
        assert_eq!(
            calls.iter().filter(|c| c.contains("restart nut-server")).count(),
            1,
            "{calls:?}"
        );
        assert!(!calls.iter().any(|c| c.contains("daemon-reload")), "{calls:?}");
    }

    #[test]
    fn drift_of_the_accounts_block_is_reported() {
        let root = nut_root();
        let keys = tempfile::tempdir().unwrap();
        let (_s, cfg, _sys) = open(root.path(), keys.path());
        apply_value(&cfg, Some(&sealed(keys.path(), input()))).unwrap();
        assert!(cfg.drift().unwrap().state.is_empty());
        let users = root.path().join("etc/nut/upsd.users");
        let text = std::fs::read_to_string(&users).unwrap();
        std::fs::write(&users, text.replace("\tupsmon secondary\n", "\tupsmon primary\n")).unwrap();
        let d = cfg.drift().unwrap();
        assert_eq!(d.state.len(), 1, "{d:?}");
        assert_eq!(d.state[0].key, NETSERVER_KEY);
        assert_eq!(d.state[0].kind, StateDriftKind::Mismatch);
        // overwrite puts the block back
        cfg.drift_overwrite(NETSERVER_KEY, &|_| {}).unwrap();
        assert!(cfg.drift().unwrap().state.is_empty());
        assert!(std::fs::read_to_string(&users).unwrap().contains("upsmon secondary"));
    }

    #[test]
    fn listen_addresses_must_be_the_nas_own_and_keys_stay_private() {
        let addrs: Vec<IpAddr> = vec!["192.168.1.10".parse().unwrap()];
        assert!(check_listen_owned(&["192.168.1.10".into()], &addrs, &[]).is_ok());
        assert_eq!(
            check_listen_owned(&["192.168.1.10".into(), "10.9.9.9".into()], &addrs, &[])
                .unwrap_err()
                .0,
            "listen.1"
        );
        assert!(check_listen_owned(&["10.9.9.9".into()], &addrs, &["10.9.9.9".into()]).is_ok());
        assert!(check_listen_owned(&["10.9.9.9".into()], &[], &[]).is_ok(), "unknown: no check");
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("notify");
        secret_box(&dir).unwrap();
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn staged_values_need_sealed_passwords_and_admin_users_win() {
        let root = nut_root();
        let keys = tempfile::tempdir().unwrap();
        let (_s, cfg, _sys) = open(root.path(), keys.path());
        // a raw password never reaches the configuration
        let e = cfg
            .stage("admin", NETSERVER_KEY, Some(serde_json::to_value(input()).unwrap()))
            .unwrap_err();
        assert!(e.to_string().contains("password"), "{e}");
        let mut c = input();
        c.clients[0].name = "admin".into();
        assert_eq!(
            net::normalize(&mut c, &foreign_users(root.path())).unwrap_err().0,
            "clients.0.name"
        );
    }

    #[test]
    fn listen_choices_skip_loopback_and_link_local() {
        let f = crate::web_access::model::parse_facts(
            "NAS",
            r#"[{"ifname":"lo","link_type":"loopback","addr_info":[{"local":"127.0.0.1","prefixlen":8}]},
                {"ifname":"eth0","addr_info":[{"local":"fe80::1","prefixlen":64},{"local":"fd00::10","prefixlen":64},{"local":"192.168.1.10","prefixlen":24}]}]"#,
            r#"[{"dev":"eth0"}]"#,
        );
        let (a, host) = addresses(&f);
        assert_eq!(
            a.iter().map(|a| a.address.as_str()).collect::<Vec<_>>(),
            ["192.168.1.10", "fd00::10"]
        );
        assert_eq!(a[0].subnet, "192.168.1.0/24");
        assert_eq!(host.as_deref(), Some("192.168.1.10"));
    }

    /// A layer's server: `set`/`off`/`status` lines, answered in turn.
    struct FakeNet;

    impl NetServerLayer for FakeNet {
        fn status_cmd(&self) -> Cmd {
            Cmd::new("tool").args(["netserver", "status"])
        }
        fn parse_status(&self, out: &rynas_tasks::CmdOutput) -> NetLayerRead {
            match out.stdout.trim() {
                "old" => NetLayerRead::Unsupported,
                "" => NetLayerRead::Unreadable("no answer".into()),
                users => NetLayerRead::Status(NetLayerStatus {
                    enabled: true,
                    users: users
                        .split(',')
                        .filter(|u| *u != "-")
                        .map(|u| (u.to_string(), UpsClientRole::Secondary))
                        .collect(),
                    ..Default::default()
                }),
            }
        }
        fn check(&self, c: &UpsNetServerConfig) -> Result<(), (String, String)> {
            if c.enabled && c.allow.is_empty() {
                return Err(("allow".into(), "required".into()));
            }
            Ok(())
        }
        fn set_cmd(
            &self,
            allow: &[String],
            users: &[(String, UpsClientRole)],
            passwords: &[(String, String)],
        ) -> Cmd {
            let mut c = Cmd::new("tool").args(["netserver", "set"]).args(allow);
            for (u, _) in users {
                c = c.args(["--user", u]);
            }
            c.stdin(passwords.iter().map(|(n, p)| format!("{n} {p}\n")).collect::<String>())
        }
        fn off_cmd(&self) -> Cmd {
            Cmd::new("tool").args(["netserver", "off"])
        }
    }

    struct Script {
        status: String,
        set_code: i32,
        calls: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl Runner for Script {
        fn run(&self, cmd: &Cmd) -> Result<rynas_tasks::CmdOutput, String> {
            let line = cmd.display();
            let stdin = String::from_utf8_lossy(cmd.stdin_data().unwrap_or_default()).into_owned();
            self.calls.lock().unwrap().push((line.clone(), stdin));
            let status = line.contains("status");
            Ok(rynas_tasks::CmdOutput {
                code: Some(if status { 0 } else { self.set_code }),
                stdout: if status { self.status.clone() } else { "applied\n".into() },
                stderr: if self.set_code == 2 { "invalid client 300.1.1.1\n".into() } else { String::new() },
            })
        }
    }

    #[test]
    fn a_layer_gets_the_set_with_only_new_or_changed_passwords_on_stdin() {
        let keys = tempfile::tempdir().unwrap();
        let mut c = sealed(keys.path(), input());
        let v = |c: &UpsNetServerConfig| serde_json::to_value(c).unwrap();
        let run = |status: &str, code: i32, from: Option<&UpsNetServerConfig>, to: Option<&UpsNetServerConfig>| {
            let r = Script { status: status.into(), set_code: code, calls: Default::default() };
            let logged = std::sync::Mutex::new(Vec::<String>::new());
            let res = layer_apply(&FakeNet, &r, keys.path(), from.map(v).as_ref(), to.map(v).as_ref(), &|l| {
                logged.lock().unwrap().push(l.to_string())
            });
            let logs = logged.into_inner().unwrap().join("\n");
            (res, r.calls.into_inner().unwrap(), logs)
        };
        // first apply: both accounts new to the layer, both passwords on stdin, none in argv
        let (res, calls, logs) = run("-", 0, None, Some(&c));
        res.unwrap();
        let (line, stdin) = &calls[1];
        assert_eq!(line, "tool netserver set 192.168.1.0/24 --user pc-office --user homeassistant");
        assert!(stdin.starts_with("pc-office ") && stdin.contains("\nhomeassistant Chosen.Password-1\n"), "{stdin}");
        assert!(!logs.contains("Chosen.Password-1"), "{logs}");
        // the layer has both and nothing changed: no password goes
        let (res, calls, _) = run("pc-office,homeassistant", 0, Some(&c), Some(&c));
        res.unwrap();
        assert_eq!(calls[1].1, "");
        // a new password for one account; the other one lost by the layer (an off by hand)
        let before = c.clone();
        let sb = secret_box(keys.path()).unwrap();
        c.clients[0].sealed = Some(seal(&sb, "Another.Password-2").unwrap());
        let (_, calls, _) = run("pc-office", 0, Some(&before), Some(&c));
        assert_eq!(calls[1].1, "pc-office Another.Password-2\nhomeassistant Chosen.Password-1\n");
        // an unreadable state: every password goes
        let (_, calls, _) = run("", 0, Some(&c), Some(&c));
        assert_eq!(calls[1].1.lines().count(), 2);
        // exit 2: refused with the layer's reason; 1: not applied
        let (res, _, _) = run("-", 2, None, Some(&c));
        assert!(res.unwrap_err().contains("refused the values: invalid client 300.1.1.1"));
        let (res, _, _) = run("-", 1, None, Some(&c));
        assert!(res.unwrap_err().contains("did not apply"));
        // off, or the key removed: the layer's off, no status read, no stdin
        let mut off = c.clone();
        off.enabled = false;
        for to in [Some(&off), None] {
            let (res, calls, _) = run("old", 0, Some(&c), to);
            res.unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0], ("tool netserver off".into(), String::new()));
        }
        // a layer without the server refuses the apply
        let (res, _, _) = run("old", 0, None, Some(&c));
        assert!(res.unwrap_err().contains("update"));
        // the plan shows the command, never a password
        let p = layer_plan(&FakeNet, &c);
        assert!(p.files.is_empty());
        assert_eq!(p.commands[0], "tool netserver set 192.168.1.0/24 --user pc-office --user homeassistant");
        assert!(p.commands[1].contains("standard input"));
        assert_eq!(layer_plan(&FakeNet, &off).commands, ["tool netserver off"]);
    }
}
