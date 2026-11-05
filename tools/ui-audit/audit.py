"""Wave C GUI audit: every route, page tab, dialog (and nested confirm), dialog tab, wizard step,
drawer, palette, tasks panel, notification centre and app-bar menu, for one language/theme at
390/768/1024/1280 px. Usage: audit.py <lang> <theme> <cdp-port> [--quick] [--routes a,b]
Writes results-<lang>-<theme>.jsonl (one line per view) and screenshots in shots/."""
import asyncio, json, os, re, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
# results, screenshots and browser profiles go on disk under target/ (never /tmp: RAM on dev boxes)
OUT_DIR = os.environ.get("AUDIT_OUT", os.path.join(REPO, "target", "ui-audit"))
os.makedirs(os.path.join(OUT_DIR, "shots"), exist_ok=True)
BASE_URL = os.environ.get("BASE", "http://127.0.0.1:18600")
sys.path.insert(0, HERE)
import cdp  # noqa: E402
import run_all  # noqa: E402

LANG, THEME, PORT = sys.argv[1], sys.argv[2], int(sys.argv[3])
QUICK = "--quick" in sys.argv
cdp.S = OUT_DIR
cdp.SHOTS = os.path.join(OUT_DIR, "shots")
cdp.PORT = PORT
BASE = BASE_URL
run_all.BASE = BASE
TAG = f"{LANG}-{THEME}"
OUT = open(os.path.join(OUT_DIR, f"results-{TAG}.jsonl"), "w")
WIDTHS = [(390, 844, True), (768, 1024, True), (1024, 768, False), (1280, 860, False)]
if "--widths" in sys.argv:
    ws = [int(x) for x in sys.argv[sys.argv.index("--widths") + 1].split(",")]
    WIDTHS = [w for w in WIDTHS if w[0] in ws]
ROUTES = ["/", "/storage", "/storage/disks", "/storage/create", "/network", "/hardware", "/system",
          "/system/configuration", "/system/notifications", "/system/scheduler", "/notifications",
          "/snapshots", "/security", "/shares", "/users", "/services", "/files", "/plugins", "/nope"]
if "--routes" in sys.argv:
    ROUTES = sys.argv[sys.argv.index("--routes") + 1].split(",")
AUDIT_JS = open(os.path.join(HERE, "audit.js")).read()
SHOT_DIALOGS = os.environ.get("SHOT_DIALOGS", "1") == "1"

# English texts that differ in Italian (to spot English in Italian mode)
SRC = open(os.path.join(REPO, "crates/rynas-ui/src/i18n.rs")).read()
STR = r'"((?:[^"\\]|\\.)*)"'
EN_ONLY = sorted({en for k, en, it in re.findall(STR + r"\s*=>\s*" + STR + r",\s*" + STR + r";", SRC)
                  if en != it and re.search(r"[A-Za-z]{3}", en) and "{" not in en})

SKIP = r"apply|applica|run now|esegui ora|^scrub|avvia scrub|test|prova|refresh|aggiorna|reload|ricarica|discard|scarta|^start|^avvia|^stop|^ferma|sign out|^esci|identify|beep|copy|copia|download|scarica|export|esporta|mark all|segna|retry|riprova|wake|shut ?down|spegn|riavv|reboot|restart|clear|svuota|^cancel|^annulla|^back|^indietro|undo|import|confirm|conferma|generate|genera|reveal|mostra chiave|logout|ping|traceroute|lookup|resolve|risolvi|mount$|^monta|unmount|smonta|lock|unlock|blocca|sblocca|^save|^salva|send|invia|rollback|keep|mantieni|disable|disattiva|enable|attiva|resume|riprendi|pause|sospendi|forget|dimentica"

_SEL = open(os.path.join(HERE, "cands.js")).read().strip()
CANDS = "((scope, skip) => (" + _SEL + ")(scope, skip).map((b) => ((b.innerText || '').trim() || b.getAttribute('aria-label') || b.title || '?').replace(/\\s+/g, ' ').slice(0, 40)))"
CLICK = "((scope, skip, i, label) => { const els = (" + _SEL + ")(scope, skip); const l = (b) => ((b.innerText || '').trim() || b.getAttribute('aria-label') || b.title || '?').replace(/\\s+/g, ' ').slice(0, 40); if (!els[i] || l(els[i]) !== label) return 'moved'; els[i].click(); return 'ok'; })"

views = 0
issues = 0


async def ensure_js(p):
    if not await p.js("typeof window.__audit === 'function' && !!window.__EN"):
        await p.js(AUDIT_JS)
        await p.js(f"window.__EN = new Set({json.dumps(EN_ONLY)})")


