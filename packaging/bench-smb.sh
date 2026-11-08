#!/usr/bin/env bash
# SMB copy speed: smbclient as the baseline, then rynas-files-worker running a real copy job
# (the same code path as the file manager), download and optionally upload, with the CPU
# time of the worker's busiest thread.
#
# Run it as a normal user (the worker refuses root) on the NAS, against a server on the LAN:
#
#   SMB_HOST=nas2.example.lan SMB_SHARE=data SMB_USER=me SMB_FILE=iso/big.iso \
#       packaging/bench-smb.sh
#
# Variables:
#   SMB_HOST, SMB_SHARE, SMB_USER, SMB_FILE   server, share, user, file in the share (required;
#                                             a file of a few GB gives steady numbers)
#   SMB_PORT (445), SMB_DOMAIN ("")
#   RYNAS_BENCH_PASSWORD                      the password; asked for when unset. It is passed
#                                             on standard input / in the environment only,
#                                             never on a command line, never stored.
#   DEST                                      local folder on disk for the download (default:
#                                             a new folder in /var/tmp, removed at the end)
#   WORKER                                    the worker binary
#                                             (/usr/libexec/rynas/rynas-files-worker)
#   UPLOAD=1                                  also upload the file back, as
#                                             rynas-bench-<pid>.tmp next to it (removed after)
#   VERIFY=1                                  the job verifies the copy (reads it back)
set -euo pipefail

: "${SMB_HOST:?set SMB_HOST}" "${SMB_SHARE:?set SMB_SHARE}" "${SMB_USER:?set SMB_USER}" "${SMB_FILE:?set SMB_FILE}"
SMB_PORT=${SMB_PORT:-445}
SMB_DOMAIN=${SMB_DOMAIN:-}
WORKER=${WORKER:-/usr/libexec/rynas/rynas-files-worker}
UPLOAD=${UPLOAD:-0}
VERIFY=${VERIFY:-0}
[ "$(id -u)" != 0 ] || { echo "run it as a normal user: the worker refuses root" >&2; exit 1; }
[ -x "$WORKER" ] || { echo "no worker at $WORKER (set WORKER)" >&2; exit 1; }
command -v smbclient >/dev/null || echo "smbclient not installed: no baseline (apt install smbclient)" >&2

if [ -z "${RYNAS_BENCH_PASSWORD:-}" ]; then
    read -rsp "password of $SMB_USER on $SMB_HOST: " RYNAS_BENCH_PASSWORD
    echo
fi

TMP=$(mktemp -d /var/tmp/rynas-bench.XXXXXX)
OWN_DEST=0
if [ -z "${DEST:-}" ]; then
    DEST=$TMP/dest
    mkdir "$DEST"
    OWN_DEST=1
