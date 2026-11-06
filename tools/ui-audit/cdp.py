"""Minimal CDP driver for the rynas login/session checks (chromium headless)."""
import asyncio, json, subprocess, sys, time, urllib.request, base64, os

import websockets

S = os.environ.get("AUDIT_OUT", os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "target", "ui-audit"))
SHOTS = os.path.join(S, "shots")
PORT = 9333


class Page:
    def __init__(self, ws):
        self.ws, self.n, self.pending, self.events = ws, 0, {}, []
        # server-sent events seen after Network.enable: (time, event name, data)
        self.sse = []

    async def reader(self):
        async for msg in self.ws:
            m = json.loads(msg)
            if "id" in m and m["id"] in self.pending:
                self.pending.pop(m["id"]).set_result(m)
            elif m.get("method") == "Runtime.consoleAPICalled":
                args = " ".join(str(a.get("value", a.get("description", ""))) for a in m["params"]["args"])
                self.events.append(f"console.{m['params']['type']}: {args}")
            elif m.get("method") == "Runtime.exceptionThrown":
                self.events.append("exception: " + json.dumps(m["params"]["exceptionDetails"])[:400])
            elif m.get("method") == "Network.eventSourceMessageReceived":
                e = m["params"]
                self.sse.append((time.time(), e.get("eventName", ""), e.get("data", "")))
            elif m.get("method") == "Log.entryAdded":
                e = m["params"]["entry"]
                self.events.append(f"log.{e['level']}: {e['text']} {e.get('url','')}")

    async def send(self, method, **params):
        self.n += 1
        fut = asyncio.get_event_loop().create_future()
        self.pending[self.n] = fut
        await self.ws.send(json.dumps({"id": self.n, "method": method, "params": params}))
        r = await asyncio.wait_for(fut, 30)
        if "error" in r:
            raise RuntimeError(f"{method}: {r['error']}")
        return r.get("result", {})

    async def js(self, expr):
        r = await self.send("Runtime.evaluate", expression=expr, returnByValue=True, awaitPromise=True)
        if "exceptionDetails" in r:
            raise RuntimeError(f"js error: {r['exceptionDetails']}")
        return r["result"].get("value")

    async def wait(self, expr, timeout=10, what=None):
        t0 = time.time()
        while time.time() - t0 < timeout:
            try:
                if await self.js(f'!!({expr})'):
                    return True
            except RuntimeError:
                pass
            await asyncio.sleep(0.1)
        raise TimeoutError(what or expr)

    async def shot(self, name):
        await asyncio.sleep(0.45)  # let entry animations settle
        r = await self.send("Page.captureScreenshot", format="png")
        p = os.path.join(SHOTS, name + ".png")
        with open(p, "wb") as f:
            f.write(base64.b64decode(r["data"]))
        print("  shot", p)

    async def fullshot(self, name):
        await asyncio.sleep(0.45)
        m = await self.send("Page.getLayoutMetrics")
        h = int(m["cssContentSize"]["height"]); w = int(m["cssLayoutViewport"]["clientWidth"])
        r = await self.send("Page.captureScreenshot", format="png", captureBeyondViewport=True,
                            clip={"x": 0, "y": 0, "width": w, "height": min(h, 6000), "scale": 1})
        p = os.path.join(SHOTS, name + ".png")
        with open(p, "wb") as f:
            f.write(base64.b64decode(r["data"]))
        print("  shot", p)

    async def goto(self, url):
        await self.send("Page.navigate", url=url)
        await asyncio.sleep(0.3)
        await self.wait("document.readyState === 'complete'")

    async def type_into(self, selector, text, clear=True):
        await self.js(f"(() => {{ const e = document.querySelector({json.dumps(selector)}); e.focus(); {'e.select();' if clear else ''} }})()")
        if clear:
            await self.send("Input.dispatchKeyEvent", type="keyDown", key="Backspace", code="Backspace", windowsVirtualKeyCode=8)
            await self.send("Input.dispatchKeyEvent", type="keyUp", key="Backspace", code="Backspace", windowsVirtualKeyCode=8)
        await self.send("Input.insertText", text=text)

    async def enter(self):
        await self.send("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13, text="\r")
        await self.send("Input.dispatchKeyEvent", type="keyUp", key="Enter", code="Enter", windowsVirtualKeyCode=13)

    async def click(self, selector):
        ok = await self.js(f"(() => {{ const e = document.querySelector({json.dumps(selector)}); if (!e) return false; e.click(); return true; }})()")
        if not ok:
            raise RuntimeError("no element " + selector)

    async def click_text(self, selector, text):
        ok = await self.js(
            f"(() => {{ const e = [...document.querySelectorAll({json.dumps(selector)})].find(x => x.textContent.includes({json.dumps(text)})); if (!e) return false; e.click(); return true; }})()"
        )
        if not ok:
            raise RuntimeError(f"no element {selector} with {text}")

    async def text(self, selector):
        return await self.js(f"(document.querySelector({json.dumps(selector)})||{{}}).textContent")


def start_browser():
    prof = os.path.join(S, f"chrome-profile-{PORT}")
    p = subprocess.Popen(
        ["chromium", "--headless=new", f"--remote-debugging-port={PORT}", f"--user-data-dir={prof}",
         "--window-size=1280,860", "--no-first-run", "--no-default-browser-check", "--lang=en-US",
         "--disable-gpu", "about:blank"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json/version").read()
            break
        except Exception:
            time.sleep(0.1)
    return p


async def connect():
    targets = json.loads(urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json/list").read())
    t = next(t for t in targets if t["type"] == "page")
    ws = await websockets.connect(t["webSocketDebuggerUrl"], max_size=50_000_000)
    pg = Page(ws)
    asyncio.get_event_loop().create_task(pg.reader())
    for d in ("Page", "Runtime", "Log"):
        await pg.send(f"{d}.enable")
    await pg.send("Emulation.setDeviceMetricsOverride", width=1280, height=860, deviceScaleFactor=1, mobile=False)
    return pg
