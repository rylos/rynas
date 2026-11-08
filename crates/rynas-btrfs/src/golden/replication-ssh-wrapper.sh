#!/bin/sh
# Written by rynas: the ssh of the snapshot replication (btrbk). No ssh_config file
# is read; every option is here.
exec /usr/bin/ssh -F none \
    -o StrictHostKeyChecking=yes -o UserKnownHostsFile=/etc/rynas/ssh/known_hosts \
    -o GlobalKnownHostsFile=none -o CheckHostIP=no -o UpdateHostKeys=no \
    -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none -i /etc/rynas/ssh/replication_ed25519 \
    -o ProxyCommand=none -o ProxyJump=none -o ControlMaster=no -o ControlPath=none \
    -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes -o PermitLocalCommand=no \
    -o ConnectTimeout=20 -o ServerAliveInterval=30 -o ServerAliveCountMax=4 \
    "$@"
