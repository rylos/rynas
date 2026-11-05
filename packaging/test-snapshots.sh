#!/usr/bin/env bash
# Task Scheduler and snapshots for real, in a throw-away Debian 13 container booted with
# systemd (privileged: loop-mounted btrfs, systemd as PID 1, btrbk from Debian).
#
#   packaging/test-snapshots.sh [--no-build]
#
# Builds debug rynasd/rynasctl/rynas-web against trixie's glibc (builder image of
# build-deb.sh), then inside the container: a share on a btrfs subvolume, a snapshot job
# through the API (job + scheduler task + SMB previous versions in one changeset), apply,
# timer present and active, run now (btrbk through `rynasctl job run`), history recorded by
# `rynasctl task-finished`, listing, browse, restore (copy and replace), retention preview,
# chain-safe delete, rollback, a user task as a local user with its output, calendar and
# cron previews, read-only system timers and crontabs, system restore points from the apt
# hook (deduplicated), then removal of everything.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-systemd:trixie-2
NAME=rynas-test-snapshots-$$
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
    docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash /opt/rynas/packaging/test-snapshots.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
step() { printf '\n=== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; journalctl -u rynasd --no-pager | tail -40 >&2; exit 1; }

step "packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends btrbk btrfs-progs samba acl jq passwd cron >/dev/null 2>&1
dpkg-query -W -f '${Package} ${Version}\n' btrbk btrfs-progs samba systemd

step "a btrfs volume with a subvolume per share"
truncate -s 512M /var/tmp/vol1.img
mkfs.btrfs -q /var/tmp/vol1.img
mkdir -p /srv/vol1
mount -o loop /var/tmp/vol1.img /srv/vol1
btrfs -q subvolume create /srv/vol1/docs
btrfs -q subvolume create /srv/vol1/sys
mkdir -p /srv/vol1/docs/reports
echo "version 1" > /srv/vol1/docs/reports/a.txt
echo "keep me" > /srv/vol1/docs/b.txt
echo "system file" > /srv/vol1/sys/etc.conf

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
install -m 644 /opt/rynas/packaging/systemd/rynasd.service /opt/rynas/packaging/systemd/rynasd.socket \
    /opt/rynas/packaging/systemd/rynas-web.service /etc/systemd/system/
echo 'RYNAS_WEB_LISTEN=127.0.0.1:8080' > /etc/default/rynas-web
systemctl daemon-reload
systemctl start rynasd.socket rynas-web
useradd -m -s /bin/bash -G rynas-admin admin1
echo 'admin1:Adm1n-test-pw' | chpasswd
useradd -m -u 1001 alice
for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:8080/ && break; sleep 0.1; done
systemctl is-active rynasd.socket rynas-web

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
    api POST /config/apply '{"comment":"test-snapshots"}'
    expect 202 "apply"
    wait_task "$(echo "$BODY" | jq -r .task_id)"
    [ "$TASK_STATUS" = ok ] || fail "apply: $TASK_STATUS"
}
started() { expect 202 "$1"; wait_task "$(echo "$BODY" | jq -r .task_id)"; }
# runs: wait for run number $2 of scheduler task $1 (recorded by rynasctl task-finished)
wait_run() {
    for _ in $(seq 300); do
        api GET "/scheduler/tasks/$1/history"
        [ "$(echo "$BODY" | jq length)" -ge "$2" ] && break
        sleep 0.2
    done
    [ "$(echo "$BODY" | jq length)" -ge "$2" ] || fail "no run $2 of $1 recorded"
    # the output is copied from the journal right after the run is recorded
    for _ in $(seq 50); do
        [ "$(echo "$BODY" | jq '.[0].output_bytes')" -gt 0 ] && break
        sleep 0.2
        api GET "/scheduler/tasks/$1/history"
    done
    RUN=$(echo "$BODY" | jq -c '.[0]')
    echo "run of $1: $(echo "$RUN" | jq -c '{id, result, exit_code, ok, output_bytes}')"
}

step "a share with SMB on the subvolume"
api POST /shares '{"name":"docs","path":"/srv/vol1/docs","smb":{"enabled":true}}'; expect 201 "share docs"
apply

