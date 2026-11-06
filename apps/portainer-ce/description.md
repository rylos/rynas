Portainer Community Edition is a widely used container management interface for Docker,
Swarm and Kubernetes, with stacks, templates, registries and access control.

The web interface uses HTTPS with a self-signed certificate on its own port. The shared
stacks folder is mounted at the same path, so compose files there can be referenced, but
Portainer keeps the stacks it creates in its own data folder.
