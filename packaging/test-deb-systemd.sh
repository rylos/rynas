#!/usr/bin/env bash
# Install the rynas .deb in a Debian 13 container booted with systemd and check that the
# maintainer scripts enable and start the units, that the hardened units work, that the
# scheduling gate of the Task Scheduler (ExecCondition=, docs/research/scheduling.md S2)
# behaves with the trixie systemd, that heavy work on one volume queues and a scrub gives
# way to a backup and resumes (S4, on a loop-mounted btrfs volume), that Debian's
# apt-daily-upgrade is gated, and that remove/purge clean up. Then, in a second container,
# an upgrade from the release $UPGRADE_FROM (downloaded from GitHub, checked against its
# SHA256SUMS): the jobs that became scheduler tasks (S3) are created without a failed or
# extra run. Needs a privileged container (systemd as PID 1; container=docker and masked
# gettys keep it off the host's consoles).
#
#   packaging/test-deb-systemd.sh [path/to/rynas_*.deb]
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEB=$(realpath "${1:-$(ls -t "$ROOT"/target/debian/rynas_*_amd64.deb | head -1)}")
# the release the upgrade test starts from (UPGRADE_FROM= skips it)
UPGRADE_FROM=${UPGRADE_FROM-0.8.15}
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
api GET /scheduler/tasks/slow; [ "$(echo "$BODY" | jq -r .task.policy.overlap)" = skip ] || fail "slow: overlap not stored: $BODY"
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
echo "--- retries exhausted: the failure is notified once (none while a retry was armed)"
api GET '/notifications?key=scheduler:task:flaky&limit=50'; expect 200 notifications
[ "$(echo "$BODY" | jq 'length')" = 1 ] || fail "flaky: not one failure notification: $BODY"
[ "$(echo "$BODY" | jq '.[0].count')" = 1 ] || fail "flaky: the failure was notified more than once: $BODY"

echo "--- overlap: skip (stored on tasks created now) and queue"
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
for prop in NoNewPrivileges=yes ProtectSystem=yes ProtectKernelModules=yes; do
    [ "$(systemctl show -p "${prop%%=*}" rynas-task-scrubnone.service)" = "$prop" ] || fail "scrub unit sandbox: no $prop"
done
systemctl show -P CapabilityBoundingSet rynas-task-scrubnone.service | grep -q cap_sys_admin || fail "scrub unit: no CAP_SYS_ADMIN"
! systemctl show -P CapabilityBoundingSet rynas-task-scrubnone.service | grep -q cap_setuid || fail "scrub unit: full capabilities"
panel=0
for _ in $(seq 20); do
    api GET '/tasks?limit=50'
    panel=$(echo "$BODY" | jq '[.[] | select(.type == "btrfs.scrub" and .user == "scheduler" and .status == "error")] | length')
    [ "$panel" -ge 1 ] && break
    sleep 0.5
done
[ "$panel" -ge 1 ] || fail "scrub not in the task panel (ended with its run): $BODY"
echo "scheduling gate: OK"

echo "=== the queue of heavy work and Debian's timers under the gate (S4)"
apt-get install -y -qq --no-install-recommends btrfs-progs mdadm >/dev/null 2>&1
# a btrfs volume on a loop device (a kernel object of the host: the container has no node
# for a new one, so it is made here; it is detached at the end whatever happens)
qclean() {
    mountpoint -q /srv/qvol && umount -l /srv/qvol
    losetup -j /var/tmp/q.img 2>/dev/null | cut -d: -f1 | while read -r l; do losetup -d "$l" || true; done
}
trap qclean EXIT
truncate -s 2G /var/tmp/q.img
mkfs.btrfs -q /var/tmp/q.img
n=$(losetup -f | tr -dc '0-9'); [ -e "/dev/loop$n" ] || mknod "/dev/loop$n" b 7 "$n"
losetup "/dev/loop$n" /var/tmp/q.img
mkdir -p /srv/qvol; mount "/dev/loop$n" /srv/qvol
mkdir -p /srv/qvol/data /srv/qvol/small /srv/qvol/repo
head -c 700M /dev/urandom > /srv/qvol/data/blob; echo hello > /srv/qvol/small/f; sync
# a scrub slow enough to watch (20 MiB/s: about 35 s for this volume)
QUUID=$(findmnt -no UUID /srv/qvol)
ls /sys/fs/btrfs/"$QUUID"/devinfo/*/scrub_speed_max >/dev/null || fail "no scrub_speed_max on this kernel"
for f in /sys/fs/btrfs/"$QUUID"/devinfo/*/scrub_speed_max; do echo 20m > "$f"; done
# as a volume mounted at boot: rynasd's mount namespace (its sandbox) is made at its start,
# and in this container a later mount does not reach it
systemctl restart rynasd.service; sleep 2; relogin
mk '{"id":"qscrub-a","action":{"kind":"scrub","filesystem":"/srv/qvol"},"trigger":{"kind":"manual"}}'
mk '{"id":"qscrub-b","action":{"kind":"scrub","filesystem":"/srv/qvol"},"trigger":{"kind":"manual"}}'
api POST /backup/restic/jobs '{"job":{"name":"qb","sources":[{"kind":"folder","path":"/srv/qvol/small"}],"destination":{"kind":"local","path":"/srv/qvol/repo"}},"schedule":{"kind":"manual"}}'
expect 201 "create backup qb"
apply
grep -qx 'TimeoutStartSec=infinity' $U/rynas-task-qscrub-a.service || fail "qscrub-a: a start timeout"
! grep -q TimeoutStartSec $U/rynas-task-tick.service || fail "tick: a user command changed"

