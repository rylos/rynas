# Mobile devices: pairing, device tokens, push (M1)

Status: M1a implemented 2026-11-24 (pairing, tokens, management, UI); M1b push follows. The protocol comes from the Android app's project
(`rynas-apk/docs/pairing-protocol.md` v1, background in its `research-security-architecture.md`
§1-3); this document records how rynas implements it and where it differs.

## 1. What and why

A phone pairs with the NAS by scanning a QR code shown to a signed-in user, who approves the
claim after comparing a 6-digit code shown on both screens. The phone keeps a non-exportable
P-256 key; its refresh token is useless without that key. Devices are listed, renamed and
revoked in rynas; revocation is immediate. Requests from the phone carry
`Authorization: Bearer <access token>`: no cookie, no CSRF. Notifications reach the phone only
by push (Web Push to its UnifiedPush endpoint, e.g. an ntfy server), never by polling.

## 2. Differences from the app's proposal (the app adapts)

| Topic | Proposal | rynas |
|---|---|---|
| Times in responses | RFC 3339 strings | Unix milliseconds in `*_ms` fields, like every rynas API (`expires_ms`, `claimed_ms`, `created_ms`, `last_seen_ms`); `ts` in signed messages stays Unix seconds |
| Error codes | names in the text | constants in `rynas_api_types::error_codes`: `pairing_invalid`, `pairing_rejected`, `pairing_expired`, `reauth_required`, `not_for_devices`, `device_revoked`, `refresh_invalid`, `rate_limited` (existing) |
| Who may pair | every user | the users who may sign in to the web interface today (administrators); other users when the login opens to them |
| TLS pins (`p`, `tls_pins`) | leaf key hashes | the SPKI hash of rynas's local CA first, then the leaf's (current and staged); the app accepts a pin anywhere in the chain, so leaf renewals change nothing. Omitted for URLs with a publicly trusted certificate |
| Storage | JSON under `/var/lib/rynas/devices/` | `<state>/devices/devices.json`, 0600, written atomically: metadata, public key, SHA-256 of the refresh token (current and previous, for the grace window), push subscription. Access tokens in memory only |
| Admin routes from a device | open question | a device acts as its user, with that user's role, and **no excluded route in v1** (restart and shutdown included; system updates too). The `not_for_devices` mechanism stays — one constant (`rynas_api_types::devices::NOT_FOR_DEVICES`, empty), the check in `require_session`, the 403 code — for per-device scopes later. Risk: a stolen unlocked phone acts as its user. Mitigations: revoke from the web interface (immediate), 15-minute access tokens, refresh bound to the device key (non-exportable), reuse detection revokes the device, every write of a device in the audit log with its id and name, a security notification for restarts and shutdowns, and the app's own biometric confirmation before destructive actions |

## 3. API

Public (no session):

- `GET /api/v1/ping` → `{sid, name, version, api: 1}`. `sid`: 16 random bytes b64u, made once
  in `<state>/nas-sid`.
- `POST /api/v1/devices/pairing/claim`, `POST /api/v1/devices/pairing/poll` (long poll ≤ 25 s),
  `POST /api/v1/auth/device/refresh`: rate limited with the login limiter; failures logged for
  fail2ban like login failures.

Session (cookie + CSRF for writes):

- `POST /api/v1/devices/pairing` (a sign-in or TOTP check in the last 10 minutes, else 403
  `reauth_required`), `GET /api/v1/devices/pairing/{id}`, `POST …/{id}/approve` `{name?}`,
  `POST …/{id}/reject`, `DELETE …/{id}`. At most 3 open pairings per user; code 32 bytes, 300 s,
  single use, only its SHA-256 stored. A paired device may open pairings too (a device acts as
  its user; its refresh counts as the recent check).
- `POST /api/v1/auth/reauth` `{password, totp?}` → 204: the password (and TOTP code) of the
  session's own user again; the session counts as freshly authenticated. Same rate limit, delay
  and error codes as the login (`invalid_credentials`, `totp_required`, `invalid_totp` keep the
  session).
- `GET /api/v1/devices` (own; administrators everyone's, with `user`), `PATCH /api/v1/devices/{id}`
  `{name}` (1-64 characters, trimmed), `DELETE /api/v1/devices/{id}`.

Bearer (device):

- `GET|PATCH|DELETE /api/v1/devices/self` (`GET` adds `tls_pins` and `endpoints`).
- `PUT /api/v1/devices/self/push` `{endpoint, p256dh, auth}` (https endpoint only, p256dh 65-byte
  uncompressed point, auth 16 bytes), `DELETE /api/v1/devices/self/push`.
- Every other route a web session may call, as the device's user with that user's role (no
  excluded route in v1: the `not_for_devices` list is empty). SSE, downloads with Range,
  thumbnails and uploads accept the bearer too. Every write of a device is written to the audit
  log (`rynas::audit`) with the device's id and name; a restart, shutdown or its cancellation
  also raises the security notification `device.power` (info).

Signed messages, QR fields, compare code, rotation with reuse detection and the 60 s grace
window: as in the proposal (§2-4 there). A refresh also checks that the Linux account still exists
and may sign in (PAM account management through rynasd); a removed or disabled account revokes
its devices. Devices without a refresh for 180 days expire. rynasd looks for both every 10
minutes too, so a removed account loses its devices without waiting for their next refresh.

Details of the implementation:

- Grace window: the old refresh token is accepted again for 60 s after a rotation, only with a
  valid signature of the device's key, and answered with a **fresh** pair (the server keeps no
  token in clear, so it cannot repeat the lost one); the token the lost answer carried is dead
  from then on. The mock NAS of the app behaves the same, and the app takes whatever pair comes
  back.
