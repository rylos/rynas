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
    let (client, server) = tokio::io::duplex(1 << 20);
    let rt = runtime().unwrap();
    rt.spawn(serve(server, script, seen));
    let (sftp, features) = rt.block_on(start_session(client)).unwrap();
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
    let sftp = connected(Script { data, stall_read_at: Some(2 << 20), ..Default::default() }, Seen::default());
    sftp.sftp.set_timeout(1);
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
    let sftp = connected(Script { fsync_delay: Some(Duration::from_secs(2)), ..Default::default() }, seen.clone());
    // the session gives each request a second; fsync takes two
    sftp.sftp.set_timeout(1);
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
