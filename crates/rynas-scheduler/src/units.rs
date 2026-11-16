//! The systemd units of a task.
//!
//! - `rynas-task-<id>.service`: `Type=exec` (so that `RuntimeMaxSec=` applies), the task's
//!   user, `Nice=`/`IOWeight=`, `NoNewPrivileges=`, `PrivateTmp=`,
//!   `ExecCondition=+rynasctl gate <id>` (the scheduling gate, [`crate::gate`]) and
//!   `ExecStopPost=+rynasctl task-finished <id>` (as root, whatever `User=`) recording the run.
//!   Adding the gate changed the services only: the timers are byte-identical, so an upgrade
//!   rewrites the services, reloads systemd once and restarts no timer (their `Persistent=`
//!   stamps stay).
//!   User actions read their script on standard input (`StandardInput=file:`): the file stays
//!   root:root 0700, the service manager opens it before dropping privileges, the user never
//!   gets a path to it. Internal actions run `rynasctl job run <id>`, which starts the rynasd
//!   task and follows it to the end, or runs the command rynasd plans for it (scrub, SMART
//!   test, snapshots: [`runs_in_unit`], sandboxed with [`JOB_SANDBOX`]).
//! - `rynas-task-<id>.timer`: `OnCalendar=` (+ `Persistent=`, `RandomizedDelaySec=`) or
//!   `OnBootSec=`; no `[Install]` section.
//! - `timers.target.d/rynas-tasks.conf`: `Wants=` on the timers of the enabled tasks. Enabling
//!   a task is a line in a managed file, not a symlink made by `systemctl enable`, so it is
//!   covered by revisions, rollback and drift like every other file.

use std::path::{Path, PathBuf};

use rynas_api_types::scheduler::{ScheduledTask, TaskAction, TaskTrigger};

pub const UNIT_DIR: &str = "/etc/systemd/system";
pub const UNIT_PREFIX: &str = "rynas-task-";
pub const TIMERS_DROPIN: &str = "/etc/systemd/system/timers.target.d/rynas-tasks.conf";

/// Where things live on the target system.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Scripts: `<scripts>/<id>/run`.
    pub scripts: PathBuf,
    /// The rynasctl binary.
    pub rynasctl: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            scripts: PathBuf::from("/var/lib/rynas/tasks"),
            rynasctl: PathBuf::from("/usr/libexec/rynas/rynasctl"),
        }
    }
}

pub fn service_name(id: &str) -> String {
    format!("{UNIT_PREFIX}{id}.service")
}

pub fn timer_name(id: &str) -> String {
    format!("{UNIT_PREFIX}{id}.timer")
}

pub fn service_path(id: &str) -> PathBuf {
    Path::new(UNIT_DIR).join(service_name(id))
}

pub fn timer_path(id: &str) -> PathBuf {
    Path::new(UNIT_DIR).join(timer_name(id))
}

pub fn script_path(p: &Paths, id: &str) -> PathBuf {
    p.scripts.join(id).join("run")
}

/// The program (and optional argument) of a script's `#!` line; `/bin/sh` without one.
pub fn interpreter(script: &str) -> Result<(String, Option<String>), String> {
    let Some(first) = script.lines().next().and_then(|l| l.strip_prefix("#!")) else {
        return Ok(("/bin/sh".into(), None));
    };
    let mut parts = first.split_whitespace();
    let prog = parts
        .next()
        .ok_or_else(|| "the #! line names no interpreter".to_string())?;
    let arg = parts.next().map(str::to_string);
    if parts.next().is_some() {
        return Err("the #! line takes at most one argument".into());
    }
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._+-=".contains(&b))
    };
    if !prog.starts_with('/') || !ok(prog) || arg.as_deref().is_some_and(|a| !ok(a)) {
        return Err(format!("unsupported #! line {first:?}"));
    }
    Ok((prog.into(), arg))
}

/// Text safe for a unit file value: one line, `%` escaped (specifiers).
fn unit_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .replace('%', "%%")
}

/// The `.service` unit (without the rynas header).
pub fn render_service(t: &ScheduledTask, p: &Paths) -> Result<String, String> {
    let mut s = String::new();
    let name = if t.name.is_empty() { &t.id } else { &t.name };
    s.push_str("[Unit]\n");
    s.push_str(&format!(
        "Description=rynas task {}: {}\n",
        t.id,
        unit_text(name)
    ));
    s.push_str("Documentation=https://github.com/rylos/rynas\n");
    s.push_str("After=local-fs.target network-online.target\n");
    s.push_str("\n[Service]\n");
    s.push_str("Type=exec\n");
    let ctl = p.rynasctl.display();
    // the scheduling gate: as root whatever `User=`; exit 1 skips the start (not a failure)
    s.push_str(&format!("ExecCondition=+{ctl} gate {}\n", t.id));
    let internal = t.action.is_internal();
    match &t.action {
        TaskAction::Command { user, .. } | TaskAction::Script { user, .. } => {
            if user != "root" {
                s.push_str(&format!("User={user}\n"));
            }
            let (prog, arg) = match &t.action {
                TaskAction::Script { script, .. } => interpreter(script)?,
                _ => ("/bin/sh".into(), None),
            };
            s.push_str(&format!(
                "StandardInput=file:{}\n",
                script_path(p, &t.id).display()
            ));
            s.push_str(&match arg {
                Some(a) => format!("ExecStart={prog} {a} -\n"),
                None => format!("ExecStart={prog} -\n"),
            });
        }
        // the shutdown checks and the poweroff run in rynasd; the unit follows them
        TaskAction::PowerShutdown { rule } => s.push_str(&format!(
            "ExecStart={ctl} power scheduled-shutdown {rule}\n"
        )),
        _ => s.push_str(&format!("ExecStart={ctl} job run {}\n", t.id)),
    }
    s.push_str("StandardOutput=journal\nStandardError=journal\n");
    s.push_str(&format!("SyslogIdentifier={UNIT_PREFIX}{}\n", t.id));
    s.push_str(&format!("ExecStopPost=+{ctl} task-finished {}\n", t.id));
    match t.timeout_s {
        Some(n) if n > 0 => s.push_str(&format!("RuntimeMaxSec={n}\n")),
        _ => {}
    }
    let nice = t.nice.unwrap_or(if internal { 0 } else { 10 });
    if nice != 0 {
        s.push_str(&format!("Nice={nice}\n"));
    }
    if let Some(w) = t.io_weight {
        s.push_str(&format!("IOWeight={w}\n"));
    }
    if !internal {
        s.push_str("NoNewPrivileges=yes\nPrivateTmp=yes\n");
    }
    if runs_in_unit(&t.action) {
        s.push_str(JOB_SANDBOX);
    }
    Ok(s)
}

