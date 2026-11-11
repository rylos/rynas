//! sshd configuration text: the drop-in rynas writes, the main file rewritten to check a
//! candidate drop-in in place, and the effective configuration (`sshd -T`) read back.
//!
//! Facts checked against OpenSSH 10 (`sshd -t`/`-T` on temporary files):
//! - most keywords keep the first value read, so the drop-in sorts first (`00-`) and Debian's
//!   `sshd_config` includes `sshd_config.d/*.conf` on its first line;
//! - `Port`, `AllowGroups`, `AllowUsers`, `DenyUsers`, `DenyGroups` add up across files: a
//!   `Port` in another file is an extra port, an `AllowUsers` elsewhere narrows the list;
//! - a `Match` block ends with the file that opens it: lines of `sshd_config` after the
//!   `Include` stay global;
//! - a second `Subsystem sftp` is skipped, not an error.

use std::collections::BTreeSet;

use rynas_api_types::ssh::{SSH_BASE_GROUPS, SSH_SFTP_GROUP, SshAccount, SshAllow, SshLevel, SshRootLogin, SshSettings};

/// The drop-in's file name: sorts before the usual `50-cloud-init.conf` and friends.
pub const DROPIN_NAME: &str = "00-rynas.conf";
pub const DROPIN_DIR: &str = "/etc/ssh/sshd_config.d";
pub const MAIN: &str = "/etc/ssh/sshd_config";
pub const HEADER: &str = "# Managed by rynas (Services > SSH): changes here are replaced.\n";

/// The drop-in for `s`.
pub fn render(s: &SshSettings) -> String {
    let yn = |b: bool| if b { "yes" } else { "no" };
    let root = match s.root_login {
        SshRootLogin::No => "no",
        SshRootLogin::Keys => "prohibit-password",
        SshRootLogin::Yes => "yes",
    };
    let mut t = String::from(HEADER);
    t.push_str(
        "# Read before every other file of sshd_config.d: sshd keeps the first value it reads.\n",
    );
    t.push_str(&format!("Port {}\n", s.port));
    t.push_str(&format!("PermitRootLogin {root}\n"));
    t.push_str(&format!("PasswordAuthentication {}\n", yn(s.password_auth)));
    // with UsePAM, keyboard-interactive is a second way to send a password
    t.push_str(&format!("KbdInteractiveAuthentication {}\n", yn(s.password_auth)));
    t.push_str("PermitEmptyPasswords no\n");
    t.push_str("PubkeyAuthentication yes\n");
    t.push_str(&format!("AllowTcpForwarding {}\n", yn(s.tcp_forwarding)));
    t.push_str(&format!("AllowStreamLocalForwarding {}\n", yn(s.tcp_forwarding)));
    t.push_str(&format!("PermitTunnel {}\n", yn(s.tcp_forwarding)));
    t.push_str(&format!("AllowAgentForwarding {}\n", yn(s.agent_forwarding)));
    t.push_str(&format!("X11Forwarding {}\n", yn(s.x11_forwarding)));
    if s.allow == SshAllow::Listed {
        t.push_str(&format!("AllowGroups {}\n", allowed_groups(s).join(" ")));
    }
    t.push_str(&format!(
        "\n# SFTP only for {SSH_SFTP_GROUP}: no terminal, no forwarding.\nMatch Group {SSH_SFTP_GROUP}\n"
    ));
    for l in [
        "ForceCommand internal-sftp",
        "PermitTTY no",
        "AllowTcpForwarding no",
        "AllowStreamLocalForwarding no",
        "PermitTunnel no",
        "AllowAgentForwarding no",
        "X11Forwarding no",
    ] {
        t.push_str("    ");
        t.push_str(l);
        t.push('\n');
    }
    t
}

pub use rynas_api_types::ssh::allowed_groups;

/// sshd's `*` and `?` patterns.
pub fn glob(p: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    go(p.as_bytes(), s.as_bytes())
}

/// The configuration files sshd reads, in order, and whether the drop-in is one of them.
/// `list_dir` gives the file names of a directory (sorted or not).
pub struct Layout {
    /// `(path, text)` in reading order; the main file's lines are split around its
    /// `Include` lines (`main#0`, `main#1`…), so the order of keywords is kept.
    pub files: Vec<(String, String)>,
    pub includes_dropin: bool,
}

