//! Both domains in a real configuration store, on a scratch root, with the in-memory
//! system, accounts and filesystem.
//!
//! Golden files live in `src/golden/`; regenerate them with
//! `RYNAS_UPDATE_GOLDEN=1 cargo test -p rynas-services`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rynas_api_types::{
    Access, LocalGroup, LocalUser, NfsRule, NfsSquash, Permission, PrincipalKind,
    ShadowCopySettings, Share, ShareNfs, ShareSmb, SharesConfig, ShellAccess, SmbPreset,
    StateDriftKind, UsersConfig,
};
use rynas_config::{Config, ConfigError, ConfigOptions, MemorySystem};
use tempfile::TempDir;

use crate::accounts::{MemoryAccounts, Secret, SysUser};
use crate::host::MemoryShares;
use crate::users::set_password;
use crate::{SHARES_KEY, SharesDomain, USERS_KEY, UsersDomain};

struct Env {
    _dir: TempDir,
    root: PathBuf,
    sys: Arc<MemorySystem>,
    acc: Arc<MemoryAccounts>,
    host: Arc<MemoryShares>,
    creator: Arc<TestCreator>,
    removed: Arc<crate::removed::RemovedShares>,
    cfg: Config,
}

/// Volume "vol1" mounted at /srv/vol1; created subvolumes appear in the memory filesystem.
struct TestCreator {
    host: Arc<MemoryShares>,
    calls: Mutex<Vec<String>>,
}

impl crate::SubvolumeCreator for TestCreator {
    fn volume_path(&self, volume: &str) -> Result<String, String> {
        match volume {
            "vol1" => Ok("/srv/vol1".into()),
            v => Err(format!("volume {v} is not mounted")),
        }
    }

    fn create(&self, path: &str, compression: bool, _log: &dyn Fn(&str)) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{path} compression={compression}"));
        self.host
            .add_dir(path, Some("99999999-2222-4333-8444-555555555555"));
        Ok(())
    }

    fn volumes(&self) -> Vec<(String, String)> {
        vec![("vol1".into(), "/srv/vol1".into())]
    }
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc/samba")).unwrap();
    std::fs::write(
        root.join("etc/samba/smb.conf"),
        "# Debian default\n[global]\n",
    )
    .unwrap();
    let sys = Arc::new(MemorySystem::new("nas"));
    let acc = Arc::new(MemoryAccounts::new());
    let host = Arc::new(MemoryShares::new());
    for (p, uuid) in [
        (
            "/srv/vol1/docs",
            Some("11111111-2222-4333-8444-555555555555"),
        ),
        (
            "/srv/vol1/media",
            Some("66666666-7777-4888-9999-aaaaaaaaaaaa"),
        ),
        ("/srv/vol1/tm", None),
        ("/srv/vol1/scans", None),
    ] {
        host.add_dir(p, uuid);
    }
    let creator = Arc::new(TestCreator {
        host: host.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let removed = Arc::new(crate::removed::RemovedShares::memory());
    let cfg = Config::open(
        ConfigOptions::new(dir.path().join("state"), &root),
        sys.clone(),
        vec![
            Arc::new(UsersDomain::new(acc.clone())),
            Arc::new(
                SharesDomain::new(host.clone(), acc.clone())
                    .with_subvolumes(creator.clone())
                    .with_removed(removed.clone()),
            ),
        ],
    )
    .unwrap();
    Env {
        _dir: dir,
        root,
        sys,
        acc,
        host,
        creator,
        removed,
        cfg,
    }

}

fn apply(e: &Env) -> Result<u64, ConfigError> {
    let lines = Mutex::new(Vec::<String>::new());
    let r = e.cfg.apply(
        "admin",
        "admin",
        "test",
        &|l| lines.lock().unwrap().push(l.into()),
        &|| false,
    );
    if r.is_err() {
        eprintln!("apply log:\n{}", lines.lock().unwrap().join("\n"));
    }
    r
}

fn read(root: &Path, p: &str) -> String {
    std::fs::read_to_string(root.join(p.trim_start_matches('/'))).unwrap()
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/golden")
        .join(name);
    if std::env::var_os("RYNAS_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing golden file {name}: run with RYNAS_UPDATE_GOLDEN=1"));
    assert_eq!(actual, want, "golden file {name} differs");
}

