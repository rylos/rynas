//! The system time zone for rynas's long-running processes (D-TIME, `docs/phase2/date-time.md`).
//!
//! glibc's `localtime_r` reads `/etc/localtime` once per process and jiff keeps the system
//! zone for five minutes, so a zone changed in System > Date and time, with `timedatectl` or by
//! hand would reach snapshot names, quiet hours and the power schedule only later. Here the
//! zone is read again as soon as the file changes: [`system_tz`] keeps the parsed zone with the
//! file's identity (device, inode, size, modification time, and the target of the link) and
//! looks at it again at most once a second.
//!
//! With `TZ` set the process follows it (jiff's own reading), as glibc would.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use jiff::tz::TimeZone;

/// The file systemd and glibc read the zone from.
pub const LOCALTIME: &str = "/etc/localtime";
/// Debian's name of the zone, for an `/etc/localtime` that is a copy instead of a link.
pub const TIMEZONE: &str = "/etc/timezone";

/// What identifies one version of the zone file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    target: Option<PathBuf>,
}

impl Key {
    fn of(path: &Path) -> Option<Key> {
        let m = std::fs::metadata(path).ok()?;
        Some(Key {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            target: std::fs::read_link(path).ok(),
        })
    }
}

struct Cached {
    key: Option<Key>,
    tz: TimeZone,
    checked: Instant,
}

/// A zone file kept parsed until it changes.
pub struct ZoneCache {
    path: PathBuf,
    names: PathBuf,
    recheck: Duration,
    state: Mutex<Option<Cached>>,
}

impl ZoneCache {
    /// `path` is the zone file (`/etc/localtime`), `names` the file with its name when it is
    /// not a link (`/etc/timezone`); the file is looked at again after `recheck`.
    pub fn new(path: impl Into<PathBuf>, names: impl Into<PathBuf>, recheck: Duration) -> Self {
        ZoneCache { path: path.into(), names: names.into(), recheck, state: Mutex::new(None) }
    }

    /// The zone now: the cached one while the file is the same.
    pub fn get(&self) -> TimeZone {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = st.as_mut() {
            if c.checked.elapsed() < self.recheck {
                return c.tz.clone();
            }
            let key = Key::of(&self.path);
            c.checked = Instant::now();
            if key == c.key {
                return c.tz.clone();
            }
        }
        let key = Key::of(&self.path);
        let tz = self.load(key.as_ref());
        *st = Some(Cached { key, tz: tz.clone(), checked: Instant::now() });
        tz
    }

    fn load(&self, key: Option<&Key>) -> TimeZone {
        // no zone file: UTC, like glibc
        let Some(key) = key else { return TimeZone::UTC };
        let name = key.target.as_deref().and_then(zone_name).or_else(|| {
            let n = std::fs::read_to_string(&self.names).ok()?;
            let n = n.trim();
            (!n.is_empty() && !n.contains("..")).then(|| n.to_string())
        });
        match (name, std::fs::read(&self.path)) {
            (Some(name), Ok(data)) => TimeZone::tzif(&name, &data).unwrap_or(TimeZone::UTC),
            // a copy without a name: jiff reads it on its own
            (None, Ok(_)) => TimeZone::system(),
            (_, Err(_)) => TimeZone::UTC,
        }
    }
}

static SYSTEM: LazyLock<ZoneCache> = LazyLock::new(|| ZoneCache::new(LOCALTIME, TIMEZONE, Duration::from_secs(1)));

/// The system's time zone (`TZ`, else `/etc/localtime`), read again when the file changes.
pub fn system_tz() -> TimeZone {
    if std::env::var_os("TZ").is_some_and(|v| !v.is_empty()) {
        return TimeZone::system();
    }
    SYSTEM.get()
}

