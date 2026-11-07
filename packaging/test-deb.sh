#!/usr/bin/env bash
# Install and exercise the rynas .deb in a throw-away Debian 13 container (no systemd: the
# daemons are started by hand with the ExecStart lines of the units).
#
#   packaging/test-deb.sh [path/to/rynas_*.deb]
#
# Checks: install, user/groups/permissions, a real PAM login over HTTP with a test user in
# rynas-admin, refusal of a non-admin and of a wrong password, precompressed UI, lintian,
# apt remove and apt purge.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)

if [ "${1:-}" != "--inside" ]; then
    DEB=${1:-$(ls -t "$ROOT"/target/debian/rynas_*_amd64.deb | head -1)}
    exec docker run --rm -e DEBIAN_FRONTEND=noninteractive \
        -v "$DEB:/tmp/$(basename "$DEB"):ro" -v "$ROOT/packaging/test-deb.sh:/test.sh:ro" \
        debian:trixie bash /test.sh --inside "/tmp/$(basename "$DEB")"
fi
DEB=$2
step() { printf '\n=== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

step "apt install ./$(basename "$DEB")"
apt-get update -qq
apt-get install -y -qq curl lintian >/dev/null
apt-get install -y "$DEB" 2>&1 | grep -v '^\(Get\|Selecting\|Preparing\|Unpacking\|Reading\)' | tail -15
dpkg -s rynas | grep -E '^(Status|Version|Depends)'

step "users, groups, permissions"
getent passwd rynas
getent group rynas rynas-admin
stat -c '%A %U:%G %n' /var/lib/rynas /var/lib/rynas/auth /etc/pam.d/rynas /etc/default/rynas-web \
    /usr/libexec/rynas/rynasd /usr/libexec/rynas/rynas-web /usr/share/rynas/ui
[ "$(stat -c '%a %U:%G' /var/lib/rynas)" = "750 root:rynas" ] || fail "/var/lib/rynas"
[ "$(stat -c '%a %U:%G' /var/lib/rynas/auth)" = "700 root:root" ] || fail "/var/lib/rynas/auth"
getent passwd rynas | grep -q ':/var/lib/rynas:/usr/sbin/nologin$' || fail "rynas home/shell"
ls /etc/systemd/system/*.wants/ 2>/dev/null | grep rynas || true
cat /var/lib/rynas/../systemd/deb-systemd-helper-enabled/*.dsh-also 2>/dev/null | sort -u || true
ls -l --time-style=+ /usr/lib/systemd/system/rynas* | awk '{print $1, $NF}'

step "postinst is idempotent (dpkg-reconfigure)"
dpkg-reconfigure rynas
apt-get install -y --reinstall "$DEB" >/dev/null 2>&1
getent group rynas-admin >/dev/null || fail "rynas-admin lost"

step "test users: alice (rynas-admin), bob (no admin group)"
useradd -m -s /bin/bash -G rynas-admin alice
useradd -m -s /bin/bash bob
echo 'alice:Al1ce-test-pw' | chpasswd
echo 'bob:B0b-test-pw' | chpasswd

step "start rynasd (root) and rynas-web (user rynas) as the units do"
mkdir -p -m 0755 /run/rynas
RUST_LOG=info /usr/libexec/rynas/rynasd --socket /run/rynas/rynasd.sock --state-dir /var/lib/rynas/auth \
    >/tmp/rynasd.log 2>&1 &
for _ in $(seq 50); do [ -S /run/rynas/rynasd.sock ] && break; sleep 0.1; done
stat -c '%A %U:%G %n' /run/rynas/rynasd.sock
# shellcheck disable=SC1091
. /etc/default/rynas-web
setpriv --reuid=rynas --regid=rynas --init-groups --no-new-privs \
    /usr/libexec/rynas/rynas-web --listen "$RYNAS_WEB_LISTEN" --ui-dir /usr/share/rynas/ui \
    --rynasd-socket /run/rynas/rynasd.sock $RYNAS_WEB_OPTS >/tmp/rynas-web.log 2>&1 &
case $RYNAS_WEB_LISTEN in
    auto) PORT=8080 ;;
    *) PORT=${RYNAS_WEB_LISTEN##*:} ;;
esac
B="http://127.0.0.1:$PORT"
for _ in $(seq 50); do curl -s -o /dev/null "$B/" && break; sleep 0.1; done
ps -o user,pid,rss,args -C rynasd,rynas-web

step "unauthenticated /api/v1/system"
curl -s -w ' HTTP %{http_code}\n' "$B/api/v1/system"

login() { curl -s -c "/tmp/$1.jar" -H 'Content-Type: application/json' \
    -d "{\"username\":\"$1\",\"password\":\"$2\"}" -w '\nHTTP %{http_code}\n' "$B/api/v1/auth/login"; }

step "PAM login alice (rynas-admin)"
out=$(login alice 'Al1ce-test-pw'); echo "$out"
echo "$out" | grep -q 'HTTP 200' || fail "alice login"
step "GET /api/v1/system with the session cookie"
curl -s -b /tmp/alice.jar -w '\nHTTP %{http_code}\n' "$B/api/v1/system" | tee /tmp/system.out
grep -q 'HTTP 200' /tmp/system.out || fail "system with cookie"
step "logout (CSRF header)"
csrf=$(echo "$out" | head -1 | sed -n 's/.*"csrf_token":"\([^"]*\)".*/\1/p')
curl -s -b /tmp/alice.jar -X POST -H "X-CSRF-Token: $csrf" -w 'HTTP %{http_code}\n' "$B/api/v1/auth/logout"
curl -s -b /tmp/alice.jar -o /dev/null -w 'after logout: HTTP %{http_code}\n' "$B/api/v1/system"

step "PAM login alice, wrong password"
login alice 'wrong-password' | tail -2
step "PAM login bob (right password, not an administrator)"
login bob 'B0b-test-pw' | tail -2

step "UI: index and precompressed wasm"
curl -s -o /dev/null -w 'index: HTTP %{http_code} %{size_download} B\n' "$B/"
wasm=$(curl -s "$B/" | grep -oE '/rynas-ui-[0-9a-f]+_bg\.wasm' | head -1)
curl -s -o /dev/null -D - -H 'Accept-Encoding: br, gzip' "$B$wasm" | grep -iE '^(HTTP|content-encoding|content-length|cache-control)'
curl -s -o /dev/null -D - -H 'Accept-Encoding: gzip' "$B$wasm" | grep -iE '^(content-encoding|content-length)'

step "logs"
cat /tmp/rynasd.log /tmp/rynas-web.log

step "lintian"
lintian --allow-root --info --display-info --pedantic "$DEB" || true

step "stop daemons, apt remove"
pkill -x rynas-web || true; pkill -x rynasd || true; sleep 0.5
apt-get remove -y rynas >/dev/null 2>&1
dpkg -s rynas | grep '^Status'
getent passwd rynas >/dev/null && echo "user rynas kept"
[ -f /etc/pam.d/rynas ] && [ -f /etc/default/rynas-web ] && echo "conffiles kept"
[ -d /var/lib/rynas ] && echo "/var/lib/rynas kept"
[ ! -e /usr/libexec/rynas/rynasd ] || fail "binary left after remove"

step "apt purge"
apt-get purge -y rynas >/dev/null 2>&1
dpkg -s rynas 2>&1 | head -1 || true
getent passwd rynas && fail "user rynas left" || echo "user rynas removed"
getent group rynas rynas-admin && fail "groups left" || echo "groups rynas, rynas-admin removed"
[ -e /var/lib/rynas ] && fail "/var/lib/rynas left" || echo "/var/lib/rynas removed"
[ -e /etc/pam.d/rynas ] && fail "conffile left" || echo "conffiles removed"
id alice
echo; echo "ALL CHECKS PASSED"
