//! Wake-on-LAN and power on the PC (docs/research/wake-on-lan.md): the NAS' `Sleep` and
//! `EnableWol` commands, magic-packet wake kept on across boots, and the "after a wake" action
//! once the backup the NAS woke the PC for has ended, only when nobody used the PC meanwhile.
//!
//! Every outside program goes through [`Exec`], so the tests never touch this PC's network or
//! power, and the hidden schedule test only logs what it would change.

use std::path::Path;
use std::process::{Command, Stdio};

use rynas_api_types::pcagent::{AfterWake, AgentPlan, SleepMode};

/// How long a wake by the NAS counts: a due backup starts without the start delay only
/// within this time of [`AgentPlan::woken_ms`].
pub const WOKEN_FOR_MS: u64 = 30 * 60_000;

/// The pause before the PC sleeps or shuts down: the poll answer and the log line get out.
pub const SLEEP_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);

/// Outside programs: reading ones (`ethtool <if>`, `busctl`, `loginctl`) and ones that change
/// the PC (`ethtool -s`, `systemctl suspend`).
pub trait Exec {
    /// `prog args`: `None` when it cannot start; else whether it succeeded and its output.
    fn read(&self, prog: &str, args: &[&str]) -> Option<(bool, String)>;
    /// `prog args`, a change of the PC.
    fn act(&self, prog: &str, args: &[&str]) -> Result<(), String>;
}

/// The real programs (the first found in the usual folders, else the service's `PATH`).
pub struct System;

/// Reads for real, logs the changes instead of making them (`schedule-test`).
pub struct DryRun;

fn program(prog: &str) -> String {
    ["/usr/sbin", "/usr/bin", "/sbin", "/bin", "/usr/local/sbin", "/usr/local/bin"]
        .iter()
        .map(|d| format!("{d}/{prog}"))
        .find(|p| Path::new(p).exists())
        .unwrap_or_else(|| prog.to_string())
}

fn read_real(prog: &str, args: &[&str]) -> Option<(bool, String)> {
    let out = Command::new(program(prog)).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    Some((out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned()))
}

impl Exec for System {
    fn read(&self, prog: &str, args: &[&str]) -> Option<(bool, String)> {
        read_real(prog, args)
    }

    fn act(&self, prog: &str, args: &[&str]) -> Result<(), String> {
        let out = Command::new(program(prog))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{prog}: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(format!("{prog} {}: {}", args.join(" "), if err.is_empty() { out.status.to_string() } else { err }))
    }
}

impl Exec for DryRun {
    fn read(&self, prog: &str, args: &[&str]) -> Option<(bool, String)> {
        read_real(prog, args)
    }

    fn act(&self, prog: &str, args: &[&str]) -> Result<(), String> {
        eprintln!("(test) would run: {prog} {}", args.join(" "));
        Ok(())
    }
}

pub fn verb(mode: SleepMode) -> &'static str {
    match mode {
        SleepMode::Suspend => "suspend",
        SleepMode::Hibernate => "hibernate",
        SleepMode::Poweroff => "poweroff",
        SleepMode::Unknown => "unknown",
    }
}

/// What the service does with a `Sleep` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepDecision {
    /// A backup runs: refused (logged; the command counts as handled, so it never fires
    /// later when the backup ends).
    RefuseBackup,
    /// A mode of a newer NAS this agent does not know: refused (logged), never carried out.
    RefuseUnknown,
    Go(SleepMode),
}

pub fn sleep_decision(mode: SleepMode, backup_running: bool) -> SleepDecision {
    if mode == SleepMode::Unknown {
        SleepDecision::RefuseUnknown
    } else if backup_running {
        SleepDecision::RefuseBackup
    } else {
        SleepDecision::Go(mode)
    }
}

/// The first `block` inhibitor lock against this mode in logind's `ListInhibitors` answer
/// (`busctl --json=short`, entries `[what, who, why, mode, uid, pid]`): `who (why)`. `what`
/// is a `:` list; suspend and hibernate are stopped by `sleep`, power off by `shutdown`.
/// `delay` locks only delay it. Not `systemctl --check-inhibitors=yes`: that also refuses
/// whenever any user is logged in, so the NAS's command would never work on a desktop.
pub fn blocking_lock(json: &str, mode: SleepMode) -> Option<String> {
    let want = if mode == SleepMode::Poweroff { "shutdown" } else { "sleep" };
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let list = v.get("data")?.get(0)?.as_array()?;
    list.iter().find_map(|e| {
        let what = e.get(0)?.as_str()?;
        let blocks = e.get(3)?.as_str()? == "block" && what.split(':').any(|w| w == want);
        blocks.then(|| format!("{} ({})", e.get(1).and_then(|x| x.as_str()).unwrap_or("?"), e.get(2).and_then(|x| x.as_str()).unwrap_or("")))
    })
}

