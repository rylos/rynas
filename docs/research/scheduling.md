# One common scheduling system for rynas: research and proposal

Date: 2026-11-16. Repo state: `main` at `21b3336`. Read-only research: no code changed.

Owner request: *"every scheduled activity in rynas (task scheduler, backups, snapshots, etc.) must share one serious common scheduling system"*.

**In short.** rynas already has a good core: the Task Scheduler (`rynas-scheduler` + `rynasd::scheduler`). It writes persistent systemd timers, records history in SQLite, streams live output, and has a calendar builder with a server-side preview. Snapshots, replication, restic backups, scrub, SMART, recycle bin cleanup, scheduled shutdown and dedup already run through it. But **at least 9 other recurring activities have their own clock**: rynasd loops, Debian's own timers, or the PC agent. They use **at least 7 different ways of writing a schedule** and **6 different UI widgets**. Missed runs, overlap, locks, jitter and history are each handled differently.

The proposal keeps systemd as the clock. It adds a rynasd **gate** that applies one set of run policies to every task: missed runs, concurrency, resource locks, pause, windows and retry. Every recurring *job* becomes a scheduler task with an owner. Feature pages embed one `ScheduleEditor`, and a new global **Schedule** page shows every job and window on a timeline, with conflicts marked.

---

## Part 1 — Inventory

Legend for "Executor": **ST** = rynas Task Scheduler, meaning a persistent unit `rynas-task-<id>.{service,timer}` written by rynasd (`crates/rynas-scheduler/src/units.rs:1-14`, `:155-183`). **Loop** = a tokio loop inside rynasd, rynas-web or the agent. **Deb** = a Debian-shipped systemd timer.

### 1.0 The common core that exists today

| Aspect | Today (Task Scheduler) | Where |
|---|---|---|
| Model | `ScheduledTask { id, name, enabled, action: TaskAction, trigger: TaskTrigger, timeout_s, nice, io_weight, notify, notify_output }` | `crates/rynas-api-types/src/scheduler.rs:23-52` |
| Trigger | `TaskTrigger::Calendar { on_calendar, cron?, persistent (default true), randomized_delay_s }`, `Boot { delay_s }`, `Manual` | `scheduler.rs:135-160` |
| Actions | user `Command`/`Script`. Internal `Snapshot`, `Replicate`, `Scrub`, `SmartTest`, `RecycleCleanup`, `PowerShutdown`, `Backup`, `Dedup` | `scheduler.rs:54-106` |
| Storage | config key `scheduler` (config.db, staged changeset, apply, revisions, drift) | `crates/rynasd/src/scheduler.rs:1-8` |
| Units | `.service` `Type=exec`, `RuntimeMaxSec`, `Nice`, `IOWeight`, `ExecStopPost=+rynasctl task-finished`. `.timer` has `OnCalendar` + `Persistent` + `RandomizedDelaySec` (or `OnBootSec`), `AccuracySec=1s`. Timers are enabled through a managed drop-in `timers.target.d/rynas-tasks.conf` | `units.rs:100-183`, `:186-200` |
| Internal action | the unit runs `rynasctl job run <id>`, which calls rynasd `execute`. rynasd starts a UPID task, and rynasctl follows its log until it ends | `crates/rynasd/src/jobs.rs:1-62`, `crates/rynasd/src/bin/rynasctl.rs:367-420` |
| Missed runs | `Persistent=true` per task, default on (a missed run happens once after boot) | `units.rs:165-171` |
| Overlap | systemd never starts a second instance of an active service. "Run now" on a running task is a no-op start | `crates/rynasd/src/scheduler.rs:462-492` |
| Preview | `POST /scheduler/tasks/preview-calendar` → `systemd-analyze calendar --iterations` (normalized form and next N runs). Cron is converted to OnCalendar | `crates/rynasd/src/scheduler.rs:560-605`, `crates/rynas-scheduler/src/calendar.rs` |
| History | `scheduler.db`: one row per run (invocation id, result, exit, last 256 KiB of output). 100 per task, 5000 in total | `crates/rynas-scheduler/src/history.rs:1-17` |
| Live output | SSE `/scheduler/tasks/{id}/live` (journal follow) | `crates/rynasd/src/scheduler_live.rs` |
| Notifications | `NotifyPolicy { Never, OnFailure, Always }` + optional output | `crates/rynasd/src/scheduler.rs:738-823` |
| Read-only views | other systemd timers (`/scheduler/system-timers`) and crontab lines (`/scheduler/cron-entries`) | `scheduler.rs:298-325` |
| UI editor | `TriggerEdit` (segmented Calendar/Boot/Manual; repeat = minutes/hourly/daily/weekly/monthly/custom/cron; weekday checkboxes; time; 300 ms debounced server preview; "Catch up missed runs" switch; "Random delay (seconds)") | `crates/rynas-ui/src/pages/scheduler.rs:28-430` |
| Resource lock | `FsLocks`: one lock per filesystem (`major:minor`). Heavy holders (scrub, replication, restic backup, storage ops) do **not** wait for each other. Only dedup gives way (SIGINT, saves its work) | `crates/rynasd/src/fslock.rs:1-14`, users `jobs.rs:37`, `replication.rs:624`, `restic_backup.rs:1578`, `storage_ops.rs:1239-1241` |
| Task queue | `rynas-tasks`: semaphore `max_concurrent`, `max_queued` 32 (global, not per resource) | `crates/rynas-tasks/src/lib.rs:35-48,120,180` |

The original analysis (`docs/research/scheduler-lxc.md` §1.5) already asked for "a single rynas Task Scheduler" with internal, user and external read-only tasks. What follows is what has drifted away from that.

### 1.1 Activities that already go through the Task Scheduler

| # | Activity | Schedule format | Stored in | Executor | Missed | Jitter | Overlap / locks | History / notify | UI widget |
|---|---|---|---|---|---|---|---|---|---|
| 1 | **Snapshot jobs** (btrbk) | `TaskTrigger`, default `hourly`, persistent | key `snapshots` (job) + `scheduler` (task `snapshot-<job>`) | ST → `snapshot.run` | Persistent | 0 | **no FsLock**. systemd skips if running | ST history, key `snapshot:job:<job>` | `TriggerEdit` in the job drawer (`pages/snapshots.rs:747`) |
| 2 | **Replication** targets | "after each snapshot run" (chained, `after_snapshots`), or own `TaskTrigger`, default `daily`, persistent | key `snapshots`/replication + task `replica-<id>` | ST, or inline after the snapshot run | Persistent | 0 | heavy FsLock + own lock retry (`replication.rs:602`) | ST history. When chained, its failures notify separately | `TriggerEdit` (`pages/replication.rs:456`) |
| 3 | **Restic NAS backups** | `TaskTrigger`, default `*-*-* 02:00:00`, persistent, **600 s random** | key backup + task `backup-<job>` | ST → `rynas-backup-worker` (transient unit) | Persistent | 600 s | heavy FsLock (`restic_backup.rs:1578`) | ST history + job status | `TriggerEdit` (`pages/backup/restic.rs`) |
| 3b | restic **prune / check** | "at most every N days" (`prune_every_days`, `check_every_days`): no clock of its own, piggy-backs on the backup run | job `maintenance` | inside the backup run | runs at the next backup after N days | — | as backup | job status | two number fields "Clean up every (days)", "Check every (days, 0 never)" (`restic.rs:383`) |
| 4 | **btrfs scrub** | `TaskTrigger` (user-created) | `scheduler` | ST → task `btrfs.scrub` (72 h timeout) | per task | per task | heavy FsLock | ST | Scheduler page only |
| 5 | **SMART self-test** | `TaskTrigger` (user-created) | `scheduler` | ST → `smartctl -t` (120 s, the test runs in the drive) | per task | per task | **none** (a long test can overlap a scrub on the same disk) | ST (only "started") | Scheduler page only |
| 6 | **Recycle bin cleanup** | `TaskTrigger`, retention from the share (`retention_days`, `api-types/services.rs:449`) | `scheduler` | ST | per task | per task | none | ST | Scheduler page |
| 7 | **Scheduled shutdown** (power schedule) | `PowerRule { days: Vec<Weekday>, hour, minute }` → `OnCalendar` (`rynas-power/src/rules.rs:129-138`), task `power-off-<rule>` | key `power.schedule` | ST → `rynasctl power scheduled-shutdown` | **persistent=false** (`rynasd/src/power.rs:142-146`) | 0 | busy checks: storage, tasks, SMB. Then `wait` up to `max_wait_min`, or skip (`api-types/power.rs:84-100`) | ST + power events | own weekday checkboxes + `<input type=time>` (`pages/hardware/power.rs:470-495`) |
| 7b | **Scheduled start-up** | same rules, action `Startup` → RTC `wakealarm` armed before every shutdown | `power.schedule` | firmware RTC (`rynas-power-wake.service`) | n/a | 0 | conflicts between rules refused (`rules.rs`) | — | same dialog |
| 8 | **Deduplication** | 168-cell weekly grid string (`api-types/dedup.rs:38-39,82-158`) → OnCalendar of the **cartesian product** of block-start days × hours (`rynas-dedup/src/window.rs:34-55`). The run itself checks whether it is really inside a block | key dedup + task `dedup-<key>` | ST → oans until the block ends (SIGINT) | **persistent=false** (`dedup/mod.rs:1002`) | 0 | yields to every heavy holder. Skips when the disks are asleep | own `dedup.*` events (`NotifyPolicy::Never`) | `week_grid` (`pages/storage/dedup.rs`). Read-only on the Scheduler page (`scheduler.rs:337-341` backend, UI `managed_href`) |

