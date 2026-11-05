//! Group membership through NSS (files, LDAP, sssd... whatever `nsswitch.conf` says).

use std::ffi::CString;

use nix::unistd::{Group, User, getgrouplist};

/// Names of the primary and supplementary groups of `user`; `None` if the user does not exist.
pub fn user_groups(user: &str) -> Option<Vec<String>> {
    let pw = User::from_name(user).ok()??;
    let cname = CString::new(user).ok()?;
    let gids = getgrouplist(&cname, pw.gid).unwrap_or_else(|_| vec![pw.gid]);
    let mut names: Vec<String> = gids
        .into_iter()
        .filter_map(|g| Group::from_gid(g).ok().flatten().map(|g| g.name))
        .collect();
    names.sort();
    names.dedup();
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_nobody() {
        let g = user_groups("root").expect("root exists");
        assert!(g.iter().any(|n| n == "root"));
        assert!(user_groups("no-such-user-rynas-test").is_none());
    }
}
