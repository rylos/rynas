# Research: managing apt repositories (2026-11-25)

What the page System › Software repositories (`docs/phase2/repositories.md`) is built on:
how Debian describes apt sources, how other systems present them, what breaks a Debian
system, and the pitfalls seen elsewhere.

## 1. Debian's own rules

### Formats

- `sources.list(5)` describes two formats. The **one-line** format
  (`deb [options] uri suite components…`, `/etc/apt/sources.list` and `*.list`) and the
  **deb822** format (`*.sources` stanzas with `Types`, `URIs`, `Suites`, `Components`).
  Debian 13 announces the one-line format as deprecated, removed "not before 2029".
  [sources.list(5), trixie](https://manpages.debian.org/trixie/apt/sources.list.5.en.html)
- deb822 fields: `Types`, `URIs`, `Suites`, `Components`, `Enabled: no`, `Architectures`
  (and `-Add`/`-Remove`), `Languages`, `Targets`, `PDiffs`, `By-Hash`, `Snapshot`,
  `Signed-By` (a file, a fingerprint, or an inline armored key), `Trusted`,
  `Allow-Insecure`, `Allow-Weak`, `Allow-Downgrade-To-Insecure`, `Check-Valid-Until`,
  `Valid-Until-Min/Max`, `Check-Date`, `Date-Max-Future`. A suite ending in `/` is an
  exact path and takes no components. Multi-value fields (several URIs or suites in one
  stanza) are normal and must be kept together by any writer.
- File names in `sources.list.d`: only letters, digits, `_`, `-`, `.`; other files are
  ignored (with a notice), backups (`~`, `.bak`, `.dpkg-old`…) silently
  (`Dir::Ignore-Files-Silently`, [apt.conf(5)](https://manpages.debian.org/trixie/apt/apt.conf.5.en.html)).
- Debian 13 installs a single `/etc/apt/sources.list.d/debian.sources`. apt 3.0 has
  `apt modernize-sources`, which converts one-line files to deb822 and keeps the old file
  as `.bak`; it warns when it cannot work out `Signed-By`.
  [Debian 13 release notes, §5.1](https://www.debian.org/releases/trixie/release-notes/issues.en.html)

### Suites and components

- Use the release **codename** (`trixie`), never `stable`: a literal `stable` silently
  jumps to the next release on release day.
  [SourcesList](https://wiki.debian.org/SourcesList)
- Security: `<codename>-security` on `security.debian.org/debian-security`
  (`deb.debian.org/debian-security` too); the old `<codename>/updates` layout ended with
  Debian 11. [Debian security FAQ](https://www.debian.org/security/faq)
- `-updates` brings urgent fixes before a point release; `-proposed-updates` holds what
  waits for the next point release and is for testing it.
  [StableUpdates](https://wiki.debian.org/StableUpdates),
  [StableProposedUpdates](https://wiki.debian.org/StableProposedUpdates)
- `non-free-firmware` exists since Debian 12 and is a default component there; using
  `non-free` without it misses the firmware.
  [Firmware](https://wiki.debian.org/Firmware)
- `deb.debian.org` is a CDN in front of Debian's mirrors and the recommended default;
  country mirrors are `ftp.<cc>.debian.org`. [deb.debian.org](https://deb.debian.org/)
- Backports are `NotAutomatic: yes` + `ButAutomaticUpgrades: yes` (apt pin 100): nothing
  comes from them unless asked (`-t <codename>-backports`), and packages taken from there
  follow their backports updates. Security support is best effort.
  [Backports](https://backports.debian.org/Instructions/)

### Third-party repositories

- Keys go to `/usr/share/keyrings` when a package ships them, else to
  `/etc/apt/keyrings`, and each source names its key with `Signed-By`. Never
  `apt-key` or `trusted.gpg.d` (a key there is trusted for every source).
  [DebianRepository/UseThirdParty](https://wiki.debian.org/DebianRepository/UseThirdParty)
- `extrepo` is Debian's curated list of external repositories (search/enable/disable,
  GPG-checked index, `main` only by default).
  [extrepo](https://manpages.debian.org/trixie/extrepo/extrepo.1p.en.html)

### What breaks Debian

- Mixing stable with testing/unstable, adding another distribution's archives or
  personal package archives (PPAs), repositories that replace Debian's packages, and
  over-using backports. Before a release upgrade: drop backports and proposed-updates and
  check every third-party source.
  [DontBreakDebian](https://wiki.debian.org/DontBreakDebian)

### Detecting broken repositories

- `apt-get update` exits 100 on errors (`--error-on=any` also on warnings); its
  status-fd does not say which source failed. The `Err:<n> <uri> <suite> <file>` blocks and
  the `W:`/`E:` lines, with `LC_ALL=C`, name the source: `NO_PUBKEY`, `EXPKEYSIG`/
  `KEYEXPIRED`, `BADSIG`, "is not signed", "Release file … is expired", 404 or "does not
  have a Release file", name resolution and connection errors, "changed its 'Origin'
  value", "configured multiple times".
  [apt-get(8)](https://manpages.debian.org/trixie/apt/apt-get.8.en.html),
  [apt-secure(8)](https://manpages.debian.org/trixie/apt/apt-secure.8.en.html)
- What a source is: the cached `InRelease`/`Release` in `/var/lib/apt/lists` (`Origin`,
  `Label`, `Suite`, `Codename`, `Date`, `Valid-Until`), named by apt's `URItoFileName`
  (scheme and credentials dropped, some characters `%xx`, `/` → `_`).

## 2. How other systems do it

- **A virtualization platform's repository panel** (the model chosen; read from its
  open-source apt library): parses `sources.list` and every `.list`/`.sources`, one error
  per file that never blocks the others, a digest per file and a combined digest sorted by
  path that every write carries (stale edits refused). The release comes from
  `VERSION_CODENAME`. Suite checks: old suite, "should not be used in production" for
  testing/unstable/newer, literal `stable` → "use the codename", security suite must be
  `<codename>-security`, the next release's codename ignored during an upgrade. Origin per
  repository from the cached Release file (fallback by host). Enable/disable comments a
  one-line entry or sets `Enabled` in deb822, keeping comments and options. UI: a grid
  grouped by file (Enabled, Types, URIs, Suites, Components, Options, origin badge,
  comment) and a status summary (critical: parse errors or no product repository;
  warnings: misconfigured or mixed suites).
- **A Debian-based NAS distribution**: adds the base suite and `-updates` when missing,
  a mirror list with fallback, proposed and partner sources as switches, and kernel
  backports pinned so that only kernel and firmware packages come from backports.
- **A web administration panel**: its repository tab is empty on deb822 systems
  ("not supported yet") — a failure to avoid: both formats from the start.
- **A server web console with a package backend**: enables or disables known
  repositories only.
- **A desktop distribution's "software sources" tool**: a "select best server" test
  (ping, then a download test on the top five plus two random mirrors); known hangs, and
  it once left systems with no active entries — tests run in the background with a time
  limit, and no change may leave zero Debian entries.
- **An enterprise distribution's installer/config tool**: priorities, per-repository
  refresh, key import on add; when a repository's key changes it disables the repository
  until the cause is known.
- **A community desktop distribution**: a curated set of third-party repositories,
  shipped disabled, enabled by one explicit choice.

## 3. Pitfalls to avoid

- Support both formats; keep multi-value stanzas, comments and unknown fields byte for
  byte on every rewrite.
- Never `apt-key`, `trusted.gpg.d`, `Trusted: yes` or `Allow-Insecure` from the UI.
- A changed key (`BADSIG`/`EXPKEYSIG`) is a critical finding; rynas shows it and leaves
  Debian's own entries enabled (a stale key on Debian's archive is a local problem).
- `LC_ALL=C` for every apt output that is parsed.
- Respect apt's filename rules for every file rynas writes.
- Mirror tests in the background, with a hard time limit.
- Never leave the system without an enabled Debian main suite.

## 4. Decisions taken from this

Recorded in `docs/phase2/repositories.md`: the virtualization platform's model as the
base (files, digest, per-file errors, origin from Release files, classification, checks),
plus Debian 13's single `debian.sources`, third-party sources only with their own key in
`/etc/apt/keyrings` and `Signed-By`, an "unsupported configuration" flag for anything
outside Debian and rynas's own sources, and rynas's own deb822 converter instead of
`apt modernize-sources` (absent on Debian 12, interactive, guesses `Signed-By`).
