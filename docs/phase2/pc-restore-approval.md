# Approving a PC's bare-metal restore from the phone: API for the Android app

A PC is dead and its disk wiped. Someone boots a live system on it and runs `rynas-agent
restore`. The TUI asks the NAS for an approval instead of a restore code typed from the web
interface; an administrator approves on the phone **by typing the six digits the PC's screen
shows** (or by scanning the QR code on that screen). This page is the contract for the app. The
design and the threat model are in `../research/pc-backup.md` §4.2.2.

Everything below is under `https://<nas>/api/v1`, with the device's access token as
`Authorization: Bearer <token>` (device tokens are administrators' sessions and are allowed on
every route here). Times are Unix milliseconds. Errors are the usual JSON
`{code, message, field?}`.

## What the phone does

1. A Web Push arrives with `action.kind == "pc_restore"` (below), or the user scans the QR code
   shown by the TUI, or opens the list of waiting requests.
2. Show the request: the PC **as the requester claims it** (`pc`, label it as a claim), the
   address it came from (`from_ip`), the time left, and a PC picker for the backups it may read,
   preselected with `device` when not `null` and labelled "suggested from the name typed, not a
   check".
3. Red warnings, prominent:
   - `tls == false`: "This request came over plain HTTP: no certificate binds its code; someone
     on the network may stand in between."
   - `same_pc > 0`: "{same_pc} other requests for this PC wait too: only one can be the person
     you know."
4. A field for the **six digits on the PC's screen** (spaces allowed). The NAS never sends the
   code to the app: not in the push, the list or the events. After a QR scan, fill the field
   with the QR code's `code` (it comes from the PC's own screen) and open that request.
5. Check the user locally (biometric prompt), then `POST /pcbackup/restore-requests/{id}/approve`
   with `{code, device}`. On `code_mismatch` say the code is wrong and let the user try again
   (the third wrong code denies the request; the answer then says so and
   `pcbackup.restore_resolved` arrives with `denied`).
6. Or Deny, or "Deny and block this address" (`{"block": true}`): no new requests from that
   address for an hour.
7. Close the screen on `pcbackup.restore_resolved` for its id (another administrator decided,
   the TUI gave up, the time ran out) or on 404 / 409 / 410.

## QR code shown by the TUI

```
rynas-restore:v1?nas=<sid>&req=<id>&code=<6 digits>
```

- `nas`: the NAS' identity, the same `sid` as `GET /api/v1/ping` and the pairing QR code
  (22 characters, base64url): pick the paired NAS with that `sid`; unknown → "this phone is not
  paired with that NAS".
- `req`: the request id, 16 hex digits.
- `code`: the six digits the TUI computed and shows (as `482 913`). Put them in the code field;
  the NAS checks them.
- Nothing in it is a credential. Values need no percent-decoding. Ignore unknown parameters; a
  different scheme or `v` is not ours.

## Push payload

The usual `PushPayload` (Web Push, aes128gcm), plus `action`:

```json
{
  "v": 1,
  "id": 1234,
  "title": "Restore of thinkpad waits for approval",
  "body": "A bare-metal restore from 192.168.1.37 asks to read the backups of thinkpad. Approve it only if you started it, typing the code the PC's screen shows.",
  "severity": "info",
  "category": "pcbackup.restore_request",
  "source": "pcbackup",
  "ts_ms": 1795000000000,
  "action": {
    "kind": "pc_restore",
    "request": "0123456789abcdef",
    "pc": "thinkpad",
    "device": "laptop",
    "from_ip": "192.168.1.37",
    "tls": true,
    "same_pc": 0,
    "expires_ms": 1795000600000
  }
}
```

- Sent to every phone with push of every administrator, whatever their notification rules,
  with urgency `high`; while other requests already wait, at most one push every 5 minutes (the
  list has them all). `title` and `body` are in the device's push language (`lang` of
  `PUT /devices/self/push`); over plain HTTP the title says so.
- `id` is the notification's id on the NAS (`GET /notifications`). The stored notification is
  quiet (no copy to any other channel): `category: "backup"`, `code:
  "pcbackup.restore_request"` (`"pcbackup.restore_request_http"` over plain HTTP), `fields`
  {`request`, `pc`, `from_ip`}; it turns `resolved` when the request ends.
- `action.device`: the PC the claimed name matches when exactly one does, else `null`.
- `action` is absent from every other push. Older payload fields are unchanged.
- After an approval every administrator also gets the security notification
  `pcbackup.restore_approved` (warning, normal rules): "Restore of {device} approved by {user}".

## Routes for the app

### `GET /pcbackup/restore-requests`

The requests waiting for a decision, oldest first:

```json
[
  {
    "id": "0123456789abcdef",
    "pc": "thinkpad",
    "device": "laptop",
    "from_ip": "192.168.1.37",
    "tls": true,
    "same_pc": 0,
    "created_ms": 1795000000000,
    "expires_ms": 1795000600000
  }
]
```

