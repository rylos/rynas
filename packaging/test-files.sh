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
# closed to bob (permission_denied) and readable and writable for carol (the administrator
# keeps CAP_DAC_READ_SEARCH, CAP_DAC_OVERRIDE and CAP_FOWNER under Landlock; what she makes
# stays hers, alice's files stay alice's; without Landlock ABI 3 in the container she
# reads only); uploads, copies and the recycle bin run as the user; the worker refuses to
# run as root and holds nothing beyond its level; symbolic links never lead an
# administrator's worker out of the share (/etc/shadow).
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
# carol, administrator: reads everything; writes over permissions where Landlock (ABI 3)
# fences her worker to the shares, else only where her permissions allow
[ "$(code carol GET '/v1/files/list?path=/docs/alice-only')" = 200 ] || fail "carol cannot read alice's folder"
[ "$(api carol GET '/v1/files/text?path=/docs/alice-only/plan.txt' | jq -r .text)" = secret ] || fail "carol cannot read alice's file"
FENCED=yes
grep -q 'administrators write as themselves' /var/log/rynasd.log && FENCED=no
echo "administrator's worker fenced by Landlock: $FENCED"
if [ "$FENCED" = yes ]; then
    # the listing says so (the interface shows Delete, Rename, Cut), and it is true
    api carol GET '/v1/files/list?path=/docs/alice-only' | jq -e '.writable == true' >/dev/null || fail "alice's folder not writable for carol"
    [ "$(code carol POST '/v1/files/mkdir?path=/docs/alice-only&name=x' '{}')" = 200 ] || fail "carol cannot write into alice's folder"
    [ "$(stat -c %U /srv/vol1/docs/alice-only/x)" = carol ] || fail "what carol made is not hers"
    [ "$(code carol POST '/v1/files/rename?path=/docs/alice-only/plan.txt&name=plan2.txt' '{}')" = 200 ] || fail "carol cannot rename alice's file"
    [ "$(stat -c %U /srv/vol1/docs/alice-only/plan2.txt)" = alice ] || fail "alice's file changed owner"
    [ "$(code carol POST '/v1/files/rename?path=/docs/alice-only/plan2.txt&name=plan.txt' '{}')" = 200 ] || fail "rename back"
    rmdir /srv/vol1/docs/alice-only/x
    # bob, not an administrator: unchanged (alice's 0755 folder is not his to write in)
    api bob GET '/v1/files/list?path=/docs/common/by-alice' | jq -e '.writable == false' >/dev/null || fail "bob may write in alice's folder"
    [ "$(code bob POST '/v1/files/mkdir?path=/docs/alice-only&name=x' '{}')" = 403 ] || fail "bob writes into alice's folder"
else
    [ "$(code carol POST '/v1/files/mkdir?path=/docs/alice-only&name=x' '{}')" = 403 ] || fail "carol writes into alice's folder"
fi
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

step "links stay inside their share (wide links = no)"
ln -s /etc /srv/vol1/docs/common/etc
ln -s ../../../../etc/shadow /srv/vol1/docs/common/up
mkdir -p /srv/vol1/docs/common/real && echo inside > /srv/vol1/docs/common/real/f.txt
ln -s real /srv/vol1/docs/common/in
# carol keeps CAP_DAC_READ_SEARCH: through a link she would read /etc/shadow
for path in /docs/common/etc/shadow /docs/common/up; do
    api carol GET "/v1/files/text?path=$path" | jq -e '.code == "outside_share"' >/dev/null || fail "carol reads $path"
    [ "$(code carol GET "/v1/files/download?path=$path")" = 403 ] || fail "carol downloads $path"
done
[ "$(code carol GET '/v1/files/list?path=/docs/common/etc')" = 403 ] || fail "carol lists /etc through a link"
api carol GET '/v1/files/list?path=/docs/common' | jq -e '.entries[] | select(.name=="etc") | .outside == true' >/dev/null \
    || fail "the link is not marked outside"
curl -s --unix-socket "$SOCK" -H 'x-rynas-user: carol' -o /tmp/common.zip \
    'http://rynasd/v1/files/download?paths=%5B%22%2Fdocs%2Fcommon%22%5D'
if grep -q 'root:[*!]' /tmp/common.zip; then fail "/etc/shadow inside the zip"; fi
api carol POST /v1/files/jobs/new '{"op":"copy","sources":["/docs/common/etc/shadow"],"dest":"/docs/common/real"}' >/dev/null
sleep 2
[ ! -e /srv/vol1/docs/common/real/shadow ] || fail "a job copied /etc/shadow"
# a link inside the share works as before
[ "$(api carol GET '/v1/files/text?path=/docs/common/in/f.txt' | jq -r .text)" = inside ] || fail "a link inside the share"
rm -rf /srv/vol1/docs/common/etc /srv/vol1/docs/common/up /srv/vol1/docs/common/in /srv/vol1/docs/common/real

step "the worker itself"
if /opt/rynas/rynas-files-worker serve </dev/null 2>/dev/null; then fail "the worker ran as root"; fi
# a worker started by rynasd: reading and writing over owners for carol (reading only
# without Landlock), reading alone ambient; nothing for bob
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
    amb_want=0000000000000000
    if [ "$who" = carol ]; then
        want=0000000000000004
        [ "$FENCED" = yes ] && want=000000000000000e
        amb_want=0000000000000004
    fi
    [ "$eff" = "$want" ] || fail "$who's worker has CapEff $eff"
    amb=$(awk '/^CapAmb/ {print $2}' "/proc/$pid/status")
    [ "$amb" = "$amb_want" ] || fail "$who's worker has CapAmb $amb"
    grep -q '^NoNewPrivs:[[:space:]]*1' "/proc/$pid/status" || fail "no_new_privs not set"
    kill "$client" 2>/dev/null || true
    wait "$client" 2>/dev/null || true
    sleep 0.5
done

step "OK"
