# Mobile devices: pairing, device tokens, push (M1)

Status: design, 2026-11-24. The protocol comes from the Android app's project
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
| Admin routes from a device | open question | a device acts with its user's role; refused with 403 `not_for_devices`: pairings, other users' devices, TOTP, passwords, user and group management, configuration apply/discard/restore, system power and updates, storage changes that destroy data, Docker engine settings. The list is one constant, extended when scopes come |

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
  single use, only its SHA-256 stored.
- `GET /api/v1/devices` (own; administrators everyone's, with `user`), `PATCH /api/v1/devices/{id}`
  `{name}` (1-64 characters, trimmed), `DELETE /api/v1/devices/{id}`.

Bearer (device):

- `GET|PATCH|DELETE /api/v1/devices/self` (`GET` adds `tls_pins` and `endpoints`).
- `PUT /api/v1/devices/self/push` `{endpoint, p256dh, auth}` (https endpoint only, p256dh 65-byte
  uncompressed point, auth 16 bytes), `DELETE /api/v1/devices/self/push`.
- Every other route a web session may call, as the device's user, except the `not_for_devices`
  list. SSE, downloads with Range, thumbnails and uploads accept the bearer too.

Signed messages, QR fields, compare code, rotation with reuse detection and the 60 s grace
window: as in the proposal (§2-4 there). A refresh also checks that the Linux account still exists
and may sign in (PAM account management through rynasd); a removed or disabled account revokes
its devices. Devices without a refresh for 180 days expire.

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

Notifications (category security) and SSE: `device.paired`, `device.renamed`, `device.revoked`,
`device.refresh_reuse` (warning: the device was revoked).

## 6. Web interface

Settings › Mobile devices: table (name editable inline, model, last seen, address, created),
Revoke with a confirmation, "Pair a device": QR with a countdown, then the claiming device's
name, model, address and compare code with Approve / Reject and an editable name. Administrators
see every user's devices.

## 7. Phases

| Phase | Content |
|---|---|
| M1a | `ping`, sid, device store, pairing (web + phone), tokens, refresh with rotation, bearer in `require_session`, `not_for_devices`, device management API, UI page |
| M1b | push subscription, `device` target in rynas-notify, RFC 8291 delivery |
| later | share links from the app, per-device scopes |
