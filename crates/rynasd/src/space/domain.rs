//! The configuration domain of Space (key [`SPACE_KEY`], [`SpaceSettings`]).
//!
//! Its only file is a journald drop-in (`/etc/systemd/journald.conf.d/50-rynas-space.conf`)
//! with `SystemMaxUse=`: journald's own default is 10 % of the filesystem capped at 4 GiB,
//! which on a small system disk is a lot for logs; rynas caps it at min(2 GiB, 5 %) unless
//! the administrator sets another value (0: no drop-in, journald's default). The rest of the
//! settings (notifications, thinning, budgets, recycle, thumbnails) is read by rynasd's loop
//! from the applied value.

use std::path::{Path, PathBuf};

use rynas_api_types::space::{SPACE_KEY, SpaceSettings};
use rynas_config::{ApplyCtx, Applier, Invalid, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

/// The drop-in rynas writes.
pub const JOURNALD_DROPIN: &str = "/etc/systemd/journald.conf.d/50-rynas-space.conf";
/// The default cap: at most 2 GiB …
pub const JOURNAL_MAX_BYTES: u64 = 2 << 30;
/// … and at most this percent of the journal's filesystem.
pub const JOURNAL_MAX_PCT: u64 = 5;
/// Never below 64 MiB (journald needs room for a few files).
pub const JOURNAL_MIN_BYTES: u64 = 64 << 20;

/// The cap in bytes for the settings and the size of the journal's filesystem; `None`: no
/// cap from rynas.
pub fn journal_cap(s: &SpaceSettings, fs_bytes: u64) -> Option<u64> {
    match s.journal_cap_mib {
        Some(0) => None,
        Some(m) => Some((u64::from(m) << 20).max(JOURNAL_MIN_BYTES)),
        None => {
            let pct = if fs_bytes == 0 { JOURNAL_MAX_BYTES } else { fs_bytes / 100 * JOURNAL_MAX_PCT };
            Some(pct.clamp(JOURNAL_MIN_BYTES, JOURNAL_MAX_BYTES))
        }
    }
}

/// Size of the filesystem holding the journal below `root`.
pub fn journal_fs_bytes(root: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    for d in ["var/log/journal", "var/log", ""] {
        let p: PathBuf = root.join(d);
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { continue };
        // SAFETY: statvfs(3) fills the struct; a valid C string
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0 {
            return s.f_blocks as u64 * s.f_frsize as u64;
        }
    }
    0
}

/// The drop-in's text.
pub fn dropin(cap: u64) -> String {
    format!(
        "# Written by rynas (Storage > Space): the journal's largest size on disk.\n[Journal]\nSystemMaxUse={}M\n",
        cap >> 20
    )
}

fn settings(v: &Value) -> SpaceSettings {
    serde_json::from_value(v.clone()).unwrap_or_default()
}

pub struct SpaceDomain;

impl Applier for SpaceDomain {
    fn key(&self) -> &str {
        SPACE_KEY
    }

    fn order(&self) -> i32 {
        90
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let mut s: SpaceSettings = serde_json::from_value(value).map_err(|e| format!("invalid space settings: {e}"))?;
        if let Some((field, msg)) = s.problem() {
            return Err(Invalid::at(field, msg));
        }
        s.auto_free.sort();
        s.auto_free.dedup();
        serde_json::to_value(s).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(vec![match journal_cap(&settings(value), journal_fs_bytes(ctx.root)) {
            Some(cap) => ManagedFile::whole(JOURNALD_DROPIN, dropin(cap), false),
            None => ManagedFile::absent(JOURNALD_DROPIN),
        }])
    }

    fn render_removed(&self, _old: Option<&Value>, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(vec![ManagedFile::absent(JOURNALD_DROPIN)])
    }

    /// A changed cap: journald reads its drop-ins when it starts (its open log streams are
    /// kept in systemd's file descriptor store), then the journal is cut to the cap at once.
    fn reconcile(&self, from: Option<&Value>, to: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        let fs = journal_fs_bytes(ctx.root);
        let cap = |v: Option<&Value>| v.map(settings).and_then(|s| journal_cap(&s, fs));
        let (old, new) = (cap(from), cap(to));
        if old == new && from.is_some() {
            return Ok(());
        }
        (ctx.log)("journald: reading the new size limit");
        ctx.system.run(&Cmd::new("systemctl").args(["try-restart", "systemd-journald.service"]))?;
        if let Some(c) = new {
            let out = ctx.system.run(&Cmd::new("journalctl").arg(format!("--vacuum-size={}M", c >> 20)))?;
            if !out.success() {
                (ctx.log)(&format!("journalctl --vacuum-size: {}", out.stderr.trim()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_is_two_gib_or_five_percent() {
        let s = SpaceSettings::default();
        assert_eq!(journal_cap(&s, 10 << 30), Some((10u64 << 30) / 100 * 5));
        assert_eq!(journal_cap(&s, 4 << 40), Some(JOURNAL_MAX_BYTES));
        assert_eq!(journal_cap(&s, 100 << 20), Some(JOURNAL_MIN_BYTES));
        let mut s2 = s.clone();
        s2.journal_cap_mib = Some(0);
        assert_eq!(journal_cap(&s2, 1 << 40), None);
        s2.journal_cap_mib = Some(500);
        assert_eq!(journal_cap(&s2, 1 << 40), Some(500 << 20));
        assert!(dropin(2 << 30).contains("SystemMaxUse=2048M"));
    }

    #[test]
    fn validation_names_the_field_and_normalizes() {
        let d = SpaceDomain;
        let v = d.validate(serde_json::json!({"auto_free": ["vol2", "vol1", "vol1"]})).unwrap();
        assert_eq!(v["auto_free"], serde_json::json!(["vol1", "vol2"]));
        assert_eq!(v["low_free_pct"], 10);
        let e = d.validate(serde_json::json!({"low_free_pct": 20, "recover_pct": 15})).unwrap_err();
        assert_eq!(e.field.as_deref(), Some("recover_pct"));
    }
}
