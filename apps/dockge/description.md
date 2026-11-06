Dockge is a lightweight manager for compose stacks. Each stack is an ordinary folder with
a `compose.yaml`; Dockge edits it, runs it and streams its output.

rynas points `DOCKGE_STACKS_DIR` at the shared stacks folder, mounted at the same path, so
every stack in that folder shows up in Dockge too.
