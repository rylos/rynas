#!/usr/bin/env bash
# Synology hardware provider and power schedule for real, in a throw-away Debian 13 container
# booted with systemd, on a FAKE hardware tree: never the host's RTC, LEDs or power state.
#
#   packaging/test-synology.sh [--no-build]
#
# Builds debug rynasd/rynasctl/rynas-web against trixie's glibc (builder image of
# build-deb.sh), then inside the container: a fake DS920+ below /srv/fakehw (DMI, platform
# driver directory, LED class devices, RTC with wakealarm, /proc files), a fake synohwd unit
# that reads the command FIFO like synohwd and logs what it gets, an rtcwake shim that only
# writes the fake RTC, an smbstatus shim, and a fake poweroff. rynasd runs with
# --hw-root /srv/fakehw and --poweroff-command. Through the API: settings staged and applied
# (/etc/synohwd.toml, synohwd restarted), beeps through the FIFO, identify on the fake LEDs,
# fan curves and status; the power schedule (shutdown rule = scheduler task), the
# rynas-power-wake.service arming the RTC at start and at stop, the next times, and the
# scheduled shutdown skipped while SMB files are open, then done (fake poweroff).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-systemd:trixie-2
NAME=rynas-test-synology-$$
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
    # privileged (systemd as PID 1): the host's /dev/rtc0 is visible, so nothing in the test
    # may reach it: rynasd and rynasctl only see /srv/fakehw, the real rtcwake is absent
    # (checked) and the rtcwake shim writes the fake RTC only
    docker run -d --name "$NAME" -e container=docker --privileged --cgroupns=private --tmpfs /run --tmpfs /run/lock \
        --tmpfs /tmp -v "$BIN/rynasd:/opt/rynas/rynasd:ro" -v "$BIN/rynas-web:/opt/rynas/rynas-web:ro" \
        -v "$BIN/rynasctl:/opt/rynas/rynasctl:ro" \
        -v "$ROOT/packaging:/opt/rynas/packaging:ro" \
        -v "$ROOT/crates/rynas-hw-synology/tests/fixtures:/opt/rynas/fixtures:ro" "$IMAGE" >/dev/null
    trap 'docker rm -f "$NAME" >/dev/null' EXIT
    sleep 3
    docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash /opt/rynas/packaging/test-synology.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
