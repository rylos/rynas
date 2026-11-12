<h1 align="center">rynas</h1>

<h3 align="center">
A fast, modern NAS manager for plain Debian.<br>
Storage, shares, snapshots, Docker apps and HTTPS from one web interface, written in Rust.
</h3>

<p align="center">
  <a href="LICENSE"><img alt="License: AGPL-3.0" src="https://img.shields.io/badge/license-AGPL--3.0-blue"></a>
  <img alt="Debian 13" src="https://img.shields.io/badge/Debian-13%20trixie-a80030">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-2024-orange">
  <img alt="Status: beta" src="https://img.shields.io/badge/status-beta-blue">
</p>

<p align="center">
  <img src="docs/screenshots/dashboard.png" alt="rynas dashboard" width="100%">
</p>

You install Debian your way, then add one package: **rynas** turns the server into a NAS you
manage from the browser. Nothing is hidden: everything rynas does is plain Debian underneath
(mdadm, btrfs, Samba, NFS, systemd, Docker Compose, Caddy), so your data and your system stay
yours, with or without rynas.

> [!NOTE]
> rynas is **beta software**. The main features are in place and it runs every day on real NAS
> hardware, but there can still be rough edges and small changes before 1.0. Keep backups, as
> with any storage system, and report what you find.

## Why rynas

- **Fast on modest hardware.** A small privileged daemon, a Rust HTTP server and a WebAssembly
  interface. At rest the daemons use a few MB of RAM, and pages update live without reloading.
- **Plain Debian underneath.** No custom distribution, no lock-in: standard tools and config
  files, standard `apt` updates. Remove rynas and the server keeps working.
- **Safe changes.** Changes are staged and applied together. Network changes roll back by
  themselves if you lose the connection. Snapshots and system restore points come before
  risky operations.
- **Apps that stay up to date.** The app catalogue is signed and updated on its own, apart from
  rynas releases; image updates are checked every day and can be applied automatically, stack
  by stack.
- **Made for the keyboard.** <kbd>Ctrl</kbd> <kbd>K</kbd> finds any page, setting or action. Light and dark
  themes, English and Italian.
- **Free software, every feature included.** AGPL-3.0, free for everyone, companies included.

## Features

| | |
|---|---|
| **Dashboard** | live CPU, memory, network, disk I/O and temperatures; customizable widgets |
| **Storage** | RAID arrays and volumes (mdadm, btrfs, ext4, LUKS2 encryption), a drive view of the chassis, SMART, spin-down, zram |
| **Files** | a full web file manager: upload, preview, text editor, space analysis |
| **Shared folders and services** | SMB (with Time Machine), NFS, per-user and per-group permissions, quotas, recycle bin |
| **Snapshots** | scheduled btrfs snapshots with retention, file restore, rollback, system restore points, replication to another volume or NAS |
| **Backup** | encrypted, deduplicated backups (restic format) to another NAS, a USB disk or the cloud (SFTP, S3, B2, REST, rclone); backups of your PCs |
| **Docker** | engine setup, compose stacks with an editor, containers with CPU and memory, logs, daily image update checks with optional automatic updates; works next to Portainer, Dockge or Dockhand |
| **Apps** | a curated, signed catalogue (Immich, Jellyfin, Home Assistant, Nextcloud, Paperless-ngx, Vaultwarden, Frigate, JDownloader… 27 and growing) installed in a few clicks, kept current by automated, tested updates |
| **Web access** | HTTPS for the interface and every app through a built-in Caddy: `.local` names, Let's Encrypt certificates through your DNS provider, LAN or VPN access rules, Tailscale |
| **Users and security** | users, groups, PAM login, two-factor authentication (TOTP), fail2ban |
| **System** | network (bonding, VLANs) with safe apply, task scheduler, notifications (e-mail, ntfy, Gotify), updates, configuration restore |
| **Hardware** | fans, temperatures, status LEDs, UPS (NUT), power schedule, removable drives; dedicated modules for Synology (with synodeb) and QNAP hardware |

<table>
  <tr>
    <td><img src="docs/screenshots/storage.png" alt="Storage with the chassis view"></td>
    <td><img src="docs/screenshots/apps.png" alt="The app catalogue"></td>
  </tr>
  <tr>
    <td align="center"><b>Storage</b>: volumes and a live view of every bay</td>
    <td align="center"><b>Apps</b>: a curated catalogue, installed in a few clicks</td>
  </tr>
  <tr>
    <td><img src="docs/screenshots/docker.png" alt="Docker compose stacks"></td>
    <td><img src="docs/screenshots/hardware.png" alt="Hardware: fans and temperatures"></td>
  </tr>
  <tr>
    <td align="center"><b>Docker</b>: your stacks, including those made by other tools</td>
    <td align="center"><b>Hardware</b>: fans, temperatures, LEDs and UPS</td>
  </tr>