/// Suspends, hibernates or shuts the PC down after [`SLEEP_PAUSE`], unless an application
/// holds a `block` inhibitor lock against it ([`blocking_lock`]): then nothing happens and the
/// error names the application.
pub fn sleep_now(exec: &dyn Exec, mode: SleepMode) -> Result<(), String> {
    if mode == SleepMode::Unknown {
        return Err("a sleep mode this agent does not know".into());
    }
    let locks = exec
        .read("busctl", &["--json=short", "call", "org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager", "ListInhibitors"])
        .map(|(_, o)| o)
        .unwrap_or_default();
    if let Some(who) = blocking_lock(&locks, mode) {
        return Err(format!("blocked by {who}"));
    }
    std::thread::sleep(SLEEP_PAUSE);
    exec.act("systemctl", &[verb(mode)]).map_err(|e| format!("refused by systemd: {e}"))
}

/// A card name safe to hand to `ethtool`: 1–15 characters of `[A-Za-z0-9_.:@-]`, not
/// starting with `-` (never an option), not `.` or `..`.
pub fn valid_nic_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 15
        && !n.starts_with('-')
        && n != "."
        && n != ".."
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '@' | '-'))
}

/// The cards of an `EnableWol` command that this PC has (names checked against
/// `<sys>/class/net`, never a path).
pub fn existing_nics(sys: &Path, nics: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in nics {
        if valid_nic_name(n) && sys.join("class/net").join(n).exists() && !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// `ethtool -s <nic> wol g` for each card (the kernel directly when the `ethtool` program
/// fails or is missing): the ones that took it.
pub fn enable_wol(exec: &dyn Exec, nics: &[String]) -> Vec<String> {
    let mut ok = Vec::new();
    for n in nics.iter().filter(|n| valid_nic_name(n)) {
        match exec.act("ethtool", &["-s", n, "wol", "g"]) {
            Ok(()) => ok.push(n.clone()),
            Err(e) => match crate::wolioctl::enable_magic(n) {
                Ok(()) => ok.push(n.clone()),
                Err(e2) => eprintln!("Wake-on-LAN on {n}: {e}; {e2}"),
            },
        }
    }
    ok
}

/// The kept cards with an `EnableWol`'s new ones (no duplicates, the old order first).
pub fn merge_kept(kept: &[String], new: &[String]) -> Vec<String> {
    let mut out = kept.to_vec();
    for n in new {
        if !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

/// The NAS woke this PC recently ([`WOKEN_FOR_MS`]): `Some(woken_ms)`.
pub fn recent_wake(plan: &AgentPlan, now_ms: u64) -> Option<u64> {
    plan.woken_ms.filter(|w| now_ms.abs_diff(*w) <= WOKEN_FOR_MS)
}

/// One logind session, from `loginctl show-session`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    pub class: String,
    pub kind: String,
    pub idle: Option<bool>,
    /// `IdleSinceHint`: since when it is idle (µs, wall clock; 0 or absent: unknown).
    pub idle_since_us: Option<u64>,
    /// `IdleSinceHintMonotonic` (µs, monotonic clock; 0 or absent: unknown).
    pub idle_since_mono_us: Option<u64>,
}

/// The properties asked of every session.
pub const SESSION_PROPS: &[&str] = &["-p", "Class", "-p", "Type", "-p", "IdleHint", "-p", "IdleSinceHint", "-p", "IdleSinceHintMonotonic"];

/// `loginctl show-session <id> -p …` output ([`SESSION_PROPS`]).
pub fn parse_session(text: &str) -> Session {
    let yes_no = |v: &str| match v {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    };
    let micros = |v: &str| v.parse::<u64>().ok().filter(|n| *n > 0);
    let mut s = Session::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "Class" => s.class = v.to_string(),
            "Type" => s.kind = v.to_string(),
            "IdleHint" => s.idle = yes_no(v),
            "IdleSinceHint" => s.idle_since_us = micros(v),
            "IdleSinceHintMonotonic" => s.idle_since_mono_us = micros(v),
            _ => {}
        }
    }
    s
}

/// The session ids of `loginctl list-sessions --no-legend` (first column).
pub fn parse_session_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|id| id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .map(str::to_string)
        .collect()
}

