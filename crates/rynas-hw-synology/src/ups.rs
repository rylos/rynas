//! synodeb's UPS layer (S2–S4 in `docs/synodeb-integration.md`), the module's
//! `rynas_module::UpsLayer`. rynas is the interface, synodeb the executor: synodeb owns
//! `/etc/nut`, rynas never writes it.
//!
//! - Policy (`ups.safemode`): `SAFEMODE_AFTER` and `PRE_SAFEMODE_TIMEOUT` of
//!   `/etc/synodeb/ups.conf`, in a `# BEGIN rynas ups` block (other assignments of the two keys
//!   are dropped, the rest of the file stays synodeb's). `synodeb-ups` reads the file at every
//!   event, so an apply needs no reload.
//! - Battery tests: `synodeb-ups test quick|deep|stop|result`.
//! - Safe Mode: synodeb's pre-Safe Mode hook (`/etc/synodeb/ups-pre-safemode.d/50-rynas`) runs
//!   `rynasctl ups safe-mode <reason>`; rynasd stops its work and records the notice below.
//! - Setup wizard: the test run is `synodeb-ups probe --driver D --port P --opt k=v …
//!   --timeout S`, the apply `synodeb-ups setup --keep-password --driver …` (back to the default
//!   with `--default-driver`). A synodeb without them blocks the wizard. The current driver
//!   comes from `synodeb-ups status --json` (synodeb 4.5), else the saved
//!   `/etc/synodeb/ups-driver.conf`.
//!
//! The layer is present when `/etc/synodeb/ups.conf` exists ([`present`]; the file, not the
//! directory).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rynas_api_types::ups::{
    BatteryTest, UpsDevice, UpsOption, UpsProbeState, UpsSettings, UpsSetupBlock,
};
use rynas_config::ManagedFile;
use rynas_config::files::FileKind;
use rynas_module::ups::{Runner, UpsLayer};
use rynas_tasks::{Cmd, CmdOutput};
use serde_json::Value;

/// synodeb's UPS configuration (its presence selects this layer).
pub const UPS_CONF: &str = "/etc/synodeb/ups.conf";
/// synodeb's UPS tool.
pub const SYNODEB_UPS: &str = "/usr/sbin/synodeb-ups";
/// synodeb's saved driver choice (`UPS_DRIVER`, `UPS_PORT`, `UPS_OPTIONS`).
pub const DRIVER_CONF: &str = "/etc/synodeb/ups-driver.conf";
const KEYS: (&str, &str) = ("SAFEMODE_AFTER", "PRE_SAFEMODE_TIMEOUT");
const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_secs(20);
const SETUP_TIMEOUT: Duration = Duration::from_secs(90);

/// Whether synodeb's UPS layer is installed below the configuration root. The file, not the
/// directory: older rynas packages installed `/etc/synodeb/ups-pre-safemode.d/` themselves.
pub fn present(root: &Path) -> bool {
    rynas_config::files::under_root(root, Path::new(UPS_CONF)).is_file()
}

/// The lines of the rynas block in synodeb's `ups.conf`.
pub fn render_block(s: &UpsSettings) -> String {
    format!(
        "# Safe Mode settings of the rynas UPS page (ups.safemode). NUT low battery and a\n\
         # critical UPS (OB+LB) always start Safe Mode as well, whichever comes first.\n\
         SAFEMODE_AFTER={}\n\
         PRE_SAFEMODE_TIMEOUT={}\n",
        s.safemode_after_s.unwrap_or(0),
        s.pre_safemode_timeout_s
    )
}

/// The two keys as the shell would see them (the last assignment wins; comments skipped).
pub fn parse_conf(text: &str) -> UpsSettings {
    let mut s = UpsSettings::default();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('#') {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        // `X=1 # comment` and quotes
        let v = v
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches(['"', '\'']);
        match (k.trim(), v.parse::<u32>()) {
            (k, Ok(n)) if k == KEYS.0 => s.safemode_after_s = (n > 0).then_some(n),
            (k, Ok(n)) if k == KEYS.1 => s.pre_safemode_timeout_s = n,
            _ => {}
        }
    }
    s
}

/// The text after `ups: ` of `synodeb-ups test result`.
pub fn result_text(out: &str) -> String {
    let l = out.lines().next().unwrap_or("").trim();
    l.split_once(": ").map_or(l, |(_, r)| r).trim().to_string()
}

