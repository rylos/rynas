# Plugin `files` — file manager

Decision D-FM (`../phase0/05-decisions.md`): the SynoFile backend extracted into rynas, the
SynoFile interface first ported as a Web Component, then rewritten in Leptos piece by piece. This
file has two parts kept apart to ease merging: the backend (routes, worker, transfers as tasks)
and the UI below.

## Backend

Status (2026-11-06): ported and wired end to end (rynas-web → rynasd → worker), tests below.
SynoFile is MIT and by the same author; its Rust code was moved, not rewritten.

### Features (all of SynoFile's, adapted)

| Feature | In rynas |
|---|---|
| Browse shares, `home`, details, media info (EXIF, MP4/MOV/MKV tags; codecs, bitrate, duration with ffmpeg when installed) | `list`, `stat`, `media`, `init`/`shares` |
| Copy / move / delete with `.name.rynas-part` + rename, btrfs reflink in one filesystem, rename for moves, xxHash3 verify, conflicts (ask / overwrite / skip / keep both / newer), pause, cancel, live speed limit, speed series, current file, error list | transfer jobs = rynas tasks (`files.copy`, `files.move`, `files.delete`, `files.compress`, `files.extract`) |
| Transfers that survive the browser; after a rynasd restart the partial files are cleaned and the job comes back interrupted with Retry | journal in `/var/lib/rynas/files/journal/` (no secrets) |
| Recycle bin | own `#recycle` per share, Samba `vfs_recycle` layout (see below) |
| Archives as folders (zip, 7z, iso, cab, jar, rar, tar, tar.gz/tgz, tar.bz2/tbz2/tbz, tar.xz/txz, tar.zst/tar.zstd/tzst/tzstd): browse, preview, download one item, extract all or selected items, password archives (password on stdin, never on the command line) | 7-Zip, GNU tar (with gzip, bzip2, xz or zstd, which it runs by name), unrar; a missing program fails only its feature with `tool_missing`, naming the program, before anything is read or unpacked |
| Compress to `.zip` (own streaming writer, zip64, UTF-8 names) or `.tar.zst` (see below), download folders and selections as a zip (or tar) stream | `jobs/new` `op: compress`, `format: zip \| tar.zst`, `download?paths=` |
| Space analysis (rings), folder usage | `space`, `usage` |
| Text editor: load/save with an mtime+size stamp guard (409 `conflict`), permissions, owner and group kept, symlinks saved on their target, hard links written in place | `edit`, `edit/save` |
| Search below a folder (words or a glob) | `search` |
| Thumbnails | own cache per user, `@eaDir` of migrated volumes read when present, ffmpeg (optional) |
| Resumable chunked uploads (chunks in any order, idempotent last chunk, completeness checked with `SEEK_HOLE`, abandoned parts swept after 24 h, 507 on a full volume) | `upload`, `upload/abort`, `exists` |
| SMB 2/3 and SFTP connections, encrypted stored credentials (AES-256-GCM), own SSH key per user, SFTP host key pinned on first use, browse/preview/edit/upload/download/copy both ways | `conns*`, any route with a `/:<id>/…` path |
| Favourites, preferences, per-folder view and sort, folder icon and colour | `prefs` (a JSON document the interface owns, ≤ 256 KiB) |
| USB eject | `eject`, through the removable-drives module (seam) |

### Processes and security model

```
browser ──HTTPS── rynas-web (user rynas) ──Unix socket── rynasd (root)
                     session, CSRF,             │ x-rynas-user: alice
                     streams both ways          │ fork + setgroups/setresgid/setresuid
                                                ▼ + no_new_privs, then exec
                                     rynas-files-worker (uid alice)
                                     serve: one request · job: one transfer · call: SSH chores
```

- **Every file operation runs as the signed-in Linux user.** rynasd starts the worker through
  `rynas_files::identity::become_raw` between fork and exec: supplementary groups, gid, uid
  (refused for uid 0), `PR_SET_NO_NEW_PRIVS`, the bounding set reduced, then every capability
  dropped. The kernel applies the user's permissions and POSIX ACLs — the same ones rynas renders
  for SMB and NFS from the share matrix.
- **Administrators read every folder, writes stay theirs** (SynoFile 0.8.12): for a member of
  the rynas admin groups (`--admin-group`, default `rynas-admin`, `sudo`; `AdminPolicy`) the worker
  keeps `CAP_DAC_READ_SEARCH` only (effective, permitted, inheritable and ambient, so 7z and ffmpeg
  read too). Other users keep nothing.
- The worker checks itself first (`caps::check_worker`): never root, nothing but reading,
  `PR_SET_DUMPABLE=0` (nobody attaches a debugger to borrow the reading capability). The test
  `packaging/test-files.sh` reads `/proc/<pid>/status` of live workers.
- No file capabilities, no setup step: `rynasd.service` already has `CAP_SETUID`,
  `CAP_SETGID`, `CAP_DAC_READ_SEARCH` in its bounding set; `NoNewPrivileges=yes` is inherited by
  the workers.
- **Paths never leave a share**: a virtual path is `/<share>/<plain components>`; `..`, `.`,
  NUL and absolute parts are refused (400 `invalid_value`, `field: path`), a share name is
  matched exactly. Real paths never go to the interface (`Share::public`).
