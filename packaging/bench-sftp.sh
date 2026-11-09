#!/usr/bin/env bash
# SFTP copy speed: OpenSSH's sftp as the baseline (per cipher), then rynas-files-worker
# running a real copy job (the same code path as the file manager), download and upload,
# with the CPU time of the worker's busiest threads, and a micro-benchmark of the ciphers
# with the library the worker encrypts with (ring).
#
# Run it as a normal user (the worker refuses root) on the NAS, against a server on the LAN:
#
#   SFTP_HOST=server.example.lan SFTP_USER=me SFTP_FILE=/data/big.iso packaging/bench-sftp.sh
#
# Variables:
#   SFTP_HOST, SFTP_USER, SFTP_FILE   server, user, absolute path of a file on it (required;
#                                     a file of a few GB gives steady numbers)
#   SFTP_PORT (22)
#   SFTP_KEY                          a private key file (no passphrase) to log in with;
#                                     read by this script's processes only, never copied
#   RYNAS_BENCH_PASSWORD              the password, when no SFTP_KEY; asked for when unset.
#                                     Passed in the environment only, never on a command
#                                     line, never stored. The baseline needs sshpass for it.
#   DEST                              local folder on disk for the download (default: a new
#                                     folder in /var/tmp). The script makes its own hidden
#                                     folder inside it and removes only that. Not tmpfs: the
#                                     disk is part of what is measured.
#   WORKER                            the worker binary (/usr/libexec/rynas/rynas-files-worker)
#   CIPHERS                           the OpenSSH baseline's ciphers, space-separated
#                                     (default: aes128-gcm@openssh.com aes256-gcm@openssh.com
#                                     chacha20-poly1305@openssh.com)
#   UPLOAD=1                          also upload the file back, as rynas-bench-<random>.tmp next
#                                     to it (removed after)
#   VERIFY=1                          the job verifies the copy (reads it back)
#   TASKSET="taskset -c 0,1"          prefix for the worker (a rough stand-in for a smaller CPU)
set -euo pipefail

: "${SFTP_HOST:?set SFTP_HOST}" "${SFTP_USER:?set SFTP_USER}" "${SFTP_FILE:?set SFTP_FILE}"
SFTP_PORT=${SFTP_PORT:-22}
WORKER=${WORKER:-/usr/libexec/rynas/rynas-files-worker}
UPLOAD=${UPLOAD:-0}
VERIFY=${VERIFY:-0}
CIPHERS=${CIPHERS:-aes128-gcm@openssh.com aes256-gcm@openssh.com chacha20-poly1305@openssh.com}
TASKSET=${TASKSET:-}
[ "$(id -u)" != 0 ] || { echo "run it as a normal user: the worker refuses root" >&2; exit 1; }
[ -x "$WORKER" ] || { echo "no worker at $WORKER (set WORKER)" >&2; exit 1; }

"$WORKER" bench-ciphers || echo "this worker has no cipher benchmark" >&2

if [ -z "${SFTP_KEY:-}" ] && [ -z "${RYNAS_BENCH_PASSWORD:-}" ]; then
    read -rsp "password of $SFTP_USER on $SFTP_HOST: " RYNAS_BENCH_PASSWORD
    echo
fi

