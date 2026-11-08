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
pub mod crypt;
pub mod folders;
pub mod homes;
pub mod idremap;
pub mod host;
pub mod nfs;
pub mod nocow;
pub mod perm;
pub mod recycle;
pub mod removed;
pub mod shares;
pub mod smb;
pub mod snapview;
pub mod users;
pub mod validate;


pub use accounts::{Accounts, HostAccounts, MemoryAccounts};
pub use homes::{HomesDomain, HomesHost, HostHomes, MemoryHomes};
pub use host::{HostShares, MemoryShares, ShareHost, SubvolumeCreator};
pub use shares::{SHARES_KEY, SharesDomain};
pub use users::{USERS_KEY, UsersDomain};

/// A state drift of `key` with its code, its values and their English text.
pub(crate) fn drift(
    key: &str,
    object: String,
    kind: rynas_api_types::StateDriftKind,
    code: rynas_api_types::StateDriftCode,
    args: rynas_api_types::StateDriftArgs,
) -> rynas_api_types::StateDrift {
    rynas_api_types::StateDrift {
        key: key.into(),
        object,
        kind,
        detail: drift_text(code, &args),
        detail_code: Some(code),
        detail_args: args,
    }
}

/// The English text of a state drift (logs, CLI).
pub(crate) fn drift_text(
    code: rynas_api_types::StateDriftCode,
    a: &rynas_api_types::StateDriftArgs,
) -> String {
    use rynas_api_types::StateDriftCode as C;
    let name = a.name.as_deref().unwrap_or("?");
    let id = a.id.unwrap_or_default();
    let path = a.path.as_deref().unwrap_or("?");
    match code {
        C::SambaWithoutUser => format!("Samba account {name} has no Linux user"),
        C::UserUnmanaged => format!("{name} (uid {id}) was created outside rynas"),
        C::UserMissing => format!("{name} is configured but not in /etc/passwd"),
        C::UserMismatch => format!("{name}: account attributes differ from the configuration"),
        C::UserWithoutSamba => {
            format!("{name} has SMB access but no Samba account: set the password again")
        }
        C::GroupMissing => format!("group {name} is configured but not in /etc/group"),
        C::GroupUnmanaged => format!("group {name} (gid {id}) was created outside rynas"),
        C::SharePathMissing => format!("{path} does not exist any more"),
        C::ShareAclDiffers => format!(
            "ACL of {path} differs (group membership changed or edited by hand): missing {:?}, unexpected {:?}",
            a.missing, a.extra
        ),
        C::Other => String::new(),
    }
}

#[cfg(test)]
mod tests;
