//! The transaction engine against the scripted kernel of [`crate::fake`].

use std::net::IpAddr;
use std::sync::Arc;

use rynas_api_types::network::{InterfaceConfig, Ipv4Method, NetBackend, TrialKind};

use super::*;
use crate::fake::FakeOs;
use crate::os::NetOs;

struct T {
    os: Arc<FakeOs>,
    e: Engine,
    _state: tempfile::TempDir,
}

fn st(addr: &str, gw: Option<&str>) -> InterfaceConfig {
    InterfaceConfig {
        ipv4: Ipv4Method::Static,
        addresses: vec![addr.into()],
        gateway: gw.map(str::to_string),
        ..Default::default()
    }
}

fn ip(s: &str) -> Option<IpAddr> {
    Some(s.parse().unwrap())
}

fn log(_: &str) {}

/// Records the rescue signals, one per line, in `<state>/signals`.
struct Signals(PathBuf);

impl crate::rescue::Feedback for Signals {
    fn signal(&self, s: crate::rescue::Signal) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.0)
            .unwrap();
        writeln!(f, "{s:?}").unwrap();
    }
}

fn signals(e: &Engine) -> String {
    std::fs::read_to_string(e.paths.run.parent().unwrap().join("signals")).unwrap_or_default()
}

fn engine(os: &Arc<FakeOs>, state: &std::path::Path) -> Engine {
    Engine::new(
        os.clone() as Arc<dyn NetOs>,
        NetPaths::new(state.join("net"), state.join("run")),
    )
    .with_feedback(Some(Arc::new(Signals(state.join("signals")))))
}

/// Two ports on networkd: lan1 static 192.0.2.10/24 (rynas file), lan2 on DHCP (lease).
fn networkd_box() -> T {
    let os = Arc::new(FakeOs::new());
    os.add_port("lan1", "00:11:32:ab:cd:01");
    os.add_port("lan2", "00:11:32:ab:cd:02");
    os.set_active("systemd-networkd", true);
    {
        let mut s = os.state.lock().unwrap();
        s.leases.insert(
            "lan2".into(),
            ("198.51.100.20/24".into(), "198.51.100.1".into()),
        );
        s.reachable = vec!["192.0.2.1".into(), "198.51.100.1".into()];
    }
    os.write_config(
        "/etc/systemd/network/05-rynas-lan1.network",
        &render_file(
            "lan1",
            &st("192.0.2.10/24", Some("192.0.2.1")),
            Some("00:11:32:ab:cd:01"),
        ),
    );
    os.write_config(
        "/etc/systemd/network/05-rynas-lan2.network",
        &render_file(
            "lan2",
            &InterfaceConfig::default(),
            Some("00:11:32:ab:cd:02"),
        ),
    );
    os.run(&["networkctl", "reconfigure", "lan1", "lan2"], QUICK)
        .unwrap();
    let state = tempfile::tempdir().unwrap();
    let e = engine(&os, state.path());
    T {
        os,
        e,
        _state: state,
    }
}

const IFUPDOWN: &str = "source /etc/network/interfaces.d/*\nauto lo\niface lo inet loopback\n\n\
allow-hotplug lan1\niface lan1 inet static\n\taddress 192.0.2.37/24\n\tgateway 192.0.2.1\n\n\
allow-hotplug lan2\niface lan2 inet dhcp\n";

/// Debian as d-i installs it: ifupdown, networkd installed but not running.
fn ifupdown_box() -> T {
    let os = Arc::new(FakeOs::new());
    os.add_port("lan1", "00:11:32:ab:cd:01");
    os.add_port("lan2", "00:11:32:ab:cd:02");
    os.set_active("networking", true);
    os.state.lock().unwrap().reachable.push("192.0.2.1".into());
    os.write_config("/etc/network/interfaces", IFUPDOWN);
    os.run(&["systemctl", "restart", "networking.service"], QUICK)
        .unwrap();
    os.state.lock().unwrap().log.clear();
    let state = tempfile::tempdir().unwrap();
    let e = engine(&os, state.path());
    T {
        os,
        e,
        _state: state,
    }
}

#[test]
fn inventory_view_and_staging() {
    let t = networkd_box();
    let list = t.e.interfaces("admin", ip("192.0.2.10")).unwrap();
    let lan1 = list.iter().find(|i| i.name == "lan1").unwrap();
    assert!(lan1.managed_by_rynas && lan1.editable && lan1.management);
    assert_eq!(lan1.backend, NetBackend::Networkd);
    assert_eq!(lan1.applied, Some(st("192.0.2.10/24", Some("192.0.2.1"))));
    assert_eq!(lan1.link_local_v4.as_deref(), Some("169.254.103.1"));
    assert_eq!(lan1.speed_mbps, Some(1000));
    // staging validates and normalizes; staging the applied value drops the change
    let e =
        t.e.stage("admin", "lan1", &st("192.0.2.10/33", None))
            .unwrap_err();
    assert!(matches!(e, NetError::InvalidField("addresses", _)), "{e}");
    t.e.stage("admin", "lan1", &st(" 192.0.2.20/24", Some("192.0.2.1")))
        .unwrap();
    let s = t.e.state("admin", ip("192.0.2.10")).unwrap();
    assert_eq!(s.pending, vec!["lan1"]);
    assert_eq!(s.management_interface.as_deref(), Some("lan1"));
    assert!(
        t.e.state("bob", None).unwrap().pending.is_empty(),
        "changesets are per user"
    );
    t.e.stage("admin", "lan1", &st("192.0.2.10/24", Some("192.0.2.1")))
        .unwrap();
    assert!(t.e.state("admin", None).unwrap().pending.is_empty());
    assert!(matches!(
        t.e.stage("admin", "nope0", &InterfaceConfig::default()),
        Err(NetError::NotFound(_))
    ));
    t.e.stage(
        "admin",
        "lan2",
        &InterfaceConfig {
            mtu: Some(9000),
            ..Default::default()
        },
    )
    .unwrap();
    t.e.discard("admin").unwrap();
    assert!(matches!(
        t.e.plan_apply("admin", "admin", "", None, None, false),
        Err(NetError::NothingPending(_))
    ));
}

#[test]
fn apply_then_confirm_over_the_new_path() {
    let t = networkd_box();
    t.e.stage("admin", "lan1", &st("192.0.2.20/24", Some("192.0.2.1")))
        .unwrap();
    let plan =
        t.e.plan_apply("admin", "admin", "move", ip("192.0.2.10"), None, false)
            .unwrap();
    assert!(plan.management_touched);
    assert_eq!(plan.interfaces, vec!["lan1"]);
    assert_eq!(plan.removed_addresses, vec!["192.0.2.10"]);
    assert_eq!(plan.timeout_s, 120);
    assert_eq!(plan.candidate_urls[0], "http://192.0.2.20:8080/");
    assert!(
        plan.candidate_urls
            .contains(&"http://nas.local:8080/".to_string())
    );
    assert!(
        plan.candidate_urls
            .contains(&"http://169.254.103.1:8080/".to_string())
    );
    // the management interface cannot skip the test
    assert!(matches!(
        t.e.plan_apply("admin", "admin", "", ip("192.0.2.10"), None, true),
        Err(NetError::Invalid(_))
    ));
    assert!(matches!(
        t.e.plan_apply("admin", "admin", "", None, Some(30), false),
        Err(NetError::Invalid(_))
    ));
    let token = new_token();
    let b =
        t.e.begin(&plan, Some(&token), Some("UPID:x"), &log)
            .unwrap();
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.20/24"]);
    assert_eq!(t.os.state.lock().unwrap().timer.as_deref(), Some("120s"));
    let marker = t.e.trial().unwrap().unwrap();
    assert_eq!((marker.revision, marker.lkg), (b.revision, b.lkg));
    assert_eq!(marker.task_id.as_deref(), Some("UPID:x"));
    let s = t.e.state("admin", ip("192.0.2.20")).unwrap();
    assert_eq!(s.trial.unwrap().kind, TrialKind::Apply);
    // a second change waits
    t.e.stage(
        "admin",
        "lan2",
        &InterfaceConfig {
            mtu: Some(9000),
            ..Default::default()
        },
    )
    .unwrap();
    let p2 =
        t.e.plan_apply("admin", "admin", "", ip("192.0.2.20"), None, false)
            .unwrap();
    assert!(matches!(
        t.e.begin(&p2, Some("t"), None, &log),
        Err(NetError::Conflict(_))
    ));
    t.e.discard("admin").unwrap();
    // confirmations that prove nothing
    assert!(matches!(
        t.e.confirm("nope", Origin::Web(ip("192.0.2.20"))),
        Err(NetError::Invalid(_))
    ));
    for bad in [ip("192.0.2.10"), ip("127.0.0.1"), None, ip("203.0.113.9")] {
        let r = t.e.confirm(&token, Origin::Web(bad));
        assert!(matches!(r, Err(NetError::Conflict(_))), "{bad:?}: {r:?}");
    }
    assert_eq!(
        t.e.confirm(&token, Origin::Web(ip("::ffff:192.0.2.20")))
            .unwrap(),
        b.revision
    );
    assert!(t.e.trial().unwrap().is_none());
    assert_eq!(t.os.state.lock().unwrap().timer, None);
    assert_eq!(t.e.store.state().unwrap().lkg, Some(b.revision));
    assert!(t.e.store.revision(b.revision).unwrap().unwrap().good);
    assert!(matches!(
        t.e.confirm(&token, Origin::Cli),
        Err(NetError::NothingPending(_))
    ));
    // the rollback timer has nothing to do any more
    assert!(t.e.rollback(reason::TIMEOUT, &log).unwrap().is_none());
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.20/24"]);
    let ev = t.e.take_events();
    assert!(
        ev.iter()
            .any(|e| e.key.as_deref() == Some("network:confirmed"))
    );
}

