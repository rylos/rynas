//! The configuration store: pending changesets, apply pipeline with rollback, revisions,
//! restore and drift detection.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use rynas_api_types::{
    DiffEntry, DriftEntry, DriftReport, DriftStatus, PendingChange, PendingChanges, RevisionDetail,
    RevisionSummary,
};
use rynas_tasks::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::diff;
use crate::domain::{Applier, ApplyCtx, Candidate, Invalid};
use crate::files::{
    FileKind, ManagedFile, candidate_path, current_hash, rename_synced, render_final, under_root,
    write_synced,
};
use crate::system::System;

/// Where the store lives and how much history it keeps.
#[derive(Debug, Clone)]
pub struct ConfigOptions {
    /// `config.db` and `backup/<rev>/` go here.
    pub state_dir: PathBuf,
    /// Managed files are written below this directory: "/" in production, a scratch
    /// directory for tests and `--dry-system`.
    pub root: PathBuf,
    /// Revisions kept...
    pub keep_revisions: usize,
    /// ...and for at most this long (the latest revision is always kept).
    pub keep_age: Duration,
}

impl ConfigOptions {
    pub fn new(state_dir: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            root: root.into(),
            keep_revisions: 200,
            keep_age: Duration::from_secs(365 * 86400),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// No domain registered for the key.
    UnknownKey(String),
    /// The value failed validation.
    Invalid(Invalid),
    NothingPending,
    /// Keys whose applied value changed after they were staged.
    Conflict(Vec<String>),
    /// A partial apply selects a key of a review group ([`Config::stage_group`] with
    /// `review`: a destructive change such as a folder deletion), which only a full apply
    /// commits. `asked`: the selected key that brings the group in (itself a member, or
    /// joined to it through plain groups); `group`: the pending keys of the group.
    GroupSplit { asked: String, group: Vec<String> },
    NotFound(String),
    /// The apply failed and was rolled back.
    Apply(String),
    Cancelled,
    Storage(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::UnknownKey(k) => write!(f, "unknown configuration key {k:?}"),
            ConfigError::Invalid(e) => write!(f, "{e}"),
            ConfigError::NothingPending => write!(f, "there are no pending changes"),
            ConfigError::Conflict(keys) => write!(
                f,
                "{} changed after the change was staged: discard and stage it again",
                keys.join(", ")
            ),
            ConfigError::GroupSplit { asked, group } if group.len() == 1 && group[0] == *asked => write!(
                f,
                "the pending change of {asked} is a destructive step (such as deleting a folder) that is applied only with the whole changeset: review and apply it from the pending changes (apply without keys)"
            ),
            ConfigError::GroupSplit { asked, group } => write!(
                f,
                "the pending change of {asked} was staged together with a destructive step on {} (such as deleting a folder) that is applied only with the whole changeset: review and apply it from the pending changes (apply without keys)",
                group.join(", ")
            ),
            ConfigError::NotFound(what) => write!(f, "{what} not found"),
            ConfigError::Apply(e) => write!(f, "{e}"),
            ConfigError::Cancelled => write!(f, "cancelled"),
            ConfigError::Storage(e) => write!(f, "configuration storage: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<rusqlite::Error> for ConfigError {
    fn from(e: rusqlite::Error) -> Self {
        ConfigError::Storage(e.to_string())
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Storage(e.to_string())
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::Storage(e.to_string())
    }
}

pub type Result<T, E = ConfigError> = std::result::Result<T, E>;

/// One key of a changeset being applied.
struct Step {
    key: String,
    applier: Arc<dyn Applier>,
    old: Option<Value>,
    new: Option<Value>,
}

/// A file written (or found already identical) by an apply.
struct Written {
    path: PathBuf,
    key: String,
    kind: FileKind,
    hash: String,
}

/// How to undo the replacement of one file.
struct Restore {
    target: PathBuf,
    /// Copy of the previous file; `None`: there was none, delete the target.
    backup: Option<PathBuf>,
    /// `(mode, uid, gid)` of the previous file: put back with it (a file of group `nut`
    /// read by a daemon that dropped privileges must not come back as `root:root`).
    owner: Option<(u32, u32, u32)>,
}

#[derive(Serialize, Deserialize)]
struct StoredChange {
    key: String,
    old: Option<Value>,
    new: Option<Value>,
}

fn to_json(v: &Option<Value>) -> Option<String> {
    v.as_ref().map(Value::to_string)
}

fn from_json(s: Option<String>) -> Result<Option<Value>> {
    Ok(match s {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    })
}

fn no_log(_: &str) {}

/// The configuration store. `rynasd` is its only writer.
pub struct Config {
    db: Mutex<Connection>,
    appliers: BTreeMap<String, Arc<dyn Applier>>,
    templates: minijinja::Environment<'static>,
    system: Arc<dyn System>,
    opts: ConfigOptions,
    /// One apply at a time.
    apply_lock: Mutex<()>,
    /// Bumped whenever a changeset may look different: a stage, a discard, an apply (which
    /// also changes the `conflict` flags of the other users' changesets).
    pending_gen: AtomicU64,
    /// Called with the new generation after each bump ([`Config::on_pending_change`]).
    pending_listener: OnceLock<PendingListener>,
}

/// See [`Config::on_pending_change`].
pub type PendingListener = Box<dyn Fn(u64) + Send + Sync>;

impl Config {
    /// Opens (creates, migrates) `<state_dir>/config.db` with the given domains.
    pub fn open(
        opts: ConfigOptions,
        system: Arc<dyn System>,
        appliers: Vec<Arc<dyn Applier>>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&opts.state_dir)?;
        let db = crate::db::open(&opts.state_dir.join("config.db"))?;
        let mut templates = minijinja::Environment::new();
        templates.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        let mut map = BTreeMap::new();
        for a in appliers {
            for (name, src) in a.templates() {
                templates
                    .add_template_owned(name.clone(), src)
                    .map_err(|e| ConfigError::Invalid(format!("template {name}: {e}").into()))?;
            }
            map.insert(a.key().to_string(), a);
        }
        Ok(Self {
            db: Mutex::new(db),
            appliers: map,
            templates,
            system,
            opts,
            apply_lock: Mutex::new(()),
            pending_gen: AtomicU64::new(0),
            pending_listener: OnceLock::new(),
        })
    }

    /// Generation of the pending changesets: it changes whenever some changeset may have
    /// changed (stage, discard, apply). Not persisted: starts at 0 with the process.
    pub fn pending_generation(&self) -> u64 {
        self.pending_gen.load(Ordering::Relaxed)
    }

    /// Registers the one function called (on the calling thread, keep it quick) with the
    /// new [`Config::pending_generation`] after every change; later calls are ignored.
    pub fn on_pending_change(&self, f: PendingListener) {
        let _ = self.pending_listener.set(f);
    }

    fn pending_changed(&self) {
        let g = self.pending_gen.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(f) = self.pending_listener.get() {
            f(g);
        }
    }

    pub fn options(&self) -> &ConfigOptions {
        &self.opts
    }

    pub fn system(&self) -> &dyn System {
        self.system.as_ref()
    }

    /// Registered configuration keys.
    pub fn keys(&self) -> Vec<String> {
        self.appliers.keys().cloned().collect()
    }

    pub(crate) fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap()
    }

    fn ctx(&self) -> ApplyCtx<'_> {
        self.ctx_logging(&no_log)
    }

