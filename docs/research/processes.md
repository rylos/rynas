# Processes: research and design (D-PROCESSES)

Status: research and first implementation, 2026-11-11. Decision entry: `D-PROCESSES` in
`docs/phase0/05-decisions.md`; API in `docs/phase1/plan.md` ("API v1: processes").

The question: what should a process view look like on a NAS manager whose users think in
services, shared folders and apps, not in PIDs? It must answer "what is eating my CPU, RAM or
disks right now, and what can I do about it", cheaply enough to run on a 2-core Celeron with
2 GB of RAM.

## 1. What the reference tools do

### btop (main reference)
Source: <https://github.com/aristocratos/btop> (README, `btop.conf` options, `src/linux/btop_collect.cpp`).
- One process box with a **flat list or a tree** (`proc_tree`), tree branches collapsible, and
  an option to **aggregate children into the parent** (`proc_aggregate`), auto-collapse of
  parents with many children.
- Sort by pid, program, arguments, threads, user, memory, CPU ("lazy" = smoothed and
  "direct" = last sample). Reverse with one key.
- **Filter** as you type (name, command line, user; `!` for a regex), kernel threads hidden
  on Linux (`proc_filter_kernel`).
- **Detail panel** of the selected process: status, elapsed time, parent, user, threads,
  memory in bytes, command line, and two small graphs (CPU and memory) of that process only,
  which start when the process is selected. "Follow" keeps the selection when the list moves.
- Signals: terminate, kill, or any signal from a menu, always with a confirmation line.
- Default refresh 2000 ms; memory from RSS, more exact numbers from `smaps` only on request
  ("slow", off by default).
- What makes it pleasant: the per-process mini graph, sorting that does not jump every frame
  (lazy CPU), colour gradients for load, everything one key away.

### htop
Source: <https://htop.dev>, `htop(1)`, `linux/LinuxProcessTable.c`.
- Tree (F5) and flat view, incremental search (F3) and filter (F4), hide kernel threads (K)
  and userland threads (H).