#[test]
fn unconfirmed_change_is_rolled_back_and_verified() {
    let t = networkd_box();
    let mut c = st("192.0.2.30/24", Some("192.0.2.254"));
    c.mtu = Some(9000);
    t.e.stage("admin", "lan1", &c).unwrap();
    let plan =
        t.e.plan_apply("admin", "admin", "", ip("198.51.100.20"), None, false)
            .unwrap();
    assert!(!plan.management_touched, "the session comes through lan2");
    let token = new_token();
    t.e.begin(&plan, Some(&token), None, &log).unwrap();
    assert_eq!(
        (t.os.addrs("lan1"), t.os.mtu("lan1")),
        (vec!["192.0.2.30/24".to_string()], 9000)
    );
    // the timer fires only at the deadline
    assert!(t.e.rollback(reason::TIMEOUT, &log).unwrap().is_none());
    t.os.sleep(Duration::from_secs(121));
    assert!(
        matches!(t.e.confirm(&token, Origin::Cli), Err(NetError::Conflict(_))),
        "too late"
    );
    let r = t.e.rollback(reason::TIMEOUT, &log).unwrap().unwrap();
    assert!(r.verified, "{:?}", r.problems);
    assert_eq!(r.reason, reason::TIMEOUT);
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.10/24"]);
    // networkd keeps the MTU when MTUBytes= disappears: the engine resets it (LP#1768802)
    assert_eq!(t.os.mtu("lan1"), 1500);
    assert!(t.os.ran("ip link set dev lan1 mtu 1500"));
    assert_eq!(
        t.os.state.lock().unwrap().default_routes["lan1"],
        "192.0.2.1"
    );
    assert!(t.e.trial().unwrap().is_none());
    assert_eq!(
        t.e.state("admin", None)
            .unwrap()
            .last_rollback
            .unwrap()
            .restored_revision,
        r.restored_revision
    );
    let ev = t.e.take_events();
    assert!(
        ev.iter()
            .any(|e| e.key.as_deref() == Some("network:rollback")
                && e.severity == NotifySeverity::Warning)
    );
    // the staged change stays for another try
    assert_eq!(t.e.state("admin", None).unwrap().pending, vec!["lan1"]);
    // a later change that is confirmed makes the notice about the rollback obsolete
    let plan =
        t.e.plan_apply("admin", "admin", "", ip("198.51.100.20"), None, false)
            .unwrap();
    let token = new_token();
    t.e.begin(&plan, Some(&token), None, &log).unwrap();
    t.e.confirm(&token, Origin::Cli).unwrap();
    assert!(t.e.state("admin", None).unwrap().last_rollback.is_none());
    // a notice an older rynas left behind after that confirmation is not shown either
    let old = NetRollbackReport { ts_ms: t.os.now_ms() - 60_000, ..r.clone() };
    t.e.store.set_last_rollback(&old).unwrap();
    assert!(t.e.state("admin", None).unwrap().last_rollback.is_none());
}

#[test]
fn a_change_that_does_not_come_up_is_undone_at_once() {
    let t = networkd_box();
    t.os.state.lock().unwrap().broken_networkd = true;
    t.e.stage("admin", "lan1", &st("192.0.2.40/24", None))
        .unwrap();
    let plan =
        t.e.plan_apply("admin", "admin", "", ip("198.51.100.20"), None, true)
            .unwrap();
    let e = t.e.begin(&plan, None, None, &log).unwrap_err();
    assert!(
        e.to_string().contains("did not come up") && e.to_string().contains("verified"),
        "{e}"
    );
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.10/24"]);
    assert!(
        t.os.read_config(Path::new("/etc/systemd/network/05-rynas-lan1.network"))
            .unwrap()
            .contains("192.0.2.10/24")
    );
    assert!(t.e.trial().unwrap().is_none());
    assert_eq!(
        t.e.store.state().unwrap().lkg,
        None,
        "nothing was committed"
    );
}

#[test]
fn applied_without_test_when_the_session_is_elsewhere() {
    let t = networkd_box();
    t.e.stage("admin", "lan1", &st("192.0.2.50/24", Some("192.0.2.1")))
        .unwrap();
    let plan =
        t.e.plan_apply("admin", "admin", "", ip("198.51.100.20"), None, true)
            .unwrap();
    let b = t.e.begin(&plan, None, None, &log).unwrap();
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.50/24"]);
    assert_eq!(t.e.store.state().unwrap().lkg, Some(b.revision));
    assert!(t.e.trial().unwrap().is_none());
    assert!(t.os.state.lock().unwrap().timer.is_none());
    assert!(t.e.state("admin", None).unwrap().pending.is_empty());
}

#[test]
fn duplicate_addresses_and_zero_management_paths_are_refused() {
    let t = networkd_box();
    t.os.state.lock().unwrap().taken.push("192.0.2.60".into());
    t.e.stage("admin", "lan1", &st("192.0.2.60/24", None))
        .unwrap();
    let plan =
        t.e.plan_apply("admin", "admin", "", None, None, false)
            .unwrap();
    let e = t.e.begin(&plan, Some("tok"), None, &log).unwrap_err();
    assert!(e.to_string().contains("already used"), "{e}");
    assert!(t.e.trial().unwrap().is_none());
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.10/24"]);
    let off = InterfaceConfig {
        ipv4: Ipv4Method::Disabled,
        ..Default::default()
    };
    t.e.stage("admin", "lan1", &off).unwrap();
    t.e.stage("admin", "lan2", &off).unwrap();
    let e =
        t.e.plan_apply("admin", "admin", "", None, None, false)
            .unwrap_err();
    assert!(e.to_string().contains("no management path"), "{e}");
}

