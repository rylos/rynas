# Review 2026-11-07 — phase 2 backend (Docker, apps, TLS, web access)

Scope: `git diff 0406ae4..main` minus `crates/rynas-ui`: `rynas-docker`, `rynas-apps`, `rynas-tls`,
`rynasd/src/{docker.rs,apps.rs,tls.rs,web_access/}`, `rynas-web` (proxy, conn, listen, tls),
`packaging/caddy`, `apps/`. Read-only review, time-boxed (~40 min). References are `file:line` on
`main` at fba32cb.

## High

**H1. Stack folders: root writes follow symlinks in folders owned by `docker-apps`.**
`crates/rynas-docker/src/stacks.rs:465-497` (`write_file`) opens `.<name>.rynas-tmp` with
`create(true).truncate(true)` (follows symlinks), then path-based `chown` (`:406-412`, follows
symlinks) before the rename. The stack folder is `0750 docker-apps` (`:546-548`), and the parent
walk creates subfolders as `docker-apps` too. Any process running as that uid (the PUID of
containers, which commonly bind `./config` or other subfolders) can plant
`.env.rynas-tmp -> /etc/shadow`. The next `PUT /docker/stacks/{n}/files`, app update or
`apps.proxy` then makes rynasd truncate and overwrite the target as root and hand it to
`docker-apps`, which is root escalation. `create_bind_dirs` (`:630-670`) and `apps.rs:1107-1140`
(`make_folders`) also walk paths with `exists()`/`create_dir`/`chown` in folders the same uid
controls.
*Fix:* remove the temp file, then `create_new(true)` plus `custom_flags(O_NOFOLLOW)`. Use
`fchown`/`fchmod` on the open fd. Open parents with `openat(O_DIRECTORY|O_NOFOLLOW)` (rustix or
cap-std), and refuse a symlink anywhere below the stack folder. Better still, make the stack
folder, `compose.yaml` and `.env` `root:docker-apps 0750/0640/0600 root`. Compose runs as root, so
the containers' uid does not need to own them. Only data subfolders need `docker-apps`.

**H2. `apps.uninstall` with `delete_data` can delete arbitrary system or user folders.**
`apps.rs:1806-1820` collects the "outside" folders from the stack's **`.env`**, which
`docker-apps` can write (H1) and admins can edit with PUT files. They pass only `on_volume`
(`apps.rs:246-257`), which accepts any `/<label>/x` where `/<label>` is a directory and the label
passes `volume_label`. `bin`, `lib`, `lib64`, `sbin`, `snap`, `nix`… are valid labels and not in
`RESERVED_LABELS` (`rynas-api-types/src/validate.rs:81`). `/lib` is a directory (a symlink on
merged-usr). Then `remove_tree` (`apps.rs:1858-1862`) deletes it recursively as root. The same
applies to a folder the user picked that **already existed**: `make_folders` skips existing
folders, but uninstall still deletes them (for example `/vol1/media` given as a config folder).
*Fix:* `on_volume` must check the label against the mount table (`rynas_volumes()`), not
`is_dir`. Uninstall must delete only the folders the install **created**. Record them in the
`x-rynas` block, keep them under `/<vol>/docker/appdata/<stack>/`, and never read them back from
`.env`. Add a "contains a mount point / is a volume root" guard before `remove_tree`.

**H3. `POST /v1/system/tls/import` and `/local` have no rynasd admin check and no audit line.**
`crates/rynasd/src/tls.rs:451-468` takes neither `x-rynas-user` nor `require_admin`, and logs only
the subject. Every other changing route checks the role again (guidelines A6, docker-apps §3.6).
Replacing the interface certificate is a MITM primitive.
*Fix:* take the `HeaderMap`, call `user()` + `ops.admins.require_admin(&who, "HTTPS certificate")`,
and add `audit(who, "tls.import|tls.local", subject/fingerprint)`. Also check the GET routes.

