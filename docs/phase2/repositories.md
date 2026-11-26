# System › Software repositories (D-REPOS, 2026-11-25)

The apt sources of the system on one page: what they are, whether they are healthy and
supported, and the changes an administrator needs — Debian's own suites, components and
mirror, enabling or disabling an entry, adding or removing a third-party repository,
converting to deb822. It replaces the "Debian backports" switch of System › Updates, which
becomes part of the Debian section; Updates keeps a short link card and the "unsupported"
banner. Research and sources: [`docs/research/apt-repositories.md`](../research/apt-repositories.md).

Code: `crates/rynasd/src/repositories/` (`sources.rs` parsing and edits, `classify.rs`
origin, kinds and checks, `official.rs` Debian section and conversion, `aptlog.rs`
apt-get update output, `keys.rs` OpenPGP keys, `mod.rs` state, routes, tasks), API types
`crates/rynas-api-types/src/repositories.rs`, page `crates/rynas-ui/src/pages/repositories.rs`.

## 1. Why

- A NAS on plain Debian collects sources over time (vendor repositories, a mirror typed
  once, testing lines copied from a forum). Nothing showed them, and a broken or foreign
  source silently stops updates or replaces Debian packages.
- Debian 13 moves to deb822 `.sources` files; existing systems keep one-line files for
  years. Both must be read and written without losing anything.
- rynas itself writes sources (backports, Docker, Tailscale) and a hardware module's
  system layer ships its own: these must be shown but changed only where they belong.

## 2. Model

- **Files**: `/etc/apt/sources.list` and every `*.list`/`*.sources` in `sources.list.d`.
  One-line entries are lines (`# deb …` is a disabled entry, other comments stay); deb822
  entries are stanzas (multi-value fields kept together, `Enabled: no` disables). A file
  that does not parse (or is larger than 1 MiB, or not UTF-8) is reported with its error
  — apt refuses to run with it — and never blocks the others.
- **Ignored files**: other names in `sources.list.d`, listed as "ignored by apt" with the
  reason (`backup`, `name`, `extension`).
- **Digest**: sha256 over the sorted paths and contents of the files read plus the names
  of the ignored ones. `GET` returns it; every write sends it back; a stale one is 409
  `conflict`, checked when the request arrives and again when the task starts.
- **Entry index**: position in its file, disabled entries counted.

## 3. Classification

Per entry, in this order:

1. **rynas** — `rynas-backports.sources` (managed by the Debian section), `docker.sources`
   and `tailscale.sources` when they carry rynas's marker (`/docker`,
   `/network/remote-access`). Read-only here, with a link.
2. **System layer** — with a hardware module's system layer (UPS or boot layer), a file a
   package ships as a conffile (`/var/lib/dpkg/info/*.conffiles`) or named `synodeb*`.
   Read-only: "managed by the hardware support".
3. **Local media** (`local_media`) — every URI on the machine itself: the installation disc
   (`cdrom:[Debian GNU/Linux 13.x …]/`, which d-i leaves in `sources.list`, usually
   commented out) or a local folder (`file:`, `copy:`). Shown as "Installation disc" or
   "Local folder". Checked before Debian, because a disc's cached Release file says
   `Origin: Debian`. Never unsupported; an enabled disc entry gets the `cdrom_enabled`
   warning instead (apt stops and asks for the disc).
4. **Debian** — `Origin: Debian…` in the cached `InRelease`/`Release`, else every URI on
   Debian's hosts (`deb`, `security`, `ftp`, `ftp.<cc>`, `snapshot`, `httpredir`, the CDN).
   On a system with a system layer the Debian entries are read-only too.
5. **Third party** — everything else.

Origin is the Release file's `Origin`; last download = the oldest Release file mtime of its
suites. A file a package ships is never removable here (its entries may be disabled).

## 4. Checks → infos and status

Each finding has a level, a stable code (UI text `repo.i.<code>`), values and the
file/entry it is about. Status = the highest level (good / warning / critical).

