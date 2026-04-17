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
//! ### Best-effort path (background thread, never errors)
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
//!
//! ### Strict path (synchronous, returns errors)
//!
//! ```rust,no_run
//! use nanodock::detect_containers;
//!
//! match detect_containers(None) {
//!     Ok(port_map) => {
//!         for ((ip, port, proto), info) in &port_map {
//!             println!("{proto} port {port} -> {} ({})", info.name, info.image);
//!         }
//!     }
//!     Err(e) => eprintln!("detection failed: {e}"),
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
use serde::{Deserialize, Serialize};

// ── Public API re-exports ────────────────────────────────────────────

pub use api::parse_containers_json;
pub use api::parse_containers_json_strict;
pub use api::short_container_id;
#[cfg(target_os = "linux")]
pub use podman::is_podman_rootlessport_process;
#[cfg(target_os = "linux")]
pub use podman::{RootlessPodmanResolver, lookup_rootless_podman_container};

// ── Error type ───────────────────────────────────────────────────────

/// Error returned by [`detect_containers`] when the daemon cannot be
/// reached or returns an unusable response.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// No container runtime daemon was reachable on any known transport
    /// (Unix sockets, Windows named pipes, or TCP via `DOCKER_HOST`).
    DaemonNotFound,

    /// A daemon transport connected but the response body was not valid
    /// JSON for the container-list endpoint.
    InvalidJson(serde_json::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DaemonNotFound => {
                write!(
                    f,
                    "no container runtime daemon found on any known transport"
                )
            }
            Self::InvalidJson(source) => {
                write!(f, "container daemon returned invalid JSON: {source}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DaemonNotFound => None,
            Self::InvalidJson(source) => Some(source),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::InvalidJson(err)
    }
}

// ── Protocol ─────────────────────────────────────────────────────────

/// Network transport protocol.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerInfo {
    /// Full container ID (hex string) for API calls, empty when unavailable.
    pub id: String,
    /// Container name (e.g. "backend-postgres-1").
    pub name: String,
    /// Container image (e.g. "postgres:16").
    pub image: String,
}

impl std::fmt::Display for ContainerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.image.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{} ({})", self.name, self.image)
        }
    }
}

/// Maps `(host_ip, host_port, protocol)` to container info.
pub type ContainerPortMap = HashMap<(Option<IpAddr>, u16, Protocol), ContainerInfo>;

#[cfg(test)]
fn test_container_info(id: &str, name: &str, image: &str) -> ContainerInfo {
    ContainerInfo {
        id: id.to_string(),
        name: name.to_string(),
        image: image.to_string(),
    }
}

#[cfg(test)]
fn insert_test_container(
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
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PublishedContainerMatch<'a> {
    /// Exactly one container binding matched the socket.
    Match(&'a ContainerInfo),
    /// No published container binding matched the socket.
    NotFound,
    /// Multiple distinct published bindings matched and no safe choice exists.
    Ambiguous,
}

impl std::fmt::Display for PublishedContainerMatch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Match(info) => write!(f, "{info}"),
            Self::NotFound => write!(f, "no matching container"),
            Self::Ambiguous => write!(f, "ambiguous match"),
        }
    }
}

/// Handle for an in-progress Docker/Podman container detection.
///
/// Created by [`start_detection`] and consumed by [`await_detection`].
/// The inner channel is hidden to allow future changes to the detection
/// mechanism without breaking the public API.
#[derive(Debug)]
pub struct DetectionHandle(std::sync::mpsc::Receiver<Option<ContainerPortMap>>);

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

/// Synchronously detect Docker/Podman containers and their published ports.
///
/// Tries all known daemon transports (TCP via `DOCKER_HOST`, Unix
/// sockets on Linux, Windows named pipes) and returns the first
/// successful result. Returns an error if no daemon could be reached
/// or if the response could not be parsed.
///
/// Unlike [`start_detection`] / [`await_detection`], this function
/// blocks the calling thread and surfaces errors so the caller can
/// distinguish "no containers running" (empty map) from "daemon
/// unreachable" ([`Error::DaemonNotFound`]).
///
/// # Errors
///
/// Returns [`Error::DaemonNotFound`] when no transport connected.
/// Returns [`Error::InvalidJson`] when the daemon responded but the
/// body was not valid container-list JSON.
pub fn detect_containers(home: Option<PathBuf>) -> Result<ContainerPortMap, Error> {
    debug!("starting synchronous container runtime detection");
    let body = query_daemon_body(home).ok_or(Error::DaemonNotFound)?;
    let map = api::parse_containers_json_strict(&body).map_err(Error::InvalidJson)?;
    debug!(
        "finished synchronous container runtime detection: port_mappings={}",
        map.len()
    );
    Ok(map)
}

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
    DetectionHandle(rx)
}

