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
//! use nanodock::start_detection;
//!
//! let handle = start_detection(None);
//! // ... do other work while detection runs in the background ...
//! let port_map = handle.wait();
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
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Why container detection failed.
///
/// Detection queries every known daemon endpoint at once. It fails only
/// when none of them produced a container list, and then reports the most
/// informative of their failures: a daemon that answered badly
/// ([`InvalidResponse`](Self::InvalidResponse),
/// [`HttpStatus`](Self::HttpStatus)) beats one that refused the connection
/// ([`PermissionDenied`](Self::PermissionDenied)), which beats one that was
/// too slow ([`Timeout`](Self::Timeout)) or failed with another I/O error
/// ([`Io`](Self::Io)), which beats finding no daemon at all
/// ([`DaemonNotFound`](Self::DaemonNotFound)).
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// No container runtime daemon is listening on any known endpoint: no
    /// socket or named pipe exists, or every connection was refused.
    DaemonNotFound,

    /// A daemon endpoint exists but the current user may not connect to it.
    ///
    /// On Linux this usually means the user is not in the `docker` group
    /// (or the socket belongs to another user).
    PermissionDenied {
        /// The endpoint that refused access, such as
        /// `/var/run/docker.sock`, `\\.\pipe\docker_engine`, or
        /// `tcp://host:port`.
        endpoint: String,
    },

    /// No daemon finished answering within the detection timeout.
    Timeout,

    /// A daemon answered with this unexpected (non-2xx) HTTP status.
    HttpStatus(u16),

    /// A daemon answered, but the reply was not a valid HTTP response or
    /// container list.
    InvalidResponse(ParseError),

    /// Another I/O error occurred while talking to the daemon.
    Io(std::io::Error),
}

impl Error {
    /// How much the error tells the caller; when every endpoint fails,
    /// detection reports the error that ranks highest.
    const fn informativeness(&self) -> u8 {
        match self {
            Self::DaemonNotFound => 0,
            Self::Io(_) => 1,
            Self::Timeout => 2,
            Self::PermissionDenied { .. } => 3,
            Self::HttpStatus(_) => 4,
            Self::InvalidResponse(_) => 5,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DaemonNotFound => {
                f.write_str("no container runtime daemon found on any known endpoint")
            }
            Self::PermissionDenied { endpoint } => write!(
                f,
                "permission denied connecting to the container runtime at {endpoint}"
            ),
            Self::Timeout => f.write_str("the container runtime daemon did not answer in time"),
            Self::HttpStatus(status) => write!(
                f,
                "the container runtime daemon answered with HTTP status {status}"
            ),
            Self::InvalidResponse(_) => {
                f.write_str("the container runtime daemon sent an invalid response")
            }
            Self::Io(_) => f.write_str("I/O error talking to the container runtime daemon"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidResponse(source) => Some(source),
            Self::Io(source) => Some(source),
            Self::DaemonNotFound
            | Self::PermissionDenied { .. }
            | Self::Timeout
            | Self::HttpStatus(_) => None,
        }
    }
}

impl From<ParseError> for Error {
    fn from(error: ParseError) -> Self {
        Self::InvalidResponse(error)
    }
}

/// Pick the most informative of several endpoint failures, keeping the
/// earliest (highest priority) one on a tie.
fn most_informative(errors: impl IntoIterator<Item = Error>) -> Error {
    let mut best: Option<Error> = None;
    for error in errors {
        if best
            .as_ref()
            .is_none_or(|best| error.informativeness() > best.informativeness())
        {
            best = Some(error);
        }
    }
    best.unwrap_or(Error::DaemonNotFound)
}

/// A daemon reply that could not be parsed: malformed HTTP framing or a
/// container list that is not valid JSON.
///
/// The type is opaque so the JSON parser behind it stays an implementation
/// detail; its [`Display`](std::fmt::Display) output describes the problem.
#[derive(Debug)]
pub struct ParseError(ParseErrorKind);

#[derive(Debug)]
enum ParseErrorKind {
    Json(serde_json::Error),
    Http(&'static str),
}

impl ParseError {
    /// The container list is not valid JSON.
    const fn json(error: serde_json::Error) -> Self {
        Self(ParseErrorKind::Json(error))
    }

    /// The HTTP reply is malformed.
    const fn http(reason: &'static str) -> Self {
        Self(ParseErrorKind::Http(reason))
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            ParseErrorKind::Json(error) => write!(f, "invalid container list JSON: {error}"),
            ParseErrorKind::Http(reason) => write!(f, "malformed HTTP response: {reason}"),
        }
    }
}

impl std::error::Error for ParseError {}

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
///
/// The fields are public for reading. The struct is `#[non_exhaustive]`, so
/// code outside this crate builds one with [`ContainerInfo::new`] and the
/// `with_*` methods instead of a struct literal, which lets later releases
/// add fields without a breaking change.
///
/// ```
/// use nanodock::ContainerInfo;
///
/// let info = ContainerInfo::new("abc123", "shop-db-1", "postgres:16")
///     .with_compose_project("shop")
///     .with_compose_service("db");
/// assert_eq!(info.name, "shop-db-1");
/// assert_eq!(info.compose_project.as_deref(), Some("shop"));
/// assert_eq!(info.compose_service.as_deref(), Some("db"));
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerInfo {
    /// Full container ID (hex string) for API calls, empty when unavailable.
    pub id: String,
    /// Container name (e.g. "backend-postgres-1").
    pub name: String,
    /// Container image (e.g. "postgres:16").
    pub image: String,
    /// Compose project the container belongs to, read from the
    /// `com.docker.compose.project` label (or `io.podman.compose.project`
    /// when only that one is set). `None` when the container was not
    /// started by Docker Compose or `podman-compose`.
    #[serde(default)]
    pub compose_project: Option<String>,
    /// Compose service name, read from the `com.docker.compose.service`
    /// label. `None` when the label is absent.
    #[serde(default)]
    pub compose_service: Option<String>,
}

impl ContainerInfo {
    /// Create container metadata with no Compose labels.
    ///
    /// `id` is the full container ID (empty when unknown), `name` the
    /// container name without a leading `/`, and `image` the image
    /// reference.
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            image: image.into(),
            compose_project: None,
            compose_service: None,
        }
    }

    /// Set the Compose project name.
    #[must_use]
    pub fn with_compose_project(mut self, project: impl Into<String>) -> Self {
        self.compose_project = Some(project.into());
        self
    }

    /// Set the Compose service name.
    #[must_use]
    pub fn with_compose_service(mut self, service: impl Into<String>) -> Self {
        self.compose_service = Some(service.into());
        self
    }
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

