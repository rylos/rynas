#!/bin/bash
# Local test instance for the UI audits: fake DS920+ tree + rynasd --dry-system + rynas-web.
# Never touches the host's disks, network, users or power. Usage: start.sh [port] (default 18600).
# Login: tester / correct-horse (users.toml, test-only). PIDs in $R/pids (stop.sh kills them).
# FAKE_UPS=ol|ob|lb|none: also a fake NUT upsd (fake_upsd.py) for rynas-web and rynasd, and NUT
# files in the dry root, so the UPS page shows a UPS, its shutdown policy and battery tests;
# without it the page shows "UPS service not running" and "install NUT".
# FAKE_UPS_CMDS=all|quick|none: battery tests the fake UPS offers (default all).
# FAKE_UPS_BACKEND=nut|synodeb: plain NUT (default; /etc/nut in the dry root) or synodeb
# (/etc/synodeb/ups.conf; a dry root used once with synodeb keeps it: delete $R to switch).
# GENERIC=1: a plain PC instead of the DS920+ (no Synology platform, no synohwd status), to check
# that no Synology screen, tab or hint shows up on generic hardware (G8d).
set -e
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
PORT=${1:-18600}; R=${AUDIT_OUT:-$REPO/target/ui-audit}/rt-$PORT; H=$R/fakehw
mkdir -p "$R/state" "$R/web"; : > "$R/pids"
cd "$REPO"
[ -x target/release/rynasd ] && [ -x target/release/rynas-web ] || cargo build --release -p rynasd -p rynas-web
[ -f crates/rynas-ui/dist/index.html ] || (cd crates/rynas-ui && trunk build --release)
FIXTURE=crates/rynas-web/tests/fixtures/synohwd-status.json
[ -n "${GENERIC:-}" ] && FIXTURE=generic
python3 -I "$HERE/fakehw.py" "$H" "$FIXTURE" > "$R/fakehw.log" 2>&1 &
echo $! >> "$R/pids"; sleep 1
UPSD_ARG=(--upsd 127.0.0.1:9)   # nothing listens there: "UPS service not running"
RYNASD_UPS=()                   # rynasd --dry-system talks to no upsd without --upsd
if [ -n "${FAKE_UPS:-}" ]; then
  UPSPORT=$((PORT + 1000))
  python3 -I "$HERE/fake_upsd.py" "$UPSPORT" "$FAKE_UPS" "${FAKE_UPS_CMDS:-all}" > "$R/upsd.log" 2>&1 &
  echo $! >> "$R/pids"; UPSD_ARG=(--upsd "127.0.0.1:$UPSPORT"); RYNASD_UPS=("${UPSD_ARG[@]}")
  D=$R/state/dry-root/etc
  if [ "${FAKE_UPS_BACKEND:-nut}" = synodeb ]; then
    mkdir -p "$D/synodeb"
    [ -f "$D/synodeb/ups.conf" ] || printf '# synodeb (fake)\nDRYRUN=1\nSAFEMODE_AFTER=300\n' > "$D/synodeb/ups.conf"
  else
    mkdir -p "$D/nut"
    [ -f "$D/nut/nut.conf" ] || printf 'MODE=standalone\n' > "$D/nut/nut.conf"
    [ -f "$D/nut/upsd.users" ] || { printf '[upsmon]\n\tpassword = fake\n\tupsmon primary\n' > "$D/nut/upsd.users"; chmod 640 "$D/nut/upsd.users"; }
  fi
fi
target/release/rynasd --socket "$R/rynasd.sock" --state-dir "$R/state" --dry-system --hw-root "$H" \
  --notify-socket "" --synodeb-notify-conf "" --storage-check-interval 0 "${RYNASD_UPS[@]}" > "$R/rynasd.log" 2>&1 &
echo $! >> "$R/pids"; sleep 1.5
target/release/rynas-web --listen "127.0.0.1:$PORT" --rynasd-socket "$R/rynasd.sock" --users-file "$HERE/users.toml" \
  --auth-state-dir "$R/web" --ui-dir crates/rynas-ui/dist --synohwd-status "$H/run/synohwd/status.json" "${UPSD_ARG[@]}" > "$R/web.log" 2>&1 &
echo $! >> "$R/pids"
for _ in $(seq 50); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && { echo "up on http://127.0.0.1:$PORT"; exit 0; }; sleep 0.1; done
echo down; tail -n 5 "$R/rynasd.log" "$R/web.log"; exit 1
