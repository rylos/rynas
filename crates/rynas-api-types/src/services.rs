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
    /// Home directory (the passwd home). Empty in a creation request: `/srv/rynas/homes/<name>`
    /// ([`HOMES_LINK`]) when the home goes on a volume ([`LocalUser::home_volume`]), else
    /// `/home/<name>`; created at creation. It cannot change through `PUT`: the home move
    /// (`POST /api/v1/services/homes/move`) copies the files and then changes it.
    #[serde(default)]
    pub home: String,
    /// Volume (mount point) whose homes folder holds the home, `<volume>/homes/<name>`; empty
    /// for a home elsewhere (the system disk). In a creation request: empty = the default
    /// volume of the user homes setting while the service is on, else the system disk;
    /// [`HOME_SYSTEM`] = the system disk (`/home/<name>`) even while the service is on; a
    /// mount point = that volume (the service must be on). It cannot change through `PUT`: the
    /// home move updates it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub home_volume: String,
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
            home_volume: String::new(),
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
    /// Ids accounts had before an id change (`POST /api/v1/ids/remap` with `new_id`). They
    /// are never given to a new account: files the walk did not reach (snapshots, backups,
    /// folders left out) still carry them. Kept by rynas; a document may drop entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired_ids: Vec<RetiredId>,
}

/// An id an account had before an id change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RetiredId {
    pub kind: IdKind,
    pub id: u32,
    /// The account that had it (why it is retired: `name` moved to `new_id`).
    pub name: String,
    pub new_id: u32,
    /// Day of the change, `YYYY-MM-DD`.
    #[serde(default)]
    pub date: String,
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
// Adoption of existing accounts, UID/GID changes and ownership remap
// ---------------------------------------------------------------------------------------------

/// Body of `POST /api/v1/users/adopt` and `POST /api/v1/groups/adopt`: accounts of the system
/// (1000–59999, created outside rynas) taken into rynas' configuration as they are (staged).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdoptRequest {
    /// Names to adopt; empty = every account not managed yet.
    #[serde(default)]
    pub names: Vec<String>,
}

/// A user (uid) or a group (gid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum IdKind {
    #[default]
    User,
    Group,
}

/// Body of `POST /api/v1/ids/plan` (dry run) and `POST /api/v1/ids/remap` (task). Exactly one
/// of `new_id` and `from_id`:
/// - `new_id`: change the account's uid/gid (`usermod -u` / `groupmod -g`), then give the
///   files owned by the old id to the new one;
/// - `from_id`: the account keeps its id; files owned by `from_id` (an id no account has,
///   e.g. data copied from another system with numeric owners) become the account's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IdRemapRequest {
    #[serde(default)]
    pub kind: IdKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_id: Option<u32>,
    /// Folders to walk (absolute, inside a data volume, `/home` or `/srv`). Empty: the
    /// suggested ones ([`IdRemapPlan::roots`]).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Rewrite POSIX ACL entries naming the old id too.
    #[serde(default = "default_true")]
    pub acls: bool,
    /// Entries counted by the plan (progress of the task only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<u64>,
}

/// What a walk found or changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RemapCounts {
    /// Entries looked at.
    pub scanned: u64,
    /// Entries whose owner changes (or changed).
    pub owners: u64,
    /// Entries whose group changes.
    pub groups: u64,
    /// POSIX ACL entries naming the old id.
    pub acl_entries: u64,
    /// Of those, entries dropped because the new id had its own entry on the same file (the
    /// new id's entry is kept as it is): the old id's permissions there are lost.
    #[serde(default)]
    pub acl_conflicts: u64,
    /// Files with such a conflict (at most 50).
    #[serde(default)]
    pub acl_conflict_paths: Vec<String>,
    /// Read-only subvolumes (snapshots) skipped: they keep the old id.
    #[serde(default)]
    pub readonly: Vec<String>,
    /// Mount points of other file systems inside the folders, not entered.
    #[serde(default)]
    pub other_fs: Vec<String>,
    pub errors: u64,
    #[serde(default)]
    pub error_samples: Vec<String>,
    /// The dry run stopped at its time limit: the numbers are lower bounds.
    #[serde(default)]
    pub partial: bool,
    #[serde(default)]
    pub cancelled: bool,
}

