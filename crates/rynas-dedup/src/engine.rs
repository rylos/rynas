//! The engine behind deduplication: oans, run as a separate program (GPL-2.0-only, never
//! linked). [`DedupEngine`] keeps the rest of rynas independent of it (duperemove 0.15 could
//! become a second implementation once Debian ships it).
//!
//! rynas never replays a hashfile (`oans --hashfile=F` alone runs the stored job, whose
//! values beat the command line): every run names its roots and every option, so the
//! hashfile is only a cache of hashes.

use std::collections::BTreeMap;

use rynas_api_types::dedup::{DedupProgress, DedupReport, VolumeDedup};
use serde_json::Value;

/// One run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupJob {
    pub hashfile: String,
    /// Absolute folders, all on one filesystem.
    pub roots: Vec<String>,
    /// Final patterns ([`crate::excludes::build`]).
    pub excludes: Vec<String>,
    pub settings: VolumeDedup,
    /// `false`: an estimate (hash and report, change nothing).
    pub dedupe: bool,
}

/// A line of the engine's progress stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineLine {
    Progress(DedupProgress),
    /// The run finished its work.
    Done { reclaimed_bytes: u64, files_scanned: Option<u64> },
}

/// What an exit status means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Done,
    /// Finished, but some roots could not be covered.
    Partial,
    /// Stopped by a signal, work saved.
    Interrupted,
    Failed,
}

pub trait DedupEngine: Send + Sync {
    /// The run's command line (program first).
    fn command(&self, job: &DedupJob) -> Vec<String>;
    /// Forget roots that left the job (`-R`).
    fn remove_command(&self, hashfile: &str, paths: &[String]) -> Vec<String>;
    /// The hashfile's figures as JSON (read-only, allowed during a run).
    fn report_command(&self, hashfile: &str) -> Vec<String>;
    fn version_command(&self) -> Vec<String>;
    fn parse_version(&self, out: &str) -> Option<String>;
    /// The version is in the range rynas was tested with.
    fn supported(&self, version: &str) -> bool;
    /// `(min, max)` shown to users.
    fn tested_range(&self) -> (&'static str, &'static str);
    fn parse_line(&self, line: &str) -> Option<EngineLine>;
    fn parse_report(&self, json: &str) -> Option<DedupReport>;
    fn exit_kind(&self, code: Option<i32>) -> ExitKind;
}

/// oans (`martinus/oans`), package `rynas-oans`.
#[derive(Debug, Clone)]
pub struct Oans {
    pub binary: String,
}

impl Default for Oans {
    fn default() -> Self {
        Self { binary: rynas_api_types::dedup::ENGINE_PATH.into() }
    }
}

/// Tested versions: `MIN` up to (not including) the next minor of `MAX_MINOR`.
const MIN: (u32, u32, u32) = (1, 14, 1);
const MAX_MINOR: (u32, u32) = (1, 14);

fn parse_semver(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
    Some((a, b, c))
}

fn size_arg(bytes: u64) -> String {
    if bytes.is_multiple_of(1 << 20) {
        format!("{}M", bytes >> 20)
    } else if bytes.is_multiple_of(1024) {
        format!("{}K", bytes >> 10)
    } else {
        bytes.to_string()
    }
}

fn u(v: &Value, k: &str) -> Option<u64> {
    v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)))
}

impl DedupEngine for Oans {
    fn command(&self, job: &DedupJob) -> Vec<String> {
        let s = &job.settings;
        let mut v = vec![self.binary.clone(), "-r".into()];
        if job.dedupe {
            v.push("-d".into());
        }
        v.push(format!("--hashfile={}", job.hashfile));
        v.push("-b".into());
        v.push(size_arg(s.block_size as u64));
        v.push(format!("--min-filesize={}", size_arg(s.min_file_size)));
        if s.skip_zeroes {
            v.push("--skip-zeroes".into());
        }
        // snapshots (and received backups) are never touched, whatever oans' default is
        v.push("--skip-readonly-subvols".into());
        v.push(format!(
            "--dedupe-options=same,{}partial,noonly_whole_files",
            if s.partial { "" } else { "no" }
        ));
        if let Some(n) = s.cpu_threads {
            v.push(format!("--cpu-threads={n}"));
        }
        if let Some(n) = s.io_threads {
            v.push(format!("--io-threads={n}"));
        }
        v.extend(["-q".into(), "--no-color".into(), "--progress=json".into()]);
        for e in &job.excludes {
            v.push(format!("--exclude={e}"));
        }
        v.push("--".into());
        v.extend(job.roots.iter().cloned());
        v
    }

