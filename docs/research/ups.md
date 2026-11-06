# rynas: UPS page (P6)

Short follow-up of `notifications-ups-updates.md` §2 (D-UPS), for the first UPS page on top of
synodeb. Sources: the DSM help (Hardware & Power › UPS), TrueNAS SCALE (System › Services › UPS
and the dashboard widget), Unraid (Settings › UPS, apcupsd/NUT plugin), NUT's `upsstats.cgi`,
`upsset.cgi` and `upsimage.cgi`.

## What the others show and allow

| | Status | Policy | Test | Other |
|---|---|---|---|---|
| DSM | on/off battery, model, battery %, runtime | "Safe Mode after N minutes" or "until low battery"; network UPS server with allowed IPs | none in the UI | shutdown UPS when the NAS enters Safe Mode |
| TrueNAS | dashboard widget: status, charge, runtime, load | shutdown mode "UPS goes on battery" (after N s) or "UPS reaches low battery"; shutdown command; no hook order | none in the UI (shell `upscmd`) | driver picker, remote monitor, e-mail on events |
| Unraid | charge, runtime, load, nominal power, model | "battery level", "runtime left" or "time on battery" thresholds, whichever first | apcupsd self-test only via CLI | kill UPS power, network server |
| NUT CGI | `upsstats`: status flags decoded, charge/load/voltage bars, battery date, model | — (`upsset` edits variables, needs upsd users) | instant commands list | `upsimage` bar graphs |

## Picked

- **Status card** like NUT's `upsstats`: overall state pill from the flags (FSD > OB+LB >
  LB > OB > RB > OL), every flag decoded as its own pill (icon + text), charge and load as bars,
  runtime, input/output voltage, model, manufacturer, serial, driver, beeper and battery date
  when the UPS reports them. Read straight from upsd's text protocol (no `upsc` process), every
  5 s only while a browser listens; never touches a disk. No chart of long ranges yet: the
  persistent history belongs to D-MONITOR's RRD.
- **Safe Mode policy**: DSM's "after N minutes" timer, but shown as an *extra* trigger. Low
  battery (LB) and a critical UPS (OB+LB) always start Safe Mode, whichever comes first
  (Unraid's "whichever first" wording, which is also synodeb's behaviour). Marco explicitly
  wants low battery to trigger it, so it is not a choice in the form, only a statement.
- **Ordered stop before Safe Mode** (none of the others has it): synodeb's
  `ups-pre-safemode.d` hook asks rynasd to cancel its tasks and stop running scheduler units,
  bounded well within `PRE_SAFEMODE_TIMEOUT` (editable, as DSM-style hooks need time for VMs
  and apps later).
- **Battery test** in the UI (missing in DSM and TrueNAS): quick, deep (confirm dialog: LB still
  starts Safe Mode, someone must be present), stop; the task follows `ups.test.result` to an
  outcome. Scheduling: a Task Scheduler command task (`synodeb-ups test quick` as root) for now;
  a dedicated task action can come with the `nut` plugin.
- **Recent UPS events** from the notification center (category `ups`; synodeb's own UPS
  notifications arrive there through the S1 forward), so there is one log.

## Generic backend (2026-11-05)

The page must work on any Debian with NUT, synodeb being only an extra backend where
`/etc/synodeb/ups.conf` exists. Sources: NUT's network protocol
(`docs/developer-guide.chunked/net-protocol.html`: `LIST CMD`, `USERNAME`, `PASSWORD`,
`INSTCMD`, errors `ACCESS-DENIED`, `CMD-NOT-SUPPORTED`, `USERNAME-REQUIRED`), `upsd.users(5)`
(`instcmds` repeated per command or `ALL`, root:nut 0640), `upsmon(8)` (`upsmon -c fsd`:
"shut down before the battery is critical, e.g. through upssched"; primary sets FSD, waits
`HOSTSYNC`, `FINALDELAY`, runs `SHUTDOWNCMD`, `POWERDOWNFLAG` for the UPS power-off),
`upssched(8)` (`AT ONBATT * START-TIMER`, one `CMDSCRIPT`), Debian's `nut-server.service`
(`ExecReload=upsd -c reload`).

How the others do it: TrueNAS SCALE, the openmediavault-nut plugin and Unraid's NUT plugin
**own the whole of `/etc/nut`**: they generate `ups.conf`, `upsd.users` (their own monitor user),
`upsmon.conf` and, for "shut down after N seconds on battery", an `upssched.conf` timer whose
command runs `upsmon -c fsd`. Battery tests are a shell `upscmd` in all three.

