#!/usr/bin/env bash
# Flexible RAID (md tiers + LVM + btrfs) for real, on sparse loop files, in a throw-away
# privileged Debian 13 container (md, dm and loop devices are kernel objects: the container
# needs the host's /dev). Never touches a real disk: rynasd sees loop devices as disks only
# with the test flag --storage-loop-disks, every destructive request names the loop devices
# it erases, and every destructive command of this script names only the loop devices of its
# own files, the test volume groups (rynas_<label>) and the test arrays (/dev/md/<label>_tN).
#
#   packaging/test-flexraid.sh [--no-build]
#
# Builds debug rynasd/rynas-web against trixie's glibc (builder image of build-deb.sh), then
# inside the container (no systemd, no udev: mounts fall back to mount/umount from /etc/fstab)
# logs in over HTTP (session + CSRF) and through the public API, with --storage-min-tier-mib
# 64 so that GiB-sized files make tiers:
#   1. create a 1-disk tolerance volume on 1+2+2 GiB (RAID5x3 + RAID1x2), checksummed data;
#   2. add a 3 GiB disk (tier 1 grows to 4 members, tier 2 RAID1 -> RAID5);
#   3. replace the smallest disk with a 3 GiB one (hot replace, joins, a new top tier);
#   4. fail every partition of one disk, replace it;
#   5. remove a disk (partitions failed and removed, loop device detached), replace "missing";
#   5b. a disk failed and removed but still attached comes back: replace "missing" with it
#       (its leftover partitions cleared, then rebuilt on);
#   6. kill -9 rynasd during a reshape, restart it, complete what is left;
#   7. a "reboot": arrays stopped, assembled again from mdadm.conf, volume group activated;
#   7b. a hot replace killed (kill -9 rynasd) during the copy of tier 1: requested again, it
#       goes on; killed again during the copy of tier 2: complete finishes it (old disk
#       cleared included);
#   8. a 2-disk tolerance volume with a three-way RAID1 tier, grown to RAID6;
#   8b. two of its disks failed and removed (still attached): a new disk rebuilds one slot
#       (the tiers stay one disk short), then one of the two comes back; the other, left
#       over, is not erased (the plan warns about it) until a growth names it;
#   9. an encrypted volume: create, add a disk (LUKS resized), lock, unlock.
# After every step: the data checksums, no growth left to complete, every tier healthy.
# At the end the host must have no md array, device-mapper mapping or loop device left.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-flexraid:trixie
NAME=rynas-test-flexraid-$$
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
      mdadm lvm2 cryptsetup-bin systemd-cryptsetup btrfs-progs gdisk util-linux e2fsprogs \
      udev curl jq ca-certificates procps \
 && rm -rf /var/lib/apt/lists/*
DOCKERFILE
    # the kernel drivers (a container cannot load modules); harmless when already loaded
    for m in loop md_mod raid1 raid456 dm_mod dm_crypt; do
        grep -q "^$m " /proc/modules || sudo -n modprobe "$m" 2>/dev/null || true
    done
    # labels unique to this run: volume groups rynas_<label>, arrays /dev/md/<label>_tN
    TAG=$(printf '%x' $$)
    # the host's udev does the container's udev work, as on a real system: its database
    # (read-only) gives lsblk partition labels and filesystems, `udevadm settle` waits for
    # it, it makes the /dev/mapper links and signals device-mapper cookies in the host's IPC
    # namespace (LVM and cryptsetup wait for them); mdadm's map is shared with it, so that it
    # names the arrays like the container and a stop in the container removes the entry
    docker run -d --name "$NAME" --privileged -v /dev:/dev --tmpfs /run --tmpfs /tmp \
        -v /run/udev:/run/udev:ro -v /run/mdadm:/run/mdadm --ipc host \
        -e TAG="$TAG" "$IMAGE" sleep infinity >/dev/null
    docker exec "$NAME" mkdir -p /opt/rynas
    docker cp -q "$BIN/rynasd" "$NAME:/opt/rynas/rynasd"
    docker cp -q "$BIN/rynas-web" "$NAME:/opt/rynas/rynas-web"
    docker cp -q "$ROOT/packaging" "$NAME:/opt/rynas/packaging"
    host_check() {
        echo
        echo "=== host after the test"
        local left=0
        cat /proc/mdstat 2>/dev/null || echo "(no md driver loaded)"
        grep -E "(fa|fb|fc)$TAG" /proc/mdstat 2>/dev/null && left=1
        ls /dev/mapper
        for m in /dev/mapper/rynas[-_]f[abc]"$TAG"*; do [ -e "$m" ] && left=1; done
        losetup -a
        losetup -a | grep -q 'rynas-flexloops' && left=1
        # links in /dev/md and entries in mdadm's map (both root only on the host)
        docker run --rm -v /dev:/dev -v /run/mdadm:/run/mdadm "$IMAGE" \
            sh -c "ls /dev/md 2>/dev/null; cat /run/mdadm/map 2>/dev/null" | grep "f[abc]$TAG" && left=1
        for f in speed_limit_min speed_limit_max; do
            [ -e "/proc/sys/dev/raid/$f" ] && echo "$f: $(cat "/proc/sys/dev/raid/$f")"
        done
        [ "$left" = 0 ] && echo "host clean" || echo "LEFTOVERS ON THE HOST" >&2
    }
    # whatever happens: volume groups deactivated, arrays stopped, mappings closed, loop
    # devices detached, md speed limits restored
    trap 'docker exec "$NAME" bash /opt/rynas/packaging/test-flexraid.sh --cleanup >/dev/null 2>&1 || true; docker rm -f "$NAME" >/dev/null; host_check' EXIT
    docker exec -i -e HOLD="${HOLD:-}" -e FROM="${FROM:-1}" "$NAME" bash /opt/rynas/packaging/test-flexraid.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
LOOPS=/var/tmp/rynas-flexloops
F1=fa$TAG # 1-disk tolerance volume of scenarios 1-7
F2=fb$TAG # 2-disk tolerance volume of scenario 8
F3=fc$TAG # encrypted volume of scenario 9
LABELS="$F1 $F2 $F3"
SPEED=/run/rynas-test-speed

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
    for l in $LABELS; do
        umount -q "/$l" 2>/dev/null || true
        cryptsetup close "rynas-$l" 2>/dev/null || true
        vgchange -an "rynas_$l" >/dev/null 2>&1 || true
        for md in /dev/md/"${l}"_t*; do
            [ -e "$md" ] && mdadm --stop "$md" >/dev/null 2>&1 || true
        done
    done
    # arrays of our loop devices under another name (assembled by the host's udev)
    local loops
    loops=$(our_loops | sed 's|/dev/||' | tr '\n' ' ')
    for md in /sys/block/md*; do
        [ -d "$md/md" ] || continue
        for s in "$md"/slaves/*; do
            [ -e "$s" ] || continue
            base=$(basename "$s")
            for lp in $loops; do
                case $base in "$lp"p*)
                    for h in "$md"/holders/dm-*; do
                        [ -e "$h" ] && dmsetup remove "$(cat "$h/dm/name")" 2>/dev/null || true
                    done
                    mdadm --stop "/dev/$(basename "$md")" >/dev/null 2>&1 || true
                    ;;
                esac
            done
        done
    done
    for dev in $(our_loops); do
        for p in "$dev"p*; do
            [ -e "$p" ] && mdadm --zero-superblock "$p" 2>/dev/null || true
            [ -e "$p" ] && wipefs -q -a "$p" 2>/dev/null || true
        done
        losetup -d "$dev" 2>/dev/null || true
    done
    # links mdadm made itself (no udev removes them when an array stops)
    for l in $LABELS; do rm -f /dev/md/"${l}"_t*; done
    if [ -e "$SPEED" ]; then
        read -r min max <"$SPEED"
        echo "$min" >/proc/sys/dev/raid/speed_limit_min
        echo "$max" >/proc/sys/dev/raid/speed_limit_max
        rm -f "$SPEED"
    fi
}
if [ "${1:-}" = "--cleanup" ]; then
    cleanup
    exit 0
fi

step() { printf '\n=== %s\n' "$*"; }
fail() {
    echo "FAIL: $*" >&2
    tail -40 /var/log/rynasd.log >&2 || true
    cat /proc/mdstat >&2 || true
    # debugging: HOLD=1 keeps the container (and the devices) for docker exec
    [ -n "${HOLD:-}" ] && sleep 1800
    exit 1
}
trap cleanup EXIT

step "versions"
for p in mdadm lvm2 cryptsetup-bin btrfs-progs gdisk util-linux; do
    dpkg-query -W -f '${Package} ${Version}\n' "$p"
done
uname -r
echo "labels: $LABELS"

# the host has no LVM udev rules: LVM makes /dev/<vg>/<lv> itself when udev did not
sed -i 's/^\(\s*\)# *verify_udev_operations = 0/\1verify_udev_operations = 1/' /etc/lvm/lvm.conf
# LVM looks at md arrays only: never at the host's disks
sed -i '0,/^devices {/s//devices {\n\tglobal_filter = [ "a|^\/dev\/md|", "r|.*|" ]/' /etc/lvm/lvm.conf
lvmconfig --type full devices/global_filter activation/udev_sync activation/verify_udev_operations devices/use_devicesfile 2>/dev/null || true

# mdadm sees the udev database and would wait for udev to make /dev/md/<name>: it makes
# the link itself (rynasd starts mdadm with a clean environment, hence the wrapper)
printf '#!/bin/sh\nMDADM_NO_UDEV=1 exec /usr/sbin/mdadm "$@"\n' >/usr/local/sbin/mdadm
chmod 755 /usr/local/sbin/mdadm

# sparse loop files; NEW_LOOP size -> the kernel name in $DEV
mkdir -p "$LOOPS"
N=0
new_loop() {
    N=$((N + 1))
    rm -f "$LOOPS/d$N.img"
    truncate -s "$1" "$LOOPS/d$N.img"
    DEV=$(losetup -f --show -P "$LOOPS/d$N.img")
    DEV=${DEV#/dev/}
}

step "rynasd and rynas-web (root, no systemd)"
install -d /usr/libexec/rynas /var/lib/rynas /run/rynas /run/rynas-test /etc/mdadm
install -m 755 /opt/rynas/rynasd /opt/rynas/rynas-web /usr/libexec/rynas/
touch /etc/fstab /etc/crypttab
HASH=$(printf 'Adm1n-test-pw\n' | /usr/libexec/rynas/rynas-web hash-password)
printf '[[user]]\nname = "admin1"\nhash = "%s"\ngroups = ["rynas-admin"]\n' "$HASH" >/etc/rynas-users.toml
start_rynasd() {
    rm -f /run/rynas/rynasd.sock
    # no memory limits on rynasd's cgroup leaves (rynasd::cgroup): with the unit's 256M
    # cap on daemon/, cryptsetup's Argon2 (256 MiB, locked) never finishes. On a real install
    # rynasd runs LUKS key derivations in transient units outside its cgroup
    # (rynas_tasks::crypt_cmd); this container has no systemd as PID 1, so they run inside
    RYNAS_CGROUP_DAEMON=max:max RYNAS_CGROUP_WORKERS=max:max /usr/libexec/rynas/rynasd --socket /run/rynas/rynasd.sock --state-dir /var/lib/rynas \
        --storage-loop-disks --storage-min-tier-mib 64 >>/var/log/rynasd.log 2>&1 &
    echo $! >/run/rynas-test/rynasd.pid
    for _ in $(seq 100); do [ -S /run/rynas/rynasd.sock ] && break; sleep 0.1; done
    [ -S /run/rynas/rynasd.sock ] || fail "rynasd did not start"
}
start_rynasd
/usr/libexec/rynas/rynas-web --listen 127.0.0.1:8080 --rynasd-socket /run/rynas/rynasd.sock \
    --users-file /etc/rynas-users.toml >/var/log/rynas-web.log 2>&1 &
echo $! >/run/rynas-test/rynas-web.pid
for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:8080/ && break; sleep 0.1; done

B=http://127.0.0.1:8080/api/v1
login() {
    curl -s -c /tmp/jar -H 'Content-Type: application/json' -o /tmp/session.json \
        -d '{"username":"admin1","password":"Adm1n-test-pw"}' "$B/auth/login"
    CSRF=$(jq -r .csrf_token /tmp/session.json)
    [ -n "$CSRF" ] && [ "$CSRF" != null ] || fail "login: $(cat /tmp/session.json)"
}
login
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
    for _ in $(seq 3000); do
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
# start METHOD PATH JSON -> task id in $TASK
start() {
    api "$1" "$2" "${3:-}"
    expect 202 "$1 $2"
    TASK=$(echo "$BODY" | jq -r .task_id)
}
# run METHOD PATH JSON EXPECTED_TASK_STATUS
run() {
    start "$1" "$2" "${3:-}"
    wait_task "$TASK"
    [ "$TASK_STATUS" = "$4" ] || fail "$1 $2: task $TASK_STATUS $TASK_ERROR"
}
# plan PATH JSON: a plan that must be valid; steps and warnings printed
plan() {
    # plans read the inventory rynasd keeps for 5 s: let it see the new loop devices
    sleep 5.5
    api POST "$1" "$2"
    expect 200 "plan $1"
    [ "$(echo "$BODY" | jq -r .valid)" = true ] || fail "plan $1 invalid: $(echo "$BODY" | jq -c .errors)"
    echo "$BODY" | jq -r '.steps[] | "  - " + .description'
    echo "$BODY" | jq -r '.warnings[] | "  ! " + .'
    PLAN=$BODY
}

# the tier arrays of volume $1 (kernel names, tier order)
tiers_of() {
    local l=$1 t
    for t in /dev/md/"${l}"_t*; do
        [ -e "$t" ] && basename "$(readlink -f "$t")"
    done
}
# wait until no tier of volume $1 resyncs, recovers or reshapes
flexwait() {
    local md
    for md in $(tiers_of "$1"); do
        mdadm --wait "/dev/$md" >/dev/null 2>&1 || true
    done
    for _ in $(seq 600); do
        grep -A3 -E "^($(tiers_of "$1" | paste -sd'|')) " /proc/mdstat \
            | grep -qE 'resync|recovery|reshape|DELAYED|PENDING' || return 0
        sleep 0.5
    done
    fail "tiers of $1 still busy"
}
speed_up() {
    [ -e "$SPEED" ] && return
    [ -e /proc/sys/dev/raid/speed_limit_min ] || return
    echo "$(cat /proc/sys/dev/raid/speed_limit_min) $(cat /proc/sys/dev/raid/speed_limit_max)" >"$SPEED"
    echo 500000 >/proc/sys/dev/raid/speed_limit_min
    echo 2000000 >/proc/sys/dev/raid/speed_limit_max
    echo "md speed limits raised (were: $(cat "$SPEED"))"
}

# show volume $1: tiers (mdadm), LVM, btrfs
show() {
    local l=$1 md
    for md in /dev/md/"${l}"_t*; do
        [ -e "$md" ] || continue
        printf '  %s: %s\n' "$(basename "$md")" "$(mdadm --detail "$md" \
            | awk -F' : ' '/Raid Level/{lv=$2} /Array Size/{sz=$2} /Raid Devices/{rd=$2} /State :/{st=$2} END{print lv", "rd" devices, "sz", "st}')"
        mdadm --detail "$md" | grep -E '/dev/loop' | sed 's/^ */      /'
    done
    pvs --noheadings --units m -o pv_name,vg_name,pv_size,pv_free 2>/dev/null | grep "rynas_$l" | sed 's/^/  pv/'
    vgs --noheadings --units m -o vg_name,pv_count,vg_size,vg_free "rynas_$l" | sed 's/^/  vg/'
    lvs --noheadings --units m -o lv_name,lv_size "rynas_$l" | sed 's/^/  lv/'
    btrfs filesystem show "/$l" | grep -E 'devid' | sed 's/^/  /'
    btrfs filesystem usage -b "/$l" 2>/dev/null | grep -E 'Device size' | sed 's/^/  /'
}
fs_size() { btrfs filesystem usage -b "/$1" 2>/dev/null | awk '/Device size:/{print $3}'; }

