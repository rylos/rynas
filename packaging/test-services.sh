#!/usr/bin/env bash
# Users, groups, SMB shares and NFS exports for real, in a throw-away Debian 13 container
# booted with systemd (privileged: loop-mounted btrfs, systemd as PID 1).
#
#   packaging/test-services.sh [--no-build]
#
# Builds debug rynasd/rynas-web against trixie's glibc (builder image of build-deb.sh), then
# inside the container: installs samba, smbclient, nfs-kernel-server, acl, avahi, wsdd2 and
# the rynas units (hardening included), logs in over HTTP with a PAM account, creates groups,
# users and shares through the API, applies, sets passwords, and checks the result with
# testparm, getfacl, pdbedit, smbclient (allowed, read-only and denied users), exportfs,
# the drift report, a testparm failure (apply rolled back) and user disable/delete.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=rynas-test-systemd:trixie-2
NAME=rynas-test-services-$$
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
        -v "$ROOT/packaging:/opt/rynas/packaging:ro" "$IMAGE" >/dev/null
    trap 'docker rm -f "$NAME" >/dev/null' EXIT
    sleep 3
    docker exec -i -e DEBIAN_FRONTEND=noninteractive "$NAME" bash /opt/rynas/packaging/test-services.sh --inside
    exit
fi

# ---------------------------------------------------------------- inside the container
step() { printf '\n=== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; journalctl -u rynasd --no-pager | tail -40 >&2; exit 1; }

step "packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends samba smbclient nfs-kernel-server acl avahi-daemon \
    wsdd2 btrfs-progs jq passwd >/dev/null 2>&1
dpkg-query -W -f '${Package} ${Version}\n' samba smbclient nfs-kernel-server acl avahi-daemon wsdd2

step "a btrfs volume with subvolumes"
truncate -s 512M /var/tmp/vol1.img
mkfs.btrfs -q /var/tmp/vol1.img
mkdir -p /srv/vol1
mount -o loop /var/tmp/vol1.img /srv/vol1
btrfs -q subvolume create /srv/vol1/docs
btrfs -q subvolume create /srv/vol1/tm
mkdir /srv/vol1/plain
DOCS_UUID=$(btrfs subvolume show /srv/vol1/docs | awk '$1=="UUID:"{print $2}')
echo "docs subvolume UUID $DOCS_UUID"

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
useradd -m -s /bin/bash -G rynas-admin admin1
echo 'admin1:Adm1n-test-pw' | chpasswd
for _ in $(seq 50); do curl -s -o /dev/null http://127.0.0.1:8080/ && break; sleep 0.1; done
systemctl is-active rynasd.socket rynas-web

B=http://127.0.0.1:8080/api/v1
curl -s -c /tmp/jar -H 'Content-Type: application/json' -o /tmp/session.json \
    -d '{"username":"admin1","password":"Adm1n-test-pw"}' "$B/auth/login"
CSRF=$(jq -r .csrf_token /tmp/session.json)
[ -n "$CSRF" ] && [ "$CSRF" != null ] || fail "login: $(cat /tmp/session.json)"
echo "logged in as admin1"

# api METHOD PATH [JSON] -> body on stdout, HTTP status in $CODE
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
    for _ in $(seq 300); do
        api GET "/tasks/$id"
        st=$(echo "$BODY" | jq -r .status)
        case $st in ok|error|cancelled) break ;; esac
        sleep 0.2
    done
    TASK_STATUS=$st
    curl -s -b /tmp/jar "$B/tasks/$id/log" | sed 's/^/    | /'
}
apply() {
    api POST /config/apply '{"comment":"test-services"}'
    expect 202 "apply"
    wait_task "$(echo "$BODY" | jq -r .task_id)"
}
password() {
    api POST "/users/$1/password" "{\"password\":\"$2\"}"
    expect 202 "password $1"
    wait_task "$(echo "$BODY" | jq -r .task_id)"
    [ "$TASK_STATUS" = ok ] || fail "password task for $1: $TASK_STATUS"
}

step "groups and users (staged)"
api POST /groups '{"name":"office","description":"Office staff"}'; expect 201 "group office"
api POST /groups '{"name":"interns"}'; expect 201 "group interns"
api POST /users '{"name":"alice","full_name":"Alice Smith","email":"alice@example.com","groups":["office"],"shell":"shell"}'
expect 201 "user alice"; echo "$BODY" | jq -c '{user: .user.name, uid: .user.uid, status: .status}'
api POST /users '{"name":"bob","full_name":"Bob Jones","groups":["office","interns"]}'; expect 201 "user bob"
api POST /users '{"name":"carol","full_name":"Carol White","shell":"sftp","expires":"2031-12-31"}'; expect 201 "user carol"
api POST /users/alice/password '{"password":"Al1ce-smb-pw"}'
expect 409 "password before apply"; echo "password before apply: $(echo "$BODY" | jq -r .code)"

