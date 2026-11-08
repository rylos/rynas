//! Starts `rynas-backup-worker`, gives it one request and follows its events.
//!
//! [`Launch::Unit`] runs it as `systemd-run --wait --pipe` in a transient service
//! `rynas-backup-<job>-<n>`: outside rynasd's sandbox and memory limit, with its own
//! `MemoryHigh`/`MemoryMax`, `Nice=19`, `IOSchedulingClass=idle` and `KillSignal=SIGINT`; a
//! cancel stops the unit (`systemctl stop`), so the worker never outlives its task.
//! [`Launch::Direct`] runs it as a plain child (tests, `--dry-system`, development); the
//! worker then lowers its own priorities.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rynas_api_types::backup::{WorkerEvent, WorkerRequest};
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

/// The unit name for a run of `job` (`rynas-backup-<job>-<secs>-<n>`).
fn unit_name(job: &str) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let safe: String = job
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    format!(
        "rynas-backup-{safe}-{secs}-{}",
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// The command line (program, arguments) and the unit, for a run of `job`.
pub fn command(launch: &Launch, job: &str) -> (Vec<String>, Option<String>) {
    match launch {
        Launch::Direct { exe } => (vec![exe.display().to_string()], None),
        Launch::Unit {
            exe,
            memory_high,
            memory_max,
        } => {
            let unit = unit_name(job);
            let argv = vec![
                "systemd-run".into(),
                "--wait".into(),
                "--pipe".into(),
                "--collect".into(),
                "--quiet".into(),
                "--service-type=exec".into(),
                format!("--unit={unit}"),
                format!("--description=rynas backup worker ({job})"),
                format!("--property=MemoryHigh={memory_high}"),
                format!("--property=MemoryMax={memory_max}"),
                "--property=Nice=19".into(),
                "--property=IOSchedulingClass=idle".into(),
                "--property=CPUSchedulingPolicy=batch".into(),
                "--property=KillSignal=SIGINT".into(),
                "--property=TimeoutStopSec=60".into(),
                "--property=PrivateTmp=yes".into(),
                "--setenv=HOME=/root".into(),
                "--setenv=LANG=C.UTF-8".into(),
                "--".into(),
                exe.display().to_string(),
            ];
            (argv, Some(format!("{unit}.service")))
        }
    }
}

/// Runs one request; `on` sees every event (steps, progress, logs, partial results). The
/// result is the last event, or the worker's error message. `timeout`: the whole run.
pub async fn run_worker(
    launch: &Launch,
    job: &str,
    req: &WorkerRequest,
    cancel: &CancellationToken,
    timeout: Option<Duration>,
    mut on: impl FnMut(&WorkerEvent),
) -> Result<WorkerEvent, String> {
    let (argv, unit) = command(launch, job);
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
    let read = async {
        while let Ok(Some(l)) = lines.next_line().await {
            match serde_json::from_str::<WorkerEvent>(&l) {
                Ok(ev) => {
                    on(&ev);
                    if !matches!(
                        ev,
                        WorkerEvent::Progress { .. } | WorkerEvent::Step { .. } | WorkerEvent::Log { .. }
                    ) {
                        last = Some(ev);
                    }
                }
                Err(_) if !l.trim().is_empty() => on(&WorkerEvent::Log { text: l }),
                Err(_) => {}
            }
        }
    };
    let limit = timeout.unwrap_or(Duration::from_secs(u64::MAX / 4));
    let outcome = tokio::select! {
        _ = read => None,
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
        Some(WorkerEvent::Error { message }) => Err(message),
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
            Err(format!(
                "the backup worker ended ({status}) without a result{}",
                if tail.is_empty() { String::new() } else { format!(": {tail}") }
            ))
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
            None,
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
        let t = tempfile::tempdir().unwrap();
        let exe = fake(t.path(), r#"echo '{"event":"error","message":"no repository"}'; exit 1"#);
        let r = run_worker(&Launch::Direct { exe }, "j", &req(), &CancellationToken::new(), None, |_| {}).await;
        assert_eq!(r.unwrap_err(), "no repository");
        let exe = fake(t.path(), "echo boom >&2; exit 3");
        let r = run_worker(&Launch::Direct { exe }, "j", &req(), &CancellationToken::new(), None, |_| {}).await;
        assert!(r.unwrap_err().contains("boom"));
        let exe = fake(t.path(), "exec sleep 30");
        let c = CancellationToken::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let start = std::time::Instant::now();
        let r = run_worker(&Launch::Direct { exe: exe.clone() }, "j", &req(), &c, None, |_| {}).await;
        assert_eq!(r.unwrap_err(), "cancelled");
        assert!(start.elapsed() < Duration::from_secs(10));
        let r = run_worker(&Launch::Direct { exe }, "j", &req(), &CancellationToken::new(), Some(Duration::from_millis(200)), |_| {}).await;
        assert_eq!(r.unwrap_err(), "timed out");

        let (argv, unit) = command(&Launch::unit("/usr/libexec/rynas/rynas-backup-worker".into()), "docs");
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--property=MemoryMax=2G".to_string()));
        assert!(argv.contains(&"--property=KillSignal=SIGINT".to_string()));
        assert!(unit.unwrap().starts_with("rynas-backup-docs-"));
        assert_eq!(argv.last().unwrap(), "/usr/libexec/rynas/rynas-backup-worker");
    }
}
