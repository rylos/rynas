#!/usr/bin/env python3
"""How fast the built-in UI starts and changes page: repeatable numbers, before and after a change.

    tools/ui-audit/perf_stage.sh crates/rynas-ui/dist target/ui-audit/perf/stage-base   # as the .deb
    UI_DIR=target/ui-audit/perf/stage-base FAKE_DOCKER=1 FAKE_CADDY=1 FAKE_UPS=ol tools/ui-audit/start.sh
    tools/ui-audit/files_mock.py 18601 18600 &        # the Files pass goes through it (no worker here)
    AUDIT_USER=$(id -un) python3 -I tools/ui-audit/perf.py base [--scenarios lan,cpu4,phone] [--no-files] [--lang it]
    python3 -I tools/ui-audit/perf.py --compare base after        # the two runs as one table

Results: target/ui-audit/perf/<label>.json. What it does, per scenario, in a browser of its own:
login screen cold and warm (cache cleared / kept, 3 warm loads, median), sign-in, dashboard cold
and warm, every main page by client-side navigation (first visit and revisit), the Files page
(script loaded, first folder listed), then every endpoint seen, directly with one keep-alive
connection (the server's own latency, no browser).

The site is opened as http://perf.test:<port> (the browser resolves it to loopback): a plain-HTTP
LAN name, so the browser behaves as on a NAS reached by address. On `localhost` it would
advertise brotli, which it does not do over plain HTTP elsewhere.

Scenarios (`CDP throttling` slows only the page's main thread, so the browser is also pinned to
two logical CPUs of different cores when throttled: V8's background compile threads are limited too):
  lan    no throttling              desktop on the LAN (this machine's speed)
  cpu4   CPU 4x, 100 Mbit/s, 2 ms   weak NAS-class laptop or tablet on Wi-Fi
  phone  CPU 6x, 20 Mbit/s, 40 ms   a phone on a weak link

Timings are in the page's clock (ms from navigation start), from a rAF loop started before any
script: `first useful paint` is the frame in which the screen has its content (login form, or
app bar and page without any `.skeleton`).
"""
import argparse, asyncio, http.client, json, os, re, statistics, subprocess, sys, time, urllib.parse, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
OUT = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
PERF = os.path.join(OUT, "perf")
os.makedirs(PERF, exist_ok=True)
sys.path.insert(0, HERE)
import websockets  # noqa: E402

import cdp  # noqa: E402

PORT = int(os.environ.get("PORT", "18600"))
FILES_PORT = int(os.environ.get("FILES_PORT", "18601"))
HOST = os.environ.get("PERF_HOST", "perf.test")
USER = os.environ.get("AUDIT_USER", "tester")
ROUTES = ["/storage", "/storage/disks", "/shares", "/snapshots", "/backup", "/users", "/services", "/docker", "/apps",
          "/network", "/network/web-access", "/network/remote-access", "/hardware", "/system", "/system/configuration",
          "/system/notifications", "/system/scheduler", "/system/updates", "/notifications", "/security", "/"]
SCENARIOS = {
    "lan": {"cpu": 1, "net": None, "pin": None},
    "cpu4": {"cpu": 4, "net": (2, 12_500_000), "pin": "0,2"},
    "phone": {"cpu": 6, "net": (40, 2_500_000), "pin": "0,2"},
}
WARM = 3

BOOT = r"""
(() => {
  const P = window.__perf = { marks: {}, long: [], wasm: {}, inflight: 0 };
  const realFetch = window.fetch;
  window.fetch = function (...a) {
    P.inflight++;
    const p = realFetch.apply(this, a);
    const done = () => { P.inflight--; };
    p.then(done, done);
    return p;
  };
  const mark = (k) => { if (!(k in P.marks)) P.marks[k] = performance.now(); };
  P.mark = mark;
  try {
    new PerformanceObserver((l) => { for (const e of l.getEntries()) P.long.push([e.startTime, e.duration]); })
      .observe({ type: "longtask", buffered: true });
  } catch (e) {}
  const wrap = (name) => {
    const f = WebAssembly[name];
    if (!f) return;
    WebAssembly[name] = function (...a) {
      const t0 = performance.now();
      return f.apply(this, a).then((r) => {
        if (!P.wasm.ready) {
          P.wasm = { via: name, start: t0, ready: performance.now() };
          const inst = r.instance || r;
          P.wasm.memory = () => inst.exports.memory.buffer.byteLength;
        }
        return r;
      });
    };
  };
  wrap("instantiateStreaming");
  wrap("instantiate");
  const tick = () => {
    if (document.querySelector(".login-card input")) mark("login");
    if (document.querySelector(".appbar")) mark("shell");
    if (document.querySelector(".appbar") && document.querySelector(".page") && !document.querySelector(".skeleton")) mark("page");
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
})();
"""