#[test]
fn the_boot_guard_undoes_an_interrupted_trial() {
    let t = networkd_box();
    let before =
        t.os.read_config(Path::new("/etc/systemd/network/05-rynas-lan1.network"))
            .unwrap();
    t.e.stage("admin", "lan1", &st("192.0.2.70/24", None))
        .unwrap();
    let plan =
        t.e.plan_apply("admin", "admin", "", None, None, false)
            .unwrap();
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    // power cut: a new boot, before networkd starts
    let e2 = engine(&t.os, t._state.path());
    t.os.state.lock().unwrap().log.clear();
    let msg = e2.boot_guard(&log).unwrap();
    assert!(msg.contains("undone"), "{msg}");
    assert_eq!(
        t.os.read_config(Path::new("/etc/systemd/network/05-rynas-lan1.network"))
            .unwrap(),
        before
    );
    assert!(e2.trial().unwrap().is_none());
    assert!(
        !t.os.ran("systemctl disable"),
        "the units of a networkd system stay as they are"
    );
    assert!(
        !t.os.ran("networkctl"),
        "nothing is reloaded before networkd starts"
    );
    assert_eq!(
        e2.store.last_rollback().unwrap().unwrap().report.reason,
        reason::BOOT_GUARD
    );
    assert_eq!(e2.boot_guard(&log).unwrap(), "nothing to do");
}

#[test]
fn watchdog_decisions() {
    let p = |carrier: bool, routable: bool, gateway_ok: Option<bool>| PortHealth {
        name: "x".into(),
        carrier,
        routable,
        gateway: gateway_ok.map(|_| "g".into()),
        gateway_ok,
    };
    assert_eq!(verdict(&[]), Verdict::NoCarrier);
    assert_eq!(verdict(&[p(false, true, None)]), Verdict::NoCarrier);
    assert_eq!(
        verdict(&[p(true, true, None)]),
        Verdict::Healthy,
        "a LAN without gateway"
    );
    assert_eq!(verdict(&[p(true, true, Some(true))]), Verdict::Healthy);
    assert_eq!(verdict(&[p(true, true, Some(false))]), Verdict::Unhealthy);
    assert_eq!(verdict(&[p(true, false, None)]), Verdict::Unhealthy);
    assert_eq!(
        verdict(&[p(true, false, None), p(true, true, Some(true))]),
        Verdict::Healthy
    );
    assert_eq!(
        verdict(&[p(true, false, None), p(false, true, Some(true))]),
        Verdict::Unhealthy
    );
}

#[test]
fn watchdog_healthy_then_rescue() {
    let t = networkd_box();
    assert!(t.e.watchdog(&log).unwrap().starts_with("healthy"));
    // the gateway is gone and lan2 lost its lease
    {
        let mut s = t.os.state.lock().unwrap();
        s.reachable.clear();
        s.leases.clear();
        s.links.get_mut("lan2").unwrap().addrs.clear();
    }
    let msg = t.e.watchdog(&log).unwrap();
    assert!(msg.contains("rescue configuration"), "{msg}");
    let lan1 =
        t.os.read_config(Path::new("/etc/systemd/network/05-rynas-lan1.network"))
            .unwrap();
    assert!(
        lan1.contains("DHCP=ipv4") && lan1.contains("LinkLocalAddressing=yes"),
        "{lan1}"
    );
    assert_eq!(signals(&t.e), "RescueApplied\n");
    let revs = t.e.revisions().unwrap();
    assert_eq!(revs[0].kind, "rescue");
    assert!(revs[0].lkg);
    assert_eq!(
        revs[1].kind, "snapshot",
        "the previous settings can be restored"
    );
    assert!(
        t.e.take_events()
            .iter()
            .any(|e| e.severity == NotifySeverity::Critical)
    );
    // no cable: nothing to judge
    for l in t.os.state.lock().unwrap().links.values_mut() {
        l.carrier = false;
    }
    assert!(t.e.watchdog(&log).unwrap().starts_with("no port"));
}

#[test]
fn reset_button_double_press() {
    let t = networkd_box();
    assert_eq!(t.e.reset_button(&log).unwrap(), Press::First);
    assert_eq!(signals(&t.e), "PressAgain\n");
    t.os.sleep(Duration::from_millis(300));
    assert_eq!(t.e.reset_button(&log).unwrap(), Press::Bounce);
    t.os.sleep(Duration::from_secs(3));
    assert_eq!(t.e.reset_button(&log).unwrap(), Press::Second);
    let lan1 =
        t.os.read_config(Path::new("/etc/systemd/network/05-rynas-lan1.network"))
            .unwrap();
    assert!(lan1.contains("DHCP=ipv4"));
    assert!(
        t.os.addrs("lan1").is_empty(),
        "no DHCP server for lan1 in this test"
    );
    let revs = t.e.revisions().unwrap();
    assert_eq!(
        (revs[0].kind.as_str(), revs[1].kind.as_str()),
        ("rescue", "snapshot")
    );
    // the previous settings come back as a timed trial
    let plan = t.e.plan_restore(revs[1].id, "admin", None, None).unwrap();
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.10/24"]);
    t.e.confirm("tok", Origin::Cli).unwrap();
    // a press after the window starts over
    t.os.sleep(Duration::from_secs(20));
    assert_eq!(t.e.reset_button(&log).unwrap(), Press::First);
    t.os.sleep(Duration::from_secs(11));
    assert_eq!(t.e.reset_button(&log).unwrap(), Press::First);
}

#[test]
fn ifupdown_is_read_only_until_adopted_then_rolled_back() {
    let t = ifupdown_box();
    let list = t.e.interfaces("admin", ip("192.0.2.37")).unwrap();
    let lan1 = list.iter().find(|i| i.name == "lan1").unwrap();
    assert_eq!(lan1.backend, NetBackend::Ifupdown);
    assert!(
        !lan1.editable
            && lan1
                .read_only_reason
                .as_deref()
                .unwrap()
                .contains("let rynas manage")
    );
    assert_eq!(
        lan1.read_only,
        Some(ReadOnlyReason {
            code: ReadOnlyCode::Ifupdown,
            master: None
        })
    );
    assert_eq!(lan1.ifupdown[0].address.as_deref(), Some("192.0.2.37/24"));
    assert!(matches!(
        t.e.stage("admin", "lan1", &InterfaceConfig::default()),
        Err(NetError::Conflict(_))
    ));
    assert!(t.e.state("admin", None).unwrap().ifupdown_adoptable);

    let plan =
        t.e.plan_adopt("admin", ip("192.0.2.37"), Some(300))
            .unwrap();
    assert!(plan.management_touched);
    assert_eq!(
        plan.target.config["lan1"],
        st("192.0.2.37/24", Some("192.0.2.1"))
    );
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    assert_eq!(
        t.os.read_config(Path::new(INTERFACES)).unwrap(),
        ifupdown::LOOPBACK_ONLY
    );
    assert_eq!(
        t.os.read_config(Path::new(INTERFACES_BACKUP)).unwrap(),
        IFUPDOWN
    );
    assert_eq!(
        t.os.addrs("lan1"),
        vec!["192.0.2.37/24"],
        "same address under networkd"
    );
    {
        let s = t.os.state.lock().unwrap();
        assert!(s.active["systemd-networkd"] && !s.active["networking"]);
        assert!(!s.enabled["networking"]);
    }
    assert!(t.os.ran("systemctl stop networking.service"));
    // not confirmed: ifupdown comes back
    let r = t.e.rollback(reason::MANUAL, &log).unwrap().unwrap();
    assert!(r.verified, "{:?}", r.problems);
    assert_eq!(t.os.read_config(Path::new(INTERFACES)).unwrap(), IFUPDOWN);
    assert!(t.os.read_config(Path::new(INTERFACES_BACKUP)).is_none());
    assert!(
        t.os.read_config(Path::new("/etc/systemd/network/05-rynas-lan1.network"))
            .is_none()
    );
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.37/24"]);
    let s = t.os.state.lock().unwrap();
    assert!(!s.active["systemd-networkd"] && s.active["networking"]);
    assert!(s.enabled["networking"]);
}

