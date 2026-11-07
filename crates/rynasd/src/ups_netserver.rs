//! The network UPS server (`/v1/ups/netserver`, `/v1/ups/plan/netserver`) and the
//! `ups.netserver` configuration domain: other machines (servers and PCs running upsmon,
//! home automation, dashboards) read this NAS's UPS from upsd on port 3493 and shut down on
//! its low battery. Design and sources: `docs/research/ups.md` ("Network UPS server").
//!
//! - **Files** (plain NUT, rynas blocks only): `LISTEN` lines in `/etc/nut/upsd.conf` (the
//!   admin's own `LISTEN` lines are dropped while the server is on), the client accounts in
//!   `upsd.users` (block `ups-netserver`), and, with allowed hosts, a drop-in of
//!   `nut-server.service` with systemd's per-service IP filter. `upsd.users` already carries
//!   blocks of `ups.device` and of the battery tests and the engine keeps one owner per file,
//!   so the accounts block is written by [`NetServerDomain::reconcile`] (atomic, the file's
//!   mode and owner kept, written back with the old value on rollback), like the battery
//!   test user.
//! - **Passwords** arrive write-only (`password`, or `generate`; a new account without
//!   either gets a generated one) and are sealed at once with rynasd's age key (the
//!   notification engine's `<state>/notify/secret.key`, which a passphrase-protected export
//!   carries): the configuration key, its revisions and answers never hold them in clear.
//!   The `PUT` answer returns the passwords of that request once.
//! - **Restart**: upsd reads `LISTEN` only at start, so an apply restarts
//!   `nut-server.service` when it runs (upsmon on the NAS reconnects by itself).
//! - **A module's UPS layer** owns `/etc/nut`: the server is not offered there
//!   ([`UpsNetBlock::Module`]).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use rynas_api_types::error_codes;
use rynas_api_types::ups::*;
use rynas_config::files::{self, FileKind};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use rynas_module::UpsLayer;
use rynas_notify::engine::secrets::SecretBox;
use rynas_nut::netserver as net;
use serde_json::Value;

use crate::ops::{Error, Res, user};
use crate::ups::{Ups, UpsState};
use crate::ups_setup::{DEVICE_KEY, NUT_USERS, foreign_sections};

pub const NETSERVER_KEY: &str = "ups.netserver";
pub const UPSD_CONF: &str = "/etc/nut/upsd.conf";
pub const DROPIN: &str = "/etc/systemd/system/nut-server.service.d/rynas-netserver.conf";
pub const SERVER_UNIT: &str = "nut-server.service";
const BLOCK: &str = "ups-netserver";
const MASK: &str = "********";

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// rynasd's secret key (created on first use, folder 0700).
pub fn secret_box(dir: &Path) -> Result<SecretBox, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("secret store: {e}"))?;
    SecretBox::open(dir).map_err(|e| format!("secret store: {e}"))
}

fn seal(sb: &SecretBox, pw: &str) -> Result<String, String> {
    let m = [("password".to_string(), pw.to_string())].into();
    sb.seal(&m).map(|s| b64().encode(s))
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

/// The files the engine writes for `c` (plain NUT).
pub fn nut_files(c: &UpsNetServerConfig) -> Vec<ManagedFile> {
    let mut upsd = ManagedFile::block(UPSD_CONF, BLOCK, net::render_upsd_conf(c))
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
    files::write_synced(&cand, new.as_bytes(), meta.mode() & 0o7777)
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
        "systemctl daemon-reload".into(),
        format!("systemctl restart {SERVER_UNIT} (when it runs)"),
    ]
}

// ---------------------------------------------------------------------------------------------
// the configuration domain
// ---------------------------------------------------------------------------------------------

