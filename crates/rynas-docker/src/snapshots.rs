//! Read-only snapshots of a stack folder (`/<vol>/docker/.snapshots/<stack>/<id>`, with
//! `<id>.json` next to it: reason and the images the services ran) and the rollback.
//!
//! The snapshots are outside the stacks folder on purpose: folder-based managers scan
//! `stacks/` and would list a copy as another stack.
//!
//! Rollback of a stack (stopped first by the caller): a read-only snapshot of the current
//! folder (`before-rollback`), a writable snapshot of the chosen one next to the folder,
//! then two renames swap them and the old folder is deleted (it is kept in the
//! `before-rollback` snapshot). The images go back by tag ([`retag_cmds`]).

use std::path::{Path, PathBuf};

use rynas_api_types::docker::{ServiceImage, StackSnapshot};
use rynas_btrfs::ioctl;
use rynas_tasks::Cmd;
use serde::{Deserialize, Serialize};

use crate::stacks::{check_name, is_subvolume, remove_tree};
use crate::{Layout, parse_stamp, stamp, under};

/// What `<id>.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotMeta {
    pub reason: String,
    pub created_s: i64,
    #[serde(default)]
    pub images: Vec<ServiceImage>,
}

/// Valid snapshot reasons.
pub const REASONS: &[&str] = &["manual", "update", "pull", "before-rollback"];

fn snap_dir(root: &Path, layout: &Layout, stack: &str) -> PathBuf {
    under(root, &layout.stack_snapshots(stack))
}

/// Can the stack folder be snapshotted (a btrfs subvolume)?
pub fn supported(root: &Path, layout: &Layout, stack: &str) -> bool {
    is_subvolume(&under(root, &layout.stack(stack)))
}

/// The snapshots of a stack, newest first.
pub fn list(root: &Path, layout: &Layout, stack: &str) -> Vec<StackSnapshot> {
    let dir = snap_dir(root, layout, stack);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut v: Vec<StackSnapshot> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_string();
            let created = parse_stamp(&id)?;
            let meta: SnapshotMeta = std::fs::read_to_string(dir.join(format!("{id}.json")))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            Some(StackSnapshot {
                id,
                created_s: if meta.created_s > 0 {
                    meta.created_s
                } else {
                    created
                },
                reason: if meta.reason.is_empty() {
                    "manual".into()
                } else {
                    meta.reason
                },
                images: meta.images,
            })
        })
        .collect();
    v.sort_by(|a, b| b.id.cmp(&a.id));
    v
}

/// Takes a read-only snapshot of the stack folder. The id is the UTC time (one second later
/// when that id exists).
pub fn take(
    root: &Path,
    layout: &Layout,
    stack: &str,
    reason: &str,
    images: Vec<ServiceImage>,
    now_s: i64,
) -> Result<StackSnapshot, String> {
    check_name(stack).map_err(|e| e.to_string())?;
    if !REASONS.contains(&reason) {
        return Err(format!("invalid snapshot reason {reason}"));
    }
    let live = under(root, &layout.stack(stack));
    if !is_subvolume(&live) {
        return Err(format!(
            "{} is not a btrfs subvolume: snapshots are not available for this stack",
            layout.stack(stack)
        ));
    }
    let dir = snap_dir(root, layout, stack);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let mut t = now_s;
    while dir.join(stamp(t)).exists() {
        t += 1;
    }
    let id = stamp(t);
    let src = ioctl::open_dir(&live).map_err(|e| format!("{}: {e}", live.display()))?;
    let dst = ioctl::open_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    ioctl::create_snapshot(&src, &dst, &id, true)
        .map_err(|e| format!("snapshot of {stack}: {e}"))?;
    let meta = SnapshotMeta {
        reason: reason.into(),
        created_s: t,
        images,
    };
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| format!("snapshot metadata: {e}"))?;
    Ok(StackSnapshot {
        id,
        created_s: t,
        reason: meta.reason,
        images: meta.images,
    })
}

