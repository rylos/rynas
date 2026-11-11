# rynas: UPS page (P6)

Short follow-up of `notifications-ups-updates.md` §2 (D-UPS), for the first UPS page on top of
synodeb. Sources: the DSM help (Hardware & Power › UPS), TrueNAS SCALE (System › Services › UPS
and the dashboard widget), Unraid (Settings › UPS, apcupsd/NUT plugin), NUT's `upsstats.cgi`,
`upsset.cgi` and `upsimage.cgi`.

## What the others show and allow

| | Status | Policy | Test | Other |
|---|---|---|---|---|
| DSM | on/off battery, model, battery %, runtime | "Safe Mode after N minutes" or "until low battery"; network UPS server with allowed IPs | none in the UI | shutdown UPS when the NAS enters Safe Mode |
| TrueNAS | dashboard widget: status, charge, runtime, load | shutdown mode "UPS goes on battery" (after N s) or "UPS reaches low battery"; shutdown command; no hook order | none in the UI (shell `upscmd`) | driver picker, remote monitor, e-mail on events |
| Unraid | charge, runtime, load, nominal power, model | "battery level", "runtime left" or "time on battery" thresholds, whichever first | apcupsd self-test only via CLI | kill UPS power, network server |
| NUT CGI | `upsstats`: status flags decoded, charge/load/voltage bars, battery date, model | — (`upsset` edits variables, needs upsd users) | instant commands list | `upsimage` bar graphs |

## Picked

- **Status card** like NUT's `upsstats`: overall state pill from the flags (FSD > OB+LB >
  LB > OB > RB > OL), every flag decoded as its own pill (icon + text), charge and load as bars,
  runtime, input/output voltage, model, manufacturer, serial, driver, beeper and battery date
  when the UPS reports them. Read straight from upsd's text protocol (no `upsc` process), every
  5 s only while a browser listens; never touches a disk. No chart of long ranges yet: the
  persistent history belongs to D-MONITOR's RRD.
- **Safe Mode policy**: DSM's "after N minutes" timer, but shown as an *extra* trigger. Low
  battery (LB) and a critical UPS (OB+LB) always start Safe Mode, whichever comes first
  (Unraid's "whichever first" wording, which is also synodeb's behaviour). Marco explicitly
  wants low battery to trigger it, so it is not a choice in the form, only a statement.
- **Ordered stop before Safe Mode** (none of the others has it): synodeb's
  `ups-pre-safemode.d` hook asks rynasd to cancel its tasks and stop running scheduler units,
  bounded well within `PRE_SAFEMODE_TIMEOUT` (editable, as DSM-style hooks need time for VMs
  and apps later).
- **Battery test** in the UI (missing in DSM and TrueNAS): quick, deep (confirm dialog: LB still
  starts Safe Mode, someone must be present), stop; the task follows `ups.test.result` to an
  outcome. Scheduling: a Task Scheduler command task (`synodeb-ups test quick` as root) for now;
  a dedicated task action can come with the `nut` plugin.
- **Recent UPS events** from the notification center (category `ups`; synodeb's own UPS
  notifications arrive there through the S1 forward), so there is one log.

## Generic backend (2026-11-05)

The page must work on any Debian with NUT, synodeb being only an extra backend where
`/etc/synodeb/ups.conf` exists. Sources: NUT's network protocol
(`docs/developer-guide.chunked/net-protocol.html`: `LIST CMD`, `USERNAME`, `PASSWORD`,
`INSTCMD`, errors `ACCESS-DENIED`, `CMD-NOT-SUPPORTED`, `USERNAME-REQUIRED`), `upsd.users(5)`
(`instcmds` repeated per command or `ALL`, root:nut 0640), `upsmon(8)` (`upsmon -c fsd`:
"shut down before the battery is critical, e.g. through upssched"; primary sets FSD, waits
`HOSTSYNC`, `FINALDELAY`, runs `SHUTDOWNCMD`, `POWERDOWNFLAG` for the UPS power-off),
`upssched(8)` (`AT ONBATT * START-TIMER`, one `CMDSCRIPT`), Debian's `nut-server.service`
(`ExecReload=upsd -c reload`).

