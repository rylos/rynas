# Web access: reverse proxy, app exposure, certificates and remote access

Research of 2026-11-07, at Marco's request: "anche la parte nginx è da fare in modo migliore di tutti e professionale e semplice: ogni app docker deve poter essere esposta". Here "nginx" means the whole web exposure layer: reverse proxy, names, certificates, access control and remote access.

This report re-checks D-PROXY and D-EXPOSE (`../phase0/05-decisions.md`) and extends topic 1 of `reverse-proxy-app-store.md`. It builds on `docker.md`, `network.md` and `../phase2/docker-apps.md`. Sources are listed at the end of each section. **(unverified)** marks claims I could not confirm from a primary source.

## Short answer

- **Engine: Caddy stays.** It is the only engine with all three of these:
  - an API that loads a complete configuration atomically and rolls back on failure;
  - DNS-01 through compiled-in provider modules;
  - ARI and ACME profiles.
- **D-PROXY stays in direction but changes in seven places:**
  1. **Caddy is always on and owns ports 80/443 from install.** It is no longer an optional module that takes the ports only when switched on. `rynas-web` keeps its own HTTPS port as the door that never closes.
  2. **The exposure model splits *names* from *who may connect*.** Per app: a local name, a name on your own domain, a Tailscale name, a tunnel. Separately, an access level of LAN, LAN + VPN or public.
  3. **The LAN gets trusted HTTPS without opening a port.** The path is a wildcard certificate via DNS-01 plus a DNS record that rynas itself creates. Without a domain, rynas uses single-label mDNS names with its own local CA, honest about the limits.
  4. **Proxied apps bind to `127.0.0.1` only.** Their ports are never open on the LAN unless the user asks.
  5. **Forward auth to the rynas login uses its own SSO cookie with per-app groups,** never the admin session. Each app also declares paths that skip the login wall, for mobile apps and APIs.
  6. **The access check matches on the tailnet ranges, backed by an interface check.** Not on the whole 100.64.0.0/10 range alone, which is also ISP CGNAT.
  7. **The fallback changes.** nginx with ACME is no longer a drop-in fallback: the official ACME module has no DNS-01. The route model is engine-neutral, with an Angie/nginx renderer kept as a contingency.
- **D-EXPOSE gets a precise definition:**
  - new routes are LAN-only;
  - public needs a domain, an app login or the rynas login, and a typed confirmation;
  - nothing becomes public as a side effect of anything else.

---

## 1. Engines (state on 2026-11-07)

| Engine | Latest | Programmatic config | Atomic apply / rollback | ACME | DNS-01 / wildcard | Fit for rynas |
|---|---|---|---|---|---|---|
| **Caddy** | 2.11.9 (2026-11-05) | full JSON over the admin API, Unix socket | `POST /load` blocks until the new config runs; on failure the old one stays, no downtime; 400 on invalid config since 2.11.6 | built in (CertMagic): HTTP-01, TLS-ALPN, DNS-01, ARI, profiles (experimental, 2.10+), on-demand TLS with `ask` | yes, through libdns modules compiled in (xcaddy) | **best**. Weak spot: WebSockets are closed on every reload until 2.12 (`stream_detached`, merged 2026-10-04, not released) |
| nginx | 1.30.6 stable / 1.31.8 mainline | text files, `nginx -t`, graceful reload, no write API | reload is graceful; old workers live as long as their WebSockets unless `worker_shutdown_timeout` is set | official `ngx_http_acme` 0.4.1 (Rust): HTTP-01 and TLS-ALPN only, profiles, ARI | **no DNS-01**, so no wildcard | good engine, poor appliance fit; ACME for LAN-only names impossible without an external client |
| Angie | 1.12.4 | nginx-style files, read-only status API | as nginx | built in since 1.8: HTTP-01, ALPN, profiles, IP certificates | DNS-01 only through its own DNS responder with NS delegation, or hooks | best nginx-family contingency, smaller ecosystem |
| HAProxy | 3.4.7 LTS | runtime API + Data Plane API | hitless reload | 3.2 experimental HTTP-01; 3.3/3.4 add DNS-01 through Data Plane API, `dns-persist-01` | yes (3.4) | excellent L4/L7, but certificates and DNS live in a second daemon; overkill |
| Traefik v3 | 3.7.17 | file/Docker/HTTP providers, hot reload | per-provider | built in, `acme.profile`; ARI unconfirmed | yes, many providers | label-driven, hard for an average user; broke on Docker 29 (API 1.44) until 3.6.1; ~2× the memory of others (anecdotal) |
| Nginx Proxy Manager | 2.16.0 | undocumented internal API | — | certbot | yes (plugins; certbot upgrades broke them several times) | app only; ~827 open issues; RCE fixed in 2.16.0 |
| Zoraxy | 3.3.4 (3.3.5-rc) | GUI | — | lego v5 | yes | app only; essentially one maintainer; AGPL |
| Pangolin | 1.26.0 | GUI/API on a VPS | — | Traefik | yes | a tunnelled proxy for exposure without open ports; AGPL CE + commercial; app/remote option |
| caddy-docker-proxy | 2.13.1 | container labels → Caddyfile | graceful reload | as Caddy | as Caddy | labels as the source of truth contradict D-CFG; not used |
| Cosmos | 0.23.4 | own UI | — | built in | yes | "Apache 2.0 with Commons Clause and Anti Tampering Clause": **not open source**; not used |

**Why Caddy wins for an appliance:**
- the configuration is data (JSON) pushed whole over a permissioned Unix socket;
- validation and rollback happen inside the engine;
- WebSocket, HTTP/2 and HTTP/3 need no setup;
- ARI comes built in, and so do ACME profiles, which matter for 45-day certificates;
- `forward_auth` is built in;
- `ask`-gated on-demand TLS;
- small memory footprint, measured by rynas rather than taken from blogs (see risks);
- DNS-01 for LAN-only and Tailscale-only names.

**What must be handled:**
1. **Package.** Debian trixie ships Caddy **2.6.2**, with 4 open CVEs on the tracker. trixie-backports has 2.11.2, still below the `forward_auth` fixes. Go packages get only "limited security support" in Debian. rynas therefore ships its own `rynas-caddy`:
   - built with xcaddy from a **pinned upstream release** plus a short list of DNS modules;
   - rebuilt by CI on every upstream release;
   - minimum version **2.11.9** today, and never below **2.11.4**. CVE-2026-30851 let a client inject identity headers through `forward_auth`'s `copy_headers`, fixed in 2.11.2, with a related underscore-alias issue fixed in 2.11.4.
   - Never `caddy add-package` or `caddy upgrade`, which rewrite the binary on disk.
2. **Reloads close WebSockets.** Rules for rynas:
   - reload only when the user applies a change, never on container restarts (upstreams are fixed loopback ports, §5);
   - batch changes into one `/load`;
   - set `stream_close_delay` so upgraded connections stay open for a few minutes after a reload;
   - switch to `stream_detached` as soon as 2.12 ships.
3. **The admin API is attack surface.** CVE-2026-27589 was a `no-cors` request to the admin API, and 2.11.3 fixed a remote-admin auth bypass. rynas therefore:
   - runs the API **only** on a Unix socket with mode 0600 in `/run/rynas-caddy/`;
   - never opens TCP `:2019`;
   - turns remote admin off.
4. **Boot without rynasd.** Caddy must come up with the last good configuration even when rynasd is down. rynasd writes `/var/lib/rynas-caddy/last-good.json` after every successful `/load`. The unit starts with `caddy run --config last-good.json`, with `persist_config off`, so there is exactly one copy of the truth on disk.

