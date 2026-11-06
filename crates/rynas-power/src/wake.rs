//! The RTC wake alarm and the boot record, used by `rynasctl power arm-wake` from
//! `rynas-power-wake.service` (`ExecStart=` at boot, `ExecStop=` at every shutdown) and read
//! by rynasd for the API. Works without rynasd: the rules come from the file the apply wrote.
//!
//! On a Synology the RTC is the S-35390A (I²C address 0x30); synohwd's shutdown hook reads
//! its `wakealarm` and tells the microcontroller to honour it (`s`) or not (`r`), so the
//! alarm must be written before synohwd stops (the unit is ordered `After=synohwd.service`,
//! hence stopped before it).

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use rynas_api_types::power::{
    BootCause, BootInfo, PowerSchedule, WakeArmError, WakeArmed, WakeSupport, WakeWarning,
};
use serde::{Deserialize, Serialize};

use crate::rules::wake_time;

pub const RTC_CLASS: &str = "/sys/class/rtc";
/// State of `rynasctl power arm-wake` below rynasd's state directory.
pub const STATE_FILE: &str = "power/state.json";

fn under(root: &Path, p: &str) -> PathBuf {
    root.join(p.trim_start_matches('/'))
}

/// The RTC to arm: the Synology S-35390A (`device` → `…-0030`) when present, else `rtc0`,
/// else the first one.
pub fn find_rtc(root: &Path) -> Option<String> {
    let dir = under(root, RTC_CLASS);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("rtc") && n[3..].bytes().all(|b| b.is_ascii_digit()))
        .collect();
    names.sort();
    let s35390 = names.iter().find(|n| {
        let dev = dir.join(n).join("device");
        let target = std::fs::canonicalize(&dev)
            .ok()
            .or_else(|| std::fs::read_link(&dev).ok());
        target
            .and_then(|t| t.file_name().map(|f| f.to_string_lossy().into_owned()))
            .is_some_and(|f| f.ends_with("-0030"))
    });
    s35390
        .or_else(|| names.iter().find(|n| *n == "rtc0"))
        .or_else(|| names.first())
        .cloned()
}

/// The alarm in the RTC (epoch seconds), `None` when unset or unreadable.
pub fn read_wakealarm(root: &Path, rtc: &str) -> Option<u64> {
    let p = under(root, RTC_CLASS).join(rtc).join("wakealarm");
    std::fs::read_to_string(p)
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|&v| v > 0)
}

pub fn has_wakealarm(root: &Path, rtc: &str) -> bool {
    under(root, RTC_CLASS).join(rtc).join("wakealarm").exists()
}

/// What the API says about waking: RTC, current alarm, last arming, warning.
pub fn support(root: &Path, state: &PowerState) -> WakeSupport {
    let rtc = find_rtc(root);
    let supported = rtc.as_deref().is_some_and(|r| has_wakealarm(root, r));
    let (warning_code, warning) = match &rtc {
        None => (
            Some(WakeWarning::NoRtc),
            Some("no real-time clock found: the NAS cannot start itself on schedule".into()),
        ),
        Some(r) if !supported => (
            Some(WakeWarning::NoWakeAlarm),
            Some(format!(
                "{r} has no wake alarm: the NAS cannot start itself on schedule"
            )),
        ),
        Some(_) => {
            let e = state.wake.as_ref().and_then(|w| w.error.as_ref());
            (
                e.map(|_| WakeWarning::LastArmFailed),
                e.map(|e| format!("the last arming of the wake alarm failed: {e}")),
            )
        }
    };
    WakeSupport {
        supported,
        armed_ms: rtc
            .as_deref()
            .and_then(|r| read_wakealarm(root, r))
            .map(|s| s * 1000),
        rtc,
        last_armed: state.wake.clone(),
        warning,
        warning_code,
    }
}

