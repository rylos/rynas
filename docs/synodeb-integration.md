# rynas ↔ synodeb integration

Principle: on synodeb, **rynas is the interface and orchestrator, synodeb stays the hardware
executor**. rynas never duplicates what synodeb already does (fans, LEDs, buttons, UPS Safe Mode,
kernel trial, notifications); it reads it, configures it through synodeb's own tools and shows it.

Status: proposals collected from the phase 1 research (2026-11-05). Nothing has been changed in
synodeb; each item needs the maintainer's approval in the synodeb repo.

## What rynas reads today (no synodeb change needed)

| Data | Source |
|---|---|
| Fans, temperatures, bays, LED, events | `/run/synohwd/status.json` |
| Raw uP commands (validated) | FIFO `/run/synohwd/cmd` |
| Bay presence/power, fan rpm/alarm | sysfs `/sys/devices/platform/synology-nas`, hwmon `synology` |
| synohwd config | `/etc/synohwd.toml` |
| UPS state | upsd on `127.0.0.1:3493`, text protocol (NUT configured by `synodeb-ups setup`); `SAFEMODE_AFTER`/`PRE_SAFEMODE_TIMEOUT` of `/etc/synodeb/ups.conf` are the rynas block `ups.safemode`; hook `ups-pre-safemode.d/50-rynas`; battery test via `synodeb-ups test` (see `phase1/plan.md`, API v1: UPS) |
| Unattended-upgrades policy | `/etc/apt/apt.conf.d/52synodeb-unattended-upgrades` (read-only, respected) |
| Notification channels | `/etc/synodeb/notify.conf` (imported once into rynas targets) |

## Proposed synodeb changes

| # | Area | Change | Why (rynas decision) |
|---|---|---|---|
| S1 | Notifications | `notify -s <source>`; when `/run/rynas/notify.sock` answers (or `FORWARD=rynas` in `notify.conf`) POST the event to rynasd and exit, otherwise use its own channels. rynas side done: contract below | one notification center without losing alerts when rynas is down (D-NOTIFY) |
| S2 | UPS | `SAFEMODE_AFTER=<sec>` in `ups.conf` (today only a commented upssched timer) | DSM-style "Safe Mode after N minutes" (D-UPS) |
| S3 | UPS | hook directory `/etc/synodeb/ups-pre-safemode.d/` run with timeout before `isolate synodeb-ups-safe.target` | ordered stop of VMs, apps, tasks before Safe Mode (D-UPS) |
| S4 | UPS | NUT `admin` user with `instcmds = test.battery.start.quick, test.battery.start.deep` | scheduled battery test (D-UPS) |
| S5 | Kernel/DOM | `synodeb-dom status --json` with a stable schema (entries, default, trial result, kernel versions) | updates page and kernel trial result in the UI (D-UPDATES) |
| S6 | Kernel/DOM | coordinated rollback: a new `synodeb-dom sync --root <snapshot>` that rewrites the DOM entry from the snapshot's kernel. `default debian-old` is **not enough**: DEBIAN_OLD keeps only one previous kernel, so a snapshot two kernels old has matching modules on no entry (synodeb review, 2026-11-05) | root rollback must not leave a kernel on the DOM without its `/lib/modules` (D-SNAP) |
| S7 | Network | reset button gesture → network back to DHCP + safe firewall + beep, like DSM "mode 1 reset". The uP sends `a` once and never the press length (DS923+ after ~2 s, DS920+ on a short press), so the gesture is a **double press within 10 s**, timed on the host. **No synodeb change needed**: `synohwd.toml` already supports `reset_button = command:<…>`, which rynas points at `rynasctl net reset-button` (the double-press logic lives in rynas). rynas side done (P4): the line is a `# BEGIN rynas network` block of `/etc/synohwd.toml` written by the configuration key `network.rescue` (only where the file exists), beeps and the orange LED go through the FIFO (`2`, `3`, `;`); see `phase1/plan.md`, API v1: network | headless NAS must never be locked out (D-NET) |
| S9 | Kernel | enable `CONFIG_ZRAM_MULTI_COMP` (today not set; default compressor lz4, zstd/writeback/actime already built) so rynas can offer lz4 + zstd recompression of idle pages on 4-8 GB models | D-ZRAM |
| S10 | Kernel | full **iGPU** support on the DS920+ (i915 DRM, GuC/HuC firmware, `/dev/dri/renderD128`, VAAPI): hardware transcoding (Jellyfin, Immich, Frigate), OpenVINO on GPU, GPU stats. Requested by the maintainer | D-PHOTOS, D-NVR, D-MONITOR |
| S8 | Hardware | synohwd control socket (JSON/varlink) instead of the FIFO; later the Rust rewrite `synohw` shared by daemon and rynas plugin | D-HW phase 2 |
| S11 | Hardware | **done** (synodeb 021aa66, `/etc/udev/rules.d/60-synodeb.rules`): the internal USB boot flash (`ID_BUS=usb`, ids `f400:f400`) and its partitions get `SYNODEB_BOOT_FLASH=1`, `UDISKS_IGNORE=1` and the links `/dev/synoboot`, `/dev/synoboot1`, `/dev/synoboot2` | rynas skips drives with either property; the `f400:f400` check stays as a fallback for older synodeb (D-REMOVABLE) |

