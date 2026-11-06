//! The actions: mount, unmount, unlock, lock and safe removal, run as root by rynasd.
//!
//! Every action takes the drives of a fresh scan, checks the request against them and runs
//! its steps through [`Host`]. What "remove safely" does (as `udisksctl power-off` and
//! `eject`, without udisks):
//!
//! 1. every volume of the drive is unmounted (a busy one stops everything and names the
//!    processes that hold it) and every open LUKS container is closed;
//! 2. USB: the same for the other drives on the same USB device (the slots of a card
//!    reader), then each is flushed (`fsync`, `BLKFLSBUF`), sent SYNCHRONIZE CACHE and
//!    START STOP UNIT (failures only logged: many bridges do not implement them), and `1` is
//!    written to the USB device's `remove` attribute: the port powers the drive off;
//! 3. optical drives: `CDROMEJECT`; other removable media: START STOP UNIT with eject;
//! 4. SD cards in a built-in reader: nothing more, the card can be pulled.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::removable::{RemovableDrive, RemovableVolume, RemoveMethod, VolumeHolder};

use crate::holders;
use crate::host::Host;
use crate::mount::{self, Names};
use crate::scan::{FsSupport, Tree, usb_device_dir};

/// Longest mount or unmount (an unmount flushes the cache to a slow stick).
pub const MOUNT_TIMEOUT: Duration = Duration::from_secs(300);
const CRYPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Why an action was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovableError {
    NotFound(String),
    /// The volume is in use: the processes that hold it.
    Busy(String, Vec<VolumeHolder>),
    /// The request does not fit the state (mount a locked volume, unlock a plain one).
    Conflict(String),
    /// No driver for the filesystem, or a tool is missing.
    Unsupported(String),
    WrongPassphrase,
    Failed(String),
}

impl std::fmt::Display for RemovableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Unsupported(m) | Self::Failed(m) => {
                f.write_str(m)
            }
            Self::Busy(m, h) if h.is_empty() => write!(f, "{m}: in use"),
            Self::Busy(m, h) => write!(f, "{m}: in use by {}", holders::describe(h)),
            Self::WrongPassphrase => f.write_str("wrong passphrase"),
        }
    }
}

pub type Result<T> = std::result::Result<T, RemovableError>;

/// What an action needs besides the drives.
pub struct Ctx<'a> {
    /// Task log (command lines, outcomes). Never receives a secret.
    pub log: &'a dyn Fn(&str),
    /// Group owning the files of filesystems without Unix owners.
    pub gid: u32,
    /// Mount read-only (settings).
    pub read_only: bool,
    /// Called with a mount point before it is unmounted (removes its SMB share).
    pub before_unmount: &'a dyn Fn(&str),
}

/// Outcome of a mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounted {
    pub mount_point: String,
    pub read_only: bool,
    /// NTFS refused read-write (Windows left it dirty or hibernated): mounted read-only.
    pub ntfs_dirty: bool,
}

/// The removable drives service: one per rynasd.
pub struct Removable {
    host: Arc<dyn Host>,
    tree: Tree,
    names: Mutex<Names>,
    names_file: Option<PathBuf>,
    support: Mutex<FsSupport>,
}

pub fn find_volume<'a>(
    drives: &'a [RemovableDrive],
    name: &str,
) -> Option<(&'a RemovableDrive, &'a RemovableVolume)> {
    drives
        .iter()
        .find_map(|d| d.volumes.iter().find(|v| v.name == name).map(|v| (d, v)))
}

pub fn find_drive<'a>(drives: &'a [RemovableDrive], name: &str) -> Option<&'a RemovableDrive> {
    drives.iter().find(|d| d.name == name)
}

