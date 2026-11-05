#!/usr/bin/env bash
# Safe networking (D-NET) for real, in a throw-away Debian 13 container booted with systemd
# (privileged: its own network namespace with veth pairs; this host's network is never
# touched).
#
#   packaging/test-network.sh [--no-build]
#
# Builds debug rynasd/rynasctl/rynas-web against trixie's glibc (builder image of
# build-deb.sh), then inside the container: two "NAS ports" (veth-nas, veth-nas2) whose peers
# live in a namespace `peer` that plays the LAN (gateway 192.0.2.1, admin PC on
# 198.51.100.1). The administrator logs in from `peer` over veth-nas2 and, through the API:
#   1. changes veth-nas's address (a confirmation over loopback is refused, the one arriving on
#      the new address is accepted), then the management interface itself (the timed test
#      cannot be skipped; confirmed on its new address) and back;
#   2. changes it again without confirming: the rollback timer (systemd-run) restores the
#      previous address and MTU and verifies them;
#   3. starts a change and "reboots" (timer lost, networkd stopped, addresses flushed): the
#      boot guard restores the last known good files before networkd starts again;
#   4. moves the ports to ifupdown, adopts them (live), lets the trial expire (ifupdown comes
#      back), adopts again and confirms;
#   5. diagnostics, rescue info (deterministic link-local address active), reset button
#      double press (factory network with the previous settings kept as a revision).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-systemd:trixie-2
NAME=rynas-test-network-$$
BIN=$ROOT/target/deb-build/debug

