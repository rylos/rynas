//! PC backup, receive side (D-PCBACKUP phase 2): the folder of a device where its btrbk pushes
//! snapshots, read and summarised; the NAS-side delete rules; the texts rynas writes for the
//! dedicated account (`authorized_keys` lines, sudoers) and shows for the PC (`btrbk.conf`).
//!
//! The filter that enforces all this on every SSH command is [`crate::receive_filter`].

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use base64::Engine;
use rynas_api_types::pcbackup::{
    ACCOUNT, DeletePolicy, FILTER, ReceivedGroup, ReceivedSnapshot,
};
use sha2::{Digest, Sha256};

use crate::ioctl::{self, SubvolInfo};
use crate::name::{compare_names, parse_timestamp, split_snapshot_name};

const DAY_MS: u64 = 86_400_000;

/// One entry of a device folder.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    pub name: String,
    /// `None`: not a subvolume (or its facts could not be read).
    pub info: Option<SubvolInfo>,
}

impl Entry {
    /// Writable and without received UUID: a receive that did not finish.
    pub fn incomplete(&self) -> bool {
        self.info
            .as_ref()
            .is_some_and(|i| !i.readonly && i.received_uuid.is_none())
    }
}

/// The directories of `dir` with btrbk snapshot names, with their subvolume facts, and the
/// number of other entries. Blocking; reads with ioctls on descriptors (no symlink followed).
pub fn scan_dir(dir: &Path) -> io::Result<(Vec<Entry>, u32)> {
    let parent = ioctl::open_dir(dir)?;
    let mut out = Vec::new();
    let mut other = 0;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let Ok(name) = e.file_name().into_string() else {
            other += 1;
            continue;
        };
        if !e.file_type().is_ok_and(|t| t.is_dir()) || split_snapshot_name(&name).is_none() {
            other += 1;
            continue;
        }
        let info = ioctl::open_dir_at(&parent, &name)
            .ok()
            .filter(ioctl::is_subvolume)
            .and_then(|f| ioctl::subvol_info(&f).ok());
        out.push(Entry { name, info });
    }
    out.sort_by(|a, b| compare_names(&b.name, &a.name));
    Ok((out, other))
}

/// The time written in a snapshot name, in ms (the PC's clock).
pub fn name_ms(name: &str) -> Option<u64> {
    let (_, ts) = split_snapshot_name(name)?;
    let ms = parse_timestamp(ts)?.epoch_ms();
    (ms >= 0).then_some(ms as u64)
}

/// When the NAS made the snapshot (its `otime`), in ms.
pub fn otime_ms(e: &Entry) -> Option<u64> {
    e.info
        .as_ref()
        .and_then(|i| (i.otime > 0).then_some(i.otime as u64 * 1000))
}

/// The name says a time more than a day after `now_ms` (the PC chose it: not trusted).
pub fn future_dated(name: &str, now_ms: u64) -> bool {
    name_ms(name).is_some_and(|ms| ms > now_ms.saturating_add(DAY_MS))
}

