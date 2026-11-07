# Review 2026-11-07 — web access W2 backend (domains, DNS-01, records), A3 catalogue, packaging

Scope: `git diff 1626853..main -- . ':!crates/rynas-ui'` (`crates/rynas-dns`,
`rynasd/src/web_access/{domains,certs,avahi,caddy,mod}.rs`, `rynas-docker/src/install.rs`,
`rynasd/src/docker.rs`, `rynas-services` Samba mDNS restart, 13 new `apps/`, manifest format,
`packaging/caddy`). Read-only, time-boxed (~35 min). References are `file:line` on `main` at
648ae0a. No real DNS provider, ACME CA, host Docker or system file was contacted or touched.

Previous review (`review-2026-11-07-docker-apps-backend.md`): H1-H3, M1-M8 still fixed in the
code they covered. The **M5 pattern regressed** in new code (H1 below).

## High

**H1. DNS secret files: path-based root writes and recursive deletes below the engine's
writable folder (M5 regressed, worse).**
`web_access/mod.rs:697-704` checks `rynas/` through a descriptor (`rynas_dir`), then calls
`domains::write_engine_secrets` with the **path** `/var/lib/rynas-caddy/rynas/dns`
(`mod.rs:86`). `domains.rs:261-310` then uses `create_dir_all`, `set_permissions(0o750)`,
`chown(0, rynas-caddy)`, `read_dir` + `remove_file` and **`remove_dir_all`** on that path.
`/var/lib/rynas-caddy` is `0750 rynas-caddy` (`rynas-caddy.service:44-45`). The engine user can
rename the root-owned `rynas/` inside its own folder: a same-parent rename of a directory needs
no write permission on the directory itself. It can then create its own `rynas/` with
`dns -> /`. `rynas_dir` only re-owns the folder itself (`mod.rs:721-724`), and the planted
`dns` link inside it survives. The next sync (every 30 s; the last loop runs even with no
credentials, `domains.rs:302`) would then:
- `chmod 0750` and `chown root:rynas-caddy` on `/`;
- `remove_dir_all` every top-level folder that is not a credential id, until the first plain
  file.

So a compromised Caddy, the internet-facing process, can wipe the host as root. The comment at
`mod.rs:695` ("root-owned `rynas/`, so plain paths are safe there") does not hold.
*Fix:* do it all on descriptors. Use `rynas_dir()` → `ensure_dir_at(&rynas, "dns")` →
`ensure_dir_at(&dns, id)` → `safefs::write_at` for the files. Delete with `unlinkat`, plus a
descriptor-based recursive delete that opens with `O_NOFOLLOW` and never crosses a symlink.
In `rynas_dir`, refuse or replace an existing `rynas/` whose `st_uid != 0`: rename it aside and
make a fresh one, so nothing planted inside is trusted. Add a test with a planted `dns -> <tmp>`
link that the delete pass must leave alone.

**H2. "Records rynas did not write are never touched" does not hold: existing records are
overwritten, adopted and later deleted.**
`rynas-dns/src/plan.rs:144-152` turns any existing set with other values into `Update`. A set
with the same values becomes `Keep`. `mod.rs:1836-1857` then records both as **owned**. When
the domain is removed, changes provider or turns `lan_record` off, `removal_changes` /
`stale` (`mod.rs:1653`, `1724`) **delete** them. Example: a user whose `nas.example.com A` holds
the public IP (DDNS, VPN) enables the domain. rynas silently points it at the LAN address, and
removing the domain later deletes the record altogether. DuckDNS has no read API, so every sync
overwrites its one public record (`duckdns.rs:66-85`). This contradicts docker-apps.md §5.4.
`Owned` stores only `(name, type)`, not the values written, so a record the user edited after
rynas wrote it is deleted too.
*Fix:* a set rynas does not own that exists with other values plans as a new action `conflict`
(or `manual`), with the current values. It is written only after explicit consent (a
per-domain `take_over: [names]`, or a confirm in the plan). Store the values written in `Owned`.
Delete a set only when the provider's current values equal what rynas wrote; otherwise drop
ownership and report it. A pre-existing `Keep` set is not adopted. For DuckDNS, say plainly in
the form and the plan that its single record will hold the LAN address.

