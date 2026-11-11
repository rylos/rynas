#!/usr/bin/env bash
# Build the rynas-oans Debian package (amd64): the oans deduplication engine from a pinned
# upstream release (D-DEDUP, docs/phase2/dedup.md §1.4).
#
#   packaging/oans/build.sh [--no-docker]
#
#   --no-docker   run directly (already inside the pinned builder image, e.g. as a CI job)
#
# Inputs are pinned in versions.env (source tarball by SHA-256 and tag commit, builder image
# by digest, build packages from a snapshot.debian.org timestamp). The build runs in that
# image, verifies the tarball and the commit it was made from, builds with -O2 -march=$MARCH (the
# upstream hardening flags stay), strips the binary and dates every file with
# SOURCE_DATE_EPOCH from the last commit touching packaging/oans, so the same inputs give
# the same package.
#
# The package puts the binary at /usr/lib/rynas/oans/oans: not in PATH, no `duperemove`
# symlink, no systemd units (rynas schedules the runs itself). oans is GPL-2.0-only and is
# only ever executed by rynas as a separate program. The corresponding source (the
# upstream tarball and this folder) is published next to the package (GPL-2.0 §3(a));
# target/oans/ gets a copy of the tarball for that.
#
# Output: target/oans/rynas-oans_<version>-<rev>_amd64.deb and its .sha256, plus
# target/oans/oans-<version>.tar.gz. Nothing is installed on the build host.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# shellcheck source=versions.env
. "$HERE/versions.env"

DOCKER=1
while [ $# -gt 0 ]; do
    case "$1" in
        --no-docker|--inside) DOCKER=0 ;;
        -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

OUT="$ROOT/target/oans"
mkdir -p "$OUT"

if [ "$DOCKER" = 1 ]; then
    epoch=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct -- packaging/oans 2>/dev/null || echo 1762819200)}
    [ -n "$epoch" ] || epoch=1762819200
    # the tarball is fetched here (the build container has no reason to reach the network
    # after its packages) and checked inside
    tarball="$OUT/oans-$OANS_VERSION.tar.gz"
    if [ ! -f "$tarball" ]; then
        curl -fsSL -o "$tarball.part" "$OANS_SOURCE_URL"
        mv "$tarball.part" "$tarball"
    fi
    exec docker run --rm -e SOURCE_DATE_EPOCH="$epoch" -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        -v "$ROOT:/src" -w /src "$BUILDER_IMAGE" packaging/oans/build.sh --inside
fi

# ---- inside the builder image (root: it installs the build dependencies) ----
export DEBIAN_FRONTEND=noninteractive
# the build packages from a fixed state of the archive, not today's mirror
cat > /etc/apt/sources.list.d/debian.sources <<EOF
Types: deb
URIs: http://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT
Suites: trixie trixie-updates
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp
Check-Valid-Until: no

Types: deb
URIs: http://snapshot.debian.org/archive/debian-security/$DEBIAN_SNAPSHOT
Suites: trixie-security
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp
Check-Valid-Until: no
EOF
apt-get update -qq
apt-get install -y -qq --no-install-recommends build-essential pkg-config ca-certificates \
    libglib2.0-dev libsqlite3-dev libxxhash-dev uuid-dev libmount-dev libblkid-dev libbsd-dev \
    dpkg-dev file git >/dev/null
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-1762819200}
export SOURCE_DATE_EPOCH

