#!/bin/bash
# Stops the instance started by start.sh (by PID, never pkill -f). Usage: stop.sh [port]
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd)
R=${AUDIT_OUT:-$REPO/target/ui-audit}/rt-${1:-18600}
[ -f "$R/pids" ] && kill $(cat "$R/pids") 2>/dev/null; rm -f "$R/pids"; echo stopped
