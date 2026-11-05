# Docker/container strategy for rynas: analyst report

## Short answer

The proposal holds up almost entirely. It needs four corrections:

1. **Engine and images.** With Docker 29, new installations store images and containers in `/var/lib/containerd`, not in the `data-root`. Moving both onto the NAS volume requires configuring two paths.
2. **Firewall.** Docker only supports `iptables-nft` or `iptables-legacy`. Rules written directly with `nft` are not supported. The rynas firewall plugin must use the `DOCKER-USER` chain.
3. **Startup order.** Docker must start only after the volume is mounted. This is a classic complaint on OMV and Unraid.
4. **App catalog.** Not in phase 2. Phase 3 only needs a catalog of templates that generate ordinary compose folders. Never a proprietary format or a hidden dataset like TrueNAS's `ix-apps`.

The core principle: **the truth lives on disk** (one folder per stack with `compose.yaml` and `.env`). rynas creates it, displays it, protects it with snapshots and backups, but does not own it. Dockhand, Dockge, Arcane and Komodo all work on the same files. Portainer is the exception, see below.

---

## 1. Summary comparison

| Product | Model | Compose files | Adopts existing stacks | Notes relevant to rynas |
|---|---|---|---|---|
| **TrueNAS 24.10+** | Docker Compose under the hood, "train" catalog, Custom App YAML | hidden dataset `ix-apps` (`/mnt/.ix-apps`), not to be touched | no; catalog apps can be converted to custom | many users work around it with Dockge or Portainer, but those apps then don't show up in the GUI |
| **OMV + openmediavault-compose** | folder per stack inside a shared folder | YAML and `.env` per stack, `compose.yml`/`.env` symlinks for the CLI | yes, the files are ordinary | scheduled backups (including encrypted Borg), exclusions, option to not stop containers during backup, global `.env` |
| **Unraid** | Docker tab with XML templates and Community Apps; compose via plugin | proprietary XML | compose plugin with display glitches | hard to merge upstream XML template updates; native compose expected with Unraid 8 |
| **Synology Container Manager** | "Projects" = compose in a chosen folder | project folder | imports an uploaded compose | does not create bind mount folders, they must exist beforehand |
| **Cockpit** | cockpit-podman, Podman only | n/a | n/a | Docker support was dropped; only a young third-party cockpit-docker exists |
| **CasaOS/ZimaOS, Umbrel** | app store on compose + metadata | `Apps/<App>/docker-compose.yml` + metadata | custom compose import | CasaOS moved from a proprietary JSON to compose: confirms compose is the way to go |
| **Portainer CE/BE** | full manager, stacks in its own database | `/data/compose/<id>` in its volume | **no**: stacks created outside are "external", with "limited" control | Docker 29 requires 2.33.5 LTS / 2.36.0 STS or later |
| **Dockhand** (BSL 1.1) | compose, git, Hawser agents, Trivy/Grype, update checks | `DATA_DIR/stacks/...` or `STACKS_DIR` | **yes, in place** ("Import → Scan this folder"; files stay where they are) | requires a bind mount with the same path inside and outside the container; labels `dockhand.update=false`, `dockhand.notify=false`, `dockhand.adopt=false` |
| **Dockge** (MIT) | stack folder | `/opt/stacks/<stack>/compose.yaml` | yes, if the stack is in the folder | last official release in January 2024; forks are where development happens now |
| **Arcane** (BSD-3) | `PROJECTS_DIRECTORY` | `compose.yaml` / `docker-compose.yml` | yes, by scanning the folder | same host and container path, otherwise `./data` paths break |
| **Komodo** (Rust) | "Files on Server" stacks (`run_directory`), from UI or git | on disk | yes, via project name; the third-party komodo-import tool also exists | Periphery agent on every host |
| **Yacht** | — | — | — | not recommended, stalled since 2023 |
| **Lazydocker** | terminal UI | — | — | worth mentioning in the docs, no integration needed |

## 2. Best ideas, with sources

- **One folder per stack, ordinary files** (Dockge, Arcane, Komodo, OMV): Dockge "won't kidnap your compose files". This is the standard that lets users switch managers. Sources: github.com/louislam/dockge, Arcane documentation.
- **In-place adoption** (Dockhand ≥1.0.7): imports files "in place", so relative paths keep working. Source: dockhand.pro/manual.
- **Same path inside and outside the manager containers**: every manager requires it. rynas must honor this when installing them.
- **`compose.yml`/`.env` symlinks and per-stack backups with exclusions** (OMV compose). Source: openmediavault-compose changelog.
- **Stack discovery via labels** (`com.docker.compose.project`, `…project.working_dir`, `…project.config_files`): this is how Dockhand identifies "Untracked" stacks. rynas can use them to list all stacks, whoever created them.
- **Update checks without pulling the image**: query only registry metadata and pull only if the digest is new. Dockhand adds `MINIMUM_RELEASE_AGE_HOURS`.
- **Separate standard compose from catalog metadata** (ZimaOS v2, Umbrel): the catalog adds metadata alongside an ordinary compose file.

