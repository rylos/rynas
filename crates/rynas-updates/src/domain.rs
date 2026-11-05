//! The `updates.auto` configuration domain: automatic updates with unattended-upgrades.
//!
//! Files:
//! - `/etc/apt/apt.conf.d/53rynas-unattended-upgrades`: read after Debian's
//!   `50unattended-upgrades` and synodeb's `52synodeb-unattended-upgrades`, so its values win.
//!   It clears and rewrites the origins (security only, or all of Debian stable), turns the
//!   upgrade on or off, sets the automatic reboot and keeps every `Remove-Unused-*` off (on
//!   synodeb an autoremove would take the old synodeb kernel and `synology-ds923`). The package
//!   blacklist (synodeb's kernels and package) is not touched: it still applies. Package lists
//!   are refreshed daily in every mode, for the "updates available" notification.
//! - drop-ins of `apt-daily-upgrade.timer` (the window: start time + randomized delay) and
//!   `apt-daily.timer` (the lists, an hour before).
//!
//! Apply: `daemon-reload`, restart of the two timers (when installed).

use std::path::Path;

use rynas_api_types::updates::{AutoMode, AutoUpdates};
use rynas_config::{Applier, ApplyCtx, Invalid, ManagedFile};
use serde_json::Value;

pub const AUTO_KEY: &str = "updates.auto";
pub const APT_CONF: &str = "/etc/apt/apt.conf.d/53rynas-unattended-upgrades";
pub const UPGRADE_TIMER: &str = "apt-daily-upgrade.timer";
pub const LISTS_TIMER: &str = "apt-daily.timer";
pub const UPGRADE_DROPIN: &str = "/etc/systemd/system/apt-daily-upgrade.timer.d/50-rynas.conf";
pub const LISTS_DROPIN: &str = "/etc/systemd/system/apt-daily.timer.d/50-rynas.conf";
/// Where unattended-upgrades lives when installed.
pub const UNATTENDED_BIN: &str = "/usr/bin/unattended-upgrade";

pub struct AutoDomain;

pub fn auto_of(v: &Value) -> Result<AutoUpdates, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("invalid updates.auto document: {e}"))
}

/// Checks the ranges.
pub fn check(a: &AutoUpdates) -> Result<(), Invalid> {
    if a.hour > 23 {
        return Err(Invalid::at("hour", "the hour must be 0-23"));
    }
    if a.minute > 59 {
        return Err(Invalid::at("minute", "the minute must be 0-59"));
    }
    if !(10..=360).contains(&a.window_min) {
        return Err(Invalid::at("window_min", "the window must be 10-360 minutes"));
    }
    if a.reboot_hour > 23 {
        return Err(Invalid::at("reboot_hour", "the hour must be 0-23"));
    }
    if a.reboot_minute > 59 {
        return Err(Invalid::at("reboot_minute", "the minute must be 0-59"));
    }
    Ok(())
}

/// The apt configuration (without the rynas header).
pub fn render_apt_conf(a: &AutoUpdates) -> String {
    let on = a.mode != AutoMode::Off;
    let mut s = String::from(
        "// rynas automatic updates (configuration key updates.auto): edit them in rynas.\n\
         // Read after 50unattended-upgrades and 52synodeb-unattended-upgrades: these values win;\n\
         // their Package-Blacklist (synodeb kernels and package) still applies.\n\
         APT::Periodic::Update-Package-Lists \"1\";\n",
    );
    s.push_str(&format!(
        "APT::Periodic::Unattended-Upgrade \"{}\";\n",
        u8::from(on)
    ));
    s.push_str("#clear Unattended-Upgrade::Allowed-Origins;\n");
    s.push_str("#clear Unattended-Upgrade::Origins-Pattern;\n");
    s.push_str("Unattended-Upgrade::Origins-Pattern {\n");
    s.push_str("        \"origin=Debian,codename=${distro_codename},label=Debian-Security\";\n");
    s.push_str(
        "        \"origin=Debian,codename=${distro_codename}-security,label=Debian-Security\";\n",
    );
    if a.mode == AutoMode::All {
        s.push_str("        \"origin=Debian,codename=${distro_codename},label=Debian\";\n");
        s.push_str("        \"origin=Debian,codename=${distro_codename}-updates\";\n");
    }
    s.push_str("};\n");
    s.push_str(&format!(
        "Unattended-Upgrade::Automatic-Reboot \"{}\";\n",
        a.reboot
    ));
    s.push_str(&format!(
        "Unattended-Upgrade::Automatic-Reboot-Time \"{:02}:{:02}\";\n",
        a.reboot_hour, a.reboot_minute
    ));
    // a NAS has SSH sessions open all the time: they must not block a planned reboot
    s.push_str("Unattended-Upgrade::Automatic-Reboot-WithUsers \"true\";\n");
    s.push_str("// never remove packages by itself (autoremove is an explicit action in rynas)\n");
    s.push_str("Unattended-Upgrade::Remove-Unused-Dependencies \"false\";\n");
    s.push_str("Unattended-Upgrade::Remove-New-Unused-Dependencies \"false\";\n");
    s.push_str("Unattended-Upgrade::Remove-Unused-Kernel-Packages \"false\";\n");
    s.push_str("Unattended-Upgrade::MinimalSteps \"true\";\n");
    s
}

