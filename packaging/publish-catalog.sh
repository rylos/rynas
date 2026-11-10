#!/usr/bin/env bash
# Builds a signed online app catalogue from apps/ (D-APPCAT, docs/phase2/docker-apps.md § 6.12)
# and lists the files to upload. It uploads nothing and never prints or stores a secret.
#
#   packaging/publish-catalog.sh [--offline] [--days N] [--out DIR] [--base-url URL]
#
#   --offline      do not read the published index: serial = apps/catalog.toml + 1
#   --days N       validity of the index (default 30; NAS refuse it afterwards)
#   --out DIR      output folder (default target/catalog-publish)
#   --base-url U   where the catalogue is published (default the official site)
#
# The minisign secret key, one of:
#   RYNAS_CATALOG_KEY=<path>        the secret key file (on a RAM folder, please)
#   RYNAS_CATALOG_KEY_CMD='<cmd>'   a command that prints the secret key file, read through
#                                   process substitution, never written to disk; e.g.
#                                   'rbw get --raw "rynas catalogue key" | jq -r .notes'
# The passphrase: minisign asks on the terminal, or
#   RYNAS_CATALOG_PASS_CMD='<cmd>'  a command that prints it, piped to minisign's standard
#                                   input; e.g. 'rbw get "rynas catalogue key"'
# Optional: RYNAS_CATALOG_PUB=<path of the public key file> verifies the new signature.
#
# Afterwards: upload index.json, index.json.minisig and apps-<serial>.tar.zst to v1/ of the
# catalogue site, and commit apps/catalog.toml (the next rynas package carries the serial).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

BASE_URL=https://rylos.github.io/rynas-catalog/v1/
DAYS=30
OUT=$ROOT/target/catalog-publish
OFFLINE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --offline) OFFLINE=1 ;;
        --days) DAYS=$2; shift ;;
        --out) OUT=$2; shift ;;
        --base-url) BASE_URL=$2; shift ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
case "$DAYS" in ''|*[!0-9]*) echo "--days must be a number" >&2; exit 2 ;; esac
if [ "$DAYS" -lt 1 ] || [ "$DAYS" -gt 365 ]; then echo "--days must be 1-365" >&2; exit 2; fi
BASE_URL=${BASE_URL%/}/

for tool in minisign zstd tar sha256sum cargo; do
    command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done
if [ -z "${RYNAS_CATALOG_KEY:-}" ] && [ -z "${RYNAS_CATALOG_KEY_CMD:-}" ]; then
    echo "set RYNAS_CATALOG_KEY or RYNAS_CATALOG_KEY_CMD (see the header)" >&2
    exit 1
fi

serial_of() { sed -n 's/^serial *= *\([0-9][0-9]*\).*/\1/p' "$1" | head -n1; }
local_serial=$(serial_of apps/catalog.toml)
[ -n "$local_serial" ] || { echo "apps/catalog.toml has no serial" >&2; exit 1; }
remote_serial=0
if [ "$OFFLINE" = 0 ]; then
    if idx=$(curl -fsS --max-time 20 "${BASE_URL}index.json" 2>/dev/null); then
        remote_serial=$(printf '%s' "$idx" | sed -n 's/.*"serial" *: *\([0-9][0-9]*\).*/\1/p' | head -n1)
        remote_serial=${remote_serial:-0}
    else
        echo "note: no published index at ${BASE_URL}index.json (first publication?)" >&2
    fi
fi
prev=$(( local_serial > remote_serial ? local_serial : remote_serial ))
serial=$(( prev + 1 ))
published=$(date -u +%Y-%m-%dT%H:%M:%SZ)
expires=$(date -u -d "+$DAYS days" +%Y-%m-%dT%H:%M:%SZ)
name=apps-$serial.tar.zst
echo "catalogue serial $serial (local $local_serial, published $remote_serial), expires $expires"

# the new serial goes into the repository first: the archive and the next package carry it
sed -i "s/^serial *= *[0-9][0-9]*/serial = $serial/" apps/catalog.toml
[ "$(serial_of apps/catalog.toml)" = "$serial" ] || { echo "could not write apps/catalog.toml" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT"
ids=()
for d in apps/*/; do ids+=("$(basename "$d")"); done
# reproducible: sorted names, fixed owners, modes and times
tar --format=gnu --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
    --mode='u=rwX,go=rX' -C apps -cf - catalog.toml "${ids[@]}" \
    | zstd -19 -q -o "$OUT/$name"

export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
cargo run -q -p rynas-apps --features online --example catalog-index -- \
    apps "$serial" "$published" "$expires" "$name" "$OUT/$name" > "$OUT/index.json"

sign() {
    minisign -S -s "$1" -m "$OUT/index.json" -x "$OUT/index.json.minisig" \
        -c "rynas app catalogue $serial" -t "serial=$serial expires=$expires"
}
sign_with_pass() {
    if [ -n "${RYNAS_CATALOG_PASS_CMD:-}" ]; then
        bash -c "$RYNAS_CATALOG_PASS_CMD" | sign "$1"
    else
        sign "$1"
    fi
}
if [ -n "${RYNAS_CATALOG_KEY_CMD:-}" ]; then
    sign_with_pass <(bash -c "$RYNAS_CATALOG_KEY_CMD")
else
    sign_with_pass "$RYNAS_CATALOG_KEY"
fi

if [ -n "${RYNAS_CATALOG_PUB:-}" ]; then
    minisign -V -q -p "$RYNAS_CATALOG_PUB" -m "$OUT/index.json" -x "$OUT/index.json.minisig"
    echo "signature verified with $RYNAS_CATALOG_PUB"
fi

echo
echo "Upload to ${BASE_URL}:"
for f in index.json index.json.minisig "$name"; do
    printf '  %s  (%s bytes, sha256 %s)\n' "$OUT/$f" "$(stat -c %s "$OUT/$f")" "$(sha256sum "$OUT/$f" | cut -d' ' -f1)"
done
echo "Then commit apps/catalog.toml (serial $serial). Publish again before $expires."
