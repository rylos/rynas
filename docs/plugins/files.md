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
| Copy / move / delete with `.name.rynas-part` + rename, btrfs reflink in one filesystem, rename for moves, xxHash3 verify on request (no global setting since 2026-11-12: "Copy / Move with verification to…", Shift+F5 / Shift+F6, "Paste with verification" Ctrl+Shift+V, the Alt-drop menu; plain copy and move do not verify; a verified move deletes the originals only after the check), conflicts (ask / overwrite / skip / keep both / newer), pause, cancel, live speed limit, speed series, current file, error list | transfer jobs = rynas tasks (`files.copy`, `files.move`, `files.delete`, `files.compress`, `files.extract`) |
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
- **Administrators can do everything in the shares** (like DSM's File Station): for a member of
  the rynas admin groups (`--admin-group`, default `rynas-admin`, `sudo`; `AdminPolicy`) the worker
  keeps `CAP_DAC_READ_SEARCH`, `CAP_DAC_OVERRIDE` and `CAP_FOWNER` (`caps::Powers::Admin`): it
  reads every file, and deletes, renames, moves and writes whatever the mode bits, ACLs and
  sticky bits say. Never `CAP_CHOWN`: what it makes is the administrator's (as before), existing
  files keep their owners. The powers are fenced:
  - rynasd starts that worker under a Landlock ruleset (`jobs::manager::worker_command`,
    `admin_fence`, `landlock::writes_below_all`) that allows writes only below the shared folder a
    writing request names (`rynasd::files::share_roots_of`: `mkdir`, `rename`, `edit/save`,
    `upload`, `upload/abort` — a share, the home or a removable drive, never an app's folder), the
    folders a job writes in (`Spec::write_roots`: its destination's share, the sources' for a
    move or a deletion), the user's thumbnail cache and archive listing cache
    (`/tmp/synofile-<uid>` in rynasd's private `/tmp`, made by rynasd: `archive::prepare_cache`).
    Reading requests get the powers with no share to write in. System folders, rynasd's stores
    and the apps' folders stay out of reach. Landlock ABI 3 (Linux 6.2, truncate) is required;
    below it, or without Landlock, the administrator's worker keeps reading only and writes as
    itself, as before. A job of the journal written before `write_roots` (or one that writes on a
    server only) also keeps reading only.
  - A read-only mount stays read-only, and so does a read-only btrfs snapshot (EROFS comes before
    any capability: `inode_permission` → `sb_permission`, `btrfs_permission`). The top of a
    read-only snapshot, whose folder lies in the writable subvolume above, is never renamed, moved
    or deleted (`fsops::refuse_snapshot`, in `rename` and in the jobs; 409 `read_only`), and the
    listing flags it (`readonly`) as before.
  - Only reading passes on to the programs the worker runs (7z, unrar, tar, ffmpeg: inheritable and
    ambient). The powers over owners never do: those programs parse archives and media from
    anyone, and Landlock does not cover chmod or extended attributes. They write into folders the
    worker made for them (`.rynas-x-…`), which are the administrator's.
  - Residual risk: Landlock does not fence chmod, utimes or ACL changes, so a compromised worker
    of an administrator (with `CAP_FOWNER`) could change the modes of files outside the shares it
    can name; the jail (`confine`) keeps every path the worker opens inside its shares.
  - `writable` flags (`list`, `shares`, `stat`, `edit`) are asked with the worker's own powers
    (`fsops::access`): access(2) — and faccessat(2) without `AT_EACCESS` — check with the real
    credentials and clear the effective capabilities of a uid other than 0
    (`access_override_creds`), so a 0755 folder of another uid would show as not writable and the
    interface would keep hiding Delete. faccessat2(2) with `AT_EACCESS` (raw system call, Linux
    5.8) keeps the capabilities and still answers EROFS on read-only places; without faccessat2
    (ENOSYS) the answer is access(2), or an existing item not written on a read-only volume.
    Checked in an unprivileged user namespace (`fsops::tests::an_administrators_worker_asks_with_its_powers`).
  - The interface needs nothing of its own: it already follows the server's `writable` and
    `readonly` flags (menus, Cut, Delete, Rename, editor, destinations), never owners or modes.

  Other users keep nothing, and their workers are unchanged.
- **The apps' data** (`/@apps/<folder>/…`, administrators, Docker setting `files_app_data`,
  `../phase2/docker-apps.md` §5.9): a worker that writes there keeps `CAP_CHOWN` as well
  (`caps::Powers::AppData`, never ambient) and runs under a Landlock ruleset that allows writes
  below those app folders only; no Landlock, no powers. What it makes takes the owner of the
  folder it lands in (`confine::adopt`), existing files keep theirs.
- **Threat model of the PCs' backups**: what follows keeps the received snapshots safe from
  accidental writes through the File app and through SMB/NFS shares (a share can never be
  over a backup); administrators with a shell or root, and rynas' own receive, are out of
  its scope.
- **The PCs' backups** (`/@backups/<pc>/…`, administrators only, one "Backups" entry in the
  sidebar): every folder of btrfs snapshots received from a PC (`rynasd::pc_backup`, agent and
  btrbk folders alike; `PcBackup::snapshot_folders`, read from the registry, no disk walk) is a
  **read-only share** `@backups/<pc>` (`Share.read_only`, `Share.backup`), added by
  `rynasd::files::add_backup_shares` (real folders only, never a link as root). Read-only is the
  server's:
  - every writing route (`rynas_files::api::WRITING_ROUTES`) naming such a share is refused
    with 409 `read_only` by rynasd before any worker and again by the worker
    (`api::refuse_read_only`); `job_spec` refuses a job whose destination is in one, and a
    delete or move with a source in one (copy out instead: copies, downloads, previews and
    thumbnails work, the thumbnail cache lies outside the share);
  - **the final guard** (the real protection): rynasd gives every administrator's request
    and job the PC backup folders as (real path, device, inode) (`Context.pc_roots`,
    `Spec.pc_roots`, from the registry and one stat each; nobody else gets them: the folders
    are root's alone). Before any write (mkdir, parents included; upload; edit save; rename,
    its folder and what it takes away; delete; move; the destination of a copy, extract or
    compress, and every existing folder a copy or extraction merges into) the worker opens
    the folder written in and refuses with 409 `read_only` when its real path lies in a PC
    folder or when it, or any folder above it up to `/` (`..` one step at a time), has the
    identity of one (`confine::refuse_pc_write`); what a removal takes away (rename, move,
    delete) may not be, by identity, a PC folder or any folder above one. A share that is a
    link to a backup, a home or an app folder holding one, a bind mount or the same btrfs
    mounted elsewhere all end there; jobs with a server at the other end guard every local
    write and removal the same way. Writes only: listings pay nothing; the identity of the
    last folder found clear is remembered. It fails closed: an administrator's worker (it
    reads everywhere) told nothing writes nothing, and whatever the guard cannot decide is
    refused. The manager asks rynasd for the folders each time an administrator's job starts
    (`Hooks::pc_roots`), so a retried or journaled job is guarded with today's list;
  - construction checks keep the setup clean, on canonical paths (real path of the deepest
    existing part, so links hide nothing): rynasd refuses a PC folder (new or adopted) that
    is, lies inside or holds a share, a user's home or the apps' data (Docker stacks and app
    data folders) (`pc_backup::share_overlap`, `clash`), and a share created or moved over,
    inside or above a PC folder (`services::refuse_pc_folder`), and checks the shares again
    at every apply, however they were staged (`pc_backup::shares_guard`: an edit, a restored
    revision, an imported archive). An install made before this is said on both sides (PC
    issue `shared_folder`, share warning `pc_backup_folder`; the PC issue also names a home
    or an apps' folder over it, looked at anew each time, so moving the homes folder or
    Docker's folders over a PC folder shows at once) and the share, the home or the app
    folder stays read-only in the file manager for administrators (`Share.holds_backup`). Two
    PCs never share a folder either, a link between them included;
  - copying a file out of a snapshot (`POST /v1/pc-backups/targets/{pc}/copy-out`, run as
    root) refuses a destination in or above any PC folder (their staging folders included)
    or in a share that overlaps one;
  - `/@backups` is not there (404) for anyone without PC folders in their shares;
  - the staging folder of a receive (`.incoming`, `shares::BACKUP_STAGING` =
    `pc_backup::STAGING_DIR`) is not there for an administrator's worker by any path: what an
    opened descriptor really is decides (its real path, or its folder's identity), and
    listings of a PC folder skip the entry (search, usage, space analysis, zip and tar
    downloads included);
  - copies out of a backup stay as private as their originals (`Spec.private_roots`): the
    worker narrows a copied file, before any byte is written, and a copied folder to no more
    mode bits than the source had, the owner's only when the copy's group differs from the
    source's (`keep_private`; with a default ACL the mask narrows; an inherited setgid stays);
    the owner keeps reading and writing while a file fills and is verified (a 0000 source), a
    folder while it fills; an archive made from backup files is 0600; ordinary copies keep the
    destination's usual modes. Copies to a server (SFTP, SMB) take the server's modes;
  - a PC's whole folder is never measured, analysed or zipped (`usage`, `space`, `download` of
    `/@backups/<pc>` → 400): it holds every snapshot, mostly the same data again and again.

  Listings there say `writable: false`, `read_only: true` and mark every entry `readonly`
  (search hits too); `stat` and the editor say `writable: false`. The interface
  (`files-app/js/backups.js`) hides New folder, Upload, Paste, Cut, Move, Rename, Delete,
  Compress, Extract here, Edit and Empty the bin, refuses at once a move out of a backup
  (Alt drop, F6, Shift+F6), hides Compute size, Space analysis and the zip download at `/@backups` and at a PC
  folder's top, and the network path and smb:// address in `/@backups` and `/@apps` (no SMB
  shares). Snapshot folders (`<prefix>.YYYYMMDD[THHMM[SS][±hhmm]][_N]`) show by their date
  first, as the rest of the interface writes dates ("13 nov 2026, 15:43 · @ (radice)",
  "Nov 13, 2026, 3:43 PM · @home", the PC's clock, `#2` for a second one in the same minute),
  in the list, grid, columns, info panel, path bar and tab, the real name beside it (hidden
  below 600 px, with the Permissions column) and as the tooltip, newest first when sorted by
  name (columns view too).
- The worker checks itself first (`caps::check_worker`): never root, no capability beyond the set
  of its level (nothing, reading, an administrator's, the apps' data: each holds the one before),
  effective within permitted, nothing but reading ambient, `PR_SET_DUMPABLE=0` (nobody attaches a
  debugger to borrow the capabilities). The test `packaging/test-files.sh` reads
  `/proc/<pid>/status` of live workers.
- No file capabilities, no setup step: `rynasd.service` already has `CAP_SETUID`,
  `CAP_SETGID`, `CAP_DAC_READ_SEARCH`, `CAP_DAC_OVERRIDE`, `CAP_FOWNER` and `CAP_CHOWN` in its
  bounding set; `NoNewPrivileges=yes` is inherited by the workers.
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
  - External programs (7z, unrar, tar, ffmpeg) run without the administrator's powers over owners
    (only reading is ambient): the worst a race gets them is what the administrator may read.
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
  `workers/`, never in rynasd's leaf. So that a copy is not stopped in bursts by that limit,
  the worker sends each window of a file to the disk as it completes and waits for the one
  before (`jobs/writeback.rs`): the window is an eighth of the cgroup's memory limit or 1/64 of
  the RAM, whichever is smaller. A worker killed at `workers/`'s `memory.max` fails its
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
  workspace): the binary, the SMB (`smb` 0.12) and SFTP (`russh` 0.65, `russh-sftp` 3) clients, the
  remote routes and the transfer engine. Reason: `smb` goes through `sspi`/`picky`, which pin
  release candidates of RustCrypto crates (`rsa 0.10.0-rc`…) and no `russh` release has resolved
  with the workspace's own locked crypto crates and `argon2` so far. `sspi` is pinned to 0.22.1
  in the vendored `smb` (`vendor/smb/Cargo.toml`, "rynas patch"): 0.21.3 pinned
  `curve25519-dalek =5.0.0-rc.1`, which `russh` 0.65 cannot share a lock file with.
  Build, test and lint it with `--manifest-path crates/rynas-files-worker/Cargo.toml`;
  `cargo test --workspace` does not reach it, but rynasd's `tests/files.rs` builds it and drives
  it end to end. When `smb` releases on final crypto crates, the worker can rejoin the workspace.
