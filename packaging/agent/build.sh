#!/bin/sh
# Static rynas-agent binaries (musl) for the GitHub release, the AUR -bin package and the
# .deb/.rpm: one file that runs on any Linux, live systems included.
# Usage: packaging/agent/build.sh [x86_64|aarch64 ...]   (default: x86_64)
# Needs the rustup targets <arch>-unknown-linux-musl and a musl C compiler for ring and zstd
# (musl-gcc for x86_64; for aarch64 a cross gcc, CC_aarch64_unknown_linux_musl=...).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
OUT=${OUT:-$REPO/target/agent-dist}
mkdir -p "$OUT"
for arch in "${@:-x86_64}"; do
  t=$arch-unknown-linux-musl
  case $arch in
    x86_64) export CC_x86_64_unknown_linux_musl=${CC_x86_64_unknown_linux_musl:-musl-gcc} ;;
  esac
  cargo build --release --locked --target "$t" --manifest-path "$REPO/crates/rynas-agent/Cargo.toml"
  dir=${CARGO_TARGET_DIR:-$REPO/crates/rynas-agent/target}
  cp "$dir/$t/release/rynas-agent" "$OUT/rynas-agent-$arch"
  (cd "$OUT" && sha256sum "rynas-agent-$arch" > "rynas-agent-$arch.sha256")
  file "$OUT/rynas-agent-$arch" 2>/dev/null || true
done
ls -l "$OUT"
