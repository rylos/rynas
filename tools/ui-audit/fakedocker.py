"""Fake Docker Engine API on a Unix socket, for the UI audits (rynasd --docker-socket).

Serves the read endpoints bollard calls (/_ping, /version, /info, /containers/json,
/containers/{id}/json, /containers/{id}/logs, /images/json, /images/{name}/json,
/distribution/{name}/json, /volumes, /networks, /system/df, /events) with a few stacks:

- `media` (managed by rynas: working dir /volume1/docker/stacks/media), two services;
- `monitoring` (Portainer's compose folder), one running and one stopped service;
- `portainer` itself (a detected manager with its port);
- `homeassist`, untracked and unhealthy; a standalone stopped container.

Changes (start, stop, restart) answer 204 and change nothing: rynasd --dry-system never
sends them anyway. Usage: fakedocker.py <socket> [--cgroups]. With --cgroups the running
containers borrow the ids of `docker-<id>.scope` cgroups found on this machine (read-only),
so rynasd's stats sampler reads real CPU and memory numbers for the widget.
"""
import asyncio, glob, json, os, re, sys, time

SOCK = sys.argv[1]
NOW = int(time.time())


def cid(n):
    return (f"{n:02x}" * 32)[:64]


def ports(*specs):
    out = []
    for s in specs:
        pub, priv, proto = s
        out.append({"IP": "0.0.0.0", "PrivatePort": priv, "PublicPort": pub, "Type": proto})
    return out


def compose(project, service, wd, files=None):
    return {
        "com.docker.compose.project": project,
        "com.docker.compose.service": service,
        "com.docker.compose.project.working_dir": wd,
        "com.docker.compose.project.config_files": files or f"{wd}/compose.yaml",
    }


IMAGES = {
    "jellyfin/jellyfin:10.11.2": ("sha256:" + "a1" * 32, 1_290_000_000, "sha256:" + "b1" * 32),
    "syncthing/syncthing:2.0.10": ("sha256:" + "a2" * 32, 41_000_000, "sha256:" + "b2" * 32),
    "louislam/uptime-kuma:2.0.2": ("sha256:" + "a3" * 32, 470_000_000, "sha256:" + "b3" * 32),
    "grafana/grafana:12.2.1": ("sha256:" + "a4" * 32, 690_000_000, "sha256:" + "b4" * 32),
    "portainer/portainer-ce:2.33.5": ("sha256:" + "a5" * 32, 280_000_000, "sha256:" + "b5" * 32),
    "ghcr.io/home-assistant/home-assistant:2026.11.1": ("sha256:" + "a6" * 32, 1_800_000_000, "sha256:" + "b6" * 32),
    "traefik/whoami:v1.11": ("sha256:" + "a7" * 32, 7_000_000, "sha256:" + "b7" * 32),
}
# registry digests: jellyfin and grafana have a newer image
REMOTE = {k: v[2] for k, v in IMAGES.items()}
REMOTE["jellyfin/jellyfin:10.11.2"] = "sha256:" + "c1" * 32
REMOTE["grafana/grafana:12.2.1"] = "sha256:" + "c4" * 32

