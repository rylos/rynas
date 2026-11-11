//! Offline deduplication of shared folders (D-DEDUP, `docs/phase2/dedup.md`).
//!
//! What can be decided without the machine: the `dedup` configuration domain
//! ([`DedupDomain`]), the weekly window ([`window`]), the exclude list ([`excludes`]), the
//! engine's command lines and output parsers ([`engine`]) and the job state rynasd keeps
//! next to the hashfile ([`state`]). Running, locking and the API live in rynasd.

pub mod config;
pub mod engine;
pub mod excludes;
pub mod state;
pub mod window;

use rynas_api_types::dedup::{DEDUP_KEY, DedupConfig};
use rynas_config::{ApplyCtx, ManagedFile};
use serde_json::Value;

pub use config::normalize;
pub use engine::{DedupEngine, DedupJob, EngineLine, ExitKind, Oans};

/// The `dedup` key: settings only, no files (the runs are scheduler tasks).
pub struct DedupDomain;

impl rynas_config::Applier for DedupDomain {
    fn key(&self) -> &str {
        DEDUP_KEY
    }

    /// Before `scheduler` (40), whose units start the runs.
    fn order(&self) -> i32 {
        36
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let c: DedupConfig = serde_json::from_value(value)
            .map_err(|e| format!("invalid dedup document: {e}"))?;
        let c = normalize(c)?;
        serde_json::to_value(c).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }
}
