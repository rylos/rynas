# Encrypted shared folders (phase E1)

Status: implemented 2026-11-08 (backend, API, UI for create / unlock / lock / recovery key /
password / automatic unlock). Decision row: `../phase0/05-decisions.md` (D-CRYPT). Earlier
research: `../research/encryption-compression-dedup.md`. Gap list: `dsm-migration-gaps.md`
row E.

A volume can already be encrypted as a whole (LUKS2, D-CRYPT phase 1). This adds one
encrypted folder inside an ordinary volume: a share whose files are stored encrypted and that
can be locked while the rest of the NAS keeps running. It works on any Debian machine with
`gocryptfs` from the Debian archive; nothing here depends on the hardware.

## 1. Research (2026-11-08)

### 1.1 The kernel and the tools

| Option | State in late 2026 | Verdict |
|---|---|---|
| fscrypt on btrfs | The 43-patch series "btrfs: add fscrypt support" (Daniel Vacek, v7, May 2026) is still under review; Eric Biggers asked for more work in June 2026 and objected to the support depending on `CONFIG_BTRFS_EXPERIMENTAL`. Only a generic fscrypt clean-up landed, in Linux 7.3 (Aug 2026). Debian 13 ships 6.12. | Not usable before a Debian kernel carries it as stable: earliest Debian 14 (2027). The design keeps the backend behind one trait so it can be swapped. |
| eCryptfs | Orphaned in the kernel since 2023, removal planned; 143-byte name limit. | No. |
| **gocryptfs** | Debian 13: 2.5.1, trixie-backports and forky: 2.6.1 (Aug 2025: go-fuse 2.8, `-force_owner` fix). 2.5.0 (Jan 2025) fixed `-reverse` name bugs, added a btrfs fragmentation workaround and `-init -masterkey`. | Chosen, labelled **experimental** in the UI (FUSE, single external audit). |

gocryptfs facts that shape the design (man page and source, v2.6):

- **Audit**: one external audit, Taylor Hornby, March 2017 (defuse.ca). Its findings about
  file-content integrity across files were addressed or documented upstream; there has been
  no newer audit. Hence "experimental".
- **Passwords**: read from `-passfile` (first line), `-extpass`, or standard input when it is
  not a terminal; standard input is read one byte at a time up to the newline
  (`readLineUnbuffered`), so several secrets can follow each other on one pipe; at most 2048
  bytes; never on the command line.
- **Master key**: `-init -masterkey stdin` takes a key we generate (32 bytes, printed as 8
  groups of 8 hex digits), so rynas never parses gocryptfs output for it.
  `-passwd -masterkey stdin` sets a new password with only the master key: that is the
  recovery path. `-passwd` leaves `gocryptfs.conf.bak` wrapped with the **old** password:
  rynas deletes it after a change (otherwise a leaked old password still opens the folder).
- **Exit codes**: 12 wrong password, 6 cipher dir not empty, 10 mount point not empty, 23/24
  config file unreadable/unwritable.
- **Mount options**: `-allow_other` (implies `default_permissions`; root needs no
  `user_allow_other`), `-acl` (POSIX ACLs enforced and stored as encrypted xattrs: needed by
  the rynas permission matrix), `user.*` xattrs are supported (Samba `streams_xattr`, DOS
  attributes), `security.*` are not. `-noprealloc`: on btrfs preallocation gives no
  guarantee anyway (copy-on-write) and costs write speed. `-idle` exists but rynas does not
  use it: a folder that locks itself would break Samba and apps without a word.
