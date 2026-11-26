# Photos from the RyNAS app (D-PHOTOS, 2026-11-26)

Status: implemented 2026-11-26, revised the same day after a review (staging folder and
mounts from descriptors, administrator's choice, confirmed accounts). Not yet tried against a live Immich or PiGallery2: the API client is tested
against a fake server built from Immich's OpenAPI document.

## 1. Decision

The Android app's photo backup is the **single way photos come in**. Each NAS user has one
folder, `Photos`, in their home; the app writes there, by year and month (`Photos/YYYY/MM`),
through the files API (virtual path `/home/Photos`, made with `files/mkdir path=/home
name=Photos parents=1`). The photo apps of the catalogue never receive uploads from rynas'
phones: they **read** those folders, read-only, and rynas wires them.

Rules:
- A container never sees a whole home, nor any path a user can change: rynasd mounts each
  chosen user's `Photos` folder read-only into a root-owned staging folder the app binds.
- Nobody is shown in an app until an administrator chooses them (with an API: confirms the
  user's account in the app once).
- Nothing is ever deleted by the wiring: the binds are read-only, rynas never deletes nor
  changes an app's library (deleting an Immich library deletes its assets in Immich).
- Secrets (the Immich API key) live in the stack's `.env` (0600, root) like every app secret,
  never in a response, a log or the audit line.

## 2. Research

Immich (v3.4.2, the catalogue's version), from its OpenAPI document
(<https://github.com/immich-app/immich/blob/v3.4.2/open-api/immich-openapi-specs.json>, rendered
at <https://api.immich.app/endpoints/libraries>) and the external library guide
(<https://docs.immich.app/features/libraries>):
- `POST /api/libraries` (`createLibrary`, admin only, permission `library.create`), body
  `CreateLibraryDto` `{ownerId (required), name, importPaths, exclusionPatterns}` → 201
  `LibraryResponseDto` `{id, ownerId, name, importPaths, exclusionPatterns, assetCount, …}`.
- `GET /api/libraries` (`getAllLibraries`, admin, `library.read`): every library.
- `POST /api/libraries/{id}/scan` (`scanLibrary`, admin, `library.update`), no body → 204: the
  scan is queued ("find and import new assets").
- `GET /api/admin/users` (`searchUsersAdmin`, admin, `adminUser.read`) → `UserAdminResponseDto[]`
  with `id`, `email`, `name`, `deletedAt`, `isAdmin`, `oauthId`, `storageLabel`, …
- Authentication with an API key: header `x-api-key` (security scheme `api_key`). Keys are made
  by a user in Account settings → API keys, with chosen permissions; the library and admin
  routes need an administrator's key.
- Libraries: each import path must be a readable folder inside the container; paths are
  scanned recursively. A file deleted from disk goes to Immich's trash on the next scan
  (restored if the file comes back), and leaves for good after 30 days. A daily scan of every
  library is scheduled by default (Administration → Settings → External Library); automatic
  watching is **experimental** and does not work on network file systems, so rynas leaves it
  off and asks for a scan itself.

PiGallery2 (3.8.0), from its configuration classes
(<https://github.com/bpatrik/pigallery2/blob/3.8.0/src/common/config/private/PrivateConfig.ts>),
user model (<https://github.com/bpatrik/pigallery2/blob/3.8.0/src/common/entities/UserDTO.ts>)
and routes (<https://github.com/bpatrik/pigallery2/blob/3.8.0/src/backend/routes/UserRouter.ts>):
- One images folder (`/app/data/images` in the image). `Users.allowDirectoryList` (global) is a
  list of directory searches relative to it (`{value, matchType: exact_match | like |
  globMatch, negate}`, e.g. `/photos/private/*`); "parent directories of an allowed directory
  are still navigable, but their content is hidden". Every user can override it
  (`overrideAllowBlockList`, `allowDirectoryList`, `blockDirectoryList` on the user).
- Per-user overrides are stored in the gallery's database and changed through
  `POST /pgapi/user/:id/settings`, which needs a signed-in gallery administrator (a session,
  no API key). `Users.enforcedUsers` in the configuration file creates users at start but never
  overwrites an existing one.
- So rynas **cannot** set per-user allow lists without the gallery administrator's password,
  which it never has (the gallery starts with `admin`/`admin` and the user changes it). It
  mounts the chosen folders and the app page lists the value to enter for each account.

## 3. Folders and mounts (`rynasd::photos`, `rynasd::photos_mount`)

Review 2026-11-26 (H1): the first version bound `<home>/Photos` in the compose file. The user
owns the home and can replace `Photos` with a link (to `/`) at any time; Docker resolves the
bind source on every container start, and Immich's server runs as root, so the whole host
would have been imported into that user's library. No path a user can change is ever bound
now:

- Each photo stack has a **staging folder**, `<state>/docker/photos/<stack>`
  (`/var/lib/rynas/docker/photos/<stack>`), made by rynasd, root, 0755, with one empty folder
  per chosen user and the entry's `mountpoints`. It is outside the stack folder, so stack
  snapshots, backups of the stack and an uninstall that deletes the data never walk into it.
- The compose file binds the staging folder **read-only** at `[user_photos].target`, with
  `bind.propagation: rslave`: mounts rynasd makes in it later appear in the running app, and
  one it takes away goes, without a restart. (The host's `/var/lib` must be on a shared mount,
  the systemd default.)
- rynasd mounts each chosen user's folder at `<staging>/<user>` with `rynasd photos-mount`, run
  in a transient unit (`rynas_tasks::Outside`: rynasd's own sandbox has no mount calls). The
  helper opens the real home (links of its parents resolved by rynasd; they are root's)
  without following its last component, checks it is a folder of the user, opens `Photos`
  below it with `O_NOFOLLOW` and checks it is a folder of the user, checks the target is a
  root folder nobody else may change, and mounts **from that descriptor**: `open_tree` +
  `mount_setattr` (read-only, nosuid, nodev, noexec, **nosymfollow**, private) +
  `move_mount`, so the mount has its attributes before it is attached. Kernels without these
  calls get a bind of `/proc/self/fd/N` in a private scratch folder (`/run/rynas-photos`, made
  once as a private mount of itself, one subfolder per helper process; subfolders of helpers
  that died are cleaned when a helper starts), remounted there read-only, nosuid, nodev,
  noexec, `MS_NOSYMFOLLOW`, then moved onto the target with `MS_MOVE`: no app ever sees it
  writable. On both paths the mount is checked with `statvfs` afterwards (`ST_RDONLY`,
  `ST_NOSUID`, `ST_NODEV`, `ST_NOEXEC`, `ST_NOSYMFOLLOW`): a mount the kernel gave less is
  taken away and refused.
  A link swapped in after the check changes nothing: the mount holds the folder, not its name.
- Review 2026-11-26 (2nd, H2): links are never followed inside a mount. Every user sits below
  the same container folder, so `ln -s /mnt/photos/alice ~/Photos/x` would otherwise show
  Alice's photos under Bob's account (Immich's scanner and PiGallery2 follow links). A kernel
  that does not know the flag (before Linux 5.10) refuses the mount: nothing is mounted rather
  than a mount without it.
- A failed check (no `Photos` yet, a link, a file, a home on a volume not mounted yet) changes
  nothing at the target: a user mounted before **stays mounted** (an app that loses a folder
  trashes its photos after a while) and the task log says why; a user not mounted yet stays an
  empty root folder, and the app starts as usual. The same folder mounted already is left as
  it is; a new `Photos` (same user, another folder) replaces the old mount.
- At boot, the unit `rynas-photos-mounts.service` (`rynasd photos-restore`, oneshot,
  `Before=docker.service`, wanted by it, never failing) mounts the chosen users' folders from
  `<state>/docker/photos/<stack>.json` with the same checks before the apps start (rynasd's
  unit is ordered after it), skipping stacks whose compose file is gone. rynasd also mounts
  what is missing when it starts, after each sync, and **when the mount table changes** (a
  home on a volume mounted later): a thread sleeps in `poll(2)` on `/proc/self/mountinfo`
  (`POLLPRI`, no wakeup otherwise), changes are gathered for 3 s, and only if a chosen folder
  of an installed stack is not mounted does rynasd mount it, stack by stack, reading the
  choices and the mount table under the stack's busy mark. No timer runs for this; the apps'
  `rslave` binds pick the mounts up. Uninstall removes the choices with the staging folder. The user list is checked first: an unreadable or empty passwd changes
  nothing (every chosen user would look gone).
- Users leave the mounts only when they leave passwd (logged: the app may move the photos to
  its trash) or when an administrator leaves them out.
- The apps' account (`docker-apps`) gets read access through ACLs (`idremap::grant` with an
  `owner`: only files and folders of the user, never a regular file with several hard links,
  only bits added, default ACLs on folders), walked from the checked descriptor
  (`idremap::grant_fd`; a path through `/proc/self/fd` would stop at its `self` link), so apps
  that run as that account (PiGallery2) read the folders; Immich's server runs as root.
  Skipping a hard-linked file only withholds the ACL entry: a file the user made world-readable
  elsewhere and hard-linked into `Photos` stays readable through the mount by that user's own
  library, which exposes nothing the user could not read already.
- A sync of a user mounted already compares the mounted folder with the source (same folder:
  left as it is; another one: the old mount is replaced after the staging folder is checked
  through its root-owned parent) and never touches the mounted folder's mode.
- Uninstall: the mounts are taken away after the containers stop; with the data, the
  staging folder (only empty folders are removed) and the choices go too.

## 4. Catalogue: `[user_photos]`

A new optional table of `app.toml` (§6.2 of `docker-apps.md`):

| Key | Meaning |
|---|---|
| `env` | `.env` variable keeping the staging folder; never referenced by compose |
| `service` | the compose service the staging folder is bound into |
| `target` | absolute container folder; each user's folder is `<target>/<user>` |
| `description` | shown with the entry |
| `key_env` | optional: `.env` variable keeping an API key the administrator gives rynas (never referenced by compose, never generated) |
| `mountpoints` | optional: folders kept in the staging folder for other binds of the service below `target` (names start with an upper-case letter, never a user name) |

The render appends one long-syntax bind of the staging folder, `read_only: true`,
`bind.propagation: rslave`; the folder is filled by rynasd, never by the install request. An
update keeps it (`request_from_env`); "Sync" binds it when it is missing.

Entries:
- **Immich 1.1.0**: `target = "/mnt/photos"`, `key_env = "PHOTOS_API_KEY"`, service
  `immich-server`. The optional `EXTERNAL_LIBRARY` share stays at `/mnt/external`.
- **PiGallery2 2.0.0**: the images folder is the staging folder; the shared folder is bound at
  `/app/data/images/Shared` (`mountpoints = ["Shared"]`: a read-only bind cannot get a new
  mount point, so rynasd keeps the folder there), each chosen user is
  `/app/data/images/<user>`. The 2.0.0 migration says that the gallery indexes again and
  shared links must be made again. The install note points phones to the RyNAS app's backup.

## 5. Who: chosen by an administrator

Review 2026-11-26 (M2, M5): nobody is mounted at install. Every account of a fresh PiGallery2
(`admin`/`admin` first) would otherwise see every user's photos, and matching Immich accounts
by name is spoofable (users rename themselves). The choices are kept per stack in
`<state>/docker/photos/<stack>.json` (root, 0600): `{users: {<user>: {account?, blocked?}}}`.

- PiGallery2 (no API): the administrator shows a user in the app or leaves them out
  (`POST …/photos/users/{user}` `{include}`); the page says that every gallery account sees
  every chosen folder until it has a directory allow list.
- Immich: a user is mounted only with an account an administrator **confirmed** once
  (`{include: true, account: <Immich user id>}`, checked against `GET /api/admin/users`).
  rynas proposes one (the account with the user's email in rynas, else the only account with
  the user's name; never the email's local part, never a deleted account) and never confirms
  nor changes it by itself: a renamed account keeps its user, a new account with a matching
  name changes nothing.

## 6. Immich wiring

With a key (`POST /api/v1/apps/installed/{stack}/photos/key` `{key}`: the stack is marked busy
first, the files read after that, the key checked with `GET /api/admin/users` and
`GET /api/libraries`, then written to `.env`; empty forgets it; no restart, compose never
references it), **Sync** creates for each mounted, confirmed user without a library a library
owned by the confirmed account, named "Photos from the RyNAS app", whose only import path is
`/mnt/photos/<user>`, and queues its scan.

Review 2026-11-26 (M3): a library of **another** account on the user's path (a user name used
again, a library made by hand) blocks the user: the folder is unmounted, the block kept in the
choices (and honoured at start and while Immich does not answer) and shown on the page until
an administrator resolves it in Immich; one other account's library on the path is enough, even
next to the user's own. A library of the confirmed account is left as it is. Choosing a user
marks the stack busy before the choices are read and written, and the sync runs under it.
Immich's own mobile upload keeps working; OIDC accounts (phase 3) are confirmed the same way.

The client talks to `http://127.0.0.1:<web port>` (the port is published on loopback or on
every address, both reach it). Errors never carry the key.

## 7. Rescan after uploads

The files gateway (`rynasd::files`) tells `photos::uploaded(user, dir)` about every successful
`upload` request; one whose folder is `/home/Photos` or below queues the user. A user's batch
is over after 45 s without another upload, or 10 minutes after its first upload while uploads
keep coming (`Debounce`); then rynas asks every Immich where the user is confirmed and not
blocked to scan **the library of the confirmed account on the user's path** (`scan_target`),
and no other. A missed one is caught by Immich's daily scan. Nothing is persisted: a restart
forgets a pending batch. The worker sleeps while nothing is queued and is woken by an upload;
then it sleeps only until the next batch is due (no periodic timer).

## 8. API

Admin only, under `/api/v1/apps/installed/{stack}/photos` (409 for an app without
`[user_photos]`):
- `GET` → `PhotosState { users: [{user, folder, included, mounted, problem?, library?,
  account?, proposed?}], api?: {key, error?}, allow_list? }` (`library`: `linked`, `missing`,
  `unconfirmed`, `no-account`, `other-owner`; `allow_list` is PiGallery2's per-user value,
  `/{user}/*`).
- `POST …/users/{user}` `{include, account?}` → task `apps.photos-sync` (400 for a user who is
  not a NAS user with a home, or without `account` for an app with an API; 409 without a key).
- `POST …/sync` → task `apps.photos-sync`: the staging folder (a new render and `up` only when
  the stack does not bind it yet, the previous files back on a failure), the mounts as
  planned (`photos::plan`), then the libraries.
- `POST …/key` `{key}` → 204.

`InstalledApp.user_photos` tells the UI to show the section ("Photos from the RyNAS app" in
the installed app's drawer: users wired, one line per user with its state and a button to
show, confirm or leave out, the key field for Immich, the allow-list hint for PiGallery2,
Sync).

## 9. Open points

- A home on a volume mounted after Docker starts is empty in the app until that mount shows
  in the mount table and rynasd mounts the folder (a few seconds). Immich only trashes photos it finds missing during a scan (its daily scan runs at
  midnight by default) and takes them back when they return.
- After updating PiGallery2 from 1.x the staging folder is bound at the first Sync.
- Per-user allow lists in PiGallery2 stay manual (no API key there).
- The mount itself is not exercised by the tests (it needs root and a mount namespace); the
  checks it depends on (`open_photos`, the link swapped after the check, the target, the
  mount table) are.