## 3. Mistakes to avoid (user complaints)

1. **Hidden or proprietary state.** TrueNAS's `ix-apps` dataset ("any modification … instability") and Portainer's database (with limitations on external stacks and stacks lost after a volume restore) push users toward Dockge. There is a thread "Edit apps directly in compose.yaml like in dockge" on the TrueNAS forum.
2. **Non-standard formats.** Unraid's XML templates, CasaOS's early JSON.
3. **Apps installed with other tools vanishing from the GUI** (TrueNAS + Dockge). rynas must show all containers, not just its own.
4. **Bind mounts on nonexistent folders** (Synology). rynas must create the folders with correct permissions, or flag them.
5. **Major Docker upgrades breaking clients.** Docker 29 raised the minimum API version to 1.44: old Portainer and Traefik broke. The temporary workaround is `min-api-version` in `daemon.json`.
6. **Docker starting before the volume is mounted**: containers start on an empty folder on the system disk.
7. **For Dockhand**: root-owned stack folders that block edits, and the security module being heavy below 4 GB of RAM.

## 4. Recommendation for rynas

### Package choice: `docker-ce` from the Docker repository

- Debian 13 is officially supported: `docker-ce`, `containerd.io`, `docker-buildx-plugin`, `docker-compose-plugin`, repository in deb822 format (`/etc/apt/sources.list.d/docker.sources`). Consistent with rynas's deb822 packaging.
- trixie's `docker.io` is frozen at **26.1.5** for the whole life of the release (see the piuparts logs), with only backported fixes. It is old compared to current managers and images. How trixie ships compose v2 was not reliably verified.
- **Rules:**
  - if Docker is already installed (`docker.io`, `docker-ce` or Snap), rynas **adopts** it and does not reinstall it;
  - `docker-ce` is installed only on request;
  - the package stays pinned to its major version (apt pinning / hold);
  - moving to a new major is an explicit action, with a changelog warning and a snapshot beforehand.

### What the `docker` plugin DOES

**Engine**
- Installs and updates `docker-ce`.
- Writes `daemon.json`:
  - `data-root` on the volume;
  - `log-driver: local` (rotation included);
  - `live-restore: true`, so updating the daemon doesn't stop containers;
  - configurable `default-address-pools`, to avoid conflicts with 172.x/192.168.x LANs;
  - `min-api-version` only as a temporary workaround.
- Sets `root` in `/etc/containerd/config.toml` (with the containerd store, the default for new Docker 29 installations, images and snapshots end up there).
- Adds a systemd drop-in with `RequiresMountsFor=<volume>` for `docker.service` and `containerd.service`.

**Status and dashboard**
- Engine version, disk usage.
- `docker.summary` widget (details in §6).
- Streaming events: `health_status: unhealthy`, `die`, `oom`, restart loops. These become notifications.

**Listing**
- All containers and all stacks, grouped by `com.docker.compose.project`.
- For each stack: who manages it (rynas/folder, Portainer, untracked) and the folder (`working_dir`).
- Actions: start, stop, restart, live logs, "open". The "open" link uses the first published port, or the `rynas.url` or `homepage.href` label if present.

**Stacks**
- Create, edit (YAML and `.env` editor, validated with `docker compose config`), `up -d`, `pull`, `down` (without `-v`), remove.
- Everything happens in the standard folder, so anyone else sees the changes.

**Storage integration**
- Each stack is a **btrfs subvolume** (`/<vol>/docker/stacks/<stack>`). Before `pull`/update rynas takes a read-only snapshot (option "stop the stack during the snapshot" for databases). Restore rolls back to the snapshot and reruns `up -d`.
- The `data-root` goes in a separate subvolume, **excluded** from snapshots and backups: images can be pulled again.
- The backup plugin saves the stack folders (configuration + bind data) plus an export of `docker compose config`.
- "Docker volumes" panel in the volume tab, already planned in the manifest.
- Warning when a bind points outside `/<vol>/`.

