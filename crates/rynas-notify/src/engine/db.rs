//! `notify.db`: schema, migrations and row mapping.
//!
//! Same conventions as `config.db` (rynas-config): WAL, `synchronous=NORMAL`, migrations as
//! SQL strings, version in `PRAGMA user_version`, a newer database is refused.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, Row};
use rynas_api_types::notify::{
    BodyFormat, EventState, Notification, NotifyCategory, NotifySeverity,
};

const MIGRATIONS: &[&str] = &[
    // 1: events, condition state per key, silences, targets, rules, outbox
    "CREATE TABLE meta (
        k TEXT PRIMARY KEY,
        v TEXT NOT NULL
    );
    CREATE TABLE events (
        id          INTEGER PRIMARY KEY,
        ts_ms       INTEGER NOT NULL,
        last_ms     INTEGER NOT NULL,
        count       INTEGER NOT NULL DEFAULT 1,
        source      TEXT NOT NULL,
        category    TEXT NOT NULL,
        severity    TEXT NOT NULL,
        key         TEXT NOT NULL,
        state       TEXT NOT NULL,
        title       TEXT NOT NULL,
        body        TEXT NOT NULL,
        fields      TEXT NOT NULL,      -- JSON object
        actions     TEXT NOT NULL,      -- JSON array
        silenced    INTEGER NOT NULL DEFAULT 0,
        acked_ms    INTEGER,
        acked_by    TEXT,
        resolved_ms INTEGER
    );
    CREATE INDEX events_key ON events (key, id);
    CREATE INDEX events_unacked ON events (id) WHERE acked_ms IS NULL;
    CREATE TABLE keys (
        key       TEXT PRIMARY KEY,
        firing    INTEGER NOT NULL,     -- 1 while the condition is open
        severity  TEXT NOT NULL,
        event_id  INTEGER NOT NULL,     -- the firing event
        since_ms  INTEGER NOT NULL,
        reminders TEXT NOT NULL DEFAULT '{}'  -- JSON {rule id: last reminder ms}
    );
    CREATE TABLE silences (
        id         INTEGER PRIMARY KEY,
        key        TEXT NOT NULL,       -- exact key, or a prefix ending with '*'
        created_ms INTEGER NOT NULL,
        created_by TEXT NOT NULL,
        until_ms   INTEGER
    );
    CREATE TABLE targets (
        id            INTEGER PRIMARY KEY,
        name          TEXT NOT NULL UNIQUE,
        enabled       INTEGER NOT NULL,
        language      TEXT NOT NULL,
        config        TEXT NOT NULL,    -- JSON TargetConfig
        secrets       BLOB,             -- age envelope of a JSON {name: value}
        secret_names  TEXT NOT NULL DEFAULT '[]',
        last_ok_ms    INTEGER,
        last_error_ms INTEGER,
        last_error    TEXT
    );
    CREATE TABLE rules (
        id   INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        doc  TEXT NOT NULL              -- JSON NotifyRule
    );
    CREATE TABLE outbox (
        id         INTEGER PRIMARY KEY,
        target_id  INTEGER NOT NULL,
        event_id   INTEGER NOT NULL,
        reminder   INTEGER NOT NULL DEFAULT 0,
        group_key  TEXT,                -- rows of one target with the same key go out together
        next_ms    INTEGER NOT NULL,    -- not before (grouping, quiet hours, retry backoff)
        attempts   INTEGER NOT NULL DEFAULT 0,
        created_ms INTEGER NOT NULL,
        last_error TEXT
    );
    CREATE INDEX outbox_due ON outbox (next_ms);
    INSERT INTO targets (name, enabled, language, config) VALUES ('Web interface', 1, 'en', '{\"kind\":\"ui\"}');",
    // 2: stable event code (translated by clients from `fields`) and body format (text|log)
    "ALTER TABLE events ADD COLUMN code TEXT NOT NULL DEFAULT '';
    ALTER TABLE events ADD COLUMN body_format TEXT NOT NULL DEFAULT 'text';",
];

/// `meta` flag of [`legacy_run_codes`]: the conversion ran (a data change, not a schema
/// step, so an older rynas still opens the database).
const LEGACY_RUN_CODES: &str = "legacy_run_codes";

pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // a small page cache (the default allows 2 MiB): the working set is a few pages
    conn.pragma_update(None, "cache_size", -256)?;
    migrate(&conn)?;
    // a failure leaves the rows as they were and is tried again at the next start
    if let Err(e) = legacy_run_codes_once(&conn) {
        tracing::warn!("notify.db: codes of old run notifications: {e}");
    }
    Ok(conn)
}

fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "notify.db schema {current} is newer than this rynas ({SCHEMA_VERSION})"
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as u32)?;
        tx.commit()?;
    }
    Ok(())
}

