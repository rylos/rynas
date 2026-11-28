Jellyfin 是自由软件媒体服务器，整理共享文件夹中的电影、剧集和音乐，并串流到浏览器、手机、电视和机顶盒。客户端需要时可进行转码。

建议以只读方式映射媒体共享文件夹。若 Intel CPU 支持 Quick Sync，可开启硬件转码，减少单路播放占用的 CPU 资源。

Jellyfin 默认每 12 小时扫描一次媒体库，可能唤醒存放媒体的硬盘。为各媒体库开启实时监视后，可在 Dashboard → Scheduled Tasks 中将 Scan Media Library 改为每天定时运行一次。
