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

- **Dockhand `-baseline` tags: resolved in wave A3 (2026-11-07), no overlay.** They are
  Alpine/musl builds, amd64 only, for CPUs **without x86-64-v2** (no SSE4.2/POPCNT), not for
  CPUs without AVX2. Evidence:
  - `Dockerfile.baseline`: *"For older x86_64 hardware without AVX2/SSE4.2 (TrueNAS, older
    Intel Atom/Celeron) … The Wolfi/glibc build crashes with SIGILL on CPUs that don't support
    the microarchitecture level Wolfi packages are compiled for."*
    (https://github.com/Finsys/dockhand/blob/main/Dockerfile.baseline);
  - `src/lib/server/backups/restic.ts`: *"The baseline Dockhand image (old x86_64 CPUs without
    x86-64-v2) sets DOCKHAND_VARIANT=baseline"*, and `updater-image-core.ts`: *"The default
    updater is Wolfi-based (compiled for x86-64-v2)"* (https://github.com/Finsys/dockhand);
  - history: issue #47 (Celeron J4105, exit 132, when the image ran on Bun built for AVX2),
    changelog 1.0.6 "Legacy CPU support (Celeron, Atom)" and 1.0.7 "Baseline Docker images for
    CPUs without AVX support"; since the move to Node.js (copied from `node:24-slim`, "conservative
    CPU baseline") only the Wolfi base sets the floor, at x86-64-v2. Later reports needing
    baseline are pre-v2 CPUs (AMD C-70, Phenom II, Turion N54L in #47, Atom D2700 in #129).

  The J4125 (Goldmont Plus) has x86-64-v2, so it runs the normal image. `RYNAS_CPU_AVX2` is the
  wrong signal, and on arm64 (no AVX2) a `!RYNAS_CPU_AVX2` overlay would even select an
  amd64-only image. The entry now declares `hardware.cpu_features = ["x86-64-v2"]` (package
  1.0.1). If pre-v2 machines ever matter, the clean way is a provided `RYNAS_CPU_X86_64_V2`
  (true on arm64) and an overlay on its negation.
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

## 4. Wave A3: thirteen more entries (2026-11-07)

Goal: the ~25-app catalogue of `reverse-proxy-app-store.md` ("Initial curated catalog", sized
for 4 GB of RAM), one or more apps per area. For every entry: the official upstream compose
and docs of today, the registry manifest, and how TrueNAS (`truenas/apps`, `ix-dev/community`),
Umbrel (`umbrel-apps`) and Runtipi (`runtipi-appstore`) package it.

### Picks and skips

| Area | Added | Skipped, and why |
|---|---|---|
| Access | Nginx Proxy Manager, Pocket ID, cloudflared | **Traefik**: configured by files and Docker labels, needs the Docker socket and has no GUI; the external-proxy case is covered by NPM, and rynas does no label discovery (D-PROXY). **Authelia**: needs a hand-written `configuration.yml` and users file, which an entry cannot ship; rynas SSO already covers forward auth, Pocket ID covers OIDC |
| Media | Navidrome, Audiobookshelf | **Plex**: non-free, needs a claim token and an account; Jellyfin covers the area |
| Documents | Stirling-PDF | **Collabora Online**: only useful wired into Nextcloud (WOPI, domain allow-list), ~2 GB; better as a Nextcloud option later |
| Home | Mosquitto, Zigbee2MQTT, Frigate | — |
| Network | AdGuard Home | — |
| Utilities | Forgejo, Mealie, Actual Budget | **qBittorrent**: fine technically, left for the next wave to stay at 25 |
| AI | — | **Open WebUI + Ollama**: CPU-only inference on a 4 GB NAS is not usable (models of 2-8 GB; a GPU in practice) |

### Images pinned

All digests were read on 2026-11-07 from the registries' anonymous manifest API
(`GET /v2/<repo>/manifests/<tag>`, `Docker-Content-Digest` of the image index; Docker Hub,
ghcr.io and codeberg.org), read-only. **All verified; none left for CI to fill.** Every index
lists `linux/amd64` and `linux/arm64`.

| Entry | Image | Upstream reference |
|---|---|---|
| Nginx Proxy Manager | `jc21/nginx-proxy-manager:2.16.0@sha256:4393e642…7f9594` | https://nginxproxymanager.com/setup/ · release v2.16.0 (2026-09-24) |
| Pocket ID | `ghcr.io/pocket-id/pocket-id:v2.19.2@sha256:3ee23895…7695eb` | https://github.com/pocket-id/pocket-id/blob/main/docker-compose.yml · v2.19.2 (2026-11-02) |
| cloudflared | `cloudflare/cloudflared:2026.10.7@sha256:4955524a…b9c5ba` | https://github.com/cloudflare/cloudflared/blob/master/Dockerfile |
| AdGuard Home | `adguard/adguardhome:v0.107.80@sha256:e2e1b48c…93027c` | https://github.com/AdguardTeam/AdGuardHome/wiki/Docker · v0.107.80 (2026-11-03) |
| Navidrome | `deluan/navidrome:0.64.2@sha256:38dc2727…a44ee52` | https://www.navidrome.org/docs/installation/docker/ · v0.64.2 (2026-09-24) |
| Audiobookshelf | `ghcr.io/advplyr/audiobookshelf:2.37.2@sha256:b897c33f…a7658efa` | https://github.com/advplyr/audiobookshelf/blob/master/docker-compose.yml · v2.37.2 (2026-10-23) |
| Stirling-PDF | `stirlingtools/stirling-pdf:3.3.0@sha256:ed7b1775…036025e6` | https://docs.stirlingpdf.com/Server/Install/Docker/ · v3.3.0 (2026-11-05) |
| Actual Budget | `actualbudget/actual-server:26.11.0@sha256:06cf1865…1d398da6` | https://github.com/actualbudget/actual/blob/master/packages/sync-server/docker-compose.yml · v26.11.0 (2026-11-02) |
| Mosquitto | `eclipse-mosquitto:2.1.2-alpine@sha256:75d6662d…8bbc98` | https://hub.docker.com/_/eclipse-mosquitto · https://github.com/eclipse-mosquitto/mosquitto/blob/master/ChangeLog.txt |
| Zigbee2MQTT | `ghcr.io/koenkk/zigbee2mqtt:2.14.3@sha256:fb4a6b0f…908f43a` | https://www.zigbee2mqtt.io/guide/installation/02_docker.html · 2.14.3 (2026-11-01) |
| Frigate | `ghcr.io/blakeblackshear/frigate:0.18.2@sha256:fd60f99f…ffac09a` | https://docs.frigate.video/frigate/installation · 0.18.2 (2026-10-15) |
| Forgejo | `codeberg.org/forgejo/forgejo:15.0.11-rootless@sha256:3cd6bc2b…2df9f40c` | https://forgejo.org/docs/latest/admin/installation/docker/ · LTS 15.0 (EOL 2027-07-15) |
| Mealie | `ghcr.io/mealie-recipes/mealie:v3.30.0@sha256:90f83da6…63d784` | https://docs.mealie.io/documentation/getting-started/installation/sqlite/ · v3.30.0 (2026-10-31) |

Full digests are in each `apps/<id>/compose.yaml`.

### Format changes (backward compatible: optional, `false`/empty by default)

- **`reverse_proxy`**: the clean way to say "needs ports 80/443, works only with web access in
  external proxy mode". The validator requires tcp ports with defaults 80 and 443,
  `relocatable = false`, not proxied, on a bridged network, and refuses defaults 80/443 tcp in
  every other bridged entry (they belong to the built-in proxy). Reverse proxies are exempt
  from the catalogue-wide unique-default rule for those two ports. Enforcement at install
  needs no new code: rynasd already counts the proxy's ports as used unless external mode, so
  the port check refuses the fixed 80/443. `AppCard.reverse_proxy` lets the UI say so upfront.
- **`featured`**: the curated Featured pick (Immich, Jellyfin, Nextcloud, Vaultwarden, Home
  Assistant, Paperless-ngx, Dockhand), exposed as `AppCard.featured`. Presentation only: setting
  it does not bump the package version.
- **`security.devices`**: services allowed to map host devices that are not a GPU (a USB
  Zigbee coordinator). Before, any `devices:` needed `hardware.gpu`, which would have forced a
  false GPU claim on Zigbee2MQTT.

### Per-entry notes

- **Nginx Proxy Manager.** Ports 80/443 (LAN, fixed) and admin 81 (proxied: with web access in
  external mode the install publishes it on the LAN). Since 2.13 there is no default
  `admin@example.com/changeme` account; `INITIAL_ADMIN_EMAIL/PASSWORD` create the first user
  (form e-mail + generated password shown once; TrueNAS's notes still advertise the old login).
  Runs as root like upstream: with `PUID/PGID` binding port 80 "may" fail (docs). The healthcheck
  is opt-in upstream (`/usr/bin/check-health`), switched on as TrueNAS does;
  `host.docker.internal` points at the host gateway (TrueNAS) so proxy hosts reach apps on the
  NAS. Umbrel maps 40080/40443 instead, which defeats Let's Encrypt HTTP-01 without router
  changes. Sources:
  https://github.com/NginxProxyManager/nginx-proxy-manager/blob/develop/docs/src/setup/index.md ·
  https://github.com/NginxProxyManager/nginx-proxy-manager/releases/tag/v2.13.0 ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/nginx-proxy-manager ·
  https://github.com/getumbrel/umbrel-apps/blob/master/nginx-proxy-manager/docker-compose.yml
- **Pocket ID.** `ENCRYPTION_KEY` is the only required variable (≥ 16 bytes; generated as 32
  bytes base64, like `openssl rand -base64 32`); `APP_URL` without a path (no sub-path; passkeys
  are bound to the domain and need HTTPS → `requires-https`); `TRUST_PROXY` takes a list of
  CIDRs (`trusted_proxies_env`); PUID/PGID handled by the entrypoint; image healthcheck.
  **Telemetry on by default** (daily heartbeat) → `ANALYTICS_DISABLED=true`. SQLite WAL
  connection string from TrueNAS. Never put SSO in front: it is the identity provider. Sources:
  https://github.com/pocket-id/pocket-id/blob/main/.env.example ·
  https://github.com/pocket-id/website/blob/main/src/content/docs/docs/configuration/environment-variables.md ·
  https://github.com/pocket-id/website/blob/main/src/content/docs/docs/configuration/analytics.md ·
  https://github.com/pocket-id/website/blob/main/src/content/docs/docs/setup/reverse-proxy.mdx ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/pocket-id
- **cloudflared.** Remotely managed tunnel: `tunnel --no-autoupdate run` with `TUNNEL_TOKEN`
  (TrueNAS). Distroless image (no shell, uid 65532): metrics pinned to `127.0.0.1:20241`, and
  the healthcheck is `cloudflared tunnel --metrics … ready` (since 2024.11.1). Host network so
  the tunnel reaches the web access proxy and loopback-bound apps; no ports, no web block.
  Umbrel and Runtipi wrap it in third-party web UIs; not taken. Traffic is decrypted at
  Cloudflare (wizard warning). Sources: https://github.com/cloudflare/cloudflared/blob/master/Dockerfile ·
  https://github.com/cloudflare/cloudflared/blob/master/cmd/cloudflared/tunnel/subcommands.go ·
  https://github.com/cloudflare/cloudflared/blob/master/metrics/metrics.go ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/cloudflared
- **AdGuard Home.** 53 tcp/udp fixed on the LAN (bridged; DHCP would need host networking and
  is not offered); `--web-addr 0.0.0.0:3000` keeps the interface on 3000 after the wizard
  (TrueNAS and Umbrel do the same; Runtipi seeds a config file). Port 53 against the
  systemd-resolved stub: the port check refuses the install and the wizard note gives upstream's
  FAQ fix (`DNSStubListener=no`). Runs as root; upstream removed the image healthcheck in
  v0.107.34 "because it caused many issues". Sub-path documented for nginx and Caddy. Sources:
  https://github.com/AdguardTeam/AdGuardHome/wiki/Docker · https://adguard-dns.io/kb/adguard-home/faq/ ·
  https://adguard-dns.io/kb/adguard-home/configuration/ ·
  https://github.com/AdguardTeam/AdGuardHome/blob/master/docker/build.Dockerfile
- **Navidrome.** `user:` (the image ignores PUID/PGID, which Umbrel sets to no effect); `/data`,
  `/music:ro`; **`ND_ENABLEINSIGHTSCOLLECTOR` defaults to true** → off; hourly scan (Runtipi
  still uses the old `ND_SCANSCHEDULE` name); healthcheck `/ping` (TrueNAS). Subsonic clients
  (`/rest`) and shares (`/share`) bypass SSO; external auth (`ND_EXTAUTH_TRUSTEDSOURCES`, the
  new name of `ReverseProxyWhitelist`) left off. Sources:
  https://www.navidrome.org/docs/installation/docker/ · https://www.navidrome.org/docs/usage/configuration/options/ ·
  https://www.navidrome.org/docs/usage/integration/authentication/ ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/navidrome
- **Audiobookshelf.** Upstream's commented `user:` line (Umbrel uses it), with `PORT=13378`
  so the non-root process does not bind 80; `init: true` (Umbrel); healthcheck `/healthcheck`
  (TrueNAS). WebSocket required (socket.io); sub-path only as `/audiobookshelf`, so its own
  host name; large uploads (10G, community nginx examples). The SSO bypass list (`/api`,
  `/socket.io`, `/auth`, `/login`, `/public`, `/feed`, `/hls`) is inferred from the app's
  routes, not documented upstream. Sources:
  https://github.com/advplyr/audiobookshelf/blob/master/docker-compose.yml ·
  https://github.com/advplyr/audiobookshelf#reverse-proxy-set-up ·
  https://github.com/advplyr/audiobookshelf/blob/master/index.js ·
  https://github.com/getumbrel/umbrel-apps/blob/master/audiobookshelf/docker-compose.yml
- **Stirling-PDF.** Standard image (ultra-lite drops OCR and LibreOffice; fat suggests 6 GB).
  Upstream suggests 4 GB for standard → `high-resources`. **`non-free`**: `app/proprietary/`,
  `engine/` and parts of the frontend are under the "Stirling PDF User License". PUID/PGID are
  handled by the image (no `user:`; Umbrel: "does not work with a custom user"). Login on by
  default since v2 (`admin`/`stirling`, wizard warning); `DOCKER_ENABLE_SECURITY` is a legacy v1
  switch. Analytics are asked at first launch → `SYSTEM_ENABLEANALYTICS=false` (plus PostHog
  and Scarf). The tessdata folder is not mounted (Runtipi mounts the old path and breaks extra
  languages; an empty bind would hide the shipped ones). Limits 2000 MB / 20 min. Sources:
  https://docs.stirlingpdf.com/Server/Install/Docker/ ·
  https://github.com/Stirling-Tools/Stirling-PDF/blob/main/LICENSE ·
  https://github.com/Stirling-Tools/Stirling-PDF/blob/main/app/core/src/main/resources/settings.yml.template ·
  https://github.com/Stirling-Tools/Stirling-PDF/blob/main/app/core/src/main/resources/application.properties
- **Actual Budget.** HTTPS required (SharedArrayBuffer needs a secure context) →
  `requires-https`; the server sends COOP/COEP itself (the proxy must not duplicate them).
  `user:` works (Umbrel); `ACTUAL_TRUSTED_PROXIES`; upload limits raised from 20/20/50 MB to
  100; healthcheck from upstream's compose (`node scripts/health-check.js`). No documented
  sub-path; a login in front breaks sync, so no SSO bypass list (TrueNAS's forum lists
  SharedArrayBuffer errors as the common failure). Sources: https://actualbudget.org/docs/config/ ·
  https://actualbudget.org/docs/config/https · https://actualbudget.org/docs/config/reverse-proxies/ ·
  https://actualbudget.org/docs/troubleshooting/shared-array-buffer
- **Mosquitto.** 2.1 ships only `-alpine` tags (the `2` tag still points at 2.0.x). The default
  config listens on localhost only and an entry ships no config file, so the command writes
  `mosquitto.conf` (one listener, `allow_anonymous false`, persistence, log to stdout) and runs
  `mosquitto_passwd -b -c` from the environment at every start, under `umask 077` (2.0.16+ warns
  about world-readable password files; `password_file` is deprecated for 3.0 but works). TrueNAS
  renders the same settings through `configs:`, Umbrel in a pre-start hook. Healthcheck
  `pidof mosquitto` (TrueNAS). Port 1883 only (websockets on 9001 are not needed by the
  catalogue's clients). Sources: https://github.com/eclipse-mosquitto/mosquitto/blob/master/ChangeLog.txt ·
  https://github.com/eclipse-mosquitto/mosquitto/tree/master/docker/2.1-alpine ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/eclipse-mosquitto
- **Zigbee2MQTT.** Rootless variant (`user:` + `group_add: dialout`). Without
  `configuration.yaml` the image serves its onboarding page on 8080, where the broker and the
  adapter are chosen (`ZIGBEE2MQTT_CONFIG_*` variables do not skip it). USB coordinator through
  an overlay (stable `/dev/serial/by-id/…` path → `/dev/zigbee`; TrueNAS also requires by-id or
  `tcp://`); network coordinators need none. Frontend protected by a generated `auth_token`
  (basic auth breaks the WebSocket on WebKit). Umbrel runs it `privileged`: not taken.
  Healthcheck on the web port (TrueNAS). Sources:
  https://www.zigbee2mqtt.io/guide/installation/02_docker.html ·
  https://www.zigbee2mqtt.io/guide/getting-started/ ·
  https://www.zigbee2mqtt.io/guide/configuration/frontend.html ·
  https://github.com/Koenkk/zigbee2mqtt/blob/master/lib/util/onboarding.ts ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/zigbee2mqtt
- **Frigate** (D-NVR). **The AVX question, answered:** the docs now say *"Frigate requires a
  CPU with AVX + AVX2 instructions… Intel Celeron and Pentium models prior to the 2020 Tiger
  Lake generation typically lack AVX"*. On a J4125 (no AVX) the container starts and detection
  and recording work. TensorFlow-based features crash with SIGILL (object/state classification,
  semantic search through the embeddings maintainer); face recognition and LPR use runtimes
  that fall back but are unsupported. OpenVINO on the J4125 iGPU works (a DS920+ user reports
  ~21 ms for YOLOv9-t 320), and VA-API there needs `LIBVA_DRIVER_NAME=i965` in the container
  environment. → `cpu_features = ["avx", "avx2"]` with a wizard warning, not a refusal.
  Compose: not privileged (upstream: "may not be necessary"), `shm_size` 512 MB, tmpfs
  `/tmp/cache` 1 GB, runs as root (no PUID support upstream), only 8971 (authenticated), 8554
  and 8555 tcp+udp published, never 5000. 0.18 writes a default config itself, but with TLS on
  8971 (self-signed); the entrypoint seeds `config.yml` once with TLS and MQTT off (and, with
  the GPU overlay, VA-API decoding and the OpenVINO GPU detector from upstream's documented
  example), then runs `exec /init`. The generated admin password is in the container log. The
  recordings folder is separate, with the D-NVR advice (dedicated volume, no CoW or snapshots)
  as a wizard warning. RAM: 4 GB basic, 8 GB with enrichments. Sources:
  https://docs.frigate.video/frigate/planning_setup · https://docs.frigate.video/frigate/installation ·
  https://docs.frigate.video/configuration/authentication · https://docs.frigate.video/configuration/tls ·
  https://docs.frigate.video/configuration/object_detectors · https://docs.frigate.video/guides/reverse_proxy ·
  https://github.com/blakeblackshear/frigate/discussions/22381 ·
  https://github.com/blakeblackshear/frigate/discussions/22173 ·
  https://github.com/blakeblackshear/frigate/discussions/22708 ·
  https://github.com/blakeblackshear/frigate/discussions/24518 ·
  https://github.com/blakeblackshear/frigate/discussions/24497 ·
  https://github.com/blakeblackshear/frigate/discussions/22198 ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/frigate
- **Forgejo.** LTS 15.0 rather than 17 (supported until 2027-07-15; 11 LTS ended 2026-07-16;
  major upgrades need a manual check). Rootless image (TrueNAS and Umbrel), `user:`, one folder
  (`/var/lib/gitea`; since v15 there is no `/etc/gitea`, which Umbrel still mounts), SQLite (all
  three catalogues add Postgres; not needed for a household). `FORGEJO__section__KEY`
  overrides; `INSTALL_LOCK=true` with generated `SECRET_KEY` and `INTERNAL_TOKEN` (Forgejo
  generates the base64url JWT secrets itself). With the install page locked the first
  registered user becomes administrator (wizard warning). `REVERSE_PROXY_TRUSTED_PROXIES` comes
  from rynas (not through `trusted_proxies_env`: the name is not env-shaped). Built-in SSH on
  2222, with the host port in clone URLs. Healthcheck `/api/healthz` (TrueNAS). Sources:
  https://forgejo.org/docs/next/admin/release-schedule/ · https://forgejo.org/docs/latest/admin/installation/docker/ ·
  https://forgejo.org/docs/latest/admin/setup/reverse-proxy/ ·
  https://codeberg.org/forgejo/forgejo/raw/tag/v17.0.1/Dockerfile.rootless ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/forgejo ·
  https://github.com/getumbrel/umbrel-apps/blob/master/forgejo/docker-compose.yml
- **Mealie.** SQLite (1-20 users on local disk), PUID/PGID through the entrypoint (with `user:`,
  `/nltk_data` must be writable, issue #5242), one uvicorn worker, sign-up off by default, image
  healthcheck. No sub-path (FAQ). **Default administrator `changeme@example.com` /
  `MyPassword`** until the first login runs the setup wizard → wizard warning. AI providers are
  configured in the UI now (no environment variables). Sources:
  https://github.com/mealie-recipes/mealie/blob/mealie-next/docs/docs/documentation/getting-started/installation/sqlite.md ·
  https://github.com/mealie-recipes/mealie/blob/mealie-next/docs/docs/documentation/getting-started/installation/backend-config.md ·
  https://github.com/mealie-recipes/mealie/blob/mealie-next/docs/docs/documentation/getting-started/faq.md ·
  https://github.com/truenas/apps/tree/master/ix-dev/community/mealie ·
  https://github.com/runtipi/runtipi-appstore/tree/master/apps/mealie-1

### Open points

- **Frigate proxy-header SSO** (D-NVR: `proxy.header_map`, `X-Proxy-Secret`) needs the web
  access module to send identity headers on one route; until then Frigate uses its own login.
- **Frigate recordings volume**: the installer should offer (or require) a dedicated non-CoW
  volume for `RECORDINGS_FOLDER` instead of a warning.
- **Not verified at run time**: the entries were validated by `cargo test` and
  `docker compose config` only. The healthchecks assume the tools named by upstream or TrueNAS
  (`curl` in the Forgejo rootless image, busybox `wget` in Navidrome, Audiobookshelf and
  Zigbee2MQTT). The first `up` on a test machine should confirm them.
