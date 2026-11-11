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

/// Called with the key of each job an apply removed.
pub type OnRemoved = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// The `dedup` key: settings only, no files (the runs are scheduler tasks). A removed job
/// is handed to `removed` (rynasd stops its run and deletes its hashfile and state).
#[derive(Default)]
pub struct DedupDomain {
    pub removed: Option<OnRemoved>,
}

impl DedupDomain {
    pub fn with_removed(removed: OnRemoved) -> Self {
        Self { removed: Some(removed) }
    }
}

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

    fn reconcile(&self, from: Option<&Value>, to: Option<&Value>, _ctx: &ApplyCtx) -> Result<(), String> {
        let Some(cb) = &self.removed else { return Ok(()) };
        let keys = |v: Option<&Value>| -> Vec<String> {
            v.and_then(|v| serde_json::from_value::<DedupConfig>(v.clone()).ok())
                .map(|c| c.jobs.into_keys().collect())
                .unwrap_or_default()
        };
        let after = keys(to);
        for k in keys(from).into_iter().filter(|k| !after.contains(k)) {
            cb(&k);
        }
        Ok(())
    }
}