if [ "${1:-}" != "--inside" ]; then
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
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends systemd systemd-sysv dbus curl \
 && rm -rf /var/lib/apt/lists/* /usr/sbin/policy-rc.d \
 && systemctl mask getty.target console-getty.service systemd-vconsole-setup.service systemd-firstboot.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
DOCKERFILE
    docker run -d --name "$NAME" -e container=docker --privileged --cgroupns=private --tmpfs /run --tmpfs /run/lock \
        --tmpfs /tmp -v "$BIN/rynasd:/opt/rynas/rynasd:ro" -v "$BIN/rynas-web:/opt/rynas/rynas-web:ro" \
        -v "$BIN/rynasctl:/opt/rynas/rynasctl:ro" \
        -v "$ROOT/packaging:/opt/rynas/packaging:ro" "$IMAGE" >/dev/null
    trap 'docker rm -f "$NAME" >/dev/null' EXIT
    sleep 3
    docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash /opt/rynas/packaging/test-network.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
step() { printf '\n=== %s\n' "$*"; }
fail() {
    echo "FAIL: $*" >&2
    journalctl -u rynasd -u 'rynas-net-*' -u systemd-networkd --no-pager | tail -60 >&2
    ip -br addr >&2
    exit 1
}
P() { ip netns exec peer "$@"; }

step "packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends ifupdown iproute2 iputils-ping \
    iputils-arping jq passwd >/dev/null 2>&1
dpkg-query -W -f '${Package} ${Version}\n' systemd ifupdown iproute2 iputils-arping

step "the LAN: veth pairs into the namespace 'peer'"
# networkd waits for udev when /sys is writable; containers have no udev, and the container
# interface wants a read-only /sys anyway (as systemd-nspawn does)
mount -o remount,ro /sys
ip netns add peer
ip link add veth-nas type veth peer name veth-peer
ip link add veth-nas2 type veth peer name veth-peer2
ip link set veth-peer netns peer
ip link set veth-peer2 netns peer
P ip link set lo up
P ip addr add 192.0.2.1/24 dev veth-peer
P ip addr add 198.51.100.1/24 dev veth-peer2
P ip link set veth-peer up
P ip link set veth-peer2 up
ip link set veth-nas up
ip link set veth-nas2 up
# the NAS side starts on networkd with a foreign file (like the synodeb image)
cat > /etc/systemd/network/10-test.network <<'EOF'
[Match]
Name=veth-nas

[Network]
Address=192.0.2.10/24
Gateway=192.0.2.1
EOF
cat > /etc/systemd/network/11-test.network <<'EOF'
[Match]
Name=veth-nas2

[Network]
Address=198.51.100.10/24
EOF
# containers have no physical ports: these two count as ports (test-only switch)
systemctl set-environment RYNAS_NET_TEST_PORTS="veth-nas veth-nas2"
systemctl enable --now systemd-networkd.service >/dev/null 2>&1
for _ in $(seq 50); do ip -4 -o addr show dev veth-nas | grep -q 192.0.2.10 && break; sleep 0.2; done
ip -4 -o addr show dev veth-nas | grep -q 192.0.2.10/24 || fail "networkd did not configure veth-nas"
networkctl list --no-pager

step "rynas installed as the package does it (units with their hardening)"
install -d /usr/libexec/rynas /usr/share/rynas/ui
install -m 755 /opt/rynas/rynasd /opt/rynas/rynas-web /opt/rynas/rynasctl /usr/libexec/rynas/
echo '<!doctype html><title>rynas</title>' > /usr/share/rynas/ui/index.html
addgroup --system --quiet rynas
adduser --system --quiet --ingroup rynas --home /var/lib/rynas --no-create-home --shell /usr/sbin/nologin rynas
addgroup --system --quiet rynas-admin
install -d -m 0750 -o root -g rynas /var/lib/rynas
install -d -m 0700 /var/lib/rynas/auth
install -m 644 /opt/rynas/packaging/pam.d/rynas /etc/pam.d/rynas
for u in rynasd.service rynasd.socket rynas-web.service rynas-net-guard.service \
    rynas-net-watchdog.service rynas-net-watchdog.timer; do
    install -m 644 "/opt/rynas/packaging/systemd/$u" /etc/systemd/system/
done
echo 'RYNAS_WEB_LISTEN=0.0.0.0:8080' > /etc/default/rynas-web
systemctl daemon-reload
systemctl enable rynas-net-guard.service >/dev/null 2>&1
systemctl start rynasd.socket rynas-web
useradd -m -s /bin/bash -G rynas-admin admin1
echo 'admin1:Adm1n-test-pw' | chpasswd
for _ in $(seq 50); do P curl -s -o /dev/null http://198.51.100.10:8080/ && break; sleep 0.1; done

MGMT=http://198.51.100.10:8080/api/v1
P curl -s -c /tmp/jar -H 'Content-Type: application/json' -o /tmp/session.json \
    -d '{"username":"admin1","password":"Adm1n-test-pw"}' "$MGMT/auth/login"
CSRF=$(jq -r .csrf_token /tmp/session.json)
SID=$(awk '$6=="rynas_session"{print $7}' /tmp/jar)
[ -n "$CSRF" ] && [ "$CSRF" != null ] && [ -n "$SID" ] || fail "login: $(cat /tmp/session.json)"
echo "logged in as admin1 from the peer, over veth-nas2"

# api METHOD PATH [JSON] [BASE] -> body in $BODY, HTTP status in $CODE (always from the peer)
api() {
    local out
    out=$(P curl -s -m 30 -H "Cookie: rynas_session=$SID" -H "X-CSRF-Token: $CSRF" \
        -H 'Content-Type: application/json' -X "$1" ${3:+-d "$3"} -w '\n%{http_code}' "${4:-$MGMT}$2") || true
    CODE=${out##*$'\n'}
    BODY=${out%$'\n'*}
}
expect() { [ "$CODE" = "$1" ] || fail "$2: HTTP $CODE $BODY"; }
wait_addr() { # iface cidr [present|absent] [tries]
    for _ in $(seq "${4:-100}"); do
        if ip -4 -o addr show dev "$1" | grep -qw "${2%/*}/${2#*/}"; then
            [ "${3:-present}" = present ] && return 0
        else
            [ "${3:-present}" = absent ] && return 0
        fi
        sleep 0.3
    done
    fail "$1: $2 not ${3:-present}"
}
wait_task() {
    local id=$1 st
    for _ in $(seq "${2:-300}"); do
        api GET "/tasks/$id"
        st=$(echo "$BODY" | jq -r .status)
        case $st in ok|error|cancelled) break ;; esac
        sleep 0.5
    done
    TASK_STATUS=$st
    P curl -s -H "Cookie: rynas_session=$SID" "$MGMT/tasks/$id/log" | sed 's/^/    | /'
}
no_trial() {
    for _ in $(seq 60); do
        api GET /network/state
        [ "$(echo "$BODY" | jq -r .trial)" = null ] && return 0
        sleep 0.5
    done
    fail "the trial did not end: $BODY"
}

