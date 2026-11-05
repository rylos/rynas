//! btrbk as the snapshot engine: the configuration rynas generates per job, and the parser
//! of `--format=raw` output (`run`, `prune -n`).
//!
//! The configuration is one volume section with one subvolume, `timestamp_format long`
//! (`<name>.YYYYMMDDTHHMM`, local time: what SMB `shadow_copy2` reads), GFS retention as
//! `snapshot_preserve`/`snapshot_preserve_min`, and the job lock shared with rynas's own
//! delete/restore/rollback ([`crate::scan::lock_path`]).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use rynas_api_types::snapshots::{Retention, SnapshotJob};

use crate::scan::lock_path;

/// Directory of the generated configurations.
pub const CONF_DIR: &str = "/etc/rynas/btrbk";

pub fn conf_path(job: &str) -> PathBuf {
    Path::new(CONF_DIR).join(format!("{job}.conf"))
}

/// `snapshot_preserve` value: `24h 14d 8w 12m`, or `no` when every count is 0.
pub fn preserve(r: &Retention) -> String {
    let parts: Vec<String> = [
        (r.hourly, 'h'),
        (r.daily, 'd'),
        (r.weekly, 'w'),
        (r.monthly, 'm'),
        (r.yearly, 'y'),
    ]
    .iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, u)| format!("{n}{u}"))
    .collect();
    if parts.is_empty() {
        "no".into()
    } else {
        parts.join(" ")
    }
}

/// A valid `snapshot_preserve_min`: `all`, `latest` or `<n>h|d|w|m|y`.
pub fn valid_min(m: &str) -> bool {
    if m == "all" || m == "latest" {
        return true;
    }
    let Some(unit) = m.chars().last() else {
        return false;
    };
    let n = &m[..m.len() - unit.len_utf8()];
    "hdwmy".contains(unit) && !n.is_empty() && n.len() <= 5 && n.bytes().all(|b| b.is_ascii_digit())
}

/// The longest common ancestor of two absolute normalized paths.
fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for (x, y) in a.components().zip(b.components()) {
        if x != y {
            break;
        }
        out.push(x.as_os_str());
    }
    out
}

/// Clean absolute path: no `.`/`..`, no empty components, no characters btrbk's parser or
/// a unit file would trip on.
pub fn clean_abs(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 1024
        && !p
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "\"'\\#%$".contains(c))
        && Path::new(p)
            .components()
            .skip(1)
            .all(|c| matches!(c, Component::Normal(_)))
        && !p[1..].split('/').any(str::is_empty)
        || p == "/"
}

/// `volume`, `snapshot_dir` and `subvolume` of a job: the volume is the longest common
/// ancestor of the subvolume and the snapshot directory, the other two are relative to it.
pub fn layout(job: &SnapshotJob) -> Result<(String, String, String), String> {
    let sv = Path::new(&job.subvolume);
    let sd = Path::new(&job.snapshot_dir);
    let vol = common_ancestor(sv, sd);
    let rel = |p: &Path| -> Result<String, String> {
        let r = p.strip_prefix(&vol).map_err(|e| e.to_string())?;
        Ok(r.to_string_lossy().into_owned())
    };
    let (sd_rel, sv_rel) = (rel(sd)?, rel(sv)?);
    if sv_rel.is_empty() {
        return Err(format!(
            "the snapshot directory {} cannot be inside the subvolume {}",
            job.snapshot_dir, job.subvolume
        ));
    }
    Ok((
        vol.to_string_lossy().into_owned(),
        if sd_rel.is_empty() {
            ".".into()
        } else {
            sd_rel
        },
        sv_rel,
    ))
}

/// The configuration file of a job (without the rynas header).
pub fn render_conf(job: &SnapshotJob) -> Result<String, String> {
    let (vol, sd, sv) = layout(job)?;
    let r = &job.retention;
    Ok(format!(
        "# btrbk configuration of the rynas snapshot job \"{name}\".\n\
         # Generated from the `snapshots` configuration key: edit the job in rynas.\n\
         # Run by the scheduler task snapshot-{name}: btrbk -c {conf} run\n\
         timestamp_format        long\n\
         snapshot_create         always\n\
         snapshot_preserve_min   {min}\n\
         snapshot_preserve       {pre}\n\
         lockfile                {lock}\n\
         \n\
         volume {vol}\n\
         \x20 snapshot_dir {sd}\n\
         \x20 subvolume {sv}\n\
         \x20   snapshot_name {name}\n",
        name = job.name,
        conf = conf_path(&job.name).display(),
        min = r.min,
        pre = preserve(r),
        lock = lock_path(&job.name).display(),
    ))
}

/// One line of `btrbk --format=raw` output: `format="transaction" key='value' ...`.
pub fn parse_raw_line(line: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i] != b'=' && b[i] != b' ' {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            break;
        }
        let key = line[start..i].to_string();
        i += 1;
        let value = if i < b.len() && (b[i] == b'\'' || b[i] == b'"') {
            let q = b[i];
            i += 1;
            let vs = i;
            while i < b.len() && b[i] != q {
                i += 1;
            }
            let v = line[vs..i.min(b.len())].to_string();
            i += 1;
            v
        } else {
            let vs = i;
            while i < b.len() && b[i] != b' ' {
                i += 1;
            }
            line[vs..i].to_string()
        };
        out.insert(key, value);
    }
    out
}

