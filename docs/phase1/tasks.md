# Long work and events

How rynas runs work that takes time, how every view finds it again, and how the daemons learn
that something changed without looking every few seconds. Rules A20, A20b, A20c and S1b of
[guidelines.md](../quality/guidelines.md) point here. API types in `rynas-api-types`
(`TaskInfo`, `TaskEvent`), the engine in `crates/rynas-tasks`.

## Long work

**One action, one server task.** Everything a button starts that may take more than a moment
runs in rynasd as one `rynas-tasks` task: all its steps (a loop over the selected items, a check
then a change, the result kept for the client) are inside the job. The page only starts it
(`POST` → 202 `TaskStarted`) and shows it. Closing the dialog, the page or the browser changes
nothing: the task goes on and its outcome is reported (below).

Never chain steps in the browser: no `for item in list { run_task(..).await }` in a
`spawn_local`, no "follow the task, then post its result". When the tab closes halfway the rest
never runs and nobody learns how it ended. Example: Apps › Updates › "Update selected" is one
task `apps.update-many` (`POST /api/v1/apps/update-many`), not one task per app started from the
page.

**Subject.** A task about one thing (a stack, a PC, a volume) is started with
`TaskManager::spawn_on(kind, Some(subject), user, job)`. `TaskInfo::subject` lets any view find
it, and `TaskManager::spawn_once(kind, subject, user, job)` checks for a running task and
starts one in a single step under the task list's lock (`Spawned::New` or
`Spawned::Running`): two requests at once never start two tasks.

**Partial outcome.** A job that stops at its time budget, or skips items, calls
`TaskCtx::partial(why)` and returns `Ok`. The task ends with `status: ok` and
`partial: "<why>"`: the task panel shows "Partial" (a warning), the log ends with
`TASK PARTIAL: <why>`, the notification engine raises `task.partial` (a warning on the key
`task:<type>`, closed by the next full success). A flag and not a new status, so clients built
before it (the Android app) keep parsing `status` and see `ok`. Examples: a space preview that
ran out of its time limit, "Update selected" with apps that need answers.

