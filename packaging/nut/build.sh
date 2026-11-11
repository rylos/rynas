#!/usr/bin/env bash
# Build rynas's NUT backport for Debian 13 (trixie, amd64): Debian's own NUT source from
# forky/sid rebuilt unmodified on trixie (D-UPS-NUT, docs/research/ups.md "NUT newer than
# Debian's"). Some UPSes need a NUT newer than trixie's 2.8.1 (USB 0925:1234 needs
# nutdrv_qx subdriver=armac, NUT 2.8.5); rynasd offers this build in UPS setup only when a
# UPS needs it and no apt source has a new enough NUT (crates/rynasd/src/ups_update.rs).
#
#   packaging/nut/build.sh [--no-docker]
#
#   --no-docker   run directly (already inside the pinned builder image, e.g. as a CI job)
#
# Inputs are pinned in versions.env: the Debian source by the SHA-256 of its .dsc (which pins
# the upstream tarball and the Debian packaging; dpkg-source checks them), the builder image
# by digest, the build dependencies from a snapshot.debian.org timestamp. The only change to
# the source is one changelog entry giving the version <Debian version>~rynas13+<revision>
# (below Debian's own, so a Debian package replaces it) dated SOURCE_DATE_EPOCH, the time
# of the last commit touching packaging/nut: the same inputs give the same packages.
# Built with Debian's nodoc profile (no manual pages) and without the test suite, in a
# fixed path. The metapackage nut stays Debian's (it depends on nut-server and nut-client
# unversioned).
#
# Only the packages listed in PACKAGES are kept (upsd and the USB/serial drivers, upsmon,
# upsc, nut-scanner and their libraries): the SNMP, XML, IPMI, Modbus, I2C, PowerMan and CGI
# packages are not offered (rynasd's install refuses to remove them: apt --no-remove).
# NUT is GPL-2.0-or-later: the corresponding source (the .dsc and its files, plus this
# folder) is published next to the packages (GPL-2.0 §3(a)); target/nut/source/ gets it.
#
# Output: target/nut/<package>_<version>_<arch>.deb, target/nut/SHA256SUMS and
# target/nut/source/. Nothing is installed on the build host. Test: packaging/nut/test.sh.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# shellcheck source=versions.env
. "$HERE/versions.env"

DOCKER=1
while [ $# -gt 0 ]; do
    case "$1" in
        --no-docker|--inside) DOCKER=0 ;;
        -h|--help) sed -n '2,27p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

OUT="$ROOT/target/nut"
SRCDIR="$OUT/source"
mkdir -p "$SRCDIR"
DSC="$SRCDIR/$(basename "$NUT_DSC_URL")"

if [ "$DOCKER" = 1 ]; then
    epoch=${SOURCE_DATE_EPOCH:-$(git -C "$ROOT" log -1 --format=%ct -- packaging/nut 2>/dev/null || true)}
    [ -n "$epoch" ] || epoch=1762819200
    # the source is fetched here and checked: first the .dsc by its pinned checksum, then
    # its files by the checksums it lists (dpkg-source checks them again inside)
    base=$(dirname "$NUT_DSC_URL")
    if [ ! -f "$DSC" ]; then
        curl -fsSL -o "$DSC.part" "$NUT_DSC_URL"
        mv "$DSC.part" "$DSC"
    fi
    echo "$NUT_DSC_SHA256  $DSC" | sha256sum -c --quiet - || { echo "source .dsc checksum mismatch" >&2; exit 1; }
    sed -n '/^Checksums-Sha256:/,/^[A-Z]/p' "$DSC" | awk 'NF == 3 {print $1, $3}' | while read -r sum name; do
        case "$name" in *[!A-Za-z0-9.+~_-]*) echo "unexpected file name in the .dsc: $name" >&2; exit 1 ;; esac
        if [ ! -f "$SRCDIR/$name" ]; then
            curl -fsSL -o "$SRCDIR/$name.part" "$base/$name"
            mv "$SRCDIR/$name.part" "$SRCDIR/$name"
        fi
        echo "$sum  $SRCDIR/$name" | sha256sum -c --quiet - || { echo "$name checksum mismatch" >&2; exit 1; }
    done
    exec docker run --rm -e SOURCE_DATE_EPOCH="$epoch" -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        -v "$ROOT:/src" -w /src "$BUILDER_IMAGE" packaging/nut/build.sh --inside
