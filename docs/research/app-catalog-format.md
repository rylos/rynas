# App catalogue format and first entries: research (wave A1, 2026-11-07)

Input for `docs/phase2/docker-apps.md` § 6. The earlier survey of app stores is topic 2 of
`reverse-proxy-app-store.md`; this note looks at the current file formats in detail, then
records, for every first entry, what upstream recommends today and which image was pinned.

## 1. Catalogue formats today

| System | Metadata | Form | Template | Secrets / provided values | Migrations | Pinning |
|---|---|---|---|---|---|---|
| **TrueNAS apps** (`truenas/apps`, `ix-dev/<train>/<app>/`) | `app.yaml`: `name`, `train`, `version` (package semver, bumped on every change), `app_version` (upstream, kept in sync with the main image tag), `lib_version`, `run_as_context`, `host_mounts`, `changelog_url`, `categories`, `maintainers` | `questions.yaml`: groups + questions with `schema` (`string`, `int`, `boolean`, `dict`, `list`, `uri`, `path`, `hostpath`), `required`, `default`, `min`/`max`, `enum`, `show_if`, `private`, `hidden` | `templates/docker-compose.yaml` is Jinja2 driving a versioned Python library (`ix_lib`), ends in `tpl.render() \| tojson` | `ix_values.yaml` (`images`, `consts`) merged with answers | `app_migrations.yaml` maps scripts in `migrations/` to version ranges; each script reads a values file and prints the new values | Renovate; default ports unique across the catalogue (`port_validation.py`) |
| **Runtipi** | `config.json`: `id`, `name`, `port`, `tipi_version` (package counter), `version` (upstream), `categories`, `supported_architectures`, `min_tipi_version`, `deprecated`, `exposable`, `url_suffix`, `https`, `uid`/`gid` | `form_fields`: `text`, `password`, `email`, `number`, `fqdn`, `ip`, `fqdnip`, `random`, `boolean`; `env_variable`, `required`, `default`, `min`/`max`, `regex` + `pattern_error`, `options`, `hint`, `placeholder` | `docker-compose.yml` with `x-runtipi` (dynamic compose) | `random` fields with `encoding` base64/hex | — | — |
| **Umbrel** (`umbrel-apps`) | `umbrel-app.yml`: `manifestVersion`, `id` (= folder, never changes), `version` (upstream), `port` (unique in the store), `path`, `requiresHttps`, `dependencies`, `permissions` (`GPU`, `STORAGE_DOWNLOADS`), `storage.dataRoot`, `defaultUsername/Password`, `deterministicPassword` | none (fixed apps) | plain compose + an `app_proxy` service (`APP_HOST`, `APP_PORT`, `PROXY_AUTH_WHITELIST/BLACKLIST`), `exports.sh` | `APP_DATA_DIR`, `APP_SEED` / `derive_entropy` (stable per-install secrets), `DEVICE_DOMAIN_NAME`, `APP_PASSWORD` | — | **every image `repo:tag@sha256:<index digest>`, multi-arch checked, no `latest`, no `build:`** |
| **CasaOS / ZimaOS** (v2 store) | top-level `x-casaos` in the compose file: `id` (reverse domain), `main` (service), `index`, `port_map`, `scheme`, `icon`, locale-keyed `title`/`tagline`/`description`/`tips`, `category` (9 fixed values), `architectures`, `version` | none (service-level `x-casaos` env/port descriptions are legacy, dropped at build) | the compose file itself | `$AppID`, `$PUID`, `$PGID`, `$TZ` (v1 docs) | — | — |
| **YunoHost** | `manifest.toml` v2 with declarative **resources** (`system_user`, `install_dir`, `data_dir`, `ports` with `exposed`/`fixed`, `database`, `apt`, `permissions`) provisioned, upgraded and removed by the core; **antifeatures**; quality **levels 0-9 computed by CI** | `install` questions in the manifest | shell scripts (native apps) | resource-provided values | upgrade scripts | — |

Sources:
- TrueNAS: https://github.com/truenas/apps/blob/master/CONTRIBUTIONS.md
- Runtipi: https://runtipi.io/docs/reference/config-json
- Umbrel: https://github.com/getumbrel/umbrel-apps/blob/master/.agents/skills/umbrel-package-app/SKILL.md · https://github.com/getumbrel/umbrel-apps/blob/master/README.md
- CasaOS: https://github.com/IceWhaleTech/CasaOS-AppStore/blob/main/docs/specs/compose-and-x-casaos.md · https://github.com/IceWhaleTech/CasaOS-AppStore/blob/main/docs/specs/overview.md
- YunoHost: https://doc.yunohost.org/en/packaging_apps_resources · https://github.com/yunohost/apps
- JSON Schema keywords: https://json-schema.org/draft/2020-12/json-schema-validation
- RFC 7396 (JSON Merge Patch): https://www.rfc-editor.org/rfc/rfc7396

