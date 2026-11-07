//! The API types from the engine's answers, stacks grouped by the compose labels, who
//! manages each stack, and the manager containers recognised by image.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bollard::models as bm;
use rynas_api_types::docker::{
    ContainerHealth, ContainerState, ContainerSummary, DetectedManager, DockerDiskUsage,
    ImageSummary, ManagerKind, NetworkSummary, PortMapping, ServiceImage, StackManager, StackState,
    StackSummary, VolumeSummary,
};

use crate::Layout;
use crate::updates::{local_digest, normalize_repo};

pub const LABEL_PROJECT: &str = "com.docker.compose.project";
pub const LABEL_SERVICE: &str = "com.docker.compose.service";
pub const LABEL_WORKING_DIR: &str = "com.docker.compose.project.working_dir";
pub const LABEL_CONFIG_FILES: &str = "com.docker.compose.project.config_files";
pub const LABEL_ENV_FILE: &str = "com.docker.compose.project.environment_file";
/// Labels with the "open" link of a container.
pub const URL_LABELS: &[&str] = &["rynas.url", "homepage.href"];

/// A container with the compose labels the grouping needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Seen {
    pub c: ContainerSummary,
    pub working_dir: Option<String>,
    pub config_files: Vec<String>,
    /// `com.docker.compose.project.environment_file` (`--env-file`, comma-separated).
    pub env_files: Vec<String>,
}

fn state_of(s: Option<bm::ContainerSummaryStateEnum>) -> ContainerState {
    use bm::ContainerSummaryStateEnum as S;
    match s {
        Some(S::CREATED) => ContainerState::Created,
        Some(S::RUNNING) => ContainerState::Running,
        Some(S::PAUSED) => ContainerState::Paused,
        Some(S::RESTARTING) => ContainerState::Restarting,
        Some(S::REMOVING) => ContainerState::Removing,
        Some(S::EXITED) => ContainerState::Exited,
        Some(S::DEAD) => ContainerState::Dead,
        Some(S::STOPPING) => ContainerState::Stopping,
        _ => ContainerState::Unknown,
    }
}

fn health_of(h: Option<&bm::ContainerSummaryHealth>, status: &str) -> Option<ContainerHealth> {
    use bm::ContainerSummaryHealthStatusEnum as H;
    match h.and_then(|h| h.status) {
        Some(H::STARTING) => Some(ContainerHealth::Starting),
        Some(H::HEALTHY) => Some(ContainerHealth::Healthy),
        Some(H::UNHEALTHY) => Some(ContainerHealth::Unhealthy),
        // older engines: only in the status text
        _ if status.contains("(unhealthy)") => Some(ContainerHealth::Unhealthy),
        _ if status.contains("(healthy)") => Some(ContainerHealth::Healthy),
        _ if status.contains("(health: starting)") => Some(ContainerHealth::Starting),
        _ => None,
    }
}

/// The manager an image is, if any.
pub fn manager_of_image(image: &str) -> Option<ManagerKind> {
    let (repo, _) = crate::updates::split_ref(image);
    let short = repo.strip_prefix("docker.io/").unwrap_or(&repo);
    let last = short.rsplit('/').next().unwrap_or("");
    if matches!(
        short,
        "portainer/portainer-ce" | "portainer/portainer-ee" | "portainer/portainer"
    ) {
        Some(ManagerKind::Portainer)
    } else if short == "fnsys/dockhand" {
        Some(ManagerKind::Dockhand)
    } else if last == "dockge" {
        // louislam/dockge and its forks
        Some(ManagerKind::Dockge)
    } else if short.starts_with("ghcr.io/getarcaneapp/") {
        Some(ManagerKind::Arcane)
    } else if short.starts_with("ghcr.io/moghtech/komodo-") {
        Some(ManagerKind::Komodo)
    } else {
        None
    }
}

