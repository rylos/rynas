//! Small validators shared by the domains. Everything that ends up in a generated file or
//! on a command line goes through one of these first.

pub use rynas_api_types::validate::{account_name, share_name};

/// Names that rynas never manages as local users or groups.
pub use rynas_api_types::validate::RESERVED_ACCOUNT_NAMES as RESERVED_NAMES;

/// Free text on one line (descriptions, full names): no control characters.
pub fn one_line(s: &str, max: usize) -> Result<(), String> {
    if s.chars().count() > max {
        return Err(format!("too long (max {max} characters)"));
    }
    if s.chars().any(char::is_control) {
        return Err("control characters are not allowed".into());
    }
    Ok(())
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `YYYY-MM-DD` → days since the epoch; `None` if it is not a real date.
pub fn date_to_days(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = &s[r];
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if d > mdays[(m - 1) as usize] {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Days since the epoch → `YYYY-MM-DD`.
pub fn days_to_date(z: i64) -> String {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today, in days since the epoch (UTC).
pub fn today_days() -> i64 {
    (rynas_tasks::now_ms() / 86_400_000) as i64
}

/// Directories that can never be shared (or have ACLs rewritten by rynas).
const FORBIDDEN_PREFIXES: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    "/var/lib/rynas",
    "/tmp",
    "/var/tmp",
];
/// Mount parents: their children can be shared, not they themselves.
const FORBIDDEN_EXACT: &[&str] = &["/", "/home", "/srv", "/mnt", "/media", "/var", "/opt"];

/// Normalized absolute directory for a share: no `..`, `.`, `//`, trailing `/`, quotes,
/// backslashes or control characters, and not a system directory.
pub fn share_path(p: &str) -> Result<(), String> {
    if !p.starts_with('/') {
        return Err(format!("{p:?} is not an absolute path"));
    }
    if p.len() > 1024 {
        return Err("path too long".into());
    }
    if p.chars().any(|c| c.is_control() || c == '"' || c == '\\') {
        return Err(format!("{p:?} has characters that are not allowed"));
    }
    if p.len() > 1 && p.ends_with('/') {
        return Err(format!("{p:?}: remove the trailing '/'"));
    }
    if p[1..]
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(format!("{p:?} is not a normalized path"));
    }
    if FORBIDDEN_EXACT.contains(&p) {
        return Err(format!("{p} cannot be shared: share a folder inside it"));
    }
    if let Some(f) = FORBIDDEN_PREFIXES
        .iter()
        .find(|f| p == **f || p.starts_with(&format!("{f}/")))
    {
        return Err(format!("{p} is inside {f}, which cannot be shared"));
    }
    Ok(())
}

fn ipv4(s: &str) -> bool {
    s.parse::<std::net::Ipv4Addr>().is_ok()
}

fn ipv6(s: &str) -> bool {
    s.parse::<std::net::Ipv6Addr>().is_ok()
}

/// NFS client specification: `*`, IP, IP/prefix, host name, wildcard host, `@netgroup`.
pub fn nfs_client(s: &str) -> bool {
    if s == "*" {
        return true;
    }
    if let Some((ip, len)) = s.split_once('/') {
        return match len.parse::<u8>() {
            Ok(n) if ipv4(ip) => n <= 32,
            Ok(n) if ipv6(ip) => n <= 128,
            _ => false,
        };
    }
    if ipv4(s) || ipv6(s) {
        return true;
    }
    let host = s.strip_prefix('@').unwrap_or(s);
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'*' | b'?' | b'_'))
        && !host.starts_with('-')
}

/// Network interface name or IP address (smb.conf `interfaces`).
pub fn interface(s: &str) -> bool {
    if ipv4(s) || ipv6(s) {
        return true;
    }
    if let Some((ip, len)) = s.split_once('/') {
        return (ipv4(ip) || ipv6(ip)) && len.parse::<u8>().is_ok();
    }
    !s.is_empty()
        && s.len() <= 15
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// One `key = value` line of the extra options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraOption {
    pub key: String,
    pub value: String,
}