    fn remove_command(&self, hashfile: &str, paths: &[String]) -> Vec<String> {
        let mut v = vec![self.binary.clone(), "-R".into(), format!("--hashfile={hashfile}"), "--".into()];
        v.extend(paths.iter().cloned());
        v
    }

    fn report_command(&self, hashfile: &str) -> Vec<String> {
        vec![self.binary.clone(), "--json".into(), format!("--hashfile={hashfile}")]
    }

    fn version_command(&self) -> Vec<String> {
        vec![self.binary.clone(), "--version".into()]
    }

    /// `oans v1.14.1` (or `oans 1.14.1-3-gabcd`) → `1.14.1`.
    fn parse_version(&self, out: &str) -> Option<String> {
        let line = out.lines().find(|l| l.trim_start().starts_with("oans "))?;
        let v = line.trim_start().strip_prefix("oans ")?.trim();
        let v = v.strip_prefix('v').unwrap_or(v);
        let v: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        parse_semver(&v).map(|_| v)
    }

    fn supported(&self, version: &str) -> bool {
        parse_semver(version).is_some_and(|(a, b, c)| (a, b, c) >= MIN && (a, b) <= MAX_MINOR)
    }

    fn tested_range(&self) -> (&'static str, &'static str) {
        ("1.14.1", "1.14.x")
    }

    fn parse_line(&self, line: &str) -> Option<EngineLine> {
        let t = line.trim();
        if !t.starts_with('{') {
            return None;
        }
        let v: Value = serde_json::from_str(t).ok()?;
        if v.get("event").and_then(Value::as_str) == Some("done") {
            return Some(EngineLine::Done {
                reclaimed_bytes: u(&v, "reclaimed_bytes").unwrap_or(0),
                files_scanned: u(&v, "files_scanned"),
            });
        }
        let phase = v.get("phase").and_then(Value::as_str)?;
        let (done, total) = match phase {
            "scanning" => (u(&v, "files_examined").unwrap_or(0), 0),
            "hashing" => match u(&v, "bytes_total") {
                Some(t) if t > 0 => (u(&v, "bytes").unwrap_or(0), t),
                _ => (u(&v, "files").unwrap_or(0), u(&v, "files_total").unwrap_or(0)),
            },
            "dedupe" => (u(&v, "work_done_bytes").unwrap_or(0), u(&v, "work_total_bytes").unwrap_or(0)),
            _ => return None,
        };
        Some(EngineLine::Progress(DedupProgress {
            phase: phase.to_string(),
            done,
            total,
            bytes_per_sec: u(&v, "bytes_per_sec"),
            eta_s: u(&v, "eta_sec"),
            reclaimed_bytes: u(&v, "reclaimed_bytes").unwrap_or(0),
        }))
    }

    fn parse_report(&self, json: &str) -> Option<DedupReport> {
        let start = json.find('{')?;
        let v: Value = serde_json::from_str(json[start..].trim()).ok()?;
        v.as_object()?;
        let mut skipped = BTreeMap::new();
        if let Some(o) = v.get("scan_skipped_last_run").and_then(Value::as_object) {
            for (k, n) in o {
                if let Some(n) = n.as_u64() {
                    skipped.insert(k.clone(), n);
                }
            }
        }
        Some(DedupReport {
            files_tracked: u(&v, "files_tracked").unwrap_or(0),
            logical_bytes: u(&v, "logical_bytes").unwrap_or(0),
            dup_groups: u(&v, "dup_groups").unwrap_or(0),
            dup_files: u(&v, "dup_files").unwrap_or(0),
            reclaimable_bytes: u(&v, "reclaimable_logical_bytes").unwrap_or(0),
            runs: u(&v, "runs").unwrap_or(0),
            reclaimed_total_bytes: u(&v, "reclaimed_total_bytes").unwrap_or(0),
            dedupe_configured: v.get("scan_configured_dedupe").and_then(Value::as_bool),
            readonly_subvols_skipped: u(&v, "readonly_subvols_skipped_last_run").unwrap_or(0),
            skipped_last_run: skipped,
            read_ms: 0,
        })
    }

