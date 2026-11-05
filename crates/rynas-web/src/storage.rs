//! `GET /api/v1/storage`: the unprivileged overview of [`rynas_storage::overview`] (disks from
//! sysfs, md arrays from mdstat, mounted filesystems), with filesystem sizes from the
//! [`MountProber`].
//!
//! `statvfs` on a dead NFS/CIFS server blocks in the kernel, possibly forever. It must never
//! run on the shared blocking pool, which also serves the static files (guidelines S7): each
//! probe gets its own small thread, at most one probe per mount is in flight, at most
//! [`MAX_IN_FLIGHT`] in total, and a caller waits at most [`PROBE_WAIT`]. A mount whose probe
//! did not come back is *unresponsive*: it keeps its last known sizes (zero if none), nobody
//! waits for it again until the stuck probe returns, and nothing else is slowed down.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rynas_storage::overview::{FsSizes, statvfs};
use tokio::sync::watch;

/// Sizes younger than this are served from the cache (several tabs polling share a probe).
pub const SIZE_TTL: Duration = Duration::from_secs(5);
/// How long a request waits for a probe before calling the mount unresponsive.
pub const PROBE_WAIT: Duration = Duration::from_secs(2);
/// Probe threads alive at the same time (each may be stuck on a hung mount): with all of them
/// stuck, further mounts are reported unresponsive without a new thread. Counted in the
/// unit's `TasksMax`.
pub const MAX_IN_FLIGHT: usize = 8;

type Probe = watch::Receiver<Option<Option<FsSizes>>>;

#[derive(Default)]
struct MountState {
    last: Option<FsSizes>,
    at: Option<Instant>,
    /// The probe running now, if any.
    in_flight: Option<Probe>,
    /// The last probe did not answer within [`PROBE_WAIT`].
    unresponsive: bool,
}

/// Single-flight, bounded `statvfs` with a per-mount cache and unresponsive state.
#[derive(Default)]
pub struct MountProber {
    /// Keyed by mountpoint; pruned to the mounted filesystems on every overview, so it is
    /// bounded by the mount table.
    mounts: Mutex<HashMap<String, MountState>>,
}

/// What a probe request found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probed {
    pub sizes: Option<FsSizes>,
    pub unresponsive: bool,
}

impl MountProber {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, MountState>> {
        self.mounts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forget mounts that are no longer mounted (unless a probe of theirs is still out).
    pub fn retain(&self, mounted: &[&str]) {
        self.lock()
            .retain(|k, v| v.in_flight.is_some() || mounted.contains(&k.as_str()));
    }

    /// Whether `mountpoint` did not answer its last probe.
    pub fn is_unresponsive(&self, mountpoint: &str) -> bool {
        self.lock().get(mountpoint).is_some_and(|m| m.unresponsive)
    }

    /// Sizes of the filesystem at `mountpoint`, probing it if the cached value is old.
    pub async fn sizes(self: &Arc<Self>, mountpoint: &str) -> Probed {
        self.sizes_with(mountpoint, statvfs).await
    }