fn check_id(id: &str) -> Result<(), String> {
    parse_stamp(id)
        .map(|_| ())
        .ok_or_else(|| format!("invalid snapshot id {id:?}"))
}

/// Deletes one snapshot and its metadata.
pub fn delete(
    root: &Path,
    layout: &Layout,
    stack: &str,
    id: &str,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    check_name(stack).map_err(|e| e.to_string())?;
    check_id(id)?;
    let dir = snap_dir(root, layout, stack);
    let p = dir.join(id);
    if !p.is_dir() {
        return Err(format!("no snapshot {id} of {stack}"));
    }
    remove_tree(&p, log)?;
    let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    Ok(())
}

/// Deletes the oldest snapshots beyond `keep`; the ids deleted.
pub fn prune(
    root: &Path,
    layout: &Layout,
    stack: &str,
    keep: usize,
    log: &dyn Fn(&str),
) -> Vec<String> {
    list(root, layout, stack)
        .into_iter()
        .skip(keep)
        .filter_map(|s| delete(root, layout, stack, &s.id, log).ok().map(|_| s.id))
        .collect()
}

/// The metadata of one snapshot.
pub fn get(root: &Path, layout: &Layout, stack: &str, id: &str) -> Option<StackSnapshot> {
    list(root, layout, stack).into_iter().find(|s| s.id == id)
}

/// Puts the stack folder back to snapshot `id` (the stack must be stopped). A
/// `before-rollback` snapshot of the current folder is taken first.
pub fn rollback(
    root: &Path,
    layout: &Layout,
    stack: &str,
    id: &str,
    images_now: Vec<ServiceImage>,
    now_s: i64,
    log: &dyn Fn(&str),
) -> Result<StackSnapshot, String> {
    check_name(stack).map_err(|e| e.to_string())?;
    check_id(id)?;
    let snap = snap_dir(root, layout, stack).join(id);
    if !snap.is_dir() {
        return Err(format!("no snapshot {id} of {stack}"));
    }
    let before = take(root, layout, stack, "before-rollback", images_now, now_s)?;
    log(&format!("current folder kept as snapshot {}", before.id));
    let stacks = under(root, &layout.stacks());
    let fresh = format!(".{stack}.rynas-new");
    let old = format!(".{stack}.rynas-old");
    let parent = ioctl::open_dir(&stacks).map_err(|e| format!("{}: {e}", stacks.display()))?;
    for leftover in [&fresh, &old] {
        let p = stacks.join(leftover);
        if p.exists() {
            remove_tree(&p, log)?;
        }
    }
    let src = ioctl::open_dir(&snap).map_err(|e| format!("{}: {e}", snap.display()))?;
    ioctl::create_snapshot(&src, &parent, &fresh, false)
        .map_err(|e| format!("writable copy of snapshot {id}: {e}"))?;
    if let Err(e) = ioctl::rename_noreplace(&parent, stack, &old) {
        let _ = ioctl::destroy_subvolume(&parent, &fresh);
        return Err(format!("renaming the current folder: {e}"));
    }
    if let Err(e) = ioctl::rename_noreplace(&parent, &fresh, stack) {
        let _ = ioctl::rename_noreplace(&parent, &old, stack);
        return Err(format!("moving the snapshot in place: {e}"));
    }
    log(&format!(
        "stack folder {} restored from snapshot {id}",
        layout.stack(stack)
    ));
    if let Err(e) = remove_tree(&stacks.join(&old), log) {
        log(&format!(
            "the replaced folder {old} could not be deleted: {e}"
        ));
    }
    Ok(before)
}

