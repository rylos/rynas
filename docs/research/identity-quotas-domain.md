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
