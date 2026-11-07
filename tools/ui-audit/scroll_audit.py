"""Every element that scrolls sideways (inner scrollbars too), on every route, in the dialogs
opened by the page's primary buttons and in each of their tabs; widths 1280/1024/390, en+it."""
# ROUTES=/hardware,/network limits it to some routes (`/docker:2` opens the page's third tab);
# LANGS=en / WIDTHS=1280,390 narrow the run; CDP_PORT picks another browser port;
# ROUTE_TIMEOUT (s, default 180) ends a route that takes longer (printed, the run goes on).
# One line per route and width is printed as it is done (progress), findings as they come.
# Row buttons: only the first visible row of each table is opened (the others open the same
# dialog), and buttons that start a task instead of a dialog (`data-act`) are skipped.
import asyncio, json, os, sys, time
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
# results, screenshots and browser profiles go on disk under target/ (never /tmp: RAM on dev boxes)
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
BASE_URL = os.environ.get("BASE", "http://127.0.0.1:18600")
sys.path.insert(0, HERE)
import cdp, run_all
cdp.S = OUT_DIR; cdp.SHOTS = os.path.join(OUT_DIR, "shots"); cdp.PORT = int(os.environ.get("CDP_PORT", "9396"))
ROUTE_TIMEOUT = float(os.environ.get("ROUTE_TIMEOUT", "180"))

FIND = r"""(() => { const out = [];
  for (const e of document.querySelectorAll('body *')) {
    if (e.offsetParent === null && getComputedStyle(e).position !== 'fixed') continue;
    const ox = getComputedStyle(e).overflowX;
    const scrolls = (ox === 'auto' || ox === 'scroll') && e.scrollWidth > e.clientWidth + 1;
    if (scrolls) out.push((e.closest('dialog[open]') ? 'dialog ' : '') + e.tagName.toLowerCase() + '.' + [...e.classList].join('.') + ' ' + e.scrollWidth + '>' + e.clientWidth);
  }
  if (document.documentElement.scrollWidth > innerWidth + 1) out.push('PAGE ' + document.documentElement.scrollWidth);
  return out; })()"""

# the openers: visible primary buttons outside dialogs, and the row buttons of the first
# visible row of every table (no task buttons, nothing disabled)
OPENERS = r"""(() => {
  const ok = b => b.offsetParent !== null && !b.closest('dialog') && !b.disabled
    && b.getAttribute('aria-disabled') !== 'true' && !b.hasAttribute('data-act');
  const list = [...document.querySelectorAll('.page .btn.primary')].filter(ok);
  for (const t of document.querySelectorAll('.page table')) {
    const row = [...t.querySelectorAll('tbody tr')].find(r => r.offsetParent !== null);
    if (row) list.push(...[...row.querySelectorAll('td.actions .icon-btn')].filter(ok));
  }
  return list; })()"""

def log(*a):
    print(*a, flush=True)

async def audit(p, tag):
    r = await p.js(FIND)
    for x in r: log(tag, '::', x)
    return len(r)

async def go(p, route):
    path, _, tab = route.partition(":")
    await run_all.nav(p, path)
    if tab:
        await asyncio.sleep(1.0)
        await p.js(f"document.querySelectorAll('.page [role=tab]')[{int(tab)}]?.click()")

async def route_views(p, route, tag):
    """(findings, dialog views) of one route at the current width and language."""
    bad = dlg = 0
    await go(p, route)
    await asyncio.sleep(1.2)
    bad += await audit(p, tag)
    n = await p.js(f"{OPENERS}.length")
    for i in range(n):
        if not await p.js(f"(() => {{ const b = {OPENERS}[{i}]; if (!b) return false; b.click(); return true; }})()"):
            continue
        await asyncio.sleep(0.8)
        if not await p.js("!!document.querySelector('dialog[open]')"):
            await go(p, route); await asyncio.sleep(0.8); continue
        tabs = await p.js("document.querySelectorAll('dialog[open] [role=tab]').length")
        for t in range(max(tabs, 1)):
            if tabs: await p.js(f"document.querySelectorAll('dialog[open] [role=tab]')[{t}].click()"); await asyncio.sleep(0.6)
            bad += await audit(p, f"{tag} dialog{i} tab{t}"); dlg += 1
        await run_all.esc(p); await asyncio.sleep(0.4)
        # a dialog that asks before closing (unsaved form): close every dialog by force
        await p.js("document.querySelectorAll('dialog[open]').forEach(d => d.close())")
        await go(p, route); await asyncio.sleep(0.6)
    return bad, dlg

async def main():
    br = cdp.start_browser(); bad = 0; dlg = 0; slow = []
    langs = os.environ.get("LANGS", "it,en").split(",")
    widths = [w for w in [(1280, 860, False), (1024, 768, False), (390, 844, True)] if str(w[0]) in os.environ.get("WIDTHS", "1280,1024,390").split(",")]
    routes = os.environ["ROUTES"].split(",") if os.environ.get("ROUTES") else run_all.ROUTES
    try:
        p = await cdp.connect()
        await run_all.login(p)
        for lang in langs:
            await p.js(f"localStorage.setItem('rynas.lang', {json.dumps(lang)})")
            for w, h, m in widths:
                await p.send("Emulation.setDeviceMetricsOverride", width=w, height=h, deviceScaleFactor=1, mobile=m)
                await p.goto(run_all.BASE + "/"); await asyncio.sleep(2)
                for route in routes:
                    tag = f"{lang} {w} {route}"
                    t0 = time.time()
                    try:
                        b, d = await asyncio.wait_for(route_views(p, route, tag), ROUTE_TIMEOUT)
                        bad += b; dlg += d
                        log(f"-- {tag}: {b} finding(s), {d} dialog view(s), {time.time() - t0:.0f} s")
                    except asyncio.TimeoutError:
                        slow.append(tag); log(f"-- {tag}: TIMEOUT after {ROUTE_TIMEOUT:.0f} s")
                        await p.js("document.querySelectorAll('dialog[open]').forEach(d => d.close())")
                    except Exception as e:
                        slow.append(tag); log(f"-- {tag}: failed: {e}")
        log("TOTAL", bad, "dialog views", dlg, "timeouts/failures", len(slow))
    finally:
        br.terminate()
    sys.exit(1 if bad or slow else 0)
asyncio.run(main())
