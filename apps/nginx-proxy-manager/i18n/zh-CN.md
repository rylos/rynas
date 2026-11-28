Nginx Proxy Manager 为 nginx 提供网页界面。添加主机名并将它指向网络中的服务，即可获取及续期 Let's Encrypt 证书。它也支持重定向、访问列表和 TCP/UDP 流。

rynas 已内置「网页访问」反向代理。如果希望自行管理代理，可安装 Nginx Proxy Manager。它会占用 80 和 443 端口，因此需要先把「网页访问」切换到外部代理模式。