echo "--- two heavy jobs on one volume go one at a time"
T=$(date +%s)
api POST /scheduler/tasks/qscrub-a/run; expect 204 "run qscrub-a"
wait_journal qscrub-a "$T" 'btrfs scrub start -B -d /srv/qvol'
api POST /scheduler/tasks/qscrub-b/run '{}'; expect 204 "run qscrub-b"
wait_journal qscrub-b "$T" 'queued: waiting for task qscrub-a on /srv/qvol'
[ "$(systemctl show -P SubState rynas-task-qscrub-b.service)" = condition ] || fail "qscrub-b: not waiting in its gate"
api GET /scheduler/queue; expect 200 queue
echo "$BODY" | jq -e 'any(.[]; .task == "qscrub-b" and .queued.blocker.task == "qscrub-a")' >/dev/null || fail "queue: $BODY"
api GET /scheduler/tasks/qscrub-b/conflicts; expect 200 conflicts
echo "$BODY" | jq -e 'any(.busy[]; .task == "qscrub-a" and .resource.path == "/srv/qvol")' >/dev/null || fail "conflicts: $BODY"
# stopped while it waits: a skipped start, not a failed unit
systemctl stop rynas-task-qscrub-b.service
! systemctl is-failed -q rynas-task-qscrub-b.service || fail "qscrub-b: failed after a stop in the queue"
[ "$(rows qscrub-b 'true')" = 0 ] || fail "qscrub-b: a start that never ran left a row"

echo "--- the scrub gives way to a backup and resumes from where it stopped"
api POST /scheduler/tasks/backup-qb/run '{}'; expect 204 "run backup-qb"
wait_rows qscrub-a '.yielded and .ok' 1 120
wait_rows backup-qb '.ok and .queued_ms > 0' 1 240
wait_journal qscrub-a "$T" 'btrfs scrub resume -B -d /srv/qvol' 240
wait_rows qscrub-a '.trigger == "resume" and .ok and (.yielded | not)' 1 240
! systemctl is-failed -q rynas-task-qscrub-a.service || fail "qscrub-a: unit failed"
api GET /scheduler/tasks/qscrub-a; [ "$(echo "$BODY" | jq -c .status.yielded)" = null ] || fail "qscrub-a: still marked: $BODY"
btrfs scrub status /srv/qvol | grep -Eq 'finished|Status:[[:space:]]+finished' || { btrfs scrub status /srv/qvol; fail "the scrub did not finish"; }
api GET '/notifications?key=scheduler:task:qscrub-a&limit=10'
[ "$(echo "$BODY" | jq 'length')" = 0 ] || fail "qscrub-a: giving way notified: $BODY"
qclean; trap - EXIT

echo "--- Debian's apt-daily-upgrade under the gate: pause all skips its timer's start"
systemctl cat apt-daily-upgrade.service | grep -qx 'ExecCondition=+/usr/libexec/rynas/rynasctl gate apt-upgrade' \
    || { systemctl cat apt-daily-upgrade.service; fail "apt-daily-upgrade: no gate"; }
