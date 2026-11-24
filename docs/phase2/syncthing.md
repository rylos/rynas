# Syncthing integrated (S1)

Status: design, 2026-11-24; S1a to S1d implemented the same day (S1d still to verify on a test
machine, below). Item S1 of
`dsm-migration-gaps.md` (round 2).

A NAS leaving a cloud-drive style service keeps PCs and phones in sync with Syncthing. rynas runs
Syncthing as a catalogue app and adds what Syncthing's own web interface lacks on a NAS: the
state of every folder and device next to the shares, notifications, versions from the share's
snapshots, and encrypted shares that pause their folders while locked. Adding devices and folders
stays in Syncthing's interface, which already does it well.

## 1. Research summary

Other NAS systems (TrueNAS, Umbrel, Unraid, CasaOS, the old OpenMediaVault plugin) deploy the
container and link to Syncthing's interface; none shows folder or device state in its own UI and
none reports conflicts. Syncthing 2.x facts used below (docs.syncthing.net, source on `main`,
2026-11-24):

- REST with `X-API-Key`; `STGUIAPIKEY` / `--gui-apikey` overrides the key in `config.xml`, so
  rynas injects its own key and never reads the file at run time.
- `PATCH /rest/config/folders/{id}` (since 1.12) merges top-level fields: `{"paused": true}`
  pauses a folder. `/rest/config/restart-required` after a change.
- State: `/rest/db/status?folder=` (expensive: use events), `/rest/folder/errors`,
  `/rest/system/connections` (no last seen), `/rest/stats/device` (`lastSeen`,
  `lastConnectionDurationS`), `/rest/stats/folder` (`lastScan`, `lastFile`).
- Events: `GET /rest/events?since=&timeout=&events=` with one stable mask (one buffer of 1000
  events per mask, created on first use, memory only: ids restart with Syncthing, a gap or a
  smaller id means a full re-read). `/rest/events/disk` carries `LocalChangeDetected` and
  `RemoteChangeDetected`.
- No conflict event: conflict copies are named `<base>.sync-conflict-YYYYMMDD-HHMMSS-<SHORTID>.<ext>`
  (time of the device that made the copy, short id of the device whose change lost); they show
  up as `ItemFinished` / disk events with that pattern; Prometheus `/metrics` has
  `syncthing_model_folder_conflicts_total{folder}`. `maxConflicts` default 10.
- Safety: a folder whose path or `.stfolder` marker is missing goes to state `error` and is not
  scanned, so nothing is deleted on the peers. The danger is a marker created by hand in a wrong
  or empty folder. `.stfolder/syncthing-folder-<sha256(id)[:3]>.txt` names the folder.
- Moving an instance without pairing again: keep `cert.pem`/`key.pem` (device id),
  `config.xml` and the database (`index-v2`, SQLite in 2.x); change folder paths with the folder
  id unchanged, then rescan. Official image: `STHOMEDIR=/var/syncthing/config`, PUID/PGID,
  `/var/syncthing` chowned at start; linuxserver image: `/config`.
- Syncthing versioning keeps only files replaced or deleted by remote changes, in `.stversions`
  inside the folder. btrfs snapshots keep local edits too, at almost no cost.
