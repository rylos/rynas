# Approving a PC's bare-metal restore from the phone: API for the Android app

A PC is dead and its disk wiped. Someone boots a live system on it and runs `rynas-agent
restore`. The TUI asks the NAS for an approval instead of a restore code typed from the web
interface; an administrator approves on the phone. This page is the contract for the app.
The design and the threat model are in `../research/pc-backup.md` §4.2.2.

Everything below is under `https://<nas>/api/v1`, with the device's access token as
`Authorization: Bearer <token>` (device tokens are administrators' sessions and are allowed on
every route here). Times are Unix milliseconds. Errors are the usual JSON
`{code, message, field?}`.

## What the phone does

1. A Web Push arrives with `action.kind == "pc_restore"` (below), or the user scans the QR code
   shown by the TUI, or opens the list of waiting requests.
2. Show the request: the PC **as the requester claims it** (`pc`), the PC whose backups will be
   read (`device`, or a picker when it is `null`), the address it came from (`from_ip`), the
   compare code (`482 913`), the certificate the TUI saw (`fingerprint`) and the time left. Tell
   the user to approve only a restore they started and whose screen shows the same code. After
   a QR scan, compare the scanned `code` with the request's `code` yourself and refuse loudly
   when they differ (someone in the middle of the TUI's connection).
3. Check the user locally (biometric prompt) before Approve.
4. `POST /pcbackup/restore-requests/{id}/approve` or `…/deny`. The TUI hears it within a second.

## QR code shown by the TUI

```
rynas-restore:v1?nas=<sid>&req=<id>&code=<6 digits>
```

- `nas`: the NAS' identity, the same `sid` as `GET /api/v1/ping` and the pairing QR code
  (22 characters, base64url): pick the paired NAS with that `sid`; unknown → "this phone is not
  paired with that NAS".
- `req`: the request id, 16 hex digits.
- `code`: the compare code the TUI computed, 6 digits (shown as `482 913`).
- Nothing in it is a secret. Values need no percent-decoding. Ignore unknown parameters; a
  different scheme or `v` is not ours.

## Push payload

The usual `PushPayload` (Web Push, aes128gcm), plus `action`:

```json
{
  "v": 1,
  "id": 1234,
  "title": "Restore of thinkpad waits for approval",
  "body": "A bare-metal restore from 192.0.2.10 asks to read the backups of thinkpad (code 482 913). Approve it only if you started it and the PC's screen shows the same code.",
  "severity": "info",
  "category": "pcbackup.restore_request",
  "source": "pcbackup",
  "ts_ms": 1795000000000,
  "action": {
    "kind": "pc_restore",
    "request": "0123456789abcdef",
    "pc": "thinkpad",
    "device": "laptop",
    "code": "482913",
    "from_ip": "192.0.2.10",
    "expires_ms": 1795000600000
  }
}
```

- Sent to every phone with push of every administrator, whatever their notification rules,
  with urgency `high`. `title` and `body` are in the device's push language (`lang` of
  `PUT /devices/self/push`).
- `id` is the notification's id on the NAS (`GET /notifications`): the stored notification has
  `category: "backup"`, `code: "pcbackup.restore_request"` and `fields`
  {`request`, `pc`, `from_ip`, `code` (`482 913`)}; it turns `resolved` when the request ends.
- `action.device` is `null` when the claim matched no PC or several: the approver picks one.
- `action` is absent from every other push. Older payload fields are unchanged.
- After the approval every administrator also gets the security notification
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
    "code": "482913",
    "from_ip": "192.0.2.10",
    "fingerprint": "AB:CD:…:EF",
    "created_ms": 1795000000000,
    "expires_ms": 1795000600000
  }
]
```

- `pc`: what the person at the PC typed (1–64 characters): show it as a claim.
- `device`: the NAS' PC whose name or host name the claim matches when exactly one does, else
  `null`.
- `fingerprint`: SHA-256 of the NAS certificate the TUI saw (`AA:BB:…`, upper case); empty when
  the TUI used plain HTTP (`--allow-http`), which deserves a warning.
- A request waits 10 minutes.

### `GET /pcbackup/devices`

The PCs a restore can read (for the picker when `device` is `null`):

```json
[
  { "device": "laptop", "description": "Office laptop", "hostname": "thinkpad", "files": true, "btrfs": false }
]
```

`files`: file backups (restic repository); `btrfs`: a folder of btrfs snapshots. The full
overview with states stays `GET /pcbackup` (`PcBackupOverview`, which also carries
`fingerprint`, the certificate the web interface serves).

### `POST /pcbackup/restore-requests/{id}/approve`

Body (JSON, may be empty or `{}` when the request has a `device`):

```json
{ "device": "laptop" }
```

| Answer | When |
|---|---|
| 204 | approved: the TUI receives a restore code for that PC |
| 400 `invalid_value`, `field: "device"` | no device given and the request matched none |
| 404 `not_found` | unknown request, or no such PC |
| 409 `conflict` | already approved, denied or withdrawn; or the PC has nothing to restore from |
| 410 `request_expired` | the 10 minutes are over |

What the approval gives is exactly a restore code (one-time, read-only, that PC only, valid
1 hour; it replaces any earlier restore code of that PC).

### `POST /pcbackup/restore-requests/{id}/deny`

No body. 204; 404, 409, 410 as above.

### Restore code made by hand (unchanged, the fallback)

`POST /pcbackup/devices/{device}/restore-code` → 201

```json
{ "code": "7K2M-Q9XD-4HTR", "expires_ms": 1795003600000, "device": "laptop" }
```

The TUI pins the certificate shown as `fingerprint` in `GET /pcbackup` (the web interface
shows both, with the command to run).

## Events (`GET /events`, server-sent, web interface and app)

- `pcbackup.restore_request`, data: one `RestoreRequestView` as in the list above.
- `pcbackup.restore_resolved`, data: `{"id": "0123456789abcdef", "outcome": "approved"}` with
  `outcome` one of `approved`, `denied`, `expired`, `canceled` (withdrawn by the TUI).
- `resync` with `{"stream":"pcbackup"}`: events were missed, read the list again.

Close an open approval screen on `pcbackup.restore_resolved` for its id (another administrator
decided, the TUI gave up, or the time ran out).

## The TUI's side (for reference, no session)

| Method | Path | Auth | Body | Answer |
|---|---|---|---|---|
| POST | `/pcbackup/agent/restore-requests` | none (≤ 4 KiB; 2 waiting per client, 16 in all, 6 new per client and 60 in all per 10 min) | `{pc, fingerprint}` | 201 `{id, poll_token, nonce, sid, expires_ms}`; 409 `certificate_mismatch`; 429 `rate_limited` |
| GET | `/pcbackup/agent/restore-requests/{id}?wait=<≤25>` | `Bearer <poll_token>` | — | `{state: pending\|approved\|denied\|expired\|canceled, expires_ms, approval?: {code, device, expires_ms}}`; 404 for an unknown id or a wrong token |
| DELETE | `/pcbackup/agent/restore-requests/{id}` | `Bearer <poll_token>` | — | 204 (withdrawn; an approved, unused code is revoked) |

Compare code: the first four bytes of SHA-256(`rynas-restore-v1\n<nonce>\n<fingerprint as
upper-case hex without separators>`), big-endian, modulo 1 000 000, six digits. The app does not
compute it: it compares the QR code's `code` with the request's.
