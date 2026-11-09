//! rynas-files-worker: started by rynasd, already switched to the logged-in user
//! (`rynas_files::identity::become_raw`), for
//! - `serve`: one file manager request (`rynas_files::api::serve`), the remote routes
//!   answered here ([`remote::api`]);
//! - `job`: one transfer job ([`jobs::worker`]);
//! - `call`: one SSH chore rynasd keeps out of its own process (learning a server's key,
//!   making or opening a private key), JSON in, JSON out.
//!
//! It refuses to run as root or with any capability but reading (administrators).

/// A message (English); the interface translates codes.
macro_rules! t {
    ($($arg:tt)*) => { format!($($arg)*) };
}

// the core's modules under their SynoFile names, so the ported code reads as before
pub use rynas_files::{archive, edit, fsops, names, shares, transfer, wire, zip};

pub mod jobs;
pub mod remote;

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

/// The remote routes of `serve`, over the SMB and SFTP clients of this crate.
struct Remote;

impl rynas_files::api::Remote for Remote {
    fn serve(&self, req: &wire::Request, mut out: &mut dyn Write, conns: &[remote::Conn]) -> Option<Result<(), wire::ApiError>> {
        remote::api::serve(req, &mut out, conns)
    }
    fn test(&self, req: &wire::Request, mut out: &mut dyn Write, conns: &[remote::Conn]) -> Result<(), wire::ApiError> {
        remote::api::test(req, &mut out, conns)
    }
    fn usage(&self, mut out: &mut dyn Write, conns: &[remote::Conn], paths: &[String]) -> Result<(), wire::ApiError> {
        remote::api::usage(&mut out, conns, paths)
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
enum Call {
    ServerKey { conn: Box<remote::Conn> },
    NewKey { comment: String },
    ImportKey { text: String, passphrase: Option<String> },
}

/// How long the first look at an SFTP server's key may take: rynasd waits on it.
const PIN_TIMEOUT: Duration = Duration::from_secs(5);

fn call() -> ExitCode {
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return ExitCode::FAILURE;
    }
    let answer = match serde_json::from_str::<Call>(&line) {
        Err(e) => serde_json::json!({ "error": format!("bad call: {e}") }),
        Ok(Call::ServerKey { conn }) => match remote::sftp::server_key(&conn, PIN_TIMEOUT) {
            Ok(key) => serde_json::json!({ "key": key }),
            Err(e) => serde_json::json!({ "error": fsops::describe(&e) }),
        },
        Ok(Call::NewKey { comment }) => match remote::sftp::new_key().and_then(|p| Ok((remote::sftp::public_key(&p, &comment)?, p))) {
            Ok((public, private)) => serde_json::json!({ "private": private, "public": public }),
            Err(e) => serde_json::json!({ "error": fsops::describe(&e) }),
        },
        Ok(Call::ImportKey { text, passphrase }) => match remote::sftp::import_key(&text, passphrase.as_deref()) {
            Ok((private, info)) => serde_json::json!({ "private": private, "info": info }),
            Err(e) => serde_json::json!({ "error": e }),
        },
    };
    let mut out = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, &answer);
    let _ = out.write_all(b"\n");
    ExitCode::SUCCESS
}

/// The speed of the SSH ciphers on this CPU, with the library russh encrypts with (ring), on
/// 32 KiB buffers: what `packaging/bench-sftp.sh` prints before measuring the connection. No
/// other use.
fn bench_ciphers() -> ExitCode {
    use ring::aead::{AES_128_GCM, AES_256_GCM, Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
    let cases: [(&str, &'static ring::aead::Algorithm, usize); 3] = [("aes128-gcm", &AES_128_GCM, 16), ("aes256-gcm", &AES_256_GCM, 32), ("chacha20-poly1305", &CHACHA20_POLY1305, 32)];
    #[cfg(target_arch = "x86_64")]
    println!("cpu features: aes={} pclmulqdq={} avx={} ssse3={}", std::arch::is_x86_feature_detected!("aes"), std::arch::is_x86_feature_detected!("pclmulqdq"), std::arch::is_x86_feature_detected!("avx"), std::arch::is_x86_feature_detected!("ssse3"));
    for (name, algo, key_len) in cases {
        let key = LessSafeKey::new(UnboundKey::new(algo, &vec![7u8; key_len]).expect("key length"));
        let mut block = vec![7u8; 32 * 1024];
        let (mut seal_bytes, mut seq) = (0u64, 0u64);
        let t = std::time::Instant::now();
        while t.elapsed() < Duration::from_secs(1) {
            seq += 1;
            let mut nonce = [0u8; 12];
            nonce[4..].copy_from_slice(&seq.to_be_bytes());
            let _ = key.seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut block).expect("seal");
            seal_bytes += block.len() as u64;
        }
        let seal = seal_bytes as f64 / t.elapsed().as_secs_f64();
        // what the client of a download does: authenticate and decrypt
        let mut sealed = vec![7u8; 32 * 1024];
        let nonce = [1u8; 12];
        let tag = key.seal_in_place_separate_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut sealed).expect("seal");
        sealed.extend_from_slice(tag.as_ref());
        let (mut open_bytes, mut work) = (0u64, sealed.clone());
        let t = std::time::Instant::now();
        while t.elapsed() < Duration::from_secs(1) {
            work.copy_from_slice(&sealed);
            let plain = key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut work).expect("open");
            open_bytes += plain.len() as u64;
        }
        let open = open_bytes as f64 / t.elapsed().as_secs_f64();
        println!("{name}: encrypt {:.0} MB/s, decrypt {:.0} MB/s", seal / 1e6, open / 1e6);
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "version" {
        println!("rynas-files-worker {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if mode == "bench-ciphers" {
        return bench_ciphers();
    }
    if let Err(e) = rynas_files::caps::check_worker() {
        eprintln!("rynas-files-worker: {e}");
        return ExitCode::FAILURE;
    }
    // what the worker makes is the user's; a share's default ACL decides the rest (the
    // same as Samba with its create masks of 0666/0777)
    // SAFETY: umask has no preconditions
    unsafe { libc::umask(0o022) };
    match mode.as_str() {
        "serve" => match rynas_files::api::serve(&Remote) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        },
        "job" => jobs::worker::run(),
        "call" => call(),
        _ => {
            eprintln!("usage: rynas-files-worker serve | job | call | version (started by rynasd)");
            ExitCode::from(2)
        }
    }
}
