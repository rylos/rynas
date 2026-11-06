// Checks of one view, loaded into the page by audit.py: `window.__audit()` returns
// { overflow, hscroll, clipped, ellipsis, raw, eng, btn } for the top open dialog (or the
// whole page when none is open). Every list holds short descriptions of what was found:
// - overflow: the page itself is wider than the viewport (G7);
// - hscroll: an element inside scrolls sideways (overflow-x auto|scroll, content wider; G7);
//   log viewers and code blocks (pre, code, .log, .mono-block) are wide by nature;
// - clipped: text cut off by overflow hidden/clip without an ellipsis, or a control whose
//   label is wider than the control;
// - ellipsis: text shortened with an ellipsis (informational: audit.py does not count it);
// - raw: visible i18n keys (`area.thing`) instead of their text;
// - eng: in Italian, texts that exist only in English in the dictionary (window.__EN, a Set
//   filled by audit.py), outside [lang=en] (the server's English details, raw UPS texts);
// - btn: number of buttons in the scope.
window.__audit = () => {
  const dialogs = [...document.querySelectorAll('dialog[open]')];
  const scope = dialogs.length ? dialogs[dialogs.length - 1] : document.body;
  const it = (document.documentElement.lang || '').startsWith('it');
  const visible = (e) => e.checkVisibility ? e.checkVisibility() : e.offsetParent !== null;
  const name = (e) => {
    const cls = [...e.classList].slice(0, 3).join('.');
    const txt = (e.innerText || e.getAttribute('aria-label') || '').trim().replace(/\s+/g, ' ').slice(0, 40);
    return e.tagName.toLowerCase() + (cls ? '.' + cls : '') + (txt ? ' "' + txt + '"' : '');
  };
  const wide = (e) => e.closest('pre, code, .log, .log-tail, .mono-block, [data-wide]');
  const out = { overflow: [], hscroll: [], clipped: [], ellipsis: [], raw: [], eng: [], btn: 0 };

  if (document.documentElement.scrollWidth > innerWidth + 1) {
    out.overflow.push('page ' + document.documentElement.scrollWidth + '>' + innerWidth);
  }
  for (const e of scope.querySelectorAll('*')) {
    if (!visible(e)) continue;
    const cs = getComputedStyle(e);
    // visually hidden on purpose (screen-reader labels of icon buttons): 1 px, clipped
    if (e.clientWidth <= 1 || (cs.clip && cs.clip !== 'auto')) continue;
    const over = e.scrollWidth > e.clientWidth + 1 && e.clientWidth > 0;
    if (over && (cs.overflowX === 'auto' || cs.overflowX === 'scroll') && !wide(e)) {
      out.hscroll.push(name(e) + ' ' + e.scrollWidth + '>' + e.clientWidth);
    } else if (over && (cs.overflowX === 'hidden' || cs.overflowX === 'clip') && e.innerText && e.innerText.trim()) {
      (cs.textOverflow === 'ellipsis' ? out.ellipsis : out.clipped).push(name(e));
    }
    // a button or pill whose own label does not fit (overflow visible draws outside it)
    if (over && cs.overflowX === 'visible' && e.matches('button, .btn, .status, .pill, .chip') && !e.querySelector('svg:only-child')) {
      out.clipped.push(name(e) + ' ' + e.scrollWidth + '>' + e.clientWidth);
    }
  }
  out.btn = scope.querySelectorAll('button, [role=button]').length;

  const KEY = /^[a-z][a-z0-9_]*(\.[a-z0-9_]+)+$/;
  const walker = document.createTreeWalker(scope, NodeFilter.SHOW_TEXT);
  const seen = new Set();
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const t = n.textContent.trim().replace(/\s+/g, ' ');
    const el = n.parentElement;
    if (!t || !el || !visible(el) || el.closest('script, style, pre, code, .mono, [lang=en]')) continue;
    if (KEY.test(t) && !seen.has('k' + t)) {
      seen.add('k' + t);
      out.raw.push(t);
    }
    if (it && window.__EN && window.__EN.has(t) && !seen.has('e' + t)) {
      seen.add('e' + t);
      out.eng.push(t);
    }
  }
  // labels that are not text nodes: aria-label, title, placeholder
  for (const e of scope.querySelectorAll('[aria-label], [title], [placeholder]')) {
    if (!visible(e) || e.closest('[lang=en]')) continue;
    for (const a of ['aria-label', 'title', 'placeholder']) {
      const v = (e.getAttribute(a) || '').trim();
      if (!v) continue;
      if (KEY.test(v) && !seen.has('k' + v)) {
        seen.add('k' + v);
        out.raw.push(a + '=' + v);
      }
      if (it && window.__EN && window.__EN.has(v) && !seen.has('e' + v)) {
        seen.add('e' + v);
        out.eng.push(a + '=' + v);
      }
    }
  }
  return out;
};