- `smb` and `smb-transport` are patched (`[patch.crates-io]` to
  `crates/rynas-files-worker/vendor/smb` and `vendor/smb-transport`, both 0.12.1; every change
  is marked "rynas patch"):
  - `connection/worker/worker_trait.rs`: the credits granted in an interim `STATUS_PENDING`
    response are counted (MS-SMB2 3.3.4.2: an asynchronous request's credits come there, the
    final response grants none — Samba answers a read asynchronously once it takes over
    0.5 s). Without it each such read lost its 16 credits and the client soon waited forever on
    its credit semaphore with nothing in flight.
  - progress watchdog (`connection/worker/parallel/{base,threading_backend}.rs`,
    `connection.rs`): the connection's timeout no longer bounds one answer but counts from the
    last message the server sent (`ParallelWorker::activity`); waits for an answer and for
    credits (`Semaphore::acquire_many_timeout`) fail once nothing came for that long. The
    credit pool saturates instead of wrapping (`AtomicU16`).
  - `resource/file.rs`: read/write/flush errors keep the crate's `smb::Error` inside the
    `io::Error` (they were flattened to text, so a lock conflict looked like a broken
    connection); `File::read_owned` returns the response's buffer (no copy) and treats
    `STATUS_END_OF_FILE` as the end; `File::times` (last write and change time of the open,
    `ResourceHandle::raw_times`).
  - `crypto/signing.rs`: AES-GMAC offered first (a 3.1.1 server takes the client's order;
    CMAC's AES chain is serial and every message is signed). AES-GMAC has two implementations,
    chosen by CPU (`GmacBackend`): `ring` where AVX is missing, RustCrypto's `aes-gcm` where it is
    there. RustCrypto's GHASH (`polyval`) uses CLMUL only together with AVX: on a NAS CPU without
    AVX (Celeron J4125) it runs its software code, and that held the network thread at 99.9 % at
    1 GbE. `ring` has CLMUL without AVX too, but feeds additional data one block at a time, so
    on an AVX CPU it would sign about 3x slower than `polyval` (GMAC is nothing but additional
    data). AES-CMAC and HMAC-SHA256 stay on RustCrypto (`ring` has HMAC-SHA256 only and no CMAC).
  - `crypto/encryption/encrypt_gcm.rs`: AES-128/256-GCM encryption on `ring` on every CPU, about
    2x RustCrypto's speed with AVX; AES-GCM is offered before CCM (`ENCRYPTING_ALGOS`), CCM stays
    on RustCrypto. `crypto/kbkdf.rs`, `session/state.rs`: 32-byte keys for the AES-256 ciphers
    (MS-SMB2 3.1.4.2; a session that negotiated AES-256 used to fail with "Invalid length").
    `tests/smb_crypto.rs` cross-checks both implementations against each other, the old
    implementation and published vectors.
  - `Connection::credits_granted`, `mod parallel` `pub(crate)` (for the credit wait).
  - `smb-transport` `tcp/transport.rs`: `TCP_NODELAY` (each request is two writes; with Nagle
    every round trip waited for a delayed ACK, up to 40 ms: the old reads crawled at ~85 MB/s
    even on the loopback); the sync `receive` no longer drops the bytes already read of a
    message when the receive loop's 100 ms poll hits halfway (the stream lost its framing and
    the connection was dead for good: a server pausing inside a large response); a message
    that stops halfway for longer than the timeout ends the connection; the first answer
    (multi-protocol negotiation, read before the worker exists) has a read timeout.
  Drop the copies once releases carry these fixes.

