//! # `nanodock`
//!
//! Minimal-dependency, synchronous Docker/Podman daemon client for container
//! detection, port mapping, and lifecycle control. Runtime dependencies are
//! `serde`, `serde_json`, `httparse`, and `log`, plus `libc` on Unix.
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
use std::time::Instant;

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
/// Tries all known daemon transports concurrently under one shared time
/// budget. A `DOCKER_HOST` `tcp://` daemon is queried alongside the local
/// Unix sockets or Windows named pipes and is used on its own when it
/// answers; otherwise the containers of all answering local daemons are
/// merged. Returns an error if no daemon could be
/// reached or if the response could not be parsed.
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
    /// No daemon could be reached, the daemon returned an unexpected status,
    /// or it received the request but gave no usable reply (the
    /// container may or may not have been stopped).
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
/// When `force` is false, sends `POST /containers/{id}/stop?t=10`
/// (graceful SIGTERM, then SIGKILL after 10 seconds). When `force` is
/// true, sends `POST /containers/{id}/kill` (immediate SIGKILL).
///
/// The `id` parameter can be a container ID (hex) or a container name.
/// Characters that would corrupt the HTTP request path (`/`, `?`, `#`,
/// `%`, control characters, spaces) are rejected early with
/// [`StopOutcome::NotFound`].
///
/// The `home` parameter provides the user's home directory path, used
/// on Unix to discover daemon socket locations.
///
/// Tries the same daemons as detection, in priority order (`DOCKER_HOST`
/// first, then the platform defaults; a `unix://` `DOCKER_HOST` replaces the
/// default sockets). Before the stop request is sent to a daemon, it must
/// answer `GET /_ping` on a separate connection; a daemon that cannot be
/// reached or does not answer the ping (for example a forwarder whose
/// backend is down) is skipped without receiving the stop.
///
/// Any reply from the `DOCKER_HOST` daemon, including "not found", is the
/// result. A "not found" from a default daemon moves on to the next one,
/// and the first other reply is the result. Once a daemon has received the
/// stop request, a closed or reset connection, a timeout, or a partial or
/// malformed reply ends the search with [`StopOutcome::Failed`], even when
/// an earlier daemon answered "not found": that daemon may still be
/// stopping the container, so no other daemon is tried.
#[must_use]
pub fn stop_container(id: &str, force: bool, home: Option<PathBuf>) -> StopOutcome {
    if !is_safe_container_id(id) {
        debug!("rejected container id with unsafe characters");
        return StopOutcome::NotFound;
    }

    let endpoint = stop_endpoint(id, force);
    let attempt = first_stop_owner(stop_targets(home), |target| target.send_stop(&endpoint));
    stop_outcome(attempt, force)
}

/// Build the stop or kill endpoint for an already validated container id.
fn stop_endpoint(id: &str, force: bool) -> String {
    let endpoint = if force {
        format!("/containers/{id}/kill")
    } else {
        // An explicit grace period keeps the daemon's stop time in line
        // with the transport timeout (`ipc::STOP_TIMEOUT`).
        format!("/containers/{id}/stop?t={}", ipc::STOP_GRACE_SECS)
    };
    debug!(
        "attempting container stop: id={} force={force} endpoint={endpoint}",
        short_container_id(id)
    );
    endpoint
}

/// Reject container IDs that would corrupt the HTTP request line.
///
/// Docker accepts both hex IDs and container names (alphanumeric, hyphens,
/// underscores, dots). This function rejects only characters that could
/// cause path traversal or HTTP header injection: `/`, `?`, `#`, `%`,
/// spaces, and every control character.
fn is_safe_container_id(id: &str) -> bool {
    !id.is_empty()
        && !id
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '?' | '#' | '%' | ' '))
}

