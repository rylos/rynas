//! The `scheduler` configuration domain: tasks → systemd units, scripts and the timers
//! drop-in; apply = files, `daemon-reload`, timers started/stopped; rollback = the same with
//! the previous value.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rynas_api_types::scheduler::{ScheduledTask, SchedulerConfig, TaskAction, TaskTrigger};
use rynas_config::files::HEADER_PREFIX;
use rynas_config::{Applier, ApplyCtx, Candidate, ManagedFile};
use rynas_tasks::Cmd;
use serde_json::Value;

use crate::calendar::{Converted, cron_to_calendar, plausible_calendar};
use crate::units::{
    Paths, TIMERS_DROPIN, UNIT_DIR, UNIT_PREFIX, interpreter, render_dropin, render_service,
    render_timer, script_content, script_path, service_name, service_path, timer_name, timer_path,
};

pub const SCHEDULER_KEY: &str = "scheduler";
/// Ids that would clash with API paths.
pub const RESERVED_IDS: &[&str] = &["preview-calendar"];
pub const MAX_COMMAND: usize = 64 << 10;
pub const MAX_SCRIPT: usize = 256 << 10;
pub const MAX_TIMEOUT_S: u64 = 7 * 86400;

/// uid of a user name, `None` when it does not exist.
pub type UidLookup = dyn Fn(&str) -> Option<u32> + Send + Sync;

/// uid lookup in the system user database (NSS).
pub fn system_uid(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 << 10];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwnam_r writes into the buffers we own
    let rc =
        unsafe { libc::getpwnam_r(c.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut out) };
    (rc == 0 && !out.is_null()).then_some(pwd.pw_uid)
}

pub struct SchedulerDomain {
    paths: Paths,
    uid: Arc<UidLookup>,
    /// Tasks of the value last rendered: `check_files` (called right after `render` within
    /// the same apply, applies being serialized) checks them against the system.
    rendered: Mutex<Vec<ScheduledTask>>,
}

impl SchedulerDomain {
    pub fn new(paths: Paths, uid: Arc<UidLookup>) -> Self {
        Self {
            paths,
            uid,
            rendered: Mutex::new(Vec::new()),
        }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }
}

/// `[a-z0-9][a-z0-9-]{0,47}`, not reserved.
pub fn valid_task_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !RESERVED_IDS.contains(&id)
}

