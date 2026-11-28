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
//!
//! **Checkpoints.** One pair can take long (the `LOGICAL_INO_V2` of each candidate is a
//! back-reference walk: a pair of 228 802 candidates took more than 10 minutes on a J4125 with
//! disks), so a measure keeps its progress in a [`Checkpoint`] the caller writes down
//! ([`Hooks::save`]): every finished pair at once ([`PairDone`], by a [`PairKey`]: both
//! snapshots' UUIDs and generations and their own neighbours), and inside a pair a
//! [`PairCursor`] (how many candidates, in ascending start, are counted, with their sums and a
//! fingerprint of the candidate list) at most every [`SAVE_EVERY`] and when the pair stops. A
//! later measure reuses a finished pair as it is and goes on in a stopped one from its cursor
//! when the candidates are the same list (else it counts that pair again): the numbers are
//! those of a measure never stopped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairFacts {
    pub born: u64,
    pub died: u64,
    /// Referenced by the newer one alone / by the older one alone.
    pub excl_newer: u64,
    pub excl_older: u64,
    /// `distinct` of the older and the newer, when they were compared by full walks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct: Option<(u64, u64)>,
}

/// Where the numbers come from (the volume, or a test's sets).
pub trait Source {
    fn pair(&mut self, older: usize, newer: usize) -> Result<PairFacts, WalkError>;
    /// One full tree search: `distinct` of snapshot `i`.
    fn distinct(&mut self, i: usize) -> Result<u64, WalkError>;
    /// The generation of snapshot `i` (0: not known). A snapshot whose generation changed is
    /// not the one a checkpoint saw.
    fn generation(&self, _i: usize) -> u64 {
        0
    }
    /// [`Source::pair`] going on from `from` (a cursor of an earlier, stopped comparison of
    /// the same pair; ignored when the candidates are not the same list), calling `at` with a
    /// consistent cursor every few hundred candidates and right before it stops. By default
    /// the pair is compared whole.
    fn pair_from(&mut self, older: usize, newer: usize, _from: Option<&PairCursor>, _at: &mut dyn FnMut(&PairCursor)) -> Result<PairFacts, WalkError> {
        self.pair(older, newer)
    }
}

/// How often at most a cursor inside a pair is handed to [`Hooks::save`] (a finished pair and
/// a stop always are).
pub const SAVE_EVERY: Duration = Duration::from_secs(30);

/// How long a cursor inside a pair stays usable: its sums come from reference lists read
/// then, and a count spread over weeks would add up moments far apart.
pub const CURSOR_MAX_AGE: Duration = Duration::from_secs(7 * 86_400);

/// A pair of neighbours as a checkpoint knows it: both snapshots with their generations. What
/// each of the two holds alone also depends on their own neighbours: the cursor and the facts
/// say which ones they were read with ([`Sides`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairKey {
    pub older: String,
    pub newer: String,
    /// Of the older and the newer.
    #[serde(default)]
    pub generations: (u64, u64),
}

/// The neighbours what each snapshot of a pair holds alone was read with: `before` the older
/// one's older neighbour, `after` the newer one's newer neighbour. A count that went on after
/// one of them changed has that side's sum mixed (`stale_*`): born and died stay exact (they
/// depend on the two snapshots only), that side's "alone" is not used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale_older: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale_newer: bool,
}

impl Sides {
    /// These sides as seen with the neighbours `now` (`before`, `after`): a side whose
    /// neighbour changed is stale from now on.
    fn seen_with(&self, now: &(Option<String>, Option<String>)) -> Sides {
        Sides {
            before: now.0.clone(),
            after: now.1.clone(),
            stale_older: self.stale_older || self.before != now.0,
            stale_newer: self.stale_newer || self.after != now.1,
        }
    }
}

/// Where the counting of one pair stands: the first `done` of its `total` candidate extents
/// (ascending start) are counted in the sums.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairCursor {
    pub total: u64,
    pub done: u64,
    /// Of the candidate list: the count goes on only over the same list.
    pub fingerprint: u64,
    pub born: u64,
    pub died: u64,
    pub excl_newer: u64,
    pub excl_older: u64,
    /// Candidates with too many references to list (counted as shared).
    #[serde(default)]
    pub truncated: u64,
    /// The neighbours the "alone" sums were counted with.
    #[serde(default, flatten)]
    pub sides: Sides,
}

/// A finished pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairDone {
    pub key: PairKey,
    pub facts: PairFacts,
    #[serde(default, flatten)]
    pub sides: Sides,
}

/// A pair stopped part way, and when (ms since the epoch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairPartial {
    pub key: PairKey,
    pub cursor: PairCursor,
    #[serde(default)]
    pub at_ms: u64,
}

