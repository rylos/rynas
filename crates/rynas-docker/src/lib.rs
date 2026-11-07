//! The Docker engine side of rynas (D-DOCKER, phase 2 wave D1; `docs/phase2/docker-apps.md`).
//!
//! Built into rynasd behind this crate boundary so that it can later move into a plugin
//! process (D-PLUG). Generic Debian only: nothing here knows any hardware module.
//!
//! - [`detect`]: is there an engine, which packages, which versions, can rynas install one;
//! - [`install`]: Docker's deb822 repository, the apt pin to the major version, the
//!   install commands (`docker-ce`, `containerd.io`, compose and buildx plugins);
//! - [`engine`]: `daemon.json` merged (never overwritten), containerd's root, the
//!   `RequiresMountsFor=` drop-ins, the layout on the volume ([`Layout`]);
//! - [`stacks`]: stack folders (`/<vol>/docker/stacks/<name>/compose.yaml` + `.env`, one
//!   subvolume each), name and path validation, `docker compose` command lines, the output
//!   of `docker compose config`, bind folders;
//! - [`adopt`]: compose projects managed where they are (adopted, imported), moved only
//!   on request;
//! - [`snapshots`]: read-only snapshots of a stack folder and the rollback;
//! - [`model`]: the API types from the engine's answers, stack grouping, manager detection;
//! - [`events`]: Docker events → notification transitions (unhealthy, OOM, crash loop);
//! - [`stats`]: the aggregated sampler (cgroup v2 files, no per-container API calls);
//! - [`updates`]: image references and the digest comparison of the update check;
//! - [`client`]: the bollard client over `/var/run/docker.sock` (whitelisted calls only).

pub mod adopt;
pub mod client;
pub mod detect;
pub mod engine;
pub mod events;
pub mod install;
pub mod model;
pub mod safefs;
pub mod snapshots;
pub mod stacks;
pub mod stats;
pub mod updates;

pub use engine::Layout;

use rynas_tasks::{Cmd, CmdOutput};

/// The engine's socket.
pub const DOCKER_SOCKET: &str = "/var/run/docker.sock";
/// System user and group that own stack folders and bind folders (PUID/PGID of apps).
pub const APPS_USER: &str = "docker-apps";

/// Runs a command and returns its output whatever the exit code; `Err` only when it could
/// not run (missing program, timeout). Fakes in tests answer by command line.
pub trait Runner: Send + Sync {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String>;
}

/// The real system ([`Cmd::output_blocking`]).
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String> {
        cmd.output_blocking().map_err(|e| e.to_string())
    }
}

/// Real location of the absolute path `abs` below `root` (`/` in production, a dry root or
/// a temporary folder elsewhere).
pub fn under(root: &std::path::Path, abs: &str) -> std::path::PathBuf {
    root.join(abs.trim_start_matches('/'))
}

/// Seconds since the epoch.
pub fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `YYYYMMDDTHHMMSS` (UTC) of `secs` since the epoch.
pub fn stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Seconds since the epoch of a [`stamp`]; `None` when it is not one.
pub fn parse_stamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 15
        || b[8] != b'T'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 8 || c.is_ascii_digit())
    {
        return None;
    }
    let n = |a: usize, z: usize| s[a..z].parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)?, n(4, 6)?, n(6, 8)?);
    let (hh, mm, ss) = (n(9, 11)?, n(11, 13)?, n(13, 15)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss)
}

// Howard Hinnant's civil calendar algorithms.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip() {
        assert_eq!(stamp(0), "19700101T000000");
        let t = 1_794_000_000;
        assert_eq!(parse_stamp(&stamp(t)), Some(t));
        assert_eq!(stamp(t), "20261106T212000");
        assert_eq!(parse_stamp("2026111T2240000"), None);
        assert_eq!(parse_stamp("20261312T224000"), None);
    }
}
