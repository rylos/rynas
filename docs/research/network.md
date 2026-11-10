# rynas network module: research and proposal

## 0. Summary

- **The biggest risk is locking yourself out of the NAS.** Every network change goes through `rynasd` with a **timed confirmation and an automatic rollback handled server-side**. The rollback is kept alive by a systemd timer and a **boot guard**, not by the browser. The **confirmation comes from a new connection over the new path**, not from the old session.
- **The NAS always has at least three rescue paths.** An IPv4/IPv6 link-local address with `nas.local` via mDNS on every physical port. Tailscale as out-of-band access. A **reset button gesture** that puts every port on DHCP, disables the firewall and keeps the old configuration so it can be restored.
- **Backend: systemd-networkd.** synodeb already uses it in its image, and trixie ships version 257. It supports bonds, VLANs, bridges and WireGuard natively, reloads with `networkctl reload` and exposes the `org.freedesktop.network1` D-Bus API. Existing ifupdown configurations are shown **read-only** until the user picks "let rynas manage this". That starts a guided migration with rollback.
- **synodeb-specific risk.** Debian installed by the installer uses **ifupdown** (`/etc/network/interfaces`) with predictable `enp*` names. Only the synodeb image (the reference setup: DS920+, system on two NVMe in RAID1) uses networkd with `lan1`/`lan2`/`lan10g`. So "adopt ifupdown" will be the most common case.

## 1. How networking is configured today on synodeb and Debian 13

