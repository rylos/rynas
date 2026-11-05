# Functional analyst report: NAS managers (state as of November 2026) and ideas for rynas

**Reference versions.** Synology DSM 7.3 was released on 8 October 2025, 7.4 on 16 June 2026. TrueNAS 25.10 "Goldeye" came out in October 2025, with 25.10.1 on 18 December 2025. For TrueNAS 26 I only found betas (BETA.2 of 17 June 2026). The official docs already have a `/scale/27/` section, so a stable 26 probably exists, but I did not verify it. OpenMediaVault 8 "Synchrony" (Debian 13) was released in December 2025. Unraid 7.2 is from October 2025, 7.3.0 is stable and the latest I found is 7.3.2 (8 July 2026). ZimaOS is at 1.7.1.

---

## 1. Feature matrix by area

Legend: ★ = who does it best.

| Area | DSM | TrueNAS | OMV | Unraid | Notes / winner |
|---|---|---|---|---|---|
| **Disks / SMART** | Health Info, scheduled SMART tests, IronWolf scans; since 7.2.1 shows only "Healthy" | ★ scheduled SMART tests, threshold alerts | full SMART (smartd) and scheduling | per-disk SMART attributes, highlighted counters | **TrueNAS** for data and alerts. **DSM** for simple presentation, but hiding the attributes was a heavily criticized mistake. |
| **RAID / pools** | ★ SHR (mdadm+LVM), guided expansion and level migration | ★ ZFS (RAIDZ expansion, hybrid pools with OpenZFS 2.4) | mdadm via UI, ZFS and btrfs via plugins | parity array + ZFS/btrfs pools, RAIDZ expansion since 7.2 | **DSM** for UX on mdadm, **TrueNAS** for ZFS. |
| **LVM** | internal, hidden under SHR | – | LVM plugin | – | OMV exposes it, DSM hides it: for rynas hiding it like DSM is better. |
| **Snapshots** | ★ Snapshot Replication: scheduling, retention, visible from Windows as "previous versions" | ★ periodic ZFS snapshots, retention, replication | btrfs/ZFS plugin | plugin | Tie between **DSM** and **TrueNAS**. |
| **Scrub** | scheduled, one per pool, speed limit, resumes after a crash | ★ scheduled ZFS scrubs with threshold | cron | scheduled parity check | **DSM** for limit management. |
| **Quotas** | per user/share | per dataset/user/group | ext4/xfs quotas | – | **TrueNAS**. |
| **SMB / NFS** | ★ complete, Windows ACLs | ★ complete, Spotlight in 26 | good, "extra options" field | good | DSM and TrueNAS. **OMV** has the field for extra configuration lines, much appreciated. |
| **WebDAV / FTP / S3 / iSCSI** | all (S3 only via package) | iSCSI, NVMe-oF ★ since 25.10, WebShare | FTP, WebDAV and iSCSI via plugins | via plugins/Docker | **TrueNAS** for block protocols. Everyone dropped AFP. |
| **Users / groups / ACLs** | ★ wizard with per-share permissions, quotas and applications on a single screen | NFSv4/POSIX ACL editor | basic | basic | **DSM**. |
| **LDAP / AD join** | ★ AD join, LDAP server as a package | ★ AD join, LDAP, IPA | plugin | AD only in a limited way | Tie. |
| **Networking** | bond, VLAN, OVS bridge | ★ bond/VLAN/bridge with test and automatic rollback | interfaces, bond, VLAN, WPA3 Wi-Fi | bond, bridge, VLAN | **TrueNAS** for "test changes" with rollback. |
| **Firewall** | ★ per-service rules, geo-IP, auto-block | almost absent | no native one | absent | **DSM**. |
| **Containers / apps** | Container Manager (compose) | ★ app catalog on Docker Compose, YAML editor with validation, moving apps between pools | ★ compose plugin (omv-extras) with scheduled backups | ★ Community Apps (XML templates) | **Unraid** for catalog and simplicity, **TrueNAS** for the curated catalog. |
| **VMs / LXC** | VMM | VMs, LXC fully supported since 26 | KVM plugin | ★ VMs with very polished GPU passthrough | **Unraid** for passthrough. |
| **Backup / replication** | ★ Hyper Backup, ABB, USB Copy, Snapshot Replication | ★ Cloud Sync (rclone), ZFS replication, rsync | rsync, rsnapshot, borg/restic plugins | plugins | **DSM** for UX, **TrueNAS** for replication and cloud. |
| **Notifications** | ★ email, SMS, push, webhook, per event | ★ alert services (Slack, Telegram, Pushover…) | email | ★ agents (Discord, ntfy, Gotify…) | **Unraid** and **TrueNAS**. |
| **Updates** | ★ one click, signed versions | trains, ZFS boot environments with rollback ★ | apt and omv-upgrade, shows previous versions | OS image with rollback | **TrueNAS** for rollback via boot environments. |
| **Logs / audit** | ★ Log Center: syslog server, file access logs | ★ SMB and middleware audit | journal | syslog | Tie between **DSM** and **TrueNAS**. |
| **Monitoring** | ★ Resource Monitor with history | netdata charts, widget dashboard | widget dashboard, collectd charts | dashboard | **DSM**. |
| **UPS** | ★ NUT, also UPS server for other Synologys | NUT | NUT plugin | apcupsd and NUT | **DSM**, because it acts as a network server in a simple way. |
| **Power scheduling** | ★ scheduled power on/off, Wake-on-LAN | – | autoshutdown plugin | plugin | **DSM**, which however relies on Synology hardware. On Debian it is done with rtcwake and the suspend state. |
| **HW info** | ★ fans, temperatures, LEDs, beep | basic sensors | basic | basic | **DSM**. For synodeb this is gold: the DS923+ LEDs, fans and buzzer. |
| **Remote access** | ★ QuickConnect, DDNS, Let's Encrypt certificates | TrueNAS Connect, still in development | – | ★ Unraid Connect, Tailscale plugin | **DSM**. For an open source project it makes sense to integrate Tailscale/WireGuard. |
| **2FA / SSO** | ★ TOTP, passkeys, Secure SignIn | TOTP 2FA, API keys, passkeys on WebShare | – | ★ OIDC since 7.2 | **Unraid** for OIDC, **DSM** overall. |
| **API / CLI** | web APIs not officially documented (synowebapi) | ★ versioned JSON-RPC 2.0 over WebSocket, REST removed in 26, `midclt` | JSON RPC and `omv-rpc` | built-in GraphQL API since 7.2 | **TrueNAS**. |

