//! Fake roots (tempdirs): nothing touches the host's apt configuration or the network.

use super::*;

const OS13: &str = "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nVERSION_CODENAME=trixie\nID=debian\n";

/// The DS920+ layout: one-line Debian entries in sources.list, deb822 files of rynas and of
/// the system layer next to them.
const MAIN_LIST: &str = "deb http://deb.debian.org/debian/ trixie main non-free non-free-firmware\ndeb-src http://deb.debian.org/debian/ trixie main non-free non-free-firmware\n\ndeb http://security.debian.org/debian-security trixie-security main non-free non-free-firmware\ndeb-src http://security.debian.org/debian-security trixie-security main non-free non-free-firmware\n\n# trixie-updates, to get updates before a point release is made;\ndeb http://deb.debian.org/debian/ trixie-updates main non-free non-free-firmware\ndeb-src http://deb.debian.org/debian/ trixie-updates main non-free non-free-firmware\n";
const DOCKER: &str = "# Managed by rynas: Docker's repository (docker-ce)\nTypes: deb\nURIs: https://download.docker.com/linux/debian\nSuites: trixie\nComponents: stable\nArchitectures: amd64\nSigned-By: /etc/apt/keyrings/docker.asc\n";
const SYNODEB: &str = "Types: deb\nURIs: https://example.github.io/synodeb/debian\nSuites: trixie\nComponents: main\nSigned-By: /usr/share/keyrings/synodeb.gpg\n";
const VENDOR: &str = "Types: deb\nURIs: https://pkgs.vendor.example/apt\nSuites: stable\nComponents: main\nSigned-By: /etc/apt/keyrings/vendor.asc\n";

fn root(files: &[(&str, &str)]) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("etc/apt/sources.list.d")).unwrap();
    std::fs::write(d.path().join("etc/os-release"), OS13).unwrap();
    for (p, t) in files {
        let p = d.path().join(p.trim_start_matches('/'));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, t).unwrap();
    }
    d
}

fn ds920() -> tempfile::TempDir {
    root(&[
        ("/etc/apt/sources.list", MAIN_LIST),
        ("/etc/apt/sources.list.d/docker.sources", DOCKER),
        ("/etc/apt/sources.list.d/synodeb.sources", SYNODEB),
        ("/var/lib/dpkg/info/synodeb.conffiles", "/etc/apt/sources.list.d/synodeb.sources\n/etc/synodeb/x.conf\n"),
    ])
}

fn codes(st: &RepositoriesState) -> Vec<&str> {
    st.infos.iter().map(|i| i.code.as_str()).collect()
}

fn st(d: &tempfile::TempDir, layer: bool) -> RepositoriesState {
    state(d.path(), &d.path().join("state"), layer)
}

#[test]
fn classification() {
    let d = ds920();
    std::fs::write(d.path().join("etc/apt/sources.list.d/vendor.sources"), VENDOR).unwrap();
    std::fs::write(d.path().join("etc/apt/sources.list.d/ppa.list"), "deb [signed-by=/etc/apt/keyrings/p.gpg] https://ppa.launchpadcontent.net/x/y/ubuntu noble main\n").unwrap();
    // with the system layer
    let s = st(&d, true);
    let kind = |name: &str| s.files.iter().find(|f| f.path.ends_with(name)).unwrap().entries[0].kind;
    assert_eq!(kind("sources.list"), RepoKind::Debian);
    assert_eq!(kind("docker.sources"), RepoKind::Rynas);
    assert_eq!(kind("synodeb.sources"), RepoKind::SystemLayer);
    assert_eq!(kind("vendor.sources"), RepoKind::ThirdParty);
    assert_eq!(kind("ppa.list"), RepoKind::ThirdParty);
    let f = |name: &str| s.files.iter().find(|f| f.path.ends_with(name)).unwrap().clone();
    assert_eq!(f("sources.list").entries[0].read_only, Some(ReadOnly::DebianOnSystemLayer));
    assert_eq!(f("docker.sources").entries[0].manage_url.as_deref(), Some("/docker"));
    assert_eq!(f("synodeb.sources").package.as_deref(), Some("synodeb"));
    assert!(f("vendor.sources").removable && !f("synodeb.sources").removable && !f("docker.sources").removable);
    assert!(!s.official.manageable);
    assert_eq!(s.official.reason.as_deref(), Some("system_layer"));
    assert!(codes(&s).contains(&"other_distro"));
    assert!(s.unsupported.iter().any(|u| u == "pkgs.vendor.example"), "{:?}", s.unsupported);
    assert_eq!(s.status, RepoStatus::Critical);
    // without it: Debian writable, synodeb a third party (no hardware support claims it)
    let s = st(&d, false);
    assert!(s.official.manageable);
    let syn = s.files.iter().find(|f| f.path.ends_with("synodeb.sources")).unwrap();
    assert_eq!(syn.entries[0].kind, RepoKind::ThirdParty);
    assert!(!syn.removable, "a package's file is never removed here");
    // a hand-written docker.sources (no rynas marker) is the vendor's
    std::fs::write(d.path().join("etc/apt/sources.list.d/docker.sources"), DOCKER.replace("# Managed by rynas: ", "# ")).unwrap();
    assert_eq!(st(&d, false).files.iter().find(|f| f.path.ends_with("docker.sources")).unwrap().entries[0].kind, RepoKind::ThirdParty);
}