### Remote servers that stop answering

- Progress watchdog, no call on a thread of its own: an SMB request fails when requests wait
  and nothing at all came from the server for 60 s (`REMOTE_TIMEOUT`, the crate's connection
  timeout with the patch above): `remote_unreachable` "the remote server stopped answering
  (no reply in 60 s)". A slow server that keeps answering is never cut off: a huge listing
  (each page is a request) or 8 reads in flight on a slow link take as long as they need.
- SFTP: the same rule, one watchdog per session (`sftp::Watch`): any bytes from the server
  reset it, and it fails the requests only when requests are outstanding and nothing has
  arrived for 60 s, for reads, writes and metadata alike. The clock restarts when the first
  request of a quiet session goes out and when the copy, not the server, was what waited
  (a paused or busy job). So a large file over a slow link is never cut off while replies keep
  coming, however far behind the last of 64 requests is. Connect, login and channel open have
  20 s each. The final `fsync@openssh.com` is outside the watchdog (nothing arrives while a server
  flushes) with a limit of its own (1 h; a dead connection is found by the keepalives
  first); the job waits for it on a thread of its own, so pause and cancel work meanwhile and
  a flush that lasts shows in the progress (`method: flush`). SSH keepalives every 15 s
  (3 missed end the session); the connection's runtime threads are always running, so
  keepalives, window adjusts and rekeys never wait for a caller.