COLLECT = r"""
(() => {
  const P = window.__perf, nav = performance.getEntriesByType("navigation")[0];
  const res = performance.getEntriesByType("resource");
  const pick = (re) => res.filter((r) => re.test(r.name)).map((r) => ({ name: new URL(r.name).pathname, start: r.startTime, ttfb: r.responseStart, end: r.responseEnd, enc: r.encodedBodySize, dec: r.decodedBodySize, tr: r.transferSize, proto: r.nextHopProtocol }));
  const paint = performance.getEntriesByName("first-contentful-paint")[0];
  const w = P.wasm;
  return JSON.stringify({
    marks: P.marks, long: P.long, wasm: { via: w.via, start: w.start, ready: w.ready, memory: w.memory ? w.memory() : 0 },
    ttfb: nav.responseStart, dcl: nav.domContentLoadedEventEnd, load: nav.loadEventEnd, fcp: paint ? paint.startTime : null,
    wasmres: pick(/_bg\.wasm/), js: pick(/rynas-ui-[0-9a-f]+\.js/), css: pick(/\.css/), font: pick(/\.woff2/),
    api: pick(/\/api\//), docs: pick(/\/i18n\//), count: res.length,
  });
})()
"""

NAV = r"""
(route, limit) => new Promise((res) => {
  const P = window.__perf, long0 = P.long.length, t0 = performance.now();
  const entries0 = performance.getEntriesByType("resource").length;
  const main = document.querySelector("#main");
  let first = null, quiet = 0;
  const mo = new MutationObserver(() => { if (first === null) first = performance.now() - t0; });
  mo.observe(main, { childList: true, subtree: true, characterData: true });
  history.pushState(null, "", route);
  dispatchEvent(new PopStateEvent("popstate"));
  const end = (ok) => requestAnimationFrame(() => {
    mo.disconnect();
    const api = performance.getEntriesByType("resource").slice(entries0).filter((r) => r.name.includes("/api/"));
    res(JSON.stringify({ ok, first, ready: performance.now() - t0, api: api.map((r) => [new URL(r.name).pathname + new URL(r.name).search, r.startTime - t0, r.responseEnd - t0]), long: P.long.slice(long0) }));
  });
  // ready: the page has changed, nothing is loading (no skeleton, no fetch in flight) for 3 frames
  const tick = () => {
    if (first !== null && P.inflight === 0 && !document.querySelector(".skeleton")) quiet++;
    else quiet = 0;
    if (quiet >= 3) return end(true);
    if (performance.now() - t0 > limit) return end(false);
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
})
"""

FILES = r"""
(limit) => new Promise((res) => {
  const P = window.__perf, long0 = P.long.length, t0 = performance.now();
  history.pushState(null, "", "/files");
  dispatchEvent(new PopStateEvent("popstate"));
  let chrome = null, entries = 0;
  const tick = () => {
    const host = document.querySelector(".rynas-files-host");
    const root = host && host.shadowRoot;
    if (root && root.querySelector(".tabbar") && chrome === null) chrome = performance.now() - t0;
    const rows = root && root.querySelector(".rows");
    if (rows && rows.children.length > 0) entries = rows.children.length;
    if (entries > 0) {
      return requestAnimationFrame(() => {
        const mods = performance.getEntriesByType("resource").filter((r) => /files-app/.test(r.name));
        const bytes = mods.reduce((a, r) => a + r.transferSize, 0);
        res(JSON.stringify({ ok: true, chrome, ready: performance.now() - t0, entries, modules: mods.length, moduleBytes: bytes, last: Math.max(...mods.map((r) => r.responseEnd)) - t0, long: P.long.slice(long0) }));
      });
    }
    if (performance.now() - t0 > limit) return res(JSON.stringify({ ok: false, chrome, ready: limit, entries, long: P.long.slice(long0) }));
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
})
"""


def median(xs):
    xs = [x for x in xs if x is not None]
    return round(statistics.median(xs), 1) if xs else None


def percentile(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p))] if xs else None