/// One container of the engine's list.
pub fn container_of(c: &bm::ContainerSummary) -> Seen {
    let labels = c.labels.clone().unwrap_or_default();
    let status = c.status.clone().unwrap_or_default();
    let image = c.image.clone().unwrap_or_default();
    let mut ports: Vec<PortMapping> = c
        .ports
        .iter()
        .flatten()
        .map(|p| PortMapping {
            ip: p.ip.clone().filter(|s| !s.is_empty()),
            private_port: p.private_port,
            public_port: p.public_port,
            protocol: p
                .typ
                .map(|t| t.to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "tcp".into()),
        })
        .collect();
    // the engine lists a published port once per address family
    ports.sort_by(|a, b| {
        (a.public_port, a.private_port, &a.protocol, &a.ip).cmp(&(
            b.public_port,
            b.private_port,
            &b.protocol,
            &b.ip,
        ))
    });
    ports.dedup_by(|a, b| {
        a.public_port == b.public_port
            && a.private_port == b.private_port
            && a.protocol == b.protocol
    });
    let state = state_of(c.state);
    // a stopped container keeps its last health: meaningless once it no longer runs
    let health = matches!(state, ContainerState::Running | ContainerState::Restarting)
        .then(|| health_of(c.health.as_ref(), &status))
        .flatten();
    let summary = ContainerSummary {
        id: c.id.clone().unwrap_or_default(),
        name: c
            .names
            .as_ref()
            .and_then(|n| n.first())
            .map(|n| n.trim_start_matches('/').to_string())
            .unwrap_or_default(),
        manager: manager_of_image(&image),
        image,
        image_id: c.image_id.clone().unwrap_or_default(),
        state,
        health,
        status,
        created_s: c.created.unwrap_or(0),
        ports,
        stack: labels.get(LABEL_PROJECT).cloned(),
        service: labels.get(LABEL_SERVICE).cloned(),
        url: URL_LABELS.iter().find_map(|l| labels.get(*l).cloned()),
    };
    Seen {
        c: summary,
        working_dir: labels.get(LABEL_WORKING_DIR).cloned(),
        config_files: list_label(labels.get(LABEL_CONFIG_FILES)),
        env_files: list_label(labels.get(LABEL_ENV_FILE)),
    }
}