- **Symbolic links stay inside their share** (`rynas_files::confine`), as Samba with its default
  `wide links = no`: a link is followed while its target stays in the same share (relative or
  absolute, chains included), and refused with 403 `outside_share` when it leads to the system,
  to another share or to a home. Without this an administrator's worker, which keeps
  `CAP_DAC_READ_SEARCH`, would read `/etc/shadow` or rynasd's stores through a link any user can
  plant (an archive with `etc -> /`).
  - Resolution: from the share's opened directory with `openat2(RESOLVE_BENEATH |
    RESOLVE_NO_MAGICLINKS)` (Linux 5.6; Debian 13); where the kernel lacks it (ENOSYS), and for
    absolute links pointing inside the share (which the kernel refuses beneath a root), a walk of
    one component at a time with `O_NOFOLLOW` from the folder opened before, `..` of link targets
    never above the share. The result is a real path without links.
  - Races: the worker holds a jail for its request or job (the share roots its paths name);
    every file and folder it reads is opened with `O_NOFOLLOW` and refused unless the descriptor
    is really inside one of them (`/proc/self/fd`), so a folder swapped for a link after the
    resolution is caught. Files it makes (`create_new` part files, archives, the editor's
    temporary file) are checked the same way and removed when they landed elsewhere.
  - Per route: `list` shows a link out of the share as a link (`outside: true`, never stat'ed
    through: no `link_dir`), `stat` gives its target text with `readable`/`writable` false;
    `list`, `view`, `download`, `text`, `edit`, `edit/save`, `thumb`, `media`, `space`,
    `search`, `mkdir`, `upload` refuse it. Zip and tar downloads, `.tar.zst` and zip archives,
    copies and moves keep links as links (never followed); `usage` and `search` do not go down
    links. The editor saves the target of a link that stays inside (the link remains a link).
    ffmpeg (thumbnails, media info) reads the checked descriptor (`file:/proc/self/fd/3`), never a
    path.
  - Jobs: `jobs/new` sends the share roots (`Spec::roots`); the worker resolves each source as
    the item itself (a selected link is copied, moved or deleted as a link) and the destination
    followed, both inside their share; a source out of it is skipped with `outside_share`, a
    destination out of it fails the job. The recycle bin's folders are made one by one and must be
    real folders (a `#recycle` link fails the deletion, nothing is deleted for good). The
    extraction tools run under Landlock (Linux 5.13) with every write allowed below their own
    folder only, so an archive's `a -> /etc` followed by `a/passwd` writes nothing out of it;
    without Landlock the tools' own guards apply (GNU tar delays links that go up or are
    absolute), and what they made is moved into place without following links.
  - External programs (7z, unrar, tar, ffmpeg) run without the administrator's capability (exec
    drops it): the worst a race gets them is what the user may read anyway.
- **Stores** (`/var/lib/rynas/files/`, root 0700, only rynasd reads them): `<user>/prefs.json`,
  `<user>/conns.json` (passwords and imported keys sealed with AES-256-GCM under `conns.key`, made
  once under a lock and never replaced), `<user>/id_ed25519{,.pub}`, `journal/`. A worker gets
  only what its request needs, in its context line (the connections a path names, with their
  secrets).
- **rynasd links no SSH or SMB library**: learning an SFTP server's key, making the user's
  key, opening an imported key run in a worker as the user (`rynas-files-worker call`); rynasd
  stores the result. A worker never pins a key itself: an unknown key is pinned by rynasd at
  save, test or job submission (trust on first use), a changed key is refused
  (`host_key_changed`).
- Resource bounds: at most 12 request workers at once (the others wait, then 503 `busy`), 2
  transfer jobs at a time (deletions start at once), ffmpeg with one thread; jobs kept 24 h, 200
  in memory, 2400 samples and 500 errors each (M1).
- **Workers in a cgroup of their own** (`rynasd::cgroup`): `rynasd.service` delegates its cgroup
  (`Delegate=memory pids`, `ProtectControlGroups=private`); at start rynasd moves itself into the
  leaf `daemon/` (96M high / 256M max) and opens `workers/` (192M / 384M; tunable with
  `RYNAS_CGROUP_DAEMON` / `RYNAS_CGROUP_WORKERS` in the unit), and every worker moves itself
  there between fork and exec (one write of "0" to `cgroup.procs`, before the identity
  change). A `.tar.zst` job or the page cache of a big copy is reclaimed and throttled in
  `workers/`, never in rynasd's leaf; a worker killed at `workers/`'s `memory.max` fails its
  request or job and nothing else (`OOMPolicy=continue`). The unit's `MemoryHigh=288M` /
  `MemoryMax=640M` and `TasksMax=256` bound the sum. Without delegation (an older systemd, a
  container, `--dry-system`, the tests) everything stays in one cgroup and works the same.
  Cost, measured: spawn and wait of a process ~300 µs, ~460-560 µs when it moves itself into the
  leaf; a `systemd-run --scope` per process (the other way to give workers their own limits)
  ~8 ms.

**Latency: per-request spawn, no warm worker.** A request is a fork + exec of a ~3 MB static-ish
binary plus one JSON line; see "Measurements" below. Keeping a worker per user warm would save
little and would keep processes with the reading capability alive between requests.

### Two crates, two lock files

