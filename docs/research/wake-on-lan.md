# Wake-on-LAN and device power

Status: research 2026-11-20, phase 1 being implemented (0.8.26).

## 1. What others do

- **Self-hosted managers.** The leader keeps a device list with a ping status, a wake button
  that waits until the device answers (1 s pings, 120 s timeout), cron wake and shutdown
  schedules, an nmap/ARP scan that follows a device's IP by MAC, groups and per-user
  permissions. Shutdown is a shell command template (SSH with a stored password, `net rpc`),
  the weak point: secrets in clear and command injection risks. It sends to the subnet
  broadcast and to the device's IP, ports 9 and 7.
- **Routers.** The best router UI puts a "Start computer" button on every device of its home
  network list (the same button is the test that WoL works) and can start a computer when it
  is accessed from the internet. Router WoL pages always ask for the interface: a NAS with
  bonds and VLANs has the same need.
- **Home automation.** A wake *switch* whose state is a ping and whose "off" is any action;
  no interface choice (a common complaint).
- **NAS systems.** None has a manager of other devices in its UI. One vendor's newest backup
  product, for Windows PCs, wakes a device for its scheduled backup, keeps it awake during it
  and shuts it down after: that is the integration that matters most to a NAS with PC backup.
  The best known open-source PC backup server has no wake at all (long-standing requests).
- **UPS.** A small tool remembers which clients were on before a power cut and wakes them when
  power and battery are back (a clean UPS shutdown means "restore on AC loss" never fires).

## 2. Technical facts

- Magic packet: 6 × `FF`, 16 × the MAC, optional SecureOn password (4 or 6 bytes, clear
  text). No delivery confirmation: only the device's status proves a wake.
- Linux sends `255.255.255.255` out of one interface only (the default route's). With bonds,
  VLANs or several NICs the packet must be tied to an interface: a raw Ethernet frame
  (EtherType `0x0842`, `AF_PACKET`, destination `ff:ff:ff:ff:ff:ff`) on that interface, plus a
  UDP datagram to the interface's subnet broadcast, port 9, with `SO_BINDTODEVICE`. rynasd has
  `CAP_NET_RAW` and `AF_PACKET` already.
- Broadcasts are not routed and Tailscale carries no layer 2: a device on another subnet can be
  woken only by a host on its LAN. The NAS is that host: a click in its UI over Tailscale wakes
  a PC at home.
- Most Wi-Fi cards cannot be woken; laptops on Wi-Fi need the PC's own RTC wake timer instead.
  Wi-Fi MACs may be randomized.
- On the PC (Linux): `ethtool <if>` shows `Supports Wake-on: …g…` and `Wake-on: g`; the setting
  may be lost at boot (driver default), so it has to be applied at every start (systemd
  `.link` `WakeOnLan=magic`, NetworkManager `802-3-ethernet.wake-on-lan`, or the agent itself).
  BIOS options (WoL, ErP/deep sleep) cannot be read from the OS.
- A device may answer ping before its OS is ready: the agent's contact is the strongest
  "ready" signal; TCP ports and ping are for devices without an agent.

## 3. Decisions

1. **A device manager of its own**, page **Wake-on-LAN** (`/wake`, sidebar next to Network),
   for any device (PC with or without agent, server, TV box). Admins manage it; a device can
   name users who may wake (and, with an agent, put to sleep) it, who see only those devices.
2. **Sending**: per wake, on the interface whose IPv4 subnet holds the device's last IP (else
   every up, non-loopback, non-tailscale, broadcast-capable interface; a bond itself, never its
   ports), a raw `0x0842` frame and a UDP datagram to the subnet broadcast (device port, 9 by
   default), three times 1 s apart; again at +30 s and +60 s while the device is still off.
   A fixed interface can be chosen per device.
3. **Status**: agent contact (PC linked to a paired agent: online like the PC backup page
   counts it) > TCP port check (the device's ports) > ICMP ping > neighbor table `REACHABLE`.
   States Online / Offline / Waking / Unknown (no IP and no agent). Polled every 5 s while a
   page watches (SSE or short poll), every 60 s otherwise, and once a second while waking.
4. **Wake = task** with live lines ("Sent on enp3s0 (3×)", "Network up after 14 s, waiting for
   the agent", "Online after 23 s"), timeout per device (180 s by default). The result goes to
   the device's history (last 20) and, for wakes nobody watched (schedule, backup), to the
   notification center when it fails.
5. **MAC sources**: the paired agent reports every NIC (name, MAC, IPv4, wired/Wi-Fi,
   WoL supported/enabled) at start and every 6 h when it changed (`POST …/agent/facts`); the
   neighbor table (`ip -j neigh`) for devices without agent; manual entry. "Add device" offers
   PCs with agents first, then devices seen on the network, then manual.
6. **WoL health** for agent PCs: wired NIC, magic supported, enabled; Wi-Fi warning; no NAS
   interface on the device's subnet warning. "Enable on this PC" asks the agent to run
   `ethtool -s <if> wol g` now and at every agent start.
7. **Sleep and shutdown only through the paired agent** (authenticated channel; no SSH
   passwords, no command templates): Suspend, Hibernate, Shut down, refused while a backup
   runs. Sent only to agents ≥ 0.8.26 (older agents cannot parse the new command).
8. **Wake for backups** (PC backup setting, needs a linked WoL device): when a scheduled backup
   is due inside its window and the PC is off, the NAS wakes it (at most once per due time,
   no retry storm), marks the plan "woken by the NAS"; the agent then starts the backup without
   the start delay and, after it, does the chosen "after a wake" action (stay on, suspend, shut
   down) only if nobody used the PC meanwhile (every logind session idle or none).
9. **Later** (phase 2): wake and sleep schedules as scheduler tasks (presets + OnCalendar);
   wake the PCs that were on after a UPS power return; wake groups.

## 4. API (phase 1)

Types in `crates/rynas-api-types/src/wol.rs` (UI and server) and `pcagent.rs` (agent).
Configuration in config key `wol` (`WolConfig`: revisions, export, rollback like every
domain); runtime state (status, history) in `<state>/wol/state.json`.

- `GET /api/v1/wol` → `WolOverview` (devices visible to the caller, NAS interfaces).
- `POST /api/v1/wol/devices` (admin) `WolDevice` → created device; `PUT …/{id}`, `DELETE …/{id}`.
- `POST /api/v1/wol/devices/{id}/wake` → `{ upid }` (task, live lines).
- `POST /api/v1/wol/devices/{id}/sleep` `WolSleepRequest` → 202 (agent command).
- `POST /api/v1/wol/devices/{id}/enable` (admin) → 202 (agent command, enable WoL on its NICs).
- `GET /api/v1/wol/discover` (admin) → `Vec<WolCandidate>` (agents' NICs + neighbor table).
- Agent: `POST /api/v1/pcbackup/agent/facts` `AgentFacts` (204; an older NAS answers 404 and
  the agent stops sending until it restarts).
- PC backup settings: `wake_for_backup`, `after_wake`; plan: `woken_ms`, `after_wake`.

## 5. Pitfalls handled

255.255.255.255 on the wrong interface; LACP bonds (send on the bond); never `tailscale0`;
Wi-Fi and random MACs (warning); stale IPs (refreshed from the agent and the neighbor table);
ping before the OS is ready (agent contact wins); the NAS's own MACs refused as a target;
wake requests rate-limited (one running wake per device; a second click joins it).