fn list_label(v: Option<&String>) -> Vec<String> {
    v.map(|s| {
        s.split(',')
            .map(str::to_string)
            .filter(|x| !x.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// First published TCP port of a container.
pub fn first_tcp_port(c: &ContainerSummary) -> Option<u16> {
    c.ports
        .iter()
        .filter(|p| p.protocol == "tcp")
        .filter_map(|p| p.public_port)
        .min()
}

/// The manager containers on this engine.
pub fn detected_managers(containers: &[Seen]) -> Vec<DetectedManager> {
    let mut v: Vec<DetectedManager> = containers
        .iter()
        .filter_map(|s| {
            Some(DetectedManager {
                kind: s.c.manager?,
                container: s.c.name.clone(),
                container_id: s.c.id.clone(),
                image: s.c.image.clone(),
                running: s.c.state == ContainerState::Running,
                port: first_tcp_port(&s.c),
                stack: s.c.stack.clone(),
            })
        })
        .collect();
    v.sort_by(|a, b| (a.kind, &a.container).cmp(&(b.kind, &b.container)));
    v
}

/// Who manages a compose project: rynas when its working directory is the rynas folder of
/// the same name, Portainer when it is in Portainer's compose data, else untracked.
pub fn classify(
    project: &str,
    working_dir: Option<&str>,
    config_files: &[String],
    layout: Option<&Layout>,
) -> StackManager {
    if let (Some(l), Some(w)) = (layout, working_dir)
        && w.trim_end_matches('/') == l.stack(project)
    {
        return StackManager::Rynas;
    }
    let portainer = |p: &str| {
        p.starts_with("/data/compose/") || (p.contains("portainer") && p.contains("/compose/"))
    };
    if working_dir.is_some_and(portainer) || config_files.iter().any(|f| portainer(f)) {
        return StackManager::Portainer;
    }
    StackManager::Untracked
}

/// Stacks from the containers' compose labels plus the rynas folders on disk without
/// containers. `folders`: stack folder names in the stacks folder; `subvolumes`: those that
/// are btrfs subvolumes.
pub fn group_stacks(
    containers: &[Seen],
    layout: Option<&Layout>,
    folders: &BTreeSet<String>,
    subvolumes: &BTreeSet<String>,
) -> Vec<StackSummary> {
    let mut by: BTreeMap<String, Vec<&Seen>> = BTreeMap::new();
    for s in containers {
        if let Some(p) = &s.c.stack {
            by.entry(p.clone()).or_default().push(s);
        }
    }
    let mut out = Vec::new();
    for (name, cs) in &by {
        let working_dir = cs.iter().find_map(|s| s.working_dir.clone());
        let config_files = cs
            .iter()
            .find(|s| !s.config_files.is_empty())
            .map(|s| s.config_files.clone())
            .unwrap_or_default();
        let manager = classify(name, working_dir.as_deref(), &config_files, layout);
        let running = cs
            .iter()
            .filter(|s| s.c.state == ContainerState::Running)
            .count() as u32;
        let total = cs.len() as u32;
        let images: BTreeSet<String> = cs.iter().map(|s| s.c.image.clone()).collect();
        let folder = manager == StackManager::Rynas && folders.contains(name);
        out.push(StackSummary {
            name: name.clone(),
            manager,
            state: state_for(running, total),
            working_dir,
            config_files,
            containers: total,
            running,
            unhealthy: cs
                .iter()
                .filter(|s| s.c.health == Some(ContainerHealth::Unhealthy))
                .count() as u32,
            editable: folder,
            snapshots: folder && subvolumes.contains(name),
            images: images.into_iter().collect(),
            adopted: None,
        });
    }
    if let Some(l) = layout {
        for f in folders {
            if by.contains_key(f) {
                continue;
            }
            out.push(StackSummary {
                name: f.clone(),
                manager: StackManager::Rynas,
                state: StackState::Down,
                working_dir: Some(l.stack(f)),
                editable: true,
                snapshots: subvolumes.contains(f),
                ..Default::default()
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The state of a stack from its counts.
pub fn state_for(running: u32, total: u32) -> StackState {
    match (running, total) {
        (_, 0) => StackState::Down,
        (0, _) => StackState::Stopped,
        (r, t) if r == t => StackState::Running,
        _ => StackState::Partial,
    }
}

pub fn image_of(i: &bm::ImageSummary) -> ImageSummary {
    let tags: Vec<String> = i
        .repo_tags
        .iter()
        .filter(|t| t.as_str() != "<none>:<none>")
        .cloned()
        .collect();
    ImageSummary {
        id: i.id.clone(),
        dangling: tags.is_empty(),
        tags,
        digests: i
            .repo_digests
            .iter()
            .filter(|d| !d.starts_with("<none>"))
            .cloned()
            .collect(),
        size_bytes: i.size.max(0) as u64,
        created_s: i.created,
        containers: i.containers,
    }
}

/// Volumes with their size and use from `system df` when given (`df_volumes`: name →
/// (size, ref count)).
pub fn volume_of(v: &bm::Volume, df: &HashMap<String, (i64, i64)>) -> VolumeSummary {
    let (size, refs) = df.get(&v.name).copied().unwrap_or((-1, -1));
    VolumeSummary {
        name: v.name.clone(),
        driver: v.driver.clone(),
        mountpoint: v.mountpoint.clone(),
        stack: v.labels.get(LABEL_PROJECT).cloned(),
        size_bytes: (size >= 0).then_some(size as u64),
        in_use: (refs >= 0).then_some(refs as u32),
    }
}

/// name → (size, ref count) of the volumes in a `system df` answer.
pub fn df_volumes(df: &bm::SystemDataUsageResponse) -> HashMap<String, (i64, i64)> {
    let mut m = HashMap::new();
    let items = df.volume_usage.as_ref().and_then(|u| u.items.as_ref());
    for it in items.into_iter().flatten() {
        let (Some(name), Some(u)) = (it.get("Name").and_then(|v| v.as_str()), it.get("UsageData"))
        else {
            continue;
        };
        let size = u.get("Size").and_then(|v| v.as_i64()).unwrap_or(-1);
        let refs = u.get("RefCount").and_then(|v| v.as_i64()).unwrap_or(-1);
        m.insert(name.to_string(), (size, refs));
    }
    m
}

pub fn network_of(n: &bm::Network) -> NetworkSummary {
    NetworkSummary {
        id: n.id.clone().unwrap_or_default(),
        name: n.name.clone().unwrap_or_default(),
        driver: n.driver.clone().unwrap_or_default(),
        scope: n.scope.clone().unwrap_or_default(),
        subnets: n
            .ipam
            .as_ref()
            .and_then(|i| i.config.as_ref())
            .into_iter()
            .flatten()
            .filter_map(|c| c.subnet.clone())
            .collect(),
        internal: n.internal.unwrap_or(false),
        stack: n
            .labels
            .as_ref()
            .and_then(|l| l.get(LABEL_PROJECT).cloned()),
    }
}

pub fn disk_usage_of(df: &bm::SystemDataUsageResponse) -> DockerDiskUsage {
    let n = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;
    DockerDiskUsage {
        images_bytes: n(df.image_usage.as_ref().and_then(|u| u.total_size)),
        images_reclaimable_bytes: n(df.image_usage.as_ref().and_then(|u| u.reclaimable)),
        containers_bytes: n(df.container_usage.as_ref().and_then(|u| u.total_size)),
        volumes_bytes: n(df.volume_usage.as_ref().and_then(|u| u.total_size)),
        build_cache_bytes: n(df.build_cache_usage.as_ref().and_then(|u| u.total_size)),
    }
}

/// The image each service of a stack runs, with its registry digest (for a later rollback).
pub fn service_images(
    containers: &[&ContainerSummary],
    images: &[ImageSummary],
) -> Vec<ServiceImage> {
    let mut v: Vec<ServiceImage> = containers
        .iter()
        .filter_map(|c| {
            let service = c.service.clone()?;
            let digest = images
                .iter()
                .find(|i| i.id == c.image_id)
                .and_then(|i| local_digest(&c.image, &i.digests))
                .map(|d| {
                    let (repo, _) = crate::updates::split_ref(&c.image);
                    format!("{}@{d}", familiar(&repo))
                });
            Some(ServiceImage {
                service,
                image: c.image.clone(),
                image_id: c.image_id.clone(),
                digest,
            })
        })
        .collect();
    v.sort_by(|a, b| a.service.cmp(&b.service));
    v.dedup_by(|a, b| a.service == b.service);
    v
}

/// `docker.io/library/nginx` → `nginx`, `docker.io/user/x` → `user/x`.
pub fn familiar(repo: &str) -> String {
    let r = normalize_repo(repo);
    r.strip_prefix("docker.io/library/")
        .or_else(|| r.strip_prefix("docker.io/"))
        .unwrap_or(&r)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(json: &str) -> Vec<bm::ContainerSummary> {
        serde_json::from_str(json).unwrap()
    }

    // what GET /containers/json answers (trimmed)
    const LIST: &str = r#"[
      {"Id":"aaa1","Names":["/immich_server"],"Image":"ghcr.io/immich-app/immich-server:v2.1.0","ImageID":"sha256:i1",
       "Created":1790000000,"State":"running","Status":"Up 2 hours (healthy)",
       "Ports":[{"IP":"0.0.0.0","PrivatePort":2283,"PublicPort":2283,"Type":"tcp"},{"IP":"::","PrivatePort":2283,"PublicPort":2283,"Type":"tcp"}],
       "Labels":{"com.docker.compose.project":"immich","com.docker.compose.service":"server",
         "com.docker.compose.project.working_dir":"/vol1/docker/stacks/immich",
         "com.docker.compose.project.config_files":"/vol1/docker/stacks/immich/compose.yaml","homepage.href":"http://nas:2283"},
       "Health":{"Status":"healthy","FailingStreak":0}},
      {"Id":"aaa2","Names":["/immich_db"],"Image":"postgres:16","ImageID":"sha256:i2","State":"exited","Status":"Exited (1) 3 minutes ago","Health":{"Status":"unhealthy"},
       "Labels":{"com.docker.compose.project":"immich","com.docker.compose.service":"db",
         "com.docker.compose.project.working_dir":"/vol1/docker/stacks/immich"}},
      {"Id":"bbb1","Names":["/portainer"],"Image":"portainer/portainer-ce:2.33.5","ImageID":"sha256:p","State":"running","Status":"Up 1 day",
       "Ports":[{"IP":"0.0.0.0","PrivatePort":9443,"PublicPort":9443,"Type":"tcp"},{"PrivatePort":8000,"Type":"tcp"}],"Labels":{}},
      {"Id":"ccc1","Names":["/wiki-app-1"],"Image":"wiki:1","State":"running","Status":"Up 5 minutes (unhealthy)",
       "Labels":{"com.docker.compose.project":"wiki","com.docker.compose.project.working_dir":"/data/compose/7",
         "com.docker.compose.project.config_files":"/data/compose/7/docker-compose.yml"}},
      {"Id":"ddd1","Names":["/dev-web-1"],"Image":"nginx","State":"running","Status":"Up",
       "Labels":{"com.docker.compose.project":"dev","com.docker.compose.project.working_dir":"/home/alice/dev"}},
      {"Id":"eee1","Names":["/loose"],"Image":"alpine","State":"created","Status":"Created","Labels":{}}
    ]"#;

    fn seen() -> Vec<Seen> {
        raw(LIST).iter().map(container_of).collect()
    }

    #[test]
    fn containers() {
        let s = seen();
        let c = &s[0].c;
        assert_eq!(
            (c.name.as_str(), c.state, c.health),
            (
                "immich_server",
                ContainerState::Running,
                Some(ContainerHealth::Healthy)
            )
        );
        assert_eq!(
            c.ports.len(),
            1,
            "IPv4 and IPv6 of one port are one mapping"
        );
        assert_eq!(c.url.as_deref(), Some("http://nas:2283"));
        assert_eq!(
            (c.stack.as_deref(), c.service.as_deref()),
            (Some("immich"), Some("server"))
        );
        assert_eq!(
            s[3].c.health,
            Some(ContainerHealth::Unhealthy),
            "from the status text"
        );
        assert_eq!(s[1].c.health, None, "a stopped container has no health");
        assert_eq!(s[2].c.manager, Some(ManagerKind::Portainer));
        assert_eq!(first_tcp_port(&s[2].c), Some(9443));
    }

    #[test]
    fn managers_by_image() {
        for (img, k) in [
            ("portainer/portainer-ee:lts", Some(ManagerKind::Portainer)),
            (
                "docker.io/portainer/portainer-ce",
                Some(ManagerKind::Portainer),
            ),
            ("fnsys/dockhand:v1.0.37", Some(ManagerKind::Dockhand)),
            ("louislam/dockge:1", Some(ManagerKind::Dockge)),
            ("ghcr.io/someone/dockge:nightly", Some(ManagerKind::Dockge)),
            (
                "ghcr.io/getarcaneapp/arcane:latest",
                Some(ManagerKind::Arcane),
            ),
            ("ghcr.io/moghtech/komodo-core:1", Some(ManagerKind::Komodo)),
            ("nginx", None),
            ("ghcr.io/getarcane/arcane", None),
        ] {
            assert_eq!(manager_of_image(img), k, "{img}");
        }
        let m = detected_managers(&seen());
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].port, m[0].running), (Some(9443), true));
    }

    #[test]
    fn grouping_and_classification() {
        let l = Layout::of_label("vol1");
        let folders: BTreeSet<String> = ["immich", "fresh"].map(String::from).into();
        let subs: BTreeSet<String> = ["immich".to_string()].into();
        let st = group_stacks(&seen(), Some(&l), &folders, &subs);
        let names: Vec<&str> = st.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["dev", "fresh", "immich", "wiki"]);
        let get = |n: &str| st.iter().find(|s| s.name == n).unwrap();
        let im = get("immich");
        assert_eq!(
            (im.manager, im.state, im.containers, im.running),
            (StackManager::Rynas, StackState::Partial, 2, 1)
        );
        assert!(im.editable && im.snapshots);
        assert_eq!(im.config_files, ["/vol1/docker/stacks/immich/compose.yaml"]);
        let w = get("wiki");
        assert_eq!(
            (w.manager, w.unhealthy, w.editable),
            (StackManager::Portainer, 1, false)
        );
        assert_eq!(get("dev").manager, StackManager::Untracked);
        let f = get("fresh");
        assert_eq!(
            (f.state, f.manager, f.editable, f.snapshots),
            (StackState::Down, StackManager::Rynas, true, false)
        );
        // without a volume chosen nothing is rynas's
        let st = group_stacks(&seen(), None, &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(
            st.iter().find(|s| s.name == "immich").unwrap().manager,
            StackManager::Untracked
        );
        // a project whose folder name differs is not acted on as a rynas stack
        assert_eq!(
            classify("other", Some("/vol1/docker/stacks/immich"), &[], Some(&l)),
            StackManager::Untracked
        );
    }

    #[test]
    fn service_images_with_digests() {
        let s = seen();
        let cs: Vec<&ContainerSummary> = s
            .iter()
            .filter(|x| x.c.stack.as_deref() == Some("immich"))
            .map(|x| &x.c)
            .collect();
        let images = vec![ImageSummary {
            id: "sha256:i2".into(),
            digests: vec!["postgres@sha256:dd".into()],
            ..Default::default()
        }];
        let v = service_images(&cs, &images);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].service, "db");
        assert_eq!(v[0].digest.as_deref(), Some("postgres@sha256:dd"));
        assert_eq!(v[1].digest, None);
    }

    #[test]
    fn images_volumes_networks_df() {
        let i: bm::ImageSummary = serde_json::from_str(
            r#"{"Id":"sha256:x","ParentId":"","RepoTags":["<none>:<none>"],"RepoDigests":["<none>@<none>"],"Created":1,"Size":10,"SharedSize":-1,"Labels":{},"Containers":0}"#,
        )
        .unwrap();
        let s = image_of(&i);
        assert!(s.dangling && s.digests.is_empty());
        let df: bm::SystemDataUsageResponse = serde_json::from_str(
            r#"{"ImageUsage":{"TotalSize":100,"Reclaimable":40},"VolumeUsage":{"TotalSize":7,"Items":[{"Name":"db","UsageData":{"Size":7,"RefCount":1}}]}}"#,
        )
        .unwrap();
        let u = disk_usage_of(&df);
        assert_eq!(
            (u.images_bytes, u.images_reclaimable_bytes, u.volumes_bytes),
            (100, 40, 7)
        );
        let v: bm::Volume = serde_json::from_str(
            r#"{"Name":"db","Driver":"local","Mountpoint":"/var/lib/docker/volumes/db/_data","Labels":{"com.docker.compose.project":"immich"},"Scope":"local","Options":{}}"#,
        )
        .unwrap();
        let vs = volume_of(&v, &df_volumes(&df));
        assert_eq!(
            (vs.size_bytes, vs.in_use, vs.stack.as_deref()),
            (Some(7), Some(1), Some("immich"))
        );
        let n: bm::Network = serde_json::from_str(
            r#"{"Name":"immich_default","Id":"n1","Driver":"bridge","Scope":"local","IPAM":{"Config":[{"Subnet":"10.200.3.0/24"}]},"Internal":false,"Labels":{"com.docker.compose.project":"immich"}}"#,
        )
        .unwrap();
        let ns = network_of(&n);
        assert_eq!(
            (ns.subnets.as_slice(), ns.stack.as_deref()),
            (&["10.200.3.0/24".to_string()][..], Some("immich"))
        );
    }
}
