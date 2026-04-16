//! Rootless Podman container resolution via overlay metadata and network
//! namespace paths.
//!
//! This entire module is gated behind `#[cfg(target_os = "linux")]` at the
//! `mod` declaration in `lib.rs`.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use log::debug;
use serde::Deserialize;

use crate::ContainerInfo;
use crate::api::short_container_id;

/// Cache for rootless Podman container lookups keyed by process and network namespace.
#[derive(Default)]
pub struct RootlessPodmanResolver {
    containers_by_netns: Option<HashMap<PathBuf, ContainerInfo>>,
    containers_by_pid: HashMap<u32, Option<ContainerInfo>>,
}

#[derive(Deserialize)]
struct PodmanStorageContainer {
    id: String,
    #[serde(default)]
    names: Vec<String>,
    metadata: Option<String>,
}

#[derive(Deserialize)]
struct PodmanStorageMetadata {
    #[serde(rename = "image-name")]
    image_name: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct PodmanContainerConfig {
    linux: Option<PodmanLinuxConfig>,
}

#[derive(Deserialize)]
struct PodmanLinuxConfig {
    #[serde(default)]
    namespaces: Vec<PodmanNamespace>,
}

#[derive(Deserialize)]
struct PodmanNamespace {
    #[serde(rename = "type")]
    namespace_type: String,
    path: Option<PathBuf>,
}

/// Resolve a rootless Podman `rootlessport` helper process back to its container.
///
/// When the Podman API socket is unavailable to the current process, this falls
/// back to local overlay metadata and Linux network namespace paths.
pub fn lookup_rootless_podman_container(
    pid: u32,
    process_name: &str,
    resolver: &mut RootlessPodmanResolver,
    home: Option<&Path>,
) -> Option<ContainerInfo> {
    if !is_podman_rootlessport_process(process_name) {
        return None;
    }

    if let Some(container) = resolver.containers_by_pid.get(&pid) {
        return container.clone();
    }

    let containers_by_netns = resolver
        .containers_by_netns
        .get_or_insert_with(|| load_rootless_podman_containers_by_netns(home));
    let container =
        match_container_by_netns_paths(&read_process_netns_paths(pid), containers_by_netns);

    resolver.containers_by_pid.insert(pid, container.clone());
    container
}

/// Check whether a process name matches the Podman rootless port-forwarder.
#[must_use]
pub const fn is_podman_rootlessport_process(process_name: &str) -> bool {
    process_name.eq_ignore_ascii_case("rootlessport")
}

fn load_rootless_podman_containers_by_netns(
    home: Option<&Path>,
) -> HashMap<PathBuf, ContainerInfo> {
    let mut containers = HashMap::new();

    for overlay_root in podman_overlay_container_roots(home) {
        containers.extend(load_podman_rootless_containers_from_overlay_root(
            &overlay_root,
        ));
    }

    containers
}

fn podman_overlay_container_roots(home: Option<&Path>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut roots = Vec::new();

    let mut push_unique = |path: PathBuf| {
        if seen.insert(path.clone()) {
            roots.push(path);
        }
    };

    if let Some(xdg_data_home) = std::env::var_os("XDG_DATA_HOME") {
        push_unique(PathBuf::from(xdg_data_home).join("containers/storage/overlay-containers"));
    }

    if let Some(home) = home {
        push_unique(home.join(".local/share/containers/storage/overlay-containers"));
    }

    push_unique(PathBuf::from(
        "/var/lib/containers/storage/overlay-containers",
    ));

    roots
}

fn load_podman_rootless_containers_from_overlay_root(
    overlay_root: &Path,
) -> HashMap<PathBuf, ContainerInfo> {
    let catalog_path = overlay_root.join("containers.json");
    let Ok(catalog_json) = fs::read_to_string(catalog_path) else {
        return HashMap::new();
    };
    let Ok(containers) = serde_json::from_str::<Vec<PodmanStorageContainer>>(&catalog_json) else {
        return HashMap::new();
    };

    let mut containers_by_netns = HashMap::new();
    for container in containers {
        let info = podman_storage_container_info(&container);
        let config_path = overlay_root
            .join(&container.id)
            .join("userdata/config.json");
        let Some(netns_path) = read_podman_network_namespace_path(&config_path) else {
            continue;
        };
        containers_by_netns.insert(netns_path, info);
    }

    containers_by_netns
}

fn podman_storage_container_info(container: &PodmanStorageContainer) -> ContainerInfo {
    let metadata: Option<PodmanStorageMetadata> = container
        .metadata
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok());
    let name = container
        .names
        .first()
        .cloned()
        .or_else(|| metadata.as_ref().and_then(|value| value.name.clone()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| short_container_id(&container.id));
    let image = metadata
        .and_then(|value| value.image_name)
        .unwrap_or_default();

    ContainerInfo {
        id: container.id.clone(),
        name,
        image,
    }
}

fn read_podman_network_namespace_path(config_path: &Path) -> Option<PathBuf> {
    let config_json = fs::read_to_string(config_path).ok()?;
    let config = serde_json::from_str::<PodmanContainerConfig>(&config_json).ok()?;

    config.linux?.namespaces.into_iter().find_map(|namespace| {
        (namespace.namespace_type == "network")
            .then_some(namespace.path)
            .flatten()
    })
}

fn read_process_netns_paths(pid: u32) -> Vec<PathBuf> {
    let fd_dir = PathBuf::from("/proc").join(pid.to_string()).join("fd");
    let entries = match fs::read_dir(&fd_dir) {
        Ok(entries) => entries,
        Err(error) => {
            debug!(
                "failed to read process fd directory for rootless Podman lookup: pid={pid} fd_dir={} error={error}",
                fd_dir.display()
            );
            return Vec::new();
        }
    };

    let mut netns_paths = HashSet::new();
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let target = match fs::read_link(&entry_path) {
            Ok(target) => target,
            Err(error) => {
                debug!(
                    "failed to read process fd symlink for rootless Podman lookup: pid={pid} fd_entry={} error={error}",
                    entry_path.display()
                );
                continue;
            }
        };
        if is_podman_network_namespace_path(&target) {
            netns_paths.insert(target);
        }
    }

