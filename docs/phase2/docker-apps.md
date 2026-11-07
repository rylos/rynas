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
| D1 | `rynas-docker` + rynasd API: engine status, install and adoption, engine config, listing, actions, logs, stacks CRUD on disk, compose tasks, snapshot and rollback, events into notifications, stats; API section below | built (§5.1) |
| A1 | `rynas-apps` format + loader + renderer + CI lint, and the first curated entries in `apps/`: the managers (Arcane, Dockhand, Portainer CE, Dockge), Immich, Jellyfin, Vaultwarden, Nextcloud, Syncthing, Home Assistant, Uptime Kuma, Paperless-ngx; format in § 6 | done (2026-11-07) |
| D2 | UI: Docker page, dashboard widget, notification texts | after D1 |
| A2 | Apps & Plugins page: install wizard (form from `form.json`), installed apps, updates with snapshot and rollback, convert to custom stack; rynasd catalogue endpoints | after D1 + A1 |
| W0 | rynas-web on `:8443` with rustls, the emergency door that never goes through the proxy (still owed from phase 1) | before W1 |
| W1 | Web access (D-PROXY/D-EXPOSE revised on 2026-11-07, `../research/web-access.md`): `rynas-caddy` package, rynasd renderer + `POST /load` + last-good, local names + local CA, app routes, the wizard's Access step, diagnostics | with A2 |
| W2 | Domains, DNS-01 wildcard, DNS records managed by rynas, certificate dashboard | after W1 |
| R | Quality review: GUI/API consistency, speed, memory, no inner horizontal scroll; then a deploy on the DS920+ with Docker installed through the module | after D2 + A2 |

## 5. API

Each wave adds its own section here: routes, request and response shapes, error codes, tasks. It follows the conventions of `../phase1/plan.md` (§ Error codes, configuration pending/apply, tasks with UPIDs).

### 5.1 API — wave D1 (Docker engine, `/api/v1/docker/...`)

Code: `crates/rynas-docker` (engine side, no rynasd types), `crates/rynasd/src/docker.rs`
(routes, tasks, the `docker` key, the event watcher, the stats sampler). Types:
`rynas_api_types::docker` (the names below are those structs; every field is documented
there). rynas-web forwards every route unchanged (`crates/rynas-web/src/proxy.rs`, 30 s to
the answer head).

**Access.** Every route, reads included, needs the administrator role: rynas-web checks it
(`admin_read` covers `/docker/`), and rynasd checks it again from the system groups
(`--admin-group`, 403 `forbidden`). Only the operations below exist. Nothing is passed
through to the engine, and the socket never leaves rynasd. Every change is a task `docker.*`
(user, start, end and log in the task history) and a line of the audit log (log target
`rynas::audit`: `docker <action> <target> by <user>`).

**Without Docker** only `GET /docker/status` answers normally (`installed: false`;
`install_block` says whether the install action is possible). `GET/PUT /docker/settings`
and `POST /docker/plan/engine` work too, since they are preferences only. Every other route
answers 409 `docker_not_installed`, or 503 `docker_unavailable` when the engine is
installed but its daemon does not answer. Nothing else in rynas changes.

#### New error codes (`rynas_api_types::error_codes`, `err.<code>` in en + it)

