//! synodeb's network UPS server (synodeb 4.6, `synodeb-ups netserver`), the
//! `rynas_module::ups::NetServerLayer` of [`crate::ups::SynodebUps`]. rynas keeps the setting
//! (`ups.netserver`, passwords sealed) and synodeb applies it: upsd on every address, synodeb's
//! own nftables table lets only the allowed hosts reach port 3493.
//!
//! - `synodeb-ups netserver status --json` (schema 1) → the state; a synodeb before 4.6 does
//!   not know the subcommand ([`NetLayerRead::Unsupported`]).
//! - `synodeb-ups netserver set --client IP|CIDR … --user NAME:secondary|readonly …`: the whole
//!   set of clients and users; stdin carries one `NAME PASSWORD` line per user whose password
//!   is new or changed (never argv). Exit 0 applied (with NUT off: saved, applied when it
//!   runs), 2 refused values (file untouched), 1 not applied.
//! - `synodeb-ups netserver off`: upsd only on localhost, the extra users removed.
//! - Roles: `secondary` (upsmon secondary, LOGIN allowed) is rynas's
//!   [`UpsClientRole::Secondary`]; `readonly` (no rights, LOGIN refused; reading needs no
//!   login in NUT) is [`UpsClientRole::Monitor`]. synodeb's own `monuser` (upsmon primary, the
//!   account Synology NAS clients expect) is reported, its password set with
//!   `synodeb-ups passwd`.
//! - `--dry-system`: [`dry_netserver`] answers like synodeb 4.6 with its state in the dry root
//!   (`/etc/synodeb/ups-network.conf`, a fake format without passwords); a file
//!   `/etc/synodeb/fake-version` with `4.5` makes it a synodeb before the server.

use std::path::Path;
use std::time::Duration;

use rynas_api_types::ups::{UpsClientRole, UpsNetServerConfig, UpsNetSource};
use rynas_module::ups::{NetLayerRead, NetLayerStatus, NetServerLayer};
use rynas_tasks::{Cmd, CmdOutput};
use serde::Deserialize;

use crate::ups::SynodebUps;

/// synodeb's file of the server (it wins over `NETWORK_CLIENTS` of `ups.conf`).
pub const NET_CONF: &str = "/etc/synodeb/ups-network.conf";
/// The only schema of `status --json` this rynas reads.
pub const STATUS_SCHEMA: u64 = 1;
/// User names synodeb keeps for itself.
pub const RESERVED_USERS: [&str; 2] = ["monuser", "admin"];
/// synodeb's limit on clients (rynas's own limit of 16 is lower).
pub const MAX_CLIENTS: usize = 64;
const SHORT: Duration = Duration::from_secs(20);
/// `set` restarts nut-server and loads the firewall.
const APPLY: Duration = Duration::from_secs(90);

/// A synodeb user name: `[a-z][a-z0-9_]{0,31}`, not `monuser` or `admin`.
pub fn valid_user(n: &str) -> bool {
    let b = n.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        && !RESERVED_USERS.contains(&n)
}

/// A password synodeb accepts: 1–64 printable ASCII, no space, `"`, `#` or `\`.
pub fn valid_password(p: &str) -> bool {
    (1..=64).contains(&p.len())
        && p.bytes().all(|c| c.is_ascii_graphic() && !b"\"#\\".contains(&c))
}

fn role_arg(r: UpsClientRole) -> &'static str {
    match r {
        UpsClientRole::Secondary => "secondary",
        UpsClientRole::Monitor => "readonly",
    }
}

fn role_of(s: &str) -> UpsClientRole {
    // an unknown role of a newer synodeb: the one with no rights
    if s == "secondary" { UpsClientRole::Secondary } else { UpsClientRole::Monitor }
}

#[derive(Deserialize)]
struct UserDoc {
    name: String,
    #[serde(default)]
    role: String,
}

#[derive(Deserialize)]
struct StatusDoc {
    enabled: bool,
    #[serde(default)]
    listen: Vec<String>,
    #[serde(default)]
    clients: Vec<String>,
    #[serde(default)]
    users: Vec<UserDoc>,
    #[serde(default)]
    monuser: bool,
    #[serde(default)]
    firewall_active: bool,
    #[serde(default)]
    upsd_active: bool,
    source: Option<String>,
}

