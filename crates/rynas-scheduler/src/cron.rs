//! Read-only listing of the crontab entries found on the system: `/etc/crontab`,
//! `/etc/cron.d/*` (both with a user field) and `/var/spool/cron/crontabs/<user>`.

use std::path::Path;

use rynas_api_types::scheduler::CronEntry;

use crate::calendar::cron_to_calendar;

/// Entries kept per file (a crontab is a few lines; this only bounds a broken one).
const MAX_PER_FILE: usize = 500;

/// Parses one crontab. `user`: the owner of a user crontab, `None` for system crontabs
/// (whose lines carry a user field).
pub fn parse_crontab(source: &str, text: &str, user: Option<&str>) -> Vec<CronEntry> {
    let mut out = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // environment assignments: NAME=value (the name before any blank)
        let first = line.split_whitespace().next().unwrap_or("");
        if first.contains('=') && !first.starts_with('@') && !first.starts_with('*') {
            continue;
        }
        let mut rest = line;
        let mut take = || {
            let r = rest.trim_start();
            let end = r.find(char::is_whitespace).unwrap_or(r.len());
            let (word, tail) = r.split_at(end);
            rest = tail;
            word.to_string()
        };
        let schedule = if first.starts_with('@') {
            take()
        } else {
            (0..5).map(|_| take()).collect::<Vec<_>>().join(" ")
        };
        let u = match user {
            Some(u) => u.to_string(),
            None => take(),
        };
        let command = rest.trim().to_string();
        if command.is_empty() || u.is_empty() {
            continue;
        }
        out.push(CronEntry {
            source: source.into(),
            line: n as u32 + 1,
            trigger: cron_to_calendar(&schedule).ok().map(|c| c.trigger()),
            schedule,
            user: u,
            command,
        });
        if out.len() >= MAX_PER_FILE {
            break;
        }
    }
    out
}

/// Every crontab entry below `root` ("/" in production).
pub fn list(root: &Path) -> Vec<CronEntry> {
    let at = |p: &str| root.join(p.trim_start_matches('/'));
    let mut out = Vec::new();
    if let Ok(t) = std::fs::read_to_string(at("/etc/crontab")) {
        out.extend(parse_crontab("/etc/crontab", &t, None));
    }
    for (dir, system) in [("/etc/cron.d", true), ("/var/spool/cron/crontabs", false)] {
        let Ok(rd) = std::fs::read_dir(at(dir)) else {
            continue;
        };
        let mut files: Vec<_> = rd.flatten().collect();
        files.sort_by_key(|e| e.file_name());
        for e in files {
            let name = e.file_name().to_string_lossy().into_owned();
            // run-parts skips these in cron.d too
            if name.starts_with('.') || name.contains('~') || name.ends_with(".dpkg-dist") {
                continue;
            }
            if !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let Ok(t) = std::fs::read_to_string(e.path()) else {
                continue;
            };
            let source = format!("{dir}/{name}");
            out.extend(parse_crontab(
                &source,
                &t,
                (!system).then_some(name.as_str()),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::scheduler::TaskTrigger;

    #[test]
    fn parses_system_and_user_crontabs() {
        let sys = "SHELL=/bin/sh\nPATH=/usr/bin:/bin\n# m h dom mon dow user command\n17 *\t* * *\troot\tcd / && run-parts --report /etc/cron.hourly\n@reboot root /usr/local/bin/boot.sh --flag\n0 4 1 * 1 root odd\n";
        let e = parse_crontab("/etc/crontab", sys, None);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].schedule, "17 * * * *");
        assert_eq!(e[0].user, "root");
        assert_eq!(e[0].command, "cd / && run-parts --report /etc/cron.hourly");
        assert_eq!(e[0].line, 4);
        assert!(
            matches!(&e[0].trigger, Some(TaskTrigger::Calendar { on_calendar, .. }) if on_calendar == "*-*-* *:17:00")
        );
        assert_eq!(e[1].trigger, Some(TaskTrigger::Boot { delay_s: 60 }));
        assert_eq!(e[1].command, "/usr/local/bin/boot.sh --flag");
        assert_eq!(e[2].trigger, None, "dom and dow both set: no equivalent");

        let user = "MAILTO=\"\"\n*/5 * * * * /home/alice/bin/sync >/dev/null 2>&1\n";
        let e = parse_crontab("/var/spool/cron/crontabs/alice", user, Some("alice"));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].user, "alice");
        assert_eq!(e[0].command, "/home/alice/bin/sync >/dev/null 2>&1");
    }

    #[test]
    fn lists_below_a_root() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("etc/cron.d")).unwrap();
        std::fs::create_dir_all(r.join("var/spool/cron/crontabs")).unwrap();
        std::fs::write(r.join("etc/crontab"), "0 1 * * * root a\n").unwrap();
        std::fs::write(r.join("etc/cron.d/b"), "0 2 * * * root b\n").unwrap();
        std::fs::write(r.join("etc/cron.d/.placeholder"), "0 2 * * * root x\n").unwrap();
        std::fs::write(r.join("var/spool/cron/crontabs/bob"), "0 3 * * * c\n").unwrap();
        let e = list(r);
        let cmds: Vec<_> = e
            .iter()
            .map(|e| (e.source.as_str(), e.command.as_str()))
            .collect();
        assert_eq!(
            cmds,
            [
                ("/etc/crontab", "a"),
                ("/etc/cron.d/b", "b"),
                ("/var/spool/cron/crontabs/bob", "c")
            ]
        );
    }
}
