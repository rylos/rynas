//! Encrypted shared folders (D-CRYPT E1, `docs/phase2/encrypted-folders.md`): gocryptfs
//! from the Debian archive.
//!
//! A folder `<place>/<name>` keeps its ciphertext in `<place>/.rynas-crypt/<name>` (a
//! subvolume on btrfs) and is mounted on `<place>/<name>` (a plain folder, mode 000,
//! immutable) while unlocked. Mounting never happens inside rynasd: unlock starts the
//! transient service `rynas-crypt-<escaped path>` through [`rynas_tasks::Outside::daemon`]
//! with the password as a systemd credential, lock unmounts through [`rynas_tasks::Outside`].
//!
//! Secrets travel on standard input or in the 0600 credential file that lives only for the
//! start of the unit; they are never in argv, the environment, a log or an error. The
//! master key (the recovery key) is generated here and returned once by [`CryptHost::create`].
//!
//! [`HostCrypt`] works on the machine, [`MemoryCrypt`] in tests and `rynasd --dry-system`.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::{CryptInfo, CryptState, CryptStatus};
use rynas_tasks::{Cmd, CmdOutput, Outside};
use sha2::{Digest, Sha256};

use crate::accounts::Secret;
use crate::host::SubvolumeCreator;

/// Folder below a place that holds the ciphertext directories (0700 root).
pub const CRYPT_DIR: &str = ".rynas-crypt";
pub const GOCRYPTFS: &str = "/usr/bin/gocryptfs";
/// The configuration file of a gocryptfs ciphertext directory.
pub const CONF: &str = "gocryptfs.conf";
/// What `gocryptfs -passwd` leaves behind, wrapped with the **old** password.
pub const CONF_BAK: &str = "gocryptfs.conf.bak";
/// Prefix of the mount units.
pub const UNIT_PREFIX: &str = "rynas-crypt";
/// Credential files that live for the start of a mount unit (tmpfs, 0700 root).
pub const RUN_DIR: &str = "/run/rynas/crypt";
/// Debian package.
pub const PACKAGE: &str = "gocryptfs";
pub const PASSWORD_MIN: usize = 8;
/// gocryptfs reads at most 2048 bytes; rynas keeps well below.
pub const PASSWORD_MAX: usize = 1024;
/// `gocryptfs` exit code for a wrong password.
pub const EXIT_WRONG_PASSWORD: i32 = 12;
const CMD_TIMEOUT: Duration = Duration::from_secs(60);
/// Bytes of a gocryptfs master key.
const KEY_LEN: usize = 32;

/// Why an operation failed. The texts never carry a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptError {
    /// Wrong password or recovery key.
    WrongPassword,
    /// Still in use: not locked.
    Busy(String),
    /// gocryptfs is not installed.
    NotInstalled,
    /// The request does not fit the folder (exists already, not an encrypted folder...).
    Invalid(String),
    /// A command failed.
    Failed(String),
}

impl std::fmt::Display for CryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongPassword => f.write_str("wrong password or recovery key"),
            Self::Busy(m) => write!(f, "the folder is in use: {m}"),
            Self::NotInstalled => f.write_str("gocryptfs is not installed"),
            Self::Invalid(m) | Self::Failed(m) => f.write_str(m),
        }
    }
}

impl From<String> for CryptError {
    fn from(s: String) -> Self {
        Self::Failed(s)
    }
}

// ---------------------------------------------------------------------------------------------
// layout and command lines (pure)
// ---------------------------------------------------------------------------------------------

/// `<parent>/.rynas-crypt/<name>` for the mount point `<parent>/<name>`; `None` for a path
/// that cannot be one (empty, `/`, a hidden name).
pub fn cipher_dir(path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    let (parent, name) = p.rsplit_once('/')?;
    if !p.starts_with('/') || name.is_empty() || name.starts_with('.') || name == ".." {
        return None;
    }
    Some(format!("{parent}/{CRYPT_DIR}/{name}"))
}

/// The key-check file of a folder, next to its ciphertext (`<cipher>.check`).
fn check_file(cipher: &str) -> String {
    format!("{cipher}.check")
}

/// `systemd-escape --path` of `path`.
pub fn escape_path(path: &str) -> String {
    let p = path.trim_matches('/');
    let mut out = String::new();
    for (i, b) in p.bytes().enumerate() {
        match b {
            b'/' => out.push('-'),
            b'.' if i == 0 => out.push_str("\\x2e"),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'.' => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// The mount unit of the folder at `path` (no `.service`).
pub fn unit_name(path: &str) -> String {
    format!("{UNIT_PREFIX}-{}", escape_path(path))
}

/// A file-name-safe stem for the folder's credential and key files.
pub fn file_stem(path: &str) -> String {
    let h = Sha256::digest(path.trim_end_matches('/').as_bytes());
    let hex: String = h.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!("{UNIT_PREFIX}-{hex}")
}

/// A password rynas accepts: 8–1024 bytes, one line (gocryptfs reads up to the newline).
pub fn check_password(p: &str) -> Result<(), String> {
    if p.chars().count() < PASSWORD_MIN {
        return Err(format!("the password needs at least {PASSWORD_MIN} characters"));
    }
    if p.len() > PASSWORD_MAX {
        return Err(format!("the password is longer than {PASSWORD_MAX} bytes"));
    }
    if p.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        return Err("the password cannot contain line breaks".into());
    }
    Ok(())
}

/// 32 random bytes as gocryptfs prints a master key: 8 groups of 8 hex digits.
pub fn new_master_key() -> Secret {
    let mut b = [0u8; KEY_LEN];
    getrandom::fill(&mut b).expect("the operating system random source failed");
    let s = group_hex(&b);
    b.fill(0);
    s
}

fn group_hex(b: &[u8]) -> Secret {
    let mut s = String::with_capacity(KEY_LEN * 2 + 7);
    for (i, x) in b.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            s.push('-');
        }
        s.push_str(&format!("{x:02x}"));
    }
    Secret::new(s)
}

