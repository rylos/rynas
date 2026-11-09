#!/bin/bash
# Copies a trunk build and compresses it as the .deb does (packaging/build-deb.sh), so that the
# measured instance serves what a NAS serves. Usage: perf_stage.sh <dist> <staged-dir>
set -e
SRC=$1; DST=$(realpath -m "$2")
rm -rf "$DST"; mkdir -p "$DST"
(cd "$SRC" && find . -type f ! -path '*/.*' ! -name '*-LICENSE.txt' ! -name '*-OFL.txt' -print0 | cpio -0 -pdm --quiet "$DST")
find "$DST" -type f \( -name '*.wasm' -o -name '*.js' -o -name '*.css' -o -name '*.svg' -o -name '*.json' -o -name '*.txt' \) -print0 |
  xargs -0 -P 4 -I{} bash -c 'f="$1"; brotli -q 11 -k -f "$f"; gzip -9 -n -k -f "$f"; [ "$(stat -c %s "$f.br")" -lt "$(stat -c %s "$f")" ] || rm -f "$f.br"; [ "$(stat -c %s "$f.gz")" -lt "$(stat -c %s "$f")" ] || rm -f "$f.gz"' _ {}
du -sh "$DST"; ls -la "$DST"/*.wasm*
