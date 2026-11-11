# Date, time and time zone (D-TIME)

System › Date and time: the time zone, automatic sync with time servers, the clock's state, a
manual setting while sync is off, and notifications when the clock is not synchronized. Status
2026-11-11: implemented (API, page, notifications); not yet tried on real hardware (§7).

## 1. Research

### How mature systems do it
- **Synology DSM** (Control Panel › Regional Options › Time): time zone drop-down; "Synchronize
  with NTP server" with a server field and *Update now*; "Manually" with date/time fields and
  "Synchronize with computer"; a separate *NTP Service* tab turns the NAS into a time server for
  the LAN. A NAS left on "Manually" is the usual reason it misses daylight saving changes.
- **TrueNAS SCALE** (System › General): a *Localization* widget (time zone with a type-to-filter
  list, date/time formats) and an *NTP Servers* widget (several default pool servers; per server
  iburst/burst/prefer/min-max poll; advice: no burst for public servers, prefer only for
  hardware-backed ones). chrony underneath; serving the LAN is a hand edit.
- **openmediavault** (System › Date & Time): time zone, "Use NTP server", a server list; chrony
  underneath, `allow` for clients only through extra configuration.
- **Proxmox VE**: time zone in the node's *System › Time*; NTP is chrony (since PVE 7), servers
  only by editing `chrony.conf`.
- **Cockpit** (Overview › System time): time zone, "Automatically using NTP" / "using specific
  servers" / "manually"; everything through timedated, servers written for the daemon in use
  (an old request asked for a server list independent of the daemon, closed in cockpit 288).

Common shape, kept here: time zone list filtered by typing, automatic sync on/off with an
optional own server list, a manual setting only when sync is off, "use this computer's time",
a "sync now" button, the clock's state. What rynas adds: notifications (unsynchronized, drift)
and the hardware-clock warning.

### Debian 13 specifics
- **Daemon.** `systemd-timesyncd` (SNTP client only) is the default client of a Debian install;
  chrony is common on cloud images and servers that serve time. They provide the same virtual
  package `time-daemon` and conflict: exactly one is there, or none (minimal containers).
  The classic `ntp` package is gone; ntpsec and OpenNTPD (both `/usr/sbin/ntpd`) provide
  `time-daemon` too. `systemd-timesyncd` declares `Conflicts: time-daemon` (Debian's
  `debian/control`), so installing it with `apt-get -y` would remove ntpsec.
- **timedated** (`timedatectl`, D-Bus `org.freedesktop.timedate1`): `set-timezone` (writes the
  `/etc/localtime` symlink), `set-ntp` (enables/starts the first unit of
  `/usr/lib/systemd/ntp-units.d/*.list`, timesyncd on Debian), `set-time` (refused while NTP is
  on; accepts `@<epoch>` and `YYYY-MM-DD HH:MM:SS` in the system zone), `set-local-rtc`.
  `timedatectl show` gives `Timezone`, `LocalRTC`, `NTP`, `NTPSynchronized` (the kernel's
  `STA_UNSYNC`, valid for both daemons).
- **timesyncd**: `NTP=` and `FallbackNTP=` in `/etc/systemd/timesyncd.conf` and drop-ins
  `timesyncd.conf.d/*.conf` in `/etc`, `/run` and `/usr/lib`, read in file name order (60–90
  for local files; `NTP=` lines add up, an empty `NTP=` resets the list read so far). The
  per-interface servers networkd got from DHCP (`LinkNTPServers`) are asked first, then the
  `NTP=` list, then `FallbackNTP=` (Debian: `0-3.debian.pool.ntp.org`) when none answers.
  Debian's dhclient hook (`/etc/dhcp/dhclient-exit-hooks.d/timesyncd`, ifupdown systems) is
  different: it writes DHCP's servers as `NTP=` into `/run/systemd/timesyncd.conf.d/01-dhclient.conf`,
  so they are part of the `NTP=` list and a later empty `NTP=` drops them. Its D-Bus
  interface `org.freedesktop.timesync1` gives `ServerName`, `ServerAddress (iay)`,
  `NTPMessage (uuuuittayttttbtt)` (leap, version, mode, stratum, precision, root delay, root
  dispersion, reference, origin/receive/transmit/destination times in µs, spike, packet
  count, jitter), `PollIntervalUSec`, `FallbackNTPServers`, `LinkNTPServers`. No "sync now"
  command: a restart asks again at once.
