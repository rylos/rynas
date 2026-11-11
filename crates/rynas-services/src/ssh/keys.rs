//! `authorized_keys` files and host keys: parsing, fingerprints (`SHA256:…` as `ssh-keygen
//! -lf` prints them) and changes made without following links.
//!
//! The home and its `.ssh` belong to the user, who may replace anything in them with a
//! symbolic link at any time. So the file is reached through directory descriptors opened
//! with `O_NOFOLLOW` (`openat`), written to a new file in the same `.ssh` and renamed over the
//! old one (`renameat` on the same descriptor): root never writes through a user's link.

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use rynas_api_types::ssh::{SshHostKey, SshKey};
use sha2::{Digest, Sha256};

const FILE: &str = "authorized_keys";
const TMP: &str = ".authorized_keys.rynas-new";
/// Larger files are refused (a key line is under 1 KiB, a big RSA key under 2 KiB).
const MAX_FILE: u64 = 1 << 20;

/// A key type sshd knows (plain keys, security keys, certificates).
fn is_kind(w: &str) -> bool {
    w.starts_with("ssh-") || w.starts_with("ecdsa-sha2-") || w.starts_with("sk-")
}

/// SSH wire `string`s of a key blob.
fn strings(mut b: &[u8]) -> Vec<&[u8]> {
    let mut v = Vec::new();
    while b.len() >= 4 {
        let n = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize;
        if b.len() < 4 + n {
            break;
        }
        v.push(&b[4..4 + n]);
        b = &b[4 + n..];
    }
    v
}

fn mpint_bits(m: &[u8]) -> u32 {
    let m: Vec<u8> = m.iter().copied().skip_while(|b| *b == 0).collect();
    match m.first() {
        None => 0,
        Some(f) => (m.len() as u32 - 1) * 8 + (8 - f.leading_zeros()),
    }
}

/// `(type inside the blob, bits)` of a decoded key blob.
fn blob_info(blob: &[u8]) -> Option<(String, u32)> {
    let s = strings(blob);
    let kind = std::str::from_utf8(s.first()?).ok()?.to_string();
    let bits = match kind.as_str() {
        "ssh-rsa" => mpint_bits(s.get(2)?),
        "ssh-dss" => mpint_bits(s.get(1)?),
        "ssh-ed25519" | "sk-ssh-ed25519@openssh.com" => 256,
        "ecdsa-sha2-nistp256" | "sk-ecdsa-sha2-nistp256@openssh.com" => 256,
        "ecdsa-sha2-nistp384" => 384,
        "ecdsa-sha2-nistp521" => 521,
        k if k.contains("-cert-") => 0,
        _ => return None,
    };
    Some((kind, bits))
}

pub fn fingerprint(blob: &[u8]) -> String {
    format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(blob)))
}

/// One key of a line: `(options?, kind, blob, comment)`; `None` for comments, blanks and
/// lines that are not keys.
fn split_line(line: &str) -> Option<(bool, &str, &str, &str)> {
    let l = line.trim();
    if l.is_empty() || l.starts_with('#') {
        return None;
    }
    let (options, rest) = match l.split_whitespace().next() {
        Some(w) if is_kind(w) => (false, l),
        _ => {
            // options: up to the first blank outside double quotes
            let mut quoted = false;
            let mut end = l.len();
            for (i, c) in l.char_indices() {
                match c {
                    '"' => quoted = !quoted,
                    c if c.is_whitespace() && !quoted => {
                        end = i;
                        break;
                    }
                    _ => {}
                }
            }
            (true, l[end..].trim_start())
        }
    };
    let mut it = rest.splitn(3, char::is_whitespace);
    let kind = it.next().filter(|k| is_kind(k))?;
    let blob = it.next()?;
    let comment = it.next().unwrap_or("").trim();
    Some((options, kind, blob, comment))
}

/// The key of one `authorized_keys` line.
pub fn parse_line(line: &str) -> Option<SshKey> {
    let (options, kind, b64, comment) = split_line(line)?;
    let blob = STANDARD.decode(b64).ok()?;
    let (_, bits) = blob_info(&blob)?;
    Some(SshKey {
        kind: kind.to_string(),
        bits,
        fingerprint: fingerprint(&blob),
        comment: comment.to_string(),
        options,
        weak: kind == "ssh-dss" || (kind == "ssh-rsa" && bits < 2048),
    })
}

/// Keys and the number of other non-blank lines of a file.
pub fn parse_file(text: &str) -> (Vec<SshKey>, u32) {
    let mut keys = Vec::new();
    let mut other = 0;
    for l in text.lines() {
        match parse_line(l) {
            Some(k) => keys.push(k),
            None if !l.trim().is_empty() && !l.trim_start().starts_with('#') => other += 1,
            None => {}
        }
    }
    (keys, other)
}

