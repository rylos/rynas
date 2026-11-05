//! The share × principal matrix: effective permission with provenance, POSIX ACLs and the
//! smb.conf access lists.
//!
//! Rule (DSM): `deny` beats everything, then `rw`, then `ro`; a principal with no entry has
//! no access. POSIX ACLs check a named-user entry before the group entries, and the group
//! entries grant the union of their permissions, so the rendering is:
//! - every group with `rw`/`ro`/`deny` gets `g:<group>:rwx|r-x|---`;
//! - every user with an explicit entry gets `u:<user>:<effective permission>` (so an
//!   explicit RO does not hide the RW of one of the user's groups);
//! - every member of a denied group gets `u:<member>:---` (a group entry cannot take away
//!   what another group grants). Membership is read at apply time; the SMB lists
//!   (`invalid users = @group`) follow membership changes at once, the ACL on the next
//!   apply (the drift check reports the difference meanwhile).

use std::collections::{BTreeMap, BTreeSet};

use rynas_api_types::{
    Access, EffectiveAccess, EffectivePermission, Permission, PermissionSource, PrincipalKind,
};

/// Who the effective permission is computed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal<'a> {
    User(&'a str),
    Group(&'a str),
}

impl<'a> Principal<'a> {
    /// `alice` or `@staff`.
    pub fn parse(s: &'a str) -> Principal<'a> {
        match s.strip_prefix('@') {
            Some(g) => Principal::Group(g),
            None => Principal::User(s),
        }
    }
}

fn label(a: Access) -> &'static str {
    match a {
        Access::Ro => "RO",
        Access::Rw => "RW",
        Access::Deny => "denied",
    }
}

fn kind_label(k: PrincipalKind) -> &'static str {
    match k {
        PrincipalKind::User => "user",
        PrincipalKind::Group => "group",
    }
}

/// Effective access of `who` to a share. `groups` are the user's groups (ignored for a
/// group principal).
pub fn effective(
    share: &str,
    perms: &[Permission],
    who: Principal,
    groups: &[String],
) -> EffectivePermission {
    let applies = |p: &Permission| match (who, p.kind) {
        (Principal::User(u), PrincipalKind::User) => p.name == u,
        (Principal::User(_), PrincipalKind::Group) => groups.contains(&p.name),
        (Principal::Group(g), PrincipalKind::Group) => p.name == g,
        (Principal::Group(_), PrincipalKind::User) => false,
    };
    let matching: Vec<&Permission> = perms.iter().filter(|p| applies(p)).collect();
    let best = matching.iter().map(|p| p.access).max();
    let mut sources: Vec<PermissionSource> = matching
        .iter()
        .map(|p| PermissionSource {
            kind: p.kind,
            name: p.name.clone(),
            access: p.access,
            decisive: Some(p.access) == best,
        })
        .collect();
    // decisive first, then by strength, users before groups, then by name
    sources.sort_by(|a, b| {
        (!a.decisive, std::cmp::Reverse(a.access), a.kind, &a.name).cmp(&(
            !b.decisive,
            std::cmp::Reverse(b.access),
            b.kind,
            &b.name,
        ))
    });
    let access = match best {
        None => EffectiveAccess::None,
        Some(Access::Ro) => EffectiveAccess::Ro,
        Some(Access::Rw) => EffectiveAccess::Rw,
        Some(Access::Deny) => EffectiveAccess::Deny,
    };
    let principal = match who {
        Principal::User(u) => u.to_string(),
        Principal::Group(g) => format!("@{g}"),
    };
    let explanation = match best {
        None => "no entry for this principal or its groups: no access".to_string(),
        Some(b) => {
            let describe = |s: &PermissionSource| format!("{} {}", kind_label(s.kind), s.name);
            let decisive: Vec<String> = sources
                .iter()
                .filter(|s| s.decisive)
                .map(describe)
                .collect();
            let head = match b {
                Access::Deny => format!("denied by {}", decisive.join(" and ")),
                a => format!("{} from {}", label(a), decisive.join(" and ")),
            };
            let others: Vec<String> = sources
                .iter()
                .filter(|s| !s.decisive)
                .map(|s| format!("{} from {}", label(s.access), describe(s)))
                .collect();
            if others.is_empty() {
                head
            } else {
                format!("{head} (also {})", others.join(", "))
            }
        }
    };
    EffectivePermission {
        share: share.into(),
        principal,
        access,
        sources,
        explanation,
    }
}