TMP=$(mktemp -d /var/tmp/rynas-bench.XXXXXX)
WORK=
cleanup() {
    rm -rf "$WORK" "$TMP"
}
trap cleanup EXIT
[ -n "${DEST:-}" ] || DEST=$TMP
DEST=$(cd "$DEST" && pwd -P)
# everything the benchmark writes goes in a folder of its own: no file of the user's in DEST
# can have one of these names, and only this folder is removed at the end
WORK=$(mktemp -d "$DEST/.rynas-bench.XXXXXX")
NAME=${SFTP_FILE##*/}
UP_NAME=$(mktemp -u rynas-bench-XXXXXXXX.tmp)
UP_REMOTE=${SFTP_FILE%/*}/$UP_NAME

# an unknown server is recorded in the user's known_hosts the first time; a key that changed
# is refused, before any password is sent
SSH_OPTS=(-o BatchMode=no -o StrictHostKeyChecking=accept-new -P "$SFTP_PORT")
sftp_run() { # cipher, then the batch on standard input
    local cipher=$1
    if [ -n "${SFTP_KEY:-}" ]; then
        $TASKSET sftp "${SSH_OPTS[@]}" -i "$SFTP_KEY" -o "Ciphers=$cipher" -b - "$SFTP_USER@$SFTP_HOST"
    elif command -v sshpass >/dev/null; then
        SSHPASS=$RYNAS_BENCH_PASSWORD $TASKSET sshpass -e sftp "${SSH_OPTS[@]}" -o "Ciphers=$cipher" -b - "$SFTP_USER@$SFTP_HOST"
    else
        return 127
    fi
}

now() { date +%s.%N; }
mbps() { awk -v b="$1" -v t="$2" 'BEGIN { printf "%.1f MB/s", b / t / 1e6 }'; }
secs() { awk -v a="$1" -v b="$2" 'BEGIN { print b - a }'; }

SIZE=
if ! command -v sftp >/dev/null; then
    echo "sftp (openssh-client) not installed: no baseline" >&2
else
    for cipher in $CIPHERS; do
        t0=$(now)
        # to a real file in the same folder as the worker's copy: the disk is part of it
        if printf 'get "%s" "%s"\n' "$SFTP_FILE" "$WORK/.baseline-$$.tmp" | sftp_run "$cipher" >"$TMP/out" 2>&1; then
            t1=$(now)
            rm -f "$WORK/.baseline-$$.tmp"
            T=$(secs "$t0" "$t1")
            if [ -z "$SIZE" ]; then
                SIZE=$(printf 'ls -l "%s"\n' "$SFTP_FILE" | sftp_run "$cipher" 2>/dev/null | awk '$NF ~ /[^ ]/ && $5 ~ /^[0-9]+$/ { s = $5 } END { print s }')
            fi
            echo "sftp download ($cipher): $(mbps "${SIZE:-0}" "$T") (${SIZE:-?} bytes in ${T}s)"
        else
            [ $? -eq 127 ] && { echo "no sshpass: no baseline with a password (use SFTP_KEY)" >&2; break; }
            echo "sftp download ($cipher) failed: $(head -c 200 "$TMP/out")" >&2
        fi
    done
fi

json() { # a JSON string (a key file spans lines)
    local s=${1//\\/\\\\}
    s=${s//\"/\\\"}
    s=${s//$'\n'/\\n}
    printf '"%s"' "$s"
}

if [ -n "${SFTP_KEY:-}" ]; then
    AUTH="\"auth\":\"own\",\"ssh_key\":$(json "$(cat "$SFTP_KEY")")"
else
    AUTH="\"auth\":\"password\",\"password\":$(json "$RYNAS_BENCH_PASSWORD")"
fi
CONN_NOKEY="{\"id\":\"bench\",\"name\":\"bench\",\"kind\":\"sftp\",\"host\":$(json "$SFTP_HOST"),\"port\":$SFTP_PORT,\"user\":$(json "$SFTP_USER"),$AUTH}"
HOST_KEY=$(printf '{"op":"server_key","conn":%s}\n' "$CONN_NOKEY" | "$WORKER" call | sed -n 's/.*"key":"\([^"]*\)".*/\1/p')
[ -n "$HOST_KEY" ] || { echo "cannot read the server's host key" >&2; exit 1; }
CONN="${CONN_NOKEY%\}},\"host_key\":$(json "$HOST_KEY")}"
VERIFY_JSON=false
[ "$VERIFY" = 1 ] && VERIFY_JSON=true

# runs one job (its spec on standard input, like rynasd does); prints the time and the
# busiest threads' CPU
job() {
    local spec=$1 label=$2 bytes=$3
    local fifo=$TMP/in events=$TMP/events
    rm -f "$fifo"
    mkfifo "$fifo"
    # the connection's terms and any error go to the journal (here: the screen)
    $TASKSET "$WORKER" job <"$fifo" >"$events" &
    local pid=$!
    exec 4>"$fifo"
    local t0 t1 T done hz
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
    t1=$(now)
    exec 4>&-
    T=$(secs "$t0" "$t1")
    done=$(grep '"t":"done"' "$events" | tail -1)
    case $done in
    *'"status":"ok"'*) echo "rynas $label: $(mbps "$bytes" "$T") ($bytes bytes in ${T}s)" ;;
    *) echo "rynas $label failed: $done" >&2; grep '"t":"error"' "$events" | head -3 >&2; return 1 ;;
    esac
    hz=$(getconf CLK_TCK)
    for tid in "${!ticks[@]}"; do
        awk -v n="${names[$tid]}" -v k="${ticks[$tid]}" -v hz="$hz" -v t="$T" -v id="$tid" 'BEGIN { printf "%6.1f%%  %s/%s\n", k / hz / t * 100, n, id }'
    done | sort -rn | head -5 | sed 's/^/    cpu /'
}

SPEC="{\"op\":\"copy\",\"sources\":[$(json "/:bench$SFTP_FILE")],\"dest\":$(json "$WORK"),\"roots\":[$(json "$WORK")],\"conflict\":\"overwrite\",\"verify\":$VERIFY_JSON,\"remotes\":[$CONN]}"
job "$SPEC" download "${SIZE:-0}"
[ -n "$SIZE" ] || SIZE=$(stat -c %s "$WORK/$NAME")

if [ "$UPLOAD" = 1 ]; then
    mv "$WORK/$NAME" "$WORK/$UP_NAME"
    if command -v sftp >/dev/null; then
        for cipher in $CIPHERS; do
            t0=$(now)
            if printf 'put "%s" "%s"\n' "$WORK/$UP_NAME" "$UP_REMOTE" | sftp_run "$cipher" >"$TMP/out" 2>&1; then
                t1=$(now)
                echo "sftp upload ($cipher): $(mbps "$SIZE" "$(secs "$t0" "$t1")")"
            fi
            printf 'rm "%s"\n' "$UP_REMOTE" | sftp_run "$cipher" >/dev/null 2>&1 || true
        done
    fi
    DST="/:bench${SFTP_FILE%/*}"
    SPEC="{\"op\":\"copy\",\"sources\":[$(json "$WORK/$UP_NAME")],\"dest\":$(json "$DST"),\"roots\":[$(json "$WORK")],\"conflict\":\"overwrite\",\"verify\":$VERIFY_JSON,\"remotes\":[$CONN]}"
    job "$SPEC" upload "$SIZE" || true
    printf 'rm "%s"\n' "$UP_REMOTE" | sftp_run "${CIPHERS%% *}" >/dev/null 2>&1 || true
    rm -f "$WORK/$UP_NAME"
fi