- `crates/rynas-files` (workspace member): the core — `api` (local routes), `wire` (worker
  request line and gateway answer), `fsops`, `archive`, `zip`, `media`, `thumbs`, `space`, `edit`,
  `names`, `transfer` (downloads, uploads), `shares`, `identity`, `caps`, `store`, `conns`,
  `conn_routes`, `jobs::{manager, protocol}`. rynasd links it.
- `crates/rynas-files-worker` (its own `[workspace]` and `Cargo.lock`, excluded from the root
  workspace): the binary, the SMB (`smb` 0.12) and SFTP (`russh` 0.54, `russh-sftp`) clients, the
  remote routes and the transfer engine. Reason: `smb` pins release candidates of RustCrypto crates
  (`curve25519-dalek 5.0.0-rc.1`, `p256 rc.14`…) and `russh` 0.54 pins `base64ct ~1.6`; no `russh`
  release resolves together with both `smb` and the workspace's `argon2 0.6` (checked 0.54–0.65).
  Build, test and lint it with `--manifest-path crates/rynas-files-worker/Cargo.toml`;
  `cargo test --workspace` does not reach it, but rynasd's `tests/files.rs` builds it and drives
  it end to end. When `smb` releases on final crypto crates, the worker can rejoin the workspace.

### Transfers as rynas tasks