def start_browser(name, pin):
    prof = os.path.join(PERF, f"profile-{name}")
    subprocess.run(["rm", "-rf", prof], check=False)
    cmd = ["chromium", "--headless=new", f"--remote-debugging-port={cdp.PORT}", f"--user-data-dir={prof}",
           "--window-size=1280,860", "--no-first-run", "--no-default-browser-check", "--lang=en-US", "--disable-gpu",
           f"--host-resolver-rules=MAP {HOST} 127.0.0.1", "about:blank"]
    if pin:
        cmd = ["taskset", "-c", pin] + cmd
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{cdp.PORT}/json/version").read()
            break
        except Exception:
            time.sleep(0.1)
    return p


async def setup(p, sc):
    await p.send("Network.enable")
    await p.send("Page.addScriptToEvaluateOnNewDocument", source=BOOT)
    if sc["cpu"] > 1:
        await p.send("Emulation.setCPUThrottlingRate", rate=sc["cpu"])
    if sc["net"]:
        lat, tp = sc["net"]
        await p.send("Network.emulateNetworkConditions", offline=False, latency=lat, downloadThroughput=tp, uploadThroughput=tp)


async def load(p, url, ready, timeout=45):
    await p.send("Page.navigate", url=url)
    await asyncio.sleep(0.2)
    await p.wait(ready, timeout=timeout, what=ready)
    await p.js("new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))")
    return json.loads(await p.js(COLLECT))


def wasm_stats(r):
    w = r["wasmres"]
    if not w:
        return {}
    d = w[0]
    return {
        "requests": len(w), "encoded": d["enc"], "decoded": d["dec"], "transfer": d["tr"], "proto": d["proto"],
        "download": round(d["end"] - d["start"], 1),
        "tail_compile": round(r["wasm"]["ready"] - d["end"], 1) if r["wasm"].get("ready") else None,
        "via": r["wasm"].get("via"),
        "ready": r["wasm"].get("ready"),
    }


def summarize_load(name, cold, warm, key):
    def row(r):
        lt = [d for s, d in r["long"] if s <= r["marks"].get(key, 1e12)]
        return {"ttfb": r["ttfb"], "fcp": r["fcp"], "first_useful": r["marks"].get(key), "long_total": sum(lt),
                "long_max": max(lt, default=0), "wasm_download": wasm_stats(r).get("download"), "wasm_ready": r["wasm"].get("ready"),
                "wasm_tail": wasm_stats(r).get("tail_compile"), "api_calls": len(r["api"]), "res": r["count"]}

    w = [row(x) for x in warm]
    out = {"cold": row(cold), "warm": {k: median([x[k] for x in w]) for k in w[0]}, "wasm": wasm_stats(cold), "css": cold["css"][:1], "font": cold["font"][:2],
           "startup_api": [(a["name"], round(a["start"]), round(a["end"])) for a in cold["api"]], "memory": cold["wasm"].get("memory", 0)}
    return out


async def login(p, base):
    await p.send("Page.navigate", url=base + "/")
    await asyncio.sleep(0.2)
    await p.wait("document.querySelector('.login-card input[name=password]') || document.querySelector('.appbar')", timeout=40, what="login or shell")
    if await p.js("!!document.querySelector('.login-card')"):
        await p.type_into("input[name=username]", USER)
        await p.type_into("input[name=password]", "correct-horse")
        await p.enter()
    await p.wait("document.querySelector('.appbar') && !document.querySelector('.login')", timeout=40, what="shell")


async def wait_stable(p):
    """Signed in: let the first load finish, so the next navigations measure themselves."""
    try:
        await p.wait("document.querySelector('.page') && !document.querySelector('.skeleton')", timeout=20)
    except TimeoutError:
        pass
    await asyncio.sleep(0.4)


