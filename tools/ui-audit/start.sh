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
# With synodeb, rynasd's simulated synodeb-ups answers 'netserver status|set|off' like synodeb 4.6
# (state in $R/state/dry-root/etc/synodeb/ups-network.conf, no passwords);
# FAKE_SYNODEB_VERSION=4.5 makes it a synodeb before the network UPS server (the update note).
# FAKE_USB=1: fake removable drives (a mounted USB stick, a USB disk with NTFS and a locked
# LUKS volume, an SD card) below the fake tree: the USB icon of the app bar and its menu.
# FAKE_DOCKER=1: a fake Docker Engine (fakedocker.py on $R/docker.sock: a rynas stack, a Portainer
# stack, an untracked unhealthy one, a manager) for the Docker page and widget; the running
# containers borrow this machine's docker-*.scope cgroup ids for real stats numbers (read-only;
# FAKE_DOCKER_CGROUPS=0 turns that off). rynasd checks administrators from the system groups, so a
# second login named after the current Unix user (same password) is added and its primary group
# is the admin group: use it for logs, actions and stacks (AUDIT_USER=$(id -un) for the probes).
# Without FAKE_DOCKER rynasd never contacts a Docker socket (not even the host's).
# The app catalogue is the repository's apps/ (rynasd --apps-dir), so the Apps & Plugins page
# shows the curated entries; installs under --dry-system only create files below the dry root.
# FAKE_CADDY=1: a fake admin socket of the web access proxy (fakecaddy.py on $R/caddy-admin.sock:
# GET /config/ and POST /load only, no Caddy, no port) so the Web access page shows the engine
# installed and running and rynasd applies its defaults; FAKE_CADDY=refuse answers every load
# with an error (the "last load failed" state). Without it rynasd sees no proxy (the socket
# path below the instance folder does not exist), never the host's.
# FAKE_TAILSCALE=running|login|expiring: a fake tailscaled LocalAPI (faketailscale.py on $R/ts.sock)
# and the CLI's path in the dry root, so Network > Remote access shows Tailscale installed and
# connected (a subnet waiting for approval), logged out with a login address, or with a key about
# to expire. Without it rynasd --dry-system never contacts a tailscaled socket (Install offered).
# TLS_LISTEN=off: rynas-web without its HTTPS port (8443 is taken when another instance runs).
# FAKE_LEDS=1: synohwd's status says the front LED level chip is driven (synodeb 4.8, scale 0..3):
# Hardware > Synology shows the front LED brightness schedule card.
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
    if [ -n "${FAKE_SYNODEB_VERSION:-}" ]; then echo "$FAKE_SYNODEB_VERSION" > "$D/synodeb/fake-version"; else rm -f "$D/synodeb/fake-version"; fi
  else
    mkdir -p "$D/nut"
    [ -f "$D/nut/nut.conf" ] || printf 'MODE=standalone\n' > "$D/nut/nut.conf"
    [ -f "$D/nut/upsd.users" ] || { printf '[upsmon]\n\tpassword = fake\n\tupsmon primary\n' > "$D/nut/upsd.users"; chmod 640 "$D/nut/upsd.users"; }
  fi
fi
DOCKER_ARGS=(--docker-socket "")
USERS=$HERE/users.toml
if [ -n "${FAKE_DOCKER:-}" ]; then
  CG=--cgroups; [ "${FAKE_DOCKER_CGROUPS:-1}" = 0 ] && CG=
  python3 -I "$HERE/fakedocker.py" "$R/docker.sock" $CG > "$R/fakedocker.log" 2>&1 &
  echo $! >> "$R/pids"
  D=$R/state/dry-root
  mkdir -p "$D/etc/docker" "$D/volume1/docker/stacks/media" "$D/volume1/docker/stacks/paperless"
  [ -f "$D/etc/docker/daemon.json" ] || printf '{"data-root": "/volume1/docker/engine", "log-driver": "local"}\n' > "$D/etc/docker/daemon.json"
  [ -f "$D/volume1/docker/stacks/media/compose.yaml" ] || cat > "$D/volume1/docker/stacks/media/compose.yaml" <<'YAML'
services:
  jellyfin:
    image: jellyfin/jellyfin:10.11.2
    restart: unless-stopped
    ports:
      - "8096:8096"
    volumes:
      - ./config:/config
      - /volume1/media:/media:ro
  syncthing:
    image: syncthing/syncthing:2.0.10
    restart: unless-stopped
    environment:
      - PUID=${PUID}
      - PGID=${PGID}
    ports:
      - "8384:8384"
      - "22000:22000"
