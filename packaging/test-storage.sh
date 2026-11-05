#!/usr/bin/env bash
# The storage write side for real, on loop files, in a throw-away privileged Debian 13
# container (md, dm-crypt and loop devices are kernel objects: the container needs the
# host's /dev). Never touches a real disk: rynasd sees loop devices as disks only with the
# test flag --storage-loop-disks, every destructive request names the loop devices it
# erases, and rynas refuses devices the kernel reports in use (O_EXCL).
#
#   packaging/test-storage.sh [--no-build]
#
# Builds debug rynasd/rynas-web against trixie's glibc (builder image of build-deb.sh), then
# inside the container (no systemd: mounts fall back to mount/umount from /etc/fstab):
# installs mdadm, cryptsetup, systemd-cryptsetup, btrfs-progs, gdisk; logs in over HTTP
# (session + CSRF) and through the public API: creates a RAID1 volume, subvolumes (with and
# without compression, read-only, refused and confirmed deletes), a scrub with md check, a
# hot replace of one member, an encrypted RAID1 (recovery key revealed once, LUKS header
# backup downloaded and checked), lock, unlock with a wrong passphrase and with the recovery
# key, refusals, and a failing mkfs rolled back (array stopped, partitions removed). Results
# are checked with mdadm, btrfs, cryptsetup, lsblk, findmnt and the files in /etc.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-storage:trixie
NAME=rynas-test-storage-$$
BIN=$ROOT/target/deb-build/debug

if [ "${1:-}" != "--inside" ] && [ "${1:-}" != "--cleanup" ]; then
    if [ "${1:-}" != "--no-build" ]; then
        docker build -q -t rynas-deb-builder:trixie --build-arg RUST_IMAGE=rust:1.98-trixie \
            -f "$ROOT/packaging/deb/builder.Dockerfile" "$ROOT/packaging/deb" >/dev/null
        mkdir -p "$ROOT/target/deb-build"
        docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e CARGO_HOME=/tmp/cargo \
            -e PATH=/usr/local/cargo/bin:/usr/bin:/bin -e CARGO_TARGET_DIR=/src/target/deb-build \
            -v "$HOME/.cargo/registry:/tmp/cargo/registry" -v "$ROOT:/src" -w /src \
            rynas-deb-builder:trixie cargo build --locked -p rynasd -p rynas-web
    fi
    docker build -q -t "$IMAGE" - >/dev/null <<'DOCKERFILE'
FROM debian:trixie
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      mdadm cryptsetup-bin systemd-cryptsetup btrfs-progs gdisk util-linux e2fsprogs \
      udev curl jq ca-certificates \
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE
    # files are copied, not bind-mounted: `btrfs filesystem show --mounted` gives up on
    # read-only bind mounts of files from a btrfs host
    docker run -d --name "$NAME" --privileged -v /dev:/dev --tmpfs /run --tmpfs /tmp \
        "$IMAGE" sleep infinity >/dev/null
    docker exec "$NAME" mkdir -p /opt/rynas
    docker cp -q "$BIN/rynasd" "$NAME:/opt/rynas/rynasd"
    docker cp -q "$BIN/rynas-web" "$NAME:/opt/rynas/rynas-web"
    docker cp -q "$ROOT/packaging" "$NAME:/opt/rynas/packaging"
    # whatever happens: arrays stopped, mappings closed, loop devices detached
    trap 'docker exec "$NAME" bash /opt/rynas/packaging/test-storage.sh --cleanup >/dev/null 2>&1 || true; docker rm -f "$NAME" >/dev/null' EXIT
    docker exec -i -e HOLD="${HOLD:-}" "$NAME" bash /opt/rynas/packaging/test-storage.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
LOOPS=/var/tmp/rynas-loops

