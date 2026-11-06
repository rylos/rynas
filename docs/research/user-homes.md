# User homes on a data volume: research and decision

Checked on 2026-11-06. Refines D-IDENTITY ("homes as subvolumes"), see the decision log entry
D-HOMES in `../phase0/05-decisions.md`.

## 0. Problem and summary

rynas created every user's home as `/home/<user>`, on the system disk. On the reference box
(DS920+) that is a 232 GB NVMe root next to a 3.7 TB data array on `/volume1`: personal files
end up on the wrong disk, outside the snapshots and the backups of the data volume, and can
fill the root filesystem. D-IDENTITY already said "homes as subvolumes" (`homes/<user>`, one
squota per home in phase 2); the code and `plan.md` did not.

Decision, in short:

- A **"User homes" setting** (key `homes`, pending/apply): on/off and the volume that holds
  the homes folder: any mounted local filesystem that is not a system path (btrfs, ext4, xfs,
  f2fs, zfs), rynas volumes listed first.
- Homes live in **`<volume>/homes/<user>`**: a **btrfs subvolume per user** when the volume is
  btrfs (snapshots, send/receive and the phase 2 squota work per home), a plain folder
  otherwise. The homes folder is `root:root 0711`, each home `<user>:users 0700`.
- The passwd entries name a **stable path, `/srv/rynas/homes/<user>`**, through a **symbolic
  link** `/srv/rynas/homes → <volume>/homes`. Changing the volume later changes the link, not
  passwd, Samba, SSH or the file manager.
- **New users** get their home there when the service is on. **Existing homes never move by
  themselves**: an explicit, per-user (or "all") **move** shows a plan, copies with rsync
  (hard links, ACLs, xattrs, numeric ids), verifies, switches passwd, and keeps the old folder
  renamed until the admin **confirms** (deleted) or **undoes** (put back).
- **SMB `[homes]`** (off by default, own recycle bin) follows the passwd home (`path = %H`);
  the file manager's `home` share follows it too (resolved to the real folder).
- Deleting a user with "also delete the home" deletes the subvolume. Turning the service off
  deletes nothing.

## 1. What the main NAS systems do

### 1.1 DSM (Synology), User Home service

- Control Panel › User & Group › Advanced › **User Home**: a single "Enable user home service"
  switch, a **location** (the volume), and "Enable Recycle Bin" for the homes.
- It creates a shared folder **`homes`** on that volume, with **one folder per user**
  (`/volume1/homes/<user>`). Each user sees it as **`home`** (only their own folder); members of
  `administrators` see the whole **`homes`** folder.
- Stable path: the system names homes through **`/var/services/homes`**, a **symbolic link** to
  `/volumeN/homes` (also `/var/services/homes/<user>` in passwd). Services and apps never hard
  code the volume.
- Changing the location **moves the data** to the new volume (a long operation, the service is
  unavailable meanwhile).
- Disabling the service hides `home`/`homes` from users; the folder and its data stay. The
  `homes` shared folder cannot be deleted while the service is on.
- Deleting a user deletes their home folder (the dialog warns).
- Domain/LDAP users get homes under `homes/@DH-<DOMAIN>/…` when "Enable home service for
  domain users" is on (a separate switch).
- The home is the root of per-user apps (Drive "My Drive", Photos personal space).