- **chrony** (Debian `chrony.conf`): `pool 2.debian.pool.ntp.org iburst`, `sourcedir
  /run/chrony-dhcp` (DHCP servers), `sourcedir /etc/chrony/sources.d` (`*.sources`, reloaded by
  `chronyc reload sources`), `confdir /etc/chrony/conf.d`, `rtcsync`, `makestep 1 3`. Status:
  `chronyc -c -n tracking` (CSV: reference id, address, stratum, reference time, system time
  correction — positive means the clock is slow —, …, update interval, leap status; "Not
  synchronised" until a source is selected). On Linux chronyd clears the kernel's
  `STA_UNSYNC` only with `rtcsync` (`sys_timex.c`), so timedated's `NTPSynchronized` is no
  verdict for chrony; the leap status is. Sync now:
  `chronyc burst 4/4` then `chronyc makestep`. Serving the LAN: `allow <subnet>` (not done, §6).
- **RTC**: UTC is right on a server; local time only for dual boot with Windows, and it shifts
  by an hour around DST changes when nothing corrects it.
- **Who reads the zone when.** glibc reads `/etc/localtime` once per process for
  `localtime_r` (only `tzset()`/`localtime()`/`mktime()` look again); jiff caches the system zone
  for five minutes; PID 1 watches `/etc/localtime` and recomputes calendar timers; every new process
  reads the new zone. Containers that bind `/etc/localtime` keep the old inode (timedated
  replaces the symlink) until they are recreated.

## 2. Design

- **The system is the state.** Nothing in rynas's configuration (like zram and Tailscale): the
  zone and the hardware clock are timedated's, the server list is a drop-in of the daemon, sync
  is the daemon's unit. A configuration backup therefore does not carry the time zone (follow-up).
- **Daemon detection**: chrony when `/usr/sbin/chronyd` exists, else *another daemon* when
  `/usr/sbin/ntpd` exists (ntpsec, OpenNTPD: zone and hardware clock still editable, sync and
  servers shown read-only with "managed outside rynas", no Sync now; checked before timesyncd
  so rynas never drives or installs over it), else timesyncd when
  `/usr/lib/systemd/systemd-timesyncd` exists, else none (turning sync on then installs
  `systemd-timesyncd` from the archive, like zram's generator, with `apt-get install
  --no-remove`: apt refuses instead of removing a daemon rynas did not see).
- **Sync on/off**: timesyncd through `timedatectl set-ntp`; chrony with `systemctl
  enable|disable --now chrony.service` (timedated may not know chrony's unit). "On" for chrony is
  the unit being active or enabled (a failed chronyd is still "on", and unsynchronized), for
  timesyncd timedated's `NTP`; for another daemon timedated's `NTP` or an active
  `ntpsec`/`openntpd`/`ntp` unit.
- **Servers**: at most 8 host names or addresses (`validate::ntp_server`), one per line in the
  page. timesyncd: `/etc/systemd/timesyncd.conf.d/60-rynas.conf` with `NTP=` (reset) and
  `NTP=<list>`, written only when the administrator lists servers: the reset drops the `NTP=`
  of files read before (the main file, dhclient's `01-dhclient.conf`), so the page says that
  DHCP servers received that way are not used meanwhile; networkd's per-interface servers are
  still asked first (said too). chrony: `/etc/chrony/sources.d/rynas.sources` (`pool` for
  names containing "pool", else `server`, both `iburst`), then `chronyc reload sources`; a
  warning when neither `chrony.conf` nor `conf.d/*.conf` has `sourcedir /etc/chrony/sources.d`
  (the file would be ignored). Empty list: the file is removed and the distribution's and
  DHCP's servers apply. With chrony the sources of `chrony.conf` stay in use besides rynas's
  (rynas never edits a conffile; the page says so). The page lists the default and DHCP
  servers it can read (networkd's, dhclient's runtime drop-ins, `/run/chrony-dhcp`).
- **State** (`GET /system/time`): `timedatectl show`; timesyncd details with `busctl
  --json=short --auto-start=no get-property …` (never wakes a stopped timesyncd through D-Bus
  activation); chrony with `chronyc -c -n tracking` (synchronized = its leap status). Offset = ((T2−T1)+(T3−T4))/2 from the
  last NTP message for timesyncd, the correction for chrony; positive = the clock is behind.
- **Zones** (`GET /system/time/zones`): `tzdata.zi` zones and links in the IANA regions (Africa,
  America, …, Pacific) plus `UTC`, each with its offset now (jiff on the TZif file). Legacy links
  (`US/Eastern`, `Etc/GMT+5`), `right/…`, `posix/…` and `Factory` are not offered; the current
  zone always is. An apply accepts only an offered zone (or the current one) whose file is a
  TZif file. The parsed list (about 600 files) is kept until `tzdata.zi` or the zoneinfo folder
  changes.
- **Manual time** only while sync is off: `timedatectl set-time @<epoch>` ("use this browser's
  time", exact instant) or `timedatectl set-time "YYYY-MM-DD HH:MM:SS"` read in the system zone by
  timedatectl itself (DST gaps handled by systemd); only real dates from 2000 to 2099
  (`MIN_YEAR`/`MAX_YEAR`), the error on the field that was sent.
- **Hardware clock**: a warning when `LocalRTC=yes`, with "Keep it in UTC" (`timedatectl
  set-local-rtc 0`); never moved to local time by rynas.
- **After a zone change**, whoever made it (this page, `timedatectl`, an edit by hand),
  rynasd's local times follow at once: snapshot names, quiet hours of notifications and the power
  schedule read the zone through `rynas_tz::system_tz`, a cache of the parsed
  `/etc/localtime` keyed on its device, inode, size, modification time and link target, looked
  at again at most once a second (no `localtime_r`/`tzset`, no unsafe code). The file manager's
  archives run in a worker process per job, which reads the zone when it starts. Scheduler next runs come from systemd
  (`list-timers`, `systemd-analyze`), already correct. Running apps keep the old zone until they
  restart (help line under the zone).
- **Tasks**: `time.apply`, `time.set`, `time.sync`; one at a time (409 otherwise), the slot
  taken before the state is read and checked; audit lines. Sync now ends only when a server
  answered after the request (the last answer's time moves), at most 30 s.
- **Sandbox**: no change. timedated does the privileged writes (`/etc/localtime`, `/etc/adjtime`,
  the clock) outside rynasd, so `ProtectClock=yes` stays; the drop-ins are in `/etc`
  (writable under `ProtectSystem=yes`); D-Bus and chrony's socket are `AF_UNIX`.
- **Admin only**: the whole router behind `roles::admin_only` (reads too), CSRF from rynas-web.

## 3. API (rynasd `/v1`, rynas-web `/api/v1`)

| Method | Path | Body / answer |
|---|---|---|
| GET | `/system/time` | `ClockState` (now, zone and offset, local RTC, daemon, sync on, synchronized, rynas's / default / DHCP / dhclient-hook servers, servers ignored by chrony's configuration, source: server, address, stratum, offset µs, last sync, poll) |
| GET | `/system/time/zones` | `Vec<ZoneInfo>` (name, offset now) |
| POST | `/system/time` | `ClockSettings` (zone, sync, servers, local_rtc) → task `time.apply` |
| POST | `/system/time/set` | `SetClock` (`epoch_ms` or `local`) → task `time.set`; 409 while sync is on |
| POST | `/system/time/sync-now` | → task `time.sync` (waits up to 30 s for a new answer of a server); 409 while sync is off or managed by another daemon |

Types in `rynas_api_types::clock`; backend `crates/rynasd/src/clock.rs`; page
`crates/rynas-ui/src/pages/time.rs` (System › Date and time).

## 4. Notifications

Looked at every five minutes by rynasd (`clock::spawn_watch`):

| Code | Fields | When |
|---|---|---|
| `system.clock_unsynced` | `minutes` | sync on and the kernel has not counted the clock as synchronized for an hour (warning) |
| `system.clock_synced` | — | resolved: synchronized again |
| `system.clock_drift` | `offset` | sync on and more than 1 s from the source in two looks in a row, each on a new answer of the server (warning) |
| `system.clock_in_step` | — | resolved: back below 0.5 s |
| `system.clock_sync_off` | — | resolved *quietly* (`NewEvent::quiet`: kept in the list, sent to no target): sync turned off while one of the two was open |

While sync is off there is no alarm. A look below 1 s starts the drift count over; an open drift
alarm stays open between 0.5 and 1 s.

## 5. Not done, on purpose
- Product names (timesyncd, chrony) never appear in the page.
- 12/24-hour display: rynas has no per-user display preference place yet; times follow the
  browser language.
- Per-server options (prefer, poll limits, NTS): chrony only and rarely needed on a NAS.

## 6. Follow-ups
- **Serve time to the LAN** (DSM's NTP Service): chrony only (`allow <subnet>` in
  `/etc/chrony/conf.d/rynas-serve.conf`, restart), so with timesyncd it means replacing the
  daemon; UDP 123 and the LAN subnets belong with the firewall work. Not cheap enough for now.
- Time zone in the configuration backup (`.rynasbak`) and in the first-run wizard.
- NTS (`server … nts`) for chrony.

## 7. To try on real hardware
- Debian 13 with timesyncd: zone change (page, snapshot names, power schedule), servers drop-in
  and restart, sync off/on, manual time, sync now, `busctl` answers with timesyncd stopped.
- Debian 13 with chrony: `reload sources`, `burst`/`makestep`, `enable/disable --now`,
  tracking parse with an unreachable server, DHCP servers in `/run/chrony-dhcp`.
- A system without either daemon (install path).
- A machine with `LocalRTC=yes` (move to UTC).
- Notifications: unplug the network for over an hour with sync on.