/// Map the combined result of a stop request to `StopOutcome`.
///
/// Both "no daemon reachable" and "a daemon received the request but gave
/// no usable reply" are failures: in the second case the container may or
/// may not have been stopped, so it must not be reported as not found.
fn stop_outcome(attempt: ipc::StopAttempt, force: bool) -> StopOutcome {
    match attempt {
        ipc::StopAttempt::Status(status_code) => interpret_stop_status(status_code, force),
        ipc::StopAttempt::NoResponse => {
            debug!("container runtime daemon did not reply to stop request");
            StopOutcome::Failed
        }
        ipc::StopAttempt::Unreachable => {
            debug!("no transport could reach container runtime daemon for stop");
            StopOutcome::Failed
        }
    }
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

/// Send the stop request to each endpoint in order and pick the outcome.
///
/// Each endpoint is tagged with whether it is the `DOCKER_HOST` override.
/// Unreachable endpoints (the stop request was never sent) move on to the
/// next endpoint. A 404 from a default endpoint also moves on: the
/// container may exist on a different daemon (e.g., Podman when Docker
/// returns 404). That 404 is returned only if no other daemon answered, and
/// [`ipc::StopAttempt::Unreachable`] only if no daemon was reached at all.
/// Any reply from the override, including 404, and any other status code
/// from a default endpoint is returned immediately. An endpoint that
/// received the request but gave no usable reply ends the search with
/// [`ipc::StopAttempt::NoResponse`], even after an earlier 404: that daemon
/// may still be stopping the container, and another daemon must not act on
/// a same-named container in the meantime.
fn first_stop_owner<P, I, F>(endpoints: I, mut attempt: F) -> ipc::StopAttempt
where
    I: IntoIterator<Item = (bool, P)>,
    F: FnMut(P) -> ipc::StopAttempt,
{
    let mut result = ipc::StopAttempt::Unreachable;
    for (is_override, endpoint) in endpoints {
        match attempt(endpoint) {
            ipc::StopAttempt::Unreachable => {}
            ipc::StopAttempt::Status(404) if !is_override => {
                result = ipc::StopAttempt::Status(404);
            }
            owned @ (ipc::StopAttempt::NoResponse | ipc::StopAttempt::Status(_)) => return owned,
        }
    }
    result
}

// ── Daemon endpoints ─────────────────────────────────────────────────

/// One daemon endpoint that detection and stop requests can be sent to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonEndpoint {
    /// `DOCKER_HOST` `tcp://` address (`host:port`).
    Tcp(String),
    /// Unix domain socket path.
    #[cfg(unix)]
    Unix(PathBuf),
    /// Windows named pipe path.
    #[cfg(windows)]
    Pipe(String),
}

impl DaemonEndpoint {
    /// Fetch the container list JSON body before `deadline`.
    fn fetch_json(&self, deadline: Instant) -> Option<String> {
        match self {
            Self::Tcp(addr) => ipc::fetch_tcp_json(addr, deadline),
            #[cfg(unix)]
            Self::Unix(path) => ipc::fetch_unix_socket_json(path, deadline),
            #[cfg(windows)]
            Self::Pipe(path) => ipc::fetch_named_pipe_json(path, deadline),
        }
    }

    /// Send a stop or kill request for `endpoint`.
    fn send_stop(&self, endpoint: &str) -> ipc::StopAttempt {
        match self {
            Self::Tcp(addr) => ipc::stop_via_tcp(addr, endpoint),
            #[cfg(unix)]
            Self::Unix(path) => ipc::stop_via_unix_socket(path, endpoint),
            #[cfg(windows)]
            Self::Pipe(path) => ipc::stop_via_named_pipe(path, endpoint),
        }
    }
}

/// The `DOCKER_HOST` `tcp://` endpoint, if configured.
fn docker_host_tcp_endpoint() -> Option<DaemonEndpoint> {
    ipc::docker_host_tcp_addr().map(DaemonEndpoint::Tcp)
}

/// The well-known local daemon sockets for the current user.
#[cfg(unix)]
fn default_local_endpoints(home: Option<PathBuf>) -> impl Iterator<Item = DaemonEndpoint> {
    // Safety: getuid() is a simple syscall with no preconditions.
    let uid = unsafe { libc::getuid() };
    ipc::unix_socket_paths(uid, home)
        .into_iter()
        .map(DaemonEndpoint::Unix)
}