#[test]
fn origin_from_the_cached_release() {
    let d = root(&[
        ("/etc/apt/sources.list.d/m.list", "deb http://mirror.example.net/debian trixie main\ndeb https://repo.example.com/apt ./\n"),
        (
            "/var/lib/apt/lists/mirror.example.net_debian_dists_trixie_InRelease",
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA512\n\nOrigin: Debian\nLabel: Debian\nSuite: stable\nCodename: trixie\n\nMD5Sum:\n-----BEGIN PGP SIGNATURE-----\n",
        ),
        ("/var/lib/apt/lists/repo.example.com_apt_._Release", "Origin: Example\nLabel: Example\n"),
    ]);
    let s = st(&d, false);
    let e = &s.files[0].entries;
    assert_eq!(e[0].kind, RepoKind::Debian);
    assert_eq!(e[0].origin.as_deref(), Some("Debian"));
    assert!(e[0].last_fetch_ms.is_some());
    assert_eq!(e[1].origin.as_deref(), Some("Example"));
    assert_eq!(e[1].kind, RepoKind::ThirdParty);
    assert_eq!(classify::uri_to_file_name("https://u:p@x.example/a_b/dists/s/"), "x.example_a%5fb_dists_s_");
}

#[test]
fn checks() {
    // the DS920+ layout with its system layer: all good, supported
    let d = ds920();
    let s = st(&d, true);
    assert_eq!(s.status, RepoStatus::Good, "{:?}", s.infos);
    assert!(s.unsupported.is_empty());
    let one = |list: &str| {
        let d = root(&[("/etc/apt/sources.list", list)]);
        st(&d, false)
    };
    let s = one("deb http://deb.debian.org/debian trixie-updates main\n");
    assert!(codes(&s).contains(&"no_debian"));
    assert_eq!(s.status, RepoStatus::Critical);
    let s = one("deb http://deb.debian.org/debian trixie main\n");
    assert!(codes(&s).contains(&"no_security") && codes(&s).contains(&"no_updates"));
    assert_eq!(s.status, RepoStatus::Warning);
    let s = one("deb http://deb.debian.org/debian trixie main\ndeb http://deb.debian.org/debian stable main\ndeb http://deb.debian.org/debian bookworm main\ndeb http://deb.debian.org/debian sid main\ndeb http://deb.debian.org/debian forky main\ndeb http://security.debian.org/debian-security trixie/updates main\ndeb http://deb.debian.org/debian trixie-security main non-free\n");
    let c = codes(&s);
    for k in ["use_codename", "old_suite", "testing", "next_release", "security_suite", "security_uri", "non_free_firmware", "mixed_suites"] {
        assert!(c.contains(&k), "{k}: {c:?}");
    }
    assert!(s.unsupported.contains(&"Debian sid".to_string()));
    // the next release alone is not unsupported (an upgrade in progress)
    assert!(!s.unsupported.iter().any(|u| u.contains("forky")));
    let s = one("deb http://deb.debian.org/debian trixie main\ndeb http://deb.debian.org/debian duke main\n");
    assert!(s.unsupported.contains(&"Debian duke".to_string()));
    // insecure, unsigned third party, duplicates
    let s = one("deb http://deb.debian.org/debian trixie main\ndeb http://deb.debian.org/debian/ trixie main\ndeb [trusted=yes] http://x.example/apt ./\ndeb http://y.example/apt stable main\n# deb http://z.example/apt stable main\n");
    let c = codes(&s);
    assert!(c.contains(&"insecure") && c.contains(&"no_signed_by") && c.contains(&"duplicate"), "{c:?}");
    assert!(!s.unsupported.iter().any(|u| u.contains("z.example")), "a disabled entry is not unsupported");
    // legacy keys (Debian's own fragments are fine)
    let d = root(&[
        ("/etc/apt/sources.list", MAIN_LIST),
        ("/etc/apt/trusted.gpg.d/debian-archive-trixie-stable.asc", "k"),
        ("/etc/apt/trusted.gpg.d/vendor.gpg", "k"),
    ]);
    let s = st(&d, false);
    assert_eq!(s.infos.iter().filter(|i| i.code == "legacy_keyring").count(), 1);
    // a file that does not parse: critical, the rest read
    let d = root(&[("/etc/apt/sources.list", MAIN_LIST), ("/etc/apt/sources.list.d/bad.list", "deb http://x\n")]);
    let s = st(&d, false);
    assert!(codes(&s).contains(&"parse_error"));
    assert!(s.files.iter().find(|f| f.path.ends_with("sources.list")).unwrap().entries.len() == 6);
}

