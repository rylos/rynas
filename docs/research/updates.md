# Updates page: what to take from DSM, Proxmox VE, Cockpit and TrueNAS

Short UI study for System › Updates (D-UPDATES, P6). The policy side (channels, windows,
orchestration, synodeb kernel) is in `notifications-ups-updates.md` §3; this file is only about
the page and the first, apt-based version of it. Checked from the products' current docs and
from memory of their UIs (November 2026).

## What each one does

| | List | Refresh | Install | Automatic | Reboot | Changelog |
|---|---|---|---|---|---|---|
| **DSM 7** (Control Panel › Update & Restore) | one block "DSM x.y-z Update n" with release notes; packages apart in Package Center | "Check for updates" button, daily check in the background | "Download" then "Update now", one confirm, progress bar | radio: notify only / install important (security) updates / install all; a weekly time ("check schedule") | automatic when needed, announced | release notes link per update |
| **Proxmox VE** (node › Updates) | flat table: package, current version, new version, description; grouped by origin | "Refresh" = `apt-get update` as a **task with a log window** | "Upgrade" opens a shell running `apt-get dist-upgrade` (you see and answer apt) | none built in (a `package-updates` notification instead) | "reboot required" not shown; kernel shown in node summary | **"Changelog" button per package**, fetched on demand |
| **Cockpit** (Software Updates) | table with severity icon per row (security / bug fix / enhancement), versions, details expander | automatic on page open (PackageKit cache), "Check for updates" button | "Install security updates" **and** "Install all updates" side by side, progress with live log | card "Automatic updates": off / security only / all, **day + time**, reboot after (dnf-automatic or unattended-upgrades) | **"Restart needed" banner** with "Restart now" and the services that need a restart | details expander shows the changelog/advisory |
| **TrueNAS 25.10** | one OS image per train; apps apart | "Check" | download, then apply; boot environments keep the old one | update profiles (conservative / general / early adopter) | needed after apply, user starts it | release notes link |

## Ideas taken

1. **Security first** (Cockpit): a security pill on each row, the count in the summary and a
   separate "Install security updates" action next to "Install all". Security = an origin from
   a `*-security` suite (`Debian-Security` label).
2. **Refresh is a task with a log** (Proxmox): "Check now" runs `apt-get update` as a rynasd
   task; the outcome and the log stay in the task panel.
3. **Install is a task too, but non-interactive** (unlike Proxmox's shell): `apt-get -y
   full-upgrade` with `--force-confdef --force-confold` (local conffiles kept),
   `DEBIAN_FRONTEND=noninteractive`, `NEEDRESTART_MODE=l` (needrestart only lists, it never
   restarts rynasd mid-task).
4. **Show what apt would remove before confirming** (none of them does it well): the
   `apt-get -s full-upgrade` simulation is parsed; removals are listed in the confirmation and
   must be confirmed by name. `autoremove` is never automatic (on synodeb it would remove the
   old synodeb kernel and `synology-ds923`): a separate, listed, confirmed action.
5. **Reboot needed banner** (Cockpit) from `/var/run/reboot-required(.pkgs)`, with the packages
   that asked for it. No "reboot now" button in this version.
6. **Changelog on demand only** (Proxmox): `apt-get changelog` goes to the network, so it is a
   button per row that runs a short task and shows its log.
7. **Automatic updates card** (Cockpit/DSM): off / security only / all, a start time and a
   window, automatic reboot off or at a time. Implemented with unattended-upgrades (Debian's
   tool, already configured by synodeb), staged and applied like every rynas setting.
8. **Kernel / DOM card** (Synology-specific, nobody else has it): running kernel vs installed,
   the DOM boot entries, default and fallback, trial state, outdated warnings and DOM free
   space from `synodeb-dom status --json`. Read-only, refreshed by hand.
9. **Cheap page load**: the list comes from a cached `apt-get -s` simulation, recomputed only
   when `/var/lib/dpkg/status`, `/var/cache/apt/pkgcache.bin` or the lists change; a daily
   check notifies (warning when security updates are waiting).

## Left out for now

- Restore point before installing (D-UPDATES orchestration; needs the root rollback work).
- Reboot from the page, needrestart service list, apt-listchanges NEWS blocking.
- Per-row "description" column (needs `apt-cache show`: not cheap for 100+ packages).
- rynas/plugin and Docker app channels (B, C of D-UPDATES).
