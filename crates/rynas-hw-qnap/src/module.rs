//! The QNAP module (`rynas_module::Module`). `detect` registers it on a machine whose firmware
//! strings and CPU match a model (nothing else is stable at the start of the boot). At the
//! start of rynasd it waits a bounded time for the rest of the signature (NICs may come late),
//! loads the hwmon driver, has the chip confirmed by it, and only then loads the other drivers,
//! writes `/etc/modules-load.d/rynas-qnap.conf` and starts the controller. Until then, and for
//! good when something does not match, its routes answer 404, nothing is shown and nothing is
//! driven. The file is removed when the machine no longer matches.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::storage::HealthVerdict;
use rynas_config::{Applier, Config};
use rynas_module::Module;
use rynas_notify::{EventState, NewEvent, Notifier, NotifyCategory as Cat, NotifySeverity as Sev};

use crate::api::{ButtonEvent, PowerRecovery, PowerRecoveryState};
use crate::beep::{PcSpeaker, SilentSpeaker, Speaker};
use crate::control::{self, ConfirmError, Controller};
use crate::disks;
use crate::domain::{self, Cell, QnapDomain, Shared};
use crate::gpio::{CdevGpio, ChipInfo, Direction, Gpio, MemoryGpio};
use crate::model::{self, Chip, Model};

pub const MODULES_LOAD: &str = "/etc/modules-load.d/rynas-qnap.conf";
/// Marks the file as the module's.
const MODULES_HEADER: &str = "# managed by rynas (QNAP module)";
/// The driver that names the chip: loaded first, alone.
const CHIP_DRIVER: &str = "f71882fg";
/// The drivers loaded once the chip is confirmed.
const OTHER_DRIVERS: [&str; 2] = ["gpio-f7188x", "pcspkr"];
/// The worker the Super I/O access runs in.
const RYNASCTL: &str = "/usr/libexec/rynas/rynasctl";
const WAIT_SIGNATURE: Duration = Duration::from_secs(120);
const WAIT_STEP: Duration = Duration::from_secs(5);
const DEMO_DISK_PERIOD: Duration = Duration::from_secs(60);

/// What reaches the Super I/O: never at start, only on an explicit request.
pub trait Runner: Send + Sync {
    fn read(&self) -> Result<PowerRecoveryState, String>;
    fn set(&self, mode: PowerRecovery) -> Result<PowerRecoveryState, String>;
    fn activate_gpio(&self) -> Result<PowerRecoveryState, String>;
}

/// The worker outside rynasd's sandbox, with the polling stopped while it runs.
struct RealRunner(Cell);

impl RealRunner {
    fn worker(&self, word: &str) -> Result<PowerRecoveryState, String> {
        let ctl = self.0.ctl.get().ok_or("not confirmed")?;
        let out = ctl.pause_for_worker(|| {
            rynas_tasks::Outside::new("rynas-qnap-sio", "rynas: QNAP Super I/O", Duration::from_secs(20))
                .cmd([RYNASCTL, "qnap", "power-recovery", word])
                .run_blocking()
        });
        let out = out.map_err(|e| e.to_string())?;
        serde_json::from_str(out.stdout.trim()).map_err(|e| format!("worker: {e}"))
    }
}

impl Runner for RealRunner {
    fn read(&self) -> Result<PowerRecoveryState, String> {
        self.worker("read")
    }

    fn set(&self, mode: PowerRecovery) -> Result<PowerRecoveryState, String> {
        self.worker(match mode {
            PowerRecovery::LastState => "last_state",
            PowerRecovery::PowerOn => "power_on",
            PowerRecovery::PowerOff => "power_off",
        })
    }

    fn activate_gpio(&self) -> Result<PowerRecoveryState, String> {
        self.worker("activate-gpio")
    }
}

/// `rynasd --dry-system`: registers in memory.
#[derive(Default)]
struct DemoRunner(Mutex<(Option<PowerRecovery>, bool)>);