- **Daemon**: without `-fg` the parent waits until the child has mounted, then exits 0 (or
  with the child's exit code): a systemd `Type=forking` service starts exactly when the mount
  is there.
- **Speed**: AES-GCM uses AES-NI (Celeron J4125 has it); CPUs without it can use
  `-xchacha`. The project lists no NAS-class numbers; expect the FUSE round trips, not the
  cipher, to limit small-file work. Shown as a note, not promised.
- **Storage side effects**: every file and name is unique ciphertext, so btrfs compression and
  deduplication gain nothing; snapshots, replication and backups of the ciphertext directory
  work and stay encrypted (restoring needs the password or the recovery key).
- **Samba on FUSE**: needs `-allow_other`; Samba 4.22 defaults to `kernel share modes = no`
  and `kernel oplocks = no`, which suits FUSE. **NFS on FUSE**: the export needs an explicit
  `fsid=` (rynas already writes one per share) and the `mp` option exports the path only
  while it is a mount point (exports(5)).

### 1.2 How others present it

| System | What it does | Ideas kept / left |
|---|---|---|
| DSM shared folder | eCryptfs per share, password chosen at creation, a key file downloaded at once, Key Manager to mount several at once, auto-mount at boot with the key kept on the NAS ("machine key") or on a USB drive; 143-byte names; the share disappears while unmounted. Elcomsoft (2023): auto-mount keeps the key on the box, so it protects little against theft of the whole NAS. | Kept: password + recovery key at creation; optional automatic unlock with a clear warning; locked share disappears from clients. Left: the 143-byte limit (gocryptfs has none), plaintext key file on the NAS. |
| QNAP QTS / QuTS | Encryption only at folder creation; unlock with password or uploaded key file; "save encryption key" for unlock at boot; locked folders shown with a lock in the list; File Station unlock. | Kept: encryption chosen at creation only; status and unlock in the folder list. |
| TrueNAS SCALE | ZFS native encryption per dataset; passphrase datasets lock/unlock from the Datasets screen; key datasets have "Export Key"; locked datasets' SMB shares disappear; unlock fails when the mount path exists and is not empty; check that the dataset is not in use before locking. | Kept: refuse to lock while busy, never mount over a non-empty folder, shares follow the lock state. |
| openmediavault | LUKS per device (plugin); no per-folder encryption. | — |
| Unraid | One LUKS key for all array disks; no per-share encryption. | — |

Sources: btrfs fscrypt v7 review (lkml.iu.edu/2606.0/01908.html, lkml.iu.edu/2606.1/06607.html),
Linux 7.3 fscrypt (phoronix.com/news/Linux-7.3-FSCRYPT); gocryptfs README/changelog and man page
(github.com/rfjakob/gocryptfs, Documentation/MANPAGE.md), `internal/readpassword/read.go`,
`masterkey.go`; Debian tracker (tracker.debian.org/gocryptfs, ftp-master
gocryptfs_2.6.1-1~bpo13+1); exports(5) (man7.org); systemd.exec(5) credentials and
systemd-run(1) `--expand-environment` (freedesktop.org); DSM Key Manager
(kb.synology.com …/file_share_key_manager), Elcomsoft "Volume Encryption in Synology DSM 7.2"
(blog.elcomsoft.com, 2023-06); QNAP QTS 5.1 "Encrypting a shared folder", "Locking or
unlocking an encrypted shared folder" (docs.qnap.com); TrueNAS SCALE "Encrypting Datasets"
(truenas.com/docs/scale/datasets/encryption); Unraid "Data encryption" (docs.unraid.net).

## 2. Model

- **Where**: an encrypted folder `<name>` on a place (a rynas volume `/<label>` or the system
  volume `/srv/rynas/system`, D-ROOTVOL) has
  - its **ciphertext** in the subvolume `<place>/.rynas-crypt/<name>` (parent folder 0700
    root, hidden from the folder lists like every dot name; compression property `none`), and
  - its **mount point** `<place>/<name>`: a plain folder, mode 000, immutable (`chattr +i`),
    so while locked nobody, root and containers included, can write into it.
- **Share**: an ordinary share with `path = <place>/<name>`, `encrypted: true`, no
  `subvolume`, no `show_snapshots` (the snapshots of an encrypted folder are snapshots of its
  ciphertext). A share whose path is an encrypted folder is marked encrypted on validation.
- **State** (`ShareStatus.crypt`): `locked` (ciphertext there, not mounted), `unlocked`
  (`fuse.gocryptfs` mounted at the path, from `/proc/self/mountinfo`), `missing` (no
  `gocryptfs.conf`). Nothing about the state is stored: it is read from the system.
- **Backend trait** `rynas_services::crypt::CryptHost`: `HostCrypt` on the machine,
  `MemoryCrypt` for tests and `--dry-system` (a future fscrypt backend implements the same).

### 2.1 Mounting (never inside rynasd)

Unlock starts a transient service through the shared builder (`rynas_tasks::Outside::daemon`):

```
systemd-run --unit=rynas-crypt-<place>-<name> --service-type=forking --quiet
  --property=LoadCredential=pass:/run/rynas/crypt/<unit>.pass --property=TimeoutStopSec=30
  -- /usr/bin/gocryptfs -allow_other -acl [-noprealloc] -passfile ${CREDENTIALS_DIRECTORY}/pass
     -- <ciphertext> <mount point>
```

- The password goes to a 0600 file in `/run/rynas/crypt` (tmpfs, 0700 root) only for the
  start: systemd copies it into the unit's private credential directory (non-swappable,
  readable by the unit only, gone when it stops), then rynasd overwrites and deletes the file
  whatever the outcome. It is never in argv, the environment or a log.
- `systemd-run` returns when gocryptfs has mounted (forking type). A failure reads
  `ExecMainStatus`: 12 → wrong password (400 `crypt_wrong_password`, answered after a
  1-second pause; unlocks are serialised), anything else → error with the unit name for the
  journal; the failed unit is reset.