Sources: https://github.com/caddyserver/caddy/releases · https://github.com/caddyserver/caddy/releases/tag/v2.10.0 · https://caddyserver.com/docs/api · https://caddyserver.com/docs/caddyfile/options · https://caddyserver.com/docs/caddyfile/directives/forward_auth · https://caddyserver.com/docs/caddyfile/directives/tls · https://caddyserver.com/docs/command-line · https://github.com/caddyserver/caddy/pull/7649 · https://caddy.community/t/forced-refreshes-happening-for-website-using-websockets/33301 · https://www.kodemsecurity.com/cve-archive/cve-2026-30851 · https://tracker.debian.org/pkg/caddy · https://www.debian.org/releases/trixie/release-notes/issues.en.html · https://nginx.org/ · https://nginx.org/en/docs/http/ngx_http_acme_module.html · https://github.com/nginx/nginx-acme · https://trac.nginx.org/nginx/ticket/2558 · https://en.angie.software/angie/docs/configuration/modules/http/http_acme/ · https://www.haproxy.com/blog/announcing-haproxy-3-4 · https://github.com/traefik/traefik/releases · https://forums.docker.com/t/docker-29-increased-minimum-api-version-breaks-traefik-reverse-proxy/150384 · https://github.com/NginxProxyManager/nginx-proxy-manager/releases · https://github.com/tobychui/zoraxy/releases · https://github.com/fosrl/pangolin · https://github.com/lucaslorentz/caddy-docker-proxy · https://github.com/azukaar/Cosmos-Server/blob/master/LICENCE

Unverified:
- Memory of each engine with ~30 hosts. No rigorous 2025-26 benchmark exists; rynas measures it on the DS920+.
- Whether ACME is still experimental in HAProxy 3.4.
- Whether nginx.org builds `nginx-module-acme` for trixie.
- The release date of Caddy 2.12.

---

## 2. How the best platforms expose apps

| Platform | How an app gets a URL | Certificates | Access control | Remote access | What users love / hate |
|---|---|---|---|---|---|
| Synology DSM | Login Portal → Reverse Proxy, one rule per host or port | Let's Encrypt over HTTP-01 only; no DNS-01 for custom domains (users script acme.sh) | access-control profiles (IP lists) | QuickConnect (DSM apps only, relay is slow), DDNS | love: system panel + one certificate manager. Hate: WebSocket is a manual "custom header"; hand edits overwritten by updates; one nginx for everything; a failed DNS check at boot can break the config |
| TrueNAS 25.x | none built in: IP:port; users install NPM/Caddy/Traefik from the catalog | ACME DNS-01 only for the UI certificate | — | Tailscale app | hate: no ingress at all; sharing the Tailscale socket with a proxy is fiddly |
| Unraid 7 | IP:port; per-container "Use Tailscale" switch makes each container a tailnet node, Serve/Funnel as toggles | `myunraid.net` hashed names with valid certificates | Funnel/Serve add no auth | Unraid Connect, WireGuard, Tailscale | love: per-container Tailscale. Hate: Funnel flakiness; strict mode can lock you out locally |
| Umbrel | per-app `app_proxy` that checks the umbrel session on every request and WebSocket | — | one login wall | Tailscale app, tunnel guides | hate: the login wall breaks third-party/mobile clients (`PROXY_AUTH_ADD=false` workaround) |
| CasaOS / ZimaOS | IP:port | — | — | ZeroTier-based remote ID (unverified) | moving the UI off 80/443 to free them for a proxy broke the client until ZimaOS 1.5 |
| YunoHost | domain **or** path chosen at install; some apps need a whole domain | Let's Encrypt per domain + diagnosis | per-app permission: visitors (public) or groups; changeable later without reinstall; SSO portal with tiles | — | love: permissions as groups, change without reinstall, honest "needs full domain" |
| Home Assistant ingress | add-on served under HA's URL and auth (`X-Ingress-Path`) | HA's | HA login | HA cloud | hate: apps must handle a path prefix; native clients cannot pass HA auth |
| Cosmos | a URL per container, subdomains (docs advise against path prefixes) | built in | per-route OIDC SSO + "SmartShield" throttling | — | SmartShield banned the owner's own IP (anecdotal) |
| Runtipi | "expose" switch + domain → Traefik + Let's Encrypt; `app.tipi.lan` needs your own DNS rewrite | Let's Encrypt | — | tunnel guides | local names fail because the router does not know them |
| Cloudflare Tunnel | per-hostname ingress via cloudflared | Cloudflare terminates TLS (sees traffic) | Access policies (free ≤ 50 users) | outbound only | hate: **100 MB request body cap on Free/Pro** (photo/video uploads fail), video ToS ambiguity |
| Tailscale | Serve (tailnet), Funnel (public, ports 443/8443/10000, `*.ts.net` only), **Services** (`svc:` names, beta since 2025-10-28: a stable name per service, host advertises it, admin approves) | Let's Encrypt for `*.ts.net` (names go into CT logs) | tailnet ACLs | built in | love: zero open ports, real certificates. Hate: Funnel has no auth and fixed ports; ECDSA certs rejected by some apps |
| Pangolin / NetBird | identity-aware proxy on a VPS + outbound connector (Newt) / NetBird reverse proxy (beta) warns when a service has no auth | built in | SSO, PIN, IP/geo rules | outbound tunnel | self-hosted Cloudflare Tunnel without its body cap; needs a VPS |

**Lessons from incidents:**
- **QNAP DeadBolt (2022)** hit internet-exposed NAS, and QNAP's advice was to turn off port forwarding and UPnP.
- **eCh0raix** has targeted Synology and QNAP since 2021.
- **Synology CVE-2024-10443 (RISK:STATION)** was a zero-click RCE in Photos. It was reachable through port forwarding **and QuickConnect**, with up to ~2M devices exposed.

Rule: public exposure is a per-app, informed and reversible choice, never a platform-wide relay that is on by default.

**Subdomain vs path:**

| App | Subpath |
|---|---|
| Immich | not supported; feature request closed |
| Home Assistant | not supported; decision called "final" |
| Nextcloud | possible via `overwritewebroot`, redirect loops common |
| Jellyfin | works, but breaks DLNA, Chromecast and the *arr apps |
| Paperless-ngx | needs two environment variables |
| Vaultwarden | works with `DOMAIN` |

**Conclusion: one hostname per app, never path prefixes.** The catalog keeps a `subpath` flag only for the rare app that needs it.

Sources: https://www.synoforum.com/resources/synology-reverse-proxy-under-the-hood.135/updates · https://github.com/yegle/dsm-reverse-proxy-websocket · https://github.com/JessThrysoee/synology-letsencrypt · https://tailscale.com/kb/1131/synology · https://forums.truenas.com/t/new-app-catalogue-request-caddy-reverse-proxy/47490 · https://docs.unraid.net/go/tailscale-docker/ · https://docs.unraid.net/unraid-os/system-administration/secure-your-server/securing-your-connection · https://github.com/getumbrel/umbrel · https://community.umbrel.com/t/connecting-helmarr-or-any-third-party-client-to-transmission-on-umbrelos-no-way-past-the-auth-proxy/25120 · https://community.zimaspace.com/t/zima-app-windows-no-connection-after-changing-ports-on-zimaos/6510 · https://doc.yunohost.org/admin/apps/ · https://github.com/YunoHost/yunohost/pull/797 · https://cosmos-cloud.io/docs/urls · https://www.runtipi.io/docs/guides/expose-your-apps · https://forums.runtipi.io/t/local-expose-with-local-domain-fritz-box/70 · https://developers.cloudflare.com/workers/platform/limits/ · https://tailscale.com/kb/1223/funnel · https://tailscale.com/docs/features/tailscale-services · https://tailscale.com/blog/services-beta · https://tailscale.com/kb/1153/enabling-https/ · https://docs.netbird.io/manage/reverse-proxy · https://www.helpnetsecurity.com/2022/09/06/deadbolt-hitting-qnap-nas/ · https://www.helpnetsecurity.com/2024/11/04/cve-2024-10443/ · https://docs.immich.app/administration/reverse-proxy/ · https://github.com/home-assistant/architecture/issues/156 · https://jellyfin.org/docs/general/post-install/networking/ · https://github.com/nextcloud/server/issues/20338 · https://github.com/dani-garcia/vaultwarden/wiki/Using-an-alternate-base-dir

---

## 3. Names, certificates and DNS