/// `root` or a POSIX-ish user name.
pub fn valid_user(u: &str) -> bool {
    u == "root"
        || (!u.is_empty()
            && u.len() <= 32
            && (u.as_bytes()[0].is_ascii_lowercase() || u.as_bytes()[0] == b'_')
            && u.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'))
}

fn valid_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// Tasks of a configuration value (none for `None`).
pub fn tasks_of(v: Option<&Value>) -> Vec<ScheduledTask> {
    v.and_then(|v| serde_json::from_value::<SchedulerConfig>(v.clone()).ok())
        .map(|c| c.tasks)
        .unwrap_or_default()
}

/// A user task that runs as `root` (administrators only).
pub fn runs_as_root(t: &ScheduledTask) -> bool {
    t.action.user() == Some("root")
}

/// Ids of the root user tasks that going from `old` to `new` adds, changes or removes (also a
/// task that becomes or stops being a root task), sorted. Whoever brings such a change —
/// staging one task, restoring a revision, importing an archive, applying a changeset — must
/// be an administrator, like for the scheduler endpoints.
pub fn root_task_changes(old: Option<&Value>, new: Option<&Value>) -> Vec<String> {
    let (old, new) = (tasks_of(old), tasks_of(new));
    let mut ids: Vec<String> = Vec::new();
    for t in new.iter().chain(old.iter()) {
        if ids.contains(&t.id) {
            continue;
        }
        let o = old.iter().find(|x| x.id == t.id);
        let n = new.iter().find(|x| x.id == t.id);
        if o != n && (o.is_some_and(runs_as_root) || n.is_some_and(runs_as_root)) {
            ids.push(t.id.clone());
        }
    }
    ids.sort();
    ids
}

fn has_timer(t: &ScheduledTask) -> bool {
    t.trigger != TaskTrigger::Manual
}

/// Validates and normalizes one task (cron converted, name defaulted).
pub fn normalize_task(mut t: ScheduledTask) -> Result<ScheduledTask, String> {
    t.id = t.id.trim().to_string();
    if !valid_task_id(&t.id) {
        return Err(format!(
            "invalid task id {:?}: 1-48 lowercase letters, digits and '-', starting with a letter or digit",
            t.id
        ));
    }
    let id = t.id.clone();
    t.name = t.name.trim().to_string();
    if t.name.is_empty() {
        t.name = id.clone();
    }
    if t.name.len() > 128 || t.description.len() > 1024 {
        return Err(format!("task {id}: name or description too long"));
    }
    let no_nul = |s: &str| !s.contains('\0');
    match &mut t.action {
        TaskAction::Command { command, user } => {
            if command.trim().is_empty() || command.len() > MAX_COMMAND || !no_nul(command) {
                return Err(format!("task {id}: the command is empty or too long"));
            }
            if !valid_user(user) {
                return Err(format!("task {id}: invalid user {user:?}"));
            }
        }
        TaskAction::Script { script, user } => {
            if script.trim().is_empty() || script.len() > MAX_SCRIPT || !no_nul(script) {
                return Err(format!(
                    "task {id}: the script is empty or over {} KiB",
                    MAX_SCRIPT >> 10
                ));
            }
            interpreter(script).map_err(|e| format!("task {id}: {e}"))?;
            if !valid_user(user) {
                return Err(format!("task {id}: invalid user {user:?}"));
            }
        }
        TaskAction::Backup { job } => {
            if !valid_ref(job) {
                return Err(format!("task {id}: invalid backup job {job:?}"));
            }
        }
        TaskAction::Snapshot { job } => {
            if !valid_ref(job) {
                return Err(format!("task {id}: invalid snapshot job {job:?}"));
            }
        }
        TaskAction::Scrub { filesystem } => {
            let ok = filesystem.starts_with('/')
                && filesystem.len() <= 1024
                && !filesystem
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace())
                && !filesystem.split('/').any(|c| c == "..");
            if !ok {
                return Err(format!("task {id}: invalid filesystem {filesystem:?}"));
            }
        }
        TaskAction::SmartTest { disk, .. } => {
            let ok = !disk.is_empty()
                && disk.len() <= 32
                && disk
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
            if !ok {
                return Err(format!("task {id}: invalid disk {disk:?}"));
            }
        }
        TaskAction::RecycleCleanup { share, days } => {
            if share
                .as_deref()
                .is_some_and(|s| s.is_empty() || s.len() > 64 || s.contains('/'))
            {
                return Err(format!("task {id}: invalid share"));
            }
            if days.is_some_and(|d| d > 3650) {
                return Err(format!("task {id}: at most 3650 days"));
            }
        }
        TaskAction::PowerShutdown { rule } => {
            let ok = !rule.is_empty()
                && rule.len() <= 32
                && rule
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err(format!("task {id}: invalid power rule {rule:?}"));
            }
        }
    }
    t.trigger = match t.trigger {
        TaskTrigger::Calendar {
            on_calendar,
            cron: Some(cron),
            persistent,
            randomized_delay_s,
        } if !cron.trim().is_empty() => {
            let _ = on_calendar;
            match cron_to_calendar(&cron).map_err(|e| format!("task {id}: {e}"))? {
                Converted::Calendar(c) => TaskTrigger::Calendar {
                    on_calendar: c,
                    cron: None,
                    persistent,
                    randomized_delay_s,
                },
                Converted::Boot => TaskTrigger::Boot { delay_s: 60 },
            }
        }
        TaskTrigger::Calendar {
            on_calendar,
            persistent,
            randomized_delay_s,
            ..
        } => {
            let c = on_calendar.trim().to_string();
            plausible_calendar(&c).map_err(|e| format!("task {id}: {e}"))?;
            TaskTrigger::Calendar {
                on_calendar: c,
                cron: None,
                persistent,
                randomized_delay_s,
            }
        }
        other => other,
    };
    match &t.trigger {
        TaskTrigger::Calendar {
            randomized_delay_s, ..
        } if *randomized_delay_s > 86_400 => {
            return Err(format!("task {id}: the random delay is at most one day"));
        }
        TaskTrigger::Boot { delay_s } if *delay_s > 86_400 => {
            return Err(format!("task {id}: the boot delay is at most one day"));
        }
        _ => {}
    }
    if t.timeout_s.is_some_and(|s| s > MAX_TIMEOUT_S) {
        return Err(format!("task {id}: the timeout is at most 7 days"));
    }
    if t.nice.is_some_and(|n| !(-20..=19).contains(&n)) {
        return Err(format!("task {id}: nice is -20 to 19"));
    }
    if t.io_weight.is_some_and(|w| !(1..=10_000).contains(&w)) {
        return Err(format!("task {id}: the I/O weight is 1 to 10000"));
    }
    Ok(t)
}

