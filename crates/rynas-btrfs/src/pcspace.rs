//! The space the received snapshots of a PC take, and what each one holds alone (Backup ›
//! PCs, "Measure space"; design and measurements in `docs/research/pc-backup.md` §11.12).
//!
//! `btrfs filesystem du` answers by reading every file of every snapshot and asking the
//! kernel, extent by extent, whether it is shared (a back-reference walk each): on a J4125
//! with disks one `@home` snapshot of 3.3 million files takes more than 10 minutes, and a
//! folder of 21 snapshots did not finish in an hour. Here the snapshots of one **line** (the
//! same subvolume of the PC over time, oldest first: received incrementally, each one a
//! snapshot of the one before) are measured by what changed between neighbours, and every
//! number is kept per snapshot, so a measure only works on the pairs of neighbours that are
//! new since the last one (a new backup: two pairs per line).
//!
//! The unit is the whole data extent, by its start (`disk_bytenr`) and on-disk size
//! (`disk_num_bytes`: compressed extents count what they take), as btrfs frees an extent
//! only when nothing references any part of it. For neighbours `A` (older) and `B`:
//!
//! - **candidates**: the extents of `B` at places (inode, offset) where `A` has another
//!   extent, and the other way round: [`crate::freed`]'s comparisons (tree blocks written
//!   since `B` was made from `A`, else a no-data send), or, when they cannot be used, the
//!   difference of the two full extent lists;
//! - **references**: `BTRFS_IOC_LOGICAL_INO_V2` lists the subvolumes referencing each
//!   candidate, across the whole filesystem;
//! - `born(B)` = candidates of `B` that `A` does not reference, `died(A)` = candidates of `A`
//!   that `B` does not reference, `exclusive(B)` = candidates of `B` referenced by `B` alone
//!   (for the oldest snapshot, from its candidates against the newer one). An extent only
//!   `B` references sits at some place where `A` has something else, so it is a candidate:
//!   [`crate::freed`] has the full argument.
//!
//! An extent lives in a contiguous run of the line (a receive clones only from its parent),
//! so with `distinct(S)` the bytes of the distinct extents `S` references:
//!
//! - `distinct(B) = distinct(A) + born(B) - died(A)`;
//! - the line takes `distinct(newest) + Σ died(S)` over the other snapshots (every extent
//!   counted once, in the last snapshot that has it).
//!
//! `distinct` needs one full tree search of one snapshot the first time (an anchor); read-only
//! snapshots never change, so it is kept and later ones follow by the formula. Limits (shown
//! as "≈"): the metadata (tree blocks, inline data of small files) is not counted; data a line
//! shares with another line counts in both; a lone snapshot's own data is its whole size.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::extents::{self, Limits, WalkError};
use crate::freed::{self, Member, Opened};

/// A number about a snapshot and one of its neighbours (by subvolume UUID).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub uuid: String,
    pub bytes: u64,
}

/// What only one snapshot references, read while it had these neighbours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excl {
    pub bytes: u64,
    #[serde(default)]
    pub older: Option<String>,
    #[serde(default)]
    pub newer: Option<String>,
}

/// What earlier measures found out about one snapshot (kept by the caller per UUID).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Known {
    /// Bytes of the distinct extents it references (a read-only snapshot never changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct: Option<u64>,
    /// What it references and its older neighbour does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub born: Option<Edge>,
    /// What it references and its newer neighbour does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub died: Option<Edge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusive: Option<Excl>,
}

impl Known {
    fn exclusive_for(&self, older: Option<&str>, newer: Option<&str>) -> bool {
        self.exclusive.as_ref().is_some_and(|x| x.older.as_deref() == older && x.newer.as_deref() == newer)
    }
}

/// One snapshot of a line (oldest first).
#[derive(Debug, Clone)]
pub struct Snap {
    pub path: PathBuf,
    pub uuid: String,
    pub known: Known,
}

/// The comparison of two neighbours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PairFacts {
    pub born: u64,
    pub died: u64,
    /// Referenced by the newer one alone / by the older one alone.
    pub excl_newer: u64,
    pub excl_older: u64,
    /// `distinct` of the older and the newer, when they were compared by full walks.
    pub distinct: Option<(u64, u64)>,
}

/// Where the numbers come from (the volume, or a test's sets).
pub trait Source {
    fn pair(&mut self, older: usize, newer: usize) -> Result<PairFacts, WalkError>;
    /// One full tree search: `distinct` of snapshot `i`.
    fn distinct(&mut self, i: usize) -> Result<u64, WalkError>;
}

