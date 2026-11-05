//! Subvolumes of a volume for shares and homes: names, the commands that create, change and
//! delete them, and the list view. The commands run as tasks (rynasd) or inside a
//! configuration apply (shares that ask for their subvolume).

use std::time::Duration;

use rynas_api_types::storage::{BtrfsFilesystem, SubvolumeInfo, SubvolumeSettings};

use crate::host::Host;

/// Created by rynas with every volume and never changed through the subvolume API.
pub const SYSTEM_SUBVOLUMES: [&str; 2] = [".snapshots", "@rynas-cache"];

/// [`rynas_api_types::validate::subvolume_name`].
pub use rynas_api_types::validate::subvolume_name as valid_name;

fn argv(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Commands that create `<mountpoint>/<name>` with its settings.
pub fn create_commands(path: &str, s: &SubvolumeSettings) -> Vec<Vec<String>> {
    let mut v = vec![argv(&["btrfs", "subvolume", "create", path])];
    if !s.compression {
        v.push(argv(&[
            "btrfs",
            "property",
            "set",
            path,
            "compression",
            "none",
        ]));
    }
    if s.readonly {
        v.push(argv(&[
            "btrfs", "property", "set", "-ts", path, "ro", "true",
        ]));
    }
    v
}

/// Commands that give an existing subvolume these settings. The read-only flag is cleared
/// first: properties of a read-only subvolume cannot change.
pub fn settings_commands(path: &str, s: &SubvolumeSettings) -> Vec<Vec<String>> {
    let mut v = vec![argv(&[
        "btrfs", "property", "set", "-ts", path, "ro", "false",
    ])];
    // "" resets the property: the subvolume follows the mount option again
    let c = if s.compression { "" } else { "none" };
    v.push(argv(&["btrfs", "property", "set", path, "compression", c]));
    if s.readonly {
        v.push(argv(&[
            "btrfs", "property", "set", "-ts", path, "ro", "true",
        ]));
    }
    v
}

pub fn delete_commands(path: &str) -> Vec<Vec<String>> {
    vec![argv(&["btrfs", "subvolume", "delete", path])]
}

/// The `compression` property from `btrfs property get <path> compression`.
pub fn parse_compression(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("compression="))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The subvolumes directly below the top level of a mounted volume, with their compression
/// property (one quick `btrfs property get` each).
pub fn list(host: &dyn Host, fs: &BtrfsFilesystem) -> Vec<SubvolumeInfo> {
    let Some(mnt) = fs.mountpoint.as_deref() else {
        return Vec::new();
    };
    let mut v: Vec<SubvolumeInfo> = fs
        .subvolumes
        .iter()
        .filter(|s| s.parent_id == 5 && !s.path.contains('/'))
        .map(|s| {
            let path = format!("{}/{}", mnt.trim_end_matches('/'), s.path);
            let compression = host
                .run(
                    "btrfs",
                    &["property", "get", &path, "compression"],
                    Duration::from_secs(10),
                )
                .ok()
                .filter(|o| o.success())
                .and_then(|o| parse_compression(&o.stdout));
            SubvolumeInfo {
                name: s.path.clone(),
                path,
                id: s.id,
                readonly: s.readonly,
                compression,
                system: SYSTEM_SUBVOLUMES.contains(&s.path.as_str()),
            }
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: Vec<Vec<String>>) -> Vec<String> {
        v.into_iter().map(|a| a.join(" ")).collect()
    }

    #[test]
    fn names_and_commands() {
        assert!(valid_name("photos"));
        assert!(valid_name("Video_2026.raw"));
        assert!(!valid_name(".snapshots"));
        assert!(!valid_name("@rynas-cache"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name("../x"));
        assert!(!valid_name(""));
        assert_eq!(
            lines(create_commands(
                "/vol1/video",
                &SubvolumeSettings {
                    compression: false,
                    readonly: true
                }
            )),
            [
                "btrfs subvolume create /vol1/video",
                "btrfs property set /vol1/video compression none",
                "btrfs property set -ts /vol1/video ro true",
            ]
        );
        assert_eq!(
            lines(create_commands("/vol1/docs", &SubvolumeSettings::default())),
            ["btrfs subvolume create /vol1/docs"]
        );
        assert_eq!(
            lines(settings_commands(
                "/vol1/docs",
                &SubvolumeSettings::default()
            )),
            [
                "btrfs property set -ts /vol1/docs ro false",
                "btrfs property set /vol1/docs compression ",
            ]
        );
        assert_eq!(
            parse_compression("compression=zstd:1\n").as_deref(),
            Some("zstd:1")
        );
        assert_eq!(
            parse_compression("compression=none\n").as_deref(),
            Some("none")
        );
        assert_eq!(parse_compression(""), None);
    }
}