/// A file (and every folder above it) that only root can change. A root task running a
/// file someone else can edit is a privilege escalation (the DSM "script on a share" trap).
pub fn root_only(path: &Path) -> Result<(), String> {
    let mut p = Some(path);
    while let Some(cur) = p {
        let m = std::fs::metadata(cur).map_err(|e| format!("{}: {e}", cur.display()))?;
        let mode = m.mode();
        let group_writable = mode & 0o020 != 0 && m.gid() != 0;
        // a sticky world-writable folder (/tmp) lets nobody replace root's entries in it
        let sticky_parent = m.is_dir() && mode & 0o1000 != 0 && cur != path;
        let others_writable = mode & 0o002 != 0 && !sticky_parent;
        if m.uid() != 0 || group_writable || others_writable {
            return Err(format!(
                "{} can be changed by users other than root: a task running as root cannot use it",
                cur.display()
            ));
        }
        p = cur.parent();
    }
    Ok(())
}

/// The program a command line starts, when it is an absolute path.
fn command_program(cmd: &str) -> Option<&str> {
    cmd.split_whitespace().next().filter(|w| w.starts_with('/'))
}

impl Applier for SchedulerDomain {
    fn key(&self) -> &str {
        SCHEDULER_KEY
    }

    /// After `snapshots` (30): the units run the jobs it configures.
    fn order(&self) -> i32 {
        40
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let c: SchedulerConfig = serde_json::from_value(value)
            .map_err(|e| format!("invalid scheduler document: {e}"))?;
        let mut tasks: Vec<ScheduledTask> = Vec::with_capacity(c.tasks.len());
        for t in c.tasks {
            let t = normalize_task(t)?;
            if tasks.iter().any(|x| x.id == t.id) {
                return Err(format!("task {} is defined twice", t.id).into());
            }
            tasks.push(t);
        }
        if tasks.len() > 1000 {
            return Err("at most 1000 tasks".into());
        }
        serde_json::to_value(SchedulerConfig { tasks }).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let tasks = tasks_of(Some(value));
        *self.rendered.lock().unwrap() = tasks.clone();
        let mut files = Vec::new();
        for t in &tasks {
            files.push(ManagedFile::whole(
                service_path(&t.id),
                render_service(t, &self.paths)?,
                true,
            ));
            if let Some(timer) = render_timer(t) {
                files.push(ManagedFile::whole(timer_path(&t.id), timer, true));
            } else {
                files.push(ManagedFile::absent(timer_path(&t.id)));
            }
            match script_content(t) {
                // a script has its own first line (#!): no header, the hash detects drift
                Some(s) => files.push(
                    ManagedFile::whole(script_path(&self.paths, &t.id), s, false).mode(0o700),
                ),
                None => files.push(ManagedFile::absent(script_path(&self.paths, &t.id))),
            }
        }
        files.push(ManagedFile::whole(
            TIMERS_DROPIN,
            render_dropin(&tasks),
            true,
        ));
        let known = |id: &str| tasks.iter().any(|t| t.id == id);
        // units of removed tasks (only the ones rynas generated)
        if let Ok(rd) = std::fs::read_dir(ctx.path(Path::new(UNIT_DIR))) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(rest) = name.strip_prefix(UNIT_PREFIX) else {
                    continue;
                };
                let id = rest
                    .strip_suffix(".service")
                    .or_else(|| rest.strip_suffix(".timer"))
                    .unwrap_or("");
                let ours =
                    std::fs::read_to_string(e.path()).is_ok_and(|t| t.starts_with(HEADER_PREFIX));
                if !id.is_empty() && ours && !known(id) {
                    files.push(ManagedFile::absent(Path::new(UNIT_DIR).join(&name)));
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(ctx.path(&self.paths.scripts)) {
            for e in rd.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                if valid_task_id(&id) && !known(&id) && e.path().join("run").is_file() {
                    files.push(ManagedFile::absent(script_path(&self.paths, &id)));
                }
            }
        }
        Ok(files)
    }