- A copy is complete only at the size the file had when it was opened (`remote::Ident.size`,
  for a local source its `fstat`): reads that end early or run past it (an EOF status or empty
  reply mid-file from a server, a file that shrank or grew, on SMB or SFTP, up- or download) fail
  that file with `conflict`. The temporary file goes, the existing target stays, a move keeps
  its source.
- Overwriting a target (`Fs::replace`): in one step where the side can (local `rename`, SMB
  rename with replace, SFTP `posix-rename@openssh.com`). Otherwise the old file is moved
  aside to `.name.rynas-aside`, the part takes the name, the old file is removed, and it is
  put back if the part could not take the name; if even that fails, both stay under their
  hidden names and the error says so (the part is never deleted once the target has gone).
  The clean-up run and the cancel watchdog put an aside file back too.
- TCP keepalive (20 s idle, 10 s interval, 3 probes), `TCP_USER_TIMEOUT` 60 s and
  `TCP_NODELAY` on the worker's TCP sockets (`remote::tune_sockets`).
- Errors by side: only the clients make connection errors (`remote::stalled`, `remote::lost`,
  code `remote_unreachable`), and only those are retried (`remote::retryable`). The server's
  answer about one file (SMB `FILE_LOCK_CONFLICT`, `SHARING_VIOLATION`, `ACCESS_DENIED`, an
  unknown status; SFTP a status) fails that file only, as do the NAS's own errors (a local
  `ETIMEDOUT` or `EPIPE` is not the server's). SFTP errors are typed (russh-sftp's `Error`:
  a server status is the file's; `Timeout`, a broken or out-of-sync session is the connection's,
  and russh-sftp 3 ends its receive loop on those, failing the requests in flight). SMB session/handle loss
  (`USER_SESSION_DELETED`, `NETWORK_NAME_DELETED`, `FILE_CLOSED`, `INVALID_HANDLE`…) counts as
  the connection's.
