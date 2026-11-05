"""Ad-hoc probe: login, then run the JS steps given as arguments (each awaited, result printed)."""
import asyncio, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
# results, screenshots and browser profiles go on disk under target/ (never /tmp: RAM on dev boxes)
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
BASE_URL = os.environ.get("BASE", "http://127.0.0.1:18600")
sys.path.insert(0, HERE)
import cdp  # noqa: E402
import run_all  # noqa: E402

cdp.S = OUT_DIR
cdp.SHOTS = os.path.join(OUT_DIR, "shots")
cdp.PORT = 9395


async def main():
    br = cdp.start_browser()
    try:
        p = await cdp.connect()
        await run_all.login(p)
        for step in sys.argv[1:]:
            if step.startswith("nav:"):
                await run_all.nav(p, step[4:])
                await asyncio.sleep(1.5)
                continue
            if step.startswith("sleep:"):
                await asyncio.sleep(float(step[6:]))
                continue
            if step.startswith("shot:"):
                await p.shot(step[5:])
                continue
            if step.startswith("w:"):
                await p.send("Emulation.setDeviceMetricsOverride", width=int(step[2:]), height=800, deviceScaleFactor=1, mobile=False)
                await asyncio.sleep(0.5)
                continue
            if step == "esc":
                await run_all.esc(p)
                continue
            try:
                print(step[:80], "=>", await p.js(step))
            except Exception as e:
                print(step[:80], "=> ERR", e)
            await asyncio.sleep(0.5)
        for e in p.events:
            if "integrity" not in e and "subsystem" not in e:
                print(" ", e[:500])
    finally:
        br.terminate()


asyncio.run(main())
