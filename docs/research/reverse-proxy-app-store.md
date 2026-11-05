# Report: reverse proxy and App & Plugin catalog for rynas

This report is based on web research from November 2026. Where sources are secondary or disagree, the text says so.

---

## Topic 1: reverse proxy and web access to services

### Comparison

| Product | ACME certificates | Wildcard | Access and authentication | WebSocket | TCP/UDP | RAM (approx.) | License and status |
|---|---|---|---|---|---|---|---|
| **Nginx Proxy Manager** | certbot, HTTP-01 and DNS-01 (many plugins) | yes, with DNS-01 | access lists (IP + basic auth); per-path since 2.16 | must be enabled manually | yes ("streams") | ~100-150 MB (nginx + Node) | MIT. Active (2.16 in 2026), but the certbot update in 2.16 can break DNS plugins, and armv7 has been unsupported since 2.15 |
| **Caddy** | built in (certmagic): HTTP-01, TLS-ALPN, DNS-01 with modules | yes; since 2.10 uses the wildcard for subdomains | `forward_auth`, basic auth, IP filtering | automatic | only with the `caddy-l4` module | ~20-40 MB | Apache-2.0. Very active (2.10: ECH, post-quantum). Configured through a JSON API, including over a Unix socket |
| **caddy-docker-proxy** | as Caddy | yes | as Caddy | yes | — | as Caddy | MIT. Reads configuration from container labels |
| **Traefik v3** | built in, many DNS providers | yes | middlewares, forwardAuth | yes | yes | ~50-80 MB | MIT. Very active, but label-based configuration is hard for an average user |
| **Zoraxy** | built in | yes | access control, geo-IP, uptime monitor | yes | yes | low (Go) | AGPL-3.0, v3.3.x in 2026. Small project |
| **SWAG** (linuxserver) | certbot + fail2ban | yes | preset confs for Authelia/Authentik | yes | — | ~nginx | GPL-3.0. Files only, no UI |
| **Pangolin** | built in (Traefik) | yes | identity-aware, WireGuard tunnel (Newt) | yes | yes | VPS + client | AGPL-3 + commercial license. For exposing services without opening ports |
| **Synology DSM** | Let's Encrypt HTTP-01 only; DNS-01 only for `synology.me` | `synology.me` only | access control profiles (IP), Login Portal | manual, via "Custom header" | no | built in (nginx) | proprietary |
| **TrueNAS** | ACME DNS-01 (Cloudflare, Route53, OVH, script) only for the UI certificate | yes | — | — | — | — | no built-in proxy: users install NPM, Traefik or Caddy from the catalog |
| **OMV / Unraid** | OMV: self-signed or import. Unraid: `myunraid.net` certificate and Unraid Connect | — | — | — | — | — | no built-in proxy: SWAG/NPM as containers. Unraid 7 integrates Tailscale into containers |

### Ideas worth taking

- **DSM**: the reverse proxy as a system panel (source → destination, access profile, headers) and a single certificate manager, with a certificate assigned to each service.
  - Its best-known limitation is the lack of DNS-01 for custom domains. That is why many users run `acme.sh` scripts by hand.
  - WebSocket must be enabled manually, a recurring problem with Vaultwarden and Home Assistant.
- **TrueNAS**: "DNS authenticator" credentials reusable across multiple certificates.
- **NPM**: a very simple three-tab UI (Proxy Hosts, Access Lists, SSL), which is the reason for its success.
- **Caddy**: automatic HTTPS, zero-config WebSocket, `forward_auth`. The JSON API over a Unix socket allows loading the whole configuration atomically (`POST /load`), which fits rynas's pending/apply/anti-drift model well (decision D-CFG).
- **Cosmos / Umbrel / Runtipi**: when installing an app, an "expose on `app.domain`" option appears and the route is created automatically. Neither DSM nor TrueNAS does this, and it is the feature that can set rynas apart.
- **Unraid 7 and Pangolin**: remote access via Tailscale or a tunnel as a safe alternative to opening port 443.

### Mistakes to avoid

