//! Copies of the boot filesystems that no sent snapshot holds (the ESP, a separate non-btrfs
//! `/boot`), kept in `/.rynas/boot/<name>/` inside the root subvolume, so every root
//! snapshot carries the boot files of its moment (`docs/research/pc-backup.md` §11.3).
//!
//! - Only changed files are written, each through a temporary file in its own folder +
//!   fsync + rename: there is never a moment without a copy (a crash leaves the previous
//!   files or the new ones, and the next run finishes the job; the manifest keeps the old
//!   signature until a run completes). Files gone from the source are removed.
//! - Mode, owner and times are kept; the copy's top folders are `0700 root`.
//! - Mounts behind an automount are triggered by opening the folder (`O_DIRECTORY`); a
//!   source that is still `autofs`, or empty while the last copy was not, is refused (the
//!   previous copy stays).
//! - At most [`MAX_BYTES`], and only when the root filesystem has room for what changes.
//! - Names that look like key material, and systemd-boot's `loader/random-seed`, are never
//!   copied (and are listed).

use std::ffi::CString;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rynas_api_types::pcagent::BootCopy;
use sha2::{Digest, Sha256};

/// Largest boot tree copied.
pub const MAX_BYTES: u64 = 2 << 30;
/// Free space left over on the root filesystem after a copy.
const SPARE_BYTES: u64 = 64 << 20;
/// `statfs.f_type` of autofs.
const AUTOFS_MAGIC: i64 = 0x0187;
/// Prefix of the per-file temporary names.
const TMP: &str = ".rynas-tmp.";

/// Names of files never copied: key material some setups keep on `/boot`.
fn secret_name(n: &str) -> bool {
    let l = n.to_ascii_lowercase();
    l.contains("keyfile") || l.ends_with(".key") || l.ends_with(".keys")
}

/// Paths (relative to the boot mount, any case: the ESP is FAT) never copied: systemd-boot's
/// random seed (`loader/random-seed`), which must never be reused on another boot or machine.
fn secret_path(rel: &Path) -> bool {
    rel.to_string_lossy().eq_ignore_ascii_case("loader/random-seed")
}

/// The folder name of a mount point's copy: `/boot/efi` → `boot-efi`.
pub fn copy_name(mount: &str) -> String {
    let n = mount.trim_matches('/').replace('/', "-");
    if n.is_empty() { "root".into() } else { n }
}

/// Opens the usual boot mount points once, so automounts (systemd-gpt-auto,
/// `x-systemd.automount`) are mounted before the mount table is read.
pub fn trigger_automounts(root: &Path) {
    for p in ["boot", "boot/efi", "efi"] {
        let _ = open_dir(&root.join(p));
    }
}

fn open_dir(p: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(p)
}

fn fs_type(f: &std::fs::File) -> i64 {
    // SAFETY: fstatfs fills the struct it is given
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::fstatfs(f.as_raw_fd(), &mut s) != 0 {
            return 0;
        }
        s.f_type as i64
    }
}

/// Bytes free for root on the filesystem holding `p`.
fn free_bytes(p: &Path) -> Option<u64> {
    let c = CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs fills the struct it is given
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return None;
        }
        Some(s.f_bfree * s.f_frsize)
    }
}

/// One entry of a boot tree, relative to its mount.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Link(PathBuf),
}

#[derive(Debug, Clone)]
struct Item {
    rel: PathBuf,
    kind: Kind,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    mtime: (i64, i64),
}

/// The tree under `src` on filesystem `dev` (other mounts below it are left out), its
/// signature (names, modes, sizes, times) and the files left out as secrets.
fn scan(src: &Path, dev: u64) -> Result<(Vec<Item>, String, Vec<String>), String> {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    let mut h = Sha256::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = src.join(&rel);
        let mut entries: Vec<_> = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let r = rel.join(e.file_name());
            let Ok(md) = std::fs::symlink_metadata(e.path()) else { continue };
            if md.dev() != dev {
                continue;
            }
            if secret_name(&e.file_name().to_string_lossy()) || secret_path(&r) {
                skipped.push(r.to_string_lossy().into_owned());
                continue;
            }
            let kind = if md.is_dir() {
                Kind::Dir
            } else if md.file_type().is_symlink() {
                match std::fs::read_link(e.path()) {
                    Ok(t) => Kind::Link(t),
                    Err(_) => continue,
                }
            } else if md.is_file() {
                Kind::File
            } else {
                continue;
            };
            h.update(format!("{}\0{}\0{}\0{}.{}\n", r.display(), md.mode(), md.len(), md.mtime(), md.mtime_nsec()).as_bytes());
            if kind == Kind::Dir {
                stack.push(r.clone());
            }
            items.push(Item { rel: r, kind, mode: md.mode() & 0o7777, uid: md.uid(), gid: md.gid(), size: md.len(), mtime: (md.mtime(), md.mtime_nsec()) });
        }
    }
    items.sort_by(|a, b| a.rel.cmp(&b.rel));
    let sig = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((items, sig, skipped))
}

