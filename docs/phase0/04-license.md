# rynas — License and business model (proposal)

Source: `research/04-license-business.md` (market models, prices, URLs).
This is not legal advice: the final text must be reviewed.

## The "open source" issue
Orlok's BSL 1.1 **is not open source** (OSI/DFSG): the license says so itself. With the BSL,
rynas must be presented as **"source-available, becomes Apache 2.0 after 4 years"** (or "fair source").
If the words "open source" are a must, it takes AGPLv3 + a commercial license (Proxmox),
but then companies can use it for free and revenue comes only from repo/support/plugins.

## Proposal (consistent with Orlok)

| Component | License |
|---|---|
| Core (rynasd, rynas-web, UI, CLI, builtin plugins) | **BSL 1.1**, grant: personal + non-commercial use; Change Date 4 years → Apache-2.0 |
| Plugin SDK, `rynas-api-types` | **Apache-2.0** (third-party plugins, proprietary ones included) |
| Official community plugins (synology, nut, docker…) | Apache-2.0 *or* BSL like the core (decision D-LIC2) |
| Business plugins | proprietary EULA |
| Documentation | CC BY 4.0 |
| Name and logo | excluded; trademark policy; EUIPO registration to be evaluated |
| Contributions | DCO + contributions under Apache-2.0 (inbound), stated in CONTRIBUTING |

Note on synodeb: synodeb is GPL-2.0. The synology plugin does **not** include synodeb code: it talks to
synohwd via files/socket, so there is no conflict. The `synohw` crate (Rust rewrite) would be born
inside synodeb under GPL-2.0, and the plugin would use it as a separate process.

## Repos (Proxmox model, without its flaws)

| Repo | Access | Content |
|---|---|---|
| `test` | public | release candidates |
| `community` | public | every release, immediately |
| `business` | authenticated (`/etc/apt/auth.conf.d/rynas.conf`) | the same release after 2–4 weeks without regressions + security backports on a **2-year LTS** + business plugins |

- deb822 files with `Signed-By`, `rynas-archive-keyring` package, `Valid-Until`.
- The business repo is installed **disabled**: `rynasctl license activate <file>` enables it
  (no 401 errors by default, unlike Proxmox).
- **No "No valid subscription" popup**: no warning at all for Community; a discreet warning
  only when a business plugin is active without a license.

## Business license
- **Ed25519**-signed file (customer, tier, server, expiry), verified **offline** in the binary.
- Optional machine binding (`machine-id` hash), self-service reset.
- On expiry only the business repo and updates are lost; nothing stops working
  (30-day grace period for business plugins), shares and data are **never** touched.

## Tiers (indicative, per server/year)

| Tier | Price | For whom | What |
|---|---|---|---|
| Community | free | personal use, non-profits, schools | the whole NAS: storage, shares, users, file manager, synology, docker, local backup |
| Supporter | ~€30 | homelabbers who want to support the project | business repo (stable/LTS), no support |
| Business | ~€120–150 | freelancers, SMBs, business use | production license, business repo, basic business plugins, e-mail support |
| Enterprise/MSP | ~€450–600 | companies, MSPs | + SLA, offline key, multi-NAS console, white-label |

Golden rule: **everything a homelab needs stays free**. Business = organizational
needs: advanced AD/LDAP, NAS-to-NAS replication with retention and WORM, multi-NAS console,
RBAC with delegation, signed audit/SIEM, compliance reports, cloud backup with policies, HA, white-label.