/// Kernel names are the only ids accepted in routes.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Removable {
    /// `names_file`: where the names chosen per UUID are kept (`None`: memory only).
    pub fn new(host: Arc<dyn Host>, names_file: Option<PathBuf>) -> Self {
        let tree = Tree::new(host.root().to_path_buf());
        let names = names_file.as_deref().map(Names::load).unwrap_or_default();
        let support = FsSupport::read(&tree);
        Self {
            host,
            tree,
            names: Mutex::new(names),
            names_file,
            support: Mutex::new(support),
        }
    }

    pub fn host(&self) -> &Arc<dyn Host> {
        &self.host
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    fn run(
        &self,
        ctx: &Ctx,
        argv: &[String],
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<crate::host::Output> {
        (ctx.log)(&format!("$ {}", argv.join(" ")));
        let o = self
            .host
            .run(argv, stdin, timeout)
            .map_err(RemovableError::Failed)?;
        if !o.ok() {
            (ctx.log)(&o.reason());
        }
        Ok(o)
    }

    /// Processes holding the filesystem mounted at `mount_point`.
    pub fn holders(&self, mount_point: &str) -> Vec<VolumeHolder> {
        match holders::mount_dev(&self.tree.path(mount_point)) {
            Some(dev) => holders::holders(&self.tree.path("/proc"), dev),
            None => Vec::new(),
        }
    }

    /// Checks that `volume` can be mounted now; the device and filesystem to mount.
    pub fn check_mount<'a>(
        &self,
        drives: &'a [RemovableDrive],
        volume: &str,
    ) -> Result<(&'a RemovableDrive, &'a RemovableVolume)> {
        let (d, v) = find_volume(drives, volume)
            .ok_or_else(|| RemovableError::NotFound(format!("no removable volume {volume}")))?;
        if v.luks && !v.unlocked {
            return Err(RemovableError::Conflict(format!(
                "{volume} is encrypted: unlock it first"
            )));
        }
        if !v.supported {
            return Err(RemovableError::Unsupported(format!(
                "no driver for the {} filesystem of {volume}",
                v.mounted_fs()
            )));
        }
        Ok((d, v))
    }

    /// Mounts a volume (the cleartext device of an unlocked LUKS volume) below
    /// [`mount::MOUNT_ROOT`]. Already mounted: its current mount point.
    pub fn mount(
        &self,
        drives: &[RemovableDrive],
        volume: &str,
        read_only: bool,
        ctx: &Ctx,
    ) -> Result<Mounted> {
        let (d, v) = self.check_mount(drives, volume)?;
        if let Some(mp) = &v.mount_point {
            return Ok(Mounted {
                mount_point: mp.clone(),
                read_only: v.read_only,
                ntfs_dirty: false,
            });
        }
        let (dev, fs, label) = match &v.clear {
            Some(c) if v.luks => (
                c.dev_path.clone(),
                c.fs_type.clone(),
                v.display_label().unwrap_or("").to_string(),
            ),
            _ => (v.dev_path.clone(), v.fs_type.clone(), v.label.clone()),
        };
        let driver = self.support.lock().unwrap().driver(&fs).ok_or_else(|| {
            RemovableError::Unsupported(format!("no driver for the {fs} filesystem of {volume}"))
        })?;
        let taken: HashSet<String> = drives
            .iter()
            .flat_map(|d| d.volumes.iter().filter_map(|v| v.mount_point.clone()))
            .collect();
        let root = self.tree.root().to_path_buf();
        let dir = self.names.lock().unwrap().choose(
            &label,
            v.uuid.as_deref(),
            &v.name,
            &taken,
            &|p| mount::non_empty_dir(&root, p),
            now_ms(),
        );
        if let Some(f) = &self.names_file {
            let n = self.names.lock().unwrap().clone();
            if let Err(e) = n.save(f) {
                tracing::warn!("{}: {e}", f.display());
            }
        }
        let ro = read_only || ctx.read_only || d.read_only;
        let spec = mount::spec(&fs, driver, ctx.gid, ro);
        self.host.make_dir(&dir).map_err(RemovableError::Failed)?;
        let systemd = self.host.systemd();
        let o = self.run(
            ctx,
            &mount::mount_argv(systemd, &dev, &dir, &spec),
            None,
            MOUNT_TIMEOUT,
        )?;
        if o.ok() {
            (ctx.log)(&format!("{dev} mounted at {dir}"));
            return Ok(Mounted {
                mount_point: dir,
                read_only: spec.read_only,
                ntfs_dirty: false,
            });
        }
        let mut reason = o.reason();
        // systemd-mount only says that the job failed: the kernel says why
        if systemd {
            reason = self.kernel_reason(&dev).unwrap_or(reason);
        }
        if driver == "ntfs3" && !spec.read_only && mount::ntfs_needs_windows(&reason) {
            (ctx.log)(
                "Windows left this NTFS volume in use (dirty or hibernated): mounting it read-only",
            );
            let spec = mount::spec(&fs, driver, ctx.gid, true);
            let o = self.run(
                ctx,
                &mount::mount_argv(systemd, &dev, &dir, &spec),
                None,
                MOUNT_TIMEOUT,
            )?;
            if o.ok() {
                return Ok(Mounted {
                    mount_point: dir,
                    read_only: true,
                    ntfs_dirty: true,
                });
            }
        }
        self.host.remove_empty_dir(&dir);
        Err(RemovableError::Failed(format!(
            "cannot mount {dev}: {reason}"
        )))
    }

    /// The last kernel messages about `dev` (ntfs3, exfat and vfat explain refusals there).
    fn kernel_reason(&self, dev: &str) -> Option<String> {
        let name = dev.rsplit('/').next().unwrap_or(dev);
        let argv: Vec<String> = [
            "journalctl",
            "-k",
            "-o",
            "cat",
            "--no-pager",
            "-n",
            "20",
            "--since",
            "-30s",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let o = self.host.run(&argv, None, Duration::from_secs(10)).ok()?;
        o.stdout
            .lines()
            .rev()
            .find(|l| l.contains(name))
            .map(|l| l.trim().to_string())
    }

    /// Checks that the volume can be unmounted now: mounted, not busy.
    pub fn check_unmount(&self, drives: &[RemovableDrive], volume: &str) -> Result<String> {
        let (_, v) = find_volume(drives, volume)
            .ok_or_else(|| RemovableError::NotFound(format!("no removable volume {volume}")))?;
        let mp = v
            .mount_point
            .clone()
            .ok_or_else(|| RemovableError::Conflict(format!("{volume} is not mounted")))?;
        Ok(mp)
    }

    fn unmount_at(&self, mp: &str, what: &str, ctx: &Ctx) -> Result<()> {
        (ctx.before_unmount)(mp);
        let h = self.holders(mp);
        if !h.is_empty() {
            return Err(RemovableError::Busy(format!("{what} ({mp})"), h));
        }
        let o = self.run(
            ctx,
            &mount::umount_argv(self.host.systemd(), mp),
            None,
            MOUNT_TIMEOUT,
        )?;
        if !o.ok() {
            let h = self.holders(mp);
            let reason = o.reason();
            if !h.is_empty() || reason.contains("busy") {
                return Err(RemovableError::Busy(format!("{what} ({mp})"), h));
            }
            return Err(RemovableError::Failed(format!(
                "cannot unmount {mp}: {reason}"
            )));
        }
        self.host.remove_empty_dir(mp);
        (ctx.log)(&format!("{mp} unmounted"));
        Ok(())
    }

    pub fn unmount(&self, drives: &[RemovableDrive], volume: &str, ctx: &Ctx) -> Result<()> {
        let mp = self.check_unmount(drives, volume)?;
        self.unmount_at(&mp, volume, ctx)
    }

    fn mapper_name(v: &RemovableVolume) -> String {
        match &v.uuid {
            Some(u) => format!("luks-{u}"),
            None => format!("luks-{}", v.name),
        }
    }

    pub fn check_unlock(&self, drives: &[RemovableDrive], volume: &str) -> Result<()> {
        let (_, v) = find_volume(drives, volume)
            .ok_or_else(|| RemovableError::NotFound(format!("no removable volume {volume}")))?;
        if !v.luks {
            return Err(RemovableError::Conflict(format!(
                "{volume} is not encrypted"
            )));
        }
        if v.unlocked {
            return Err(RemovableError::Conflict(format!(
                "{volume} is already unlocked"
            )));
        }
        if !self.host.has_program("cryptsetup") {
            return Err(RemovableError::Unsupported(
                "cryptsetup is not installed".into(),
            ));
        }
        Ok(())
    }

    /// Opens a LUKS volume with `passphrase` (on cryptsetup's standard input only).
    pub fn unlock(
        &self,
        drives: &[RemovableDrive],
        volume: &str,
        passphrase: &str,
        ctx: &Ctx,
    ) -> Result<()> {
        self.check_unlock(drives, volume)?;
        let (d, v) = find_volume(drives, volume).expect("checked");
        let mut argv: Vec<String> = ["cryptsetup", "open", "--type", "luks", "--key-file", "-"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if d.read_only || ctx.read_only {
            argv.push("--readonly".into());
        }
        argv.push(v.dev_path.clone());
        argv.push(Self::mapper_name(v));
        let o = self.run(ctx, &argv, Some(passphrase.as_bytes()), CRYPT_TIMEOUT)?;
        match o.code {
            Some(0) => {
                (ctx.log)(&format!("{} unlocked", v.dev_path));
                Ok(())
            }
            // cryptsetup: 2 = no key available with this passphrase
            Some(2) => Err(RemovableError::WrongPassphrase),
            _ => Err(RemovableError::Failed(format!(
                "cannot unlock {}: {}",
                v.dev_path,
                o.reason()
            ))),
        }
    }

    fn close_luks(&self, v: &RemovableVolume, ctx: &Ctx) -> Result<()> {
        let Some(c) = &v.clear else { return Ok(()) };
        if let Some(mp) = &v.mount_point {
            self.unmount_at(mp, &v.name, ctx)?;
        }
        let mapper = self
            .tree
            .read(&format!("/sys/block/{}/dm/name", c.name))
            .unwrap_or_else(|| c.name.clone());
        let argv: Vec<String> = vec!["cryptsetup".into(), "close".into(), mapper];
        let o = self.run(ctx, &argv, None, CRYPT_TIMEOUT)?;
        if !o.ok() {
            return Err(RemovableError::Failed(format!(
                "cannot lock {}: {}",
                v.dev_path,
                o.reason()
            )));
        }
        (ctx.log)(&format!("{} locked", v.dev_path));
        Ok(())
    }

    pub fn check_lock(&self, drives: &[RemovableDrive], volume: &str) -> Result<()> {
        let (_, v) = find_volume(drives, volume)
            .ok_or_else(|| RemovableError::NotFound(format!("no removable volume {volume}")))?;
        if !v.luks || !v.unlocked {
            return Err(RemovableError::Conflict(format!(
                "{volume} is not an unlocked encrypted volume"
            )));
        }
        Ok(())
    }

    /// Unmounts the cleartext filesystem of a LUKS volume and closes the container.
    pub fn lock(&self, drives: &[RemovableDrive], volume: &str, ctx: &Ctx) -> Result<()> {
        self.check_lock(drives, volume)?;
        let (_, v) = find_volume(drives, volume).expect("checked");
        self.close_luks(v, ctx)
    }

    /// Unmounts every volume of `d` and locks its LUKS containers.
    fn release(&self, d: &RemovableDrive, ctx: &Ctx) -> Result<()> {
        for v in &d.volumes {
            if v.luks {
                if v.unlocked {
                    self.close_luks(v, ctx)?;
                }
            } else if let Some(mp) = &v.mount_point {
                self.unmount_at(mp, &v.name, ctx)?;
            }
        }
        Ok(())
    }

    /// Drives on the same USB device as `d` (`d` included), e.g. the slots of a card reader.
    pub fn siblings<'a>(
        &self,
        drives: &'a [RemovableDrive],
        d: &RemovableDrive,
    ) -> Vec<&'a RemovableDrive> {
        let mine = usb_device_dir(&self.tree, &d.name);
        drives
            .iter()
            .filter(|x| {
                x.name == d.name || (mine.is_some() && usb_device_dir(&self.tree, &x.name) == mine)
            })
            .collect()
    }

    /// Checks that `drive` exists; the busy volumes of it (and of its siblings) refuse the
    /// removal before anything is unmounted.
    pub fn check_remove(&self, drives: &[RemovableDrive], drive: &str) -> Result<()> {
        let d = find_drive(drives, drive)
            .ok_or_else(|| RemovableError::NotFound(format!("no removable drive {drive}")))?;
        let group = if d.remove_method == RemoveMethod::PowerOff {
            self.siblings(drives, d)
        } else {
            vec![d]
        };
        for x in group {
            for v in &x.volumes {
                if let Some(mp) = &v.mount_point {
                    let h = self.holders(mp);
                    if !h.is_empty() {
                        return Err(RemovableError::Busy(format!("{} ({mp})", v.name), h));
                    }
                }
            }
        }
        Ok(())
    }

    /// Removes a drive safely (see the module documentation). Returns what was done.
    pub fn safely_remove(
        &self,
        drives: &[RemovableDrive],
        drive: &str,
        ctx: &Ctx,
    ) -> Result<RemoveMethod> {
        self.check_remove(drives, drive)?;
        let d = find_drive(drives, drive).expect("checked");
        match d.remove_method {
            RemoveMethod::PowerOff => {
                let group = self.siblings(drives, d);
                for x in &group {
                    self.release(x, ctx)?;
                }
                for x in &group {
                    if let Err(e) = self.host.flush_device(&x.dev_path) {
                        (ctx.log)(&e);
                    }
                    if let Err(e) = self.host.scsi_stop(&x.dev_path, false) {
                        (ctx.log)(&format!("{e} (ignored)"));
                    }
                }
                let usb = usb_device_dir(&self.tree, &d.name).ok_or_else(|| {
                    RemovableError::Failed(format!(
                        "{}: no USB device above it in sysfs",
                        d.dev_path
                    ))
                })?;
                let rel = usb
                    .strip_prefix(self.tree.root())
                    .map(|p| format!("/{}", p.display()))
                    .unwrap_or_else(|_| usb.display().to_string());
                (ctx.log)(&format!("powering off the USB device {rel}"));
                self.host
                    .write_sysfs(&format!("{rel}/remove"), "1")
                    .map_err(|e| {
                        RemovableError::Failed(format!("cannot power off {}: {e}", d.dev_path))
                    })?;
            }
            RemoveMethod::Eject => {
                self.release(d, ctx)?;
                if let Err(e) = self.host.flush_device(&d.dev_path) {
                    (ctx.log)(&e);
                }
                (ctx.log)(&format!("ejecting the medium of {}", d.dev_path));
                let r = if d.name.starts_with("sr") {
                    self.host.cdrom_eject(&d.dev_path)
                } else {
                    self.host.scsi_stop(&d.dev_path, true)
                };
                r.map_err(|e| RemovableError::Failed(format!("cannot eject {}: {e}", d.dev_path)))?;
            }
            RemoveMethod::Release => {
                self.release(d, ctx)?;
                if let Err(e) = self.host.flush_device(&d.dev_path) {
                    (ctx.log)(&e);
                }
            }
        }
        (ctx.log)(&format!("{} can be removed safely", d.title));
        Ok(d.remove_method)
    }

    /// Reads the filesystem drivers again (a package added one).
    pub fn refresh_support(&self) {
        *self.support.lock().unwrap() = FsSupport::read(&self.tree);
    }
}
