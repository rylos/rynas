//! Read-only questions to systemd, behind a trait (fake in tests): calendar previews
//! (`systemd-analyze calendar`), timer states (`systemctl list-timers -o json`), running
//! task units, timer calendars, start time of a run, and the output of a run from the
//! journal (`_SYSTEMD_INVOCATION_ID=`). Commands that change the system go through
//! [`rynas_config::System`] instead.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use rynas_tasks::Cmd;
use serde::Deserialize;

use crate::calendar::{Analyzed, parse_analyze};

/// A timer as `systemctl list-timers` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TimerState {
    pub unit: String,
    pub activates: String,
    pub next_ms: Option<u64>,
    pub last_ms: Option<u64>,
}

/// Output of a run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunOutput {
    pub text: String,
    /// Older output was dropped.
    pub truncated: bool,
}

pub trait SystemdProbe: Send + Sync {
    /// Analyzes one calendar expression; `Err` with systemd's message when it is invalid.
    fn analyze_calendar(&self, expr: &str, iterations: u32) -> Result<Analyzed, String>;
    /// Every loaded timer.
    fn timers(&self) -> Result<Vec<TimerState>, String>;
    /// `OnCalendar=` expressions of some timers.
    fn timer_calendars(&self, units: &[String]) -> Result<HashMap<String, Vec<String>>, String>;
    /// Services matching `rynas-task-*` that are running.
    fn running_tasks(&self) -> Result<Vec<String>, String>;
    /// When the main process of `unit`'s current/last run started.
    fn start_ms(&self, unit: &str) -> Option<u64>;
    /// The last `max_bytes` of a run's output.
    fn journal(&self, invocation_id: &str, max_bytes: usize) -> Result<RunOutput, String>;
    /// Asks journald to process and write what it received so far (before reading the
    /// output of a run that just ended).
    fn sync_journal(&self) {}
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The real systemd.
pub struct HostProbe;

fn run(cmd: Cmd) -> Result<rynas_tasks::CmdOutput, String> {
    let program = cmd.program().to_string_lossy().into_owned();
    cmd.env_clear()
        .timeout(PROBE_TIMEOUT)
        .output_blocking()
        .map_err(|e| format!("{program}: {e}"))
}

#[derive(Deserialize)]
struct JsonTimer {
    #[serde(default)]
    next: Option<u64>,
    #[serde(default)]
    last: Option<u64>,
    unit: String,
    #[serde(default)]
    activates: String,
}

#[derive(Deserialize)]
struct JsonUnit {
    unit: String,
    #[serde(default)]
    active: String,
}

fn us_to_ms(v: Option<u64>) -> Option<u64> {
    v.filter(|v| *v > 0 && *v != u64::MAX).map(|v| v / 1000)
}

/// Parses `systemctl list-timers --all --output=json`.
pub fn parse_timers(json: &str) -> Result<Vec<TimerState>, String> {
    let v: Vec<JsonTimer> = serde_json::from_str(json).map_err(|e| format!("list-timers: {e}"))?;
    Ok(v.into_iter()
        .map(|t| TimerState {
            unit: t.unit,
            activates: t.activates,
            next_ms: us_to_ms(t.next),
            last_ms: us_to_ms(t.last),
        })
        .collect())
}

/// Parses `systemctl show -p Id -p TimersCalendar <timers>`: blocks separated by blank lines,
/// `TimersCalendar={ OnCalendar=*-*-* 06,18:00:00 ; next_elapse=... }` (one per expression).
pub fn parse_timer_calendars(out: &str) -> HashMap<String, Vec<String>> {
    let mut res = HashMap::new();
    for block in out.split("\n\n") {
        let mut id = None;
        let mut cals = Vec::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("Id=") {
                id = Some(v.to_string());
            } else if let Some(v) = line.strip_prefix("TimersCalendar=")
                && let Some(rest) = v.trim().strip_prefix("{ OnCalendar=")
            {
                let expr = rest.split(" ;").next().unwrap_or("").trim();
                if !expr.is_empty() {
                    cals.push(expr.to_string());
                }
            }
        }
        if let Some(id) = id {
            res.insert(id, cals);
        }
    }
    res
}