mkdir -p /run/systemd/system/apt-daily-upgrade.timer.d
printf '[Timer]\nRandomizedDelaySec=0\n' > /run/systemd/system/apt-daily-upgrade.timer.d/zz-test.conf
api POST /scheduler/pause '{"reason":"maintenance"}'; expect 200 "pause all"
systemctl stop apt-daily-upgrade.timer
touch -d '2 days ago' /var/lib/systemd/timers/stamp-apt-daily-upgrade.timer
systemctl daemon-reload; systemctl start apt-daily-upgrade.timer
wait_rows apt-upgrade '.skipped == "paused_all" and .detail == "maintenance"' 1
[ "$(systemctl show -P Result apt-daily-upgrade.service)" = exec-condition ] || fail "apt-daily-upgrade: not a condition skip"
! systemctl is-failed -q apt-daily-upgrade.service || fail "apt-daily-upgrade: failed"
api POST /scheduler/resume; expect 200 resume
rm -r /run/systemd/system/apt-daily-upgrade.timer.d; systemctl daemon-reload
api GET /scheduler/tasks; expect 200 tasks
echo "$BODY" | jq -e 'any(.[]; .task.id == "apt-upgrade" and .status.last_run.skipped == "paused_all")' >/dev/null \
    || fail "apt-upgrade: its history is not shown: $BODY"
if systemctl cat mdcheck_start.service >/dev/null 2>&1; then
    systemctl cat mdcheck_start.service | grep -q 'rynasctl gate mdcheck-start' || fail "mdcheck_start: no gate"
    echo "$BODY" | jq -e 'any(.[]; .task.id == "mdcheck-start" and .status.system)' >/dev/null || fail "mdcheck-start not listed: $BODY"
    echo "mdadm's RAID check: gated"
else
    echo "mdadm has no mdcheck units here: nothing gated, nothing listed"
    ! echo "$BODY" | jq -e 'any(.[]; .task.id == "mdcheck-start")' >/dev/null || fail "mdcheck-start listed without its timer"
fi
echo "queue and Debian timers: OK"

echo "=== the jobs of rynas are scheduler tasks (S3)"
api GET /scheduler/tasks; expect 200 "tasks"
for id in updates-check docker-update-check docker-auto-update apps-catalog-refresh pcbackup-retention; do
    echo "$BODY" | jq -e --arg id "$id" 'any(.[]; .task.id == $id and .task.owner.kind == "feature")' >/dev/null \
        || fail "no task $id owned by its feature: $BODY"
done
echo "$BODY" | jq -e 'any(.[]; .task.id == "apt-upgrade" and .status.system and .status.unit == "apt-daily-upgrade.service")' >/dev/null \
    || fail "the automatic system updates are not listed: $BODY"
echo "$BODY" | jq -e 'any(.[]; .task.id == "docker-auto-update" and (.task.enabled | not))' >/dev/null \
    || fail "docker-auto-update on without a stack that takes updates"
grep -qx 'ExecCondition=+/usr/libexec/rynas/rynasctl gate updates-check' $U/rynas-task-updates-check.service || fail "updates-check: no gate"
grep -q $'^OnBootSec=600\nOnUnitInactiveSec=86400$' <(grep -E '^On' $U/rynas-task-updates-check.timer | head -2) \
    || { cat $U/rynas-task-updates-check.timer; fail "updates-check: not the interval timer"; }
systemctl is-active -q rynas-task-updates-check.timer || fail "updates-check: timer not active"
echo "--- never run: not due before its delay after rynasd's start (10 min), no row"
systemctl stop rynas-task-updates-check.timer
# (OnBootSec has passed: a timer that never elapsed in this boot elapses at once)
systemctl start rynas-task-updates-check.timer
sleep 5
[ "$(rows updates-check 'true')" = 0 ] || fail "updates-check: a start that was not due left a row"
echo "--- the retention (2 min after rynasd's start) is started by the watch loop when due"
wait_rows pcbackup-retention '.ok and .trigger == "timer"' 1 400
echo "--- run now runs the check (lists refreshed when old, notification)"
api POST /scheduler/tasks/updates-check/run; expect 204 "run updates-check"
wait_rows updates-check '.ok and .trigger == "manual"' 1 240
journalctl -u rynas-task-updates-check.service --no-pager -o cat | grep -q 'package lists' \
    || { journalctl -u rynas-task-updates-check.service --no-pager -o cat; fail "updates-check: the check did not run"; }
echo "--- after that run, a start of its timer is not due: skipped, no row"
n=$(rows updates-check 'true')
systemctl restart rynas-task-updates-check.timer
sleep 5
[ "$(rows updates-check 'true')" = "$n" ] || fail "updates-check: a start that was not due left a row"
[ "$(systemctl show -P Result rynas-task-updates-check.service)" != failed ] || fail "updates-check: unit failed"
echo "rynas jobs as tasks: OK"
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