- A mount made on the host after a container started is invisible inside it unless the bind uses
  `propagation: rslave`. Measured on a test NAS (2026-11-24, a tmpfs in place of gocryptfs;
  propagation does not depend on the filesystem): an rslave bind of the share path made while
  the share is not mounted sees a later mount, loses it on unmount and sees it again on
  remount; one made while the share is mounted keeps the plaintext after the host unmounts it;
  an rslave bind of a strict parent loses the mount in both cases; an rprivate bind never sees
  a later mount and keeps a copy made before.

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| 1 | Official image `syncthing/syncthing` (already the catalogue entry), bridged ports, GUI behind the rynas proxy | same as today; the entry checks ports and proxies the GUI |
| 2 | Each synchronized share mounted at `/shares/<share>` in the container; the list is a setting of the app (the entry's `[share_set]`, `PUT /api/v1/apps/installed/{stack}/share-set`), the compose file is rewritten and the container recreated when it changes | folder paths inside Syncthing stay the same when a share moves to another volume |
| 2b | Syncthing runs as a chosen NAS user (`run_as = "user"` in the entry), by default the owner of the synchronized files, not as `docker-apps`; the shares are granted to that user | files from the PCs and phones belong to their owner, as with the package on the old system; no group ACL needed on every file |
| 3 | rynas sets `STGUIAPIKEY` from an app secret (`ST_API_KEY`, generated at install): it lives in the stack's `.env` (0600, root) like every catalogue secret, not in a sealed store | no key in the compose file, no `config.xml` read; whoever can read the stack folder as root can read it, as for every app's secrets |
| 4 | Status from events, one long-poll per instance with a fixed mask, a full re-read at start, on a gap and after a Syncthing restart; device last seen from `/rest/stats/device` every 10 minutes | cheap, no `db/status` polling |
| 5 | Versions = the share's snapshots; Syncthing versioning left off; the page warns when a synchronized share has no snapshot schedule | local edits are kept too |
| 6 | Encrypted shares: bind with `propagation: rslave`; lock = pause the share's folders, wait for idle, restart the app's container around the unmount (a bind made while the share was mounted keeps it), lock; unlock = check the marker, resume only the folders rynas paused | after the restart the bind follows the share; the app is down for a few seconds per lock; no plaintext copy stays in the container |
| 7 | Adding devices and folders stays in Syncthing's interface; rynas offers pause/resume, rescan, open GUI | no second editor of the same config |
| 8 | rynas never creates `.stfolder` | the one way to spread deletions |

## 3. Adoption of an existing instance

Adoption is generic: any Syncthing a user already runs becomes the rynas app without pairing
the devices again. Sources, found by `POST /v1/syncthing/adopt/preview` in this order:

| Source | How it is found | Volume table (container path → host path) |
|---|---|---|
| the rynas app with the older layout (one share at `/data`) | the installed catalogue app | from the app's compose file; the update moves each folder to `/shares/<share>` |
| a container on this host (official image, linuxserver, any other) | `docker ps` / `inspect`: image or command `syncthing`; its home from the mounts (`/var/syncthing`, `/config`) | from the container's mounts, no typing |
| a service on the host (`syncthing@<user>`, a user unit, a distribution package) | systemd units; home in `~/.local/state/syncthing` or `~/.config/syncthing` | none: folder paths are host paths already |
| a folder copied from another machine (a DSM or QTS package, another NAS, a backup) | a path the user gives | typed in, or read from a compose file or `docker inspect` output found next to it |
| nothing | — | fresh install with the shares chosen |

Preview steps, the same for every source:

1. finds `cert.pem`, `key.pem`, `config.xml`, the database (`index-v2`, or a 1.x
   `index-v0.14.0.db` that Syncthing migrates on its first 2.x start, slowly);
2. lists the folders (id, label, old path, type, devices) and maps each old path, through the
   volume table, to a host path, then to `/shares/<share>/<rest>`; a folder outside every share
   stays unmapped and is shown (make a share of it, or leave it paused);
3. checks on the host that each mapped folder has `.stfolder` with the file of its id; a folder
   without it is paused after adoption and never "fixed";
4. proposes `.stignore` changes: other systems' patterns (`#snapshot`, `@eaDir`,
   `.SynologyWorkingDirectory`, `@Recycle`, `.@__thumb`, `.streams`) out, `(?d)#recycle` in
   (rynas snapshots live outside the share);
5. proposes the user the app runs as: the NAS user owning most synchronized files (shown with
   how many top-level entries have another owner); the request's `run_as` overrides it. A uid
   that no NAS user has is a problem: give a NAS user that uid (the ownership remap of Users &
   Groups) or choose another user. The copied home is chowned to that user (keys 0600), each
   mapped share gets that user's read and write access (share grants with a user subject),
   and files arriving from the devices belong to them, as on the old system. An installed app
   keeps its user (changing it is not supported yet: uninstall and install again); the older
   catalogue layout, updated in place, keeps the apps' account.

`POST /v1/syncthing/adopt` (task `syncthing.adopt`): the old instance is stopped first (two
instances with one device id fight), its home copied into the app's config folder (owned by the
app user; the old one left untouched as a fallback), folder paths rewritten in `config.xml` with
Syncthing stopped, the app installed or reconfigured with the shares, started, every folder
rescanned. For the rynas app with the older layout the home stays where it is and the paths are
changed with `PATCH /rest/config/folders/{id}` after the container is recreated.