/// A key to add: one public key line without options, a type that matches its blob, not
/// weak; returns the clean line (`kind blob comment`) and the key.
pub fn check_new(line: &str) -> Result<(String, SshKey), String> {
    let line = line.trim();
    if line.contains(['\n', '\r']) {
        return Err("one key at a time".into());
    }
    if line.len() > 16 * 1024 {
        return Err("this is not a public key (too long)".into());
    }
    let Some((options, kind, b64, comment)) = split_line(line) else {
        return Err("not a public key: paste the line of the .pub file (ssh-ed25519 AAAA… comment)".into());
    };
    if options {
        return Err("options before the key are not accepted here".into());
    }
    let blob = STANDARD.decode(b64).map_err(|_| "the key is damaged (not base64)".to_string())?;
    let (inner, _) = blob_info(&blob).ok_or_else(|| format!("unknown key type {kind}"))?;
    if inner != kind {
        return Err(format!("the key says {inner}, the line says {kind}"));
    }
    if comment.chars().any(char::is_control) {
        return Err("the comment has control characters".into());
    }
    let clean = if comment.is_empty() { format!("{kind} {b64}") } else { format!("{kind} {b64} {comment}") };
    let key = parse_line(&clean).ok_or("not a public key")?;
    if key.weak {
        return Err(format!("{kind} keys of {} bits are refused: make an ed25519 key (ssh-keygen -t ed25519)", key.bits));
    }
    if kind.contains("-cert-") {
        return Err("certificates are not accepted here".into());
    }
    Ok((clean, key))
}

/// The file without the lines of the key `fp`; `None` when no line has it.
pub fn without(text: &str, fp: &str) -> Option<String> {
    let mut out = String::new();
    let mut found = false;
    for l in text.lines() {
        if parse_line(l).is_some_and(|k| k.fingerprint == fp) {
            found = true;
            continue;
        }
        out.push_str(l);
        out.push('\n');
    }
    found.then_some(out)
}

/// A host key from its `.pub` file.
pub fn host_key(pubfile: &str) -> Option<SshHostKey> {
    let mut w = pubfile.split_whitespace();
    let kind = w.next()?;
    let blob = STANDARD.decode(w.next()?).ok()?;
    let (inner, bits) = blob_info(&blob)?;
    let label = match inner.as_str() {
        "ssh-ed25519" => "ED25519",
        "ssh-rsa" => "RSA",
        "ssh-dss" => "DSA",
        k if k.starts_with("ecdsa-") => "ECDSA",
        _ => kind,
    };
    Some(SshHostKey { kind: label.into(), bits, fingerprint: fingerprint(&blob) })
}

// ------------------------------------------------------------------ files without links

fn cstr(s: &str) -> CString {
    CString::new(s).expect("fixed names")
}

fn openat(dir: RawFd, name: &str, flags: libc::c_int, mode: libc::mode_t) -> io::Result<OwnedFd> {
    let n = cstr(name);
    // SAFETY: valid C string; the descriptor returned is owned here.
    let fd = unsafe { libc::openat(dir, n.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: zeroed stat filled by the kernel.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The `.ssh` folder of a home (made `0700` for the user when `create`); `None` when absent.
/// The home must belong to the user or root and not be writable by others (sshd's
/// `StrictModes` would ignore the keys anyway).
fn open_ssh(home: &Path, uid: u32, gid: u32, create: bool) -> Result<Option<OwnedFd>, String> {
    let shown = home.display();
    // the home itself may be a link made by root (`/srv/rynas/homes/<name>`)
    let real = std::fs::canonicalize(home).map_err(|e| format!("{shown}: {e}"))?;
    let real_s = real.to_str().ok_or_else(|| format!("{shown}: not UTF-8"))?;
    let home_fd = openat(libc::AT_FDCWD, real_s, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|e| format!("{shown}: {e}"))?;
    let st = fstat(home_fd.as_raw_fd()).map_err(|e| format!("{shown}: {e}"))?;
    if st.st_uid != uid && st.st_uid != 0 {
        return Err(format!("{shown} does not belong to the user"));
    }
    if st.st_mode & 0o022 != 0 {
        return Err(format!("{shown} is writable by others: sshd would ignore the keys"));
    }
    let dir = libc::O_RDONLY | libc::O_DIRECTORY;
    let fd = match openat(home_fd.as_raw_fd(), ".ssh", dir, 0) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) && create => {
            let n = cstr(".ssh");
            // SAFETY: valid C string, descriptor owned above.
            check(unsafe { libc::mkdirat(home_fd.as_raw_fd(), n.as_ptr(), 0o700) })
                .map_err(|e| format!("{shown}/.ssh: {e}"))?;
            let fd = openat(home_fd.as_raw_fd(), ".ssh", dir, 0).map_err(|e| format!("{shown}/.ssh: {e}"))?;
            // SAFETY: descriptor owned here.
            check(unsafe { libc::fchown(fd.as_raw_fd(), uid, gid) }).map_err(|e| format!("{shown}/.ssh: {e}"))?;
            fd
        }
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) => {
            return Err(format!("{shown}/.ssh is not a folder (a link?)"));
        }
        Err(e) => return Err(format!("{shown}/.ssh: {e}")),
    };
    let st = fstat(fd.as_raw_fd()).map_err(|e| format!("{shown}/.ssh: {e}"))?;
    if st.st_uid != uid && st.st_uid != 0 {
        return Err(format!("{shown}/.ssh does not belong to the user"));
    }
    if st.st_mode & 0o022 != 0 {
        return Err(format!("{shown}/.ssh is writable by others: sshd would ignore the keys"));
    }
    Ok(Some(fd))
}

