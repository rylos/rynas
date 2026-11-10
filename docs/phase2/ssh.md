# SSH service (D-SSH)

Services › SSH: OpenSSH's sshd managed from rynas on any Debian, without ever losing the way
in. Code: `crates/rynas-services/src/ssh/` (engine), `crates/rynasd/src/ssh.rs` (API),
`rynasctl ssh rollback`, UI `crates/rynas-ui/src/pages/ssh.rs`. Types:
`rynas_api_types::ssh`.

## 1. Research (2026-11-10)

What mature systems offer for their SSH service:

| System | Settings |
|---|---|
| TrueNAS (Services › SSH) | port, password login groups, allow password authentication, TCP port forwarding (advised off), weak ciphers (off), bind interfaces, SFTP log level, auxiliary parameters ("unsupported"); keys in the user form |
| OpenMediaVault | port, permit root login, password authentication, public key authentication, TCP forwarding, compression; shell access through the `ssh` group; keys in the user profile; SFTP for root and the group |
| Synology DSM (Terminal & SNMP) | on/off, port; only administrators may log in; cipher strength levels under Security |
| Unraid | on/off, port; root's authorized keys in the user page |
| Proxmox VE | no sshd settings in the UI (root keys shared by the cluster) |
| Cockpit (Accounts) | per-user authorized keys: list with comment and fingerprint, add, remove |

Hardening guidance: Mozilla's OpenSSH guideline (keys only, no root, change only what needs
changing: modern defaults are good), ssh-audit (checks algorithms; on OpenSSH 10 the defaults
already include the post-quantum `mlkem768x25519-sha256` key exchange and no DSA), `sshd -T`
for the effective values.

Facts checked on OpenSSH 10 with temporary files (`sshd -t -f`, `sshd -T -f`):

- most keywords keep the **first value read**; Debian's `sshd_config` has
  `Include /etc/ssh/sshd_config.d/*.conf` on its first line and the glob is read in lexical
  order, so the drop-in `00-rynas.conf` beats `50-cloud-init.conf` and the main file;
- `Port`, `AllowGroups`, `AllowUsers`, `DenyUsers`, `DenyGroups` **add up** across files: a
  `Port 22` uncommented in the main file is an extra port, an `AllowUsers` elsewhere narrows
  the list;
- a `Match` block ends with the file that opens it (the main file's lines after the
  `Include` stay global);
- a second `Subsystem sftp` is skipped, not an error.

Debian 13 (trixie, OpenSSH 10.0): `ssh.service` is the default; `ssh.socket` exists for
socket activation (one sshd per connection, the port then comes from the socket unit, not
from `sshd_config`). `systemctl reload ssh` re-executes sshd with the new configuration and
keeps every open session (`KillMode=process`).

## 2. Choices

- **One drop-in, the saved state**: `/etc/ssh/sshd_config.d/00-rynas.conf`, read before every
  other file (first value wins). No settings copy elsewhere: the page reads the file back; a
  hand edit shows as drift and the next save writes it again. The main file is never edited.
  Before the first save the page shows the running values (`sshd -T`), so adopting changes
  nothing.
- **Settings**: on/off; port; password login (also turns keyboard-interactive off: with PAM it
  is a second password path); root login no / keys only / yes; who may log in; port
  forwarding (TCP, stream-local, tunnels), agent forwarding, X11 forwarding. Always written:
  `PermitEmptyPasswords no`, `PubkeyAuthentication yes`. **No algorithm lists**: OpenSSH's
  defaults are the modern set and pinned lists age badly (they would drop new key exchanges).
  No free-text parameters: an administrator's own drop-in still works, and the page names any
  other file that overrides a setting or adds a port.
- **Who may log in**: every account, or only accounts with SSH access: `AllowGroups`
  `rynas-ssh rynas-sftp` (the Shell and SFTP levels of the user page), `rynas-pcbackup` (PC
  backup) and `rynas-cert-drop` (certificate drop folder) always, plus groups the
  administrator adds (`sudo` for accounts made outside rynas). The PC backup setup check reads
  the same lines (`receive::sshd_allows`), so it stays green.
- **SFTP only** for `rynas-sftp`: `Match Group rynas-sftp` with `ForceCommand internal-sftp`,
  no terminal, no forwarding (works with the `nologin` shell). No chroot yet: users see the
  folders their permissions allow, as over SMB.
- **Keys** on the SSH page, per account (root and the local users): list with type, bits,
  comment, SHA256 fingerprint, options flag, weak flag (DSA, RSA under 2048); add one line
  (no options, type must match the blob, no certificates, no weak keys); remove by
  fingerprint. Lines that are not keys stay as they are. Written without following links:
  `.ssh` and `authorized_keys` are opened with `O_NOFOLLOW` below the home's descriptor, a
  new file is written next to it and renamed over it; the home and `.ssh` must belong to the
  user and not be writable by others (`StrictModes`).
