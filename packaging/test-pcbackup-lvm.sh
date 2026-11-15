#!/bin/sh
# Bare-metal restore of an LVM PC (agent restore/lvm.rs) on throwaway loop disks, in a
# throwaway privileged container: never on the host's disks.
#
#   packaging/test-pcbackup-lvm.sh <static rynas-agent> [work dir on disk]
#
# The PC is the Arch layout of docs/research/pc-backup.md §4.4: GPT with an ESP at /boot and
# an LVM partition, a volume group with root and home (ext4) and 256 MiB free,
# mkinitcpio (its HOOKS without lvm2, to see it added), systemd-boot with a UKI, the command
# line root=/dev/<vg>/root. Its system files are the container's own (Debian, with
# lvm2), so the restore's chroot steps run; mkinitcpio and bootctl are stand-ins that log.
#
# 1. The manifest is taken by `rynas-agent manifest --print` chrooted into the PC (its
#    mounts are what the chroot sees); the "backup" is a copy of the PC's tree.
# 2. A dry run onto the target while the PC's disk is still attached: refused, the volume
#    group's name is in use on another disk.
# 3. The PC's disk detached; dry run and the real scripted restore onto a disk of the same
#    size (hidden `--test-manifest`/`--test-tree` source, the rest is the scripted mode).
#    Checked: GPT IDs, VG/LV/PV names and UUIDs, sizes and free space, filesystem UUIDs,
#    files, fstab and /etc/kernel/cmdline unchanged, the lvm2 hook in HOOKS when mkinitcpio
#    ran, the group deactivated at the end.
# 4. With that disk detached, the restore onto a smaller disk: volumes shrunk, data there.
#
# The container shares the host's /dev and device mapper. The volume group gets a new name
# on every run, with a dash (Rt-Vg<random>: its mapper names double the dash, as
# ArchinstallVg's would not), refused when the host already has it; only the test's own
# mappings are ever removed. The test's LVM commands see only its loop devices (its own
# lvm.conf through LVM_SYSTEM_DIR: a filter on them, no devices file, no udev sync); the
# restore's look for other disks reads every device's LVM label (read only), as on a real
# PC. At the end: no loop device, no device-mapper node and no volume group of the test is
# left, the container is removed (docker run --rm), the images are deleted.
set -eu

if [ "${1:-}" != "--inside" ]; then
    agent=$(realpath "$1")
    repo=$(cd "$(dirname "$0")/.." && pwd)
    work=${2:-$repo/target/lvm-restore-test}
    mkdir -p "$work"
    work=$(realpath "$work")
    vg="Rt-Vg$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
    vgm=$(printf '%s' "$vg" | sed 's/-/--/g')
    if ls /dev/mapper | grep -qF -- "$vgm-" || [ -e "/dev/$vg" ]; then
        echo "the host already has a volume group named $vg: run again" >&2
        exit 1
    fi
    rc=0
    docker run --rm --privileged -v /dev:/dev -v "$work:/work" -v "$agent:/rynas-agent:ro" \
        -e RYNAS_TEST_VG="$vg" -v "$(realpath "$0"):/test.sh:ro" debian:trixie sh /test.sh --inside || rc=$?
    # nothing of the test may be left on the host
    left=$(losetup -a | grep -F "$work/" || true)
    maps=$(ls /dev/mapper | grep -F -- "$vgm-" || true)
    [ -z "$left$maps" ] || { echo "LEFT ON THE HOST: $left $maps" >&2; rc=1; }
    rm -rf "$work"
    [ "$rc" = 0 ] && echo "== all ok" || echo "== FAILED ($rc)"
    exit "$rc"
fi

# ------------------------------------------------------------------ inside the container
export DEBIAN_FRONTEND=noninteractive
apt-get -qq update >/dev/null
apt-get -qq install -y --no-install-recommends lvm2 e2fsprogs dosfstools fdisk util-linux coreutils >/dev/null

VG=${RYNAS_TEST_VG:?}
# the device-mapper names of the group's volumes (a dash in a group name is doubled)
VGM=$(printf '%s' "$VG" | sed 's/-/--/g')
S=""; T=""; T2=""
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