#[cfg(windows)]
const DEFAULT_PIPE_PATHS: &[&str] = &[
    r"\\.\pipe\docker_engine",
    r"\\.\pipe\podman-machine-default",
];

/// The well-known local daemon named pipes.
#[cfg(windows)]
fn default_local_endpoints(_home: Option<PathBuf>) -> impl Iterator<Item = DaemonEndpoint> {
    DEFAULT_PIPE_PATHS
        .iter()
        .map(|path| DaemonEndpoint::Pipe((*path).to_string()))
}

/// The local `DOCKER_HOST` override (`unix://` or `npipe://`), if configured.
#[cfg(unix)]
fn docker_host_local_endpoint() -> Option<DaemonEndpoint> {
    ipc::docker_host_unix_path().map(|path| DaemonEndpoint::Unix(PathBuf::from(path)))
}

#[cfg(windows)]
fn docker_host_local_endpoint() -> Option<DaemonEndpoint> {
    ipc::docker_host_npipe_path().map(DaemonEndpoint::Pipe)
}

/// Whether a local `DOCKER_HOST` override replaces the default endpoints.
///
/// On Unix a `unix://` path replaces the default sockets. On Windows an
/// `npipe://` pipe is tried alongside the default pipes, like `tcp://`.
const LOCAL_OVERRIDE_REPLACES_DEFAULTS: bool = cfg!(unix);

/// Endpoints a stop request is tried against, in order, each tagged with
/// whether it was configured through `DOCKER_HOST`.
///
/// The same endpoints, in the same order, as one detection pass (see
/// [`detection_targets`]), so a stop never reaches a daemon that detection
/// excluded. Any reply from the `DOCKER_HOST` endpoint, including "not
/// found", ends the search (see [`first_stop_owner`]); the defaults are
/// tried only when that endpoint is unreachable.
fn stop_targets(home: Option<PathBuf>) -> Vec<(bool, DaemonEndpoint)> {
    detection_targets(home)
}

/// Endpoints one detection pass queries, in priority order, each tagged with
/// whether it was configured through `DOCKER_HOST`.
///
/// A `tcp://` daemon is queried alongside the local defaults, so a stale
/// address cannot use up the budget the local endpoints need.
fn detection_targets(home: Option<PathBuf>) -> Vec<(bool, DaemonEndpoint)> {
    prioritized_targets(
        docker_host_tcp_endpoint(),
        docker_host_local_endpoint(),
        LOCAL_OVERRIDE_REPLACES_DEFAULTS,
        || default_local_endpoints(home).collect(),
    )
}

/// Order daemon endpoints by priority: the `DOCKER_HOST` endpoint first
/// (tagged `true`), then the defaults in list order (tagged `false`).
///
/// `DOCKER_HOST` holds one scheme, so at most one of `tcp` and
/// `local_override` is set. The defaults are dropped when a local override
/// is set and `override_replaces_defaults` is true; a `tcp://` endpoint
/// never replaces them.
fn prioritized_targets<P>(
    tcp: Option<P>,
    local_override: Option<P>,
    override_replaces_defaults: bool,
    defaults: impl FnOnce() -> Vec<P>,
) -> Vec<(bool, P)> {
    let defaults = if override_replaces_defaults && local_override.is_some() {
        Vec::new()
    } else {
        defaults()
    };
    tcp.or(local_override)
        .map(|endpoint| (true, endpoint))
        .into_iter()
        .chain(defaults.into_iter().map(|endpoint| (false, endpoint)))
        .collect()
}

// ── Daemon queries ───────────────────────────────────────────────────

/// Query every detection target concurrently and keep the bodies to use,
/// highest priority first.
fn query_daemon_bodies(home: Option<PathBuf>) -> Vec<String> {
    collect_daemon_bodies(
        detection_targets(home),
        DaemonEndpoint::fetch_json,
        ipc::query_deadline(),
    )
}

