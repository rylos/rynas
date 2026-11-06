"""Idle DOM churn: a page left alone must not rebuild itself.

Every view is opened, given time to load, then watched for SECONDS with a MutationObserver on
the page root. Polls and pushed updates may change text and classes; they must not remove and
re-create elements (that loses hover, focus, scroll and an identify blink: the "page reloads
every few seconds" bug). A view fails when more than LIMIT elements are removed while idle.

    python3 -I tools/ui-audit/idle_dom.py [seconds] [--views hardware:1,storage] [--limit N] [-v]

A view is a route, optionally with `:<tab>` (the n-th `[role=tab]` of the page clicked first).
Prints, per view, the elements removed/added and the biggest removed subtrees (tag.class),
so the closure that rebuilds can be found. Exit 1 on a failure.
"""
import asyncio, json, os, sys, time

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

# the views with live data (polls, the event stream, the 1 s clocks)
VIEWS = ["/", "/storage", "/storage/disks", "/hardware", "/hardware:1", "/hardware:2", "/hardware:3",
         "/hardware:4", "/network", "/system", "/snapshots", "/system/scheduler", "/services"]

OBSERVE = r"""
(() => {
  const root = document.querySelector('main') || document.body;
  const w = window.__idle = { removed: 0, added: 0, redrawn: 0, big: {}, times: [] };
  const sel = (n) => n.tagName.toLowerCase() + (n.className && typeof n.className === 'string'
      ? '.' + n.className.trim().split(/\s+/).slice(0, 3).join('.') : '');
  w.obs = new MutationObserver((list) => {
    for (const m of list) {
      // the inside of an <svg> (an icon's markup set again, a curve drawn again) is a redraw:
      // counted apart, the element itself stays with its hover and focus
      if (m.target.namespaceURI === 'http://www.w3.org/2000/svg') {
        for (const n of m.removedNodes) if (n.nodeType === 1) w.redrawn++;
        continue;
      }
      for (const n of m.removedNodes) {
        if (n.nodeType !== 1) continue;
        const k = 1 + n.querySelectorAll('*').length;
        w.removed += k;
        const key = sel(n) + ' in ' + (m.target.nodeType === 1 ? sel(m.target) : '#text');
        w.big[key] = (w.big[key] || 0) + k;
        w.times.push(Math.round(performance.now()));
      }
      for (const n of m.addedNodes) {
        if (n.nodeType === 1) w.added += 1 + n.querySelectorAll('*').length;
      }
    }
  });
  w.obs.observe(root, { childList: true, subtree: true });
  w.start = performance.now();
  return root.querySelectorAll('*').length;
})()
"""

REPORT = r"""
(() => {
  const w = window.__idle; w.obs.disconnect();
  const big = Object.entries(w.big).sort((a, b) => b[1] - a[1]).slice(0, 6);
  const t0 = w.start;
  const secs = [...new Set(w.times.map(t => Math.round((t - t0) / 1000)))];
  const req = performance.getEntriesByType('resource').filter(e => e.startTime >= t0)
      .map(e => Math.round((e.startTime - t0) / 1000) + 's ' + e.name.replace(location.origin, ''));
  return JSON.stringify({ removed: w.removed, added: w.added, redrawn: w.redrawn, big, secs, req });
})()
"""


async def open_view(p, view):
    route, _, tab = view.partition(":")
    await run_all.nav(p, route)
    await asyncio.sleep(1.0)
    if tab:
        ok = await p.js(f"(() => {{ const t = document.querySelectorAll('.page [role=tab]')[{int(tab)}]; if (!t) return false; t.click(); return true; }})()")
        if not ok:
            return False
    # loading, skeletons, the first poll answers
    await asyncio.sleep(4.0)
    return True


async def main():
    args = sys.argv[1:]
    seconds, limit, verbose, views = 30.0, 20, False, VIEWS
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--views":
            views = args[i + 1].split(","); i += 1
        elif a == "--limit":
            limit = int(args[i + 1]); i += 1
        elif a == "-v":
            verbose = True
        else:
            seconds = float(a)
        i += 1
    fails = []
    br = cdp.start_browser()
    try:
        p = await cdp.connect()
        await p.send("Emulation.setDeviceMetricsOverride", width=1280, height=900, deviceScaleFactor=1, mobile=False)
        if verbose:
            await p.send("Network.enable")
        await run_all.login(p)
        for v in views:
            if not await open_view(p, v):
                print(f"  skip {v}: no such tab")
                continue
            total = await p.js(OBSERVE)
            t0 = time.time()
            await asyncio.sleep(seconds)
            r = json.loads(await p.js(REPORT))
            bad = r["removed"] > limit
            tag = "FAIL" if bad else "ok  "
            print(f"  {tag} {v:22} {total:5} el, removed {r['removed']:5} added {r['added']:5}"
                  f" (svg redrawn {r['redrawn']}) in {seconds:.0f}s"
                  + (f"  at s {r['secs'][:20]}" if r["removed"] else ""), flush=True)
            if bad or verbose:
                for k, n in r["big"]:
                    print(f"         {n:5}  {k[:140]}")
            if verbose:
                print("         requests:", ", ".join(r["req"]))
                ev = [f"{t - t0:.0f}s {n} {d[:80]}" for t, n, d in p.sse if t >= t0 and not n.startswith("metrics")]
                print("         events:", "; ".join(ev) or "-")
            if bad:
                fails.append(v)
    finally:
        br.terminate()
    print("idle DOM:", "FAIL " + ", ".join(fails) if fails else "ok")
    sys.exit(1 if fails else 0)


asyncio.run(main())