rynas cannot own `/etc/nut`: on a plain Debian the admin may already have a working NUT setup
(drivers, slaves, their own `NOTIFYCMD`/`upssched` script). `upsmon.conf` has a single
`NOTIFYCMD` and `upssched.conf` a single `CMDSCRIPT`, so an upssched timer would fight such a
setup. Picked, least invasive:

- **Status** as before (anonymous `LIST`), plus `LIST CMD`: only the `test.battery.*` commands
  the UPS lists are offered; a UPS without them gets a neutral note.
- **Battery test**: rynasd sends `INSTCMD` itself (no `upscmd` child) as a dedicated upsd user
  `rynas`, a managed block of `upsd.users` with only the three test commands (no `actions`, no
  `upsmon` role), password generated once into `/etc/rynas/nut-upsd.secret` (root 0600, never
  logged). Written only when a test runs and the block is missing or drifted, then
  `systemctl reload nut-server.service` (fallback `upsd -c reload`). An existing `[rynas]`
  section of the admin's is an error, never overwritten. Same task and result tracking as on
  synodeb; `rynasctl ups test quick|deep|stop` for scheduled tests on both backends.
- **Policy**: no change to NUT's files. upsmon keeps handling low battery exactly as the admin
  configured it. rynasd's watcher (upsd every 5 s, the same cost as the page's own reading)
  carries out the extra "after N minutes on battery" trigger from a rynas-owned file
  (`/etc/rynas/ups.conf`, config domain `ups.safemode` with drift detection): it stops rynas
  tasks and scheduler units within the configured time, then calls `upsmon -c fsd` (NUT's own
  shutdown path, so slaves, `SHUTDOWNCMD` and the UPS power-off still work), falling back to
  `systemctl poweroff` with an error notification if upsmon refuses. On OB+LB or FSD it only
  does the ordered stop (upsmon is already shutting down). It also records "on battery" and
  "power back" in the notification center, which plain NUT does not feed.
- **UI**: neutral texts on NUT ("shut down" instead of Safe Mode, "install NUT (Debian package
  nut)" when missing); synodeb, `synodeb-ups` and Safe Mode appear only on the synodeb backend.

Limits: the backend is NUT when `/etc/nut/nut.conf` or `upsd.users` exists, so a `netclient`
setup (upsd on another host) shows the page but the tests need a local upsd; rynasd and rynas-web
both default to `127.0.0.1:3493`. The watcher needs upsmon running as primary for the shutdown
(otherwise the poweroff fallback). NUT plans to require login for `LIST` later; the status
reader will then need the same upsd user.

## Driver wizard (2026-11-06)

Goal: set up any UPS NUT supports from the UI, on a plain Debian without touching an admin's
own NUT setup, and on synodeb through synodeb's own tool.

Sources: NUT's `nutupsdrv(8)` (`-s <id>` "configure this driver only with command line
arguments instead of reading ups.conf", `-x var=val`, `-d <count>` "dump the data tree (in
upsc format) after the update loop ran count times and exit", NUT ≥ 2.8.0, banner hidden by
`NUT_QUIET_INIT_BANNER`), `nut-scanner(8)` (`-U` USB, `-S` SNMP, `-M` XML, `-O` old NUT
servers; output is a ready `ups.conf` section with `driver`, `port`, `vendorid`,
`productid`), `upsdrvctl(8)`, `upsdrvsvcctl(8)` and `nut-driver-enumerator(8)` (Debian:
one `nut-driver@<section>.service` per `ups.conf` section, `nut-driver.target`, resynced by
`upsdrvsvcctl resync` or the enumerator's path unit), `upsdrvquery(8)` (NUT 2.8.1+, talks to
a running driver's socket; not needed for a one-shot test), NUT's hardware list
`/usr/share/nut/driver.list` (shipped by Debian's `nut-server`, also
`62-nut-usbups.rules` for USB permissions), NUT issue 2738 / PR 3323 (USB ID `0925:1234`).

How the others do it: TrueNAS SCALE offers a driver drop-down built from `driver.list`
(manufacturer · model → driver), a port field, "auxiliary parameters" for `ups.conf` and
owns `/etc/nut`; no detection. The openmediavault NUT plugin has driver, port and a free text
of extra directives, also owning the files; no detection. Unraid's NUT plugin lists the USB
drivers (custom driver allowed); users report nut-scanner suggestions that start but never
talk to the UPS. Home Assistant's NUT add-on takes a YAML list of devices (driver, port,
config lines). None of them tests a driver before writing it.

