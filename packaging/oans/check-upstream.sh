#!/usr/bin/env bash
# CI watch for rynas-oans: compares versions.env with the latest oans release; exits 1 with
# what to bump when it is newer, 0 when up to date. A bump changes versions.env (version,
# commit, tarball checksum), runs build.sh and packaging/test-dedup.sh, and widens the
# tested range in crates/rynas-dedup/src/engine.rs only once that passed.
#
#   packaging/oans/check-upstream.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=versions.env
. "$HERE/versions.env"

latest=$(curl -fsSL "https://api.github.com/repos/martinus/oans/releases/latest" \
    | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1)
if [ -z "$latest" ]; then
    echo "oans: latest release not found" >&2
    exit 2
fi
if [ "$latest" != "$OANS_VERSION" ]; then
    echo "oans: pinned $OANS_VERSION, latest $latest"
    exit 1
fi
echo "oans: $OANS_VERSION (latest)"