### 1.2 Activities with their own clock (outside the Task Scheduler)

| # | Activity | Schedule format | Stored in | Executor | Missed | Jitter | Overlap / mutual exclusion | History / notify | UI widget |
|---|---|---|---|---|---|---|---|---|---|
| 9 | **Automatic OS updates** (unattended-upgrades) | `AutoUpdates { hour, minute, window_min 10–360, reboot_hour, reboot_minute }` (`api-types/updates.rs:111-131`) → drop-ins of Debian's `apt-daily-upgrade.timer` (`OnCalendar=*-*-* HH:MM`, `RandomizedDelaySec=window`, `Persistent=true`) and `apt-daily.timer` (one hour earlier, 30 min random) (`rynas-updates/src/domain.rs:100-117`). The reboot time is an apt.conf value | key `updates.auto` | **Deb** timers + unattended-upgrades. The reboot is done by u-u itself | Persistent | the window | **none with rynas jobs** (an upgrade can start during a scrub or backup) | journal of u-u only. Not in ST history | `<input type=time>` start + "Window length (minutes)" (meaning: *the start is spread randomly*) (`pages/updates.rs:485-490`) |
| 10 | **"Updates available" check** | fixed: 10 min after rynasd starts, then every 24 h of **monotonic** sleep (`rynasd/src/updates.rs:698-706`) | not stored | rynasd **loop** | resets on every restart (no catch-up, possible double run) | none | none | notification | none |
| 11 | **Docker image update check** | fixed: first run 5–15 min after start (or 24 h after the last one), then 24 h + up to 1 h random (`docker_auto.rs:1-13`, `:455-470`) | `checked_at_ms` in state | rynasd loop → task `docker.update-check` (2 h deadline) | yes (from the persisted last run) | ≤ 1 h | none | task panel + notification on change | none ("Images are checked every day") |
| 12 | **Docker automatic updates** | `docker.auto_update { hour, minute, window_min 10–360 }` (`api-types/docker.rs:350-375`). Checked every 60 s against local time. Once per day (`last_window`). Start delayed at random inside the window. Updates only *start* inside it | key `docker` | rynasd loop → tasks `docker.auto-update`/`apps.auto-update`, one stack at a time | **missed window = skipped until tomorrow** | random inside the window | its own `maintenance()` list of task kinds + `/proc/mdstat` (`docker_auto.rs:352-374`). See inconsistency I-6 | task panel + notification | **two number fields** "Window start (hour)" / "(minute)" + "Window length (minutes)" (meaning: *updates start only inside it*) (`pages/docker/overview.rs:450-456`) |
| 13 | **Online app catalogue refresh** | fixed: 24 h + jitter after the last check. First run 2–32 min after start. Retries sooner on download error (`rynasd/src/apps/online.rs:44,559-571,717-745`) | `last_check` in state | rynasd loop | yes | yes | none | log + notification when refused | switch "download daily" only |
| 14 | **PC backup (agent, files and btrfs methods)** | `PcPlan { interval_hours 1–168, start_delay_min, paused }` (`api-types/pcagent.rs:101-131`). Due = `last_ok + interval`, at least `start + delay`. Failed runs retry after 5, 10, 20… min, capped at the interval (`crates/rynas-agent/src/scheduler.rs:34-51`) | NAS device settings → plan pushed to the agent | **agent loop on the PC** (`systemd-inhibit` while running) | anacron-like: due at once after start + delay | none | agent lock `/.rynas/.lock`. Refuses while `btrbk.timer` is active (`send.rs:702-724`). NAS returns 423 during maintenance | NAS run reports + late/critical alerts (`alert_hours`, `critical_hours`) | "Every (hours)" + "After start-up (minutes)". The same number fields are duplicated in `pages/backup/agent.rs:1304-1305` and `pages/backup/pcs.rs:1167-1168` |
| 15 | **PC repository maintenance** (retention, prune, check) | `maintenance_hour: u8` (0–23, every night) (`api-types/pcagent.rs:208-210`). A 5-minute loop tests `local hour == maintenance_hour`. `maintained_day` stops a second run on the same day (`rynasd/src/pc_agent/maint.rs:1082-1134`) | key PC agent settings | rynasd loop → `rynas-backup-worker` in a transient unit with `MemoryMax` | **NAS off or busy during that hour → skipped until tomorrow**. A spring-forward gap hour is skipped | none | waits 10 min after uploads (`QUIET`), memory budget. **No FsLock** | `maintenance.postponed` reason + notifications | single number field "Maintenance hour" (`pages/backup/agent.rs:629`) |
| 16 | **PC backup btrbk (B1) retention + freshness** | fixed: every 3600 s, first after 120 s (`rynasd/src/pc_backup.rs:2745-2770`) | — | rynasd loop | n/a | none | filter flock | alerts `pcbackup.subvolume_late` (`alert_days`) | — (the PC's own `btrbk.timer` decides when to send) |
| 17 | **Notification quiet hours / grouping / reminders** | `QuietWindow { days 1..7, start "HH:MM", end "HH:MM" }` + `group_window_s` + `repeat_interval_s` (`api-types/notify.rs:516-563`) | notify rules | rynas-notify engine | held and sent at the window's end | — | — | outbox | per-rule weekday checkboxes + two time inputs (`pages/notifications.rs:1248-1280`) |
| 18 | **Front LED brightness schedule** (Synology plugin) | 168-char grid, Sunday first (`api-types/synology.rs:117-139`) | module settings | synohwd (synodeb) | n/a | — | — | — | `week_grid` (`modules/synology/led.rs`) |
| 19 | **Disk standby / spin-down** | minutes (`disk_standby_minutes`, `synology.rs:110`) or `standby_s` per disk (`api-types/storage.rs:1731`) | settings | drive firmware / synohwd (`hdparm -S`) | — | — | dedup skips sleeping disks. Storage watch avoids waking them | — | number fields |
| 20 | **One-shot delayed restart/shutdown** | delay presets (`api-types/power.rs:278-310`) | logind | systemd-logind | — | — | busy reasons include `SchedulerTaskRunning` | — | dialog |
| 21 | **Debian system timers not managed by rynas** | Debian defaults: `fstrim.timer` (rynas only *checks* that it is on, `rynas-storage/src/tuning.rs:323-387,475`), `mdcheck_start/continue.timer` (md RAID check), `e2scrub_all`, `logrotate`, `apt-daily*`, `btrbk.timer` (made inert by a rynas drop-in, `packaging/systemd/btrbk.service.d`) | Debian | Deb | Debian defaults | Debian defaults | **none with rynas jobs**. Docker auto-update and scheduled shutdown do look at `/proc/mdstat` | — | read-only list on the Scheduler page |
| 22 | **Monitoring and housekeeping loops** (not user schedules) | fixed intervals: storage watch (`notifications.rs:364-390`), degraded/initramfs alerts (`:397,523`), clock sync watch 300 s (`clock.rs:871-882`), local CA/TLS names 120 s (`rynasd/src/tls.rs:61,670-690`), web-access certificate check 600 s and watch 30 s (`web_access/mod.rs:89,100`; ACME renewal is done by rynas-caddy itself), removable 2 s (`removable.rs:54`), UPS 5/10 s, hardware 30 s in rynas-web (`rynas-web/src/notify.rs:27`), session sweep 300 s (`rynas-web/src/auth.rs:393`), catalogue idle drop 300 s (`main.rs:1321`) | consts | loops | n/a | — | — | events | none |
| 23 | **Safety rollback timers** | one-shot `rynas-net-rollback.timer`, `rynas-ssh-rollback.timer`, `rynas-net-watchdog.timer` (5 min after boot) | — | systemd | — | — | — | — | — (must stay outside the scheduler: they must work when rynasd is broken) |

**Count.** 8 activities already run on the Task Scheduler (rows 1–8). **8 user-visible recurring jobs** run on their own clocks (rows 9–16). 4 time-based *states* or windows have their own formats (rows 17–20). About 6 Debian timers do disk or package work that rynas does not coordinate with (row 21). Rows 22–23 are infrastructure and should stay as they are, shown read-only.

### 1.3 Ways of writing a schedule found

1. systemd `OnCalendar` string (`TaskTrigger::Calendar`), optionally entered as cron
2. `Vec<Weekday>` + `hour` + `minute` (power rules)
3. `hour` + `minute` + `window_min` (updates: random start within the window; docker: start only within the window — **same fields, different meaning**)
4. `hour: u8` only (PC maintenance)
5. `interval_hours` + `start_delay_min` (PC agent)
6. 168-character weekly grid (dedup `0/1`, LED `0/1/2`)
7. `QuietWindow { days 1..7, "HH:MM", "HH:MM" }` (notifications)
8. "at most every N days" piggy-back counters (restic prune/check)
9. hard-coded constants (updates check, image check, catalogue, B1 prune)

### 1.4 Inconsistencies (the ones that matter)

- **I-1 Two clocks for jobs.** Rows 9–16 never show in the Scheduler list or its history, have no "run now" there (some have it on their own page), and cannot be paused with the others. Pausing is per feature: `enabled` on a task, `paused` on a PC plan, `mode: Off` on updates, the per-stack switch in Docker.
- **I-2 Missed runs.** At least six different behaviours: `Persistent=true` (snapshots, backups, replication, updates), `false` (shutdown, dedup), anacron-like from the persisted last run (agent, image check, catalogue), "same hour only, otherwise tomorrow" (PC maintenance), "today's window only" (Docker auto-update), monotonic 24 h reset by every restart (updates check). The user cannot see or choose which one applies.
- **I-3 Jitter.** Seconds in the Scheduler (`randomized_delay_s`), minutes as "window length" in updates and Docker, fixed 600 s for restic, built-in random delays for the image check and catalogue, none anywhere else.
- **I-4 Mutual exclusion.** Several partial mechanisms exist, and none of them is complete:
  - `FsLocks` covers scrub, replication, restic and storage ops, but not snapshots, SMART long tests, PC maintenance (repository prune/check on the same volume) or unattended-upgrades. Heavy holders also run **in parallel** by design (`fslock.rs:10-11`): a 72 h scrub + a backup + a replication on one volume all run together.
  - Docker auto-update has its own list of "maintenance" task kinds (`docker_auto.rs:353-357`).
  - Scheduled shutdown has its own busy checks (`api-types/power.rs:100-110`).
  - Debian's `mdcheck` and `apt-daily-upgrade` know nothing about any of these.
- **I-5 DST and time zones.** The handling is correct but scattered. systemd timers (rows 1–9) and jiff with `compatible` disambiguation (power `rules.rs:1-6`, dedup `window.rs`) agree with each other. PC maintenance compares `hour ==` every 5 min: a 02:xx setting is skipped on the spring-forward day. The updates check uses monotonic time. Docker uses `local_now()` per minute.
- **I-6 Bug-like gap.** Docker's `maintenance()` is meant to skip updates "while a scrub… runs" (module doc `docker_auto.rs:12-13`). Its list catches the manual scrub (`storage.scrub`) but **not the scheduled one**, whose task kind is `btrfs.scrub` (`jobs.rs:35`). It also does not include `backup.run` or the PC maintenance.
- **I-7 Run history.** The ST `scheduler.db` keeps runs of rows 1–8 only. Docker and catalogue jobs appear in the volatile task panel only. PC maintenance shows only the "postponed" reason. unattended-upgrades only in its own log.
- **I-8 Retry.** Only the agent (exponential) and the catalogue/image check (ad hoc) retry. Scheduler tasks never retry, so a backup that fails on a network blip waits for the next day.
- **I-9 UI widgets and texts.** There are **6 different editors** for the same concepts:
  - `TriggerEdit` (scheduler, snapshots, replication, restic)
  - weekday checkboxes + `type=time` (power)
  - weekday checkboxes + two time inputs (quiet hours)
  - two **number** fields for hour and minute (Docker), against a `type=time` input (updates)
  - single "Maintenance hour" number (PC)
  - "Every (hours)" number (agent)
  - `week_grid` (dedup, LED)

  Texts differ for the same idea: "Window length (minutes)" means two different things (random start in updates, start-only-inside in Docker); "Catch up missed runs" exists only in `TriggerEdit`; "Local time." / "Local time of the NAS." / nothing. `DAY_KEYS` is shared (good), but notifications keep their own copy `DAYS` (`pages/notifications.rs:1041`).
- **I-10 No overview.** Nothing shows "what runs tonight": snapshots, backups, scrub, dedup windows, update window, Docker window, maintenance hour and shutdown together, with conflicts.
- **I-11 Fragile internal jobs.** An internal ST job runs *inside* rynasd as a UPID task. A rynasd restart (for example a package upgrade) ends it, and `FsLocks` has to wait for leftovers (`fslock.rs:44-46,60-61`). Restic and PC maintenance already avoid this by running in their own transient units.
- **I-12 DST mismatch, confirmed on 257 (fixed for the power page in S0).** The power schedule (`rynas-power/src/rules.rs:1-6`) and dedup compute times with jiff `compatible` (a gap time runs after the gap) and say this matches systemd. systemd **skips** a nonexistent time instead (see §2.2). The power page showed a shutdown that the timer does not run on the spring-forward day; since S0 the next shutdown comes from `rynas-schedule` (the RTC startup keeps `compatible`: the firmware alarm is an instant, it does fire). The dedup window still has its own arithmetic: a block that starts in the missing hour does not start that night (S3, `Window` trigger).

---

## Part 2 — How serious systems do it

(Summary of the web research. The references list is at the end.)

### 2.1 What each system teaches

| System | Model | Missed runs | Overlap | Notable |
|---|---|---|---|---|
| **systemd timers** (Debian 13 ships **257.13**) | `OnCalendar` (weekday lists and ranges, `/` repetition, `~` last days, IANA zone suffix since v235), monotonic `OnBootSec`/`OnUnitActiveSec`/`OnUnitInactiveSec` | `Persistent=`: **one** catch-up run when the timer starts, if ≥ 1 run was missed. The stamp is in `/var/lib/systemd/timers/stamp-<unit>.timer` | a timer never starts a second instance of an active service | `RandomizedDelaySec` (redrawn every run), `FixedRandomDelay` (v247, stable per machine), `RandomizedOffsetSec` (v258, **not in trixie**). `AccuracySec` default 1 min. `WakeSystem` (suspend only). `systemd-analyze calendar --iterations=N` gives the preview. Transient timers (`systemd-run --on-calendar`) live in `/run` and **do not survive a reboot**. A `daemon-reload` during the random delay can skip a run (RH bz 2193262) |
| **cron / anacron** | 5 fields. anacron counts in days | cron: none. anacron: catches up with timestamps | none (needs `flock`) | Debian cron runs a skipped DST hour right after the jump. No history, no jitter, no retry |
| **Kubernetes CronJob** | cron + `timeZone` (stable since 1.27) | `startingDeadlineSeconds`. If more than 100 slots were missed, only the latest one runs | `concurrencyPolicy` Allow / **Forbid** / Replace | `suspend`. History limits (3 ok / 1 failed). Jobs must be idempotent |
| **Proxmox VE / PBS** | systemd-like calendar events | PVE backup jobs: `repeat-missed` (Advanced) | global backup lock with `lockwait` (180 min) | PBS checks every minute, local or UTC only. Prune simulator. GC and verify jobs with "re-verify after N days". Parser `proxmox-time` (Rust, AGPL, not on crates.io) |
| **TrueNAS** | presets + cron "advanced scheduler" with a **schedule preview** | — | — | snapshot task Begin/End window (hourly). Scrub **threshold days**: the task fires on schedule but runs only if N days have passed since the last scrub. Docs warn against overlapping SMART tests and scrubs. Replication runs "after the snapshot task" |
| **Commercial NAS task schedulers** | daily / weekly / monthly / date. "Repeat every N min/h between first and last run time". Boot and shutdown triggers in the same list | — | — | run now, per-run results with exit code, notification with output (1 MB cap). Some vendors have **no** general UI scheduler, only per-app schedulers and crontab editing |
| **borgmatic / restic** | an external timer (`OnCalendar=daily`, `Persistent=true`, `RandomizedDelaySec=10m`) | via the timer | restic: exclusive lock for prune. A lock older than 30 min is stale | borgmatic: per-check `frequency` ("2 weeks"), `only_run_on` weekdays, frequency gate inside the job |
| **Windows Task Scheduler** | triggers (time, boot, logon, idle, event) + `RandomDelay` | `StartWhenAvailable` ("run as soon as possible after a missed start") | `MultipleInstancesPolicy`: Parallel / Queue / **IgnoreNew** / StopExisting | conditions (idle, AC power, network), `ExecutionTimeLimit` (72 h default), `RestartCount × RestartInterval` on failure, `WakeToRun`, `AllowDemandStart` |
| **Home Assistant** | `time` (fixed or helper), `time_pattern` (`/5`) | — | automation `mode` (single/queued/restart/parallel) | the time helper is reused by many automations |

### 2.2 DST: what really happens

First tested with `systemd-analyze calendar` on systemd 262, Europe/Rome. The man pages do not say. **Verified in S0 on Debian trixie's systemd 257.13** (golden files in `crates/rynas-schedule/tests/golden/`: 2524 cases in nine zones around their 2026 changes, among them Lord Howe's half-hour shift, Dublin's negative DST and Havana's change at midnight; `rynas-schedule` reproduces all of them):

