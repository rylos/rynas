Nginx Proxy Manager puts a web interface on nginx: add a host name, point it at a service
on your network, and it fetches and renews a Let's Encrypt certificate. It also handles
redirects, access lists and TCP/UDP streams.

rynas already includes a reverse proxy (Web access). Choose Nginx Proxy Manager only if you
prefer to manage the proxy yourself: it takes over ports 80 and 443, so Web access must be in
external proxy mode.
