# UI audit tools

Headless-Chromium checks of the built-in UI against a local, fake-hardware instance. Used by the
quality reviews (docs/quality/). Nothing here touches the host system or a running NAS.

    tools/ui-audit/start.sh            # rynasd --dry-system + rynas-web on 127.0.0.1:18600
    python3 -I tools/ui-audit/scroll_audit.py        # G7: anything that scrolls sideways
    python3 -I tools/ui-audit/audit.py it light 9401 # every view: overflow, clipping, raw keys, English in it
    python3 -I tools/ui-audit/probe.py nav:/storage w:1024 shot:storage "document.title"
    tools/ui-audit/stop.sh

- Login `tester` / `correct-horse` (`users.toml`, a test-only account of the local instance).
- `BASE` changes the instance URL; `AUDIT_OUT` the output folder (default `target/ui-audit`:
  results, screenshots, browser profiles). Keep it on disk, not on a RAM tmpfs.
- Run one browser at a time (each Chromium takes 0.5-1 GB).
- `cdp.py` is a minimal DevTools-protocol driver; `run_all.py` has login and client-side navigation;
  `states.py` checks the loading/error states (second instance with rynasd stopped).