# the volume as rynas sees it, then: every tier healthy, nothing pending or blocking, no
# step left to complete
check_volume() {
    local l=$1 want_tiers=${2:-}
    api GET /storage/tree; expect 200 "tree"
    echo "$BODY" | jq --arg l "$l" '.volumes[] | select(.label == $l)' >/tmp/vol.json
    [ -s /tmp/vol.json ] || fail "volume $l not in the tree"
    jq -r '.flexible | "  rynas: tolerance \(.tolerance), lv \(.size) B, managed \(.managed), pending \(.pending | length), blockers \(.blockers)"' /tmp/vol.json
    jq -r '.flexible.tiers[] | "    t\(.index) \(.level) \(.members | join(",")) usable=\(.usable) \(.health) degraded=\(.degraded)"' /tmp/vol.json
    [ "$(jq '.flexible.tiers | length' /tmp/vol.json)" -gt 0 ] || fail "$l: no tiers"
    jq -e '.flexible.tiers | all(.health == "healthy")' /tmp/vol.json >/dev/null || fail "$l: a tier is not healthy"
    jq -e '.flexible.pending | length == 0' /tmp/vol.json >/dev/null || fail "$l: pending $(jq -c .flexible.pending /tmp/vol.json)"
    jq -e '.flexible.blockers | length == 0' /tmp/vol.json >/dev/null || fail "$l: blockers $(jq -c .flexible.blockers /tmp/vol.json)"
    jq -e '.flexible.managed' /tmp/vol.json >/dev/null || fail "$l: not managed"
    if [ -n "$want_tiers" ]; then
        local got
        got=$(jq -r '[.flexible.tiers[] | "\(.level)x\(.members | length)"] | join(" ")' /tmp/vol.json)
        [ "$got" = "$want_tiers" ] || fail "$l: tiers \"$got\", expected \"$want_tiers\""
    fi
    api POST /storage/plan/complete "{\"volume\":\"$l\"}"; expect 200 "plan complete"
    [ "$(echo "$BODY" | jq '.steps | length')" = 0 ] || fail "$l: steps left to complete: $(echo "$BODY" | jq -c '[.steps[].description]')"
    grep -c "^ARRAY /dev/md/${l}_t" /etc/mdadm/mdadm.conf >/tmp/n
    [ "$(cat /tmp/n)" = "$(tiers_of "$l" | wc -l)" ] || fail "$l: mdadm.conf lists $(cat /tmp/n) tiers"
}