impl Runner for DemoRunner {
    fn read(&self) -> Result<PowerRecoveryState, String> {
        let g = self.0.lock().unwrap();
        let mode = g.0.unwrap_or(PowerRecovery::LastState);
        Ok(PowerRecoveryState { mode: Some(mode), sio_bits: 0, cmos: 0x21, eup: false, gpio_active: g.1 })
    }

    fn set(&self, mode: PowerRecovery) -> Result<PowerRecoveryState, String> {
        self.0.lock().unwrap().0 = Some(mode);
        self.read()
    }

    fn activate_gpio(&self) -> Result<PowerRecoveryState, String> {
        self.0.lock().unwrap().1 = true;
        self.read()
    }
}

pub struct QnapModule {
    model: &'static Model,
    root: PathBuf,
    hw_root: PathBuf,
    dry: bool,
    shared: Cell,
    runner: Arc<dyn Runner>,
    notifier: Arc<Mutex<Option<Arc<dyn Notifier>>>>,
}

fn module_file(root: &Path) -> PathBuf {
    rynas_config::files::under_root(root, Path::new(MODULES_LOAD))
}

/// Removes the driver list if it is the module's (an admin's file of that name stays).
pub fn remove_modules_load(root: &Path) -> bool {
    let path = module_file(root);
    if std::fs::read_to_string(&path).is_ok_and(|t| t.starts_with(MODULES_HEADER)) {
        return std::fs::remove_file(&path).is_ok();
    }
    false
}

impl QnapModule {
    /// The module on a machine with a model's firmware strings and CPU; elsewhere `None`, and
    /// a driver list left by disks that came from a QNAP is removed.
    pub fn detect(hw_root: &Path, root: &Path, dry: bool) -> Option<Self> {
        let Some(model) = model::plausible(hw_root) else {
            remove_modules_load(root);
            return None;
        };
        let shared: Cell = Arc::new(Shared::default());
        let runner: Arc<dyn Runner> =
            if dry { Arc::new(DemoRunner::default()) } else { Arc::new(RealRunner(shared.clone())) };
        Some(Self {
            model,
            root: root.to_path_buf(),
            hw_root: hw_root.to_path_buf(),
            dry,
            shared,
            runner,
            notifier: Arc::default(),
        })
    }

    /// The driver list below `root`; written when it differs.
    pub fn write_modules_load(root: &Path) -> Result<bool, String> {
        let path = module_file(root);
        let mut drivers = vec![CHIP_DRIVER];
        drivers.extend(OTHER_DRIVERS);
        let text = format!("{MODULES_HEADER}\n{}\n", drivers.join("\n"));
        if std::fs::read_to_string(&path).is_ok_and(|t| t == text) {
            return Ok(false);
        }
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        std::fs::write(&path, text).map_err(|e| format!("{MODULES_LOAD}: {e}"))?;
        Ok(true)
    }

    fn modprobe(&self, drivers: &[&str]) {
        if self.dry {
            return;
        }
        let mut argv = vec!["modprobe", "-a"];
        argv.extend(drivers);
        let r = rynas_tasks::Outside::new("rynas-qnap-modprobe", "rynas: QNAP hardware drivers", Duration::from_secs(30))
            .cmd(argv)
            .output_blocking();
        if let Err(e) = r {
            tracing::warn!("qnap: modprobe: {e}");
        }
    }

    /// The whole signature, within the bounded wait.
    fn wait_for_signature(&self) -> bool {
        let mut waited = Duration::ZERO;
        loop {
            if model::identify(&self.hw_root).is_some() {
                return true;
            }
            if waited >= WAIT_SIGNATURE {
                return false;
            }
            std::thread::sleep(WAIT_STEP);
            waited += WAIT_STEP;
        }
    }

