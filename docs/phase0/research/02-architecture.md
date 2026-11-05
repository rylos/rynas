# rynas: architecture proposal

## 0. Choices at a glance

| Area | Choice | Alternative |
|---|---|---|
| Frontend | **Leptos 0.8 in CSR mode** (client only), static files served by axum | Yew + Proxmox toolkit (pwt), for the same stack as Proxmox Datacenter Manager |
| Plugin UI | **Hybrid**: a declarative manifest (JSON Schema → forms, tables, widgets) for 90% of cases; for custom pages, **Web Components** in ES modules loaded with `import()` | per-plugin wasm inside the Leptos runtime (cannot be done well) |
| Backend | **axum 0.8 + tokio**, REST + SSE, OpenAPI with utoipa 5 | WebSocket only for terminal and streaming logs |
| Privileges | **Two daemons**: `rynas-web` (non-root user) and `rynasd` (root), connected by HTTP over a Unix socket, as Proxmox Backup Server does | varlink (`zlink` crate) / D-Bus with zbus |
| Plugin backend | **Separate processes**: each plugin is a binary with a manifest, speaks JSON-RPC over a Unix socket, and is its own systemd service and `.deb` | first-party plugins compiled into the core with feature flags |
| Configuration | **SQLite (rusqlite) as the single source of truth**, with revisions, "pending changes / apply", minijinja templates | redb (rejected: a relational model is needed) |
| Metrics | **Internal collector + RRD-style ring buffer** (modeled on `proxmox-rrd`), optional `/metrics` endpoint | Prometheus and node_exporter as plugins |
| Packaging | **cargo-deb** per crate, apt repo signed with **aptly**, business repo authenticated via `auth.conf.d` | debcargo (Debian and Proxmox style), to get into official Debian |

---

## 1. WASM frontend

### Frameworks compared

| | Leptos 0.8 | Dioxus 0.7 | Yew 0.21/0.22 | Sycamore |
|---|---|---|---|---|
| Model | fine-grained signals, no VDOM | optimized VDOM and signals | VDOM, struct or function components | fine-grained signals |
| Latest version | 0.8.0 (May 2025), 0.8.x active; cargo-leptos 0.8 | 0.7 (September 2025), 0.7.9, then 0.8 in alpha | slow pace | small community |
| Bundle (estimate, average admin app, wasm-opt -Oz + brotli) | 150–400 KB | similar; "<50 KB" for a hello world | larger, VDOM | small |
| Ecosystem | Thaw, leptos-use, leptos_i18n, leptos_router | own components, strong focus on desktop and mobile | **pwt / proxmox-yew-comp** (Proxmox) | scarce |
| Real-world admin UIs | many projects | growing | **Proxmox Datacenter Manager 1.0** and PVE mobile UI | few |

**Choice: Leptos 0.8 CSR.**
- **Performance:** fine-grained reactivity updates only the nodes that change. This matters for dashboards with dozens of metrics per second on any client.
- **Why not SSR + hydration:**
  - a NAS UI sits behind a login and SEO is not needed;
  - SSR ties the frontend build to the server binary;
  - it complicates plugin loading, because the server would have to know how to render their pages;
  - it makes the Celeron CPU work on every page.
- **With CSR** the frontend is a set of precompressed static files (`tower-http::ServeDir` with `precompressed_br`). Cost for the server: zero.
- **Why not Dioxus:** its value is cross-platform (desktop and mobile). `wasm-split` splits code *within a build*, it does not load third-party plugins.
- **Yew remains a serious alternative:** Proxmox chose it (PDM was rewritten in Rust with a new widget toolkit, because upstream ExtJS is effectively abandoned). There, however, the toolkit API is still changing (December 2025 patches removing `TextRenderFn`) and public documentation is scarce.