# checksummed data of volume $1
write_data() {
    mkdir -p "/$1/check"
    for i in 1 2 3 4; do head -c 48M /dev/urandom >"/$1/check/f$i"; done
    (cd "/$1/check" && sha256sum f* >"/run/rynas-test/$1.sha")
    sync
}
verify_data() {
    (cd "/$1/check" && sha256sum --quiet -c "/run/rynas-test/$1.sha") || fail "$1: data changed"
    echo "  data of $1: checksums ok"
}
after() { # VOLUME [TIERS]: checks after every step
    flexwait "$1"
    show "$1"
    check_volume "$1" "${2:-}"
    verify_data "$1"
}
serials() { printf '%s\n' "$@" | jq -R . | jq -sc .; }
disks_json() { serials "$@"; }
grow() { # VOLUME DISK...: add disks
    local l=$1; shift
    plan /storage/plan/grow "{\"volume\":\"$l\",\"disks\":$(disks_json "$@")}"
    run POST "/storage/volumes/$l/grow" "{\"disks\":$(disks_json "$@"),\"confirm\":\"$l\",\"serials\":$(serials "$@")${PASSJ:-}}" ok
}
replace() { # VOLUME OLD NEW
    plan /storage/plan/replace "{\"volume\":\"$1\",\"old\":\"$2\",\"new_disk\":\"$3\"}"
    run POST "/storage/volumes/$1/replace" "{\"old\":\"$2\",\"new_disk\":\"$3\",\"confirm\":\"$1\",\"serials\":$(serials "$3")${PASSJ:-}}" ok
}
create() { # LABEL LEVEL DISK...
    local l=$1 lvl=$2; shift 2
    plan /storage/plan/volume "{\"label\":\"$l\",\"level\":\"$lvl\",\"disks\":$(disks_json "$@")${ENCJ:-}}"
    run POST /storage/volumes "{\"label\":\"$l\",\"level\":\"$lvl\",\"disks\":$(disks_json "$@"),\"confirm\":\"$l\",\"serials\":$(serials "$@")${ENCJ:-}${PASSJ:-}}" ok
    speed_up
}
fail_remove() { # VOLUME DISK...: every partition of the disks failed and removed
    local l=$1 md p d; shift
    for md in $(tiers_of "$l"); do
        for d in "$@"; do
            for p in /dev/"$d"p*; do
                if mdadm --detail "/dev/$md" | grep -q "$p\$"; then
                    mdadm "/dev/$md" --fail "$p"
                    mdadm "/dev/$md" --remove "$p"
                fi
            done
        done
    done
}
# FROM=8 or FROM=9 starts at that scenario (debugging)
FROM=${FROM:-1}