/// A measured line: every snapshot's [`Known`] filled in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineSpace {
    pub known: Vec<Known>,
    /// What the line takes on the disk (each extent once).
    pub union: u64,
    /// Pairs compared and full tree searches made by this measure.
    pub pairs: u32,
    pub walks: u32,
}

/// The pairs `(newer - 1, newer)` a measure compares (by the newer index): those whose
/// numbers are missing or were read with other neighbours.
pub fn pairs_needed(line: &[Snap]) -> Vec<usize> {
    let id = |i: usize| line[i].uuid.as_str();
    let edge = |e: &Option<Edge>| e.as_ref().map(|e| e.uuid.clone());
    (1..line.len())
        .filter(|&b| {
            let a = b - 1;
            edge(&line[b].known.born).as_deref() != Some(id(a))
                || edge(&line[a].known.died).as_deref() != Some(id(b))
                || !line[b].known.exclusive_for(Some(id(a)), line.get(b + 1).map(|s| s.uuid.as_str()))
                || (a == 0 && !line[0].known.exclusive_for(None, Some(id(1))))
        })
        .collect()
}

/// Whether measuring `line` needs a full tree search (nothing known of any snapshot's
/// `distinct`).
pub fn needs_walk(line: &[Snap]) -> bool {
    !line.is_empty() && line.iter().all(|s| s.known.distinct.is_none())
}

/// What a measure is about to do: for progress and for timing each kind of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Compare pair `done + 1` of `total` (by the newer snapshot's index).
    Pair { done: usize, total: usize, newer: usize },
    /// The full tree search of snapshot `index` (the anchor of the line).
    Walk { index: usize },
}

/// Measures one line with `src`; `walk` false refuses full tree searches
/// ([`WalkError::Unsupported`]); `step` before each pair and before the full search.
pub fn measure_with(line: &[Snap], src: &mut dyn Source, walk: bool, step: &mut dyn FnMut(Step)) -> Result<LineSpace, WalkError> {
    let n = line.len();
    let mut out = LineSpace { known: line.iter().map(|s| s.known.clone()).collect(), ..LineSpace::default() };
    if n == 0 {
        return Ok(out);
    }
    if !walk && needs_walk(line) {
        return Err(WalkError::Unsupported("nothing measured yet: a full measure is needed".into()));
    }
    let id = |i: usize| line[i].uuid.clone();
    let todo = pairs_needed(line);
    for (j, &b) in todo.iter().enumerate() {
        step(Step::Pair { done: j, total: todo.len(), newer: b });
        let a = b - 1;
        let f = src.pair(a, b)?;
        out.pairs += 1;
        let k = &mut out.known;
        k[b].born = Some(Edge { uuid: id(a), bytes: f.born });
        k[a].died = Some(Edge { uuid: id(b), bytes: f.died });
        k[b].exclusive = Some(Excl { bytes: f.excl_newer, older: Some(id(a)), newer: (b + 1 < n).then(|| id(b + 1)) });
        if a == 0 {
            k[0].exclusive = Some(Excl { bytes: f.excl_older, older: None, newer: Some(id(1)) });
        }
        if let Some((da, db)) = f.distinct {
            k[a].distinct = Some(da);
            k[b].distinct = Some(db);
            out.walks += 2;
        }
    }
    if out.known.iter().all(|k| k.distinct.is_none()) {
        // the newest stays longest: the anchor of the next measures
        step(Step::Walk { index: n - 1 });
        out.known[n - 1].distinct = Some(src.distinct(n - 1)?);
        out.walks += 1;
    }
    let k = &mut out.known;
    if n == 1 && !k[0].exclusive_for(None, None) {
        k[0].exclusive = Some(Excl { bytes: k[0].distinct.unwrap_or(0), older: None, newer: None });
    }
    let bytes = |e: &Option<Edge>| e.as_ref().map_or(0, |e| e.bytes);
    let first = k.iter().position(|x| x.distinct.is_some()).unwrap_or(0);
    for i in first + 1..n {
        if k[i].distinct.is_none() {
            let prev = k[i - 1].distinct.unwrap_or(0);
            k[i].distinct = Some(prev.saturating_add(bytes(&k[i].born)).saturating_sub(bytes(&k[i - 1].died)));
        }
    }
    for i in (0..first).rev() {
        let next = k[i + 1].distinct.unwrap_or(0);
        k[i].distinct = Some(next.saturating_add(bytes(&k[i].died)).saturating_sub(bytes(&k[i + 1].born)));
    }
    out.union = k[n - 1].distinct.unwrap_or(0).saturating_add(k[..n - 1].iter().map(|x| bytes(&x.died)).sum::<u64>());
    Ok(out)
}