---

## 2. The 20 best ideas to copy

1. **DSM Storage Manager with "hidden" mdadm+LVM (SHR).** The user sees pool → volume → disk and guided actions (repair, expand, add disk), never `mdadm` or `lvcreate`. On Debian SHR can be replicated by combining partitions, mdadm and LVM. https://www.synology.com/en-global/dsm/7.4/software_spec/storage_management
2. **Scheduled scrub with speed limit and resume after a crash (DSM).** One per pool, not concurrent. https://www.synology.com/en-id/knowledgebase/DSM/help/DSM/StorageManager/storage_pool_data_scrubbing
3. **Network "test changes" (TrueNAS).** Applies the change and, if no confirmation arrives from the UI within 60 seconds, rolls back automatically. Essential on a headless NAS.
4. **Boot environments and update rollback (TrueNAS).** With btrfs as root on Debian you get it with a snapshot before every apt run (snapper/grub-btrfs).
5. **A single, versioned API, JSON-RPC over WebSocket (TrueNAS 25.10).** The UI is just a client of the API, and so is the CLI. With push events for job status. https://www.truenas.com/blog/truenas-25-10-1
6. **Asynchronous jobs with progress (TrueNAS middleware).** Scrub, resync, replication and image pulls are "jobs" with percentage, log and the option to cancel them.
7. **"Dirty modules" and Apply (OMV).** Changes accumulate and are applied in bulk, and the UI shows which modules will change (new in OMV 8). https://docs.openmediavault.org/en/7.x/development/howitworks.html
8. **Generating files from a single, declarative database (OMV with Salt, TrueNAS with `etc` plugins).** One source of truth.
9. **"Extra options" field per share and service (OMV, TrueNAS `auxsmbconf`).** Keeps users from hand-editing a file that later gets overwritten.
10. **Declarative YAML plugins for the UI (OMV workbench).** `navigation.d`, `component.d` and `dashboard.d`: a simple plugin needs no JavaScript. https://docs.openmediavault.org/en/7.x/development/plugins.html
11. **App catalog on Docker Compose with a schema-generated form (TrueNAS, Unraid Community Apps).** Plus "install via YAML" with a validating editor, and moving apps between pools. https://www.truenas.com/docs/truenasapps/
12. **Scheduled backup of compose stacks (openmediavault-compose plugin).** Stops, saves the volumes, restarts. https://wiki.omv-extras.org/
13. **Snapshots visible as "previous versions" over SMB (DSM, TrueNAS).** With Samba's `vfs_btrfs`/`shadow_copy2`.
14. **Single-screen user wizard (DSM).** Groups, per-share permissions, quotas and application access together.
15. **Multi-channel per-event notifications (Unraid, TrueNAS).** Email, ntfy, Gotify, Telegram, webhook, with severity level and muting.
16. **Configurable widget dashboards (OMV, TrueNAS) with history (DSM Resource Monitor).**
17. **Network UPS server with NUT (DSM).** The NAS acts as server, the other clients shut down in cascade.
18. **Optional OIDC SSO for the UI (Unraid 7.2), passkeys (TrueNAS WebShare, DSM).** https://docs.unraid.net/unraid-os/release-notes/7.2.0/
19. **First-boot wizard (Unraid 7.3, HexOS).** Detects the hardware and proposes pools, network and admin user. https://unraid.net/blog/unraid-7-3-0
20. **Platform-specific hardware page (DSM).** LEDs, fans, buzzer and ports. For synodeb this is the "Synology platform plugin".