### 3.1 Certificate landscape in 2026

| Date | Change |
|---|---|
| 2025-05-07 | OCSP URLs dropped from Let's Encrypt certificates |
| 2025-08-06 | OCSP responders off; revocation via CRL only. OCSP stapling is no longer a concern |
| 2026-01-15 | 6-day (`shortlived`) and IP-address certificates GA (IP: public IPs only) |
| 2026-02-11 | client-auth EKU removed. A Let's Encrypt certificate is never usable for mTLS |
| 2026-03-15 | CA/B SC-081: max validity and DCV reuse 200 days |
| 2026-05-13 | Let's Encrypt `tlsserver` profile: 45 days |
| 2027-02-10 | `classic`: 64 days, 10-day authorization reuse |
| 2027-03-15 | SC-081: 100 days |
| 2028-02-16 | `classic`: 45 days, 7-hour authorization reuse |
| 2029-03-15 | SC-081: 47 days, DCV reuse 10 days |

**Consequences:**
- ARI (RFC 9773) must be on. Renewal is driven by the CA's window, or 2/3 of the lifetime, never a fixed number of days. ARI renewals are also exempt from rate limits.
- The ACME account key persists across reinstalls, as part of the config export, so the 5 duplicates/week limit cannot bite on reinstall loops.
- **One wildcard per domain instead of per-app certificates.** It needs fewer orders and fewer rate-limit hits, and app names do not leak into Certificate Transparency logs.
- DNS-PERSIST-01 (one standing TXT record, no API token on the NAS) is the future low-privilege option. Its production status is (unverified); plan it, don't depend on it.

### 3.2 The four ways to name an app

| Mode | Example | Trusted HTTPS? | Works where | Needs |
|---|---|---|---|---|
| **Own domain, LAN** | `immich.nas.example.com` → 192.0.2.10 | yes, public CA, wildcard via DNS-01 | every device on the LAN, all mobile apps | a domain + DNS API token (or CNAME delegation); **no open port** |
| **Local name** | `immich-nas.local` | only with the rynas local CA installed | browsers on devices that trust the CA; mobile apps usually not (Android 7+ apps ignore user CAs) | nothing |
| **Tailscale** | `immich.tailnet-name.ts.net` | yes, issued by Tailscale | any device on the tailnet, at home or away | Tailscale plugin |
| **Public** | `immich.nas.example.com` reachable from the internet | yes | everyone | domain + port forward 443 (or tunnel) + explicit confirmation |

**Key finding: the own-domain-on-LAN mode is the best everyday experience, and almost nobody makes it easy.** DSM has no DNS-01 for custom domains. TrueNAS limits DNS-01 to its UI certificate. Runtipi users fight local DNS. rynas does the whole thing in one form. The user gives a domain (ideally a subzone such as `nas.example.com`) and a DNS provider token, and rynas then:
1. obtains `*.nas.example.com` + `nas.example.com` via DNS-01;
2. **creates the DNS record itself** through the same libdns provider: `*.nas.example.com A 192.0.2.10` (and AAAA if the LAN has stable IPv6). Every LAN device then resolves app names with no router change;
3. keeps the record updated when the NAS address changes (rynas-net knows).

Pitfalls handled in the checklist:
- **DNS rebind protection.** dnsmasq/OpenWrt drops public answers that point to RFC 1918 addresses by default, and some ISP routers do the same. The checklist detects it: the name resolves through a public resolver but not through the LAN resolver. It then offers two fixes:
  - a router exception (instructions for OpenWrt `rebind_domain` and common routers);
  - a local DNS override, and when AdGuard Home or Pi-hole runs as a rynas app, rynas writes the record there.
- **Token scope.** Some providers cannot scope tokens (DigitalOcean, Namecheap). The form says so and offers **CNAME delegation** of `_acme-challenge` to a throwaway zone or acme-dns, so a leaked token cannot hijack the main zone. In that mode rynas cannot create the A record, so the user adds the wildcard record once (the form shows exactly what to add).
- **HSTS.** Never `preload`, never `includeSubDomains` by default. `includeSubDomains` on the parent domain would lock every LAN host under it into HTTPS for months. HSTS (max-age only) is sent on public routes.

**Local names without a domain:**
- **Single-label `<app>-<host>.local`, not `<app>.<host>.local`.** Multi-label `.local` names need a running alias publisher and resolve inconsistently (Windows and Android: unverified/flaky). Single-label names are plain A records of the NAS's primary IPv4 address, published by rynasd through Avahi's D-Bus API and republished when addresses change.
- **Not `/etc/avahi/hosts` (found on the first deployment, 2026-11-07, Debian trixie, avahi-daemon 0.8-16).** A static hosts entry publishes the address record *and* the reverse PTR record of the address. Every app name points to the NAS's own address, whose PTR the host name already owns, so Avahi rejects each entry: `Static host name <app>-<host>.local: avahi_server_add_address failure: Local name collision`. The hosts file has no option to skip the PTR (https://pi3g.com/?p=9174, https://lists.freedesktop.org/archives/avahi/2009-April/001625.html). The D-Bus API has one: `org.freedesktop.Avahi.EntryGroup.AddAddress(interface, protocol, flags, name, address)` with `AVAHI_PUBLISH_NO_REVERSE` (= 16 in `avahi-common/defs.h`, https://github.com/avahi/avahi/blob/master/avahi-common/defs.h), after `Server.EntryGroupNew` and before `EntryGroup.Commit` (https://github.com/avahi/avahi/blob/master/avahi-daemon/org.freedesktop.Avahi.EntryGroup.xml). Entry groups belong to the D-Bus connection that made them, so the publisher must be a long-lived process (rynasd), and Avahi frees them when it disconnects. AAAA records are not published for local names; the own-domain mode handles IPv6.
- **Local CA.** Certificates come from Caddy's local CA (`pki`), with a 10-year root and short-lived leaves. The "Trust this NAS" page gives:
  - a root download;
  - per-OS steps: Windows/macOS store; iOS profile **plus** the Certificate Trust Settings toggle; Firefox on Linux needs a policy; Android: browser only;
  - a QR code for phones.
- **Honest limit, shown in the UI:** "Native mobile apps (Immich, Home Assistant, Nextcloud…) usually refuse local certificates. Use your domain or Tailscale for them."
- **Do not use names publicly unresolvable but CA-impossible**, such as `.internal` or `.home.arpa`. They are valid private names, but no public CA issues for them, so they add nothing over `.local` + local CA unless the user runs their own DNS. They are offered as an "Advanced: custom local domain" option.

**rynas-web emergency port** (`:8443`, see §4):
- uses its own self-signed certificate (SANs: hostname, `hostname.local`, IPs);
- automatically switches to the domain wildcard when one exists. rynasd copies the active certificate into `/etc/rynas/tls/ui.{crt,key}` (group `rynas`, key 0640; W0 moved it from `/var/lib/rynas/certs`, which rynas-web cannot enter) and rynas-web reloads it by itself;
- never depends on Caddy running.

Sources: https://letsencrypt.org/2024/12/05/ending-ocsp/ · https://letsencrypt.org/2025/05/14/ending-tls-client-authentication/ · https://letsencrypt.org/docs/profiles/ · https://letsencrypt.org/2025/12/02/from-90-to-45/ · https://letsencrypt.org/2025/07/01/issuing-our-first-ip-address-certificate.html · https://www.rfc-editor.org/info/rfc9773 · https://letsencrypt.org/2026/02/18/dns-persist-01.html · https://letsencrypt.org/docs/rate-limits/ · https://www.sectigo.com/root-causes/root-causes-486-47-day-maximum-term-ballot-passes-cabf · https://community.letsencrypt.org/t/follow-up-of-dns-providers-who-easily-integrate-with-let-s-encrypt-dns-validation/195364 · https://forum.openwrt.org/t/nip-io-doesnt-work-for-local-network-behind-lede-solved-dns-rebind-protection/7766 · https://hstspreload.org/removal/ · https://support.apple.com/en-ca/102390 · https://wiki.mozilla.org/CA/AddRootToFirefox · https://github.com/Tugzrida/avahi-aliases · https://en.wikipedia.org/wiki/.internal

