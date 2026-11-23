//! Resource classes and the queue of the scheduling gate (S4 of
//! `docs/research/scheduling.md`, decision D4): the pure rules. rynasd builds the table of
//! what holds what ([`Holder`]) and of the starts waiting ([`Waiter`]); this module says
//! whether a start may go ([`blocked_by`]) and whether a running scrub must give way
//! ([`yield_to`]).
//!
//! Claims come from the action, never from the user ([`priority_of`] and rynasd's
//! `claims_of`):
//!
//! | Claim | Held by | Rule |
//! |---|---|---|
//! | [`Claim::Volume`] | scrub, backups (their source volumes), replication (send and local receive), PC repository maintenance, storage work | one at a time per volume, by priority |
//! | [`Claim::Disk`] | SMART long test, md RAID check | not with heavy work on volumes of that disk |
//! | [`Claim::Packages`] | system updates, Docker and app updates, package installs | one at a time |
//! | [`Claim::Power`] | a scheduled shutdown | nothing new starts while it waits |
//! | [`Claim::Maintenance`] | applying the configuration, rollbacks, migrations | updates wait for it |
//! | [`Claim::Busy`] | any other work | only a shutdown waits for it |
//!
//! Priority when queued: shutdown > snapshot > backup/replication > PC maintenance > scrub >
//! SMART long test (and the md check) > deduplication. A waiter goes first when its priority
//! is higher, or equal and it came first.
//!
//! What gives way: a scrub *yields* (rynasd cancels it, `btrfs scrub resume` later goes on
//! from where it stopped) to more urgent work on its volume; deduplication gives way through
//! the filesystem lock as before (`rynasd::fslock`); a SMART long test and the md check run
//! inside the drive or the md driver, cannot be paused without losing their progress and slow
//! themselves down for other I/O, so more urgent work on their disks goes on beside them
//! instead of waiting hours (they never start beside heavy work, though).

use rynas_api_types::scheduler::TaskAction;
use serde::{Deserialize, Serialize};

/// What scheduled work holds or waits for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "class", content = "key", rename_all = "snake_case")]
pub enum Claim {
    /// Heavy I/O on a filesystem: its key (`major:minor`, as `rynasd::fslock`), or
    /// [`EVERY`] for every volume.
    Volume(String),
    /// A disk (`sda`).
    Disk(String),
    Packages,
    Power,
    Maintenance,
    /// Any work going on: only a shutdown waits for it.
    Busy,
}

/// The key of a claim on every volume.
pub const EVERY: &str = "*";

/// Priorities (higher goes first).
pub mod priority {
    pub const SHUTDOWN: u8 = 70;
    pub const SNAPSHOT: u8 = 60;
    pub const TRANSFER: u8 = 50;
    pub const PC_MAINTENANCE: u8 = 40;
    pub const PACKAGES: u8 = 35;
    pub const SCRUB: u8 = 30;
    pub const DISK_TEST: u8 = 20;
    pub const DEDUP: u8 = 10;
    pub const OTHER: u8 = 0;
}

/// The priority of a task's work.
pub fn priority_of(a: &TaskAction) -> u8 {
    match a {
        TaskAction::PowerShutdown { .. } => priority::SHUTDOWN,
        TaskAction::Snapshot { .. } => priority::SNAPSHOT,
        TaskAction::Backup { .. } | TaskAction::Replicate { .. } => priority::TRANSFER,
        TaskAction::PcMaintenance => priority::PC_MAINTENANCE,
        TaskAction::SystemUpgrade | TaskAction::DockerAutoUpdate | TaskAction::KernelUpdate => priority::PACKAGES,
        TaskAction::Scrub { .. } => priority::SCRUB,
        TaskAction::SmartTest { .. } | TaskAction::MdCheck { .. } => priority::DISK_TEST,
        TaskAction::Dedup { .. } => priority::DEDUP,
        _ => priority::OTHER,
    }
}

/// The task's start can wait in a queue (it claims a volume, a disk or the package system):
/// its unit has no start timeout, its gate keeps asking while it waits.
pub fn queueable(a: &TaskAction) -> bool {
    a.heavy()
}

/// How a holder behaves when more urgent work wants what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gives {
    /// It goes on: the other waits.
    #[default]
    No,
    /// It stops and resumes later (a scheduled scrub).
    Yields,
    /// It slows itself down and cannot be paused (a SMART long test, the md check): more
    /// urgent work on its disks goes on beside it.
    Throttles,
    /// Deduplication: gives way through the filesystem lock; never blocks anything here.
    Dedup,
}

/// Work holding resources now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Holder {
    /// The scheduled task, when it is one.
    pub task: Option<String>,
    pub name: String,
    /// The rynas task type, for other work (`backup.run`).
    pub kind: Option<String>,
    pub claims: Vec<Claim>,
    pub priority: u8,
    pub gives: Gives,
}

