Dockhand 是一款 Docker 管理工具，支持容器、带可视化编辑器的 Compose 应用栈、带 Webhook 的 Git 部署、镜像更新检查、漏洞扫描、终端和定时 restic 备份。

rynas 将 `STACKS_DIR` 设为共享应用栈目录，并以相同路径挂载到容器内。在 Dockhand 中创建的新应用栈与其他应用栈保存在同一目录中，现有应用栈也可就地纳入管理。
