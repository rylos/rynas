#!/usr/bin/env python3
"""A tiny stand-in for the files API, for screenshots of the file manager before its backend exists.

    files_mock.py <listen-port> <rynas-web-port> [empty|error|xfer|denied]

Listens on 127.0.0.1:<listen-port>: `/api/v1/files/<route>` answers from fake data with SynoFile's
JSON shapes (init, shares, list, stat, jobs, prefs, conns); everything else is forwarded to
rynas-web on <rynas-web-port> (login, session, the UI files). Mode `empty`: no shared folders;
mode `error`: every files route answers 503 `backend_unavailable` (the "unavailable" banner);
mode `xfer`: one running tar.zst compression with bursty speed samples (a pause included), for
the transfer dialog's speed chart (open it with `/files?xfer=last`); mode `denied`: a finished copy
whose only item was refused (`permission_denied`), for the error list and summary.
Test only: no file is read or written.
"""
import http.client
import http.server
import json
import random
import sys
import time
import urllib.parse

LISTEN, UPSTREAM = int(sys.argv[1]), int(sys.argv[2])
MODE = sys.argv[3] if len(sys.argv) > 3 else "full"
NOW = int(time.time())
STARTED = time.time() - 75  # the compression has been running for 75 s when the mock starts
MIB = 1 << 20
XFER_TOTAL = 20 * 1024 * MIB


def xfer_samples(upto_ms):
    """Samples [ms since start, bytes, files, verify bytes] as the files worker writes them: one
    every 250 ms, plus forced ones at file boundaries a few ms apart; zstd reads in bursts (big
    blocks, then almost nothing while it compresses), so the instantaneous speed jumps between ~0
    and ~300 MiB/s around a ~95 MiB/s average; paused from 40 s to 46 s (no samples)."""
    rnd = random.Random(7)
    out, t, b, f = [[0, 0, 0, 0]], 0, 0, 0
    while True:
        t += 250
        if 40_000 < t <= 46_000:
            continue
        if t > upto_ms:
            break
        burst = rnd.random() < 0.45
        b += int((rnd.uniform(150, 260) if burst else rnd.uniform(0, 12)) * MIB / 4)
        out.append([t, b, f, 0])
        if rnd.random() < 0.2:  # a file ends: a forced sample right after
            f += 1
            b += int(rnd.uniform(4, 40) * MIB)
            out.append([t + rnd.randint(5, 40), b, f, 0])
    return out


def xfer_job():
    # it keeps running for a minute after the mock starts, then holds (still "running")
    ms = int(min(time.time() - STARTED, 135) * 1000)
    s = xfer_samples(ms)
    done = min(s[-1][1], XFER_TOTAL)
    return {
        "id": 4242, "op": "compress", "state": "running", "source": "nas", "created": NOW - 75, "started": int(STARTED * 1000), "verify": False,
        "info": {"sources": ["/documents/Projects"], "dest": "/documents/Projects.tar.zst"},
        "progress": {"bytes_done": done, "bytes_total": XFER_TOTAL, "files_done": s[-1][2], "files_total": 4100,
                     "dirs": 120, "errors": 0, "verify_bytes": 0, "scanning": False, "phase": "compress", "method": "zstd",
                     "current": "/documents/Projects/render/frames.bin", "current_size": 2 << 30, "current_done": done % (2 << 30)},
        "samples": s, "active_ms": ms - 6000, "errors": [], "msg": "",
    }

def denied_job():
    """A copy into a shared folder the user may not write to: every item refused (mode `denied`)."""
    return {
        "id": 4343, "op": "copy", "state": "done", "source": "nas", "created": NOW - 30, "started": NOW - 30, "finished": NOW - 30,
        "verify": False, "info": {"sources": ["/documents/annual report.pdf"], "dest": "/sistema/reports"},
        "progress": {"bytes_done": 2_480_000, "bytes_total": 2_480_000, "files_done": 1, "files_total": 1, "dirs": 0, "errors": 1,
                     "skipped": 0, "verify_bytes": 0, "scanning": False, "phase": "end", "method": "copy", "current": "",
                     "current_size": 0, "current_done": 0},
        "samples": [[0, 0, 0, 0], [40, 2_480_000, 1, 0]], "active_ms": 40,
        "errors": [{"path": "reports/annual report.pdf", "msg": "you are not allowed to write to the folder “reports”", "code": "permission_denied"}],
        "msg": "complete: 1 errors", "code": "",
    }


