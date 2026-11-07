# rynas: identity and access (users, groups, quotas, AD domain)

## Summary

- **Phase 1.** Local users and groups are real Linux users, with the Samba password kept in sync in a single step inside `rynasd`. One DSM-style wizard, a share × users/groups matrix, an explained effective permission, shell login off by default.
- **Phase 2.** Honest quotas: squota per **shared folder** and per **home** (one subvolume per user). Per-owner usage computed by a nightly scan, not enforced. Then CSV import, password policies, mandatory 2FA per group, account expiry.
- **Phase 3 (business).** Active Directory join with **winbind + idmap rid** (no sssd), shares with Windows ACLs editable from Explorer, UI login with domain users and an AD group → rynas role mapping. LDAP client right after. A built-in LDAP server/DC is not needed.
- **Reference case.** An office Synology (DS1019+, DSM 7.4.1) joined to `EXAMPLE.LOCAL`, with a few dozen shares in Windows ACL mode and permissions granted to AD groups such as `ENGINEERING`/`PURCHASING`, plus a few per-user denies. rynas must be able to do exactly this. Source: field experience administering an AD-joined Synology.

---

## 1. Local users and groups

### Comparison

| | DSM 7.4 | TrueNAS 25.10 | OMV 7/8 | Unraid 7 |
|---|---|---|---|---|
| Creation | wizard: details → groups → share permissions → quotas → apps → speed limits → summary | a single form: UID, groups, home, shell, sudo, SSH keys, "SMB user", password-disabled login | simple form plus groups and shares tabs | user with per-share permissions only (SMB/NFS) |
| Home | "user home" service → `homes` share | per-user home, dataset of choice | optional `homes` | no |
| Password policy | strength, expiry 1–999 days, reminder | minimal | no | no |
| 2FA | mandatory for admins, users or groups | per-user 2FA | no | no |
| CSV import | yes (tab-separated TXT file) | no (via API) | yes (CSV) | no |
| Expiry/suspension | expiry date, "disable" | lock user | disable | no |
| Limits | 16,000 local users and 512 local groups; app permissions deny > allow; shares NA > RW > RO | n/a | n/a | n/a |