/// The upgrade timer drop-in: the window.
pub fn render_upgrade_dropin(a: &AutoUpdates) -> String {
    format!(
        "# rynas automatic updates (updates.auto): daily window\n[Timer]\nOnCalendar=\nOnCalendar=*-*-* {:02}:{:02}:00\nRandomizedDelaySec={}m\nPersistent=true\n",
        a.hour, a.minute, a.window_min
    )
}

/// The lists timer drop-in: an hour before the window.
pub fn render_lists_dropin(a: &AutoUpdates) -> String {
    let start = (u32::from(a.hour) * 60 + u32::from(a.minute) + 1440 - 60) % 1440;
    format!(
        "# rynas automatic updates (updates.auto): package lists before the window\n[Timer]\nOnCalendar=\nOnCalendar=*-*-* {:02}:{:02}:00\nRandomizedDelaySec=30m\nPersistent=true\n",
        start / 60,
        start % 60
    )
}

fn unit_installed(root: &Path, unit: &str) -> bool {
    ["lib/systemd/system", "usr/lib/systemd/system", "etc/systemd/system"]
        .iter()
        .any(|d| root.join(d).join(unit).exists())
}

/// unattended-upgrades is installed below `root`.
pub fn unattended_installed(root: &Path) -> bool {
    root.join(UNATTENDED_BIN.trim_start_matches('/')).exists()
}

/// What the system does today when rynas never applied the key: unattended upgrades on in
/// some `apt.conf.d` file (Debian's `20auto-upgrades`, synodeb's file) → all, else off.
pub fn import_from(root: &Path) -> Option<AutoUpdates> {
    if !unattended_installed(root) {
        return None;
    }
    let dir = root.join("etc/apt/apt.conf.d");
    let mut on = false;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        for l in text.lines().map(str::trim) {
            if l.starts_with("APT::Periodic::Unattended-Upgrade") {
                on = l.contains("\"1\"");
            }
        }
    }
    Some(AutoUpdates {
        mode: if on { AutoMode::All } else { AutoMode::Off },
        ..AutoUpdates::default()
    })
}

impl Applier for AutoDomain {
    fn key(&self) -> &str {
        AUTO_KEY
    }

    fn order(&self) -> i32 {
        70
    }

    fn validate(&self, value: Value) -> Result<Value, Invalid> {
        let a = auto_of(&value)?;
        check(&a)?;
        serde_json::to_value(a).map_err(|e| e.to_string().into())
    }

    fn render(&self, value: &Value, _ctx: &ApplyCtx) -> Result<Vec<ManagedFile>, String> {
        let a = auto_of(value)?;
        Ok(vec![
            ManagedFile::whole(APT_CONF, render_apt_conf(&a), true),
            ManagedFile::whole(UPGRADE_DROPIN, render_upgrade_dropin(&a), true),
            ManagedFile::whole(LISTS_DROPIN, render_lists_dropin(&a), true),
        ])
    }

    fn reconcile(
        &self,
        _from: Option<&Value>,
        _to: Option<&Value>,
        ctx: &ApplyCtx,
    ) -> Result<(), String> {
        ctx.system.daemon_reload()?;
        for t in [LISTS_TIMER, UPGRADE_TIMER] {
            if unit_installed(ctx.root, t) {
                ctx.system.restart_unit(t)?;
            } else {
                (ctx.log)(&format!("{t} is not installed: skipped"));
            }
        }
        Ok(())
    }

