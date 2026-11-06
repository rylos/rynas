# Phase 2: Docker and apps

Started on 2026-11-07, at Marco's request: "Ora rynas deve lavorare su docker e quindi app aggiuntive come da spec".

The specs this builds on:
- D-DOCKER, the thin `docker` module (`../research/docker.md`);
- D-APPCAT, the "Apps & Plugins" page with the curated `rynas-apps` catalogue (`../research/reverse-proxy-app-store.md`, topic 2);
- D-UPDATES channel C, the digest check and one-click update with snapshot and rollback;
- D-PHOTOS / D-NVR, Immich and Frigate as curated apps;
- D-PROXY, exposing apps through the web access module, which belongs to a later wave.

D-APPCAT, adopted after D-DOCKER, moves the catalogue into **phase 2**. That supersedes the line "no app catalog before phase 3" in D-DOCKER.

## 1. Research refresh (2026-11-07)

- **Docker Engine.**
  - The 29.x line is current. pc-work runs 29.9.3, and the third-party trackers I found stop at 29.6.1 in June, so they lag behind.
  - The containerd image store is the default for new installs, and the minimum API is 1.44.
  - The native nftables firewall backend is still experimental. In that mode Docker creates no `DOCKER-USER` chain, so rynas keeps iptables-nft plus `DOCKER-USER`, as in the research.
  - Sources: https://linuxiac.com/docker-engine-29-containerd-becomes-default-experimental-nftables-support/ · https://blog.canadianwebhosting.com/docker-v29-migration-guide-self-hosters/
