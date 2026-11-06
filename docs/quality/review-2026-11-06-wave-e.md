# Quality review — 2026-11-06, wave E

Scope: `git diff 07fdf6b..c203481` (40 commits): translatable notifications (event codes, synodeb
forward), remaining English backend texts, the file manager (`rynas-files`, `rynas-files-worker`,
`rynasd/src/files.rs`, `files-app`, `pages/files.rs`, `docs/plugins/files.md`), tar.zst, removable
drives (`rynas-removable`, `rynasd/src/removable.rs`, `rynas-ui/src/removable.rs` + Hardware tab).
Rules `G*`, `A*`, `M*`, `S*` in [`guidelines.md`](guidelines.md). Security first, then GUI/API,
then speed and memory. Time box ~30 min (overran to ~55 min: cold release build + LTO).

## Measurements

| Check | Result |
|---|---|
| `cargo test --workspace` | 691 passed, 0 failed (before the fixes); `rynasd --test files` green after fix 2 |
| `cargo clippy --workspace --all-targets -D warnings` | clean |
| worker (own workspace): `cargo clippy/test --manifest-path crates/rynas-files-worker/Cargo.toml` | clean / 23 passed |
| wasm brotli -q 11 | **967 460 B = 945 KiB** (budget 1 200 KiB) |
| `files-app` in dist | 3.1 MB, libheif (1.4 MB wasm) loaded only for a HEIC preview (worker, lazy) |
| rynasd RSS at rest, `FAKE_USB=1` (removable watcher running) | **19.0 MB** (< 20 MB, little headroom left) |
| rynasd CPU at rest, watcher every 2 s | 0 clock ticks in 30 s (< 10 ms), ~5 wakeups/s |
| rynas-web RSS at rest | 8.8 MB |
| G7 (page + every element with overflow-x, incl. shadow roots): `/hardware#removable`, USB popover, `/notifications`, `/files` (files_mock), it/en, 1280/1024/390 | **0** sideways scroll |
| Raw i18n keys / Synology-DSM-synodeb words (it, en; light + dark) | none besides the DS920+ "Synology" hardware tab (expected, G8d) |
| Event codes emitted by the backend (22) vs `nev.<code>.title` keys | all present |
| zstd memory, measured (`zstd -T3`, 200 MB incompressible) | level 3: 87 MB, level 9: 194 MB (the worker uses 8 MiB jobs: ~110 MB at "best", per its own comment) |

`scroll_audit.py` found no dialog to open on these routes (`dialog views 0`); the tab and popover
checks were done with `probe.py` and screenshots (`target/ui-audit/shots/e-*`).

## Findings

### High

**H1 — rynasd stopped whenever a file manager worker was OOM-killed (fixed, 084ccc2).**
Workers and transfer jobs run in rynasd's cgroup (`packaging/systemd/rynasd.service`,
`MemoryMax=256M`). A tar.zst at "best" takes ~110 MB and two jobs run at once, plus up to 12
request workers: the cgroup can reach `MemoryMax`, the kernel kills the biggest process (a worker),
and systemd's default `OOMPolicy=stop` then stops the whole unit — rynasd and every running task
(storage, apply, scheduler) with it, restarted 2 s later by `Restart=on-failure`.
Fix: `OOMPolicy=continue` (the job fails; if rynasd itself is killed its exit still restarts the
unit). The structural part is M2.

**H2 — Any signed-in user could fill rynasd's memory with chunked uploads (fixed, 75d40d1).**
`crates/rynasd/src/files.rs` (`handle`, default branch): a non-GET request without
`Content-Length` was read whole (`to_bytes(body, 80 << 20)`) **before** a worker slot was taken,
so N parallel chunked requests held N × 80 MiB in rynasd. rynas-web (`crates/rynas-web/src/files.rs`)
forwards chunked bodies as they are (only `Limited`, no buffering), and the route is open to every
user, not only administrators. Fix: 411 `bad_request` for a body without length (browsers always
send `Content-Length` with a fetch body); test in `crates/rynasd/tests/files.rs`.

### Medium

