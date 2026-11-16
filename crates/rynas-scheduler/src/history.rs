//! Run history: one row per finished run (from the `ExecStopPost` hook), with the last
//! 256 KiB of output copied from the journal, which rotates, and one row per start the
//! scheduling gate skipped (paused, missed, overlap: `skipped` set, `result` `skipped`).
//! SQLite at `<state>/scheduler.db`, written by rynasd only. Two small tables beside it: the
//! gate's note of each start it let through (`gates`: how it started, read back when the run
//! ends) and the retries armed after failed runs (`retries`, one per task, kept across
//! restarts and reboots).
//!
//! Caps (guidelines M1): [`KEEP_PER_TASK`] runs per task and [`KEEP_TOTAL`] overall, oldest
//! dropped first; output [`OUTPUT_MAX`] bytes per run; [`KEEP_GATES`] gate notes.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use rynas_api_types::scheduler::{PendingRetry, RunTrigger, SkipReason, TaskRun};

/// Output kept per run.
pub const OUTPUT_MAX: usize = 256 << 10;
pub const KEEP_PER_TASK: usize = 100;
pub const KEEP_TOTAL: usize = 5000;
pub const KEEP_GATES: usize = 2000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
    id               INTEGER PRIMARY KEY,
    task_id          TEXT NOT NULL,
    invocation_id    TEXT NOT NULL UNIQUE,
    start_ms         INTEGER,
    end_ms           INTEGER NOT NULL,
    result           TEXT NOT NULL,
    exit_code        INTEGER,
    signal           TEXT,
    ok               INTEGER NOT NULL,
    output           TEXT NOT NULL DEFAULT '',
    output_truncated INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS runs_task ON runs (task_id, id);
CREATE TABLE IF NOT EXISTS gates (
    invocation_id TEXT PRIMARY KEY,
    task_id       TEXT NOT NULL,
    at_ms         INTEGER NOT NULL,
    trigger       TEXT,
    attempt       INTEGER NOT NULL DEFAULT 0,
    ungated       INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS retries (
    task_id  TEXT PRIMARY KEY,
    attempt  INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    due_ms   INTEGER NOT NULL,
    after_ms INTEGER NOT NULL
);
";

/// Columns added to `runs` by the scheduling gate (older databases get them on open).
const ADDED: [(&str, &str); 5] = [
    ("trigger", "TEXT"),
    ("skipped", "TEXT"),
    ("attempt", "INTEGER NOT NULL DEFAULT 0"),
    ("detail", "TEXT"),
    ("ungated", "INTEGER NOT NULL DEFAULT 0"),
];

/// What the gate noted about a start it let through.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GateNote {
    pub invocation_id: String,
    pub task_id: String,
    pub at_ms: u64,
    pub trigger: Option<RunTrigger>,
    pub attempt: u8,
    /// rynasd was not asked (`rynasctl gate` could not reach it).
    pub ungated: bool,
}

/// A retry armed after a failed run (the run ended at `after_ms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmedRetry {
    pub retry: PendingRetry,
    pub after_ms: u64,
}

fn word<T: serde::Serialize>(v: &T) -> Option<String> {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

fn from_word<T: serde::de::DeserializeOwned>(s: Option<String>) -> Option<T> {
    s.and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok())
}

pub struct History {
    db: Mutex<Connection>,
}

/// A run (or a skipped start) being recorded.
#[derive(Debug, Clone, Default)]
pub struct NewRun {
    pub task_id: String,
    pub invocation_id: String,
    pub start_ms: Option<u64>,
    pub end_ms: u64,
    pub result: String,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub trigger: Option<RunTrigger>,
    pub skipped: Option<SkipReason>,
    pub attempt: u8,
    pub detail: Option<String>,
    pub ungated: bool,
}

impl NewRun {
    /// systemd says success (a skipped start never is).
    pub fn ok(&self) -> bool {
        self.result == "success" && self.skipped.is_none()
    }

    /// A start the gate skipped.
    pub fn skipped(
        task_id: &str,
        invocation_id: &str,
        at_ms: u64,
        trigger: RunTrigger,
        reason: SkipReason,
        detail: Option<String>,
    ) -> NewRun {
        NewRun {
            task_id: task_id.into(),
            invocation_id: invocation_id.into(),
            start_ms: Some(at_ms),
            end_ms: at_ms,
            result: "skipped".into(),
            trigger: Some(trigger),
            skipped: Some(reason),
            detail,
            ..NewRun::default()
        }
    }
}

