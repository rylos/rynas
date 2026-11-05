//! rynas file services (phase 1, priority P2): local users and groups, SMB shares with
//! Samba 4.22, NFS exports with the kernel server, and the share × principal permission
//! matrix rendered as POSIX ACLs.
//!
//! Two configuration domains for [`rynas_config`]:
//! - [`users::UsersDomain`] (`users`, order 10): Linux users/groups in 1000–59999 and their
//!   Samba accounts, reconciled with `useradd`/`usermod`/`gpasswd`/`pdbedit`...;
//!   passwords through [`users::set_password`] only (never stored);
//! - [`shares::SharesDomain`] (`shares`, order 20): `smb.conf` (validated by `testparm`),
//!   Avahi service file, exports and `nfs.conf`, ACLs on the share roots, services.
//!
//! System access goes through [`accounts::Accounts`] and [`host::ShareHost`], with
//! in-memory implementations for tests and `rynasd --dry-system`.

pub mod accounts;
pub mod host;
pub mod nfs;
pub mod perm;
pub mod recycle;
pub mod shares;
pub mod smb;
pub mod users;
pub mod validate;

pub use accounts::{Accounts, HostAccounts, MemoryAccounts};
pub use host::{HostShares, MemoryShares, ShareHost, SubvolumeCreator};
pub use shares::{SHARES_KEY, SharesDomain};
pub use users::{USERS_KEY, UsersDomain};

#[cfg(test)]
mod tests;
