//! The system volume's folder (D-ROOTVOL): [`SYSTEM_VOLUME_DIR`], made ready by
//! [`ensure_system_dir`], the one function every user of the system volume calls (Docker,
//! apps, shares, restic, PC backup, replication).
//!
//! **btrfs root.** The data lives in the top-level subvolume [`SUBVOLUME`], a sibling of the
//! root subvolume (like `@home`), mounted on the folder by the unit [`UNIT`]
//! (`What=/dev/disk/by-uuid/<root fs>`, `Options=subvol=@rynas-system`, wanted by
//! `local-fs.target`). Being outside the root subvolume, it is neither copied by a restore
//! point nor replaced by a rollback, rynas's own or another tool's (snapper, timeshift): a
//! rollback changes nothing in it and needs no step for it. The subvolume is created once,
//! with the top level (subvolid 5) mounted for a moment at [`TOP_DIR`]; mounts run outside
//! rynasd's sandbox (seccomp `@system-service` has no `mount`): `mount`/`umount` in a
//! transient unit (`systemd-run`), the folder's mount through `systemctl`.
//!
//! The folder is written into only once it is that mount (same filesystem as `/`,
//! subvolume [`SUBVOLUME`]). A plain folder there (an empty placeholder left by a rollback,
//! or one made by hand) is mounted over when empty and refused when it holds files: rynas
//! never moves data on its own.
//!
//! **Other roots** (ext4, xfs...): a plain folder, no subvolume, no snapshots, no NOCOW.
//!
//! **Always**: `/srv/rynas` and the folder are real directories (no symbolic link, opened
//! without following one), owned by root and not writable by group or others. What rynas
//! creates gets mode 0755; what it finds otherwise is refused, not changed.

use std::fs::File;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rynas_api_types::storage::SYSTEM_VOLUME_DIR;
use rynas_tasks::Cmd;

use crate::ioctl;

/// The top-level subvolume that holds the system volume's data on a btrfs root.
pub const SUBVOLUME: &str = "@rynas-system";
/// The mount unit of [`SYSTEM_VOLUME_DIR`] (`systemd-escape --path`).
pub const UNIT: &str = "srv-rynas-system.mount";
/// Where [`UNIT`] is written.
pub const UNIT_PATH: &str = "/etc/systemd/system/srv-rynas-system.mount";
/// First line of the unit.
pub const MARKER: &str = "# rynas: the system volume (D-ROOTVOL)";
/// Where the top level is mounted while [`SUBVOLUME`] is created.
pub const TOP_DIR: &str = "/run/rynas/system-volume-top";
/// The parent of the folder.
pub const PARENT: &str = "/srv/rynas";
const CMD_TIMEOUT: Duration = Duration::from_secs(90);

/// The unit that mounts [`SUBVOLUME`] of the filesystem `fs_uuid` on [`SYSTEM_VOLUME_DIR`].
pub fn unit_text(fs_uuid: &str) -> String {
    format!(
        "{MARKER}\n\
         # The data of the system volume: the subvolume {SUBVOLUME} next to the root\n\
         # subvolume, so system restore points and rollbacks leave it alone. Written by rynasd.\n\
         [Unit]\n\
         Description=rynas system volume\n\
         Documentation=https://github.com/rylos/rynas\n\
         \n\
         [Mount]\n\
         What=/dev/disk/by-uuid/{fs_uuid}\n\
         Where={SYSTEM_VOLUME_DIR}\n\
         Type=btrfs\n\
         Options=subvol={SUBVOLUME},noatime\n\
         DirectoryMode=0755\n\
         \n\
         [Install]\n\
         WantedBy=local-fs.target\n"
    )
}