- Reconnects (2 tries in a row, `RECONNECTS`): every server request of a job (`End::call`:
  stat, list, open, mkdir, remove, rename) goes on over a new connection; a retried request
  that may have been carried out before the break takes "already exists"/"not found" as done.
  A copy goes on from the current offset: the source only if it is still the same file
  (`remote::Ident`: size and last write time, for SMB also the change time, from the open),
  otherwise that file fails with `conflict` "the file changed on the server during the copy";
  the destination writer is reopened at the offset and the blocks the old one had not got
  confirmed are sent again (`Writer::unconfirmed`/`resend`). Then the job fails with
  `remote_unreachable` (Retry), without going through the next items one timeout each.
- Throughput (SMB): a file of more than one block is read by 8 threads (`smb-read`) keeping
  8 reads of up to 1 MiB (the negotiated MaxReadSize, 1 MiB cap) in flight, at most 16 blocks
  ahead of the copy, delivered in order straight into the copy loop (`Reader::next_block`, no
  copy); local writes overlap the network. Uploads keep up to 8 writes in flight
  (`smb-write`, `write_block_zc`); a one-block file uses no threads. Credits backlog 512.
  Each job's connection logs once (journal): dialect, signing algorithm, encryption cipher,
  MaxRead/MaxWrite, credits granted. Encryption only where the server or the share asks.
  `packaging/bench-smb.sh` measures smbclient and the worker against a real server.
