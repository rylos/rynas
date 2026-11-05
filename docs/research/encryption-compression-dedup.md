# Storage efficiency and data security in rynas: encryption, compression, deduplication

## Summary

| Topic | Choice | Phase |
|---|---|---|
| Encryption | **LUKS2 per volume, between mdadm and btrfs** (mdadm → dm-crypt → btrfs). Unlock from the UI, mandatory recovery key, key on USB, then TPM2 and Tang | 1 (base), 2 (automatic keys) |
| Per-folder encryption | There is no good tool on btrfs yet: fscrypt for btrfs **is not in the kernel** (v7 series under review). No eCryptfs. gocryptfs only as a limited option | 3 |
| Compression | **`compress=zstd:1` on the whole volume** (not `compress-force`), `none` property per folder, reporting with `compsize` | 1 |
| Deduplication | **"Storage efficiency"** module with the **oans** engine, run as a rynas task inside a time window, folder by folder, snapshots excluded, never concurrently with replication | 2 (dry-run estimate already in 1) |

---

## 1. Encryption

### How others do it

| Product | Type | Granularity | Keys | Known issues |
|---|---|---|---|---|
| **DSM: shared folder** | eCryptfs (stacked on top of the filesystem) | folder | password, key file, Key Manager with key on USB, auto-mount | file names limited to **143 bytes**, slow writes, no reflink and therefore no dedup (encrypted shares on an older DSM file server had to be left out of oans) |
| **DSM 7.2+: volume** | LUKS aes-xts-plain64 | volume | local Key Vault (auto-mount mandatory) or KMIP only against another Synology | without KMIP the key lives on the same disks; auto-mount uses a key stored in the NAS flash, **not a TPM**. It protects disks pulled out of the NAS, not a stolen NAS |
| **TrueNAS** | native ZFS encryption | dataset | generated key or passphrase; unlock from UI or API (`pool.dataset.unlock`), key export | recommends an unencrypted pool with individually encrypted datasets. With `zfs send -w` the replica stays encrypted and the target does not need the key |
| **OMV** | `luksencryption` plugin (LUKS) | disk or device | passphrase, key file, `noauto,nofail` | the plugin edits fstab and the OMV database, and headless unlock is left to community scripts |
| **Unraid** | LUKS on each array disk | disk | passphrase or key file at array start; auto-unlock plugin | the key is not stored and the array stays stopped until it is entered: a good model for "data locked until unlocked" |

**Ideas worth taking:**
- from Unraid and TrueNAS: the volume stays locked after reboot and is unlocked from the UI;
- from TrueNAS: key export and the reminder to save the keys;
- from DSM: the key on USB and the recovery file downloaded during the wizard.

**Mistakes to avoid:**
- eCryptfs: it has been "orphan" in the kernel since 2023 and the maintainer wants to remove it;
- keeping the key on the same disks as the data, as the DSM local Key Vault does;
- KMIP locked to a single vendor.

### Options on Linux

- **LUKS2 on top of md** (md → dm-crypt → btrfs). This is the proposed choice: each write is encrypted once, whereas with RAID1 or RAID5 below the encryption every copy would be encrypted separately. `mdadm check` and resync also run while the volume is locked. The volume opens in a single operation and there is a single set of keys to manage.
- **LUKS below md** (one encryption layer per disk): right for Unraid, which has one filesystem per disk. For rynas it multiplies the CPU work and the number of keys.
- **fscrypt on btrfs**: the "btrfs: add fscrypt support" series (43 patches, first by Josef Bacik, Omar Sandoval and Sweet Tea Dorminy, now by Daniel Vacek) is at **v7**. In June 2026 Sterba judged it too late for 7.2, and Eric Biggers is asking for more work. The ioctls sit behind `BTRFS_EXPERIMENTAL`. Only an fscrypt cleanup that paves the way landed in 7.3. Debian 13 ships kernel 6.12, so **nothing before Debian 14** (2027) or a backports kernel with the support marked stable.
- **gocryptfs** (Debian 13: 2.5.1, backports 2.6.1): FUSE, per-file encryption, handles long names, so no 143-byte limit. The ciphertext is unique per file, so **dedup and compression become useless**. Samba goes through FUSE, restoring from snapshots requires decrypting names, and ACLs are limited. The real advantage: an encrypted folder can be replicated to an untrusted target, as with `zfs send -w`.
- **CryFS**: also hides file sizes, but is slow. Not suitable for a NAS.

