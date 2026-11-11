#!/usr/bin/env bash
# The deduplication engine for real (D-DEDUP, docs/phase2/dedup.md §4.4): the rynas-oans
# package in a throw-away Debian 13 container booted with systemd (privileged: loop-mounted
# btrfs and XFS image files, systemd as PID 1 for the transient units). No disk of the host
# is touched.
#
#   packaging/test-dedup.sh [--no-build]
#
# Runs the engine the way rynasd does: a transient unit with rynas's properties (SIGINT with
# 5 minutes to save, ProtectSystem=strict with the roots writable, the reduced capability
# set) and rynas's command line (every option, anchored excludes, roots after `--`). Checks:
# known duplicates are shared; mtime and ctime of deduplicated files do not change; a
# read-only snapshot and the #snapshot bind mount are untouched; #recycle and no
# copy-on-write folders are skipped; files of another user with mode 0600 are deduplicated
# (capabilities enough); the end of the window (RuntimeMaxSec) stops the engine with
# SIGINT, and the next run goes on; a missing root gives exit 2; XFS with reflink=1.
# Runs on every rynas-oans bump (packaging/oans/check-upstream.sh).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-systemd:trixie-2
NAME=rynas-test-dedup-$$

if [ "${1:-}" != "--inside" ]; then
    if [ "${1:-}" != "--no-build" ]; then
        "$ROOT/packaging/oans/build.sh"
    fi
    # shellcheck source=oans/versions.env
    . "$ROOT/packaging/oans/versions.env"
    DEB="$ROOT/target/oans/rynas-oans_${OANS_VERSION}-${PKG_REVISION}_amd64.deb"
    [ -f "$DEB" ] || { echo "missing $DEB: run without --no-build" >&2; exit 1; }
    docker build -q -t "$IMAGE" - >/dev/null <<'DOCKERFILE'
