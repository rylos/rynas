#!/usr/bin/env bash
# A btrfs volume of several devices that loses one (docs/phase2/btrfs-degraded.md), for real,
# on sparse loop files, in a throw-away privileged Debian 13 container. Never touches a real
# disk: rynasd sees loop devices as disks only with the test flag --storage-loop-disks, every
# filesystem is made by this script on its own loop files, and every destructive command
# names only those loop devices (the files live in /var/tmp/rynas-btrfsloops of the container).
#
#   packaging/test-btrfs-degraded.sh [--no-build]
#
# Builds debug rynasd/rynas-web against trixie's glibc (builder image of build-deb.sh), then
# inside the container (no systemd: rynasd mounts with mount(8) directly; the host's udev
# database read-only, as in test-flexraid.sh) logs in over HTTP and through the public API:
#   A. a 3-device volume (data raid1, metadata raid1c3) loses a device: listed as not mounted
#      (1 of 3 missing), also on the dashboard (udev's btrfs ready); mounted degraded; data
#      written while degraded leaves single metadata
#      chunks; removing the missing device is refused (raid1c3 needs three); a replacement
#      smaller than the missing device is refused; the replace rebuilds on a bigger loop
#      device, grows it and converts the single chunks back: nothing missing, nothing weak,
#      the data intact;
#   B. a 2-device raid1 loses a device and is mounted degraded on the other (recorded); with
#      only the missing one back, the degraded mount is refused (split brain); with both back
#      it mounts normally, the single data chunks written meanwhile are converted back, the
#      record stays (scrub needed) until a scrub finishes without errors; the error counters
#      of the device that was away keep the volume a warning until they are reset;
#   C. a 3-device raid1 loses a device: mounted degraded, the missing device removed (the
#      other two take its chunks), two devices left, the data intact.
# At the end the host must have no loop device and no btrfs filesystem of this test left.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-btrfs:trixie
NAME=rynas-test-btrfs-$$
BIN=$ROOT/target/deb-build/debug

if [ "${1:-}" != "--inside" ] && [ "${1:-}" != "--cleanup" ]; then
    if [ "${1:-}" != "--no-build" ]; then
        docker build -q -t rynas-deb-builder:trixie --build-arg RUST_IMAGE=rust:1.98-trixie \
            -f "$ROOT/packaging/deb/builder.Dockerfile" "$ROOT/packaging/deb" >/dev/null
        mkdir -p "$ROOT/target/deb-build"
        docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e CARGO_HOME=/tmp/cargo \
            -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" \
            -e PATH=/usr/local/cargo/bin:/usr/bin:/bin -e CARGO_TARGET_DIR=/src/target/deb-build \
            -v "$HOME/.cargo/registry:/tmp/cargo/registry" -v "$ROOT:/src" -w /src \
            rynas-deb-builder:trixie cargo build --locked -p rynasd -p rynas-web
    fi
    docker build -q -t "$IMAGE" - >/dev/null <<'DOCKERFILE'
