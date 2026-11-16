#!/usr/bin/env bash
# Install the rynas .deb in a Debian 13 container booted with systemd and check that the
# maintainer scripts enable and start the units, that the hardened units work, that the
# scheduling gate of the Task Scheduler (ExecCondition=, docs/research/scheduling.md S2)
# behaves with the trixie systemd, and that remove/purge clean up. Needs a privileged
# container (systemd as PID 1; container=docker and masked gettys keep it off the host's
# consoles).
#
#   packaging/test-deb-systemd.sh [path/to/rynas_*.deb]
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEB=${1:-$(ls -t "$ROOT"/target/debian/rynas_*_amd64.deb | head -1)}
IMAGE=rynas-test-systemd:trixie-3
NAME=rynas-test-systemd-$$

docker build -q -t "$IMAGE" - >/dev/null <<'DOCKERFILE'
FROM debian:trixie
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends systemd systemd-sysv dbus curl jq \
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

echo "=== scheduling gate"
B=http://127.0.0.1:8080/api/v1
relogin() {
    curl -s -c /tmp/j -H 'Content-Type: application/json' -o /tmp/session.json \
        -d '{"username":"alice","password":"Al1ce-test-pw"}' "$B/auth/login"
    CSRF=$(jq -r .csrf_token /tmp/session.json)
    [ -n "$CSRF" ] && [ "$CSRF" != null ] || fail "login: $(cat /tmp/session.json)"
}
relogin
api() {
    local out
    out=$(curl -s -m 60 -b /tmp/j -H "X-CSRF-Token: $CSRF" -H 'Content-Type: application/json' \
        -X "$1" ${3:+-d "$3"} -w '\n%{http_code}' "$B$2")
    CODE=${out##*$'\n'}
    BODY=${out%$'\n'*}
}
expect() { [ "$CODE" = "$1" ] || fail "$2: HTTP $CODE $BODY"; }
apply() {
    local id st=
    api POST /config/apply '{"comment":"scheduling gate test"}'
    expect 202 apply
    id=$(echo "$BODY" | jq -r .task_id)
    for _ in $(seq 300); do
        api GET "/tasks/$id"
        st=$(echo "$BODY" | jq -r .status)
        case $st in ok|error|cancelled) break ;; esac
        sleep 0.2
    done
    [ "$st" = ok ] || fail "apply: $st"
}
# rows of task $1's history matching the jq condition $2
rows() { api GET "/scheduler/tasks/$1/history?limit=100"; echo "$BODY" | jq "[.[] | select($2)] | length"; }
# wait (up to $4 x 0.5 s, default 60 s) for at least $3 such rows
wait_rows() {
    local n=0
    for _ in $(seq "${4:-120}"); do
        n=$(rows "$1" "$2")
        [ "$n" -ge "$3" ] && return 0
        sleep 0.5
    done
    api GET "/scheduler/tasks/$1/history?limit=100"
    fail "$1: wanted $3 rows with $2, found $n: $BODY"
}
mk() { api POST /scheduler/tasks "$1"; expect 201 "create $(echo "$1" | jq -r .id)"; }
journal_since() { journalctl -u "rynas-task-$1.service" --since "@$2" --no-pager -o cat 2>/dev/null; }
wait_journal() {
    for _ in $(seq "${4:-60}"); do journal_since "$1" "$2" | grep -q "$3" && return 0; sleep 0.5; done
    journal_since "$1" "$2"; fail "$1: no journal line '$3'"
}
cmd() { jq -nc --arg id "$1" --arg c "$2" --arg cal "$3" --argjson p "${4:-{\}}" \
    '{id: $id, action: {kind: "command", command: $c}, trigger: {kind: "calendar", on_calendar: $cal}, policy: $p}'; }
PAST=$(date -d '-3 hours' '+*-*-* %H:%M:00')
SOON=$(date -d '+40 seconds' '+%Y-%m-%d %H:%M:%S')
mk "$(cmd tick 'echo tick' '*:*:0/10')"
mk "$(cmd slow 'sleep 13' '*:*:0/10')"
mk "$(cmd slowq 'sleep 13' '*:*:0/10' '{"overlap":"queue"}')"
mk "$(cmd miss 'echo nightly' "$PAST" '{"missed":{"kind":"run_once","within_h":1}}')"
mk "$(cmd catchup 'echo nightly' "$PAST" '{"missed":{"kind":"run_once","within_h":6}}')"
mk "$(cmd flaky 'echo flaky; exit 3' "$SOON" '{"retry":{"attempts":2,"delay_s":5,"backoff":false}}')"
mk '{"id":"scrubnone","action":{"kind":"scrub","filesystem":"/srv/none"},"trigger":{"kind":"manual"}}'
apply
U=/etc/systemd/system
grep -qx 'ExecCondition=+/usr/libexec/rynas/rynasctl gate tick' $U/rynas-task-tick.service || fail "no gate in the service"
! grep -q ExecCondition $U/rynas-task-tick.timer || fail "the timer changed"

