//! A file's checksum, computed on the NAS for the info panel (route `checksum`).
//!
//! The answer is a stream of JSON lines (`application/x-ndjson`): `{"done":N,"size":S}`
//! about every half second, then `{"hash":"…"}` or `{"error":"…","code":"…"}`. A big file
//! takes minutes, and a proxy that hears nothing for a minute may close the request; the
//! lines also drive the interface's progress bar. A client that goes away ends the run:
//! rynasd kills the worker when the answer is dropped, and a write that finds the pipe
//! closed stops the loop first.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::wire::{self as cgi, ApiError};

/// Read size: large sequential reads, what a disk or an array likes best.
const CHUNK: usize = 1 << 20;
/// How often a progress line goes out.
const TICK: Duration = Duration::from_millis(500);

/// The algorithms offered: sha256, sha1, md5, xxh128.
pub enum Hasher {
    Sha256(sha2::Sha256),
    Sha1(sha1::Sha1),
    Md5(md5::Md5),
    Xxh128(Box<xxhash_rust::xxh3::Xxh3>),
}

impl Hasher {
    /// The hasher named by the route's `algo`, or `None` for an unknown name.
    pub fn new(algo: &str) -> Option<Self> {
        Some(match algo {
            "sha256" => Self::Sha256(<sha2::Sha256 as sha2::Digest>::new()),
            "sha1" => Self::Sha1(<sha1::Sha1 as sha1::Digest>::new()),
            "md5" => Self::Md5(<md5::Md5 as md5::Digest>::new()),
            "xxh128" => Self::Xxh128(Box::default()),
            _ => return None,
        })
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(h) => sha2::Digest::update(h, data),
            Self::Sha1(h) => sha1::Digest::update(h, data),
            Self::Md5(h) => md5::Digest::update(h, data),
            Self::Xxh128(h) => h.update(data),
        }
    }

    /// The digest in lowercase hex.
    pub fn hex(self) -> String {
        match self {
            Self::Sha256(h) => hex(&sha2::Digest::finalize(h)),
            Self::Sha1(h) => hex(&sha1::Digest::finalize(h)),
            Self::Md5(h) => hex(&md5::Digest::finalize(h)),
            // as `xxhsum -H2` prints it: the 128 bits, big-endian
            Self::Xxh128(h) => hex(&h.digest128().to_be_bytes()),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Answers `checksum` for `file` (already placed inside the user's share): a file only,
/// never a folder; the archive file itself is a file like any other.
pub fn send(out: &mut impl Write, file: &Path, algo: &str) -> Result<(), ApiError> {
    let hasher = Hasher::new(algo).ok_or_else(|| ApiError::invalid("algo", t!("unknown algorithm: {algo}")))?;
    let input = crate::confine::open(file)?;
    let meta = input.metadata()?;
    if !meta.is_file() {
        return Err(ApiError::invalid("path", t!("a checksum is computed on files only")));
    }
    // read once from start to end: the kernel may read ahead further
    // SAFETY: a valid descriptor, no memory handed over
    unsafe { libc::posix_fadvise(input.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
    cgi::head(out, 200, &[("Content-Type", "application/x-ndjson"), ("X-Accel-Buffering", "no")])?;
    out.flush()?;
    let line = match hash(input, hasher, meta.len(), out) {
        Ok(hash) => json!({ "hash": hash }),
        // the client went away
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
        Err(e) => {
            let err = ApiError::from(e);
            json!({ "error": err.message, "code": err.code })
        }
    };
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    Ok(())
}

/// Reads `input` to its end into `hasher`, telling `out` how far it got every [`TICK`].
fn hash(mut input: impl Read, mut hasher: Hasher, size: u64, out: &mut impl Write) -> io::Result<String> {
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    let mut told = Instant::now();
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        done += n as u64;
        if told.elapsed() >= TICK {
            told = Instant::now();
            writeln!(out, "{}", json!({ "done": done, "size": size }))?;
            out.flush()?;
        }
    }
    Ok(hasher.hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(algo: &str, data: &[u8]) -> String {
        let mut h = Hasher::new(algo).unwrap();
        h.update(data);
        h.hex()
    }

    #[test]
    fn known_vectors() {
        assert_eq!(of("sha256", b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(of("sha1", b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(of("md5", b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        // `xxhsum -H2` of an empty file and of a file holding "abc": big-endian
        assert_eq!(of("xxh128", b""), "99aa06d3014798d86001c324468d497f");
        assert_eq!(of("xxh128", b"abc"), "06b05ab6733a618578af5f94892f3950");
        assert!(Hasher::new("crc32").is_none());
    }

    #[test]
    fn streams_progress_then_hash() {
        // the outputs of sha256sum, sha1sum, md5sum and xxhsum -H2 for this file
        let data = b"rynas checksum test\n";
        for (algo, want) in [
            ("sha256", "7115fcff93260338d3cf50015de50ac66275b6781fb9aa5e985fc88f2819a658"),
            ("sha1", "f64c473f92526177ce7a7096e43912a74964f066"),
            ("md5", "9050d4f64a65a6880a02fe4a14fa056e"),
            ("xxh128", "4c1a4dea776ee65ad45e7b6bb4aaf11a"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("t.txt");
            std::fs::write(&file, data).unwrap();
            let mut out = Vec::new();
            send(&mut out, &file, algo).unwrap();
            let text = String::from_utf8(out).unwrap();
            let (head, body) = text.split_once("\r\n\r\n").unwrap();
            assert!(head.contains("Content-Type: application/x-ndjson") && head.contains("X-Accel-Buffering: no"), "{head}");
            let last: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
            assert_eq!(last["hash"], want, "{algo}");
        }
    }

    #[test]
    fn refuses_folders_and_unknown_algorithms() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        assert_eq!(send(&mut out, dir.path(), "sha256").unwrap_err().status, 400);
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        assert_eq!(send(&mut out, &dir.path().join("f"), "sha512").unwrap_err().status, 400);
        assert!(out.is_empty(), "nothing is written before the answer is known");
    }
}