## Medium (M9, rynasd idle RSS, is in the Memory section)

**M1. rynasd does not check the administrator role on Docker reads, contradicting §5.1
"rynasd checks it again".**
`docker.rs:819-829` (status, settings GET), `:864` (`plan_engine`, returns
`/etc/docker/daemon.json` contents), `:1260-1312` (containers, stacks, images, volumes, networks,
df, stats), `:1584` (`stack_validate`, which runs `docker compose config` on caller-supplied YAML
as root; `env_file:` and `include:` read root files into the error text), `:1599` (`stack_get`),
`:2089` (`snapshots_get`), `:2155` (`updates_get`). rynas-web blocks non-admins (`auth.rs:154`),
so this is defence in depth. Apps and web-access do check every route.
*Fix:* one `admin_reader` extractor applied to the whole `/v1/docker` router (route layer), plus
a test that a non-admin gets 403 on every GET.

**M2. `docker.changed` hint storm from healthcheck exec events.**
`docker.rs:2423-2426` sets `dirty = true` for **every** container-type event, including
`exec_create/exec_start/exec_die` and other noise. Docker emits these for each healthcheck run.
With ~10 healthchecked containers, the UI gets `docker.changed` every 1-3 s and re-reads every
list, and the stats sampler's container cache (`:2505`) is invalidated each tick. That is a
steady cost on the J4125.
*Fix:* mark dirty only for lifecycle actions (create, start, die, stop, destroy, rename, pause,
unpause, update, health_status), or pass an `event` filter to the engine
(`rynas-docker/src/client.rs:234`).

**M3. Watcher backoff resets on every connect.**
`docker.rs:2415`: if the events stream fails immediately (API mismatch, proxy socket), the loop
reconnects every ~1 s forever. Each pass makes a list call, a `seed` and a `changed()`.
*Fix:* reset the backoff only after the stream has lived for more than 30 s.

**M4. A failed migration leaves Docker stopped.**
`docker.rs:1164-1250`: any error after `systemctl stop` (copy fails or disk full,
`apply_update` fails) returns without restarting the engine. There is no free-space check before
`cp -a`. The volume check is `is_dir` (`:1108`), not "mounted", so an unmounted `/vol1` copies the
data-root onto the system disk, and it is later hidden under the mount.
*Fix:* a guard that restarts `containerd`/`docker` on the old roots on any error. Before
stopping, compare a `du`/`df` estimate with the free space on the target. Use `rynas_volumes()`
for the mount check (the same applies in `stacks.rs:448` and `apps.rs:253`).

**M5. Root writes into `rynas-caddy`'s writable state folder.**
`tls.rs:333-349` (`write_atomic`) uses `create_new` (good), then **path-based**
`chown`/`set_permissions` on the temp file. `/var/lib/rynas-caddy` is `0750 rynas-caddy`
(`rynas-caddy.service`, `StateDirectory`), and `create_dir_all(rynas/)` (`web_access/mod.rs:468`)
follows a planted symlink. A compromised Caddy can win the race and get any file chowned to its
group at 0640 (for example `/etc/shadow`).
*Fix:* `fchown` + `f.set_permissions` on the fd before `rename`. Open the folder with `O_NOFOLLOW`.
Alternatively, put rynasd's files in a root-owned `/var/lib/rynas-caddy/rynas` (`0750 root:rynas-caddy`)
that Caddy cannot replace.

**M6. `GET /docker/volumes` and `/docker/df` call `docker system df` on every request.**
`docker.rs:1278-1298`. It walks every volume's size, takes seconds on HDDs and wakes them up.
*Fix:* cache it for 5 min (or one per task), or make sizes opt-in (`?sizes=1`).