SHARES = [] if MODE == "empty" else [
    {"name": "documents", "volume": "volume1", "comment": "Office documents", "recycle": True, "hidden": False, "encrypted": False, "home": False,
     "free": 1_600_000_000_000, "total": 3_900_000_000_000, "readable": True, "writable": True},
    {"name": "photos", "volume": "volume1", "comment": "", "recycle": True, "hidden": False, "encrypted": False, "home": False,
     "free": 1_600_000_000_000, "total": 3_900_000_000_000, "readable": True, "writable": True},
    {"name": "homes", "volume": "volume1", "comment": "", "recycle": False, "hidden": False, "encrypted": False, "home": True,
     "free": 1_600_000_000_000, "total": 3_900_000_000_000, "readable": True, "writable": True},
    {"name": "usbshare1", "volume": "volumeUSB1", "comment": "", "recycle": False, "hidden": False, "encrypted": False, "home": False,
     "removable": True, "free": 20_000_000_000, "total": 64_000_000_000, "readable": True, "writable": True},
]


def entry(name, kind="f", size=0, age=3600, **extra):
    return {"name": name, "kind": kind, "size": size, "mtime": NOW - age, "mode": 0o40755 if kind == "d" else 0o100644, **extra}


FOLDERS = {
    "/documents": [
        entry("#recycle", "d", full=True), entry("Contracts", "d", age=86400 * 3), entry("Invoices 2026", "d", age=7200),
        entry("Projects", "d", age=86400 * 40), entry("annual report.pdf", size=2_480_000, age=600),
        entry("budget.xlsx", size=88_000, age=86400), entry("notes.md", size=4_200, age=120),
        entry("a very long file name that should be truncated with an ellipsis and never widen the pane.txt", size=12_000),
        entry("backup.zip", size=734_000_000, age=86400 * 9), entry("logo.svg", size=7_100, age=86400 * 2),
        entry(".hidden-config", size=310),
    ],
    "/photos": [entry("2025", "d"), entry("2026", "d"), entry("beach.jpg", size=4_100_000, thumb=True), entry("IMG_0001.HEIC", size=2_900_000)],
}


def listing(path):
    if path in FOLDERS:
        return {"entries": FOLDERS[path], "writable": True, "space": {"free": 1_600_000_000_000, "total": 3_900_000_000_000}, "recycle": True}
    return {"entries": [], "writable": True, "space": None, "recycle": True}


def shares_info():
    return {"shares": SHARES, "user": "tester", "volumes": {"volume1": "Volume 1"}, "hostname": "rynas-test"}


def route(name, q):
    path = q.get("path", [""])[0]
    if name == "init":
        return {"whoami": {"user": "tester", "version": "0.1.0", "setup": True, "daemon": True}, "prefs": {},
                "shares": shares_info(), "conns": {"conns": [], "key": None}}
    if name == "whoami":
        return {"user": "tester", "version": "0.1.0", "setup": True, "daemon": True}
    if name == "shares":
        return shares_info()
    if name == "list":
        return listing(path)
    if name == "stat":
        base = path.rsplit("/", 1)[-1]
        return {**entry(base, size=1000), "owner": "tester", "group": "users"}
    if name == "jobs":
        return {"jobs": [xfer_job()] if MODE == "xfer" else [denied_job()] if MODE == "denied" else [], "now": int(time.time() * 1000)}
    if name in ("prefs", "conns"):
        return {} if name == "prefs" else {"conns": [], "key": None}
    if name in ("usage", "media"):
        return {}
    return None


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def answer(self, status, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def handle_any(self):
        u = urllib.parse.urlsplit(self.path)
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        if u.path.startswith("/api/v1/files/"):
            if "rynas_session" not in (self.headers.get("Cookie") or "") and "session" not in (self.headers.get("Cookie") or ""):
                return self.answer(401, {"code": "unauthenticated", "message": "no session"})
            if MODE == "error":
                return self.answer(503, {"code": "backend_unavailable", "message": "files worker down (mock)"})
            out = route(u.path[len("/api/v1/files/"):], urllib.parse.parse_qs(u.query))
            if out is None:
                return self.answer(404, {"code": "not_found", "message": "not in the mock"})
            return self.answer(200, out)
        c = http.client.HTTPConnection("127.0.0.1", UPSTREAM, timeout=30)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "connection")}
        c.request(self.command, self.path, body=body, headers=headers)
        r = c.getresponse()
        data = r.read()
        self.send_response(r.status)
        for k, v in r.getheaders():
            if k.lower() not in ("transfer-encoding", "connection", "content-length"):
                self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = handle_any


http.server.ThreadingHTTPServer(("127.0.0.1", LISTEN), Handler).serve_forever()
