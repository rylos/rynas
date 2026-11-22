//! The overview of every job over a range of time (S5 of `docs/research/scheduling.md`,
//! `GET /api/v1/scheduler/overview`): when each job starts (with systemd's calendar rules:
//! [`rynas_schedule`], never a second calendar implementation), how long it usually takes,
//! the windows, and the conflicts:
//!
//! - two jobs of heavy work on one volume that overlap: the less urgent waits in the queue, or
//!   a running scrub gives way to more urgent work and resumes after it ([`crate::queue`]);
//! - a disk test (SMART long test, md RAID check) that overlaps heavy work on a volume of its
//!   disk: it waits when the heavy work came first, else both run (it slows itself down);
//! - a job that follows the maintenance window and starts outside it: it waits for the window,
//!   or is skipped when the window opens after its deadline ([`crate::maintenance`]).
//!
//! rynasd gathers what this module needs ([`JobInput`]: the tasks, their claims, their last
//! runs and estimates); everything here is pure.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use rynas_api_types::scheduler::{
    ConflictKind, ConflictOutcome, OverviewConflict, OverviewJob, OverviewRun, OverviewWindow, OverviewWindowKind, Resource, RunTrigger,
    ScheduledTask, SchedulerOverview, TaskAction, TaskTrigger, week_window_on, week_window_valid,
};
use rynas_schedule::CalendarSpec;

use crate::maintenance::{Defer, defer};
use crate::queue::{Claim, EVERY, priority_of};

/// Starts listed per job at most (a job every few minutes is marked `truncated`).
pub const MAX_RUNS_PER_JOB: usize = 400;

/// Conflicts listed at most, of each group (jobs that meet on a volume or disk; starts
/// outside the maintenance window).
pub const MAX_CONFLICTS: usize = 500;

/// The length shown for a job that never ran (no estimate yet).
pub const DEFAULT_MS: u64 = 15 * 60_000;

/// The longest range asked (five weeks).
pub const MAX_RANGE_MS: u64 = 35 * 86_400_000;

/// A job as rynasd found it.
#[derive(Debug, Clone)]
pub struct JobInput {
    /// The job as the overview lists it (rynasd fills everything but `truncated`).
    pub job: OverviewJob,
    /// Its task (absent for a PC's backup plan).
    pub task: Option<ScheduledTask>,
    /// What it claims (the keys of its volumes, its disks): the conflicts.
    pub claims: Vec<Claim>,
    /// The resource of each claim, in the same order (for the sentences).
    pub resources: Vec<Resource>,
    /// End of its last run (an interval task), or a PC's last good backup.
    pub last_ms: Option<u64>,
    /// A PC's plan: when its interval counts from (the last good run's start for agents from
    /// 0.8.24, else `last_ms`).
    pub interval_from_ms: Option<u64>,
    /// A PC's plan: each next start an interval after the previous start (agents from
    /// 0.8.24), else after the previous run's end.
    pub from_start: bool,
    /// When its timer elapses next, when known (an interval task's own timer).
    pub next_ms: Option<u64>,
}

fn ts(ms: u64) -> Option<Timestamp> {
    Timestamp::from_millisecond(i64::try_from(ms).ok()?).ok()
}

fn ms_of(t: Timestamp) -> u64 {
    t.as_millisecond().max(0) as u64
}

/// The start of the local hour after the one `ms` is in.
fn next_hour(tz: &TimeZone, ms: u64) -> u64 {
    // (saturating: a wrapped hour would send the caller's walk back to the epoch)
    let Some(t) = ts(ms) else { return ms.saturating_add(3_600_000) };
    let local = t.to_zoned(tz.clone()).datetime();
    let next = local
        .with()
        .minute(0)
        .second(0)
        .subsec_nanosecond(0)
        .build()
        .ok()
        .and_then(|h| h.checked_add(jiff::ToSpan::hour(1)).ok())
        .and_then(|n| tz.to_ambiguous_zoned(n).compatible().ok())
        .map(|z| ms_of(z.timestamp()));
    match next {
        Some(n) if n > ms => n,
        _ => ms.saturating_add(3_600_000),
    }
}

fn cell_at(tz: &TimeZone, ms: u64) -> usize {
    let dt: DateTime = ts(ms).map(|t| t.to_zoned(tz.clone()).datetime()).unwrap_or_default();
    dt.weekday().to_sunday_zero_offset() as usize * 24 + dt.hour() as usize
}

/// The blocks of a weekly window in `[from, to)`: (start, end) by local hours.
pub fn blocks(window: &str, tz: &TimeZone, from: u64, to: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut at = from;
    let mut open: Option<u64> = None;
    while at < to {
        let next = next_hour(tz, at).min(to);
        let on = week_window_on(window, cell_at(tz, at));
        match (on, open) {
            (true, None) => open = Some(at),
            (false, Some(s)) => {
                out.push((s, at));
                open = None;
            }
            _ => {}
        }
        at = next;
    }
    if let Some(s) = open {
        out.push((s, to));
    }
    out
}

