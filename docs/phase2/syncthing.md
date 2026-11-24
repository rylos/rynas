# Syncthing integrated (S1)

Status: design, 2026-11-24. Item S1 of `dsm-migration-gaps.md` (round 2).

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
  `propagation: rslave`.

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| 1 | Official image `syncthing/syncthing` (already the catalogue entry), bridged ports, GUI behind the rynas proxy | same as today; the entry checks ports and proxies the GUI |
| 2 | Each synchronized share mounted at `/shares/<share>` in the container; the list is a setting of the app, the compose file is rewritten and the container recreated when it changes | folder paths inside Syncthing stay the same when a share moves to another volume |
| 3 | rynas sets `STGUIAPIKEY` from its own sealed secret | no key in the compose file, no `config.xml` read |
| 4 | Status from events, one long-poll per instance with a fixed mask, a full re-read at start, on a gap and after a Syncthing restart; device last seen from `/rest/stats/device` every 10 minutes | cheap, no `db/status` polling |
| 5 | Versions = the share's snapshots; Syncthing versioning left off; the page warns when a synchronized share has no snapshot schedule | local edits are kept too |
| 6 | Encrypted shares: bind with `propagation: rslave`; lock = pause the share's folders, wait for idle, lock; unlock = check the marker, resume only the folders rynas paused | Syncthing keeps running for the other folders; no plaintext copy stays in the container |
| 7 | Adding devices and folders stays in Syncthing's interface; rynas offers pause/resume, rescan, open GUI | no second editor of the same config |
| 8 | rynas never creates `.stfolder` | the one way to spread deletions |

## 3. Adoption of an existing instance

`POST /v1/syncthing/adopt/preview` reads a folder holding a Syncthing home (official layout, the
linuxserver `/config` layout, or a package's `var` folder), plus the old volume table (container
path → host path, typed in or read from the old compose file):

1. finds `cert.pem`, `key.pem`, `config.xml`, the database (`index-v2`, or a 1.x
   `index-v0.14.0.db` that Syncthing migrates on its first 2.x start, slowly);
2. lists the folders (id, label, old path, type, devices) and maps each old path to
   `/shares/<share>/<rest>` through the volume table and the rynas shares; a folder without a
   share stays unmapped and is shown;
3. checks on the host that each mapped folder has `.stfolder` with the file of its id; a folder
   without it is refused for that folder (paused after adoption) and never "fixed";
4. proposes `.stignore` changes: DSM patterns (`#snapshot`, `@eaDir`, `.SynologyWorkingDirectory`)
   out, `(?d)#recycle` in (rynas snapshots live outside the share);
5. shows the PUID/PGID the app will run as (the owner of the shares, after an ownership remap)
   and the files with another owner.

`POST /v1/syncthing/adopt` (task `syncthing.adopt`) copies the home into the app's config folder
(owned by the app user), rewrites the folder paths in `config.xml` with Syncthing stopped,
installs or reconfigures the app with the shares, starts it, and rescans every folder. The old
instance must be stopped first (two instances with one device id would fight).

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
| S1a | catalogue entry with `/shares/<share>` mounts (list setting, rslave for encrypted shares), `STGUIAPIKEY`, adoption preview and task |
| S1b | rynasd client and event loop, `GET /v1/syncthing`, page |
| S1c | notifications and settings |
| S1d | lock/unlock integration: pause before lock, resume after unlock and after the automatic unlock at boot; `containers_using` accepts an rslave bind of a share that follows the host |

To verify on a test machine before S1d is called done: an rslave bind sees a gocryptfs mount made
later on the host and loses it on unmount.