step "shares (staged): docs (multi-protocol, NFS), plain (standard), tm (Time Machine)"
api POST /shares "{\"name\":\"docs\",\"path\":\"/srv/vol1/docs\",\"description\":\"Office documents\",
  \"smb\":{\"enabled\":true,\"preset\":\"multiprotocol\"},
  \"nfs\":{\"enabled\":true,\"rules\":[{\"client\":\"127.0.0.1\"},{\"client\":\"10.0.0.0/8\",\"read_only\":true,\"squash\":\"all\"}]},
  \"permissions\":[{\"kind\":\"group\",\"name\":\"office\",\"access\":\"rw\"},
                   {\"kind\":\"group\",\"name\":\"interns\",\"access\":\"deny\"},
                   {\"kind\":\"user\",\"name\":\"carol\",\"access\":\"ro\"}]}"
expect 201 "share docs"
echo "$BODY" | jq -c '{fsid: .share.nfs.fsid, status: .status}'
[ "$(echo "$BODY" | jq -r .share.nfs.fsid)" = "$DOCS_UUID" ] || fail "fsid is not the subvolume UUID"
api POST /shares '{"name":"plain","path":"/srv/vol1/plain","permissions":[{"kind":"user","name":"alice","access":"rw"}]}'
expect 201 "share plain"
api POST /shares '{"name":"tm","path":"/srv/vol1/tm","smb":{"enabled":true,"preset":"timemachine","timemachine_max_size":"200G","recycle":{"enabled":false}},"permissions":[{"kind":"user","name":"alice","access":"rw"}]}'
expect 201 "share tm"
api POST /shares '{"name":"bad","path":"/srv/vol1/nope"}'
expect 400 "missing path refused"; echo "missing path: $(echo "$BODY" | jq -r .message)"
api GET "/shares/docs/effective?principal=bob"
echo "effective bob: $(echo "$BODY" | jq -r '.access + " — " + .explanation')"
api GET "/shares/docs/effective?principal=alice"
echo "effective alice: $(echo "$BODY" | jq -r '.access + " — " + .explanation')"
api GET /config/pending
echo "$BODY" | jq -r '.changes[] | .key as $k | .diff[] | "  " + .path + " " + .op' | head -20

step "apply"
apply
[ "$TASK_STATUS" = ok ] || fail "apply: $TASK_STATUS"

step "passwords (chpasswd + pdbedit as one transaction)"
password alice 'Al1ce-smb-pw'
password bob 'B0b-smb-pw-1'
password carol 'Car0l-smb-pw'

step "system state"
getent passwd alice bob carol
getent group office interns rynas-ssh rynas-sftp rynas-admin
chage -l carol | grep 'Account expires'
pdbedit -L
testparm -s 2>/dev/null | sed -n '/^\[docs\]/,/^$/p'
testparm -s >/dev/null 2>/tmp/testparm.err || fail "testparm"; grep -i 'unknown\|error' /tmp/testparm.err && fail "testparm warnings" || true
getfacl -p /srv/vol1/docs
getfacl -p /srv/vol1/docs | grep -q '^user:bob:---' || fail "deny of interns not expanded to bob"
cat /etc/exports.d/rynas.exports
cat /etc/nfs.conf.d/rynas.conf | head -8
cat /etc/avahi/services/rynas-smb.service
systemctl is-active smbd wsdd2 avahi-daemon nfs-server || true
exportfs -v 2>&1 || true
mkdir -p /mnt/nfs
if mount -t nfs4 -o vers=4.2 127.0.0.1:/srv/vol1/docs /mnt/nfs 2>/tmp/nfs.err; then
    findmnt -n -o SOURCE,FSTYPE,OPTIONS /mnt/nfs | cut -c1-80
    # root_squash: root is nobody over NFS, and the ACL gives others nothing
    ls /mnt/nfs >/dev/null 2>&1 && fail "root not squashed" || echo "NFSv4.2 mount OK, root squashed"
    umount /mnt/nfs
else
    echo "NFS mount not possible in this container: $(cat /tmp/nfs.err)"
fi

step "SMB logins"
smbclient -L //localhost -U 'alice%Al1ce-smb-pw' -m SMB3 2>&1 | grep -E 'Disk|Sharename' || fail "smbclient -L alice"
echo hello > /tmp/hello.txt
smbclient //localhost/docs -U 'alice%Al1ce-smb-pw' -c 'put /tmp/hello.txt hello.txt; ls' 2>&1 | grep hello.txt || fail "alice write docs"
getfacl -p /srv/vol1/docs/hello.txt | grep -E '^(user|group|mask)'
getfacl -p /srv/vol1/docs/hello.txt | grep -q '^group:office:rw' || fail "new file not writable by office"
smbclient //localhost/docs -U 'alice%Al1ce-smb-pw' -c 'mkdir sub; put /tmp/hello.txt sub/x.txt' >/dev/null 2>&1
getfacl -p /srv/vol1/docs/sub | grep -E '^(default:)?(group:office|mask)'
smbclient //localhost/docs -U 'carol%Car0l-smb-pw' -c 'get hello.txt /tmp/carol.txt' >/dev/null 2>&1 && cmp -s /tmp/hello.txt /tmp/carol.txt || fail "carol read docs"
out=$(smbclient //localhost/docs -U 'carol%Car0l-smb-pw' -c 'put /tmp/hello.txt carol.txt' 2>&1 || true)
echo "carol writing: $(echo "$out" | grep -o 'NT_STATUS_[A-Z_]*' | head -1)"
[ ! -e /srv/vol1/docs/carol.txt ] || fail "carol (read-only) could write"
out=$(smbclient //localhost/docs -U 'bob%B0b-smb-pw-1' -c ls 2>&1 || true)
echo "bob on docs: $(echo "$out" | grep -o 'NT_STATUS_[A-Z_]*' | head -1)"
echo "$out" | grep -q NT_STATUS_ACCESS_DENIED || fail "bob (interns: deny) got in"
smbclient //localhost/docs -U 'alice%Al1ce-smb-pw' -c 'del hello.txt' >/dev/null 2>&1
ls -la '/srv/vol1/docs/#recycle/' | grep hello.txt && echo "deleted file is in #recycle: OK"
out=$(smbclient //localhost/docs -U 'alice%wrong-password' -c ls 2>&1 || true)
echo "wrong password: $(echo "$out" | grep -o 'NT_STATUS_[A-Z_]*' | head -1)"

step "recycle cleanup task"
api POST "/services/smb/recycle-cleanup?share=docs&days=1"; expect 202 "recycle cleanup"
wait_task "$(echo "$BODY" | jq -r .task_id)"
[ "$TASK_STATUS" = ok ] || fail "recycle cleanup"

step "drift: none, then a user created outside rynas"
api GET /config/drift; echo "$BODY" | jq -c .
[ "$(echo "$BODY" | jq '.entries | length')" = 0 ] || fail "file drift right after apply"
useradd -m eve
api GET /config/drift; echo "$BODY" | jq -c '.state'
echo "$BODY" | jq -e '.state[] | select(.object=="user:eve" and .kind=="unmanaged")' >/dev/null || fail "eve not reported"
api GET /users/eve; echo "eve: $(echo "$BODY" | jq -c .status.badges)"

step "testparm refuses an unknown extra option: apply rolled back"
SUM=$(sha256sum /etc/samba/smb.conf)
api PUT /shares/plain '{"path":"/srv/vol1/plain","smb":{"enabled":true,"extra":"bogus option = 1"},"permissions":[{"kind":"user","name":"alice","access":"rw"}]}'
expect 200 "stage bogus option"
apply
[ "$TASK_STATUS" = error ] || fail "apply with a bogus option did not fail"
[ "$(sha256sum /etc/samba/smb.conf)" = "$SUM" ] || fail "smb.conf changed"
echo "smb.conf unchanged: OK"
api POST /config/discard; expect 204 discard

step "disable bob, delete carol with her home"
api GET /users/bob
api PUT /users/bob "$(echo "$BODY" | jq -c '.user + {disabled: true}')"; expect 200 "disable bob"
api DELETE "/users/carol?remove_home=true"; expect 400 "delete without confirmation"
api DELETE "/users/carol?remove_home=true&confirm=carol"; expect 200 "delete carol"
echo "staged: $BODY"
apply
[ "$TASK_STATUS" = ok ] || fail "apply disable/delete"
chage -l bob | grep 'Account expires'
pdbedit -L -v bob | grep 'Account Flags'
out=$(smbclient //localhost/plain -U 'bob%B0b-smb-pw-1' -c ls 2>&1 || true)
echo "disabled bob: $(echo "$out" | grep -o 'NT_STATUS_[A-Z_]*' | head -1)"
echo "$out" | grep -q NT_STATUS_ || fail "disabled bob could log in"
! getent passwd carol || fail "carol still exists"
[ ! -d /home/carol ] || fail "carol's home still exists"
! pdbedit -L | grep -q '^carol:' || fail "carol still in tdbsam"
grep -c carol /etc/samba/smb.conf && fail "carol still in smb.conf" || echo "carol removed from the share lists: OK"

step "revisions"
api GET /config/revisions; echo "$BODY" | jq -c '.[] | {id, author, keys}'
journalctl -u rynasd --no-pager | grep -iE 'error|denied' | tail -5 || true
echo
echo "ALL CHECKS PASSED"