fn bits(a: EffectiveAccess) -> &'static str {
    match a {
        EffectiveAccess::Rw => "rwx",
        EffectiveAccess::Ro => "r-x",
        EffectiveAccess::None | EffectiveAccess::Deny => "---",
    }
}

/// POSIX ACL of a share root: the same entries as access ACL and default ACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclPlan {
    /// `(tag, qualifier, perms)` in `setfacl` short form, base entries first.
    pub entries: Vec<(char, String, String)>,
}

impl AclPlan {
    /// `u::rwx,g::---,o::---,m::rwx,u:alice:rwx,g:staff:r-x` for `setfacl --set`.
    pub fn spec(&self) -> String {
        self.entries
            .iter()
            .map(|(t, q, p)| format!("{t}:{q}:{p}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The lines `getfacl -c -p -E` prints for this ACL (access and default), sorted: the
    /// form [`parse_getfacl`] returns, to compare with the system.
    pub fn canonical(&self) -> BTreeSet<String> {
        let long = |t: char| match t {
            'u' => "user",
            'g' => "group",
            'm' => "mask",
            _ => "other",
        };
        self.entries
            .iter()
            .flat_map(|(t, q, p)| {
                let l = format!("{}:{q}:{p}", long(*t));
                [format!("default:{l}"), l]
            })
            .collect()
    }
}

/// Lines of `getfacl -c -p -E` (comments and blanks dropped).
pub fn parse_getfacl(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// The ACL for a matrix. `groups_of(user)` gives the user's groups, `members_of(group)` the
/// members of a group (both from the system at apply time). The owner keeps `rwx`, the
/// owning group and others get nothing.
pub fn acl_plan(
    perms: &[Permission],
    groups_of: &dyn Fn(&str) -> Vec<String>,
    members_of: &dyn Fn(&str) -> Vec<String>,
) -> AclPlan {
    let mut users: BTreeMap<String, EffectiveAccess> = BTreeMap::new();
    let mut groups: BTreeMap<String, Access> = BTreeMap::new();
    let mut user_effective = |u: &str| {
        let e = effective("", perms, Principal::User(u), &groups_of(u)).access;
        users.insert(u.to_string(), e);
    };
    for p in perms {
        match p.kind {
            PrincipalKind::User => user_effective(&p.name),
            PrincipalKind::Group => {
                groups.insert(p.name.clone(), p.access);
                if p.access == Access::Deny {
                    for m in members_of(&p.name) {
                        user_effective(&m);
                    }
                }
            }
        }
    }
    let mut entries = vec![
        ('u', String::new(), "rwx".to_string()),
        ('g', String::new(), "---".to_string()),
        ('o', String::new(), "---".to_string()),
        ('m', String::new(), "rwx".to_string()),
    ];
    for (u, a) in users {
        entries.push(('u', u, bits(a).into()));
    }
    for (g, a) in groups {
        let e = match a {
            Access::Ro => EffectiveAccess::Ro,
            Access::Rw => EffectiveAccess::Rw,
            Access::Deny => EffectiveAccess::Deny,
        };
        entries.push(('g', g, bits(e).into()));
    }
    AclPlan { entries }
}

/// smb.conf access lists of a share.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SmbLists {
    pub valid: Vec<String>,
    pub write: Vec<String>,
    pub read: Vec<String>,
    pub invalid: Vec<String>,
}

/// `valid users` (everyone with RW or RO), `write list` (RW), `read list` (RO),
/// `invalid users` (deny; Samba refuses them whatever the other lists say).
pub fn smb_lists(perms: &[Permission]) -> SmbLists {
    let mut l = SmbLists::default();
    let mut sorted: Vec<&Permission> = perms.iter().collect();
    sorted.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    for p in sorted {
        let n = match p.kind {
            PrincipalKind::User => p.name.clone(),
            PrincipalKind::Group => format!("@{}", p.name),
        };
        match p.access {
            Access::Rw => {
                l.valid.push(n.clone());
                l.write.push(n);
            }
            Access::Ro => {
                l.valid.push(n.clone());
                l.read.push(n);
            }
            Access::Deny => l.invalid.push(n),
        }
    }
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(kind: PrincipalKind, name: &str, access: Access) -> Permission {
        Permission {
            kind,
            name: name.into(),
            access,
        }
    }
    use Access::*;
    use PrincipalKind::{Group as G, User as U};

    fn matrix() -> Vec<Permission> {
        vec![
            p(G, "everyone", Ro),
            p(G, "staff", Rw),
            p(U, "alice", Ro),
            p(U, "mallory", Deny),
            p(G, "interns", Deny),
        ]
    }

    fn groups_of(u: &str) -> Vec<String> {
        match u {
            "alice" => vec!["everyone".into(), "staff".into()],
            "bob" => vec!["everyone".into()],
            "carol" => vec!["everyone".into(), "staff".into(), "interns".into()],
            "mallory" => vec!["staff".into()],
            _ => vec![],
        }
        .into_iter()
        .collect()
    }

    fn members_of(g: &str) -> Vec<String> {
        ["alice", "bob", "carol", "mallory"]
            .into_iter()
            .filter(|u| groups_of(u).iter().any(|x| x == g))
            .map(String::from)
            .collect()
    }

    #[test]
    fn effective_permission_with_provenance() {
        let m = matrix();
        let e = effective("docs", &m, Principal::User("alice"), &groups_of("alice"));
        assert_eq!(e.access, EffectiveAccess::Rw);
        assert_eq!(
            e.explanation,
            "RW from group staff (also RO from user alice, RO from group everyone)"
        );
        assert!(e.sources[0].decisive && e.sources[0].name == "staff");

        let e = effective("docs", &m, Principal::User("bob"), &groups_of("bob"));
        assert_eq!(e.access, EffectiveAccess::Ro);
        assert_eq!(e.explanation, "RO from group everyone");

        let e = effective("docs", &m, Principal::User("carol"), &groups_of("carol"));
        assert_eq!(e.access, EffectiveAccess::Deny);
        assert!(
            e.explanation
                .starts_with("denied by group interns (also RW from group staff")
        );

        let e = effective(
            "docs",
            &m,
            Principal::User("mallory"),
            &groups_of("mallory"),
        );
        assert_eq!(e.access, EffectiveAccess::Deny);
        assert_eq!(
            e.explanation,
            "denied by user mallory (also RW from group staff)"
        );

        let e = effective("docs", &m, Principal::User("dave"), &[]);
        assert_eq!(e.access, EffectiveAccess::None);
        assert!(e.sources.is_empty());

        let e = effective("docs", &m, Principal::parse("@staff"), &[]);
        assert_eq!(
            (e.access, e.principal.as_str()),
            (EffectiveAccess::Rw, "@staff")
        );
    }

    #[test]
    fn acl_follows_deny_over_rw_over_ro() {
        let plan = acl_plan(&matrix(), &groups_of, &members_of);
        assert_eq!(
            plan.spec(),
            "u::rwx,g::---,o::---,m::rwx,\
             u:alice:rwx,u:carol:---,u:mallory:---,\
             g:everyone:r-x,g:interns:---,g:staff:rwx"
        );
        let c = plan.canonical();
        assert!(c.contains("user:alice:rwx") && c.contains("default:user:alice:rwx"));
        assert!(c.contains("default:group:staff:rwx") && c.contains("mask::rwx"));
        assert_eq!(c.len(), plan.entries.len() * 2);
        let from_getfacl = parse_getfacl(
            "user::rwx\nuser:alice:rwx\nuser:carol:---\nuser:mallory:---\ngroup::---\n\
             group:everyone:r-x\ngroup:interns:---\ngroup:staff:rwx\nmask::rwx\nother::---\n\n\
             default:user::rwx\ndefault:user:alice:rwx\ndefault:user:carol:---\n\
             default:user:mallory:---\ndefault:group::---\ndefault:group:everyone:r-x\n\
             default:group:interns:---\ndefault:group:staff:rwx\ndefault:mask::rwx\n\
             default:other::---\n",
        );
        assert_eq!(from_getfacl, c);
    }

    #[test]
    fn smb_lists_of_the_matrix() {
        let l = smb_lists(&matrix());
        assert_eq!(l.valid, ["alice", "@everyone", "@staff"]);
        assert_eq!(l.write, ["@staff"]);
        assert_eq!(l.read, ["alice", "@everyone"]);
        assert_eq!(l.invalid, ["mallory", "@interns"]);
    }
}