    fn bring_up(&self, state_dir: PathBuf) {
        // first of all: a previous rynasd may have died in the middle of a tone, and the wait
        // below can be long (nothing happens unless a PC speaker device exists)
        if !self.dry {
            PcSpeaker::new(&self.hw_root).stop();
        }
        if !self.dry && !self.wait_for_signature() {
            tracing::info!("qnap: the full signature of {} does not match: the module stays out", self.model.display);
            remove_modules_load(&self.root);
            return;
        }
        if self.dry && model::identify(&self.hw_root).is_none() {
            return;
        }
        self.modprobe(&[CHIP_DRIVER]);
        if !self.dry {
            std::thread::sleep(Duration::from_secs(1));
        }
        match model::chip(&self.hw_root, self.model) {
            Chip::Confirmed => {}
            other => {
                tracing::info!("qnap: chip {other:?}: the module stays out");
                remove_modules_load(&self.root);
                return;
            }
        }
        self.modprobe(&OTHER_DRIVERS);
        let mut gpio: Box<dyn Gpio> = if self.dry {
            Box::new(demo_gpio(self.model))
        } else {
            Box::new(CdevGpio::default())
        };
        let confirmed = match control::confirm(&self.hw_root, self.model, gpio.as_mut()) {
            Ok(c) => c,
            Err(e) => {
                tracing::info!("qnap: {} not confirmed ({e:?}): the module stays out", self.model.display);
                if e != ConfirmError::GpioUnreadable {
                    remove_modules_load(&self.root);
                }
                return;
            }
        };
        match Self::write_modules_load(&self.root) {
            Ok(true) => tracing::info!("qnap: wrote the driver list for the next boots"),
            Ok(false) => {}
            Err(e) => tracing::warn!("qnap: {e}"),
        }
        let speaker: Arc<dyn Speaker> =
            if self.dry { Arc::new(SilentSpeaker) } else { Arc::new(PcSpeaker::new(&self.hw_root)) };
        let lock = (!self.dry).then(|| PathBuf::from(crate::superio::POLL_LOCK));
        let ctl = Arc::new(Controller::new(
            confirmed,
            &self.hw_root,
            gpio,
            speaker,
            Box::new(self.notifier_fn()),
            Some(state_dir),
            lock,
        ));
        ctl.apply(self.shared.latest.lock().unwrap().clone());
        let _ = self.shared.ctl.set(ctl.clone());
        if self.dry {
            let (root, m, c) = (self.hw_root.clone(), self.model, ctl.clone());
            let demo = move || {
                loop {
                    c.set_bays(disks::bay_states(&root, m, &demo_health(&root)));
                    std::thread::sleep(DEMO_DISK_PERIOD);
                }
            };
            if let Err(e) = std::thread::Builder::new().name("qnap-disks".into()).stack_size(128 << 10).spawn(demo) {
                tracing::warn!("qnap: disk thread: {e}");
            }
        }
        control::spawn(ctl);
    }

    /// Button events go to the notification engine through one small queue, so that the
    /// polling thread never waits for it.
    fn notifier_fn(&self) -> impl Fn(&ButtonEvent) + Send + Sync + 'static {
        let (tx, rx) = sync_channel::<ButtonEvent>(8);
        let notifier = self.notifier.clone();
        let sender = move || {
            for e in rx {
                let n = notifier.lock().unwrap().clone();
                if let Some(n) = n {
                    n.emit(button_event(&e));
                }
            }
        };
        if let Err(e) = std::thread::Builder::new().name("qnap-notify".into()).stack_size(128 << 10).spawn(sender) {
            tracing::warn!("qnap: notify thread: {e}");
        }
        let tx: SyncSender<ButtonEvent> = tx;
        move |e: &ButtonEvent| {
            let _ = tx.try_send(e.clone());
        }
    }
}

fn button_event(e: &ButtonEvent) -> NewEvent {
    let title = if e.button == "reset" { "Reset button pressed" } else { "USB copy button pressed" };
    let mut fields = BTreeMap::new();
    fields.insert("held_ms".to_string(), e.held_ms.to_string());
    NewEvent {
        source: format!("qnap:{}", e.button),
        category: Cat::Hardware,
        severity: Sev::Notice,
        key: Some(format!("qnap:button:{}", e.button)),
        state: EventState::Oneshot,
        title: title.into(),
        body: String::new(),
        fields,
        actions: Vec::new(),
        code: format!("qnap.button_{}", e.button),
        ..NewEvent::default()
    }
}