/// Key of one published binding: host IP (`None` for every interface),
/// host port, and protocol.
type PortKey = (Option<IpAddr>, u16, Protocol);

/// Published container ports: maps `(host_ip, host_port, protocol)` to the
/// container that publishes it.
///
/// A host IP of `None` is a wildcard binding (`0.0.0.0`, `::`, or no
/// address). Every binding of one container shares a single
/// [`ContainerInfo`] through an [`Arc`], so a container that publishes a
/// large port range costs one pointer per port rather than one copy.
///
/// ```
/// use std::net::{IpAddr, Ipv4Addr};
/// use nanodock::{ContainerInfo, ContainerPortMap, Protocol, ProxyFallback};
///
/// let mut map = ContainerPortMap::default();
/// map.insert(None, 5432, Protocol::Tcp, ContainerInfo::new("abc", "db", "postgres:16"));
///
/// assert_eq!(map.len(), 1);
/// assert_eq!(map.get(None, 5432, Protocol::Tcp).map(|info| info.name.as_str()), Some("db"));
///
/// // A socket on any address matches the wildcard binding.
/// let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// let found = map.lookup(localhost, 5432, Protocol::Tcp, ProxyFallback::Deny);
/// assert_eq!(found.container().map(|info| info.name.as_str()), Some("db"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerPortMap {
    bindings: HashMap<PortKey, Arc<ContainerInfo>>,
}

impl ContainerPortMap {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of published bindings (one per host IP, port, and protocol).
    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether no binding is published.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// The container published on exactly this binding.
    ///
    /// This is an exact key lookup: `host_ip` of `None` finds only wildcard
    /// bindings. Use [`ContainerPortMap::lookup`] to match a local socket
    /// address, which also falls back to wildcard bindings.
    #[must_use]
    pub fn get(
        &self,
        host_ip: Option<IpAddr>,
        port: u16,
        proto: Protocol,
    ) -> Option<&ContainerInfo> {
        self.bindings.get(&(host_ip, port, proto)).map(Arc::as_ref)
    }

    /// Record that `info` publishes `(host_ip, port, proto)`, returning the
    /// container previously recorded for that binding, if any.
    ///
    /// Pass an `Arc<ContainerInfo>` to share one container between several
    /// bindings, or a plain [`ContainerInfo`].
    pub fn insert(
        &mut self,
        host_ip: Option<IpAddr>,
        port: u16,
        proto: Protocol,
        info: impl Into<Arc<ContainerInfo>>,
    ) -> Option<Arc<ContainerInfo>> {
        self.bindings.insert((host_ip, port, proto), info.into())
    }

    /// Iterate over every binding in arbitrary order.
    #[must_use]
    pub fn iter(&self) -> PortMapIter<'_> {
        PortMapIter {
            inner: self.bindings.iter(),
        }
    }

    /// Match a local socket address against the published bindings.
    ///
    /// Exact `(ip, port, proto)` matches win first. If the daemon reported an
    /// unspecified host IP (stored as `None`), the wildcard binding is used
    /// next. For known proxy or helper processes (`docker-proxy`,
    /// `rootlessport`, and similar), [`ProxyFallback::Allow`] also accepts a
    /// unique `(port, proto)` match when the proxy's socket address does not
    /// line up with the published host IP; more than one distinct container
    /// on that port and protocol is [`PublishedContainerMatch::Ambiguous`].
    #[must_use]
    pub fn lookup(
        &self,
        ip: IpAddr,
        port: u16,
        proto: Protocol,
        fallback: ProxyFallback,
    ) -> PublishedContainerMatch<'_> {
        if let Some(container) = self.get(Some(ip), port, proto) {
            return PublishedContainerMatch::Match(container);
        }

        if let Some(container) = self.get(None, port, proto) {
            return PublishedContainerMatch::Match(container);
        }

        match fallback {
            ProxyFallback::Allow => self.unique_published_container(port, proto),
            ProxyFallback::Deny => PublishedContainerMatch::NotFound,
        }
    }

    fn unique_published_container(
        &self,
        port: u16,
        proto: Protocol,
    ) -> PublishedContainerMatch<'_> {
        let mut matches = self
            .bindings
            .iter()
            .filter(|((_, candidate_port, candidate_proto), _)| {
                *candidate_port == port && *candidate_proto == proto
            })
            .map(|(_, container)| container);

        let Some(first) = matches.next() else {
            return PublishedContainerMatch::NotFound;
        };

        if matches.all(|candidate| Arc::ptr_eq(candidate, first) || candidate == first) {
            PublishedContainerMatch::Match(first)
        } else {
            PublishedContainerMatch::Ambiguous
        }
    }

    /// Add every binding of `other`, replacing bindings with the same key.
    fn merge(&mut self, other: Self) {
        self.bindings.extend(other.bindings);
    }
}