How the others do it: TrueNAS SCALE, the openmediavault-nut plugin and Unraid's NUT plugin
**own the whole of `/etc/nut`**: they generate `ups.conf`, `upsd.users` (their own monitor user),
`upsmon.conf` and, for "shut down after N seconds on battery", an `upssched.conf` timer whose
command runs `upsmon -c fsd`. Battery tests are a shell `upscmd` in all three.

rynas cannot own `/etc/nut`: on a plain Debian the admin may already have a working NUT setup
(drivers, slaves, their own `NOTIFYCMD`/`upssched` script). `upsmon.conf` has a single
`NOTIFYCMD` and `upssched.conf` a single `CMDSCRIPT`, so an upssched timer would fight such a
setup. Picked, least invasive:

- **Status** as before (anonymous `LIST`), plus `LIST CMD`: only the `test.battery.*` commands
  the UPS lists are offered; a UPS without them gets a neutral note.
- **Battery test**: rynasd sends `INSTCMD` itself (no `upscmd` child) as a dedicated upsd user
  `rynas`, a managed block of `upsd.users` with only the three test commands (no `actions`, no
  `upsmon` role), password generated once into `/etc/rynas/nut-upsd.secret` (root 0600, never
  logged). Written only when a test runs and the block is missing or drifted, then
  `systemctl reload nut-server.service` (fallback `upsd -c reload`). An existing `[rynas]`
  section of the admin's is an error, never overwritten. Same task and result tracking as on
  synodeb; `rynasctl ups test quick|deep|stop` for scheduled tests on both backends.
- **Policy**: no change to NUT's files. upsmon keeps handling low battery exactly as the admin
  configured it. rynasd's watcher (upsd every 5 s, the same cost as the page's own reading)
  carries out the extra "after N minutes on battery" trigger from a rynas-owned file
  (`/etc/rynas/ups.conf`, config domain `ups.safemode` with drift detection): it stops rynas
  tasks and scheduler units within the configured time, then calls `upsmon -c fsd` (NUT's own
  shutdown path, so slaves, `SHUTDOWNCMD` and the UPS power-off still work), falling back to
  `systemctl poweroff` with an error notification if upsmon refuses. On OB+LB or FSD it only
  does the ordered stop (upsmon is already shutting down). It also records "on battery" and
  "power back" in the notification center, which plain NUT does not feed.
- **UI**: neutral texts on NUT ("shut down" instead of Safe Mode, "install NUT (Debian package
  nut)" when missing); synodeb, `synodeb-ups` and Safe Mode appear only on the synodeb backend.

Limits: the backend is NUT when `/etc/nut/nut.conf` or `upsd.users` exists, so a `netclient`
setup (upsd on another host) shows the page but the tests need a local upsd; rynasd and rynas-web
both default to `127.0.0.1:3493`. The watcher needs upsmon running as primary for the shutdown
(otherwise the poweroff fallback). NUT plans to require login for `LIST` later; the status
reader will then need the same upsd user.

## Driver wizard (2026-11-06)

Goal: set up any UPS NUT supports from the UI, on a plain Debian without touching an admin's
own NUT setup, and on synodeb through synodeb's own tool.

Sources: NUT's `nutupsdrv(8)` (`-s <id>` "configure this driver only with command line
arguments instead of reading ups.conf", `-x var=val`, `-d <count>` "dump the data tree (in
upsc format) after the update loop ran count times and exit", NUT ≥ 2.8.0, banner hidden by
`NUT_QUIET_INIT_BANNER`), `nut-scanner(8)` (`-U` USB, `-S` SNMP, `-M` XML, `-O` old NUT
servers; output is a ready `ups.conf` section with `driver`, `port`, `vendorid`,
`productid`), `upsdrvctl(8)`, `upsdrvsvcctl(8)` and `nut-driver-enumerator(8)` (Debian:
one `nut-driver@<section>.service` per `ups.conf` section, `nut-driver.target`, resynced by
`upsdrvsvcctl resync` or the enumerator's path unit), `upsdrvquery(8)` (NUT 2.8.1+, talks to
a running driver's socket; not needed for a one-shot test), NUT's hardware list
`/usr/share/nut/driver.list` (shipped by Debian's `nut-server`, also
`62-nut-usbups.rules` for USB permissions), NUT issue 2738 / PR 3323 (USB ID `0925:1234`).

How the others do it: TrueNAS SCALE offers a driver drop-down built from `driver.list`
(manufacturer · model → driver), a port field, "auxiliary parameters" for `ups.conf` and
owns `/etc/nut`; no detection. The openmediavault NUT plugin has driver, port and a free text
of extra directives, also owning the files; no detection. Unraid's NUT plugin lists the USB
drivers (custom driver allowed); users report nut-scanner suggestions that start but never
talk to the UPS. Home Assistant's NUT add-on takes a YAML list of devices (driver, port,
config lines). None of them tests a driver before writing it.

