//! The `qnap.hw` configuration key: the settings of [`crate::control::Controller`]. No file is
//! written (the hardware is driven by rynasd); applying a value hands it to the controller.

use std::sync::{Arc, OnceLock};

use rynas_config::{Applier, ApplyCtx, Config, ManagedFile};
use serde_json::Value;

use crate::api::QnapSettings;
use crate::control::Controller;

pub const QNAP_KEY: &str = "qnap.hw";

/// The controller, set once at the start of rynasd (the domain is registered before it).
pub type Cell = Arc<OnceLock<Arc<Controller>>>;

pub struct QnapDomain(pub Cell);

pub fn settings_of(v: &Value) -> Result<QnapSettings, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("invalid qnap.hw document: {e}"))
}

/// The settings in force (`current`, else the defaults).
pub fn current(cfg: &Config) -> QnapSettings {
    cfg.current(QNAP_KEY)
        .ok()
        .flatten()
        .and_then(|v| settings_of(&v).ok())
        .unwrap_or_default()
}

impl Applier for QnapDomain {
    fn key(&self) -> &str {
        QNAP_KEY
    }

    fn order(&self) -> i32 {
        50
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let s = settings_of(&value)?;
        serde_json::to_value(s).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    fn reload(&self, value: Option<&Value>, _ctx: &ApplyCtx) -> Result<(), String> {
        if let Some(ctl) = self.0.get() {
            let s = value.map(settings_of).transpose()?.unwrap_or_default();
            ctl.apply(s);
        }
        Ok(())
    }
}
