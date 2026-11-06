# Module boundary audit — 2026-11-06

Rule (Marco, D-MODULE in `../phase0/05-decisions.md`): everything Synology lives only in the
Synology module; other hardware modules will come; without a module everything works and
nothing points to non-standard things. Users without Synology hardware have no
`synodeb-ups status --json`, no `/etc/synodeb`, no synohwd.

Scope: every reference to Synology, synodeb, synohwd, Safe Mode or DSM outside the module
(`crates/rynas-hw-synology`, `crates/rynas-ui/src/modules/synology`,
`packaging/modules/synology`), at `main` 8cccbf9. Verdicts: **moved** (done in this change),
**moved (follow-up)** (a "move later" item of the first pass, done in the follow-up change of
the same day), **keep-generic** (a generic mechanism a module plugs into, or standard
behaviour), **ok-as-is** (comments, tests, wire contract, demo data; reason given). Nothing is
left as "move later".

## Extension points (what the core offers a module)

| Point | Core side | Synology module |
|---|---|---|
| `rynas_module::Module` | `rynasd` main: `detect_modules`, `Modules` registry | `rynas_hw_synology::module::SynologyModule::detect(hw_root, root)`: Synology hardware (DMI, platform driver) **or** synodeb's own files (`/etc/synodeb/ups.conf`, `notify.conf`; not the directory) |
| `UpsLayer` (UPS executor) | `rynasd::ups`, `rynasd::ups_setup`: NUT (rynas blocks in `/etc/nut`, own policy file, upsd watcher) when no layer; wizard orchestration, `ups.safemode`/`ups.device` keys, `ups.test` task, `stop_work` always core | `rynas_hw_synology::ups::SynodebUps`: `synodeb-ups probe/setup/status --json/test`, `/etc/synodeb/ups.conf` block, Safe Mode notice, `--dry-system` answers of its tool |
| `EventForward` | `/run/rynas/notify.sock` opened only when a module provides one | `rynas_hw_synology::notify::Forward` (synodeb's `notify`) |
| `notify_import` | `Engine::import_once(ImportedSettings)` (meta key, once) | `notify.conf` → targets + "Imported from synodeb" rule (key `synodeb_import` kept) |
| `system_disk_usb_ids` | `rynas_removable::Scanner::with_system_usb_ids`; the core reads only `UDISKS_IGNORE`/`UDISKS_SYSTEM` | DOM `f400:f400` (only on Synology hardware) |
| `domains` | configuration store | `synology.hw` (only on Synology hardware) |
| `start` | once at rynasd start (blocking, with the configuration store) | links `/etc/synodeb/ups-pre-safemode.d/50-rynas` → `/usr/lib/rynas/modules/synology/ups-pre-safemode` where synodeb runs the UPS; adopts `/etc/synohwd.toml` into `synology.hw` (`domain::adopt`) |
| `router` (`rynas_module::http`) | rynasd nests each module's router under `/v1/modules/<id>`; rynas-web forwards `/api/v1/modules/{id}/{*rest}` by prefix (no module named); shared helpers `http::{Error, user, docs, stage, blocking}`, `Ctx` {config, hw_root, state_dir, ack_hardware} | `http.rs`: status, settings, beep, mute, fan curves (404 without the hardware) |
| `hardware_control` | provider-neutral `POST /v1/hardware/bays/{n}/identify` (404 without one) | the `Synology` provider (`HardwareControl`) |
| `boot` (`BootLayer` over `rynas_btrfs::rootfs::BootSync`) | kernel card (`KernelStatus.boot_layer`, `boot: BootImage`); root rollback's boot half, refused with `no_boot_layer` without one | `boot::SynodebDom`: `synodeb-dom status --json`, `sync --root`; dry system: fixture, logged sync |
| `rescue_feedback` (`rynas_net::rescue::{Feedback, Signal}`) | `rynas_net::Engine::with_feedback` (rynasd and `rynasctl net`); silent by default | `module::Beeps`: `2`/`3`/`;` through synohwd's FIFO |
| `reset_button` | `RescueInfo.reset_button: Option<ResetButton {module, configured, pending}>` | reflection of `synology.hw` `reset_button: network_rescue` |
| `wake_clock` (`WakeClock`) | `rynas_power::wake::find_rtc(root, prefer)` (rynasd's power API, `rynasctl power arm-wake`), `PowerDomain.alarm_reader` (`After=` of `rynas-power-wake.service`) | S-35390A (`-0030`), read by `synohwd.service` |
| web provider (`rynas_web::hardware::detect`) | `--hw-root`; `HardwareProvider::thermal_limits` for the hardware watch | `Synology::web_provider` (its hardware or synohwd's status file) with the model's shutdown thresholds |
| UI module folder | `crate::modules::{ups_key(UpsSlot), source_label}`, a second dictionary table read by `crate::i18n` (one mechanism) and by rynas-notify's build script | `modules/synology/{mod,page,i18n}.rs`: tab + presence, UPS texts, synodeb event texts, reset button status |

## Daemon (rynasd and its crates)

| File | What | Verdict |
|---|---|---|
| `rynasd/src/ups.rs` | `synodeb: bool`, `UPS_CONF`, `SYNODEB_UPS`, synodeb block render/parse, `synodeb-ups test`, Safe Mode notice | **moved** → `UpsLayer` (`rynas-hw-synology/src/ups.rs`); core = NUT only |
| `rynasd/src/ups_setup.rs` | `SynodebBackend`, `SYNODEB_DRIVER_CONF`, status JSON / driver file parsers, synodeb DryRunner answers, too-old messages | **moved** → `UpsLayer`; core keeps `LayerBackend` (generic) |
| `rynas-nut/src/setup.rs` | `synodeb_args` | **moved** → `rynas_hw_synology::ups::driver_args` (core exposes `option_arg`) |
| `rynasd/src/main.rs` | Synology detect, `ups_synodeb`, `--synodeb-notify-conf`, socket always on | **moved** → `detect_modules` + `Modules`; `--synodeb-notify-conf` removed (read below the configuration root by the module); socket only with a module forward |
| `rynas-notify/src/synodeb.rs` | forward parser, `notify.conf` import | **moved** → `rynas-hw-synology/src/notify.rs`; engine keeps generic `ImportedSettings` |
| `rynas-notify/src/engine/mod.rs` | `synodeb_conf`, `import_synodeb` | **moved** → `imports: Vec<ImportedSettings>`, `import_once` |
| `rynasd/src/notifications.rs` | `socket_router` parsed synodeb bodies | **moved** → `socket_router(engine, Arc<dyn EventForward>)` |
| `rynas-removable/src/scan.rs` | `SYNODEB_BOOT_FLASH` prop, `f400:f400` table | **moved**: the prop is dropped (synodeb's udev rule `60-synodeb.rules` sets `UDISKS_IGNORE=1` on the same device, the standard property the core reads); the ids come from the module |
| `packaging/synodeb/ups-pre-safemode.d/50-rynas` + deb asset in `/etc/synodeb` | conffile created `/etc/synodeb` on every system | **moved** → `/usr/lib/rynas/modules/synology/ups-pre-safemode`, linked by the module at start; `dpkg-maintscript-helper rm_conffile` (preinst/postinst/postrm) removes the old conffile, postrm the link |
| `rynas-api-types/src/ups.rs` | `UpsBackend::Synodeb`, `UpsSetupBlock::SynodebTooOld`, synodeb docs | **moved** → `UpsBackend::Module`, `UpsSetupBlock::ModuleTooOld` (wire `module`, `module_too_old`) |
| `rynasd/src/hardware.rs` | `/v1/hardware/synology/*` glue, `adopt_synohwd_conf` | **moved (follow-up)** → `rynas-hw-synology/src/http.rs` (module router under `/v1/modules/synology`, `rynas_module::http` helpers), `domain::adopt` from `Module::start`; the core keeps the power state, the provider-neutral identify and `ack_hardware_alerts` |
| `rynasd/src/network.rs` | reads `synology.hw` for the reset button reflection, `RescueInfo.synology` | **moved (follow-up)** → `Module::reset_button`; wire `RescueInfo.reset_button: Option<ResetButton>` |
| `rynas-net/src/rescue.rs`, `txn.rs` | synohwd FIFO beeps/LED on rescue, `rynasctl net reset-button` | **moved (follow-up)** → `rescue::Feedback` (`Signal::{PressAgain, Restoring, RescueApplied}`), the Synology module's `Beeps`; `rynasctl` detects the modules (`rynasd::modules::detect`) |
| `rynasd/src/updates.rs`, `rynas-updates/src/dom.rs`, `api-types/src/updates.rs` | `synodeb-dom status --json`, kernel/DOM card, `synodeb` flag | **moved (follow-up)** → `BootLayer::status` (`rynas-hw-synology/src/boot.rs`, parser and fixture); `rynas-updates` keeps `kernels::installed_kernels`; wire `KernelStatus.boot_layer`, `boot: BootImage`, `BootKernel`, `KernelTrial` |
| `rynasd/src/snapshots.rs`, `protection.rs`, `rynas-btrfs/src/rootfs.rs`, `api-types/src/snapshots.rs` | root rollback through `synodeb-dom sync`, `RollbackRefusal::NotSynodeb` | **moved (follow-up)** → `rootfs::BootSync` (`rollback_root(ops, boot, …)`), `Protection::with_boot`; `RollbackRefusal::NoBootLayer` (wire `no_boot_layer`) |
| `rynas-power/src/wake.rs`, `domain.rs` | S-35390A RTC preference, `After=synohwd.service` | **moved (follow-up)** → `Module::wake_clock`: `find_rtc(root, prefer)`, `PowerDomain.alarm_reader` (no module: `rtc0`, no extra `After=`) |
| `rynas-updates/src/domain.rs` | reads synodeb's `52synodeb-unattended-upgrades` among other `apt.conf.d` files | **keep-generic**: any file under `apt.conf.d` is read; the name appears in comments and tests only |
| `rynas-notify/src/checks.rs`, `rynas-storage/src/overview.rs` | "synohwd" in comments; provider data age | **ok-as-is**: generic `HardwareStatus` |
| `rynas-net/src/networkd.rs`, `ifupdown.rs`, `rynas-storage/src/domain.rs` | comments about the synodeb image's files | **ok-as-is**: generic behaviour, comments |
| `rynasd/tests/notifications.rs` | socket test with the Synology forward | **ok-as-is**: integration test of the module through the core socket |
| `packaging/test-synology.sh`, `tools/ui-audit/start.sh` (`FAKE_UPS_BACKEND=synodeb`) | Synology test scripts | **ok-as-is**: tests of the module; `--synodeb-notify-conf` dropped from `start.sh` |

## rynas-web

| File | What | Verdict |
|---|---|---|
| `src/hardware/synohwd.rs`, `main.rs --synohwd-status` | the only `HardwareProvider`, built on every machine | **moved (follow-up)** → `hardware::detect(--hw-root)`: the module's `Synology::web_provider`, else `NoProvider` (one line per module) |
| `src/notify.rs` | shutdown thresholds of the Synology model (`model::shutdown_c`) | **moved (follow-up)** → `HardwareProvider::thermal_limits` (Synology provider); no provider-name filter left |
| `src/proxy.rs` | `/hardware/synology/*` routes | **moved (follow-up)** → `/modules/{id}/{*rest}` forwarded by prefix, `/hardware/bays/{n}/identify` |
| `src/rynasd_client.rs`, `metrics/temps.rs` | comments | **ok-as-is** |

## Web interface (rynas-ui)

| File | What | Verdict |
|---|---|---|
| `pages/hardware/synology.rs` | the Synology tab | **moved** → `modules/synology/page.rs` |
| `pages/hardware/mod.rs` | `synology_present` probe, `hw.tab_synology` | **moved** → `modules::synology::{present, TAB}` |
| `i18n.rs` | `syn.*`, `nev.synodeb.*`, `nt.src.synodeb`, `cfg.key.synology.hw`, synodeb UPS/wizard texts, reset button texts | **moved** → `modules/synology/i18n.rs` (same `dict!` macro, second table, one lookup); keys renamed `syn.ups_*`, `syn.upsw_*`, `syn.reset_*`, `syn.tab`; shared chassis texts renamed `hw.*` and kept in the core. Test `core_names_no_module` keeps the core table free of platform names |
| `pages/hardware/ups.rs`, `ups_setup.rs` | `UpsBackend::Synodeb` texts | **moved** → `modules::ups_key(UpsSlot)` |
| `pages/notifications.rs` | `synodeb:` source label | **moved** → `modules::source_label` |
| `pages/network/tools.rs` | reset button status | **moved** → `modules::synology::reset_button` (section still gated by `RescueInfo.synology`) |
| `chassis.rs` | identify through `/api/v1/hardware/synology/bays/{n}/identify` | **moved (follow-up)** → `/api/v1/hardware/bays/{n}/identify`; the module page calls `/api/v1/modules/synology/*` |
| `pages/snapshots.rs` (`snap.root_step2`), `pages/updates.rs` (DOM card) | synodeb-dom texts and card | **moved (follow-up)**: neutral texts (`snap.root_step2` "the boot entry is pointed at that kernel", `upd.boot_*` "boot device"), shown only with a boot layer; `PENDING` of `core_names_no_module` removed |
| `pages/network/tools.rs` | reset button gated by `RescueInfo.synology` | **moved (follow-up)** → `RescueInfo.reset_button`, `modules::reset_button` dispatches on its `module` |
| `demo.rs`, `alerts.rs` | sample provider `synology` | **ok-as-is**: demo of a Synology |
| `files_check.rs` | forbidden product names | **ok-as-is**: the check itself |
| `style/main.css` | comments | **ok-as-is** |
| page module comments naming another system ("DSM Control Panel › …") | code comments, not UI texts | **ok-as-is** (removed where touched) |

## Pragmatic exceptions (kept, documented)

- **Wire types** stay in `rynas-api-types` (one serde crate for daemon, web and wasm):
  `rynas_api_types::synology` is the module's namespace. The core types are generic
  (`RescueInfo.reset_button`, `RollbackRefusal::NoBootLayer`, `KernelStatus.boot_layer/boot`,
  `IdentifyRequest` at the crate root). `rynas_hw::HardwareControl` still names
  `synology::{BeepKind, FanCurves}` (the only provider's action types; they move with a second
  provider).
- **Detection hooks** are the module's own (`SynologyModule::detect`); the core calls them in
  `rynasd::modules::detect` (rynasd and `rynasctl`) and `rynas_web::hardware::detect`, one line
  per module each.
- **The Synology module is one crate** (`rynas-hw-synology`, daemon side behind its `domain`
  feature) plus the UI folder. Its routes run in rynasd's process through `rynas_module::http`
  (a plugin process later, D-PLUG).
- **Demo data** of the web interface (`--features demo`) is a Synology (sample provider
  `synology`, synodeb packages, the DOM in the kernel card): it is a demo of that hardware.

## Checks

`cargo test --workspace`, `cargo clippy --workspace --all-targets -D warnings`, wasm clippy with
and without `demo`, `trunk build --release` (brotli in the commit message of the UI change),
`cargo test --manifest-path crates/rynas-files-worker/Cargo.toml`. Follow-up: the same, the
i18n tests with `core_names_no_module` (no exception left).

## What to check on a Synology after the follow-up

- `/api/v1/modules/synology/{status,settings,fan-curves}`, beep, mute; identify from the
  chassis drawing (`/api/v1/hardware/bays/{n}/identify`); the old `/api/v1/hardware/synology/*`
  paths answer 404.
- rynasd's start log: `module synology: DS920+ (synohwd provider)`, `synohwd settings adopted`
  only when there was something to adopt.
- Updates › Kernel and boot: entries, default, fallback, kernels on the boot device, trial (the
  DOM through `synodeb-dom status --json`); Snapshots: a restore point preview is allowed (not
  `no_boot_layer`).
- Reset button double press: short beep + orange blink, then long beep and the factory
  network; Network › Rescue shows the reset button section.
- Power schedule: the RTC is the `-0030` one (`/api/v1/power/next` `wake.rtc`), the unit keeps
  `After=… synohwd.service` after the next apply of `power.schedule`.
- Dashboard thermal warnings near the shutdown thresholds still come (hardware watch).