/// The state of `synodeb-ups netserver status --json`.
pub fn parse_status(out: &CmdOutput) -> NetLayerRead {
    if !out.success() {
        let err = format!("{}{}", out.stderr, out.stdout).to_ascii_lowercase();
        // a synodeb before 4.6: usage error on the unknown subcommand
        if out.code == Some(2) || err.contains("unknown") || err.contains("usage") {
            return NetLayerRead::Unsupported;
        }
        let last = out.stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
        return NetLayerRead::Unreadable(format!(
            "synodeb-ups netserver status: exit code {}: {last}",
            out.code.map_or("-".into(), |c| c.to_string())
        ));
    }
    let v: serde_json::Value = match serde_json::from_str(out.stdout.trim()) {
        Ok(v) => v,
        Err(e) => return NetLayerRead::Unreadable(format!("synodeb-ups netserver status: {e}")),
    };
    match v.get("schema").and_then(serde_json::Value::as_u64) {
        Some(STATUS_SCHEMA) => {}
        s => {
            return NetLayerRead::Unreadable(format!(
                "synodeb-ups netserver status: unknown schema {} (this rynas reads {STATUS_SCHEMA})",
                s.map_or("-".into(), |s| s.to_string())
            ));
        }
    }
    let d: StatusDoc = match serde_json::from_value(v) {
        Ok(d) => d,
        Err(e) => return NetLayerRead::Unreadable(format!("synodeb-ups netserver status: {e}")),
    };
    NetLayerRead::Status(NetLayerStatus {
        enabled: d.enabled,
        listen: d.listen,
        allow: d.clients,
        users: d.users.into_iter().map(|u| (u.name, role_of(&u.role))).collect(),
        primary_account: d.monuser.then(|| "monuser".to_string()),
        firewall_active: d.firewall_active,
        server_active: d.upsd_active,
        source: match d.source.as_deref() {
            None => UpsNetSource::None,
            Some("ups-network.conf") => UpsNetSource::Own,
            Some("ups.conf") => UpsNetSource::Manual,
            Some(_) => UpsNetSource::Other,
        },
    })
}

/// synodeb's limits on a value rynas already normalized.
pub fn check(c: &UpsNetServerConfig) -> Result<(), (String, String)> {
    if c.enabled && c.allow.is_empty() {
        return Err((
            "allow".into(),
            "at least one host or subnet: synodeb's firewall lets only these reach port 3493".into(),
        ));
    }
    if c.allow.len() > MAX_CLIENTS {
        return Err(("allow".into(), format!("at most {MAX_CLIENTS} hosts or subnets")));
    }
    for (i, cl) in c.clients.iter().enumerate() {
        if !valid_user(&cl.name) {
            return Err((
                format!("clients.{i}.name"),
                "a user name: starts with a-z, then a-z, 0-9 and _, at most 32 (not monuser or admin)"
                    .into(),
            ));
        }
        if let Some(p) = &cl.password
            && !valid_password(p)
        {
            return Err((
                format!("clients.{i}.password"),
                "1 to 64 printable characters, no space, \", # or \\".into(),
            ));
        }
    }
    Ok(())
}

impl SynodebUps {
    fn net(&self) -> Cmd {
        Cmd::new(&self.tool).env_clear().arg("netserver")
    }
}

impl NetServerLayer for SynodebUps {
    fn status_cmd(&self) -> Cmd {
        self.net().args(["status", "--json"]).timeout(SHORT)
    }

    fn parse_status(&self, out: &CmdOutput) -> NetLayerRead {
        parse_status(out)
    }

    fn check(&self, c: &UpsNetServerConfig) -> Result<(), (String, String)> {
        check(c)
    }

    fn set_cmd(
        &self,
        allow: &[String],
        users: &[(String, UpsClientRole)],
        passwords: &[(String, String)],
    ) -> Cmd {
        let mut c = self.net().arg("set").timeout(APPLY);
        for a in allow {
            c = c.args(["--client", a]);
        }
        for (n, r) in users {
            c = c.args(["--user", &format!("{n}:{}", role_arg(*r))]);
        }
        let lines: String = passwords.iter().map(|(n, p)| format!("{n} {p}\n")).collect();
        c.stdin(lines)
    }

    fn off_cmd(&self) -> Cmd {
        self.net().arg("off").timeout(APPLY)
    }
}

