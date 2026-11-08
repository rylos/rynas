//! `shares` domain: SMB shares and NFS exports of existing directories, the global SMB and
//! NFS settings, and the share × principal matrix rendered as POSIX ACLs.
//!
//! Files: `/etc/samba/smb.conf` (checked with `testparm -s` before it replaces the current
//! one), `/etc/avahi/services/rynas-smb.service`, `/etc/exports.d/rynas.exports`,
//! `/etc/nfs.conf.d/rynas.conf`. Reconcile: ACLs on the share roots, smbd/nmbd/wsdd2 and
//! nfs-server enabled or disabled, smbd reloaded (restarted when the listening setup
//! changes), `exportfs -ra`.
//!
//! Services whose unit is not installed are skipped with a log line: the configuration is
//! written anyway and takes effect when the package is installed.
//!
//! Snapshot folders (`show_snapshots`): one mount unit per share and the target that pulls
//! them in ([`crate::snapview`]). At the end of a reconcile, shares that went away are either
//! recorded with their settings ([`crate::removed`]) or, when deleted with their folder
//! (`remove_folders`), removed with their snapshots ([`crate::folders::remove_folder`]).
//! Limit: a deleted folder cannot come back on rollback.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use rynas_api_types::{
    Access, CryptState, PrincipalKind, Share, ShareWarning, ShareWarningArgs, ShareWarningCode, SharesConfig,
    SmbPreset, SmbSettings, StateDrift, StateDriftArgs, StateDriftCode, StateDriftKind,
};
use rynas_config::files::HEADER_PREFIX;
use rynas_config::{Applier, ApplyCtx, Candidate, Invalid, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::accounts::Accounts;
use crate::crypt::CryptHost;
use crate::host::{ShareHost, SubvolumeCreator};
use crate::perm::{AclPlan, acl_plan};
use crate::removed::RemovedShares;
use crate::validate::{self, account_name, one_line, share_name, share_path};
use crate::{folders, nfs, smb, snapview};

pub const SHARES_KEY: &str = "shares";

pub const SMBD: &str = "smbd.service";
pub const NMBD: &str = "nmbd.service";
pub const WSDD2: &str = "wsdd2.service";
pub const NFS_SERVER: &str = "nfs-server.service";

pub fn parse(v: Option<&Value>) -> Result<SharesConfig, String> {
    match v {
        Some(v) => {
            serde_json::from_value(v.clone()).map_err(|e| format!("invalid shares document: {e}"))
        }
        None => Ok(SharesConfig::default()),
    }
}

pub struct SharesDomain {
    host: Arc<dyn ShareHost>,
    accounts: Arc<dyn Accounts>,
    /// Creates the subvolumes shares ask for; without it such shares are refused.
    subvolumes: Option<Arc<dyn SubvolumeCreator>>,
    /// Shares removed while their folder stayed.
    removed: Arc<RemovedShares>,
    /// Sizes of the folders without a share, cleared by every apply.
    sizes: Arc<crate::folders::SizeCache>,
    /// Encrypted folders (D-CRYPT E1); in memory and empty unless set.
    crypt: Arc<dyn CryptHost>,
}

/// Snapshot folders of a document: mount unit → (share, share path, snapshot directory).
fn views(c: &SharesConfig) -> BTreeMap<String, (String, String, String)> {
    c.shares
        .iter()
        .filter_map(|s| {
            snapview::wanted(s)
                .map(|(p, d)| (snapview::unit_name(&p), (s.name.clone(), p, d)))
        })
        .collect()
}

fn warn(code: ShareWarningCode) -> ShareWarning {
    ShareWarning {
        code,
        args: ShareWarningArgs::default(),
    }
}

/// English text of a remark (logs, CLI, `ShareStatus.warnings`).
pub fn warning_text(w: &ShareWarning) -> String {
    let a = &w.args;
    let path = a.path.as_deref().unwrap_or("");
    let volume = a.volume.as_deref().unwrap_or("");
    match w.code {
        ShareWarningCode::SubvolumeOnApply => format!(
            "{path} will be created as a subvolume of volume {volume} when the change is applied"
        ),
        ShareWarningCode::SubvolumeOnApplyNoCompression => format!(
            "{path} will be created as a subvolume of volume {volume}, without compression, when the change is applied"
        ),
        ShareWarningCode::SubvolumeOnApplyNocow => format!(
            "{path} will be created as a subvolume of volume {volume}, without copy-on-write, when the change is applied"
        ),
        ShareWarningCode::NocowSnapshots => format!(
            "{path} has no copy-on-write and a snapshot job: snapshots work, but data rewritten after a snapshot takes new space while the snapshot is kept"
        ),
        ShareWarningCode::NotSubvolume => format!(
            "{path} is not a btrfs subvolume: snapshots, previous versions and quotas need one"
        ),
        ShareWarningCode::ExtraOverrides => format!(
            "the SMB extra option '{}' overrides a setting of rynas",
            a.key.as_deref().unwrap_or("")
        ),
        ShareWarningCode::NfsUids => {
            "NFS (sec=sys) trusts client uids: clients must use the same uids as this NAS".into()
        }
        ShareWarningCode::NfsServiceOff => "NFS is disabled in the NFS service settings".into(),
        ShareWarningCode::SmbServiceOff => "SMB is disabled in the SMB service settings".into(),
        ShareWarningCode::TimemachineNoLimit => {
            "Time Machine without a size limit can fill the volume".into()
        }
        ShareWarningCode::MultiprotocolWithoutNfs => "the multi-protocol preset turns off SMB client caching; it is meant for shares also exported over NFS".into(),
        ShareWarningCode::NoPermissions => {
            "no permissions: the directory's own permissions apply (rynas does not set ACLs)".into()
        }
        ShareWarningCode::AllDenied => {
            "every entry denies access: nobody can use this share".into()
        }
        ShareWarningCode::SnapshotFolderNoJob => "the snapshot folder needs a snapshot job for this share: it appears once the share has one".into(),
        ShareWarningCode::SnapshotFolderNfs => "NFS clients see the snapshot folder only when the export includes the mounts below it (crossmnt)".into(),
        ShareWarningCode::CryptLocked => format!(
            "{path} is encrypted and locked: SMB and NFS clients cannot connect; apps and containers started while it is locked see an empty folder and must be restarted after it is unlocked"
        ),
        ShareWarningCode::Other => String::new(),
    }
}

/// Since when (ms, whole seconds) rynas's `smb.conf` carries `multicast dns register = no`:
/// an smbd started before that still announces itself through Avahi (`_smb._tcp` and
/// `_device-info._tcp` under its NetBIOS name, `model=MacSamba`), which collides with
/// rynas's own service file, and only a restart stops it (Samba reads the parameter once, at
/// start: `source3/smbd/server.c`).
pub const SMBD_MDNS_MARKER: &str = "/var/lib/rynas/smbd-mdns-off";

/// Does smbd need a restart (not just a reload) to go from `a` to `b`?
fn smb_needs_restart(a: &SmbSettings, b: &SmbSettings) -> bool {
    a.interfaces != b.interfaces || a.netbios != b.netbios || a.min_protocol != b.min_protocol
}

fn smb_running(c: &SharesConfig) -> bool {
    c.smb.enabled
}

fn nfs_running(c: &SharesConfig) -> bool {
    c.nfs.enabled && c.shares.iter().any(|s| s.nfs.enabled)
}

impl SharesDomain {
    /// The running smbd was started before rynas's `multicast dns register = no` (marker
    /// [`SMBD_MDNS_MARKER`], written now when missing): it still announces itself.
    fn smbd_announces_itself(&self, ctx: &ApplyCtx) -> bool {
        let p = ctx.path(Path::new(SMBD_MDNS_MARKER));
        let since = match std::fs::read_to_string(&p)
            .ok()
            .and_then(|t| t.trim().parse::<i64>().ok())
        {
            Some(t) => t,
            None => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64 * 1000)
                    .unwrap_or(0);
                let _ = p
                    .parent()
                    .map(std::fs::create_dir_all)
                    .transpose()
                    .and_then(|_| std::fs::write(&p, format!("{now}\n")));
                now
            }
        };
        self.host
            .unit_started_ms(SMBD)
            .is_some_and(|started| started < since)
    }

    pub fn new(host: Arc<dyn ShareHost>, accounts: Arc<dyn Accounts>) -> Self {
        Self {
            host,
            accounts,
            subvolumes: None,
            removed: Arc::new(RemovedShares::memory()),
            sizes: Arc::default(),
            crypt: Arc::new(crate::crypt::MemoryCrypt::default()),
        }
    }

    /// The encrypted folders backend ([`crate::crypt::HostCrypt`] on the machine).
    pub fn with_crypt(mut self, c: Arc<dyn CryptHost>) -> Self {
        self.crypt = c;
        self
    }

    pub fn crypt(&self) -> &Arc<dyn CryptHost> {
        &self.crypt
    }

    /// An encrypted share whose folder is not unlocked now: its ACL cannot be read or written.
    pub fn crypt_closed(&self, s: &Share) -> bool {
        s.encrypted && self.crypt.state(&s.path) != CryptState::Unlocked
    }

    /// Writes the ACL of a share now (an encrypted folder right after its unlock: the apply
    /// skipped it while locked). Nothing for a share without permissions.
    pub fn write_acl(&self, s: &Share) -> Result<(), String> {
        if s.permissions.is_empty() {
            return Ok(());
        }
        let plan = self.acl_for(s)?;
        self.host.set_acl(&s.path, &plan)
    }

    /// Sizes of the folders without a share ([`crate::folders::unshared`]).
    pub fn sizes(&self) -> &Arc<crate::folders::SizeCache> {
        &self.sizes
    }

    /// A disk under the volume mounted at `mount` sleeps ([`SubvolumeCreator::asleep`]).
    pub fn asleep(&self, mount: &str) -> bool {
        self.subvolumes.as_ref().is_some_and(|c| c.asleep(mount))
    }

    /// Where shares removed without their folder are recorded.
    pub fn with_removed(mut self, r: Arc<RemovedShares>) -> Self {
        self.removed = r;
        self
    }

    pub fn removed(&self) -> &Arc<RemovedShares> {
        &self.removed
    }

    /// The mounted rynas volumes (label, mount point), when this rynasd manages volumes.
    pub fn volumes(&self) -> Vec<(String, String)> {
        self.subvolumes
            .as_ref()
            .map(|c| c.volumes())
            .unwrap_or_default()
    }

    /// Lets shares ask for their subvolume ([`Share::subvolume`]).
    pub fn with_subvolumes(mut self, c: Arc<dyn SubvolumeCreator>) -> Self {
        self.subvolumes = Some(c);
        self
    }

    pub fn host(&self) -> &Arc<dyn ShareHost> {
        &self.host
    }

    /// The ACL of a share with the group membership of the system now.
    pub fn acl_for(&self, s: &Share) -> Result<AclPlan, String> {
        let groups = self.accounts.groups()?;
        let users = self.accounts.users()?;
        let groups_of = |u: &str| -> Vec<String> {
            let primary = users.iter().find(|x| x.name == u).map(|x| x.gid);
            groups
                .iter()
                .filter(|g| Some(g.gid) == primary || g.members.iter().any(|m| m == u))
                .map(|g| g.name.clone())
                .collect()
        };
        let members_of = |g: &str| -> Vec<String> {
            let Some(grp) = groups.iter().find(|x| x.name == g) else {
                return Vec::new();
            };
            let mut m: BTreeSet<String> = grp.members.iter().cloned().collect();
            m.extend(
                users
                    .iter()
                    .filter(|u| u.gid == grp.gid)
                    .map(|u| u.name.clone()),
            );
            m.into_iter().collect()
        };
        Ok(acl_plan(&s.permissions, &groups_of, &members_of))
    }

    /// Checks and normalizes a document (fills in NFS fsids).
    pub fn normalize(&self, mut c: SharesConfig) -> Result<SharesConfig, Invalid> {
        // global SMB
        let g = &mut c.smb;
        g.workgroup = g.workgroup.trim().to_ascii_uppercase();
        if !rynas_api_types::validate::workgroup(&g.workgroup) {
            return Err(Invalid::at(
                "workgroup",
                format!(
                    "invalid workgroup {:?}: 1-15 letters, digits, '-' or '_'",
                    g.workgroup
                ),
            ));
        }
        g.server_string = g.server_string.trim().to_string();
        one_line(&g.server_string, 64)
            .map_err(|e| Invalid::at("server_string", format!("server string {e}")))?;
        if !["SMB2_10", "SMB3_00", "SMB3_11"].contains(&g.min_protocol.as_str()) {
            return Err(Invalid::at(
                "min_protocol",
                format!(
                    "invalid minimum protocol {:?}: SMB2_10, SMB3_00 or SMB3_11",
                    g.min_protocol
                ),
            ));
        }
        g.interfaces.sort();
        g.interfaces.dedup();
        if let Some(i) = g.interfaces.iter().find(|i| !validate::interface(i)) {
            return Err(Invalid::at(
                "interfaces",
                format!("invalid interface {i:?}"),
            ));
        }
        validate::smb_extra(&g.extra)
            .map_err(|e| Invalid::at("extra", format!("SMB extra options: {e}")))?;
        if g.homes.recycle.retention_days > 3650 {
            return Err(Invalid::at(
                "smb.homes.recycle.retention_days",
                "the recycle bin keeps items at most 3650 days",
            ));
        }
        if g.homes.enabled
            && let Some(sh) = c.shares.iter().find(|s| s.name.eq_ignore_ascii_case("homes"))
        {
            return Err(Invalid::at(
                "smb.homes",
                format!(
                    "share {} would hide the SMB home folders ([homes]): rename the share or keep the home folders off SMB",
                    sh.name
                ),
            ));
        }

        // global NFS
        let n = &c.nfs;
        if !(1..=128).contains(&n.threads) {
            return Err("NFS threads must be between 1 and 128".into());
        }
        let ports = [
            n.mountd_port,
            n.statd_port,
            n.statd_port.saturating_add(1),
            n.lockd_port,
        ];
        if ports.contains(&0) || ports.iter().collect::<BTreeSet<_>>().len() != ports.len() {
            return Err(
                "NFS ports must be different and not 0 (statd also uses its port + 1)".into(),
            );
        }

        // shares
        let mut names = BTreeSet::new();
        for s in &mut c.shares {
            s.name = s.name.trim().to_string();
            s.description = s.description.trim().to_string();
            let name = s.name.clone();
            if !share_name(&name) {
                return Err(Invalid::at(
                    "name",
                    format!(
                        "invalid share name {name:?}: 1-64 letters, digits, '.', '_' or '-', starting with a letter or digit"
                    ),
                ));
            }
            if !names.insert(name.to_ascii_lowercase()) {
                return Err(Invalid::at(
                    "name",
                    format!("share {name} exists twice (names ignore case)"),
                ));
            }
            one_line(&s.description, 256).map_err(|e| {
                Invalid::at("description", format!("share {name}: description {e}"))
            })?;
            // a share that asks for its subvolume: the path is /<volume mount>/<name>; it
            // is created by the apply when it does not exist yet
            let missing = s.path.is_empty() || !self.host.probe(&s.path).exists;
            if let (Some(sv), true) = (&s.subvolume, missing) {
                let c = self.subvolumes.as_ref().ok_or_else(|| {
                    format!("share {name}: subvolumes cannot be created by this rynasd")
                })?;
                let base = c
                    .volume_path(&sv.volume)
                    .map_err(|e| Invalid::at("path", format!("share {name}: {e}")))?;
                let want = format!("{}/{name}", base.trim_end_matches('/'));
                if s.path.is_empty() {
                    s.path = want.clone();
                }
                if s.path != want {
                    return Err(Invalid::at(
                        "path",
                        format!(
                            "share {name}: a subvolume on volume {} is created at {want}",
                            sv.volume
                        ),
                    ));
                }
            }
            share_path(&s.path).map_err(|e| Invalid::at("path", format!("share {name}: {e}")))?;
            // encrypted folders (D-CRYPT E1): the mount point of a folder made by
            // `POST /crypt/folders`; a share on one is always marked encrypted
            let crypt = self.crypt.state(&s.path);
            if crypt != CryptState::Missing {
                s.encrypted = true;
            }
            if s.encrypted {
                if crypt == CryptState::Missing {
                    return Err(Invalid::at(
                        "path",
                        format!("share {name}: {} is not an encrypted folder: create it first", s.path),
                    ));
                }
                if s.subvolume.is_some() {
                    return Err(Invalid::at(
                        "subvolume",
                        format!("share {name}: an encrypted folder has its own ciphertext subvolume"),
                    ));
                }
                if s.show_snapshots {
                    return Err(Invalid::at(
                        "show_snapshots",
                        format!("share {name}: the snapshot folder is not available on an encrypted folder (its snapshots hold ciphertext)"),
                    ));
                }
            }
            let info = self.host.probe(&s.path);
            if !info.exists && s.subvolume.is_some() {
                // created on apply: nothing more to check now
            } else if !info.exists {
                return Err(Invalid::at(
                    "path",
                    format!("share {name}: {} does not exist", s.path),
                ));
            } else if !info.is_dir {
                return Err(Invalid::at(
                    "path",
                    format!("share {name}: {} is not a directory", s.path),
                ));
            }
            // an unlocked encrypted folder is a FUSE mount
            if info.remote && !s.encrypted {
                return Err(Invalid::at(
                    "path",
                    format!(
                        "share {name}: {} is on a {} filesystem; shares must be on a local filesystem",
                        s.path, info.fs_type
                    ),
                ));
            }

            // permissions
            s.permissions
                .sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
            for w in s.permissions.windows(2) {
                if w[0].kind == w[1].kind && w[0].name == w[1].name {
                    return Err(Invalid::at(
                        "permissions",
                        format!(
                            "share {name}: {} is listed twice in the permissions",
                            w[0].name
                        ),
                    ));
                }
            }
            for p in &s.permissions {
                if !account_name(&p.name) {
                    return Err(Invalid::at(
                        "permissions",
                        format!("share {name}: invalid principal {:?}", p.name),
                    ));
                }
            }

            // SMB
            let smb = &mut s.smb;
            smb.veto_files.retain(|v| !v.trim().is_empty());
            for v in &smb.veto_files {
                if v.contains('/') || v.chars().any(char::is_control) || v.len() > 255 {
                    return Err(Invalid::at(
                        "smb.veto_files",
                        format!("share {name}: invalid veto pattern {v:?}"),
                    ));
                }
            }
            if smb.recycle.retention_days > 3650 {
                return Err(Invalid::at(
                    "smb.recycle.retention_days",
                    format!("share {name}: recycle retention is at most 3650 days"),
                ));
            }
            if smb.shadow_copy.enabled {
                let d = &smb.shadow_copy.snapshot_dir;
                if d.is_empty() {
                    return Err(Invalid::at(
                        "smb.shadow_copy.snapshot_dir",
                        format!("share {name}: previous versions need the snapshot directory"),
                    ));
                }
                if !d.starts_with('/')
                    || d.chars().any(|c| c.is_control() || c == '"' || c == '\\')
                    || d[1..]
                        .split('/')
                        .any(|c| c.is_empty() || c == "." || c == "..")
                {
                    return Err(Invalid::at(
                        "smb.shadow_copy.snapshot_dir",
                        format!("share {name}: invalid snapshot directory {d:?}"),
                    ));
                }
                let p = &smb.shadow_copy.prefix;
                if !p.is_empty()
                    && !p
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
                {
                    return Err(Invalid::at(
                        "smb.shadow_copy.prefix",
                        format!("share {name}: invalid snapshot prefix {p:?}"),
                    ));
                }
            }
            let tm = smb.timemachine_max_size.trim().to_ascii_uppercase();
            if !tm.is_empty() {
                let digits = tm.trim_end_matches(['K', 'M', 'G', 'T', 'P']);
                if digits.is_empty()
                    || !digits.bytes().all(|b| b.is_ascii_digit())
                    || tm.len() - digits.len() > 1
                {
                    return Err(Invalid::at(
                        "smb.timemachine_max_size",
                        format!("share {name}: Time Machine size {tm:?} must look like 500G or 2T"),
                    ));
                }
            }
            smb.timemachine_max_size = tm;
            validate::smb_extra(&smb.extra).map_err(|e| {
                Invalid::at("smb.extra", format!("share {name}: SMB extra options: {e}"))
            })?;

            // NFS
            let nfs = &mut s.nfs;
            if nfs.enabled {
                if nfs.rules.is_empty() {
                    return Err(Invalid::at(
                        "nfs.rules",
                        format!("share {name}: NFS needs at least one client rule"),
                    ));
                }
                for r in &mut nfs.rules {
                    r.client = r.client.trim().to_string();
                    if !validate::nfs_client(&r.client) {
                        return Err(Invalid::at(
                            "nfs.rules",
                            format!("share {name}: invalid NFS client {:?}", r.client),
                        ));
                    }
                }
                nfs.fsid = nfs.fsid.trim().to_ascii_lowercase();
                if nfs.fsid.is_empty() {
                    nfs.fsid = info
                        .subvolume_uuid
                        .clone()
                        .unwrap_or_else(validate::new_uuid);
                }
                if !validate::uuid(&nfs.fsid) {
                    return Err(Invalid::at(
                        "nfs.fsid",
                        format!("share {name}: fsid {:?} is not a UUID", nfs.fsid),
                    ));
                }
            }
        }
        c.shares.sort_by_key(|s| s.name.to_ascii_lowercase());
        // folders deleted with their share: only for shares that are gone
        c.remove_folders
            .retain(|r| !c.shares.iter().any(|s| s.name.eq_ignore_ascii_case(&r.share)));
        c.remove_folders.sort_by(|a, b| a.share.cmp(&b.share));
        c.remove_folders.dedup_by(|a, b| a.share == b.share);
        for r in &c.remove_folders {
            if !share_name(&r.share) || share_path(&r.path).is_err() || r.path == "/" {
                return Err(Invalid::at(
                    "remove_folders",
                    format!("invalid folder removal {:?} {:?}", r.share, r.path),
                ));
            }
            for s in &r.snapshots {
                let prefix_ok = !s.prefix.is_empty()
                    && s.prefix
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
                if share_path(&s.dir).is_err() || s.dir == "/" || !prefix_ok {
                    return Err(Invalid::at(
                        "remove_folders",
                        format!("share {}: invalid snapshot directory {:?}", r.share, s.dir),
                    ));
                }
            }
        }
        let mut paths = BTreeSet::new();
        let mut fsids = BTreeSet::new();
        for s in c.shares.iter().filter(|s| s.nfs.enabled) {
            if !paths.insert(&s.path) {
                return Err(Invalid::at(
                    "path",
                    format!("{} is exported over NFS by two shares", s.path),
                ));
            }
            if !fsids.insert(&s.nfs.fsid) {
                return Err(Invalid::at(
                    "nfs.fsid",
                    format!("share {}: fsid {} is used twice", s.name, s.nfs.fsid),
                ));
            }
        }
        Ok(c)
    }

    /// Remarks about a share that do not block it, in English.
    pub fn warnings(&self, c: &SharesConfig, s: &Share) -> Vec<String> {
        self.warning_items(c, s).iter().map(warning_text).collect()
    }

    /// Remarks about a share that do not block it, with codes.
    pub fn warning_items(&self, c: &SharesConfig, s: &Share) -> Vec<ShareWarning> {
        let mut w = Vec::new();
        let info = self.host.probe(&s.path);
        let at_path = |code| ShareWarning {
            code,
            args: ShareWarningArgs {
                path: Some(s.path.clone()),
                ..Default::default()
            },
        };
        if let (Some(sv), false) = (&s.subvolume, info.exists) {
            w.push(ShareWarning {
                code: if sv.nocow {
                    ShareWarningCode::SubvolumeOnApplyNocow
                } else if sv.compression {
                    ShareWarningCode::SubvolumeOnApply
                } else {
                    ShareWarningCode::SubvolumeOnApplyNoCompression
                },
                args: ShareWarningArgs {
                    path: Some(s.path.clone()),
                    volume: Some(sv.volume.clone()),
                    key: None,
                },
            });
        }
        // an encrypted folder is a mount point; its ciphertext is the subvolume
        if s.encrypted {
            if matches!(self.crypt.state(&s.path), CryptState::Locked | CryptState::InUse) {
                w.push(at_path(ShareWarningCode::CryptLocked));
            }
        } else if info.exists && !info.subvolume {
            w.push(at_path(ShareWarningCode::NotSubvolume));
        }
        let managed: Vec<smb::Line> = smb::share_lines(
            &Share {
                smb: rynas_api_types::ShareSmb {
                    extra: String::new(),
                    ..s.smb.clone()
                },
                ..s.clone()
            },
            info.fs_type == "btrfs",
        );
        for k in smb::overridden_keys(&managed, &s.smb.extra) {
            w.push(ShareWarning {
                code: ShareWarningCode::ExtraOverrides,
                args: ShareWarningArgs {
                    key: Some(k),
                    ..Default::default()
                },
            });
        }
        if s.nfs.enabled {
            w.push(warn(ShareWarningCode::NfsUids));
            if !c.nfs.enabled {
                w.push(warn(ShareWarningCode::NfsServiceOff));
            }
        }
        if s.smb.enabled && !c.smb.enabled {
            w.push(warn(ShareWarningCode::SmbServiceOff));
        }
        if s.smb.preset == SmbPreset::Timemachine && s.smb.timemachine_max_size.is_empty() {
            w.push(warn(ShareWarningCode::TimemachineNoLimit));
        }
        if s.smb.preset == SmbPreset::Multiprotocol && !s.nfs.enabled {
            w.push(warn(ShareWarningCode::MultiprotocolWithoutNfs));
        }
        if s.permissions.is_empty() {
            w.push(warn(ShareWarningCode::NoPermissions));
        } else if s.permissions.iter().all(|p| p.access == Access::Deny) {
            w.push(warn(ShareWarningCode::AllDenied));
        }
        if s.show_snapshots {
            if snapview::wanted(s).is_none() {
                w.push(warn(ShareWarningCode::SnapshotFolderNoJob));
            } else if s.nfs.enabled && !s.nfs.crossmnt {
                w.push(warn(ShareWarningCode::SnapshotFolderNfs));
            }
        }
        w
    }

    /// Snapshot folders: stops the ones that go or change, then starts the wanted ones.
    fn reconcile_views(
        &self,
        from: &SharesConfig,
        to: &SharesConfig,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let (old, new) = (views(from), views(to));
        for (unit, (name, path, _)) in &old {
            if new.get(unit) == Some(&old[unit]) {
                continue;
            }
            (ctx.log)(&format!("snapshot folder of {name}: stopping {unit}"));
            if let Err(e) = ctx.system.stop_unit(unit) {
                (ctx.log)(&format!("{unit}: {e}"));
            }
            if !new.contains_key(unit) {
                self.host.remove_empty_dir(&snapview::mount_point(path));
            }
        }
        if old == new && new.is_empty() {
            return Ok(());
        }
        ctx.system.daemon_reload()?;
        if new.is_empty() {
            return Ok(());
        }
        ctx.system.set_unit_enabled(snapview::TARGET, true)?;
        for (unit, (name, _, dir)) in &new {
            // the job's directory may come with the same apply (snapshots, after shares)
            self.host.ensure_dir(dir)?;
            if old.get(unit) != new.get(unit) || !ctx.system.unit_active(unit)? {
                (ctx.log)(&format!("snapshot folder of {name}: starting {unit}"));
                ctx.system.start_unit(unit)?;
            }
        }
        Ok(())
    }

    /// Shares that went away: recorded with their settings, or deleted with their folder.
    fn reconcile_removed(
        &self,
        from: &SharesConfig,
        to: &SharesConfig,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let volumes = self.volumes();
        let now = rynas_tasks::now_ms();
        for s in &from.shares {
            if to.shares.iter().any(|x| x.name == s.name) {
                continue;
            }
            match to
                .remove_folders
                .iter()
                .find(|r| r.share == s.name && r.path == s.path)
            {
                Some(r) if to.shares.iter().any(|x| folders::is_within(&x.path, &r.path)) => {
                    (ctx.log)(&format!(
                        "{}: another share uses it, the folder stays",
                        r.path
                    ));
                }
                Some(r) => {
                    (ctx.log)(&format!(
                        "share {}: deleting {} with its snapshots",
                        s.name, r.path
                    ));
                    folders::remove_folder(
                        ctx.system,
                        self.host.as_ref(),
                        self.crypt.as_ref(),
                        &volumes,
                        &r.path,
                        &r.snapshots,
                        ctx.log,
                    )?;
                    self.removed.forget(&[&r.path])?;
                }
                None => {
                    if let Err(e) = self.removed.record(s, now) {
                        (ctx.log)(&format!("recording the settings of {}: {e}", s.name));
                    }
                }
            }
        }
        let paths: Vec<&str> = to.shares.iter().map(|s| s.path.as_str()).collect();
        if let Err(e) = self.removed.forget(&paths) {
            (ctx.log)(&format!("removed shares record: {e}"));
        }
        Ok(())
    }

    fn manage_unit(&self, ctx: &ApplyCtx, unit: &str, enabled: bool) -> Result<bool, String> {
        if !self.host.unit_installed(unit) {
            if enabled {
                (ctx.log)(&format!("{unit} is not installed: skipped"));
            }
            return Ok(false);
        }
        (ctx.log)(&format!(
            "{} {unit}",
            if enabled { "enabling" } else { "disabling" }
        ));
        ctx.system.set_unit_enabled(unit, enabled)?;
        Ok(true)
    }
}

