#!/usr/bin/env bash
# Build the rynas Debian package (amd64, Debian 13 trixie).
#
#   packaging/build-deb.sh [--build-ui] [--no-docker]
#
# Steps: (optionally) build the web UI with trunk on the host, then inside a Debian 13
# container: cargo build --release of rynasd and rynas-web for x86-64-v2, stage the UI with
# precompressed .br/.gz siblings, generate the DEP-5 copyright file, run cargo deb.
# Output: target/debian/rynas_<version>-1_amd64.deb
#
# The container build matters: the host may have a newer glibc than trixie, and
# dpkg-shlibdeps (for the $auto dependencies) exists only on Debian.
# --no-docker runs the in-container part directly (when already on Debian 13).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

IMAGE=${RYNAS_BUILDER_IMAGE:-rynas-deb-builder:trixie}
RUST_IMAGE=${RUST_IMAGE:-rust:1.98-trixie}
CARGO_DEB_VERSION=${CARGO_DEB_VERSION:-3.8.0}
BUILD_UI=0
DOCKER=1
for a in "$@"; do
    case "$a" in
        --build-ui) BUILD_UI=1 ;;
        --no-docker) DOCKER=0 ;;
        --inside) DOCKER=0 ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

UI_DIST=crates/rynas-ui/dist
if [ "$BUILD_UI" = 1 ]; then
    (cd crates/rynas-ui && trunk build --release)
fi
if [ ! -f "$UI_DIST/index.html" ] || ! ls "$UI_DIST"/*.wasm >/dev/null 2>&1; then
    echo "no built UI in $UI_DIST: run with --build-ui (needs trunk)" >&2
    exit 1
fi

if [ "$DOCKER" = 1 ]; then
    docker build -q -t "$IMAGE" \
        --build-arg RUST_IMAGE="$RUST_IMAGE" --build-arg CARGO_DEB_VERSION="$CARGO_DEB_VERSION" \
        -f packaging/deb/builder.Dockerfile packaging/deb >/dev/null
    mkdir -p "$HOME/.cargo/registry" target/deb-build
    # same uid as the caller: no root-owned files in target/; the host cargo registry is
    # reused as download cache
    exec docker run --rm --user "$(id -u):$(id -g)" \
        -e HOME=/tmp -e CARGO_HOME=/tmp/cargo -e PATH=/usr/local/cargo/bin:/usr/bin:/bin \
        -v "$HOME/.cargo/registry:/tmp/cargo/registry" \
        -v "$ROOT:/src" -w /src \
        "$IMAGE" packaging/build-deb.sh --inside
fi

# ---- inside Debian 13 ----
# reproducible timestamps (cargo-deb honors SOURCE_DATE_EPOCH)
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct 2>/dev/null || date +%s)}
# own target dir: binaries built against trixie's glibc never mix with host builds
export CARGO_TARGET_DIR="$ROOT/target/deb-build"
# NAS CPUs (Celeron J4125 and up) all have x86-64-v2; set here only, not globally
export RUSTFLAGS="-C target-cpu=x86-64-v2 --remap-path-prefix=$ROOT=/build/rynas"

cargo build --release --locked -p rynasd -p rynas-web
# the release profile strips symbols; drop the compiler version notes too (lintian)
for b in rynasd rynasctl rynas-web; do
    objcopy --remove-section=.comment "$CARGO_TARGET_DIR/release/$b"
done

if [ -d "$UI_DIST/.stage" ]; then
    echo "trunk is writing $UI_DIST right now (.stage exists): retry when it has finished" >&2
    exit 1
fi
STAGE="$ROOT/target/deb-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE/ui"
# the UI without license texts (they are in the copyright file) and trunk's lock file
(cd "$UI_DIST" && find . -type f ! -path '*/.*' ! -name '*-LICENSE.txt' ! -name '*-OFL.txt' -print0 \
    | xargs -0 cp --parents -t "$STAGE/ui")
# every file index.html refers to must be there (trunk may be rewriting dist right now)
for ref in $(grep -oE '/[A-Za-z0-9_./-]+\.(wasm|js|css|svg|woff2)' "$STAGE/ui/index.html" | sort -u); do
    [ -f "$STAGE/ui$ref" ] || { echo "UI incomplete: index.html refers to missing $ref" >&2; exit 1; }
done
# precompressed siblings, served as is by rynas-web (index.html is rendered per request)
find "$STAGE/ui" -type f \( -name '*.wasm' -o -name '*.js' -o -name '*.css' -o -name '*.svg' -o -name '*.json' \) \
    -print0 | while IFS= read -r -d '' f; do
        brotli -q 11 -k -f "$f"
        gzip -9 -n -k -f "$f"
        # keep the compressed files only when they save something
        [ "$(stat -c %s "$f.br")" -lt "$(stat -c %s "$f")" ] || rm -f "$f.br"
        [ "$(stat -c %s "$f.gz")" -lt "$(stat -c %s "$f")" ] || rm -f "$f.gz"
    done
find "$STAGE/ui" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

# DEP-5 copyright: header and short licenses + full BUSL-1.1 and OFL-1.1 texts
fmt_license() { sed -e 's/[[:space:]]*$//' -e 's/^$/./' -e 's/^/ /' "$1"; }
{
    cat packaging/deb/copyright.in
    echo
    echo "License: BUSL-1.1"
    fmt_license LICENSE
    echo
    echo "License: OFL-1.1"
    # the OFL text without the copyright line of one font
    tail -n +3 crates/rynas-ui/assets/fonts/Inter-LICENSE.txt > "$STAGE/ofl.txt"
    sed -i '/./,$!d' "$STAGE/ofl.txt"
    fmt_license "$STAGE/ofl.txt"
} > "$STAGE/copyright"
rm -f "$STAGE/ofl.txt"

# binaries are already stripped by the release profile
mkdir -p "$ROOT/target/debian"
cargo deb -p rynas-web --no-build --no-strip -o "$ROOT/target/debian/"
ls -l "$ROOT"/target/debian/*.deb