fi

# ---- inside the builder image (root: it installs the build dependencies) ----
export DEBIAN_FRONTEND=noninteractive
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-1762819200}
export SOURCE_DATE_EPOCH
# the build dependencies from a fixed state of the archive, not today's mirror
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
apt-get install -y -qq --no-install-recommends build-essential dpkg-dev ca-certificates >/dev/null
echo "$NUT_DSC_SHA256  $DSC" | sha256sum -c --quiet - || { echo "source .dsc checksum mismatch" >&2; exit 1; }

# a fixed build path (it ends up in debug info and error messages)
BUILD=/build
rm -rf "$BUILD" && mkdir -p "$BUILD"
upstream=${NUT_DEBIAN_VERSION%-*}
dpkg-source -x "$DSC" "$BUILD/nut-$upstream" >/dev/null
cd "$BUILD/nut-$upstream"
[ "$(dpkg-parsechangelog -S Version)" = "$NUT_DEBIAN_VERSION" ] || { echo "unexpected source version" >&2; exit 1; }
VERSION="$NUT_DEBIAN_VERSION~rynas13+$PKG_REVISION"
{
    printf 'nut (%s) trixie; urgency=medium\n\n' "$VERSION"
    printf '  * Debian %s rebuilt unmodified for Debian 13 (trixie) for rynas\n' "$NUT_DEBIAN_VERSION"
    printf '    (packaging/nut in https://github.com/rylos/rynas).\n\n'
    printf ' -- Marco Ziliani <508111+rylos@users.noreply.github.com>  %s\n\n' "$(date -u -R -d "@$SOURCE_DATE_EPOCH")"
    cat debian/changelog
} > debian/changelog.new
mv debian/changelog.new debian/changelog

# Debian's nodoc profile: without it debian/rules asks for the PDF and HTML manuals of
# nut-doc, which need a full TeX toolchain (dblatex). The price: no manual pages in the
# packages (they are on networkupstools.org; `<driver> -h` lists a driver's options).
apt-get build-dep -y -qq --no-install-recommends -P nodoc ./ >/dev/null
DEB_BUILD_OPTIONS="nocheck nodoc parallel=$JOBS" DEB_BUILD_PROFILES=nodoc \
    dpkg-buildpackage -b -us -uc > "$BUILD/build.log" 2>&1 || { tail -n 60 "$BUILD/build.log"; exit 1; }

cd "$BUILD"
rm -f "$OUT"/*_"$NUT_DEBIAN_VERSION"~rynas13+*.deb
kept=()
for p in $PACKAGES; do
    f=$(find "$BUILD" -maxdepth 1 -name "${p}_${VERSION}_*.deb" | head -n 1)
    [ -n "$f" ] || { echo "package $p was not built" >&2; exit 1; }
    cp "$f" "$OUT/"
    kept+=("$(basename "$f")")
done
# every Depends of a kept package built from this source must be kept too
for f in "${kept[@]}"; do
    dpkg-deb -f "$OUT/$f" Depends Pre-Depends | tr ',' '\n' | sed 's/[ (|].*//; s/^ *//' | while read -r dep; do
        [ -n "$dep" ] || continue
        if ls "$BUILD/${dep}_${VERSION}_"*.deb >/dev/null 2>&1 && ! printf '%s\n' "${kept[@]}" | grep -q "^${dep}_"; then
            echo "$f depends on $dep, which is not in PACKAGES" >&2
            exit 1
        fi
    done
done
"$BUILD/nut-$upstream/drivers/nutdrv_qx" -h 2>&1 | head -n 1 || true
(cd "$OUT" && sha256sum "${kept[@]}" > SHA256SUMS)
chown -R "${HOST_UID:-0}:${HOST_GID:-0}" "$OUT" 2>/dev/null || true
echo "built in $OUT:"
cat "$OUT/SHA256SUMS"