fi
DEST=$(cd "$DEST" && pwd -P)
NAME=${SMB_FILE##*/}
DIR_IN_SHARE=${SMB_FILE%/*}
[ "$DIR_IN_SHARE" != "$SMB_FILE" ] || DIR_IN_SHARE=
UP_NAME=rynas-bench-$$.tmp
cleanup() {
    rm -f "$DEST/$NAME" "$DEST/.$NAME.rynas-part"
    [ "$OWN_DEST" = 1 ] && rm -rf "$DEST"
    rm -rf "$TMP"
}
trap cleanup EXIT

smb() { # smbclient with the password from the environment, never on the command line
    PASSWD=$RYNAS_BENCH_PASSWORD smbclient -p "$SMB_PORT" "//$SMB_HOST/$SMB_SHARE" -U "${SMB_DOMAIN:+$SMB_DOMAIN\\}$SMB_USER" "$@"
}

json() { # a JSON string
    local s=${1//\\/\\\\}
    s=${s//\"/\\\"}
    printf '"%s"' "$s"
}

now() { date +%s.%N; }
mbps() { awk -v b="$1" -v t="$2" 'BEGIN { printf "%.1f MB/s", b / t / 1e6 }'; }

# size of the file on the server
# (ls: "  name  A  size  Sun Nov  8 18:37:27 2026")
SIZE=$(smb -c "ls \"${SMB_FILE//\//\\}\"" 2>/dev/null | awk 'NF >= 7 && $(NF-5) ~ /^[0-9]+$/ { print $(NF-5); exit }' || true)

if command -v smbclient >/dev/null; then
    t0=$(now)
    smb -c "get \"${SMB_FILE//\//\\}\" /dev/null" >/dev/null 2>&1
    t1=$(now)
    T=$(awk -v a="$t0" -v b="$t1" 'BEGIN { print b - a }')
    [ -n "$SIZE" ] && echo "smbclient download: $(mbps "$SIZE" "$T") ($SIZE bytes in ${T}s)"
fi

CONN="{\"id\":\"bench\",\"name\":\"bench\",\"kind\":\"smb\",\"host\":$(json "$SMB_HOST"),\"port\":$SMB_PORT,\"domain\":$(json "$SMB_DOMAIN"),\"user\":$(json "$SMB_USER"),\"password\":$(json "$RYNAS_BENCH_PASSWORD")}"
VERIFY_JSON=false
[ "$VERIFY" = 1 ] && VERIFY_JSON=true

# runs one job (its spec on standard input, like rynasd does); prints the time and the
# busiest threads' CPU
job() {
    local spec=$1 label=$2 bytes=$3
    local fifo=$TMP/in events=$TMP/events
    rm -f "$fifo"
    mkfifo "$fifo"
    "$WORKER" job <"$fifo" >"$events" &
    local pid=$!
    exec 4>"$fifo"
    local t0
    t0=$(now)
    printf '%s\n' "$spec" >&4
    declare -A ticks=() names=()
    while kill -0 "$pid" 2>/dev/null; do
        for st in /proc/"$pid"/task/*/stat; do
            local line
            line=$(cat "$st" 2>/dev/null) || continue
            local tid=${line%% *} comm=${line#*(}
            comm=${comm%)*}
            local rest=${line##*) }
            # fields after the name: state(3) … utime(14) stime(15)
            set -- $rest
            ticks[$tid]=$(( ${12} + ${13} ))
            names[$tid]=$comm
        done
        sleep 0.25
    done
    wait "$pid" || true
    local t1
    t1=$(now)
    exec 4>&-
    local T
    T=$(awk -v a="$t0" -v b="$t1" 'BEGIN { print b - a }')
    local done
    done=$(grep '"t":"done"' "$events" | tail -1)
    case $done in
    *'"status":"ok"'*) echo "rynas $label: $(mbps "$bytes" "$T") ($bytes bytes in ${T}s)" ;;
    *) echo "rynas $label failed: $done" >&2; grep '"t":"error"' "$events" | head -3 >&2; return 1 ;;
    esac
    local hz
    hz=$(getconf CLK_TCK)
    for tid in "${!ticks[@]}"; do
        awk -v n="${names[$tid]}" -v k="${ticks[$tid]}" -v hz="$hz" -v t="$T" -v id="$tid" 'BEGIN { printf "%6.1f%%  %s/%s\n", k / hz / t * 100, n, id }'
    done | sort -rn | head -4 | sed 's/^/    cpu /'
}

SRC="/:bench/$SMB_SHARE/$SMB_FILE"
SPEC="{\"op\":\"copy\",\"sources\":[$(json "$SRC")],\"dest\":$(json "$DEST"),\"roots\":[$(json "$DEST")],\"conflict\":\"overwrite\",\"verify\":$VERIFY_JSON,\"remotes\":[$CONN]}"
rm -f "$DEST/$NAME"
job "$SPEC" download "${SIZE:-0}"
[ -n "$SIZE" ] || SIZE=$(stat -c %s "$DEST/$NAME")

if [ "$UPLOAD" = 1 ]; then
    mv "$DEST/$NAME" "$DEST/$UP_NAME"
    if command -v smbclient >/dev/null; then
        t0=$(now)
        smb -c "put \"$DEST/$UP_NAME\" \"${DIR_IN_SHARE:+${DIR_IN_SHARE//\//\\}\\}$UP_NAME\"" >/dev/null 2>&1
        t1=$(now)
        echo "smbclient upload: $(mbps "$SIZE" "$(awk -v a="$t0" -v b="$t1" 'BEGIN { print b - a }')")"
        smb -c "del \"${DIR_IN_SHARE:+${DIR_IN_SHARE//\//\\}\\}$UP_NAME\"" >/dev/null 2>&1 || true
    fi
    DST="/:bench/$SMB_SHARE${DIR_IN_SHARE:+/$DIR_IN_SHARE}"
    SPEC="{\"op\":\"copy\",\"sources\":[$(json "$DEST/$UP_NAME")],\"dest\":$(json "$DST"),\"roots\":[$(json "$DEST")],\"conflict\":\"overwrite\",\"verify\":$VERIFY_JSON,\"remotes\":[$CONN]}"
    job "$SPEC" upload "$SIZE" || true
    rm -f "$DEST/$UP_NAME"
    command -v smbclient >/dev/null && smb -c "del \"${DIR_IN_SHARE:+${DIR_IN_SHARE//\//\\}\\}$UP_NAME\"" >/dev/null 2>&1 || true
fi
