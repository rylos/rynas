//! Starts `rynas-backup-worker`, gives it one request and follows its events.
//!
//! [`Launch::Unit`] runs it as `systemd-run --wait --pipe` in a transient service
//! `rynas-backup-<job>-<n>`: outside rynasd's memory limit, with its own
//! `MemoryHigh`/`MemoryMax`, `Nice=19`, `IOSchedulingClass=idle` and `KillSignal=SIGINT`,
//! and a sandbox of its own (`ProtectSystem=strict` with only the request's folders
//! writable, see [`write_paths`]; `CAP_DAC_READ_SEARCH` only, plus the ownership
//! capabilities for a restore). `BindsTo=rynasd.service`: it stops with rynasd. A cancel,
//! the run's timeout or a stall (no event for [`Limits::stall`]) stops the unit
//! (`systemctl stop`), so the worker never outlives its task. Secret values of the request
//! are hidden from the lines that do not come from the worker's own (redacted) events.
//! [`Launch::Direct`] runs it as a plain child (tests, `--dry-system`, development); the
//! worker then lowers its own priorities.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rynas_api_types::backup::{WorkerEvent, WorkerOp, WorkerRequest, redact_secrets};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// How the worker is started.
#[derive(Debug, Clone)]
pub enum Launch {
    Direct {
        exe: PathBuf,
    },
    Unit {
        exe: PathBuf,
        /// `MemoryHigh` and `MemoryMax` ("1G", "2G").
        memory_high: String,
        memory_max: String,
    },
}

impl Launch {
    /// The production launch: a unit, limits from `RYNAS_BACKUP_MEMORY` (`<high>:<max>`,
    /// default `1536M:2G`).
    pub fn unit(exe: PathBuf) -> Self {
        let (high, max) = std::env::var("RYNAS_BACKUP_MEMORY")
            .ok()
            .and_then(|v| {
                let (h, m) = v.split_once(':')?;
                let ok = |s: &str| {
                    let d = s.strip_suffix(['K', 'M', 'G']).unwrap_or(s);
                    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())
                };
                (ok(h) && ok(m)).then(|| (h.to_string(), m.to_string()))
            })
            .unwrap_or_else(|| ("1536M".into(), "2G".into()));
        Launch::Unit {
            exe,
            memory_high: high,
            memory_max: max,
        }
    }
}

static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn unit_name(job: &str) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let safe: String = job
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    format!(
        "{UNIT_PREFIX}{safe}-{secs}-{}",
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// Every worker unit starts with this (rynasd stops leftovers at its start).
pub const UNIT_PREFIX: &str = "rynas-backup-";

/// Limits of one run.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    /// The whole run.
    pub timeout: Option<Duration>,
    /// No event for this long: the worker is stopped ("stalled").
    pub stall: Option<Duration>,
    /// The unit name to use ([`unit_name`]), to record it before the start.
    pub unit: Option<String>,
}

/// The folders a request writes: its cache, a local repository, a download's folder, a
/// restore's target. Everything else is read-only in the worker's unit.
pub fn write_paths(req: &WorkerRequest) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(c) = &req.cache_dir {
        v.push(c.clone());
    }
    if req.repo.repository.starts_with('/') {
        v.push(req.repo.repository.clone());
    }
    match &req.op {
        WorkerOp::Dump { out, .. } => {
            if let Some(p) = std::path::Path::new(out).parent() {
                v.push(p.display().to_string());
            }
        }
        WorkerOp::Restore { target: Some(t), .. } => v.push(t.clone()),
        _ => {}
    }
    v
}

/// A path as one quoted word of a systemd list (refused with quotes, backslashes or
/// control characters, which rynas never puts in such a folder).
fn unit_path_word(p: &str) -> Result<String, String> {
    if !p.starts_with('/') || p.contains(['"', '\\', '\'']) || p.chars().any(char::is_control) {
        return Err(format!("unsupported folder name for the worker unit: {p:?}"));
    }
    // "-": a missing folder does not stop the unit (the write then fails clearly)
    Ok(format!("\"-{p}\""))
}

