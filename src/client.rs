//! [`Client`] and [`DetectionHandle`]: daemon settings, endpoint
//! selection, the concurrent detection query, and merging the replies.

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use log::debug;

use crate::error::most_informative;
use crate::stop::{StopKind, first_stop_owner, is_safe_container_id, stop_endpoint, stop_outcome};
use crate::{ContainerPortMap, Error, ParseError, StopOutcome, api, ipc};

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
#[derive(Debug, Clone)]
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
    /// and wins when it answers. As in the Docker CLI, the port defaults to
    /// 2375, an empty host (`tcp://` or `tcp://:2376`) means `127.0.0.1`, an
    /// IPv6 address goes in brackets (`tcp://[::1]:2375`), a path after the
    /// address is ignored, and a value without a scheme (`host:port`) is a
    /// `tcp://` address. `unix:///path` (Unix) replaces the default sockets.
    /// `npipe:////./pipe/name` or `npipe:////host/pipe/name` (Windows) is
    /// queried alongside the default pipes and wins when it answers; an
    /// `npipe://` value that names no pipe is ignored. Surrounding
    /// whitespace does not matter, and unlike the Docker CLI nanodock also
    /// accepts the scheme in any letter case. `None`, an empty value, or a
    /// value with another scheme (such as `ssh://`) or a malformed address,
    /// uses only the default endpoints.
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
    /// [`Error::InvalidResponse`] when a daemon whose answer is used sent
    /// something other than a JSON array of containers.
    pub fn detect(&self) -> Result<ContainerPortMap, Error> {
        debug!("starting synchronous container runtime detection");
        let bodies = self.query_daemon_bodies(Instant::now())?;
        let map = merge_bodies(&bodies, api::parse_containers_json_strict)?;
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
    /// skipping malformed container entries instead of failing. If the
    /// operating system cannot start the thread, the handle reports
    /// [`Error::Io`] at once.
    #[must_use]
    pub fn start_detection(&self) -> DetectionHandle {
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let client = self.clone();
        debug!("starting container runtime detection");
        let worker_tx = tx.clone();
        let spawned = ipc::spawn_detached("nanodock-detect", move || {
            let result = client.query_daemon(started);
            match &result {
                Ok(map) => debug!(
                    "finished container runtime detection: port_mappings={}",
                    map.len()
                ),
                Err(error) => debug!("container runtime detection failed: {error}"),
            }
            // Ignore send error: receiver may have timed out and been dropped.
            drop(worker_tx.send(result));
        });
        if let Err(source) = spawned {
            // The handle reports the failure at once instead of waiting
            // for a result that cannot come.
            drop(tx.send(Err(Error::Io {
                source,
                endpoint: None,
            })));
        }
        DetectionHandle {
            receiver: rx,
            deadline: started + self.timeout,
        }
    }

    /// Stop a running container gracefully through the daemon API.
    ///
    /// Sends `POST /containers/{id}/stop?t=10`: the daemon sends the
    /// container's stop signal (SIGTERM by default) and kills it if it is
    /// still running 10 seconds later. Use [`Client::kill`] to kill it at
    /// once.
    ///
    /// The `id` can be a container ID (hex), a unique ID prefix, or a
    /// container name. An `id` that cannot name a container is rejected with
    /// [`StopOutcome::NotFound`] before any daemon is contacted: it must
    /// start with an ASCII letter or digit, continue with ASCII letters,
    /// digits, `_`, `.`, or `-`, and be at most 256 bytes long (the name
    /// pattern Docker and Podman enforce).
    ///
    /// Tries the same daemons as detection, in priority order
    /// (`DOCKER_HOST` first, then the platform defaults; a `unix://`
    /// `DOCKER_HOST` replaces the default sockets). Before the stop request
    /// is sent to a daemon, it must answer `GET /_ping` with a 2xx status on
    /// a separate connection; a daemon that cannot be reached or does not
    /// answer the ping that way (for example a forwarder whose backend is
    /// down, or a TLS port that answers plain HTTP with 400) is skipped
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
    pub fn stop(&self, id: &str) -> StopOutcome {
        self.send_stop(id, StopKind::Graceful)
    }

    /// Kill a running container at once through the daemon API.
    ///
    /// Sends `POST /containers/{id}/kill` (SIGKILL, no grace period). The
    /// daemon answers once the signal is delivered, before the container has
    /// finished exiting, so [`StopOutcome::Stopped`] does not mean it is gone
    /// from a listing taken right away. A container that is not running is
    /// [`StopOutcome::AlreadyStopped`].
    ///
    /// The `id`, the daemons tried, their order, the `GET /_ping` check
    /// before the request, and every outcome rule are the same as for
    /// [`Client::stop`]: in particular, once a daemon may have received the
    /// kill request no other daemon is tried, and any reply from the
    /// `DOCKER_HOST` daemon is the result.
    #[must_use]
    pub fn kill(&self, id: &str) -> StopOutcome {
        self.send_stop(id, StopKind::Kill)
    }

    /// Validate `id` and send a stop or kill request for it.
    fn send_stop(&self, id: &str, kind: StopKind) -> StopOutcome {
        if !is_safe_container_id(id) {
            debug!("rejected a container id that cannot name a container");
            return StopOutcome::NotFound;
        }

        let endpoint = stop_endpoint(id, kind);
        let attempt = first_stop_owner(self.stop_targets(), |target| target.send_stop(&endpoint));
        stop_outcome(attempt, kind)
    }
}