1. **Making NPM a dependency.** Its API has no stable official documentation and updates change behavior (certbot in 2.16, armv7 dropped in 2.15). On top of that, if rynas drove it, there would be two sources of truth for the configuration.
2. **Having a proxy that can break as the only way into the UI.** If the proxy goes down, the user also loses access to the NAS.
3. **Public exposure by default.** DSM and QNAP have suffered ransomware waves precisely through exposed UIs.
4. **Offering only HTTP-01.** It forces port 80 to be opened and rules out wildcard certificates and certificates for LAN-only or Tailscale-only hosts.

### Recommendation: option (c), with the native proxy as the foundation

**What rynas does natively** (builtin "Web access" module, later the `rynas-plugin-proxy` plugin):

- **Engine**: Caddy, shipped as a `rynas-caddy` package built with xcaddy and a set of DNS modules (Cloudflare, Route53, OVH, Hetzner, deSEC, DuckDNS, Gandi, rfc2136).
  - Debian's `caddy` package is old and has no DNS modules, so rynas has to ship its own.
  - The cost is that security updates for that package become rynas's responsibility.
  - Fallback: Debian's nginx (security support from Debian) with ACME handled by rynas via `lego`. More work (reloads, text configuration, manual WebSocket).
- **Configuration**: rynas generates it from its SQLite database and loads it in full through the API on `/run/rynas/caddy-admin.sock`. Caddy keeps no state of its own.
- **Certificate manager** ("Certificates" panel, DSM/TrueNAS style):
  - initial self-signed certificate, import, ACME HTTP-01 and DNS-01, wildcard;
  - DNS credentials stored as reusable, encrypted objects;
  - a certificate assigned to each service;
  - expiry warnings in notifications.