[ -n "$UPGRADE_FROM" ] || exit 0
echo "=== upgrade from $UPGRADE_FROM"
CACHE=$ROOT/target/release-debs/$UPGRADE_FROM
OLD=$CACHE/rynas_${UPGRADE_FROM}-1_amd64.deb
mkdir -p "$CACHE"
if [ ! -s "$OLD" ]; then
    URL=https://github.com/rylos/rynas/releases/download/v$UPGRADE_FROM
    curl -fsSL -o "$CACHE/SHA256SUMS" "$URL/SHA256SUMS"
    curl -fsSL -o "$OLD" "$URL/rynas_${UPGRADE_FROM}-1_amd64.deb"
fi
(cd "$CACHE" && grep " rynas_${UPGRADE_FROM}-1_amd64.deb\$" SHA256SUMS | sha256sum -c --quiet) || { echo "FAIL: checksum of $OLD" >&2; exit 1; }
docker rm -f "$NAME" >/dev/null
docker run -d --name "$NAME" -e container=docker --privileged --cgroupns=private --tmpfs /run --tmpfs /run/lock \
    --tmpfs /tmp -v "$DEB:/tmp/rynas.deb:ro" -v "$OLD:/tmp/rynas-old.deb:ro" "$IMAGE" >/dev/null
sleep 3
docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash -s <<'UPGRADE'
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }
B=http://127.0.0.1:8080/api/v1
apt-get update -qq
apt-get install -y -qq /tmp/rynas-old.deb >/dev/null 2>&1
useradd -m -G rynas-admin alice; echo 'alice:Al1ce-test-pw' | chpasswd
sleep 2
systemctl is-active -q rynasd.service || fail "old release not running"
# the online catalog was checked an hour ago (the next automatic check is due in about a day);
# the machine booted long ago (a container sees the host's uptime): every OnBootSec= is past
systemctl stop rynasd.service rynasd.socket
mkdir -p /var/lib/rynas/catalog
echo "{\"last_check\": $(( $(date +%s) - 3600 ))}" > /var/lib/rynas/catalog/state.json
systemctl start rynasd.socket rynasd.service
T=$(date +%s)
apt-get install -y -qq /tmp/rynas.deb >/dev/null 2>&1
# the tasks are created at rynasd's start; their timers start at once
for _ in $(seq 120); do
    [ -e /etc/systemd/system/rynas-task-updates-check.timer ] && break
    sleep 0.5
done
[ -e /etc/systemd/system/rynas-task-updates-check.timer ] || fail "the jobs' tasks were not created"
sleep 30
curl -s -c /tmp/j -H 'Content-Type: application/json' -o /tmp/session.json \
    -d '{"username":"alice","password":"Al1ce-test-pw"}' "$B/auth/login"
hist() { curl -s -b /tmp/j "$B/scheduler/tasks/$1/history?limit=100"; }
for id in updates-check docker-update-check apps-catalog-refresh pcbackup-retention; do
    h=$(hist "$id")
    # no run: none of them is due yet (10, 5 and 2 minutes after rynasd's start; the catalog a
    # day after its last check), and nothing failed
    [ "$(echo "$h" | jq '[.[] | select(.skipped == null)] | length')" = 0 ] || fail "$id ran right after the upgrade: $h"
    ! systemctl is-failed -q "rynas-task-$id.service" || fail "$id: unit failed"
    systemctl is-active -q "rynas-task-$id.timer" || fail "$id: timer not active"
done
! journalctl --since "@$T" --no-pager -o cat | grep -E 'not in the (applied )?configuration|answered 404' \
    || fail "a timer started before its task was known"
[ -z "$(systemctl list-units --failed --plain --no-legend 'rynas-task-*')" ] || fail "failed task units: $(systemctl list-units --failed --plain --no-legend)"
echo "--- the retention (2 min after rynasd's start) then runs"
for _ in $(seq 200); do
    n=$(hist pcbackup-retention | jq '[.[] | select(.ok)] | length')
    [ "$n" -ge 1 ] && break
    sleep 1
done
[ "$n" -ge 1 ] || fail "pcbackup-retention did not run when due"
[ "$(hist apps-catalog-refresh | jq 'length')" = 0 ] || fail "the catalog was refreshed although checked an hour ago"
echo "UPGRADE CHECKS PASSED"
UPGRADE
