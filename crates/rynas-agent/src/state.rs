//! The agent's only local state: the pairing (NAS address and pinned certificate, device,
//! group, token, repository password), the last plan and the schedule's counters, in
//! `/var/lib/rynas-agent/state.json` (0600, root). There is no configuration file: plans come
//! from the NAS, so a package manager can never overwrite one.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rynas_api_types::pcagent::AgentPlan;
use serde::{Deserialize, Serialize};

pub const STATE_DIR: &str = "/var/lib/rynas-agent";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    /// The NAS address that answered last (tried first).
    pub url: String,
    /// The pinned certificate (SHA-256) of agents paired before the NAS' local CA was
    /// pinned; empty once [`State::nas_id`] and [`State::cas`] are set.
    pub fingerprint: String,
    /// The NAS identity (`rynas_api_types::pcagent::AgentTrust`): with [`State::cas`] what
    /// is trusted instead of [`State::fingerprint`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nas_id: String,
    /// The NAS' local CA certificates (PEM): the current one, and the next one ahead of a
    /// rotation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cas: Vec<String>,
    /// The NAS' other addresses, refreshed on each contact, tried when [`State::url`] does
    /// not answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    /// When the trust was last refreshed from the NAS (or found unsupported by it).
    #[serde(default)]
    pub trust_checked_ms: Option<u64>,
    pub device: String,
    pub group: String,
    pub token: String,
    pub repo_password: String,
    #[serde(default)]
    pub nas_name: String,
    #[serde(default)]
    pub plan: AgentPlan,
    #[serde(default)]
    pub last_ok_ms: Option<u64>,
    #[serde(default)]
    pub last_attempt_ms: Option<u64>,
    /// Failed runs in a row (the retry delay grows with them).
    #[serde(default)]
    pub failures: u32,
    /// The last command of the NAS that was carried out.
    #[serde(default)]
    pub handled_command: u64,
    /// The skipped start last reported to the NAS while a due backup waits (paused, outside
    /// the allowed window), and why: one report per wait. Cleared when a backup starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_noted: Option<(u64, rynas_api_types::scheduler::SkipReason)>,
}

pub fn dir() -> PathBuf {
    std::env::var_os("RYNAS_AGENT_STATE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(STATE_DIR))
}

pub fn path() -> PathBuf {
    dir().join("state.json")
}

impl State {
    pub fn load() -> Result<State, String> {
        let p = path();
        let b = std::fs::read(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "this PC is not paired: run `rynas-agent pair <nas address> <code>`".to_string()
            } else {
                format!("{}: {e}", p.display())
            }
        })?;
        serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", p.display()))
    }

    /// Written to a temporary file and renamed: never half a state.
    pub fn save(&self) -> Result<(), String> {
        let d = dir();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&d)
            .map_err(|e| format!("{}: {e}", d.display()))?;
        write_atomic(&path(), &serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?, 0o600)
    }

    /// Loads the state written last, changes it with `f` and saves it: a process that holds
    /// an older copy (the backup child, the service) writes only what it changed, never back
    /// what another wrote meanwhile (an address or trust refreshed by the service).
    pub fn update(f: impl FnOnce(&mut State)) -> Result<State, String> {
        let mut st = State::load()?;
        f(&mut st);
        st.save()?;
        Ok(st)
    }
}

/// `data` to `p` through `p.tmp` + fsync + rename.
pub fn write_atomic(p: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    let tmp = p.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    f.write_all(data).and_then(|_| f.sync_all()).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, p).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
