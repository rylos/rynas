#!/usr/bin/env python3
"""A tiny stand-in for the files API, for screenshots of the file manager before its backend exists.

    files_mock.py <listen-port> <rynas-web-port> [empty|error]

Listens on 127.0.0.1:<listen-port>: `/api/v1/files/<route>` answers from fake data with SynoFile's
JSON shapes (init, shares, list, stat, jobs, prefs, conns); everything else is forwarded to
rynas-web on <rynas-web-port> (login, session, the UI files). Mode `empty`: no shared folders;
mode `error`: every files route answers 503 `backend_unavailable` (the "unavailable" banner).
Test only: no file is read or written.
"""
import http.client
import http.server
import json
import sys
import time
import urllib.parse

LISTEN, UPSTREAM = int(sys.argv[1]), int(sys.argv[2])
MODE = sys.argv[3] if len(sys.argv) > 3 else "full"
NOW = int(time.time())

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
        return {"jobs": [], "now": NOW}
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