**M7. The Docker apt key is not pinned.**
`rynas-docker/src/install.rs:110-114` downloads `gpg` over HTTPS with no fingerprint check.
*Fix:* check the fingerprint `9DC8 5822 9FC7 DD38 854A E2D8 8D81 803C 0EBF CD88` (`gpg --show-keys`)
before writing `docker.asc`, and fail the task otherwise.

**M8. The icon's sandbox CSP is dropped by rynas-web.**
`rynas-web/src/proxy.rs:257-270` (`passed_headers`) does not pass `Content-Security-Policy` or
`Cache-Control`. The `sandbox` CSP set in `apps.rs:1405-1408` never reaches the browser, and the
SVG gets the app-wide CSP instead. §5.2 promises "a sandboxing CSP".
*Fix:* pass `Content-Security-Policy` and `Cache-Control` from rynasd (the global layer is
`if_not_present`).

## Low

- **L1.** The local CA has no name constraints (`rynas-tls/src/lib.rs:203`). Users trust it in
  their browsers, so a stolen `ca.key` signs any site. Add permitted subtrees (`.local`, the host
  name, private IP ranges) or document it.
- **L2.** `address_pool` changes do nothing once `default-address-pools` exists
  (`rynas-docker/src/engine.rs:113-122`), including the value rynas wrote itself. Either track
  rynas's own value and replace it, or report the setting as "kept".
- **L3.** `merge_daemon_json` keeps a legacy `graph` key next to `data-root`, and dockerd then
  refuses to start. Drop `graph` or fail the plan.
- **L4.** The local XFF trust (`conn.rs:66`) accepts any loopback process, host-network
  containers included (Home Assistant is privileged on the host network). Such a process can
  rotate fake client IPs past the per-IP login limit. Confirm that a per-account lockout also
  exists, or require the peer uid (`SO_PEERCRED` does not work on TCP) or a shared header secret
  from Caddy.
- **L5.** `.env` stays owned by `docker-apps` 0600 (see H1). Secrets end up readable by every
  container uid that sees the folder.

## Checked and fine

- Catalogue: every image is `tag@sha256`. The socket appears only in the four manager entries.
  Home Assistant declares `privileged` + `host_network` + the `root-access` antifeature.
- No generic Docker API proxy: rynas-web has a fixed route list, and actions are whitelisted
  (`docker.rs:1345`).
- Caddy: the admin block is identical in `bootstrap.json` and the render (`unix//…|0600`,
  `persist:false`). The unit is hardened. Identity headers are stripped. Unknown hosts get
  `abort`.
- Avahi: a managed block with validated labels. The confirm flow uses explicit headers
  (`proxy.rs:318`).
- Compose and apt commands: `Cmd` without a shell, `env_clear` with a fixed PATH, timeouts set.
- Snapshots: `.snapshots` 0700 root. The rollback renames are safe.
- XFF parsing uses the right-most untrusted entry (`conn.rs:84-120`).

## Memory and speed

Measured on pc-work, release build of `main`, `FAKE_DOCKER=1 tools/ui-audit/start.sh 18690`,
idle with no browser connected, sampled about 25 s after start:

| Process | RSS | Budget |
|---|---|---|
| rynasd | **23.1 MB** | < 20 MB: **over budget** (M9) |
| rynas-web | 8.5 MB | < 20 MB: ok |

Binary sizes: `rynasd` went from 14.8 MB to **20.8 MB** (+6 MB: bollard, rustls/rcgen,
rynas-apps, web access). `rynas-web` went from 3.63 MB to 4.93 MB (+1.3 MB: rustls).

**M9. rynasd idle RSS is over budget.**
Likely contributors: the catalogue kept in memory, bollard/hyper with its own runtime pieces, the
rcgen/rustls code pages, the tracker maps.
*Fix:* measure with `heaptrack` / `/proc/<pid>/smaps_rollup` (anonymous vs file-backed). File-backed
text from a larger binary counts in RSS but can be reclaimed. If the anonymous part dominates:
load the catalogue lazily (drop it after N minutes idle), and build bollard with the fewest
features (no `ssl`, `buildkit`, `chrono`). Caddy is capped at `GOMEMLIMIT=100MiB`, `MemoryHigh=128M`, and its
expected idle RSS is 30-60 MB. Idle paths: the sampler sleeps without listeners (good), and the
watcher costs one list call per minute plus M2. Per tick, the sampler allocates a `Vec<Target>`
and JSON for ≤ 50 containers, which is negligible.

