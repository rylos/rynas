//! SSH engine tests: a machine below a temporary root and [`FakeRunner`] for sshd and
//! systemctl. Nothing of the host (sshd, /etc/ssh, units, accounts) is read or touched.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use super::fake::{DEBIAN_MAIN, FakeRunner};
use super::*;

const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICqfkRRvTxTaXrS5r1iAceZ/JZlbIfF/uX70TgYNhG4U";
const ED_FP: &str = "SHA256:WxplmhaiNVphHChN+O6tWxdsMMDw0DDKlTFU8oLiQaI";
const ED2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGjOhT75X7lrkc1BT7fnBEz3h/i9nOg7+RlBth5dZYCd laptop";
const ED2_FP: &str = "SHA256:LG0A27PQd1afWL/TsUPr4FyuUY7PrdZJtkydnN4gA+k";
const RSA1024: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDynSMwcljujOGz1eaAqR3y3lTwKI0I1L13UOI9uCfdEpwSvTWA9r2l2sNlpqRlnMYqVqs14cxl/iDbjtWhL2stdDmgV+O6kOr7oBn1u8bsxHG/7YjJnkeWJLIJhlX6t8Ht/y16e0jLXBmn/etn0coy+Ptn1qAEt/gcK40hgTYGTw== weak";
const RSA3072: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDDNIUqUpZHuNCdLNUqXuW22sq9KqoZzrVZn5gS03VkYNhpuWpVdOg8LvUn38Tg3QUvc5aGfaRHjA9YElFK5EvAqHQUABQYgocoQf4yM6TohhENzAtbfl53lrIxgxOsiCL2CeH24v+Djx/X8KYm5rBwvqf50rJ6GpWaebwS4x0lMe67BHaSQ1Cw6em8U15MCnnmew5UNDR7OWDVNA7b1nH2GVpO0UclqX0I0aT2n0t9+tDAwQ2k23pGivo7gbCDaBWVlLt9yZCUbTnUqdh7pebyPgrTBIxvmKxykINMHQu+u/isu1ZnYejfi3kNcan8IUwvJb6u0NlezJhS0Nknrs4SHhX6x58XOaEuVvgjKeCdYuqRXP63cXLwpL1njWbA93JPl93dAjAkEUmt7TXKpdwXN/LI8qS/i/1wkq0C9q0NWzeo3z1zyre9+dzdpT8sOMgIWcp9uH3j+0hXEyuEtmHJA82tFPt0Ulm9OlCJM1nc2eyiLzdjZ6knehvn8L9oM80= rsa";
const EC384: &str = "ecdsa-sha2-nistp384 AAAAE2VjZHNhLXNoYTItbmlzdHAzODQAAAAIbmlzdHAzODQAAABhBLsweVQ5kNTFN/AxLNUuAO8UZ7hOYikmDrz80TQqCzVI7NevEYnEgDNy6iPqFSo+54jiIcN3AlJ+jcpudlDk39MYA+TM2Hb2gs7LlXBwaUaTqi+Zrpi7n8RzhnZiHgOpBw== ec";

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    fake: Arc<FakeRunner>,
    ssh: Ssh,
}

/// The test user's ids (chown to them needs no privilege), inside the local range.
fn ids() -> (u32, u32) {
    // SAFETY: plain getters
    let (u, g) = unsafe { (libc::getuid(), libc::getgid()) };
    if u == 0 { (1000, 100) } else { (u, g) }
}

