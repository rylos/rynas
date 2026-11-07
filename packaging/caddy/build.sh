#!/usr/bin/env bash
# Build the rynas-caddy Debian package (amd64): Caddy from a pinned upstream release built
# with xcaddy, its own systemd unit, system user and bootstrap configuration
# (D-PROXY; docs/research/web-access.md §1 and §4; docs/phase2/docker-apps.md §5.3).
#
#   packaging/caddy/build.sh [--dns] [--no-docker] [--validate FILE.json]...
#
#   --dns          compile the DNS provider modules of dns-modules.txt (wave W2; every line
#                  must pin a version)
#   --no-docker    run directly (already inside the pinned Go image, e.g. as a CI job image)
#   --validate F   after the build, `rynas-caddy validate` each given configuration (e.g.
#                  crates/rynasd/tests/golden/web_access_caddy.json); certificate paths
#                  below /var/lib/rynas-caddy are pointed at a throw-away certificate
#
# Inputs are pinned in versions.env (Caddy, xcaddy, Go image by digest). The build runs in
# that image as the calling user, with CGO off, -trimpath, no VCS stamping, a stripped
# binary and SOURCE_DATE_EPOCH from the last commit touching packaging/caddy, so the same
# inputs give the same package. Go modules are verified against sum.golang.org (default
# GOSUMDB). Never `caddy upgrade` / `add-package`: they rewrite the binary on disk.
#
# Output: target/caddy/rynas-caddy_<caddy>-<rev>_amd64.deb and its .sha256.
# Nothing is installed or started on the build host.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# shellcheck source=versions.env
. "$HERE/versions.env"

DNS=0
DOCKER=1
VALIDATE=()
while [ $# -gt 0 ]; do
    case "$1" in
        --dns) DNS=1 ;;
        --no-docker|--inside) DOCKER=0 ;;
        --validate) shift; VALIDATE+=("$1") ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

OUT="$ROOT/target/caddy"
mkdir -p "$OUT"

if [ "$DOCKER" = 1 ]; then
    args=(--inside)
    [ "$DNS" = 1 ] && args+=(--dns)
    for v in "${VALIDATE[@]}"; do
        args+=(--validate "$v")
    done
    mkdir -p "$OUT/gocache"
    # the worktree's git data may live outside the mount: the epoch is computed here
    epoch=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct -- packaging/caddy 2>/dev/null || echo 1762473600)}
    exec docker run --rm --user "$(id -u):$(id -g)" \
        -e SOURCE_DATE_EPOCH="$epoch" \
        -e HOME=/tmp -e GOPATH=/src/target/caddy/gocache/path \
        -e GOCACHE=/src/target/caddy/gocache/build \
        -v "$ROOT:/src" -w /src \
        "$GO_IMAGE" packaging/caddy/build.sh "${args[@]}"
fi

# ---- inside the Go image ----
go_have=$(go env GOVERSION)
if [ "$go_have" != "go$GO_VERSION" ]; then
    echo "Go $GO_VERSION expected, found $go_have (use the pinned image $GO_IMAGE)" >&2
    exit 1
fi
export CGO_ENABLED=0 GOTOOLCHAIN=local GOFLAGS="-trimpath -buildvcs=false" GOAMD64 GOOS=linux GOARCH=amd64
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct -- packaging/caddy 2>/dev/null || echo 1762473600)}
export SOURCE_DATE_EPOCH

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

go install "github.com/caddyserver/xcaddy/cmd/xcaddy@v$XCADDY_VERSION"
XCADDY=$(go env GOPATH)/bin/xcaddy