step() { printf '\n=== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; journalctl -u rynasd -u synohwd -u rynas-power-wake --no-pager | tail -60 >&2; exit 1; }

step "packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends jq passwd adduser iproute2 >/dev/null 2>&1
# the real rtcwake (util-linux) must not run: diverted away, only the shim may answer
dpkg -S /usr/sbin/rtcwake 2>/dev/null || true
for p in /usr/sbin/rtcwake /usr/bin/rtcwake; do
    [ -e "$p" ] && dpkg-divert --quiet --local --rename --divert "$p.real-not-in-tests" --add "$p"
    [ -e "$p.real-not-in-tests" ] && chmod 000 "$p.real-not-in-tests"
done
for p in /usr/sbin/rtcwake /sbin/rtcwake /usr/bin/rtcwake; do
    [ ! -e "$p" ] || fail "a real rtcwake is installed ($p)"
done

step "a fake DS920+ below /srv/fakehw"
H=/srv/fakehw
mkdir -p $H/sys/devices/platform/synology-nas $H/sys/class/dmi/id $H/run/synohwd \
    $H/proc/sys/kernel/random $H/sys/class/rtc/rtc0 $H/sys/devices/pnp0/00:02
echo "Synology Inc." > $H/sys/class/dmi/id/sys_vendor
echo "DS920+" > $H/sys/class/dmi/id/product_name
ln -s ../../../devices/pnp0/00:02 $H/sys/class/rtc/rtc0/device
: > $H/sys/class/rtc/rtc0/wakealarm
cat /proc/sys/kernel/random/boot_id > $H/proc/sys/kernel/random/boot_id
echo "120.00 60.00" > $H/proc/uptime
printf 'Personalities : [raid1]\nunused devices: <none>\n' > $H/proc/mdstat
for bay in 1 2 3 4; do
    for color in green orange; do
        d=$H/sys/class/leds/syno:$color:disk$bay
        mkdir -p "$d"
        if [ $color = green ]; then echo "none [ata$bay] timer" > "$d/trigger"; echo 255 > "$d/brightness"
        else echo "[none] timer" > "$d/trigger"; echo 0 > "$d/brightness"; fi
    done
done

step "fake synohwd: FIFO reader, status file"
cat > /usr/local/sbin/fake-synohwd <<'EOF'
#!/bin/bash
# reads the command FIFO like synohwd's cmd_loop (O_RDWR: never EOF) and logs the commands
H=/srv/fakehw
FIFO=$H/run/synohwd/cmd
[ -p "$FIFO" ] || mkfifo -m 600 "$FIFO"
now=$(date +%s)
jq --argjson e "$now" --arg t "$(date '+%F %T')" '.epoch = $e | .time = $t' \
    /opt/rynas/fixtures/synohwd-status.json > $H/run/synohwd/status.json
echo "start $(cat /etc/synohwd.toml 2>/dev/null | grep -c . || echo 0) lines" >> /var/log/fake-synohwd.log
exec 3<>"$FIFO"
while read -r line <&3; do echo "cmd $line" >> /var/log/fake-synohwd.log; done
EOF
chmod 755 /usr/local/sbin/fake-synohwd
cat > /etc/systemd/system/synohwd.service <<'EOF'
[Unit]
Description=fake synohwd (test)
[Service]
ExecStart=/usr/local/sbin/fake-synohwd
ExecStopPost=/bin/rm -f /srv/fakehw/run/synohwd/status.json
[Install]
WantedBy=multi-user.target
EOF

step "shims: rtcwake (fake RTC only), smbstatus, poweroff"
cat > /usr/local/sbin/rtcwake <<'EOF'
#!/bin/bash
echo "rtcwake $*" >> /var/log/rtcwake.log
dev=rtc0; t=; mode=
while [ $# -gt 0 ]; do case $1 in -d) dev=$2; shift ;; -t) t=$2; shift ;; -m) mode=$2; shift ;; esac; shift; done
f=/srv/fakehw/sys/class/rtc/$dev/wakealarm
case $mode in disable) : > "$f" ;; no) echo "$t" > "$f" ;; *) exit 1 ;; esac
EOF
cat > /usr/local/bin/smbstatus <<'EOF'
#!/bin/sh
if [ -e /run/test-smb-busy ]; then
  echo '{"timestamp":"x","version":"4.22","open_files":{"/srv/docs/a.txt":{"filename":"a.txt"}}}'
else
  echo '{"timestamp":"x","version":"4.22","open_files":{}}'
fi
EOF
cat > /usr/local/sbin/fake-poweroff <<'EOF'
#!/bin/sh
date +%s > /var/log/fake-poweroff
EOF
chmod 755 /usr/local/sbin/rtcwake /usr/local/bin/smbstatus /usr/local/sbin/fake-poweroff

step "an /etc/synohwd.toml with the block an older network.rescue wrote"
cat > /etc/synohwd.toml <<'EOF'
# synohwd configuration (synodeb)
fan_mode = "low"
wol = true
# BEGIN rynas network — managed by rynas, do not edit (sha256:0000)
reset_button = "command:/usr/libexec/rynas/rynasctl net reset-button"
# END rynas network
EOF

step "rynas installed as the package does it, rynasd on the fake tree"
install -d /usr/libexec/rynas /usr/share/rynas/ui
install -m 755 /opt/rynas/rynasd /opt/rynas/rynas-web /opt/rynas/rynasctl /usr/libexec/rynas/
echo '<!doctype html><title>rynas</title>' > /usr/share/rynas/ui/index.html
addgroup --system --quiet rynas
adduser --system --quiet --ingroup rynas --home /var/lib/rynas --no-create-home --shell /usr/sbin/nologin rynas
addgroup --system --quiet rynas-admin
install -d -m 0750 -o root -g rynas /var/lib/rynas
install -m 644 /opt/rynas/packaging/pam.d/rynas /etc/pam.d/rynas
install -m 644 /opt/rynas/packaging/systemd/rynasd.service /opt/rynas/packaging/systemd/rynasd.socket \
    /opt/rynas/packaging/systemd/rynas-web.service /etc/systemd/system/
