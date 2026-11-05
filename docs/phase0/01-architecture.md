# rynas — Architecture (proposal)

Source: `research/02-architecture.md` (details, alternatives, URLs). This is the summary to decide on.

## Principles
1. **API first**: the UI and the CLI are clients of the same versioned API (`/api/v1`).
2. **The system is the source of truth for state, the DB for intent**: disks, RAID and mounts are always
   read from the system; the DB only holds policies and desired configuration (avoids the Cockpit/45Drives flaw).
3. **Pending changes → Apply** with diff, validation and rollback (OMV done right).
4. **Least privilege**: the web server is not root; plugins are not root by default.
5. **Lightweight**: must run comfortably on a Celeron J4125 / Ryzen R1600 with 4 GB; zero cost for unused plugins.

## Processes

```
Browser — Leptos CSR (wasm) + plugin Web Components
   │ HTTPS: REST /api/v1 · SSE (metrics, tasks, events) · WS (terminal, live logs)
   ▼
rynas-web  (rynas user, systemd hardening)
   ├ precompressed static UI (brotli), sessions, CSRF, ACL, schema validation
   ├ metrics collector /proc /sys → RRD ring buffer
   └ plugin router (merges manifests)
   │ HTTP over Unix socket (SO_PEERCRED)      │ JSON-RPC over Unix socket (socket activation)
   ▼                                          ▼
rynasd (root)                              rynas-plugin-<id>  (one service per plugin)
   ├ PAM, configuration apply, root tasks     e.g. synology (sysfs + synohwd), nut, docker…
   ├ SQLite config.db (revisions)
   ├ zbus → systemd1, UDisks2
   └ exec with timeout: smartctl, mdadm, lvm, btrfs, zfs, testparm…
```

## Stack

| Area | Proposed choice | Alternative |
|---|---|---|
| Frontend | **Leptos 0.8 CSR** (static files, no SSR) | Yew + pwt (Proxmox PDM toolkit) |
| Styling | own CSS tokens (Fusion palette) + Tailwind v4 | DaisyUI 5 / Thaw |
| Charts | uPlot (time series), ECharts lazy-loaded | plotters |
| Backend | axum 0.8 + tokio, rustls, utoipa (OpenAPI 3.1) | — |
| Real time | SSE; WebSocket only for terminal/logs | WebSocket only (TrueNAS-style JSON-RPC) |
| Privileges | two daemons, HTTP over UDS (Proxmox Backup Server model) | varlink (`zlink`) |
| Auth | PAM in rynasd, cookie session + CSRF, TOTP, WebAuthn/passkey, API tokens, path × role ACLs | OIDC in phase 3 |
| Tasks | asynchronous tasks with UPID, log, cancel (PBS style) | — |
| Config | **SQLite** (rusqlite + refinery), changesets/revisions, **minijinja** templates, atomic write + backup + anti-drift hash | versioned TOML files |
| Metrics | internal collector + multi-resolution RRD (`proxmox-rrd` style), optional Prometheus `/metrics` | node_exporter as a plugin |
| Notifications | e-mail (lettre), ntfy, Gotify, Telegram, webhook | — |
| Packaging | workspace + **cargo-deb**, signed **aptly** apt repo, authenticated business repo | debcargo (official Debian) |
| CPU | `x86-64-v2` (J4125 has no AVX2), arm64 as second target | — |

## Plugin system

**Backend**: plugin = **separate process** speaking JSON-RPC 2.0 over a Unix socket,
socket-activated by systemd (`rynas-plugin@<id>.socket`), shipped as its own `.deb` package.
- Pros: any language, isolated crashes, per-plugin systemd sandbox, clean license boundary,
  independent apt updates.
- First-party plugins (storage, smb, nfs, users) use **the same trait** with an in-process
  transport and are compiled into the core via features: no IPC, a single API.
- Rejected: dylib (unstable Rust ABI, shared crashes); WASM component model postponed
  (hardware plugins need sysfs/serial/exec; useful later for sandboxed automations).

**Manifest** (`/usr/share/rynas/plugins/<id>/manifest.toml`):
id, version, `api_version`, methods with input/output JSON Schema, ACL permissions, UI contributions
(menus, pages, widgets, slot panels, settings, palette commands, notifications),
configuration templates, license/entitlement.

**Plugin UI**: hybrid.
- **Declarative** (90% of cases): `form | table | tabs | widget` pages generated from a schema
  by the shell's `SchemaRenderer` → uniform look, no plugin code in the browser.
- **Custom**: Web Component in an ES module (`import()`), with a minimal contract
  (`api-base`, `rynas:navigate` and `rynas:toast` events, CSS tokens). E.g. `<rynas-synology-bays>`.
- Not done: plugin wasm inside the Leptos runtime (two wasm modules share neither allocator nor reactive graph).

**Generic hardware interface `rynas-hw`**: fans, leds, bays, beeper, buttons,
power_schedule, ups. The dashboard shows fans and bays without knowing it is a Synology;
tomorrow `rynas-plugin-ugreen` or `rynas-plugin-hwmon` can implement the same interface.

## Workspace

```
rynas/
├── crates/
│   ├── rynas-api-types/   serde types + JSON Schema (native + wasm32)     [Apache-2.0]
│   ├── rynas-plugin-sdk/  manifest, protocol, plugin server               [Apache-2.0]
│   ├── rynas-schema/      #[api] macro: validation, OpenAPI, CLI
│   ├── rynas-plugin-host/ discovery, socket activation, routing
│   ├── rynas-hw/          generic hardware traits
│   ├── rynas-config/      SQLite, changesets, render, apply/rollback, anti-drift
│   ├── rynas-sys/         zbus systemd/udisks2, smartctl, mdadm, lvm, btrfs, zfs, Cmd
│   ├── rynas-auth/        PAM, sessions, TOTP, WebAuthn, tokens, ACL
│   ├── rynas-tasks/       UPID tasks
│   ├── rynas-metrics/     collectors + RRD + Prometheus
│   ├── rynas-notify/      notification channels
│   ├── rynas-web/   (bin) non-root server
│   ├── rynasd/      (bin) root daemon
│   ├── rynasctl/    (bin) CLI
│   ├── rynas-ui/    (wasm) Leptos shell
│   └── rynas-ui-kit/(wasm) components, SchemaRenderer, uPlot, i18n
├── plugins/ storage, shares-smb, shares-nfs, users, files, synology, nut, docker…
└── packaging/ systemd, postinst, aptly, deb822 sources, auth.conf
```

## synodeb integration
- Phase 1: `rynas-plugin-synology` reads `/run/synohwd/status.json` and the `synology-nas` sysfs, and
  writes validated commands to the FIFO `/run/synohwd/cmd`; config in `/etc/synohwd.toml`.
- Phase 2: synohwd rewritten in Rust as the `synohw` crate, used by both the daemon and the plugin
  (less RAM, no Python), with a real control socket instead of the FIFO.
- UPS: generic `rynas-plugin-nut` plugin (upsd protocol), useful outside Synology too.
