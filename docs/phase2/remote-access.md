# Phase 2: remote access (Tailscale)

Started on 2026-11-07. It builds on D-NET (Tailscale as a rescue path and a phase 2 plugin), D-EXPOSE (the `lan_vpn` access level admits the tailnet ranges only while `tailscale0` exists) and the remote access notes of `../research/network.md` §5 and `../research/web-access.md`. Decision row: **D-TAILSCALE** in `../phase0/05-decisions.md`.

The goal: from the web interface an administrator installs Tailscale, logs the NAS in, sees its state, sets the few options a server needs, logs out and uninstalls. None of these steps may silently cut the session that is doing them.

## 1. Research (2026-11-07)

**Package and repository.**
- The official Debian install puts the binary keyring `https://pkgs.tailscale.com/stable/debian/trixie.noarmor.gpg` in `/usr/share/keyrings/tailscale-archive-keyring.gpg`. It adds a source for `https://pkgs.tailscale.com/stable/debian trixie main` that names this keyring. The only package is `tailscale` ([pkgs](https://pkgs.tailscale.com/stable/)).
- The postinst restarts `tailscaled.service` but re-enables it only if it was enabled before ([postinst](https://raw.githubusercontent.com/tailscale/tailscale/main/release/deb/debian.postinst.sh)), so rynas runs `systemctl enable --now` itself.
- Key fingerprint: `2596A99EAAB33821893C0A79458CA832957F5868`. Tailscale does not publish it on its own pages; an open issue asks it to ([#13284](https://github.com/tailscale/tailscale/issues/13284)). The value here comes from package-manager logs that show the key.

**CLI and LocalAPI.**
- `tailscale set` changes only the flags it is given ([CLI](https://tailscale.com/kb/1080/cli)).
- `tailscale up` refuses unless every non-default flag is repeated; with `--reset`, flags it is not given go back to their defaults ([up.go](https://github.com/tailscale/tailscale/blob/main/cmd/tailscale/cli/up.go)).
- `tailscale login` takes the same flags but starts a new, empty profile every time ([login.go](https://github.com/tailscale/tailscale/blob/main/cmd/tailscale/cli/login.go)).
- `--auth-key=file:/path` reads the key from a file (up.go).
- LocalAPI: socket `/var/run/tailscale/tailscaled.sock`, `Host: local-tailscaled.sock`; requests that carry `Origin` or `Referer` are refused.
  - `GET /localapi/v0/status`, `GET /localapi/v0/prefs` and `login-interactive` are marked stable ([localapi.go](https://github.com/tailscale/tailscale/blob/main/ipn/localapi/localapi.go), [client/local](https://github.com/tailscale/tailscale/blob/main/client/local/local.go)).
  - The status carries `BackendState`, `AuthURL`, `TailscaleIPs`, `Self{DNSName, HostName, Online, KeyExpiry, AllowedIPs, PrimaryRoutes}`, `CurrentTailnet{Name, MagicDNSEnabled}`, `Peer`, `Health` and `ClientVersion{RunningLatest, LatestVersion}` ([ipnstate.go](https://github.com/tailscale/tailscale/blob/main/ipn/ipnstate/ipnstate.go), [tailcfg](https://github.com/tailscale/tailscale/blob/main/tailcfg/tailcfg.go)).
  - The prefs carry `RouteAll`, `CorpDNS`, `AdvertiseRoutes` (an exit node is `0.0.0.0/0` plus `::/0`), `RunSSH` and `ControlURL` ([prefs.go](https://github.com/tailscale/tailscale/blob/main/ipn/prefs.go)).
- `logout` expires the node key and disconnects; `down` only disconnects (CLI).

**Keys.**
- Node keys expire after 180 days by default. Expiry is disabled per device in the admin console only, and an expired key stops every connection ([key expiry](https://tailscale.com/kb/1028/key-expiry)).
- Auth keys can be one-off, reusable, ephemeral, pre-approved or tagged. "Key expiry for tagged devices is disabled by default", so a tagged auth key suits a server ([auth keys](https://tailscale.com/kb/1085/auth-keys)).

**Routing and DNS.**
- A subnet router or exit node needs `net.ipv4.ip_forward = 1` and `net.ipv6.conf.all.forwarding = 1`. Its routes must be approved in the admin console unless `autoApprovers` covers them ([subnets](https://tailscale.com/kb/1019/subnets)).
- Linux does not accept routes by default (prefs.go). Accepting them on a node inside the advertised LAN makes the LAN, the web interface and the shares unreachable ([TrueNAS forum](https://forums.truenas.com/t/cannot-connect-using-local-ip-after-installing-tailscale-app/56026), [Tailscale forum](https://forum.tailscale.com/t/activating-accept-routes-disables-local-ip-access/3897)).
- `--accept-dns` (on by default) makes tailscaled take over the resolver: through systemd-resolved, NetworkManager or resolvconf, or by rewriting `/etc/resolv.conf` when there is none ([Linux DNS](https://tailscale.com/kb/1188/linux-dns)). The TrueNAS app defaults it to off ([questions.yaml](https://raw.githubusercontent.com/truenas/apps/master/ix-dev/community/tailscale/questions.yaml)).
- UDP GRO forwarding (`ethtool -K <dev> rx-udp-gro-forwarding on rx-gro-list off`, Tailscale ≥ 1.54, kernel ≥ 6.2) speeds up a subnet router or exit node; it does not survive a reboot ([performance](https://tailscale.com/kb/1320/performance-best-practices)).

**Certificates and serve.**
- `tailscale cert` gets a Let's Encrypt certificate for `<host>.<tailnet>.ts.net` through DNS-01. It needs MagicDNS and HTTPS enabled in the admin console, and the name appears in Certificate Transparency logs ([HTTPS](https://tailscale.com/kb/1153/enabling-https)).
- `tailscale serve` publishes a local port on the tailnet.

**How other NAS systems present it.** These ideas shaped the design; the interface names no other product (G8c).
- TrueNAS app: hostname, auth key, "auth once", accept DNS off, accept routes off, exit node, advertised routes, and extra flags passed to `tailscale set`. It states that forwarding is required.
- Unraid plugin: login and reauthenticate buttons, tailnet name and address, the web interface's tailnet URLs, advertised routes with an "approve in the admin console" note, and a warning that certificate names are public ([Unraid](https://docs.unraid.net/unraid-os/system-administration/secure-your-server/tailscale/)).
- Synology package: userspace networking by default (other apps cannot use the tailnet until `tailscale configure-host`), no accept-routes, no Tailscale SSH, daily self-update, and the advice not to upgrade the system over Tailscale ([Synology](https://tailscale.com/kb/1131/synology)).
- OMV, Umbrel and CasaOS were not checked in this round.

## 2. Design

**Scope.** This is a system integration, like the Docker engine, not a container app: tailscaled runs on the host with a real `tailscale0`, so SMB, NFS, SSH and the web interface are reachable over the tailnet, and the web access ranges work.
- The code lives in `crates/rynas-tailscale` (repository and package, LocalAPI, CLI lines; it can move into a plugin process later, D-PLUG) and `crates/rynasd/src/tailscale.rs` (routes and tasks).
- Generic Debian/Ubuntu: the distribution and codename come from `/etc/os-release`. Nothing depends on a hardware module.

**State.** Tailscale keeps its own state in `/var/lib/tailscale`, so rynas stores no copy: the options are not a configuration key, and there is no pending bar or drift for them. The cost is no revision history of Tailscale options. The gain is no second source of truth that can disagree with tailscaled, for example when someone runs `tailscale set` by hand.

**Reads.** `GET /localapi/v0/status` and `/prefs` over the socket: one HTTP/1.0 request per connection with a 3 s timeout, cached for 2 s with a single-flight lock (S8). Installed means `/usr/bin/tailscale` exists below the configuration root. A missing package gets an install block when the system has no Debian/Ubuntu codename.

**Writes.** Every change is a task `tailscale.*` with its log and an audit line, through the CLI (its flags are the stable interface):
- **install**: `ca-certificates curl` → the key downloaded to `<state>/tailscale/archive-keyring.gpg.new` → key check → placed as the keyring with `install -D -m 0644` in a transient unit → deb822 `/etc/apt/sources.list.d/tailscale.sources` with the official keyring path (a hand-written source naming the same `Signed-By` does not conflict) → `apt-get update` → `apt-get install tailscale` → `systemctl enable --now tailscaled`.
  - apt runs in a transient unit outside rynasd's sandbox, like the Docker install, and a cancelled step stops its unit.
  - rynasd.service has `ProtectSystem=yes`, so `/usr` is read-only inside rynasd. The keyring path is under `/usr/share/keyrings`, so it is placed (and removed at uninstall) by `install`/`rm` in a transient unit, never written by rynasd. rynasd writes only `/etc` files itself (the source, the sysctl file) and refuses a path under `/usr` or `/efi`. A unit test ties this list to the unit file.
  - Key check: every OpenPGP packet of the download is read. The file must hold exactly one public key (tag 6), first, with fingerprint `2596A99E…7F5868`, plus only signatures, user IDs, subkeys and user attributes (tags 2, 13, 14, 17). A second key or any other packet is refused, because apt trusts every key of a `Signed-By` keyring. A refused file is deleted and never reaches the keyring.
  - The temporary file of a write in a `.d` folder is staged in its parent (`/etc/apt/.tailscale.sources.rynas-new`), so apt never sees it in `sources.list.d`.
  - A retry keeps a key that checks out.
- **login**: `tailscale up --reset --timeout=600s` with every option rynas edits spelled out. That keeps the current options, or the server defaults for a node that never logged in. The login server is the request's, else the current non-default one (Headscale). `tailscale login` is not used because it creates a new profile every time.
  - Interactive: the page reads the status every 5 s while the task runs and shows `AuthURL` as a link and a QR code. The CLI prints the same address into the task log.
  - Auth key: written to `<state>/tailscale/authkey` (0600, `create_new`), passed as `--auth-key=file:`, removed after. Never on a command line, in a log, in the status or in the audit line. A file left by a stop during a login is deleted when rynasd starts.
- **options**: `tailscale set --accept-routes --accept-dns --advertise-exit-node --advertise-routes --ssh`.
  - Subnets are validated: CIDR, host bits clear, not the default route, at most 16. A subnet may not overlap the tailnet ranges (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`; so `100.0.0.0/8` is refused too) or link-local (`169.254.0.0/16`, `fe80::/10`).
  - The current options are read from tailscaled first. If they cannot be read, the change is refused (503) and nothing changes, because a revert would otherwise restore defaults.
  - The forwarding sysctl `/etc/sysctl.d/60-rynas-tailscale.conf` is written and loaded while a subnet router or exit node is offered. It is removed otherwise, and the running value stays until a reboot because Docker needs forwarding too.
  - On systemd-networkd (D-NET) IPv6 forwarding does not stop router advertisements, since networkd handles them in user space.
- **logout**: `tailscale logout`.
- **uninstall**: `systemctl disable --now tailscaled` → `apt-get remove tailscale` → `rm -f` of the keyring in a transient unit → removal of the source and the sysctl file. With "forget this device": logout first, `apt-get purge`, and `/var/lib/tailscale` deleted. Without it the identity stays, and a new install comes back as the same device.

**Defaults of a server.**
- Accept routes **off**, the TrueNAS case above.
- Accept DNS **off**: the NAS keeps its own resolvers. tailscaled would otherwise take over `/etc/resolv.conf` or resolved, and local names and the NAS's DNS role would depend on the tailnet. MagicDNS names of other devices are rarely needed on the NAS itself.
- Tailscale SSH **off**: with it on, the tailnet policy decides who logs in, not the NAS's accounts.
- Exit node and subnet routes off, each with its "approve in the admin console" note. The routes still waiting for approval are listed: advertised but not in `AllowedIPs`/`PrimaryRoutes`.

**Never cut the session silently.**
- `via_tailscale`: rynas-web forwards the connection's local address (`x-rynas-local-addr`, as for network changes) and the client's address (`x-rynas-client-addr`). The client address is the TCP peer, or behind rynas-caddy the address from `X-Forwarded-For`, which rynas-web trusts only from a loopback peer. rynasd flags the session when either one is a tailnet address (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`). The page also flags it when the browser uses a `*.ts.net` name or one of the NAS's Tailscale addresses.
- Logout, uninstall and every option change ask first. When the page is open over the tailnet, the dialog says plainly that the session may end and names the other ways in (LAN, emergency port).
- Uninstall needs the typed word `tailscale`.
- An option change is a **timed trial**. Right after `tailscale set` (before the forwarding file), rynasd waits 90 s for `POST /tailscale/options/confirm`, then puts the previous options (and forwarding) back in a `tailscale.revert` task.
  - While a confirmation is pending, options, login and logout answer 409 `conflict`. The options restored are always the ones from before the first change. Uninstall is still allowed and drops the pending change.
  - A revert that finds another Tailscale task running waits for it and retries every 0.5 s; it is never dropped. A confirmation that arrives before the revert starts keeps the change.
  - The pending change (`{previous, deadline_ms}`) is kept in `<state>/tailscale/confirm.json`. After a rynasd restart, the window carries on, or the revert runs at once if the deadline has passed. The page confirms as soon as it reaches rynas after the task; that request is the proof the session survived. If the change hid the LAN or the tailnet path, nothing confirms and the old options return. A pending confirmation shows a banner with "Keep the new options".

**Web access.**
- The `lan_vpn` level already adds `100.64.0.0/10` and `fd7a:115c:a1e0::/48` while `tailscale0` exists, and host facts include that flag, so the proxy configuration is re-applied when Tailscale comes up or goes away (`web_access::model`).
- The Web access overview now has a line saying whether "This network and VPN" admits tailnet devices right now (with the NAS's tailnet name), linking to Remote access.
- `tailscale serve` is **not** used: rynas-caddy owns the routes, the access levels and SSO (D-PROXY), and a second front door would bypass them.
- Future note: the `<host>.<tailnet>.ts.net` name with a certificate from `tailscale cert` can become one more name source of the certificate store (D-PROXY already lists "Tailscale certificates"). It needs MagicDNS and HTTPS enabled in the admin console, and a warning that the name lands in public CT logs. It is not built in this wave.

**Interface.** Network › Remote access (`/network/remote-access`, a link card on the Network page under Web access, and the command palette):
- not installed: what it is, Install, the task in place;
- logged out: auth key (password field, cleared once sent), device name, coordination server; Log in; then the address with a QR code;
- connected: a status card (state, tailnet name, addresses, tailnet, devices online of total, key expiry with a warning within 14 days, version with "x.y available with the system updates", health) and an options card (switches with help texts, LAN subnets as checkboxes, pending approvals);
- Log out and Uninstall.

Updates come through apt like every package, and the System › Updates page lists them.

## 3. API

Every route needs an administrator, reads included (tailnet names and addresses).

| Method | Path | Body → answer |
|---|---|---|
| GET | `/api/v1/tailscale` | `TailscaleStatus` (installed, install_block, running, state, version, update_available, auth_url, tailnet, dns_name, magic_dns, hostname, addresses, online, peers, peers_online, key_expiry_ms, options, routes_pending_approval, lan_subnets, health, via_tailscale, task_id, confirm) |
| POST | `/api/v1/tailscale/install` | — → 202 `TaskStarted` (`tailscale.install`); 409 when installed, 501 `not_available` without a repository |
| POST | `/api/v1/tailscale/login` | `TailscaleLogin {auth_key?, hostname?, login_server?}` → 202 (`tailscale.login`); 400 `invalid_value` with `field` |
| POST | `/api/v1/tailscale/logout` | — → 202 (`tailscale.logout`) |
| PUT | `/api/v1/tailscale/options` | `TailscaleOptions` → 202 (`tailscale.options`), then a 90 s confirmation |
| POST | `/api/v1/tailscale/options/confirm` | — → 204; 409 `nothing_pending` |
| POST | `/api/v1/tailscale/uninstall` | `TailscaleUninstall {purge_state}` → 202 (`tailscale.uninstall`) |

Without Tailscale, the actions other than install answer 409 `tailscale_not_installed`. Login and options answer 503 `backend_unavailable` while tailscaled is down, 409 `busy` while another Tailscale task runs, and (with logout) 409 `conflict` while an option change waits for its confirmation. rynasd's `--tailscale-socket` defaults to `/run/tailscale/tailscaled.sock`. Under `--dry-system` only a socket given explicitly is read (the test double `tools/ui-audit/faketailscale.py`), and changes are only logged.

## 4. Open points

- The key fingerprint should be checked against an official Tailscale publication once one exists (#13284).
- The UDP GRO tweak for a subnet router or exit node (persisted through a networkd hook) is not built.
- No firewall profile yet: the nftables plugin (D-NET phase 2) should drop 100.64.0.0/10 on interfaces other than `tailscale0`.
- Headscale works through the coordination server field; it is not tested against a real server.
- Funnel and `tailscale serve` stay out (D-EXPOSE: nothing public as a side effect).
