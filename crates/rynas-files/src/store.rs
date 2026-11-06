//! What the file manager keeps for each user, in rynasd's state directory
//! (`/var/lib/rynas/files/`, root 0700): only rynasd reads and writes it, workers never
//! see it (they get what a request needs in its [`crate::Context`]).
//!
//! ```text
//! files/
//!   conns.key            the AES-256-GCM key that seals the connections' secrets (0600)
//!   <user>/prefs.json    the interface's preferences (view, sort, favourites, icons...)
//!   <user>/conns.json    remote connections, secrets sealed
//!   <user>/id_ed25519    the user's own SSH key for SFTP servers
//!   journal/<id>.json    transfer jobs not finished yet, no secrets (jobs::manager)
//! ```

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value, json};

static BASE: OnceLock<PathBuf> = OnceLock::new();

/// Sets the state directory (rynasd, once at start). Workers never call it: without it the
/// stores are empty and nothing is ever written.
pub fn init(base: impl Into<PathBuf>) {
    let base = base.into();
    let _ = std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&base);
    let _ = BASE.set(base);
}

/// The state directory, when this process keeps the stores.
pub fn base() -> Option<&'static Path> {
    BASE.get().map(PathBuf::as_path)
}

/// A user's directory name: user names are already checked by rynas
/// (`[a-z_][a-z0-9_.-]*`), the replacement is a second guard.
fn user_dir(user: &str) -> Option<PathBuf> {
    let safe = user.replace(['/', '\0'], "_");
    if safe.is_empty() || safe.starts_with('.') {
        return None;
    }
    Some(base()?.join(safe))
}

/// `<base>/<user>/<name>`
pub fn user_file(user: &str, name: &str) -> Option<PathBuf> {
    Some(user_dir(user)?.join(name))
}

/// Largest preferences document.
pub const MAX_PREFS: usize = 256 * 1024;

pub fn read_prefs(user: &str) -> Value {
    user_file(user, "prefs.json").and_then(|f| std::fs::read(f).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}))
}

pub fn write_prefs(user: &str, value: &Value) -> io::Result<()> {
    let text = serde_json::to_vec(value).map_err(io::Error::other)?;
    if text.len() > MAX_PREFS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "preferences too large"));
    }
    let file = user_file(user, "prefs.json").ok_or_else(|| io::Error::other("no state directory"))?;
    write_private(&file, &text)
}

/// Writes `data` to `path` all at once (temporary file, then rename), 0600, creating the
/// directory 0700.
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = write_temp(path, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// `data` in a new file beside `path`, mode 0600, named after this process and thread so
/// two writers never share it.
pub fn write_temp(path: &Path, data: &[u8]) -> io::Result<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    std::fs::DirBuilder::new().mode(0o700).recursive(true).create(dir)?;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let _ = std::fs::remove_file(&tmp);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(data).and_then(|()| f.sync_all()));
    match written {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_writes_replace_whole_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u/x.json");
        write_private(&path, b"one").unwrap();
        write_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path().join("u")).unwrap().count(), 1);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(dir.path().join("u")).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
