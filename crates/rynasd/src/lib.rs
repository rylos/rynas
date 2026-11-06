//! rynasd: the root daemon of rynas.
//!
//! It serves a small HTTP/1.1 API on a Unix socket (`/run/rynas/rynasd.sock`, 0660, group
//! `rynas`) and only talks to the uids it is told to trust (`SO_PEERCRED`), in production the
//! `rynas` user that runs `rynas-web`. For now it does authentication: PAM password checks,
//! group lookups and the TOTP secrets (`/var/lib/rynas/auth/totp/<user>.json`, root 0600).
//! It is also the only writer of the configuration store (`/var/lib/rynas/config.db`,
//! [`rynas_config`]) and runs the background tasks ([`rynas_tasks`], logs in
//! `/var/lib/rynas/tasks/`). It owns the notification engine ([`rynas_notify::engine`],
//! `/var/lib/rynas/notify/`) and listens for synodeb's `notify` forward on
//! `/run/rynas/notify.sock` (root only). Network changes are transactions with a timed
//! confirmation ([`network`], [`rynas_net`]).

pub mod backup;
pub mod error;
pub mod files;
pub mod groups;
pub mod hardware;
pub mod jobs;
pub mod network;
pub mod notifications;
pub mod ops;
pub mod pam;
pub mod power;
pub mod protection;
pub mod roles;
pub mod scheduler;
pub mod server;
pub mod services;
pub mod snapshots;
pub mod storage_ops;
pub mod ups;
pub mod updates;