/// A start waiting in the queue.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Waiter {
    pub task: String,
    pub claims: Vec<Claim>,
    pub priority: u8,
    /// When it started waiting (kept across restarts of rynasd: rynasctl sends it).
    pub since_ms: u64,
}

/// What blocks a start: a holder or a waiter ahead of it, and the claim of the start it
/// meets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Holder(usize, Claim),
    Waiter(usize, Claim),
}

/// Two claims cannot be held together. `disks` gives the disks under a volume key.
pub fn conflict(a: &Claim, b: &Claim, disks: &dyn Fn(&str) -> Vec<String>) -> bool {
    use Claim::*;
    match (a, b) {
        (Busy, Power) | (Power, Busy) => true,
        (Busy, _) | (_, Busy) => false,
        (Power, _) | (_, Power) => true,
        (Packages, Packages) | (Packages, Maintenance) | (Maintenance, Packages) => true,
        (Maintenance, _) | (_, Maintenance) => false,
        (Volume(x), Volume(y)) => x == y || x == EVERY || y == EVERY,
        (Disk(x), Disk(y)) => x == y,
        (Disk(d), Volume(v)) | (Volume(v), Disk(d)) => v == EVERY || disks(v).iter().any(|x| x == d),
        _ => false,
    }
}

/// The first claim of `mine` that meets one of `theirs`.
fn meets(mine: &[Claim], theirs: &[Claim], disks: &dyn Fn(&str) -> Vec<String>) -> Option<(Claim, Claim)> {
    mine.iter()
        .find_map(|m| theirs.iter().find(|t| conflict(m, t, disks)).map(|t| (m.clone(), t.clone())))
}

/// `w` goes before `me` in the queue.
fn ahead(w: &Waiter, me: &Waiter) -> bool {
    (w.priority, std::cmp::Reverse(w.since_ms), std::cmp::Reverse(&w.task))
        > (me.priority, std::cmp::Reverse(me.since_ms), std::cmp::Reverse(&me.task))
}

/// What keeps the start `me` from going now (`None`: it goes): a holder of something it
/// needs (a yielding scrub too, until it has stopped), or a waiter that goes before it and
/// could start now. A deduplication run never blocks; a disk test does not block more urgent
/// volume work.
///
/// Backfill: a waiter ahead that is itself held back by what other work holds (a backup of
/// volumes A and C while C is busy) does not keep a less urgent start on a free resource (a
/// scrub of A) waiting; once that waiter can start, it goes first again.
pub fn blocked_by(
    me: &Waiter,
    holders: &[Holder],
    waiters: &[Waiter],
    disks: &dyn Fn(&str) -> Vec<String>,
) -> Option<Block> {
    if let Some(b) = held_by(me, holders, disks) {
        return Some(b);
    }
    waiters
        .iter()
        .enumerate()
        .filter(|(_, w)| w.task != me.task && ahead(w, me) && held_by(w, holders, disks).is_none())
        .find_map(|(i, w)| meets(&me.claims, &w.claims, disks).map(|(m, _)| Block::Waiter(i, m)))
}

/// The holder that keeps the start `me` from going, waiters left aside.
fn held_by(me: &Waiter, holders: &[Holder], disks: &dyn Fn(&str) -> Vec<String>) -> Option<Block> {
    for (i, h) in holders.iter().enumerate() {
        if h.task.as_deref() == Some(me.task.as_str()) || h.gives == Gives::Dedup {
            continue;
        }
        for m in &me.claims {
            for c in &h.claims {
                if !conflict(m, c, disks) {
                    continue;
                }
                let beside = h.gives == Gives::Throttles
                    && me.priority > h.priority
                    && matches!(c, Claim::Disk(_))
                    && matches!(m, Claim::Volume(_));
                if !beside {
                    return Some(Block::Holder(i, m.clone()));
                }
            }
        }
    }
    None
}