/// Internal actions whose command the unit runs itself (`rynasctl job run` gets a plan from
/// rynasd: btrfs scrub, smartctl, btrbk) instead of following a rynasd task.
pub fn runs_in_unit(a: &TaskAction) -> bool {
    matches!(
        a,
        TaskAction::Scrub { .. } | TaskAction::SmartTest { .. } | TaskAction::Snapshot { .. }
    )
}

/// The sandbox of a unit that runs btrfs, smartctl or btrbk as root: what rynasd's own unit
/// allowed those commands when they ran inside it, without the rest (no network, no
/// changing users, no namespaces). `ExecCondition=+`/`ExecStopPost=+` run outside it.
pub const JOB_SANDBOX: &str = "\
NoNewPrivileges=yes
ProtectSystem=yes
PrivateTmp=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
RestrictNamespaces=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
RestrictAddressFamilies=AF_UNIX AF_NETLINK
CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH CAP_FOWNER CAP_CHOWN CAP_FSETID
";

/// The `.timer` unit, `None` for manual tasks.
pub fn render_timer(t: &ScheduledTask) -> Option<String> {
    let body = match &t.trigger {
        TaskTrigger::Calendar { .. } | TaskTrigger::Recurrence { .. } => {
            // the policy decides catch-up and delay (the same values as the trigger fields
            // of a normalized task)
            let p = t.effective_policy();
            let mut b = format!("OnCalendar={}\n", t.trigger.on_calendar()?);
            if p.missed.catches_up() {
                b.push_str("Persistent=true\n");
            }
            if p.jitter_s > 0 {
                b.push_str(&format!("RandomizedDelaySec={}\n", p.jitter_s));
            }
            b
        }
        TaskTrigger::Boot { delay_s } => format!("OnBootSec={}\n", (*delay_s).max(1)),
        // no timer (manual), or not run by the Task Scheduler yet (refused when staged)
        TaskTrigger::Manual
        | TaskTrigger::Interval { .. }
        | TaskTrigger::Window { .. }
        | TaskTrigger::After { .. } => return None,
    };
    Some(format!(
        "[Unit]\nDescription=Timer of rynas task {id}\n\n[Timer]\n{body}AccuracySec=1s\nUnit={svc}\n",
        id = t.id,
        svc = service_name(&t.id),
    ))
}

/// The drop-in of `timers.target`: the timers of enabled tasks.
pub fn render_dropin(tasks: &[ScheduledTask]) -> String {
    let timers: Vec<String> = tasks
        .iter()
        .filter(|t| t.enabled && t.trigger.has_timer())
        .map(|t| timer_name(&t.id))
        .collect();
    let mut s = String::from(
        "# Timers of the enabled rynas tasks (scheduler configuration key).\n[Unit]\n",
    );
    for chunk in timers.chunks(8) {
        s.push_str(&format!("Wants={}\n", chunk.join(" ")));
    }
    s
}

/// The script file of a user action.
pub fn script_content(t: &ScheduledTask) -> Option<String> {
    match &t.action {
        TaskAction::Script { script, .. } => Some(if script.ends_with('\n') {
            script.clone()
        } else {
            format!("{script}\n")
        }),
        TaskAction::Command { command, .. } => Some(format!(
            "#!/bin/sh\n# rynas task {}: command\n{}\n",
            t.id,
            command.trim_end()
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpreters() {
        assert_eq!(interpreter("echo hi").unwrap(), ("/bin/sh".into(), None));
        assert_eq!(
            interpreter("#!/usr/bin/env python3\nprint(1)").unwrap(),
            ("/usr/bin/env".into(), Some("python3".into()))
        );
        assert_eq!(interpreter("#!/bin/bash\n").unwrap().0, "/bin/bash");
        for bad in [
            "#!bash",
            "#!/bin/sh -e -x",
            "#!/bin/s%h",
            "#!",
            "#!/bin/sh $X",
        ] {
            assert!(interpreter(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn text_is_escaped() {
        assert_eq!(unit_text("50% done\nExecStart=x"), "50%% done ExecStart=x");
    }
}