/// Why the NAS refuses to delete `name` now (`None`: allowed). `all` is the folder's content.
///
/// Ages and order come from the NAS side (the subvolume's `otime`, the name only when that
/// cannot be read): the names are chosen by the PC, so a PC that sends snapshots named in the
/// future cannot push its real snapshots out of the newest N. Snapshots whose name is more
/// than a day in the future never count among the newest N.
pub fn delete_refusal(
    name: &str,
    all: &[Entry],
    policy: &DeletePolicy,
    now_ms: u64,
) -> Option<String> {
    let Some((group, _)) = split_snapshot_name(name) else {
        return Some(format!("{name}: not a btrbk snapshot name"));
    };
    let entry = all.iter().find(|e| e.name == name);
    // an interrupted receive goes: btrbk deletes it before it sends that snapshot again
    if entry.is_some_and(Entry::incomplete) {
        return None;
    }
    if !policy.allowed {
        return Some(format!("{name}: deleting is turned off on the NAS"));
    }
    if policy.min_age_days > 0
        && let Some(ms) = entry.and_then(otime_ms).or_else(|| name_ms(name))
        && now_ms < ms.saturating_add(u64::from(policy.min_age_days) * DAY_MS)
    {
        return Some(format!(
            "{name}: kept by the NAS for {} days",
            policy.min_age_days
        ));
    }
    if policy.keep_last > 0 {
        let mut newest: Vec<&Entry> = all
            .iter()
            .filter(|e| e.info.is_some() && !e.incomplete() && !future_dated(&e.name, now_ms))
            .filter(|e| split_snapshot_name(&e.name).is_some_and(|(g, _)| g == group))
            .collect();
        newest.sort_by(|a, b| {
            otime_ms(b)
                .cmp(&otime_ms(a))
                .then_with(|| compare_names(&b.name, &a.name))
        });
        if newest
            .iter()
            .take(policy.keep_last as usize)
            .any(|e| e.name == name)
        {
            return Some(format!(
                "{name}: one of the newest {} of {group}, kept by the NAS",
                policy.keep_last
            ));
        }
    }
    None
}

/// What a folder holds, for the API.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    pub list: Vec<ReceivedSnapshot>,
    pub groups: Vec<ReceivedGroup>,
    pub incomplete: u32,
    /// Newest NAS-side creation time of a complete received snapshot.
    pub last_received_ms: Option<u64>,
}

/// The newest complete snapshot of each subvolume, by the NAS-side `otime` (the name breaks
/// ties): the parent btrbk on the PC uses for its next incremental send. Names more than a
/// day in the future only count when a subvolume has nothing else.
pub fn parents(entries: &[Entry], now_ms: u64) -> BTreeSet<String> {
    let mut best: BTreeMap<&str, &Entry> = BTreeMap::new();
    let rank = |e: &Entry| (!future_dated(&e.name, now_ms), otime_ms(e));
    for e in entries.iter().filter(|e| e.info.is_some() && !e.incomplete()) {
        let Some((group, _)) = split_snapshot_name(&e.name) else { continue };
        let better = |a: &Entry, b: &Entry| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| compare_names(&a.name, &b.name))
                .is_gt()
        };
        match best.get(group) {
            Some(b) if !better(e, b) => {}
            _ => {
                best.insert(group, e);
            }
        }
    }
    best.values().map(|e| e.name.clone()).collect()
}

pub fn summarize(entries: &[Entry], policy: &DeletePolicy, now_ms: u64) -> Summary {
    let parents = parents(entries, now_ms);
    let mut s = Summary::default();
    let mut groups: BTreeMap<String, ReceivedGroup> = BTreeMap::new();
    for e in entries {
        let group = split_snapshot_name(&e.name)
            .map(|(g, _)| g.to_string())
            .unwrap_or_default();
        let created_ms = name_ms(&e.name);
        let received_at_ms = e
            .info
            .as_ref()
            .and_then(|i| (i.otime > 0).then_some(i.otime as u64 * 1000));
        let incomplete = e.incomplete();
        if incomplete {
            s.incomplete += 1;
        } else if e.info.is_some() {
            s.last_received_ms = s.last_received_ms.max(received_at_ms.or(created_ms));
            let g = groups.entry(group.clone()).or_insert_with(|| ReceivedGroup {
                name: group.clone(),
                ..Default::default()
            });
            g.snapshots += 1;
            g.newest_ms = g.newest_ms.max(created_ms);
        }
        s.list.push(ReceivedSnapshot {
            name: e.name.clone(),
            group,
            created_ms,
            received_at_ms,
            readonly: e.info.as_ref().is_some_and(|i| i.readonly),
            received_uuid: e.info.as_ref().and_then(|i| i.received_uuid.clone()),
            incomplete,
            protected: e.info.is_some()
                && !incomplete
                && delete_refusal(&e.name, entries, policy, now_ms).is_some(),
            not_subvolume: e.info.is_none(),
            parent: parents.contains(&e.name),
            size: None,
        });
    }
    s.groups = groups.into_values().collect();
    s
}

// ------------------------------------------------------------------ keys and texts

