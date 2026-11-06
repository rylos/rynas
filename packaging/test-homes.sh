#!/usr/bin/env bash
# User homes for real, in a throw-away Debian 13 container booted with systemd (privileged:
# loop-mounted btrfs, systemd as PID 1, getty/vconsole masked so the host's tty stays alone).
#
#   packaging/test-homes.sh [--no-build]
#
# Builds debug rynasd/rynas-web against trixie's glibc (builder image of build-deb.sh), then
# inside the container: a btrfs volume at /volume1, the rynas units (hardening included), an
# "install account" admin1 (uid 1000) with dotfiles, SSH keys, a hard link, an ACL and an xattr
# in /home/admin1. Through the API: turns user homes on (with SMB home folders), creates a user
# (home = a subvolume behind /srv/rynas/homes), opens her SMB home, checks the move plan and
# its blockers (a running process), moves admin1's home (contents, ACL, xattr, hard link and
# login checked), undoes it, moves it again and confirms, deletes the user with her home, and
# turns the service off (homes keep working).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-systemd:trixie-2
NAME=rynas-test-homes-$$
BIN=$ROOT/target/deb-build/debug

if [ "${1:-}" != "--inside" ]; then
    if [ "${1:-}" != "--no-build" ]; then
        docker build -q -t rynas-deb-builder:trixie --build-arg RUST_IMAGE=rust:1.98-trixie \
            -f "$ROOT/packaging/deb/builder.Dockerfile" "$ROOT/packaging/deb" >/dev/null
        mkdir -p "$ROOT/target/deb-build"
        docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e CARGO_HOME=/tmp/cargo \
            -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}" \
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
        -v "$ROOT/packaging:/opt/rynas/packaging:ro" "$IMAGE" >/dev/null
    trap 'docker rm -f "$NAME" >/dev/null' EXIT
    sleep 3
    docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash /opt/rynas/packaging/test-homes.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