/// The last `max` bytes of `text`, cut at a line start; whether something was dropped.
pub fn tail(text: &str, max: usize) -> RunOutput {
    if text.len() <= max {
        return RunOutput {
            text: text.to_string(),
            truncated: false,
        };
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if let Some(nl) = text[start..].find('\n') {
        start += nl + 1;
    }
    RunOutput {
        text: text[start..].to_string(),
        truncated: true,
    }
}

impl SystemdProbe for HostProbe {
    fn analyze_calendar(&self, expr: &str, iterations: u32) -> Result<Analyzed, String> {
        let o = run(Cmd::new("systemd-analyze")
            .arg("calendar")
            .arg(format!("--iterations={}", iterations.clamp(1, 50)))
            .arg("--")
            .arg(expr))?;
        if !o.success() {
            let msg = o
                .stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("invalid");
            return Err(msg.trim().to_string());
        }
        parse_analyze(&o.stdout)
            .into_iter()
            .next()
            .ok_or_else(|| "systemd-analyze printed nothing".into())
    }

    fn timers(&self) -> Result<Vec<TimerState>, String> {
        let o = run(Cmd::new("systemctl").args([
            "list-timers",
            "--all",
            "--output=json",
            "--no-pager",
        ]))?;
        if !o.success() {
            return Err(format!("systemctl list-timers: {}", o.stderr.trim()));
        }
        parse_timers(&o.stdout)
    }

    fn timer_calendars(&self, units: &[String]) -> Result<HashMap<String, Vec<String>>, String> {
        if units.is_empty() {
            return Ok(HashMap::new());
        }
        let o = run(Cmd::new("systemctl")
            .args([
                "show",
                "--no-pager",
                "-p",
                "Id",
                "-p",
                "TimersCalendar",
                "--",
            ])
            .args(units))?;
        Ok(parse_timer_calendars(&o.stdout))
    }

    fn running_tasks(&self) -> Result<Vec<String>, String> {
        let o = run(Cmd::new("systemctl").args([
            "list-units",
            "--all",
            "--output=json",
            "--no-pager",
            "--type=service",
            "rynas-task-*",
        ]))?;
        if !o.success() {
            return Err(format!("systemctl list-units: {}", o.stderr.trim()));
        }
        let v: Vec<JsonUnit> =
            serde_json::from_str(&o.stdout).map_err(|e| format!("list-units: {e}"))?;
        Ok(v.into_iter()
            .filter(|u| {
                matches!(
                    u.active.as_str(),
                    "active" | "activating" | "deactivating" | "reloading"
                )
            })
            .map(|u| u.unit)
            .collect())
    }

    fn start_ms(&self, unit: &str) -> Option<u64> {
        let o = run(Cmd::new("systemctl").args([
            "show",
            "--no-pager",
            "--timestamp=unix",
            "--value",
            "-p",
            "ExecMainStartTimestamp",
            "--",
            unit,
        ]))
        .ok()?;
        o.stdout
            .trim()
            .strip_prefix('@')
            .and_then(|s| s.parse::<u64>().ok())
            .map(|s| s * 1000)
    }

    fn journal(&self, invocation_id: &str, max_bytes: usize) -> Result<RunOutput, String> {
        if invocation_id.is_empty() || !invocation_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid invocation id".into());
        }
        // `-o cat`: messages only; `-n`: bounded, the runner keeps 1 MiB at most
        let o = run(Cmd::new("journalctl")
            .args(["--no-pager", "-o", "cat", "-n", "4000"])
            .arg(format!("_SYSTEMD_INVOCATION_ID={invocation_id}")))?;
        if !o.success() {
            return Err(format!("journalctl: {}", o.stderr.trim()));
        }
        Ok(tail(&o.stdout, max_bytes))
    }

    fn sync_journal(&self) {
        if let Err(e) = run(Cmd::new("journalctl").arg("--sync")) {
            tracing::debug!("journalctl --sync: {e}");
        }
    }
}