mkdir -p /etc/systemd/system/rynasd.service.d
cat > /etc/systemd/system/rynasd.service.d/fakehw.conf <<'EOF'
[Service]
ExecStart=
ExecStart=/usr/libexec/rynas/rynasd --socket /run/rynas/rynasd.sock --state-dir /var/lib/rynas --hw-root /srv/fakehw --poweroff-command /usr/local/sbin/fake-poweroff
EOF
echo 'RYNAS_WEB_LISTEN=127.0.0.1:8080' > /etc/default/rynas-web
systemctl daemon-reload
systemctl start synohwd rynasd.socket rynas-web
useradd -m -s /bin/bash -G rynas-admin admin1
echo 'admin1:Adm1n-test-pw' | chpasswd
for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:8080/ && break; sleep 0.1; done
systemctl is-active synohwd rynasd.socket rynas-web

B=http://127.0.0.1:8080/api/v1
curl -s -c /tmp/jar -H 'Content-Type: application/json' -o /tmp/session.json \
    -d '{"username":"admin1","password":"Adm1n-test-pw"}' "$B/auth/login"
CSRF=$(jq -r .csrf_token /tmp/session.json)
[ -n "$CSRF" ] && [ "$CSRF" != null ] || fail "login: $(cat /tmp/session.json)"

api() {
    local out
    out=$(curl -s -b /tmp/jar -H "X-CSRF-Token: $CSRF" -H 'Content-Type: application/json' \
        -X "$1" ${3:+-d "$3"} -w '\n%{http_code}' "$B$2")
    CODE=${out##*$'\n'}
    BODY=${out%$'\n'*}
}
expect() { [ "$CODE" = "$1" ] || fail "$2: HTTP $CODE $BODY"; }
wait_task() {
    local id=$1 st
    for _ in $(seq 600); do
        api GET "/tasks/$id"
        st=$(echo "$BODY" | jq -r .status)
        case $st in ok|error|cancelled) break ;; esac
        sleep 0.2
    done
    TASK_STATUS=$st
    curl -s -b /tmp/jar "$B/tasks/$id/log" | sed 's/^/    | /'
}
apply() {
    api POST /config/apply '{"comment":"test-synology"}'
    expect 202 "apply"
    wait_task "$(echo "$BODY" | jq -r .task_id)"
    [ "$TASK_STATUS" = ok ] || fail "apply: $TASK_STATUS"
}
fifo_has() {
    for _ in $(seq 50); do grep -qx "cmd $1" /var/log/fake-synohwd.log && return 0; sleep 0.1; done
    fail "synohwd FIFO did not get '$1': $(cat /var/log/fake-synohwd.log)"
}

step "synohwd.toml adopted at start: synology.hw is its only writer, the legacy block gone"
for _ in $(seq 100); do head -1 /etc/synohwd.toml | grep -q '^# Managed by rynas' && break; sleep 0.1; done
cat /etc/synohwd.toml
head -1 /etc/synohwd.toml | grep -q '^# Managed by rynas' || fail "synohwd.toml not adopted"
! grep -q 'rynas network' /etc/synohwd.toml || fail "legacy block still there"
[ "$(grep -c '^reset_button' /etc/synohwd.toml)" = 1 ] || fail "reset_button lines"
grep -qx 'reset_button = "command:/usr/libexec/rynas/rynasctl net reset-button"' /etc/synohwd.toml || fail "reset handler lost"
grep -qx 'wol = true' /etc/synohwd.toml || fail "wol lost"
api GET /modules/synology/settings; expect 200 "settings"
[ "$(echo "$BODY" | jq -r .applied.reset_button)" = network_rescue ] || fail "applied reset_button: $BODY"
api GET /network/rescue; expect 200 "rescue info"
echo "$BODY" | jq -c '{synology, reset_button_configured, reset_button_pending, settings}'
[ "$(echo "$BODY" | jq -r '.synology and .reset_button_configured')" = true ] || fail "rescue info: $BODY"
api PUT /network/rescue '{"reset_button": false, "mdns_service": true}'; expect 400 "reset_button moved"
api PUT /network/rescue '{"mdns_service": false}'; expect 200 "rescue put"
apply
[ "$(grep -c . /etc/synohwd.toml)" -gt 10 ] && ! grep -q 'rynas network' /etc/synohwd.toml || fail "network.rescue touched synohwd.toml"
api GET /config/drift; [ "$(echo "$BODY" | jq '.entries | length')" = 0 ] || fail "drift after network.rescue: $BODY"

