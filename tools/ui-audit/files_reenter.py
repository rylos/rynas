#!/usr/bin/env python3
"""Leaving the file manager and coming back: the Files page shows again, with the same app.

    files_reenter.py            # BASE (default http://127.0.0.1:18601: files_mock.py in front of start.sh)

Sequences (sidebar clicks, router navigation, a full load in between, `?window`): after each
return to /files the <main> holds exactly one page, the Files one, whose <rynas-files> hosts the
shared app (same tabs element as the first time), and no exception or wasm panic was logged.
"""
import asyncio, os, sys

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
cdp.PORT = int(os.environ.get("CDP_PORT", "9396"))
BASE = os.environ["BASE"]
FAILS = []

# what the main area shows: the pages in it, and the state of the file manager
STATE = """(() => {
  const main = document.querySelector('main.main');
  const pages = [...main.querySelectorAll(':scope .page')].filter(p => !p.parentElement.closest('.page'));
  const rf = main.querySelector('rynas-files');
  const host = rf && rf.querySelector(':scope > .rynas-files-host');
  const tabs = host && host.shadowRoot && host.shadowRoot.querySelector('#tabs');
  if (tabs && !window.__rfTabs) window.__rfTabs = tabs;
  return {
    pages: pages.map(p => p.className),
    hosted: !!host,
    sameApp: !!tabs && tabs === window.__rfTabs,
    tabCount: tabs ? tabs.children.length : 0,
    current: (document.querySelector('.sidebar a[aria-current=page]') || {}).textContent || '',
  };
})()"""


def check(cond, msg):
    print(("  ok   " if cond else "  FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


async def click_nav(p, href):
    await p.js(f"document.querySelector('.sidebar a.nav-item[href=\"{href}\"]').click()")
    await asyncio.sleep(0.3)


async def expect_files(p, tag):
    try:
        await p.wait("(() => { const m = document.querySelector('main.main'); const h = m && m.querySelector('rynas-files > .rynas-files-host');"
                     " return h && h.shadowRoot.querySelector('#tabs').children.length > 0 && !m.querySelector('.page:not(.files-page)'); })()",
                     timeout=10, what="files page")
    except TimeoutError:
        pass
    s = await p.js(STATE)
    check(s["pages"] == ["page files-page"], f"{tag}: only the Files page in <main> ({s['pages']})")
    check(s["hosted"] and s["tabCount"] > 0, f"{tag}: the file manager is shown ({s['tabCount']} tabs)")
    check(s["sameApp"], f"{tag}: same app as the first time (tabs and transfers kept)")


async def expect_other(p, tag):
    await asyncio.sleep(0.5)
    s = await p.js(STATE)
    check(len(s["pages"]) == 1 and "files-page" not in s["pages"][0], f"{tag}: one page, not Files ({s['pages']})")


def errors(p):
    return [e for e in p.events if ("exception" in e or "panicked" in e or "console.error" in e)
            and "integrity" not in e and "subsystem" not in e]


async def main():
    br = cdp.start_browser()
    try:
        p = await cdp.connect()
        await run_all.login(p)
        # sidebar clicks, as Marco does
        await click_nav(p, "/files")
        await expect_files(p, "click 1 /files")
        for other in ["/storage", "/", "/users", "/storage"]:
            await click_nav(p, other)
            await expect_other(p, f"click {other}")
            await click_nav(p, "/files")
            await expect_files(p, f"click {other} -> /files")
        # router navigation, a folder address, and a page in between
        for route in ["/storage", "/files/documents", "/network", "/files"]:
            await run_all.nav(p, route)
            if route.startswith("/files"):
                await expect_files(p, f"nav {route}")
            else:
                await expect_other(p, f"nav {route}")
        # a full load on another page, then the sidebar (the app is made again: new page life)
        await p.goto(BASE + "/storage")
        await p.wait("document.querySelector('.appbar')", what="shell")
        await p.js("window.__rfTabs = null")
        await click_nav(p, "/files")
        await expect_files(p, "load /storage, click /files")
        await click_nav(p, "/hardware")
        await expect_other(p, "click /hardware")
        await click_nav(p, "/files")
        await expect_files(p, "click /hardware -> /files")
        # the file manager in a window of its own, then the shell around it again
        await p.goto(BASE + "/files?window")
        await p.wait("document.querySelector('.app.bare')", what="bare window")
        await p.js("window.__rfTabs = null")
        await expect_files(p, "?window")
        await run_all.nav(p, "/storage")
        await expect_other(p, "?window -> nav /storage")
        await run_all.nav(p, "/files?window")
        await expect_files(p, "nav /storage -> /files?window")
        errs = errors(p)
        for e in errs:
            print("  ", e[:500])
        check(not errs, f"no exception or panic in the console ({len(errs)})")
    finally:
        br.terminate()
    print("FAILS:", len(FAILS))
    sys.exit(1 if FAILS else 0)


asyncio.run(main())
