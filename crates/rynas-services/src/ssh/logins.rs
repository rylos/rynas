//! Logins and failed attempts of the last day, read from the journal on request
//! (`journalctl -o short-unix` of sshd's identifiers: `sshd`, and `sshd-session`/`sshd-auth`
//! since OpenSSH 9.8/10).

use std::collections::BTreeMap;

use rynas_api_types::ssh::{SshLoginEvent, SshLoginKind, SshLogins};

/// Events kept for the page (the counts cover the whole day).
pub const MAX_EVENTS: usize = 100;

/// The command (argv).
pub fn argv() -> Vec<&'static str> {
    vec![
        "journalctl", "--since=-24h", "--lines=20000", "--output=short-unix", "--no-pager",
        "--quiet", "--identifier=sshd", "--identifier=sshd-session", "--identifier=sshd-auth",
    ]
}

/// `user` and `address` of "... for USER from ADDR port N ...".
fn who(rest: &str) -> Option<(String, String)> {
    let (user, after) = rest.split_once(" from ")?;
    let addr = after.split_whitespace().next()?;
    Some((user.trim().to_string(), addr.to_string()))
}

/// One journal line.
pub fn parse_line(line: &str) -> Option<SshLoginEvent> {
    let (ts, rest) = line.split_once(' ')?;
    let secs: f64 = ts.parse().ok()?;
    let msg = rest.split_once("]: ").map(|(_, m)| m)?;
    let at_ms = (secs * 1000.0) as u64;
    let ev = |kind, method: &str, user: String, address: String| SshLoginEvent {
        at_ms,
        kind,
        user,
        address,
        method: method.to_string(),
    };
    if let Some(r) = msg.strip_prefix("Accepted ") {
        let (method, r) = r.split_once(" for ")?;
        let (u, a) = who(r)?;
        return Some(ev(SshLoginKind::Accepted, method, u, a));
    }
    if let Some(r) = msg.strip_prefix("Failed ") {
        let (method, r) = r.split_once(" for ")?;
        // counted once by its "Invalid user" line
        if r.starts_with("invalid user ") {
            return None;
        }
        let (u, a) = who(r)?;
        return Some(ev(SshLoginKind::Failed, method, u, a));
    }
    if let Some(r) = msg.strip_prefix("Invalid user ") {
        let (u, a) = who(r)?;
        return Some(ev(SshLoginKind::InvalidUser, "", u, a));
    }
    None
}

pub fn summarize(out: &str) -> SshLogins {
    let mut events: Vec<SshLoginEvent> = out.lines().filter_map(parse_line).collect();
    let mut s = SshLogins::default();
    let mut by_addr: BTreeMap<String, u32> = BTreeMap::new();
    for e in &events {
        match e.kind {
            SshLoginKind::Accepted => s.accepted += 1,
            SshLoginKind::Failed | SshLoginKind::InvalidUser => {
                s.failed += 1;
                *by_addr.entry(e.address.clone()).or_default() += 1;
            }
        }
    }
    let mut top: Vec<(String, u32)> = by_addr.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    top.truncate(5);
    s.top_failed = top;
    events.sort_by_key(|e| std::cmp::Reverse(e.at_ms));
    events.truncate(MAX_EVENTS);
    s.events = events;
    s
}
