//! SFTP jobs through the real worker binary against an in-process SSH server (russh's) with a
//! scripted SFTP subsystem: nothing leaves this machine.

#[path = "support/fake_sftp.rs"]
mod fake;

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::keys::ssh_key::{HashAlg, private::Ed25519Keypair};
use russh::keys::PrivateKey;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};

use fake::{Script, Seen, serve};

fn worker() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_rynas-files-worker"))
}

struct Server {
    script: Script,
    seen: Seen,
    channel: Option<Channel<Msg>>,
}

impl russh::server::Handler for Server {
    type Error = russh::Error;
    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if password == "secret" { Auth::Accept } else { Auth::reject() })
    }
    async fn channel_open_session(&mut self, channel: Channel<Msg>, reply: russh::server::ChannelOpenHandle, _session: &mut Session) -> Result<(), Self::Error> {
        self.channel = Some(channel);
        reply.accept().await;
        Ok(())
    }
    async fn subsystem_request(&mut self, channel: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        assert_eq!(name, "sftp");
        session.channel_success(channel)?;
        let stream = self.channel.take().expect("channel opened").into_stream();
        tokio::spawn(serve(stream, self.script.clone(), self.seen.clone()));
        Ok(())
    }
}

/// An SSH server on a loopback port; the fingerprint its key shows.
fn start(script: Script) -> (SocketAddr, String, Seen, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&[42u8; 32]));
    let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    let config = Arc::new(russh::server::Config { keys: vec![key], ..Default::default() });
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Seen::default();
    let (sc, s2) = (script, seen.clone());
    rt.spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            let handler = Server { script: sc.clone(), seen: s2.clone(), channel: None };
            let config = Arc::clone(&config);
            tokio::spawn(async move {
                if let Ok(session) = russh::server::run_stream(config, tcp, handler).await {
                    let _ = session.await;
                }
            });
        }
    });
    (addr, fingerprint, seen, rt)
}

fn conn(addr: SocketAddr, host_key: &str, password: &str) -> serde_json::Value {
    serde_json::json!({ "id": "c", "name": "t", "kind": "sftp", "host": "127.0.0.1", "port": addr.port(), "user": "u", "auth": "password", "password": password, "host_key": host_key })
}

fn file(len: usize) -> Vec<u8> {
    (0..len as u32).map(|i| (i % 251) as u8).collect()
}

/// Runs a job to the end; (done event, stderr, how long).
fn run_job(spec: serde_json::Value, feed: impl FnOnce(&mut std::process::ChildStdin)) -> (serde_json::Value, String, Duration) {
    let t = Instant::now();
    let mut child = Command::new(worker()).arg("job").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{spec}").unwrap();
    feed(&mut stdin);
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let stderr_pipe = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || std::io::read_to_string(stderr_pipe).unwrap_or_default());
    let mut done = serde_json::Value::Null;
    for line in stdout.lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && v["t"] == "done"
        {
            done = v;
        }
    }
    let _ = child.wait();
    (done, err_thread.join().unwrap(), t.elapsed())
}

#[test]
fn a_job_downloads_a_file_through_a_real_ssh_session() {
    let data = file(8 << 20);
    let (addr, key, _seen, _rt) = start(Script { data: data.clone(), reverse_batches: 4, mtime: 1_700_000_000, ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("in");
    std::fs::create_dir(&dest).unwrap();
    let spec = serde_json::json!({ "op": "copy", "sources": ["/:c/data/big.iso"], "dest": dest, "roots": [dir.path()], "conflict": "overwrite", "verify": false, "remotes": [conn(addr, &key, "secret")] });
    let (done, stderr, _) = run_job(spec, |_| {});
    assert_eq!(done["status"], "ok", "{done} {stderr}");
    assert!(std::fs::read(dest.join("big.iso")).unwrap() == data);
    assert!(!dest.join(".big.iso.rynas-part").exists());
    assert!(stderr.contains("SFTP 127.0.0.1"), "one line per connection: {stderr}");
    assert!(stderr.contains("cipher"), "{stderr}");
}

#[test]
fn a_changed_host_key_refuses_the_job() {
    let (addr, _key, _seen, _rt) = start(Script { data: file(1000), ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("in");
    std::fs::create_dir(&dest).unwrap();
    let spec = serde_json::json!({ "op": "copy", "sources": ["/:c/data/x.bin"], "dest": dest, "roots": [dir.path()], "remotes": [conn(addr, "SHA256:not-this-key", "secret")] });
    let (done, _, _) = run_job(spec, |_| {});
    assert_eq!(done["status"], "failed", "{done}");
}

#[test]
fn a_cancel_during_a_blocked_read_ends_the_job_in_seconds() {
    // the server never answers a read
    let (addr, key, _seen, _rt) = start(Script { data: file(4 << 20), stall_read_at: Some(0), ..Default::default() });
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("in");
    std::fs::create_dir(&dest).unwrap();
    let spec = serde_json::json!({ "op": "copy", "sources": ["/:c/data/big.iso"], "dest": dest, "roots": [dir.path()], "remotes": [conn(addr, &key, "secret")] });
    let (done, stderr, took) = run_job(spec, |stdin| {
        // the job is at its first read when this arrives
        std::thread::sleep(Duration::from_secs(1));
        writeln!(stdin, "{}", serde_json::json!({ "c": "cancel" })).unwrap();
    });
    assert_eq!(done["status"], "cancelled", "{done} {stderr}");
    assert!(took < Duration::from_secs(12), "took {took:?}");
    assert!(!dest.join("big.iso").exists());
    let leftovers: Vec<_> = std::fs::read_dir(&dest).unwrap().flatten().collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn server_key_answers_without_logging_in() {
    let (addr, key, _seen, _rt) = start(Script::default());
    let mut child = Command::new(worker()).arg("call").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let call = serde_json::json!({ "op": "server_key", "conn": conn(addr, "", "") });
    writeln!(child.stdin.take().unwrap(), "{call}").unwrap();
    let output = child.wait_with_output().unwrap();
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(answer["key"], key, "{answer}");
}
