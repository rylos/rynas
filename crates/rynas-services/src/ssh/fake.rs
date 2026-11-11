//! A stand-in for sshd, systemctl, systemd-run and journalctl, for the tests and
//! `rynasd --dry-system`: `sshd -T` reads the files below the root the way OpenSSH does
//! (includes in order, first value wins, `Port`/`Allow*`/`Deny*` add up, a `Match` ends with
//! its file), systemctl keeps unit states in memory. Nothing of the host is run or read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::config::glob;
use super::{Out, SshRunner};

const MULTI: [&str; 5] = ["port", "allowgroups", "allowusers", "denyusers", "denygroups"];
const KNOWN: [&str; 31] = [
    "authenticationmethods",
    "port", "allowgroups", "allowusers", "denyusers", "denygroups", "include", "match",
    "passwordauthentication", "kbdinteractiveauthentication", "permitrootlogin",
    "permitemptypasswords", "pubkeyauthentication", "allowtcpforwarding",
    "allowstreamlocalforwarding", "permittunnel", "allowagentforwarding", "x11forwarding",
    "forcecommand", "permittty", "usepam", "printmotd", "acceptenv", "subsystem", "hostkey",
    "chrootdirectory", "listenaddress", "authorizedkeysfile", "loglevel", "maxauthtries",
    "clientaliveinterval",
];

#[derive(Default)]
struct State {
    /// unit → (enabled, active)
    units: BTreeMap<String, (bool, bool)>,
    calls: Vec<String>,
    /// `sshd -t` fails with this text.
    sshd_error: Option<String>,
    /// `systemctl start/reload ssh.service` leaves it stopped.
    fail_start: bool,
    journal: String,
}

pub struct FakeRunner {
    /// Paths below this are used as they are; others are taken below `root`.
    base: PathBuf,
    root: PathBuf,
    st: Mutex<State>,
}

impl FakeRunner {
    pub fn new(base: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        let f = FakeRunner { base: base.into(), root: root.into(), st: Mutex::new(State::default()) };
        f.set_unit("ssh.service", true, true);
        f.set_unit("ssh.socket", false, false);
        f
    }

    pub fn set_unit(&self, unit: &str, enabled: bool, active: bool) {
        self.st.lock().unwrap().units.insert(unit.into(), (enabled, active));
    }

    pub fn unit(&self, unit: &str) -> (bool, bool) {
        self.st.lock().unwrap().units.get(unit).copied().unwrap_or_default()
    }

    pub fn set_sshd_error(&self, e: Option<&str>) {
        self.st.lock().unwrap().sshd_error = e.map(String::from);
    }

    pub fn set_fail_start(&self, v: bool) {
        self.st.lock().unwrap().fail_start = v;
    }

    pub fn set_journal(&self, t: &str) {
        self.st.lock().unwrap().journal = t.into();
    }

    /// Every command run, as one line each.
    pub fn calls(&self) -> Vec<String> {
        self.st.lock().unwrap().calls.clone()
    }

    fn path(&self, p: &str) -> PathBuf {
        if Path::new(p).starts_with(&self.base) {
            PathBuf::from(p)
        } else {
            self.root.join(p.trim_start_matches('/'))
        }
    }

    /// The lines sshd reads, in order: `(keyword, value, in a Match block that applies to
    /// `who`)`. Without `who` (plain `sshd -T`) no Match block applies.
    fn lines_of(&self, file: &str, who: Option<&(String, Vec<String>)>, out: &mut Vec<(String, String, bool)>, check: &mut Vec<String>, depth: u32) {
        let Ok(text) = std::fs::read_to_string(self.path(file)) else { return };
        // None: global part; Some(applies)
        let mut in_match: Option<bool> = None;
        for l in text.lines() {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                continue;
            }
            let mut w = l.split_whitespace();
            let k = w.next().unwrap_or("").to_ascii_lowercase();
            let rest: Vec<&str> = w.collect();
            if !KNOWN.contains(&k.as_str()) {
                check.push(format!("{file}: Bad configuration option: {k}"));
            }
            if k == "match" {
                in_match = Some(who.is_some_and(|w| match_applies(&rest, w)));
                continue;
            }
            match in_match {
                Some(false) => continue,
                Some(true) => {
                    out.push((k, rest.join(" "), true));
                    continue;
                }
                None => {}
            }
            if k == "include" && depth < 8 {
                for arg in rest {
                    let full = if arg.starts_with('/') { arg.to_string() } else { format!("/etc/ssh/{arg}") };
                    let (dir, pat) = full.rsplit_once('/').unwrap_or(("", full.as_str()));
                    let mut names: Vec<String> = std::fs::read_dir(self.path(dir))
                        .map(|rd| rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
                        .unwrap_or_default();
                    names.retain(|n| glob(pat, n));
                    names.sort();
                    for n in names {
                        self.lines_of(&format!("{dir}/{n}"), who, out, check, depth + 1);
                    }
                }
                continue;
            }
            out.push((k, rest.join(" "), false));
        }
    }

