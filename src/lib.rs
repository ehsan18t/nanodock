//! # `nanodock`
//!
//! Zero-dependency-light Docker/Podman daemon client for container
//! detection, port mapping, and lifecycle control.
//!
//! ## Module structure
//!
//! - `api` - JSON response parsing and container name resolution.
//! - `http` - Minimal HTTP/1.0 response parser (headers via `httparse`).
//! - `ipc` - OS-specific transport (Unix socket, Windows named pipe, TCP).
//! - `podman` - Rootless Podman resolver via overlay metadata (Linux only).
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use nanodock::{start_detection, await_detection};
//!
//! let handle = start_detection(None);
//! // ... do other work while detection runs in the background ...
//! let port_map = await_detection(handle);
//! for ((ip, port, proto), info) in &port_map {
//!     println!("{proto} port {port} -> {} ({})", info.name, info.image);
//! }
//! ```

mod api;
mod http;
mod ipc;
#[cfg(target_os = "linux")]
mod podman;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use log::debug;
use serde::Serialize;

// ── Public API re-exports ────────────────────────────────────────────

pub use api::parse_containers_json;
pub use api::short_container_id;
#[cfg(target_os = "linux")]
pub use podman::is_podman_rootlessport_process;
#[cfg(target_os = "linux")]
pub use podman::{RootlessPodmanResolver, lookup_rootless_podman_container};

// ── Protocol ─────────────────────────────────────────────────────────

/// Network transport protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Protocol {
    /// Transmission Control Protocol.
    #[serde(rename = "TCP")]
    Tcp,
    /// User Datagram Protocol.
    #[serde(rename = "UDP")]
    Udp,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(f, "TCP"),
            Self::Udp => write!(f, "UDP"),
        }
    }
}

// ── Container types ──────────────────────────────────────────────────

/// Metadata about a running container that has published ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Full container ID (hex string) for API calls, empty when unavailable.
    pub id: String,
    /// Container name (e.g. "backend-postgres-1").
    pub name: String,
    /// Container image (e.g. "postgres:16").
    pub image: String,
}

/// Maps `(host_ip, host_port, protocol)` to container info.
pub type ContainerPortMap = HashMap<(Option<IpAddr>, u16, Protocol), ContainerInfo>;

/// Build a reusable container fixture for test maps.
#[must_use]
pub fn test_container_info(id: &str, name: &str, image: &str) -> ContainerInfo {
    ContainerInfo {
        id: id.to_string(),
        name: name.to_string(),
        image: image.to_string(),
    }
}

/// Insert a reusable container fixture into a test port map.
pub fn insert_test_container(
    map: &mut ContainerPortMap,
    host_ip: Option<IpAddr>,
    port: u16,
    proto: Protocol,
    id: &str,
    name: &str,
    image: &str,
) {
    map.insert((host_ip, port, proto), test_container_info(id, name, image));
}

/// Result of matching a socket against published container port bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishedContainerMatch<'a> {
    /// Exactly one container binding matched the socket.
    Match(&'a ContainerInfo),
    /// No published container binding matched the socket.
    NotFound,
    /// Multiple distinct published bindings matched and no safe choice exists.
    Ambiguous,
}

/// Handle for an in-progress Docker/Podman container detection.
///
/// Created by [`start_detection`] and consumed by [`await_detection`].
pub type DetectionHandle = std::sync::mpsc::Receiver<Option<ContainerPortMap>>;

/// Match a local socket against known published container bindings.
///
/// Exact `(host_ip, port, proto)` matches win first. If the daemon reported an
/// unspecified host IP (stored as `None`), the wildcard binding is used next.
/// For known proxy/helper processes, callers may enable `allow_proxy_fallback`
/// to accept a unique `(port, proto)` match when the proxy socket address does
/// not line up with the published host IP.
#[must_use]
pub fn lookup_published_container(
    container_map: &ContainerPortMap,
    socket: SocketAddr,
    proto: Protocol,
    allow_proxy_fallback: bool,
) -> PublishedContainerMatch<'_> {
    if let Some(container) = container_map.get(&(Some(socket.ip()), socket.port(), proto)) {
        return PublishedContainerMatch::Match(container);
    }

    if let Some(container) = container_map.get(&(None, socket.port(), proto)) {
        return PublishedContainerMatch::Match(container);
    }

    if allow_proxy_fallback {
        return unique_published_container(container_map, socket.port(), proto);
    }

    PublishedContainerMatch::NotFound
}