### Components, i18n, charts
- **CSS: Tailwind v4 + DaisyUI 5** as the shared styling system. It is just CSS, so it works for the Leptos core, for schema-generated pages and for plugin Web Components written with any framework. Thaw is good, but 0.4 declares Leptos 0.7: I have not verified 0.8 support. It would also tie plugins to Leptos.
- **i18n:**
  - core: `leptos_i18n` (keys checked at compile time);
  - plugins: JSON catalogs (`i18n/it.json`) inside the package, loaded at runtime by the schema renderer.
  - Languages from day one: it and en.
- **Real-time charts:** **uPlot** (JS, about 45 KB, the fastest for time series) behind a `wasm-bindgen` wrapper. Rust canvas libraries (plotters) are heavier and less polished.

### Dynamically loading plugin UIs

| Approach | Pros | Cons |
|---|---|---|
| **A. Declarative** (manifest with JSON Schema: formPage, datatablePage, tabs, dashboard widgets; OMV workbench model) | zero JS or wasm in the plugin; uniform look; the plugin can be written in any language; safe (no code in the browser); i18n and validation for free | not enough for very specific UIs (bay map, custom charts) |
| **B. Web Components** (`/plugins/<id>/ui/index.js` exports `<rynas-synology-bays>`) | isolated; a plugin can use Leptos, Lit or vanilla; minimal contract (`api-base` attribute, `rynas:navigate` and `rynas:toast` events) | each wasm plugin brings its own runtime (+100–300 KB); third-party code in the browser |
| C. Plugin wasm in the host Leptos runtime | shared components in theory | **cannot be done well**: two wasm modules share neither allocator, reactive graph nor types; an ABI between them would be needed |
| D. Module Federation / micro-frontends | mature in the JS world | webpack/rspack toolchain foreign to Rust, overkill |

**Choice: A + B.** The plugin manifest declares pages of `type: "form" | "table" | "tabs" | "dashboard-widget"`, or `type: "custom", element: "rynas-synology-bays", module: "ui/synology.js"`. The Leptos shell:
1. reads `/api/v1/ui/manifest`, already merged for all installed plugins;
2. builds menus and routes;
3. renders declarative pages with a `SchemaRenderer`;
4. for custom pages, runs `import()` on the module and mounts the custom element.

**Security:** CSP with `script-src 'self' 'wasm-unsafe-eval'`. Modules only come from files installed by `.deb` packages, so same origin and signed via apt.

---

## 2. Backend

- **HTTP:** axum 0.8, tower-http (compression, ServeDir, timeouts, body size limits), rustls (no OpenSSL; self-signed certificate generated in postinst, optional ACME with `instant-acme`).
- **API:** versioned REST JSON (`/api/v1`). For the schema there is a design choice (see §7):
  - **utoipa 5.5** (OpenAPI 3.1, `utoipa-axum`) for core routes;
  - for plugins, the schema comes from the manifest (JSON Schema) and the core merges it into the published OpenAPI.
- **Real time: SSE** (`axum::response::sse`) for metrics, task progress and events. It is simpler, works through proxies and HTTP/2, and reconnects on its own. **WebSocket** only for the web terminal (pty) and interactive `journalctl -f`.
- **Asynchronous tasks**, PBS style: every long operation (scrub, RAID creation, apply) becomes a task with a UPID, a log file and queryable status. This is the best thing to copy from Proxmox.
- **Authentication:**
  - **PAM** (`pam-client` crate) run **in `rynasd`**, because reading `/etc/shadow` requires root;
  - optional internal "rynas" realm (argon2) for web-only users;
  - session: `HttpOnly; Secure; SameSite=Strict` cookie + CSRF token in a header;
  - API tokens for scripts with reduced privileges (like PBS);
  - **2FA:** TOTP (`totp-rs`), **WebAuthn / passkeys** (`webauthn-rs`), recovery codes;
  - login rate limiting and journal logging (fail2ban compatible).
