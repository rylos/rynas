//! rynas-files: the core of the rynas file manager, ported from SynoFile (MIT, same author).
//!
//! Two sides:
//! - the library rynasd links: the transfer jobs ([`jobs::manager`]), the per-user stores
//!   (preferences, remote connections with sealed secrets: [`store`], [`conns`]) and the
//!   wire types it talks to the worker with;
//! - `rynas-files-worker`, which rynasd starts already switched to the logged-in user
//!   ([`identity::become_raw`]) for every request that touches files ([`api::serve`]) and
//!   for every transfer job (`rynas-files-worker job`, a separate crate with its own lock
//!   file: the SMB and SFTP clients pin crypto crates the rest of rynas cannot share). The kernel decides what that user may read
//!   and change: the user's permissions and POSIX ACLs apply, as for SMB.
//!
//! Messages are English; the interface translates the error `code`s (rynas's
//! `error_codes`) and the job message codes (see `docs/plugins/files.md`).

/// A message (English). SynoFile's `t!(italian, english)` became `t!(english)`: the
/// interface translates codes, not sentences.
macro_rules! t {
    ($($arg:tt)*) => { format!($($arg)*) };
}

pub mod api;
pub mod archive;
pub mod caps;
pub mod conn_routes;
pub mod conns;
pub mod edit;
pub mod fsops;
pub mod identity;
pub mod jobs;
pub mod media;
pub mod names;
pub mod shares;
pub mod space;
pub mod store;
pub mod thumbs;
pub mod transfer;
pub mod wire;
pub mod zip;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Version of the file manager API (the crate's).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What rynasd tells a worker about the request's user and the system, with every request
/// ([`wire::WorkerRequest::ctx`]).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Context {
    pub user: identity::User,
    /// An administrator: reads every folder (writes stay the user's).
    #[serde(default)]
    pub admin: bool,
    /// The shares this user may see, with their real paths.
    #[serde(default)]
    pub shares: Vec<shares::Share>,
    /// Display names of the volumes (`Share::volume` → name).
    #[serde(default)]
    pub volumes: BTreeMap<String, String>,
    #[serde(default)]
    pub hostname: String,
    /// The remote connections the request names, secrets included (only when it needs
    /// them: [`conn_routes::wants_conns`]).
    #[serde(default)]
    pub conns: Vec<conns::Conn>,
    /// rynasd's own part of `init` (whoami, prefs, conns): the worker merges it.
    #[serde(default)]
    pub extra: serde_json::Value,
    /// The user's thumbnail cache (made by rynasd, owned by the user, 0700).
    #[serde(default)]
    pub thumbs: Option<PathBuf>,
}