Real case: Marco's VULTECH UPS1050VA-PRO, USB `0925:1234` ("Lakeview Research UPS USB Mon
V2.0"). `nut-scanner -U` suggests `richcomm_usb`, synodeb's default was `usbhid-ups`; neither
reads it. NUT's hardware list (Armac, Accurat Flux, issue 2738) and months of use on another
PC: `nutdrv_qx` with `subdriver=armac`, `protocol=megatec`, `norating`, `pollfreq=30`,
`vendorid=0925`, `productid=1234`. On Debian trixie (NUT 2.8.1) no driver works
(`nutdrv_qx` armac: "Device not supported!", `richcomm_usb`: "driver callback failed: Entity
not found", `blazer_usb`: "No supported UPS detected"); it works from NUT 2.8.5.

Picked:

- **Candidates, then a test run of each** (none of the others does it). For each UPS on
  USB: rynas's small table of USB IDs known to need a driver setting first
  (`rynas_nut::setup::KNOWN`, with the lowest NUT version it works with), then the
  scanner's suggestion, then `usbhid-ups`, `nutdrv_qx`, `blazer_usb`, all with the USB IDs.
  Each runs as `<driver> -s rynasprobe -x port=… -x k=v … -d 1` under a 45 s timeout; the
  first one that reports `ups.status` wins and the others are not run. A candidate known to
  need a newer NUT than the installed one (`upsdrvctl -V`) is skipped and the result says
  "needs NUT X, this NAS has Y" with each failed driver's last lines.
- **Manual entry** for everything else: any installed driver (manufacturers from
  `driver.list` as a hint), port (`auto`, `/dev/ttyS0`, an SNMP address), extra options;
  tested the same way, or applied untested on request.
- **A running driver holds the device**: the detection reports it and stops
  `nut-driver@*` only when the admin allows it, starting them again afterwards.
- **Apply = configuration key `ups.device`** (pending → apply, revision, rollback):
  rynas blocks only, never the admin's lines: its section in `ups.conf`, `MODE` in
  `nut.conf` (`netserver` stays), the upsmon user `rynas-upsmon` in `upsd.users` and its
  `MONITOR … primary` line in `upsmon.conf`, files root:nut 0640. A section of the admin's in
  the way (same name, or a USB section without different USB IDs) makes the apply refuse
  unless the admin takes it over in the wizard (the section and its `MONITOR` line are then
  removed; the apply keeps a backup). Then `upsdrvsvcctl resync`, `reset-failed` of the
  driver unit (a failed start with an older setting, e.g. right after the package install),
  `enable --now` and restart of
  `nut-driver`, `nut-server`, `nut-monitor`; a driver that does not start rolls the apply back.
  The review step shows the blocks, the removed lines and the commands (passwords masked).
- **synodeb** owns `/etc/nut`: test runs are `synodeb-ups probe --driver … --timeout 45`
  (exit 0 vars, 1 no status, 2 bad usage/unknown driver, 3 device held by a nut-driver), the
  apply `synodeb-ups setup --keep-password --driver D --port P --opt k=v …` (synodeb keeps it
  in `/etc/synodeb/ups-driver.conf`; rollback to nothing: `--default-driver`). An older
  synodeb (no `--driver` in `setup --help`) blocks the wizard with "update synodeb".
- Drivers are looked up in `/usr/libexec/nut` (NUT ≥ 2.8.2), then `/lib/nut` and `/usr/lib/nut`.
- Values limited to `[A-Za-z0-9._:/@+,=-]` (option names `[a-z][a-z0-9_.]`, ≤ 32 options):
  nothing needs quoting in `ups.conf`, argv or synodeb's `--opt`.

## Left for later
Network UPS server with connected clients, **client mode** (monitor another NAS's upsd: the
status reader of rynas-web and the watcher read `127.0.0.1:3493` only, so it needs a
configurable upsd address first), SNMP/XML scans (`nut-scanner -S/-M` need a range or a host:
manual entry for now), a udev rule for a USB ID missing from NUT's rules (the test run then
fails with a permission error), removing the wizard's setup from the UI (the `ups.device` key
can be removed from the configuration), UPS power-off at the end (synodeb `SAFESHUTDOWN`; on
NUT it is the admin's `POWERDOWNFLAG` setup), outage statistics and battery trend (RRD), a
check that upsmon is running as primary (shown on the page).
