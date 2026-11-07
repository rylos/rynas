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

MEMORY_PLACEHOLDER

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