    fn import(&self, ctx: &ApplyCtx) -> Result<Option<Value>, String> {
        Ok(import_from(ctx.root).map(|a| serde_json::to_value(a).expect("serializable")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rynas_config::{Config, ConfigOptions, MemorySystem};

    use super::*;

    #[test]
    fn renders_security_only() {
        let a = AutoUpdates {
            mode: AutoMode::Security,
            reboot: true,
            reboot_hour: 4,
            reboot_minute: 30,
            ..Default::default()
        };
        let c = render_apt_conf(&a);
        assert!(c.contains("APT::Periodic::Unattended-Upgrade \"1\";\n"));
        assert!(c.contains("#clear Unattended-Upgrade::Origins-Pattern;\n"));
        assert!(c.contains("label=Debian-Security"));
        assert!(!c.contains("-updates\""));
        assert!(c.contains("Automatic-Reboot \"true\";\n"));
        assert!(c.contains("Automatic-Reboot-Time \"04:30\";\n"));
        assert!(c.contains("Remove-Unused-Kernel-Packages \"false\";\n"));
        assert!(!c.contains("Package-Blacklist {"), "synodeb's blacklist is kept");
        let off = render_apt_conf(&AutoUpdates::default());
        assert!(off.contains("Unattended-Upgrade \"0\";\n"));
        assert!(off.contains("Update-Package-Lists \"1\";\n"));
        let all = render_apt_conf(&AutoUpdates { mode: AutoMode::All, ..Default::default() });
        assert!(all.contains("codename=${distro_codename}-updates\""));
    }

    #[test]
    fn timers() {
        let a = AutoUpdates { hour: 0, minute: 30, window_min: 90, ..Default::default() };
        let u = render_upgrade_dropin(&a);
        assert!(u.contains("OnCalendar=\nOnCalendar=*-*-* 00:30:00\nRandomizedDelaySec=90m\n"));
        assert!(render_lists_dropin(&a).contains("OnCalendar=*-*-* 23:30:00\n"));
    }

    #[test]
    fn ranges() {
        assert!(check(&AutoUpdates::default()).is_ok());
        assert_eq!(check(&AutoUpdates { hour: 24, ..Default::default() }).unwrap_err().field.as_deref(), Some("hour"));
        assert_eq!(check(&AutoUpdates { window_min: 5, ..Default::default() }).unwrap_err().field.as_deref(), Some("window_min"));
        assert!(check(&AutoUpdates { reboot_minute: 60, ..Default::default() }).is_err());
    }

    #[test]
    fn apply_and_import() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("root");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::create_dir_all(root.join("etc/apt/apt.conf.d")).unwrap();
        std::fs::create_dir_all(root.join("lib/systemd/system")).unwrap();
        std::fs::write(root.join("usr/bin/unattended-upgrade"), "").unwrap();
        std::fs::write(root.join("lib/systemd/system/apt-daily-upgrade.timer"), "").unwrap();
        std::fs::write(
            root.join("etc/apt/apt.conf.d/52synodeb-unattended-upgrades"),
            "APT::Periodic::Unattended-Upgrade \"1\";\n",
        )
        .unwrap();
        let sys = Arc::new(MemorySystem::new("nas"));
        let cfg = Config::open(
            ConfigOptions::new(d.path().join("state"), &root),
            sys.clone(),
            vec![Arc::new(AutoDomain)],
        )
        .unwrap();
        let cur: AutoUpdates = serde_json::from_value(cfg.current(AUTO_KEY).unwrap().unwrap()).unwrap();
        assert_eq!(cur.mode, AutoMode::All, "imported from synodeb's file");
        let a = AutoUpdates { mode: AutoMode::Security, ..Default::default() };
        cfg.stage("admin", AUTO_KEY, Some(serde_json::to_value(&a).unwrap())).unwrap();
        cfg.apply("admin", "admin", "", &|_| {}, &|| false).unwrap();
        let conf = std::fs::read_to_string(root.join("etc/apt/apt.conf.d/53rynas-unattended-upgrades")).unwrap();
        assert!(conf.starts_with("# Managed by rynas"));
        assert!(root.join("etc/systemd/system/apt-daily.timer.d/50-rynas.conf").exists());
        let calls = sys.state().calls.clone();
        assert!(calls.iter().any(|c| c.contains("restart apt-daily-upgrade.timer")), "{calls:?}");
        assert!(!calls.iter().any(|c| c.contains("restart apt-daily.timer")), "not installed: {calls:?}");
        assert!(cfg.drift().unwrap().entries.is_empty());
        let bad = AutoUpdates { window_min: 1000, ..Default::default() };
        assert!(cfg.stage("admin", AUTO_KEY, Some(serde_json::to_value(&bad).unwrap())).is_err());
    }
}