step "status and the /api/v1/hardware summary"
api GET /modules/synology/status; expect 200 "status"
echo "$BODY" | jq -c '{model, running, fan_duty_pct, bays: [.bays[] | {number, device, asleep}], events: (.events | length)}'
[ "$(echo "$BODY" | jq -r .model)" = DS920+ ] || fail "model"

step "settings: imported defaults, validation, staged, applied"
api GET /modules/synology/settings; expect 200 "settings"
echo "$BODY" | jq -c '{applied, platform}'
api PUT /modules/synology/settings '{"fan_period_s": 4}'; expect 400 "fan_period 4"
api PUT /modules/synology/settings '{"power_button": "command:/bin/sh"}'; expect 400 "command reserved"
api PUT /modules/synology/settings '{"fan_mode":"high","fan_period_s":30,"reset_button":"network_rescue","wol":true,"led_brightness":128}'
expect 200 "settings put"
[ "$(echo "$BODY" | jq -r .pending.fan_mode)" = high ] || fail "pending"
apply
head -1 /etc/synohwd.toml | grep -q '^# Managed by rynas' || fail "managed header"
sed 1d /etc/synohwd.toml
grep -qx 'fan_mode = "high"' /etc/synohwd.toml || fail "fan_mode"
grep -qx 'reset_button = "command:/usr/libexec/rynas/rynasctl net reset-button"' /etc/synohwd.toml || fail "reset handler"
[ "$(grep -c '^start' /var/log/fake-synohwd.log)" -ge 2 ] || fail "synohwd not restarted"
api GET /config/drift; [ "$(echo "$BODY" | jq '.entries | length')" = 0 ] || fail "drift after apply: $BODY"

step "actions: beeps through the FIFO, identify on the LEDs, fan curves, mute"
api POST /modules/synology/beep '{"kind":"short"}'; expect 204 "short beep"; fifo_has 2
api POST /modules/synology/beep '{"kind":"long"}'; expect 204 "long beep"; fifo_has 3
api POST /modules/synology/beep '{"kind":"1"}'; expect 400 "bad beep"
api POST /hardware/bays/2/identify '{"duration_s":2}'; expect 204 "identify"
[ "$(cat $H/sys/class/leds/syno:orange:disk2/trigger)" = timer ] || fail "orange not blinking"
[ "$(cat $H/sys/class/leds/syno:green:disk2/brightness)" = 0 ] || fail "green not off"
sleep 3
[ "$(cat $H/sys/class/leds/syno:orange:disk2/trigger)" = none ] || fail "orange not restored"
[ "$(cat $H/sys/class/leds/syno:green:disk2/trigger)" = ata2 ] || fail "green trigger not restored"
api POST /hardware/bays/7/identify; expect 404 "bay 7"
api GET /modules/synology/fan-curves; expect 200 "fan curves"
echo "$BODY" | jq -c '{model, active_mode, cpu_shutdown: .modes[0].curves[2].shutdown_c, readings: (.readings | length)}'
[ "$(echo "$BODY" | jq -r .active_mode)" = high ] || fail "active mode"
api POST /modules/synology/mute; expect 200 "mute"; echo "$BODY"
grep -c '^cmd' /var/log/fake-synohwd.log

