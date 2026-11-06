//! The `power.schedule` configuration domain.
//!
//! Files: the rules for `rynasctl power arm-wake` (`/etc/rynas/power-schedule.json`),
//! `rynas-power-wake.service` (oneshot, `RemainAfterExit=`: `ExecStart=` re-arms at boot,
//! `ExecStop=` arms before every shutdown; ordered after the unit of a hardware module that
//! reads the alarm at shutdown, so that it stops — and writes the alarm — first) and the
//! drop-in that pulls it
//! into `multi-user.target`. Apply: `daemon-reload`, restart of the unit (stop arms, start
//! arms again: the new rules are in the RTC at once). The shutdown rules are scheduler tasks,
//! staged together with this key by the API.

use std::path::{Path, PathBuf};

use rynas_api_types::power::PowerSchedule;
use rynas_config::{Applier, ApplyCtx, ManagedFile};
use serde_json::Value;

use crate::rules::normalize;

pub const POWER_KEY: &str = "power.schedule";
pub const SCHEDULE_FILE: &str = "/etc/rynas/power-schedule.json";
pub const WAKE_UNIT: &str = "rynas-power-wake.service";
pub const WAKE_UNIT_PATH: &str = "/etc/systemd/system/rynas-power-wake.service";
pub const WANTS_DROPIN: &str = "/etc/systemd/system/multi-user.target.d/rynas-power.conf";

pub struct PowerDomain {
    pub rynasctl: PathBuf,
    /// rynasd's state directory (`power/state.json`).
    pub state_dir: PathBuf,
    /// Root of `/sys` and `/proc` for `rynasctl` (tests: a fake tree).
    pub hw_root: PathBuf,
    /// The unit of a hardware module that reads the alarm at shutdown (its wake clock).
    pub alarm_reader: Option<String>,
}

pub fn schedule_of(v: &Value) -> Result<PowerSchedule, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("invalid power.schedule document: {e}"))
}

/// The service unit (without the rynas header).
pub fn render_unit(ctl: &Path, state_dir: &Path, hw_root: &Path, reader: Option<&str>) -> String {
    let mut args = format!(
        "--schedule {SCHEDULE_FILE} --state-dir {}",
        state_dir.display()
    );
    if hw_root != Path::new("/") {
        args.push_str(&format!(" --root {}", hw_root.display()));
    }
    let ctl = ctl.display();
    let after = match reader {
        Some(u) => format!(
            "# stopped before {u}, which reads the alarm at shutdown\nAfter=local-fs.target time-sync.target {u}\n"
        ),
        None => "After=local-fs.target time-sync.target\n".to_string(),
    };
    format!(
        "[Unit]\n\
         Description=rynas power schedule: RTC wake alarm for the next scheduled startup\n\
         Documentation=https://github.com/rylos/rynas\n\
         {after}\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart={ctl} power arm-wake --boot {args}\n\
         ExecStop={ctl} power arm-wake --shutdown {args}\n\
         TimeoutSec=30\n"
    )
}

pub fn render_dropin() -> String {
    format!(
        "# rynas power schedule (power.schedule configuration key)\n[Unit]\nWants={WAKE_UNIT}\n"
    )
}

impl Applier for PowerDomain {
    fn key(&self) -> &str {
        POWER_KEY
    }

    /// After the scheduler (40), whose tasks run the shutdown rules.
    fn order(&self) -> i32 {
        45
    }

    fn validate(&self, value: Value) -> Result<Value, rynas_config::Invalid> {
        let s = normalize(schedule_of(&value)?)?;
        serde_json::to_value(s).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let s = schedule_of(value)?;
        let json = serde_json::to_string_pretty(&s).map_err(|e| e.to_string())?;
        Ok(vec![
            // JSON has no comments: no header, the hash alone detects drift
            ManagedFile::whole(SCHEDULE_FILE, json, false),
            ManagedFile::whole(
                WAKE_UNIT_PATH,
                render_unit(
                    &self.rynasctl,
                    &self.state_dir,
                    &self.hw_root,
                    self.alarm_reader.as_deref(),
                ),
                true,
            ),
            ManagedFile::whole(WANTS_DROPIN, render_dropin(), true),
        ])
    }