impl Module for QnapModule {
    fn id(&self) -> &'static str {
        "qnap"
    }

    fn summary(&self) -> String {
        format!("qnap: candidate {}", self.model.display)
    }

    fn system_disk_usb_ids(&self) -> &'static [(&'static str, &'static str)] {
        self.model.boot_flash_usb
    }

    /// Folders data copied from this machine's former system carries: thumbnails, media
    /// indexes, its recycle bin. Like every extension point here, only on the hardware: the
    /// module exists only when [`QnapModule::detect`] found it.
    fn dedup_excludes(&self) -> &'static [&'static str] {
        &[".@__thumb/", ".@__qini/", "@Recycle/"]
    }

    fn domains(&self) -> Vec<Arc<dyn Applier>> {
        vec![Arc::new(QnapDomain(self.shared.clone()))]
    }

    fn start(&self, cfg: &Config, _log: &dyn Fn(&str)) {
        *self.shared.latest.lock().unwrap() = domain::current(cfg);
        let state_dir = cfg.options().state_dir.clone();
        let me = Self {
            model: self.model,
            root: self.root.clone(),
            hw_root: self.hw_root.clone(),
            dry: self.dry,
            shared: self.shared.clone(),
            runner: self.runner.clone(),
            notifier: self.notifier.clone(),
        };
        let spawned = std::thread::Builder::new().name("qnap-start".into()).stack_size(256 << 10).spawn(move || me.bring_up(state_dir));
        if let Err(e) = spawned {
            tracing::warn!("qnap: start thread: {e}");
        }
    }

    fn attach(&self, notifier: Arc<dyn Notifier>) {
        *self.notifier.lock().unwrap() = Some(notifier);
    }

    fn storage_update(&self, health: &[(String, HealthVerdict)]) {
        if self.dry {
            return;
        }
        if let Some(ctl) = self.shared.ctl.get() {
            ctl.set_bays(disks::bay_states(&self.hw_root, self.model, health));
        }
    }

    fn stop(&self) {
        if let Some(ctl) = self.shared.ctl.get() {
            ctl.shutdown();
        }
    }

    fn router(&self, ctx: &rynas_module::http::Ctx) -> Option<axum::Router> {
        Some(crate::http::router(ctx.clone(), self.shared.clone(), self.runner.clone()))
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
    outputs.extend(m.brightness);
    outputs.extend(m.bays.iter().map(|b| b.err_led));
    for l in outputs {
        g.set_direction(&format!("gpio-f7188x-{}", l.bank), l.offset, Direction::Output);
    }
    g
}