trap 'echo "FAIL: line $LINENO: $BASH_COMMAND" >&2' ERR
step() { printf '\n=== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; journalctl -u rynasd --no-pager | tail -40 >&2; exit 1; }

step "packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends adduser samba smbclient acl attr btrfs-progs rsync jq passwd >/dev/null 2>&1
dpkg-query -W -f '${Package} ${Version}\n' samba rsync btrfs-progs

step "btrfs volumes at /volume1 and /volume2 (mounted before rynasd, as at boot)"
truncate -s 1G /var/tmp/vol1.img
mkfs.btrfs -q /var/tmp/vol1.img
mkdir -p /volume1 /volume2
mount -o loop /var/tmp/vol1.img /volume1
# a second "volume": another subvolume of the same loop device (one free loop device is enough)
btrfs -q subvolume create /volume1/@vol2
mount -o subvol=@vol2 "$(findmnt -n -o SOURCE /volume1)" /volume2
findmnt -n -o TARGET,FSTYPE /volume1; findmnt -n -o TARGET,FSTYPE /volume2

step "rynas installed as the package does it (units with their hardening)"
install -d /usr/libexec/rynas /usr/share/rynas/ui
install -m 755 /opt/rynas/rynasd /opt/rynas/rynas-web /usr/libexec/rynas/
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

step "the install account (uid 1000) with a lived-in home on the system disk"
useradd -m -u 1000 -s /bin/bash -G rynas-admin admin1
echo 'admin1:Adm1n-test-pw' | chpasswd
H=/home/admin1
install -d -m 700 -o admin1 -g admin1 $H/.ssh $H/docs
echo 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest admin1@laptop' > $H/.ssh/authorized_keys
echo 'notes' > $H/docs/notes.txt
ln $H/docs/notes.txt $H/docs/notes-hardlink.txt
ln -s docs/notes.txt $H/notes-link
setfacl -m u:nobody:r $H/docs/notes.txt
setfattr -n user.rynas.test -v kept $H/docs/notes.txt
head -c 3M /dev/urandom > $H/docs/blob.bin
chown -R admin1:admin1 $H
chmod 600 $H/.ssh/authorized_keys
( cd $H && find . -printf '%p %m %u %n\n' | sort ) > /tmp/before.txt
sha256sum $H/docs/blob.bin | cut -d' ' -f1 > /tmp/blob.sum

for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:8080/ && break; sleep 0.1; done
B=http://127.0.0.1:8080/api/v1
login() {
    curl -s -c /tmp/jar -H 'Content-Type: application/json' -o /tmp/session.json \
        -d '{"username":"admin1","password":"Adm1n-test-pw"}' "$B/auth/login"
    CSRF=$(jq -r .csrf_token /tmp/session.json)
    [ -n "$CSRF" ] && [ "$CSRF" != null ] || fail "login: $(cat /tmp/session.json)"
}
login
echo "logged in as admin1"

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
task() { expect 202 "$1"; wait_task "$(echo "$BODY" | jq -r .task_id)"; }
apply() { api POST /config/apply '{"comment":"test-homes"}'; task apply; [ "$TASK_STATUS" = ok ] || fail "apply: $TASK_STATUS"; }

step "service off: the volumes offered, a move refused"
api GET /services/homes; expect 200 "homes state"
echo "$BODY" | jq -c '{enabled: .settings.enabled, volumes: [.volumes[].path], homes: [.homes[] | {user, location}]}'
[ "$(echo "$BODY" | jq -c '[.volumes[].path]')" = '["/volume1","/volume2"]' ] || fail "volumes offered: $BODY"
api POST /services/homes/move '{"users":["admin1"]}'; expect 409 "move while off"

step "turn user homes on (SMB home folders with recycle bin)"
api PUT /services/homes '{"enabled":true,"volume":"/","smb":{"enabled":true}}'; expect 400 "system disk refused"
api PUT /services/homes '{"enabled":true,"volume":"/volume1","smb":{"enabled":true,"recycle":{"enabled":true,"retention_days":30}}}'
expect 200 "stage homes"
apply
[ "$(readlink /srv/rynas/homes)" = /volume1/homes ] || fail "link: $(readlink /srv/rynas/homes)"
btrfs subvolume show /volume1/homes >/dev/null || fail "homes folder is not a subvolume"
[ "$(stat -c '%a %U' /volume1/homes)" = "711 root" ] || fail "homes folder mode: $(stat -c '%a %U' /volume1/homes)"
testparm -s 2>/dev/null | sed -n '/^\[homes\]/,/^$/p'
testparm -s 2>/dev/null | grep -q '^\[homes\]' || fail "no [homes] section"
systemctl is-active smbd >/dev/null || systemctl start smbd

step "a new user: home is a subvolume in the homes folder"
api POST /users '{"name":"alice","full_name":"Alice Smith"}'; expect 201 "user alice"
[ "$(echo "$BODY" | jq -r .user.home)" = /srv/rynas/homes/alice ] || fail "alice home: $BODY"
apply
getent passwd alice
[ "$(getent passwd alice | cut -d: -f6)" = /srv/rynas/homes/alice ] || fail "passwd home of alice"
btrfs subvolume show /volume1/homes/alice >/dev/null || fail "alice's home is not a subvolume"
[ "$(stat -c '%a %U' /volume1/homes/alice)" = "700 alice" ] || fail "alice home mode: $(stat -c '%a %U' /volume1/homes/alice)"
[ -z "$(ls -A /volume1/homes/alice)" ] || fail "an SMB-only user got skeleton files"
api POST /users/alice/password '{"password":"Al1ce-smb-pw"}'; task "alice password"
echo hello > /tmp/hello.txt
smbclient //localhost/alice -U 'alice%Al1ce-smb-pw' -c 'put /tmp/hello.txt hello.txt' >/dev/null 2>&1 || fail "alice's SMB home"
[ -f /volume1/homes/alice/hello.txt ] || fail "file not in alice's home"
smbclient //localhost/alice -U 'alice%Al1ce-smb-pw' -c 'del hello.txt' >/dev/null 2>&1
ls '/volume1/homes/alice/#recycle/' | grep -q hello.txt || fail "deleted file not in the home's recycle bin"
out=$(smbclient //localhost/admin1 -U 'alice%Al1ce-smb-pw' -c ls 2>&1 || true)
echo "alice on admin1's home: $(echo "$out" | grep -o 'NT_STATUS_[A-Z_]*' | head -1)"
echo "$out" | grep -q NT_STATUS_ || fail "alice opened another user's home"

step "the move plan of admin1, and a running process that blocks it"
su -s /bin/sh admin1 -c 'exec sleep 600' &
SLEEPER=$!
sleep 0.5
api POST /services/homes/move/plan '{"users":["admin1"]}'; expect 200 "plan"
echo "$BODY" | jq -c '.[0] | {from, to, target, subvolume, size_bytes, ssh_keys, allowed, blockers}'
echo "$BODY" | jq -e '.[0].blockers | map(.code) | index("processes")' >/dev/null || fail "process not blocking"
api POST /services/homes/move '{"users":["admin1"]}'; expect 409 "move with a process"
kill $SLEEPER; wait $SLEEPER 2>/dev/null || true
api POST /services/homes/move/plan '{"users":["admin1"]}'
echo "$BODY" | jq -e '.[0].allowed and .[0].ssh_keys and .[0].subvolume' >/dev/null || fail "plan: $BODY"

compare_home() {
    local d=$1
    ( cd "$d" && find . -printf '%p %m %u %n\n' | sort ) > /tmp/after.txt
    diff /tmp/before.txt /tmp/after.txt | grep -v '^[<>] \. ' | grep '^[<>]' && fail "tree of $d differs" || true
    [ "$(sha256sum "$d/docs/blob.bin" | cut -d' ' -f1)" = "$(cat /tmp/blob.sum)" ] || fail "blob differs"
    getfacl -p "$d/docs/notes.txt" | grep -q '^user:nobody:r--' || fail "ACL lost in $d"
    [ "$(getfattr --only-values -n user.rynas.test "$d/docs/notes.txt")" = kept ] || fail "xattr lost in $d"
    [ "$(stat -c %h "$d/docs/notes.txt")" = 2 ] || fail "hard link lost in $d"
    [ "$(readlink "$d/notes-link")" = docs/notes.txt ] || fail "symlink lost in $d"
}

step "move admin1's home"
api POST /services/homes/move '{"users":["admin1"]}'; task "move"
[ "$TASK_STATUS" = ok ] || fail "move: $TASK_STATUS"
[ "$(getent passwd admin1 | cut -d: -f6)" = /srv/rynas/homes/admin1 ] || fail "passwd not switched"
btrfs subvolume show /volume1/homes/admin1 >/dev/null || fail "admin1's new home is not a subvolume"
[ "$(stat -c '%a %U' /volume1/homes/admin1)" = "700 admin1" ] || fail "new home mode"
[ -d /home/admin1.rynas-old ] && [ ! -e /home/admin1 ] || fail "old home not kept aside"
compare_home /srv/rynas/homes/admin1
su -s /bin/sh admin1 -c 'cd && pwd && cat .ssh/authorized_keys >/dev/null' || fail "admin1 cannot use the new home"
login
api GET /services/homes
echo "$BODY" | jq -c '.homes[] | {user, home, location, subvolume, kept: .moved.kept}'
api DELETE /users/admin1; expect 409 "delete while a move waits"

step "undo"
api POST /services/homes/move/admin1/undo; task "undo"
[ "$TASK_STATUS" = ok ] || fail "undo: $TASK_STATUS"
[ "$(getent passwd admin1 | cut -d: -f6)" = /home/admin1 ] || fail "passwd not back"
compare_home /home/admin1
ls -A /volume1/homes | grep -q '^\.admin1\.undone-' || fail "copy not kept aside"
btrfs subvolume delete /volume1/homes/.admin1.undone-* >/dev/null

step "move again, then confirm"
api POST /services/homes/move '{"users":["admin1"]}'; task "move again"
[ "$TASK_STATUS" = ok ] || fail "second move: $TASK_STATUS"
api POST /services/homes/move/admin1/finish; task "finish"
[ "$TASK_STATUS" = ok ] || fail "finish: $TASK_STATUS"
[ ! -e /home/admin1.rynas-old ] || fail "old home not deleted"
compare_home /srv/rynas/homes/admin1

step "the volume cannot change under homes; delete alice with her home"
api PUT /services/homes '{"enabled":true,"volume":"/volume2","smb":{"enabled":true}}'; expect 400 "volume change under homes"
echo "refused: $(echo "$BODY" | jq -r .message)"
echo "$BODY" | jq -r .message | grep -q 'holds homes' || fail "refused for another reason"
api DELETE "/users/alice?remove_home=true&confirm=alice"; expect 200 "delete alice"
apply
! getent passwd alice || fail "alice still exists"
[ ! -e /volume1/homes/alice ] || fail "alice's subvolume still there"

step "service off: the [homes] share goes, the homes keep working"
api PUT /services/homes '{"enabled":false,"volume":"/volume1","smb":{"enabled":true}}'; expect 200 "service off"
apply
testparm -s 2>/dev/null | grep -q '^\[homes\]' && fail "[homes] still there"
[ "$(readlink /srv/rynas/homes)" = /volume1/homes ] || fail "link removed"
su -s /bin/sh admin1 -c 'cd && ls docs >/dev/null' || fail "admin1's home broken"
api POST /users '{"name":"bob"}'; expect 201 "user bob"
[ "$(echo "$BODY" | jq -r .user.home)" = /home/bob ] || fail "bob home while off: $BODY"

journalctl -u rynasd --no-pager | grep -iE 'error|denied' | tail -5 || true
echo
echo "ALL CHECKS PASSED"
