//! Online catalogue: validation of an unpacked catalogue before it may replace the one in
//! use, and the merge with the shipped catalogue (real entries of `apps/`).

use std::fs;
use std::path::{Path, PathBuf};

use rynas_apps::catalog::{Origin, merge, read_serial};
use rynas_apps::online::test_signer::{TestKey, archive, files_of};
use rynas_apps::online::{ArchiveRef, Index, IndexApp, OnlineError, extract, sha256_hex, validate, verify_index};
use rynas_apps::{Catalog, Version};

fn apps_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let t = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &t);
        } else {
            fs::copy(e.path(), t).unwrap();
        }
    }
}

/// A catalogue folder with `ids` copied from `apps/` and `catalog.toml` at `serial`.
fn catalogue(root: &Path, serial: u64, ids: &[&str]) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("catalog.toml"), format!("serial = {serial}\n")).unwrap();
    for id in ids {
        copy_dir(&apps_dir().join(id), &root.join(id));
    }
    root.to_owned()
}

fn set_field(dir: &Path, id: &str, key: &str, value: &str) {
    let p = dir.join(id).join("app.toml");
    let text = fs::read_to_string(&p).unwrap();
    let out: Vec<String> = text
        .lines()
        .map(|l| if l.starts_with(&format!("{key} = ")) { format!("{key} = \"{value}\"") } else { l.to_owned() })
        .collect();
    fs::write(p, out.join("\n") + "\n").unwrap();
}

fn entry(id: &str, version: &str, min: &str) -> IndexApp {
    IndexApp { id: id.into(), version: version.parse().unwrap(), min_rynas: min.parse().unwrap() }
}

fn index(serial: u64, apps: Vec<IndexApp>) -> Index {
    Index {
        schema: 1,
        serial,
        published: None,
        expires: "2099-01-01T00:00:00Z".into(),
        archive: ArchiveRef { url: "a.tar.zst".into(), sha256: "0".repeat(64), size: 1 },
        apps,
    }
}

const RUNNING: Version = Version::new(0, 5, 0);

#[test]
fn shipped_catalogue_has_a_serial() {
    assert!(read_serial(&apps_dir()).unwrap().is_some_and(|s| s >= 1));
}

#[test]
fn valid_catalogue_passes() {
    let t = tempfile::tempdir().unwrap();
    let dir = catalogue(&t.path().join("c"), 7, &["uptime-kuma", "mosquitto"]);
    let idx = index(7, vec![entry("uptime-kuma", "1.0.0", "0.1.0"), entry("mosquitto", "1.0.0", "0.1.0")]);
    let v = validate(&dir, &idx, RUNNING).unwrap();
    assert_eq!(v.apps.len(), 2);
    assert!(v.skipped.is_empty());
}

#[test]
fn broken_entry_refuses_the_whole_catalogue() {
    let t = tempfile::tempdir().unwrap();
    let dir = catalogue(&t.path().join("c"), 7, &["uptime-kuma", "mosquitto"]);
    fs::write(dir.join("mosquitto/compose.yaml"), "services: [not, a, map]\n").unwrap();
    let idx = index(7, vec![entry("uptime-kuma", "1.0.0", "0.1.0"), entry("mosquitto", "1.0.0", "0.1.0")]);
    assert!(matches!(validate(&dir, &idx, RUNNING), Err(OnlineError::Invalid(_))));
}

#[test]
fn entry_for_a_newer_rynas_may_fail_and_is_skipped() {
    let t = tempfile::tempdir().unwrap();
    let dir = catalogue(&t.path().join("c"), 7, &["uptime-kuma", "mosquitto"]);
    // a field this rynas does not know yet
    let p = dir.join("mosquitto/app.toml");
    let text = fs::read_to_string(&p).unwrap().replace("min_rynas = \"0.1.0\"", "min_rynas = \"9.0.0\"\nfuture_field = true");
    fs::write(p, text).unwrap();
    let idx = index(7, vec![entry("uptime-kuma", "1.0.0", "0.1.0"), entry("mosquitto", "1.0.0", "9.0.0")]);
    let v = validate(&dir, &idx, RUNNING).unwrap();
    assert_eq!(v.apps, ["uptime-kuma"]);
    assert_eq!(v.skipped, ["mosquitto"]);
}

