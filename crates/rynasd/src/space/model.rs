//! The pure rules of Space: daily history and growth, budgets, the low-space state with its
//! hysteresis and the order of the automatic thinning.

use std::collections::{BTreeMap, BTreeSet};

use rynas_api_types::space::{SnapshotSource, SourceKind, SpaceSettings};
use serde::{Deserialize, Serialize};

/// Days of history kept per source.
pub const HISTORY_DAYS: u32 = 90;
const DAY_MS: u64 = 86_400_000;

/// Daily values per key (a source id, or `volume:<label>` for the snapshot total of a
/// volume): `(day since the epoch, bytes)`, oldest first, one per day.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub series: BTreeMap<String, Vec<(u32, u64)>>,
    /// When the last growth notification check ran (ms).
    #[serde(default)]
    pub growth_checked_ms: u64,
    /// Volumes in the low-space state (hysteresis survives restarts).
    #[serde(default)]
    pub low: Vec<String>,
    /// The automatic cleanup's daily count and pause, per volume.
    #[serde(default)]
    pub thin: BTreeMap<String, ThinDay>,
    /// When the "automatic cleanup cannot free more" problem was last notified, per volume
    /// (at most one per [`STUCK_NOTIFY_MS`]); cleared when free space is back.
    #[serde(default)]
    pub stuck_notified_ms: BTreeMap<String, u64>,
    /// Volumes whose automatic cleanup is suspended: their snapshots grew far more than
    /// usual in a week (a mass change such as ransomware encrypting a share), so the oldest
    /// snapshots may be the clean ones. Cleared by the next weekly check that is not anomalous.
    #[serde(default)]
    pub thin_suspended: BTreeSet<String>,
}

/// At most one "cannot free more" notification per volume in this time.
pub const STUCK_NOTIFY_MS: u64 = 3600 * 1000;
/// After a pass that found nothing more to remove or failed, the next one waits this long.
pub const STUCK_RETRY_MS: u64 = 3600 * 1000;
/// Weekly snapshot growth above this many times the median of the 4 weeks before is
/// anomalous …
pub const ANOMALY_FACTOR: i64 = 3;
/// … with a median counted as at least this much (a flat history makes any growth "3×").
pub const ANOMALY_FLOOR_BYTES: i64 = 1 << 30;

/// Most automatic deletions per volume and day.
pub const DAILY_CAP: u32 = 100;
/// A pass stops when this many deletions in a row …
pub const NO_GAIN_WINDOW: usize = 5;
/// … freed less than this together.
pub const NO_GAIN_BYTES: u64 = 1 << 30;
/// The pause after a pass stopped for no gain.
pub const COOLDOWN_MS: u64 = 24 * 3600 * 1000;

/// Why a pass of the automatic cleanup stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinStop {
    Recovered,
    Nothing,
    NoGain,
    DailyCap,
    PassCap,
    Failed,
    Cooldown,
    /// The pass used its time ([`crate::space::THIN_TIME_CAP`]).
    TimeCap,
    /// Other work holds the volume (replication, receive, scrub, backup, snapshot run).
    Busy,
    /// Suspended after anomalous growth.
    Suspended,
}

/// The automatic cleanup of one volume today.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinDay {
    pub day: u32,
    pub count: u32,
    pub cooldown_until_ms: u64,
}

impl ThinDay {
    /// Deletions still allowed now, or why none (a pause, the daily cap).
    pub fn allowance(&mut self, now_ms: u64) -> Result<u32, ThinStop> {
        if now_ms < self.cooldown_until_ms {
            return Err(ThinStop::Cooldown);
        }
        if self.day != day_of(now_ms) {
            self.day = day_of(now_ms);
            self.count = 0;
        }
        DAILY_CAP.checked_sub(self.count).filter(|n| *n > 0).ok_or(ThinStop::DailyCap)
    }
}

