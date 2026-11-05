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

## Left for later
Network UPS server with connected clients, client mode (another NAS's upsd), driver wizard
(`nut-scanner`), UPS power-off at the end (DSM's option, synodeb `SAFESHUTDOWN`), outage
statistics and battery trend (RRD), the native (non-synodeb) backend that writes `/etc/nut`.