#[test]
fn index_and_folders_must_agree() {
    let t = tempfile::tempdir().unwrap();
    let dir = catalogue(&t.path().join("c"), 7, &["uptime-kuma"]);
    let ok = vec![entry("uptime-kuma", "1.0.0", "0.1.0")];
    // serial of catalog.toml
    assert!(matches!(validate(&dir, &index(8, ok.clone()), RUNNING), Err(OnlineError::Invalid(_))));
    // version differs from app.toml
    assert!(matches!(validate(&dir, &index(7, vec![entry("uptime-kuma", "1.0.1", "0.1.0")]), RUNNING), Err(OnlineError::Invalid(_))));
    // listed but missing
    let two = vec![entry("uptime-kuma", "1.0.0", "0.1.0"), entry("mosquitto", "1.0.0", "0.1.0")];
    assert!(matches!(validate(&dir, &index(7, two), RUNNING), Err(OnlineError::Invalid(_))));
    // an unexpected file at the top
    fs::write(dir.join("README"), "x").unwrap();
    assert!(matches!(validate(&dir, &index(7, ok), RUNNING), Err(OnlineError::Invalid(_))));
}

#[test]
fn signed_archive_round_trip() {
    let t = tempfile::tempdir().unwrap();
    let src = catalogue(&t.path().join("src"), 3, &["uptime-kuma"]);
    let files = files_of(&src, "");
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
    let ar = archive(&refs);
    let mut idx = index(3, vec![entry("uptime-kuma", "1.0.0", "0.1.0")]);
    idx.archive = ArchiveRef { url: "apps-3.tar.zst".into(), sha256: sha256_hex(&ar), size: ar.len() as u64 };
    let body = serde_json::to_vec(&idx).unwrap();
    let key = TestKey::new(1);
    let sig = key.sign(&body, &format!("serial=3 expires={}", idx.expires));
    let got = verify_index(&body, &sig, &[&key.public()]).unwrap();
    rynas_apps::online::check_archive(&ar, &got.archive).unwrap();
    let out = t.path().join("out");
    extract(&ar, &out).unwrap();
    assert_eq!(validate(&out, &got, RUNNING).unwrap().apps, ["uptime-kuma"]);
}

fn load(dir: &Path) -> Catalog {
    Catalog::load(dir, "rynas").unwrap()
}

#[test]
fn merge_keeps_the_higher_runnable_version() {
    let t = tempfile::tempdir().unwrap();
    let shipped = catalogue(&t.path().join("s"), 5, &["uptime-kuma", "mosquitto"]);
    let online = catalogue(&t.path().join("o"), 6, &["uptime-kuma", "mosquitto", "navidrome"]);
    set_field(&online, "uptime-kuma", "version", "1.1.0");
    // newer, but for a rynas this one is not
    set_field(&online, "mosquitto", "version", "2.0.0");
    set_field(&online, "mosquitto", "min_rynas", "9.0.0");
    let (m, origin) = merge(load(&shipped), load(&online), RUNNING);
    assert_eq!(m.get("uptime-kuma").unwrap().manifest.version, Version::new(1, 1, 0));
    assert_eq!(origin["uptime-kuma"], Origin::Online);
    assert_eq!(m.get("mosquitto").unwrap().manifest.version, Version::new(1, 0, 0));
    assert_eq!(origin["mosquitto"], Origin::Shipped);
    // only in the download: kept
    assert_eq!(origin["navidrome"], Origin::Online);
    assert!(m.get("navidrome").unwrap().dir.starts_with(&online));
}

#[test]
fn merge_keeps_shipped_on_equal_or_older_versions() {
    let t = tempfile::tempdir().unwrap();
    let shipped = catalogue(&t.path().join("s"), 5, &["uptime-kuma", "mosquitto"]);
    set_field(&shipped, "uptime-kuma", "version", "1.2.0");
    let online = catalogue(&t.path().join("o"), 6, &["uptime-kuma", "mosquitto"]);
    set_field(&online, "uptime-kuma", "version", "1.1.0");
    let (m, origin) = merge(load(&shipped), load(&online), RUNNING);
    assert_eq!(m.get("uptime-kuma").unwrap().manifest.version, Version::new(1, 2, 0));
    assert_eq!(origin["uptime-kuma"], Origin::Shipped);
    // same version: the shipped entry
    assert_eq!(origin["mosquitto"], Origin::Shipped);
    assert!(m.get("mosquitto").unwrap().dir.starts_with(&shipped));
}

#[test]
fn merge_prefers_a_runnable_entry() {
    let t = tempfile::tempdir().unwrap();
    let shipped = catalogue(&t.path().join("s"), 5, &["uptime-kuma"]);
    set_field(&shipped, "uptime-kuma", "min_rynas", "9.0.0");
    set_field(&shipped, "uptime-kuma", "version", "3.0.0");
    let online = catalogue(&t.path().join("o"), 6, &["uptime-kuma"]);
    let (_, origin) = merge(load(&shipped), load(&online), RUNNING);
    assert_eq!(origin["uptime-kuma"], Origin::Online);
}