/// The paths an `Include` argument names (relative ones are below `/etc/ssh`); only a glob
/// in the last component is expanded.
fn expand(arg: &str, list_dir: &dyn Fn(&str) -> Vec<String>) -> Vec<String> {
    let full = if arg.starts_with('/') { arg.to_string() } else { format!("/etc/ssh/{arg}") };
    let (dir, pat) = full.rsplit_once('/').unwrap_or(("", full.as_str()));
    if !pat.contains(['*', '?']) {
        return vec![full.clone()];
    }
    let mut names: Vec<String> = list_dir(dir).into_iter().filter(|n| glob(pat, n)).collect();
    names.sort();
    names.into_iter().map(|n| format!("{dir}/{n}")).collect()
}

/// Does this `Include` argument name the drop-in (whether it exists yet or not)?
fn names_dropin(arg: &str) -> bool {
    let full = if arg.starts_with('/') { arg.to_string() } else { format!("/etc/ssh/{arg}") };
    let (dir, pat) = full.rsplit_once('/').unwrap_or(("", full.as_str()));
    dir == DROPIN_DIR && glob(pat, DROPIN_NAME)
}

/// Splits the main file around its `Include` lines and expands them.
pub fn layout(
    main: &str,
    list_dir: &dyn Fn(&str) -> Vec<String>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Layout {
    let dropin = format!("{DROPIN_DIR}/{DROPIN_NAME}");
    let mut files = Vec::new();
    let mut part = String::new();
    let mut n = 0;
    let mut includes_dropin = false;
    for line in main.lines() {
        let mut w = line.split_whitespace();
        if w.next().is_some_and(|k| k.eq_ignore_ascii_case("include")) {
            files.push((format!("{MAIN}#{n}"), std::mem::take(&mut part)));
            n += 1;
            for arg in w {
                includes_dropin |= names_dropin(arg);
                for p in expand(arg, list_dir) {
                    includes_dropin |= p == dropin;
                    if let Some(t) = read(&p) {
                        files.push((p, t));
                    }
                }
            }
        } else {
            part.push_str(line);
            part.push('\n');
        }
    }
    files.push((format!("{MAIN}#{n}"), part));
    Layout { files, includes_dropin }
}

/// The main file with each `Include` written out as one line per file it names, the drop-in
/// replaced by `candidate`: `sshd -t -f` / `-T -f` on it read the candidate exactly where
/// the drop-in will be.
pub fn candidate_main(
    main: &str,
    candidate: &str,
    list_dir: &dyn Fn(&str) -> Vec<String>,
) -> String {
    let dropin = format!("{DROPIN_DIR}/{DROPIN_NAME}");
    let mut out = String::new();
    let mut placed = false;
    for line in main.lines() {
        let mut w = line.split_whitespace();
        if w.next().is_some_and(|k| k.eq_ignore_ascii_case("include")) {
            for arg in w {
                let mut paths = expand(arg, list_dir);
                // the drop-in may not exist yet: put it where the glob would list it
                if names_dropin(arg) && !paths.contains(&dropin) {
                    paths.push(dropin.clone());
                    paths.sort();
                }
                for p in paths {
                    if p == dropin {
                        out.push_str(&format!("Include {candidate}\n"));
                        placed = true;
                    } else {
                        out.push_str(&format!("Include {p}\n"));
                    }
                }
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !placed {
        // not included: check it anyway (the caller reports `not_included`)
        out = format!("Include {candidate}\n{out}");
    }
    out
}

/// The effective configuration, from `sshd -T` (keywords in any case).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effective {
    pub ports: Vec<u16>,
    pub password_auth: Option<bool>,
    pub kbd_interactive: Option<bool>,
    pub root_login: Option<SshRootLogin>,
    pub allow_groups: Vec<String>,
    pub allow_users: Vec<String>,
    pub deny_users: Vec<String>,
    pub deny_groups: Vec<String>,
    pub tcp_forwarding: Option<bool>,
    pub agent_forwarding: Option<bool>,
    pub x11_forwarding: Option<bool>,
    pub authorized_keys_files: Vec<String>,
    pub pubkey_auth: Option<bool>,
    /// `AuthenticationMethods`: lists of methods that must all succeed (`any`: none).
    pub auth_methods: Vec<String>,
    /// `PermitRootLogin forced-commands-only`: keys with a forced command, never a shell.
    pub root_forced_only: bool,
}

pub fn effective(t: &str) -> Effective {
    let mut e = Effective::default();
    let yes = |v: &str| !matches!(v.to_ascii_lowercase().as_str(), "no");
    for l in t.lines() {
        let mut w = l.split_whitespace();
        let Some(k) = w.next() else { continue };
        let vals: Vec<&str> = w.collect();
        let v = vals.first().copied().unwrap_or("");
        match k.to_ascii_lowercase().as_str() {
            "port" => e.ports.extend(v.parse::<u16>().ok()),
            "passwordauthentication" => e.password_auth = Some(yes(v)),
            "kbdinteractiveauthentication" => e.kbd_interactive = Some(yes(v)),
            "pubkeyauthentication" => e.pubkey_auth = Some(yes(v)),
            "authenticationmethods" => e.auth_methods = vals.iter().map(|s| s.to_string()).collect(),
            "permitrootlogin" => {
                e.root_forced_only = v.eq_ignore_ascii_case("forced-commands-only");
                e.root_login = Some(match v.to_ascii_lowercase().as_str() {
                    "yes" => SshRootLogin::Yes,
                    "no" => SshRootLogin::No,
                    _ => SshRootLogin::Keys,
                })
            }
            "allowgroups" => e.allow_groups.extend(vals.iter().map(|s| s.to_string())),
            "allowusers" => e.allow_users.extend(vals.iter().map(|s| s.to_string())),
            "denyusers" => e.deny_users.extend(vals.iter().map(|s| s.to_string())),
            "denygroups" => e.deny_groups.extend(vals.iter().map(|s| s.to_string())),
            "allowtcpforwarding" => e.tcp_forwarding = Some(yes(v)),
            "allowagentforwarding" => e.agent_forwarding = Some(yes(v)),
            "x11forwarding" => e.x11_forwarding = Some(yes(v)),
            "authorizedkeysfile" => {
                e.authorized_keys_files.extend(vals.iter().map(|s| s.to_string()))
            }
            _ => {}
        }
    }
    e.ports.sort_unstable();
    e.ports.dedup();
    e
}

/// Settings as the running configuration has them (the first save adopts them).
pub fn adopt(e: &Effective, enabled: bool) -> SshSettings {
    let d = SshSettings::default();
    let listed = e.allow_groups.iter().any(|g| g == SSH_BASE_GROUPS[0]);
    SshSettings {
        enabled,
        port: e.ports.first().copied().unwrap_or(22),
        password_auth: e.password_auth.unwrap_or(d.password_auth),
        root_login: e.root_login.unwrap_or(SshRootLogin::Keys),
        allow: if listed { SshAllow::Listed } else { SshAllow::Everyone },
        allow_groups: if listed {
            e.allow_groups
                .iter()
                .filter(|g| !SSH_BASE_GROUPS.contains(&g.as_str()))
                .cloned()
                .collect()
        } else {
            Vec::new()
        },
        tcp_forwarding: e.tcp_forwarding.unwrap_or(true),
        agent_forwarding: e.agent_forwarding.unwrap_or(true),
        x11_forwarding: e.x11_forwarding.unwrap_or(false),
    }
}

/// Where the effective configuration differs from `s`: `(keyword, extra ports)`. The
/// keywords are the ones another file sets first (or adds to).
pub fn differences(s: &SshSettings, e: &Effective) -> (Vec<&'static str>, Vec<u16>) {
    let mut k = Vec::new();
    let mut ne = |key: &'static str, want: bool, got: Option<bool>| {
        if got.is_some_and(|g| g != want) {
            k.push(key);
        }
    };
    ne("PasswordAuthentication", s.password_auth, e.password_auth);
    ne("AllowTcpForwarding", s.tcp_forwarding, e.tcp_forwarding);
    ne("AllowAgentForwarding", s.agent_forwarding, e.agent_forwarding);
    ne("X11Forwarding", s.x11_forwarding, e.x11_forwarding);
    if e.root_login.is_some_and(|r| r != s.root_login) {
        k.push("PermitRootLogin");
    }
    let want: BTreeSet<String> = match s.allow {
        SshAllow::Listed => allowed_groups(s).into_iter().collect(),
        SshAllow::Everyone => BTreeSet::new(),
    };
    let got: BTreeSet<String> = e.allow_groups.iter().cloned().collect();
    if want != got {
        k.push("AllowGroups");
    }
    if !e.allow_users.is_empty() {
        k.push("AllowUsers");
    }
    if !e.deny_users.is_empty() {
        k.push("DenyUsers");
    }
    if !e.deny_groups.is_empty() {
        k.push("DenyGroups");
    }
    let extra = e.ports.iter().copied().filter(|p| *p != s.port).collect();
    (k, extra)
}

/// The first file other than the drop-in whose global part (before any `Match`) sets
/// `keyword`; the main file's parts are named by the main file.
pub fn who_sets<'a>(layout: &'a Layout, keyword: &str) -> Option<&'a str> {
    let dropin = format!("{DROPIN_DIR}/{DROPIN_NAME}");
    layout.files.iter().find_map(|(path, text)| {
        if *path == dropin {
            return None;
        }
        for l in text.lines() {
            let Some(k) = l.split_whitespace().next() else { continue };
            if k.eq_ignore_ascii_case("match") {
                break;
            }
            if k.eq_ignore_ascii_case(keyword) {
                return Some(path.split('#').next().unwrap_or(path));
            }
        }
        None
    })
}