/// Key types sshd accepts for these keys.
const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "sk-ssh-ed25519@openssh.com",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ecdsa-sha2-nistp256@openssh.com",
    "ssh-rsa",
];

/// A pasted public key: (type, base64 blob). Options, other key types, a blob that does not
/// decode or does not name its own type are refused; the comment is dropped.
pub fn parse_public_key(text: &str) -> Result<(String, String), String> {
    let line = text.trim();
    if line.contains(['\n', '\r', '"', '\0']) {
        return Err("paste one public key line (the .pub file)".into());
    }
    let mut parts = line.split_whitespace();
    let (Some(kind), Some(blob)) = (parts.next(), parts.next()) else {
        return Err("paste the whole line: type, key and comment (the .pub file)".into());
    };
    if !KEY_TYPES.contains(&kind) {
        return Err(format!(
            "not a public key of a supported type ({})",
            KEY_TYPES[..3].join(", ")
        ));
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(blob)
        .map_err(|_| "the key part is not valid base64".to_string())?;
    let named = raw
        .get(..4)
        .map(|l| u32::from_be_bytes([l[0], l[1], l[2], l[3]]) as usize)
        .and_then(|n| raw.get(4..4 + n));
    if named != Some(kind.as_bytes()) {
        return Err("the key part does not match its type".into());
    }
    if kind == "ssh-rsa" && raw.len() < 270 {
        return Err("RSA keys shorter than 2048 bits are refused".into());
    }
    Ok((kind.to_string(), blob.to_string()))
}

/// `SHA256:…` as `ssh-keygen -l` prints it.
pub fn fingerprint(blob_b64: &str) -> Option<String> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(blob_b64)
        .ok()?;
    let d = Sha256::digest(&raw);
    Some(format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(d)
    ))
}

