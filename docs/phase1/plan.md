# rynas — Phase 1: plan

Started on 2026-11-04. The decisions still marked "proposal" in `../phase0/05-decisions.md` are adopted
as recommended (maintainer: "go ahead"); they are revisited only if a step proves them wrong.

Primary language: English (code, docs, default UI language). Italian is secondary; more languages
will follow later.

## UI requirement (maintainer, 2026-11-04)
The interface must be **modern, appealing and very fast**. Translated into measurable constraints:

| Budget | Target |
|---|---|
| UI wasm (brotli) | ≤ 1 500 KiB for the whole built-in UI; first paint still < 300 ms on LAN. Raised from 350 KiB, then 900 KiB, on 2026-11-05, to 1 500 KiB on 2026-11-07 (Marco): one bundle is simpler and still small next to DSM/TrueNAS; lazy pages (plugin loader) only if it grows past this |
| CSS + critical fonts (brotli) | ≤ 60 KB |
| First paint on LAN | < 300 ms; no white flash (theme applied inline in `<head>`) |
| Fully interactive on LAN | < 800 ms |
| Page change | < 50 ms perceived (no spinner for already cached data) |
| Live metrics update | 1 s via SSE, 60 fps, no full-page reflow |
| Idle backend | < 20 MB RSS, < 1% CPU on J4125 |

Style: clean surfaces, Inter typography, short micro-animations (120–200 ms, honoring
`prefers-reduced-motion`), smooth live charts, glass/blur only where it is cheap (appbar), Fusion palette with
saturated accents on a calm background, states always shown with icon + text.

## Priorities
Goal of phase 1: **replace DSM on the reference DS920+** (Celeron J4125, system on two NVMe in RAID1, data on
HDDs) for everyday use. Order of work, each item ending with tests, a real run and a commit:

| P | Area | Why first | Decisions |
|---|---|---|---|
| P0 | Foundation: login, sessions, `.deb`, HTTPS on `:8443` with a local CA (W0, done), config store with pending/apply/revisions, task engine (UPID, logs, cancel) | everything else builds on it | D-PRIV, D-CFG, D-CFGBAK |
| P1 | Storage: disks + SMART, mdadm and btrfs status and creation wizard, scrub, per-disk-type defaults | the NAS exists to store data | D-STO, D-SYSTUNE |
| P2 | Users & groups (local) + SMB/NFS shares with POSIX ACL matrix, `#recycle`, previous versions | the main daily use | D-IDENTITY, D-FILESVC |
| P3 | Snapshots (btrbk engine, GFS, browse/restore) + task scheduler + notifications (SMTP/ntfy/Telegram) | protect the data, know when something breaks | D-SNAP, D-TASKS, D-NOTIFY |
| P4 | Safe networking (networkd, timed confirmation, boot guard, rescue paths) + synology plugin (hardware, reset gesture) | headless NAS must never be locked out | D-NET, D-HW |
| P5 | File manager (SynoFile port) | DSM File Station replacement | D-FM |
| P6 | Updates page, UPS widget over synodeb-ups, maintenance/tweaks page, config export | operations | D-UPDATES, D-UPS, D-SYSTUNE |
| Phase 2 | Docker plugin, backup plugin (restic-format repos), apps & plugins store, web access (Caddy), Tailscale, file-services plugin, dedup, firewall, encryption key options | | see decision log |

## Steps
| # | Step | Area | Status |
|---|---|---|---|
| 1 | Workspace skeleton, `rynas-api-types`, API v1 contracts (system, metrics, storage, hardware) | coordinator | done |
| 2 | `rynas-web`: axum, /proc /sys metrics collector, ring buffer, SSE, storage, hardware (synohwd) | backend | done |
| 3 | `rynas-ui`: Leptos CSR shell, Fusion design system, layout A, live dashboard, command palette, task panel, customizable dashboard, notifications | frontend/UI | done |
| 4 | PAM + TOTP login, sessions, CSRF; root `rynasd` via Unix socket | backend/security | done (backend); login screen in progress |
| 5 | `.deb` package, systemd units, postinst, test in Debian trixie container | packaging | done |
| 6 | `rynas-config` (pending/apply/rollback, revisions, anti-drift) + `rynas-tasks` (UPID tasks) | backend | done (backend, `system.hostname` demo domain; UI: global pending bar with review/apply/discard, task panel with live log and cancel, System → Configuration with revisions and drift) |
| 7 | storage (disks, SMART, mdadm, btrfs, snapshots, scrub) | storage | read side, planner and write side done (plans executed as tasks, LUKS unlock/lock, subvolumes, disk settings; loop-device container test); UI pages missing |
| 8 | users, shares-smb, shares-nfs | services | done (backend: `rynas-services`, `users` and `shares` domains, API, container test; UI: Users & Groups, Shared Folders with permissions matrix and effective permissions, File Services pages) |
| 9 | Plugin protocol v1, `SchemaRenderer`, synology plugin | plugins | synology backend done as a built-in provider (`rynas-hw`, `rynas-hw-synology`) + power schedule (`rynas-power`), API below, container test; UI: Hardware › Synology and Hardware › Power schedule; plugin protocol missing |
| 10 | files (SynoFile port) | file manager | backend done (`rynas-files` core, `rynas-files-worker` as the user, transfers as rynas tasks, `/api/v1/files/*` streamed through rynas-web, container test `packaging/test-files.sh`); spec and route table: `../plugins/files.md`; UI: Web Component port |
| 12 | Task Scheduler (systemd units) + snapshots (btrbk engine, browse/restore/rollback, system restore points), P3 | data protection | done (backend: `rynas-scheduler`, `rynas-btrfs`, `rynasctl`, API, container test; UI pages missing; run notifications through `rynas-notify`) |
| 11 | networking, notifications, apt updates; aptly repo | network / ops | notifications: backend done (`rynas-notify`, API below; UI pages missing); networking (P4, phase 1 scope of D-NET): backend done (`rynas-net`, API below, boot guard, watchdog, reset button, ifupdown adoption, container test; UI: Network page `crates/rynas-ui/src/pages/network/` with the timed-confirmation banner, revisions, rescue, diagnostics) |

## Error codes (every endpoint)
Every non-2xx answer under `/api` is `application/json` with an `ApiError {code, message, field?}`
body, also for unknown paths, 405, 413, 415 and query-string rejections (framework rejections are
wrapped by a middleware in both daemons). The codes are the constants of
`rynas_api_types::error_codes` (a test fails on code literals elsewhere); `message` is English
and never parsed by the UI. `field` (omitted when unknown) names the request field a
`400 invalid_value` is about, as in the JSON body (`name`, `email`, `expires`, `password`,
`workgroup`, `permissions`, `smb.veto_files`, `nfs.rules`, `addresses`, `gateway`, `label`,
`passphrase`…): the UI shows the translated code under that form field instead of in the form
banner. Domain validators return `rynas_config::Invalid {field, message}` (`Invalid::at`),
rynasd handlers `Error::invalid(field, msg)`; rynas-web passes the body through unchanged.
JSON bodies and framework rejections are mapped in one place for both daemons (`rynas-http`):
the field comes from the deserializer's path (`smb.veto_files`, `members[2]`) and the message is
a short English sentence; serde's own text ("invalid type: …, expected … at line 1 column 9")
only goes to the debug log (A13).

```json
{"code": "invalid_value", "message": "user bob: \"x\" is not an email address", "field": "email"}
```