/// Answer of `POST /api/v1/ids/plan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IdRemapPlan {
    pub kind: IdKind,
    pub name: String,
    /// Files owned by this id...
    pub from: u32,
    /// ...become this id's.
    pub to: u32,
    /// The account's id changes (`new_id`), not only files.
    pub change: bool,
    /// Suggested folders: the data volumes and the user's home.
    pub roots: Vec<String>,
    /// The folders walked by the dry run (the request's or the suggested ones).
    pub paths: Vec<String>,
    pub allowed: bool,
    #[serde(default)]
    pub blockers: Vec<IdBlocker>,
    /// Ids of this kind retired by earlier id changes (never given to new accounts).
    #[serde(default)]
    pub retired: Vec<RetiredId>,
    /// The chosen `new_id` was retired by an earlier change of another account: files under
    /// it (snapshots, folders not walked then) may belong to that account (warn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_retired: Option<RetiredId>,
    /// `from_id` is this account's own retired id: the run finishes an earlier id change
    /// (cancelled or with folders left out), with the set-id bits of its files kept as in
    /// the change itself.
    #[serde(default)]
    pub resumes: bool,
    /// Dry-run counts; `None` when blocked before the walk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan: Option<RemapCounts>,
}

/// Why an id change or remap cannot run now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdBlocker {
    pub code: IdBlockerCode,
    /// Account name, id, pids, path... (shown as is).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdBlockerCode {
    /// Not a local account in 1000–59999 (root and system accounts are never changed).
    NotLocal,
    /// Created outside rynas: adopt it first (id changes only).
    Unmanaged,
    /// The id outside 1000–59999 (`new_id`), or below 1000 / reserved (`from_id`).
    OutOfRange,
    /// The new id is the current one.
    SameId,
    /// Another account has that id (`detail`: its name).
    IdInUse,
    /// The user has running processes (`detail`: pids).
    Processes,
    /// The user is connected over SMB (`detail`: number of sessions).
    SmbSessions,
    /// A folder is not absolute, does not exist or is outside the data folders.
    PathInvalid,
    /// Neither or both of `new_id` and `from_id`.
    BadRequest,
    #[serde(other)]
    Other,
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
    /// Label of a rynas volume, e.g. "vol1" (mounted at `/vol1`), or the system volume
    /// (`system`, its folder `/srv/rynas/system`: D-ROOTVOL).
    pub volume: String,
    /// false: `compression` property "none" (share profiles for video, photos, backups).
    #[serde(default = "share_yes")]
    pub compression: bool,
    /// No copy-on-write: `chattr +C` on the new, empty subvolume, so every file created in
    /// it is written in place (recordings, databases, VM images). Those files have no
    /// checksums and no compression (`compression` is ignored). Only used when the
    /// subvolume is created.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nocow: bool,
}

/// `POST /api/v1/shares/{name}/nocow`: turns no copy-on-write on or off for the share's
/// folder (task `shares.nocow`). On is refused unless the folder is empty; off is always
/// allowed and only changes the files created afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareNocowRequest {
    pub enabled: bool,
}

fn share_yes() -> bool {
    true
}

/// The administrators group of rynas ([`LocalUser::admin`]): created by the package, its
/// members may log in to rynas. A new shared folder gives it read and write by default.
pub const ADMIN_GROUP: &str = "rynas-admin";

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
    /// leaves the permissions of the directory alone. Nothing is filled in by the server
    /// (an existing share keeps meaning what it meant): a new folder (`subvolume`) is created
    /// owned by root with no access for anybody else, so an empty matrix there gives the
    /// warning `no_permissions_root_only`. Clients should send a matrix for a new folder;
    /// the rynas UI starts from the creating user and [`ADMIN_GROUP`] with read and write.
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// A read-only `#snapshot` folder at the share root with the share's snapshots (one
    /// folder each, named after the snapshot, `<prefix>.YYYYMMDDTHHMM`): a read-only bind
    /// mount of the snapshot directory of the share (`smb.shadow_copy.snapshot_dir`, set by
    /// its snapshot job). Off by default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub show_snapshots: bool,
    /// An encrypted folder (D-CRYPT E1, `docs/phase2/encrypted-folders.md`): `path` is the
    /// mount point `<place>/<name>` of the gocryptfs folder whose ciphertext is
    /// `<place>/.rynas-crypt/<name>`, made by `POST /api/v1/crypt/folders`. Needs no
    /// `subvolume` and no `show_snapshots`. Set by validation when `path` is one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub encrypted: bool,
    /// Offline deduplication of the folder (D-DEDUP, `docs/phase2/dedup.md`): the share is a
    /// root of its filesystem's job (`dedup` key). Refused on encrypted and no
    /// copy-on-write folders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup: Option<crate::dedup::ShareDedup>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Name of the snapshot folder inside a share ([`Share::show_snapshots`]).