    fn exit_kind(&self, code: Option<i32>) -> ExitKind {
        match code {
            Some(0) => ExitKind::Done,
            Some(2) => ExitKind::Partial,
            Some(130) | Some(143) => ExitKind::Interrupted,
            _ => ExitKind::Failed,
        }
    }
}

/// Properties of the transient unit a run goes in: lowest priority, a memory cap, SIGINT
/// with 5 minutes to save the work, stopped with rynasd, a sandbox that can still write the
/// roots and the hashfile (a read-only mount would make every dedupe fail with `EROFS`).
pub fn unit_properties(settings: &VolumeDedup, writable: &[String]) -> Vec<String> {
    let max = settings.memory_max_mib;
    let mut p: Vec<String> = [
        "Nice=19",
        "IOSchedulingClass=idle",
        "CPUWeight=20",
        "KillSignal=SIGINT",
        "TimeoutStopSec=5min",
        "SendSIGKILL=yes",
        "BindsTo=rynasd.service",
        "After=rynasd.service",
        "ProtectSystem=strict",
        "PrivateTmp=yes",
        "PrivateNetwork=yes",
        "NoNewPrivileges=yes",
        "CapabilityBoundingSet=CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_FOWNER",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    p.push(format!("MemoryHigh={}M", max * 3 / 4));
    p.push(format!("MemoryMax={max}M"));
    for w in writable {
        // `-`: a missing folder does not stop the unit (oans then reports exit 2)
        if w.chars().any(char::is_whitespace) || w.contains('"') {
            p.push(format!("ReadWritePaths=\"-{}\"", w.replace('\\', "\\\\").replace('"', "\\\"")));
        } else {
            p.push(format!("ReadWritePaths=-{w}"));
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(dedupe: bool) -> DedupJob {
        DedupJob {
            hashfile: "/vol1/@rynas-cache/dedup/volume.hash".into(),
            roots: vec!["/vol1/docs".into(), "/vol1/media".into()],
            excludes: vec!["/vol1/docs/\\#recycle/".into(), ".snapshots/".into()],
            settings: VolumeDedup::default(),
            dedupe,
        }
    }

    #[test]
    fn golden_command_line() {
        let o = Oans::default();
        assert_eq!(
            o.command(&job(true)).join(" "),
            "/usr/lib/rynas/oans/oans -r -d --hashfile=/vol1/@rynas-cache/dedup/volume.hash -b 128K \
             --min-filesize=64K --skip-zeroes --skip-readonly-subvols \
             --dedupe-options=same,nopartial,noonly_whole_files --cpu-threads=2 -q --no-color \
             --progress=json --exclude=/vol1/docs/\\#recycle/ --exclude=.snapshots/ -- /vol1/docs /vol1/media"
        );
        let mut j = job(false);
        j.settings.partial = true;
        j.settings.skip_zeroes = false;
        j.settings.cpu_threads = None;
        j.settings.io_threads = Some(3);
        j.settings.block_size = 1 << 20;
        let c = o.command(&j);
        assert!(!c.contains(&"-d".to_string()));
        assert!(c.contains(&"--dedupe-options=same,partial,noonly_whole_files".to_string()));
        assert!(c.contains(&"--io-threads=3".to_string()) && c.contains(&"1M".to_string()));
        assert!(!c.iter().any(|a| a == "--skip-zeroes" || a.starts_with("--cpu-threads")));
        // the roots always come after `--`, never a replay
        assert_eq!(c[c.len() - 3], "--");
        assert_eq!(
            o.remove_command("/h", &["/vol1/old".into()]),
            ["/usr/lib/rynas/oans/oans", "-R", "--hashfile=/h", "--", "/vol1/old"]
        );
    }

    #[test]
    fn versions() {
        let o = Oans::default();
        assert_eq!(o.parse_version("oans v1.14.1\n").as_deref(), Some("1.14.1"));
        assert_eq!(o.parse_version("oans 1.14.2-3-gabcd").as_deref(), Some("1.14.2"));
        assert_eq!(o.parse_version("oans unknown"), None);
        assert!(o.supported("1.14.1") && o.supported("1.14.9"));
        assert!(!o.supported("1.14.0") && !o.supported("1.15.0") && !o.supported("2.0.0"));
    }

    #[test]
    fn progress_stream() {
        let o = Oans::default();
        let lines = [
            r#"{"phase":"scanning","elapsed_sec":2.1,"files_examined":48120,"files_to_hash":12340}"#,
            r#"{"phase":"hashing","files":10,"files_total":20,"bytes":1000,"bytes_total":4000,"bytes_per_sec":500,"eta_sec":6}"#,
            "ERROR: /vol1/docs/x: Permission denied",
            r#"{"phase":"dedupe","groups":3,"groups_total":9,"work_done_bytes":10,"work_total_bytes":40,"reclaimed_bytes":7,"activity":"comparing","eta_sec":2.5}"#,
            r#"{"phase":"later-phase","x":1}"#,
            r#"{"event":"done","elapsed_sec":9.9,"files_scanned":48120,"groups_deduped":9,"reclaimed_bytes":123}"#,
            "{not json",
        ];
        let got: Vec<_> = lines.iter().map(|l| o.parse_line(l)).collect();
        assert!(matches!(&got[0], Some(EngineLine::Progress(p)) if p.phase == "scanning" && p.done == 48120 && p.total == 0));
        assert!(matches!(&got[1], Some(EngineLine::Progress(p)) if p.done == 1000 && p.total == 4000 && p.eta_s == Some(6)));
        assert_eq!(got[2], None);
        assert!(matches!(&got[3], Some(EngineLine::Progress(p)) if p.reclaimed_bytes == 7 && p.eta_s == Some(2)));
        assert_eq!(got[4], None);
        assert_eq!(got[5], Some(EngineLine::Done { reclaimed_bytes: 123, files_scanned: Some(48120) }));
        assert_eq!(got[6], None);
    }

    #[test]
    fn report() {
        let o = Oans::default();
        let r = o
            .parse_report(
                r#"{"block_size":131072,"files_tracked":190000,"logical_bytes":11274289152,"dup_groups":12,"dup_files":30,
                "reclaimable_logical_bytes":5000,"runs":4,"reclaimed_total_bytes":268435456000,"files_hashed":10,
                "last_run_ts":1762800000,"scan_configured_dedupe":true,"readonly_subvols_skipped_last_run":2,
                "scan_skipped_last_run":{"permission":3,"unreadable":0,"path_too_long":1,"unsupported_fs":0},
                "scan_skipped_errors_total":9}"#,
            )
            .unwrap();
        assert_eq!(r.files_tracked, 190_000);
        assert_eq!(r.reclaimed_total_bytes, 268_435_456_000);
        assert_eq!(r.dedupe_configured, Some(true));
        assert_eq!(r.skipped_last_run["permission"], 3);
        assert_eq!(o.parse_report("garbage"), None);
        let empty = o.parse_report(r#"{"scan_configured_dedupe":null}"#).unwrap();
        assert_eq!(empty.dedupe_configured, None);
    }

    #[test]
    fn exits() {
        let o = Oans::default();
        assert_eq!(o.exit_kind(Some(0)), ExitKind::Done);
        assert_eq!(o.exit_kind(Some(2)), ExitKind::Partial);
        assert_eq!(o.exit_kind(Some(130)), ExitKind::Interrupted);
        assert_eq!(o.exit_kind(Some(143)), ExitKind::Interrupted);
        assert_eq!(o.exit_kind(Some(1)), ExitKind::Failed);
        assert_eq!(o.exit_kind(None), ExitKind::Failed);
    }

    #[test]
    fn unit() {
        let p = unit_properties(&VolumeDedup::default(), &["/vol1/docs".into(), "/srv/my data".into()]);
        assert!(p.contains(&"KillSignal=SIGINT".to_string()));
        assert!(p.contains(&"TimeoutStopSec=5min".to_string()));
        assert!(p.contains(&"MemoryMax=1024M".to_string()) && p.contains(&"MemoryHigh=768M".to_string()));
        assert!(p.contains(&"ReadWritePaths=-/vol1/docs".to_string()));
        assert!(p.contains(&"ReadWritePaths=\"-/srv/my data\"".to_string()));
    }
}
