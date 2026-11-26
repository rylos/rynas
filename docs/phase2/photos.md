# Photos from the RyNAS app (D-PHOTOS, 2026-11-26)

Status: implemented 2026-11-26 (folders, mounts, Immich libraries, rescan after uploads, app
page section). Not yet tried against a live Immich or PiGallery2: the API client is tested
against a fake server built from Immich's OpenAPI document.

## 1. Decision

The Android app's photo backup is the **single way photos come in**. Each NAS user has one
folder, `Photos`, in their home; the app writes there, by year and month (`Photos/YYYY/MM`),
through the files API (virtual path `/home/Photos`, made with `files/mkdir path=/home
name=Photos parents=1`). The photo apps of the catalogue never receive uploads from rynas'
phones: they **read** those folders, read-only, and rynas wires them.

Rules:
- A container never sees a whole home: only the `Photos` folder of each user, each as its own
  read-only bind.
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
  mounts the folders and the app page lists the value to enter for each account.

## 3. Folders (`rynasd::photos`)

- Users: the NAS users of `/etc/passwd` (uid 1000-59999, not root, a system account or
  `docker-apps`) whose home field is absolute. Their real home is the passwd home with links
  resolved (the links are root's: `/srv/rynas/homes/<user>` → `<volume>/homes/<user>`); it
  must be a folder of the user.
- The folder is `<real home>/Photos`. When missing, the install and "Sync" make it, owner the
  user and their primary group, mode 0700 like the home. A `Photos` that is a link, not a
  folder or not the user's is refused (logged, the user is left out): a bind of a link would
  expose whatever it points at.
- The apps' account (`docker-apps`) gets read access through ACLs (`idremap::grant`: only bits
  added, a default ACL on folders so new photos inherit it, no link followed, one file
  system), so apps that run as that account (PiGallery2) read the folders; Immich's server runs
  as root in its container and does not need it.
- Known limit: the user owns their home and could swap `Photos` for a link between rynas'
  check and the container's start (the bind resolves links on the host). The check makes it
  a race, not a free choice; a bind through a file descriptor is not possible with compose.

## 4. Catalogue: `[user_photos]`

A new optional table of `app.toml` (§6.2 of `docker-apps.md`):

| Key | Meaning |
|---|---|
| `env` | `.env` variable keeping the list (`user=path;…`, the share set encoding); never referenced by compose |
| `service` | the compose service the folders are mounted into |
| `target` | absolute container folder; each user's folder is `<target>/<user>` |
| `description` | shown with the entry |
| `key_env` | optional: `.env` variable keeping an API key the administrator gives rynas (never referenced by compose, never generated) |

The render appends one long-syntax bind per user, always `read_only: true`, never following
host mounts; item names must be NAS user names and paths absolute (`check_user_photos`). The
list is filled by rynasd, never by the install request. An update keeps the list stored in
`.env` (`request_from_env`); "Sync" fills it again.

Entries:
- **Immich 1.1.0**: `target = "/mnt/photos"`, `key_env = "PHOTOS_API_KEY"`, service
  `immich-server`. The optional `EXTERNAL_LIBRARY` share stays at `/mnt/external`.
- **PiGallery2 2.0.0**: the images folder holds the shared folder as `/app/data/images/Shared`
  (it was the whole images folder in 1.x) and each user's folder as `/app/data/images/<user>`
  (user names are lowercase, so `Shared` never collides). A nested bind inside a read-only
  bind cannot be made, hence the move; the 2.0.0 migration says that the gallery indexes again
  and shared links must be made again. The install note no longer points phones to another
  sync tool: it points to the RyNAS app's backup.

## 5. Immich wiring

With a key (`POST /api/v1/apps/installed/{stack}/photos/key` `{key}`: checked first with
`GET /api/admin/users` and `GET /api/libraries`, then written to `.env`; empty forgets it; no
restart, compose never references it), **Sync** creates for each mounted user with an Immich
account a library owned by that account, named "Photos from the RyNAS app", whose only import
path is `/mnt/photos/<user>`, and queues its scan.

Account match: the account with the NAS user's email (when the user has one in rynas), else
the account named like the user or whose email's local part is the user name (case does not
matter, deleted accounts never). Several matches choose none (the page says to set the email).
A library already reading the path is left as it is: "library ready" when it belongs to the
user's account, "read by another account's library" otherwise. Immich's own mobile upload
keeps working; OIDC accounts (phase 3) match the same way.

The client talks to `http://127.0.0.1:<web port>` (the port is published on loopback or on
every address, both reach it). Errors never carry the key.

## 6. Rescan after uploads

The files gateway (`rynasd::files`) tells `photos::uploaded(user, dir)` about every successful
`upload` request; one whose folder is `/home/Photos` or below queues the user. A user's batch
is over after 45 s without another upload, or 10 minutes after its first upload while uploads
keep coming (`Debounce`); then rynas asks every photo app with a key that mounts the user's
folder to scan the user's library (`POST /api/libraries/{id}/scan`). A missed one is caught by
Immich's daily scan. Nothing is persisted: a restart forgets a pending batch.

## 7. API

Admin only, under `/api/v1/apps/installed/{stack}/photos` (409 for an app without
`[user_photos]`):
- `GET` → `PhotosState { users: [{user, folder, mounted, library?}], api?: {key, error?},
  allow_list? }` (`library`: `linked`, `missing`, `no-account`, `ambiguous`, `other-owner`;
  `allow_list` is PiGallery2's per-user value, `/{user}/*`).
- `POST …/sync` → task `apps.photos-sync`: folders and ACLs, a new render and `up` only when the
  list changed (the previous files come back on a failure), then the libraries. 409 when the
  installed version differs from the catalogue's (update first).
- `POST …/key` `{key}` → 204.

`InstalledApp.user_photos` tells the UI to show the section ("Photos from the RyNAS app" in
the installed app's drawer: users wired, one line per user, the key field for Immich, the
allow-list hint for PiGallery2, Sync).

## 8. Open points

- New users are mounted at the next Sync (or install/update of the app); rynas does not yet
  sync by itself when a user is created.
- An update keeps the stored list: after updating PiGallery2 from 1.x the folders appear after
  a Sync.
- Per-user allow lists in PiGallery2 stay manual (no API key there).