/// ifupdown ran dhcpcd with a DUID (Debian's default): the DHCP port keeps that identity
/// and its leased address; the static port gets no drop-in; a rollback removes it.
#[test]
fn ifupdown_dhcp_port_keeps_its_identity() {
    let t = ifupdown_box();
    t.os.write_config("/usr/sbin/dhcpcd", "");
    t.os.write_config("/etc/dhcpcd.conf", "# defaults\nhostname\nduid\npersistent\nslaac private\n");
    t.os.write_config("/var/lib/dhcpcd/duid", "00:01:00:01:32:85:aa:ac:00:11:32:ab:cd:02\n");
    // the lease dhcpcd holds under ifupdown, and the one networkd will get
    t.os.add_addr("lan2", "198.51.100.20/24", true);
    t.os.state.lock().unwrap().leases.insert("lan2".into(), ("198.51.100.20/24".into(), "198.51.100.1".into()));
    let plan = t.e.plan_adopt("admin", ip("192.0.2.37"), Some(300)).unwrap();
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    let dropin = "/etc/systemd/network/05-rynas-lan2.network.d/10-dhcp-identity.conf";
    let text = t.os.read_config(Path::new(dropin)).expect("drop-in written");
    assert!(text.contains("ClientIdentifier=duid\n"), "{text}");
    assert!(text.contains(&format!("IAID={}\n", 0x32abcd02u32)), "{text}");
    assert!(text.contains("DUIDType=1\nDUIDRawData=00:01:32:85:aa:ac:00:11:32:ab:cd:02\n"), "{text}");
    assert!(text.contains("RequestAddress=198.51.100.20\n"), "{text}");
    assert!(t.os.read_config(Path::new("/etc/systemd/network/05-rynas-lan1.network.d/10-dhcp-identity.conf")).is_none());
    t.e.rollback(reason::MANUAL, &log).unwrap().unwrap();
    assert!(t.os.read_config(Path::new(dropin)).is_none(), "gone with the rollback");
}

#[test]
fn ifupdown_adopted_and_confirmed() {
    let t = ifupdown_box();
    let plan = t.e.plan_adopt("admin", ip("192.0.2.37"), None).unwrap();
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    // the confirmation arrives on the same address, now held by networkd: that is the new path
    t.e.confirm("tok", Origin::Web(ip("192.0.2.37"))).unwrap();
    let list = t.e.interfaces("admin", None).unwrap();
    let lan1 = list.iter().find(|i| i.name == "lan1").unwrap();
    assert_eq!(lan1.backend, NetBackend::Networkd);
    assert!(lan1.editable && lan1.managed_by_rynas);
}

#[test]
fn ifupdown_migrated_at_next_boot() {
    let t = ifupdown_box();
    let (rev, ifaces) = t.e.adopt_next_boot("admin").unwrap();
    assert_eq!(ifaces, vec!["lan1", "lan2"]);
    assert_eq!(
        t.os.read_config(Path::new(INTERFACES)).unwrap(),
        IFUPDOWN,
        "nothing changes now"
    );
    // reboot: the guard migrates before networking starts
    let msg = t.e.boot_guard(&log).unwrap();
    assert!(msg.contains("migrated"), "{msg}");
    assert_eq!(
        t.os.read_config(Path::new(INTERFACES)).unwrap(),
        ifupdown::LOOPBACK_ONLY
    );
    assert!(t.os.ran("systemctl disable networking.service"));
    assert_eq!(t.os.state.lock().unwrap().timer.as_deref(), Some("180s"));
    let m = t.e.trial().unwrap().unwrap();
    assert_eq!((m.kind, m.revision), (TrialKind::AdoptAtBoot, rev));
    t.os.sleep(Duration::from_secs(181));
    // a routable address: kept
    assert!(t.e.rollback(reason::BOOT_CHECK, &log).unwrap().is_none());
    assert!(t.e.trial().unwrap().is_none());
    assert_eq!(t.e.store.state().unwrap().lkg, Some(rev));
}

#[test]
fn migration_at_boot_without_address_goes_back_to_ifupdown() {
    let t = ifupdown_box();
    t.e.adopt_next_boot("admin").unwrap();
    // networkd will not bring lan1 up (say, a driver issue)
    t.os.state.lock().unwrap().broken_networkd = true;
    {
        let mut s = t.os.state.lock().unwrap();
        for l in s.links.values_mut() {
            l.addrs.clear();
        }
        s.default_routes.clear();
    }
    t.e.boot_guard(&log).unwrap();
    t.os.sleep(Duration::from_secs(181));
    let r = t.e.rollback(reason::BOOT_CHECK, &log).unwrap().unwrap();
    assert_eq!(r.reason, reason::BOOT_CHECK);
    assert_eq!(t.os.read_config(Path::new(INTERFACES)).unwrap(), IFUPDOWN);
    assert_eq!(t.os.addrs("lan1"), vec!["192.0.2.37/24"]);
    assert!(t.os.state.lock().unwrap().active["networking"]);
}