- `pc`: what the person at the PC typed (1–64 characters): show it as a claim.
- `device`: the NAS' PC whose name or host name the claim matches when exactly one does, else
  `null`. A suggestion, not a check.
- `tls`: `false` → the red plain-HTTP warning.
- `same_pc`: other waiting requests for the same PC (same match or same claim) → red warning.
- A request waits 10 minutes.

### `GET /pcbackup/devices`

The PCs a restore can read (the picker):

```json
[
  { "device": "laptop", "description": "Office laptop", "hostname": "thinkpad", "files": true, "btrfs": false }
]
```

`files`: file backups (restic repository); `btrfs`: a folder of btrfs snapshots.

### `POST /pcbackup/restore-requests/{id}/approve`

```json
{ "code": "482 913", "device": "laptop" }
```

`code` is required (spaces ignored); `device` may be left out only when the request has one.

| Answer | When |
|---|---|
| 204 | approved: the TUI receives a restore code for that PC |
| 400 `code_mismatch`, `field: "code"` | not the code on the PC's screen (the third one denies the request) |
| 400 `invalid_value`, `field: "code"` | no code |
| 400 `invalid_value`, `field: "device"` | no device given and the request matched none |
| 404 `not_found` | unknown request, or no such PC |
| 409 `conflict` | already approved, denied or withdrawn, another approval running, or the PC has nothing to restore from |
| 410 `request_expired` | the 10 minutes are over |

What the approval gives is exactly a restore code (one-time, read-only, that PC only, valid
1 hour; it replaces any earlier restore code of that PC).

### `POST /pcbackup/restore-requests/{id}/deny`

Body optional: `{"block": true}` also refuses new requests from that address (its IPv6 /64)
for an hour. 204; 404, 409, 410 as above.

### Restore code made by hand (unchanged, the fallback)

`POST /pcbackup/devices/{device}/restore-code` → 201

```json
{ "code": "7K2M-Q9XD-4HTR", "expires_ms": 1795003600000, "device": "laptop" }
```

The TUI pins the certificate served for the address it uses: `GET /pcbackup` (`PcBackupOverview`) has `fingerprint` (the interface's own, port 8443) and `served_certificates` [{fingerprint, names}] (every certificate the NAS presents, also rynas-caddy's for proxied names); show the one whose `names` cover the NAS address the user will type, else `fingerprint`.

### Settings (web interface; the app may show them)

`GET /pcbackup` → `settings` has two new fields, saved with `PUT /pcbackup/settings`:

- `restore_request_sources`: addresses or CIDR ranges restore requests may come from; absent or
  empty = private/LAN ranges, Tailscale (100.64.0.0/10, fd7a:…), link-local and loopback.
- `restore_requests_over_http`: `true` accepts requests over plain HTTP (absent = off).

## Events (`GET /events`, server-sent)

- `pcbackup.restore_request`, data: one request as in the list above.
- `pcbackup.restore_resolved`, data: `{"id": "0123456789abcdef", "outcome": "approved"}` with
  `outcome` one of `approved`, `denied`, `expired`, `canceled` (withdrawn by the TUI).
- `resync` with `{"stream":"pcbackup"}`: events were missed, read the list again.

## The TUI's side (for reference, no session)

| Step | Method | Path | Auth | Body | Answer |
|---|---|---|---|---|---|
| 1 | POST | `/pcbackup/agent/restore-requests` | none (allowed addresses only; ≤ 4 KiB; 2 waiting per client, 16 in all, 6 new per client and 60 in all per 10 min) | `{pc, fingerprint, commit}` | 201 `{id, poll_token, nonce, sid, expires_ms}`; 403 address not allowed, blocked, or plain HTTP off; 400 `fingerprint` missing over TLS; 409 `certificate_mismatch` (not a certificate this NAS presents); 429 `rate_limited`. Works directly on the HTTPS port and through rynas-caddy (HTTPS there counts as TLS) |
| 2 | POST | `/pcbackup/agent/restore-requests/{id}/reveal` | `Bearer <poll_token>` | `{secret}` | 204 (now shown to administrators); 400 secret does not match (request dropped); 409 already revealed |
| 3 | GET | `/pcbackup/agent/restore-requests/{id}?wait=<≤25>` | `Bearer <poll_token>` | — | `{state: pending\|approved\|denied\|expired\|canceled, expires_ms, approval?: {code, device, expires_ms}}`; 404 unknown id or wrong token; 429 a third poll at once |
| — | DELETE | `/pcbackup/agent/restore-requests/{id}` | `Bearer <poll_token>` | — | 204 (withdrawn; an issued code is revoked) |

- `secret`: 256 random bits as 64 hex digits; `commit` = hex SHA-256 of
  `rynas-restore-commit-v1\n<secret>`.
- Compare code: the first four bytes of SHA-256(`rynas-restore-v2\n<secret>\n<nonce>\n<certificate
  as upper-case hex without separators>`), big-endian, modulo 1 000 000, six digits. The NAS uses
  its own certificate (empty over plain HTTP), the TUI the one it saw. The app does not compute
  it.
- A request not revealed within 60 s is dropped.
