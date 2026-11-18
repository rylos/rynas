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

/// Why there is no snapshot of a volume.
#[derive(Debug)]
pub enum SnapError {
    /// Its group has too little free space for a classic snapshot ([`cow_room`]).
    NoSpace,
    /// A tool failed (its message).
    Failed(String),
}

impl std::fmt::Display for SnapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapError::NoSpace => write!(f, "too little free space in the volume group for a snapshot"),
            SnapError::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// A snapshot of `vg/lv`, or why there is none.
pub fn snapshot(vg: &str, lv: &str) -> Result<Snap, SnapError> {
    let info = lvm(&["lvs", "--noheadings", "--units", "b", "--nosuffix", "-o", "lv_size,segtype,vg_free", &format!("{vg}/{lv}")]).map_err(SnapError::Failed)?;
    let f: Vec<&str> = info.split_whitespace().collect();
    if f.len() < 3 {
        return Err(SnapError::Failed(format!("lvs {vg}/{lv}: unexpected answer")));
    }
    let size: u64 = f[0].parse().unwrap_or(0);
    let thin = f[1] == "thin";
    let free: u64 = f[2].parse().unwrap_or(0);
    let name = format!("{lv}{SUFFIX}");
    if thin {
        lvm(&["lvcreate", "-s", "-n", &name, &format!("{vg}/{lv}")]).map_err(SnapError::Failed)?;
        lvm(&["lvchange", "-ay", "-K", &format!("{vg}/{name}")]).map_err(SnapError::Failed)?;
    } else {
        let Some(want) = cow_room(size, free) else {
            return Err(SnapError::NoSpace);
        };
        lvm(&["lvcreate", "-s", "-n", &name, "-L", &format!("{want}b"), &format!("{vg}/{lv}")]).map_err(SnapError::Failed)?;
    }
    Ok(Snap {
        vg: vg.to_string(),
        dev: format!("/dev/{vg}/{name}"),
        name,
    })
}

/// Copy-on-write room for a classic snapshot of a volume of `size` bytes with `free` bytes
/// free in its group: a tenth of the volume, at least 256 MiB, at most what is free. `None`
/// when less than a quarter of that is free: a snapshot that small fills up (a package update
/// in `/var` is enough) and is invalidated mid-run, while reading the live volume works.
pub fn cow_room(size: u64, free: u64) -> Option<u64> {
    const MIN: u64 = 256 << 20;
    let want = (size / 10).max(MIN);
    let got = want.min(free);
    (got >= MIN && got >= want / 4).then_some(got)
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

#[cfg(test)]
mod tests {
    use super::cow_room;
    const MIB: u64 = 1 << 20;
    const GB: u64 = 1_000_000_000;

    #[test]
    fn room_for_a_snapshot() {
        // plenty of room: a tenth of the volume
        assert_eq!(cow_room(46 * GB, 100 * GB), Some(46 * GB / 10));
        // small volume: the 256 MiB floor
        assert_eq!(cow_room(GB, 10 * GB), Some(256 * MIB));
        // a quarter of what it wants is enough
        assert_eq!(cow_room(40 * GB, GB + GB / 2), Some(GB + GB / 2));
        // a 46 GB volume with exactly 256 MiB free: too small, read live instead
        assert_eq!(cow_room(46 * GB, 256 * MIB), None);
        // nothing free
        assert_eq!(cow_room(GB, 0), None);
        assert_eq!(cow_room(GB, 255 * MIB), None);
    }
}