impl<'a> IntoIterator for &'a ContainerPortMap {
    type Item = ((Option<IpAddr>, u16, Protocol), &'a ContainerInfo);
    type IntoIter = PortMapIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<I: Into<Arc<ContainerInfo>>> FromIterator<((Option<IpAddr>, u16, Protocol), I)>
    for ContainerPortMap
{
    fn from_iter<T: IntoIterator<Item = ((Option<IpAddr>, u16, Protocol), I)>>(iter: T) -> Self {
        let mut map = Self::new();
        map.extend(iter);
        map
    }
}

impl<I: Into<Arc<ContainerInfo>>> Extend<((Option<IpAddr>, u16, Protocol), I)>
    for ContainerPortMap
{
    fn extend<T: IntoIterator<Item = ((Option<IpAddr>, u16, Protocol), I)>>(&mut self, iter: T) {
        self.bindings
            .extend(iter.into_iter().map(|(key, info)| (key, info.into())));
    }
}

/// Iterator over the bindings of a [`ContainerPortMap`], created by
/// [`ContainerPortMap::iter`].
///
/// Yields `((host_ip, port, protocol), container)` in arbitrary order.
#[derive(Debug, Clone)]
pub struct PortMapIter<'a> {
    inner: std::collections::hash_map::Iter<'a, PortKey, Arc<ContainerInfo>>,
}

impl<'a> Iterator for PortMapIter<'a> {
    type Item = ((Option<IpAddr>, u16, Protocol), &'a ContainerInfo);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(key, info)| (*key, info.as_ref()))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for PortMapIter<'_> {}

impl std::iter::FusedIterator for PortMapIter<'_> {}

/// Whether [`ContainerPortMap::lookup`] may fall back to matching on port
/// and protocol alone.
///
/// Docker's `docker-proxy` and Podman's `rootlessport` hold the host socket
/// for a published port, and the address they listen on does not always
/// equal the host IP the daemon reports. Allow the fallback only for such
/// processes: for any other process a port-only match would attribute an
/// unrelated listener to a container.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ProxyFallback {
    /// Accept a unique container on the same port and protocol when no
    /// address-level binding matches.
    Allow,
    /// Match on the address-level bindings only.
    #[default]
    Deny,
}

#[cfg(test)]
fn test_container_info(id: &str, name: &str, image: &str) -> ContainerInfo {
    ContainerInfo::new(id, name, image)
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
    map.insert(host_ip, port, proto, test_container_info(id, name, image));
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

impl<'a> PublishedContainerMatch<'a> {
    /// The matched container, or `None` for [`NotFound`](Self::NotFound) and
    /// [`Ambiguous`](Self::Ambiguous).
    #[must_use]
    pub const fn container(self) -> Option<&'a ContainerInfo> {
        match self {
            Self::Match(info) => Some(info),
            Self::NotFound | Self::Ambiguous => None,
        }
    }
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

// ── Client ───────────────────────────────────────────────────────────

/// Detection timeout of [`Client::new`] and the free functions.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Longest accepted detection timeout. [`Client::timeout`] caps longer
/// values so deadline arithmetic cannot overflow.
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Largest part of the timeout kept back from the daemon queries for JSON
/// parsing and the hand-off to the waiting thread.
const MAX_HANDOFF_MARGIN: Duration = Duration::from_millis(500);

/// Budget for querying the daemons within a detection `timeout`.
///
/// Strictly shorter than any nonzero `timeout`, so the detection thread
/// always delivers what it collected before the [`DetectionHandle`] stops
/// waiting. The margin is a sixth of the timeout, at most 500 ms: the
/// default 3 second timeout queries the daemons for 2.5 seconds.
fn query_budget(timeout: Duration) -> Duration {
    let margin = (timeout / 6).clamp(Duration::from_nanos(1), MAX_HANDOFF_MARGIN);
    timeout.saturating_sub(margin)
}

/// Settings for talking to the Docker/Podman daemons, and the entry point
/// for detection and stop requests.
///
/// [`Client::new`] reads the environment: the `DOCKER_HOST` variable and
/// the user's home directory. The setters replace those values and the
/// detection timeout; each takes and returns the client, so they chain.
/// A client is cheap to clone and can be shared between threads.
///
/// ```no_run
/// use std::time::Duration;
/// use nanodock::Client;
///
/// let client = Client::new().timeout(Duration::from_secs(1));
/// match client.detect() {
///     Ok(port_map) => println!("{} published ports", port_map.len()),
///     Err(error) => eprintln!("detection failed: {error}"),
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    home: Option<PathBuf>,
    timeout: Duration,
    docker_host: Option<String>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// Create a client from the environment.
    ///
    /// The daemon override comes from the `DOCKER_HOST` variable (ignored
    /// when unset, empty, or not valid UTF-8), the home directory from
    /// [`std::env::home_dir`], and the detection timeout is 3 seconds.
    #[must_use]
    pub fn new() -> Self {
        Self {
            home: std::env::home_dir(),
            timeout: DEFAULT_TIMEOUT,
            docker_host: std::env::var("DOCKER_HOST")
                .ok()
                .filter(|value| !value.is_empty()),
        }
    }

    /// Set the home directory used on Unix to find per-user sockets, such
    /// as Docker Desktop's `~/.docker/desktop/docker.sock`.
    ///
    /// With `None` only the system-wide and `/run/user/{uid}` sockets are
    /// tried. Windows ignores the home directory.
    #[must_use]
    pub fn home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    /// Set the detection timeout (3 seconds by default).
    ///
    /// [`Client::detect`] returns within about this long, and
    /// [`DetectionHandle`] waits until this long after
    /// [`Client::start_detection`] was called. The daemons are queried
    /// under a slightly shorter budget so their answers are always handed
    /// over in time. Values above 24 hours are capped. Stop requests are not
    /// affected: they allow for the daemon's 10 second grace period.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.min(MAX_TIMEOUT);
        self
    }

    /// Set the daemon override in `DOCKER_HOST` syntax, replacing the
    /// environment variable.
    ///
    /// `tcp://host:port` is queried alongside the default local endpoints
    /// and wins when it answers. `unix:///path` (Unix) replaces the default
    /// sockets. `npipe:////./pipe/name` (Windows) is queried alongside the
    /// default pipes and wins when it answers. `None`, or a value with
    /// another scheme, uses only the default endpoints.
    #[must_use]
    pub fn docker_host(mut self, docker_host: Option<String>) -> Self {
        self.docker_host = docker_host;
        self
    }

    /// Synchronously detect Docker/Podman containers and their published
    /// ports.
    ///
    /// Queries all known daemon endpoints concurrently under one shared
    /// time budget. A `DOCKER_HOST` `tcp://` daemon is queried alongside
    /// the local Unix sockets or Windows named pipes and is used on its own
    /// when it answers; otherwise the containers of all answering local
    /// daemons are merged.
    ///
    /// This blocks the calling thread and surfaces errors, so the caller
    /// can tell "no containers running" (an empty map) from "no daemon"
    /// ([`Error::DaemonNotFound`]) or "not allowed"
    /// ([`Error::PermissionDenied`]).
    ///
    /// # Errors
    ///
    /// Fails only when no endpoint produced a container list, with the
    /// most informative endpoint failure (see [`Error`]), or with
    /// [`Error::InvalidResponse`] when the merged list is not valid JSON.
    pub fn detect(&self) -> Result<ContainerPortMap, Error> {
        debug!("starting synchronous container runtime detection");
        let bodies = self.query_daemon_bodies(Instant::now())?;
        let body = merge_prioritized_bodies(&bodies).ok_or(Error::DaemonNotFound)?;
        let map = api::parse_containers_json_strict(&body)?;
        debug!(
            "finished synchronous container runtime detection: port_mappings={}",
            map.len()
        );
        Ok(map)
    }