    /// The groups of `user` from the files below the root (primary and supplementary).
    fn groups_of(&self, user: &str) -> Vec<String> {
        let read = |p: &str| std::fs::read_to_string(self.root.join(p)).unwrap_or_default();
        let gid = read("etc/passwd").lines().find_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.first() == Some(&user)).then(|| f.get(3).map(|g| g.to_string())).flatten()
        });
        read("etc/group")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(':').collect();
                let member = f.get(3).is_some_and(|m| m.split(',').any(|x| x == user));
                (member || f.get(2).map(|g| g.to_string()) == gid).then(|| f[0].to_string())
            })
            .collect()
    }

    fn sshd(&self, argv: &[&str]) -> Out {
        let main = argv
            .iter()
            .position(|a| *a == "-f")
            .and_then(|i| argv.get(i + 1))
            .copied()
            .unwrap_or("/etc/ssh/sshd_config");
        // `-C user=NAME,host=…,addr=…`: the Match blocks for that user apply
        let who = argv
            .iter()
            .position(|a| *a == "-C")
            .and_then(|i| argv.get(i + 1))
            .and_then(|spec| spec.split(',').find_map(|kv| kv.strip_prefix("user=")))
            .map(|u| (u.to_string(), self.groups_of(u)));
        let mut lines = Vec::new();
        let mut errors = Vec::new();
        self.lines_of(main, who.as_ref(), &mut lines, &mut errors, 0);
        if let Some(e) = self.st.lock().unwrap().sshd_error.clone() {
            errors.push(e);
        }
        if argv.contains(&"-t") {
            return if errors.is_empty() { ok("") } else { Out { code: Some(255), stdout: String::new(), stderr: errors.join("\n") } };
        }
        let mut single: BTreeMap<String, String> = BTreeMap::new();
        let mut multi: Vec<(String, String)> = Vec::new();
        // a Match block that applies wins over the global lines (its first value)
        for (k, v, _) in lines.iter().filter(|l| l.2) {
            if MULTI.contains(&k.as_str()) {
                for x in v.split_whitespace() {
                    multi.push((k.clone(), x.to_string()));
                }
            } else {
                single.entry(k.clone()).or_insert(v.clone());
            }
        }
        let from_match: Vec<String> = multi.iter().map(|(k, _)| k.clone()).collect();
        for (k, v, _) in lines.iter().filter(|l| !l.2) {
            if MULTI.contains(&k.as_str()) {
                if from_match.contains(k) {
                    continue;
                }
                for x in v.split_whitespace() {
                    multi.push((k.clone(), x.to_string()));
                }
            } else {
                single.entry(k.clone()).or_insert(v.clone());
            }
        }
        for (k, v) in [
            ("passwordauthentication", "yes"),
            ("pubkeyauthentication", "yes"),
            ("kbdinteractiveauthentication", "yes"),
            ("permitrootlogin", "without-password"),
            ("allowtcpforwarding", "yes"),
            ("allowagentforwarding", "yes"),
            ("x11forwarding", "no"),
            ("authorizedkeysfile", ".ssh/authorized_keys .ssh/authorized_keys2"),
            ("authenticationmethods", "any"),
        ] {
            single.entry(k.into()).or_insert(v.into());
        }
        if !multi.iter().any(|(k, _)| k == "port") {
            multi.insert(0, ("port".into(), "22".into()));
        }
        let mut s = String::new();
        for (k, v) in multi.iter().map(|(k, v)| (k, v)).chain(single.iter()) {
            s.push_str(&format!("{k} {v}\n"));
        }
        ok(&s)
    }

    fn systemctl(&self, argv: &[&str]) -> Out {
        let mut st = self.st.lock().unwrap();
        let unit = argv.last().copied().unwrap_or("").to_string();
        let now = argv.contains(&"--now");
        let fail_start = st.fail_start;
        let e = st.units.entry(unit.clone()).or_default();
        match argv.get(1).copied().unwrap_or("") {
            "is-enabled" => return if e.0 { ok("enabled\n") } else { Out { code: Some(1), stdout: "disabled\n".into(), stderr: String::new() } },
            "is-active" => return if e.1 { ok("active\n") } else { Out { code: Some(3), stdout: "inactive\n".into(), stderr: String::new() } },
            "enable" => {
                e.0 = true;
                if now {
                    e.1 = !(fail_start && unit == "ssh.service");
                }
            }
            "disable" => {
                e.0 = false;
                if now {
                    e.1 = false;
                }
            }
            "start" | "reload" | "restart" => {
                if fail_start && unit == "ssh.service" {
                    e.1 = false;
                    return Out { code: Some(1), stdout: String::new(), stderr: "Job failed".into() };
                }
                e.1 = true;
            }
            "stop" => e.1 = false,
            _ => {}
        }
        ok("")
    }
}