fn sync_dir(p: &Path) {
    if let Ok(d) = std::fs::File::open(p) {
        let _ = d.sync_all();
    }
}

fn set_times(p: &Path, (s, ns): (i64, i64)) {
    let Ok(c) = CString::new(p.as_os_str().as_encoded_bytes()) else { return };
    let t = libc::timespec { tv_sec: s, tv_nsec: ns };
    // SAFETY: valid C string and two timespecs; the link itself, not its target
    unsafe {
        libc::utimensat(libc::AT_FDCWD, c.as_ptr(), [t, t].as_ptr(), libc::AT_SYMLINK_NOFOLLOW);
    }
}

fn set_owner_mode(p: &Path, it: &Item) {
    let _ = std::os::unix::fs::lchown(p, Some(it.uid), Some(it.gid));
    if !matches!(it.kind, Kind::Link(_)) {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(it.mode));
    }
}

/// The copy of `it` at `to` is already right (kind, size, mode and time).
fn same(to: &Path, it: &Item) -> bool {
    let Ok(md) = std::fs::symlink_metadata(to) else { return false };
    match &it.kind {
        Kind::Dir => md.is_dir(),
        Kind::Link(t) => md.file_type().is_symlink() && std::fs::read_link(to).is_ok_and(|x| &x == t),
        Kind::File => {
            md.is_file() && md.len() == it.size && md.mode() & 0o7777 == it.mode && (md.mtime(), md.mtime_nsec()) == it.mtime
        }
    }
}

/// Removes whatever is at `p` (file, link or folder tree), not following links.
fn remove_any(p: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(p) {
        Ok(md) if md.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => Ok(()),
    }
    .map_err(|e| format!("{}: {e}", p.display()))
}