#[test]
fn fetch_errors_on_entries() {
    let d = root(&[("/etc/apt/sources.list", MAIN_LIST), ("/etc/apt/sources.list.d/vendor.sources", VENDOR)]);
    let out = "Err:3 https://pkgs.vendor.example/apt stable InRelease\n  The following signatures were invalid: BADSIG 1111222233334444 Vendor\nW: GPG error: https://pkgs.vendor.example/apt stable InRelease: The following signatures were invalid: BADSIG 1111222233334444 Vendor\n";
    save_check(&d.path().join("state"), &RepoCheck { at_ms: 1, ok: false, errors: aptlog::parse(out) });
    let s = st(&d, false);
    let v = s.files.iter().find(|f| f.path.ends_with("vendor.sources")).unwrap();
    assert_eq!(v.entries[0].errors.len(), 1);
    assert_eq!(v.entries[0].errors[0].kind, FetchErrorKind::BadSignature);
    assert!(s.infos.iter().any(|i| i.code == "key_changed" && i.level == RepoLevel::Critical));
    // a Debian entry is never auto-disabled: only shown
    assert!(s.files[0].entries.iter().all(|e| e.enabled));
}

#[test]
fn toggle_digest_and_last_entry() {
    let d = ds920();
    let v = View::read(d.path(), false);
    let req = |path: &str, index: usize, enabled: bool| ToggleRequest { digest: v.scan.digest.clone(), path: path.into(), index, enabled };
    // stale digest
    let mut r = req("/etc/apt/sources.list", 2, false);
    r.digest = "0".repeat(64);
    assert_eq!(plan_toggle(&v, &r).unwrap_err().code(), error_codes::CONFLICT);
    // the only main suite entry: refused
    assert_eq!(plan_toggle(&v, &req("/etc/apt/sources.list", 0, false)).unwrap_err().code(), error_codes::LAST_DEBIAN_ENTRY);
    // rynas's docker file: read-only here
    assert_eq!(plan_toggle(&v, &req("/etc/apt/sources.list.d/docker.sources", 0, false)).unwrap_err().code(), error_codes::READ_ONLY);
    // the security line: commented, comments and other lines kept
    let c = plan_toggle(&v, &req("/etc/apt/sources.list", 2, false)).unwrap();
    let Action::Write(t) = &c[0].action else { panic!() };
    assert!(t.contains("\n# deb http://security.debian.org/debian-security trixie-security"), "{t}");
    assert!(t.contains("# trixie-updates, to get updates before a point release is made;\n"));
    apply(d.path(), &d.path().join("state"), &c, &|_| {}).unwrap();
    let after = st(&d, false);
    assert!(!after.files[0].entries[2].enabled);
    assert!(codes(&after).contains(&"no_security"));
    assert_ne!(after.digest, v.scan.digest);
    // the backup holds the old file
    let bk = std::fs::read_dir(d.path().join("state/apt-backup")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(std::fs::read_to_string(bk.join("etc/apt/sources.list")).unwrap(), MAIN_LIST);
    // on the system layer the Debian entries are read-only
    let v = View::read(d.path(), true);
    let r = ToggleRequest { digest: v.scan.digest.clone(), path: "/etc/apt/sources.list".into(), index: 2, enabled: true };
    assert_eq!(plan_toggle(&v, &r).unwrap_err().code(), error_codes::READ_ONLY);
}

fn official_req(digest: &str) -> OfficialRequest {
    OfficialRequest {
        digest: digest.into(),
        mirror: "http://deb.debian.org/debian".into(),
        security: true,
        updates: true,
        backports: false,
        proposed: false,
        components: vec!["main".into(), "non-free".into(), "non-free-firmware".into()],
        sources: false,
        preview: false,
    }
}

#[test]
fn official_rewrite_into_debian_sources() {
    let d = root(&[
        ("/etc/apt/sources.list", MAIN_LIST),
        ("/etc/apt/sources.list.d/vendor.sources", VENDOR),
        ("/etc/apt/keyrings/vendor.asc", "k"),
        ("/usr/share/keyrings/debian-archive-keyring.pgp", "k"),
    ]);
    let s = st(&d, false);
    assert_eq!(s.official.components, ["main", "non-free", "non-free-firmware"]);
    assert!(s.official.security && s.official.updates && s.official.sources && !s.official.backports);
    let v = View::read(d.path(), false);
    let mut req = official_req(&s.digest);
    req.backports = true;
    req.components.insert(1, "contrib".into());
    let plan = plan_official(d.path(), &v, &req).unwrap();
    let paths: Vec<(&str, &Action)> = plan.iter().map(|c| (c.path.as_str(), &c.action)).collect();
    assert!(matches!(paths[0], ("/etc/apt/sources.list.d/debian.sources", Action::Write(_))));
    assert!(matches!(paths[1], ("/etc/apt/sources.list.d/rynas-backports.sources", Action::Write(_))));
    assert_eq!(paths[2], ("/etc/apt/sources.list", &Action::Rename("/etc/apt/sources.list.bak".into())));
    assert_eq!(plan.len(), 3, "the vendor file is not touched");
    let Action::Write(deb) = &plan[0].action else { panic!() };
    assert!(deb.contains("Types: deb\nURIs: http://deb.debian.org/debian\nSuites: trixie trixie-updates\nComponents: main contrib non-free non-free-firmware\nSigned-By: /usr/share/keyrings/debian-archive-keyring.pgp\n"), "{deb}");
    assert!(deb.contains("URIs: http://deb.debian.org/debian-security\nSuites: trixie-security\n"), "{deb}");
    let Action::Write(bp) = &plan[1].action else { panic!() };
    assert!(bp.contains("Suites: trixie-backports\nComponents: main contrib non-free non-free-firmware\n"), "{bp}");
    // the preview names them
    let p = preview_of(d.path(), &plan);
    assert_eq!(p.changes[2].action, "rename");
    apply(d.path(), &d.path().join("state"), &plan, &|_| {}).unwrap();
    let after = st(&d, false);
    assert!(after.official.by_rynas && after.official.backports && !after.official.backports_by_system);
    assert_eq!(after.official.components, ["main", "contrib", "non-free", "non-free-firmware"]);
    assert_eq!(after.status, RepoStatus::Good, "{:?}", after.infos);
    assert!(d.path().join("etc/apt/sources.list.bak").exists());
    // the backports wrapper sees rynas's file
    let bp = crate::backports::state(d.path(), false);
    assert!(bp.by_rynas && !bp.by_system);
    // never zero Debian entries: main cannot be dropped, components need main
    let v = View::read(d.path(), false);
    let mut bad = official_req(&v.scan.digest);
    bad.components = vec!["contrib".into()];
    assert_eq!(plan_official(d.path(), &v, &bad).unwrap_err().code(), error_codes::INVALID_VALUE);
    bad.components = vec!["main".into(), "universe".into()];
    assert!(plan_official(d.path(), &v, &bad).is_err());
    // backports off again: rynas's file removed
    let mut off = official_req(&v.scan.digest);
    off.components.insert(1, "contrib".into());
    let plan = plan_official(d.path(), &v, &off).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].action, Action::Remove);
    // on the system layer: refused
    let v = View::read(d.path(), true);
    assert_eq!(plan_official(d.path(), &v, &official_req(&v.scan.digest)).unwrap_err().code(), error_codes::NOT_AVAILABLE);
}