**M1 — Symbolic links lead out of the shared folders (and admins read everything through them).**
`crates/rynas-files/src/shares.rs:7,76` checks only the components of the virtual path; links inside
a share are followed "as SMB does". Samba's default is `wide links = no` (rynas sets nothing else),
so the file manager is wider than SMB. Any user can plant a link (extract a tar with
`etc -> /`, worker.rs `extract`/copy keeps links) and then browse the whole system with their own
permissions (every world-readable file). An administrator's worker keeps `CAP_DAC_READ_SEARCH`, so
through such a link it reads `/etc/shadow`, `/var/lib/rynas/auth` (TOTP secrets) and rynasd's
connection key (the sealed SMB/SFTP passwords of every user) — beyond "admins look at every
*folder*" in `caps.rs`. Fix: in the worker, open paths with `openat2(RESOLVE_BENEATH |
RESOLVE_NO_MAGICLINKS)` from the share root's fd (Linux ≥ 5.6), or canonicalize and require the
result to stay under the canonical share root (any share root, to keep links between shares);
refuse otherwise with `invalid_value`. Update the doc comment and `docs/plugins/files.md`.

**M2 — Workers share rynasd's memory limits: MemoryHigh=96M throttles rynasd during big jobs.**
`rynasd.service:26-27`. One tar.zst job at the default level (~87 MB measured) already exceeds
`MemoryHigh` with rynasd's own 19 MB, and page cache from file copies is charged to the same cgroup:
the kernel then throttles every process of the cgroup, rynasd included (slow API, slow events),
for the length of the job. Fix: run workers in a cgroup of their own — `Delegate=yes` and move each
worker into `rynasd.service/workers/` (memory.high ~192M, memory.max ~384M on a 4 GB box), or start
them with `systemd-run --scope --slice=rynas-files.slice -p MemoryMax=…`; keep rynasd in a leaf
`rynasd.service/daemon/` with today's limits. Also cap concurrent tar.zst at "best" to one.

**M3 — Removable ext4/btrfs volumes are open to every NAS user through the file manager.**
`crates/rynasd/src/files.rs:180` (`shares_for`) offers every mounted removable volume to **every**
user. FAT/exFAT/NTFS are protected by `gid=<access group>,fmask=0117,dmask=0007`
(`rynas-removable/src/mount.rs:54`), but ext4/btrfs keep their own owners and modes: world-readable
files of a stick are readable by any account, and files owned by uid 1000 on another machine are
*writable* by whoever is uid 1000 on the NAS. SMB restricts the same volumes to `@group`
(`smb.rs`). Fix: list removable volumes only to administrators and members of the access group
(`settings.share.group`), the same rule as SMB; optionally mount ext4/btrfs below a
`0750 root:<group>` directory (`/media/.rynas/<name>` with `/media/<name>` as a bind or symlink) so
the kernel enforces it for every path.

### Low

- **L1** `crates/rynasd/src/files.rs` routes answered by rynasd itself (`prefs`, `conns*`,
  `jobs/new`) buffer up to 4 MiB (`MAX_OWN_BODY`) before any limit on concurrency: smaller than H2
  but unbounded in count. Take a permit of `files.slots` (or a small semaphore) before `own_body`.
- **L2** `conns/test` (`files.rs`, `"conns/test"` arm) accepts GET as well as POST; GET skips the
  CSRF check in rynas-web. It reads no body on GET so nothing is tested today, but add `need_post`
  like the other changing routes.
- **L3** SMB include (`rynas-removable/src/smb.rs:64`): `path`/`comment` may contain `%` from a
  label, which Samba expands (`%u`, `%m`…). Map `%` to `_` in `mount::clean_name` and drop it from
  the comment.
- **L4** Removable watcher polls every 2 s (`rynasd/src/removable.rs:54`). Cheap (measured
  < 10 ms / 30 s) but ~5 wakeups/s on an idle NAS; a `NETLINK_KOBJECT_UEVENT` socket for block
  events with a 30 s poll as fallback would idle the J4125 completely.
- **L5** `crates/rynas-files-worker` is a workspace of its own (own `Cargo.lock`): the playbook's
  `cargo test/clippy --workspace` does not cover it. Add the two `--manifest-path` commands to
  `tools/ui-audit/README.md` / the review playbook and to any CI script.
- **L6** `files-app` at 390 px: the toolbar filter field shrinks to "Filtr…" and share names to
  "docume…"; hide the filter label behind its icon below ~480 px.
- **L7** rynasd RSS at rest is 19.0 MB with the removable watcher (wave D: 17.5 MB): within budget,
  but the next resident feature needs to free something first.
- **L8** The view CSP (`rynas-web/src/static_files.rs:46`) replaces the worker's preview CSP and
  drops its `sandbox` (needed for PDFs in an iframe, presumably). Scripts stay blocked
  (`default-src 'none'`, global `nosniff`); document why `sandbox` is absent next to the constant.

## Checked, no finding

Worker privilege drop (`identity::become_raw`: setgroups/setresgid/setresuid, bounding set,
`no_new_privs`, never uid 0, `check_worker` refuses any capability but reading, `PR_SET_DUMPABLE 0`);
path parsing (`..`, `.`, NUL, absolute members via `valid_member`); upload part file opened with
`O_NOFOLLOW`; thumbnail dir created with `symlink_metadata` + `lchown`; connection store 0600/0700,
AES-GCM; removable and eject routes administrators-only (rynas-web `require_admin`, rynasd
`eject`); CSRF on every non-GET `/api/v1/files/*` (`require_session`); LUKS passphrase only on
`cryptsetup --key-file -` stdin, request type without `Debug`, length 1–512, never in argv, task
log or config, cleared in the UI after use; mount options `nosuid,nodev,noexec,noatime`; SMB
include: `valid users = @group`, `guest ok = no`, group validated as an account name, labels
without control characters; file downloads `application/octet-stream` + attachment, previews of
HTML/JS as `text/plain`, CSP without inline scripts on the page; notification event codes all
translated.

## Commits

- `084ccc2` packaging: rynasd keeps running when a file manager worker is OOM-killed (H1)
- `75d40d1` files: refuse bodies without Content-Length instead of buffering them (H2)