async def run_scenario(name, sc, args):
    base = f"http://{HOST}:{PORT}"
    cdp.PORT = 9471
    cdp.S = PERF
    cdp.SHOTS = os.path.join(PERF, "shots")
    os.makedirs(cdp.SHOTS, exist_ok=True)
    br = start_browser(name, sc["pin"])
    res = {"scenario": sc}
    try:
        p = await cdp.connect()
        await setup(p, sc)
        if args.lang:
            await p.send("Page.navigate", url=base + "/favicon.svg")
            await asyncio.sleep(0.3)
            await p.js(f"localStorage.setItem('rynas.lang', {json.dumps(args.lang)})")
        await p.send("Network.clearBrowserCookies")
        await p.send("Network.clearBrowserCache")
        ready = "document.querySelector('.login-card input')"
        cold = await load(p, base + "/", ready)
        warm = [await load(p, base + "/", ready) for _ in range(WARM)]
        res["login"] = summarize_load("login", cold, warm, "login")
        await login(p, base)
        await wait_stable(p)
        await p.send("Network.clearBrowserCache")
        ready = "document.querySelector('.appbar') && document.querySelector('.page') && !document.querySelector('.skeleton')"
        cold = await load(p, base + "/", ready)
        await wait_stable(p)
        warm = [await load(p, base + "/", ready) for _ in range(WARM)]
        await wait_stable(p)
        res["dashboard"] = summarize_load("dashboard", cold, warm, "page")
        # the dashboard left alone: main-thread work per second (live charts, animations, events)
        await p.send("Performance.enable")

        async def perf_metrics():
            return {m["name"]: m["value"] for m in (await p.send("Performance.getMetrics"))["metrics"]}

        await asyncio.sleep(2)
        a = await perf_metrics()
        await asyncio.sleep(10)
        b = await perf_metrics()
        res["idle"] = {k: round((b[k] - a[k]) * 1000 / 10, 1) for k in ("TaskDuration", "ScriptDuration", "LayoutDuration", "RecalcStyleDuration")}
        res["idle"].update({k + "PerS": round((b[k] - a[k]) / 10, 1) for k in ("LayoutCount", "RecalcStyleCount", "Frames")})
        heap0 = (await p.send("Runtime.getHeapUsage"))["usedSize"]

        print("  pages...", flush=True)
        routes = {}
        for r in ROUTES:
            routes[r] = []
        for _ in range(2):
            for r in ROUTES:
                nav = f"({NAV})({json.dumps(r)}, 10000)"
                routes[r].append(json.loads(await p.js(nav)))
                await asyncio.sleep(0.25)
        res["routes"] = {r: {"first_visit": x[0]["ready"], "first_paint": x[0]["first"], "revisit": x[1]["ready"], "ok": x[0]["ok"] and x[1]["ok"],
                             "api": len(x[0]["api"]), "long_max": max([d for _, d in x[0]["long"]] or [0]), "calls": [a[0] for a in x[0]["api"]]} for r, x in routes.items()}
        heap1 = (await p.send("Runtime.getHeapUsage"))["usedSize"]
        res["heap"] = {"after_dash": heap0, "after_tour": heap1, "wasm_linear": json.loads(await p.js(COLLECT))["wasm"]["memory"]}
        res["console"] = [e for e in p.events if e.startswith(("exception", "console.error", "log.error", "log.warning"))][:20]

        # endpoints seen, measured from outside the browser on one keep-alive connection
        cookies = await p.send("Network.getCookies", urls=[base])
        cookie = "; ".join(f"{c['name']}={c['value']}" for c in cookies["cookies"])
        seen = sorted({a[0] for x in routes.values() for v in x for a in v["api"]} | {a[0] for a in res["dashboard"]["startup_api"]})
        res["endpoints"] = probe_endpoints(seen, cookie)
        await p.ws.close()
    finally:
        br.terminate()
        br.wait()
    if not args.no_files:
        res["files"] = await run_files(name, sc)
    return res


def probe_endpoints(paths, cookie):
    out = {}
    conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=20)
    for path in paths:
        if "/events" in path or "/stream" in path:
            continue
        times, size, status = [], 0, 0
        for _ in range(12):
            t0 = time.perf_counter()
            try:
                conn.request("GET", path, headers={"Cookie": cookie})
                r = conn.getresponse()
                body = r.read()
            except Exception:
                conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=20)
                break
            times.append((time.perf_counter() - t0) * 1000)
            size, status = len(body), r.status
        if times:
            out[path] = {"p50": round(statistics.median(times), 2), "p95": round(percentile(times, 0.95), 2), "max": round(max(times), 2), "size": size, "status": status}
    return out


async def run_files(name, sc):
    """Through files_mock.py (the dry instance has no worker). Cold, warm, and warm again after a tour of pages."""
    base = f"http://{HOST}:{FILES_PORT}"
    cdp.PORT = 9472
    br = start_browser("files-" + name, sc["pin"])
    out = {}
    try:
        p = await cdp.connect()
        await setup(p, sc)
        await p.send("Network.clearBrowserCookies")
        await login(p, base)
        await wait_stable(p)
        for tag, clear in (("cold", True), ("warm", False)):
            if clear:
                await p.send("Network.clearBrowserCache")
            await p.send("Page.navigate", url=base + "/storage")
            await asyncio.sleep(0.2)
            await p.wait("document.querySelector('.appbar')", timeout=40)
            await wait_stable(p)
            await p.js(f"(document.querySelector('.files-app') || {{}}).id = 'x'")
            r = json.loads(await p.js(f"({FILES})(25000)"))
            r["long_max"] = max([d for _, d in r["long"]] or [0])
            r.pop("long")
            out[tag] = r
            await asyncio.sleep(0.3)
        # leave and come back (one shadow root for the life of the page: should be instant)
        r = json.loads(await p.js(f"({NAV})('/storage', 8000)"))
        await asyncio.sleep(0.3)
        r = json.loads(await p.js(f"({FILES})(10000)"))
        r["long_max"] = max([d for _, d in r["long"]] or [0])
        r.pop("long")
        out["reenter"] = r
        await p.ws.close()
    finally:
        br.terminate()
        br.wait()
    return out