- **Proxy hosts**: domain → destination (`http://127.0.0.1:port` or container), with:
  - an **access profile**: `LAN`, `LAN + Tailscale (100.64.0.0/10)`, `public` (the latter with confirmation and warnings);
  - **authentication**: none, basic, rynas login via `forward_auth` to `rynas-web /api/v1/auth/verify` (equivalent to DSM's Login Portal/SSO, with 2FA and passkeys already in the core), or an external IdP (Authelia, Pocket ID, Authentik);
  - WebSocket always on, with no setting to touch.
- **App integration**: the install wizard asks "Expose on `<app>.<domain>`" and creates the route. Each app declares in its manifest the port, WebSocket, any `forward_auth` and the headers it needs.

**How rynas itself is exposed over HTTPS:**

1. `rynas-web` (rustls) always listens on its own dedicated port, for example `:8443`. On first start it generates a self-signed certificate with SANs for the hostname, `hostname.local` and the IPs. This port is the **emergency access** and always works, like DSM's 5001.
2. When the user enables "Web access", Caddy takes ports 80 and 443 and also publishes the UI at `https://nas.domain`. `rynas-web` reuses the same ACME certificate for its dedicated port, reading it from the certificate store managed by rynas.
3. If an app wants port 443 (for example NPM installed from the catalog), the port manager blocks it and offers two options: turn off the native proxy, or install the app on another port.

**What is delegated to apps (catalog):**

- NPM, Traefik and Zoraxy, for those who prefer them. With one of them active, rynas turns off its own Caddy and only serves its UI on the dedicated port.
- Authelia, Authentik and Pocket ID as IdPs.
- Cloudflare Tunnel (`cloudflared`) and Pangolin/Newt for exposure without opening ports.
- Tailscale, on the other hand, ships as a **plugin** (system service integrated with access profiles and with `tailscale cert` for `*.ts.net`).
- TCP/UDP streams: not in the first release. Later with `caddy-l4` or an app (NPM/Zoraxy). On a NAS this request is rare.

---

## Topic 2: application and plugin store

### Comparison

| System | Format | Third-party sources | Curation and quality | Updates | Known flaws |
|---|---|---|---|---|---|
| **Synology Package Center** | signed `.spk` | yes (SynoCommunity, third-party sources) | since DSM 7 only Synology is "trusted"; third parties always get a warning | automatic for packages | binary packages tied to the DSM version; the Docker catalog is separate (Container Manager) |
| **TrueNAS Apps** | git `truenas/apps`: `ix-dev/<train>/<app>/` with `app.yaml`, `questions.yaml` (form), `templates/docker-compose.yaml` (Jinja + versioned library), `test_values`, `migrations/`; CI produces `trains/` and `catalog.json` | custom git catalogs (rarely used) | `stable` / `community` / `enterprise` trains; 3-month deprecation shown in the UI; bot updating images; test renders in CI | `version` bump + value migrations | TrueCharts' exit at the Kubernetes-to-Docker switch (24.10): no migration, apps had to be reinstalled |
| **Unraid CA** | XML template per container | many author repos | moderation: *deprecated*, *incompatible*, *blacklisted* states; warnings via Fix Common Problems | often `latest` tag | single-container templates, highly variable quality |
| **OMV-extras / compose** | `.deb` plugins + `openmediavault-compose` with example files | — | examples curated by a single maintainer | manual, with scheduled backup | minimal catalog |
| **CasaOS / ZimaOS** | compose + `x-casaos` extensions | yes, many sources (awesome.casaos.io) | unverified third-party sources | — | CasaOS core stalled since v0.4.15 (Dec 2024), according to a page from the vendor itself |
| **Umbrel** | `umbrel-app.yml` + `docker-compose.yml`; community stores as git repos with an id prefix | yes, addable from the UI | official store with review | by the team | poor fit for a NAS: no path choice, little control |
| **Runtipi** | `config.json` (`form_fields` → env, `random` for secrets) + compose `x-runtipi` + `metadata/logo.jpg`, `description.md` | custom app stores since v4 | PR review | — | very simple, small ecosystem |
| **YunoHost** | `manifest.toml` + `apps.toml` | — | **levels 0-9 computed weekly by CI**, **antifeatures** (`package-not-maintained`, `deprecated-software`), free upstreams only | via CI | native apps, not containers |
| **Cosmos Cloud** | compose ServApps + built-in reverse proxy and SSO | yes | — | — | "other" license (to be verified), single developer |
| **Proxmox helper scripts** | `bash -c "$(curl …)"` | — | no formal audit | — | a model not to copy: remote scripts executed as root |

### Ideas worth taking

- **TrueNAS**:
  - schema separate from the template (form generated from the schema);
  - versioned template library;
  - `test_values` rendered in CI;
  - migrations of saved values;
  - 3-month deprecation visible in the UI;
  - bot for image updates.
- **YunoHost**: quality level computed by CI (not self-declared by the author) and declared antifeatures. Excellent for honestly surfacing risks such as telemetry, non-free components or the need for public exposure.
- **Unraid**: moderation states (deprecated / incompatible / blacklisted) with an active warning on already-installed apps.
- **Runtipi / Umbrel**: one folder per app, easy to contribute to; auto-generated secrets; third-party stores as git repos with an id prefix to avoid collisions.
- **Synology**: third-party sources allowed but flagged, with an always-visible warning.

### Mistakes to avoid

1. **The TrueCharts case.** A complex catalog (Helm) depended on platform internals, and when the backend changed everything broke with no migration path. For rynas this means:
   - standard compose;
   - data in readable paths;
   - templates with almost no logic;
   - a **"Convert to custom stack"** button always available (in TrueNAS the equivalent escape hatch is installing via YAML).
2. **`latest` tags and blind updates** (Unraid). Images must be pinned by tag + digest; every update goes through PR, CI and channel.
3. **Abandoned apps.** Each app needs a declared maintainer, a CI-computed status, and the `unmaintained` antifeature assigned automatically after N months without upstream updates.
4. **Remote-script installers** (Proxmox helper scripts): never.
5. **Too many unchecked sources** (CasaOS): third-party sources must be disabled by default and flagged.

### Proposal: a single "Apps & Plugins" page

**Sections**: Featured · Apps (containers) · Plugins (system) · Installed · Updates · Sources.

Each card shows icon, screenshots, description, source badge (Official / Business / Third-party ⚠), quality level, antifeatures, minimum resources (RAM), ports, license and compatibility.

**Plugins** (`rynas-plugin-*.deb`):

- **Source**: the signed apt repo (aptly), with community and business channels (the latter authenticated).
- **UI index**: CI generates `plugins-index.json` (icon, screenshots, supported `api_version`, license, entitlement) and publishes it in the repo, signed together with the `Release` file.
- **Install and update**: via rynasd, with `apt` run as a UPID task. Before a core update, `api_version` compatibility is checked (already planned in the roadmap).

**Apps** (compose containers):

- **`rynas-apps` git repo**, one folder per app:

```
apps/<id>/
  app.toml          # id, name, version (package), app_version (upstream),
                    # maintainers, license, categories, min_rynas, arch,
                    # resources {ram_min_mb}, ports, web {port, websocket, auth},
                    # antifeatures, homepage, changelog_url
  compose.yaml      # standard compose; ${VAR} interpolation only, no Jinja
  form.json         # JSON Schema + ui-hints → SchemaRenderer (same as plugins)
  migrations/       # value transformations between versions
  tests/values.toml # values for CI tests
  metadata/icon.svg, screenshots/*.webp, description.md
```

- **Variables provided by rynas**: `RYNAS_APPDATA`, `PUID`/`PGID`, user-selected shares, `TZ`, domain.
- **CI pipeline**:
  1. schema lint;
  2. render and `docker compose up` with healthcheck;
  3. level computation;
  4. bundle signing (`catalog.json` + tarball) with minisign/sigstore.

  The bundle is distributed as a `rynas-apps-catalog` package from the apt repo, so it reuses signing, channels and mirrors.
- **Channels**: `stable` (updates after 7-14 days in testing without reports) and `testing`. Renovate opens the image update PRs.
- **One-click install**:
  - form generated from `form.json`;
  - auto-generated secrets;
  - creation of a **btrfs subvolume** `apps/<id>` for the data;
  - port conflict check;
  - optional proxy route (Topic 1).
- **Update**: changelog, **automatic btrfs snapshot** of the data before updating, migrations and **one-click rollback** (previous image + snapshot). A concrete advantage over DSM, Unraid and CasaOS.
- **Third-party sources**: a git repo URL with an id prefix, disabled by default. Adding one shows a warning ("not verified by rynas"); the signing key is optional (TOFU).
- **Business apps**: a separate, authenticated catalog (for example apps with LTS support, or OnlyOffice/Collabora configured for businesses).
- **Custom stacks**: free-form compose editor, always available.

### Initial curated catalog (~25 apps, sized for 4 GB of RAM)

| Area | Apps | Rationale |
|---|---|---|
| Access | NPM, Traefik, Authelia, Pocket ID, cloudflared, Pangolin Newt | alternatives to the native proxy; Authelia is light (~30 MB) while Authentik weighs ~1 GB and ships only with a warning |
| Photos | **Immich** | de facto standard, replacement for Synology Photos (≥4 GB recommended, warning) |
| Media | **Jellyfin** (free), Plex (*non-free* antifeature), Navidrome, Audiobookshelf | streaming; hardware acceleration is declared in the manifest |
| Cloud and documents | **Nextcloud** (official image with DB), Paperless-ngx, Stirling-PDF, Collabora | replacements for DSM's Drive/Office; for Nextcloud avoid AIO, which wants to manage its own proxy and containers |
| Passwords | **Vaultwarden** | requires HTTPS, so it showcases the Topic 1 integration |
| Home | **Home Assistant** (container, host network), Frigate (antifeature: needs GPU/Coral, lots of RAM), Mosquitto, Zigbee2MQTT | home automation |
| Sync and network | **Syncthing**, AdGuard Home (port 53 conflict handled by the installer) | sync, DNS |
| Utilities | Uptime Kuma, Forgejo, Mealie, Actual Budget, qBittorrent | popular, lightweight |
| AI (optional) | Open WebUI + Ollama | *high resources* antifeature |

These ship as **plugins**, not apps: Tailscale, WireGuard, NUT, rclone/backup, because they touch the system and the UI.

---

## Phases

**Phase 1 (in progress)**
- `rynas-web` over rustls HTTPS with a self-signed certificate and complete SANs;
- certificate import;
- dedicated port as emergency access;
- minimal "Plugins" page based on the apt index.

**Phase 2**
- "Web access" module with `rynas-caddy`: ACME HTTP-01 and DNS-01 certificates, wildcard, LAN/Tailscale/public profiles, `forward_auth` to rynas;
- Tailscale plugin;
- docker plugin with the `rynas-apps` v1 catalog (~25 apps): snapshots and rollback, exposure from the wizard, single "Apps & Plugins" page, stable/testing channels.

**Phase 3**
- third-party sources, business catalog, published CI-computed quality levels;
- IdP/OIDC integration (rynas as provider, or Authelia);
- L4 streams;
- opt-in ratings or install statistics;
- possible DDNS/certificate service in the style of `myunraid.net` for the business channel.

Decisions to record in `05-decisions.md`:
- **D-PROXY**: native Caddy, with nginx as fallback;
- **D-APPCAT**: compose + `app.toml` + JSON Schema, catalog distributed via apt;
- **D-EXPOSE**: LAN-only by default.

---

## Cited URLs

- NPM: https://github.com/NginxProxyManager/nginx-proxy-manager/releases · https://linuxiac.com/nginx-proxy-manager-2-16-adds-built-in-log-viewer/ · https://jacar.es/?p=7546
- Caddy 2.10: https://newreleases.io/project/github/caddyserver/caddy/release/v2.10.0 · https://alternativeto.net/news/2025/4/web-server-caddy-2-10-released-with-ech-post-quantum-key-exchange-and-more-enhancements
- Zoraxy: https://apps.truenas.com/catalog/zoraxy · https://www.freshports.org/www/zoraxy/
- Pangolin: https://github.com/fosrl/pangolin
- Proxy RAM usage: https://homelab-guide.pages.dev/reverse-proxy-traefik-caddy-nginx · https://dev.to/byte-guard/nginx-proxy-manager-vs-traefik-vs-caddy-which-reverse-proxy-should-you-pick-in-2026-gl3
- Authelia/Authentik: https://selfhosting.sh/compare/authentik-vs-authelia/
- DSM reverse proxy and WebSocket: https://vaultwarden.discourse.group/t/solved-websocket-setup-on-synology/116 · https://www.synoforum.com/threads/synology-reverse-proxy-under-the-hood.5271/post-23956
- SynoCommunity: https://docs.synocommunity.com/user-guide/installation/ · https://docs.synocommunity.com/developer-guide/publishing/package-server/
- TrueNAS apps: https://github.com/truenas/apps · https://raw.githubusercontent.com/truenas/apps/master/CONTRIBUTIONS.md · https://www.truenas.com/docs/truenasapps/communityapps/
- TrueCharts: https://truecharts.org/news/scale-faq
- Unraid CA: https://docs.unraid.net/unraid-os/using-unraid-to/run-docker-containers/community-applications/ · https://forums.unraid.net/topic/87144-ca-application-policies
- OMV-extras: https://wiki.omv-extras.org/
- CasaOS/ZimaOS: https://shop.zimaspace.com/pages/is-casaos-abandoned-development-status · https://community.zimaspace.com/t/community-conversation-may-2024-part-3-app-store-ecosystem-zimaos-technical-framework/1872
- Umbrel: https://github.com/getumbrel/umbrel-community-app-store
- Runtipi: https://runtipi.io/docs/reference/config-json · https://runtipi.io/docs/guides/create-your-own-app-store
- YunoHost: https://github.com/yunohost/apps
- Proxmox helper scripts: https://dev.co/devops/open-source/proxmoxve

Open points to verify:
- **Cosmos license**: aggregator sites say "other"; the LICENSE file in the repo needs checking.
- **NPM 2.16 dates**: secondary sources that disagree with each other.
- **CasaOS status**: the source is the vendor itself.
- **Redis in Authentik**: recent Authentik versions reportedly no longer require it, but this was not verified in this research.