- **Privilege separation** (PBS model: the non-root `proxmox-backup-proxy` forwards privileged calls to `proxmox-backup-api`, which runs as root):
  - `rynas-web` runs as the `rynas` user with systemd hardening: `ProtectSystem=strict`, `NoNewPrivileges`, `PrivateTmp`, `RestrictAddressFamilies`;
  - `rynasd` (root) listens on `/run/rynas/rynasd.sock` (0660, group `rynas`) and checks `SO_PEERCRED`;
  - **transport: HTTP/1.1 over UDS with hyper**: the same router and types are reused, and debugging is done with `curl --unix-socket`;
  - elegant alternative: **varlink**, which systemd uses more and more (`zlink` crate);
  - D-Bus/zbus is used as a *client* towards systemd and udisks2, not as internal IPC.
- **polkit:** not needed between the daemons. Only needed if `rynasctl` must work for non-root users in the `rynas-admin` group. In that case a polkit action `net.rynas.manage`, checked by `rynasd` via zbus (`org.freedesktop.PolicyKit1.Authority.CheckAuthorization`), is enough.
- **Authorization:** Proxmox-style path ACLs (`/storage/pool1`, `/plugins/synology`) × roles (Admin, Operator, Audit), checked in `rynas-web` and re-checked in `rynasd`.

---

## 3. Backend plugin system

| Option | Third-party plugins | Separate licenses | Security | Independent apt updates | Verdict |
|---|---|---|---|---|---|
| Static crates + feature flags | no | no (all one binary) | maximum, full trust | no | **core only** |
| dylib (abi_stable / stabby) | yes | hard, it gets linked | no isolation, one crash takes down the core | fragile: ABI tied to rustc and dependency versions | **rejected** |
| WASM component model (wasmtime + WIT) | yes, multi-language | yes | excellent capability-based sandbox | yes | interesting, but hardware plugins (sysfs, serial, ioctl, exec of `mdadm`) require re-exposing everything as host capabilities; WASI 0.3 only stabilizes in 2026 |
| **Separate processes** (JSON-RPC over UDS; Terraform/HashiCorp plugin model and Cockpit bridge) | **yes, any language** | **yes: clean boundary between programs** | good: dedicated user, systemd sandbox, isolated crashes | **yes: `rynas-plugin-xxx.deb`** | **chosen** |

**How it works:**
- **Plugin files:**
  - `/usr/lib/rynas/plugins/<id>/rynas-plugin-<id>` (binary);
  - `/usr/share/rynas/plugins/<id>/manifest.json`: id, version, protocol `api_version`, methods with input and output JSON Schema, ACL permissions, UI pages, templates, license and entitlement;
  - `/usr/share/rynas/plugins/<id>/ui/`, `i18n/`, `templates/`.
- **Startup:** **socket-activated** systemd unit `rynas-plugin@<id>.socket`, socket at `/run/rynas/plugins/<id>.sock`. Zero cost when unused (important with 4 GB of RAM). Per-plugin overrides with `User=`, `CapabilityBoundingSet=`, `DeviceAllow=`.
- **Protocol:** JSON-RPC 2.0 with length-prefixed framing. Methods `describe`, `call`, `subscribe` (streaming events and metrics), `config.render` (the plugin returns the files to generate: the core decides whether to write them), `health`.
- **Routing:** `rynas-web` exposes `/api/v1/plugins/<id>/<method>`, validates the JSON against the manifest schema, applies ACLs, then forwards. Root operations go through `rynasd`'s restricted APIs: the plugin is **not** root by default.
- **SDK** `rynas-plugin-sdk` (Rust): `#[rynas::method]` macro, ready-made server, manifest types. **MIT/Apache-2.0** license (or LGPL), so proprietary plugins can use it.
- **Core:** **AGPL-3.0** (like PBS) or GPL-3.0. Business plugins are separate programs communicating over sockets, in packages from the authenticated repo. The entitlement (subscription key) is verified by the plugin itself, against a signed file in `/etc/rynas/subscription`.
- **First-party plugins** (smb, nfs, users, storage): they use **the same** `Plugin` **trait** with an *in-process* transport. With the `builtin-smb` feature they are compiled into the binary, so no IPC, but the API stays the same. The wasm component model can be added later as a third transport, for sandboxed "logic" plugins (automations, notifications).