def fmt(x):
    if x is None:
        return "—"
    if isinstance(x, float):
        return f"{x:.0f}"
    return str(x)


def table(data):
    lines = []
    for sn, s in data.items():
        lines.append(f"### {sn} (cpu {s['scenario']['cpu']}x, net {s['scenario']['net']})")
        for page in ("login", "dashboard"):
            q = s[page]
            for tag in ("cold", "warm"):
                r = q[tag]
                lines.append(f"{page:9} {tag:4}: ttfb {fmt(r['ttfb']):>5}  fcp {fmt(r['fcp']):>5}  first-useful {fmt(r['first_useful']):>6}  wasm dl {fmt(r['wasm_download']):>6} ready {fmt(r['wasm_ready']):>6} tail {fmt(r['wasm_tail']):>5}  longtask sum {fmt(r['long_total']):>5} max {fmt(r['long_max']):>5}  api {fmt(r['api_calls']):>3}")
            lines.append(f"  wasm: {s[page]['wasm']}")
        if "routes" in s:
            for r, x in s["routes"].items():
                lines.append(f"  {r:26} first {fmt(x['first_visit']):>6} (paint {fmt(x['first_paint']):>5}) revisit {fmt(x['revisit']):>5}  api {x['api']:>2}  longmax {fmt(x['long_max']):>4} {'' if x['ok'] else 'TIMEOUT'}")
        lines.append(f"  idle dashboard (ms of main thread per s) {s.get('idle')}")
        lines.append(f"  heap {s.get('heap')}")
        lines.append(f"  files {s.get('files')}")
    return "\n".join(lines)


def compare(a, b):
    A = json.load(open(os.path.join(PERF, f"{a}.json")))
    B = json.load(open(os.path.join(PERF, f"{b}.json")))
    rows = []
    for sn in A:
        if sn not in B:
            continue
        for page in ("login", "dashboard"):
            for tag in ("cold", "warm"):
                for k in ("ttfb", "fcp", "first_useful", "wasm_ready", "long_total", "long_max", "api_calls"):
                    rows.append((sn, page, tag, k, A[sn][page][tag][k], B[sn][page][tag][k]))
        for r in A[sn]["routes"]:
            if r in B[sn]["routes"]:
                rows.append((sn, "route", r, "first_visit", A[sn]["routes"][r]["first_visit"], B[sn]["routes"][r]["first_visit"]))
                rows.append((sn, "route", r, "revisit", A[sn]["routes"][r]["revisit"], B[sn]["routes"][r]["revisit"]))
        if "files" in A[sn] and "files" in B[sn]:
            for tag in A[sn]["files"]:
                rows.append((sn, "files", tag, "ready", A[sn]["files"][tag]["ready"], B[sn]["files"][tag]["ready"]))
                rows.append((sn, "files", tag, "chrome", A[sn]["files"][tag].get("chrome"), B[sn]["files"][tag].get("chrome")))
    print(f"scenario | where | what | metric | {a} | {b}")
    print("---|---|---|---|---|---")
    for sn, p, t, k, x, y in rows:
        print(f"{sn} | {p} | {t} | {k} | {fmt(x)} | {fmt(y)}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("label", nargs="?")
    ap.add_argument("other", nargs="?")
    ap.add_argument("--scenarios", default="lan,cpu4,phone")
    ap.add_argument("--no-files", action="store_true")
    ap.add_argument("--lang", default="")
    ap.add_argument("--compare", action="store_true")
    args = ap.parse_args()
    if args.compare:
        compare(args.label, args.other)
        return
    data = {}
    for name in args.scenarios.split(","):
        print("==", name, flush=True)
        data[name] = asyncio.run(run_scenario(name, SCENARIOS[name], args))
    path = os.path.join(PERF, f"{args.label}.json")
    json.dump(data, open(path, "w"), indent=1)
    print(table(data))
    print("saved", path)


if __name__ == "__main__":
    main()