    fn check_files(&self, _files: &[Candidate], ctx: &ApplyCtx) -> Result<(), String> {
        let tasks = self.rendered.lock().unwrap().clone();
        self.check_value(&tasks, &|expr| {
            ctx.system
                .run(
                    &Cmd::new("systemd-analyze")
                        .args(["calendar", "--iterations=1", "--"])
                        .arg(expr)
                        .env_clear(),
                )
                .map(|_| ())
        })
    }

    fn reconcile(
        &self,
        from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        let (old, new) = (tasks_of(from), tasks_of(to));
        ctx.system.daemon_reload()?;
        for o in &old {
            let n = new.iter().find(|n| n.id == o.id);
            let timer_on = |t: &ScheduledTask| t.enabled && has_timer(t);
            if n.is_none() {
                if has_timer(o) {
                    let _ = ctx.system.stop_unit(&timer_name(&o.id));
                }
                let _ = ctx.system.stop_unit(&service_name(&o.id));
                // no "failed" leftovers of units that no longer exist
                let _ = ctx.system.run(
                    &Cmd::new("systemctl")
                        .args(["reset-failed", "--"])
                        .arg(service_name(&o.id))
                        .arg(timer_name(&o.id)),
                );
                (ctx.log)(&format!("task {}: removed", o.id));
            } else if timer_on(o) && !n.is_some_and(timer_on) {
                ctx.system.stop_unit(&timer_name(&o.id))?;
                (ctx.log)(&format!("task {}: timer stopped", o.id));
            }
        }
        for n in new.iter().filter(|t| t.enabled && has_timer(t)) {
            let unchanged = old.iter().any(|o| o == n);
            if unchanged {
                ctx.system.start_unit(&timer_name(&n.id))?;
            } else {
                ctx.system.restart_unit(&timer_name(&n.id))?;
                (ctx.log)(&format!("task {}: timer started", n.id));
            }
        }
        Ok(())
    }

    fn health(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        for t in tasks_of(value).iter().filter(|t| t.enabled && has_timer(t)) {
            if !ctx.system.unit_active(&timer_name(&t.id))? {
                return Err(format!("the timer of task {} is not active", t.id));
            }
        }
        Ok(())
    }
}

impl SchedulerDomain {
    /// Checks of tasks against the system, run by rynasd before staging and by the apply
    /// (`check_files`): calendar expressions accepted by `systemd-analyze`, users that exist, root tasks that
    /// only run root-owned files.
    pub fn check_value(
        &self,
        tasks: &[ScheduledTask],
        calendar_ok: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut seen: Vec<&str> = Vec::new();
        for t in tasks {
            if let TaskTrigger::Calendar { on_calendar, .. } = &t.trigger
                && !seen.contains(&on_calendar.as_str())
            {
                seen.push(on_calendar);
                calendar_ok(on_calendar)
                    .map_err(|e| format!("task {}: invalid calendar {on_calendar:?}: {e}", t.id))?;
            }
            let Some(user) = t.action.user() else {
                continue;
            };
            if user != "root" {
                match (self.uid)(user) {
                    Some(uid)
                        if (rynas_api_types::LOCAL_ID_MIN..=rynas_api_types::LOCAL_ID_MAX)
                            .contains(&uid) => {}
                    Some(_) => {
                        return Err(format!(
                            "task {}: {user} is a system account; tasks run as root or as a local user",
                            t.id
                        ));
                    }
                    None => return Err(format!("task {}: user {user} does not exist", t.id)),
                }
                continue;
            }
            let program = match &t.action {
                TaskAction::Command { command, .. } => command_program(command).map(str::to_string),
                TaskAction::Script { script, .. } => interpreter(script).ok().map(|(p, _)| p),
                _ => None,
            };
            if let Some(p) = program
                && Path::new(&p).exists()
            {
                root_only(Path::new(&p)).map_err(|e| format!("task {}: {e}", t.id))?;
            }
        }
        Ok(())
    }
}
