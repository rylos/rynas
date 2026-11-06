//! The `synology.hw` configuration domain: [`SynologySettings`] → `/etc/synohwd.toml`
//! (managed header, anti-drift), applied by restarting synohwd (it has no reload: SIGHUP
//! stops it), import of a file rynas never wrote the way synohwd reads it.
//!
//! It is the **only writer** of `/etc/synohwd.toml`, the reset-button network rescue
//! included (`reset_button: network_rescue`); `network.rescue` only reflects it. At the start
//! of rynasd, [`startup_value`] adopts the file: once on a Synology rynas never configured
//! (network rescue on by default) and whenever the file still has the `# BEGIN rynas network`
//! block that `network.rescue` wrote before (the block goes, its setting is kept).

use std::path::Path;

use rynas_api_types::synology::{ResetButtonAction, SynologySettings};
use rynas_config::{Applier, ApplyCtx, Config, ManagedFile};
use serde_json::Value;

use crate::conf::{self, CONF_PATH};

pub const SYNOLOGY_KEY: &str = "synology.hw";
pub const SYNOHWD_UNIT: &str = "synohwd.service";

/// Registered by rynasd only on a Synology (see [`crate::model::detect`]).
pub struct SynologyDomain;

pub fn settings_of(v: &Value) -> Result<SynologySettings, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("invalid synology.hw document: {e}"))
}

impl Applier for SynologyDomain {
    fn key(&self) -> &str {
        SYNOLOGY_KEY
    }

    /// Hardware after storage and services; nothing depends on it.
    fn order(&self) -> i32 {
        50
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let s = settings_of(&value)?;
        conf::validate(&s)?;
        serde_json::to_value(s).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let s = settings_of(value)?;
        Ok(vec![ManagedFile::whole(CONF_PATH, conf::render(&s), true)])
    }

    fn reload(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        if value.is_none() {
            // key removed: synohwd reads its defaults... from the file that stays; nothing to do
            return Ok(());
        }
        (ctx.log)("restarting synohwd (it reads its settings at start)");
        ctx.system.restart_unit(SYNOHWD_UNIT)
    }

    fn health(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        if value.is_some() && !ctx.system.unit_active(SYNOHWD_UNIT)? {
            return Err("synohwd is not running after the restart".into());
        }
        Ok(())
    }

    fn import(&self, ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        let text = ctx.read(Path::new(CONF_PATH));
        let (s, _) = conf::effective(text.as_deref());
        serde_json::to_value(s).map(Some).map_err(|e| e.to_string())
    }
}

/// Start of the block that `network.rescue` wrote in `synohwd.toml` before `synology.hw`
/// became the file's only writer.
pub const LEGACY_BLOCK: &str = "# BEGIN rynas network";

/// The file still has the legacy `network.rescue` block.
pub fn has_legacy_block(text: &str) -> bool {
    text.lines().any(|l| l.starts_with(LEGACY_BLOCK))
}

/// What `synology.hw` must be applied with at the start of rynasd (`None`: nothing to do),
/// with the revision comment. Only where `/etc/synohwd.toml` exists (synodeb installs it).
///
/// - The file has the legacy `# BEGIN rynas network` block: the current value (applied, or
///   imported) with the reset button synohwd runs with now — the block's — so applying it
///   rewrites the whole file without the block and nothing changes for synohwd.
/// - `synology.hw` was never applied: what synohwd runs with, plus network rescue on the
///   reset button (the Synology default of rynas), unless the old `network.rescue` key had it
///   off (`legacy_reset_button == Some(false)`): then synohwd's value stays.
pub fn startup_value(
    cfg: &Config,
    legacy_reset_button: Option<bool>,
) -> Result<Option<(Value, &'static str)>, String> {
    let file = rynas_hw::under(&cfg.options().root, CONF_PATH);
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(None);
    };
    let applied = cfg.applied(SYNOLOGY_KEY).map_err(|e| e.to_string())?;
    let legacy = has_legacy_block(&text);
    if applied.is_some() && !legacy {
        return Ok(None);
    }
    let (effective, _) = conf::effective(Some(&text));
    let (mut s, comment) = match &applied {
        Some(v) => (
            settings_of(v)?,
            "reset button setting moved from network.rescue",
        ),
        None => (
            effective.clone(),
            "synohwd settings adopted (network rescue on the reset button)",
        ),
    };
    if legacy {
        s.reset_button = effective.reset_button;
    } else if legacy_reset_button != Some(false) {
        s.reset_button = ResetButtonAction::NetworkRescue;
    }
    let v = serde_json::to_value(s).map_err(|e| e.to_string())?;
    Ok(Some((v, comment)))
}

/// `command:` button actions in the current file that rynas does not manage.
/// The configuration key that held the reset button before `synology.hw` owned it.
const LEGACY_RESCUE_KEY: &str = "network.rescue";

