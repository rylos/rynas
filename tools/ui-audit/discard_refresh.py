"""Discard of the pending changes: the page shows the applied state again without F5.

A change is staged through the API (a new snapshot job; the removal of an applied user), the
page is opened and shows it ("Pending: new" / "Pending: removal", the struck-through name),
then the pending bar's Discard is pressed and confirmed. Within a few seconds, without a
reload, the page must show what is applied: no job row, no pending badge on the user.
The bug: a discard is no task, so no `config.*` task event bumped the configuration
generation and the pages kept the staged state until F5.

    python3 -I tools/ui-audit/discard_refresh.py      # BASE=http://127.0.0.1:18600 by default

Needs a running `start.sh` instance (it stages and discards the user `tester`'s changes and
creates the user `alice` in the dry root). Exit 1 on a failure.
"""
import asyncio, http.cookiejar, json, os, sys, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
sys.path.insert(0, HERE)
import cdp  # noqa: E402
import run_all  # noqa: E402

cdp.S = OUT_DIR
cdp.SHOTS = os.path.join(OUT_DIR, "shots")
cdp.PORT = int(os.environ.get("CDP_PORT", "9398"))
BASE = run_all.BASE
PENDING = "(() => /Pending: |In attesa: /.test(document.querySelector('.page').innerText))()"
FAILS = []


class Api:
    """The test account's own session (the changeset is per user, shared with the browser's)."""

    def __init__(self):
        self.op = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
        self.csrf = self.call("POST", "/auth/login", {"username": "tester", "password": "correct-horse"})["csrf_token"]

    def call(self, method, path, body=None):
        req = urllib.request.Request(BASE + "/api/v1" + path, method=method, data=None if body is None else json.dumps(body).encode())
        req.add_header("Content-Type", "application/json")
        if getattr(self, "csrf", None):
            req.add_header("X-CSRF-Token", self.csrf)
        try:
            with self.op.open(req) as r:
                txt = r.read().decode()
        except urllib.error.HTTPError as e:
            txt = e.read().decode()
        return json.loads(txt) if txt else None

    def wait_tasks(self):
        for _ in range(50):
            if not any(t.get("status") == "running" for t in self.call("GET", "/tasks") or []):
                return
            import time
            time.sleep(0.2)


def check(cond, msg):
    print(("  ok   " if cond else "  FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


async def discard(p):
    """Discard in the pending bar, then the confirm dialog's button."""
    await p.wait("document.querySelector('.pending .pending-actions button')", what="pending bar")
    await p.js("document.querySelector('.pending .pending-actions button').click()")
    await p.wait("document.querySelectorAll('.dialog-foot button').length >= 2", what="confirm")
    await p.js("document.querySelector('.dialog-foot button:not([data-autofocus])').click()")


async def main():
    api = Api()
    api.call("POST", "/config/discard")
    # an applied user to remove (the dry root keeps the managed state)
    if not any(u["user"]["name"] == "alice" for u in api.call("GET", "/users")):
        api.call("POST", "/users", {"name": "alice", "password": "Sup3r-secret-pw!"})
        api.call("POST", "/config/apply", {"comment": "", "keys": []})
        api.wait_tasks()
    br = cdp.start_browser()
    try:
        p = await cdp.connect()
        await p.send("Emulation.setDeviceMetricsOverride", width=1280, height=900, deviceScaleFactor=1, mobile=False)

        # 1. snapshots, tab "Snapshot jobs": a staged job, discarded
        api.call("POST", "/snapshots/jobs", {"name": "discardme", "subvolume": "/volume1/data", "retention": {"preset": "standard"}})
        await run_all.login(p)
        await p.goto(BASE + "/snapshots")
        await p.wait("document.querySelectorAll('.page [role=tab]').length > 1", what="tabs")
        await p.js("document.querySelectorAll('.page [role=tab]')[1].click()")
        await p.wait("document.querySelector('.page').innerText.includes('discardme')", what="job row")
        check(await p.js(PENDING), "snapshots: the staged job shows as pending")
        await discard(p)
        await asyncio.sleep(3)
        gone = await p.js("!document.querySelector('.page').innerText.includes('discardme')")
        check(gone, "snapshots: after the discard the staged job is gone without a reload")
        if not gone:
            await p.shot("discard_snapshots")

        # 2. users: the removal of an applied user, discarded
        api.call("DELETE", "/users/alice")
        await p.goto(BASE + "/users")
        await p.wait("document.querySelector('.page').innerText.includes('alice')", what="user row")
        check(await p.js(PENDING), "users: the staged removal shows as pending")
        await discard(p)
        await asyncio.sleep(3)
        stale = await p.js(PENDING)
        check(not stale, "users: after the discard no pending badge without a reload")
        if stale:
            await p.shot("discard_users")
    finally:
        br.terminate()
        api.call("POST", "/config/discard")
    print("discard refresh:", "FAIL" if FAILS else "ok")
    sys.exit(1 if FAILS else 0)


asyncio.run(main())
