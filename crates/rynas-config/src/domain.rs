//! Configuration domains: the [`Applier`] trait and the built-in domains.
//!
//! A domain owns one configuration key (a JSON document such as `system.hostname`) and
//! knows how to validate it, render it to files (minijinja templates), check the rendered
//! files, reload the services that read them, verify the result and read the current state
//! of a system it never managed (import).

use std::path::{Path, PathBuf};

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
}

impl ApplyCtx<'_> {
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
/// convention), and the running hostname through [`System::set_hostname`].
pub struct HostnameDomain;

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
        match value {
            Some(v) => ctx.system.set_hostname(&hostname_of(v)?),
            None => Ok(()),
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