/// The progress of measures not finished yet (one per volume or folder, kept by the caller):
/// the pairs they finished and the pairs they stopped in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pairs: Vec<PairDone>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub partial: Vec<PairPartial>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Checkpoint {
    /// Most finished pairs kept (about 300 bytes each in JSON); the oldest go first.
    pub const MAX_PAIRS: usize = 2048;
    /// Most stopped pairs kept.
    pub const MAX_PARTIAL: usize = 16;

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty() && self.partial.is_empty()
    }

    pub fn facts(&self, key: &PairKey) -> Option<PairFacts> {
        self.done(key).map(|p| p.facts)
    }

    fn done(&self, key: &PairKey) -> Option<&PairDone> {
        self.pairs.iter().find(|p| &p.key == key)
    }

    pub fn cursor(&self, key: &PairKey) -> Option<&PairCursor> {
        self.partial.iter().find(|p| &p.key == key).map(|p| &p.cursor)
    }

    fn put_facts(&mut self, key: PairKey, facts: PairFacts, sides: Sides) {
        self.drop_pair(&key);
        self.pairs.push(PairDone { key, facts, sides });
        let over = self.pairs.len().saturating_sub(Self::MAX_PAIRS);
        self.pairs.drain(..over);
    }

    fn put_cursor(&mut self, key: PairKey, cursor: PairCursor, at_ms: u64) {
        self.partial.retain(|p| p.key != key);
        self.partial.push(PairPartial { key, cursor, at_ms });
        let over = self.partial.len().saturating_sub(Self::MAX_PARTIAL);
        self.partial.drain(..over);
    }

    fn drop_pair(&mut self, key: &PairKey) {
        self.partial.retain(|p| &p.key != key);
        self.pairs.retain(|p| &p.key != key);
    }

    /// Drops the cursors saved before `now_ms - max_age`.
    pub fn expire(&mut self, now_ms: u64, max_age: Duration) {
        let oldest = now_ms.saturating_sub(max_age.as_millis() as u64);
        self.partial.retain(|p| p.at_ms >= oldest);
    }

    /// Keeps only what describes snapshots there now (`now`: UUID → generation): both
    /// snapshots of the pair present with the generations they had.
    pub fn retain_present(&mut self, now: &BTreeMap<String, u64>) {
        let ok = |k: &PairKey| now.get(&k.older) == Some(&k.generations.0) && now.get(&k.newer) == Some(&k.generations.1);
        self.pairs.retain(|p| ok(&p.key));
        self.partial.retain(|p| ok(&p.key));
    }

    /// Forgets the pairs of the snapshots `uuids` (a line whose numbers are now kept with its
    /// snapshots).
    pub fn forget(&mut self, uuids: &BTreeSet<&str>) {
        let mine = |k: &PairKey| uuids.contains(k.older.as_str()) || uuids.contains(k.newer.as_str());
        self.pairs.retain(|p| !mine(&p.key));
        self.partial.retain(|p| !mine(&p.key));
    }
}

/// What a measure tells its caller while it works (every method optional).
pub trait Hooks {
    /// Before each pair and before the full search.
    fn step(&mut self, _s: Step) {}
    /// Pair `newer` came whole from the checkpoint: nothing read.
    fn reused(&mut self, _newer: usize) {}
    /// Inside pair `newer`: `done` of its `total` candidate extents counted, `from` of them
    /// already before this measure.
    fn extents(&mut self, _newer: usize, _done: u64, _total: u64, _from: u64) {}
    /// The checkpoint changed in a way worth keeping: write it down (the caller may write
    /// less often; what it holds when the measure returns is the state to keep).
    fn save(&mut self, _cp: &Checkpoint) {}
}

/// [`Hooks`] that only pass the steps on.
struct StepOnly<'a>(&'a mut dyn FnMut(Step));

impl Hooks for StepOnly<'_> {
    fn step(&mut self, s: Step) {
        (self.0)(s)
    }
}

/// The key of the pair `(newer - 1, newer)` of `line` (`generations` by index; missing: 0).
pub fn pair_key(line: &[Snap], generations: &[u64], newer: usize) -> PairKey {
    let (a, b) = (newer - 1, newer);
    let g = |i: usize| generations.get(i).copied().unwrap_or(0);
    PairKey { older: line[a].uuid.clone(), newer: line[b].uuid.clone(), generations: (g(a), g(b)) }
}

/// The neighbours of the pair ending at `newer` now: the older one's older, the newer one's
/// newer.
fn neighbours(line: &[Snap], newer: usize) -> (Option<String>, Option<String>) {
    ((newer - 1).checked_sub(1).map(|i| line[i].uuid.clone()), line.get(newer + 1).map(|s| s.uuid.clone()))
}