/// The brake of one pass: what each deletion gave back.
#[derive(Debug, Default)]
pub struct Brake {
    gains: Vec<u64>,
}

impl Brake {
    /// Records one deletion's gain; `true`: the last [`NO_GAIN_WINDOW`] together freed less than
    /// [`NO_GAIN_BYTES`] (stop the pass).
    pub fn record(&mut self, gained: u64) -> bool {
        self.gains.push(gained);
        let n = self.gains.len();
        n >= NO_GAIN_WINDOW && self.gains[n - NO_GAIN_WINDOW..].iter().sum::<u64>() < NO_GAIN_BYTES
    }
}

pub fn day_of(ms: u64) -> u32 {
    (ms / DAY_MS) as u32
}

impl History {
    /// Records today's value of `key` (replacing an earlier one of the same day) and drops
    /// what is older than [`HISTORY_DAYS`].
    pub fn record(&mut self, key: &str, now_ms: u64, bytes: u64) {
        let day = day_of(now_ms);
        let v = self.series.entry(key.to_string()).or_default();
        match v.last_mut() {
            Some(last) if last.0 == day => last.1 = bytes,
            _ => v.push((day, bytes)),
        }
        v.retain(|(d, _)| day.saturating_sub(*d) < HISTORY_DAYS);
    }

    /// Change of `key` against `days` days ago: today's value minus the newest value at
    /// least `days` old (and at most 3 days older than that). `None` without such a value.
    pub fn growth(&self, key: &str, now_ms: u64, days: u32) -> Option<i64> {
        let v = self.series.get(key)?;
        let today = day_of(now_ms);
        let (_, now) = *v.last().filter(|(d, _)| today.saturating_sub(*d) <= 1)?;
        let target = today.saturating_sub(days);
        let (d, then) = *v.iter().rev().find(|(d, _)| *d <= target)?;
        (target - d <= 3).then(|| now as i64 - then as i64)
    }

    /// The newest value of `key` on `day` or at most 3 days before it.
    fn value_at(&self, key: &str, day: u32) -> Option<u64> {
        let (d, v) = *self.series.get(key)?.iter().rev().find(|(d, _)| *d <= day)?;
        (day - d <= 3).then_some(v)
    }

    /// The growth of `key` in each of the last `weeks` weeks before the current one, newest
    /// first (`None` when the history does not reach that far).
    pub fn weekly_growths(&self, key: &str, now_ms: u64, weeks: u32) -> Option<Vec<i64>> {
        let today = day_of(now_ms);
        (1..=weeks)
            .map(|k| {
                let end = today.checked_sub(7 * k)?;
                let start = end.checked_sub(7)?;
                Some(self.value_at(key, end)? as i64 - self.value_at(key, start)? as i64)
            })
            .collect()
    }

    /// Drops the series of keys that are gone (removed sources), keeping volume totals.
    pub fn keep_only(&mut self, live: &std::collections::BTreeSet<String>) {
        self.series.retain(|k, _| k.starts_with("volume:") || live.contains(k));
    }
}

/// The low-space state with hysteresis: entered below `low_pct` free, left at or above
/// `recover_pct`.
pub fn low_state(was_low: bool, free_bytes: u64, total_bytes: u64, low_pct: u8, recover_pct: u8) -> bool {
    if total_bytes == 0 {
        return false;
    }
    let pct = free_bytes as f64 * 100.0 / total_bytes as f64;
    if was_low { pct < f64::from(recover_pct) } else { pct < f64::from(low_pct) }
}

/// Bytes to free so that `free` reaches `recover_pct` of `total`.
pub fn bytes_to_recover(free_bytes: u64, total_bytes: u64, recover_pct: u8) -> u64 {
    (total_bytes as u128 * u128::from(recover_pct) / 100).saturating_sub(u128::from(free_bytes)) as u64
}