---

## 4. Architecture

```
                 LAN / tailnet / internet
                          │
          :80  :443 (tcp+udp h3)          :8443 (emergency, always)
               │                                  │
        ┌──────▼───────────┐               ┌──────▼──────────┐
        │  rynas-caddy     │──────────────►│  rynas-web       │ rustls, own cert
        │  (systemd, host) │  127.0.0.1:8080 (non-root)        │ or copied wildcard
        │  JSON from rynasd│               └──────┬──────────┘
        └──┬───────────▲───┘                      │ unix socket
           │           │ POST /load (unix 0600)   │
           │      ┌────┴─────────────────────────▼┐
           │      │ rynasd: web_access domain      │ config.db → render → /load
           │      │ (D-CFG pending/apply, revisions, last-good.json, probes)
           │      └────────────────────────────────┘
           ▼
   127.0.0.1:<port>  →  app containers (ports published on loopback only)
```

**Ownership of ports:**
- **80/443 belong to `rynas-caddy` from install**, also on systems without Docker. It serves the UI at `https://<host>.local` (local CA) and at `https://nas.example.com` when a domain exists. Port 80 only redirects to HTTPS and answers HTTP-01 challenges.
- **`:8443` belongs to `rynas-web`** and never goes through Caddy:
  - it listens on all addresses, link-local included;
  - it is covered by the firewall anti-lockout;
  - it is the DSM-5001 equivalent.
  - The current `0.0.0.0:8080` plain-HTTP listener becomes `127.0.0.1:8080` (Caddy's upstream) once `:8443` with rustls lands. Done in W1b (2026-11-07, `../phase1/plan.md` "Behind the built-in reverse proxy (W1b)"): `--listen auto` moves it once rynasd says the proxy fronts the interface and `:8443` is up; rynas-web trusts `X-Forwarded-For` only from the loopback proxy on that port (right-most entry).
- **External proxy mode.** A user who prefers NPM, Traefik or Zoraxy turns on "Use another reverse proxy" (Advanced). rynas-caddy stops and releases 80/443, and the port check stops blocking an app that publishes 443. The UI is then only on `:8443`. rynas still shows each app's loopback port so the user can point their proxy at it.

**Config generation and apply:**
1. The `web_access` key in `config.db` (§6) is the only source.
2. On apply, rynasd renders the full Caddy JSON:
   - one server for `:443` with a route per hostname;
   - a catch-all that **aborts** unknown hosts (no default site, no information leak);
   - a `:80` server for the redirect and ACME.
3. Validation is done by `/load` itself, which leaves the running config untouched on error; the 400 body becomes the task error. On success rynasd writes `last-good.json` and a revision.
4. A change that touches the UI's own route uses D-NET's timed confirmation. If the UI does not confirm through the new path within 120 s, the previous revision is loaded. `:8443` stays reachable throughout, so nobody is locked out.
5. Anti-drift: rynasd compares `GET /config/` with its render after Caddy restarts or rynasd starts, and re-loads when they differ.

**Service hardening:**
- `rynas-caddy` runs as its own user with `CAP_NET_BIND_SERVICE` only;
- its state is in `/var/lib/rynas-caddy` (certificates, local CA, ACME account);
- `ProtectSystem=strict`;
- `MemoryHigh` around 128 MB (tuned after measurement);
- `GOMEMLIMIT` set below it;
- access logs as JSON to journald, which feeds auto-block and diagnostics.

---

## 5. Docker networking

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| **Publish on `127.0.0.1:<port>`, Caddy on host proxies to it** | works for every stack and manager; stable across restarts (no reload); since Docker 28 loopback-published ports are not reachable from the LAN (direct-routing drop in raw PREROUTING); no Docker dependency for Caddy | the app sees the Docker gateway as the client (docker-proxy) and must trust `X-Forwarded-For` from Docker subnets; one host port per app | **default** |
| Caddy reaches container IPs on a `rynas-proxy` bridge, no published ports | no host ports, real source IP | container IPs change → config reload on restarts (drops WebSockets) unless fixed `ipv4_address`, which means editing the compose | phase 3 option, catalog apps only |
| Proxy in a container on a shared network | the Traefik/NPM way | the proxy would depend on Docker; the UI would vanish when Docker is down | rejected |
| App ports open on the LAN | the DSM/TrueNAS default | published ports **bypass the host firewall** (DNAT before INPUT; `DOCKER-USER` needs `--ctorigdstport`) | only as the user's explicit "also open port on LAN" |

**Rules:**
- **Catalog apps bind to loopback.** Their compose uses `${RYNAS_BIND:-127.0.0.1}:${APP_PORT}:<container port>`. That is plain `${VAR}`, so D-APPCAT's no-templating rule holds. "Also open port on LAN" sets `RYNAS_BIND=0.0.0.0`.
- **Non-HTTP ports** (Syncthing 22000, Jellyfin DLNA, MQTT 1883) are declared separately in `app.toml` `ports` and published on the LAN as before. The web route does not touch them.
- **Custom and foreign stacks** (Portainer, Dockhand, untracked) can be proxied to whatever port they publish. When that port is also open on 0.0.0.0, the route card says so ("the app is also reachable without the proxy, bypassing its access rules"). It offers the one-line change to `127.0.0.1:` but never edits a foreign stack on its own.
- **Host-network apps** (Home Assistant) are proxied to `127.0.0.1:8123` like any other. Their port is open on the LAN by nature; the route card says so.
- **Upstream address.** It is `127.0.0.1:<port>`, or `[::1]` when the app publishes only IPv6. Containers do not need IPv6 for the NAS to serve them over IPv6, because Caddy terminates on the host.
- **No dynamic discovery from labels.** Routes are explicit rows. The catalog's `web` block creates them; for other stacks the user picks the container and port from a list built from the Docker API.
- **Firewall.** The nftables firewall plugin (D-NET) opens 80/443 TCP and 443 UDP (HTTP/3) for the profiles in use, and `:8443` per anti-lockout. Docker's own NAT for loopback ports needs no rule.

Sources: https://docs.docker.com/engine/network/packet-filtering-firewalls/ · https://docs.docker.com/engine/network/firewall-iptables/ · https://docs.docker.com/engine/network/port-publishing/ · https://www.docker.com/blog/docker-engine-28-hardening-container-networking-by-default/ · https://docs.docker.com/engine/network/firewall-nftables/ · https://blog.ipspace.net/kb/DockerSvc/40-userland-proxy/

---

## 6. Exposure model (data model)

Everything lives in the `web_access` key (JSON domain in `config.db`, D-CFG).

```toml
[settings]
enabled = true              # false = external proxy mode (80/443 released)
https_port = 443            # http_port = 80, h3 = true
ui_names = ["local", "domain"]
emergency_port = 8443       # informational; owned by rynas-web
local_names = true          # publish <app>-<host>.local via Avahi

[[domains]]
id = "d1"
name = "nas.example.com"    # a subzone is recommended
validation = "dns01"        # dns01 | dns01_delegated (CNAME/acme-dns) | http01 | manual
credential = "c1"           # encrypted, reusable (TrueNAS "DNS authenticator" idea)
wildcard = true
manage_records = true       # rynas writes *.nas.example.com A/AAAA = NAS LAN address
public_ip_record = false    # DDNS when a route on this domain is public

[[credentials]]
id = "c1"
provider = "cloudflare"     # module list in rynas-caddy
secret = "<encrypted>"

[[routes]]
id = "r-immich"
kind = "app"                # app | stack | ui | custom (any URL, e.g. another host on the LAN)
app = "immich"              # or stack + service
upstream = "http://127.0.0.1:2283"
names = { local = true, domain = { id = "d1", label = "immich" }, tailscale = false, tunnel = "" }
access = "lan"              # lan | lan_vpn | public
allow = []                  # extra CIDRs (office, a friend's IP)
auth = { mode = "app" }     # none | app | rynas{groups=[..], bypass=["/api/*"]}
limits = { body = "50G", timeout = "600s", buffering = false }
headers = "default"         # default | relaxed (allow framing) | custom table
enabled = true
```

**Catalog `web` block** (extends D-APPCAT's `web{port,path,websocket,auth}`):

```toml
[web]
port = 2283                 # container port; host port allocated (native if free)
path = "/"                  # landing path for the Open button
auth = "app"                # app (own login) | none (needs rynas login before LAN+VPN/public) | proxy (supports header auth)
subpath = false             # true only for the rare app that supports it
body = "50G"; timeout = "600s"; buffering = false
bypass = ["/api/*", "/.well-known/*"]  # paths mobile/API clients use when auth=rynas
public = "ok"               # ok | warn | never (e.g. admin tools, Docker managers)
trusted_proxies_env = { IMMICH_TRUSTED_PROXIES = "172.16.0.0/12,10.200.0.0/16" }  # set at render
```

Note: WebSocket is always on in Caddy, so the old `websocket` field becomes informational.

**Access levels:**

| Level | Allowed sources | Notes |
|---|---|---|
| `lan` (**default**) | the NAS's connected subnets (from rynas-net, IPv4 + IPv6 prefixes, link-local) + `allow` | other sources get a TCP-level abort, not a 403 page |
| `lan_vpn` | `lan` + tailnet (100.64.0.0/10 and fd7a:115c:a1e0::/48) + WireGuard subnets | the tailnet ranges are trusted only when they arrive on `tailscale0`: the firewall plugin drops 100.64.0.0/10 from any other interface (anti-spoof), because 100.64.0.0/10 is also ISP CGNAT |
| `public` | anyone | requires a domain, `auth` ≠ none (app login or rynas login), catalog `public` ≠ never, the checklist green, and a typed confirmation. Auto-block on |

**Trap: hairpin NAT.** Some routers SNAT port-forwarded or hairpinned traffic to the router's own LAN address. Internet clients would then look like LAN clients. The public-exposure test (§8) detects this:
- it calls the public URL through an outside probe (optional opt-in rynas probe, or the user's phone on mobile data);
- it checks the source address Caddy logged.

When the router address shows up for outside traffic, rynas warns that `lan` routes on the same port are not protected. It recommends turning off NAT loopback/SNAT for the forward, or using `lan_vpn` + Tailscale.

**Defaults at app install:**
- local name on;
- domain name on when a domain exists;
- access `lan`;
- auth from the catalog;
- Tailscale and public off.

The wizard's Access step shows this as a one-line summary with "Change". Nothing is ever public without the dedicated flow.

---

## 7. Authentication

| Option | RAM | Strength | Fit |
|---|---|---|---|
| **rynas login via `forward_auth`** | 0 (in rynas-web) | same users, 2FA/passkeys, groups | **built in**, the default for apps without their own login |
| Authelia 4.39 | ~30-50 MB (unverified) | OpenID Certified (2025-05), forward auth + OIDC | catalog app; rynas can point `forward_auth` at it |
| Pocket ID 2.19 | small | passkey-only OIDC | catalog app for OIDC-capable apps |
| Authentik 2025.10+ | min 2 GB | full IdP; Redis removed in 2025.10 | catalog app with high-resources antifeature |
| tinyauth, oauth2-proxy | small | simple | not needed with the built-in option |

**Built-in SSO design** (DSM Login Portal + YunoHost permissions):
- `rynas-web` gets `GET /api/v1/auth/verify` (forward_auth target) and a login page at `https://nas.example.com/sso/login?rd=…`.
- **Separate cookie:** `rynas_sso`, `Domain=nas.example.com` (covering the app subdomains), `Secure; HttpOnly; SameSite=Lax`, with its own token and timeouts. It grants access to apps only, **never** the admin API. The admin session cookie stays host-only and `SameSite=Strict`.
  - Reason: every app behind the proxy receives that cookie, and a compromised app container could replay it.
  - Caddy also strips `rynas_sso` from the `Cookie` header before proxying to the upstream.
- **Per-route groups.** `auth.rynas.groups` (default: `users`). Verify answers 2xx with `Remote-User`, `Remote-Groups`, `Remote-Email`. Caddy copies them with `copy_headers` and always strips client-supplied copies first: Caddy ≥ 2.11.4, plus an explicit `request_header -Remote-*` before `forward_auth` as defence in depth.
- **Bypass paths.** Mobile apps, WebDAV/CalDAV and API clients cannot follow an HTML login redirect. The catalog's `bypass` list (or the user's) skips the wall for those paths, and the app's own token auth protects them. `OPTIONS` preflight is always bypassed.
  - Apps with their own login (`auth = "app"`: Immich, Nextcloud, Jellyfin, Vaultwarden, Home Assistant) get **no** forward auth by default. Umbrel's lesson: a login wall in front of apps with mobile clients breaks them.
- **Local names.** The SSO cookie needs a shared parent, so on `.local` names forward auth works per host. The user logs in once per app host, through the same login page served on that host.
- **Phase 3: rynas as an OIDC provider** (Immich, Nextcloud user_oidc, Paperless, Grafana, Forgejo, Audiobookshelf support OIDC natively). It removes the need for a separate Authelia/Pocket ID on small boxes.

**Hardening for public routes:**
- **Auto-block.** Caddy's JSON access log is read by rynasd: repeated 401/403 on a route, and failed rynas SSO logins, go into the nft `@blocked` set (D-NET), which already exists for the UI login. LAN and tailnet are allowlisted.
- **Rate limit.** `caddy-ratelimit` has only a v0.1.0 tag (immature), so it is not in the default build. Auto-block plus per-app login limits cover the risk.
- **CrowdSec.** Optional (Advanced), as an app plus the Caddy bouncer module. The bouncer fails closed when AppSec is unreachable since 0.11; the UI warns.
- **Geo-blocking.** Optional. It needs the user's own free MaxMind GeoLite2 licence key (EULA), so it is never bundled.
- **Headers baseline:**
  - `X-Content-Type-Options: nosniff`;
  - `Referrer-Policy: strict-origin-when-cross-origin`;
  - `X-Frame-Options`/`frame-ancestors` `SAMEORIGIN` unless the app's preset is `relaxed` (HA dashboards, OnlyOffice);
  - remove `Server` and `X-Powered-By`;
  - HSTS `max-age=31536000` on public routes only, no `includeSubDomains`, no `preload`.
- **Client IP.** `trusted_proxies` stays empty, because Caddy is the edge. The exception is tunnel mode, where the connector's address is trusted. Apps get `X-Forwarded-For`/`-Proto` from Caddy; the catalog sets each app's trusted-proxy variable to the Docker address pools.

Sources: https://www.authelia.com/blog/we-are-now-openid-certified/ · https://github.com/authelia/authelia/releases · https://www.authelia.com/configuration/security/access-control/ · https://goauthentik.io/blog/2025-11-13-we-removed-redis/ · https://docs.goauthentik.io/install-config/install/docker-compose/ · https://github.com/pocket-id/pocket-id/releases · https://caddyserver.com/docs/caddyfile/directives/forward_auth · https://newreleases.io/project/github/hslatman/caddy-crowdsec-bouncer/release/v0.11.0 · https://github.com/mholt/caddy-ratelimit · https://tailscale.com/kb/1610/reserved-ip-addresses

---

## 8. Remote access without opening ports

| Option | How rynas offers it | Good for | Limits |
|---|---|---|---|
| **Tailscale** (plugin, D-NET phase 2) | (a) **`lan_vpn` access level**: the NAS's own tailnet name `nas.<tailnet>.ts.net` reaches the UI. Apps on the own domain are reached remotely through Tailscale **split DNS** for `nas.example.com` pointing at the NAS's tailnet address (rynas shows the one admin-console step; Caddy matches the tailnet ranges). (b) per-app **Tailscale Services** (`svc:<app>`): rynasd runs `tailscale serve --service=svc:<app> --https=443 127.0.0.1:<port>`, so the app gets `https://<app>.<tailnet>.ts.net` with a real certificate, no domain needed. Beta; each service needs admin approval in the console. | you and your family, mobile apps, no domain | Services is beta (unverified GA); Funnel (public) only on ports 443/8443/10000 and without auth, so it is offered only for apps with their own login and `public = ok` |
| **Cloudflare Tunnel** | a curated `cloudflared` app; rynas writes a locally-managed tunnel config with an ingress entry per public route pointing at `https://127.0.0.1:443` with the route's host (Caddy still applies auth and headers) | public sharing without opening ports | **100 MB request body cap on Free/Pro** (warned for Immich, Nextcloud, Paperless uploads); Cloudflare sees decrypted traffic |
| **Pangolin + Newt** | `newt` as a catalog app pointing at Caddy | self-hosted tunnel without the body cap | needs a VPS |
| **Port forward 443** | the public checklist: router forward, DDNS through the domain's DNS token, outside reachability test | the classic, fastest | the NAS is on the internet; only per-app `public` routes are answered |
| WireGuard | plugin, phase 3 (D-NET) | full network access | client setup |

**Recommendation shown in the UI:** "To reach your apps from outside, Tailscale is the safest choice. Use public exposure only to share an app with people outside your devices."

Sources: https://tailscale.com/docs/features/tailscale-services · https://tailscale.com/kb/1223/funnel · https://developers.cloudflare.com/workers/platform/limits/ · https://docs.pangolin.net/self-host/quick-install

---

## 9. UI

**Web access page:**
- **Overview:** health checklist + certificates expiring + routes with problems.
- **Routes:** apps, stacks, custom.
- **Domains & certificates.**
- **Remote access:** Tailscale, tunnels, port forward.
- **Trust this NAS:** local CA download, per-OS steps, QR.
- **Advanced:** headers presets, external proxy mode, HTTP/3, CrowdSec/geo, logs.

All of it lives under Network or as a top-level item, decided by the UI agent. Tables keep to the no-inner-horizontal-scroll rule: cards on narrow screens.

**App install wizard, "Access" step (one screen, sane defaults):**

```
┌ Install Immich ─────────────────────────────────── Step 3 of 4: Access ┐
│ Address                                                                │
│  ☑ https://immich.nas.example.com    LAN  ✓ certificate  ✓ DNS         │
│  ☑ https://immich-nas.local          LAN  local certificate ⓘ          │
│  ☐ Tailscale  https://immich.tailnet-name.ts.net                       │
│                                                                        │
│ Who can connect   (•) This network   ( ) + my VPN   ( ) Everyone ⚠     │
│ Login             Immich's own login (mobile apps keep working)        │
│ ▸ Advanced: upload limit 50 GB · timeout 10 min · open port on LAN ☐   │
│                                                  [Back]  [Install]     │
└────────────────────────────────────────────────────────────────────────┘
```

**"Make public" flow** (separate dialog, never inline). Each line is a live test with a fix button:

```
┌ Make "immich.nas.example.com" public ─────────────────────────────────┐
│ ✓ Domain nas.example.com, certificate valid until 2026-12-21 (ARI)    │
│ ✓ Immich has its own login                                            │
│ ✗ Port 443 not reachable from the internet   [How to forward 443 ▸]   │
│   or  [Use a tunnel instead ▸]  [Use Tailscale instead ▸]             │
│ ✓ Public DNS: immich.nas.example.com → 203.0.113.7 (DDNS on)          │
│ ⚠ Router hides client addresses (hairpin NAT)   [Details ▸]           │
│ ✓ Auto-block on (10 failures → 1 h)                                   │
│ Type the app name to confirm: [ immich        ]       [Make public]   │
└───────────────────────────────────────────────────────────────────────┘
```

**Route diagnostics** ("Test" button on every route). Each step names its fix:
1. DNS through the LAN resolver vs a public resolver (rebind detection);
2. TCP to Caddy;
3. TLS chain and expiry;
4. HTTP status through Caddy;
5. upstream `127.0.0.1:<port>` directly (container up? healthcheck?);
6. WebSocket upgrade where the catalog declares one;
7. source-address check (§6 hairpin).

**Certificate dashboard:** one row per certificate. It shows:
- names, issuer (Let's Encrypt / local CA / Tailscale / imported);
- profile and lifetime, next renewal (ARI window);
- last error in plain language ("DNS provider rejected the token: needs Zone.DNS edit");
- which routes and services use it (Caddy routes, `:8443`, and later FTP/mail/LDAP through the same store).

Notifications through D-NOTIFY come on renewal failure, 7 days before expiry, and on a DNS record that no longer matches.

---

## 10. Mistakes to avoid

1. **A proxy as the only way into the UI.** `:8443` is never routed through Caddy.
2. **Public by default or as a side effect.** No platform-wide relay; public is per route, with a checklist.
3. **HTTP-01 only.** It forces an open port 80 and rules out LAN-only certificates. DNS-01 is the default.
4. **Per-app certificates on a domain.** They leak app names into CT logs and burn rate limits. Use one wildcard.
5. **Path prefixes.** Use one hostname per app.
6. **A login wall in front of apps with mobile clients.** Respect the app's own login and support bypass paths.
7. **Trusting 100.64.0.0/10 by address alone.** Add the interface check.
8. **HSTS `includeSubDomains`/`preload` by default.**
9. **App ports on 0.0.0.0 behind a proxy.** The ports bypass the firewall and the proxy's rules.
10. **Reloading the proxy on container events.** It drops every WebSocket. Use fixed loopback upstreams.
11. **Shipping Debian's Caddy 2.6** or self-updating binaries. Ship a pinned `rynas-caddy` with fast security rebuilds.
12. **Hand-editable proxy files that the system overwrites** (DSM). Caddy has no files to edit. Custom needs go through a "custom route" with raw upstream options, or external proxy mode.

---

## 11. Recommendation

**Engine.** Caddy, as `rynas-caddy`:
- built with xcaddy from a pinned release, minimum 2.11.9;
- with the DNS modules for cloudflare, route53, ovh, hetzner, desec, duckdns, gandi, porkbun, digitalocean, rfc2136 and acmedns;
- rebuilt by CI on upstream releases, with a security SLA of days;
- 2.12 adopted for `stream_detached`.

The route model and renderer are engine-neutral (a `Renderer` trait). An Angie/nginx renderer plus rynas-side ACME is the documented contingency, not a maintained fallback.

**Architecture.**
- rynas-caddy owns 80/443 from install, with the admin API on a 0600 Unix socket.
- rynasd renders the full JSON from `config.db` and applies it with `/load`. It keeps `last-good.json` for boot, runs anti-drift, and uses timed confirmation for changes to the UI route.
- rynas-web serves `:8443` with rustls as emergency access.
- External proxy mode releases 80/443.

**Exposure.** Names (local `.local` / own domain / Tailscale / tunnel) are separate from access (`lan` default / `lan_vpn` / `public`).
- Public needs a domain, an auth check, a passing checklist and typed confirmation.
- Catalog apps bind to loopback.
- One hostname per app.

**Certificates and DNS.**
- Own domain via DNS-01 wildcard, with rynas managing the LAN A/AAAA record (or delegated `_acme-challenge` with manual records).
- Local CA for `.local`, with a "Trust this NAS" page.
- Tailscale certificates for `ts.net`.
- ARI, a persistent ACME account, one certificate store also used by `:8443`.

**Auth.**
- Built-in SSO through `forward_auth` to rynas-web, with a separate app-only cookie, per-route groups and bypass paths.
- Apps with their own login are left alone.
- Authelia, Pocket ID and Authentik as apps; rynas as OIDC provider in phase 3.

**Remote.**
- Tailscale first: `lan_vpn` + split DNS, per-app Services.
- Then a tunnel (cloudflared/Newt) or port forward for public routes.

### Phases (fitted to `../phase2/docker-apps.md` waves)

| Wave | Content |
|---|---|
| **W0** (phase 1 debt, before W1) | rynas-web rustls on `:8443` with self-signed SANs, `127.0.0.1:8080` for the proxy, certificate import. **Done 2026-11-07** (`../phase1/plan.md` "API v1: web TLS (W0)"): leaf from a per-NAS local CA (the root Caddy will reuse), published in `/etc/rynas/tls/` (not `/var/lib/rynas/certs`: systemd resets the state directory to 0700 root); `:8080` stays on all addresses for the transition and redirects other hosts to `:8443`, `127.0.0.1` only from W1 |
| **W1** (with A2) | `rynas-caddy` package and unit; `web_access` domain, renderer, `/load`, last-good, anti-drift; UI at `https://<host>.local` via local CA; app routes from the catalog `web` block with loopback binding; local names through Avahi's D-Bus API (not `/etc/avahi/hosts`, §3.2); access `lan`; Routes page + wizard Access step; route diagnostics; Trust this NAS |
| **W2** | domains + credentials, DNS-01 wildcard, managed DNS records, rebind detection, certificate dashboard + notifications, `:8443` reuses the wildcard; custom routes (other LAN hosts); routes for foreign stacks |
| **W3** (with the Tailscale plugin) | `lan_vpn` with anti-spoof, split-DNS guide, per-app Tailscale Services; built-in SSO (`/auth/verify`, `rynas_sso`, groups, bypass); public flow with checklist, DDNS, hairpin test, auto-block from access logs, headers presets; external proxy mode |
| **Phase 3** | rynas OIDC provider; cloudflared/Newt integration writing tunnel ingress; CrowdSec/geo options; DNS-PERSIST-01; `rynas-proxy` network mode; L4 streams (caddy-l4); optional rynas-run DDNS/certificate name service (`myunraid.net` model) for the business channel |

### Risks

1. **The WebSocket drop on reload** until Caddy 2.12. Mitigated by reloading only on apply, batching and `stream_close_delay`. Residual: a user editing routes kicks open HA dashboards and live streams once.
2. **Security cadence of our own Caddy build.** We own the CVE response (four `forward_auth`/admin advisories in 2026 alone). Needs CI that rebuilds within days, a watch on GitHub security advisories, and an update channel that ships it outside the normal release train.
3. **DNS provider tokens on the NAS** are high-value secrets. They are stored encrypted, scope guidance is shown, and delegation is offered. A full-zone token leak can hijack the domain.
4. **Rebind protection and odd routers** can make the own-domain LAN mode fail silently. Diagnostics must detect it and give router-specific fixes. Without that, users blame rynas.
5. **Hairpin NAT** turning internet clients into "LAN" clients. Detected by the public test; documented loudly.
6. **Tailscale Services is beta and needs admin-console approval.** If it changes, fall back to per-app ports on the NAS's ts.net name.
7. **Local CA adoption is poor on mobile.** The UI must steer mobile users to a domain or Tailscale instead of leaving them on certificate warnings.
8. **Memory on 4 GB.** Caddy idle with ~30 routes must be measured on the DS920+ (budget: < 60 MB RSS). The quality reviewer checks it like `rynas-web`.
9. **Certificate lifetimes shrinking to 45/47 days** make silent renewal failures surface faster. Renewal failures must alert early (D-NOTIFY), and the DNS check runs before every renewal window.

---

## 12. Proposed decision rows (not applied; for Marco's review)

**D-PROXY (replacement text):**

| D-PROXY | Reverse proxy / web access | native · NPM as dependency · both | native "Web access" on **Caddy**, shipped as `rynas-caddy`: xcaddy build from a pinned upstream release (≥ 2.11.9, never < 2.11.4) with a fixed set of libdns DNS modules, CI rebuild on every upstream release; Debian's caddy (2.6 in trixie) never used. **rynas-caddy owns 80/443 from install** (systemd, host, `CAP_NET_BIND_SERVICE` only, admin API only on a 0600 Unix socket); rynasd renders the full JSON from the `web_access` key and applies it with `POST /load` (engine-side validation and rollback), keeps `last-good.json` for boot without rynasd, anti-drift, timed confirmation for changes to the UI route; reload only on apply, never on container events (WebSocket drop until Caddy 2.12 `stream_detached`). **rynas-web keeps `:8443` with rustls as emergency access, never routed through Caddy**; it reuses the active certificate. Certificate store: local CA (`.local` names, "Trust this NAS" page), ACME DNS-01 wildcard per domain (default; HTTP-01 and CNAME/acme-dns delegation as options), Tailscale certificates, import; ARI, persistent ACME account, expiry/renewal-failure notifications. Docker apps proxied to `127.0.0.1:<port>` (catalog compose binds loopback through `${RYNAS_BIND}`); no label discovery. Built-in SSO: `forward_auth` to rynas-web with a separate app-only `rynas_sso` cookie, per-route groups and bypass paths; apps with their own login left alone; Authelia/Pocket ID/Authentik as apps, rynas OIDC provider phase 3. External proxy mode releases 80/443 for NPM/Traefik/Zoraxy apps. Engine-neutral route model; Angie/nginx renderer with rynas-side ACME is the contingency. See `../research/web-access.md` | adopted 2026-11-05, revised 2026-11-07 |

**D-EXPOSE (replacement text):**

| D-EXPOSE | App exposure model | public by default · LAN only · per-app names + access levels | each route has **names** (local `<app>-<host>.local` via Avahi, `<app>.<domain>` with rynas-managed DNS record, per-app Tailscale Service, tunnel hostname) separate from **access**: `lan` (**default**: the NAS's connected subnets) · `lan_vpn` (+ tailnet ranges accepted only from `tailscale0`, + WireGuard) · `public`. Public requires a domain, an app login or the rynas login, catalog `public` ≠ never, a green live checklist (port/tunnel reachability, DNS, certificate, hairpin-NAT test) and a typed confirmation; auto-block and HSTS (no includeSubDomains/preload) on public routes; unknown hosts aborted. One hostname per app, no path prefixes. Catalog `web` block: `port, path, auth (app/none/proxy), subpath, body, timeout, buffering, bypass, public (ok/warn/never), trusted_proxies_env`. Remote access: Tailscale first (split DNS, Services), cloudflared/Newt tunnels or port forward for public routes. Nothing becomes public as a side effect | adopted 2026-11-05, revised 2026-11-07 |

---

## 13. W2 research: domains, DNS-01, records (2026-11-07)

Checked before wave W2 (own domains, `../phase2/docker-apps.md` §5.4). **(unverified)** marks what comes from a secondary source or memory.

### 13.1 How the others do it

| Product | Credentials | Wildcard / names | LAN records | What rynas takes |
|---|---|---|---|---|
| Synology DSM | Synology account only | wildcard only for `*.<x>.synology.me`; own domains get HTTP-01 (port 80 open) | none | nothing: rynas does DNS-01 for any domain |
| TrueNAS 26 | "ACME DNS authenticators" (Cloudflare, Route 53, OVH, DigitalOcean, shell script), reused by certificates; the docs suggest the Cloudflare **global** key | per certificate | none | the reusable credential object; **not** the global key (scoped tokens only) |
| Unraid (myunraid.net) | none for the user | `https://<lan-ip-dashed>.<hash>.myunraid.net`, a wildcard of their own | the name *encodes* the LAN address | the rebind warning: Unraid tells users to allow `myunraid.net` in rebind protection; rynas detects it instead of only warning |
| Nginx Proxy Manager 2.16 | certbot DNS plugins; a credentials snippet pasted in the UI and stored in a file **(unverified)** | per proxy host or wildcard | none | nothing (clear-text credentials file) |
| Caddy | caddy-dns modules, `{env.*}`/`{file.*}` placeholders | managed (`automate`) or on-demand | none | managed wildcard + `{file.*}` placeholders, ARI by default, `profile` |
| Traefik | lego environment variables per provider | `certResolvers` with `dnsChallenge` (`resolvers`, propagation options), `acme.profile` | none | the public `resolvers` for the propagation check |
| Home Assistant | DuckDNS add-on (token), Let's Encrypt add-on with `dns-cloudflare`, `dns-desec`, `dns-hetzner`, `dns-ovh`, `dns-porkbun` and a generic `dns-lego` | yes | DuckDNS sets the public address only | the provider list European home users actually use |
| acme.sh | `CF_Token`... saved in clear in `account.conf`; DNS alias mode | yes | none | CNAME delegation (alias mode) as the low-privilege option |
| lego 5.6 | one environment variable set per provider, `_FILE` variants **(unverified)**; follows CNAMEs | yes | none | — |

**Nobody writes the LAN record.** Every product above stops at the certificate; the user still has to make `*.nas.example.com` resolve to the NAS (router DNS override, Pi-hole, a manual record) and hits rebind protection on the way. rynas writes `*.<domain>` + `<domain>` A/AAAA = the LAN address with the same token, keeps it current, and its checklist tells rebind protection from a missing record.

### 13.2 ACME in 2026

- **ARI** (RFC 9773): Let's Encrypt serves it in production and staging; Caddy uses it by default since 2.8 (certmagic 0.21). Nothing to configure; ARI renewals are exempt from rate limits.
- **Profiles**: `classic` (90 days, the default; 64 days from 2027-02-10, 45 from 2028-02-16), `tlsserver` (45 days, 7-hour authorization reuse, at most 25 names). `shortlived` (6-day) certificates are being withdrawn: kept only for accounts that used them before 2026-05-13 (per a translated copy of https://letsencrypt.org/docs/profiles/ **(unverified)**). Caddy selects one with the ACME issuer's `profile` (2.10, experimental). rynas: optional `acme.profile`, default none (the CA's default).
- **Lifetimes** (CA/B SC-081v3): 200 days from 2026-03-15, 100 from 2027-03-15, 47 from 2029-03-15. Let's Encrypt runs ahead (45 days by 2028). Silent renewal failures surface within weeks: the dashboard flags a certificate past 2/3 of its lifetime plus a day as `renewal_overdue` and notifies.
- **Rate limits**: 50 certificates per registered domain per week, 5 duplicates per week, 300 new orders per account per 3 hours, 5 failed authorizations per name per hour. One wildcard per domain (two orders: `*.d` and `d`) stays far below them; staging (`https://acme-staging-v02.api.letsencrypt.org/directory`) is the `acme.staging` switch for tests.

### 13.3 CNAME delegation

`_acme-challenge.<domain>` CNAME → a name in a throw-away zone or an acme-dns account (`/register` gives username, password, subdomain, fulldomain; a leaked account can change only that one TXT record; it keeps the last two values, so apex + wildcard work). Caddy follows it with `challenges.dns.override_domain` (or the `acmedns` module). rynas: `challenge = cname_delegation` + `delegate_to`; the credential cannot write the main zone, so the LAN records and the CNAME are listed for the administrator (`manual` in the plan). It is the recommended mode for providers without scoped tokens (DigitalOcean, Hetzner projects, DuckDNS accounts).

### 13.4 Providers for European home users

| Provider | Record API rynas calls | Wildcard records | Least-privilege token | Min TTL |
|---|---|---|---|---|
| **Cloudflare** | v4 `GET/POST/PUT/DELETE /zones/{id}/dns_records`, `Authorization: Bearer` | yes | API token, Zone → DNS → Edit, one zone | 60 (1 = auto) |
| **deSEC** | v1 `GET /domains/?owns_qname=`, `GET /domains/{d}/rrsets/{sub}/{type}/`, bulk `PATCH /domains/{d}/rrsets/` (atomic, `records: []` deletes), `Authorization: Token` | yes (first label) | token policies per domain/subname/type, subnets | domain's `minimum_ttl` (3600) |
| **Hetzner** | Cloud API `https://api.hetzner.cloud/v1/zones/{z}/rrsets[...]`, `actions/set_records`; the old `dns.hetzner.com` API was shut down in May 2026 (https://docs.hetzner.com/networking/dns/faq/beta) — request shapes from a published client **(unverified against Hetzner's reference)** | yes | per project only | 60 |
| **Porkbun** | v3 `POST /dns/{retrieve,edit,delete}ByNameType/...`, `/dns/create/{d}`, keys in the body | yes (`*`) | API access per domain | 600 |
| **OVHcloud** | `/domain/zone/{z}/record`, signed requests (`$1$` + SHA-1), `/refresh` | yes | consumer key rights per method and path | 60 |
| **DuckDNS** | `GET /update?domains=&token=&ip=|ipv6=`; one address per name, every name below answers the same | implicit | one token per account | — |
| Gandi, Route 53, DigitalOcean, RFC 2136, acme-dns | ACME module only (no rynas record client yet: the plan lists the records) | — | Gandi PAT per domain; Route 53 IAM per zone; DigitalOcean none | — |
| IONOS, Namecheap | not in rynas-caddy: Namecheap's `setHosts` replaces the whole zone and needs an IP allow-list | — | — | — |

### 13.5 Choices for W2

- **Clients in Rust** (`crates/rynas-dns`, behind a `DnsApi` trait): Cloudflare, deSEC, Hetzner, Porkbun, OVH, DuckDNS. Tested against fake HTTP servers only.
- **Caddy modules pinned** (`packaging/caddy/dns-modules.txt`): cloudflare v0.2.4, desec v1.1.0, hetzner/v2 v2.0.1, porkbun v0.3.1, ovh v1.1.0, duckdns v0.5.0, gandi v1.2.1, route53 v1.6.2, digitalocean (pseudo-version 2025-06-06), rfc2136 v1.0.0, acmedns v0.7.0. Built with Caddy 2.11.9 in the pinned Go image; `rynas-caddy validate` accepts the W2 golden render with real module provisioning.
- **Secrets never in the engine JSON**: every module above except acmedns (password only) replaces placeholders in its credential fields in `Provision`, and Caddy's replacer reads `{file./path}` since 2.8 (one trailing newline trimmed, unreadable file → empty value + log line; https://github.com/caddyserver/caddy/blob/master/replacer.go).
- **Credentials sealed** with the notification engine's age key, stored in the configuration key as an envelope: never in clear in `config.db`, its revisions or a `.rynasbak`.
- **Propagation checked against public resolvers** (`1.1.1.1`, `9.9.9.9`): the LAN resolver may filter (rebind) or hold a split-horizon zone.

Sources: https://synology.com/en-us/knowledgebase/tutorials/611 · https://www.truenas.com/docs/scale/26/credentials/certificates/acmednsauthscreens/ · https://docs.unraid.net/unraid-os/system-administration/secure-your-server/securing-your-connection/ · https://github.com/NginxProxyManager/nginx-proxy-manager · https://caddyserver.com/docs/caddyfile/directives/tls · https://caddyserver.com/docs/json/apps/tls/automation/policies/issuers/acme/ · https://doc.traefik.io/traefik/reference/install-configuration/tls/certificate-resolvers/acme/ · https://github.com/home-assistant/addons/blob/master/letsencrypt/DOCS.md · https://github.com/acmesh-official/acme.sh/wiki/dnsapi · https://go-acme.github.io/lego/dns/ · https://letsencrypt.org/2024/04/25/guide-to-integrating-ari-into-existing-acme-clients.html · https://letsencrypt.org/docs/profiles/ · https://letsencrypt.org/2025/12/02/from-90-to-45.html · https://letsencrypt.org/docs/rate-limits/ · https://github.com/caddyserver/caddy/releases/tag/v2.10.0 · https://github.com/joohoi/acme-dns · https://developers.cloudflare.com/api/resources/dns/subresources/records/methods/create/ · https://desec.readthedocs.io/en/latest/dns/rrsets.html · https://desec.readthedocs.io/en/latest/auth/tokens.html · https://docs.hetzner.com/networking/dns/faq/beta · https://docs.rs/dns-update/0.5.3/src/dns_update/providers/hetzner.rs.html · https://porkbun.com/llms/dns · https://docs.ovhcloud.com/en/guides/manage-and-operate/api/first-steps · https://www.duckdns.org/spec.jsp · https://github.com/caddy-dns