**External managers**
- Detection from the image of running containers: `portainer/portainer-ce|ee`, `fnsys/dockhand`, `louislam/dockge` and forks, `ghcr.io/getarcaneapp/*`, `ghcr.io/moghtech/komodo-*`.
- Link to the manager. Deep links only where the URL is stable, i.e. not relying on undocumented internal routes.
- **One-click install** from a small curated catalog:
  - Dockhand, with `STACKS_DIR=/<vol>/docker/stacks` mounted at the same path;
  - Dockge/Arcane, with the same folder;
  - Komodo;
  - Portainer CE.
- Each manager is an ordinary stack in `/<vol>/docker/stacks/<manager>/`, so it is updated like any other.

**Notifications**
- Unhealthy, OOM, crash loop, `data-root` disk full.
- Image updates: notification only, comparing digests without pulling. No automatic updates in rynas: if Dockhand is present, that job is delegated to it.

### What the `docker` plugin DOES NOT do

- It is not a full manager: no management of networks, registries, builds, Swarm, Kubernetes, per-stack RBAC, GitOps or vulnerability scanning. Dockhand and Komodo cover that.
- No `exec` terminal in phase 2. If it comes, it will be admin-only, with confirmation and audit logging.
- No proprietary format, no database as the source of truth for stacks. `config.db` holds only preferences: chosen volume, snapshot options, preferred manager.
- No forced adoption of Portainer stacks: they are shown as "managed by Portainer", read-only on the compose file, with start/stop/logs.
- Does not replace the engine if Docker is already present. No Podman in phase 2.
- **Docker not installed:** the plugin is not installed, so there is no trace of it in navigation, the dashboard or the storage tabs. It appears only in the Plugin Center ("Docker — containers and compose stacks"). If Docker exists but the plugin doesn't, a single informational line in the Plugin Center ("Docker detected, install the plugin to manage it").

### Podman

trixie ships Podman 5.4.2 with Quadlet, but the managers (Portainer, Dockhand, Arcane) are designed for Docker. There are already reports of Quadlet issues on trixie, and podman-compose has gaps in compose v2 support (profiles, for example).

Recommendation:
- not now;
- `bollard` also works with the Podman API, so it remains an "experimental" option for phase 3;
- Podman users can use Cockpit, which rynas can link to.

## 5. Phases

**Phase 2 (plugin MVP)**
- Engine install or adoption, `daemon.json` and containerd root, mount drop-in, assisted `data-root` migration.
- Container and stack listing, actions, logs, widget, unhealthy/OOM notifications.
- Standard on-disk layout with one subvolume per stack, compose and `.env` editor, pre-update snapshots, backup through the backup plugin.
- Manager detection, links, one-click install of Dockhand / Dockge-Arcane / Portainer.
- `DOCKER-USER` rule for the firewall plugin.

**Phase 3**
- App catalog from git, already planned in `03-modules-roadmap` ("schema → form"). Each entry is a **template** that generates a standard folder (`compose.yaml` + `.env` + `x-rynas` for metadata). After installation the app becomes an ordinary stack that other managers can edit too (lesson from CasaOS/ZimaOS and from the criticism of TrueNAS).
  - Pro: onboarding for users who don't know Docker; correct folders and permissions.
  - Con: ongoing maintenance of the entries, which is why Umbrel/CasaOS often lag behind upstream versions.
  - Therefore: a few curated entries (the typical "NAS" apps: Jellyfin, Immich, Nextcloud, Home Assistant…) plus third-party catalogs addable via git URL.
- Update notifications with preview and rollback from snapshot.
- rynas reverse proxy in front of managers and apps (TLS).
- Read-only `docker-socket-proxy` for third-party dashboards (Homepage).
- Experimental Podman support.

## 6. Key technical details