/// A session of no person: login screens, the per-user service manager, background jobs.
/// Every other class (`user`, `user-early`, `user-incomplete`, `lock-screen`, an unknown
/// one) is a person's, whatever its type (tty, x11, wayland, remote).
fn not_a_person(class: &str) -> bool {
    matches!(class, "greeter" | "manager" | "manager-early" | "background" | "background-light")
}

/// The time of the wake on both clocks: wall clock and monotonic (µs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeTime {
    pub wall_us: u64,
    pub mono_us: Option<u64>,
}

/// Idle since before the wake (no activity since the NAS woke the PC); unknown = no.
fn idle_since_before(s: &Session, wake: WakeTime) -> bool {
    if s.idle != Some(true) {
        return false;
    }
    match (s.idle_since_us, s.idle_since_mono_us, wake.mono_us) {
        (Some(t), _, _) => t < wake.wall_us,
        (None, Some(m), Some(w)) => m < w,
        _ => false,
    }
}

/// Whether the "after a wake" action may go ahead: shutting down only with no person's session
/// at all; suspending when every person's session has been idle since before the wake (a
/// locked screen alone is no proof). Unparsable sessions (no class) count as in use.
pub fn may_sleep_after_wake(mode: SleepMode, sessions: &[Session], wake: WakeTime) -> bool {
    if sessions.iter().any(|s| s.class.is_empty()) {
        return false;
    }
    let mut people = sessions.iter().filter(|s| !not_a_person(&s.class));
    match mode {
        SleepMode::Poweroff => people.next().is_none(),
        SleepMode::Suspend => people.all(|s| idle_since_before(s, wake)),
        SleepMode::Hibernate | SleepMode::Unknown => false,
    }
}

/// The sessions from logind; `None` whenever they cannot be read (no loginctl, a failed call).
pub fn sessions_now(exec: &dyn Exec) -> Option<Vec<Session>> {
    let Some((true, list)) = exec.read("loginctl", &["list-sessions", "--no-legend", "--no-pager"]) else {
        return None;
    };
    let mut sessions = Vec::new();
    for id in parse_session_ids(&list) {
        let mut args = vec!["show-session", id.as_str()];
        args.extend_from_slice(SESSION_PROPS);
        match exec.read("loginctl", &args) {
            Some((true, out)) => sessions.push(parse_session(&out)),
            // unsure (even a session that ended meanwhile): no sleep this time
            _ => return None,
        }
    }
    Some(sessions)
}

fn clock_us(clock: libc::clockid_t) -> Option<u64> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime fills the timespec it is given
    if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
        return None;
    }
    Some(ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000)
}

/// The wake at `woken_ms` (wall clock) on both clocks, the monotonic one counted back from now.
pub fn wake_time(woken_ms: u64) -> WakeTime {
    let wall_us = woken_ms.saturating_mul(1000);
    let mono_us = match (clock_us(libc::CLOCK_REALTIME), clock_us(libc::CLOCK_MONOTONIC)) {
        (Some(now), Some(mono)) if now >= wall_us => mono.checked_sub(now - wall_us),
        _ => None,
    };
    WakeTime { wall_us, mono_us }
}