/// Tests: canned answers.
#[derive(Default)]
pub struct FakeProbe {
    pub state: Mutex<FakeProbeState>,
}

#[derive(Default, Debug, Clone)]
pub struct FakeProbeState {
    pub timers: Vec<TimerState>,
    pub calendars: HashMap<String, Vec<String>>,
    pub running: Vec<String>,
    pub journals: HashMap<String, String>,
    pub starts: HashMap<String, u64>,
    /// Expressions reported invalid.
    pub invalid: Vec<String>,
}

impl SystemdProbe for FakeProbe {
    fn analyze_calendar(&self, expr: &str, iterations: u32) -> Result<Analyzed, String> {
        if self.state.lock().unwrap().invalid.iter().any(|e| e == expr) {
            return Err(format!(
                "Failed to parse calendar specification '{expr}': Invalid argument"
            ));
        }
        Ok(Analyzed {
            original: expr.into(),
            normalized: format!("normalized {expr}"),
            next_ms: (1..=iterations as u64).map(|i| i * 3_600_000).collect(),
        })
    }

    fn timers(&self) -> Result<Vec<TimerState>, String> {
        Ok(self.state.lock().unwrap().timers.clone())
    }

    fn timer_calendars(&self, units: &[String]) -> Result<HashMap<String, Vec<String>>, String> {
        let s = self.state.lock().unwrap();
        Ok(units
            .iter()
            .filter_map(|u| s.calendars.get(u).map(|c| (u.clone(), c.clone())))
            .collect())
    }

    fn running_tasks(&self) -> Result<Vec<String>, String> {
        Ok(self.state.lock().unwrap().running.clone())
    }

    fn start_ms(&self, unit: &str) -> Option<u64> {
        self.state.lock().unwrap().starts.get(unit).copied()
    }

    fn journal(&self, invocation_id: &str, max_bytes: usize) -> Result<RunOutput, String> {
        let s = self.state.lock().unwrap();
        Ok(tail(
            s.journals.get(invocation_id).map_or("", String::as_str),
            max_bytes,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_timers() {
        let json = r#"[{"next":1793843571050549,"left":1793843571050549,"last":0,"passed":0,"unit":"systemd-tmpfiles-clean.timer","activates":"systemd-tmpfiles-clean.service"},{"next":null,"last":1793840000000000,"unit":"x.timer","activates":"x.service"}]"#;
        let t = parse_timers(json).unwrap();
        assert_eq!(t[0].next_ms, Some(1_793_843_571_050));
        assert_eq!(t[0].last_ms, None);
        assert_eq!(t[1].next_ms, None);
        assert_eq!(t[1].last_ms, Some(1_793_840_000_000));
        assert!(parse_timers("nope").is_err());
    }

    #[test]
    fn parses_timer_calendars() {
        let out = "Id=apt-daily.timer\nTimersCalendar={ OnCalendar=*-*-* 06,18:00:00 ; next_elapse=@1793858400 }\n\nId=fstrim.timer\nTimersCalendar={ OnCalendar=Mon *-*-* 00:00:00 ; next_elapse=n/a }\nTimersCalendar={ OnCalendar=Thu *-*-* 12:00:00 ; next_elapse=n/a }\n\nId=boot.timer\nTimersCalendar=\n";
        let m = parse_timer_calendars(out);
        assert_eq!(m["apt-daily.timer"], ["*-*-* 06,18:00:00"]);
        assert_eq!(m["fstrim.timer"].len(), 2);
        assert!(m["boot.timer"].is_empty());
    }

    #[test]
    fn tails_at_line_starts() {
        let text = "line one\nline two\nline three\n";
        assert_eq!(tail(text, 100).text, text);
        let t = tail(text, 14);
        assert!(t.truncated);
        assert_eq!(t.text, "line three\n");
        let t = tail("ééééé", 3);
        assert!(t.truncated && t.text.len() <= 3);
    }
}
