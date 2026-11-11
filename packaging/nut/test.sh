#!/usr/bin/env bash
# Install test of rynas's NUT backport (packaging/nut/build.sh) in a throwaway Debian 13
# container: Debian's NUT 2.8.1 first, an /etc/nut written like rynas's UPS setup does, then
# the update with exactly rynasd's apt-get line (crates/rynasd/src/ups_update.rs). Checks:
# no conffile question, the configuration untouched, NUT 2.8.5 with nutdrv_qx's armac
# subdriver, richcomm_usb without USB matching options, and apt's --no-remove refusing the
# update when a driver package of the old version (nut-snmp) is installed.
#
#   packaging/nut/test.sh            (after build.sh; needs Docker, nothing runs on the host)
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# shellcheck source=versions.env
. "$HERE/versions.env"
OUT="$ROOT/target/nut"
ls "$OUT"/*_"$NUT_DEBIAN_VERSION"~rynas13+"$PKG_REVISION"_*.deb >/dev/null || { echo "run build.sh first" >&2; exit 1; }

# shellcheck disable=SC2016  # the script runs in the container
docker run --rm -i -e DEBIAN_SNAPSHOT="$DEBIAN_SNAPSHOT" -v "$OUT:/debs:ro" "$BUILDER_IMAGE" bash -euo pipefail <<'SCRIPT'
export DEBIAN_FRONTEND=noninteractive LC_ALL=C
fail() { echo "FAIL: $*" >&2; exit 1; }
cat > /etc/apt/sources.list.d/debian.sources <<EOF
Types: deb
URIs: http://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT
Suites: trixie trixie-updates
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp
Check-Valid-Until: no

Types: deb
URIs: http://snapshot.debian.org/archive/debian-security/$DEBIAN_SNAPSHOT
Suites: trixie-security
Components: main
Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp
Check-Valid-Until: no
EOF
apt-get update -qq
# "Install NUT": Debian's package
apt-get install -y -qq --no-install-recommends nut >/dev/null
dpkg-query -W -f '${Package} ${Version}\n' nut nut-server nut-client
/lib/nut/richcomm_usb -h 2>&1 | grep -q vendorid && fail "2.8.1 richcomm_usb takes vendorid?" || echo "2.8.1 richcomm_usb: no vendorid option"
# an /etc/nut as UPS setup leaves it (rynas blocks in Debian's conffiles)
cat >> /etc/nut/ups.conf <<'EOF'
# BEGIN rynas ups-device
[ups]
	driver = "nutdrv_qx"
	port = "auto"
	vendorid = "0925"
	productid = "1234"
	subdriver = "armac"
	protocol = "megatec"
	norating
# END rynas ups-device
EOF
sed -i 's/^MODE=.*/MODE=standalone/' /etc/nut/nut.conf
printf '# BEGIN rynas ups-monitor\n[rynas-upsmon]\n\tpassword = x\n\tupsmon primary\n# END rynas ups-monitor\n' >> /etc/nut/upsd.users
printf 'MONITOR ups@localhost 1 rynas-upsmon x primary\n' >> /etc/nut/upsmon.conf
# the files rynas changed must stay as they are (the untouched ones follow the package)
CHANGED="nut.conf ups.conf upsd.users upsmon.conf"
before=$(cd /etc/nut && sha256sum $CHANGED)

TARGETS=(/debs/*_*~rynas13+*_*.deb)
# rynasd's simulation, then its install line (crate::updates::apt + ups_update)
apt-get -s -q -o Debug::NoLocking=true --no-remove install -- "${TARGETS[@]}" >/dev/null
apt-get -q -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold \
    install --no-remove -- "${TARGETS[@]}" </dev/null > /tmp/apt.log 2>&1 || { cat /tmp/apt.log; fail "install"; }
# --force-confold keeps the admin's files without asking (dpkg: "Keeping old config file")
grep -q "What would you like to do" /tmp/apt.log && { cat /tmp/apt.log; fail "conffile question"; }
echo "$(grep -c "Keeping old config file" /tmp/apt.log || true) changed conffiles kept without a question"
after=$(cd /etc/nut && sha256sum $CHANGED)
[ "$before" = "$after" ] || { diff <(echo "$before") <(echo "$after"); fail "/etc/nut changed"; }
ls /etc/nut/*.dpkg-* 2>/dev/null && echo "(the new samples, as a Debian upgrade leaves them)"
dpkg-query -W -f '${Package} ${Version}\n' nut nut-server nut-client libupsclient7 libnutscan4
upsdrvctl -V 2>&1 | grep -q " 2\.8\.5" || fail "upsdrvctl is not 2.8.5"
/usr/libexec/nut/nutdrv_qx -V 2>&1 | head -n 1
/usr/libexec/nut/nutdrv_qx -h 2>&1 | grep -qi armac || fail "nutdrv_qx has no armac subdriver"
echo "nutdrv_qx: armac subdriver present"
/usr/libexec/nut/richcomm_usb -h 2>&1 | grep -q vendorid && fail "2.8.5 richcomm_usb takes vendorid?" || echo "2.8.5 richcomm_usb: no vendorid option"
/usr/libexec/nut/usbhid-ups -h 2>&1 | grep -q vendorid || fail "usbhid-ups lost vendorid"
[ -f /usr/lib/udev/rules.d/62-nut-usbups.rules ] || fail "udev rules missing"
grep -q 'ATTR{idVendor}=="0925", ATTR{idProduct}=="1234"' /usr/lib/udev/rules.d/62-nut-usbups.rules || fail "no udev rule for 0925:1234"
echo "udev rule for 0925:1234 present"

# a second system: Debian's NUT with nut-snmp; the update must be refused, nothing removed
apt-get purge -y -qq nut nut-server nut-client libupsclient7 libnutscan4 >/dev/null
apt-get install -y -qq --no-install-recommends nut nut-snmp > /tmp/snmp.log 2>&1 || { cat /tmp/snmp.log; fail "nut-snmp install"; }
if apt-get -s -q -o Debug::NoLocking=true --no-remove install -- "${TARGETS[@]}" > /tmp/sim.log 2>&1; then
    cat /tmp/sim.log; fail "the update would go ahead with nut-snmp 2.8.1 installed"
fi
grep -E "^E:" /tmp/sim.log | head -n 2
echo "with nut-snmp installed: refused before anything runs"
echo "OK"
SCRIPT