- spring forward, `*-*-* 02:30`: **skipped that day**, next run the day after. Later times of the same day still happen (`*-*-* 02,03:30` runs at 03:30; `*:00/20` goes 01:40 → 03:00)
- fall back, `02:30`: runs **once**, at the first (summer-time) occurrence
- `hourly` across fall back skips the repeated hour (a 2 h gap in real time)
- `Persistent=` uses the same computation from the last trigger (`timer.c`), so a skipped time is never "missed" and never caught up at boot
- computed from **inside** the repeated hour, a later time of that hour runs at its second occurrence: a timer **without** `Persistent=` that starts there (boot, restart of the timer) runs `02:30` a second time that night. With `Persistent=` the base is the last trigger and it does not
- in a zone that changes at midnight (America/Havana), `*-*-* 00/2:00` makes systemd give up ("Infinite loop in calendar calculation"): that timer gets no next elapse at all
- 257.13 has a fix 257.0 lacks (`find_next()` "stuck → advance, let mktime decide"): without it Europe/Dublin's `daily` would loop on the spring-forward night

This differs from Debian cron (it runs the skipped job right after the jump), from croner/OCPS (fixed times run after the gap) and from jiff `Compatible` (gap → the later instant).

**Consequence for rynas (inconsistency I-12):** `rynas-power/src/rules.rs:1-6` documents "a rule time in the gap happens at the first valid time after it… the same as systemd's `OnCalendar=`". That holds for the jiff computation (the "next shutdown" shown and the RTC wake alarm), but if the test result also holds on v257, the shutdown timer itself **skips** that day. The power page would then show a shutdown that does not happen, while a startup at the same time does happen. The same mismatch can hit `rynas-dedup/src/window.rs` (jiff) against its timer. The shared evaluator must reproduce systemd's actual behaviour, checked against `systemd-analyze` in tests, rather than state it.