echo "--- a timer start goes through the gate"
wait_rows tick '.ok and .trigger == "timer"' 1

echo "--- retry: a failed scheduled run is tried again twice, 5 s apart"
wait_rows flaky '.trigger == "timer" and (.ok | not)' 1 160
wait_rows flaky '.trigger == "retry" and .attempt == 2' 1 80
sleep 7
[ "$(rows flaky '.trigger == "retry"')" = 2 ] || fail "flaky: not exactly two retries"
api GET /scheduler/tasks/flaky; [ "$(echo "$BODY" | jq -c .status.retry)" = null ] || fail "flaky: retry still armed: $BODY"

echo "--- overlap: skip (default) and queue"
wait_rows slow '.skipped == "overlap"' 1
wait_rows slowq '.trigger == "queued" and .ok' 1
for t in slow slowq; do api POST "/scheduler/tasks/$t/pause" '{"reason":"quiet"}'; expect 200 "pause $t"; done
wait_rows slow '.skipped == "paused" and .detail == "quiet"' 1

echo "--- missed run after downtime: past the deadline skipped, within it caught up"
for t in miss catchup; do
    systemctl stop "rynas-task-$t.timer"
    touch -d '2 days ago' "/var/lib/systemd/timers/stamp-rynas-task-$t.timer"
done
systemctl daemon-reload
systemctl start rynas-task-miss.timer rynas-task-catchup.timer
wait_rows miss '.skipped == "missed" and .trigger == "catch_up"' 1
wait_rows catchup '.ok and .trigger == "catch_up"' 1
[ "$(systemctl show -P Result rynas-task-miss.service)" = exec-condition ] || fail "miss: not a condition skip"
! systemctl is-failed -q rynas-task-miss.service || fail "miss: unit failed"

echo "--- the stamp of a timer survives a changed service and a daemon-reload"
st=/var/lib/systemd/timers/stamp-rynas-task-catchup.timer
before="$(stat -c %Y $st) $(systemctl show -P LastTriggerUSec rynas-task-catchup.timer)"
cp $U/rynas-task-catchup.service /tmp/svc.bak
echo "# changed service" >> $U/rynas-task-catchup.service
systemctl daemon-reload
after="$(stat -c %Y $st) $(systemctl show -P LastTriggerUSec rynas-task-catchup.timer)"
[ "$before" = "$after" ] || fail "stamp changed: $before -> $after"
systemctl is-active -q rynas-task-catchup.timer || fail "timer stopped"
cp /tmp/svc.bak $U/rynas-task-catchup.service; systemctl daemon-reload

echo "--- pause all: timer starts skipped (not failed), run now still runs"
api POST /scheduler/pause '{"reason":"disk swap"}'; expect 200 "pause all"
wait_rows tick '.skipped == "paused_all" and .detail == "disk swap"' 1
[ "$(systemctl show -P Result rynas-task-tick.service)" = exec-condition ] || fail "tick: not a condition skip"
! systemctl is-failed -q rynas-task-tick.service || fail "tick: unit failed"
api POST /scheduler/tasks/tick/run; expect 204 "run now"
wait_rows tick '.ok and .trigger == "manual"' 1
n=$(rows tick '.ok and .trigger == "timer"')
api POST /scheduler/resume; expect 200 resume
wait_rows tick '.ok and .trigger == "timer"' $((n + 1))

echo "--- rynasd down: the start runs as before the gate; a pause in the file still holds"
T=$(date +%s)
systemctl stop rynasd.socket rynasd.service
wait_journal tick "$T" "running the start as before the gate"
wait_journal tick "$T" "tick"
systemctl start rynasd.socket; relogin
wait_rows tick '.ungated and .ok' 1
api POST /scheduler/tasks/tick/pause '{}'; expect 200 "pause tick"
T=$(date +%s)
systemctl stop rynasd.socket rynasd.service
wait_journal tick "$T" "the task is paused: start skipped"
systemctl start rynasd.socket; relogin
wait_rows tick '.skipped == "paused" and (.detail // "" | test("not reachable"))' 1
api POST /scheduler/tasks/tick/resume; expect 200 "resume tick"

echo "--- an internal action that is one command runs in its own unit"
T=$(date +%s)
api POST /scheduler/tasks/scrubnone/run; expect 204 "run scrub"
wait_rows scrubnone '.trigger == "manual" and (.ok | not)' 1
journal_since scrubnone "$T" | grep -q '^\$ btrfs scrub start -B -d /srv/none' || { journal_since scrubnone "$T"; fail "scrub not run by the unit"; }
echo "scheduling gate: OK"
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
# (scheduler task units fail on purpose above)
journalctl -b --no-pager | grep -E 'rynas.*(refusing|Failed)' | grep -v 'rynas-task-' && fail "unit errors in journal" || true
echo "ALL CHECKS PASSED"
TEST
