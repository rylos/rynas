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

/// A `SystemMaxUse=` value in bytes (`2G`, `500M`, `1048576`; 1024-based suffixes).
pub fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim();
    let (num, mul) = match v.char_indices().last()? {
        (i, c) if c.is_ascii_alphabetic() => (
            &v[..i],
            match c.to_ascii_uppercase() {
                'K' => 1u64 << 10,
                'M' => 1 << 20,
                'G' => 1 << 30,
                'T' => 1 << 40,
                'P' => 1 << 50,
                _ => return None,
            },
        ),
        _ => (v, 1),
    };
    num.trim().parse::<u64>().ok().map(|n| n.saturating_mul(mul))
}

/// Where journald reads drop-ins, the first hiding a file of the same name in the later ones.
const DROPIN_DIRS: &[&str] = &[
    "etc/systemd/journald.conf.d",
    "run/systemd/journald.conf.d",
    "usr/local/lib/systemd/journald.conf.d",
    "usr/lib/systemd/journald.conf.d",
];

/// The files journald reads below `root`, in its order (the last value read wins): the main
/// file (`/etc/systemd/journald.conf`, else the vendor one in `/usr/lib/systemd/`), then every
/// `*.conf` drop-in of [`DROPIN_DIRS`] sorted by file name, whatever its directory (a name in
/// an earlier directory hides the same name in a later one). `skip_ours`: leave rynas's own
/// drop-in out, and every file of its name (which it hides when present).
pub fn journald_files(root: &Path, skip_ours: bool) -> Vec<PathBuf> {
    let ours = Path::new(JOURNALD_DROPIN).file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    let main = root.join("etc/systemd/journald.conf");
    let vendor = root.join("usr/lib/systemd/journald.conf");
    let mut files: Vec<PathBuf> = vec![if main.exists() || !vendor.exists() { main } else { vendor }];
    let mut dropins: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for d in DROPIN_DIRS {
        for e in std::fs::read_dir(root.join(d)).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !n.ends_with(".conf") || (skip_ours && n == ours) {
                continue;
            }
            dropins.entry(n).or_insert_with(|| e.path());
        }
    }
    files.extend(dropins.into_values());
    files
}

/// The last `SystemMaxUse=` of the `[Journal]` section in `files` (journald's reading: an
/// empty value resets it to the default). `None`: journald's default.
pub fn system_max_use(files: &[PathBuf]) -> Option<u64> {
    let mut cap = None;
    for f in files {
        let Ok(t) = std::fs::read_to_string(f) else { continue };
        let mut in_journal = false;
        for l in t.lines().map(str::trim) {
            if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
                continue;
            }
            if l.starts_with('[') {
                in_journal = l == "[Journal]";
            } else if in_journal
                && let Some((k, v)) = l.split_once('=')
                && k.trim() == "SystemMaxUse"
            {
                cap = if v.trim().is_empty() { None } else { parse_size(v) };
            }
        }
    }
    cap
}

/// The administrator's own `SystemMaxUse=` below `root`: every journald file except rynas's
/// drop-in, wherever it sorts (before or after rynas's, in `/etc`, `/run` or `/usr/lib`).
/// When set, rynas writes no drop-in and never vacuums below it. `None`: not set.
pub fn admin_journal_cap(root: &Path) -> Option<u64> {
    system_max_use(&journald_files(root, true))
}

/// The `SystemMaxUse=` journald uses now below `root` (all its files, rynas's drop-in included).
pub fn journald_cap_now(root: &Path) -> Option<u64> {
    system_max_use(&journald_files(root, false))
}

/// The cap a vacuum may cut the journal to: what journald reads now (`in_force`, all its
/// files with rynas's drop-in), never below the administrator's own value (`admin`); `None`:
/// no cap in force (journald's default, or rynas's never applied).
pub fn effective_journal_cap(admin: Option<u64>, in_force: Option<u64>) -> Option<u64> {
    match (admin, in_force) {
        (Some(a), Some(c)) => Some(a.max(c)),
        (a, c) => a.or(c),
    }
}

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

    /// No drop-in at all when the administrator set `SystemMaxUse=` in an earlier file: theirs
    /// stays in force (the Space tab shows it).
    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let cap = if admin_journal_cap(ctx.root).is_some() { None } else { journal_cap(&settings(value), journal_fs_bytes(ctx.root)) };
        Ok(vec![match cap {
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
        if admin_journal_cap(ctx.root).is_some() {
            return Ok(());
        }
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
    fn the_administrators_cap_wins() {
        let d = tempfile::tempdir().unwrap();
        let etc = d.path().join("etc/systemd");
        std::fs::create_dir_all(etc.join("journald.conf.d")).unwrap();
        assert_eq!(admin_journal_cap(d.path()), None);
        std::fs::write(etc.join("journald.conf"), "[Journal]\n#SystemMaxUse=\nStorage=auto\n").unwrap();
        assert_eq!(admin_journal_cap(d.path()), None);
        std::fs::write(etc.join("journald.conf"), "[Journal]\nSystemMaxUse=500M\n").unwrap();
        assert_eq!(admin_journal_cap(d.path()), Some(500 << 20));
        // a drop-in numbered before rynas's overrides the main file; ours is never the admin's
        std::fs::write(etc.join("journald.conf.d/10-site.conf"), "[Journal]\nSystemMaxUse=1G\n").unwrap();
        std::fs::write(etc.join("journald.conf.d/50-rynas-space.conf"), "[Journal]\nSystemMaxUse=2048M\n").unwrap();
        assert_eq!(admin_journal_cap(d.path()), Some(1 << 30));
        assert_eq!(journald_cap_now(d.path()), Some(2048 << 20));
        // one sorting after ours wins in journald too, wherever its folder is (`/run`, `/usr/lib`)
        let run = d.path().join("run/systemd/journald.conf.d");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("90-late.conf"), "[Journal]\nSystemMaxUse = 3G\n").unwrap();
        assert_eq!(admin_journal_cap(d.path()), Some(3 << 30));
        assert_eq!(journald_cap_now(d.path()), Some(3 << 30));
        // a file in /etc hides the same name in /usr/lib; an empty value resets to the default
        let lib = d.path().join("usr/lib/systemd/journald.conf.d");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("95-vendor.conf"), "[Journal]\nSystemMaxUse=8G\n").unwrap();
        assert_eq!(admin_journal_cap(d.path()), Some(8 << 30));
        std::fs::write(etc.join("journald.conf.d/95-vendor.conf"), "[Journal]\nSystemMaxUse=\n").unwrap();
        assert_eq!(admin_journal_cap(d.path()), None);
        // comments and other sections do not count
        std::fs::write(etc.join("journald.conf.d/96-other.conf"), "# SystemMaxUse=1M\n[Other]\nSystemMaxUse=1M\n").unwrap();
        assert_eq!(admin_journal_cap(d.path()), None);
        // the vacuum never goes below the administrator's value
        assert_eq!(effective_journal_cap(Some(1 << 30), Some(512 << 20)), Some(1 << 30));
        assert_eq!(effective_journal_cap(Some(1 << 30), None), Some(1 << 30));
        assert_eq!(effective_journal_cap(None, Some(JOURNAL_MAX_BYTES)), Some(JOURNAL_MAX_BYTES));
        // never applied, no admin value: journald's own default, no vacuum
        assert_eq!(effective_journal_cap(None, None), None);
        assert_eq!(parse_size("2G"), Some(2 << 30));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("5%"), None);
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
