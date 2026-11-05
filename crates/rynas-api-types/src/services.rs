//! Local users and groups, shares, SMB and NFS (`/api/v1/users`, `/api/v1/groups`,
//! `/api/v1/shares`, `/api/v1/services/{smb,nfs}`).
//!
//! Configuration keys: `users` ([`UsersConfig`]) and `shares` ([`SharesConfig`]). Writes
//! stage pending changes; `POST /api/v1/config/apply` applies them.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------------------------
// Local users and groups
// ---------------------------------------------------------------------------------------------

/// Lowest uid/gid of the users and groups rynas manages (Debian `adduser.conf`).
pub const LOCAL_ID_MIN: u32 = 1000;
/// Highest uid/gid of the users and groups rynas manages.
pub const LOCAL_ID_MAX: u32 = 59999;

/// What a user may do over SSH: nothing (default), SFTP only (group `rynas-sftp`) or a
/// shell (group `rynas-ssh`, login shell `/bin/bash`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ShellAccess {
    #[default]
    None,
    Sftp,
    Shell,
}

fn default_true() -> bool {
    true
}

/// A local Linux user managed by rynas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalUser {
    /// Login name: `[a-z_][a-z0-9_-]{0,31}`; cannot change.
    #[serde(default)]
    pub name: String,
    /// 1000..=59999; `0` in a creation request picks the next free one. Cannot change.
    #[serde(default)]
    pub uid: u32,
    /// Stored in the GECOS field.
    #[serde(default)]
    pub full_name: String,
    /// Kept by rynas only (notifications).
    #[serde(default)]
    pub email: String,
    /// Kept by rynas only.
    #[serde(default)]
    pub description: String,
    /// Supplementary groups managed by rynas (local groups in the id range). The primary
    /// group of users created by rynas is `users`.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Member of `rynas-admin`: may log in to rynas.
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub shell: ShellAccess,
    /// Account expiry date `YYYY-MM-DD` (the account stops working that day).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Disabled accounts cannot log in anywhere (shadow expiry, Samba `D` flag).
    #[serde(default)]
    pub disabled: bool,
    /// Has a Samba account (the password is synchronised by rynas).
    #[serde(default = "default_true")]
    pub smb: bool,
    /// Home directory; empty in a creation request: `/home/<name>`, created at creation.
    #[serde(default)]
    pub home: String,
}

impl Default for LocalUser {
    fn default() -> Self {
        Self {
            name: String::new(),
            uid: 0,
            full_name: String::new(),
            email: String::new(),
            description: String::new(),
            groups: Vec::new(),
            admin: false,
            shell: ShellAccess::None,
            expires: None,
            disabled: false,
            smb: true,
            home: String::new(),
        }
    }
}

/// A local Linux group managed by rynas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LocalGroup {
    /// `[a-z_][a-z0-9_-]{0,31}`; cannot change.
    #[serde(default)]
    pub name: String,
    /// 1000..=59999; `0` in a creation request picks the next free one. Cannot change.
    #[serde(default)]
    pub gid: u32,
    #[serde(default)]
    pub description: String,
}

/// Document of the `users` configuration key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UsersConfig {
    #[serde(default)]
    pub users: Vec<LocalUser>,
    #[serde(default)]
    pub groups: Vec<LocalGroup>,
    /// Users deleted by this change whose home directory goes too (set by
    /// `DELETE /api/v1/users/{name}?remove_home=true&confirm={name}`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_homes: Vec<String>,
}

/// What the current user's pending changeset does to an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PendingOp {
    #[default]
    Add,
    Change,
    Delete,
}

/// System state of a user, next to its configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UserStatus {
    /// In the applied configuration of rynas (users created outside rynas: `false`).
    pub managed: bool,
    /// Exists in `/etc/passwd`.
    pub on_system: bool,
    /// Has a Samba account; `None` when Samba is not installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_samba: Option<bool>,
    /// SMB access is on but there is no Samba account: set the password again.
    pub needs_password: bool,
    /// Change staged by the current user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingOp>,
    /// Short flags for the UI.
    #[serde(default)]
    pub badges: Vec<UserBadge>,
}