step "inventory: interfaces, state, management interface"
api GET /network/interfaces; expect 200 "interfaces"
echo "$BODY" | jq -c '.[] | {name, kind, backend, editable, management, carrier, mtu, applied, link_local_v4, addresses: [.addresses[] | select(.family=="ipv4") | .address]}'
[ "$(echo "$BODY" | jq -r '.[] | select(.name=="veth-nas") | .backend')" = networkd ] || fail "veth-nas backend"
[ "$(echo "$BODY" | jq -r '.[] | select(.name=="veth-nas") | .editable')" = true ] || fail "veth-nas not editable"
[ "$(echo "$BODY" | jq -r '.[] | select(.name=="veth-nas2") | .management')" = true ] || fail "management interface not detected"
[ "$(echo "$BODY" | jq -r '.[] | select(.name=="eth0") | .editable')" = false ] || fail "docker's eth0 must not be editable"
api GET /network/state; expect 200 "state"
echo "$BODY" | jq -c '{services, management_interface, trial, timeout_default_s, routes: [.routes[] | select(.destination=="default")]}'
[ "$(echo "$BODY" | jq -r .management_interface)" = veth-nas2 ] || fail "state: management interface"

step "validation: bad subnet, gateway outside, MTU over the driver limit, zero management paths"
api PUT /network/interfaces/veth-nas '{"ipv4":"static","addresses":["192.0.2.0/24"]}'; expect 400 "network address"
api PUT /network/interfaces/veth-nas '{"ipv4":"static","addresses":["192.0.2.20/24"],"gateway":"10.0.0.1"}'; expect 400 "gateway outside"
api PUT /network/interfaces/veth-nas '{"ipv4":"dhcp","mtu":70000}'; expect 400 "mtu"
api PUT /network/interfaces/eth0 '{"ipv4":"dhcp"}'; expect 409 "eth0 is not managed by networkd"
api PUT /network/interfaces/veth-nas '{"ipv4":"disabled"}'; expect 200 "stage disabled"
api PUT /network/interfaces/veth-nas2 '{"ipv4":"disabled"}'; expect 200 "stage disabled 2"
api POST /network/apply '{}'; expect 400 "zero management paths"; echo "refused: $(echo "$BODY" | jq -r .message)"
api POST /network/discard; expect 204 "discard"
api POST /network/apply '{}'; expect 409 "nothing staged"