</table>

<sub>The dashboard screenshot uses demo data; the others come from a test instance.</sub>

## Got a Synology? Use it with synodeb

[**synodeb**](https://github.com/rylos/synodeb) installs **Debian 13 on Synology DS920+ and
DS923+** with all the NAS hardware working: fans, LEDs, buttons, beeper, power schedule, UPS,
watchdog, every bay and both M.2 slots. DSM can stay on its own disks.

Put rynas on top and you get a modern NAS interface on Synology hardware. rynas detects synodeb
and adds its hardware screens: the real chassis with its bays, fan modes, LEDs and the power
schedule. On any other Debian server those screens simply stay hidden.

QNAP models get the same treatment through their own module, starting with the **HS-251**
(verified on the hardware): bays, fans, LEDs and power recovery.

## Install

rynas targets **Debian 13 (trixie), amd64**. There is no apt repository yet. Each
[release](https://github.com/rylos/rynas/releases/latest) has the package, built by GitHub
Actions, with its `SHA256SUMS`:

```sh
curl -fLO https://github.com/rylos/rynas/releases/latest/download/rynas_<version>-1_amd64.deb
sudo apt install ./rynas_<version>-1_amd64.deb
```

Or build it yourself: `packaging/build-deb.sh --build-ui` writes `target/debian/rynas_*.deb`,
built in a trixie container.

The PC backup agent is in the same release: static binaries `rynas-agent-x86_64` and
`rynas-agent-aarch64`, plus `.deb` and `.rpm` packages.

### Releases

A `vX.Y.Z` tag runs [`.github/workflows/release.yml`](.github/workflows/release.yml). It
checks that the tag, the workspace version and `rynas-agent`'s version are the same, then
builds the NAS package and the agent and publishes them as the latest release. Fixed-tag
releases such as the NUT backport (`nut-2.8.5-0rynas1`) never become "latest". Run by hand
(`gh workflow run release.yml -f ref=main`), the workflow only builds: the packages stay as
artifacts of the run, for a test install with `dpkg -i`, and no release is made.

Then open `https://<your-server>.local/` (or `http://<your-server>:8080/` before HTTPS is set
up) and log in with an administrator account of the server. Installed layout and first login:
[`packaging/deb/README.Debian`](packaging/deb/README.Debian).

## Build from source

Requirements: Rust 1.90+ (edition 2024), the `wasm32-unknown-unknown` target and
[trunk](https://trunkrs.dev) 0.22 for the interface.

```sh
rustup target add wasm32-unknown-unknown
(cd crates/rynas-ui && trunk build --release)   # interface → crates/rynas-ui/dist
cargo build --release                           # rynasd, rynas-web, …
```

Development without root (`trunk serve` for the interface, rynasd with a private socket) is
described in [`docs/phase1/plan.md`](docs/phase1/plan.md).

## How it works

```
browser ── HTTPS ──> rynas-caddy ──> rynas-web ── Unix socket ──> rynasd (root)
           (WebAssembly UI)          HTTP API, auth                system work, plugins
```

- **rynasd**: the only privileged process. It does the system work through standard tools and
  keeps every change in a staged, auditable list.
- **rynas-web**: the HTTP API (`/api/v1`), login and sessions; it never runs as root.
- **rynas-ui**: a Leptos (Rust → WebAssembly) single-page interface, under 1.5 MB compressed.
- **Plugins** add features, interface included: hardware support (such as Synology through
  synodeb) lives in its own module, so the core works with standard tools only.

Design documents are in [`docs/`](docs/): architecture, UI, decisions and the research behind
each feature.

## License

rynas is **free software** under the **GNU Affero General Public License v3.0 or later**
([`LICENSE`](LICENSE)). Everyone may use it for free, companies included, and every feature is
in the free version. The plugin interface (`rynas-api-types`) is Apache-2.0, so plugins can
use any license. Contributions: see [`CONTRIBUTING.md`](CONTRIBUTING.md).

Like Proxmox VE, the project may one day offer an optional subscription with a more
thoroughly tested update repository and support. It will never lock features. Details:
[`docs/phase0/04-license.md`](docs/phase0/04-license.md).

App icons come from [Dashboard Icons](https://github.com/homarr-labs/dashboard-icons)
(Apache-2.0); the logos belong to their owners ([`apps/ICONS.md`](apps/ICONS.md)).

## Disclaimer

rynas is an independent project. It is **not affiliated with, endorsed by or supported by
Synology Inc.** Synology, DSM and related names are trademarks of Synology Inc., used here only
to describe compatibility.