/// A flag of [`UserStatus::badges`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserBadge {
    /// On the system but not in rynas' configuration.
    Unmanaged,
    /// SMB access is on but Samba has no account: the password must be set again.
    SmbPasswordReset,
    Disabled,
    /// The account's expiry date has passed.
    Expired,
    /// In the configuration but not on the system.
    Missing,
    /// Member of an administrator group.
    Admin,
}

/// An entry of `GET /api/v1/users`: the configuration as the current user sees it (its
/// staged value if any) and the system state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UserView {
    pub user: LocalUser,
    pub status: UserStatus,
}

/// An entry of `GET /api/v1/groups`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GroupView {
    pub group: LocalGroup,
    /// Users of the configuration that are members.
    pub members: Vec<String>,
    /// In the applied configuration of rynas.
    pub managed: bool,
    /// Exists in `/etc/group`.
    pub on_system: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingOp>,
}

/// Body of `POST /api/v1/groups` and `PUT /api/v1/groups/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GroupRequest {
    #[serde(flatten)]
    pub group: LocalGroup,
    /// When present, the full member list (rewrites the `groups` of the users).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,
}

/// Body of `POST /api/v1/users/{name}/password`. Never stored by rynas: `chpasswd`
/// (yescrypt) and, with SMB access, `pdbedit` get it on their standard input.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PasswordRequest {
    pub password: String,
}

impl std::fmt::Debug for PasswordRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordRequest { password: <redacted> }")
    }
}

// ---------------------------------------------------------------------------------------------
// Shares, SMB and NFS
// ---------------------------------------------------------------------------------------------

/// Access of a principal to a share. `deny` beats everything, then `rw`, then `ro`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    #[default]
    Ro,
    Rw,
    Deny,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    #[default]
    User,
    Group,
}

/// One cell of the share × principal matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Permission {
    pub kind: PrincipalKind,
    pub name: String,
    pub access: Access,
}

/// SMB share presets (TrueNAS-style purposes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SmbPreset {
    /// Plain share, macOS-friendly (fruit).
    #[default]
    Standard,
    /// Also exported over NFS: oplocks and leases off on this share.
    Multiprotocol,
    /// Time Machine destination (`fruit:time machine`, `_adisk._tcp`).
    Timemachine,
    /// Folders with 100k+ files: `case sensitive = yes` (Windows clients see a
    /// case-sensitive share).
    Hugefolders,
}

/// `#recycle` bin of a share (vfs_recycle).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecycleSettings {
    pub enabled: bool,
    /// The cleanup task removes items deleted more than this many days ago; 0 keeps them.
    #[serde(default)]
    pub retention_days: u32,
}

impl Default for RecycleSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: 30,
        }
    }
}

/// "Previous versions" from btrbk snapshots (vfs_shadow_copy2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShadowCopySettings {
    /// Needs a snapshot directory.
    pub enabled: bool,
    /// Absolute directory holding the snapshots, e.g. `/srv/vol1/.snapshots`.
    #[serde(default)]
    pub snapshot_dir: String,
    /// Snapshot name prefix (btrbk `snapshot_name`; empty: last component of the share
    /// path); names are `<prefix>.YYYYMMDDTHHMM` (btrbk `timestamp_format short`/`long`).
    #[serde(default)]
    pub prefix: String,
}

fn default_veto() -> Vec<String> {
    ["@eaDir", "#snapshot", ".snapshots", "lost+found"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// SMB settings of one share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSmb {
    pub enabled: bool,
    #[serde(default)]
    pub preset: SmbPreset,
    #[serde(default = "default_true")]
    pub browseable: bool,
    #[serde(default)]
    pub recycle: RecycleSettings,
    #[serde(default)]
    pub shadow_copy: ShadowCopySettings,
    /// `server smb encrypt = required` on this share.
    #[serde(default)]
    pub encryption_required: bool,
    #[serde(default = "default_true")]
    pub hide_dot_files: bool,
    /// `veto files` patterns (no `/`).
    #[serde(default = "default_veto")]
    pub veto_files: Vec<String>,
    /// Time Machine preset: `fruit:time machine max size`, e.g. `1T`; empty: no limit.
    #[serde(default)]
    pub timemachine_max_size: String,
    /// Extra `key = value` lines appended to the share section, validated by `testparm`.
    #[serde(default)]
    pub extra: String,
}

impl Default for ShareSmb {
    fn default() -> Self {
        Self {
            enabled: true,
            preset: SmbPreset::Standard,
            browseable: true,
            recycle: RecycleSettings::default(),
            shadow_copy: ShadowCopySettings::default(),
            encryption_required: false,
            hide_dot_files: true,
            veto_files: default_veto(),
            timemachine_max_size: String::new(),
            extra: String::new(),
        }
    }
}

/// `root_squash` (default), `all_squash`, or `no_root_squash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NfsSquash {
    #[default]
    Root,
    All,
    None,
}