pub fn command(
    launch: &Launch,
    job: &str,
    req: &WorkerRequest,
    unit: Option<String>,
) -> Result<(Vec<String>, Option<String>), String> {
    match launch {
        Launch::Direct { exe } => Ok((vec![exe.display().to_string()], None)),
        Launch::Unit {
            exe,
            memory_high,
            memory_max,
        } => {
            let unit = unit.unwrap_or_else(|| unit_name(job));
            let restore = matches!(req.op, WorkerOp::Restore { .. });
            let mut argv: Vec<String> = vec![
                "systemd-run".into(),
                "--wait".into(),
                "--pipe".into(),
                "--collect".into(),
                "--quiet".into(),
                "--service-type=exec".into(),
                format!("--unit={unit}"),
                format!("--description=rynas backup worker ({job})"),
            ];
            let mut props: Vec<String> = vec![
                format!("MemoryHigh={memory_high}"),
                format!("MemoryMax={memory_max}"),
                "Nice=19".into(),
                "IOSchedulingClass=idle".into(),
                "CPUSchedulingPolicy=batch".into(),
                "KillSignal=SIGINT".into(),
                "TimeoutStopSec=60".into(),
                // stopped with rynasd: a worker nobody reads any more never runs on (its
                // pipe is gone; rynasd puts back stopped stacks and snapshots at its start)
                "BindsTo=rynasd.service".into(),
                "After=rynasd.service".into(),
                // sandbox (review M1): the system read-only but the request's folders
                "NoNewPrivileges=yes".into(),
                "ProtectSystem=strict".into(),
                "PrivateTmp=yes".into(),
                "ProtectKernelTunables=yes".into(),
                "ProtectKernelModules=yes".into(),
                "ProtectKernelLogs=yes".into(),
                "ProtectControlGroups=yes".into(),
                "ProtectClock=yes".into(),
                "RestrictNamespaces=yes".into(),
                "LockPersonality=yes".into(),
                "RestrictRealtime=yes".into(),
                "SystemCallArchitectures=native".into(),
                // rclone (Go) and ssh run inside: @system-service covers them; a call outside
                // fails with EPERM instead of killing the worker
                "SystemCallFilter=@system-service".into(),
                "SystemCallErrorNumber=EPERM".into(),
                "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6".into(),
            ];
            if restore {
                // ownership, modes, set-id bits and device nodes of the restored files
                props.push(
                    "CapabilityBoundingSet=CAP_DAC_READ_SEARCH CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE CAP_FSETID CAP_MKNOD"
                        .into(),
                );
            } else {
                // read every source; the worker's own files are root's
                props.push("CapabilityBoundingSet=CAP_DAC_READ_SEARCH".into());
                props.push("PrivateDevices=yes".into());
                props.push("RestrictSUIDSGID=yes".into());
            }
            let rw: Vec<String> = write_paths(req)
                .iter()
                .map(|p| unit_path_word(p))
                .collect::<Result<_, _>>()?;
            if !rw.is_empty() {
                props.push(format!("ReadWritePaths={}", rw.join(" ")));
            }
            argv.extend(props.into_iter().map(|p| format!("--property={p}")));
            argv.extend([
                // the private /tmp: ssh's control socket, rclone's cache
                "--setenv=HOME=/tmp".into(),
                "--setenv=LANG=C.UTF-8".into(),
                "--".into(),
                exe.display().to_string(),
            ]);
            Ok((argv, Some(format!("{unit}.service"))))
        }
    }
}

