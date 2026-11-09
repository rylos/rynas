# Restart and shut down from the web interface (D-POWERACT)

Status: adopted 2026-11-07. Generic: any Debian with systemd; nothing here is Synology code.

Until now the NAS could only be shut down by a power schedule rule (`task.power.shutdown`), and
the Updates page said "Restart required. Asked by: linux-image-…" with no way to act on it.

## What others do (research 2026-11-07)

- **Cockpit** (Overview page): one Restart/Shut down dialog with a delay choice and a message
  for the logged-in users; delayed actions go through systemd's schedule.
  [Oracle Linux: Restart or Power Off System](https://docs.oracle.com/en/operating-systems/oracle-linux/cockpit/reboot.html),
  [cockpit PR 23113 (ShutdownModal, delays)](https://github.com/cockpit-project/cockpit/pull/23113).
- **DSM**: Restart and Shut down in the user menu at the top right, next to Sign out; shutting
  down another way skips its own checks.
  [Wundertech: How to shut down a Synology NAS](https://wundertech.net/?p=13695).
- **TrueNAS**: power menu in the top toolbar (Log Out, Restart, Shut Down); each opens a dialog
  that asks for a reason (audit log) and a Confirm checkbox; older releases warned about a
  running scrub or resilver.
  [TrueNAS top toolbar](https://www.truenas.com/docs/scale/25.04/scaletutorials/toptoolbar),
  [TrueNAS 11.3 Additional Options](https://www.ixsystems.com/documentation/truenas/11.3-U5/tn_options.html).
- **openmediavault**: power actions (shutdown, reboot, standby) with confirmation, scheduled
  power management as a separate page.
  [OMV 7 administration](https://docs.openmediavault.org/en/7.x/administration/general/index.html),
  [OMV features](https://openmediavault-docs.readthedocs.io/en/latest/features.html).
- **Proxmox VE**: Reboot and Shutdown buttons in the node's header, with a confirmation window
  (and an HA shutdown policy since 2023).
  [pve-devel: GUI documentation](https://pve.proxmox.com/pipermail/pve-devel/2019-February/035719.html),
  [pve-devel: shutdown policy in the confirm window](https://lists.proxmox.com/pipermail/pve-devel/2023-June/057731.html).
- **systemd-logind** keeps a scheduled shutdown (`/run/systemd/shutdown/scheduled`: `USEC`,
  `MODE`, `WARN_WALL`, `WALL_MESSAGE`), `shutdown -c` cancels it.
  [Phoronix: shutdownd moved into logind](https://www.phoronix.com/news/Systemd-Kill-Shutdownd),
  [Baeldung: checking a scheduled shutdown](https://www.baeldung.com/linux/shutdown-scheduled-time-check).

Ideas taken: the user menu entry (where Sign out is), one dialog with a delay (now / 1 / 5 min),
the list of what would be interrupted read **before** confirming (wider than a scrub: RAID, rynas
and scheduler tasks, SMB open files, apt, UPS on battery, a kernel trial boot), a danger button
without a typed word (restart is not destructive; the warnings are the safety), the audit line and
a notification with the user, a wall message for SSH users, a cancel for a delayed action.
Not taken: a mandatory reason (an extra field for a one-admin NAS; the audit line has the user and
the time), a free-text message (the wall message names rynas and the user).

## Behaviour

- **Entry points**: user menu (Restart, Shut down), "Restart now" in the "Restart required" note
  of System › Updates. The snapshot rollback's "a reboot is needed" note stays text: it is shown
  inside the rollback confirmation, before the rollback exists.
- **Dialog** (`ui::confirm`, danger): the delay (segmented), the pending action if any, then the
  warnings read from `GET /api/v1/system/power` (translated by code, `sysp.w.<code>`; the English
  text only for an unknown code). "You can go on anyway".
- **Immediate action**: the task starts, the dialog closes, a full-page overlay says
  "Restarting…/Shutting down…". It reads the task (`/tasks/{id}`, every 1.5 s: the event stream
  goes down with the server) until the server stops answering; a failed task closes the overlay
  with its error. Then it polls `/api/v1/auth/session` (any HTTP answer = up) every 3 s and opens
  `/` (the sign-in page: sessions end with the restart). A shut down ends on "The NAS is off"
  once the server stayed away for three probes. A "Reload page" button is always there.
- **Delayed action**: the dock shows "The NAS restarts at 14:05 (asked by admin)" with Cancel,
  for every administrator, also when the schedule was set over SSH (then without a user). The
  strip reads the state again when the time passes.

## Backend (`crates/rynasd/src/system_power.rs`, `rynas_power::manual`)

| Route | Body | Answer |
|---|---|---|
| `GET /v1/system/power[?pending_only=true]` (the option skips the warnings, which run `smbstatus`: what the interface asks at start) | — | `SystemPowerState` {pending? {action restart\|shutdown, at_ms, by?}, warnings [{code, subject?, op?, pct?, n?, text}]} |
| `POST /v1/system/power` | `SystemPowerRequest` {action, delay_min 0\|1\|5} | 202 `TaskStarted` (`system.restart` / `system.shutdown`); 400 `invalid_value` (`delay_min`) |
| `POST /v1/system/power/cancel` | — | 204; 409 `nothing_pending` |

Administrators only twice: rynas-web's `require_admin` and rynasd's `AdminGate` (reads included:
the warnings name tasks and arrays).

Warnings (codes `PowerWarningCode`): `raid_busy` (`/proc/mdstat`: resync, recovery, reshape,
check, also queued), `scrub_running` and `task_running` (running rynas tasks, the asking one and
other `system.*` left out), `scheduler_task_running` (Task Scheduler units), `smb_open_files`
(`smbstatus -L --json`, 5 s timeout), `apt_running` (`/proc/<pid>/comm` = apt, apt-get,
aptitude, dpkg, unattended-upgr), `ups_on_battery` (upsd, 2 s bound), `boot_trial` (the boot
layer's cached status has `trial: armed`: the next boot tries the new kernel, the boot layer falls
back by itself). The trial comes from the module's `BootLayer` through the generic
`KernelStatus.boot.trial` of the updates page; no module code in this feature, and the text does
not name the hardware. The cached status only: reading the boot device may mount it.

The task logs every warning, writes the audit line (`rynas::audit`: `system power restart now by
admin`), emits `power.manual_restart|manual_shutdown` (fields `user`, `delay_min`; English title
"NAS restarting by admin"), then runs `shutdown -r|-P now|+N "rynas: the NAS restarts (asked by
admin)"`. `now` waits 3 s first so the 202, the notification and the task's end leave before the
network goes. A delayed action also records the user in `<state>/power/manual.json` (logind only
knows uid 0); the pending action shows that user when the schedule matches (same mode, within
2 min). Cancel: `shutdown -c`, the record removed, audit line, notification
`power.manual_cancelled`.

### Why `shutdown`, not `systemctl reboot`

`systemctl reboot|poweroff` is immediate only. systemd's `shutdown` asks systemd-logind
(D-Bus `ScheduleShutdown`), which keeps the schedule outside rynasd (a restart of rynasd or
rynas-web does not lose it), writes `/run/systemd/shutdown/scheduled` (read back for the pending
strip, whoever set it), sends the wall message, creates `/run/nologin` five minutes before (new
non-root logins are refused: open sessions keep working, so the Cancel stays reachable) and is
cancelled with `shutdown -c`. Same command for `now`, so one code path.

Sandbox (`packaging/systemd/rynasd.service`): the call is a D-Bus message over `AF_UNIX` from
root, allowed as is (`RestrictAddressFamilies`, `SystemCallFilter=@system-service`,
`NoNewPrivileges`, no capability needed); PID 1 does the reboot outside the unit. Nothing to
relax.

### Tests never reboot

`SystemPower.shutdown` is the argv of the command: `["shutdown"]` in production, empty on
`--dry-system` (nothing runs; logind's file is simulated in `<state>/power/dry-scheduled`, so the
interface works end to end), and a recording shell script in `crates/rynasd/tests/system_power.rs`
(hidden `--shutdown-command` for manual runs). The tests cover administrators only (403/400),
the warnings (fake `/proc`), the delayed restart, the pending state with and without a user,
cancel and 409, the immediate shut down and the dry system.
