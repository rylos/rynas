"""Dashboard start-up stability: layout shifts and chart paths in the first seconds.

Records every 250 ms, for the first seconds after (1) signing in, (2) reloading the page with a
session and (3) coming back to the dashboard from another page:
- the Layout Instability API score (CLS-style sum of `layout-shift` entries, input or not);
- each widget card's box, the device rows of the network / disk widgets, the charts present;
- the path each chart canvas last stroked (canvas calls are wrapped), to check that curves
  never go below the baseline / above the top and that the scale top does not jump around.

Usage: python3 -I tools/ui-audit/dash_shift.py [seconds] [--json out.json]
Fails (exit 1) when the layout moves (shift score > 0.005 or a card changes height once its
content is shown), the device rows change, or a curve overshoots its data range.
"""
import asyncio, json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
sys.path.insert(0, HERE)
import cdp  # noqa: E402
import run_all  # noqa: E402

cdp.S = OUT_DIR
cdp.SHOTS = os.path.join(OUT_DIR, "shots")
cdp.PORT = int(os.environ.get("CDP_PORT", "9397"))
BASE = run_all.BASE

# installed before any page script: shift observer + canvas path recorder
INSTRUMENT = r"""
(() => {
  window.__ls = [];
  try {
    new PerformanceObserver(l => { for (const e of l.getEntries()) window.__ls.push({
      t: e.startTime, v: e.value, input: e.hadRecentInput,
      src: (e.sources || []).map(s => s.node ? (s.node.className || s.node.nodeName) + '' : '?').slice(0, 4) }); })
      .observe({ type: 'layout-shift', buffered: true });
  } catch (e) {}
  const P = CanvasRenderingContext2D.prototype;
  const cur = c => c.__p || (c.__p = []);
  const ow = (name, f) => { const o = P[name]; P[name] = function (...a) { f(this, a); return o.apply(this, a); }; };
  // points along the path, in canvas CSS px (the charts use a dpr transform only)
  ow('beginPath', c => { c.__p = []; c.__xy = null; });
  ow('moveTo', (c, [x, y]) => { cur(c).push([x, y]); c.__xy = [x, y]; });
  ow('lineTo', (c, [x, y]) => { cur(c).push([x, y]); c.__xy = [x, y]; });
  ow('quadraticCurveTo', (c, [cx, cy, x, y]) => {
    const [x0, y0] = c.__xy || [x, y];
    for (let i = 1; i <= 8; i++) { const t = i / 8, u = 1 - t;
      cur(c).push([u*u*x0 + 2*u*t*cx + t*t*x, u*u*y0 + 2*u*t*cy + t*t*y]); }
    c.__xy = [x, y];
  });
  ow('bezierCurveTo', (c, [ax, ay, bx, by, x, y]) => {
    const [x0, y0] = c.__xy || [x, y];
    for (let i = 1; i <= 8; i++) { const t = i / 8, u = 1 - t;
      cur(c).push([u*u*u*x0 + 3*u*u*t*ax + 3*u*t*t*bx + t*t*t*x, u*u*u*y0 + 3*u*u*t*ay + 3*u*t*t*by + t*t*t*y]); }
    c.__xy = [x, y];
  });
  ow('closePath', c => { c.__closed = true; });
  ow('stroke', c => {
    const cv = c.canvas; if (!cv.__strokes) cv.__strokes = [];
    cv.__strokes.push((c.__p || []).slice()); if (cv.__strokes.length > 8) cv.__strokes.shift();
  });
  ow('clearRect', c => { c.canvas.__strokes = []; c.canvas.__label = null; });
  ow('fillText', (c, [s]) => { c.canvas.__label = s; });
})();
"""

SAMPLE = r"""
(() => {
  const r = e => { const b = e.getBoundingClientRect(); return [Math.round(b.top), Math.round(b.height)]; };
  const cells = [...document.querySelectorAll('.grid.dash > .cell')].map(c => {
    const w = c.dataset.w, card = c.querySelector('.widget');
    const charts = [...c.querySelectorAll('canvas')].map(cv => {
      const h = cv.clientHeight;
      // data lines: grid lines are 2-point paths
      const lines = (cv.__strokes || []).filter(s => s.length > 2);
      let minY = null, maxY = null, firstY = null, lastY = null, n = 0;
      for (const s of lines) { for (const [, y] of s) { minY = minY === null ? y : Math.min(minY, y); maxY = maxY === null ? y : Math.max(maxY, y); } n += s.length; }
      if (lines[0]) { firstY = lines[0][0][1]; lastY = lines[0][lines[0].length - 1][1]; }
      return { h, w: cv.clientWidth, minY, maxY, firstY, lastY, n, label: cv.__label || null };
    });
    return { w, box: card ? r(card) : null, skel: !!c.querySelector('.skeleton'),
             rows: [...c.querySelectorAll('.rows > .row .mono, .rows > .row > .mono')].map(e => e.textContent), charts };
  });
  const ls = window.__ls || [];
  return { t: Math.round(performance.now()), path: location.pathname, cells,
           cls: ls.reduce((a, e) => a + e.v, 0), shifts: ls.length, last: ls.slice(-3) };
})()
"""


