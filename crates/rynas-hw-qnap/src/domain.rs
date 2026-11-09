//! The `qnap.hw` configuration key: the settings of [`crate::control::Controller`]. No file is
//! written (the hardware is driven by rynasd); applying a value hands it to the controller.

use std::sync::{Arc, Mutex, OnceLock};

use rynas_config::{Applier, ApplyCtx, Config, ManagedFile};
use serde_json::Value;

use crate::api::QnapSettings;
use crate::control::Controller;

pub const QNAP_KEY: &str = "qnap.hw";

/// The controller, set once it is confirmed (the domain and the routes are registered before
/// it), and the latest settings until then.
#[derive(Default)]
pub struct Shared {
    pub ctl: OnceLock<Arc<Controller>>,
    pub latest: Mutex<QnapSettings>,
}

pub type Cell = Arc<Shared>;

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
        if ![0, 50, 100].contains(&s.led_brightness) {
            return Err("led_brightness must be 0, 50 or 100".to_string().into());
        }
        serde_json::to_value(s).map_err(|e| e.to_string().into())
    }

    fn render(&self, _value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        Ok(Vec::new())
    }

    fn reload(&self, value: Option<&Value>, _ctx: &ApplyCtx) -> Result<(), String> {
        let s = value.map(settings_of).transpose()?.unwrap_or_default();
        *self.0.latest.lock().unwrap() = s.clone();
        if let Some(ctl) = self.0.ctl.get() {
            ctl.apply(s);
        }
        Ok(())
    }
}
