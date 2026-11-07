//! The network UPS server's pure part (`ups.netserver`): checks and the text rynas writes.
//!
//! - **`upsd.conf`** ([`render_upsd_conf`]): `LISTEN` lines. All addresses is
//!   `LISTEN 0.0.0.0` + `LISTEN ::` (upsd sets `IPV6_V6ONLY`, so both bind; they cover
//!   localhost too); chosen addresses come after `127.0.0.1` and `::1`, so the first `LISTEN`
//!   (the only one whose failure stops upsd) always binds and an address that went away
//!   only logs. The admin's own `LISTEN` lines are dropped while the server is on (they would
//!   clash), kept when it is off.
//! - **`upsd.users`** ([`render_users`]): one section per client, `upsmon secondary` or no
//!   rights at all (a monitor account).
//! - **Allowed hosts** ([`render_dropin`]): NUT dropped its own ACLs in 2.4; rynas uses
//!   systemd's per-service IP filter (`IPAddressDeny=any` + `IPAddressAllow=` on
//!   `nut-server.service`, cgroup eBPF), which drops other sources' packets before upsd sees
//!   them. Without eBPF cgroup support systemd ignores it: a firewall is then the way.
//! - **Client lines** ([`monitor_line`]): what another Linux `upsmon.conf` needs.

use std::net::IpAddr;

use rynas_api_types::ups::{
    MAX_NET_ADDRS, MAX_NET_CLIENTS, NET_PASSWORD_LEN, NUT_PORT, UpsClientRole, UpsNetServerConfig,
};

/// User names rynas itself uses in `upsd.users` (battery tests, its own upsmon).
pub const RESERVED_USERS: &[&str] = &["rynas", "rynas-upsmon"];

/// A client account name: `[a-z0-9_-]`, 1–32 characters, not one of rynas's own.
pub fn valid_client_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && !RESERVED_USERS.contains(&s)
}

/// A password the admin chooses: 12–64 of `[A-Za-z0-9._~@%+=:,-]` (one word everywhere: no
/// spaces, quotes or `#`, so it fits `upsd.users`, a `MONITOR` line and a form field).
pub fn valid_password(s: &str) -> bool {
    let (lo, hi) = NET_PASSWORD_LEN;
    (lo..=hi).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~@%+=:,-".contains(&b))
}

/// A listen address: an IP of this machine's LAN, not loopback, any, multicast or
/// link-local (upsd binds it by number).
pub fn valid_listen(s: &str) -> bool {
    match s.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => {
            !(a.is_loopback() || a.is_unspecified() || a.is_multicast() || a.is_link_local())
        }
        Ok(IpAddr::V6(a)) => {
            !(a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (a.segments()[0] & 0xffc0) == 0xfe80)
        }
        Err(_) => false,
    }
}

/// An allowed host or subnet in its canonical form (`192.168.1.0/24`, `10.0.0.5`,
/// `fd00::/64`); host bits of a subnet must be zero.
pub fn canonical_allow(s: &str) -> Option<String> {
    let s = s.trim();
    let (ip, prefix) = match s.split_once('/') {
        Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
        None => (s.parse::<IpAddr>().ok()?, None),
    };
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let Some(p) = prefix else {
        return Some(ip.to_string());
    };
    if p > max || p == 0 {
        return None;
    }
    let host_bits_zero = match ip {
        IpAddr::V4(a) => u32::from(a) & (u32::MAX.checked_shr(u32::from(p)).unwrap_or(0)) == 0,
        IpAddr::V6(a) => u128::from(a) & (u128::MAX.checked_shr(u32::from(p)).unwrap_or(0)) == 0,
    };
    if !host_bits_zero {
        return None;
    }
    Some(if p == max {
        ip.to_string()
    } else {
        format!("{ip}/{p}")
    })
}

/// Checks a value and puts it in its stored form (trimmed, canonical addresses); `foreign`
/// are the user names of the admin's own `upsd.users` sections. The error is
/// `(field, message)` with the field as in the JSON body.
pub fn normalize(
    c: &mut UpsNetServerConfig,
    foreign: &[String],
) -> Result<(), (String, String)> {
    let bad = |f: String, m: &str| Err((f, m.to_string()));
    if c.listen.len() > MAX_NET_ADDRS {
        return bad("listen".into(), "at most 16 addresses");
    }
    for (i, a) in c.listen.iter_mut().enumerate() {
        *a = a.trim().to_string();
        if !valid_listen(a) {
            return bad(
                format!("listen.{i}"),
                "an IP address of this NAS (not loopback or link-local)",
            );
        }
        *a = a.parse::<IpAddr>().map(|ip| ip.to_string()).unwrap_or_default();
    }
    c.listen.dedup();
    if c.allow.len() > MAX_NET_ADDRS {
        return bad("allow".into(), "at most 16 hosts or subnets");
    }
    for (i, a) in c.allow.iter_mut().enumerate() {
        match canonical_allow(a) {
            Some(v) => *a = v,
            None => {
                return bad(
                    format!("allow.{i}"),
                    "an IP address or a subnet such as 192.168.1.0/24",
                );
            }
        }
    }
    c.allow.dedup();
    if c.clients.len() > MAX_NET_CLIENTS {
        return bad("clients".into(), "at most 32 accounts");
    }
    for i in 0..c.clients.len() {
        let cl = &mut c.clients[i];
        cl.name = cl.name.trim().to_string();
        cl.note = cl.note.trim().to_string();
        if !valid_client_name(&cl.name) {
            return bad(
                format!("clients.{i}.name"),
                "a user name: a-z, 0-9, _ and -, at most 32 (not rynas or rynas-upsmon)",
            );
        }
        if foreign.contains(&cl.name) {
            return bad(
                format!("clients.{i}.name"),
                "upsd.users already has a user with this name that rynas does not manage",
            );
        }
        if cl.note.chars().count() > 64 || cl.note.chars().any(char::is_control) {
            return bad(format!("clients.{i}.note"), "at most 64 characters");
        }
        if let Some(p) = &cl.password
            && !valid_password(p)
        {
            return bad(
                format!("clients.{i}.password"),
                "12 to 64 letters, digits or ._~@%+=:,- (no spaces, quotes or #)",
            );
        }
        let name = c.clients[i].name.clone();
        if c.clients[..i].iter().any(|o| o.name == name) {
            return bad(format!("clients.{i}.name"), "user name given twice");
        }
    }
    Ok(())
}

