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
| A1 | `rynas-apps` format + loader + renderer + CI lint, and the first curated entries in `apps/`: the managers (Arcane, Dockhand, Portainer CE, Dockge), Immich, Jellyfin, Vaultwarden, Nextcloud, Syncthing, Home Assistant, Uptime Kuma, Paperless-ngx | in progress |
| D2 | UI: Docker page, dashboard widget, notification texts | after D1 |
| A2 | Apps & Plugins page: install wizard (form from `form.json`), installed apps, updates with snapshot and rollback, convert to custom stack; rynasd catalogue endpoints | after D1 + A1 |
| W0 | rynas-web on `:8443` with rustls, the emergency door that never goes through the proxy (still owed from phase 1) | before W1 |
| W1 | Web access (D-PROXY/D-EXPOSE revised on 2026-11-07, `../research/web-access.md`): `rynas-caddy` package, rynasd renderer + `POST /load` + last-good, local names + local CA, app routes, the wizard's Access step, diagnostics | with A2 |
| W2 | Domains, DNS-01 wildcard, DNS records managed by rynas, certificate dashboard | after W1 |
| R | Quality review: GUI/API consistency, speed, memory, no inner horizontal scroll; then a deploy on the DS920+ with Docker installed through the module | after D2 + A2 |

## 5. API

Each wave adds its own section here: routes, request and response shapes, error codes, tasks. It follows the conventions of `../phase1/plan.md` (§ Error codes, configuration pending/apply, tasks with UPIDs).