impl Applier for SharesDomain {
    fn key(&self) -> &str {
        SHARES_KEY
    }

    fn order(&self) -> i32 {
        20
    }

    fn templates(&self) -> Vec<(String, String)> {
        vec![
            (smb::TEMPLATE_SMB.into(), smb::SMB_CONF_TEMPLATE.into()),
            (smb::TEMPLATE_AVAHI.into(), smb::AVAHI_TEMPLATE.into()),
            (nfs::TEMPLATE_EXPORTS.into(), nfs::EXPORTS_TEMPLATE.into()),
            (nfs::TEMPLATE_NFS_CONF.into(), nfs::NFS_CONF_TEMPLATE.into()),
        ]
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let c = self.normalize(parse(Some(&value))?)?;
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let c = parse(Some(value))?;
        let hostname = ctx.system.hostname().unwrap_or_default();
        let smb_shares: Vec<&Share> = c.shares.iter().filter(|s| s.smb.enabled).collect();
        let conf = smb::SmbConfCtx {
            global: smb::global_lines(&c.smb, &hostname),
            shares: smb_shares
                .iter()
                .map(|s| smb::SmbShareCtx {
                    name: s.name.clone(),
                    lines: smb::share_lines(s, self.host.probe(&s.path).fs_type == "btrfs"),
                })
                .chain(c.smb.homes.enabled.then(|| smb::SmbShareCtx {
                    name: "homes".into(),
                    lines: smb::homes_lines(&c.smb.homes),
                }))
                .collect(),
        };
        let mut files = vec![ManagedFile::whole(
            smb::SMB_CONF,
            ctx.render(smb::TEMPLATE_SMB, &to_value(&conf))?,
            true,
        )];
        if c.smb.enabled && c.smb.mdns && !smb_shares.is_empty() {
            let avahi = smb::AvahiCtx {
                model: smb::DEVICE_MODEL.into(),
                timemachine: smb_shares
                    .iter()
                    .filter(|s| s.smb.preset == SmbPreset::Timemachine)
                    .map(|s| s.name.clone())
                    .collect(),
            };
            // XML: no '#' header, the stored hash alone detects drift
            files.push(ManagedFile::whole(
                smb::AVAHI_SERVICE,
                ctx.render(smb::TEMPLATE_AVAHI, &to_value(&avahi))?,
                false,
            ));
        } else {
            files.push(ManagedFile::absent(smb::AVAHI_SERVICE));
        }
        let exports: Vec<String> = if c.nfs.enabled {
            c.shares.iter().filter_map(nfs::export_line).collect()
        } else {
            Vec::new()
        };
        files.push(ManagedFile::whole(
            nfs::EXPORTS,
            ctx.render(
                nfs::TEMPLATE_EXPORTS,
                &serde_json::json!({ "exports": exports }),
            )?,
            true,
        ));
        files.push(ManagedFile::whole(
            nfs::NFS_CONF,
            ctx.render(nfs::TEMPLATE_NFS_CONF, &to_value(&nfs::conf_ctx(&c.nfs)))?,
            true,
        ));
        // snapshot folders: one mount unit each, the target, and our units that went away
        let wanted = views(&c);
        for (unit, (name, path, dir)) in &wanted {
            files.push(ManagedFile::whole(
                format!("{}/{unit}", snapview::UNIT_DIR),
                snapview::unit(name, path, dir),
                true,
            ));
        }
        let units: Vec<String> = wanted.keys().cloned().collect();
        files.push(ManagedFile::whole(
            format!("{}/{}", snapview::UNIT_DIR, snapview::TARGET),
            snapview::target(&units),
            true,
        ));
        if let Ok(rd) = std::fs::read_dir(ctx.path(Path::new(snapview::UNIT_DIR))) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".mount") || wanted.contains_key(&name) {
                    continue;
                }
                let ours = std::fs::read_to_string(e.path()).is_ok_and(|t| {
                    t.starts_with(HEADER_PREFIX) && t.lines().any(|l| l == snapview::MARKER)
                });
                if ours {
                    files.push(ManagedFile::absent(format!(
                        "{}/{name}",
                        snapview::UNIT_DIR
                    )));
                }
            }
        }
        Ok(files)
    }

    fn check_files(&self, files: &[Candidate], ctx: &ApplyCtx) -> Result<(), String> {
        for c in files
            .iter()
            .filter(|c| c.file.path.as_os_str() == smb::SMB_CONF)
        {
            let staged = c.staged.to_string_lossy().into_owned();
            (ctx.log)(&format!("testparm -s {}", c.file.path.display()));
            match ctx.system.run(&Cmd::new("testparm").args(["-s", &staged])) {
                Ok(o) => {
                    let p = smb::testparm_problems(&o.stderr);
                    if !p.is_empty() {
                        return Err(format!("testparm: {}", p.join("; ")));
                    }
                }
                // Samba not installed: nothing can read the file yet
                Err(e) if e.contains("cannot start") => {
                    (ctx.log)("testparm is not installed (samba missing): check skipped")
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn reconcile(
        &self,
        from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let Some(to) = to else {
            (ctx.log)("shares: no longer managed, services left as they are");
            return Ok(());
        };
        let from = parse(from)?;
        let to = parse(Some(to))?;
        // folders get shared, unshared, created or deleted: their sizes are measured again
        self.sizes.clear();

        // 0. subvolumes asked for by new shares (an empty one left by a failed apply is
        // harmless and reused by the next one)
        for s in &to.shares {
            let Some(sv) = &s.subvolume else { continue };
            if self.host.probe(&s.path).exists {
                continue;
            }
            let c = self
                .subvolumes
                .as_ref()
                .ok_or_else(|| format!("share {}: cannot create its subvolume", s.name))?;
            (ctx.log)(&format!(
                "creating subvolume {} on volume {}",
                s.path, sv.volume
            ));
            c.create(&s.path, sv.compression, sv.nocow, ctx.log)?;
        }

        // 1. ACLs first, so a share never appears with the wrong permissions
        for s in to.shares.iter().filter(|s| !s.permissions.is_empty()) {
            if self.crypt_closed(s) {
                (ctx.log)(&format!(
                    "ACL of {}: the encrypted folder is locked, set at its next unlock",
                    s.path
                ));
                continue;
            }
            let plan = self.acl_for(s)?;
            (ctx.log)(&format!("ACL of {}: {}", s.path, plan.spec()));
            self.host.set_acl(&s.path, &plan)?;
        }

        // 1b. snapshot folders, before the services that show them
        self.reconcile_views(&from, &to, ctx)?;

        // 2. SMB
        if smb_running(&to) {
            if self.manage_unit(ctx, SMBD, true)? {
                if smb_needs_restart(&from.smb, &to.smb) {
                    (ctx.log)("restarting smbd (listening setup changed)");
                    ctx.system.restart_unit(SMBD)?;
                } else if self.smbd_announces_itself(ctx) {
                    // a restart cuts every open copy and Time Machine backup: only when no
                    // client is connected, else deferred to a later apply (review
                    // 2026-11-07, M7)
                    match self.host.smb_sessions() {
                        Some(n) if n > 0 => {
                            (ctx.log)(&format!(
                                "smbd restart deferred: {n} clients connected (it still announces itself through Avahi until a later apply restarts it)"
                            ));
                            ctx.system.reload_or_restart_unit(SMBD)?;
                        }
                        _ => {
                            (ctx.log)(
                                "restarting smbd once: it was started before rynas turned off its own mDNS announcement",
                            );
                            ctx.system.restart_unit(SMBD)?;
                        }
                    }
                } else {
                    ctx.system.reload_or_restart_unit(SMBD)?;
                }
            }
            if to.smb.netbios || from.smb.netbios {
                self.manage_unit(ctx, NMBD, to.smb.netbios)?;
            }
            self.manage_unit(ctx, WSDD2, to.smb.wsdd)?;
        } else if smb_running(&from) {
            self.manage_unit(ctx, SMBD, false)?;
            if from.smb.netbios {
                self.manage_unit(ctx, NMBD, false)?;
            }
            self.manage_unit(ctx, WSDD2, false)?;
        }

        // 3. NFS
        if self.host.unit_installed(NFS_SERVER) {
            if nfs_running(&to) {
                self.manage_unit(ctx, NFS_SERVER, true)?;
                if from.nfs != to.nfs && nfs_running(&from) {
                    (ctx.log)("restarting nfs-server (settings changed)");
                    ctx.system.restart_unit(NFS_SERVER)?;
                }
            }
            if nfs_running(&to) || nfs_running(&from) {
                (ctx.log)("exportfs -ra");
                let o = ctx.system.run(&Cmd::new("exportfs").arg("-ra"))?;
                for l in o.stderr.lines().filter(|l| !l.trim().is_empty()) {
                    (ctx.log)(&format!("exportfs: {l}"));
                }
            }
            if !nfs_running(&to) && nfs_running(&from) {
                self.manage_unit(ctx, NFS_SERVER, false)?;
            }
        } else if nfs_running(&to) {
            (ctx.log)(&format!("{NFS_SERVER} is not installed: skipped"));
        }

        // 4. shares that went away, once no service uses their folder any more
        self.reconcile_removed(&from, &to, ctx)
    }


    fn health(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        let c = parse(value)?;
        if smb_running(&c) && self.host.unit_installed(SMBD) && !ctx.system.unit_active(SMBD)? {
            return Err(format!("{SMBD} is not active after the reload"));
        }
        if nfs_running(&c)
            && self.host.unit_installed(NFS_SERVER)
            && !ctx.system.unit_active(NFS_SERVER)?
        {
            // the kernel server may be unavailable (containers): not a reason to roll back SMB
            (ctx.log)(&format!("warning: {NFS_SERVER} is not active"));
        }
        Ok(())
    }

    fn state_drift(
        &self,
        applied: Option<&Value>,
        _ctx: &ApplyCtx,
    ) -> Result<Vec<StateDrift>, String> {
        let Some(applied) = applied else {
            return Ok(Vec::new());
        };
        let c = parse(Some(applied))?;
        let mut out = Vec::new();
        for s in &c.shares {
            let object = format!("share:{}", s.name);
            if !self.host.probe(&s.path).exists {
                out.push(crate::drift(
                    SHARES_KEY,
                    object,
                    StateDriftKind::Missing,
                    StateDriftCode::SharePathMissing,
                    StateDriftArgs {
                        path: Some(s.path.clone()),
                        ..Default::default()
                    },
                ));
                continue;
            }
            if s.permissions.is_empty() || self.crypt_closed(s) {
                continue;
            }
            let want = self.acl_for(s)?.canonical();
            if let Some(have) = self.host.get_acl(&s.path)?
                && have != want
            {
                let extra = have.difference(&want).cloned().collect();
                let missing = want.difference(&have).cloned().collect();
                out.push(crate::drift(
                    SHARES_KEY,
                    object,
                    StateDriftKind::Mismatch,
                    StateDriftCode::ShareAclDiffers,
                    StateDriftArgs {
                        path: Some(s.path.clone()),
                        missing,
                        extra,
                        ..Default::default()
                    },
                ));
            }
        }
        Ok(out)
    }
}

/// Removes a principal from every permission matrix (used when a user or group is deleted).
pub fn drop_principal(c: &mut SharesConfig, kind: PrincipalKind, name: &str) -> bool {
    let mut changed = false;
    for s in &mut c.shares {
        let before = s.permissions.len();
        s.permissions
            .retain(|p| !(p.kind == kind && p.name == name));
        changed |= s.permissions.len() != before;
    }
    changed
}

fn to_value<T: serde::Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}