/// The volume as a [`Source`]: the opened snapshots of one line.
struct Tree<'a> {
    line: Vec<Opened>,
    facts: Vec<Member>,
    lim: &'a Limits<'a>,
    walk: bool,
}

/// `(bytenr, len)` of the extents in `a` and not in `b` (both sorted packed lists).
fn minus(a: &[u64], b: &[u64], out: &mut BTreeMap<u64, u64>) {
    let mut j = 0;
    for &k in a {
        while j < b.len() && b[j] < k {
            j += 1;
        }
        if b.get(j) != Some(&k) {
            let (bytenr, len) = extents::unpack(k);
            out.insert(bytenr, len);
        }
    }
}

impl Tree<'_> {
    /// Candidates of the newer (`cb`) and of the older (`ca`) by the comparisons.
    fn by_diff(&self, a: usize, b: usize, cb: &mut BTreeMap<u64, u64>, ca: &mut BTreeMap<u64, u64>) -> Result<(), WalkError> {
        let (Some(db), Some(da)) = (freed::older_diff(&self.facts, b), freed::send_diff(&self.facts, a, b)) else {
            return Err(WalkError::Unsupported(format!("{}: not read-only", self.line[b].path.display())));
        };
        freed::diff_extents(&self.line, db, self.lim, cb)?;
        freed::diff_extents(&self.line, da, self.lim, ca)
    }
}

impl Source for Tree<'_> {
    fn pair(&mut self, a: usize, b: usize) -> Result<PairFacts, WalkError> {
        let (mut cb, mut ca) = (BTreeMap::new(), BTreeMap::new());
        let mut f = PairFacts::default();
        match self.by_diff(a, b, &mut cb, &mut ca) {
            Ok(()) => {}
            Err(WalkError::Unsupported(e)) if self.walk => {
                (self.lim.progress)(&format!("{e}; comparing the two snapshots by full walks instead"));
                let (oa, ob) = (&self.line[a], &self.line[b]);
                let sa = extents::distinct_extents(&oa.file, oa.root, self.lim)?;
                let sb = extents::distinct_extents(&ob.file, ob.root, self.lim)?;
                cb.clear();
                ca.clear();
                minus(&sb, &sa, &mut cb);
                minus(&sa, &sb, &mut ca);
                let sum = |s: &[u64]| s.iter().map(|&k| extents::unpack(k).1).sum::<u64>();
                f.distinct = Some((sum(&sa), sum(&sb)));
            }
            Err(e) => return Err(e),
        }
        let (ra, rb) = (self.line[a].root, self.line[b].root);
        let fd = &self.line[a].file;
        let mut roots: BTreeMap<u64, Option<BTreeSet<u64>>> = BTreeMap::new();
        for (i, &bytenr) in cb.keys().chain(ca.keys()).enumerate() {
            if i % 256 == 0 && (self.lim.stop)() {
                return Err(WalkError::Stopped);
            }
            if let std::collections::btree_map::Entry::Vacant(v) = roots.entry(bytenr) {
                v.insert(freed::extent_roots(fd, bytenr)?);
            }
        }
        let mut truncated = 0u64;
        for (side, cands) in [(true, &cb), (false, &ca)] {
            let (me, other) = if side { (rb, ra) } else { (ra, rb) };
            for (bytenr, &len) in cands {
                // a list too long to read: referenced by many, so neither new nor gone here
                let Some(r) = roots.get(bytenr).and_then(Option::as_ref) else {
                    truncated += 1;
                    continue;
                };
                let mine = r.contains(&me) && !r.contains(&other);
                let alone = r.len() == 1 && r.contains(&me);
                match side {
                    true => {
                        f.born += if mine { len } else { 0 };
                        f.excl_newer += if alone { len } else { 0 };
                    }
                    false => {
                        f.died += if mine { len } else { 0 };
                        f.excl_older += if alone { len } else { 0 };
                    }
                }
            }
        }
        (self.lim.progress)(&format!(
            "{} → {}: {} + {} candidate extents, {} bytes new, {} bytes gone",
            self.line[a].path.display(),
            self.line[b].path.display(),
            ca.len(),
            cb.len(),
            f.born,
            f.died
        ));
        if truncated > 0 {
            (self.lim.progress)(&format!("{truncated} extents with too many references to list: counted as shared"));
        }
        Ok(f)
    }

    fn distinct(&mut self, i: usize) -> Result<u64, WalkError> {
        let o = &self.line[i];
        let s = extents::distinct_extents(&o.file, o.root, self.lim)?;
        let bytes = s.iter().map(|&k| extents::unpack(k).1).sum();
        (self.lim.progress)(&format!("{}: {} distinct extents, {bytes} bytes (full tree search)", o.path.display(), s.len()));
        Ok(bytes)
    }
}

