#!/usr/bin/env bash
# The file manager's identity switch for real, in a throw-away Debian 13 container: rynasd
# runs as root (configuration in --dry-system: only the switch matters here) and starts
# rynas-files-worker as the signed-in user for every request and job.
#
#   packaging/test-files.sh [--no-build]
#
# Builds debug rynasd and rynas-files-worker against trixie's glibc (builder image of
# build-deb.sh), then inside the container: real users alice and bob and the administrator
# carol, a share whose folders belong to them, and the internal API over the Unix socket.
# Checks: what a worker creates belongs to the user who asked; a folder of alice's is
# closed to bob (permission_denied) and readable but not writable for carol (the
# administrator keeps CAP_DAC_READ_SEARCH only); uploads, copies and the recycle bin run
# as the user; the worker refuses to run as root and holds no other capability.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
NAME=rynas-test-files-$$
BIN=$ROOT/target/deb-build/debug

if [ "${1:-}" != "--inside" ]; then
    if [ "${1:-}" != "--no-build" ]; then
        docker build -q -t rynas-deb-builder:trixie --build-arg RUST_IMAGE=rust:1.98-trixie \
            -f "$ROOT/packaging/deb/builder.Dockerfile" "$ROOT/packaging/deb" >/dev/null
        mkdir -p "$ROOT/target/deb-build"
        docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e CARGO_HOME=/tmp/cargo \
            -e PATH=/usr/local/cargo/bin:/usr/bin:/bin -e CARGO_TARGET_DIR=/src/target/deb-build \
            -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" \
            -v "$HOME/.cargo/registry:/tmp/cargo/registry" -v "$ROOT:/src" -w /src \
            rynas-deb-builder:trixie sh -c 'cargo build --locked -p rynasd && cargo build --locked --manifest-path crates/rynas-files-worker/Cargo.toml'
    fi
    # no systemd needed; the container's root plus CAP_DAC_READ_SEARCH, which rynasd.service
    # grants and Docker leaves out by default
    docker run -d --name "$NAME" --cap-add DAC_READ_SEARCH -v "$BIN/rynasd:/opt/rynas/rynasd:ro" \
        -v "$BIN/rynas-files-worker:/opt/rynas/rynas-files-worker:ro" \
        -v "$ROOT/packaging:/opt/rynas/packaging:ro" debian:trixie sleep infinity >/dev/null
    trap 'docker rm -f "$NAME" >/dev/null' EXIT
    docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash /opt/rynas/packaging/test-files.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