/// Parses the free-form extra options of smb.conf: blank lines and `#`/`;` comments are
/// dropped, every other line must be `key = value` (no sections).
pub fn smb_extra(text: &str) -> Result<Vec<ExtraOption>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if l.chars().any(char::is_control) {
            return Err(format!("line {}: control characters", n + 1));
        }
        let Some((k, v)) = l.split_once('=') else {
            return Err(format!("line {}: expected 'key = value'", n + 1));
        };
        let k = k.trim().to_ascii_lowercase();
        let k = k.split_whitespace().collect::<Vec<_>>().join(" ");
        if k.is_empty()
            || !k
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b' ' | b':' | b'_' | b'-' | b'.'))
        {
            return Err(format!("line {}: invalid parameter name {k:?}", n + 1));
        }
        out.push(ExtraOption {
            key: k,
            value: v.trim().to_string(),
        });
    }
    Ok(out)
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, lowercase hex.
pub fn uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

/// A random (version 4) UUID.
pub fn new_uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the operating system random source failed");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(account_name("alice") && account_name("_svc") && account_name("a-b_1"));
        assert!(!account_name("Alice") && !account_name("1a") && !account_name(""));
        assert!(!account_name("a:b") && !account_name(&"a".repeat(33)));
        assert!(share_name("Photos") && share_name("time.machine") && share_name("a_b-c"));
        assert!(!share_name("homes") && !share_name("IPC$") && !share_name("a b"));
        assert!(!share_name("-x") && !share_name("a]"));
    }

    #[test]
    fn dates() {
        assert_eq!(date_to_days("1970-01-01"), Some(0));
        assert_eq!(date_to_days("1970-01-02"), Some(1));
        assert_eq!(date_to_days("2026-11-05"), Some(20762));
        assert_eq!(days_to_date(20762), "2026-11-05");
        assert_eq!(date_to_days("2024-02-29"), Some(19782));
        assert_eq!(date_to_days("2025-02-29"), None);
        assert_eq!(date_to_days("2025-13-01"), None);
        assert_eq!(date_to_days("2025-1-01"), None);
        for d in [0, 1, 19782, 20762, 40000] {
            assert_eq!(date_to_days(&days_to_date(d)), Some(d));
        }
    }

    #[test]
    fn paths() {
        assert!(share_path("/srv/vol1/photos").is_ok());
        assert!(share_path("/home/alice/shared").is_ok());
        for bad in [
            "srv/x",
            "/",
            "/srv",
            "/etc/samba",
            "/proc",
            "/srv/../etc",
            "/srv//x",
            "/srv/x/",
            "/srv/a\"b",
            "/srv/a\nb",
            "/usr/share",
            "/var/lib/rynas/x",
        ] {
            assert!(share_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn clients_and_interfaces() {
        for ok in [
            "*",
            "192.168.1.0/24",
            "10.0.0.5",
            "fd00::/64",
            "nas.lan",
            "*.lan",
            "@trusted",
        ] {
            assert!(nfs_client(ok), "{ok}");
        }
        for bad in ["192.168.1.0/33", "a b", "x(rw)", "-o", "a,b", ""] {
            assert!(!nfs_client(bad), "{bad}");
        }
        assert!(interface("eth0") && interface("192.168.1.10/24") && interface("bond0.10"));
        assert!(!interface("eth 0") && !interface("eth0;rm"));
    }

    #[test]
    fn extra_options() {
        let o = smb_extra("# comment\n  Strict Sync = yes\n\nfruit:aapl=no\n").unwrap();
        assert_eq!(o[0].key, "strict sync");
        assert_eq!(
            o[1],
            ExtraOption {
                key: "fruit:aapl".into(),
                value: "no".into()
            }
        );
        assert!(smb_extra("[evil]\npath = /").is_err());
        assert!(smb_extra("no equals sign").is_err());
    }

    #[test]
    fn uuids() {
        let u = new_uuid();
        assert!(uuid(&u), "{u}");
        assert_ne!(u, new_uuid());
        assert!(!uuid("not-a-uuid"));
    }
}