/// The IANA name in the target of `/etc/localtime` (`../usr/share/zoneinfo/Europe/Rome`).
pub fn zone_name(target: &Path) -> Option<String> {
    let s = target.to_str()?;
    let (_, name) = s.rsplit_once("zoneinfo/")?;
    let name = name.strip_prefix("posix/").unwrap_or(name);
    (!name.is_empty() && !name.contains("..")).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TZif v2 file of a fixed offset (hours east of UTC), no transitions.
    fn tzif(hours: i32) -> Vec<u8> {
        let header = |out: &mut Vec<u8>| {
            out.extend(b"TZif2");
            out.extend([0u8; 15]);
            // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
            for n in [0u32, 0, 0, 0, 1, 4] {
                out.extend(n.to_be_bytes());
            }
            out.extend((hours * 3600).to_be_bytes());
            out.extend([0u8, 0]);
            out.extend(b"XYZ\0");
        };
        let mut out = Vec::new();
        header(&mut out);
        header(&mut out);
        // POSIX TZ footer: the sign is west of UTC
        out.extend(format!("\nXYZ{}\n", -hours).as_bytes());
        out
    }

    fn offset(tz: &TimeZone) -> i32 {
        tz.to_offset(jiff::Timestamp::UNIX_EPOCH).seconds()
    }

    #[test]
    fn follows_the_link_and_the_file() {
        let d = tempfile::tempdir().unwrap();
        let zi = d.path().join("zoneinfo/Test");
        std::fs::create_dir_all(&zi).unwrap();
        std::fs::write(zi.join("One"), tzif(1)).unwrap();
        std::fs::write(zi.join("Two"), tzif(2)).unwrap();
        let lt = d.path().join("localtime");
        std::os::unix::fs::symlink(zi.join("One"), &lt).unwrap();
        let c = ZoneCache::new(&lt, d.path().join("timezone"), Duration::ZERO);
        let tz = c.get();
        assert_eq!((tz.iana_name(), offset(&tz)), (Some("Test/One"), 3600));
        // timedated replaces the link
        std::fs::remove_file(&lt).unwrap();
        std::os::unix::fs::symlink(zi.join("Two"), &lt).unwrap();
        let tz = c.get();
        assert_eq!((tz.iana_name(), offset(&tz)), (Some("Test/Two"), 7200));
        // a copy with its name in the other file, rewritten in place
        std::fs::remove_file(&lt).unwrap();
        std::fs::write(&lt, tzif(3)).unwrap();
        std::fs::write(d.path().join("timezone"), "Test/Three\n").unwrap();
        let tz = c.get();
        assert_eq!((tz.iana_name(), offset(&tz)), (Some("Test/Three"), 10800));
        std::fs::write(&lt, tzif(-5)).unwrap();
        assert_eq!(offset(&c.get()), -18000, "the size or the time of the file changed");
        std::fs::remove_file(&lt).unwrap();
        assert_eq!(offset(&c.get()), 0, "no file: UTC");
    }

    #[test]
    fn looks_again_only_after_the_interval() {
        let d = tempfile::tempdir().unwrap();
        let lt = d.path().join("localtime");
        std::fs::write(&lt, tzif(1)).unwrap();
        std::fs::write(d.path().join("timezone"), "Test/One\n").unwrap();
        let c = ZoneCache::new(&lt, d.path().join("timezone"), Duration::from_secs(3600));
        assert_eq!(offset(&c.get()), 3600);
        std::fs::write(&lt, tzif(2)).unwrap();
        assert_eq!(offset(&c.get()), 3600, "cached within the interval");
    }

    #[test]
    fn zone_names_from_the_link() {
        let n = |p: &str| zone_name(Path::new(p));
        assert_eq!(n("../usr/share/zoneinfo/Europe/Rome").as_deref(), Some("Europe/Rome"));
        assert_eq!(n("/usr/share/zoneinfo/UTC").as_deref(), Some("UTC"));
        assert_eq!(n("/usr/share/zoneinfo/posix/Asia/Tokyo").as_deref(), Some("Asia/Tokyo"));
        assert_eq!(n("/etc/zone"), None);
        assert_eq!(n("/usr/share/zoneinfo/"), None);
    }
}
