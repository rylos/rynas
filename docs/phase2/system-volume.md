# The system volume: the root filesystem as a normal volume (D-ROOTVOL)

Status: implemented 2026-11-08. Decision row: `../phase0/05-decisions.md` (D-ROOTVOL).

Request: "in general, the root volume should also be usable as a normal volume, everywhere."
Before this, every volume choice listed only the btrfs volumes rynas created (`/<label>`), so a
machine whose data disks are not set up yet (or a single-disk machine, a mini PC, a VM) could
not put Docker, an app or a shared folder anywhere from the UI.

## 1. Research

| System | What it does with the system disk | Why |
|---|---|---|
| TrueNAS | The boot pool is never offered for data; the installer dedicates the boot device. | Configuration and the system dataset live there; a community report of a boot pool filled to 100 % by a failed mount (a copy that landed on the boot pool) left the machine unbootable, with no warning, and asked for alerts at 80/90/95 % and reserved space. |
| openmediavault | Shared folders need a mounted data filesystem; the root filesystem is offered only with the `openmediavault-sharerootfs` plugin. | Opt-in: reinstalling the OS wipes that disk, the OS disk is often a USB stick, and a full root breaks the system. |
| Proxmox VE | The installer creates the `local` storage, a directory on the root filesystem (`/var/lib/vz`) with ISO images, templates and backups, usable like any other storage. | Single-disk installs must work out of the box; data goes into one dedicated folder, not anywhere on `/`. |
| Unraid | Docker's image and app data go on a pool (cache); putting them on the boot flash is discouraged (FAT, wear). | The boot device is a USB flash drive. |

Best ideas kept:

- **Offer it, but as one dedicated folder** (Proxmox `local`): every feature that writes data
  writes below that folder, so nothing rynas makes mixes with the OS tree, and "share `/`" or
  "share `/etc`" stays impossible.
- **Name it clearly and put it last**: "System" with its free space, after the real volumes,
  so the default choice stays a data volume when there is one.
- **Warn where it matters** (OMV, TrueNAS report): next to every choice of it, say that it shares
  the OS disk's space and fate; flag low free space on every choice.
- **Keep data out of the OS restore points**: on a btrfs root the folder is a subvolume of its
  own, so the system snapshots (rynas' restore points) neither copy the data nor roll it back,
  and a root rollback carries it into the new root.
- **Backups on the same disk**: allowed with a warning rather than refused. A restic repository
  on the system disk still protects against deleted, overwritten or encrypted files (snapshots,
  retention); it does not protect against a failure of that disk (3-2-1). Refusing would leave a
  single-disk machine without any local versioning; the warning shows in the form and in the log
  of every run.

Sources: TrueNAS forum "TrueNAS fails catastrophically when boot-pool fills to 100 %"
(forums.truenas.com/t/63649) and "Why can't I use the boot pool" (forums.truenas.com/t/27035);
openmediavault shared folders (docs.openmediavault.org, 7.x/8.x "Shared Folders"); Proxmox VE
storage (pve.proxmox.com/pve-docs/chapter-pvesm.html, wiki "Storage: Directory"); Unraid Docker
overview (docs.unraid.net, "Run Docker containers").

## 2. Model

- Label `system` (`rynas_api_types::storage::SYSTEM_VOLUME`), added to `RESERVED_LABELS`: no
  rynas volume can take it. `validate::place_label` accepts it wherever a place is named
  (Docker settings, installs, migrations, stacks, apps SSD volume); `volume_label` (volume
  creation) still refuses it.
- Folder `/srv/rynas/system` (`SYSTEM_VOLUME_DIR`). `storage::volume_dir(label)` gives
  `/<label>` or that folder; `split_volume_path(path)` goes back from a path to its label. Every
  place that used to build `/<label>` goes through them (`Layout::of_label`, app folders,
  share subvolume paths, data-root import).
- Made on first use (`rynasd::places::ensure_system_dir`): `/srv/rynas` (0755), then the folder,
  `btrfs subvolume create` on a btrfs root, `mkdir` otherwise; dry runs and tests make plain
  folders below their root. Listing never creates it.
- `GET /api/v1/storage/places` → `Vec<VolumePlace>` (`label`, `path`, `avail_bytes`,
  `size_bytes`, `fs_type`, `system`, `low_space`): the mounted rynas volumes, then the system
  volume when the root filesystem is a disk (not overlay, tmpfs, squashfs, NFS, 9p, FUSE...).
  `DockerSettingsState::volumes` is the same list (`VolumeChoice` is an alias of `VolumePlace`).
- Low space: `< 10 GiB` or `< 10 %` free (`storage::low_space`), shown next to the choice.

## 3. Where it is offered

| Feature | Behaviour on the system volume |
|---|---|
| Docker engine (settings, install, move) | data-root, containerd and stacks in `/srv/rynas/system/docker/...` (subvolumes on btrfs); the "data outside the volumes" warning now names only Docker's default folder |
| Stacks, adoption, apps | stack folders and app data folders below the folder; uninstall may delete what it made there (the folder is under `/srv` but counts as a volume) |
| Shared folders (new subvolume) | `/srv/rynas/system/<share>`; a subvolume on btrfs, a plain folder otherwise (NOCOW and compression hidden then, with the note) |
| Unshared folders, folder delete | as on any volume |
| restic backup, local destination | `<folder>/backup` shortcut; allowed with the same-disk warning (form and every run); restores may write below the folder |
| Replication, local target | btrfs only; a path in the folder no longer counts as "not mounted"; the folder is made as a subvolume |
| PC backups (btrbk receive) | the folder is listed after the btrfs volumes on a btrfs root; made as a subvolume before the first target folder |
| File manager | shares in the folder show on the "system" volume, as before for shares elsewhere on `/` |
| Root rollback | the folder, when it is a subvolume nested in `@rootfs`, moves into the new root (undone if the swap fails) |

## 4. Exceptions (what stays impossible, and why)

- **Format, delete, RAID, grow, replace, lock, unmount**: the root filesystem is not a volume
  rynas created; the Storage page keeps it out of the volume actions.
- **User homes**: homes keep their own "System disk (/home)" choice, the standard place on the
  root filesystem; the system volume is not offered a second time there.
- **Anything outside the folder**: shares of `/`, `/etc`, `/usr`... stay refused
  (`validate::share_path`), and a local backup destination elsewhere on the root disk stays
  refused (review M6: an unplugged USB disk leaves an empty folder there).
- **btrfs-only features on a non-btrfs root**: no subvolumes, snapshots, replication, PC
  backups or NOCOW; the choice says so instead of failing later.
- **A root that is no disk** (container overlay, live tmpfs, network root): the system volume is
  not offered.

## 5. Left for later

- A notification when the root filesystem crosses the low-space threshold (today: the Docker
  data-root alert and the flag on every choice).
- A Storage page card for the system volume (device, free space, the exceptions above).