/// Run `fetch` for every tagged target on its own thread under one shared
/// `deadline`, then pick the bodies to use with [`select_daemon_bodies`].
///
/// `targets` must be in priority order. Each response is tagged with its
/// target index, so the result does not depend on arrival order.
fn collect_daemon_bodies<P, F>(targets: Vec<(bool, P)>, fetch: F, deadline: Instant) -> Vec<String>
where
    P: Send + 'static,
    F: Fn(&P, Instant) -> Option<String> + Send + Sync + 'static,
{
    let responses = ipc::fetch_all_successes(
        targets.into_iter().enumerate(),
        move |(priority, (from_docker_host, target))| {
            fetch(&target, deadline).map(|body| (priority, from_docker_host, body))
        },
        deadline,
    );
    select_daemon_bodies(responses)
}

/// Pick the bodies to use from daemon responses tagged with their target
/// index (lower is higher priority) and whether they came from
/// `DOCKER_HOST`, and return them highest priority first.
///
/// A response from the `DOCKER_HOST` endpoint is used on its own. Otherwise
/// every default endpoint that answered is kept so their containers can be
/// merged.
fn select_daemon_bodies(mut responses: Vec<(usize, bool, String)>) -> Vec<String> {
    responses.sort_by_key(|(priority, _, _)| *priority);
    let (overrides, defaults): (Vec<_>, Vec<_>) = responses
        .into_iter()
        .partition(|(_, from_docker_host, _)| *from_docker_host);
    let chosen = if overrides.is_empty() {
        defaults
    } else {
        overrides
    };
    chosen.into_iter().map(|(_, _, body)| body).collect()
}

/// Merge bodies (highest priority first) into one JSON array for strict
/// parsing.
///
/// The bodies are concatenated lowest priority first: when two daemons
/// publish the same key, the later entry wins the map insert, so the
/// higher-priority daemon is kept.
fn merge_prioritized_bodies(bodies: &[String]) -> Option<String> {
    merge_daemon_response_bodies(bodies.iter().rev())
}

/// Merge bodies (highest priority first) into one port map.
///
/// Bodies are merged lowest priority first so that, when two daemons
/// publish the same key, the higher-priority daemon overwrites the other.
fn merge_prioritized_responses(bodies: &[String]) -> Option<ContainerPortMap> {
    merge_daemon_responses(bodies.iter().rev())
}

fn query_daemon_body(home: Option<PathBuf>) -> Option<String> {
    merge_prioritized_bodies(&query_daemon_bodies(home))
}

