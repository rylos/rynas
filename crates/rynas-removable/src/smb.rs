//! SMB shares of mounted removable volumes (setting `share`, off by default).
//!
//! rynasd rewrites [`INCLUDE`] whenever the mounted volumes or the setting change, and asks
//! smbd to read its configuration again; the shares domain's `smb.conf` ends with an
//! `include` of that file. A share is removed (and its connections closed) before its volume
//! is unmounted, so a connected client never keeps a volume busy.
//!
//! Each share is private: only members of the access group (`valid users = @group`), read
//! only unless the setting says read-write, never guest. Filesystems without extended
//! attributes (FAT, exFAT, NTFS through ntfs3) get no streams module and no DOS attributes.

use rynas_api_types::removable::ShareAccess;

/// The file the shares domain includes at the end of `smb.conf`.
pub const INCLUDE: &str = "/etc/samba/rynas-removable.conf";

/// One share to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareSpec {
    pub name: String,
    pub path: String,
    pub comment: String,
    pub fs_type: String,
}

/// Share name for a mount point `/media/<dir>`: `usb-<dir>` with the characters Samba and
/// Windows dislike replaced, 80 characters at most.
pub fn share_name(mount_point: &str) -> String {
    let dir = mount_point.rsplit('/').next().unwrap_or(mount_point);
    let clean: String = dir
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut s = format!("usb-{}", clean.trim());
    if let Some((i, _)) = s.char_indices().nth(80) {
        s.truncate(i);
    }
    s
}

fn xattr_less(fs: &str) -> bool {
    matches!(
        fs,
        "vfat" | "exfat" | "ntfs" | "ntfs3" | "iso9660" | "udf" | "hfsplus"
    )
}

/// The include file for `shares`.
pub fn render(shares: &[ShareSpec], group: &str, access: ShareAccess) -> String {
    let mut s = String::from(
        "# Removable drives offered on the network by rynas (setting \"removable\").\n\
         # Rewritten whenever a drive is mounted or unmounted: do not edit.\n",
    );
    for sh in shares {
        s.push_str(&format!("\n[{}]\n", sh.name));
        let comment: String = sh.comment.chars().filter(|c| !c.is_control()).collect();
        let mut lines = vec![
            ("path", sh.path.clone()),
            ("comment", comment),
            ("browseable", "yes".into()),
            ("guest ok", "no".into()),
            ("valid users", format!("@{group}")),
            (
                "read only",
                if access == ShareAccess::ReadWrite {
                    "no"
                } else {
                    "yes"
                }
                .into(),
            ),
        ];
        if xattr_less(&sh.fs_type) {
            lines.push(("vfs objects", "catia".into()));
            lines.push(("ea support", "no".into()));
            lines.push(("store dos attributes", "no".into()));
        }
        for (k, v) in lines {
            s.push_str(&format!("   {k} = {v}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(share_name("/media/My Stick"), "usb-My Stick");
        assert_eq!(share_name("/media/a[b]"), "usb-a_b_");
        assert_eq!(
            share_name(&format!("/media/{}", "x".repeat(100)))
                .chars()
                .count(),
            80
        );
    }

    #[test]
    fn renders_private_shares() {
        let t = render(
            &[ShareSpec {
                name: "usb-STICK".into(),
                path: "/media/STICK".into(),
                comment: "SanDisk Ultra".into(),
                fs_type: "vfat".into(),
            }],
            "rynas-admin",
            ShareAccess::ReadOnly,
        );
        assert!(t.contains("[usb-STICK]\n   path = /media/STICK\n"));
        assert!(t.contains("valid users = @rynas-admin"));
        assert!(t.contains("read only = yes"));
        assert!(t.contains("vfs objects = catia"));
        assert!(t.contains("guest ok = no"));
        let t = render(&[], "users", ShareAccess::ReadWrite);
        assert!(!t.contains('['));
    }
}