### Performance (R1600 and J4125, both with AES-NI)

No published `cryptsetup benchmark` results exist for the R1600 or J4125. As a reference: techtest.org, with DSM 7.2 on a DS1821+ (V1500B, Zen like the R1600), SSDs and 10 Gb networking, measured volume encryption at zero cost for reads and **676 MB/s for writes**. With two or four HDDs and 1 or 2.5 Gb networking the disks remain the bottleneck. CPU only matters with SSDs and 10 GbE on the R1600 (2 cores).

For rynas:
- `cryptsetup benchmark` inside the wizard, with the result shown on screen;
- LUKS2 with `--sector-size 4096`;
- on SSDs, `no-read-workqueue,no-write-workqueue` and `discard`, with a warning that `discard` reveals which blocks are free;
- **Argon2id with bounded memory** (256–512 MiB): the DS920+ has 4 GB of RAM, and the default of up to 1 GiB risks running out of memory during unlock.

### Key management

| Mode | Protects against | How | Phase |
|---|---|---|---|
| Passphrase, manual unlock from the UI | disk removed, NAS stolen | the volume is `noauto` in crypttab; rynasd runs `cryptsetup open` with the passphrase on stdin, mounts, then starts the dependent services | 1 |
| **Recovery key** (always) | lost passphrase | an extra slot via `systemd-cryptenroll --recovery-key`; the wizard does not proceed until the key has been downloaded or printed | 1 |
| LUKS header backup | damaged header | `cryptsetup luksHeaderBackup`, downloaded and kept off the NAS | 1 |
| Key file on USB | disk removed (not if the stick is left plugged in) | crypttab entry with `keyfile-timeout`; if the stick is missing, fall back to manual unlock | 2 |
| TPM2 | only a removed disk, like DSM auto-mount | `systemd-cryptenroll --tpm2-device=auto` (systemd 257 in Debian 13), offered only if `/dev/tpmrm0` exists. Synology units do not appear to use a TPM: to be checked per model | 2 |
| **Tang/Clevis** (network-bound) | whole NAS stolen: it will not unlock outside the LAN | `clevis luks bind -d … tang`, `clevis-systemd`; the Tang server can run on another host or in a container | 2 |
| KMIP / Vault | enterprises | — | 3 (business edition) |

**Details to sort out:**
- services such as Samba, Docker (data-root on the volume), Immich and the btrbk timers get `RequiresMountsFor=/vol…`;
- while the volume is locked the UI shows a red "Volume locked · Unlock" banner, and the task panel marks jobs that depend on the volume as "waiting".

**Changing the key:** `luksAddKey`/`luksChangeKey` only change key slots, without rewriting data. Changing the master key requires `cryptsetup reencrypt`, which works with the volume mounted but takes hours: it is a maintenance task.

**Snapshots and replication:** LUKS is transparent, so snapshots, compression and dedup work as without encryption. `btrfs send`, however, produces **a plaintext stream**:
- SSH encrypts the transport;
- the target must itself be encrypted, and rynas flags this with a warning such as "replicating to an unencrypted volume";
- for untrusted targets: btrbk `raw` target with gpg or openssl, or restic, which encrypts natively.

---

## 2. btrfs compression

**How it works:**
- with **`compress`**, if the first chunk of a file does not shrink, btrfs sets the NOCOMPRESS flag and stops compressing that file. Before that, a heuristic (sampling and entropy) runs at virtually no cost;
- with **`compress-force`** it tries to compress every 128 KiB block; the official documentation **advises against it**;
- data is split into 128 KiB chunks, which means more metadata and more fragmentation;
- zstd levels: 1 to 15, default 3, with "1..3 near real-time". Negative levels exist only since 6.15, so not on the Debian 13 kernel;
- **`btrfs property set <dir> compression zstd|none`** applies per folder and new files inherit it. **It does not accept a level**, which is set only in the mount option and applies to the whole filesystem;
- nodatacow or nodatasum files are not compressed: that is why VMs and databases are not compressed.

**CPU:** zstd:1 compresses at a few hundred MB/s per core and decompresses at over 1 GB/s. btrfs spreads the work across threads. On the R1600 and J4125 with HDDs it is not a bottleneck. With SSDs and 10 GbE on the R1600, zstd:1 is preferable to zstd:3.