step "1. change veth-nas, confirm over the new address"
api PUT /network/interfaces/veth-nas '{"ipv4":"static","addresses":["192.0.2.20/24"],"gateway":"192.0.2.1"}'
expect 200 "stage"; echo "$BODY" | jq -c '{name, pending}'
api POST /network/apply '{"timeout_s":60}'; expect 202 "apply"
echo "$BODY" | jq -c .
TOKEN=$(echo "$BODY" | jq -r .token); TASK=$(echo "$BODY" | jq -r .task_id)
echo "$BODY" | jq -e '.candidate_urls | index("http://192.0.2.20:8080/")' >/dev/null || fail "candidate URL"
wait_addr veth-nas 192.0.2.20/24
wait_addr veth-nas 192.0.2.10/24 absent
grep -q 'Address=192.0.2.20/24' /etc/systemd/network/05-rynas-veth-nas.network || fail "rynas file"
grep -q 'LinkLocalAddressing=yes' /etc/systemd/network/05-rynas-veth-nas.network || fail "link-local"
head -1 /etc/systemd/network/05-rynas-veth-nas.network | grep -q '^# Managed by rynas' || fail "managed header"
systemctl list-timers --all --no-pager | grep -q rynas-net-rollback || fail "rollback timer not armed"
[ -f /var/lib/rynas/net/trial.json ] || fail "trial marker"
api GET /network/state; echo "trial: $(echo "$BODY" | jq -c .trial)"
# over loopback (an SSH tunnel, a local proxy) nothing about the new network is proven
out=$(curl -s -m 10 -H "Cookie: rynas_session=$SID" -H "X-CSRF-Token: $CSRF" -H 'Content-Type: application/json' \
    -d "{\"token\":\"$TOKEN\"}" -w '\n%{http_code}' http://127.0.0.1:8080/api/v1/network/confirm)
[ "${out##*$'\n'}" = 409 ] || fail "confirmation over loopback: $out"
echo "confirmation over loopback refused: $(echo "${out%$'\n'*}" | jq -r .message)"
api POST /network/confirm '{"token":"wrong"}' http://192.0.2.20:8080/api/v1; expect 400 "wrong token"
api POST /network/confirm "{\"token\":\"$TOKEN\"}" http://192.0.2.20:8080/api/v1; expect 200 "confirm over the new address"
echo "$BODY" | jq -c .
wait_task "$TASK"
[ "$TASK_STATUS" = ok ] || fail "apply task: $TASK_STATUS"
systemctl list-timers --all --no-pager | grep -q rynas-net-rollback && fail "timer still armed"
[ -f /var/lib/rynas/net/trial.json ] && fail "trial marker still there"
P ping -c 1 -W 2 192.0.2.20 >/dev/null || fail "new address does not answer"

step "1b. change the management interface itself: mandatory test, confirmed on its new address"
api PUT /network/interfaces/veth-nas2 '{"ipv4":"static","addresses":["198.51.100.11/24"]}'; expect 200 "stage mgmt"
api POST /network/apply '{"skip_test":true}'; expect 400 "skip the test on the management interface"
echo "skip_test refused: $(echo "$BODY" | jq -r .message)"
api POST /network/apply '{"timeout_s":60}'; expect 202 "apply mgmt"
[ "$(echo "$BODY" | jq -r .management_touched)" = true ] || fail "management_touched"
TOKEN=$(echo "$BODY" | jq -r .token); TASK=$(echo "$BODY" | jq -r .task_id)
wait_addr veth-nas2 198.51.100.11/24
NEW=http://198.51.100.11:8080/api/v1
sleep 1
api POST /network/confirm "{\"token\":\"$TOKEN\"}" "$NEW"; expect 200 "confirm on the new management address"
MGMT=$NEW wait_task "$TASK"
[ "$TASK_STATUS" = ok ] || fail "mgmt task: $TASK_STATUS"
# and back, from the new address
api PUT /network/interfaces/veth-nas2 '{"ipv4":"static","addresses":["198.51.100.10/24"]}' "$NEW"; expect 200 "stage back"
api POST /network/apply '{"timeout_s":60}' "$NEW"; expect 202 "apply back"
TOKEN=$(echo "$BODY" | jq -r .token); TASK=$(echo "$BODY" | jq -r .task_id)
wait_addr veth-nas2 198.51.100.10/24
api POST /network/confirm "{\"token\":\"$TOKEN\"}"; expect 200 "confirm back"
wait_task "$TASK"
[ "$TASK_STATUS" = ok ] || fail "mgmt back task: $TASK_STATUS"

step "2. change without confirmation: automatic rollback by the timer, verified"
api PUT /network/interfaces/veth-nas '{"ipv4":"static","addresses":["192.0.2.30/24"],"gateway":"192.0.2.1","mtu":1400}'
expect 200 "stage"
api POST /network/apply '{"timeout_s":60}'; expect 202 "apply"
TASK=$(echo "$BODY" | jq -r .task_id)
wait_addr veth-nas 192.0.2.30/24
[ "$(cat /sys/class/net/veth-nas/mtu)" = 1400 ] || fail "MTU 1400 not applied"
echo "applied 192.0.2.30/24 MTU 1400; not confirming, waiting for the timer..."
wait_task "$TASK" 200
[ "$TASK_STATUS" = error ] || fail "an unconfirmed change must end in error, got $TASK_STATUS"
wait_addr veth-nas 192.0.2.20/24
wait_addr veth-nas 192.0.2.30/24 absent
[ "$(cat /sys/class/net/veth-nas/mtu)" = 1500 ] || fail "MTU not restored: $(cat /sys/class/net/veth-nas/mtu)"
journalctl -u rynas-net-rollback --no-pager | grep -q 'restored and verified' || fail "the timer did not roll back"
journalctl -u rynas-net-rollback --no-pager | tail -8 | sed 's/^/    timer| /'
api GET /network/state
echo "$BODY" | jq -c .last_rollback
[ "$(echo "$BODY" | jq -r .last_rollback.verified)" = true ] || fail "rollback not verified"
[ "$(echo "$BODY" | jq -r .last_rollback.reason)" = timeout ] || fail "rollback reason"
api GET "/notifications?limit=5"; echo "$BODY" | jq -c '.[] | {severity, key, title}' | head -3
api POST /network/discard; expect 204 "discard"

step "3. reboot during a trial: the boot guard restores the last known good files"
api PUT /network/interfaces/veth-nas '{"ipv4":"static","addresses":["192.0.2.40/24"]}'; expect 200 "stage"
api POST /network/apply '{"timeout_s":600}'; expect 202 "apply"
wait_addr veth-nas 192.0.2.40/24
# power cut: the transient timer is gone, the kernel forgets the addresses
systemctl stop rynas-net-rollback.timer
systemctl stop rynasd.service
systemctl stop systemd-networkd.socket systemd-networkd.service
ip addr flush dev veth-nas scope global
[ -f /var/lib/rynas/net/trial.json ] || fail "the marker must survive the crash"
systemctl start rynas-net-guard.service
journalctl -u rynas-net-guard --no-pager | tail -4 | sed 's/^/    guard| /'
[ -f /var/lib/rynas/net/trial.json ] && fail "the guard left the marker"
grep -q 'Address=192.0.2.20/24' /etc/systemd/network/05-rynas-veth-nas.network || fail "guard did not restore the file"
systemctl start systemd-networkd.service
wait_addr veth-nas 192.0.2.20/24
wait_addr veth-nas 192.0.2.40/24 absent
systemctl start rynasd.socket
api GET /network/state; expect 200 "state after the guard"
[ "$(echo "$BODY" | jq -r .last_rollback.reason)" = boot_guard ] || fail "boot guard report"
api POST /network/discard; expect 204 "discard"

step "4. ifupdown: read-only, adoption, expiry (ifupdown back), adoption confirmed"
rm -f /etc/systemd/network/05-rynas-*.network /etc/systemd/network/1?-test.network
systemctl stop systemd-networkd.socket systemd-networkd.service
systemctl disable systemd-networkd.service systemd-networkd.socket >/dev/null 2>&1
ip addr flush dev veth-nas scope global; ip addr flush dev veth-nas2 scope global
cat > /etc/network/interfaces <<'EOF'
source /etc/network/interfaces.d/*

auto lo
iface lo inet loopback

allow-hotplug veth-nas
iface veth-nas inet static
	address 192.0.2.20/24
	gateway 192.0.2.1

auto veth-nas2
iface veth-nas2 inet static
	address 198.51.100.10
	netmask 255.255.255.0
EOF
cp /etc/network/interfaces /tmp/interfaces.orig
systemctl restart networking.service
wait_addr veth-nas2 198.51.100.10/24
# the inventory is cached for 2 s
sleep 2.5
api GET /network/interfaces; expect 200 "interfaces (ifupdown)"
echo "$BODY" | jq -c '.[] | select(.name|startswith("veth-nas")) | {name, backend, editable, read_only_reason, ifupdown: [.ifupdown[] | {method, address, gateway}]}'
[ "$(echo "$BODY" | jq -r '.[] | select(.name=="veth-nas") | .backend')" = ifupdown ] || fail "ifupdown backend"
api PUT /network/interfaces/veth-nas '{"ipv4":"dhcp"}'; expect 409 "ifupdown is read-only"
api GET /network/state; [ "$(echo "$BODY" | jq -r .ifupdown_adoptable)" = true ] || fail "adoptable"
api POST /network/adopt '{"mode":"now"}'; expect 400 "adopt without confirm"
api POST /network/adopt '{"mode":"now","confirm":true,"timeout_s":60}'; expect 202 "adopt"
echo "$BODY" | jq -c '{management_touched, candidate_urls}'
TASK=$(echo "$BODY" | jq -r .task_id)
for _ in $(seq 100); do systemctl is-active -q systemd-networkd && break; sleep 0.3; done
wait_addr veth-nas2 198.51.100.10/24
wait_addr veth-nas 192.0.2.20/24
cmp -s /etc/network/interfaces.rynas-bak /tmp/interfaces.orig || fail "backup of interfaces"
grep -q 'iface lo inet loopback' /etc/network/interfaces && ! grep -q veth-nas /etc/network/interfaces || fail "interfaces not reduced"
networkctl list --no-pager | grep veth-nas
echo "adopted live; not confirming, waiting for the rollback..."
wait_task "$TASK" 200
[ "$TASK_STATUS" = error ] || fail "unconfirmed adoption must end in error, got $TASK_STATUS"
cmp -s /etc/network/interfaces /tmp/interfaces.orig || fail "interfaces not restored"
[ -e /etc/network/interfaces.rynas-bak ] && fail "backup left behind"
systemctl is-active -q systemd-networkd && fail "networkd still running after the rollback"
systemctl is-active -q networking || fail "ifupdown not back"
wait_addr veth-nas 192.0.2.20/24
wait_addr veth-nas2 198.51.100.10/24
api GET /network/state; echo "after the rollback: $(echo "$BODY" | jq -c '{services, last_rollback}')"
[ "$(echo "$BODY" | jq -r .last_rollback.verified)" = true ] || fail "adoption rollback not verified"

api POST /network/adopt '{"mode":"now","confirm":true,"timeout_s":120}'; expect 202 "adopt again"
TOKEN=$(echo "$BODY" | jq -r .token); TASK=$(echo "$BODY" | jq -r .task_id)
for _ in $(seq 100); do systemctl is-active -q systemd-networkd && break; sleep 0.3; done
wait_addr veth-nas2 198.51.100.10/24
sleep 2
api POST /network/confirm "{\"token\":\"$TOKEN\"}"; expect 200 "confirm the adoption"
wait_task "$TASK"
[ "$TASK_STATUS" = ok ] || fail "adoption task: $TASK_STATUS"
sleep 2.5
api GET /network/interfaces
[ "$(echo "$BODY" | jq -r '.[] | select(.name=="veth-nas") | .backend')" = networkd ] || fail "networkd after adoption"
[ "$(echo "$BODY" | jq -r '.[] | select(.name=="veth-nas") | .managed_by_rynas')" = true ] || fail "managed by rynas"
systemctl is-enabled -q networking && fail "networking still enabled"

step "5. diagnostics, rescue paths, reset button"
api POST /network/diagnostics/ping '{"host":"192.0.2.1","count":2}'; expect 200 "ping"
echo "$BODY" | jq -c '{tool, ok, exit_code, duration_ms}'
[ "$(echo "$BODY" | jq -r .ok)" = true ] || fail "ping the gateway: $(echo "$BODY" | jq -r .output)"
api POST /network/diagnostics/dns '{"name":"localhost"}'; expect 200 "dns"; echo "$BODY" | jq -c '{tool, ok}'
api POST /network/diagnostics/ping '{"host":"-f 192.0.2.1"}'; expect 400 "ping injection"
api POST /network/diagnostics/nope '{}'; expect 404 "unknown diagnostic"
for _ in $(seq 40); do
    api GET /network/rescue
    [ "$(echo "$BODY" | jq -r '.link_local[] | select(.interface=="veth-nas") | .active')" = true ] && break
    sleep 0.5
done
echo "$BODY" | jq -c .
LL=$(echo "$BODY" | jq -r '.link_local[] | select(.interface=="veth-nas") | .address')
ip -4 -o addr show dev veth-nas | grep -q "$LL" || fail "deterministic link-local $LL not on veth-nas"
[ -f /etc/avahi/services/rynas.service ] && grep -q _rynas._tcp /etc/avahi/services/rynas.service \
    || fail "avahi service file"
export RYNAS_NET_TEST_PORTS="veth-nas veth-nas2"
/usr/libexec/rynas/rynasctl net reset-button
sleep 1.5
/usr/libexec/rynas/rynasctl net reset-button
grep -q 'DHCP=ipv4' /etc/systemd/network/05-rynas-veth-nas.network || fail "factory network"
grep -q 'DHCP=ipv4' /etc/systemd/network/05-rynas-veth-nas2.network || fail "factory network 2"
for _ in $(seq 40); do ip -4 -o addr show dev veth-nas2 | grep -q 169.254. && break; sleep 0.5; done
ip -4 -o addr show dev veth-nas2 | grep -q 169.254. || fail "no link-local after the factory reset"
# the admin PC reaches the NAS on its link-local address
LL2=$(ip -4 -o addr show dev veth-nas2 | awk '/169.254/{print $4}' | cut -d/ -f1)
P ip addr add 169.254.200.200/16 dev veth-peer2
P curl -s -o /dev/null -w '%{http_code}\n' "http://$LL2:8080/" | grep -q 200 || fail "web interface over link-local"
echo "web interface answers on $LL2 (link-local)"
/usr/libexec/rynas/rynasctl net status
ls /var/lib/rynas/net/revisions | wc -l | xargs echo "network revisions kept:"

printf '\nALL NETWORK CHECKS PASSED\n'
