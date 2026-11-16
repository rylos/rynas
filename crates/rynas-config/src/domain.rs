//! Configuration domains: the [`Applier`] trait and the built-in domains.
//!
//! A domain owns one configuration key (a JSON document such as `system.hostname`) and
//! knows how to validate it, render it to files (minijinja templates), check the rendered
//! files, reload the services that read them, verify the result and read the current state
//! of a system it never managed (import).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rynas_api_types::{HostnameConfig, StateDrift};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::files::{FileKind, ManagedFile, under_root};
use crate::system::System;

/// What an applier gets while it works.
pub struct ApplyCtx<'a> {
    /// Files are written below this directory ("/" in production).
    pub root: &'a Path,
    pub system: &'a dyn System,
    pub templates: &'a minijinja::Environment<'static>,
    /// Progress lines for the task log (a no-op outside an apply).
    pub log: &'a dyn Fn(&str),
    /// Real paths (below the root) of the files this apply (or rollback) replaced or removed.
    pub(crate) replaced: Mutex<Vec<PathBuf>>,
    /// Units restarted through [`ApplyCtx::restart_unit`] during this apply (or rollback).
    pub(crate) restarted: Mutex<Vec<String>>,
    /// The reconcile puts a failed apply's previous values back ([`ApplyCtx::rolling_back`]).
    pub(crate) rolling_back: bool,
    /// The host name this apply puts in force, when it changes `system.hostname`
    /// ([`ApplyCtx::hostname`]).
    pub(crate) new_hostname: Option<String>,
    /// This rollback puts the previous host name back ([`ApplyCtx::hostname_changes`]).
    pub(crate) hostname_reverted: bool,
    /// Units to restart once every domain has reloaded ([`ApplyCtx::restart_at_end`]).
    pub(crate) at_end: Mutex<Vec<String>>,
}

impl ApplyCtx<'_> {
    /// The host name the rendered files must name: the one this apply puts in force when it
    /// changes `system.hostname` (files are rendered before the new name is set), else the
    /// running one.
    pub fn hostname(&self) -> Result<String, String> {
        match &self.new_hostname {
            Some(h) => Ok(h.clone()),
            None => self.system.hostname(),
        }
    }

    /// Whether this apply changes the host name, or this rollback puts the previous one
    /// back: services that announce it (NetBIOS, WS discovery) restart to take the name in
    /// force ([`ApplyCtx::restart_at_end`]).
    pub fn hostname_changes(&self) -> bool {
        self.new_hostname.is_some() || self.hostname_reverted
    }

    /// Restarts `unit` (when active) after every domain of this apply or rollback has
    /// reloaded: in a rollback the host name is put back after the domains that follow it,
    /// so a service that reads it at start must start after that. A failure is logged, never
    /// fails the apply.
    pub fn restart_at_end(&self, unit: &str) {
        let mut v = self.at_end.lock().unwrap();
        if !v.iter().any(|u| u == unit) {
            v.push(unit.to_string());
        }
    }

    /// Whether this reconcile is the rollback of a failed apply: a domain does only what
    /// puts the previous configuration in force again and skips slow side work (it ran, or
    /// was skipped, when that configuration was applied).
    pub fn rolling_back(&self) -> bool {
        self.rolling_back
    }

    /// Whether this apply replaced or removed the system file `abs` (its content changed):
    /// a domain restarts its service only then. A rollback counts the files it put back.
    pub fn replaced(&self, abs: &Path) -> bool {
        let real = self.path(abs);
        self.replaced.lock().unwrap().contains(&real)
    }

    /// `systemctl restart <unit>`, remembered for the rest of this apply
    /// ([`ApplyCtx::restarted`]).
    pub fn restart_unit(&self, unit: &str) -> Result<(), String> {
        self.system.restart_unit(unit)?;
        self.restarted.lock().unwrap().push(unit.to_string());
        Ok(())
    }

    /// Whether an earlier domain of this apply restarted `unit` ([`ApplyCtx::restart_unit`]):
    /// all files are in place before the first domain reloads, so a later domain whose own
    /// files the restart already read need not restart it again (e.g. `ups.device` and
    /// `ups.netserver` applied together restart upsd once).
    pub fn restarted(&self, unit: &str) -> bool {
        self.restarted.lock().unwrap().iter().any(|u| u == unit)
    }

    /// Renders a registered template (see [`Applier::templates`]).
    pub fn render(&self, template: &str, ctx: &Value) -> Result<String, String> {
        self.templates
            .get_template(template)
            .and_then(|t| t.render(minijinja::value::Serde(ctx)))
            .map_err(|e| format!("template {template}: {e}"))
    }

    /// Real location of a system path (below the root).
    pub fn path(&self, abs: &Path) -> PathBuf {
        under_root(self.root, abs)
    }

    /// Current content of a system file, `None` if it does not exist.
    pub fn read(&self, abs: &Path) -> Option<String> {
        std::fs::read_to_string(self.path(abs)).ok()
    }
}