/// A btrfs filesystem UUID (`8-4-4-4-12` hex), the only text put in [`unit_text`].
pub fn valid_uuid(u: &str) -> bool {
    let parts: Vec<&str> = u.split('-').collect();
    parts.len() == 5
        && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| {
            p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
}

// ---------------------------------------------------------------------------------------------
// the mount table
// ---------------------------------------------------------------------------------------------

/// One line of `/proc/self/mountinfo`.
struct Mount {
    dev: String,
    root: String,
    point: String,
    fstype: String,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn mounts(mountinfo: &str) -> Vec<Mount> {
    mountinfo
        .lines()
        .filter_map(|l| {
            let (pre, post) = l.split_once(" - ")?;
            let f: Vec<&str> = pre.split(' ').collect();
            Some(Mount {
                dev: f.get(2)?.to_string(),
                root: unescape(f.get(3)?),
                point: unescape(f.get(4)?),
                fstype: post.split(' ').next()?.to_string(),
            })
        })
        .collect()
}

/// Type of the filesystem mounted at `/` (the last mount there wins).
pub fn root_fstype(mountinfo: &str) -> Option<String> {
    mounts(mountinfo)
        .into_iter()
        .rfind(|m| m.point == "/")
        .map(|m| m.fstype)
}

/// What is mounted on [`SYSTEM_VOLUME_DIR`] (btrfs root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountState {
    /// [`SUBVOLUME`] of the root filesystem.
    Mounted,
    /// Nothing.
    NotMounted,
    /// Something else: why it is not the system volume.
    Wrong(String),
}

/// [`MountState`] from the mount table: the last mount on the folder must be btrfs, of the
/// same filesystem as `/` (same device number) and of the subvolume [`SUBVOLUME`].
pub fn mount_state(mountinfo: &str) -> MountState {
    let all = mounts(mountinfo);
    let Some(m) = all.iter().rfind(|m| m.point == SYSTEM_VOLUME_DIR) else {
        return MountState::NotMounted;
    };
    let root = all.iter().rfind(|m| m.point == "/");
    let wrong = |what: &str| {
        MountState::Wrong(format!(
            "{SYSTEM_VOLUME_DIR} is a mount of {what}, not of the subvolume {SUBVOLUME} of the system disk: rynas does not write into it"
        ))
    };
    if m.fstype != "btrfs" {
        return wrong(&format!("a {} filesystem", m.fstype));
    }
    if root.is_none_or(|r| r.dev != m.dev) {
        return wrong("another filesystem");
    }
    if m.root.trim_start_matches('/') != SUBVOLUME {
        return wrong(&format!("the subvolume {}", m.root));
    }
    MountState::Mounted
}

// ---------------------------------------------------------------------------------------------
// folder checks
// ---------------------------------------------------------------------------------------------

/// The folder open as `f` (`what` names it in errors) is a directory owned by `uid`, not
/// writable by group or others.
fn check_open(f: &File, what: &str, uid: u32) -> Result<(), String> {
    let m = f.metadata().map_err(|e| format!("{what}: {e}"))?;
    if !m.is_dir() {
        return Err(format!("{what} is not a folder"));
    }
    if m.uid() != uid {
        return Err(format!(
            "{what} is owned by user {}, not by root: rynas does not use it",
            m.uid()
        ));
    }
    if m.mode() & 0o022 != 0 {
        return Err(format!(
            "{what} is writable by group or others (mode {:o}): rynas does not use it",
            m.mode() & 0o7777
        ));
    }
    Ok(())
}

/// Opens a folder without following a symbolic link (an error names it).
fn open_nofollow(open: std::io::Result<File>, what: &str) -> Result<Option<File>, String> {
    match open {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) || e.raw_os_error() == Some(libc::ENOTDIR) => {
            Err(format!("{what} is a symbolic link or a file, not a folder: rynas does not use it"))
        }
        Err(e) => Err(format!("{what}: {e}")),
    }
}

/// The folder has no entries.
fn is_empty(f: &File) -> Result<bool, String> {
    let fd = f.try_clone().map_err(|e| e.to_string())?;
    let path = PathBuf::from(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&fd)));
    let mut d = std::fs::read_dir(&path).map_err(|e| format!("{SYSTEM_VOLUME_DIR}: {e}"))?;
    Ok(d.next().is_none())
}