- **Host key fingerprints** (`SHA256:…`) to check on the first connection.
- **Logins of the last day** from the journal on request (`sshd`, `sshd-session`,
  `sshd-auth`): accepted, failed, unknown accounts, top failing addresses. Bans stay with
  fail2ban.

## 3. Applying without locking anyone out

1. Checks: port 1-65535 and free (from `/proc/net/tcp*`, sshd's own ports excepted); group
   names; **lockout guard**: if an administrator (root, `rynas-admin`, `sudo`) can open a
   shell now, one must still be able to afterwards (shell level, allowed by the list, a key
   or password login; root only as its setting allows). Password login off without an
   administrator key is refused with `ssh_lockout`; so is removing the last key of the only
   administrator who can log in. The page runs the same rule
   (`rynas_api_types::ssh::admin_can_log_in`) live and keeps Save off.
2. The candidate is checked **in place**: a copy of `sshd_config` whose `Include` lines name
   each file explicitly, the candidate where the drop-in goes; `sshd -t -f` on it. A main file
   that does not include `sshd_config.d/*.conf` is refused (the drop-in would not be read).
3. A change that can shut someone out (on/off, port, password, root, allow list, forwarding,
   socket activation) is a **trial**: `<state>/ssh/trial.json` keeps the previous drop-in and
   unit states, and `systemd-run --on-active=120s` arms `rynas-ssh-rollback.timer`
   (`rynasctl ssh rollback`, works without rynasd). rynasd also undoes an expired trial every
   5 s. Window 60-600 s (default 120).
4. Write the drop-in (new file + rename), bring the units to the wanted state (socket
   activation switched off, `ssh.service` enabled), **reload** (sessions stay).
5. sshd must be active and listen on the port within 5 s, else everything goes back at once.
6. The page shows the countdown and the command to test (`ssh -p N user@host`): **Keep**
   (`POST …/confirm` with the token only the author gets) or **Undo now**.

Every writer takes `<state>/ssh/lock` (`flock`), so rynasd and the timer never interleave.

## 4. API

| Method | Path | Body | Answer |
|---|---|---|---|
| GET | `/api/v1/services/ssh` | — | `SshState` {installed, managed, settings, running, ports, dropin, host_keys, accounts [{name, uid, admin, level, groups, keys}], problems [{code not_installed\|not_included\|overridden\|extra_ports\|drift\|socket_activation\|not_running, subject?}], trial? {token (author only), owner, deadline_ms, timeout_s, port}} |
| PUT | `/api/v1/services/ssh` | `SshApply` {settings, timeout_s?} | `SshApplied` {trial?}; 400 `invalid_value` (field `port`, `allow_groups`, `timeout_s`, `settings` = sshd refused), 409 `ssh_lockout`, 409 `conflict` (trial running, not installed, drop-in not included), 500 (sshd did not start: undone) |
| POST | `/api/v1/services/ssh/confirm` | {token} | 204 |
| POST | `/api/v1/services/ssh/rollback` | — | 204; 409 when nothing waits |
| GET | `/api/v1/services/ssh/logins` | — | `SshLogins` {events, failed, accepted, top_failed} |
| GET | `/api/v1/services/ssh/keys/{user}` | — | `SshKeyList` {user, keys, other_lines} |
| POST | `/api/v1/services/ssh/keys/{user}` | {key} | 201 `SshKey`; 400 `invalid_value` field `key`, 409 already there |
| POST | `/api/v1/services/ssh/keys/{user}/remove` | {fingerprint} | 204; 404; 409 `ssh_lockout` |

Administrators only (also in rynasd).

## 5. Tests and dry system

`crates/rynas-services/src/ssh/tests.rs`: a machine below a temporary root (Debian's
`sshd_config`, host keys, passwd/group, homes, `/proc/net/tcp`) and `FakeRunner`, which reads
the files the way sshd does and keeps unit states in memory. Nothing of the host is read or
run. `rynasd --dry-system` uses the same stand-in with a seeded dry root.

## 6. Open

- Real-hardware test on Debian 13: reload with a port change, socket activation switch,
  rollback timer from a lost session, journal identifiers of `sshd-session`.
- SFTP chroot for `rynas-sftp` (needs a root-owned path; the homes folder qualifies, shares
  would need bind mounts).
- Users managing their own keys (today administrators only), login notifications.
- Configuration export (`.rynasbak`) does not carry the drop-in yet.