FROM debian:trixie
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends systemd systemd-sysv dbus curl \
 && rm -rf /var/lib/apt/lists/* /usr/sbin/policy-rc.d \
 && systemctl mask getty.target console-getty.service systemd-vconsole-setup.service systemd-firstboot.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
DOCKERFILE
    docker run -d --name "$NAME" -e container=docker --privileged --cgroupns=private --tmpfs /run --tmpfs /run/lock \
        --tmpfs /tmp -v "$DEB:/opt/rynas/rynas-oans.deb:ro" \
        -v "$ROOT/packaging:/opt/rynas/packaging:ro" "$IMAGE" >/dev/null
    trap 'docker rm -f "$NAME" >/dev/null' EXIT
    sleep 3
    docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash /opt/rynas/packaging/test-dedup.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
step() { printf '\n=== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
OANS=/usr/lib/rynas/oans/oans
# loop devices are kernel objects of the host: the container's /dev has no node for a new
# one (no udev), so it is made here; every one is detached at the end, whatever happens
attach() {
    local n
    n=$(losetup -f | tr -dc '0-9')
    [ -e "/dev/loop$n" ] || mknod "/dev/loop$n" b 7 "$n"
    losetup "/dev/loop$n" "$1"
    echo "/dev/loop$n"
}
cleanup() {
    for m in /srv/vol1/docs/#snapshot /srv/vol1 /srv/data; do mountpoint -q "$m" && umount -l "$m"; done
    for img in /var/tmp/vol1.img /var/tmp/xfs.img; do
        [ -f "$img" ] || continue
        losetup -j "$img" | cut -d: -f1 | while read -r l; do losetup -d "$l" || true; done
    done
}
trap cleanup EXIT

step "packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends btrfs-progs xfsprogs e2fsprogs jq /opt/rynas/rynas-oans.deb >/dev/null 2>&1
dpkg-query -W -f '${Package} ${Version}\n' rynas-oans btrfs-progs xfsprogs systemd
"$OANS" --version
[ ! -e /usr/bin/duperemove ] && [ ! -e /usr/bin/oans ] || fail "the package must stay out of PATH"
uname -r

# rynas's unit properties (rynas_dedup::engine::unit_properties) and command line
# (rynas_dedup::engine::Oans::command); $1: RuntimeMaxSec, then the writable folders
props() {
    local max=$1; shift
    local p=(--property=Nice=19 --property=IOSchedulingClass=idle --property=CPUWeight=20
        --property=KillSignal=SIGINT --property=TimeoutStopSec=5min --property=SendSIGKILL=yes
        --property=ProtectSystem=strict --property=PrivateTmp=yes --property=PrivateNetwork=yes
        --property=NoNewPrivileges=yes
        "--property=CapabilityBoundingSet=CAP_DAC_READ_SEARCH CAP_DAC_OVERRIDE CAP_FOWNER"
        --property=MemoryHigh=768M --property=MemoryMax=1024M "--property=RuntimeMaxSec=$max")
    for w in "$@"; do p+=("--property=ReadWritePaths=-$w"); done
    printf '%s\n' "${p[@]}"
}
# run_oans <max s> <hashfile> <dedupe 0|1> <excludes (newline separated)> <roots...>;
# progress lines go to /tmp/progress.jsonl, the exit code to $RC
run_oans() {
    local max=$1 hash=$2 dedupe=$3 excludes=$4; shift 4
    local args=("$OANS" -r)
    [ "$dedupe" = 1 ] && args+=(-d)
    args+=("--hashfile=$hash" -b 128K --min-filesize=64K --skip-zeroes --skip-readonly-subvols
        --dedupe-options=same,nopartial,noonly_whole_files --cpu-threads=2 -q --no-color --progress=json)
    while IFS= read -r e; do [ -n "$e" ] && args+=("--exclude=$e"); done <<< "$excludes"
    args+=(-- "$@")
    mapfile -t p < <(props "$max" "$@" "$(dirname "$hash")")
    set +e
    systemd-run --wait --pipe --collect --quiet --service-type=exec --unit="rynas-dedup-test-$RANDOM" \
        "${p[@]}" -- "${args[@]}" > /tmp/stdout.txt 2> /tmp/progress.jsonl
    RC=$?
    set -e
    echo "exit $RC; $(grep -c '^{' /tmp/progress.jsonl || true) progress lines"
}
done_reclaimed() { grep '"event":"done"' /tmp/progress.jsonl | jq -r .reclaimed_bytes | tail -1; }
times() { stat -c '%n %.9Y %.9Z' "$@"; }
exclusive() { btrfs filesystem du --raw "$1" | awk 'NR==2 {print $2}'; }

step "a btrfs volume: shares, recycle bin, a snapshot, a no copy-on-write folder"
truncate -s 2G /var/tmp/vol1.img
mkfs.btrfs -q /var/tmp/vol1.img
mkdir -p /srv/vol1
VOL1=$(attach /var/tmp/vol1.img)
mount "$VOL1" /srv/vol1
for s in docs media vms; do btrfs -q subvolume create "/srv/vol1/$s"; done
mkdir -p /srv/vol1/@rynas-cache/dedup /srv/vol1/.snapshots /srv/vol1/docs/#recycle /srv/vol1/docs/sub
chattr +C /srv/vol1/vms
useradd -M -s /usr/sbin/nologin alice
# a read-only snapshot first (so the files below are not shared with it), holding a file
# whose content comes back later as a new live file with its own extents
head -c 8M /dev/urandom > /tmp/snapblob
cp --reflink=never /tmp/snapblob /srv/vol1/docs/s.bin
sync
btrfs -q subvolume snapshot -r /srv/vol1/docs /srv/vol1/.snapshots/docs.1
rm /srv/vol1/docs/s.bin
cp --reflink=never /tmp/snapblob /srv/vol1/docs/s2.bin
head -c 32M /dev/urandom > /tmp/blob
cp --reflink=never /tmp/blob /srv/vol1/docs/a.bin
cp --reflink=never /tmp/blob /srv/vol1/docs/sub/b.bin
cp --reflink=never /tmp/blob /srv/vol1/media/c.bin
cp --reflink=never /tmp/blob /srv/vol1/docs/#recycle/r.bin
cp --reflink=never /tmp/blob /srv/vol1/vms/disk.img
# another user's private file: dedupe must still be allowed (CAP_FOWNER / CAP_DAC_OVERRIDE)
cp --reflink=never /tmp/blob /srv/vol1/media/private.bin
chown alice: /srv/vol1/media/private.bin
chmod 0600 /srv/vol1/media/private.bin
sync
mkdir -p /srv/vol1/docs/#snapshot
mount --bind -o ro /srv/vol1/.snapshots /srv/vol1/docs/#snapshot
sync
gen_before=$(btrfs subvolume show /srv/vol1/.snapshots/docs.1 | awk '/Generation:/ {print $2}')
times_before=$(times /srv/vol1/docs/a.bin /srv/vol1/docs/sub/b.bin /srv/vol1/media/c.bin /srv/vol1/media/private.bin)
sleep 1.1

EXCLUDES='/srv/vol1/docs/\#snapshot/
/srv/vol1/docs/\#recycle/
/srv/vol1/media/\#snapshot/
/srv/vol1/media/\#recycle/
.snapshots/
@rynas-cache/
.replicas/
.rynas-crypt/
.*.rynas-part
.*.rynas-aside
/srv/vol1/docker/engine/'
HASH=/srv/vol1/@rynas-cache/dedup/volume.hash

step "estimate (no -d): nothing changes"
run_oans 600 "$HASH" 0 "$EXCLUDES" /srv/vol1/docs /srv/vol1/media
[ "$RC" = 0 ] || fail "estimate exit $RC"
est=$("$OANS" --json --hashfile="$HASH")
echo "$est" | jq -c '{files_tracked, dup_files, reclaimable_logical_bytes, scan_configured_dedupe}'
[ "$(echo "$est" | jq .scan_configured_dedupe)" = false ] || fail "an estimate stores no -d"
[ "$(exclusive /srv/vol1/docs/a.bin)" -gt 0 ] || fail "the estimate changed something"

step "deduplication with rynas's unit and command line"
run_oans 600 "$HASH" 1 "$EXCLUDES" /srv/vol1/docs /srv/vol1/media /srv/vol1/vms
[ "$RC" = 0 ] || { cat /tmp/progress.jsonl | tail -20; fail "dedupe exit $RC"; }
rec=$(done_reclaimed)
echo "reclaimed $rec bytes"
[ "${rec:-0}" -ge $((32 << 20)) ] || fail "expected at least 32 MiB reclaimed"
btrfs filesystem du -s /srv/vol1/docs /srv/vol1/media /srv/vol1/vms
for f in /srv/vol1/docs/a.bin /srv/vol1/docs/sub/b.bin /srv/vol1/media/c.bin /srv/vol1/media/private.bin; do
    echo "$f exclusive $(exclusive "$f")"
done
shared=0
for f in /srv/vol1/docs/sub/b.bin /srv/vol1/media/c.bin /srv/vol1/media/private.bin; do
    [ "$(exclusive "$f")" = 0 ] && shared=$((shared + 1))
done
[ "$shared" -ge 2 ] || fail "the copies were not shared"
[ "$(exclusive /srv/vol1/media/private.bin)" = 0 ] || fail "another user's 0600 file was not deduplicated: capabilities"
[ "$(exclusive /srv/vol1/docs/#recycle/r.bin)" -ge $((32 << 20)) ] || fail "#recycle was deduplicated"
[ "$(exclusive /srv/vol1/vms/disk.img)" -ge $((32 << 20)) ] || fail "the no copy-on-write file was deduplicated"

step "times of deduplicated files"
times_after=$(times /srv/vol1/docs/a.bin /srv/vol1/docs/sub/b.bin /srv/vol1/media/c.bin /srv/vol1/media/private.bin)
echo "$times_before"
echo "$times_after"
if [ "$times_before" = "$times_after" ]; then
    echo "RESULT: mtime and ctime unchanged by FIDEDUPERANGE on btrfs ($(uname -r))"
else
    fail "mtime or ctime changed: backups would read every deduplicated file again"
fi

step "the read-only snapshot is untouched"
gen_after=$(btrfs subvolume show /srv/vol1/.snapshots/docs.1 | awk '/Generation:/ {print $2}')
[ "$gen_before" = "$gen_after" ] || fail "snapshot generation $gen_before → $gen_after"
[ "$(btrfs property get -ts /srv/vol1/.snapshots/docs.1 ro)" = "ro=true" ] || fail "snapshot no longer read-only"
[ "$(exclusive /srv/vol1/docs/s2.bin)" -gt 0 ] || fail "a live file was deduplicated into the snapshot"

step "the end of the window: SIGINT, work saved, the next run goes on"
for i in $(seq 1 6); do head -c 64M /dev/urandom > "/tmp/big$i"; cp --reflink=never "/tmp/big$i" "/srv/vol1/docs/big$i.a"; cp --reflink=never "/tmp/big$i" "/srv/vol1/media/big$i.b"; done
rm -f /tmp/big*
sync
echo 3 > /proc/sys/vm/drop_caches || true
run_oans 1 "$HASH" 1 "$EXCLUDES" /srv/vol1/docs /srv/vol1/media
echo "systemd-run exit after RuntimeMaxSec: $RC (oans: 130 on SIGINT)"
grep -q '"event":"done"' /tmp/progress.jsonl && echo "note: the run finished before its window ended"
run_oans 600 "$HASH" 1 "$EXCLUDES" /srv/vol1/docs /srv/vol1/media
[ "$RC" = 0 ] || fail "resume exit $RC"
[ "$(exclusive /srv/vol1/media/big6.b)" = 0 ] || fail "the resumed run did not finish the work"
"$OANS" --history --hashfile="$HASH" | tail -5

step "a missing root: exit 2, the rest done"
run_oans 600 "$HASH" 1 "$EXCLUDES" /srv/vol1/docs /srv/vol1/missing
[ "$RC" = 2 ] || fail "expected exit 2, got $RC"

step "XFS with reflink"
truncate -s 1G /var/tmp/xfs.img
mkfs.xfs -q -m reflink=1 /var/tmp/xfs.img
mkdir -p /srv/data
XFS=$(attach /var/tmp/xfs.img)
mount "$XFS" /srv/data
xfs_info /srv/data | grep -o 'reflink=[01]'
mkdir -p /srv/data/share /var/lib/rynas/dedup
cp --reflink=never /tmp/blob /srv/data/share/x.bin
cp --reflink=never /tmp/blob /srv/data/share/y.bin
sync
xtimes_before=$(times /srv/data/share/x.bin /srv/data/share/y.bin)
sleep 1.1
run_oans 600 /var/lib/rynas/dedup/uuid.test.hash 1 '/srv/data/share/\#recycle/' /srv/data/share
[ "$RC" = 0 ] || fail "XFS dedupe exit $RC"
echo "reclaimed $(done_reclaimed) bytes"
filefrag -v /srv/data/share/y.bin | grep -q shared || fail "XFS extents not shared"
xtimes_after=$(times /srv/data/share/x.bin /srv/data/share/y.bin)
[ "$xtimes_before" = "$xtimes_after" ] && echo "RESULT: mtime and ctime unchanged by FIDEDUPERANGE on XFS ($(uname -r))" \
    || fail "XFS: mtime or ctime changed"

step "done"
echo "all deduplication checks passed"