Test matrix: official image, linuxserver image, the old rynas layout, a host service, a copied
DSM home, a 1.x database.

## 4. Page and API

Page "Syncthing" (Services), shown when the app is installed:

- folders: label, share and subfolder, type, state (idle, scanning, syncing with %, error,
  paused, paused while the share is locked), errors (first lines of `/rest/folder/errors`), last
  scan, devices with their completion, conflict copies count with the list;
- devices: name, connected (address, local or relay), last seen, folders;
- actions: pause/resume a folder, rescan, open Syncthing; settings: devices unseen alert days
  (default 7), conflicts alert on/off.

API: `GET /v1/syncthing` (`SyncthingState`), `POST /v1/syncthing/folders/{id}/pause|resume|rescan`,
`GET|PUT /v1/syncthing/settings`, the adopt routes above. Admin only.

## 5. Notifications

| Code | Level | When |
|---|---|---|
| `syncthing.conflict` | warning | new conflict copies in a folder (grouped for 10 minutes, list of files) |
| `syncthing.folder_error` | error | a folder in `error` for more than 5 minutes (one notification, then `…_recovered`) |
| `syncthing.device_unseen` | warning | a device seen before and not connected for N days |
| `syncthing.down` | error | the API does not answer for 10 minutes while the app runs |

## 6. Phases

| Phase | Content |
|---|---|
| S1a | catalogue entry with `/shares/<share>` mounts (list setting, rslave for encrypted shares), `STGUIAPIKEY`, adoption preview and task. Done 2026-11-24: entry 2.0.0 (`[share_set]`, migration from the single `/data` share), API in `../phase1/plan.md` "API v1: Syncthing adoption (S1a)"; the wizard picks the shares, adoption is API only for now |
| S1b | rynasd client and event loop, `GET /v1/syncthing`, page. Done 2026-11-24: API and loop in `../phase1/plan.md` "API v1: Syncthing state (S1b)"; the page has the adoption dialog (candidates, a copied folder with typed volume lines, preview, user, stop and `.stignore` choices, task). Deviations: device stats also at every full read; a full read after any connection error too (a restart drops the long poll, and its new ids would never pass the old `since`); several installed stacks of the entry are supported (`?stack=`), the first by name is the default |
| S1c | notifications and settings. Done 2026-11-24: codes in `../phase1/plan.md` "Event codes" (category `app`, source `syncthing`), evaluated every 30 s from the model; added `syncthing.device_seen` (quiet resolution), `syncthing.up` and `syncthing.folder_kept_paused` (S1d). The settings are in `<state>/syncthing/state.json`, not a configuration key: they render nothing and need no apply |
| S1d | lock/unlock integration: pause before lock, resume after unlock and after the automatic unlock at boot; `containers_using` accepts an rslave bind of a share that follows the host. Done 2026-11-24 (tests with a fake REST server and `docker inspect` JSON): the lock finds the share and refuses it while a running container holds a private bind (`rprivate`/`private`) of it, inside it or of a folder holding it, or when Docker cannot be asked; an `rslave`/`slave` bind of a folder holding it is no obstacle. Then it marks the share "being locked" (the event loops treat it as locked until the lock ends), pauses the running Syncthing folders inside it in every instance at once (remembered first and again after each answer, per stack, in `<state>/syncthing/state.json`), waits until none scans or syncs (one deadline, 10 s; a silent Syncthing is skipped with a warning), stops the running containers with an `rslave`/`slave` bind of the share or inside it (**the Syncthing app is briefly restarted**), unmounts, checks `/proc/<pid>/mountinfo` of every running container for the gocryptfs mount (an error naming them if one still has it), and starts the stopped containers again in every case. A failed lock resumes the paused folders (unless the share stayed locked). The manual unlock and the automatic one (boot unit and rynasd start) resume, in the background, the remembered folders whose marker file is there; a missing marker leaves the folder paused with `syncthing.folder_kept_paused`. Every full read pauses the running folders of a locked share and resumes remembered ones of an unlocked share (Syncthing may start before the boot unlock, or rynasd may not watch it yet); a remembered folder found running (the user resumed it) is forgotten; a folder the user paused is never resumed |

To verify on a test machine before S1d is called done: the whole lock with the Syncthing app
running (pause, restart, unmount, mountinfo check, start) and the unlock that resumes its
folders, with a real gocryptfs folder.
