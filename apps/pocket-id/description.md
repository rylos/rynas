Pocket ID is a small OpenID Connect provider: apps that support "Sign in with OIDC"
(Immich, Jellyfin with a plugin, Nextcloud, Paperless-ngx, Forgejo and many more) send their
users to it, and users sign in with a passkey on their phone or security key. No passwords
to remember or leak.

It needs an HTTPS address of its own, because passkeys only work in a secure context.