step "power schedule: rules, conflicts, scheduler task, RTC armed"
api PUT /power/schedule '{"rules":[{"id":"up","action":"startup","days":["mon"],"hour":7,"minute":0},{"id":"down","action":"shutdown","days":["mon"],"hour":7,"minute":3}]}'
expect 400 "conflict"
api PUT /power/schedule '{"rules":[
  {"id":"morning","action":"startup","days":["mon","tue","wed","thu","fri","sat","sun"],"hour":7,"minute":0},
  {"id":"night","action":"shutdown","days":["mon","tue","wed","thu","fri","sat","sun"],"hour":23,"minute":30,
   "shutdown":{"when_busy":"skip","max_wait_min":1}}]}'
expect 200 "schedule"
api GET /config/pending
KEYS=$(echo "$BODY" | jq -r '[.changes[].key] | sort | join(",")')
[ "$KEYS" = "power.schedule,scheduler" ] || fail "pending keys $KEYS"
apply
cat /etc/systemd/system/rynas-power-wake.service
grep -q 'power scheduled-shutdown night' /etc/systemd/system/rynas-task-power-off-night.service || fail "task unit"
systemctl is-active rynas-power-wake.service || fail "wake unit not active"
systemctl is-active rynas-task-power-off-night.timer || fail "shutdown timer not active"
cat /var/log/rtcwake.log
grep -q -- '-d rtc0 -m no -t ' /var/log/rtcwake.log || fail "rtcwake not run"
ALARM=$(cat $H/sys/class/rtc/rtc0/wakealarm)
[ -n "$ALARM" ] && [ "$ALARM" -gt "$(date +%s)" ] || fail "fake RTC alarm $ALARM"
[ "$(date -d "@$ALARM" +%H:%M)" = 07:00 ] || fail "alarm not at 07:00: $(date -d "@$ALARM")"
api GET /power/next; expect 200 "next"
echo "$BODY" | jq -c .
[ "$(echo "$BODY" | jq -r .next_startup_rule)" = morning ] || fail "next startup"
[ "$(echo "$BODY" | jq -r .wake.supported)" = true ] || fail "wake supported"
[ "$(echo "$BODY" | jq '.wake.armed_ms / 1000')" = "$ALARM" ] || fail "armed_ms"
[ "$(echo "$BODY" | jq -r .last_boot.cause)" = unknown ] || fail "boot cause"

step "every stop arms (ExecStop), a schedule without startups clears"
: > /var/log/rtcwake.log
systemctl stop rynas-power-wake.service
grep -q -- '-m no' /var/log/rtcwake.log || fail "ExecStop did not arm"
systemctl start rynas-power-wake.service
api DELETE /scheduler/tasks/power-off-night; expect 409 "task belongs to the schedule"

step "scheduled shutdown: SMB files open → skipped; idle → fake poweroff"
touch /run/test-smb-busy
systemctl start rynas-task-power-off-night.service || true
for _ in $(seq 100); do systemctl is-active -q rynas-task-power-off-night.service || break; sleep 0.2; done
journalctl -u rynas-task-power-off-night.service --no-pager | tail -8
journalctl -u rynas-task-power-off-night.service --no-pager | grep -q 'files open over SMB' || fail "smb check"
journalctl -u rynas-task-power-off-night.service --no-pager | grep -q 'skipped' || fail "not skipped"
[ ! -e /var/log/fake-poweroff ] || fail "powered off while busy"
rm /run/test-smb-busy
systemctl start rynas-task-power-off-night.service || true
for _ in $(seq 100); do [ -e /var/log/fake-poweroff ] && break; sleep 0.2; done
[ -e /var/log/fake-poweroff ] || fail "no poweroff when idle"
api GET /scheduler/tasks/power-off-night/history; echo "$BODY" | jq -c '.[] | {result, ok}'

step "clear the schedule"
api PUT /power/schedule '{"rules":[]}'; expect 200 "empty schedule"
apply
[ -z "$(cat $H/sys/class/rtc/rtc0/wakealarm)" ] || fail "alarm not cleared"
[ ! -e /etc/systemd/system/rynas-task-power-off-night.service ] || fail "task unit left"
api GET /config/drift; [ "$(echo "$BODY" | jq '.entries | length')" = 0 ] || fail "drift: $BODY"

echo
echo "ALL OK"