    fn reconcile(
        &self,
        _from: Option<&Value>,
        to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        ctx.system.daemon_reload()?;
        match to {
            Some(_) => {
                (ctx.log)("arming the RTC wake alarm with the new rules");
                ctx.system.restart_unit(WAKE_UNIT)
            }
            None => ctx.system.stop_unit(WAKE_UNIT),
        }
    }

    fn health(&self, value: Option<&Value>, ctx: &ApplyCtx) -> Result<(), String> {
        if value.is_some() && !ctx.system.unit_active(WAKE_UNIT)? {
            return Err(format!("{WAKE_UNIT} is not active"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rynas_api_types::power::{PowerAction, PowerRule, Weekday};
    use rynas_config::{Config, ConfigOptions, MemorySystem};

    use super::*;

    #[test]
    fn apply_writes_files_and_restarts_the_unit() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        let sys = Arc::new(MemorySystem::new("nas"));
        let cfg = Config::open(
            ConfigOptions::new(d.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(PowerDomain {
                rynasctl: "/usr/libexec/rynas/rynasctl".into(),
                state_dir: "/var/lib/rynas".into(),
                hw_root: "/".into(),
                alarm_reader: Some("synohwd.service".into()),
            })],
        )
        .unwrap();
        assert_eq!(cfg.current(POWER_KEY).unwrap(), None);
        let s = PowerSchedule {
            rules: vec![PowerRule {
                id: "up".into(),
                enabled: true,
                action: PowerAction::Startup,
                days: vec![Weekday::Mon],
                hour: 7,
                minute: 0,
                shutdown: None,
            }],
        };
        // conflicts are refused when staged
        let mut bad = s.clone();
        bad.rules.push(PowerRule {
            id: "down".into(),
            action: PowerAction::Shutdown,
            minute: 5,
            ..s.rules[0].clone()
        });
        assert!(
            cfg.stage(
                "admin",
                POWER_KEY,
                Some(serde_json::to_value(&bad).unwrap())
            )
            .unwrap_err()
            .to_string()
            .contains("less than 10 minutes")
        );
        cfg.stage("admin", POWER_KEY, Some(serde_json::to_value(&s).unwrap()))
            .unwrap();
        cfg.apply("admin", "admin", "", &|_| {}, &|| false).unwrap();
        let unit =
            std::fs::read_to_string(root.join("etc/systemd/system/rynas-power-wake.service"))
                .unwrap();
        assert!(unit.starts_with("# Managed by rynas"));
        assert!(unit.contains(
            "ExecStop=/usr/libexec/rynas/rynasctl power arm-wake --shutdown --schedule /etc/rynas/power-schedule.json --state-dir /var/lib/rynas\n"
        ));
        assert!(unit.contains("After=local-fs.target time-sync.target synohwd.service\n"));
        let json = std::fs::read_to_string(root.join("etc/rynas/power-schedule.json")).unwrap();
        let back: PowerSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        let dropin = std::fs::read_to_string(
            root.join("etc/systemd/system/multi-user.target.d/rynas-power.conf"),
        )
        .unwrap();
        assert!(dropin.contains("Wants=rynas-power-wake.service\n"));
        let calls = sys.state().calls.clone();
        assert!(
            calls.iter().any(|c| c.contains("daemon-reload")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.contains("systemctl restart rynas-power-wake.service")),
            "{calls:?}"
        );
        assert!(cfg.drift().unwrap().entries.is_empty());
    }

    #[test]
    fn unit_with_a_fake_root() {
        let u = render_unit(
            Path::new("/x/rynasctl"),
            Path::new("/s"),
            Path::new("/srv/fake"),
            None,
        );
        assert!(u.contains("ExecStart=/x/rynasctl power arm-wake --boot --schedule /etc/rynas/power-schedule.json --state-dir /s --root /srv/fake\n"));
        assert!(
            u.contains("After=local-fs.target time-sync.target\n"),
            "no module: no reader unit"
        );
    }
}
