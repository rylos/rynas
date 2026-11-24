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

/// Like [`user_groups`], telling a user that does not exist (`Ok(None)`, a clean answer of
/// NSS) from a lookup that failed (`Err`: a directory down, an I/O error). The mobile
/// devices revoke only on the first.
pub fn user_groups_checked(user: &str) -> Result<Option<Vec<String>>, String> {
    let Some(pw) = User::from_name(user).map_err(|e| format!("user lookup: {e}"))? else {
        return Ok(None);
    };
    let cname = CString::new(user).map_err(|_| "user name with a NUL".to_string())?;
    let gids = getgrouplist(&cname, pw.gid).map_err(|e| format!("group lookup: {e}"))?;
    let mut names = Vec::with_capacity(gids.len());
    for g in gids {
        match Group::from_gid(g) {
            Ok(Some(g)) => names.push(g.name),
            Ok(None) => {}
            Err(e) => return Err(format!("group lookup: {e}")),
        }
    }
    names.sort();
    names.dedup();
    Ok(Some(names))
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
