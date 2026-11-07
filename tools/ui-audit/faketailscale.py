"""Fake tailscaled LocalAPI on a Unix socket, for the UI audits (rynasd --tailscale-socket).

Serves GET /localapi/v0/status and GET /localapi/v0/prefs (the only calls rynasd makes; every
change goes through the CLI, which rynasd --dry-system only logs). Usage:
faketailscale.py <socket> <mode>, mode `running` (connected, a subnet waiting for approval,
an update available), `login` (logged out, a login address shown) or `expiring` (running,
key expiring in 5 days). Nothing here talks to a real tailscaled.
"""
import asyncio, json, os, sys, time

SOCK, MODE = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else "running")


def iso(ts):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


def status():
    if MODE == "login":
        return {"Version": "1.90.6-tfake", "BackendState": "NeedsLogin",
                "AuthURL": "https://login.tailscale.com/a/1a2b3c4d5e6f",
                "TailscaleIPs": None, "Self": {"Online": False}, "Peer": None,
                "CurrentTailnet": None, "Health": [], "ClientVersion": {"RunningLatest": True}}
    expiry = time.time() + (5 if MODE == "expiring" else 150) * 86400
    peers = {f"nodekey:{i}": {"HostName": f"dev{i}", "Online": i % 3 != 0} for i in range(7)}
    return {
        "Version": "1.90.6-tfake", "BackendState": "Running", "AuthURL": "",
        "TailscaleIPs": ["100.101.102.103", "fd7a:115c:a1e0:ab12:4843:cd96:6265:6667"],
        "Self": {"HostName": "nas", "DNSName": "nas.tail1234.ts.net.", "Online": True,
                 "KeyExpiry": iso(expiry),
                 "AllowedIPs": ["100.101.102.103/32", "fd7a:115c:a1e0:ab12:4843:cd96:6265:6667/128"],
                 "PrimaryRoutes": []},
        "Health": [],
        "CurrentTailnet": {"Name": "family@example.com", "MagicDNSSuffix": "tail1234.ts.net", "MagicDNSEnabled": True},
        "Peer": peers,
        "ClientVersion": {"RunningLatest": False, "LatestVersion": "1.92.1"},
    }


def prefs():
    routes = ["192.168.1.0/24"] if MODE != "login" else None
    return {"ControlURL": "https://controlplane.tailscale.com", "RouteAll": False, "CorpDNS": False,
            "RunSSH": False, "WantRunning": MODE != "login", "LoggedOut": MODE == "login",
            "AdvertiseRoutes": routes}


async def handle(r, w):
    try:
        head = await r.readuntil(b"\r\n\r\n")
        line = head.split(b"\r\n", 1)[0].decode()
        path = line.split(" ")[1] if " " in line else ""
        if path == "/localapi/v0/status":
            body, code = json.dumps(status()), "200 OK"
        elif path == "/localapi/v0/prefs":
            body, code = json.dumps(prefs()), "200 OK"
        else:
            body, code = '{"error":"not found"}', "404 Not Found"
        w.write(f"HTTP/1.0 {code}\r\nContent-Type: application/json\r\n\r\n{body}".encode())
        await w.drain()
    except Exception as e:  # noqa: BLE001
        print("fake tailscaled:", e, flush=True)
    finally:
        w.close()


async def main():
    if os.path.exists(SOCK):
        os.unlink(SOCK)
    srv = await asyncio.start_unix_server(handle, path=SOCK)
    print(f"fake tailscaled ({MODE}) on {SOCK}", flush=True)
    async with srv:
        await srv.serve_forever()


asyncio.run(main())