- Columns on demand: `IO_READ_RATE`/`IO_WRITE_RATE` from `/proc/<pid>/io` (root needed for
  other users' processes), `PSS` from `smaps_rollup` (expensive, optional), `CGROUP` and a
  compressed cgroup (`/s/sshd.service`), OOM score, start time, CPU affinity.
- Nice up/down (F7/F8), I/O priority (`i`), kill with a signal list (F9), follow (`F`),
  `l` for open files (lsof), `s` for strace.
- Reads `/proc` directly, keeps the previous tick per PID to compute deltas.

### Windows Task Manager and Resource Monitor
- Task Manager's Processes tab **groups by what the user knows**: "Apps", "Background
  processes", "Windows processes", each app expandable to its processes and **sums** for the
  group (CPU, memory, disk, network). "End task" on the group, "Go to details" for PIDs,
  "Restart" on services in the Services tab; protected system processes refuse to end or warn
  that the system will become unstable.
- Details tab: PID, status, user, priority, affinity; "Set priority" with named levels.
- Resource Monitor: per-process **disk read/write bytes per second**, **listening ports per
  process** (address, port, protocol, firewall status), a filter checkbox that applies to every
  pane at once.

### GNOME System Monitor and KDE System Monitor (Plasma)
- GNOME: views All / My / Active processes, "Show dependencies" = tree, Change priority with
  **named levels** (Very high, High, Normal, Low, Very low) plus a custom nice, End and Kill
  with confirmation, Properties (command line, cgroup, security context, waiting channel),
  memory maps and open files.
- KDE System Monitor: an **Applications** page that groups processes by the cgroup systemd
  gives each app (`app-*.scope`), next to the process table; terminate/kill/renice; columns
  for I/O rates and PSS-like "shared/private" memory.

### Netdata apps.plugin and cgroups
Source: <https://learn.netdata.cloud/docs/collecting-metrics/processes-and-system-services/applications>,
`src/collectors/apps.plugin/`.
- Groups every process into an **application group** (by name patterns, users, user groups)
  and charts per group: CPU (user/system), RSS and private memory, physical and logical disk
  I/O, open files, processes, threads, uptime. A process belongs to the group of its nearest
  matching ancestor, so children count for the service that started them.
- The cgroups collector does the same per systemd unit and per container, from the cgroup
  files: one read per group instead of one per process.

### Other NAS systems
- DSM Resource Monitor › Task Manager: a **Services** view (packages and system services with
  their CPU and memory, expandable to processes) and a Processes view. Stopping is done on the
  service or package, not on the PID.
- QTS Resource Monitor › Processes: sortable list with **"Group by application"**, so the
  processes of an app or system feature are counted together; QNAP's own advice for a hog is
  to disable the app, not to kill the process.
- TrueNAS and Unraid have no process manager in the web interface (a shell, `htop`, or a
  plugin); container usage is shown on the container pages.

## 2. What rynas takes

| Idea | From | Why it fits rynas |
|---|---|---|
| Group by what the user knows: system service, container (named after the catalogue app when it is one), user session, user services, kernel | Task Manager, QTS, DSM, KDE, Netdata | the rest of rynas speaks of services, apps and containers; a PID means nothing to most NAS owners |
| Group totals (CPU, memory, disk I/O) and the group row first, processes below | Task Manager, Netdata | answers "which service" in one look |
| Flat view sortable by CPU, memory, disk read, disk write, age, name | btop, htop | the expert answer, one click away |
| Tree view (parent/child), kernel threads hidden by default | btop, htop, GNOME | shows who started what (smbd children, container init) |
| Filter as you type on name, command line, user, group | btop, htop | finds `rsync` or a user's session quickly |
| Detail panel with command line, user, state, threads, nice, I/O class, cgroup/unit, start time, listening ports, PSS, and a CPU/memory graph of that process | btop (graphs), Resource Monitor (ports), GNOME (properties) | everything needed to decide what to do, no shell |
| Restart the service or container instead of killing a process it would restart | Task Manager (services), DSM/QTS advice | systemd restarts a killed service process anyway; restart is the real fix |
| Terminate/kill with confirmation, priority with named levels | btop, GNOME | familiar and safe |
| Protected processes: no kill buttons, and the server refuses | Task Manager | a click must never take rynas, SSH, PID 1 or the kernel down |
| Disk I/O rates per process from `/proc/<pid>/io`, per group from the sum | htop, Resource Monitor, Netdata | on a NAS the disks are the scarce resource |
| CPU % from deltas between two samples, 2 s refresh, previous tick kept per PID | btop, htop | standard and cheap |

## 3. What rynas leaves out, and why

- **Any signal from a list** (HUP, USR1, STOP…): only terminate and kill. Reloads belong to
  the service's own page, STOP leaves things hung. Not needed by the people rynas is for.
- **strace, lsof, memory maps, open files**: expert debugging that needs a shell anyway and
  costs a lot to collect. Listening ports are the one "open files" view that is useful on a
  NAS, so they stay (detail panel only).
- **CPU affinity, realtime priorities, realtime I/O class**: easy to hurt the box with, no
  NAS use case. Nice is offered as five named levels (−10, −5, 0, 10, 19; the server takes
  −20…19) and I/O as best-effort or idle.
- **PSS for every process**: reading `smaps_rollup` for hundreds of processes every 2 s is the
  slow path btop and htop keep off by default; rynas shows RSS in the list and PSS in the
  detail panel of the one selected process.
- **Per-process network rates**: needs eBPF or netlink accounting; the dashboard already shows
  network per interface and Docker shows it per container.
- **Long history per process**: memory grows with every PID. The server keeps only the
  previous sample per PID (dropped when nobody looks); the graph history lives in the browser,
  for the selected process only, at most 150 points (5 minutes).
- **Background sampling**: nothing runs while nobody has the page open (see §4).
- **A terminal look** (colour gradients on every cell, key bindings for everything): rynas
  uses its own components, tables that turn into cards on phones, and the usual meters.

## 4. Design for rynas

- **Where**: rynasd (root) reads `/proc`; rynas-web forwards. rynasd can read every process's
  `io` and command line, and it already owns the Docker and catalogue state the groups need.
  The unit gains `CAP_KILL` and `CAP_SYS_NICE` (signals and priorities of other users'
  processes) and `CAP_SYS_PTRACE`: `/proc/<pid>/io` and the `fd` links of other users'
  processes need it (Marco, 2026-11-11: the page must see everything). rynasd only reads;
  it never attaches, and it already holds `CAP_SYS_ADMIN`.
- **Sampling only on demand**: `GET /processes` takes a sample when the last one is older
  than 1 s (concurrent viewers share it) and computes CPU % and I/O rates against the
  previous sample of the same PID (same start time: no confusion with a reused PID). The
  previous-sample table is dropped 60 s after the last request. No thread, no timer.
- **Cost per sample**: `/proc/stat`, `/proc/meminfo`, and per PID `stat` and `io`; the command
  line, owner and cgroup are read once per process and kept while it lives. Kernel threads are
  read only when asked for.
- **Groups**: from the cgroup path (`*.service`, `docker-<id>.scope` or `docker/<id>`,
  `session-N.scope`, `user@UID.service`, kernel threads = PF_KTHREAD); containers are named from
  Docker, and from the catalogue when the stack is an installed app (looked up again only when
  the set of containers changes).
- **Actions** (administrators, CSRF, audit lines `rynas::audit`): `signal` (term, kill),
  `priority` (nice, I/O class), `restart-service` (task, `systemctl restart`). Containers are
  restarted through the existing Docker route. Each action names the PID **and its start
  time**; a different start time is `not_found` (the PID was reused).
- **Protected**: PID 1, kernel threads, every process of `rynasd.service` and
  `rynas-web.service`, the main process of `ssh(d).service`, journald, logind, udevd, networkd
  and D-Bus. No kill, priority or restart buttons; the server answers `403 process_protected`.
  Under `--dry-system` no signal, priority or restart ever reaches the host.
- **Polling at 2 s while the page is visible** instead of an SSE stream: the list exists only
  for this page, a stream would cost a connection per tab and a sampler task; the request is
  marked as background after a minute without input (A24b). This is a written exception to
  S1's "≥ 5 s" (see D-PROCESSES).