FROM debian:trixie
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      btrfs-progs util-linux udev curl jq ca-certificates procps \
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE
    grep -q "^loop " /proc/modules || sudo -n modprobe loop 2>/dev/null || true
    TAG=$(printf '%x' $$)
    # no systemd in the container (nothing takes a console); the host's udev database
    # read-only gives lsblk the filesystems of the loop devices
    docker run -d --name "$NAME" --privileged -v /dev:/dev --tmpfs /run --tmpfs /tmp \
        -v /run/udev:/run/udev:ro -e TAG="$TAG" "$IMAGE" sleep infinity >/dev/null
    docker exec "$NAME" mkdir -p /opt/rynas
    docker cp -q "$BIN/rynasd" "$NAME:/opt/rynas/rynasd"
    docker cp -q "$BIN/rynas-web" "$NAME:/opt/rynas/rynas-web"
    docker cp -q "$ROOT/packaging/test-btrfs-degraded.sh" "$NAME:/opt/rynas/test.sh"
    host_check() {
        echo
        echo "=== host after the test"
        local left=0
        losetup -a | grep -q 'rynas-btrfsloops' && { losetup -a | grep rynas-btrfsloops; left=1; }
        for l in /sys/fs/btrfs/*/label; do
            grep -qE "^b[abc]$TAG\$" "$l" 2>/dev/null && { echo "mounted: $l"; left=1; }
        done
        [ "$left" = 0 ] && echo "host clean" || echo "LEFTOVERS ON THE HOST" >&2
    }
    trap 'docker exec "$NAME" bash /opt/rynas/test.sh --cleanup >/dev/null 2>&1 || true; docker rm -f "$NAME" >/dev/null; host_check' EXIT
    docker exec -i -e HOLD="${HOLD:-}" -e DUMP="${DUMP:-}" "$NAME" bash /opt/rynas/test.sh --inside
    if [ -n "${DUMP:-}" ]; then
        mkdir -p "$DUMP" && docker cp -q "$NAME:/var/tmp/rynas-test-dump/." "$DUMP/"
    fi
    exit
fi

# ---------------------------------------------------------------- inside the container
LOOPS=/var/tmp/rynas-btrfsloops
A=ba$TAG
B=bb$TAG
C=bc$TAG

stop_daemons() {
    for p in /run/rynas-test/*.pid; do
        [ -e "$p" ] || continue
        kill -9 "$(cat "$p")" 2>/dev/null || true
        rm -f "$p"
    done
}

# our loop devices only: the ones backed by files in $LOOPS
our_loops() { losetup -a | grep "$LOOPS/" | cut -d: -f1; }

cleanup() {
    stop_daemons
    for l in $A $B $C; do umount -q "/$l" 2>/dev/null || true; done
    for dev in $(our_loops); do
        btrfs device scan --forget "$dev" 2>/dev/null || true
        wipefs -q -a "$dev" 2>/dev/null || true
        losetup -d "$dev" 2>/dev/null || true
    done
    rm -rf "$LOOPS"
}
if [ "${1:-}" = "--cleanup" ]; then
    cleanup
    exit 0
fi

step() { printf '\n=== %s\n' "$*"; }
fail() {
    echo "FAIL: $*" >&2
    tail -30 /var/log/rynasd.log >&2 || true
    [ -n "${HOLD:-}" ] && sleep 1800
    exit 1
}
trap cleanup EXIT

step "versions"
dpkg-query -W -f '${Package} ${Version}\n' btrfs-progs util-linux
uname -r
echo "volumes: $A $B $C"

mkdir -p "$LOOPS" /run/rynas-test /var/tmp/rynas-test-dump
# attach file $1 (created with size $2 when missing): kernel name in $DEV
attach() {
    [ -e "$LOOPS/$1.img" ] || truncate -s "$2" "$LOOPS/$1.img"
    DEV=$(losetup -f --show "$LOOPS/$1.img")
    DEV=${DEV#/dev/}
    udevadm settle 2>/dev/null || sleep 1
}
# the device of file $1 gone: forgotten by btrfs, detached
lose() {
    local d
    d=$(losetup -a | grep "$LOOPS/$1.img" | cut -d: -f1)
    [ -n "$d" ] || fail "no loop device for $1"
    btrfs device scan --forget "$d" 2>/dev/null || true
    losetup -d "$d"
    # as at boot: udev looks at the members left again (btrfs ready: not all there)
    for o in $(our_loops); do echo change >"/sys/class/block/${o#/dev/}/uevent"; done
    udevadm settle 2>/dev/null || sleep 1
}
dev_of() { losetup -a | grep "$LOOPS/$1.img" | cut -d: -f1 | sed 's|/dev/||'; }

step "rynasd and rynas-web (root, no systemd)"
install -d /usr/libexec/rynas /var/lib/rynas /run/rynas /etc
install -m 755 /opt/rynas/rynasd /opt/rynas/rynas-web /usr/libexec/rynas/
touch /etc/fstab
# a system user in an admin group: rynasd checks administrators from the system groups
groupadd -f rynas-admin; id admin1 >/dev/null 2>&1 || useradd -M -G rynas-admin admin1
HASH=$(printf 'Adm1n-test-pw\n' | /usr/libexec/rynas/rynas-web hash-password)
printf '[[user]]\nname = "admin1"\nhash = "%s"\ngroups = ["rynas-admin"]\n' "$HASH" >/etc/rynas-users.toml
RYNAS_CGROUP_DAEMON=max:max RYNAS_CGROUP_WORKERS=max:max /usr/libexec/rynas/rynasd --socket /run/rynas/rynasd.sock \
    --state-dir /var/lib/rynas --storage-loop-disks --storage-check-interval 5 >>/var/log/rynasd.log 2>&1 &
echo $! >/run/rynas-test/rynasd.pid
for _ in $(seq 100); do [ -S /run/rynas/rynasd.sock ] && break; sleep 0.1; done
[ -S /run/rynas/rynasd.sock ] || fail "rynasd did not start"
/usr/libexec/rynas/rynas-web --listen 127.0.0.1:8080 --rynasd-socket /run/rynas/rynasd.sock \
    --users-file /etc/rynas-users.toml >/var/log/rynas-web.log 2>&1 &
echo $! >/run/rynas-test/rynas-web.pid
for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:8080/ && break; sleep 0.1; done

B_URL=http://127.0.0.1:8080/api/v1
curl -s -c /tmp/jar -H 'Content-Type: application/json' -o /tmp/session.json \
    -d '{"username":"admin1","password":"Adm1n-test-pw"}' "$B_URL/auth/login"
CSRF=$(jq -r .csrf_token /tmp/session.json)
[ -n "$CSRF" ] && [ "$CSRF" != null ] || fail "login: $(cat /tmp/session.json)"
api() {
    local out
    out=$(curl -s -b /tmp/jar -H "X-CSRF-Token: $CSRF" -H 'Content-Type: application/json' \
        -X "$1" ${3:+-d "$3"} -w '\n%{http_code}' "$B_URL$2")
    CODE=${out##*$'\n'}
    BODY=${out%$'\n'*}
}
expect() { [ "$CODE" = "$1" ] || fail "$2: HTTP $CODE $BODY"; }
wait_task() {
    local id=$1 st=""
    for _ in $(seq 3000); do
        api GET "/tasks/$id"
        st=$(echo "$BODY" | jq -r .status)
        case $st in ok|error|cancelled) break ;; esac
        sleep 0.2
    done
    TASK_STATUS=$st
    curl -s -b /tmp/jar "$B_URL/tasks/$id/log" | sed 's/^/    | /'
}
# run PATH JSON: a task that must end ok
run() {
    api POST "$1" "$2"
    expect 202 "POST $1"
    wait_task "$(echo "$BODY" | jq -r .task_id)"
    [ "$TASK_STATUS" = ok ] || fail "POST $1: task $TASK_STATUS"
}
# fs UUID JQ [what]: wait (up to 75 s: btrfs details are cached a minute) until the
# filesystem's entry of /storage/filesystems matches JQ; it is left in /tmp/fs.json
fs() {
    for _ in $(seq 75); do
        api GET /storage/filesystems
        echo "$BODY" | jq --arg u "$1" '.[] | select(.uuid == $u)' >/tmp/fs.json
        [ -s /tmp/fs.json ] && jq -e "$2" /tmp/fs.json >/dev/null && return 0
        sleep 1
    done
    jq -c '{mountpoint, devices: [.devices[] | {devid, path, missing}], repair}' /tmp/fs.json >&2 || true
    fail "${3:-$2}"
}
show() { jq -c '{mountpoint, devices: [.devices[] | "\(.devid):\(.path)\(if .missing then " MISSING" else "" end)"], repair}' /tmp/fs.json | sed 's/^/  /'; }
dump() { [ -n "${DUMP:-}" ] || return 0; for p in tree filesystems arrays disks; do curl -s -b /tmp/jar "$B_URL/storage/$p" >"/var/tmp/rynas-test-dump/$1-$p.json"; done; curl -s -b /tmp/jar "$B_URL/storage" >"/var/tmp/rynas-test-dump/$1-overview.json"; }
write_data() { # VOLUME NAME MiB
    mkdir -p "/$1/check"
    head -c "${3}M" /dev/urandom >"/$1/check/$2"
    (cd "/$1/check" && sha256sum ./* >"/run/rynas-test/$1.sha")
    sync
}
verify_data() {
    (cd "/$1/check" && sha256sum --quiet -c "/run/rynas-test/$1.sha") || fail "$1: data changed"
    echo "  data of $1: checksums ok"
}
uuid_of() { blkid -p -s UUID -o value "/dev/$1"; }
fstab() { echo "UUID=$1 /$2 btrfs noatime,nofail 0 0" >>/etc/fstab; mkdir -p "/$2"; }

# ===================================================================== A
step "A. 3 devices, data raid1, metadata raid1c3"
attach a1 512M; A1=$DEV; attach a2 512M; A2=$DEV; attach a3 512M; A3=$DEV
mkfs.btrfs -q -f -L "$A" -d raid1 -m raid1c3 "/dev/$A1" "/dev/$A2" "/dev/$A3" >/dev/null
UA=$(uuid_of "$A1"); fstab "$UA" "$A"
mount -o noatime "/dev/$A1" "/$A"
write_data "$A" one 64
fs "$UA" '(.devices | length) == 3 and .repair == null' "A: healthy 3-device volume"
show
umount "/$A"

step "A. a device is lost: the volume is not mounted, 1 of 3 missing"
lose a3
fs "$UA" '.repair.mounted == false and .repair.total_devices == 3 and .repair.missing_count == 1 and .repair.mount == null' "A: not listed as unmounted with 1 of 3 missing"
show
api GET /storage/tree; expect 200 tree
[ "$(echo "$BODY" | jq -r --arg u "$UA" '.volumes[] | select(.id == $u) | .health')" = failing ] || fail "A: the tree does not say failing"
# udev works on the change events in its own time
for _ in $(seq 30); do
    api GET /storage; expect 200 "dashboard overview"
    echo "$BODY" | jq -e --arg l "$A" '.arrays | any(.name == $l and .state == "inactive")' >/dev/null && break
    sleep 1
done
echo "$BODY" | jq -e --arg l "$A" '.arrays | any(.name == $l and .state == "inactive")' >/dev/null \
    || fail "A: the dashboard does not list the volume as inactive: $(echo "$BODY" | jq -c .arrays)"
echo "  dashboard: $A inactive"
dump unmounted

step "A. mounted degraded from the API"
api POST "/storage/btrfs/$UA/mount-degraded" '{"dry_run":true}'; expect 200 "mount plan"
echo "$BODY" | jq -r '.steps[] | "  - " + .description'
api POST "/storage/btrfs/$UA/mount-degraded" '{"confirm":"wrong"}'; expect 400 "a wrong name is refused"
run "/storage/btrfs/$UA/mount-degraded" "{\"confirm\":\"$A\"}"
grep " /$A " /proc/self/mounts | grep -q degraded || fail "A: not mounted degraded"
verify_data "$A"
[ -s /var/lib/rynas/storage/btrfs-degraded.json ] || fail "A: the degraded mount was not recorded"
write_data "$A" two 48
fs "$UA" '.repair.mounted and .repair.degraded_mount and .repair.missing == [3] and .repair.remove == "too_few_devices" and (.repair.lost | length) == 0' "A: degraded facts"
show
dump degraded
btrfs filesystem usage -b "/$A" | grep -E '^(Data|Metadata|System),' | sed 's/^/  /'

step "A. remove refused, a replacement too small refused"
api POST "/storage/btrfs/$UA/remove-missing" '{"dry_run":true}'; expect 400 "remove (raid1c3 on two devices)"
echo "  $(echo "$BODY" | jq -r .message)"
attach small 256M; SMALL=$DEV
sleep 5.5
api POST "/storage/btrfs/$UA/replace" "{\"devid\":3,\"new_disk\":\"$SMALL\",\"dry_run\":true}"; expect 400 "a smaller replacement"
echo "  $(echo "$BODY" | jq -r .message)"

step "A. replace device 3 with a bigger loop device"
attach a4 768M; A4=$DEV
sleep 5.5
api POST "/storage/btrfs/$UA/replace" "{\"devid\":3,\"new_disk\":\"$A4\",\"dry_run\":true}"; expect 200 "replace plan"
echo "$BODY" | jq -r '.steps[] | "  - " + .description'
api POST "/storage/btrfs/$UA/replace" "{\"devid\":3,\"new_disk\":\"$A4\",\"confirm\":\"$A\",\"serials\":[]}"
expect 400 "the new disk's serial (its name for a loop device) is required"
run "/storage/btrfs/$UA/replace" "{\"devid\":3,\"new_disk\":\"$A4\",\"confirm\":\"$A\",\"serials\":[\"$A4\"]}"
fs "$UA" '.repair == null and (.devices | map(select(.missing)) | length) == 0' "A: still something to repair after the replace"
show
btrfs filesystem usage -b "/$A" | grep -E '^(Data|Metadata|System),' | sed 's/^/  /'
btrfs filesystem usage -b "/$A" | grep -qiE '^(Data|Metadata|System),single' && fail "A: single chunks left"
size3=$(btrfs filesystem show --raw "/$A" | awk '$1 == "devid" && $2 == 3 {print $4}')
[ "$size3" = $((768 << 20)) ] || fail "A: device 3 not grown to the new disk ($size3)"
verify_data "$A"
umount "/$A"

# ===================================================================== B
step "B. 2 devices, raid1: degraded on one, then only the other: split brain refused"
attach b1 512M; B1=$DEV; attach b2 512M; B2=$DEV
mkfs.btrfs -q -f -L "$B" -d raid1 -m raid1 "/dev/$B1" "/dev/$B2" >/dev/null
UB=$(uuid_of "$B1"); fstab "$UB" "$B"
mount -o noatime "/dev/$B1" "/$B"; write_data "$B" one 32; umount "/$B"
lose b2
fs "$UB" '.repair.mounted == false and .repair.missing_count == 1' "B: not listed as unmounted"
run "/storage/btrfs/$UB/mount-degraded" "{\"confirm\":\"$B\"}"
write_data "$B" two 160
umount "/$B"
lose b1; attach b2 512M
fs "$UB" '.repair.mounted == false and .repair.present[0].devid == 2' "B: device 2 alone not listed"
api POST "/storage/btrfs/$UB/mount-degraded" "{\"confirm\":\"$B\"}"; expect 409 "the split brain is refused"
echo "  $(echo "$BODY" | jq -r .message)"

step "B. both back: a normal mount, the single data chunks converted back, a scrub, the record forgotten"
attach b1 512M
btrfs device scan "/dev/$(dev_of b1)" "/dev/$(dev_of b2)" >/dev/null
mount -o noatime "/dev/$(dev_of b1)" "/$B"
fs "$UB" '.repair.mounted and .repair.missing_count == 0 and (.repair.convert | map(.kind) | index("data")) != null' "B: weak data chunks not listed"
show
dump weak
run "/storage/btrfs/$UB/convert" '{"dry_run":false}'
btrfs filesystem usage -b "/$B" | grep -qiE '^Data,single' && fail "B: single data chunks left"
verify_data "$B"
# device 2 lacks the copies written while it was away: the record stays, a scrub is asked for
fs "$UB" '.repair.scrub_needed and (.repair.convert | length) == 0' "B: no scrub asked for after the reunion"
sleep 6
jq -e --arg u "$UB" '.mounts[$u].whole_scrub != null' /var/lib/rynas/storage/btrfs-degraded.json >/dev/null || fail "B: the record went before a scrub"
echo "  record of $B kept until a scrub: $(jq -c --arg u "$UB" '.mounts[$u]' /var/lib/rynas/storage/btrfs-degraded.json)"
run "/storage/volumes/$B/scrub" '{"action":"start","md_check":false,"dry_run":false}'
for _ in $(seq 60); do
    jq -e --arg u "$UB" '.mounts[$u] == null' /var/lib/rynas/storage/btrfs-degraded.json >/dev/null && break
    sleep 1
done
jq -e --arg u "$UB" '.mounts[$u] == null' /var/lib/rynas/storage/btrfs-degraded.json >/dev/null || fail "B: the record stays after a clean scrub"
fs "$UB" '.repair == null' "B: still something to repair after the scrub"
echo "  degraded-mount record of $B forgotten after the scrub"
# the scrub's start is a timestamp and its errors come by kind; the error counters of the
# device that was away keep the volume a warning until an administrator resets them
jq -e '.scrub.status == "finished" and (.scrub.started_ms | type) == "number"' /tmp/fs.json >/dev/null || fail "B: no start time of the scrub"
echo "  scrub: $(jq -c '.scrub | {errors, corrected, uncorrectable, error_kinds}' /tmp/fs.json)"
echo "  counters: $(jq -c '[.devices[] | {devid, errors}]' /tmp/fs.json)"
health() { api GET /storage/tree; echo "$BODY" | jq -r --arg u "$UB" '.volumes[] | select(.id == $u) | .health'; }
if jq -e '[.devices[].errors[]] | add > 0' /tmp/fs.json >/dev/null; then
    [ "$(health)" = warning ] || fail "B: counters above zero but the volume is $(health)"
fi
run "/storage/btrfs/$UB/reset-errors" "{}"
fs "$UB" '[.devices[].errors[]] | add == 0' "B: error counters not zero after the reset"
[ "$(health)" != warning ] || fail "B: still a warning after the reset of the counters"
echo "  error counters reset, volume $(health)"
umount "/$B"

# ===================================================================== C
step "C. 3 devices, raid1: the missing device removed"
attach c1 1G; C1=$DEV; attach c2 1G; C2=$DEV; attach c3 1G; C3=$DEV
mkfs.btrfs -q -f -L "$C" -d raid1 -m raid1 "/dev/$C1" "/dev/$C2" "/dev/$C3" >/dev/null
UC=$(uuid_of "$C1"); fstab "$UC" "$C"
mount -o noatime "/dev/$C1" "/$C"; write_data "$C" one 48; umount "/$C"
lose c3
fs "$UC" '.repair.mounted == false' "C: not listed as unmounted"
run "/storage/btrfs/$UC/mount-degraded" "{\"confirm\":\"$C\"}"
fs "$UC" '.repair.mounted and .repair.remove == null' "C: removal not possible"
api POST "/storage/btrfs/$UC/remove-missing" '{"dry_run":true}'; expect 200 "remove plan"
echo "$BODY" | jq -r '.steps[] | "  - " + .description'
run "/storage/btrfs/$UC/remove-missing" "{\"confirm\":\"$C\"}"
fs "$UC" '.repair == null and (.devices | length) == 2' "C: not two whole devices after the removal"
show
verify_data "$C"
umount "/$C"

echo "ALL BTRFS DEGRADED TESTS PASSED"
