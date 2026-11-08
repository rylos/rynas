# Plugin `synology` — specification

Status: backend implemented as a built-in provider (2026-11-05): crates `rynas-hw` (provider
traits), `rynas-hw-synology` (status, `synology.hw` settings domain, FIFO commands, identify,
fan curves) and `rynas-power` (power schedule), API in `../phase1/plan.md` ("Synology hardware
and power schedule"), real check `packaging/test-synology.sh`. UI (2026-11-05): Hardware › Synology (chassis, fans with the DSM curve chart, thermal shutdown,
LEDs, beeper, buttons, standby, WoL with the MACs, AC recovery, identify, events, boot cause) and
Hardware › Power schedule. Missing: the plugin process (protocol v1), USB copy button actions and
the diagnostics page (phase 2).
`rynasctl net reset-button` (network module, D-NET) is implemented.

## Module boundary (D-MODULE, 2026-11-06)

Everything Synology or synodeb lives in the module; the core names neither and works without
it with standard Debian tools ("se non lo si ha deve funzionare tutto e senza rimandi a cose
che non sono standard"). The module is:

- `crates/rynas-hw-synology`: the `rynas-hw` provider (status, commands, identify, fan curves)
  and, behind its `domain` feature (rynasd only), `module::SynologyModule` (the
  `rynas_module::Module`), `domain` (`synology.hw`, the adoption of `/etc/synohwd.toml` at
  start), `http` (the module's routes `/api/v1/modules/synology/{status,settings,beep,mute,
  fan-curves}`), `boot` (synodeb's boot layer: `synodeb-dom status --json` on the kernel card,
  `synodeb-dom sync --root` as the boot half of a root rollback), `ups` (synodeb's `UpsLayer`:
  `synodeb-ups probe/setup/status --json/test`, the `/etc/synodeb/ups.conf` block, the Safe Mode
  notice), `notify` (synodeb's `notify` forward, the one-time `notify.conf` import); in
  `module.rs` the rescue beeps through synohwd's FIFO (`Beeps`), the reset button reflection and
  the wake clock (S-35390A, read by `synohwd.service`); in `provider.rs` the web provider
  (`Synology::web_provider`) with the model's thermal shutdown thresholds;
- `crates/rynas-ui/src/modules/synology`: the Hardware › Synology tab and its presence check,
  the module's dictionary table (`syn.*`, `nev.synodeb.*`, `nt.src.synodeb`, the UPS texts of
  synodeb's layer, the reset button), the UPS text slots, the reset button status;
- `packaging/modules/synology/ups-pre-safemode`: installed under
  `/usr/lib/rynas/modules/synology/`, linked as `/etc/synodeb/ups-pre-safemode.d/50-rynas` by
  the module at the start of rynasd, only where synodeb runs the UPS.

Detection (`SynologyModule::detect`, called by `rynasd::modules::detect` in rynasd and
`rynasctl`): Synology hardware (DMI, platform driver) gives the `synology.hw` domain, the DOM's
USB ids, the module routes' data, bay identify, the rescue beeps and the wake clock; synodeb's own
files below the configuration root (`/etc/synodeb/ups.conf`, `/etc/synodeb/notify.conf`) give the
UPS layer (with `ups.conf`), the forward socket and the import; an installed `synodeb-dom` gives
the boot layer. rynas-web picks the web provider on the same hardware (or where synohwd's
status file exists). Without any of them, nothing of the module is registered: the UPS page uses
NUT, the notify socket stays closed, `/api/v1/modules/synology/*` and identify answer 404, the
kernel card shows `/boot` only, a root rollback is a console procedure, the rescue paths are
silent, the power schedule arms `rtc0`; the Synology tab, texts and hook do not exist. Audit:
`../quality/module-boundary-2026-11-06.md` (nothing left to move).

Requirements (2026-11-05). Implements the `rynas-hw` provider for Synology NAS running
[synodeb](https://github.com/rylos/synodeb). Rule: **the plugin exposes every feature of synohwd and
synodeb's hardware tools in the UI; synohwd stays the executor** (it owns fans, LEDs, the
microcontroller and the RTC handshake). The plugin reads `/run/synohwd/status.json` and the
`synology-nas` sysfs, writes `/etc/synohwd.toml` through the rynas config pipeline
(pending → diff → apply → `systemctl reload/restart synohwd`), and sends validated microcontroller
commands through the synohwd FIFO (later its control socket, S8 in `../synodeb-integration.md`).

Supported models: DS920+ (reference, Celeron J4125, 4 bays, 2 fans) and DS923+ (Ryzen R1600, 4
bays, 2 fans); others as synodeb adds them. Model data (bays, fan tables) comes from synohwd/DMI.

## Feature map (synohwd → rynas UI)

| synohwd feature | Source / setting | rynas UI |
|---|---|---|
| Fan control on DSM curves | `fan_mode` low (quiet, DSM default) / high (cool) / full; per-model tables for disk, NVMe, CPU, NIC | Hardware → Fans: mode selector like DSM "Fan Speed Mode", live rpm and duty, the active curve drawn as a chart with the current sensor values |
| Fan tuning | `fan_period` (5–300 s), `fan_min_duty` (0–99), `hysteresis` (0–20 °C) | Advanced section with ranges and help text |
| Fan failure detection | `fan1/2_alarm`, uP events `f`/`g`, `fan_fail` | status badge, critical notification, history of events |
| Thermal shutdown | `thermal_shutdown` (disks 61 °C, NVMe 70 °C, CPU 90–95 °C per model) | toggle with the thresholds shown read-only; warning when disabled |
| Temperatures | `sensors` in status.json (cpu, bayN, nvme, nic; asleep disks keep the last value) | dashboard widget + Monitor graphs (D-MONITOR), "asleep" shown, never woken |
| Bays | `bayN_present`, `bayN_power`, device mapping | Storage chassis view: presence, power, disk, temperature, sleep state |
| Disk LEDs | `disk_leds` (green = present, activity blink via kernel trigger `ataN`) | toggle |
| LED brightness | `led_brightness` 1–255 (PWM A, shared) | slider with live preview |
| Front LED brightness schedule (synodeb ≥ 4.8) | `led_schedule` ("" = always normal, or 168 characters `0` normal / `1` reduced / `2` off, index = 24 × weekday + hour, local time, Sunday = 0: the DSM format) and `led_reduced_level` (0–7, clamped to the model's scale); status fields `led_schedule_supported`, `led_level_min/max/default`, `led_level_now`, `led_mode_now`. A separate chip from `led_brightness` (bay LED PWM): the effects add up. DS923+: scale 0–3, normal 3, level 0 = every front LED off; DS920+: not supported yet | card "Front LED brightness schedule" (`modules/synology/led.rs`), only when the status says `led_schedule_supported`: on/off switch (off = `""`), the shared week grid (`week_grid`, Normal / Reduced / Off), the reduced level as a number (levels below the normal one and above 0 are offered: 0 is Off in the grid), the current mode and level, **Import from DSM** (paste the raw string or `synoinfo.conf` lines with `led_brightness_schedule` and optionally `led_brightness_adjusted_level`; the recognised values fill the form, then the usual Save). rynasd refuses a schedule that is not "" or 168 × `[012]`, a reduced level outside the running synohwd's `led_level_min..=max`, and a new schedule when synohwd reports no chip (checks against the status skipped while synohwd is stopped). Written to `/etc/synohwd.toml` with the other keys; synohwd is restarted (it reads the file only at start) |
| Status LED | `status_led` (green, green blink, orange, orange blink, off) | shown in the chassis view; driven by synohwd from rynas health in a later phase |
| Bay "identify" | LED class devices `syno:{green,orange}:diskN` (no uP command exists for bay LEDs) | "Identify" button on a disk: blinks its bay LED for 30 s, then restores triggers and brightness |
| Beeper | `beep_on_start`, `beep_on_error`; uP `2` short, `3` long | toggles + "Test beep"; "Mute current alarm" action |
| Power button | `power_button` = poweroff / ignore / log / `command:` | selector; poweroff = clean shutdown through rynas (stops tasks/apps in order) |
| Reset button | `reset_button` = log / ignore / `command:` | `synology.hw` `reset_button`: `network_rescue` (default on a Synology) sets `command:` to rynas' handler: **double press within 10 s → network rescue** (D-NET); `log` / `ignore`. Owned here only: the network page shows it read-only (see below) |
| USB copy button / USB LED | uP event `` ` ``, commands `@` `A` `B` | phase 2: assign an action (e.g. run a backup plan to the USB disk, DSM "USB Copy") with LED feedback |
| Disk standby | `disk_standby_minutes` (0–330, hdparm -S) | per-volume idle timer, linked to D-SYSTUNE per-disk options; shows what woke disks last |
| Wake-on-LAN | `wol` (all lan* ports, uP `l`/`k`) | toggle + MAC addresses to copy |
| Power back after AC loss | `ac_recovery` (uP `q`/`p`) | toggle (DSM "Restart automatically when power supply issue is fixed") |
| **Scheduled power on/off** | RTC `wakealarm` (`rtcwake -d rtc0 -m no -t <epoch>`), synohwd tells the uP at shutdown (`s`/`r`) | **Power Schedule like DSM**: weekly rules "power on at" / "power off at" per day, multiple rules; see below |
| Watchdog | synodeb hardware watchdog | status shown; reset count from boot-report |
| UPS | synodeb-ups (NUT, Safe Mode) | the core UPS page over this module's `UpsLayer` (`ups.rs`); NUT without the module (D-UPS, D-MODULE) |
| Microcontroller current reading | `up_current` (raw ×16.13, unit unknown) | shown in diagnostics only, labelled raw |
| Events | `events` (last 20) | Hardware → Events, merged into notifications |
| Raw uP commands | `synohwd send` (validated sequences only) | Diagnostics (admin, behind a confirmation) |

## One writer of `/etc/synohwd.toml`

`synology.hw` is the only configuration key that writes `/etc/synohwd.toml` (whole file,
managed header). The reset-button network rescue is one of its settings
(`PUT /api/v1/modules/synology/settings`, `reset_button: network_rescue`); the network module's
`network.rescue` key (Avahi file, link-local) does not touch the file and shows the button
read-only in `GET /api/v1/network/rescue` (`reset_button` {module, configured, pending?},
filled by the module's `reset_button` hook).
At the start of rynasd (blocking task after the network defaults, `Module::start` →
`domain::adopt`):

- first start on a Synology where the file exists and `synology.hw` was never applied: the
  file is adopted with what synohwd runs with and `reset_button = network_rescue` (kept as it
  is when an older `network.rescue` value had `reset_button: false`);
- a file with the `# BEGIN rynas network` block written by `network.rescue` before this
  change: rewritten by `synology.hw` with the block's reset-button setting, without the block.

Both are one revision by `rynas` and one synohwd restart; then nothing until the file needs it
again. Tests: `rynas-hw-synology` `domain` (adoption, migration of every legacy layout),
`conf::golden_rynas_default`, `rynasd/tests/hardware.rs` `synohwd_conf_has_one_writer` (applies
of both keys in any order, no drift), `packaging/test-synology.sh` (legacy file adopted for
real).

## Power schedule (DSM model)

DSM "Power Schedule": a list of rules, each with a weekday set, a time and an action (startup or
shutdown). The RTC holds **one** alarm, so rynas computes it:

1. **Shutdown rules** become rynas tasks (D-TASKS, systemd timers): at the time, a pre-check (no
   resync/scrub, no running backup/replication, no active SMB sessions with open files — each
   configurable: wait up to N minutes, skip, or force) and then a clean poweroff through rynas.
2. **Startup rules**: before *every* poweroff (scheduled, manual, UPS Safe Mode exit excluded),
   rynas computes the next startup time from the rules and writes it with `rtcwake -m no -t <epoch>`;
   synohwd's shutdown hook informs the microcontroller (`s`), as it does today. A boot-time check
   re-arms the alarm if the machine was started by other means.
3. Conflicts (startup and shutdown at the same minute, shutdown less than 10 minutes after a
   startup) are rejected in the editor; DST handled by computing in local time and converting.
4. The UI shows "next startup: Mon 07:00", "next shutdown: today 23:30", and the history from the
   boot reports (who started the NAS: power button, RTC, WoL, AC recovery — if synohwd can tell).

## Phases

| Phase | Content |
|---|---|
| 1 (P4) | read-only status (fans, temps, bays, LEDs, events), fan mode, thermal shutdown, LEDs/brightness, beeper, buttons, standby, WoL, AC recovery, identify, reset-button network rescue, **power schedule** |
| 2 | USB copy button actions, status LED driven by rynas health, synohwd control socket (S8), diagnostics page |
| 3 | `synohw` Rust rewrite shared by daemon and plugin (D-HW) |
