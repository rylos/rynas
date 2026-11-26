//! rynasd: the root daemon of rynas.
//!
//! It serves a small HTTP/1.1 API on a Unix socket (`/run/rynas/rynasd.sock`, 0660, group
//! `rynas`) and only talks to the uids it is told to trust (`SO_PEERCRED`), in production the
//! `rynas` user that runs `rynas-web`. For now it does authentication: PAM password checks,
//! group lookups and the TOTP secrets (`/var/lib/rynas/auth/totp/<user>.json`, root 0600).
//! It is also the only writer of the configuration store (`/var/lib/rynas/config.db`,
//! [`rynas_config`]) and runs the background tasks ([`rynas_tasks`], logs in
//! `/var/lib/rynas/tasks/`). It owns the notification engine ([`rynas_notify::engine`],
//! `/var/lib/rynas/notify/`) and, when a hardware module forwards its notifications
//! (D-MODULE, [`rynas_module`]), listens on
//! `/run/rynas/notify.sock` (root only). Network changes are transactions with a timed
//! confirmation ([`network`], [`rynas_net`]).

pub mod android_app;
pub mod apps;
pub mod backports;
pub mod repositories;
pub mod backup;
pub mod cgroup;
pub mod claims;
pub mod clock;
pub mod dedup;
pub mod devices;
pub mod disk_standby;
pub mod docker;
pub mod docker_adopt;
pub mod docker_auto;
pub mod error;
pub mod files;
pub mod fslock;
pub mod gate;
pub mod groups;
pub mod hardware;
pub mod heavy;
pub mod jobs;
pub mod kernel_updates;
pub mod modules;
pub mod network;
pub mod notifications;
pub mod ops;
pub mod pam;
pub mod photos;
pub mod photos_mount;
pub mod pc_agent;
pub mod pc_backup;
pub mod places;
pub mod power;
pub mod processes;
pub mod protection;
pub mod read_cache;
pub mod removable;
pub mod replication;
pub mod restic_backup;
pub mod roles;
pub mod schedule_view;
pub mod scheduler;
pub mod scheduler_live;
pub mod server;
pub mod services;
pub mod snapshots;
pub mod space;
pub mod storage_ops;
pub mod ssh;
pub mod syncthing;
pub mod system_power;
pub mod sysevents;
pub mod system_tasks;
pub mod tailscale;
pub mod tls;
pub mod ui_prefs;
pub mod updates;
pub mod ups;
pub mod ups_netserver;
pub mod ups_setup;
pub mod ups_update;
pub mod web_access;
pub mod wol;
pub mod zram;