#[test]
fn read_only_codes_keep_their_english_text() {
    let r = |code, master: Option<&str>| {
        Engine::read_only_text(&ReadOnlyReason {
            code,
            master: master.map(str::to_string),
        })
    };
    assert_eq!(
        r(ReadOnlyCode::Enslaved, Some("bond0")),
        "a member of bond0: change it from there"
    );
    for c in [
        ReadOnlyCode::Loopback,
        ReadOnlyCode::NotEthernet,
        ReadOnlyCode::Ifupdown,
        ReadOnlyCode::NetworkManager,
        ReadOnlyCode::NoNetworkd,
        ReadOnlyCode::Other,
    ] {
        assert!(!r(c, None).is_empty());
    }
    let j = serde_json::to_string(&ReadOnlyReason {
        code: ReadOnlyCode::NetworkManager,
        master: None,
    })
    .unwrap();
    assert_eq!(j, r#"{"code":"network_manager"}"#);
    let unknown: ReadOnlyReason = serde_json::from_str(r#"{"code":"wifi"}"#).unwrap();
    assert_eq!(unknown.code, ReadOnlyCode::Other);
}

#[test]
fn planned_migration_is_shown_to_every_session_and_cancelled() {
    let t = ifupdown_box();
    let (rev, _) = t.e.adopt_next_boot("admin").unwrap();
    // another session, after a reload: the plan is visible (deadline 0 = before the reboot)
    let tr = t.e.state("other", None).unwrap().trial.unwrap();
    assert_eq!(
        (tr.kind, tr.revision, tr.deadline_ms, tr.author.as_str()),
        (TrialKind::AdoptAtBoot, rev, 0, "admin")
    );
    assert_eq!(tr.interfaces, vec!["lan1", "lan2"]);
    assert!(t.e.rollback(reason::MANUAL, &log).unwrap().is_none());
    assert!(t.e.state("other", None).unwrap().trial.is_none());
    assert_eq!(t.e.boot_guard(&log).unwrap(), "nothing to do");
    assert_eq!(t.os.read_config(Path::new(INTERFACES)).unwrap(), IFUPDOWN);
}

#[test]
fn migration_at_boot_rolled_back_by_hand_goes_back_to_ifupdown() {
    let t = ifupdown_box();
    t.e.adopt_next_boot("admin").unwrap();
    t.e.boot_guard(&log).unwrap();
    // networkd came up with an address, yet the administrator asks to go back: ifupdown returns
    // (it used to keep the migration instead)
    let r = t.e.rollback(reason::MANUAL, &log).unwrap().unwrap();
    assert_eq!(r.reason, reason::MANUAL);
    assert_eq!(t.os.read_config(Path::new(INTERFACES)).unwrap(), IFUPDOWN);
    assert!(t.e.trial().unwrap().is_none());
    let s = t.os.state.lock().unwrap();
    assert!(s.active["networking"] && s.enabled["networking"]);
    assert!(!s.active["systemd-networkd"]);
}

#[test]
fn migration_at_boot_kept_by_any_admin_without_a_token() {
    let t = ifupdown_box();
    let (rev, _) = t.e.adopt_next_boot("admin").unwrap();
    t.e.boot_guard(&log).unwrap();
    let tr = t.e.state("other", None).unwrap().trial.unwrap();
    assert_eq!(tr.kind, TrialKind::AdoptAtBoot);
    assert!(tr.deadline_ms > 0, "running now");
    // no browser applied it: no token exists, the address check still applies
    assert!(matches!(
        t.e.confirm("", Origin::Web(ip("127.0.0.1"))),
        Err(NetError::Conflict(_))
    ));
    assert_eq!(t.e.confirm("", Origin::Web(ip("192.0.2.37"))).unwrap(), rev);
    assert!(t.e.trial().unwrap().is_none());
    assert_eq!(t.e.store.state().unwrap().lkg, Some(rev));
    assert!(t.os.state.lock().unwrap().timer.is_none(), "timer disarmed");
}

/// What dhcpcd leaves in `/etc/resolv.conf` once it released its lease (ifupdown stopped).
const DHCPCD_LEFTOVER: &str = "# Generated by dhcpcd\n# /etc/resolv.conf.head can replace this line\n# /etc/resolv.conf.tail can replace this line\n";

fn event_codes(e: &Engine) -> Vec<String> {
    e.take_events().into_iter().map(|e| e.code).collect()
}

/// ifupdown box whose DHCP port gets a lease with a DNS server, migrated at boot (the guard
/// ran, the machine came up with networkd only).
fn migrated_at_boot(dns_answers: bool) -> T {
    let t = ifupdown_box();
    {
        let mut s = t.os.state.lock().unwrap();
        s.leases.insert(
            "lan2".into(),
            ("198.51.100.20/24".into(), "198.51.100.1".into()),
        );
        s.lease_dns
            .insert("lan2".into(), vec!["198.51.100.53".into()]);
        if dns_answers {
            s.dns_up.push("198.51.100.53".into());
        }
    }
    t.os.write_config("/etc/resolv.conf", DHCPCD_LEFTOVER);
    t.e.adopt_next_boot("admin").unwrap();
    t.e.boot_guard(&log).unwrap();
    // the boot itself: ifupdown no longer starts, networkd applies the files
    t.os.set_active("networking", false);
    t.os.run(&["networkctl", "reconfigure", "lan1", "lan2"], QUICK)
        .unwrap();
    t.os.sleep(Duration::from_secs(181));
    t
}

#[test]
fn migration_at_boot_writes_resolv_conf_and_is_kept() {
    let t = migrated_at_boot(true);
    assert!(t.e.rollback(reason::BOOT_CHECK, &log).unwrap().is_none());
    assert!(t.e.trial().unwrap().is_none(), "kept");
    let r = t.os.read_config(Path::new("/etc/resolv.conf")).unwrap();
    assert!(r.starts_with(resolv::HEADER), "{r}");
    assert!(r.contains("nameserver 198.51.100.53\n"), "{r}");
    let codes = event_codes(&t.e);
    assert!(codes.contains(&"net.dns_repaired".to_string()), "{codes:?}");
    assert!(codes.contains(&"net.adopted".to_string()), "{codes:?}");
    // in step afterwards: nothing to write, no new event
    assert!(t.e.sync_resolv(&log).is_none());
    assert!(event_codes(&t.e).is_empty());
}

/// As seen on a DS920+: `networking.service` was already queued when the guard disabled it,
/// so it stays "active (exited)" for that boot with a loopback-only file. It must not block
/// the repair of `/etc/resolv.conf`.
#[test]
fn exited_ifupdown_with_loopback_only_does_not_block_the_repair() {
    let t = migrated_at_boot(true);
    t.os.set_active("networking", true);
    t.os.write_config("/etc/resolv.conf", DHCPCD_LEFTOVER);
    let what = t.e.sync_resolv(&log).expect("repaired");
    assert!(what.contains("198.51.100.53"), "{what}");
    let r = t.os.read_config(Path::new("/etc/resolv.conf")).unwrap();
    assert!(r.starts_with(resolv::HEADER), "{r}");
}

#[test]
fn migration_at_boot_without_dns_goes_back_to_ifupdown() {
    let t = migrated_at_boot(false);
    let r = t.e.rollback(reason::BOOT_CHECK, &log).unwrap().unwrap();
    assert_eq!(r.reason, reason::BOOT_CHECK);
    assert_eq!(t.os.read_config(Path::new(INTERFACES)).unwrap(), IFUPDOWN);
    assert!(t.os.state.lock().unwrap().active["networking"]);
    let codes = event_codes(&t.e);
    assert!(
        codes.contains(&"net.boot_dns_failed".to_string()),
        "{codes:?}"
    );
    assert!(!codes.contains(&"net.adopted".to_string()), "{codes:?}");
}

#[test]
fn an_older_migration_left_without_dns_is_repaired_once() {
    // networkd runs rynas files, DHCP gave a DNS server, resolv.conf is dhcpcd's leftover
    let t = networkd_box();
    t.os.state
        .lock()
        .unwrap()
        .lease_dns
        .insert("lan2".into(), vec!["198.51.100.53".into(), "192.0.2.53#dns.example".into()]);
    t.os.run(&["networkctl", "reconfigure", "lan2"], QUICK)
        .unwrap();
    t.os.write_config("/etc/resolv.conf", DHCPCD_LEFTOVER);
    let what = t.e.sync_resolv(&log).unwrap();
    assert!(what.contains("198.51.100.53, 192.0.2.53"), "{what}");
    assert_eq!(event_codes(&t.e), vec!["net.dns_repaired"]);
    // a file the administrator wrote is never touched
    t.os.write_config("/etc/resolv.conf", "nameserver 192.0.2.99\n");
    assert!(t.e.sync_resolv(&log).is_none());
    assert_eq!(
        t.os.read_config(Path::new("/etc/resolv.conf")).unwrap(),
        "nameserver 192.0.2.99\n"
    );
    // ifupdown running too, with a port in its file: not ours to write
    t.os.write_config("/etc/resolv.conf", DHCPCD_LEFTOVER);
    t.os.write_config(
        "/etc/network/interfaces",
        "auto lo\niface lo inet loopback\n\nallow-hotplug lan2\niface lan2 inet dhcp\n",
    );
    t.os.set_active("networking", true);
    assert!(t.e.sync_resolv(&log).is_none());
}

#[test]
fn systemd_resolved_gets_the_link() {
    let t = networkd_box();
    t.os.set_active("systemd-resolved", true);
    t.os.add_runtime_file(resolv::RESOLVED_STUB, "nameserver 127.0.0.53\n");
    t.os.add_runtime_file(resolv::RESOLVED_UPSTREAM, "nameserver 198.51.100.53\n");
    t.os.state.lock().unwrap().dns_up.push("198.51.100.53".into());
    let what = t.e.sync_resolv(&log).unwrap();
    assert!(what.contains("systemd-resolved"), "{what}");
    let real = crate::fake::real(&t.os, "/etc/resolv.conf");
    assert_eq!(
        std::fs::read_link(&real).unwrap(),
        Path::new(resolv::RESOLVED_STUB)
    );
    // the stub's upstream servers are the ones probed
    assert_eq!(t.e.dns_answers_now(), Some(true));
    assert!(t.e.sync_resolv(&log).is_none(), "a link to resolved is resolved's");
}

#[test]
fn a_change_that_breaks_name_resolution_is_rolled_back() {
    let t = networkd_box();
    {
        let mut s = t.os.state.lock().unwrap();
        s.lease_dns
            .insert("lan2".into(), vec!["198.51.100.53".into()]);
        s.dns_up.push("198.51.100.53".into());
    }
    t.os.run(&["networkctl", "reconfigure", "lan2"], QUICK)
        .unwrap();
    t.e.sync_resolv(&log).unwrap();
    assert_eq!(t.e.dns_answers_now(), Some(true));
    // lan2 static with a DNS server that does not answer
    let mut c = st("198.51.100.20/24", None);
    c.dns = vec!["198.51.100.99".into()];
    t.e.stage("admin", "lan2", &c).unwrap();
    let plan =
        t.e.plan_apply("admin", "admin", "", ip("192.0.2.10"), None, false)
            .unwrap();
    let err = t.e.begin(&plan, Some("tok"), None, &log).unwrap_err();
    assert!(
        matches!(&err, NetError::Failed(m) if m.contains("name resolution stopped working")),
        "{err}"
    );
    assert!(t.e.trial().unwrap().is_none());
    assert_eq!(t.os.addrs("lan2"), vec!["198.51.100.20/24"], "DHCP again");
    // the next refresh puts the leased server back
    t.e.sync_resolv(&log).unwrap();
    assert_eq!(t.e.dns_answers_now(), Some(true));
}

#[test]
fn no_dns_before_no_dns_after_is_not_a_failure() {
    let t = networkd_box();
    t.e.stage("admin", "lan1", &st("192.0.2.20/24", Some("192.0.2.1")))
        .unwrap();
    let plan =
        t.e.plan_apply("admin", "admin", "", ip("192.0.2.10"), None, false)
            .unwrap();
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    assert!(t.e.trial().unwrap().is_some());
}

// ---------------------------------------------------------------------------------------------
// bonds
// ---------------------------------------------------------------------------------------------

use rynas_api_types::network::{BondConfig, BondHash, BondMode};

const MAC3: &str = "24:5e:be:00:00:03";
const MAC4: &str = "24:5e:be:00:00:04";
const DROPIN3: &str = "/etc/systemd/network/05-rynas-enp3s0.network.d/10-dhcp-identity.conf";
const DROPIN_BOND: &str = "/etc/systemd/network/05-rynas-bond0.network.d/10-dhcp-identity.conf";
const IDENTITY: &str = "# rynas: the DHCP identity dhcpcd used before rynas took over this port, so the\n\
# server keeps giving the same address.\n[DHCPv4]\nClientIdentifier=duid\nIAID=3187671043\n\
DUIDType=1\nDUIDRawData=00:01:32:85:aa:ac:24:5e:be:00:00:03\nRequestAddress=192.0.2.37\n";

/// Two 1 GbE ports on networkd, both on DHCP (as an adoption from ifupdown leaves them):
/// enp3s0 holds the lease the session comes through and the identity drop-in, enp4s0 is
/// cabled too. The DHCP server gives the same address to the bond (same identity).
fn two_port_box() -> T {
    let os = Arc::new(FakeOs::new());
    os.add_port("enp3s0", MAC3);
    os.add_port("enp4s0", MAC4);
    os.set_active("systemd-networkd", true);
    {
        let mut s = os.state.lock().unwrap();
        let lease = ("192.0.2.37/24".to_string(), "192.0.2.1".to_string());
        s.leases.insert("enp3s0".into(), lease.clone());
        s.leases.insert("bond0".into(), lease);
        s.leases.insert("enp4s0".into(), ("192.0.2.38/24".into(), "192.0.2.1".into()));
        s.reachable = vec!["192.0.2.1".into()];
    }
    for (p, mac) in [("enp3s0", MAC3), ("enp4s0", MAC4)] {
        os.write_config(
            &format!("/etc/systemd/network/05-rynas-{p}.network"),
            &render_file(p, &InterfaceConfig::default(), Some(mac)),
        );
    }
    os.write_config(DROPIN3, IDENTITY);
    os.run(&["networkctl", "reload"], QUICK).unwrap();
    let state = tempfile::tempdir().unwrap();
    let e = engine(&os, state.path());
    T { os, e, _state: state }
}

fn bond_cfg(mode: BondMode) -> InterfaceConfig {
    InterfaceConfig {
        bond: Some(BondConfig {
            members: vec!["enp4s0".into(), "enp3s0".into()],
            mode,
            miimon_ms: 100,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn master(t: &T, port: &str) -> Option<String> {
    t.os.state.lock().unwrap().links[port].master.clone()
}

fn bond_mode(t: &T) -> Option<String> {
    t.os.state
        .lock()
        .unwrap()
        .links
        .get("bond0")
        .and_then(|l| l.bond_mode.clone())
}

/// The ports become an active-backup bond on DHCP: the bond takes the MAC and the DHCP
/// identity of the port that had the address, so the address and the page stay.
fn make_bond(t: &T) -> u64 {
    t.e.stage("admin", "bond0", &bond_cfg(BondMode::ActiveBackup)).unwrap();
    let plan = t.e.plan_apply("admin", "admin", "bond", ip("192.0.2.37"), None, false).unwrap();
    let token = new_token();
    t.e.begin(&plan, Some(&token), None, &log).unwrap();
    t.e.confirm(&token, Origin::Web(ip("192.0.2.37"))).unwrap()
}

#[test]
fn a_dhcp_port_becomes_a_bond_and_keeps_mac_and_identity() {
    let t = two_port_box();
    t.e.stage("admin", "bond0", &bond_cfg(BondMode::ActiveBackup)).unwrap();
    // the staged bond shows up before it exists, normalized
    let list = t.e.interfaces("admin", ip("192.0.2.37")).unwrap();
    let staged = list.iter().find(|i| i.name == "bond0").unwrap();
    let b = staged.pending.as_ref().unwrap().bond.clone().unwrap();
    assert_eq!(b.members, vec!["enp3s0", "enp4s0"]);
    assert_eq!(b.mac.as_deref(), Some(MAC3), "the MAC of the port with the address");
    assert_eq!(staged.link_local_v4.as_deref(), Some("169.254.1.3"));
    // a bond change never skips the timed test
    assert!(matches!(
        t.e.plan_apply("admin", "admin", "", None, None, true),
        Err(NetError::Invalid(m)) if m.contains("timed test")
    ));
    let plan = t.e.plan_apply("admin", "admin", "bond", ip("192.0.2.37"), None, false).unwrap();
    assert!(plan.management_touched);
    assert_eq!(plan.interfaces, vec!["bond0", "enp3s0", "enp4s0"]);
    assert!(plan.candidate_urls.contains(&"http://192.0.2.37:8080/".to_string()));
    assert!(plan.removed_addresses.is_empty());
    let file = |p: &str| {
        plan.target
            .files
            .iter()
            .find(|f| f.path == Path::new(p))
            .unwrap_or_else(|| panic!("{p} in the plan"))
            .content
            .clone()
    };
    let netdev = file("/etc/systemd/network/05-rynas-bond0.netdev").unwrap();
    assert!(netdev.contains(&format!("Kind=bond\nMACAddress={MAC3}\n")), "{netdev}");
    assert!(netdev.contains("Mode=active-backup\nMIIMonitorSec=100ms\n"), "{netdev}");
    assert!(file("/etc/systemd/network/05-rynas-enp3s0.network").unwrap().contains("Bond=bond0\nLinkLocalAddressing=no\n"));
    // the identity moves to the bond as it is
    assert_eq!(file(DROPIN_BOND).as_deref(), Some(IDENTITY));
    assert_eq!(file(DROPIN3), None);
    let token = new_token();
    t.e.begin(&plan, Some(&token), None, &log).unwrap();
    assert_eq!(bond_mode(&t).as_deref(), Some("active-backup"));
    assert_eq!((master(&t, "enp3s0"), master(&t, "enp4s0")), (Some("bond0".into()), Some("bond0".into())));
    assert_eq!(t.os.addrs("bond0"), vec!["192.0.2.37/24"]);
    assert!(t.os.addrs("enp3s0").is_empty() && t.os.addrs("enp4s0").is_empty());
    assert_eq!(t.os.state.lock().unwrap().links["bond0"].mac, MAC3);
    assert_eq!(t.os.read_config(Path::new(DROPIN_BOND)).as_deref(), Some(IDENTITY));
    assert!(t.os.read_config(Path::new(DROPIN3)).is_none());
    // the same address, now on the bond, confirms
    let rev = t.e.confirm(&token, Origin::Web(ip("192.0.2.37"))).unwrap();
    assert_eq!(t.e.store.state().unwrap().lkg, Some(rev));
    assert!(t.e.state("admin", None).unwrap().pending.is_empty());
    let cur = t.e.current_config();
    assert_eq!(cur.keys().collect::<Vec<_>>(), vec!["bond0"]);
    assert_eq!(cur["bond0"].bond.as_ref().unwrap().mac.as_deref(), Some(MAC3));
    // the page: the bond is editable with its members' state, the members are not
    t.e.invalidate();
    let list = t.e.interfaces("admin", ip("192.0.2.37")).unwrap();
    let bond = list.iter().find(|i| i.name == "bond0").unwrap();
    assert!(bond.editable && bond.managed_by_rynas && bond.management, "{bond:?}");
    let st = bond.bond.as_ref().unwrap();
    assert_eq!(st.mode.as_deref(), Some("active-backup"));
    assert_eq!(st.active_member.as_deref(), Some("enp3s0"));
    assert_eq!(st.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["enp3s0", "enp4s0"]);
    assert!(st.members.iter().all(|m| m.link_up));
    let enp4 = list.iter().find(|i| i.name == "enp4s0").unwrap();
    assert_eq!(enp4.read_only.as_ref().unwrap().master.as_deref(), Some("bond0"));
    assert!(matches!(t.e.stage("admin", "enp4s0", &InterfaceConfig::default()), Err(NetError::Conflict(_))));
    // the rescue address lives on the bond
    let ll = t.e.link_locals().unwrap();
    assert_eq!(ll.iter().map(|l| l.interface.as_str()).collect::<Vec<_>>(), vec!["bond0"]);
}

#[test]
fn bond_staging_rules() {
    let t = two_port_box();
    let mut c = bond_cfg(BondMode::ActiveBackup);
    c.bond.as_mut().unwrap().members.push("nope9".into());
    assert!(matches!(t.e.stage("admin", "bond0", &c), Err(NetError::InvalidField("bond.members", _))));
    assert!(matches!(
        t.e.stage("admin", "team0", &bond_cfg(BondMode::ActiveBackup)),
        Err(NetError::InvalidField("name", _))
    ));
    let mut c = bond_cfg(BondMode::BalanceAlb);
    c.bond.as_mut().unwrap().hash_policy = Some(BondHash::Layer34);
    assert!(matches!(t.e.stage("admin", "bond0", &c), Err(NetError::InvalidField("bond.hash_policy", _))));
    let mut c = bond_cfg(BondMode::ActiveBackup);
    c.mtu = Some(9500);
    assert!(matches!(t.e.stage("admin", "bond0", &c), Err(NetError::InvalidField("mtu", _))));
    // a port staged on its own is dropped when it joins the bond
    t.e.stage("admin", "enp4s0", &InterfaceConfig { mtu: Some(9000), ..Default::default() }).unwrap();
    t.e.stage("admin", "bond0", &bond_cfg(BondMode::Lacp)).unwrap();
    assert_eq!(t.e.state("admin", None).unwrap().pending, vec!["bond0"]);
    // a second bond cannot take the same ports
    assert!(t.e.stage("admin", "bond1", &bond_cfg(BondMode::ActiveBackup)).is_err());
    // 802.3ad gets layer2+3
    let list = t.e.interfaces("admin", None).unwrap();
    let b = list.iter().find(|i| i.name == "bond0").unwrap().pending.clone().unwrap().bond.unwrap();
    assert_eq!(b.hash_policy, Some(BondHash::Layer23));
    // DELETE of a bond that is only staged drops it
    t.e.stage_remove("admin", "bond0").unwrap();
    assert!(t.e.state("admin", None).unwrap().pending.is_empty());
    assert!(matches!(t.e.stage_remove("admin", "bond0"), Err(NetError::NotFound(_))));
    // no member with a cable: refused
    for p in ["enp3s0", "enp4s0"] {
        t.os.state.lock().unwrap().links.get_mut(p).unwrap().carrier = false;
    }
    t.e.invalidate();
    assert!(matches!(t.e.stage("admin", "bond0", &bond_cfg(BondMode::ActiveBackup)), Err(NetError::InvalidField("bond.members", m)) if m.contains("carrier")));
}

/// Both ports have a lease: the bond takes the MAC of the port the session comes through, and
/// that port had no drop-in, so the identity networkd used for its lease is written down.
#[test]
fn the_session_port_gives_the_bond_its_mac_and_lease_identity() {
    let t = two_port_box();
    let idx = t.os.state.lock().unwrap().links["enp4s0"].index;
    t.os.add_runtime_file(
        &format!("/run/systemd/netif/leases/{idx}"),
        "# This is private data. Do not parse.\nADDRESS=192.0.2.38\nNETMASK=255.255.255.0\n\
         CLIENTID=ff00000004000200005ab12bc6e93a1c41\n",
    );
    t.e.stage_from("admin", "bond0", &bond_cfg(BondMode::ActiveBackup), ip("192.0.2.38")).unwrap();
    let plan = t.e.plan_apply("admin", "admin", "", ip("192.0.2.38"), None, false).unwrap();
    assert_eq!(plan.target.config["bond0"].bond.as_ref().unwrap().mac.as_deref(), Some(MAC4));
    let dropin = plan.target.files.iter().find(|f| f.path == Path::new(DROPIN_BOND)).unwrap();
    let text = dropin.content.clone().unwrap();
    assert!(text.contains("ClientIdentifier=duid\nIAID=4\nDUIDType=2\nDUIDRawData=00:00:5a:b1:2b:c6:e9:3a:1c:41\nRequestAddress=192.0.2.38\n"), "{text}");
    // enp3s0 keeps its own drop-in (not the bond's identity)
    assert!(!plan.target.files.iter().any(|f| f.path == Path::new(DROPIN3)));
    assert!(plan.candidate_urls.contains(&"http://192.0.2.38:8080/".to_string()));
}

#[test]
fn an_unconfirmed_bond_is_rolled_back_and_the_ports_come_back() {
    let t = two_port_box();
    t.e.stage("admin", "bond0", &bond_cfg(BondMode::ActiveBackup)).unwrap();
    let plan = t.e.plan_apply("admin", "admin", "", ip("192.0.2.37"), None, false).unwrap();
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    assert_eq!(master(&t, "enp3s0").as_deref(), Some("bond0"));
    t.os.sleep(Duration::from_secs(121));
    let r = t.e.rollback(reason::TIMEOUT, &log).unwrap().unwrap();
    assert!(r.verified, "{:?}", r.problems);
    // the bond is deleted (networkd would keep it), the ports are free with their lease
    assert!(t.os.ran("ip link del dev bond0"));
    assert!(bond_mode(&t).is_none());
    assert_eq!((master(&t, "enp3s0"), master(&t, "enp4s0")), (None, None));
    assert_eq!(t.os.addrs("enp3s0"), vec!["192.0.2.37/24"]);
    assert_eq!(t.os.read_config(Path::new(DROPIN3)).as_deref(), Some(IDENTITY), "the identity is back on the port");
    assert!(t.os.read_config(Path::new(DROPIN_BOND)).is_none());
    assert!(t.os.read_config(Path::new("/etc/systemd/network/05-rynas-bond0.netdev")).is_none());
    assert!(!t.e.current_config().contains_key("bond0"));
    // the staged bond stays for another try
    assert_eq!(t.e.state("admin", None).unwrap().pending, vec!["bond0"]);
}

#[test]
fn a_mode_change_deletes_the_bond_and_enslaves_the_ports_again() {
    let t = two_port_box();
    make_bond(&t);
    t.os.state.lock().unwrap().lacp_partner = Some("00:11:22:33:44:55".into());
    t.os.state.lock().unwrap().log.clear();
    t.e.stage("admin", "bond0", &bond_cfg(BondMode::Lacp)).unwrap();
    let plan = t.e.plan_apply("admin", "admin", "", ip("192.0.2.37"), None, false).unwrap();
    assert_eq!(plan.interfaces, vec!["bond0", "enp3s0", "enp4s0"]);
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    assert!(t.os.ran("ip link del dev bond0"));
    assert!(t.os.ran("networkctl reconfigure enp3s0 enp4s0"));
    assert_eq!(bond_mode(&t).as_deref(), Some("802.3ad"));
    assert_eq!(master(&t, "enp4s0").as_deref(), Some("bond0"));
    assert_eq!(t.os.addrs("bond0"), vec!["192.0.2.37/24"]);
    assert_eq!(t.os.state.lock().unwrap().links["bond0"].mac, MAC3, "the MAC stays");
    t.e.confirm("tok", Origin::Web(ip("192.0.2.37"))).unwrap();
    t.e.invalidate();
    let list = t.e.interfaces("admin", None).unwrap();
    let st = list.iter().find(|i| i.name == "bond0").unwrap().bond.clone().unwrap();
    assert_eq!((st.lacp_ok, st.partner_mac.as_deref()), (Some(true), Some("00:11:22:33:44:55")));
}

#[test]
fn lacp_without_a_switch_group_is_rolled_back() {
    let t = two_port_box();
    make_bond(&t);
    t.os.state.lock().unwrap().log.clear();
    let start = t.os.now_ms();
    t.e.stage("admin", "bond0", &bond_cfg(BondMode::Lacp)).unwrap();
    let plan = t.e.plan_apply("admin", "admin", "", ip("192.0.2.37"), None, false).unwrap();
    let err = t.e.begin(&plan, Some("tok"), None, &log).unwrap_err();
    assert!(
        matches!(&err, NetError::Failed(m) if m.contains("the switch is not configured for LACP") && m.contains("verified")),
        "{err}"
    );
    // it waited for a partner before giving up
    assert!(t.os.now_ms() - start >= 30_000);
    // the active-backup bond runs again: deleted and made again, the ports enslaved
    assert_eq!(bond_mode(&t).as_deref(), Some("active-backup"));
    assert_eq!(t.os.log().iter().filter(|l| *l == "ip link del dev bond0").count(), 2);
    assert_eq!(master(&t, "enp3s0").as_deref(), Some("bond0"));
    assert_eq!(t.os.addrs("bond0"), vec!["192.0.2.37/24"]);
    assert!(t.e.trial().unwrap().is_none());
}

#[test]
fn a_deleted_bond_hands_its_settings_and_identity_to_the_port() {
    let t = two_port_box();
    make_bond(&t);
    t.e.stage_remove("admin", "bond0").unwrap();
    let list = t.e.interfaces("admin", None).unwrap();
    assert!(list.iter().find(|i| i.name == "bond0").unwrap().pending.as_ref().unwrap().removed);
    let plan = t.e.plan_apply("admin", "admin", "", ip("192.0.2.37"), None, false).unwrap();
    assert_eq!(plan.interfaces, vec!["bond0", "enp3s0", "enp4s0"]);
    assert_eq!(plan.target.config["enp3s0"], InterfaceConfig::default(), "the port with the bond's MAC");
    assert_eq!(plan.target.config["enp4s0"].ipv4, Ipv4Method::Disabled);
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    assert!(t.os.ran("ip link del dev bond0"));
    assert!(bond_mode(&t).is_none());
    assert_eq!((master(&t, "enp3s0"), master(&t, "enp4s0")), (None, None));
    assert_eq!(t.os.addrs("enp3s0"), vec!["192.0.2.37/24"]);
    assert_eq!(t.os.read_config(Path::new(DROPIN3)).as_deref(), Some(IDENTITY));
    assert!(t.os.read_config(Path::new(DROPIN_BOND)).is_none());
    t.e.confirm("tok", Origin::Web(ip("192.0.2.37"))).unwrap();
    assert!(t.e.state("admin", None).unwrap().pending.is_empty(), "the deletion is done");
    let cur = t.e.current_config();
    assert_eq!(cur.keys().collect::<Vec<_>>(), vec!["enp3s0", "enp4s0"]);
}

#[test]
fn removing_a_member_frees_it() {
    let t = two_port_box();
    make_bond(&t);
    let mut c = bond_cfg(BondMode::ActiveBackup);
    c.bond.as_mut().unwrap().members = vec!["enp3s0".into()];
    t.e.stage("admin", "bond0", &c).unwrap();
    assert_eq!(t.e.state("admin", None).unwrap().pending, vec!["bond0", "enp4s0"]);
    let plan = t.e.plan_apply("admin", "admin", "", ip("192.0.2.37"), None, false).unwrap();
    t.e.begin(&plan, Some("tok"), None, &log).unwrap();
    assert!(t.os.ran("ip link set dev enp4s0 nomaster"));
    assert!(!t.os.ran("ip link del dev bond0"));
    assert_eq!((master(&t, "enp3s0").as_deref(), master(&t, "enp4s0")), (Some("bond0"), None));
    t.e.confirm("tok", Origin::Web(ip("192.0.2.37"))).unwrap();
    assert!(t.e.state("admin", None).unwrap().pending.is_empty());
}

#[test]
fn the_reset_gesture_frees_the_members_of_a_bond() {
    let t = two_port_box();
    make_bond(&t);
    assert_eq!(t.e.reset_button(&log).unwrap(), Press::First);
    t.os.sleep(Duration::from_secs(3));
    assert_eq!(t.e.reset_button(&log).unwrap(), Press::Second);
    assert!(bond_mode(&t).is_none(), "bonds are suspended");
    assert_eq!((master(&t, "enp3s0"), master(&t, "enp4s0")), (None, None));
    for p in ["enp3s0", "enp4s0"] {
        let f = t.os.read_config(Path::new(&format!("/etc/systemd/network/05-rynas-{p}.network"))).unwrap();
        assert!(f.contains("DHCP=ipv4") && !f.contains("Bond="), "{f}");
    }
    assert_eq!(t.os.addrs("enp3s0"), vec!["192.0.2.37/24"]);
    assert_eq!(t.os.read_config(Path::new(DROPIN3)).as_deref(), Some(IDENTITY), "the identity follows");
}

#[test]
fn bond_watch_notifies_what_got_worse() {
    let t = two_port_box();
    make_bond(&t);
    t.e.take_events();
    t.e.watch_bonds();
    assert!(t.e.take_events().is_empty(), "the first look only learns");
    {
        let mut s = t.os.state.lock().unwrap();
        let l = s.links.get_mut("enp3s0").unwrap();
        l.carrier = false;
        l.failures = 1;
    }
    t.e.watch_bonds();
    let codes = event_codes(&t.e);
    assert_eq!(codes, vec!["net.bond_member_down", "net.bond_active_changed"]);
    t.e.watch_bonds();
    assert!(t.e.take_events().is_empty(), "nothing new");
}

#[test]
fn an_ifupdown_bond_is_migrated_with_its_mac() {
    let os = Arc::new(FakeOs::new());
    os.add_port("enp3s0", MAC3);
    os.add_port("enp4s0", MAC4);
    os.set_active("networking", true);
    {
        // ifenslave made the bond: the members show the bond's MAC
        let mut s = os.state.lock().unwrap();
        s.links.insert(
            "bond0".into(),
            crate::fake::FakeLink {
                index: 9,
                mac: MAC4.into(),
                mtu: 1500,
                max_mtu: 65535,
                carrier: true,
                bond_mode: Some("active-backup".into()),
                ..Default::default()
            },
        );
        for p in ["enp3s0", "enp4s0"] {
            s.links.get_mut(p).unwrap().master = Some("bond0".into());
        }
        s.leases.insert("bond0".into(), ("192.0.2.37/24".into(), "192.0.2.1".into()));
        s.reachable = vec!["192.0.2.1".into()];
    }
    os.add_addr("bond0", "192.0.2.37/24", true);
    os.write_config(
        "/etc/network/interfaces",
        "auto lo\niface lo inet loopback\n\nauto bond0\niface bond0 inet dhcp\n bond-slaves enp3s0 enp4s0\n\
         \x20bond-mode active-backup\n bond-miimon 100\n bond-primary enp4s0\n",
    );
    let state = tempfile::tempdir().unwrap();
    let e = engine(&os, state.path());
    let plan = e.plan_adopt("admin", ip("192.0.2.37"), None).unwrap();
    let b = plan.target.config["bond0"].bond.clone().unwrap();
    assert_eq!(b.mac.as_deref(), Some(MAC4), "the MAC the bond runs with");
    assert_eq!(b.primary.as_deref(), Some("enp4s0"));
    assert_eq!(plan.target.config.keys().collect::<Vec<_>>(), vec!["bond0"]);
    e.begin(&plan, Some("tok"), None, &log).unwrap();
    let s = os.state.lock().unwrap();
    assert_eq!(s.links["bond0"].bond_mode.as_deref(), Some("active-backup"));
    assert_eq!(s.links["enp3s0"].master.as_deref(), Some("bond0"));
}