fn unique_published_container(
    container_map: &ContainerPortMap,
    port: u16,
    proto: Protocol,
) -> PublishedContainerMatch<'_> {
    let mut matches = container_map
        .iter()
        .filter(|((_, candidate_port, candidate_proto), _)| {
            *candidate_port == port && *candidate_proto == proto
        })
        .map(|(_, container)| container);

    let Some(first) = matches.next() else {
        return PublishedContainerMatch::NotFound;
    };

    if matches.all(|candidate| candidate == first) {
        PublishedContainerMatch::Match(first)
    } else {
        PublishedContainerMatch::Ambiguous
    }
}

// ── Detection orchestration ──────────────────────────────────────────

/// Start asynchronous detection of Docker/Podman containers.
///
/// Spawns a background thread to query the Docker/Podman daemon.
/// The returned handle should be passed to [`await_detection`] to
/// retrieve the results. This allows other work (socket enumeration,
/// process metadata refresh) to proceed concurrently.
///
/// The `home` parameter provides the user's home directory path, used
/// on Unix to discover rootless Docker/Podman socket locations.
#[must_use]
pub fn start_detection(home: Option<PathBuf>) -> DetectionHandle {
    let (tx, rx) = std::sync::mpsc::channel();
    debug!("starting container runtime detection");
    std::thread::spawn(move || {
        let result = query_daemon(home);
        debug!(
            "finished container runtime detection: port_mappings={}",
            result.as_ref().map_or(0, HashMap::len)
        );
        // Ignore send error: receiver may have timed out and been dropped.
        drop(tx.send(result));
    });
    rx
}

/// Wait for Docker/Podman detection to complete.
///
/// Blocks for at most 3 seconds before returning an empty map.
/// Never returns an error - this is best-effort enrichment.
// The handle is a `Receiver` which must be consumed (moved) to
// read from it; passing by reference is not possible.
#[allow(clippy::needless_pass_by_value)]
#[must_use]
pub fn await_detection(handle: DetectionHandle) -> ContainerPortMap {
    match handle.recv_timeout(ipc::DAEMON_TIMEOUT) {
        Ok(Some(container_map)) => container_map,
        Ok(None) => {
            debug!("container runtime detection returned no data");
            ContainerPortMap::default()
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            debug!(
                "container runtime detection timed out: timeout_secs={}",
                ipc::DAEMON_TIMEOUT.as_secs()
            );
            ContainerPortMap::default()
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            debug!("container runtime detection channel disconnected");
            ContainerPortMap::default()
        }
    }
}

// ── Container stop / kill ────────────────────────────────────────────

/// Result of attempting to stop or kill a container via the daemon API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Container was successfully stopped (HTTP 204).
    Stopped,
    /// Container was already stopped (HTTP 304 for stop, 409 for kill).
    AlreadyStopped,
    /// Container was not found (HTTP 404).
    NotFound,
    /// The daemon could not be reached or returned an unexpected status.
    Failed,
}

/// Stop or kill a running container via the Docker/Podman daemon API.
///
/// When `force` is false, sends `POST /containers/{id}/stop` (graceful
/// SIGTERM with a 10-second timeout before SIGKILL). When `force` is
/// true, sends `POST /containers/{id}/kill` (immediate SIGKILL).
///
/// The `home` parameter provides the user's home directory path, used
/// on Unix to discover daemon socket locations.
///
/// Tries all known transports (TCP, Unix sockets, Windows named pipes)
/// and returns the outcome from the first transport that connects.
#[must_use]
pub fn stop_container(id: &str, force: bool, home: Option<PathBuf>) -> StopOutcome {
    let endpoint = if force {
        format!("/containers/{id}/kill")
    } else {
        format!("/containers/{id}/stop")
    };
    debug!(
        "attempting container stop: id={} force={force} endpoint={endpoint}",
        &id[..id.len().min(12)]
    );

    send_stop_request(&endpoint, home).map_or_else(
        || {
            debug!("no transport could reach container runtime daemon for stop");
            StopOutcome::Failed
        },
        |status_code| interpret_stop_status(status_code, force),
    )
}