### 2.3 Best practices to adopt

1. **One schedule type**: presets + an advanced calendar expression, with a **preview of the next N runs** and a sentence (TrueNAS, PVE, systemd-analyze).
2. **Explicit missed-run policy per job**: coalesce to one run (systemd `Persistent`, PVE `repeat-missed`), skip, or run only within a deadline (k8s `startingDeadlineSeconds`). Never replay every slot.
3. **Explicit concurrency policy**: forbid/ignore-new by default, plus queue and replace (k8s, Windows), and a maximum run time.
4. **Resource locks for disk-heavy work**, serialized per pool/disk (PVE global lock + `lockwait`, TrueNAS SMART/scrub advice, restic exclusive locks).
5. **Frequency gates inside jobs** ("at most every N days": TrueNAS scrub threshold, borgmatic `frequency`, PBS re-verify). rynas already does this for restic prune/check.
6. **Jitter** for jobs that touch the network or the same disks (`RandomizedDelaySec`, Windows `RandomDelay`).
7. **Conditions** (Windows: AC power, idle, network). For a NAS: UPS on battery, disks asleep, pool degraded or resyncing.
8. **Retries with a count and an interval** (Windows). None of the NAS systems document any.
9. **Run history per job** with exit code and output, kept within limits (k8s 3/1, DSM results). **Suspend without deleting** (k8s `suspend`, TrueNAS enabled).
10. **DST-explicit semantics**, documented and tested. Warn when a time falls in the gap hour.
11. **Calendar overview** of every job and window: no mainstream NAS UI does this well, so it is a place where rynas can lead.

---

## Part 3 — Proposal: one rynas scheduling system

### 3.1 Principles

1. **One model and one engine for every recurring job.** A *job* is anything that does work on a schedule. Each job is a scheduler task with an **owner** (feature). Its schedule is edited either on the feature page or on the Scheduler page, always with the same component, and the owner decides which fields are locked.
2. **systemd is the clock, rynasd is the policy.** Timers stay persistent units: they survive rynasd restarts and catch up after boot (`Persistent=`), and `systemctl list-timers` shows them. Every start passes through a rynasd **gate** that applies the run policies.
3. **Windows are first-class, separate from jobs.** "Allowed / quiet / on / reduced" over a week (dedup, quiet hours, LED, global maintenance window, update windows) is one `WeekWindow` type with one widget, the week grid already in `week_grid.rs`.
4. **No behaviour change on migration.** Every new field defaults to what the feature does today. Changing a behaviour is a separate, visible decision.
5. **Generic Debian first.** Nothing depends on the Synology plugin. The design uses only systemd ≥ 252 features, and trixie ships 257.

### 3.2 Data model (`rynas-api-types::scheduler`, evolving the existing types)

`ScheduledTask` keeps its id, action, timeout, nice, io_weight and notify. New fields are added with serde defaults, so every stored document still parses:

```rust
pub struct ScheduledTask {
    // existing: id, name, description, enabled, action, timeout_s, nice, io_weight, notify, notify_output
    pub trigger: TaskTrigger,              // extended, see below
    #[serde(default)] pub owner: TaskOwner,          // User | Feature { key, href, locked: LockedFields }
    #[serde(default)] pub policy: RunPolicy,         // defaults = today's behaviour per action
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskTrigger {
    /// Structured recurrence, rendered losslessly to OnCalendar (what the presets edit).
    Recurrence { every: Every, at: Vec<TimeOfDay>, days: WeekdaySet, month_days: Vec<u8>, #[serde(default)] start_window_min: u16 },
    /// Advanced: any systemd calendar expression (today's `Calendar`, kept as-is, cron accepted on input).
    Calendar { on_calendar: String, cron: Option<String>, persistent: bool, randomized_delay_s: u32 },
    /// Elapsed-time interval from the last *successful* run (anacron-like): PC agents, catalogue, image checks.
    Interval { hours: u32, #[serde(default)] after_boot_min: u32 },
    /// Starts of the "on" blocks of a weekly window; the run is stopped at the block end (dedup).
    Window { window: WeekWindow, stop_at_end: bool },
    /// After another task finished (replication "after snapshots").
    After { task: String, #[serde(default)] only_on_success: bool },
    Boot { delay_s: u32 },
    Manual,
}

pub enum Every { Minutes(u16), Hours(u16), Day, Week, Month }

pub struct RunPolicy {
    pub missed: MissedPolicy,          // Skip | RunOnce { within_h: Option<u32> }  (Persistent= + deadline)
    pub jitter_s: u32,                 // RandomizedDelaySec (replaces randomized_delay_s / window_min "spread")
    pub concurrency: Concurrency,      // Skip (default; = systemd today) | Queue | ReplaceRunning
    pub resources: Vec<Claim>,         // derived from the action when empty, see 3.4
    pub conditions: Conditions,        // skip_if_disks_asleep, skip_on_battery (UPS), only_in: Option<WindowRef>
    pub retry: Retry,                  // attempts (0 = today), delay_s, backoff x2, cap
    pub notify: NotifyRules,           // OnFailure | Always | Never + after_consecutive_failures, on_recovery, on_missed
}

pub struct WeekWindow(String);         // 168 cells, Sunday first, the existing grid format; levels per use
```

