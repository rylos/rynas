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
   - `daemon.json` is merged, never overwritten: `data-root` on the volume, `log-driver: local`, `live-restore: true`, `default-address-pools` 10.200.0.0/16 size 24 when no pool is already set (a pool rynas wrote itself, the default or the last applied one, follows the setting; any other stays); a legacy `graph` key is dropped.
   - The containerd root goes on the volume.
   - `RequiresMountsFor=` on `docker.service` and `containerd.service`.
   - Moving the data-root is a task that refuses to run while containers are running.
6. **Security.**
   - Only whitelisted operations, admin role only, recorded in the audit log.
   - Never a generic Docker API proxy, and never the socket handed to the browser.
   - A manager that receives the socket comes with the warning "has root privileges and its own login".
7. **Folders.** Bind folders are created before `up`, owned by the `docker-apps` user and group (PUID/PGID), never root. A bind outside `/<vol>/` gets a warning. The stack folder and its two files belong to **root** (folder `0750 root:docker-apps`, `compose.yaml` `0640 root:docker-apps`, `.env` `0600 root:root`): compose runs as root and reads them, containers never need to, and no container uid can plant a symlink where rynasd writes as root. Every root write below a volume is descriptor-based: paths are walked with `O_NOFOLLOW` (a symlink anywhere refuses the operation), new folders and files get owner and mode through `fchown`/`fchmod` on the open descriptor, and files are written to a fresh `O_EXCL|O_NOFOLLOW` temporary name, then renamed (`rynas_docker::safefs`). Reads of a stack's files never follow a symlink either.
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
| A2 | Apps & Plugins page: install wizard (form from `form.json`), installed apps, updates with snapshot and rollback, convert to custom stack; rynasd catalogue endpoints | backend built (§5.2); UI in progress |
| W0 | rynas-web on `:8443` with rustls, the emergency door that never goes through the proxy (still owed from phase 1) | before W1 |
| W1 | Web access (D-PROXY/D-EXPOSE revised on 2026-11-07, `../research/web-access.md`): `rynas-caddy` package, rynasd renderer + `POST /load` + last-good, local names + local CA, app routes, the wizard's Access step, diagnostics | backend and package built (§5.3; Caddy 2.11.9); UI (Routes page, Access step, Trust this NAS) in progress |
| W2 | Domains, DNS-01 wildcard, DNS records managed by rynas, certificate dashboard | backend built (§5.4; rynas-caddy with 11 DNS modules pinned and validated, record clients for Cloudflare, deSEC, Hetzner, Porkbun, OVH, DuckDNS); UI next |
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
(`--admin-group`, 403 `forbidden`) with one route layer over the whole `/v1/docker`
router. Only the operations below exist. Nothing is passed through to the engine, and the
socket never leaves rynasd. Every change is a task `docker.*`
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
| POST | `/docker/install` | optional `InstallRequest` {volume?} | 202 `TaskStarted` (`docker.install`). Also accepted after a failed install: half-installed packages (`install_incomplete`) are repaired first, and an install rynas started whose last steps never ran is finished. 409 `conflict` when a complete engine exists (it is adopted, never reinstalled); 501 `not_available` with the `InstallBlock` in the message; 409 `busy` |
| POST | `/docker/engine/migrate` | `MigrateRequest` {volume} | 202 (`docker.migrate`). 409 `containers_running`; 409 `conflict` (already there); 400 `invalid_value` with `field: volume` (not mounted) |
| GET | `/docker/containers` | — | `Vec<ContainerSummary>`, sorted by stack then name |
| POST | `/docker/containers/{ref}/start`, `/stop`, `/restart` | — | 202 (`docker.container.start\|stop\|restart`). `{ref}` is the full id, a unique id prefix of 12+ characters, or the name. 404; 400 for an ambiguous prefix |
| GET | `/docker/containers/{ref}/logs?tail=N&timestamps=1` | — | `Vec<LogLine>` {stream stdout\|stderr, line}; `tail` default 200, max 5000 |
| GET | `/docker/containers/{ref}/logs/stream?tail=N&timestamps=1` | — | SSE: `log` (data `LogLine`) for the last N lines (default 100), then every new line; `error` (data {message}); `end` (data `{}`) when the container stops logging. Counted like the session's other streams |
| GET | `/docker/stacks` | — | `Vec<StackSummary>`, sorted by name |
| POST | `/docker/stacks` | `CreateStackRequest` {name, compose, env, files [`StackFile` {path, content, mode?}], volume?, start} | 201 `StackCreated` {name, path, task_id?, validation} with `Location: /api/v1/docker/stacks/<name>`. Validated with `docker compose config` first (400 `invalid_value`, `field: compose`, compose's message). 409 `already_exists`; 400 with `field` `name`, `files`, `env` or `volume` |
| POST | `/docker/stacks/validate` | `ValidateStackRequest` {name?, compose, env} | `StackValidation` (a bad compose file is `ok: false` + `error`, never a 400). Compose runs as root, so the run is narrow: 512 KiB per file (400 `invalid_value`), a top-level `include:` is refused (`ok: false`), services' `env_file:` are not read (`--no-env-resolution`, compose ≥ 2.24; older compose runs without it), 30 s timeout, scratch files `0600` in rynasd's state, nothing written in the project folder |
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
| GET | `/docker/volumes` | — | `Vec<VolumeSummary>` {name, driver, mountpoint, stack?, size_bytes?, in_use?}. Sizes come from the cached `docker system df` (below) |
| GET | `/docker/networks` | — | `Vec<NetworkSummary>` {id, name, driver, scope, subnets, internal, stack?} |
| GET | `/docker/df` | — | `DockerDiskUsage` {images_bytes, images_reclaimable_bytes, containers_bytes, volumes_bytes, build_cache_bytes}. `docker system df` walks every volume (seconds, wakes sleeping disks): rynasd runs it at most once per 60 s, shared by concurrent requests, and keeps only the totals and volume sizes |
| GET | `/docker/stats` | — | `DockerStats`: the sampler's last sample, or one sample on demand when nobody listens (CPU reads 0 until the next one) |
| GET | `/docker/updates` | — | `UpdateReport` {checked_at_ms (0 = never), images [`ImageUpdate` {image, local_digest?, remote_digest?, update_available, error?, stacks, containers}]}. Cheap: the report of the last check, held in memory (loaded from `<state>/docker/updates.json` at start); no registry or engine call. Only `POST /docker/updates/check` refreshes it |
| POST | `/docker/updates/check` | — | 202 (`docker.update-check`): registry digests compared without pulling; the report lands in `GET /docker/updates` |

#### Shapes worth knowing

- **`DockerStatus`**:
  - engine: installed, running, source `docker_ce|docker_io|other|none`, packages
    [{name, version}] (dpkg status "install ok installed" only);
  - install_incomplete [`IncompletePackage` {name, version, status}] (additive, default
    `[]`): engine packages dpkg left half-done, status from `${db:Status-Abbrev}`
    (`iU` unpacked, `iF` half-configured, `iHR` half-installed with reinstall required,
    `iW`/`it` triggers). Non-empty: `installed` is false (unpacked binaries are no
    engine), `install_block` is null and Install repairs;
  - versions: version, api_version, min_api_version, compose_version, buildx_version;
  - storage and runtime: storage_driver, data_root, containerd_snapshotter, cgroup_driver,
    live_restore, logging_driver;
  - counts {containers, running, paused, stopped, images}, data_root_usage {total_bytes,
    used_bytes, avail_bytes};
  - managers [`DetectedManager` {kind arcane|dockhand|portainer|dockge|komodo, container,
    container_id, image, running, port?, stack?}];
  - install_block `installed|not_debian|unsupported_release|unsupported_arch` (null: the
    install is possible; null with `installed: true`: an install rynas started, with its
    `docker.sources`, still lacks the pin or the `docker-apps` account, and Install
    finishes it), task_id (a running install or migration);
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
| `docker.install` | Every step can run again, so running the install after any failure finishes the job. First, with `install_incomplete` (only Docker's or the engine's own packages count): `dpkg --configure -a --force-confdef --force-confold`, then `apt-get -s install -f` (logged, what the fix would do), then `apt-get install -f -y --no-remove -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold` (apt aborts rather than remove any package), all but the dry run through the same `systemd-run --wait --pipe` wrapper as every apt call (outside rynasd's sandbox). Every apt call waits up to 120 s for the dpkg lock (`-o DPkg::Lock::Timeout=120`), its transient unit carries `RuntimeMaxSec=1800` (the step's timeout), and a cancelled or timed out step also runs `systemctl stop <unit>` so apt never keeps running behind the task; when only another engine's packages (docker.io…) were half-done, the task stops there and adopts it. Then apt update, ca-certificates and curl, the key downloaded to a staging file and checked against Docker's published fingerprint `9DC8 5822 9FC7 DD38 854A E2D8 8D81 803C 0EBF CD88` (OpenPGP v4, computed by rynasd; another key fails the task and never reaches `/etc/apt/keyrings`), then written to `/etc/apt/keyrings/docker.asc` (0644; a keyring that already holds that key is kept, no download), `/etc/apt/sources.list.d/docker.sources` (deb822, signed-by), apt update, the pin `/etc/apt/preferences.d/rynas-docker` (`Pin: version 5:<major>.*`, priority 1001, major = the one an earlier attempt pinned, else apt's candidate). With `volume`, the apply of `docker` before the packages, so the engine starts on the volume. Then `apt-get install -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin` (apt skips what is installed), the system user and group `docker-apps` (only what is missing; an account still missing afterwards fails the task), and `systemctl enable --now containerd.service docker.service` |
| `docker.migrate` | refuses while containers run, when the volume is not in the mount table, or when `du -sx` of the old folders (+10 %) exceeds the volume's free space (all before anything stops); stops docker.socket, docker and containerd; creates the subvolumes; `cp -a --reflink=auto` of the old data-root and containerd root; applies `docker` with the new volume; starts the engine. Once stopped, the engine is started again on **any** failure (on its old folders, since the configuration only switches after a complete copy); the log says where the partial copy is. The old folders are kept (the log says where) |
| `docker.container.start\|stop\|restart` | the engine call (stop and restart give 30 s of grace) |
| `docker.stack.up` | `docker compose config`; the missing bind folders below the volume are created, owned by `docker-apps` (binds outside the volume are logged as warnings); then `docker compose -p <stack> --project-directory <dir> -f <dir>/<file> up --detach --wait=false` (30 min) |
| `docker.stack.down\|restart\|stop\|start` | the compose command (`down` never removes volumes; 10 min) |
| `docker.stack.pull` | snapshot (setting or body), then `pull` |
| `docker.stack.update` | snapshot (the services' images are recorded with their registry digests; `stop_first` stops the stack during the snapshot), `pull`, `up`. A non-zero exit of `pull` or `up` is a failure (compose's last error line in the message); on failure, an automatic rollback to that snapshot |
| `docker.stack.rollback` | `down`; a `before-rollback` snapshot of the current folder; the folder swapped with a writable copy of the snapshot; each service's image tagged back (`docker image tag <image_id> <ref>`, or `docker pull <repo>@<digest>` first when the image is gone); `up` |
| `docker.stack.remove` | `down`; with `delete_files`, the folder (subvolume) and its snapshots are deleted |
| `docker.update-check` | the registry digest of every tagged image in use (distribution endpoint, no pull) against the local repo digest. New updates are notified once per registry digest; the report is saved in `<state>/docker/updates.json` |

#### Stacks on disk (for the app catalogue, A1/A2)

- A stack is `/<vol>/docker/stacks/<name>/`:
  - `name`: 1-63 characters of `a-z 0-9 - _`, starting with a letter or a digit;
  - a btrfs subvolume where the volume is btrfs, a plain folder elsewhere (no snapshots
    then);
  - folder `0750 root:docker-apps`, `compose.yaml` `0640 root:docker-apps`, `.env`
    `0600 root:root` (§3 rule 7; a write repairs the owner of folders made before
    2026-11-07, and a stricter mode the administrator chose stays); extra files, owned by
    `docker-apps`, at
    relative paths (no `..`, no absolute paths, mode 0644 by default).
- Snapshots live in `/<vol>/docker/.snapshots/<name>/<YYYYMMDDTHHMMSS>` (plus a `.json`),
  outside the folder that managers scan.
- **The `docker-apps` account comes first.** Stack creation, stack file writes, `up`
  (bind folders), app installs and app updates make sure it exists (`groupadd`/`useradd`
  when missing) before a folder or file gets its owner. When it cannot be created the
  request or task fails with that message: folders are never given `nobody` (65534) or
  root instead. Only `--dry-system` runs without it.
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
  cpus, mem_bytes, mem_total_bytes, top 5 [{id, name, stack?, cpu_pct, mem_bytes}], at_ms,
  and `usage` [`ContainerUsage` {id, cpu, mem}]: every running container of the sample
  (full id, % of one CPU with one decimal, bytes), for the containers table. Same cgroup
  reading as the totals (one read per container per tick); `top` stays for the widget.
  Sent every 5 s while a stream listens; the latest one on connect.
- `docker.changed` (data `DockerChanged` {generation}): re-read the lists the page shows.
  The engine stream is filtered to the lifecycle actions (create, start, restart, die,
  stop, kill, destroy, rename, pause, unpause, update, oom, health_status): healthcheck
  `exec_*`, `attach`, `resize`… never reach rynasd. The first list-changing event arms a
  1 s timer and one hint covers everything until it fires; nothing ticks while the engine
  is quiet.
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
- **Watcher.** It reconnects with backoff (1 s doubling up to 60 s; back to 1 s only after
  a stream that stayed up 30 s, so a stream that fails at once slows down) and reports the
  containers that are unhealthy at each (re)connection. It checks the data-root fill level every minute,
  with 2 points of hysteresis.

#### Development

`rynasd --dry-system` reads the engine through `--docker-socket` (default
`/var/run/docker.sock`; empty means never contacted) and only logs changes: compose, engine
calls, apt, systemctl, copies. Folders go below the dry root (create `<dry-root>/<vol>` to
try stacks).

UI texts the Docker page needs for the repair (en + it): a notice when
`install_incomplete` is non-empty ("A previous Docker install did not finish: N packages
are half-installed. Install repairs them and finishes the install." with the package list
and statuses as they are), and the Install button shown while `install_block` is null even
with `installed: true` ("Finish the Docker install"). Suggested keys:
`docker.install_incomplete`, `docker.install_incomplete_hint`, `docker.install_finish`.

Not yet: scheduled update checks, image prune, a per-stack backup export, the Docker page
and the widget (D2).

### 5.2 API — wave A2 (apps, `/api/v1/apps/...`)

Code: `crates/rynasd/src/apps.rs` (catalogue cache, routes, tasks), built on `rynas-apps`
(render, migrations, update plan) and the D1 stack code (`crates/rynasd/src/docker.rs`).
Types: `rynas_api_types::apps`; every field is documented there. rynas-web forwards every
route unchanged. It treats every `/apps/` read as an administrator read, like `/docker/`.

**Access.** Every route needs the administrator role, reads and images included. rynasd checks
it too (403 `forbidden`). Every change is a task `apps.*` and a line of the audit log
(`apps <action> <target> by <user>`).

**Catalogue source.** rynasd reads `--apps-dir`:
- the default is `/usr/share/rynas/apps`, from the `rynas-apps-catalog` package;
- for development, a checkout's `apps/`, e.g. `rynasd --dry-system --apps-dir ./apps`.

The catalogue stays in memory. At most every 2 s, a request checks the name, size and mtime of
every file, and any change reloads it. An entry that fails validation goes into `issues` and is
never offered. A missing folder gives an empty catalogue with one issue.

**Disk is the source of truth.** An installed app is a rynas stack
(`/<vol>/docker/stacks/<stack>/`) whose `compose.yaml` has an `x-rynas` block. Nothing else
records installs. Apps always go on the Docker volume (setting `docker.volume`); `folders` can
place single folders elsewhere.

**Secrets.**
- They are generated at install and stored only in the stack's `.env` (0600).
- No GET returns them. Plans mask them as `********`.
- The install answer lists, once, the secrets the entry marks `show = true` in `[[secrets]]`
  (e.g. an initial administrator password).

#### Error codes

Only existing codes; this wave adds none.
- 400:
  - `invalid_value` with `field`: a form property, share, folder or port env, `stack`,
    `volume` (Docker has no volume yet) or `snapshot`;
  - `confirmation_required` (`field: confirm`).
- 404 `not_found`: unknown entry or image, a stack that is not an app, unknown snapshot.
- 409:
  - `already_exists` (`field: stack`): the stack folder exists;
  - `conflict` with `field`: the port env for a port in use, or `ram`, `arch`, `min_rynas`;
  - `conflict` without a field: an update when the app is up to date, or a downgrade;
  - `busy`: an action on that stack is running;
  - `docker_not_installed`: an install without an engine (plans still work).
- 503 `backend_unavailable`: the catalogue is not enabled in rynasd.

#### Routes

Paths are relative to `/api/v1`.

| Method | Path | Body | Answer |
|---|---|---|---|
| GET | `/apps/catalog` | — | `AppCatalog` {source, apps [`AppCard`] sorted by name, issues [{app, file, message}], installed_ram_mb?, arch, docker_installed, stacks_dir?} |
| GET | `/apps/catalog/{id}` | — | `AppDetail` {card, description (Markdown), homepage, source?, changelog_url, maintainers, host_network, form `AppForm`, web `AppWeb`?, folders, shares, secrets [{env, description, show}], hardware, overlays, named_volumes, notes, screenshots [URL]} |
| GET | `/apps/catalog/{id}/icon` | — | the SVG: `image/svg+xml`, at most 64 KiB, `nosniff`, a sandboxing CSP; `ETag`, and 304 on `If-None-Match` |
| GET | `/apps/catalog/{id}/screenshots/{file}` | — | a PNG or WebP of the entry, at most 1 MiB. Only file names the loader listed are served; nothing from the request reaches the filesystem |
| POST | `/apps/catalog/{id}/plan` | `AppInstallRequest` (optional) | `AppInstallPlan`. No side effects. Works without Docker (warning `docker_not_installed`) |
| POST | `/apps/catalog/{id}/install` | `AppInstallRequest` | 202 `AppInstallStarted` {task_id, stack, stack_dir, secrets [`ShownSecret` {env, description, value}]} (task `apps.install`). Refusals, checked in this order: `docker_not_installed`, `invalid_value` on `volume`, the first plan error, a port conflict, the RAM, the architecture, `min_rynas` |
| GET | `/apps/installed` | — | `Vec<InstalledApp>`, sorted by stack |
| GET | `/apps/installed/{stack}` | — | `InstalledAppDetail` {app `InstalledApp`, snapshots [`StackSnapshot`]} |
| GET | `/apps/installed/{stack}/update` | — | `AppUpdatePlan`. 404 when the entry left the catalogue |
| POST | `/apps/installed/{stack}/update` | optional `AppUpdateRequest` {answers, snapshot?, stop_first?} | 202 `TaskStarted` (`apps.update`). 409 `conflict` when up to date or for a downgrade. 400 `invalid_value` for a `missing_values` field that `answers` lacks |
| POST | `/apps/installed/{stack}/rollback` | `AppRollbackRequest` {snapshot} | 202 (`apps.rollback`). 404 for an unknown snapshot |
| POST | `/apps/installed/{stack}/convert` | — | 204. Removes `x-rynas` (and adds a header comment), nothing else. The stack stays as a custom stack and leaves this list |
| POST | `/apps/installed/{stack}/uninstall` | optional `AppUninstallRequest` {delete_data, confirm?} | 202 (`apps.uninstall`). `delete_data` needs `confirm` equal to the stack name |

#### Shapes worth knowing

- **`AppInstallRequest`** (every field optional):
  - `stack`: default the app id;
  - `form`: {PROPERTY: string, integer or boolean};
  - `shares`: {ENV: "/vol1/media"}, a folder that exists below a mounted volume;
  - `folders`: {ENV: "/ssd/docker/appdata/<stack>/<path>"}, below a mounted volume; created at
    install;
  - `ports`: {ENV: port}, relocatable ports only;
  - `publish_lan`: false binds proxied ports to `127.0.0.1`, so only the web access proxy
    reaches them (D-PROXY); true binds them to `0.0.0.0`. Unset (since W1): false while web
    access is active, true otherwise (§7, §5.3);
  - `start`: true (default) runs `up -d` after creating the stack.
- **`AppCard`**:
  - id, name, summary, categories, version, app_version, license, antifeatures;
  - ports [`AppPort`, as in `app.toml`];
  - ram_min_mb, ram_recommended_mb, and `ram`, a `RamGate`:
    - `{"state":"ok"}`;
    - `{"state":"below_recommended","installed_mb","recommended_mb"}`;
    - `{"state":"below_minimum","installed_mb","minimum_mb"}`;
    - `{"state":"unknown"}`;
  - arch_ok, rynas_ok, manager;
  - `reverse_proxy` (publishes 80/443 itself: needs external proxy mode) and `featured`
    (the curated Featured pick), both from `app.toml`;
  - icon (relative URL), installed [stack names], update_available.
- **`AppInstallPlan`**:
  - app, version, stack, stack_dir;
  - `ok`: the install would be accepted as it is;
  - `errors` [`FieldError` {field, kind, message}], every problem at once. `kind` is one of
    `invalid_value`, `missing_value`, `unknown_value`, `unsafe_value`, `share_missing`,
    `bad_path`, `bad_port`, `bad_stack`;
  - `port_conflicts` [{name, env, port, protocol, relocatable, suggestion?}]:
    - used ports are the published ports of every container, plus the host's listening TCP
      sockets and bound UDP sockets (`/proc/net/*`);
    - a suggestion skips used ports, the app's own ports and earlier suggestions;
  - `ports` [{name, env, port, protocol, proxied, bind?, purpose}];
  - ram, arch_ok, rynas_ok;
  - `folders` [{env, path, intent?, ssd, share, exists, owner "uid:gid", suggested?}].
    `suggested` is `/<apps_ssd_volume>/docker/appdata/<stack>/<path>`, for folders with `ssd`
    ok or preferred, when that preference is set;
  - secrets [{env, description, show}], never the values;
  - images, overlays, notes;
  - `warnings`, from this set:
    - `docker_not_installed`;
    - `no_volume`;
    - `engine_unreachable`: container ports were not checked;
    - `ram_below_recommended`;
    - `published_on_lan`;
  - `compose`, the `compose.yaml` that will be written, and `env`, the `.env` with secrets
    masked. Both are empty while there are errors.
- **`InstalledApp`**:
  - stack, app, name, version, app_version, catalog;
  - in_catalog, catalog_version?, update_available;
  - state? (`running|partial|stopped|down`; null when the engine does not answer), containers,
    running;
  - stack_dir, web_port?, web_path?, bind? (`RYNAS_BIND`);
  - snapshots: the folder is a subvolume.

  The Open button: with `bind = 0.0.0.0` it opens
  `<location.protocol>//<location.hostname>:<web_port><web_path>`. With loopback it opens
  `web_url`, the web access route (W1, §5.3).
- **`AppUpdatePlan`**:
  - stack, app, from_version, to_version, from_app_version, to_app_version;
  - up_to_date, downgrade;
  - migrations [versions];
  - image_changes [{service, kind `added|removed|tag|digest`, from?, to?}];
  - missing_values, changelog_url, snapshot.

#### Configuration

There is one preference, `apps_ssd_volume` (a volume label, or null), in the `docker` key. It is
staged and applied like the rest of `DockerSettings`. The wizard suggests that volume for
folders marked `ssd`. Installed state is never stored.

#### Tasks

| Type | What it does |
|---|---|
| `apps.install` | Makes sure the `docker-apps` account exists, then renders again with the plan's secrets and the account's real ids. `create_stack`: subvolume, owners and modes of §3 rule 7. Creates the folders below a mounted volume, missing parents included (descriptor-based, no symlink followed), mode 0750, owner PUID:PGID or the entry's `owner`, and records the folders it made for uninstall; shares are only checked. Then `up --detach`. A failure after the stack exists keeps the stack, and the log names the two ways forward: `POST /api/v1/docker/stacks/<stack>/up`, or `.../apps/installed/<stack>/uninstall` with `delete_data` |
| `apps.update` | Snapshot, by the setting or the body (`stop_first` stops the stack meanwhile). Writes the re-rendered files: values migrated; secrets, ports, folders, answers, `RYNAS_BIND` and PUID/PGID kept. Creates new folders, then `pull` and `up`. On failure: the D1 rollback to that snapshot. Without a snapshot (plain folder), the previous files are written back and `up` runs |
| `apps.rollback` | The D1 rollback: `down`, a `before-rollback` snapshot, the folder restored from the snapshot, images tagged back, `up` |
| `apps.uninstall` | `down`, never with `-v`. By default everything stays on disk, and the log says how to start the stack again or delete it. With `delete_data`, it deletes the stack folder, its snapshots and the folders **the install made** outside it. rynasd records them itself (`<state>/docker/app-folders/<stack>.json`, root only; install and update add to it) and never reads paths back from the stack's `.env`. A recorded folder is deleted only when it is below a volume in the mount table (deeper than its root), not under a system top folder (`/bin`, `/lib`, `/usr`, …), and holds no mount point; the deletion never follows a symlink, and parents the install made go only when empty. A folder that existed before the install is never recorded, so never deleted |

Under `--dry-system`, compose commands and deletions are only logged. Files and folders go below
the dry root; when the account does not exist they get nobody's ids (65534).

#### UI texts (i18n keys the UI adds, en + it)

Task names: the task list and the channel messages read `task.<type>`.

| Key | en | it |
|---|---|---|
| `task.apps.install` | Install app | Installazione dell'app |
| `task.apps.update` | Update app | Aggiornamento dell'app |
| `task.apps.rollback` | Roll back app | Ripristino dell'app |
| `task.apps.uninstall` | Uninstall app | Disinstallazione dell'app |

The UI also needs texts for the plan's closed sets:
- field error kinds: `invalid_value`, `missing_value`, `unknown_value`, `unsafe_value`,
  `share_missing`, `bad_path`, `bad_port`, `bad_stack`;
- warnings: `docker_not_installed`, `no_volume`, `engine_unreachable`,
  `ram_below_recommended`, `published_on_lan`;
- RAM gate states, antifeatures, categories, folder intents, image change kinds.

Entry texts (names, summaries, descriptions, port purposes, notes, form titles) come in English
from the catalogue and are shown as they are.

### 5.3 API — wave W1 (web access, `/api/v1/web-access/...`)

Code: `crates/rynasd/src/web_access/` (`mod.rs` service, configuration domain, routes and
the periodic check; `model.rs` validation, local names, access ranges, host facts;
`caddy.rs` the renderer; `admin.rs` the admin-socket client; `diag.rs` probes), the app
side in `crates/rynasd/src/apps.rs`, the package in `packaging/caddy/`. Types:
`rynas_api_types::web_access` (every field documented there). rynas-web forwards every
route unchanged and treats every `/web-access` read as an administrator read.

**What W1 does** (D-PROXY / D-EXPOSE revised 2026-11-07, `../research/web-access.md`):
- `rynas-caddy` owns 80/443 from install. The interface is at `https://<host>.local` and
  `https://<host>` through it; `:8443` (rynas-web, rustls) never goes through it and stays
  the emergency door.
- Each app route gets the local name `<label>-<host>.local`, published through Avahi's
  D-Bus API (below; not `/etc/avahi/hosts`), with a certificate from the rynas local CA, the same root as `:8443` ("Trust this
  NAS"). There is no ACME in W1.
- Access: `lan` (the default) is the NAS's connected subnets, plus loopback, link-local and
  the route's `allow`. `lan_vpn` adds the tailnet ranges, only while `tailscale0` exists
  (the anti-spoof drop on other interfaces belongs to the firewall plugin, W3), and the
  WireGuard subnets. Sources outside the ranges and unknown host names get the connection
  closed (Caddy `abort`), never a page.

**Access.** Every route needs the administrator role, reads included (rynasd checks it too:
403 `forbidden`). Changes are audit lines (`web-access <action> <target> by <user>`).

#### Error codes

Only existing codes:
- 400 `invalid_value` with `field`: the JSON path of the value (`settings.https_port`,
  `routes[2].label`, `routes[0].access`...);
- 404 `not_found` (unknown route id);
- 409 `nothing_pending` (confirm with nothing waiting); 409 `conflict` (a confirmation that
  did not come through the proxy on a new name; "Move behind the proxy" when web access is
  not active or the app is already behind it);
- 503 `backend_unavailable` (rynasd without web access).

Refusals of the engine are not HTTP errors: they fail the apply task, whose log carries
Caddy's message.

#### Routes

Paths are relative to `/api/v1`.

| Method | Path | Body | Answer |
|---|---|---|---|
| GET | `/web-access` | — | `WebAccessState` |
| PUT | `/web-access` | `WebAccessConfig` | `WebAccessState`. Validates, then stages the key `web_access` (apply with `/config/apply`, like every key). 400 `invalid_value` with `field` |
| POST | `/web-access/plan` | optional `WebAccessConfig` (default: the caller's staged or applied value) | `WebAccessPlan` {engine_config (the full Caddy JSON, `null` in external mode), avahi_hosts (the local names published through Avahi, as `<address> <name>` lines; no file is written), names, ui_route_changes}. No side effects |
| POST | `/web-access/sync` | — | `ProxyEngine`. Runs the anti-drift check now (below) |
| POST | `/web-access/confirm` | — | 204. Confirms a change of the interface's own route. rynas-web adds the connection's local address and the `Host`. It is accepted only when it arrived through the proxy (local address `127.0.0.1:<rynas-web plain port>`) on one of the new names. 409 `nothing_pending`; 409 `conflict` |
| POST | `/web-access/routes/{id}/test` | — | `RouteDiagnostics` {route, checks [`RouteCheck` {id, state `ok\|warn\|fail\|skipped`, detail}], at_ms}. 404 |
| POST | `/apps/installed/{stack}/proxy` | — | 202 `TaskStarted` (`apps.proxy`), "Move behind the proxy". 409 `conflict` when web access is not active, the app has no web interface, or it is already behind the proxy; 404 |

#### Shapes worth knowing

- **`WebAccessConfig`** (the key), every field with a default:
  ```json
  {"settings": {"mode": "builtin", "http_port": 80, "https_port": 443, "h3": true,
                "local_names": true, "ui": {"enabled": true, "access": "lan", "allow": []}},
   "routes": [{"id": "app-immich", "kind": "app", "stack": "immich", "app": "immich",
               "label": "immich", "upstream": {"scheme": "http", "host": "127.0.0.1", "port": 2283},
               "names": {"local": true}, "access": "lan", "allow": [], "auth": "app",
               "limits": {"body": "50G", "timeout": "600s", "buffering": false},
               "websocket": true, "subpath": false, "path": "/", "headers": "default",
               "enabled": true}]}
  ```
  - `mode` is `builtin` or `external`. `external` is external proxy mode: rynas-caddy is
    disabled and stopped, 80/443 are free for an NPM/Traefik/Zoraxy app, and the interface
    is only on `:8443`.
  - `access` is `lan`, `lan_vpn` or `public`. `public` is refused until the public flow
    (W3).
  - `kind` is `app`, `stack` or `custom`. `custom` (other LAN hosts) is refused until W2.
  - `headers` is `default` (nosniff, `Referrer-Policy`, `X-Frame-Options: SAMEORIGIN` when
    the app sends none; `Server`/`X-Powered-By` removed) or `relaxed` (framing allowed).
  - `sso` (the rynas login in front of a route) is in the model and refused until W3.
- **Validation** (field in brackets):
  - `id`: `[a-z0-9-]` 1-63, unique (`routes[i].id`);
  - `label`: `[a-z0-9-]` 1-40, unique, lowercased (`routes[i].label`);
  - `app`/`stack` routes name their stack (`routes[i].stack`);
  - the upstream is `127.0.0.1` or `::1`, port > 0;
  - `allow` holds addresses or CIDRs, never `/0`;
  - `auth = "none"` routes stay `lan` (`routes[i].access`);
  - `body` is `<digits>[K|M|G]`, `timeout` is `<digits>s|m|h`, `path` starts with `/`;
  - the ports are non-zero and different, and not rynas-web's plain or emergency port
    (`settings.https_port`, `settings.http_port`).
- **`WebAccessState`**: applied, pending?, engine `ProxyEngine` {installed, running, in_sync?,
  last_load_ms?, last_error?}, host (first label of the host name), ui_names, ui_urls,
  routes [`RouteView` {id, kind, stack?, app?, names, urls, upstream, access, enabled,
  problems}], lan (computed `lan` ranges), vpn (what `lan_vpn` adds), confirm?
  (`PendingConfirm` {deadline_ms, names}), emergency_port, reserved_ports.

  `problems` codes: `name_too_long` (`<label>-<host>` over 63 characters: no name),
  `local_names_off`, `disabled`, `name_collision` (another device, or this host, uses the
  name: Avahi refused it) and `name_unpublished` (Avahi is not on the system bus, or
  refused the name for another reason).
- **Route checks**, in order:
  0. `local_name`: the name's state in rynasd's Avahi publisher: `ok` published, `warn`
     being probed (or Avahi re-registering its host name), `fail` collision, refusal or
     Avahi not on the bus, `skipped` without a name or without a publisher
     (`--dry-system`);
  1. `dns`: the name through the NAS's resolver. `warn` when only loopback answers, `fail`
     when it does not resolve or points to another device;
  2. `proxy_running`: the admin socket answers;
  3. `route_loaded`: the route `@id` `rynas-route-<id>` is in the running configuration;
  4. `https_listening`: TCP to `127.0.0.1:<https_port>`;
  5. `certificate`: the engine's certificate covers the name and is valid;
  6. `upstream`: TCP to the upstream.

  `skipped` means the step does not apply (no name, the proxy not running).
- **App plan and list** (§5.2, extended):
  - `AppInstallRequest.publish_lan` is now optional: `null` means loopback plus a route
    while web access is active, the LAN otherwise (§7);
  - `AppInstallPlan` gains `proxy_active` and `web_url?` (`https://<label>-<host>.local/`
    when the install adds a route) and the warning `proxy_inactive`;
  - `InstalledApp` gains `web_url?`: the Open button uses it with loopback binding.

#### Configuration key `web_access`

A key that was never applied counts as the defaults. On a system with rynas-caddy
installed, the first check applies them (revision "web access on by default"), so the
interface is behind the proxy from install.

The apply:
1. hands the local names to rynasd's Avahi publisher (`web_access/avahi.rs`): one name
   `<label>-<host>.local` → the primary IPv4 per named route (the address of the interface
   with the default route). No file is written, nothing is reloaded.
2. migration: removes rynas's old `# BEGIN rynas web-access` block from
   `/etc/avahi/hosts` (written before 2026-11-07; the administrator's own lines are kept)
   and reloads `avahi-daemon` once when it runs. The periodic check does the same at
   start, and rewrites the key's drift records so the old block is forgotten.
3. with rynas-caddy installed:
   - `external`: `systemctl disable --now rynas-caddy`.
   - `builtin`:
     - `enable --now` when the unit is inactive;
     - issues the engine's certificate when a name is missing or it is near expiry
       (`/var/lib/rynas-caddy/rynas/local.{crt,key}`, 0640 root:rynas-caddy, in a folder
       `0750 root:rynas-caddy` the engine can read but not replace; written by descriptor,
       no symlink followed; the key
       written first; one leaf from the rynas local CA for every served name);
     - compares `GET /config/` with the render and calls `POST /load` when they differ
       (`Cache-Control: must-revalidate` when the certificate changed, so the engine
       re-reads the same paths);
     - writes `/var/lib/rynas-caddy/last-good.json` (0640 root:rynas-caddy; the state
       folder is the engine's, so the write is descriptor-based: `O_EXCL|O_NOFOLLOW`
       temporary file, `fchown`/`fchmod`, `renameat`).
   - A refusal (`400 {"error"}`) fails the apply. The rollback finds the previous
     configuration still running, since the engine keeps it on error.
4. without rynas-caddy the value is only stored, and loaded once the package exists.

**Timed confirmation.** It applies when an apply changes the interface's own route (mode,
ports, `settings.ui`) and the new value still serves it. `GET /web-access` then shows
`confirm` {deadline_ms, names}.
- The UI opens `https://<names[0]>/` and calls `POST /web-access/confirm` from there.
- Without a confirmation within 120 s, rynasd applies the previous value again (revision
  "…not confirmed…"). Only changes of the UI route wait; app routes never do.
- `:8443` is never affected.

**Anti-drift and host facts** (at start, every 30 s, and `POST /web-access/sync`):
- When the host name, the LAN subnets, `tailscale0`, the WireGuard subnets or the primary
  address change, the applied value is written again: access ranges and certificate names
  follow. Local names follow on every check (an unchanged set wakes nothing).
- Otherwise the running configuration is compared with the render and loaded again when it
  differs (Caddy restarted from an older file, a change through the socket).

Container events never reload the proxy. Upstreams are fixed loopback ports, and a reload
closes WebSockets until Caddy 2.12: `stream_close_delay` (5 min) keeps them open meanwhile.

#### Local names through Avahi (D-Bus)

`/etc/avahi/hosts` cannot carry them: a static entry also publishes the reverse PTR of the
address, which the NAS's host name already owns, so avahi-daemon refuses every app name
("Static host name …: avahi_server_add_address failure: Local name collision"; seen on
Debian trixie, avahi-daemon 0.8-16; https://pi3g.com/?p=9174, https://lists.freedesktop.org/archives/avahi/2009-April/001625.html;
`../research/web-access.md` §3.2). rynasd publishes them over the system bus with zbus:
- `org.freedesktop.Avahi.Server.EntryGroupNew` →
  `EntryGroup.AddAddress(interface -1, protocol -1, flags AVAHI_PUBLISH_NO_REVERSE = 16,
  name, address)` → `Commit` (values from `avahi-common/defs.h` and `address.h`);
- one entry group per name, all owned by rynasd's own long-lived bus connection: Avahi
  reports collisions per group (`EntryGroup.StateChanged` → 3), so one taken name never
  takes the others down; Avahi frees the groups when rynasd disconnects;
- names are published once `Server.GetState`/`StateChanged` says `RUNNING` (2); another
  server state (its own host name re-registering) withdraws them, `RUNNING` again adds them
  back;
- `NameOwnerChanged` of `org.freedesktop.Avahi`: Avahi restarted (republish everything) or
  went away (names unavailable);
- a collision or a refusal is kept in the report (route problem, `local_name` check) and
  not retried in a loop: only an address change, an Avahi restart or `POST
  /web-access/sync` tries again;
- routes removed or disabled, external mode or local names off: their groups are freed;
- IPv4 only (A records of the primary address, research §3.2);
- without Avahi on the bus, or without the system bus, local names are unavailable (the
  check says so) and nothing else in web access changes. rynasd's unit already allows
  `AF_UNIX` (`RestrictAddressFamilies`), and Avahi's bus policy lets any client publish;
- `--dry-system` and tests never start the publisher; the `local_name` check is then
  `skipped`.

`cargo test -p rynasd -- --ignored real_avahi` publishes a test name against the real bus.

#### What rynasd renders (Caddy JSON)

Pinned by the golden file `crates/rynasd/tests/golden/web_access_caddy.json`, which
`packaging/caddy/build.sh --validate` checks with the real binary.
- **Admin and globals**:
  - `admin.listen` `unix//run/rynas-caddy/admin.sock|0600`
    (https://caddyserver.com/docs/conventions#network-addresses); Caddy skips the Host and
    Origin checks on Unix sockets (`admin.go`);
  - `admin.config.persist: false`;
  - storage `file_system` in `/var/lib/rynas-caddy/data`;
  - JSON logs on stderr (the journal).
- **Server `rynas_https`** (`:443`):
  - protocols h1, h2 and h3, `strict_sni_host`;
  - `automatic_https.disable` (no ACME in W1), TLS through `tls_connection_policies: [{}]`
    and `tls.certificates.load_files`
    (https://caddyserver.com/docs/json/apps/tls/certificates/load_files/);
  - access logs on;
  - one route per host set (`@id` `rynas-ui` or `rynas-route-<id>`, `terminal`). Inside a
    subroute, sources outside the ranges are aborted
    (`not` + `remote_ip`, https://caddyserver.com/docs/json/apps/http/servers/routes/match/remote_ip/;
    `static_response.abort`), then:
    - the header handlers;
    - `request_body.max_size` in bytes
      (https://caddyserver.com/docs/json/apps/http/servers/routes/handle/request_body/);
    - `reverse_proxy` (https://caddyserver.com/docs/json/apps/http/servers/routes/handle/reverse_proxy/):
      HTTP transport `dial_timeout` 5 s, `read_timeout` = `limits.timeout`,
      `tls.insecure_skip_verify` for `https` upstreams on loopback;
      `flush_interval: -1` when `buffering` is false; `stream_close_delay` 5 min;
      `Remote-User/-Groups/-Email/-Name` always deleted from requests.
  - the interface's route: upstream `127.0.0.1:<rynas-web plain port>`, unbuffered, no read
    timeout (server-sent events);
  - a last route that aborts every other host name.
- **Server `rynas_http`** (`:80`): served names get `307` to `https://<host><uri>`; any
  other name or a bare address gets `307` to `https://<host>:8443<uri>`; both carry
  `Cache-Control: no-store`.
- **External mode** renders `null`: the engine does not run.

Checked against the built rynas-caddy 2.11.9 in a throw-away container (2026-11-07):
- the golden render validates and loads over the socket;
- `GET /config/` returns it unchanged, so the anti-drift compares equal;
- a refused load (400) leaves the old configuration running and the socket in place;
- served names proxy, unknown names are aborted, and `:80` redirects as above.

**Invariant:** every load carries the same `admin` block. A refused configuration with a
*different* admin block (for example none, which means TCP `localhost:2019`) made Caddy
stop the Unix admin socket in that test. rynasd only ever sends its own render.

#### Apps (§3 rule 8, §7)

- **Install.** While web access is active (built-in mode, rynas-caddy installed),
  `publish_lan` defaults to false: `RYNAS_BIND=127.0.0.1`, and after the stack is created
  the task adds a route with `apply_update` on `web_access`. The route has:
  - id `app-<stack>`;
  - label = the stack name with `_` and other characters turned into `-`, at most 40
    characters, `-2`, `-3`… on a collision;
  - upstream `127.0.0.1:<web port>`, access `lan`;
  - auth, body, timeout, buffering, websocket, subpath and path from the catalogue `[web]`.

  The task log names the URL. A route that cannot be added is a warning in the log, never
  a failed install.
- **While web access is not active**, `publish_lan` defaults to true. Loopback asked for
  explicitly gives the warning `proxy_inactive`.
- **Proxy ports.** The proxy's ports (80/tcp, 443/tcp, and 443/udp with h3) count as used
  in the port check, running or not, unless `mode = "external"`.
- **`apps.proxy`** ("Move behind the proxy"):
  - sets `RYNAS_BIND=127.0.0.1` in `.env` and runs `up`. When `up` fails, the previous
    `.env` comes back and `up` runs again;
  - adds the route;
  - host-network apps only get the route.
- **`apps.uninstall`** removes the stack's route after `down`.

#### Package `rynas-caddy` (`packaging/caddy/`)

`build.sh` runs in `golang:1.27.3-trixie`, pinned by digest, as the calling user:
- `xcaddy build v2.11.9` with xcaddy 0.4.7, `CGO_ENABLED=0`, `-trimpath`, no VCS stamp,
  stripped, `GOAMD64=v2`, `SOURCE_DATE_EPOCH` from the last commit of `packaging/caddy`;
- `rynas-caddy validate` on the bootstrap configuration and on every `--validate` file;
- `dpkg-deb --root-owner-group` → `target/caddy/rynas-caddy_2.11.9-1_amd64.deb` + `.sha256`.

Versions live in `versions.env`, checked 2026-11-07: Caddy 2.11.9 (2026-11-05), Go
1.27.3, xcaddy 0.4.7 (https://github.com/caddyserver/caddy/releases,
https://go.dev/dl/?mode=json, https://github.com/caddyserver/xcaddy/releases).
`check-upstream.sh` is the daily CI watch: it exits 1 when one of them is newer.

W1 builds without DNS modules. `dns-modules.txt` lists the W2 providers: cloudflare,
route53, ovh, hetzner, desec, duckdns, gandi, porkbun, digitalocean, rfc2136 and acmedns.
`--dns` refuses unpinned lines. Pinned in W2 (§5.4): `build.sh --dns` checks one
`dns.providers.*` module per line with `list-modules` and gives `--validate` throw-away values
for the `{file.*}` secrets.

Contents:
- `/usr/bin/rynas-caddy`;
- `rynas-caddy.service`:
  - user `rynas-caddy` from `sysusers.d`, `Type=notify`;
  - `ExecStartPre` copies `/usr/share/rynas-caddy/bootstrap.json` to `last-good.json`
    when it is missing (plain HTTP → `:8443`), then `run --config last-good.json`;
  - `AmbientCapabilities`/`CapabilityBoundingSet` `CAP_NET_BIND_SERVICE` only,
    `ProtectSystem=strict` and the usual sandboxing (`systemd-analyze security`: 1.6 OK);
  - `GOMEMLIMIT=100MiB`, `MemoryHigh=128M`, `MemoryMax=256M`;
  - `StateDirectory`/`RuntimeDirectory` 0750, logs to the journal;
- `Conflicts: caddy`.

postinst enables and restarts the unit, unless rynasd had disabled it (external mode).
Purge removes `/var/lib/rynas-caddy`.

#### Tasks

| Type | What it does |
|---|---|
| `apps.proxy` | `RYNAS_BIND=127.0.0.1` in `.env` + `up` (unless host network), then the web access route |

Applies of `web_access` run as the usual configuration apply (`/config/apply`).

#### Development

`rynasd --dry-system` never touches a proxy. rynas-caddy counts as installed only when its
unit file exists below the dry root, or when `--caddy-admin` (default
`/run/rynas-caddy/admin.sock`) points at a socket that exists. Files go below the dry root.
`--web-tls-port` (default 8443) is the port the `:80` catch-all redirects to.

#### UI texts (i18n keys the UI adds, en + it)

| Key | en | it |
|---|---|---|
| `task.apps.proxy` | Move app behind the proxy | Spostamento dell'app dietro il proxy |

The UI also needs texts for these closed sets:
- `ProxyMode`: `builtin`, `external`;
- `AccessLevel`: `lan`, `lan_vpn`, `public`;
- `RouteKind`: `app`, `stack`, `custom`;
- `HeaderPreset`: `default`, `relaxed`;
- `CheckState`: `ok`, `warn`, `fail`, `skipped`;
- check ids: `local_name` (new), `dns`, `proxy_running`, `route_loaded`,
  `https_listening`, `certificate`, `upstream`;
- route problems: `name_too_long`, `local_names_off`, `disabled`, `name_collision` (new),
  `name_unpublished` (new);
- the plan warning `proxy_inactive`;
- the confirmation banner (deadline and the address to open).

Check details and engine errors are English, shown as they are.

#### UI notes W1 (rynas-ui, 2026-11-07)

Where it lives: **Network › Web access** (`/network/web-access`, a link card on the Network
page, a palette entry, and a "Certificate" card on System opening `?tab=certificate`), with
three tabs: Overview (engine, this interface, settings), App addresses (routes) and
Certificate (W0). The configuration is edited as JSON (`serde_json::Value`): the page changes
only `settings.mode|local_names|h3|ui.access` and `routes[i].access|enabled` and sends the
rest back, so no typed copy of the key's deserializer goes into the wasm. Saves follow A19b
(PUT, then `/config/apply` with `web_access` only). The apps wizard has an Access step for
entries with `[web]` (not for host networking): "Through web access (recommended)" with the
plan's `web_url`, or "Directly on the LAN port"; `publish_lan` stays `null` until the
administrator picks one. "Move behind the proxy" shows the resulting address
(`<label>-<host>.local`, from `host` of the state). The Docker install card shows
`install_incomplete` (package, version, dpkg status) and "Finish the Docker install"; the
same card appears on the Overview tab for an install rynas started that lacks its last steps.
Since A3 the Featured tab lists the entries marked `featured` (those that fit this NAS
first), and a `reverse_proxy` entry cannot be installed until web access is in external proxy
mode (the drawer links to the mode setting; the wizard skips its Access step).
The wizard counts its steps once the entry and the web access state are both in (the stepper
never grows); without web access there is no Access step and the Network step says where the
app opens. The timed confirmation (review 2026-11-07, `docs/quality/review-2026-11-07-web-access-gui.md`):
Confirm only when the page is on one of `confirm.names` *and* on the HTTPS port of web access
(never on the emergency port, where rynasd refuses it), otherwise one Open link per name;
after the deadline the state is read again with a growing pause until `confirm` is gone; a
confirmation that disappears while `applied.settings` changes back is a rollback, said with a
toast and a note in the banner's place; the Settings form is built again whenever the
server's settings change. A route whose app has no login (`auth: none`) offers `lan_vpn`
disabled, with the reason as its help.

Patterns taken, and complaints avoided:
- one row per address with an access badge and a status, details in a drawer (Nginx Proxy
  Manager's proxy hosts list); not NPM's tabbed host modal, where a hidden SSL tab blocked
  the save without an error and the files drifted from what the UI showed (NPM issues #150,
  #1753): one form, server state re-read after every save, drift shown as "running an older
  configuration" with Check and load again;
- the certificate as its own panel with subject, names, validity and the services using it
  (DSM Control Panel › Security › Certificate), plus the local CA download and per-system
  steps next to it (Proxmox and Cockpit leave this to the docs);
- a per-route **Test** with a fix next to each failed step (Cloudflare Zero Trust's hostname
  status; DSM gives no diagnostics at all);
- access as a choice of who may connect, separate from the name (Unraid's per-container
  Tailscale toggle, Cosmos' per-URL settings), with `public` visible but disabled until its
  own flow (research §9) instead of a toggle that opens the app to the internet;
- an "Open Web UI" link wherever an app is listed (Home Assistant add-ons);
- the timed confirmation of the interface's own address, the same model as the network
  trial apply (D-NET), with the emergency `:8443` address always shown; the lockout DSM and
  ZimaOS users hit after moving the UI ports does not happen silently.

Sources (2026-11-07): https://github.com/NginxProxyManager/nginx-proxy-manager/issues/150,
https://github.com/NginxProxyManager/nginx-proxy-manager/issues/1753,
https://github.com/NginxProxyManager/nginx-proxy-manager/issues/372, research §2 and §9.
"Trust this NAS" steps: Windows certificate import wizard (Local Machine › Trusted Root
Certification Authorities), macOS Keychain Access "Always Trust", iOS profile install then
Settings › General › About › Certificate Trust Settings (an iOS 18 report: the toggle shows
only after the profile is fully installed, https://community.start9.com/t/enable-trusted-certificate-on-iphone-ios-18/2051),
Android Settings › Security › Encryption & credentials › Install a certificate › CA
certificate (menu names vary by maker; apps may ignore user CAs), Debian
`update-ca-certificates`, Fedora/Arch `trust anchor`, Firefox's own Authorities store,
Chrome on Linux 134+ `chrome://certificate-manager` › Local certificates › Custom › Installed
by you › Trusted certificates › Import
(https://chromium.googlesource.com/chromium/src/+/main/net/data/ssl/chrome_root_store/faq.md).

#### Not in W1

- **W2:** domains, credentials, DNS-01 wildcard, DNS records managed by rynas, rebind
  detection, the certificate dashboard, `:8443` reusing the wildcard, custom routes and
  routes for foreign stacks (the model's `kind`), the DNS modules pinned.
- **W3:**
  - `lan_vpn` anti-spoof in the firewall;
  - the rynas login in front of apps (`sso`: `/api/v1/auth/verify`, the `rynas_sso`
    cookie, groups, bypass paths, `forward_auth` with `copy_headers`; identity headers
    are already stripped);
  - `public` with its checklist, auto-block from the access logs, HSTS;
  - Tailscale Services.
- **Owed by rynas-web** — done in W1b (`../phase1/plan.md`, "Behind the built-in reverse
  proxy (W1b)"):
  - `X-Forwarded-For` trusted from the loopback proxy on the plain port only (right-most
    untrusted entry), so the login rate limit and fail2ban see clients instead of
    `127.0.0.1`;
  - the plain listener moving to `127.0.0.1:8080` (`--listen auto`) once rynasd publishes
    `/run/rynas/web-access.ui` and `:8443` is up.

### 5.4 API — wave W2 (own domains, `/api/v1/web-access/...`)

Code: `crates/rynasd/src/web_access/` (`domains.rs` credentials, names, engine secret files,
records; `certs.rs` the certificate dashboard and its notifications; the W2 part of `mod.rs`
records sync, domain checklist, inventory; `caddy.rs` the ACME policies), `crates/rynas-dns`
(provider record clients behind the `DnsApi` trait, planning, a minimal DNS client, rebind
verdict), the modules in `packaging/caddy/dns-modules.txt`. Types: `rynas_api_types::web_access`
(every field documented there). Research: `../research/web-access.md` §13. Every route needs
the administrator role (reads too); rynas-web forwards them unchanged.

**What W2 does:**
- A domain (ideally a subzone, `nas.example.com`) gets one certificate per name of `*.<domain>`
  and `<domain>` from Let's Encrypt through ACME DNS-01 with the provider's caddy-dns module
  (ARI on by default, optional account e-mail and ACME profile, a staging switch). No port is
  opened.
- rynas writes the LAN records `*.<domain>` and `<domain>` A (AAAA with `lan_ipv6`) = the NAS's
  LAN address through the provider's API, with the same token, and keeps them current when
  the address changes. Every LAN device then resolves `<app>.<domain>` with a trusted
  certificate, mobile apps included. Records rynas did not write are never changed or
  deleted without consent: they show as `conflict` until the administrator takes the name
  over (`take_over`).
- Every route gets `<label>.<domain>` next to its local name; the interface gets `<domain>`
  (per domain, `ui`). The app's Open button prefers the domain name.
- The domain checklist tells DNS rebind protection (the router drops public answers that
  point to private addresses) from a missing or wrong record.
- The certificate dashboard lists every certificate rynas serves; certificate trouble becomes
  D-NOTIFY events (firing on change, resolved when it clears).

#### Configuration key `web_access` (additions)

```json
{"domains": [{"id": "home", "name": "nas.example.com", "provider": "cloudflare",
              "credential": "cf", "challenge": "dns01", "wildcard": true,
              "lan_record": true, "lan_ipv6": false, "ui": true, "enabled": true,
              "take_over": []}],
 "credentials": [{"id": "cf", "provider": "cloudflare", "label": "token for example.com",
                  "secrets": {"api_token": "…"}}],
 "acme": {"email": "admin@example.com", "staging": false, "profile": "tlsserver"},
 "routes": [{"…": "…", "names": {"local": true, "domain": true}}]}
```

- `challenge`: `dns01` (default) · `http01` (port 80 reachable from the internet, one
  certificate per name: `wildcard` is forced off, app names land in CT logs; not checked
  against a live CA yet) · `cname_delegation` (`delegate_to` = the CNAME target of
  `_acme-challenge.<name>`; the credential cannot write the main zone, so the records are
  listed for the administrator instead of written).
- `provider`: `cloudflare`, `desec`, `hetzner`, `porkbun`, `ovh`, `duckdns`, `gandi`,
  `route53`, `digitalocean`, `rfc2136`, `acmedns`. Required except for `http01` without
  `lan_record`. `acmedns` only with `cname_delegation`. DuckDNS domains are `<x>.duckdns.org`
  (its one record covers every name below). **DuckDNS has no read API:** rynas sets its one
  record to the NAS's LAN address on every sync, replacing whatever it held (a public
  address kept by a DDNS updater too); the form and every plan say so (`update` with that
  `detail`), and with the domain removed the record is left as it is (`conflict`).
- `take_over`: record names (`<domain>` or below, `*.<domain>` allowed) whose existing values
  rynas may replace although it did not write them. Without it such a set is a `conflict`
  and stays as it is. Names are lowercased; duplicates dropped.
- **Credentials are write-only.** A PUT that carries `secrets` replaces the values; a PUT
  without `secrets` keeps the stored ones (send back what GET gave). Answers carry `fields`
  (the names stored), never `secrets` or `sealed`. Values are sealed at once with the
  notification engine's age key (`<state>/notify/secret.key`): `config.db`, its revisions and
  a `.rynasbak` without a passphrase hold only the envelope; the key travels only in the
  passphrase-encrypted secrets of an export. A credential restored without its key shows
  `credential_missing` until its values are entered again. The fields per provider come
  from `GET /web-access/providers`.
- Validation (`400 invalid_value`, `field` in brackets): ids `[a-z0-9-]` 1-63 unique
  (`domains[i].id`, `credentials[i].id`); `name` a DNS name of ≥ 2 labels, ≤ 212 characters,
  no wildcard, not `.local`/`.lan`/`.home.arpa`/`.internal`… (no public CA), unique
  (`domains[i].name`); provider and credential present and matching (`domains[i].provider`,
  `domains[i].credential`); `delegate_to` for delegation (`domains[i].delegate_to`); the
  credential's values complete and known (`credentials[i].secrets.<field>`; a new credential
  without values: `credentials[i].secrets`); `take_over` names of the domain
  (`domains[i].take_over`); `acme.email` an address, `acme.profile`
  `[a-z0-9-]` ≤ 32. Names are lowercased and lose a final dot.
- Minimum-scope tokens (shown under the form, `DnsProviderInfo.token_hint`): Cloudflare an API
  token with Zone → DNS → Edit on the one zone (never the global key); deSEC a token whose
  policy allows only this domain (optionally only `*.<d>`, `<d>`, `_acme-challenge.<d>`);
  Porkbun API access turned on for this domain only; OVH a consumer key limited to
  `/domain/zone/<zone>/*`; Gandi a PAT for the one domain; Route 53 an IAM policy on the
  one hosted zone; RFC 2136 a TSIG key limited to `_acme-challenge` TXT. Hetzner Cloud tokens
  cover a project, DigitalOcean tokens every domain, DuckDNS tokens every name of the
  account: the form recommends CNAME delegation there (`scoped_tokens: false`).

#### Routes

| Method | Path | Body | Answer |
|---|---|---|---|
| GET | `/web-access` | — | `WebAccessState`, now with `domains` [`DomainView`]; `applied`/`pending` without secrets |
| PUT | `/web-access` | `WebAccessConfig` | as in W1; credentials sealed (above). Audit lines name a credential id, never a value |
| POST | `/web-access/plan` | optional `WebAccessConfig` | `WebAccessPlan` + `dns_changes` [`DnsChange`]: the providers are **read** (never written) to say exactly what an apply changes; `engine_config` with every literal secret as `***` |
| POST | `/web-access/domains/{id}/test` | — | `RouteDiagnostics` (the checklist below). 404 |
| POST | `/web-access/domains/{id}/records` | — | `DnsSyncReport`: writes the domain's records now ("Write records"). 404 |
| GET | `/web-access/certificates` | — | `CertificateInventory` |
| GET | `/web-access/providers` | — | `[DnsProviderInfo]` {provider, fields [{name, secret, optional}], records, wildcard_records, scoped_tokens, min_ttl, token_hint} |

No new error codes (`invalid_value`, `not_found`, `forbidden`, `backend_unavailable`).

#### Shapes worth knowing

- **`DnsChange`** {domain, name, rtype `A|AAAA|CNAME`, action, current [], wanted [], ttl,
  detail}; `action` `keep` · `create` · `update` · `delete` · `manual` (the administrator adds
  it: delegation, a provider without a rynas client) · `unknown` (the provider could not be
  read; `detail` says why) · `conflict` (a set rynas did not write, or one changed since rynas
  wrote it, holds other values: nothing is written; with `wanted` it is written once its name
  is in `take_over`, without `wanted` it is a set rynas no longer wants, left in place and no
  longer managed). After a write, a failed change keeps its action and `detail` starts with
  `failed: `. TTL: 300 or the zone's minimum (deSEC 3600, Porkbun 600).
- **`DnsSyncReport`** {domain, changes, ok, at_ms}: the last sync per domain, in
  `DomainView.records`.
- **`DomainView`** {id, name, enabled, certificate_names, ui_urls, records?, problems};
  problems: `credential_missing` (no stored values, or they cannot be decrypted),
  `records_failed`, `records_manual`, `records_conflict` (a wanted set rynas did not write
  holds other values: take it over or fix it at the provider), `no_lan_address`, `staging`.
- **`RouteView.problems`** adds `domain_name_too_long`; `names`/`urls` list the domain names
  after the local one.
- **Domain checklist** (`RouteDiagnostics.checks`, in order):
  1. `dns_record`: the last record sync (`ok`; `warn` not written yet or to add by hand, with
     the records; `fail` with the provider's error);
  2. `dns_nas`: a name under the domain resolves on the NAS to its LAN address;
  3. `dns_public`: the same through public resolvers (`1.1.1.1`, `9.9.9.9`) asked directly;
     `warn` when they cannot be asked (outbound DNS blocked);
  4. `rebind`: the LAN resolvers (`/etc/resolv.conf`, systemd-resolved's upstreams) compared
     with the public answer: `fail` when the public answer is the private LAN address and
     the LAN resolver drops it (NXDOMAIN, no data, REFUSED, 0.0.0.0), with the fix for
     OpenWrt/dnsmasq, Fritz!Box, pfSense/OPNsense or a local DNS app; `warn` when the LAN
     resolver answers other addresses (a local override); `skipped` while the public record
     is not right;
  5. `certificate`: the ACME certificates of the domain (`warn` being obtained or staging,
     `fail` not obtained after 15 minutes with the engine's last error, or expired);
  6. `renewal`: `ok` with the date renewal starts; `warn` expires within 14 days; `fail`
     renewal overdue;
  7. `hairpin`: `ok` the name points to the LAN address (no NAT loopback needed); `warn` it
     points to a public one.
- **Route test** adds `domain_dns` and `domain_certificate` for a route with a domain name.
- **`CertEntry`** {id, kind, status, names, domain?, staging, source?, certificate?
  (`TlsCertificate`), renew_after_ms?, last_error?}; ids `local-ca`, `interface`,
  `proxy-local`, `acme:<name>` (certmagic keeps one certificate per name: `*.d` and `d`);
  kind `local_ca` · `interface` · `proxy_local` · `acme`; status `ok` · `expires_soon`
  (< 14 days) · `expired` · `renewal_overdue` (ACME, a day past 2/3 of its lifetime) ·
  `pending` (expected, not there yet) · `failed` (expected for more than 15 minutes).
  `last_error` comes from the engine's JSON log (`tls.obtain`/`tls.renew` errors per
  identifier, cleared by a later success). Read-only: the files in rynas-caddy's storage
  (`/var/lib/rynas-caddy/data/certificates/<ca>/<name>/`), never its admin API.

#### Notifications (D-NOTIFY)

The periodic check (every 30 s, the inventory at most every 10 minutes) emits on changes only:
a certificate entering trouble fires once (again if it gets worse), clearing it resolves.

| Code | State, key, category, severity | Fields |
|---|---|---|
| `web_access.cert_expires_soon` / `web_access.cert_renewal_overdue` / `web_access.cert_failed` | firing, `web-access:cert:<id>`, system, warning | `name`, `cert` |
| `web_access.cert_expired` | firing, same key, system, critical | `name`, `cert` |
| `web_access.cert_ok` | resolved, same key | `name`, `cert` |

#### What rynasd renders (additions to §5.3)

Pinned by `crates/rynasd/tests/golden/web_access_caddy_domains.json`, validated with the real
rynas-caddy 2.11.9 + 11 DNS modules (`build.sh --dns --validate`, 2026-11-07):
- `apps.tls.certificates.automate` lists every ACME subject; `load_files` (the local leaf)
  only while there are local names; the local leaf covers only local names.
- One `apps.tls.automation.policies` entry per enabled domain: `subjects`, and one `acme`
  issuer with `ca` (Let's Encrypt or staging), `email`/`profile` when set, `challenges`:
  `tls-alpn` off; for DNS-01 `http` off and `dns` {`provider` {`name`, fields},
  `resolvers` ["1.1.1.1:53", "9.9.9.9:53"], `override_domain` (delegation)}.
- Provider fields that the module replaces placeholders in are rendered as
  `{file./var/lib/rynas-caddy/rynas/dns/<credential>/<field>}`; rynasd writes those files
  (0640 root:rynas-caddy, folders 0750) before each load and removes the ones no longer used.
  The JSON, `last-good.json` and `GET /config/` hold no secret. Fields the module does not
  replace (acme-dns username/subdomain/server URL, RFC 2136 server and key name,
  DigitalOcean's token, not checked in its source) are written as values. **DigitalOcean
  and RFC 2136 therefore keep their secret (`auth_token`, the TSIG `key`) in the engine
  configuration**: in each load body and in `last-good.json` (0640 root:rynas-caddy, readable
  by the engine, which holds the value anyway). An engine error that echoes the
  configuration is stored and logged with every secret value replaced by `***`.
- A changed secret with an unchanged JSON forces a reload (`Cache-Control: must-revalidate`).
- External mode renders `null`: no ACME either. The LAN records are written in every mode
  (they point to the NAS, useful for an external proxy too).

#### Records

- On every apply (after the engine), and when the host facts change (address, host name):
  per domain, plan (read the provider) then write `create`/`update`/`delete`. Failures are
  logged, reported in the domain view, never fail the apply, and are tried again after 10
  minutes or on the next apply; "Write records" runs one domain now.
- rynasd remembers the record sets it wrote **with the values it wrote**
  (`<state>/web-access/dns-records.json`, 0600). A wanted set is created when missing, kept
  when it already holds the wanted values (not adopted: rynas does not own it), updated when
  rynas wrote it and it still holds what rynas wrote, and otherwise a `conflict` until its
  name is in `take_over`. A set rynas owns is deleted when the domain is removed or disabled,
  `lan_record` is turned off, a per-name record is no longer wanted, or the provider changes
  (through the old credential) — **only while the provider still holds exactly the values
  rynas wrote**; a set edited since is left in place, reported as `conflict` and no longer
  owned. A record rynas did not write is never deleted.
- The record state lock is never held across a provider call: syncs run one at a time
  under their own guard, so `GET /web-access` and the checklist never wait on a slow
  provider. Provider reads are cached (zone lookups 10 minutes, record sets 30 s, cleared by
  every write) and an HTTP 429 is retried once after `Retry-After` (≤ 10 s), else reported as
  rate limited and tried again with the 10-minute retry.
- `--dry-system` never calls a provider (the plan shows `unknown`). Tests use fakes only.

#### Development

`cargo test -p rynas-dns` (provider clients against fake HTTP servers, wire format against a
fake UDP server), `cargo test -p rynasd --test web_access` (render goldens, sealing and
redaction in answers, logs, `config.db`, the export and the engine files; records with an
in-memory provider; checklist; inventory and notification dedup).

#### UI texts (i18n keys the UI adds, en + it)

Closed sets:
- `DnsProvider` (names as brands: Cloudflare, deSEC, Hetzner, Porkbun, OVHcloud, DuckDNS,
  Gandi, Amazon Route 53, DigitalOcean, RFC 2136, acme-dns);
- `ChallengeKind`: `dns01`, `http01`, `cname_delegation`;
- `DnsAction`: `keep`, `create`, `update`, `delete`, `manual`, `unknown`, `conflict`;
- `CertKind`: `local_ca`, `interface`, `proxy_local`, `acme`;
- `CertStatus`: `ok`, `expires_soon`, `expired`, `renewal_overdue`, `pending`, `failed`;
- domain problems: `credential_missing`, `records_failed`, `records_manual`,
  `records_conflict`, `no_lan_address`, `staging`; route problem `domain_name_too_long`;
- check ids: `dns_record`, `dns_nas`, `dns_public`, `rebind`, `certificate`, `renewal`,
  `hairpin`, `domain_dns`, `domain_certificate`;
- notification codes `nev.web_access.cert_*` (table above) with `{name}`;
- credential field names (`api_token`, `token`, `api_key`, `api_secret_key`, `endpoint`,
  `application_key`, `application_secret`, `consumer_key`, `bearer_token`, `access_key_id`,
  `secret_access_key`, `region`, `auth_token`, `server`, `key_name`, `key_alg`, `key`,
  `server_url`, `username`, `subdomain`, `password`).

`token_hint`, check details, `DnsChange.detail` and `last_error` are English, shown as they
are.

#### Not in W2

- `:8443` reusing the wildcard (rynas-web still serves the local or imported certificate);
- custom routes to other LAN hosts and routes for foreign stacks (`kind: custom` still
  refused);
- record clients for Gandi, Route 53, DigitalOcean, RFC 2136 (their records are `manual`);
- the ACME account key in the `.rynasbak` (it stays in rynas-caddy's storage, which survives
  a reinstall of rynas but not a purge of rynas-caddy);
- DNS-PERSIST-01; writing the record into an AdGuard Home / Pi-hole app as the rebind fix.

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
| `reverse_proxy` | a reverse proxy app (Nginx Proxy Manager, Traefik) that publishes host ports 80 and 443 itself: it must declare tcp ports with defaults 80 and 443, `relocatable = false`, not `proxied`, on a bridged network. The web access proxy owns 80/443, so the app installs only in external proxy mode (or without web access): the port check refuses it otherwise, and the card shows `reverse_proxy` so the UI can say so first. No other bridged entry may default to 80/443 tcp; reverse proxies are exempt from the unique-default rule for those two ports |
| `featured` | listed in the catalogue's Featured section (a small curated pick; the UI shows it, it does not compute it). Catalogue presentation only: changing it alone does not bump `version`, since nothing installed changes |
| `[resources]` | `ram_min_mb` (install refused below), `ram_recommended_mb` (warning below) |
| `[[ports]]` | `name`, `env` (host-port variable), `service`, `container`, `default`, `protocol` (`tcp`/`udp`), `purpose`, `relocatable` (default true; false for discovery and protocol ports and with `host_network`), `proxied` (HTTP port fronted by the web access proxy) |
| `[web]` | see 6.3 |
| `[[folders]]` | app-owned bind folders: `env`, `path` (relative; default location `./<path>` in the stack folder, so the stack snapshot covers it), `intent` (`config`, `data`, `db`, `media`, `cache`), `ssd` (`no`, `ok`, `preferred`), `owner` (`uid:gid` when the image needs it, default PUID:PGID), `description` |
| `[[shares]]` | user shared folders: `env`, `label`, `description`, `access` (`ro`/`rw`, must match `:ro` in compose), `required`, `fallback` (stack folder used when an optional share is not chosen) |
| `[[secrets]]` | `env`, `length` (16-256), `encoding` (`alnum` characters, or `hex` / `base64` of `length` random bytes), `description`, `show` (the install answer shows the value once: an initial administrator password); generated once, kept on update |
| `[hardware]` | `gpu` (`none`, `optional`, `required`), `gpu_kinds` (`intel-qsv`, `vaapi`, `nvidia`), `cpu_features` (`x86-64-v2`, `avx`, `avx2`) |
| `[security]` | `privileged`, `docker_socket` (services allowed), `cap_add` (capabilities allowed), `devices` (services allowed to map host devices that are not a GPU, such as a USB Zigbee adapter; GPU devices only need `hardware.gpu`) |
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
- `privileged`, `cap_add`, `devices` (`hardware.gpu` or `security.devices`) and
  `network_mode: host` only as declared in `app.toml`.

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

Wave A3 (2026-11-07), 25 entries in all: Nginx Proxy Manager (`reverse_proxy`), Pocket ID,
cloudflared, AdGuard Home, Navidrome, Audiobookshelf, Stirling-PDF, Actual Budget, Mosquitto,
Zigbee2MQTT (USB adapter through `security.devices`), Frigate, Forgejo, Mealie; `featured` on
Immich, Jellyfin, Nextcloud, Vaultwarden, Home Assistant, Paperless-ngx and Dockhand. Picks,
skips, digests and sources in `../research/app-catalog-format.md` § 4.

## 7. Open points between waves

- **Loopback binding before the web access exists (A2 → W1).** With `RYNAS_BIND=127.0.0.1` an app is unreachable until the proxy (W1) runs. Rule:
  - while web access is not active, the install wizard defaults to `publish_lan` and says the app is reachable on `http://<nas>:<port>`;
  - once W1 is active, the default becomes loopback plus a route, and the "Expose" step replaces the port.

  Already-installed apps get a one-click "Move behind the proxy", which re-renders with loopback and adds a route.

  Done in W1 (§5.3): `publish_lan` is optional and follows this rule, and "Move behind the proxy" is `POST /api/v1/apps/installed/{stack}/proxy`.
- **Install without `docker compose config` first:** the validation only fails at `up`. Add a validation step that is read-only and safe under `--dry-system`.
- **Per-install volume choice:** apps always go on the Docker volume today. Per-folder placement exists, for example a database on SSD.
- **Deploy on the test NAS (2026-11-07): SMB announced twice — fixed in W2.** avahi logged `Failed to add service 'synodeb' of type '_device-info._tcp' … (rynas-smb.service): Local name collision`: Samba still announced itself as `SYNODEB` with `model=MacSamba` although `smb.conf` had `multicast dns register = no`. Cause: smbd reads that parameter only when it starts (`source3/smbd/server.c`, samba-4.22.0: `avahi_start_register()` runs once inside `if (lp_multicast_dns_register() && dns_port != 0)`, and it registers `_smb._tcp` and `_device-info._tcp` with `model=` from `fruit:model`, default `MacSamba`); rynas only reloaded the smbd that was started under the previous configuration. Fix (`crates/rynas-services`):
  - the shares apply restarts smbd once when the running smbd started before rynas's configuration turned the announcement off (marker `/var/lib/rynas/smbd-mdns-off` with that time, compared with systemd's `ActiveEnterTimestamp`), but only while no SMB client is connected (`smbstatus -b -j`): with sessions it reloads, logs "smbd restart deferred: N clients connected" and a later apply restarts it; later applies reload as before, and a boot never restarts it;
  - `fruit:model` and the `_device-info` record of `rynas-smb.service` carry the same model (`RackMac`), so macOS shows one icon whichever it reads.
  Single announcement: rynas's service file. Sources: https://github.com/samba-team/samba/blob/samba-4.22.0/source3/smbd/avahi_register.c, https://github.com/samba-team/samba/blob/master/docs-xml/smbdotconf/base/multicastdnsregister.xml.
- **Client side:** a Linux client whose resolver has no mDNS (systemd-resolved with `MulticastDNS=no`, no nss-mdns) does not resolve `*.local` names, even though avahi answers. The web access diagnostics should say so, and offer the domain path (W2) as the reliable option.