/// A target path rynas accepts: absolute, normalized, at least two levels, made of
/// `[A-Za-z0-9_@+.-]` segments (it is written unquoted in the forced command, and btrbk's
/// own filter takes the same set).
pub fn valid_path(p: &str) -> Result<(), String> {
    if !p.starts_with('/') || p.len() > 512 {
        return Err("an absolute path, such as /volume1/backup/laptop".into());
    }
    let segs: Vec<&str> = p[1..].split('/').collect();
    if segs.len() < 2 {
        return Err("a folder inside a volume, such as /volume1/backup/laptop".into());
    }
    for s in &segs {
        if s.is_empty() || *s == "." || *s == ".." {
            return Err("no empty, . or .. parts and no trailing /".into());
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@+.-".contains(&b))
        {
            return Err("letters, digits and _ @ + . - only".into());
        }
    }
    for sys in ["/etc", "/usr", "/bin", "/sbin", "/lib", "/boot", "/proc", "/sys", "/dev", "/run", "/root", "/var/lib/rynas"] {
        if p == sys || p.starts_with(&format!("{sys}/")) {
            return Err("not inside a system folder".into());
        }
    }
    Ok(())
}

/// Arguments of the filter for one device (also what [`crate::receive_filter::parse_args`]
/// reads back).
pub fn filter_args(device: &str, root: &str, policy: &DeletePolicy) -> Vec<String> {
    let mut a = vec![
        "--device".to_string(),
        device.to_string(),
        "--root".into(),
        root.to_string(),
    ];
    if policy.allowed {
        a.push("--delete".into());
        if policy.keep_last > 0 {
            a.extend(["--keep-last".into(), policy.keep_last.to_string()]);
        }
        if policy.min_age_days > 0 {
            a.extend(["--min-age-days".into(), policy.min_age_days.to_string()]);
        }
    } else {
        // the PC can only add, but btrbk must still clear its own interrupted receive
        // before it sends that snapshot again
        a.push("--delete-interrupted".into());
    }
    a
}

/// The `authorized_keys` line of a device: its key, only the filter, nothing else.
pub fn key_line(device: &str, root: &str, policy: &DeletePolicy, kind: &str, blob: &str) -> String {
    format!(
        "restrict,command=\"{FILTER} {}\" {kind} {blob} rynas:{device}",
        filter_args(device, root, policy).join(" ")
    )
}

/// The whole `authorized_keys` of the account.
pub fn authorized_keys(lines: &[String]) -> String {
    let mut s = String::from(
        "# Written by rynas (Backup › PCs). Changes here are replaced: edit the devices there.\n",
    );
    for l in lines {
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// `/etc/sudoers.d/rynas-pcbackup`: btrfs only, with the fixed leading arguments the filter
/// produces, one set of lines per device folder (`roots`, each a [`valid_path`]): receive
/// only `--chroot` into the folder itself, delete only `--commit-each` below it, show, list
/// and usage only on it or below it. A `*` still matches `..` or a second path, so the filter
/// stays required (it normalizes every path and names only snapshots of that device); these
/// lines keep a filter bug from being a root shell. Without folders: no rights at all.
pub fn sudoers(btrfs: &str, roots: &[&str]) -> String {
    let mut s = format!(
        "# Written by rynas (Backup \u{203a} PCs): the account PCs push btrbk snapshots as.\n\
         # Only btrfs, only these forms and only the folders of the PCs; each command is\n\
         # checked first by {FILTER}, the forced command of every key.\n\
         Defaults:{ACCOUNT} !requiretty, !use_pty, !lecture, env_reset\n"
    );
    for r in roots {
        if valid_path(r).is_err() {
            continue;
        }
        let cmds = [
            format!("{btrfs} receive --chroot {r}/"),
            format!("{btrfs} receive --max-errors\\=0 --chroot {r}/"),
            format!("{btrfs} subvolume delete --commit-each {r}/*"),
            format!("{btrfs} subvolume show {r}"),
            format!("{btrfs} subvolume show {r}/*"),
            format!("{btrfs} subvolume list {r}"),
            format!("{btrfs} subvolume list * {r}"),
            format!("{btrfs} subvolume list * {r}/*"),
            format!("{btrfs} filesystem usage {r}"),
            format!("{btrfs} filesystem usage * {r}"),
        ]
        .join(", ");
        s.push_str(&format!("{ACCOUNT} ALL=(root) NOPASSWD: {cmds}\n"));
    }
    s
}

/// The lines for the PC's `/etc/btrbk/btrbk.conf`. With `nas_retention` the NAS deletes old
/// snapshots, so btrbk keeps every one it sent (it could not delete them anyway).
pub fn client_config(
    device: &str,
    host: &str,
    port: Option<u16>,
    path: &str,
    nas_retention: bool,
) -> String {
    let at = match port {
        Some(p) if p != 22 => format!("{host}:{p}"),
        _ => host.to_string(),
    };
    let target = if nas_retention {
        "# old snapshots on the NAS are deleted by the NAS\n\
         target_preserve_min    all\n\
         target_preserve        no\n"
    } else {
        "target_preserve_min    no\n\
         target_preserve        20d 10w 6m\n"
    };
    format!(
        "# /etc/btrbk/btrbk.conf on {device}\n\
         ssh_identity      /etc/btrbk/ssh/id_ed25519\n\
         ssh_user          {ACCOUNT}\n\
         backend_remote    btrfs-progs-sudo\n\
         stream_compress   zstd\n\
         \n\
         snapshot_preserve_min  2d\n\
         snapshot_preserve      14d\n\
         {target}\
         \n\
         # the btrfs volume of this PC and its subvolumes (change to your layout)\n\
         volume /mnt/btr_pool\n\
         \x20 snapshot_dir btrbk_snapshots\n\
         \x20 target ssh://{at}{path}\n\
         \x20 subvolume @\n\
         \x20 subvolume @home\n"
    )
}

/// The btrbk target URL of a device folder (`ssh://host[:port]/path`).
pub fn target_url(host: &str, port: Option<u16>, path: &str) -> String {
    match port {
        Some(p) if p != 22 => format!("ssh://{host}:{p}{path}"),
        _ => format!("ssh://{host}{path}"),
    }
}

/// The one command that sets up a PC with `rynas-agent`: it reads the PC's subvolumes,
/// writes the same `btrbk.conf` settings as [`client_config`] (diff and confirmation when
/// one exists), makes the key and installs the manifest hooks
/// (`docs/research/pc-backup.md` §11.2.5).
pub fn agent_setup_command(device: &str, host: &str, port: Option<u16>, path: &str, nas_retention: bool) -> String {
    let retention = if nas_retention { "nas" } else { "pc" };
    format!(
        "sudo rynas-agent btrbk-setup --nas {} --device {device} --retention {retention}",
        target_url(host, port, path)
    )
}

/// The commands that make the PC's key and print the public half.
pub fn client_key_commands(device: &str) -> String {
    format!(
        "sudo mkdir -p /etc/btrbk/ssh\n\
         sudo ssh-keygen -t ed25519 -N '' -C 'btrbk@{device}' -f /etc/btrbk/ssh/id_ed25519\n\
         sudo cat /etc/btrbk/ssh/id_ed25519.pub"
    )
}

/// The first connection (accepts the NAS host key) and a dry run.
pub fn client_test_commands(host: &str, port: Option<u16>) -> String {
    let p = match port {
        Some(p) if p != 22 => format!(" -p {p}"),
        _ => String::new(),
    };
    format!(
        "sudo ssh -i /etc/btrbk/ssh/id_ed25519{p} {ACCOUNT}@{host}\n\
         sudo btrbk -v dryrun\n\
         sudo btrbk -v run"
    )
}

/// The device name a folder's last part gives, when it is a valid one.
pub fn device_from_path(path: &str) -> Option<String> {
    let last = path.trim_end_matches('/').rsplit('/').next()?.to_ascii_lowercase();
    let d: String = last
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(32)
        .collect();
    rynas_api_types::pcbackup::valid_device(&d).then_some(d)
}

/// `btrfs filesystem du -s --raw <dir>` output: (total, exclusive, shared).
pub fn parse_du(out: &str) -> Option<(u64, u64, u64)> {
    let line = out.lines().filter(|l| !l.trim().is_empty()).nth(1)?;
    let mut f = line.split_whitespace();
    let total = f.next()?.parse().ok()?;
    let excl = f.next()?.parse().ok()?;
    let shared = f.next()?.parse().unwrap_or(0);
    Some((total, excl, shared))
}

/// `sshd -T` (or sshd_config) lines `allowusers` / `allowgroups`: whether `user` (member of
/// `groups`) is left out.
pub fn sshd_allows(config: &str, user: &str, groups: &[&str]) -> bool {
    let mut users: Option<Vec<String>> = None;
    let mut grps: Option<Vec<String>> = None;
    for l in config.lines() {
        let l = l.trim();
        if l.starts_with('#') {
            continue;
        }
        let mut it = l.split_whitespace();
        let Some(k) = it.next() else { continue };
        // a Match block ends the global part
        if k.eq_ignore_ascii_case("match") {
            break;
        }
        let vals: Vec<String> = it.map(|s| s.to_string()).collect();
        if k.eq_ignore_ascii_case("allowusers") {
            users.get_or_insert_default().extend(vals);
        } else if k.eq_ignore_ascii_case("allowgroups") {
            grps.get_or_insert_default().extend(vals);
        }
    }
    let user_ok = users
        .as_ref()
        .is_none_or(|u| u.iter().any(|p| glob(p.split('@').next().unwrap_or(p), user)));
    let group_ok = grps
        .as_ref()
        .is_none_or(|g| g.iter().any(|p| groups.iter().any(|x| glob(p, x))));
    user_ok && group_ok
}

/// sshd's `*` and `?` patterns.
fn glob(p: &str, s: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &s[1..]),
            _ => false,
        }
    }
    go(p.as_bytes(), s.as_bytes())
}

