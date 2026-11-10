#!/usr/bin/env bash
# Renovate post-upgrade task (.github/renovate-config.js): for every catalogue entry whose
# compose.yaml the update changed, `app_version` in app.toml follows the tag of the main
# service's image and `version` (the catalogue entry's own version) gets a patch bump over
# the base branch, so NAS see the entry as an update (docs/phase2/docker-apps.md § 6.13).
# Idempotent: run once per branch, it bumps from the base version, never twice.
set -euo pipefail
cd "$(dirname "$0")/.."

changed=$(git diff --name-only HEAD -- 'apps/*/compose.yaml' | sed -n 's#^apps/\([^/]*\)/compose\.yaml$#\1#p')
for id in $changed; do
    toml=apps/$id/app.toml
    compose=apps/$id/compose.yaml
    [ -f "$toml" ] || continue
    main=$(sed -n 's/^main_service *= *"\([^"]*\)".*/\1/p' "$toml" | head -n1)
    # the image of the main service: the first `image:` line after `  <main>:` under services
    tag=$(awk -v svc="$main" '
        /^services:/ { in_s = 1; next }
        in_s && /^[^ #]/ { in_s = 0 }
        in_s && $0 ~ "^  " svc ":[[:space:]]*$" { want = 1; next }
        in_s && want && /^  [^ #]/ { want = 0 }
        in_s && want && /^[[:space:]]+image:/ {
            img = $2; sub(/@.*/, "", img); n = split(img, p, ":")
            if (n > 1 && p[n] !~ /\//) print p[n]
            exit
        }' "$compose")
    base=$(git show "HEAD:$toml" 2>/dev/null | sed -n 's/^version *= *"\([0-9]*\)\.\([0-9]*\)\.\([0-9]*\)".*/\1 \2 \3/p' | head -n1)
    if [ -n "$base" ]; then
        read -r ma mi pa <<<"$base"
        sed -i "s/^version *= *\"[^\"]*\"/version = \"$ma.$mi.$((pa + 1))\"/" "$toml"
    fi
    if [ -n "$tag" ]; then
        sed -i "s/^app_version *= *\"[^\"]*\"/app_version = \"$tag\"/" "$toml"
    fi
    echo "$id: version $(sed -n 's/^version *= *"\(.*\)"/\1/p' "$toml"), app_version ${tag:-unchanged}"
done