### `synology` plugin
- **`rynas-plugin-synology`** implements a generic **`rynas-hw`** interface, defined in the core for any hardware: `fans`, `leds`, `bays`, `beeper`, `buttons`, `power_schedule`, `ups`.
  - The core dashboards show fans and bays without knowing it is a Synology.
  - A future `rynas-plugin-ugreen` or `-generic-hwmon` implements the same interface.
- **Data sources:**
  1. direct reading of the `synology_nas` / `synology_ds923` driver sysfs (`/sys/devices/platform/synology-…`, hwmon), which does not require root if udev grants permissions to the `rynas-hw` group;
  2. for microcontroller commands (beep, LEDs, power schedule via `wakealarm` and serial commands like `s`): **do not duplicate `synohwd` logic**. Proposal: `synohwd` exposes a small varlink or JSON socket (`/run/synohwd.sock`), to be added in synodeb. Then **synohwd is rewritten in Rust as the `synohw` crate**, used by both the daemon and the plugin. This removes Python from the NAS and saves RAM and boot time.
- **UPS:** separate **`rynas-plugin-nut`** plugin speaking the `upsd` protocol (TCP 3493). synodeb already configures NUT, so it also works on non-Synology NASes.
- **UI:** declarative pages to configure fan curves, LEDs and power schedule. A `<rynas-synology-bays>` Web Component for the graphical front-panel view with disk status.

---

## 4. Configuration

- **Source of truth: `/var/lib/rynas/config.db`, SQLite** with WAL and `synchronous=NORMAL`.
  - **rusqlite** + `refinery` for migrations: a single writer (`rynasd`) in `spawn_blocking` is more than enough.
  - `sqlx` brings more dependencies and an async runtime that is not needed here.
  - `redb` is a key-value store: no joins or constraints, rejected for configuration.
- **Revisions and "pending changes":**
  - every change from the UI goes into a *staging* revision (`changeset` table);
  - the UI shows the "N pending changes" banner, the diff and the "Apply" / "Discard" buttons (OMV model, but with real transactions).
- **Apply = idempotent task:**
  1. load the revision;
  2. render with **minijinja**, because runtime templates let plugins bring their own (askama, compiled at build time, only for very stable core files);
  3. write to `*.rynas-new` files;
  4. validate: `testparm -s`, `exportfs -ra` dry run, `nft -c -f`, `visudo -c`…;
  5. atomic `rename` with backup in `/var/lib/rynas/backup/<rev>/`;
  6. reload or restart the affected units via **zbus → `org.freedesktop.systemd1`**;
  7. healthcheck;
  8. commit the revision. If a step fails, automatic rollback to the previous revision's files.
- **Generated files and drift:** every generated file has a "managed by rynas" header and its hash is stored in the DB. If someone edits it by hand, the UI flags it and offers "import" or "overwrite".
- **System integrations:**
  - **systemd:** zbus 5 (`Manager`/`Unit` proxies, `JobRemoved` signals);
  - **udisks2:** zbus (hotplug events, mount, format), plus `lsblk --json` for the inventory;
  - **SMART:** `smartctl --json=c` (stable JSON output since smartmontools 7);
  - **mdadm:** no JSON. Use `/proc/mdstat`, `mdadm --detail --export` (KEY=VALUE) and `mdadm --monitor --program` events;
  - **LVM:** `lvs/vgs/pvs --reportformat json`;
  - **btrfs:** `btrfs` CLI + `libbtrfsutil` (subvolumes and snapshots via ioctl, `btrfsutil` crate);
  - **ZFS:** `zfs list -j` / `zpool status -j` (JSON output since OpenZFS 2.3). On the synodeb kernel `zfs-dkms` is needed: the synodeb note says OpenZFS 2.4.4 supports up to 7.2, so the 6.18 longterm series is safer.
- **External commands:** all behind a `rynas-sys::Cmd` wrapper with timeout, process-group kill, logging into the task, and arguments never passed through a shell.

---

## 5. Metrics