with=()
if [ "$DNS" = 1 ]; then
    while read -r line; do
        line=${line%%#*}
        line=$(echo "$line" | tr -d '[:space:]')
        [ -z "$line" ] && continue
        case "$line" in
            *@v[0-9]*) with+=(--with "$line") ;;
            *) echo "dns-modules.txt: $line has no pinned version" >&2; exit 1 ;;
        esac
    done < "$HERE/dns-modules.txt"
    if [ ${#with[@]} -eq 0 ]; then
        echo "dns-modules.txt pins no module yet (wave W2)" >&2
        exit 1
    fi
fi

BIN="$WORK/rynas-caddy"
(
    cd "$WORK"
    XCADDY_SKIP_CLEANUP=0 XCADDY_GO_BUILD_FLAGS="-ldflags '-s -w -buildid='" \
        "$XCADDY" build "v$CADDY_VERSION" --output "$BIN" "${with[@]}"
)
"$BIN" version | grep -q "^v$CADDY_VERSION" || { echo "built binary is not v$CADDY_VERSION" >&2; exit 1; }
"$BIN" build-info > "$WORK/build-info.txt"

# the bootstrap configuration and any requested ones must pass Caddy's own validation
CERTDIR="$WORK/certs"
mkdir -p "$CERTDIR" "$WORK/data"
validate() {
    local f=$1 tmp="$WORK/validate.json"
    sed -e "s#/var/lib/rynas-caddy/rynas/#$CERTDIR/#g" \
        -e "s#/var/lib/rynas-caddy/data#$WORK/data#g" \
        -e "s#unix//run/rynas-caddy/admin.sock#unix/$WORK/admin.sock#g" "$f" > "$tmp"
    if grep -q "$CERTDIR/local.crt" "$tmp" && [ ! -f "$CERTDIR/local.crt" ]; then
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 \
            -subj /CN=nas.local -keyout "$CERTDIR/local.key" -out "$CERTDIR/local.crt" 2>/dev/null
    fi
    XDG_DATA_HOME="$WORK/data" XDG_CONFIG_HOME="$WORK/data" "$BIN" validate --config "$tmp" \
        || { echo "rynas-caddy validate: $f refused" >&2; exit 1; }
    echo "validated: $f"
}
validate "$HERE/bootstrap.json"
for v in "${VALIDATE[@]}"; do
    validate "$ROOT/$v"
done

VERSION="$CADDY_VERSION-$PKG_REVISION"
STAGE="$WORK/stage"
install -D -m 0755 "$BIN" "$STAGE/usr/bin/rynas-caddy"
install -D -m 0644 "$HERE/rynas-caddy.service" "$STAGE/usr/lib/systemd/system/rynas-caddy.service"
install -D -m 0644 "$HERE/rynas-caddy.sysusers" "$STAGE/usr/lib/sysusers.d/rynas-caddy.conf"
install -D -m 0644 "$HERE/bootstrap.json" "$STAGE/usr/share/rynas-caddy/bootstrap.json"
install -D -m 0644 "$HERE/debian/copyright" "$STAGE/usr/share/doc/rynas-caddy/copyright"
install -D -m 0644 "$WORK/build-info.txt" "$STAGE/usr/share/doc/rynas-caddy/build-info.txt"
mkdir -p "$STAGE/DEBIAN"
for s in postinst prerm postrm; do
    install -m 0755 "$HERE/debian/$s" "$STAGE/DEBIAN/$s"
done
size=$(du -sk --exclude=DEBIAN "$STAGE" | cut -f1)
modules="none"
[ "$DNS" = 1 ] && modules="$(grep -c -v -e '^\s*#' -e '^\s*$' "$HERE/dns-modules.txt") DNS provider modules"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: rynas-caddy
Version: $VERSION
Architecture: amd64
Maintainer: Marco Ziliani <508111+rylos@users.noreply.github.com>
Installed-Size: $size
Depends: systemd
Conflicts: caddy
Section: httpd
Priority: optional
Homepage: https://github.com/rylos/rynas
Description: rynas web access reverse proxy (Caddy $CADDY_VERSION)
 Caddy $CADDY_VERSION built from the upstream release with xcaddy (Go $GO_VERSION,
 extra modules: $modules) for rynas web access. It owns ports 80 and 443, takes its
 configuration only from rynasd through an admin API on a private Unix socket, and
 starts from the last configuration rynasd loaded.
EOF
find "$STAGE" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
DEB="$OUT/rynas-caddy_${VERSION}_amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$DEB" >/dev/null
(cd "$OUT" && sha256sum "$(basename "$DEB")" > "$(basename "$DEB").sha256")
echo "built $DEB"
cat "$DEB.sha256"