Real case: Marco's VULTECH UPS1050VA-PRO, USB `0925:1234` ("Lakeview Research UPS USB Mon
V2.0"). `nut-scanner -U` suggests `richcomm_usb`, synodeb's default was `usbhid-ups`; neither
reads it. NUT's hardware list (Armac, Accurat Flux, issue 2738) and months of use on another
PC: `nutdrv_qx` with `subdriver=armac`, `protocol=megatec`, `norating`, `pollfreq=30`,
`vendorid=0925`, `productid=1234`. On Debian trixie (NUT 2.8.1) no driver works
(`nutdrv_qx` armac: "Device not supported!", `richcomm_usb`: "driver callback failed: Entity
not found", `blazer_usb`: "No supported UPS detected"); it works from NUT 2.8.5.

Picked:

- **Candidates, then a test run of each** (none of the others does it). For each UPS on
  USB: rynas's small table of USB IDs known to need a driver setting first
  (`rynas_nut::setup::KNOWN`, with the lowest NUT version it works with), then the
  scanner's suggestion, then `usbhid-ups`, `nutdrv_qx`, `blazer_usb`, with the USB IDs
  (`vendorid`, `productid`) only for drivers that take NUT's USB matching options
  (`rynas_nut::setup::USB_MATCHING_DRIVERS`: the ones calling `nut_usb_addvars()`).
  `richcomm_usb` and `bcmxcp_usb` refuse them in 2.8.1 and 2.8.5 ("Fatal error: 'vendorid'
  is not a valid variable name for this driver", NUT issue 1768; `nut-scanner` 2.8.5 leaves
  them out for those two), `nutdrv_atcl_usb` only takes `vendor`; a manual entry giving them
  to one of those three is refused with that reason. Seen on a real NAS on 2026-11-11.
  Each runs as `<driver> -s rynasprobe -x port=… -x k=v … -d 1` under a 45 s timeout; the
  first one that reports `ups.status` wins and the others are not run. A candidate known to
  need a newer NUT than the installed one (`upsdrvctl -V`) is skipped and the result says
  "needs NUT X, this NAS has Y" with each failed driver's last lines.
- **Manual entry** for everything else: any installed driver (manufacturers from
  `driver.list` as a hint), port (`auto`, `/dev/ttyS0`, an SNMP address), extra options;
  tested the same way, or applied untested on request.
- **A running driver holds the device**: the detection reports it and stops
  `nut-driver@*` only when the admin allows it, starting them again afterwards.
- **Apply = configuration key `ups.device`** (pending → apply, revision, rollback):
  rynas blocks only, never the admin's lines: its section in `ups.conf`, `MODE` in
  `nut.conf` (`netserver` stays), the upsmon user `rynas-upsmon` in `upsd.users` and its
  `MONITOR … primary` line in `upsmon.conf`, files root:nut 0640. A section of the admin's in
  the way (same name, or a USB section without different USB IDs) makes the apply refuse
  unless the admin takes it over in the wizard (the section and its `MONITOR` line are then
  removed; the apply keeps a backup). Then `upsdrvsvcctl resync`, `reset-failed` of the
  driver unit (a failed start with an older setting, e.g. right after the package install),
  `enable --now` and restart of
  `nut-driver`, `nut-server`, `nut-monitor`; a driver that does not start rolls the apply back.
  The review step shows the blocks, the removed lines and the commands (passwords masked).
- **synodeb** owns `/etc/nut`: test runs are `synodeb-ups probe --driver … --timeout 45`
  (exit 0 vars, 1 no status, 2 bad usage/unknown driver, 3 device held by a nut-driver), the
  apply `synodeb-ups setup --keep-password --driver D --port P --opt k=v …` (synodeb keeps it
  in `/etc/synodeb/ups-driver.conf`; rollback to nothing: `--default-driver`). An older
  synodeb (no `--driver` in `setup --help`) blocks the wizard with "update synodeb".
- Drivers are looked up in `/usr/libexec/nut` (NUT ≥ 2.8.2), then `/lib/nut` and `/usr/lib/nut`.
- Values limited to `[A-Za-z0-9._:/@+,=-]` (option names `[a-z][a-z0-9_.]`, ≤ 32 options):
  nothing needs quoting in `ups.conf`, argv or synodeb's `--opt`.

## Network UPS server (2026-11-07)

Goal: the NAS serves its UPS to the other machines of the LAN (servers and PCs that run
NUT's upsmon and must shut down first, Home Assistant, dashboards), on any Debian with NUT,
the NUT way and without a firewall of rynas's own.

Sources:
- `upsd.conf(5)` <https://networkupstools.org/docs/man/upsd.conf.html>: `LISTEN <address>
  [port]`, several lines allowed, default `127.0.0.1` and `::1`; `0.0.0.0` / `::` bind all
  addresses; read only at start (restart, not reload). The ACL/ACCEPT/REJECT mechanism was
  removed in NUT 2.4; the page says to use `LISTEN` on multihomed systems and "if this is not
  fine grained enough, use a firewall", and not to rely on `LISTEN` alone. SSL: `CERTFILE`
  (OpenSSL builds), `CERTPATH`/`CERTIDENT` (NSS), `CERTREQUEST`, `DISABLE_WEAK_SSL`; from
  2.8.6 incomplete SSL settings stop upsd.
- upsd source (v2.8.1, `server/upsd.c`, `setuptcp()`): `IPV6_V6ONLY` on IPv6 sockets (so
  `0.0.0.0` and `::` bind side by side), `SO_REUSEADDR`; a `LISTEN` that does not bind is
  logged and skipped, only a failure of the **first** one is fatal ("no listening interface
  available").
- `upsd.users(5)` <https://networkupstools.org/docs/man/upsd.users.html>: sections with
  `password`, `actions` (`SET`, `FSD`), `instcmds`, `upsmon primary|secondary` ("do not
  assign actions to upsmon by hand"); file root:nut 0640. NUT 2.8.0 renamed master/slave to
  primary/secondary (the old words are still accepted).
- `upsmon.conf(5)` <https://networkupstools.org/docs/man/upsmon.conf.html>:
  `MONITOR <ups>@<host>[:<port>] <power value> <user> <password> primary|secondary`;
  client TLS with `CERTVERIFY` / `FORCESSL`.
- NUT network protocol: `LIST UPS`, `LIST VAR` work **without logging in** (rynas-web's own
  status reader relies on it); a login is only needed for `LOGIN` (upsmon's attach),
  `INSTCMD`, `SET`, `FSD`.
- Debian trixie: `nut 2.8.1-5`, `debian/rules` configures `--with-ssl --with-nss`
  (<https://sources.debian.org/src/nut/trixie/debian/rules/>; Jim Klimov on nut-upsuser,
  2025-07: <https://alioth-lists.debian.net/pipermail/nut-upsuser/2025-July/014052.html>), no
  libwrap flag. Debian's README: "in netserver MODE you will have to modify the LISTEN
  option" (<https://metadata-backend.ftp-master.debian.org/changelogs/main/n/nut/stable_nut.README.Debian>).
- systemd `IPAddressAllow=`/`IPAddressDeny=`
  (<https://www.freedesktop.org/software/systemd/man/latest/systemd.resource-control.html>):
  per-unit eBPF cgroup filter on AF_INET/AF_INET6 sockets, ingress by source address,
  `localhost`/`link-local`/CIDR, allow wins over deny; without eBPF cgroup support (some
  containers) it has no effect and should not be the only protection.
- Home Assistant NUT integration (<https://www.home-assistant.io/integrations/nut/>): host,
  port 3493, user and password **optional**; without credentials no buttons/switches; those
  need `instcmds` on the server; the integration only reads variables otherwise.
- Others: DSM "Enable network UPS server" with a list of "Permitted Synology NAS Devices"
  (IPs), clients log in with the fixed `monuser`/`secret` as slave
  (<https://kb.synology.com/en-id/DSM/help/DSM/AdminCenter/system_hardware_ups?version=7>,
  <https://www.wundertech.net/use-a-synology-nas-as-a-nut-server/>); TrueNAS SCALE UPS
  service: "Remote Monitor" listens on all interfaces with the known `upsmon`/`fixmepass`
  unless changed, "Monitor User/Password", free-text "Extra Users" in upsd.users format, no
  host list (<https://www.truenas.com/docs/scale/scaleuireference/systemsettings/services/upsservicesscreenscale/>);
  openmediavault's nut plugin and Unraid's NUT plugin offer a netserver mode with a monitor
  user and free-text extra lines (community guides). None of them shows the client line to
  paste, and DSM's host list is its own filter in front of a fixed password.

Picked:

- **Configuration key `ups.netserver`** (`UpsNetServerConfig`: `enabled`, `listen`,
  `allow`, `clients`), staged and applied like the rest of the UPS setup; the dialogs of the
  UPS tab save and apply only this key (A19b). Order 46, after `ups.device`.
- **Listen** (`upsd.conf`, block `ups-netserver`, root:nut 0640): "all addresses" is
  `LISTEN 0.0.0.0 3493` + `LISTEN :: 3493` (localhost included); chosen addresses come after
  `127.0.0.1` and `::1`, so the first, fatal one always binds and an address that went away
  (DHCP) only logs. Loopback, any and link-local addresses are refused (the NAS's addresses
  are offered from `ip -j addr`; a `PUT` refuses one the NAS does not have, unless it is
  already saved, and IPv4-mapped `::ffff:` forms). The admin's own `LISTEN` lines would clash:
  while the server is on they are set aside as `# saved: LISTEN …` comments inside the block
  (the plan lists them as removed); turning the server off, or removing the key, writes an
  "off" block that puts them back as lines upsd reads (they stay in the block, so the next
  "on" sets them aside again). Off and never on, the block only holds a comment.
  Chosen addresses may not exist yet at boot, and upsd binds each `LISTEN` once at start:
  with chosen addresses the drop-in adds `Wants=`/`After=network-online.target` to
  `nut-server.service` (Debian's unit only waits for `network.target`).
- **Allowed hosts**: NUT has no ACL any more, so rynas uses what every systemd Debian has: a
  drop-in `/etc/systemd/system/nut-server.service.d/rynas-netserver.conf` with
  `IPAddressDeny=any`, `IPAddressAllow=localhost` and the admin's hosts/subnets; packets from
  other sources never reach upsd. Honest limits (said in the UI and here): it filters the
  whole upsd service only; without eBPF cgroups (old kernels, some containers) systemd
  ignores it with only a log line, and a firewall (nftables/ufw of the admin, or the router)
  is then the way; it is not a firewall and rynas does not grow one. To see whether it works,
  the drop-in also sets `IPAccounting=yes`: `systemctl show -p IPIngressPackets` gives a
  count when the eBPF programs are attached and `[not set]` (or `UINT64_MAX`) when not; the
  state reports `filter_active` and the problem `filter_inactive`, and an apply logs a
  warning. The filter sees source addresses: `localhost` covers clients on the NAS that
  connect to `127.0.0.1`/`::1`, but one that connects from another address of the NAS (a
  Docker container on a bridge comes from `172.17.0.0/16` or its network's subnet) is
  filtered like any host, so its subnet goes in the list (allowing the NAS's own LAN
  addresses would not help: the container's source is the bridge address). Alternatives considered: `LISTEN` on one
  address (done, but it limits interfaces, not peers); tcp-wrappers (not built in Debian, and
  dropped by most distributions); an nftables table of rynas (out of scope, and would fight an
  admin's firewall). Empty list: any host that reaches the port, with a warning on the card.
- **Accounts** in `upsd.users` (block `ups-netserver`): role *secondary* (`upsmon secondary`,
  for upsmon on another machine) or *monitor* (no `actions`, no `instcmds`: logs in, reads;
  for Home Assistant and dashboards: reading needs no login in NUT, but an account names the
  client and is what the integration's form asks for). No account can set variables, run
  instant commands or force a shutdown; buttons in Home Assistant stay off (a "control" role
  with chosen `instcmds` is left for later). Names `[a-z0-9_-]`, never `rynas`/`rynas-upsmon`
  or a user of the admin's own sections; the local primary user of `ups.device` is untouched.
- **Passwords**: generated (32 letters and digits) or chosen (12–64 of
  `[A-Za-z0-9._~@%+=:,-]`, one word everywhere). Write-only in the API, sealed at once with
  rynasd's age key (the notification engine's `<state>/notify/secret.key`, folder 0700,
  carried only by a passphrase-protected export), so `config.db` and the revisions hold only
  the sealed envelopes (ciphertext; `/api/v1/config/revisions` shows them as such) and the
  UPS answers leave even those out; the
  `PUT` answer returns the passwords of that request **once**, the UI shows them with a copy
  button right after the apply. Later only a new password can be made. A restore without the
  key fails the apply with "generate a new one"; so does an opened password that could not
  go between the double quotes of `upsd.users` (a quote, backslash or line break from an
  older or tampered envelope), checked again before every write.
- **One file, several owners**: `upsd.users` also has the blocks of `ups.device` and of the
  battery test user, and the configuration engine keeps one owner per file, so the accounts
  block is written by the domain's reconcile step (atomic, owner kept, mode capped at 0640,
  written back on rollback), like the battery test user; the battery test writes its block
  under the configuration's apply lock (`Config::exclusive`), so the two read-modify-writes
  never interleave. The drift report compares the block with the applied accounts (state
  drift of `ups.netserver`; "overwrite" writes it again). `upsd.conf` and the drop-in are
  ordinary managed files (drift detection, backup); a rollback puts every file back with its
  mode **and owner** (root:nut, so upsd can still read them).
- **Removing the key** (restoring a revision from before the server existed stages it as
  removed): the engine's `render_removed` hook writes the "off" files (no rynas `LISTEN`, the
  admin's back, no drop-in) and the reconcile removes the accounts; without it the server
  would stay open with files nobody manages any more.
- **Apply**: files; `systemctl daemon-reload` when the drop-in changed; `systemctl restart
  nut-server` when it runs and `upsd.conf` or the drop-in changed (`LISTEN` needs a restart;
  the NAS's upsmon reconnects by itself), only `systemctl reload` when only the accounts
  changed. Applied together with `ups.device` (which restarts upsd after every file is in
  place), upsd restarts once, unless the drop-in changed (it needs the daemon-reload first).
  Health check: upsd answers on its first `LISTEN` address (`127.0.0.1:3493` in both
  layouts) within 3 s, else the apply rolls back. `nut.conf`'s `MODE` stays with
  `ups.device` (`standalone`, or an admin's `netserver` kept): checked on Debian trixie
  (nut 2.8.1), `MODE=standalone` with these `LISTEN` lines serves the network exactly like
  `netserver` (the units start upsd in both modes and nothing gates on the value; Debian's
  README names `MODE` for its init scripts), so rynas keeps `MODE` as it is and `nut.conf`
  keeps one owner.
- **Connection details** (card and the dialog after a password): host (the LAN address of
  the default route), port 3493, user, UPS name (the wizard's, else the first `ups.conf`
  section) for Home Assistant (Settings › Devices and services › Network UPS Tools), and
  `MONITOR ups@host 1 user PASSWORD secondary` with `MODE=netclient` for another Linux upsmon
  (with the real password right after it was made), each with a copy button.
- **A module's UPS layer** owns `/etc/nut`: rynas writes nothing there. A layer with a server
  of its own (`rynas_module::ups::NetServerLayer`, today synodeb 4.6 in
  `rynas-hw-synology/src/ups_net.rs`) gets the same key through its tool; a layer without one
  shows "cannot be set up here" (`UpsNetBlock::Module`), one too old for it the module's update
  note (`UpsNetBlock::ModuleTooOld`). See "With synodeb" below.
- **API**: `GET/PUT /api/v1/ups/netserver` (`UpsNetServerState` with the NAS's addresses
  and subnets, `filter_active`, `problems` = `server_inactive` / `filter_inactive`;
  `UpsNetServerSaved`), `POST /api/v1/ups/plan/netserver` (`UpsSetupPlan`, passwords
  masked).

### With synodeb (2026-11-07)

synodeb 4.6 has its own network UPS server (`synodeb-ups netserver`), so the card works on the
synodeb backend too, through the module (D-MODULE: the core knows only the trait; the
synodeb code and texts are in `rynas-hw-synology` and the web interface's `modules/synology`).
- **Availability**: `synodeb-ups netserver status --json` exits 0 → offered; a synodeb before
  4.6 answers a usage error on the unknown subcommand → "update synodeb to 4.6"
  (`UpsNetBlock::ModuleTooOld`).
- **State** from `status --json`, schema 1 only (another schema or a failed run:
  `problems: layer_unreadable`; the apply then sends every password, harmless):
  `enabled`, `listen`, `clients`, `users` (`secondary` = rynas's "shuts down with the UPS",
  `readonly` = "read only": LOGIN refused, reading needs no login in NUT), `monuser`,
  `firewall_active` (reported as `filter_active`, `filter_inactive` when off), `upsd_active`
  (`server_inactive`), `source`. It goes to `UpsNetServerState::layer`: listen addresses (not
  editable: synodeb listens on every address), allowed hosts now, `primary_account`
  (synodeb's `monuser`, upsmon primary, the account other Synology NAS units expect; shown as
  information, its password set with `synodeb-ups passwd`), `outside` (synodeb users the rynas
  setting does not have: the next save removes them) and `source` (`manual`: clients set by
  hand as `NETWORK_CLIENTS` in `/etc/synodeb/ups.conf`; the card says a save writes
  `/etc/synodeb/ups-network.conf`, which then wins, and the settings dialog starts from those
  hosts).
- **Apply** (same staged key and flow, `reconcile` of `ups.netserver`): on →
  `synodeb-ups netserver set --client IP|CIDR … --user NAME:secondary|readonly …` (the whole
  set); off or key removed → `synodeb-ups netserver off` (upsd on localhost only, synodeb
  deletes the extra users and their passwords). Passwords stay sealed in the key; stdin carries
  `NAME PASSWORD` only for accounts whose envelope changed since the applied value or that the
  layer does not have (after an `off`); never argv, never the log, never the plan (which shows
  the command and "passwords … on its standard input"). Exit 0 applied (with NUT off synodeb
  saves and applies when it runs), 2 values refused (the layer's reason, file untouched), else
  not applied: both fail the apply, which rolls back by applying the previous value.
- **Checks**: allowed hosts required while on (synodeb's nftables table is the only filter);
  synodeb's user names `[a-z][a-z0-9_]{0,31}`, not `monuser`/`admin` (the core's password rule
  is a subset of synodeb's: 12–64 of `[A-Za-z0-9._~@%+=:,-]`); at most 16 hosts and 32 accounts
  (rynas's limits, under synodeb's 64/32).
- **Drift**: the layer's on/off, clients and users against the applied value
  (`ups-layer:netserver`).
- **Test instance**: under `--dry-system` the layer's simulated `synodeb-ups` keeps the server's
  state in the dry root (`/etc/synodeb/ups-network.conf`, a fake format without passwords);
  `FAKE_UPS_BACKEND=synodeb` in `tools/ui-audit/start.sh`, `FAKE_SYNODEB_VERSION=4.5` for the
  update note.

Not done, documented:
- **TLS**: Debian's build has SSL (`--with-ssl --with-nss`), but upsmon clients must be set
  up with `CERTVERIFY`/`FORCESSL` and a trusted certificate, Home Assistant's integration has
  no TLS option, and NUT 2.8.6 refuses to start on incomplete SSL settings. Later: `CERTFILE`
  from rynas's certificate store (OpenSSL builds) with `DISABLE_WEAK_SSL`, optional.
- **Client mode** (this NAS shuts down on another NAS's UPS): not supported yet and not small:
  rynas-web's status reader, rynasd's watcher and the battery tests all talk to
  `127.0.0.1:3493`, the wizard writes a local `MONITOR … primary` line, and `ups.device`
  owns `nut.conf`. It needs an `ups.remote` key (host, port, UPS, user, sealed password)
  that replaces `ups.device`'s driver part with `MODE=netclient` and a `MONITOR … secondary`
  line, and a configurable upsd address for the reader and the watcher. Until then an admin
  can write `MONITOR ups@other 1 user pass secondary` in `upsmon.conf` by hand (outside
  rynas's blocks); the UPS page then stays "not configured".
- A list of connected clients (`LIST CLIENT <ups>`, NUT ≥ 2.8) on the card.

## NUT newer than Debian's (2026-11-11)

Real case on a Debian 13 NAS: the USB UPS `0925:1234` above, NUT 2.8.1-5 from trixie. Only
`nutdrv_qx subdriver=armac protocol=megatec norating` reads it, and armac exists from NUT
2.8.5 (2.8.1: "Device not supported!"). `richcomm_usb` 2.8.1: "driver callback failed:
Entity not found"; `usbhid-ups` does not match it.

Facts (checked 2026-11-11): Debian has NUT 2.8.5-1 in forky and sid only (uploaded
2026-09-09); trixie has 2.8.1-5 (a 2.8.1-5+deb13u1 stable update is listed by
snapshot.debian.org), there is no trixie-backports upload. NUT 2.8.2 moved the drivers to
`/usr/libexec/nut` (rynas looks there first, then `/lib/nut`). 2.8.5's `libupsclient7` and
`libnutscan4` are new sonames next to trixie's (`libupsclient6t64`, `libnutscan2t64`), so the
old libraries stay for other programs. synodeb already rebuilt sid's 2.8.5 for its own
repository; the same rebuild for rynas works unmodified on trixie.

What the others do: TrueNAS and openmediavault ship the distribution's NUT and do not offer a
newer one; Home Assistant's add-on follows its base image. None of them offers a newer NUT
for one UPS that needs it.

Picked (D-UPS-NUT):

- **Stay on the distribution's NUT.** "Install NUT" installs Debian's package. The update is
  offered only when the wizard's detection skipped a known setting for its NUT version
  (`UpsDeviceState.nut_update`), never with a module's UPS layer (it brings its own NUT).
- **Sources in order**: an apt source of the system whose `nut-server` candidate is new
  enough (a future trixie-backports, a vendor repository: synodeb systems run NUT through
  their layer and never see rynas's packages), then **rynas's rebuild**: `packaging/nut/`
  builds Debian's own 2.8.5-1 source (pinned by the SHA-256 of its `.dsc`) on trixie in the
  pinned builder image with build dependencies from snapshot.debian.org, version
  `2.8.5-1~rynas13+1` (below Debian's `2.8.5-1`, so a Debian package replaces it), Debian's
  `nodoc` profile (no manual pages: the PDF/HTML manuals need a TeX toolchain), reproducible
  (two builds give the same files). Only `nut-server`, `nut-client`, `libupsclient7` and
  `libnutscan4` (the metapackage `nut` stays Debian's: unversioned dependencies). The
  packages are attached to the rynas release; the admin copies them to
  `/var/lib/rynas/packages` and rynasd installs them only when every file of
  `ups_update::PINNED_DEBS` is there with its SHA-256 (the same delivery as the
  deduplication engine). Debian 13 on x86-64 only; elsewhere the note just says a newer NUT
  is needed.
- **The install**: apt's simulation first (`--no-remove`: a system with `nut-snmp`, `nut-xml`…
  of the old version is refused before anything runs, since those packages depend on the
  exact `nut-server` version), then `apt-get install --no-remove` in the `rynas-apt`
  transient unit with `--force-confold`: the configuration files rynas or the admin changed
  stay (dpkg says "Keeping old config file as default" and leaves `*.dpkg-dist`, no
  question), untouched ones follow the package. Tested by `packaging/nut/test.sh` (trixie
  container, 2.8.1 then the update with rynasd's exact apt line).
- **After any NUT package change** (install and update): udev rules applied to the UPS
  already plugged in (`udevadm control --reload-rules`, `udevadm trigger
  --subsystem-match=usb --action=change`, `udevadm settle --timeout=30`); seen on the real
  NAS: without it the device stays `root:root` and the drivers fail "insufficient
  permissions" until a replug. Then `systemctl reset-failed` of the driver units (the
  package may start NUT with the old setting), the version read again and the detection
  report dropped: the admin detects again.
- CI watch: `packaging/nut/check-upstream.sh` says when sid has a newer NUT or trixie reaches
  the pinned one (then the rebuild can go).

## Left for later
SNMP/XML scans (`nut-scanner -S/-M` need a range or a host:
manual entry for now), a udev rule for a USB ID missing from NUT's rules (the test run then
fails with a permission error), removing the wizard's setup from the UI (the `ups.device` key
can be removed from the configuration), UPS power-off at the end (synodeb `SAFESHUTDOWN`; on
NUT it is the admin's `POWERDOWNFLAG` setup), outage statistics and battery trend (RRD), a
check that upsmon is running as primary (shown on the page); for the network UPS server see
its own section (TLS, client mode, connected clients, a control role).