pub const SNAPSHOT_FOLDER: &str = "#snapshot";

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
    /// The `[homes]` share: every user sees their own home folder as a share named after
    /// them. Staged together with the user homes setting (`PUT /api/v1/services/homes`).
    #[serde(default, skip_serializing_if = "SmbHomes::is_off")]
    pub homes: SmbHomes,
}

/// The SMB `[homes]` share ([`SmbSettings::homes`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SmbHomes {
    pub enabled: bool,
    #[serde(default)]
    pub recycle: RecycleSettings,
}

impl SmbHomes {
    fn is_off(&self) -> bool {
        *self == SmbHomes::default()
    }
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
            homes: SmbHomes::default(),
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
    /// Shares deleted by this change whose folder goes too, with its snapshots and recycle
    /// bin (set by `DELETE /api/v1/shares/{name}?remove_folder=true&confirm={name}`). The
    /// removal runs at the end of the apply, after the services stopped using the folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_folders: Vec<FolderRemoval>,
}

/// A share folder removed with its share ([`SharesConfig::remove_folders`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FolderRemoval {
    pub share: String,
    /// The share's path when the removal was asked for (nothing happens if it changed).
    pub path: String,
    /// Snapshot directories of the folder: every `<prefix>.<timestamp>` subvolume in them
    /// goes too.
    #[serde(default)]
    pub snapshots: Vec<SnapshotSource>,
}

/// A snapshot directory and the name prefix of the snapshots of one folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotSource {
    pub dir: String,
    pub prefix: String,
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
    /// The folder has the no copy-on-write attribute (`FS_NOCOW_FL`, `lsattr -d` shows
    /// `C`): files created in it are written in place, without checksums. `None` when the
    /// folder is not on btrfs or the attribute cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nocow: Option<bool>,
    /// Non-fatal remarks (extra options overriding managed keys, NFS clients need the
    /// same uids...) in English, for logs and the CLI; the UI translates `warning_items`.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// The same remarks as `warnings`, in the same order, with a stable code and the values
    /// their text names.
    #[serde(default)]
    pub warning_items: Vec<ShareWarning>,
    /// Encrypted folder state (only for `Share::encrypted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crypt: Option<CryptStatus>,
}

/// State of an encrypted folder, read from the system (never stored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CryptState {
    /// No ciphertext (`gocryptfs.conf`) for the path.
    #[default]
    Missing,
    /// Ciphertext there, nothing mounted: the path is an empty, immutable folder.
    Locked,
    /// Mounted (`fuse.gocryptfs` at the path).
    Unlocked,
    /// Not mounted at the path any more, but gocryptfs still runs: a container or service
    /// started while the folder was unlocked holds its own copy of the mount, so the files
    /// stay readable there. Not locked; a lock succeeds once that copy is gone.
    InUse,
}

/// [`ShareStatus::crypt`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptStatus {
    pub state: CryptState,
    /// The password is sealed on this machine and the folder is unlocked when rynasd starts.
    #[serde(default)]
    pub auto_unlock: bool,
    /// The ciphertext directory (`<place>/.rynas-crypt/<name>`): what snapshots, replication
    /// and backups of the folder hold.
    #[serde(default)]
    pub cipher_dir: String,
}

/// `GET /api/v1/crypt`: can encrypted folders be made here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptInfo {
    /// `gocryptfs` is installed (`POST /api/v1/crypt/install` installs it).
    pub installed: bool,
    /// `gocryptfs -version`, first word after the name (`2.5.1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The installed version can make encrypted folders (2.5 or newer: `-init -masterkey`).
    #[serde(default)]
    pub supported: bool,
    /// `systemd-creds has-tpm2` says yes: an automatic-unlock password is sealed with the
    /// TPM2 and the host key (`--with-key=host+tpm2`), else with the host key only.
    #[serde(default)]
    pub tpm2: bool,
}

