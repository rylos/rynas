Jellyfin is a free software media server: it organizes movies, shows and music from your
shared folders and streams them to browsers, phones, TVs and set-top boxes, transcoding when
a client needs it.

Map the media shared folder read-only. On Intel CPUs with Quick Sync, turn on hardware
transcoding so the CPU is not saturated by a single stream.

Jellyfin scans its libraries every 12 hours by default, which wakes hard disks that hold the
media: with real-time monitoring on in each library, set the Scan Media Library task
(Dashboard, Scheduled Tasks) to once a day at a fixed hour.