- **Internal collector** in `rynas-web` (almost everything is readable without root: `/proc/stat`, `meminfo`, `diskstats`, `net/dev`, hwmon, `/sys/block/*/stat`). SMART temperatures are provided by `rynasd` every 5–10 minutes: do not wake up spun-down disks (`smartctl -n standby`).
- **RRD-like storage** modeled on **`proxmox-rrd`** (a proxmox-rs crate that PBS uses for this):
  - multi-resolution in-memory archives: 1 s × 10 min (live), 1 min × 48 h, 30 min × 30 d, 6 h × 5 years;
  - periodic saving to mmap/binary files in `/var/lib/rynas/rrd/`;
  - fixed size (KB per series), no growth, infrequent writes (friendly to flash and DOM).
- **SSE `/api/v1/metrics/stream`:** sends deltas only for the series the page has subscribed to.
- **Prometheus:** optional `/metrics` endpoint (`prometheus-client` crate) for those who already have Grafana. Do **not** install Prometheus or node_exporter on the NAS: they cost RAM and IO on a J4125 with 4 GB.
- Alarm thresholds (temperature, SMART, RAID degradation, UPS on battery) go into the notification engine (mail with `lettre`, ntfy, webhook).

---

## 6. Debian packaging

- **Cargo workspace** with `cargo-deb` (`[package.metadata.deb]` per crate, `systemd-units`, `maintainer-scripts`, `assets`). Packages:
  - `rynas` (meta), `rynas-core` (web + rynasd + cli), `rynas-ui` (static assets, `Architecture: all`);
  - `rynas-plugin-{smb,nfs,synology,nut,…}`;
  - protocol version dependencies are expressed with `Depends: rynas-core (>= 1.2), rynas-plugin-abi-1`, a virtual package with `Provides`.
- **CPU target:** `-C target-cpu=x86-64-v2`. The J4125 (Goldmont Plus) has no AVX2, so no v3. Release profile with `lto="fat"`, `codegen-units=1`, `panic="abort"`, `strip`. arm64 as second target (cross-compiled with `cargo-zigbuild`).
- **Signed apt repo: aptly** (atomic snapshots and publishing, convenient in CI); reprepro is fine if you want the bare minimum. The synodeb layout can be reused: Pages / GitHub Actions + GPG key in a secret, deb822 `.sources` files with `Signed-By:`.
- **Business repo:** Proxmox enterprise style.
  - `/etc/apt/auth.conf.d/rynas.conf` with `machine repo.rynas.example login <server-id> password <subscription-key>`, written by `rynasctl subscription set`;
  - server side: nginx + a small validation service (auth_request) that checks key and status.
- **Future ISO:** debian-installer with preseed + `rynas` (like synodeb itself), or live-build.
- **Getting into official Debian** would require debcargo and all dependencies packaged: Proxmox does it, at a huge cost. Postpone.

---

## 7. Reference projects: what to copy

- **Proxmox Backup Server** (Rust, AGPL-3.0, repos `proxmox/proxmox-backup` + `proxmox/proxmox-rs`, about 51 crates):
  - two daemons (non-root proxy and root API) → **copy**;
  - `proxmox-schema` + `proxmox-api-macro`: a single `#[api]` on a function generates runtime validation, documentation and CLI → **copy the concept**. It can be used directly: proxmox-rs is AGPL and published on crates.io. Evaluate `proxmox-schema`, `proxmox-router`, `proxmox-rrd`, `proxmox-section-config`, `proxmox-sys`. Con: unstable APIs, designed for Proxmox's internal use. Alternative: redo it on a small scale with utoipa + schemars;
  - task workers with UPID, path ACLs, "section config" `.cfg` files → copy the ideas (rynas uses SQLite instead of files);
  - UI: ExtJS in PBS, **Yew + pwt** in PDM. Confirms that a wasm admin UI is viable in production.
