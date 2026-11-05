"""Every element that scrolls sideways (inner scrollbars too), on every route, in the dialogs
opened by the page's primary buttons and in each of their tabs; widths 1280/1024/390, en+it."""
import asyncio, json, os, sys
HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
# results, screenshots and browser profiles go on disk under target/ (never /tmp: RAM on dev boxes)
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
BASE_URL = os.environ.get("BASE", "http://127.0.0.1:18600")
sys.path.insert(0, HERE)
import cdp, run_all
cdp.S = OUT_DIR; cdp.SHOTS = os.path.join(OUT_DIR, "shots"); cdp.PORT = 9396

FIND = r"""(() => { const out = [];
  for (const e of document.querySelectorAll('body *')) {
    if (e.offsetParent === null && getComputedStyle(e).position !== 'fixed') continue;
    const ox = getComputedStyle(e).overflowX;
    const scrolls = (ox === 'auto' || ox === 'scroll') && e.scrollWidth > e.clientWidth + 1;
    if (scrolls) out.push((e.closest('dialog[open]') ? 'dialog ' : '') + e.tagName.toLowerCase() + '.' + [...e.classList].join('.') + ' ' + e.scrollWidth + '>' + e.clientWidth);
  }
  if (document.documentElement.scrollWidth > innerWidth + 1) out.push('PAGE ' + document.documentElement.scrollWidth);
  return out; })()"""

async def audit(p, tag):
    r = await p.js(FIND)
    for x in r: print(tag, '::', x)
    return len(r)

async def main():
    br = cdp.start_browser(); bad = 0; dlg = 0
    try:
        p = await cdp.connect()
        await run_all.login(p)
        for lang in ["it", "en"]:
            await p.js(f"localStorage.setItem('rynas.lang', {json.dumps(lang)})")
            for w, h, m in [(1280, 860, False), (1024, 768, False), (390, 844, True)]:
                await p.send("Emulation.setDeviceMetricsOverride", width=w, height=h, deviceScaleFactor=1, mobile=m)
                await p.goto(run_all.BASE + "/"); await asyncio.sleep(2)
                for route in run_all.ROUTES:
                    try: await run_all.nav(p, route)
                    except Exception as e: print("nav fail", route, e); continue
                    await asyncio.sleep(1.2)
                    tag = f"{lang} {w} {route}"
                    bad += await audit(p, tag)
                    n = await p.js("document.querySelectorAll('.page .btn.primary:not(dialog *), .page td.actions .icon-btn:not(dialog *)').length")
                    for i in range(n):
                        await p.js(f"document.querySelectorAll('.page .btn.primary:not(dialog *), .page td.actions .icon-btn:not(dialog *)')[{i}].click()")
                        await asyncio.sleep(0.8)
                        if not await p.js("!!document.querySelector('dialog[open]')"): 
                            await run_all.nav(p, route); await asyncio.sleep(0.8); continue
                        tabs = await p.js("document.querySelectorAll('dialog[open] [role=tab]').length")
                        for t in range(max(tabs, 1)):
                            if tabs: await p.js(f"document.querySelectorAll('dialog[open] [role=tab]')[{t}].click()"); await asyncio.sleep(0.6)
                            bad += await audit(p, f"{tag} dialog{i} tab{t}"); dlg += 1
                        await run_all.esc(p); await asyncio.sleep(0.4)
                        await run_all.nav(p, route); await asyncio.sleep(0.6)
        print("TOTAL", bad, "dialog views", dlg)
    finally:
        br.terminate()
asyncio.run(main())
