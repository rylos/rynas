"""Tiny fake NUT upsd for the UI audits: answers LIST UPS and LIST VAR on localhost.

    python3 -I tools/ui-audit/fake_upsd.py 13493 [ol|ob|lb|none]

`ol` (default): on mains, charging; `ob`: on battery, discharging (charge and runtime fall a
little at every reading); `lb`: on battery with low battery; `none`: upsd without any UPS.
Only binds 127.0.0.1; read commands only, everything else answers ERR.
"""
import socketserver
import sys
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 13493
MODE = sys.argv[2] if len(sys.argv) > 2 else "ol"
START = time.time()


def variables():
    t = time.time() - START
    if MODE == "ol":
        status, charge, runtime = "OL CHRG", min(100.0, 92 + t / 30), 2400
    elif MODE == "ob":
        status, charge, runtime = "OB DISCHRG", max(30.0, 88 - t / 10), max(600, 2100 - int(t * 3))
    else:
        status, charge, runtime = "OB DISCHRG LB", 9.0, 180
    return {
        "battery.charge": f"{charge:.0f}",
        "battery.runtime": str(runtime),
        "battery.mfr.date": "2024/03/18",
        "device.mfr": "American Power Conversion",
        "device.model": "Back-UPS ES 700G",
        "device.serial": "5B1234X56789",
        "driver.name": "usbhid-ups",
        "input.voltage": "0.0" if MODE != "ol" else "229.0",
        "output.voltage": "230.0",
        "ups.beeper.status": "enabled",
        "ups.load": "18",
        "ups.status": status,
        "ups.test.result": "Done and passed",
    }


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        for raw in self.rfile:
            line = raw.decode("utf-8", "replace").strip()
            if line == "LIST UPS":
                body = "" if MODE == "none" else 'UPS ups "fake UPS"\n'
                out = f"BEGIN LIST UPS\n{body}END LIST UPS\n"
            elif line == "LIST VAR ups" and MODE != "none":
                rows = "".join(f'VAR ups {k} "{v}"\n' for k, v in variables().items())
                out = f"BEGIN LIST VAR ups\n{rows}END LIST VAR ups\n"
            elif line == "LOGOUT":
                self.wfile.write(b"OK Goodbye\n")
                return
            else:
                out = "ERR UNKNOWN-COMMAND\n" if not line.startswith("LIST VAR") else "ERR UNKNOWN-UPS\n"
            self.wfile.write(out.encode())


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    with Server(("127.0.0.1", PORT), Handler) as s:
        print(f"fake upsd on 127.0.0.1:{PORT} ({MODE})", flush=True)
        s.serve_forever()