tarball="$OUT/oans-$OANS_VERSION.tar.gz"
echo "$OANS_SOURCE_SHA256  $tarball" | sha256sum -c --quiet - || { echo "source tarball checksum mismatch" >&2; exit 1; }
# GitHub's archive records the commit it was made from: it must be the pinned tag commit
commit=$(gzip -dc "$tarball" | git get-tar-commit-id || true)
[ "$commit" = "$OANS_COMMIT" ] || { echo "source tarball is of commit '$commit', not $OANS_COMMIT" >&2; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
tar -xzf "$tarball" -C "$WORK"
SRC="$WORK/oans-$OANS_VERSION"
[ -d "$SRC" ] || { echo "unexpected tarball layout" >&2; exit 1; }

# upstream's CFLAGS (warnings, gnu11, dependency files) plus the target CPU; the Makefile
# adds -O2 and its hardening flags itself
make -C "$SRC" -j"$(nproc)" VERSION="v$OANS_VERSION" \
    CFLAGS="-Wall -Wextra -Wno-unused-parameter -std=gnu11 -Werror=strict-prototypes -MMD -MP -march=$MARCH -ffile-prefix-map=$SRC=." \
    oans >/dev/null
BIN="$SRC/oans"
strip --strip-unneeded "$BIN"
"$BIN" --version | grep -q "^oans v$OANS_VERSION" || { echo "built binary is not oans v$OANS_VERSION" >&2; "$BIN" --version >&2; exit 1; }

VERSION="$OANS_VERSION-$PKG_REVISION"
STAGE="$WORK/stage"
install -D -m 0755 "$BIN" "$STAGE/usr/lib/rynas/oans/oans"
install -D -m 0644 "$SRC/docs/man/oans.8" "$STAGE/usr/share/man/man8/rynas-oans.8"
gzip -9n "$STAGE/usr/share/man/man8/rynas-oans.8"
install -D -m 0644 "$HERE/debian/copyright" "$STAGE/usr/share/doc/rynas-oans/copyright"

# dependencies: the packages of the shared libraries the binary links
deps=$(objdump -p "$BIN" | awk '/NEEDED/ {print $2}' | while read -r lib; do
    path=$(ldconfig -p | awk -v l="$lib" '$1 == l && /x86-64/ {print $NF; exit}')
    [ -n "$path" ] || { echo "no path for $lib" >&2; exit 1; }
    dpkg -S "$(readlink -f "$path")" 2>/dev/null | head -1 | cut -d: -f1 \
        || dpkg -S "$path" | head -1 | cut -d: -f1
done | sort -u | while read -r pkg; do
    # the release the binary was built on is the floor (Debian 13); SQLite needs 3.25 for
    # the hashfile's window functions
    case "$pkg" in
        libc6) echo "libc6 (>= 2.36)" ;;
        libsqlite3-0) echo "libsqlite3-0 (>= 3.25)" ;;
        *) echo "$pkg" ;;
    esac
done | paste -sd, | sed 's/,/, /g')
[ -n "$deps" ] || { echo "no dependencies found" >&2; exit 1; }

mkdir -p "$STAGE/DEBIAN"
size=$(du -sk --exclude=DEBIAN "$STAGE" | cut -f1)
cat > "$STAGE/DEBIAN/control" <<EOF
Package: rynas-oans
Version: $VERSION
Architecture: amd64
Maintainer: Marco Ziliani <508111+rylos@users.noreply.github.com>
Installed-Size: $size
Depends: $deps
Recommends: btrfs-compsize
Section: admin
Priority: optional
Homepage: https://github.com/rylos/rynas
Description: deduplication engine for rynas (oans $OANS_VERSION)
 oans $OANS_VERSION (https://github.com/martinus/oans, GPL-2.0-only) built unmodified
 from the upstream release for rynas offline deduplication. The kernel compares the
 data byte by byte before it shares anything (FIDEDUPERANGE). Installed at
 /usr/lib/rynas/oans/oans, outside PATH: it does not replace another oans or
 Debian's duperemove, and rynas schedules the runs itself.
EOF
find "$STAGE" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
DEB="$OUT/rynas-oans_${VERSION}_amd64.deb"
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$DEB" >/dev/null
(cd "$OUT" && sha256sum "$(basename "$DEB")" > "$(basename "$DEB").sha256")
chown "${HOST_UID:-0}:${HOST_GID:-0}" "$DEB" "$DEB.sha256" "$tarball" 2>/dev/null || true
echo "built $DEB (Depends: $deps)"
cat "$DEB.sha256"
