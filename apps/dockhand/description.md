Dockhand is a Docker manager: containers, compose stacks with a visual editor, Git
deployments with webhooks, image update checks, vulnerability scans, terminals and
scheduled restic backups.

rynas sets `STACKS_DIR` to the shared stacks folder, mounted at the same path inside the
container, so stacks created in Dockhand live next to the others and existing stacks can be
adopted in place.
