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

use std::collections::BTreeSet;
use std::sync::Arc;

use rynas_api_types::{
    Access, PrincipalKind, Share, SharesConfig, SmbPreset, SmbSettings, StateDrift, StateDriftKind,
};
use rynas_config::{Applier, ApplyCtx, Candidate, Invalid, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::accounts::Accounts;
use crate::host::{ShareHost, SubvolumeCreator};
use crate::perm::{AclPlan, acl_plan};
use crate::validate::{self, account_name, one_line, share_name, share_path};
use crate::{nfs, smb};

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
}

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
    pub fn new(host: Arc<dyn ShareHost>, accounts: Arc<dyn Accounts>) -> Self {
        Self {
            host,
            accounts,
            subvolumes: None,
        }
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
            if info.remote {
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

    /// Remarks about a share that do not block it.
    pub fn warnings(&self, c: &SharesConfig, s: &Share) -> Vec<String> {
        let mut w = Vec::new();
        let info = self.host.probe(&s.path);
        if let (Some(sv), false) = (&s.subvolume, info.exists) {
            w.push(format!(
                "{} will be created as a subvolume of volume {}{} when the change is applied",
                s.path,
                sv.volume,
                if sv.compression {
                    ""
                } else {
                    ", without compression"
                }
            ));
        }
        if info.exists && !info.subvolume {
            w.push(format!(
                "{} is not a btrfs subvolume: snapshots, previous versions and quotas need one",
                s.path
            ));
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
            w.push(format!(
                "the SMB extra option '{k}' overrides a setting of rynas"
            ));
        }
        if s.nfs.enabled {
            w.push(
                "NFS (sec=sys) trusts client uids: clients must use the same uids as this NAS"
                    .into(),
            );
            if !c.nfs.enabled {
                w.push("NFS is disabled in the NFS service settings".into());
            }
        }
        if s.smb.enabled && !c.smb.enabled {
            w.push("SMB is disabled in the SMB service settings".into());
        }
        if s.smb.preset == SmbPreset::Timemachine && s.smb.timemachine_max_size.is_empty() {
            w.push("Time Machine without a size limit can fill the volume".into());
        }
        if s.smb.preset == SmbPreset::Multiprotocol && !s.nfs.enabled {
            w.push("the multi-protocol preset turns off SMB client caching; it is meant for shares also exported over NFS".into());
        }
        if s.permissions.is_empty() {
            w.push(
                "no permissions: the directory's own permissions apply (rynas does not set ACLs)"
                    .into(),
            );
        } else if s.permissions.iter().all(|p| p.access == Access::Deny) {
            w.push("every entry denies access: nobody can use this share".into());
        }
        w
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
                .collect(),
        };
        let mut files = vec![ManagedFile::whole(
            smb::SMB_CONF,
            ctx.render(smb::TEMPLATE_SMB, &to_value(&conf))?,
            true,
        )];
        if c.smb.enabled && c.smb.mdns && !smb_shares.is_empty() {
            let avahi = smb::AvahiCtx {
                model: "RackMac".into(),
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
            c.create(&s.path, sv.compression, ctx.log)?;
        }

        // 1. ACLs first, so a share never appears with the wrong permissions
        for s in to.shares.iter().filter(|s| !s.permissions.is_empty()) {
            let plan = self.acl_for(s)?;
            (ctx.log)(&format!("ACL of {}: {}", s.path, plan.spec()));
            self.host.set_acl(&s.path, &plan)?;
        }

        // 2. SMB
        if smb_running(&to) {
            if self.manage_unit(ctx, SMBD, true)? {
                if smb_needs_restart(&from.smb, &to.smb) {
                    (ctx.log)("restarting smbd (listening setup changed)");
                    ctx.system.restart_unit(SMBD)?;
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
        Ok(())
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
                out.push(StateDrift {
                    key: SHARES_KEY.into(),
                    object,
                    kind: StateDriftKind::Missing,
                    detail: format!("{} does not exist any more", s.path),
                });
                continue;
            }
            if s.permissions.is_empty() {
                continue;
            }
            let want = self.acl_for(s)?.canonical();
            if let Some(have) = self.host.get_acl(&s.path)?
                && have != want
            {
                let extra: Vec<&String> = have.difference(&want).collect();
                let missing: Vec<&String> = want.difference(&have).collect();
                out.push(StateDrift {
                    key: SHARES_KEY.into(),
                    object,
                    kind: StateDriftKind::Mismatch,
                    detail: format!(
                        "ACL of {} differs (group membership changed or edited by hand): missing {:?}, unexpected {:?}",
                        s.path, missing, extra
                    ),
                });
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
