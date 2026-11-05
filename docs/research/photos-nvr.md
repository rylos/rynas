# Rynas: photos and video surveillance, analysis and proposal

The project documentation already plans `@eaDir` thumbnails in the files module, the compose catalog in phase 2 and OIDC in phase 3; this report builds on those.

---

## Topic 1: photos

### Comparison (as of November 2026)

| App | License | Status | Phone backup | Recognition (faces, CLIP) | External library | Multi-user and SSO |
|---|---|---|---|---|---|---|
| **Synology Photos** | proprietary | mature, DSM only | yes, official app | faces, objects | yes, shared folders | DSM users, team shared space, links |
| **Immich** | AGPL-3.0 | 118k stars, v3.3.6 (2026-11-04), roughly one release a week | **yes, native app, Google Photos style** | faces, CLIP, OCR; OpenVINO; remote ML | **yes, read-only** if mounted `:ro` | yes, OIDC with quotas and roles taken from the IdP, partners, shared albums and links |
| PhotoPrism | AGPL plus paid parts | 40k stars, monthly releases | no native app with backup (PWA, PhotoSync) | faces, classification | **yes, indexes existing folders** | advanced users and roles only in paid editions |
| Ente (self-hosted) | AGPL-3.0 | 30k stars, active | yes, excellent | on-device ML, because it is E2E | **no**: E2E encryption prevents indexing existing folders | own accounts |
| LibrePhotos | MIT | 8k stars, 1.5.1 | weak | yes | yes | basic |
| Nextcloud Memories | AGPL | v11, active | through the Nextcloud app | yes, with Recognize | yes | requires **all of Nextcloud** |
| Lychee / Piwigo | MIT / GPL-2 | active | no | none or minimal | partial | galleries for publishing, not personal libraries |
| PhotoView | AGPL | **stalled since 2024 (v2.4.0)** | no | basic faces | yes | basic |

### Best ideas to borrow