/// Owner, group and mode of a file.
type Meta = (u32, u32, u32);

/// `(text, meta)` of `authorized_keys` in an open `.ssh`; `None` when absent.
fn read_at(ssh: &OwnedFd) -> Result<Option<(String, Meta)>, String> {
    let fd = match openat(ssh.as_raw_fd(), FILE, libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY, 0) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(format!("{FILE} is a link: not read")),
        Err(e) => return Err(format!("{FILE}: {e}")),
    };
    let st = fstat(fd.as_raw_fd()).map_err(|e| format!("{FILE}: {e}"))?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(format!("{FILE} is not a regular file"));
    }
    if st.st_size as u64 > MAX_FILE {
        return Err(format!("{FILE} is larger than 1 MiB"));
    }
    let mut text = String::new();
    std::fs::File::from(fd).read_to_string(&mut text).map_err(|e| format!("{FILE}: {e}"))?;
    Ok(Some((text, (st.st_uid, st.st_gid, st.st_mode & 0o7777))))
}

/// The `authorized_keys` of a home; empty when there is none.
pub fn read(home: &Path, uid: u32, gid: u32) -> Result<String, String> {
    let Some(ssh) = open_ssh(home, uid, gid, false)? else { return Ok(String::new()) };
    Ok(read_at(&ssh)?.map(|(t, _)| t).unwrap_or_default())
}

/// Changes the `authorized_keys` of a home: `change` gets the current text and returns the
/// new one (or an error). The new file keeps the owner of the old one (the user for a new
/// file), mode `0600` unless the old one was stricter.
pub fn update(
    home: &Path,
    uid: u32,
    gid: u32,
    change: &dyn Fn(&str) -> Result<String, String>,
) -> Result<(), String> {
    let ssh = open_ssh(home, uid, gid, true)?.ok_or("no .ssh folder")?;
    let old = read_at(&ssh)?;
    let new = change(old.as_ref().map(|(t, _)| t.as_str()).unwrap_or(""))?;
    let (owner, group, mode) = match &old {
        Some((_, (u, g, m))) => (*u, *g, m & 0o600),
        None => (uid, gid, 0o600),
    };
    // a leftover of an interrupted write of an older version
    let old_tmp = cstr(TMP);
    // SAFETY: valid C string, descriptor owned above.
    unsafe { libc::unlinkat(ssh.as_raw_fd(), old_tmp.as_ptr(), 0) };
    // a name of this write's own (the callers serialize on the engine's lock as well): two
    // writers never share, nor remove, each other's file
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = format!("{TMP}.{}.{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let tmp_c = cstr(&tmp);
    let fd = openat(ssh.as_raw_fd(), &tmp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)
        .map_err(|e| format!("{FILE}: {e}"))?;
    let raw = fd.as_raw_fd();
    let mut f = std::fs::File::from(fd);
    let written = (|| -> io::Result<()> {
        f.write_all(new.as_bytes())?;
        // SAFETY: descriptor owned by `f`.
        check(unsafe { libc::fchown(raw, owner, group) })?;
        check(unsafe { libc::fchmod(raw, mode) })?;
        f.sync_all()
    })();
    if let Err(e) = written {
        // SAFETY: as above.
        unsafe { libc::unlinkat(ssh.as_raw_fd(), tmp_c.as_ptr(), 0) };
        return Err(format!("{FILE}: {e}"));
    }
    let file_c = cstr(FILE);
    // SAFETY: valid C strings, both names relative to the same descriptor.
    check(unsafe { libc::renameat(ssh.as_raw_fd(), tmp_c.as_ptr(), ssh.as_raw_fd(), file_c.as_ptr()) })
        .map_err(|e| format!("{FILE}: {e}"))?;
    // SAFETY: descriptor owned above.
    unsafe { libc::fsync(ssh.as_raw_fd()) };
    Ok(())
}