MEDIA = "/volume1/docker/stacks/media"
C = [
    dict(n=1, name="media-jellyfin-1", image="jellyfin/jellyfin:10.11.2", state="running", status="Up 3 days (healthy)", health="healthy",
         ports=ports((8096, 8096, "tcp")), labels=compose("media", "jellyfin", MEDIA)),
    dict(n=2, name="media-syncthing-1", image="syncthing/syncthing:2.0.10", state="running", status="Up 3 days", health=None,
         ports=ports((8384, 8384, "tcp"), (22000, 22000, "tcp"), (22000, 22000, "udp"), (21027, 21027, "udp")),
         labels={**compose("media", "syncthing", MEDIA), "homepage.href": "http://nas.example.lan:8384/"}),
    dict(n=3, name="monitoring-uptime-kuma-1", image="louislam/uptime-kuma:2.0.2", state="running", status="Up 26 hours (healthy)", health="healthy",
         ports=ports((3001, 3001, "tcp")), labels=compose("monitoring", "uptime-kuma", "/data/compose/3", "/data/compose/3/docker-compose.yml")),
    dict(n=4, name="monitoring-grafana-1", image="grafana/grafana:12.2.1", state="exited", status="Exited (0) 2 days ago", health=None,
         ports=[], labels=compose("monitoring", "grafana", "/data/compose/3", "/data/compose/3/docker-compose.yml")),
    dict(n=5, name="portainer", image="portainer/portainer-ce:2.33.5", state="running", status="Up 3 days", health=None,
         ports=ports((9443, 9443, "tcp"), (8000, 8000, "tcp")), labels={}),
    dict(n=6, name="homeassistant", image="ghcr.io/home-assistant/home-assistant:2026.11.1", state="running", status="Up 5 hours (unhealthy)",
         health="unhealthy", ports=[], labels=compose("homeassist", "homeassistant", "/srv/compose/homeassist")),
    dict(n=7, name="whoami", image="traefik/whoami:v1.11", state="exited", status="Exited (2) 6 days ago", health=None, ports=[], labels={}),
]

if "--cgroups" in sys.argv:
    real = [os.path.basename(p)[len("docker-"):-len(".scope")] for p in sorted(glob.glob("/sys/fs/cgroup/system.slice/docker-*.scope"))]
    for c, rid in zip([c for c in C if c["state"] == "running"], real):
        c["id"] = rid
for c in C:
    c.setdefault("id", cid(c["n"]))


def container_json(c):
    img = IMAGES[c["image"]]
    out = {
        "Id": c["id"], "Names": ["/" + c["name"]], "Image": c["image"], "ImageID": img[0],
        "Command": "/init", "Created": NOW - 86400 * c["n"], "Ports": c["ports"], "Labels": c["labels"],
        "State": c["state"], "Status": c["status"], "HostConfig": {"NetworkMode": "bridge"},
        "NetworkSettings": {"Networks": {}}, "Mounts": [],
    }
    if c["health"]:
        out["Health"] = {"Status": c["health"], "FailingStreak": 3 if c["health"] == "unhealthy" else 0}
    return out


def images_json():
    out = []
    for tag, (iid, size, digest) in IMAGES.items():
        repo = tag.rsplit(":", 1)[0]
        used = sum(1 for c in C if c["image"] == tag)
        out.append({"Id": iid, "ParentId": "", "RepoTags": [tag], "RepoDigests": [f"{repo}@{digest}"], "Created": NOW - 86400 * 20,
                    "Size": size, "SharedSize": -1, "Labels": {}, "Containers": used})
    out.append({"Id": "sha256:" + "a9" * 32, "ParentId": "", "RepoTags": [], "RepoDigests": [], "Created": NOW - 86400 * 60,
                "Size": 230_000_000, "SharedSize": -1, "Labels": {}, "Containers": 0})
    return out


VOLUMES = [
    ("media_cache", "media", 3_400_000_000, 1),
    ("monitoring_kuma-data", "monitoring", 120_000_000, 1),
    ("portainer_data", None, 2_000_000, 1),
    ("0f3c9a1b7e2d4c6a8b0e1f2a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e", None, 0, 0),
]


def volumes_json():
    vs = []
    for name, proj, _, _ in VOLUMES:
        labels = {"com.docker.compose.project": proj} if proj else {}
        vs.append({"Name": name, "Driver": "local", "Mountpoint": f"/volume1/docker/engine/volumes/{name}/_data", "Labels": labels,
                   "Options": {}, "Scope": "local", "CreatedAt": "2026-10-01T10:00:00Z"})
    return {"Volumes": vs, "Warnings": []}