- `TaskTrigger::Calendar` stays the **canonical storage for systemd**. `Recurrence` exists so the UI and the PC agent never have to parse arbitrary OnCalendar. Each `Recurrence` renders to exactly one OnCalendar, and `Cal::parse` (UI) moves into the shared crate as `Recurrence::from_calendar` (`None` → it stays `Calendar`).
- A new crate **`rynas-schedule`** (pure, no I/O; S0) holds `next_runs(expr, tz, from, n)`. It ports systemd 257.13's `find_next()` and the way glibc's `mktime()` resolves DST gaps and folds onto `jiff` (not `compatible`: systemd skips a gap time). rynasd, the PC agent and the UI share it (the UI through the server preview, so no tzdb has to ship in wasm; the crate builds for wasm without its `next` feature). `systemd-analyze calendar` stays the authority for `Calendar` expressions; the golden files keep the two in agreement.

### 3.3 Engine

**Recommended: systemd timers + rynasd gate (hybrid).**

| | systemd timers + gate (rec.) | Internal scheduler in rynasd | Status quo |
|---|---|---|---|
| Survives rynasd restart and crash | yes (timers are PID 1's) | needs persisted next/last stamps and careful restart logic | mixed |
| Catch-up after power-off | `Persistent=` stamp files, proven | must reimplement (anacron logic) | mixed |
| DST | systemd semantics, documented | jiff, must match systemd | mixed |
| Visibility | `systemctl list-timers`, journal, drift-checked files | only via rynas UI | partial |
| Policies (locks, queue, pause, windows, retry) | in the gate (rynasd) | in the scheduler (rynasd) | scattered |
| Precision and cost | `AccuracySec=1s`, no polling | a timer wheel. Cheap | 60 s and 5 min polls |
| Fails when rynasd is down | gate: fail-closed for internal actions (they need rynasd anyway), configurable fail-open for user scripts | everything stops | — |

How it works:

- **Gate.** Every task service gets `ExecCondition=+rynasctl gate <id>` (systemd ≥ 243). rynasd answers **run** (exit 0), **skip** (exit 1: the unit is "condition failed", not failed, and the timer stamp still advances, so `Persistent=` does not loop), or **wait** (rynasctl blocks while the task is queued on a resource, up to the policy's deadline). The gate checks, in order:
  1. pause-all and per-task pause
  2. the `only_in` window, or the global maintenance window for heavy jobs
  3. `missed.within_h` for a catch-up start (rynasd knows the timer's `LastTriggerUSec` and the scheduled time)
  4. conditions (disks asleep, UPS on battery)
  5. concurrency and resource claims. The claims are granted as a **lease** bound to `$INVOCATION_ID` and released by the existing `ExecStopPost=+rynasctl task-finished`.

  After a rynasd restart, leases are rebuilt from the running `rynas-task-*` units. This replaces the "wait for leftovers" logic in `FsLocks`.
- **Internal actions run in their own transient or child unit**, not as in-process rynasd tasks. The pattern already exists for `rynas-backup-worker` and `rynas_tasks::Outside`. A rynasd upgrade then no longer kills a running scrub or snapshot (I-11). The UPID task stays as the *view* of the run.
- **Interval triggers** use `OnUnitInactiveSec=` + `OnBootSec=` timers. On the agent side they use the agent's existing loop, driven by `rynas-schedule::next_runs` so both sides agree.
- **After triggers**: when the producer finishes, `task-finished` starts the consumer unit (the replication "after snapshots" case becomes a regular task with its own history, instead of running inline).
- **Retry**: on failure, rynasd arms a transient one-shot `systemd-run --on-active=<delay> --unit=rynas-retry-<id>` that starts the same service. If a reboot comes first, the retry is lost and the normal schedule and catch-up take over. This is acceptable and documented.
- **Debian timers.** `apt-daily-upgrade.timer` and `mdcheck_*` get a rynas drop-in `ExecCondition=` as well (the btrbk drop-in is the precedent). They then respect pause-all, locks and the maintenance window, and are recorded in history as **system** tasks. Alternative: replace them with rynas tasks (Decision D3).

### 3.4 Concurrency and resource classes

Claims are derived from the action, so the user never has to set them:

| Class | Holders | Rule |
|---|---|---|
| `volume:<fs-key>` **heavy-io** | scrub, restic backup (per source volume), replication send/receive, PC repository prune/check, storage ops | **serialized per volume** (queue, priority order below). Scrub is *resumable*: `btrfs scrub cancel` / `resume` keeps its position, so like dedup it can yield instead of blocking for 72 h |
| `volume:<fs-key>` **yielding** | dedup | today's rule (gives way, saves its work) |
| `disk:<dev>` | SMART long test, md `check` of arrays on the disk | not together with a heavy-io claim on volumes on that disk (a long test during a scrub slows both) |
| `system:packages` | unattended-upgrades, Docker/app updates, rynas package install | exclusive |
| `system:power` | scheduled shutdown/reboot | waits (policy) for every other claim. Replaces the hand-made busy list |
| none | snapshots, recycle cleanup, user scripts (unless the user adds a volume claim) | — |

Priority when queued: shutdown > snapshot > backup/replication > PC maintenance > scrub > SMART long > dedup. **Manual "Run now" skips the queue only after a confirmation**, and the dialog names what it will slow down. Docker's `maintenance()` list and the power busy checks become questions to this same lock table, which also closes I-6.

### 3.5 Missed runs, DST, windows, pause

- `MissedPolicy::RunOnce { within_h }` = `Persistent=true` + a deadline checked by the gate, like Kubernetes `startingDeadlineSeconds`. For example, a nightly backup missed by 3 days runs once after boot, but a "shutdown at 23:00" never runs at boot. Defaults: snapshots, backups, replication, scrub, SMART, PC maintenance → RunOnce (no deadline, today's `Persistent`). Shutdown, dedup, Docker window → Skip (today).
- **DST**: one rule, the one systemd actually applies.
  - Observed on v262 and verified on trixie's 257.13 (S0): a time that does not exist is **skipped** that day, and a repeated time runs once, at its first occurrence.
  - `rynas-schedule` reproduces that behaviour, with golden tests against `systemd-analyze calendar --base-time`.
  - Timers whose base can fall inside a repeated hour should keep `Persistent=true` (or the gate should drop a second run that night): without it, a timer started inside the repeated hour runs that hour's time twice (§2.2).
  - The power page's jiff computation (`rules.rs`) and the dedup window move onto it (fixes I-12).
  - Interval triggers count elapsed time.
  - The editor warns when a chosen time falls in a DST gap of the system zone, and the preview leaves out the skipped day.
  - If the owner prefers "run after the gap" for fixed daily times, the gate can do it: a `02:30` daily job gets a second `OnCalendar=` line at the gap's end, enabled only on that date. This is not recommended: it adds complexity for one night a year.
- **Global maintenance window** (`scheduler.maintenance: WeekWindow`, off by default). If set, heavy-io jobs and updates whose run falls outside it are deferred to the next "on" cell. This replaces the private windows of Docker (`window_min`) and updates for users who want one night window for everything.
- **Pause all** (`scheduler.paused_until: Option<ts>`, with a reason): the gate skips with the outcome `paused`. rynasd stops arming retries. The agent plans receive `paused`. The top bar shows a chip while paused.

### 3.6 History and notifications

- `scheduler.db` `runs` gains `trigger` (timer / catch-up / boot / manual / after / retry), `outcome` (ok / failed / skipped:<reason> / missed / preempted / window_ended / timed_out), `queued_ms` and `owner`. Skips and misses become **visible rows**. System tasks (u-u, mdcheck) and agent runs (from the existing run reports) are written there too, so there is one history for everything.
- Notification rules for every task: on failure (default), always, never. Add *after N consecutive failures*, *on recovery*, *when missed*, and *late* (no success for X hours; this generalizes `alert_hours`/`alert_days` of the PC backups).
- The duration estimate (median of the last 10 runs) feeds the overview and conflict detection.

### 3.7 API (additions under `/api/v1/scheduler`)

| Method | Path | Purpose |
|---|---|---|
| POST | `/preview` | replaces `tasks/preview-calendar`: any `TaskTrigger` + `RunPolicy` → normalized text, human sentence, next N runs (DST-shifted runs flagged) |
| GET | `/overview?from&to` | occurrences of every task (rynas, system, agent plans), windows (`WeekWindow`s with their owner), estimated durations, resource claims, **conflicts** |
| POST | `/pause` / `/resume` | pause-all with an optional end and reason |
| GET/PUT | `/maintenance-window` | global window |
| GET | `/runs?task&outcome&from` | unified history across tasks |
| internal | `/tasks/{id}/gate` | used by `rynasctl gate` |

Feature APIs keep their `schedule: Option<TaskTrigger>` fields (snapshots, replication, backup already do). Power rules, updates, Docker, dedup and PC settings gain `schedule`/`window` fields of the shared types, while the old fields stay readable (§3.9).

### 3.8 UI

- **`ScheduleEditor` component** (`crates/rynas-ui/src/schedule/`), grown from `TriggerEdit`:
  - presets as a segmented control: *Every N minutes / Hourly / Daily / Weekly / Monthly / Every N hours since last run / In a weekly window / After task / At boot / Manual*. "Advanced" opens the OnCalendar field (cron accepted).
  - weekday chips shared with power and quiet hours. One `type=time` input everywhere: no hour/minute number pairs.
  - live preview: the next 5 runs plus a one-line sentence ("Every weekday at 02:30, catch-up once if missed within 12 h, random delay up to 10 min").
  - a collapsible "When it can't run" section: missed, overlap, retry, jitter, conditions, with plain-language options. Owners lock fields through `LockedFields`, for example the power shutdown hides "missed".
  - one set of i18n keys (`sch.*`). "Random delay" is always shown in minutes. A "window" always means *allowed start window*, and the stop-at-end flag is shown separately.
  - `WeekWindowEditor` = the existing `week_grid`, used by dedup, LED, quiet hours (a migration of their two-time-inputs rows into grid levels), the global maintenance window and an optional allowed window for PC backups.
- **Global "Schedule" page** (System › Schedule, next to Task Scheduler, or as its first tab):
  - week view (7 columns × 24 h, the same grid look) and a "Tonight / next 24 h" timeline list
  - each job is a bar of its estimated length, coloured by owner. Windows are background bands. Conflicts (two heavy-io claims on one volume, or a job outside the maintenance window) get a warning outline and a sentence ("Scrub of *data* (≈9 h) overlaps the backup *offsite* at 02:00; the backup will wait").
  - filters by volume and owner. Click opens the owner's editor.
  - "Pause all" button and the unified history (outcomes as chips) on the same page.
  - must respect the no-inner-horizontal-scroll rule: the week view collapses to a day list on narrow screens.
- The Scheduler page list shows **every** task, rynas and system, with the owner as a column. Feature-owned tasks link to their page, as dedup does today.

### 3.9 Migration (no behaviour change)

Config-domain migrations in `rynas-config`. Old fields stay readable for one release and are removed after.

| Today | Becomes | Behaviour kept by |
|---|---|---|
| `TaskTrigger::Calendar` (all ST tasks) | same variant. `policy.missed` from `persistent`, `jitter_s` from `randomized_delay_s` | identical units |
| snapshot/replication/backup/scrub/SMART/recycle/power-off/dedup tasks | `owner = Feature{…}`. Claims derived (scrub, backup and replication already heavy). **Queueing for heavy-io starts only after Decision D4.** Until then, `Concurrency::Parallel` on the volume class reproduces today | same |
| replication `after_snapshots` | `TaskTrigger::After { task: snapshot-<job> }` | same order. The run gets its own history row |
| power rule `days+hour+minute` | stays the power UI's model. The task trigger is still generated (`rules.rs:129`) with `owner` lock | same OnCalendar |
| dedup grid | `TaskTrigger::Window { window, stop_at_end: true }`. The unit gets the **exact** block starts (one `OnCalendar=` line per distinct start pattern instead of today's cartesian product: removes the spurious "outside the window" runs) | same runs. Fewer no-op starts |
| updates `hour/minute/window_min` | system task `apt-upgrade` (owner updates) with `Recurrence daily at hh:mm`, `jitter = window_min`, `missed RunOnce`. The drop-in is generated from it. Reboot time stays a u-u setting | identical drop-in text |
| updates check (24 h monotonic) | task `updates-check`, `Interval 24h, after_boot 10 min`, owner updates, locked | first run 10 min after boot, then daily (now persisted, so no double run after restarts: a small improvement) |
| Docker image check | task `docker-update-check`, `Interval 24h`, jitter 1 h | same |
| Docker auto-update `hour/minute/window_min` | task `docker-auto-update`, `Recurrence daily at hh:mm`, `start_window_min = window_min`, missed **Skip**, claim `system:packages` | same window. The loop goes away |
| catalogue refresh | task `apps-catalog-refresh`, `Interval 24h`, jitter | same |
| PC maintenance `maintenance_hour` | task `pc-maintenance`, `Recurrence daily at HH:00`, missed **Skip** (today) or RunOnce (D5), claim heavy-io on the PC folder's volume (**new: only after D4**) | same hour. The DST gap is now handled the systemd way |
| B1 retention hourly loop | task `pcbackup-retention`, `Recurrence hourly` | same |
| agent `interval_hours + start_delay_min` | `TaskTrigger::Interval { hours, after_boot_min }` in `AgentPlan`. Old agents keep reading the two old fields, which the NAS still sends | identical |
| quiet hours `QuietWindow[]` | `WeekWindow` (grid), converted losslessly (hour granularity; *see risk R4*) | same windows |
| restic prune/check "every N days" | stay as job maintenance options (they are part of the backup run, not separate jobs). They are shown in the overview as "with the backup run" | same |
| housekeeping loops (row 22) | unchanged. Listed read-only on the overview ("system checks") | — |

### 3.10 Phased plan

| Phase | Content | Size |
|---|---|---|
| **S0** | Verify systemd 257 DST behaviour; `rynas-schedule` crate (Recurrence ⇄ OnCalendar, `next_runs`, DST tests against `systemd-analyze` golden files). Move `Cal` out of the UI. Fix I-6 (Docker `maintenance()` must see `btrfs.scrub`, `backup.run` and the PC maintenance) | S |
| **S1** | Model: `owner`, `RunPolicy` with defaults = today, new triggers (`Recurrence`, `Interval`, `Window`, `After`), config migrations, `/preview`. `ScheduleEditor` replaces `TriggerEdit` on the 4 pages that use it. No behaviour change | M |
| **S2** | Gate (`ExecCondition`) + leases + pause-all + missed deadline + outcome rows in history. Internal actions moved to their own units (I-11) | L |
| **S3** | Move the rynasd loops into tasks: updates check, image check, Docker auto-update, catalogue, PC maintenance, B1 retention. Updates drop-in generated from its task. `ScheduleEditor` and `WeekWindowEditor` on the power, updates, Docker, PC and notifications pages | L |
| **S4** | Resource classes active (after D4): per-volume queue, scrub yields (cancel/resume), disk class for SMART/mdcheck, `system:packages`. Debian timers gated by drop-ins (after D3). Retry | M |
| **S5** | Global Schedule page (week view, timeline, conflicts, filters, unified history), global maintenance window | M |
| **S6** | Agent: `Interval` plus an optional allowed window from `rynas-schedule`. History rows from the agent run reports | S |

**S1 as built (2026-11-16).** `ScheduledTask.owner` (`TaskOwner::User | Feature { feature, key }`, locked fields derived from the feature) and `ScheduledTask.policy` (`RunPolicy`: every field optional, absent = the kind's default; `missed`/`jitter_s` enforced through `Persistent=`/`RandomizedDelaySec=`, deadline, overlap other than skip, retry (default 3 attempts for backups and replications) and notification rules stored for S2/S4). The migration is `ScheduledTask::migrate` (owner from the action, `missed`/`jitter_s` from a calendar trigger's fields, then the trigger fields mirror the policy), run by the domain's validation and when tasks are read; golden files generated by 0.8.13 from a stored document prove the same units (`crates/rynas-scheduler/src/golden/compat/`). `Recurrence` triggers are stored as their `Calendar` expression (the canonical form stays the systemd text); `Interval`, `Window` and `After` are modelled and previewed but refused by the Task Scheduler until the gate. Timers are restarted only when their file changes. `POST /api/v1/scheduler/preview` (`rynas_scheduler::preview`) gives normalized text, the preset (structured description), the next runs as wall-clock times with zone abbreviation, the DST changes of the next year that skip or merge runs, and `no_next_run`/`stops` when systemd would leave the timer with no next elapse. The UI editor is `crates/rynas-ui/src/schedule/` (`ScheduleEditor`, pure `Draft`).

**S1 review fixes (2026-11-16).** Policy and trigger fields: the run policy is the single reference for catch-up and random delay; a calendar trigger's `persistent` and `randomized_delay_s` mirror it. A write may give only the trigger fields (an older client: the policy follows them) or both; when both are given they must agree (`missed` catches up ⇔ `persistent`, `jitter_s` = `randomized_delay_s`), otherwise the request is refused (`rynas_scheduler::domain::trigger_agrees`) instead of the policy winning silently. The schedule editor saves a preset's stored text (`hourly`, `Mon..Fri 02:30`) unchanged until its fields are edited (`Draft::stored`), so opening and saving never rewrites or restarts a timer. PC repository maintenance catch-up: see `docs/research/pc-backup.md`.

**S2 as built (2026-11-16).** The gate is `ExecCondition=+rynasctl gate <id>` in every task service (`rynas_scheduler::gate` for the pure decisions, `rynasd::gate` for state, endpoint `POST /v1/scheduler/tasks/{id}/gate`, root only). Exit 0 runs, exit 1 skips: systemd 257 marks the start `exec-condition` (not failed), still runs `ExecStopPost=` with `SERVICE_RESULT=exec-condition`, and the timer's stamp has already advanced, so a skipped catch-up never loops (checked in `packaging/test-deb-systemd.sh`). The start's origin comes from `$TRIGGER_UNIT` (systemd ≥ 253) plus the starts rynasd asked for itself (run now, retries); a timer start is *on time* when a scheduled time lies within its random delay + 2 min before it, a *catch-up* otherwise.
- Checks, in order: pause of every task, pause of the task (both skip only scheduled starts and retries; run now and `systemctl start` go ahead); overlap; the catch-up deadline (`missed.within_h`: a catch-up runs only when a scheduled time lies within that many hours before it, the Kubernetes `startingDeadlineSeconds` rule); a lease (an earlier start of the task still running as a rynasd task).
- **Overlap: a finding.** A timer whose time comes while its service runs does not drop that time: when the run ends, systemd computes the next elapse from the last trigger, finds it in the past and starts the service again at once (verified on 257.13, with and without `Persistent=`). So before S2 the effective behaviour was "queue one", not "skip" as §1.0 says. The gate recognizes that start (the previous run ended moments ago, a scheduled time fell inside it, none since) and applies the policy: `skip` records an `overlap` row and skips it, `queue` lets it run (recorded as `queued`), `replace` is done by rynasd's watch loop (every 5 s: a running task one of whose times came after its start is restarted). **Default after the S2 review:** a task without a stored overlap (every task written before S2, and the tasks features write) is `queue`, systemd's real behaviour before the gate, so an upgrade changes nothing; a task created on the Task Scheduler page gets `skip` stored explicitly. A "Run now" whose run spans a scheduled time is followed by that time's start under `queue` (an `AfterOverlap` start like any other).
- **rynasd unreachable** (stopped, crashed, hung): `rynasctl gate` waits at most 20 s (socket activation starts a stopped rynasd, a restarting one answers when up), then runs the start as before the gate, so scheduled backups never stop silently. The only exception is a pause the user set: it is kept in `/var/lib/rynas/scheduler/pause.json` (root only), which rynasctl reads without rynasd, so a paused task stays paused. Both rynasctl helpers spool what rynasd could not take (`scheduler/gate-spool.jsonl`: starts with or without the gate, run ends); rynasd imports it into the history at its next run end or watch tick (5 s), with the output from the journal, never in the gate's answer (which must stay quick); every entry is recorded once however often it is imported (a spooled end is idempotent: no second retry, output or notification), and a spooled start without the gate never replaces the note of a real answer. `task-finished` spools on connection errors and on 5xx answers. **Late answers:** rynasctl sends its deadline (`answer_by_ms`, 15 s); an answer decided after it says "run" and records no skip, and the end of a run that really ran replaces a skipped row of its start, so a real result, output, retry and notification are never lost to a skip. **rynasctl never skips by accident:** in `gate` and `task-finished` a panic (the binary is `panic = "abort"`, the panic hook runs first) exits 0, writes to a closed journal are ignored, and arguments it does not understand (a newer or hand-edited unit) exit 0 with a log line instead of clap's exit 2 (which `ExecCondition=` would take as a skip). This replaces R1's "fail-closed for internal actions": an internal action needs rynasd anyway and fails on its own if it is really down; there is no reason to add a second failure mode.
- **Leases** are in memory only and rebuilt at start from the task units systemd reports running (with the filesystem lock of a scrub running in its own unit), so nothing persisted can go stale after a crash. A run's end releases its lease first, before anything is recorded, so an error while recording never leaves a heavy lease (Docker updates, a scrub's filesystem lock, deduplication) behind. A request (run now, a retry) is used once and never applies to a start of the task's own timer (`$TRIGGER_UNIT`); run now on a task that runs already leaves none.
- **Pauses**: `GET/POST /api/v1/scheduler/pause`, `POST /scheduler/resume`, `POST /scheduler/tasks/{id}/pause|resume` (immediate, not staged; optional end and reason). The Task Scheduler page has "Pause all" in its header, a banner while every task is paused, and a pause toggle per task (chip "Paused" in the row).
- **History**: `runs` gained `trigger` (timer, catch_up, boot, manual, retry, queued, other), `skipped` (paused_all, paused, missed, overlap, running), `attempt`, `detail`, `ungated`; skipped starts are rows with `result: skipped`. The history drawer shows the reason and origin (en/it/ru). Old databases get the columns on open. A skipped start never counts as a failure (the recovery notice after failures looks at runs that ran).
- **Retry** (default 3 attempts, 10 min doubling to 1 h for backups and replications, none otherwise): after a failed run started by its timer, a catch-up, a queued start or a retry (not run now), rynasd stores the next attempt in `scheduler.db` (`retries`) and its watch loop starts the unit when due, as a retry the gate knows. The row survives rynasd restarts and reboots; a retry is dropped when a newer run already happened (the schedule or a catch-up after the reboot), when the task is gone or disabled, or when it falls due during a pause (recorded as skipped). A failure with a retry armed does not notify; the last one does. The task status shows the armed retry. **Retry chains (after the S2 review):** the held-back failure notification is stored in the chain's row (`retries.held`); a newer scheduled failure keeps the chain's count and due time (it only moves `after_ms`), a run started by hand notifies as always and leaves the chain alone, and the run that follows a started retry (`retries.started_ms`) is that retry even when the gate did not know it (a request lost to a restart). The chain ends with a success (nothing is sent: the task works again), with its last attempt (that run notifies), or dropped: task deleted or off, a pause at its due time, its start skipped by the gate, lost (no run within 2 min) or not startable; a dropped chain sends its held notification exactly once (one caller takes the row). A deleted task's pause goes with it. Chosen over transient `systemd-run --on-active` timers, which a reboot loses.
- **Internal actions in their own unit (I-11)**: `rynasctl job run` asks rynasd for a plan; a scrub, a SMART self-test and a snapshot job without targets that follow it come back as a command (`rynasd::jobs::JobPlan`) that the task unit runs itself, so a rynasd restart no longer ends them. Their units get the sandbox rynasd's own unit gave those commands (`rynas_scheduler::units::JOB_SANDBOX`: `ProtectSystem=yes`, no network but `AF_UNIX`/`AF_NETLINK`, `@system-service`, a capability set limited to `CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH CAP_FOWNER CAP_CHOWN CAP_FSETID`; the `+` hooks run outside it), and they show in the task panel as before (`btrfs.scrub`, `smart.test`, `snapshot.run` as user `scheduler`): the entry follows the unit's journal, ends with the run's result, and cancelling it stops the unit (`rynasd::jobs::track`; rebuilt with the leases after a restart). (The scrub's filesystem lock is taken by rynasd first and held by the run's lease, so deduplication still gives way, and Docker's update window sees it through `gate::heavy_runs_now`). Restic backups and deduplication already run in transient units; replication and snapshot jobs with chained targets still run as rynasd tasks (they become `After` triggers later). A rynasd task the unit follows that disappears with a restart now ends the follow at once instead of hanging until `RuntimeMaxSec`.
- **Upgrade**: only the services change (one line); timers are byte-identical (compat test), so `refresh_rendered` rewrites the services, runs one `daemon-reload` and starts (never restarts) the timers. The systemd test checks that a changed service and a reload keep the timer's stamp and `LastTriggerUSec`.
- Not in S2: R8 (`daemon-reload` during a random delay), the notification rules other than `on_missed`, conditions and windows (S4/S5).

S0–S1 can ship in one release without visible change except the new editor. S2 is the risky step and needs systemd integration tests in `packaging/test-deb-systemd.sh`.

### 3.11 Risks

- **R1 Gate availability.** If rynasd is down, internal actions fail anyway, but user scripts would now depend on rynasd. Mitigation: socket activation (`rynasd.socket` exists). User tasks fail-open after a 30 s timeout, logged as `gate_unavailable`. **S2 chose fail-open for every task after 20 s** (a pause in the pause file still holds), recorded as `ungated`: see "S2 as built".
- **R2 `ExecCondition` and `Persistent=`.** A skipped run counts as triggered, so the stamp advances and no catch-up loop happens. That is the desired behaviour, but it must be covered by a systemd test (the trixie version), including `RandomizedDelaySec` + `Persistent` on boot. **S2:** checked on 257.13 (skip = `exec-condition`, stamp advanced, `ExecStopPost=` still runs).
- **R3 Queueing changes timing.** Serializing heavy jobs per volume can push a backup hours later behind a scrub. Mitigation: scrub yields (resumable), priorities, the overview shows the delay, and it stays opt-in until D4.
- **R4 Granularity loss.** Quiet hours today accept minutes ("22:30"). A 1-hour grid would round them. Keep `QuietWindow` as an alternative representation (blocks with minutes) inside `WeekWindow`, or accept 30-min cells (336). Decide during S3.
- **R5 Agent compatibility.** Older agents ignore new plan fields. The NAS keeps sending `interval_hours`/`start_delay_min` until the minimum supported agent version understands `Interval`.
- **R6 Two editors for one schedule.** Owner-locked fields must be enforced in the backend, not only hidden in the UI. Today only dedup is protected server-side (`rynasd/src/scheduler.rs:337-341`); power-off, backup and snapshot tasks can still be edited through `PUT /scheduler/tasks/{id}`. **Done after the S1 review (2026-11-16):** `normalize_task` always recomputes `owner` from the action (a client cannot claim or shed a feature's locks) and refuses a catch-up for owners that lock `missed` (a missed shutdown or deduplication run never runs right after boot); `PUT /scheduler/tasks/{id}` refuses a change of what the owner decides (`rynas_scheduler::domain::check_locked`: action, schedule expression or boot delay, and the catch-up where locked; options such as nice, timeout, notifications stay free), and the Task Scheduler page shows the schedule of a job's, replica's or power rule's task read-only.
- **R8 `daemon-reload` during `RandomizedDelaySec`** can skip a run (RH bz 2193262). Every rynas apply reloads systemd. Mitigation: the gate records the expected run. After an apply, rynasd compares each timer's `NextElapseUSecRealtime` and `LastTriggerUSec` with the expected time and starts a skipped run through the normal path (outcome `recovered`).
- **R9 Frequency gates.** Restic prune/check already use "at most every N days". The same `RunPolicy.min_interval_days` could serve scrub (as TrueNAS threshold days) and SMART long tests, so a daily trigger survives downtime well. It is optional and easy to add in S2.
- **R7 Wasm size.** Keep the evaluator on the server for previews. The UI gets only the `Recurrence` render/parse (tiny).

### 3.12 Decisions for the owner

**D1 — Engine.**
- (a) systemd timers + rynasd gate (hybrid)
- (b) internal scheduler in rynasd, with systemd only to start units
- (c) keep today's mix and only unify the UI

*Recommendation: (a).* It keeps what already works (persistent units, catch-up at boot, drift and revisions, `list-timers`) and puts every policy in one place. (b) means rewriting anacron and DST logic and losing visibility. (c) does not fix missed runs, locks or history.

**D2 — How a schedule is written.**
- (a) structured `Recurrence` presets + "Advanced" OnCalendar (cron accepted on input) + `Interval` + `Window`
- (b) OnCalendar only, everywhere
- (c) cron

*Recommendation: (a).* Presets cover more than 95 % of cases with the same widget, and OnCalendar keeps full power. Cron has no seconds, no time zone and no catch-up.

**D3 — Debian's own timers (apt-daily-upgrade, mdcheck, fstrim, e2scrub).**
- (a) keep them, add rynas `ExecCondition` drop-ins so they respect pause, locks and the maintenance window, and show them as *system* tasks in the overview and history
- (b) replace them with rynas tasks (u-u started by rynas, `mdadm --action=check` by rynas)
- (c) leave them alone (read-only, as today)

*Recommendation: (a)* for apt and mdcheck. Debian keeps working if rynas is removed, and conflicts disappear. (c) for fstrim and e2scrub, which are light.

**D4 — Heavy work on the same volume.**
- (a) serialize per volume by priority, with scrub made resumable so it yields like dedup
- (b) keep today's "heavy jobs run in parallel, only dedup yields"
- (c) serialize only during the global maintenance window

*Recommendation: (a).* On HDD arrays, parallel scrub + backup + replication makes all of them slower and wakes everything. This is the one behaviour change. It ships behind its own setting, on for new installations and offered (not forced) on upgraded ones.

**D5 — Default for missed runs and a global maintenance window.**
- (a) per-job defaults as today, plus a catch-up deadline (for example 24 h for backups) and an optional global window, off by default
- (b) every job catches up, no deadline
- (c) a global "night window", on by default (for example 00:00–07:00), for heavy jobs and updates

*Recommendation: (a).* PC maintenance should move from "skip" to "catch up once within 12 h": today a NAS that is off at 03:00 never prunes.

(Optional **D6 — Retry by default**: 2 retries after 15 and 60 min for backups and replication, none for the rest. Recommended yes.)

---

## References

- systemd.timer: https://www.freedesktop.org/software/systemd/man/latest/systemd.timer.html
- systemd.time: https://www.freedesktop.org/software/systemd/man/latest/systemd.time.html
- systemd-run: https://www.freedesktop.org/software/systemd/man/latest/systemd-run.html
- systemd NEWS (v235 time zones, v247, v258): https://github.com/systemd/systemd/blob/main/NEWS
- daemon-reload skipping a randomized run: https://bugzilla.redhat.com/show_bug.cgi?id=2193262
- Debian trixie systemd package: https://packages.debian.org/trixie/systemd
- cron(8), anacron(8), flock(1) on Debian trixie: https://manpages.debian.org/trixie/cron/cron.8.en.html , https://manpages.debian.org/trixie/anacron/anacron.8.en.html , https://manpages.debian.org/trixie/util-linux/flock.1.en.html
- Kubernetes CronJob: https://kubernetes.io/docs/concepts/workloads/controllers/cron-jobs/
- Proxmox VE admin guide (calendar events, vzdump): https://pve.proxmox.com/pve-docs/pve-admin-guide.html , https://pve.proxmox.com/pve-docs/chapter-vzdump.html
- Proxmox Backup Server: https://pbs.proxmox.com/docs/calendarevents.html , https://pbs.proxmox.com/docs/maintenance.html , https://pbs.proxmox.com/docs/prune-simulator/
- proxmox-time `calendar_event.rs`: https://git.proxmox.com/?p=proxmox.git;a=blob_plain;f=proxmox-time/src/calendar_event.rs;hb=HEAD
- TrueNAS periodic snapshots, scrub, SMART, replication: https://www.truenas.com/docs/scale/dataprotection/periodicsnapshottasks/periodicsnapshottasksscreens/ , https://www.truenas.com/docs/core/13.3/uireference/tasks/scrubtasks , https://www.truenas.com/docs/scale/scaletutorials/dataprotection/smarttestsscale , https://www.truenas.com/docs/scale/23.10/scaletutorials/dataprotection/replication/advancedreplication/useadvancedreplicationscale/
- Commercial NAS task scheduler help: https://kb.synology.com/da-dk/DSM/help/DSM/AdminCenter/system_taskscheduler ; crontab FAQ: https://www.qnap.com/en/how-to/faq/con_show.php?cid=3042
- borgmatic: https://torsion.org/borgmatic/how-to/set-up-backups/ , https://torsion.org/borgmatic/reference/configuration/consistency-checks/
- restic locks: https://restic.readthedocs.io/en/stable/100_references.html
- Home Assistant triggers: https://www.home-assistant.io/docs/automation/trigger/
- Windows Task Scheduler: https://learn.microsoft.com/en-us/windows/win32/taskschd/tasksettings , https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-multipleinstancespolicy-settingstype-element , https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-restartonfailure-settingstype-element
- jiff time zones and disambiguation: https://docs.rs/jiff/latest/jiff/tz/index.html , https://docs.rs/jiff/latest/jiff/tz/enum.Disambiguation.html
- croner (DST policy): https://github.com/hexagon/croner-rust ; cron crate: https://github.com/zslayton/cron
- Internal: `docs/research/scheduler-lxc.md` (original Task Scheduler analysis), `docs/phase2/week-grid.md`, `docs/phase2/dedup.md` §3.7 (FsLocks)
- systemd 257.13 sources read for S0: `src/shared/calendarspec.c` (257.13 differs from 257.0 in `find_next()`: the "stuck" DST fix), `src/core/timer.c` (`Persistent=` base), `src/analyze/analyze-calendar.c`