/// The starts of a calendar expression in `[from, to]` (at most `max`; `true` when cut).
fn calendar_starts(expr: &str, tz: &TimeZone, from: u64, to: u64, max: usize) -> (Vec<u64>, bool) {
    let Ok(spec) = CalendarSpec::parse_with(expr, |z| TimeZone::get(z).is_ok()) else {
        return (Vec::new(), false);
    };
    let mut out = Vec::new();
    let Some(mut at) = ts(from.saturating_sub(1)) else { return (out, false) };
    while let Some(n) = spec.next_after(tz, at) {
        let m = ms_of(n);
        if m > to {
            return (out, false);
        }
        if out.len() >= max {
            return (out, true);
        }
        out.push(m);
        at = n;
    }
    (out, false)
}

/// Every `every_ms` from `first` (moved forward into the range) up to `to`.
fn every_starts(first: u64, every_ms: u64, from: u64, to: u64, max: usize) -> (Vec<u64>, bool) {
    let mut out = Vec::new();
    if every_ms == 0 {
        return (out, false);
    }
    let mut at = first;
    if at < from {
        at += (from - at).div_ceil(every_ms) * every_ms;
    }
    while at <= to {
        if out.len() >= max {
            return (out, true);
        }
        out.push(at);
        at += every_ms;
    }
    (out, false)
}

/// "02:00", or "Tue 02:00" for a range of more than a day.
fn when(tz: &TimeZone, ms: u64, with_day: bool) -> String {
    let Some(t) = ts(ms) else { return String::new() };
    let z = t.to_zoned(tz.clone());
    if with_day { z.strftime("%a %H:%M").to_string() } else { z.strftime("%H:%M").to_string() }
}

/// "≈9 h", "≈25 min", "≈2 h 30 min".
fn about(ms: u64) -> String {
    let min = ms.div_ceil(60_000).max(1);
    match (min / 60, min % 60) {
        (0, m) => format!("≈{m} min"),
        (h, _) if h >= 10 => format!("≈{} h", (min + 30) / 60),
        (h, 0) => format!("≈{h} h"),
        (h, m) => format!("≈{h} h {m} min"),
    }
}

fn resource_text(r: &Resource) -> String {
    match r {
        Resource::Volume { path } if path == EVERY => "every volume".into(),
        Resource::Volume { path } => path.clone(),
        Resource::Disk { disk } => format!("disk {disk}"),
        Resource::Packages => "the package system".into(),
        Resource::Power => "the shutdown".into(),
        Resource::Maintenance => "maintenance work".into(),
    }
}

/// A start as it will really happen (after the maintenance window).
#[derive(Debug, Clone)]
struct Eff {
    job: usize,
    start: u64,
    end: u64,
}

