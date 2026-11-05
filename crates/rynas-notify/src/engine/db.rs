//! `notify.db`: schema, migrations and row mapping.
//!
//! Same conventions as `config.db` (rynas-config): WAL, `synchronous=NORMAL`, migrations as
//! SQL strings, version in `PRAGMA user_version`, a newer database is refused.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, Row};
use rynas_api_types::notify::{EventState, Notification, NotifyCategory, NotifySeverity};

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
];

pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // a small page cache (the default allows 2 MiB): the working set is a few pages
    conn.pragma_update(None, "cache_size", -256)?;
    migrate(&conn)?;
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

pub const EVENT_COLS: &str = "id, ts_ms, last_ms, count, source, category, severity, key, state, \
    title, body, fields, actions, silenced, acked_ms, acked_by, resolved_ms";

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