/// A rendered file waiting next to its target as `<target>.rynas-new`.
pub struct Candidate<'a> {
    pub file: &'a ManagedFile,
    /// Where the candidate is on disk (below the root): give this path to validators.
    pub staged: PathBuf,
}

/// The behaviour of one configuration domain.
/// A value refused by [`Applier::validate`]: English message, and the field it is about
/// (named as in the JSON of the API request, e.g. `email`, `workgroup`) when known. The API
/// passes the field on as `ApiError.field`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub field: Option<String>,
    pub message: String,
}

impl Invalid {
    /// A rejection of the field `field`.
    pub fn at(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: Some(field.into()),
            message: message.into(),
        }
    }
}

impl From<String> for Invalid {
    fn from(message: String) -> Self {
        Self {
            field: None,
            message,
        }
    }
}

impl From<&str> for Invalid {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub trait Applier: Send + Sync {
    /// The configuration key, e.g. "system.hostname".
    fn key(&self) -> &str;

    /// Position of the domain in an apply that changes several keys: lower first (then by
    /// key). Rollback goes in reverse. E.g. `users` (10) runs before `shares` (20), whose
    /// ACLs name the users.
    fn order(&self) -> i32 {
        0
    }

    /// `(name, source)` of the minijinja templates used by [`Applier::render`]; names should
    /// start with the key.
    fn templates(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Checks a value and returns it normalized (the stored form). A rejection names the
    /// field of the value it is about when it can ([`Invalid::at`]).
    fn validate(&self, value: Value) -> Result<Value, Invalid>;

    /// Files for `value`.
    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String>;

    /// Files a removed key leaves behind (the key staged as `None`, e.g. by restoring a
    /// revision from before the key existed); `old` is the value being removed. Default:
    /// none, the files stay as they are and are no longer managed. A domain whose files
    /// would keep a service open or changed (a listening port, a firewall hole) returns what
    /// its "off" value renders, or [`FileKind::Absent`] files. Written like the files of a
    /// value (candidates, backup, rollback) but not tracked by the drift check.
    fn render_removed(
        &self,
        _old: Option<&Value>,
        _ctx: &ApplyCtx,
    ) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    /// Checks the rendered files before they replace the current ones (e.g. `testparm -s`).
    fn check_files(&self, _files: &[Candidate], _ctx: &ApplyCtx) -> Result<(), String> {
        Ok(())
    }

    /// Makes the system use the new files. `value` is `None` when the key was removed; also
    /// called with the previous value on rollback.
    fn reload(&self, _value: Option<&Value>, _ctx: &ApplyCtx) -> Result<(), String> {
        Ok(())
    }

    /// Brings the running system from the value `from` to the value `to` (`None`: no value).
    /// Called with `(old, new)` by an apply and with `(new, old)` by its rollback; a drift
    /// overwrite passes the applied value twice. The default reloads with `to`
    /// ([`Applier::reload`]); domains whose state is not only files (users, ACLs) override
    /// it to compute what to add, change and remove.
    fn reconcile(
        &self,
        _from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        self.reload(to, ctx)
    }

    /// Verifies the system after the reload; a failure rolls the apply back.
    fn health(&self, _value: Option<&Value>, _ctx: &ApplyCtx) -> Result<(), String> {
        Ok(())
    }

    /// Whether this domain's files depend on the value of `key` (e.g. the Samba NetBIOS name
    /// on `system.hostname`, read through [`ApplyCtx::hostname`]): an apply that changes
    /// `key` without this domain renders, writes and reconciles it again with its applied
    /// value, so its files never keep the old one. Such a pass stores no new value and leaves
    /// the owner's staged change of this domain pending. Default: none.
    fn follows(&self, _key: &str) -> bool {
        false
    }

    /// Makes the system read this domain's files written again because a key it
    /// [`follows`](Applier::follows) changed (`value`: its applied value, unchanged): only
    /// what reads the followed value (e.g. reload smbd, restart nmbd and wsdd2), never the
    /// whole [`reconcile`](Applier::reconcile) (folders, ACLs, exports). Also called by the
    /// rollback of that apply. A failure is a warning: it never fails nor rolls back the
    /// apply of the key followed. Default: the reconcile of `value` to itself.
    fn follow_reload(&self, value: &Value, ctx: &ApplyCtx) -> Result<(), String> {
        self.reconcile(Some(value), Some(value), ctx)
    }

    /// Called once the apply that changed this key from `old` to `new` is committed (never
    /// after a rollback): side effects outside the system files that must not run for a
    /// change that may still be rolled back (e.g. the daemon's reaction to a new host name).
    fn committed(&self, _old: Option<&Value>, _new: Option<&Value>) {}

    /// The current state of the system as a value of this domain, used when rynas has never
    /// applied the key and to import drifted files.
    fn import(&self, _ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        Ok(None)
    }

    /// Differences between the system and `applied` that are not files (e.g. users missing
    /// from the Samba database), shown by the drift report next to the drifted files.
    fn state_drift(
        &self,
        _applied: Option<&Value>,
        _ctx: &ApplyCtx,
    ) -> Result<Vec<StateDrift>, String> {
        Ok(Vec::new())
    }
}

/// `system.hostname`: `/etc/hostname`, the `127.0.1.1` entry of `/etc/hosts` (Debian
/// convention), and the running hostname through [`System::set_hostname`]. When the running
/// name changes, Avahi restarts (it does not follow the kernel's name: `<host>.local` would
/// keep answering with the old one) and the hook runs with the new name, so the services of
/// the daemon that hold the name (certificates, web access, notification texts) follow at
/// once the apply is committed (a rolled back rename never reaches them). Domains whose files
/// carry the name re-render through [`Applier::follows`].
#[derive(Default)]
pub struct HostnameDomain {
    on_change: Option<HostnameHook>,
    /// The name the running apply put in force, for the hook once it is committed.
    renamed: Mutex<Option<String>>,
}

/// Called with the new running host name once an apply that changed it is committed (never
/// for a rollback). Runs inside the apply: it must not apply configuration itself (wake a
/// task that does).
pub type HostnameHook = Box<dyn Fn(&str) + Send + Sync>;

/// The mDNS responder that publishes `<host>.local`.
pub const AVAHI_UNIT: &str = "avahi-daemon.service";

impl HostnameDomain {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `f` with the new name whenever an apply changes the running host name.
    pub fn on_change(mut self, f: HostnameHook) -> Self {
        self.on_change = Some(f);
        self
    }
}

pub const HOSTNAME_KEY: &str = "system.hostname";

/// RFC 1123 label, lowercase ([`rynas_api_types::validate::hostname`]).
pub use rynas_api_types::validate::hostname as valid_hostname;

fn hostname_of(v: &Value) -> Result<String, String> {
    let c: HostnameConfig =
        serde_json::from_value(v.clone()).map_err(|e| format!("invalid hostname document: {e}"))?;
    Ok(c.hostname)
}

impl Applier for HostnameDomain {
    fn key(&self) -> &str {
        HOSTNAME_KEY
    }