/// The budget of a source is exceeded.
pub fn over_budget(s: &SnapshotSource, settings: &SpaceSettings, now_ms: u64) -> bool {
    let Some(b) = settings.budget(&s.id) else { return false };
    b.max_count.is_some_and(|n| s.count > n)
        || b.max_bytes.is_some_and(|n| s.freed_bytes.is_some_and(|x| x > n))
        || b.max_age_days.is_some_and(|d| s.oldest_ms.is_some_and(|o| now_ms.saturating_sub(o) > u64::from(d) * DAY_MS))
}

/// Sources the automatic thinning may take from: never PC backups (their own retention and
/// alerts decide) and never replicas of removed targets or orphans (a person decides).
pub fn thinnable(kind: SourceKind) -> bool {
    matches!(kind, SourceKind::ShareJob | SourceKind::AppStack | SourceKind::SystemPoints)
}

/// The order of the automatic thinning on one volume: first, in each over-budget source,
/// the oldest snapshots beyond its budget (count and age); then every other unprotected
/// snapshot, oldest first across sources. Protected and pinned snapshots never appear.
pub fn thin_order(sources: &[SnapshotSource], settings: &SpaceSettings, now_ms: u64) -> Vec<String> {
    let mut first: Vec<(u64, String)> = Vec::new();
    let mut rest: Vec<(u64, String)> = Vec::new();
    for s in sources.iter().filter(|s| thinnable(s.kind)) {
        let budget = settings.budget(&s.id);
        // newest first in the source: index = how many are newer
        for (i, x) in s.snapshots.iter().enumerate() {
            if x.protected.is_some() || x.pinned {
                continue;
            }
            let t = x.created_ms.unwrap_or(0);
            let beyond_count = budget.and_then(|b| b.max_count).is_some_and(|n| i as u32 >= n);
            let beyond_age = budget
                .and_then(|b| b.max_age_days)
                .is_some_and(|d| now_ms.saturating_sub(t) > u64::from(d) * DAY_MS);
            if beyond_count || beyond_age {
                first.push((t, x.path.clone()));
            } else {
                rest.push((t, x.path.clone()));
            }
        }
    }
    first.sort();
    rest.sort();
    first.into_iter().chain(rest).map(|(_, p)| p).collect()
}

/// The snapshot sources that grew the most in 7 days (for the growth notification).
pub fn top_growth(sources: &[SnapshotSource], n: usize) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = sources
        .iter()
        .filter_map(|s| s.growth_7d_bytes.filter(|g| *g > 0).map(|g| (s.name.clone(), g)))
        .collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v.truncate(n);
    v
}

/// Whether this week's snapshot growth `grown` is anomalous against the weeks before
/// (`prior`, at least 4): more than [`ANOMALY_FACTOR`] times their median (counted as at
/// least [`ANOMALY_FLOOR_BYTES`]). Without 4 weeks of history nothing is anomalous.
pub fn growth_anomalous(grown: i64, prior: &[i64]) -> bool {
    if prior.len() < 4 || grown <= 0 {
        return false;
    }
    let mut v = prior[..4].to_vec();
    v.sort();
    let median = ((v[1] + v[2]) / 2).max(ANOMALY_FLOOR_BYTES);
    grown > ANOMALY_FACTOR * median
}

/// Whether 7-day snapshot growth `grown` (from `before`) passes the limits.
pub fn growth_alarming(grown: i64, before: u64, settings: &SpaceSettings) -> bool {
    if grown <= 0 {
        return false;
    }
    let gib = settings.growth_gib > 0 && grown as u64 >= u64::from(settings.growth_gib) << 30;
    let pct = settings.growth_pct > 0 && before > 0 && grown as u128 * 100 >= u128::from(before) * u128::from(settings.growth_pct);
    gib || pct
}

#[cfg(test)]
mod tests {
    use rynas_api_types::space::{MeasureMethod, Protection, SourceBudget, SourceCaps, SpaceSnapshot};

    use super::*;

    const D: u64 = DAY_MS;

