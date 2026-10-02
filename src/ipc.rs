//! OS-specific transport layer for Docker/Podman daemon communication.
//!
//! Provides Unix socket, Windows named pipe, and TCP connection functions.
//! Each transport connects to the daemon, delegates to the HTTP engine for
//! request/response handling, and returns the raw JSON body string, or a
//! [`FetchError`] that says why the endpoint produced none.
//!
//! Every query and stop request runs against one overall deadline. Socket
//! read timeouts only apply per call, so sockets are wrapped in
//! [`DeadlineStream`], which shrinks the timeout before every read and write
//! and fails once the deadline has passed. Windows named pipes have no read
//! timeout at all; `PipeStream` gives them the same deadline behavior, so
//! every transport is a plain `Read + Write` stream for the HTTP parser.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use log::debug;

use crate::http;

/// Why one daemon endpoint produced no container list.
#[derive(Debug)]
pub enum FetchError {
    /// Nothing answers at the endpoint: the socket or pipe does not exist,
    /// or the connection was refused.
    NotFound,
    /// The endpoint exists but the current user may not connect to it.
    PermissionDenied,
    /// The endpoint did not finish answering before the deadline.
    Timeout,
    /// The daemon answered with a non-2xx HTTP status.
    Status(u16),
    /// The reply was not a well-formed HTTP response.
    Malformed(&'static str),
    /// Any other I/O failure.
    Io(io::Error),
}

impl FetchError {
    /// Classify a failure to connect to (or open) an endpoint.
    pub fn from_connect_error(error: io::Error) -> Self {
        match error.kind() {
            // A stale socket file with no listener refuses the connection:
            // no daemon is running there.
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            kind if is_timeout(kind) => Self::Timeout,
            _ => Self::Io(error),
        }
    }
}

impl From<http::ResponseError> for FetchError {
    fn from(error: http::ResponseError) -> Self {
        match error {
            http::ResponseError::Io(error) if is_timeout(error.kind()) => Self::Timeout,
            http::ResponseError::Io(error) => Self::Io(error),
            http::ResponseError::Status(status_code) => Self::Status(status_code),
            http::ResponseError::Malformed(reason) => Self::Malformed(reason),
        }
    }
}

/// Whether an I/O error kind means a deadline or socket timeout passed.
///
/// [`DeadlineStream`] reports an expired deadline as `TimedOut`; a socket
/// read timeout surfaces as `WouldBlock` on Unix and `TimedOut` on Windows.
const fn is_timeout(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

/// Time left before `deadline`, or `None` when it has passed.
///
/// Never returns a zero duration because std rejects zero socket timeouts.
fn remaining_until(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
}

// ---------------------------------------------------------------------------
// Overall-deadline stream wrapper
// ---------------------------------------------------------------------------

/// Sockets whose per-call read and write timeouts can be adjusted.
pub trait SocketTimeouts {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

impl SocketTimeouts for std::net::TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Self::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Self::set_write_timeout(self, timeout)
    }
}

#[cfg(unix)]
impl SocketTimeouts for std::os::unix::net::UnixStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Self::set_read_timeout(self, timeout)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        Self::set_write_timeout(self, timeout)
    }
}

/// Stream wrapper that enforces one overall deadline across all I/O calls.
///
/// A plain socket read timeout restarts on every call, so a daemon that
/// trickles bytes slowly would never trip it. This wrapper sets the socket
/// timeout to the time remaining before each call and returns
/// [`io::ErrorKind::TimedOut`] once the deadline has passed.
pub struct DeadlineStream<S> {
    inner: S,
    deadline: Instant,
}

impl<S: SocketTimeouts> DeadlineStream<S> {
    const fn new(inner: S, deadline: Instant) -> Self {
        Self { inner, deadline }
    }

    fn remaining(&self) -> io::Result<Duration> {
        remaining_until(self.deadline).ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    }
}

impl<S: SocketTimeouts + Read> Read for DeadlineStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        self.inner.set_read_timeout(Some(remaining))?;
        self.inner.read(buf)
    }
}

impl<S: SocketTimeouts + Write> Write for DeadlineStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        self.inner.set_write_timeout(Some(remaining))?;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// DOCKER_HOST parsing
// ---------------------------------------------------------------------------

/// Port of a `tcp://` `DOCKER_HOST` that names none, as in the Docker CLI.
const DEFAULT_TCP_PORT: u16 = 2375;

/// The local `DOCKER_HOST` scheme this platform supports.
const LOCAL_SCHEME: &str = if cfg!(windows) { "npipe" } else { "unix" };

/// Split a `DOCKER_HOST` value into its scheme and the rest, ignoring
/// surrounding whitespace.
fn split_scheme(docker_host: &str) -> Option<(&str, &str)> {
    docker_host.trim().split_once("://")
}

/// The part of `docker_host` after `scheme://`, when it uses that scheme
/// (in any letter case).
fn strip_scheme<'a>(docker_host: &'a str, scheme: &str) -> Option<&'a str> {
    split_scheme(docker_host)
        .filter(|(found, _)| found.eq_ignore_ascii_case(scheme))
        .map(|(_, rest)| rest)
}

/// Extract a Unix socket path from a `DOCKER_HOST` value.
///
/// Returns the path after `unix://`, or `None` when it is empty or the
/// value uses a different scheme.
#[cfg(unix)]
pub fn docker_host_unix_path(docker_host: &str) -> Option<String> {
    let path = strip_scheme(docker_host, "unix")?;
    (!path.is_empty()).then(|| path.to_string())
}

/// Extract a named pipe path from a `DOCKER_HOST` value.
///
/// Returns the pipe path, with forward slashes turned into backslashes,
/// when the value is `npipe://` followed by `//./pipe/<name>` or
/// `//<host>/pipe/<name>` (in either slash direction). Anything else,
/// such as `npipe://C:/x.txt`, is rejected so that no ordinary file is
/// ever opened as a daemon pipe.
#[cfg(windows)]
pub fn docker_host_npipe_path(docker_host: &str) -> Option<String> {
    let path = strip_scheme(docker_host, "npipe")?.replace('/', "\\");
    if is_pipe_path(&path) {
        Some(path)
    } else {
        debug!("ignoring DOCKER_HOST npipe:// value that names no pipe: path={path}");
        None
    }
}

/// Whether `path` is `\\<host>\pipe\<name>`, the only form of a named pipe
/// path: `<host>` is `.` for the local machine, and neither it nor
/// `<name>` is empty or contains a backslash.
#[cfg(any(windows, test))]
fn is_pipe_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix(r"\\") else {
        return false;
    };
    let mut parts = rest.splitn(3, '\\');
    let (Some(host), Some(pipe), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !host.is_empty()
        && pipe.eq_ignore_ascii_case("pipe")
        && !name.is_empty()
        && !name.contains('\\')
}

/// Extract a TCP address from a `DOCKER_HOST` value.
///
/// Accepts what the Docker CLI accepts: `tcp://host:port`, `tcp://host`
/// (port 2375), an IPv6 literal in brackets (`tcp://[::1]:2375` or
/// `tcp://[::1]`), surrounding whitespace, any letter case in the scheme,
/// and a trailing slash or path after the address, which is ignored.
/// Returns `host:port`, or `None` for a malformed address or another
/// scheme. A scheme nanodock does not support at all (such as `ssh://`,
/// `fd://`, or `http://`) is logged at debug level and otherwise ignored,
/// so only the default endpoints are used.
pub fn docker_host_tcp_addr(docker_host: &str) -> Option<String> {
    let Some((scheme, rest)) = split_scheme(docker_host) else {
        debug!("ignoring DOCKER_HOST without a scheme: value={docker_host}");
        return None;
    };
    if !scheme.eq_ignore_ascii_case("tcp") {
        if !scheme.eq_ignore_ascii_case(LOCAL_SCHEME) {
            debug!("ignoring DOCKER_HOST with an unsupported scheme: scheme={scheme}");
        }
        return None;
    }
    let addr = tcp_host_port(rest);
    if addr.is_none() {
        debug!("ignoring malformed DOCKER_HOST tcp:// address: address={rest}");
    }
    addr
}

/// `host:port` from what follows `tcp://`: any path after the address is
/// dropped, and a missing port is [`DEFAULT_TCP_PORT`].
fn tcp_host_port(address_and_path: &str) -> Option<String> {
    let address = address_and_path
        .split_once('/')
        .map_or(address_and_path, |(address, _)| address);
    let (host, port) = match address.strip_prefix('[') {
        Some(bracketed) => {
            let (ip, after) = bracketed.split_once(']')?;
            ip.parse::<std::net::Ipv6Addr>().ok()?;
            let port = match after {
                "" => None,
                after => Some(after.strip_prefix(':')?),
            };
            // The literal with its brackets: `[`, the address, `]`.
            (&address[..ip.len() + 2], port)
        }
        None => match address.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (address, None),
        },
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None | Some("") => DEFAULT_TCP_PORT,
        Some(port) => port.parse().ok()?,
    };
    Some(format!("{host}:{port}"))
}

// ---------------------------------------------------------------------------
// Concurrent fan-out across daemon endpoints
// ---------------------------------------------------------------------------

/// Results of querying several endpoints concurrently.
#[derive(Debug)]
pub struct FanOut<T> {
    /// Results that arrived before the deadline, in arrival order.
    pub results: Vec<T>,
    /// Workers still running at the deadline; their results are dropped.
    pub unfinished: usize,
    /// Candidates whose worker thread could not be started, by their
    /// position in the candidate list, with the error.
    pub spawn_failures: Vec<(usize, io::Error)>,
}

/// Start `work` on a detached thread named `name`.
///
/// Fails, instead of panicking like `std::thread::spawn`, when the OS cannot
/// create the thread (for example when the process is out of threads or
/// memory).
pub fn spawn_detached(name: &str, work: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(work)
        .map(drop)
        .inspect_err(|error| debug!("failed to start thread: name={name} error={error}"))
}