fn default_anon() -> u32 {
    65534
}

/// One client rule of an NFS export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NfsRule {
    /// Host name, IP address, CIDR subnet, wildcard (`*.lan`) or `*`.
    pub client: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub squash: NfsSquash,
    /// uid that squashed requests map to.
    #[serde(default = "default_anon")]
    pub anonuid: u32,
    /// gid that squashed requests map to.
    #[serde(default = "default_anon")]
    pub anongid: u32,
    /// `sync` (default, safe) or `async`.
    #[serde(default = "default_true")]
    pub sync: bool,
}

impl Default for NfsRule {
    fn default() -> Self {
        Self {
            client: String::new(),
            read_only: false,
            squash: NfsSquash::Root,
            anonuid: default_anon(),
            anongid: default_anon(),
            sync: true,
        }
    }
}

/// NFS settings of one share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareNfs {
    pub enabled: bool,
    #[serde(default)]
    pub rules: Vec<NfsRule>,
    /// Explicit `fsid=` (btrfs subvolumes share the filesystem UUID): the subvolume UUID
    /// when the path is a subvolume, else a generated UUID kept in the configuration.
    /// Empty in a request: filled in by rynas.
    #[serde(default)]
    pub fsid: String,
    /// `crossmnt`: also export subvolumes mounted below the path.
    #[serde(default)]
    pub crossmnt: bool,
}

/// Where a share's subvolume is created ([`Share::subvolume`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSubvolume {
    /// Label of a rynas volume, e.g. "vol1" (mounted at `/vol1`).
    pub volume: String,
    /// false: `compression` property "none" (share profiles for video, photos, backups).
    #[serde(default = "share_yes")]
    pub compression: bool,
}

fn share_yes() -> bool {
    true
}

/// A shared folder, exported over SMB and/or NFS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Share {
    /// Share name: `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`, unique ignoring case; cannot change.
    #[serde(default)]
    pub name: String,
    /// Existing directory on a local filesystem (ideally a btrfs subvolume). With
    /// `subvolume` it may be empty (then `/<volume>/<name>`) or not exist yet.
    #[serde(default)]
    pub path: String,
    /// Create the share's directory as a btrfs subvolume on a rynas volume when the
    /// configuration is applied (if it does not exist yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subvolume: Option<ShareSubvolume>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub smb: ShareSmb,
    #[serde(default)]
    pub nfs: ShareNfs,
    /// The share × principal matrix, rendered as POSIX ACLs on the share root (access and
    /// default) and as `valid users`/`write list`/`read list`/`invalid users`. Empty: rynas
    /// leaves the permissions of the directory alone.
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

fn default_workgroup() -> String {
    "WORKGROUP".into()
}

fn default_min_protocol() -> String {
    "SMB2_10".into()
}

/// Global SMB settings (`[global]` of `/etc/samba/smb.conf`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmbSettings {
    pub enabled: bool,
    #[serde(default = "default_workgroup")]
    pub workgroup: String,
    /// `server string`; empty: "rynas %h".
    #[serde(default)]
    pub server_string: String,
    /// `SMB2_10` (default), `SMB3_00` or `SMB3_11`.
    #[serde(default = "default_min_protocol")]
    pub min_protocol: String,
    /// Legacy NetBIOS name service (nmbd); off by default.
    #[serde(default)]
    pub netbios: bool,
    /// Interfaces or addresses to listen on; empty: all.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// `server smb encrypt = required` on every share.
    #[serde(default)]
    pub encryption_required: bool,
    /// WS-Discovery for Windows (wsdd2).
    #[serde(default = "default_true")]
    pub wsdd: bool,
    /// Bonjour/Avahi announcements for macOS and Linux.
    #[serde(default = "default_true")]
    pub mdns: bool,
    /// Extra `key = value` lines appended to `[global]`, validated by `testparm`.
    #[serde(default)]
    pub extra: String,
}