## Status on the synodeb side (2026-11-05)
S1, S2, S3 and S5 are implemented on synodeb's main (not yet released) and installed on the test
DS920+; S1 was tested end to end against rynasd (202, "forwarded to rynas"). S4, S6 and S8 followed
in synodeb 4.3; S7 needs no synodeb change. Facts for the rynas side:

- **S1**: `notify` posts form-urlencoded `title`, `message`, `priority`, `source`, `force` (0/1)
  before MIN_PRIORITY and its duplicate filter; stops on 202, otherwise uses its own channels;
  `FORWARD=off` in `notify.conf` disables it. Sources: `smart`, `raid`, `hw` (synohwd), `ups`,
  `boot`, `dom`, `unit` (failed unit), `emergency`, `synodeb` when the caller gives none.
- **S5**: `synodeb-dom status --json` (root: it mounts the DOM), schema 1, fields only ever added:
  `{"schema":1,"entries":[...],"default":"DEBIAN","default_index":2,"fallback":[titles],
  "trial":"none|armed|trying","running":"DEBIAN","running_kernel":"…","cmdline":"…",
  "outdated":null|"msg","kernels":{"DEBIAN":{"release":…,"version":…},"DEBIAN_OLD":{…}|null},
  "dom_free_kb":N}`.
- **S2**: `SAFEMODE_AFTER=<seconds>` in `/etc/synodeb/ups.conf` (default 0 = off): on ONBATT a
  systemd timer (`synodeb-ups-wait.timer`) starts, ONLINE cancels it, expiry enters Safe Mode
  with reason `waittimeup`.
- **S3**: executables in `/etc/synodeb/ups-pre-safemode.d/` (names `[A-Za-z0-9_-]`) run in order
  before the isolate with the reason as `$1` (`lowbatt|waittimeup|nocomm|fsd|shutdowncmd`), total
  time bounded by `PRE_SAFEMODE_TIMEOUT` (default 120 s, then killed), output in the journal
  (tag `synodeb-ups-hook`), `DRYRUN=1` only lists them. rynas installs its own hook there
  (e.g. `50-rynas`) asking rynasd to stop VMs and apps.
- **Safe Mode triggers** (any of them, the first wins; `safemode()` exits if
  `/run/synodeb-ups/safemode` exists, so the pre-safemode hooks run once with the first reason;
  events are serialised with a flock and stop the `synodeb-ups-wait*` timers): NUT LOWBATT
  (`upssched` → `lowbatt`), upsmon OB+LB critical (`SHUTDOWNCMD` → `shutdowncmd`), the
  `SAFEMODE_AFTER` timer (`waittimeup`), `nocomm`, `fsd`. If the UPS is back OL (no FSD) meanwhile,
  no Safe Mode ("UPS back online: no Safe Mode (reason)"). Checked in the code, not yet with a
  real UPS.