- **bollard.** Version 0.21.1 came out on 2026-08-16 (https://crates.io/api/v1/crates/bollard).
- **Managers.**
  - Dockhand: v1.0.37, July 2026, BSL 1.1.
  - Arcane: v2.4.0, July 2026, BSD-3, with OIDC and GitOps.
  - Dockge: last release v1.5.0, March 2025.
  - Portainer must be at least 2.33.5 LTS or 2.36 STS to work with Docker 29.
  - The one-click managers are offered in this order: **Arcane, Dockhand, Portainer CE, Dockge**.
  - Sources: https://www.bitdoze.com/arcane-vs-dockhand/ · https://www.bitdoze.com/portainer-alternatives/
- **Test target.** The DS920+ has no Docker today: `/volume1` is 3.7 TB on md + btrfs, with 4 GB of RAM. The catalogue's RAM gates matter there.

## 2. Where the code goes

- **`crates/rynas-docker`.** The engine side, built into rynasd behind a module boundary so it can later move into a plugin process (D-PLUG):
  - detection and adoption, plus installing `docker-ce` from Docker's deb822 repository, pinned to its major version;
  - writing `daemon.json`, the containerd root, and the `RequiresMountsFor=` drop-in;
  - a bollard client for containers, events and stats;
  - `docker compose` runs with a timeout;
  - the stack layout on disk.
- **`crates/rynas-apps`.** The catalogue:
  - the format: `app.toml` + `compose.yaml` + `form.json` + metadata;
  - loading and validating it;
  - rendering it, with `${VAR}` interpolation only, into an ordinary stack folder;
  - migrations, and the update and rollback plan.
- **`apps/`** in this repository. The curated catalogue entries, with CI lint and render tests. The decision names a separate `rynas-apps` git repository; the folder moves there when the catalogue is published. The apt package `rynas-apps-catalog` installs it under `/usr/share/rynas/apps/`.
- **UI.**
  - A **Docker** page, shown only when Docker is present or the module is installed, with tabs Overview, Containers, Stacks and Images & volumes.
  - The **Apps & Plugins** page, with sections Featured, Apps, Plugins, Installed and Updates.
  - A dashboard widget.
- **What is not built.** Nothing in navigation, the dashboard or the storage tabs when Docker is not installed. The Apps page then offers "Install Docker" as the first step.

## 3. Rules (from the research, binding)

1. **Disk is the source of truth.** Each stack is `/<vol>/docker/stacks/<stack>/compose.yaml` + `.env`, one btrfs subvolume per stack. `config.db` holds preferences only: the volume, snapshot options, and the preferred manager.
2. **Show every container and stack, whoever made it.** Stacks are grouped by the `com.docker.compose.project` / `working_dir` labels. Each one is marked as managed by rynas (in the folder), by Portainer, or untracked.
3. **Compose commands** are `docker compose -p <stack> --project-directory <dir>`, run as tasks with a timeout. `down` never takes `-v`.
4. **Before pull or update**, take a read-only snapshot of the stack subvolume, with an option to stop the stack first for databases. Rollback restores that snapshot, puts back the previous image digests, and runs `up -d`.
5. **The engine.**
   - Adopt an existing Docker; never reinstall it.
   - `daemon.json` is merged, never overwritten: `data-root` on the volume, `log-driver: local`, `live-restore: true`, `default-address-pools` 10.200.0.0/16 size 24 when no pool is already set.
   - The containerd root goes on the volume.
   - `RequiresMountsFor=` on `docker.service` and `containerd.service`.
   - Moving the data-root is a task that refuses to run while containers are running.
6. **Security.**
   - Only whitelisted operations, admin role only, recorded in the audit log.
   - Never a generic Docker API proxy, and never the socket handed to the browser.
   - A manager that receives the socket comes with the warning "has root privileges and its own login".
7. **Folders.** Bind folders are created before `up`, owned by the `docker-apps` user and group (PUID/PGID), never root. A bind outside `/<vol>/` gets a warning.
8. **Catalogue.**
   - Images are pinned by tag + digest. The compose file has no templating logic.
   - Secrets are generated automatically.
   - A port conflict is checked before install.
   - A RAM gate is applied to the installed RAM.
   - Antifeatures (non-free, high resources, needs GPU, telemetry, unmaintained) are shown.
   - **"Convert to custom stack"** is always available, and an installed app is an ordinary stack.
   - Third-party sources are off by default.
9. **Notifications** go through D-NOTIFY, only on a state change: unhealthy, OOM, crash loop, data-root disk full, image update available. The update check compares digests without pulling.
10. **Generic and module-free.** No Synology code, no product comparisons in the UI, and every text goes through i18n (it, en).

## 4. Waves

| Wave | Content | State |
|---|---|---|
| D1 | `rynas-docker` + rynasd API: engine status, install and adoption, engine config, listing, actions, logs, stacks CRUD on disk, compose tasks, snapshot and rollback, events into notifications, stats; API section below | in progress |
| A1 | `rynas-apps` format + loader + renderer + CI lint, and the first curated entries in `apps/`: the managers (Arcane, Dockhand, Portainer CE, Dockge), Immich, Jellyfin, Vaultwarden, Nextcloud, Syncthing, Home Assistant, Uptime Kuma, Paperless-ngx; format in § 6 | done (2026-11-07) |
| D2 | UI: Docker page, dashboard widget, notification texts | after D1 |
| A2 | Apps & Plugins page: install wizard (form from `form.json`), installed apps, updates with snapshot and rollback, convert to custom stack; rynasd catalogue endpoints | after D1 + A1 |
| R | Quality review: GUI/API consistency, speed, memory, no inner horizontal scroll; then a deploy on the DS920+ with Docker installed through the module | after D2 + A2 |

## 5. API

Each wave adds its own section here: routes, request and response shapes, error codes, tasks. It follows the conventions of `../phase1/plan.md` (§ Error codes, configuration pending/apply, tasks with UPIDs).

## 6. App catalog format (wave A1)

Normative. Code: `crates/rynas-apps` (pure data in, files out; no Docker access). Entries:
`apps/`. Research and sources: `../research/app-catalog-format.md`.

### 6.1 Entry layout

```
apps/<id>/
  app.toml              metadata (required)
  compose.yaml          ordinary compose file, ${VAR} references only (required)
  form.json             install questions (optional; none = no questions)
  description.md        long English description (required)
  metadata/icon.svg     card icon, SVG < 64 KiB, no scripts or external links (required)
  metadata/screenshots/ *.webp | *.png (optional)
  overlays/<name>.yaml  compose merge patches listed in app.toml (optional)
  migrations/<ver>.toml value migrations (optional)
  tests/values.toml     answers CI renders with (required)
```

Any other file is an error. `<id>` is `[a-z0-9][a-z0-9-]*`, at most 40 characters, equal to
`app.toml` `id`, and never changes.

### 6.2 `app.toml`

| Key | Meaning |
|---|---|
| `schema` | `1` |
| `id`, `name` | identifier (= folder), display name |
| `version` | package version `MAJOR.MINOR.PATCH`, bumped on every change of the entry; drives migrations and updates |
| `app_version` | upstream version; the main service's image tag must equal it or start with `<app_version>-` |
| `description` | one line (≤ 140 characters) for the card |
| `homepage`, `source`, `changelog_url` | `https://` URLs (`source` optional) |
| `license` | SPDX expression of upstream |
| `maintainers`, `categories`, `arch` | non-empty; categories from a fixed list (`management`, `media`, `photos`, `files`, `documents`, `productivity`, `security`, `home-automation`, `monitoring`, `network`, `sync`, `development`, `backup`, `ai`); arch `amd64` / `arm64` |
| `min_rynas` | oldest rynas that may install it |
| `antifeatures` | `non-free`, `high-resources` (exactly when `ram_recommended_mb` > 2048), `needs-gpu` (required with `hardware.gpu = "required"`), `telemetry`, `unmaintained`, `requires-https`, `root-access` (required for managers, privileged services and socket mounts) |
| `main_service` | the service that is "the app" |
| `host_network` | all services use `network_mode: host`; ports are then fixed and only informative |
| `manager` | Docker manager: the main service mounts the Docker socket and `${RYNAS_STACKS_DIR}:${RYNAS_STACKS_DIR}` (same path); `web.public = "never"` |
| `[resources]` | `ram_min_mb` (install refused below), `ram_recommended_mb` (warning below) |
| `[[ports]]` | `name`, `env` (host-port variable), `service`, `container`, `default`, `protocol` (`tcp`/`udp`), `purpose`, `relocatable` (default true; false for discovery and protocol ports and with `host_network`), `proxied` (HTTP port fronted by the web access proxy) |
| `[web]` | see 6.3 |
| `[[folders]]` | app-owned bind folders: `env`, `path` (relative; default location `./<path>` in the stack folder, so the stack snapshot covers it), `intent` (`config`, `data`, `db`, `media`, `cache`), `ssd` (`no`, `ok`, `preferred`), `owner` (`uid:gid` when the image needs it, default PUID:PGID), `description` |
| `[[shares]]` | user shared folders: `env`, `label`, `description`, `access` (`ro`/`rw`, must match `:ro` in compose), `required`, `fallback` (stack folder used when an optional share is not chosen) |
| `[[secrets]]` | `env`, `length` (16-256), `encoding` (`alnum` characters, or `hex` / `base64` of `length` random bytes); generated once, kept on update |
| `[hardware]` | `gpu` (`none`, `optional`, `required`), `gpu_kinds` (`intel-qsv`, `vaapi`, `nvidia`), `cpu_features` (`x86-64-v2`, `avx`, `avx2`) |
| `[security]` | `privileged`, `docker_socket` (services allowed), `cap_add` (capabilities allowed) |
| `[[overlays]]` | `file` (`overlays/<name>.yaml`), `when` (a boolean form field or `RYNAS_CPU_AVX2`, optionally `!`-negated), `description` |
| `[[named_volumes]]` | `name`, `description`; only for disposable content (caches, models, queues): named volumes live in the data-root, outside snapshots and backups |
| `[[notes]]` | `level` (`info`/`warning`), `text`: shown in the wizard before confirming |

All `env` names are `[A-Z][A-Z0-9_]*`, unique across ports, folders, shares, secrets and form
properties, and never one of the provided variables (6.5).

### 6.3 `[web]`: how the proxy fronts the app

Consumed by the web access module (D-PROXY, D-EXPOSE as revised on 2026-11-07).

| Key | Meaning |
|---|---|
| `port` | name of a `tcp` port; must be `proxied` unless `host_network` |
| `path` | entry path, default `/` |
| `scheme` | `http` / `https` spoken by the container |
| `auth` | `app` (own login), `none` (the proxy must add rynas SSO; never with `public = "ok"`), `proxy` (trusts the proxy's identity headers) |
| `websocket` | needs WebSocket upgrades |
| `subpath` | works under a path prefix; otherwise it needs its own host name |
| `body` | largest request body, `<digits>[K\|M\|G]` (Immich `50G`, Nextcloud `16G`) |
| `timeout` | upstream read timeout, `<digits>s\|m\|h` |
| `buffering` | proxy buffering, default `true`; `false` for uploads and streaming |
| `bypass` | path prefixes that skip rynas SSO (mobile apps, DAV, API tokens) |
| `public` | `ok`, `warn` (allowed with a warning), `never` (managers) |
| `trusted_proxies_env` | the app's variable listing trusted proxies; the main service must set it to `${RYNAS_TRUSTED_PROXIES}` |

### 6.4 `compose.yaml`

Standard compose, used as is. The lint (`cargo test -p rynas-apps`) enforces:
- top-level keys only `services`, `volumes`, `networks` (rynas adds `name` and `x-rynas`);
- every service: a literal `image` with a tag that is not moving (`latest`, `stable`, `release`,
  `main`, `master`, `nightly`, `edge`) **and** an `@sha256:` digest (multi-arch index); a
  `restart` policy; no `build`, `container_name`, `extends`, `pid`, `ipc`, `userns_mode`,
  `cgroup_parent`, `volumes_from`; `env_file` only `.env`;
- variables only as `${VAR}` (no defaults, no bare `$VAR`), each provided by rynas, `app.toml`
  or `form.json`; every declared variable is used;
- ports exactly as declared: `"${RYNAS_BIND}:${ENV}:<container>"` for proxied ports,
  `"${ENV}:<container>[/udp]"` for the others (protocol ports reachable on the LAN), none with
  `host_network`;
- volume sources only declared folders, shares, named volumes, `${RYNAS_STACKS_DIR}`
  (managers), the Docker socket (declared services), `/etc/localtime` and `/run/dbus` read-only;
- `privileged`, `cap_add`, `devices` and `network_mode: host` only as declared in `app.toml`.

The same lint runs on the base file and on the base with each overlay merged in.

### 6.5 Provided variables

| Variable | Value |
|---|---|
| `RYNAS_APPDATA` | absolute stack folder, `/<vol>/docker/stacks/<stack>` |
| `RYNAS_STACKS_DIR` | `/<vol>/docker/stacks` |
| `RYNAS_HOST` | host name users reach the NAS on |
| `PUID`, `PGID` | the `docker-apps` user and group |
| `TZ` | system time zone |
| `DOCKER_GID` | group of `/var/run/docker.sock` |
| `RENDER_GID` | group of `/dev/dri/renderD*` |
| `RYNAS_CPU_AVX2` | `true` / `false` |
| `RYNAS_BIND` | host address of proxied ports: `127.0.0.1` by default (only the proxy reaches them), `0.0.0.0` when the user publishes the app on the LAN directly |
| `RYNAS_TRUSTED_PROXIES` | addresses the apps trust for `X-Forwarded-*` |

### 6.6 `form.json`

A JSON Schema subset, rendered by one generic schema-driven form:
- top level: `type: "object"`, `properties`, `required`, `x-groups`
  (`[{ "title", "fields": [...] }]`, every property exactly once: order and sections);
- property: `type` (`string`, `integer`, `boolean`), `title`, `description`, `default`, `enum`,
  `minimum`, `maximum`, `minLength`, `maxLength`, `pattern` (anchored), `format` (`hostname`,
  `uri` = http(s) URL, `email`), and `x-ui`: `widget` (`text`, `password`, `textarea`, `select`,
  `switch`), `placeholder`, `advanced` (folded), `enum_labels`, `visible_if`
  (`{ "FIELD": value }`; a hidden field is not required);
- unknown keywords are errors. Property names are `.env` names; answers are written as strings
  (`true`/`false` for booleans; an optional unanswered field is empty).

### 6.7 Overlays

An overlay is a compose fragment applied as an RFC 7396 merge patch (mappings merge, `null`
deletes, anything else replaces) when its `when` condition holds. Used for optional hardware
(`/dev/dri` + `${RENDER_GID}`) and for dropping a service (Immich without machine learning).
The rendered `compose.yaml` contains the merged result; nothing conditional is left in it.

### 6.8 Rendering

`render(app, catalog, provided, request)` → `RenderedStack`:
- `request`: stack name (compose project rules; default the app id), form answers, chosen
  shares, folder placements (absolute paths, e.g. a database on the SSD volume), port
  overrides (relocatable ports only), secrets already known (updates);
- files, ready for "create stack from files": `compose.yaml` (0644) with a header comment,
  `name: <stack>`, the merged services, volumes and networks with `${VAR}` references kept, and
  ```yaml
  x-rynas: { app: <id>, version: <package version>, app_version: <upstream>, catalog: <source> }
  ```
  and `.env` (0600) with every value (provided, ports, folders, shares, secrets, answers),
  single-quoted when not plain; values with a newline or a single quote are refused;
- `folders`: host folders to create before `up` (owner PUID:PGID or the declared `owner`) and
  shares to check (never created or chowned);
- `images`, `env`, the overlays applied.

The stack is ordinary: any manager may edit it. **Convert to custom stack** removes `x-rynas`
and nothing else.

### 6.9 Checks, migrations, updates

- **Ports**: `port_conflicts(app, chosen, used)` lists collisions with used ports and inside the
  app, with a free suggestion for relocatable ports. Default host ports of bridged apps are
  unique across the catalogue.
- **RAM**: `ram_gate(app, MemTotal)` → `ok`, `below_recommended` (warn), `below_minimum`
  (refuse), with a 12.5 % allowance for memory reserved by firmware and the iGPU.
- **Migrations**: `migrations/<version>.toml` with `to = "<version>"`, `description` and
  `[[ops]]` (`rename {from, to}`, `set {key, value}`, `set_default {key, value}`,
  `delete {key}`, `map {key, values}`), applied in order to the stored `.env` values for every
  `to` above the installed version, up to the catalogue version.
- **Update plan**: `update_plan(app, installed compose, installed .env)` compares the installed
  `x-rynas` version and images (after migrations and overlay selection) with the catalogue:
  versions, `up_to_date`, `downgrade`, migrations, image changes (`added`, `removed`, `tag`,
  `digest`), required answers still missing. `prepare_update` re-renders with the migrated
  values, keeping secrets, ports, folders and answers; the engine snapshots the stack, writes
  the files, pulls and runs `up -d`, and rollback restores the snapshot.

### 6.10 CI

`cargo test -p rynas-apps` loads every entry, validates it (6.2-6.7), renders it with
`tests/values.toml` and with defaults, and checks the rendered stack: it parses, has `name` and
`x-rynas`, every `${VAR}` is in `.env`, images are pinned, secrets are not in the compose file,
proxied ports bind to `${RYNAS_BIND}`. `packaging/apps-render-test.sh` also runs
`docker compose config -q` on every render (CI or a throw-away machine).

### 6.11 First entries

Arcane, Dockhand, Portainer CE, Dockge (managers); Immich, Jellyfin, Vaultwarden, Nextcloud,
Syncthing, Home Assistant, Uptime Kuma, Paperless-ngx. Images pinned by tag and verified digest
(2026-11-07); per-entry choices in `../research/app-catalog-format.md` § 2.