/// `ups.netserver`.
pub struct NetServerDomain {
    pub layer: Option<Arc<dyn UpsLayer>>,
    /// Folder of rynasd's secret key (`<state>/notify`).
    pub secret_dir: PathBuf,
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
        Ok(nut_files(&c))
    }

    fn reconcile(
        &self,
        _from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        if self.layer.is_some() {
            return Ok(());
        }
        let block = match to {
            Some(v) => {
                let c = config_of(v).map_err(|e| e.message)?;
                let sb = if c.enabled && !c.clients.is_empty() {
                    Some(secret_box(&self.secret_dir)?)
                } else {
                    None
                };
                users_block(&c, &|cl| match &sb {
                    Some(sb) => unseal(sb, cl),
                    None => Err("no secret store".into()),
                })?
            }
            None => None,
        };
        if write_users(ctx.root, block.as_deref())? {
            (ctx.log)(&format!("updated the network UPS server accounts in {NUT_USERS}"));
        }
        (ctx.log)("$ systemctl daemon-reload");
        ctx.system.daemon_reload()?;
        if ctx.system.unit_active(SERVER_UNIT).unwrap_or(false) {
            (ctx.log)(&format!("$ systemctl restart {SERVER_UNIT}"));
            ctx.system.restart_unit(SERVER_UNIT)?;
        } else {
            (ctx.log)(&format!(
                "{SERVER_UNIT} is not running: the settings take effect when it starts"
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// the plan
// ---------------------------------------------------------------------------------------------

/// What applying `c` does (no side effects; passwords masked).
pub fn plan(root: &Path, c: &UpsNetServerConfig) -> UpsSetupPlan {
    let mut out: Vec<UpsPlanFile> = nut_files(c)
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

fn blocked(u: &Ups) -> Option<UpsNetBlock> {
    if u.layer.is_some() {
        Some(UpsNetBlock::Module)
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
    let blocked = blocked(u);
    let facts = if blocked.is_none() {
        (crate::web_access::WebAccessSvc::system_facts())()
    } else {
        Default::default()
    };
    let (addresses, host) = addresses(&facts);
    Ok(UpsNetServerState {
        blocked,
        applied,
        pending,
        addresses,
        host,
        hostname: facts.host(),
        port: NUT_PORT,
        ups_name: if blocked.is_none() { ups_name(u) } else { None },
    })
}

async fn state_get(State(st): State<UpsState>, hd: HeaderMap) -> Res<Json<UpsNetServerState>> {
    let (u, owner) = (ups(&st)?, user(&hd)?);
    Ok(Json(blocking(&u, move |u| net_state(u, &owner)).await?))
}

fn refuse_blocked(u: &Ups) -> Res<()> {
    match blocked(u) {
        Some(b) => Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            match b {
                UpsNetBlock::NutMissing => "NUT is not installed (Debian package nut)",
                UpsNetBlock::Module => "the hardware module's UPS layer runs NUT here",
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
            net::normalize(&mut req, &foreign_users(&u.root))
                .map_err(|(f, m)| Error::invalid(f, m))?;
            let (applied, pending) = docs(u, &owner)?;
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
            "# LISTEN 127.0.0.1 3493\nLISTEN 127.0.0.1 3493\nMAXAGE 15\n",
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
        let state = tempfile::tempdir().unwrap();
        let sys = Arc::new(rynas_config::MemorySystem::new("nas"));
        let domains: Vec<Arc<dyn Applier>> = vec![Arc::new(NetServerDomain {
            layer: None,
            secret_dir: secrets.to_path_buf(),
        })];
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
        assert!(upsd.contains("# LISTEN 127.0.0.1 3493\n") && upsd.contains("MAXAGE 15"));
        assert!(!upsd.contains("\nLISTEN 127.0.0.1"), "the admin's LISTEN goes: {upsd}");
        let users = rd("etc/nut/upsd.users");
        assert!(users.starts_with("[admin]\n"), "{users}");
        assert!(users.contains(&format!(
            "[pc-office]\n\tpassword = \"{}\"\n\tupsmon secondary\n",
            shown[0].password
        )));
        assert!(users.contains("[homeassistant]\n\tpassword = \"Chosen.Password-1\"\n# END"));
        let dropin = rd("etc/systemd/system/nut-server.service.d/rynas-netserver.conf");
        assert!(dropin.contains("IPAddressAllow=192.168.1.0/24"));
        let calls = sys.state().calls.clone();
        assert!(calls.iter().any(|c| c.contains("daemon-reload")), "{calls:?}");
        assert!(calls.iter().any(|c| c.contains("restart nut-server.service")), "{calls:?}");
        // off: LISTEN lines, accounts and the filter go
        let mut off = c.clone();
        off.enabled = false;
        cfg.stage("admin", NETSERVER_KEY, Some(serde_json::to_value(&off).unwrap()))
            .unwrap();
        cfg.apply("admin", "admin", "ups", &|_| {}, &|| false).unwrap();
        assert!(!rd("etc/nut/upsd.conf").contains("LISTEN 0.0.0.0"));
        let users = rd("etc/nut/upsd.users");
        assert_eq!(users, "[admin]\n\tpassword = mine\n\tactions = SET\n");
        assert!(!root
            .path()
            .join("etc/systemd/system/nut-server.service.d/rynas-netserver.conf")
            .exists());
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
}