| Status | Code | When |
|---|---|---|
| 409 | `docker_not_installed` | any route except status, settings and plan, without an engine |
| 503 | `docker_unavailable` | engine installed, daemon down or socket unreachable (the message comes from the client) |
| 409 | `containers_running` | `POST /docker/engine/migrate` while containers run (the message says how many) |
| 409 | `stack_not_managed` | files, compose actions or snapshots of a stack outside the rynas stacks folder (Portainer's or untracked) |

Existing codes in use:
- 400 `invalid_value` (with `field`); 400 `bad_request` (bad stack name or container
  reference); 400 `confirmation_required` (`field: confirm`);
- 404 `not_found`;
- 409 `already_exists` (the stack folder exists); 409 `busy` (an action on that stack, or an
  install or migration, is running, or the task queue is full); 409 `conflict`;
- 501 `not_available` (install refused on this system); 503 `tool_missing` (no compose
  plugin for a validation); 500 `internal`.

#### Routes

Paths are relative to `/api/v1`.

| Method | Path | Body | Answer |
|---|---|---|---|
| GET | `/docker/status` | — | `DockerStatus` |
| GET | `/docker/settings` | — | `DockerSettingsState` {applied, pending?, volumes [`VolumeChoice` {label, path, avail_bytes?}], stacks_dir?} |
| PUT | `/docker/settings` | `DockerSettings` | `DockerSettingsState`. Stages the key `docker` (apply with `/config/apply`). 400 `invalid_value` with `field`; 409 `conflict` with `field: volume` when the volume changes while the engine holds containers or images (use the migration) |
| POST | `/docker/plan/engine` | `DockerSettings` | `EnginePlan` {files [{path, before?, after}] (changed files only), create [folders and subvolumes to make], commands [systemctl lines], restart}. No side effects |
| POST | `/docker/install` | optional `InstallRequest` {volume?} | 202 `TaskStarted` (`docker.install`). 409 `conflict` when an engine exists (it is adopted, never reinstalled); 501 `not_available` with the `InstallBlock` in the message; 409 `busy` |
| POST | `/docker/engine/migrate` | `MigrateRequest` {volume} | 202 (`docker.migrate`). 409 `containers_running`; 409 `conflict` (already there); 400 `invalid_value` with `field: volume` (not mounted) |
| GET | `/docker/containers` | — | `Vec<ContainerSummary>`, sorted by stack then name |
| POST | `/docker/containers/{ref}/start`, `/stop`, `/restart` | — | 202 (`docker.container.start\|stop\|restart`). `{ref}` is the full id, a unique id prefix of 12+ characters, or the name. 404; 400 for an ambiguous prefix |
| GET | `/docker/containers/{ref}/logs?tail=N&timestamps=1` | — | `Vec<LogLine>` {stream stdout\|stderr, line}; `tail` default 200, max 5000 |
| GET | `/docker/containers/{ref}/logs/stream?tail=N&timestamps=1` | — | SSE: `log` (data `LogLine`) for the last N lines (default 100), then every new line; `error` (data {message}); `end` (data `{}`) when the container stops logging. Counted like the session's other streams |
| GET | `/docker/stacks` | — | `Vec<StackSummary>`, sorted by name |
| POST | `/docker/stacks` | `CreateStackRequest` {name, compose, env, files [`StackFile` {path, content, mode?}], volume?, start} | 201 `StackCreated` {name, path, task_id?, validation} with `Location: /api/v1/docker/stacks/<name>`. Validated with `docker compose config` first (400 `invalid_value`, `field: compose`, compose's message). 409 `already_exists`; 400 with `field` `name`, `files`, `env` or `volume` |
| POST | `/docker/stacks/validate` | `ValidateStackRequest` {name?, compose, env} | `StackValidation` (a bad compose file is `ok: false` + `error`, never a 400) |
| GET | `/docker/stacks/{name}` | — | `StackDetail` {summary, containers, snapshots} |
| GET | `/docker/stacks/{name}/files` | — | `StackFiles` {compose, env, compose_file}; 409 `stack_not_managed` |
| PUT | `/docker/stacks/{name}/files` | `StackFiles` {compose, env} | `StackValidation`, after writing. Validated first (400 with `field: compose`). Does not restart the stack: run `up` |
| POST | `/docker/stacks/{name}/up`, `down`, `restart`, `stop`, `start` | — | 202 (`docker.stack.<action>`). `start`, `stop` and `restart` also work for stacks rynas does not manage (per container, through the engine); `up` and `down` only for rynas stacks (409 `stack_not_managed`) |
| POST | `/docker/stacks/{name}/pull`, `update` | optional `UpdateStackRequest` {snapshot?, stop_first?} (defaults from the settings) | 202 (`docker.stack.pull` / `docker.stack.update`) |
| POST | `/docker/stacks/{name}/rollback` | `RollbackRequest` {snapshot} | 202 (`docker.stack.rollback`); 404 for an unknown snapshot |
| POST | `/docker/stacks/{name}/remove` | optional `RemoveStackRequest` {delete_files, confirm?} | 202 (`docker.stack.remove`). `delete_files` needs `confirm` equal to the stack name (400 `confirmation_required`, `field: confirm`) |
| GET | `/docker/stacks/{name}/snapshots` | — | `Vec<StackSnapshot>` {id, created_s, reason manual\|update\|pull\|before-rollback, images [`ServiceImage` {service, image, image_id, digest?}]}, newest first |
| POST | `/docker/stacks/{name}/snapshots` | — | 201 `StackSnapshot` (reason `manual`); 409 `conflict` when the folder is not a btrfs subvolume |
| DELETE | `/docker/stacks/{name}/snapshots/{id}` | — | 204; 404 |
| GET | `/docker/images` | — | `Vec<ImageSummary>` {id, tags, digests, size_bytes, created_s, containers, dangling} |
| GET | `/docker/volumes` | — | `Vec<VolumeSummary>` {name, driver, mountpoint, stack?, size_bytes?, in_use?} |
| GET | `/docker/networks` | — | `Vec<NetworkSummary>` {id, name, driver, scope, subnets, internal, stack?} |
| GET | `/docker/df` | — | `DockerDiskUsage` {images_bytes, images_reclaimable_bytes, containers_bytes, volumes_bytes, build_cache_bytes} |
| GET | `/docker/stats` | — | `DockerStats`: the sampler's last sample, or one sample on demand when nobody listens (CPU reads 0 until the next one) |
| GET | `/docker/updates` | — | `UpdateReport` {checked_at_ms (0 = never), images [`ImageUpdate` {image, local_digest?, remote_digest?, update_available, error?, stacks, containers}]} |
| POST | `/docker/updates/check` | — | 202 (`docker.update-check`): registry digests compared without pulling; the report lands in `GET /docker/updates` |

#### Shapes worth knowing

- **`DockerStatus`**:
  - engine: installed, running, source `docker_ce|docker_io|other|none`, packages
    [{name, version}];
  - versions: version, api_version, min_api_version, compose_version, buildx_version;
  - storage and runtime: storage_driver, data_root, containerd_snapshotter, cgroup_driver,
    live_restore, logging_driver;
  - counts {containers, running, paused, stopped, images}, data_root_usage {total_bytes,
    used_bytes, avail_bytes};
  - managers [`DetectedManager` {kind arcane|dockhand|portainer|dockge|komodo, container,
    container_id, image, running, port?, stack?}];
  - install_block `installed|not_debian|unsupported_release|unsupported_arch` (null: the
    install is possible), task_id (a running install or migration);
  - warnings [`daemon_down|compose_missing|data_root_on_system|data_root_almost_full|not_pinned|engine_too_old`],
    read_at_ms.

  The UI builds a manager's link as `<location.protocol>//<location.hostname>:<port>/` and
  shows "has root privileges and its own login" next to it.
- **`ContainerSummary`**: id, name, image, image_id, state
  `created|running|paused|restarting|removing|exited|dead|stopping|unknown`, status
  (Docker's English text, not translated), health `starting|healthy|unhealthy` or null,
  created_s, ports [{ip?, private_port, public_port?, protocol}], stack?, service?, url?
  (the `rynas.url` or `homepage.href` label), manager?.
- **`StackSummary`**: name, manager `rynas|portainer|untracked`, state
  `running|partial|stopped|down`, working_dir?, config_files, containers, running,
  unhealthy, editable (a rynas stack whose folder exists: files and compose actions),
  snapshots (the folder is a subvolume), images. Rynas folders without containers are
  listed as `down`.
- **`StackValidation`**: ok, error?, services, images, ports (`tcp/8080`), port_conflicts
  (published ports that containers of other stacks use), binds [{service, source, target,
  exists, outside_volume}].

#### Configuration key `docker` (`DockerSettings`, staged and applied like every key)

```json
{"volume": "vol1", "address_pool": {"base": "10.200.0.0/16", "size": 24},
 "snapshot_before_update": true, "stop_for_snapshot": false, "keep_snapshots": 5,
 "data_root_full_pct": 90, "crash_restarts": 5, "crash_window_min": 10,
 "preferred_manager": null}
```

`volume` is a rynas volume label; null means rynas leaves the engine configuration alone.
With a volume, the apply:
- merges `/etc/docker/daemon.json`:
  - `data-root` is always `/<vol>/docker/engine`;
  - `log-driver: local`, `live-restore: true` and `default-address-pools` (=
    `address_pool`) are added only when the file has no value of its own;
  - other keys stay untouched;
  - a file that is not a JSON object fails the apply instead of being replaced;
- sets the top-level `root = "/<vol>/docker/containerd"` of `/etc/containerd/config.toml`,
  keeping the rest;
- writes the drop-ins `docker.service.d/rynas-mounts.conf` (`RequiresMountsFor=` the engine
  and containerd folders) and `containerd.service.d/rynas-mounts.conf`;
- creates the subvolumes `/<vol>/docker`, `engine`, `containerd` and `stacks`;
- runs `systemctl daemon-reload` and, when the volume changed and the engine is active,
  restarts containerd and Docker. That restart is refused (the apply rolls back) while
  containers run.

Drift detection covers the four files. A key never applied imports `volume` from a
`daemon.json` whose data-root is `/<label>/docker/engine`.

Ranges (400 `invalid_value` with the field):
- `keep_snapshots` 1-50, `data_root_full_pct` 50-99, `crash_restarts` 2-100,
  `crash_window_min` 1-120;
- `address_pool.base` an IPv4 network /8-/28 without host bits, `size` longer than the
  base and at most 30.

#### Tasks

| Type | What it does |
|---|---|
| `docker.install` | apt update, ca-certificates and curl, the key to `/etc/apt/keyrings/docker.asc`, `/etc/apt/sources.list.d/docker.sources` (deb822, signed-by), apt update, the pin `/etc/apt/preferences.d/rynas-docker` (`Pin: version 5:<major>.*`, priority 1001, major = apt's candidate). With `volume`, the apply of `docker` before the packages, so the engine starts on the volume. Then `apt-get install -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin` and the system user and group `docker-apps` |
| `docker.migrate` | refuses while containers run; stops docker.socket, docker and containerd; creates the subvolumes; `cp -a --reflink=auto` of the old data-root and containerd root; applies `docker` with the new volume; starts the engine. The old folders are kept (the log says where) |
| `docker.container.start\|stop\|restart` | the engine call (stop and restart give 30 s of grace) |
| `docker.stack.up` | `docker compose config`; the missing bind folders below the volume are created, owned by `docker-apps` (binds outside the volume are logged as warnings); then `docker compose -p <stack> --project-directory <dir> -f <dir>/<file> up --detach --wait=false` (30 min) |
| `docker.stack.down\|restart\|stop\|start` | the compose command (`down` never removes volumes; 10 min) |
| `docker.stack.pull` | snapshot (setting or body), then `pull` |
| `docker.stack.update` | snapshot (the services' images are recorded with their registry digests; `stop_first` stops the stack during the snapshot), `pull`, `up`. On failure, an automatic rollback to that snapshot |
| `docker.stack.rollback` | `down`; a `before-rollback` snapshot of the current folder; the folder swapped with a writable copy of the snapshot; each service's image tagged back (`docker image tag <image_id> <ref>`, or `docker pull <repo>@<digest>` first when the image is gone); `up` |
| `docker.stack.remove` | `down`; with `delete_files`, the folder (subvolume) and its snapshots are deleted |
| `docker.update-check` | the registry digest of every tagged image in use (distribution endpoint, no pull) against the local repo digest. New updates are notified once per registry digest; the report is saved in `<state>/docker/updates.json` |

#### Stacks on disk (for the app catalogue, A1/A2)

- A stack is `/<vol>/docker/stacks/<name>/`:
  - `name`: 1-63 characters of `a-z 0-9 - _`, starting with a letter or a digit;
  - a btrfs subvolume where the volume is btrfs, a plain folder elsewhere (no snapshots
    then);
  - mode 0750, owned by `docker-apps`; `compose.yaml` 0640, `.env` 0600; extra files at
    relative paths (no `..`, no absolute paths, mode 0644 by default).
- Snapshots live in `/<vol>/docker/.snapshots/<name>/<YYYYMMDDTHHMMSS>` (plus a `.json`),
  outside the folder that managers scan.
- Making a stack from rendered files: `POST /docker/stacks` with `files` and `start: true`,
  or in Rust `rynas_docker::stacks::StackFs::create_stack(layout, name, compose, env, files,
  log)`. It refuses an existing folder and starts nothing. Update with snapshot and
  rollback: `POST /docker/stacks/{name}/update` and `/rollback`.
- Classification:
  - `rynas` when `com.docker.compose.project.working_dir` is
    `/<vol>/docker/stacks/<project>` (the same name);
  - `portainer` when the working dir or a config file is in Portainer's compose data
    (`/data/compose/<id>`);
  - otherwise `untracked`.

#### Live events and notifications

`/api/v1/events` adds three events. They ride on rynasd's task stream (`?docker=1`).
- `docker.stats` (data `DockerStats`): running, total, cpu_pct (the sum, in % of one CPU),
  cpus, mem_bytes, mem_total_bytes, top 5 [{id, name, stack?, cpu_pct, mem_bytes}], at_ms.
  Sent every 5 s while a stream listens; the latest one on connect.
- `docker.changed` (data `DockerChanged` {generation}): re-read the lists the page shows.
  At most one per second.
- `resync {"stream":"docker"}`.

The sampler reads cgroup v2 files (`cpu.stat`, `memory.current` minus `inactive_file`) and
makes one list call per 30 s or per change. It sleeps when nobody listens.

Notification codes (source `docker`; texts `nev.docker.*` in en + it):

| Code | State, category, severity | Fields |
|---|---|---|
| `docker.unhealthy` / `docker.recovered` | firing / resolved (key `docker:health:<container>`), app, warning | `container`, `stack` |
| `docker.oom` | oneshot (at most one per container per crash window), app, warning | `container`, `stack` |
| `docker.crash_loop` / `docker.crash_loop_over` | firing / resolved (key `docker:loop:<container>`), app, error | `container`, `stack`, `restarts`, `minutes` |
| `docker.disk_full` / `docker.disk_ok` | firing / resolved (key `docker:disk`), storage, warning | `path`, `used_pct`, `limit_pct` |
| `docker.update_available` | oneshot (key `docker:update:<image>`), update, info | `image`, `stacks` |

- **Crash loop.** It counts restarts made by the engine: a `die` followed by a `start`
  without a `stop` or `kill` in between. `crash_restarts` of them within
  `crash_window_min` open the loop; a whole window without restarts closes it.
- **Watcher.** It reconnects with backoff (1 s up to 60 s) and reports the containers that
  are unhealthy at each (re)connection. It checks the data-root fill level every minute,
  with 2 points of hysteresis.

#### Development

`rynasd --dry-system` reads the engine through `--docker-socket` (default
`/var/run/docker.sock`; empty means never contacted) and only logs changes: compose, engine
calls, apt, systemctl, copies. Folders go below the dry root (create `<dry-root>/<vol>` to
try stacks).

Not yet: scheduled update checks, image prune, a per-stack backup export, the Docker page
and the widget (D2).

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