- **Lock**: Samba connections to the share are closed (`smbcontrol smbd close-share`), then
  `umount <mount point>` runs outside the sandbox (`Outside`, never lazy). A busy folder is
  refused (409 `crypt_busy`, with the containers using it); gocryptfs exits by itself after
  the unmount. Stopping the unit is not used: gocryptfs falls back to a lazy unmount on
  SIGTERM, which would leave open files readable.
- **Services follow the state without a configuration apply**: the share's Samba section
  carries `root preexec = /usr/bin/mountpoint -q <path>` with `root preexec close = yes`
  (connections are refused while locked), the NFS export carries `mp` (exported only while
  mounted) and rynas runs `exportfs -ra` after each lock and unlock. The ACL matrix of a
  locked folder is skipped during an apply (logged) and written right after the next unlock;
  state drift ignores it while locked.

### 2.2 Recovery key

The master key is generated by rynasd (getrandom, 32 bytes) and given to `gocryptfs -init` on
standard input. It is kept in rynasd's memory for one hour and handed out **once**
(`reveal-recovery-key`), like the LUKS recovery key of a new volume; the UI shows it right
after the creation with copy / download and a mandatory "I stored it" confirmation. It is
never written to disk by rynas. With it, "Change password" works without the old password.
It cannot be shown again later (an export would need it to be stored).

### 2.3 Automatic unlock after a restart (opt-in)

Off by default: after a restart every encrypted folder is locked. When turned on (at
creation, or later with the password, which rynas checks first), the password is sealed with
`systemd-creds encrypt` (`--name` bound to the folder) into
`/var/lib/rynas/crypt/<unit>.cred` (0600): with a TPM2 it is bound to this machine's TPM and
the host key, else to the host key `/var/lib/systemd/credential.secret` on the system disk.
rynasd unlocks those folders when it starts. The UI says what this means: it protects data
disks that leave the NAS (sold, returned under warranty, stolen without the system disk), not
a NAS stolen whole (without TPM2) or someone with root access. A key on a USB drive is left
for later.

### 2.4 Package

`gocryptfs` (Debian main) is installed on demand from the encryption section of the share
dialog: task `crypt.install`, `apt-get install gocryptfs` in a transient unit (the same
helper as System > Updates). `GET /api/v1/crypt` tells whether it is installed, its version
and whether a TPM2 is present.

## 3. API

| Method | Path | Body | Answer |
|---|---|---|---|
| GET | `/api/v1/crypt` | — | `CryptInfo` |
| POST | `/api/v1/crypt/install` | — | 202 task `crypt.install` |
| POST | `/api/v1/crypt/folders` | `CryptCreateRequest {volume, name, password, auto_unlock}` | 202 task `crypt.create` (creates, initialises, mounts) |
| POST | `/api/v1/crypt/folders/{volume}/{name}/reveal-recovery-key` | — | `RecoveryKey`, once within the hour |
| POST | `/api/v1/shares/{share}/unlock` | `CryptUnlockRequest {password}` | `ShareView`; 400 `crypt_wrong_password` |
| POST | `/api/v1/shares/{share}/lock` | — | `ShareView`; 409 `crypt_busy` |
| POST | `/api/v1/shares/{share}/crypt/password` | `CryptPasswordRequest {current, recovery, new_password}` | 204; 400 `crypt_wrong_password` |
| PUT | `/api/v1/shares/{share}/crypt/auto-unlock` | `CryptAutoUnlockRequest {enabled, password}` | `ShareView` |

The share actions work on the applied share. The UI creates the folder first (task), then
stages the share with `encrypted: true` and applies it, then shows the recovery key. An
encrypted folder whose share is deleted with its folder is locked, unprotected and removed
with its ciphertext; a folder kept without share stays encrypted and can be shared again.

## 4. Limits shown in the UI

Experimental; files are encrypted one by one (FUSE), slower than a plain folder on small
files; compression and deduplication give nothing; snapshots, replication and backups of the
folder hold ciphertext (restores need the password or the recovery key); apps and Docker
containers using the folder see an empty, read-only folder while it is locked; encryption is
chosen when the folder is created (an existing folder is not converted: copy the files into
a new encrypted folder).

## 5. Not in this phase

- Converting an existing folder in place (copy into a new encrypted folder instead).
- Key on a USB drive, Tang/Clevis, KMIP.
- Unlock with the recovery key directly (reset the password with it, then unlock).
- Snapshot jobs and backups that pick the ciphertext subvolume from the share (they can be
  pointed at `<place>/.rynas-crypt/<name>` by path today).
- fscrypt backend once btrfs supports it in a Debian kernel.