async def audit(p, where, shot=None):
    global views, issues
    await ensure_js(p)
    r = await p.js("window.__audit()")
    views += 1
    bad = {k: v for k, v in r.items() if k != "btn" and v}
    r["where"] = where
    OUT.write(json.dumps(r) + "\n")
    OUT.flush()
    if bad:
        issues += 1
        brief = {k: (v[:3] if isinstance(v, list) else v) for k, v in bad.items() if k not in ("ellipsis",)}
        if brief:
            print(where, json.dumps(brief, ensure_ascii=False)[:600], flush=True)
    if shot or (bad.keys() - {"ellipsis", "eng"}):
        name = (shot or where).replace("/", "_").replace(" ", "_").replace(":", "")[:120]
        await p.shot(name)


async def settle(p):
    await asyncio.sleep(0.5)
    try:
        await p.wait("!document.querySelector('.skeleton')", timeout=6, what="skeletons gone")
    except TimeoutError:
        print("  skeleton left", flush=True)
    await asyncio.sleep(0.3)


async def esc(p):
    await run_all.esc(p)
    await asyncio.sleep(0.35)


async def close_all(p):
    for _ in range(4):
        if not await p.js("!!document.querySelector('dialog[open], .popover, .palette')"):
            return
        await esc(p)
    await p.js("document.querySelector('.pop-scrim')?.click()")


async def ndlg(p):
    return await p.js("document.querySelectorAll('dialog[open]').length")


async def dialog_views(p, where, depth=0):
    """The top dialog: every tab, then every nested dialog opened from its buttons."""
    tabs = await p.js("[...document.querySelectorAll('dialog[open]')].pop().querySelectorAll('[role=tab]').length")
    for t in range(max(tabs, 1)):
        if tabs:
            await p.js(f"[...document.querySelectorAll('dialog[open]')].pop().querySelectorAll('[role=tab]')[{t}].click()")
            await asyncio.sleep(0.5)
        w = f"{where} tab{t}" if tabs else where
        await audit(p, w, shot=w if SHOT_DIALOGS else None)
        if depth >= 1 or QUICK:
            continue
        cands = await p.js(f"({CANDS})('dialog', {json.dumps(SKIP)})")
        base = await ndlg(p)
        for i, label in enumerate(cands):
            r = await p.js(f"({CLICK})('dialog', {json.dumps(SKIP)}, {i}, {json.dumps(label)})")
            if r != "ok":
                continue
            await asyncio.sleep(0.6)
            n = await ndlg(p)
            if n > base:
                await dialog_views(p, f"{w} > {label}", depth + 1)
                await esc(p)
                await asyncio.sleep(0.3)
            if await ndlg(p) < base:
                return  # the button closed the dialog
        if tabs:
            await p.js(f"[...document.querySelectorAll('dialog[open]')].pop()?.querySelectorAll('[role=tab]')[{t}]?.click()")


async def page_tabs(p):
    return await p.js("[...document.querySelectorAll('.page [role=tab]')].filter(t => !t.closest('dialog') && t.checkVisibility()).length")


async def route_views(p, w, route):
    await run_all.nav(p, route)
    await settle(p)
    tabs = await page_tabs(p)
    for t in range(max(tabs, 1)):
        if tabs:
            await p.js(f"[...document.querySelectorAll('.page [role=tab]')].filter(t => !t.closest('dialog') && t.checkVisibility())[{t}].click()")
            await settle(p)
        where = f"{TAG} {w} {route}" + (f" ptab{t}" if tabs else "")
        await audit(p, where, shot=where)
        if route == "/storage/create":
            await wizard(p, where)
        if QUICK:
            continue
        cands = await p.js(f"({CANDS})('page', {json.dumps(SKIP)})")
        for i, label in enumerate(cands):
            href0 = await p.js("location.pathname")
            r = await p.js(f"({CLICK})('page', {json.dumps(SKIP)}, {i}, {json.dumps(label)})")
            if r != "ok":
                continue
            await asyncio.sleep(0.7)
            if await ndlg(p):
                await dialog_views(p, f"{where} [{label}]")
                await close_all(p)
            elif await p.js("location.pathname") != href0:
                await audit(p, f"{where} -> {await p.js('location.pathname')} [{label}]")
                await run_all.nav(p, route)
                await settle(p)
            else:
                # inline change (expand row, menu): audit it, then restore the page
                if await p.js("!!document.querySelector('.popover, .menu')"):
                    await audit(p, f"{where} [{label}] popover")
                    await close_all(p)
            if tabs:
                await p.js(f"[...document.querySelectorAll('.page [role=tab]')].filter(t => !t.closest('dialog') && t.checkVisibility())[{t}]?.click()")
                await asyncio.sleep(0.3)