/// Start of rynasd on a Synology: makes `synology.hw` the owner of `/etc/synohwd.toml` —
/// network rescue on the reset button the first time, the legacy `# BEGIN rynas network`
/// block of `network.rescue` folded in (see [`startup_value`]). Returns the revision when
/// something was applied. Blocking.
pub fn adopt(cfg: &Config) -> Result<Option<u64>, String> {
    if !cfg.keys().iter().any(|k| k == SYNOLOGY_KEY) {
        return Ok(None);
    }
    // the old `network.rescue` value had the reset button (later values never do)
    let legacy = cfg
        .applied(LEGACY_RESCUE_KEY)
        .map_err(|e| e.to_string())?
        .and_then(|v| v.get("reset_button").and_then(Value::as_bool));
    let Some((v, comment)) = startup_value(cfg, legacy)? else {
        return Ok(None);
    };
    cfg.apply_value("rynas", SYNOLOGY_KEY, Some(v), comment, &|l| {
        tracing::info!("synology.hw: {l}")
    })
    .map(Some)
    .map_err(|e| e.to_string())
}

pub fn foreign_commands(root: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(rynas_hw::under(root, CONF_PATH)).ok();
    conf::effective(text.as_deref()).1
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rynas_config::{ConfigOptions, FileKind, MemorySystem};

    use super::*;

    /// What `network.rescue` rendered in `synohwd.toml` before (rynas 0.x up to the
    /// single-writer change), to build the files the migration meets.
    struct LegacyRescue;

    impl Applier for LegacyRescue {
        fn key(&self) -> &str {
            "network.rescue"
        }
        fn validate(&self, v: Value) -> Result<Value, rynas_config::Invalid> {
            Ok(v)
        }
        fn render(&self, v: &Value, _: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
            let line = if v["reset_button"] == Value::Bool(true) {
                format!("reset_button = \"command:{}\"\n", conf::RESET_HANDLER)
            } else {
                "reset_button = \"log\"\n".to_string()
            };
            let mut f = ManagedFile::block(CONF_PATH, "network", line);
            f.kind = FileKind::Block {
                id: "network".into(),
                drop_outside: vec!["reset_button".into()],
            };
            Ok(vec![f])
        }
    }

    fn open(state: &Path, root: &Path, legacy: bool) -> (Config, Arc<MemorySystem>) {
        let sys = Arc::new(MemorySystem::new("nas"));
        let mut d: Vec<Arc<dyn Applier>> = vec![Arc::new(SynologyDomain)];
        if legacy {
            d.push(Arc::new(LegacyRescue));
        }
        let cfg = Config::open(ConfigOptions::new(state, root), sys.clone(), d).unwrap();
        (cfg, sys)
    }

    /// Runs the start-up adoption like rynasd; returns the revision, if any.
    fn adopt(cfg: &Config, legacy: Option<bool>) -> Option<u64> {
        let (v, comment) = startup_value(cfg, legacy).unwrap()?;
        Some(
            cfg.apply_value("rynas", SYNOLOGY_KEY, Some(v), comment, &|_| {})
                .unwrap(),
        )
    }

    fn reset_of(cfg: &Config) -> ResetButtonAction {
        settings_of(&cfg.applied(SYNOLOGY_KEY).unwrap().unwrap())
            .unwrap()
            .reset_button
    }

    #[test]
    fn first_start_enables_network_rescue_once() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        // no synohwd.toml (synohwd not installed): nothing
        let (cfg, sys) = open(&d.path().join("state"), &root, false);
        assert!(startup_value(&cfg, None).unwrap().is_none());
        // synodeb's commented file: adopted with network rescue, the rest as synohwd runs
        std::fs::write(
            root.join("etc/synohwd.toml"),
            "# reset_button = \"log\"\nfan_mode = \"high\"\n",
        )
        .unwrap();
        assert!(adopt(&cfg, None).is_some());
        assert_eq!(reset_of(&cfg), ResetButtonAction::NetworkRescue);
        let text = std::fs::read_to_string(root.join("etc/synohwd.toml")).unwrap();
        assert!(text.contains(&format!(
            "\nreset_button = \"command:{}\"\n",
            conf::RESET_HANDLER
        )));
        assert!(text.contains("\nfan_mode = \"high\"\n"));
        assert!(
            sys.state()
                .calls
                .iter()
                .any(|c| c.contains("restart synohwd"))
        );
        // once: a later choice of the administrator stays
        let mut s = settings_of(&cfg.applied(SYNOLOGY_KEY).unwrap().unwrap()).unwrap();
        s.reset_button = ResetButtonAction::Ignore;
        cfg.apply_value(
            "admin",
            SYNOLOGY_KEY,
            Some(serde_json::to_value(s).unwrap()),
            "",
            &|_| {},
        )
        .unwrap();
        assert!(startup_value(&cfg, None).unwrap().is_none());
        assert_eq!(reset_of(&cfg), ResetButtonAction::Ignore);
        assert!(cfg.drift().unwrap().entries.is_empty());
    }

    #[test]
    fn first_start_keeps_a_rescue_turned_off_before() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/synohwd.toml"), "reset_button = \"ignore\"\n").unwrap();
        let (cfg, _) = open(&d.path().join("state"), &root, false);
        adopt(&cfg, Some(false)).unwrap();
        assert_eq!(reset_of(&cfg), ResetButtonAction::Ignore);
    }

    /// The `# BEGIN rynas network` block of the old `network.rescue` goes, its setting stays,
    /// whatever wrote the file last.
    #[test]
    fn legacy_block_migrates_cleanly() {
        for (synology_first, rescue_on) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let d = tempfile::tempdir().unwrap();
            let root = d.path().join("root");
            let state = d.path().join("state");
            std::fs::create_dir_all(root.join("etc")).unwrap();
            std::fs::write(
                root.join("etc/synohwd.toml"),
                "fan_mode = \"full\"\nreset_button = \"ignore\"\nwol = true\n",
            )
            .unwrap();
            // the old layout: both keys wrote the file
            {
                let (cfg, _) = open(&state, &root, true);
                if synology_first {
                    let mut s = settings_of(&cfg.current(SYNOLOGY_KEY).unwrap().unwrap()).unwrap();
                    s.led_brightness = 99;
                    cfg.apply_value(
                        "admin",
                        SYNOLOGY_KEY,
                        Some(serde_json::to_value(s).unwrap()),
                        "",
                        &|_| {},
                    )
                    .unwrap();
                }
                cfg.apply_value(
                    "rynas",
                    "network.rescue",
                    Some(serde_json::json!({"reset_button": rescue_on, "mdns_service": true})),
                    "",
                    &|_| {},
                )
                .unwrap();
                let text = std::fs::read_to_string(root.join("etc/synohwd.toml")).unwrap();
                assert!(has_legacy_block(&text), "{text}");
            }
            // the new layout: synology.hw alone
            let (cfg, sys) = open(&state, &root, false);
            assert!(adopt(&cfg, Some(rescue_on)).is_some());
            let text = std::fs::read_to_string(root.join("etc/synohwd.toml")).unwrap();
            assert!(!text.contains("rynas network"), "{text}");
            assert!(text.starts_with("# Managed by rynas"), "{text}");
            assert_eq!(text.matches("reset_button").count(), 1, "{text}");
            let want = if rescue_on {
                ResetButtonAction::NetworkRescue
            } else {
                ResetButtonAction::Log
            };
            assert_eq!(reset_of(&cfg), want, "{text}");
            assert_eq!(conf::effective(Some(&text)).0.reset_button, want);
            // the rest of the settings is what synohwd ran with
            let s = settings_of(&cfg.applied(SYNOLOGY_KEY).unwrap().unwrap()).unwrap();
            assert_eq!(s.fan_mode, rynas_api_types::synology::FanMode::Full);
            assert!(s.wol);
            assert_eq!(s.led_brightness, if synology_first { 99 } else { 255 });
            assert!(
                sys.state()
                    .calls
                    .iter()
                    .any(|c| c.contains("restart synohwd"))
            );
            assert!(cfg.drift().unwrap().entries.is_empty(), "{:?}", cfg.drift());
            // done: nothing at the next start
            assert!(startup_value(&cfg, Some(rescue_on)).unwrap().is_none());
        }
    }

    #[test]
    fn apply_renders_restarts_and_imports() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        // a hand-made file: synohwd's view of it is imported
        std::fs::write(
            root.join("etc/synohwd.toml"),
            "fan_mode = \"high\"\nfan_period = 900\npower_button = \"command:/x\"\n",
        )
        .unwrap();
        assert_eq!(
            foreign_commands(&root),
            vec!["power_button = command:/x".to_string()]
        );
        let sys = Arc::new(MemorySystem::new("nas"));
        let cfg = Config::open(
            ConfigOptions::new(d.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(SynologyDomain)],
        )
        .unwrap();
        let cur = settings_of(&cfg.current(SYNOLOGY_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(cur.fan_mode, rynas_api_types::synology::FanMode::High);
        assert_eq!(cur.fan_period_s, 300, "clamped like synohwd");
        // out of range: refused when staged
        let mut bad = cur.clone();
        bad.led_brightness = 0;
        assert!(
            cfg.stage(
                "admin",
                SYNOLOGY_KEY,
                Some(serde_json::to_value(&bad).unwrap())
            )
            .is_err()
        );
        let mut s = cur.clone();
        s.wol = true;
        cfg.stage(
            "admin",
            SYNOLOGY_KEY,
            Some(serde_json::to_value(&s).unwrap()),
        )
        .unwrap();
        cfg.apply("admin", "admin", "wol", &|_| {}, &|| false)
            .unwrap();
        let text = std::fs::read_to_string(root.join("etc/synohwd.toml")).unwrap();
        assert!(text.starts_with("# Managed by rynas"));
        assert!(text.contains("\nwol = true\n"));
        assert!(text.contains("\nfan_period = 300\n"));
        assert!(foreign_commands(&root).is_empty());
        assert!(
            sys.state()
                .calls
                .iter()
                .any(|c| c.contains("systemctl restart synohwd.service")),
            "{:?}",
            sys.state().calls
        );
        assert!(cfg.drift().unwrap().entries.is_empty());
        // a hand edit is drift
        std::fs::write(
            root.join("etc/synohwd.toml"),
            format!("{text}wol = false\n"),
        )
        .unwrap();
        assert_eq!(cfg.drift().unwrap().entries.len(), 1);
    }
}