step "snapshot job (job + scheduler task + previous versions staged together)"
api POST /snapshots/jobs '{"name":"docs","share":"docs","retention":{"preset":"standard"},"schedule":{"kind":"calendar","on_calendar":"*-*-* *:00/30:00"}}'
expect 201 "snapshot job"
echo "$BODY" | jq -c '{job: .job.snapshot_dir, task: .task_id, schedule, pending}'
api GET /config/pending
KEYS=$(echo "$BODY" | jq -r '[.changes[].key] | sort | join(",")')
echo "pending keys: $KEYS"
[ "$KEYS" = "scheduler,shares,snapshots" ] || fail "pending keys $KEYS"
api POST /snapshots/jobs '{"name":"docs","share":"docs"}'; expect 409 "job twice"
api POST /snapshots/jobs '{"name":"bad","subvolume":"/srv/vol1/docs/../x"}'; expect 400 "bad subvolume"
apply
head -1 /etc/rynas/btrbk/docs.conf | grep -q '^# Managed by rynas' || fail "managed header"
sed 1d /etc/rynas/btrbk/docs.conf
cat /etc/systemd/system/rynas-task-snapshot-docs.service
cat /etc/systemd/system/timers.target.d/rynas-tasks.conf
systemctl is-active rynas-task-snapshot-docs.timer || fail "timer not active"
systemctl list-timers --all --no-pager | grep rynas-task-snapshot-docs || fail "timer not listed"
testparm -s 2>/dev/null | sed -n '/^\[docs\]/,/^$/p' | grep shadow
testparm -s 2>/dev/null | grep -q 'shadow:snapdir = /srv/vol1/.snapshots/docs' || fail "shadow_copy2 not wired"
api GET /snapshots/jobs; echo "$BODY" | jq -c '.[] | {name: .job.name, enabled, next_run_ms, snapshots}'
[ "$(echo "$BODY" | jq '.[0].next_run_ms != null')" = true ] || fail "no next run"

step "run now: btrbk through rynasctl job run, history from rynasctl task-finished"
api POST /scheduler/tasks/snapshot-docs/run; expect 204 "run now"
wait_run snapshot-docs 1
[ "$(echo "$RUN" | jq -r .ok)" = true ] || fail "snapshot run failed"
api GET "/scheduler/tasks/snapshot-docs/history/$(echo "$RUN" | jq -r .id)"
echo "$BODY" | jq -r .output | sed 's/^/    > /'
echo "$BODY" | jq -r .output | grep -q 'created docs\.' || fail "run output not recorded"
api GET '/tasks?limit=5'; echo "$BODY" | jq -c '.[] | select(.type=="snapshot.run") | {type, user, status}' | head -1
ls /srv/vol1/.snapshots/docs
api GET '/snapshots?share=docs'; echo "$BODY" | jq -c '.[] | {id, readonly, deletion}'
SNAP1=$(echo "$BODY" | jq -r '.[0].id')
[ "$(echo "$BODY" | jq -r '.[0].deletion')" = latest ] || fail "newest snapshot not protected"

step "browse"
api GET "/snapshots/$SNAP1/browse?path="; echo "$BODY" | jq -c '[.entries[] | {name, kind}]'
api GET "/snapshots/$SNAP1/browse?path=reports"; echo "$BODY" | jq -c '[.entries[] | {name, size_bytes}]'
echo "$BODY" | jq -e '.entries[] | select(.name=="a.txt")' >/dev/null || fail "a.txt not listed"
api GET "/snapshots/$SNAP1/browse?path=../etc"; expect 400 "browse outside"
api GET "/snapshots/$SNAP1/browse?path=nothing"; expect 404 "browse missing"

step "restore: copy next to the original, replace after saving the current one"
echo "version 2" > /srv/vol1/docs/reports/a.txt
rm /srv/vol1/docs/b.txt
api POST "/snapshots/$SNAP1/restore" '{"path":"reports/a.txt","mode":"copy"}'; started "restore copy"
[ "$TASK_STATUS" = ok ] || fail "restore copy"
ls /srv/vol1/docs/reports
grep -q 'version 1' /srv/vol1/docs/reports/a\ \(restored\ *\).txt || fail "restored copy"
grep -q 'version 2' /srv/vol1/docs/reports/a.txt || fail "original changed by a copy restore"
api POST "/snapshots/$SNAP1/restore" '{"path":"b.txt","mode":"replace"}'; started "restore b"
grep -q 'keep me' /srv/vol1/docs/b.txt || fail "b.txt not restored"
api POST "/snapshots/$SNAP1/restore" '{"path":"reports/a.txt","mode":"replace"}'; started "restore replace"
grep -q 'version 1' /srv/vol1/docs/reports/a.txt || fail "replace"
ls /srv/vol1/docs/reports/ | grep -q 'a.txt.rynas-before-restore-' || fail "current version not saved"

