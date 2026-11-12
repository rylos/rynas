//! Users as the kernel knows them, and becoming one.
//!
//! rynasd (root) starts every worker through [`become_raw`] between fork and exec: the
//! supplementary groups, the gid and the uid of the logged-in user, `PR_SET_NO_NEW_PRIVS`,
//! and no capability left — except for administrators, who read every folder and, where
//! Landlock fences it to the shares and removable drives, write over the mode bits and ACLs
//! too (see [`crate::caps`]). The kernel then applies the user's permissions and POSIX ACLs
//! to everything any other user's worker does, as it does for SMB.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub home: PathBuf,
}

/// Looks `name` up in /etc/passwd and /etc/group (local accounts: rynas's users are real
/// Linux users). The passwd spelling wins.
pub fn lookup(name: &str) -> io::Result<User> {
    let passwd = fs::read_to_string("/etc/passwd")?;
    let groups = fs::read_to_string("/etc/group").unwrap_or_default();
    parse(&passwd, &groups, name).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown user {name}")))
}

/// The user running this process (development: a rynasd that is not root runs the worker
/// as itself).
pub fn current() -> io::Result<User> {
    // SAFETY: plain getters
    let uid = unsafe { libc::getuid() };
    let passwd = fs::read_to_string("/etc/passwd")?;
    let name = passwd
        .lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() >= 7 && f[2].parse() == Ok(uid))
        .map(|f| f[0].to_string())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("uid {uid} has no passwd entry")))?;
    lookup(&name)
}

fn parse(passwd: &str, groups: &str, name: &str) -> Option<User> {
    let fields: Vec<&str> = passwd.lines().map(|line| line.split(':').collect::<Vec<_>>()).find(|f| f.len() >= 7 && f[0] == name)?;
    let (uid, gid) = (fields[2].parse().ok()?, fields[3].parse().ok()?);
    let mut list = vec![gid];
    for line in groups.lines() {
        let g: Vec<&str> = line.split(':').collect();
        if g.len() >= 4
            && g[3].split(',').any(|m| m == fields[0])
            && let Ok(id) = g[2].parse()
            && !list.contains(&id)
        {
            list.push(id);
        }
    }
    Some(User { name: fields[0].to_string(), uid, gid, groups: list, home: PathBuf::from(fields[5]) })
}

/// The names behind uids and gids, for the properties dialog.
pub fn names(file: &str, id: u32) -> Option<String> {
    let text = fs::read_to_string(file).ok()?;
    text.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() > 2 && f[2].parse() == Ok(id)).then(|| f[0].to_string())
    })
}

/// Becomes `uid`/`gid`/`groups` for good: from root (rynasd, between fork and exec) or
/// from a process that holds `CAP_SETUID`/`CAP_SETGID`. With [`crate::caps::Powers::Read`]
/// the process keeps `CAP_DAC_READ_SEARCH` (and passes it on to the programs it runs: 7z,
/// ffmpeg), with `Admin` also `CAP_DAC_OVERRIDE` and `CAP_FOWNER`, with `AppData` also
/// `CAP_CHOWN` (those never passed on), with `None` nothing at all. Never root. Safe between fork and exec: only raw system calls, no
/// allocation.
pub fn become_raw(uid: u32, gid: u32, groups: &[u32], powers: crate::caps::Powers) -> io::Result<()> {
    // never root: the capabilities would come back with uid 0, and nobody signs in to the
    // file manager as root (refused before any system call)
    if uid == 0 {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    let keeps = powers != crate::caps::Powers::None;
    crate::caps::limit_bounding(powers);
    // SAFETY: system calls with valid arguments
    unsafe {
        // keep the permitted set across setresuid, only for what caps::keep keeps
        if keeps && libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::setgroups(groups.len(), groups.as_ptr()) < 0
            || libc::setresgid(gid, gid, gid) < 0
            || libc::setresuid(uid, uid, uid) < 0
        {
            return Err(io::Error::last_os_error());
        }
        if libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0) < 0 || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if keeps {
        crate::caps::keep(powers)?;
    } else {
        crate::caps::clear()?;
    }
    // SAFETY: plain getters
    if unsafe { libc::getuid() != uid || libc::geteuid() != uid || libc::getegid() != gid } {
        return Err(io::Error::other("identity change did not take"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_users_and_groups() {
        let passwd = "root:x:0:0::/root:/bin/sh\nalice:x:1026:100:Alice R:/home/alice:/usr/sbin/nologin\n";
        let groups = "users:x:100:\nrynas-admin:x:990:admin,alice\nhttp:x:1023:\n";
        let user = parse(passwd, groups, "alice").unwrap();
        assert_eq!(user.name, "alice");
        assert_eq!((user.uid, user.gid), (1026, 100));
        assert_eq!(user.groups, vec![100, 990]);
        assert_eq!(user.home, PathBuf::from("/home/alice"));
        assert!(parse(passwd, groups, "nobody").is_none());
        // Linux user names are case-sensitive
        assert!(parse(passwd, groups, "Alice").is_none());
    }

    #[test]
    fn never_becomes_root() {
        // refused before any system call, so safe to try in a test
        let e = become_raw(0, 0, &[0], crate::caps::Powers::Read).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EPERM));
        let e = become_raw(0, 0, &[0], crate::caps::Powers::Admin).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EPERM));
        let e = become_raw(0, 0, &[0], crate::caps::Powers::AppData).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EPERM));
        let e = become_raw(0, 100, &[100], crate::caps::Powers::None).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EPERM));
    }
}
