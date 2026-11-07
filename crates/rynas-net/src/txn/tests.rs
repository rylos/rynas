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
        "enslaved to bond0: bonds and bridges are configured in a later version"
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
