#!/usr/bin/env bash
# CI watch for rynas's NUT backport: compares versions.env with the NUT versions Debian has.
# Exits 1 with what to do when sid has a newer NUT than the pinned one, or when trixie (or
# trixie-backports) itself reaches the pinned version (then the backport can go: rynasd
# prefers any apt source with a new enough NUT); 0 when nothing changed.
# A bump changes versions.env (version, .dsc URL and checksum, snapshot), runs build.sh and
# packaging/nut/test.sh, and updates PINNED_DEBS in crates/rynasd/src/ups_update.rs.
#
#   packaging/nut/check-upstream.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=versions.env
. "$HERE/versions.env"

# madison-lite style answers: "nut | 2.8.1-5+deb13u1 | trixie | source"
versions=$(curl -fsSL "https://qa.debian.org/madison.php?package=nut&table=debian&a=source&text=on")
[ -n "$versions" ] || { echo "nut: Debian versions not found" >&2; exit 2; }
ver_of() { printf '%s\n' "$versions" | awk -F'|' -v s="$1" '{gsub(/ /, "", $2); gsub(/ /, "", $3)} $3 == s {print $2}' | sort -V | tail -n 1; }
sid=$(ver_of sid)
rc=0
if [ -n "$sid" ] && dpkg --compare-versions "$sid" gt "$NUT_DEBIAN_VERSION"; then
    echo "nut: pinned $NUT_DEBIAN_VERSION, sid has $sid"
    rc=1
fi
for s in trixie trixie-security trixie-updates trixie-backports; do
    v=$(ver_of "$s")
    if [ -n "$v" ] && dpkg --compare-versions "$v" ge "$NUT_DEBIAN_VERSION"; then
        echo "nut: $s has $v: the rynas backport is no longer needed"
        rc=1
    fi
done
[ "$rc" = 1 ] || echo "nut: $NUT_DEBIAN_VERSION (sid has ${sid:-?}, trixie has $(ver_of trixie))"
exit "$rc"