Sources: DSM Knowledge Center (User & Group › Advanced; "home" vs "homes" folders,
[Drive: My Drive vs homes](https://kb.synology.com/en-my/DSM/tutorial/Drive_difference_between_homes_My_Drive_home_folders)),
field experience administering DSM 7.x boxes (the `/var/services/homes` link, the
`@DH-<DOMAIN>` layout and the location change are from that experience: the Knowledge Center
pages render client-side and could not be quoted).

### 1.2 TrueNAS SCALE

- Per user: **Home Directory** = a path on a dataset (`/mnt/tank/homes`), "Create Home
  Directory" makes `<path>/<user>`; **home directory permissions** are a field (default owner
  only). Users without a home get `/var/empty` (SMB-only users need none).
- SMB: one share can be the **home share** (`home: true`, renamed `homes`, `path_suffix %U`):
  `//nas/homes` resolves to `<share path>/<user>`; per-user datasets on request.
- No indirection: the passwd home is the dataset path; moving means ZFS replication by hand.

Sources: [TrueNAS 25.04 API sharing.smb](https://api.truenas.com/v25.04/api_events_sharing.smb.query.html),
[24.10 API (home share)](https://api.truenas.com/v24.10/api_events_sharing.smb.query.html),
[forum: home share paths](https://www.truenas.com/community/threads/smb-getting-two-different-home-folders.101850/).

### 1.3 openmediavault

- Users › Settings › **User home directory**: enable and pick a **shared folder** as root of all
  homes; new users get `<shared folder>/<user>` with Debian's `/etc/skel`.
- Changing it **updates the passwd home of every existing user** but "the previous home
  directory content will NOT be moved … This has to be done manually". Disabling unsets the
  home of existing users; content is not deleted.
- SMB: Services › SMB › Settings › Home directories (enable, browseable, recycle bin): Samba's
  `[homes]`.

Source: [OMV docs, Users](https://docs.openmediavault.org/en/latest/administration/users.html).

### 1.4 Unraid, QNAP (for completeness)

- Unraid: no per-user homes for share users (only root's). QNAP QTS: "Home folder" service
  like DSM (`homes` share, per-user `home`, volume choice, recycle bin).

### 1.5 What we take

| Idea | From | In rynas |
|---|---|---|
| One switch + a volume, a `homes` folder with one subfolder per user | DSM, QNAP, OMV | yes |
| Stable path through a symbolic link, volume can change underneath | DSM (`/var/services/homes`) | yes: `/srv/rynas/homes` |
| Per-user dataset | TrueNAS (optional) | yes, always on btrfs: one subvolume per home (squota in phase 2) |
| `[homes]` with its own recycle bin | DSM, OMV | yes, off by default |
| Owner-only permissions, configurable | TrueNAS | owner only (0700); admin access through rynas, see §3 |
| Never move data silently | (OMV's footgun is the counter-example) | explicit move task with plan, verify, keep, undo |
| Disabling keeps data | DSM, OMV | yes |
| Deleting a user deletes the home | DSM | only when asked (`remove_home`, typed confirmation), as today |
| Moving the whole homes folder to another volume | DSM | phase 2 (refused while the folder holds homes) |

## 2. Debian specifics

- **useradd**: `-d <home>` and `-m` (create, copy `/etc/skel`, `HOME_MODE` from `login.defs`,
  0700 on Debian 13) or `-M` (don't). rynas creates homes in the homes folder itself (a
  subvolume cannot be made by useradd) and runs `useradd -M`; homes elsewhere keep `useradd -m
  -K HOME_MODE=0700`.
- **`/etc/adduser.conf`** (`DHOME=/home`) and `/etc/default/useradd` (`HOME=/home`) are left
  alone: rynas passes `-d`; changing the system default would surprise admins who create
  accounts by hand and is not needed.
- **pam_mkhomedir**: not used. It creates a missing home at first login (useful for domain
  users), but with `umask`/skel only, never a subvolume, and it would silently recreate a home on
  the system disk below an unmounted volume path. Domain user homes (phase 3) will be created by
  rynas the same way (`homes/<DOMAIN>/<user>`, Samba `template homedir =
  /srv/rynas/homes/%D/%U`).
- **Skeleton**: `/etc/skel` (Debian: `.bashrc`, `.profile`, `.bash_logout`) is copied only for
  users with shell access; SMB/SFTP-only users get an empty home (nothing in it is ever read,
  and Windows/macOS clients would show the dotfiles once "hidden files" are visible).
- **Symbolic link vs bind mount** for the stable path. A bind mount (`/srv/rynas/homes` mounted
  from `<volume>/homes`) needs a mount unit ordered after the volume, and when the volume is
  missing the mount point is an empty directory **on the system disk** that logins and SMB
  would happily fill. A symbolic link needs nothing at boot, is what DSM does, and when the
  volume is missing it **dangles**: logins and connections fail cleanly instead of writing to
  the wrong disk. Every consumer resolves it: Samba canonicalises the share path (`wide links =
  no` checks paths below the share root, the root itself may be a link), OpenSSH `StrictModes`
  realpath()s both the key file and `pw_dir` before comparing, the file manager resolves the
  home before opening it (its jail never follows links). The link is replaced atomically
  (`symlink` to a temporary name + `rename`). rynas never replaces a real directory at that path.
- **Why `/srv/rynas/homes`**: FHS `/srv` is "data served by this system"; a rynas sub-directory
  avoids clashing with the admin's own `/srv` content; `/home` stays untouched for accounts made
  outside rynas.
- **SSH/SFTP**: with `StrictModes` the home must not be group/other writable and owned by the
  user (0700 user:users is fine). A future SFTP chroot (`rynas-sftp` group, file-services
  plugin) cannot chroot into the home itself (sshd requires a root-owned, non-writable chroot
  path, every component); the homes folder (root, 0711) satisfies that, so the plan is
  `ChrootDirectory /srv/rynas/homes` + `ForceCommand internal-sftp -d /%u`: 0711 lets users
  reach their own folder without listing the others.
- **systemd-homed**: not used (LUKS/fscrypt home images per user, logins via homed's own
  records; incompatible with Samba accounts, NFS uids and admin-managed NAS users).
- **Our units**: `rynasd.service` already has `ProtectHome=no` (it creates and deletes homes
  below `/home`) and `ProtectSystem=yes` (so `/srv` and the volumes are writable).
  `rynas-web.service` keeps `ProtectHome=read-only`. The file manager worker runs as the user
  with `setresuid` and opens the real folder.
- **Processes and sessions**: a move is refused while the user has processes (any uid field
  in `/proc/<pid>/status`, which also catches smbd children working as the user) or SMB
  sessions (`smbstatus -b -j`), checked in the plan, again between the two copy passes and just
  before passwd changes.
- **Copy**: `rsync -aHAXx --numeric-ids` (hard links, POSIX ACLs, xattrs, one filesystem),
  first pass, live check, second pass with `--delete`, then a dry run (`--itemize-changes`)
  that must report nothing (the root folder's mode is left out: the copy is 0700) and equal
  counts of files, folders, links and bytes on both sides. Reflinks are not possible across
  filesystems (system disk → data volume), so no `cp --reflink`.

## 3. Permissions and admin access

- Homes folder `root:root 0711`; homes `<user>:users 0700`; `[homes]` with `valid users = %S`,
  `create mask 0600`, `directory mask 0700`.
- **Administrators do not get file access to other users' homes by default** (no ACL entry,
  no `homes` share for admins): they manage homes through rynas (location, move, delete, and
  the phase 2 quota and size report). An explicit "administrators can open all homes" option
  (DSM's `homes` share for `administrators`) is phase 2, with a clear warning: it is a privacy
  decision, not a default.

## 4. Lifecycle

| Event | Behaviour |
|---|---|
| Service turned on | `<volume>/homes` created (subvolume on btrfs, 0711), link `/srv/rynas/homes` set. Existing homes untouched. |
| New user, service on | home `/srv/rynas/homes/<user>`, created by rynas before `useradd -M` (subvolume, 0700, skel for shell users). An existing folder of that name is refused (left by a deleted user?). |
| New user, service off | `/home/<user>` (`useradd -m`), as before. An explicit home in the homes folder is refused. |
| Service turned off | nothing on disk changes; the link stays so moved homes keep working; `[homes]` goes off with it. |
| User deleted, keep home | `userdel`; the folder stays. |
| User deleted with home | `userdel`, then `btrfs subvolume delete` (or `rm` for a plain folder) of `<volume>/homes/<user>`. |
| Volume changed | refused while any passwd home is in the homes folder (phase 2: a "move the homes folder" task: per-home copy + link switch). |
| Home move | plan → copy → verify → passwd (`usermod -d`, through a `users` revision when rynas manages the user) → old folder `<home>.rynas-old` kept → confirm (deleted) or undo (old folder back, copy kept aside as `<homes>/.<user>.undone-<ms>`). The user cannot be deleted while a move waits. |
| Volume missing at boot | the link dangles: logins and SMB fail, nothing is written on the system disk. |

## 5. Phase 2

- squota per home subvolume (the "user quota" of D-IDENTITY) and `dfree command` for `[homes]`.
- Move of the whole homes folder to another volume (per-home copy with the same verification,
  then the link switch).
- Optional admin access to all homes, per-user size report in the homes tab.
- Homes for domain users (`homes/<DOMAIN>/<user>`).
