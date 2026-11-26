//! PC backup on a real btrfs folder ([`HostReceive`]): quota sizes from sysfs, the space by
//! neighbours against full walks, the manifest read through descriptors, the folder lock shared with the filter and
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

use rynas_btrfs::extents::{self, Limits};
use rynas_btrfs::pcspace::Snap;
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
    // the space by neighbours, timed, checked against full walks of every snapshot; a second
    // measure with what the first found out has nothing to compare
    let mut by: std::collections::BTreeMap<&str, Vec<&rynas_btrfs::receive::Entry>> = Default::default();
    for e in &complete {
        let (line, _) = rynas_btrfs::name::split_snapshot_name(&e.name).unwrap();
        by.entry(line).or_default().push(e);
    }
    let mut lines: Vec<Vec<Snap>> = by
        .into_values()
        .map(|mut v| {
            v.sort_by(|a, b| rynas_btrfs::name::compare_names(&a.name, &b.name));
            v.into_iter().map(|e| Snap { path: root.join(&e.name), uuid: e.info.as_ref().unwrap().uuid.clone().unwrap(), known: Default::default() }).collect()
        })
        .collect();
    let t = Instant::now();
    let r = h.measure_space(&lines, true, &|| false, &|l| println!("{l}"), &|_, _| {}).unwrap();
    println!("space measured in {:?}", t.elapsed());
    let stop = || false;
    let lim = Limits { max_ranges: 0, max_extents: 1 << 23, stop: &stop, progress: &|_| {} };
    for (line, m) in lines.iter_mut().zip(&r) {
        let sets: Vec<Vec<u64>> = line
            .iter()
            .map(|s| {
                let f = rynas_btrfs::ioctl::open_dir(&s.path).unwrap();
                let id = rynas_btrfs::ioctl::subvol_info(&f).unwrap().root_id;
                extents::distinct_extents(&f, id, &lim).unwrap()
            })
            .collect();
        let bytes = |v: &mut dyn Iterator<Item = &u64>| v.map(|&k| extents::unpack(k).1).sum::<u64>();
        let mut count: std::collections::BTreeMap<u64, usize> = Default::default();
        for s in &sets {
            for &k in s {
                *count.entry(k).or_default() += 1;
            }
        }
        assert_eq!(m.union, bytes(&mut count.keys()), "{}", line[0].path.display());
        for (i, s) in sets.iter().enumerate() {
            let alone = bytes(&mut s.iter().filter(|k| count[k] == 1));
            assert_eq!(m.known[i].distinct, Some(bytes(&mut s.iter())), "{}", line[i].path.display());
            assert_eq!(m.known[i].exclusive.as_ref().map(|x| x.bytes), Some(alone), "{}", line[i].path.display());
        }
        for (s, k) in line.iter_mut().zip(&m.known) {
            s.known = k.clone();
        }
    }
    let again = h.measure_space(&lines, false, &|| false, &|_| {}, &|_, _| {}).unwrap();
    assert!(again.iter().zip(&r).all(|(a, b)| a.pairs == 0 && a.walks == 0 && a.union == b.union));

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
