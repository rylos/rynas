# rynas — License and business model

Decided 2026-11-07 (Marco: "the same license model as Proxmox VE"). It replaces the BSL 1.1
model of 2026-11-04. Background research: `research/04-license-business.md`.
This is not legal advice.

## The model

rynas is **free software**, the way Proxmox VE is: the whole code base is under the
**GNU AGPL v3 or later**, everyone may use it for free, companies included, and every feature
is in the free version. Revenue, when it comes, comes from **a stable update repository and
support**, never from locked features.

| Component | License |
|---|---|
| Core (rynasd, rynas-web, UI, CLI, builtin plugins, files worker) | **AGPL-3.0-or-later** |
| `rynas-api-types` and the plugin SDK | **Apache-2.0**, so third-party plugins can use any license |
| App catalogue (`apps/`) | AGPL-3.0-or-later like the core; icons from Dashboard Icons under Apache-2.0 (`apps/ICONS.md`) |
| Documentation | AGPL-3.0-or-later with the code (CC BY 4.0 can be added later) |
| Name and logo | not covered by the license; a trademark policy will say how they may be used |
| Contributions | AGPL-3.0-or-later plus a relicensing grant to Marco Ziliani (`CONTRIBUTING.md`), so another licence stays possible; no CLA for now, a signed one (Proxmox-style) when larger outside contributions arrive |

Why AGPL and not GPL: rynas is used over the network, and the AGPL makes a modified
version offered as a service share its source too.

synodeb is GPL-2.0. The synology plugin does not include synodeb code: it talks to synohwd
through files and a socket, so there is no conflict.

## Repositories

| Repo | Access | Content |
|---|---|---|
| `test` | public | release candidates |
| `community` (no-subscription) | public, free | every release, immediately |
| `enterprise` | with a subscription key, **later** | the same release after 2–4 weeks without regressions, plus security backports on an LTS |

**Today there is only the free repository** (Marco, 2026-11-07: no paid repo for now, it will
come one day). When it arrives:

- deb822 files with `Signed-By`, a `rynas-archive-keyring` package, `Valid-Until`;
- the enterprise repo is installed **disabled** and is turned on when a key is entered, so a
  fresh install never fails `apt update` with a 401;
- **no "no valid subscription" popup**: the free version shows no nag.

## Subscriptions (later, indicative, per server per year)

| Tier | For whom | What |
|---|---|---|
| Community | everyone | all of rynas, the free repo, community help |
| Supporter | homelabbers who want to support the project | enterprise repo, no support |
| Standard | freelancers, small businesses | enterprise repo, e-mail support |
| Premium | companies, MSPs | enterprise repo, faster support, an SLA |

Golden rule: a subscription buys **tested updates and support**, never a feature.
