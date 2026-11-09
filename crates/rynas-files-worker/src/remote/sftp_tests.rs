use super::*;
use std::time::Instant;

#[path = "../../tests/support/fake_sftp.rs"]
mod fake;

use fake::{Script, Seen, serve};

fn file(len: usize) -> Vec<u8> {
    (0..len as u32).map(|i| (i % 251) as u8).collect()
}

/// A connection to the scripted server.
fn connected(script: Script, seen: Seen) -> Sftp {
    connected_with(script, seen, Duration::from_secs(60))
}

/// The same, the session's watchdog at `limit`.
fn connected_with(script: Script, seen: Seen, limit: Duration) -> Sftp {
    let (client, server) = tokio::io::duplex(1 << 20);
    let rt = runtime().unwrap();
    rt.spawn(serve(server, script, seen));
    let (sftp, features) = rt.block_on(start_session(client, limit)).unwrap();
    Sftp { rt, sftp: Arc::new(sftp), features, _session: None, host_key: String::new(), home: "/".into() }
}

fn path() -> Vec<String> {
    vec!["data".to_string(), "big.iso".to_string()]
}

fn read_all(r: &mut Box<dyn Reader>) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        let n = r.next_block(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[test]
fn unknown_keys_only_where_they_are_pinned() {
    let check = |expected: &str, learn: bool| Check { expected: expected.into(), learn, seen: Arc::default(), names: Arc::default() };
    assert!(check("SHA256:abc", false).accepts("SHA256:abc"));
    assert!(!check("SHA256:abc", true).accepts("SHA256:other"));
    // none remembered: taken only by a process that saves it
    assert!(check("", true).accepts("SHA256:abc"));
    assert!(!check("", false).accepts("SHA256:abc"));
    assert!(unknown_key("").to_string() != unknown_key("SHA256:abc").to_string());
}

#[test]
fn makes_keys() {
    let k = new_key().unwrap();
    assert!(k.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    let p = public_key(&k, "synofile-marco").unwrap();
    assert!(p.starts_with("ssh-ed25519 ") && p.ends_with(" synofile-marco"));
    // a key made here opens with the same function as an imported one
    assert!(import_key(&k, None).unwrap().1.starts_with("ssh-ed25519 "));
}

#[test]
fn aes_comes_first_only_with_hardware_aes() {
    use russh::cipher::{AES_128_GCM, AES_256_GCM, CHACHA20_POLY1305};
    let fast = cipher_order(true);
    assert_eq!((fast[0], fast[1], fast[2]), (AES_128_GCM, AES_256_GCM, CHACHA20_POLY1305));
    let slow = cipher_order(false);
    assert_eq!(slow[0], CHACHA20_POLY1305);
    assert!(slow.contains(&AES_128_GCM));
}

#[test]
fn errors_are_classified_by_their_type() {
    assert!(super::super::retryable(&map(SftpError::Timeout)));
    assert!(super::super::retryable(&map(SftpError::IO("broken".into()))));
    assert!(super::super::retryable(&map(SftpError::UnexpectedBehavior("sender dropped".into()))));
    let status = |code| russh_sftp::protocol::Status { id: 1, status_code: code, error_message: "x".into(), language_tag: String::new() };
    assert!(!super::super::retryable(&map(SftpError::Status(status(StatusCode::Failure)))));
    assert!(!super::super::retryable(&map(SftpError::Status(status(StatusCode::PermissionDenied)))));
    assert_eq!(map(SftpError::Status(status(StatusCode::NoSuchFile))).raw_os_error(), Some(libc::ENOENT));
}

#[test]
fn window_grows_per_reply_and_follows_the_link() {
    let mut p = Pace::new(64, 256 * 1024);
    assert_eq!(p.allowed(), 1);
    for _ in 0..100 {
        p.replied(256 * 1024);
    }
    assert_eq!(p.allowed(), 64);
    // a link that carries little: the window comes down so a request does not wait for long
    p.since = Instant::now() - Duration::from_secs(2);
    p.bytes = 200 * 1024;
    p.replied(0);
    assert!(p.allowed() < 64, "{}", p.allowed());
    assert!(p.allowed() >= 4);
}

#[test]
fn download_arrives_whole_and_in_order_even_out_of_order_on_the_wire() {
    let data = file(12 << 20);
    let sftp = connected(Script { data: data.clone(), reverse_batches: 8, mtime: 77, ..Default::default() }, Seen::default());
    let mut r = sftp.reader(&path()).unwrap();
    assert_eq!(r.len(), data.len() as u64);
    assert_eq!(r.ident(), Ident { size: data.len() as u64, mtime: 77, change: 0 });
    assert!(read_all(&mut r).unwrap() == data);
}

#[test]
fn short_replies_are_asked_for_again() {
    let data = file(3 << 20);
    // every reply is at most 60 KiB, as a server with its own idea of a block size
    let sftp = connected(Script { data: data.clone(), short: Some(60 * 1024), ..Default::default() }, Seen::default());
    let mut r = sftp.reader(&path()).unwrap();
    assert!(read_all(&mut r).unwrap() == data);
}

#[test]
fn a_file_that_shrank_ends_where_it_ends() {
    let data = file(1 << 20);
    // the size says 3 MiB, the file has 1: short reads, then the end, no error
    let sftp = connected(Script { data: data.clone(), size: Some(3 << 20), ..Default::default() }, Seen::default());
    let mut r = sftp.reader(&path()).unwrap();
    assert!(read_all(&mut r).unwrap() == data);
    assert_eq!(r.len(), data.len() as u64);
}

#[test]
fn seeking_starts_a_new_read_ahead_from_there() {
    let data = file(4 << 20);
    let sftp = connected(Script { data: data.clone(), ..Default::default() }, Seen::default());
    let mut r = sftp.reader(&path()).unwrap();
    let mut buf = vec![0u8; 1000];
    r.read_exact(&mut buf).unwrap();
    assert!(buf == data[..1000]);
    r.seek(SeekFrom::Start(3 << 20)).unwrap();
    r.read_exact(&mut buf).unwrap();
    assert!(buf == data[3 << 20..(3 << 20) + 1000]);
}

#[test]
fn a_stalling_server_fails_as_the_connections_error() {
    let data = file(4 << 20);
    let sftp = connected_with(Script { data, stall_read_at: Some(2 << 20), ..Default::default() }, Seen::default(), Duration::from_secs(1));
    let mut r = sftp.reader(&path()).unwrap();
    let start = Instant::now();
    let e = read_all(&mut r).unwrap_err();
    assert!(super::super::retryable(&e), "{e}");
    assert!(start.elapsed() < Duration::from_secs(10), "took {:?}", start.elapsed());
}

#[test]
fn a_file_the_server_refuses_is_that_files_error() {
    let data = file(2 << 20);
    let sftp = connected(Script { data, refuse_read_at: Some(1 << 20), ..Default::default() }, Seen::default());
    let mut r = sftp.reader(&path()).unwrap();
    let e = read_all(&mut r).unwrap_err();
    assert!(!super::super::retryable(&e), "{e}");
    assert!(e.to_string().contains("disk said no"), "{e}");
    // the session itself is fine
    assert!(sftp.stat(&path()).is_ok());
}

#[test]
fn a_connection_lost_in_mid_window_is_the_connections() {
    let data = file(8 << 20);
    let sftp = connected(Script { data, drop_read_at: Some(4 << 20), ..Default::default() }, Seen::default());
    let mut r = sftp.reader(&path()).unwrap();
    let e = read_all(&mut r).unwrap_err();
    assert!(super::super::retryable(&e), "{e}");
    // everything else fails too, quickly, not after a timeout
    let start = Instant::now();
    assert!(sftp.stat(&path()).is_err());
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_source_that_changed_shows_in_the_ident() {
    let a = connected(Script { data: file(1000), mtime: 10, ..Default::default() }, Seen::default());
    let b = connected(Script { data: file(1000), mtime: 20, ..Default::default() }, Seen::default());
    let (ra, rb) = (a.reader(&path()).unwrap(), b.reader(&path()).unwrap());
    assert_ne!(ra.ident(), rb.ident());
}

#[test]
fn the_limits_of_the_server_set_the_block() {
    let sftp = connected(Script { data: file(1 << 20), limits: Some((64 * 1024, 32 * 1024)), ..Default::default() }, Seen::default());
    assert_eq!(sftp.features.read_block, 64 * 1024);
    assert_eq!(sftp.features.write_block, 32 * 1024);
    let mut r = sftp.reader(&path()).unwrap();
    assert_eq!(read_all(&mut r).unwrap().len(), 1 << 20);
}

#[test]
fn an_upload_arrives_whole() {
    let seen = Seen::default();
    let sftp = connected(Script::default(), seen.clone());
    let mut w = sftp.writer(&path(), None).unwrap();
    let data = file(3 << 20);
    w.write_all(&data).unwrap();
    w.finish().unwrap();
    assert!(*seen.written.lock().unwrap() == data);
    assert_eq!(*seen.fsyncs.lock().unwrap(), 1);
}

#[test]
fn an_upload_without_fsync_support_needs_none() {
    let seen = Seen::default();
    let sftp = connected(Script { bare: true, ..Default::default() }, seen.clone());
    let mut w = sftp.writer(&path(), None).unwrap();
    w.write_all(b"data").unwrap();
    w.finish().unwrap();
    assert_eq!(*seen.fsyncs.lock().unwrap(), 0);
    assert_eq!(*seen.written.lock().unwrap(), b"data");
}

#[test]
fn a_slow_fsync_is_not_a_lost_connection() {
    let seen = Seen::default();
    // the watchdog gives up after a second of silence; fsync takes two
    let sftp = connected_with(Script { fsync_delay: Some(Duration::from_secs(2)), ..Default::default() }, seen.clone(), Duration::from_secs(1));
    let mut w = sftp.writer(&path(), None).unwrap();
    w.write_all(b"data").unwrap();
    w.finish().unwrap();
    assert_eq!(*seen.fsyncs.lock().unwrap(), 1);
}

#[test]
fn a_write_the_server_refuses_is_that_files_error() {
    let seen = Seen::default();
    let sftp = connected(Script { refuse_write_at: Some(1 << 20), ..Default::default() }, seen);
    let mut w = sftp.writer(&path(), None).unwrap();
    let data = file(3 << 20);
    let e = w.write_all(&data).and_then(|()| w.flush()).unwrap_err();
    assert!(!super::super::retryable(&e), "{e}");
    assert!(e.to_string().contains("disk said no"), "{e}");
    assert!(sftp.stat(&path()).is_ok());
}

#[test]
fn a_lost_connection_gives_the_unconfirmed_blocks_back() {
    let data = file(6 << 20);
    let seen = Seen::default();
    let sftp = connected(Script { drop_write_at: Some(2 << 20), ..Default::default() }, seen.clone());
    let mut w = sftp.writer(&path(), None).unwrap();
    let mut at = 0;
    let fail = loop {
        if at == data.len() {
            match w.flush() {
                Err(e) => break e,
                Ok(()) => panic!("the write should have failed"),
            }
        }
        match w.write(&data[at..]) {
            Ok(n) => at += n,
            Err(e) => break e,
        }
    };
    assert!(super::super::retryable(&fail), "{fail}");
    let back = w.unconfirmed();
    assert!(!back.is_empty());
    // everything accepted and not confirmed is given back: the server's copy plus these
    // blocks cover the whole of what `write` took
    let confirmed = seen.written.lock().unwrap().clone();
    let mut check = confirmed.clone();
    check.resize(at, 0);
    for (off, block) in &back {
        let end = *off as usize + block.len();
        assert!(end <= at);
        check[*off as usize..end].copy_from_slice(block);
    }
    assert!(check == data[..at], "{} bytes confirmed, {at} taken, {} blocks back", confirmed.len(), back.len());

    // a new connection takes them and goes on
    let seen2 = Seen::default();
    *seen2.written.lock().unwrap() = confirmed;
    let sftp2 = connected(Script::default(), seen2.clone());
    let mut w2 = sftp2.writer(&path(), Some(at as u64)).unwrap();
    w2.resend(back).unwrap();
    w2.write_all(&data[at..]).unwrap();
    w2.finish().unwrap();
    assert!(*seen2.written.lock().unwrap() == data);
}

#[test]
fn a_server_that_answers_slowly_is_never_cut_off_reading() {
    let data = file(5 << 20);
    // each reply takes 150 ms, one after the other: replies keep coming for about 3 s while
    // the requests behind them wait far longer than the watchdog's second
    let sftp = connected_with(Script { data: data.clone(), reply_delay: Some(Duration::from_millis(150)), ..Default::default() }, Seen::default(), Duration::from_secs(1));
    let mut r = sftp.reader(&path()).unwrap();
    let start = Instant::now();
    assert!(read_all(&mut r).unwrap() == data);
    assert!(start.elapsed() > Duration::from_secs(1), "the test must outlast the watchdog");
}

#[test]
fn a_server_that_answers_slowly_is_never_cut_off_writing() {
    let seen = Seen::default();
    let data = file(5 << 20);
    let sftp = connected_with(Script { reply_delay: Some(Duration::from_millis(150)), ..Default::default() }, seen.clone(), Duration::from_secs(1));
    let mut w = sftp.writer(&path(), None).unwrap();
    let start = Instant::now();
    w.write_all(&data).unwrap();
    w.finish().unwrap();
    assert!(*seen.written.lock().unwrap() == data);
    assert!(start.elapsed() > Duration::from_secs(1), "the test must outlast the watchdog");
}

#[test]
fn a_session_quiet_for_longer_than_the_watchdog_is_not_stalled() {
    let sftp = connected_with(Script { data: file(1000), ..Default::default() }, Seen::default(), Duration::from_millis(300));
    assert!(sftp.stat(&path()).is_ok());
    // nothing waits for an answer now: the time without replies does not count
    std::thread::sleep(Duration::from_millis(700));
    assert!(sftp.stat(&path()).is_ok());
}

#[test]
fn a_slow_copy_behind_the_read_ahead_is_not_a_stalled_server() {
    let data = file(6 << 20);
    let sftp = connected_with(Script { data: data.clone(), ..Default::default() }, Seen::default(), Duration::from_millis(400));
    let mut r = sftp.reader(&path()).unwrap();
    let mut buf = Vec::new();
    let mut out = Vec::new();
    r.next_block(&mut buf).unwrap();
    out.extend_from_slice(&buf);
    // a paused job: the queue is full, the read-ahead waits, the server has nothing to say
    std::thread::sleep(Duration::from_millis(1200));
    while r.next_block(&mut buf).unwrap() > 0 {
        out.extend_from_slice(&buf);
    }
    assert!(out == data);
}

#[test]
fn an_upload_does_not_pile_up_behind_a_slow_wire() {
    let sftp = connected(Script { stop_reading_at: Some(1 << 20), ..Default::default() }, Seen::default());
    let handle = "h".to_string();
    let writer = SftpWriter::new(&sftp, handle, 0);
    let (shared, written) = (Arc::clone(&writer.shared), Arc::clone(&sftp.sftp.watch));
    std::thread::spawn(move || {
        let mut writer = writer;
        let data = file(1 << 20);
        loop {
            if writer.write_all(&data).is_err() {
                return;
            }
        }
    });
    std::thread::sleep(Duration::from_millis(800));
    let s = shared.state.lock().unwrap();
    let unsent = s.handed.saturating_sub(written.written.load(Ordering::Relaxed));
    assert!(unsent <= MAX_UNSENT + BLOCK as u64 + 512, "{unsent} bytes wait to leave");
    assert!(s.in_flight < WRITES_IN_FLIGHT, "held by what waits to leave, not by the window: {}", s.in_flight);
}

#[test]
fn a_server_that_only_limits_its_packets_gets_uploads_that_fit() {
    let seen = Seen::default();
    let sftp = connected(Script { limits: Some((0, 0)), packet_limit: Some(64 * 1024), ..Default::default() }, seen.clone());
    assert_eq!(sftp.features.write_block, 64 * 1024 - WRITE_OVERHEAD as usize);
    assert_eq!(sftp.features.read_block, 64 * 1024 - READ_OVERHEAD as u32);
    let data = file(1 << 20);
    let mut w = sftp.writer(&path(), None).unwrap();
    w.write_all(&data).unwrap();
    w.finish().unwrap();
    assert!(*seen.written.lock().unwrap() == data);
}

#[test]
fn rsa_keys_ask_for_sha2_before_sha1() {
    assert_eq!(rsa_hashes(Some(Some(HashAlg::Sha512))), vec![Some(HashAlg::Sha512)]);
    // the server says it only takes the old one: asked for just that
    assert_eq!(rsa_hashes(Some(None)), vec![None]);
    // it says nothing: SHA-256 first, SHA-1 only when that fails
    assert_eq!(rsa_hashes(None), vec![Some(HashAlg::Sha256), None]);
}

#[test]
fn ed25519_comes_first_and_ssh_rsa_last_for_host_keys() {
    use russh::keys::Algorithm;
    let config = client_config();
    let keys = config.preferred.key.as_ref();
    assert_eq!(keys.first(), Some(&Algorithm::Ed25519));
    assert_eq!(keys.last(), Some(&Algorithm::Rsa { hash: None }));
    assert!(keys.iter().any(|k| matches!(k, Algorithm::Rsa { hash: Some(_) })));
}

#[test]
fn posix_rename_replaces_where_the_server_has_it() {
    let seen = Seen::default();
    let sftp = connected(Script { posix_rename: true, ..Default::default() }, seen.clone());
    assert!(sftp.replace(&path(), &path()).unwrap());
    assert_eq!(*seen.posix_renames.lock().unwrap(), 1);
    // not advertised: nothing is sent, the caller goes through a hidden name
    let seen = Seen::default();
    let sftp = connected(Script::default(), seen.clone());
    assert!(!sftp.replace(&path(), &path()).unwrap());
    assert_eq!(*seen.posix_renames.lock().unwrap(), 0);
}

#[test]
fn dropping_files_outside_the_runtime_does_not_panic() {
    let data = file(1 << 20);
    let sftp = connected(Script { data, ..Default::default() }, Seen::default());
    let mut r = sftp.reader(&path()).unwrap();
    let mut buf = vec![0u8; 10];
    r.read_exact(&mut buf).unwrap();
    let w = sftp.writer(&path(), None).unwrap();
    // plain thread: no runtime around it, the regression of 867d330
    std::thread::spawn(move || {
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(r);
        drop(w);
    })
    .join()
    .unwrap();
    assert!(sftp.stat(&path()).is_ok(), "the connection stays usable");
}

#[test]
fn dropping_files_on_a_dead_connection_is_quick() {
    let data = file(1 << 20);
    let sftp = connected(Script { data, drop_read_at: Some(0), ..Default::default() }, Seen::default());
    let r = sftp.reader(&path()).unwrap();
    let start = Instant::now();
    let mut r = r;
    let _ = read_all(&mut r);
    drop(r);
    assert!(start.elapsed() < CLOSE_LIMIT + Duration::from_secs(2), "took {:?}", start.elapsed());
}