**Proposal:**
- `compress=zstd:1` when the volume is created (the same value Fedora uses by default);
- in the folder wizard: "Compress: Automatic (yes) / No";
- "No" suggested for the Video, Photos, Backup and VM profiles, and mandatory for the Frigate recordings volume;
- no per-folder level choice, because btrfs does not allow it.

**Recompressing existing data** (`btrfs fi defrag -r -czstd -L N`, level since progs 6.14):
- **it rewrites extents and breaks reflinks**: files shared with snapshots or deduplicated get split, and space usage can multiply by the number of snapshots;
- rynas should offer it only on folders without snapshots. Otherwise it needs a strong confirmation, with an estimate of the extra space used until the snapshots expire;
- the correct order is **compress first, then deduplicate**, as field experience shows ("dedup AFTER defrag").

**Measuring space** (`compsize`, Debian package `btrfs-compsize`, on the live subvolume, without snapshots):
- `Referenced` = logical data;
- `Uncompressed` = unique extents before compression;
- `Disk Usage` = physical space.
- **Compression savings = Uncompressed − Disk Usage**; **dedup and reflink savings = Referenced − Uncompressed**.

This is the correct breakdown that DSM lacks: the DSM "savings" figure also includes blocks shared with snapshots, as verified on a DSM NAS on 2026-10-05. `compsize` is slow on large trees, so it should run as a scheduled task after dedup, with results stored in SQLite. qgroups stay off, consistent with D-SNAP; simple quotas are enabled only on request.

---

## 3. Deduplication

| Engine | Model | Pros | Cons |
|---|---|---|---|
| **oans** (martinus) | duperemove fork, batch, incremental (SQLite hashfile) | GPL-2.0; v1.14.1 of 2026-10-19. `.gitignore`-style excludes, stops on SIGINT and resumes, skips already-shared files. Metrics via `--json`, `--progress=json`, `--stats`, `--history` (read-only, even during a run), `oans@.timer` unit, also supports XFS. Field numbers: **250 GiB reclaimed** on a 2.7 TiB design share in 8 h 26 min on a J3455; on desktop PCs, scanning at about 2.9 GiB/s | project started 2026-07-14, single author. 1.12–1.14 fix wrong digests and a WAL that grew without bound. Bugs cause missed duplicates but **do not corrupt data**, because the kernel compares byte by byte (`FIDEDUPERANGE`). No .deb |
| **bees** | continuous daemon, fixed-size hash table | block-level, covers the whole filesystem | always running, so no time windows and no disk standby. With many snapshots it grows metadata. A balance invalidates the hash table. Conflicts with `send` (needs `--workaround-btrfs-send`). No formal releases. Abandoned after field use |
| **duperemove** | batch, hashfile | long-standing and reliable | Debian 13 ships **0.11.2**, from 2022 (0.15.2 is only in forky). On the design share the snapshot exclusion did not work: 29 TB analyzed instead of 2.9. Slower |
| **DSM Storage Efficiency** | post-process, Synology kernel module | 168-cell weekly schedule grid, dry-run estimate, dedup+compression mode | proprietary. A manual run **ignores the time window**. The reported "savings" include snapshots |
| **ZFS fast dedup** (OpenZFS 2.3, TrueNAS) | inline | DDT log, prune, quota (above the quota writes revert to normal) | ZFS only. The advice remains: only where the ratio exceeds about 3:1, per dataset, after a `zdb -S` simulation |

**Recommendation: oans.** Three reasons:
- it is the only one combining incremental operation, clean interruption (essential for time windows) and machine-readable metrics;
- it has already been validated on four hosts with multi-TB volumes;
- data risk is nil, and project risk is covered by: a **pinned** version in the rynas apt repository, oans tests in CI before every upgrade, and a `DedupEngine` interface in `rynas-btrfs` with duperemove as a fallback.

**Packaging:**
- a `.deb` built by rynas CI from the official tag and published with aptly, together with the source package to comply with the GPL;
- oans runs as a separate process, so it has no effect on the rynas BSL license;
- depends on SQLite ≥ 3.25 (Debian 13 has it). Other libraries to be checked against the README;
- no `-march=native`.

### Module technical details

