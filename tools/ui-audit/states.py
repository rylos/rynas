"""Focus visibility (Tab through every route, light and dark), loading states (3 s latency) and
error states (rynas-web without rynasd on 18601), en and it. Screenshots in shots/st-*."""
import asyncio, json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
# results, screenshots and browser profiles go on disk under target/ (never /tmp: RAM on dev boxes)
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
BASE_URL = os.environ.get("BASE", "http://127.0.0.1:18600")
sys.path.insert(0, HERE)
import cdp, run_all  # noqa: E402

cdp.S = OUT_DIR
cdp.SHOTS = os.path.join(OUT_DIR, "shots")
cdp.PORT = 9440
ROUTES = ["/", "/storage", "/storage/disks", "/storage/create", "/network", "/hardware", "/system",
          "/system/configuration", "/system/notifications", "/system/scheduler", "/notifications",
          "/snapshots", "/security", "/shares", "/users", "/services"]
FOCUS = r"""(() => { const e = document.activeElement; if (!e || e === document.body) return null;
  const cs = getComputedStyle(e); const vis = cs.outlineStyle !== 'none' && parseFloat(cs.outlineWidth) > 0 || (cs.boxShadow && cs.boxShadow !== 'none');
  const r = e.getBoundingClientRect();
  return [e.tagName.toLowerCase() + '.' + [...e.classList].join('.') + ' "' + (e.innerText || e.getAttribute('aria-label') || '').trim().slice(0, 30) + '"', vis, e.matches(':focus-visible'), r.width > 0 && r.height > 0]; })()"""


async def tab(p):
    await p.send("Input.dispatchKeyEvent", type="keyDown", key="Tab", code="Tab", windowsVirtualKeyCode=9)
    await p.send("Input.dispatchKeyEvent", type="keyUp", key="Tab", code="Tab", windowsVirtualKeyCode=9)
    await asyncio.sleep(0.08)


async def main():
    br = cdp.start_browser()
    out = {"focus_bad": [], "focus_n": 0, "loading": [], "error": []}
    try:
        p = await cdp.connect()
        run_all.BASE = BASE_URL
        await p.goto(run_all.BASE + "/")
        await run_all.login(p)
        # focus
        for theme in ["light", "dark"]:
            await p.js(f"localStorage.setItem('rynas.theme','{theme}')")
            await p.goto(run_all.BASE + "/")
            await asyncio.sleep(1.5)
            for r in ROUTES:
                await run_all.nav(p, r)
                await asyncio.sleep(1.2)
                await p.js("document.activeElement && document.activeElement.blur(); document.querySelector('.main').focus()")
                seen = set()
                for _ in range(30):
                    await tab(p)
                    f = await p.js(FOCUS)
                    if not f:
                        continue
                    out["focus_n"] += 1
                    if not f[1] and f[3] and f[0] not in seen:
                        seen.add(f[0])
                        out["focus_bad"].append(f"{theme} {r}: {f[0]} focus-visible={f[2]}")
                        await p.shot(f"st-focus-{theme}{r.replace('/', '_')}-{len(seen)}")
                    if await p.js("!!document.querySelector('dialog[open]')"):
                        await run_all.esc(p)
        await p.js("localStorage.setItem('rynas.theme','light')")
        # loading: 3 s latency, screenshot after 1 s
        await p.send("Network.enable")
        for lang in ["it", "en"]:
            await p.js(f"localStorage.setItem('rynas.lang','{lang}')")
            for w in [390, 1280]:
                await p.send("Emulation.setDeviceMetricsOverride", width=w, height=860, deviceScaleFactor=1, mobile=w < 800)
                await p.send("Network.emulateNetworkConditions", offline=False, latency=0, downloadThroughput=-1, uploadThroughput=-1)
                await p.goto(run_all.BASE + "/")
                await asyncio.sleep(1.5)
                await p.send("Network.emulateNetworkConditions", offline=False, latency=3000, downloadThroughput=-1, uploadThroughput=-1)
                for r in ROUTES[:6] + ["/users", "/shares", "/snapshots", "/system/scheduler"]:
                    await run_all.nav(p, r)
                    await asyncio.sleep(1.0)
                    n = await p.js("document.querySelectorAll('.skeleton').length")
                    blank = await p.js("(document.querySelector('.page')?.innerText || '').trim().length")
                    out["loading"].append(f"{lang} {w} {r}: skeletons={n} text={blank}")
                    await p.shot(f"st-load-{lang}-{w}{r.replace('/', '_')}")
                await p.send("Network.emulateNetworkConditions", offline=False, latency=0, downloadThroughput=-1, uploadThroughput=-1)
        # errors: rynas-web without rynasd
        run_all.BASE = BASE_URL
        await p.goto(run_all.BASE + "/")
        await run_all.login(p)
        for lang in ["it", "en"]:
            await p.js(f"localStorage.setItem('rynas.lang','{lang}')")
            for w in [390, 1280]:
                await p.send("Emulation.setDeviceMetricsOverride", width=w, height=860, deviceScaleFactor=1, mobile=w < 800)
                await p.goto(run_all.BASE + "/")
                await asyncio.sleep(2)
                for r in ROUTES:
                    await run_all.nav(p, r)
                    await asyncio.sleep(2.5)
                    st = await p.js("""(() => ({err: document.querySelectorAll('.error-state').length, skel: document.querySelectorAll('.skeleton').length,
                        text: (document.querySelector('.page')?.innerText || '').replace(/\\s+/g, ' ').slice(0, 160)}))()""")
                    out["error"].append(f"{lang} {w} {r}: {json.dumps(st, ensure_ascii=False)}")
                    await p.shot(f"st-err-{lang}-{w}{r.replace('/', '_')}")
    finally:
        br.terminate()
    json.dump(out, open(os.path.join(HERE, "states.json"), "w"), indent=1, ensure_ascii=False)
    print("focus checked", out["focus_n"], "bad", len(out["focus_bad"]))
    for x in out["focus_bad"]:
        print(" ", x)
    for x in out["loading"] + out["error"]:
        print(" ", x)


asyncio.run(main())