- **Cockpit:**
  - packages = directories with `manifest.json` in `/usr/share/cockpit`, overrides in `/etc/cockpit/<pkg>.override.json` (JSON Merge Patch);
  - `cockpit-bridge` and privileged bridges declared in the manifest (sudo/polkit);
  - to take: the per-plugin manifest, overrides in `/etc`, `.deb` packages per module;
  - to avoid: every plugin ships arbitrary JS without a uniform UI, and the bridge exposes the system too generically (spawn/file channels).
- **45Drives Houston:** Cockpit + modules (`cockpit-file-sharing`, `cockpit-navigator`, `45drives-tools`) in separate `.deb`s. Shows that "one plugin = one package" works commercially.
- **OpenMediaVault:** declarative YAML workbench (`component.d`, `navigation.d`, `dashboard.d`, `route.d`; `formPage`, `datatablePage`, `tabsPage` types; built with `omv-mkworkbench`), plus PHP RPC (`engined`) and file generation with Salt. The **pending changes / apply** model and declarative pages are the basis of point 1A and §4. Avoid the slow PHP + Salt chain.

---

## 8. Workspace crates

```
rynas/
├── Cargo.toml                    # workspace, release profiles, lints
├── crates/
│   ├── rynas-api-types/          # shared serde types + JSON Schema (native + wasm32)     [MIT/Apache]
│   ├── rynas-schema/             # #[api] macro: validation, OpenAPI, CLI (proxmox-schema style)
│   ├── rynas-plugin-sdk/         # manifest, JSON-RPC protocol, plugin server             [MIT/Apache]
│   ├── rynas-plugin-host/        # manifest discovery, socket activation, routing, in-process transport
│   ├── rynas-hw/                 # generic hardware traits (fans/leds/bays/beeper/ups/power)
│   ├── rynas-config/             # SQLite, migrations, changesets/revisions, minijinja render, apply/rollback
│   ├── rynas-sys/                # zbus systemd/udisks2, smartctl, mdadm, lvm, btrfs, zfs, Cmd wrapper
│   ├── rynas-auth/               # PAM (rynasd side), sessions, TOTP, WebAuthn, API tokens, ACL
│   ├── rynas-tasks/              # UPID task workers, logs, cancellation
│   ├── rynas-metrics/            # /proc /sys collectors + RRD ring buffer + Prometheus exporter
│   ├── rynas-notify/             # mail/ntfy/webhook
│   ├── rynas-web/      (bin)     # axum, rynas user: REST, SSE, static UI, proxy to rynasd and plugins
│   ├── rynasd/         (bin)     # root: privileged operations, apply, PAM
│   ├── rynasctl/       (bin)     # CLI (same API schema)
│   ├── rynas-ui/       (wasm)    # Leptos CSR shell: login, router, menu, dashboard
│   └── rynas-ui-kit/   (wasm)    # components, SchemaRenderer (form/table/tabs), uPlot wrapper, i18n
├── plugins/
│   ├── smb/  nfs/  users/  storage/  shares/   # first-party, also as builtins via features
│   ├── synology/                 # rynas-plugin-synology (+ ui/ with <rynas-synology-bays>)
│   ├── nut/                      # UPS
│   └── docker/                   # future
├── synohw/                       # (possible) Rust crate replacing synohwd
└── packaging/  (aptly, systemd units, postinst, auth.conf template)
```

## 9. Processes