/// Map an HTTP status code from the stop/kill endpoint to `StopOutcome`.
fn interpret_stop_status(status_code: u16, force: bool) -> StopOutcome {
    match status_code {
        204 => StopOutcome::Stopped,
        // POST /containers/{id}/stop returns 304 when already stopped.
        304 => StopOutcome::AlreadyStopped,
        // POST /containers/{id}/kill returns 409 when container is not running.
        409 if force => StopOutcome::AlreadyStopped,
        404 => StopOutcome::NotFound,
        _ => {
            debug!("unexpected status code from container stop endpoint: {status_code}");
            StopOutcome::Failed
        }
    }
}

/// Try each known transport until one successfully sends the POST request.
fn send_stop_request(endpoint: &str, home: Option<PathBuf>) -> Option<u16> {
    // TCP via DOCKER_HOST takes precedence (both platforms).
    if let Some(addr) = ipc::docker_host_tcp_addr()
        && let Some(code) = ipc::stop_via_tcp(&addr, endpoint)
    {
        return Some(code);
    }

    send_stop_request_platform(endpoint, home)
}

#[cfg(unix)]
fn send_stop_request_platform(endpoint: &str, home: Option<PathBuf>) -> Option<u16> {
    use std::path::Path;

    // Honour DOCKER_HOST unix:// if set.
    if let Some(path) = ipc::docker_host_unix_path()
        && let Some(code) = ipc::stop_via_unix_socket(Path::new(&path), endpoint)
    {
        return Some(code);
    }

    // Safety: getuid() is a simple syscall with no preconditions.
    let uid = unsafe { libc::getuid() };
    for path in ipc::unix_socket_paths(uid, home) {
        if let Some(code) = ipc::stop_via_unix_socket(Path::new(&path), endpoint) {
            return Some(code);
        }
    }
    None
}

#[cfg(windows)]
fn send_stop_request_platform(endpoint: &str, _home: Option<PathBuf>) -> Option<u16> {
    // Honour DOCKER_HOST npipe:// if set.
    if let Some(path) = ipc::docker_host_npipe_path()
        && let Some(code) = ipc::stop_via_named_pipe(&path, endpoint)
    {
        return Some(code);
    }

    DEFAULT_PIPE_PATHS
        .iter()
        .find_map(|path| ipc::stop_via_named_pipe(path, endpoint))
}

// ── Platform-specific daemon queries ─────────────────────────────────

/// If `DOCKER_HOST` is set to a `tcp://` URL, query it and return the map.
///
/// Shared across Unix and Windows since the TCP transport is platform-agnostic.
fn query_docker_host_tcp() -> Option<ContainerPortMap> {
    let addr = ipc::docker_host_tcp_addr()?;
    ipc::fetch_tcp_json(&addr).map(|body| api::parse_containers_json(&body))
}

#[cfg(unix)]
fn query_daemon(home: Option<PathBuf>) -> Option<ContainerPortMap> {
    use std::path::Path;

    if let Some(map) = query_docker_host_tcp() {
        return Some(map);
    }

    // Honour DOCKER_HOST when it points at a Unix socket (unix://).
    if let Some(path) = ipc::docker_host_unix_path() {
        return ipc::fetch_unix_socket_json(Path::new(&path))
            .map(|body| api::parse_containers_json(&body));
    }

    // Safety: getuid() is a simple syscall with no preconditions.
    let uid = unsafe { libc::getuid() };
    let responses = ipc::fetch_all_successes(ipc::unix_socket_paths(uid, home), |path| {
        ipc::fetch_unix_socket_json(Path::new(&path))
    });

    merge_daemon_responses(responses)
}