/// synodeb's driver arguments: `--driver D --port P --opt k=v …` (setup and probe).
pub fn driver_args(d: &UpsDevice) -> Vec<String> {
    let mut a = vec![
        "--driver".into(),
        d.driver.clone(),
        "--port".into(),
        d.port.clone(),
    ];
    for o in &d.options {
        a.push("--opt".into());
        a.push(rynas_nut::setup::option_arg(o));
    }
    a
}

fn option_of(w: &str) -> UpsOption {
    match w.split_once('=') {
        Some((k, v)) => UpsOption::new(k, Some(v)),
        None => UpsOption::new(w, None),
    }
}

/// The driver of `synodeb-ups status --json` (schema 1): `Some(None)` for synodeb's default
/// driver, `None` when the output is not that document.
pub fn parse_status_json(text: &str) -> Option<Option<UpsDevice>> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    let d = v.get("driver")?;
    if d.get("default").and_then(Value::as_bool).unwrap_or(false) {
        return Some(None);
    }
    let name = d.get("name")?.as_str()?.to_string();
    let port = d.get("port").and_then(Value::as_str).unwrap_or("auto").to_string();
    let options = d
        .get("options")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(option_of).collect())
        .unwrap_or_default();
    Some(Some(UpsDevice { driver: name, port, options }))
}

/// synodeb's saved choice (`UPS_DRIVER=`, `UPS_PORT=`, `UPS_OPTIONS='k=v flag …'`; values
/// never hold spaces or quotes, commas are allowed).
pub fn parse_driver_conf(text: &str) -> Option<UpsDevice> {
    let mut driver = None;
    let mut port = None;
    let mut options = Vec::new();
    for l in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches(['"', '\'']);
        match k.trim() {
            "UPS_DRIVER" => driver = Some(v.to_string()).filter(|v| !v.is_empty()),
            "UPS_PORT" => port = Some(v.to_string()).filter(|v| !v.is_empty()),
            "UPS_OPTIONS" => {
                options = v.split(' ').filter(|w| !w.is_empty()).map(option_of).collect()
            }
            _ => {}
        }
    }
    Some(UpsDevice {
        driver: driver?,
        port: port.unwrap_or_else(|| "auto".into()),
        options,
    })
}

/// synodeb's UPS layer.
pub struct SynodebUps {
    /// The tool (`/usr/sbin/synodeb-ups`; tests give another path).
    pub tool: PathBuf,
}

impl Default for SynodebUps {
    fn default() -> Self {
        Self {
            tool: PathBuf::from(SYNODEB_UPS),
        }
    }
}

impl SynodebUps {
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.tool).env_clear()
    }

    fn test(&self, arg: &str) -> Cmd {
        self.cmd()
            .args(["test", arg])
            .env("PATH", PATH)
            .timeout(CMD_TIMEOUT)
    }
}