    fn source(id: &str, kind: SourceKind, snaps: &[(u64, Option<Protection>)]) -> SnapshotSource {
        SnapshotSource {
            id: id.into(),
            kind,
            name: id.into(),
            volume: "vol1".into(),
            dir: "/vol1/x".into(),
            count: snaps.len() as u32,
            oldest_ms: snaps.last().map(|s| s.0),
            newest_ms: snaps.first().map(|s| s.0),
            freed_bytes: None,
            freed_at_least_bytes: None,
            method: MeasureMethod::None,
            growth_7d_bytes: None,
            caps: SourceCaps::default(),
            over_budget: false,
            snapshots: snaps
                .iter()
                .map(|(t, p)| SpaceSnapshot {
                    path: format!("/vol1/{id}/{t}"),
                    name: t.to_string(),
                    created_ms: Some(*t),
                    pinned: *p == Some(Protection::Pinned),
                    protected: *p,
                    identical: false,
                })
                .collect(),
        }
    }

    #[test]
    fn history_records_one_value_a_day_and_measures_growth() {
        let mut h = History::default();
        let t0 = 100 * D;
        for day in 0..10u64 {
            h.record("job:a", t0 + day * D, 1000 + day * 100);
        }
        h.record("job:a", t0 + 9 * D + 5, 2000); // same day: replaced
        assert_eq!(h.series["job:a"].len(), 10);
        // today 2000, 7 days ago 1200
        assert_eq!(h.growth("job:a", t0 + 9 * D, 7), Some(800));
        // not enough history
        assert_eq!(h.growth("job:a", t0 + 9 * D, 30), None);
        // a stale series (no value today or yesterday) has no growth
        assert_eq!(h.growth("job:a", t0 + 20 * D, 7), None);
        // 90 days kept
        h.record("job:a", t0 + 200 * D, 1);
        assert_eq!(h.series["job:a"].len(), 1);
        h.keep_only(&Default::default());
        assert!(h.series.is_empty());
    }

    #[test]
    fn low_state_has_hysteresis() {
        let total = 1000;
        assert!(!low_state(false, 120, total, 10, 15));
        assert!(low_state(false, 99, total, 10, 15));
        // once low, stays low until 15 %
        assert!(low_state(true, 120, total, 10, 15));
        assert!(!low_state(true, 150, total, 10, 15));
        assert_eq!(bytes_to_recover(99, total, 15), 51);
        assert_eq!(bytes_to_recover(200, total, 15), 0);
        assert!(!low_state(true, 0, 0, 10, 15));
    }

    #[test]
    fn thinning_goes_oldest_first_and_never_takes_protected() {
        let a = source("job:a", SourceKind::ShareJob, &[(50 * D, Some(Protection::Newest)), (40 * D, None), (10 * D, Some(Protection::Pinned)), (5 * D, None)]);
        let b = source("stack:b", SourceKind::AppStack, &[(45 * D, Some(Protection::Newest)), (20 * D, None)]);
        let pc = source("pc:x", SourceKind::PcBackup, &[(60 * D, Some(Protection::NewestBackup)), (D, None)]);
        let order = thin_order(&[a.clone(), b.clone(), pc], &SpaceSettings::default(), 60 * D);
        assert_eq!(order, ["/vol1/job:a/432000000", "/vol1/stack:b/1728000000", "/vol1/job:a/3456000000"]);
        // a budget of 2 snapshots on b... and 30 days on a puts their excess first
        let mut s = SpaceSettings::default();
        s.budgets.push(SourceBudget { source: "stack:b".into(), max_count: Some(1), ..Default::default() });
        s.budgets.push(SourceBudget { source: "job:a".into(), max_age_days: Some(30), ..Default::default() });
        let order = thin_order(&[a, b], &s, 60 * D);
        assert_eq!(order[0], "/vol1/job:a/432000000");
        assert_eq!(order[1], "/vol1/stack:b/1728000000");
        assert_eq!(order[2], "/vol1/job:a/3456000000");
    }