cleanup() {
    set +e
    for m in /src/work/lvm /src/tmp/rynas-agent /src/run /src/sys /src/proc /src/dev /src/home /src/boot /src /mnt/chk/home /mnt/chk/boot /mnt/chk; do
        mountpoint -q "$m" && umount "$m"
    done
    [ -f /work/lvm/lvm.conf ] && LVM_SYSTEM_DIR=/work/lvm vgchange -an "$VG" >/dev/null 2>&1
    # only the test's own mappings, by their exact names
    for lv in root home; do dmsetup info "$VGM-$lv" >/dev/null 2>&1 && dmsetup remove "$VGM-$lv"; done
    for l in "$S" "$T" "$T2"; do [ -n "$l" ] && losetup -d "$l" 2>/dev/null; done
    rm -f /work/*.img
    rm -rf /work/tree /work/lvm /work/live
}
trap cleanup EXIT INT TERM

# the host must not have the group's name: its mappings, or a group LVM finds on any device
# (a read-only look, without a filter or devices file)
dmsetup ls 2>/dev/null | grep -qF -- "$VGM-" && { echo "the host has device mappings of $VG: run again" >&2; trap - EXIT; exit 1; }
vgs --devicesfile "" --config 'devices{filter=["a|.*|"] global_filter=["a|.*|"] obtain_device_list_from_udev=0}' --foreign --noheadings -o vg_name 2>/dev/null | grep -qx " *$VG" && { echo "a volume group named $VG exists: run again" >&2; trap - EXIT; exit 1; }

for n in src tgt small; do rm -f "/work/$n.img"; done
truncate -s 3G /work/src.img; truncate -s 3G /work/tgt.img; truncate -s 1600M /work/small.img
S=$(losetup -fP --show /work/src.img)
T=$(losetup -fP --show /work/tgt.img)
T2=$(losetup -fP --show /work/small.img)
s=${S#/dev/}; t=${T#/dev/}; t2=${T2#/dev/}
echo "loop disks: PC $S, target $T, smaller target $T2"

# LVM sees only these three disks
mkdir -p /work/lvm
cat > /work/lvm/lvm.conf <<EOF
devices {
    global_filter = [ "a|^/dev/${s}p[0-9]+\$|", "a|^/dev/${t}p[0-9]+\$|", "a|^/dev/${t2}p[0-9]+\$|", "r|.*|" ]
    use_devicesfile = 0
    obtain_device_list_from_udev = 0
    external_device_info_source = "none"
}
activation {
    udev_sync = 0
    udev_rules = 0
    monitoring = 0
}
backup {
    backup = 0
    archive = 0
}
EOF
export LVM_SYSTEM_DIR=/work/lvm
export RYNAS_TEST_LOOP_DISKS=1

# the live system the restore runs on is unhelpful on purpose: a global filter that hides
# every device, a devices file that lists none, and a system ID from the host name. The
# restore must neither miss the PC's group (step 2) nor make the groups foreign or hidden.
mkdir -p /work/live/devices
sed -e 's/global_filter = .*/global_filter = [ "r|.*|" ]/' -e 's/use_devicesfile = 0/use_devicesfile = 1/' /work/lvm/lvm.conf > /work/live/lvm.conf
printf 'global {\n    system_id_source = "uname"\n}\n' >> /work/live/lvm.conf
printf '# LVM uses devices listed in this file.\nVERSION=1.1.1\n' > /work/live/devices/system.devices
restore() { LVM_SYSTEM_DIR=/work/live /rynas-agent restore "$@"; }

# ---- the PC
printf 'label: gpt\nlabel-id: 1E2B8F6D-3643-4D8D-AAC5-AB7EDEBECACC\n\nstart=2048, size=256MiB, type=U, uuid=0397CC02-0000-4000-8000-000000000001\ntype=E6D6D379-F507-44C2-A23C-238F2A3DF928, uuid=3612A9BF-0000-4000-8000-000000000002\n' | sfdisk -q "$S"
partx -u "$S" 2>/dev/null || true
for i in $(seq 50); do [ -b "${S}p2" ] && break; sleep 0.1; done
mkfs.vfat -F 32 -n ESP "${S}p1" >/dev/null
pvcreate -q "${S}p2"
vgcreate -q "$VG" "${S}p2"
lvcreate -q -y -L 1G -n root "$VG"
total=$(vgs --noheadings -o vg_extent_count "$VG" | tr -d ' ')
lvcreate -q -y -l $((total - 256 - 64)) -n home "$VG"
mkfs.ext4 -q "/dev/$VG/root"; mkfs.ext4 -q "/dev/$VG/home"
mkdir -p /src && mount "/dev/$VG/root" /src
mkdir -p /src/home /src/boot && mount "/dev/$VG/home" /src/home && mount -o fmask=0077,dmask=0077 "${S}p1" /src/boot

# its system: the container's own files, made to look like Arch
for d in usr etc var root bin sbin lib lib64; do [ -e "/$d" ] && cp -a "/$d" /src/; done
mkdir -p /src/proc /src/sys /src/dev /src/run /src/tmp && chmod 1777 /src/tmp
printf 'NAME="Arch Linux"\nPRETTY_NAME="Arch Linux"\nID=arch\nBUILD_ID=rolling\n' > /src/etc/os-release
rm -f /src/usr/lib/os-release
printf 'MODULES=()\nBINARIES=()\nFILES=()\nHOOKS=(base systemd autodetect microcode modconf kms keyboard sd-vconsole block filesystems fsck)\n' > /src/etc/mkinitcpio.conf
mkdir -p /src/etc/mkinitcpio.d /src/etc/kernel
printf "ALL_kver='/boot/vmlinuz-linux'\nPRESETS=('default')\ndefault_uki='/boot/EFI/Linux/arch-linux.efi'\n" > /src/etc/mkinitcpio.d/linux.preset
cmdline="root=/dev/$VG/root zswap.enabled=0 rw rootfstype=ext4"
echo "$cmdline" > /src/etc/kernel/cmdline
cat > /src/usr/bin/mkinitcpio <<'EOF'
#!/bin/sh
# stand-in: logs its arguments and the HOOKS it would use
{ echo "args: $*"; grep -h '^HOOKS=' /etc/mkinitcpio.conf /etc/mkinitcpio.conf.d/*.conf 2>/dev/null; } >> /var/log/fake-mkinitcpio.log
EOF
cat > /src/usr/bin/bootctl <<'EOF'
#!/bin/sh
echo "args: $*" >> /var/log/fake-bootctl.log
EOF
chmod 755 /src/usr/bin/mkinitcpio /src/usr/bin/bootctl
mkdir -p /src/boot/EFI/systemd /src/boot/EFI/BOOT /src/boot/EFI/Linux /src/boot/loader
head -c 65536 /dev/urandom > /src/boot/EFI/systemd/systemd-bootx64.efi
cp /src/boot/EFI/systemd/systemd-bootx64.efi /src/boot/EFI/BOOT/BOOTX64.EFI
head -c 1048576 /dev/urandom > /src/boot/EFI/Linux/arch-linux.efi
printf 'timeout 3\n' > /src/boot/loader/loader.conf
mkdir -p /src/home/user
head -c 20M /dev/urandom > /src/home/user/data.bin
echo "restored from LVM" > /src/etc/rynas-marker
# the PC's lvm.conf has a device filter on its disk, and a devices file with its PV and a PV
# of a second disk (outside the backup): the restore rewrites the one, keeps the other's line
# (and uses its devices file, as newer Fedora and RHEL do)
awk -v f="    filter = [ \"a|^/dev/${s}p2\$|\", \"r|.*|\" ]" '{ print } /^devices \{/ && !done { print f; print "    use_devicesfile = 1"; done = 1 }' /src/etc/lvm/lvm.conf > /src/etc/lvm/lvm.conf.new
mv /src/etc/lvm/lvm.conf.new /src/etc/lvm/lvm.conf
grep -q "^    filter = \[ \"a|^/dev/${s}p2\\$|\"" /src/etc/lvm/lvm.conf || fail "the PC's lvm.conf filter was not written"
u_root=$(blkid -s UUID -o value "/dev/$VG/root"); u_home=$(blkid -s UUID -o value "/dev/$VG/home"); u_esp=$(blkid -s UUID -o value "${S}p1")
printf 'UUID=%s / ext4 rw,relatime 0 1\nUUID=%s /home ext4 rw,relatime 0 2\nUUID=%s /boot vfat rw,fmask=0077,dmask=0077 0 2\n' "$u_root" "$u_home" "$u_esp" > /src/etc/fstab
sum=$(sha256sum /src/home/user/data.bin | cut -d' ' -f1)
vg_uuid=$(vgs --noheadings -o vg_uuid "$VG" | tr -d ' ')
pv_uuid=$(pvs --noheadings -o pv_uuid "${S}p2" | tr -d ' ')
lvs_before=$(lvs --noheadings -o lv_name,lv_uuid,seg_size_pe --units b "$VG" | sort)
free_before=$(vgs --noheadings -o vg_free_count "$VG" | tr -d ' ')
pvid=$(echo "$pv_uuid" | tr -d -)
mkdir -p /src/etc/lvm/devices
printf '# LVM uses devices listed in this file.\nVERSION=1.1.4\nIDTYPE=devname IDNAME=/dev/%sp2 DEVNAME=/dev/%sp2 PVID=%s PART=2\nIDTYPE=sys_wwid IDNAME=naa.5000000000000001 DEVNAME=/dev/sdx1 PVID=OtherDiskPV0000000000000000000000 PART=1\n' "$s" "$s" "$pvid" > /src/etc/lvm/devices/system.devices
# the GPT's IDs, starts and types (the restore writes first-lba 34, and the last partition
# reaches the end of the disk)
gpt_ids() { sfdisk --dump "$1" | grep -E '^(label-id|/dev)' | sed -e "s|$1|DISK|" -e 's/size= *[0-9]*, //'; }
gpt_before=$(gpt_ids "$S")

# ---- 1. the manifest, from inside the PC
mount --bind /dev /src/dev; mount -t proc proc /src/proc; mount --bind /sys /src/sys; mount -t tmpfs tmpfs /src/run
touch /src/tmp/rynas-agent && mount --bind /rynas-agent /src/tmp/rynas-agent
# the PC's lvm reads the test's lvm.conf too (only the loop disks)
mkdir -p /src/work/lvm && mount --bind /work/lvm /src/work/lvm
chroot /src /tmp/rynas-agent manifest --print > /work/manifest.json
for m in /src/work/lvm /src/tmp/rynas-agent /src/run /src/sys /src/proc /src/dev; do umount "$m"; done
rm -f /src/tmp/rynas-agent; rmdir /src/work/lvm /src/work
grep -q '"lvm_report"' /work/manifest.json || fail "the manifest has no LVM report"
grep -q "$vg_uuid" /work/manifest.json || fail "the manifest has no VG UUID"
grep -q "vg_extent_size" /work/manifest.json || { grep lvm_report /work/manifest.json; fail "the manifest has no extent size"; }
grep -q '"kind": "lvm"' /work/manifest.json || fail "no filesystem on LVM in the manifest"
grep -q '"loader": "systemd_boot"' /work/manifest.json || fail "loader"
grep -q '"initramfs": "mkinitcpio"' /work/manifest.json || fail "initramfs tool"
ok "manifest: LVM report with UUIDs and extent size, systemd-boot, mkinitcpio"

# the "backup": the PC's whole tree as a version holds it
mkdir -p /work/tree && cp -a /src/. /work/tree/

# ---- 2. the PC's disk still attached: its group's name is taken
out=$(restore --test-manifest /work/manifest.json --test-tree /work/tree --disk "$T" --dry-run 2>&1) && fail "a dry run with the PC's group attached passed: $out"
echo "$out" | grep -q "volume group named $VG is on another attached disk" || fail "unexpected refusal: $out"
ok "refused while a group of the same name is attached"

umount /src/boot /src/home /src
vgchange -an "$VG" >/dev/null
losetup -d "$S"; S=""
ok "PC disk detached"

# ---- 3. same size
restore --test-manifest /work/manifest.json --test-tree /work/tree --disk "$T" --dry-run > /work/dry.txt 2>&1 || { cat /work/dry.txt; fail "dry run"; }
for w in "pvcreate --uuid $pv_uuid --norestorefile ${T}p2" "vgcreate -s 4096k $VG ${T}p2" "lvcreate -l 256 -n root $VG" "mkfs.ext4 -F -q -U $u_root /dev/$VG/root" "lvm2 hook"; do
    grep -qF -- "$w" /work/dry.txt || { cat /work/dry.txt; fail "dry run lacks: $w"; }
done
ok "dry run"
sed 's/^/  | /' /work/dry.txt
restore --test-manifest /work/manifest.json --test-tree /work/tree --disk "$T" --confirm "$t" --no-nvram > /work/run.txt 2>&1 || { cat /work/run.txt; fail "restore"; }
grep -q "restore finished" /work/run.txt || { cat /work/run.txt; fail "no end"; }
grep -q "check failed" /work/run.txt && { grep "check failed" /work/run.txt; fail "the restore reported problems"; }
[ -z "$(dmsetup ls | grep -F -- "$VGM-" || true)" ] || fail "the group is still active after the restore"
ok "restore ran; group deactivated at the end"
grep -E '^\$ (pvcreate|vgcreate|lvcreate|vgcfg|vgchange|mkfs|chroot /mnt/rynas-restore mkinitcpio)|lvm2 hook|UUIDs set|restore finished' /work/run.txt | sed 's/^/  | /'

gpt_after=$(gpt_ids "$T")
[ "$gpt_before" = "$gpt_after" ] || fail "GPT differs: $gpt_before // $gpt_after"
[ "$(vgs --noheadings -o vg_uuid "$VG" | tr -d ' ')" = "$vg_uuid" ] || fail "VG UUID"
[ "$(pvs --noheadings -o pv_uuid "${T}p2" | tr -d ' ')" = "$pv_uuid" ] || fail "PV UUID"
[ "$(lvs --noheadings -o lv_name,lv_uuid,seg_size_pe --units b "$VG" | sort)" = "$lvs_before" ] || fail "LV names, UUIDs or sizes: $(lvs "$VG")"
free_after=$(vgs --noheadings -o vg_free_count "$VG" | tr -d ' ')
[ "$free_after" -ge "$free_before" ] && [ "$free_after" -le $((free_before + 1)) ] || fail "free extents: $free_after, the PC had $free_before"
vgchange -ay "$VG" >/dev/null
[ "$(blkid -s UUID -o value "/dev/$VG/root")" = "$u_root" ] && [ "$(blkid -s UUID -o value "/dev/$VG/home")" = "$u_home" ] && [ "$(blkid -s UUID -o value "${T}p1")" = "$u_esp" ] || fail "filesystem UUIDs"
mkdir -p /mnt/chk && mount "/dev/$VG/root" /mnt/chk && mount "/dev/$VG/home" /mnt/chk/home && mount "${T}p1" /mnt/chk/boot
[ "$(sha256sum /mnt/chk/home/user/data.bin | cut -d' ' -f1)" = "$sum" ] || fail "data"
cmp -s /mnt/chk/etc/fstab /work/tree/etc/fstab || fail "fstab changed"
[ "$(cat /mnt/chk/etc/kernel/cmdline)" = "$cmdline" ] || fail "cmdline: $(cat /mnt/chk/etc/kernel/cmdline)"
grep -q '^HOOKS=(.* block lvm2 filesystems .*)' /mnt/chk/etc/mkinitcpio.conf || fail "HOOKS: $(grep HOOKS /mnt/chk/etc/mkinitcpio.conf)"
grep -q '^args: -P' /mnt/chk/var/log/fake-mkinitcpio.log && grep -q 'block lvm2 filesystems' /mnt/chk/var/log/fake-mkinitcpio.log || fail "mkinitcpio: $(cat /mnt/chk/var/log/fake-mkinitcpio.log)"
grep -q 'install --esp-path=/boot' /mnt/chk/var/log/fake-bootctl.log || fail "bootctl"
[ -z "$(vgs --noheadings -o vg_systemid "$VG" | tr -d ' ')" ] || fail "the group got the live system's system ID: $(vgs -o vg_name,vg_systemid "$VG")"
grep -q "^    filter = \[ \"a|^/dev/${t}p2\\$|\", \"r|.\*|\" \]" /mnt/chk/etc/lvm/lvm.conf || fail "lvm.conf filter: $(grep -n '^ *filter' /mnt/chk/etc/lvm/lvm.conf)"
grep -q "device filter rewritten" /work/run.txt || fail "no filter rewrite in the log"
grep -q "names the old disk: rewritten to" /work/dry.txt || fail "the dry run does not say the filter is rewritten"
cmp -s /mnt/chk/etc/lvm/devices/system.devices.rynas-old /work/tree/etc/lvm/devices/system.devices || fail "the PC's devices file was not kept"
if [ -f /mnt/chk/etc/lvm/devices/system.devices ]; then
    d=/mnt/chk/etc/lvm/devices/system.devices
    grep -q "PVID=OtherDiskPV" "$d" || fail "the other disk's line is gone: $(cat "$d")"
    grep -q "DEVNAME=/dev/${s}p2" "$d" && fail "the old PV line stayed: $(cat "$d")"
    grep -q "PVID=$pvid" "$d" || fail "the restored PV is not in the devices file: $(cat "$d")"
    ok "devices file: the other disk's line kept, the restored PV added by vgimportdevices"
else
    grep "vgimportdevices" /work/run.txt | sed 's/^/  | /'
    fail "the devices file is gone"
fi
ls /mnt/chk/var/log/rynas-restore-*.log >/dev/null || fail "restore log"
cmp -s /mnt/chk/boot/EFI/Linux/arch-linux.efi /work/tree/boot/EFI/Linux/arch-linux.efi || fail "UKI"
ok "same size: GPT, VG/PV/LV names+UUIDs+sizes, no system ID, free space, FS UUIDs, files, fstab, cmdline, lvm.conf filter, lvm2 hook before mkinitcpio -P"
umount /mnt/chk/boot /mnt/chk/home /mnt/chk
vgchange -an "$VG" >/dev/null

# ---- 4. a smaller disk (the first target detached: its group would be in the way)
losetup -d "$T"; T=""
restore --test-manifest /work/manifest.json --test-tree /work/tree --disk "$T2" --confirm "$t2" --no-nvram > /work/small.txt 2>&1 || { cat /work/small.txt; fail "smaller restore"; }
grep -q "note: $VG/home: .* → " /work/small.txt || { cat /work/small.txt; fail "no shrink note"; }
grep -E '^note: |lvcreate -l' /work/small.txt | sed 's/^/  | /' 
grep -q "check failed" /work/small.txt && fail "problems on the smaller disk"
vgchange -ay "$VG" >/dev/null
[ "$(vgs --noheadings -o vg_uuid "$VG" | tr -d ' ')" = "$vg_uuid" ] || fail "VG UUID (smaller)"
lvs "$VG"
mount "/dev/$VG/root" /mnt/chk && mount "/dev/$VG/home" /mnt/chk/home
[ "$(sha256sum /mnt/chk/home/user/data.bin | cut -d' ' -f1)" = "$sum" ] || fail "data (smaller)"
[ "$(blkid -s UUID -o value "/dev/$VG/home")" = "$u_home" ] || fail "home UUID (smaller)"
umount /mnt/chk/home /mnt/chk
vgchange -an "$VG" >/dev/null
ok "smaller disk: volumes shrunk, UUIDs and data kept"
echo "inside: all ok"
