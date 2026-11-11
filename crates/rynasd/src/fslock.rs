//! One lock per filesystem for heavy work (D-DEDUP, `docs/phase2/dedup.md` §3.7).
//!
//! Deduplication is the only work that gives way: scrub, replication transfers, backups and
//! storage operations ([`FsLocks::heavy`]) register at once, ask a running deduplication on
//! the same filesystem to stop (its engine saves its work on SIGINT, up to 5 minutes) and
//! wait for it to end; a deduplication ([`FsLocks::dedup`]) waits until no other work holds
//! the filesystem, or gives up at its deadline. Two deduplication writers never share a
//! filesystem (oans has no lock of its own on its hashfile).
//!
//! Heavy holders do not wait for each other: that would change what they did before this
//! lock (a 72-hour scrub would hold a replication back).
//!
//! The key of a filesystem is the `major:minor` of its superblock in `/proc/self/mountinfo`
//! ([`key_of`]): the same for every subvolume and bind mount of one btrfs filesystem.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How long heavy work waits for a deduplication run to save its work and end.
pub const PREEMPT_GRACE: Duration = Duration::from_secs(5 * 60 + 30);

#[derive(Default)]
struct Slot {
    /// Heavy holders: `(id, task type)`.
    heavy: Vec<(u64, String)>,
    /// The deduplication run holding the filesystem.
    dedup: Option<DedupHold>,
}

struct DedupHold {
    id: u64,
    preempt: CancellationToken,
    by: Arc<Mutex<Option<String>>>,
}

pub struct FsLocks {
    slots: Mutex<HashMap<String, Slot>>,
    seq: Mutex<u64>,
    changed: watch::Sender<u64>,
    /// `false` while runs left by a previous rynasd are being stopped: the lock lives in
    /// memory only, so nothing takes it until they are gone ([`FsLocks::hold`]).
    ready: watch::Sender<bool>,
}

impl Default for FsLocks {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            seq: Mutex::new(0),
            changed: watch::channel(0).0,
            ready: watch::channel(true).0,
        }
    }
}

/// How long a lock request waits for the leftovers of a previous rynasd to be stopped.
const READY_WAIT: Duration = Duration::from_secs(7 * 60);

/// Held by heavy work; released on drop.
pub struct HeavyGuard {
    locks: Arc<FsLocks>,
    keys: Vec<String>,
    id: u64,
}

impl Drop for HeavyGuard {
    fn drop(&mut self) {
        let mut s = self.locks.slots.lock().unwrap();
        for k in &self.keys {
            if let Some(slot) = s.get_mut(k) {
                slot.heavy.retain(|(i, _)| *i != self.id);
            }
        }
        drop(s);
        self.locks.bump();
    }
}

/// Held by a deduplication run; released on drop.
pub struct DedupGuard {
    locks: Arc<FsLocks>,
    key: String,
    id: u64,
    /// Fires when other work wants the filesystem.
    pub preempt: CancellationToken,
    by: Arc<Mutex<Option<String>>>,
}

impl DedupGuard {
    /// The task type that asked for the filesystem.
    pub fn preempted_by(&self) -> Option<String> {
        self.by.lock().unwrap().clone()
    }
}

impl Drop for DedupGuard {
    fn drop(&mut self) {
        let mut s = self.locks.slots.lock().unwrap();
        if let Some(slot) = s.get_mut(&self.key)
            && slot.dedup.as_ref().is_some_and(|d| d.id == self.id)
        {
            slot.dedup = None;
        }
        drop(s);
        self.locks.bump();
    }
}

static GLOBAL: OnceLock<Arc<FsLocks>> = OnceLock::new();

impl FsLocks {
    /// The locks of this rynasd.
    pub fn global() -> Arc<FsLocks> {
        GLOBAL.get_or_init(|| Arc::new(FsLocks::default())).clone()
    }

    fn bump(&self) {
        self.changed.send_modify(|g| *g += 1);
    }

    fn next_id(&self) -> u64 {
        let mut s = self.seq.lock().unwrap();
        *s += 1;
        *s
    }

    /// Registers heavy work `what` (a task type) on the filesystems `keys` at once, asks a
    /// deduplication run there to stop and waits until it has ended (at most `grace`; the
    /// work goes on after it either way).
    /// Nothing takes the lock until [`FsLocks::release`] (at start, while runs a previous
    /// rynasd left are stopped).
    pub fn hold(&self) {
        self.ready.send_replace(false);
    }

    pub fn release(&self) {
        self.ready.send_replace(true);
    }