#[cfg(windows)]
const DEFAULT_PIPE_PATHS: &[&str] = &[
    r"\\.\pipe\docker_engine",
    r"\\.\pipe\podman-machine-default",
];

#[cfg(windows)]
fn query_daemon(_home: Option<PathBuf>) -> Option<ContainerPortMap> {
    if let Some(map) = query_docker_host_tcp() {
        return Some(map);
    }

    let deadline = std::time::Instant::now() + ipc::DAEMON_TIMEOUT;

    // Honour DOCKER_HOST when it points at a named pipe (npipe://).
    if let Some(path) = ipc::docker_host_npipe_path()
        && let Some(body) = ipc::fetch_named_pipe_json(&path, deadline)
    {
        return Some(api::parse_containers_json(&body));
    }

    DEFAULT_PIPE_PATHS
        .iter()
        .find_map(|path| ipc::fetch_named_pipe_json(path, deadline))
        .map(|body| api::parse_containers_json(&body))
}

#[cfg(unix)]
fn merge_daemon_responses<T, I>(responses: I) -> Option<ContainerPortMap>
where
    T: AsRef<str>,
    I: IntoIterator<Item = T>,
{
    let mut saw_response = false;
    let mut merged = ContainerPortMap::new();

    for response in responses {
        saw_response = true;
        merged.extend(api::parse_containers_json(response.as_ref()));
    }

    saw_response.then_some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[cfg(unix)]
    #[test]
    fn merge_daemon_responses_combines_multiple_runtime_payloads() {
        let merged = merge_daemon_responses([
            "[]",
            r#"[{
                "Names": ["/backend-postgres-1"],
                "Image": "postgres:16",
                "Ports": [{"PublicPort": 5432, "Type": "tcp"}]
            }]"#,
        ])
        .expect("at least one daemon response should produce a map");

        let container = merged
            .get(&(None, 5432, Protocol::Tcp))
            .expect("podman/docker ports should survive multi-daemon merging");
        assert_eq!(container.name, "backend-postgres-1");
        assert_eq!(container.image, "postgres:16");
    }

    #[test]
    fn lookup_published_container_keeps_protocol_bindings_separate() {
        let mut map = ContainerPortMap::new();
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            53,
            Protocol::Tcp,
            "tcp53",
            "dns-tcp",
            "bind9",
        );
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            53,
            Protocol::Udp,
            "udp53",
            "dns-udp",
            "bind9",
        );

        let tcp = lookup_published_container(
            &map,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53),
            Protocol::Tcp,
            false,
        );
        let udp = lookup_published_container(
            &map,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 53),
            Protocol::Udp,
            false,
        );

        assert!(matches!(
            tcp,
            PublishedContainerMatch::Match(info) if info.name == "dns-tcp"
        ));
        assert!(matches!(
            udp,
            PublishedContainerMatch::Match(info) if info.name == "dns-udp"
        ));
    }

    #[test]
    fn lookup_published_container_marks_ambiguous_proxy_matches() {
        let mut map = ContainerPortMap::new();
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            8080,
            Protocol::Tcp,
            "api-a",
            "api-a",
            "node:22",
        );
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))),
            8080,
            Protocol::Tcp,
            "api-b",
            "api-b",
            "node:22",
        );

        let result = lookup_published_container(
            &map,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080),
            Protocol::Tcp,
            true,
        );

        assert_eq!(result, PublishedContainerMatch::Ambiguous);
    }

    #[test]
    fn lookup_published_container_uses_normalized_wildcard_bindings() {
        let map = api::parse_containers_json(
            r#"[{
                "Names": ["/postgres"],
                "Image": "postgres:16",
                "Ports": [{"IP": "0.0.0.0", "PrivatePort": 5432, "PublicPort": 5432, "Type": "tcp"}]
            }]"#,
        );

        let result = lookup_published_container(
            &map,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5432),
            Protocol::Tcp,
            false,
        );

        assert!(matches!(
            result,
            PublishedContainerMatch::Match(info) if info.name == "postgres"
        ));
    }
}