    /// Start detection on a background thread and return at once.
    ///
    /// Other work (socket enumeration, process lookups) can run while the
    /// daemons are queried; collect the result from the returned
    /// [`DetectionHandle`]. Detection parses each daemon's list leniently,
    /// skipping malformed container entries instead of failing.
    #[must_use]
    pub fn start_detection(&self) -> DetectionHandle {
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let client = self.clone();
        debug!("starting container runtime detection");
        std::thread::spawn(move || {
            let result = client.query_daemon(started);
            match &result {
                Ok(map) => debug!(
                    "finished container runtime detection: port_mappings={}",
                    map.len()
                ),
                Err(error) => debug!("container runtime detection failed: {error}"),
            }
            // Ignore send error: receiver may have timed out and been dropped.
            drop(tx.send(result));
        });
        DetectionHandle {
            receiver: rx,
            deadline: started + self.timeout,
        }
    }

    /// Stop or kill a running container through the daemon API.
    ///
    /// When `force` is false, sends `POST /containers/{id}/stop?t=10`
    /// (SIGTERM, then SIGKILL after 10 seconds). When `force` is true,
    /// sends `POST /containers/{id}/kill` (immediate SIGKILL).
    ///
    /// The `id` can be a container ID (hex) or a container name. Characters
    /// that would corrupt the HTTP request path (`/`, `?`, `#`, `%`, control
    /// characters, spaces) are rejected early with [`StopOutcome::NotFound`].
    ///
    /// Tries the same daemons as detection, in priority order
    /// (`DOCKER_HOST` first, then the platform defaults; a `unix://`
    /// `DOCKER_HOST` replaces the default sockets). Before the stop request
    /// is sent to a daemon, it must answer `GET /_ping` on a separate
    /// connection; a daemon that cannot be reached or does not answer the
    /// ping (for example a forwarder whose backend is down) is skipped
    /// without receiving the stop.
    ///
    /// Any reply from the `DOCKER_HOST` daemon, including "not found", is
    /// the result. A "not found" from a default daemon moves on to the next
    /// one, and the first other reply is the result. Once a daemon has
    /// received the stop request, a closed or reset connection, a timeout,
    /// or a partial or malformed reply ends the search with
    /// [`StopOutcome::NoResponse`], even when an earlier daemon answered
    /// "not found": that daemon may still be stopping the container, so no
    /// other daemon is tried. When no daemon could be contacted at all the
    /// result is [`StopOutcome::Unreachable`], and an unexpected HTTP status
    /// is [`StopOutcome::Rejected`].
    ///
    /// The detection timeout does not apply: a stop allows for the grace
    /// period plus a margin (20 seconds overall).
    #[must_use]
    pub fn stop(&self, id: &str, force: bool) -> StopOutcome {
        if !is_safe_container_id(id) {
            debug!("rejected container id with unsafe characters");
            return StopOutcome::NotFound;
        }

        let endpoint = stop_endpoint(id, force);
        let attempt = first_stop_owner(self.stop_targets(), |target| target.send_stop(&endpoint));
        stop_outcome(attempt, force)
    }
}

/// Handle for a container detection running on a background thread.
///
/// Created by [`Client::start_detection`] or [`start_detection`]. The
/// channel inside is private so the detection mechanism can change without
/// breaking the public API.
#[derive(Debug)]
pub struct DetectionHandle {
    receiver: std::sync::mpsc::Receiver<Result<ContainerPortMap, Error>>,
    /// When the waiting side gives up: the start plus the client timeout.
    deadline: Instant,
}

impl DetectionHandle {
    /// Wait for the detection to finish and return the published ports.
    ///
    /// Blocks until the result arrives or the client's timeout (3 seconds
    /// by default) has passed since the detection started. Never fails:
    /// when no daemon answered, or the timeout passed, the map is empty.
    /// This suits enrichment, where a missing daemon is not an error; use
    /// [`DetectionHandle::wait_result`] to learn why the map is empty.
    ///
    /// ```no_run
    /// let handle = nanodock::start_detection(None);
    /// // ... do other work while detection runs ...
    /// let port_map = handle.wait();
    /// println!("{} published ports", port_map.len());
    /// ```
    #[must_use]
    pub fn wait(self) -> ContainerPortMap {
        self.wait_result().unwrap_or_default()
    }

    /// Wait like [`DetectionHandle::wait`], but report why detection failed.
    ///
    /// # Errors
    ///
    /// Fails with the most informative endpoint failure when no daemon
    /// produced a container list (see [`Error`]), or with
    /// [`Error::Timeout`] when the client's timeout passed first.
    pub fn wait_result(self) -> Result<ContainerPortMap, Error> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        match self.receiver.recv_timeout(remaining) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                debug!("container runtime detection timed out");
                Err(Error::Timeout)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                debug!("container runtime detection channel disconnected");
                Err(Error::Io(std::io::Error::other(
                    "the detection thread stopped without a result",
                )))
            }
        }
    }
}

// ── Convenience functions ────────────────────────────────────────────

/// Synchronously detect containers with a default [`Client`] whose home
/// directory is `home`.
///
/// Shorthand for `Client::new().home(home).detect()`; see
/// [`Client::detect`]. The `home` directory is used on Unix to find
/// per-user sockets such as Docker Desktop's.
///
/// # Errors
///
/// See [`Client::detect`].
pub fn detect_containers(home: Option<PathBuf>) -> Result<ContainerPortMap, Error> {
    Client::new().home(home).detect()
}

/// Start detection on a background thread with a default [`Client`] whose
/// home directory is `home`.
///
/// Shorthand for `Client::new().home(home).start_detection()`; see
/// [`Client::start_detection`].
#[must_use]
pub fn start_detection(home: Option<PathBuf>) -> DetectionHandle {
    Client::new().home(home).start_detection()
}

