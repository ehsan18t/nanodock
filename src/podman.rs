//! Rootless Podman container resolution via overlay metadata and network
//! namespace paths.
//!
//! The public items exist on every platform so callers need no `cfg` gates of
//! their own. Only Linux runs `rootlessport` on the host, so the lookup
//! returns `None` elsewhere. The code is still compiled and type-checked on
//! every platform; the platform check is a `cfg!` branch, not an item gate.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use log::debug;
use serde::Deserialize;

use crate::ContainerInfo;
use crate::api::short_container_id;

/// Resolves rootless Podman `rootlessport` helper processes back to their
/// containers, caching what it reads.
///
/// When the Podman API socket is unavailable to the current process, the
/// resolver falls back to local overlay storage metadata and Linux network
/// namespace paths. It reads the storage once, on the first lookup, and
/// remembers the result of every process it looks up.
///
/// **Use one resolver per scan.** The cache is never refreshed on its own:
/// containers started after the first lookup are missing from it, and a
/// process ID reused by a new `rootlessport` keeps the old answer. Create a
/// new resolver for each scan, or call [`RootlessPodmanResolver::clear`]
/// between scans.
///
/// Available on every platform. Outside Linux [`RootlessPodmanResolver::lookup`]
/// returns `None` without touching the filesystem, so the resolver stays
/// empty: rootless Podman's `rootlessport` helper only runs on a Linux host
/// (a Podman machine on macOS or Windows runs it inside the VM).
///
/// ```
/// use nanodock::RootlessPodmanResolver;
///
/// let mut resolver = RootlessPodmanResolver::new();
/// // Only `rootlessport` processes are resolved.
/// assert_eq!(resolver.lookup(1, "nginx"), None);
/// ```
#[derive(Debug)]
pub struct RootlessPodmanResolver {
    home: Option<PathBuf>,
    containers_by_netns: Option<HashMap<PathBuf, ContainerInfo>>,
    containers_by_pid: HashMap<u32, Option<ContainerInfo>>,
}

impl Default for RootlessPodmanResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl RootlessPodmanResolver {
    /// Create a resolver that searches the user's home directory, from
    /// [`std::env::home_dir`], for rootless Podman storage.
    ///
    /// Storage is looked for below `$XDG_DATA_HOME`, then
    /// `~/.local/share/containers`, then `/var/lib/containers`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            home: std::env::home_dir(),
            containers_by_netns: None,
            containers_by_pid: HashMap::new(),
        }
    }

    /// Set the home directory below which rootless Podman storage is
    /// searched, replacing the one from the environment, and clear the
    /// cache. With `None`, only `$XDG_DATA_HOME` and `/var/lib/containers`
    /// are searched.
    #[must_use]
    pub fn home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self.clear();
        self
    }

    /// Resolve a rootless Podman `rootlessport` helper process back to its
    /// container.
    ///
    /// Returns `None` when `process_name` is not `rootlessport`, when the
    /// process shares no network namespace with exactly one known
    /// container, and always outside Linux. The answer for each `pid` is
    /// cached; see the type documentation for when to start over.
    pub fn lookup(&mut self, pid: u32, process_name: &str) -> Option<ContainerInfo> {
        if !cfg!(target_os = "linux") || !is_podman_rootlessport_process(process_name) {
            return None;
        }

        if let Some(container) = self.containers_by_pid.get(&pid) {
            return container.clone();
        }

        let netns_paths = read_process_netns_paths(pid);
        let container = match_container_by_netns_paths(&netns_paths, self.containers_by_netns());
        self.containers_by_pid.insert(pid, container.clone());
        container
    }

    /// Forget everything read so far, so the next lookup reads the overlay
    /// storage and the process again. The home directory is kept.
    pub fn clear(&mut self) {
        self.containers_by_netns = None;
        self.containers_by_pid.clear();
    }

    /// The containers by network namespace path, read on first use.
    fn containers_by_netns(&mut self) -> &HashMap<PathBuf, ContainerInfo> {
        let home = self.home.as_deref();
        self.containers_by_netns
            .get_or_insert_with(|| load_rootless_podman_containers_by_netns(home))
    }
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