/// What a `btrbk run` / `prune` reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Snapshots created (names).
    pub created: Vec<String>,
    /// Snapshots deleted (or that a dry run would delete).
    pub deleted: Vec<String>,
    /// Failed actions: `type target: message`.
    pub errors: Vec<String>,
    /// The final `finished` line said success.
    pub finished_ok: bool,
}

fn name_of(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_string()
}

/// Parses `--format=raw` transaction output.
pub fn parse_report(out: &str) -> RunReport {
    let mut r = RunReport::default();
    for line in out.lines() {
        let f = parse_raw_line(line);
        if f.get("format").map(String::as_str) != Some("transaction") {
            continue;
        }
        let (t, status) = (
            f.get("type").map_or("", String::as_str),
            f.get("status").map_or("", String::as_str),
        );
        let target = f.get("target_url").map_or("", String::as_str);
        let ok = matches!(status, "success" | "dryrun_success");
        match t {
            "finished" => r.finished_ok = status == "success",
            "startup" => {}
            _ if !ok => r.errors.push(format!(
                "{t} {target}: {}",
                f.get("message").map_or(status, String::as_str)
            )),
            "snapshot" => r.created.push(name_of(target)),
            t if t.starts_with("delete") => r.deleted.push(name_of(target)),
            _ => {}
        }
    }
    r
}

/// Applies a preset (if any) to the counts.
pub fn normalize_retention(r: &Retention) -> Retention {
    match r.preset {
        Some(p) => p.retention(),
        None => r.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rynas_api_types::snapshots::RetentionPreset;

    fn job(sv: &str, sd: &str) -> SnapshotJob {
        SnapshotJob {
            name: "docs".into(),
            share: Some("docs".into()),
            subvolume: sv.into(),
            snapshot_dir: sd.into(),
            retention: RetentionPreset::Standard.retention(),
        }
    }

    #[test]
    fn retention_strings() {
        let mut r = RetentionPreset::Standard.retention();
        assert_eq!(preserve(&r), "24h 14d 8w 12m");
        r.hourly = 0;
        r.yearly = 3;
        assert_eq!(preserve(&r), "14d 8w 12m 3y");
        let none = Retention {
            preset: None,
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: 0,
            yearly: 0,
            min: "latest".into(),
        };
        assert_eq!(preserve(&none), "no");
        for ok in ["all", "latest", "2d", "36h", "1y"] {
            assert!(valid_min(ok), "{ok}");
        }
        for bad in ["no", "", "d", "2x", "-1d", "1.5d", "1234567d"] {
            assert!(!valid_min(bad), "{bad}");
        }
        let mut custom = r.clone();
        custom.preset = Some(RetentionPreset::Light);
        assert_eq!(normalize_retention(&custom).daily, 7);
    }

    #[test]
    fn golden_configuration() {
        let text = render_conf(&job("/srv/vol1/docs", "/srv/vol1/.snapshots/docs")).unwrap();
        crate::tests::golden("docs.conf", &text);
        // another snapshot location on the same volume
        let (v, sd, sv) = layout(&job("/srv/vol1/a/docs", "/srv/vol1/snaps")).unwrap();
        assert_eq!(
            (v.as_str(), sd.as_str(), sv.as_str()),
            ("/srv/vol1", "snaps", "a/docs")
        );
        assert!(layout(&job("/srv/vol1/docs", "/srv/vol1/docs/.snapshots")).is_err());
    }

    #[test]
    fn clean_paths() {
        assert!(clean_abs("/srv/vol1/docs") && clean_abs("/"));
        for bad in [
            "srv",
            "/srv//x",
            "/srv/../etc",
            "/srv/a b",
            "/srv/x/",
            "/srv/%h",
            "/a\"b",
        ] {
            assert!(!clean_abs(bad), "{bad}");
        }
    }

    #[test]
    fn parses_raw_transactions() {
        let out = "\
format=\"transaction\" time='1793842701' localtime='2026-11-05T01:38:21+0000' type='startup' status='v0.32.6' duration='' target_url='' source_url='' parent_url='' message='btrbk command line client, version 0.32.6'
format=\"transaction\" time='1793842701' localtime='2026-11-05T01:38:21+0000' type='snapshot' status='success' duration='0' target_url='/srv/vol1/.snapshots/docs/docs.20261105T0138' source_url='/srv/vol1/docs' parent_url='' message=''
format=\"transaction\" time='1793842701' localtime='2026-11-05T01:38:21+0000' type='delete_snapshot' status='dryrun_success' duration='' target_url='/srv/vol1/.snapshots/docs/docs.20261001T0000' source_url='' parent_url='' message=''
format=\"transaction\" time='1793842701' localtime='2026-11-05T01:38:21+0000' type='delete_snapshot' status='ERROR' duration='' target_url='/srv/vol1/.snapshots/docs/docs.20261002T0000' source_url='' parent_url='' message='permission denied'
format=\"transaction\" time='1793842701' localtime='2026-11-05T01:38:21+0000' type='finished' status='success' duration='0' target_url='' source_url='' parent_url='' message=''
";
        let r = parse_report(out);
        assert_eq!(r.created, ["docs.20261105T0138"]);
        assert_eq!(r.deleted, ["docs.20261001T0000"]);
        assert_eq!(
            r.errors,
            ["delete_snapshot /srv/vol1/.snapshots/docs/docs.20261002T0000: permission denied"]
        );
        assert!(r.finished_ok);
        let f = parse_raw_line("format=\"snapshots\" snapshot_name='a b' status=");
        assert_eq!(f["snapshot_name"], "a b");
        assert_eq!(f["status"], "");
    }
}
