"""Tiny fake NUT upsd for the UI audits: answers LIST UPS, LIST VAR, LIST CMD and, after
USERNAME/PASSWORD, INSTCMD on localhost.

    python3 -I tools/ui-audit/fake_upsd.py 13493 [ol|ob|lb|none] [all|quick|none]

Status: `ol` (default): on mains, charging; `ob`: on battery, discharging (charge and runtime
fall a little at every reading); `lb`: on battery with low battery; `none`: upsd without any UPS.
The load (and the mains voltage of `ol`) drifts every 5 s as on a real unit, so rynas-web pushes
`ups.status` at every poll (the UPS page's idle redraw check).
Battery test commands offered (`LIST CMD`): `all` (default: quick, deep, stop), `quick` (quick
and stop), `none`. INSTCMD of an offered test answers OK (any user and password) and moves
`ups.test.result` to "In progress", then to "Done and passed" after 20 s (`stop`: "Aborted").
Only binds 127.0.0.1; everything else answers ERR.
"""
import socketserver
import sys
import threading
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 13493
MODE = sys.argv[2] if len(sys.argv) > 2 else "ol"
CMDS = sys.argv[3] if len(sys.argv) > 3 else "all"
START = time.time()
TESTS = {
    "all": ["test.battery.start.deep", "test.battery.start.quick", "test.battery.stop"],
    "quick": ["test.battery.start.quick", "test.battery.stop"],
    "none": [],
}[CMDS]
OTHER = ["beeper.disable", "beeper.enable", "load.off"]
LOCK = threading.Lock()
TEST = {"result": "Done and passed", "until": 0.0}


def test_result():
    with LOCK:
        if TEST["until"] and time.time() >= TEST["until"]:
            TEST["result"], TEST["until"] = "Done and passed", 0.0
        return TEST["result"]


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
        # mains voltage and load drift like a real unit: a new reading every 5 s poll
        "input.voltage": "0.0" if MODE != "ol" else f"{228 + int(t / 5) % 4}.0",
        "output.voltage": "230.0",
        "ups.beeper.status": "enabled",
        "ups.load": str(16 + int(t / 5) % 3),
        "ups.status": status,
        "ups.test.result": test_result(),
    }


def instcmd(cmd):
    if cmd not in TESTS:
        return "ERR CMD-NOT-SUPPORTED\n"
    with LOCK:
        if cmd == "test.battery.stop":
            TEST["result"], TEST["until"] = "Aborted", 0.0
        else:
            TEST["result"], TEST["until"] = "In progress", time.time() + 20
    return "OK\n"


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        user = password = None
        for raw in self.rfile:
            line = raw.decode("utf-8", "replace").strip()
            words = line.split()
            have_ups = MODE != "none"
            if line == "LIST UPS":
                body = 'UPS ups "fake UPS"\n' if have_ups else ""
                out = f"BEGIN LIST UPS\n{body}END LIST UPS\n"
            elif line == "LIST VAR ups" and have_ups:
                rows = "".join(f'VAR ups {k} "{v}"\n' for k, v in variables().items())
                out = f"BEGIN LIST VAR ups\n{rows}END LIST VAR ups\n"
            elif line == "LIST CMD ups" and have_ups:
                rows = "".join(f"CMD ups {c}\n" for c in sorted(OTHER + TESTS))
                out = f"BEGIN LIST CMD ups\n{rows}END LIST CMD ups\n"
            elif len(words) == 2 and words[0] == "USERNAME":
                user, out = words[1], "OK\n"
            elif len(words) == 2 and words[0] == "PASSWORD":
                password, out = words[1], "OK\n"
            elif len(words) == 3 and words[0] == "INSTCMD" and words[1] == "ups" and have_ups:
                if not user:
                    out = "ERR USERNAME-REQUIRED\n"
                elif not password:
                    out = "ERR PASSWORD-REQUIRED\n"
                else:
                    out = instcmd(words[2])
            elif line == "LOGOUT":
                self.wfile.write(b"OK Goodbye\n")
                return
            elif line.startswith(("LIST VAR", "LIST CMD", "INSTCMD")):
                out = "ERR UNKNOWN-UPS\n"
            else:
                out = "ERR UNKNOWN-COMMAND\n"
            self.wfile.write(out.encode())


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    with Server(("127.0.0.1", PORT), Handler) as s:
        print(f"fake upsd on 127.0.0.1:{PORT} ({MODE}, tests: {CMDS})", flush=True)
        s.serve_forever()