/// Check whether a process name matches the Podman rootless port-forwarder.
///
/// Matches `rootlessport` only (ASCII case-insensitive), which is the process
/// [`RootlessPodmanResolver::lookup`] can resolve. To recognize every
/// container runtime port proxy, use [`crate::is_container_proxy_process`].
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
    // An empty name, in `names` or the metadata, falls through to the next
    // source instead of hiding it.
    let name = container
        .names
        .iter()
        .map(String::as_str)
        .chain(metadata.as_ref().and_then(|value| value.name.as_deref()))
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map_or_else(|| short_container_id(&container.id), ToOwned::to_owned);
    let image = metadata
        .and_then(|value| value.image_name)
        .unwrap_or_default();

    ContainerInfo::new(container.id.clone(), name, image)
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

    /// Write a rootless overlay storage catalog with one container to
    /// `overlay_root`, its network namespace at `netns_path`.
    fn write_overlay_container(overlay_root: &Path, id: &str, entry: &str, netns_path: &str) {
        let userdata = overlay_root.join(id).join("userdata");
        fs::create_dir_all(&userdata).expect("create the container directory");
        fs::write(overlay_root.join("containers.json"), format!("[{entry}]"))
            .expect("write the container catalog");
        fs::write(
            userdata.join("config.json"),
            format!(r#"{{"linux":{{"namespaces":[{{"type":"network","path":"{netns_path}"}}]}}}}"#),
        )
        .expect("write the runtime config");
    }

    #[test]
    fn lookup_ignores_processes_other_than_rootlessport() {
        let mut resolver = RootlessPodmanResolver::new().home(None);

        let container = resolver.lookup(1, "nginx");

        assert_eq!(container, None, "only rootlessport is resolved");
        assert!(
            resolver.containers_by_netns.is_none() && resolver.containers_by_pid.is_empty(),
            "a non-rootlessport process must not load or cache overlay metadata"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn lookup_returns_none_outside_linux() {
        let home = TempDir::new().expect("create a home directory");
        let mut resolver = RootlessPodmanResolver::new().home(Some(home.path().to_path_buf()));

        let container = resolver.lookup(std::process::id(), "rootlessport");

        assert_eq!(container, None, "rootlessport only runs on a Linux host");
        assert!(
            resolver.containers_by_netns.is_none() && resolver.containers_by_pid.is_empty(),
            "the resolver stays untouched outside Linux"
        );
    }

    #[test]
    fn resolver_reads_storage_below_its_home_and_clear_forgets_it() {
        let home = TempDir::new().expect("create a home directory");
        let overlay_root = home
            .path()
            .join(".local/share/containers/storage/overlay-containers");
        let netns_path = "/run/user/1000/netns/netns-home-test";
        write_overlay_container(
            &overlay_root,
            "abc123",
            r#"{"id": "abc123", "names": ["web"]}"#,
            netns_path,
        );
        let mut resolver = RootlessPodmanResolver::new().home(Some(home.path().to_path_buf()));

        let found = resolver
            .containers_by_netns()
            .get(Path::new(netns_path))
            .map(|info| info.name.clone());
        assert_eq!(found.as_deref(), Some("web"), "storage below home is read");

        resolver.containers_by_pid.insert(42, None);
        resolver.clear();
        assert!(
            resolver.containers_by_netns.is_none() && resolver.containers_by_pid.is_empty(),
            "clear forgets the storage and every process"
        );
        assert_eq!(
            resolver.home.as_deref(),
            Some(home.path()),
            "clear keeps the home directory"
        );
    }

    #[test]
    fn empty_names_fall_back_to_the_metadata_name_then_the_short_id() {
        let id = "e603f8ebd438b8405b9b835b9d38cb913ea2479f5b29f8e4308b88e9a92e8c4b";
        let metadata = r#"{\"name\":\"from-metadata\"}"#;
        let cases = [
            (
                format!(r#"{{"id": "{id}", "names": [""], "metadata": "{metadata}"}}"#),
                "from-metadata",
                "an empty name must not hide the metadata name",
            ),
            (
                format!(r#"{{"id": "{id}", "names": ["", "second"]}}"#),
                "second",
                "the first non-empty name wins",
            ),
            (
                format!(r#"{{"id": "{id}", "metadata": "{metadata}"}}"#),
                "from-metadata",
                "missing names fall back to the metadata name",
            ),
            (
                format!(r#"{{"id": "{id}", "names": [" "], "metadata": "{{\"name\":\"\"}}"}}"#),
                "e603f8ebd438",
                "no usable name falls back to the short id",
            ),
        ];
        for (entry, expected, why) in cases {
            let container: PodmanStorageContainer =
                serde_json::from_str(&entry).expect("valid catalog entry");
            assert_eq!(
                podman_storage_container_info(&container).name,
                expected,
                "{why}"
            );
        }
    }

    #[test]
    fn match_container_by_netns_paths_returns_unique_match() {
        let netns_path = PathBuf::from("/run/user/1000/netns/netns-demo");
        let mut containers = HashMap::new();
        containers.insert(
            netns_path.clone(),
            ContainerInfo::new(
                "abc123",
                "ensurily-redis-dev",
                "docker.io/library/redis:7.2-alpine",
            ),
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
            ContainerInfo::new("aaa111", "postgres", "postgres:16"),
        );
        containers.insert(
            second_path.clone(),
            ContainerInfo::new("bbb222", "redis", "redis:7-alpine"),
        );

        let container = match_container_by_netns_paths(&[first_path, second_path], &containers);
        assert!(
            container.is_none(),
            "multiple distinct netns matches should not guess a container"
        );
    }
}
