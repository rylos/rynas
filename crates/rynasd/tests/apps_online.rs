//! Signed online catalogue downloads against a local HTTP server on 127.0.0.1 (no real
//! service): switch to a verified catalogue, 304, refusals that keep the catalogue in use,
//! the previous catalogue kept for fallback.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rynas_apps::Version;
use rynas_apps::online::test_signer::{TestKey, archive, files_of};
use rynas_apps::online::{ArchiveRef, Index, IndexApp, OnlineError, sha256_hex};
use rynasd::apps::online::{Online, Outcome};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

type Files = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A minimal HTTP/1.1 server: GET of the files in the map, ETag = length + hash, 304 on a
/// matching `If-None-Match`.
async fn serve() -> (String, Files) {
    let files: Files = Arc::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1/", l.local_addr().unwrap());
    let f2 = files.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { return };
            let files = f2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("/").trim_start_matches("/v1/").to_owned();
                let inm = req
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().starts_with("if-none-match:").then(|| l[14..].trim().to_owned()));
                let body = files.lock().unwrap().get(&path).cloned();
                let resp = match body {
                    None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
                    Some(b) => {
                        let etag = format!("\"{}-{}\"", b.len(), &sha256_hex(&b)[..8]);
                        if inm.as_deref() == Some(etag.as_str()) {
                            format!("HTTP/1.1 304 Not Modified\r\netag: {etag}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").into_bytes()
                        } else {
                            let mut r = format!("HTTP/1.1 200 OK\r\netag: {etag}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", b.len()).into_bytes();
                            r.extend_from_slice(&b);
                            r
                        }
                    }
                };
                let _ = s.write_all(&resp).await;
                let _ = s.shutdown().await;
            });
        }
    });
    (base, files)
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

/// Publishes a catalogue with `uptime-kuma` (and a broken `mosquitto` when asked) at
/// `serial`, signed by `key`; `tamper` changes the archive after the index named it.
fn publish(files: &Files, work: &Path, serial: u64, key: &TestKey, broken: bool, tamper: bool) {
    let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps");
    let src = work.join(format!("src-{serial}-{broken}-{tamper}"));
    let _ = fs::remove_dir_all(&src);
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("catalog.toml"), format!("serial = {serial}\n")).unwrap();
    copy_dir(&apps.join("uptime-kuma"), &src.join("uptime-kuma"));
    let mut listed = vec![IndexApp { id: "uptime-kuma".into(), version: Version::new(1, 0, 0), min_rynas: Version::new(0, 1, 0) }];
    if broken {
        copy_dir(&apps.join("mosquitto"), &src.join("mosquitto"));
        fs::write(src.join("mosquitto/compose.yaml"), "services: 3\n").unwrap();
        listed.push(IndexApp { id: "mosquitto".into(), version: Version::new(1, 0, 0), min_rynas: Version::new(0, 1, 0) });
    }
    let all = files_of(&src, "");
    let refs: Vec<(&str, &[u8])> = all.iter().map(|(p, d)| (p.as_str(), d.as_slice())).collect();
    let ar = archive(&refs);
    let name = format!("apps-{serial}.tar.zst");
    let idx = Index {
        schema: 1,
        serial,
        published: Some("2026-11-10T00:00:00Z".into()),
        expires: (jiff::Timestamp::now() + jiff::SignedDuration::from_hours(24 * 30)).round(jiff::Unit::Second).unwrap().to_string(),
        archive: ArchiveRef { url: name.clone(), sha256: sha256_hex(&ar), size: ar.len() as u64 },
        apps: listed,
    };
    let body = serde_json::to_vec_pretty(&idx).unwrap();
    let sig = key.sign(&body, &format!("serial={serial} expires={}", idx.expires));
    let mut ar = ar;
    if tamper {
        let n = ar.len() - 1;
        ar[n] ^= 0xff;
    }
    let mut f = files.lock().unwrap();
    f.insert("index.json".into(), body);
    f.insert("index.json.minisig".into(), sig.into_bytes());
    f.insert(name, ar);
}

fn link(dir: &Path, name: &str) -> Option<String> {
    fs::read_link(dir.join(name)).ok().map(|p| p.to_string_lossy().into_owned())
}

fn numbered(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.contains('.') && n.parse::<u64>().is_ok() || n.starts_with(".new")).collect();
    v.sort();
    v
}

const RUNNING: Version = Version::new(0, 5, 0);