#[test]
fn official_rewrite_keeps_mixed_files() {
    // Debian and a vendor in one file: only the Debian lines are commented
    let list = format!("{MAIN_LIST}deb [signed-by=/etc/apt/keyrings/v.asc] https://pkgs.vendor.example/apt stable main\n");
    let d = root(&[("/etc/apt/sources.list", &list)]);
    let v = View::read(d.path(), false);
    let plan = plan_official(d.path(), &v, &official_req(&v.scan.digest)).unwrap();
    let edit = plan.iter().find(|c| c.path == "/etc/apt/sources.list").unwrap();
    let Action::Write(t) = &edit.action else { panic!("{edit:?}") };
    assert_eq!(t.lines().filter(|l| l.starts_with("# deb")).count(), 6, "{t}");
    assert!(t.contains("\ndeb [signed-by=/etc/apt/keyrings/v.asc] https://pkgs.vendor.example/apt stable main\n"));
    // an existing debian.sources with a foreign stanza: the stanza stays
    let deb = format!("Types: deb\nURIs: http://deb.debian.org/debian\nSuites: trixie\nComponents: main\n\n{VENDOR}");
    let d = root(&[("/etc/apt/sources.list.d/debian.sources", &deb)]);
    let v = View::read(d.path(), false);
    let plan = plan_official(d.path(), &v, &official_req(&v.scan.digest)).unwrap();
    let Action::Write(t) = &plan[0].action else { panic!() };
    assert!(t.ends_with(VENDOR), "{t}");
    assert!(t.matches("Suites: trixie").count() >= 2);
}

