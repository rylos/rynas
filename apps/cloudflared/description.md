cloudflared connects the NAS to a Cloudflare Tunnel with an outbound connection, so selected
apps get a public address on your domain without port forwarding on the router and without
a public IP address (it works behind CGNAT).

Hostnames and access rules are managed in the Cloudflare dashboard; this app only runs the
connector with the tunnel token.