/// Writes `from` into `to` through a temporary file next to it, fsynced, with the source's
/// owner, mode and time, then renamed over `to`.
fn put_file(from: &Path, to: &Path, it: &Item) -> Result<u64, String> {
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = to.with_file_name(format!("{TMP}{name}"));
    let _ = std::fs::remove_file(&tmp);
    let mut i = std::fs::File::open(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let mut o = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    let n = std::io::copy(&mut i, &mut o).map_err(|e| format!("{}: {e}", from.display()))?;
    o.flush().and_then(|_| o.sync_all()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    drop(o);
    set_owner_mode(&tmp, it);
    set_times(&tmp, it.mtime);
    if std::fs::symlink_metadata(to).is_ok_and(|m| m.is_dir()) {
        remove_any(to)?;
    }
    std::fs::rename(&tmp, to).map_err(|e| format!("{}: {e}", to.display()))?;
    Ok(n)
}

/// Copies the boot filesystem at `src` (mounted at `mount`) into `<boot_dir>/<name>/`,
/// writing only what changed since the copy on disk. `previous`: the last good record (an
/// unchanged tree returns it as it is). `shown_dir`: `boot_dir` as the PC sees it.
pub fn copy_boot(src: &Path, mount: &str, boot_dir: &Path, shown_dir: &str, previous: Option<&BootCopy>) -> Result<BootCopy, String> {
    let name = copy_name(mount);
    let dest = boot_dir.join(&name);
    // opening the folder mounts an automount; the descriptor tells what is there now
    let fd = open_dir(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if fs_type(&fd) == AUTOFS_MAGIC {
        return Err(format!("{mount}: the automount did not mount anything"));
    }
    let dev = fd.metadata().map_err(|e| format!("{}: {e}", src.display()))?.dev();
    let (items, signature, skipped) = scan(src, dev)?;
    if let Some(p) = previous
        && p.signature == signature
        && p.mount == mount
        && dest.is_dir()
    {
        return Ok(BootCopy { stale: false, error: None, ..p.clone() });
    }
    let files = items.iter().filter(|i| i.kind == Kind::File).count() as u64;
    let bytes: u64 = items.iter().filter(|i| i.kind == Kind::File).map(|i| i.size).sum();
    if files == 0 && (previous.is_some_and(|p| p.files > 0) || dest.is_dir()) {
        return Err(format!("{mount} is empty now: the previous copy is kept"));
    }
    if bytes > MAX_BYTES {
        return Err(format!("{mount} holds {bytes} bytes, more than the {MAX_BYTES} a boot copy may take"));
    }
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(boot_dir).map_err(|e| format!("{}: {e}", boot_dir.display()))?;
    let need: u64 = items.iter().filter(|i| i.kind == Kind::File && !same(&dest.join(&i.rel), i)).map(|i| i.size).sum();
    if let Some(free) = free_bytes(boot_dir)
        && free < need + SPARE_BYTES
    {
        return Err(format!("{}: {free} bytes free, {need} needed for the copy of {mount}", boot_dir.display()));
    }
    // an older copy as one folder of temporary files (the earlier format): nothing to keep
    for old in [format!(".{name}.tmp"), format!(".{name}.old")] {
        remove_any(&boot_dir.join(old))?;
    }
    if std::fs::symlink_metadata(&dest).is_ok_and(|m| !m.is_dir()) {
        remove_any(&dest)?;
    }
    std::fs::DirBuilder::new().mode(0o700).create(&dest).or_else(|e| if dest.is_dir() { Ok(()) } else { Err(e) }).map_err(|e| format!("{}: {e}", dest.display()))?;
    let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o700));
    // folders first (sorted: parents before children), then files and links
    for it in items.iter().filter(|i| i.kind == Kind::Dir) {
        let to = dest.join(&it.rel);
        if !std::fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
            remove_any(&to)?;
            std::fs::DirBuilder::new().mode(0o700).create(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        }
    }
    let mut touched: Vec<PathBuf> = Vec::new();
    for it in items.iter().filter(|i| i.kind != Kind::Dir) {
        let to = dest.join(&it.rel);
        if same(&to, it) {
            continue;
        }
        match &it.kind {
            Kind::File => {
                put_file(&src.join(&it.rel), &to, it)?;
            }
            Kind::Link(t) => {
                let tmp = to.with_file_name(format!("{TMP}{}", to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                let _ = std::fs::remove_file(&tmp);
                std::os::unix::fs::symlink(t, &tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
                set_owner_mode(&tmp, it);
                set_times(&tmp, it.mtime);
                if std::fs::symlink_metadata(&to).is_ok_and(|m| m.is_dir()) {
                    remove_any(&to)?;
                }
                std::fs::rename(&tmp, &to).map_err(|e| format!("{}: {e}", to.display()))?;
            }
            Kind::Dir => {}
        }
        if let Some(p) = to.parent() {
            touched.push(p.to_path_buf());
        }
    }
    // what the source no longer has (and temporary files of an interrupted run)
    let want: std::collections::BTreeSet<&Path> = items.iter().map(|i| i.rel.as_path()).collect();
    let mut stack = vec![PathBuf::new()];
    let mut gone = Vec::new();
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(dest.join(&rel)) else { continue };
        for e in rd.flatten() {
            let r = rel.join(e.file_name());
            if !want.contains(r.as_path()) {
                gone.push(r);
            } else if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(r);
            }
        }
    }
    for r in gone {
        let p = dest.join(&r);
        remove_any(&p)?;
        if let Some(d) = p.parent() {
            touched.push(d.to_path_buf());
        }
    }
    touched.sort();
    touched.dedup();
    for d in &touched {
        sync_dir(d);
    }
    // folder owners, modes and times last (writing inside changes a folder's time)
    for it in items.iter().filter(|i| i.kind == Kind::Dir).rev() {
        let to = dest.join(&it.rel);
        set_owner_mode(&to, it);
        set_times(&to, it.mtime);
    }
    sync_dir(&dest);
    sync_dir(boot_dir);
    Ok(BootCopy {
        mount: mount.to_string(),
        dir: format!("{}/{name}", shown_dir.trim_end_matches('/')),
        bytes,
        files,
        signature,
        skipped,
        stale: false,
        error: None,
    })
}

/// Copies that are no longer wanted (mount gone) are removed; the others stay.
pub fn prune_copies(boot_dir: &Path, keep: &[BootCopy]) {
    let Ok(rd) = std::fs::read_dir(boot_dir) else { return };
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !keep.iter().any(|k| copy_name(&k.mount) == n) {
            let _ = remove_any(&e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(src: &Path) {
        std::fs::create_dir_all(src.join("EFI/systemd")).unwrap();
        std::fs::write(src.join("EFI/systemd/systemd-bootx64.efi"), vec![1u8; 1000]).unwrap();
        std::fs::write(src.join("vmlinuz-linux"), vec![2u8; 500]).unwrap();
        std::fs::write(src.join("crypto_keyfile.bin"), b"secret").unwrap();
        std::fs::create_dir_all(src.join("loader")).unwrap();
        std::fs::write(src.join("loader/random-seed"), [9u8; 32]).unwrap();
        std::os::unix::fs::symlink("vmlinuz-linux", src.join("vmlinuz")).unwrap();
    }

    #[test]
    fn copies_only_what_changed_and_keeps_metadata() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("esp");
        tree(&src);
        std::fs::set_permissions(src.join("vmlinuz-linux"), std::fs::Permissions::from_mode(0o640)).unwrap();
        let boot = t.path().join("root/.rynas/boot");
        let c = copy_boot(&src, "/boot", &boot, "/.rynas/boot", None).unwrap();
        assert_eq!((c.bytes, c.files), (1500, 2));
        assert_eq!(c.dir, "/.rynas/boot/boot");
        assert_eq!(c.skipped, ["crypto_keyfile.bin", "loader/random-seed"]);
        assert!(!c.stale);
        let d = boot.join("boot");
        assert!(!d.join("crypto_keyfile.bin").exists());
        assert!(!d.join("loader/random-seed").exists());
        assert_eq!(std::fs::read(d.join("vmlinuz-linux")).unwrap(), vec![2u8; 500]);
        assert_eq!(std::fs::read_link(d.join("vmlinuz")).unwrap(), Path::new("vmlinuz-linux"));
        let (s, c2) = (std::fs::metadata(src.join("vmlinuz-linux")).unwrap(), std::fs::metadata(d.join("vmlinuz-linux")).unwrap());
        assert_eq!((c2.mode() & 0o7777, c2.mtime(), c2.mtime_nsec()), (0o640, s.mtime(), s.mtime_nsec()));
        assert_eq!(std::fs::metadata(&d).unwrap().mode() & 0o777, 0o700);
        // unchanged: the same record, nothing copied
        let again = copy_boot(&src, "/boot", &boot, "/.rynas/boot", Some(&c)).unwrap();
        assert_eq!(again, c);
        // one file changed, one removed: only that file is rewritten (the other keeps its
        // inode), the removed one goes, no temporary file is left
        let ino = std::fs::metadata(d.join("EFI/systemd/systemd-bootx64.efi")).unwrap().ino();
        std::fs::write(src.join("vmlinuz-linux"), vec![3u8; 700]).unwrap();
        std::fs::remove_file(src.join("vmlinuz")).unwrap();
        std::fs::write(d.join(format!("{TMP}stray")), b"").unwrap();
        let c3 = copy_boot(&src, "/boot", &boot, "/.rynas/boot", Some(&c)).unwrap();
        assert_ne!(c3.signature, c.signature);
        assert_eq!(c3.bytes, 1700);
        assert_eq!(std::fs::metadata(d.join("EFI/systemd/systemd-bootx64.efi")).unwrap().ino(), ino);
        assert_eq!(std::fs::read(d.join("vmlinuz-linux")).unwrap(), vec![3u8; 700]);
        assert!(std::fs::symlink_metadata(d.join("vmlinuz")).is_err());
        assert!(!d.join(format!("{TMP}stray")).exists());
        let names: Vec<_> = std::fs::read_dir(&boot).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        assert_eq!(names, ["boot"]);
        prune_copies(&boot, &[]);
        assert!(!boot.join("boot").exists());
        assert_eq!(copy_name("/boot/efi"), "boot-efi");
    }

    #[test]
    fn an_empty_source_never_replaces_a_good_copy() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("esp");
        tree(&src);
        let boot = t.path().join("boot");
        let c = copy_boot(&src, "/efi", &boot, "/.rynas/boot", None).unwrap();
        // the ESP not mounted any more: an empty folder where it was
        let empty = t.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let e = copy_boot(&empty, "/efi", &boot, "/.rynas/boot", Some(&c)).unwrap_err();
        assert!(e.contains("empty"), "{e}");
        assert_eq!(std::fs::read(boot.join("efi/vmlinuz-linux")).unwrap(), vec![2u8; 500]);
        // a first copy of an empty tree is fine (nothing to lose)
        let first = copy_boot(&empty, "/boot", &boot, "/.rynas/boot", None).unwrap();
        assert_eq!(first.files, 0);
        // a missing source is an error, not an empty copy
        assert!(copy_boot(&t.path().join("nowhere"), "/efi", &boot, "/.rynas/boot", Some(&c)).is_err());
    }

    #[test]
    fn size_cap() {
        let t = tempfile::tempdir().unwrap();
        let src = t.path().join("big");
        std::fs::create_dir_all(&src).unwrap();
        // sparse: no real space used
        let f = std::fs::File::create(src.join("huge.img")).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        let e = copy_boot(&src, "/boot", &t.path().join("b"), "/.rynas/boot", None).unwrap_err();
        assert!(e.contains("more than"), "{e}");
        assert!(!t.path().join("b/boot").exists());
    }
}