/// `rtcwake` arguments: set the alarm without suspending (`-m no`), or clear it.
pub fn rtcwake_args(rtc: &str, wake_s: Option<i64>) -> Vec<String> {
    match wake_s {
        Some(t) => vec![
            "-d".into(),
            rtc.into(),
            "-m".into(),
            "no".into(),
            "-t".into(),
            t.to_string(),
        ],
        None => vec!["-d".into(), rtc.into(), "-m".into(), "disable".into()],
    }
}

/// The same through sysfs, for machines without `rtcwake`: the kernel
/// refuses a new alarm while one is set, so it is cleared first.
pub fn sysfs_arm(root: &Path, rtc: &str, wake_s: Option<i64>) -> Result<(), String> {
    let p = under(root, RTC_CLASS).join(rtc).join("wakealarm");
    let w = |v: &str| std::fs::write(&p, v).map_err(|e| format!("{}: {e}", p.display()));
    w("0")?;
    match wake_s {
        Some(t) => w(&t.to_string()),
        None => Ok(()),
    }
}

/// Persistent state (root 0600).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PowerState {
    /// `boot_id` of the boot that wrote `boot`.
    #[serde(default)]
    pub boot_id: Option<String>,
    #[serde(default)]
    pub boot: Option<BootInfo>,
    /// `boot_id` of the boot that ended with a clean shutdown (`ExecStop=`).
    #[serde(default)]
    pub clean_shutdown_boot_id: Option<String>,
    #[serde(default)]
    pub wake: Option<WakeArmed>,
}