# ===================================================================== scenarios 1-7
if [ "$FROM" -le 7 ]; then
step "1. create $F1 on 1+2+2 GiB (1-disk tolerance)"
new_loop 1G; A=$DEV
new_loop 2G; Bd=$DEV
new_loop 2G; C=$DEV
echo "disks: $A(1G) $Bd(2G) $C(2G)"
create "$F1" flex1 "$A" "$Bd" "$C"
findmnt -n -o SOURCE,FSTYPE,OPTIONS "/$F1" || fail "not mounted"
grep "$F1" /etc/fstab /etc/mdadm/mdadm.conf
write_data "$F1"
after "$F1" "raid5x3 raid1x2"
S1=$(fs_size "$F1"); echo "  filesystem: $S1 B"

step "2. add a 3 GiB disk to $F1"
new_loop 3G; D=$DEV
echo "new disk: $D(3G)"
grow "$F1" "$D"
after "$F1" "raid5x4 raid5x3"
S2=$(fs_size "$F1"); echo "  filesystem: $S1 -> $S2 B"
[ "$S2" -gt "$S1" ] || fail "filesystem did not grow"

step "3. replace the smallest disk $A with a 3 GiB disk"
new_loop 3G; E=$DEV
echo "new disk: $E(3G)"
replace "$F1" "$A" "$E"
after "$F1" "raid5x4 raid5x4 raid1x2"
S3=$(fs_size "$F1"); echo "  filesystem: $S2 -> $S3 B"
[ "$S3" -gt "$S2" ] || fail "filesystem did not grow"
for md in $(tiers_of "$F1"); do mdadm --detail "/dev/$md" | grep -q "/dev/${A}p" && fail "$A still in $md"; done

