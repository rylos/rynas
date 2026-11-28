Pocket ID 是轻量的 OpenID Connect 身份提供方。支持 OIDC 登录的应用（如 Immich、装有相关插件的 Jellyfin、Nextcloud、Paperless-ngx 和 Forgejo）可将用户引导至 Pocket ID。用户可使用手机的通行密钥或安全密钥登录，无需记住密码，也不必担心密码泄露。

通行密钥只在安全上下文中可用，因此 Pocket ID 需要独立的 HTTPS 地址。