/// Wait for Docker/Podman detection to complete.
///
/// Blocks for at most 3 seconds before returning an empty map.
/// Never returns an error - this is best-effort enrichment.
// The handle wraps a `Receiver` which must be consumed (moved) to
// read from it; passing by reference is not possible.
#[allow(clippy::needless_pass_by_value)]
#[must_use]
pub fn await_detection(handle: DetectionHandle) -> ContainerPortMap {
    match handle.0.recv_timeout(ipc::DAEMON_TIMEOUT) {
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
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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

impl std::fmt::Display for StopOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => write!(f, "stopped"),
            Self::AlreadyStopped => write!(f, "already stopped"),
            Self::NotFound => write!(f, "not found"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// Stop or kill a running container via the Docker/Podman daemon API.
///
/// When `force` is false, sends `POST /containers/{id}/stop` (graceful
/// SIGTERM with a 10-second timeout before SIGKILL). When `force` is
/// true, sends `POST /containers/{id}/kill` (immediate SIGKILL).
///
/// The `id` parameter can be a container ID (hex) or a container name.
/// Characters that would corrupt the HTTP request path (`/`, `?`, `#`,
/// control characters, spaces) are rejected early with
/// [`StopOutcome::NotFound`].
///
/// The `home` parameter provides the user's home directory path, used
/// on Unix to discover daemon socket locations.
///
/// Tries all known transports (TCP, Unix sockets, Windows named pipes)
/// and returns the outcome from the first transport that connects.
#[must_use]
pub fn stop_container(id: &str, force: bool, home: Option<PathBuf>) -> StopOutcome {
    if !is_safe_container_id(id) {
        debug!("rejected container id with unsafe characters");
        return StopOutcome::NotFound;
    }

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

/// Reject container IDs that would corrupt the HTTP request line.
///
/// Docker accepts both hex IDs and container names (alphanumeric, hyphens,
/// underscores, dots). This function rejects only characters that could
/// cause path traversal or HTTP header injection.
fn is_safe_container_id(id: &str) -> bool {
    !id.is_empty()
        && !id
            .bytes()
            .any(|b| matches!(b, b'/' | b'?' | b'#' | b'%' | b'\r' | b'\n' | b' '))
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
///
/// A 404 ("not found") response does not short-circuit: the container may
/// exist on a different daemon (e.g., Podman when Docker returns 404).
fn send_stop_request(endpoint: &str, home: Option<PathBuf>) -> Option<u16> {
    // TCP via DOCKER_HOST takes precedence (both platforms).
    if let Some(addr) = ipc::docker_host_tcp_addr()
        && let Some(code) = ipc::stop_via_tcp(&addr, endpoint)
    {
        if code != 404 {
            return Some(code);
        }
        // Container not found on TCP daemon; try platform sockets before
        // giving up, but remember the 404 as a fallback.
        return Some(send_stop_request_platform(endpoint, home).unwrap_or(code));
    }

    send_stop_request_platform(endpoint, home)
}

/// Return `code` if it is NOT a 404, otherwise fold it into `fallback` so the
/// caller can return the 404 only after all daemons have been tried.
const fn fold_stop_code(code: u16, fallback: &mut Option<u16>) -> Option<u16> {
    if code == 404 {
        *fallback = Some(404);
        None
    } else {
        Some(code)
    }
}

#[cfg(unix)]
fn send_stop_request_platform(endpoint: &str, home: Option<PathBuf>) -> Option<u16> {
    use std::path::Path;

    let mut not_found = None;

    // Honour DOCKER_HOST unix:// if set.
    if let Some(path) = ipc::docker_host_unix_path()
        && let Some(code) = ipc::stop_via_unix_socket(Path::new(&path), endpoint)
        && let Some(result) = fold_stop_code(code, &mut not_found)
    {
        return Some(result);
    }

    // Safety: getuid() is a simple syscall with no preconditions.
    let uid = unsafe { libc::getuid() };
    for path in ipc::unix_socket_paths(uid, home) {
        if let Some(code) = ipc::stop_via_unix_socket(Path::new(&path), endpoint)
            && let Some(result) = fold_stop_code(code, &mut not_found)
        {
            return Some(result);
        }
    }
    not_found
}

#[cfg(windows)]
fn send_stop_request_platform(endpoint: &str, _home: Option<PathBuf>) -> Option<u16> {
    let mut not_found = None;

    // Honour DOCKER_HOST npipe:// if set.
    if let Some(path) = ipc::docker_host_npipe_path()
        && let Some(code) = ipc::stop_via_named_pipe(&path, endpoint)
        && let Some(result) = fold_stop_code(code, &mut not_found)
    {
        return Some(result);
    }

    for path in DEFAULT_PIPE_PATHS {
        if let Some(code) = ipc::stop_via_named_pipe(path, endpoint)
            && let Some(result) = fold_stop_code(code, &mut not_found)
        {
            return Some(result);
        }
    }
    not_found
}

// ── Platform-specific daemon queries ─────────────────────────────────

/// If `DOCKER_HOST` is set to a `tcp://` URL, query it and return the
/// raw JSON body.
///
/// Shared across Unix and Windows since the TCP transport is
/// platform-agnostic.
fn query_docker_host_tcp_body() -> Option<String> {
    let addr = ipc::docker_host_tcp_addr()?;
    ipc::fetch_tcp_json(&addr)
}

/// If `DOCKER_HOST` is set to a `tcp://` URL, query it and return the map.
///
/// Shared across Unix and Windows since the TCP transport is platform-agnostic.
fn query_docker_host_tcp() -> Option<ContainerPortMap> {
    query_docker_host_tcp_body().map(|body| api::parse_containers_json(&body))
}

#[cfg(unix)]
fn query_daemon_body(home: Option<PathBuf>) -> Option<String> {
    use std::path::Path;

    if let Some(body) = query_docker_host_tcp_body() {
        return Some(body);
    }

    if let Some(path) = ipc::docker_host_unix_path() {
        return ipc::fetch_unix_socket_json(Path::new(&path));
    }

    // Safety: getuid() is a simple syscall with no preconditions.
    let uid = unsafe { libc::getuid() };
    let responses = ipc::fetch_all_successes(ipc::unix_socket_paths(uid, home), |path| {
        ipc::fetch_unix_socket_json(Path::new(&path))
    });

    merge_daemon_response_bodies(responses)
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
fn query_daemon_body(_home: Option<PathBuf>) -> Option<String> {
    if let Some(body) = query_docker_host_tcp_body() {
        return Some(body);
    }

    let deadline = std::time::Instant::now() + ipc::DAEMON_TIMEOUT;

    if let Some(path) = ipc::docker_host_npipe_path()
        && let Some(body) = ipc::fetch_named_pipe_json(&path, deadline)
    {
        return Some(body);
    }

    DEFAULT_PIPE_PATHS
        .iter()
        .find_map(|path| ipc::fetch_named_pipe_json(path, deadline))
}

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
fn merge_daemon_response_bodies<T, I>(responses: I) -> Option<String>
where
    T: AsRef<str>,
    I: IntoIterator<Item = T>,
{
    let mut saw_response = false;
    let mut has_content = false;
    let mut combined = String::from("[");

    for response in responses {
        saw_response = true;
        let body = response.as_ref().trim();
        // Each daemon returns a JSON array; unwrap the outer brackets and
        // concatenate elements so the caller sees a single flat array.
        let inner = body
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(body)
            .trim();
        if inner.is_empty() {
            continue;
        }
        if has_content {
            combined.push(',');
        }
        has_content = true;
        combined.push_str(inner);
    }

    if !saw_response {
        return None;
    }

    combined.push(']');
    Some(combined)
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

    // ── interpret_stop_status ────────────────────────────────────────

    #[test]
    fn interpret_stop_status_204_means_stopped() {
        assert_eq!(
            interpret_stop_status(204, false),
            StopOutcome::Stopped,
            "204 should mean stopped for graceful stop"
        );
        assert_eq!(
            interpret_stop_status(204, true),
            StopOutcome::Stopped,
            "204 should mean stopped for force kill"
        );
    }

    #[test]
    fn interpret_stop_status_304_means_already_stopped() {
        assert_eq!(
            interpret_stop_status(304, false),
            StopOutcome::AlreadyStopped,
            "304 from stop endpoint means already stopped"
        );
    }

    #[test]
    fn interpret_stop_status_409_on_force_means_already_stopped() {
        assert_eq!(
            interpret_stop_status(409, true),
            StopOutcome::AlreadyStopped,
            "409 from kill endpoint means container not running"
        );
    }

    #[test]
    fn interpret_stop_status_409_on_graceful_means_failed() {
        assert_eq!(
            interpret_stop_status(409, false),
            StopOutcome::Failed,
            "409 on non-force is unexpected and should map to Failed"
        );
    }

    #[test]
    fn interpret_stop_status_404_means_not_found() {
        assert_eq!(
            interpret_stop_status(404, false),
            StopOutcome::NotFound,
            "404 means container not found"
        );
    }

    #[test]
    fn interpret_stop_status_500_means_failed() {
        assert_eq!(
            interpret_stop_status(500, false),
            StopOutcome::Failed,
            "server error should map to Failed"
        );
    }

    // ── fold_stop_code ───────────────────────────────────────────────

    #[test]
    fn fold_stop_code_passes_non_404_through() {
        let mut fallback = None;
        assert_eq!(
            fold_stop_code(204, &mut fallback),
            Some(204),
            "non-404 should pass through"
        );
        assert_eq!(fallback, None, "fallback should remain None");
    }

    #[test]
    fn fold_stop_code_defers_404_to_fallback() {
        let mut fallback = None;
        assert_eq!(
            fold_stop_code(404, &mut fallback),
            None,
            "404 should be deferred"
        );
        assert_eq!(fallback, Some(404), "fallback should record the 404");
    }

    // ── merge_daemon_response_bodies ─────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn merge_bodies_empty_iterator_returns_none() {
        let result = merge_daemon_response_bodies::<&str, Vec<&str>>(vec![]);
        assert!(result.is_none(), "no responses means None");
    }

    #[cfg(unix)]
    #[test]
    fn merge_bodies_single_empty_array() {
        let result = merge_daemon_response_bodies(["[]"]);
        assert_eq!(
            result.as_deref(),
            Some("[]"),
            "single empty array should produce []"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_bodies_concatenates_non_empty_arrays() {
        let result = merge_daemon_response_bodies([r#"[{"a":1}]"#, r#"[{"b":2},{"c":3}]"#]);
        assert_eq!(
            result.as_deref(),
            Some(r#"[{"a":1},{"b":2},{"c":3}]"#),
            "elements from both arrays should be combined"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_bodies_skips_empty_arrays_without_spurious_commas() {
        let result = merge_daemon_response_bodies(["[]", r#"[{"a":1}]"#]);
        assert_eq!(
            result.as_deref(),
            Some(r#"[{"a":1}]"#),
            "empty arrays should not introduce leading commas"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_bodies_trailing_empty_array_does_not_add_comma() {
        let result = merge_daemon_response_bodies([r#"[{"a":1}]"#, "[]"]);
        assert_eq!(
            result.as_deref(),
            Some(r#"[{"a":1}]"#),
            "trailing empty array should not add trailing comma"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_bodies_all_empty_arrays_produces_empty_array() {
        let result = merge_daemon_response_bodies(["[]", "[]"]);
        assert_eq!(result.as_deref(), Some("[]"), "all-empty should produce []");
    }

    // ── is_safe_container_id ─────────────────────────────────────────

    #[test]
    fn safe_id_accepts_hex_id() {
        assert!(
            is_safe_container_id("abc123def456"),
            "hex ID should be valid"
        );
    }

    #[test]
    fn safe_id_accepts_container_name() {
        assert!(
            is_safe_container_id("my-container_1.0"),
            "name with hyphens, underscores, dots should be valid"
        );
    }

    #[test]
    fn safe_id_rejects_empty() {
        assert!(!is_safe_container_id(""), "empty ID should be rejected");
    }

    #[test]
    fn safe_id_rejects_path_traversal() {
        assert!(
            !is_safe_container_id("../../../etc/passwd"),
            "path traversal should be rejected"
        );
    }

    #[test]
    fn safe_id_rejects_query_injection() {
        assert!(
            !is_safe_container_id("abc?signal=SIGKILL"),
            "query injection should be rejected"
        );
    }

    #[test]
    fn safe_id_rejects_crlf_injection() {
        assert!(
            !is_safe_container_id("abc\r\nX-Injected: true"),
            "CRLF injection should be rejected"
        );
    }
}
