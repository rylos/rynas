# UI audit tools

Headless-Chromium checks of the built-in UI against a local, fake-hardware instance. Used by the
quality reviews (docs/quality/). Nothing here touches the host system or a running NAS.

    tools/ui-audit/start.sh            # rynasd --dry-system + rynas-web on 127.0.0.1:18600
    GENERIC=1 tools/ui-audit/start.sh 18611   # a plain PC: no Synology screens may appear (G8d)
    python3 -I tools/ui-audit/scroll_audit.py        # G7: anything that scrolls sideways
    python3 -I tools/ui-audit/audit.py it light 9401 # every view: overflow, clipping, raw keys, English in it
    python3 -I tools/ui-audit/probe.py nav:/storage w:1024 shot:storage "document.title"
    python3 -I tools/ui-audit/idle_dom.py 30         # pages left alone must not rebuild (exit 1 if one does)
    tools/ui-audit/stop.sh

- UPS page: `FAKE_UPS=ol|ob|lb|none` adds `fake_upsd.py` (status, `LIST CMD`, `INSTCMD`) and NUT
  files in the dry root; `FAKE_UPS_CMDS=all|quick|none` picks the offered battery tests,
  `FAKE_UPS_BACKEND=synodeb` the synodeb backend (see `start.sh`). The UPS page is the 4th tab of
  `/hardware` (`#hwt-t3`): the route audits only see the first tab.
- Login `tester` / `correct-horse` (`users.toml`, a test-only account of the local instance).
- `BASE` changes the instance URL; `AUDIT_OUT` the output folder (default `target/ui-audit`:
  results, screenshots, browser profiles). Keep it on disk, not on a RAM tmpfs.
- Run one browser at a time (each Chromium takes 0.5-1 GB).
- `idle_dom.py [seconds] [--views /hardware:1,/storage] [--limit N] [-v]`: each view (`:n` clicks
  the n-th tab) is watched while idle with a MutationObserver; polls and pushed updates may change
  text and classes, but more than N (default 20) elements removed and re-created fails the view
  (lost hover, focus, scroll, identify blink). `-v` lists the requests and events of the window,
  to find what rebuilt. The fake tree's fans drift a few rpm per write, so `hardware.status` is
  pushed every 10 s as on a real unit.
- `cdp.py` is a minimal DevTools-protocol driver; `run_all.py` has login and client-side navigation;
  `states.py` checks the loading/error states (second instance with rynasd stopped).
- `files_reenter.py`: leave the file manager and come back (sidebar, router, `?window`): the Files
  page shows again with the same app and no panic. Needs `files_mock.py 18601 18600` in front.
- `dash_shift.py [seconds]`: the dashboard's first seconds after sign-in, after a reload and after
  coming back from another page, sampled every 250 ms: layout-shift score (Layout Instability
  API), card heights placeholder vs content, device rows, chart paths inside the plot, scale label
  changes. Exit 1 on any shift. `SLOW=1` emulates a slow link and CPU (VPN, modest laptop), where
  the placeholders show long enough to matter; frames in `target/ui-audit/dash_shift.json`.
- `files_mock.py ... xfer`: a running tar.zst compression with bursty speed samples and a pause,
  for the transfer dialog's speed chart (`/files?xfer=last` opens it).
- `files_menus.py`: right click at a known point, the menu's top-left corner at the pointer (also
  sidebar collapsed, scrolled, `?window`), icon picker, transfers drawer. Same mock in front.