step "4. fail every partition of $Bd, replace it"
for md in $(tiers_of "$F1"); do
    for p in /dev/"$Bd"p*; do
        mdadm --detail "/dev/$md" | grep -q "$p" && mdadm "/dev/$md" --fail "$p"
    done
done
cat /proc/mdstat
sleep 5.5
api GET /storage/tree
echo "$BODY" | jq -r --arg l "$F1" '.volumes[] | select(.label == $l) | .flexible.tiers[] | "    t\(.index) \(.health) missing=\(.missing)"'
new_loop 2G; G=$DEV
echo "new disk: $G(2G)"
replace "$F1" "$Bd" "$G"
after "$F1" "raid5x4 raid5x4 raid1x2"

step "5. remove $C (partitions failed, removed, loop device detached), replace \"missing\""
for md in $(tiers_of "$F1"); do
    for p in /dev/"$C"p*; do
        if mdadm --detail "/dev/$md" | grep -q "$p"; then
            mdadm "/dev/$md" --fail "$p"
            mdadm "/dev/$md" --remove "$p"
        fi
    done
done
losetup -d "/dev/$C" && echo "  $C detached" || echo "  $C still attached"
sleep 5.5
api GET /storage/tree
echo "$BODY" | jq -r --arg l "$F1" '.volumes[] | select(.label == $l) | .flexible.tiers[] | "    t\(.index) \(.health) degraded=\(.degraded)"'
new_loop 2G; H=$DEV
echo "new disk: $H(2G)"
replace "$F1" missing "$H"
after "$F1" "raid5x4 raid5x4 raid1x2"

