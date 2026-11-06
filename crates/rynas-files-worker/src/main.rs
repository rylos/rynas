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

fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if mode == "version" {
        println!("rynas-files-worker {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
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