**Talking to Docker**
- `bollard` crate (latest version 0.21.0, API schema 1.52, with version negotiation). Works with Docker ≥25 (Docker 29's minimum API is 1.44) and also with `docker.io` 26.
- For compose, `bollard` is not enough: the plugin runs `docker compose -p <stack> --project-directory <dir> …` with a timeout, following the "exec with timeout" pattern of `rynasd`.

**Processes and permissions**
- The `rynas-plugin-docker` service has access to `/var/run/docker.sock`, which is effectively root (`docker` group or root).
- The plugin API exposes **only whitelisted operations**, and only to the admin role.
- Never a generic proxy to the Docker API, and never the socket passed to the browser or to non-admin users.
- Every action is recorded in the audit log.
- Installed managers receive the socket: the UI warns that "the manager has root privileges and its own authentication".

**On-disk layout**
```
/<vol>/docker/              "docker" subvolume
  engine/                   subvolume: data-root (excluded from snapshot/backup)
  containerd/               containerd root (excluded)
  stacks/                   ← STACKS_DIR (Dockhand) / DOCKGE_STACKS_DIR / PROJECTS_DIRECTORY (Arcane) / run_directory (Komodo)
    <stack>/                one subvolume per stack
      compose.yaml
      .env
      data/ …               relative bind mounts (./data)
  .env.global (optional, included via env_file)
```
- Managers must be mounted at the same path: `-v /<vol>/docker/stacks:/<vol>/docker/stacks`.
- Folder ownership: `docker-apps` user/group (PUID/PGID), never root, to avoid the Dockhand issue described above.

**Baseline daemon.json**
```json
{ "data-root": "/<vol>/docker/engine", "log-driver": "local",
  "live-restore": true,
  "default-address-pools": [{"base": "10.200.0.0/16", "size": 24}] }
```
Plus `root = "/<vol>/docker/containerd"` in `/etc/containerd/config.toml`.

**Filesystem**
- `overlay2` is supported on btrfs; the `btrfs`/`zfs` drivers need more maintenance. Default: overlay2/overlayfs snapshotter.

**Firewall**
- Published ports bypass ufw/firewalld. User rules go in `DOCKER-USER`.
- Docker 29's nftables backend is experimental: to be evaluated once it becomes the default.

**Dashboard widget**
- `GET /containers/json` + `stats?stream=false` for running containers. Better still, a single aggregated stats stream in the plugin, pushed via SSE, which rynas already uses for metrics.

## Cited URLs

- https://docs.docker.com/engine/install/debian/
- https://www.docker.com/blog/docker-engine-version-29/
- https://docs.docker.com/engine/storage/containerd/
- https://docs.docker.com/engine/daemon/
- https://docs.docker.com/engine/storage/drivers/select-storage-driver/
- https://piuparts.debian.org/trixie2next/pass/docker.io_26.1.5+dfsg1-9+b13.log
- https://lists.debian.org/debian-go/2025/01/msg00070.html
- https://dockhand.pro/manual
- https://forum.hhf.technology/t/deploying-dockhand-a-detailed-technical-guide-2026/4181
- https://github.com/louislam/dockge
- https://reddit.birdcat.cafe/r/selfhosted/comments/1hee1h6/dockge_development_abandoned/m23t1zs/?context=3
- https://komo.do/docs/deploy/compose
- https://foxxmd.github.io/komodo-import/docs/usage/overview
- https://www.bitdoze.com/arcane-vs-dockhand/
- https://pkg.go.dev/github.com/getarcaneapp/arcane/backend/v2/pkg/projects
- https://www.bitdoze.com/portainer-alternatives/
- https://oneuptime.com/blog/post/2026-08-01-portainer-control-over-stack-limited/markdown
- https://oneuptime.com/blog/post/2026-03-20-docker-engine-29-portainer/markdown
- https://github.com/portainer/portainer/discussions/11456
- https://www.truenas.com/docs/truenasapps/communityapps/
- https://apps.truenas.com/resources/deploy-ix-app
- https://forums.truenas.com/t/solutions-provided-edit-apps-directly-in-compose-yaml-file-and-env-like-in-dockge/35552
- https://technotim.com/posts/truenas-docker-pro/
- https://github.com/OpenMediaVault-Plugin-Developers/openmediavault-compose
- https://raw.githubusercontent.com/OpenMediaVault-Plugin-Developers/openmediavault-compose/main/debian/changelog
- https://forums.unraid.net/topic/200462-staxx-new-docker-compose-management-plugin/
- https://www.wundertech.net/how-to-use-docker-on-a-synology-nas
- https://hometechops.com/self-hosting/docker-on-synology-container-manager
- https://shop.zimaspace.com/pages/zimaos-install-custom-docker-compose
- https://www.linuxlinks.com/cockpit-podman-web-interface-managing-podman-containers/
- https://michael-prokop.at/blog/2025/07/20/what-to-expect-from-debian-trixie-newintrixie/
- https://www.redhat.com/it/blog/podman-compose-docker-compose
- https://docs.rs/crate/bollard/latest

**Weak sources:** Synology, Unraid, CasaOS and the manager comparisons rely on blogs and forums, not official documentation. How trixie ships compose v2 (`docker-compose` package) was not reliably verified. The lowering of the minimum API version to 1.40 in Docker 29.3, mentioned on a forum, also remains unconfirmed.