/// Answer of `POST /api/v1/shares/{share}/unlock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptUnlockResponse {
    pub share: ShareView,
    /// Running containers that mount the folder: they were started while it was locked and
    /// still see the empty locked folder until they are restarted.
    #[serde(default)]
    pub containers: Vec<String>,
}

/// Answer of `POST /api/v1/shares/{share}/crypt/password`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptPasswordResponse {
    /// The password changed but the automatic unlock had to be turned off (the new password
    /// could not be sealed): the reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_unlock_off: Option<String>,
}

/// `POST /api/v1/crypt/folders/{volume}/{name}/recovery-key`: the recovery key again, read
/// with the folder's password (administrators).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptRecoveryKeyRequest {
    pub password: String,
}

/// `POST /api/v1/crypt/folders`: a new encrypted folder `<place>/<name>` (task
/// `crypt.create`); its recovery key is revealed once afterwards.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptCreateRequest {
    /// A place label: a rynas volume or `system`.
    pub volume: String,
    /// Folder name, the share name rules.
    pub name: String,
    pub password: String,
    /// Seal the password on this machine and unlock after every start of rynasd.
    #[serde(default)]
    pub auto_unlock: bool,
}

/// `POST /api/v1/shares/{share}/unlock`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptUnlockRequest {
    pub password: String,
}

/// `POST /api/v1/shares/{share}/crypt/password`: the current password, or the recovery key
/// with `recovery`, and the new password.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptPasswordRequest {
    pub current: String,
    #[serde(default)]
    pub recovery: bool,
    pub new_password: String,
}

/// `PUT /api/v1/shares/{share}/crypt/auto-unlock`: turning it on needs the password (checked).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CryptAutoUnlockRequest {
    pub enabled: bool,
    #[serde(default)]
    pub password: String,
}

macro_rules! crypt_redacted_debug {
    ($t:ident $(, $f:ident)*) => {
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let mut d = f.debug_struct(stringify!($t));
                $( d.field(stringify!($f), &self.$f); )*
                d.field("secret", &"<redacted>").finish()
            }
        }
    };
}
crypt_redacted_debug!(CryptCreateRequest, volume, name, auto_unlock);
crypt_redacted_debug!(CryptUnlockRequest);
crypt_redacted_debug!(CryptPasswordRequest, recovery);
crypt_redacted_debug!(CryptAutoUnlockRequest, enabled);
crypt_redacted_debug!(CryptRecoveryKeyRequest);

/// A remark of [`ShareStatus::warning_items`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareWarning {
    pub code: ShareWarningCode,
    #[serde(default, skip_serializing_if = "ShareWarningArgs::is_empty")]
    pub args: ShareWarningArgs,
}

/// Values named by a [`ShareWarning`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareWarningArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    /// SMB option name; the PC's name for `pc_backup_folder`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl ShareWarningArgs {
    pub fn is_empty(&self) -> bool {
        self.path.is_none() && self.volume.is_none() && self.key.is_none()
    }
}

/// Stable code of a share remark; unknown codes of a newer server read as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareWarningCode {
    /// `path` is created as a subvolume of `volume` on apply.
    SubvolumeOnApply,
    /// The same, without compression.
    SubvolumeOnApplyNoCompression,
    /// The same, without copy-on-write.
    SubvolumeOnApplyNocow,
    /// The folder has no copy-on-write and a snapshot job: snapshots work, but every block
    /// rewritten after a snapshot takes new space while the snapshot is kept.
    NocowSnapshots,
    /// `path` is not a btrfs subvolume.
    NotSubvolume,
    /// The SMB extra option `key` overrides a setting of rynas.
    ExtraOverrides,
    /// NFS clients must use the same uids.
    NfsUids,
    /// NFS is off in the service settings.
    NfsServiceOff,
    /// SMB is off in the service settings.
    SmbServiceOff,
    /// Time Machine without a size limit.
    TimemachineNoLimit,
    /// Multi-protocol preset without NFS.
    MultiprotocolWithoutNfs,
    /// No permissions: the folder's own permissions apply.
    NoPermissions,
    /// No permissions and the folder is (or, created on apply, will be) owned by root with
    /// no access for anybody else: only root can use the share. Given instead of
    /// `no_permissions`; the remedy is a permission matrix (the UI fills in the creating
    /// user and [`ADMIN_GROUP`] with read and write for a new folder).
    NoPermissionsRootOnly,
    /// Every entry denies access.
    AllDenied,
    /// The snapshot folder is on but the share has no snapshot job (no snapshot directory).
    SnapshotFolderNoJob,
    /// The snapshot folder is visible to NFS clients only when the export includes the
    /// mounts below it (`crossmnt`).
    SnapshotFolderNfs,
    /// The encrypted folder is locked: SMB and NFS clients cannot connect and apps using
    /// `path` see an empty, read-only folder until it is unlocked.
    CryptLocked,
    /// The share's folder holds, is, or lies inside the backup folder of the PC `key` at
    /// `path` (made before rynas refused it): the share is read-only in the file manager for
    /// administrators until the PC's backups move to a folder of their own.
    PcBackupFolder,
    #[serde(other)]
    Other,
}