## Medium

**M1. The DNS state lock is held across provider HTTP calls.** `sync_records` takes
`dns_lock()` (`mod.rs:1787`) and keeps it through every `plan`/`apply` call (20 s timeout per
request, several requests per domain, no overall deadline). `domain_views` (`mod.rs:1924`,
called by `GET /web-access`) and `diagnose_domain` (`mod.rs:2114`) wait on that same mutex, so
the Web access page hangs for as long as a provider is slow, and one blocking-pool thread is
held for each request.
*Fix:* clone `owned` under the lock, run the I/O unlocked, then merge the result under the lock.
Keep a separate `sync_in_progress` guard so two syncs never interleave.

**M2. `apt-get install -f -y` in the repair can remove packages.** `install.rs:151-165`:
`install -f` may resolve broken dependencies by removing packages, and `-y` accepts that
(unrelated packages on a half-broken system included). *Fix:* add `--no-remove` (apt aborts
when it would remove). Run the repair only when the broken packages are Docker's or the
engine's, and log `apt-get -s install -f` first.

**M3. Cancelling or timing out an apt step does not stop apt.** `outside()` (`install.rs:111-137`)
runs `systemd-run --wait --pipe`. `Cmd` kills only the client's process group on cancel or
timeout (`rynas-tasks/src/cmd.rs:186-200`). The transient unit keeps running, or dies of
SIGPIPE midway (that is the `iU` state the repair then has to fix). A retried install then hits
the dpkg lock. *Fix:* on cancel or timeout, `systemctl stop <unit>` (the unit name is already
unique). Pass `-o DPkg::Lock::Timeout=120`. Consider `--property=RuntimeMaxSec=` equal to
`APT_TIMEOUT`.

**M4. Literal secrets in the engine config for DigitalOcean and RFC 2136.** `rynas-dns/src/lib.rs:215-221`
(`placeholder: false`): `auth_token` and the TSIG `key` go into the JSON posted to Caddy and into
`last-good.json` (`mod.rs:728-742`, `0640 root:rynas-caddy`). Caddy has them anyway, but they
also travel in every load body, and any Caddy error that echoes the config reaches
`engine.last_error`, the logs and task logs unredacted (only `plan` redacts, `mod.rs:637-652`).
*Fix:* redact every known credential value (`rynas_dns::redact`) in engine error texts before
storing or logging them. Verify placeholder support upstream (libdns/digitalocean reads
`auth_token` once in `Provision`) or document the two providers as "secret in the engine
config".