#[test]
fn modernize_converts_one_line_files() {
    let d = root(&[
        ("/etc/apt/sources.list", MAIN_LIST),
        ("/etc/apt/sources.list.d/vendor.list", "# the vendor\ndeb [arch=amd64,arm64 signed-by=/etc/apt/keyrings/v.asc] https://pkgs.vendor.example/apt stable main\n# deb https://pkgs.vendor.example/apt beta main\n"),
        ("/usr/share/keyrings/debian-archive-keyring.pgp", "k"),
    ]);
    let v = View::read(d.path(), false);
    let s = st(&d, false);
    assert_eq!(s.convertible, ["/etc/apt/sources.list", "/etc/apt/sources.list.d/vendor.list"]);
    let plan = plan_modernize(d.path(), &v, &ModernizeRequest { digest: v.scan.digest.clone(), preview: true }).unwrap();
    assert_eq!(plan.len(), 4);
    let Action::Write(deb) = &plan[0].action else { panic!() };
    assert_eq!(plan[0].path, "/etc/apt/sources.list.d/debian.sources");
    // deb + deb-src of the same suites together; Debian's keyring added
    assert!(deb.contains("Types: deb deb-src\nURIs: http://deb.debian.org/debian/\nSuites: trixie trixie-updates\nComponents: main non-free non-free-firmware\nSigned-By: /usr/share/keyrings/debian-archive-keyring.pgp\n"), "{deb}");
    let Action::Write(ven) = &plan[2].action else { panic!() };
    assert_eq!(plan[2].path, "/etc/apt/sources.list.d/vendor.sources");
    assert!(ven.contains("Architectures: amd64 arm64\nSigned-By: /etc/apt/keyrings/v.asc\n"), "{ven}");
    assert!(ven.contains("Suites: beta\nComponents: main\nEnabled: no\n"), "{ven}");
    apply(d.path(), &d.path().join("state"), &plan, &|_| {}).unwrap();
    let after = View::read(d.path(), false);
    let n = |v: &View| v.scan.files.iter().flat_map(|f| f.entries.iter()).map(|e| e.types.len() * e.suites.len()).sum::<usize>();
    assert_eq!(n(&after), n(&v), "the same targets as before");
    assert!(after.scan.files.iter().all(|f| f.format == RepoFormat::Deb822 && f.error.is_none()));
    assert!(after.scan.ignored.iter().any(|i| i.path == "/etc/apt/sources.list.d/vendor.list.bak"));
    assert!(st(&d, false).convertible.is_empty());
}