/// The "after a wake" action once the woken run ended: the mode to go to, if any.
pub fn after_wake_mode(after: AfterWake, woken: Option<u64>, done: Option<u64>) -> Option<SleepMode> {
    let w = woken?;
    if done == Some(w) {
        return None;
    }
    match after {
        AfterWake::Stay | AfterWake::Unknown => None,
        AfterWake::Suspend => Some(SleepMode::Suspend),
        AfterWake::Poweroff => Some(SleepMode::Poweroff),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Canned outputs; the changes asked for are recorded, never made.
    #[derive(Default)]
    pub struct Fake {
        pub outputs: HashMap<String, (bool, String)>,
        pub acts: RefCell<Vec<String>>,
        pub fail_acts: bool,
    }

    impl Exec for Fake {
        fn read(&self, prog: &str, args: &[&str]) -> Option<(bool, String)> {
            self.outputs.get(&format!("{prog} {}", args.join(" "))).cloned()
        }
        fn act(&self, prog: &str, args: &[&str]) -> Result<(), String> {
            self.acts.borrow_mut().push(format!("{prog} {}", args.join(" ")));
            if self.fail_acts { Err("no".into()) } else { Ok(()) }
        }
    }

    #[test]
    fn sleep_is_refused_during_a_backup() {
        assert_eq!(sleep_decision(SleepMode::Suspend, true), SleepDecision::RefuseBackup);
        assert_eq!(sleep_decision(SleepMode::Hibernate, false), SleepDecision::Go(SleepMode::Hibernate));
        assert_eq!(sleep_decision(SleepMode::Unknown, false), SleepDecision::RefuseUnknown);
        // no locks known: the plain command
        let f = Fake::default();
        sleep_now(&f, SleepMode::Poweroff).unwrap();
        assert_eq!(f.acts.borrow().as_slice(), ["systemctl poweroff"]);
        // delay locks only delay: still suspended; a block lock on sleep stops it
        let busctl = "busctl --json=short call org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager ListInhibitors";
        let delay = r#"{"type":"a(ssssuu)","data":[[["sleep","NetworkManager","turn off networks","delay",0,694],["handle-power-key:handle-lid-switch","Desktop","keys","block",1000,40]]]}"#;
        let mut f = Fake::default();
        f.outputs.insert(busctl.into(), (true, delay.into()));
        sleep_now(&f, SleepMode::Suspend).unwrap();
        assert_eq!(f.acts.borrow().as_slice(), ["systemctl suspend"]);
        let block = r#"{"type":"a(ssssuu)","data":[[["shutdown:sleep","Updater","installing updates","block",0,99]]]}"#;
        let mut f = Fake::default();
        f.outputs.insert(busctl.into(), (true, block.into()));
        assert_eq!(sleep_now(&f, SleepMode::Poweroff).unwrap_err(), "blocked by Updater (installing updates)");
        assert!(f.acts.borrow().is_empty());
        assert_eq!(blocking_lock(block, SleepMode::Hibernate).as_deref(), Some("Updater (installing updates)"));
        assert_eq!(blocking_lock("not json", SleepMode::Suspend), None);
        // systemd refuses: the error says so
        let blocked = Fake { fail_acts: true, ..Default::default() };
        assert!(sleep_now(&blocked, SleepMode::Suspend).unwrap_err().contains("refused"));
        // never an unknown mode
        let f = Fake::default();
        assert!(sleep_now(&f, SleepMode::Unknown).is_err());
        assert!(f.acts.borrow().is_empty());
        // card names: never an option or odd characters
        assert!(valid_nic_name("enp3s0") && valid_nic_name("eth0.10") && valid_nic_name("wlan0@x"));
        assert!(!valid_nic_name("-s") && !valid_nic_name("a b") && !valid_nic_name("..") && !valid_nic_name("a\u{7}"));
    }

    #[test]
    fn enable_wol_on_existing_cards() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("class/net/enp3s0")).unwrap();
        let asked = ["enp3s0".to_string(), "eth9".into(), "../x".into(), "enp3s0".into()];
        let nics = existing_nics(d.path(), &asked);
        assert_eq!(nics, ["enp3s0"]);
        let f = Fake::default();
        assert_eq!(enable_wol(&f, &nics), ["enp3s0"]);
        assert_eq!(f.acts.borrow().as_slice(), ["ethtool -s enp3s0 wol g"]);
        let failing = Fake { fail_acts: true, ..Default::default() };
        assert!(enable_wol(&failing, &nics).is_empty());
        assert_eq!(merge_kept(&["a".into()], &["b".into(), "a".into()]), ["a", "b"]);
    }

    #[test]
    fn wakes_count_for_half_an_hour() {
        let mut p = AgentPlan::default();
        assert_eq!(recent_wake(&p, 10_000_000), None);
        p.woken_ms = Some(10_000_000);
        assert_eq!(recent_wake(&p, 10_000_000 + WOKEN_FOR_MS), Some(10_000_000));
        assert_eq!(recent_wake(&p, 10_000_000 + WOKEN_FOR_MS + 1), None);
        assert_eq!(after_wake_mode(AfterWake::Suspend, Some(5), None), Some(SleepMode::Suspend));
        assert_eq!(after_wake_mode(AfterWake::Poweroff, Some(5), Some(4)), Some(SleepMode::Poweroff));
        assert_eq!(after_wake_mode(AfterWake::Poweroff, Some(5), Some(5)), None);
        assert_eq!(after_wake_mode(AfterWake::Stay, Some(5), None), None);
        assert_eq!(after_wake_mode(AfterWake::Unknown, Some(5), None), None);
        assert_eq!(after_wake_mode(AfterWake::Suspend, None, None), None);
    }

    #[test]
    fn idle_sessions() {
        let s = parse_session("Class=user\nType=wayland\nIdleHint=yes\nIdleSinceHint=1000\nIdleSinceHintMonotonic=0\n");
        assert_eq!(s, Session { class: "user".into(), kind: "wayland".into(), idle: Some(true), idle_since_us: Some(1000), idle_since_mono_us: None });
        assert_eq!(parse_session_ids("  2 1000 alice seat0 tty2 active no -\nc1 120 gdm seat0 tty1\n"), ["2", "c1"]);
        let wake = WakeTime { wall_us: 5_000, mono_us: Some(500) };
        let user = |idle: Option<bool>, since: Option<u64>, mono: Option<u64>| Session {
            class: "user".into(),
            kind: "tty".into(),
            idle,
            idle_since_us: since,
            idle_since_mono_us: mono,
        };
        let idle_before = user(Some(true), Some(4_000), None);
        let idle_after = user(Some(true), Some(6_000), None);
        let busy = user(Some(false), None, None);
        let idle_mono = user(Some(true), None, Some(400));
        let idle_unknown_since = user(Some(true), None, None);
        let service = Session { class: "manager".into(), idle: Some(false), ..Default::default() };
        let greeter = Session { class: "greeter".into(), idle: Some(false), ..Default::default() };
        let unparsable = Session::default();
        // suspend: every person idle since before the wake
        assert!(may_sleep_after_wake(SleepMode::Suspend, &[], wake));
        assert!(may_sleep_after_wake(SleepMode::Suspend, &[idle_before.clone(), idle_mono, service.clone(), greeter.clone()], wake));
        assert!(!may_sleep_after_wake(SleepMode::Suspend, &[idle_before.clone(), idle_after], wake));
        assert!(!may_sleep_after_wake(SleepMode::Suspend, &[busy], wake));
        assert!(!may_sleep_after_wake(SleepMode::Suspend, &[idle_unknown_since], wake));
        assert!(!may_sleep_after_wake(SleepMode::Suspend, std::slice::from_ref(&unparsable), wake));
        // a locked screen is a person's session too
        let locked = Session { class: "lock-screen".into(), idle: Some(false), ..Default::default() };
        assert!(!may_sleep_after_wake(SleepMode::Suspend, &[locked], wake));
        // shut down: no person's session at all
        assert!(may_sleep_after_wake(SleepMode::Poweroff, &[service, greeter], wake));
        assert!(!may_sleep_after_wake(SleepMode::Poweroff, &[idle_before], wake));
        assert!(!may_sleep_after_wake(SleepMode::Poweroff, &[unparsable], wake));
        assert!(!may_sleep_after_wake(SleepMode::Unknown, &[], wake));

        // from loginctl: no loginctl = unsure
        assert_eq!(sessions_now(&Fake::default()), None);
        let mut f = Fake::default();
        let show = format!("loginctl show-session 3 {}", SESSION_PROPS.join(" "));
        f.outputs.insert("loginctl list-sessions --no-legend --no-pager".into(), (true, "3 1000 alice seat0 tty2\n".into()));
        f.outputs.insert(show.clone(), (true, "Class=user\nType=x11\nIdleHint=yes\nIdleSinceHint=4000\n".into()));
        let got = sessions_now(&f).unwrap();
        assert!(may_sleep_after_wake(SleepMode::Suspend, &got, wake));
        f.outputs.remove(&show);
        assert_eq!(sessions_now(&f), None);
        f.outputs.insert("loginctl list-sessions --no-legend --no-pager".into(), (true, String::new()));
        assert_eq!(sessions_now(&f), Some(vec![]));
        // the wake on the monotonic clock: in the past
        let now = crate::state::now_ms();
        let w = wake_time(now - 60_000);
        assert_eq!(w.wall_us, (now - 60_000) * 1000);
        assert!(w.mono_us.is_some_and(|m| m < clock_us(libc::CLOCK_MONOTONIC).unwrap()));
    }
}
