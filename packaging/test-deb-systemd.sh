#!/usr/bin/env bash
# Install the rynas .deb in a Debian 13 container booted with systemd and check that the
# maintainer scripts enable and start the units, that the hardened units work, and that
# remove/purge clean up. Needs a privileged container (systemd as PID 1).
#
#   packaging/test-deb-systemd.sh [path/to/rynas_*.deb]
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEB=${1:-$(ls -t "$ROOT"/target/debian/rynas_*_amd64.deb | head -1)}
IMAGE=rynas-test-systemd:trixie-2
NAME=rynas-test-systemd-$$

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
    --tmpfs /tmp -v "$DEB:/tmp/rynas.deb:ro" "$IMAGE" >/dev/null
trap 'docker rm -f "$NAME" >/dev/null' EXIT
sleep 3

docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash -s <<'TEST'
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
active() { { systemctl is-active rynasd.socket rynasd.service rynas-web.service || true; } | tr '\n' ' '; echo; }
login() {
    curl -s -c /tmp/j -H 'Content-Type: application/json' \
        -d '{"username":"alice","password":"Al1ce-test-pw"}' -o /dev/null -w '%{http_code}' \
        http://127.0.0.1:8080/api/v1/auth/login
}
apt-get update -qq
echo "=== install"
apt-get install -y -qq /tmp/rynas.deb >/dev/null 2>&1
sleep 1; active
[ "$(active)" = "active active active " ] || fail "units not active after install"
useradd -m -G rynas-admin alice; echo 'alice:Al1ce-test-pw' | chpasswd
[ "$(login)" = 200 ] || fail "login"
[ "$(curl -s -b /tmp/j -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/api/v1/system)" = 200 ] || fail "system"
echo "PAM login and /api/v1/system: OK"
systemd-analyze security --no-pager rynas-web.service rynasd.service | tail -2
echo "=== upgrade (reinstall)"
apt-get install -y -qq --reinstall /tmp/rynas.deb >/dev/null 2>&1; sleep 1; active
echo "=== cold start of rynas-web alone (pulls the socket and rynasd)"
systemctl stop rynas-web rynasd.service rynasd.socket; systemctl start rynas-web; sleep 1; active
[ "$(login)" = 200 ] || fail "login after cold start"
echo "=== remove"
apt-get remove -y -qq rynas >/dev/null 2>&1; active
echo "=== purge"
apt-get purge -y -qq rynas >/dev/null 2>&1
! ls /etc/systemd/system/*.wants 2>/dev/null | grep -q rynas || fail "enable links left"
! getent passwd rynas >/dev/null || fail "user left"
[ ! -e /var/lib/rynas ] || fail "state left"
journalctl -b --no-pager | grep -E 'rynas.*(refusing|Failed)' && fail "unit errors in journal" || true
echo "ALL CHECKS PASSED"
TEST
