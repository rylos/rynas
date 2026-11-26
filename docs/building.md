# Building rynas

Requirements: Rust 1.90+ (edition 2024), the `wasm32-unknown-unknown` target and
[trunk](https://trunkrs.dev) 0.22 for the interface.

```sh
rustup target add wasm32-unknown-unknown
(cd crates/rynas-ui && trunk build --release)   # interface → crates/rynas-ui/dist
cargo build --release                           # rynasd, rynas-web, …
```

The Debian package: `packaging/build-deb.sh --build-ui` writes `target/debian/rynas_*.deb`,
built in a trixie container. Releases are built by
[`.github/workflows/release.yml`](../.github/workflows/release.yml) from a `vX.Y.Z` tag.

Development without root (`trunk serve` for the interface, rynasd with a private socket) is
described in [`phase1/plan.md`](phase1/plan.md). A local instance with fake NAS hardware, for
trying the interface without touching the machine: [`tools/ui-audit/`](../tools/ui-audit/README.md).

## How it fits together

```
browser ── HTTPS ──> rynas-caddy ──> rynas-web ── Unix socket ──> rynasd (root)
           (WebAssembly UI)          HTTP API, auth                system work, plugins
```

- **rynasd**: the only privileged process. It does the system work through standard tools and
  keeps every change in a staged, auditable list.
- **rynas-web**: the HTTP API (`/api/v1`), login and sessions; it never runs as root.
- **rynas-ui**: a Leptos (Rust → WebAssembly) single-page interface.
- **Plugins** add features, interface included: hardware support (such as Synology through
  synodeb) lives in its own module, so the core works with standard tools only.

Design documents: [architecture](phase0/01-architecture.md), [UI](phase0/02-ui.md),
[decisions](phase0/05-decisions.md), and the research behind each feature in
[`research/`](research/).