/// The overview of `jobs` over `[from, to]`. `disks_of` gives the disks of a volume key (for
/// the disk class); `window` is the global maintenance window.
pub fn build(
    jobs: Vec<JobInput>,
    window: Option<&str>,
    tz: &TimeZone,
    from: u64,
    to: u64,
    now: u64,
    disks_of: &dyn Fn(&str) -> Vec<String>,
) -> SchedulerOverview {
    let to = to.min(from.saturating_add(MAX_RANGE_MS)).max(from);
    let with_day = to - from > 86_400_000;
    let mut runs: Vec<OverviewRun> = Vec::new();
    let mut windows: Vec<OverviewWindow> = Vec::new();
    let mut effs: Vec<Eff> = Vec::new();
    let mut out_jobs: Vec<OverviewJob> = Vec::with_capacity(jobs.len());
    if let Some(w) = window {
        for (s, e) in blocks(w, tz, from, to) {
            windows.push(OverviewWindow { kind: OverviewWindowKind::Maintenance, job: None, start_ms: s, end_ms: e });
        }
    }
    for (i, j) in jobs.iter().enumerate() {
        let mut job = j.job.clone();
        let dur = job.estimate_ms.filter(|d| *d > 0);
        let mut starts: Vec<(u64, u64, bool)> = Vec::new(); // start, end, estimated
        if job.enabled {
            match (&j.task, job.every_h) {
                (Some(t), _) => match &t.trigger {
                    TaskTrigger::Calendar { .. } | TaskTrigger::Recurrence { .. } => {
                        if let Some(expr) = t.trigger.on_calendar() {
                            let (s, cut) = calendar_starts(&expr, tz, from, to, MAX_RUNS_PER_JOB);
                            job.truncated = cut;
                            let p = t.effective_policy();
                            for st in s {
                                starts.push((st, st + dur.unwrap_or(DEFAULT_MS), dur.is_some()));
                                // where it may start: its start window, or its random delay
                                let band = if p.window_min > 0 {
                                    u64::from(p.window_min) * 60_000
                                } else if p.jitter_s >= 600 {
                                    u64::from(p.jitter_s) * 1000
                                } else {
                                    0
                                };
                                if band > 0 {
                                    windows.push(OverviewWindow { kind: OverviewWindowKind::Start, job: Some(job.id.clone()), start_ms: st, end_ms: st + band });
                                }
                            }
                        }
                    }
                    TaskTrigger::Interval { hours, .. } => {
                        let every = u64::from(*hours) * 3_600_000;
                        let first = j.next_ms.or(j.last_ms.map(|l| l + every)).unwrap_or(now).max(now.min(to));
                        let (s, cut) = every_starts(first, every, from, to, MAX_RUNS_PER_JOB);
                        job.truncated = cut;
                        starts.extend(s.into_iter().map(|st| (st, st + dur.unwrap_or(DEFAULT_MS), dur.is_some())));
                    }
                    TaskTrigger::Window { window: w, stop_at_end } => {
                        for (s, e) in blocks(w, tz, from, to) {
                            windows.push(OverviewWindow { kind: OverviewWindowKind::Work, job: Some(job.id.clone()), start_ms: s, end_ms: e });
                            let end = if *stop_at_end { e } else { s + dur.unwrap_or(DEFAULT_MS) };
                            starts.push((s, end, true));
                        }
                    }
                    TaskTrigger::After { .. } | TaskTrigger::Boot { .. } | TaskTrigger::Manual => {}
                },
                // a PC's backup plan: the agent's own rules (S6, `rynas_schedule::window`):
                // due every so many hours after the last good run (its start for agents from
                // 0.8.24, its end before), started only inside its allowed window (drawn as
                // its start band)
                (None, Some(h)) if h > 0 => {
                    let every = u64::from(h) * 3_600_000;
                    let first = j.interval_from_ms.or(j.last_ms).map(|l| l + every).unwrap_or(now).max(now.min(to));
                    let window = job.window.as_deref().filter(|w| week_window_valid(w));
                    let (s, cut) = rynas_schedule::window::interval_starts(first, every, dur.unwrap_or(0), j.from_start, window, tz, to, MAX_RUNS_PER_JOB);
                    job.truncated = cut;
                    starts.extend(s.into_iter().filter(|st| *st >= from).map(|st| (st, st + dur.unwrap_or(DEFAULT_MS), dur.is_some())));
                    if let Some(w) = window {
                        for (s, e) in blocks(w, tz, from, to) {
                            windows.push(OverviewWindow { kind: OverviewWindowKind::Start, job: Some(job.id.clone()), start_ms: s, end_ms: e });
                        }
                    }
                }
                // a PC's plan at set times (agents from 0.8.21): the calendar's first time after
                // the last good backup (a missed one at once), each next one after the run's
                // end, inside the window ("also at every start of the PC" is not foreseeable)
                (None, None) => {
                    let own_window = job.window.clone();
                    if let Some(TaskTrigger::Calendar { on_calendar, .. }) = job.trigger.clone()
                        && let Ok(spec) = CalendarSpec::parse(&on_calendar)
                    {
                        let window = own_window.as_deref().filter(|w| week_window_valid(w));
                        let first = rynas_schedule::window::calendar_due(&spec, tz, j.last_ms, 0, 0).max(now.min(to));
                        let (s, cut) = rynas_schedule::window::calendar_starts(&spec, first, dur.unwrap_or(0), window, tz, to, MAX_RUNS_PER_JOB);
                        job.truncated = cut;
                        starts.extend(s.into_iter().filter(|st| *st >= from).map(|st| (st, st + dur.unwrap_or(DEFAULT_MS), dur.is_some())));
                    }
                    if let Some(w) = job.window.as_deref().filter(|w| week_window_valid(w))
                        && job.trigger.is_some()
                    {
                        for (s, e) in blocks(w, tz, from, to) {
                            windows.push(OverviewWindow { kind: OverviewWindowKind::Start, job: Some(job.id.clone()), start_ms: s, end_ms: e });
                        }
                    }
                }
                _ => {}
            }
        }
        // a start whose time comes while an earlier one of the job waits for the window: the
        // unit is starting already, systemd does not start it again (merged into that one)
        let mut waiting_until: Option<u64> = None;
        for (st, end, estimated) in starts {
            let mut run = OverviewRun {
                job: job.id.clone(),
                start_ms: st,
                end_ms: end.max(st + 60_000),
                estimated,
                deferred_to_ms: None,
                skipped: false,
                merged: false,
            };
            let mut eff_start = Some(st);
            if job.maintenance
                && let Some(t) = &j.task
            {
                match defer(window, t, RunTrigger::Timer, st, st, tz) {
                    Defer::Go => {}
                    Defer::Until(o) if waiting_until == Some(o) => {
                        run.deferred_to_ms = Some(o);
                        run.merged = true;
                        eff_start = None;
                    }
                    Defer::Until(o) => {
                        run.deferred_to_ms = Some(o);
                        eff_start = Some(o);
                        waiting_until = Some(o);
                    }
                    Defer::Skip(_) => {
                        run.skipped = true;
                        eff_start = None;
                    }
                }
            }
            if let Some(s) = eff_start {
                effs.push(Eff { job: i, start: s, end: s + (run.end_ms - run.start_ms) });
            }
            runs.push(run);
        }
        out_jobs.push(job);
    }
    runs.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then_with(|| a.job.cmp(&b.job)));
    windows.sort_by_key(|w| w.start_ms);
    let conflicts = conflicts(&jobs, &runs, &effs, tz, with_day, disks_of);
    SchedulerOverview {
        from_ms: from,
        to_ms: to,
        zone: tz.iana_name().unwrap_or_default().to_string(),
        maintenance: window.map(str::to_string),
        paused: None,
        jobs: out_jobs,
        runs,
        windows,
        conflicts,
    }
}

