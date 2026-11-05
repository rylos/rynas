#!/bin/bash
# Local test instance for the UI audits: fake DS920+ tree + rynasd --dry-system + rynas-web.
# Never touches the host's disks, network, users or power. Usage: start.sh [port] (default 18600).
# Login: tester / correct-horse (users.toml, test-only). PIDs in $R/pids (stop.sh kills them).
# FAKE_UPS=ol|ob|lb|none: also a fake NUT upsd (fake_upsd.py) and a synodeb ups.conf in the dry
# root, so the UPS page shows a UPS and its Safe Mode form; without it the page shows "no UPS".
set -e
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
PORT=${1:-18600}; R=${AUDIT_OUT:-$REPO/target/ui-audit}/rt-$PORT; H=$R/fakehw
mkdir -p "$R/state" "$R/web"; : > "$R/pids"
cd "$REPO"
[ -x target/release/rynasd ] && [ -x target/release/rynas-web ] || cargo build --release -p rynasd -p rynas-web
[ -f crates/rynas-ui/dist/index.html ] || (cd crates/rynas-ui && trunk build --release)
python3 -I "$HERE/fakehw.py" "$H" crates/rynas-web/tests/fixtures/synohwd-status.json > "$R/fakehw.log" 2>&1 &
echo $! >> "$R/pids"; sleep 1
UPSD_ARG=(--upsd 127.0.0.1:9)   # nothing listens there: "UPS service not running"
if [ -n "${FAKE_UPS:-}" ]; then
  UPSPORT=$((PORT + 1000))
  python3 -I "$HERE/fake_upsd.py" "$UPSPORT" "$FAKE_UPS" > "$R/upsd.log" 2>&1 &
  echo $! >> "$R/pids"; UPSD_ARG=(--upsd "127.0.0.1:$UPSPORT")
  mkdir -p "$R/state/dry-root/etc/synodeb"
  [ -f "$R/state/dry-root/etc/synodeb/ups.conf" ] || printf '# synodeb (fake)\nDRYRUN=1\nSAFEMODE_AFTER=300\n' > "$R/state/dry-root/etc/synodeb/ups.conf"
fi
target/release/rynasd --socket "$R/rynasd.sock" --state-dir "$R/state" --dry-system --hw-root "$H" \
  --notify-socket "" --synodeb-notify-conf "" --storage-check-interval 0 > "$R/rynasd.log" 2>&1 &
echo $! >> "$R/pids"; sleep 1.5
target/release/rynas-web --listen "127.0.0.1:$PORT" --rynasd-socket "$R/rynasd.sock" --users-file "$HERE/users.toml" \
  --auth-state-dir "$R/web" --ui-dir crates/rynas-ui/dist --synohwd-status "$H/run/synohwd/status.json" "${UPSD_ARG[@]}" > "$R/web.log" 2>&1 &
echo $! >> "$R/pids"
for _ in $(seq 50); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && { echo "up on http://127.0.0.1:$PORT"; exit 0; }; sleep 0.1; done
echo down; tail -5 "$R/rynasd.log" "$R/web.log"; exit 1
