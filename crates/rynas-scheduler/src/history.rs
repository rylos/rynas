//! Run history: one row per finished run (from the `ExecStopPost` hook), with the last
//! 256 KiB of output copied from the journal, which rotates. SQLite at
//! `<state>/scheduler.db`, written by rynasd only.
//!
//! Caps (guidelines M1): [`KEEP_PER_TASK`] runs per task and [`KEEP_TOTAL`] overall, oldest
//! dropped first; output [`OUTPUT_MAX`] bytes per run.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use rynas_api_types::scheduler::TaskRun;

/// Output kept per run.
pub const OUTPUT_MAX: usize = 256 << 10;
pub const KEEP_PER_TASK: usize = 100;
pub const KEEP_TOTAL: usize = 5000;

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
";

pub struct History {
    db: Mutex<Connection>,
}

/// A run being recorded.
#[derive(Debug, Clone, Default)]
pub struct NewRun {
    pub task_id: String,
    pub invocation_id: String,
    pub start_ms: Option<u64>,
    pub end_ms: u64,
    pub result: String,
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
}

impl NewRun {
    /// systemd says success.
    pub fn ok(&self) -> bool {
        self.result == "success"
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
    })
}

const COLS: &str = "id, task_id, invocation_id, start_ms, end_ms, result, exit_code, signal, ok, output, output_truncated";

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
        Ok(Self { db: Mutex::new(c) })
    }

    /// Records a finished run (a second report of the same invocation is ignored); returns
    /// its id.
    pub fn record(&self, r: &NewRun) -> rusqlite::Result<u64> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT OR IGNORE INTO runs (task_id, invocation_id, start_ms, end_ms, result, exit_code, signal, ok)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                r.task_id,
                r.invocation_id,
                r.start_ms.map(|v| v as i64),
                r.end_ms as i64,
                r.result,
                r.exit_code,
                r.signal,
                r.ok() as i64
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

    /// Drops the history of a deleted task.
    pub fn forget(&self, task_id: &str) -> rusqlite::Result<usize> {
        self.db
            .lock()
            .unwrap()
            .execute("DELETE FROM runs WHERE task_id = ?1", [task_id])
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
        }
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