---

## 3. Weak points users complain about (to avoid)

- **Manual edits get overwritten without warning (OMV).** The most frequent complaint on r/OpenMediaVault. Countermeasure: a "generated by rynas" header in the files, drift detection (hash of the generated file) with a warning in the UI, documented extension points. https://docs.openmediavault.org/en/8.x/various/advset.html
- **Architecture changes that break apps.** TrueNAS moved from k3s to Docker in 2024. TrueCharts was abandoned without a migration path and many moved their apps to Proxmox or Unraid. Countermeasure: use standard formats (plain compose) and keep app data in readable paths, not inside opaque volumes.
- **Lock-in and paternalism (DSM).** The block on non-certified drives in 2025, later withdrawn with 7.3. SMART attributes hidden since 7.2.1. The old kernel. Features tied to branded hardware (7.4 deduplication requires Synology drives). Countermeasure: always show raw data in an "advanced" view.
- **Fragile configuration database.** In TrueNAS the sqlite lock requires restarting middlewared. Configuration backups also need the `pwenc_secret` key. Countermeasure: exportable and restorable TOML/JSON configuration files.
- **Plugins that break at every major release.** In Unraid 7.2 several plugins were removed automatically. In OMV plugins must be ported at every Debian release. Countermeasure: a versioned plugin API and a compatibility check before upgrading.
- **Unraid array performance.** Writes are limited by the slowest disk and penalized by parity, and ZFS in the array has had bugs. Rynas must not invent its own array.
- **Surprise monetization.** ZimaOS 1.5 introduced limits of 10 apps, 4 disks and 3 users, causing protests. HexOS depends on the cloud. It must be stated clearly that rynas is completely free and works without the cloud.
- **Unstable 45Drives Cockpit modules.** A created ZFS pool sometimes does not show up in the UI. Countermeasure: the UI must always re-read the real system state (`lsblk`, `zpool`, `mdadm --detail`), not just its own database.
- **"Experimental" features left half-done for years.** TrueNAS 25.x LXC, Rockstor btrfs replication still in beta.

---

## 4. Proposed roadmap for rynas

**Phase 1 (MVP): "a DS923+ running Debian, managed from the browser"**
- Core: Rust daemon (`rynasd`) with JSON-RPC API over WebSocket, session + TOTP authentication, asynchronous jobs, configuration database and file generators, drift detection.
- Storage: disks (lsblk/udev), SMART with smartctl and scheduled tests, mdadm (create, repair, add disk, resync with progress), btrfs on top of mdadm (volumes, subvolumes as shares, scheduled snapshots with retention, scheduled scrub).
- Shares: SMB (Samba with previous versions and extra options) and NFS.
- Local users and groups, with per-share permissions.
- Read-only networking, plus static IP/DHCP and DNS with test and rollback.
- Dashboard: CPU, RAM, network, disks and temperatures.
- Notifications via email and ntfy/webhook.
- apt updates with a snapshot before upgrading.
- "Synology platform" plugin: LEDs, fans and buzzer.

Rationale: it covers 80% of home NAS usage and replaces DSM on the DS923+.

**Phase 2: services and protection**
- Docker/Podman with compose stacks, catalog from git repositories (schema → form) and stack backups.
- Backup: rsync to another NAS, restic/rclone to the cloud, btrfs send/receive replication.
- UPS with NUT, also as a network server.
- Full networking: bond, VLAN, bridge, per-service nftables firewall.
- Log center with journald, SMB audit and metrics history.
- Third-party declarative plugins.
- WebDAV, FTP/SFTP.
- Power on/off scheduling (rtcwake).