### What rynas takes, and why

- **Two versions** (TrueNAS): `version` is the package, bumped on every change of the entry and
  driving migrations; `app_version` is upstream and must match the main image tag (lint).
- **Schema separate from the template** (TrueNAS, Runtipi): but as a JSON Schema subset, so
  one generic Leptos form renders it and plugins reuse the same renderer. Only the keywords a
  form needs (`type`, `title`, `description`, `default`, `enum`, `minimum`/`maximum`,
  `minLength`/`maxLength`, `pattern`, `format`) plus `x-ui` hints and `x-groups` for order.
  Property names are the `.env` names (Runtipi's `env_variable`, without the indirection).
- **No template engine** (lesson of TrueNAS's Jinja + library and TrueCharts): the compose file
  is used as is; `docker compose` itself interpolates `${VAR}` from `.env`. Optional parts that
  TrueNAS would template (GPU devices, dropping a service) are **overlays**: plain compose
  fragments merged with RFC 7396 semantics when a boolean answer is true. Immich does the same
  upstream with its `hwaccel.*.yml` files and `extends`.
- **Provided values** (Umbrel, CasaOS): `RYNAS_APPDATA`, `RYNAS_STACKS_DIR`, `RYNAS_HOST`, `PUID`,
  `PGID`, `TZ`, `DOCKER_GID`, `RENDER_GID`, `RYNAS_CPU_AVX2`, `RYNAS_BIND`, `RYNAS_TRUSTED_PROXIES`.
- **Generated secrets** (Runtipi `random`, Umbrel `APP_SEED`): declared in `app.toml`, generated
  once from system randomness, stored in `.env` and kept by every update. Not derived from a
  seed: a restored stack keeps its `.env`, and a seed would tie stacks to one machine.
- **Pinning by tag and digest** (Umbrel's rule, the strictest): lint refuses moving tags and
  images without digest; the digest is the multi-arch index digest.
- **Declarative resources** (YunoHost): folders with an intent (`config`, `data`, `db`,
  `media`, `cache`) and an SSD hint, user shares with read-only/read-write access, ports with a
  relocatable flag (YunoHost's `fixed`), named volumes only for disposable caches.
- **Unique default ports across the catalogue** (TrueNAS, Umbrel): enforced by the loader for
  bridged apps, so two catalogue apps never collide on defaults.
- **Antifeatures** (YunoHost): `non-free`, `high-resources`, `needs-gpu`, `telemetry`,
  `unmaintained`, `requires-https`, `root-access`. Lint ties some of them to facts
  (`root-access` ⇔ manager/privileged/socket, `high-resources` ⇔ recommended RAM > 2 GB).
- **Not taken**: CasaOS's metadata inside the compose file (rynas keeps the compose file
  standard and the metadata beside it; only the small `x-rynas` block is added at install, so
  "Convert to custom stack" just removes it); TrueNAS's executable migration scripts
  (rynas migrations are declarative value operations: rename, set, set_default, delete, map).

### YAML library

`serde_yaml` is archived. `yaml_serde` (YAML organisation, 0.10.7, Aug 2026) is the maintained
fork with the same `Value`/`Mapping` API and a pure-Rust `libyaml-rs` backend.
Sources: https://lib.rs/crates/yaml_serde · https://crates.io/api/v1/crates/yaml_serde ·
https://github.com/rubyatscale/pks/pull/46 (comparison of the forks).

## 2. The first entries

All digests below were read on 2026-11-07 from the registries' anonymous manifest API
(`GET /v2/<repo>/manifests/<tag>`, `Docker-Content-Digest` of the image index), read-only,
and cross-checked with the Docker Hub tag listing where available. **All of them are
verified; none is left for CI to fill.** Every index lists `linux/amd64` and `linux/arm64`.

| Entry | Image(s) pinned | Upstream reference |
|---|---|---|
| Arcane | `ghcr.io/getarcaneapp/manager:v2.17.3@sha256:2e0dc9f5…f882a7` | https://getarcane.app/docs/setup/installation · release v2.17.3 (2026-11-04) |
| Dockhand | `fnsys/dockhand:v1.0.57@sha256:4fd64d32…013988` | https://dockhand.pro/manual · release v1.0.57 (2026-11-04) |
| Portainer CE | `portainer/portainer-ce:2.45.3@sha256:3609c307…dd4e96e` (same digest as `lts`) | https://docs.portainer.io/start/install-ce/server/docker/linux |
| Dockge | `louislam/dockge:1.5.0@sha256:335c6368…502bf` | https://github.com/louislam/dockge/blob/1.5.0/compose.yaml |
| Immich | `ghcr.io/immich-app/immich-server:v3.3.6@sha256:e1981983…6c593a`, `immich-machine-learning:v3.3.6@sha256:14367bfb…50bc2`, `ghcr.io/immich-app/postgres:14-vectorchord0.4.3-pgvectors0.2.0@sha256:bcf63357…1c23` (upstream pin), `valkey/valkey:9.0.7@sha256:7e1deda0…59d6d` | https://github.com/immich-app/immich/releases/download/v3.3.6/docker-compose.yml · https://docs.immich.app/install/requirements · https://docs.immich.app/install/environment-variables |
| Jellyfin | `jellyfin/jellyfin:12.4@sha256:fea863d0…1b771` | https://jellyfin.org/docs/general/installation/container |
| Vaultwarden | `vaultwarden/server:1.37.5@sha256:4fe17f41…d9e1f` | https://github.com/dani-garcia/vaultwarden (README, wiki "Using Docker Compose") |
| Nextcloud | `nextcloud:34.0.5-apache@sha256:b89746a4…eec3f` (= `stable-apache`), `mariadb:11.8.9-noble@sha256:c4124d1a…22a9bd` (= `lts`), `redis:8.8.4-alpine@sha256:c027ae0f…36125` | https://github.com/nextcloud/docker#base-version---apache |
| Syncthing | `syncthing/syncthing:2.1.7@sha256:2b71da7c…018f7546` | https://github.com/syncthing/syncthing/blob/v2.1.7/README-Docker.md |
| Home Assistant | `ghcr.io/home-assistant/home-assistant:2026.11.0@sha256:c6c1ca99…2f69e` | https://www.home-assistant.io/installation/linux |
| Uptime Kuma | `louislam/uptime-kuma:2.5.9@sha256:e88415a9…591bd` | https://github.com/louislam/uptime-kuma/blob/2.5.9/compose.yaml |
| Paperless-ngx | `ghcr.io/paperless-ngx/paperless-ngx:3.4.2@sha256:d704e84f…84c84`, `postgres:18.6@sha256:e3d24f69…c42`, `valkey/valkey:9.0.7-alpine@sha256:2d04268d…a416` | https://github.com/paperless-ngx/paperless-ngx/blob/v3.4.2/docker/compose/docker-compose.postgres.yml |

Full digests are in each `apps/<id>/compose.yaml`.

### Per-entry notes

- **Managers** (Arcane, Dockhand, Portainer, Dockge). All four want the Docker socket and the
  stacks folder at the same absolute path inside and outside (`${RYNAS_STACKS_DIR}`), which
  the lint enforces for `manager = true`. Arcane: `PROJECTS_DIRECTORY`, `ENCRYPTION_KEY`
  (32 bytes, generated), `APP_URL`, `PUID/PGID`, `cgroup: host`, `arcane health` healthcheck;
  `JWT_SECRET` is no longer used. Dockhand: `STACKS_DIR`, `PUID/PGID` plus `group_add` of the
  socket's gid (`DOCKER_GID`); 512 MB minimum, 1 GB recommended; `ENCRYPTION_KEY` optional (key
  file in the data folder otherwise); BSL 1.1, flagged `non-free`. Portainer: 2.45.3 is what
  `lts` points to; ≥ 2.33.5 LTS is needed for Docker 29; Edge port 8000 and legacy HTTP 9000
  are not published; anonymous statistics are on by default (`telemetry`). Dockge: last release
  1.5.0 (March 2025), flagged `unmaintained`.
- **Immich.** Upstream RAM: 6 GB minimum, 8 GB recommended, 4 GB only with machine learning
  disabled. Entry gate: refuse below 4 GB, warn below 8 GB; a `ML_DISABLED` overlay removes the
  ML service. There is **no environment variable** to disable ML (only the admin setting or a
  config file, which would lock the settings page), so the wizard tells the user to switch it
  off in the admin settings. The ML image needs **x86-64-v2** since v3 (the J4125 has it).
  Database: local SSD advised, never a network share → `ssd = "preferred"`; with the database on
  HDD upstream suggests `DB_STORAGE_TYPE=HDD` (not set by the entry). `DB_PASSWORD` only
  `A-Za-z0-9` (alnum secret). `IMMICH_TRUSTED_PROXIES` exists (server). Hardware transcoding:
  the upstream `hwaccel.transcoding.yml` passes `/dev/dri` (quicksync/vaapi) → overlay.
  Proxy: upstream's nginx example uses `client_max_body_size 50000M` and 600 s timeouts.
- **Jellyfin.** Upstream compose: `user: uid:gid`, `/config`, `/cache`, media bind, 8096/tcp
  and 7359/udp (discovery); host networking only for DLNA (not offered). QSV/VA-API: `/dev/dri`
  plus the render group (overlay with `${RENDER_GID}`). The image has its own healthcheck.
- **Vaultwarden.** WebSocket notifications on the main port since 1.29; clients need HTTPS
  (`requires-https`); `DOMAIN` must be the public HTTPS URL; signups open by default with a
  warning (no SMTP needed to create the first account). The admin page stays disabled (no
  `ADMIN_TOKEN`; a plain token is discouraged upstream in favour of an Argon2 PHC string).
- **Nextcloud.** Official image + MariaDB (`lts` = 11.8.9, `--transaction-isolation=READ-COMMITTED`)
  + Redis, as in upstream's "Base version - apache"; AIO avoided (it manages its own
  containers and proxy). Automatic install through `NEXTCLOUD_ADMIN_USER/PASSWORD`,
  `NEXTCLOUD_TRUSTED_DOMAINS`, `TRUSTED_PROXIES`, `PHP_UPLOAD_LIMIT`. The data folder must be
  owned by www-data (33:33) → `owner = "33:33"` on the folder.
- **Syncthing.** Upstream strongly advises host networking for LAN discovery; the entry uses
  bridged ports (GUI behind the proxy, conflict checks) and documents adding the NAS address by
  hand if discovery fails. The GUI checks the `Host` header: the web access module must send
  `Host: localhost` upstream (or the user enables "insecure skip host check").
- **Home Assistant.** Upstream compose: `privileged: true`, `network_mode: host`,
  `/run/dbus:ro`, `stop_grace_period: 60s`. No RAM figure for the container edition (HAOS VM:
  2 GB). Trusted proxies live in `configuration.yaml` (`http.use_x_forwarded_for`,
  `http.trusted_proxies`), not in the environment → note in the wizard.
- **Uptime Kuma.** Upstream compose: `./data:/app/data`, 3001. The image has a healthcheck.
  "Trust proxy" is a UI setting.
- **Paperless-ngx.** Upstream postgres compose: Valkey broker, PostgreSQL 18 (data at
  `/var/lib/postgresql`, the PG 18 layout), `PAPERLESS_SECRET_KEY`, `USERMAP_UID/GID`;
  `PAPERLESS_ADMIN_USER/PASSWORD` creates the superuser; `PAPERLESS_URL` is needed for CSRF behind
  a proxy; the image ships eng/deu/fra/ita/spa OCR data.

## 3. Open points

- **Dockhand `-baseline` tags** (`fnsys/dockhand:v1.0.57-baseline`, amd64 only) exist next to
  the normal tags, but neither the manual nor the Dockerfile explains them (the image moved
  from Bun to Node.js). If they turn out to be builds for CPUs without AVX/AVX2 (low-power
  Celeron and Atom CPUs such as the J4125 have neither), add an overlay on `!RYNAS_CPU_AVX2`; the provided variable exists for that.
- **Immich on HDD**: whether to set `DB_STORAGE_TYPE=HDD` automatically when the database folder
  is not on an SSD (needs the engine to know the volume's rotational flag).
- **Syncthing host networking** as an option (would need host-network variants of the port
  declarations; the format keeps one networking mode per entry).
- **Secrets shown to the user**: the Nextcloud and Paperless admin passwords are form answers and
  stay in `.env` (mode 0600) for re-renders; a later wave could drop them from `.env` after the
  first start.
- **CI pipeline beyond `cargo test`**: `packaging/apps-render-test.sh` runs
  `docker compose config` on every render; the real `up` + healthcheck test and Renovate PRs
  for digests belong to the catalogue repository's CI.