/// Query every candidate on its own thread and collect the results that
/// arrive before `deadline`.
///
/// Returns as soon as every worker has finished, or at the deadline with
/// whatever has arrived by then. Workers still running at the deadline are
/// detached rather than joined: a stuck endpoint must not delay or discard
/// the answers of the others. Detached workers are bounded by their own
/// transport deadline, and their late results are dropped with the channel.
/// A candidate whose thread cannot be started is listed in
/// [`FanOut::spawn_failures`]; the others still run.
pub fn fetch_all<P, T, I, F>(candidates: I, fetch: F, deadline: Instant) -> FanOut<T>
where
    P: Send + 'static,
    T: Send + 'static,
    I: IntoIterator<Item = P>,
    F: Fn(P) -> T + Send + Sync + 'static,
{
    fetch_all_with(candidates, fetch, deadline, |work| {
        spawn_detached("nanodock-query", work)
    })
}

/// [`fetch_all`] with the thread spawner passed in, so tests can make it
/// fail.
fn fetch_all_with<P, T, I, F, S>(candidates: I, fetch: F, deadline: Instant, spawn: S) -> FanOut<T>
where
    P: Send + 'static,
    T: Send + 'static,
    I: IntoIterator<Item = P>,
    F: Fn(P) -> T + Send + Sync + 'static,
    S: Fn(Box<dyn FnOnce() + Send>) -> io::Result<()>,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let fetch = std::sync::Arc::new(fetch);
    let mut spawned = 0_usize;
    let mut spawn_failures = Vec::new();

    for (position, candidate) in candidates.into_iter().enumerate() {
        let tx = tx.clone();
        let fetch = std::sync::Arc::clone(&fetch);
        // Detached on purpose: the collector below never joins workers.
        let started = spawn(Box::new(move || {
            // The receiver is gone once the deadline passed; ignore that.
            drop(tx.send(fetch(candidate)));
        }));
        match started {
            Ok(()) => spawned += 1,
            Err(error) => spawn_failures.push((position, error)),
        }
    }

    drop(tx);
    let mut results = Vec::with_capacity(spawned);

    while let Some(remaining) = remaining_until(deadline) {
        match rx.recv_timeout(remaining) {
            Ok(result) => results.push(result),
            Err(
                std::sync::mpsc::RecvTimeoutError::Disconnected
                | std::sync::mpsc::RecvTimeoutError::Timeout,
            ) => break,
        }
    }

    // Keep anything that landed right at the deadline boundary.
    results.extend(rx.try_iter());
    FanOut {
        unfinished: spawned.saturating_sub(results.len()),
        results,
        spawn_failures,
    }
}

// ---------------------------------------------------------------------------
// Unix socket discovery
// ---------------------------------------------------------------------------
//
// The pure helpers below are also compiled for tests on every host, so the
// candidate order and the existence and symlink filtering are tested on
// Windows too. Only the Unix transport uses them at runtime.

/// Well-known daemon sockets below the user's home directory, in priority
/// order.
#[cfg(any(unix, test))]
const HOME_SOCKET_PATHS: &[&str] = &[
    // Docker Desktop for Linux.
    ".docker/desktop/docker.sock",
    // Docker Desktop for macOS (4.13 and later).
    ".docker/run/docker.sock",
    // Colima default profile (0.4 and later), then its pre-0.4 location.
    ".colima/default/docker.sock",
    ".colima/docker.sock",
    // OrbStack.
    ".orbstack/run/docker.sock",
    // Rancher Desktop with the moby engine.
    ".rd/docker.sock",
    // Lima: the `default` instance and the instance `template://docker` creates.
    ".lima/default/sock/docker.sock",
    ".lima/docker/sock/docker.sock",
    // Podman machine API forwarding on macOS (Podman 4).
    ".local/share/containers/podman/machine/podman.sock",
    ".local/share/containers/podman/machine/qemu/podman.sock",
    ".local/share/containers/podman/machine/podman-machine-default/podman.sock",
];

/// Podman machine API socket below `$TMPDIR` on macOS (Podman 5).
///
/// Only looked up on macOS (see [`podman_machine_tmpdir`]): there `$TMPDIR`
/// is a per-user directory with mode 0700. On Linux it is usually the shared
/// `/tmp`, where another local user could plant a socket at this path.
#[cfg(any(unix, test))]
const TMPDIR_SOCKET_PATH: &str = "podman/podman-machine-default-api.sock";

/// Every default Unix socket candidate, in priority order, without duplicates.
///
/// A pure function of its inputs so the order can be tested without touching
/// the process environment. `xdg_runtime_dir` (where rootless Docker and
/// Podman put their sockets) is tried before the hardcoded `/run/user/{uid}`
/// fallback; when the two are the same directory it is listed once. `tmpdir`
/// adds the Podman machine socket below it and must only be passed on macOS.
#[cfg(any(unix, test))]
fn unix_socket_candidates(
    uid: u32,
    home: Option<std::path::PathBuf>,
    xdg_runtime_dir: Option<&std::path::Path>,
    tmpdir: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let user_runtime_dir = std::path::PathBuf::from(format!("/run/user/{uid}"));
    let runtime_dirs: Vec<&std::path::Path> = xdg_runtime_dir
        .into_iter()
        .chain([user_runtime_dir.as_path()])
        .collect();

    let mut candidates = vec![std::path::PathBuf::from("/var/run/docker.sock")];
    candidates.extend(runtime_dirs.iter().map(|dir| dir.join("docker.sock")));
    candidates.extend(
        runtime_dirs
            .iter()
            .map(|dir| dir.join("podman/podman.sock")),
    );
    candidates.push(std::path::PathBuf::from("/run/podman/podman.sock"));
    if let Some(home) = home {
        candidates.extend(HOME_SOCKET_PATHS.iter().map(|path| home.join(path)));
    }
    if let Some(tmpdir) = tmpdir {
        candidates.push(tmpdir.join(TMPDIR_SOCKET_PATH));
    }

    let mut seen = std::collections::HashSet::new();
    candidates.retain(|path| seen.insert(path.clone()));
    candidates
}

/// Keep the candidates whose file exists, in order, dropping any that resolve
/// to the same file as an earlier one.
///
/// Checking up front means no worker thread is spawned for a socket that is
/// not there. Comparing canonical paths means a socket reachable through
/// several paths (for example `/var/run/docker.sock` symlinked to Docker
/// Desktop's or Podman's socket) is queried once, at its highest-priority
/// position. Dangling symlinks are skipped like missing files.
#[cfg(any(unix, test))]
fn existing_unique_sockets(candidates: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let mut seen = std::collections::HashSet::new();
    candidates
        .into_iter()
        .filter(|path| match std::fs::canonicalize(path) {
            Ok(resolved) => {
                let first = seen.insert(resolved);
                if !first {
                    debug!(
                        "skipping socket that resolves to an earlier candidate: {}",
                        path.display()
                    );
                }
                first
            }
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    debug!(
                        "skipping unusable socket candidate: path={} error={error}",
                        path.display()
                    );
                }
                false
            }
        })
        .collect()
}

/// An absolute path from the environment variable `name`, if set.
///
/// Relative and empty values are ignored, as the XDG Base Directory
/// specification requires for `XDG_RUNTIME_DIR`.
#[cfg(unix)]
fn absolute_env_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os(name)
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// `$TMPDIR`, where Podman 5 puts its machine API socket on macOS.
///
/// macOS gives every user a private `$TMPDIR` (mode 0700), so a socket there
/// was created by this user.
#[cfg(target_os = "macos")]
fn podman_machine_tmpdir() -> Option<std::path::PathBuf> {
    absolute_env_path("TMPDIR")
}

/// Outside macOS `$TMPDIR` is not consulted: it is usually the shared `/tmp`,
/// where any local user could create `podman/podman-machine-default-api.sock`.
#[cfg(all(unix, not(target_os = "macos")))]
const fn podman_machine_tmpdir() -> Option<std::path::PathBuf> {
    None
}

/// Whether a daemon socket whose file belongs to `owner` may be used by the
/// user `uid`: only sockets owned by that user or by root are trusted.
///
/// Every default candidate is checked, so a socket another unprivileged user
/// planted at a well-known path (in a shared or misconfigured directory) is
/// never queried for containers and never receives a stop request.
#[cfg(any(unix, test))]
const fn is_trusted_socket_owner(owner: u32, uid: u32) -> bool {
    owner == uid || owner == 0
}

/// Whether the socket at `path` (after following symlinks) is owned by `uid`
/// or by root; see [`is_trusted_socket_owner`].
#[cfg(unix)]
fn is_owned_by_trusted_user(path: &std::path::Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    match std::fs::metadata(path) {
        Ok(metadata) if is_trusted_socket_owner(metadata.uid(), uid) => true,
        Ok(metadata) => {
            debug!(
                "skipping socket owned by another user: path={} owner_uid={}",
                path.display(),
                metadata.uid()
            );
            false
        }
        Err(error) => {
            debug!(
                "skipping unusable socket candidate: path={} error={error}",
                path.display()
            );
            false
        }
    }
}

/// The default Unix daemon sockets that exist for this user, in priority
/// order, each real socket listed once.
///
/// Reads `XDG_RUNTIME_DIR` (and `TMPDIR` on macOS) from the environment; see
/// [`unix_socket_candidates`] for the order and [`existing_unique_sockets`]
/// for the filtering. A socket that is not owned by `uid` or by root is
/// skipped (see [`is_trusted_socket_owner`]). A `DOCKER_HOST=unix://` path
/// is the user's explicit choice and is not checked.
#[cfg(unix)]
pub fn unix_socket_paths(uid: u32, home: Option<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let xdg_runtime_dir = absolute_env_path("XDG_RUNTIME_DIR");
    let tmpdir = podman_machine_tmpdir();
    existing_unique_sockets(unix_socket_candidates(
        uid,
        home,
        xdg_runtime_dir.as_deref(),
        tmpdir.as_deref(),
    ))
    .into_iter()
    .filter(|path| is_owned_by_trusted_user(path, uid))
    .collect()
}

// ---------------------------------------------------------------------------
// Unix socket transport
// ---------------------------------------------------------------------------

/// Connect to a local Unix stream socket and apply `deadline` to all I/O.
///
/// std offers no connect timeout for `UnixStream`. On a local socket
/// `connect(2)` completes or fails immediately in practice: the kernel either
/// queues the connection or refuses it. The one blocking case (Linux, listener
/// backlog full) cannot be bounded with std alone because `SO_SNDTIMEO` would
/// have to be set before connecting. Callers still stay bounded: detection
/// workers are detached at the deadline by [`fetch_all`], and all
/// I/O after the connect runs under [`DeadlineStream`].
#[cfg(unix)]
pub fn connect_unix(
    path: &std::path::Path,
    deadline: Instant,
) -> io::Result<DeadlineStream<std::os::unix::net::UnixStream>> {
    std::os::unix::net::UnixStream::connect(path)
        .map(|stream| DeadlineStream::new(stream, deadline))
        .inspect_err(|error| {
            debug!(
                "failed to connect to container runtime socket: socket={} error={error}",
                path.display()
            );
        })
}