step "5b. fail and remove every partition of $G (still attached), replace \"missing\" with $G"
fail_remove "$F1" "$G"
blkid -s TYPE /dev/"$G"p* || true
replace "$F1" missing "$G"
echo "$PLAN" | jq -e --arg p "/dev/${G}p1" '.steps | any(.argv == ["wipefs","--all",$p])' >/dev/null \
    || fail "the leftover partitions of $G are not cleared"
after "$F1" "raid5x4 raid5x4 raid1x2"
for md in $(tiers_of "$F1" | head -2); do
    mdadm --detail "/dev/$md" | grep -q "/dev/${G}p" || fail "$G not back in $md"
done

step "6. kill -9 rynasd during a reshape, then complete"
new_loop 3G; I=$DEV
echo "new disk: $I(3G)"
plan /storage/plan/grow "{\"volume\":\"$F1\",\"disks\":[\"$I\"]}"
# slow reshapes down so that one is caught running
echo 1000 >/proc/sys/dev/raid/speed_limit_min
echo 5000 >/proc/sys/dev/raid/speed_limit_max
start POST "/storage/volumes/$F1/grow" "{\"disks\":[\"$I\"],\"confirm\":\"$F1\",\"serials\":[\"$I\"]}"
for _ in $(seq 600); do
    grep -A3 -E "^($(tiers_of "$F1" | paste -sd'|')) " /proc/mdstat | grep -q reshape && break
    sleep 0.2
done
grep -A3 -E "^($(tiers_of "$F1" | paste -sd'|')) " /proc/mdstat | grep reshape || fail "no reshape seen"
kill -9 "$(cat /run/rynas-test/rynasd.pid)"
echo "  rynasd killed during:"; grep -B1 -A3 reshape /proc/mdstat | sed 's/^/    /'
# On a systemd host mdadm hands the reshape over to mdadm-grow-continue@<md>.service, which
# outlives rynasd. Without systemd mdadm keeps watching the reshape in a child of rynasd's,
# which dies with it and leaves the reshape's suspended window behind (I/O to it hangs):
# kill whatever is left of it, then do what that unit does.
for pid in $(ps -eo pid=,comm= | awk '$2 == "mdadm" {print $1}'); do kill -9 "$pid" 2>/dev/null || true; done
for md in $(tiers_of "$F1"); do
    if [ "$(cat "/sys/block/$md/md/sync_action")" = reshape ]; then
        echo "  mdadm --grow --continue /dev/$md (as mdadm-grow-continue@$md.service)"
        setsid mdadm --grow --continue "/dev/$md" >>/var/log/grow-continue.log 2>&1 </dev/null &
    fi
