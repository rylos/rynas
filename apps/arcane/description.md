Arcane is a web manager for Docker: containers, images, networks, volumes and compose
projects, with image update checks, OIDC login and Git-backed projects.

rynas installs it on the shared stacks folder, mounted at the same path inside the
container, so Arcane sees and edits the same stacks rynas shows, and stacks created in
Arcane appear in rynas.