**Phase 3: "pro" level**
- ZFS (plugin, separate license via DKMS).
- Exposed LVM, SHR-like with mixed-size disks.
- iSCSI/NVMe-oF, S3 (Garage/MinIO as an app).
- AD join and LDAP client, OIDC/passkeys.
- VMs with libvirt and LXC.
- Built-in remote access (Tailscale/WireGuard), multi-NAS.
- Guided first boot and a DSM migration assistant (import shares and users).

---

## 5. Configuration management and extensibility

| | Configuration | Extensibility | Pros | Cons |
|---|---|---|---|---|
| **OMV** | `/etc/openmediavault/config.xml` with a JSON-schema datamodel. `omv-engined` RPC (PHP). Dirty modules in `/var/lib/openmediavault/dirtymodules.json`. `omv-salt deploy run <module>` generates the files and restarts services. | Plugins as .deb packages: datamodel, PHP RPC, Salt states, YAML UI in `/usr/share/openmediavault/workbench/*.d`. omv-extras repo. | Declarative, "just Debian", simple plugins, atomic Apply | Awkward XML, heavy Salt (led to dropping 32-bit), overwrites manual edits, PHP |
| **TrueNAS** | `middlewared` (Python) with sqlite datastore `/data/freenas-v1.db` and `etc` plugins that generate files (Mako templates). | Apps (Docker Compose catalog in git, schema-driven forms). No system plugins: it is a read-only appliance. | Single versioned API, jobs, strong validation, boot environments | Monolithic, closed: no `apt install`. sqlite lock, secrets encrypted with a separate key |
| **DSM** | Key-value files (`/etc/synoinfo.conf`, `/usr/syno/etc/*`), `synoservice` services, `SYNO.*` web API (synowebapi). | `.spk` packages (INFO, start/stop scripts, resources, privileges), Package Center. | Complete packages (UI, ports, firewall, permissions) | Proprietary, scattered and undocumented configuration, non-public APIs |
| **Unraid** | `.cfg` files on a USB stick (`/boot/config`), OS in RAM. GraphQL API since 7.2. | `.plg` plugins (XML with scripts and events), Community Apps (Docker XML templates). | Huge community, powerful plugins | Weakly typed, plugins break the UI on upgrades, paid license |
| **Cockpit** | No database: reads and writes the system directly (D-Bus, files, commands). | Modules = folder with `manifest.json` and HTML/JS. | Zero drift, very lightweight | No model, no transactions, every module does its own thing |

**Recommendation for rynas.** A hybrid of TrueNAS and OMV, with Cockpit's discipline:
1. Desired state in **versioned TOML/SQLite**, with serde validation and an exported JSON schema. It is diffable and easy to back up.
2. **Rust generators** (templates, e.g. minijinja) that write files atomically with a header and a hash. No Salt.
3. **Two-phase apply**: preview of the differences, then apply, with rollback for networking.
4. **Reconciliation with the real state**: storage is always read from the system, the database only holds policies.
5. **Plugins**: declarative manifest with configuration schema, generators, menu entries and widgets, plus a separate JSON-RPC backend (external process, any language). This avoids the unstable Rust ABI and isolates crashes. Distributed as .deb packages.

---

**Main sources:**
- https://www.synology.com/en-us/company/news/article/dsm73 · https://www.synology.com/en-us/company/news/article/dsm74
- https://www.truenas.com/blog/truenas-25-10-1 · https://www.truenas.com/blog/truenas-plans-for-2026 · https://www.truenas.com/docs/truenasapps/
- https://docs.openmediavault.org/en/7.x/development/howitworks.html · https://docs.openmediavault.org/en/8.x/various/advset.html · https://linuxiac.com/openmediavault-8-nas-launches-with-debian-13-upgrade/
- https://docs.unraid.net/unraid-os/release-notes/7.2.0/ · https://unraid.net/blog/unraid-7-3-0
- https://github.com/45Drives/cockpit-file-sharing/wiki · https://rockstor.com/docs · https://nascompares.com/2026/07/22/is-hexos-still-ok-checking-in-with-hexos-in-mid-2026/ · https://www.notebookcheck.net/ZimaOS-1-5-0-goes-paid-community-reacts-with-dissapointment.1122850.0.html
- Complaints: r/homelab thread "I quit TrueCharts" (2024), r/OpenMediaVault on file overwriting, Unraid forums on array performance, https://www.androidauthority.com/synology-third-party-drive-policy-3605210

For this report I did not open the Proxmox VE and Webmin forums: the points about them come from general knowledge and need verification.