    fn ctx_logging<'a>(&'a self, log: &'a dyn Fn(&str)) -> ApplyCtx<'a> {
        ApplyCtx {
            root: &self.opts.root,
            system: self.system.as_ref(),
            templates: &self.templates,
            log,
            replaced: Mutex::new(Vec::new()),
            restarted: Mutex::new(Vec::new()),
        }
    }

    fn applier(&self, key: &str) -> Result<&Arc<dyn Applier>> {
        self.appliers
            .get(key)
            .ok_or_else(|| ConfigError::UnknownKey(key.into()))
    }

    /// The applied value of a key (`None` if rynas never applied it).
    pub fn applied(&self, key: &str) -> Result<Option<Value>> {
        let s: Option<String> = self
            .db()
            .query_row("SELECT value FROM config WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        from_json(s)
    }

    /// The applied value, or what the system has when the key was never applied.
    pub fn current(&self, key: &str) -> Result<Option<Value>> {
        let a = self.applier(key)?;
        if let Some(v) = self.applied(key)? {
            return Ok(Some(v));
        }
        a.import(&self.ctx())
            .map_err(|e| ConfigError::Invalid(e.into()))
    }

    /// The value `owner` has staged for `key`: `None` if nothing is staged, `Some(None)` if
    /// the key is staged for removal.
    pub fn pending_value(&self, owner: &str, key: &str) -> Result<Option<Option<Value>>> {
        let row: Option<Option<String>> = self
            .db()
            .query_row(
                "SELECT value FROM pending WHERE owner = ?1 AND key = ?2",
                params![owner, key],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            Some(v) => Ok(Some(from_json(v)?)),
            None => Ok(None),
        }
    }

    /// Stages a new value (`None`: remove the key) in `owner`'s changeset. Staging the value
    /// the key already has drops the pending change.
    pub fn stage(&self, owner: &str, key: &str, value: Option<Value>) -> Result<bool> {
        self.stage_inner(owner, key, value, None)
    }

    fn stage_inner(
        &self,
        owner: &str,
        key: &str,
        value: Option<Value>,
        restored_from: Option<u64>,
    ) -> Result<bool> {
        let r = self.stage_row(owner, key, value, restored_from);
        if r.is_ok() {
            self.pending_changed();
        }
        r
    }

    fn stage_row(
        &self,
        owner: &str,
        key: &str,
        value: Option<Value>,
        restored_from: Option<u64>,
    ) -> Result<bool> {
        let a = self.applier(key)?;
        let value = match value {
            Some(v) => Some(a.validate(v).map_err(ConfigError::Invalid)?),
            None => None,
        };
        let current = self.current(key)?;
        let db = self.db();
        let base: Option<Option<String>> = db
            .query_row(
                "SELECT base FROM pending WHERE owner = ?1 AND key = ?2",
                params![owner, key],
                |r| r.get(0),
            )
            .optional()?;
        // the base stays the value seen when the key was first staged
        let base = match base {
            Some(b) => from_json(b)?,
            None => current,
        };
        if value == base {
            db.execute(
                "DELETE FROM pending WHERE owner = ?1 AND key = ?2",
                params![owner, key],
            )?;
            db.execute(
                "DELETE FROM pending_groups WHERE owner = ?1 AND key = ?2",
                params![owner, key],
            )?;
            return Ok(false);
        }
        db.execute(
            "INSERT INTO pending (owner, key, base, value, staged_ms, restored_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (owner, key) DO UPDATE SET
               value = excluded.value, staged_ms = excluded.staged_ms,
               restored_from = excluded.restored_from",
            params![
                owner,
                key,
                to_json(&base),
                to_json(&value),
                now_ms() as i64,
                restored_from.map(|r| r as i64)
            ],
        )?;
        Ok(true)
    }

    /// `owner`'s changeset with the diff of each key against the current configuration.
    pub fn pending(&self, owner: &str) -> Result<PendingChanges> {
        let rows = {
            let db = self.db();
            let mut st = db.prepare(
                "SELECT key, base, value, staged_ms FROM pending WHERE owner = ?1 ORDER BY key",
            )?;
            st.query_map([owner], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut changes = Vec::new();
        for (key, base, value, staged) in rows {
            let (base, value) = (from_json(base)?, from_json(value)?);
            let current = match self.current(&key) {
                Ok(c) => c,
                Err(ConfigError::UnknownKey(_)) => base.clone(),
                Err(e) => return Err(e),
            };
            let diff = diff(&key, current.as_ref(), value.as_ref());
            changes.push(PendingChange {
                diff_count: diff.len() as u32,
                diff,
                conflict: current != base,
                key,
                base,
                value,
                staged_ms: staged as u64,
            });
        }
        Ok(PendingChanges {
            owner: owner.into(),
            changes,
        })
    }

    /// Drops `owner`'s changeset; returns how many keys it had.
    pub fn discard(&self, owner: &str) -> Result<u32> {
        let db = self.db();
        let n = db.execute("DELETE FROM pending WHERE owner = ?1", [owner])? as u32;
        db.execute("DELETE FROM pending_groups WHERE owner = ?1", [owner])?;
        drop(db);
        if n > 0 {
            self.pending_changed();
        }
        Ok(n)
    }

    /// Records that `keys` of `owner`'s changeset were staged together by one operation and
    /// must be applied together: a partial apply ([`Config::apply_keys`] with keys) that
    /// selects some of the group's pending keys applies all of them ([`Config::group_keys`]).
    /// With `review` (a destructive change, e.g. a folder deletion), only a full apply
    /// commits the group: any partial apply that selects one of its keys is refused with
    /// [`ConfigError::GroupSplit`], so a dialog's Save never commits it on the side. Keys that are not pending now are left out (staging their
    /// base value dropped the change); the group is forgotten as its keys are applied,
    /// unstaged or discarded.
    pub fn stage_group(&self, owner: &str, keys: &[&str], review: bool) -> Result<()> {
        let mut pending = Vec::new();
        for k in keys {
            if self.pending_value(owner, k)?.is_some() {
                pending.push(*k);
            }
        }
        let keys = pending;
        if keys.is_empty() {
            return Ok(());
        }
        let db = self.db();
        let grp: i64 = db.query_row(
            "SELECT COALESCE(MAX(grp), 0) + 1 FROM pending_groups WHERE owner = ?1",
            [owner],
            |r| r.get(0),
        )?;
        for k in keys {
            db.execute(
                "INSERT OR IGNORE INTO pending_groups (owner, grp, key, review)
                 VALUES (?1, ?2, ?3, ?4)",
                params![owner, grp, k, review as i64],
            )?;
        }
        Ok(())
    }

    /// The keys a partial apply of `keys` really applies: every pending key of a plain group
    /// of [`Config::stage_group`] that `keys` selects a key of is added (keys staged together
    /// by one operation go together), until nothing more joins. A review group (a
    /// destructive change) is never applied by a partial apply: selecting any of its keys is
    /// [`ConfigError::GroupSplit`]. Empty `keys` (the full changeset, always allowed) stays
    /// empty.
    pub fn group_keys(&self, owner: &str, keys: &[String]) -> Result<Vec<String>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let db = self.db();
        let mut st = db.prepare(
            "SELECT g.grp, g.key, g.review FROM pending_groups g
             JOIN pending p ON p.owner = g.owner AND p.key = g.key
             WHERE g.owner = ?1 ORDER BY g.grp, g.key",
        )?;
        let rows = st
            .query_map([owner], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? != 0))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut groups: BTreeMap<i64, (Vec<String>, bool)> = BTreeMap::new();
        for (g, k, review) in rows {
            let e = groups.entry(g).or_default();
            e.0.push(k);
            e.1 |= review;
        }
        let mut out: Vec<String> = keys.to_vec();
        // the selected key each key of `out` joined through, for the refusal's message
        let mut origin: BTreeMap<String, String> =
            keys.iter().map(|k| (k.clone(), k.clone())).collect();
        loop {
            let before = out.len();
            for (members, review) in groups.values() {
                let Some(via) = members
                    .iter()
                    .find(|k| keys.contains(k))
                    .or_else(|| members.iter().find(|k| out.contains(k)))
                else {
                    continue;
                };
                let asked = origin.get(via).cloned().unwrap_or_else(|| via.clone());
                if *review {
                    return Err(ConfigError::GroupSplit {
                        asked,
                        group: members.clone(),
                    });
                }
                for k in members {
                    if !out.contains(k) {
                        out.push(k.clone());
                        origin.insert(k.clone(), asked.clone());
                    }
                }
            }
            if out.len() == before {
                return Ok(out);
            }
        }
    }

    /// Applies `owner`'s changeset: render → candidates → validate → backup + atomic
    /// rename → reload → health check → commit, rolling back the files and the reloads on
    /// any failure. Returns the new revision.
    ///
    /// Blocking (files, commands, D-Bus): call it from a blocking thread. `cancelled` is
    /// checked until the files are replaced; after that the apply runs to the end.
    pub fn apply(
        &self,
        owner: &str,
        author: &str,
        comment: &str,
        log: &dyn Fn(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64> {
        self.apply_keys(owner, author, comment, &[], log, cancelled)
    }

    /// [`Config::apply`] limited to `keys` of `owner`'s changeset (empty: all of it): the
    /// other staged keys stay pending, untouched. Keys that are not staged are ignored;
    /// [`ConfigError::NothingPending`] when none of them is. The keys staged together with a
    /// selected one join it, a destructive change is refused ([`Config::group_keys`]).
    /// Conflicts count only among the selected keys. Same pipeline, rollback and revision as
    /// a full apply.
    pub fn apply_keys(
        &self,
        owner: &str,
        author: &str,
        comment: &str,
        keys: &[String],
        log: &dyn Fn(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64> {
        let _apply = self.apply_lock.lock().unwrap();
        let keys = self.group_keys(owner, keys)?;
        self.apply_selected(owner, author, comment, &keys, log, cancelled)
    }

    /// [`Config::apply_keys`] of keys a caller already expanded with [`Config::group_keys`]
    /// and checked (e.g. the guards of the HTTP apply): exactly them are applied. When the
    /// groups expand them further now (keys staged together since) or a review group joined,
    /// nothing is applied, so the checked set is the applied set.
    pub fn apply_expanded(
        &self,
        owner: &str,
        author: &str,
        comment: &str,
        keys: &[String],
        log: &dyn Fn(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64> {
        let _apply = self.apply_lock.lock().unwrap();
        let extra: Vec<String> = self
            .group_keys(owner, keys)?
            .into_iter()
            .filter(|k| !keys.contains(k))
            .collect();
        if !extra.is_empty() {
            return Err(ConfigError::Apply(format!(
                "{} joined the selected changes after the apply was requested: nothing was applied, apply again",
                extra.join(", ")
            )));
        }
        self.apply_selected(owner, author, comment, keys, log, cancelled)
    }

    /// The apply of `keys` (expanded, empty: all), with the apply lock held.
    fn apply_selected(
        &self,
        owner: &str,
        author: &str,
        comment: &str,
        keys: &[String],
        log: &dyn Fn(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64> {
        let mut pending = self.pending(owner)?;
        let selected = |k: &str| keys.is_empty() || keys.iter().any(|x| x == k);
        pending.changes.retain(|c| selected(&c.key));
        if pending.changes.is_empty() {
            return Err(ConfigError::NothingPending);
        }
        let conflicts: Vec<String> = pending
            .changes
            .iter()
            .filter(|c| c.conflict)
            .map(|c| c.key.clone())
            .collect();
        if !conflicts.is_empty() {
            return Err(ConfigError::Conflict(conflicts));
        }
        let restored_from: Option<u64> = {
            let db = self.db();
            let mut st =
                db.prepare("SELECT key, restored_from FROM pending WHERE owner = ?1")?;
            let rows = st
                .query_map([owner], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .filter(|(k, _)| selected(k))
                .filter_map(|(_, r)| r)
                .max()
                .map(|v| v as u64)
        };
        let mut steps = Vec::new();
        for c in pending.changes {
            let applier = self.applier(&c.key)?.clone();
            steps.push(Step {
                key: c.key,
                applier,
                old: c.base,
                new: c.value,
            });
        }
        self.apply_steps(steps, owner, author, comment, restored_from, log, cancelled)
    }

    /// Runs `f` while no apply runs (the apply lock): for code outside the engine that
    /// rewrites a file an apply may also write (read-modify-write of a shared file, e.g. the
    /// battery tests' block of `upsd.users`). `f` must not apply. Blocking.
    pub fn exclusive<T>(&self, f: impl FnOnce() -> T) -> T {
        let _apply = self.apply_lock.lock().unwrap();
        f()
    }

    /// Applies one value of `key` at once, without a changeset: for state that an operation
    /// of rynas itself has just created on the system (a new storage volume, its fstab and
    /// crypttab entries). Same pipeline, rollback and revision as [`Config::apply`]; staged
    /// changes of users on `key` become conflicts (their base changed). Blocking.
    pub fn apply_value(
        &self,
        author: &str,
        key: &str,
        value: Option<Value>,
        comment: &str,
        log: &dyn Fn(&str),
    ) -> Result<u64> {
        let _apply = self.apply_lock.lock().unwrap();
        self.apply_value_locked(author, key, value, comment, log)
    }

    /// [`Config::apply_value`] of a value computed from the applied one inside the apply
    /// lock: `update` gets the applied value of `key` (`None` if never applied) and returns
    /// the new value, or `None` to leave it as it is (then nothing is applied and the result
    /// is `None`). No apply can run between the read and the apply, so changes of others
    /// are never reverted by a stale copy (read-modify-write of a document). Blocking.
    pub fn apply_update(
        &self,
        author: &str,
        key: &str,
        comment: &str,
        log: &dyn Fn(&str),
        update: impl FnOnce(Option<Value>) -> std::result::Result<Option<Value>, String>,
    ) -> Result<Option<u64>> {
        let _apply = self.apply_lock.lock().unwrap();
        let applied = self.applied(key)?;
        let Some(v) = update(applied).map_err(|e| ConfigError::Invalid(e.into()))? else {
            return Ok(None);
        };
        self.apply_value_locked(author, key, Some(v), comment, log)
            .map(Some)
    }

    fn apply_value_locked(
        &self,
        author: &str,
        key: &str,
        value: Option<Value>,
        comment: &str,
        log: &dyn Fn(&str),
    ) -> Result<u64> {
        let applier = self.applier(key)?.clone();
        let new = match value {
            Some(v) => Some(applier.validate(v).map_err(ConfigError::Invalid)?),
            None => None,
        };
        let old = self.current(key)?;
        let steps = vec![Step {
            key: key.to_string(),
            applier,
            old,
            new,
        }];
        // the owner of no changeset: `@` is not allowed in user names
        self.apply_steps(steps, "@rynas", author, comment, None, log, &|| false)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_steps(
        &self,
        mut steps: Vec<Step>,
        owner: &str,
        author: &str,
        comment: &str,
        restored_from: Option<u64>,
        log: &dyn Fn(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<u64> {
        // domains run in their declared order (users before the shares that name them)
        steps.sort_by(|a, b| (a.applier.order(), &a.key).cmp(&(b.applier.order(), &b.key)));
        let rev = self.next_rev()?;
        let backup_dir = self.opts.state_dir.join("backup").join(rev.to_string());
        log(&format!(
            "applying revision {rev} ({}) by {author}",
            steps
                .iter()
                .map(|s| s.key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let (written, restore) = match self.run_pipeline(&steps, &backup_dir, log, cancelled) {
            Ok(r) => r,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&backup_dir);
                return Err(e);
            }
        };
        if let Err(e) = self.commit(rev, owner, author, comment, restored_from, &steps, &written) {
            log(&format!("saving the revision failed: {e}; rolling back"));
            self.rollback(&steps, &restore, steps.len(), log);
            let _ = std::fs::remove_dir_all(&backup_dir);
            return Err(e);
        }
        log(&format!("revision {rev} applied"));
        if let Err(e) = self.prune() {
            tracing::warn!("revision retention: {e}");
        }
        Ok(rev)
    }

    fn next_rev(&self) -> Result<u64> {
        let v: Option<String> = self
            .db()
            .query_row("SELECT v FROM meta WHERE k = 'next_rev'", [], |r| r.get(0))
            .optional()?;
        Ok(v.and_then(|v| v.parse().ok()).unwrap_or(1))
    }

    fn run_pipeline(
        &self,
        steps: &[Step],
        backup_dir: &Path,
        log: &dyn Fn(&str),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(Vec<Written>, Vec<Restore>)> {
        let ctx = self.ctx_logging(log);
        let fail = |key: &str, what: &str, e: String| {
            let m = format!("{key}: {what}: {e}");
            log(&m);
            ConfigError::Apply(m)
        };

        // 1. render
        struct Planned {
            step: usize,
            file: ManagedFile,
            real: PathBuf,
            text: String,
            hash: String,
            unchanged: bool,
        }
        let mut planned: Vec<Planned> = Vec::new();
        for (i, s) in steps.iter().enumerate() {
            let files = match &s.new {
                Some(v) => s.applier.render(v, &ctx),
                None => s.applier.render_removed(s.old.as_ref(), &ctx).inspect(|f| {
                    log(&if f.is_empty() {
                        format!("{}: removed, its files are no longer managed", s.key)
                    } else {
                        format!("{}: removed, putting back what it leaves behind", s.key)
                    })
                }),
            }
            .map_err(|e| fail(&s.key, "render", e))?;
            for f in files {
                if !f.path.is_absolute() {
                    return Err(fail(
                        &s.key,
                        "render",
                        format!("{} is not absolute", f.path.display()),
                    ));
                }
                if planned.iter().any(|p| p.file.path == f.path) {
                    return Err(fail(
                        &s.key,
                        "render",
                        format!("{} rendered twice", f.path.display()),
                    ));
                }
                let real = ctx.path(&f.path);
                let (text, hash, unchanged) = if f.kind == FileKind::Absent {
                    (String::new(), String::new(), !real.exists())
                } else {
                    let existing = std::fs::read_to_string(&real).ok();
                    let (text, hash) = render_final(&f.kind, &f.content, existing.as_deref());
                    let unchanged = existing.as_deref() == Some(text.as_str());
                    (text, hash, unchanged)
                };
                planned.push(Planned {
                    step: i,
                    file: f,
                    real,
                    text,
                    hash,
                    unchanged,
                });
            }
        }
        if cancelled() {
            return Err(ConfigError::Cancelled);
        }

        // 2. candidates next to their targets
        let cleanup = |planned: &[Planned]| {
            for p in planned {
                let _ = std::fs::remove_file(candidate_path(&p.real));
            }
        };
        let absent = |p: &Planned| p.file.kind == FileKind::Absent;
        for p in planned.iter().filter(|p| !p.unchanged && !absent(p)) {
            log(&format!("writing {}", p.file.path.display()));
            if let Err(e) = write_synced(&candidate_path(&p.real), p.text.as_bytes(), p.file.mode) {
                cleanup(&planned);
                return Err(fail(&steps[p.step].key, "write", e.to_string()));
            }
            if let Some(g) = &p.file.group
                && let Err(e) = set_group(ctx.root, &candidate_path(&p.real), g)
            {
                cleanup(&planned);
                return Err(fail(&steps[p.step].key, "write", e));
            }
        }

        // 3. validate
        for (i, s) in steps.iter().enumerate() {
            let cands: Vec<Candidate> = planned
                .iter()
                .filter(|p| p.step == i && !absent(p))
                .map(|p| Candidate {
                    file: &p.file,
                    staged: if p.unchanged {
                        p.real.clone()
                    } else {
                        candidate_path(&p.real)
                    },
                })
                .collect();
            if let Err(e) = s.applier.check_files(&cands, &ctx) {
                cleanup(&planned);
                return Err(fail(&s.key, "validation failed", e));
            }
        }
        if cancelled() {
            cleanup(&planned);
            return Err(ConfigError::Cancelled);
        }

        // 4. backup and atomic replace: the point of no return
        let mut restore: Vec<Restore> = Vec::new();
        for (n, p) in planned.iter().enumerate().filter(|(_, p)| !p.unchanged) {
            let r = (|| -> std::io::Result<Restore> {
                let owner = std::fs::metadata(&p.real).ok().map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    (m.mode() & 0o7777, m.uid(), m.gid())
                });
                let backup = if p.real.exists() {
                    let b = under_root(&backup_dir.join("files"), &p.file.path);
                    if let Some(d) = b.parent() {
                        std::fs::create_dir_all(d)?;
                    }
                    std::fs::copy(&p.real, &b)?;
                    Some(b)
                } else {
                    None
                };
                if absent(p) {
                    log(&format!("removing {}", p.file.path.display()));
                    std::fs::remove_file(&p.real)?;
                } else {
                    rename_synced(&candidate_path(&p.real), &p.real)?;
                }
                ctx.replaced.lock().unwrap().push(p.real.clone());
                Ok(Restore {
                    target: p.real.clone(),
                    backup,
                    owner,
                })
            })();
            match r {
                Ok(r) => restore.push(r),
                Err(e) => {
                    let err = fail(&steps[p.step].key, "replace", e.to_string());
                    cleanup(&planned[n..]);
                    self.rollback(steps, &restore, 0, log);
                    return Err(err);
                }
            }
        }

        // 5. reload
        for (n, s) in steps.iter().enumerate() {
            log(&format!("{}: reloading", s.key));
            if let Err(e) = s.applier.reconcile(s.old.as_ref(), s.new.as_ref(), &ctx) {
                let err = fail(&s.key, "reload", e);
                self.rollback(steps, &restore, n + 1, log);
                return Err(err);
            }
        }

        // 6. health check
        for s in steps {
            if let Err(e) = s.applier.health(s.new.as_ref(), &ctx) {
                let err = fail(&s.key, "health check", e);
                self.rollback(steps, &restore, steps.len(), log);
                return Err(err);
            }
        }
        log("health checks passed");

        let written = planned
            .into_iter()
            // a file that must not exist, or left behind by a removed key, is not tracked by
            // the drift check
            .filter(|p| p.file.kind != FileKind::Absent && steps[p.step].new.is_some())
            .map(|p| Written {
                path: p.file.path,
                key: steps[p.step].key.clone(),
                kind: p.file.kind,
                hash: p.hash,
            })
            .collect();
        Ok((written, restore))
    }

    /// Puts the previous files back and reloads the first `reloaded` steps with their old
    /// values. Best effort: failures are logged.
    fn rollback(&self, steps: &[Step], restore: &[Restore], reloaded: usize, log: &dyn Fn(&str)) {
        log("rolling back");
        let ctx = self.ctx_logging(log);
        for r in restore.iter().rev() {
            ctx.replaced.lock().unwrap().push(r.target.clone());
            let res = match &r.backup {
                Some(b) => {
                    let cand = candidate_path(&r.target);
                    std::fs::copy(b, &cand)
                        .and_then(|_| match r.owner {
                            Some(o) => restore_owner(&cand, o),
                            None => Ok(()),
                        })
                        .and_then(|_| rename_synced(&cand, &r.target))
                        .inspect_err(|_| {
                            let _ = std::fs::remove_file(&cand);
                        })
                }
                None => match std::fs::remove_file(&r.target) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    r => r,
                },
            };
            match res {
                Ok(()) => log(&format!("restored {}", r.target.display())),
                Err(e) => log(&format!("cannot restore {}: {e}", r.target.display())),
            }
        }
        for s in steps[..reloaded].iter().rev() {
            if let Err(e) = s.applier.reconcile(s.new.as_ref(), s.old.as_ref(), &ctx) {
                log(&format!(
                    "{}: reload of the previous configuration failed: {e}",
                    s.key
                ));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn commit(
        &self,
        rev: u64,
        owner: &str,
        author: &str,
        comment: &str,
        restored_from: Option<u64>,
        steps: &[Step],
        written: &[Written],
    ) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        for s in steps {
            match &s.new {
                Some(v) => tx.execute(
                    "INSERT INTO config (key, value, rev) VALUES (?1, ?2, ?3)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value, rev = excluded.rev",
                    params![s.key, v.to_string(), rev as i64],
                )?,
                None => tx.execute("DELETE FROM config WHERE key = ?1", [&s.key])?,
            };
            tx.execute("DELETE FROM managed_files WHERE key = ?1", [&s.key])?;
            tx.execute(
                "DELETE FROM pending WHERE owner = ?1 AND key = ?2",
                params![owner, s.key],
            )?;
            tx.execute(
                "DELETE FROM pending_groups WHERE owner = ?1 AND key = ?2",
                params![owner, s.key],
            )?;
        }
        for w in written {
            tx.execute(
                "INSERT OR REPLACE INTO managed_files (path, key, kind, sha256, rev)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    w.path.to_string_lossy(),
                    w.key,
                    serde_json::to_string(&w.kind)?,
                    w.hash,
                    rev as i64
                ],
            )?;
        }
        let snapshot: BTreeMap<String, Value> = {
            let mut st = tx.prepare("SELECT key, value FROM config")?;
            st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .map(|r| {
                    let (k, v) = r?;
                    Ok((k, serde_json::from_str(&v).unwrap_or(Value::Null)))
                })
                .collect::<rusqlite::Result<_>>()?
        };
        let changes: Vec<StoredChange> = steps
            .iter()
            .map(|s| StoredChange {
                key: s.key.clone(),
                old: s.old.clone(),
                new: s.new.clone(),
            })
            .collect();
        let files: Vec<String> = written
            .iter()
            .map(|w| w.path.to_string_lossy().into_owned())
            .collect();
        tx.execute(
            "INSERT INTO revisions (id, author, ts_ms, comment, restored_from, changes, snapshot, files)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                rev as i64,
                author,
                now_ms() as i64,
                comment,
                restored_from.map(|r| r as i64),
                serde_json::to_string(&changes)?,
                serde_json::to_string(&snapshot)?,
                serde_json::to_string(&files)?
            ],
        )?;
        tx.execute(
            "INSERT INTO meta (k, v) VALUES ('next_rev', ?1)
             ON CONFLICT (k) DO UPDATE SET v = excluded.v",
            [(rev + 1).to_string()],
        )?;
        tx.commit()?;
        drop(db);
        self.pending_changed();
        Ok(())
    }

    /// Drops revisions beyond the retention (count and age), with their file backups.
    fn prune(&self) -> Result<()> {
        let cutoff = now_ms().saturating_sub(self.opts.keep_age.as_millis() as u64) as i64;
        let doomed: Vec<i64> = {
            let db = self.db();
            let mut st = db.prepare("SELECT id, ts_ms FROM revisions ORDER BY id DESC")?;
            let rows = st
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .enumerate()
                .filter(|(i, (_, ts))| *i > 0 && (*i >= self.opts.keep_revisions || *ts <= cutoff))
                .map(|(_, (id, _))| id)
                .collect()
        };
        for id in doomed {
            self.db()
                .execute("DELETE FROM revisions WHERE id = ?1", [id])?;
            let _ =
                std::fs::remove_dir_all(self.opts.state_dir.join("backup").join(id.to_string()));
        }
        Ok(())
    }

    fn summary(row: &rusqlite::Row) -> rusqlite::Result<(RevisionSummary, String, String)> {
        let changes: String = row.get(5)?;
        let keys = serde_json::from_str::<Vec<StoredChange>>(&changes)
            .map(|c| c.into_iter().map(|c| c.key).collect())
            .unwrap_or_default();
        Ok((
            RevisionSummary {
                id: row.get::<_, i64>(0)? as u64,
                author: row.get(1)?,
                ts_ms: row.get::<_, i64>(2)? as u64,
                comment: row.get(3)?,
                restored_from: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                keys,
            },
            changes,
            row.get(6)?,
        ))
    }

    /// Revisions, newest first.
    pub fn revisions(&self, limit: usize) -> Result<Vec<RevisionSummary>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT id, author, ts_ms, comment, restored_from, changes, files
             FROM revisions ORDER BY id DESC LIMIT ?1",
        )?;
        let r = st
            .query_map([limit as i64], |r| Self::summary(r).map(|s| s.0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(r)
    }

    /// A revision with its semantic diff.
    pub fn revision(&self, id: u64) -> Result<RevisionDetail> {
        let (revision, changes, files) = self
            .db()
            .query_row(
                "SELECT id, author, ts_ms, comment, restored_from, changes, files
                 FROM revisions WHERE id = ?1",
                [id as i64],
                Self::summary,
            )
            .optional()?
            .ok_or_else(|| ConfigError::NotFound(format!("revision {id}")))?;
        let changes: Vec<StoredChange> = serde_json::from_str(&changes)?;
        let diff: Vec<DiffEntry> = changes
            .iter()
            .flat_map(|c| diff(&c.key, c.old.as_ref(), c.new.as_ref()))
            .collect();
        Ok(RevisionDetail {
            revision,
            diff,
            files: serde_json::from_str(&files)?,
        })
    }

    /// The whole configuration right after revision `id` (key → applied value).
    pub fn revision_snapshot(&self, id: u64) -> Result<BTreeMap<String, Value>> {
        let snapshot: Option<String> = self
            .db()
            .query_row(
                "SELECT snapshot FROM revisions WHERE id = ?1",
                [id as i64],
                |r| r.get(0),
            )
            .optional()?;
        Ok(serde_json::from_str(&snapshot.ok_or_else(|| {
            ConfigError::NotFound(format!("revision {id}"))
        })?)?)
    }

    /// "Restore revision N": stages, in `owner`'s changeset, every key whose applied value
    /// differs from the configuration right after revision N. History is never rewritten:
    /// applying the changeset creates a new revision. Returns the number of keys staged.
    pub fn restore(&self, owner: &str, id: u64) -> Result<u32> {
        let snapshot = self.revision_snapshot(id)?;
        let mut staged = 0;
        for key in self.appliers.keys() {
            let target = snapshot.get(key).cloned();
            let applied = self.applied(key)?;
            if target == applied {
                continue;
            }
            if self.stage_inner(owner, key, target, Some(id))? {
                staged += 1;
            }
        }
        Ok(staged)
    }

    /// Stages a whole set of values (e.g. from an imported archive). Keys without a
    /// registered domain are skipped.
    pub fn stage_values(&self, owner: &str, values: &BTreeMap<String, Value>) -> Result<u32> {
        let mut staged = 0;
        for (key, v) in values {
            if !self.appliers.contains_key(key) {
                tracing::warn!("import: no domain for {key}, skipped");
                continue;
            }
            if self.applied(key)?.as_ref() == Some(v) {
                continue;
            }
            if self.stage(owner, key, Some(v.clone()))? {
                staged += 1;
            }
        }
        Ok(staged)
    }

    /// Managed files that no longer match what rynas wrote.
    pub fn drift(&self) -> Result<DriftReport> {
        let rows = {
            let db = self.db();
            let mut st =
                db.prepare("SELECT path, key, kind, sha256 FROM managed_files ORDER BY path")?;
            st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut report = DriftReport {
            checked: rows.len() as u32,
            entries: Vec::new(),
            state: Vec::new(),
        };
        let ctx = self.ctx();
        for (key, a) in &self.appliers {
            let applied = self.applied(key)?;
            match a.state_drift(applied.as_ref(), &ctx) {
                Ok(mut s) => report.state.append(&mut s),
                Err(e) => tracing::warn!("{key}: state drift check: {e}"),
            }
        }
        for (path, key, kind, expected) in rows {
            let kind: FileKind = serde_json::from_str(&kind)?;
            let real = under_root(&self.opts.root, Path::new(&path));
            let (status, actual) = match std::fs::read(&real) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (DriftStatus::Missing, None),
                Err(e) => return Err(e.into()),
                Ok(data) => {
                    let actual = current_hash(&kind, &String::from_utf8_lossy(&data));
                    if actual.as_deref() == Some(expected.as_str()) {
                        continue;
                    }
                    (DriftStatus::Modified, actual)
                }
            };
            report.entries.push(DriftEntry {
                path,
                key,
                status,
                expected_sha256: expected,
                actual_sha256: actual,
            });
        }
        Ok(report)
    }

    /// Resolves drift by taking the system's current state: stages the domain's import as
    /// a pending change of `owner`.
    pub fn drift_import(&self, owner: &str, key: &str) -> Result<u32> {
        let a = self.applier(key)?;
        let v = a
            .import(&self.ctx())
            .map_err(|e| ConfigError::Invalid(e.into()))?
            .ok_or_else(|| {
                ConfigError::Invalid(format!("{key} cannot import from the system").into())
            })?;
        Ok(self.stage(owner, key, Some(v))? as u32)
    }

    /// Resolves drift by writing the applied configuration of `key` again (no new revision;
    /// the replaced files are kept under `backup/overwrite-<time>/`).
    pub fn drift_overwrite(&self, key: &str, log: &dyn Fn(&str)) -> Result<()> {
        let _apply = self.apply_lock.lock().unwrap();
        let applier = self.applier(key)?.clone();
        let value = self
            .applied(key)?
            .ok_or_else(|| ConfigError::NotFound(format!("applied configuration of {key}")))?;
        let steps = [Step {
            key: key.into(),
            applier,
            old: Some(value.clone()),
            new: Some(value),
        }];
        let backup_dir = self
            .opts
            .state_dir
            .join("backup")
            .join(format!("overwrite-{}", now_ms()));
        log(&format!("{key}: writing the applied configuration again"));
        let (written, _) = self.run_pipeline(&steps, &backup_dir, log, &|| false)?;
        let rev = self.applied_rev(key)?;
        let mut db = self.db();
        let tx = db.transaction()?;
        let paths: HashSet<String> = written
            .iter()
            .map(|w| w.path.to_string_lossy().into_owned())
            .collect();
        tx.execute("DELETE FROM managed_files WHERE key = ?1", [key])?;
        for w in &written {
            tx.execute(
                "INSERT OR REPLACE INTO managed_files (path, key, kind, sha256, rev)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    w.path.to_string_lossy(),
                    w.key,
                    serde_json::to_string(&w.kind)?,
                    w.hash,
                    rev
                ],
            )?;
        }
        tx.commit()?;
        log(&format!("{key}: {} files written", paths.len()));
        Ok(())
    }

    fn applied_rev(&self, key: &str) -> Result<Option<i64>> {
        Ok(self
            .db()
            .query_row("SELECT rev FROM config WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }
}

/// The gid of `group` in `<root>/etc/group`.
pub fn group_id(root: &Path, group: &str) -> Option<u32> {
    let text = std::fs::read_to_string(under_root(root, Path::new("/etc/group"))).ok()?;
    text.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next() == Some(group))
            .then(|| f.nth(1).and_then(|g| g.parse().ok()))
            .flatten()
    })
}

/// Gives `path` to `root:<group>` ([`crate::ManagedFile::group`]). A group missing below the
/// configuration root is skipped (test roots); so is a refused chown when not running as root.
fn set_group(root: &Path, path: &Path, group: &str) -> std::result::Result<(), String> {
    let Some(gid) = group_id(root, group) else {
        return Ok(());
    };
    match std::os::unix::fs::chown(path, Some(0), Some(gid)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && !is_root() => Ok(()),
        Err(e) => Err(format!("{}: chown to root:{group}: {e}", path.display())),
    }
}

/// Puts back the mode and owner of a restored file. A refused chown is skipped when not
/// running as root (tests).
fn restore_owner(path: &Path, (mode, uid, gid): (u32, u32, u32)) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::os::unix::fs::chown(path, Some(uid), Some(gid)) {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && !is_root() => {}
        r => r?,
    }
    // after the chown, which may clear set-id bits
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}