    fn templates(&self) -> Vec<(String, String)> {
        vec![
            ("system.hostname/hostname".into(), "{{ hostname }}\n".into()),
            (
                "system.hostname/hosts".into(),
                "127.0.1.1\t{{ hostname }}\n".into(),
            ),
        ]
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let h = hostname_of(&value)?.trim().to_ascii_lowercase();
        if !valid_hostname(&h) {
            return Err(Invalid::at(
                "hostname",
                format!(
                    "invalid hostname {h:?}: use 1-63 letters, digits and '-', not at the start or end"
                ),
            ));
        }
        serde_json::to_value(HostnameConfig { hostname: h }).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let hostname = ManagedFile::whole(
            "/etc/hostname",
            ctx.render("system.hostname/hostname", value)?,
            // hostnamectl rewrites this file: no header, the hash alone detects drift
            false,
        );
        let mut hosts = ManagedFile::block(
            "/etc/hosts",
            "hostname",
            ctx.render("system.hostname/hosts", value)?,
        );
        hosts.kind = FileKind::Block {
            id: "hostname".into(),
            drop_outside: vec!["127.0.1.1".into()],
        };
        Ok(vec![hostname, hosts])
    }

    fn reload(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        *self.renamed.lock().unwrap() = None;
        let Some(v) = value else { return Ok(()) };
        let want = hostname_of(v)?;
        let before = ctx.system.hostname().unwrap_or_default();
        ctx.system.set_hostname(&want)?;
        if before == want {
            return Ok(());
        }
        if !ctx.rolling_back() {
            *self.renamed.lock().unwrap() = Some(want.clone());
        }
        (ctx.log)(&format!("host name changed from {before:?} to {want:?}"));
        // best effort: a NAS without Avahi has no `.local` name to move
        if ctx.system.unit_active(AVAHI_UNIT).unwrap_or(false) {
            (ctx.log)("restarting Avahi for the new .local name");
            if let Err(e) = ctx.restart_unit(AVAHI_UNIT) {
                (ctx.log)(&format!("Avahi: {e}"));
            }
        }
        Ok(())
    }