YAML
  [ -f "$D/volume1/docker/stacks/media/.env" ] || printf 'PUID=1000\nPGID=1000\n' > "$D/volume1/docker/stacks/media/.env"
  [ -f "$D/volume1/docker/stacks/paperless/compose.yaml" ] || printf 'services:\n  broker:\n    image: redis:8\n    restart: unless-stopped\n' > "$D/volume1/docker/stacks/paperless/compose.yaml"
  # projects to adopt (§5.7): hand-made, Portainer's data, untracked, a restored folder
  NC=$D/volume1/docker/nextcloud; PD=$D/volume1/docker/engine/volumes/portainer_data/_data/compose/3
  mkdir -p "$NC" "$PD" "$D/srv/compose/homeassist" "$D/volume1/restore/vaultwarden"
  [ -f "$NC/docker-compose.yml" ] || printf 'services:\n  app:\n    image: nextcloud:31-apache\n    ports:\n      - "8080:80"\n    volumes:\n      - ./html:/var/www/html\n    networks: [proxy]\nnetworks:\n  proxy:\n    external: true\n' > "$NC/docker-compose.yml"
  [ -f "$NC/docker-compose.override.yml" ] || printf 'services:\n  app:\n    restart: unless-stopped\n' > "$NC/docker-compose.override.yml"
  [ -f "$NC/nextcloud.env" ] || printf 'TZ=Europe/Rome\n' > "$NC/nextcloud.env"
  [ -f "$PD/docker-compose.yml" ] || printf 'services:\n  uptime-kuma:\n    image: louislam/uptime-kuma:2.0.2\n  grafana:\n    image: grafana/grafana:12.2.1\n' > "$PD/docker-compose.yml"
  [ -f "$D/srv/compose/homeassist/compose.yaml" ] || printf 'services:\n  homeassistant:\n    image: ghcr.io/home-assistant/home-assistant:2026.11.1\n    network_mode: host\n' > "$D/srv/compose/homeassist/compose.yaml"
  [ -f "$D/volume1/restore/vaultwarden/compose.yaml" ] || printf 'name: vaultwarden\nservices:\n  server:\n    image: vaultwarden/server:1.34.3\n    volumes:\n      - ./data:/data\n  backup:\n    image: busybox\n    profiles: [backup]\n' > "$D/volume1/restore/vaultwarden/compose.yaml"
  for _ in $(seq 30); do [ -S "$R/docker.sock" ] && break; sleep 0.1; done
  DOCKER_ARGS=(--docker-socket "$R/docker.sock" --admin-group "$(id -gn)")
  USERS=$R/users.toml
  { cat "$HERE/users.toml"; printf '\n[[user]]\nname = "%s"\n' "$(id -un)"; grep '^hash' "$HERE/users.toml"; grep '^groups' "$HERE/users.toml"; } > "$USERS"
fi
# ADMIN=1 without FAKE_DOCKER: the same admin login, and no engine at all (the Apps & Plugins page
# without Docker: the module card first, installs blocked)
if [ -n "${ADMIN:-}" ] && [ -z "${FAKE_DOCKER:-}" ]; then
  DOCKER_ARGS=(--docker-socket "" --admin-group "$(id -gn)")
  USERS=$R/users.toml
  { cat "$HERE/users.toml"; printf '\n[[user]]\nname = "%s"\n' "$(id -un)"; grep '^hash' "$HERE/users.toml"; grep '^groups' "$HERE/users.toml"; } > "$USERS"
fi
CADDY_SOCK=$R/caddy-admin.sock
rm -f "$CADDY_SOCK"
if [ -n "${FAKE_CADDY:-}" ]; then
  REFUSE=; [ "$FAKE_CADDY" = refuse ] && REFUSE=refuse
  python3 -I "$HERE/fakecaddy.py" "$CADDY_SOCK" $REFUSE > "$R/fakecaddy.log" 2>&1 &
  echo $! >> "$R/pids"
  for _ in $(seq 30); do [ -S "$CADDY_SOCK" ] && break; sleep 0.1; done
fi
TS_ARGS=()
if [ -n "${FAKE_TAILSCALE:-}" ]; then
  python3 -I "$HERE/faketailscale.py" "$R/ts.sock" "$FAKE_TAILSCALE" > "$R/faketailscale.log" 2>&1 &
  echo $! >> "$R/pids"
  mkdir -p "$R/state/dry-root/usr/bin"; : > "$R/state/dry-root/usr/bin/tailscale"
  for _ in $(seq 30); do [ -S "$R/ts.sock" ] && break; sleep 0.1; done
  TS_ARGS=(--tailscale-socket "$R/ts.sock")
else
  rm -f "$R/state/dry-root/usr/bin/tailscale"
fi
# any docker CLI rynasd might run reads the fake engine, never the host's
DOCKER_HOST=unix://$R/docker.sock target/release/rynasd --socket "$R/rynasd.sock" --state-dir "$R/state" --dry-system --hw-root "$H" \
  --notify-socket "" --storage-check-interval 0 --apps-dir "$REPO/apps" --caddy-admin "$CADDY_SOCK" "${RYNASD_UPS[@]}" "${DOCKER_ARGS[@]}" "${TS_ARGS[@]}" ${FAKE_USB:+--removable-demo} > "$R/rynasd.log" 2>&1 &
echo $! >> "$R/pids"; sleep 1.5
target/release/rynas-web --listen "127.0.0.1:$PORT" --rynasd-socket "$R/rynasd.sock" --users-file "$USERS" \
  --auth-state-dir "$R/web" --ui-dir "${UI_DIR:-crates/rynas-ui/dist}" --hw-root "$H" "${UPSD_ARG[@]}" ${TLS_LISTEN:+--tls-listen "$TLS_LISTEN"} > "$R/web.log" 2>&1 &
echo $! >> "$R/pids"
for _ in $(seq 50); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && { echo "up on http://127.0.0.1:$PORT"; exit 0; }; sleep 0.1; done
echo down; tail -n 5 "$R/rynasd.log" "$R/web.log"; exit 1