done
echo 500000 >/proc/sys/dev/raid/speed_limit_min; echo 2000000 >/proc/sys/dev/raid/speed_limit_max
start_rynasd
login
flexwait "$F1"
api POST /storage/plan/complete "{\"volume\":\"$F1\"}"; expect 200 "plan complete"
echo "$BODY" | jq -r '.steps[] | "  - " + .description'
[ "$(echo "$BODY" | jq '.steps | length')" -gt 0 ] || fail "nothing to complete after the kill"
api GET /storage/tree
echo "$BODY" | jq -c --arg l "$F1" '.volumes[] | select(.label == $l) | .flexible.pending'
run POST "/storage/volumes/$F1/complete" '{}' ok
after "$F1" "raid5x5 raid5x5 raid5x3"

step "7. \"reboot\": stop everything, assemble from mdadm.conf, activate, mount"
grep "^ARRAY /dev/md/${F1}_t" /etc/mdadm/mdadm.conf
kill "$(cat /run/rynas-test/rynasd.pid)"; sleep 1
umount "/$F1"
vgchange -an "rynas_$F1"
for md in /dev/md/"${F1}"_t*; do mdadm --stop "$md"; done
grep -E "^md" /proc/mdstat && echo "(other arrays of the host)" || true
mdadm --assemble --scan --config=/etc/mdadm/mdadm.conf
for n in $(seq "$(grep -c "^ARRAY /dev/md/${F1}_t" /etc/mdadm/mdadm.conf)"); do
    [ -e "/dev/md/${F1}_t$n" ] || fail "tier $n not assembled"
done
vgchange -ay "rynas_$F1"
mount "/$F1" || fail "mount from fstab"
start_rynasd
login
after "$F1" "raid5x5 raid5x5 raid5x3"

step "7b. hot replace $H with a 3 GiB disk, rynasd killed twice: requested again, then completed"
new_loop 3G; J=$DEV
echo "new disk: $J(3G)"
mapfile -t T < <(tiers_of "$F1")
# one tier at a time copies slowly, so that rynasd is caught waiting for it
slow() { echo 1000 >"/sys/block/$1/md/sync_speed_min"; echo 1000 >"/sys/block/$1/md/sync_speed_max"; }
fast() { echo system >"/sys/block/$1/md/sync_speed_min"; echo system >"/sys/block/$1/md/sync_speed_max"; }
copying() { # MD: wait until md copies onto a member of it
    for _ in $(seq 900); do grep -A3 "^$1 " /proc/mdstat | grep -q recovery && return 0; sleep 0.2; done
    fail "no copy seen on $1"
}
kill_rynasd() {
    kill -9 "$(cat /run/rynas-test/rynasd.pid)"
    for pid in $(ps -eo pid=,comm= | awk '$2 == "mdadm" {print $1}'); do kill -9 "$pid" 2>/dev/null || true; done
    echo "  rynasd killed during:"; grep -A3 "^$1 " /proc/mdstat | sed 's/^/    /' || true
}
REPLACE_J="{\"old\":\"$H\",\"new_disk\":\"$J\",\"confirm\":\"$F1\",\"serials\":$(serials "$J")}"
slow "${T[0]}"
plan /storage/plan/replace "{\"volume\":\"$F1\",\"old\":\"$H\",\"new_disk\":\"$J\"}"
start POST "/storage/volumes/$F1/replace" "$REPLACE_J"
copying "${T[0]}"
kill_rynasd "${T[0]}"
fast "${T[0]}"; slow "${T[1]}"
start_rynasd
login
sleep 5.5
api GET /storage/tree
echo "$BODY" | jq -c --arg l "$F1" '.volumes[] | select(.label == $l) | .flexible.pending'
echo "$BODY" | jq -e --arg l "$F1" '.volumes[] | select(.label == $l) | .flexible.pending | any(.kind == "change_interrupted")' >/dev/null \
    || fail "the interrupted replace is not pending"
# the same request again goes on ($J already holds a member of tier 1)
plan /storage/plan/replace "{\"volume\":\"$F1\",\"old\":\"$H\",\"new_disk\":\"$J\"}"
start POST "/storage/volumes/$F1/replace" "$REPLACE_J"
copying "${T[1]}"
kill_rynasd "${T[1]}"
fast "${T[1]}"
start_rynasd
login
flexwait "$F1"
api POST /storage/plan/complete "{\"volume\":\"$F1\"}"; expect 200 "plan complete"
echo "$BODY" | jq -r '.steps[] | "  - " + .description'
[ "$(echo "$BODY" | jq '.steps | length')" -gt 0 ] || fail "nothing to complete after the second kill"
run POST "/storage/volumes/$F1/complete" '{}' ok
after "$F1" "raid5x5 raid5x5 raid5x4"
for md in $(tiers_of "$F1"); do mdadm --detail "/dev/$md" | grep -q "/dev/${H}p" && fail "$H still in $md"; done
[ "$(lsblk -nro NAME "/dev/$H" | wc -l)" = 1 ] || fail "$H not cleared: $(lsblk -nro NAME "/dev/$H")"
echo "  $H out of every tier and cleared"