def df_json():
    items = [{"Name": n, "UsageData": {"Size": s, "RefCount": r}} for n, _, s, r in VOLUMES]
    total = sum(v[1] for v in IMAGES.values()) + 230_000_000
    return {
        "ImageUsage": {"ActiveCount": 6, "TotalCount": 8, "Reclaimable": 920_000_000, "TotalSize": total},
        "ContainerUsage": {"ActiveCount": 5, "TotalCount": len(C), "Reclaimable": 0, "TotalSize": 61_000_000},
        "VolumeUsage": {"ActiveCount": 3, "TotalCount": len(VOLUMES), "Reclaimable": 0, "TotalSize": sum(v[2] for v in VOLUMES), "Items": items},
        "BuildCacheUsage": {"ActiveCount": 0, "TotalCount": 0, "Reclaimable": 0, "TotalSize": 0},
    }


NETWORKS = [
    {"Name": "bridge", "Id": "1" * 64, "Driver": "bridge", "Scope": "local", "Internal": False, "IPAM": {"Config": [{"Subnet": "172.17.0.0/16"}]}, "Labels": {}},
    {"Name": "media_default", "Id": "2" * 64, "Driver": "bridge", "Scope": "local", "Internal": False, "IPAM": {"Config": [{"Subnet": "10.200.0.0/24"}]},
     "Labels": {"com.docker.compose.project": "media"}},
    {"Name": "host", "Id": "3" * 64, "Driver": "host", "Scope": "local", "Internal": False, "IPAM": {"Config": []}, "Labels": {}},
]

VERSION = {"Version": "29.1.3", "ApiVersion": "1.52", "MinAPIVersion": "1.44", "Os": "linux", "Arch": "amd64", "GoVersion": "go1.25.3",
           "KernelVersion": "6.12.48+deb13-amd64", "GitCommit": "fake", "Components": []}


def info_json():
    running = sum(1 for c in C if c["state"] == "running")
    return {"ID": "fake", "Containers": len(C), "ContainersRunning": running, "ContainersPaused": 0, "ContainersStopped": len(C) - running,
            "Images": len(IMAGES) + 1, "Driver": "overlayfs", "DriverStatus": [["driver-type", "io.containerd.snapshotter.v1"]],
            "DockerRootDir": os.environ.get("FAKE_DOCKER_ROOT", "/volume1/docker/engine"), "CgroupDriver": "systemd", "CgroupVersion": "2",
            "LiveRestoreEnabled": True, "LoggingDriver": "local", "ServerVersion": VERSION["Version"], "OperatingSystem": "Debian GNU/Linux 13 (trixie)",
            "NCPU": 4, "MemTotal": 8 * 1024 ** 3, "Name": "nas"}


LOG_LINES = [
    "[info] starting service",
    "[info] listening on 0.0.0.0",
    "[warn] configuration file has no TLS section, serving plain HTTP",
    "[info] scheduled task finished in 412 ms",
    "[info] health check ok",
]


def frame(stream, text):
    data = (text + "\n").encode()
    return bytes([stream, 0, 0, 0]) + len(data).to_bytes(4, "big") + data


def find(ref):
    for c in C:
        if c["id"] == ref or c["name"] == ref or (len(ref) >= 12 and c["id"].startswith(ref)):
            return c
    return None


async def send(w, status, body=b"", ctype="application/json"):
    if isinstance(body, (dict, list)):
        body = json.dumps(body).encode()
    reason = {200: "OK", 204: "No Content", 404: "Not Found"}.get(status, "OK")
    head = f"HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nApi-Version: 1.52\r\nContent-Length: {len(body)}\r\n\r\n"
    w.write(head.encode() + body)
    await w.drain()


async def stream(w, ctype, chunks):
    """Chunked answer: `chunks` is an async generator of bytes; ends the connection after."""
    w.write(f"HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nApi-Version: 1.52\r\nTransfer-Encoding: chunked\r\n\r\n".encode())
    await w.drain()
    async for b in chunks:
        w.write(f"{len(b):x}\r\n".encode() + b + b"\r\n")
        await w.drain()
    w.write(b"0\r\n\r\n")
    await w.drain()


