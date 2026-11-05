//! Pre-checks of a scheduled shutdown and the decision of its policy.

use rynas_api_types::power::{BusyPolicy, ShutdownPolicy};

/// What the shutdown task does after a round of checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    PowerOff,
    /// Check again in [`RECHECK_S`].
    Wait,
    Skip,
}

/// Seconds between two rounds of checks while waiting.
pub const RECHECK_S: u64 = 60;

/// `busy`: the reasons found by the enabled checks; `waited_s`: time spent waiting so far.
pub fn decide(p: &ShutdownPolicy, busy: &[String], waited_s: u64) -> Decision {
    if busy.is_empty() {
        return Decision::PowerOff;
    }
    match p.when_busy {
        BusyPolicy::Force => Decision::PowerOff,
        BusyPolicy::Skip => Decision::Skip,
        BusyPolicy::Wait if waited_s < p.max_wait_min as u64 * 60 => Decision::Wait,
        BusyPolicy::Wait => Decision::Skip,
    }
}

/// RAID arrays resyncing, recovering, reshaping or checking (`/proc/mdstat`), also when the
/// operation is only queued (`resync=DELAYED`, `PENDING`).
pub fn mdstat_busy(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut array = String::new();
    for line in text.lines() {
        if let Some((name, _)) = line.split_once(" : ")
            && !line.starts_with(' ')
            && name.starts_with("md")
        {
            array = name.trim().to_string();
            continue;
        }
        let l = line.trim();
        for op in ["resync", "recovery", "reshape", "check"] {
            let hit = l.contains(&format!("{op} =")) || l.contains(&format!("{op}="));
            if hit && !array.is_empty() {
                let pct = l
                    .split_whitespace()
                    .find(|w| w.ends_with('%'))
                    .map(|p| format!(" {p}"))
                    .unwrap_or_default();
                out.push(format!("{array}: RAID {op}{pct}"));
                break;
            }
        }
    }
    out
}

/// Files SMB clients hold open, from `smbstatus -L --json` (`open_files` object).
pub fn smb_open_files(json: &str) -> Result<usize, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("smbstatus --json: {e}"))?;
    Ok(v.get("open_files")
        .and_then(|o| o.as_object())
        .map_or(0, |o| o.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(when_busy: BusyPolicy, max_wait_min: u32) -> ShutdownPolicy {
        ShutdownPolicy {
            when_busy,
            max_wait_min,
            ..Default::default()
        }
    }

    #[test]
    fn decisions() {
        let busy = vec!["md1: RAID resync 8.5%".to_string()];
        let none: Vec<String> = vec![];
        for p in [BusyPolicy::Wait, BusyPolicy::Skip, BusyPolicy::Force] {
            assert_eq!(
                decide(&policy(p, 5), &none, 0),
                Decision::PowerOff,
                "{p:?} idle"
            );
        }
        assert_eq!(
            decide(&policy(BusyPolicy::Force, 5), &busy, 0),
            Decision::PowerOff
        );
        assert_eq!(
            decide(&policy(BusyPolicy::Skip, 5), &busy, 0),
            Decision::Skip
        );
        assert_eq!(
            decide(&policy(BusyPolicy::Wait, 5), &busy, 0),
            Decision::Wait
        );
        assert_eq!(
            decide(&policy(BusyPolicy::Wait, 5), &busy, 299),
            Decision::Wait
        );
        assert_eq!(
            decide(&policy(BusyPolicy::Wait, 5), &busy, 300),
            Decision::Skip
        );
        // busy, then idle while waiting: power off
        assert_eq!(
            decide(&policy(BusyPolicy::Wait, 5), &none, 120),
            Decision::PowerOff
        );
    }

    #[test]
    fn mdstat() {
        let text = "Personalities : [raid1] [raid6]\n\
md1 : active raid1 sdb3[1] sda3[0]\n      976630464 blocks super 1.2 [2/2] [UU]\n      [=>...................]  resync =  8.5% (83124224/976630464) finish=81.0min speed=183609K/sec\n      bitmap: 1/8 pages [4KB], 65536KB chunk\n\n\
md0 : active raid1 sdb1[1] sda1[0]\n      2097088 blocks [2/2] [UU]\n\n\
md2 : active raid5 sdc1[2] sdd1[1] sde1[0]\n      1953259520 blocks level 5 [3/3] [UUU]\n        resync=DELAYED\n\n\
md3 : active raid1 sdf1[1] sdg1[0]\n      100 blocks [2/2] [UU]\n      [==>..]  check = 12.0% (1/8) finish=1.0min speed=1K/sec\n\nunused devices: <none>\n";
        assert_eq!(
            mdstat_busy(text),
            vec![
                "md1: RAID resync 8.5%".to_string(),
                "md2: RAID resync".into(),
                "md3: RAID check 12.0%".into()
            ]
        );
        assert!(mdstat_busy("Personalities :\nunused devices: <none>\n").is_empty());
    }

    #[test]
    fn smbstatus() {
        let j = r#"{"timestamp":"2026-11-05T10:00:00","version":"4.22.3","smb_conf":"/etc/samba/smb.conf",
            "open_files":{"/srv/vol1/docs/a.txt":{"service_path":"/srv/vol1/docs","filename":"a.txt","opens":{}},
                          "/srv/vol1/docs/b.txt":{}}}"#;
        assert_eq!(smb_open_files(j).unwrap(), 2);
        assert_eq!(smb_open_files(r#"{"open_files":{}}"#).unwrap(), 0);
        assert_eq!(smb_open_files(r#"{"sessions":{}}"#).unwrap(), 0);
        assert!(smb_open_files("No locked files").is_err());
    }
}