/// [`legacy_run_codes`] once: rows and the `meta` flag in one transaction, so a crash
/// leaves neither; afterwards one indexed read of `meta` at open.
fn legacy_run_codes_once(conn: &Connection) -> rusqlite::Result<()> {
    if meta_get(conn, LEGACY_RUN_CODES)?.is_some() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    legacy_run_codes(&tx)?;
    meta_set(&tx, LEGACY_RUN_CODES, "1")?;
    tx.commit()
}

/// Once (see [`legacy_run_codes_once`]): run notifications (snapshot and backup jobs, scheduled tasks) stored before
/// events had codes get their code and the fields their texts need (`name`, `result`,
/// `exit_code`, `signal`), read once from the English title and body they were stored with;
/// a body that is not the result line was the run's output and becomes a log body. Clients
/// then translate them from the code like newer ones and never read the English text.
fn legacy_run_codes(c: &Connection) -> rusqlite::Result<()> {
    type Row = (i64, String, String, String, String, String, String);
    let rows: Vec<Row> = {
        let mut st = c.prepare(
            "SELECT id, source, key, state, title, body, fields FROM events
             WHERE code = '' AND source IN ('snapshots', 'backup', 'scheduler')",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut up = c.prepare("UPDATE events SET code = ?1, fields = ?2, body_format = ?3 WHERE id = ?4")?;
    for (id, source, key, state, title, body, fields) in rows {
        let Some(state) = EventState::parse(&state) else { continue };
        let Some(run) = legacy_run(&source, &key, state, &title, &body) else { continue };
        let mut f: std::collections::BTreeMap<String, String> = serde_json::from_str(&fields).unwrap_or_default();
        for (k, v) in run.fields {
            f.entry(k.to_string()).or_insert(v);
        }
        let format = if run.log_body { "log" } else { "text" };
        let fields = serde_json::to_string(&f).unwrap_or_else(|_| "{}".into());
        up.execute(rusqlite::params![run.code, fields, format, id])?;
    }
    Ok(())
}

/// What [`legacy_run_codes`] gives a stored run notification.
#[derive(Debug, PartialEq, Eq)]
struct LegacyRun {
    code: String,
    fields: Vec<(&'static str, String)>,
    log_body: bool,
}

/// A run notification as `run_event` (rynasd) stored it before codes: key, title
/// "<what> <name> failed (<how>)" / "… succeeded" / "… finished" and body "Result: <result>
/// (<how>). See the task history for the output." or the run's output.
fn legacy_run(source: &str, key: &str, state: EventState, title: &str, body: &str) -> Option<LegacyRun> {
    let (family, what, prefix) = match source {
        "snapshots" => ("snapshot.job", "Snapshot job ", "snapshot:job:"),
        "backup" => ("backup.job", "Backup job ", "backup:job:"),
        "scheduler" => ("sched.task", "Task ", "scheduler:task:"),
        _ => return None,
    };
    let key = key.strip_prefix(prefix)?;
    let rest = title.strip_prefix(what)?;
    let (outcome, name) = match state {
        EventState::Firing => ("failed", rest.strip_suffix(')')?.rsplit_once(" failed (")?.0),
        EventState::Resolved => ("ok", rest.strip_suffix(" succeeded")?),
        EventState::Oneshot => ("finished", rest.strip_suffix(" finished")?),
    };
    // a job's key is its name; a task's key is its id, the title its name
    let key = if state == EventState::Oneshot { key.strip_suffix(":ok")? } else { key };
    if name.is_empty() || key.is_empty() || (family != "sched.task" && name != key) {
        return None;
    }
    let mut fields = vec![("name", name.to_string())];
    let result = body
        .strip_prefix("Result: ")
        .and_then(|b| b.strip_suffix("). See the task history for the output."))
        .and_then(|b| b.split_once(" ("));
    if let Some((result, how)) = result {
        fields.push(("result", result.to_string()));
        if let Some(code) = how.strip_prefix("exit code ").filter(|c| c.parse::<i32>().is_ok()) {
            fields.push(("exit_code", code.to_string()));
        }
        if let Some(sig) = how.strip_prefix("killed by SIG").filter(|s| !s.is_empty()) {
            fields.push(("signal", sig.to_string()));
        }
    }
    Some(LegacyRun { code: format!("{family}_{outcome}"), fields, log_body: result.is_none() && !body.is_empty() })
}

pub const EVENT_COLS: &str = "id, ts_ms, last_ms, count, source, category, severity, key, state, \
    title, body, fields, actions, silenced, acked_ms, acked_by, resolved_ms, code, body_format";

pub fn event_row(r: &Row) -> rusqlite::Result<Notification> {
    let text = |i: usize| -> rusqlite::Result<String> { r.get(i) };
    Ok(Notification {
        id: r.get::<_, i64>(0)? as u64,
        ts_ms: r.get::<_, i64>(1)? as u64,
        last_ms: r.get::<_, i64>(2)? as u64,
        count: r.get::<_, i64>(3)? as u32,
        source: text(4)?,
        category: NotifyCategory::parse(&text(5)?).unwrap_or_default(),
        severity: NotifySeverity::parse(&text(6)?).unwrap_or_default(),
        key: text(7)?,
        state: EventState::parse(&text(8)?).unwrap_or_default(),
        title: text(9)?,
        body: text(10)?,
        fields: serde_json::from_str(&text(11)?).unwrap_or_default(),
        actions: serde_json::from_str(&text(12)?).unwrap_or_default(),
        silenced: r.get::<_, i64>(13)? != 0,
        acked_ms: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        acked_by: r.get(15)?,
        resolved_ms: r.get::<_, Option<i64>>(16)?.map(|v| v as u64),
        code: text(17)?,
        body_format: BodyFormat::parse(&text(18)?).unwrap_or_default(),
    })
}

pub fn get_event(c: &Connection, id: u64) -> rusqlite::Result<Option<Notification>> {
    let mut st = c.prepare_cached(&format!("SELECT {EVENT_COLS} FROM events WHERE id = ?1"))?;
    let mut rows = st.query_map([id as i64], event_row)?;
    rows.next().transpose()
}

pub fn meta_get(c: &Connection, k: &str) -> rusqlite::Result<Option<String>> {
    let mut st = c.prepare_cached("SELECT v FROM meta WHERE k = ?1")?;
    let mut rows = st.query_map([k], |r| r.get(0))?;
    rows.next().transpose()
}

pub fn meta_set(c: &Connection, k: &str, v: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO meta (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        [k, v],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(i: usize) -> &'static str {
        MIGRATIONS[i]
    }

    fn insert(c: &Connection, source: &str, key: &str, state: &str, title: &str, body: &str, fields: &str) {
        c.execute(
            "INSERT INTO events (ts_ms, last_ms, source, category, severity, key, state, title, body, fields, actions)
             VALUES (1, 1, ?1, 'backup', 'error', ?2, ?3, ?4, ?5, ?6, '[]')",
            [source, key, state, title, body, fields],
        )
        .unwrap();
    }

    /// A database at schema 2 (the previous release's) with two run notifications of before
    /// codes.
    fn schema_2(path: &Path) -> Connection {
        let c = Connection::open(path).unwrap();
        c.execute_batch(sql(0)).unwrap();
        c.execute_batch(sql(1)).unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
        let result = "Result: exit-code (exit code 1). See the task history for the output.";
        insert(&c, "backup", "backup:job:cs", "firing", "Backup job cs failed (exit code 1)", result, "{}");
        insert(&c, "backup", "backup:job:cs", "resolved", "Backup job cs succeeded", "", "{}");
        c
    }

    fn codes(c: &Connection) -> Vec<String> {
        let mut st = c.prepare("SELECT code FROM events ORDER BY id").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    #[test]
    fn the_conversion_keeps_the_schema_and_stops_after_a_crash_or_a_flag() {
        let dir = tempfile::tempdir().unwrap();
        // a crash on the second row: no row changed, no flag; the next start does it all
        let path = dir.path().join("crash.db");
        let c = schema_2(&path);
        c.execute_batch("CREATE TRIGGER crash BEFORE UPDATE ON events WHEN NEW.id = 2 BEGIN SELECT RAISE(ABORT, 'crash'); END;").unwrap();
        drop(c);
        let c = open(&path).unwrap();
        assert_eq!(codes(&c), ["", ""]);
        assert_eq!(meta_get(&c, LEGACY_RUN_CODES).unwrap(), None);
        c.execute_batch("DROP TRIGGER crash;").unwrap();
        drop(c);
        let c = open(&path).unwrap();
        assert_eq!(codes(&c), ["backup.job_failed", "backup.job_ok"]);
        assert_eq!(meta_get(&c, LEGACY_RUN_CODES).unwrap().as_deref(), Some("1"));
        // a data change only: the previous release (schema 2) still opens the database
        let v: u32 = c.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!((v, SCHEMA_VERSION), (2, 2));
        // a database with the flag already set is left alone
        let path = dir.path().join("flagged.db");
        let c = schema_2(&path);
        meta_set(&c, LEGACY_RUN_CODES, "1").unwrap();
        drop(c);
        assert_eq!(codes(&open(&path).unwrap()), ["", ""]);
    }

    #[test]
    fn legacy_run_notifications_get_codes_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.db");
        {
            // a database at schema 2 with notifications of before codes, and one with a code
            let c = Connection::open(&path).unwrap();
            c.execute_batch(sql(0)).unwrap();
            c.execute_batch(sql(1)).unwrap();
            c.pragma_update(None, "user_version", 2).unwrap();
            let result = "Result: exit-code (exit code 1). See the task history for the output.";
            insert(&c, "backup", "backup:job:cs", "firing", "Backup job cs failed (exit code 1)", result, "{}");
            insert(&c, "snapshots", "snapshot:job:docs", "firing", "Snapshot job docs failed (killed by SIGTERM)", "Result: signal (killed by SIGTERM). See the task history for the output.", r#"{"task":"snapshot-docs","result":"signal"}"#);
            insert(&c, "snapshots", "snapshot:job:docs:ok", "oneshot", "Snapshot job docs finished", "ERROR: output line", "{}");
            insert(&c, "scheduler", "scheduler:task:t1", "resolved", "Task Nightly sync succeeded", "Result: success (success). See the task history for the output.", "{}");
            // not a run notification of rynas, or a title that does not fit its key
            insert(&c, "third-party", "backup:job:cs", "firing", "Backup job cs failed (exit code 1)", result, "{}");
            insert(&c, "backup", "backup:job:cs", "firing", "Backup job other failed (exit code 1)", result, "{}");
            c.execute("INSERT INTO events (ts_ms, last_ms, source, category, severity, key, state, title, body, fields, actions, code)
                 VALUES (1, 1, 'backup', 'backup', 'error', 'backup:job:x', 'firing', 'Backup job x failed (exit code 2)', '', '{}', '[]', 'backup.job_failed')", []).unwrap();
        }
        let c = open(&path).unwrap();
        let v: u32 = c.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let n = |id| get_event(&c, id).unwrap().unwrap();
        let f = |n: &Notification, k: &str| n.fields.get(k).cloned().unwrap_or_default();
        let b = n(1);
        assert_eq!((b.code.as_str(), f(&b, "name"), f(&b, "result"), f(&b, "exit_code")), ("backup.job_failed", "cs".into(), "exit-code".into(), "1".into()));
        assert_eq!(b.body_format, BodyFormat::Text);
        let s = n(2);
        assert_eq!((s.code.as_str(), f(&s, "name"), f(&s, "signal"), f(&s, "task")), ("snapshot.job_failed", "docs".into(), "TERM".into(), "snapshot-docs".into()));
        let o = n(3);
        assert_eq!((o.code.as_str(), f(&o, "name"), o.body_format), ("snapshot.job_finished", "docs".into(), BodyFormat::Log));
        let t = n(4);
        assert_eq!((t.code.as_str(), f(&t, "name"), f(&t, "result")), ("sched.task_ok", "Nightly sync".into(), "success".into()));
        assert_eq!((n(5).code.as_str(), n(6).code.as_str()), ("", ""));
        assert!(n(7).fields.is_empty(), "a coded row is left alone");
        // once: a later row of the same shape stays as written
        insert(&c, "backup", "backup:job:cs", "firing", "Backup job cs failed (exit code 1)", "", "{}");
        drop(c);
        let c = open(&path).unwrap();
        assert_eq!(get_event(&c, 8).unwrap().unwrap().code, "");
    }

    #[test]
    fn migration_2_keeps_old_events_and_adds_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.db");
        {
            // a database at schema 1 with one event
            let c = Connection::open(&path).unwrap();
            c.execute_batch(sql(0)).unwrap();
            c.pragma_update(None, "user_version", 1).unwrap();
            c.execute(
                "INSERT INTO events (ts_ms, last_ms, source, category, severity, key, state,
                   title, body, fields, actions)
                 VALUES (1, 1, 'storage', 'storage', 'critical', 'k', 'firing', 'RAID md2 degraded',
                   '', '{\"array\":\"md2\"}', '[]')",
                [],
            )
            .unwrap();
        }
        let c = open(&path).unwrap();
        let v: u32 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let n = get_event(&c, 1).unwrap().unwrap();
        assert_eq!(n.title, "RAID md2 degraded");
        assert_eq!(n.code, "");
        assert_eq!(n.body_format, BodyFormat::Text);
        assert_eq!(n.fields["array"], "md2");
        c.execute(
            "UPDATE events SET code = 'storage.raid_health', body_format = 'log' WHERE id = 1",
            [],
        )
        .unwrap();
        let n = get_event(&c, 1).unwrap().unwrap();
        assert_eq!(
            (n.code.as_str(), n.body_format),
            ("storage.raid_health", BodyFormat::Log)
        );
        drop(c);
        // reopening is a no-op
        assert!(open(&path).is_ok());
    }
}
