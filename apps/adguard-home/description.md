AdGuard Home is a DNS server for your whole network: devices that use it get ads, trackers
and known malicious domains blocked, without installing anything on them. It shows which
devices ask for what, supports per-client rules and parental controls, and forwards queries
to encrypted upstream resolvers (DNS-over-HTTPS or DNS-over-TLS).

After install, set the NAS address as the DNS server in your router.

Port 53 is published on every address of the NAS. If the NAS has a public address (no
router in front, or port 53 forwarded), anyone on the internet could use it as a resolver:
block port 53 from the internet in the firewall, or limit AdGuard Home's "Allowed clients"
to your LAN.