/// Whether the criteria of a `Match` line (`User a,b`, `Group g`, `All`; pairs all apply)
/// hold for `(user, groups)`.
fn match_applies(criteria: &[&str], who: &(String, Vec<String>)) -> bool {
    let mut it = criteria.iter();
    while let Some(c) = it.next() {
        if c.eq_ignore_ascii_case("all") {
            continue;
        }
        let Some(arg) = it.next() else { return false };
        let any = |set: &[String]| arg.split(',').any(|p| set.iter().any(|x| glob(p, x)));
        let hit = match c.to_ascii_lowercase().as_str() {
            "user" => any(std::slice::from_ref(&who.0)),
            "group" => any(&who.1),
            // host, address, …: the fake says "localhost", 127.0.0.1
            "host" => arg.split(',').any(|p| glob(p, "localhost")),
            "address" => arg.split(',').any(|p| glob(p, "127.0.0.1")),
            _ => false,
        };
        if !hit {
            return false;
        }
    }
    true
}

fn ok(s: &str) -> Out {
    Out { code: Some(0), stdout: s.into(), stderr: String::new() }
}

impl SshRunner for FakeRunner {
    fn run(&self, argv: &[&str]) -> std::result::Result<Out, String> {
        self.st.lock().unwrap().calls.push(argv.join(" "));
        Ok(match argv.first().copied().unwrap_or("") {
            "sshd" => self.sshd(argv),
            "systemctl" => self.systemctl(argv),
            "journalctl" => ok(&self.st.lock().unwrap().journal.clone()),
            _ => ok(""),
        })
    }

    fn simulated(&self) -> bool {
        true
    }
}

/// Debian's `sshd_config` (trixie), shortened.
pub const DEBIAN_MAIN: &str = "Include /etc/ssh/sshd_config.d/*.conf\n\n#Port 22\nKbdInteractiveAuthentication no\nUsePAM yes\nX11Forwarding yes\nPrintMotd no\nAcceptEnv LANG LC_* COLORTERM NO_COLOR\nSubsystem sftp /usr/lib/openssh/sftp-server\n";

/// A machine below `root` for `--dry-system`: Debian's sshd_config, an ed25519 host key, an
/// sshd binary, an administrator; written only where missing.
pub fn seed(root: &Path) -> std::io::Result<()> {
    let put = |p: &str, t: &str| -> std::io::Result<()> {
        let f = root.join(p);
        if !f.exists() {
            std::fs::create_dir_all(f.parent().expect("has a parent"))?;
            std::fs::write(f, t)?;
        }
        Ok(())
    };
    put("etc/ssh/sshd_config", DEBIAN_MAIN)?;
    std::fs::create_dir_all(root.join("etc/ssh/sshd_config.d"))?;
    put(
        "etc/ssh/ssh_host_ed25519_key.pub",
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICqfkRRvTxTaXrS5r1iAceZ/JZlbIfF/uX70TgYNhG4U root@nas\n",
    )?;
    put("usr/sbin/sshd", "")?;
    put("etc/passwd", "root:x:0:0:root:/root:/bin/bash\nadmin:x:1000:100:Admin:/home/admin:/bin/bash\n")?;
    put("etc/group", "root:x:0:\nusers:x:100:\nsudo:x:27:admin\nrynas-admin:x:990:admin\nrynas-ssh:x:991:admin\n")?;
    Ok(())
}
