#!/usr/bin/env python3
"""Where the file manager's menus open: at the pointer, down and to the right (SynoFile).

    files_menus.py              # BASE (default http://127.0.0.1:18601: files_mock.py in front of start.sh)

A real right click (CDP mouse events) on a file at a known point: the menu's top-left corner
must be that point (±2 px), or the menu flipped to stay in the window near the bottom-right
corner. Also with the rynas sidebar collapsed, in `?window`, after scrolling the document, after
leaving and re-entering the page, and for the icon picker (beside the icon it was opened from).
"""
import asyncio, json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
os.environ.setdefault("BASE", "http://127.0.0.1:18601")
sys.path.insert(0, HERE)
import cdp  # noqa: E402
import run_all  # noqa: E402

cdp.S = OUT_DIR
cdp.SHOTS = os.path.join(OUT_DIR, "shots")
cdp.PORT = int(os.environ.get("CDP_PORT", "9397"))
BASE = os.environ["BASE"]
FAILS = []
SR = "document.querySelector('.rynas-files-host').shadowRoot"


def check(cond, msg):
    print(("  ok   " if cond else "  FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


async def mouse(p, kind, x, y, button="right"):
    await p.send("Input.dispatchMouseEvent", type=kind, x=x, y=y, button=button, buttons=2 if button == "right" else 1, clickCount=1)


async def right_click(p, x, y):
    await mouse(p, "mouseMoved", x, y, "none")
    await mouse(p, "mousePressed", x, y)
    await mouse(p, "mouseReleased", x, y)
    await asyncio.sleep(0.3)


async def menu_rect(p):
    return await p.js(f"(() => {{ const m = {SR}.querySelector('#menu'); if (m.hidden) return null;"
                      " const r = m.getBoundingClientRect(); return [r.left, r.top, r.right, r.bottom]; })()")


async def close(p):
    await run_all.esc(p)
    await asyncio.sleep(0.2)


async def row_point(p, n=0, dx=40):
    """A point inside the n-th visible row (or tile) of the active pane."""
    r = await p.js(f"(() => {{ const rows = [...{SR}.querySelectorAll('.row, .tile')].filter(e => e.offsetParent);"
                   f" const e = rows[{n}] || rows[rows.length - 1]; if (!e) return null; const b = e.getBoundingClientRect();"
                   f" return [Math.round(b.left + {dx}), Math.round(b.top + b.height / 2)]; }})()")
    return r


async def at_pointer(p, tag, x, y):
    await right_click(p, x, y)
    m = await menu_rect(p)
    if not m:
        check(False, f"{tag}: a menu opened at ({x},{y})")
        return
    check(abs(m[0] - x) <= 2 and abs(m[1] - y) <= 2, f"{tag}: menu top-left at the pointer ({x},{y}) -> ({m[0]:.0f},{m[1]:.0f})")
    if tag.endswith(" row"):
        await p.shot("files-menu-" + tag.split()[0].strip("?"))
    await close(p)


async def flipped(p, tag):
    """Near the bottom-right corner (the last row's right end): the menu stays in the visible
    part of the app, at the pointer on each axis unless it was pushed back against that edge."""
    b = await p.js(f"(() => {{ const r = {SR}.querySelector('.sf').getBoundingClientRect();"
                   " return [Math.max(0, r.left), Math.max(0, r.top), Math.min(innerWidth, r.right), Math.min(innerHeight, r.bottom)]; })()")
    x, y = await p.js(f"(() => {{ const rows = [...{SR}.querySelectorAll('.row')].filter(e => e.offsetParent); const e = rows[rows.length - 1];"
                      " const b = e.getBoundingClientRect(); return [Math.round(b.right - 30), Math.round(b.bottom - 4)]; })()")
    await right_click(p, x, y)
    m = await menu_rect(p)
    if not m:
        check(False, f"{tag}: a menu opened at ({x},{y})")
        return
    mr = [round(v) for v in m]
    inside = m[0] >= b[0] and m[1] >= b[1] and m[2] <= b[2] and m[3] <= b[3]
    check(inside, f"{tag}: menu inside the app {[round(v) for v in b]} ({mr})")
    left_ok = abs(m[0] - x) <= 2 or (m[0] < x and m[2] >= b[2] - 8)
    top_ok = abs(m[1] - y) <= 2 or (m[1] < y and m[3] >= b[3] - 8)
    check(left_ok and top_ok, f"{tag}: menu at the pointer, pushed back only against an edge ({x},{y}) -> {mr}")
    await close(p)


async def drawer(p, tag):
    """The transfers drawer: 44 px under the app's top edge, 10 px from its right edge."""
    await p.js(f"{SR}.querySelector('#xfer-btn').click()")
    await asyncio.sleep(0.4)
    r = await p.js(f"(() => {{ const d = {SR}.querySelector('#xfer-drawer'); if (d.hidden) return null; const a = {SR}.querySelector('.sf').getBoundingClientRect();"
                   " const b = d.getBoundingClientRect(); return [b.top - a.top, a.right - b.right]; })()")
    if r is None:
        check(False, f"{tag}: transfers drawer opened")
    else:
        check(abs(r[0] - 44) <= 2 and abs(r[1] - 10) <= 2, f"{tag}: transfers drawer inside the app (top +{r[0]:.0f}, right -{r[1]:.0f})")
    await p.js(f"(() => {{ const d = {SR}.querySelector('#xfer-drawer'); if (!d.hidden) {SR}.querySelector('#xfer-drawer-close').click(); }})()")
    await asyncio.sleep(0.2)


async def picker(p, tag):
    """The icon picker: opened from a row's context menu, beside the point it was asked at."""
    # open it as the app does from the menu ("Icona…"), at a known point
    ok = await p.js(f"(async () => {{ const m = await import('/files-app/js/iconpick.js');"
                    f" const rows = [...{SR}.querySelectorAll('.row')].filter(e => e.offsetParent);"
                    " if (!rows.length) return null; m.pickIcon({x: 300, y: 200}, '/documents'); return true; })()")
    await asyncio.sleep(0.4)
    r = await p.js(f"(() => {{ const e = {SR}.querySelector('.icon-pick'); if (!e) return null; const b = e.getBoundingClientRect(); return [b.left, b.top]; }})()")
    if not ok or not r:
        check(False, f"{tag}: icon picker opened ({ok}, {r})")
        return
    check(abs(r[0] - 308) <= 2 and abs(r[1] - 200) <= 2, f"{tag}: icon picker beside the point (308,200) -> ({r[0]:.0f},{r[1]:.0f})")
    await close(p)


async def open_files(p):
    await p.wait(f"{SR} && [...{SR}.querySelectorAll('.row, .tile')].some(e => e.offsetParent)", timeout=15, what="file rows")
    await asyncio.sleep(0.5)


async def round_of(p, tag):
    await open_files(p)
    pt = await row_point(p, 1)
    await at_pointer(p, f"{tag} row", *pt)
    pt = await row_point(p, 2, dx=200)
    await at_pointer(p, f"{tag} row, further right", *pt)
    await flipped(p, f"{tag} bottom-right")
    await picker(p, f"{tag}")
    await drawer(p, tag)


async def main():
    br = cdp.start_browser()
    try:
        p = await cdp.connect()
        await run_all.login(p)
        await p.js("document.querySelector('.sidebar a.nav-item[href=\"/files/documents\"], .sidebar a.nav-item[href=\"/files\"]').click()")
        await round_of(p, "shell")
        # sidebar collapsed
        await p.js("document.querySelector('.collapse-btn').click()")
        await asyncio.sleep(0.6)
        await round_of(p, "collapsed")
        await p.js("document.querySelector('.collapse-btn').click()")
        await asyncio.sleep(0.6)
        # the document scrolled (when it can) and a narrower window
        await p.js("scrollTo(0, 200)")
        await p.send("Emulation.setDeviceMetricsOverride", width=1024, height=700, deviceScaleFactor=1, mobile=False)
        await asyncio.sleep(0.6)
        await round_of(p, "1024 scrolled")
        await p.send("Emulation.setDeviceMetricsOverride", width=1280, height=860, deviceScaleFactor=1, mobile=False)
        # leave and come back
        await run_all.nav(p, "/storage")
        await asyncio.sleep(0.5)
        await run_all.nav(p, "/files")
        await round_of(p, "re-entered")
        # a window of its own
        await p.goto(BASE + "/files?window")
        await round_of(p, "?window")
        errs = [e for e in p.events if ("exception" in e or "panicked" in e) and "integrity" not in e]
        for e in errs:
            print("  ", e[:400])
        check(not errs, f"no exception or panic ({len(errs)})")
    finally:
        br.terminate()
    print("FAILS:", len(FAILS))
    sys.exit(1 if FAILS else 0)


asyncio.run(main())