fn perm(kind: PrincipalKind, name: &str, access: Access) -> Permission {
    Permission {
        kind,
        name: name.into(),
        access,
    }
}

fn user(name: &str, groups: &[&str]) -> LocalUser {
    LocalUser {
        name: name.into(),
        full_name: format!("{} Test", name.to_uppercase()),
        groups: groups.iter().map(|g| g.to_string()).collect(),
        ..Default::default()
    }
}

fn group(name: &str) -> LocalGroup {
    LocalGroup {
        name: name.into(),
        gid: 0,
        description: format!("{name} group"),
    }
}

fn users_doc() -> UsersConfig {
    UsersConfig {
        users: vec![
            LocalUser {
                admin: true,
                shell: ShellAccess::Shell,
                ..user("alice", &["office"])
            },
            user("bob", &["office", "interns"]),
            LocalUser {
                shell: ShellAccess::Sftp,
                expires: Some("2030-01-31".into()),
                ..user("carol", &[])
            },
        ],
        groups: vec![group("office"), group("interns")],
        remove_homes: vec![],
    }
}

/// One share per preset; docs also over NFS.
fn shares_doc() -> SharesConfig {
    SharesConfig {
        shares: vec![
            Share {
                name: "docs".into(),
                path: "/srv/vol1/docs".into(),
                description: "Office documents".into(),
                smb: ShareSmb {
                    preset: SmbPreset::Multiprotocol,
                    shadow_copy: ShadowCopySettings {
                        enabled: true,
                        snapshot_dir: "/srv/vol1/.snapshots".into(),
                        prefix: String::new(),
                    },
                    ..Default::default()
                },
                nfs: ShareNfs {
                    enabled: true,
                    rules: vec![
                        NfsRule {
                            client: "192.168.1.0/24".into(),
                            ..Default::default()
                        },
                        NfsRule {
                            client: "backup.lan".into(),
                            read_only: true,
                            squash: NfsSquash::All,
                            anonuid: 1001,
                            anongid: 100,
                            sync: true,
                        },
                    ],
                    fsid: String::new(),
                    crossmnt: false,
                },
                permissions: vec![
                    perm(PrincipalKind::Group, "office", Access::Rw),
                    perm(PrincipalKind::User, "carol", Access::Ro),
                    perm(PrincipalKind::Group, "interns", Access::Deny),
                ],
                subvolume: None,
                show_snapshots: false,
            },
            Share {
                name: "media".into(),
                path: "/srv/vol1/media".into(),
                smb: ShareSmb {
                    preset: SmbPreset::Standard,
                    encryption_required: true,
                    extra: "strict sync = yes\n".into(),
                    ..Default::default()
                },
                permissions: vec![perm(PrincipalKind::Group, "office", Access::Ro)],
                ..Default::default()
            },
            Share {
                name: "TimeMachine".into(),
                path: "/srv/vol1/tm".into(),
                smb: ShareSmb {
                    preset: SmbPreset::Timemachine,
                    timemachine_max_size: "1t".into(),
                    recycle: rynas_api_types::RecycleSettings {
                        enabled: false,
                        retention_days: 0,
                    },
                    ..Default::default()
                },
                permissions: vec![perm(PrincipalKind::User, "alice", Access::Rw)],
                ..Default::default()
            },
            Share {
                name: "scans".into(),
                path: "/srv/vol1/scans".into(),
                smb: ShareSmb {
                    preset: SmbPreset::Hugefolders,
                    browseable: false,
                    ..Default::default()
                },
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

fn stage<T: serde::Serialize>(e: &Env, key: &str, v: &T) -> Result<bool, ConfigError> {
    e.cfg
        .stage("admin", key, Some(serde_json::to_value(v).unwrap()))
}

#[test]
fn users_and_shares_render_golden_files_and_acls() {
    let e = env();
    stage(&e, USERS_KEY, &users_doc()).unwrap();
    stage(&e, SHARES_KEY, &shares_doc()).unwrap();
    // the docs share is a subvolume: its UUID became the fsid
    let staged: SharesConfig = serde_json::from_value(
        e.cfg
            .pending_value("admin", SHARES_KEY)
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        staged.shares[0].nfs.fsid,
        "11111111-2222-4333-8444-555555555555"
    );
    assert_eq!(staged.shares[3].smb.timemachine_max_size, "1T");
    apply(&e).unwrap();

    golden("smb.conf", &read(&e.root, "/etc/samba/smb.conf"));
    golden(
        "rynas.exports",
        &read(&e.root, "/etc/exports.d/rynas.exports"),
    );
    golden("nfs.conf", &read(&e.root, "/etc/nfs.conf.d/rynas.conf"));
    golden(
        "rynas-smb.service",
        &read(&e.root, "/etc/avahi/services/rynas-smb.service"),
    );

    // accounts: groups before users, the special groups created on demand
    let log = e.acc.state().log.clone();
    let pos = |s: &str| {
        log.iter()
            .position(|l| l.starts_with(s))
            .unwrap_or_else(|| panic!("{s} not in {log:#?}"))
    };
    assert!(pos("groupadd rynas-admin") < pos("useradd alice"));
    assert!(pos("groupadd office (gid 1000)") < pos("useradd alice (uid 1000)"));
    assert!(pos("useradd carol") < pos("gpasswd: add carol to rynas-sftp"));
    assert!(log.contains(&"gpasswd: add alice to rynas-ssh".to_string()));
    let carol = e
        .acc
        .state()
        .users
        .iter()
        .find(|u| u.name == "carol")
        .cloned()
        .unwrap();
    assert_eq!(carol.shell, crate::users::NOLOGIN);
    assert_eq!(
        carol.expire_days,
        crate::validate::date_to_days("2030-01-31")
    );

    // ACLs: deny of interns expanded to bob, carol RO, office RW, alice in office
    let acls = e.host.state().acls.clone();
    let docs = &acls["/srv/vol1/docs"];
    for l in [
        "user:bob:---",
        "user:carol:r-x",
        "group:office:rwx",
        "group:interns:---",
        "default:group:office:rwx",
        "other::---",
    ] {
        assert!(docs.contains(l), "{l} missing in {docs:?}");
    }
    assert!(
        !acls.contains_key("/srv/vol1/scans"),
        "no matrix: ACL untouched"
    );

    // services: smbd enabled and reloaded, wsdd2 on, NFS exported
    let calls = e.sys.state().calls.clone();
    for c in [
        "run testparm -s",
        "run systemctl enable --now smbd.service",
        "reload smbd.service",
        "run systemctl enable --now wsdd2.service",
        "run systemctl enable --now nfs-server.service",
        "run exportfs -ra",
    ] {
        assert!(
            calls.iter().any(|x| x.starts_with(c)),
            "{c} not in {calls:#?}"
        );
    }
    assert!(
        !calls.iter().any(|x| x.contains("nmbd")),
        "NetBIOS stays off"
    );

    // no drift right after the apply
    let d = e.cfg.drift().unwrap();
    assert!(d.entries.is_empty(), "{:?}", d.entries);
    let state: Vec<_> = d
        .state
        .iter()
        .filter(|s| s.kind != StateDriftKind::MissingInSamba)
        .collect();
    assert!(state.is_empty(), "{state:?}");
    // SMB users without a password yet are reported
    assert_eq!(
        d.state
            .iter()
            .filter(|s| s.kind == StateDriftKind::MissingInSamba)
            .count(),
        3
    );
}

#[test]
fn testparm_failure_aborts_and_keeps_the_files() {
    let e = env();
    stage(&e, USERS_KEY, &users_doc()).unwrap();
    stage(&e, SHARES_KEY, &shares_doc()).unwrap();
    e.sys.state().fail_programs.push("testparm".into());
    let err = apply(&e).unwrap_err();
    assert!(err.to_string().contains("validation failed"), "{err}");
    assert_eq!(
        read(&e.root, "/etc/samba/smb.conf"),
        "# Debian default\n[global]\n"
    );
    assert!(!e.root.join("etc/samba/smb.conf.rynas-new").exists());
    assert!(!e.root.join("etc/exports.d/rynas.exports").exists());
    // validation runs before any system change
    assert!(e.acc.state().log.is_empty());
    assert!(e.host.state().acls.is_empty());
    assert_eq!(
        e.cfg.pending("admin").unwrap().changes.len(),
        2,
        "still pending"
    );
}

#[test]
fn a_failing_share_rolls_the_users_back() {
    let e = env();
    stage(&e, USERS_KEY, &users_doc()).unwrap();
    stage(&e, SHARES_KEY, &shares_doc()).unwrap();
    e.host.state().fail_acl.push("/srv/vol1/media".into());
    let err = apply(&e).unwrap_err();
    assert!(err.to_string().contains("Operation not supported"), "{err}");
    let st = e.acc.state();
    assert!(
        st.users.is_empty(),
        "users created by the failed apply are removed: {:?}",
        st.users
    );
    assert!(!st.groups.iter().any(|g| g.name == "office"));
    assert_eq!(
        read(&e.root, "/etc/samba/smb.conf"),
        "# Debian default\n[global]\n"
    );
}

#[test]
fn user_lifecycle_and_password_transaction() {
    let e = env();
    stage(&e, USERS_KEY, &users_doc()).unwrap();
    apply(&e).unwrap();

    // password: chpasswd first, then the Samba account
    e.acc.state().log.clear();
    set_password(
        e.acc.as_ref(),
        "bob",
        &Secret::new("correct horse"),
        true,
        false,
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        e.acc.state().log,
        [
            "chpasswd: new password for bob",
            "pdbedit: Samba account for bob"
        ]
    );
    // a second change goes through smbpasswd (the Samba account exists)
    set_password(
        e.acc.as_ref(),
        "bob",
        &Secret::new("battery staple"),
        true,
        false,
        &|_| {},
    )
    .unwrap();
    assert_eq!(e.acc.state().samba_pw["bob"], "hash(battery staple)");

    // Samba failure: the shadow password goes back to what it was
    let before = e.acc.state().shadow["alice"].clone();
    e.acc.state().fail.push("pdbedit-add".into());
    let err = set_password(
        e.acc.as_ref(),
        "alice",
        &Secret::new("new password"),
        true,
        false,
        &|_| {},
    )
    .unwrap_err();
    assert!(err.contains("Linux password was not changed"), "{err}");
    let log = e.acc.state().log.clone();
    assert_eq!(
        &log[log.len() - 3..],
        [
            "chpasswd: new password for alice",
            "pdbedit: Samba account for alice",
            "chpasswd: previous password of alice restored"
        ]
    );
    assert_eq!(e.acc.state().shadow["alice"], before);
    e.acc.state().fail.clear();
    // rules
    assert!(
        set_password(
            e.acc.as_ref(),
            "bob",
            &Secret::new("short"),
            true,
            false,
            &|_| {}
        )
        .is_err()
    );
    assert!(
        set_password(
            e.acc.as_ref(),
            "nobody",
            &Secret::new("long enough"),
            true,
            false,
            &|_| {}
        )
        .is_err()
    );

    // modify: disable bob, take him out of interns, no SMB for carol
    let mut doc = users_doc();
    doc.users[1].disabled = true;
    doc.users[1].groups = vec!["office".into()];
    doc.users[2].smb = false;
    e.acc.state().log.clear();
    stage(&e, USERS_KEY, &doc).unwrap();
    apply(&e).unwrap();
    let log = e.acc.state().log.clone();
    assert!(
        log.contains(&"usermod bob: disabled".to_string()),
        "{log:#?}"
    );
    assert!(log.contains(&"gpasswd: remove bob from interns".to_string()));
    assert!(log.contains(&"pdbedit: Samba account of bob disabled".to_string()));
    assert_eq!(
        e.acc
            .state()
            .users
            .iter()
            .find(|u| u.name == "bob")
            .unwrap()
            .expire_days,
        Some(1)
    );

    // a user created outside rynas is reported, not touched
    e.acc.state().users.push(SysUser {
        name: "eve".into(),
        uid: 1500,
        gid: 100,
        home: "/home/eve".into(),
        shell: "/bin/bash".into(),
        ..Default::default()
    });
    let d = e.cfg.drift().unwrap();
    assert!(
        d.state
            .iter()
            .any(|s| s.object == "user:eve" && s.kind == StateDriftKind::Unmanaged)
    );
    assert!(
        d.state
            .iter()
            .any(|s| s.object == "user:alice" && s.kind == StateDriftKind::MissingInSamba)
    );

    // delete bob with his home, and the interns group
    let mut doc2 = doc.clone();
    doc2.users.retain(|u| u.name != "bob");
    doc2.groups.retain(|g| g.name != "interns");
    doc2.remove_homes = vec!["bob".into()];
    e.acc.state().log.clear();
    stage(&e, USERS_KEY, &doc2).unwrap();
    apply(&e).unwrap();
    let log = e.acc.state().log.clone();
    assert_eq!(
        log,
        [
            "pdbedit: Samba account of bob removed",
            "userdel bob (home removed)",
            "groupdel interns"
        ]
    );
    assert!(
        e.acc.state().users.iter().any(|u| u.name == "eve"),
        "unmanaged user kept"
    );
}

#[test]
fn a_share_can_ask_for_its_subvolume() {
    let e = env();
    stage(&e, USERS_KEY, &users_doc()).unwrap();
    let mut d = shares_doc();
    d.shares.push(Share {
        name: "video".into(),
        subvolume: Some(rynas_api_types::ShareSubvolume {
            volume: "vol1".into(),
            compression: false,
        }),
        ..Default::default()
    });
    stage(&e, SHARES_KEY, &d).unwrap();
    let staged: SharesConfig = serde_json::from_value(
        e.cfg
            .pending_value("admin", SHARES_KEY)
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let video = staged.shares.iter().find(|s| s.name == "video").unwrap();
    assert_eq!(video.path, "/srv/vol1/video");
    // nothing is created before the apply
    assert!(e.creator.calls.lock().unwrap().is_empty());
    apply(&e).unwrap();
    assert_eq!(
        e.creator.calls.lock().unwrap().as_slice(),
        ["/srv/vol1/video compression=false"]
    );
    assert!(read(&e.root, "/etc/samba/smb.conf").contains("[video]"));
    // applying again does not create it twice
    let mut d2 = staged.clone();
    d2.shares
        .iter_mut()
        .find(|s| s.name == "video")
        .unwrap()
        .description = "films".into();
    stage(&e, SHARES_KEY, &d2).unwrap();
    apply(&e).unwrap();
    assert_eq!(e.creator.calls.lock().unwrap().len(), 1);

    // refusals: unknown volume, a path elsewhere
    let bad = |doc: SharesConfig| stage(&e, SHARES_KEY, &doc).unwrap_err().to_string();
    let mut d = shares_doc();
    d.shares.push(Share {
        name: "x".into(),
        subvolume: Some(rynas_api_types::ShareSubvolume {
            volume: "vol9".into(),
            compression: true,
        }),
        ..Default::default()
    });
    assert!(bad(d.clone()).contains("vol9 is not mounted"));
    d.shares
        .last_mut()
        .unwrap()
        .subvolume
        .as_mut()
        .unwrap()
        .volume = "vol1".into();
    d.shares.last_mut().unwrap().path = "/srv/other/x".into();
    assert!(bad(d).contains("is created at /srv/vol1/x"));
}

fn calls(e: &Env) -> Vec<String> {
    std::mem::take(&mut e.sys.state().calls)
}

#[test]
fn snapshot_folder_mount_units_follow_the_option() {
    let e = env();
    let mut doc = shares_doc();
    let docs = &mut doc.shares[0];
    docs.show_snapshots = true;
    docs.smb.shadow_copy.snapshot_dir = "/srv/vol1/.snapshots/docs".into();
    stage(&e, SHARES_KEY, &doc).unwrap();
    stage(&e, USERS_KEY, &users_doc()).unwrap();
    apply(&e).unwrap();
    let unit = "/etc/systemd/system/srv-vol1-docs-\\x23snapshot.mount";
    let text = read(&e.root, unit);
    assert!(text.contains("What=/srv/vol1/.snapshots/docs\n"), "{text}");
    assert!(text.contains("Where=/srv/vol1/docs/#snapshot\n"));
    let target = read(&e.root, "/etc/systemd/system/rynas-snapshot-folders.target");
    assert!(target.contains("Wants=srv-vol1-docs-\\x23snapshot.mount"));
    let c = calls(&e);
    let pos = |needle: &str| c.iter().position(|x| x.contains(needle));
    let reload = pos("daemon-reload").expect("daemon-reload");
    let enable = pos("enable --now rynas-snapshot-folders.target").expect("target enabled");
    assert!(reload < enable, "{c:?}");
    assert!(e.host.state().dir_log.contains(&"mkdir /srv/vol1/.snapshots/docs".to_string()));
    // SMB shows the folder: not vetoed, excluded from the recycle bin
    let smb = read(&e.root, "/etc/samba/smb.conf");
    assert!(smb.contains("recycle:exclude_dir = #snapshot"), "{smb}");

    // off again: the unit goes, it is stopped and its empty mount point removed
    doc.shares[0].show_snapshots = false;
    stage(&e, SHARES_KEY, &doc).unwrap();
    apply(&e).unwrap();
    assert!(!e.root.join(unit.trim_start_matches('/')).exists());
    let c = calls(&e);
    assert!(
        c.iter().any(|x| x.contains("stop srv-vol1-docs-\\x23snapshot.mount")),
        "{c:?}"
    );
    assert!(e.host.state().dir_log.contains(&"rmdir /srv/vol1/docs/#snapshot".to_string()));
    let target = read(&e.root, "/etc/systemd/system/rynas-snapshot-folders.target");
    assert!(!target.contains("Wants="));
}

#[test]
fn deleted_shares_are_recorded_or_removed_with_their_folder() {
    let e = env();
    let mut doc = shares_doc();
    stage(&e, SHARES_KEY, &doc).unwrap();
    stage(&e, USERS_KEY, &users_doc()).unwrap();
    apply(&e).unwrap();
    calls(&e);
    // "media" goes without its folder, "scans" with it
    e.host.state().snapshots.insert(
        "/srv/vol1/.snapshots/scans".into(),
        vec!["scans.20261105T1200".into()],
    );
    doc.shares.retain(|s| s.name != "media" && s.name != "scans");
    doc.remove_folders.push(rynas_api_types::FolderRemoval {
        share: "scans".into(),
        path: "/srv/vol1/scans".into(),
        snapshots: vec![rynas_api_types::SnapshotSource {
            dir: "/srv/vol1/.snapshots/scans".into(),
            prefix: "scans".into(),
        }],
    });
    stage(&e, SHARES_KEY, &doc).unwrap();
    apply(&e).unwrap();
    let c = calls(&e);
    let smb_reload = c.iter().position(|x| x.contains("reload smbd")).unwrap();
    let snap = c
        .iter()
        .position(|x| x.contains("btrfs subvolume delete /srv/vol1/.snapshots/scans/scans.20261105T1200"))
        .expect("snapshot deleted");
    let rm = c
        .iter()
        .position(|x| x.contains("rm -rf --one-file-system --preserve-root=all -- /srv/vol1/scans"))
        .expect("folder deleted");
    assert!(smb_reload < snap && snap < rm, "{c:?}");
    assert!(!c.iter().any(|x| x.contains("/srv/vol1/media") && x.starts_with("run ")));
    let rec = e.removed.find("/srv/vol1/media").expect("media recorded");
    assert_eq!(rec.share.permissions.len(), 1);
    assert!(e.removed.find("/srv/vol1/scans").is_none());

    // a removal entry for a share that still exists is dropped by validation
    let mut again = doc.clone();
    again.remove_folders.push(rynas_api_types::FolderRemoval {
        share: "docs".into(),
        path: "/srv/vol1/docs".into(),
        snapshots: Vec::new(),
    });
    stage(&e, SHARES_KEY, &again).unwrap();
    // normalized away: nothing differs from the applied document
    let pending = e.cfg.pending_value("admin", SHARES_KEY).unwrap().flatten();
    let staged: SharesConfig = pending
        .map(|v| serde_json::from_value(v).unwrap())
        .unwrap_or_default();
    assert!(staged.remove_folders.iter().all(|r| r.share != "docs"));
}

#[test]
fn validation_errors() {
    let e = env();
    let bad = |doc: SharesConfig| stage(&e, SHARES_KEY, &doc).unwrap_err().to_string();
    let mut d = shares_doc();
    d.shares[0].path = "/srv/vol1/nope".into();
    assert!(bad(d).contains("does not exist"));
    let mut d = shares_doc();
    d.shares[1].name = "DOCS".into();
    assert!(bad(d).contains("twice"));
    let mut d = shares_doc();
    d.shares[1].smb.extra = "[evil]\npath = /etc".into();
    assert!(bad(d).contains("extra options"));
    let mut d = shares_doc();
    d.shares[0].nfs.rules[0].client = "x(rw,no_root_squash)".into();
    assert!(bad(d).contains("invalid NFS client"));
    let mut d = shares_doc();
    d.shares[0].path = "/etc".into();
    assert!(bad(d).contains("cannot be shared"));
    e.host.state().paths.insert(
        "/srv/remote".into(),
        crate::host::PathInfo {
            exists: true,
            is_dir: true,
            fs_type: "nfs".into(),
            remote: true,
            ..Default::default()
        },
    );
    let mut d = shares_doc();
    d.shares[3].path = "/srv/remote".into();
    assert!(bad(d).contains("local filesystem"));

    let bad_users = |doc: UsersConfig| stage(&e, USERS_KEY, &doc).unwrap_err().to_string();
    let mut u = users_doc();
    u.users[0].groups.push("nosuchgroup".into());
    assert!(bad_users(u).contains("not a local group"));
    let mut u = users_doc();
    u.users[0].name = "root".into();
    assert!(bad_users(u).contains("system account"));
    let mut u = users_doc();
    u.users[0].expires = Some("2030-02-30".into());
    assert!(bad_users(u).contains("not a date"));
    let mut u = users_doc();
    u.users[0].full_name = "Smith, Alice".into();
    assert!(bad_users(u).contains("cannot contain"));
}

#[test]
fn turning_services_off_removes_the_avahi_file() {
    let e = env();
    stage(&e, SHARES_KEY, &shares_doc()).unwrap();
    // the matrix names users that do not exist: their ACL entries still render (setfacl on
    // a real system would refuse unknown names; the API checks principals before staging)
    apply(&e).unwrap();
    assert!(e.root.join("etc/avahi/services/rynas-smb.service").exists());
    let mut d = shares_doc();
    d.smb.mdns = false;
    d.smb.enabled = false;
    d.nfs.enabled = false;
    e.sys.state().calls.clear();
    stage(&e, SHARES_KEY, &d).unwrap();
    apply(&e).unwrap();
    assert!(!e.root.join("etc/avahi/services/rynas-smb.service").exists());
    assert!(!read(&e.root, "/etc/exports.d/rynas.exports").contains("/srv"));
    let calls = e.sys.state().calls.clone();
    for c in [
        "run systemctl disable --now smbd.service",
        "run systemctl disable --now wsdd2.service",
        "run exportfs -ra",
        "run systemctl disable --now nfs-server.service",
    ] {
        assert!(calls.iter().any(|x| x == c), "{c} not in {calls:#?}");
    }
    // and back on: the file returns
    stage(&e, SHARES_KEY, &shares_doc()).unwrap();
    apply(&e).unwrap();
    assert!(e.root.join("etc/avahi/services/rynas-smb.service").exists());
}

#[test]
fn imported_users_keep_their_login_shell() {
    let e = env();
    {
        let mut s = e.acc.state();
        s.users.push(SysUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            full_name: "Alice".into(),
            home: "/home/alice".into(),
            shell: "/bin/zsh".into(),
            expire_days: None,
        });
        s.groups.push(crate::accounts::SysGroup {
            name: "alice".into(),
            gid: 1000,
            members: vec![],
        });
    }
    // never applied: the current value is the system
    let cur: UsersConfig =
        serde_json::from_value(e.cfg.current(USERS_KEY).unwrap().unwrap()).unwrap();
    assert_eq!(cur.users.len(), 1);
    assert_eq!(cur.users[0].shell, ShellAccess::Shell);
    // add a user on top of the import
    let mut doc = cur.clone();
    doc.users.push(user("dave", &[]));
    stage(&e, USERS_KEY, &doc).unwrap();
    apply(&e).unwrap();
    let st = e.acc.state();
    assert_eq!(
        st.users.iter().find(|u| u.name == "alice").unwrap().shell,
        "/bin/zsh"
    );
    let dave = st.users.iter().find(|u| u.name == "dave").unwrap();
    assert_eq!(dave.uid, 1001, "next free uid after the imported one");
    assert!(
        st.log.iter().all(|l| !l.starts_with("usermod marco")),
        "{:?}",
        st.log
    );
}