- **S4** (synodeb 4.3): `synodeb-ups test quick|deep|stop|result` (local NUT admin user); during a
  test started this way ONBATT is not notified and `waittimeup` waits for the end; LB still enters
  Safe Mode (a deep test only with someone present).
- **S6** (4.3): `synodeb-dom sync --root DIR` with the snapshot mounted at DIR before the rollback;
  refuses when the snapshot lacks `/lib/modules` of its kernel; the snapshot then replaces
  `@rootfs`.
- **S8** (4.3): `/run/synohwd/control.sock` (root, one JSON line per request, schema 1):
  `{"cmd":"ping"}` → `{"ok":true,"schema":1}`, `{"cmd":"status"}` → `{"ok":true,"status":…}`,
  `{"cmd":"send","up":"2"}` → `{"ok":true}` or `{"ok":false,"error":"…"}`; the FIFO stays.
- **status.json `m2`**: `"m2": {"1": {"present", "controller", "device", "pci", "port"}, …}`;
  slot order from the model's power-limit table (DS923+: 00:01.2, 00:01.3; DS920+: 00:14.1,
  00:14.0); `{}` on models without a map. nvme0/nvme1 swap between boots: always go through `m2`.

## Status on the rynas side: S6 and S8 (2026-11-05)

**S8, control socket** (done): `rynas_hw_synology::cmd::Commander` sends microcontroller
commands through `/run/synohwd/control.sock` when it answers `{"cmd":"ping"}` with schema 1
(result cached 30 s, 2 s timeouts for connect, write and read), otherwise through the FIFO as
before. `{"ok":false,"error":…}` reaches the caller as `ControlError::Failed` with synohwd's
text (500 `internal`, the text as the message), so the beep test in the UI shows a real error. A
socket that cannot be reached falls back to the FIFO; a request sent without an answer is not
repeated on the FIFO (it may have run) and answers 503 `backend_unavailable`. The periodic
status still reads `status.json`; `Commander::status()` (socket `status`) exists for places
where freshness matters, none uses it yet. Tests with a fake socket in `cmd.rs`.
Rescue beeps: the Synology module's `rescue_feedback` (`module::Beeps`, used by rynasd and by
`rynasctl` from the reset button and the watchdog) sends `2`/`3`/`;` with `cmd::send` on the FIFO,
best effort (a stopped synohwd is not an error).

**S6, root rollback** (done, not yet tried on the DS920+): `POST /api/v1/snapshots/{id}/rollback`
on a system restore point is no longer a 409 on synodeb (`/usr/sbin/synodeb-dom` present).
`GET /api/v1/snapshots/{id}/preview-rollback` (`RollbackPreview`: kernel the restore point boots,
running kernel, modules present, `allowed`/`refusal`) feeds the confirmation dialog (kernel
shown, steps, typed restore point name). The task `snapshot.rollback` runs
`rynas_btrfs::rootfs::rollback_root`:
1. bind-mount of the restore point read-only at `/run/rynas/rollback/<id>`; kernel from
   `/vmlinuz` (or the newest `/boot/vmlinuz-*`), refused without `/lib/modules/<release>`;