fn volume_meet<'a>(a: &'a [Claim], b: &[Claim]) -> Option<&'a Claim> {
    a.iter().find(|c| match c {
        Claim::Volume(k) => b.iter().any(|d| matches!(d, Claim::Volume(l) if l == k || l == EVERY || k == EVERY)),
        _ => false,
    })
}

fn disk_meet(test: &[Claim], heavy: &[Claim], disks_of: &dyn Fn(&str) -> Vec<String>) -> Option<String> {
    for c in test {
        let Claim::Disk(d) = c else { continue };
        for h in heavy {
            if let Claim::Volume(k) = h
                && (k == EVERY || disks_of(k).iter().any(|x| x == d))
            {
                return Some(d.clone());
            }
        }
    }
    None
}

fn resource_of(j: &JobInput, c: &Claim) -> Option<Resource> {
    j.claims.iter().position(|x| x == c).and_then(|i| j.resources.get(i).cloned())
}

fn is_disk_test(a: Option<&TaskAction>) -> bool {
    matches!(a, Some(TaskAction::SmartTest { .. } | TaskAction::MdCheck { .. }))
}

fn conflicts(
    jobs: &[JobInput],
    runs: &[OverviewRun],
    effs: &[Eff],
    tz: &TimeZone,
    with_day: bool,
    disks_of: &dyn Fn(&str) -> Vec<String>,
) -> Vec<OverviewConflict> {
    let name = |i: usize| jobs[i].job.name.clone();
    let est = |i: usize| jobs[i].job.estimate_ms.map(|d| format!(" ({})", about(d))).unwrap_or_default();
    let index: std::collections::HashMap<&str, usize> = jobs.iter().enumerate().map(|(i, j)| (j.job.id.as_str(), i)).collect();
    // heavy work first (each kind has its own cap: many starts outside the window never hide
    // two jobs on one volume): the starts that claim something, by effective start
    let mut pairs: Vec<OverviewConflict> = Vec::new();
    let mut heavy: Vec<&Eff> = effs
        .iter()
        .filter(|e| {
            let j = &jobs[e.job];
            !j.claims.is_empty() && !matches!(j.task.as_ref().map(|t| &t.action), Some(TaskAction::Dedup { .. }))
        })
        .collect();
    heavy.sort_by(|a, b| {
        a.start.cmp(&b.start).then_with(|| {
            let pa = jobs[a.job].job.action.as_ref().map_or(0, priority_of);
            let pb = jobs[b.job].job.action.as_ref().map_or(0, priority_of);
            pb.cmp(&pa)
        })
    });
    'outer: for (x, a) in heavy.iter().enumerate() {
        for b in heavy.iter().skip(x + 1) {
            // sorted by start: nothing later overlaps `a` either
            if b.start >= a.end {
                break;
            }
            if pairs.len() >= MAX_CONFLICTS {
                break 'outer;
            }
            if a.job == b.job {
                continue;
            }
            let (ja, jb) = (&jobs[a.job], &jobs[b.job]);
            let (aa, ab) = (ja.job.action.as_ref(), jb.job.action.as_ref());
            let (pa, pb) = (aa.map_or(0, priority_of), ab.map_or(0, priority_of));
            let at = when(tz, b.start, with_day);
            if let Some(c) = volume_meet(&ja.claims, &jb.claims) {
                let res = resource_of(ja, c);
                let on = res.as_ref().map(|r| format!(" on {}", resource_text(r))).unwrap_or_default();
                let (outcome, affected, tail) = if matches!(aa, Some(TaskAction::Scrub { .. })) && pb > pa {
                    (ConflictOutcome::GivesWay, a.job, format!("{} will give way and resume after it", name(a.job)))
                } else {
                    (ConflictOutcome::Waits, b.job, format!("{} will wait", name(b.job)))
                };
                pairs.push(OverviewConflict {
                    kind: ConflictKind::Volume,
                    outcome,
                    first: ja.job.id.clone(),
                    first_ms: a.start,
                    second: Some(jb.job.id.clone()),
                    second_ms: Some(b.start),
                    affected: jobs[affected].job.id.clone(),
                    resource: res,
                    opens_ms: None,
                    sentence: format!("{}{} overlaps {} at {at}{on}; {tail}", name(a.job), est(a.job), name(b.job)),
                });
                continue;
            }
            // a disk test against heavy work on a volume of its disk
            let pair = if is_disk_test(aa) && !is_disk_test(ab) {
                disk_meet(&ja.claims, &jb.claims, disks_of).map(|d| (d, true))
            } else if is_disk_test(ab) && !is_disk_test(aa) {
                disk_meet(&jb.claims, &ja.claims, disks_of).map(|d| (d, false))
            } else {
                None
            };
            if let Some((disk, test_first)) = pair {
                let (outcome, affected, tail) = if test_first && a.start < b.start {
                    (ConflictOutcome::BothRun, b.job, "both run, more slowly".to_string())
                } else {
                    let t = if test_first { a.job } else { b.job };
                    (ConflictOutcome::Waits, t, format!("{} will wait", name(t)))
                };
                pairs.push(OverviewConflict {
                    kind: ConflictKind::Disk,
                    outcome,
                    first: ja.job.id.clone(),
                    first_ms: a.start,
                    second: Some(jb.job.id.clone()),
                    second_ms: Some(b.start),
                    affected: jobs[affected].job.id.clone(),
                    resource: Some(Resource::Disk { disk: disk.clone() }),
                    opens_ms: None,
                    sentence: format!("{}{} overlaps {} at {at} on disk {disk}; {tail}", name(a.job), est(a.job), name(b.job)),
                });
            }
        }
    }
    // outside the maintenance window (a start merged into an earlier waiting one is no start)
    let mut window: Vec<OverviewConflict> = Vec::new();
    for r in runs.iter().filter(|r| !r.merged) {
        if window.len() >= MAX_CONFLICTS {
            break;
        }
        let Some(&i) = index.get(r.job.as_str()) else { continue };
        let (outcome, opens) = match (r.deferred_to_ms, r.skipped) {
            (Some(o), _) => (ConflictOutcome::Deferred, Some(o)),
            (None, true) => (ConflictOutcome::Skipped, None),
            _ => continue,
        };
        let sentence = match opens {
            Some(o) => format!(
                "{} at {} is outside the maintenance window; it will wait until {}",
                name(i),
                when(tz, r.start_ms, with_day),
                when(tz, o, true)
            ),
            None => format!(
                "{} at {} is outside the maintenance window, which opens only after its deadline; it will be skipped",
                name(i),
                when(tz, r.start_ms, with_day)
            ),
        };
        window.push(OverviewConflict {
            kind: ConflictKind::Maintenance,
            outcome,
            first: r.job.clone(),
            first_ms: r.start_ms,
            second: None,
            second_ms: None,
            affected: r.job.clone(),
            resource: None,
            opens_ms: opens,
            sentence,
        });
    }
    pairs.extend(window);
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::scheduler::{OverviewOwner, RunPolicy, SmartTestKind};

    #[test]
    fn blocks_of_a_range_at_the_end_of_time_end() {
        let all = "1".repeat(168);
        let (from, to) = (u64::MAX - 10, u64::MAX - 5);
        assert_eq!(blocks(&all, &TimeZone::UTC, from, to), vec![(from, to)]);
    }

    fn ms(s: &str, tz: &TimeZone) -> u64 {
        let dt: DateTime = s.parse().unwrap();
        tz.to_ambiguous_zoned(dt).compatible().unwrap().timestamp().as_millisecond() as u64
    }

    fn task(id: &str, action: TaskAction, cal: &str) -> ScheduledTask {
        ScheduledTask {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            enabled: true,
            action,
            trigger: TaskTrigger::Calendar { on_calendar: cal.into(), cron: None, persistent: true, randomized_delay_s: 0 },
            timeout_s: None,
            nice: None,
            io_weight: None,
            notify: Default::default(),
            notify_output: false,
            owner: Default::default(),
            policy: RunPolicy::default(),
        }
    }

    fn input(t: ScheduledTask, name: &str, est_h: Option<u64>, claims: Vec<Claim>, res: Vec<Resource>) -> JobInput {
        JobInput {
            job: OverviewJob {
                id: t.id.clone(),
                name: name.into(),
                owner: OverviewOwner::User,
                action: Some(t.action.clone()),
                enabled: true,
                system: false,
                paused: false,
                estimate_ms: est_h.map(|h| h * 3_600_000),
                estimate_runs: est_h.map_or(0, |_| 10),
                resources: res.clone(),
                maintenance: t.follows_maintenance(),
                trigger: Some(t.trigger.clone()),
                every_h: None,
                window: None,
                truncated: false,
            },
            task: Some(t),
            claims,
            resources: res,
            last_ms: None,
            interval_from_ms: None,
            from_start: true,
            next_ms: None,
        }
    }

    fn vol(p: &str) -> Resource {
        Resource::Volume { path: p.into() }
    }

    #[test]
    fn a_long_scrub_and_a_nightly_backup() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let from = ms("2026-11-17T00:00", &tz);
        let to = from + 86_400_000;
        let scrub = input(task("scrub-data", TaskAction::Scrub { filesystem: "/srv/data".into() }, "*-*-* 22:00"), "Scrub of data", Some(9), vec![Claim::Volume("0:40".into())], vec![vol("/srv/data")]);
        let backup = input(task("backup-offsite", TaskAction::Backup { job: "offsite".into() }, "*-*-* 02:00"), "the backup offsite", Some(1), vec![Claim::Volume("0:40".into())], vec![vol("/srv/data")]);
        // the range starts on the 17th: the scrub at 22:00, the backup at 02:00 the next day is out of range;
        // ask two days
        let o = build(vec![scrub, backup], None, &tz, from, to + 86_400_000, from, &|_| vec![]);
        assert_eq!(o.runs.len(), 4);
        let c: Vec<&OverviewConflict> = o.conflicts.iter().filter(|c| c.kind == ConflictKind::Volume).collect();
        assert_eq!(c.len(), 1, "{:?}", o.conflicts);
        // a backup is more urgent: the scrub gives way
        assert_eq!(c[0].outcome, ConflictOutcome::GivesWay);
        assert_eq!(c[0].affected, "scrub-data");
        assert_eq!(c[0].sentence, "Scrub of data (≈9 h) overlaps the backup offsite at Wed 02:00 on /srv/data; Scrub of data will give way and resume after it");
    }

    /// S6: a PC's backup plan with an allowed window starts only inside it (the agent's rules)
    /// and draws the window as its start band; without one, every interval from the previous
    /// start (agents from 0.8.24; older ones from the previous run's end).
    #[test]
    fn a_pc_plan_with_an_allowed_window() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let from = ms("2026-11-17T12:00", &tz);
        let to = ms("2026-11-20T00:00", &tz);
        let mut w = vec![b'0'; 168];
        for d in 0..7 {
            for h in 22..24 {
                w[d * 24 + h] = b'1';
            }
        }
        let pc = |window: Option<String>| JobInput {
            job: OverviewJob {
                id: "pc:laptop".into(),
                name: "laptop".into(),
                owner: OverviewOwner::PcBackup,
                action: None,
                enabled: true,
                system: false,
                paused: false,
                estimate_ms: Some(1_800_000),
                estimate_runs: 3,
                resources: vec![],
                maintenance: false,
                trigger: None,
                every_h: Some(6),
                window,
                truncated: false,
            },
            task: None,
            claims: vec![],
            resources: vec![],
            last_ms: Some(ms("2026-11-17T09:00", &tz)),
            interval_from_ms: None,
            from_start: true,
            next_ms: None,
        };
        let o = build(vec![pc(Some(String::from_utf8(w).unwrap()))], None, &tz, from, to, from, &|_| vec![]);
        let starts: Vec<u64> = o.runs.iter().map(|r| r.start_ms).collect();
        assert_eq!(starts, vec![ms("2026-11-17T22:00", &tz), ms("2026-11-18T22:00", &tz), ms("2026-11-19T22:00", &tz)]);
        let bands: Vec<(u64, u64)> = o.windows.iter().filter(|x| x.kind == OverviewWindowKind::Start).map(|x| (x.start_ms, x.end_ms)).collect();
        assert_eq!(bands.len(), 3);
        assert_eq!(bands[0], (ms("2026-11-17T22:00", &tz), ms("2026-11-18T00:00", &tz)));
        let o = build(vec![pc(None)], None, &tz, from, ms("2026-11-18T00:00", &tz), from, &|_| vec![]);
        let starts: Vec<u64> = o.runs.iter().map(|r| r.start_ms).collect();
        // every 6 h from the previous start (the agent counts from the run's start)
        assert_eq!(starts, vec![ms("2026-11-17T15:00", &tz), ms("2026-11-17T21:00", &tz)]);
        assert!(o.windows.is_empty());
        let until = ms("2026-11-18T00:00", &tz);
        let starts = |j: JobInput| build(vec![j], None, &tz, from, until, from, &|_| vec![]).runs.iter().map(|r| r.start_ms).collect::<Vec<u64>>();
        // the last good run started at 08:30 (an agent that counts from the start): 14:30 first
        let mut j = pc(None);
        j.interval_from_ms = Some(ms("2026-11-17T08:30", &tz));
        assert_eq!(starts(j), vec![ms("2026-11-17T14:30", &tz), ms("2026-11-17T20:30", &tz)]);
        // an agent before 0.8.24: each next one an interval after the previous run's end
        let mut j = pc(None);
        j.from_start = false;
        assert_eq!(starts(j), vec![ms("2026-11-17T15:00", &tz), ms("2026-11-17T21:30", &tz)]);
    }

    /// A PC's plan at set times: a missed time at once, then the calendar's times.
    #[test]
    fn a_pc_plan_at_set_times() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let from = ms("2026-11-17T12:00", &tz);
        let pc = |trigger: TaskTrigger| JobInput {
            job: OverviewJob {
                id: "pc:laptop".into(),
                name: "laptop".into(),
                owner: OverviewOwner::PcBackup,
                action: None,
                enabled: true,
                system: false,
                paused: false,
                estimate_ms: Some(600_000),
                estimate_runs: 3,
                resources: vec![],
                maintenance: false,
                trigger: Some(trigger),
                every_h: None,
                window: None,
                truncated: false,
            },
            task: None,
            claims: vec![],
            resources: vec![],
            last_ms: Some(ms("2026-11-16T02:10", &tz)),
            interval_from_ms: None,
            from_start: true,
            next_ms: None,
        };
        let daily = TaskTrigger::Calendar { on_calendar: "*-*-* 02:00:00".into(), cron: None, persistent: true, randomized_delay_s: 0 };
        let o = build(vec![pc(daily)], None, &tz, from, ms("2026-11-19T12:00", &tz), from, &|_| vec![]);
        let starts: Vec<u64> = o.runs.iter().map(|r| r.start_ms).collect();
        assert_eq!(starts, vec![from, ms("2026-11-18T02:00", &tz), ms("2026-11-19T02:00", &tz)]);
    }

    #[test]
    fn the_less_urgent_waits_and_disk_tests() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let from = ms("2026-11-17T00:00", &tz);
        let to = from + 86_400_000 - 1;
        let backup = input(task("backup-offsite", TaskAction::Backup { job: "offsite".into() }, "*-*-* 02:00"), "Backup offsite", Some(3), vec![Claim::Volume("0:40".into())], vec![vol("/srv/data")]);
        let scrub = input(task("scrub-data", TaskAction::Scrub { filesystem: "/srv/data".into() }, "*-*-* 03:00"), "Scrub of data", None, vec![Claim::Volume("0:40".into())], vec![vol("/srv/data")]);
        let smart = input(
            task("smart-sda", TaskAction::SmartTest { disk: "sda".into(), test: SmartTestKind::Long }, "*-*-* 04:00"),
            "SMART sda",
            Some(2),
            vec![Claim::Disk("sda".into())],
            vec![Resource::Disk { disk: "sda".into() }],
        );
        let o = build(vec![backup, scrub, smart], None, &tz, from, to, from, &|k| if k == "0:40" { vec!["sda".into()] } else { vec![] });
        let v = o.conflicts.iter().find(|c| c.kind == ConflictKind::Volume).unwrap();
        assert_eq!(v.outcome, ConflictOutcome::Waits);
        assert_eq!(v.affected, "scrub-data");
        assert_eq!(v.sentence, "Backup offsite (≈3 h) overlaps Scrub of data at 03:00 on /srv/data; Scrub of data will wait");
        let d = o.conflicts.iter().find(|c| c.kind == ConflictKind::Disk).unwrap();
        assert_eq!(d.outcome, ConflictOutcome::Waits);
        assert_eq!(d.affected, "smart-sda");
        assert_eq!(d.resource, Some(Resource::Disk { disk: "sda".into() }));
    }

    #[test]
    fn outside_the_maintenance_window() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let from = ms("2026-11-17T00:00", &tz);
        let to = from + 86_400_000 - 1;
        let mut w = vec![b'0'; 168];
        for d in 0..7 {
            for h in 1..5 {
                w[d * 24 + h] = b'1';
            }
        }
        let w = String::from_utf8(w).unwrap();
        let backup = input(task("backup-day", TaskAction::Scrub { filesystem: "/srv/data".into() }, "*-*-* 14:00"), "Backup day", Some(1), vec![Claim::Volume("0:40".into())], vec![vol("/srv/data")]);
        let snap = input(task("snap", TaskAction::Snapshot { job: "s".into() }, "hourly"), "Snapshots", None, vec![], vec![]);
        let o = build(vec![backup, snap], Some(&w), &tz, from, to, from, &|_| vec![]);
        assert_eq!(o.windows.iter().filter(|x| x.kind == OverviewWindowKind::Maintenance).count(), 1);
        let r = o.runs.iter().find(|r| r.job == "backup-day").unwrap();
        assert_eq!(r.deferred_to_ms, Some(ms("2026-11-18T01:00", &tz)));
        let c = o.conflicts.iter().find(|c| c.kind == ConflictKind::Maintenance).unwrap();
        assert_eq!(c.outcome, ConflictOutcome::Deferred);
        assert_eq!(c.sentence, "Backup day at 14:00 is outside the maintenance window; it will wait until Wed 01:00");
        // snapshots never follow it
        assert!(o.runs.iter().filter(|r| r.job == "snap").all(|r| r.deferred_to_ms.is_none() && !r.skipped));
        assert_eq!(o.runs.iter().filter(|r| r.job == "snap").count(), 24);
    }

    /// Starts deferred to the same opening are one start (systemd does not start a unit that
    /// waits already), and they never crowd out the conflicts of two jobs on one volume.
    #[test]
    fn deferred_starts_merge_and_pairs_come_first() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let from = ms("2026-11-17T00:00", &tz);
        let to = from + 86_400_000 - 1;
        let mut w = vec![b'0'; 168];
        for d in 0..7 {
            for h in 1..5 {
                w[d * 24 + h] = b'1';
            }
        }
        let w = String::from_utf8(w).unwrap();
        let hourly = input(task("h", TaskAction::Scrub { filesystem: "/a".into() }, "hourly"), "h", None, vec![Claim::Volume("0:40".into())], vec![vol("/a")]);
        let other = input(task("o", TaskAction::Scrub { filesystem: "/a".into() }, "*-*-* 02:00"), "o", Some(1), vec![Claim::Volume("0:40".into())], vec![vol("/a")]);
        let o = build(vec![hourly, other], Some(&w), &tz, from, to, from, &|_| vec![]);
        // 00:00 waits for 01:00, 05:00 for tomorrow 01:00, 06:00–23:00 merged into it
        assert_eq!(o.runs.iter().filter(|r| r.job == "h" && r.merged).count(), 18);
        let m: Vec<_> = o.conflicts.iter().filter(|c| c.kind == ConflictKind::Maintenance).collect();
        assert_eq!(m.len(), 2, "{m:?}");
        // the two scrubs of /a at 02:00 are listed, and before the window's
        assert_eq!(o.conflicts[0].kind, ConflictKind::Volume);
    }

    #[test]
    fn dst_and_windows() {
        let tz = TimeZone::get("Europe/Rome").unwrap();
        // the spring-forward night: 02:30 does not exist and is skipped, as systemd does
        let from = ms("2026-03-29T00:00", &tz);
        let to = ms("2026-03-30T23:59", &tz);
        let t = input(task("x", TaskAction::Scrub { filesystem: "/a".into() }, "*-*-* 02:30"), "x", None, vec![], vec![]);
        let o = build(vec![t], None, &tz, from, to, from, &|_| vec![]);
        assert_eq!(o.runs.len(), 1);
        assert_eq!(o.runs[0].start_ms, ms("2026-03-30T02:30", &tz));
        assert!(!o.runs[0].estimated);
        // a weekly window job: one run per block, to the block's end
        let mut w = vec![b'0'; 168];
        // Monday 01:00–03:00
        w[24 + 1] = b'1';
        w[24 + 2] = b'1';
        let mut d = task("dd", TaskAction::Dedup { volume: "v".into() }, "x");
        d.trigger = TaskTrigger::Window { window: String::from_utf8(w).unwrap(), stop_at_end: true };
        let o = build(vec![input(d, "dd", None, vec![], vec![])], None, &tz, from, to, from, &|_| vec![]);
        assert_eq!(o.runs.len(), 1);
        assert_eq!(o.runs[0].end_ms - o.runs[0].start_ms, 2 * 3_600_000);
        assert_eq!(o.windows[0].kind, OverviewWindowKind::Work);
    }

    #[test]
    fn durations_in_words() {
        assert_eq!(about(25 * 60_000), "≈25 min");
        assert_eq!(about(9 * 3_600_000 + 10 * 60_000), "≈9 h 10 min");
        assert_eq!(about(12 * 3_600_000 + 40 * 60_000), "≈13 h");
        assert_eq!(about(2 * 3_600_000), "≈2 h");
    }
}