fn query_daemon(home: Option<PathBuf>) -> Option<ContainerPortMap> {
    merge_prioritized_responses(&query_daemon_bodies(home))
}

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

    // ── merge_daemon_response_bodies ─────────────────────────────────

    #[test]
    fn merge_bodies_empty_iterator_returns_none() {
        let result = merge_daemon_response_bodies::<&str, Vec<&str>>(vec![]);
        assert!(result.is_none(), "no responses means None");
    }

    #[test]
    fn merge_bodies_single_empty_array() {
        let result = merge_daemon_response_bodies(["[]"]);
        assert_eq!(
            result.as_deref(),
            Some("[]"),
            "single empty array should produce []"
        );
    }

    #[test]
    fn merge_bodies_concatenates_non_empty_arrays() {
        let result = merge_daemon_response_bodies([r#"[{"a":1}]"#, r#"[{"b":2},{"c":3}]"#]);
        assert_eq!(
            result.as_deref(),
            Some(r#"[{"a":1},{"b":2},{"c":3}]"#),
            "elements from both arrays should be combined"
        );
    }

    #[test]
    fn merge_bodies_skips_empty_arrays_without_spurious_commas() {
        let result = merge_daemon_response_bodies(["[]", r#"[{"a":1}]"#]);
        assert_eq!(
            result.as_deref(),
            Some(r#"[{"a":1}]"#),
            "empty arrays should not introduce leading commas"
        );
    }

    #[test]
    fn merge_bodies_trailing_empty_array_does_not_add_comma() {
        let result = merge_daemon_response_bodies([r#"[{"a":1}]"#, "[]"]);
        assert_eq!(
            result.as_deref(),
            Some(r#"[{"a":1}]"#),
            "trailing empty array should not add trailing comma"
        );
    }

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

    #[test]
    fn safe_id_rejects_every_control_character() {
        for id in [
            "abc\tdef",
            "abc\0",
            "abc\u{7f}",
            "abc\u{85}",
            "abc\u{1b}[0m",
        ] {
            assert!(
                !is_safe_container_id(id),
                "control character in {id:?} should be rejected"
            );
        }
    }

    #[test]
    fn safe_id_accepts_non_ascii_name() {
        assert!(
            is_safe_container_id("caf\u{e9}-container"),
            "non-ASCII printable characters do not corrupt the request line"
        );
    }

    // ── stop_endpoint ────────────────────────────────────────────────

    /// Logger that enables every level, so `debug!` arguments are evaluated.
    struct EnabledLogger;

    impl log::Log for EnabledLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, _record: &log::Record<'_>) {}

        fn flush(&self) {}
    }

    static ENABLED_LOGGER: EnabledLogger = EnabledLogger;

    #[test]
    fn stop_endpoint_sends_explicit_grace_period() {
        assert_eq!(
            stop_endpoint("abc123", false),
            format!("/containers/abc123/stop?t={}", ipc::STOP_GRACE_SECS),
            "graceful stop must pin the grace period the transport timeout is sized for"
        );
        assert_eq!(
            stop_endpoint("abc123", true),
            "/containers/abc123/kill",
            "kill takes no grace period"
        );
    }

    #[test]
    fn stop_endpoint_handles_multibyte_id_with_debug_logging() {
        // Another test may already have installed a logger; either way the
        // max level below makes `debug!` evaluate its arguments.
        drop(log::set_logger(&ENABLED_LOGGER));
        log::set_max_level(log::LevelFilter::Debug);

        // Byte 12 falls inside the two-byte 'e9' character.
        let id = "aaaaaaaaaaa\u{e9}bc";
        assert!(is_safe_container_id(id), "non-ASCII ids pass validation");
        assert_eq!(
            stop_endpoint(id, false),
            format!("/containers/{id}/stop?t={}", ipc::STOP_GRACE_SECS),
            "a multi-byte id must not panic when logged"
        );
    }

    // ── first_stop_owner ─────────────────────────────────────────────

    /// Tag every attempt as coming from a default (non-override) endpoint.
    fn from_defaults<const N: usize>(
        attempts: [ipc::StopAttempt; N],
    ) -> impl Iterator<Item = (bool, ipc::StopAttempt)> {
        attempts.into_iter().map(|attempt| (false, attempt))
    }

    #[test]
    fn first_stop_owner_skips_unreachable_and_404() {
        let result = first_stop_owner(
            from_defaults([
                ipc::StopAttempt::Unreachable,
                ipc::StopAttempt::Status(404),
                ipc::StopAttempt::Status(204),
            ]),
            |attempt| attempt,
        );
        assert_eq!(
            result,
            ipc::StopAttempt::Status(204),
            "the daemon that owns the container wins"
        );
    }

    #[test]
    fn first_stop_owner_falls_back_to_404() {
        let result = first_stop_owner(
            from_defaults([ipc::StopAttempt::Status(404), ipc::StopAttempt::Unreachable]),
            |attempt| attempt,
        );
        assert_eq!(
            result,
            ipc::StopAttempt::Status(404),
            "404 is reported only after all endpoints"
        );
    }

    #[test]
    fn first_stop_owner_reports_unreachable_when_nothing_answered() {
        let result = first_stop_owner(
            from_defaults([ipc::StopAttempt::Unreachable, ipc::StopAttempt::Unreachable]),
            |attempt| attempt,
        );
        assert_eq!(
            result,
            ipc::StopAttempt::Unreachable,
            "no endpoint was reached"
        );
        assert_eq!(
            stop_outcome(result, false),
            StopOutcome::Failed,
            "an unreachable daemon is a failure"
        );
    }

    #[test]
    fn first_stop_owner_stops_after_no_response() {
        let mut tried = 0;
        let result = first_stop_owner(
            from_defaults([
                ipc::StopAttempt::Status(404),
                ipc::StopAttempt::NoResponse,
                ipc::StopAttempt::Status(204),
            ]),
            |attempt| {
                tried += 1;
                attempt
            },
        );
        assert_eq!(
            result,
            ipc::StopAttempt::NoResponse,
            "a timed-out daemon owns the outcome"
        );
        assert_eq!(tried, 2, "no endpoint after the timed-out one is tried");
    }

    #[test]
    fn stop_after_404_then_silent_daemon_is_failed_not_not_found() {
        // One default daemon answers 404, then the next one receives the
        // request and never replies: the container may have been stopped.
        for force in [false, true] {
            let attempt = first_stop_owner(
                from_defaults([ipc::StopAttempt::Status(404), ipc::StopAttempt::NoResponse]),
                |attempt| attempt,
            );
            assert_eq!(
                stop_outcome(attempt, force),
                StopOutcome::Failed,
                "a daemon that received the stop and went silent must not read as not found"
            );
        }
    }

    #[test]
    fn override_404_is_not_found_without_trying_defaults() {
        let mut tried = 0;
        let attempt = first_stop_owner(
            [
                (true, ipc::StopAttempt::Status(404)),
                (false, ipc::StopAttempt::Status(204)),
            ],
            |attempt| {
                tried += 1;
                attempt
            },
        );
        assert_eq!(
            stop_outcome(attempt, false),
            StopOutcome::NotFound,
            "the DOCKER_HOST daemon's answer is final"
        );
        assert_eq!(
            tried, 1,
            "no default endpoint is tried after the override answered"
        );
    }

    #[test]
    fn unreachable_override_falls_through_to_defaults() {
        let attempt = first_stop_owner(
            [
                (true, ipc::StopAttempt::Unreachable),
                (false, ipc::StopAttempt::Status(204)),
            ],
            |attempt| attempt,
        );
        assert_eq!(
            stop_outcome(attempt, false),
            StopOutcome::Stopped,
            "only an override that never received the stop falls through"
        );
    }

    // ── Target selection ─────────────────────────────────────────────

    fn defaults() -> Vec<&'static str> {
        vec!["default-a", "default-b"]
    }

    #[test]
    fn prioritized_targets_replacing_override_drops_defaults() {
        // DOCKER_HOST=unix://: the user excluded the default sockets.
        assert_eq!(
            prioritized_targets(None, Some("override"), true, defaults),
            vec![(true, "override")],
            "a unix:// override must not chain the default sockets"
        );
    }

    #[test]
    fn prioritized_targets_non_replacing_override_keeps_defaults_after_it() {
        // DOCKER_HOST=npipe://: detection queries the default pipes too.
        assert_eq!(
            prioritized_targets(None, Some("override"), false, defaults),
            vec![
                (true, "override"),
                (false, "default-a"),
                (false, "default-b")
            ]
        );
    }

    #[test]
    fn prioritized_targets_tcp_never_replaces_defaults() {
        assert_eq!(
            prioritized_targets(Some("tcp"), None, true, defaults),
            vec![(true, "tcp"), (false, "default-a"), (false, "default-b")]
        );
    }

    #[test]
    fn prioritized_targets_without_docker_host_uses_defaults() {
        assert_eq!(
            prioritized_targets(None, None, true, defaults),
            vec![(false, "default-a"), (false, "default-b")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_local_override_replaces_defaults() {
        const {
            assert!(LOCAL_OVERRIDE_REPLACES_DEFAULTS);
        }
    }

    // ── Merge priority ───────────────────────────────────────────────

    fn shared_port_body(name: &str) -> String {
        format!(
            r#"[{{"Names": ["/{name}"], "Image": "img", "Ports": [{{"PublicPort": 8080, "Type": "tcp"}}]}}]"#
        )
    }

    #[test]
    fn merge_keeps_higher_priority_daemon_regardless_of_arrival_order() {
        let first = (0, false, shared_port_body("from-first-default"));
        let second = (1, false, shared_port_body("from-second-default"));
        let key = (None, 8080, Protocol::Tcp);

        for responses in [vec![first.clone(), second.clone()], vec![second, first]] {
            let bodies = select_daemon_bodies(responses);

            let lenient = merge_prioritized_responses(&bodies).expect("responses were given");
            assert_eq!(
                lenient.get(&key).map(|info| info.name.as_str()),
                Some("from-first-default"),
                "the earlier default endpoint wins a shared key"
            );

            let merged = merge_prioritized_bodies(&bodies).expect("responses were given");
            let strict = api::parse_containers_json_strict(&merged).expect("valid JSON");
            assert_eq!(
                strict.get(&key).map(|info| info.name.as_str()),
                Some("from-first-default"),
                "the strict path resolves a shared key the same way"
            );
        }
    }

    // ── Detection target selection ───────────────────────────────────

    #[test]
    fn select_daemon_bodies_merges_all_default_endpoints() {
        let bodies = select_daemon_bodies(vec![
            (0, false, "[1]".to_string()),
            (1, false, "[2]".to_string()),
        ]);
        assert_eq!(
            bodies,
            vec!["[1]".to_string(), "[2]".to_string()],
            "every answering default endpoint should contribute"
        );
    }

    #[test]
    fn select_daemon_bodies_prefers_docker_host() {
        let bodies = select_daemon_bodies(vec![
            (1, false, "[1]".to_string()),
            (0, true, "[9]".to_string()),
        ]);
        assert_eq!(
            bodies,
            vec!["[9]".to_string()],
            "an answering DOCKER_HOST endpoint is used on its own"
        );
    }

    /// Stand-in detection target: a real TCP address or a canned local reply.
    enum FakeTarget {
        Tcp(String),
        Local(&'static str),
        Hung,
    }

    fn fetch_fake(target: &FakeTarget, deadline: Instant) -> Option<String> {
        match target {
            FakeTarget::Tcp(addr) => ipc::fetch_tcp_json(addr, deadline),
            FakeTarget::Local(body) => Some((*body).to_string()),
            FakeTarget::Hung => {
                std::thread::sleep(std::time::Duration::from_secs(3));
                None
            }
        }
    }

    #[test]
    fn collect_daemon_bodies_falls_back_to_local_when_tcp_is_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address").to_string();
        drop(listener);

        let started = Instant::now();
        let mut bodies = collect_daemon_bodies(
            vec![
                (true, FakeTarget::Tcp(addr)),
                (false, FakeTarget::Local("[1]")),
                (false, FakeTarget::Local("[2]")),
            ],
            fetch_fake,
            started + std::time::Duration::from_millis(300),
        );
        let elapsed = started.elapsed();
        bodies.sort();

        assert_eq!(
            bodies,
            vec!["[1]".to_string(), "[2]".to_string()],
            "a refused DOCKER_HOST must not hide the local daemons"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "the shared budget bounds the pass, took {elapsed:?}"
        );
    }

    #[test]
    fn collect_daemon_bodies_keeps_local_results_when_tcp_hangs() {
        let started = Instant::now();
        let bodies = collect_daemon_bodies(
            vec![(true, FakeTarget::Hung), (false, FakeTarget::Local("[1]"))],
            fetch_fake,
            started + std::time::Duration::from_millis(200),
        );
        let elapsed = started.elapsed();

        assert_eq!(
            bodies,
            vec!["[1]".to_string()],
            "a blackholed DOCKER_HOST must not starve the local endpoints"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "a hung endpoint must not outlive the budget, took {elapsed:?}"
        );
    }

    #[test]
    fn collect_daemon_bodies_prefers_answering_tcp_daemon() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address").to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match std::io::Read::read(&mut stream, &mut byte) {
                    Ok(1) => request.push(byte[0]),
                    _ => break,
                }
            }
            drop(std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.0 200 OK\r\nContent-Length: 3\r\n\r\n[9]",
            ));
        });

        let bodies = collect_daemon_bodies(
            vec![
                (true, FakeTarget::Tcp(addr)),
                (false, FakeTarget::Local("[1]")),
            ],
            fetch_fake,
            Instant::now() + std::time::Duration::from_secs(5),
        );
        drop(server.join());

        assert_eq!(
            bodies,
            vec!["[9]".to_string()],
            "an answering DOCKER_HOST daemon is used on its own"
        );
    }
}