fi

# ===================================================================== scenario 8
if [ "$FROM" -le 8 ]; then
step "8. $F2: 2-disk tolerance on 1+2+2+2 GiB (RAID6x4 + three-way RAID1)"
new_loop 1G; K1=$DEV; new_loop 2G; K2=$DEV; new_loop 2G; K3=$DEV; new_loop 2G; K4=$DEV
create "$F2" flex2 "$K1" "$K2" "$K3" "$K4"
write_data "$F2"
after "$F2" "raid6x4 raid1x3"
T1=$(fs_size "$F2")
new_loop 2G; K5=$DEV
echo "new disk: $K5(2G)"
grow "$F2" "$K5"
echo "$PLAN" | jq -e '.warning_items | any(.code == "flex_reduced_tolerance_window")' >/dev/null \
    || fail "no reduced-tolerance warning"
after "$F2" "raid6x5 raid6x4"
T2=$(fs_size "$F2"); echo "  filesystem: $T1 -> $T2 B"
[ "$T2" -gt "$T1" ] || fail "filesystem did not grow"

step "8b. $F2 loses $K4 and $K5 (failed, removed, still attached): a new disk, then $K4 again"
fail_remove "$F2" "$K4" "$K5"
new_loop 2G; K6=$DEV
echo "new disk: $K6(2G)"
replace "$F2" missing "$K6"
echo "$PLAN" | grep -q 'degraded <= 1' || fail "the rebuild does not wait for a tier one disk short"
flexwait "$F2"
show "$F2"
api GET /storage/tree
jq_tiers='.volumes[] | select(.label == $l) | .flexible.tiers'
echo "$BODY" | jq -r --arg l "$F2" "$jq_tiers"'[] | "    t\(.index) \(.health) degraded=\(.degraded)"'
echo "$BODY" | jq -e --arg l "$F2" "$jq_tiers"' | all(.degraded == 1)' >/dev/null || fail "$F2: every tier should miss one disk"
verify_data "$F2"
replace "$F2" missing "$K4"
echo "$PLAN" | jq -e --arg d "$K5" '.steps | all(.destroys | index($d) | not)' >/dev/null \
    || fail "the leftover disk $K5 is erased by a replace that does not name it"
echo "$PLAN" | jq -e --arg d "$K5" '.warning_items | any(.code == "flex_leftover_disk" and .args.subject == $d)' >/dev/null \
    || fail "no warning about the leftover disk $K5"
after "$F2" "raid6x5 raid6x4"
[ "$(lsblk -nro NAME "/dev/$K5" | wc -l)" -gt 1 ] || fail "$K5 lost its partitions"
echo "  $K5 left as it was: $(lsblk -nro NAME "/dev/$K5" | tr '\n' ' ')"
# a growth naming it clears it and uses it
grow "$F2" "$K5"
echo "$PLAN" | jq -e --arg d "$K5" '[.steps[] | select(.destroys == [$d])] | length > 0' >/dev/null \
    || fail "the growth does not clear $K5 first"
after "$F2" "raid6x6 raid6x5"

fi

# ===================================================================== scenario 9
step "9. $F3: encrypted, 1+2+2 GiB, add a disk, lock, unlock"
PASS='correct horse battery'
ENCJ=',"encrypt":true'
PASSJ=",\"passphrase\":\"$PASS\""
new_loop 1G; M1=$DEV; new_loop 2G; M2=$DEV; new_loop 2G; M3=$DEV
create "$F3" flex1 "$M1" "$M2" "$M3"
ENCJ=
cryptsetup status "rynas-$F3" | grep -E 'type|size|device'
write_data "$F3"
after "$F3" "raid5x3 raid1x2"
U1=$(fs_size "$F3")
new_loop 3G; M4=$DEV
grow "$F3" "$M4"
cryptsetup status "rynas-$F3" | grep -E 'size'
after "$F3" "raid5x4 raid5x3"
U2=$(fs_size "$F3"); echo "  filesystem: $U1 -> $U2 B"
[ "$U2" -gt "$U1" ] || fail "filesystem did not grow"
run POST "/storage/volumes/$F3/lock" "" ok
findmnt -n "/$F3" >/dev/null && fail "still mounted"
[ -e "/dev/mapper/rynas-$F3" ] && fail "mapping still open"
run POST "/storage/volumes/$F3/unlock" "{\"passphrase\":\"$PASS\"}" ok
after "$F3" "raid5x4 raid5x3"
PASSJ=

step "all good"
