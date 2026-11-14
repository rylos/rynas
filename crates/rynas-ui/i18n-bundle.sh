#!/bin/sh
# Trunk post_build hook: the translations (assets/i18n, docs/i18n.md) are one file per language
# and per part. The wasm reads its parts (core.txt, module-*.txt) as one file per language,
# i18n/<code>.txt, made here, so a language costs one request. The File app keeps reading
# i18n/<code>/files.txt; notify.txt is compiled into rynasd (rynas-notify) and not served.
set -eu
cd "$TRUNK_STAGING_DIR"
[ -d i18n ] || exit 0
for d in i18n/*/; do
    [ -d "$d" ] || continue
    code=$(basename "$d")
    out="i18n/$code.txt"
    : > "$out"
    for f in "$d"core.txt "$d"module-*.txt; do
        [ -f "$f" ] || continue
        grep -v '^#' "$f" >> "$out" || true
        rm "$f"
    done
    rm -f "$d"notify.txt
    rmdir "$d" 2>/dev/null || true
done