impl PowerState {
    pub fn load(state_dir: &Path) -> PowerState {
        std::fs::read(state_dir.join(STATE_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, state_dir: &Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = state_dir.join(STATE_FILE);
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    }
}

/// The RTC alarm fired this boot when the boot began from 60 s before to 10 min after it.
pub const RTC_BOOT_WINDOW: (i64, i64) = (-60_000, 600_000);

/// How the boot that started at `boot_ms` began, from the previous state.
pub fn boot_info(prev: &PowerState, boot_ms: u64, watchdog_reset: bool) -> BootInfo {
    let rtc = prev.wake.as_ref().and_then(|w| w.wake_ms).is_some_and(|w| {
        let d = boot_ms as i64 - w as i64;
        (RTC_BOOT_WINDOW.0..=RTC_BOOT_WINDOW.1).contains(&d)
    });
    BootInfo {
        boot_ms,
        cause: if watchdog_reset {
            BootCause::Watchdog
        } else if rtc {
            BootCause::ScheduledStartup
        } else {
            BootCause::Unknown
        },
        previous_shutdown_clean: prev
            .boot_id
            .as_ref()
            .map(|b| prev.clean_shutdown_boot_id.as_ref() == Some(b)),
    }
}

/// The last reset was done by a watchdog (`bootstatus` & `WDIOF_CARDRESET`, as synodeb's
/// boot-report reads it).
pub fn watchdog_reset(root: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(under(root, "/sys/class/watchdog")) else {
        return false;
    };
    rd.flatten().any(|e| {
        std::fs::read_to_string(e.path().join("bootstatus"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_some_and(|b| b & 0x20 != 0)
    })
}

pub fn boot_id(root: &Path) -> Option<String> {
    std::fs::read_to_string(under(root, "/proc/sys/kernel/random/boot_id"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Seconds since boot (`/proc/uptime`).
pub fn uptime_s(root: &Path) -> Option<f64> {
    std::fs::read_to_string(under(root, "/proc/uptime"))
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// When `rynasctl power arm-wake` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmReason {
    /// `ExecStart=`: at boot (or a restart by an apply).
    Boot,
    /// `ExecStop=`: at every shutdown (or the stop half of a restart).
    Shutdown,
}

/// Sets the alarm of an RTC (`rtc0`) to an epoch second, or clears it (`None`).
pub type SetAlarm<'a> = dyn Fn(&str, Option<i64>) -> Result<(), String> + 'a;

pub struct ArmInput<'a> {
    pub root: &'a Path,
    pub state_dir: &'a Path,
    /// The applied schedule (`None`: no schedule file, the alarm is cleared).
    pub schedule: Option<&'a PowerSchedule>,
    pub now: Timestamp,
    pub tz: &'a TimeZone,
    pub reason: ArmReason,
}

/// Arms (or clears) the alarm and updates the state; `run(rtc, wake)` sets the alarm of `rtc`
/// to `wake` (epoch seconds) or clears it (`rtcwake`, see [`rtcwake_args`], or
/// [`sysfs_arm`]). Never fails the unit: problems end up in `WakeArmed::error` and the log.
pub fn arm(i: &ArmInput, run: &SetAlarm<'_>) -> WakeArmed {
    let mut st = PowerState::load(i.state_dir);
    let now_ms = i.now.as_millisecond().max(0) as u64;
    let id = boot_id(i.root);
    match i.reason {
        ArmReason::Boot if id.is_some() && id != st.boot_id => {
            let boot_ms = uptime_s(i.root)
                .map(|u| now_ms.saturating_sub((u * 1000.0) as u64))
                .unwrap_or(now_ms);
            st.boot = Some(boot_info(&st, boot_ms, watchdog_reset(i.root)));
            st.boot_id = id.clone();
            st.clean_shutdown_boot_id = None;
        }
        // a restart within the same boot: its stop half was not a shutdown
        ArmReason::Boot => st.clean_shutdown_boot_id = None,
        ArmReason::Shutdown => st.clean_shutdown_boot_id = id.clone(),
    }
    let plan = i.schedule.and_then(|s| wake_time(&s.rules, i.now, i.tz));
    let rtc = find_rtc(i.root);
    let mut armed = WakeArmed {
        at_ms: now_ms,
        wake_ms: plan.as_ref().map(|(t, _)| (*t as u64) * 1000),
        rule: plan.as_ref().map(|(_, r)| r.clone()),
        rtc: rtc.clone(),
        reason: match i.reason {
            ArmReason::Boot => "boot",
            ArmReason::Shutdown => "shutdown",
        }
        .into(),
        error: None,
        error_code: None,
    };
    let failure = match rtc.as_deref() {
        Some(r) if has_wakealarm(i.root, r) => run(r, plan.as_ref().map(|(t, _)| *t))
            .err()
            .map(|e| (WakeArmError::Failed, e)),
        Some(r) if plan.is_some() => {
            Some((WakeArmError::NoWakeAlarm, format!("{r} has no wake alarm")))
        }
        None if plan.is_some() => Some((WakeArmError::NoRtc, "no real-time clock".into())),
        _ => None,
    };
    if let Some((code, e)) = failure {
        armed.error_code = Some(code);
        armed.error = Some(e);
    }
    st.wake = Some(armed.clone());
    if let Err(e) = st.save(i.state_dir) {
        tracing::warn!("power state: {e}");
    }
    armed
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use rynas_api_types::power::{PowerAction, PowerRule, Weekday};

    fn fake(root: &Path, boot_id: &str, uptime: &str) {
        std::fs::create_dir_all(root.join("proc/sys/kernel/random")).unwrap();
        std::fs::write(root.join("proc/sys/kernel/random/boot_id"), boot_id).unwrap();
        std::fs::write(root.join("proc/uptime"), uptime).unwrap();
    }

    fn rtc(root: &Path, name: &str, device: &str, wakealarm: bool) {
        let d = root.join("sys/class/rtc").join(name);
        std::fs::create_dir_all(&d).unwrap();
        let dev = root.join("sys/devices").join(device);
        std::fs::create_dir_all(&dev).unwrap();
        std::os::unix::fs::symlink(&dev, d.join("device")).unwrap();
        if wakealarm {
            std::fs::write(d.join("wakealarm"), "").unwrap();
        }
    }

    fn schedule() -> PowerSchedule {
        PowerSchedule {
            rules: vec![PowerRule {
                id: "work".into(),
                enabled: true,
                action: PowerAction::Startup,
                days: Weekday::ALL.to_vec(),
                hour: 7,
                minute: 0,
                shutdown: None,
            }],
        }
    }

    #[test]
    fn rtc_choice() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(find_rtc(d.path()), None);
        rtc(d.path(), "rtc0", "pnp0/00:02", true);
        assert_eq!(find_rtc(d.path()).as_deref(), Some("rtc0"));
        // the Synology S-35390A wins over the CMOS clock
        rtc(d.path(), "rtc1", "i2c-0/0-0030", true);
        assert_eq!(find_rtc(d.path()).as_deref(), Some("rtc1"));
        std::fs::write(
            d.path().join("sys/class/rtc/rtc1/wakealarm"),
            "1793862000\n",
        )
        .unwrap();
        assert_eq!(read_wakealarm(d.path(), "rtc1"), Some(1_793_862_000));
        assert_eq!(read_wakealarm(d.path(), "rtc0"), None);
    }

    #[test]
    fn args() {
        assert_eq!(
            rtcwake_args("rtc0", Some(1_793_862_000)),
            ["-d", "rtc0", "-m", "no", "-t", "1793862000"]
        );
        assert_eq!(rtcwake_args("rtc1", None), ["-d", "rtc1", "-m", "disable"]);
    }

    #[test]
    fn arm_boot_shutdown_and_boot_cause() {
        let d = tempfile::tempdir().unwrap();
        let (root, state) = (d.path().join("root"), d.path().join("state"));
        rtc(&root, "rtc0", "pnp0/00:02", true);
        fake(&root, "boot-a", "100.00 50.00");
        let tz = TimeZone::get("Europe/Rome").unwrap();
        let s = schedule();
        let calls = RefCell::new(Vec::<Vec<String>>::new());
        let run = |r: &str, w: Option<i64>| {
            calls.borrow_mut().push(rtcwake_args(r, w));
            Ok(())
        };
        let now: Timestamp = "2026-11-04T21:00:00Z".parse().unwrap();
        let input = |now, reason| ArmInput {
            root: &root,
            state_dir: &state,
            schedule: Some(&s),
            now,
            tz: &tz,
            reason,
        };
        // first boot: unknown cause, unknown previous shutdown
        let a = arm(&input(now, ArmReason::Boot), &run);
        let wake: Timestamp = "2026-11-05T06:00:00Z".parse().unwrap(); // 07:00 CET
        assert_eq!(a.wake_ms, Some(wake.as_millisecond() as u64));
        assert_eq!(
            calls.borrow()[0],
            rtcwake_args("rtc0", Some(wake.as_second()))
        );
        let st = PowerState::load(&state);
        assert_eq!(st.boot.as_ref().unwrap().cause, BootCause::Unknown);
        assert_eq!(st.boot.as_ref().unwrap().previous_shutdown_clean, None);
        // a restart by an apply (stop + start, same boot) does not count as a clean shutdown
        arm(&input(now, ArmReason::Shutdown), &run);
        arm(&input(now, ArmReason::Boot), &run);
        assert_eq!(PowerState::load(&state).clean_shutdown_boot_id, None);
        // shutdown at 23:30, boot by the RTC at 07:00 (kernel up 20 s later)
        let late: Timestamp = "2026-11-04T22:30:00Z".parse().unwrap();
        arm(&input(late, ArmReason::Shutdown), &run);
        fake(&root, "boot-b", "40.0 10.0");
        let up = wake + jiff::SignedDuration::from_secs(60);
        arm(&input(up, ArmReason::Boot), &run);
        let st = PowerState::load(&state);
        let b = st.boot.unwrap();
        assert_eq!(b.cause, BootCause::ScheduledStartup);
        assert_eq!(b.previous_shutdown_clean, Some(true));
        assert_eq!(b.boot_ms, (up.as_millisecond() - 40_000) as u64);
        // a crash (no ExecStop), then a boot by the power button in the afternoon
        fake(&root, "boot-c", "30.0 10.0");
        let pm: Timestamp = "2026-11-05T14:00:00Z".parse().unwrap();
        arm(&input(pm, ArmReason::Boot), &run);
        let b = PowerState::load(&state).boot.unwrap();
        assert_eq!(
            (b.cause, b.previous_shutdown_clean),
            (BootCause::Unknown, Some(false))
        );
        // watchdog reset wins
        std::fs::create_dir_all(root.join("sys/class/watchdog/watchdog0")).unwrap();
        std::fs::write(root.join("sys/class/watchdog/watchdog0/bootstatus"), "32\n").unwrap();
        fake(&root, "boot-d", "30.0 10.0");
        arm(&input(pm, ArmReason::Boot), &run);
        assert_eq!(
            PowerState::load(&state).boot.unwrap().cause,
            BootCause::Watchdog
        );
    }

    #[test]
    fn no_rules_clears_and_no_rtc_warns() {
        let d = tempfile::tempdir().unwrap();
        let (root, state) = (d.path().join("root"), d.path().join("state"));
        let tz = TimeZone::UTC;
        let calls = RefCell::new(Vec::<Vec<String>>::new());
        let run = |r: &str, w: Option<i64>| {
            calls.borrow_mut().push(rtcwake_args(r, w));
            Ok(())
        };
        let s = schedule();
        let now: Timestamp = "2026-11-04T21:00:00Z".parse().unwrap();
        // no RTC at all: nothing run, error recorded, warning in the API
        let a = arm(
            &ArmInput {
                root: &root,
                state_dir: &state,
                schedule: Some(&s),
                now,
                tz: &tz,
                reason: ArmReason::Shutdown,
            },
            &run,
        );
        assert_eq!(a.error.as_deref(), Some("no real-time clock"));
        assert_eq!(a.error_code, Some(WakeArmError::NoRtc));
        assert!(calls.borrow().is_empty());
        let w = support(&root, &PowerState::load(&state));
        assert_eq!(w.warning_code, Some(WakeWarning::NoRtc));
        assert!(!w.supported && w.warning.unwrap().contains("no real-time clock"));
        // an RTC without wakealarm
        rtc(&root, "rtc0", "pnp0/00:02", false);
        let w = support(&root, &PowerState::default());
        assert_eq!(w.warning_code, Some(WakeWarning::NoWakeAlarm));
        assert!(!w.supported && w.warning.unwrap().contains("no wake alarm"));
        // with an alarm and no schedule: cleared
        std::fs::write(root.join("sys/class/rtc/rtc0/wakealarm"), "").unwrap();
        let a = arm(
            &ArmInput {
                root: &root,
                state_dir: &state,
                schedule: None,
                now,
                tz: &tz,
                reason: ArmReason::Shutdown,
            },
            &run,
        );
        assert_eq!((a.wake_ms, a.error, a.error_code), (None, None, None));
        assert_eq!(calls.borrow()[0], rtcwake_args("rtc0", None));
        // a failing rtcwake is recorded, never raised
        let a = arm(
            &ArmInput {
                root: &root,
                state_dir: &state,
                schedule: Some(&s),
                now,
                tz: &tz,
                reason: ArmReason::Shutdown,
            },
            &|_, _| Err("rtcwake: exit code 1".into()),
        );
        assert_eq!(a.error.as_deref(), Some("rtcwake: exit code 1"));
        assert_eq!(a.error_code, Some(WakeArmError::Failed));
        let w = support(&root, &PowerState::load(&state));
        assert_eq!(w.warning_code, Some(WakeWarning::LastArmFailed));
        assert!(w.supported && w.warning.unwrap().contains("failed"));
    }
}
