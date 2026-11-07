"""Click through every page of the consolidated UI (rynasd --dry-system, fake DS920+):
light/dark, en/it, desktop/mobile; console errors, horizontal scroll, dialogs (open, Esc,
focus back), confirm with typed word, drawers, task follow."""
import asyncio, os, sys, json

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
# results, screenshots and browser profiles go on disk under target/ (never /tmp: RAM on dev boxes)
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
BASE_URL = os.environ.get("BASE", "http://127.0.0.1:18600")
import cdp  # noqa: E402

cdp.S = OUT_DIR
cdp.SHOTS = os.path.join(OUT_DIR, "shots")
os.makedirs(cdp.SHOTS, exist_ok=True)
cdp.PORT = 9391
BASE = os.environ.get("BASE", "http://127.0.0.1:18600")
FAILS = []

ROUTES = ["/", "/storage", "/storage/disks", "/storage/create", "/network", "/hardware", "/system",
          "/system/configuration", "/system/notifications", "/system/scheduler", "/notifications",
          "/snapshots", "/security", "/shares", "/users", "/services", "/plugins", "/docker", "/files"]


def check(cond, msg):
    print(("  ok   " if cond else "  FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


async def settle(p):
    await asyncio.sleep(0.8)
    await p.wait("!document.querySelector('.skeleton')", timeout=8, what="skeletons gone")


async def nav(p, route):
    """Client-side navigation (the router), as a user changes page."""
    await p.js(f"history.pushState(null, '', {json.dumps(route)}); dispatchEvent(new PopStateEvent('popstate'))")
    await asyncio.sleep(0.3)
    await p.wait("document.querySelector('.page')", what="page " + route)


async def reload_shell(p):
    await p.goto(BASE + "/")
    await p.wait("document.querySelector('.appbar')", what="shell")


async def page_checks(p, tag, route):
    await nav(p, route)
    try:
        await settle(p)
    except TimeoutError:
        check(False, f"{tag} {route}: skeletons still shown")
    hs = await p.js("document.documentElement.scrollWidth - innerWidth")
    check(hs <= 0, f"{tag} {route}: no horizontal scroll ({hs})")
    err = await p.js("document.querySelectorAll('.error-state').length")
    check(err == 0, f"{tag} {route}: no error boxes ({err})")
    name = (tag + route.replace("/", "_")).strip("_") or tag
    await p.shot(f"{tag}{route.replace('/', '_') or '_root'}")


async def login(p):
    await p.goto(BASE + "/")
    await p.wait("document.querySelector('.login-card input[name=password]') || document.querySelector('.appbar')", what="login or shell")
    if await p.js("!!document.querySelector('.login-card')"):
        await p.type_into("input[name=username]", os.environ.get("AUDIT_USER", "tester"))
        await p.type_into("input[name=password]", "correct-horse")
        await p.enter()
    await p.wait("document.querySelector('.appbar') && !document.querySelector('.login')", what="shell")


async def esc(p):
    await p.send("Input.dispatchKeyEvent", type="keyDown", key="Escape", code="Escape", windowsVirtualKeyCode=27)
    await p.send("Input.dispatchKeyEvent", type="keyUp", key="Escape", code="Escape", windowsVirtualKeyCode=27)


async def dialog_checks(p):
    # users: create drawer, Esc closes, focus back on the opener
    await nav(p, "/users")
    await p.wait("document.querySelector('.toolbar .btn.primary')", what="users toolbar")
    await p.js("document.querySelector('.toolbar .btn.primary').focus()")
    await p.click(".toolbar .btn.primary")
    await p.wait("document.querySelector('dialog.modal.drawer[open]')", what="user drawer")
    check(await p.js("document.activeElement && document.activeElement.tagName === 'INPUT'"), "drawer focuses its first field")
    check(await p.js("document.querySelector('dialog.modal.drawer').matches(':modal')"), "drawer is a modal dialog (inert page)")
    await p.shot("d01-user-drawer")
    # type a bad name, check field error appears only after blur
    await p.type_into("dialog input", "Bad Name")
    shown_typing = await p.js("!!document.querySelector('dialog .field-error svg')")
    check(not shown_typing, "field error not shown while typing the first entry")
    await p.send("Input.dispatchKeyEvent", type="keyDown", key="Tab", code="Tab", windowsVirtualKeyCode=9)
    await p.send("Input.dispatchKeyEvent", type="keyUp", key="Tab", code="Tab", windowsVirtualKeyCode=9)
    await asyncio.sleep(0.2)
    check(await p.js("!!document.querySelector('dialog .field-error svg')"), "field error shown after leaving the field")
    await esc(p)
    await p.wait("!document.querySelector('dialog.modal')", what="drawer closed by Esc")
    check(await p.js("document.activeElement === document.querySelector('.toolbar .btn.primary')"), "focus back on the opener")
    # create a user for later
    await p.click(".toolbar .btn.primary")
    await p.wait("document.querySelector('dialog.modal.drawer[open]')", what="user drawer 2")
    await p.type_into("dialog input", "u" + str(int(__import__("time").time()) % 100000))
    await p.js("(() => { const f = [...document.querySelectorAll('dialog input[type=password]')]; f.forEach(i => { i.focus(); }); })()")
    pw = await p.js("document.querySelectorAll('dialog input[type=password]').length")
    if pw:
        await p.type_into("dialog input[type=password]", "Correct-Horse-Battery-9")
        await p.js("(() => { const f = document.querySelectorAll('dialog input[type=password]'); if (f[1]) { f[1].focus(); } })()")
        await p.send("Input.insertText", text="Correct-Horse-Battery-9")
    await p.click("dialog .dialog-foot .btn.primary")
    try:
        await p.wait("!document.querySelector('dialog.modal')", timeout=6, what="user saved")
        check(True, "user staged")
    except TimeoutError:
        txt = await p.text("dialog .form-errors")
        check(False, f"user save: {txt}")
        await esc(p)
    await asyncio.sleep(0.6)
    # delete confirm: typed word appears with the option
    await p.wait("document.querySelector('.table .icon-btn.danger')", what="delete buttons")
    await p.js("[...document.querySelectorAll('.table .icon-btn.danger')].find(b => !b.disabled)?.click()")
    try:
        await p.wait("document.querySelector('dialog.modal.alert[open]')", what="delete confirm")
        check(await p.js("document.activeElement.textContent.trim().length > 0 && document.activeElement.classList.contains('ghost')"), "confirm focuses Cancel")
        await p.js("document.querySelector('dialog.modal.alert .switch').click()")
        await asyncio.sleep(0.2)
        check(await p.js("!!document.querySelector('dialog.modal.alert .confirm-field input')"), "typed confirmation appears with remove-home")
        check(await p.js("document.querySelector('dialog.modal.alert .dialog-foot .btn.danger').disabled"), "confirm disabled until typed")
        await p.shot("d02-delete-confirm")
        await esc(p)
        await p.wait("!document.querySelector('dialog.modal')", what="confirm closed")
    except TimeoutError as e:
        check(False, f"delete confirm: {e}")
    # pending bar -> review dialog
    try:
        await p.wait("document.querySelector('.pending')", timeout=6, what="pending bar")
        await p.click_text(".pending .btn", "Review")
        await p.wait("document.querySelector('dialog.modal.wide[open] .review-key')", what="review dialog")
        await p.shot("d03-review")
        await esc(p)
        await p.wait("!document.querySelector('dialog.modal')", what="review closed")
        # apply follows the task
        await p.click_text(".pending .btn.primary", "Apply")
        await p.wait("document.querySelector('.pending.done') || !document.querySelector('.pending')", timeout=20, what="apply done")
        check(True, "apply finished")
    except TimeoutError as e:
        check(False, f"pending/apply: {e}")
    # task panel log tail
    await p.click(".tasks-head")
    await p.wait("document.querySelector('.tasks-table .row-link')", what="task list")
    await p.click(".tasks-table .row-link")
    await p.wait("document.querySelector('.task-log pre.log')", what="task log")
    await p.shot("d04-task-log")
    await p.click(".tasks-head")
    # services: SMB draft survives an unrelated change
    await nav(p, "/services")
    await p.wait("document.querySelector('.card input')", what="smb form")
    await settle(p)
    await p.js("(() => { const i = [...document.querySelectorAll('.card .field input')].find(x => x.maxLength === 15); i.focus(); })()")
    await p.send("Input.insertText", text="X")
    await asyncio.sleep(0.3)
    before = await p.js("[...document.querySelectorAll('.card .field input')].find(x => x.maxLength === 15).value")
    # an unrelated configuration change: stage a group
    # an unrelated configuration change: save the NFS card (bumps the configuration generation)
    await p.js("(() => { const c = [...document.querySelectorAll('section.card')][1]; c.querySelector('.switch').click(); })()")
    await asyncio.sleep(0.3)
    await p.js("(() => { const c = [...document.querySelectorAll('section.card')][1]; c.querySelector('button[type=submit]').click(); })()")
    await asyncio.sleep(2.0)
    check(await p.js("!!document.querySelector('.toast')"), "NFS staged (unrelated change)")
    after = await p.js("[...document.querySelectorAll('.card .field input')].find(x => x.maxLength === 15).value")
    check(before == after and before.endswith("X"), f"SMB draft kept ({before!r} -> {after!r})")
    # hardware: power rule drawer
    await nav(p, "/hardware")
    await p.wait("document.querySelector('#hwt-t2')", what="hardware tabs")
    await p.click("#hwt-t2")
    await p.wait("document.querySelector('#hwt-p2 .btn.primary')", what="power")
    await settle(p)
    await p.js("[...document.querySelectorAll('#hwt-p2 .btn.primary')].find(b => b.textContent.includes('Add rule') && !b.disabled).click()")
    await p.wait("document.querySelector('dialog.modal.drawer[open]')", what="power drawer")
    await p.shot("d05-power-drawer")
    await esc(p)
    # storage: disk drawer
    await nav(p, "/storage/disks")
    await settle(p)
    if await p.js("!!document.querySelector('.cell-btn')"):
        await p.click(".cell-btn")
        await p.wait("document.querySelector('dialog.modal.drawer[open]')", what="disk drawer")
        await p.shot("d06-disk-drawer")
        await esc(p)
        await p.wait("!document.querySelector('dialog.modal')", what="disk drawer closed")
    # scheduler create drawer
    await nav(p, "/system/scheduler")
    await settle(p)
    await p.js("[...document.querySelectorAll('.btn.primary')].find(b => b.offsetParent)?.click()")
    await p.wait("document.querySelector('dialog.modal[open]')", what="scheduler drawer")
    await p.shot("d07-scheduler-drawer")
    await esc(p)
    # notifications settings drawer
    await nav(p, "/system/notifications")
    await settle(p)
    await p.js("[...document.querySelectorAll('.btn.primary')].find(b => b.offsetParent)?.click()")
    try:
        await p.wait("document.querySelector('dialog.modal[open]')", what="notifications drawer")
        await p.shot("d08-notif-drawer")
        await esc(p)
    except TimeoutError as e:
        check(False, str(e))
    # snapshots job drawer
    await nav(p, "/snapshots")
    await settle(p)
    await p.js("[...document.querySelectorAll('.btn.primary')].find(b => b.offsetParent)?.click()")
    try:
        await p.wait("document.querySelector('dialog.modal[open]')", what="snapshot drawer")
        await p.shot("d09-snap-drawer")
        await esc(p)
    except TimeoutError as e:
        check(False, str(e))


async def main():
    br = cdp.start_browser()
    try:
        p = await cdp.connect()
        await p.goto(BASE + "/")
        await p.js("localStorage.setItem('rynas.lang','en'); localStorage.setItem('rynas.theme','light')")
        await login(p)
        for r in ROUTES:
            await page_checks(p, "en-light", r)
        if "--pages-only" not in sys.argv:
            await dialog_checks(p)
        await p.js("localStorage.setItem('rynas.lang','it'); localStorage.setItem('rynas.theme','dark')")
        await reload_shell(p)
        for r in ROUTES:
            await page_checks(p, "it-dark", r)
        await p.send("Emulation.setDeviceMetricsOverride", width=390, height=844, deviceScaleFactor=2, mobile=True)
        for r in ROUTES:
            await page_checks(p, "m-it-dark", r)
        await p.js("localStorage.setItem('rynas.lang','en'); localStorage.setItem('rynas.theme','light')")
        await reload_shell(p)
        for r in ["/users", "/storage", "/system/configuration", "/network"]:
            await page_checks(p, "m-en-light", r)
        await nav(p, "/users")
        await settle(p)
        await p.js("document.querySelector('.toolbar .btn.primary').click()")
        await p.wait("document.querySelector('dialog.modal.drawer[open]')", what="mobile drawer")
        await p.shot("m-drawer")
        await esc(p)
        bad = [e for e in p.events if "exception" in e or "error" in e.lower() or "panic" in e.lower()]
        for e in bad:
            print("  console:", e[:400])
        check(not bad, f"no console errors ({len(bad)})")
    finally:
        br.terminate()
    print("FAILS:", len(FAILS))
    for f in FAILS:
        print("  -", f)


if __name__ == "__main__":
    asyncio.run(main())
