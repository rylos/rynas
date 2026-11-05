//! rynas configuration store (`config.db`), used by `rynasd`, its only writer.
//!
//! Model (OPNsense-style history, OMV-style pending/apply done right):
//! - **domains**: one configuration key per domain, holding a JSON document
//!   (`system.hostname`, later `smb`, `nfs`, `users`...), each with an [`Applier`];
//! - **changesets**: pending edits per owner (the user name), each remembering the value it
//!   was based on, so that a stale edit is refused instead of overwriting someone else's;
//! - **revisions**: applied changesets with author, time, comment, the semantic diff per key
//!   and a snapshot of the whole configuration; "restore revision N" stages a new changeset,
//!   history is never rewritten; retention 200 revisions / 365 days;
//! - **apply pipeline**: render (minijinja) → `*.rynas-new` candidates → validate → backup
//!   under `backup/<rev>/` + atomic rename → reload (systemd over D-Bus, behind [`System`])
//!   → health check → commit, rolling back files and reloads on failure;
//! - **anti-drift**: managed files carry a header or block markers and their hash is stored;
//!   [`Config::drift`] reports files changed by hand, resolved by import or overwrite;
//! - **export/import**: `.rynasbak` archives, see [`archive`].

pub mod archive;
pub mod db;
pub mod diff;
pub mod domain;
mod engine;
pub mod files;
pub mod system;

pub use domain::{Applier, ApplyCtx, Candidate, FileDomain, HOSTNAME_KEY, HostnameDomain, Invalid};
pub use engine::{Config, ConfigError, ConfigOptions, Result};
pub use files::{FileKind, ManagedFile};
pub use system::{HostSystem, MemorySystem, System};

#[cfg(test)]
mod tests;