/// An entry of `GET /api/v1/shares`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareView {
    pub share: Share,
    pub status: ShareStatus,
}

// ---------------------------------------------------------------------------------------------
// Folders: what a deletion removes, folders without a share
// ---------------------------------------------------------------------------------------------

/// `POST /api/v1/shares/plan/delete`: what deleting a share with its folder removes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareDeletePlanRequest {
    pub share: String,
}

/// A top-level folder of a rynas volume (`/<volume>/<name>`): `DELETE /api/v1/folders/{volume}/{name}`
/// and the body of `POST /api/v1/folders/plan/delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FolderRef {
    pub volume: String,
    pub name: String,
}

/// What a folder deletion removes and what still uses the folder (no side effect).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FolderDeletePlan {
    pub path: String,
    /// The share deleted with it, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    /// The folder is a btrfs subvolume.
    pub subvolume: bool,
    /// Space used by the files of the folder, its recycle bin included (one pass over the
    /// folder, bounded in time: `size_partial` means at least this much).
    pub size_bytes: u64,
    pub size_partial: bool,
    /// Snapshots deleted with the folder (their space is freed as far as nothing else
    /// shares it).
    pub snapshots: u32,
    /// The folder has a `#recycle` bin (deleted with it).
    pub recycle_bin: bool,
    /// What else uses the folder.
    #[serde(default)]
    pub usages: Vec<FolderUsage>,
    /// The deletion is possible now (`blockers` explains why not).
    pub allowed: bool,
    /// Usages that prevent the deletion (a subset of `usages`).
    #[serde(default)]
    pub blockers: Vec<FolderUsage>,
}

/// Something that uses a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderUsage {
    pub kind: FolderUsageKind,
    /// Share, job, task or container name.
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderUsageKind {
    /// The share's own NFS export (removed with the share).
    NfsExport,
    /// Another share uses the folder or a folder inside it.
    Share,
    /// A snapshot job takes snapshots of the folder.
    SnapshotJob,
    /// A scheduled task names the folder or the share.
    ScheduledTask,
    /// A container mounts the folder or something inside it.
    Container,
    /// The folder is not directly on a rynas volume: rynas does not delete it.
    NotOnVolume,
    #[serde(other)]
    Other,
}

/// An entry of `GET /api/v1/folders`: a top-level folder of a rynas volume that no share
/// uses (system folders such as `.snapshots`, `#recycle` or `@rynas-cache` are left out).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UnsharedFolder {
    pub volume: String,
    pub name: String,
    pub path: String,
    pub subvolume: bool,
    /// See [`FolderDeletePlan::size_bytes`]; `None` when it could not be read.
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub size_partial: bool,
    /// When `size_bytes` was measured: sizes are kept for an hour, and longer while the
    /// disks of the volume sleep.
    #[serde(default)]
    pub size_at_ms: Option<u64>,
    /// The disks of the volume are in standby: the size was not measured now, so as not to
    /// wake them (`size_bytes` is the last known one, if any).
    #[serde(default)]
    pub size_standby: bool,
    pub snapshots: u32,
    /// When the share of this folder was removed, if rynas knows.
    #[serde(default)]
    pub removed_at_ms: Option<u64>,
    /// The share as it was when it was removed (name, permissions and settings), to share
    /// the folder again.
    #[serde(default)]
    pub previous: Option<Share>,
}

/// `GET /api/v1/share-sizes`: the space each share's folder uses. Answered at once from
/// what rynasd already knows; sizes that are unknown or old are measured in the background
/// (low IO priority) and `measuring` stays true until they are done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareSizes {
    pub shares: Vec<ShareSize>,
    /// Some size is still being measured: ask again later for the result.
    #[serde(default)]
    pub measuring: bool,
}

