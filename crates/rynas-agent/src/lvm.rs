//! LVM snapshots of the logical volumes a backup reads: thin snapshots of thin volumes, or a
//! classic snapshot sized from the free extents of the volume group (none free: the caller
//! falls back to reading the filesystem as it is, frozen or hot). Always removed after the run
//! and at the next start (`*-rynas-snap`).

use std::process::Command;

pub const SUFFIX: &str = "-rynas-snap";

fn lvm(args: &[&str]) -> Result<String, String> {
    let o = Command::new("lvm").args(args).output().map_err(|e| format!("lvm: {e}"))?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    } else {
        Err(format!("lvm {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
    }
}

/// A snapshot made for one run.
#[derive(Debug)]
pub struct Snap {
    pub vg: String,
    pub name: String,
    pub dev: String,
}

impl Snap {
    pub fn remove(&self) {
        if let Err(e) = lvm(&["lvremove", "-y", &format!("{}/{}", self.vg, self.name)]) {
            eprintln!("{e}");
        }
    }
}

/// A snapshot of `vg/lv`, or why there is none.
pub fn snapshot(vg: &str, lv: &str) -> Result<Snap, String> {
    let info = lvm(&["lvs", "--noheadings", "--units", "b", "--nosuffix", "-o", "lv_size,segtype,vg_free", &format!("{vg}/{lv}")])?;
    let f: Vec<&str> = info.split_whitespace().collect();
    if f.len() < 3 {
        return Err(format!("lvs {vg}/{lv}: unexpected answer"));
    }
    let size: u64 = f[0].parse().unwrap_or(0);
    let thin = f[1] == "thin";
    let free: u64 = f[2].parse().unwrap_or(0);
    let name = format!("{lv}{SUFFIX}");
    if thin {
        lvm(&["lvcreate", "-s", "-n", &name, &format!("{vg}/{lv}")])?;
        lvm(&["lvchange", "-ay", "-K", &format!("{vg}/{name}")])?;
    } else {
        // copy-on-write room: a tenth of the volume, at least 256 MiB, at most what is free
        let want = (size / 10).max(256 << 20).min(free);
        if want < (256 << 20) {
            return Err(format!("volume group {vg} has no free space for a snapshot"));
        }
        lvm(&["lvcreate", "-s", "-n", &name, "-L", &format!("{want}b"), &format!("{vg}/{lv}")])?;
    }
    Ok(Snap {
        vg: vg.to_string(),
        dev: format!("/dev/{vg}/{name}"),
        name,
    })
}

/// Removes snapshots an interrupted run left.
pub fn cleanup_leftovers() {
    let Ok(out) = lvm(&["lvs", "--noheadings", "-o", "vg_name,lv_name"]) else { return };
    for l in out.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() == 2 && f[1].ends_with(SUFFIX) {
            let s = Snap {
                vg: f[0].into(),
                name: f[1].into(),
                dev: String::new(),
            };
            eprintln!("{}/{}: removed (left by an interrupted backup)", s.vg, s.name);
            s.remove();
        }
    }
}