def analyse(tag, frames):
    """Problems in one recording (empty list = stable)."""
    probs = []
    shown = [f for f in frames if f["cells"]]
    if not shown:
        return [f"{tag}: dashboard never shown"]
    cls = shown[-1]["cls"] - shown[0]["cls"]
    if cls > 0.005:
        srcs = {s for f in shown for e in f["last"] for s in e["src"]}
        probs.append(f"{tag}: layout shift score {cls:.4f} ({shown[-1]['shifts'] - shown[0]['shifts']} shifts; {sorted(srcs)[:6]})")
    by = {}
    for f in shown:
        for c in f["cells"]:
            by.setdefault(c["w"], []).append((f["t"], c))
    for w, seq in by.items():
        full = [(t, c) for t, c in seq if not c["skel"]]
        hs = sorted({c["box"][1] for _, c in full if c["box"]})
        if len(hs) > 1:
            probs.append(f"{tag}: {w} card height changes after its content is shown: {hs}")
        skel_h = {c["box"][1] for _, c in seq if c["skel"] and c["box"]}
        if skel_h and hs and (skel_h - set(hs)):
            probs.append(f"{tag}: {w} placeholder height {sorted(skel_h)} != content height {hs}")
        rows = [tuple(c["rows"]) for _, c in full]
        if len(set(rows)) > 1:
            probs.append(f"{tag}: {w} device rows change: {sorted(set(rows), key=len)}")
        for t, c in full:
            for i, ch in enumerate(c["charts"]):
                if ch["minY"] is None:
                    continue
                # strokes stay inside the canvas (1 px of line width tolerated)
                if ch["maxY"] > ch["h"] + 0.5 or ch["minY"] < -0.5:
                    probs.append(f"{tag}: {w} chart {i} path leaves the plot at t={t}: y {ch['minY']:.1f}..{ch['maxY']:.1f} (h {ch['h']})")
                    break
        labels = [c["charts"][0]["label"] for _, c in full if c["charts"] and c["charts"][0]["label"]]
        changes = sum(1 for a, b in zip(labels, labels[1:]) if a != b)
        if changes > 2:
            probs.append(f"{tag}: {w} scale label changes {changes}x: {labels[:12]}")
        no_chart = [t for t, c in full if any(ch["n"] == 0 for ch in c["charts"])]
        # a line needs two samples: right after the instance started there is one for a second
        if len(no_chart) > 8:
            probs.append(f"{tag}: {w} chart empty while content shown ({len(no_chart)} frames)")
    return probs


async def record(p, secs):
    frames = []
    for _ in range(int(secs * 4)):
        try:
            frames.append(await p.js(SAMPLE))
        except Exception as e:  # navigation in progress
            frames.append({"t": 0, "path": "", "cells": [], "cls": 0, "shifts": 0, "last": [], "err": str(e)})
        await asyncio.sleep(0.25)
    return frames


async def main():
    secs = float(sys.argv[1]) if len(sys.argv) > 1 and not sys.argv[1].startswith("-") else 15.0
    out = sys.argv[sys.argv.index("--json") + 1] if "--json" in sys.argv else os.path.join(OUT_DIR, "dash_shift.json")
    br = cdp.start_browser()
    runs = {}
    try:
        p = await cdp.connect()
        await p.send("Page.addScriptToEvaluateOnNewDocument", source=INSTRUMENT)
        # (1) sign in: the dashboard mounts right after the login
        await p.send("Network.enable")
        await p.send("Network.clearBrowserCookies")
        if os.environ.get("SLOW"):
            # a NAS over a VPN from a modest laptop: slow link, slow CPU
            await p.send("Network.emulateNetworkConditions", offline=False, latency=150,
                         downloadThroughput=300_000, uploadThroughput=100_000)
            await p.send("Emulation.setCPUThrottlingRate", rate=4)
        await p.goto(BASE + "/")
        # (the first load downloads the whole app: long on the slow link)
        await p.wait("document.querySelector('.login-card input[name=password]') || document.querySelector('.appbar')", timeout=90)
        if await p.js("!!document.querySelector('.login-card')"):
            await p.type_into("input[name=username]", "tester")
            await p.type_into("input[name=password]", "correct-horse")
            await p.enter()
        runs["login"] = await record(p, secs)
        # (2) reload with the session
        await p.send("Page.navigate", url=BASE + "/")
        runs["reload"] = await record(p, secs)
        # (3) another page, then back to the dashboard (client-side navigation)
        await run_all.nav(p, "/storage")
        await asyncio.sleep(2)
        await p.js("window.__ls.length = 0")
        await p.js("history.pushState(null, '', '/'); dispatchEvent(new PopStateEvent('popstate'))")
        runs["nav"] = await record(p, secs)
        await p.shot("dash-shift-end")
    finally:
        br.terminate()
    with open(out, "w") as f:
        json.dump(runs, f)
    probs = []
    for tag, frames in runs.items():
        fr = [f for f in frames if f.get("path") == "/"]
        shown = [f for f in fr if f["cells"]]
        cls = (shown[-1]["cls"] - shown[0]["cls"]) if shown else 0
        print(f"{tag}: {len(fr)} frames, shift score {cls:.4f}")
        probs += analyse(tag, fr)
    for pr in probs:
        print("  FAIL", pr)
    print("ok" if not probs else f"{len(probs)} problem(s); frames in {out}")
    sys.exit(1 if probs else 0)


asyncio.run(main())