/// A fingerprint (FNV-1a) of a candidate list.
fn fingerprint(all: &BTreeMap<u64, (Option<u64>, Option<u64>)>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for (&k, &(n, o)) in all {
        for w in [k, n.unwrap_or(u64::MAX), o.unwrap_or(u64::MAX)] {
            for byte in w.to_le_bytes() {
                h ^= u64::from(byte);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    h
}

/// Counts the candidates of one pair: `all` maps each candidate's start to its on-disk size as
/// a candidate of the newer (root `rb`) and of the older (root `ra`); `roots` lists the
/// subvolumes referencing a start (`None`: too many to list, counted as shared). Goes on from
/// `from` when it was made over the same list; `at` gets the cursor every 256 candidates and
/// right before a stop.
#[allow(clippy::too_many_arguments)]
pub fn count_candidates(
    all: &BTreeMap<u64, (Option<u64>, Option<u64>)>,
    from: Option<&PairCursor>,
    (ra, rb): (u64, u64),
    roots: &mut dyn FnMut(u64) -> Result<Option<BTreeSet<u64>>, WalkError>,
    stop: &dyn Fn() -> bool,
    at: &mut dyn FnMut(&PairCursor),
    progress: &dyn Fn(&str),
) -> Result<PairCursor, WalkError> {
    let (total, fp) = (all.len() as u64, fingerprint(all));
    let mut c = match from {
        Some(c) if c.total == total && c.fingerprint == fp && c.done <= total => c.clone(),
        Some(c) => {
            progress(&format!("the candidate extents are not those of the saved point ({} then, {total} now): counted again from the start", c.total));
            PairCursor { total, fingerprint: fp, ..PairCursor::default() }
        }
        None => PairCursor { total, fingerprint: fp, ..PairCursor::default() },
    };
    if c.done > 0 {
        progress(&format!("going on from {} of {total} candidate extents", c.done));
    }
    for (&bytenr, &(newer, older)) in all.iter().skip(c.done as usize) {
        if c.done % 256 == 0 {
            at(&c);
            if stop() {
                return Err(WalkError::Stopped);
            }
        }
        let r = roots(bytenr)?;
        for (len, me, other, newer_side) in [(newer, rb, ra, true), (older, ra, rb, false)] {
            let Some(len) = len else { continue };
            // a list too long to read: referenced by many, so neither new nor gone here
            let Some(r) = r.as_ref() else {
                c.truncated += 1;
                continue;
            };
            let mine = if r.contains(&me) && !r.contains(&other) { len } else { 0 };
            let alone = if r.len() == 1 && r.contains(&me) { len } else { 0 };
            if newer_side {
                c.born += mine;
                c.excl_newer += alone;
            } else {
                c.died += mine;
                c.excl_older += alone;
            }
        }
        c.done += 1;
    }
    at(&c);
    Ok(c)
}

/// The candidate list of [`count_candidates`] from the newer's (`cb`) and the older's (`ca`).
pub fn candidate_list(cb: &BTreeMap<u64, u64>, ca: &BTreeMap<u64, u64>) -> BTreeMap<u64, (Option<u64>, Option<u64>)> {
    let mut all: BTreeMap<u64, (Option<u64>, Option<u64>)> = BTreeMap::new();
    for (&k, &l) in cb {
        all.entry(k).or_default().0 = Some(l);
    }
    for (&k, &l) in ca {
        all.entry(k).or_default().1 = Some(l);
    }
    all
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
    /// Pairs taken whole from the checkpoint.
    pub reused: u32,
}

/// The pairs `(newer - 1, newer)` a measure compares (by the newer index): those whose
/// numbers are missing or were read with other neighbours.
pub fn pairs_needed(line: &[Snap]) -> Vec<usize> {
    let n = line.len();
    let id = |i: usize| line[i].uuid.as_str();
    let edge = |e: &Option<Edge>| e.as_ref().map(|e| e.uuid.clone());
    // born and died need their own pair; a comparison also reads what each of its two
    // snapshots holds alone, so a snapshot whose own data is out of date only needs one of
    // its two pairs: one already compared, else the one with its newer neighbour (the newest
    // pair is the one most likely compared anyway when the next snapshot comes)
    let mut need: BTreeSet<usize> = (1..n)
        .filter(|&b| edge(&line[b].known.born).as_deref() != Some(id(b - 1)) || edge(&line[b - 1].known.died).as_deref() != Some(id(b)))
        .collect();
    if n >= 2 {
        for i in 0..n {
            if exclusive_current(line, &line[i].known, i) || need.contains(&i) || need.contains(&(i + 1)) {
                continue;
            }
            need.insert(if i + 1 < n { i + 1 } else { i });
        }
    }
    need.into_iter().collect()
}

/// Whether `k`'s own data was read with the neighbours snapshot `i` of `line` has now.
fn exclusive_current(line: &[Snap], k: &Known, i: usize) -> bool {
    k.exclusive_for(i.checked_sub(1).map(|j| line[j].uuid.as_str()), line.get(i + 1).map(|s| s.uuid.as_str()))
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
    measure_with_opts(line, src, walk, true, step)
}

/// [`measure_with`]; `anchor` false never makes the full tree search of the anchor: the pairs
/// alone (their `born`, `died` and `exclusive`), for a line whose newest snapshot is followed
/// by a live subvolume that keeps sharing its data (what removing the line frees is then the
/// `died` of every snapshot but the newest plus the newest's `exclusive`, no `distinct`
/// needed). Without any `distinct` known, `distinct` stays unknown and `union` is the sum of
/// the `died` of every snapshot but the newest.
pub fn measure_with_opts(line: &[Snap], src: &mut dyn Source, walk: bool, anchor: bool, step: &mut dyn FnMut(Step)) -> Result<LineSpace, WalkError> {
    measure_core(line, src, walk, anchor, &mut Checkpoint::default(), &mut StepOnly(step), SAVE_EVERY)
}

/// [`measure_with_opts`] keeping its progress in `cp` (module text, "Checkpoints"): pairs
/// finished there are taken as they are, a pair stopped there goes on from its cursor, and
/// `hooks` is told of every change worth writing down. On an error (a stop) `cp` holds what
/// was done.
pub fn measure_resumable(line: &[Snap], src: &mut dyn Source, walk: bool, anchor: bool, cp: &mut Checkpoint, hooks: &mut dyn Hooks) -> Result<LineSpace, WalkError> {
    measure_core(line, src, walk, anchor, cp, hooks, SAVE_EVERY)
}

/// One pair compared with its progress kept in `cp` (a cursor at most every `every`, and
/// when it stops; the facts when it ends). `nb`: the pair's neighbours now. Returns the facts
/// and the sides they were read with; a pair finished before is taken as it is.
#[allow(clippy::too_many_arguments)]
fn pair_resumable(src: &mut dyn Source, newer: usize, key: PairKey, nb: &(Option<String>, Option<String>), cp: &mut Checkpoint, hooks: &mut dyn Hooks, every: Duration) -> Result<(PairFacts, Sides, bool), WalkError> {
    if let Some(d) = cp.done(&key) {
        hooks.reused(newer);
        return Ok((d.facts, d.sides.seen_with(nb), true));
    }
    // a count going on after a neighbour changed: that side's "alone" is mixed from now on
    let from = cp.cursor(&key).map(|c| PairCursor { sides: c.sides.seen_with(nb), ..c.clone() });
    let mut saved = cp.cursor(&key).cloned();
    let mut last: Option<PairCursor> = None;
    let mut first: Option<u64> = None;
    let mut at = Instant::now();
    // a cursor of a fresh count (no `from`, or another candidate list) has fresh sides
    let fresh = Sides { before: nb.0.clone(), after: nb.1.clone(), ..Sides::default() };
    let sided = |c: &PairCursor| {
        let resumed = from.as_ref().is_some_and(|f| f.fingerprint == c.fingerprint && f.total == c.total && c.done >= f.done);
        PairCursor { sides: if resumed { from.as_ref().map(|f| f.sides.clone()).unwrap_or_default() } else { fresh.clone() }, ..c.clone() }
    };
    let r = src.pair_from(newer - 1, newer, from.as_ref(), &mut |c: &PairCursor| {
        let c = sided(c);
        let start = *first.get_or_insert(c.done);
        hooks.extents(newer, c.done, c.total, start);
        if c.done > 0 && saved.as_ref() != Some(&c) && at.elapsed() >= every {
            cp.put_cursor(key.clone(), c.clone(), now_ms());
            hooks.save(cp);
            saved = Some(c.clone());
            at = Instant::now();
        }
        last = Some(c);
    });
    match r {
        Ok(f) => {
            let sides = last.map(|c| c.sides).unwrap_or(fresh);
            cp.put_facts(key, f, sides.clone());
            hooks.save(cp);
            Ok((f, sides, false))
        }
        Err(e) => {
            if let Some(c) = last.filter(|c| c.done > 0 && saved.as_ref() != Some(c)) {
                cp.put_cursor(key, c, now_ms());
                hooks.save(cp);
            }
            Err(e)
        }
    }
}

fn measure_core(line: &[Snap], src: &mut dyn Source, walk: bool, anchor: bool, cp: &mut Checkpoint, hooks: &mut dyn Hooks, every: Duration) -> Result<LineSpace, WalkError> {
    let n = line.len();
    let mut out = LineSpace { known: line.iter().map(|s| s.known.clone()).collect(), ..LineSpace::default() };
    if n == 0 {
        return Ok(out);
    }
    if anchor && !walk && needs_walk(line) {
        return Err(WalkError::Unsupported("nothing measured yet: a full measure is needed".into()));
    }
    cp.expire(now_ms(), CURSOR_MAX_AGE);
    let id = |i: usize| line[i].uuid.clone();
    let todo = pairs_needed(line);
    let generations: Vec<u64> = (0..n).map(|i| src.generation(i)).collect();
    let mut total = todo.len();
    let mut queue: std::collections::VecDeque<usize> = todo.into_iter().collect();
    let mut j = 0;
    while let Some(b) = queue.pop_front() {
        hooks.step(Step::Pair { done: j, total, newer: b });
        j += 1;
        let a = b - 1;
        let nb = neighbours(line, b);
        let (f, sides, reused) = pair_resumable(src, b, pair_key(line, &generations, b), &nb, cp, hooks, every)?;
        if reused {
            out.reused += 1;
        } else {
            out.pairs += 1;
        }
        let k = &mut out.known;
        // born and died depend on the two snapshots only; what each holds alone counts only
        // when read with the neighbours it has now
        k[b].born = Some(Edge { uuid: id(a), bytes: f.born });
        k[a].died = Some(Edge { uuid: id(b), bytes: f.died });
        if !sides.stale_newer {
            k[b].exclusive = Some(Excl { bytes: f.excl_newer, older: Some(id(a)), newer: nb.1.clone() });
        }
        if !sides.stale_older {
            k[a].exclusive = Some(Excl { bytes: f.excl_older, older: nb.0.clone(), newer: Some(id(b)) });
        }
        if let Some((da, db)) = f.distinct {
            k[a].distinct = Some(da);
            k[b].distinct = Some(db);
            if !reused {
                out.walks += 2;
            }
        }
        if queue.is_empty() && n >= 2 {
            // a snapshot whose own data is still out of date (its pairs' counts were mixed
            // across a change of neighbours): one of its pairs again, counted afresh
            if let Some(i) = (0..n).find(|&i| !exclusive_current(line, &out.known[i], i)) {
                let p = if i + 1 < n { i + 1 } else { i };
                cp.drop_pair(&pair_key(line, &generations, p));
                queue.push_back(p);
                total += 1;
            }
        }
    }
    let bytes = |e: &Option<Edge>| e.as_ref().map_or(0, |e| e.bytes);
    if out.known.iter().all(|k| k.distinct.is_none()) {
        if !anchor {
            out.union = out.known[..n - 1].iter().map(|x| bytes(&x.died)).sum();
            return Ok(out);
        }
        // the newest stays longest: the anchor of the next measures
        hooks.step(Step::Walk { index: n - 1 });
        out.known[n - 1].distinct = Some(src.distinct(n - 1)?);
        out.walks += 1;
    }
    let k = &mut out.known;
    if n == 1 && !k[0].exclusive_for(None, None) {
        k[0].exclusive = Some(Excl { bytes: k[0].distinct.unwrap_or(0), older: None, newer: None });
    }
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
        self.pair_from(a, b, None, &mut |_| {})
    }

    fn generation(&self, i: usize) -> u64 {
        self.facts[i].generation
    }

    fn pair_from(&mut self, a: usize, b: usize, from: Option<&PairCursor>, at: &mut dyn FnMut(&PairCursor)) -> Result<PairFacts, WalkError> {
        let (mut cb, mut ca) = (BTreeMap::new(), BTreeMap::new());
        let mut distinct = None;
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
                distinct = Some((sum(&sa), sum(&sb)));
            }
            Err(e) => return Err(e),
        }
        let all = candidate_list(&cb, &ca);
        let (ra, rb) = (self.line[a].root, self.line[b].root);
        let fd = &self.line[a].file;
        let (pa, pb) = (self.line[a].path.display().to_string(), self.line[b].path.display().to_string());
        let progress = |m: &str| (self.lim.progress)(&format!("{pa} → {pb}: {m}"));
        let c = count_candidates(&all, from, (ra, rb), &mut |bytenr| freed::extent_roots(fd, bytenr), self.lim.stop, at, &progress)?;
        progress(&format!("{} + {} candidate extents, {} bytes new, {} bytes gone", ca.len(), cb.len(), c.born, c.died));
        if c.truncated > 0 {
            (self.lim.progress)(&format!("{} extents with too many references to list: counted as shared", c.truncated));
        }
        Ok(PairFacts { born: c.born, died: c.died, excl_newer: c.excl_newer, excl_older: c.excl_older, distinct })
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
    measure_line_opts(line, lim, walk, true, step)
}

/// [`measure_line`] with [`measure_with_opts`]'s `anchor`.
pub fn measure_line_opts(line: &[Snap], lim: &Limits, walk: bool, anchor: bool, step: &mut dyn FnMut(Step)) -> Result<LineSpace, WalkError> {
    let opened: Vec<Opened> = line.iter().map(|s| freed::open(&s.path, false)).collect::<Result<_, _>>()?;
    let facts = opened.iter().map(|o| o.facts.clone()).collect();
    let mut src = Tree { line: opened, facts, lim, walk };
    measure_with_opts(line, &mut src, walk, anchor, step)
}

/// [`measure_line_opts`] with [`measure_resumable`]'s checkpoint.
pub fn measure_line_resumable(line: &[Snap], lim: &Limits, walk: bool, anchor: bool, cp: &mut Checkpoint, hooks: &mut dyn Hooks) -> Result<LineSpace, WalkError> {
    let opened: Vec<Opened> = line.iter().map(|s| freed::open(&s.path, false)).collect::<Result<_, _>>()?;
    let facts = opened.iter().map(|o| o.facts.clone()).collect();
    let mut src = Tree { line: opened, facts, lim, walk };
    measure_resumable(line, &mut src, walk, anchor, cp, hooks)
}

/// Whether two neighbours can be compared without full walks: both read-only (the no-data
/// send of the older side needs it; the newer side's comparison falls back to the same send).
pub fn pair_cheap(older_readonly: bool, newer_readonly: bool) -> bool {
    older_readonly && newer_readonly
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
        // a new backup: the new pair only (it also reads what the previous newest holds alone)
        let next = history(6).pop().unwrap();
        src.sets.push(next);
        let names = ["s0", "s1", "s2", "s3", "s4", "s5"];
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert_eq!(src.pairs, [(4, 5)]);
        // the oldest went: only the new oldest is compared again
        src.pairs.clear();
        src.sets.remove(0);
        let names = ["s1", "s2", "s3", "s4", "s5"];
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert_eq!(src.pairs, [(0, 1)]);
        // one in the middle went: the new pair, which reads its two snapshots' own data again
        src.pairs.clear();
        src.sets.remove(2);
        let names = ["s1", "s2", "s4", "s5"];
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert_eq!(src.pairs, [(1, 2)]);
        // the anchor of the first measure (s4) went: the numbers kept still hold
        src.pairs.clear();
        src.sets.remove(2);
        let names = ["s1", "s2", "s5"];
        let r = run(&mut src, &names, &mut known, false).unwrap();
        check(&r, &src);
        assert_eq!(src.pairs, [(1, 2)]);
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
    fn without_an_anchor_only_the_pairs_are_compared() {
        let mut src = Sets { sets: history(4), outside: BTreeSet::new(), pairs: vec![], walks: vec![] };
        let l = line(&["a", "b", "c", "d"], &BTreeMap::new());
        let r = measure_with_opts(&l, &mut src, false, false, &mut |_| {}).unwrap();
        assert!(src.walks.is_empty() && r.walks == 0);
        assert_eq!(r.pairs, 3);
        assert!(r.known.iter().all(|k| k.distinct.is_none()));
        // the died of all but the newest: what the older ones hold that the next one dropped
        let died: u64 = (0..3).map(|i| sum(src.sets[i].difference(&src.sets[i + 1]))).sum();
        assert_eq!(r.union, died);
        let (_, _, excl) = truth(&src);
        assert_eq!(r.known[3].exclusive.as_ref().unwrap().bytes, excl[3]);
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

    /// Like [`Sets`], but every pair counted candidate by candidate through
    /// [`count_candidates`] (root of snapshot `i`: `i + 1`; elsewhere: 999), stopping at the
    /// next check once `stop_after` reference lists were read in all.
    struct Counted {
        sets: Vec<BTreeSet<(u64, u64)>>,
        roots: BTreeMap<u64, BTreeSet<u64>>,
        reads: std::cell::Cell<usize>,
        stop_after: Option<usize>,
        gens: Vec<u64>,
    }

    impl Counted {
        fn new(sets: &[BTreeSet<(u64, u64)>], outside: &BTreeSet<(u64, u64)>) -> Self {
            let mut roots: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
            for (i, s) in sets.iter().enumerate() {
                for e in s {
                    roots.entry(e.0).or_default().insert(i as u64 + 1);
                }
            }
            for e in outside {
                roots.entry(e.0).or_default().insert(999);
            }
            Counted { sets: sets.to_vec(), roots, reads: Default::default(), stop_after: None, gens: Vec::new() }
        }
    }

    impl Source for Counted {
        fn pair(&mut self, a: usize, b: usize) -> Result<PairFacts, WalkError> {
            self.pair_from(a, b, None, &mut |_| {})
        }

        fn distinct(&mut self, i: usize) -> Result<u64, WalkError> {
            Ok(sum(self.sets[i].iter()))
        }

        fn generation(&self, i: usize) -> u64 {
            self.gens.get(i).copied().unwrap_or(0)
        }

        fn pair_from(&mut self, a: usize, b: usize, from: Option<&PairCursor>, at: &mut dyn FnMut(&PairCursor)) -> Result<PairFacts, WalkError> {
            let (sa, sb) = (&self.sets[a], &self.sets[b]);
            let cb: BTreeMap<u64, u64> = sb.difference(sa).copied().collect();
            let ca: BTreeMap<u64, u64> = sa.difference(sb).copied().collect();
            let all = candidate_list(&cb, &ca);
            let (roots, reads, limit) = (&self.roots, &self.reads, self.stop_after);
            let mut read = |bytenr: u64| {
                reads.set(reads.get() + 1);
                Ok(roots.get(&bytenr).cloned())
            };
            let stop = || limit.is_some_and(|l| reads.get() >= l);
            let c = count_candidates(&all, from, (a as u64 + 1, b as u64 + 1), &mut read, &stop, at, &|_| {})?;
            Ok(PairFacts { born: c.born, died: c.died, excl_newer: c.excl_newer, excl_older: c.excl_older, distinct: None })
        }
    }

    /// Everything a measure tells.
    #[derive(Default)]
    struct Told {
        saves: Vec<Checkpoint>,
        extents: Vec<(usize, u64, u64, u64)>,
        reused: Vec<usize>,
    }

    impl Hooks for Told {
        fn reused(&mut self, newer: usize) {
            self.reused.push(newer);
        }
        fn extents(&mut self, newer: usize, done: u64, total: u64, from: u64) {
            self.extents.push((newer, done, total, from));
        }
        fn save(&mut self, cp: &Checkpoint) {
            self.saves.push(cp.clone());
        }
    }

    /// A line of thousands of extents, about 1900 candidates per pair.
    fn big_history(n: usize) -> Vec<BTreeSet<(u64, u64)>> {
        let mut out = Vec::new();
        let mut cur: BTreeSet<(u64, u64)> = (0..3000u64).map(|i| (i * 16, 4096 * (1 + i % 7))).collect();
        let mut next = 1_000_000u64;
        for s in 0..n as u64 {
            out.push(cur.clone());
            let gone: Vec<_> = cur.iter().copied().filter(|e| (e.0 / 16 + s) % 3 == 0).collect();
            for g in gone {
                cur.remove(&g);
            }
            for j in 0..900 {
                cur.insert((next, 8192 + 4096 * (j % 5)));
                next += 16;
            }
        }
        out
    }

    fn truth_of(sets: &[BTreeSet<(u64, u64)>], outside: &BTreeSet<(u64, u64)>) -> Sets {
        Sets { sets: sets.to_vec(), outside: outside.clone(), pairs: vec![], walks: vec![] }
    }

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("s{i}")).collect()
    }

    fn line_of(names: &[String]) -> Vec<Snap> {
        names.iter().map(|n| Snap { path: PathBuf::from(n), uuid: n.clone(), known: Known::default() }).collect()
    }

    /// Runs until done, each run stopped after `every_reads` more reads; `lose_last`: each
    /// stop loses the checkpoint's last write (a power cut), so the run goes on from the one
    /// before. Returns the result and the runs.
    fn interrupted(src: &mut Counted, l: &[Snap], every_reads: usize, save_every: Duration, lose_last: bool) -> (LineSpace, usize) {
        let mut cp = Checkpoint::default();
        for runs in 0..10_000 {
            src.stop_after = Some(src.reads.get() + every_reads);
            let mut told = Told::default();
            match measure_core(l, src, true, true, &mut cp, &mut told, save_every) {
                Ok(r) => return (r, runs),
                Err(WalkError::Stopped) => {
                    if lose_last && told.saves.len() >= 2 {
                        cp = told.saves[told.saves.len() - 2].clone();
                    }
                }
                Err(e) => panic!("{e}"),
            }
        }
        panic!("never finished");
    }

    #[test]
    fn a_stopped_measure_goes_on_and_ends_with_the_numbers_of_one_never_stopped() {
        let sets = big_history(5);
        let outside: BTreeSet<_> = sets[4].iter().copied().step_by(97).collect();
        let l = line_of(&names(5));
        let mut src = Counted::new(&sets, &outside);
        let whole = measure_core(&l, &mut src, true, true, &mut Checkpoint::default(), &mut Told::default(), Duration::MAX).unwrap();
        check(&whole, &truth_of(&sets, &outside));
        let reads = src.reads.get();
        assert!(reads > 4 * 1500, "{reads}");
        for every in [1, 255, 256, 257, 700, 1000, 1999, 2500, 5000] {
            // stopped and saved where it stopped: nothing read twice
            let mut src = Counted::new(&sets, &outside);
            let (r, runs) = interrupted(&mut src, &l, every, Duration::MAX, false);
            assert!(runs >= 1, "every {every}");
            assert_eq!((&r.known, r.union), (&whole.known, whole.union), "every {every}");
            assert_eq!(src.reads.get(), reads, "every {every}: nothing read twice");
            // saved at every cursor and each stop loses its last write: some read again
            let mut src = Counted::new(&sets, &outside);
            let (r, _) = interrupted(&mut src, &l, every, Duration::ZERO, true);
            assert_eq!((&r.known, r.union), (&whole.known, whole.union), "every {every}, last write lost");
            assert!(src.reads.get() >= reads);
        }
    }

    #[test]
    fn progress_inside_a_pair_and_finished_pairs_reused() {
        let sets = big_history(3);
        let l = line_of(&names(3));
        let mut src = Counted::new(&sets, &BTreeSet::new());
        let mut cp = Checkpoint::default();
        // stopped part way in the second pair
        let first: u64 = sets[1].symmetric_difference(&sets[0]).count() as u64;
        src.stop_after = Some(first as usize + 600);
        let mut told = Told::default();
        assert_eq!(measure_core(&l, &mut src, true, true, &mut cp, &mut told, Duration::MAX), Err(WalkError::Stopped));
        assert_eq!((cp.pairs.len(), cp.partial.len()), (1, 1));
        let stopped_at = cp.partial[0].cursor.done;
        assert!(stopped_at > 0 && stopped_at % 256 == 0, "{stopped_at}");
        assert_eq!(told.saves.last(), Some(&cp));
        // the next measure takes the first pair as it is and goes on in the second
        src.stop_after = None;
        let mut told = Told::default();
        let r = measure_core(&l, &mut src, true, true, &mut cp, &mut told, Duration::MAX).unwrap();
        assert_eq!((r.reused, r.pairs, told.reused.clone()), (1, 1, vec![1]));
        let (b, done, total, from) = told.extents[0];
        assert_eq!((b, done, from), (2, stopped_at, stopped_at));
        assert!(told.extents.windows(2).all(|w| w[0].1 <= w[1].1));
        assert_eq!(told.extents.last().map(|x| (x.1, x.2)), Some((total, total)));
        assert!(cp.partial.is_empty() && cp.pairs.len() == 2);
        check(&r, &truth_of(&sets, &BTreeSet::new()));
    }

    #[test]
    fn a_checkpoint_holds_only_for_the_same_snapshots() {
        let sets = big_history(5);
        let none = BTreeSet::new();
        let l = line_of(&names(5));
        let truth = truth_of(&sets, &none);
        // a cursor over another candidate list: the pair is counted again, right
        let mut src = Counted::new(&sets, &none);
        src.stop_after = Some(700);
        let mut cp = Checkpoint::default();
        assert!(measure_core(&l, &mut src, true, true, &mut cp, &mut Told::default(), Duration::MAX).is_err());
        cp.partial[0].cursor.fingerprint ^= 1;
        src.stop_after = None;
        check(&measure_core(&l, &mut src, true, true, &mut cp, &mut Told::default(), Duration::MAX).unwrap(), &truth);
        // a snapshot whose generation changed: none of its pairs reused
        let mut src = Counted::new(&sets, &none);
        src.gens = vec![1, 2, 3, 4, 5];
        src.stop_after = Some(3 * 2000);
        let mut cp = Checkpoint::default();
        assert!(measure_core(&l, &mut src, true, true, &mut cp, &mut Told::default(), Duration::MAX).is_err());
        assert!(cp.pairs.len() >= 2, "{}", cp.pairs.len());
        src.gens[1] = 22;
        src.stop_after = None;
        let mut told = Told::default();
        let r = measure_core(&l, &mut src, true, true, &mut cp, &mut told, Duration::MAX).unwrap();
        check(&r, &truth);
        assert!(!told.reused.contains(&1) && !told.reused.contains(&2), "{:?}", told.reused);
        let now: BTreeMap<String, u64> = names(5).into_iter().zip(src.gens.clone()).collect();
        let mut kept = cp.clone();
        kept.retain_present(&now);
        assert!(kept.pairs.iter().all(|p| p.key.older != "s1" || p.key.generations.0 == 22));
        // a snapshot gone in the middle: pairs that had it as a neighbour keep born and died,
        // their "alone" on that side comes from the new pair
        let mut src = Counted::new(&sets, &none);
        src.stop_after = Some(3 * 2000);
        let mut cp = Checkpoint::default();
        assert!(measure_core(&l, &mut src, true, true, &mut cp, &mut Told::default(), Duration::MAX).is_err());
        let mut left = sets.clone();
        left.remove(2);
        let ln: Vec<String> = ["s0", "s1", "s3", "s4"].iter().map(|s| s.to_string()).collect();
        let mut src = Counted::new(&left, &none);
        let mut told = Told::default();
        let r = measure_core(&line_of(&ln), &mut src, true, true, &mut cp, &mut told, Duration::MAX).unwrap();
        check(&r, &truth_of(&left, &none));
        assert_eq!(told.reused, [1]);
        let now: BTreeMap<String, u64> = ln.iter().map(|n| (n.clone(), 0)).collect();
        cp.retain_present(&now);
        let gone = |k: &PairKey| k.older == "s2" || k.newer == "s2";
        assert!(cp.pairs.iter().all(|p| !gone(&p.key)) && cp.partial.iter().all(|p| !gone(&p.key)));
    }

    #[test]
    fn new_snapshots_between_passes_never_start_a_pair_again() {
        let all = big_history(9);
        let none = BTreeSet::new();
        let mut known: BTreeMap<String, Known> = BTreeMap::new();
        let mut cp = Checkpoint::default();
        let (mut n, mut reads, mut passes, mut stopped) = (3, 0usize, 0, 0);
        loop {
            passes += 1;
            assert!(passes < 60, "never finished");
            let names = names(n);
            let l: Vec<Snap> = names.iter().map(|x| Snap { path: PathBuf::from(x), uuid: x.clone(), known: known.get(x).cloned().unwrap_or_default() }).collect();
            let mut src = Counted::new(&all[..n], &none);
            // each pass reads about one pair, and a snapshot comes after it: the newest pair is
            // often stopped and a new snapshot changes its neighbour before it goes on
            src.stop_after = Some(2000);
            let r = measure_core(&l, &mut src, true, true, &mut cp, &mut Told::default(), Duration::MAX);
            reads += src.reads.get();
            match r {
                Ok(r) => {
                    for (s, k) in l.iter().zip(&r.known) {
                        known.insert(s.uuid.clone(), k.clone());
                    }
                    if n == all.len() {
                        check(&r, &truth_of(&all, &none));
                        break;
                    }
                }
                Err(WalkError::Stopped) => stopped += 1,
                Err(e) => panic!("{e}"),
            }
            n = (n + 1).min(all.len());
        }
        assert!(stopped >= 3, "{stopped}");
        // each pair's candidates counted once, also across the new snapshots
        let want: usize = (1..all.len()).map(|b| all[b - 1].symmetric_difference(&all[b]).count()).sum();
        assert_eq!(reads, want);
    }

    #[test]
    fn a_checkpoint_stays_small() {
        let key = |i: usize| PairKey { older: format!("a{i}"), newer: format!("b{i}"), generations: (0, 0) };
        let mut cp = Checkpoint::default();
        for i in 0..Checkpoint::MAX_PAIRS + 10 {
            cp.put_cursor(key(i), PairCursor { done: 1, ..PairCursor::default() }, 0);
            cp.put_facts(key(i), PairFacts::default(), Sides::default());
        }
        assert_eq!(cp.pairs.len(), Checkpoint::MAX_PAIRS);
        assert!(cp.partial.is_empty());
        assert_eq!(cp.pairs[0].key, key(10));
        for i in 0..Checkpoint::MAX_PARTIAL + 3 {
            cp.put_cursor(key(i), PairCursor::default(), if i % 2 == 0 { 1_000 } else { 9_000_000_000 });
        }
        assert_eq!(cp.partial.len(), Checkpoint::MAX_PARTIAL);
        // cursors older than their limit go
        let mut aged = cp.clone();
        aged.expire(9_000_000_000 + 1000, CURSOR_MAX_AGE);
        assert!(!aged.partial.is_empty() && aged.partial.iter().all(|p| p.at_ms == 9_000_000_000));
        let line: BTreeSet<&str> = ["a20", "b30"].into_iter().collect();
        let before = cp.pairs.len();
        cp.forget(&line);
        assert_eq!(cp.pairs.len(), before - 2);
        let back: Checkpoint = serde_json::from_slice(&serde_json::to_vec(&cp).unwrap()).unwrap();
        assert_eq!(back, cp);
    }
}