## The 5 most valuable missing tests

1. Symlink planted in the stack folder (`.env.rynas-tmp`, a subfolder link): `write_files` /
   `create_bind_dirs` must refuse it and never touch the target (H1).
2. `apps.uninstall delete_data` with a tampered `.env` (`/lib/x`, a pre-existing folder): nothing
   outside the created appdata is deleted (H2).
3. A non-admin gets 403 on **every** `/v1/docker/*`, `/v1/system/tls/*` and `/v1/apps/*` route,
   generated from the router table (H3, M1).
4. `docker.stack.update` with a failing `pull`/`up` against `fakedocker`: automatic rollback,
   folder swapped back, retag commands, final state logged. Also a migration that fails mid-copy
   leaves the engine restarted (M4).
5. Watcher: an events stream that fails immediately backs off past 1 s, and a burst of `exec_*`
   events gives no `docker.changed` (M2, M3).

## Resolution (2026-11-07, backend fix wave)

| Finding | Status | What changed / why not |
|---|---|---|
| H1 | fixed | New `rynas_docker::safefs`: walks with `openat(O_DIRECTORY\|O_NOFOLLOW)`, `mkdirat` + `fchown`/`fchmod` on the descriptor, files through an `unlinkat` + `O_CREAT\|O_EXCL\|O_NOFOLLOW` temporary name, owner/mode set before any byte, `renameat`. `StackFs::{create_stack, write_files, read_files, create_bind_dirs}` and `apps::make_folders` use it (`stacks::create_below`). Stack folder `0750 root:docker-apps`, `compose.yaml` `0640 root:docker-apps`, `.env` `0600 root:root` (existing stacks are repaired on the next write); extra files and bind folders stay `docker-apps`. Docs §3 rule 7 and "Stacks on disk" updated. |
| H2 | fixed | Install/update record the folders they made in `<state>/docker/app-folders/<stack>.json` (root only); uninstall never reads `.env`. `uninstall_targets` keeps only recorded folders below a volume of the mount table (`DockerSvc::volume_mounted`), deeper than its root, not under a system top folder, without a mount point at or below; deletion uses `symlink_metadata` + `remove_dir_all` (no symlink followed), made parents only when empty. `RESERVED_LABELS` also refuses `bin`, `sbin`, `lib*`, `snap`, `nix`, `efi`. |
| H3 | fixed | `tls::router` takes the admin policy and has a route layer (`roles::admin_only`) over all four routes, reads included; import and local write `rynas::audit` lines with subject and SHA-256. |
| M1 | fixed | One `admin_only` route layer over the whole `/v1/docker` router. `stack_validate`: 512 KiB limit, top-level `include:` refused, `--no-env-resolution` (fallback for compose < 2.24), 0600 scratch files, 30 s timeout; documented in §5.1. Running compose as `docker-apps` was rejected: with `.env` now `0600 root`, `env_file: .env` stacks would fail to validate. |
| M2 | fixed | Engine-side `event` filter (`events::FOLLOWED_ACTIONS`), `changes_lists` drops `exec_*`, `kill`, `oom` and other noise; hints coalesced by a 1 s one-shot timer (no per-second tick); the sampler only re-lists on those hints. |
| M3 | fixed | `events::Backoff`: doubles to 60 s, resets only after a stream that lived 30 s. |
| M4 | fixed | Mount-table check, `du -sx` vs free space (+10 %) before stopping, engine restarted on any failure after the stop (`migrate_stopped` + unconditional start), clear log of where the partial copy is. Also found: `compose()` ignored compose's exit status, so a failing `pull`/`up` never failed the task and never rolled back; fixed. |
| M5 | fixed | `/var/lib/rynas-caddy/rynas` is `0750 root:rynas-caddy`, opened without following, repaired when it exists; certificate, key and `last-good.json` written with `safefs::write_at`; `tls::write_atomic` is descriptor-based too. |
| M6 | fixed | `DockerSvc::df()`: 60 s TTL, one call shared by concurrent requests, only totals and volume sizes kept. |
| M7 | fixed | Key downloaded to a staging file, `install::check_key` computes the OpenPGP v4 fingerprint (SHA-1 of the key packet) and compares it with `9DC858229FC7DD38854AE2D88D81803C0EBFCD88` before writing `docker.asc` (0644). |
| M8 | fixed | rynas-web's `passed_headers` forwards `Content-Security-Policy` and `Cache-Control`. |
| L1 | not fixed | The web-access leaves of the local CA carry administrator-chosen domain names and global IPv6 addresses, so permitted subtrees cannot be bounded; documented in `docs/phase1/plan.md` (W0). |
| L2 | fixed | A pool rynas wrote (the default or the last applied one) follows the setting; any other stays. |
| L3 | fixed | `graph` is dropped from `daemon.json`. |
| L4 | not fixed | Mitigated: the login limiter also counts failures per account (`rynas_auth::ratelimit`, `free_per_user`), so rotating fake client IPs does not get more password tries. A shared secret from Caddy needs a design pass with web access. |
| L5 | fixed | Covered by H1 (`.env` `0600 root:root`). |
| M9 | fixed | Measured with `/proc/<pid>/smaps` (dry instance, `FAKE_DOCKER=1`, idle ~35 s): heap is small (anonymous 2.7 MB); file-backed code dominates, because the page cache maps whole large folios and ~95 % of `.text` is resident from the start. Release profile: dependencies `opt-level = "s"` (speed-sensitive crypto crates stay at 3), rynasd `opt-level = "z"` (its fat LTO pass covers everything linked into it), `strip` restated on the override. rynasd's key check uses only rustls' signing half (`any_supported_type`), not the whole provider. The app catalogue is still parsed lazily on the first request, and now dropped after 10 min without one. Numbers below. |