impl UpsLayer for SynodebUps {
    fn policy_path(&self) -> &'static str {
        UPS_CONF
    }

    fn render_policy(&self, s: &UpsSettings) -> ManagedFile {
        let mut f = ManagedFile::block(UPS_CONF, "ups", render_block(s));
        if let FileKind::Block { drop_outside, .. } = &mut f.kind {
            *drop_outside = vec![format!("{}=", KEYS.0), format!("{}=", KEYS.1)];
        }
        f
    }

    fn parse_policy(&self, text: &str) -> UpsSettings {
        parse_conf(text)
    }

    fn policy_note(&self) -> &'static str {
        "synodeb-ups reads ups.conf at its next event: no reload needed"
    }

    fn stop_notice(&self, reason: &str) -> (String, String) {
        (
            format!("Safe Mode: {reason}"),
            "The UPS started Safe Mode: rynas stops its running and scheduled tasks; services \
             stop, volumes are unmounted and the NAS restarts when power is back."
                .into(),
        )
    }

    fn test_cmd(&self, kind: BatteryTest) -> Cmd {
        self.test(kind.arg())
    }

    fn result_cmd(&self) -> Cmd {
        self.test("result")
    }

    fn parse_result(&self, stdout: &str) -> String {
        result_text(stdout)
    }

    fn setup_blocked(&self, runner: &dyn Runner) -> Option<UpsSetupBlock> {
        let help = runner
            .run(&self.cmd().args(["setup", "--help"]).timeout(SHORT))
            .map(|o| format!("{}{}", o.stdout, o.stderr))
            .unwrap_or_default();
        (!help.contains("--driver")).then_some(UpsSetupBlock::ModuleTooOld)
    }

    fn too_old_message(&self) -> &'static str {
        "this synodeb cannot set up other UPS drivers yet: update synodeb"
    }

    fn probe_cmd(&self, d: &UpsDevice, timeout: Duration) -> Cmd {
        self.cmd()
            .arg("probe")
            .args(driver_args(d))
            .args(["--timeout", &timeout.as_secs().to_string()])
            .timeout(timeout + Duration::from_secs(15))
    }

    /// Exit 2: bad usage or unknown driver; 3: an active nut-driver holds the device.
    fn probe_exit_state(&self, code: Option<i32>) -> Option<UpsProbeState> {
        match code {
            Some(2) => Some(UpsProbeState::NoDriver),
            Some(3) => Some(UpsProbeState::Busy),
            _ => None,
        }
    }

    fn setup_cmd(&self, to: Option<&UpsDevice>) -> Cmd {
        let c = self
            .cmd()
            .timeout(SETUP_TIMEOUT)
            .args(["setup", "--keep-password"]);
        match to {
            Some(d) => c.args(driver_args(d)),
            None => c.arg("--default-driver"),
        }
    }

    fn current_device(
        &self,
        runner: &dyn Runner,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Option<UpsDevice> {
        runner
            .run(&self.cmd().args(["status", "--json"]).timeout(SHORT))
            .ok()
            .and_then(|o| parse_status_json(&o.stdout))
            .unwrap_or_else(|| read(Path::new(DRIVER_CONF)).and_then(|t| parse_driver_conf(&t)))
    }

    /// The test instance's UPS (`0925:1234`) answers only with the `armac` subdriver, as the
    /// generic dry runner's drivers do.
    fn dry_output(&self, cmd: &Cmd) -> Option<CmdOutput> {
        let program = Path::new(cmd.program()).file_name()?.to_string_lossy().into_owned();
        if program != "synodeb-ups" {
            return None;
        }
        let line = cmd.display();
        let out = |code: i32, stdout: &str, stderr: &str| CmdOutput {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        };
        Some(if line.contains("--help") {
            out(
                0,
                "usage: synodeb-ups setup [--keep-password] [--no-stop] [--driver NAME [--port PORT] [--opt NAME[=VALUE]]...]\n",
                "",
            )
        } else if line.contains("subdriver=armac") {
            out(
                0,
                "battery.charge: 100\nbattery.voltage: 13.70\ninput.voltage: 230.0\n\
                 ups.load: 12\nups.status: OL\n",
                "",
            )
        } else {
            out(1, "", "Device not supported!\n")
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use rynas_config::files;

    #[test]
    fn conf_round_trip() {
        let synodeb = "# synodeb\n#SAFEMODE_AFTER=0\nDRYRUN=0\nSAFEMODE_AFTER=300 # five\nPRE_SAFEMODE_TIMEOUT=\"90\"\n";
        let s = parse_conf(synodeb);
        assert_eq!(s.safemode_after_s, Some(300));
        assert_eq!(s.pre_safemode_timeout_s, 90);
        assert_eq!(parse_conf("SAFEMODE_AFTER=0\n"), UpsSettings::default());
        let want = UpsSettings {
            safemode_after_s: Some(600),
            pre_safemode_timeout_s: 120,
        };
        let f = SynodebUps::default().render_policy(&want);
        let (text, _) = files::render_final(&f.kind, &f.content, Some(synodeb));
        assert!(text.contains("#SAFEMODE_AFTER=0\n"), "{text}");
        assert!(text.contains("DRYRUN=0\n"));
        assert!(!text.contains("=300"));
        assert!(!text.contains("\"90\""));
        assert_eq!(parse_conf(&text), want);
        // the keys of the generic NUT policy file are not read
        assert_eq!(parse_conf("SHUTDOWN_AFTER=300\n"), UpsSettings::default());
        assert_eq!(result_text("ups: Done and passed\n"), "Done and passed");
    }

    #[test]
    fn present_only_with_the_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("etc/synodeb/ups-pre-safemode.d")).unwrap();
        assert!(!present(d.path()));
        std::fs::write(d.path().join("etc/synodeb/ups.conf"), "").unwrap();
        assert!(present(d.path()));
    }

    #[test]
    fn status_json_and_saved_driver() {
        let custom = r#"{"schema":1,"ups":"ups@localhost","status":"OL","driver":{"name":"nutdrv_qx","port":"auto","options":["vendorid=0925","subdriver=armac","norating"],"default":false,"conf":"/etc/synodeb/ups-driver.conf"}}"#;
        let dev = parse_status_json(custom).unwrap().unwrap();
        assert_eq!(dev.driver, "nutdrv_qx");
        assert_eq!(dev.options.len(), 3);
        let default = r#"{"schema":1,"driver":{"name":"usbhid-ups","port":"auto","options":[],"default":true,"conf":null}}"#;
        assert!(parse_status_json(default).unwrap().is_none());
        assert!(parse_status_json("driver: usbhid-ups").is_none());
        let saved = "UPS_DRIVER=nutdrv_qx\nUPS_PORT=auto\nUPS_OPTIONS='vendorid=0925 productid=1234 norating'\n";
        let dev = parse_driver_conf(saved).unwrap();
        assert_eq!(dev.options[2], UpsOption::new("norating", None));
    }

    /// Answers `synodeb-ups` by a substring of the command line.
    struct Fake {
        rules: Vec<(&'static str, i32, &'static str)>,
        calls: Mutex<Vec<String>>,
    }

    impl Runner for Fake {
        fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
            let line = cmd.display();
            self.calls.lock().unwrap().push(line.clone());
            let (code, out) = self
                .rules
                .iter()
                .find(|r| line.contains(r.0))
                .map_or((1, ""), |r| (r.1, r.2));
            Ok(CmdOutput {
                code: Some(code),
                stdout: out.into(),
                stderr: String::new(),
            })
        }
    }

    fn armac() -> UpsDevice {
        UpsDevice {
            driver: "nutdrv_qx".into(),
            port: "auto".into(),
            options: ["vendorid=0925", "productid=1234", "subdriver=armac", "norating"]
                .into_iter()
                .map(option_of)
                .collect(),
        }
    }

    #[test]
    fn commands_and_gate() {
        let l = SynodebUps::default();
        let old = Fake {
            rules: vec![("--help", 0, "usage: synodeb-ups setup [--keep-password]\n")],
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(l.setup_blocked(&old), Some(UpsSetupBlock::ModuleTooOld));
        let new = Fake {
            rules: vec![(
                "--help",
                0,
                "synodeb-ups setup [--keep-password] [--no-stop] [--driver NAME [--port PORT] [--opt NAME[=VALUE]]...]\n",
            )],
            calls: Mutex::new(Vec::new()),
        };
        assert_eq!(l.setup_blocked(&new), None);
        let d = armac();
        assert_eq!(
            l.probe_cmd(&d, Duration::from_secs(45)).display(),
            "/usr/sbin/synodeb-ups probe --driver nutdrv_qx --port auto --opt vendorid=0925 --opt productid=1234 --opt subdriver=armac --opt norating --timeout 45"
        );
        assert_eq!(l.probe_exit_state(Some(3)), Some(UpsProbeState::Busy));
        assert_eq!(l.probe_exit_state(Some(2)), Some(UpsProbeState::NoDriver));
        assert_eq!(l.probe_exit_state(Some(1)), None);
        assert_eq!(
            l.setup_cmd(Some(&d)).display(),
            "/usr/sbin/synodeb-ups setup --keep-password --driver nutdrv_qx --port auto --opt vendorid=0925 --opt productid=1234 --opt subdriver=armac --opt norating"
        );
        assert_eq!(
            l.setup_cmd(None).display(),
            "/usr/sbin/synodeb-ups setup --keep-password --default-driver"
        );
        assert_eq!(l.test_cmd(BatteryTest::Quick).display(), "/usr/sbin/synodeb-ups test quick");
        assert_eq!(l.result_cmd().display(), "/usr/sbin/synodeb-ups test result");
        // the current driver: status --json first, the saved file on older synodeb
        let json = Fake {
            rules: vec![("status --json", 0, r#"{"schema":1,"driver":{"name":"usbhid-ups","port":"auto","default":false}}"#)],
            calls: Mutex::new(Vec::new()),
        };
        let none = |_: &Path| None;
        assert_eq!(l.current_device(&json, &none).unwrap().driver, "usbhid-ups");
        let saved = |p: &Path| (p == Path::new(DRIVER_CONF)).then(|| "UPS_DRIVER=blazer_usb\n".to_string());
        assert_eq!(l.current_device(&old, &saved).unwrap().driver, "blazer_usb");
        // dry runs: only the armac subdriver answers
        assert_eq!(l.dry_output(&l.probe_cmd(&d, Duration::from_secs(5))).unwrap().code, Some(0));
        assert_eq!(l.dry_output(&Cmd::new("upsc")), None);
    }
}