**Heavy commands.** `rynasd::heavy::heavy_cmd(program)` builds a command at the lowest
priorities (`nice 19`, idle I/O class, set in the child before `exec`), with a clean
environment and a last-resort timeout of 24 h. Run it with `TaskCtx::run` (async) or
`TaskCtx::run_blocking` (on the job's blocking thread): both kill the command's process group
when the task is cancelled. `Cmd::run_blocking` alone does not know the task; use
`Cmd::run_blocking_in(Some(token))` where only a token is at hand. A command whose progress shows
(it prints as it goes, or the caller reads its progress elsewhere and bumps an `Activity`) gets
`heavy::stalled_after(cmd, activity)`: it is stopped after 10 minutes without any sign of life,
never at a guessed fixed time while it still moves. A quiet command (`du -s`) keeps a timeout
of its own. What must run in a unit of its own (outside the sandbox, memory limits) uses
`rynas_tasks::Outside` with `Nice=19` and `IOSchedulingClass=idle`.

## Showing a task (UI)

The task store (`crates/rynas-ui/src/tasks.rs`) holds the newest 50 tasks, read at sign-in and
kept current by the `task.*` events of `/api/v1/events`. Nothing is polled.

- `tasks::use_task(kind, subject)` (or `use_task_by(kind, filter)`) returns the newest matching
  task, running or ended within the last 10 minutes, and `running`. The view shows "running",
  progress, the log (`tasks().show(id)`) and Stop from it, also after a reload and for a task
  someone else started. `UseTask::on_end(f)` runs `f` once when a task ends while the view is
  mounted (read the result, reload the list, say how it went).
- `run_and_report` / `TaskRun` / `follow` stay for a task the page starts and watches to its
  end while the user waits in a dialog.
- A partial end is never shown as a success: `task_tone`, `task_status_key` and `task_error`
  take the whole `TaskInfo`.

**Global toast.** When a task the signed-in user started ends and no view reports it, a toast
says so: a short "<task>: done", or the error (or the partial reason) with a "Log" button. A
view reports a task when it follows it (`follow`, `run_and_report`, `TaskRun`), mounts a
`use_task` for its type, or calls `tasks::reports(kind)` (a page that follows its tasks another
way, e.g. the PC list that names the running measure). A view registers the type and, for
`use_task` with a subject, that subject only. The decision waits 1.5 s (plus up to 0.4 s, a
different delay per tab), so a page that starts a task and follows it has registered by then;
the first tab of the browser to toast notes the task in local storage and the other tabs stay
quiet (a tab whose page reports the task does not tell the others). `config.*` tasks are left
to the pending bar; cancellations say nothing.

**Without a live stream** (a reconnect that takes long, or a proxy that holds the stream back:
"live" but no metrics sample for 20 s) the task store and the pending changes are read every
15 s while the tab is visible, so `use_task` views and the pending bar still move.

## Events, not loops

No loop wakes the NAS to look for a change that the kernel or another component can announce.

**`rynasd::sysevents`** is the one watcher of storage events: one thread in `poll(2)` on

- the udev netlink socket (`block` add/remove/change; udevd's group when udevd runs, so the
  event comes after its rules, the kernel's otherwise; a dropped burst is an unknown change),
- `/proc/self/mountinfo` (`POLLPRI` on every change of the mount table),
- `/proc/mdstat` (`POLLPRI` on md events).

Subscribers call `sysevents::subscribe()` and wait with `next_burst(rx, quiet, filter)`: a burst
(a disk with its partitions, a container start) is one wake-up. A lagged subscriber treats the
lag as "everything may have changed". The thread is restarted whenever it stops (pause doubling
up to 60 s; also when udevd starts after rynasd, to switch to its group): each stop is
`SysEvent::Lost`, each restart `SysEvent::Restarted` (a burst for everyone), and
`sysevents::alive()` says whether events flow, so a subscriber can run its old pace while they
do not (the removable drives go back to 2 s). Mount changes are filtered with `MountFilter`
(block-device mounts only). Netlink messages are taken only from the kernel (port id 0) in the
kernel's group, or from root (`SCM_CREDENTIALS`) in udevd's group. `sysevents::storage_changes()` is the debounced "the
storage overview may have changed" (block and md events; mount changes only when the set of
block-device mounts changed), sent on the task stream as `storage_changed` (`?storage=1`) and by
rynas-web on `/api/v1/events` as `storage.changed`.

Other sources: inotify for files (the watched certificate folders: a thread only while an
import has a watched folder), the configuration's pending-change events
(`Ops::subscribe_pending`: a stage, discard, apply, restore or import) for anything derived from
the applied configuration.

A timer stays only as a slow safety net next to an event source (≥ 60 s), or while someone
watches.

| Loop (before) | Now | Wake-ups when idle |
|---|---|---|
| Removable drives scanned every 2 s (rynasd) | on block or mount events (settled 0.7 s), safety net every 60 s | 30/min → 1/min |
| Watched certificate folders, 2 `stat` every 2 s (thread always on) | inotify; thread only while an import has a watched folder, wakes at the end of a settle window | 30/min → 0 |
| Scheduler tasks of rynas's jobs checked every 60 s | when a new configuration revision is written; hourly safety net | 1/min → 1/h |
| Photos mount watcher (its own thread) | a subscriber of `sysevents` (one thread for all) | 0 → 0, one thread less |
| UI: storage overview every 30 s | on `storage.changed`; fill levels every 5 min while visible | 2/min → 0.2/min per tab |
| UI: `/config/pending` every 30 s | on `config.pending` events and when the tab becomes visible | 2/min → 0 per tab |
| rynas-web: paired devices swept every 10 s, rynasd asked each time | every 60 s, rynasd asked only while a device has a session | 6/min → 1/min (no rynasd call) |
| rynas-web: metrics sampled every second | every second while a client listens, every 10 s otherwise (history by time; a first client wakes the collector at once) | 60/min → 6/min |
| QNAP worker 8 Hz (buttons, blink) | 4 Hz when nothing blinks and no button moves, 8 Hz otherwise; a level change is confirmed by a second read 30 ms later (gpio-f7188x has no interrupts), so a 150 ms tap is caught 60 % of the time instead of 20 % | 480/min → 240/min |

## Not converted yet

The base above is in place; these follow it feature by feature (new code follows the rules
from now on):

- **Pages that follow a task only while open** (`follow`/`run_task` in a dialog, result lost on
  reload): PC agent actions, replication, encrypted shares, restic, dedup, UPS, Docker stacks
  and images, user homes, share folders. Each gets a subject on the server and `use_task` on
  the page.
- **Heavy commands not yet on `heavy_cmd`**: stack data copies (`docker::copy_cmd`, `cp -a`),
  home copies (`rsync`, `cp -a /etc/skel`), the space measures (FIEMAP walk, `btrfs subvolume
  sync`), the PC backup measure; `cmd_ok` in `pc_backup` still ignores cancellation (its
  callers have no task).
- **Time budgets that should end partial**: the space auto-free pass (15 min), other budgets
  found per feature.
- **Loops still on a timer**: rynas-web TLS reload check (10 s) and listen check (5 s), web
  access sync (30 s), the storage notification watch and the degraded-start check (60 s, could
  subscribe to `sysevents`), rynas-web hardware notification check (30 s), the automatic Docker
  update tick (60 s), disk standby (30 s, reads diskstats by design); in the UI the
  `live::poll` timers of the task panel's queue, storage, scheduler, schedule, Wake-on-LAN,
  Syncthing and the hardware module pages, and the `poll_while` progress loops (scrub, dedup,
  space scan, PC receive) that task progress events could replace.