| Code | Level | When |
|---|---|---|
| `parse_error` | critical | a file does not parse |
| `no_debian` | critical | no enabled `deb` entry of `<codename>` (no updates) |
| `no_security` | warning | no `<codename>-security` |
| `no_updates` | info | no `<codename>-updates` |
| `testing` | critical | `testing`, `unstable`, `sid`, `experimental`, or a codename two releases ahead |
| `use_codename` | warning | literal `stable` |
| `old_suite` | warning | `oldstable` or an older codename |
| `next_release` | info | the next codename (during an upgrade; not unsupported) |
| `mixed_suites` | warning | Debian entries of several releases |
| `security_suite` | warning | the old `<codename>/updates` layout |
| `security_uri` | warning | `-security` not on a `debian-security` archive |
| `proposed` | info | `-proposed-updates` enabled |
| `non_free_firmware` | info | `non-free` without `non-free-firmware` (Debian 12+) |
| `other_distro` | critical | another distribution's archive or a PPA |
| `insecure` | critical | `Trusted`/`Allow-Insecure`/`Allow-Weak`/`Allow-Downgrade-To-Insecure` |
| `no_signed_by` | warning | a third-party entry without `Signed-By` |
| `cdrom_enabled` | warning | an enabled installation disc entry (`cdrom:`): apt asks for the disc at every update |
| `key_missing` | warning | `Signed-By` names a file that does not exist |
| `duplicate` | warning | the same type/URI/suite/component twice |
| `legacy_keyring` | warning | keys in `/etc/apt/trusted.gpg(.d)` (Debian's own fragments excepted) |
| `key_changed`, `key_expired`, `no_pubkey`, `not_signed` | critical | from the last check |
| `fetch_failed`, `fetch_warning` | warning | other problems of the last check |

Disabled entries are not checked.

## 5. Unsupported configuration

The configuration is outside what rynas supports when an **enabled** entry is third-party,
or a critical check about the sources themselves fires (`testing`, `other_distro`,
`insecure`). `unsupported` lists them (origin or host, `Debian sid`…). It drives:

- a banner on this page and on System › Updates ("Unsupported configuration: packages from
  … are not tested with rynas; updates and support may break");
- the notification `repositories.unsupported` (firing, warning, category update, key
  `repositories:unsupported`, field `sources`) / `repositories.supported` (resolved),
  re-evaluated after every repositories task and by the daily updates check — the place
  where system warnings already reach the overview (notification center). No banner on
  other pages.

## 6. API (administrators only)

| Method and path | Body | Answer |
|---|---|---|
| `GET /repositories` | | `RepositoriesState` (files, entries, ignored, infos, status, unsupported, official, last_check, convertible, digest, running_task) |
| `POST /repositories/entries/toggle` | `ToggleRequest {digest, path, index, enabled}` | 202 task |
| `PUT /repositories/official` | `OfficialRequest {digest, mirror, security, updates, backports, proposed, components, sources, preview}` | 202 task, or 200 `RepoPreview` |
| `POST /repositories/modernize` | `{digest, preview}` | 202 task, or 200 `RepoPreview` |
| `POST /repositories/key-preview` | `{armored}` or `{url}` (https) | `KeyPreview {keys, armored}` |
| `POST /repositories/third-party` | `AddRepoRequest {digest, name, uris, suites, components, architectures, sources, key, fingerprints}` | 202 task |
| `DELETE /repositories/third-party/{file}?digest=` | | 202 task |
| `POST /repositories/check` | | 202 task (`updates.sources-check`) |
| `POST /repositories/mirror-test` | `{mirrors}` (≤ 5) | `MirrorTest` (fastest first) |

`/updates/backports` GET/PUT stays as it was (the old switch's API).

Errors: `conflict` (stale digest, nothing to change), `read_only` (rynas's, the system
layer's or a package's), `last_debian_entry` (disabling the last enabled main suite),
`not_available` (Debian section on a non-plain system), `key_invalid` (not a public key,
private key, fingerprints not the confirmed ones, every key expired), `already_exists`,
`invalid_value` with the field, `remote_unreachable` (key download).

## 7. Writes

Every write is an `updates.repositories` task (one update task at a time, in the package
work kinds of the scheduler):

1. the digest is checked again;
2. the files it touches are copied to `<state>/apt-backup/<UTC timestamp>/<path>`;
3. writes are atomic (`.rynas-new` then rename), 0644, no symlinks followed;
4. `apt-get update` runs in the rynas-apt unit with `LC_ALL=C`; its `Err:`/`W:`/`E:` lines
   are mapped (`aptlog.rs`) and kept as the last check
   (`<state>/repositories/check.json`), shown on the entries by URI and suite;
5. the unsupported notification is re-evaluated.

### Toggle

One-line: `# ` prefixed / removed; deb822: `Enabled: no` inserted before the first field,
or set on an existing `Enabled` line; enabling removes the line. Every other byte stays.
Refused on read-only entries and for the last enabled Debian main suite.

### Debian section

Plain Debian only (`ID=debian` with a codename) and never with a hardware module's system
layer — the backports switch's rule. Switches: security, updates, backports (rynas's
`rynas-backports.sources` as before, so the UPS/NUT flow keeps working), proposed updates
(off, warned). Components: `main` always, `contrib`, `non-free`, `non-free-firmware` as
visible switches with a one-line explanation each; turning `non-free` on also turns on
`non-free-firmware`; the same components go to every official suite (release, security,
updates, backports). Source packages switch (`deb-src`). Mirror: `deb.debian.org` (default)
or any http(s) mirror typed or chosen; "Test mirrors" fetches the Release file of
`deb.debian.org` and up to five more, in parallel. Each mirror's name is resolved first
(at most 3 s, so a slow or broken resolver shows as a DNS problem), then the request runs
on those addresses (at most 5 s), 9 s per mirror in all; a late mirror does not erase the
others' results. A failure is a code, `MirrorError {code, status?, detail?}` with `code`
one of `timeout`, `dns`, `connect`, `tls`, `http` (with `status`), `other`; the UI shows
`repo.me.<code>` and the English `detail` as a tooltip.

Saving shows a preview first. The task writes one deb822
`/etc/apt/sources.list.d/debian.sources` (marker comment, main + `-updates` +
`-proposed-updates` stanza, `-security` stanza, `Signed-By` Debian's keyring — `.pgp` on
Debian 13, `.gpg` before); stanzas of an old `debian.sources` that are not official stay
after it. Official entries elsewhere are disabled in place, or the file is renamed to
`.bak` when it held nothing else (as `apt modernize-sources` does). The main suite is always
written, so there is never zero Debian entries.

### Convert to the modern format

rynas's own converter (not `apt modernize-sources`: absent on Debian 12, interactive,
guesses `Signed-By`): each writable one-line file becomes `<name>.sources`
(`sources.list` → `debian.sources` when it holds only Debian entries and that file does
not exist, else `moved-from-main.sources`), the old file is renamed `.bak`. Entries with
the same state, URI, components and options share a stanza; suites with the same set of
types are kept together, so apt reads exactly the same targets. Options map to their
deb822 fields (`arch` → `Architectures`, `signed-by` → `Signed-By`, `arch+` →
`Architectures-Add`…); Debian entries without `Signed-By` get Debian's keyring.
Comments are kept.

### Third-party sources

- Add: name (`[a-z0-9][a-z0-9._-]*`, not a name rynas or Debian uses, no existing file),
  1–4 URIs (https preferred, http warned, `file:`/`cdrom:` and other distributions
  refused), suites or one exact path, components, architectures (default the system's),
  `deb-src` optional. The key is pasted (armored) or downloaded by rynasd over https
  (≤ 1 MiB); the preview shows fingerprints, user ids, creation and expiry, and the user
  confirms the fingerprints, which are sent back and must match. Only public-key packets
  are accepted (a private key or anything else is refused); the keyring is written
  armored as `/etc/apt/keyrings/<name>.asc`, the source as
  `/etc/apt/sources.list.d/<name>.sources` with `Signed-By:`. Never `apt-key`,
  `trusted.gpg.d`, `Trusted` or `Allow-Insecure`.
- If `apt-get update` then reports a problem for the new source (missing or bad key,
  unsigned, 404, network, expired Release), both files are removed again and the task fails
  with apt's message.
- Remove: a third-party file of `sources.list.d` no package ships; its keyring in
  `/etc/apt/keyrings` goes too when no other source names it. Copies kept.
- Disable: any third-party entry, from the entries table.

## 8. Page

Status card (pill, release, last check, findings with file/entry, "Convert to the modern
format" when there are one-line files); Debian (official) card (`#debian`) with the
switches and mirror, read-only with an explanation where rynas does not manage it; Sources
card: one block per file (path, format, package, remove), a table of entries (switch, type,
URIs, suites, components, origin and kind, last download and problems; read-only entries
with their reason and link), the ignored files; "Add repository" drawer; preview, confirm
and task-log dialogs. Tables wrap and stack, never scroll sideways inside the page.
Reached from the System hub, the command palette and System › Updates.

## 9. `--dry-system`

Files below the dry root, apt never run (the check records "no problems"), the mirror test
answers made-up times, nothing downloaded except a key explicitly requested by URL.

## 10. Phases

1. Done: everything above.
2. Later: `extrepo`'s curated repositories as a source for the add dialog (search, enable
   through `extrepo enable`); per-repository apt pinning (labels only, never above Debian);
   a release-upgrade assistant that drops backports and proposed-updates and checks
   third-party sources before changing the codename.