1. **rynas task** (D-TASKS), unit `rynas-task-dedup-<vol>.service`:
   - `Nice=19`, `CPUWeight=20`, `IOSchedulingClass=idle`, `MemoryHigh=`;
   - **`RuntimeMaxSec=`** set to the end of the window, `KillSignal=SIGINT`, `TimeoutStopSec=15min`: oans saves and resumes on the next run, like the `timeout -s INT` approach of the existing dedup script.
   - Caveat: `IOWeight` only works with BFQ, and stock Debian uses mq-deadline. So either BFQ on HDDs via udev, or `IOReadBandwidthMax` (to be verified on md devices).
2. **Time window** with the DSM 7×24 grid, but **"Run now" asks when to stop** (proposal: at the end of the window). This is the DSM mistake not to repeat.
3. **Replication and mutual exclusion (hard constraint):**
   - "*send* cannot start while a dedup is in progress on a file of the filesystem", per the official documentation. Since 2019 the kernel returns EAGAIN;
   - so the scheduler enforces **one lock per filesystem**: btrbk send, scrub, balance and dedup never run concurrently. If a replication starts, dedup receives SIGINT and resumes afterwards.
4. **Snapshots excluded** (btrbk snapshot directory, `.snapshots`):
   - the kernel can dedup inside a read-only subvolume too, but it is pointless, pollutes the hashfile and grows metadata;
   - the UI explains that **space is freed when the snapshots expire** and shows the estimated date based on retention, as on the design share (about 9–10 days).
5. **After a large dedup:**
   - the first incremental send must rebuild who shares each extent. On an older DSM file server (kernel 4.4) it never finished;
   - above a certain number of references send gives up on cloning and sends the data (`SEND_MAX_EXTENT_REFS`: 64 in the original commit, 1024 in later sources), and the target copy no longer stays deduplicated;
   - so on the first dedup of a replicated folder rynas shows a warning, monitors the duration of the next replication, and offers a separate dedup on the target.
   - send is much improved on 6.12, but it must be tested before enabling this by default. Folders with hundreds of copies of the same file, such as a technical drawings share with 536 copies of the same SolidWorks spare-part file, get a "replication risk" warning.
6. **Hashfile:**
   - in `/<vol>/@rynas-cache/dedup/<folder>.hash`, a subvolume excluded from snapshots and replication, with `chattr +C` because SQLite on CoW fragments;
   - it is a cache: after a power loss it is deleted;
   - options are **always passed explicitly** (`-d`), because the hashfile remembers those of the first run.
7. **Default options:** whole extents and files, `partial` off (too heavy on Celerons). Files are briefly locked during comparison, so work happens off-hours. Default excludes: `@eaDir`, recycle bin, already-deduplicated backup repositories, VM disks.
8. **Dry-run estimate** (`oans -r` then `--stats`): the wizard shows "whole-file duplicates: X GiB (Y%)" before enabling dedup, like the DSM dry run and `zdb -S`.
9. **Reporting:**
   - logical figure: `reclaimed_total_bytes` from `oans --json`, run by run (history);
   - real figure: the `compsize` breakdown described above;
   - space held by snapshots: `btrfs fi du -s` on the snapshots;
   - end-of-run notification with the outcome for each folder, like the Telegram message of the existing dedup script.
10. **Disk standby:** no daemon (bees excluded). If the disks are in standby when the window starts, a single weekly run wakes them; the "skip if disks are asleep" option uses `smartctl -n standby`.

---

## 4. UI mockup

```
Volume 1  ·  RAID5 3×8 TB  ·  🔒 LUKS2 unlocked  [Lock] [Keys…]
──────────────────────────────────────────────────────────────────
Space       ███████████░░░░  10.7 / 14.0 TiB   (3.3 free)
Efficiency  Logical data 13.1 TiB → on disk 10.7 TiB  (−18%)
            ├ Compression zstd:1         −1.1 TiB
            ├ Dedup / reflink            −1.3 TiB
            └ Held by snapshots           0.9 TiB  (freed within 10 days)
Dedup       oans 1.14.1 · Fri 19:30 → Mon 06:30 · last: OK, +250 GiB
            [Run now…] [Estimate] [History] [Settings]
──────────────────────────────────────────────────────────────────
Folder         Encr.  Compr.   Ratio     Dedup      Saved
design         vol    auto     1.04×     ✔ weekly   262 GiB
engineering    vol    auto     1.31×     ✔ weekly    98 GiB
Video          vol    no       —         ✗          —
PC-backup      vol    no       —         ✗ excluded —
```