#[test]
fn add_and_rollback() {
    let d = root(&[("/etc/apt/sources.list", MAIN_LIST)]);
    let v = View::read(d.path(), false);
    let ts = include_bytes!("../../../rynas-tailscale/testdata/tailscale-archive-keyring.gpg");
    let (keys, armored) = keys::preview(ts, 0).unwrap();
    let mut req = AddRepoRequest {
        digest: v.scan.digest.clone(),
        name: "vendor".into(),
        uris: vec!["https://pkgs.vendor.example/apt".into()],
        suites: vec!["stable".into()],
        components: vec!["main".into()],
        architectures: vec![],
        sources: false,
        key: armored,
        fingerprints: vec![keys[0].fingerprint.to_lowercase()],
    };
    let (plan, entry) = plan_add(d.path(), &v, &req).unwrap();
    assert_eq!(plan[0].path, "/etc/apt/keyrings/vendor.asc");
    let Action::Write(src) = &plan[1].action else { panic!() };
    assert!(src.contains(&format!("Architectures: {}\nSigned-By: /etc/apt/keyrings/vendor.asc\n", dpkg_arch())), "{src}");
    assert!(!src.to_ascii_lowercase().contains("trusted"));
    // refusals
    let bad = |f: &dyn Fn(&mut AddRepoRequest)| {
        let mut r = req.clone();
        f(&mut r);
        plan_add(d.path(), &v, &r).unwrap_err().code()
    };
    assert_eq!(bad(&|r| r.fingerprints = vec!["00".into()]), error_codes::KEY_INVALID);
    assert_eq!(bad(&|r| r.key = "junk".into()), error_codes::KEY_INVALID);
    assert_eq!(bad(&|r| r.uris = vec!["file:/srv/repo".into()]), error_codes::INVALID_VALUE);
    assert_eq!(bad(&|r| r.uris = vec!["http://ppa.launchpadcontent.net/x/y/ubuntu".into()]), error_codes::INVALID_VALUE);
    assert_eq!(bad(&|r| r.name = "debian".into()), error_codes::INVALID_VALUE);
    assert_eq!(bad(&|r| r.suites = vec!["./".into()]), error_codes::INVALID_VALUE);
    assert_eq!(bad(&|r| r.digest = "1".repeat(64)), error_codes::CONFLICT);
    // written, then apt-get update fails for it: removed again
    apply(d.path(), &d.path().join("state"), &plan, &|_| {}).unwrap();
    assert!(d.path().join("etc/apt/sources.list.d/vendor.sources").exists());
    let out = "Err:4 https://pkgs.vendor.example/apt stable InRelease\n  The following signatures couldn't be verified because the public key is not available: NO_PUBKEY 0123456789ABCDEF\nE: The repository 'https://pkgs.vendor.example/apt stable InRelease' is not signed.\n";
    let check = RepoCheck { at_ms: 1, ok: false, errors: aptlog::parse(out) };
    let failed = new_source_failures(&entry, &check);
    assert_eq!(failed.len(), 1);
    rollback(d.path(), &plan);
    assert!(!d.path().join("etc/apt/sources.list.d/vendor.sources").exists());
    assert!(!d.path().join("etc/apt/keyrings/vendor.asc").exists());
    // an unrelated failure does not remove it
    let other = RepoCheck { at_ms: 1, ok: false, errors: aptlog::parse("W: Failed to fetch http://bad.example/debian/dists/x/InRelease  Temporary failure resolving 'bad.example'\n") };
    assert!(new_source_failures(&entry, &other).is_empty());
    // an exact path
    req.suites = vec!["./".into()];
    req.components.clear();
    let (plan, _) = plan_add(d.path(), &v, &req).unwrap();
    let Action::Write(src) = &plan[1].action else { panic!() };
    assert!(src.contains("Suites: ./\nArchitectures:") && !src.contains("Components"), "{src}");
}