/// A recovery key as typed (dashes, spaces, any case) in gocryptfs' form; `None` when it is
/// not 64 hex digits.
pub fn normalize_master_key(k: &str) -> Option<Secret> {
    let hex: String = k
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t' | '\n' | '\r'))
        .collect::<String>()
        .to_ascii_lowercase();
    let hex = Secret::new(hex);
    let h = hex.expose();
    if h.len() != KEY_LEN * 2 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; KEY_LEN];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    let s = group_hex(&bytes);
    bytes.fill(0);
    Some(s)
}

/// What rynas keeps to recognise the right recovery key: SHA-256 over a label and the key
/// bytes. A 256-bit random key cannot be found from it.
pub fn key_check(master: &Secret) -> String {
    let hex: String = master.expose().chars().filter(|c| *c != '-').collect();
    let hex = Secret::new(hex);
    let mut h = Sha256::new();
    h.update(b"rynas-crypt-key-check\0");
    h.update(hex.expose().as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// `gocryptfs -init` with our master key and the password on standard input.
pub fn init_cmd(cipher: &str, master: &Secret, password: &Secret) -> Cmd {
    Cmd::new(GOCRYPTFS)
        .args(["-init", "-masterkey", "stdin", "--", cipher])
        .stdin(format!("{}\n{}\n", master.expose(), password.expose()))
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// `gocryptfs -passwd`: the current password (or with `recovery` the master key), then the
/// new password, on standard input.
pub fn passwd_cmd(cipher: &str, current: &Secret, recovery: bool, new: &Secret) -> Cmd {
    let mut c = Cmd::new(GOCRYPTFS).arg("-passwd");
    if recovery {
        c = c.args(["-masterkey", "stdin"]);
    }
    c.args(["--", cipher])
        .stdin(format!("{}\n{}\n", current.expose(), new.expose()))
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// The gocryptfs command line of the mount unit; the password comes from the unit's
/// credential `pass`.
pub fn mount_argv(cipher: &str, mount: &str, btrfs: bool) -> Vec<String> {
    let mut v: Vec<String> = [GOCRYPTFS, "-allow_other", "-acl"].map(String::from).to_vec();
    if btrfs {
        // copy-on-write: preallocation guarantees nothing on btrfs and costs write speed
        v.push("-noprealloc".into());
    }
    v.extend(
        ["-passfile", "${CREDENTIALS_DIRECTORY}/pass", "--", cipher, mount].map(String::from),
    );
    v
}

/// `systemd-run` of the mount unit with the credential file `cred`.
pub fn mount_cmd(path: &str, cipher: &str, cred: &str, btrfs: bool) -> Cmd {
    let unit = unit_name(path);
    let props = [
        format!("LoadCredential=pass:{cred}"),
        "TimeoutStopSec=30".to_string(),
        format!("RequiresMountsFor={cipher}"),
    ];
    let props: Vec<&str> = props.iter().map(String::as_str).collect();
    Outside::new(&unit, "rynas: encrypted folder", CMD_TIMEOUT)
        .properties(&props)
        .daemon(mount_argv(cipher, path, btrfs))
}

/// The unmount of a lock: never lazy, so a folder in use stays mounted.
pub fn umount_cmd(path: &str) -> Cmd {
    Outside::new("rynas-crypt-lock", "rynas: lock an encrypted folder", Duration::from_secs(30))
        .cmd(["umount", path])
}

/// `chattr ±i` on the mount point (needs `CAP_LINUX_IMMUTABLE`: outside the sandbox).
pub fn immutable_cmd(path: &str, on: bool) -> Cmd {
    Outside::new("rynas-crypt-mountpoint", "rynas: encrypted folder mount point", Duration::from_secs(30))
        .cmd(["chattr", if on { "+i" } else { "-i" }, path])
}

/// `systemd-creds encrypt` of the password (standard input) into `file`, bound to `name`.
pub fn seal_cmd(name: &str, file: &str, password: &Secret) -> Cmd {
    Cmd::new("systemd-creds")
        .args(["encrypt", &format!("--name={name}"), "-", file])
        .stdin(password.expose().as_bytes().to_vec())
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// `systemd-creds decrypt` of `file` to standard output.
pub fn unseal_cmd(name: &str, file: &str) -> Cmd {
    Cmd::new("systemd-creds")
        .args(["decrypt", &format!("--name={name}"), file, "-"])
        .env_clear()
        .timeout(CMD_TIMEOUT)
}

/// The exit status of the start command of a unit from `systemctl show -p ExecStart
/// --value` (`{ path=… ; … ; code=exited ; status=12 }`; for a forking service the start
/// command is not the main process, so `ExecMainStatus` stays 0) or from
/// `ExecMainStatus=`.
pub fn parse_exit_status(show: &str) -> Option<i32> {
    let i = show.rfind("status=")?;
    let digits: String = show[i + 7..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Unescapes the octal escapes of a mountinfo field (`\040` for a space).
fn unescape_mountinfo(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let oct = std::str::from_utf8(oct).unwrap_or("077");
                out.push(u8::from_str_radix(oct, 8).unwrap_or(b'?'));
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Is the last mount on `path` in this mountinfo text a gocryptfs one?
pub fn mounted_in(mountinfo: &str, path: &str) -> bool {
    let want = path.trim_end_matches('/');
    let mut last = None;
    for l in mountinfo.lines() {
        let Some((left, right)) = l.split_once(" - ") else { continue };
        let Some(mp) = left.split(' ').nth(4) else { continue };
        if unescape_mountinfo(mp) != want {
            continue;
        }
        last = right.split(' ').next().map(str::to_string);
    }
    last.is_some_and(|t| t == "fuse.gocryptfs")
}

/// `gocryptfs 2.5.1; go-fuse 2.5.1; 2025-01-23 go1.24.1 linux/amd64` → `2.5.1`.
pub fn parse_version(out: &str) -> Option<String> {
    let first = out.lines().next()?.split(';').next()?;
    let v = first.trim().strip_prefix("gocryptfs")?.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// The end of a command's error output, for an error text (never a secret: gocryptfs and
/// systemd do not print them).
fn tail(o: &CmdOutput) -> String {
    let t = if o.stderr.trim().is_empty() { &o.stdout } else { &o.stderr };
    t.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(300)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// the backend trait
// ---------------------------------------------------------------------------------------------

/// Encrypted folders on a system. `path` is always the mount point `<place>/<name>`.
pub trait CryptHost: Send + Sync {
    fn info(&self) -> CryptInfo;
    fn state(&self, path: &str) -> CryptState;
    /// A sealed password exists: the folder is unlocked when rynasd starts.
    fn auto_unlock(&self, path: &str) -> bool;
    /// Creates, initialises and unlocks a new folder; returns its recovery key.
    fn create(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<Secret, CryptError>;
    fn unlock(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError>;
    fn lock(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError>;
    /// A new password from the current one, or from the recovery key with `recovery`.
    fn change_password(
        &self,
        path: &str,
        current: &Secret,
        recovery: bool,
        new: &Secret,
        log: &dyn Fn(&str),
    ) -> Result<(), CryptError>;
    /// `Some`: checks the password and seals it on this machine; `None`: forgets it.
    fn set_auto_unlock(&self, path: &str, password: Option<&Secret>, log: &dyn Fn(&str)) -> Result<(), CryptError>;
    /// The sealed password, when there is one.
    fn sealed_password(&self, path: &str) -> Result<Option<Secret>, CryptError>;
    /// Locks the folder and deletes its ciphertext, its mount point and its keys.
    fn remove(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError>;

    /// The folders live in memory (tests, `--dry-system`): nothing is installed or mounted.
    fn simulated(&self) -> bool {
        false
    }

    /// A simulated install finished (no-op on the machine, where the package tells).
    fn mark_installed(&self) {}

    /// [`rynas_api_types::ShareStatus::crypt`] of an encrypted share.
    fn status(&self, path: &str) -> CryptStatus {
        CryptStatus {
            state: self.state(path),
            auto_unlock: self.auto_unlock(path),
            cipher_dir: cipher_dir(path).unwrap_or_default(),
        }
    }
}

fn need_cipher(path: &str) -> Result<String, CryptError> {
    cipher_dir(path).ok_or_else(|| CryptError::Invalid(format!("{path} cannot be an encrypted folder")))
}

// ---------------------------------------------------------------------------------------------
// the machine
// ---------------------------------------------------------------------------------------------

/// rynasd as root on the machine.
pub struct HostCrypt {
    /// Creates the ciphertext subvolume (the same creator as the shares').
    pub subvolumes: Option<Arc<dyn SubvolumeCreator>>,
    /// rynasd's state directory: sealed passwords in `<state>/crypt` (0700).
    pub state_dir: PathBuf,
    /// One change at a time (two unlocks of one folder, a lock during a create...).
    pub busy: Mutex<()>,
}

impl HostCrypt {
    pub fn new(subvolumes: Option<Arc<dyn SubvolumeCreator>>, state_dir: &Path) -> Self {
        Self { subvolumes, state_dir: state_dir.to_path_buf(), busy: Mutex::new(()) }
    }

    fn key_file(&self, path: &str) -> PathBuf {
        self.state_dir.join("crypt").join(format!("{}.cred", file_stem(path)))
    }

    fn installed(&self) -> bool {
        Path::new(GOCRYPTFS).exists()
    }

    fn mounted(&self, path: &str) -> bool {
        mounted_in(&std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default(), path)
    }

    fn systemctl(&self, args: &[&str]) -> Result<CmdOutput, String> {
        Cmd::new("systemctl")
            .args(args)
            .timeout(Duration::from_secs(30))
            .output_blocking()
            .map_err(|e| format!("systemctl: {e}"))
    }

    fn run(&self, c: &Cmd, log: &dyn Fn(&str), what: &str) -> Result<CmdOutput, CryptError> {
        log(&format!("$ {}", c.display()));
        c.output_blocking().map_err(|e| CryptError::Failed(format!("{what}: {e}")))
    }

    fn unlock_inner(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        match self.state(path) {
            CryptState::Unlocked => return Ok(()),
            CryptState::Missing => return Err(CryptError::Invalid(format!("{path} is not an encrypted folder"))),
            CryptState::Locked => {}
        }
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        let cipher = need_cipher(path)?;
        let unit = unit_name(path);
        let cred = write_credential(path, password)?;
        let _ = self.systemctl(&["reset-failed", &format!("{unit}.service")]);
        let btrfs = std::fs::metadata(&cipher).is_ok_and(|m| m.ino() == 256);
        let cmd = mount_cmd(path, &cipher, &cred.to_string_lossy(), btrfs);
        log(&format!("$ {}", cmd.display()));
        let r = cmd.output_blocking();
        // the credential was copied into the unit (or the start failed): gone either way
        wipe(&cred);
        let ok = matches!(&r, Ok(o) if o.success());
        if ok {
            for _ in 0..40 {
                if self.mounted(path) {
                    log(&format!("{path} unlocked ({unit})"));
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            return Err(CryptError::Failed(format!("{unit} started but {path} is not mounted: see journalctl -u {unit}")));
        }
        let svc = format!("{unit}.service");
        let status = self
            .systemctl(&["show", "-p", "ExecStart", "--value", &svc])
            .ok()
            .and_then(|o| parse_exit_status(&o.stdout))
            .filter(|s| *s != 0)
            .or_else(|| {
                self.systemctl(&["show", "-p", "ExecMainStatus", &svc])
                    .ok()
                    .and_then(|o| parse_exit_status(&o.stdout.replace("ExecMainStatus=", "status=")))
            });
        let _ = self.systemctl(&["reset-failed", &svc]);
        if status == Some(EXIT_WRONG_PASSWORD) {
            return Err(CryptError::WrongPassword);
        }
        let detail = match r {
            Ok(o) => tail(&o),
            Err(e) => e.to_string(),
        };
        Err(CryptError::Failed(format!(
            "gocryptfs did not start (exit status {}): {detail}; see journalctl -u {unit}",
            status.map(|s| s.to_string()).unwrap_or_else(|| "?".into())
        )))
    }

    fn lock_inner(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        match self.state(path) {
            CryptState::Unlocked => {}
            CryptState::Locked => return Ok(()),
            CryptState::Missing => return Err(CryptError::Invalid(format!("{path} is not an encrypted folder"))),
        }
        let o = self.run(&umount_cmd(path), log, "umount")?;
        if !o.success() {
            let t = tail(&o);
            return Err(if t.to_ascii_lowercase().contains("busy") {
                CryptError::Busy(t)
            } else {
                CryptError::Failed(format!("umount {path}: {t}"))
            });
        }
        // gocryptfs ends by itself once its filesystem is gone
        let svc = format!("{}.service", unit_name(path));
        for _ in 0..100 {
            if self.systemctl(&["is-active", "--quiet", &svc]).map(|o| !o.success()).unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.systemctl(&["reset-failed", &svc]);
        if self.mounted(path) {
            return Err(CryptError::Failed(format!("{path} is still mounted")));
        }
        log(&format!("{path} locked"));
        Ok(())
    }

    /// Deletes a ciphertext directory (subvolume or plain folder) and its key check.
    fn delete_cipher(&self, cipher: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        if let Ok(m) = std::fs::symlink_metadata(cipher) {
            if m.file_type().is_symlink() {
                return Err(CryptError::Invalid(format!("{cipher} is a symbolic link: not deleted")));
            }
            if m.ino() == 256 {
                let o = self.run(&Cmd::new("btrfs").args(["subvolume", "delete", cipher]), log, "btrfs")?;
                if !o.success() {
                    return Err(CryptError::Failed(format!("btrfs subvolume delete {cipher}: {}", tail(&o))));
                }
            } else {
                log(&format!("rm -r {cipher}"));
                std::fs::remove_dir_all(cipher).map_err(|e| CryptError::Failed(format!("{cipher}: {e}")))?;
            }
        }
        let _ = std::fs::remove_file(check_file(cipher));
        if let Some(parent) = Path::new(cipher).parent() {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(())
    }

    fn remove_mountpoint(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        match std::fs::symlink_metadata(path) {
            Err(_) => Ok(()),
            Ok(m) if !m.is_dir() => Err(CryptError::Invalid(format!("{path} is not a folder: not deleted"))),
            Ok(_) => {
                let o = self.run(&immutable_cmd(path, false), log, "chattr")?;
                if !o.success() {
                    return Err(CryptError::Failed(format!("chattr -i {path}: {}", tail(&o))));
                }
                std::fs::remove_dir(path).map_err(|e| CryptError::Failed(format!("rmdir {path}: {e}")))
            }
        }
    }

    fn seal(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let file = self.key_file(path);
        let dir = file.parent().expect("key files live in a folder");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| CryptError::Failed(format!("{}: {e}", dir.display())))?;
        let f = file.to_string_lossy().into_owned();
        let o = self.run(&seal_cmd(&file_stem(path), &f, password), log, "systemd-creds")?;
        if !o.success() {
            let _ = std::fs::remove_file(&file);
            return Err(CryptError::Failed(format!("systemd-creds encrypt: {}", tail(&o))));
        }
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        Ok(())
    }
}

/// Writes the password to a fresh 0600 file in [`RUN_DIR`] (never following a link).
fn write_credential(path: &str, password: &Secret) -> Result<PathBuf, CryptError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(RUN_DIR)
        .map_err(|e| CryptError::Failed(format!("{RUN_DIR}: {e}")))?;
    let f = Path::new(RUN_DIR).join(format!("{}.pass", file_stem(path)));
    // a file left by a crash: replaced
    let _ = std::fs::remove_file(&f);
    let mut h = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&f)
        .map_err(|e| CryptError::Failed(format!("{}: {e}", f.display())))?;
    h.write_all(password.expose().as_bytes())
        .and_then(|_| h.write_all(b"\n"))
        .map_err(|e| CryptError::Failed(format!("{}: {e}", f.display())))?;
    Ok(f)
}

/// Overwrites a credential file with zeros and deletes it.
fn wipe(f: &Path) {
    if let Ok(m) = std::fs::symlink_metadata(f)
        && m.is_file()
        && let Ok(mut h) = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(f)
    {
        let _ = h.write_all(&vec![0u8; m.len() as usize]);
        let _ = h.sync_all();
    }
    let _ = std::fs::remove_file(f);
}

impl CryptHost for HostCrypt {
    fn info(&self) -> CryptInfo {
        let installed = self.installed();
        let version = installed
            .then(|| Cmd::new(GOCRYPTFS).arg("-version").env_clear().timeout(Duration::from_secs(10)).output_blocking().ok())
            .flatten()
            .and_then(|o| parse_version(&o.stdout));
        CryptInfo {
            installed,
            version,
            tpm2: Path::new("/sys/class/tpm/tpm0").exists(),
        }
    }

    fn state(&self, path: &str) -> CryptState {
        let Some(cipher) = cipher_dir(path) else { return CryptState::Missing };
        if self.mounted(path) {
            CryptState::Unlocked
        } else if Path::new(&cipher).join(CONF).is_file() {
            CryptState::Locked
        } else {
            CryptState::Missing
        }
    }

    fn auto_unlock(&self, path: &str) -> bool {
        self.key_file(path).is_file()
    }

    fn create(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<Secret, CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        check_password(password.expose()).map_err(CryptError::Invalid)?;
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        let cipher = need_cipher(path)?;
        if std::fs::symlink_metadata(path).is_ok() {
            return Err(CryptError::Invalid(format!("{path} already exists")));
        }
        if std::fs::symlink_metadata(&cipher).is_ok() {
            return Err(CryptError::Invalid(format!("{cipher} already exists")));
        }
        let parent = Path::new(&cipher).parent().expect("cipher dir has a parent");
        match std::fs::symlink_metadata(parent) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(CryptError::Invalid(format!("{} is not a folder", parent.display())));
            }
            Ok(_) => {}
            Err(_) => {
                log(&format!("mkdir -m 700 {}", parent.display()));
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(parent)
                    .map_err(|e| CryptError::Failed(format!("{}: {e}", parent.display())))?;
            }
        }
        // ciphertext does not compress
        match &self.subvolumes {
            Some(c) => c.create(&cipher, false, false, log)?,
            None => {
                log(&format!("mkdir {cipher}"));
                std::fs::DirBuilder::new()
                    .mode(0o755)
                    .create(&cipher)
                    .map_err(|e| CryptError::Failed(format!("{cipher}: {e}")))?;
            }
        }
        let master = new_master_key();
        let undo = |log: &dyn Fn(&str)| {
            let _ = self.remove_mountpoint(path, log);
            let _ = self.delete_cipher(&cipher, log);
        };
        log(&format!("gocryptfs -init {cipher} (password and master key on standard input)"));
        let o = match init_cmd(&cipher, &master, password).output_blocking() {
            Ok(o) => o,
            Err(e) => {
                undo(log);
                return Err(CryptError::Failed(format!("gocryptfs -init: {e}")));
            }
        };
        if !o.success() {
            undo(log);
            return Err(CryptError::Failed(format!("gocryptfs -init: {}", tail(&o))));
        }
        if let Err(e) = std::fs::write(check_file(&cipher), format!("{}\n", key_check(&master))) {
            log(&format!("warning: {}: {e}", check_file(&cipher)));
        }
        log(&format!("mkdir -m 000 {path}"));
        if let Err(e) = std::fs::DirBuilder::new().mode(0o000).create(path) {
            undo(log);
            return Err(CryptError::Failed(format!("{path}: {e}")));
        }
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000));
        match self.run(&immutable_cmd(path, true), log, "chattr") {
            Ok(o) if o.success() => {}
            Ok(o) => {
                undo(log);
                return Err(CryptError::Failed(format!("chattr +i {path}: {}", tail(&o))));
            }
            Err(e) => {
                undo(log);
                return Err(e);
            }
        }
        self.unlock_inner(path, password, log)?;
        Ok(master)
    }

    fn unlock(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        self.unlock_inner(path, password, log)
    }

    fn lock(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        self.lock_inner(path, log)
    }

    fn change_password(
        &self,
        path: &str,
        current: &Secret,
        recovery: bool,
        new: &Secret,
        log: &dyn Fn(&str),
    ) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        check_password(new.expose()).map_err(CryptError::Invalid)?;
        if self.state(path) == CryptState::Missing {
            return Err(CryptError::Invalid(format!("{path} is not an encrypted folder")));
        }
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        let cipher = need_cipher(path)?;
        let current = if recovery {
            // -passwd -masterkey trusts any key: a wrong one would lock the files out for
            // good. The key check made at creation tells.
            let k = normalize_master_key(current.expose()).ok_or(CryptError::WrongPassword)?;
            let want = std::fs::read_to_string(check_file(&cipher)).map_err(|_| {
                CryptError::Invalid(format!(
                    "{path} has no key check (it was not made by rynas): reset its password with gocryptfs -passwd -masterkey"
                ))
            })?;
            if want.trim() != key_check(&k) {
                return Err(CryptError::WrongPassword);
            }
            k
        } else {
            current.clone()
        };
        log(&format!("gocryptfs -passwd {cipher} (passwords on standard input)"));
        let o = passwd_cmd(&cipher, &current, recovery, new)
            .output_blocking()
            .map_err(|e| CryptError::Failed(format!("gocryptfs -passwd: {e}")))?;
        if o.code == Some(EXIT_WRONG_PASSWORD) {
            return Err(CryptError::WrongPassword);
        }
        if !o.success() {
            return Err(CryptError::Failed(format!("gocryptfs -passwd: {}", tail(&o))));
        }
        // the backup is wrapped with the old password
        let bak = Path::new(&cipher).join(CONF_BAK);
        if bak.exists() {
            log(&format!("removing {}", bak.display()));
            let _ = std::fs::remove_file(&bak);
        }
        if self.auto_unlock(path) {
            log("sealing the new password for the automatic unlock");
            self.seal(path, new, log)?;
        }
        Ok(())
    }

    fn set_auto_unlock(&self, path: &str, password: Option<&Secret>, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = password else {
            let f = self.key_file(path);
            if f.exists() {
                log(&format!("removing {}", f.display()));
                std::fs::remove_file(&f).map_err(|e| CryptError::Failed(format!("{}: {e}", f.display())))?;
            }
            return Ok(());
        };
        if self.state(path) == CryptState::Missing {
            return Err(CryptError::Invalid(format!("{path} is not an encrypted folder")));
        }
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        // the only check gocryptfs offers without mounting: re-wrap the key with the same
        // password (a fresh salt, the folder's files are untouched)
        let cipher = need_cipher(path)?;
        let o = passwd_cmd(&cipher, p, false, p)
            .output_blocking()
            .map_err(|e| CryptError::Failed(format!("gocryptfs -passwd: {e}")))?;
        if o.code == Some(EXIT_WRONG_PASSWORD) {
            return Err(CryptError::WrongPassword);
        }
        if !o.success() {
            return Err(CryptError::Failed(format!("gocryptfs -passwd: {}", tail(&o))));
        }
        let _ = std::fs::remove_file(Path::new(&cipher).join(CONF_BAK));
        self.seal(path, p, log)
    }

    fn sealed_password(&self, path: &str) -> Result<Option<Secret>, CryptError> {
        let f = self.key_file(path);
        if !f.is_file() {
            return Ok(None);
        }
        let o = unseal_cmd(&file_stem(path), &f.to_string_lossy())
            .output_blocking()
            .map_err(|e| CryptError::Failed(format!("systemd-creds decrypt: {e}")))?;
        if !o.success() {
            return Err(CryptError::Failed(format!("systemd-creds decrypt: {}", tail(&o))));
        }
        let s = Secret::new(o.stdout.trim_end_matches('\n'));
        let mut out = o.stdout;
        // SAFETY: zero bytes are valid UTF-8; the string is dropped right after.
        unsafe {
            for b in out.as_bytes_mut() {
                std::ptr::write_volatile(b, 0);
            }
        }
        Ok(Some(s))
    }

    fn remove(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        let _g = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        let cipher = need_cipher(path)?;
        self.lock_inner(path, log).or_else(|e| match e {
            // nothing to lock
            CryptError::Invalid(_) => Ok(()),
            e => Err(e),
        })?;
        self.remove_mountpoint(path, log)?;
        self.delete_cipher(&cipher, log)?;
        let _ = std::fs::remove_file(self.key_file(path));
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// in memory (tests, --dry-system)
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct MemFolder {
    password: Secret,
    master: Secret,
    unlocked: bool,
    sealed: bool,
    busy: bool,
}

/// Encrypted folders kept in memory. With [`MemoryCrypt::with_shares`] the folders it creates
/// appear in that in-memory share host, so shares can be made on them.
#[derive(Default)]
pub struct MemoryCrypt {
    folders: Mutex<BTreeMap<String, MemFolder>>,
    installed: std::sync::atomic::AtomicBool,
    shares: Option<Arc<crate::host::MemoryShares>>,
    /// Every operation (`create /p`, `unlock /p`...), never with a secret.
    pub log: Mutex<Vec<String>>,
}

impl MemoryCrypt {
    pub fn new(installed: bool) -> Self {
        let m = Self::default();
        m.set_installed(installed);
        m
    }

    pub fn with_shares(mut self, shares: Arc<crate::host::MemoryShares>) -> Self {
        self.shares = Some(shares);
        self
    }

    pub fn set_installed(&self, on: bool) {
        self.installed.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// A locked folder at `path` with `password` (tests, demo data).
    pub fn add_locked(&self, path: &str, password: &str) {
        self.folders.lock().unwrap().insert(
            path.into(),
            MemFolder {
                password: Secret::new(password),
                master: new_master_key(),
                unlocked: false,
                sealed: false,
                busy: false,
            },
        );
        if let Some(s) = &self.shares {
            s.add_dir(path, None);
        }
    }

    /// Makes a lock of `path` fail as busy.
    pub fn set_busy(&self, path: &str, busy: bool) {
        if let Some(f) = self.folders.lock().unwrap().get_mut(path) {
            f.busy = busy;
        }
    }

    fn note(&self, s: String) {
        self.log.lock().unwrap().push(s);
    }

    fn with<T>(&self, path: &str, f: impl FnOnce(&mut MemFolder) -> Result<T, CryptError>) -> Result<T, CryptError> {
        let mut m = self.folders.lock().unwrap();
        let x = m
            .get_mut(path)
            .ok_or_else(|| CryptError::Invalid(format!("{path} is not an encrypted folder")))?;
        f(x)
    }

    fn installed(&self) -> bool {
        self.installed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl CryptHost for MemoryCrypt {
    fn simulated(&self) -> bool {
        true
    }

    fn mark_installed(&self) {
        self.set_installed(true);
    }

    fn info(&self) -> CryptInfo {
        CryptInfo {
            installed: self.installed(),
            version: self.installed().then(|| "2.5.1".into()),
            tpm2: false,
        }
    }

    fn state(&self, path: &str) -> CryptState {
        match self.folders.lock().unwrap().get(path) {
            None => CryptState::Missing,
            Some(f) if f.unlocked => CryptState::Unlocked,
            Some(_) => CryptState::Locked,
        }
    }

    fn auto_unlock(&self, path: &str) -> bool {
        self.folders.lock().unwrap().get(path).is_some_and(|f| f.sealed)
    }

    fn create(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<Secret, CryptError> {
        check_password(password.expose()).map_err(CryptError::Invalid)?;
        if !self.installed() {
            return Err(CryptError::NotInstalled);
        }
        let cipher = need_cipher(path)?;
        let mut m = self.folders.lock().unwrap();
        if m.contains_key(path) || self.shares.as_ref().is_some_and(|s| crate::host::ShareHost::probe(s.as_ref(), path).exists) {
            return Err(CryptError::Invalid(format!("{path} already exists")));
        }
        let master = new_master_key();
        m.insert(
            path.into(),
            MemFolder { password: password.clone(), master: master.clone(), unlocked: true, sealed: false, busy: false },
        );
        drop(m);
        if let Some(s) = &self.shares {
            s.add_dir(path, None);
        }
        log(&format!("created {path} (ciphertext {cipher}) and unlocked it (in memory)"));
        self.note(format!("create {path}"));
        Ok(master)
    }

    fn unlock(&self, path: &str, password: &Secret, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        self.with(path, |f| {
            if f.password != *password {
                return Err(CryptError::WrongPassword);
            }
            f.unlocked = true;
            Ok(())
        })?;
        log(&format!("{path} unlocked (in memory)"));
        self.note(format!("unlock {path}"));
        Ok(())
    }

    fn lock(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        self.with(path, |f| {
            if f.unlocked && f.busy {
                return Err(CryptError::Busy("target is busy".into()));
            }
            f.unlocked = false;
            Ok(())
        })?;
        log(&format!("{path} locked (in memory)"));
        self.note(format!("lock {path}"));
        Ok(())
    }

    fn change_password(
        &self,
        path: &str,
        current: &Secret,
        recovery: bool,
        new: &Secret,
        _log: &dyn Fn(&str),
    ) -> Result<(), CryptError> {
        check_password(new.expose()).map_err(CryptError::Invalid)?;
        self.with(path, |f| {
            let ok = if recovery {
                normalize_master_key(current.expose()).is_some_and(|k| k == f.master)
            } else {
                f.password == *current
            };
            if !ok {
                return Err(CryptError::WrongPassword);
            }
            f.password = new.clone();
            Ok(())
        })?;
        self.note(format!("password {path}"));
        Ok(())
    }

    fn set_auto_unlock(&self, path: &str, password: Option<&Secret>, _log: &dyn Fn(&str)) -> Result<(), CryptError> {
        self.with(path, |f| {
            if let Some(p) = password
                && f.password != *p
            {
                return Err(CryptError::WrongPassword);
            }
            f.sealed = password.is_some();
            Ok(())
        })?;
        self.note(format!("auto-unlock {path} {}", password.is_some()));
        Ok(())
    }

    fn sealed_password(&self, path: &str) -> Result<Option<Secret>, CryptError> {
        Ok(self
            .folders
            .lock()
            .unwrap()
            .get(path)
            .filter(|f| f.sealed)
            .map(|f| f.password.clone()))
    }

    fn remove(&self, path: &str, log: &dyn Fn(&str)) -> Result<(), CryptError> {
        self.lock(path, log).or_else(|e| match e {
            CryptError::Invalid(_) => Ok(()),
            e => Err(e),
        })?;
        self.folders.lock().unwrap().remove(path);
        if let Some(s) = &self.shares {
            s.state().paths.remove(path);
        }
        log(&format!("{path} and its ciphertext deleted (in memory)"));
        self.note(format!("remove {path}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        assert_eq!(cipher_dir("/vol1/docs").as_deref(), Some("/vol1/.rynas-crypt/docs"));
        assert_eq!(cipher_dir("/vol1/docs/").as_deref(), Some("/vol1/.rynas-crypt/docs"));
        assert_eq!(
            cipher_dir("/srv/rynas/system/private").as_deref(),
            Some("/srv/rynas/system/.rynas-crypt/private")
        );
        assert_eq!(cipher_dir(""), None);
        assert_eq!(cipher_dir("/"), None);
        assert_eq!(cipher_dir("/vol1/.hidden"), None);
        assert_eq!(cipher_dir("relative/x"), None);
        assert_eq!(unit_name("/vol1/docs"), "rynas-crypt-vol1-docs");
        assert_eq!(unit_name("/srv/rynas/system/my-docs"), "rynas-crypt-srv-rynas-system-my\\x2ddocs");
        assert_eq!(escape_path("/a b/.c"), "a\\x20b-.c");
        assert_eq!(escape_path("/.x"), "\\x2ex");
        let s = file_stem("/vol1/docs");
        assert!(s.starts_with("rynas-crypt-") && s.len() == "rynas-crypt-".len() + 24, "{s}");
        assert_eq!(s, file_stem("/vol1/docs/"));
        assert_ne!(s, file_stem("/vol1/docs2"));
    }

    #[test]
    fn passwords() {
        assert!(check_password("correct horse").is_ok());
        assert!(check_password("short").is_err());
        assert!(check_password("eight\nchars").is_err());
        assert!(check_password(&"x".repeat(PASSWORD_MAX + 1)).is_err());
        // characters, not bytes, for the minimum
        assert!(check_password("ééééééé").is_err());
        assert!(check_password("éééééééé").is_ok());
    }

    #[test]
    fn master_keys() {
        let k = new_master_key();
        let s = k.expose();
        assert_eq!(s.len(), 64 + 7);
        assert_eq!(s.split('-').count(), 8);
        assert!(s.split('-').all(|g| g.len() == 8 && g.bytes().all(|b| b.is_ascii_hexdigit())));
        assert_ne!(new_master_key(), k);
        // typed back with spaces, upper case, no dashes
        let typed = s.replace('-', " ").to_ascii_uppercase();
        assert_eq!(normalize_master_key(&typed), Some(k.clone()));
        assert_eq!(normalize_master_key(&s.replace('-', "")), Some(k.clone()));
        assert_eq!(normalize_master_key("abc"), None);
        assert_eq!(normalize_master_key(&"g".repeat(64)), None);
        assert_eq!(key_check(&k), key_check(&normalize_master_key(&typed).unwrap()));
        assert_ne!(key_check(&k), key_check(&new_master_key()));
        assert_eq!(key_check(&k).len(), 64);
        assert!(!key_check(&k).contains(&s.replace('-', "")[..16]));
        // never printed
        assert_eq!(format!("{k:?}"), "<secret>");
    }

    #[test]
    fn secrets_stay_off_the_command_lines() {
        let pw = Secret::new("correct horse battery");
        let mk = new_master_key();
        let init = init_cmd("/vol1/.rynas-crypt/docs", &mk, &pw);
        let d = init.display();
        assert_eq!(d, "/usr/bin/gocryptfs -init -masterkey stdin -- /vol1/.rynas-crypt/docs");
        assert!(!d.contains("horse") && !d.contains(mk.expose()));
        assert_eq!(init.stdin_data().unwrap(), format!("{}\ncorrect horse battery\n", mk.expose()).as_bytes());
        let dbg = format!("{init:?}");
        assert!(!dbg.contains("horse") && !dbg.contains(mk.expose()), "{dbg}");

        let new = Secret::new("new password here");
        let p = passwd_cmd("/c", &pw, false, &new);
        assert_eq!(p.display(), "/usr/bin/gocryptfs -passwd -- /c");
        assert_eq!(p.stdin_data().unwrap(), b"correct horse battery\nnew password here\n");
        let r = passwd_cmd("/c", &mk, true, &new);
        assert_eq!(r.display(), "/usr/bin/gocryptfs -passwd -masterkey stdin -- /c");
        assert!(!r.display().contains(mk.expose()));

        let m = mount_cmd("/vol1/docs", "/vol1/.rynas-crypt/docs", "/run/rynas/crypt/x.pass", true);
        let d = m.display();
        assert!(d.starts_with("systemd-run --quiet --service-type=forking --unit=rynas-crypt-vol1-docs "), "{d}");
        assert!(d.contains("--property=LoadCredential=pass:/run/rynas/crypt/x.pass"), "{d}");
        assert!(d.ends_with(
            "-- /usr/bin/gocryptfs -allow_other -acl -noprealloc -passfile ${CREDENTIALS_DIRECTORY}/pass -- /vol1/.rynas-crypt/docs /vol1/docs"
        ), "{d}");
        assert!(m.stdin_data().is_none());
        let plain = mount_argv("/c", "/m", false);
        assert!(!plain.contains(&"-noprealloc".to_string()));

        let u = umount_cmd("/vol1/docs").display();
        assert!(u.contains("systemd-run --wait --pipe") && u.ends_with("-- umount /vol1/docs"), "{u}");
        assert!(!u.contains("-l "), "never lazy: {u}");
        assert!(immutable_cmd("/vol1/docs", true).display().ends_with("-- chattr +i /vol1/docs"));
        assert!(immutable_cmd("/vol1/docs", false).display().ends_with("-- chattr -i /vol1/docs"));

        let s = seal_cmd("rynas-crypt-x", "/var/lib/rynas/crypt/x.cred", &pw);
        assert_eq!(s.display(), "systemd-creds encrypt --name=rynas-crypt-x - /var/lib/rynas/crypt/x.cred");
        assert_eq!(s.stdin_data().unwrap(), b"correct horse battery");
        assert_eq!(
            unseal_cmd("rynas-crypt-x", "/f").display(),
            "systemd-creds decrypt --name=rynas-crypt-x /f -"
        );
    }

    #[test]
    fn exit_status_and_version() {
        let show = "{ path=/usr/bin/gocryptfs ; argv[]=/usr/bin/gocryptfs -allow_other ; ignore_errors=no ; start_time=[n/a] ; stop_time=[Sun 2026-11-08] ; pid=4242 ; code=exited ; status=12 }";
        assert_eq!(parse_exit_status(show), Some(12));
        assert_eq!(parse_exit_status("status=0/SUCCESS"), Some(0));
        assert_eq!(parse_exit_status("ExecMainStatus=1".replace("ExecMainStatus=", "status=").as_str()), Some(1));
        assert_eq!(parse_exit_status(""), None);
        assert_eq!(
            parse_version("gocryptfs 2.5.1; go-fuse 2.5.1; 2025-01-23 go1.24.1 linux/amd64\n").as_deref(),
            Some("2.5.1")
        );
        assert_eq!(parse_version("something else"), None);
    }

    #[test]
    fn mountinfo() {
        let mi = "\
36 25 0:31 / /vol1 rw,noatime shared:1 - btrfs /dev/md0 rw,space_cache=v2,subvolid=5,subvol=/
90 36 0:52 / /vol1/docs rw,nosuid,nodev,relatime shared:50 - fuse.gocryptfs /vol1/.rynas-crypt/docs rw,user_id=0,group_id=0,default_permissions,allow_other
91 36 0:53 / /vol1/my\\040files rw shared:51 - fuse.gocryptfs /vol1/.rynas-crypt/my\\040files rw
92 36 0:54 / /vol1/other rw shared:52 - fuse.sshfs host: rw
";
        assert!(mounted_in(mi, "/vol1/docs"));
        assert!(mounted_in(mi, "/vol1/docs/"));
        assert!(mounted_in(mi, "/vol1/my files"));
        assert!(!mounted_in(mi, "/vol1"));
        assert!(!mounted_in(mi, "/vol1/other"));
        assert!(!mounted_in(mi, "/vol1/none"));
        // something else mounted over it later wins
        let over = format!("{mi}93 90 0:60 / /vol1/docs rw - tmpfs tmpfs rw\n");
        assert!(!mounted_in(&over, "/vol1/docs"));
    }

    #[test]
    fn memory_state_machine() {
        let m = MemoryCrypt::new(false);
        let pw = Secret::new("correct horse");
        let log = |_: &str| {};
        assert_eq!(m.create("/vol1/docs", &pw, &log), Err(CryptError::NotInstalled));
        m.set_installed(true);
        assert!(matches!(m.create("/vol1/docs", &Secret::new("short"), &log), Err(CryptError::Invalid(_))));
        let key = m.create("/vol1/docs", &pw, &log).unwrap();
        assert!(matches!(m.create("/vol1/docs", &pw, &log), Err(CryptError::Invalid(_))));
        assert_eq!(m.state("/vol1/docs"), CryptState::Unlocked);
        assert_eq!(m.state("/vol1/other"), CryptState::Missing);
        m.set_busy("/vol1/docs", true);
        assert!(matches!(m.lock("/vol1/docs", &log), Err(CryptError::Busy(_))));
        m.set_busy("/vol1/docs", false);
        m.lock("/vol1/docs", &log).unwrap();
        assert_eq!(m.state("/vol1/docs"), CryptState::Locked);
        m.lock("/vol1/docs", &log).unwrap();
        assert_eq!(m.unlock("/vol1/docs", &Secret::new("wrong password"), &log), Err(CryptError::WrongPassword));
        m.unlock("/vol1/docs", &pw, &log).unwrap();
        // password change with the recovery key, then the old password fails
        let new = Secret::new("brand new password");
        assert_eq!(
            m.change_password("/vol1/docs", &Secret::new("0".repeat(64)), true, &new, &log),
            Err(CryptError::WrongPassword)
        );
        let typed = Secret::new(key.expose().replace('-', " "));
        m.change_password("/vol1/docs", &typed, true, &new, &log).unwrap();
        m.lock("/vol1/docs", &log).unwrap();
        assert_eq!(m.unlock("/vol1/docs", &pw, &log), Err(CryptError::WrongPassword));
        assert_eq!(m.set_auto_unlock("/vol1/docs", Some(&pw), &log), Err(CryptError::WrongPassword));
        m.set_auto_unlock("/vol1/docs", Some(&new), &log).unwrap();
        assert!(m.auto_unlock("/vol1/docs"));
        assert_eq!(m.sealed_password("/vol1/docs").unwrap(), Some(new.clone()));
        let st = m.status("/vol1/docs");
        assert_eq!(st.state, CryptState::Locked);
        assert!(st.auto_unlock);
        assert_eq!(st.cipher_dir, "/vol1/.rynas-crypt/docs");
        m.set_auto_unlock("/vol1/docs", None, &log).unwrap();
        assert_eq!(m.sealed_password("/vol1/docs").unwrap(), None);
        m.remove("/vol1/docs", &log).unwrap();
        assert_eq!(m.state("/vol1/docs"), CryptState::Missing);
        let ops = m.log.lock().unwrap().join(",");
        assert!(!ops.contains("horse") && !ops.contains("password here"), "{ops}");
    }

    #[test]
    fn credential_file_is_private_and_wiped() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.pass");
        {
            let mut h = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&f).unwrap();
            h.write_all(b"secret\n").unwrap();
        }
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        wipe(&f);
        assert!(!f.exists());
        // a link is never followed
        let target = d.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &f).unwrap();
        wipe(&f);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    }
}