/// A yielding holder (a scrub) must give way now: more urgent work holds or waits for what it
/// holds. Returns what it gives way to.
pub fn yield_to(
    h: &Holder,
    holders: &[Holder],
    waiters: &[Waiter],
    disks: &dyn Fn(&str) -> Vec<String>,
) -> Option<Block> {
    if h.gives != Gives::Yields {
        return None;
    }
    for (i, o) in holders.iter().enumerate() {
        if o.task.is_some() && o.task == h.task || o.gives == Gives::Dedup || o.priority <= h.priority {
            continue;
        }
        if let Some((m, _)) = meets(&h.claims, &o.claims, disks) {
            return Some(Block::Holder(i, m));
        }
    }
    waiters
        .iter()
        .enumerate()
        .filter(|(_, w)| Some(w.task.as_str()) != h.task.as_deref() && w.priority > h.priority)
        .find_map(|(i, w)| meets(&h.claims, &w.claims, disks).map(|(m, _)| Block::Waiter(i, m)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::scheduler::SmartTestKind;

    fn disks(v: &str) -> Vec<String> {
        match v {
            "0:40" => vec!["sda".into(), "sdb".into()],
            "0:41" => vec!["sdc".into()],
            _ => vec![],
        }
    }

    fn vol(k: &str) -> Claim {
        Claim::Volume(k.into())
    }

    fn holder(task: &str, claims: Vec<Claim>, priority: u8, gives: Gives) -> Holder {
        Holder { task: Some(task.into()), name: task.into(), kind: None, claims, priority, gives }
    }

    fn waiter(task: &str, claims: Vec<Claim>, priority: u8, since_ms: u64) -> Waiter {
        Waiter { task: task.into(), claims, priority, since_ms }
    }

    #[test]
    fn claims_meet() {
        assert!(conflict(&vol("0:40"), &vol("0:40"), &disks));
        assert!(!conflict(&vol("0:40"), &vol("0:41"), &disks));
        assert!(conflict(&vol(EVERY), &vol("0:41"), &disks));
        assert!(conflict(&Claim::Disk("sdb".into()), &vol("0:40"), &disks));
        assert!(!conflict(&Claim::Disk("sdc".into()), &vol("0:40"), &disks));
        assert!(conflict(&Claim::Packages, &Claim::Packages, &disks));
        assert!(conflict(&Claim::Packages, &Claim::Maintenance, &disks));
        assert!(!conflict(&Claim::Packages, &vol("0:40"), &disks));
        assert!(conflict(&Claim::Power, &Claim::Busy, &disks));
        assert!(conflict(&Claim::Power, &vol("0:40"), &disks));
        assert!(!conflict(&Claim::Busy, &vol("0:40"), &disks));
        assert!(!conflict(&Claim::Busy, &Claim::Busy, &disks));
    }

    #[test]
    fn heavy_work_on_one_volume_goes_one_at_a_time() {
        let backup = waiter("backup", vec![vol("0:40")], priority::TRANSFER, 10);
        let running = [holder("replica", vec![vol("0:40")], priority::TRANSFER, Gives::No)];
        assert_eq!(blocked_by(&backup, &running, &[], &disks), Some(Block::Holder(0, vol("0:40"))));
        // another volume: at once
        let other = waiter("b2", vec![vol("0:41")], priority::TRANSFER, 10);
        assert_eq!(blocked_by(&other, &running, &[], &disks), None);
        // its own lease (a restart of rynasd rebuilt it) never blocks it
        let mine = [holder("backup", vec![vol("0:40")], priority::TRANSFER, Gives::No)];
        assert_eq!(blocked_by(&backup, &mine, &[], &disks), None);
    }

    #[test]
    fn the_queue_goes_by_priority_then_arrival() {
        let scrub = waiter("scrub", vec![vol("0:40")], priority::SCRUB, 1);
        let backup = waiter("backup", vec![vol("0:40")], priority::TRANSFER, 5);
        let pc = waiter("pc", vec![vol("0:40")], priority::PC_MAINTENANCE, 3);
        let all = [scrub.clone(), backup.clone(), pc.clone()];
        assert_eq!(blocked_by(&backup, &[], &all, &disks), None);
        assert_eq!(blocked_by(&pc, &[], &all, &disks), Some(Block::Waiter(1, vol("0:40"))));
        assert!(blocked_by(&scrub, &[], &all, &disks).is_some());
        // equal priority: who came first
        let b2 = waiter("backup2", vec![vol("0:40")], priority::TRANSFER, 4);
        let q = [backup.clone(), b2.clone()];
        assert!(blocked_by(&backup, &[], &q, &disks).is_some());
        assert_eq!(blocked_by(&b2, &[], &q, &disks), None);
    }

    #[test]
    fn a_scrub_yields_to_more_urgent_work_only() {
        let scrub = holder("scrub", vec![vol("0:40")], priority::SCRUB, Gives::Yields);
        let backup = waiter("backup", vec![vol("0:40")], priority::TRANSFER, 5);
        assert_eq!(yield_to(&scrub, std::slice::from_ref(&scrub), std::slice::from_ref(&backup), &disks), Some(Block::Waiter(0, vol("0:40"))));
        // the backup waits until the scrub has stopped
        assert!(blocked_by(&backup, std::slice::from_ref(&scrub), std::slice::from_ref(&backup), &disks).is_some());
        // a SMART long test is less urgent: it waits, the scrub goes on
        let smart = waiter("smart", vec![Claim::Disk("sda".into())], priority::DISK_TEST, 5);
        assert_eq!(yield_to(&scrub, &[], std::slice::from_ref(&smart), &disks), None);
        assert!(blocked_by(&smart, std::slice::from_ref(&scrub), &[], &disks).is_some());
        // work started by hand on the volume (a backup run from its page) makes it yield too
        let manual = Holder { task: None, name: String::new(), kind: Some("backup.run".into()), claims: vec![vol("0:40")], priority: priority::TRANSFER, gives: Gives::No };
        assert!(yield_to(&scrub, &[scrub.clone(), manual], &[], &disks).is_some());
        // a non-yielding holder never yields
        let pc = holder("pc", vec![vol("0:40")], priority::PC_MAINTENANCE, Gives::No);
        assert_eq!(yield_to(&pc, &[], &[backup], &disks), None);
    }

    #[test]
    fn disk_tests_and_deduplication() {
        // a SMART long test runs: a backup on that disk goes on beside it, a scrub (less
        // urgent than the backup but more than the test) too; another test of the disk waits
        let smart = holder("smart", vec![Claim::Disk("sda".into())], priority::DISK_TEST, Gives::Throttles);
        let backup = waiter("backup", vec![vol("0:40")], priority::TRANSFER, 5);
        assert_eq!(blocked_by(&backup, std::slice::from_ref(&smart), &[], &disks), None);
        let md = waiter("md", vec![Claim::Disk("sda".into())], priority::DISK_TEST, 5);
        assert!(blocked_by(&md, std::slice::from_ref(&smart), &[], &disks).is_some());
        // deduplication never blocks (the filesystem lock makes it give way)
        let dedup = Holder { task: Some("dedup-vol1".into()), gives: Gives::Dedup, claims: vec![vol("0:40")], priority: priority::DEDUP, ..Default::default() };
        assert_eq!(blocked_by(&backup, &[dedup], &[], &disks), None);
    }

    #[test]
    fn shutdowns_and_packages() {
        let shut = holder("power-off-night", vec![Claim::Power], priority::SHUTDOWN, Gives::No);
        let backup = waiter("backup", vec![vol("0:40")], priority::TRANSFER, 5);
        assert!(blocked_by(&backup, std::slice::from_ref(&shut), &[], &disks).is_some());
        let apt = waiter("apt-upgrade", vec![Claim::Packages], priority::PACKAGES, 5);
        let docker = holder("docker-auto-update", vec![Claim::Packages], priority::PACKAGES, Gives::No);
        assert!(blocked_by(&apt, std::slice::from_ref(&docker), &[], &disks).is_some());
        // packages and heavy volume work go together
        let scrub = holder("scrub", vec![vol("0:40")], priority::SCRUB, Gives::Yields);
        assert_eq!(blocked_by(&apt, &[scrub], &[], &disks), None);
        assert!(queueable(&TaskAction::SmartTest { disk: "sda".into(), test: SmartTestKind::Long }));
        assert!(!queueable(&TaskAction::SmartTest { disk: "sda".into(), test: SmartTestKind::Short }));
        assert!(!queueable(&TaskAction::PowerShutdown { rule: "n".into() }));
    }

    #[test]
    fn a_waiter_held_back_elsewhere_does_not_block_a_free_resource() {
        // a backup of volumes 0:40 and 0:41 waits for the replica on 0:41; a scrub of 0:40
        // (less urgent) may go meanwhile
        let replica = holder("replica", vec![vol("0:41")], priority::TRANSFER, Gives::No);
        let backup = waiter("backup", vec![vol("0:40"), vol("0:41")], priority::TRANSFER, 1);
        let scrub = waiter("scrub", vec![vol("0:40")], priority::SCRUB, 2);
        let q = [backup.clone(), scrub.clone()];
        assert!(blocked_by(&backup, std::slice::from_ref(&replica), &q, &disks).is_some());
        assert_eq!(blocked_by(&scrub, std::slice::from_ref(&replica), &q, &disks), None);
        // once the backup could start, it goes first again
        assert_eq!(blocked_by(&scrub, &[], &q, &disks), Some(Block::Waiter(0, vol("0:40"))));
        // a waiter held back only by the scrub that yields to it still goes first: the scrub
        // holds the volume, so the less urgent start waits for it anyway
        let s = holder("scrub-running", vec![vol("0:40")], priority::SCRUB, Gives::Yields);
        let pc = waiter("pc", vec![vol("0:40")], priority::PC_MAINTENANCE, 3);
        assert!(blocked_by(&pc, std::slice::from_ref(&s), &[backup, pc.clone()], &disks).is_some());
    }
}