// ---------------------------------------------------------------------------------------------
// --dry-system: a fake synodeb 4.6
// ---------------------------------------------------------------------------------------------

/// The fake's state (no passwords: users with one are the users listed).
#[derive(Default)]
struct Fake {
    enabled: bool,
    clients: Vec<String>,
    users: Vec<(String, String)>,
}

fn fake_read(root: &Path) -> Option<Fake> {
    let text = std::fs::read_to_string(root.join(&NET_CONF[1..])).ok()?;
    let mut f = Fake::default();
    for l in text.lines() {
        match l.split_once('=') {
            Some(("ENABLED", v)) => f.enabled = v.trim() == "1",
            Some(("CLIENTS", v)) => f.clients = v.split_whitespace().map(str::to_string).collect(),
            Some(("USERS", v)) => {
                f.users = v
                    .split_whitespace()
                    .filter_map(|u| u.split_once(':'))
                    .map(|(n, r)| (n.to_string(), r.to_string()))
                    .collect()
            }
            _ => {}
        }
    }
    Some(f)
}

fn fake_write(root: &Path, f: &Fake) -> std::io::Result<()> {
    let p = root.join(&NET_CONF[1..]);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let users: Vec<String> = f.users.iter().map(|(n, r)| format!("{n}:{r}")).collect();
    std::fs::write(
        p,
        format!(
            "# fake synodeb-ups netserver state (rynasd --dry-system)\nENABLED={}\nCLIENTS={}\nUSERS={}\n",
            u8::from(f.enabled),
            f.clients.join(" "),
            users.join(" ")
        ),
    )
}

