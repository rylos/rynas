#!/bin/sh
# PC backup (D-PCBACKUP B1) on a real btrfs folder, without any real disk: a loop image in
# a scratch folder, received snapshots made with btrfs send | btrfs receive (as btrbk does),
# then the ignored host test of crates/rynasd/tests/pc_backup_host.rs as root.
#
#   sudo packaging/test-pcbackup-loop.sh <scratch dir> <test binary> [files]
#
# <test binary>: target/.../deps/pc_backup_host-<hash> (cargo test -p rynasd --test
# pc_backup_host --no-run prints it). [files]: files per snapshot (default 20000), for the
# timing of the size measure. Everything is removed at the end, also on failure.
set -eu
[ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 2; }
dir=$1; bin=$2; files=${3:-20000}
img="$dir/pcbackup.img"; mnt="$dir/mnt"
loop=""
cleanup() {
    set +e
    mountpoint -q "$mnt" && umount "$mnt"
    [ -n "$loop" ] && losetup -d "$loop"
    rm -f "$img"; rmdir "$mnt" 2>/dev/null
}
trap cleanup EXIT INT TERM
mkdir -p "$mnt"
truncate -s 1G "$img"
loop=$(losetup --find --show "$img")
mkfs.btrfs -q "$loop"
mount "$loop" "$mnt"

# the PC side: a subvolume with files, three read-only snapshots with btrbk names
btrfs -q subvolume create "$mnt/src"
mkdir -p "$mnt/snaps" "$mnt/src/.rynas" "$mnt/src/data"
i=0
while [ "$i" -lt "$files" ]; do
    head -c $((4096 + i % 8192)) /dev/urandom > "$mnt/src/data/f$i"
    i=$((i + 1))
done
# the oldest snapshot: the manifest is a symlink (must never be followed)
ln -s /etc/shadow "$mnt/src/.rynas/manifest.json"
btrfs -q subvolume snapshot -r "$mnt/src" "$mnt/snaps/@.20261108T0300"
rm "$mnt/src/.rynas/manifest.json"
printf '{"format":1,"boot":{"loader":"systemd-boot"},"btrbk":{"subvolumes":[{"name":"@","mount":"/"}]}}' > "$mnt/src/.rynas/manifest.json"
head -c 50M /dev/urandom > "$mnt/src/data/big1"
btrfs -q subvolume snapshot -r "$mnt/src" "$mnt/snaps/@.20261109T0300"
head -c 30M /dev/urandom > "$mnt/src/data/big2"
btrfs -q subvolume snapshot -r "$mnt/src" "$mnt/snaps/@.20261110T0300"

# the NAS side: the device folder, received incrementally (full, then with parents)
btrfs -q subvolume create "$mnt/pc"
btrfs send -q "$mnt/snaps/@.20261108T0300" | btrfs receive -q "$mnt/pc/"
btrfs send -q -p "$mnt/snaps/@.20261108T0300" "$mnt/snaps/@.20261109T0300" | btrfs receive -q "$mnt/pc/"
btrfs send -q -p "$mnt/snaps/@.20261109T0300" "$mnt/snaps/@.20261110T0300" | btrfs receive -q "$mnt/pc/"
sync

echo "== without quotas"
RYNAS_TEST_PCBACKUP="$mnt/pc" "$bin" --ignored --nocapture --test-threads 1 host_receive

# again with quota groups on (the oldest snapshot was deleted by the first run: receive it back)
btrfs send -q "$mnt/snaps/@.20261108T0300" | btrfs receive -q "$mnt/pc/"
btrfs quota enable "$mnt"
btrfs quota rescan -w "$mnt" >/dev/null
echo "== with quotas"
RYNAS_TEST_PCBACKUP="$mnt/pc" "$bin" --ignored --nocapture --test-threads 1 host_receive
echo "ok"
