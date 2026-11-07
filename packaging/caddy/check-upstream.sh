#!/usr/bin/env bash
# CI watch for rynas-caddy (docs/research/web-access.md §11: rebuild within days of an
# upstream release, security SLA). Compares versions.env with the latest stable releases of
# Caddy, xcaddy and Go; exits 1 with what to bump when one is newer, 0 when up to date.
#
#   packaging/caddy/check-upstream.sh
#
# A CI job runs it daily; on exit 1 it opens an issue (or a pull request bumping
# versions.env, then build.sh --validate crates/rynasd/tests/golden/web_access_caddy.json).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=versions.env
. "$HERE/versions.env"

latest_gh() {
    curl -fsSL "https://api.github.com/repos/$1/releases/latest" \
        | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -1
}

caddy=$(latest_gh caddyserver/caddy)
xcaddy=$(latest_gh caddyserver/xcaddy)
go=$(curl -fsSL 'https://go.dev/dl/?mode=json' | sed -n 's/.*"version": *"go\([^"]*\)".*/\1/p' | head -1)

stale=0
check() {
    if [ -n "$3" ] && [ "$2" != "$3" ]; then
        echo "$1: pinned $2, latest $3"
        stale=1
    else
        echo "$1: $2 (latest)"
    fi
}
check caddy "$CADDY_VERSION" "$caddy"
check xcaddy "$XCADDY_VERSION" "$xcaddy"
check go "$GO_VERSION" "$go"
exit "$stale"