/// `NETWORK_CLIENTS=` of synodeb's `ups.conf` (set by hand).
fn manual_clients(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(&crate::ups::UPS_CONF[1..]))
        .unwrap_or_default()
        .lines()
        .rev()
        .filter(|l| !l.trim_start().starts_with('#'))
        .find_map(|l| l.trim().strip_prefix("NETWORK_CLIENTS="))
        .map(|v| v.trim_matches(['"', '\'']).split([' ', ',']).filter(|w| !w.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// `synodeb-ups netserver …` under `--dry-system` (`root`: the dry root; `None`: no state, a
/// server that is off).
pub fn dry_netserver(root: Option<&Path>, cmd: &Cmd) -> CmdOutput {
    let out = |code: i32, stdout: &str, stderr: &str| CmdOutput {
        code: Some(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    };
    let old = root
        .and_then(|r| std::fs::read_to_string(r.join("etc/synodeb/fake-version")).ok())
        .is_some_and(|v| v.trim() < "4.6");
    if old {
        return out(2, "", "synodeb-ups: unknown command 'netserver'\nusage: synodeb-ups status|test|setup|probe|passwd ...\n");
    }
    let line = cmd.display();
    let args: Vec<&str> = line.split_whitespace().skip(2).collect();
    let saved = root.and_then(fake_read);
    let has_file = saved.is_some();
    let mut f = saved.unwrap_or_default();
    match args.first().copied() {
        Some("status") => {
            let manual = root.map(manual_clients).unwrap_or_default();
            let (clients, source) = if has_file {
                (f.clients.clone(), "\"ups-network.conf\"".to_string())
            } else if !manual.is_empty() {
                (manual, "\"ups.conf\"".to_string())
            } else {
                (Vec::new(), "null".to_string())
            };
            let enabled = if has_file { f.enabled } else { !clients.is_empty() };
            let doc = serde_json::json!({
                "schema": 1,
                "enabled": enabled,
                "listen": if enabled { vec!["0.0.0.0", "::"] } else { vec!["127.0.0.1", "::1"] },
                "clients": clients,
                "users": f.users.iter().map(|(n, r)| serde_json::json!({"name": n, "role": r})).collect::<Vec<_>>(),
                "monuser": true,
                "firewall_active": enabled,
                "upsd_active": true,
            });
            let text = doc.to_string();
            // `source` last, as a raw value
            out(0, &format!("{},\"source\":{source}}}\n", &text[..text.len() - 1]), "")
        }
        Some("set") => {
            let mut clients = Vec::new();
            let mut users: Vec<(String, String)> = Vec::new();
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                match (*a, it.next()) {
                    ("--client", Some(v)) => clients.push(v.to_string()),
                    ("--user", Some(v)) => match v.split_once(':') {
                        Some((n, r)) if valid_user(n) && ["secondary", "readonly"].contains(&r) => {
                            users.push((n.into(), r.into()))
                        }
                        _ => return out(2, "", &format!("synodeb-ups: invalid user '{v}'\n")),
                    },
                    _ => return out(2, "", &format!("synodeb-ups: invalid argument '{a}'\n")),
                }
            }
            if clients.is_empty() {
                return out(2, "", "synodeb-ups: at least one --client is needed\n");
            }
            let stdin = String::from_utf8_lossy(cmd.stdin_data().unwrap_or_default()).into_owned();
            let given: Vec<&str> = stdin.lines().filter_map(|l| l.split_once(' ')).map(|(n, _)| n).collect();
            if let Some((n, _)) = users.iter().find(|(n, _)| !given.contains(&n.as_str()) && !f.users.iter().any(|(o, _)| o == n)) {
                return out(2, "", &format!("synodeb-ups: new user '{n}' needs a password on stdin\n"));
            }
            f = Fake { enabled: true, clients, users };
            if let Some(r) = root
                && let Err(e) = fake_write(r, &f)
            {
                return out(1, "", &format!("synodeb-ups: {e}\n"));
            }
            out(0, &format!("network UPS server on: {} clients, {} users\n", f.clients.len(), f.users.len()), "")
        }
        Some("off") => {
            f.enabled = false;
            f.users.clear();
            if let Some(r) = root
                && let Err(e) = fake_write(r, &f)
            {
                return out(1, "", &format!("synodeb-ups: {e}\n"));
            }
            out(0, "network UPS server off: upsd listens on localhost only\n", "")
        }
        _ => out(2, "", "usage: synodeb-ups netserver set|off|status\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::ups::UpsNetClient;

    fn ok(stdout: &str) -> CmdOutput {
        CmdOutput { code: Some(0), stdout: stdout.into(), stderr: String::new() }
    }

    #[test]
    fn status_schema_roles_and_sources() {
        let doc = r#"{"schema":1,"enabled":true,"listen":["0.0.0.0","::"],"clients":["192.168.1.0/24","10.0.0.5"],
            "users":[{"name":"pc","role":"secondary"},{"name":"ha","role":"readonly"}],"monuser":true,
            "firewall_active":true,"upsd_active":false,"source":"ups-network.conf"}"#;
        let NetLayerRead::Status(s) = parse_status(&ok(doc)) else { panic!() };
        assert!(s.enabled && s.firewall_active && !s.server_active);
        assert_eq!(s.allow, ["192.168.1.0/24", "10.0.0.5"]);
        assert_eq!(s.users, [("pc".into(), UpsClientRole::Secondary), ("ha".into(), UpsClientRole::Monitor)]);
        assert_eq!(s.primary_account.as_deref(), Some("monuser"));
        assert_eq!(s.source, UpsNetSource::Own);
        let manual = r#"{"schema":1,"enabled":true,"clients":["10.0.0.5"],"users":[],"monuser":false,"firewall_active":true,"upsd_active":true,"source":"ups.conf"}"#;
        let NetLayerRead::Status(s) = parse_status(&ok(manual)) else { panic!() };
        assert_eq!(s.source, UpsNetSource::Manual);
        assert!(s.primary_account.is_none());
        let off = r#"{"schema":1,"enabled":false,"source":null}"#;
        let NetLayerRead::Status(s) = parse_status(&ok(off)) else { panic!() };
        assert_eq!(s.source, UpsNetSource::None);
        // an unknown schema is not read
        let r = parse_status(&ok(r#"{"schema":2,"enabled":true}"#));
        assert!(matches!(r, NetLayerRead::Unreadable(ref m) if m.contains("schema 2")), "{r:?}");
        assert!(matches!(parse_status(&ok("not json")), NetLayerRead::Unreadable(_)));
        // synodeb before 4.6
        let old = CmdOutput { code: Some(2), stdout: String::new(), stderr: "unknown command 'netserver'\n".into() };
        assert_eq!(parse_status(&old), NetLayerRead::Unsupported);
        let failed = CmdOutput { code: Some(1), stdout: String::new(), stderr: "must be root\n".into() };
        assert!(matches!(parse_status(&failed), NetLayerRead::Unreadable(ref m) if m.contains("must be root")));
    }

    #[test]
    fn commands_keep_passwords_off_the_command_line() {
        let l = SynodebUps::default();
        let c = l.set_cmd(
            &["192.168.1.0/24".into(), "10.0.0.5".into()],
            &[("pc".into(), UpsClientRole::Secondary), ("ha".into(), UpsClientRole::Monitor)],
            &[("ha".into(), "s3cret-pw".into())],
        );
        assert_eq!(
            c.display(),
            "/usr/sbin/synodeb-ups netserver set --client 192.168.1.0/24 --client 10.0.0.5 --user pc:secondary --user ha:readonly"
        );
        assert!(!c.display().contains("s3cret"));
        assert_eq!(c.stdin_data(), Some(&b"ha s3cret-pw\n"[..]));
        assert_eq!(l.off_cmd().display(), "/usr/sbin/synodeb-ups netserver off");
        assert_eq!(l.status_cmd().display(), "/usr/sbin/synodeb-ups netserver status --json");
    }

    #[test]
    fn synodeb_limits() {
        assert!(valid_user("pc_1") && !valid_user("1pc") && !valid_user("pc-1") && !valid_user("monuser") && !valid_user("admin"));
        assert!(valid_password("a") && !valid_password("a b") && !valid_password("a#b") && !valid_password("a\"b") && !valid_password("a\\b"));
        let mut c = UpsNetServerConfig { enabled: true, ..Default::default() };
        assert_eq!(check(&c).unwrap_err().0, "allow");
        c.allow = vec!["10.0.0.5".into()];
        c.clients = vec![UpsNetClient { name: "my-pc".into(), ..Default::default() }];
        assert_eq!(check(&c).unwrap_err().0, "clients.0.name");
        c.clients[0].name = "my_pc".into();
        assert!(check(&c).is_ok());
        // off: no host needed
        assert!(check(&UpsNetServerConfig::default()).is_ok());
    }

    #[test]
    fn fake_keeps_state_and_wants_passwords_of_new_users() {
        let d = tempfile::tempdir().unwrap();
        let l = SynodebUps::default();
        let status = |root: &Path| match parse_status(&dry_netserver(Some(root), &l.status_cmd())) {
            NetLayerRead::Status(s) => s,
            r => panic!("{r:?}"),
        };
        assert!(!status(d.path()).enabled);
        // NETWORK_CLIENTS set by hand in ups.conf
        std::fs::create_dir_all(d.path().join("etc/synodeb")).unwrap();
        std::fs::write(d.path().join("etc/synodeb/ups.conf"), "NETWORK_CLIENTS=\"10.0.0.7\"\n").unwrap();
        let s = status(d.path());
        assert_eq!((s.enabled, s.source, s.allow.clone()), (true, UpsNetSource::Manual, vec!["10.0.0.7".to_string()]));
        let users = [("pc".to_string(), UpsClientRole::Secondary)];
        let allow = ["10.0.0.0/24".to_string()];
        assert_eq!(dry_netserver(Some(d.path()), &l.set_cmd(&allow, &users, &[])).code, Some(2));
        let r = dry_netserver(Some(d.path()), &l.set_cmd(&allow, &users, &[("pc".into(), "pw".into())]));
        assert_eq!(r.code, Some(0), "{r:?}");
        let s = status(d.path());
        assert_eq!((s.enabled, s.source, s.users.len()), (true, UpsNetSource::Own, 1));
        // known user: no password needed
        assert_eq!(dry_netserver(Some(d.path()), &l.set_cmd(&allow, &users, &[])).code, Some(0));
        assert!(!std::fs::read_to_string(d.path().join("etc/synodeb/ups-network.conf")).unwrap().contains("pw\n"));
        assert_eq!(dry_netserver(Some(d.path()), &l.off_cmd()).code, Some(0));
        let s = status(d.path());
        assert!(!s.enabled && s.users.is_empty() && s.source == UpsNetSource::Own);
        std::fs::write(d.path().join("etc/synodeb/fake-version"), "4.5\n").unwrap();
        assert_eq!(parse_status(&dry_netserver(Some(d.path()), &l.status_cmd())), NetLayerRead::Unsupported);
    }
}