// ---------------------------------------------------------------------------
// Windows named pipe transport
// ---------------------------------------------------------------------------

#[cfg(windows)]
use std::{ffi::OsStr, ffi::c_void, os::windows::ffi::OsStrExt, os::windows::io::AsRawHandle};

#[cfg(windows)]
type RawHandle = *mut c_void;

#[cfg(windows)]
const ERROR_BROKEN_PIPE: i32 = 109;

#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

#[cfg(windows)]
const ERROR_PIPE_NOT_CONNECTED: i32 = 233;

/// First wait of a [`PipeStream`] read that finds no bytes in the pipe.
#[cfg(windows)]
const PIPE_POLL_MIN: Duration = Duration::from_micros(250);

/// Longest wait between two checks of an idle pipe; each wait doubles from
/// [`PIPE_POLL_MIN`] up to this.
#[cfg(windows)]
const PIPE_POLL_MAX: Duration = Duration::from_millis(10);

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn WaitNamedPipeW(name: *const u16, timeout: u32) -> i32;
    fn PeekNamedPipe(
        named_pipe: RawHandle,
        buffer: *mut c_void,
        buffer_size: u32,
        bytes_read: *mut u32,
        total_bytes_avail: *mut u32,
        bytes_left_this_message: *mut u32,
    ) -> i32;
}

/// Open the named pipe at `path` and apply `deadline` to all I/O.
///
/// When every instance of the pipe is busy, waits for one with
/// `WaitNamedPipeW`, but never past `deadline`.
#[cfg(windows)]
pub fn connect_pipe(path: &str, deadline: Instant) -> io::Result<PipeStream> {
    open_named_pipe(path, deadline).map(|file| PipeStream::new(file, deadline))
}

#[cfg(windows)]
fn open_named_pipe(path: &str, deadline: Instant) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;

    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(stream) => return Ok(stream),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                wait_named_pipe(path, deadline)?;
            }
            Err(error) => {
                debug!("failed to open container runtime named pipe: pipe={path} error={error}");
                return Err(error);
            }
        }
    }
}

/// A connected named pipe read as a blocking stream under one overall
/// deadline, so it goes through the same HTTP parser as sockets.
///
/// A synchronous pipe read has no timeout: a daemon that stops answering
/// would block it forever. Each read therefore peeks first and reads only
/// the bytes already in the pipe. While the pipe is empty it waits, starting
/// at [`PIPE_POLL_MIN`] and doubling up to [`PIPE_POLL_MAX`], so a prompt
/// reply is picked up within a fraction of a millisecond while an idle pipe
/// costs little CPU. Once the deadline has passed, every read and write
/// fails with [`io::ErrorKind::TimedOut`]. A pipe the server has closed
/// reads as EOF.
#[cfg(windows)]
pub struct PipeStream {
    file: std::fs::File,
    deadline: Instant,
}

#[cfg(windows)]
impl PipeStream {
    const fn new(file: std::fs::File, deadline: Instant) -> Self {
        Self { file, deadline }
    }

    fn remaining(&self) -> io::Result<Duration> {
        remaining_until(self.deadline).ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
    }
}

#[cfg(windows)]
impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut wait = PIPE_POLL_MIN;
        loop {
            // Checked before every peek, so a daemon that streams without
            // pause cannot hold the request open past the deadline.
            let remaining = self.remaining()?;
            match peek_available_bytes(&self.file) {
                Ok(0) => {}
                Ok(available) => {
                    let len = usize::try_from(available).map_or(buf.len(), |n| n.min(buf.len()));
                    return closed_pipe_as_eof(self.file.read(&mut buf[..len]));
                }
                Err(error) => return closed_pipe_as_eof(Err(error)),
            }
            std::thread::sleep(wait.min(remaining));
            wait = wait.saturating_mul(2).min(PIPE_POLL_MAX);
        }
    }
}

#[cfg(windows)]
impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.remaining()?;
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Report a pipe the server has closed as EOF (`Ok(0)`).
#[cfg(windows)]
fn closed_pipe_as_eof(result: io::Result<usize>) -> io::Result<usize> {
    match result {
        Err(error) if is_pipe_closed_code(error.raw_os_error()) => Ok(0),
        other => other,
    }
}