```
New shared folder ─ step 2/4: Data
  Name          [ Projects        ]
  Profile       (•) Documents  ( ) Photos/Video  ( ) Backup  ( ) VM/DB
  Compression   [✔] zstd (recommended for Documents)
  Dedup         [✔] include in the volume's weekly dedup
                    ⚠ replicated folder: the first replication after dedup will be slower
  Encryption    Encrypted volume (LUKS2) ✔
                [ ] Separate encrypted folder (gocryptfs, experimental:
                    no dedup/compression, names readable only when unlocked)
  Snapshots     [✔] daily, keep 7/4/6 (GFS)                 [Back] [Next]
```

---

## Phases

| Phase | Encryption | Compression | Deduplication |
|---|---|---|---|
| **1** | LUKS2 below the volume in the wizard: passphrase, mandatory recovery key, header backup, lock and unlock from the UI, `RequiresMountsFor`, benchmark | `zstd:1` by default, per-folder property, scheduled `compsize` report | oans package in CI, **dry-run estimate** only |
| **2** | key file on USB, TPM2 if present, Tang/Clevis, passphrase change, `reencrypt` as a task, warning for replication to unencrypted targets | guided recompression only without snapshots, with estimate | full module: 7×24 window, lock with send/scrub/balance, history, notifications, replication warnings, dedup on the target |
| **3** | encrypted folder: **fscrypt on btrfs** once stable and in the Debian kernel, otherwise gocryptfs as an option labeled experimental; KMIP in the business edition | negative levels with kernel ≥ 6.15 | XFS (oans), ZFS plugin with native dedup and encryption |

## Sources

- https://github.com/martinus/oans · https://github.com/martinus/oans/releases
- https://btrfs.readthedocs.io/en/latest/Compression.html · https://btrfs.readthedocs.io/en/latest/Deduplication.html · https://btrfs.readthedocs.io/en/latest/btrfs-property.html
- https://phoronix.com/news/Linux-6.15-Btrfs (negative zstd levels)
- fscrypt on btrfs: https://lkml.iu.edu/2606.0/01908.html · https://lkml.iu.edu/hypermail/linux/kernel/2605.1/09933.html · https://phoronix.com/news/Linux-7.3-FSCRYPT
- send vs dedup conflict: https://lore.kernel.org/all/20190422154342.11873-1-fdmanana@kernel.org/T/ · send reference limit: https://android-kvm.googlesource.com/linux/+/fd0ddbe2509568b00df364156f47561e9f469f15%5E%21/fs/btrfs/send.c
- bees: https://github.com/Zygo/bees/blob/master/docs/gotchas.md
- Debian packages: https://packages.debian.org/search?keywords=duperemove · https://packages.debian.org/search?keywords=gocryptfs
- eCryptfs orphan: https://lkml.iu.edu/hypermail/linux/kernel/2303.3/01229.html
- DSM: https://blog.elcomsoft.com/2023/06/volume-encryption-in-synology-dsm-7-2-luks-with-questionable-key-management/ · https://techtest.org/synology-volumen-verschluesselung-performance-und-sicherheit/ · https://rossmanngroup.com/services/nas-data-recovery/synology/encrypted-volume · https://github.com/Difegue/LANraragi/issues/644 · https://kb.synology.com/en-us/DSM/help/DSM/StorageManager/volume_btrfs_dedup?version=7
- TrueNAS: https://www.truenas.com/docs/scale/25.10/scaletutorials/datasets/encryptionscale/ · https://truenas.com/docs/references/zfsdeduplication · https://forum.proxmox.com/posts/863350/
- OMV: https://forum.openmediavault.org/index.php/Thread/18158-LUKS-KeyFile-AutoMount-SOLVED/ · Unraid: https://forums.unraid.net/topic/195584-plugin-automatic-disk-unlock/ · https://forums.unraid.net/topic/193638-leave-encrypted-disks-unmounted-and-manually-mount-with-passphrase-from-windows-client/
- Field notes from running oans, bees and DSM Storage Efficiency on several btrfs hosts

**To verify on real hardware:**
- `cryptsetup benchmark` on the R1600 and J4125;
- presence of a TPM (`/dev/tpmrm0`) on the DS920+ and DS923+;
- duration of the first incremental send after a large dedup on kernel 6.12;
- effectiveness of `IOWeight`/`io.max` on md devices.