    async fn wait_ready(&self) {
        let mut rx = self.ready.subscribe();
        if tokio::time::timeout(READY_WAIT, rx.wait_for(|r| *r)).await.is_err() {
            tracing::warn!("filesystem lock: leftover deduplication runs still stopping, going on");
        }
    }

    pub async fn heavy_with(self: &Arc<Self>, keys: &[String], what: &str, grace: Duration) -> HeavyGuard {
        self.wait_ready().await;
        let id = self.next_id();
        let mut keys: Vec<String> = keys.to_vec();
        keys.sort();
        keys.dedup();
        {
            let mut s = self.slots.lock().unwrap();
            for k in &keys {
                let slot = s.entry(k.clone()).or_default();
                slot.heavy.push((id, what.to_string()));
                if let Some(d) = &slot.dedup {
                    d.by.lock().unwrap().get_or_insert_with(|| what.to_string());
                    d.preempt.cancel();
                }
            }
        }
        self.bump();
        let guard = HeavyGuard { locks: self.clone(), keys: keys.clone(), id };
        let mut rx = self.changed.subscribe();
        let end = Instant::now() + grace;
        loop {
            let busy = {
                let s = self.slots.lock().unwrap();
                keys.iter().any(|k| s.get(k).is_some_and(|x| x.dedup.is_some()))
            };
            if !busy {
                break;
            }
            match tokio::time::timeout_at(end, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => {
                    tracing::warn!("{what}: a deduplication run did not end in time, going on");
                    break;
                }
            }
        }
        guard
    }

    /// [`FsLocks::heavy_with`] with [`PREEMPT_GRACE`].
    pub async fn heavy(self: &Arc<Self>, keys: &[String], what: &str) -> HeavyGuard {
        self.heavy_with(keys, what, PREEMPT_GRACE).await
    }

    /// Heavy work on the filesystems holding `paths` (a path without a known filesystem is
    /// left out).
    pub async fn heavy_paths(self: &Arc<Self>, paths: &[&str], what: &str) -> HeavyGuard {
        let keys: Vec<String> = paths.iter().filter_map(|p| key_of(p)).collect();
        self.heavy(&keys, what).await
    }

    /// Takes the filesystem `key` for a deduplication run: waits while other work holds it
    /// (`waiting` is told who, `None` once free), until `deadline`. `Err(holder)` when the
    /// deadline came first.
    pub async fn dedup(
        self: &Arc<Self>,
        key: &str,
        deadline: Instant,
        waiting: &(dyn Fn(Option<&str>) + Sync),
    ) -> Result<DedupGuard, String> {
        let mut ready = self.ready.subscribe();
        if tokio::time::timeout_at(deadline, ready.wait_for(|r| *r)).await.is_err() {
            return Err("storage.dedup".into());
        }
        let mut rx = self.changed.subscribe();
        let mut told: Option<String> = None;
        loop {
            {
                let mut s = self.slots.lock().unwrap();
                let slot = s.entry(key.to_string()).or_default();
                let holder = slot
                    .heavy
                    .first()
                    .map(|(_, w)| w.clone())
                    .or_else(|| slot.dedup.as_ref().map(|_| "storage.dedup".to_string()));
                match holder {
                    None => {
                        let id = self.next_id();
                        let preempt = CancellationToken::new();
                        let by = Arc::new(Mutex::new(None));
                        slot.dedup = Some(DedupHold { id, preempt: preempt.clone(), by: by.clone() });
                        drop(s);
                        if told.is_some() {
                            waiting(None);
                        }
                        self.bump();
                        return Ok(DedupGuard { locks: self.clone(), key: key.to_string(), id, preempt, by });
                    }
                    Some(h) => {
                        if told.as_deref() != Some(h.as_str()) {
                            waiting(Some(&h));
                            told = Some(h);
                        }
                    }
                }
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                _ => return Err(told.unwrap_or_default()),
            }
        }
    }
}

/// The filesystem key of `path`: `major:minor` of the mount holding it (the longest mount
/// point containing it).
pub fn key_of(path: &str) -> Option<String> {
    key_in(&crate::places::mountinfo(), path)
}

/// [`key_of`] in a given mountinfo.
pub fn key_in(mountinfo: &str, path: &str) -> Option<String> {
    let mounts = rynas_storage::mounts::parse_mountinfo(mountinfo);
    mount_of(&mounts, path).map(|m| m.majmin.clone())
}