### Memory after the fixes

| Build | rynasd RSS | rynasd anon | rynasd binary resident | rynasd file size | rynas-web RSS |
|---|---|---|---|---|---|
| `main` fba32cb (opt 3) | 23.3 MB | 2.7 MB | 15.5 MB | 20.8 MB | 7.6 MB |
| fixes, opt 3 | 23.8 MB | 2.7 MB | 16.0 MB | 21.0 MB | 8.6 MB |
| fixes, deps "s" | 23.0 MB | 2.6 MB | 15.3 MB | 20.0 MB | 8.3 MB |
| fixes, deps "s", rynasd "s" | 20.6 MB | 2.7 MB | 12.8 MB | — | 8.2 MB |
| **fixes, deps "s", rynasd "z"** | **19.7 MB** | 2.8 MB | 11.9 MB | **15.8 MB** | **8.3 MB** |

Rest of rynasd's RSS: libc 1.7 MB, libsqlite3 1.4 MB, libcrypto + libssl 1.4 MB (native-tls for
notification mail and webhooks, kept on purpose for Debian security updates). Switching those to
rustls would save about 1 MB more if the margin is ever needed.

### Also changed in this wave (GUI review follow-ups)

- `DockerStats.usage` (new, additive, `#[serde(default)]`): `[ContainerUsage {id, cpu, mem}]`
  for every running container of the sample, from the same cgroup reading as the totals; `top`
  stays for the widget (§5.1).
- `GET /docker/updates` is already cheap (the last check's report in memory); documented in §5.1.
