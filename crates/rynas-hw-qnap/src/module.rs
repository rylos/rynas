//! The QNAP module (`rynas_module::Module`). `detect` registers it only when the whole
//! signature of a model matches ([`crate::model::identify`]); nothing else of the module runs
//! elsewhere. At the start of rynasd the module writes `/etc/modules-load.d/rynas-qnap.conf`,
//! loads the drivers (outside the sandbox: `ProtectKernelModules`) and confirms the chip; a
//! chip that does not confirm leaves the module without a controller, so its routes answer 404.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rynas_config::{Applier, Config};
use rynas_module::{EventForward, Module};
use rynas_notify::{EventState, NewEvent, NotifyCategory as Cat, NotifySeverity as Sev};

use crate::api::ButtonEvent;
use crate::beep::{PcSpeaker, SilentSpeaker, Speaker};
use crate::control::{self, Controller};
use crate::disks::{self, DiskSource, SystemDisks};
use crate::domain::{self, Cell, QnapDomain};
use crate::gpio::{CdevGpio, ChipInfo, Direction, Gpio, MemoryGpio};
use crate::model::{self, Model};

pub const MODULES_LOAD: &str = "/etc/modules-load.d/rynas-qnap.conf";
pub const NOTIFY_SOCKET: &str = "/run/rynas/notify.sock";
/// The drivers of the model's hardware: the Super I/O GPIO banks, its sensors, the speaker.
pub const KERNEL_MODULES: [&str; 3] = ["gpio-f7188x", "f71882fg", "pcspkr"];
const DISK_PERIOD: Duration = Duration::from_secs(60);

pub struct QnapModule {
    model: &'static Model,
    root: PathBuf,
    hw_root: PathBuf,
    dry: bool,
    ctl: Cell,
}

impl QnapModule {
    /// The module on an identified machine; `None` everywhere else.
    pub fn detect(hw_root: &Path, root: &Path, dry: bool) -> Option<Self> {
        let model = model::identify(hw_root)?;
        Some(Self {
            model,
            root: root.to_path_buf(),
            hw_root: hw_root.to_path_buf(),
            dry,
            ctl: Arc::new(OnceLock::new()),
        })
    }

    /// `/etc/modules-load.d` below `root`: the file is the module's, written when it differs.
    pub fn write_modules_load(root: &Path) -> Result<bool, String> {
        let path = rynas_config::files::under_root(root, Path::new(MODULES_LOAD));
        let text = format!("# managed by rynas (QNAP module)\n{}\n", KERNEL_MODULES.join("\n"));
        if std::fs::read_to_string(&path).is_ok_and(|t| t == text) {
            return Ok(false);
        }
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{MODULES_LOAD}: {e}"))?;
        Ok(true)
    }

    fn load_drivers(&self, log: &dyn Fn(&str)) {
        if self.dry {
            return;
        }
        let mut argv = vec!["modprobe".to_string(), "-a".to_string()];
        argv.extend(KERNEL_MODULES.iter().map(|m| m.to_string()));
        let r = rynas_tasks::Outside::new("rynas-qnap-modprobe", "rynas: QNAP hardware drivers", Duration::from_secs(30))
            .cmd(&argv)
            .output_blocking();
        if let Err(e) = r {
            log(&format!("qnap: modprobe: {e}"));
        }
    }

    fn start_controller(&self, cfg: &Config, log: &dyn Fn(&str)) {
        let mut gpio: Box<dyn Gpio> = if self.dry {
            Box::new(demo_gpio(self.model))
        } else {
            Box::new(CdevGpio::default())
        };
        let confirmed = match control::confirm(&self.hw_root, self.model, gpio.as_mut()) {
            Ok(c) => c,
            Err(e) => {
                log(&format!("qnap: {} not confirmed ({e:?}): the module stays out", self.model.display));
                // another chip: the driver list is not for this machine
                if e != control::ConfirmError::ChipUnknown {
                    let _ = std::fs::remove_file(rynas_config::files::under_root(&self.root, Path::new(MODULES_LOAD)));
                }
                return;
            }
        };
        let speaker: Arc<dyn Speaker> =
            if self.dry { Arc::new(SilentSpeaker) } else { Arc::new(PcSpeaker::new(&self.hw_root)) };
        let notify = Box::new(forward_button);
        let ctl = Arc::new(Controller::new(confirmed, gpio, speaker, notify));
        ctl.apply(domain::current(cfg));
        let _ = self.ctl.set(ctl.clone());
        let mut source: Box<dyn DiskSource> =
            if self.dry { Box::new(DemoDisks(self.hw_root.clone())) } else { Box::new(SystemDisks::default()) };
        let (root, m) = (self.hw_root.clone(), self.model);
        let c = ctl.clone();
        let disk_watch = move || {
            loop {
                c.set_bays(disks::bay_states(&root, m, source.as_mut()));
                std::thread::sleep(DISK_PERIOD);
            }
        };
        if let Err(e) = std::thread::Builder::new().name("qnap-disks".into()).stack_size(256 << 10).spawn(disk_watch) {
            log(&format!("qnap: disk thread: {e}"));
        }
        control::spawn(ctl);
    }
}