/// Wait until a busy pipe instance is free, failing with the OS error (or
/// [`io::ErrorKind::TimedOut`] once the deadline has passed).
#[cfg(windows)]
fn wait_named_pipe(path: &str, deadline: Instant) -> io::Result<()> {
    let timeout_ms =
        remaining_timeout_ms(deadline).ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
    let wide_path = wide_string(path);
    // SAFETY: `wide_path` is a valid null-terminated UTF-16 string produced by
    // `wide_string`, and `timeout_ms` is a plain u32. No aliasing or lifetime
    // invariants apply; the kernel copies the string internally.
    let success = unsafe { WaitNamedPipeW(wide_path.as_ptr(), timeout_ms) };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The number of bytes waiting in the pipe, or the OS error of the peek.
#[cfg(windows)]
fn peek_available_bytes(stream: &std::fs::File) -> io::Result<u32> {
    let mut available = 0;
    // SAFETY: `stream` is an open named-pipe file whose raw handle is valid
    // for the lifetime of this call. We pass null for all output pointers
    // except `total_bytes_avail`, which points to a stack-local u32. The
    // zero-length buffer and null `bytes_read` pointer tell the kernel we
    // only want the available-byte count, not actual data.
    let success = unsafe {
        PeekNamedPipe(
            stream.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &raw mut available,
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

/// Milliseconds left before `deadline`, or `None` once it has passed.
///
/// Never returns 0: `WaitNamedPipeW` reads a zero timeout as
/// `NMPWAIT_USE_DEFAULT_WAIT` (the pipe's default, typically 50ms), not as
/// "do not wait".
#[cfg(windows)]
fn remaining_timeout_ms(deadline: Instant) -> Option<u32> {
    deadline
        .checked_duration_since(Instant::now())
        .map(pipe_wait_timeout_ms)
}

/// Convert a remaining duration to a `WaitNamedPipeW` timeout in the range
/// `1..=u32::MAX` milliseconds.
#[cfg(windows)]
fn pipe_wait_timeout_ms(remaining: Duration) -> u32 {
    u32::try_from(remaining.as_millis())
        .unwrap_or(u32::MAX)
        .max(1)
}

#[cfg(windows)]
fn wide_string(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Whether a Windows error code means the server closed its end of the pipe.
#[cfg(windows)]
const fn is_pipe_closed_code(code: Option<i32>) -> bool {
    matches!(code, Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED))
}

// ---------------------------------------------------------------------------
// TCP transport
// ---------------------------------------------------------------------------

/// Upper bound for one TCP connect attempt.
///
/// A stop request has a long overall deadline ([`STOP_TIMEOUT`]) to cover
/// the grace period, but connecting should be fast. Capping each attempt
/// keeps one unreachable address from using up the whole deadline before
/// the other resolved addresses are tried.
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound for one connect attempt to a loopback address.
///
/// A loopback connect succeeds or is refused at once, except on Windows,
/// which retries a refused connect for about 2 seconds. Without this cap a
/// stale `DOCKER_HOST=tcp://127.0.0.1:2375` would spend those seconds on
/// every detection and stop.
const LOOPBACK_CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(250);

/// Upper bound for one connect attempt to `socket_addr`: short for loopback
/// addresses (`127.0.0.0/8`, `::1`, and `::ffff:127.0.0.0/104`), longer for
/// any other address.
const fn connect_attempt_cap(socket_addr: &std::net::SocketAddr) -> Duration {
    if socket_addr.ip().to_canonical().is_loopback() {
        LOOPBACK_CONNECT_ATTEMPT_TIMEOUT
    } else {
        CONNECT_ATTEMPT_TIMEOUT
    }
}

/// Connect to a Docker/Podman daemon over plain TCP (`DOCKER_HOST` set to
/// `tcp://host:port`) and apply `deadline` to all I/O.
pub fn connect_tcp(
    addr: &str,
    deadline: Instant,
) -> io::Result<DeadlineStream<std::net::TcpStream>> {
    connect_tcp_stream(addr, deadline).map(|stream| DeadlineStream::new(stream, deadline))
}

/// Connect to the first reachable address that `addr` resolves to.
///
/// The connect attempts share `deadline`, and each one is further capped
/// by [`connect_attempt_cap`]. Name resolution itself has no timeout in
/// std; `DOCKER_HOST` normally names a literal IP or `localhost`, which
/// resolve locally. Fails with the error of the last attempt, or with
/// [`io::ErrorKind::TimedOut`] once the deadline has passed.
fn connect_tcp_stream(addr: &str, deadline: Instant) -> io::Result<std::net::TcpStream> {
    use std::net::ToSocketAddrs;

    let socket_addrs = addr.to_socket_addrs().inspect_err(|error| {
        debug!("failed to resolve container runtime TCP address: tcp={addr} error={error}");
    })?;

    let mut last_error = io::Error::new(
        io::ErrorKind::NotFound,
        "the address resolved to no socket address",
    );
    for socket_addr in socket_addrs {
        let Some(remaining) = remaining_until(deadline) else {
            debug!("container runtime TCP connect deadline expired: tcp={addr}");
            return Err(io::ErrorKind::TimedOut.into());
        };
        let attempt_timeout = remaining.min(connect_attempt_cap(&socket_addr));
        match std::net::TcpStream::connect_timeout(&socket_addr, attempt_timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => {
                debug!(
                    "failed to connect to container runtime TCP address: socket_addr={socket_addr} error={error}"
                );
                last_error = error;
            }
        }
    }

    Err(last_error)
}

// ---------------------------------------------------------------------------
// Container list
// ---------------------------------------------------------------------------

/// A connected daemon stream of any transport, with its deadline applied.
pub trait Stream: Read + Write {}

impl<T: Read + Write> Stream for T {}

/// Connect with `connect` and fetch the container list JSON body, all
/// before `deadline`.
///
/// `connect` opens a socket, pipe, or TCP connection that already enforces
/// the deadline it is given ([`connect_tcp`], `connect_unix`,
/// `connect_pipe`).
pub fn fetch_json<S, C>(connect: C, deadline: Instant) -> Result<String, FetchError>
where
    S: Read + Write,
    C: FnOnce(Instant) -> io::Result<S>,
{
    let mut stream = connect(deadline).map_err(FetchError::from_connect_error)?;
    http::send_http_request(&mut stream).map_err(FetchError::from)
}

// ---------------------------------------------------------------------------
// Container stop / kill transport
// ---------------------------------------------------------------------------

/// Grace period, in seconds, sent as `?t=` on graceful stop requests.
///
/// The daemon sends SIGTERM, waits this long, then sends SIGKILL.
pub const STOP_GRACE_SECS: u64 = 10;

/// Overall deadline for one stop or kill request.
///
/// The grace period plus a margin for the ping preflight, the SIGKILL and
/// the HTTP reply, so a stop that runs the full grace period is still
/// reported correctly.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(STOP_GRACE_SECS + 10);

/// Upper bound for the `GET /_ping` preflight sent before a stop request.
///
/// A live daemon answers the ping at once; a forwarder whose backend is
/// down closes the connection or never answers. The ping also never runs
/// past the stop deadline.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

// Compile-time guard: the preflight plus one stop connect attempt must fit in
// the margin past the grace period, so a stop that uses the full grace period
// is not cut short.
const _: () = assert!(
    PING_TIMEOUT.as_secs() + CONNECT_ATTEMPT_TIMEOUT.as_secs()
        < STOP_TIMEOUT.as_secs() - STOP_GRACE_SECS
);

/// Result of sending a stop or kill request to one daemon endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAttempt {
    /// The stop request was never sent: the endpoint could not be reached,
    /// it did not answer the `GET /_ping` preflight with a 2xx status (for
    /// example a forwarder whose backend is down), or writing the stop
    /// request failed. Other endpoints may safely be tried.
    Unreachable,
    /// The stop request was fully written but no usable reply arrived: the
    /// connection was closed or reset, the deadline passed, or the reply
    /// was partial or malformed. That daemon may still be acting on the
    /// request, so no other endpoint should be tried.
    NoResponse,
    /// The daemon replied with this HTTP status code.
    Status(u16),
}

/// Deadline for the ping preflight of a stop request due by `stop_deadline`.
fn ping_deadline(stop_deadline: Instant) -> Instant {
    stop_deadline.min(Instant::now() + PING_TIMEOUT)
}

/// Whether the ping preflight got a 2xx reply.
///
/// Every failure, including a reply that is partial or malformed, counts as
/// "not answered", and so does any other status: a 400 from a TLS port or a
/// web server, or a 5xx from a forwarder whose backend is broken, means no
/// working daemon is behind the endpoint. The stop request has not been sent
/// yet, so skipping this endpoint can never cause a second daemon to act on
/// the same container.
fn ping_answered(result: Result<u16, http::StatusFailure>) -> bool {
    match result {
        Ok(status_code) if (200..300).contains(&status_code) => {
            debug!("container runtime answered ping: status={status_code}");
            true
        }
        Ok(status_code) => {
            debug!("container runtime ping failed, skipping stop: status={status_code}");
            false
        }
        Err(failure) => {
            debug!("container runtime did not answer ping, skipping stop: failure={failure:?}");
            false
        }
    }
}

/// Map the result of a stop POST to a [`StopAttempt`].
///
/// Only a request that was never written is [`StopAttempt::Unreachable`].
/// Once it is written, a daemon may have read it, so every failure fails
/// closed as [`StopAttempt::NoResponse`].
fn stop_attempt_from(result: Result<u16, http::StatusFailure>) -> StopAttempt {
    match result {
        Ok(status_code) => StopAttempt::Status(status_code),
        Err(http::StatusFailure::NotSent) => {
            debug!("failed to write stop request to container runtime");
            StopAttempt::Unreachable
        }
        Err(http::StatusFailure::NoReply) => {
            debug!("container runtime received stop request but gave no usable reply");
            StopAttempt::NoResponse
        }
    }
}

/// Send `POST {endpoint}` to stop or kill a container, connecting with
/// `connect`, and classify the result.
///
/// The daemon must first answer a `GET /_ping` preflight on its own
/// connection with a 2xx status, within [`PING_TIMEOUT`]; otherwise the
/// stop is not sent.
/// The ping and the stop request share `deadline`.
pub fn stop_via<S, C>(connect: C, endpoint: &str, deadline: Instant) -> StopAttempt
where
    S: Read + Write,
    C: Fn(Instant) -> io::Result<S>,
{
    let ping_deadline = ping_deadline(deadline);
    let pinged = connect(ping_deadline).is_ok_and(|mut stream| {
        ping_answered(http::send_http_status_request(
            &mut stream,
            http::PING_HTTP_REQUEST,
        ))
    });
    if !pinged {
        return StopAttempt::Unreachable;
    }
    let Ok(mut stream) = connect(deadline) else {
        return StopAttempt::Unreachable;
    };
    stop_attempt_from(http::send_http_post_status(&mut stream, endpoint))
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use super::*;

    /// Fetch the container list from the TCP daemon at `addr`.
    fn fetch_tcp(addr: &str, deadline: Instant) -> Result<String, FetchError> {
        fetch_json(|deadline| connect_tcp(addr, deadline), deadline)
    }

    /// Send a stop request to the TCP daemon at `addr`.
    fn stop_tcp(addr: &str, endpoint: &str, deadline: Instant) -> StopAttempt {
        stop_via(|deadline| connect_tcp(addr, deadline), endpoint, deadline)
    }

    /// Bind a loopback listener and return it with its `host:port` string.
    fn loopback_listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener address").to_string();
        (listener, addr)
    }

    /// Read until the end of the request headers (or the peer gives up) and
    /// return what was read.
    fn drain_request(stream: &mut impl Read) -> Vec<u8> {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => request.push(byte[0]),
                _ => break,
            }
        }
        request
    }

    /// How a [`TestDaemon`] answers one connection, given the request head
    /// it read. Returning without writing closes the connection unanswered.
    type Respond = fn(&mut TcpStream, &[u8]);

    /// Loopback daemon that accepts every connection until
    /// [`TestDaemon::finish`], records each request head, and answers it with
    /// a [`Respond`] function.
    struct TestDaemon {
        addr: String,
        done: Arc<AtomicBool>,
        handle: JoinHandle<Vec<Vec<u8>>>,
    }

    impl TestDaemon {
        fn start(respond: Respond) -> Self {
            let (listener, addr) = loopback_listener();
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let done = Arc::new(AtomicBool::new(false));
            let stop = Arc::clone(&done);
            let handle = std::thread::spawn(move || {
                let mut requests = Vec::new();
                loop {
                    // Read the flag before accepting so a connection queued
                    // before `finish` is still served and recorded.
                    let finishing = stop.load(Ordering::SeqCst);
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).expect("blocking stream");
                            let request = drain_request(&mut stream);
                            respond(&mut stream, &request);
                            requests.push(request);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock && !finishing => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => return requests,
                    }
                }
            });
            Self { addr, done, handle }
        }

        /// Stop accepting and return the request lines received, in order.
        fn finish(self) -> Vec<String> {
            self.done.store(true, Ordering::SeqCst);
            self.handle
                .join()
                .expect("test daemon thread")
                .iter()
                .map(|request| {
                    String::from_utf8_lossy(request)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        }
    }

    const PING_REQUEST_LINE: &str = "GET /_ping HTTP/1.0";

    fn is_ping(request: &[u8]) -> bool {
        request.starts_with(PING_REQUEST_LINE.as_bytes())
    }

    /// Answer a ping like dockerd does; leave any other request unanswered.
    fn answer_ping_only(stream: &mut TcpStream, request: &[u8]) {
        if is_ping(request) {
            drop(stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nOK"));
        }
    }

    /// Answer the ping, then reply to the stop request with 204.
    fn answer_ping_then_204(stream: &mut TcpStream, request: &[u8]) {
        if is_ping(request) {
            answer_ping_only(stream, request);
        } else {
            drop(stream.write_all(b"HTTP/1.0 204 No Content\r\n\r\n"));
        }
    }

    /// Keep the connection open, without answering, until the client closes
    /// it: a daemon that went silent, with no sleep the test must outlast.
    fn hold_until_client_closes(stream: &mut impl Read) {
        let mut byte = [0_u8; 1];
        while matches!(stream.read(&mut byte), Ok(read) if read > 0) {}
    }

    /// Whether any recorded request line is a POST (a stop or kill).
    fn any_post(requests: &[String]) -> bool {
        requests.iter().any(|line| line.starts_with("POST "))
    }

    // ── Unix socket discovery ────────────────────────────────────────

    #[test]
    fn unix_socket_candidates_follow_documented_priority_order() {
        let home = PathBuf::from("/home/tester");
        let xdg = PathBuf::from("/xdg/runtime");
        let tmpdir = PathBuf::from("/var/folders/xy/T");

        let paths = unix_socket_candidates(1000, Some(home.clone()), Some(&xdg), Some(&tmpdir));

        let expected: Vec<PathBuf> = [
            PathBuf::from("/var/run/docker.sock"),
            xdg.join("docker.sock"),
            PathBuf::from("/run/user/1000/docker.sock"),
            xdg.join("podman/podman.sock"),
            PathBuf::from("/run/user/1000/podman/podman.sock"),
            PathBuf::from("/run/podman/podman.sock"),
            home.join(".docker/desktop/docker.sock"),
            home.join(".docker/run/docker.sock"),
            home.join(".colima/default/docker.sock"),
            home.join(".colima/docker.sock"),
            home.join(".orbstack/run/docker.sock"),
            home.join(".rd/docker.sock"),
            home.join(".lima/default/sock/docker.sock"),
            home.join(".lima/docker/sock/docker.sock"),
            home.join(".local/share/containers/podman/machine/podman.sock"),
            home.join(".local/share/containers/podman/machine/qemu/podman.sock"),
            home.join(".local/share/containers/podman/machine/podman-machine-default/podman.sock"),
            tmpdir.join("podman/podman-machine-default-api.sock"),
        ]
        .into();
        assert_eq!(paths, expected, "candidates must keep the documented order");
    }

    #[test]
    fn unix_socket_candidates_list_matching_xdg_runtime_dir_once() {
        let xdg = PathBuf::from("/run/user/1000/");

        let paths = unix_socket_candidates(1000, None, Some(&xdg), None);

        assert_eq!(
            paths,
            [
                "/var/run/docker.sock",
                "/run/user/1000/docker.sock",
                "/run/user/1000/podman/podman.sock",
                "/run/podman/podman.sock",
            ]
            .map(PathBuf::from),
            "XDG_RUNTIME_DIR equal to /run/user/{{uid}} must not be listed twice"
        );
    }

    #[test]
    fn unix_socket_candidates_without_env_or_home_keep_system_paths() {
        let paths = unix_socket_candidates(501, None, None, None);

        assert_eq!(
            paths,
            [
                "/var/run/docker.sock",
                "/run/user/501/docker.sock",
                "/run/user/501/podman/podman.sock",
                "/run/podman/podman.sock",
            ]
            .map(PathBuf::from),
            "without home, XDG_RUNTIME_DIR or TMPDIR only the system paths remain"
        );
    }

    #[test]
    fn existing_unique_sockets_skips_missing_files_and_keeps_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = dir.path().join("first.sock");
        let second = dir.path().join("second.sock");
        std::fs::write(&first, b"").expect("create first");
        std::fs::write(&second, b"").expect("create second");

        let kept = existing_unique_sockets(vec![
            dir.path().join("missing.sock"),
            second.clone(),
            dir.path().join("missing-dir/docker.sock"),
            first.clone(),
        ]);

        assert_eq!(
            kept,
            vec![second, first],
            "missing candidates are dropped and the rest keep their order"
        );
    }

    #[test]
    fn existing_unique_sockets_queries_one_file_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(dir.path().join("sub")).expect("create sub dir");
        let socket = dir.path().join("podman.sock");
        std::fs::write(&socket, b"").expect("create socket file");
        let other_spelling = dir.path().join("sub/../podman.sock");

        let kept = existing_unique_sockets(vec![socket.clone(), other_spelling]);

        assert_eq!(
            kept,
            vec![socket],
            "two spellings of one file must be queried once, at the first position"
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_unique_sockets_follows_symlinks_and_drops_dangling_ones() {
        let dir = tempfile::tempdir().expect("temp dir");
        let podman = dir.path().join("podman.sock");
        std::fs::write(&podman, b"").expect("create podman socket file");
        let docker = dir.path().join("docker.sock");
        std::os::unix::fs::symlink(&podman, &docker).expect("symlink docker.sock");
        let dangling = dir.path().join("dangling.sock");
        std::os::unix::fs::symlink(dir.path().join("gone.sock"), &dangling)
            .expect("symlink dangling.sock");

        let kept = existing_unique_sockets(vec![dangling, docker.clone(), podman]);

        assert_eq!(
            kept,
            vec![docker],
            "docker.sock symlinked to podman.sock is one daemon, and a dangling link is skipped"
        );
    }

    #[test]
    fn only_the_user_and_root_own_trusted_sockets() {
        assert!(is_trusted_socket_owner(1000, 1000), "the user's own socket");
        assert!(
            is_trusted_socket_owner(0, 1000),
            "a root-owned system socket"
        );
        assert!(
            !is_trusted_socket_owner(1001, 1000),
            "a socket another user planted must be skipped"
        );
        assert!(is_trusted_socket_owner(0, 0), "root trusts its own sockets");
        assert!(
            !is_trusted_socket_owner(1000, 0),
            "root does not trust an unprivileged user's socket"
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_owner_check_reads_the_file_owner() {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("docker.sock");
        std::fs::write(&socket, b"").expect("create socket file");
        // SAFETY: getuid() is a simple syscall with no preconditions.
        let uid = unsafe { libc::getuid() };

        assert!(
            is_owned_by_trusted_user(&socket, uid),
            "a socket the current user created is used"
        );
        if uid != 0 {
            assert!(
                !is_owned_by_trusted_user(&socket, uid + 1),
                "a socket owned by neither the user nor root is skipped"
            );
        }
        assert!(
            !is_owned_by_trusted_user(&dir.path().join("missing.sock"), uid),
            "a missing socket is skipped"
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn tmpdir_is_not_searched_outside_macos() {
        assert!(
            podman_machine_tmpdir().is_none(),
            "a shared /tmp must never contribute a socket candidate"
        );
    }

    // ── DOCKER_HOST parsing ──────────────────────────────────────────

    #[test]
    fn docker_host_tcp_addr_accepts_docker_cli_forms() {
        for (value, expected) in [
            ("tcp://127.0.0.1:2375", "127.0.0.1:2375"),
            ("tcp://localhost", "localhost:2375"),
            ("tcp://10.0.0.5", "10.0.0.5:2375"),
            ("tcp://docker.example:", "docker.example:2375"),
            ("tcp://127.0.0.1:2375/", "127.0.0.1:2375"),
            ("tcp://127.0.0.1:2375/v1.43/api", "127.0.0.1:2375"),
            ("tcp://host/", "host:2375"),
            ("  tcp://127.0.0.1:2375\n", "127.0.0.1:2375"),
            ("TCP://127.0.0.1:2376", "127.0.0.1:2376"),
            ("Tcp://Host:1", "Host:1"),
            ("tcp://[::1]:2375", "[::1]:2375"),
            ("tcp://[::1]", "[::1]:2375"),
            ("tcp://[fe80::1]:2376/", "[fe80::1]:2376"),
        ] {
            assert_eq!(
                docker_host_tcp_addr(value).as_deref(),
                Some(expected),
                "{value:?} is a valid tcp:// DOCKER_HOST"
            );
        }
    }

    #[test]
    fn docker_host_tcp_addr_rejects_malformed_addresses_and_other_schemes() {
        for value in [
            "tcp://",
            "tcp://:2375",
            "tcp:///path",
            "tcp://host:port",
            "tcp://host:70000",
            "tcp://::1:2375",
            "tcp://[::1",
            "tcp://[::1]2375",
            "tcp://[not-an-ip]:2375",
            "ssh://user@host",
            "fd://",
            "http://127.0.0.1:2375",
            "unix:///var/run/docker.sock",
            "npipe:////./pipe/docker_engine",
            "127.0.0.1:2375",
            "",
        ] {
            assert_eq!(
                docker_host_tcp_addr(value),
                None,
                "{value:?} is no tcp:// address"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn docker_host_unix_path_parses_only_unix_values() {
        assert_eq!(
            docker_host_unix_path("unix:///run/docker.sock").as_deref(),
            Some("/run/docker.sock")
        );
        assert_eq!(
            docker_host_unix_path(" UNIX:///run/docker.sock ").as_deref(),
            Some("/run/docker.sock"),
            "whitespace and the scheme's letter case do not matter"
        );
        assert_eq!(docker_host_unix_path("unix://"), None);
        assert_eq!(docker_host_unix_path("tcp://127.0.0.1:2375"), None);
    }

    #[test]
    fn pipe_paths_must_name_a_pipe() {
        for path in [
            r"\\.\pipe\docker_engine",
            r"\\.\PIPE\podman-machine-default",
            r"\\buildhost\pipe\docker_engine",
        ] {
            assert!(is_pipe_path(path), "{path} names a pipe");
        }
        for path in [
            r"C:\x.txt",
            r"\\.\C:\x.txt",
            r"\\.\pipe\",
            r"\\.\pipe",
            r"\\\pipe\docker_engine",
            r"\\.\pipes\docker_engine",
            r"\\.\pipe\nested\name",
            r"\.\pipe\docker_engine",
            r"\\?\C:\x.txt",
            "",
        ] {
            assert!(!is_pipe_path(path), "{path} must not be opened as a pipe");
        }
    }

    #[cfg(windows)]
    #[test]
    fn docker_host_npipe_path_accepts_only_pipe_paths() {
        for (value, expected) in [
            ("npipe:////./pipe/docker_engine", r"\\.\pipe\docker_engine"),
            (r"npipe://\\.\pipe\docker_engine", r"\\.\pipe\docker_engine"),
            (
                " NPIPE:////./pipe/docker_engine ",
                r"\\.\pipe\docker_engine",
            ),
            (
                "npipe:////buildhost/pipe/docker_engine",
                r"\\buildhost\pipe\docker_engine",
            ),
        ] {
            assert_eq!(
                docker_host_npipe_path(value).as_deref(),
                Some(expected),
                "{value:?}"
            );
        }
        for value in [
            "npipe://",
            "npipe://C:/x.txt",
            "npipe:///C:/x.txt",
            "npipe:////./C:/x.txt",
            "npipe://./pipe/docker_engine",
            "npipe:////./pipe/",
            "tcp://127.0.0.1:2375",
        ] {
            assert_eq!(
                docker_host_npipe_path(value),
                None,
                "{value:?} names no pipe"
            );
        }
    }

    // ── fetch_all ────────────────────────────────────────────────────

    #[test]
    fn fetch_all_collects_every_result() {
        let fan_out = fetch_all(
            [1_u8, 2, 3],
            |candidate| (candidate != 2).then(|| candidate.to_string()),
            Instant::now() + Duration::from_secs(5),
        );
        let mut responses: Vec<_> = fan_out.results.into_iter().flatten().collect();
        responses.sort();

        assert_eq!(
            responses,
            vec!["1".to_string(), "3".to_string()],
            "every successful candidate should be returned"
        );
        assert_eq!(fan_out.unfinished, 0, "every worker reported back");
    }

    #[test]
    fn fetch_all_reports_workers_that_cannot_start() {
        let attempts = std::cell::Cell::new(0_usize);
        let fan_out = fetch_all_with(
            [1_u8, 2, 3],
            |candidate| candidate,
            Instant::now() + Duration::from_secs(5),
            |work| {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 2 {
                    Err(io::Error::other("no threads left"))
                } else {
                    spawn_detached("nanodock-test", work)
                }
            },
        );
        let mut results = fan_out.results;
        results.sort_unstable();

        assert_eq!(results, vec![1, 3], "the other candidates still run");
        assert_eq!(
            fan_out.unfinished, 0,
            "a worker that never started is not waited for"
        );
        let failed: Vec<usize> = fan_out
            .spawn_failures
            .iter()
            .map(|(position, _)| *position)
            .collect();
        assert_eq!(
            failed,
            vec![1],
            "the failure names the candidate's position"
        );
    }

    #[test]
    fn fetch_all_workers_are_named() {
        let fan_out = fetch_all(
            [()],
            |()| std::thread::current().name().map(str::to_string),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(fan_out.results, vec![Some("nanodock-query".to_string())]);
    }

    #[test]
    fn fetch_all_returns_early_when_all_workers_finish() {
        let started = Instant::now();
        let fan_out = fetch_all(
            [1_u8, 2],
            |candidate| {
                std::thread::sleep(Duration::from_millis(20));
                candidate
            },
            started + Duration::from_secs(10),
        );

        assert_eq!(
            fan_out.results.len(),
            2,
            "both late-ish answers should be kept"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the collector should not wait for the deadline once all workers are done"
        );
    }

    #[test]
    fn fetch_all_does_not_wait_for_stuck_workers() {
        // The stuck worker blocks until released below (or for 10 seconds),
        // far past the 2 second deadline the fast worker easily meets.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = std::sync::Mutex::new(released);
        let started = Instant::now();
        let fan_out = fetch_all(
            [false, true],
            move |stuck| {
                if stuck {
                    let released = released.lock().expect("release lock");
                    let _woken = released.recv_timeout(Duration::from_secs(10));
                }
                stuck
            },
            started + Duration::from_secs(2),
        );
        let elapsed = started.elapsed();
        drop(release);

        assert_eq!(
            fan_out.results,
            vec![false],
            "the fast answer must survive a stuck sibling"
        );
        assert_eq!(fan_out.unfinished, 1, "the stuck worker is counted");
        assert!(
            elapsed < Duration::from_secs(5),
            "a stuck worker must be detached, not joined, took {elapsed:?}"
        );
    }

    // ── DeadlineStream / TCP ─────────────────────────────────────────

    #[test]
    fn tcp_fetch_reads_complete_response() {
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            drop(stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n[]"));
        });

        let body = fetch_tcp(&addr, Instant::now() + Duration::from_secs(5)).ok();
        drop(server.join());

        assert_eq!(body.as_deref(), Some("[]"), "body should pass through");
    }

    #[test]
    fn tcp_fetch_reports_http_status() {
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            drop(stream.write_all(b"HTTP/1.0 403 Forbidden\r\nContent-Length: 0\r\n\r\n"));
        });

        let result = fetch_tcp(&addr, Instant::now() + Duration::from_secs(5));
        drop(server.join());

        assert!(
            matches!(result, Err(FetchError::Status(403))),
            "a non-2xx reply is reported with its status, got {result:?}"
        );
    }

    #[test]
    fn connect_errors_are_classified() {
        let classify = |kind: io::ErrorKind| FetchError::from_connect_error(kind.into());
        assert!(matches!(
            classify(io::ErrorKind::NotFound),
            FetchError::NotFound
        ));
        assert!(
            matches!(
                classify(io::ErrorKind::ConnectionRefused),
                FetchError::NotFound
            ),
            "a refused connection means no daemon listens there"
        );
        assert!(matches!(
            classify(io::ErrorKind::PermissionDenied),
            FetchError::PermissionDenied
        ));
        assert!(matches!(
            classify(io::ErrorKind::TimedOut),
            FetchError::Timeout
        ));
        assert!(matches!(
            classify(io::ErrorKind::ConnectionReset),
            FetchError::Io(_)
        ));
    }

    #[test]
    fn tcp_fetch_enforces_overall_deadline_against_trickling_daemon() {
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            // One byte every 20ms never trips a per-read timeout.
            for byte in b"HTTP/1.0 200 OK\r\nX-Slow: ".iter().cycle().take(500) {
                if stream.write_all(&[*byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });

        let started = Instant::now();
        let body = fetch_tcp(&addr, started + Duration::from_millis(200));
        let elapsed = started.elapsed();
        drop(server.join());

        assert!(
            matches!(body, Err(FetchError::Timeout)),
            "a trickled response must time out, got {body:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "the overall deadline must stop the read, took {elapsed:?}"
        );
    }

    #[test]
    fn connect_tcp_stream_respects_expired_deadline() {
        let (_listener, addr) = loopback_listener();
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap_or_else(Instant::now);

        let error = connect_tcp_stream(&addr, past).expect_err("deadline already passed");
        assert_eq!(
            error.kind(),
            io::ErrorKind::TimedOut,
            "no connect attempt should start after the deadline"
        );
    }

    #[test]
    fn deadline_stream_fails_with_timed_out_after_deadline() {
        let (listener, addr) = loopback_listener();
        let client = TcpStream::connect(&addr).expect("connect");
        let _server = listener.accept().expect("accept");
        let mut stream = DeadlineStream::new(client, Instant::now());
        let mut buf = [0_u8; 4];

        let error = stream.read(&mut buf).expect_err("read must fail");
        assert_eq!(
            error.kind(),
            io::ErrorKind::TimedOut,
            "an expired deadline should surface as TimedOut"
        );
    }

    // ── stop transport ───────────────────────────────────────────────

    #[test]
    fn stop_timeout_exceeds_grace_period() {
        assert!(
            STOP_TIMEOUT > Duration::from_secs(STOP_GRACE_SECS),
            "a stop that uses the full grace period must not be reported as failed"
        );
    }

    #[test]
    fn tcp_stop_reports_status_code_after_ping() {
        let daemon = TestDaemon::start(answer_ping_then_204);

        let attempt = stop_tcp(
            &daemon.addr,
            "/containers/abc/stop?t=10",
            Instant::now() + STOP_TIMEOUT,
        );
        let requests = daemon.finish();

        assert_eq!(
            attempt,
            StopAttempt::Status(204),
            "status should pass through"
        );
        assert_eq!(
            requests,
            vec![
                PING_REQUEST_LINE.to_string(),
                "POST /containers/abc/stop?t=10 HTTP/1.0".to_string()
            ],
            "the ping goes out on its own connection before the stop"
        );
    }

    #[test]
    fn tcp_stop_classifies_refused_connection_as_unreachable() {
        let (listener, addr) = loopback_listener();
        drop(listener);

        assert_eq!(
            stop_tcp(
                &addr,
                "/containers/abc/stop",
                Instant::now() + Duration::from_secs(5)
            ),
            StopAttempt::Unreachable,
            "a closed port never received the request"
        );
    }

    #[test]
    fn connect_attempts_to_loopback_are_capped_short() {
        let cap = |addr: &str| connect_attempt_cap(&addr.parse().expect("socket address"));
        for loopback in [
            "127.0.0.1:2375",
            "127.8.9.10:2375",
            "[::1]:2375",
            "[::ffff:127.0.0.1]:2375",
        ] {
            assert_eq!(
                cap(loopback),
                LOOPBACK_CONNECT_ATTEMPT_TIMEOUT,
                "{loopback}"
            );
        }
        for remote in [
            "10.0.0.5:2375",
            "192.168.1.20:2376",
            "[2001:db8::1]:2375",
            "[::2]:2375",
        ] {
            assert_eq!(cap(remote), CONNECT_ATTEMPT_TIMEOUT, "{remote}");
        }
    }

    #[test]
    fn refused_loopback_connect_fails_within_the_loopback_cap() {
        // Windows retries a refused connect for about 2 seconds; the cap
        // must end it long before that, well inside the 5 second deadline.
        let (listener, addr) = loopback_listener();
        drop(listener);
        let started = Instant::now();

        let result = connect_tcp_stream(&addr, started + Duration::from_secs(5));
        let elapsed = started.elapsed();

        assert!(result.is_err(), "nothing listens on {addr}");
        assert!(
            elapsed < Duration::from_secs(1),
            "a refused loopback connect must not take {elapsed:?}"
        );
    }

    #[test]
    fn tcp_stop_classifies_silent_daemon_as_no_response() {
        let daemon = TestDaemon::start(|stream, request| {
            if is_ping(request) {
                answer_ping_only(stream, request);
            } else {
                hold_until_client_closes(stream);
            }
        });

        // Long enough that the ping is always answered in time; the stop
        // then runs into the deadline.
        let attempt = stop_tcp(
            &daemon.addr,
            "/containers/abc/stop",
            Instant::now() + Duration::from_secs(2),
        );
        let requests = daemon.finish();

        assert_eq!(
            attempt,
            StopAttempt::NoResponse,
            "a daemon that received the request and timed out owns the outcome"
        );
        assert!(any_post(&requests), "the stop request was sent");
    }

    #[test]
    fn tcp_stop_classifies_accept_then_drop_as_unreachable() {
        // A forwarder (socat, SSH tunnel) whose backend is down accepts the
        // connection and closes it without sending a byte.
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            drop(stream);
        });

        let attempt = stop_tcp(
            &addr,
            "/containers/abc/stop",
            Instant::now() + Duration::from_secs(5),
        );
        drop(server.join());

        assert_eq!(
            attempt,
            StopAttempt::Unreachable,
            "a ping closed without a reply means no daemon is behind the endpoint"
        );
    }

    #[test]
    fn tcp_stop_never_sends_stop_when_ping_is_dropped() {
        // Reads every request, then closes the connection without a reply.
        let daemon = TestDaemon::start(|_, _| {});

        let attempt = stop_tcp(
            &daemon.addr,
            "/containers/abc/stop",
            Instant::now() + Duration::from_secs(5),
        );
        let requests = daemon.finish();

        assert_eq!(
            attempt,
            StopAttempt::Unreachable,
            "an unanswered ping means no daemon is behind the endpoint"
        );
        assert_eq!(
            requests,
            vec![PING_REQUEST_LINE.to_string()],
            "only the ping may be sent"
        );
        assert!(!any_post(&requests), "the stop must never be sent");
    }

    #[test]
    fn tcp_stop_never_sends_stop_when_ping_times_out() {
        let daemon = TestDaemon::start(|stream, _| hold_until_client_closes(stream));

        let attempt = stop_tcp(
            &daemon.addr,
            "/containers/abc/stop",
            Instant::now() + Duration::from_millis(300),
        );
        let requests = daemon.finish();

        assert_eq!(
            attempt,
            StopAttempt::Unreachable,
            "a ping that times out leaves the stop unsent"
        );
        assert!(!any_post(&requests), "the stop must never be sent");
    }

    /// Answer the ping with 400 like a TLS port given plain HTTP, and any
    /// other request with 204.
    fn answer_ping_400(stream: &mut TcpStream, request: &[u8]) {
        if is_ping(request) {
            drop(stream.write_all(b"HTTP/1.0 400 Bad Request\r\nContent-Length: 0\r\n\r\n"));
        } else {
            drop(stream.write_all(b"HTTP/1.0 204 No Content\r\n\r\n"));
        }
    }

    /// Answer the ping with 502 like a forwarder whose backend is broken,
    /// and any other request with 204.
    fn answer_ping_502(stream: &mut TcpStream, request: &[u8]) {
        if is_ping(request) {
            drop(stream.write_all(b"HTTP/1.0 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n"));
        } else {
            drop(stream.write_all(b"HTTP/1.0 204 No Content\r\n\r\n"));
        }
    }

    #[test]
    fn ping_counts_only_2xx_as_answered() {
        for status in [200, 204, 299] {
            assert!(ping_answered(Ok(status)), "{status} is a working daemon");
        }
        for status in [101, 199, 300, 400, 404, 500, 502, 503] {
            assert!(!ping_answered(Ok(status)), "{status} is no working daemon");
        }
        assert!(!ping_answered(Err(http::StatusFailure::NoReply)));
        assert!(!ping_answered(Err(http::StatusFailure::NotSent)));
    }

    #[test]
    fn tcp_stop_never_sends_stop_when_ping_is_not_2xx() {
        for (respond, status) in [(answer_ping_400 as Respond, 400), (answer_ping_502, 502)] {
            let daemon = TestDaemon::start(respond);

            let attempt = stop_tcp(
                &daemon.addr,
                "/containers/abc/stop",
                Instant::now() + Duration::from_secs(5),
            );
            let requests = daemon.finish();

            assert_eq!(
                attempt,
                StopAttempt::Unreachable,
                "a ping answered with {status} means no working daemon"
            );
            assert_eq!(
                requests,
                vec![PING_REQUEST_LINE.to_string()],
                "only the ping may be sent after a {status} ping"
            );
        }
    }

    #[test]
    fn stop_falls_through_non_2xx_ping_to_next_endpoint() {
        let tls_port = TestDaemon::start(answer_ping_400);
        let daemon = TestDaemon::start(answer_ping_then_204);

        let targets = [(true, tls_port.addr.clone()), (false, daemon.addr.clone())];
        let attempt = crate::first_stop_owner(targets, |addr| {
            stop_tcp(
                &addr,
                "/containers/web/stop?t=10",
                Instant::now() + STOP_TIMEOUT,
            )
        });
        let tls_requests = tls_port.finish();
        drop(daemon.finish());

        assert_eq!(
            crate::stop_outcome(attempt, crate::StopKind::Graceful),
            crate::StopOutcome::Stopped,
            "the daemon after the endpoint that failed the ping is used"
        );
        assert!(
            !any_post(&tls_requests),
            "the 400 endpoint never receives the stop"
        );
    }

    #[test]
    fn tcp_stop_classifies_close_after_request_as_no_response() {
        // dockerd starts the grace period, then crashes, or Go net/http
        // recovers a handler panic by closing the connection unanswered.
        let daemon = TestDaemon::start(answer_ping_only);

        let attempt = stop_tcp(
            &daemon.addr,
            "/containers/abc/stop",
            Instant::now() + Duration::from_secs(5),
        );
        let requests = daemon.finish();

        assert_eq!(
            attempt,
            StopAttempt::NoResponse,
            "a daemon that read the stop request may be acting on it"
        );
        assert!(any_post(&requests), "the stop request was received");
    }

    #[test]
    fn tcp_stop_classifies_partial_reply_as_no_response() {
        let daemon = TestDaemon::start(|stream, request| {
            if is_ping(request) {
                answer_ping_only(stream, request);
            } else {
                drop(stream.write_all(b"HTTP/1.0 20"));
            }
        });

        let attempt = stop_tcp(
            &daemon.addr,
            "/containers/abc/stop",
            Instant::now() + Duration::from_secs(5),
        );
        drop(daemon.finish());

        assert_eq!(
            attempt,
            StopAttempt::NoResponse,
            "a daemon that started replying may have acted on the request"
        );
    }

    #[test]
    fn stop_after_close_after_request_never_tries_next_endpoint() {
        for kind in [crate::StopKind::Graceful, crate::StopKind::Kill] {
            let first = TestDaemon::start(answer_ping_only);
            let second = TestDaemon::start(answer_ping_then_204);
            let endpoint = crate::stop_endpoint("web", kind);

            let targets = [(false, first.addr.clone()), (false, second.addr.clone())];
            let attempt = crate::first_stop_owner(targets, |addr| {
                stop_tcp(&addr, &endpoint, Instant::now() + STOP_TIMEOUT)
            });
            let first_requests = first.finish();
            let second_requests = second.finish();

            assert_eq!(
                crate::stop_outcome(attempt, kind),
                crate::StopOutcome::NoResponse,
                "an unanswered {kind:?} request has an unknown result"
            );
            assert!(
                any_post(&first_requests),
                "the first daemon got the {kind:?} request"
            );
            assert!(
                second_requests.is_empty(),
                "a same-named container on the second daemon must not be touched by {kind:?}"
            );
        }
    }

    #[test]
    fn stop_falls_through_dead_forwarder_to_next_endpoint() {
        let forwarder = TestDaemon::start(|_, _| {});
        let daemon = TestDaemon::start(answer_ping_then_204);

        let targets = [(true, forwarder.addr.clone()), (false, daemon.addr.clone())];
        let attempt = crate::first_stop_owner(targets, |addr| {
            stop_tcp(
                &addr,
                "/containers/web/stop?t=10",
                Instant::now() + STOP_TIMEOUT,
            )
        });
        let forwarder_requests = forwarder.finish();
        drop(daemon.finish());

        assert_eq!(
            crate::stop_outcome(attempt, crate::StopKind::Graceful),
            crate::StopOutcome::Stopped,
            "the live daemon behind the dead forwarder is used"
        );
        assert!(
            !any_post(&forwarder_requests),
            "the dead forwarder never receives the stop"
        );
    }

    #[cfg(unix)]
    fn unix_test_socket_path(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nanodock-{tag}-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        drop(std::fs::remove_file(&path));
        path
    }

    #[cfg(unix)]
    #[test]
    fn unix_stop_classifies_accept_then_drop_as_unreachable() {
        let path = unix_test_socket_path("stop-drop");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind unix socket");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            drop(stream);
        });

        let attempt = stop_via(
            |deadline| connect_unix(&path, deadline),
            "/containers/abc/stop",
            Instant::now() + STOP_TIMEOUT,
        );
        drop(server.join());
        drop(std::fs::remove_file(&path));

        assert_eq!(
            attempt,
            StopAttempt::Unreachable,
            "a socket that drops the ping has no daemon behind it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_stop_classifies_close_after_request_as_no_response() {
        let path = unix_test_socket_path("stop-close");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind unix socket");
        let server = std::thread::spawn(move || {
            let mut lines = Vec::new();
            // Exactly two connections: the ping, then the stop.
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let request = drain_request(&mut stream);
                if is_ping(&request) {
                    drop(stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nOK"));
                }
                lines.push(String::from_utf8_lossy(&request).into_owned());
            }
            lines
        });

        let attempt = stop_via(
            |deadline| connect_unix(&path, deadline),
            "/containers/abc/stop",
            Instant::now() + STOP_TIMEOUT,
        );
        let lines = server.join().expect("server thread");
        drop(std::fs::remove_file(&path));

        assert_eq!(
            attempt,
            StopAttempt::NoResponse,
            "a daemon that read the stop request may be acting on it"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("POST ")),
            "the stop request was received"
        );
    }

    // ── Windows named pipe ───────────────────────────────────────────

    #[test]
    fn connect_attempt_timeout_is_shorter_than_stop_timeout() {
        assert!(
            CONNECT_ATTEMPT_TIMEOUT < STOP_TIMEOUT,
            "one unreachable address must not use up the whole stop deadline"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pipe_wait_timeout_ms_never_returns_zero() {
        assert_eq!(
            pipe_wait_timeout_ms(Duration::ZERO),
            1,
            "0 means NMPWAIT_USE_DEFAULT_WAIT to WaitNamedPipeW"
        );
        assert_eq!(
            pipe_wait_timeout_ms(Duration::from_micros(500)),
            1,
            "sub-millisecond budgets must not round down to 0"
        );
        assert_eq!(
            pipe_wait_timeout_ms(Duration::from_secs(5)),
            5000,
            "longer budgets are passed through"
        );
        assert_eq!(
            pipe_wait_timeout_ms(Duration::from_secs(u64::MAX)),
            u32::MAX,
            "huge budgets saturate"
        );
    }

    #[cfg(windows)]
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout: u32,
            security_attributes: *mut c_void,
        ) -> RawHandle;
        fn ConnectNamedPipe(named_pipe: RawHandle, overlapped: *mut c_void) -> i32;
    }

    /// `PIPE_ACCESS_DUPLEX`: the server instance reads and writes.
    #[cfg(windows)]
    const PIPE_ACCESS_DUPLEX: u32 = 3;

    /// `ERROR_PIPE_CONNECTED`: a client connected before `ConnectNamedPipe`.
    #[cfg(windows)]
    const ERROR_PIPE_CONNECTED: i32 = 535;

    /// A pipe path no other test uses.
    #[cfg(windows)]
    fn test_pipe_path(tag: &str) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        format!(
            r"\\.\pipe\nanodock-test-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// Create one byte-mode server instance of the pipe at `path`.
    #[cfg(windows)]
    fn create_pipe_instance(path: &str, max_instances: u32) -> std::fs::File {
        use std::os::windows::io::FromRawHandle;

        let wide_path = wide_string(path);
        // SAFETY: `wide_path` is a valid null-terminated UTF-16 string, the
        // sizes are plain integers, and null security attributes select the
        // default descriptor.
        let handle = unsafe {
            CreateNamedPipeW(
                wide_path.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                0,
                max_instances,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            handle as isize,
            -1,
            "CreateNamedPipeW failed: {}",
            io::Error::last_os_error()
        );
        // SAFETY: `handle` is a valid pipe handle that nothing else owns.
        unsafe { std::fs::File::from_raw_handle(handle) }
    }

    /// Block until a client connects to the server instance.
    #[cfg(windows)]
    fn accept_pipe_client(instance: &std::fs::File) {
        // SAFETY: `instance` is an open pipe server handle, and a null
        // `overlapped` pointer asks for a blocking call.
        let connected = unsafe { ConnectNamedPipe(instance.as_raw_handle(), std::ptr::null_mut()) };
        assert!(
            connected != 0
                || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED),
            "ConnectNamedPipe failed: {}",
            io::Error::last_os_error()
        );
    }

    /// Named pipe stand-in for a daemon, like [`TestDaemon`]: serves up to
    /// `connections` clients in order, answers each request head with a
    /// [`PipeRespond`] function, and records the request lines.
    #[cfg(windows)]
    struct PipeDaemon {
        path: String,
        handle: JoinHandle<Vec<String>>,
    }

    /// How a [`PipeDaemon`] answers one connection, given the request head.
    #[cfg(windows)]
    type PipeRespond = fn(&mut std::fs::File, &[u8]);

    #[cfg(windows)]
    impl PipeDaemon {
        fn start(connections: usize, respond: PipeRespond) -> Self {
            let path = test_pipe_path("daemon");
            // Created before returning, so the first client finds the pipe.
            let mut instance = create_pipe_instance(&path, 255);
            let server_path = path.clone();
            let handle = std::thread::spawn(move || {
                let mut lines = Vec::new();
                for served in 1..=connections {
                    accept_pipe_client(&instance);
                    // Listen for the next client before answering, so a
                    // client that reconnects right after the reply finds it.
                    let next =
                        (served < connections).then(|| create_pipe_instance(&server_path, 255));
                    let request = drain_request(&mut instance);
                    respond(&mut instance, &request);
                    // FlushFileBuffers: wait until the client read the reply.
                    drop(instance.sync_all());
                    let line = String::from_utf8_lossy(&request);
                    lines.push(line.lines().next().unwrap_or_default().to_string());
                    match next {
                        Some(next) => instance = next,
                        None => break,
                    }
                }
                lines
            });
            Self { path, handle }
        }

        /// Release any instance still waiting for a client and return the
        /// request lines received, in order (empty heads are dropped).
        fn finish(self) -> Vec<String> {
            while !self.handle.is_finished() {
                // An empty connection lets a waiting instance move on.
                drop(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&self.path),
                );
                std::thread::yield_now();
            }
            let mut lines = self.handle.join().expect("pipe daemon thread");
            lines.retain(|line| !line.is_empty());
            lines
        }
    }

    #[cfg(windows)]
    fn pipe_answer_ping_only(stream: &mut std::fs::File, request: &[u8]) {
        if is_ping(request) {
            drop(stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nOK"));
        }
    }

    /// Fetch the container list from the named pipe at `path`.
    #[cfg(windows)]
    fn fetch_pipe(path: &str, deadline: Instant) -> Result<String, FetchError> {
        fetch_json(|deadline| connect_pipe(path, deadline), deadline)
    }

    /// Send a stop request to the named pipe at `path`.
    #[cfg(windows)]
    fn stop_pipe(path: &str, endpoint: &str) -> StopAttempt {
        stop_via(
            |deadline| connect_pipe(path, deadline),
            endpoint,
            Instant::now() + STOP_TIMEOUT,
        )
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_fetch_reads_reply_without_content_length_to_eof() {
        let daemon = PipeDaemon::start(1, |stream, _| {
            drop(stream.write_all(b"HTTP/1.0 200 OK\r\nServer: test\r\n\r\n[]"));
        });

        let body = fetch_pipe(&daemon.path, Instant::now() + Duration::from_secs(5));
        let requests = daemon.finish();

        assert_eq!(body.ok().as_deref(), Some("[]"), "the body runs to EOF");
        assert_eq!(requests, vec!["GET /containers/json HTTP/1.0".to_string()]);
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_fetch_reports_idle_pipe_at_deadline_as_timeout() {
        // Without a Content-Length the body runs until the daemon closes the
        // pipe. A daemon that stalls must not have its partial body taken as
        // complete: here that would read as an empty container list.
        let daemon = PipeDaemon::start(1, |stream, _| {
            drop(stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n[]"));
            hold_until_client_closes(stream);
        });

        let result = fetch_pipe(&daemon.path, Instant::now() + Duration::from_millis(300));
        drop(daemon.finish());

        assert!(
            matches!(result, Err(FetchError::Timeout)),
            "an unfinished body must time out, got {result:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_fetch_rejects_headers_above_cap() {
        let daemon = PipeDaemon::start(1, |stream, _| {
            drop(stream.write_all(b"HTTP/1.0 200 OK\r\nX-Pad: "));
            let block = [b'a'; 1024];
            for _ in 0..(http::MAX_HEADER_SIZE / block.len() + 2) {
                if stream.write_all(&block).is_err() {
                    return;
                }
            }
            // Keep the pipe open so only the header cap can end the read.
            hold_until_client_closes(stream);
        });

        let result = fetch_pipe(&daemon.path, Instant::now() + Duration::from_secs(10));
        drop(daemon.finish());

        assert!(
            matches!(result, Err(FetchError::Malformed(_))),
            "the header cap, not the deadline, must end the read, got {result:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_fetch_reports_missing_pipe_as_not_found() {
        let result = fetch_pipe(
            &test_pipe_path("missing"),
            Instant::now() + Duration::from_secs(2),
        );
        assert!(
            matches!(result, Err(FetchError::NotFound)),
            "got {result:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn named_pipe_open_waits_for_busy_pipe_until_deadline() {
        let path = test_pipe_path("busy");
        let _instance = create_pipe_instance(&path, 1);
        // The only instance is taken, so the next open finds the pipe busy.
        let _first = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("first client");

        let error = open_named_pipe(&path, Instant::now() + Duration::from_millis(200))
            .expect_err("no instance frees up");

        assert!(
            matches!(FetchError::from_connect_error(error), FetchError::Timeout),
            "a pipe that stays busy until the deadline is a timeout"
        );
    }

    /// The read end of an anonymous pipe, which `PeekNamedPipe` also serves,
    /// with its write end.
    #[cfg(windows)]
    fn anonymous_pipe() -> (std::fs::File, io::PipeWriter) {
        let (reader, writer) = io::pipe().expect("anonymous pipe");
        let reader = std::fs::File::from(std::os::windows::io::OwnedHandle::from(reader));
        (reader, writer)
    }

    #[cfg(windows)]
    #[test]
    fn pipe_stream_reads_closed_pipe_as_eof() {
        let (reader, writer) = anonymous_pipe();
        drop(writer);
        let mut stream = PipeStream::new(reader, Instant::now() + Duration::from_secs(5));

        assert_eq!(
            stream.read(&mut [0_u8; 16]).ok(),
            Some(0),
            "a pipe closed by the server reads as EOF, not as an error"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pipe_stream_reads_only_available_bytes() {
        let (reader, mut writer) = anonymous_pipe();
        writer.write_all(b"abc").expect("write");
        let mut stream = PipeStream::new(reader, Instant::now() + Duration::from_secs(5));
        let mut buf = [0_u8; 16];

        assert_eq!(
            stream.read(&mut buf).ok(),
            Some(3),
            "no read waits for a full buffer"
        );
        assert_eq!(&buf[..3], b"abc");
        drop(writer);
        assert_eq!(stream.read(&mut buf).ok(), Some(0));
    }

    #[cfg(windows)]
    #[test]
    fn pipe_stream_reports_idle_pipe_as_timed_out() {
        let (reader, writer) = anonymous_pipe();
        let started = Instant::now();
        let mut stream = PipeStream::new(reader, started + Duration::from_millis(100));

        let error = stream.read(&mut [0_u8; 16]).expect_err("nothing arrives");
        drop(writer);

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "the read waits for the whole deadline"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pipe_stream_stops_continuous_stream_at_deadline() {
        let (reader, mut writer) = anonymous_pipe();
        let writer_thread = std::thread::spawn(move || {
            let block = [b'x'; 512];
            // Ends once the reader is dropped and the write fails.
            while writer.write_all(&block).is_ok() {}
        });

        let started = Instant::now();
        let mut stream = PipeStream::new(reader, started + Duration::from_millis(200));
        let mut buf = [0_u8; 256];
        let error = loop {
            if let Err(error) = stream.read(&mut buf) {
                break error;
            }
        };
        let elapsed = started.elapsed();
        drop(stream);
        drop(writer_thread.join());

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < Duration::from_secs(3),
            "a continuous stream must not outlive the deadline, took {elapsed:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pipe_stop_reports_status_code_after_ping() {
        let daemon = PipeDaemon::start(2, |stream, request| {
            if is_ping(request) {
                pipe_answer_ping_only(stream, request);
            } else {
                drop(stream.write_all(b"HTTP/1.0 204 No Content\r\n\r\n"));
            }
        });

        let attempt = stop_pipe(&daemon.path, "/containers/abc/stop?t=10");
        let requests = daemon.finish();

        assert_eq!(attempt, StopAttempt::Status(204));
        assert_eq!(
            requests,
            vec![
                PING_REQUEST_LINE.to_string(),
                "POST /containers/abc/stop?t=10 HTTP/1.0".to_string()
            ],
            "the ping goes out on its own connection before the stop"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pipe_stop_never_sends_stop_when_ping_is_dropped() {
        let daemon = PipeDaemon::start(2, |_, _| {});

        let attempt = stop_pipe(&daemon.path, "/containers/abc/stop");
        let requests = daemon.finish();

        assert_eq!(attempt, StopAttempt::Unreachable);
        assert_eq!(
            requests,
            vec![PING_REQUEST_LINE.to_string()],
            "only the ping may be sent"
        );
    }

    #[cfg(windows)]
    #[test]
    fn pipe_stop_classifies_close_after_request_as_no_response() {
        let daemon = PipeDaemon::start(2, pipe_answer_ping_only);

        let attempt = stop_pipe(&daemon.path, "/containers/abc/stop");
        let requests = daemon.finish();

        assert_eq!(
            attempt,
            StopAttempt::NoResponse,
            "a daemon that read the stop request may be acting on it"
        );
        assert!(any_post(&requests), "the stop request was received");
    }
}