cleanup() {
    for m in rynas-rytest1 rynas-rytest2 rynas-rytest3; do
        umount -q "/${m#rynas-}" 2>/dev/null || true
        cryptsetup close "$m" 2>/dev/null || true
    done
    umount -q /rytest1 /rytest2 /rytest3 2>/dev/null || true
    for md in /dev/md/rytest1 /dev/md/rytest2 /dev/md/rytest3; do
        [ -e "$md" ] && mdadm --stop "$md" 2>/dev/null || true
    done
    # only the loop devices of our files
    for dev in $(losetup -a | grep "$LOOPS/" | cut -d: -f1); do
        for p in "$dev"p*; do
            [ -e "$p" ] && mdadm --zero-superblock "$p" 2>/dev/null || true
            [ -e "$p" ] && wipefs -q -a "$p" 2>/dev/null || true
        done
        losetup -d "$dev" 2>/dev/null || true
    done
}
if [ "${1:-}" = "--cleanup" ]; then
    pkill -f /usr/libexec/rynas/rynas || true
    cleanup
    exit 0
fi

step() { printf '\n=== %s\n' "$*"; }
fail() {
    echo "FAIL: $*" >&2
    tail -30 /var/log/rynasd.log >&2 || true
    # debugging: HOLD=1 keeps the container (and the devices) for docker exec
    [ -n "${HOLD:-}" ] && sleep 900
    exit 1
}
trap cleanup EXIT

step "versions"
for p in mdadm cryptsetup-bin systemd-cryptsetup btrfs-progs gdisk util-linux; do
    dpkg-query -W -f '${Package} ${Version}\n' "$p"
done
uname -r