| | Network configuration | Interface names |
|---|---|---|
| Debian 13, install without a desktop | **ifupdown** is still the default. NetworkManager only comes with a desktop, netplan only if selected ([debian-boot 06/2025](https://lists.debian.org/debian-boot/2025/06/msg00086.html), [debian-user 08/2025](https://lists.debian.org/debian-user/2025/08/msg00288.html)) | predictable (`enp6s0`…) |
| synodeb, installed system ([`installer/files/preseed.cfg`](https://github.com/rylos/synodeb/blob/main/installer/files/preseed.cfg), `finish`) | d-i `netcfg` (DHCP, or static from the command line with `--network dsm`), hence `/etc/network/interfaces` with ifupdown. `finish` does not touch networking | `enp*`: the `synodeb` package does **not** install `.link` files |
| synodeb, image ([`image/build-image.sh`](https://github.com/rylos/synodeb/blob/main/image/build-image.sh)) | **systemd-networkd** + resolved. `10-lan10g.network` static 192.0.2.1/24, `20-lan.network` DHCP on `lan1 lan2`, `99-fallback.network` DHCP on `en* eth*`. `wait-online` drop-in with `--any --timeout=60` | `.link` by PCI path: `lan1` 06:00.0, `lan2` 07:00.0, `lan10g` 01:00.0 (AQC113, `atlantic` driver). MAC set as in DSM |
| `synodeb` package | `systemd-networkd.{service,socket}.d/synodeb-ups.conf` drop-in with `IgnoreOnIsolate=yes`, to keep networking up in UPS Safe Mode | — |
| `synohwd` | WoL via `ethtool -s lan* wol g/d` plus the `l`/`k` command to the microcontroller; reads NIC temperatures (`atlantic`, `r8169`); the reset button produces event `a` | — |

**Worth knowing:** the microcontroller sends `a` **only once, after about 2 s of pressing**, and does not report how long the press lasted (synodeb's hardware notes). A DSM-style "hold for 4 s" gesture therefore cannot be detected: a double press is needed instead (see §3.5). Also, the **only console is serial and internal** ([`docs/serial.md`](https://github.com/rylos/synodeb/blob/main/docs/serial.md); not yet enabled on the DS920+).

## 2. How others make network changes safe

| Product | Mechanism | What to take / what to avoid |
|---|---|---|
| **TrueNAS** | "Test Changes", reverts after **60 s** (configurable). Confirmed from a new browser tab ([docs](https://www.truenas.com/docs/scale/26/network/interfaces/settingupbridge/)) | Good idea. Users report the UI takes about 40 s to reconnect, leaving only a few seconds to confirm ([forum](https://www.truenas.com/community/threads/cannot-create-a-bridge-how-to-diagnose.115227/)). **Longer default (120 s) and automatic reconnection** |
| **netplan try** | reverts after 120 s without confirmation | Real bug: with networkd, **MTU and IP were not restored** ([LP#1768802](https://bugs.launchpad.net/ubuntu/+source/netplan.io/+bug/1768802)). The rollback must be **verified** by comparing actual state against expected, not assumed to have worked |
| **NetworkManager** | `CheckpointCreate(devices, rollback_timeout, flags)`, `CheckpointAdjustRollbackTimeout`, `CheckpointRollback` ([D-Bus](https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.Checkpoint.html)); used by nmstate and Cockpit | The best model, because the daemon performs the rollback. rynas recreates it in `rynasd` on top of networkd |
| **Proxmox VE** | pending changes in `interfaces.new`, diff, "Apply Configuration" with `ifreload -a` ([pve-devel](https://lore.proxmox.com/all/20220602092251.1393709-1-a.lauterer@proxmox.com/t)) | Take the diff and "pending" ideas. **No rollback**: people have locked themselves out after "Apply" ([forum](https://forum.proxmox.com/threads/ifupdown2-networking-issues-apply-configuration-will-not-bring-up-interfaces.135033)) |
| **pfSense/OPNsense** | **anti-lockout rule** on the LAN for the GUI port; "Set interface IP" console menu | Take the anti-lockout. Typical mistake: the port or IP changes and the rule does not follow ([Netgate](https://forum.netgate.com/topic/146909/cannot-log-in-to-web-gui-after-lan-ip-change)) |
| **MikroTik Safe Mode** | rollback as soon as the session drops | Only undoes the last action and has no timer ([forum](https://forum.mikrotik.com/t/any-chance-of-a-test-mode-before-applying-the-configuration/124600/1)) |
| **Junos `commit confirmed`** | timed rollback of the whole commit, which must be confirmed again | The right model: **the whole transaction**, not the single change ([Juniper](https://www.juniper.net/documentation/us/en/software/junos-space22.2/junos-space-workspaces/topics/concept/confirmed-commit-from-platform.html)) |
| **DSM** | no rollback. Recovery via the **reset button, Mode 1 (4 s, beep)**: network on DHCP, ports 5000/5001, admin password ([Synology KB](https://www.synology.com/knowledgebase/DSM/tutorial/General_Setup/How_to_reset_my_Synology_NAS_7)). Discovery with Synology Assistant via broadcast | Take the physical gesture and the "factory network". Bonds and VLANs that lock you out are a classic: switch not set to LACP, or VLAN enabled on the NAS and the unit needing a reset ([WunderTech](https://wundertech.net/how-to-set-up-link-aggregation-on-a-synology-nas-lacp), [Cisco](https://community.cisco.com/t5/switching/problem-with-link-aggregation-sg350-and-synology-nas/m-p/4721463)) |

**Other real-world cases to keep in mind:**
- Debian **systemd 257.8-1~deb13u1**: networkd segfaulted and after reboot **there was no network** ([#1112535](https://lists.debian.org/debian-devel/2025/09/msg00133.html)). Proof that a boot guard is needed even when nobody touches the configuration.
- DSM firewall: after an update the "deny all" rule also blocked the LAN, fixed only with a Mode 1 reset ([synoforum](https://www.synoforum.com/threads/dsm7-upgrade-gone-wrong.7909/)).
- Tailscale with `--accept-routes` enabled on TrueNAS made the UI and shares unreachable ([forum](https://www.truenas.com/community/threads/ip-forwarding-between-nics-for-tailscale.109364)).

## 3. Safety proposal for rynas

### 3.1 The "apply" flow (network transaction)

```
UI: pending changes → Diff → [Apply with 120 s test]
rynasd:
 1 validate schema, subnet conflicts, gateway inside subnet, duplicate IP (arping -D),
            MTU ≤ driver maximum, "management path" (§3.2)
 2 save     revision N = last known good configuration (rynas files + hash) in config.db
            + /var/lib/rynas/net/lkg/
 3 mark     /var/lib/rynas/net/trial = {rev, deadline, token}   (fsync)
 4 arm      systemd-run --unit=rynas-net-rollback --on-active=120s rynasctl net rollback
            (survives a rynasd crash or restart)
 5 write    files in /etc/systemd/network/ atomically (tmp + rename) → networkctl reload
            (+ reconfigure of touched links; .link → udevadm trigger; immutable netdevs → §3.6)
 6 check    expected vs actual state (Describe via D-Bus), carrier, gateway (ping/ARP), DNS:
            /etc/resolv.conf in step with networkd (§4.1) and a DNS server that answers;
            when one answered before the change and none does after 15 s → roll back
 7 wait     POST /api/v1/net/confirm {token} arriving **over the new path**
            → cancel timer and marker, revision N+1 becomes the last known good
 deadline → restore last known good, verify actual state (lesson from LP#1768802),
            notification + log event
```

**Client-side confirmation.** The UI knows the candidate URLs in advance: new IP, `nas.local`, Tailscale name, old IP if it stays. It probes them (`/api/v1/ping`) and then confirms automatically with the token. With a self-signed certificate the browser cannot fetch a new origin without an exception. In that case the UI opens a new tab on the new address with the "Confirm" page (as TrueNAS does). With a valid certificate (Caddy/ACME or `*.ts.net`) confirmation is automatic.

**Boot guard.** `rynas-net-guard.service` runs with `DefaultDependencies=no` and `Before=systemd-networkd.service`. If it finds the `trial` marker, meaning an unconfirmed trial interrupted by a reboot, it restores the last known good configuration before networkd starts.

**Post-boot check.** `rynas-net-watchdog.timer` fires 5 minutes after boot. If no port **with carrier** has a routable address and a reachable gateway:
1. restore the last known good configuration;
2. if that is not enough, switch to the rescue configuration (DHCP everywhere + link-local);
3. notify with a beep and orange LED via the synology plugin.

With no cable plugged in it does nothing, to avoid false triggers.

### 3.2 Protecting the management interface
- `rynas-web` reads the local address of the session socket (`getsockname`), so it knows which address and interface the user is coming in on.
- If a change touches that address, that interface or the gateway used by the client, the timed test becomes **mandatory** (cannot be skipped). The UI warns: "you will be disconnected, the new URL is X".
- Validation **rejects** a configuration that leaves zero management paths: no port with an IP, or the rescue interface without link-local.
- **Anti-lockout** in the firewall (pfSense model): the `rynas-web` port (e.g. 8443) and SSH are always accepted from the management interface subnet and from 169.254.0.0/16 / fe80::/10. It can only be disabled under "Advanced", with confirmation.

### 3.3 Rescue paths (always on)

| Path | Implementation | When it helps |
|---|---|---|
| Link-local | `LinkLocalAddressing=yes` on every physical port, alongside DHCP or static. Deterministic `IPv4LLStartAddress=` (e.g. 169.254.*x.y* derived from the MAC) shown on the label in the UI ([systemd.network](https://www.freedesktop.org/software/systemd/man/latest/systemd.network.html)) | laptop connected **directly** with a cable; wrong IP or subnet |
| mDNS | Avahi publishes `<host>.local` and `_https._tcp` / `_rynas._tcp`; wsdd for Windows | IP forgotten or changed |
| Tailscale | `tailscaled` does not depend on the LAN, has its own address in 100.64.0.0/10 and reconnects whenever there is a way out | remote access, wrong LAN subnet. Does **not** help if the gateway is lost |
| Reset button | §3.5 | everything else is lost |
| Serial console | synodeb's [`docs/serial.md`](https://github.com/rylos/synodeb/blob/main/docs/serial.md): 115200 8N1, 3.3 V adapter; to be documented in rynas too (`rynasctl net rescue`) | last resort |

### 3.4 Emergency UI access
This confirms what D-PROXY already decided: `rynas-web` keeps listening on its own port, on all addresses including link-local, independently of Caddy.

### 3.5 Reset button gesture (synology plugin)
1. **First press** (≥2 s, the microcontroller sends `a`): short beep, status LED blinking orange, 10 s window.
2. **Second press within 10 s**: long beep, then `rynasctl net factory-reset`:
   - every port on DHCP + link-local;
   - bonds, VLANs and bridges suspended;
   - firewall set to the "LAN, anti-lockout" profile;
   - `rynas-web` on the default port;
   - optional: unlock a locked admin account (like DSM Mode 1).
3. The previous configuration becomes a **restorable revision** in the UI ("Restore the network as it was before the reset").

**A single press does nothing.** This avoids accidental resets. Today the `reset_button` option in `synohwd.toml` is set to `log`. When the plugin is installed, the gesture goes through that FIFO or socket.

### 3.6 Disruptive changes
networkd **does not modify** an existing netdev: VLAN id, bond mode. Removing a `.netdev` does not delete the interface ([networkctl](https://www.freedesktop.org/software/systemd/man/latest/networkctl.html)). rynasd therefore runs `ip link del` and then `reload` in the same transaction, which the UI flags as "disrupts the network".

**Interface renaming** (`.link`) only takes effect on reboot. The UI offers it as "apply on reboot", with the boot guard active.

## 4. Backend

| | systemd-networkd | NetworkManager | ifupdown / ifupdown2 | netplan |
|---|---|---|---|---|
| In Debian 13 | 257, already installed | in main | ifupdown is the default; ifupdown2 is the Proxmox default | supported, optional |
| Footprint | excellent (C, already in RAM with systemd) | heavier | good | a Python generator on top of another backend |
| Reload without restart | `networkctl reload/reconfigure`, D-Bus `Reload()`, `Link.Reconfigure()` | yes | ifupdown no; ifupdown2 yes with `ifreload -a` | `apply/try` |
| Native rollback | no (goes into rynasd) | **yes, checkpoints** | no | `try`, buggy |
| Bond / VLAN / bridge / WireGuard | all native (`.netdev`) | yes | yes (no WireGuard) | yes |
| Already used by synodeb | **yes** (image), UPS drop-in | no | yes (installer) | no |

**Recommendation: systemd-networkd.**
- The files are declarative and easy to generate with minijinja.
- It is the same backend as the synodeb image and adds no daemons.
- `networkctl --no-reconfigure` (since v262) is not needed for now: on trixie `reload` plus a targeted `reconfigure` is enough.

NetworkManager has better rollback, but it is designed for desktops and would drag in dependencies. rynasd rebuilds the checkpoint (§3.1).

**Managed files:**
- `/etc/systemd/network/{10-rynas-*.link, 20-rynas-*.netdev, 30-rynas-*.network}` with a `# managed by rynas, hash=…` header (drift check);
- wait-online drop-in modelled on synodeb (`--any`);
- `/etc/systemd/resolved.conf.d/rynas.conf` for global DNS and domains;
- **respect synodeb's drop-ins** (`synodeb-ups.conf`): they must not be touched.

**Adoption and migration:**
1. **Detection:** which service is active among `networking.service`, `NetworkManager` and `systemd-networkd`; parsing of `/etc/network/interfaces` (inet static/dhcp, `bond-*`, `bridge_ports`, `vlan-raw-device`) and of networkd files not owned by rynas.
2. **Read-only** until "Let rynas manage this" is pressed (like Cockpit with unmanaged interfaces).
3. **Migration from ifupdown** as a trial transaction:
   - generate equivalent networkd files, **same IP**;
   - DHCP ports keep the **DHCP identity** of ifupdown's client, so the server gives the same address (2026-11-10, after the HS-251 got a new address at the takeover: networkd's default client id, a DUID from the machine id with an IAID hashed from the name, is a new client to the server). A drop-in `05-rynas-<port>.network.d/10-dhcp-identity.conf` holds `ClientIdentifier=`, and for dhcpcd's `duid` option (Debian's default) `IAID=` (the last 4 bytes of the MAC), `DUIDType=`/`DUIDRawData=` from `/var/lib/dhcpcd/duid`, plus `RequestAddress=` with the leased address. dhclient sends no client id unless configured: `ClientIdentifier=mac`. A configuration that cannot be read for sure (per-interface options, a chosen IAID, a custom dhclient id) gets no drop-in, and the revision comment says so. The drop-in is part of the adoption revision, so a rollback removes it; later edits of the port keep it;
   - `systemctl disable networking`, move `interfaces` to `interfaces.rynas-bak`, `systemctl enable --now systemd-networkd` (and `systemd-resolved` when installed; otherwise rynas writes `/etc/resolv.conf`, §4.1);
   - same timed confirmation;
   - rollback: put `interfaces` back, stop networkd and `systemctl restart networking`.
   - Safer alternative, recommended when remote: "migrate on next reboot", with the boot guard restoring ifupdown if there is no routable address after 3 minutes. The plan is shown to every session until the reboot (who planned it, when, Cancel), because a reboot can come days later. After the boot no browser holds a token, so any administrator may keep it at once (confirmation over an address of the NAS) or go back to ifupdown; "go back" always restores ifupdown, only the timer keeps the migration by itself.
4. **Stable names:** the synology plugin proposes `.link` files by PCI path, so names match the ports printed on the back (`lan1`, `lan2`, `lan10g`); by MAC on other hardware. Hardware migration maps interfaces by role (already covered in D-CFGBAK).

### 4.1 Name resolution (DNS) when networkd runs the network

**Incident (2026-11-07, test DS920+ on Debian 13 + synodeb).** The migration at boot moved the network from ifupdown (+ dhcpcd) to networkd. networkd got its DHCP lease with two DNS servers (`networkctl status` showed them), but `systemd-resolved` was not installed and nobody wrote `/etc/resolv.conf`: it still held what dhcpcd wrote when it released the lease at shutdown, a header without any `nameserver`. apt, Tailscale and ACME stopped working, and the boot check confirmed the migration because it only looked for a routable address.

**How DNS works with networkd.** networkd never writes `/etc/resolv.conf`: the servers it learns (DHCP and RA with `UseDNS=yes`, the default, plus `DNS=` of the `.network` file) go to systemd-resolved over D-Bus and nowhere else ([systemd.network](https://www.freedesktop.org/software/systemd/man/latest/systemd.network.html), [systemd-resolved](https://www.freedesktop.org/software/systemd/man/latest/systemd-resolved.service.html)). Since Debian 12 resolved is a **separate package** (`systemd-resolved`, 257 in trixie); installing it turns `/etc/resolv.conf` into a link to its stub file, but only at install time ([DebOps](https://docs.debops.org/en/master/ansible/roles/resolved/getting-started.html), [Armbian trixie](https://forum.armbian.com/topic/39088-armbian-build-pr-remove-debootstrapped-static-resolvconf/)). A Debian server installed by d-i has ifupdown, its DHCP client writes the file, and resolved is not installed: exactly the DS920+ case. The synodeb *image* ships networkd **with** resolved, which is why the problem did not show there.

**What the others do.**

| System | DNS with its network backend |
|---|---|
| OpenMediaVault 6/7 | netplan → networkd **+ systemd-resolved**, `/etc/resolv.conf` links to the stub file; custom servers go to `resolved.conf` ([OMV #1710](https://www.github.com/openmediavault/openmediavault/issues/1710), [DietPi forum](https://dietpi.com/forum/t/no-internet-after-power-outage/23283?page=2)) |
| Cockpit / NetworkManager | `rc-manager=auto`: hands the servers to resolved when it runs, otherwise **writes `/etc/resolv.conf` itself** (a regular file; a link it leaves alone) ([NetworkManager.conf](https://manpages.debian.org/NetworkManager.conf)) |
| TrueNAS SCALE | the middleware **generates `/etc/resolv.conf`** from its global settings or the DHCP lease (with a known race when the lease arrives late: [forum](https://www.truenas.com/community/threads/truenas-scale-not-setting-dns-server-from-dhcp.99309/latest)) |
| Proxmox VE | ifupdown2, `/etc/resolv.conf` written by the DNS panel |

**Effects on the rest of the NAS.**
- **Docker** cannot reach 127.0.0.53 from a container; with the stub file it uses resolved's upstream list (`/run/systemd/resolve/resolv.conf`) for the containers ([gist](https://gist.github.com/IslamAlam/0c9e9dc42126d60cfed66f4779fd8047)). A plain file with real servers works as it is.
- **Tailscale** (accept DNS is off by default in rynas, D-TAILSCALE) talks to resolved over D-Bus when `/etc/resolv.conf` links to the *stub* file; without resolved it falls back to "direct" mode and rewrites the file itself, keeping a backup ([Linux DNS](https://tailscale.com/docs/reference/linux-dns), [blog](https://tailscale.com/blog/sisyphean-dns-client-linux), [#7655](https://github.com/tailscale/tailscale/issues/7655)). rynas never touches a file Tailscale wrote.
- **Avahi** answers `.local` through `nss-mdns` (`mdns4_minimal` before `dns` in `nsswitch.conf`), independent of both choices; networkd's links have `MulticastDNS=no` by default, so resolved would not compete for port 5353.

**Choice (NetworkManager's `rc-manager=auto`, adapted).** rynas never makes name resolution depend on a download:
1. **systemd-resolved active** (installed by the administrator or the image): `/etc/resolv.conf` becomes the link to `/run/systemd/resolve/stub-resolv.conf` (what Tailscale expects; Docker handles it). The migration from ifupdown enables resolved when it is installed.
2. **Otherwise rynas writes `/etc/resolv.conf`** from networkd's per-link DNS servers and search domains (networkd's link state, ordered by interface index, at most 3 servers as glibc reads), with a `# Generated by rynas` header. rynasd refreshes it at start and every minute, so lease changes are followed; writing is atomic (temporary file + rename).
3. **Only a file nobody else manages is touched**: missing, empty, left over by dhcpcd (`# Generated by dhcpcd`, also with lines added by hand), or rynas's own. A file written by hand, by Tailscale or by resolvconf, and any other link, stay as they are; nothing happens while ifupdown or NetworkManager runs. With no server learned, the file is left as it is.

Installing resolved during the migration was rejected as the default: it needs the network (and apt) at the moment the network changes, adds a daemon on a 4 GB box, and its package script replaces the file on its own; it stays the better option for split DNS (Tailscale MagicDNS) and can be offered later as an explicit, planned step.

**Guard.** A trial (apply, adoption, restore) that leaves networkd running checks name resolution after the address checks: `/etc/resolv.conf` brought in step, then a DNS query (the root's NS record over UDP; any answer, even SERVFAIL, proves the server is reachable) to the configured servers, for up to 15 s. If a server answered **before** the change and none answers after it, the change is rolled back like a failed verification ("name resolution stopped working"); a network that had no reachable DNS server before stays a valid choice. The migration at boot is kept at its deadline only when a port has a routable address **and** (when networkd learned DNS servers) one of them answers; otherwise ifupdown is restored with the `net.boot_dns_failed` notification.

**Upgrade.** A system migrated by an older rynas keeps the broken file until rynasd starts: the first refresh replaces dhcpcd's leftover with networkd's servers and sends `net.dns_repaired` (only when the file had no server before).

| Feature | Details | Idea from |
|---|---|---|
| Interfaces | state, speed/duplex (`ethtool`), driver, MAC, NIC temperature (synohwd), counters and errors, RRD graph per port | DSM, Proxmox |
| IPv4 | DHCP (with suggested reservation) or static; gateway with metric; multiple IPs per port | DSM |
| DNS | resolved: servers, search domains, optional DNS over TLS; hostname and `/etc/hosts` | — |
| IPv6 | SLAAC (`IPv6AcceptRA`), DHCPv6, static, privacy extensions off on the NAS | DSM |
| MTU / jumbo | 1500/9000/custom; **DF test** `ping -M do -s 8972 <gw/peer>` before confirming; bonds and VLANs inherit the MTU | DSM had MTU 9000 on `ovs_eth2` |
| Bond | **active-backup** ("fault tolerance") and **balance-alb** (DSM's "Adaptive Load Balancing") work with unmanaged switches; **802.3ad** only with a configured switch, with a "configure the switch first" warning. With a bridge for VMs/LXC: 802.3ad or active-backup, as Proxmox recommends. balance-alb only balances outgoing traffic ([RHEL](https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/7/html/networking_guide/overview-of-bonding-modes-and-the-required-settings-on-the-switch)) | DSM, Proxmox |
| VLAN | VLAN `.netdev` on a port or bond; warning if it is on the management interface (Cisco/Synology case) | TrueNAS |
| Bridge | `br0` for Incus (D-LXC) and VMs; recommended over macvlan, which does not let the host talk to the guest. "Port → bridge" conversion that moves the IP to the bridge in a single transaction | Proxmox, TrueNAS |
| Wake-on-LAN | per-port toggle, written via `synohwd.toml` (`wol`) or `ethtool`; shows the MAC for the magic packet | DSM |
| Diagnostics | ping, traceroute (mtr), DNS lookup, ARP test, **timed iperf3 server** (`iperf3 -s -1`, port open for only 10 min in the LAN profile), optional Internet speed test | DSM, OPNsense |
| LLDP (optional) | `lldpd`: shows the switch and port each cable is plugged into, useful with bonds and VLANs | Proxmox/TrueNAS |

**Mockup:**
```
Network ▸ Interfaces                   [⟲ 2 pending changes] [Diff] [Apply with test]
┌───────────┬──────────┬─────────────────────┬────────┬───────┬──────────────┐
│ Name      │ State    │ Addresses           │ Speed  │ MTU   │ Role         │
│ lan10g ●  │ up 10G   │ 192.0.2.1/24 ★mgmt  │ 10000  │ 9000  │ management   │
│ lan1   ●  │ up 1G    │ DHCP 192.0.2.37     │ 1000   │ 1500  │ rescue       │
│ lan2   ○  │ no cable │ 169.254.93.12 (LL)  │ —      │ 1500  │              │
│ br0       │ —        │ (new)               │        │       │ pending      │
└───────────┴──────────┴─────────────────────┴────────┴───────┴──────────────┘
[+ Bond] [+ VLAN] [+ Bridge]   Rescue: nas.local · 169.254.93.12 · nas.tail1234.ts.net
╭ Test in progress — 87 s ─────────────────────────────────────────────╮
│ New address https://192.0.2.10:8443  ✓ reachable                     │
│ Gateway ✓  DNS ✓  MTU 9000 to 192.0.2.20 ✓                           │
│ [Confirm]  [Revert now]   Without confirmation it reverts.           │
╰──────────────────────────────────────────────────────────────────────╯
```

## 6. VPN

| Option | Proposal |
|---|---|
| **Tailscale** (plugin, phase 2) | Official package from the `pkgs.tailscale.com` apt repository. Login: `tailscale up --qr`, with URL and QR shown in the UI; `--login-server` enables Headscale ([up](https://tailscale.com/docs/reference/tailscale-cli/up)). Status from `tailscale status --json` (stable CLI, not the LocalAPI). Toggles: subnet router (with `net.ipv4.ip_forward` and an "approve the routes in the admin console" warning), exit node, **`accept-routes` off by default** (TrueNAS case), Tailscale SSH. **`tailscale serve --bg https+insecure://127.0.0.1:8443`** publishes the UI at `https://nas.<tailnet>.ts.net` with a valid certificate ([serve](https://tailscale.com/docs/reference/tailscale-cli/serve)); `tailscale cert` for Caddy. Funnel off by default (D-EXPOSE), with explicit confirmation. "Tailscale" firewall profile on `tailscale0` / 100.64.0.0/10. Unraid 7 integrates Tailscale into individual containers, with serve and funnel per container ([Unraid](https://docs.unraid.net/unraid-os/system-administration/secure-your-server/tailscale)): in rynas this would be an optional per-stack sidecar in the docker plugin (phase 3) |
| **WireGuard** (plugin, phase 3; groundwork in phase 2) | Via networkd (`.netdev` `Kind=wireguard`, `PrivateKeyFile=` 0640 root:systemd-network), so same backend and same transaction. Keys generated in rynasd (x25519; the private key never leaves). **Road warrior** modelled on wg-easy ([v15](https://mintlify.com/wg-easy/wg-easy/features/qr-codes)): named clients, QR + `.conf`, split or full tunnel, enable/disable, last handshake and traffic. **Site-to-site**: peers with endpoint and AllowedIPs. Needs a UDP port forwarded on the router and a DDNS name: wizard with a D-EXPOSE warning |
| Headscale / NetBird | not in core. **Headscale** as a catalog app (lightweight, 50–100 MB, third-party Headplane UI), which the Tailscale plugin uses via `--login-server`. NetBird is heavier (multiple services + OIDC) and at most an app ([comparison](https://www.bitdoze.com/netbird-vs-headscale-vs-tailscale/)) |
| OpenVPN / L2TP / PPTP (DSM VPN Server) | **no**. PPTP is insecure. OpenVPN at most as a third-party app for clients that cannot do anything else |

## 7. Firewall (brief)
- **nftables** with its own `inet rynas` table, separate from Docker's (iptables-nft). Ports published by containers are filtered in **`DOCKER-USER`** (see `docs/research/docker.md`).
- **Per-service** rules, as in DSM: SMB, NFS, rynas-web, SSH, apps.
- **Profiles** LAN / LAN+Tailscale / public (D-EXPOSE), plus the anti-lockout from §3.2.
- **Apply:** `nft -c -f` (check), then `nft -f` (atomic), with **the same timed confirmation** and the same guard as §3.1. The reset gesture also returns the firewall to the safe profile.
- **Auto-block:** built into rynas-web for failed logins (nft set `@blocked` with timeout, allowlist for LAN and Tailscale). For SSH and other services, fail2ban managed by the plugin with an nftables action on the same set.

## 8. Mistakes to avoid
1. Rollback left to the browser or to a session that can drop (the MikroTik problem).
2. Rollback without verifying actual state (LP#1768802).
3. Timer too short: 60 s is not enough, 120 s configurable is better, with automatic reconnection.
4. Trial not protected against a reboot: the boot guard is mandatory.
5. 802.3ad bond or management VLAN with the switch not ready: warn, and always run the timed test.
6. "Deny all" firewall without anti-lockout (DSM case).
7. Tailscale `accept-routes` enabled on the NAS.
8. Cold migration from ifupdown done remotely with no way back.
9. Renaming interfaces live.
10. Touching synodeb's drop-ins or the image's `99-fallback.network` (they must be imported, not deleted).
11. Judging a migration by addresses alone: networkd without systemd-resolved leaves the NAS without DNS (§4.1).

## 9. Phases

| Phase | Content |
|---|---|
| **1** | read-only view of all interfaces and statistics; backend detection; editing IP/DHCP/DNS/gateway/MTU/hostname **on networkd only** with transaction, timer, boot guard and watchdog; link-local + Avahi; `rynas-web` anti-lockout; reset gesture in the synology plugin; ping, traceroute, DNS; serial console docs |
| **2** | guided migration from ifupdown; bonds, VLANs, `br0` bridge (for D-LXC and Docker); IPv6; WoL in the UI; iperf3, DF test, LLDP; **Tailscale plugin** (serve, subnet router, exit node); nftables **firewall** with profiles, DOCKER-USER, auto-block, rollback |
| **3** | WireGuard plugin (road warrior with QR, site-to-site); Headscale as an app; per-container Tailscale; policy routing / multi-gateway; NetBird / OpenVPN only as apps |

## 10. Sources
- TrueNAS: https://www.truenas.com/docs/scale/26/network/interfaces/settingupbridge/ · https://www.truenas.com/community/threads/cannot-create-a-bridge-how-to-diagnose.115227/ · https://www.truenas.com/community/threads/ip-forwarding-between-nics-for-tailscale.109364
- NetworkManager Checkpoint: https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.Checkpoint.html
- netplan: https://bugs.launchpad.net/ubuntu/+source/netplan.io/+bug/1768802
- Proxmox: https://lore.proxmox.com/all/20220602092251.1393709-1-a.lauterer@proxmox.com/t · https://forum.proxmox.com/threads/ifupdown2-networking-issues-apply-configuration-will-not-bring-up-interfaces.135033
- pfSense: https://forum.netgate.com/topic/146909/cannot-log-in-to-web-gui-after-lan-ip-change
- MikroTik / Junos: https://forum.mikrotik.com/t/any-chance-of-a-test-mode-before-applying-the-configuration/124600/1 · https://www.juniper.net/documentation/us/en/software/junos-space22.2/junos-space-workspaces/topics/concept/confirmed-commit-from-platform.html
- DSM reset: https://www.synology.com/knowledgebase/DSM/tutorial/General_Setup/How_to_reset_my_Synology_NAS_7
- Lockout with bonds and VLANs: https://wundertech.net/how-to-set-up-link-aggregation-on-a-synology-nas-lacp · https://community.cisco.com/t5/switching/problem-with-link-aggregation-sg350-and-synology-nas/m-p/4721463 · https://www.synoforum.com/threads/dsm7-upgrade-gone-wrong.7909/
- Debian: https://lists.debian.org/debian-boot/2025/06/msg00086.html · https://lists.debian.org/debian-user/2025/08/msg00288.html · https://lists.debian.org/debian-devel/2025/09/msg00133.html
- systemd: https://www.freedesktop.org/software/systemd/man/latest/networkctl.html · https://www.freedesktop.org/software/systemd/man/latest/systemd.network.html · https://www.freedesktop.org/software/systemd/man/255/org.freedesktop.network1.html
- Bonding: https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/7/html/networking_guide/overview-of-bonding-modes-and-the-required-settings-on-the-switch
- Tailscale: https://tailscale.com/docs/reference/tailscale-cli/up · https://tailscale.com/docs/reference/tailscale-cli/serve · https://tailscale.com/kb/1131/synology · https://docs.unraid.net/unraid-os/system-administration/secure-your-server/tailscale
- WireGuard and mesh: https://mintlify.com/wg-easy/wg-easy/features/qr-codes · https://www.bitdoze.com/netbird-vs-headscale-vs-tailscale/
- DNS with networkd (§4.1): https://www.freedesktop.org/software/systemd/man/latest/systemd-resolved.service.html · https://www.freedesktop.org/software/systemd/man/latest/systemd.network.html (`UseDNS=`, `DNS=`) · https://docs.debops.org/en/master/ansible/roles/resolved/getting-started.html (resolved a separate package since Debian 12, symlink on install) · https://forum.armbian.com/topic/39088-armbian-build-pr-remove-debootstrapped-static-resolvconf/ · https://www.github.com/openmediavault/openmediavault/issues/1710 · https://dietpi.com/forum/t/no-internet-after-power-outage/23283?page=2 (OMV: netplan + resolved, stub link) · https://manpages.debian.org/NetworkManager.conf (`rc-manager`) · https://tailscale.com/docs/reference/linux-dns · https://tailscale.com/blog/sisyphean-dns-client-linux · https://github.com/tailscale/tailscale/issues/7655 · https://gist.github.com/IslamAlam/0c9e9dc42126d60cfed66f4779fd8047 (Docker and 127.0.0.53) · https://www.truenas.com/community/threads/truenas-scale-not-setting-dns-server-from-dhcp.99309/latest (TrueNAS writes resolv.conf itself, DHCP race)

**To verify on the NAS before implementing:**
- whether `tailscale up --json` returns the AuthURL in the installed version;
- maximum MTU of the `atlantic` driver on the AQC113;
- that the double-press gesture also works on the DS920+, where `a` arrives with a short press ([`platforms/geminilake/README.md`](https://github.com/rylos/synodeb/blob/main/platforms/geminilake/README.md)).