/// The mount holding `path`.
pub fn mount_of<'a>(mounts: &'a [rynas_storage::mounts::Mount], path: &str) -> Option<&'a rynas_storage::mounts::Mount> {
    let within = |p: &str, mp: &str| mp == "/" || p == mp || p.strip_prefix(mp).is_some_and(|r| r.starts_with('/'));
    mounts.iter().filter(|m| within(path, &m.mountpoint)).max_by_key(|m| m.mountpoint.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn heavy_work_preempts_dedup_and_dedup_waits() {
        let l = Arc::new(FsLocks::default());
        let far = Instant::now() + Duration::from_secs(3600);
        let g = l.dedup("0:40", far, &|_| {}).await.unwrap();
        let preempt = g.preempt.clone();
        let l2 = l.clone();
        let heavy = tokio::spawn(async move { l2.heavy(&["0:40".into()], "btrfs.scrub").await });
        preempt.cancelled().await;
        assert_eq!(g.preempted_by().as_deref(), Some("btrfs.scrub"));
        // the scrub waits until the run has saved its work
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!heavy.is_finished());
        drop(g);
        let hg = heavy.await.unwrap();
        // a new run waits for the scrub, and is told why
        let told = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let t2 = told.clone();
        let l3 = l.clone();
        let wait = tokio::spawn(async move {
            l3.dedup("0:40", far, &move |h| t2.lock().unwrap().push(h.map(String::from))).await.is_ok()
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!wait.is_finished());
        drop(hg);
        assert!(wait.await.unwrap());
        assert_eq!(*told.lock().unwrap(), [Some("btrfs.scrub".to_string()), None]);
    }

    #[tokio::test]
    async fn dedup_gives_up_at_its_deadline_and_heavy_work_does_not_wait_forever() {
        let l = Arc::new(FsLocks::default());
        let hg = l.heavy(&["0:41".into()], "backup.run").await;
        let r = l.dedup("0:41", Instant::now() + Duration::from_millis(100), &|_| {}).await;
        assert_eq!(r.err().as_deref(), Some("backup.run"));
        drop(hg);
        // a run that never ends: heavy work goes on after the grace
        let g = l.dedup("0:41", Instant::now() + Duration::from_millis(100), &|_| {}).await.unwrap();
        let start = Instant::now();
        let _h = l.heavy_with(&["0:41".into()], "btrfs.scrub", Duration::from_millis(150)).await;
        assert!(start.elapsed() >= Duration::from_millis(150));
        drop(g);
        // other filesystems are not touched, heavy holders do not wait for each other
        let n = AtomicUsize::new(0);
        let _a = l.heavy(&["0:42".into()], "a").await;
        let _b = l.heavy(&["0:42".into()], "b").await;
        let told = |_: Option<&str>| {
            n.fetch_add(1, Ordering::Relaxed);
        };
        assert!(l.dedup("0:43", Instant::now(), &told).await.is_ok());
        assert_eq!(n.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn nothing_takes_the_lock_while_leftovers_stop() {
        let l = Arc::new(FsLocks::default());
        l.hold();
        let r = l.dedup("0:44", Instant::now() + Duration::from_millis(100), &|_| {}).await;
        assert!(r.is_err());
        let l2 = l.clone();
        let heavy = tokio::spawn(async move { l2.heavy(&["0:44".into()], "btrfs.scrub").await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!heavy.is_finished());
        l.release();
        drop(heavy.await.unwrap());
        assert!(l.dedup("0:44", Instant::now() + Duration::from_millis(100), &|_| {}).await.is_ok());
    }

    #[test]
    fn keys_from_mountinfo() {
        let mi = "\
29 1 0:26 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,subvol=/@
40 29 0:40 / /vol1 rw,relatime shared:2 - btrfs /dev/md2 rw,subvol=/
41 29 0:26 /@rynas-system /srv/rynas/system rw shared:3 - btrfs /dev/nvme0n1p2 rw
42 40 0:52 / /vol1/crypt rw shared:4 - fuse.gocryptfs /vol1/.rynas-crypt/crypt rw
";
        assert_eq!(key_in(mi, "/vol1/docs").as_deref(), Some("0:40"));
        assert_eq!(key_in(mi, "/vol1").as_deref(), Some("0:40"));
        assert_eq!(key_in(mi, "/vol10/x").as_deref(), Some("0:26"));
        assert_eq!(key_in(mi, "/srv/rynas/system/a").as_deref(), Some("0:26"));
        assert_eq!(key_in(mi, "/vol1/crypt/a").as_deref(), Some("0:52"));
    }
}