pub async fn run_worker(
    launch: &Launch,
    job: &str,
    req: &WorkerRequest,
    cancel: &CancellationToken,
    limits: &Limits,
    mut on: impl FnMut(&WorkerEvent),
) -> Result<WorkerEvent, String> {
    let secrets = req.repo.secret_values();
    let hide = |t: &str| redact_secrets(t, &secrets);
    let (argv, unit) = command(launch, job, req, limits.unit.clone())?;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", argv[0]))?;
    let body = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    stdin.write_all(&body).await.map_err(|e| e.to_string())?;
    drop(stdin);
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(64 << 10).read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });
    let mut lines = BufReader::new(stdout).lines();
    let mut last: Option<WorkerEvent> = None;
    let stall = limits.stall.unwrap_or(Duration::from_secs(u64::MAX / 4));
    let read = async {
        loop {
            let l = match tokio::time::timeout(stall, lines.next_line()).await {
                Err(_) => return Some("stalled: no progress from the backup worker"),
                Ok(Ok(Some(l))) => l,
                Ok(_) => return None,
            };
            match serde_json::from_str::<WorkerEvent>(&l) {
                Ok(ev) => {
                    on(&ev);
                    if !matches!(
                        ev,
                        WorkerEvent::Progress { .. }
                            | WorkerEvent::Step { .. }
                            | WorkerEvent::Log { .. }
                            | WorkerEvent::SecretChanged { .. }
                    ) {
                        last = Some(ev);
                    }
                }
                Err(_) if !l.trim().is_empty() => on(&WorkerEvent::Log { text: hide(&l) }),
                Err(_) => {}
            }
        }
    };
    let limit = limits.timeout.unwrap_or(Duration::from_secs(u64::MAX / 4));
    let outcome = tokio::select! {
        r = read => r,
        _ = cancel.cancelled() => Some("cancelled"),
        _ = tokio::time::sleep(limit) => Some("timed out"),
    };
    if let Some(why) = outcome {
        stop(&mut child, unit.as_deref()).await;
        return Err(why.into());
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let stderr = err_task.await.unwrap_or_default();
    match last {
        Some(WorkerEvent::Error { message }) => Err(hide(&message)),
        Some(ev) if status.success() => Ok(ev),
        _ => {
            let tail: String = stderr
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("; ");
            Err(hide(&format!(
                "the backup worker ended ({status}) without a result{}",
                if tail.is_empty() { String::new() } else { format!(": {tail}") }
            )))
        }
    }
}