impl Module for QnapModule {
    fn id(&self) -> &'static str {
        "qnap"
    }

    fn summary(&self) -> String {
        format!("qnap: {}", self.model.display)
    }

    fn domains(&self) -> Vec<Arc<dyn Applier>> {
        vec![Arc::new(QnapDomain(self.ctl.clone()))]
    }

    fn event_forward(&self) -> Option<Arc<dyn EventForward>> {
        Some(Arc::new(Forward))
    }

    fn start(&self, cfg: &Config, log: &dyn Fn(&str)) {
        match Self::write_modules_load(&self.root) {
            Ok(true) => log("qnap: wrote the driver list for the next boots"),
            Ok(false) => {}
            Err(e) => log(&format!("qnap: {e}")),
        }
        self.load_drivers(log);
        self.start_controller(cfg, log);
    }

    fn router(&self, ctx: &rynas_module::http::Ctx) -> Option<axum::Router> {
        Some(crate::http::router(ctx.clone(), self.ctl.clone()))
    }
}

/// `rynasd --dry-system`: the banks in memory (the host's GPIO is never opened).
fn demo_gpio(m: &Model) -> MemoryGpio {
    let chips = m
        .gpio_banks
        .iter()
        .enumerate()
        .map(|(i, n)| ChipInfo { label: format!("gpio-f7188x-{i}"), lines: *n })
        .collect();
    let g = MemoryGpio::new(chips);
    let mut outputs = vec![m.status_green, m.status_red, m.usb_led];
    outputs.extend(m.bays.iter().map(|b| b.err_led));
    for l in outputs {
        g.set_direction(&format!("gpio-f7188x-{}", l.bank), l.offset, Direction::Output);
    }
    g
}

/// Dry instance: the health of the fake disks, `<root>/run/qnap-fake-disks` lines `sda failing`.
struct DemoDisks(PathBuf);

impl DiskSource for DemoDisks {
    fn health(&mut self) -> Vec<(String, rynas_api_types::storage::HealthVerdict)> {
        use rynas_api_types::storage::HealthVerdict as H;
        model::read(&self.0, "/run/qnap-fake-disks")
            .lines()
            .filter_map(|l| {
                let (d, h) = l.split_once(' ')?;
                let h = match h.trim() {
                    "failing" => H::Failing,
                    "warning" => H::Warning,
                    "healthy" => H::Healthy,
                    _ => H::Unknown,
                };
                Some((d.to_string(), h))
            })
            .collect()
    }
}

/// A button event through the local socket (the core's notification engine, D-MODULE). A
/// stopped engine or closed socket only loses the notification, the event stays listed.
fn forward_button(e: &ButtonEvent) {
    let body = format!("{{\"button\":\"{}\",\"held_ms\":{}}}", e.button, e.held_ms);
    let send = || -> std::io::Result<()> {
        let mut s = UnixStream::connect(NOTIFY_SOCKET)?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.set_write_timeout(Some(Duration::from_secs(5)))?;
        write!(
            s,
            "POST /v1/notify HTTP/1.1\r\nHost: rynasd\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        let mut sink = [0u8; 256];
        let _ = s.read(&mut sink)?;
        Ok(())
    };
    if let Err(err) = send() {
        tracing::debug!("qnap: notification not sent: {err}");
    }
}

/// The button events the controller posts.
pub struct Forward;

impl EventForward for Forward {
    fn max_body(&self) -> usize {
        256
    }

    fn parse(&self, _content_type: Option<&str>, body: &[u8]) -> Result<(NewEvent, bool), String> {
        #[derive(serde::Deserialize)]
        struct Body {
            button: String,
            held_ms: u32,
        }
        let b: Body = serde_json::from_slice(body).map_err(|e| format!("JSON: {e}"))?;
        let title = match b.button.as_str() {
            "reset" => "Reset button pressed",
            "usb_copy" => "USB copy button pressed",
            other => return Err(format!("unknown button {other}")),
        };
        let code = format!("qnap.button_{}", b.button);
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("held_ms".to_string(), b.held_ms.to_string());
        Ok((
            NewEvent {
                source: format!("qnap:{}", b.button),
                category: Cat::Hardware,
                severity: Sev::Notice,
                key: Some(format!("qnap:button:{}", b.button)),
                state: EventState::Oneshot,
                title: title.into(),
                body: String::new(),
                fields,
                actions: Vec::new(),
                code,
                ..NewEvent::default()
            },
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::testing::fake_hs251;

    #[test]
    fn nothing_on_other_hardware() {
        let d = tempfile::tempdir().unwrap();
        assert!(QnapModule::detect(d.path(), d.path(), false).is_none());
        std::fs::create_dir_all(d.path().join("sys/class/dmi/id")).unwrap();
        std::fs::write(d.path().join("sys/class/dmi/id/sys_vendor"), "Synology Inc.\n").unwrap();
        assert!(QnapModule::detect(d.path(), d.path(), false).is_none());
    }

    #[test]
    fn dry_start_on_the_model() {
        let hw = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        fake_hs251(hw.path(), Some("f71869a"));
        let m = QnapModule::detect(hw.path(), etc.path(), true).unwrap();
        assert_eq!(m.id(), "qnap");
        assert!(QnapModule::write_modules_load(etc.path()).unwrap());
        assert!(!QnapModule::write_modules_load(etc.path()).unwrap(), "unchanged");
        let t = std::fs::read_to_string(etc.path().join("etc/modules-load.d/rynas-qnap.conf")).unwrap();
        assert!(t.contains("gpio-f7188x\n") && t.contains("f71882fg\n") && t.contains("pcspkr"));
    }

    #[test]
    fn button_event_parsed() {
        let (e, force) = Forward.parse(None, br#"{"button":"reset","held_ms":320}"#).unwrap();
        assert_eq!(e.code, "qnap.button_reset");
        assert_eq!(e.fields["held_ms"], "320");
        assert!(!force);
        assert!(Forward.parse(None, br#"{"button":"rm","held_ms":1}"#).is_err());
    }
}
