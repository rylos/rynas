//! SQLite database: connection settings and schema migrations.
//!
//! A minimal migrator instead of refinery: migrations are SQL strings applied in order, the
//! schema version is SQLite's `PRAGMA user_version`. Each migration runs in its own
//! transaction. A database written by a newer rynas (higher version) is refused.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

/// Schema migrations; index + 1 is the version they bring the database to. Never edit a
/// released migration: append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: configuration, pending changesets, revisions, managed files
    "CREATE TABLE meta (
        k TEXT PRIMARY KEY,
        v TEXT NOT NULL
    );
    CREATE TABLE config (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL,          -- JSON document
        rev   INTEGER NOT NULL        -- revision that last changed it
    );
    CREATE TABLE pending (
        owner         TEXT NOT NULL,
        key           TEXT NOT NULL,
        base          TEXT,           -- JSON value when staged (NULL: no value)
        value         TEXT,           -- JSON value to apply (NULL: remove the key)
        staged_ms     INTEGER NOT NULL,
        restored_from INTEGER,
        PRIMARY KEY (owner, key)
    );
    CREATE TABLE revisions (
        id            INTEGER PRIMARY KEY,
        author        TEXT NOT NULL,
        ts_ms         INTEGER NOT NULL,
        comment       TEXT NOT NULL,
        restored_from INTEGER,
        changes       TEXT NOT NULL,  -- JSON [{key, old, new}]
        snapshot      TEXT NOT NULL,  -- JSON {key: value} after the apply
        files         TEXT NOT NULL   -- JSON [path]
    );
    CREATE TABLE managed_files (
        path   TEXT PRIMARY KEY,
        key    TEXT NOT NULL,
        kind   TEXT NOT NULL,         -- JSON FileKind
        sha256 TEXT NOT NULL,
        rev    INTEGER
    );",
    // 2: keys of a changeset staged together that must be applied together
    "CREATE TABLE pending_groups (
        owner  TEXT NOT NULL,
        grp    INTEGER NOT NULL,
        key    TEXT NOT NULL,
        review INTEGER NOT NULL,      -- 1: only a full apply (the pending bar) commits it
        PRIMARY KEY (owner, grp, key)
    );",
];

/// Version of the schema this build writes.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Opens the database with the settings rynas relies on and migrates it.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    // WAL: readers never block the writer; NORMAL: no fsync per commit (WAL keeps the
    // database consistent, at worst the last commits are lost on power failure)
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

pub fn version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
}

/// Applies the missing migrations.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current = version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "database schema version {current} is newer than this rynas ({SCHEMA_VERSION})"
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let v = i as u32 + 1;
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", v)?;
        tx.commit()?;
        tracing::info!("configuration database migrated to schema {v}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_once_and_refuses_newer_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.db");
        let c = open(&p).unwrap();
        assert_eq!(version(&c).unwrap(), SCHEMA_VERSION);
        let mode: String = c
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(c);
        let c = open(&p).unwrap();
        c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(c);
        assert!(open(&p).is_err());
    }
}