    #[test]
    fn the_brake_stops_passes_without_gain() {
        let mut b = Brake::default();
        for _ in 0..4 {
            assert!(!b.record(100 << 20));
        }
        // five deletions that freed 500 MiB together: stop
        assert!(b.record(100 << 20));
        let mut b = Brake::default();
        for _ in 0..10 {
            assert!(!b.record(1 << 30));
        }
        // the window slides: four good, then five that gave nothing
        for i in 0..5 {
            assert_eq!(b.record(0), i == 4);
        }
        // the daily cap and the pause
        let mut d = ThinDay::default();
        let now = 100 * D;
        assert_eq!(d.allowance(now), Ok(DAILY_CAP));
        d.count = DAILY_CAP;
        assert_eq!(d.allowance(now + 1), Err(ThinStop::DailyCap));
        assert_eq!(d.allowance(now + D), Ok(DAILY_CAP), "a new day");
        d.cooldown_until_ms = now + D + COOLDOWN_MS;
        assert_eq!(d.allowance(now + D + 1), Err(ThinStop::Cooldown));
        assert!(d.allowance(now + 2 * D + COOLDOWN_MS).is_ok());
    }

    #[test]
    fn budgets_and_growth_limits() {
        let mut src = source("job:a", SourceKind::ShareJob, &[(50 * D, None), (10 * D, None)]);
        src.freed_bytes = Some(5 << 30);
        let mut s = SpaceSettings::default();
        assert!(!over_budget(&src, &s, 60 * D));
        s.budgets.push(SourceBudget { source: "job:a".into(), max_bytes: Some(4 << 30), ..Default::default() });
        assert!(over_budget(&src, &s, 60 * D));
        s.budgets[0] = SourceBudget { source: "job:a".into(), max_age_days: Some(60), ..Default::default() };
        assert!(!over_budget(&src, &s, 60 * D));
        assert!(over_budget(&src, &s, 80 * D));
        let s = SpaceSettings::default();
        assert!(growth_alarming(50 << 30, 1 << 40, &s));
        assert!(growth_alarming(30 << 30, 100 << 30, &s), "30 % of 100 GiB");
        assert!(!growth_alarming(10 << 30, 100 << 30, &s));
        assert!(!growth_alarming(-5, 10, &s));
        let mut g = source("job:a", SourceKind::ShareJob, &[]);
        g.growth_7d_bytes = Some(7);
        assert_eq!(top_growth(&[g], 3), [("job:a".to_string(), 7)]);
    }

    #[test]
    fn anomalous_growth_against_the_weeks_before() {
        const G: u64 = 1 << 30;
        let mut h = History::default();
        // 10 GiB a week for 5 weeks, then 100 GiB in the last one
        let today = 400u32;
        for d in (today - 42)..=today {
            let weeks_back = (today - d) / 7;
            let base = (6 - weeks_back.min(6)) as u64 * 10 * G;
            let bytes = if d > today - 7 { base + 90 * G } else { base };
            h.record("volume:v", u64::from(d) * D, bytes);
        }
        let now = u64::from(today) * D;
        let prior = h.weekly_growths("volume:v", now, 4).unwrap();
        assert_eq!(prior, [10 * G as i64; 4]);
        let grown = h.growth("volume:v", now, 7).unwrap();
        assert!(growth_anomalous(grown, &prior), "{grown}");
        assert!(!growth_anomalous(25 * G as i64, &prior));
        // too little history: never anomalous
        assert!(h.weekly_growths("volume:v", now, 8).is_none());
        assert!(!growth_anomalous(grown, &prior[..3]));
        // a flat history counts as 1 GiB a week
        assert!(!growth_anomalous(2 * G as i64, &[0, 0, 0, 0]));
        assert!(growth_anomalous(4 * G as i64, &[0, 0, 0, 0]));
    }
}