step() { printf '\n=== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; tail -40 /var/log/rynasd.log >&2; exit 1; }

step "packages and accounts"
apt-get update -qq
apt-get install -y -qq --no-install-recommends curl jq passwd procps >/dev/null 2>&1
groupadd -g 990 rynas-admin
useradd -m -u 2001 -U alice
useradd -m -u 2002 -U bob
useradd -m -u 2003 -U -G rynas-admin carol
mkdir -p /srv/vol1/docs/alice-only /srv/vol1/docs/common
chown alice:alice /srv/vol1/docs/alice-only && chmod 0700 /srv/vol1/docs/alice-only
echo secret > /srv/vol1/docs/alice-only/plan.txt && chown alice:alice /srv/vol1/docs/alice-only/plan.txt
chmod 0777 /srv/vol1/docs/common
# writable for everyone, as a share root with rynas's ACL matrix is for its users (the
# recycle bin is made there by the first deletion)
chmod 0777 /srv/vol1/docs

step "rynasd as root"
SOCK=/run/rynasd-test.sock
/opt/rynas/rynasd --socket "$SOCK" --state-dir /var/lib/rynas-test --dry-system --allow-uid 0 \
    --files-worker /opt/rynas/rynas-files-worker >/var/log/rynasd.log 2>&1 &
for _ in $(seq 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || fail "rynasd did not start"
api() { # user method path [body]
    local user=$1 method=$2 path=$3 body=${4:-}
    curl -s --unix-socket "$SOCK" -X "$method" -H "x-rynas-user: $user" \
        -H 'content-type: application/json' ${body:+--data-binary "$body"} "http://rynasd$path"
}
code() { # user method path [body] -> HTTP status
    local user=$1 method=$2 path=$3 body=${4:-}
    curl -s -o /dev/null -w '%{http_code}' --unix-socket "$SOCK" -X "$method" -H "x-rynas-user: $user" \
        ${body:+--data-binary "$body"} "http://rynasd$path"
}

step "a share, applied"
api carol POST /v1/shares '{"name":"docs","path":"/srv/vol1/docs","description":"Docs"}' >/dev/null
api carol POST /v1/config/apply '{"comment":"files test"}' | jq -e .task_id >/dev/null || fail "apply"
for _ in $(seq 50); do
    api carol GET /v1/files/init | jq -e '.shares.shares[] | select(.name=="docs")' >/dev/null && break
    sleep 0.2
done
api alice GET /v1/files/init | jq -e '.shares.shares[] | select(.name=="docs") | .readable' >/dev/null \
    || fail "alice does not see the share: $(api alice GET /v1/files/init)"

step "what a worker creates is the user's"
[ "$(code alice POST '/v1/files/mkdir?path=/docs/common&name=by-alice' '{}')" = 200 ] || fail "mkdir as alice"
[ "$(stat -c %U:%G /srv/vol1/docs/common/by-alice)" = alice:alice ] || fail "owner $(stat -c %U:%G /srv/vol1/docs/common/by-alice)"
printf 'hello' > /tmp/up.bin
curl -s --unix-socket "$SOCK" -X POST -H 'x-rynas-user: bob' --data-binary @/tmp/up.bin \
    "http://rynasd/v1/files/upload?dir=/docs/common&name=up.bin&id=abcdefgh12&total=5" | jq -e . >/dev/null || fail "upload"
[ "$(stat -c %U /srv/vol1/docs/common/up.bin)" = bob ] || fail "upload owner"

step "permissions are the kernel's"
[ "$(code bob GET '/v1/files/list?path=/docs/alice-only')" = 403 ] || fail "bob reads alice's folder"
api bob GET '/v1/files/list?path=/docs/alice-only' | jq -e '.code == "permission_denied"' >/dev/null || fail "code"
[ "$(code alice GET '/v1/files/list?path=/docs/alice-only')" = 200 ] || fail "alice reads her folder"
# carol, administrator: reads everything, writes only where her permissions allow
[ "$(code carol GET '/v1/files/list?path=/docs/alice-only')" = 200 ] || fail "carol cannot read alice's folder"
[ "$(api carol GET '/v1/files/text?path=/docs/alice-only/plan.txt' | jq -r .text)" = secret ] || fail "carol cannot read alice's file"
[ "$(code carol POST '/v1/files/mkdir?path=/docs/alice-only&name=x' '{}')" = 403 ] || fail "carol writes into alice's folder"
[ "$(code bob POST '/v1/files/rename?path=/docs/common/by-alice&name=y' '{}')" = 200 ] || fail "rename in a 0777 folder"

step "jobs run as the user too"
api alice POST /v1/files/jobs/new '{"op":"copy","sources":["/docs/alice-only/plan.txt"],"dest":"/docs/common"}' | jq -e .id >/dev/null || fail "copy job"
for _ in $(seq 100); do [ -f /srv/vol1/docs/common/plan.txt ] && break; sleep 0.1; done
[ "$(stat -c %U /srv/vol1/docs/common/plan.txt)" = alice ] || fail "copy owner"
# bob cannot copy alice's file: the job reports the error, nothing is written
api bob POST /v1/files/jobs/new '{"op":"copy","sources":["/docs/alice-only/plan.txt"],"dest":"/docs/common/by-alice"}' >/dev/null
sleep 2
[ ! -e /srv/vol1/docs/common/y/plan.txt ] || fail "bob copied alice's file"
api alice POST /v1/files/jobs/new '{"op":"delete","sources":["/docs/common/plan.txt"]}' >/dev/null
for _ in $(seq 100); do [ -f "/srv/vol1/docs/#recycle/common/plan.txt" ] && break; sleep 0.1; done
[ -f "/srv/vol1/docs/#recycle/common/plan.txt" ] || fail "not in the recycle bin"
api alice GET /v1/tasks | jq -e '[.[] | select(.type=="files.delete" and .user=="alice")] | length > 0' >/dev/null \
    || fail "no files.delete task"

step "the worker itself"
if /opt/rynas/rynas-files-worker serve </dev/null 2>/dev/null; then fail "the worker ran as root"; fi
# a worker started by rynasd: no capability but reading for carol, nothing for bob
# (shown by the kernel while a long answer streams)
head -c 20000000 /dev/zero > /srv/vol1/docs/common/big.bin
for who in carol bob; do
    curl -s --limit-rate 200k --unix-socket "$SOCK" -H "x-rynas-user: $who" \
        "http://rynasd/v1/files/download?path=/docs/common/big.bin" -o /dev/null &
    client=$!
    sleep 1
    pid=$(pgrep -n -f 'rynas-files-worker serve') || fail "no worker running"
    grep -E 'Uid|Gid|CapEff|CapPrm|CapAmb|NoNewPrivs' "/proc/$pid/status"
    eff=$(awk '/^CapEff/ {print $2}' "/proc/$pid/status")
    want=0000000000000000
    [ "$who" = carol ] && want=0000000000000004
    [ "$eff" = "$want" ] || fail "$who's worker has CapEff $eff"
    grep -q '^NoNewPrivs:[[:space:]]*1' "/proc/$pid/status" || fail "no_new_privs not set"
    kill "$client" 2>/dev/null || true
    wait "$client" 2>/dev/null || true
    sleep 0.5
done

step "OK"