```
 Browser (Leptos CSR wasm + plugin Web Components)
     │  HTTPS :443  REST /api/v1 · SSE /stream · WS /term
     ▼
┌───────────────────────────────┐  systemd, User=rynas, ProtectSystem=strict
│ rynas-web (axum/tokio)        │  - sessions, CSRF, ACL, schema validation
│  ├ static UI (br precompress) │  - metrics /proc,/sys → RRD ring buffer
│  ├ metrics collector + RRD    │  - merges plugin UI manifests
│  └ plugin router              │
└──────┬───────────────┬────────┘
       │ HTTP over UDS │ JSON-RPC over UDS (socket-activated)
       │ SO_PEERCRED   ├──────────────────────────────────────┐
       ▼               ▼                                      ▼
┌──────────────┐ ┌───────────────────────────┐   ┌────────────────────────┐
│ rynasd (root)│ │ rynas-plugin-synology     │   │ rynas-plugin-nut / smb │
│ - PAM auth   │ │ User=rynas-hw, DeviceAllow│   │ …(1 service per plugin)│
│ - apply cfg  │ │ - sysfs synology_nas      │   │                        │
│ - root tasks │ │ - synohwd.sock (or synohw)│   └───────────┬────────────┘
│ - SQLite DB  │ └──────────────┬────────────┘               │ upsd :3493
└──┬───┬───┬───┘                │ serial / sysfs             ▼
   │   │   │                    ▼                         NUT upsd
   │   │   └─ exec: smartctl, mdadm, lvm, btrfs, zfs, testparm (with timeout)
   │   └───── D-Bus (zbus): systemd1 · UDisks2 · PolicyKit1
   └───────── writes /etc/samba/smb.conf, /etc/exports… (atomic rename + backup)
```

## 10. Recommended first steps

1. **MVP:** `rynas-web` + `rynasd` + PAM/TOTP login + dashboard with metrics and SSE + read-only `synology` plugin (fans, temperatures, bays).
2. Plugin protocol v1 and `SchemaRenderer`, validated by porting `smb` as a declarative plugin.
3. `rynas-config` with pending/apply and rollback; then storage (md + btrfs).
4. synohwd in Rust and a control socket; apt repo signed with aptly.

**Things I could not verify:**
- Thaw's support for Leptos 0.8;
- the exact Wasmtime and WASI 0.3 versions, which come from a secondary blog;
- the current API of Proxmox's pwt toolkit;
- whether `synohwd` already has a control socket (the synodeb documentation does not mention it).

Context: the synodeb documentation (4.0 package names, synohwd, NUT, multi-platform).

**Sources:**
- Leptos 0.8.0: https://github.com/leptos-rs/leptos/releases/tag/v0.8.0 · https://docs.rs/crate/leptos/0.8.19/source/src/lib.rs
- Dioxus 0.7: https://dioxuslabs.com/blog/release-070 · https://github.com/DioxusLabs/dioxus/releases
- Thaw: https://docs.rs/crate/thaw/latest/source/README.md
- proxmox-rs (overview): https://deepwiki.com/proxmox/proxmox-rs · pbs-api-types: https://lore.proxmox.com/all/20220217171457.72206-3-h.laimer@proxmox.com/t
- Proxmox Datacenter Manager / Yew: https://github.imc.re/proxmox/proxmox-datacenter-manager · https://www.linuxcompatible.org/story/proxmox-datacenter-manager-10-released · https://lore.proxmox.com/pdm-devel/20251211101029.77958-2-s.sterz@proxmox.com/T
- proxmox-yew-comp: https://metadata.cdn.proxmox.com/download/changelogs/devel/dists/bookworm/main/r/rust-proxmox-yew-comp/rust-proxmox-yew-comp_0.3.8.changelog
- Wasm component model: https://input-output-hk.github.io/hermes/architecture/09_architecture_decisions/0006-wasm-component-model · https://www.buildmvpfast.com/blog/webassembly-server-plugins-wasi-component-model-2026 · https://wasmcloud.website.cncfstack.com/community/2025-10-15-community-meeting/
- Extism and the component model: https://github.com/extism/extism/issues/666
- utoipa: https://docs.rs/crate/utoipa/latest · https://github.com/juhaku/utoipa
- OMV plugins: https://docs.openmediavault.org/en/7.x/development/plugins.html · https://www.openmediavault.org/?p=2895
- Cockpit packages: https://cockpit-project.org/guide/latest/packages.html · https://github.com/cockpit-project/cockpit/pull/13482
- Houston: https://forum.45homelab.com/t/no-applications-listed-for-install-in-houston/1296
- PVE on Debian 13: https://pve.proxmox.com/wiki/Install_Proxmox_VE_on_Debian_13_Trixie