impl Fx {
    /// Debian's sshd_config, an ed25519 host key, `admin` (administrator, shell, no key).
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let (uid, gid) = ids();
        let put = |p: &str, t: &str| {
            let f = root.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        };
        put("etc/ssh/sshd_config", DEBIAN_MAIN);
        std::fs::create_dir_all(root.join("etc/ssh/sshd_config.d")).unwrap();
        put("etc/ssh/ssh_host_ed25519_key.pub", &format!("{ED} root@nas\n"));
        put("etc/ssh/ssh_host_rsa_key.pub", &format!("{RSA3072}\n"));
        put("usr/sbin/sshd", "");
        put(
            "etc/passwd",
            &format!(
                "root:x:0:0:root:/root:/bin/bash\nadmin:x:{uid}:{gid}:Admin:/home/admin:/bin/bash\nbob:x:{}:{gid}::/home/bob:/usr/sbin/nologin\nsfty:x:{}:{gid}::/home/sfty:/usr/sbin/nologin\n",
                uid + 1,
                uid + 2
            ),
        );
        put(
            "etc/group",
            &format!("root:x:0:\nusers:x:{gid}:\nsudo:x:27:admin\nrynas-sftp:x:991:sfty\n"),
        );
        for h in ["home/admin", "root"] {
            std::fs::create_dir_all(root.join(h)).unwrap();
            std::fs::set_permissions(root.join(h), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        put("proc/net/tcp", "  sl  local_address rem_address   st\n   0: 00000000:0016 00000000:0000 0A 0\n   1: 00000000:1F90 00000000:0000 0A 0\n");
        let fake = Arc::new(FakeRunner::new(dir.path(), &root));
        let ssh = Ssh::new(&root, dir.path().join("state"), fake.clone()).with_rynasctl("/x/rynasctl");
        Fx { _dir: dir, root, fake, ssh }
    }

    fn dropin(&self) -> Option<String> {
        std::fs::read_to_string(self.root.join("etc/ssh/sshd_config.d/00-rynas.conf")).ok()
    }

    fn settings(&self) -> SshSettings {
        self.ssh.state().settings
    }

    fn problem(&self, code: SshProblemCode) -> Option<SshProblem> {
        self.ssh.state().problems.into_iter().find(|p| p.code == code)
    }
}

#[test]
fn a_fresh_machine_is_adopted_as_it_runs() {
    let f = Fx::new();
    let s = f.ssh.state();
    assert!(s.installed && !s.managed && s.running && s.trial.is_none());
    assert_eq!(s.ports, vec![22]);
    let st = s.settings;
    assert!(st.enabled && st.password_auth && st.tcp_forwarding);
    assert_eq!((st.port, st.root_login, st.allow), (22, SshRootLogin::Keys, SshAllow::Everyone));
    // Debian's main file says X11Forwarding yes
    assert!(st.x11_forwarding);
    assert!(s.problems.is_empty(), "{:?}", s.problems);
    assert_eq!(s.host_keys.iter().map(|k| (k.kind.as_str(), k.bits)).collect::<Vec<_>>(), vec![("ED25519", 256), ("RSA", 3072)]);
    assert_eq!(s.host_keys[0].fingerprint, ED_FP);
    let admin = s.accounts.iter().find(|a| a.name == "admin").unwrap();
    assert!(admin.admin && admin.level == SshLevel::Shell && admin.keys == 0);
    assert_eq!(s.accounts.iter().find(|a| a.name == "bob").unwrap().level, SshLevel::None);
    assert_eq!(s.accounts.iter().find(|a| a.name == "sfty").unwrap().level, SshLevel::Sftp);
}

#[test]
fn the_dropin_is_read_first_and_wins() {
    let f = Fx::new();
    // another file says the opposite, later in the order
    std::fs::write(f.root.join("etc/ssh/sshd_config.d/50-cloud-init.conf"), "PasswordAuthentication yes\nPort 22\n").unwrap();
    let mut s = f.settings();
    s.x11_forwarding = false;
    s.root_login = SshRootLogin::No;
    f.ssh.apply(s.clone(), None, "admin").unwrap();
    assert_eq!(f.settings(), s);
    let text = f.dropin().unwrap();
    assert!(text.starts_with(config::HEADER));
    assert!(text.contains("X11Forwarding no\n") && text.contains("PermitRootLogin no\n"));
    assert!(text.contains("Match Group rynas-sftp\n    ForceCommand internal-sftp\n"));
    assert!(f.ssh.state().problems.is_empty(), "{:?}", f.ssh.state().problems);
    // the candidate was checked in place before anything was written
    let calls = f.fake.calls();
    let t = calls.iter().position(|c| c.starts_with("sshd -t -f")).unwrap();
    let r = calls.iter().position(|c| c == "systemctl reload ssh.service").unwrap();
    assert!(t < r);
}

#[test]
fn candidate_main_puts_the_candidate_where_the_dropin_goes() {
    let list = |_: &str| vec!["50-cloud-init.conf".to_string(), "10-admin.conf".to_string()];
    let m = config::candidate_main(DEBIAN_MAIN, "/tmp/c/00-rynas.conf", &list);
    let inc: Vec<&str> = m.lines().filter(|l| l.starts_with("Include")).collect();
    assert_eq!(inc, vec![
        "Include /tmp/c/00-rynas.conf",
        "Include /etc/ssh/sshd_config.d/10-admin.conf",
        "Include /etc/ssh/sshd_config.d/50-cloud-init.conf",
    ]);
    assert!(m.contains("UsePAM yes"));
    let l = config::layout(DEBIAN_MAIN, &|_| vec![], &|_| None);
    assert!(l.includes_dropin);
    let l = config::layout("UsePAM yes\n", &|_| vec![], &|_| None);
    assert!(!l.includes_dropin);
}

#[test]
fn a_risky_change_waits_for_its_confirmation() {
    let f = Fx::new();
    let mut s = f.settings();
    s.port = 2222;
    let t = f.ssh.apply(s, None, "admin").unwrap().expect("a trial");
    assert_eq!((t.port, t.timeout_s, t.owner.as_str()), (2222, 120, "admin"));
    assert!(f.dropin().unwrap().contains("Port 2222\n"));
    let calls = f.fake.calls();
    assert!(calls.iter().any(|c| c.starts_with("systemd-run --unit=rynas-ssh-rollback") && c.contains("--on-active=120s") && c.ends_with("/x/rynasctl ssh rollback")), "{calls:?}");
    assert_eq!(f.ssh.state().trial.unwrap().token, t.token);
    // a second change waits
    assert!(matches!(f.ssh.apply(f.settings(), None, "admin"), Err(SshError::Conflict(_))));
    assert!(matches!(f.ssh.confirm("nope"), Err(SshError::Invalid("token", _))));
    f.ssh.confirm(&t.token).unwrap();
    assert!(f.ssh.state().trial.is_none());
    assert!(f.fake.calls().iter().any(|c| c == "systemctl stop rynas-ssh-rollback.timer"));
    assert_eq!(f.settings().port, 2222);
    // nothing left to undo
    assert!(!f.ssh.rollback("manual").unwrap());
}

#[test]
fn an_unconfirmed_change_goes_back_by_itself() {
    let f = Fx::new();
    let mut s = f.settings();
    s.password_auth = false;
    s.port = 2200;
    // admin needs a key for password login off
    f.ssh.add_key("admin", ED2).unwrap();
    let t = f.ssh.apply(s, Some(60), "admin").unwrap().unwrap();
    assert!(f.dropin().is_some());
    assert!(!f.ssh.expire(t.deadline_ms - 1).unwrap());
    assert!(f.ssh.expire(t.deadline_ms).unwrap());
    // there was no drop-in before: none again, the machine as it was
    assert!(f.dropin().is_none());
    let st = f.ssh.state();
    assert!(!st.managed && st.trial.is_none());
    assert_eq!((st.settings.port, st.settings.password_auth), (22, true));
    assert!(f.ssh.confirm(&t.token).is_err());
}

#[test]
fn harmless_changes_apply_at_once() {
    let f = Fx::new();
    let s = f.settings();
    // adopting the running settings unchanged shuts nobody out
    assert!(f.ssh.apply(s.clone(), None, "admin").unwrap().is_none());
    assert!(f.ssh.state().managed);
    let mut s2 = s;
    s2.x11_forwarding = !s2.x11_forwarding;
    s2.agent_forwarding = !s2.agent_forwarding;
    assert!(f.ssh.apply(s2.clone(), None, "admin").unwrap().is_none());
    assert_eq!(f.settings(), s2);
}

#[test]
fn password_login_off_needs_an_administrator_key() {
    let f = Fx::new();
    let mut s = f.settings();
    s.password_auth = false;
    let e = f.ssh.apply(s.clone(), None, "admin").unwrap_err();
    assert!(matches!(&e, SshError::Lockout(m) if m.contains("key")), "{e}");
    assert!(f.dropin().is_none());
    // a weak key does not count
    std::fs::create_dir_all(f.root.join("home/admin/.ssh")).unwrap();
    std::fs::set_permissions(f.root.join("home/admin/.ssh"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(f.root.join("home/admin/.ssh/authorized_keys"), format!("{RSA1024}\n")).unwrap();
    assert!(matches!(f.ssh.apply(s.clone(), None, "admin"), Err(SshError::Lockout(_))));
    f.ssh.add_key("admin", ED2).unwrap();
    assert!(f.ssh.apply(s, None, "admin").unwrap().is_some());
}

#[test]
fn only_listed_accounts_keep_an_administrator_in() {
    let f = Fx::new();
    let mut s = f.settings();
    s.allow = SshAllow::Listed;
    // admin is in sudo, not in rynas-ssh
    assert!(matches!(f.ssh.apply(s.clone(), None, "admin"), Err(SshError::Lockout(m)) if m.contains("allowed")));
    s.allow_groups = vec!["sudo".into(), "rynas-ssh".into(), "sudo".into()];
    f.ssh.apply(s, None, "admin").unwrap();
    let text = f.dropin().unwrap();
    assert!(text.contains("AllowGroups rynas-ssh rynas-sftp rynas-pcbackup rynas-cert-drop sudo\n"), "{text}");
    assert_eq!(f.settings().allow_groups, vec!["sudo".to_string()]);
    assert!(f.ssh.state().problems.is_empty());
    // the PC backup account's check sees it allowed
    let eff = f.fake.run(&["sshd", "-T"]).unwrap().stdout;
    assert!(eff.contains("allowgroups rynas-pcbackup"));
}

#[test]
fn settings_are_checked() {
    let f = Fx::new();
    let mut s = f.settings();
    s.port = 0;
    assert!(matches!(f.ssh.apply(s.clone(), None, "admin"), Err(SshError::Invalid("port", _))));
    // 8080 is taken by something else; 22 is sshd's own
    s.port = 8080;
    assert!(matches!(f.ssh.apply(s.clone(), None, "admin"), Err(SshError::Invalid("port", m)) if m.contains("in use")));
    s.port = 22;
    s.allow_groups = vec!["bad group".into()];
    assert!(matches!(f.ssh.apply(s.clone(), None, "admin"), Err(SshError::Invalid("allow_groups", _))));
    s.allow_groups = vec![];
    assert!(matches!(f.ssh.apply(s.clone(), Some(5), "admin"), Err(SshError::Invalid("timeout_s", _))));
    f.fake.set_sshd_error(Some("line 3: Bad configuration option"));
    assert!(matches!(f.ssh.apply(s.clone(), None, "admin"), Err(SshError::Invalid("settings", m)) if m.contains("Bad configuration")));
    assert!(f.dropin().is_none() && f.ssh.state().trial.is_none());
    f.fake.set_sshd_error(None);
    std::fs::write(f.root.join("etc/ssh/sshd_config"), "UsePAM yes\n").unwrap();
    assert!(matches!(f.ssh.apply(s, None, "admin"), Err(SshError::Conflict(m)) if m.contains("does not include")));
    assert!(f.problem(SshProblemCode::NotIncluded).is_some());
}

#[test]
fn sshd_that_does_not_start_is_undone_at_once() {
    let f = Fx::new();
    let before = f.settings();
    f.fake.set_fail_start(true);
    let mut s = before.clone();
    s.port = 2022;
    let e = f.ssh.apply(s, None, "admin").unwrap_err();
    assert!(matches!(&e, SshError::Failed(m) if m.contains("did not start")), "{e}");
    assert!(f.dropin().is_none() && f.ssh.state().trial.is_none());
    f.fake.set_fail_start(false);
    assert_eq!(f.settings().port, 22);
}

#[test]
fn socket_activation_is_switched_off_and_comes_back_on_rollback() {
    let f = Fx::new();
    f.fake.set_unit("ssh.service", false, false);
    f.fake.set_unit("ssh.socket", true, true);
    assert!(f.problem(SshProblemCode::SocketActivation).is_some());
    let s = f.settings();
    assert!(s.enabled);
    f.ssh.apply(s, None, "admin").unwrap().unwrap();
    assert_eq!((f.fake.unit("ssh.socket"), f.fake.unit("ssh.service")), ((false, false), (true, true)));
    f.ssh.rollback("manual").unwrap();
    assert_eq!((f.fake.unit("ssh.socket"), f.fake.unit("ssh.service")), ((true, true), (false, false)));
}

#[test]
fn turning_ssh_off_and_on() {
    let f = Fx::new();
    let mut s = f.settings();
    s.enabled = false;
    let t = f.ssh.apply(s.clone(), None, "admin").unwrap().unwrap();
    assert_eq!(f.fake.unit("ssh.service"), (false, false));
    f.ssh.confirm(&t.token).unwrap();
    let st = f.ssh.state();
    assert!(!st.running && !st.settings.enabled && st.problems.is_empty());
    s.enabled = true;
    f.ssh.apply(s, None, "admin").unwrap().unwrap();
    assert_eq!(f.fake.unit("ssh.service"), (true, true));
}

#[test]
fn other_files_that_win_are_named() {
    let f = Fx::new();
    let s = f.settings();
    f.ssh.apply(s, None, "admin").unwrap();
    std::fs::write(f.root.join("etc/ssh/sshd_config.d/00-aaa.conf"), "PasswordAuthentication no\n").unwrap();
    std::fs::write(f.root.join("etc/ssh/sshd_config"), format!("{DEBIAN_MAIN}Port 2200\n")).unwrap();
    let probs = f.ssh.state().problems;
    assert!(probs.iter().any(|p| p.code == SshProblemCode::Overridden
        && p.subject.as_deref() == Some("PasswordAuthentication /etc/ssh/sshd_config.d/00-aaa.conf")), "{probs:?}");
    assert!(probs.iter().any(|p| p.code == SshProblemCode::ExtraPorts && p.subject.as_deref() == Some("2200")));
}

#[test]
fn a_hand_edit_is_drift() {
    let f = Fx::new();
    f.ssh.apply(f.settings(), None, "admin").unwrap();
    let p = f.root.join("etc/ssh/sshd_config.d/00-rynas.conf");
    let t = std::fs::read_to_string(&p).unwrap();
    std::fs::write(&p, format!("{t}MaxAuthTries 2\n")).unwrap();
    assert!(f.problem(SshProblemCode::Drift).is_some());
}

#[test]
fn keys_are_added_listed_and_removed() {
    let f = Fx::new();
    assert!(f.ssh.keys("admin").unwrap().keys.is_empty());
    let k = f.ssh.add_key("admin", &format!("  {ED2}  ")).unwrap();
    assert_eq!((k.fingerprint.as_str(), k.bits, k.comment.as_str()), (ED2_FP, 256, "laptop"));
    assert!(matches!(f.ssh.add_key("admin", ED2), Err(SshError::Conflict(_))));
    f.ssh.add_key("admin", EC384).unwrap();
    let ssh = f.root.join("home/admin/.ssh");
    assert_eq!(std::fs::metadata(&ssh).unwrap().permissions().mode() & 0o777, 0o700);
    let file = ssh.join("authorized_keys");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    // lines that are not ours stay
    let t = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, format!("# mine\nrestrict,command=\"echo hi there\" {ED} cmd\n{t}")).unwrap();
    let l = f.ssh.keys("admin").unwrap();
    assert_eq!(l.keys.len(), 3);
    assert!(l.keys[0].options && l.keys[0].fingerprint == ED_FP);
    assert_eq!(l.keys[2].bits, 384);
    f.ssh.remove_key("admin", ED2_FP).unwrap();
    let t = std::fs::read_to_string(&file).unwrap();
    assert!(t.starts_with("# mine\nrestrict,command=") && !t.contains("laptop"));
    assert!(matches!(f.ssh.remove_key("admin", ED2_FP), Err(SshError::NotFound(_))));
    assert!(matches!(f.ssh.keys("nobody"), Err(SshError::NotFound(_))));
    assert_eq!(f.ssh.state().accounts.iter().find(|a| a.name == "admin").unwrap().keys, 2);
}

#[test]
fn bad_keys_are_refused() {
    let f = Fx::new();
    for bad in [
        RSA1024,
        "ssh-ed25519 notbase64!!",
        "hello world",
        &format!("restrict {ED}"),
        &format!("{ED}\n{ED2}"),
        // the blob says rsa, the line says ed25519
        &RSA3072.replacen("ssh-rsa", "ssh-ed25519", 1),
    ] {
        assert!(matches!(f.ssh.add_key("admin", bad), Err(SshError::Invalid("key", _))), "{bad}");
    }
    assert_eq!(keys::check_new(RSA3072).unwrap().1.bits, 3072);
}

#[test]
fn links_in_the_home_are_never_followed() {
    let f = Fx::new();
    let ssh = f.root.join("home/admin/.ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = f.root.join("etc/shadow");
    std::fs::write(&target, "secret\n").unwrap();
    std::os::unix::fs::symlink(&target, ssh.join("authorized_keys")).unwrap();
    assert!(f.ssh.keys("admin").is_err());
    assert!(f.ssh.add_key("admin", ED2).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "secret\n");
    // .ssh itself a link
    std::fs::remove_dir_all(&ssh).unwrap();
    std::os::unix::fs::symlink(f.root.join("etc"), &ssh).unwrap();
    assert!(f.ssh.add_key("admin", ED2).is_err());
    assert!(!f.root.join("etc/authorized_keys").exists());
    // a home others can write to
    std::fs::remove_file(&ssh).unwrap();
    std::fs::set_permissions(f.root.join("home/admin"), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(f.ssh.add_key("admin", ED2), Err(SshError::Failed(m)) if m.contains("writable")));
}

#[test]
fn the_last_key_of_a_keys_only_administrator_stays() {
    let f = Fx::new();
    f.ssh.add_key("admin", ED2).unwrap();
    let mut s = f.settings();
    s.password_auth = false;
    let t = f.ssh.apply(s, None, "admin").unwrap().unwrap();
    f.ssh.confirm(&t.token).unwrap();
    assert!(matches!(f.ssh.remove_key("admin", ED2_FP), Err(SshError::Lockout(_))));
    f.ssh.add_key("admin", EC384).unwrap();
    f.ssh.remove_key("admin", ED2_FP).unwrap();
}

#[test]
fn logins_come_from_the_journal() {
    let f = Fx::new();
    f.fake.set_journal(concat!(
        "1700000000.100000 nas sshd-session[10]: Accepted publickey for admin from 192.0.2.5 port 50000 ssh2: ED25519 SHA256:x\n",
        "1700000100.000000 nas sshd-session[11]: Invalid user oracle from 198.51.100.7 port 4000\n",
        "1700000100.500000 nas sshd-session[11]: Failed password for invalid user oracle from 198.51.100.7 port 4000 ssh2\n",
        "1700000200.000000 nas sshd-session[12]: Failed password for admin from 198.51.100.7 port 4001 ssh2\n",
        "1700000300.000000 nas sshd-session[13]: Failed publickey for bob from 203.0.113.9 port 1 ssh2: RSA SHA256:y\n",
        "1700000400.000000 nas sshd[1]: Server listening on 0.0.0.0 port 22.\n",
        "garbage\n",
    ));
    let l = f.ssh.logins();
    assert_eq!((l.accepted, l.failed), (1, 3));
    assert_eq!(l.top_failed, vec![("198.51.100.7".to_string(), 2), ("203.0.113.9".to_string(), 1)]);
    assert_eq!(l.events[0].user, "bob");
    assert_eq!(l.events.last().unwrap().method, "publickey");
    assert_eq!(l.events.last().unwrap().at_ms, 1_700_000_000_100);
    assert_eq!(l.events[2].kind, SshLoginKind::InvalidUser);
}

#[test]
fn listening_ports_from_proc() {
    let t = "  sl  local_address rem_address   st\n   0: 0100007F:0CEA 00000000:0000 0A 0\n   1: 00000000:0016 0100007F:9C40 01 0\n";
    assert_eq!(listening(t).into_iter().collect::<Vec<_>>(), vec![3306]);
}

#[test]
fn root_login_follows_its_setting() {
    let root = SshAccount { name: "root".into(), uid: 0, admin: true, level: SshLevel::Shell, groups: vec!["root".into()], keys: 0 };
    let mut s = SshSettings { root_login: SshRootLogin::Yes, ..SshSettings::default() };
    assert!(admin_can_log_in(&root, &s));
    s.root_login = SshRootLogin::Keys;
    assert!(!admin_can_log_in(&root, &s));
    s.allow = SshAllow::Listed;
    let with_key = SshAccount { keys: 1, ..root.clone() };
    assert!(!admin_can_log_in(&with_key, &s));
    s.allow_groups = vec!["root".into()];
    assert!(admin_can_log_in(&with_key, &s));
}