/// Does the `AllowUsers`/`DenyUsers` pattern `p` (`user` or `user@host`) name `user`? The
/// host part is not known before a login: only the user part counts.
fn user_pattern(p: &str, user: &str) -> bool {
    let u = p.split_once('@').map_or(p, |(u, _)| u);
    glob(u, user)
}

pub fn admin_allowed(a: &SshAccount, e: &Effective) -> bool {
    if !a.admin || a.level != SshLevel::Shell {
        return false;
    }
    if e.deny_users.iter().any(|p| user_pattern(p, &a.name))
        || (!e.allow_users.is_empty() && !e.allow_users.iter().any(|p| user_pattern(p, &a.name)))
        || e.deny_groups.iter().any(|p| a.groups.iter().any(|g| glob(p, g)))
        || (!e.allow_groups.is_empty() && !e.allow_groups.iter().any(|p| a.groups.iter().any(|g| glob(p, g))))
    {
        return false;
    }
    // sshd's default reads .ssh/authorized_keys (and authorized_keys2); relative paths are
    // below the home, `%h` is the home
    let our_file = e.authorized_keys_files.is_empty()
        || e.authorized_keys_files.iter().any(|f| matches!(f.as_str(), ".ssh/authorized_keys" | "%h/.ssh/authorized_keys"));
    let keys = a.keys > 0 && e.pubkey_auth != Some(false) && our_file;
    let (mut password, mut kbd) = (e.password_auth.unwrap_or(true), e.kbd_interactive.unwrap_or(true));
    if a.uid == 0 {
        match e.root_login {
            _ if e.root_forced_only => return false,
            Some(SshRootLogin::No) => return false,
            Some(SshRootLogin::Keys) | None => (password, kbd) = (false, false),
            Some(SshRootLogin::Yes) => {}
        }
    }
    let lists: Vec<&str> = e.auth_methods.iter().map(String::as_str).filter(|m| !m.eq_ignore_ascii_case("any")).collect();
    if lists.is_empty() {
        return keys || password || kbd;
    }
    // one list whose every method can succeed
    lists.iter().any(|list| {
        list.split(',').all(|m| match m.split(':').next().unwrap_or("") {
            "publickey" => keys,
            "password" => password,
            "keyboard-interactive" => kbd,
            _ => false,
        })
    })
}

/// `ListenAddress` lines with a port of their own (`host:port`, `[v6]:port`) in the global
/// part of a file other than the drop-in: sshd listens there whatever `Port` says, so a port
/// change of ours would not take. `(file, value, port)`.
pub fn listen_ports(layout: &Layout) -> Vec<(String, String, u16)> {
    let dropin = format!("{DROPIN_DIR}/{DROPIN_NAME}");
    let mut out = Vec::new();
    for (path, text) in &layout.files {
        if *path == dropin {
            continue;
        }
        for l in text.lines() {
            let mut w = l.split_whitespace();
            let Some(k) = w.next() else { continue };
            if k.eq_ignore_ascii_case("match") {
                break;
            }
            if !k.eq_ignore_ascii_case("listenaddress") {
                continue;
            }
            let Some(v) = w.next() else { continue };
            let port = if let Some(rest) = v.strip_prefix('[') {
                rest.split_once("]:").and_then(|(_, p)| p.parse::<u16>().ok())
            } else if v.matches(':').count() == 1 {
                v.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok())
            } else {
                None
            };
            if let Some(p) = port {
                out.push((path.split('#').next().unwrap_or(path).to_string(), v.to_string(), p));
            }
        }
    }
    out
}
