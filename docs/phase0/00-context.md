# rynas — Phase 0: context and constraints

Date: 2026-11-04. Status: design draft, no code.

## Vision

rynas is a NAS manager for Debian written in Rust: a web interface (WebAssembly) that
loads modules/plugins. It takes the best ideas from Synology DSM, TrueNAS, OpenMediaVault and
Unraid. It must be advanced, professional and very fast, and run on any Debian homelab
server. It is built first for **synodeb** (Debian 13 on the Synology DS923+/DS920+).

## Constraints set by the maintainer

| Topic | Constraint |
|---|---|
| Language | Rust (backend and WASM frontend) |
| Architecture | modular: core + plugins that add features, including in the UI |
| Target | Debian (13 trixie), installable as a `.deb` package; ISO only later |
| First user | synodeb, with a **synology** plugin for hardware info and controls |
| Base features | Samba, users/groups, the main features of a NAS |
| UI | light and dark theme, **Fusion** palette |
| File manager | model: **SynoFile** |
| License | "Orlok style" (BSL 1.1 + grant for personal/non-commercial use) |
| Business model | like Proxmox: free community repo + paid business repo |
| Method | small, well-defined work units; symbol-level code navigation |

## Starting material (the maintainer's projects)

### [synodeb](https://github.com/rylos/synodeb) (GPL-2.0)
- Kernel with the `synology_nas` platform driver (sysfs in `/sys/devices/platform/synology-nas`:
  `bayN_present`, `bayN_power`, `usb_vbus`; hwmon `synology` with `fan1_input`, `fan1_alarm`, `pwm1`).
- **synohwd** daemon (Python, 1081 lines): DSM-style fan curves, LEDs, beeper, buttons, bays,
  power schedule (RTC), WoL, restore after power loss, disk standby, thermal shutdown.
  - Status: `/run/synohwd/status.json` (sensors, fans, bays; written every `fan_period`)
  - Commands: FIFO `/run/synohwd/cmd` (validated sequences of microcontroller commands)
  - Config: `/etc/synohwd.toml`
- UPS with NUT and DSM-style Safe Mode; e-mail/Telegram/ntfy notifications.
- → The **rynas-synology** plugin starts by reading these files. In a later phase synohwd can
  be rewritten in Rust as part of the plugin (decision D-HW, see `05-decisions.md`).

### [SynoFile](https://github.com/rylos/synofile) (MIT)
File manager for DSM, the model for the rynas file manager. To be ported:
- tabs, dual pane, `eza`-style list view, grid, columns; keyboard first (Vim keys)
- transfer dialog: speed graph, ETA, pause, rate limit, conflicts, xxh3 verification;
  copies to `.part` + rename, **reflink** on btrfs, journal and "Retry" after a reboot
- archives opened as folders; ring-chart space analysis; quick look (HEIC in WASM);
  micro-style editor (CodeMirror 6); resumable chunked uploads; SMB/SFTP connections
- security: every operation runs **as the user who requests it** (setresuid, minimal
  capabilities, `PR_SET_NO_NEW_PRIVS`), separate worker for transfers
- what changes: no DSM (`authenticate.cgi`, `synorecycle`, DSM nginx) → rynas login,
  own recycle bin (`#recycle` compatible), own HTTP server.
- The SynoFile frontend is JS (ES modules + CodeMirror). To decide: rewrite it in Rust/WASM
  or keep it as a plugin with a JS UI (decision D-FM).

### Fusion (palette)
Tokyo Night as used in Fusion (a MudBlazor app by the same author). Already used by SynoFile and DSM Dark.

| Role | Light | Dark |
|---|---|---|
| Primary | `#2959aa` | `#7aa2f7` |
| Secondary | `#5a3e8e` | `#bb9af7` |
| Tertiary | `#0f4b6e` | `#2ac3de` |
| Info | `#0f4b6e` | `#7dcfff` |
| Success | `#3a8f7f` | `#73daca` |
| Warning | `#df8e1d` | `#e0af68` |
| Error | `#8c4351` | `#f7768e` |
| Background | `#e6e7ed` | `#1a1b26` |
| Background gray | `#d5d6db` | `#16161e` |
| Surface | `#ffffff` | `#24283b` |
| Drawer | `#fafafa` | `#12121c` |
| Appbar | `#e6e7ed` | `#1f2335` |
| Text primary | `#343b58` | `#c0caf5` |
| Text secondary | `#40434f` | `#a9b1d6` |
| Text disabled | `#6c6e75` | `#565f89` |
| Divider / lines | — | `#292e42` / input `#414868` |

### Orlok (license)
Business Source License 1.1. Additional Use Grant: free production use for personal and
non-commercial purposes (non-profits, schools); companies, freelancers and MSPs need a commercial license.
Change Date 4 years → Apache 2.0. Name and logo excluded.