impl Default for SmbSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            workgroup: default_workgroup(),
            server_string: String::new(),
            min_protocol: default_min_protocol(),
            netbios: false,
            interfaces: Vec::new(),
            encryption_required: false,
            wsdd: true,
            mdns: true,
            extra: String::new(),
        }
    }
}

fn default_threads() -> u32 {
    8
}
fn default_mountd_port() -> u16 {
    20048
}
fn default_statd_port() -> u16 {
    32765
}
fn default_lockd_port() -> u16 {
    32768
}

/// Global NFS settings (`/etc/nfs.conf.d/rynas.conf`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NfsSettings {
    pub enabled: bool,
    /// NFSv3 (needs rpcbind, port 111); off by default: v4.1 and v4.2 only.
    #[serde(default)]
    pub v3: bool,
    #[serde(default = "default_threads")]
    pub threads: u32,
    /// Fixed ports for the firewall (NFSv3 side services).
    #[serde(default = "default_mountd_port")]
    pub mountd_port: u16,
    #[serde(default = "default_statd_port")]
    pub statd_port: u16,
    #[serde(default = "default_lockd_port")]
    pub lockd_port: u16,
}

impl Default for NfsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            v3: false,
            threads: default_threads(),
            mountd_port: default_mountd_port(),
            statd_port: default_statd_port(),
            lockd_port: default_lockd_port(),
        }
    }
}

/// Document of the `shares` configuration key: one key, because `smb.conf` and the
/// exports are rendered from the global settings and all the shares together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SharesConfig {
    #[serde(default)]
    pub smb: SmbSettings,
    #[serde(default)]
    pub nfs: NfsSettings,
    #[serde(default)]
    pub shares: Vec<Share>,
}

/// System state of a share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingOp>,
    /// The path exists and is a directory.
    pub path_ok: bool,
    /// Filesystem type of the path (`btrfs`, `ext4`...).
    #[serde(default)]
    pub fs_type: String,
    /// The path is the root of a btrfs subvolume.
    pub subvolume: bool,
    /// Non-fatal remarks (extra options overriding managed keys, NFS clients need the
    /// same uids...).
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// An entry of `GET /api/v1/shares`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareView {
    pub share: Share,
    pub status: ShareStatus,
}

/// `GET/PUT /api/v1/services/smb`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SmbState {
    /// Staged value if the current user has one, else the applied one.
    pub settings: SmbSettings,
    pub pending: bool,
    /// `smbd.service` is running.
    pub running: bool,
}

/// `GET/PUT /api/v1/services/nfs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NfsState {
    pub settings: NfsSettings,
    pub pending: bool,
    /// `nfs-server.service` is running.
    pub running: bool,
}

/// Effective access of a principal to a share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EffectiveAccess {
    /// No entry applies.
    #[default]
    None,
    Ro,
    Rw,
    /// An entry denies access.
    Deny,
}

/// A matrix entry that applies to the principal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionSource {
    pub kind: PrincipalKind,
    pub name: String,
    pub access: Access,
    /// This entry decides the result (deny > rw > ro).
    pub decisive: bool,
}

/// `GET /api/v1/shares/{name}/effective?principal=alice` (or `@group`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EffectivePermission {
    pub share: String,
    pub principal: String,
    pub access: EffectiveAccess,
    pub sources: Vec<PermissionSource>,
    /// E.g. "RW from group staff (also RO from user alice)".
    pub explanation: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_a_minimal_share() {
        let s: Share = serde_json::from_str(r#"{"name":"photos","path":"/srv/photos"}"#).unwrap();
        assert!(s.smb.enabled && s.smb.recycle.enabled && s.smb.hide_dot_files);
        assert_eq!(s.smb.veto_files.len(), 4);
        assert!(!s.nfs.enabled);
        let u: LocalUser = serde_json::from_str(r#"{"name":"alice"}"#).unwrap();
        assert!(u.smb && !u.admin && u.shell == ShellAccess::None);
        let r: NfsRule = serde_json::from_str(r#"{"client":"*"}"#).unwrap();
        assert!(r.sync && r.anonuid == 65534);
    }

    #[test]
    fn password_is_not_printed() {
        let p = PasswordRequest {
            password: "secret".into(),
        };
        assert!(!format!("{p:?}").contains("secret"));
    }
}