step "retention preview and chain-safe delete"
btrfs -q subvolume snapshot -r /srv/vol1/docs /srv/vol1/.snapshots/docs/docs.20250101T0000
api POST /snapshots/jobs/preview-retention '{"job":{"name":"docs","share":"docs","retention":{"hourly":0,"daily":1,"min":"latest"}}}'
expect 200 "preview"; echo "$BODY" | jq -c .
echo "$BODY" | jq -e '.delete | index("docs.20250101T0000")' >/dev/null || fail "preview does not delete the old one"
[ -d /srv/vol1/.snapshots/docs/docs.20250101T0000 ] || fail "the preview deleted something"
api DELETE "/snapshots/$SNAP1"; expect 400 "delete without confirmation"
api DELETE "/snapshots/$SNAP1?confirm=$SNAP1"; expect 409 "delete the newest"
echo "newest kept: $(echo "$BODY" | jq -r .message)"
api DELETE "/snapshots/docs.20250101T0000?confirm=docs.20250101T0000"; started "delete old"
[ "$TASK_STATUS" = ok ] || fail "delete"
[ ! -d /srv/vol1/.snapshots/docs/docs.20250101T0000 ] || fail "old snapshot still there"

step "rollback of the whole share"
echo "after the snapshot" > /srv/vol1/docs/new.txt
api POST "/snapshots/$SNAP1/rollback" '{"confirm":"nope"}'; expect 400 "rollback without confirmation"
api POST "/snapshots/$SNAP1/rollback" "{\"confirm\":\"$SNAP1\"}"; started "rollback"
[ "$TASK_STATUS" = ok ] || fail "rollback"
[ ! -e /srv/vol1/docs/new.txt ] || fail "rollback kept new.txt"
grep -q 'keep me' /srv/vol1/docs/b.txt || fail "rollback content"
ls -d /srv/vol1/docs.rynas-rollback-* || fail "previous content not kept"
btrfs subvolume show /srv/vol1/docs | grep -E 'Name|Flags'

step "a user task as alice: runs, exit code and output recorded"
api POST /scheduler/tasks '{"id":"hello","name":"Hello","action":{"kind":"script","script":"#!/bin/sh\necho \"hello from $(id -un)\"\necho oops >&2\nexit 3","user":"alice"},"trigger":{"kind":"manual"},"notify":"always","notify_output":true}'
expect 201 "user task"
api POST /scheduler/tasks '{"id":"bad-user","action":{"kind":"command","command":"true","user":"nobody"},"trigger":{"kind":"manual"}}'
expect 400 "system account"; echo "nobody: $(echo "$BODY" | jq -r .message)"
api POST /scheduler/tasks '{"id":"bad-cal","action":{"kind":"command","command":"true"},"trigger":{"kind":"calendar","on_calendar":"Mon..Fry 25:00"}}'
expect 400 "invalid calendar"; echo "calendar: $(echo "$BODY" | jq -r .message)"
api POST /scheduler/tasks/hello/run; expect 409 "run before apply"
apply
stat -c '%U:%G %a %n' /var/lib/rynas/tasks/hello/run
[ "$(stat -c '%U:%G %a' /var/lib/rynas/tasks/hello/run)" = "root:root 700" ] || fail "script ownership"
api POST /scheduler/tasks/hello/run; expect 204 "run hello"
wait_run hello 1
[ "$(echo "$RUN" | jq -r .exit_code)" = 3 ] && [ "$(echo "$RUN" | jq -r .ok)" = false ] || fail "exit code not recorded"
api GET "/scheduler/tasks/hello/history/$(echo "$RUN" | jq -r .id)"
echo "$BODY" | jq -r .output | sed 's/^/    > /'
echo "$BODY" | jq -r .output | grep -q 'hello from alice' || fail "output not recorded or wrong user"
journalctl -u rynasd --no-pager | grep -q 'notification (Error, scheduler.task_failed)' || fail "no notification event"