**M5. Avahi signal match rules accept any sender.** `avahi.rs:456-465`: the `EntryGroup` and
`Server` `StateChanged` rules have no `.sender(AVAHI)`. Group paths are predictable
(`/Client<n>/EntryGroup<m>`). Any local process can emit a `COLLISION` signal and take a name
down, and collided names are never retried (`avahi.rs:268-275`). It can also send fake server
states to cause withdraw/republish churn. *Fix:* add `.sender(AVAHI)` to both rules (the bus
matches the well-known name's owner). In the handler, also compare `m.header().sender()` with
the current owner from `NameOwnerChanged`.

**M6. Plans hit the providers on every call, with no rate-limit handling.** `POST /web-access/plan`
reads every domain's zone and each set (`plan.rs:126-176`), and the UI calls it on every
review. deSEC and Porkbun throttle hard (deSEC answers 429 with `Retry-After`), and a 429 shows up
as `unknown`/`failed`. *Fix:* cache `zone()` per (provider, credential, domain) for 10 min and
plan reads for 30 s. On 429, honour `Retry-After` once (≤ 10 s) in `Http::send`, otherwise give
`DnsError::RateLimited`, which the retry loop backs off on.

**M7. The Samba restart ignores active clients.** `shares.rs:144-170`, `935-942`: the first shares
apply after the upgrade restarts smbd whenever it started before the marker. The marker is
written at that very moment, so on an upgraded system that is always true. Any open copy or
Time Machine backup is cut. It happens only once (no storm), but without warning. *Fix:* defer
while `smbstatus -j` (or `-b`) shows sessions with open files, and retry on later applies or at
the next idle check. Log "restart deferred: N clients connected".

## Low

- **L1.** `rebind::verdict` (`rebind.rs`, the `_ if expected.iter().any(is_private)` arm) reports
  **SERVFAIL** and other rcodes from the LAN resolver as `RebindBlocked`. A DNSSEC or upstream
  failure then gets the router fix text. Only `NxDomain | NoData | Refused | 0.0.0.0` should mean
  rebind; `ServFail`/`Other` → `LanUnreachable`.
- **L2.** Certificate notification state (`certs.rs:124`, `Notified`) and `expected_since` live
  in memory only. After a rynasd restart every ongoing problem fires again, and the 15-minute
  "failed" timer restarts. Persist `{id: status}` in `<state>/web-access/cert-notified.json`, or
  rely on the notify engine's own key dedup (check that `firing` with the same key and severity
  is a no-op there).
- **L3.** `certificates()` (`mod.rs:2016`) runs `journalctl -n 2000 --since -24h` on every
  `GET /web-access/certificates` and every domain test. Cache it for 60 s, or keep a journal
  cursor.
- **L4.** Mosquitto (`apps/mosquitto/compose.yaml:22`): `mosquitto_passwd -b … "$MQTT_PASSWORD"`
  puts the password in argv. Every host user can read it in `/proc/<pid>/cmdline` at each start.
  Write `user:password` with `umask 077`, then `mosquitto_passwd -U passwd`.
- **L5.** Manifest rule (`rynas-apps/src/manifest.rs`, `reverse_proxy` block): a `host_network`
  app may declare 80/443 without `reverse_proxy`. It then binds the proxy's ports with no
  external-mode gate. Apply the same rule to host-network apps.
- **L6.** `Creds` (`BTreeMap<String,String>`) and the opened values are never zeroized
  (`domains.rs:137`, `mod.rs` `cred_values` on every render and every `GET` through
  `domain_views` → `open_creds`). `zeroize::Zeroizing<String>` is cheap. `domain_views` should
  check that the envelope opens without keeping the values.
- **L7.** AdGuard Home publishes 53/tcp+udp on every interface (`apps/adguard-home/compose.yaml:19-20`).
  On a NAS with a public address, that makes an open resolver. Add a note, or a `DNS_BIND`
  defaulting to the LAN address.
- **L8.** `GET /v1/config/revisions/{n}` (`ops.rs:188`) returns the `web_access` document with
  `sealed`. It is only an envelope, but §5.4 says answers never carry `sealed`. Strip it in that
  route too, or document the exception.

Checked and fine: `Http` uses native-tls with verification on, no redirects, 8 s connect and
20 s total timeouts, a 4 MiB body cap, URLs stripped from errors and the credential values
redacted. Plans never write: `plan_records` reads only, and `seal_input` stays in memory.
Credentials are write-only (`for_answer` on `applied`/`pending`), the audit logs ids only, and
`strip_input` stops a client from planting `sealed`. Names are ASCII/IDN `xn--`. Zone candidates
use exact-name provider lookups. TTL = max(300, zone minimum). The ACME staging switch selects
the directory and storage folder consistently (`domains.rs:203-237`). The Avahi publisher uses
one group per name, does not loop on collisions, backs off on reconnect and degrades cleanly
without Avahi. All 13 new images are pinned by digest, none are privileged, and `devices` is gated
by `security.devices` or a GPU overlay. The Zigbee adapter path pattern is anchored. The Frigate
port 5000 is not published. The NPM 80/443 install is gated by external proxy mode.

## Memory and size

