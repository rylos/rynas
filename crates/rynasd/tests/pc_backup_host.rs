//! PC backup on a real btrfs folder ([`HostReceive`]): quota sizes from sysfs, measured
//! sizes, the manifest read through descriptors, the folder lock shared with the filter and
//! the NAS-side delete. Needs root and a throwaway btrfs folder holding received snapshots,
//! so it is `#[ignore]`d (it never passes in silence):
//!
//! ```sh
//! sudo RYNAS_TEST_PCBACKUP=/mnt/loop/pc <test binary> --ignored --nocapture host_receive
//! ```
//!
//! The folder must hold at least two complete received snapshots of one subvolume, the
//! newest with `.rynas/manifest.json` and the oldest with `.rynas/manifest.json` as a
//! symlink (`packaging/test-pcbackup-loop.sh` makes such a folder on a loop device).

use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Instant;

use rynasd::pc_backup::{HostReceive, ReceiveHost};

#[test]
#[ignore = "root and a throwaway btrfs folder: RYNAS_TEST_PCBACKUP=<folder>"]
fn host_receive() {
    let root = std::env::var("RYNAS_TEST_PCBACKUP").expect("RYNAS_TEST_PCBACKUP=<folder>");
    let root = Path::new(&root);
    let h = HostReceive;
    let (_, entries, _) = h.scan(root).unwrap().expect("the folder");
    let complete: Vec<_> = entries.iter().filter(|e| e.info.is_some() && !e.incomplete()).collect();
    assert!(complete.len() >= 2, "{entries:?}");

    // sizes from the quota tree (when quotas are on), timed
    let t = Instant::now();
    let q = h.quota_sizes(root, &entries);
    println!("quota sizes: {:?} in {:?}", q.as_ref().map(|m| m.len()), t.elapsed());
    if let Some(q) = &q {
        assert!(complete.iter().all(|e| q.get(&e.name).is_some_and(|&(r, _)| r > 0)), "{q:?}");
    }
    // measured sizes, timed
    for e in &complete {
        let t = Instant::now();
        let (r, x) = h.measure(&root.join(&e.name)).unwrap();
        println!("measure {}: referenced {r}, exclusive {x} in {:?}", e.name, t.elapsed());
        assert!(r > 0 && x <= r);
    }

    // the manifest of the newest snapshot; a symlink in an older one is refused, never followed
    let newest = &complete[0].name;
    let m = h.read_manifest(root, newest).unwrap().expect("a manifest in the newest snapshot");
    assert!(m.starts_with(b"{"));
    let oldest = &complete.last().unwrap().name;
    let r = h.read_manifest(root, oldest);
    println!("oldest snapshot manifest (a symlink): {r:?}");
    assert!(r.is_err());

    // the lock: a receive (the filter's exclusive flock) makes `receiving` true and `lock` None
    let dir = std::fs::File::open(root).unwrap();
    assert!(unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0);
    assert!(h.receiving(root));
    assert!(h.lock(root).unwrap().is_none());
    drop(dir);
    assert!(!h.receiving(root));

    // the NAS deletes the oldest complete snapshot under the lock
    let oldest = complete.last().unwrap().name.clone();
    let lock = h.lock(root).unwrap().expect("free");
    assert!(h.delete_snapshot(&lock, "../x").is_err());
    assert!(h.delete_snapshot(&lock, ".rynas").is_err());
    h.delete_snapshot(&lock, &oldest).unwrap();
    drop(lock);
    let (_, after, _) = h.scan(root).unwrap().unwrap();
    assert!(after.iter().all(|e| e.name != oldest));
    let (free, total) = h.space(root).unwrap();
    println!("space: {free} free of {total}; deleted {oldest}");
}
