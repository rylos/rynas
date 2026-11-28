Dockge 是轻量的 Compose 应用栈管理工具。每个应用栈都是包含 `compose.yaml` 的普通文件夹，Dockge 可以编辑、运行应用栈并实时显示输出。

rynas 将 `DOCKGE_STACKS_DIR` 设为共享应用栈目录，并以相同路径挂载，因此该目录中的应用栈也会显示在 Dockge 中。