2. `synodeb-dom sync --root /run/rynas/rollback/<id>`; a refusal ends the task, nothing changed;
3. top level (subvolid 5) mounted at `/run/rynas/rollback/top`; a new read-only restore point
   of the running system (reason `before rollback`, retention not applied so the target is
   never pruned); writable snapshot of the restore point as `@rootfs.rynas-new-<stamp>`; the
   restore points (nested subvolumes in `@rootfs/.snapshots/rootfs`) move into it so they stay
   listed after the reboot (skipped when the directory is its own subvolume or mount);
   `@rootfs` → `@rootfs.rynas-rollback-<stamp>`, new → `@rootfs` (`RENAME_NOREPLACE`). Any
   failure undoes the steps and runs `synodeb-dom sync --root /` to put the DOM entry back
   (the Synology module's boot layer, `rynas_hw_synology::boot`, D-MODULE);
4. unmount; the task log and a notice ask for the reboot.
Every `mount`/`umount` of steps 1, 3 and 4 runs in a transient unit outside rynasd's sandbox
(`systemd-run --wait --pipe`, units `rynas-rollback-*`, shared builder
`rynas_tasks::Outside`): rynasd's seccomp filter (`@system-service`) has no mount calls, and
`synodeb-dom`, which runs outside too, must see the restore point's mount. The mounts
propagate into rynasd's mount namespace, where the btrfs steps (ioctls, renames) run.
On other systems (no boot layer) the preview says `no_boot_layer` and the rollback stays a 409
with a note (console procedure). `rynasd --dry-system` only logs the steps (with the boot layer
when the fake hardware is a Synology or has `usr/sbin/synodeb-dom`). Tests: step order,
refusal, undo and dry run with a fake `RootOps`/`BootSync` (`rootfs.rs`), API refusals in
`rynasd/tests/protection.rs`.
Left:
- run it once on the test DS920+ (and in `packaging/test-snapshots.sh`, which does not cover
  root rollback: it needs a DOM and `synodeb-dom`);
- confirm with synodeb that `sync --root` reads only the tree (kernel, initrd, `etc/fstab`) and
  that the kernel rynas shows is the one it picks (`/vmlinuz` link, else newest image);
- the replaced root `@rootfs.rynas-rollback-<stamp>` stays at the top level until deleted by
  hand after the reboot: a cleanup action in the UI is not there yet;
- no reboot button: the notice asks for it (no immediate reboot endpoint exists yet);
- the btrfs default subvolume is not touched (synodeb boots with `rootflags=subvol=@rootfs`).

## S1 contract: synodeb `notify` → rynasd
rynasd side implemented (crate `rynas-notify`, `crates/rynasd/src/notifications.rs`); the
synodeb side is the proposal below.

**Socket**: `/run/rynas/notify.sock` (rynasd option `--notify-socket`), HTTP/1.1 over a Unix
socket, mode 0600, owner root; peers are checked with `SO_PEERCRED` (root and rynasd's own uid).
The socket exists only while rynasd runs (removed on stop), so "socket missing or not answering"
means "use your own channels".

**Request**: `POST /v1/notify`, body at most 64 KiB, either `application/json` or
`application/x-www-form-urlencoded` (easier from `sh` with `curl --data-urlencode`):

| Field | Type | Meaning |
|---|---|---|
| `title` | string, required | one line; rynas keeps 200 characters |
| `message` | string | body; rynas keeps 4000 characters |
| `priority` | `low` \| `normal` \| `high` \| `urgent` | default `normal`; mapped like synodeb's syslog levels: low → `info`, normal → `notice`, high → `warning`, urgent → `critical` |
| `source` | string | the caller (`unit`, `smart`, `raid`, `boot`, `emergency`, `hw`, `dom`, `ups`...); reduced to `[a-z0-9._-]{1,32}` (default `notify`); stored as source `synodeb:<source>` |
| `force` | bool / `1` | `-f`: bypass the duplicate window |
| `event` | string, optional | `-e`: stable event code, `[a-z0-9_]{1,32}`; stored as code `synodeb.<event>` |
| `field.<name>` (form) / `fields` (JSON object of strings) | optional | `-v name=value`: event parameters; name `[a-z0-9_]{1,32}`, value one line of at most 200 characters (may be empty), at most 16 |
| `format` | `text` \| `log`, optional | `-l`: the message is command output or a log; default `text` |

The last three are optional and backwards compatible: a sender without them works as before.
An invalid code, parameter (bad name, too long, a line break, a non-string JSON value, past the
16th) or format is dropped (logged by synodeb as "ignored"), never the notification; a repeated
`field.<name>` keeps its first value. Parameters are stored in the event's `fields` next to
`priority`, usable by rules (field matches) and channel templates.

**How rynas shows coded events.** The web interface translates the title and body from the code
and the parameters (dictionary keys `nev.synodeb.<event>.title` / `.body` with `{name}`
placeholders, variants `…_2`/`…_3` for parameters that may be empty, `nev.synodeb.<event>.<name>.<value>`
for values of a closed set). When the code is unknown, the key is missing or a placeholder's
parameter is missing or empty, it shows the English `title`/`message` as sent: an event rynas does
not know yet still reads fine. A `log` message is never translated: it is shown in monospace with
its line breaks (two lines in the bell, the whole text in the detail drawer). Channels whose
target language is Italian get the same translated title and body; English targets get the text
as sent. synohwd's `fan_failure` / `fans_ok` (source `hw`) are the condition
`hardware:fan_fail` of rynas' hardware watch (matched by event code, by title for senders without
codes), so the two sources make one notification.

**Event codes sent by synodeb** (synodeb commit 810cccd; translations in
`crates/rynas-ui/src/i18n.rs`, `nev.synodeb.*`):

| Event (`synodeb.<event>`) | Source | Parameters | Priority | Message |
|---|---|---|---|---|
| `unit_failed` | `unit` | `unit` | high | log (`systemctl status`) |
| `smart_error` | `smart` | `device`, `failtype` (Health, FailedHealthCheck, CurrentPendingSector, OfflineUncorrectableSector, FailedReadSmartData, EmailTest, ErrorCount, SelfTest, FailedOpenDevice, Usage; translated) | urgent / high / low | log or text (smartd's message) |
| `raid_event` | `raid` | `array`, `event` (mdadm: Fail, FailSpare, DegradedArray, DeviceDisappeared, SparesMissing, MoveSpare, SpareActive, RebuildFinished, RebuildStarted, TestMessage, NewArray; translated), `device` (may be empty) | urgent / normal | log (`/proc/mdstat`) |
| `boot_completed` | `boot` | `kernel`, `addresses`, `previous_boot` (empty, or the start of a previous boot that did not end cleanly), `watchdog` (empty, or the watchdog that reset the board), `root` (ok, degraded, resync_started, resync_failed; translated) | normal / high | text |
| `emergency_mode` | `emergency` | `failed` (units, may be empty), `addresses` (may be empty) | urgent | text |
| `fan_failure` | `hw` | `source` (microcontroller, hwmon); `fan` (fan, cpu_fan; translated) for the microcontroller, `fans` for hwmon | urgent | text |
| `fans_ok` | `hw` | — | normal | — |
| `thermal_shutdown` | `hw` | `reason` | urgent | text |
| `shutdown` | `hw` | `button` (power_button; translated), `command` | high | text |
| `dom_update_failed` | `dom` | `error` | high | text (the error) |
| `dom_default_debian` | `dom` | `kernel` | high | text |
| `kernel_not_tried` | `dom` | `kernel` | normal | text |
| `kernel_boot_failed` | `dom` | `running` | high | text |
| `ups_safe_mode` | `ups` | `reason` (lowbatt, nocomm, waittimeup, fsd; translated), `battery` (%), `runtime` (s) | urgent | text |
| `ups_power_restored_reboot` | `ups` | — | high | text |
| `ups_power_restored` | `ups` | `battery` | normal | text |
| `ups_on_battery` | `ups` | `battery`, `runtime` | high | text |
| `ups_battery_low` | `ups` | `battery`, `runtime` | urgent | text |
| `ups_unreachable` | `ups` | `ups`, `minutes` | high | text |

A new synodeb event needs only its keys in the Synology module's dictionary
(`crates/rynas-ui/src/modules/synology/i18n.rs`):
`nev.synodeb.<event>.title` (required for a translation), optionally `.body` (text messages
only), variants `.title_2`/`.body_2`/`.body_3` and value keys; rynasd picks them up for the
Italian channels at build time. Until then the English title and message are shown. In the list
the source of every synodeb event reads "Base system" (the raw `synodeb:<source>` stays in the
detail drawer).

Category from the source: smart/mdadm/raid/btrfs/disk/storage → `storage`, ups/nut → `ups`,
synohwd/hw/fan/thermal/temp → `hardware`, dom/trial/kernel/apt/update/upgrade → `update`, else
`system`. Every forward is a `oneshot` event with key `synodeb:<source>:<first 16 hex digits of
sha256(title + "\n" + message)>`: the same title and message within 10 minutes are counted, not
sent again (synodeb's `REPEAT_MINUTES` suppression, now in rynas; `force` bypasses it). rynas'
rules then decide where it goes (MIN_PRIORITY becomes the rule's minimum severity on import).

**Answer**: `202` with `{"id": <n>}` (stored) or `{"id": null}` (duplicate within the window):
both mean "rynas owns it, do not send it yourself". Any other status or a connection error means
"fall back to your channels": `400` with `{"code": "bad_request", "message": ...}` (no title,
wrong content type, bad JSON), `413` (body too large), `500`. `GET /v1/health` answers
`{"version": ...}` without side effects.

**Suggested synodeb change** (`/usr/lib/synodeb/notify`, after the logger line and before the
MIN_PRIORITY check, so that rynas sees every priority):
```sh
SOCK=${RYNAS_NOTIFY_SOCKET:-/run/rynas/notify.sock}
if [ -S "$SOCK" ] && [ "${FORWARD:-auto}" != off ]; then
    if curl -fsS --max-time 5 --unix-socket "$SOCK" -o /dev/null \
        --data-urlencode "title=$title" --data-urlencode "message=$msg" \
        --data-urlencode "priority=$prio" --data-urlencode "source=${source:-notify}" \
        -d "force=$force" http://rynas/v1/notify; then
        exit 0
    fi
    logger -t synodeb-notify -- "rynas did not accept the notification: using own channels" 2>/dev/null || true
fi
```
(`-s <source>` sets `source`; `FORWARD=off` in `notify.conf` keeps the old behaviour.)

**notify.conf import**: on its first start rynasd's Synology module reads
`/etc/synodeb/notify.conf` below the configuration root (parsed as `KEY=value` lines, never
sourced) and creates the targets
"E-mail (synodeb)" (EMAIL_TO, EMAIL_FROM, SMTP_HOST/PORT/TLS/USER, the first line of
SMTP_PASSWORD_FILE), "Telegram (synodeb)" (TELEGRAM_TOKEN, TELEGRAM_CHAT_ID) and "ntfy
(synodeb)" (NTFY_URL split into server and topic, NTFY_TOKEN), plus a rule "Imported from
synodeb" with MIN_PRIORITY as minimum severity. It runs once (a marker in `notify.db`); later
edits of `notify.conf` only matter for synodeb's own fallback.

## Facts from the synodeb review (2026-11-05)
- DS920+ is supported (kernel 7.3): SSH install, DOM boot, network, power and bay presence, RTC, fan
  speed and curves, CPU/disk temperatures, power and disk LEDs, power and reset buttons. Missing: DX517
  on eSATA and the iGPU (no DRM in the slim kernel). 4 bays, 2 fan tachometers. `status.json` has the
  same format on both models.
- Installed system network: **ifupdown** (`/etc/network/interfaces` from d-i netcfg) with predictable
  `enp*` names; networkd/`lan1` only in the prebuilt image. rynas must adopt ifupdown read-only and
  offer the guided migration (D-NET).
- Root: d-i creates only the `@rootfs` btrfs subvolume; `synodeb-dom` adds `rootflags=subvol=@rootfs`
  from `/etc/fstab`. The reference setup will use **native btrfs RAID1 on the two NVMe** (not mdadm);
  booting with a failed NVMe needs `rootflags=degraded` — rynas should surface this state and the fix.
- No serial console enabled on the DS920+ yet (LPSS UART 2, to be verified on hardware).
- RAM, TPM and AES-NI on the test unit still to be checked.
- Hooks for S3 need a hard timeout (battery time matters).

## Open questions
- Where synodeb's root layout will live long-term (`@rootfs` subvolume; reference setup: DS920+, system on two NVMe in
  native btrfs RAID1) and which extra subvolumes (`@var_log`, `@snapshots`) the installer should
  create for clean restore points.
- Whether rynas should be offered by `synodeb-setup.sh` as an optional component after install.