async def wizard(p, where):
    """Walk the volume wizard as far as the free disks allow (never the final Create)."""
    for step in range(2, 5):
        if step == 2:  # on the disks step: tick two disks
            await p.js("""(() => { const c = document.querySelector('.wizard'); [...c.querySelectorAll('input[type=checkbox]:not(:checked)')].slice(0, 2).forEach(b => b.click()); })()""")
        if step == 3:  # on the level step: the first allowed level
            await p.js("""(() => { const r = document.querySelector('.wizard [role=radio]:not([aria-disabled=true]):not(:disabled)'); if (r) r.click(); })()""")
        await asyncio.sleep(0.6)
        nxt = await p.js("(() => { const b = document.querySelector('.wizard-foot .btn.primary'); if (!b || b.disabled) return false; b.click(); return true; })()")
        if not nxt:
            print(f"  wizard stops before step {step}", flush=True)
            return
        await asyncio.sleep(1.0)
        await audit(p, f"{where} step{step}", shot=f"{where} step{step}")


async def shell_views(p, w):
    where = f"{TAG} {w}"
    # palette
    await p.send("Input.dispatchKeyEvent", type="keyDown", key="k", code="KeyK", windowsVirtualKeyCode=75, modifiers=2)
    await p.send("Input.dispatchKeyEvent", type="keyUp", key="k", code="KeyK", windowsVirtualKeyCode=75, modifiers=2)
    await asyncio.sleep(0.6)
    if await p.js("!!document.querySelector('.palette')"):
        await audit(p, f"{where} palette", shot=f"{where} palette")
        await p.send("Input.insertText", text="s")
        await asyncio.sleep(0.4)
        await audit(p, f"{where} palette-s")
        await p.send("Input.insertText", text="zzzz")
        await asyncio.sleep(0.4)
        await audit(p, f"{where} palette-empty", shot=f"{where} palette-empty")
        await close_all(p)
    else:
        print(where, "palette did not open", flush=True)
    # app-bar popovers: notification centre, user menu, theme, mobile menu
    n = await p.js("[...document.querySelectorAll('.appbar button[aria-expanded], .appbar button[aria-haspopup]')].filter(b => b.checkVisibility() && !b.classList.contains('search')).length")
    for i in range(n):
        label = await p.js(f"[...document.querySelectorAll('.appbar button[aria-expanded], .appbar button[aria-haspopup]')].filter(b => b.checkVisibility() && !b.classList.contains('search'))[{i}].getAttribute('aria-label') || 'btn{i}'")
        await p.js(f"[...document.querySelectorAll('.appbar button[aria-expanded], .appbar button[aria-haspopup]')].filter(b => b.checkVisibility() && !b.classList.contains('search'))[{i}].click()")
        await asyncio.sleep(0.6)
        await audit(p, f"{where} appbar[{label}]", shot=f"{where} appbar {label}")
        await close_all(p)
        await asyncio.sleep(0.2)
    # tasks panel and a task log
    if await p.js("!!document.querySelector('.tasks-head')"):
        await p.click(".tasks-head")
        await asyncio.sleep(0.6)
        await audit(p, f"{where} tasks", shot=f"{where} tasks")
        if await p.js("!!document.querySelector('.tasks-table .row-link')"):
            await p.click(".tasks-table .row-link")
            await asyncio.sleep(0.8)
            await audit(p, f"{where} task-log", shot=f"{where} task-log")
        await p.click(".tasks-head")
        await asyncio.sleep(0.3)


async def main():
    br = cdp.start_browser()
    import signal
    signal.signal(signal.SIGTERM, lambda *a: (br.terminate(), os._exit(1)))
    t0 = time.time()
    try:
        p = await cdp.connect()
        await p.goto(BASE + "/")
        await p.js(f"localStorage.setItem('rynas.lang','{LANG}'); localStorage.setItem('rynas.theme','{THEME}')")
        await run_all.login(p)
        for (w, h, mobile) in WIDTHS:
            await p.send("Emulation.setDeviceMetricsOverride", width=w, height=h, deviceScaleFactor=1, mobile=mobile)
            await p.goto(BASE + "/")
            await p.wait("document.querySelector('.appbar')", what="shell")
            await asyncio.sleep(1.5)
            routes = list(ROUTES)
            vol = await p.js("fetch('/api/v1/storage/tree').then(r => r.json()).then(t => JSON.stringify(t).match(/\"label\":\"([^\"]+)\"/)?.[1] || null).catch(() => null)")
            if vol and "--routes" not in sys.argv:
                routes.insert(4, "/storage/volumes/" + vol)
            for route in routes:
                try:
                    await route_views(p, w, route)
                except Exception as e:  # keep walking
                    print(f"{TAG} {w} {route} ERROR {e}", flush=True)
                    await close_all(p)
            await run_all.nav(p, "/")
            await settle(p)
            try:
                await shell_views(p, w)
            except Exception as e:
                print(f"{TAG} {w} shell ERROR {e}", flush=True)
        errs = [e for e in p.events if ("exception" in e or "error" in e.lower() or "panic" in e.lower()) and "favicon" not in e]
        for e in errs[:20]:
            print("console:", e[:300])
    finally:
        br.terminate()
    print(f"DONE {TAG}: {views} views, {issues} with findings, {time.time() - t0:.0f}s")


asyncio.run(main())