step "calendar and cron previews"
api POST /scheduler/tasks/preview-calendar '{"expression":"0 4 * * 1-5","syntax":"cron"}'
expect 200 preview; echo "$BODY" | jq -c '{valid, on_calendar, normalized, next: (.next_ms | length)}'
[ "$(echo "$BODY" | jq -r .on_calendar)" = "Mon,Tue,Wed,Thu,Fri *-*-* 04:00:00" ] || fail "cron conversion"
[ "$(echo "$BODY" | jq '.next_ms | length')" = 5 ] || fail "next 5 runs"
api POST /scheduler/tasks/preview-calendar '{"expression":"bogus 99"}'
echo "$BODY" | jq -c '{valid, error}'
[ "$(echo "$BODY" | jq -r .valid)" = false ] || fail "invalid calendar accepted"

step "read-only views: system timers and crontabs"
echo '17 3 * * * root /usr/local/bin/nightly.sh' > /etc/cron.d/nightly
api GET /scheduler/system-timers; echo "$BODY" | jq -c '.[] | {unit, on_calendar}' | head -5
echo "$BODY" | jq -e '.[] | select(.unit | startswith("rynas-task-"))' >/dev/null && fail "rynas timers listed as system timers"
api GET /scheduler/cron-entries; echo "$BODY" | jq -c '.[] | {source, schedule, user, trigger}'
echo "$BODY" | jq -e '.[] | select(.source=="/etc/cron.d/nightly" and .trigger.on_calendar=="*-*-* 03:17:00")' >/dev/null \
    || fail "cron entry not converted"

step "system restore points from the apt hook (deduplicated)"
api PUT /snapshots/system '{"enabled":true,"subvolume":"/srv/vol1/sys","snapshot_dir":"/srv/vol1/.snapshots/sys","prefix":"sys","keep":3}'
expect 200 "system restore points"; echo "$BODY" | jq -c .
apply
cat /etc/apt/apt.conf.d/80rynas-snapshot
apt-get install -y -qq --no-install-recommends tree 2>&1 | grep -i 'rynas' || fail "the apt hook did not run"
ls /srv/vol1/.snapshots/sys
[ "$(ls /srv/vol1/.snapshots/sys | wc -l)" = 1 ] || fail "restore points: $(ls /srv/vol1/.snapshots/sys)"
/usr/libexec/rynas/rynasctl snapshot system --reason manual --subvolume /srv/vol1/sys \
    --snapshot-dir /srv/vol1/.snapshots/sys --prefix sys --keep 3 | tee /tmp/second.txt
grep -q 'no new restore point' /tmp/second.txt || fail "second restore point not deduplicated"
api GET '/snapshots?job=sys'; echo "$BODY" | jq -c '.[] | {id, readonly, reason}'
[ "$(echo "$BODY" | jq -r '.[0].reason')" = apt ] || fail "reason not recorded"
api PUT /snapshots/system '{"enabled":false}'; expect 200 "disable restore points"
apply
[ ! -e /etc/apt/apt.conf.d/80rynas-snapshot ] || fail "apt hook left"

step "removal: job, task, previous versions"
api DELETE /snapshots/jobs/docs; expect 200 "delete job"; echo "staged: $BODY"
api DELETE /scheduler/tasks/hello; expect 200 "delete task"
apply
[ ! -e /etc/rynas/btrbk/docs.conf ] || fail "btrbk configuration left"
[ ! -e /etc/systemd/system/rynas-task-snapshot-docs.service ] || fail "unit left"
[ ! -e /etc/systemd/system/rynas-task-hello.service ] || fail "unit left"
[ ! -e /var/lib/rynas/tasks/hello/run ] || fail "script left"
systemctl is-active rynas-task-snapshot-docs.timer && fail "timer still active" || true
testparm -s 2>/dev/null | grep -q shadow: && fail "shadow_copy2 still configured" || true
ls /srv/vol1/.snapshots/docs | grep -q docs. || fail "snapshots must survive the job"
api GET /config/drift; echo "$BODY" | jq -c '{entries: (.entries | length)}'
[ "$(echo "$BODY" | jq '.entries | length')" = 0 ] || fail "drift after apply"
journalctl -u rynasd --no-pager | grep -iE 'error|denied' | tail -5 || true
echo
echo "ALL CHECKS PASSED"