async def logs(c, q):
    tail = q.get("tail", "100")
    n = 30 if tail == "all" else min(int(tail or 0), 30)
    ts = q.get("timestamps") in ("1", "true")
    follow = q.get("follow") in ("1", "true")

    def line(i, t):
        txt = f"{c['name']}: {LOG_LINES[i % len(LOG_LINES)]} (#{i})"
        if ts:
            txt = time.strftime("%Y-%m-%dT%H:%M:%S.000000000Z ", time.gmtime(t)) + txt
        return frame(2 if "warn" in txt else 1, txt)

    start = int(time.time()) - n * 60
    for i in range(n):
        yield line(i, start + i * 60)
    if follow and c["state"] == "running":
        i = n
        while True:
            await asyncio.sleep(2)
            yield line(i, int(time.time()))
            i += 1


async def events():
    while True:
        await asyncio.sleep(3600)
        yield b""


async def handle(r, w):
    try:
        while True:
            line = await r.readline()
            if not line:
                return
            method, target, _ = line.decode().split(" ", 2)
            length = 0
            while True:
                h = await r.readline()
                if h in (b"\r\n", b"\n", b""):
                    break
                k, _, v = h.decode().partition(":")
                if k.lower() == "content-length":
                    length = int(v.strip())
            if length:
                await r.readexactly(length)
            path, _, qs = target.partition("?")
            q = dict(p.split("=", 1) if "=" in p else (p, "") for p in qs.split("&") if p)
            path = re.sub(r"^/v1\.\d+", "", path)
            if path == "/_ping":
                await send(w, 200, b"OK", "text/plain")
            elif path == "/version":
                await send(w, 200, VERSION)
            elif path == "/info":
                await send(w, 200, info_json())
            elif path == "/containers/json":
                await send(w, 200, [container_json(c) for c in C])
            elif path == "/images/json":
                await send(w, 200, images_json())
            elif path == "/volumes":
                await send(w, 200, volumes_json())
            elif path == "/networks":
                await send(w, 200, NETWORKS)
            elif path == "/system/df":
                await send(w, 200, df_json())
            elif path == "/events":
                await stream(w, "application/json", events())
                return
            elif m := re.fullmatch(r"/containers/([^/]+)/(json|logs|start|stop|restart)", path):
                c = find(m.group(1))
                if not c:
                    await send(w, 404, {"message": f"No such container: {m.group(1)}"})
                elif m.group(2) == "json":
                    await send(w, 200, container_json(c))
                elif m.group(2) == "logs":
                    await stream(w, "application/vnd.docker.multiplexed-stream", logs(c, q))
                    return
                else:
                    await send(w, 204)
            elif m := re.fullmatch(r"/images/(.+)/json", path):
                name = m.group(1).replace("%2F", "/").replace("%3A", ":")
                hit = next(((t, v) for t, v in IMAGES.items() if name in (t, v[0])), None)
                if hit:
                    await send(w, 200, {"Id": hit[1][0], "RepoTags": [hit[0]], "RepoDigests": [], "Size": hit[1][1]})
                else:
                    await send(w, 404, {"message": f"No such image: {name}"})
            elif m := re.fullmatch(r"/distribution/(.+)/json", path):
                name = m.group(1).replace("%2F", "/").replace("%3A", ":")
                d = REMOTE.get(name)
                if d:
                    await send(w, 200, {"Descriptor": {"mediaType": "application/vnd.oci.image.index.v1+json", "digest": d, "size": 1609},
                                        "Platforms": [{"architecture": "amd64", "os": "linux"}]})
                else:
                    await send(w, 404, {"message": "not found"})
            else:
                await send(w, 404, {"message": f"page not found: {path}"})
    except (ConnectionError, asyncio.IncompleteReadError, ValueError):
        pass
    finally:
        w.close()


async def main():
    if os.path.exists(SOCK):
        os.unlink(SOCK)
    srv = await asyncio.start_unix_server(handle, path=SOCK)
    os.chmod(SOCK, 0o600)
    async with srv:
        await srv.serve_forever()


asyncio.run(main())
