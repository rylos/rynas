//! The health check after an automatic update: every container of the stack must be
//! healthy (when its image has a HEALTHCHECK) or keep running without a restart for
//! [`STABLE_S`] seconds. A container that exits with code 0 and is not restarted counts as a
//! finished one-shot job (migrations, init).

use std::collections::BTreeMap;

/// Seconds a container without a health check must run without restarting.
pub const STABLE_S: u64 = 120;
/// Longest wait for a container whose health check still says `starting`.
pub const HEALTH_MAX_S: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Starting,
    Healthy,
    Unhealthy,
}

/// One reading of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub running: bool,
    /// `None`: the image has no health check.
    pub health: Option<Health>,
    /// The engine's restart count.
    pub restarts: u64,
    pub exit_code: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Judgement {
    /// Read again later.
    Wait,
    Healthy,
    Failed(String),
}

/// The stack's state `elapsed_s` seconds after `up`, from the first reading and the latest
/// one (by container name). A container missing from `now` failed (removed or never
/// created).
pub fn judge(
    first: &BTreeMap<String, Sample>,
    now: &BTreeMap<String, Sample>,
    elapsed_s: u64,
) -> Judgement {
    if first.is_empty() && now.is_empty() {
        return Judgement::Failed("the stack has no container after the update".into());
    }
    let mut waiting = false;
    for (name, f) in first {
        let Some(n) = now.get(name) else {
            return Judgement::Failed(format!("container {name} disappeared"));
        };
        if n.restarts > f.restarts {
            return Judgement::Failed(format!(
                "container {name} restarted {} times",
                n.restarts - f.restarts
            ));
        }
        if !n.running {
            if n.exit_code == 0 && n.health.is_none() {
                continue;
            }
            return Judgement::Failed(format!(
                "container {name} stopped (exit code {})",
                n.exit_code
            ));
        }
        match n.health {
            Some(Health::Unhealthy) => {
                return Judgement::Failed(format!("container {name} is unhealthy"));
            }
            Some(Health::Starting) if elapsed_s >= HEALTH_MAX_S => {
                return Judgement::Failed(format!(
                    "container {name} was not healthy after {} minutes",
                    HEALTH_MAX_S / 60
                ));
            }
            Some(Health::Starting) => waiting = true,
            Some(Health::Healthy) => {}
            None if elapsed_s < STABLE_S => waiting = true,
            None => {}
        }
    }
    if waiting {
        Judgement::Wait
    } else {
        Judgement::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(running: bool, health: Option<Health>, restarts: u64, exit_code: i64) -> Sample {
        Sample {
            running,
            health,
            restarts,
            exit_code,
        }
    }

    fn m(v: &[(&str, Sample)]) -> BTreeMap<String, Sample> {
        v.iter().map(|(n, s)| (n.to_string(), *s)).collect()
    }

    #[test]
    fn healthy_check_needs_no_wait() {
        let f = m(&[("app", s(true, Some(Health::Starting), 0, 0))]);
        assert_eq!(judge(&f, &f, 5), Judgement::Wait);
        let n = m(&[("app", s(true, Some(Health::Healthy), 0, 0))]);
        assert_eq!(judge(&f, &n, 10), Judgement::Healthy);
        let n = m(&[("app", s(true, Some(Health::Unhealthy), 0, 0))]);
        assert!(matches!(judge(&f, &n, 10), Judgement::Failed(_)));
        assert!(matches!(judge(&f, &f, HEALTH_MAX_S), Judgement::Failed(_)));
    }

    #[test]
    fn without_health_check_two_minutes_running() {
        let f = m(&[("db", s(true, None, 3, 0))]);
        assert_eq!(judge(&f, &f, 60), Judgement::Wait);
        assert_eq!(judge(&f, &f, STABLE_S), Judgement::Healthy);
        let n = m(&[("db", s(true, None, 4, 0))]);
        assert!(matches!(judge(&f, &n, 30), Judgement::Failed(m) if m.contains("restarted")));
        let n = m(&[("db", s(false, None, 3, 1))]);
        assert!(matches!(judge(&f, &n, 30), Judgement::Failed(m) if m.contains("exit code 1")));
        assert!(matches!(judge(&f, &m(&[]), 30), Judgement::Failed(_)));
    }

    #[test]
    fn finished_jobs_count_as_done() {
        let f = m(&[("init", s(true, None, 0, 0)), ("web", s(true, Some(Health::Healthy), 0, 0))]);
        let n = m(&[("init", s(false, None, 0, 0)), ("web", s(true, Some(Health::Healthy), 0, 0))]);
        assert_eq!(judge(&f, &n, 10), Judgement::Healthy);
        assert!(matches!(judge(&m(&[]), &m(&[]), 0), Judgement::Failed(_)));
    }
}