/// SIGINT to the worker (its unit), then the stop after a grace time.
async fn stop(child: &mut tokio::process::Child, unit: Option<&str>) {
    match unit {
        Some(u) => {
            // `systemctl stop` sends the unit's KillSignal (SIGINT) and waits TimeoutStopSec
            let _ = tokio::process::Command::new("systemctl")
                .args(["stop", "--no-block", u])
                .env_clear()
                .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                .status()
                .await;
            if tokio::time::timeout(Duration::from_secs(70), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
        None => {
            if let Some(pid) = child.id() {
                // SAFETY: a signal to our own child
                unsafe { libc::kill(pid as i32, libc::SIGINT) };
            }
            if tokio::time::timeout(Duration::from_secs(30), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::backup::{RepoSpec, WorkerOp};
    use std::os::unix::fs::PermissionsExt;

    fn fake(dir: &std::path::Path, body: &str) -> PathBuf {
        let p = dir.join("worker");
        std::fs::write(&p, format!("#!/bin/sh\ncat > {}/req.json\n{body}\n", dir.display())).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn req() -> WorkerRequest {
        WorkerRequest {
            repo: RepoSpec {
                repository: "/r".into(),
                password: "pw".into(),
                ..Default::default()
            },
            op: WorkerOp::Stats,
            cache_dir: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn streams_events_and_results() {
        let t = tempfile::tempdir().unwrap();
        let exe = fake(
            t.path(),
            r#"echo '{"event":"step","title":"reading"}'
echo '{"event":"progress","done":1,"total":2,"bytes":true}'
echo 'not json'
echo '{"event":"stats","bytes":5,"files":1}'"#,
        );
        let mut seen = Vec::new();
        let r = run_worker(
            &Launch::Direct { exe },
            "j",
            &req(),
            &CancellationToken::new(),
            &Limits::default(),
            |e| seen.push(e.clone()),
        )
        .await
        .unwrap();
        assert_eq!(r, WorkerEvent::Stats { bytes: 5, files: 1 });
        assert_eq!(seen.len(), 4);
        assert!(matches!(&seen[2], WorkerEvent::Log { text } if text == "not json"));
        // the request went in on stdin
        let sent: WorkerRequest =
            serde_json::from_str(&std::fs::read_to_string(t.path().join("req.json")).unwrap()).unwrap();
        assert_eq!(sent, req());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn errors_cancel_and_units() {
        let none = Limits::default();
        let t = tempfile::tempdir().unwrap();
        let exe = fake(t.path(), r#"echo '{"event":"error","message":"no repository pw"}'; exit 1"#);
        let r = run_worker(&Launch::Direct { exe }, "j", &req(), &CancellationToken::new(), &none, |_| {}).await;
        // the error is redacted with the request's secrets ("pw" is the password)
        assert_eq!(r.unwrap_err(), "no repository ***");
        let exe = fake(t.path(), "echo boom pw >&2; exit 3");
        let r = run_worker(&Launch::Direct { exe }, "j", &req(), &CancellationToken::new(), &none, |_| {}).await;
        let e = r.unwrap_err();
        assert!(e.contains("boom ***"), "{e}");
        let exe = fake(t.path(), "exec sleep 30");
        let c = CancellationToken::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let start = std::time::Instant::now();
        let r = run_worker(&Launch::Direct { exe: exe.clone() }, "j", &req(), &c, &none, |_| {}).await;
        assert_eq!(r.unwrap_err(), "cancelled");
        assert!(start.elapsed() < Duration::from_secs(10));
        let short = Limits {
            timeout: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let r = run_worker(&Launch::Direct { exe }, "j", &req(), &CancellationToken::new(), &short, |_| {}).await;
        assert_eq!(r.unwrap_err(), "timed out");
        // a stall: one event, then silence
        let exe = fake(t.path(), r#"echo '{"event":"step","title":"x"}'; exec sleep 30"#);
        let stall = Limits {
            stall: Some(Duration::from_millis(300)),
            ..Default::default()
        };
        let r = run_worker(&Launch::Direct { exe }, "j", &req(), &CancellationToken::new(), &stall, |_| {}).await;
        assert!(r.unwrap_err().starts_with("stalled"));
        // a changed secret file is passed on, never the result
        let exe = fake(
            t.path(),
            r#"echo '{"event":"secret_changed","name":"RCLONE_CONFIG","content":"[x]"}'
echo '{"event":"stats","bytes":1,"files":1}'"#,
        );
        let mut changed = None;
        let r = run_worker(&Launch::Direct { exe }, "j", &req(), &CancellationToken::new(), &none, |e| {
            if let WorkerEvent::SecretChanged { content, .. } = e {
                changed = Some(content.clone());
            }
        })
        .await;
        assert_eq!(r.unwrap(), WorkerEvent::Stats { bytes: 1, files: 1 });
        assert_eq!(changed.as_deref(), Some("[x]"));

        let launch = Launch::unit("/usr/libexec/rynas/rynas-backup-worker".into());
        let (argv, unit) = command(&launch, "docs", &req(), None).unwrap();
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--property=MemoryMax=2G".to_string()));
        assert!(argv.contains(&"--property=KillSignal=SIGINT".to_string()));
        assert!(argv.contains(&"--property=ProtectSystem=strict".to_string()));
        assert!(argv.contains(&"--property=BindsTo=rynasd.service".to_string()));
        assert!(argv.contains(&"--property=CapabilityBoundingSet=CAP_DAC_READ_SEARCH".to_string()));
        assert!(argv.contains(&"--property=ReadWritePaths=\"-/r\"".to_string()));
        assert!(unit.unwrap().starts_with("rynas-backup-docs-"));
        assert_eq!(argv.last().unwrap(), "/usr/libexec/rynas/rynas-backup-worker");
        // a restore gets the ownership capabilities and its target writable
        let mut r = req();
        r.repo.repository = "opendal:s3".into();
        r.cache_dir = Some("/var/cache/rynas/backup/docs".into());
        r.op = WorkerOp::Restore {
            snapshot: "latest".into(),
            paths: vec![],
            target: Some("/volume1/restore".into()),
            scope: None,
        };
        let (argv, unit) = command(&launch, "docs", &r, Some("rynas-backup-docs-1-2".into())).unwrap();
        assert_eq!(unit.as_deref(), Some("rynas-backup-docs-1-2.service"));
        assert!(argv.iter().any(|a| a.contains("CAP_CHOWN")));
        assert!(!argv.iter().any(|a| a.contains("PrivateDevices")));
        assert!(argv.contains(
            &"--property=ReadWritePaths=\"-/var/cache/rynas/backup/docs\" \"-/volume1/restore\"".to_string()
        ));
        r.op = WorkerOp::Restore {
            snapshot: "latest".into(),
            paths: vec![],
            target: Some("/volume1/a\"b".into()),
            scope: None,
        };
        assert!(command(&launch, "docs", &r, None).is_err());
    }
}