/// Stop or kill a container with a default [`Client`] whose home directory
/// is `home`.
///
/// Shorthand for `Client::new().home(home).stop(id, force)`; see
/// [`Client::stop`].
#[must_use]
pub fn stop_container(id: &str, force: bool, home: Option<PathBuf>) -> StopOutcome {
    Client::new().home(home).stop(id, force)
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
    /// Container was not found (HTTP 404), or the id contains characters
    /// that cannot name a container.
    NotFound,
    /// No daemon could be contacted, so no daemon received the request and
    /// the container was not touched.
    Unreachable,
    /// A daemon received the request but gave no usable reply: the
    /// connection closed, the request timed out, or the reply was partial
    /// or malformed. The container may or may not be stopping.
    NoResponse,
    /// The daemon answered with an unexpected HTTP status, such as 500.
    Rejected {
        /// The HTTP status code of the reply.
        status: u16,
    },
}

impl StopOutcome {
    /// Whether the container is known to be stopped now: it was stopped or
    /// already was.
    #[must_use]
    pub const fn is_stopped(self) -> bool {
        matches!(self, Self::Stopped | Self::AlreadyStopped)
    }
}

impl std::fmt::Display for StopOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("stopped"),
            Self::AlreadyStopped => f.write_str("already stopped"),
            Self::NotFound => f.write_str("not found"),
            Self::Unreachable => f.write_str("no container runtime daemon could be reached"),
            Self::NoResponse => f.write_str("the daemon gave no reply, the result is unknown"),
            Self::Rejected { status } => {
                write!(f, "rejected by the daemon with HTTP status {status}")
            }
        }
    }
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
/// "A daemon received the request but gave no usable reply" is
/// [`StopOutcome::NoResponse`], never "not found": the container may or may
/// not have been stopped.
fn stop_outcome(attempt: ipc::StopAttempt, force: bool) -> StopOutcome {
    match attempt {
        ipc::StopAttempt::Status(status_code) => interpret_stop_status(status_code, force),
        ipc::StopAttempt::NoResponse => {
            debug!("container runtime daemon did not reply to stop request");
            StopOutcome::NoResponse
        }
        ipc::StopAttempt::Unreachable => {
            debug!("no transport could reach container runtime daemon for stop");
            StopOutcome::Unreachable
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
        status => {
            debug!("unexpected status code from container stop endpoint: {status}");
            StopOutcome::Rejected { status }
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
    fn fetch_json(&self, deadline: Instant) -> Result<String, Error> {
        let result = match self {
            Self::Tcp(addr) => ipc::fetch_tcp_json(addr, deadline),
            #[cfg(unix)]
            Self::Unix(path) => ipc::fetch_unix_socket_json(path, deadline),
            #[cfg(windows)]
            Self::Pipe(path) => ipc::fetch_named_pipe_json(path, deadline),
        };
        result.map_err(|error| self.error(error))
    }

    /// Turn a transport failure at this endpoint into a public [`Error`].
    fn error(&self, error: ipc::FetchError) -> Error {
        match error {
            ipc::FetchError::NotFound => Error::DaemonNotFound,
            ipc::FetchError::PermissionDenied => Error::PermissionDenied {
                endpoint: self.to_string(),
            },
            ipc::FetchError::Timeout => Error::Timeout,
            ipc::FetchError::Status(status) => Error::HttpStatus(status),
            ipc::FetchError::Malformed(reason) => Error::InvalidResponse(ParseError::http(reason)),
            ipc::FetchError::Io(error) => Error::Io(error),
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

impl std::fmt::Display for DaemonEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(addr) => write!(f, "tcp://{addr}"),
            #[cfg(unix)]
            Self::Unix(path) => write!(f, "{}", path.display()),
            #[cfg(windows)]
            Self::Pipe(path) => f.write_str(path),
        }
    }
}

/// The `tcp://` endpoint of a `DOCKER_HOST` value, if it has one.
fn docker_host_tcp_endpoint(docker_host: Option<&str>) -> Option<DaemonEndpoint> {
    docker_host
        .and_then(ipc::docker_host_tcp_addr)
        .map(DaemonEndpoint::Tcp)
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

/// The local endpoint (`unix://` or `npipe://`) of a `DOCKER_HOST` value,
/// if it has one.
#[cfg(unix)]
fn docker_host_local_endpoint(docker_host: Option<&str>) -> Option<DaemonEndpoint> {
    docker_host
        .and_then(ipc::docker_host_unix_path)
        .map(|path| DaemonEndpoint::Unix(PathBuf::from(path)))
}

#[cfg(windows)]
fn docker_host_local_endpoint(docker_host: Option<&str>) -> Option<DaemonEndpoint> {
    docker_host
        .and_then(ipc::docker_host_npipe_path)
        .map(DaemonEndpoint::Pipe)
}

/// Whether a local `DOCKER_HOST` override replaces the default endpoints.
///
/// On Unix a `unix://` path replaces the default sockets. On Windows an
/// `npipe://` pipe is tried alongside the default pipes, like `tcp://`.
const LOCAL_OVERRIDE_REPLACES_DEFAULTS: bool = cfg!(unix);

impl Client {
    /// Endpoints a stop request is tried against, in order, each tagged
    /// with whether it was configured through `DOCKER_HOST`.
    ///
    /// The same endpoints, in the same order, as one detection pass (see
    /// [`Client::detection_targets`]), so a stop never reaches a daemon that
    /// detection excluded. Any reply from the `DOCKER_HOST` endpoint,
    /// including "not found", ends the search (see [`first_stop_owner`]);
    /// the defaults are tried only when that endpoint is unreachable.
    fn stop_targets(&self) -> Vec<(bool, DaemonEndpoint)> {
        self.detection_targets()
    }

    /// Endpoints one detection pass queries, in priority order, each tagged
    /// with whether it was configured through `DOCKER_HOST`.
    ///
    /// A `tcp://` daemon is queried alongside the local defaults, so a stale
    /// address cannot use up the budget the local endpoints need.
    fn detection_targets(&self) -> Vec<(bool, DaemonEndpoint)> {
        let docker_host = self.docker_host.as_deref();
        prioritized_targets(
            docker_host_tcp_endpoint(docker_host),
            docker_host_local_endpoint(docker_host),
            LOCAL_OVERRIDE_REPLACES_DEFAULTS,
            || default_local_endpoints(self.home.clone()).collect(),
        )
    }

    /// Query every detection target concurrently and keep the bodies to
    /// use, highest priority first. The query budget runs from `started`.
    fn query_daemon_bodies(&self, started: Instant) -> Result<Vec<String>, Error> {
        collect_daemon_bodies(
            self.detection_targets(),
            DaemonEndpoint::fetch_json,
            started + query_budget(self.timeout),
        )
    }

    /// One lenient detection pass whose budget runs from `started`.
    fn query_daemon(&self, started: Instant) -> Result<ContainerPortMap, Error> {
        merge_prioritized_responses(&self.query_daemon_bodies(started)?)
            .ok_or(Error::DaemonNotFound)
    }
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

/// Run `fetch` for every tagged target on its own thread under one shared
/// `deadline`, then pick the bodies to use with [`select_daemon_bodies`].
///
/// `targets` must be in priority order. Each response is tagged with its
/// target index, so the result does not depend on arrival order. When no
/// target produced a body, the most informative failure is returned; a
/// target still running at the deadline counts as [`Error::Timeout`].
fn collect_daemon_bodies<P, F>(
    targets: Vec<(bool, P)>,
    fetch: F,
    deadline: Instant,
) -> Result<Vec<String>, Error>
where
    P: Send + 'static,
    F: Fn(&P, Instant) -> Result<String, Error> + Send + Sync + 'static,
{
    let fan_out = ipc::fetch_all(
        targets.into_iter().enumerate(),
        move |(priority, (from_docker_host, target))| {
            (priority, from_docker_host, fetch(&target, deadline))
        },
        deadline,
    );

    let mut responses = Vec::new();
    let mut failures = Vec::new();
    for (priority, from_docker_host, result) in fan_out.results {
        match result {
            Ok(body) => responses.push((priority, from_docker_host, body)),
            Err(error) => failures.push((priority, error)),
        }
    }

    if responses.is_empty() {
        failures.sort_by_key(|(priority, _)| *priority);
        let timed_out = (fan_out.unfinished > 0).then_some(Error::Timeout);
        return Err(most_informative(
            failures
                .into_iter()
                .map(|(_, error)| error)
                .chain(timed_out),
        ));
    }
    Ok(select_daemon_bodies(responses))
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
        merged.merge(api::parse_containers_json(response.as_ref()));
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
            .get(None, 5432, Protocol::Tcp)
            .expect("podman/docker ports should survive multi-daemon merging");
        assert_eq!(container.name, "backend-postgres-1");
        assert_eq!(container.image, "postgres:16");
    }

    #[test]
    fn lookup_keeps_protocol_bindings_separate() {
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

        let tcp = map.lookup(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            53,
            Protocol::Tcp,
            ProxyFallback::Deny,
        );
        let udp = map.lookup(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            53,
            Protocol::Udp,
            ProxyFallback::Deny,
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
    fn lookup_marks_ambiguous_proxy_matches() {
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

        let result = map.lookup(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            8080,
            Protocol::Tcp,
            ProxyFallback::Allow,
        );

        assert_eq!(result, PublishedContainerMatch::Ambiguous);
    }

    #[test]
    fn lookup_uses_normalized_wildcard_bindings() {
        let map = api::parse_containers_json(
            r#"[{
                "Names": ["/postgres"],
                "Image": "postgres:16",
                "Ports": [{"IP": "0.0.0.0", "PrivatePort": 5432, "PublicPort": 5432, "Type": "tcp"}]
            }]"#,
        );

        let result = map.lookup(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            5432,
            Protocol::Tcp,
            ProxyFallback::Deny,
        );

        assert!(matches!(
            result,
            PublishedContainerMatch::Match(info) if info.name == "postgres"
        ));
    }

    #[test]
    fn port_range_bindings_share_one_container() {
        let map = api::parse_containers_json(
            r#"[{"Names": ["/range"], "Ports": [{"host_port": 4000, "range": 100, "protocol": "tcp"}]}]"#,
        );
        assert_eq!(map.len(), 100, "every port in the range is mapped");
        let first = map
            .bindings
            .get(&(None, 4000, Protocol::Tcp))
            .expect("first port");
        let last = map
            .bindings
            .get(&(None, 4099, Protocol::Tcp))
            .expect("last port");
        assert!(
            Arc::ptr_eq(first, last),
            "a port range must not clone the container per port"
        );
    }

    #[test]
    fn port_map_collects_and_iterates_bindings() {
        let map: ContainerPortMap = [
            (
                (None, 80, Protocol::Tcp),
                test_container_info("a", "web", "nginx"),
            ),
            (
                (Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), 53, Protocol::Udp),
                test_container_info("b", "dns", "bind9"),
            ),
        ]
        .into_iter()
        .collect();

        assert_eq!(map.len(), 2);
        assert!(!map.is_empty());
        let mut names: Vec<_> = map.iter().map(|(_, info)| info.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["dns", "web"]);
        assert_eq!(map.iter().len(), 2, "the iterator knows its length");
        assert!(
            map.get(None, 53, Protocol::Udp).is_none(),
            "get is an exact key lookup"
        );
        assert!(ContainerPortMap::default().is_empty());
    }

    #[test]
    fn insert_returns_the_replaced_container() {
        let mut map = ContainerPortMap::new();
        assert!(
            map.insert(
                None,
                80,
                Protocol::Tcp,
                test_container_info("a", "old", "img")
            )
            .is_none()
        );
        let previous = map.insert(
            None,
            80,
            Protocol::Tcp,
            test_container_info("b", "new", "img"),
        );
        assert_eq!(
            previous.map(|info| info.name.clone()).as_deref(),
            Some("old")
        );
        assert_eq!(
            map.get(None, 80, Protocol::Tcp)
                .map(|info| info.name.as_str()),
            Some("new")
        );
    }

    #[test]
    fn proxy_fallback_finds_unique_container_on_other_address() {
        let mut map = ContainerPortMap::new();
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            8080,
            Protocol::Tcp,
            "api",
            "api",
            "node:22",
        );
        let unspecified = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

        assert_eq!(
            map.lookup(unspecified, 8080, Protocol::Tcp, ProxyFallback::Deny),
            PublishedContainerMatch::NotFound,
            "without the fallback only address-level bindings match"
        );
        let found = map.lookup(unspecified, 8080, Protocol::Tcp, ProxyFallback::Allow);
        assert_eq!(
            found.container().map(|info| info.name.as_str()),
            Some("api")
        );
        assert_eq!(ProxyFallback::default(), ProxyFallback::Deny);
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
    fn interpret_stop_status_409_on_graceful_is_rejected() {
        assert_eq!(
            interpret_stop_status(409, false),
            StopOutcome::Rejected { status: 409 },
            "409 on non-force is unexpected and should be reported with its status"
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
    fn interpret_stop_status_500_is_rejected() {
        assert_eq!(
            interpret_stop_status(500, false),
            StopOutcome::Rejected { status: 500 },
            "a server error is reported with its status"
        );
    }

    #[test]
    fn stop_outcome_reports_whether_the_container_is_stopped() {
        assert!(StopOutcome::Stopped.is_stopped());
        assert!(StopOutcome::AlreadyStopped.is_stopped());
        assert!(!StopOutcome::NoResponse.is_stopped());
        assert!(!StopOutcome::Rejected { status: 500 }.is_stopped());
        assert_eq!(
            StopOutcome::Rejected { status: 500 }.to_string(),
            "rejected by the daemon with HTTP status 500"
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
            StopOutcome::Unreachable,
            "no daemon received the request"
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
    fn stop_after_404_then_silent_daemon_is_no_response_not_not_found() {
        // One default daemon answers 404, then the next one receives the
        // request and never replies: the container may have been stopped.
        for force in [false, true] {
            let attempt = first_stop_owner(
                from_defaults([ipc::StopAttempt::Status(404), ipc::StopAttempt::NoResponse]),
                |attempt| attempt,
            );
            assert_eq!(
                stop_outcome(attempt, force),
                StopOutcome::NoResponse,
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

    // ── Client ───────────────────────────────────────────────────────

    #[test]
    fn client_is_shareable() {
        fn assert_shareable<T: Send + Sync + Clone + std::fmt::Debug + Default>() {}
        assert_shareable::<Client>();
    }

    #[test]
    fn query_budget_fits_inside_every_timeout() {
        assert_eq!(
            query_budget(DEFAULT_TIMEOUT),
            Duration::from_millis(2500),
            "the default timeout keeps the 0.1.x query budget"
        );
        for timeout in [
            Duration::from_nanos(1),
            Duration::from_millis(1),
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_secs(30),
            MAX_TIMEOUT,
        ] {
            assert!(
                query_budget(timeout) < timeout,
                "the daemons must be queried for less than the {timeout:?} timeout"
            );
        }
        assert_eq!(query_budget(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn client_caps_the_timeout() {
        let client = Client::new().timeout(Duration::MAX);
        assert_eq!(client.timeout, MAX_TIMEOUT, "deadlines must not overflow");
    }

    #[test]
    fn client_docker_host_tcp_is_tried_first() {
        let client = Client::new()
            .home(None)
            .docker_host(Some("tcp://10.0.0.1:2375".to_string()));
        let targets = client.detection_targets();
        assert_eq!(
            targets.first(),
            Some(&(true, DaemonEndpoint::Tcp("10.0.0.1:2375".to_string())))
        );
        assert!(
            targets.len() > 1,
            "a tcp:// daemon never replaces the default endpoints"
        );
    }

    #[test]
    fn client_without_docker_host_uses_only_defaults() {
        let client = Client::new().docker_host(None);
        assert!(
            client
                .detection_targets()
                .iter()
                .all(|(from_docker_host, _)| !from_docker_host),
            "no endpoint comes from DOCKER_HOST"
        );
        let ignored = Client::new().docker_host(Some("ssh://host".to_string()));
        assert_eq!(
            ignored.detection_targets(),
            client.detection_targets(),
            "an unsupported scheme is ignored"
        );
    }

    #[cfg(unix)]
    #[test]
    fn client_unix_docker_host_replaces_default_sockets() {
        let client = Client::new().docker_host(Some("unix:///tmp/custom.sock".to_string()));
        assert_eq!(
            client.stop_targets(),
            vec![(
                true,
                DaemonEndpoint::Unix(PathBuf::from("/tmp/custom.sock"))
            )]
        );
    }

    #[cfg(windows)]
    #[test]
    fn client_npipe_docker_host_is_tried_before_default_pipes() {
        let client = Client::new().docker_host(Some("npipe:////./pipe/custom".to_string()));
        let targets = client.stop_targets();
        assert_eq!(
            targets.first(),
            Some(&(true, DaemonEndpoint::Pipe(r"\\.\pipe\custom".to_string())))
        );
        assert_eq!(targets.len(), 1 + DEFAULT_PIPE_PATHS.len());
    }

    #[test]
    fn client_detect_uses_an_answering_docker_host() {
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
            let body = r#"[{"Id":"abc","Names":["/web"],"Image":"nginx","Ports":[{"PublicPort":8080,"Type":"tcp"}]}]"#;
            let response = format!(
                "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            drop(std::io::Write::write_all(&mut stream, response.as_bytes()));
        });

        let client = Client::new()
            .home(None)
            .docker_host(Some(format!("tcp://{addr}")));
        let map = client.detect().expect("the TCP daemon answered");
        drop(server.join());

        assert_eq!(
            map.get(None, 8080, Protocol::Tcp)
                .map(|info| info.name.as_str()),
            Some("web"),
            "the DOCKER_HOST daemon's containers are used"
        );
    }

    #[test]
    fn detection_handle_delivers_the_result_or_an_empty_map() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut map = ContainerPortMap::new();
        map.insert(
            None,
            80,
            Protocol::Tcp,
            test_container_info("a", "web", "nginx"),
        );
        tx.send(Ok(map)).expect("receiver alive");
        let handle = DetectionHandle {
            receiver: rx,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        assert_eq!(handle.wait().len(), 1, "the detected ports are returned");

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err(Error::DaemonNotFound)).expect("receiver alive");
        let failed = DetectionHandle {
            receiver: rx,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        assert!(failed.wait().is_empty(), "a failure reads as no containers");

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err(permission_denied())).expect("receiver alive");
        let explained = DetectionHandle {
            receiver: rx,
            deadline: Instant::now() + Duration::from_secs(5),
        };
        assert!(matches!(
            explained.wait_result(),
            Err(Error::PermissionDenied { .. })
        ));
    }

    #[test]
    fn detection_handle_times_out_at_its_deadline() {
        let (_tx, rx) = std::sync::mpsc::channel();
        let handle = DetectionHandle {
            receiver: rx,
            deadline: Instant::now() + Duration::from_millis(50),
        };
        let started = Instant::now();
        assert!(matches!(handle.wait_result(), Err(Error::Timeout)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the handle stops waiting at the deadline"
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

        for responses in [vec![first.clone(), second.clone()], vec![second, first]] {
            let bodies = select_daemon_bodies(responses);

            let lenient = merge_prioritized_responses(&bodies).expect("responses were given");
            assert_eq!(
                lenient
                    .get(None, 8080, Protocol::Tcp)
                    .map(|info| info.name.as_str()),
                Some("from-first-default"),
                "the earlier default endpoint wins a shared key"
            );

            let merged = merge_prioritized_bodies(&bodies).expect("responses were given");
            let strict = api::parse_containers_json_strict(&merged).expect("valid JSON");
            assert_eq!(
                strict
                    .get(None, 8080, Protocol::Tcp)
                    .map(|info| info.name.as_str()),
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

    /// Stand-in detection target: a real TCP address, a canned local reply,
    /// a canned failure, or an endpoint that never answers in time.
    enum FakeTarget {
        Tcp(String),
        Local(&'static str),
        Fail(fn() -> Error),
        Hung,
    }

    fn fetch_fake(target: &FakeTarget, deadline: Instant) -> Result<String, Error> {
        match target {
            FakeTarget::Tcp(addr) => DaemonEndpoint::Tcp(addr.clone()).fetch_json(deadline),
            FakeTarget::Local(body) => Ok((*body).to_string()),
            FakeTarget::Fail(error) => Err(error()),
            FakeTarget::Hung => {
                std::thread::sleep(std::time::Duration::from_secs(3));
                Err(Error::Timeout)
            }
        }
    }

    fn permission_denied() -> Error {
        Error::PermissionDenied {
            endpoint: "/var/run/docker.sock".to_string(),
        }
    }

    #[test]
    fn collect_daemon_bodies_reports_permission_denied_over_missing_daemons() {
        let error = collect_daemon_bodies(
            vec![
                (false, FakeTarget::Fail(|| Error::DaemonNotFound)),
                (false, FakeTarget::Fail(permission_denied)),
                (false, FakeTarget::Fail(|| Error::DaemonNotFound)),
            ],
            fetch_fake,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .expect_err("no endpoint answered");

        assert!(
            matches!(&error, Error::PermissionDenied { endpoint } if endpoint == "/var/run/docker.sock"),
            "the actionable failure wins, got {error:?}"
        );
    }

    #[test]
    fn collect_daemon_bodies_reports_daemon_not_found_when_nothing_listens() {
        let error = collect_daemon_bodies(
            vec![
                (true, FakeTarget::Fail(|| Error::DaemonNotFound)),
                (false, FakeTarget::Fail(|| Error::DaemonNotFound)),
            ],
            fetch_fake,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .expect_err("no endpoint answered");
        assert!(matches!(error, Error::DaemonNotFound), "got {error:?}");

        let no_targets = collect_daemon_bodies(
            Vec::<(bool, FakeTarget)>::new(),
            fetch_fake,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .expect_err("there was nothing to query");
        assert!(matches!(no_targets, Error::DaemonNotFound));
    }

    #[test]
    fn collect_daemon_bodies_reports_timeout_for_unfinished_endpoints() {
        let error = collect_daemon_bodies(
            vec![
                (false, FakeTarget::Fail(|| Error::DaemonNotFound)),
                (false, FakeTarget::Hung),
            ],
            fetch_fake,
            Instant::now() + std::time::Duration::from_millis(200),
        )
        .expect_err("no endpoint answered in time");
        assert!(matches!(error, Error::Timeout), "got {error:?}");
    }

    #[test]
    fn collect_daemon_bodies_ignores_failures_when_a_daemon_answers() {
        let bodies = collect_daemon_bodies(
            vec![
                (false, FakeTarget::Fail(permission_denied)),
                (false, FakeTarget::Local("[1]")),
            ],
            fetch_fake,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .expect("one daemon answered");
        assert_eq!(bodies, vec!["[1]".to_string()]);
    }

    #[test]
    fn most_informative_prefers_answers_and_keeps_priority_on_ties() {
        let error = most_informative([
            Error::DaemonNotFound,
            Error::Timeout,
            Error::HttpStatus(500),
            permission_denied(),
            Error::HttpStatus(503),
        ]);
        assert!(
            matches!(error, Error::HttpStatus(500)),
            "a daemon that answered beats one that refused, and the first answer wins a tie, got {error:?}"
        );
        assert!(matches!(
            most_informative(std::iter::empty()),
            Error::DaemonNotFound
        ));
    }

    #[test]
    fn errors_describe_what_happened_and_chain_their_source() {
        use std::error::Error as _;

        assert_eq!(
            permission_denied().to_string(),
            "permission denied connecting to the container runtime at /var/run/docker.sock"
        );
        assert_eq!(
            Error::HttpStatus(500).to_string(),
            "the container runtime daemon answered with HTTP status 500"
        );

        let json_error = api::parse_containers_json_strict("not json").expect_err("invalid JSON");
        assert!(
            json_error
                .to_string()
                .starts_with("invalid container list JSON"),
            "got {json_error}"
        );
        let error = Error::from(json_error);
        assert!(
            error.source().is_some(),
            "an invalid response chains the parse error"
        );
        assert!(
            Error::Io(std::io::ErrorKind::ConnectionReset.into())
                .source()
                .is_some()
        );
        assert!(Error::Timeout.source().is_none());
    }

    #[test]
    fn endpoint_permission_failure_names_the_endpoint() {
        let endpoint = DaemonEndpoint::Tcp("127.0.0.1:2375".to_string());
        let error = endpoint.error(ipc::FetchError::PermissionDenied);
        assert!(
            matches!(&error, Error::PermissionDenied { endpoint } if endpoint == "tcp://127.0.0.1:2375"),
            "got {error:?}"
        );
        assert!(matches!(
            endpoint.error(ipc::FetchError::Malformed("bad")),
            Error::InvalidResponse(_)
        ));
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
        )
        .expect("the local daemons answered");
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
        )
        .expect("the local daemon answered");
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
        )
        .expect("the TCP daemon answered");
        drop(server.join());

        assert_eq!(
            bodies,
            vec!["[9]".to_string()],
            "an answering DOCKER_HOST daemon is used on its own"
        );
    }
}