Sources: [DSM 7.4 Account, Identity and Privileges](https://www.synology.com/en-us/dsm/7.4/software_spec/account_identity_privileges), [TrueNAS Directory Services](https://www.truenas.com/docs/scale/25.10/scaletutorials/credentials/directoryservices).

### Keeping the Samba password in sync

- **TrueNAS.** On a password change the middleware computes the NT hash and stores it in its own configuration database, then regenerates `passdb.tdb`. The source of truth is the TrueNAS database, not Samba.
- **OMV.** The UI updates shadow and tdbsam together. The documentation admits, however, that the two passwords can drift apart and recommends `smbpasswd user` ([OMV Samba](https://openmediavault-docs.readthedocs.io/en/latest/administration/services/samba.html)).
- Note: `pdbedit` does not call the Unix password sync ([man pdbedit](https://www.mankier.com/8/pdbedit)).

**rynas proposal.** There is a single entry point, `rynasd::user.set_password`. In one transaction it does three things: `chpasswd` (yescrypt), then `pdbedit -a -t` with the password on stdin for tdbsam, then the audit record in SQLite.

- The NT hash is **not** stored in the rynas database. It is equivalent to the password and would end up in `.rynasbak` files.
- Any password change made outside rynas is rejected. An anti-drift check flags users in `/etc/passwd` that are missing from tdbsam, and vice versa.
- `unix password sync` stays off: rynas is the coordinator.

### Linux model

- Users with UID 1000–59999 (the Debian values in `/etc/adduser.conf`). Service users below 1000 or in the `rynas-svc` group.
- Shell `/usr/sbin/nologin` by default. A `rynas-ssh` group for those allowed to log in (sshd `AllowGroups rynas-ssh`). A `rynas-sftp` group with chroot and SFTP only.
- sudo only for the admin role, and off by default anyway.
- Real Unix groups: Samba uses them as they are, and so do NFS and POSIX permissions.

### rynas data model (SQLite)

```
user(uid, name, full_name, email, description, state[active|disabled|expired],
     expires_at, password_changed_at, must_change, shell_access[none|sftp|shell],
     totp_enforced(derived), source[local|ad|ldap], created_by)
group(gid, name, description, source, rynas_role[admin|operator|user|none])
membership(uid, gid)
share_acl(share_id, principal(sid|uid|gid), access[rw|ro|deny], custom_nt_acl?)
quota(target[share|home|user_usage_alert], limit_bytes, warn_pct)
app_permission(principal, app, allow|deny)
policy(password_*, 2fa_required_groups[], lockout_*)
```

The **effective permission** is computed server-side with the DSM rules: deny beats everything, then RW, then RO. The explanation shows the chain of origin, for example "RO from `Domain Users`, RW from `ENGINEERING` → RW". DSM does not explain it and TrueNAS does not compute it: this is better than both.

### Single wizard

```
┌ New user ─────────────────────────────────── 1 Details ▸ 2 Groups ▸ 3 Shares ▸ 4 Quotas ▸ 5 Apps ▸ 6 Summary
│ Name  [alice       ]  Full name [Alice Smith]  Email [...]
│ Password [••••••] [generate] ▓▓▓▓░ strong   ☐ change at first login
│ Account expiry [never ▾]   Shell access [none ▾]
│ ── 3 Shares ────────────────────────────────────────────
│ Share            Groups (inherit.)  User          Effective
│ drawings         RO (Domain Users)  [RW ▾]        RW  ⓘ
│ purchasing       —                  [— ▾]         none
└───────────────────────────────────────────────── [Back] [Next]
```

### CSV import

- Columns: `name,full_name,email,password|!generate,groups(;),quota_home,expires`.
- First a preview with per-row errors and a dry run. Then execution as a task, with a downloadable report of the generated passwords, shown only once.

### Password policy and 2FA

- Policy inspired by NIST 800-63B: minimum length 12, check against a list of breached passwords (offline, with optional `hibp` k-anonymity), no forced composition rules. These also apply to tdbsam, because everything goes through `rynasd`.
- Mandatory 2FA per group: at the next login the user is forced to enroll TOTP or a passkey.
- 2FA applies **only to the UI and API**. SMB cannot have 2FA: the UI must say so clearly.

---

## 2. Quotas

### What btrfs offers

| Mechanism | Granularity | Cost | Limits |
|---|---|---|---|
| classic qgroups | subvolume (+ hierarchy) | high: reference counting on every commit and on snapshot deletion | long rescans, "inconsistent" counts |
| **squota** (`btrfs quota enable --simple`, kernel ≥ 6.7; trixie ships 6.12) | subvolume | near zero | every extent stays charged forever to the subvolume that created it; exclusive = shared (it does not tell you how much snapshots retain); incompat flag on the filesystem |
| per-UID quotas | **do not exist** in upstream btrfs | — | — |

With squota a snapshot or a reflink does not move the charge. If the original is deleted while a shared copy exists, it stays charged to the source subvolume even after that subvolume has been emptied or deleted ([btrfs Qgroups](https://btrfs.readthedocs.io/en/latest/Qgroups.html), [LWN 944371](https://lwn.net/Articles/944371/)).

Squota keeps a "quota enablement generation" to handle extents written before it was enabled. It should therefore be **enabled when the volume is created**, or tested in the lab on an already-full volume.

### How DSM does it

- Synology's btrfs kernel is patched and has a **`btrfs usrquota`** subcommand (enable, enable-v1, show, limit, rescan) and `syno-usage`, i.e. a per-UID quota inside the filesystem ([wener.me notes](https://wener.me/notes/os/linux/fs/btrfs), unofficial source).
- Thanks to this, DSM offers on btrfs per-folder quotas, per-user quotas on a folder and per-user quotas on the volume. On ext4 only per-volume user quotas.
- Priority order: share quota > user quota > group quota ([nas-forum](https://www.nas-forum.com/forum/topic/60596-tuto-comprendre-et-utiliser-le-quota-dans-dsm)).
- **rynas cannot copy this**: it would need a patched kernel, which is out of the question on Debian.

### Other options

- **XFS/ext4** have real user and group quotas (`usrquota,grpquota`; XFS also project quotas). Useful only for non-btrfs volumes: the Frigate recordings volume (ext4/xfs) or a future "users" volume.
- **Samba.** smbd does not read btrfs qgroups, and `statfs` on a subvolume reports the filesystem's free space, not the limit.
  - Idea: rynas generates `dfree command = /usr/lib/rynas/smb-dfree` per share. The script returns the subvolume's squota limit and usage, so Windows sees "drawings: 2 TB, 300 GB free" instead of the pool's 40 TB.
  - `get quota command` serves to show the user quota in Windows, but enforces nothing.
- **Application quotas**: Immich (per-user quota via API, already in D-PHOTOS), Nextcloud and similar. rynas sets them to the same value.

### What to promise

| Phase | Feature | Enforced? |
|---|---|---|
| 1 | Per-share usage (squota if enabled, otherwise `btrfs fi du` as a task) | — |
| 2 | **Shared folder quota** (squota on the subvolume) + Samba `dfree` | yes |
| 2 | **Home quota** (`homes/<user>` is a subvolume with its own squota) = the "user quota" that is actually needed | yes |
| 2 | **Per-owner usage** across all shares: low-priority nightly scan (`statx`, sum of blocks per UID, hardlinked inodes counted once, snapshots excluded), stored in SQLite, threshold alert | **no**, report and alert only |
| 2 | Real user/group quotas on XFS/ext4 volumes | yes |
| 3 | App quotas (Immich) synced with the home quota | yes |

The UI must say: "Quotas apply to folders and homes. Per-user usage in shared folders is a report, updated at 03:00." Never promise a DSM-style "user quota on the volume".

---

## 3. Active Directory domain join

### Comparison

| | DSM 7.4 | TrueNAS 25.10 | OMV | Unraid |
|---|---|---|---|---|
| Join | DNS, domain, domain admin; advanced options: NetBIOS, preferred DCs (IP/FQDN in priority order), scheduled user list refresh | hostname ≤ 15, domain, auto site, **computer account OU**, user credential or Kerberos principal | no native support (community/manual) | SMB → "Yes (Active Directory)" with the array stopped; initial owner/group; does not fix existing permissions |
| idmap | proprietary, "UID/GID shifting" (LDAP) | AD, AUTORID, LDAP, NSS, RFC2307, **RID (default)**, TDB; configurable ranges | — | — |
| Trusts | two-way; nested groups up to 16 levels | trusted domains, one idmap per domain | — | — |
| Cache / status | offline cache | Enable Account Cache, Rebuild Cache, status, **Leave Domain** | — | join status (leave bug in 6.7) |
| Admin | up to 10 domain groups as local administrators | roles | — | — |
| Limits | no SID history, **no SSH for domain users**, security groups required | — | — | — |

Sources: [DSM 7.4 spec](https://www.synology.com/en-us/dsm/7.4/software_spec/account_identity_privileges), [TrueNAS AD screen](https://www.truenas.com/docs/scale/25.10/scaleuireference/credentials/directoryservices/activedirectory/), [TrueNAS 26 configad](https://www.truenas.com/docs/scale/26/credentials/directoryservices/configad/), [Unraid forum](https://forums.unraid.net/topic/130684-active-directory-setup/).

### Implementation on Debian 13 (Samba 4.22)

- Packages: `samba winbind libnss-winbind libpam-winbind krb5-user chrony`.
- **winbind, not sssd.** Since Samba 4.8 a domain member acting as a file server must use winbindd. Red Hat does not support sssd with Samba because Windows ACLs and the NTLM fallback are missing ([RH bug 1663323](https://bugzilla.redhat.com/show_bug.cgi?id=1663323), [Samba mailing list](https://lists.samba.org/archive/samba/2019-June/223538.html)). sssd + realmd is fine for a workstation, not for a NAS.
- **idmap rid** for a single domain, like the reference case.
  - It is deterministic: same SID → same UID on every rynas NAS, even after a rejoin.
  - It is required by `btrfs send` replication from the primary to the backup NAS, which preserves UIDs.
  - `ad` (RFC2307) requires `uidNumber`/`gidNumber` populated in AD, which is rare.
  - `autorid` is for many trusted domains, but its ranges depend on ordering.

  Ranges suggested by the Samba wiki: `*` 3000–7999, domain 10000–999999 ([Samba wiki](https://wiki.samba.org/index.php/Setting_up_Samba_as_a_Domain_Member)).

**`/etc/samba/smb.conf` [global]** (generated by rynas):

```ini
[global]
   security = ADS
   realm = EXAMPLE.LOCAL
   workgroup = EXAMPLE
   netbios name = RYNAS01            ; ≤15 characters
   kerberos method = secrets and keytab
   dedicated keytab file = /etc/krb5.keytab
   idmap config * : backend = tdb
   idmap config * : range = 3000-7999
   idmap config EXAMPLE : backend = rid
   idmap config EXAMPLE : range = 100000-999999
   winbind use default domain = no   ; DOMAIN\user names: no collisions with local users
   winbind refresh tickets = yes
   winbind offline logon = yes
   winbind enum users = no
   winbind enum groups = no          ; the UI searches via LDAP, it does not enumerate
   template shell = /usr/sbin/nologin
   template homedir = /srv/rynas/homes/%D/%U  ; the stable homes path (D-HOMES)
   vfs objects = acl_xattr btrfs shadow_copy2
   map acl inherit = yes
   store dos attributes = yes
   acl_xattr:ignore system acls = no  ; POSIX kept in sync for NFS/shell
   username map = /etc/samba/user.map ; !root = EXAMPLE\Administrator (optional)
   dfree command = /usr/lib/rynas/smb-dfree
```

**`/etc/krb5.conf`**: the minimum recommended by the Samba wiki, nothing else and **no** `include`:

```ini
[libdefaults]
   default_realm = EXAMPLE.LOCAL
   dns_lookup_realm = false
   dns_lookup_kdc = true
```

**`/etc/nsswitch.conf`**: `passwd: files winbind`, `group: files winbind`, `shadow: files` (never winbind).

**Commands** that rynasd runs and shows in the activity log:

```
chronyc add server <DC>                        # clock synced with the DC
net ads join -U <admin> [createcomputer=OU/…]  # password on stdin
net ads testjoin; wbinfo -t; wbinfo -p; getent passwd 'EXAMPLE\alice'
net rpc rights grant 'EXAMPLE\Domain Admins' SeDiskOperatorPrivilege -U <admin>
net ads leave -U <admin>                       # clean leave
```

### Windows ACLs on shares

- "Windows ACL" mode per share, as on the reference Synology: Samba stores the ACLs in `security.NTACL`.
- With `SeDiskOperatorPrivilege` granted to `Domain Admins`, admins change permissions from Explorer → Security.
- The rynas matrix writes the ACEs at the share root with `smbcacls` or the library. Subfolders are managed from Windows, and rynas shows them read-only.

**Idea for migrating from a Synology.** Synology ACLs are proprietary (synoacl) and do not carry over to Samba. However, the **SIDs are domain SIDs**, so they can be copied as-is from a Windows client:

- `icacls \\synology\Share\* /save acl.txt /t /c`
- `robocopy /MIR /COPY:DATSO` or rsync of the data
- `icacls \\rynas\Share\ /restore acl.txt`

This also preserves the per-user denies (e.g. for bob and carol). rynas can offer a "Migrate from Synology" page that walks through these steps.

### UI login with domain users

- PAM `rynas` → `pam_winbind`, with `require_membership_of` = the mapped groups.
- An `AD group (SID) → rynas role` table, for example `Domain Admins`→admin and `IT`→operator. The **SID** is stored, not the name, so renaming a group does not change its role.
- rynas TOTP/passkey on top of PAM for domain users too.
- Shell and SSH for domain users off by default (DSM does not allow them at all). They can be enabled per group.

### LDAP and a built-in server

- **LDAP client** (OpenLDAP/FreeIPA, RFC2307) with `idmap ldap` / `nss`: phase 3. FreeIPA is effectively AD-like via sssd, so client only.
- **LDAP server / Samba DC** (the equivalent of "Synology Directory Server"): **no**. Anyone who needs a DC runs it in a VM or LXC (Incus, D-LXC) or uses Samba AD in a container. For the UI and apps alone, the phase 3 OIDC is enough.

### Common mistakes

| Mistake | Symptom | Prevention in rynas |
|---|---|---|
| **`.local` domain with Avahi/nss-mdns**: `hosts: files mdns4_minimal [NOTFOUND=return] dns` sends `*.local` queries to mDNS | join fails, DC "not found" | the preflight check detects it and removes `[NOTFOUND=return]` (or mdns4_minimal) when the realm ends in `.local`. Seen in the reference setup |
| DNS pointing at the router instead of the DC | missing `_ldap._tcp` SRV records | `dig SRV _kerberos._tcp.<realm>` test before the join |
| Clock skew over 5 minutes | `KRB_AP_ERR_SKEW` | chrony pointed at the DC, offset shown on the page |
| Hostname over 15 characters, or FQDN resolving to 127.0.1.1 | truncated NetBIOS name, broken Kerberos | validation + generated `/etc/hosts` |
| idmap changed after use (tdb→rid, modified ranges) | different UIDs, POSIX permissions lost | ranges **locked** after the first join; changing them requires a guided "re-ownership" |
| Computer account deleted or reset in AD | `wbinfo -t` fails | health check every 15 minutes → notification; "Repair (rejoin)" with rid, so no damage |
| Microsoft RC4 hardening (CVE-2026-20833, AES-only from April 2026, final from July 2026) | tickets rejected with an RC4-only keytab | after the join check `msDS-SupportedEncryptionTypes`/AES keytab (`klist -ke`) and warn ([Microsoft TechCommunity](https://techcommunity.microsoft.com/t5/core-infrastructure-and-security/what-changed-in-rc4-with-the-january-2026-windows-update-and-why/ba-p/4504732)) |
| `winbind enum` enabled with hundreds to thousands of users | slow UI and `getent` | enum off, paged LDAP search |
| Using sssd with Samba | broken ACLs and NTLM | not offered |

---

## 3b. Existing accounts and UID/GID changes

Two real situations: a Debian install that already has accounts (the installer's first user,
uid 1000, and users made by hand), and data migrated from another NAS with numeric owners
(e.g. 1026, 1027…, the ids DSM gives its first users) that no local account has.

### What the tools do (research)

| Topic | Behaviour | Consequence for rynas |
|---|---|---|
| `usermod -u` | Rewrites passwd and chowns the files *inside the home directory* owned by the old uid (and the mail spool); "the ownership of files outside of the user's home directory must be fixed manually" ([man7 usermod(8)](https://man7.org/linux/man-pages/man8/usermod.8.html), [Debian](https://manpages.debian.org/usermod)). Exit 12 (`E_HOMEDIR`) when the home chown fails, *after* passwd is written | rynas runs `usermod -u` and treats exit 12 as done (its own walk reports what could not change, e.g. a read-only snapshot in the home) |
| `groupmod -g` | Rewrites group and the primary gid of users in passwd; changes no file | the walk is the only file change |
| `find -uid` / `chown --from` | the classic manual fix: `find /data -xdev -uid 1026 -exec chown -h 1000 {} +`, or `chown -R --from=1026 1000` (changes only files whose current owner matches) ([chown(1)](https://manpages.ubuntu.com/manpages/stonking/man1/chown.1.html)) | rynas does the same walk in-process: one file system, `lchown`, no `-exec` per file, progress, cancel |
| POSIX ACLs | entries name numeric ids; `chown` does not touch them; `setfacl`/`getfacl -n` work on numbers. The xattr `system.posix_acl_access`/`_default` is a header (version 2) plus 8-byte entries (tag, perm, id) | the walk rewrites `ACL_USER`/`ACL_GROUP` entries naming the old id, as raw xattrs (no `setfacl` per file) |
| set-id bits | Linux clears `S_ISUID` (and `S_ISGID` with group execute) on `chown` of a non-directory, also for root; directories keep theirs ([chown(2)](https://man7.org/linux/man-pages/man2/chown.2.html)) | an id change puts the bits back on regular files with `fchmod` on the descriptor it checked; directories are never chmod-ed; *Take over files* does not put them back (data from another system must not become set-user-ID to a real account) |
| `chown -R` and races | a path-based walk (`lchown`, then `chmod` by path) follows a directory swapped for a link between two calls: a share member could make root chmod `/etc/shadow` ([CVE-2017-18078 class](https://nvd.nist.gov/vuln/detail/CVE-2017-18078)) | the walk works on descriptors: `openat(O_NOFOLLOW)` from `/` down, `statx`/`fchownat(AT_EMPTY_PATH)`/`fgetxattr` on the opened entry, inode compared between check and open |
| btrfs snapshots | read-only subvolumes answer `EROFS` | skipped and listed: a snapshot keeps the old ids, which is what a snapshot is for (a restore copies files back, then remap again) |
| Samba tdbsam | the passdb entry is found by name; the RID was allocated at `pdbedit -a` and does not follow the uid ([pdbedit(8)](https://www.mankier.com/8/pdbedit), [samba list](https://lists.samba.org/archive/samba/2004-March/082696.html)) | nothing to rewrite; `net cache flush` after an id change so no cached SID↔uid mapping survives |
| Quotas | rynas' quotas are squota/qgroups **per subvolume** (shared folder, home), not per uid (§ 2) | nothing to change; the per-owner usage report counts by the new uid from the next scan |

Other systems: TrueNAS treats UID/GID as fixed after creation; the feature request asks for
"search for files with the old ID and reassign them" as an optional step
([NAS-112318](https://ixsystems.atlassian.net/browse/NAS-112318)); its forums answer with
recreating the account. OMV and Rockstor have no id change in the GUI (shell only,
[Rockstor forum](https://forum.rockstor.com/t/change-uid-of-admin-user/8732)). DSM picks the
uid (1024+) and never lets it change. Migration guides DSM → Linux say: either give the Linux
user uid 1026, or chown everything to the new uid by number
([akitaonrails](https://akitaonrails.com/en/2025/04/17/configuring-my-synology-nas-with-nfs-on-linux/),
[mariushosting](https://mariushosting.com/synology-how-to-find-uid-userid-and-gid-groupid/)).
Best ideas taken: both of those, as guided operations with a dry run first.

### What rynas does

- **Listing.** Every account of the system in 1000–59999 is listed (users and, now, groups);
  those not in the configuration carry "Not managed". System accounts (< 1000, `nobody`
  65534, reserved names) are never listed: they are hidden by the range, not by a list.
- **Adoption.** `POST /api/v1/users/adopt` / `POST /api/v1/groups/adopt` (`{names: []}` = all)
  stage the accounts as they are (`users::import_from` → `users::adopt`): uid, home, shell,
  memberships of local groups (the groups come along), Samba presence. Applying runs no
  `useradd`: the accounts exist. Saving an unmanaged user or group in its editor adopts it the
  same way (`PUT` adopts when the name is on the system). Afterwards the usual tools work:
  password (also creates the Samba account), groups, share permissions, home move, quotas.
  Memberships outside rynas' groups (`sudo`, `docker`) stay untouched as before.
- **Chosen id at creation.** The create forms take an optional UID/GID (1000–59999, empty =
  next free): a user made with uid 1026 owns data copied with owner 1026 at once.
- **Change UID/GID** (`POST /api/v1/ids/plan`, then task `ids.remap` via
  `POST /api/v1/ids/remap`, body `IdRemapRequest {kind, name, new_id, paths, acls,
  expected}`): guards first (`IdBlocker`): local managed account only (adopt first), never
  root or < 1000, the new id in range and free, no running processes and no SMB sessions of
  the user (the same check as the home move; for a group: processes with the gid as real,
  effective, saved, file system or supplementary group, and SMB sessions of its members —
  members logged in elsewhere without a running process are not seen), folders absolute,
  resolved (`canonicalize`, refused when it fails) and inside a data volume, `/home` or
  `/srv` *as resolved*; the walk runs on the resolved paths (`IdRemapPlan.paths`). The plan walks the folders for at most 5 s as a dry run and returns
  `RemapCounts` (entries, owners, ACL entries, read-only snapshots and other file systems
  skipped, ACL conflicts, errors, `partial`). The task checks everything again, then applies the id change as a revision of `users`
  (reconcile runs `usermod -u`/`groupmod -g`, then `net cache flush`), then walks. Only an id
  change task can make the document disagree with passwd on an id (`allow_id_change` guard
  around its apply); a restored old revision with another id is refused by validation.
- **Retired ids.** The change records the old id in `UsersConfig.retired_ids`
  (`RetiredId {kind, id, name, new_id, date}`): it is never picked for a new account (files
  in snapshots, backups and folders left out still carry it). The plan lists them
  (`IdRemapPlan.retired`) and sets `target_retired` when the chosen `new_id` is one of them,
  so the dialog can warn. Choosing it explicitly stays possible.
- **Take over files** (same endpoints with `from_id`): files owned by an id no account has
  become the account's; the account does not change. The DSM case "data copied with owner
  1026, the user is uid 1000" is exactly this: *Take over files… → 1026*.
- **Order and failure.** Account first, then files. A cancelled or failed walk leaves the
  account on its new id and some files on the old one; running *Take over files* from the old
  id finishes it — the walk is idempotent (the change itself cannot run again: the account
  has the new id already, `same_id`). Taking over the account's *own* retired id is marked
  `IdRemapPlan.resumes` and keeps the set-id bits as the change did; the task's error after a
  cancel names that id.
- **The walk** never follows symbolic links: each root is opened component by component
  from `/` with `O_NOFOLLOW` (a link anywhere in it is refused), every entry relative to its
  parent's descriptor, changed through its own descriptor (an entry replaced between the
  check and the open is skipped and counted as an error). It stays on the mount of its root
  (`STATX_MNT_ID`; without it, the mount points of `/proc/self/mountinfo`): btrfs subvolumes
  of that mount are walked, another mount (bind mounts included) is skipped and listed. It
  skips read-only subvolumes (inode 256 + `BTRFS_IOC_SUBVOL_GETFLAGS`) and anything
  answering `EROFS`. ACL entries are rewritten on directories and regular files (not on
  devices, FIFOs or sockets); when the new id has its own entry on a file already, that entry
  stays, the old one goes and the file is listed as an ACL conflict
  (`RemapCounts.acl_conflicts`, `acl_conflict_paths`): no permissions are merged.

### Out of scope (documented)

- Crontabs (`/var/spool/cron/crontabs/<name>` is by name, but its owner is the uid), systemd
  user units and lingering, running containers' bind mounts, files outside the chosen
  folders, mail spools elsewhere, NFS clients' caches: listed in the dialog as "not changed".
- A user-mapping step during import (old id → account for many ids at once): the take-over
  operation run once per id covers it; a batch form can come with the CSV import.
- Group membership by gid in other systems' data (e.g. ACLs written by Windows over SMB with
  SIDs): the SID of a tdbsam user does not change, so they keep working.
- Domain accounts (§ 3): ids come from idmap and are never changed here.

The code: `crates/rynas-services/src/idremap.rs` (walk, `RemapFs`, ACL xattr rewrite),
`users.rs` (`adopt`, `allow_id_change`, reconcile), `crates/rynasd/src/services/ids.rs`
(routes), `crates/rynas-ui/src/pages/user_ids.rs` (dialog).

---

## 4. Proposal for rynas

### Phases

| Phase | Content |
|---|---|
| **1** | Local users and groups (passwd/group + tdbsam kept in sync by rynasd), wizard, share × principal matrix, explained effective permission, home as a subvolume, shell/SFTP per group, UI roles |
| **2** | squota enabled at volume creation, share and home quotas + `dfree`, per-owner usage report, CSV import/export, password policy, mandatory 2FA per group, expiry/suspension, drift check |
| **3 (business)** | AD: winbind + rid, Windows ACLs, AD group → role, trusts (autorid for trusts), offline cache, migration from Synology with icacls; then LDAP client and OIDC (D-PHOTOS) |

### Domain/LDAP page

```
┌ Domain / LDAP ────────────────────────────────────────────────┐
│ ● Joined to EXAMPLE.LOCAL  DC: dc01 (192.0.2.10)  site: Default
│ Computer account RYNAS01 ✓   Clock Δ 0.3 s ✓   DNS SRV ✓   AES keytab ✓
│ Users seen 85 · Groups 22 · last check 2 min ago
│ [Test] [Repair] [Leave domain] [Settings]
│ ── Roles ─────────────────────────────────────────
│ Domain Admins → Administrator    IT → Operator    [+]
│ ── Advanced (locked after join) ──────────────────
│ idmap: rid 100000–999999   Names: DOMAIN\user   Shell: no
└───────────────────────────────────────────────────────────────┘
```

The **Test** button runs in sequence: DNS SRV, clock, `net ads testjoin`, `wbinfo -t/-p`, lookup of a test user, Kerberos `kinit`, encryption types. It shows one result per line with a suggested fix, in the style of the TrueNAS status but more explicit.

### Recommendation

**Phases 1 and 2**

- Real Linux users managed only by rynasd.
- No invented per-UID quotas: share and home quotas with squota, per-user usage as a report.

**Phase 3**

- Samba domain member with winbind and idmap rid.
- Windows ACLs managed from Explorer.
- UI roles mapped to AD group SIDs.
- No built-in DC or LDAP server.

It is the same model as DSM for what the reference AD-joined Synology uses, built on standard Samba/Debian components, with the addition of the explained effective permission, the Test button, and an ACL migration made possible by domain SIDs.

### Sources

- [Samba: Setting up Samba as a Domain Member](https://wiki.samba.org/index.php/Setting_up_Samba_as_a_Domain_Member)
- [vfs_acl_xattr(8)](https://man.archlinux.org/man/extra/samba/vfs_acl_xattr.8.en)
- [RH bug 1663323 (sssd + Samba)](https://bugzilla.redhat.com/show_bug.cgi?id=1663323)
- [btrfs Qgroups / squota](https://btrfs.readthedocs.io/en/latest/Qgroups.html)
- [btrfs-quota](https://btrfs.readthedocs.io/en/latest/btrfs-quota.html)
- [LWN 944371](https://lwn.net/Articles/944371/)
- [wener.me btrfs usrquota Synology](https://wener.me/notes/os/linux/fs/btrfs)
- [nas-forum DSM quotas](https://www.nas-forum.com/forum/topic/60596-tuto-comprendre-et-utiliser-le-quota-dans-dsm)
- [DSM 7.4 Account/Identity spec](https://www.synology.com/en-us/dsm/7.4/software_spec/account_identity_privileges)
- [TrueNAS 25.10 AD screen](https://www.truenas.com/docs/scale/25.10/scaleuireference/credentials/directoryservices/activedirectory/)
- [TrueNAS 26 configad](https://www.truenas.com/docs/scale/26/credentials/directoryservices/configad/)
- [OMV Samba docs](https://openmediavault-docs.readthedocs.io/en/latest/administration/services/samba.html)
- [pdbedit(8)](https://www.mankier.com/8/pdbedit)
- [Unraid AD forum](https://forums.unraid.net/topic/130684-active-directory-setup/)
- [Microsoft RC4 January 2026](https://techcommunity.microsoft.com/t5/core-infrastructure-and-security/what-changed-in-rc4-with-the-january-2026-windows-update-and-why/ba-p/4504732)
- [Nerdio: enforcement 2026-04-01](https://nmmhelp.getnerdio.com/hc/en-us/articles/44796137710605)

### Notes on sources

Still to be verified:

- **Synology's `btrfs usrquota`.** It comes from an unofficial source: it should be confirmed in Synology's GPL source or from a shell on a DSM box.
- **Squota on an already-full volume** (what happens to extents written before enabling it). To be tested in the lab.
- **DSM join options.** The Synology Knowledge Center page is not readable without JavaScript. The options are taken from the DSM spec sheet and from hands-on knowledge of the product.