#[tokio::test]
async fn downloads_verify_switch_and_refuse() {
    let (base, files) = serve().await;
    let t = tempfile::tempdir().unwrap();
    let dir: PathBuf = t.path().join("catalog");
    let key = TestKey::new(3);
    let o = Online::new_for_tests(dir.clone(), &[&key.public()]);
    let log = |_: &str| {};

    // first catalogue
    publish(&files, t.path(), 3, &key, false, false);
    let r = o.check(&base, RUNNING, &log).await.unwrap();
    assert_eq!(r, Outcome::Updated { serial: 3, previous: None, skipped: vec![] });
    assert_eq!(link(&dir, "current").as_deref(), Some("3"));
    assert_eq!(o.active().unwrap().index.serial, 3);
    assert!(dir.join("3/uptime-kuma/app.toml").is_file());

    // nothing changed: the ETag gives a 304
    assert_eq!(o.check(&base, RUNNING, &log).await.unwrap(), Outcome::NotModified);

    // a newer catalogue with a broken entry: refused, nothing left behind
    publish(&files, t.path(), 4, &key, true, false);
    let e = o.check(&base, RUNNING, &log).await.unwrap_err();
    assert!(matches!(e, OnlineError::Invalid(_)), "{e:?}");
    assert_eq!(link(&dir, "current").as_deref(), Some("3"));
    assert_eq!(numbered(&dir), ["3"]);

    // an archive that differs from the index
    publish(&files, t.path(), 4, &key, false, true);
    assert!(matches!(o.check(&base, RUNNING, &log).await, Err(OnlineError::Archive(_))));
    assert_eq!(link(&dir, "current").as_deref(), Some("3"));

    // signed by a key this build does not trust
    publish(&files, t.path(), 4, &TestKey::new(4), false, false);
    assert_eq!(o.check(&base, RUNNING, &log).await, Err(OnlineError::UnknownKey));

    // an older catalogue, validly signed: rollback refused
    publish(&files, t.path(), 2, &key, false, false);
    assert!(matches!(o.check(&base, RUNNING, &log).await, Err(OnlineError::Rollback { serial: 2, last: 3 })));
    assert_eq!(link(&dir, "current").as_deref(), Some("3"));

    // a good newer one: switch, the old one kept as previous
    publish(&files, t.path(), 5, &key, false, false);
    let r = o.check(&base, RUNNING, &log).await.unwrap();
    assert_eq!(r, Outcome::Updated { serial: 5, previous: Some(3), skipped: vec![] });
    assert_eq!(link(&dir, "current").as_deref(), Some("5"));
    assert_eq!(link(&dir, "previous").as_deref(), Some("3"));

    // and the next one drops the oldest
    publish(&files, t.path(), 6, &key, false, false);
    o.check(&base, RUNNING, &log).await.unwrap();
    assert_eq!(numbered(&dir), ["5", "6"]);

    // the state survives a restart: the last serial still blocks a rollback
    let state: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["serial"], 6);
    assert!(state["last_error"].is_null());
    let o2 = Online::new_for_tests(dir.clone(), &[&key.public()]);
    fs::remove_file(dir.join("current")).unwrap();
    publish(&files, t.path(), 5, &key, false, false);
    assert!(matches!(o2.check(&base, RUNNING, &log).await, Err(OnlineError::Rollback { serial: 5, last: 6 })));
    // without `current`, the previous catalogue is still used
    assert_eq!(o2.active().unwrap().serial, 5);
}

#[tokio::test]
async fn no_key_never_downloads() {
    let (base, files) = serve().await;
    let t = tempfile::tempdir().unwrap();
    publish(&files, t.path(), 3, &TestKey::new(3), false, false);
    let o = Online::new_for_tests(t.path().join("catalog"), &[]);
    assert!(!o.has_key());
    assert_eq!(o.check(&base, RUNNING, &|_| {}).await, Err(OnlineError::NoKey));
    assert!(o.active().is_none());
}

#[tokio::test]
async fn a_damaged_current_catalogue_is_downloaded_again() {
    let (base, files) = serve().await;
    let t = tempfile::tempdir().unwrap();
    let dir: PathBuf = t.path().join("catalog");
    let key = TestKey::new(3);
    let o = Online::new_for_tests(dir.clone(), &[&key.public()]);
    let log = |_: &str| {};
    publish(&files, t.path(), 3, &key, false, false);
    o.check(&base, RUNNING, &log).await.unwrap();
    publish(&files, t.path(), 4, &key, false, false);
    o.check(&base, RUNNING, &log).await.unwrap();
    assert_eq!(link(&dir, "previous").as_deref(), Some("3"));

    // the index of `current` is damaged: same serial, fetched again (no 304, no "in use")
    fs::write(dir.join("4/index.json"), b"{broken").unwrap();
    let r = o.check(&base, RUNNING, &log).await.unwrap();
    assert_eq!(r, Outcome::Updated { serial: 4, previous: None, skipped: vec![] });
    assert_eq!(o.active().unwrap().serial, 4);
    assert_eq!(link(&dir, "previous").as_deref(), Some("3"));
    assert_eq!(numbered(&dir), ["3", "4"]);
    // and in use again afterwards
    assert_eq!(o.check(&base, RUNNING, &log).await.unwrap(), Outcome::NotModified);

    // `current` missing altogether (the previous one would load)
    fs::remove_file(dir.join("current")).unwrap();
    let r = o.check(&base, RUNNING, &log).await.unwrap();
    assert_eq!(r, Outcome::Updated { serial: 4, previous: None, skipped: vec![] });
    assert_eq!(link(&dir, "current").as_deref(), Some("4"));
    assert_eq!(link(&dir, "previous").as_deref(), Some("3"));
}