/// The rynas block of `upsd.conf`.
pub fn render_upsd_conf(c: &UpsNetServerConfig) -> String {
    if !c.enabled {
        return "# Network UPS server of rynas (ups.netserver): off, upsd keeps its own setting\n\
                # (localhost only by default).\n"
            .into();
    }
    let mut s = String::from(
        "# Network UPS server of rynas (ups.netserver): other machines connect on this port.\n",
    );
    if c.listen.is_empty() {
        s.push_str(&format!("LISTEN 0.0.0.0 {NUT_PORT}\nLISTEN :: {NUT_PORT}\n"));
    } else {
        s.push_str(&format!("LISTEN 127.0.0.1 {NUT_PORT}\nLISTEN ::1 {NUT_PORT}\n"));
        for a in &c.listen {
            s.push_str(&format!("LISTEN {a} {NUT_PORT}\n"));
        }
    }
    s
}

/// Lines of the admin's `upsd.conf` the block replaces while the server is on.
pub fn upsd_conf_drops(c: &UpsNetServerConfig) -> Vec<String> {
    if c.enabled {
        vec!["LISTEN".into()]
    } else {
        Vec::new()
    }
}

/// The rynas block of `upsd.users`: `(name, role, password)` of each client.
pub fn render_users(clients: &[(String, UpsClientRole, String)]) -> String {
    let mut s = String::from("# Client accounts of the rynas network UPS server (ups.netserver).\n");
    for (name, role, pw) in clients {
        s.push_str(&format!("[{name}]\n\tpassword = \"{pw}\"\n"));
        if *role == UpsClientRole::Secondary {
            s.push_str("\tupsmon secondary\n");
        }
    }
    s
}

/// The systemd drop-in of `nut-server.service` that limits who reaches upsd (`None`: no
/// limit, the file must not exist).
pub fn render_dropin(c: &UpsNetServerConfig) -> Option<String> {
    if !c.enabled || c.allow.is_empty() {
        return None;
    }
    let mut s = String::from(
        "# Network UPS server of rynas (ups.netserver): only the NAS itself and these hosts\n\
         # reach upsd (systemd's per-service IP filter, systemd.resource-control(5)).\n\
         [Service]\nIPAddressDeny=any\nIPAddressAllow=localhost\n",
    );
    s.push_str(&format!("IPAddressAllow={}\n", c.allow.join(" ")));
    Some(s)
}

/// The `upsmon.conf` line of another Linux machine for a secondary account (`PASSWORD`
/// stands for the password).
pub fn monitor_line(ups: &str, host: &str, user: &str) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("MONITOR {ups}@{host} 1 {user} PASSWORD secondary")
}

/// User names of `upsd.users` sections outside rynas's blocks.
pub fn foreign_users(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_rynas = false;
    for raw in text.lines() {
        if raw.starts_with("# BEGIN rynas ") {
            in_rynas = true;
            continue;
        }
        if in_rynas {
            if raw.starts_with("# END rynas ") {
                in_rynas = false;
            }
            continue;
        }
        let l = raw.trim();
        if let Some(n) = l.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push(n.trim().to_string());
        }
    }
    out
}

