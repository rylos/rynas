# rynas: notifications, UPS and automatic updates

Context: synodeb components analyzed (`pkg/synodeb` in [synodeb](https://github.com/rylos/synodeb)): `notify`, `synodeb-ups`, `synodeb-dom`, `synodeb-trial`, `boot-report`, `52synodeb-unattended-upgrades` and the NUT templates.

## 0. What synodeb already does (not to be redone)

| Area | synodeb implementation | Hook point for rynas |
|---|---|---|
| Notifications | A single script `/usr/lib/synodeb/notify [-p low/normal/high/urgent] [-f] TITLE [MSG]`. Channels: e-mail (msmtp), Telegram, ntfy. Configuration in `/etc/synodeb/notify.conf` with `MIN_PRIORITY` and `REPEAT_MINUTES` (dedup on a hash of title+message). Secrets are passed to curl via stdin. | Called by smartd (`run.d/50synodeb-notify`), mdadm, synohwd (fans, thermal shutdown), a global `OnFailure=` on all services (`service.d/10-synodeb-onfailure.conf`), boot-report (boot, unclean boot, watchdog reset), synodeb-ups and synodeb-dom |
| UPS | NUT configured by `synodeb-ups setup`: upsmon primary, upssched with ONLINE, ONBATT, LOWBATT, NOCOMM and FSD events. Safe Mode = `systemctl isolate synodeb-ups-safe.target`: services stop, volumes are unmounted, disks go into standby, the NAS **never powers off** and restarts after 2 OL readings. `NETWORK_CLIENTS` opens port 3493 via nftables. Options: `DRYRUN`, `NOCOMM_SAFEMODE`, `SAFESHUTDOWN`. "Safe Mode after N minutes" exists only as a commented-out line in `upssched.conf` (`START-TIMER waittimeup`) | everything is already there: rynas must act as the interface, not as a second manager |
| Updates | unattended-upgrades on Debian, Debian-Security and trixie-updates. synodeb kernels and the `synodeb` package are excluded. `Automatic-Reboot "false"`, `MinimalSteps`. Pin -1 on all Debian kernels | rynas reads and respects this policy |
| Kernel | Never installed automatically, because it writes to the DOM. Kernel trial (`synodeb-trial` in the initramfs, `synodeb-boot-ok.service`): if the new kernel hangs or panics, the next boot uses DEBIAN_OLD and a notification is sent | rynas drives the reboot and shows the outcome; the snapshot research already flagged the "root rollback ↔ kernel on the DOM" trap |

**Principle**: on synodeb, rynas is the **interface and orchestrator**, while synodeb remains the **hardware executor**. On a generic Debian (without synodeb) the rynas plugins use their own backend, with the same APIs.

---

## 1. Notifications

### Comparison

| Product | Channels | Routing | Severity | Dedup/rate | UI center | Templates |
|---|---|---|---|---|---|---|
| **DSM 7** | e-mail, SMS, push (DS app), custom webhooks | event × channel table | per event | minimal | yes (desktop) | fixed variables |
| **TrueNAS** | AWS SNS, e-mail, InfluxDB, Mattermost, OpsGenie, PagerDuty, Slack, SNMP trap, Telegram, VictorOps | minimum level per service | Info…Emergency (7 syslog levels) | per alert class: immediately/hourly/daily/never (to be confirmed) | yes, with **dismiss**; the alert clears itself when the condition goes away | no |
| **Proxmox VE 8.1+** | sendmail, SMTP, Gotify, **webhook** (GET/POST/PUT, headers and body with Handlebars, secrets) | **matchers**: `match-severity`, `match-field` (exact/regex on metadata), `match-calendar`, `mode all/any`, `invert-match`. Each target receives a given notification only once | info, notice, warning, error, unknown | none (no grouping) | no | Handlebars, overridable in `/etc/pve/notification-templates/` |
| **Unraid** | agents: Discord, Pushover, Telegram, Slack, ntfy, Pushbullet… | per level | normal, warning, alert | — | yes (browser) | no |
| **OMV 7** | e-mail only (Postfix satellite) | per-category toggles | — | — | no | no |
| **Uptime Kuma** | 90+ providers, including Apprise | per monitor | — | "only on state change" + "resend every N checks" | yes | partial |
| **synodeb** | e-mail, Telegram, ntfy | global minimum priority | 4 levels | 30 min dedup | no | no |

### Libraries

- **Apprise** (MIT): 150+ services, each described by a URL (`tgram://`, `ntfys://`, `discord://`, `mailtos://`…). Two ways to use it: as a Python CLI (`apprise -t T -b B URL`) or via **apprise-api** in docker (stateless `POST /notify/` with `urls`, or `/notify/{KEY}` with stored configuration and tags).
- **Shoutrrr**: Go, similar URL format, about 20 services. Not needed; it only makes sense inside Go.
- **lettre**: mature Rust SMTP (STARTTLS/TLS, auth). Fine for native e-mail.
- **ntfy and Gotify**: a single HTTP POST. A few lines with `reqwest`.

### Best ideas to adopt

1. **Targets + matchers** from Proxmox: the cleanest model; it separates "where" from "what" and allows adding channels without touching the rules.
2. **Stateful alerts** from TrueNAS: the condition opens and closes on its own, with manual **dismiss**; type and level are defined by the emitter.
3. **Only on state change, with reminders** from Uptime Kuma and from [btrbk-nas](https://github.com/rylos/btrbk-nas) (the daemon from the snapshot research): a reminder every 24 h while the condition stays open.
4. **Event × channel table** from DSM: the "Advanced" view for users who do not want to write rules.
5. **Webhooks with Handlebars templates and secrets** from Proxmox: covers Discord, Slack, Teams and Home Assistant in one go.
6. **Test send** for each channel (every product has it).

### Mistakes to avoid

- Depending on a local MTA for e-mail: the OMV and cron problem. Better to use direct SMTP with lettre, with its own queue and retries (Proxmox SMTP does not retry).
- Sending notifications without dedup: a flapping disk generates 500 e-mails.
- Muting everything at night: quiet hours must **never** hold back `critical` messages (UPS, degraded RAID).
- Forgetting that web push requires a **secure context** (HTTPS with a valid certificate) and goes through the browser's push service, so it needs internet access. On iOS it only works with the PWA installed. So it is an additional channel, not the primary one.
- Putting tokens in plaintext in the exported configuration. D-CFGBAK already puts them in `secrets.age`.

### Recommendation: native notification engine in `rynasd`, with Apprise as an optional channel

**Data model (SQLite)**
```
Event   { id, ts, source: "storage.raid"|"ups"|"plugin:frigate"…, category: storage|hardware|ups|
          system|security|update|backup|app|task, severity: info|notice|warning|error|critical,
          key (dedup/state, e.g. "raid:md0:degraded"), state: firing|resolved|oneshot,
          title, body, fields{…}, actions[{label,url}] }
Target  { id, kind: smtp|ntfy|gotify|telegram|webhook|webpush|apprise|ui, config, secret_ref }
Matcher { id, severity>=, category in, source regex, field match, calendar (quiet hours),
          mode all|any, invert, targets[], group_window (e.g. 2 min), repeat_every (e.g. 24h) }
```

**Features**
- Typed events with a `key`. Notifications fire only when the state moves between firing and resolved, with `repeat_every` reminders while it stays firing.
- Grouping: events of the same category arriving within `group_window` go out as a single message.
- Dedup by `key`.
- Quiet hours are a `match-calendar` with a forced exception for `critical`.
- MiniJinja or Handlebars templates per target, with English and Italian defaults.
- Notification center in the UI: bell with counter, filterable list, *acknowledge*, *mute this key for 1h/1d/forever*, link to the action. Updates arrive via SSE, as per D-RT.
- Plugins: call `notify.emit(Event)` via JSON-RPC. The `plugin:<id>` source is set by the core and cannot be spoofed. Plugin categories are declared in the manifest.
- **Apprise**: an `apprise` target that runs `apprise` (the Debian package, if present; the trixie version needs checking) or POSTs to a user-supplied apprise-api. It covers the long tail of services (Discord, Pushover, Teams, Matrix, SMS) without having to maintain them. It is **not** a dependency: the 5 native channels cover 90% of cases and do not pull in Python.

**synodeb integration** (requires a small change to synodeb, to be agreed in both repos)
- synodeb's `notify` gains a `-s source` option. When `/run/rynas/notify.sock` exists (or `FORWARD=rynas` is set in `notify.conf`), the script POSTs the event to rynasd and exits. If the socket does not respond, it uses its own channels: this way the **fallback is guaranteed even with rynas stopped**, which is essential during Safe Mode and at boot.
- On first start rynas imports `notify.conf` into native targets (SMTP, Telegram, ntfy).
- synodeb's `OnFailure`, smartd, mdadm and boot-report stay where they are: rynas does not register duplicate hooks. rynas emits its own events (btrfs, snapshots, tasks, docker, updates).

**Mockup**
```
Notifications ─────────────────────────────────────── [🔔 3]
 ● CRIT  storage  md0 degraded (sdb removed)     2 min ago [Ack] [Mute ▾]
 ● WARN  ups      On battery 87% · 41 min        5 min ago [Ack]
 ○ INFO  update   12 security updates installed  03:14     ✓ resolved
Channels [+ Add ▾ SMTP · ntfy · Gotify · Telegram · Webhook · Web push · Apprise]
 Rules:  "Critical everywhere" sev≥error → mail, ntfy      [Test]
         "Quiet night" 23–07   sev<error → UI only
```

**Phases.** **1**: engine, SMTP, ntfy, Telegram, UI center, dedup, state changes, synodeb forward. **2**: Gotify, templated webhooks, full matchers, quiet hours, grouping, Apprise, plugin events, event × channel table. **3**: web push (VAPID), actions from notifications, daily digest, SNMP traps for business.

---

## 2. UPS

### Comparison

| | DSM | TrueNAS | OMV | Unraid | synodeb |
|---|---|---|---|---|---|
| Engine | built-in NUT | NUT | NUT (plugin) | apcupsd by default, NUT via plugin | NUT 2.8.1 (trixie) |
| Connection | USB, SNMP, network server | USB, SNMP, network | same | USB, network | USB + network client |
| Policy | Safe Mode after N minutes or on low battery | shutdown after N seconds or on low battery | shutdown | battery or runtime threshold | Safe Mode on LB/FSD/NOCOMM, "after N minutes" commented out |
| Power-on again | yes, because the NAS stays on in Safe Mode | depends on BIOS | BIOS | BIOS | yes (Safe Mode + restart) |
| Network server | yes, "permitted Synology devices" (IP) | yes | yes | yes | yes, `NETWORK_CLIENTS` + nftables |
| Dashboard | minimal status | widget | minimal | widget | CLI `synodeb-ups status` |

**PeaNUT** (Brandawg93, v5.22, login enabled by default) is the best example of a NUT UI: charge, runtime, load, history chart and instant commands. It serves as the reference for the widget, not as a dependency.

**NUT 2.8 on Debian 13**: package `2.8.1-5`. Roles are named `primary` and `secondary` (master/slave is deprecated). `nut-scanner` detects USB, SNMP, XML and networked NUT. `upsc -c ups@host` lists connected clients. `upscmd` is used for `test.battery.start.quick|deep`, only with an upsd user that has `instcmds`. In the FSD sequence, the `primary` waits for secondaries up to `HOSTSYNC` (15 s), then `FINALDELAY`. Caveat: if a secondary's FINALDELAY exceeds the primary's HOSTSYNC, the primary stops waiting for it.

### Mistakes to avoid

- **Shutting down a NAS that then does not power back on**: the classic TrueNAS, OMV and Unraid problem. Without "power on after AC loss" in the BIOS, the NAS stays off even if power returns before the battery runs out. The DSM and synodeb Safe Mode solves exactly this: it must be kept.
- Having two managers of `/etc/nut`. On synodeb rynas does **not** rewrite the NUT files: it calls `synodeb-ups setup`.
- Leaving upsd open on the network without a firewall. synodeb ties it to nftables: good.
- Scheduling the battery test without notifying the result, or during a backup.
- No shutdown order: with synodeb Safe Mode, `isolate` stops everything in no particular order, and a database in a container may shut down uncleanly.

### Recommendation: thin rynas `nut` plugin with two backends

**On synodeb (`synodeb` backend)**

The plugin reads `/etc/synodeb/ups.conf` and the status via `upsc`. For writes it goes through rynasd, which edits `ups.conf` (`NETWORK_CLIENTS`, `NOCOMM_SAFEMODE`, `SAFESHUTDOWN`, `DRYRUN`) and runs `synodeb-ups setup --keep-password` and `synodeb-ups passwd`. Three things need to be added to synodeb, and they remain synodeb's:

1. `SAFEMODE_AFTER=<sec>` in `ups.conf`, replacing the commented-out upssched line (DSM's "Safe Mode after N minutes").
2. A hook directory `/etc/synodeb/ups-pre-safemode.d/`, executed with a timeout before the `isolate`. rynas puts `rynas-graceful-stop` there: Incus/VMs with `stop --timeout`, then docker stacks in reverse priority order, then running tasks. Each step has its own timeout and the total is bounded by a runtime threshold.
3. An `admin` NUT user with `instcmds = test.battery.start.quick, test.battery.start.deep` in `upsd.users`, for the scheduled test.

**On generic Debian (`native` backend)**

rynas writes `/etc/nut/*` (templates like synodeb's) and uses a classic shutdown (`SHUTDOWNCMD poweroff`). The UI warns: *"check in the BIOS: Restore on AC power loss = On"*.

**Common to both backends**

- Automatic USB detection (udev rule, falling back to `nut-scanner -U`) and a 3-step wizard: detect, policy, network clients.
- Dashboard widget: OL/OB/LB status, charge %, runtime, load %, input voltage, 24h/30d history (sampled every 30 s into the existing metrics store).
- Policy: until low battery (default) or after N minutes; action Safe Mode (synodeb) or shutdown; shutdown order editable via drag & drop.
- Network server: list of allowed IPs plus **currently connected clients** (`upsc -c`); UPS name `ups`, user `monuser`, compatible with DSM clients.
- **Client** mode towards another NUT server (DSM, another rynas): `secondary`.
- Battery test as an internal Task Scheduler task (D-TASKS), e.g. monthly, with the result read from `ups.test.result`.
- Events to the notification engine (`category=ups`, key `ups:onbatt` firing/resolved). On synodeb they arrive via the `notify` forward, without duplicates.

```
UPS ── APC Back-UPS 950 (usb) ── ● Online ───────────────────────────────
 Charge ███████████░ 96%   Runtime 38 min   Load 22%   230 V
 [24h charge/load chart]
 Policy    (•) on low battery  ( ) after [5] min   Action: Safe Mode (synodeb)
 Shutdown order  1 VMs  2 apps: immich, frigate  3 remaining docker   timeout 120 s
 Network server ☑  Allowed IPs: 192.0.2.10, 192.0.2.0/28   Connected: 192.0.2.10 (HA)
 Battery test  monthly, 1st Sun 04:00   last: OK (2026-10-05)     [Test now]
```

**Phases.** **1**: status reading and widget, events to notifications, wizard on top of synodeb-ups. **2**: history, shutdown order (synodeb hook), network server with clients, scheduled test, native backend. **3**: SNMP (rack UPS), multiple UPSes, instant commands and outage statistics.

---

## 3. Automatic updates

### Comparison

| | System | Automation | Rollback | Reboot | Notes |
|---|---|---|---|---|---|
| **DSM 7** | DSM block + packages | "notify" / "install important updates only" / "always install", with a check schedule (described from memory: KB page not readable) | no | automatic if needed | packages have separate auto-update |
| **TrueNAS 25.10+** | image | **profiles** (Developer, Early Adopter, General, Mission Critical) instead of trains; automatic download, manual install | **boot environment** | manual | the safety model is the image |
| **Proxmox** | apt | **no auto**; `package-updates` notification (info) via matcher | no | manual | enterprise and no-subscription repos |
| **OMV** | apt | `omv-upgrade`, notice in the UI | no | manual | — |
| **Unraid** | OS image | manual; CA Auto Update for plugins and docker | previous OS | manual | — |
| **Debian u-u** | apt | origins, `Package-Blacklist`, `Automatic-Reboot(-Time/-WithUsers)`, `MinimalSteps`; `apt-daily` and `apt-daily-upgrade` timers | no | optional | `needrestart -b` (machine-readable, with kernel status) and `apt-listchanges` for the changelog |
| **Livepatch** | — | Debian has no official livepatch service; on a custom synodeb kernel it makes no sense | — | — | discarded |

### Best ideas

1. **Profiles** from TrueNAS: a few understandable choices instead of apt options.
2. **Notify without auto-install** for risky things, from Proxmox.
3. **Restore point before** (already decided in D-SNAP) + **health check after**: the rynas take on a "boot environment".
4. **Kernel trial with fallback**, already done by synodeb: rynas orchestrates it and shows the result.
5. **Maintenance window** from DSM, rebooting only when needed.

### Mistakes to avoid

- Automatic reboot without checks: a synodeb kernel update writes to the DOM and is excluded **by design**.
- One snapshot per `apt-daily-upgrade` run without retention. Better one snapshot per maintenance window, keeping the last N (e.g. 5) plus the pre-major one.
- Two managers of the same policy, i.e. a second rynas u-u file contradicting `52synodeb-…`.
- Rolling back the root while leaving the new kernel on the DOM: it gets out of sync with `/lib/modules`, as already noted in the snapshot research.
- Updating docker apps to the `latest` tag without snapshotting their data: Frigate and Immich change configuration at every minor release (photos-nvr research).
- A forward schema migration of `config.db`: after a rollback an older rynas cannot read it. That is why `config.db` stays in the root and rolls back together with the binaries.

### Recommendation: three channels, one window, one orchestrator in rynasd

**Channel A, Debian system (security)**
- Default profile **"Security + fixes only (automatic)"**: synodeb's origins stay as they are. rynas does **not** write another u-u configuration: it reads `52synodeb-unattended-upgrades` and only adds `53rynas-window` for the timers.
- Orchestration: rynas disables `apt-daily-upgrade.timer` (keeping `apt-daily` for downloads) and runs the internal *System update* task inside the window (e.g. Sun 03:00, or daily for security only).
- Task sequence:
  1. **pre-flight check**: free space, RAID not degraded, UPS online, no backup running;
  2. **restore point**: btrbk `@rootfs` + export of `config.db` (D-CFGBAK);
  3. `unattended-upgrade`;
  4. `needrestart -b -r a`: services restart immediately, the kernel status is read;
  5. **health check**: rynasd, rynas-web, smbd, nfs, docker and failed units;
  6. notification with a summary.
- Alternative profiles: "Notify only" (Proxmox style) and "All trixie updates".

**Channel B, rynas and plugins (rynas apt repo)**
- Default: **notify**, with the option "automatically install patches (x.y.**z**) in the window". Minor and major releases stay manual. Plugin `api_version` check before updating (roadmap).
- Stable/LTS channels (D-BIZ), like TrueNAS profiles: *Stable* (default), *LTS* (business), *Testing*.

**Channel C, docker apps**
- As per D-DOCKER and D-APPCAT: digest comparison without pulling, then notification.
- For catalog apps: one-click update that snapshots the stack and data subvolumes, then pull and up, healthcheck, and automatic rollback on failure.
- Optional per app: "auto for patches" in the window. If Dockhand is present, delegate to it (decision already made).

**Reboot**
- Needed when `needrestart` reports that the kernel has changed, or for packages that require it (glibc, systemd).
- Policy: *never automatic* (default) / *in the next window* / *right after the update*. Countdown warning in the UI, and no reboot while tasks, scrub or resync are running.

**Kernel on synodeb**
- Always **manual** (synodeb rule). rynas shows "kernel 6.18.x available" with the changelog and an **"Install and reboot in the window"** button.
- After the reboot rynas reads `synodeb-dom status`. If the trial failed, the system runs on DEBIAN_OLD and synodeb has already notified: rynas shows a banner ("new kernel did not boot, previous one in use") with "Retry" (`default debian`) and "Remove".
- The rynas **root rollback** must call `synodeb-dom default debian-old`, or a `sync --root` to be agreed, when the snapshot predates the kernel installation.

**Never automatic**
- Debian major releases (trixie → forky): a separate wizard, with a restore point.
- Docker major releases (hold, as in D-DOCKER), Incus 6 → 7.
- synodeb kernel; rynas major releases; apps with the `breaking-updates` antifeature.

**Pinning**: "Held packages" page (`apt-mark hold` and entries in rynas `preferences.d`). synodeb's pin -1 on Debian kernels is shown read-only.

**Changelog**: `apt-listchanges --which=news` collected before installation and shown in the UI. Critical NEWS entries block the automatic update and trigger a notification.

```
Updates ───────────────────────────────────────────────────────────────
 Debian system    ● auto (security+fixes)   last: today 03:14 ✓ 12 packages
 rynas & plugins  ○ notify   [3 available ▸ changelog]   channel: Stable
 Docker apps      immich 2.3.1→2.3.2 [Update]  frigate 0.18→0.19 ⚠ breaking
 synodeb kernel   6.18.9 → 6.18.11   [Install and reboot in the window]
 Window  Sun 03:00–05:00   Reboot: (•) in the window  ( ) never
 ⚠ Reboot required (libc6, kernel)   [Reboot now] [Schedule]
 Restore points: 2026-11-03 03:10 pre-update  [Restore…]
```

**Phases.** **1**: status page (u-u active, last result, reboot required, kernel available), notifications, changelog. **2**: orchestrator in the window (pre-flight check → snapshot → u-u → needrestart → health check), profiles, scheduled reboot, synodeb kernel guidance with trial reading, channel B, pinning. **3**: automatic rollback after a failed health check (with synodeb-dom coordination), automatic patches for apps, Debian major-release wizard, LTS channel.

---

## Summary

| Topic | rynas does | rynas does not | Reuses from synodeb | Changes to request from synodeb |
|---|---|---|---|---|
| Notifications | event/target/matcher engine, UI center, 5 native channels + optional Apprise, its own and plugin events | local MTA, duplicate hooks on smartd/mdadm/OnFailure | `notify` with all its callers; import of `notify.conf` | `-s source`, forward to `/run/rynas/notify.sock` with local fallback |
| UPS | `nut` plugin: UI, wizard, history, shutdown order, network clients, scheduled test | rewriting NUT or Safe Mode on synodeb | `synodeb-ups`, Safe Mode, nftables, power-on again | `SAFEMODE_AFTER`, `ups-pre-safemode.d` hooks, NUT admin user with instcmds |
| Updates | orchestrator with window, snapshot → u-u → needrestart → health check, channels B/C, kernel guidance | auto-updating the synodeb kernel, Debian major releases, Docker major releases | u-u policy, blacklist, pins, kernel trial | stable `synodeb-dom status --json` interface and the coordinated rollback command |

## Sources

- Proxmox notifications: https://pve.proxmox.com/wiki/Notifications
- TrueNAS alert services 25.04: https://www.truenas.com/docs/scale/25.04/scaleuireference/systemsettings/alertssettingsservicescreen/ · alert settings: https://www.truenas.com/docs/scale/25.04/scaleuireference/toptoolbar/alerts/alertsettingsscreen/
- TrueNAS 25.10 update profiles: https://www.truenas.com/blog/smarter-and-streamlined-updates-with-truenas-25-10
- Apprise API: https://github.com/caronc/apprise-api · Apprise: https://github.com/caronc/apprise
- OMV notifications: https://docs.openmediavault.org/en/7.x/administration/general/notifications.html · Mailrise (SMTP → Apprise bridge): https://pypi.org/project/mailrise/
- Unraid agents (indirect source): https://ca.unraid.net/apps/u-manager-push-notifications-0eryqfx1olt49d · Apprise-go for Unraid: https://forums.unraid.net/topic/196880-introducing-apprise-go-universal-notifications-in-a-single-binary/
- Uptime Kuma notification methods: https://github.com/louislam/uptime-kuma/wiki/Notification-Methods
- DSM UPS: https://kb.synology.com/en-id/DSM/help/DSM/AdminCenter/system_hardware_ups?version=7 · DSM update: https://kb.synology.com/en-global/DSM/help/DSM/AdminCenter/system_update?version=7
- NUT on trixie: https://packages.debian.org/trixie/nut-server · upsmon.conf: https://dyn.manpages.debian.org/trixie/nut-client/upsmon.conf.5.en.html · FSD/HOSTSYNC thread: https://alioth-lists.debian.net/pipermail/nut-upsuser/2023-October/013450.html
- PeaNUT: https://github.com/Brandawg93/PeaNUT
- Debian UnattendedUpgrades: https://wiki.debian.org/UnattendedUpgrades
- lettre crate: https://crates.io/crates/lettre

**To verify before phase 2:**
- `apprise` version in the trixie packages;
- exact TrueNAS alert frequency values;
- DSM 7.2 update options (the KB page did not load);
- whether `upsc -c` works with synodeb's `monuser` or requires a different user.
