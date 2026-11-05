# rynas: license and business model (report)

> This report is not legal advice. Before publishing, have the license text, the CLA and the business EULA reviewed by a lawyer experienced in intellectual property and software.

## 0. Summary and recommendation

**Recommendation.** Use the same license as Orlok for the core, i.e. **BSL 1.1** with an Additional Use Grant for personal and non-commercial use and a 4-year Change Date to **Apache-2.0**. The other parts:
- **plugin SDK** under **Apache-2.0**;
- **business plugins** under a proprietary license (EULA);
- **contributions** with DCO, accepted under Apache-2.0 ("inbound Apache");
- registered **trademark**.

The model is a **hybrid of Proxmox and open core**:
- a public **community** apt repo, which gets updated code first;
- an authenticated **business** repo, with a stable/LTS channel and exclusive plugins;
- a commercial license required for anyone running rynas in production at a company.

**Constraint: do not call it "open source".** The BSL is not OSI-approved, and the license says so itself: "The Business Source License is not an Open Source license" ([SPDX BUSL-1.1](https://spdx.org/licenses/BUSL-1.1.html)). The correct wording is "source-available", "fair source" ([fair.io](https://fair.io)) or "open code, becomes open source (Apache 2.0) after 4 years".

**Alternative 1.** **AGPLv3 + commercial license** (dual licensing like Proxmox). Choose it if being able to call it open source is the requirement that matters most.

**Alternative 2.** **FSL-1.1-ALv2**. It only forbids competing use and becomes Apache-2.0 after 2 years. Choose it if the goal is to maximize adoption and earn revenue only from repo, support and plugins.

---

## 1. What the market teaches

### Proxmox, the reference
- **License.** The code is under AGPLv3. Contributions go through a CLA based on Harmony 1.2: the contributor grants a license and does not assign copyright ([proxmox.com/developers](https://proxmox.com/en/about/open-source/developers), [Proxmox Individual CLA](https://dcmpx.remotevs.com/com/proxmox/www/SL/images/download/pve/agreements/Proxmox-Individual-CLA.pdf)).
- **Three repos.** `pve-test` gets packages first. They then move to `pve-no-subscription`, which is free, and finally to `pve-enterprise`, which is paid and more tested. **The code is identical:** only timing and validation differ. No-subscription users effectively act as beta testers for enterprise users.
- **Configuration.** Proxmox VE 9 installs deb822 files in `/etc/apt/sources.list.d/`: `pve-enterprise.sources` and `ceph.sources`. Without a key, `apt update` returns 401. A repo is disabled by adding `Enabled: false` ([computingforgeeks](https://computingforgeeks.com/proxmox-ve-no-subscription-repo/)).
- **Credentials.** They live in a file under `/etc/apt/auth.conf.d/`, tied to the subscription key and the server ID. The server ID is the MD5 hash of the host's RSA SSH key and must be registered online, except with the offline keys of the Premium plan ([pve-user, Oct. 2025](https://lists.proxmox.com/pipermail/pve-user/2025-October/017694.html)).
- **Nag.** Without a subscription a "No valid subscription" popup appears at every login. It is among the things the community hates most, and there is even an ecosystem of scripts to remove it. **Do not copy.**
- **2026 prices** (per socket per year, excluding VAT; [proxmox.com/pricing](https://www.proxmox.com/proxmox-ve/pricing), [horizoniq](https://www.horizoniq.com/blog/proxmox-subscription/)). The figures come from resellers: check them against the official price list.

  | Plan | Price | What it includes |
  |---|---|---|
  | Community | €120 | enterprise repo only |
  | Basic | €370 | 3 tickets |
  | Standard | €550 | 10 tickets, 4h response time |
  | Premium | €1,100 | unlimited tickets, offline key |

### Other models

| Product | License | Monetization | Lesson for rynas |
|---|---|---|---|
| TrueNAS CE / Enterprise | GPL + proprietary components | hardware appliances; HA, RDMA and iSER Enterprise-only ([forum 25.04](https://www.reddit.com/r/truenas/comments/1iomzcm/truenas_2504beta1_now_available/), [Wikipedia](https://en.wikipedia.org/wiki/TrueNAS)) | exclusive HA and enterprise features do not annoy homelab users |
| Unraid (since 2024) | proprietary | Starter $49, Unleashed $109, Lifetime $249; after the first year $36/year for updates, but the system keeps working ([unraid.net/blog/new-pricing](https://unraid.net/blog/new-pricing)) | if the license expires the system keeps working: expiry only removes updates |
| OpenMediaVault | GPLv3 | donations | large community, no revenue |
| HexOS | proprietary, based on TrueNAS | lifetime $199 in early access, then $299 per server; cloud backup and support extra ([nascompares 2026](https://nascompares.com/2026/07/22/is-hexos-still-ok-checking-in-with-hexos-in-mid-2026/)) | usage-based services (cloud) stay outside the license |
| Synology DSM | proprietary | sold with the hardware | not applicable |
| Portainer CE / BE | CE zlib; BE proprietary | BE free up to 3 nodes, one license per organization, annual renewal ([portainer.io/take-3](https://www.portainer.io/take-3)) | a free tier for small use generates many sales leads |
| GitLab CE / EE | CE MIT; EE source-available | open core | exclusive plugins work if they address organizational needs, not base features |
| Sentry | FSL (2023) | SaaS | the license was created to forbid competition, not internal use ([Sentry blog](https://blog.sentry.io/introducing-the-functional-source-license-freedom-without-free-riding/)) |
| HashiCorp | MPL → BSL (2023) | — | changing license on an established project led to the OpenTofu fork |
| Elastic, Redis | SSPL/ELv2, then AGPL added (Elastic 2024, Redis 8 in 2025, [redis.io](https://redis.io/blog/agplv3/)) | — | going back costs trust: **choose the final license from day one** |
| MariaDB MaxScale | BSL 1.1, where the license originated ([mariadb.com/bsl11](https://mariadb.com/bsl11)) | — | absent other terms, the default Change Date is 4 years |
| Netdata | GPLv3 agent; dashboard v2 under the proprietary NCUL1 license (to verify, the research did not confirm it) | — | a closed UI inside an open project was poorly received |

---

## 2. "Personal" BSL and the word "open source"

**Two inconsistencies to resolve:**
1. **The name.** The BSL meets neither the OSI Open Source Definition nor Debian's DFSG. Orlok's grant even excludes freelancers and MSPs, so it is more restrictive than FSL and ELv2. Calling it "open source" invites public criticism and marketing disputes.
2. **Distribution.** A BSL package cannot go into Debian `main`, at most into `non-free`. That is fine, since the intended channel is our own apt repo, but it means giving up on inclusion in Debian.

**Comparison for a homelab NAS that wants a broad community and third-party plugins:**

| License | Pros | Cons |
|---|---|---|
| **BSL 1.1 with Orlok-style grant** | consistent with Orlok; forces companies to pay; becomes Apache-2.0 after 4 years | not OSI; excluded from Debian; less motivation for core contributors; the boundary of "personal use" must be defined (a homelab also used for working from home?) |
| **FSL-1.1-ALv2** | standard text; 2 years; well regarded in "fair source"; only forbids competing use ([SPDX](https://spdx.org/licenses/FSL-1.1-ALv2.html)) | **companies use it for free in production**: revenue comes only from repo, support and plugins |
| **AGPLv3 + commercial** | true open source; can go into Debian; the Proxmox model; maximum trust | revenue depends on added value (stable repo, support, plugins), because the AGPL allows commercial use; a CLA is needed to relicense |
| **PolyForm Noncommercial / Small Business** | short, clear texts | never become open source; the "small business" criterion is hard to verify; little known in homelab circles |
| **Elastic License 2.0** | simple | designed against resale as a managed SaaS, which is not the risk for a NAS; internal business use remains free |

**Conclusion.** If the constraint is "companies pay for production use", the only suitable licenses are **the BSL with an Orlok-style grant** or, to a lesser extent, PolyForm. FSL and ELv2 do not guarantee it. So BSL, accepting that it cannot be called open source.

---

## 3. Proposed licenses per component

| Component | License | Reason |
|---|---|---|
| **Core** (daemon, Web UI, CLI, .deb package) | BSL 1.1, Personal + Non-Commercial grant, Change Date = publication + 4 years, then Apache-2.0 | like Orlok; each release has its own date |
| **Plugin SDK** (Rust crates, protocol, API schemas, examples) | **Apache-2.0** (or MIT/Apache dual, per Rust convention) | allows third-party plugins, proprietary ones included, without their authors having to accept the BSL; Apache's patent clause protects both sides |
| **Community plugins** (official, free) | Apache-2.0 | anyone can fork and improve them; they attract contributions |
| **Third-party plugins** | author's choice (proprietary too) | ecosystem; paid third-party plugins are fine |
| **Business plugins** | proprietary EULA, source visible to customers (optional) | the exclusive value of the business repo |
| **Documentation** | CC BY 4.0 | |

**Technical measure.** Keep plugins **out of the core process** (separate process via JSON-RPC/gRPC over a Unix socket, or WASM) with a versioned API in the SDK. This way the license boundary matches the technical one, and a faulty plugin does not bring down the NAS.

**Contributions.**
- **SDK and community plugins:** DCO (`Signed-off-by`), with contributions under Apache-2.0.
- **Core:** two options.
  - (a) **DCO + inbound Apache-2.0** (recommended). The contributor licenses their code under Apache-2.0, which is permissive and so allows including it in the BSL release and selling it under a commercial license. Less burdensome than a CLA.
  - (b) **Harmony CLA** with a license grant, as Proxmox does. Sturdier if the license ever changes.

  With option (a), `CONTRIBUTING.md` must state: *"Contributions are licensed to the project under Apache-2.0 and may be distributed under BSL 1.1 and commercial terms."*

**Trademark.**
- The name "rynas" and the logo stay excluded from all licenses, as in Orlok.
- Publish a **Trademark Policy** modeled on Mozilla/Proxmox: forks must change name; "compatible with rynas" is allowed.
- Register the trademark with the EUIPO in classes 9 and 42, after a prior-art search. Roughly €850 for the first class and €50 for the second: check current fees.

---

## 4. Business repo content, tiers and key verification

**Choice: a hybrid.**
1. **Same core, different channel** (like Proxmox). `community` gets every release immediately. `business` gets the same release after 2-4 weeks without regressions, plus security backports on a **2-year LTS** line. Companies need stability, not more code.
2. **Exclusive plugins** only for organizational, scale or compliance needs. **Everything a homelab needs stays free**: SMB/NFS, ZFS/Btrfs, snapshots, users, local backup, basic AD join, Docker/apps, notifications. This is the lesson from GitLab and TrueNAS. The opposite, removing base features, is what angered the Unraid and Netdata communities.

**Proposed tiers (indicative prices, per server per year):**

| Tier | Price | For whom | What it includes |
|---|---|---|---|
| **Community** | free | personal and non-profit use (BSL grant) | community repo, forum, community plugins |
| **Supporter** (optional) | ~€30 | homelabbers who want to support the project | access to the business repo (stable channel), badge; no support |
| **Business** | ~€120-150 | freelancers, small businesses, business use | **commercial production license**, business repo + LTS, basic business plugins, email support with next-business-day response |
| **Enterprise / MSP** | ~€450-600 (volume discounts for MSPs) | larger companies, MSPs | everything in Business + 4h SLA, offline key for air-gapped environments, multi-NAS console, branding |

A Portainer-style free tier, "1 business server free", is an option for generating leads. However it weakens the grant and must be decided carefully.

**Candidate business features.**
- **Advanced AD/LDAP:** multi-domain, nested groups, Kerberos SSO, ACL mapping per OU.
- **Replication and data protection:** scheduled NAS-to-NAS replication with retention; immutable/WORM snapshots.
- **Management and access:** **multi-NAS console**, the best "MSP" candidate; granular RBAC with delegation.
- **Compliance:** signed, exportable audit log (syslog/SIEM); compliance reports (e.g. NIS2/GDPR backups).
- **Encrypted cloud backup** (S3/B2) with policies and reports. If we ever offer our own cloud storage, it should be treated as a usage-based service, as HexOS does.
- **Two-node HA/failover** (phase 2).
- **White-label** for MSPs; support.

**Key verification without hostile DRM.**
- **Format:** **Ed25519**-signed license file: JSON with customer, tier, number of servers, expiry, key ID and signature. The binary contains the public key, crate `ed25519-dalek`. Verification happens **locally, offline**, without contacting any server.
- **Machine binding:** optional and permissive. A hash of the `machine-id` declared in the file, with self-service reset from the customer portal. Never bind it to hardware.
- **Expiry:** removes **only** access to the business repo and new business plugin updates (Unraid style). Installed business plugins **keep working** with a 30-day grace period and then a warning. Never block shares, storage or data.
- **Warnings:** no popup for Community. A discreet dashboard warning appears only when a business plugin is active without a valid license.
- **Enforcement:** mostly contractual (BSL license + EULA) and through repo authentication, not obfuscation. The code is readable: whoever wants to bypass the checks will manage anyway, and paying customers must not be penalized.
- **Telemetry:** optional, with explicit consent (opt-in) and documented.

---

## 5. apt distribution

**Community repo (public).**
- Two channels: `deb.rynas.<tld>`, suite `trixie`, components `main` and `test`.
- Managed with **aptly** or **reprepro**. The repo is static, so it can be hosted on a CDN or object storage (R2, Bunny, S3 + CloudFront).
- Free, authorized **mirrors**: public rsync.

**Business repo (authenticated).**
- `enterprise.rynas.<tld>`, same structure, with components `business` (stable core + LTS) and `plugins`.
- Client side, `/etc/apt/auth.conf.d/rynas.conf` (owner root, mode 0600):
  ```
  machine enterprise.rynas.example/debian
  login <license-id>
  password <derived-token>
  ```
  The token is derived from the license, it is not the whole key. It is written by the `rynas license activate <file>` command, which is also the only activation step.
- Server side, nginx with `auth_request` to a small service that checks ID and token, license status and rate limit. An htpasswd file regenerated by a job is enough.
- Customers may run an **internal** mirror (apt-cacher-ng, aptly mirror), and the EULA should explicitly allow it. Public mirrors of the business repo are not allowed.

**GPG signing.**
- An offline master key, with separate signing subkeys for the community and business repos, rotated periodically.
- Signed `InRelease` and `Valid-Until` set, against replay attacks.
- **`rynas-archive-keyring`** package installing `/usr/share/keyrings/rynas-archive-keyring.gpg`.
- deb822 file with a dedicated `Signed-By:`, without the deprecated `apt-key`:
  ```
  # /etc/apt/sources.list.d/rynas.sources
  Types: deb
  URIs: https://deb.rynas.example/debian
  Suites: trixie
  Components: main
  Signed-By: /usr/share/keyrings/rynas-archive-keyring.gpg
  ```
  The business repo file is similar, and the package installs it with `Enabled: false`. The `activate` command enables it, like Proxmox but the other way round: no 401 errors by default.

**Other.**
- Publish an **SBOM** and signed checksums of every .deb and ISO on the website.
- Publish the **source tags** on a public Git repo. With the BSL, public source is also the precondition for the conversion to Apache after 4 years.
- **Separate packages:** `rynas` (core, BSL), `rynas-sdk` (crate on crates.io, Apache), `rynas-plugin-*` (community, Apache), `rynas-business-*` (EULA). Each package's `License:` field must be declared explicitly in `debian/copyright` (DEP-5 format).

---

## 6. The two alternatives in brief

**Alternative 1: AGPLv3 + commercial (pure Proxmox).**
- It can be called open source, can go into Debian and enjoys the maximum trust of the community and contributors.
- Revenue comes from the enterprise repo, support and proprietary business plugins.
- Risk: companies can use it for free. A CLA is needed to sell license exceptions.
- Worth it if the community matters more than the "companies pay" constraint.

**Alternative 2: FSL-1.1-ALv2 on the core + proprietary business plugins (open core).**
- Standard, modern license, becomes Apache-2.0 after 2 years, and is well regarded in "fair source".
- It only forbids competing products, so companies use it for free. The revenue model is like GitLab's: everything on plugins and the business repo.
- The most community-friendly compromise, but it moves away from Orlok.

**Recommended choice:** Orlok-style BSL 1.1 on the core, Apache-2.0 SDK and community plugins, DCO with inbound Apache contributions, business repo with a stable/LTS channel and exclusive plugins, offline Ed25519-signed license without nagging, registered trademark. To be communicated as "**source-available, becomes Apache 2.0 after 4 years**", never as "open source".