`jobs::manager` (in rynasd) keeps SynoFile's job table: queue, two running jobs, a worker per
job reporting progress lines on a pipe, commands (pause, resume, cancel, limit, answer) on its
stdin. When a job starts, rynasd opens a rynas task (`files.copy`… with the job's owner as user):
UPID, task panel, `task.*` events on `/events`, progress in percent, the first log line describes
the job, per-item errors go to the task log. Cancelling the task cancels the job; cancelling the
job from the transfer dialog marks the task cancelled. The richer state stays on `jobs` (speed
samples, current file, conflict waiting for an answer, pause, limit, errors) and the job carries
its `task_id`.

After a rynasd restart: rynas-tasks marks the running task failed ("interrupted"); the files
journal shows the job as `failed` with `interrupted: true`, `code: interrupted`, runs a `cleanup`
worker as the user (removes `.name.rynas-part`, `.rynas-x-*` extraction folders and day-old
`.rynas-upload` files in the destination and in the trees the job was writing — and SynoFile's
`.synofile-*` names, for volumes that come from a NAS that ran it), and Retry runs the same job
again (the secrets reloaded from the store).

### Archives the file manager writes

The Compress dialog offers two formats; the name's extension follows the choice.

- **zip** (default; opens anywhere): the own streaming writer, levels normal (deflate 6), fast
  (deflate 1) and store; photos, videos, music and archives are always stored.
- **tar.zst** (`format: "tar.zst"`): written in the worker, no program spawned — the `tar`
  crate (GNU headers, long names and long link targets as GNU long-name entries) through the
  `zstd` crate on the system libzstd, with the zip's walk, progress, pause, cancel, live speed
  limit and `.name.rynas-part` + rename (a cancel removes the part file). Every entry keeps its
  mode, owner ids and modification time; symbolic links are stored as links, never followed
  (in or out of the share); empty folders are kept; sockets, pipes and devices are left out;
  a file that shrinks while it is read is padded to its announced size and reported, one that
  grows is cut, so the archive stays readable. A name already taken becomes
  `name (1).tar.zst` (the compressed tar's extension counts as one, for copies too).
  Levels: fast = zstd 1, normal = 3 (zstd's default), best = 9; `store` (a zip level) gets 3.
  Why: measured with three workers on 300 MB of `/usr` files, level 3 makes the archive in a
  fifth of level 9's time and goes faster than a disk writes even on a 4-core Celeron; 9 is
  about 9% smaller; above 9 each step costs many times the time for a few percent (19 is some
  85 times slower than 3), so it is not offered. Incompressible data (media, archives) passes
  at full speed, so nothing needs storing apart. Threads: zstd workers = cores − 1 (a core stays
  for Samba and the web interface; 3 on a J4125), at most 4, in jobs of 8 MiB, which halves the
  memory at level 9 (about 110 MB peak instead of 175) for 0.2% of size; a libzstd built
  without threads compresses in the worker's own thread. The archive carries zstd's checksum.
- Remote destinations and sources: refused for both formats (`invalid_value`, "copy the files
  to the NAS first"), as before.

### Recycle bin

A share whose SMB recycle bin is on (`smb.enabled && smb.recycle.enabled`, the same shares the
`smb.recycle-cleanup` task empties) gets deletions moved, as the user, to
`<share>/#recycle/<folders of the item>/<name>`, exactly as Samba's `vfs_recycle` does with
rynas's settings (`repository = #recycle`, `keeptree`, `versions`, `touch`, `directory_mode =
0770`): a taken name becomes `Copy #1 of <name>`, `Copy #2 of <name>`…, the access time is the
deletion time and the rename sets the ctime the cleanup reads. Items already in a bin or in
`@eaDir` are deleted for good. When the bin was asked for and cannot take an item (another
filesystem, a link out of the share), the item is left alone with an error — never deleted for
good. `permanent: true` deletes for good.

### Wire between rynasd and the worker

`rynas-files-worker serve` reads one JSON line (`wire::WorkerRequest`: `method`, `route`,
`query`, `length`, `range`, `ctx` = `rynas_files::Context`: user, admin, shares with real paths,
volume names, hostname, connections, rynasd's part of `init`, thumbnail cache) and then `length`
bytes of body on stdin, and answers in the gateway form (`Status: 200 OK`, headers, empty line,
body) on stdout. rynasd parses the head (≤ 64 KiB, within 120 s — 600 s for `upload` and
`edit/save`) and streams the rest; the worker is killed if the client goes away. A worker that
answers without reading its body reads up to 80 MiB of it away first, so a refused chunk is an
answer, not a cut connection.

### HTTP API

Base `/api/v1/files/` (rynas-web) = `/v1/files/` (rynasd), every method, behind the session;
every method but GET/HEAD/OPTIONS needs `X-CSRF-Token` (A23). Open to every signed-in user, not
only administrators: the worker runs as that user (today rynas-web still lets only
administrators sign in). Route names, query parameters and JSON bodies are SynoFile's
(`src/api.rs`, `remote/api.rs`, `ui/js/api.js` of SynoFile). Paths are virtual: `/<share>/…`,
`/:<connection id>/…` for remote servers.

| Method | Route | Parameters / body | Answer | Where |
|---|---|---|---|---|
| GET | `health` | — | `{version, worker}` (no user needed) | rynasd |
| GET | `whoami` | — | `{user, version, asset: null, setup: true, daemon: true, admin}` | rynasd |
| GET | `init` | — | `{whoami, prefs, shares: <shares answer>, conns: <conns answer>}` | rynasd + worker |
| GET | `shares` | — | `{shares: [Share + free, total, readable, writable], user, volumes: {mount: name}, hostname}` | worker |
| GET | `list` | `path` | `{entries: [Entry], writable, space: {free,total}\|null, recycle}`; in an archive `{entries, writable: false, space: null, recycle: false, inner, total}`; remote `{…, remote: {kind, top}}` | worker |
| GET | `stat` | `path` | `Details` (`name, kind, size, mtime, atime, ctime, mode, owner, group, inode, links, target, outside, readable, writable, device`) | worker |
| GET | `media` | `path` | EXIF/track facts, `{}` when none | worker |
| POST | `usage` | `{paths}` | `{bytes, files, dirs, unreadable, partial}` (25 s budget) | worker |
| GET | `space` | `path` | rings (40 s budget) | worker |
| POST | `mkdir` | `path`, `name`, `parents=1` (a relative path for folder uploads) | `{ok}`; 409 `already_exists` | worker |
| POST | `rename` | `path`, `name` | `{ok}`; 400 `share_root` on a share | worker |
| GET | `search` | `path`, `q`, `limit` (≤ 10000), `budget` (1–60 s) | `{hits: [{dir, …Entry}], scanned, partial}` | worker |
| GET | `download` | `path`, or `paths` (JSON array), `format=tar` | the file (ranges), one archive item, or a zip/tar stream | worker |
| GET | `view` | `path` | inline, ranges, sandboxing CSP | worker |
| GET | `thumb` | `path`, `size=big` | JPEG, `Cache-Control: private, max-age=86400`; 404 when none | worker |
| GET | `text` | `path` | `{text, binary, size, truncated}` (first 512 KiB) | worker |
| GET | `edit` | `path` | `{text, stamp, writable}`; 413 over 8 MiB, 415 not UTF-8 | worker |
| POST | `edit/save` | `path`, `stamp`, `force=1`; body = the text | `{stamp}`; 409 `conflict` when changed meanwhile | worker |
| POST | `upload` | `dir`, `name`, `id` (8–32 `[0-9a-z]`), `offset`, `total`, `mtime`, `overwrite=1`, `newer=1`; body = the chunk (≤ 80 MiB, the interface sends 16 MiB) | upload state | worker |
| POST | `upload/abort` | `dir`, `name`, `id` | `{ok}` | worker |
| POST | `exists` | `path`; body = `[names]` | `{taken: [{name, size, mtime, dir}]}` | worker |
| GET/POST | `prefs` | POST body = the document | the document / `{ok}`; 413 over 256 KiB | rynasd |
| GET | `conns` | — | `{conns: [Public], key}` (no secrets) | rynasd |
| POST | `conns/save` | connection form | `{conn}` | rynasd (key work in a worker) |
| POST | `conns/test` | connection form | `{ok, items, ms, home, host_key, path}` | rynasd + worker |
| POST | `conns/delete` | `{id}` | `{ok}` | rynasd |
| POST | `conns/order` | `[ids]` | `{ok}` | rynasd |
| POST | `conns/key` | `{}` | `{key}` (public half, made on first use) | rynasd (+ worker) |
| GET | `jobs` | `since` (ms) | `{jobs: [JobView], now}` | rynasd |
| POST | `jobs/new` | `{op, sources, dest, conflict, verify, limit, permanent, name, level, format, into_folder, password, members}` | `{id}` | rynasd |
| POST | `jobs/control` | `{id, action: pause\|resume\|cancel\|answer\|limit\|start\|retry\|dismiss, policy, all, bps, conflict, password}` | `{ok}` | rynasd |
| POST | `eject` | `{share}` (or `?path=/share`) | `{ok}`; administrators only; 409 `busy` while in use or transfers run; 501 `not_available` without the removable-drives module | rynasd |

Shapes added to SynoFile's (optional fields, nothing removed): `Share.removable`, `device`
(disk, `sdc`), `device_name` (vendor + model); `JobView.code`, `JobView.task_id`;
`ErrorItem.code`; `whoami.admin`; `Entry.outside`, `Details.outside` (a link out of its share);
`Spec.roots` (worker side).

### Deviations from SynoFile's contract

| SynoFile | rynas | Why |
|---|---|---|
| `api.cgi/<route>` | `/api/v1/files/<route>` | rynas API base |
| errors `{"error": "<translated sentence>"}`, 403/404/5xx sent as 422 + `X-Synofile-Status` | rynas `ApiError {code, message, field?}` with the **real status** | rynas rules A8–A11; no web server hides bodies here |
| 401 with `X-Synofile-Auth` | rynas-web's 401 `unauthenticated` (session), 403 `csrf` | rynas session |
| messages in the user's language (`lang` parameter) | English `message`, stable `code` (the `lang` parameter is ignored) | the interface translates codes |
| job `msg` translated, `msg = "need_password"` | `msg` English (`"need_password"` kept as is) + `code` (`password_required`, `cancelled`, `interrupted`, an error code; `""` when done) | same |
| `whoami.setup` / `daemon` reflect setup and daemon state | always `true` | no setup step, no separate daemon |
| `eject` through the host's own API | `POST eject {share}` | new route |
| shares from the host's share list, `homes` share | applied `shares` configuration + the user's home as `home` | rynas config |
| volume names from the host's storage status | `volumes` keyed by mount point, named after the rynas volume of the share's subvolume, else the mount's last component | rynas storage |
| thumbnails only from `@eaDir` | own cache first, `@eaDir` second, ffmpeg third | generic first |
| `.name.synofile-part`, `.name.<id>.synofile-upload`, `.rynas-x-*` | `.name.rynas-part`, `.name.<id>.rynas-upload`, `.rynas-x-*`; the old names are hidden and cleaned | rename, compatibility with migrated volumes |
| recycle through the host's recycle tool (`name_HHMMSS.ext`) | own move, Samba `vfs_recycle` layout (`Copy #N of name`) | the bin SMB uses |
| only administrators | every signed-in user (admins read everywhere) | rynas roles |

### Error codes

The codes the file manager answers (constants in `rynas_api_types::error_codes`, English
`message` beside; the interface translates the code):

| Status | Code | When |
|---|---|---|
| 400 | `invalid_value` (`field`: `path`, `name`, `sources`, `dest`, `members`, `private_key`) | bad path or name, nothing selected, not a folder / file, no destination |
| 400 | `bad_request`, `invalid_json` | missing parameter (`field`), malformed body |
| 400 | `share_root` | a shared folder renamed, copied, moved or deleted |
| 400 | `password_required` | archive needs its password (also the job's `code`) |
| 400 | `remote_auth_failed` | a server refused the login (never 401) |
| 403 | `permission_denied` | the filesystem refused (user's permissions, ACLs) |
| 403 | `outside_share` | a symbolic link leads out of its shared folder (listed, never followed); also an item of a job's error list |
| 403 | `forbidden` | eject by a non-administrator; unknown user |
| 404 | `not_found` | no such item, share, connection, transfer, thumbnail |
| 405 | `method_not_allowed` | |
| 409 | `already_exists` | name taken (mkdir, rename, upload without overwrite) |
| 409 | `conflict` | editor: changed meanwhile; transfers: no conflict waiting / already settled |
| 409 | `read_only` | inside an archive, read-only snapshot or filesystem |
| 409 | `host_key_changed` | SFTP server key differs from the pinned one |
| 409 / 503 | `busy` | clean-up of an interrupted job still running; drive in use; all worker slots taken |
| 413 | `payload_too_large` | body over its limit, editor file over 8 MiB, prefs over 256 KiB |
| 415 | `unsupported_media_type` | editor: binary or not UTF-8 |
| 502 | `remote_unreachable` | server down or connection broken |
| 501 | `not_available` | eject without the removable-drives module |
| 503 | `tool_missing` | 7z/unrar/tar missing, or the decompressor GNU tar runs (gzip, bzip2, xz, zstd) |
| 504 | `timeout` | worker did not answer in time |
| 507 | `insufficient_storage` | volume full or quota used up |
| 500 | `internal` | anything else (log) |

Differences with the interface's table (`files-app/js/i18n.js` `ERRORS`): the backend never
sends `exists`, `invalid_name`, `no_space`, `quota_exceeded`, `too_large`, `not_supported`
(it sends `already_exists`, `invalid_value` + `field: name`, `insufficient_storage` for both
full volume and quota, `payload_too_large`, `invalid_value`); the interface has no text yet for
`share_root`, `password_required`, `tool_missing`, `remote_auth_failed`, `host_key_changed`,
`unsupported_media_type` (it falls back to the English `message`).

### rynas-web

`crates/rynas-web/src/files.rs`: `/api/v1/files/{*route}`, any method, inside the session layer
(not the admin-only one). The request body streams to rynasd (`RynasdClient::send_stream`,
capped at 80 MiB: `MAX_UPLOAD`, 413 at once when `Content-Length` says more); `Content-Type`,
`Content-Length` and `Range` go along. The answer streams back with `Content-Type`,
`Content-Length`, `Content-Disposition`, `Content-Range`, `Accept-Ranges`,
`Content-Security-Policy`, `Cache-Control`, `Last-Modified`. The dynamic gzip layer skips file
contents (`NotFileContent`: answers with `Content-Disposition`, `Accept-Ranges` or
`Content-Range`), so ranges keep working and media is not recompressed. Deadlines: 130 s to the
answer head, 610 s for `upload` and `edit/save`.

### Packaging

- `/usr/libexec/rynas/rynas-files-worker` in the `rynas` package (`packaging/build-deb.sh`
  builds the worker crate with `--locked`).
- `rynasd.service`: `StateDirectory=… rynas/files`, `CacheDirectory=rynas rynas/files`
  (thumbnails, `/var/cache/rynas/files/thumbs/<uid>`, each owned by its user, 0700),
  `TasksMax=256`; capabilities unchanged (`CAP_SETUID`, `CAP_SETGID`, `CAP_DAC_READ_SEARCH`
  were already there).
- Recommends `7zip`, `zstd` (opening `.tar.zst`: GNU tar runs it; creating one needs no
  program, the worker links the system libzstd), `ffmpeg`; suggests `unrar`.
- rynasd options: `--files-worker` (default: next to rynasd), `--files-cache`.

### Tests

- `rynas-files` unit tests (55): path confinement (`..`, `.`, NUL, unknown share, exact names),
  names and temporary names (255 bytes, UTF-8 boundaries), error codes of I/O errors, the
  gateway answer, uploads (any order, idempotent, missing chunks, conflicts, symlinks never
  written through, sweeping), the editor (stamps, symlinks, hard links, mode/owner), the zip
  writer, archives listing and format detection (every tar extension incl. the zstd ones),
  the programs an archive needs, `parse_ffmpeg`, the key store (one key for concurrent first uses, a
  damaged key never replaced), thumbnails cache and `@eaDir`, job progress.
- `rynas-files-worker` (20 unit + 3 integration): recycle bin (target, Samba layout and
  versions, never deleting for good when the bin was asked for), clean-up of leftovers incl.
  SynoFile names, passwords off the command line, tool output parsing, archive names and
  zstd levels, a .tar.zst round trip (made by the job, listed and extracted with GNU tar: long
  names, links kept as links and not followed out, an empty folder, mode and mtime), a
  cancelled .tar.zst leaving no part file; through the real binary:
  interrupted jobs cleaned and offered again, a copy with a conflict answered, a cancelled copy
  leaving nothing behind.
- rynasd `tests/files.rs`: the whole API with the real worker (init, list, confinement, mkdir,
  chunked upload, download with range, zip stream, editor, search, prefs, a delete job into the
  bin as a `files.delete` task, share roots refused).
- rynas-web `tests/files.rs`: session, CSRF, 5 MiB streamed upload, 413, headers, no gzip.
- `packaging/test-files.sh` (Debian 13 container, rynasd as root): ownership of what workers
  create, permission denied for another user, an administrator reading but not writing, jobs and
  the bin as the user, the worker refusing root, `CapEff` and `NoNewPrivs` of live workers.

### Measurements

Per-request spawn (fork + exec of the 4.6 MB release worker, one JSON line, `list` of a
folder of 23 entries, answer read back), 300 runs on the development box (Ryzen 7 1800X):
**p50 0.81 ms, p99 1.00 ms**, Python subprocess overhead included. On the J4125 expect 3–4×:
still well inside the 150 ms budget of a tool-backed GET, and every list is fresh (no cache to
invalidate). To repeat on the NAS: a loop of `rynas-files-worker serve` with a request line, or
`curl -w %{time_total}` against `/api/v1/files/list`.

### Removable drives

`rynasd::files::RemovableDrives` is the only place the file manager learns about USB/eSATA
drives; `rynasd::removable::FilesDrives` implements it on the removable-drives module
(`../research/removable-drives.md`) and main wires it with `Files::with_drives`:
- `drive_of(mount)`: a rynas share on a removable drive gets `removable`, `device` (the
  drive's kernel name) and `device_name` (its title);
- `mounted()`: every mounted removable volume no share covers is listed as a shared folder of
  its own, named after its mount point's last component (`/media/STICK` → `STICK`, `-2`, `-3`
  on a clash), so the app bar's Open goes to `/files/STICK`. Only administrators and the members
  of the removable access group (`removable.share.group`, `access_gid()`) see them, the rule of
  the SMB share (`valid users = @group`): ext4 and btrfs keep the owners and modes of the machine
  the drive comes from, so the kernel alone would let any account read its world-readable files
  (and write those of a matching uid). FAT/exFAT/NTFS are mounted for that group anyway;
- `POST eject {share}` → `safely_remove(user, drive)`: starts the task `removable.remove`
  (unmount everything on the drive, power it off); busy or unknown drives keep their 409/404.
Without the module (`NoDrives`) nothing is removable and `eject` answers 501 `not_available`.

### Not done yet

- Workers have their own memory limits (`workers/`) but no `Nice=`/`IOWeight=` of their own
  yet: `cpu`/`io` could be delegated to `workers/` the same way.
- `whoami.daemon` is always true; there is no separate transfer service to be down.
- Remote routes `search` and `space` answer "not available" (as in SynoFile); remote thumbnails
  none.
- Non-administrator logins need rynas-web to open sessions for them (roles); the files routes
  are ready for it.
- Eject needs the removable-drives module (seam above).

### Best ideas kept from SynoFile

`.part` + rename (a copy cannot half-fail), reflink inside btrfs, the transfer dialog's data
(speed series halved past 2400 points, active time without pauses, conflicts side by side with
"keep both" names ready), jobs that never resume on their own after a restart but clean up and
offer Retry, uploads that resume and never write through a symlink, the editor's stamp and its
care for owner, mode and hard links, passwords on stdin only, sealed connection secrets with a
key never overwritten, SFTP keys pinned only by the privileged side, archives browsed without
extracting, extraction into a hidden folder then merged under the conflict rule.

## UI

Status (2026-11-06): phase 1 done in the UI. The SynoFile interface (vanilla ES modules,
~12.5k lines, CodeMirror 6, libheif) runs inside rynas as the Web Component `<rynas-files>` on the
Files page; it needs the files API (`/api/v1/files/*`) from the backend part to show data.

### Where things are

| What | Where |
|---|---|
| The component (ported code) | `crates/rynas-ui/files-app/`: `rynas-files.js` (custom element, shadow root, bootstrap), `js/` (the app), `app.css`, `fonts/eza-icons.woff2`, `vendor/codemirror` (MIT), `vendor/libheif` (LGPL-3.0, unmodified), `js/syntax` (from micro, MIT) |
| Build and serving | trunk copies `files-app/` into `dist/files-app/` (`<link data-trunk rel="copy-dir">` in `index.html`); rynas-web serves it like the rest of the UI (`--ui-dir`), the .deb ships it precompressed (`packaging/build-deb.sh` compresses every `.js`/`.css`/`.wasm`) |
| Leptos page | `crates/rynas-ui/src/pages/files.rs`: routes `/files` and `/files/*path`, the lazy script load, the bridge of window events |
| Checks | `crates/rynas-ui/src/files_check.rs` (texts translated, no other product named, only it/en, no undeclared sideways scroll); `tools/ui-audit/files_mock.py` (fake files API for screenshots) |

### How it is mounted

- **Lazy**: the wasm bundle does not grow. `pages/files.rs` adds
  `<script type="module" src="/files-app/rynas-files.js">` the first time the Files page opens
  (skeleton while it loads, an error box with Retry if it fails). `rynas-files.js` preloads the
  app's modules (`modulepreload`) so they come in parallel, not one import level at a time.
- **One shadow root for the life of the page**: `<rynas-files>` hosts a single element that
  carries the shadow root. Leaving the Files page and coming back finds the same tabs, transfers,
  open editor and quick look. Styles are isolated both ways (rynas `.sidebar`, `.menu`, `.btn`
  and SynoFile's same class names never meet).
- **Deep links**: `/files/<share>/<folder>` (attribute `path`) opens that folder: the tab that
  shows it, or a new one. rynas-web answers `index.html` for `/files/…` even when the last segment
  has a dot (a folder `photos.2024`).
- **Own window**: the "open in a window of its own" button opens `/files/<current folder>?window`;
  with `?window` the shell hides its bars (`.app.bare`), so the window is the file manager only.
- **Theme**: rynas tokens. SynoFile's colour variables are mapped onto rynas's (`--muted` →
  `--text-muted`, `--crit` → `--danger`, eza's palette → `--danger/--success/--warning/--accent/
  --accent-2/--info`), and custom properties inherit through the shadow boundary, so light and
  dark follow the shell with no colour of their own (the editor's code colours and the dark quick
  look overlay keep theirs). `data-theme` on `<html>` is watched for the parts that need it.
- **Language**: only Italian and English, following `<html lang>` (set by the shell from the
  user's choice), switched live. The texts system is kept: Italian strings in the code, English in
  `js/lang/en.js`; the seven other SynoFile tables are not shipped.
- **Layout**: `.sf` (the app root) is a size container: SynoFile's breakpoints follow the room
  the shell leaves, not the window, and fixed overlays (context menu, transfers drawer, toasts,
  editor) stay inside the Files page; dialogs are in the top layer as before. The page fills the
  height under the app bar and above the dock (task panel) and, on phones, the bottom bar.
- **Keyboard**: the app's shortcuts listen on its shadow root, so they work only while the focus
  is in the file manager and never fight the shell's (Ctrl K, Esc, forms); the shell's Ctrl K
  still works from inside the file manager.

### API, session, errors

- Base `/api/v1/files/<route>` with SynoFile's route names and JSON shapes (`init`, `list`,
  `stat`, `jobs`, `jobs/new`, `upload`, `view`, `download`, `conns/*`, …).
- The rynas session cookie travels with every request; requests other than GET/HEAD carry
  `X-CSRF-Token` (the shell sets the property `csrf` on `<rynas-files>`, also again after a new
  login). Requests made while the user is away carry `X-Rynas-Background: 1` (A24b), so the
  transfers' polling does not keep an unattended session alive.
- A 401 fires `rynas-files:unauth`: the shell shows its login screen over the page (route and
  file manager state kept); after the new login every pane reloads.
- Errors: the server's stable `code` is translated (`i18n.js` `ERRORS`, it/en), falling back to
  the server's `message` (rynas) or `error` (SynoFile), then to the status. Both real status codes
  and SynoFile's `422 + X-Synofile-Status` are understood.
- `view` answers (PDF in an iframe, images, media) get from rynas-web
  `X-Frame-Options: SAMEORIGIN` and a CSP without `script-src` (`static_files::FILE_VIEW_CSP`):
  framed by rynas only, and an HTML/SVG file opened there cannot run scripts. The UI's CSP allows
  `blob:` images and media (HEIC previews decoded to JPEG in a worker).

What the UI expects from the backend beyond SynoFile's contract:

| Need | Detail |
|---|---|
| `POST eject` `{share}` | unmount and power off the removable drive of a shared folder (SynoFile asked DSM's own API); 404 shows "cannot eject" |
| share fields | `removable: true` for shares on USB/eSATA drives (else the volume names `volumeUSB1`/`volumeSATA1` are recognised), optional `device` (shares of the same drive) and `device_name` (shown in the eject dialog) |
| error `code`s | the ones in `i18n.js` `ERRORS`: `forbidden`, `permission_denied`, `not_found`, `exists`, `conflict` (editor), `invalid_name`, `read_only`, `no_space`, `quota_exceeded`, `payload_too_large`, `not_supported`, `remote_unreachable`, plus the generic rynas codes |
| upload chunks | 16 MiB per request (`uploads.js` `CHUNK`): above rynas-web's 1 MiB default body cap, the route must document its own |
| `whoami.daemon` | `false` shows "the transfer service is not running" |

### What changed from SynoFile

- DSM loader (`synofile.js`, `tray.js`, `index.html`, `api.cgi`, `texts/`, `config`, icons) replaced
  by `rynas-files.js`; no SynoToken, `login.cgi`, `SYNO.SDS.*`, iframe/opener lookups,
  `postMessage` open requests, stale-build reload or Login Portal paths.
- Session: rynas login instead of DSM's; the DSM banners (package setup, Package Center, admins
  only) became neutral ones (no access, service not installed/not answering with Retry,
  transfer service down).
- Theme toggle and theme preference removed (the shell's theme); the "DSM taskbar icon"
  preference removed; the version/author credits removed from the settings dialog.
- "Open in File Station" removed. "Description…" and "Permissions…" of a shared folder open the
  rynas share editor (`/shares?edit=<share>&tab=0|3`: General, or Permissions with the explained
  effective permission) instead of DSM dialogs; SynoFile's own permission dialog (DSM ACL API) is
  gone.
- USB eject through the files API instead of DSM's External Devices API.
- A transfer that ends while another rynas page is shown becomes a rynas toast; browser
  notifications say "rynas". Other windows of the file manager are nudged through
  `localStorage` to poll at once.
- `@eaDir` (thumbnails left by another system on migrated volumes) joins the protected, greyed
  system folders with `#recycle`, `#snapshot`, `@tmp`, `@sharebin`.
- Texts reworded without DSM terms (Recycle Bin of the shared folder, the names given to the
  volumes, …); localStorage keys `rynas.files.*`, drag types `application/x-rynas-files*`.
- Lists, dialogs and panels scroll vertically only (G7); the columns view and the text preview
  are the two listed exceptions, wide by nature.
- "Comprimi…" opens "Crea un archivio" with a format choice: zip (levels normal, fast, store)
  or tar.zst (normal, fast, best, see *Archives the file manager writes*); the name's extension
  and the hint follow the format. `.tar.zstd`, `.tzst` and `.tzstd` open like `.tar.zst`
  (browsable, the archive icon, "Estrai in «name»" without the extension).

### Plan: the Leptos rewrite, piece by piece

The component stays the file manager until each piece is replaced; the order goes from what other
pages reuse to what is most tied to the component's internals:

1. **Shell pieces in Leptos around the component** — breadcrumbs/path bar and the sidebar
   (shares, favourites, connections) as Leptos components talking to the component through
   attributes and events; rynas-styled toasts only (drop the component's).
2. **Transfers as rynas tasks** — once the backend turns transfer jobs into rynas tasks (UPID,
   SSE `task.*`), the task panel shows them and the transfers drawer/dialog (speed chart, ETA,
   conflicts, pause, limit, retry) is rewritten as a Leptos `TaskRun` view; the component's
   polling goes away.
3. **Dialogs** — settings, connection editor, folder picker, conflict and confirm dialogs on
   `ui::dialog`/`ui::confirm`/`form::*`, so they get the rynas form rules (field errors, i18n
   keys in `i18n.rs`).
4. **Quick look** — images (zoom/pan), video, audio, PDF, text, archives as a Leptos overlay;
   HEIC stays a JS worker (libheif) called from Rust.
5. **The pane** — list (eza style), grid and columns views, selection, keyboard (incl. Vim keys),
   fuzzy filter, type-ahead, drag and drop, virtualised rows. The biggest piece; done last
   among the views because everything else plugs into it.
6. **Editor** — CodeMirror 6 stays (a JS library), wrapped by a small Leptos component that loads
   it lazily; the micro-style chrome (status line, keys help) in Leptos.
7. **Space analysis** — rings on `<canvas>` with the `chart.rs` conventions.
8. When nothing is left in `files-app/js` but the editor bundle and libheif, drop the custom
   element and the shadow root; the texts move to `i18n.rs` keys (`files.*`).

Each step keeps the routes and JSON shapes of the API, so the backend is not touched by the rewrite.
