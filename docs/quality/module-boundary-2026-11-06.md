# Module boundary audit — 2026-11-06

Rule (Marco, D-MODULE in `../phase0/05-decisions.md`): everything Synology lives only in the
Synology module; other hardware modules will come; without a module everything works and
nothing points to non-standard things. Users without Synology hardware have no
`synodeb-ups status --json`, no `/etc/synodeb`, no synohwd.

Scope: every reference to Synology, synodeb, synohwd, Safe Mode or DSM outside the module
(`crates/rynas-hw-synology`, `crates/rynas-ui/src/modules/synology`,
`packaging/modules/synology`), at `main` 8cccbf9. Verdicts: **moved** (done in this change),
**move later** (still in the core, with the planned extension point), **keep-generic** (a
generic mechanism a module plugs into, or standard behaviour), **ok-as-is** (comments, tests,
wire contract, demo data; reason given).

## Extension points (what the core offers a module)

| Point | Core side | Synology module |
|---|---|---|
| `rynas_module::Module` | `rynasd` main: `detect_modules`, `Modules` registry | `rynas_hw_synology::module::SynologyModule::detect(hw_root, root)`: Synology hardware (DMI, platform driver) **or** synodeb's own files (`/etc/synodeb/ups.conf`, `notify.conf`; not the directory) |
| `UpsLayer` (UPS executor) | `rynasd::ups`, `rynasd::ups_setup`: NUT (rynas blocks in `/etc/nut`, own policy file, upsd watcher) when no layer; wizard orchestration, `ups.safemode`/`ups.device` keys, `ups.test` task, `stop_work` always core | `rynas_hw_synology::ups::SynodebUps`: `synodeb-ups probe/setup/status --json/test`, `/etc/synodeb/ups.conf` block, Safe Mode notice, `--dry-system` answers of its tool |
| `EventForward` | `/run/rynas/notify.sock` opened only when a module provides one | `rynas_hw_synology::notify::Forward` (synodeb's `notify`) |
| `notify_import` | `Engine::import_once(ImportedSettings)` (meta key, once) | `notify.conf` → targets + "Imported from synodeb" rule (key `synodeb_import` kept) |
| `system_disk_usb_ids` | `rynas_removable::Scanner::with_system_usb_ids`; the core reads only `UDISKS_IGNORE`/`UDISKS_SYSTEM` | DOM `f400:f400` (only on Synology hardware) |
| `domains` | configuration store | `synology.hw` (only on Synology hardware) |
| `start` | once at rynasd start (blocking) | links `/etc/synodeb/ups-pre-safemode.d/50-rynas` → `/usr/lib/rynas/modules/synology/ups-pre-safemode` where synodeb runs the UPS |
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
| `rynasd/src/hardware.rs` | `/v1/hardware/synology/*` glue, `adopt_synohwd_conf` | **move later**: module routers (needs `Ops`/`Error` in a shared crate, or the plugin process of D-PLUG); `adopt_synohwd_conf` → `Module::start` (needs the legacy `network.rescue` value). Mounted for every machine but answers 404 without the module |
| `rynasd/src/network.rs` | reads `synology.hw` for the reset button reflection, `RescueInfo.synology` | **move later**: a module "reset button state" point; today only filled when the key exists |
| `rynas-net/src/rescue.rs`, `txn.rs` | synohwd FIFO beeps/LED on rescue, `rynasctl net reset-button` | **move later**: module "feedback" hook (beep, LED); harmless without synohwd (missing FIFO is not an error) |
| `rynasd/src/updates.rs`, `rynas-updates/src/dom.rs`, `api-types/src/updates.rs` | `synodeb-dom status --json`, kernel/DOM card, `synodeb` flag | **move later**: a `BootLayer` point (kernel trial, boot entries); runs only where `synodeb-dom` exists |
| `rynasd/src/snapshots.rs`, `protection.rs`, `rynas-btrfs/src/rootfs.rs`, `api-types/src/snapshots.rs` | root rollback through `synodeb-dom sync`, `RollbackRefusal::NotSynodeb` | **move later**: `BootLayer::sync_root`; without synodeb the rollback is refused with a neutral text |
| `rynas-power/src/wake.rs`, `domain.rs` | S-35390A RTC preference, `After=synohwd.service` | **move later** (RTC hint from the module); `After=` on a missing unit is a no-op for systemd |
| `rynas-updates/src/domain.rs` | reads synodeb's `52synodeb-unattended-upgrades` among other `apt.conf.d` files | **keep-generic**: any file under `apt.conf.d` is read; the name appears in comments and tests only |
| `rynas-notify/src/checks.rs`, `rynas-storage/src/overview.rs` | "synohwd" in comments; provider data age | **ok-as-is**: generic `HardwareStatus` |
| `rynas-net/src/networkd.rs`, `ifupdown.rs`, `rynas-storage/src/domain.rs` | comments about the synodeb image's files | **ok-as-is**: generic behaviour, comments |
| `rynasd/tests/notifications.rs` | socket test with the Synology forward | **ok-as-is**: integration test of the module through the core socket |
| `packaging/test-synology.sh`, `tools/ui-audit/start.sh` (`FAKE_UPS_BACKEND=synodeb`) | Synology test scripts | **ok-as-is**: tests of the module; `--synodeb-notify-conf` dropped from `start.sh` |

## rynas-web

| File | What | Verdict |
|---|---|---|
| `src/hardware/synohwd.rs`, `main.rs --synohwd-status` | the only `HardwareProvider`, built on every machine | **move later**: provider registry; without the status file it reports no provider (nothing shown) |
| `src/notify.rs` | shutdown thresholds of the Synology model (`model::shutdown_c`) | **move later**: `HardwareProvider` threshold method; filtered on provider `synology` |
| `src/proxy.rs` | `/hardware/synology/*` routes | **move later** with the module routers; comment on UPS fixed |
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
| `chassis.rs` | identify through `/api/v1/hardware/synology/bays/{n}/identify` | **move later**: a provider-neutral identify endpoint |
| `pages/snapshots.rs` (`snap.root_step2`), `pages/updates.rs` (DOM card) | synodeb-dom texts and card | **move later** with `BootLayer`; listed in the test's `PENDING` |
| `demo.rs`, `alerts.rs` | sample provider `synology` | **ok-as-is**: demo of a Synology |
| `files_check.rs` | forbidden product names | **ok-as-is**: the check itself |
| `style/main.css` | comments | **ok-as-is** |
| page module comments naming another system ("DSM Control Panel › …") | code comments, not UI texts | **ok-as-is** (removed where touched) |

## Pragmatic exceptions (kept, documented)

- **Wire types** stay in `rynas-api-types` (one serde crate for daemon, web and wasm):
  `rynas_api_types::synology` is the module's namespace; `RescueInfo.synology`,
  `RollbackRefusal::NotSynodeb`, `KernelState.synodeb` remain until their points move.
- **Detection hooks** are the module's own (`SynologyModule::detect`); the core only calls
  `detect_modules` in `main.rs`, one line per module.
- **The Synology module is one crate** (`rynas-hw-synology`, daemon side behind its `domain`
  feature) plus the UI folder; daemon HTTP glue that needs rynasd internals (`hardware.rs`) stays
  in rynasd until the routers move (D-PLUG).

## Checks

`cargo test --workspace`, `cargo clippy --workspace --all-targets -D warnings`, wasm clippy with
and without `demo`, `trunk build --release` (brotli in the commit message of the UI change),
`cargo test --manifest-path crates/rynas-files-worker/Cargo.toml`.
