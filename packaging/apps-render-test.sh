#!/usr/bin/env bash
# Render every catalogue entry with its tests/values.toml and validate the result with
# `docker compose config` (syntax, interpolation from .env, schema of the Compose spec).
#
#   packaging/apps-render-test.sh [<id> ...]
#
# Read-only for Docker: `docker compose config` parses files and talks to no daemon, but
# run it in CI or a throw-away machine anyway, never on a production NAS. The structural
# lint (pinned images, declared ports, privileges) is `cargo test -p rynas-apps`; this
# script adds the Compose implementation's own checks on top.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

OUT=$(mktemp -d "${TMPDIR:-/var/tmp}/rynas-apps-render.XXXXXX")
trap 'rm -rf "$OUT"' EXIT

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
cargo build -q -p rynas-apps --example render
RENDER="$CARGO_TARGET_DIR/debug/examples/render"

if [ $# -gt 0 ]; then
    ids=("$@")
else
    ids=()
    for d in apps/*/; do ids+=("$(basename "$d")"); done
fi

fail=0
for id in "${ids[@]}"; do
    dir="$OUT/$id"
    "$RENDER" apps "$id" "$dir"
    if docker compose --project-directory "$dir" -f "$dir/compose.yaml" config -q; then
        echo "ok   $id"
    else
        echo "FAIL $id"
        fail=1
    fi
done
exit $fail