- **Immich, read-only external library**: it indexes existing folders without copying them. If the volume is mounted `:ro`, Immich cannot delete files or write XMP sidecars ([docs](https://docs.immich.app/features/libraries)).
- **Immich, remote ML with fallback**: several URLs can be configured and are tried in order. The initial indexing can therefore run on a PC with a GPU, for example a workstation with an RTX 2060, while the local container stays as a fallback ([docs](https://docs.immich.app/guides/remote-machine-learning)).
- **Immich, OIDC that syncs quotas and roles at every login** (since 3.3), with a dedicated redirect for the mobile app ([docs](https://docs.immich.app/administration/oauth)).
- **Synology Photos, the team shared space**: this is the small-office use case (customer photos), and Immich **does not have it** natively.
- **synmich**: migrates each Synology Photos user into their own Immich account, with albums, shares and shared space, reading the NAS read-only ([GitHub](https://github.com/t3ksin/synmich), MIT but a small project, 25 stars). The more general alternative is PhotoMigrator.

### Mistakes to avoid (user complaints)

- **Immich database on a spinning disk or a network share**: the documentation requires a local SSD and never a network share ([requirements](https://docs.immich.app/install/requirements)).
- **External library not read-only**: people who deleted from the web UI lost their originals.
- **Moving files inside an external library**: Immich sees them as new and descriptions and albums are lost. This is a known, documented issue.
- **File watching for external libraries is experimental** (`ENOSPC` errors, does not work on network shares): a scheduled scan is better.
- **Faces cannot be migrated**: no tool carries faces from Synology to Immich, so they must be recomputed. `@eaDir` thumbnails cannot be reused either: Immich generates its own, adding 10-20% in space.
- **Upgrading Immich blindly**: releases are frequent and major versions carry breaking changes. A snapshot of database and configuration is needed before every upgrade.
- **Ente as a "free Synology Photos"**: E2E encryption rules out the external library and integration with shared folders.

### Recommendation: integrate Immich, do not write a photos module

A custom photos module would mean mobile apps, ML, a timeline engine, maps and sharing: years of work chasing Immich. Immich is the most active project in this space and already has everything needed to integrate it well.

**What rynas does:**
1. **Curated "Photos (Immich)" app** in the compose catalog, with locked values: database on an SSD pool (the M.2 drives), ML on or off, OpenVINO profile only where it makes sense.
2. **Shared folders as external libraries**: a wizard to pick the share, the owning user and read-only mode. rynas generates the `:ro` mount in the compose file and creates the library through the Immich API.
3. **Users**:
   - phase 2: rynas creates Immich accounts via the admin API, with a quota equal to the rynas quota.
   - phase 3: rynas acts as the OIDC IdP (directory module) and Immich uses auto-register plus the quota and role claims.
4. **Automatic btrfs snapshot** of the data subvolume and of the database dump before every upgrade, with one-click rollback.
5. **"Open in Photos"** from the file manager, leading to the asset or folder in Immich. File manager thumbnails remain rynas's own, `@eaDir` included.
6. **Guided migration** from Synology Photos: shares stay where they are and become external libraries; for albums and shares synmich runs as an optional job.

**What it delegates:** timeline, ML, mobile apps, link sharing, faces. All of this stays with Immich.

**Gap to declare:** there is no team shared space. For customer photos in an office, one option is an external library on a customers share owned by a service user, plus partner sharing or shared albums. Alternatively those photos stay managed as files in the file manager. To be validated with the maintainer.

### Hardware requirements

| | DS923+ (R1600, no GPU, 4 GB stock) | DS920+ (J4125, UHD 600, 4 GB stock) |
|---|---|---|
| RAM | **at least 8 GB, 16 preferred** (Immich requires 6 minimum, 8 recommended) | upgrade to 8 GB (official max) |
| ML | on CPU: works, but initial indexing of tens of thousands of photos takes days. Use **remote ML on the GPU PC** for the first pass | on CPU (the J4125 is x86-64-v2, so supported). OpenVINO on UHD 600 is not advised: the docs mention Iris Xe/Arc and warn that older iGPUs cause problems |
| Video | CPU transcoding, slow: limit or disable it | **Quick Sync for transcoding**: fine |
| Database | on an M.2 SSD pool | on SSD (SATA is fine too) |

---

## Topic 2: cameras and NVR

### Comparison

| NVR | License and cost | Status | Object detection | Live in browser | Mobile app and notifications | Notes |
|---|---|---|---|---|---|---|
| **Surveillance Station** | 2 free licenses, then about €50-60 per camera (packs of 1, 4, 8), max 40 cameras on the DS923+ | mature, DSM only | on Synology cameras or in DVA | good | DS cam, excellent | since 2026 new Synology cameras also need licenses ([NASCompares](https://nascompares.com/news/synology-official-cameras-now-need-a-camera-license-why/)) |
| **Frigate** | MIT; optional Frigate+ for trained models | 37k stars, **0.18.2 (2026-10-15)** | yes, with a dedicated detector: OpenVINO, Hailo, Coral, NPU | **WebRTC/MSE via go2rtc** | built-in web push notifications, MQTT, Home Assistant | since 0.18 everything is configurable from the UI; has face and license plate recognition and GenAI |
| Scrypted | open core; the NVR module costs about **$40/year for 4 cameras** | active | yes | very low latency | HomeKit and Google Home | great as a bridge to HomeKit, less so as a server NVR |
| Viseron | MIT | 3.7k stars, 3.7.0 | yes | yes | limited | small, single maintainer |
| ZoneMinder | GPL-2 | long-standing, 1.38.x | with add-ons | weak | zmNinja | dated UI, heavy |
| Shinobi | non-standard license | GitHub repo idle since 2020 (development is on GitLab) | plugins | yes | – | small community |
| AgentDVR | free for local private use; **business use and remote access are paid ($7.95-19.95/month)** | active | yes | yes | via iSpy cloud | unsuitable for an office without a subscription ([pricing](https://www.ispyconnect.com/pricing)) |
| motionEye | GPL-3 | maintenance mode | motion only | MJPEG | no | hobbyist |
| Blue Iris | Windows, paid | – | yes | – | yes | reference only |

### Best ideas to borrow

- **Frigate: detection only on the substream, recording of the main stream copied without re-encoding.** This is the key for a GPU-less CPU like the R1600.
- **go2rtc** (MIT, v1.9.15) acts as a single restream hub: one connection per camera, WebRTC/MSE in the browser with under 1 s latency. The rynas widget can use it directly.
- **Frigate: authentication via proxy headers** (`Remote-User`, `X-Forwarded-Groups`, `role_map`, per-camera roles). rynas, acting as reverse proxy, provides SSO **without needing OIDC** ([docs](https://docs.frigate.video/configuration/authentication)).
- **Frigate: separate retention** for continuous recording, motion, alerts and detections, plus the safety rule that deletes the oldest files when less than about an hour of space is left ([docs](https://docs.frigate.video/configuration/record)).
- **Surveillance Station: a single hub, a polished mobile app, simple license management.** Copy the simplicity, not the licensing model.

### Mistakes to avoid

- **Coral**: Frigate itself says it is *"no longer recommended for new installations"*. Hailo-8L or OpenVINO are better ([hardware](https://docs.frigate.video/frigate/hardware)).
- **AVX requirement**: since 0.17 Frigate *"requires a modern CPU with AVX"* ([FAQ](https://github.com/blakeblackshear/frigate/discussions/21275)). **The J4125 (Gemini Lake) has no AVX.** With the OpenVINO detector on GPU it historically worked, but TensorFlow features (classification) crash with "illegal instruction". **On the DS920+ it must be tested before it is promised.** The R1600 has AVX2: no problem.
- **Recording onto a btrfs subvolume with a quota**: Frigate looks at filesystem free space, not the qgroup, and also counts files that are not its own. On top of that, CoW and snapshots fragment and bloat space under continuous writes.
- **Spin-down**: with continuous recording the disks of the recordings volume never stop. If recordings and data share a volume, no disk stops.
- **Wi-Fi cameras**: packet loss causes memory growth and freezes (0.17 FAQ).
- **Frigate upgrades**: every minor release has breaking configuration changes (0.18 changed GenAI, zones and masks, snapshots, FFmpeg 8). Back up `config` and `frigate.db` first. Intel GPU stats require kernel 6.5 or later: Debian 13 ships 6.12, fine.
- **Frigate port 5000 is unauthenticated** and treated as admin: never expose it.

### Recommendation: Frigate as a curated app, integrated with storage, dashboard and notifications

Writing a custom NVR makes no sense. Frigate is MIT, the most active, uses ONVIF/RTSP with go2rtc and has the Home Assistant ecosystem.

**What rynas does:**
1. **Dedicated recordings volume**: a separate LV (rynas already has hidden LVM) or a separate disk, ext4 or xfs, **without snapshots or CoW**, mounted at `/media/frigate`. A check blocks installation if `/media/frigate` is on the data volume, because the docs warn that otherwise recordings end up on the system disk. Disks of the recordings volume are excluded from spin-down; the others can stop.
2. **Curated "Video surveillance (Frigate)" app**:
   - camera wizard: ONVIF discovery, choice of main stream and substream, retention in days with an estimate of GB per camera;
   - hardware presets per NAS model;
   - mosquitto as a sidecar;
   - port 5000 closed;
   - authentication via rynas proxy headers, with rynas users and groups mapped to admin, viewer or per-camera roles.
3. **Dashboard widget**: previews from `/api/<cam>/latest.jpg` and live view on click via go2rtc WebRTC; latest events.
4. **rynas notification center**: rynas reads events (MQTT `frigate/reviews`) and forwards them over rynas channels (mail, ntfy, Telegram) with a snapshot. Frigate push notifications remain available.
5. **Snapshot** of configuration and database before upgrades, and a pinned version (no floating `stable`).

**What it delegates:** detection, timeline, event review, semantic search, LPR and GenAI stay with Frigate. For mobile: the Frigate PWA or Home Assistant; for HomeKit, Scrypted as an optional catalog app.

### Hardware requirements

| | DS923+ (R1600 2C/4T, AVX2, no GPU) | DS920+ (J4125, UHD 600, **no AVX**) |
|---|---|---|
| Decoding | CPU only: decode 640x360 substreams at 5 fps, record by copy | VAAPI/QSV for H.264/H.265 |
| Detector | OpenVINO on CPU (AMD "not officially supported but works"): realistically **2-4 cameras**. Beyond that a **Hailo-8L M.2** is needed (about 11 ms per inference), which can be installed on Debian unlike DSM; slot and driver to be verified on the DS923+ | OpenVINO on GPU, around 20-50 ms estimated (the UHD 600 is not in the table, the HD 530 does 15-35 ms): **4-6 cameras**, **to be validated because of AVX** |
| RAM | 8 GB for Frigate alone, 16 GB with Immich | 8 GB, tight with Immich |
| Disks | dedicated volume, about 20-40 GB per day per 4 MP camera recording continuously | same |

For an office the safe choice is the DS923+ with a Hailo-8L, or a few cameras on CPU.

---

## Phases

| Phase | Photos | Cameras |
|---|---|---|
| **A, with the docker catalog (phase 2)** | Curated Immich app with database on SSD, `:ro` external libraries via wizard, users created via API, snapshots before upgrades, "Open in Photos" | Curated Frigate app with dedicated LV, per-NAS presets, proxy authentication, port 5000 closed |
| **B** | Migration from Synology Photos (shares as libraries plus synmich job), configurable remote ML | Dashboard widget with WebRTC, MQTT events in the rynas notification center, ONVIF discovery |
| **C, with directory (phase 3)** | OIDC from rynas with quotas and roles; answer to the team space (customers library or upstream proposal) | Per-camera roles from AD/LDAP groups, optional Scrypted for HomeKit |

**Open decisions:**
- whether the team shared space is essential for customer photos;
- a test of Frigate 0.18 on the DS920+ to check the AVX issue;
- verification of the Hailo-8L on the DS923+.

---

## Sources

- https://docs.immich.app/install/requirements
- https://docs.immich.app/features/libraries
- https://docs.immich.app/administration/oauth
- https://docs.immich.app/features/ml-hardware-acceleration
- https://docs.immich.app/guides/remote-machine-learning
- https://github.com/immich-app/immich/releases
- https://github.com/t3ksin/synmich
- https://github.com/jaimetur/PhotoMigrator
- https://github.com/simulot/immich-go
- https://docs.photoprism.app/getting-started/
- https://www.photoprism.app/kb/membership
- https://ente.com/help/self-hosting/
- https://docs.frigate.video/frigate/hardware
- https://docs.frigate.video/configuration/record
- https://docs.frigate.video/configuration/authentication
- https://docs.frigate.video/configuration/notifications
- https://github.com/blakeblackshear/frigate/releases/tag/v0.18.0
- https://github.com/blakeblackshear/frigate/releases/tag/v0.17.0
- https://github.com/blakeblackshear/frigate/discussions/21275
- https://github.com/blakeblackshear/frigate/issues/14712
- https://github.com/AlexxIT/go2rtc
- https://billing.scrypted.app
- https://www.ispyconnect.com/pricing
- https://www.synology.com/en-global/products/Device_License_Pack
- https://nascompares.com/news/synology-official-cameras-now-need-a-camera-license-why/
- https://videor.com/de/generate/datasheet/render/product_sku/236867

Stars, licenses and release dates come from the GitHub API, queried on 2026-11-05.
