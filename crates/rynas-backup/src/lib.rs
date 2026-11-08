//! Backups to another NAS, a local or USB disk and the cloud (D-BACKUP).
//!
//! - [`domain`]: the `backup` configuration key (jobs), checked and normalized; it renders
//!   no file (the scheduler key holds each job's task `backup-<job>`).
//! - [`repo`]: a job's destination and unsealed secrets → the worker's [`RepoSpec`].
//! - [`runner`]: starts `rynas-backup-worker` (in a transient systemd unit with its own
//!   memory limit and the idle priorities, or as a plain child), feeds it the request,
//!   streams its events and cancels it.
//! - [`status`]: what each job's last run left (`<state>/backup/<job>.json`).
//!
//! [`RepoSpec`]: rynas_api_types::backup::RepoSpec

pub mod domain;
pub mod repo;
pub mod runner;
pub mod status;

pub use domain::{BackupDomain, normalize_job, valid_job_name};
pub use repo::{JobSecrets, repo_spec};
pub use runner::{Launch, run_worker};

/// The snapshot tag of a job's snapshots.
pub fn job_tag(job: &str) -> String {
    format!("rynas-job:{job}")
}

/// The scheduler task of a job.
pub fn task_id_of(job: &str) -> String {
    format!("backup-{job}")
}

/// Where the worker is installed.
pub const WORKER: &str = "/usr/libexec/rynas/rynas-backup-worker";