Release build from the shared `target/` (built 20:07; it may already include 359de95, because a fix wave ran meanwhile), `FAKE_DOCKER=1 FAKE_CADDY=1
tools/ui-audit/start.sh 18677`, idle 40 s, `/proc/<pid>/status`:

| Process | VmRSS | RssAnon | RssFile | Budget |
|---|---|---|---|---|
| rynasd | **12.9 MB** | 3.0 MB | 9.9 MB | < 20 MB: ok |
| rynas-web | **6.3 MB** | 2.7 MB | 3.7 MB | < 20 MB: ok |

The anonymous memory is the same as in the last wave (2.8 MB). The drop from 19.7 MB comes from
the file-backed side. A freshly linked binary is not yet in large page-cache folios, so this
number is a lower bound: count about 7 MB of variance in RssFile, and still under budget.
Binary sizes: `rynasd` 15.8 → **17.05 MB** (+1.25 MB: rynas-dns, zbus for Avahi, W2 code;
reqwest/native-tls were already linked through rynas-notify). `rynas-web` 4.93 → **4.66 MB**.

## Five most valuable missing tests

1. `write_engine_secrets` with a planted `rynas/` owned by the engine and a `dns -> <tmpdir>`
   link: nothing outside is chmodded, chowned or deleted (H1).
2. Record sync against a fake provider holding a pre-existing `A nas.example.com 203.0.113.5`:
   the plan shows `conflict`/`manual`, the apply leaves it, and removing the domain deletes
   nothing rynas did not write (H2). Also a set the user edited after rynas wrote it.
3. `GET /web-access` answers within 1 s while a `sync_records` against a fake provider that
   sleeps 15 s is running (M1).
4. The avahi session ignores an `EntryGroup.StateChanged` COLLISION from a sender other than
   Avahi's owner (M5), using a private dbus-daemon in the test.
5. The Docker install task cancelled during `apt`: `systemctl stop rynas-apt-docker-*` is run,
   and `repair_cmds` include `--no-remove` (M2, M3), using the command recorder.

## Fix notes (2026-11-07)

- **H1 fixed.** `safefs` gained `list_at` (entries through `openat(dir, ".")`),
  `remove_tree_at` (`unlinkat`, folders opened `O_DIRECTORY|O_NOFOLLOW` one at a time, never
  crosses a symlink, depth-capped) and `rename_at`. `write_engine_secrets` now takes the open
  `rynas/` descriptor and does everything below it with `ensure_dir_at`/`set_owner_mode`/
  `read_at`/`write_at`/`remove_tree_at`. `rynas_dir` moves an existing `rynas` aside
  (`rynas.untrusted-<ns>`) when it is a symlink, not a folder, or not owned by rynasd's uid,
  then makes a fresh one; the cleanup pass with no credentials only opens an existing,
  trusted `rynas/`. Tests: `domains::engine_secrets_never_follow_planted_links` (planted
  `dns -> <tmp>` and links inside), `safefs::tree_removal_never_crosses_symlinks`,
  `web_access::planted_rynas_folder_is_moved_aside`. A uid-mismatch case needs root and is
  not tested (the symlink case goes through the same branch).
- **H2 fixed.** New `DnsAction::Conflict` and `WebDomain::take_over` (API change, §5.4).
  `Owned` stores the values written (`OwnedSet {name, rtype, values}`; the older `[name, type]`
  form still loads, with values unknown → never deleted). `plan()` takes the owned sets and
  `take_over`: a foreign set with other values is `conflict`; an owned one is updated only
  while it still holds what rynas wrote; a pre-existing `keep` is not adopted; an owned set
  no longer wanted is deleted only when the provider still holds exactly the written values,
  otherwise `conflict` (left in place, ownership dropped). DuckDNS: plan `detail` and the
  provider's `token_hint` say plainly that its one record is overwritten with the LAN
  address; a stale DuckDNS record is left as is (cannot be read, so it is not deleted).
  `DomainView.problems` gains `records_conflict`. Tests:
  `plan::foreign_records_are_conflicts`, `web_access::foreign_records_are_never_overwritten_or_deleted`.
