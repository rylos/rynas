#!/usr/bin/env python3
"""A fake admin socket of the web access proxy (rynas-caddy) for the UI audits.

Only the two calls rynasd makes: `GET /config/` (the running configuration, empty until a
load) and `POST /load` (stores the body; `refuse` as second argument answers 400 with
Caddy's `{"error": ...}` shape, for the "last load failed" state). Never runs Caddy and
never listens on a network port.

    fakecaddy.py <socket path> [refuse]
"""
import http.server
import json
import os
import socketserver
import sys

SOCK = sys.argv[1]
REFUSE = len(sys.argv) > 2 and sys.argv[2] == "refuse"
running = {"cfg": b""}


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def address_string(self):
        return "unix"

    def answer(self, status, body=b""):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.rstrip("/") == "/config":
            self.answer(200, running["cfg"])
        else:
            self.answer(404, b'{"error":"not found"}')

    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(n)
        if self.path != "/load":
            self.answer(404, b'{"error":"not found"}')
            return
        if REFUSE:
            self.answer(400, json.dumps({"error": "loading new config: http app module: start: listening on :443: bind: address already in use"}).encode())
            return
        try:
            json.loads(body or b"null")
        except ValueError as e:
            self.answer(400, json.dumps({"error": f"invalid JSON: {e}"}).encode())
            return
        running["cfg"] = body
        self.answer(200)


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


if os.path.exists(SOCK):
    os.unlink(SOCK)
with Server(SOCK, Handler) as srv:
    os.chmod(SOCK, 0o600)
    srv.serve_forever()
