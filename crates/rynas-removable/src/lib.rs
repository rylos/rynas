//! rynas removable drives: USB disks and sticks, USB card readers, SD cards, optical drives.
//!
//! - [`scan`]: discovery from sysfs, the udev database and the mount table (no command, no
//!   udisks), with a cheap change signature so a tick every two seconds costs a few file
//!   reads;
//! - [`ops`]: mount (transient systemd mount units below `/media`), unmount (never lazy; a
//!   busy volume names the processes that hold it, [`holders`]), LUKS unlock/lock
//!   (`cryptsetup`, passphrase on standard input only), and safe removal: power off a USB
//!   drive, eject a medium, release an SD card ([`ops::Removable::safely_remove`]);
//! - [`mount`]: mount points and options per filesystem; [`smb`]: the network shares of
//!   mounted volumes.
//!
//! Everything that touches the machine goes through [`host::Host`] (tests use
//! [`host::FakeHost`] and fake sysfs trees from [`fixture`]). rynasd owns the service; the
//! file manager lists [`mounts`] next to the shared folders and ejects through
//! [`ops::Removable::safely_remove`] (or `POST /api/v1/removable/drives/{drive}/remove`).

pub mod fixture;
pub mod holders;
pub mod host;
pub mod mount;
pub mod ops;
pub mod scan;
pub mod smb;

pub use ops::{Removable, RemovableError};
pub use scan::{Scanner, Tree};

use rynas_api_types::removable::{RemovableDrive, RemovableMount};

/// The mounted removable volumes, as the file manager lists them next to the shared
/// folders (`removable: true`, `device` = volume, `device_name` = drive title).
pub fn mounts(drives: &[RemovableDrive]) -> Vec<RemovableMount> {
    drives
        .iter()
        .flat_map(|d| {
            d.volumes.iter().filter_map(move |v| {
                Some(RemovableMount {
                    removable: true,
                    device: v.name.clone(),
                    drive: d.name.clone(),
                    device_name: d.title.clone(),
                    label: v.display_label().unwrap_or("").to_string(),
                    fs_type: v.mounted_fs().to_string(),
                    mount_point: v.mount_point.clone()?,
                    read_only: v.read_only,
                    remove_method: d.remove_method,
                })
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