/// The size of one share's folder ([`ShareSizes`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShareSize {
    pub name: String,
    pub path: String,
    /// Label of the volume holding the folder (`system` for the system volume); `None`
    /// for a folder outside the rynas volumes.
    #[serde(default)]
    pub volume: Option<String>,
    /// Bytes used by the files of the folder (nested subvolumes and mounts left out);
    /// `None` until first measured.
    #[serde(default)]
    pub bytes: Option<u64>,
    /// The walk of the files stopped at its time limit: the folder uses at least `bytes`.
    #[serde(default)]
    pub partial: bool,
    /// Read from the btrfs quota group of the subvolume: exact, kept for a minute only.
    #[serde(default)]
    pub exact: bool,
    /// When `bytes` was measured.
    #[serde(default)]
    pub at_ms: Option<u64>,
    /// A disk of the volume is in standby: the size is not measured now, so as not to wake
    /// it (`bytes` is the last known one, if any).
    #[serde(default)]
    pub standby: bool,
    /// The size is being measured now, or waits its turn.
    #[serde(default)]
    pub measuring: bool,
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

// ---------------------------------------------------------------------------------------------
// User homes (`/api/v1/services/homes`)
// ---------------------------------------------------------------------------------------------

/// Configuration key of the user homes service ([`HomesConfig`]).
pub const HOMES_KEY: &str = "homes";
/// Stable path of the homes: a root-owned folder (0711) on the system disk holding one
/// symbolic link per home on a volume, `/srv/rynas/homes/<user>` → `<volume>/homes/<user>`.
/// The passwd entries name the link, so a home can change volume without touching passwd,
/// Samba or SSH. (At first the folder itself was a link to `<volume>/homes`; rynasd turns
/// that layout into per-user links at start and at apply.)
pub const HOMES_LINK: &str = "/srv/rynas/homes";
/// Name of the homes folder on a volume.
pub const HOMES_FOLDER: &str = "homes";
/// Where homes on the system disk are (`/home/<user>`).
pub const SYSTEM_HOMES: &str = "/home";
/// "The system disk" as a home location ([`LocalUser::home_volume`] in a creation request,
/// [`HomeMoveRequest::to`]); volumes are named by their mount point, which starts with `/`.
pub const HOME_SYSTEM: &str = "system";

/// Document of the `homes` configuration key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomesConfig {
    /// New users get their home in `<volume>/homes/<user>` (a btrfs subvolume when the volume
    /// is btrfs), reached through `/srv/rynas/homes/<user>`, unless they are given another
    /// place. Off: new users get `/home/<user>`; homes already on volumes stay where they are
    /// and keep working.
    pub enabled: bool,
    /// Mount point of the default volume (`/volume1`, `/vol1`...): new users' homes go in its
    /// `homes` folder. Changing it moves no home.
    #[serde(default)]
    pub volume: String,
}

/// Body of `PUT /api/v1/services/homes`: the `homes` key plus the `[homes]` SMB share of the
/// `shares` key, staged together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomesSettings {
    pub enabled: bool,
    #[serde(default)]
    pub volume: String,
    /// The SMB `[homes]` share (only while the service is on).
    #[serde(default)]
    pub smb: SmbHomes,
}

/// A filesystem that can hold a homes folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomesVolume {
    /// Mount point.
    pub path: String,
    /// Label of a rynas volume, when it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub fs_type: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub avail_bytes: u64,
}

/// Where a user's home is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HomeLocation {
    /// In the homes folder of a volume ([`UserHome::volume`]), through [`HOMES_LINK`].
    Volume,
    /// Below `/home` (the system disk, usually).
    #[default]
    System,
    /// Somewhere else.
    Other,
    /// The folder does not exist (or its volume is not mounted).
    Missing,
}

/// The home of one local user (`HomesState::homes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct UserHome {
    pub user: String,
    pub uid: u32,
    /// The passwd home.
    pub home: String,
    pub location: HomeLocation,
    /// The real folder (the target of `/srv/rynas/homes/<user>` for a home on a volume).
    #[serde(default)]
    pub folder: String,
    /// Mount point of the volume of a home in a homes folder (also while it is missing).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub volume: String,
    /// The home is the root of a btrfs subvolume.
    #[serde(default)]
    pub subvolume: bool,
    /// A finished move whose old folder is kept until it is confirmed or undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved: Option<HomeMoveRecord>,
}