/// Measures one line of snapshots on the volume (root privileges; blocking).
pub fn measure_line(line: &[Snap], lim: &Limits, walk: bool, step: &mut dyn FnMut(Step)) -> Result<LineSpace, WalkError> {
    let opened: Vec<Opened> = line.iter().map(|s| freed::open(&s.path, false)).collect::<Result<_, _>>()?;
    let facts = opened.iter().map(|o| o.facts.clone()).collect();
    let mut src = Tree { line: opened, facts, lim, walk };
    measure_with(line, &mut src, walk, step)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshots as sets of `(bytenr, len)`; extents in `outside` are referenced elsewhere too.
    struct Sets {
        sets: Vec<BTreeSet<(u64, u64)>>,
        outside: BTreeSet<(u64, u64)>,
        pairs: Vec<(usize, usize)>,
        walks: Vec<usize>,
    }

    fn sum<'a>(it: impl Iterator<Item = &'a (u64, u64)>) -> u64 {
        it.map(|e| e.1).sum()
    }

    impl Source for Sets {
        fn pair(&mut self, a: usize, b: usize) -> Result<PairFacts, WalkError> {
            self.pairs.push((a, b));
            let alone = |i: usize, e: &(u64, u64)| !self.outside.contains(e) && self.sets.iter().enumerate().all(|(j, s)| j == i || !s.contains(e));
            let (sa, sb) = (&self.sets[a], &self.sets[b]);
            Ok(PairFacts {
                born: sum(sb.difference(sa)),
                died: sum(sa.difference(sb)),
                excl_newer: sum(sb.iter().filter(|e| alone(b, e))),
                excl_older: sum(sa.iter().filter(|e| alone(a, e))),
                distinct: None,
            })
        }

        fn distinct(&mut self, i: usize) -> Result<u64, WalkError> {
            self.walks.push(i);
            Ok(sum(self.sets[i].iter()))
        }
    }

    fn line(names: &[&str], known: &BTreeMap<String, Known>) -> Vec<Snap> {
        names.iter().map(|n| Snap { path: PathBuf::from(n), uuid: n.to_string(), known: known.get(*n).cloned().unwrap_or_default() }).collect()
    }

    fn run(src: &mut Sets, names: &[&str], known: &mut BTreeMap<String, Known>, walk: bool) -> Result<LineSpace, WalkError> {
        let l = line(names, known);
        let r = measure_with(&l, src, walk, &mut |_| {})?;
        known.clear();
        for (s, k) in l.iter().zip(&r.known) {
            known.insert(s.uuid.clone(), k.clone());
        }
        Ok(r)
    }

    /// The true numbers: (union, distinct per snapshot, exclusive per snapshot).
    fn truth(src: &Sets) -> (u64, Vec<u64>, Vec<u64>) {
        let all: BTreeSet<(u64, u64)> = src.sets.iter().flatten().copied().collect();
        let excl = (0..src.sets.len())
            .map(|i| sum(src.sets[i].iter().filter(|e| !src.outside.contains(*e) && src.sets.iter().enumerate().all(|(j, s)| j == i || !s.contains(*e)))))
            .collect();
        (sum(all.iter()), src.sets.iter().map(|s| sum(s.iter())).collect(), excl)
    }

    fn check(r: &LineSpace, src: &Sets) {
        let (union, distinct, excl) = truth(src);
        assert_eq!(r.union, union);
        assert_eq!(r.known.iter().map(|k| k.distinct.unwrap()).collect::<Vec<_>>(), distinct);
        assert_eq!(r.known.iter().map(|k| k.exclusive.as_ref().unwrap().bytes).collect::<Vec<_>>(), excl);
    }

    /// A line where each snapshot drops some extents of the one before and adds its own.
    fn history(n: usize) -> Vec<BTreeSet<(u64, u64)>> {
        let mut out = Vec::new();
        let mut cur: BTreeSet<(u64, u64)> = (0..20).map(|i| (i * 1000, 4096 * (i + 1))).collect();
        for s in 0..n as u64 {
            out.push(cur.clone());
            let gone: Vec<_> = cur.iter().copied().filter(|e| (e.0 / 1000 + s) % 4 == 0).collect();
            for g in gone {
                cur.remove(&g);
            }
            for j in 0..3 {
                cur.insert((100_000 + s * 10 + j, 8192 * (j + 1)));
            }
        }
        out
    }

    #[test]
    fn a_line_is_measured_once_then_only_where_it_changed() {
        let mut src = Sets { sets: history(5), outside: BTreeSet::new(), pairs: vec![], walks: vec![] };
        // an extent also reflinked elsewhere is never anyone's own
        let shared = *src.sets[4].iter().last().unwrap();
        src.outside.insert(shared);
        let mut known = BTreeMap::new();
        let names = ["s0", "s1", "s2", "s3", "s4"];
        // nothing known: an automatic measure refuses the full search
        assert!(matches!(run(&mut src, &names, &mut known, false), Err(WalkError::Unsupported(_))));
        let r = run(&mut src, &names, &mut known, true).unwrap();
        check(&r, &src);
        assert_eq!((src.pairs.len(), src.walks.clone()), (4, vec![4]));
        // nothing changed: nothing to compare, nothing to walk
        src.pairs.clear();
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert!(src.pairs.is_empty() && r.pairs == 0 && r.walks == 0);
        // a new backup: the new pair, and the previous newest's own data again
        let next = history(6).pop().unwrap();
        src.sets.push(next);
        let names = ["s0", "s1", "s2", "s3", "s4", "s5"];
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert_eq!(src.pairs, [(3, 4), (4, 5)]);
        // the oldest went: only the new oldest is compared again
        src.pairs.clear();
        src.sets.remove(0);
        let names = ["s1", "s2", "s3", "s4", "s5"];
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert_eq!(src.pairs, [(0, 1)]);
        // one in the middle went: the new pair, and the older neighbour's own data again
        src.pairs.clear();
        src.sets.remove(2);
        let names = ["s1", "s2", "s4", "s5"];
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert_eq!(src.pairs, [(0, 1), (1, 2)]);
        // the anchor of the first measure (s4) went: the numbers kept still hold
        src.pairs.clear();
        src.sets.remove(2);
        let names = ["s1", "s2", "s5"];
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert_eq!(src.pairs, [(0, 1), (1, 2)]);
        // one full search in all
        assert_eq!(src.walks, [4]);
    }

    #[test]
    fn steps_come_before_each_pair_and_the_full_search() {
        let mut src = Sets { sets: history(3), outside: BTreeSet::new(), pairs: vec![], walks: vec![] };
        let l = line(&["a", "b", "c"], &BTreeMap::new());
        let mut steps = Vec::new();
        measure_with(&l, &mut src, true, &mut |s| steps.push(s)).unwrap();
        assert_eq!(steps, [Step::Pair { done: 0, total: 2, newer: 1 }, Step::Pair { done: 1, total: 2, newer: 2 }, Step::Walk { index: 2 }]);
    }

    #[test]
    fn a_lone_snapshot_is_walked_once() {
        let mut src = Sets { sets: vec![[(4096, 8192), (65536, 4096)].into_iter().collect()], outside: BTreeSet::new(), pairs: vec![], walks: vec![] };
        let mut known = BTreeMap::new();
        let r = run(&mut src, &["only"], &mut known, true).unwrap();
        assert_eq!((r.union, r.known[0].exclusive.as_ref().unwrap().bytes), (12288, 12288));
        let r = run(&mut src, &["only"], &mut known, false).unwrap();
        assert_eq!((r.union, r.walks), (12288, 0));
        assert_eq!(src.walks, [0]);
    }

    #[test]
    fn set_differences_of_packed_lists() {
        let p = |b: u64, l: u64| (b >> 12) << 16 | (l >> 12);
        let (a, b) = (vec![p(4096, 4096), p(8192, 8192), p(1 << 20, 4096)], vec![p(8192, 8192)]);
        let mut out = BTreeMap::new();
        minus(&a, &b, &mut out);
        assert_eq!(out.into_iter().collect::<Vec<_>>(), [(4096, 4096), (1 << 20, 4096)]);
        let mut out = BTreeMap::new();
        minus(&b, &a, &mut out);
        assert!(out.is_empty());
    }
}