/// Dry instance: the health of the fake disks, `<root>/run/qnap-fake-disks` lines `sda failing`.
fn demo_health(root: &Path) -> Vec<(String, HealthVerdict)> {
    model::read(root, "/run/qnap-fake-disks")
        .lines()
        .filter_map(|l| {
            let (d, h) = l.split_once(' ')?;
            let h = match h.trim() {
                "failing" => HealthVerdict::Failing,
                "warning" => HealthVerdict::Warning,
                "healthy" => HealthVerdict::Healthy,
                _ => HealthVerdict::Unknown,
            };
            Some((d.to_string(), h))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::testing::fake_hs251;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Counting(AtomicU32);

    impl Runner for Counting {
        fn read(&self) -> Result<PowerRecoveryState, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PowerRecoveryState::default())
        }
        fn set(&self, _mode: PowerRecovery) -> Result<PowerRecoveryState, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PowerRecoveryState::default())
        }
        fn activate_gpio(&self) -> Result<PowerRecoveryState, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(PowerRecoveryState::default())
        }
    }

    fn config(state: &Path, root: &Path, domains: Vec<Arc<dyn Applier>>) -> Config {
        Config::open(
            rynas_config::ConfigOptions::new(state, root),
            Arc::new(rynas_config::MemorySystem::new("nas")),
            domains,
        )
        .unwrap()
    }

    #[test]
    fn nothing_on_other_hardware() {
        let d = tempfile::tempdir().unwrap();
        assert!(QnapModule::detect(d.path(), d.path(), false).is_none());
        std::fs::create_dir_all(d.path().join("sys/class/dmi/id")).unwrap();
        std::fs::write(d.path().join("sys/class/dmi/id/sys_vendor"), "Synology Inc.\n").unwrap();
        assert!(QnapModule::detect(d.path(), d.path(), false).is_none());
    }

    #[test]
    fn a_driver_list_left_on_other_hardware_is_removed() {
        let hw = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        QnapModule::write_modules_load(etc.path()).unwrap();
        assert!(QnapModule::detect(hw.path(), etc.path(), false).is_none());
        assert!(!etc.path().join("etc/modules-load.d/rynas-qnap.conf").exists());
        // an admin's file of that name stays
        let p = etc.path().join("etc/modules-load.d/rynas-qnap.conf");
        std::fs::write(&p, "mine\n").unwrap();
        assert!(!remove_modules_load(etc.path()));
        assert!(p.exists());
    }

    fn started(hw: &Path, etc: &Path, st: &Path) -> (QnapModule, Config) {
        let m = QnapModule::detect(hw, etc, true).unwrap();
        let cfg = config(st, etc, m.domains());
        m.start(&cfg, &|_| {});
        (m, cfg)
    }

    fn wait_for(f: impl Fn() -> bool) -> bool {
        for _ in 0..500 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn dry_start_on_the_model() {
        let hw = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        let st = tempfile::tempdir().unwrap();
        fake_hs251(hw.path(), Some("f71869a"));
        let (m, _cfg) = started(hw.path(), etc.path(), st.path());
        assert_eq!(m.id(), "qnap");
        assert!(wait_for(|| m.shared.ctl.get().is_some()));
        let t = std::fs::read_to_string(etc.path().join("etc/modules-load.d/rynas-qnap.conf")).unwrap();
        assert!(t.contains("gpio-f7188x\n") && t.contains("f71882fg\n") && t.contains("pcspkr"));
        assert!(!QnapModule::write_modules_load(etc.path()).unwrap(), "unchanged");
    }

    #[test]
    fn no_driver_list_while_the_chip_is_not_confirmed() {
        let hw = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        let st = tempfile::tempdir().unwrap();
        fake_hs251(hw.path(), None); // the chip's driver names nothing
        let (m, _cfg) = started(hw.path(), etc.path(), st.path());
        std::thread::sleep(Duration::from_millis(100));
        assert!(m.shared.ctl.get().is_none());
        assert!(!etc.path().join("etc/modules-load.d/rynas-qnap.conf").exists());
    }

    #[test]
    fn plausible_is_not_enough() {
        let hw = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        let st = tempfile::tempdir().unwrap();
        fake_hs251(hw.path(), Some("f71869a"));
        std::fs::create_dir_all(hw.path().join("sys/bus/pci/devices/0000:05:00.0")).unwrap();
        let (m, _cfg) = started(hw.path(), etc.path(), st.path());
        std::thread::sleep(Duration::from_millis(100));
        assert!(m.shared.ctl.get().is_none(), "one PCI device more: not this model");
    }

    #[test]
    fn the_super_io_is_never_reached_at_start() {
        let hw = tempfile::tempdir().unwrap();
        let etc = tempfile::tempdir().unwrap();
        let st = tempfile::tempdir().unwrap();
        fake_hs251(hw.path(), Some("f71869a"));
        let counting = Arc::new(Counting(AtomicU32::new(0)));
        let mut m = QnapModule::detect(hw.path(), etc.path(), true).unwrap();
        m.runner = counting.clone();
        let cfg = config(st.path(), etc.path(), m.domains());
        m.start(&cfg, &|_| {});
        assert!(wait_for(|| m.shared.ctl.get().is_some()));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(counting.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn button_event_is_a_notification() {
        let e = button_event(&ButtonEvent { button: "reset".into(), at: 1, held_ms: 320 });
        assert_eq!(e.code, "qnap.button_reset");
        assert_eq!(e.fields["held_ms"], "320");
    }
}