/// `GET/PUT /api/v1/services/homes`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomesState {
    /// Staged value if the current user has one, else the applied one.
    pub settings: HomesSettings,
    pub pending: bool,
    /// The service is on in the applied configuration: homes can move to volumes.
    #[serde(default)]
    pub active: bool,
    /// [`HOMES_LINK`].
    pub link: String,
    /// `<volume>/homes` of the default volume (empty without a volume).
    #[serde(default)]
    pub folder: String,
    /// The default volume is btrfs: homes on it are btrfs subvolumes.
    #[serde(default)]
    pub subvolume: bool,
    /// Filesystems that can hold a homes folder.
    #[serde(default)]
    pub volumes: Vec<HomesVolume>,
    /// Local users (uid 1000–59999) and where their home is.
    #[serde(default)]
    pub homes: Vec<UserHome>,
}

/// A moved home whose old folder is kept (renamed) until the move is confirmed
/// (`.../finish`: the old folder is deleted) or undone (`.../undo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomeMoveRecord {
    pub user: String,
    /// The previous passwd home.
    pub from: String,
    /// The new passwd home (the same as `from` between two volumes).
    pub to: String,
    /// The old folder, renamed (`<from_folder>.rynas-old`).
    pub kept: String,
    pub moved_ms: u64,
    /// The previous real folder (empty in the first records: `from`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from_folder: String,
    /// The new real folder (empty in the first records: behind `to`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to_folder: String,
}

/// Body of `POST /api/v1/services/homes/move` and `.../move/plan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomeMoveRequest {
    /// Users whose home moves; empty: every local user whose home exists, is not at the
    /// destination yet and has no move waiting for confirmation.
    #[serde(default)]
    pub users: Vec<String>,
    /// Destination: a volume's mount point (its homes folder), [`HOME_SYSTEM`]
    /// (`/home/<user>`), or empty: the default volume of the setting.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to: String,
}

/// What moving one home does, and what prevents it now (no side effect).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HomeMovePlan {
    pub user: String,
    /// The current passwd home.
    pub from: String,
    /// The real folder of the current home.
    #[serde(default)]
    pub source: String,
    /// The new passwd home (`/srv/rynas/homes/<user>` or `/home/<user>`).
    pub to: String,
    /// Destination: a volume's mount point or [`HOME_SYSTEM`].
    #[serde(default)]
    pub location: String,
    /// Where the files go (`<volume>/homes/<user>` or `/home/<user>`).
    pub target: String,
    /// The new home is created as a btrfs subvolume (the destination is btrfs).
    pub subvolume: bool,
    /// Space used by the home (bounded in time: `size_partial` means at least this much).
    pub size_bytes: u64,
    #[serde(default)]
    pub size_partial: bool,
    /// Free space at the destination.
    pub avail_bytes: u64,
    /// The home has SSH keys (`.ssh/authorized_keys`): they move with it.
    #[serde(default)]
    pub ssh_keys: bool,
    pub allowed: bool,
    #[serde(default)]
    pub blockers: Vec<HomeMoveBlocker>,
}

/// Why a home cannot move now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeMoveBlocker {
    pub code: HomeMoveBlockerCode,
    /// Process ids, share name, path... (shown as is).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeMoveBlockerCode {
    /// The destination is a volume and the user homes service is off (or not applied yet).
    ServiceOff,
    /// Not a local user in 1000–59999.
    NotLocal,
    /// The home is already at the destination.
    AlreadyMoved,
    /// The home folder does not exist.
    SourceMissing,
    /// The home is a symbolic link or a mount point, or has mounts inside.
    SourceNotPlain,
    /// The destination folder exists already.
    TargetExists,
    /// The destination is not a volume that can hold homes (`detail`: the path).
    TargetInvalid,
    /// The user has running processes (`detail`: pids).
    Processes,
    /// The user is connected over SMB (`detail`: number of sessions).
    SmbSessions,
    /// A share uses the home or a folder inside it (`detail`: share name).
    ShareInside,
    /// Not enough free space at the destination.
    NoSpace,
    /// rsync is not installed.
    NoRsync,
    /// A previous move of this user waits for confirmation.
    MoveWaiting,
    #[serde(other)]
    Other,
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
