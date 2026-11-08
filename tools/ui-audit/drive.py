"""Scripted review driver: python3 -I tools/ui-audit/drive.py <script.py>

Runs the async `run(d)` of the given file after sign-in, with helpers on `d`: `width`, `lang`,
`nav`, `click_text`, `type`, `esc`, `js`, `shot`, `fullshot`, `audit` (audit.js on the current
view: overflow, clipped text, raw keys, English in it) and response overrides for states the
dry instance does not have:

    await d.fake_btrfs()           # a btrfs volume /volume1 in /api/v1/storage, shares on btrfs
    await d.fake_trial("booted")   # network trial adopt_at_boot: "boot" (planned) or "booted"

    await d.answer("/api/v1/ids/plan", {...})   # a fixed JSON answer (POST by default)

`d.btn(regex)` clicks the last visible button whose text matches, `d.dialog_text()` reads the
top open dialog. Overrides apply from the next load (`d.lang` or `d.reload`). Environment: BASE (instance URL,
default http://127.0.0.1:18600), CDP_PORT (9440), AUDIT_OUT (target/ui-audit).
"""
import asyncio, json, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.environ.get("AUDIT_OUT", REPO + "/target/ui-audit")
sys.path.insert(0, HERE)
import cdp  # noqa: E402
import run_all  # noqa: E402

cdp.S = OUT
cdp.SHOTS = os.path.join(OUT, "shots", "drive")
os.makedirs(cdp.SHOTS, exist_ok=True)
cdp.PORT = int(os.environ.get("CDP_PORT", "9440"))
run_all.BASE = os.environ.get("BASE", "http://127.0.0.1:18600")
AUDIT_JS = open(HERE + "/audit.js").read()
SRC = open(REPO + "/crates/rynas-ui/src/i18n.rs").read()
STR = r'"((?:[^"\\]|\\.)*)"'
_P = re.findall(STR + r"\s*=>\s*" + STR + r",\s*" + STR + r";", SRC)
_IT = {it for k, en, it in _P}
EN_ONLY = sorted({en for k, en, it in _P if en != it and en not in _IT and re.search(r"[A-Za-z]{3}", en) and "{" not in en})

# window.fetch wrapper: each rule rewrites the JSON of a matching GET answer
OVERRIDE = """
(()=>{if(window.__rules)return;window.__rules=[];const of=window.fetch.bind(window);
window.fetch=async(input,init)=>{const url=typeof input==='string'?input:input.url;const r=await of(input,init);
 const m=(init&&init.method)||'GET';
 for(const [re,f,want] of window.__rules){if((want||'GET')===m&&re.test(url)){try{const j=f(r.ok?await r.clone().json():{});
  return new Response(JSON.stringify(j),{status:200,headers:{'content-type':'application/json'}});}catch(e){console.log('override',e)}}}
 return r;};})();
"""
BTRFS = """window.__rules.push([/\\/api\\/v1\\/storage$/,j=>{j.filesystems.push({mountpoint:'/volume1',source:'/dev/md2',fstype:'btrfs',size:7.8e12,used:2.1e12,avail:5.7e12});return j}]);
window.__rules.push([/\\/api\\/v1\\/shares$/,j=>{j.forEach((v,i)=>{v.status.fs_type='btrfs';v.status.path_ok=true;v.status.nocow=(i%2===1)});return j}]);"""
TRIAL = """window.__rules.push([/\\/api\\/v1\\/network\\/state/,j=>{const now=j.read_at_ms||Date.now();
 j.trial={kind:'adopt_at_boot',revision:7,lkg_revision:6,started_ms:now-2*86400000,deadline_ms:%s,timeout_s:180,interfaces:%s,management_touched:true,author:'marco'};return j}]);"""


class D:
    def __init__(self, p):
        self.p = p
        self._override = False

    async def js(self, e):
        try:
            return await self.p.js(e)
        except Exception as x:
            print("JS ERR", str(x)[:300])

    async def _add(self, src):
        if not self._override:
            await self.p.send("Page.addScriptToEvaluateOnNewDocument", source=OVERRIDE)
            self._override = True
        await self.p.send("Page.addScriptToEvaluateOnNewDocument", source=src)

    async def fake_btrfs(self):
        await self._add(BTRFS)

    async def answer(self, url_re, value, method="POST"):
        """Answer requests whose URL matches the JS regex source `url_re` with `value` (JSON)."""
        await self._add(f"window.__rules.push([new RegExp({json.dumps(url_re)}),()=>({json.dumps(value)}),{json.dumps(method)}]);")

    async def btn(self, pattern):
        """Click the last visible button whose text matches the JS regex source `pattern`."""
        r = await self.js(f"(()=>{{const re=new RegExp({json.dumps(pattern)});const b=[...document.querySelectorAll('button')].filter(b=>b.offsetParent).reverse().find(b=>re.test(b.textContent.trim()));if(!b)return null;b.click();return b.textContent.trim()}})()")
        print("btn", pattern, "->", r)
        await asyncio.sleep(1.0)
        return r is not None

    async def dialog_text(self):
        """Text of the top open dialog."""
        return await self.js("(()=>{const d=[...document.querySelectorAll('dialog[open]')].pop();return d?d.innerText:''})()") or ""

    async def fake_trial(self, mode="booted", interfaces=("eth0", "eth1")):
        await self._add(TRIAL % ("now+150000" if mode == "booted" else "0", json.dumps(list(interfaces))))

    async def audit(self, tag, shot=True):
        await asyncio.sleep(0.6)
        if not await self.p.js("typeof window.__audit === 'function' && !!window.__EN"):
            await self.p.js(AUDIT_JS)
            await self.p.js(f"window.__EN = new Set({json.dumps(EN_ONLY)})")
        r = await self.p.js("window.__audit()")
        bad = {k: v for k, v in r.items() if k not in ("btn", "ellipsis") and v}
        print(f"[{tag}]", json.dumps(bad, ensure_ascii=False)[:1500] if bad else "clean", "| ellipsis:", len(r.get("ellipsis", [])))
        if shot:
            await self.p.shot(tag)
        return r

    async def width(self, w, h=860):
        await self.p.send("Emulation.setDeviceMetricsOverride", width=w, height=h, deviceScaleFactor=1, mobile=w < 600)
        await asyncio.sleep(0.6)

    async def lang(self, lang, theme="light"):
        await self.p.js(f"localStorage.setItem('rynas.lang','{lang}'); localStorage.setItem('rynas.theme','{theme}')")
        await self.reload()

    async def reload(self):
        await run_all.reload_shell(self.p)
        await asyncio.sleep(1.0)

    async def nav(self, r):
        await run_all.nav(self.p, r)
        await asyncio.sleep(1.5)

    async def click_text(self, sel, text):
        try:
            await self.p.click_text(sel, text)
            await asyncio.sleep(0.8)
            return True
        except Exception as x:
            print("CLICK FAIL", sel, text, x)
            return False

    async def esc(self):
        await run_all.esc(self.p)
        await asyncio.sleep(0.6)

    async def type(self, sel, text):
        await self.p.type_into(sel, text)
        await asyncio.sleep(0.3)

    async def shot(self, n):
        await self.p.shot(n)

    async def fullshot(self, n):
        await self.p.fullshot(n)


async def main():
    br = cdp.start_browser()
    try:
        p = await cdp.connect()
        await run_all.login(p)
        ns = {}
        exec(open(sys.argv[1]).read(), ns)
        await ns["run"](D(p))
        for e in p.events:
            if "integrity" not in e and "subsystem" not in e:
                print(" EV", e[:300])
    finally:
        br.terminate()
        br.wait()


asyncio.run(main())