step "loop devices (1 GiB files)"
mkdir -p "$LOOPS"
declare -A L
for n in 0 1 2 3 4 5; do
    rm -f "$LOOPS/d$n.img"
    truncate -s 1G "$LOOPS/d$n.img"
    dev=$(losetup -f --show -P "$LOOPS/d$n.img")
    L[$n]=${dev#/dev/}
done
echo "loops: ${L[*]}"

step "rynasd and rynas-web (root, no systemd)"
install -d /usr/libexec/rynas /var/lib/rynas /run/rynas /etc/mdadm
install -m 755 /opt/rynas/rynasd /opt/rynas/rynas-web /usr/libexec/rynas/
touch /etc/fstab /etc/crypttab
HASH=$(printf 'Adm1n-test-pw\n' | /usr/libexec/rynas/rynas-web hash-password)
printf '[[user]]\nname = "admin1"\nhash = "%s"\ngroups = ["rynas-admin"]\n' "$HASH" >/etc/rynas-users.toml
/usr/libexec/rynas/rynasd --socket /run/rynas/rynasd.sock --state-dir /var/lib/rynas \
    --storage-loop-disks >/var/log/rynasd.log 2>&1 &
for _ in $(seq 50); do [ -S /run/rynas/rynasd.sock ] && break; sleep 0.1; done
/usr/libexec/rynas/rynas-web --listen 127.0.0.1:8080 --rynasd-socket /run/rynas/rynasd.sock \
    --users-file /etc/rynas-users.toml >/var/log/rynas-web.log 2>&1 &
for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:8080/ && break; sleep 0.1; done

B=http://127.0.0.1:8080/api/v1
curl -s -c /tmp/jar -H 'Content-Type: application/json' -o /tmp/session.json \
    -d '{"username":"admin1","password":"Adm1n-test-pw"}' "$B/auth/login"
CSRF=$(jq -r .csrf_token /tmp/session.json)
[ -n "$CSRF" ] && [ "$CSRF" != null ] || fail "login: $(cat /tmp/session.json)"
echo "logged in as admin1"

# api METHOD PATH [JSON] -> body in $BODY, HTTP status in $CODE
api() {
    local out
    out=$(curl -s -b /tmp/jar -H "X-CSRF-Token: $CSRF" -H 'Content-Type: application/json' \
        -X "$1" ${3:+-d "$3"} -w '\n%{http_code}' "$B$2")
    CODE=${out##*$'\n'}
    BODY=${out%$'\n'*}
}
expect() { [ "$CODE" = "$1" ] || fail "$2: HTTP $CODE $BODY"; }
wait_task() {
    local id=$1 st=""
    for _ in $(seq 900); do
        api GET "/tasks/$id"
        st=$(echo "$BODY" | jq -r .status)
        case $st in ok|error|cancelled) break ;; esac
        sleep 0.2
    done
    TASK_STATUS=$st
    TASK_ERROR=$(echo "$BODY" | jq -r '.error // ""')
    curl -s -b /tmp/jar "$B/tasks/$id/log" >/tmp/task.log
    sed 's/^/    | /' /tmp/task.log
}
# run METHOD PATH JSON EXPECTED_TASK_STATUS
run() {
    api "$1" "$2" "${3:-}"
    expect 202 "$1 $2"
    wait_task "$(echo "$BODY" | jq -r .task_id)"
    [ "$TASK_STATUS" = "$4" ] || fail "$1 $2: task $TASK_STATUS $TASK_ERROR"
}
mdwait() { mdadm --wait "$1" >/dev/null 2>&1 || true; }

step "disks seen by rynas"
api GET /storage/disks; expect 200 "disks"
echo "$BODY" | jq -r '.[] | select(.name|startswith("loop")) | "\(.name) \(.size) used_by=\(.used_by)"'
for n in 0 1 2 3 4 5; do
    echo "$BODY" | jq -e --arg d "${L[$n]}" 'any(.[]; .name == $d)' >/dev/null || fail "${L[$n]} not listed"
done

step "refusals: no confirmation, wrong serials"
api POST /storage/volumes "{\"label\":\"rytest1\",\"level\":\"raid1\",\"disks\":[\"${L[0]}\",\"${L[1]}\"],\"serials\":[\"${L[0]}\",\"${L[1]}\"]}"
expect 400 "no confirm"; echo "$BODY" | jq -c .
[ "$(echo "$BODY" | jq -r .code)" = confirmation_required ] || fail "code"
api POST /storage/volumes "{\"label\":\"rytest1\",\"level\":\"raid1\",\"disks\":[\"${L[0]}\",\"${L[1]}\"],\"confirm\":\"rytest1\",\"serials\":[\"${L[0]}\"]}"
expect 400 "wrong serials"; echo "$BODY" | jq -c .

step "dry run of RAID1 rytest1"
api POST /storage/volumes "{\"label\":\"rytest1\",\"level\":\"raid1\",\"disks\":[\"${L[0]}\",\"${L[1]}\"],\"confirm\":\"rytest1\",\"serials\":[\"${L[0]}\",\"${L[1]}\"],\"dry_run\":true}"
expect 200 "dry run"; echo "$BODY" | jq -r '.steps[] | "  " + .description'
[ -e /dev/md/rytest1 ] && fail "dry run created something"

step "create RAID1 rytest1 (unencrypted)"
run POST /storage/volumes "{\"label\":\"rytest1\",\"level\":\"raid1\",\"disks\":[\"${L[0]}\",\"${L[1]}\"],\"confirm\":\"rytest1\",\"serials\":[\"${L[0]}\",\"${L[1]}\"]}" ok
mdadm --detail /dev/md/rytest1 | grep -E 'Raid Level|Raid Devices|Intent Bitmap|State :|/dev/loop'
mdadm --detail /dev/md/rytest1 | grep -q 'Raid Level : raid1' || fail "not raid1"
mdadm --detail /dev/md/rytest1 | grep -q 'Intent Bitmap : Internal' || fail "no bitmap"
findmnt -n -o SOURCE,FSTYPE,OPTIONS /rytest1 || fail "not mounted"
findmnt -n -o OPTIONS /rytest1 | grep -q 'compress=zstd:1' || fail "no compression"
btrfs subvolume list /rytest1
btrfs subvolume list /rytest1 | grep -q ' path .snapshots$' || fail ".snapshots"
lsattr -d /rytest1/@rynas-cache | grep -q C || fail "@rynas-cache is not nocow"
grep rytest1 /etc/fstab
grep -q "/rytest1 btrfs noatime,compress=zstd:1,nofail 0 0" /etc/fstab || fail "fstab"
grep rytest1 /etc/mdadm/mdadm.conf || fail "mdadm.conf"
head -1 /etc/fstab
api GET /config/drift; expect 200 "drift"; echo "drift: $(echo "$BODY" | jq -c '[.entries[] | .path]')"

step "subvolumes"
run POST /storage/volumes/rytest1/subvolumes '{"name":"docs"}' ok
run POST /storage/volumes/rytest1/subvolumes '{"name":"video","compression":false}' ok
btrfs property get /rytest1/video compression | tee /dev/stderr | grep -q 'compression=none' || fail "video compression"
api GET /storage/volumes/rytest1/subvolumes; expect 200 "list"
echo "$BODY" | jq -c '.[] | {name, readonly, compression, system}'
run PUT /storage/volumes/rytest1/subvolumes/docs '{"compression":true,"readonly":true}' ok
btrfs property get -ts /rytest1/docs ro | grep -q 'ro=true' || fail "docs not read-only"
echo hello >/rytest1/video/file
api DELETE /storage/volumes/rytest1/subvolumes/video; expect 400 "non-empty delete"
echo "refused: $(echo "$BODY" | jq -r .message)"
run DELETE "/storage/volumes/rytest1/subvolumes/video?confirm=video" "" ok
[ -e /rytest1/video ] && fail "video still there"
api DELETE /storage/volumes/rytest1/subvolumes/.snapshots; expect 400 "system subvolume"

step "scrub with md check"
mdwait /dev/md/rytest1
run POST /storage/volumes/rytest1/scrub '{"md_check":true}' ok
grep -q 'mismatch_cnt' /tmp/task.log || fail "no md check"
btrfs scrub status /rytest1 | grep -E 'Status|Error summary'

step "hot replace of ${L[1]} with ${L[2]}"
api POST /storage/volumes/rytest1/replace "{\"old\":\"${L[1]}\",\"new_disk\":\"${L[2]}\",\"confirm\":\"rytest1\",\"serials\":[\"${L[2]}\"],\"dry_run\":true}"
expect 200 "replace dry run"; echo "mode: $(echo "$BODY" | jq -r '.summary[] | select(.[0]=="mode") | .[1]')"
run POST /storage/volumes/rytest1/replace "{\"old\":\"${L[1]}\",\"new_disk\":\"${L[2]}\",\"confirm\":\"rytest1\",\"serials\":[\"${L[2]}\"]}" ok
mdadm --detail /dev/md/rytest1 | grep -E 'State :|/dev/loop'
mdadm --detail /dev/md/rytest1 | grep -q "active sync.*/dev/${L[2]}p1" || fail "${L[2]}p1 not active"
mdadm --detail /dev/md/rytest1 | grep -q "/dev/${L[1]}p1" && fail "${L[1]}p1 still in the array"

step "refusal: a member of a running array"
api POST /storage/volumes "{\"label\":\"rytest3\",\"level\":\"single\",\"disks\":[\"${L[0]}\"],\"confirm\":\"rytest3\",\"serials\":[\"${L[0]}\"]}"
expect 400 "disk in use"; echo "refused: $(echo "$BODY" | jq -r .message)"

step "create encrypted RAID1 rytest2"
PASS='correct horse battery'
run POST /storage/volumes "{\"label\":\"rytest2\",\"level\":\"raid1\",\"encrypt\":true,\"passphrase\":\"$PASS\",\"disks\":[\"${L[3]}\",\"${L[4]}\"],\"confirm\":\"rytest2\",\"serials\":[\"${L[3]}\",\"${L[4]}\"]}" ok
grep -q "$PASS" /tmp/task.log /var/log/rynasd.log /var/lib/rynas/tasks/*.log && fail "passphrase in a log"
api POST /storage/volumes/rytest2/reveal-recovery-key; expect 200 "recovery key"
KEY=$(echo "$BODY" | jq -r .key)
echo "$KEY" | grep -Eq '^([cbdefghijklnrtuv]{8}-){7}[cbdefghijklnrtuv]{8}$' || fail "recovery key format: $KEY"
grep -q "$KEY" /tmp/task.log /var/log/rynasd.log /var/lib/rynas/tasks/*.log && fail "recovery key in a log"
api POST /storage/volumes/rytest2/reveal-recovery-key; expect 404 "recovery key twice"
curl -s -b /tmp/jar -o /tmp/header.img -D /tmp/header.h "$B/storage/volumes/rytest2/luks-header"
grep -i '^content-disposition' /tmp/header.h
cryptsetup isLuks /tmp/header.img || fail "header backup is not LUKS"
cryptsetup luksDump /tmp/header.img | grep -E '^Version|Keyslots:|^  [0-9]: luks2|PBKDF|Memory'
api DELETE /storage/volumes/rytest2/luks-header; expect 204 "header delete"
[ -e /run/rynas/luks-header-rytest2.img ] && fail "header backup still on the NAS"
cryptsetup status rynas-rytest2 | grep -E 'type|cipher|sector size|device'
grep rytest2 /etc/crypttab /etc/fstab
grep -q "rynas-rytest2 UUID=.* none luks,noauto" /etc/crypttab || fail "crypttab"
grep -q "/rytest2 btrfs noatime,compress=zstd:1,nofail,noauto 0 0" /etc/fstab || fail "fstab noauto"
findmnt -n /rytest2 >/dev/null || fail "rytest2 not mounted"
echo secret-data >/rytest2/probe

step "lock, wrong passphrase, unlock with the recovery key"
run POST /storage/volumes/rytest2/lock "" ok
findmnt -n /rytest2 >/dev/null && fail "still mounted"
[ -e /dev/mapper/rynas-rytest2 ] && fail "mapping still open"
api GET /storage/tree; expect 200 "tree"; echo "locked: $(echo "$BODY" | jq -c '[.locked[] | .name]')"
run POST /storage/volumes/rytest2/unlock '{"passphrase":"not the passphrase"}' error
echo "error: $TASK_ERROR"
[ "$TASK_ERROR" = "wrong passphrase or recovery key" ] || fail "wrong passphrase message"
run POST /storage/volumes/rytest2/unlock "{\"passphrase\":\"$KEY\"}" ok
grep -q secret-data /rytest2/probe || fail "data after unlock"
run POST /storage/volumes/rytest2/lock "" ok
run POST /storage/volumes/rytest2/unlock "{\"passphrase\":\"$PASS\"}" ok

step "rollback: mkfs.btrfs fails on rytest3"
mv /usr/sbin/mkfs.btrfs /usr/sbin/mkfs.btrfs.real
printf '#!/bin/sh\necho "ERROR: simulated failure" >&2\nexit 1\n' >/usr/sbin/mkfs.btrfs
chmod 755 /usr/sbin/mkfs.btrfs
run POST /storage/volumes "{\"label\":\"rytest3\",\"level\":\"single\",\"disks\":[\"${L[5]}\"],\"confirm\":\"rytest3\",\"serials\":[\"${L[5]}\"]}" error
mv /usr/sbin/mkfs.btrfs.real /usr/sbin/mkfs.btrfs
echo "report: $TASK_ERROR"
echo "$TASK_ERROR" | grep -q 'rolled back: stopped /dev/md/rytest3' || fail "no rollback report"
grep -q rytest3 /proc/mdstat && fail "array rytest3 still running"
[ "$(lsblk -n -o NAME "/dev/${L[5]}" | wc -l)" = 1 ] || fail "partitions left on ${L[5]}"
[ -z "$(wipefs -n "/dev/${L[5]}")" ] || fail "signatures left on ${L[5]}"
grep -q rytest3 /etc/fstab /etc/mdadm/mdadm.conf && fail "configuration left"
lsblk "/dev/${L[5]}"

step "revisions"
api GET /config/revisions; expect 200 "revisions"
echo "$BODY" | jq -r '.[] | "  \(.id) \(.author) \(.comment)"'

step "all good"
