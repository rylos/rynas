#!/bin/sh
# PC backup schedule end to end (S6 of docs/research/scheduling.md): the real rynas-agent
# service loop against rynasd's agent routes on 127.0.0.1 (crates/rynasd/tests/
# pc_agent_schedule_e2e.rs). An agent whose allowed window is closed does not start and its
# skipped start is in the NAS's scheduler history; with the window open it starts at once and
# its run is there too; Pause all reaches the plan an agent receives; the delay after start
# holds (never backed up: one run a minute after start; not due: none), a calendar time missed
# while the PC was off is caught up once, and "also at every start" runs once per boot.
#
#   packaging/test-pcbackup-schedule.sh
#
# No root, no disk, no network beyond 127.0.0.1: the agent runs `schedule-test` (a stand-in
# reports each run, nothing of this PC is read or sent) with its state in a temporary folder.
# CARGO_TARGET_DIR is left as it is (keep it on disk, not in a tmpfs).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
(cd "$root/crates/rynas-agent" && cargo build --quiet)
agent="${AGENT_TARGET_DIR:-$root/crates/rynas-agent/target}/debug/rynas-agent"
[ -x "$agent" ] || { echo "no agent binary at $agent" >&2; exit 1; }
cd "$root"
RYNAS_AGENT_BIN="$agent" cargo test -p rynasd --test pc_agent_schedule_e2e -- --ignored --nocapture