/// Commands that give each service's image reference back the image it ran when the
/// snapshot was taken: `docker image tag <image_id> <image>`; when the image is gone,
/// `docker pull <repo>@<digest>` first (only with a known digest). `present` tells whether an
/// image id still exists locally.
pub fn retag_cmds(images: &[ServiceImage], present: &dyn Fn(&str) -> bool) -> Vec<Cmd> {
    let mut v = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for i in images {
        if i.image.is_empty() || i.image_id.is_empty() || !seen.insert(i.image.clone()) {
            continue;
        }
        if !present(&i.image_id) {
            match &i.digest {
                Some(d) => v.push(
                    Cmd::new("docker")
                        .args(["pull", "--quiet", d])
                        .env_clear()
                        .timeout(std::time::Duration::from_secs(30 * 60)),
                ),
                None => continue,
            }
            let src = i.digest.clone().unwrap_or_default();
            v.push(tag_cmd(&src, &i.image));
        } else {
            v.push(tag_cmd(&i.image_id, &i.image));
        }
    }
    v
}

fn tag_cmd(src: &str, dst: &str) -> Cmd {
    Cmd::new("docker")
        .args(["image", "tag", src, dst])
        .env_clear()
        .timeout(std::time::Duration::from_secs(60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_and_pruning_plain_folders() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::of_label("vol1");
        let dir = d.path().join("vol1/docker/.snapshots/app");
        for id in [
            "20261101T100000",
            "20261102T100000",
            "20261103T100000",
            "junk",
        ] {
            std::fs::create_dir_all(dir.join(id)).unwrap();
        }
        std::fs::write(
            dir.join("20261103T100000.json"),
            r#"{"reason":"update","created_s":1,"images":[{"service":"a","image":"x:1","image_id":"sha256:1","digest":null}]}"#,
        )
        .unwrap();
        let v = list(d.path(), &l, "app");
        assert_eq!(
            v.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["20261103T100000", "20261102T100000", "20261101T100000"]
        );
        assert_eq!(v[0].reason, "update");
        assert_eq!(v[0].images.len(), 1);
        assert_eq!(v[1].reason, "manual");
        assert_eq!(
            prune(d.path(), &l, "app", 1, &|_| {}),
            ["20261102T100000", "20261101T100000"]
        );
        assert_eq!(list(d.path(), &l, "app").len(), 1);
        assert!(delete(d.path(), &l, "app", "../x", &|_| {}).is_err());
        assert!(delete(d.path(), &l, "app", "20200101T000000", &|_| {}).is_err());
    }

    #[test]
    fn take_needs_a_subvolume() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::of_label("vol1");
        std::fs::create_dir_all(d.path().join("vol1/docker/stacks/app")).unwrap();
        let e = take(d.path(), &l, "app", "manual", vec![], 0).unwrap_err();
        assert!(e.contains("not a btrfs subvolume"), "{e}");
        assert!(take(d.path(), &l, "app", "weird", vec![], 0).is_err());
        assert!(!supported(d.path(), &l, "app"));
    }

    #[test]
    fn retag() {
        let imgs = vec![
            ServiceImage {
                service: "a".into(),
                image: "nginx:1.27".into(),
                image_id: "sha256:aa".into(),
                digest: Some("nginx@sha256:d1".into()),
            },
            ServiceImage {
                service: "b".into(),
                image: "redis:7".into(),
                image_id: "sha256:bb".into(),
                digest: Some("redis@sha256:d2".into()),
            },
            ServiceImage {
                service: "c".into(),
                image: "gone:1".into(),
                image_id: "sha256:cc".into(),
                digest: None,
            },
            ServiceImage {
                service: "d".into(),
                image: "nginx:1.27".into(),
                image_id: "sha256:aa".into(),
                digest: None,
            },
        ];
        let cmds: Vec<String> = retag_cmds(&imgs, &|id| id == "sha256:aa")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            cmds,
            [
                "docker image tag sha256:aa nginx:1.27",
                "docker pull --quiet redis@sha256:d2",
                "docker image tag redis@sha256:d2 redis:7",
            ]
        );
    }
}