- A refresh of an account that may not sign in to the web interface (today: not an
  administrator) is refused with `refresh_invalid`; the device stays, and works again when the
  account gets the role back. Devices belong to system (PAM) accounts.
- Approved but never picked up: the tokens wait in rynas-web's memory until the phone's poll
  takes them (once), at most 2 minutes past the pairing's expiry; then the device is revoked
  (`unclaimed`).
- Limiter: the public routes count failures per client address (`device@<ip>` in the login
  limiter: a client guessing codes cannot lock out the others) and log
  `authentication failure for device@<ip> from <ip>` for fail2ban.

## 3a. Where things live (rynas-web and rynasd)

Bearer checks happen where `require_session` runs, without a round trip to rynasd per request:

- **rynas-web** (non-root, memory only, like the web sessions): the pairings (code and claim
  token hashes, the claim, the tokens waiting for the phone's poll) and the **access tokens**,
  which are device sessions in the same `SessionStore` as the web sessions (SHA-256 of the
  token, the device's id and name, 15 minutes fixed, at most two per device, never reachable
  with the cookie and never a CSRF token). So SSE streams, downloads, uploads and thumbnails
  accept a bearer unchanged, and a stream ends when its device session ends. The claim's
  signature is checked here (`rynas_auth::device`, `ring`). A restart of rynas-web ends the
  pairings and access tokens; the apps refresh.
- **rynasd** (root): the device store `<state>/devices/devices.json` (0600, atomic) and
  `<state>/nas-sid`, as the TOTP secrets live there; every refresh (signature with the stored
  key, account check through PAM account management and `/etc/shadow`, role, rotation, grace,
  reuse); the TLS pins and base URLs; the periodic expiry. rynasd never sees a token: rynas-web
  generates them and sends their SHA-256.
- Revocation: a revoke through rynas-web ends the device's sessions at once; revocations rynasd
  makes on its own (expiry, removed accounts, reuse) reach rynas-web's sessions within a minute
  (it compares its device sessions with `GET /v1/devices/ids`).
- Internal routes of rynasd: the table at the top of `crates/rynasd/src/devices.rs`.

## 4. Push

- rynas-notify gets the target kind `device`: one target per device with a push subscription,
  created and removed with it, named after the device, selectable in rules like the others;
  default rule: severity warning and above.
- Delivery: RFC 8291 encryption (`aes128gcm`, one record, rs 4096, padding delimiter 0x02),
  built from the `p256`, `hkdf`, `aes-gcm` crates already in the workspace and checked against
  the RFC 8291 test vector; `POST <endpoint>` with `Content-Encoding: aes128gcm`, `TTL: 86400`,
  `Urgency: high` for error and critical, `normal` otherwise. 404 or 410 drops the subscription;
  other failures follow the engine's retry like every target.
- Payload (UTF-8 JSON, at most 3 KiB, the body shortened to fit):
  `{v: 1, id, title, body, severity, category, source, ts_ms}`.
- No VAPID in v1 (ntfy does not need it); added if a distributor requires it.

## 5. Events

Notifications (category security) and SSE (`notification` events): `device.paired` (notice),
`device.renamed` (info), `device.revoked` (notice, `reason`: `user`, `self`, `account`,
`inactive`, `unclaimed`), `device.refresh_reuse` (warning: the device was revoked),
`device.power` (info: a restart, shutdown or cancellation asked from a device).

## 6. Web interface

Security › Mobile devices (`/security/devices`, also in the user menu and the command palette):
table (name editable inline, model, last seen, address, created),
Revoke with a confirmation, "Pair a device": QR with a countdown, then the claiming device's
name, model, address and compare code with Approve / Reject and an editable name. Administrators
see every user's devices. When the last sign-in is older than 10 minutes the pairing dialog
asks for the password (and TOTP code) first.

## 7. Phases

| Phase | Content |
|---|---|
| M1a | `ping`, sid, device store, pairing (web + phone), tokens, refresh with rotation, bearer in `require_session`, `not_for_devices`, device management API, UI page |
| M1b | push subscription, `device` target in rynas-notify, RFC 8291 delivery |
| later | share links from the app, per-device scopes |