#[test]
fn remove_third_party_with_its_key() {
    let d = root(&[
        ("/etc/apt/sources.list", MAIN_LIST),
        ("/etc/apt/sources.list.d/vendor.sources", VENDOR),
        ("/etc/apt/keyrings/vendor.asc", "k"),
        ("/etc/apt/sources.list.d/docker.sources", DOCKER),
    ]);
    let v = View::read(d.path(), false);
    let plan = plan_remove(d.path(), &v, "vendor.sources", &v.scan.digest).unwrap();
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[1].path, "/etc/apt/keyrings/vendor.asc");
    assert_eq!(plan_remove(d.path(), &v, "docker.sources", &v.scan.digest).unwrap_err().code(), error_codes::READ_ONLY);
    assert_eq!(plan_remove(d.path(), &v, "nope.sources", &v.scan.digest).unwrap_err().code(), error_codes::NOT_FOUND);
    apply(d.path(), &d.path().join("state"), &plan, &|_| {}).unwrap();
    assert!(!d.path().join("etc/apt/keyrings/vendor.asc").exists());
    assert!(st(&d, false).unsupported.is_empty());
    assert_eq!(query_digest(Some(format!("digest={}", "a".repeat(64)))).unwrap().len(), 64);
    assert!(query_digest(Some("digest=x".into())).is_err());
}

#[test]
fn cdrom_lines_are_one_disabled_entry() {
    let d = root(&[("/etc/apt/sources.list", &format!("# deb cdrom:[Debian GNU/Linux 13.1.0 _Trixie_ - Official amd64 DVD Binary-1 with firmware 20250906-10:24]/ trixie contrib main non-free-firmware\n\n{MAIN_LIST}"))]);
    let s = st(&d, false);
    let e = &s.files[0].entries[0];
    assert!(!e.enabled);
    assert!(e.uris[0].starts_with("cdrom:[Debian GNU/Linux 13.1.0"));
    assert_eq!(e.suites, ["trixie"]);
    assert_eq!(s.status, RepoStatus::Good, "{:?}", s.infos);
}

/// The UPS flow and the old switch keep working through `/v1/updates/backports`.
#[test]
fn backports_wrapper_unchanged() {
    let d = root(&[("/etc/apt/sources.list", MAIN_LIST)]);
    let bp = crate::backports::state(d.path(), false);
    assert!(bp.available && !bp.enabled);
    std::fs::write(
        d.path().join("etc/apt/sources.list.d/rynas-backports.sources"),
        crate::backports::render("trixie", &crate::backports::read_sources(d.path())),
    )
    .unwrap();
    let s = st(&d, false);
    let f = s.files.iter().find(|f| f.path.ends_with("rynas-backports.sources")).unwrap();
    assert_eq!(f.entries[0].kind, RepoKind::Rynas);
    assert_eq!(f.entries[0].manage_url.as_deref(), Some("/system/repositories#debian"));
    assert!(s.official.backports && !s.official.backports_by_system);
    assert!(crate::backports::state(d.path(), false).by_rynas);
}