- **M1 fixed.** `sync_records` holds a separate `dns_sync` guard for its whole run and takes
  `dns_mem` only to copy `owned` and to merge (only syncs change it). `domain_views` copies
  the reports and opens credentials outside the lock. Test:
  `web_access::pages_answer_during_a_slow_record_sync` (3 s per read, GET < 1 s).
- **M2 fixed.** `--no-remove`, and `apt-get -s install -f` logged before the fix. The repair
  already ran only for Docker's or the engine's packages (`install_facts` queries only those),
  so that part needed no change.
- **M3 fixed.** `install::on_abort` gives `systemctl stop <unit>.service` for a transient
  `rynas-apt-docker-*` unit; `DockerSvc::run` runs it on `Cancelled`/`Timeout`. Every apt call
  passes `-o DPkg::Lock::Timeout=120`; units carry `RuntimeMaxSec=1800`. The cancel path itself
  is not driven end to end (the command recorder returns at once); `on_abort` and the
  `--no-remove` line are tested in `install::repair_and_resume` and `tests/docker.rs`.
- **M4 fixed (redaction) / documented.** Engine load errors are stored and logged with every
  secret credential value replaced by `***`. Placeholder support upstream was not re-checked
  (no network in this pass): DigitalOcean and RFC 2136 are documented in §5.4 as "secret in
  the engine configuration".
- **M5 fixed.** `.sender(AVAHI)` on both rules (the bus resolves the owner; zbus skips
  well-known senders client-side), and every message's sender is compared with the owner
  from `GetNameOwner`/`NameOwnerChanged` (`from_owner`, unit-tested). The private
  dbus-daemon test was not added (no dbus-daemon is assumed on CI hosts).
- **M6 fixed.** `rynas_dns::cache`: zone lookups 10 min, record reads 30 s for plans only (a
  sync that writes reads fresh, so conflict checks never use an old value), every write drops
  the credential's reads; keyed by provider, credential id and a hash of the envelope.
  `Http::send` waits once for a `Retry-After` ≤ 10 s, else `DnsError::RateLimited` (the
  10-minute record retry backs off). Tests: `cache::plans_share_reads_syncs_read_fresh`,
  `http::throttling_waits_once_then_gives_rate_limited`.
- **M7 fixed.** `ShareHost::smb_sessions` (`smbstatus -b -j`); with sessions the one-time
  restart is deferred (reload, log line) to a later apply. Any session counts, not only those
  with open files: a connected Time Machine client holds a session between writes too.
- **L1 fixed.** `ServFail`/`Other` → `LanUnreachable`, with a detail naming SERVFAIL.
- **L2 not changed.** The notify engine keeps per-key state in SQLite: a `firing` with the same
  key and severity while open only bumps the count, and `resolved` is ignored when nothing
  fires, so a restart does not deliver ongoing problems again. The 15-minute "failed" timer
  restarting after a rynasd restart only delays that status; not worth a state file.
- **L3 fixed.** Parsed engine errors cached 60 s (`engine_log_ttl`; the raw log is not kept).
- **L4 fixed.** `printf` builtin + `mosquitto_passwd -U`.
- **L5 fixed.** The 80/443 rule applies to host-network apps too (catalogue test added).
- **L6 fixed (partly).** `zeroize` wipes opened values after a record client is built and in
  the new `creds_open` check used by `domain_views`. `Creds` stays a plain map (the render
  path and `SecretBox::open_sealed` still hold plain copies briefly).
- **L7 fixed (note).** The AdGuard Home description warns about an open resolver; no
  `DNS_BIND` default (the LAN address is not known when the catalogue renders).
- **L8 fixed.** `GET /config/revisions/{n}` and `GET /config/pending?detail=1` replace every
  `sealed` value with `***` (test in `credentials_are_sealed_and_never_returned`).