/// `text` without rynas's block `id` (`None`: there is none).
pub fn without_block(text: &str, id: &str) -> Option<String> {
    let begin = format!("# BEGIN rynas {id} ");
    let end = format!("# END rynas {id}");
    let mut out = String::new();
    let mut inside = false;
    let mut found = false;
    for l in text.lines() {
        if inside {
            if l.trim_end() == end {
                inside = false;
            }
            continue;
        }
        if l.starts_with(&begin) {
            inside = true;
            found = true;
            continue;
        }
        out.push_str(l);
        out.push('\n');
    }
    found.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::ups::UpsNetClient;

    fn cfg() -> UpsNetServerConfig {
        UpsNetServerConfig {
            enabled: true,
            listen: vec![],
            allow: vec![" 192.168.1.0/24 ".into(), "fd00::5".into()],
            clients: vec![
                UpsNetClient {
                    name: "pc-office".into(),
                    ..Default::default()
                },
                UpsNetClient {
                    name: "homeassistant".into(),
                    role: UpsClientRole::Monitor,
                    note: " Home Assistant ".into(),
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn normalize_checks_and_canonicalizes() {
        let mut c = cfg();
        normalize(&mut c, &["admin".into()]).unwrap();
        assert_eq!(c.allow, ["192.168.1.0/24", "fd00::5"]);
        assert_eq!(c.clients[1].note, "Home Assistant");
        let mut bad = cfg();
        bad.allow = vec!["192.168.1.5/24".into()];
        assert_eq!(normalize(&mut bad, &[]).unwrap_err().0, "allow.0");
        let mut bad = cfg();
        bad.listen = vec!["127.0.0.1".into()];
        assert_eq!(normalize(&mut bad, &[]).unwrap_err().0, "listen.0");
        let mut bad = cfg();
        bad.listen = vec!["fe80::1".into()];
        assert_eq!(normalize(&mut bad, &[]).unwrap_err().0, "listen.0");
        let mut bad = cfg();
        bad.clients[0].name = "rynas-upsmon".into();
        assert_eq!(normalize(&mut bad, &[]).unwrap_err().0, "clients.0.name");
        let mut bad = cfg();
        bad.clients[1].name = "pc-office".into();
        assert_eq!(normalize(&mut bad, &[]).unwrap_err().0, "clients.1.name");
        let mut bad = cfg();
        assert_eq!(
            normalize(&mut bad, &["pc-office".into()]).unwrap_err().0,
            "clients.0.name",
            "the admin's own user"
        );
        let mut bad = cfg();
        bad.clients[0].password = Some("short".into());
        assert_eq!(normalize(&mut bad, &[]).unwrap_err().0, "clients.0.password");
        bad.clients[0].password = Some("has a space in it".into());
        assert_eq!(normalize(&mut bad, &[]).unwrap_err().0, "clients.0.password");
        bad.clients[0].password = Some("Good.Password-2026".into());
        assert!(normalize(&mut bad, &[]).is_ok());
        assert_eq!(canonical_allow("10.0.0.7/32").as_deref(), Some("10.0.0.7"));
        assert_eq!(canonical_allow("10.0.0.0/0"), None);
        assert_eq!(canonical_allow("nas.lan"), None);
    }

    #[test]
    fn upsd_conf_lines() {
        let mut c = cfg();
        assert_eq!(
            render_upsd_conf(&c).lines().skip(1).collect::<Vec<_>>(),
            ["LISTEN 0.0.0.0 3493", "LISTEN :: 3493"]
        );
        c.listen = vec!["192.168.1.10".into(), "fd00::10".into()];
        assert_eq!(
            render_upsd_conf(&c).lines().skip(1).collect::<Vec<_>>(),
            [
                "LISTEN 127.0.0.1 3493",
                "LISTEN ::1 3493",
                "LISTEN 192.168.1.10 3493",
                "LISTEN fd00::10 3493"
            ]
        );
        assert_eq!(upsd_conf_drops(&c), ["LISTEN"]);
        c.enabled = false;
        assert!(!render_upsd_conf(&c).contains("LISTEN "));
        assert!(upsd_conf_drops(&c).is_empty());
        assert_eq!(render_dropin(&c), None);
    }

    #[test]
    fn users_dropin_and_client_lines() {
        let u = render_users(&[
            ("pc-office".into(), UpsClientRole::Secondary, "pw1".into()),
            ("homeassistant".into(), UpsClientRole::Monitor, "pw2".into()),
        ]);
        assert!(u.contains("[pc-office]\n\tpassword = \"pw1\"\n\tupsmon secondary\n"));
        assert!(u.ends_with("[homeassistant]\n\tpassword = \"pw2\"\n"), "{u}");
        let mut c = cfg();
        normalize(&mut c, &[]).unwrap();
        let d = render_dropin(&c).unwrap();
        assert!(d.contains("[Service]\nIPAddressDeny=any\nIPAddressAllow=localhost\n"));
        assert!(d.contains("IPAddressAllow=192.168.1.0/24 fd00::5\n"));
        assert_eq!(
            monitor_line("ups", "192.168.1.10", "pc-office"),
            "MONITOR ups@192.168.1.10 1 pc-office PASSWORD secondary"
        );
        assert_eq!(
            monitor_line("ups", "fd00::10", "x"),
            "MONITOR ups@[fd00::10] 1 x PASSWORD secondary"
        );
        let users = "[admin]\n\tpassword = x\n# BEGIN rynas ups — m\n[rynas]\n# END rynas ups\n[other]\n";
        assert_eq!(foreign_users(users), ["admin", "other"]);
        assert_eq!(
            without_block(users, "ups").as_deref(),
            Some("[admin]\n\tpassword = x\n[other]\n")
        );
        assert_eq!(without_block(users, "nope"), None);
    }
}