| Status | Code | Meaning |
|---|---|---|
| 400 | `bad_request` | malformed request: query string (`field` = the parameter), path |
| 400 | `invalid_json` | the body is not JSON (syntax error, cut short) |
| 400 | `missing_field` | a required field of the body is missing (`field`) |
| 400 | `unknown_field` | a field the endpoint does not accept (`field`) |
| 400 | `invalid_type` | a field of the wrong JSON type, e.g. a string for a number (`field`) |
| 400 | `invalid_value` | a value outside the accepted set or range, or refused by a domain rule (`field` when known) |
| 400 | `confirmation_required` | destructive request without its confirmation |
| 400/401 | `invalid_totp` | wrong TOTP or recovery code (401 at login) |
| 401 | `unauthenticated` | no valid session (also unknown `/api/**` paths without one) |
| 401 | `invalid_credentials`, `totp_required` | login |
| 403 | `csrf`, `forbidden` | missing CSRF header; no role |
| 404 | `not_found` | no such object or endpoint |
| 405 | `method_not_allowed` | known path, wrong method (`Allow` header kept) |
| 409 | `conflict`, `busy`, `nothing_pending`, `apply_group_split`, `task_finished`, `already_exists`, `not_applied`, `totp_already_enabled`, `totp_not_enrolled` | state conflicts |
| 413 | `payload_too_large` | body over the limit (1 MiB unless documented) |
| 415 | `unsupported_media_type` | JSON endpoint without a JSON body |
| 429 | `rate_limited` | with `Retry-After` |
| 500 | `internal` | unexpected failure, details in the log |
| 502 | `bad_gateway` | rynasd answered something rynas-web cannot read |
| 503 | `backend_unavailable` | rynasd or a provider (hardware) unreachable; the message names it |
| 503 | `auth_unavailable` | no authentication backend reachable (login, TOTP) |
| 504 | `timeout` | rynasd did not answer within the route's deadline (auth 15 s, storage 90 s, file manager 130 s / uploads 610 s to the answer head, others 30 s) |
| 403 | `permission_denied` | file manager: the filesystem refused (the user's permissions, ACLs) |
| 507 | `insufficient_storage` | file manager: volume full or quota used up |
| 409 | `read_only` | file manager: inside an archive, a read-only snapshot or filesystem |
| 400 | `share_root` | file manager: a shared folder itself renamed, copied, moved or deleted |
| 403 | `outside_share` | file manager: a symbolic link leads out of its shared folder (listed, never followed) |
| 400 | `password_required` | file manager: the archive needs its password |
| 503 | `tool_missing` | file manager: an optional program (7z, unrar, tar) is not installed |
| 502 | `remote_unreachable` | file manager: an SMB/SFTP server cannot be reached |
| 400 | `remote_auth_failed` | file manager: a remote server refused the login (never 401) |
| 409 | `host_key_changed` | file manager: an SFTP server key differs from the pinned one |
| 501 | `not_available` | the feature is not available on this system (eject without the removable-drives module) |
| 409 | `docker_not_installed` | Docker: the engine is not installed (`../phase2/docker-apps.md` §5.1) |
| 503 | `docker_unavailable` | Docker: installed, but the daemon does not answer on its socket |
| 409 | `containers_running` | Docker: refused while containers run (moving the engine) |
| 409 | `tailscale_not_installed` | Tailscale: not installed (`../phase2/remote-access.md`) |
| 409 | `stack_not_managed` | Docker: a stack outside the rynas stacks folder (Portainer's, untracked): no file edits, no compose actions |

## API v1 (steps 2–3)
| Method | Path | Response |
|---|---|---|
| GET | `/api/v1/system` | `SystemInfo` |
| GET | `/api/v1/metrics/history?minutes=N` | `Vec<MetricsSample>` (max 60 min, 1 sample/s, downsampled if needed) ; `MetricsSample.zram` (initialized `/sys/block/zram*`: name, algorithm, disksize, data, compressed, mem_used, huge_pages from `mm_stat`/`comp_algorithm`, D-ZRAM) is sent only when there is zram and, in the history, only on the newest point |
| GET | `/api/v1/metrics/stream` | SSE, `sample` event with a `MetricsSample` every second |
| GET | `/api/v1/events` | SSE, one stream per tab: `metrics.sample` (`MetricsSample`), `task.started` / `task.updated` / `task.finished` (`TaskEvent`), `notification` (`NotificationEvent`, since P3), `hardware.status` (`HardwareStatus`: the latest on connect, then on change — `age_s` ignored — and at least every 60 s; replaces polling `/hardware`), `config.pending` (`PendingEvent` {generation}: some changeset may have changed — stage, discard, apply — re-read `/config/pending` with `If-None-Match`; replaces polling it), `resync` {"stream": "metrics"\|"tasks"\|"config"\|"notifications"\|"hardware"} after missed events (re-read that collection). Capped at 6 streams per session (`/events`, `/metrics/stream`, `/tasks/stream` together; a new one closes the session's oldest) and 128 in total (then `429 rate_limited` + `Retry-After`) |
| GET | `/api/v1/storage` | `StorageOverview` |
| GET | `/api/v1/hardware` | `HardwareStatus` (empty if there is no provider) |

Types in `crates/rynas-api-types`. Development: `cargo run -p rynas-web -- --listen 0.0.0.0:8080`
(UI served from `crates/rynas-ui/dist`, embedded in the binary in release builds).

Since step 4 every endpoint above requires a session (401 `unauthenticated` without one).

## API v1: authentication (step 4)
| Method | Path | Body | Response |
|---|---|---|---|
| POST | `/api/v1/auth/login` | `LoginRequest` {username, password, totp?} | `Session` + `rynas_session` cookie |
| GET | `/api/v1/auth/session` | — | `Session` (401 `unauthenticated` when there is none) |
| POST | `/api/v1/auth/logout` | — | 204, cookie cleared |
| POST | `/api/v1/auth/totp/enroll` | — | `TotpEnrollment` {secret, otpauth_uri, …}; the UI draws the QR code |
| POST | `/api/v1/auth/totp/confirm` | `TotpCode` {code} | `TotpConfirmed` {recovery_codes}, shown once |
| POST | `/api/v1/auth/totp/disable` | `TotpCode` {code} (TOTP or recovery code) | 204 |

Rules:
- **Session**: cookie `rynas_session`, `HttpOnly; SameSite=Strict; Path=/` (+ `Secure` over HTTPS:
  the HTTPS port (W0), `X-Forwarded-Proto: https` or `--secure-cookies`); 256-bit random token, only its SHA-256 is kept in
  memory. Idle timeout 30 min (`--session-idle-timeout`), absolute 12 h (`--session-max-age`).
  Sessions do not survive a restart of `rynas-web`.
- **Idle timeout = user inactivity** (DSM model): only user-initiated requests renew it. Event
  streams (`/events`, `*/stream`) and requests with `X-Rynas-Background: 1`
  (`rynas_api_types::BACKGROUND_HEADER`) are checked but do not renew it; the UI sets the header
  on every request made more than 60 s after the last key, click or wheel event (pollers,
  timers, reconnects), so an unattended open tab is signed out after 30 min.
- **CSRF**: every method other than GET/HEAD/OPTIONS needs the `X-CSRF-Token` header equal to
  `Session.csrf_token` (403 `csrf`). Login accepts JSON only.
- **SSE**: `/metrics/stream` requires the session (the cookie travels with `EventSource`), does not
  renew the idle timeout and closes when the session ends (the UI reconnects and gets 401).
- **Errors** (`ApiError.code`, constants in `rynas_api_types::error_codes`): `invalid_credentials` 401
  (same answer for unknown users and non-administrators), `totp_required` 401 (password right, code
  missing: the UI asks for the code and repeats the login), `invalid_totp` 401/400, `rate_limited` 429
  with `Retry-After`, `auth_unavailable` 503 (rynasd unreachable and no `--users-file`),
  `totp_already_enabled` / `totp_not_enrolled` 409, unknown `/api/v1` paths 401 without a session.
- **Roles**: only `admin` for now: members of `rynas-admin` or `sudo` (`--admin-group`, repeatable).
  Guards are explicit anyway, so that other roles can be added safely: in rynas-web
  `require_admin` (route layer after the session middleware on every route except logout and
  the user's own TOTP) refuses any method other than GET/HEAD/OPTIONS, and the LUKS header
  download, unless the session's role is admin (`is_admin` is an exhaustive match on `Role`);
  rynasd checks the forwarded user again from the system groups (`--admin-group`, same
  defaults; `root` always) before root-only operations: user tasks running as `root`, and
  any whole `scheduler` value that adds, changes or removes one — `POST
  /config/revisions/{n}/restore` (checked before staging), `rynasd import --owner` (the owner,
  checked before anything is staged or written) and `POST /config/apply` (the changeset,
  however it was staged: restore, archive, drift import). 403 `forbidden`.
- **Rate limiting**: per IP (IPv6 per /64) and per user name, 5 free failures per user and 20 per IP,
  then exponential backoff from 2 s to 15 min. The IP is the TCP peer, or the right-most
  untrusted `X-Forwarded-For` entry from the reverse proxy on loopback ("Behind the built-in
  reverse proxy (W1b)"); failures logged as
  `authentication failure for USER from IP` for fail2ban (`packaging/fail2ban/`).
- **TOTP**: RFC 6238 (SHA1, 6 digits, 30 s, ±1 step), each code accepted once, 10 recovery codes
  (80 bits, stored as SHA-256). Enabling it ends the user's other sessions.

Processes: `rynasd` (root) listens on `/run/rynas/rynasd.sock` (0660, group `rynas`, `SO_PEERCRED`
allow-list): PAM (service `rynas`, `packaging/pam.d/rynas`, fallback `login`), groups, TOTP secrets in
`/var/lib/rynas/auth/totp/` (0600). Internal protocol in `crates/rynas-auth/src/ipc.rs`; systemd units
in `packaging/systemd/`.

Development without root:
```
cargo run -p rynasd -- --socket /tmp/x/rynasd.sock --state-dir /tmp/x/state --dry-system
printf 'pw\n' | cargo run -q -p rynas-web -- hash-password      # hash for the users file
cargo run -p rynas-web -- --listen 127.0.0.1:8080 --rynasd-socket /tmp/x/rynasd.sock \
    --users-file users.toml [--auth-state-dir /tmp/x/web]
```
`users.toml`: `[[user]]` tables with `name`, `hash`, `groups = ["rynas-admin"]` (internal realm
"rynas", tried before PAM). A non-root `rynasd` can only verify the password of the user it runs as
(`pam_unix` goes through `unix_chkpwd`); every other PAM login is refused.

## API v1: web TLS (W0)
Wave W0 of `../research/web-access.md` (phase 1 debt before the web-access module): `rynas-web`
serves HTTPS with rustls on its **own port `:8443`**, the emergency door that never goes through
the reverse proxy (D-PROXY revised 2026-11-07: "rynas-web keeps `:8443` with rustls as emergency
access … it reuses the active certificate"). Like DSM's 5001, Proxmox's 8006 and Cockpit's 9090.

**Listeners** (`rynas-web`, `/etc/default/rynas-web`, unit `rynas-web.service`):

| Setting | Default | Meaning |
|---|---|---|
| `--tls-listen` / `RYNAS_WEB_TLS_LISTEN` | `[::]:8443` | HTTPS; `[::]` is bound dual-stack (`IPV6_V6ONLY` off), `0.0.0.0` when the kernel has no IPv6; `off` disables it |
| `--tls-cert`, `--tls-key` | `/etc/rynas/tls/ui.crt`, `ui.key` | the pair rynasd publishes |
| `--listen` / `RYNAS_WEB_LISTEN` | `auto` (unit, since W1b; was `0.0.0.0:8080`) | plain HTTP: the network until rynas-caddy fronts the interface, then loopback only (next section) |
| `--proxy-state` | `/run/rynas/web-access.ui` | where rynasd says the proxy fronts the interface (W1b) |
| `--no-https-redirect` | off | serve plain HTTP to other hosts instead of redirecting |

- HTTPS and plain HTTP serve the same router. TLS 1.2/1.3 (ring provider, no aws-lc C build),
  ALPN `http/1.1`, handshake limit 10 s, one task per connection. IPv4-mapped peers
  (`::ffff:a.b.c.d`) are handed on as IPv4, so the login rate limit (IPv6 grouped per /64) and
  the network endpoints see real IPv4 addresses.
- **Redirect**: once a certificate is loaded, plain-HTTP requests from other hosts get `307` to
  `https://<same Host>:<https port><path and query>` with `Cache-Control: no-store` (never a
  cached `301/308` that would outlive a later opt-out). Never redirected: loopback peers (a proxy
  on this host — the future Caddy upstream —, SSH tunnels), `X-Forwarded-Proto: https`, a
  missing or malformed `Host`, `--no-https-redirect`. Before a certificate exists nothing is
  redirected and HTTPS handshakes fail.
- **Cookies**: `Secure` on every answer over the HTTPS port (request extension
  `rynas_web::tls::Https`), plus the step-4 rules (`X-Forwarded-Proto: https`,
  `--secure-cookies`). Sessions, CSRF and idle rules are unchanged.
- **No HSTS** on `:8443`: with a certificate the browser does not trust yet, HSTS removes the
  "continue" button (RFC 6797 §12.1), which would turn the emergency door into a lockout. HSTS
  belongs to public routes of the reverse proxy (W2), `max-age` only.
- **Hot reload** without a restart: the pair sits behind a rustls `ResolvesServerCert` swapped
  in place; rynas-web re-reads the files when size, mtime or inode change (every 10 s), on
  `SIGHUP` (`systemctl reload rynas-web`) and right after `import` / `local` succeed. A pair whose
  key does not match its chain (caught between the two renames of an update) is skipped until
  the next check; the previous pair keeps serving. Open connections keep their certificate.

**Certificates** (`rynasd`, crate `rynas-tls`, module `rynasd::tls`): rynasd (root) is the only
writer; rynas-web (user `rynas`) only reads.

| File | Mode | Content |
|---|---|---|
| `/var/lib/rynas/tls/ca.crt`, `ca.key` | 0600 root (dir 0700) | local CA: ECDSA P-256, 10 years, `CN=rynas local CA (<host>)`, path length 0 |
| `/var/lib/rynas/tls/local.crt`, `local.key` | 0600 root | leaf from the local CA + the CA certificate |
| `/var/lib/rynas/tls/imported.crt`, `imported.key` | 0600 root | the administrator's chain and key |
| `/var/lib/rynas/tls/active` | 0600 root | `local` or `imported` |
| `/etc/rynas/tls/ui.crt` | 0644 (dir 0750 root:rynas) | **active** chain, read by rynas-web |
| `/etc/rynas/tls/ui.key` | 0640 root:rynas | active key (written before `ui.crt`; each file through a temporary file and a rename) |
| `/etc/rynas/tls/ca.crt` | 0644 | local CA certificate ("Trust this NAS") |

- **Local leaf**: ECDSA P-256 (every browser and TLS stack; Ed25519 certificates are still not
  accepted by browsers for TLS), 397 days (inside the 398 days browsers allow public CAs and
  Apple's 825-day rule that applies to every TLS server certificate), `serverAuth`,
  `digitalSignature`, not a CA, authority key identifier; `notBefore` one day back for client
  clock skew. SANs: host name, `<first label>.local`, the full name when the host name has dots,
  `localhost`, `127.0.0.1`, `::1`, and the addresses of the interfaces (`ip -j addr`), except
  container/VM bridges (`docker*`, `br-*`, `veth*`, `virbr*`…), IPv6 link-local (browsers do not
  take zone ids in URLs, so such a SAN can never match), temporary and deprecated IPv6 (they
  rotate daily). IPv4 link-local stays (a NAS without DHCP).
- **Why a local CA and not a bare self-signed leaf** (Proxmox model: cluster CA + node
  certificate): trusting the CA once survives every re-issue for a new name or address, and the
  web-access module (W1) hands the same root and key to Caddy's `pki` app as its local CA, so
  the user trusts one root for `:8443` and for `<app>-<host>.local` names.
- **Re-issue**: at start and every 120 s rynasd compares the names with the local leaf and
  issues a new one when a name or address is **missing** (extra stale names do not count: a
  flapping address does not re-issue back and forth) or 30 days before it expires (Cockpit's
  `cockpit-certificate-ensure` does the same). A CA within a year of its end is replaced (the
  leaf follows). An imported pair is never touched; when it becomes unreadable the local leaf
  takes over.
- **Why `/etc/rynas/tls`** and not `/var/lib/rynas/certs` as `../research/web-access.md` §3.2
  first said: systemd resets `StateDirectory=` to `StateDirectoryMode=0700 root` at every start
  of rynasd, so rynas-web could never enter `/var/lib/rynas`. `/etc` is where Cockpit
  (`/etc/cockpit/ws-certs.d`) and Proxmox (`/etc/pve/local/pveproxy-ssl.*`) keep their UI
  certificates; `apt purge` removes `/etc/rynas`.
- **Active certificate for W1/W2**: the certificate store of the web-access module (ACME
  wildcard, Tailscale) adds sources next to `local` / `imported` in the same store and publishes
  the chosen one through the same `ui.*` files, so `:8443` reuses the wildcard automatically
  (D-PROXY).
- **Recovery**: `rynasd tls-reset` (root, also with the daemon running) makes the local
  certificate active with a new leaf; rynas-web picks it up by itself. `http://127.0.0.1:8080`
  is never redirected.

**Endpoints** (admin; forwarded by rynas-web to rynasd `/v1/system/tls…`). rynasd checks the
administrator role again on every one, reads included (a route layer, 403 `forbidden`), and
writes an audit line for a change (`rynas::audit`: `tls import|local <subject> (<sha-256>) by
<user>`). The local CA has no name constraints: the web-access leaves it issues carry the
administrator's own domain names and global IPv6 addresses, so permitted subtrees could not be
bounded; `ca.key` stays in rynasd's 0700 store.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/system/tls` | — | `TlsStatus` {source: `local`\|`imported`, certificate, chain, local_ca?, warnings, missing_names} |
| GET | `/api/v1/system/tls/ca` | — | the local CA certificate, `application/x-pem-file`, `Content-Disposition: attachment; filename="rynas-local-ca.crt"` |
| POST | `/api/v1/system/tls/import` | `TlsImportRequest` {certificate, private_key} (PEM) | `TlsStatus`; served at once |
| POST | `/api/v1/system/tls/local` | — | `TlsStatus`; back to the local certificate (re-issued when names are missing) |

`TlsCertificate` (each entry): `subject`, `issuer` (RFC 4514), `dns_names`, `ip_addresses`,
`not_before_ms`, `not_after_ms`, `serial`, `fingerprint_sha256` (uppercase hex with colons, as
browsers show it), `key_type` ("ECDSA P-256", "RSA 2048", "Ed25519"), `self_signed`, `is_ca`.
`warnings` (`TlsWarning`): `expires_soon` (< 30 days), `expired`, `names_not_covered` (see
`missing_names`), `chain_incomplete` (an imported leaf from a CA without its intermediates).

Import checks (`400 invalid_value`, `field` = `certificate` or `private_key`): PEM only, at most
64 KiB each and 8 certificates; the first certificate is not a CA; an Extended Key Usage, when
present, allows `serverAuth`; each certificate is issued **and signed** by the next one (order
of a full chain); valid now (not expired, not starting more than a day ahead); an unencrypted
key the ring provider supports (RSA ≥ 2048, ECDSA P-256/P-384, Ed25519) whose public key is the
leaf's. The chain is stored re-encoded (comments and `Bag Attributes` dropped). Names not
covered and near expiry are warnings, not errors (a certificate for another name is still a
valid choice). Errors without a field: `503 backend_unavailable` (rynasd without the
certificate service), `404 not_found` (no certificate yet).

**UI (rynas-ui, a later wave)**: a "Certificate" card under System. i18n keys it needs:
`tls.title`, `tls.source.local`, `tls.source.imported`, `tls.subject`, `tls.issuer`, `tls.names`,
`tls.addresses`, `tls.valid_from`, `tls.valid_until`, `tls.days_left`, `tls.fingerprint`,
`tls.key_type`, `tls.serial`, `tls.chain`, `tls.local_ca`, `tls.download_ca`, `tls.trust_help`
(per-OS steps), `tls.import`, `tls.import.certificate`, `tls.import.private_key`,
`tls.import.hint` (full chain, server certificate first, unencrypted key), `tls.use_local`,
`tls.use_local.confirm`, `tls.warn.expires_soon`, `tls.warn.expired`,
`tls.warn.names_not_covered`, `tls.warn.chain_incomplete`, `tls.missing_names`,
`tls.reload_note` (the page reconnects on the new certificate: a browser that trusted the old
one asks again), `tls.https_port`.

Development without root:
```
cargo run -p rynasd -- --socket /tmp/x/rynasd.sock --state-dir /tmp/x/state --dry-system
#   publishes /tmp/x/state/dry-root/etc/rynas/tls/ui.{crt,key}
cargo run -p rynas-web -- --listen 127.0.0.1:8080 --rynasd-socket /tmp/x/rynasd.sock \
    --tls-listen 127.0.0.1:8443 --tls-cert /tmp/x/state/dry-root/etc/rynas/tls/ui.crt \
    --tls-key /tmp/x/state/dry-root/etc/rynas/tls/ui.key --users-file users.toml
curl --cacert /tmp/x/state/dry-root/etc/rynas/tls/ca.crt https://localhost:8443/api/v1/auth/session
```
Tests: `rynas-tls` unit tests (SANs, CA chain, re-issue rule, key/cert match, import checks),
`rynasd::tls` (store, publish modes, re-issue on a new address, import/back to local, `ip -j`
parsing), `rynas-web` `tests/https.rs` (real rustls client trusting only the generated CA on an
ephemeral port: names, Secure cookie, no HSTS, hot reload, mismatched pair ignored, redirect
rules).

Sources (2026-11-07): rustls `ResolvesServerCert` and `CertifiedKey::keys_match`
(https://docs.rs/rustls/0.23.46/rustls/server/trait.ResolvesServerCert.html), hot-reload pattern
with an in-place resolver (https://docs.rs/crate/tls-hot-reload/latest), rcgen issuer API
(https://docs.rs/rcgen/0.14.10/rcgen/), Apple TLS requirements: SAN, `serverAuth`, ≤ 825 days for
every TLS server certificate (https://support.apple.com/en-us/103769), Chrome's 398-day limit
only for publicly trusted CAs (https://chromium.googlesource.com/chromium/src/+/HEAD/net/docs/certificate_lifetimes.md),
Cockpit's self-signed fallback and renewal (https://man.archlinux.org/man/extra/cockpit/cockpit-tls.8.en),
Proxmox `pveproxy-ssl.pem` over the node certificate from the cluster CA
(https://forum.proxmox.com/threads/custom-certificates-upload.157568/), systemd chmod of existing
`StateDirectory=` (https://bugzilla.redhat.com/show_bug.cgi?id=1778384), HSTS and untrusted
certificates (https://www.rfc-editor.org/rfc/rfc6797#section-12.1).

## Behind the built-in reverse proxy (W1b)
Wave W1 put the interface behind rynas-caddy on 80/443 (`../phase2/docker-apps.md` §5.3,
D-PROXY revised 2026-11-07); W1b pays the two debts it left in rynas-web.

**Trusted proxy** (`rynas_web::conn`, request extension `conn::Client`):
- `X-Forwarded-For` is honoured **only** when the connection is plain HTTP (never `:8443`,
  which no proxy fronts), the TCP peer is loopback **and** the local address is loopback (the
  plain listener as Caddy dials it, `127.0.0.1:<port>`). From a LAN peer or over HTTPS the
  header is ignored and the client is the peer.
- The client is the **right-most untrusted** entry of every `X-Forwarded-For` field taken
  together: walked from the right, loopback entries (the proxy) skipped, the first other
  address wins; an entry that is not an address stops the walk and the peer is used (nothing
  is guessed from the client-controlled left part). Entries may carry a port or brackets;
  IPv4-mapped addresses become IPv4.
- That address feeds the login rate limit (per IP, IPv6 per /64), the failure log line
  `authentication failure for USER from IP` (fail2ban: format unchanged, the jail now covers
  80, 443, 8080 and 8443) and the lockout notification. Other per-connection logic (the
  network endpoints' local address, the web access confirmation) keeps using the socket.
- Caddy side: nothing to render. Caddy's `reverse_proxy` sets `X-Forwarded-For`,
  `X-Forwarded-Proto` and `X-Forwarded-Host` and passes `Host` through; from clients outside
  `trusted_proxies` (rynas-caddy has none: it is the edge) it replaces the incoming
  `X-Forwarded-*` values instead of appending, so the right-most entry is the address Caddy
  saw. The golden render is unchanged.
- Residual risk: another local process (a shell user, a host-network container) can reach
  `127.0.0.1:8080` too and claim any address — to dodge the per-IP limit (the per-user limit
  still applies) or to get an address banned by fail2ban. Closing it needs an upstream only
  the proxy can reach (a Unix socket checked with `SO_PEERCRED`), noted for W3 with SSO.
- **Cookie and CSRF through the proxy**: `Secure` because Caddy sends
  `X-Forwarded-Proto: https` (honoured from anyone: it can only make the cookie stricter).
  CSRF is the per-session token plus `SameSite=Strict` and JSON-only login; nothing depends
  on `Host` or `Origin`, so the interface works on any name the proxy serves, and a forged
  `Origin` changes nothing. No `Origin` allow-list is added: it would need every name of the
  NAS and gives nothing the token does not.

**Plain port on loopback** (`rynas_web::listen`, `--listen auto[:PORT]`, package default):
- `0.0.0.0:PORT` (8080) while the proxy does not front the interface; `127.0.0.1:PORT` only
  once it does **and** `:8443` listens with a certificate. With HTTPS off, not bound or without
  a certificate the plain port stays on the network: nobody is locked out.
- "Fronts": rynasd writes the names the proxy serves the interface on to
  `/run/rynas/web-access.ui` (0644) after each successful load and periodic check
  (`rynasd::web_access::UI_STATE`), and removes it in external mode, when rynas-caddy is not
  installed or the interface has no names. rynas-web reads it every 5 s.
- A move stops accepting on the old socket (open connections and event streams carry on) and
  binds the new one, sessions intact (no restart); if the new address cannot be bound the old
  one comes back and the move is retried. Caddy dials `127.0.0.1:PORT` in both states.
- An explicit address is used as it is (`127.0.0.1:8080`: closed for good; `0.0.0.0:8080`: a
  proxy on another host, which then needs `--secure-cookies` or `X-Forwarded-Proto` and gets
  no per-client rate limit).
- Upgrade: `/etc/default/rynas-web` is a conffile. Unchanged, it becomes `auto`; edited, dpkg
  asks and the kept explicit address keeps the old behaviour. An upgraded NAS without
  rynas-caddy keeps `:8080` on the network.

Tests: `rynas-web` unit tests in `conn.rs` (right-most parsing, trust conditions) and
`listen.rs`; `tests/forwarded.rs` (rate limit per real client behind the proxy, spoofed
headers from the LAN and over HTTPS ignored, Secure cookie and CSRF via the proxy with
`Host` = the interface's name, real-socket end-to-end trust, `auto` moving to loopback and
back, staying on the network without HTTPS); `rynasd` `tests/web_access.rs` (the state file
written on apply, removed in external mode and without the engine).

Sources (2026-11-07): MDN, `X-Forwarded-For`, "Selecting an IP address" (right-most, trusted
proxy list; https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/X-Forwarded-For);
Caddy `reverse_proxy` "Defaults" and `trusted_proxies`
(https://caddyserver.com/docs/caddyfile/directives/reverse_proxy#defaults); Adam Pritchard,
"The perils of the 'real' client IP" (https://adam-p.ca/blog/2022/03/x-forwarded-for/).

## API v1: configuration and tasks (step 6)
Configuration lives in `rynasd` (`/var/lib/rynas/config.db`, crate `rynas-config`), its only writer;
`rynas-web` forwards these endpoints over the Unix socket with the session user in the internal
`x-rynas-user` header. Session and CSRF rules are those of step 4 (every non-GET needs `X-CSRF-Token`).

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/config/pending` | `?detail=1` | `PendingChanges` {owner, changes: [`PendingChange` {key, staged_ms, conflict, diff_count}]}: compact by default (what the pending-changes bar reads); with `detail=1` each change also has `base`, `value` (whole documents) and `diff` (the review dialog). Strong `ETag` (hash of the body); `If-None-Match` with the same tag → `304` without a body. API answers stay `no-store`, so the client keeps the tag and the last body itself |
| POST | `/api/v1/config/apply` | `ApplyRequest` {comment, keys?} | 202 `TaskStarted` {task_id}; 409 `nothing_pending` / `conflict` / `apply_group_split`. `keys` applies only those staged keys (the Save of a dialog, guidelines A19b), the rest stays pending; keys staged together by one operation (a snapshot job and its scheduler task) must be selected together, and a destructive staging (delete a share with its folder) is applied only without `keys` (the pending bar): otherwise 409 `apply_group_split` |
| POST | `/api/v1/config/discard` | — | 204 |
| GET | `/api/v1/config/revisions?limit=N` (default 50, max 500) | — | `Vec<RevisionSummary>` {id, author, ts_ms, comment, keys, restored_from?}, newest first |
| GET | `/api/v1/config/revisions/{n}` | — | `RevisionDetail` {revision, diff, files} |
| POST | `/api/v1/config/revisions/{n}/restore` | — | `StagedChanges` {staged}: a new pending changeset |
| GET | `/api/v1/config/drift` | — | `DriftReport` {checked, entries: [`DriftEntry` {path, key, status, expected_sha256, actual_sha256?}]} |
| POST | `/api/v1/config/drift/{key}/import` | — | `StagedChanges`: the system's current state staged as pending |
| POST | `/api/v1/config/drift/{key}/overwrite` | — | 202 `TaskStarted`: the applied configuration written again |
| GET | `/api/v1/system/hostname` | — | `HostnameState` {applied, pending?, running} |
| PUT | `/api/v1/system/hostname` | `HostnameConfig` {hostname} | `HostnameState` (stages a pending change; 400 `invalid_value`) |
| GET | `/api/v1/tasks?limit=N` (default 50, max 500) | — | `Vec<TaskInfo>` {id, type, user, start_ms, end_ms?, status, progress?, error?}, newest first |
| GET | `/api/v1/tasks/{id}` | — | `TaskInfo` |
| GET | `/api/v1/tasks/{id}/log?offset=N&limit=B` | — | `text/plain` from byte `offset`; headers `X-Rynas-Next-Offset`, `X-Rynas-Log-Complete: true\|false` |
| POST | `/api/v1/tasks/{id}/cancel` | — | 204; 409 `task_finished` |
| GET | `/api/v1/tasks/stream` | — | SSE, events `started` / `updated` / `finished`, data `TaskEvent` {kind, task}, serialized once; `resync` (data `{}`) when the client fell behind |

Model and rules:
- **Domains**: one key per domain holding a JSON document (`system.hostname`; `users` and `shares`
  since step 8), each with an `Applier` (validate/normalize, render with minijinja templates,
  check rendered files, reload, health check, import from the system). A key never applied shows
  what the system has (import).
- **Changesets are per user** (owner = user name, they survive logout and restarts). Each staged
  key remembers its base value; if the applied value changes in the meantime (another user applied),
  the change shows `conflict: true` and apply answers 409 `conflict` until it is discarded and staged
  again. Staging the current value drops the change.
- **Apply** runs as task `config.apply`, one at a time: render → `<file>.rynas-new` candidates →
  validation hooks (e.g. `testparm -s`) → copy of the replaced files to `backup/<rev>/files/` +
  atomic rename (fsync) → reload (systemd `ReloadOrRestartUnit` over D-Bus, `hostnamectl`) →
  health check → commit of the revision. Any failure restores the previous files and reloads the
  previous values; cancellation is honoured until the files are replaced. Files whose content does
  not change are not rewritten.
- **Revisions**: author, time, comment, semantic diff per key (`key.field`, `key.list[name].field`),
  full snapshot; ids never reused; retention 200 revisions or 365 days, the latest always kept.
  Restore stages a new changeset: history is never rewritten.
- **Anti-drift**: whole files carry `# Managed by rynas — do not edit (sha256:…)` as first line
  (files rewritten by other tools, like `/etc/hostname`, go without it); shared files get a
  `# BEGIN rynas <id> … # END rynas <id>` block (e.g. the `127.0.1.1` line of `/etc/hosts`; edits
  outside the block are not drift). The hash of the generated content is stored per file.
- **Tasks** (crate `rynas-tasks`): id `UPID:<node>:<pid>:<seq>:<start>:<type>:<user>:` (hex),
  status `queued|running|ok|error|cancelled`, log `/var/lib/rynas/tasks/<id>.log` capped at 1 MiB
  (ends with `TASK OK`, `TASK ERROR: …` or `TASK CANCELLED`), 4 concurrent (`--max-tasks`), 32
  queued (then 409 `busy`), history of 500 tasks / 30 days, tasks interrupted by a restart are
  marked failed. External commands: argv only (no shell), own process group, timeout, SIGTERM then
  SIGKILL to the group on timeout or cancel, output copied to the task log.
- **Errors**: `invalid_value` 400, `not_found` 404, `nothing_pending` / `conflict` / `busy` /
  `task_finished` 409, `backend_unavailable` 503 (rynasd down).

Backup of the configuration (CLI of rynasd, `.rynasbak` = tar + zstd with `manifest.json`,
`config.db` copied with the SQLite backup API, optional `secrets.age` encrypted with an age
passphrase; without a passphrase the archive has no secrets):
```
rynasd export --out nas.rynasbak [--passphrase-file pass.txt]
rynasd import nas.rynasbak [--passphrase-file pass.txt] --owner alice [--restore-secrets] [--restore-notifications]
```
Import never applies: it stages the archive's values as `alice`'s pending changeset; when
the archive adds, changes or removes root user tasks, `alice` must be an administrator
(`--admin-group`), otherwise nothing is staged.
Contents (`crates/rynasd/src/backup.rs`): `config.db` (every domain, including the scheduler's
task definitions with their scripts and the snapshot jobs), `notify.db` (targets, rules,
silences, event history; SQLite backup API, `manifest.databases`) and, with a passphrase, the
secrets: TOTP files and the notification key `notify/secret.key` (it seals the targets'
passwords inside `notify.db`). Left out on purpose: `scheduler.db` (run history with up to
256 KiB of output per run: bulky, machine-specific, rebuilt by the next runs), task logs,
generated files, snapshots. `--restore-secrets` writes TOTP files that do not exist yet;
`--restore-notifications` (rynasd stopped: the engine keeps its key in memory) replaces
`notify.db` through the SQLite backup API and the key when the archive has it, keeping the
replaced files as `*.before-import-<ms>`; without the key (no passphrase) the targets come
back but their secrets must be entered again.

Development: `rynasd --dry-system` never touches the host: managed files go below
`<state-dir>/dry-root` (seeded with copies of `/etc/hostname` and `/etc/hosts`, or `--config-root`),
the hostname and unit reloads are only logged. SQLite is the system `libsqlite3` (Debian security
updates; build dependency `libsqlite3-dev`), zstd likewise (`libzstd-dev`).
## API v1: storage (step 7, read side and planning)
Served by `rynasd` (root: smartctl, mdadm and btrfs need it) through the internal socket
(`/v1/storage/...`, see `crates/rynas-auth/src/ipc.rs`) and exposed by `rynas-web` with a session.
Types in `rynas_api_types::storage`. Nothing here changes the system: plans only list commands.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/storage/disks` | — | `Vec<DiskSummary>`: model, serial, WWN, size, kind (hdd/ssd/nvme), transport, bay, power state, temperature, health verdict with reasons, `used_by`, SMR |
| GET | `/api/v1/storage/disks/{name}` | — | `DiskDetail`: summary + `SmartInfo` (ATA attributes, NVMe health log, self-test log, life left, bytes written) + `SettingCheck` list (recommended vs current) + partitions |
| GET | `/api/v1/storage/arrays` | — | `Vec<MdArray>`: level, members with role (active/spare/faulty/rebuilding), state, health, sync progress, bitmap, chunk, events, mismatch count |
| GET | `/api/v1/storage/filesystems` | — | `Vec<BtrfsFilesystem>`: devices with error counters, usage and profiles, compression, mount options, quota mode, scrub status, subvolumes |
| GET | `/api/v1/storage/tree` | — | `StorageTree`: per volume the layers filesystem → (LUKS) → md → partitions → disks, subvolumes, locked LUKS containers, unused disks |
| POST | `/api/v1/storage/plan/volume` | `VolumePlanRequest` {disks, level, encrypt, compress, label} | `StoragePlan` |
| POST | `/api/v1/storage/plan/grow` | `GrowPlanRequest` {array, disks, level?} | `StoragePlan` |
| POST | `/api/v1/storage/plan/replace` | `ReplacePlanRequest` {array, old, new_disk} | `StoragePlan` |
| POST | `/api/v1/storage/plan/scrub` | `ScrubPlanRequest` {filesystem, md_check} | `StoragePlan` |

`StoragePlan`: `valid`, `errors`, `warnings`, `raw_bytes`, `usable_bytes`, `fault_tolerance`,
`wasted_bytes`, `summary`, ordered `steps` (`command` with argv and an optional named stdin secret,
`write_sysfs`, `config` entries owned by rynas-config, `wait`), each step listing the devices it
destroys. `errors` and `warnings` are English (logs, CLI); the UI translates their coded twins,
same order: `issues` (`PlanIssue {code, field?, subject?}`) and `warning_items`
(`PlanWarning {code, args}`). Each step carries `kind` (`PlanStepKind`) and `args` next to its
English `description`. `PlanArgs` = `subject?`, `other?`, `items[]`, `size_bytes?`, `count?`,
`level?`; unknown codes parse as `other` (the UI then shows the English text). Errors: 401 without session, 403 `csrf` on POST without the header, 400 `bad_request`,
404 `not_found` (unknown disk), 503 `backend_unavailable` when rynasd is down, 504 `timeout`
when it does not answer in time, 502 `bad_gateway` for an answer rynas-web cannot read. `GET /api/v1/storage`
(unprivileged overview) is unchanged.

Rules:
- **Never wake a disk**: power state from ATA CHECK POWER MODE (`hdparm -C` semantics); smartctl
  always with `-n standby`; SMART read at most every 10 minutes per disk, on demand only;
  `mdadm --detail` and the btrfs usage/subvolume walk (60 s cache) are skipped while a disk below is
  in standby and the last answer is returned with `stale: true`; `btrfs filesystem show --mounted`
  (no device scan). Reading SMART resets the idle timer of some drives: it never runs in the
  background.
- **Verdict** (rynas, not just the drive's PASSED): critical = SMART failed, attribute failing now,
  ≥100 reallocated or ≥10 pending sectors, failed last self-test, NVMe spare at threshold, reliability
  or read-only bits, endurance used up; warning = reallocated/pending/uncorrectable/command timeouts,
  NVMe media errors, ≤20% life left, temperature above the per-type limit; info = CRC errors (cable),
  load cycles, age.
- **Volume layout** (planner): GPT partition per disk (128 MiB left free) → md 1.2 with internal
  bitmap ("single" = one-disk RAID1, so a mirror can be added) → optional LUKS2 (`--sector-size 4096
  --pbkdf argon2id --pbkdf-memory 262144`, recovery key, header backup, `noauto` in crypttab) →
  btrfs `-d single -m dup` mounted at `/<label>` with `noatime,compress=zstd:1`, subvolumes
  `.snapshots` and `@rynas-cache` (`chattr +C`). Secrets travel on stdin, never in argv.
- The list views (disks, arrays, filesystems), the tree and the planners share one inventory
  snapshot for 5 s; a refresh runs at most once at a time (waiters queue on an async lock, not
  on threads). A disk detail (SMART) runs outside that lock.
- External commands go through `rynas_storage::host::Host`, i.e. the one runner
  `rynas_tasks::cmd::Cmd`: argv only, timeout with the process group killed, `LC_ALL=C`, clean
  environment, output capped at 1 MiB. Parsers are pure and tested on fixtures (`crates/rynas-storage/tests/fixtures`).

## API v1: storage, write side (step 7)
Served by `rynasd` (`crates/rynasd/src/storage_ops.rs`, execution in `rynas_storage::exec`),
forwarded unchanged by `rynas-web` with the session user; session and CSRF rules of step 4.
Every system change is a task (202 `TaskStarted`, log, progress, cancel); settings are staged
like every configuration key (A19). Types in `rynas_api_types::storage`.

| Method | Path | Body | Response |
|---|---|---|---|
| POST | `/api/v1/storage/volumes` | `VolumeCreateRequest` {disks, level, encrypt, compress, label, confirm, serials, passphrase?, dry_run} | 202 (task `storage.volume-create`); 200 `StoragePlan` with `dry_run` |
| POST | `/api/v1/storage/volumes/{name}/grow` | `VolumeGrowRequest` {disks, level?, confirm, serials, passphrase?, dry_run} | 202 (`storage.volume-grow`) / 200 |
| POST | `/api/v1/storage/volumes/{name}/replace` | `VolumeReplaceRequest` {old, new_disk, confirm, serials, dry_run} | 202 (`storage.volume-replace`) / 200 |
| POST | `/api/v1/storage/volumes/{name}/scrub` | `VolumeScrubRequest` {action: start\|stop, md_check, dry_run} | 202 (`storage.scrub`, `storage.scrub-stop`) / 200 |
| POST | `/api/v1/storage/volumes/{name}/unlock` | `UnlockRequest` {passphrase} (passphrase or recovery key) | 202 (`storage.unlock`) |
| POST | `/api/v1/storage/volumes/{name}/lock` | — | 202 (`storage.lock`) |
| POST | `/api/v1/storage/volumes/{name}/reveal-recovery-key` | — | `RecoveryKey` {key}, once; then 404 |
| GET / DELETE | `/api/v1/storage/volumes/{name}/luks-header` | — | `application/octet-stream` (attachment) / 204 |
| GET / PUT | `/api/v1/storage/volumes/{name}/settings` | `VolumeSettings` {stripe_cache_size?} | `VolumeSettingsState` {applied, pending?, proposed_stripe_cache_size?, proposed_memory_bytes?, current_stripe_cache_size?} (PUT stages `storage.disks`) |
| GET / POST | `/api/v1/storage/volumes/{name}/subvolumes` | `SubvolumeCreateRequest` {name, compression, readonly} | `Vec<SubvolumeInfo>` / 202 (`storage.subvolume-create`) |
| PUT / DELETE | `/api/v1/storage/volumes/{name}/subvolumes/{sub}?confirm={sub}` | `SubvolumeSettings` {compression, readonly} | 202 (`storage.subvolume-settings`, `storage.subvolume-delete`) |
| GET / PUT | `/api/v1/storage/disks/{name}/settings` | `DiskSettings` {scheduler?, read_ahead_kb?, apm?, standby_s?} | `DiskSettingsState` {defaults, applied?, pending?, effective, current_*} (PUT stages `storage.disks`) |
| POST | `/api/v1/shares` | `Share` with `subvolume` {volume, compression} and an empty `path` | as before: the share is staged with path `/<volume>/<name>`, its subvolume is created by the apply |

Rules:
- **Checked at once, then again by the task** (fresh inventory, the plan must still be the
  same): a valid plan (400 `invalid_value` with the planner's errors: disk in use, system
  disk, too small, unknown...); the **typed confirmation** for destructive requests: `confirm`
  = the volume label and `serials` = the serial numbers of exactly the disks to erase (a disk
  without serial, e.g. a virtual disk, is named by its kernel name), else 400
  `confirmation_required`; the kernel's own word that every disk and partition is free
  (`open(O_EXCL)` fails while mounted in any namespace, in an assembled array, an open dm
  mapping or swap: 409 `conflict`); no other running operation on the same disks or volume
  (409 `busy`). `dry_run` answers the validated plan without a task.
- **Secrets**: the passphrase (8-512 characters) travels on standard input only, never in
  argv or a log (task log lines show argv; outputs carrying a secret are not logged). The
  recovery key (`systemd-cryptenroll --recovery-key`) is kept in rynasd's memory for one hour
  and handed out once; the LUKS header backup waits in `/run/rynas/luks-header-<label>.img`
  (tmpfs, 0400) for its download and DELETE.
- **Create**: the planner's steps in order, with rollback while it is safe: a failing step
  (or a cancel) undoes everything done so far in reverse — unmount, configuration entries
  removed (a new revision), LUKS closed, header backup removed, array stopped and member
  superblocks zeroed, partitions removed — and the task error is the report ("step 7/13
  (Create the btrfs filesystem) failed: …; rolled back: stopped /dev/md/x and cleared its
  member superblocks, removed the partitions of /dev/sdb"; steps that could not be undone are
  listed as "NOT rolled back, check by hand"). The volume is mounted by systemd from fstab
  (`systemctl start <unit>.mount`, plain `mount` when systemd is not PID 1), never in
  rynasd's private mount namespace.
- **Grow / replace**: before the first step that touches the array a failure removes the
  partitions written on the new disks; after it nothing is undone (the kernel owns the
  reshape/rebuild) and the report says so. Waits follow `sync_action`/`degraded`/
  `sync_completed` in sysfs with progress; cancelling stops the wait, not the md operation.
- **Scrub**: `btrfs scrub start -B -d` then the md `check` with its `mismatch_cnt` in the
  log; cancel or stop runs `btrfs scrub cancel` and writes `idle` to `sync_action`.
- **Unlock/lock**: `cryptsetup open --key-file -` (exit 2 → "wrong passphrase or recovery
  key"), the mount unit (units that need the mount follow), `systemctl
  try-reload-or-restart smbd nfs-server`; lock stops the mount unit and closes LUKS. Only
  volumes with a `storage.volumes` record (an fstab entry) can be unlocked.
- **Configuration** (rynas-config, revisions, drift, rollback like every key):
  `storage.volumes` (`StorageVolumes` {volumes: [`VolumeRecord`]}, written only by
  operations through `Config::apply_value`) → `# BEGIN rynas storage` blocks in `/etc/fstab`
  (`UUID=… /<label> btrfs noatime,compress=zstd:1,nofail[,noauto] 0 0`), `/etc/crypttab`
  (`rynas-<label> UUID=… none luks,noauto`) and `/etc/mdadm/mdadm.conf`
  (`ARRAY /dev/md/<label> metadata=1.2 UUID=…`), then `systemctl daemon-reload`;
  `storage.disks` (`StorageTuning` {disks: [{serial, …DiskSettings}], arrays: [{md_uuid,
  stripe_cache_size}]}) → `/etc/udev/rules.d/69-rynas-storage.rules`: per type HDD
  `mq-deadline`/2048 KiB, SSD `mq-deadline`/256, NVMe `none`/128, per-disk overrides matched by
  `ID_SERIAL_SHORT` (APM and spin-down through `RUN+="/usr/sbin/hdparm -q -B … -S …"`, HDDs
  only), md `stripe_cache_size` per array only when chosen (proposal: largest power of two
  ≤ 8192 within 2% of RAM); checked with `udevadm verify`, then `udevadm control --reload` and
  a `change` trigger of the disks.
- **Subvolumes**: names `[A-Za-z0-9][A-Za-z0-9._-]{0,63}` (`.snapshots`, `@rynas-cache` are
  never touched); compression off = property `compression none`; read-only = `ro true`;
  DELETE refuses a subvolume a share uses (409) and a non-empty one without
  `confirm=<sub>` (400 `confirmation_required`).
- `rynasd.service` gains `CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_MKNOD CAP_IPC_LOCK`; the package
  recommends mdadm, btrfs-progs, cryptsetup-bin, systemd-cryptsetup, gdisk, hdparm,
  smartmontools.

Real check: `packaging/test-storage.sh` (privileged Debian 13 container with the host's
`/dev`, rynasd with the test-only `--storage-loop-disks`, loop files only): through the
public API a RAID1 volume, subvolumes, scrub with md check, hot replace of a member, an
encrypted RAID1 (recovery key once, header backup checked with `cryptsetup isLuks`), lock,
wrong passphrase, unlock with the recovery key, refusals and a failing mkfs rolled back;
verified with mdadm, btrfs, cryptsetup, lsblk, findmnt and `/etc`. It runs without systemd
(mounts through the `mount` fallback): the systemd mount-unit path is covered by the unit
tests only.

## API v1: users, groups, shares, SMB and NFS (step 8)
Crate `rynas-services`, three configuration domains of `rynas-config` applied by `rynasd`:
`homes` (order 5: the folder of per-user home links and the default volume's homes folder), `users` (order 10) and `shares`
(order 20: its ACLs name the users, so a changeset that touches both creates the users first
and rolls back in reverse). Writes **stage** a pending change of the
session user (`POST /api/v1/config/apply` applies it, as for every key); reads show the staged value
if the user has one, else the applied value (or, for `users`, what the system has when rynas never
applied it), next to the system state. Session, CSRF and error rules are those of steps 4 and 6.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/users` | — | `Vec<UserView>` {user: `LocalUser`, status: `UserStatus` {managed, on_system, in_samba?, needs_password, pending?, badges}}; users created outside rynas included (`managed: false`, badge `unmanaged`) |
| POST | `/api/v1/users` | `LocalUser` (uid 0: next free; `home_volume`: empty = the default volume while user homes are on, else `/home`; `system` = `/home/<name>`; a mount point = that volume, service on) | 201 `UserView`; 409 `already_exists`; 400 `invalid_value` on `home_volume` (service off, not a volume that can hold homes) |
| GET/PUT | `/api/v1/users/{name}` | `LocalUser` (name, uid, home and home_volume cannot change: 400 `invalid_value` on `home`/`home_volume`) | `UserView` |
| DELETE | `/api/v1/users/{name}?remove_home=true&confirm={name}` | — | `StagedChanges` (the user leaves the share matrices in the same changeset); 400 `confirmation_required` without `confirm`; 409 `conflict` while a home move of the user waits for confirmation |
| POST | `/api/v1/users/{name}/password` | `PasswordRequest` {password} | 202 `TaskStarted` (task `user.password`); 409 `not_applied` while the user is only staged |
| POST | `/api/v1/users/adopt`, `/api/v1/groups/adopt` | `AdoptRequest` {names; empty = all} | 200 `StagedChanges`: accounts of the system (1000–59999) not in the configuration staged as they are; 400 for a name that is not one. `PUT` of such an account adopts it too |
| POST | `/api/v1/ids/plan` | `IdRemapRequest` {kind, name, new_id \| from_id, paths, acls} | 200 `IdRemapPlan` {from, to, change, roots, paths, allowed, blockers, scan: `RemapCounts`}: guards and a dry run of at most 5 s (`scan.partial`) |
| POST | `/api/v1/ids/remap` | `IdRemapRequest` (+ `expected` for progress) | 202 `TaskStarted` (task `ids.remap`: `new_id` changes the account first, then the files follow; `from_id` only files); 409 when blocked |
| GET/POST | `/api/v1/groups` | `GroupRequest` {name, gid?, description, members?} | `Vec<GroupView>` / 201 `GroupView` |
| GET/PUT/DELETE | `/api/v1/groups/{name}` | `GroupRequest` (`members` rewrites the users' `groups`) | `GroupView` / `StagedChanges` |
| GET/PUT | `/api/v1/services/smb` | `SmbSettings` | `SmbState` {settings, pending, running} |
| POST | `/api/v1/services/smb/recycle-cleanup?share=&days=` | — | 202 `TaskStarted` (task `smb.recycle-cleanup`) |
| GET/PUT | `/api/v1/services/nfs` | `NfsSettings` | `NfsState` {settings, pending, running} |
| GET/PUT | `/api/v1/services/homes` | `HomesSettings` {enabled, volume, smb: `SmbHomes` {enabled, recycle}} (stages the `homes` key and `shares.smb.homes` together; the `[homes]` share goes off with the service) | `HomesState` {settings, pending, active, link, folder, subvolume, volumes [`HomesVolume` {path, label?, fs_type, size_bytes, avail_bytes}], homes [`UserHome` {user, uid, home, location volume/system/other/missing, folder, volume?, subvolume, moved?}]}; 400 `invalid_value` (`volume`: not mounted, system path, filesystem without POSIX ACLs). `volume` is the default for new users: changing it moves no home |
| POST | `/api/v1/services/homes/move/plan` | `HomeMoveRequest` {users, to} (`to`: a volume's mount point, `system` for `/home/<user>`, empty for the default volume; `users` empty: every local user whose home exists, is not there yet and has no move waiting) | `Vec<HomeMovePlan>` {user, from, source, to, location, target, subvolume, size_bytes, size_partial, avail_bytes, ssh_keys, allowed, blockers [{code, detail}]} (no side effect); codes `service_off` (volume destinations only), `not_local`, `already_moved` (already there), `source_missing`, `source_not_plain`, `target_exists`, `target_invalid`, `processes`, `smb_sessions`, `share_inside`, `no_space`, `no_rsync`, `move_waiting` |
| POST | `/api/v1/services/homes/move` | `HomeMoveRequest` | 202 `TaskStarted` (task `homes.move`: the homes that can move, one after the other; the others are logged as skipped and fail the task); 409 `conflict` when none can |
| POST | `/api/v1/services/homes/move/{user}/finish` | — | 202 `TaskStarted` (task `homes.move-finish`: the kept old folder is deleted); 404 without a move waiting |
| POST | `/api/v1/services/homes/move/{user}/undo` | — | 202 `TaskStarted` (task `homes.move-undo`: old folder back, its link and passwd home back, the copy kept aside next to it as `.<user>.undone-<ms>`); 404, 409 `conflict` while the user has processes or SMB sessions |
| GET/POST | `/api/v1/shares` | `Share` | `Vec<ShareView>` {share, status: {pending?, path_ok, fs_type, subvolume, warnings, warning_items [{code, args}]}} / 201 `ShareView` |
| GET/PUT/DELETE | `/api/v1/shares/{name}` | `Share` (name cannot change; a new path gets a new fsid). DELETE `?remove_folder=true&confirm={name}` also stages the deletion of the folder (with its snapshots and recycle bin, at the end of the apply) and of its snapshot jobs and their scheduler tasks | `ShareView` / `StagedChanges`; 400 `confirmation_required`, 409 `conflict` when the plan forbids it |
| POST | `/api/v1/shares/plan/delete` | `ShareDeletePlanRequest` {share} | `FolderDeletePlan` {path, share?, subvolume, size_bytes, size_partial, snapshots, recycle_bin, usages [{kind, name}], allowed, blockers} (no side effect) |
| GET | `/api/v1/folders?sizes=` | — | `Vec<UnsharedFolder>` {volume, name, path, subvolume, size_bytes?, size_partial, size_at_ms?, size_standby, snapshots, removed_at_ms?, previous?}: top-level folders of the rynas volumes no share uses. Without `sizes` only the sizes already known (quick, nothing read); `sizes=true` measures the unknown ones and those older than 1 h (btrfs quota group of a subvolume when quotas are on, else a walk of 0.5 s per folder, 3 s in all), never on a volume with a disk in standby (`size_standby`, last known size kept); sizes are cleared by every apply of `shares` and every folder deletion |
| POST | `/api/v1/folders/plan/delete` | `FolderRef` {volume, name} | `FolderDeletePlan` |
| DELETE | `/api/v1/folders/{volume}/{name}?confirm={name}` | — | 202 `TaskStarted` (task `shares.folder-delete`: snapshots, folder, recycle bin); 400 `confirmation_required`, 409 `conflict` |
| POST | `/api/v1/shares/{name}/nocow` | `ShareNocowRequest` {enabled} | 202 `TaskStarted` (task `shares.nocow`: `chattr +C`/`-C` on the folder of the **applied** share); 404 before the apply, 409 `folder_not_empty` (on, folder holds files), 409 `conflict` (not btrfs, symbolic link, missing). See "No copy-on-write folders" below |
| GET | `/api/v1/shares/{name}/effective?principal=alice` (or `@group`) | — | `EffectivePermission` {access none/ro/rw/deny, sources [{kind, name, access, decisive}], explanation} |
| GET | `/api/v1/config/drift` | — | `DriftReport` gains `state: Vec<StateDrift>` {key, object, kind, detail}: `unmanaged`, `missing`, `mismatch`, `missing_in_samba`, `missing_in_passwd` |

Users and groups (`users` key, `UsersConfig` {users, groups, remove_homes}):
- Real Linux accounts, uid/gid 1000–59999, primary group `users`; names of system accounts
  (`root`, `sudo`, `staff`, `rynas-*`...) are refused. Home: `/srv/rynas/homes/<name>` for a home
  on a volume (`home_volume`, the default volume while the user homes service is on; created by
  rynas with its link before `useradd -M`: a btrfs subvolume, mode 0700, `/etc/skel` only for
  shell users), else `/home/<name>` (`useradd -m`, mode 0700), also while the service is on. See
  "User homes" below.
  Reconcile compares the wanted accounts with `/etc/passwd`/`/etc/group` and runs `groupadd`,
  `useradd`, `usermod` (full name, shell, expiry), `gpasswd -a/-d`, `pdbedit -x`/`-c [D]`, `userdel
  [-r]`, `groupdel` (argv only, behind the `Accounts` trait; `MemoryAccounts` for tests and
  `--dry-system`). Only accounts of the configuration are touched, deletions only for those of the
  previous value; memberships only among the groups rynas manages (`sudo`, `docker`... stay).
- `admin` = member of `rynas-admin` (role mapping unchanged: `rynas-admin`/`sudo`); `shell`:
  `none` (`/usr/sbin/nologin`, default), `sftp` (group `rynas-sftp`), `shell` (`/bin/bash`, group
  `rynas-ssh`; an existing login shell such as zsh is kept). The sshd side (`AllowGroups`, chroot)
  comes with the file-services plugin.
- `disabled`: shadow expiry day 1 plus Samba flag `D` (smbd answers `NT_STATUS_ACCOUNT_DISABLED`);
  `expires` = shadow expiry date; `obey pam restrictions = yes` applies both to SMB.
- **Password** (never stored, never in a log or in `config.db`): one transaction in task
  `user.password`: `chpasswd -c YESCRYPT` (stdin), then `pdbedit -a -t` (new Samba account) or
  `smbpasswd -s` (existing one), password on stdin; if the Samba step fails the previous shadow
  hash is restored with `chpasswd -e`. Rules for now: 8–256 characters, no control characters, not
  the user name. Users with SMB access and no Samba account (created before, or outside rynas) show
  `needs_password` and drift `missing_in_samba`.
- Rollback of a failed apply reconciles back to the previous value; a home removed by `userdel -r`
  (or a home subvolume deleted with the user) cannot come back.

User homes (`homes` key, `HomesConfig` {enabled, volume}; D-HOMES, `docs/research/user-homes.md`):
- Each home is either on the system disk (`/home/<user>`) or in the homes folder of a volume,
  `<volume>/homes/<user>`, on any mounted local filesystem that is not a system path (btrfs, ext4,
  xfs, f2fs, zfs; rynas volumes listed first); a homes folder is a btrfs subvolume on btrfs,
  `root:root 0711`. Users can be on different volumes; `volume` of the setting is the default
  for new users only. passwd names homes on a volume through the stable path
  **`/srv/rynas/homes/<user>`**, a symbolic link per user inside `/srv/rynas/homes` (a real
  folder on the system disk, `root:root 0711`). The first layout (`/srv/rynas/homes` one link to
  `<volume>/homes`) is turned into per-user links at rynasd start and at every apply of `homes`:
  the folder is built as `/srv/rynas/homes.rynas-new` and swapped in with
  `renameat2(RENAME_EXCHANGE)`, so homes never stop resolving. A missing volume leaves a link
  dangling: logins and SMB fail instead of writing on the system disk. Turning the service off
  changes nothing on disk (homes on volumes keep working, moves to the system disk stay possible).
- Homes in the folder: `<volume>/homes/<user>`, one btrfs subvolume per user (squota per home in
  phase 2), `<user>:users 0700`; an existing folder of that name is refused at creation. Deleting a
  user with `remove_home` runs `userdel` and then deletes the subvolume. Administrators get no
  file access to other homes by default.
- **Home move** (never automatic), from any place to any other (volume to volume, volume to
  system disk, system disk to volume): the plan lists the blockers (above); the task creates the
  homes folder if needed and the target (subvolume when the destination is btrfs), copies with `rsync -aHAXx --numeric-ids`, checks processes and SMB
  sessions again, second pass with `--delete`, verifies (an itemized dry run must find nothing,
  same counts of files, folders, links and bytes), sets 0700, switches the account: to a volume the user's link first,
  then the passwd home if it changes (between two volumes it does not); to the system disk the
  passwd home first, then the link goes. passwd and `home_volume` change through a `users`
  revision (`Config::apply_value`) when rynas manages the user, else `usermod -d`. Then it renames
  the old folder to `<folder>.rynas-old` and records the move (with both real folders) in
  `/var/lib/rynas/homes-moves.json`. Any failure before the switch removes the copy. The `users`
  reconcile runs `usermod -d` (never `-m`) only for an explicit change of `home` between two values
  whose new folder exists, never to fix a mismatch with the system.
- SMB `[homes]` (`shares.smb.homes`, off by default): `path = %H` (wherever the passwd home is),
  `browseable = no`, `valid users = %S`, `create mask 0600`, `directory mask 0700`, own `#recycle`;
  a share named `homes` is refused while it is on. The file manager's `home` share is the real
  folder behind the passwd home (its jail never follows links), with that recycle bin.
- The homes folder never appears among the unshared folders and cannot be deleted there.
- Real check: `packaging/test-homes.sh` (Debian 13 container with systemd, loop-mounted btrfs, the
  real units): service on with `[homes]`, a new user's subvolume home and SMB home (recycle bin,
  another user refused), the plan and its `processes` blocker, the move of the uid 1000 account's
  home (tree, modes, hard link, symbolic link, ACL, xattr, content and login checked), undo, move
  and confirm, homes created on another volume and on the system disk, moves volume1 → volume2
  (undo, again), volume2 → system disk → volume1, the default volume changed with homes on the
  old one, deletion with the home and its link, service off, and the first layout migrated to
  per-user links at start.

Shares (`shares` key, `SharesConfig` {smb, nfs, shares, remove_folders}):
- **Snapshot folder** (`show_snapshots`, off by default): a read-only bind mount of the share's
  snapshot directory (the job's `snapshot_dir`, wired into `smb.shadow_copy`) at
  `<share>/#snapshot`, as a managed systemd mount unit pulled in by
  `rynas-snapshot-folders.target` (survives reboots; rynasd's own mount namespace is private).
  Not vetoed over SMB while shown, `recycle:exclude_dir`; NFS clients see it with `crossmnt`.
  A rollback of the share unmounts and mounts it again. Design: `rynas_services::snapview` and
  `docs/research/snapshots-replication.md` ("Snapshots inside the share").
- **Deleting a share with its folder** (`remove_folders` [{share, path, snapshots [{dir,
  prefix}]}]): runs at the end of the reconcile, after smbd and the exports stopped using the
  folder: snapshot folder unmounted, `btrfs subvolume delete` of every snapshot, then
  `btrfs subvolume delete --recursive` (subvolume) or `rm -rf --one-file-system` (directory).
  Only folders directly on a rynas volume (`/<volume>/<name>`); a deleted folder cannot come back
  on rollback. A share deleted without its folder is recorded with its settings in
  `<state>/shares-removed.json` (200 entries) for "Unshared folders" → "Share again".
- A share points at an **existing directory** on a local filesystem (network/FUSE refused, system
  paths refused, `..`/`//` refused), or asks for its subvolume (`subvolume` {volume,
  compression}: path `/<volume>/<name>`, created by the apply through the `SubvolumeCreator`
  hook of `rynas_services::host`, implemented by rynasd; see the storage write side).
- `/etc/samba/smb.conf` (managed header): `[global]` without SMB1 (`server min protocol =
  SMB2_10`), NetBIOS off unless asked (`disable netbios`, `smb ports = 445`), `catia fruit
  streams_xattr` everywhere, `access based share enum`, `multicast dns register = no`, `obey pam
  restrictions`, `inherit acls`/`map acl inherit`, `logging = systemd`, `log level = 1
  auth_audit:3`, optional interfaces, encryption `default`/`required`, extra options. Per share:
  presets Standard / Multi-protocol (`oplocks = no`, `level2 oplocks = no`, `kernel oplocks = yes`)
  / Time Machine (`fruit:time machine`, max size) / Huge folders (`case sensitive = yes`),
  `#recycle` (vfs_recycle: keeptree, versions, touch, excludes), `shadow_copy2` on btrbk names
  (`<prefix>.YYYYMMDDTHHMM`, only with a snapshot directory), `btrfs` VFS on btrfs, veto files,
  hide dot files, per-share encryption, extra options. The candidate is checked with `testparm -s`
  (non-zero exit, unknown parameters and global parameters in share sections abort the apply and
  restore the files).
- The share × principal matrix (`permissions`: user/group × rw/ro/deny) becomes POSIX ACLs on the
  share root, access and default (`setfacl --set` / `-d --set`; owner `rwx`, owning group and
  others nothing, users with an explicit entry get their effective permission, members of a denied
  group get `u:<member>:---`), `create mask = 0666`/`directory mask = 0777` so the default ACL
  decides, and `valid users`/`write list`/`read list`/`invalid users`. Effective permission: deny >
  RW > RO, with the deciding entries. An empty matrix leaves the directory's permissions alone.
  The drift report compares the ACL with `getfacl` (group membership changes show up there until
  the next apply of `shares`).
- Avahi `/etc/avahi/services/rynas-smb.service` (`_smb._tcp`, `_device-info._tcp` model RackMac,
  `_adisk._tcp` for Time Machine shares; removed when SMB or mDNS is off), wsdd2 and nmbd enabled or
  disabled, smbd reloaded (restarted when interfaces, NetBIOS or the minimum protocol change).
- NFS: `/etc/exports.d/rynas.exports` (per client: rw/ro, sync/async, root/all/no squash with
  anonuid/anongid, `no_subtree_check`, `sec=sys`, explicit `fsid=<uuid>`: the btrfs subvolume UUID
  read with the unprivileged `BTRFS_IOC_GET_SUBVOL_INFO`, else a generated UUID kept in the
  configuration), `/etc/nfs.conf.d/rynas.conf` (v4.1/4.2, v3 on demand, threads, fixed mountd/
  statd/lockd ports), `exportfs -ra`. `exportfs` has no dry run: every field is validated by rynas
  before rendering.
- Services whose unit is not installed are skipped with a log line.
- `#recycle` cleanup (task `smb.recycle-cleanup`): removes what was moved to the bin more than N
  days ago (ctime), walking with `openat(O_NOFOLLOW)`/`unlinkat` so that symlinks planted in a bin
  are removed, never followed; the scheduler (P3) will call it daily.

Additions to `rynas-config` for these domains: `Applier::order` (apply order across keys),
`Applier::reconcile(from, to)` (default: `reload(to)`; called with `(old, new)` by an apply and
`(new, old)` by its rollback), `Applier::state_drift` (non-file drift), `FileKind::Absent` (a file
that must not exist, removed with a backup), `ApplyCtx::log` (task log lines from appliers),
`System::restart_unit` / `System::set_unit_enabled`.

### No copy-on-write folders (NOCOW, 2026-11-07, D-NOCOW)

Video recordings, databases, VM images and torrents rewrite or append to big files all the
time; on btrfs every rewrite goes to a new place (copy-on-write) and such files fragment. The
usual answer is the per-inode `C` attribute (`FS_NOCOW_FL`, `chattr +C`) on an **empty**
folder: files created in it afterwards inherit it and are rewritten in place. rynas already
uses it for `@rynas-cache` (storage plan). What the files give up, said in the UI every time the
option is offered:

- **no data checksums** (NOCOW implies nodatasum): a scrub cannot detect damage in them, and on
  RAID1/RAID10 btrfs cannot repair them from the good copy; an interrupted write can leave a
  partial block;
- **no compression** (the kernel refuses compression on a NOCOW inode; rynas does not set the
  `compression` property on such a subvolume);
- **snapshots and reflinks still work**: the first write to a block shared with a snapshot is
  copied once, so data rewritten after a snapshot takes new space while the snapshot is kept
  (snapshot jobs on these folders should keep few snapshots, or none);
- it applies to files **created afterwards** only: the kernel sets the flag only on empty files,
  and `chattr +C` on a folder does not touch the files already in it.

The `nodatacow` mount option is not used: it is per filesystem (for btrfs, per mount of the
filesystem, so every subvolume), which is far too wide for a NAS volume.

Sources: btrfs docs, file attributes (`C`: "can be set/unset only on empty files", inherited
from the folder) https://btrfs.readthedocs.io/en/latest/ch-file-attributes.html; btrfs docs,
mount options (`nodatacow` implies `nodatasum`, "if nodatacow or nodatasum are enabled,
compression is disabled", in-place updates risk partial writes)
https://btrfs.readthedocs.io/en/latest/Administration.html; kernel patch refusing `+c +C`
together https://lore.kernel.org/r/20200710100553.13567-1-dsterba@suse.com; libvirt/QEMU set
NOCOW on VM image folders by default
https://lists.libvirt.org/archives/list/devel@lists.libvirt.org/thread/5NUPHFAH2BFLAVGZUKYAZTMHHTDDBOQC/;
the per-share "Enable Copy-on-write: Auto/No" of another NAS system, chosen when the share is
created, with the same caveats and the same "re-create the share and copy the data back with
`cp --reflink=never`" answer for existing data
https://forums.unraid.net/topic/131477-btrfs-copy-on-write-cow-new-share-question/ and
https://forums.unraid.net/topic/123037-reconsider-btrfs-nocow-default-option-on-domains-share-due-to-irrecoverable-corruption-risks/;
a shared-folder "data checksum" option that can only be chosen at creation ("create a new shared
folder with the option enabled and copy the existing data")
https://mariushosting.com/synology-how-to-create-a-shared-folder-on-dsm-7/. On ZFS the analogue
is per-dataset tuning (`recordsize`, `sync`, `logbias`) rather than turning off CoW, which ZFS
does not allow. The best ideas taken: a per-folder choice made at creation (not a volume-wide
mount option), the trade-offs spelled out next to the switch, and the honest "make a new folder
and copy" for existing data.

What rynas does:

| Where | What |
|---|---|
| New shared folder on a volume (`Share.subvolume.nocow`) | "No copy-on-write" switch under the volume picker (btrfs volumes only, so never on ext4/XFS paths); while it is on the compression switch is hidden. The apply creates the subvolume, then `chattr +C` on it while empty (`subvol::create_commands`; the `SubvolumeCreator::create` hook got a `nocow` flag). Remark `subvolume_on_apply_nocow` until the apply |
| New subvolume in Storage (`SubvolumeCreateRequest.nocow`) | the same checkbox in the create dialog (not in edit) |
| Existing share folder | `ShareStatus.nocow` (`Option<bool>`, read with one unprivileged `FS_IOC_GETFLAGS` ioctl on the open folder, only for btrfs folders: no fork per share, no disk write). The editor shows the state with an action: **on** only when the folder is empty (its `#recycle` and `#snapshot` do not count; refused with 409 `folder_not_empty`), **off** always (only new files change; the confirmation says files already there keep NOCOW). Task `shares.nocow`, `chattr` through `System` so `--dry-system` only logs it. Location column: "· No copy-on-write" |
| Snapshots | remark `nocow_snapshots` (computed by rynasd, which knows the snapshot jobs) on a NOCOW folder that has a snapshot job, plus a hint in the editor: snapshots work, rewritten data costs space; keep few, short retention, or none |
| Docker apps | `[[folders]] nocow = true` in `app.toml` (6.2): the install sets the flag on the folder it has just made (empty), through the descriptor it walked with `O_NOFOLLOW` (`FS_IOC_SETFLAGS`), only when that folder is on btrfs; an existing folder is left alone; a failure is a log warning, not an error. The wizard and the catalogue drawer show a "No copy-on-write" chip and a hint. Frigate's recordings folder is marked (entry 1.0.1); its SQLite config folder is not (small, and the stack snapshot should stay cheap) |

Not done, on purpose: moving the files of a non-empty folder into a new NOCOW folder as a task.
It needs the apps and clients that write there stopped, twice the space until the old folder
goes, a real copy (`rsync -aHAX` or `cp -a --reflink=never`: btrfs refuses to clone extents between
files with and without checksums, so a copy must not depend on reflinks), and a swap of every place that
names the folder (shares, app stacks, snapshot jobs). That is a rename across features that
rynas cannot do safely on its own; the editor explains the manual steps (new folder with the
option on, stop writers, copy, point the apps at it, delete the old one).

`rynasd.service`: `ProtectHome=no` (homes are created and removed below `/home`; shares may live
there too); `ProtectSystem=yes` keeps `/srv` (the folder of home links) and the volumes writable.
Real check: `packaging/test-services.sh` (Debian 13 container with systemd, loop-mounted btrfs,
the real units): users, groups and shares through the API, apply, passwords, then `testparm`,
`getfacl`, `pdbedit`, `smbclient` as allowed / read-only / denied / disabled users, NFSv4.2 mount,
drift, a `testparm` failure rolled back, disable and delete.

## API v1: notifications (P3)
Crate `rynas-notify` (D-NOTIFY). **rynasd is the single owner** of the engine
(`/var/lib/rynas/notify/notify.db`, its own SQLite file next to `config.db`); rynas-web forwards
the public endpoints (session + CSRF, `x-rynas-user`) and multiplexes the changes into
`/api/v1/events`. Targets and rules are rynas' own state, not system configuration: they take
effect at once, without the pending/apply cycle of A19 (like users' sessions or TOTP).

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/notifications?limit=&before=&severity=&category=&state=&acked=&key=&source=` | — | `Vec<Notification>`, newest first (A17: limit 50/500, `before` = id cursor); `severity` = at least |
| POST | `/api/v1/notifications/{id}/ack` | — | `Notification` (acknowledged by the session user; stops its reminders) |
| POST | `/api/v1/notifications/ack-all` | — | `AckAllResult` {acked} |
| POST | `/api/v1/notifications/silence` | `SilenceRequest` {key, duration_s?} | 201 `Silence` + `Location`; `key` exact or a prefix ending with `*`; `duration_s: null` = until removed |
| GET | `/api/v1/notifications/silences` | — | `Vec<Silence>` (active ones) |
| DELETE | `/api/v1/notifications/silences/{id}` | — | 204 |
| GET/POST | `/api/v1/notifications/targets` | `TargetRequest` {name, enabled, language en/it, config, secrets} | `Vec<NotifyTarget>` / 201 `NotifyTarget` + `Location`; 409 `already_exists` |
| GET/PUT/DELETE | `/api/v1/notifications/targets/{id}` | `TargetRequest` | `NotifyTarget` / 204; DELETE 409 `conflict` while a rule names it |
| POST | `/api/v1/notifications/targets/{id}/test` | — | `TargetTestResult` {ok, error?, duration_ms}: a test message sent at once (deadline 60 s) |
| GET/POST | `/api/v1/notifications/rules` | `NotifyRule` | `Vec<NotifyRule>` / 201 `NotifyRule` + `Location` |
| GET/PUT/DELETE | `/api/v1/notifications/rules/{id}` | `NotifyRule` | `NotifyRule` / 204 |

Internal only (rynasd socket, never exposed): `POST /v1/notifications/events` (`NewEvent` →
202 `NotifyAccepted` {id}: rynas-web's login and hardware events) and `GET
/v1/notifications/stream` (SSE `notification` / `resync`, followed by rynas-web for `/events`).
rynas-web follows `/v1/tasks/stream?config=1`, which adds `config_pending` (`PendingEvent`) to the
task events, so `/events` keeps two upstream connections.
Errors: `invalid_value` 400 (validation), `bad_request` 400 (JSON, query, non-numeric id),
`not_found` 404, `already_exists` / `conflict` 409, `backend_unavailable` 503. No new codes.

Model (types in `rynas_api_types::notify`):
- **Event** {source, category storage|hardware|ups|system|security|update|backup|app|task,
  severity info<notice<warning<error<critical, key, state firing|resolved|oneshot, title (200
  chars), body (4000), fields (32), actions (4, URLs `/…` or http(s) only), code (stable event
  code, `[a-z0-9_]` words joined by dots, ≤ 64; a malformed one is dropped), body_format
  text|log}. Stored as a
  `Notification` with `count`/`last_ms` (repeats), `silenced`, `acked_*`, `resolved_ms`.
- **State per key**: `firing` opens the condition and is delivered; repeats while open only
  count (a higher severity is an escalation and is delivered, a lower one is silent); `resolved`
  closes it (delivered with the condition's severity, so the same rules match; ignored when
  nothing was firing); `oneshot` repeats of a key within 10 min are counted, not delivered.
  State lives in the database: a restart neither repeats nor loses open conditions.
- **Target** {smtp (lettre; STARTTLS, TLS or none; auth), ntfy (title/priority as URL
  parameters, Bearer token or basic auth), telegram (bot API), gotify (`X-Gotify-Key`),
  webhook (GET/POST/PUT, header and body templates, default JSON body), ui (the notification
  center: no external delivery)}. A default `Web interface` target exists. Secrets are
  write-only (`secrets: {name: value|null}`; the answer lists the names that are set), sealed in
  the database with an age X25519 key (`notify/secret.key`, root 0600), redacted from error
  messages, never logged; HTTP errors are reported without their URL (Telegram tokens live in it).
- **Rule** (matcher) {min_severity, categories, source regex, field matches (exact or regex),
  mode all|any over the conditions that are set, invert, quiet_hours (weekly local windows,
  crossing midnight allowed; non-critical events are **held** and sent as one message when the
  window ends, `critical` never waits), targets, group_window_s (events for the same target
  within the window go out as one message), repeat_interval_s (reminders while a condition is
  firing, unacknowledged and not silenced; 0 or 5 min–30 days)}. Each target receives an event
  once even when several rules match.
- **Outbox** (SQLite, survives restarts): one row per (event, target), `next_ms` = now, end of
  the grouping window or end of quiet hours; the delivery loop groups due rows per target and
  group key (at most 50 events per message), rate limits per target (token bucket, 10 per
  minute; over the limit the row waits 30 s, not counted as an attempt), retries failures with
  backoff 30 s × 2ⁿ up to 1 h and drops them after 8 attempts (logged, `last_error` on the
  target).
- **Templates**: minijinja subject and body, one dictionary per language (English, Italian):
  severity and category labels, "Resolved:"/"Reminder:" prefixes, local time. Webhook URL,
  headers and body are user templates with `subject`, `message`, `severity`, `events`,
  `hostname`, `secrets` (`{{ subject|tojson }}`).
- **Caps** (M1): 5000 events / 90 days (open conditions kept), 10 000 keys, 2000 outbox rows,
  500 silences, 100 targets, 200 rules, 120 events per minute per source, ingest queue 256
  (rynasd) / 64 (rynas-web); broadcast(16) with `resync` for slow SSE clients.

Sources wired in phase 1:
| Source | Where | Keys / behaviour |
|---|---|---|
| Task failures | rynasd, task events | `task:<type>` firing on error, resolved by the next success; `config.apply` → `config:apply`, category system |
| TOTP enabled/disabled | rynasd | oneshot `auth:totp:<user>:<on>` (security; disabling is a warning) |
| Login lockouts | rynas-web login (rate limit hit) | oneshot `auth:lockout:<user>`, at most once per 10 min per account |
| Hardware (synohwd provider + live temperatures) | rynas-web, every 30 s | `hardware:fan:<n>` (error), `hardware:fan_fail` (critical), `hardware:temp:<sensor>` (CPU 85/95 °C, NVMe 70/80, bays 50/58, 3 °C hysteresis), `hardware:provider:stale` (> 300 s) |
| Storage | rynasd, every 5 min (`--storage-check-interval`), shared inventory snapshot | `storage:disk:<serial>:health` (warning/critical from the verdict; never reads SMART itself), `storage:md:<md>:health` (degraded/failed critical), `storage:btrfs:<uuid>:<devid>:errors` (error, repeats when counters grow), `…:missing` (critical), `storage:volume:<uuid>:usage` (80/90/95 % → warning/error/critical, 2-point hysteresis) |
| synodeb `notify` | `/run/rynas/notify.sock` | oneshot `synodeb:<source>:<hash>`; see `../synodeb-integration.md` S1 |

### Event codes
Every event rynas produces carries a stable `code` and the `fields` needed to rebuild its title
and body; `title`/`body` stay the English text (rules, English channels, fallback). The web
interface translates through `nev.<code>.title` / `nev.<code>.body` in
`crates/rynas-ui/src/i18n.rs` with `{field}` placeholders (variants `…_2`, `…_3` for optional
fields, most specific first; `nev.<code>.<field>.<value>` for values of closed sets; `{type}`
shows the task name `task.<type>`; decimals get the language's separator); a missing code, key or
field (empty counts as missing) shows the server's text. `body_format: log` (command output) is
never translated and is shown in monospace with its line breaks. Channel templates use the same
rules (`rynas_api_types::notify::event_text`) for Italian targets, reading the `nev.*`/`task.*`
lines of the UI dictionary at build time (`crates/rynas-notify/build.rs`). Notification db
schema 2 added the `code` and `body_format` columns.

| Code | Fields (besides the source's own) | English title |
|---|---|---|
| `hw.fan_stopped` / `hw.fan_running` | `fan` | Fan {fan} stopped / is running again |
| `hw.fan_failure` / `hw.fans_ok` | — | Fan failure / Fans back to normal |
| `hw.monitor_stale` / `hw.monitor_ok` | `provider`, `age_s` | Hardware monitor not updating / updating again |
| `hw.temp_high` / `hw.temp_ok` | `sensor`, `label`, `celsius`, `warn_c`, `crit_c` | {label} temperature {celsius} °C / back to normal |
| `hw.thermal_shutdown` / `hw.thermal_ok` | `sensor`, `label`, `celsius`, `shutdown_c` | Thermal shutdown imminent: {label} {celsius} °C / {label} cooled down |
| `storage.disk_warning` / `storage.disk_failing` / `storage.disk_ok` | `disk`, `disk_label`, `serial`, `bay` | Disk {disk_label} needs attention / failing / healthy again (body: the health reasons, not translated) |
| `storage.raid_degraded` / `storage.raid_failed` / `storage.raid_inactive` / `storage.raid_ok` | `array`, `level`, `state`, `degraded`, `raid_disks`, `members` (optional) | RAID {array} degraded / failed / inactive / healthy again |
| `storage.btrfs_errors` / `storage.btrfs_errors_cleared` | `volume`, `device`, `write_io`, `read_io`, `flush_io`, `corruption`, `generation` | Volume {volume}: device errors on {device} / counters cleared |
| `storage.btrfs_missing` / `storage.btrfs_back` | `volume`, `devid` | Volume {volume}: device {devid} missing / back |
| `storage.volume_full` / `storage.volume_ok` | `volume`, `used_pct`, `used_gib`, `free_gib` / `limit_pct` | Volume {volume} {used_pct}% full / back under {limit_pct}% |
| `task.failed` / `task.succeeded` | `type`, `task_id`, `user` | Task failed / succeeded: {type} (body: the task error, not translated) |
| `config.apply_failed` / `config.apply_ok` | `type`, `task_id`, `user` | Configuration apply failed / succeeded |
| `sched.task_failed` / `sched.task_ok` / `sched.task_finished` | `task`, `name`, `result`, `exit_code`, `signal` | Task {name} failed / succeeded / finished (body: output as `log`, or "Result: …") |
| `snapshot.job_failed` / `snapshot.job_ok` / `snapshot.job_finished` | same, `name` = job | Snapshot job {name} failed / succeeded / finished |
| `replication.failed` / `replication.ok` | `job`, `target`, `name`, `hint` (failed) | Replication {name} failed / works again (body: btrbk's or ssh's error, not translated) |
| `auth.lockout` | `user`, `ip`, `retry_after_s` | Login locked for {user} after failed attempts |
| `auth.totp_enabled` / `auth.totp_disabled` | `user` | Two-factor authentication enabled / disabled for {user} |
| `power.shutdown` / `power.shutdown_skipped` | `rule`, `reasons` (skipped) | Scheduled shutdown {rule} (skipped) |
| `net.confirmed` | `revision`, `interfaces` | Network change confirmed (revision {revision}) |
| `net.rolled_back` / `net.rollback_unverified` | `reason`, `revision`, `problems` | Network change rolled back / rollback could not be verified |
| `net.adopted` | — | Network migrated to rynas (systemd-networkd) |
| `net.boot_undone` | `revision`, `tested` | Unconfirmed network change undone at boot |
| `net.boot_dns_failed` | `revision` (ifupdown's, restored) | Network migration at boot undone: names did not resolve (`../research/network.md` §4.1) |
| `net.dns_repaired` | `detail` (English, not translated) | Name resolution repaired: `/etc/resolv.conf` had no server while networkd ran the network |
| `net.watchdog_restored` / `net.rescue` / `net.reset` | `revision` | Network restored to the last known good revision / rescue configuration / factory network |
| `removable.unplugged` | `drive`, `device`, `mount_points` | {drive} was unplugged while mounted (warning; not after a safe removal) |
| `removable.safe_to_remove` | `drive`, `device`, `method` (`power_off`/`eject`/`release`) | {drive} can be removed safely (info; setting `notify_safe_removal`) |
| `removable.ntfs_read_only` | `volume`, `drive`, `mount_point` | {volume} mounted read-only (warning: Windows left the NTFS volume dirty or hibernated) |
| `docker.unhealthy` / `docker.recovered` | `container`, `stack` | Container {container} is unhealthy / healthy again (`../phase2/docker-apps.md` §5.1) |
| `docker.oom` | `container`, `stack` | Container {container} ran out of memory |
| `docker.crash_loop` / `docker.crash_loop_over` | `container`, `stack`, `restarts`, `minutes` | Container {container} keeps restarting / stopped restarting |
| `docker.disk_full` / `docker.disk_ok` | `path`, `used_pct`, `limit_pct` | Docker storage {used_pct}% full / back under {limit_pct}% |
| `docker.update_available` | `image`, `stacks` | Image update available: {image} |
| `web_access.cert_expires_soon` / `web_access.cert_renewal_overdue` / `web_access.cert_failed` / `web_access.cert_expired` / `web_access.cert_ok` | `name`, `cert` | The certificate for {name} expires soon / is not being renewed / could not be obtained / has expired / is fine again (`../phase2/docker-apps.md` §5.4) |
| `pcbackup.late` / `pcbackup.resumed` | `device`, `days`, `alert_days` (late) | No backup from {device} for {days} days / Backups from {device} arrive again (`../research/pc-backup.md` §10) |
| `synodeb.<event>` | the forward's parameters | the forward's title; events and parameters in `../synodeb-integration.md` S1 |

Not coded: the target test message (built in the target's language already) and events from
plugins (they may send their own code; without a dictionary entry the text is shown as sent).

### Emitters and options

The watchers emit only on transitions (first observation after a start included); the engine
ignores repeats and resolutions of keys that were not firing. Other crates emit through the
`rynas_notify::Notifier` trait (`emit(NewEvent)`, never blocks): rynasd hands out the engine
(`DaemonState::notifier`), rynas-web its `WebNotifier`; plugins will use JSON-RPC with source
`plugin:<id>` set by the core. `.rynasbak` archives carry `notify.db` and, encrypted, its key
(step 6, backup of the configuration). Not yet: web push, Apprise, event × channel table, UPS events (with the nut plugin).

rynasd options: `--notify-socket` (default `/run/rynas/notify.sock`, empty = off; opened
only when a hardware module provides a forward, D-MODULE), `--storage-check-interval` (s,
default 300, 0 = off). A module's notification settings (synodeb's `/etc/synodeb/notify.conf`
below the configuration root, Synology module) are imported once into targets and an
"Imported from synodeb" rule. The unit
adds `StateDirectory=rynas/notify`; outgoing SMTP/HTTP uses the AF_INET families already allowed.

## API v1: task scheduler and snapshots (P3)
Crates `rynas-scheduler` (D-TASKS) and `rynas-btrfs` (D-SNAP, parts ported from btrbk-nas, MIT,
same author), two configuration domains of `rynas-config` applied by `rynasd` — `snapshots`
(order 30) and `scheduler` (order 40, after the jobs it runs) — and the root helper `rynasctl`
(`/usr/libexec/rynas/rynasctl`). Types in `rynas_api_types::scheduler` and
`rynas_api_types::snapshots`. Writes **stage** pending changes of the session user like every
domain (apply with `POST /api/v1/config/apply`); actions on the system are tasks (202
`TaskStarted`). Session, CSRF and error rules are those of steps 4 and 6; no new error code.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/scheduler/tasks` | — | `Vec<TaskView>` {task: `ScheduledTask`, status: {pending?, applied, unit, running, next_run_ms?, last_run?}} (staged view of the session user; staged deletions listed with `pending: delete`) |
| POST | `/api/v1/scheduler/tasks` | `ScheduledTask` | 201 `TaskView`; 409 `already_exists`; 400 `invalid_value` (calendar refused by `systemd-analyze`, unknown user, system account, root task running a file others can change, snapshot action: use the jobs endpoint) |
| GET/PUT/DELETE | `/api/v1/scheduler/tasks/{id}` | `ScheduledTask` (id cannot change; an internal task keeps its action) | `TaskView` / `StagedChanges`; DELETE of a job's task: 409 `conflict` (delete the job) |
| POST | `/api/v1/scheduler/tasks/{id}/enable`, `/disable` | — | `TaskView` (stages `enabled`) |
| POST | `/api/v1/scheduler/tasks/{id}/run` | — | 204: `systemctl start --no-block rynas-task-<id>.service` (same environment as a scheduled run); 409 `not_applied` while only staged |
| GET | `/api/v1/scheduler/tasks/{id}/history?limit=N&before=<run id>` | — | `Vec<TaskRun>` {id, invocation_id, start_ms?, end_ms, result, exit_code?, signal?, ok, output_bytes, output_truncated}, newest first |
| GET | `/api/v1/scheduler/tasks/{id}/history/{n}` | — | `TaskRun` with `output` (last 256 KiB) |
| POST | `/api/v1/scheduler/tasks/preview-calendar` | `CalendarPreviewRequest` {expression, syntax: calendar\|cron, count?} | `CalendarPreview` {valid, on_calendar, normalized, next_ms (5 by default, ≤ 20), error?}; always 200, no side effect |
| GET | `/api/v1/scheduler/system-timers` | — | `Vec<SystemTimer>` {unit, activates, next_ms?, last_ms?, on_calendar}: timers not generated by rynas (read-only) |
| GET | `/api/v1/scheduler/cron-entries` | — | `Vec<CronEntry>` {source, line, schedule, user, command, trigger?}: `/etc/crontab`, `/etc/cron.d/*`, `/var/spool/cron/crontabs/*` (read-only; `trigger` is the converted systemd trigger when one exists) |
| GET | `/api/v1/snapshots/jobs` | — | `Vec<SnapshotJobView>` {job: `SnapshotJob`, schedule?, task_id, enabled, pending?, config_path, snapshots, latest_ms?, next_run_ms?, last_run?} |
| POST | `/api/v1/snapshots/jobs` | `SnapshotJobRequest` {name, share?, subvolume?, snapshot_dir?, retention, schedule?, enabled?} | 201 `SnapshotJobView`: stages the job (`snapshots`), its task `snapshot-<name>` (`scheduler`, default `hourly`) and, for a share with SMB, its previous versions (`shares`: `shadow_copy` → the job's directory and prefix) in one changeset; 409 `already_exists` |
| GET/PUT/DELETE | `/api/v1/snapshots/jobs/{name}` | `SnapshotJobRequest` (name cannot change) | `SnapshotJobView` / `StagedChanges` (the task and the previous versions go with the job; snapshots on disk stay) |
| POST | `/api/v1/snapshots/jobs/preview-retention` | `RetentionPreviewRequest` {job} | `RetentionPreview` {keep, delete}: `btrbk prune -n` with that retention on the job's snapshots now (job saved or not) |
| GET/PUT | `/api/v1/snapshots/system` | `SystemSnapshots` {enabled, subvolume, snapshot_dir, prefix, keep} | `SystemSnapshots` (stages; the apt hook appears or goes on apply) |
| GET | `/api/v1/snapshots?share=&job=` | — | `Vec<SnapshotInfo>` {id, job, share?, path, created_ms, readonly, uuid?, parent_uuid?, received_uuid?, deletion: allowed\|latest\|writable\|not_subvolume, reason?}, newest first, of the applied jobs and system restore points |
| GET | `/api/v1/snapshots/{id}/browse?path=` | — | `SnapshotListing` {id, path, entries: [{name, kind, size_bytes, mtime_ms, mode, uid, gid, link?}], truncated} (10 000 entries max) |
| POST | `/api/v1/snapshots/{id}/restore` | `RestoreRequest` {path, mode: copy\|replace} | 202 (task `snapshot.restore`) |
| POST | `/api/v1/snapshots/{id}/rollback` | `RollbackRequest` {confirm: id} | 202 (task `snapshot.rollback`); 400 `confirmation_required`; 409 for system restore points unless synodeb (root rollback, see `docs/synodeb-integration.md`, S6) |
| GET | `/api/v1/snapshots/{id}/preview-rollback` | — | `RollbackPreview` {id, system, allowed, refusal?: no_boot_layer\|no_kernel\|no_modules\|not_readonly, kernel?, running_kernel?, modules, subvolume?}; no side effect. `no_boot_layer`: no hardware module boot layer (`rynas_module::BootLayer`) points the boot entry at a restore point's kernel, a root rollback is a console procedure |
| DELETE | `/api/v1/snapshots/{id}?confirm={id}` | — | 202 (task `snapshot.delete`); 400 `confirmation_required`; 409 `conflict` when the chain needs it |

Internal only (rynasd, never forwarded by rynas-web, called as root by `rynasctl`; the route
layer `require_root_peer` refuses with 403 `forbidden` any connection whose `SO_PEERCRED` uid is
not 0 — rynasd serves its socket with `ConnectInfo<PeerCred>`):
`POST /v1/scheduler/tasks/{id}/finished` (`TaskFinished` {invocation_id, result, exit_code,
exit_status}) and `POST /v1/scheduler/tasks/{id}/execute` (202 `TaskStarted`).

Task Scheduler (key `scheduler`, `SchedulerConfig` {tasks}):
- Every task is persistent systemd units written by the apply, with the managed header and hash
  (anti-drift, revisions, rollback like every file): `/etc/systemd/system/rynas-task-<id>.service`
  (`Type=exec`, `RuntimeMaxSec=` from `timeout_s`, `Nice=` (10 for user tasks), `IOWeight=`,
  `NoNewPrivileges=`/`PrivateTmp=` for user tasks, `SyslogIdentifier=`, and
  `ExecStopPost=+rynasctl task-finished <id>`, run as root whatever `User=`) and, unless the
  trigger is `manual`, `rynas-task-<id>.timer` (`OnCalendar=` + `Persistent=` +
  `RandomizedDelaySec=`, or `OnBootSec=` for the boot trigger; `AccuracySec=1s`; no `[Install]`).
  Enabling is a line in `/etc/systemd/system/timers.target.d/rynas-tasks.conf`
  (`Wants=` the timers of the enabled tasks), not a symlink made by `systemctl enable`, so it is
  covered by the same file machinery. Apply: files → `daemon-reload` → timers of removed or
  disabled tasks stopped (the service of a removed task too), changed timers restarted, the
  others started → health: every enabled timer active. Units, scripts and the drop-in of
  removed tasks are deleted (only files carrying rynas's header).
- User actions (`command`, `script`) run as `root` (administrators only, checked by rynasd from
  the forwarded user's system groups on create, change, delete, enable/disable and run now, and
  on restore of a revision, archive import and apply of a changeset that bring a different root
  task: `rynas_scheduler::domain::root_task_changes`) or a local user (uid 1000–59999; system accounts refused). The script
  (a command becomes `#!/bin/sh` + the line) is stored root:root 0700 in
  `/var/lib/rynas/tasks/<id>/run` and reaches the interpreter of its `#!` line on standard input
  (`StandardInput=file:`, opened by the service manager before it drops privileges): the user
  never gets a path to it and cannot change it. A root task whose command (or interpreter) is
  a file that someone other than root can change (the file or a folder above it) is refused.
- Internal actions (`snapshot`, `scrub`, `smart_test`, `recycle_cleanup`) run
  `rynasctl job run <id>`, which asks rynasd to start the task (`snapshot.run`, `btrfs.scrub`,
  `smart.test`, `smb.recycle-cleanup`: UPID, log, task panel), prints its log as it grows and
  exits with its result; SIGTERM (stop, `RuntimeMaxSec`) cancels it.
- Calendar: `OnCalendar=` checked for safe characters, then by `systemd-analyze calendar` (before
  staging and again by the apply); cron input (`m h dom mon dow`, `@daily`...) is converted
  (`@reboot` → boot trigger; a day of month and a day of week both restricted is refused: cron
  ORs them, systemd ANDs them).
- History (`/var/lib/rynas/scheduler.db`): one row per run from `$SERVICE_RESULT`, `$EXIT_CODE`,
  `$EXIT_STATUS`, `$INVOCATION_ID` and `ExecMainStartTimestamp`; the output (last 256 KiB of
  `journalctl -o cat _SYSTEMD_INVOCATION_ID=…`, read after `journalctl --sync`) is copied because
  the journal rotates. Caps: 100 runs per task, 5000 in all.
- Notifications per task (`never`, `on_failure` (default), `always`; `notify_output` attaches
  the last 3500 characters of the output) through `rynas_notify::Notifier` (the engine): a failed
  run fires the condition (error), a success resolves it (the engine ignores resolutions of
  conditions that are not open), with `always` a success that resolves nothing is a oneshot info
  (`…:ok`). Keys: `snapshot:job:<job>` (category backup, source `snapshots`) for snapshot jobs,
  `scheduler:task:<id>` (category task, source `scheduler`) for every other task; fields `task`,
  `result`, `exit_code`. The rynasd tasks started by units (`rynasctl job run`) run as user
  `scheduler` and are skipped by the generic `task:<type>` watch: one notification per run.
- Timer states (`systemctl list-timers -o json`) and running units are read at most every 2 s
  (single flight).

Snapshots (key `snapshots`, `SnapshotsConfig` {jobs, system}):
- One btrbk configuration per job, `/etc/rynas/btrbk/<job>.conf` (managed header; checked with
  `btrbk -c <candidate> config print` before it replaces the current one): `timestamp_format long`
  (names `<job>.YYYYMMDDTHHMM`, local time, `_N` on collision), `snapshot_create always`, GFS
  retention as `snapshot_preserve` (`24h 14d 8w 12m`; presets standard/light/long) and
  `snapshot_preserve_min` (`latest`, `all`, `<n>h|d|w|m|y`), and the job lock
  `/run/lock/rynas-btrbk-<job>.lock`, also taken by rynas's delete and rollback (a held lock
  refuses with 409). The volume is the common ancestor of the subvolume and the snapshot
  directory (default `<parent of subvolume>/.snapshots/<job>`, created 0755 by the apply). Health:
  the subvolume is a btrfs subvolume and the directory is on the same filesystem (fs UUID).
- A job runs as task `snapshot.run` (`btrbk -c … run --format=raw`, summary of what was
  created and deleted, failure on any btrbk error).
- Listing reads the snapshot directory and `BTRFS_IOC_GET_SUBVOL_INFO` (unprivileged); nothing
  reads or enables qgroups. Deletion verdicts: the newest read-only snapshot of a job stays
  (`latest`: base of the next incremental send), writable entries and non-subvolumes are never
  touched; delete re-checks on a fresh scan under the job lock, then `BTRFS_IOC_SNAP_DESTROY`.
- Browse: descriptor walk from the snapshot (`O_PATH|O_NOFOLLOW`, same `st_dev`, `.`/`..`
  refused), read-only snapshots only. Restore: the live side is walked the same way (folders
  deleted since the snapshot are recreated); files are reflinked (`FICLONE`), owner, mode,
  extended attributes (POSIX ACLs) and times kept; `copy` writes `<name> (restored YYYY-MM-DD
  HHMM)<.ext>`, `replace` first renames the current version
  `<name>.rynas-before-restore-<stamp>`; nothing is overwritten.
- Rollback of a share: writable snapshot of the snapshot next to the subvolume, then
  `rename(2)` of the live subvolume to `<name>.rynas-rollback-<stamp>` and of the copy to its name
  (`RENAME_NOREPLACE`, the first rename undone if the second fails); refused on mount points and
  across filesystems. NFS clients must mount again (new subvolume).
- SMB previous versions: a job created for a share with SMB stages `shadow_copy` {enabled, the
  job's directory, prefix = job name}, which renders `shadow_copy2` with `shadow:format =
  %Y%m%dT%H%M`, `shadow:localtime = yes` (step 8); deleting the job turns it off.
- System restore points (`system` {enabled, subvolume `/`, snapshot_dir `/.snapshots/rootfs`,
  prefix `rootfs`, keep 10}): `/etc/apt/apt.conf.d/80rynas-snapshot` (no header: apt.conf has no
  `#` comments; the hash detects drift) runs `rynasctl snapshot system --reason apt …` as
  `DPkg::Pre-Invoke` (`|| true`: never blocks apt; works without rynasd): one
  `BTRFS_IOC_SNAP_CREATE_V2` read-only snapshot with a btrbk name, skipped when the source
  generation is not newer than the latest restore point (btrbk's `onchange`) or that one is less
  than 2 minutes old; the oldest beyond `keep` are deleted; reasons in
  `/var/lib/rynas/snapshots/system.jsonl` (500 lines). Rolling the system back (rename of
  `@rootfs`, kernel on the DOM) is a task on synodeb (`synodeb-dom sync --root`, crate module
  `rynas_btrfs::rootfs`, details in `docs/synodeb-integration.md`, S6) and a console
  procedure elsewhere.

`rynasd.service` gains `CAP_SYS_ADMIN` (subvolume deletion by btrbk and rynas) and `CAP_FSETID`
(restored setgid bits). The package installs `rynasctl` and recommends `btrbk, btrfs-progs`.
Development: with `--dry-system` the units, scripts and btrbk files go below the dry root, unit
commands are only logged, subvolumes are probed on the host read-only; the read-only systemd
questions (calendar, timers, journal, crontabs) are answered by the host.

Real check: `packaging/test-snapshots.sh` (Debian 13 container with systemd, loop-mounted btrfs,
btrbk 0.32.6, the real units): share + snapshot job through the API, apply (units, timer active,
`shadow_copy2`), run now through `rynasctl job run`, history and output from `rynasctl
task-finished`, listing, browse, restore copy/replace, retention preview, chain-safe delete,
rollback, a user task as a local user (exit code and output recorded, failure notification),
calendar/cron previews, system timers and crontabs, restore point from the apt hook
(deduplicated), removal of job and tasks with no drift left.

## API v1: Synology hardware and power schedule (P4)
Crates `rynas-hw` (the provider interface: `HardwareProvider` for status, `HardwareControl` for
actions and model data), `rynas-hw-synology` (the `synology` provider over synodeb's synohwd,
D-SYNOPLUG, `../plugins/synology.md`; built into rynasd today, behind the traits so it can move
into a plugin process) and `rynas-power` (the power schedule, any Debian machine). Two
configuration domains applied by rynasd: `synology.hw` (order 50, registered only on a
Synology: synology-nas platform driver bound and DMI naming Synology or a known model) and
`power.schedule` (order 45, after the scheduler whose tasks run the shutdown rules). Types in
`rynas_api_types::synology` and `rynas_api_types::power`. No new error code. The Synology
endpoints are the module's routes (D-MODULE, `rynas_module::Module::router`): rynasd nests them
under `/v1/modules/synology` only where the module is detected, rynas-web forwards
`/api/v1/modules/<id>/…` by prefix without knowing any module; elsewhere they answer 404
`not_found` like any unknown path, and on synodeb without Synology hardware every route answers
404 `not_found`. `/api/v1/hardware` (provider-neutral summary, the provider of the module that
knows the machine, `rynas_web::hardware::detect`) and the bay identify are the core's. The UI
shows the Hardware › Synology tab only when the live provider is `synology` or
`GET /modules/synology/settings` answers (G8d).

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/modules/synology/status` | — | `SynologyStatus` {model, running, stale (older than 3 periods + 10 s, synohwd's rule), read_at_ms?, age_s?, period_s?, fan_duty_pct?, fan_fail, fans [{number, rpm?, alarm: ok\|stopped\|unknown}], status_led? off\|green\|green_blink\|orange\|orange_blink, bays [{number, present, powered, device?, temperature_c?, asleep}], cpu_temperature_c?, nvme/nic [{name, temperature_c?}], events [{at_ms?, text}], diagnostics {up_current_raw?}, boot?, led_schedule_supported, led_level_min?, led_level_max?, led_level_default?, led_level_now?, led_mode_now? normal\|reduced\|off (front LED chip, synodeb ≥ 4.8)} |
| GET/PUT | `/api/v1/modules/synology/settings` | `SynologySettings` {fan_mode low\|high\|full, fan_period_s 5–300, fan_min_duty_pct 0–99, hysteresis_c 0–20, thermal_shutdown, power_button poweroff\|log\|ignore, reset_button log\|ignore\|network_rescue, beep_on_start, beep_on_error, disk_leds, led_brightness 1–255, disk_standby_minutes 0–330, wol, ac_recovery, led_schedule "" or 168 × 0\|1\|2 (Sunday first, DSM format), led_reduced_level 0–7} | `SynologySettingsState` {applied, pending?, foreign_commands, platform {model, bays, fans, synohwd_running}}; PUT stages; 400 `invalid_value` out of range (synohwd would clamp: rynas refuses), also a reduced level outside the running synohwd's `led_level_min..=max` or a new schedule when its status has no LED chip, 400 `bad_request` for unknown choices (`command:` values are not accepted: reserved for rynas handlers) |
| POST | `/api/v1/modules/synology/beep` | `BeepRequest` {kind: short\|long} | 204: uP `2`/`3` through synohwd's FIFO; 503 `backend_unavailable` when synohwd does not read it, 409 `busy` when the FIFO is full |
| POST | `/api/v1/modules/synology/mute` | — | `MuteResult` {stopped_beep, acknowledged}: synohwd's beeps are single (no sequence it accepts stops one), so this acknowledges the open hardware notifications (their reminders stop) |
| POST | `/api/v1/hardware/bays/{n}/identify` | `IdentifyRequest` {duration_s? 1–300, default 30} or empty | provider-neutral (the hardware module's `HardwareControl`; 404 without one). Synology: 204, the bay's orange LED blinks (kernel `timer` trigger, else a software blink), green off, then both restored; 404 for a bay the model does not have |
| GET | `/api/v1/modules/synology/fan-curves` | — | `FanCurves` {model, active_mode (applied `fan_mode`), modes [{mode low\|high, curves [{class disk\|nvme\|cpu\|nic, points [{threshold_c, duty_pct}], shutdown_c}]}], readings [{class, sensor, temperature_c?}], duty_pct?}: DSM's tables of the model (synohwd's `FAN_TABLES`/`MODEL_FAN_TABLES`) |
| GET/PUT | `/api/v1/power/schedule` | `PowerSchedule` {rules [{id, enabled, action startup\|shutdown, days [mon…sun], hour, minute, shutdown? {when_busy wait\|skip\|force, max_wait_min 1–720, check_storage, check_tasks, check_smb}}]} | `PowerScheduleState` {applied, pending?, rules [{id, pending?, task_id?}], next: `PowerNext`}; PUT stages the schedule and the scheduler tasks `power-off-<rule>` in one changeset; 400 `invalid_value`: ids, empty days, startup and shutdown at the same minute, a shutdown less than 10 minutes after a startup (across midnight and the week too), more than 50 rules |
| GET | `/api/v1/power/next` | — | `PowerNext` {next_startup_ms?, next_startup_rule?, next_shutdown_ms?, next_shutdown_rule?, time_zone, wake {supported, rtc?, armed_ms?, last_armed? {at_ms, wake_ms?, rule?, rtc?, reason boot\|shutdown, error?}, warning?}, last_boot? {boot_ms, cause scheduled_startup\|watchdog\|unknown, previous_shutdown_clean?}} of the applied schedule |
| GET | `/api/v1/system/power` | — | `SystemPowerState` {pending? {action restart\|shutdown, at_ms, by?}, warnings [{code raid_busy\|scrub_running\|task_running\|scheduler_task_running\|smb_open_files\|apt_running\|ups_on_battery\|boot_trial, subject?, op?, pct?, n?, text}]}; administrators only (rynasd checks too). See `../phase2/restart-shutdown.md` |
| POST | `/api/v1/system/power` | `SystemPowerRequest` {action restart\|shutdown, delay_min 0\|1\|5} | 202 `TaskStarted` (`system.restart` / `system.shutdown`: warnings logged, audit line, notification `power.manual_*`, `shutdown -r\|-P now\|+N`); 400 `invalid_value` (`delay_min`) |
| POST | `/api/v1/system/power/cancel` | — | 204 (`shutdown -c`); 409 `nothing_pending` |
| GET | `/api/v1/pc-backups/setup` | — | `ReceiveSetup` {account, filter, host, ssh_port?, volumes, problems [{code sshd_missing\|sshd_allow_list\|sudo_missing\|btrfs_missing\|filter_missing, subject?}]}; administrators only (all `/pc-backups` routes). See `../research/pc-backup.md` §10 |
| POST | `/api/v1/pc-backups/plan/adopt` | `AdoptRequest` {path} | `AdoptPreview` {path, subvolume, list, groups, other_entries, suggested_device?}; read only; 400 `invalid_value` (`path`) |
| GET | `/api/v1/pc-backups/targets` | — | `[PcTarget]` {device, description, path, public_key, key_fingerprint?, alert_days, delete {allowed, keep_last, min_age_days}, adopted, created_at_ms, state ok\|late\|waiting\|missing, snapshots, incomplete, last_received_ms?, groups, usage?} |
| POST | `/api/v1/pc-backups/targets` | `PcTargetCreate` {device, description, path, adopt, public_key, alert_days, delete} | 202 `TaskStarted` (`pcbackup.create`: account + sudoers, new subvolume or adopted folder, `authorized_keys`); 400 `invalid_value` (`device`, `path`, `public_key`, `alert_days`, `delete.*`); 409 `already_exists` |
| GET / PUT / DELETE | `/api/v1/pc-backups/targets/{device}` | — / `PcTargetUpdate` {description, public_key, alert_days, delete} / — | `PcTargetDetail` {target, list, key_line?, key_commands, client_config, test_commands} (`?host=` for the PC's snippets) / 202 (`pcbackup.update`) / 202 (`pcbackup.remove`: key gone, folder kept); 404 `not_found` |
| POST | `/api/v1/pc-backups/targets/{device}/usage` | — | 202 (`pcbackup.usage`, `btrfs filesystem du -s`) |
| GET | `/api/v1/pc-backups/targets/{device}/browse?snapshot=&path=` | — | `SnapshotListing`; 400 `invalid_value` (`snapshot`, `path`) |
| POST | `/api/v1/pc-backups/targets/{device}/copy-out` | `CopyOutRequest` {snapshot, path, destination} | 202 (`pcbackup.copy_out`, reflink or copy, never overwrites); 400 `invalid_value` (`destination`, `path`) |

Internal only (rynasd, never forwarded, called as root by `rynasctl power scheduled-shutdown
<rule>`): `POST /v1/power/schedule/rules/{rule}/shutdown` (202 `TaskStarted`, task
`power.shutdown`).

Synology settings (`synology.hw` → `/etc/synohwd.toml`):
- Managed header and hash (drift), every key written in synohwd's order. Apply: `systemctl
  restart synohwd` (synohwd has no reload: SIGHUP stops it), health: unit active.
- Import (first read, drift import): synohwd's own lenient reading of the file (wrong type →
  default, integers clamped, unknown keys ignored); `command:` actions rynas does not manage are
  reported in `foreign_commands` and replaced by the default on the first apply. The reset
  button's `network_rescue` is `command:/usr/libexec/rynas/rynasctl net reset-button` (D-NET).
- `synology.hw` is the only writer of `/etc/synohwd.toml` and the owner of the reset-button
  network rescue (`network.rescue` reflects it read-only). At the start of rynasd it adopts
  the file once (network rescue on by default) and folds in the `# BEGIN rynas network` block
  older versions of `network.rescue` wrote (see the networking section).
- Actions use `HardwareControl`: FIFO commands are validated like synohwd's `check_cmd`
  (`V<nn>` or one of `2-9:;DRtuqpsrlk@AB`, never `1`/`C`), written with a non-blocking open.
  Bay LEDs are LED class devices (`syno:{green,orange}:diskN`), not uP commands; synohwd's LED
  loop only rewrites a bay when its wanted state changes, so identify can borrow and restore it.
- Notifications: the existing hardware watch of rynas-web (fan alarms, `fan_fail`, stale
  provider, temperatures) plus `hardware:thermal:<sensor>` (critical, "thermal shutdown
  imminent", 2 °C below the model's shutdown threshold, 3 °C hysteresis). synohwd's own "fan
  failure"/"fans ok again" forwards (synodeb `notify`, S1) map to the same key
  `hardware:fan_fail`, so one condition is one notification.

Power schedule (`power.schedule`):
- Times are the system time zone's wall clock. A time that does not exist on the
  spring-forward day happens right after the gap (02:30 → 03:30), one that exists twice on the
  fall-back day happens once, at the first occurrence (the same rule as systemd timers).
- Shutdown rules are internal scheduler tasks (`TaskAction::power_shutdown {rule}`, calendar
  `Mon,Tue *-*-* 23:30:00`, `Persistent=false`: a shutdown missed while off never happens at
  the next boot; `RuntimeMaxSec` = wait + 10 min). They are created, changed and deleted with
  the schedule (the scheduler API refuses to create one and answers 409 to a delete). The unit
  runs `rynasctl power scheduled-shutdown <rule>`, which follows rynasd's task
  `power.shutdown`: checks (RAID resync/recovery/reshape/check from `/proc/mdstat` and rynas
  scrubs; other running rynas tasks and scheduler units; SMB open files from `smbstatus -L
  --json`), then the rule's policy (`wait`: re-check every minute up to `max_wait_min`, then
  skip; `skip`; `force`) and `systemctl poweroff`. Skips and shutdowns are notifications
  (category system, source `power`).
- Startup: `rynas-power-wake.service` (generated, `Type=oneshot`, `RemainAfterExit=yes`,
  `After=synohwd.service` so it stops — and arms — before synohwd's shutdown hook reads the
  alarm and sends `s`/`r` to the microcontroller; pulled in by
  `multi-user.target.d/rynas-power.conf`). `ExecStart=`/`ExecStop=` run `rynasctl power
  arm-wake --boot|--shutdown`, which reads `/etc/rynas/power-schedule.json` (written by the
  apply), computes the next startup (at least 2 minutes ahead) and runs `rtcwake -d <rtc> -m
  no -t <epoch>` (`-m disable` without startup rules; sysfs `wakealarm` when `rtcwake` is
  missing). The RTC is the Synology S-35390A (`…-0030`) when present, else `rtc0`. It works
  without rynasd and never fails the unit; every stop arms (manual, scheduled, UPS), every
  start re-arms. The apply restarts the unit (new rules in the RTC at once).
- Boot record (`/var/lib/rynas/power/state.json`): a new `boot_id` at `--boot` records the
  boot time, its cause (watchdog `bootstatus` & 0x20; scheduled startup when the boot began
  between 1 min before and 10 min after the armed alarm; otherwise unknown: power button,
  WoL and AC recovery are indistinguishable) and whether the previous boot ended with a clean
  shutdown (`ExecStop=` ran; a restart within the same boot does not count).
- No RTC or no `wakealarm`: `wake.supported = false` with a warning; shutdown rules still work.

rynasd options: `--hw-root` (root of `/sys`, `/proc`, `/run/synohwd`, default `/`; also passed to
the generated wake unit as `rynasctl … --root`), hidden `--poweroff-command` (tests).
`rtcwake` comes with util-linux (essential on Debian 13).

Real check: `packaging/test-synology.sh` (Debian 13 container with systemd, a fake DS920+ tree
with LEDs and an RTC below `/srv/fakehw`, a fake synohwd unit reading the FIFO, an `rtcwake`
shim that only writes the fake RTC, an `smbstatus` shim, a fake poweroff; the host's RTC and
power state are never touched): settings applied and synohwd restarted, beeps through the FIFO,
identify and restore of the LEDs, fan curves, the schedule with its task and timer, the alarm
armed at apply and at every stop, the next times, a scheduled shutdown skipped while SMB files
are open and done when idle, removal with no drift left.
## API v1: UPS (P6)
D-UPS, choices in `../research/ups.md` (section "Generic backend"). Status from NUT's upsd, read
by rynas-web (non-root); policy settings, battery test, on-battery watcher and pre-shutdown stop
in rynasd (`crates/rynasd/src/ups.rs`). Backend (`UpsBackend`): **`nut`** wherever `/etc/nut`
has `nut.conf` or `upsd.users` (any Debian with the `nut` package), **`synodeb`** where
`/etc/synodeb/ups.conf` exists (`../synodeb-integration.md`, S2–S4), else `not_installed`. The
upsd protocol client is the crate `rynas-nut` (shared by both daemons).

| Method | Path | Body | Answer |
|---|---|---|---|
| GET | `/api/v1/ups` | — | `UpsStatus` {link connected\|not_configured\|unreachable, name?, flags [OL, OB, LB, CHRG, DISCHRG, RB, FSD…], battery_charge_pct?, battery_runtime_s?, load_pct?, input_voltage_v?, output_voltage_v?, manufacturer?, model?, serial?, beeper?, battery_date?, test_result?, driver?, commands [the `test.battery.*` instant commands of `LIST CMD`], read_at_ms}; served by rynas-web (last reading if younger than 5 s) |
| GET/PUT | `/api/v1/ups/settings` | `UpsSettings` {safemode_after_s? (60–86400, whole minutes; null = off), pre_safemode_timeout_s (10–900, default 120)} | `UpsSettingsState` {available, backend nut\|synodeb\|not_installed, applied, pending?}; PUT stages `ups.safemode`; 404 `not_found` when NUT is not installed |
| POST | `/api/v1/ups/test` | `BatteryTestRequest` {test quick\|deep\|stop} | 202 `TaskStarted` (task `ups.test`); 404 when NUT is not installed |
| GET/PUT | `/api/v1/ups/device` | `UpsDeviceConfig` {name (default `ups`), device `UpsDevice` {driver, port, options [{key, value?}]}, take_over [ups.conf sections]} | `UpsDeviceState` {backend, blocked? nut_missing\|synodeb_too_old, nut_version?, applied?, pending?, foreign [`UpsSection` {name, driver?, port?, conflict}], drivers_running, detect? (last `UpsDetectReport`)}; PUT stages `ups.device` (409 `conflict` with `field` `take_over` for a section in the way, 409 when blocked) |
| POST | `/api/v1/ups/detect` | `UpsDetectRequest` {device? (test only this one), stop_drivers} | 202 `TaskStarted` (task `ups.detect`; the report lands in `GET /api/v1/ups/device`); 409 `busy` while one runs |
| GET | `/api/v1/ups/drivers` | — | `Vec<NutDriver>` {name, manufacturers} (installed drivers, manufacturers from NUT's driver.list) |
| POST | `/api/v1/ups/plan/device` | `UpsDeviceConfig` | `UpsSetupPlan` {files [{path, before?, after, removed}], commands, conflicts} (no side effects, passwords masked) |

- upsd `127.0.0.1:3493` (`rynas-web --upsd`, `rynasd --upsd`), plain-text protocol `LIST UPS`,
  `LIST VAR <ups>`, `LIST CMD <ups>` (first UPS; a refused `LIST CMD` gives no commands), 2 s
  bound, no login, no child process. rynas-web polls every 5 s only while an `/events` stream
  listens (30 s while no UPS answers); `ups.status` on `/events` on change or every 60 s;
  `resync` `{"stream":"ups"}`.
- `ups.safemode` (registered everywhere):
  - nut: the rynas file `/etc/rynas/ups.conf` (whole file, header) with `SHUTDOWN_AFTER=<s>`
    (0 = off) and `STOP_TIMEOUT=<s>`. No reload: rynasd's watcher reads it at every power event.
  - synodeb: a `# BEGIN rynas ups` block of `/etc/synodeb/ups.conf` with `SAFEMODE_AFTER=<s>`
    and `PRE_SAFEMODE_TIMEOUT=<s>`; other assignments of the two keys outside the block are
    dropped, the rest stays synodeb's. No reload: `synodeb-ups` reads the file at every event.
  Import: the shell's view (last assignment wins). Low battery always ends in a shutdown
  (upsmon, as the admin configured it) or Safe Mode (synodeb, LOWBATT and OB+LB).
- Watcher (nut only; rynasd, every 5 s, 30 s while no UPS answers; never under `--dry-system`
  without `--upsd`): notifications "UPS on battery" / "UPS back on mains power" (category ups);
  OB for `SHUTDOWN_AFTER` → pre-shutdown stop (reason `waittimeup`, within `STOP_TIMEOUT`) then
  `upsmon -c fsd` (`systemctl poweroff` and an error notification if upsmon refuses); OB+LB or
  FSD → pre-shutdown stop only (`lowbatt`/`fsd`; upsmon shuts down by itself). Once per outage;
  an unanswered reading changes nothing.
- `ups.test`: the result before (`ups.test.result`, or `synodeb-ups test result`), the command,
  then the result every 10 s until it changes to an outcome (quick ≤ 5 min, deep ≤ 3 h); "error",
  "fail", "abort" fail the task; cancel sends stop.
  - nut: the UPS must list the command; `INSTCMD <ups> test.battery.start.quick|deep` /
    `test.battery.stop` after `USERNAME rynas` + `PASSWORD`. The upsd user is a managed block
    (`# BEGIN rynas ups`) of `/etc/nut/upsd.users`: `[rynas]`, `password`, one `instcmds` line per
    test, nothing else; mode and owner of the file kept. The password (32 letters/digits) is
    generated once into `/etc/rynas/nut-upsd.secret` (root 0600) and never logged. The block is
    written only at a test, when missing or changed (drift is put back), then upsd reloads
    (`systemctl reload nut-server.service`, else `upsd -c reload`). A `[rynas]` section outside
    the block fails the task instead of being overwritten.
  - synodeb: `synodeb-ups test quick|deep|stop`.
  - `--dry-system`: logged only, except the nut backend with an explicit `--upsd` (a test upsd:
    the block goes to the dry root, the reload is logged, INSTCMD is sent).
- Pre-shutdown stop (`stop_work`): critical notification "UPS shutdown: <reason>" (nut) or
  "Safe Mode: <reason>" (synodeb), running rynas tasks cancelled (not `config.*`), running
  scheduler units stopped, within the budget (synodeb hook 25 s, nut `STOP_TIMEOUT`).
  Internal only (root peer, `rynasctl ups safe-mode <reason>` from
  `/etc/synodeb/ups-pre-safemode.d/50-rynas`, a link the Synology module makes at the start of
  rynasd to `/usr/lib/rynas/modules/synology/ups-pre-safemode`, shipped in the package):
  `POST /v1/ups/safe-mode` {reason} → `SafeModeReport` {cancelled_tasks, stopped_units}. The
  hook is bounded to 40 s and exits 0 without doing anything when `rynasd.service` is not active.
- Scheduled battery test: a Task Scheduler command task as root running `rynasctl ups test
  quick` (or `deep`); `rynasctl ups test quick|deep|stop` starts `ups.test` through rynasd on
  either backend and follows it to its outcome (exit status = result).

## API v1: file manager (P5)
`/api/v1/files/<route>` ⇄ `/v1/files/<route>`, every method, SynoFile's route names and JSON
shapes; every signed-in user (rynasd runs each request in `rynas-files-worker` as the session's
Linux user, administrators keep `CAP_DAC_READ_SEARCH` only); bodies stream both ways (uploads up
to 80 MiB per request). Route table, security model, deviations and error codes:
`../plugins/files.md`. The worker crate has its own lock file:
`cargo test --manifest-path crates/rynas-files-worker/Cargo.toml`.

## API v1: network (P4)
Crate `rynas-net` (D-NET, design in `../research/network.md`), served by `rynasd`
(`crates/rynasd/src/network.rs`) and forwarded by `rynas-web` with the session user and the
**local address of the session's TCP connection** (internal header `x-rynas-local-addr`,
`getsockname` captured when the connection is accepted: `crates/rynas-web/src/conn.rs`). Types
in `rynas_api_types::network`. Session, CSRF and error rules of steps 4 and 6; no new error code.

Phase 1 scope: read-only inventory of everything; edits (IPv4 static/DHCP, gateway, DNS, MTU)
only on interfaces that systemd-networkd manages or may manage (Ethernet ports, not enslaved,
networkd running); ifupdown and NetworkManager interfaces are read-only until adopted.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/network/interfaces` | — | `Vec<NetInterface>` {name, index, kind, physical, mac, mtu, max_mtu, oper_state, carrier, speed_mbps, duplex, driver, addresses, dns, backend networkd\|ifupdown\|network_manager\|none, managed_by_rynas, editable, read_only_reason, config_file, applied, pending, ifupdown (stanzas), link_local_v4, management} |
| GET | `/api/v1/network/state` | — | `NetworkState` {hostname, services, routes, dns, trial?, lkg_revision?, last_rollback?, pending, management_interface?, ifupdown_adoptable, timeout_default_s/min/max, read_at_ms} |
| PUT | `/api/v1/network/interfaces/{name}` | `InterfaceConfig` {ipv4 dhcp\|static\|disabled, addresses, gateway?, dns, mtu?} | `NetInterface` (stages the session user's change; staging the applied value drops it); 400 `invalid_value`, 404, 409 `conflict` (read-only interface) |
| POST | `/api/v1/network/discard` | — | 204 |
| POST | `/api/v1/network/apply` | `NetApplyRequest` {timeout_s? (60–600, default 120), skip_test, comment} | 202 `NetApplyStarted` {task_id, token?, deadline_ms, timeout_s, candidate_urls, management_touched}; 400 `invalid_value` (validation, zero management paths, skip_test on the management interface), 409 `nothing_pending` |
| POST | `/api/v1/network/confirm` | `NetConfirmRequest` {token} | `NetConfirmed` {revision}; 400 `invalid_value` (wrong token); 409 `conflict` (not over the new network, too late), `nothing_pending`. A migration done at boot (`adopt_at_boot`) has no token: any administrator keeps it with an empty token, over an address of the NAS |
| POST | `/api/v1/network/rollback` | — | 202 `TaskStarted` (task `network.rollback`); also cancels an adoption planned for the next boot; 409 `nothing_pending` |
| GET / PUT | `/api/v1/network/rescue` | `RescueSettings` {mdns_service} | `RescueInfo` {mdns_name, mdns_active, link_local [{interface, mac, address, active}], reset_button?: `ResetButton` {module, configured, pending?}, settings, pending?} (PUT stages the key `network.rescue`; apply with `/config/apply`). The reset button exists only where a hardware module has one (`Module::reset_button`: on a Synology a reflection of `synology.hw`) and is read-only here; a PUT with `reset_button` answers 400 `invalid_value` |
| POST | `/api/v1/network/adopt` | `NetAdoptRequest` {mode now\|next_boot, timeout_s?, confirm} | `now`: 202 `NetApplyStarted` (task `network.adopt`); `next_boot`: 200 `NetAdoptScheduled` {revision, interfaces}; 400 `confirmation_required`, `invalid_value` (bonds/bridges/VLANs/static IPv6 are not migrated in phase 1) |
| GET | `/api/v1/network/revisions` | — | `Vec<NetRevision>` {id, ts_ms, author, comment, kind snapshot\|apply\|adopt\|rescue\|restore, interfaces, backend, good, lkg}, newest first |
| POST | `/api/v1/network/revisions/{n}/restore` | `NetRestoreRequest` {timeout_s?} | 202 `NetApplyStarted` (task `network.restore`, a timed trial like an apply) |
| POST | `/api/v1/network/diagnostics/ping` | `PingRequest` {host, count? 1–10, interface?, size?, dont_fragment} | `DiagnosticResult` {tool, ok, exit_code?, output (64 KiB), duration_ms, report?} |
| POST | `/api/v1/network/diagnostics/traceroute` | `TracerouteRequest` {host} | `DiagnosticResult` (`mtr --json` → `report`, else `traceroute`, else `tracepath`) |
| POST | `/api/v1/network/diagnostics/dns` | `DnsLookupRequest` {name, record_type?, server?} | `DiagnosticResult` (`resolvectl query`, `dig`, `getent`) |

Internal only (rynasd socket, called by `rynasctl`): `POST /v1/network/events/flush` (queued
notifications to the engine; rynasd also collects them at start and every minute).

The transaction (`rynas_net::txn`):
1. **Validate**: addresses are host addresses (not network/broadcast, loopback, link-local,
   multicast), prefix 8–32, gateway inside one of the interface's subnets and not its own
   address, DNS unicast, MTU 576..driver `max_mtu`, no subnet on two interfaces (rynas's and
   the others' static addresses), **at least one management path** (a port keeping an IPv4
   configuration). The interface of the session (from the local address) is the management
   interface: when the change touches it, or it is unknown (loopback, a proxy), the timed test
   cannot be skipped.
2. In the task: duplicate address check of new addresses (`arping -D`, iputils), **snapshot**
   of every governed file (rynas `.network` files, `/etc/network/interfaces`, the target's
   paths) with the observed MTU/static addresses/gateway of the interfaces, stored as a
   revision (`/var/lib/rynas/net/revisions/<n>.json`, files inline with SHA-256; 50 kept, the
   last known good and the trial's always); the target revision; **trial marker**
   `net/trial.json` (token stored as SHA-256, fsync).
3. **Rollback timer**: `systemd-run --unit=rynas-net-rollback --on-active=<N>s --collect
   rynasctl net rollback --reason timeout`, armed before any file changes (if it cannot be
   armed nothing changes).
4. **Files** written atomically (`.rynas-new` + rename + fsync of the directory):
   `/etc/systemd/network/05-rynas-<iface>.network` with the managed header. Prefix `05-`, not
   the `30-` of the research note: networkd applies the first matching file in lexical order,
   so rynas's must sort before the synodeb image's `10-lan10g`/`20-lan`/`99-fallback` (kept
   untouched). Every file has `LinkLocalAddressing=yes` and `IPv4LLStartAddress=169.254.x.y`
   (x = 1 + (mac[3] xor mac[4]) mod 254, y = mac[5]); DHCP files `UseHostname=no`.
   `networkctl reload` + `networkctl reconfigure` of the touched links.
5. **Verify** for up to 20 s: static addresses present, removed ones gone, MTU (the old MTU
   when the file sets none), default route through the gateway; **repair** what networkd
   leaves (it does not reset an MTU when `MTUBytes=` disappears — the netplan LP#1768802
   lesson; old addresses only once the new ones are up). A failed verification restores the
   snapshot at once (task error, rollback report `verify_failed`).
6. **Wait** for `POST /network/confirm`: the token, before the deadline, and — when the change
   touches the management interface or removes addresses — a local address that exists after
   the change (not loopback, not a removed address): the confirmation came over the network as
   it is now. Confirmed: timer stopped, the target becomes the last known good, the staged
   changes it applied are dropped. Not confirmed: the timer runs `rynasctl net rollback` (works
   without rynasd, under the store's `flock`), which restores the snapshot, repairs (also
   re-adding addresses and the default route by hand) and verifies again; report in
   `net/last-rollback.json` and a notification (`network:rollback`, warning; critical when the
   restored state cannot be verified). rynasd's task rolls back by itself only if the timer did
   not fire 20 s after the deadline (`--dry-system`: at the deadline, there is no timer).
   Cancelling the task rolls back too.

Safety net (units in `packaging/systemd/`, enabled by the package, not started at install):
- `rynas-net-guard.service` (`DefaultDependencies=no`, `Before=systemd-networkd.service
  networking.service`, only when `net/trial.json` or `net/boot-adopt.json` exists): an
  interrupted trial is undone **before networkd starts** (files and unit enablement only,
  report `boot_guard`); a migration planned for this boot is carried out (see below).
- `rynas-net-watchdog.timer` (5 min after boot) → `rynasctl net watchdog`: when ports have a
  cable but none has a routable IPv4 address with a gateway that answers ping (or no default
  route at all), the last known good revision is restored if the files differ from it (30 s to
  come up), then the **rescue configuration** (DHCP + link-local on every port, foreign
  `.netdev` suspended, ifupdown put aside); notification `network:watchdog` critical, long
  signal of the hardware module (`rynas_net::rescue::Feedback`, `Module::rescue_feedback`: on a
  Synology a long beep + orange LED through synohwd's FIFO; silent elsewhere). No cable:
  nothing.
- Reset button (Synology): **one owner**. `/etc/synohwd.toml` has a single writer, the
  `synology.hw` key (`PUT /api/v1/modules/synology/settings`, `reset_button:
  network_rescue` = `reset_button = "command:/usr/libexec/rynas/rynasctl net reset-button"`);
  `network.rescue` keeps the Avahi file and the link-local settings and only reflects the
  button (`RescueInfo.reset_button`, the module's `Module::reset_button`). Default on a Synology:
  network rescue on — at the first start of rynasd where `/etc/synohwd.toml` exists and
  `synology.hw` was never applied, rynas adopts the file (what synohwd runs with + network
  rescue; kept off if an older `network.rescue` had `reset_button: false`). Migration: up to
  this change `network.rescue` wrote the line as a `# BEGIN rynas network` block of the same
  file (two writers: drift on every apply, last writer wins); at start, a file that still has
  the block is rewritten by `synology.hw` with the button setting the block had, without the
  block (one revision, synohwd restarted once). The microcontroller reports one event per press,
  so the gesture is a **double press within 10 s** timed in `/run/rynas/net-reset-press`
  (presses closer than 1 s are one press): first press = signal `PressAgain` (Synology: short
  beep + orange blink), second = signal `Restoring` (long beep) + factory network (as the rescue configuration), the previous settings kept as a
  revision to restore.
- Avahi: `/etc/avahi/services/rynas.service` (`_rynas._tcp` and `_http._tcp`/`_https._tcp` on
  `--web-port`); avahi-daemon publishes `<host>.local` itself. `network.rescue` is applied with
  its defaults at the first start of rynasd (stored values of older revisions with
  `reset_button` are read without it).

ifupdown adoption: `/etc/network/interfaces` (with `source`/`source-directory` includes) is
parsed and shown read-only (method, address + netmask, gateway, dns-*, mtu, bond-*,
bridge_ports, vlan-raw-device, other options). "Let rynas manage" generates equivalent networkd
files with the **same addresses** (bonds, bridges, VLANs and static IPv6 refuse the migration
in phase 1); `interfaces` becomes loopback-only and the original goes to
`interfaces.rynas-bak`. Live: `systemctl stop networking` (ifdown with the old file), files,
flush, networking disabled, networkd (and resolved when installed) enabled and restarted, timed
confirmation; rollback: `interfaces` back, networkd stopped and disabled, `ifdown --force` +
flush, networking enabled and restarted, verified against the ifupdown file. `next_boot`: the
target is stored and the boot guard switches at the next boot with a 180 s marker; `rynasctl
net rollback --reason boot_check` then keeps it when a connected port has a routable address,
else brings ifupdown back.

The planned migration is shared state, not the planning browser's: `GET /network/state`
reports it as `trial` {kind `adopt_at_boot`, `deadline_ms` 0, author, `started_ms` = when it
was planned}, so every session shows "Migration planned for the next reboot" (who, when) with
Cancel until the reboot, even days later. After the boot the same trial has a deadline and
nobody holds a token (no browser applied it): the page says the network was migrated at this
boot, counts down, and offers **Keep it now** (`POST /network/confirm` with an empty token,
any administrator, over an address of the NAS) and **Go back to ifupdown** (`POST
/network/rollback`). Only the timer (`boot_check`, `timeout`) keeps the migration when a port
is routable; a manual rollback always restores ifupdown. (Found on the DS920+ on 2026-11-07: a
migration planned two days earlier ran at a reboot; the plan had never been shown again after
the page reload, the page showed the generic countdown with "confirm from the browser where the
change was applied", and its Roll back kept the migration instead of undoing it.)

`rynasctl net rollback|boot-guard|watchdog|reset-button|confirm --token|status` (root; the
store lock serializes them with rynasd). Diagnostics: argv only, validated host names (never a
leading `-`), timeouts (ping count×2+4 s, traceroute 45 s, DNS 10 s; rynas-web waits 60 s), 20
runs per minute for everybody (429 `rate_limited` + `Retry-After`), output capped at 64 KiB.

`rynasd.service` gains `CAP_NET_ADMIN`, `CAP_NET_RAW`, `AF_PACKET` and
`StateDirectory=rynas/net`; rynasd options `--web-port` (8080) and `--web-https`. The package
recommends `iputils-arping, iputils-ping, avahi-daemon` and suggests `mtr-tiny`. Not yet:
bonds/VLANs/bridges, IPv6 settings, hostname and global DNS (resolved drop-in), firewall
anti-lockout (phase 2 with nftables), network revisions in `.rynasbak`.

Real check: `packaging/test-network.sh` (Debian 13 container with systemd as PID 1 in its own
network namespace; veth pairs to a namespace playing the LAN; the test-only
`RYNAS_NET_TEST_PORTS` makes them count as ports): apply + confirm over the new address
(loopback refused), the management interface changed with a mandatory test, an unconfirmed
change rolled back by the systemd timer with MTU and address verified, a "reboot" during a
trial undone by the boot guard, ifupdown adopted, expired (ifupdown back) and adopted again,
diagnostics, deterministic link-local address active, reset-button double press with the web
interface reachable on link-local.

## API v1: system updates (P6)
Crate `rynas-updates` (parsers, the `updates.auto` domain), served by `rynasd`
(`crates/rynasd/src/updates.rs`, root: apt and `synodeb-dom` need it) and forwarded unchanged
by `rynas-web` (deadline 90 s: the first read after apt's state changed runs two simulations).
UI: System › Updates (`crates/rynas-ui/src/pages/updates.rs`). Ideas from DSM, Proxmox VE,
Cockpit and TrueNAS: `../research/updates.md`. Types in `rynas_api_types::updates`. No new
error codes.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/updates` | — | `UpdatesStatus` {apt, read_at_ms, lists_updated_ms?, packages: [`UpdatePackage` {name, arch, current?, candidate, origin, security}], removals, held, autoremovable, reboot_required, reboot_packages, error?, running_task?, unattended_installed} |
| POST | `/api/v1/updates/check` | — | 202 (task `updates.check`: `apt-get update`) |
| POST | `/api/v1/updates/install` | `InstallRequest` {packages (empty = all), confirm_removals} | 202 (task `updates.install`: `apt-get full-upgrade` or `install --only-upgrade -- <pkgs>`); 400 `confirmation_required` (`field` = `confirm_removals`) when a removal is not confirmed |
| POST | `/api/v1/updates/autoremove` | `AutoremoveRequest` {packages} | 202 (task `updates.autoremove`); 400 `confirmation_required` unless `packages` is exactly what autoremove would remove |
| POST | `/api/v1/updates/packages/{name}/changelog` | — | 202 (task `updates.changelog`: `apt-get changelog`, network; the text is the task log) |
| GET/PUT | `/api/v1/updates/auto` | `AutoUpdates` {mode off\|security\|all, hour, minute, window_min 10–360, reboot, reboot_hour, reboot_minute} | `AutoUpdatesState` {applied, pending?, unattended_installed} (PUT stages `updates.auto`) |
| GET | `/api/v1/updates/kernel[?refresh=1]` | — | `KernelStatus` {running, installed, boot_layer, boot?: `BootImage` {entries, default, fallback, trial none\|armed\|trying, running, outdated, kernel, kernel_old, free_bytes}, read_at_ms, error?}: `boot` is the boot device of a hardware module's boot layer (`rynas_module::BootLayer`; Synology: the DOM through `synodeb-dom status --json`), absent on a plain Debian |

Rules:
- **Cheap reads**: the list is parsed from `apt-get -s -o Debug::NoLocking=true full-upgrade`
  and `apt-get -s autoremove` (no python, no lock), cached until the mtime of
  `/var/lib/dpkg/status`, `/var/cache/apt/pkgcache.bin` or `/var/lib/apt/lists` changes or an
  `updates.*` task ends; one refresh at a time. Security = an origin from a `*-security` suite.
  `reboot-required(.pkgs)` is read on every GET.
- **Outside the sandbox**: rynasd has `ProtectSystem=yes` (`/usr`, `/boot` read-only: dpkg
  and `update-initramfs` would fail) and `MemoryMax=256M`, so every apt run that writes
  (`update`, upgrades, autoremove) goes through `systemd-run --wait --pipe --collect --quiet
  --unit=rynas-apt-<n>` (default service environment, `Nice=5`), its output streamed into the
  task log. Simulations and `apt-get changelog` stay inside rynasd (read-only). Cancelling or a
  timeout stops following the unit, not dpkg (apt's lock keeps a second run out). No
  `ReadWritePaths=/usr` in rynasd.service.
- **Tasks**: one `updates.*` task at a time (409 `busy`). apt runs with `-y`,
  `--force-confdef --force-confold` (local conffiles kept), `DEBIAN_FRONTEND=noninteractive`,
  `NEEDRESTART_MODE=l` (needrestart lists, never restarts rynasd mid-task),
  `APT_LISTCHANGES_FRONTEND=none`. Removals are checked at once against the cached simulation
  and again by the task against a fresh one (a new removal aborts it). **autoremove is never
  run automatically** (on synodeb it would remove the old synodeb kernel and `synology-ds923`).
- **`updates.auto`** (order 70) → `/etc/apt/apt.conf.d/53rynas-unattended-upgrades` (after
  Debian's `50` and synodeb's `52` file: `#clear`s and rewrites the origins, Unattended-Upgrade
  on/off, Automatic-Reboot(-Time, -WithUsers), every `Remove-Unused-*` false, MinimalSteps;
  synodeb's Package-Blacklist still applies; lists refreshed daily in every mode) and drop-ins
  of `apt-daily-upgrade.timer` (window: `OnCalendar` + `RandomizedDelaySec`) and
  `apt-daily.timer` (an hour before); apply = `daemon-reload` + restart of the installed
  timers. Never applied: imported as `all` when some `apt.conf.d` file turns
  unattended-upgrades on (synodeb does), else `off`. The `.deb` recommends
  `unattended-upgrades`.
- **Kernel / DOM**: `uname -r`, `/boot/vmlinuz-*` and `synodeb-dom status --json` (schema 1,
  only where it exists; root, mounts the DOM): cached 6 h, `refresh=1` at most once a minute.
  Read-only for now.
- **Notification**: a daily check (10 min after start, then every 24 h) emits `updates:available`
  (category `update`, `warning` with security updates, `notice` otherwise, `resolved` when
  nothing is left); every install/check task re-evaluates it. It runs `apt-get update` itself
  only when the lists are older than 36 h (apt's own timer normally keeps them fresh). apt
  state is on the root SSD: no data disk is touched.
- `--dry-system`: apt and synodeb-dom are never run; simulations and the DOM status come from
  the fixtures of `rynas-updates`, tasks only log the command lines.

Not done yet: restore point before installing (needs the root rollback work), reboot from the
page, needrestart's service list, apt-listchanges NEWS, real-machine test (`packaging/` container
test with a local apt repository).

## API v1: removable drives
Crates `rynas-removable` (discovery, mount options and names, holders, actions behind a
`Host` trait, SMB include) and `rynasd::removable` (watcher, tasks, the `removable`
configuration key, notifications). Design and research: `../research/removable-drives.md`.
Types in `rynas_api_types::removable`. Actions are immediate (not staged) and run as tasks
`removable.<verb>`; each request is first checked against a fresh reading (404 `not_found`,
409 `conflict` for a state that does not fit, 409 `busy` with the holding processes in the
message, 503 `backend_unavailable`). No new error code.

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/v1/removable` | — | `RemovableState` {drives: [`RemovableDrive` {name, dev_path, bus usb\|sd\|optical\|other, title, vendor, model, serial, size_bytes, removable_media, no_media, read_only, usb? {speed_mbps, version}, remove_method power_off\|eject\|release, busy, pending?, volumes: [`RemovableVolume` {name, dev_path, label, fs_type, uuid?, size_bytes, luks, unlocked, clear?, supported, mount_point?, read_only, total_bytes?, used_bytes?, avail_bytes?, share?, pending?}]}], mount_root, read_at_ms} |
| POST | `/api/v1/removable/refresh` | — | `RemovableState` after a forced reading (filesystem drivers read again) |
| GET | `/api/v1/removable/mounts` | — | `Vec<RemovableMount>` {removable: true, device, drive, device_name, label, fs_type, mount_point, read_only, remove_method}: the file manager's list |
| GET/PUT | `/api/v1/removable/settings` | `RemovableSettings` {auto_mount (true), read_only (false), notify_safe_removal (true), share {enabled (false), access read_write\|read_only, group ("rynas-admin")}} | `RemovableSettingsState` {applied, pending?, smb_available} (PUT stages `removable`; 400 `invalid_value` field `share.group`) |
| POST | `/api/v1/removable/volumes/{name}/mount` | optional `MountRequest` {read_only} | 202 (`removable.mount`); 409 locked LUKS, no driver, already mounted |
| POST | `/api/v1/removable/volumes/{name}/unmount` | — | 202 (`removable.unmount`); 409 `busy` |
| POST | `/api/v1/removable/volumes/{name}/unlock` | `UnlockVolumeRequest` {passphrase (1-512, stdin only), mount (true)} | 202 (`removable.unlock`); 409 not LUKS / already unlocked / no cryptsetup; a wrong passphrase fails the task |
| POST | `/api/v1/removable/volumes/{name}/lock` | — | 202 (`removable.lock`); 409 `busy` |
| GET | `/api/v1/removable/volumes/{name}/holders` | — | `Vec<VolumeHolder>` {pid, command, user} (empty when not mounted) |
| POST | `/api/v1/removable/drives/{name}/remove` | — | 202 (`removable.remove`): safe removal (unmount, lock, power off / eject / release); 409 `busy` |
| POST | `/api/v1/removable/mount-all` | — | 202 (`removable.mount-all`); 409 nothing to mount |
| POST | `/api/v1/removable/remove-all` | — | 202 (`removable.remove-all`); 409 `busy` before anything is unmounted |

Live updates: rynasd's task stream adds `removable` (data `RemovableState`, the latest on
connect, then on change) and `removable_resync` with `?removable=1`; rynas-web forwards them on
`/api/v1/events` as `removable.state` and `resync {"stream":"removable"}` (no browser polling).
Rules: readings every 2 s by a dedicated scanner thread below `--hw-root` (sysfs, udev
database, mount table: no command, no disk access); mounts through `systemd-mount` transient
units in PID 1's namespace (plain `mount` without systemd); never a lazy unmount; mounted
volumes below `/media/<label>` with names remembered per UUID
(`/var/lib/rynas/removable/names.json`); SMB shares of mounted volumes in
`/etc/samba/rynas-removable.conf` (included at the end of `smb.conf`), removed with their
connections before an unmount; `--dry-system` reads the drives and only logs the actions.
Tests: `crates/rynas-removable` (fixture sysfs trees, recording host), `crates/rynasd/tests/removable.rs`
(simulated host whose mount table and USB ports follow the commands). Not yet: per-device
overrides, filesystem check/repair, NTFS dirty-flag clearing, formatting.
