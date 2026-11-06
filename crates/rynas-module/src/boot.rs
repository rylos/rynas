//! A module's boot layer.
//!
//! On a plain Debian the boot loader reads the kernel from `/boot` of the root filesystem:
//! rynas shows the running and installed kernels and nothing more, and a rollback of the
//! system root is a console procedure. Some machines boot from a device of their own (a boot
//! flash with its own kernel entries, written by the system layer's tool). Their module
//! provides a [`BootLayer`]: the updates page then shows that device's entries and kernels,
//! and the root rollback points its boot entry at the restore point's kernel
//! ([`rynas_btrfs::rootfs::BootSync`]).

use rynas_api_types::updates::BootImage;
pub use rynas_btrfs::rootfs::BootSync;

pub trait BootLayer: BootSync {
    /// The boot device's entries and kernels (blocking: reading it may mount it).
    fn status(&self) -> Result<BootImage, String>;
}