    /// [`MountProber::sizes`] with another probe function (tests).
    pub async fn sizes_with(
        self: &Arc<Self>,
        mountpoint: &str,
        probe: fn(&str) -> Option<FsSizes>,
    ) -> Probed {
        let mut rx = {
            let mut map = self.lock();
            let in_flight_total = map.values().filter(|m| m.in_flight.is_some()).count();
            let m = map.entry(mountpoint.to_string()).or_default();
            let fresh = m.at.is_some_and(|t| t.elapsed() < SIZE_TTL);
            let now = Probed {
                sizes: m.last,
                unresponsive: m.unresponsive,
            };
            match &m.in_flight {
                // stuck on this mount: do not wait for it again
                Some(_) if m.unresponsive => return now,
                Some(rx) => rx.clone(),
                None if fresh => return now,
                None if in_flight_total >= MAX_IN_FLIGHT => {
                    if !m.unresponsive {
                        tracing::warn!(
                            "statvfs {mountpoint}: {MAX_IN_FLIGHT} probes already stuck, not probing"
                        );
                    }
                    m.unresponsive = true;
                    return Probed {
                        sizes: m.last,
                        unresponsive: true,
                    };
                }
                None => {
                    let (tx, rx) = watch::channel(None);
                    m.in_flight = Some(rx.clone());
                    let me = self.clone();
                    let mp = mountpoint.to_string();
                    let spawned = std::thread::Builder::new()
                        .name("statvfs".into())
                        .stack_size(64 << 10)
                        .spawn(move || {
                            let r = probe(&mp);
                            {
                                let mut map = me.lock();
                                if let Some(m) = map.get_mut(&mp) {
                                    if m.unresponsive {
                                        tracing::info!("statvfs {mp}: answering again");
                                    }
                                    m.in_flight = None;
                                    m.unresponsive = false;
                                    m.at = Some(Instant::now());
                                    if r.is_some() {
                                        m.last = r;
                                    }
                                }
                            }
                            let _ = tx.send(Some(r));
                        });
                    if let Err(e) = spawned {
                        tracing::error!("statvfs {mountpoint}: cannot start a probe: {e}");
                        m.in_flight = None;
                        return now;
                    }
                    rx
                }
            }
        };
        match tokio::time::timeout(PROBE_WAIT, rx.wait_for(Option::is_some)).await {
            Ok(Ok(v)) => Probed {
                sizes: v
                    .flatten()
                    .or_else(|| self.lock().get(mountpoint).and_then(|m| m.last)),
                unresponsive: false,
            },
            // the probe thread died without an answer: let the next request try again
            Ok(Err(_)) => {
                let mut map = self.lock();
                let m = map.entry(mountpoint.to_string()).or_default();
                m.in_flight = None;
                Probed {
                    sizes: m.last,
                    unresponsive: m.unresponsive,
                }
            }
            Err(_) => {
                let mut map = self.lock();
                let m = map.entry(mountpoint.to_string()).or_default();
                if !m.unresponsive {
                    tracing::warn!(
                        "statvfs {mountpoint}: no answer in {PROBE_WAIT:?}, marked unresponsive"
                    );
                }
                m.unresponsive = true;
                Probed {
                    sizes: m.last,
                    unresponsive: true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    static RELEASE: AtomicBool = AtomicBool::new(false);

    fn ok(_: &str) -> Option<FsSizes> {
        Some(FsSizes {
            size: 100,
            used: 40,
            avail: 60,
        })
    }

    /// Hangs until RELEASE, like statvfs on a dead NFS server.
    fn hung(_: &str) -> Option<FsSizes> {
        while !RELEASE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(FsSizes {
            size: 7,
            used: 1,
            avail: 6,
        })
    }

    #[tokio::test]
    async fn caches_and_marks_hung_mounts_unresponsive() {
        let p = MountProber::new();
        let r = p.sizes_with("/", ok).await;
        assert_eq!(r.sizes.unwrap().size, 100);
        assert!(!r.unresponsive);

        let t = Instant::now();
        let r = p.sizes_with("/mnt/nfs", hung).await;
        assert!(r.unresponsive);
        assert_eq!(r.sizes, None);
        assert!(t.elapsed() >= PROBE_WAIT && t.elapsed() < PROBE_WAIT * 2);
        // asked again while stuck: answered at once, no second thread
        let t = Instant::now();
        let r = p.sizes_with("/mnt/nfs", hung).await;
        assert!(r.unresponsive);
        assert!(t.elapsed() < Duration::from_millis(100));
        assert!(p.is_unresponsive("/mnt/nfs"));
        // other mounts are not slowed down
        let t = Instant::now();
        assert!(!p.sizes_with("/", ok).await.unresponsive);
        assert!(t.elapsed() < Duration::from_millis(100));

        // the server comes back: the stuck probe finishes and the mount recovers
        RELEASE.store(true, Ordering::SeqCst);
        for _ in 0..100 {
            if !p.is_unresponsive("/mnt/nfs") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let r = p.sizes_with("/mnt/nfs", hung).await;
        assert!(!r.unresponsive);
        assert_eq!(r.sizes.unwrap().size, 7);

        // unmounted filesystems are forgotten
        p.retain(&["/"]);
        assert_eq!(p.lock().len(), 1);
    }
}