    let mut netns_paths: Vec<_> = netns_paths.into_iter().collect();
    netns_paths.sort();
    netns_paths
}

fn is_podman_network_namespace_path(path: &Path) -> bool {
    path.parent().and_then(Path::file_name) == Some(OsStr::new("netns"))
        && path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("netns-"))
}

fn match_container_by_netns_paths(
    netns_paths: &[PathBuf],
    containers_by_netns: &HashMap<PathBuf, ContainerInfo>,
) -> Option<ContainerInfo> {
    let mut candidate = None;

    for netns_path in netns_paths {
        let Some(info) = containers_by_netns.get(netns_path) else {
            continue;
        };

        match &candidate {
            None => candidate = Some(info.clone()),
            Some(existing) if existing == info => {}
            Some(_) => return None,
        }
    }

    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_podman_rootless_containers_from_overlay_root_reads_metadata() {
        let overlay_root = TempDir::new().unwrap();
        let container_id = "e603f8ebd438b8405b9b835b9d38cb913ea2479f5b29f8e4308b88e9a92e8c4b";
        let netns_path = "/run/user/1000/netns/netns-demo";

        fs::create_dir_all(overlay_root.path().join(container_id).join("userdata")).unwrap();
        fs::write(
            overlay_root.path().join("containers.json"),
            format!(
                r#"[{{
                    "id": "{container_id}",
                    "names": ["ensurily-postgres-dev"],
                    "metadata": "{{\"image-name\":\"docker.io/library/postgres:14-alpine\",\"name\":\"ensurily-postgres-dev\"}}"
                }}]"#
            ),
        )
        .unwrap();
        fs::write(
            overlay_root
                .path()
                .join(container_id)
                .join("userdata/config.json"),
            format!(r#"{{"linux":{{"namespaces":[{{"type":"network","path":"{netns_path}"}}]}}}}"#),
        )
        .unwrap();

        let containers = load_podman_rootless_containers_from_overlay_root(overlay_root.path());
        let container = containers.get(Path::new(netns_path)).unwrap();

        assert_eq!(container.name, "ensurily-postgres-dev");
        assert_eq!(container.image, "docker.io/library/postgres:14-alpine");
    }

    #[test]
    fn match_container_by_netns_paths_returns_unique_match() {
        let netns_path = PathBuf::from("/run/user/1000/netns/netns-demo");
        let mut containers = HashMap::new();
        containers.insert(
            netns_path.clone(),
            ContainerInfo {
                id: "abc123".to_string(),
                name: "ensurily-redis-dev".to_string(),
                image: "docker.io/library/redis:7.2-alpine".to_string(),
            },
        );

        let container = match_container_by_netns_paths(&[netns_path], &containers).unwrap();
        assert_eq!(container.name, "ensurily-redis-dev");
    }

    #[test]
    fn match_container_by_netns_paths_rejects_conflicting_matches() {
        let first_path = PathBuf::from("/run/user/1000/netns/netns-a");
        let second_path = PathBuf::from("/run/user/1000/netns/netns-b");
        let mut containers = HashMap::new();
        containers.insert(
            first_path.clone(),
            ContainerInfo {
                id: "aaa111".to_string(),
                name: "postgres".to_string(),
                image: "postgres:16".to_string(),
            },
        );
        containers.insert(
            second_path.clone(),
            ContainerInfo {
                id: "bbb222".to_string(),
                name: "redis".to_string(),
                image: "redis:7-alpine".to_string(),
            },
        );

        let container = match_container_by_netns_paths(&[first_path, second_path], &containers);
        assert!(
            container.is_none(),
            "multiple distinct netns matches should not guess a container"
        );
    }
}