fn row(r: &rusqlite::Row, with_output: bool) -> rusqlite::Result<TaskRun> {
    let output: String = r.get(9)?;
    Ok(TaskRun {
        id: r.get::<_, i64>(0)? as u64,
        task_id: r.get(1)?,
        invocation_id: r.get(2)?,
        start_ms: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
        end_ms: r.get::<_, i64>(4)? as u64,
        result: r.get(5)?,
        exit_code: r.get(6)?,
        signal: r.get(7)?,
        ok: r.get::<_, i64>(8)? != 0,
        output_bytes: output.len() as u64,
        output_truncated: r.get::<_, i64>(10)? != 0,
        output: with_output.then_some(output),
        trigger: from_word(r.get(11)?),
        skipped: from_word(r.get(12)?),
        attempt: r.get::<_, i64>(13)?.clamp(0, 255) as u8,
        detail: r.get(14)?,
        ungated: r.get::<_, i64>(15)? != 0,
    })
}

const COLS: &str = "id, task_id, invocation_id, start_ms, end_ms, result, exit_code, signal, ok, output, output_truncated, trigger, skipped, attempt, detail, ungated";

impl History {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let c = Connection::open(path)?;
        c.busy_timeout(Duration::from_secs(5))?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "synchronous", "NORMAL")?;
        c.execute_batch(SCHEMA)?;
        let have: Vec<String> = c
            .prepare("SELECT name FROM pragma_table_info('runs')")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for (col, ty) in ADDED {
            if !have.iter().any(|h| h == col) {
                c.execute_batch(&format!("ALTER TABLE runs ADD COLUMN {col} {ty}"))?;
            }
        }
        Ok(Self { db: Mutex::new(c) })
    }

    /// Records a finished run or a skipped start (a second report of the same invocation is
    /// ignored: the first one stays); returns its id.
    pub fn record(&self, r: &NewRun) -> rusqlite::Result<u64> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO runs (task_id, invocation_id, start_ms, end_ms, result, exit_code, signal, ok,
                                         trigger, skipped, attempt, detail, ungated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                r.task_id,
                r.invocation_id,
                r.start_ms.map(|v| v as i64),
                r.end_ms as i64,
                r.result,
                r.exit_code,
                r.signal,
                r.ok() as i64,
                r.trigger.as_ref().and_then(word),
                r.skipped.as_ref().and_then(word),
                r.attempt as i64,
                r.detail,
                r.ungated as i64
            ],
        )?;
        let id: i64 = db.query_row(
            "SELECT id FROM runs WHERE invocation_id = ?1",
            [&r.invocation_id],
            |row| row.get(0),
        )?;
        // retention: per task, then overall
        db.execute(
            "DELETE FROM runs WHERE task_id = ?1 AND id NOT IN
               (SELECT id FROM runs WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2)",
            params![r.task_id, KEEP_PER_TASK as i64],
        )?;
        db.execute(
            "DELETE FROM runs WHERE id NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT ?1)",
            [KEEP_TOTAL as i64],
        )?;
        Ok(id as u64)
    }

    /// Stores the output of a run (already cut to [`OUTPUT_MAX`]).
    pub fn set_output(&self, id: u64, output: &str, truncated: bool) -> rusqlite::Result<()> {
        let text = crate::probe::tail(output, OUTPUT_MAX);
        self.db.lock().unwrap().execute(
            "UPDATE runs SET output = ?2, output_truncated = ?3 WHERE id = ?1",
            params![id as i64, text.text, (truncated || text.truncated) as i64],
        )?;
        Ok(())
    }

    /// Runs of a task, newest first, without output.
    pub fn list(
        &self,
        task_id: &str,
        limit: usize,
        before: Option<u64>,
    ) -> rusqlite::Result<Vec<TaskRun>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(&format!(
            "SELECT {COLS} FROM runs WHERE task_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3"
        ))?;
        st.query_map(
            params![task_id, before.map_or(i64::MAX, |b| b as i64), limit as i64],
            |r| row(r, false),
        )?
        .collect()
    }

    /// One run with its output.
    pub fn get(&self, task_id: &str, id: u64) -> rusqlite::Result<Option<TaskRun>> {
        let db = self.db.lock().unwrap();
        db.query_row(
            &format!("SELECT {COLS} FROM runs WHERE task_id = ?1 AND id = ?2"),
            params![task_id, id as i64],
            |r| row(r, true),
        )
        .optional()
    }

    /// The latest run of every task.
    pub fn latest(&self) -> rusqlite::Result<std::collections::HashMap<String, TaskRun>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare(&format!(
            "SELECT {COLS} FROM runs WHERE id IN (SELECT max(id) FROM runs GROUP BY task_id)"
        ))?;
        st.query_map([], |r| row(r, false))?
            .map(|r| r.map(|t| (t.task_id.clone(), t)))
            .collect()
    }

    /// Drops the history of a deleted task (its gate notes and armed retry too).
    pub fn forget(&self, task_id: &str) -> rusqlite::Result<usize> {
        let db = self.db.lock().unwrap();
        db.execute("DELETE FROM gates WHERE task_id = ?1", [task_id])?;
        db.execute("DELETE FROM retries WHERE task_id = ?1", [task_id])?;
        db.execute("DELETE FROM runs WHERE task_id = ?1", [task_id])
    }

    /// The record of one invocation, if any (without output).
    pub fn by_invocation(&self, invocation_id: &str) -> rusqlite::Result<Option<TaskRun>> {
        let db = self.db.lock().unwrap();
        db.query_row(
            &format!("SELECT {COLS} FROM runs WHERE invocation_id = ?1"),
            [invocation_id],
            |r| row(r, false),
        )
        .optional()
    }

    /// Start and end of the latest run of a task that really ran (skipped starts left out).
    pub fn last_ran(&self, task_id: &str) -> rusqlite::Result<Option<(u64, u64)>> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT start_ms, end_ms FROM runs WHERE task_id = ?1 AND skipped IS NULL ORDER BY id DESC LIMIT 1",
            [task_id],
            |r| {
                let end = r.get::<_, i64>(1)? as u64;
                Ok((r.get::<_, Option<i64>>(0)?.map_or(end, |s| s as u64), end))
            },
        )
        .optional()
    }

    /// Notes how a start the gate let through came about.
    pub fn note_gate(&self, n: &GateNote) -> rusqlite::Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR REPLACE INTO gates (invocation_id, task_id, at_ms, trigger, attempt, ungated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                n.invocation_id,
                n.task_id,
                n.at_ms as i64,
                n.trigger.as_ref().and_then(word),
                n.attempt as i64,
                n.ungated as i64
            ],
        )?;
        db.execute(
            "DELETE FROM gates WHERE rowid NOT IN (SELECT rowid FROM gates ORDER BY at_ms DESC LIMIT ?1)",
            [KEEP_GATES as i64],
        )?;
        Ok(())
    }

    /// The gate's note of an invocation.
    pub fn gate_note(&self, invocation_id: &str) -> rusqlite::Result<Option<GateNote>> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT invocation_id, task_id, at_ms, trigger, attempt, ungated FROM gates WHERE invocation_id = ?1",
            [invocation_id],
            |r| {
                Ok(GateNote {
                    invocation_id: r.get(0)?,
                    task_id: r.get(1)?,
                    at_ms: r.get::<_, i64>(2)? as u64,
                    trigger: from_word(r.get(3)?),
                    attempt: r.get::<_, i64>(4)?.clamp(0, 255) as u8,
                    ungated: r.get::<_, i64>(5)? != 0,
                })
            },
        )
        .optional()
    }

    /// Arms (or replaces) the retry of a task.
    pub fn set_retry(&self, task_id: &str, a: &ArmedRetry) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO retries (task_id, attempt, attempts, due_ms, after_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                task_id,
                a.retry.attempt as i64,
                a.retry.attempts as i64,
                a.retry.due_ms as i64,
                a.after_ms as i64
            ],
        )?;
        Ok(())
    }

    /// Drops the retry of a task; `true` when there was one.
    pub fn clear_retry(&self, task_id: &str) -> rusqlite::Result<bool> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .execute("DELETE FROM retries WHERE task_id = ?1", [task_id])?
            > 0)
    }

    /// Every armed retry.
    pub fn retries(&self) -> rusqlite::Result<Vec<(String, ArmedRetry)>> {
        let db = self.db.lock().unwrap();
        let mut st = db.prepare("SELECT task_id, attempt, attempts, due_ms, after_ms FROM retries")?;
        st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ArmedRetry {
                    retry: PendingRetry {
                        attempt: r.get::<_, i64>(1)?.clamp(0, 255) as u8,
                        attempts: r.get::<_, i64>(2)?.clamp(0, 255) as u8,
                        due_ms: r.get::<_, i64>(3)? as u64,
                    },
                    after_ms: r.get::<_, i64>(4)? as u64,
                },
            ))
        })?
        .collect()
    }

    /// A run of the task that really ran ended after `ms`.
    pub fn ran_since(&self, task_id: &str, ms: u64) -> rusqlite::Result<bool> {
        let db = self.db.lock().unwrap();
        db.query_row(
            "SELECT EXISTS (SELECT 1 FROM runs WHERE task_id = ?1 AND skipped IS NULL AND end_ms > ?2)",
            params![task_id, ms as i64],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(task: &str, inv: &str, result: &str) -> NewRun {
        NewRun {
            task_id: task.into(),
            invocation_id: inv.into(),
            start_ms: Some(1000),
            end_ms: 2000,
            result: result.into(),
            exit_code: Some(if result == "success" { 0 } else { 3 }),
            signal: None,
            ..NewRun::default()
        }
    }

    #[test]
    fn skipped_starts_gate_notes_and_retries() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("scheduler.db");
        // a database of an older rynas: the new columns are added on open
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE runs (id INTEGER PRIMARY KEY, task_id TEXT NOT NULL, invocation_id TEXT NOT NULL UNIQUE,
                 start_ms INTEGER, end_ms INTEGER NOT NULL, result TEXT NOT NULL, exit_code INTEGER, signal TEXT,
                 ok INTEGER NOT NULL, output TEXT NOT NULL DEFAULT '', output_truncated INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO runs (task_id, invocation_id, end_ms, result, ok) VALUES ('t', 'old1', 500, 'success', 1);",
            )
            .unwrap();
        }
        let h = History::open(&path).unwrap();
        let old = h.list("t", 10, None).unwrap();
        assert_eq!((old[0].trigger, old[0].skipped, old[0].attempt), (None, None, 0));
        let s = NewRun::skipped("t", "cc01", 3000, RunTrigger::Timer, SkipReason::PausedAll, Some("disk swap".into()));
        assert!(!s.ok());
        let sid = h.record(&s).unwrap();
        // the ExecStopPost report of the same invocation keeps the skipped row
        assert_eq!(h.record(&run("t", "cc01", "exec-condition")).unwrap(), sid);
        let got = h.by_invocation("cc01").unwrap().unwrap();
        assert_eq!(got.skipped, Some(SkipReason::PausedAll));
        assert_eq!(got.trigger, Some(RunTrigger::Timer));
        assert_eq!(got.detail.as_deref(), Some("disk swap"));
        assert_eq!(got.result, "skipped");
        // a skip is not a run
        assert_eq!(h.last_ran("t").unwrap(), Some((500, 500)));
        assert!(!h.ran_since("t", 600).unwrap());
        let mut r = run("t", "cc02", "exit-code");
        r.trigger = Some(RunTrigger::Retry);
        r.attempt = 2;
        h.record(&r).unwrap();
        assert_eq!(h.last_ran("t").unwrap(), Some((1000, 2000)));
        assert!(h.ran_since("t", 600).unwrap());
        assert_eq!(h.list("t", 1, None).unwrap()[0].attempt, 2);

        let n = GateNote { invocation_id: "cc03".into(), task_id: "t".into(), at_ms: 9, trigger: Some(RunTrigger::CatchUp), attempt: 0, ungated: true };
        h.note_gate(&n).unwrap();
        assert_eq!(h.gate_note("cc03").unwrap(), Some(n));
        assert_eq!(h.gate_note("nope").unwrap(), None);

        let a = ArmedRetry { retry: PendingRetry { attempt: 1, attempts: 3, due_ms: 70 }, after_ms: 60 };
        h.set_retry("t", &a).unwrap();
        assert_eq!(h.retries().unwrap(), vec![("t".to_string(), a)]);
        assert!(h.clear_retry("t").unwrap());
        assert!(!h.clear_retry("t").unwrap());
        h.set_retry("t", &a).unwrap();
        h.forget("t").unwrap();
        assert!(h.retries().unwrap().is_empty() && h.gate_note("cc03").unwrap().is_none());
    }

    #[test]
    fn records_lists_and_caps() {
        let d = tempfile::tempdir().unwrap();
        let h = History::open(&d.path().join("scheduler.db")).unwrap();
        let a = h.record(&run("backup", "aa01", "success")).unwrap();
        let b = h.record(&run("backup", "aa02", "exit-code")).unwrap();
        assert_eq!(
            h.record(&run("backup", "aa02", "exit-code")).unwrap(),
            b,
            "same invocation"
        );
        h.set_output(b, "line 1\nline 2\n", false).unwrap();
        let l = h.list("backup", 50, None).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].id, b);
        assert!(!l[0].ok && l[1].ok);
        assert_eq!(l[0].output, None);
        assert_eq!(l[0].output_bytes, 14);
        assert_eq!(h.list("backup", 50, Some(b)).unwrap()[0].id, a);
        let one = h.get("backup", b).unwrap().unwrap();
        assert_eq!(one.output.as_deref(), Some("line 1\nline 2\n"));
        assert!(h.get("other", b).unwrap().is_none());
        assert_eq!(h.latest().unwrap()["backup"].id, b);

        for i in 0..(KEEP_PER_TASK + 5) {
            h.record(&run("busy", &format!("bb{i:04}"), "success"))
                .unwrap();
        }
        assert_eq!(h.list("busy", 500, None).unwrap().len(), KEEP_PER_TASK);
        let big = "x\n".repeat(OUTPUT_MAX);
        h.set_output(a, &big, false).unwrap();
        let g = h.get("backup", a).unwrap().unwrap();
        assert!(g.output_truncated && g.output_bytes <= OUTPUT_MAX as u64);
        assert_eq!(h.forget("busy").unwrap(), KEEP_PER_TASK);
    }
}