- Throughput (SFTP): one continuous sliding window of reads on one handle (`sftp::pump`, a task
  on the connection's runtime): up to 64 requests of up to 255 KiB (`limits@openssh.com` when
  the server has it, read and write length and the largest packet it announces, minus the
  bytes around the data), one at first and one more per reply (OpenSSH's ramp), capped so the
  last request queued waits at most about 20 s behind the others at the link's measured rate
  (a short queue on a slow link; the watchdog does not depend on it). Replies go in order over
  a bounded channel (about 4 MiB) to the copy, so the thread writing to disk overlaps the
  network; about 20 MiB at most per download. A short reply is asked for again from where it
  stopped (the following requests made smaller, as OpenSSH); an empty reply or EOF status
  ends the reading where it ends, and the copy then fails that file (see above).
  Uploads: the same window of writes on one handle, unconfirmed blocks kept by offset and given
  back on a broken connection (`Writer::unconfirmed`/`resend`). The session keeps its own
  encoded copy of each write until it leaves for the server, so writes also wait while more
  than 4 MiB of them have not left yet: about 20 MiB per upload (window plus that), as for a
  download. Host keys: Ed25519 and ECDSA first, then RSA with SHA-2, `ssh-rsa` last; an RSA
  user key signs with SHA-2 where the server takes it, SHA-1 only for a server that takes
  nothing else (old servers keep working). The ciphers offered are
  AES-128/256-GCM first where the CPU has AES and carry-less multiply (`aes_accelerated`, at run
  time), ChaCha20-Poly1305 first otherwise; SSH packets of 65535 bytes (russh's largest), a
  32 MiB channel window, no compression; TCP_NODELAY set directly on the socket. Each job's
  connection logs one line (journal): KEX, cipher, packet size, server limits, in-flight
  settings. `packaging/bench-sftp.sh` measures OpenSSH's `sftp` and the worker on the target,
  per thread, plus a cipher micro-benchmark (`rynas-files-worker bench-ciphers`).
- Cancel: the worker stops between two chunks; if it has not within 3 s (a call blocked on the
  server) it removes the local `.rynas-part` it was writing — or, for a copy to a server, the
  remote `.rynas-part` and the folders the job made that are still empty, over a new
  connection, at most 10 s, logged (`RemoteLeft::clean`) — reports the job cancelled and
  exits. rynasd also enforces it: SIGTERM 5 s after the cancel, SIGKILL 5 s later (through a
  pidfd), then the job is `cancelled` and a `cleanup` run removes the temporary files, on a
  remote destination too (`.name.rynas-part` at its top and in the job's trees; the folders
  made are not known any more there). `JobView.cancelling` is true meanwhile. Pause answers at
  once and takes effect at the next chunk.
- `JobView.stalled_s`: a running job whose progress has not moved for 5 s or more; the
  transfer dialog shows "no data for N s" instead of waiting for the first data.

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
(disk, `sdc`), `device_name` (vendor + model); `JobView.code`, `JobView.task_id`, `JobView.stalled_s`, `JobView.cancelling`;
`ErrorItem.code`; `whoami.admin`; `Entry.outside`, `Details.outside` (a link out of its share);
`Spec.roots`, `Spec.app_roots` (worker side); `shares.apps`, `list.apps`, `list.stack`,
`Share.app` (the apps' data); `shares.backups`, `list.backups`, `list.read_only`,
`Share.read_only`, `Share.backup` (the PCs' backups).

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
| only administrators | every signed-in user (admins read and write everywhere in the shares, fenced by Landlock) | rynas roles |

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
  create, permission denied for another user, an administrator writing over permissions (what she
  makes is hers, existing files keep their owners; reading only without Landlock ABI 3), jobs and
  the bin as the user, the worker refusing root, `CapEff`, `CapAmb` and `NoNewPrivs` of live workers.

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
| Build and serving | trunk copies `files-app/` into the build (`<link data-trunk rel="copy-dir">` in `index.html`) and `hash-files-app.sh` renames it `dist/files-app-<hash>/`, immutable, named in `<meta name="rynas-files-app">`; rynas-web serves it like the rest of the UI (`--ui-dir`), the .deb ships it precompressed (`packaging/build-deb.sh` compresses every `.js`/`.css`/`.wasm`) |
| Leptos page | `crates/rynas-ui/src/pages/files.rs`: routes `/files` and `/files/*path`, the lazy script load, the bridge of window events |
| Checks | `crates/rynas-ui/src/files_check.rs` (texts translated, no other product named, only it/en, no undeclared sideways scroll); `tools/ui-audit/files_mock.py` (fake files API for screenshots) |

### How it is mounted

- **Lazy**: the wasm bundle does not grow. `pages/files.rs` adds
  `<script type="module" src="/files-app-<hash>/rynas-files.js">` the first time the Files page
  opens (skeleton while it loads, an error box whose Retry reloads the page if it fails: after an
  upgrade only the new page knows the new folder). `rynas-files.js` preloads the
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