/// Handle for a container detection running on a background thread.
///
/// Created by [`Client::start_detection`] or
/// [`start_detection`](crate::start_detection). The channel inside is
/// private so the detection mechanism can change without breaking the
/// public API.
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
    /// let handle = nanodock::start_detection();
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
    /// produced a container list (see [`Error`]), with [`Error::Timeout`]
    /// when the client's timeout passed first, or with [`Error::Io`] when
    /// the detection thread could not be started.
    pub fn wait_result(self) -> Result<ContainerPortMap, Error> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        match self.receiver.recv_timeout(remaining) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                debug!("container runtime detection timed out");
                Err(Error::Timeout { endpoint: None })
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                debug!("container runtime detection channel disconnected");
                Err(Error::Io {
                    source: std::io::Error::other("the detection thread stopped without a result"),
                    endpoint: None,
                })
            }
        }
    }
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
    /// Connect to this endpoint; every read and write fails once `deadline`
    /// has passed.
    fn connect(&self, deadline: Instant) -> std::io::Result<Box<dyn ipc::Stream>> {
        Ok(match self {
            Self::Tcp(addr) => Box::new(ipc::connect_tcp(addr, deadline)?),
            #[cfg(unix)]
            Self::Unix(path) => Box::new(ipc::connect_unix(path, deadline)?),
            #[cfg(windows)]
            Self::Pipe(path) => Box::new(ipc::connect_pipe(path, deadline)?),
        })
    }

    /// Fetch the container list JSON body before `deadline`.
    fn fetch_json(&self, deadline: Instant) -> Result<String, Error> {
        ipc::fetch_json(|deadline| self.connect(deadline), deadline).map_err(|error| {
            debug!("container runtime returned no container list: endpoint={self} error={error:?}");
            self.error(error)
        })
    }

    /// Turn a transport failure at this endpoint into a public [`Error`].
    fn error(&self, error: ipc::FetchError) -> Error {
        match error {
            ipc::FetchError::NotFound => Error::DaemonNotFound,
            ipc::FetchError::PermissionDenied => Error::PermissionDenied {
                endpoint: self.to_string(),
            },
            ipc::FetchError::Timeout => Error::Timeout {
                endpoint: Some(self.to_string()),
            },
            ipc::FetchError::Status(status) => Error::HttpStatus { status },
            ipc::FetchError::Malformed(reason) => Error::InvalidResponse {
                source: ParseError::http(reason),
            },
            ipc::FetchError::Io(source) => Error::Io {
                source,
                endpoint: Some(self.to_string()),
            },
        }
    }

    /// Send a stop or kill request for `endpoint`.
    fn send_stop(&self, endpoint: &str) -> ipc::StopAttempt {
        let deadline = Instant::now() + ipc::STOP_TIMEOUT;
        let attempt = ipc::stop_via(|deadline| self.connect(deadline), endpoint, deadline);
        debug!("container runtime stop attempt: endpoint={self} attempt={attempt:?}");
        attempt
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
    // SAFETY: getuid() is a simple syscall with no preconditions.
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
        Ok(merge_bodies_lenient(&self.query_daemon_bodies(started)?))
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
/// target still running at the deadline counts as [`Error::Timeout`] naming
/// the first such target.
fn collect_daemon_bodies<P, F>(
    targets: Vec<(bool, P)>,
    fetch: F,
    deadline: Instant,
) -> Result<Vec<String>, Error>
where
    P: std::fmt::Display + Send + 'static,
    F: Fn(&P, Instant) -> Result<String, Error> + Send + Sync + 'static,
{
    let mut unfinished: Vec<Option<String>> = targets
        .iter()
        .map(|(_, target)| Some(target.to_string()))
        .collect();
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
        if let Some(name) = unfinished.get_mut(priority) {
            *name = None;
        }
        match result {
            Ok(body) => responses.push((priority, from_docker_host, body)),
            Err(error) => failures.push((priority, error)),
        }
    }
    // A target whose thread could not be started was never queried: the
    // failure is local, so it names no endpoint.
    for (priority, source) in fan_out.spawn_failures {
        if let Some(name) = unfinished.get_mut(priority) {
            *name = None;
        }
        failures.push((
            priority,
            Error::Io {
                source,
                endpoint: None,
            },
        ));
    }

    if responses.is_empty() {
        failures.sort_by_key(|(priority, _)| *priority);
        let timed_out = (fan_out.unfinished > 0).then(|| Error::Timeout {
            endpoint: unfinished.into_iter().flatten().next(),
        });
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

/// Parse every daemon's body with `parse` and merge the port maps.
///
/// `bodies` are highest priority first. They are merged lowest priority
/// first, so when two daemons publish the same binding the higher-priority
/// daemon overwrites the other. The first parse error ends the merge.
fn merge_bodies<E>(
    bodies: &[String],
    mut parse: impl FnMut(&str) -> Result<ContainerPortMap, E>,
) -> Result<ContainerPortMap, E> {
    let mut merged = ContainerPortMap::new();
    for body in bodies.iter().rev() {
        merged.merge(parse(body)?);
    }
    Ok(merged)
}

/// Merge bodies like [`merge_bodies`], parsing each one leniently: a body
/// that is not a container list contributes no bindings.
fn merge_bodies_lenient(bodies: &[String]) -> ContainerPortMap {
    let Ok(map) = merge_bodies(bodies, |body| {
        Ok::<_, Infallible>(api::parse_containers_json(body))
    });
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContainerInfo, Protocol};

    // ── merge_bodies ─────────────────────────────────────────────────

    fn bodies(bodies: &[&str]) -> Vec<String> {
        bodies.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn merged_map_is_truncated_when_any_reply_was() {
        let oversized = r#"[{"Names": ["/wide"], "Ports": [
            {"host_port": 1, "range": 65535, "protocol": "tcp"},
            {"host_port": 1, "range": 65535, "protocol": "udp"},
            {"host_ip": "127.0.0.1", "host_port": 1, "range": 65535, "protocol": "tcp"}
        ]}]"#;
        let small = r#"[{"Names": ["/web"], "Ports": [{"PublicPort": 80}]}]"#;

        for order in [[small, oversized], [oversized, small]] {
            let merged = merge_bodies_lenient(&bodies(&order));
            assert!(merged.truncated(), "a truncated reply truncates the merge");
            assert!(
                merged.get(None, 80, Protocol::Tcp).is_some(),
                "the other daemon's bindings are kept"
            );
        }
        assert!(!merge_bodies_lenient(&bodies(&[small, small])).truncated());
    }

    #[test]
    fn merge_bodies_combines_every_daemon() {
        let bodies = bodies(&[
            "[]",
            r#"[{"Names": ["/db"], "Image": "postgres:16", "Ports": [{"PublicPort": 5432}]}]"#,
            r#"[{"Names": ["/web"], "Ports": [{"PublicPort": 80}]}, {"Names": ["/idle"]}]"#,
        ]);
        let lenient = merge_bodies_lenient(&bodies);
        assert_eq!(lenient.len(), 2, "every daemon's bindings are kept");
        assert_eq!(
            lenient
                .get(None, 5432, Protocol::Tcp)
                .map(|info| info.image.as_str()),
            Some("postgres:16"),
            "a lower-priority daemon's containers survive the merge"
        );

        let strict = merge_bodies(&bodies, api::parse_containers_json_strict).expect("valid lists");
        assert_eq!(strict, lenient, "both modes merge valid lists the same way");
        assert!(
            merge_bodies(&[], api::parse_containers_json_strict)
                .expect("nothing to parse")
                .is_empty()
        );
    }

    #[test]
    fn merge_bodies_rejects_a_reply_that_is_not_a_list_only_when_strict() {
        let bodies = bodies(&[
            r#"{"message": "page not found"}"#,
            r#"[{"Names": ["/web"], "Ports": [{"PublicPort": 80}]}]"#,
        ]);
        let error = merge_bodies(&bodies, api::parse_containers_json_strict)
            .map_err(Error::from)
            .expect_err("an error object is not a container list");
        assert!(
            matches!(error, Error::InvalidResponse { .. }),
            "got {error:?}"
        );

        let lenient = merge_bodies_lenient(&bodies);
        assert_eq!(lenient.len(), 1, "the error object contributes nothing");
        assert!(lenient.get(None, 80, Protocol::Tcp).is_some());
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
        // The defaults that exist depend on the host (a macOS runner has no
        // Docker socket at all), so compare with a client without the
        // override instead of counting them.
        let defaults = Client::new().home(None).docker_host(None);
        assert_eq!(
            targets.get(1..),
            Some(defaults.detection_targets().as_slice()),
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
            ContainerInfo::new("a", "web", "nginx"),
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
        assert!(matches!(
            handle.wait_result(),
            Err(Error::Timeout { endpoint: None })
        ));
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

            let lenient = merge_bodies_lenient(&bodies);
            assert_eq!(
                lenient
                    .get(None, 8080, Protocol::Tcp)
                    .map(|info| info.name.as_str()),
                Some("from-first-default"),
                "the earlier default endpoint wins a shared key"
            );

            let strict =
                merge_bodies(&bodies, api::parse_containers_json_strict).expect("valid JSON");
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

    impl std::fmt::Display for FakeTarget {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Tcp(addr) => write!(f, "tcp://{addr}"),
                Self::Local(_) => f.write_str("local"),
                Self::Fail(_) => f.write_str("failing"),
                Self::Hung => f.write_str("hung"),
            }
        }
    }

    fn fetch_fake(target: &FakeTarget, deadline: Instant) -> Result<String, Error> {
        match target {
            FakeTarget::Tcp(addr) => DaemonEndpoint::Tcp(addr.clone()).fetch_json(deadline),
            FakeTarget::Local(body) => Ok((*body).to_string()),
            FakeTarget::Fail(error) => Err(error()),
            FakeTarget::Hung => {
                // Far past every test deadline; the worker is detached.
                std::thread::sleep(std::time::Duration::from_secs(10));
                Err(Error::Timeout { endpoint: None })
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
            // Long enough that the failing target always reports in time.
            Instant::now() + std::time::Duration::from_secs(2),
        )
        .expect_err("no endpoint answered in time");
        assert!(
            matches!(&error, Error::Timeout { endpoint: Some(endpoint) } if endpoint == "hung"),
            "the endpoint still answering at the deadline is named, got {error:?}"
        );
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
    fn endpoint_permission_failure_names_the_endpoint() {
        let endpoint = DaemonEndpoint::Tcp("127.0.0.1:2375".to_string());
        let error = endpoint.error(ipc::FetchError::PermissionDenied);
        assert!(
            matches!(&error, Error::PermissionDenied { endpoint } if endpoint == "tcp://127.0.0.1:2375"),
            "got {error:?}"
        );
        assert!(matches!(
            endpoint.error(ipc::FetchError::Malformed("bad")),
            Error::InvalidResponse { .. }
        ));
        assert!(matches!(
            endpoint.error(ipc::FetchError::Timeout),
            Error::Timeout { endpoint: Some(name) } if name == "tcp://127.0.0.1:2375"
        ));
        assert!(matches!(
            endpoint.error(ipc::FetchError::Io(std::io::ErrorKind::BrokenPipe.into())),
            Error::Io { endpoint: Some(name), .. } if name == "tcp://127.0.0.1:2375"
        ));
        assert!(matches!(
            endpoint.error(ipc::FetchError::Status(503)),
            Error::HttpStatus { status: 503 }
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
            started + std::time::Duration::from_secs(2),
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
            elapsed < std::time::Duration::from_secs(4),
            "the shared budget bounds the pass, took {elapsed:?}"
        );
    }

    #[test]
    fn collect_daemon_bodies_keeps_local_results_when_tcp_hangs() {
        let started = Instant::now();
        let bodies = collect_daemon_bodies(
            vec![(true, FakeTarget::Hung), (false, FakeTarget::Local("[1]"))],
            fetch_fake,
            started + std::time::Duration::from_secs(2),
        )
        .expect("the local daemon answered");
        let elapsed = started.elapsed();

        assert_eq!(
            bodies,
            vec!["[1]".to_string()],
            "a blackholed DOCKER_HOST must not starve the local endpoints"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
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