    fn committed(&self, _old: Option<&Value>, _new: Option<&Value>) {
        let renamed = self.renamed.lock().unwrap().take();
        if let (Some(h), Some(f)) = (renamed, &self.on_change) {
            f(&h);
        }
    }

    fn health(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        let Some(v) = value else { return Ok(()) };
        let want = hostname_of(v)?;
        let now = ctx.system.hostname()?;
        if now == want {
            Ok(())
        } else {
            Err(format!(
                "the system reports hostname {now:?}, expected {want:?}"
            ))
        }
    }

    fn import(&self, ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        let from_file = ctx.read(Path::new("/etc/hostname")).and_then(|t| {
            t.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
        });
        let h = match from_file {
            Some(h) => h,
            None => ctx.system.hostname()?,
        };
        let h = h.split('.').next().unwrap_or("").to_ascii_lowercase();
        Ok(valid_hostname(&h).then(|| serde_json::json!({ "hostname": h })))
    }
}

/// A generic domain rendering one template to one file, with optional unit reload and
/// validator. The value must be a JSON object; it is the template context.
pub struct FileDomain {
    key: String,
    path: PathBuf,
    template: String,
    header: bool,
    unit: Option<String>,
    /// Validator argv; `{}` is replaced by the candidate file.
    validator: Option<Vec<String>>,
}

impl FileDomain {
    pub fn new(key: &str, path: impl Into<PathBuf>, template: &str) -> Self {
        Self {
            key: key.into(),
            path: path.into(),
            template: template.into(),
            header: true,
            unit: None,
            validator: None,
        }
    }

    pub fn unit(mut self, unit: &str) -> Self {
        self.unit = Some(unit.into());
        self
    }

    pub fn validator(mut self, argv: &[&str]) -> Self {
        self.validator = Some(argv.iter().map(|s| s.to_string()).collect());
        self
    }

    fn template_name(&self) -> String {
        format!("{}/file", self.key)
    }
}

impl Applier for FileDomain {
    fn key(&self) -> &str {
        &self.key
    }

    fn templates(&self) -> Vec<(String, String)> {
        vec![(self.template_name(), self.template.clone())]
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        if value.is_object() {
            Ok(value)
        } else {
            Err("the value must be a JSON object".into())
        }
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let content = ctx.render(&self.template_name(), value)?;
        Ok(vec![ManagedFile::whole(&self.path, content, self.header)])
    }

    fn check_files(&self, files: &[Candidate], ctx: &ApplyCtx) -> Result<(), String> {
        let Some(argv) = &self.validator else {
            return Ok(());
        };
        for c in files {
            let staged = c.staged.to_string_lossy();
            let args: Vec<String> = argv.iter().map(|a| a.replace("{}", &staged)).collect();
            ctx.system.run(&Cmd::new(&args[0]).args(&args[1..]))?;
        }
        Ok(())
    }

    fn reload(&self, _value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        match &self.unit {
            Some(u) => ctx.system.reload_or_restart_unit(u),
            None => Ok(()),
        }
    }

    fn health(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        match (&self.unit, value) {
            (Some(u), Some(_)) if !ctx.system.unit_active(u)? => {
                Err(format!("{u} is not active after the reload"))
            }
            _ => Ok(()),
        }
    }
}
