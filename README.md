# rynas

A modular NAS manager for Debian, written in Rust. A small privileged daemon does the system
work, an HTTP API serves it, and a WebAssembly web interface (Leptos) loads plugins that add
features, UI included. It takes the best ideas from Synology DSM, TrueNAS, OpenMediaVault and
Unraid, and aims to be professional, keyboard friendly and very fast on modest hardware.

It is built first for [synodeb](https://github.com/rylos/synodeb) (Debian 13 on Synology
DS923+/DS920+ hardware), where a `synology` plugin shows and controls fans, LEDs, bays and UPS,
but it targets any Debian 13 server.

## Status

**Phase 1 — MVP in progress. Not usable yet.** Interfaces, packaging and on-disk formats will
change without notice. See [`docs/phase1/plan.md`](docs/phase1/plan.md) for the current steps.

Primary language: English (code, docs, default UI language). Italian is secondary.

## License

rynas is **source-available, not open source**:

- core (rynasd, rynas-web, UI, CLI, builtin plugins): **Business Source License 1.1**, with an
  Additional Use Grant for personal and non-commercial use; each release converts to
  **Apache-2.0** four years after publication;
- `rynas-api-types` and the plugin SDK: **Apache-2.0**, so third-party plugins can use any license.

License text: [`LICENSE`](LICENSE). Details and the business model are in [`docs/phase0/04-license.md`](docs/phase0/04-license.md).
The licensing model follows [Orlok](https://orlok.xyz).

## Building

Requirements: Rust 1.90+ (edition 2024), the `wasm32-unknown-unknown` target and
[trunk](https://trunkrs.dev) 0.22 for the UI.

```
rustup target add wasm32-unknown-unknown
(cd crates/rynas-ui && trunk build --release)   # UI → crates/rynas-ui/dist, embedded by rynas-web
cargo build --release                           # rynasd, rynas-web, ...
```

For development without root (`trunk serve` for the UI, rynasd with a private socket) see
"Development without root" in [`docs/phase1/plan.md`](docs/phase1/plan.md). systemd units, PAM and
fail2ban configuration live in [`packaging/`](packaging/).

## Documents

Phase 0 — design ([`docs/phase0/`](docs/phase0/)):

| File | Content |
|---|---|
| [00-context.md](docs/phase0/00-context.md) | vision, constraints, starting material (synodeb, SynoFile, Fusion palette, Orlok) |
| [01-architecture.md](docs/phase0/01-architecture.md) | processes, stack, plugin system, workspace |
| [02-ui.md](docs/phase0/02-ui.md) | layout, design system, key patterns, file manager |
| [03-modules-roadmap.md](docs/phase0/03-modules-roadmap.md) | modules, SynoFile port, phases 1–3 |
| [04-license.md](docs/phase0/04-license.md) | license and business model |
| [05-decisions.md](docs/phase0/05-decisions.md) | decision log |
| [research/](docs/phase0/research/) | full phase 0 reports: features, architecture, UI/UX, license |

Phase 1:

| File | Content |
|---|---|
| [plan.md](docs/phase1/plan.md) | steps, UI performance budget, API v1, authentication |
| [synodeb-integration.md](docs/synodeb-integration.md) | what rynas reads from synodeb and proposed synodeb changes |

Research ([`docs/research/`](docs/research/)):

| File | Content |
|---|---|
| [docker.md](docs/research/docker.md) | container engine, compose stacks, Docker strategy |
| [reverse-proxy-app-store.md](docs/research/reverse-proxy-app-store.md) | reverse proxy and the Apps & Plugins catalog |
| [photos-nvr.md](docs/research/photos-nvr.md) | photos (Immich) and video surveillance |
| [scheduler-lxc.md](docs/research/scheduler-lxc.md) | task scheduler on systemd units, LXC/VMs |
| [snapshots-replication.md](docs/research/snapshots-replication.md) | btrfs snapshots, replication and backup (btrbk) |
| [system-reliability-tweaks.md](docs/research/system-reliability-tweaks.md) | config backup, logs, flash wear, zram, per-disk tweaks |
| [encryption-compression-dedup.md](docs/research/encryption-compression-dedup.md) | LUKS2 encryption, compression, deduplication |
| [notifications-ups-updates.md](docs/research/notifications-ups-updates.md) | notification center, UPS, automatic updates |
| [network.md](docs/research/network.md) | network configuration, bonding, VLANs, safe apply and rollback, remote access |
| [file-services.md](docs/research/file-services.md) | file services: SMB, NFS and related protocols |
| [identity-quotas-domain.md](docs/research/identity-quotas-domain.md) | users and groups, quotas, Active Directory / domain join |

## Disclaimer

rynas is an independent project. It is **not affiliated with, endorsed by or supported by
Synology Inc.** Synology, DSM and related names are trademarks of Synology Inc., used here only
to describe compatibility.

Debian package (amd64, Debian 13): `packaging/build-deb.sh` builds `target/debian/rynas_*.deb` in a
trixie container; `packaging/test-deb.sh` and `packaging/test-deb-systemd.sh` install and exercise it
in throw-away containers. Installed layout and first login: [`packaging/deb/README.Debian`](packaging/deb/README.Debian).
The web interface is plain HTTP on port 8080 for now: use it on a trusted LAN until HTTPS lands.