/// sshd's `Port` (the first one) when not 22.
pub fn sshd_port(config: &str) -> Option<u16> {
    config
        .lines()
        .map(str::trim)
        .take_while(|l| !l.to_ascii_lowercase().starts_with("match "))
        .find_map(|l| {
            let mut it = l.split_whitespace();
            (it.next()?.eq_ignore_ascii_case("port")).then(|| it.next()?.parse::<u16>().ok())?
        })
        .filter(|p| *p != 22)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(name: &str, ro: bool, received: bool) -> Entry {
        Entry {
            name: name.into(),
            info: Some(SubvolInfo {
                readonly: ro,
                received_uuid: received.then(|| "11111111-2222-3333-4444-555555555555".into()),
                otime: 1_700_000_000,
                ..Default::default()
            }),
        }
    }

    fn ms(name: &str) -> u64 {
        name_ms(name).unwrap()
    }

    /// A complete received snapshot made by the NAS at the time its name says (plus `late_s`).
    fn at(name: &str, late_s: i64) -> Entry {
        let mut e = snap(name, true, true);
        e.info.as_mut().unwrap().otime = (ms(name) / 1000) as i64 + late_s;
        e
    }

    #[test]
    fn future_names_do_not_push_real_snapshots_out() {
        let now = ms("home.20261108T0300");
        let p = DeletePolicy { allowed: true, keep_last: 2, min_age_days: 0 };
        let mut all = vec![
            at("home.20261108T0100", 0),
            at("home.20261107T0100", 0),
            at("home.20261101T0100", 0),
        ];
        // two junk snapshots named in 2099, received just now
        for n in ["home.20991231T0000", "home.20991230T0000"] {
            let mut e = snap(n, true, true);
            e.info.as_mut().unwrap().otime = (now / 1000) as i64;
            all.push(e);
        }
        assert!(future_dated("home.20991231T0000", now) && !future_dated("home.20261108T0100", now));
        assert!(delete_refusal("home.20261108T0100", &all, &p, now).is_some());
        assert!(delete_refusal("home.20261107T0100", &all, &p, now).is_some());
        assert_eq!(delete_refusal("home.20261101T0100", &all, &p, now), None);
        // the junk itself can go
        assert_eq!(delete_refusal("home.20991231T0000", &all, &p, now), None);
    }

    #[test]
    fn newest_and_age_follow_the_nas_clock() {
        let now = ms("home.20261108T0300");
        // named old but received an hour ago: newest by otime, and young
        let all = vec![
            at("home.20261108T0100", 0),
            at("home.20261001T0100", (now / 1000) as i64 - (ms("home.20261001T0100") / 1000) as i64 - 3600),
            at("home.20261101T0100", 0),
        ];
        let p = DeletePolicy { allowed: true, keep_last: 2, min_age_days: 0 };
        assert!(delete_refusal("home.20261001T0100", &all, &p, now).is_some());
        assert_eq!(delete_refusal("home.20261101T0100", &all, &p, now), None);
        let p = DeletePolicy { allowed: true, keep_last: 0, min_age_days: 3 };
        assert!(delete_refusal("home.20261001T0100", &all, &p, now).unwrap().contains("3 days"));
        assert_eq!(delete_refusal("home.20261101T0100", &all, &p, now), None);
    }

    #[test]
    fn delete_rules() {
        let all = vec![
            at("home.20261108T0100", 0),
            at("home.20261107T0100", 0),
            at("home.20261101T0100", 0),
            at("root.20261101T0100", 0),
            snap("home.20261108T0200", false, false),
        ];
        let now = ms("home.20261108T0300");
        let p = DeletePolicy { allowed: true, keep_last: 2, min_age_days: 3 };
        // the two newest of home are kept, the old one goes, the old root is the only root
        assert!(delete_refusal("home.20261108T0100", &all, &p, now).is_some());
        let p2 = DeletePolicy { min_age_days: 0, ..p };
        assert!(delete_refusal("home.20261107T0100", &all, &p2, now).unwrap().contains("newest 2"));
        assert!(delete_refusal("home.20261107T0100", &all, &p, now).is_some());
        assert_eq!(delete_refusal("home.20261101T0100", &all, &p, now), None);
        assert!(delete_refusal("root.20261101T0100", &all, &p, now).is_some());
        // an interrupted receive always goes
        assert_eq!(delete_refusal("home.20261108T0200", &all, &p, now), None);
        // age only
        let p = DeletePolicy { allowed: true, keep_last: 0, min_age_days: 3 };
        assert!(delete_refusal("home.20261107T0100", &all, &p, now).unwrap().contains("3 days"));
        assert_eq!(delete_refusal("home.20261101T0100", &all, &p, now), None);
        // off
        let p = DeletePolicy { allowed: false, keep_last: 0, min_age_days: 0 };
        assert!(delete_refusal("home.20261101T0100", &all, &p, now).is_some());
        assert_eq!(delete_refusal("home.20261108T0200", &all, &p, now), None);
        assert!(delete_refusal("notasnapshot", &all, &p, now).is_some());
    }

    #[test]
    fn summary() {
        let all = vec![
            snap("home.20261108T0200", false, false),
            snap("home.20261108T0100", true, true),
            snap("home.20261107T0100", true, true),
            snap("root.20261101T0100", true, true),
            Entry { name: "home.20261001T0100".into(), info: None },
        ];
        let s = summarize(&all, &DeletePolicy::default(), ms("home.20261108T0300"));
        assert_eq!(s.incomplete, 1);
        assert_eq!(s.groups.len(), 2);
        assert_eq!(s.groups[0].name, "home");
        assert_eq!(s.groups[0].snapshots, 2);
        assert_eq!(s.groups[0].newest_ms, Some(ms("home.20261108T0100")));
        assert_eq!(s.last_received_ms, Some(1_700_000_000_000));
        let l = &s.list;
        assert!(l[0].incomplete && !l[0].protected);
        assert!(l[1].protected && !l[2].protected && l[3].protected);
        assert!(l[4].not_subvolume && !l[4].protected);
        // the newest complete snapshot of each subvolume is the next parent
        assert!(!l[0].parent && l[1].parent && !l[2].parent && l[3].parent && !l[4].parent);
    }

    #[test]
    fn parents_ignore_future_names_and_follow_the_nas_clock() {
        let now = ms("home.20261108T0300");
        let all = vec![
            at("home.20991231T0000", 10),
            at("home.20261108T0100", 5),
            at("home.20261107T0100", 1),
            snap("root.20261108T0200", false, false),
        ];
        // home.20261108T0100 was received last on the NAS: it is the parent; the future name
        // and the interrupted receive are not
        assert_eq!(parents(&all, now), BTreeSet::from(["home.20261108T0100".to_string()]));
        // a subvolume with only a future name still has that as its parent
        assert_eq!(parents(&all[..1], now).len(), 1);
    }

    const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl marco@pc";

    #[test]
    fn keys() {
        let (k, b) = parse_public_key(ED).unwrap();
        assert_eq!(k, "ssh-ed25519");
        assert!(fingerprint(&b).unwrap().starts_with("SHA256:"));
        assert!(parse_public_key("command=\"x\" ssh-ed25519 AAAA").is_err());
        assert!(parse_public_key("ssh-ed25519").is_err());
        assert!(parse_public_key("ssh-ed25519 !!!!").is_err());
        // the blob names another type
        assert!(parse_public_key(&ED.replace("ssh-ed25519 ", "ecdsa-sha2-nistp256 ")).is_err());
        assert!(parse_public_key(&format!("{ED}\nssh-ed25519 AAAA")).is_err());
        let line = key_line("laptop", "/volume1/backup/laptop", &DeletePolicy { allowed: true, keep_last: 3, min_age_days: 7 }, &k, &b);
        assert_eq!(
            line,
            format!("restrict,command=\"{FILTER} --device laptop --root /volume1/backup/laptop --delete --keep-last 3 --min-age-days 7\" ssh-ed25519 {b} rynas:laptop")
        );
        let line = key_line("laptop", "/v/b/l", &DeletePolicy { allowed: false, keep_last: 3, min_age_days: 7 }, &k, &b);
        // the PC can only add: its own interrupted receives are all it may delete
        assert!(line.contains(" --delete-interrupted\"") && !line.contains("--delete ") && !line.contains("--keep-last"));
    }

    #[test]
    fn paths() {
        assert!(valid_path("/volume1/backup/laptop").is_ok());
        assert!(valid_path("/srv/pc").is_ok());
        for bad in ["volume1/x", "/x", "/v/../etc", "/v/b/", "/v/a b", "/v/a'b", "/v/a;b", "/etc/x", "/var/lib/rynas/x", "/v//b"] {
            assert!(valid_path(bad).is_err(), "{bad}");
        }
        assert_eq!(device_from_path("/volume1/backup/Old_Laptop"), Some("old-laptop".into()));
        assert_eq!(device_from_path("/volume1/backup/___"), None);
    }

    #[test]
    fn texts() {
        let s = sudoers("/usr/bin/btrfs", &["/volume1/backup/pc", "/v/b/x;id", "/volume2/l"]);
        assert!(s.contains("rynas-pcbackup ALL=(root) NOPASSWD: /usr/bin/btrfs receive --chroot /volume1/backup/pc/, /usr/bin/btrfs receive --max-errors\\=0 --chroot /volume1/backup/pc/, /usr/bin/btrfs subvolume delete --commit-each /volume1/backup/pc/*,"));
        assert!(s.contains("--chroot /volume2/l/"));
        // an invalid folder never reaches the file; no bare `btrfs receive *`
        assert!(!s.contains("x;id") && !s.contains("receive *") && !s.contains("delete *"));
        assert!(!s.contains("readlink") && s.contains("!use_pty"));
        assert_eq!(s.matches("NOPASSWD").count(), 2);
        assert!(!sudoers("/usr/bin/btrfs", &[]).contains("NOPASSWD"));
        let c = client_config("laptop", "nas.local", Some(2222), "/volume1/backup/laptop", false);
        assert!(c.contains("target ssh://nas.local:2222/volume1/backup/laptop"));
        assert!(c.contains("ssh_user          rynas-pcbackup"));
        assert!(c.contains("target_preserve        20d 10w 6m"));
        let c = client_config("l", "nas", Some(22), "/a/b", true);
        assert!(c.contains("ssh://nas/a/b"));
        assert!(c.contains("target_preserve_min    all") && !c.contains("20d 10w"));
        assert!(client_test_commands("nas", Some(2222)).contains("-p 2222 rynas-pcbackup@nas"));
        assert!(client_key_commands("laptop").contains("btrbk@laptop"));
        assert_eq!(
            agent_setup_command("laptop", "nas.local", Some(2222), "/volume1/backup/laptop", true),
            "sudo rynas-agent btrbk-setup --nas ssh://nas.local:2222/volume1/backup/laptop --device laptop --retention nas"
        );
        assert!(agent_setup_command("l", "nas", Some(22), "/a/b", false).contains("--nas ssh://nas/a/b --device l --retention pc"));
    }

    #[test]
    fn du_and_sshd() {
        let out = "     Total   Exclusive  Set shared  Filename\n 123456  1000  122456  /v/b/l\n";
        assert_eq!(parse_du(out), Some((123456, 1000, 122456)));
        assert_eq!(parse_du("garbage"), None);
        assert!(sshd_allows("Port 22\n", "rynas-pcbackup", &["rynas-pcbackup"]));
        assert!(!sshd_allows("AllowUsers marco admin\n", "rynas-pcbackup", &["rynas-pcbackup"]));
        assert!(sshd_allows("AllowUsers marco rynas-*\n", "rynas-pcbackup", &["rynas-pcbackup"]));
        assert!(!sshd_allows("AllowGroups sudo\n", "rynas-pcbackup", &["rynas-pcbackup"]));
        assert!(sshd_allows("AllowGroups sudo rynas-pcbackup\nMatch User x\n AllowUsers y\n", "rynas-pcbackup", &["rynas-pcbackup"]));
        assert_eq!(sshd_port("# c\nPort 2222\n"), Some(2222));
        assert_eq!(sshd_port("Port 22\n"), None);
        assert_eq!(sshd_port(""), None);
    }
}