fn mkdir(path: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .mode(0o755)
        .create(path)
        .map_err(|e| format!("mkdir {}: {e}", path.display()))?;
    // the umask may have taken bits away
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// `/srv/rynas` below `root`: made (0755) when missing, then checked; its descriptor.
fn ready_parent(root: &Path, uid: u32, log: &dyn Fn(&str)) -> Result<File, String> {
    let p = under(root, PARENT);
    if open_nofollow(ioctl::open_dir(&p), PARENT)?.is_none() {
        if let Some(srv) = p.parent()
            && !srv.is_dir()
        {
            std::fs::create_dir_all(srv).map_err(|e| format!("{}: {e}", srv.display()))?;
        }
        log(&format!("mkdir {PARENT}"));
        mkdir(&p)?;
    }
    let f = open_nofollow(ioctl::open_dir(&p), PARENT)?.ok_or_else(|| format!("{PARENT} is missing"))?;
    check_open(&f, PARENT, uid)?;
    Ok(f)
}

/// The folder inside `parent`, `None` when missing.
fn open_folder(parent: &File) -> Result<Option<File>, String> {
    open_nofollow(ioctl::open_dir_at(parent, "system"), SYSTEM_VOLUME_DIR)
}

fn under(root: &Path, abs: &str) -> PathBuf {
    root.join(abs.trim_start_matches('/'))
}

// ---------------------------------------------------------------------------------------------
// the effects
// ---------------------------------------------------------------------------------------------

/// The effects of [`ensure_with`]: [`HostSysvol`] on the machine, a recording fake in tests.
pub trait SysvolOps {
    /// The tree the folders are below (`/` on the machine).
    fn root(&self) -> &Path;
    /// The owner the folders must have (root on the machine).
    fn owner(&self) -> u32;
    fn mountinfo(&self) -> String;
    /// UUID of the root filesystem.
    fn root_uuid(&self) -> Result<String, String>;
    /// Creates [`SUBVOLUME`] at the top level of `fs_uuid` unless it is there (mode 0755).
    fn ensure_subvolume(&self, fs_uuid: &str, log: &dyn Fn(&str)) -> Result<(), String>;
    fn read_unit(&self) -> Option<String>;
    fn write_unit(&self, text: &str) -> Result<(), String>;
    fn systemctl(&self, args: &[&str], log: &dyn Fn(&str)) -> Result<(), String>;
}

/// One preparation at a time: two tasks would mount the top level together.
static LOCK: Mutex<()> = Mutex::new(());

/// Makes the system volume's folder ready below `root` and returns its path there.
///
/// `real`: rynasd on the machine (`root` is `/`, not a dry run). Then on a btrfs root the
/// folder must be (or becomes) the mount of [`SUBVOLUME`], on another root a checked plain
/// folder. A dry or test root gets a plain folder with the same checks (owned by the
/// current user).
pub fn ensure_system_dir(root: &Path, real: bool, log: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if real && root == Path::new("/") {
        let host = HostSysvol;
        if root_fstype(&host.mountinfo()).as_deref() == Some("btrfs") {
            return ensure_with(&host, log);
        }
        return plain(root, 0, log);
    }
    // SAFETY: no arguments, cannot fail
    plain(root, unsafe { libc::geteuid() }, log)
}

/// A plain folder below `root` (non-btrfs root, dry or test root).
fn plain(root: &Path, uid: u32, log: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let parent = ready_parent(root, uid, log)?;
    let dir = under(root, SYSTEM_VOLUME_DIR);
    let f = match open_folder(&parent)? {
        Some(f) => f,
        None => {
            log(&format!("mkdir {SYSTEM_VOLUME_DIR}"));
            mkdir(&dir)?;
            open_folder(&parent)?.ok_or_else(|| format!("{SYSTEM_VOLUME_DIR} is missing"))?
        }
    };
    check_open(&f, SYSTEM_VOLUME_DIR, uid)?;
    Ok(dir)
}

/// The btrfs root: see the module documentation.
pub fn ensure_with(ops: &dyn SysvolOps, log: &dyn Fn(&str)) -> Result<PathBuf, String> {
    let (root, uid) = (ops.root(), ops.owner());
    let dir = under(root, SYSTEM_VOLUME_DIR);
    let parent = ready_parent(root, uid, log)?;
    match mount_state(&ops.mountinfo()) {
        MountState::Mounted => {}
        MountState::Wrong(why) => return Err(why),
        MountState::NotMounted => {
            // a placeholder may be mounted over, data never
            if let Some(f) = open_folder(&parent)?
                && !is_empty(&f)?
            {
                return Err(format!(
                    "{SYSTEM_VOLUME_DIR} is a plain folder with files in it, not the system volume's subvolume: rynas does not write into it. Move its content away (or into the subvolume {SUBVOLUME}) and try again."
                ));
            }
            let uuid = ops.root_uuid()?;
            if !valid_uuid(&uuid) {
                return Err(format!("unexpected filesystem UUID {uuid:?}"));
            }
            ops.ensure_subvolume(&uuid, log)?;
            let text = unit_text(&uuid);
            if ops.read_unit().as_deref() != Some(text.as_str()) {
                log(&format!("writing {UNIT_PATH}"));
                ops.write_unit(&text)?;
            }
            ops.systemctl(&["daemon-reload"], log)?;
            ops.systemctl(&["enable", "--now", UNIT], log)?;
            match mount_state(&ops.mountinfo()) {
                MountState::Mounted => log(&format!("{SYSTEM_VOLUME_DIR} is mounted ({SUBVOLUME})")),
                MountState::Wrong(why) => return Err(why),
                MountState::NotMounted => {
                    return Err(format!(
                        "{UNIT} started but {SYSTEM_VOLUME_DIR} is not mounted: see `journalctl -u {UNIT}`"
                    ));
                }
            }
        }
    }
    let f = open_folder(&parent)?.ok_or_else(|| format!("{SYSTEM_VOLUME_DIR} is missing"))?;
    check_open(&f, SYSTEM_VOLUME_DIR, uid)?;
    Ok(dir)
}

/// Why the system volume's folder cannot be used now, `None` when it can (or when it can
/// be made on first use). Read-only: nothing is created or mounted.
pub fn unavailable(root: &Path, real: bool) -> Option<String> {
    let (uid, mi) = if real && root == Path::new("/") {
        (0, HostSysvol.mountinfo())
    } else {
        // SAFETY: no arguments, cannot fail
        (unsafe { libc::geteuid() }, String::new())
    };
    let parent = match open_nofollow(ioctl::open_dir(&under(root, PARENT)), PARENT) {
        Ok(Some(f)) => f,
        Ok(None) => return None,
        Err(e) => return Some(e),
    };
    if let Err(e) = check_open(&parent, PARENT, uid) {
        return Some(e);
    }
    let btrfs = real && root_fstype(&mi).as_deref() == Some("btrfs");
    let state = if btrfs { mount_state(&mi) } else { MountState::Mounted };
    match (state, open_folder(&parent)) {
        (_, Err(e)) => Some(e),
        (MountState::Wrong(why), _) => Some(why),
        (_, Ok(None)) => None,
        (MountState::NotMounted, Ok(Some(f))) => match is_empty(&f) {
            Ok(true) => check_open(&f, SYSTEM_VOLUME_DIR, uid).err(),
            Ok(false) => Some(format!(
                "{SYSTEM_VOLUME_DIR} is a plain folder with files in it, not the system volume's subvolume: move its content away to use the system volume"
            )),
            Err(e) => Some(e),
        },
        (MountState::Mounted, Ok(Some(f))) => check_open(&f, SYSTEM_VOLUME_DIR, uid).err(),
    }
}

// ---------------------------------------------------------------------------------------------
// the machine
// ---------------------------------------------------------------------------------------------

/// rynasd as root on the machine.
pub struct HostSysvol;

/// `argv` in a transient unit outside rynasd's sandbox (its seccomp filter has no `mount`,
/// and its mount namespace is its own): `--pipe` returns the output and the exit status.
fn outside(argv: &[&str]) -> Cmd {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    Cmd::new("systemd-run")
        .args(["--wait", "--pipe", "--collect", "--quiet", "--service-type=exec"])
        .arg(format!(
            "--unit=rynas-system-volume-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
        .arg(format!("--property=RuntimeMaxSec={}", CMD_TIMEOUT.as_secs()))
        .arg("--description=rynas: system volume")
        .arg("--")
        .args(argv.iter().copied())
        .env_clear()
        .timeout(CMD_TIMEOUT + Duration::from_secs(10))
}

/// The `mount` of the top level of `fs_uuid` at [`TOP_DIR`].
pub fn mount_top_argv(fs_uuid: &str) -> Vec<String> {
    vec![
        "mount".into(),
        "-t".into(),
        "btrfs".into(),
        "-o".into(),
        "subvolid=5,noatime".into(),
        format!("/dev/disk/by-uuid/{fs_uuid}"),
        TOP_DIR.into(),
    ]
}

fn run(c: Cmd, log: &dyn Fn(&str)) -> Result<(), String> {
    let shown = c.display();
    log(&format!("$ {shown}"));
    match c.run_blocking() {
        Ok(o) if o.success() => Ok(()),
        Ok(o) => Err(format!("{shown}: exit code {:?}: {}", o.code, o.stderr.trim())),
        Err(e) => Err(format!("{shown}: {e}")),
    }
}

impl HostSysvol {
    /// In the mounted top level: [`SUBVOLUME`] made unless there (refused when something
    /// else has its name).
    fn in_top(&self, log: &dyn Fn(&str)) -> Result<(), String> {
        let top = ioctl::open_dir(Path::new(TOP_DIR)).map_err(|e| format!("{TOP_DIR}: {e}"))?;
        match ioctl::open_dir_at(&top, SUBVOLUME) {
            Ok(f) if ioctl::is_subvolume(&f) => return Ok(()),
            Ok(_) => return Err(format!("{SUBVOLUME} at the top of the system disk is a folder, not a subvolume")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{SUBVOLUME}: {e}")),
        }
        let path = Path::new(TOP_DIR).join(SUBVOLUME);
        run(
            Cmd::new("btrfs")
                .args(["subvolume", "create"])
                .arg(&path)
                .env_clear()
                .timeout(CMD_TIMEOUT),
            log,
        )?;
        let f = ioctl::open_dir_at(&top, SUBVOLUME).map_err(|e| format!("{SUBVOLUME}: {e}"))?;
        f.set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("{SUBVOLUME}: {e}"))
    }
}

impl SysvolOps for HostSysvol {
    fn root(&self) -> &Path {
        Path::new("/")
    }

    fn owner(&self) -> u32 {
        0
    }

    fn mountinfo(&self) -> String {
        std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default()
    }

    fn root_uuid(&self) -> Result<String, String> {
        let f = ioctl::open_dir(Path::new("/")).map_err(|e| format!("/: {e}"))?;
        ioctl::fs_uuid(&f).map_err(|e| format!("UUID of the system disk: {e}"))
    }

    fn ensure_subvolume(&self, fs_uuid: &str, log: &dyn Fn(&str)) -> Result<(), String> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(TOP_DIR)
            .map_err(|e| format!("{TOP_DIR}: {e}"))?;
        let argv = mount_top_argv(fs_uuid);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        run(outside(&argv), log)?;
        let made = self.in_top(log);
        let unmounted = run(outside(&["umount", TOP_DIR]), log);
        if unmounted.is_ok()
            && let Err(e) = std::fs::remove_dir(TOP_DIR)
        {
            log(&format!("warning: {TOP_DIR}: {e}"));
        }
        made?;
        unmounted
    }

    fn read_unit(&self) -> Option<String> {
        std::fs::read_to_string(UNIT_PATH).ok()
    }

    fn write_unit(&self, text: &str) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = format!("{UNIT_PATH}.rynas-tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&tmp)
            .map_err(|e| format!("{tmp}: {e}"))?;
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("{tmp}: {e}"))?;
        std::fs::rename(&tmp, UNIT_PATH).map_err(|e| format!("{UNIT_PATH}: {e}"))
    }

    fn systemctl(&self, args: &[&str], log: &dyn Fn(&str)) -> Result<(), String> {
        run(
            Cmd::new("systemctl")
                .args(args.iter().copied())
                .env_clear()
                .timeout(CMD_TIMEOUT),
            log,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const ROOT: &str = "22 1 0:21 /@rootfs / rw - btrfs /dev/md0 rw\n";
    const MOUNTED: &str = "40 22 0:21 /@rynas-system /srv/rynas/system rw - btrfs /dev/md0 rw\n";
    const UUID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    #[test]
    fn the_unit_mounts_the_subvolume_at_boot() {
        let u = unit_text(UUID);
        assert!(u.starts_with(MARKER));
        assert!(u.contains(&format!("What=/dev/disk/by-uuid/{UUID}\n")));
        assert!(u.contains("Where=/srv/rynas/system\n"));
        assert!(u.contains("Type=btrfs\nOptions=subvol=@rynas-system,noatime\n"));
        assert!(u.contains("[Install]\nWantedBy=local-fs.target\n"));
        assert!(valid_uuid(UUID) && !valid_uuid("x") && !valid_uuid("0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0"));
        assert!(!valid_uuid("0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0\nWhat=/"));
        assert_eq!(
            mount_top_argv(UUID).join(" "),
            format!("mount -t btrfs -o subvolid=5,noatime /dev/disk/by-uuid/{UUID} /run/rynas/system-volume-top")
        );
    }

    #[test]
    fn only_the_subvolume_of_the_root_filesystem_counts() {
        assert_eq!(mount_state(ROOT), MountState::NotMounted);
        assert_eq!(mount_state(&format!("{ROOT}{MOUNTED}")), MountState::Mounted);
        assert_eq!(root_fstype(ROOT).as_deref(), Some("btrfs"));
        for bad in [
            "40 22 0:21 /@other /srv/rynas/system rw - btrfs /dev/md0 rw\n",
            "40 22 0:44 /@rynas-system /srv/rynas/system rw - btrfs /dev/sdb1 rw\n",
            "40 22 8:3 / /srv/rynas/system rw - ext4 /dev/sdc1 rw\n",
            "40 22 0:50 / /srv/rynas/system rw - tmpfs tmpfs rw\n",
        ] {
            assert!(
                matches!(mount_state(&format!("{ROOT}{bad}")), MountState::Wrong(_)),
                "{bad}"
            );
        }
        // the last mount on the folder wins
        let over = format!("{ROOT}{MOUNTED}50 40 0:50 / /srv/rynas/system rw - tmpfs tmpfs rw\n");
        assert!(matches!(mount_state(&over), MountState::Wrong(_)));
    }

    /// Records the effects; `mount` makes the mount table show the subvolume.
    struct Fake {
        root: tempfile::TempDir,
        mountinfo: RefCell<String>,
        unit: RefCell<Option<String>>,
        calls: RefCell<Vec<String>>,
        mounts: bool,
    }

    impl Fake {
        fn new(mounts: bool) -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
                mountinfo: RefCell::new(ROOT.into()),
                unit: RefCell::new(None),
                calls: RefCell::new(Vec::new()),
                mounts,
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl SysvolOps for Fake {
        fn root(&self) -> &Path {
            self.root.path()
        }
        fn owner(&self) -> u32 {
            unsafe { libc::geteuid() }
        }
        fn mountinfo(&self) -> String {
            self.mountinfo.borrow().clone()
        }
        fn root_uuid(&self) -> Result<String, String> {
            Ok(UUID.into())
        }
        fn ensure_subvolume(&self, u: &str, _: &dyn Fn(&str)) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("subvolume {u}"));
            Ok(())
        }
        fn read_unit(&self) -> Option<String> {
            self.unit.borrow().clone()
        }
        fn write_unit(&self, text: &str) -> Result<(), String> {
            self.calls.borrow_mut().push("write unit".into());
            *self.unit.borrow_mut() = Some(text.into());
            Ok(())
        }
        fn systemctl(&self, args: &[&str], _: &dyn Fn(&str)) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("systemctl {}", args.join(" ")));
            if args.contains(&"--now") && self.mounts {
                let dir = under(self.root.path(), SYSTEM_VOLUME_DIR);
                if !dir.exists() {
                    mkdir(&dir).unwrap();
                }
                self.mountinfo.borrow_mut().push_str(MOUNTED);
            }
            Ok(())
        }
    }

    #[test]
    fn a_btrfs_root_mounts_the_subvolume_once() {
        let f = Fake::new(true);
        let d = ensure_with(&f, &|_| {}).unwrap();
        assert!(d.ends_with("srv/rynas/system") && d.is_dir());
        assert_eq!(
            f.calls(),
            [
                format!("subvolume {UUID}"),
                "write unit".into(),
                "systemctl daemon-reload".into(),
                format!("systemctl enable --now {UNIT}"),
            ]
        );
        // mounted: nothing more to do
        ensure_with(&f, &|_| {}).unwrap();
        assert_eq!(f.calls().len(), 4);
        let mode = std::fs::metadata(under(f.root(), PARENT)).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn an_empty_placeholder_is_mounted_over_a_folder_with_files_is_refused() {
        let f = Fake::new(true);
        let dir = under(f.root(), SYSTEM_VOLUME_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(dir.parent().unwrap(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // the unit is there already, unchanged: not written again
        *f.unit.borrow_mut() = Some(unit_text(UUID));
        ensure_with(&f, &|_| {}).unwrap();
        assert!(!f.calls().contains(&"write unit".to_string()));

        let f = Fake::new(true);
        let dir = under(f.root(), SYSTEM_VOLUME_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(dir.parent().unwrap(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("data"), "x").unwrap();
        let e = ensure_with(&f, &|_| {}).unwrap_err();
        assert!(e.contains("plain folder with files"), "{e}");
        assert!(f.calls().is_empty());
    }

    #[test]
    fn a_mount_that_does_not_come_up_or_a_wrong_mount_is_an_error() {
        let f = Fake::new(false);
        let e = ensure_with(&f, &|_| {}).unwrap_err();
        assert!(e.contains("is not mounted"), "{e}");
        let f = Fake::new(true);
        *f.mountinfo.borrow_mut() =
            format!("{ROOT}40 22 8:3 / /srv/rynas/system rw - ext4 /dev/sdc1 rw\n");
        assert!(ensure_with(&f, &|_| {}).unwrap_err().contains("not of the subvolume"));
        assert!(f.calls().is_empty());
    }

    #[test]
    fn symbolic_links_and_writable_folders_are_refused() {
        // /srv/rynas a link to elsewhere
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("srv")).unwrap();
        std::fs::create_dir_all(t.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(t.path().join("elsewhere"), t.path().join("srv/rynas")).unwrap();
        let e = ensure_system_dir(t.path(), false, &|_| {}).unwrap_err();
        assert!(e.contains("symbolic link"), "{e}");
        assert!(unavailable(t.path(), false).is_some_and(|e| e.contains("symbolic link")));
        // the folder a link to /
        let t = tempfile::tempdir().unwrap();
        ensure_system_dir(t.path(), false, &|_| {}).unwrap();
        let dir = under(t.path(), SYSTEM_VOLUME_DIR);
        std::fs::remove_dir(&dir).unwrap();
        std::os::unix::fs::symlink("/", &dir).unwrap();
        assert!(ensure_system_dir(t.path(), false, &|_| {}).unwrap_err().contains("symbolic link"));
        assert!(unavailable(t.path(), false).is_some());
        // a folder writable by others, not made by rynas: refused, not changed
        let t = tempfile::tempdir().unwrap();
        ensure_system_dir(t.path(), false, &|_| {}).unwrap();
        std::fs::set_permissions(dir_of(&t), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_system_dir(t.path(), false, &|_| {}).unwrap_err().contains("writable"));
        assert_eq!(std::fs::metadata(dir_of(&t)).unwrap().mode() & 0o777, 0o777);
        // a plain folder is made once, with mode 0755
        let t = tempfile::tempdir().unwrap();
        assert!(unavailable(t.path(), false).is_none());
        let d = ensure_system_dir(t.path(), false, &|_| {}).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().mode() & 0o7777, 0o755);
        assert_eq!(ensure_system_dir(t.path(), false, &|_| {}).unwrap(), d);
        assert!(unavailable(t.path(), false).is_none());
    }

    fn dir_of(t: &tempfile::TempDir) -> PathBuf {
        under(t.path(), SYSTEM_VOLUME_DIR)
    }
}
