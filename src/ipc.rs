//! OS-specific transport layer for Docker/Podman daemon communication.
//!
//! Provides Unix socket, Windows named pipe, and TCP connection functions.
//! Each transport connects to the daemon, delegates to the HTTP engine for
//! request/response handling, and returns the raw JSON body string, or a
//! [`FetchError`] that says why the endpoint produced none.
//!
//! Every query and stop request runs against one overall deadline. Socket
//! read timeouts only apply per call, so streams are wrapped in
//! [`DeadlineStream`], which shrinks the timeout before every read and write
//! and fails once the deadline has passed.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use log::debug;

use crate::http;

/// Maximum time [`crate::await_detection`] waits for the detection thread.
pub const DAEMON_TIMEOUT: Duration = Duration::from_secs(3);

/// Overall budget for one detection pass across every transport.
///
/// Strictly shorter than [`DAEMON_TIMEOUT`] so the detection thread always
/// returns what it collected before `await_detection` gives up on it. The
/// margin covers JSON parsing and the channel hand-off.
pub const QUERY_TIMEOUT: Duration = Duration::from_millis(2500);

// Compile-time guard: the internal budget must fit inside the await window.
const _: () = assert!(QUERY_TIMEOUT.as_millis() < DAEMON_TIMEOUT.as_millis());

/// Deadline for a detection pass that starts now.
pub fn query_deadline() -> Instant {
    Instant::now() + QUERY_TIMEOUT
}

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
trait SocketTimeouts {
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
struct DeadlineStream<S> {
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
// DOCKER_HOST environment variable helpers
// ---------------------------------------------------------------------------

/// Extract a Unix socket path from the `DOCKER_HOST` environment variable.
///
/// Returns the path suffix when `DOCKER_HOST` starts with `unix://`,
/// or `None` if the variable is unset or uses a different scheme.
#[cfg(unix)]
pub fn docker_host_unix_path() -> Option<String> {
    let docker_host = std::env::var("DOCKER_HOST").ok()?;
    let path = docker_host.strip_prefix("unix://")?;
    (!path.is_empty()).then(|| path.to_string())
}

/// Extract a named pipe path from the `DOCKER_HOST` environment variable.
///
/// Returns the pipe path (with forward slashes replaced by backslashes)
/// when `DOCKER_HOST` starts with `npipe://`, or `None` if the variable
/// is unset or uses a different scheme.
#[cfg(windows)]
pub fn docker_host_npipe_path() -> Option<String> {
    let docker_host = std::env::var("DOCKER_HOST").ok()?;
    let raw = docker_host.strip_prefix("npipe://")?;
    (!raw.is_empty()).then(|| raw.replace('/', "\\"))
}

/// Extract a TCP address from the `DOCKER_HOST` environment variable.
///
/// Returns the `host:port` string when `DOCKER_HOST` starts with `tcp://`,
/// or `None` if the variable is unset or uses a different scheme.
pub fn docker_host_tcp_addr() -> Option<String> {
    let docker_host = std::env::var("DOCKER_HOST").ok()?;
    let addr = docker_host.strip_prefix("tcp://")?;
    (!addr.is_empty()).then(|| addr.to_string())
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
}

/// Query every candidate on its own thread and collect the results that
/// arrive before `deadline`.
///
/// Returns as soon as every worker has finished, or at the deadline with
/// whatever has arrived by then. Workers still running at the deadline are
/// detached rather than joined: a stuck endpoint must not delay or discard
/// the answers of the others. Detached workers are bounded by their own
/// transport deadline, and their late results are dropped with the channel.
pub fn fetch_all<P, T, I, F>(candidates: I, fetch: F, deadline: Instant) -> FanOut<T>
where
    P: Send + 'static,
    T: Send + 'static,
    I: IntoIterator<Item = P>,
    F: Fn(P) -> T + Send + Sync + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let fetch = std::sync::Arc::new(fetch);
    let mut spawned = 0_usize;

    for candidate in candidates {
        spawned += 1;
        let tx = tx.clone();
        let fetch = std::sync::Arc::clone(&fetch);
        // Detached on purpose: the collector below never joins workers.
        drop(std::thread::spawn(move || {
            // The receiver is gone once the deadline passed; ignore that.
            drop(tx.send(fetch(candidate)));
        }));
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
    }
}

// ---------------------------------------------------------------------------
// Unix socket transport
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub fn unix_socket_paths(uid: u32, home: Option<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let mut socket_paths = vec![
        std::path::PathBuf::from("/var/run/docker.sock"),
        std::path::PathBuf::from(format!("/run/user/{uid}/docker.sock")),
        std::path::PathBuf::from(format!("/run/user/{uid}/podman/podman.sock")),
        std::path::PathBuf::from("/run/podman/podman.sock"),
    ];

    if let Some(home) = home {
        socket_paths.extend([
            home.join(".docker/desktop/docker.sock"),
            home.join(".docker/run/docker.sock"),
        ]);
    }

    socket_paths
}

#[cfg(unix)]
pub fn fetch_unix_socket_json(
    path: &std::path::Path,
    deadline: Instant,
) -> Result<String, FetchError> {
    let stream = connect_unix_stream(path, "").map_err(FetchError::from_connect_error)?;
    let mut stream = DeadlineStream::new(stream, deadline);
    http::send_http_request(&mut stream).map_err(|error| {
        debug!(
            "container runtime socket returned no usable response: socket={} error={error:?}",
            path.display()
        );
        FetchError::from(error)
    })
}

/// Connect to a local Unix stream socket.
///
/// std offers no connect timeout for `UnixStream`. On a local socket
/// `connect(2)` completes or fails immediately in practice: the kernel either
/// queues the connection or refuses it. The one blocking case (Linux, listener
/// backlog full) cannot be bounded with std alone because `SO_SNDTIMEO` would
/// have to be set before connecting. Callers still stay bounded: detection
/// workers are detached at the deadline by [`fetch_all_successes`], and all
/// I/O after the connect runs under [`DeadlineStream`].
#[cfg(unix)]
fn connect_unix_stream(
    path: &std::path::Path,
    operation_suffix: &str,
) -> io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(path).inspect_err(|error| {
        debug!(
            "failed to connect to container runtime socket{operation_suffix}: socket={} error={error}",
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

#[cfg(windows)]
const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Upper bound on everything buffered from a named pipe. Applies whether or
/// not the headers are complete yet, so a daemon that never finishes its
/// headers cannot grow the buffer without limit before the deadline.
#[cfg(windows)]
const MAX_PIPE_BUFFER: usize = http::MAX_HEADER_SIZE + http::MAX_RESPONSE_BODY;

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

#[cfg(windows)]
pub fn fetch_named_pipe_json(path: &str, deadline: Instant) -> Result<String, FetchError> {
    let mut stream = open_named_pipe(path, deadline, "").map_err(FetchError::from_connect_error)?;
    send_http_request_windows(&mut stream, deadline).inspect_err(|error| {
        debug!(
            "container runtime named pipe returned no usable response: pipe={path} error={error:?}"
        );
    })
}

#[cfg(windows)]
fn send_http_request_windows(
    stream: &mut std::fs::File,
    deadline: Instant,
) -> Result<String, FetchError> {
    stream
        .write_all(http::CONTAINERS_HTTP_REQUEST)
        .map_err(|error| FetchError::from(http::ResponseError::Io(error)))?;

    let mut response = Vec::with_capacity(8192);
    let mut chunk = [0_u8; 8192];
    let mut headers: Option<http::ParsedHeaders> = None;
    let mut failure: Option<http::ResponseError> = None;

    let result = poll_named_pipe_response(
        stream,
        deadline,
        &mut chunk,
        &mut response,
        |response, eof| match container_list_step(response, eof, &mut headers) {
            Ok(Some(body)) => PipeParseState::Done(body),
            Ok(None) => PipeParseState::Pending,
            Err(error) => {
                failure = Some(error);
                PipeParseState::Failed
            }
        },
    );
    match result {
        Ok(body) => Ok(body),
        Err(PipeFailure::TimedOut) => Err(FetchError::Timeout),
        Err(PipeFailure::Closed | PipeFailure::Failed) => Err(failure.map_or_else(
            || FetchError::Io(io::Error::other("reading the named pipe failed")),
            FetchError::from,
        )),
    }
}

/// One parse step over the container-list reply buffered so far: `Ok(None)`
/// while more bytes are needed.
///
/// Once the headers are parsed they are kept in `headers`, so later steps
/// extract against the buffered body instead of reparsing the header block.
#[cfg(windows)]
fn container_list_step(
    response: &[u8],
    eof: bool,
    headers: &mut Option<http::ParsedHeaders>,
) -> Result<Option<String>, http::ResponseError> {
    if eof {
        return http::extract_body_at_eof(response, headers.as_ref()).map(Some);
    }
    if let Some(hdr) = headers.as_ref() {
        return http::extract_http_body_from_buffer(response, hdr, false);
    }

    let hdr = match http::response_header_state(response) {
        http::HeaderState::Pending => return Ok(None),
        http::HeaderState::Invalid => {
            return Err(http::ResponseError::Malformed(http::MALFORMED_HEADERS));
        }
        http::HeaderState::Complete(hdr) => hdr,
    };
    let body = http::extract_http_body_from_buffer(response, &hdr, false)?;
    if body.is_none() {
        *headers = Some(hdr);
    }
    Ok(body)
}

#[cfg(windows)]
fn open_named_pipe(
    path: &str,
    deadline: Instant,
    operation_suffix: &str,
) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;

    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(stream) => return Ok(stream),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                wait_named_pipe(path, deadline)?;
            }
            Err(error) => {
                debug!(
                    "failed to open container runtime named pipe{operation_suffix}: pipe={path} error={error}"
                );
                return Err(error);
            }
        }
    }
}

#[cfg(windows)]
enum PipeReadResult {
    Continue,
    Eof,
    Failed,
}

#[cfg(windows)]
enum PipeParseState<T> {
    Pending,
    Done(T),
    Failed,
}

/// Why polling a named pipe produced no parsed value.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipeFailure {
    /// The server closed the pipe before a complete reply was parsed.
    Closed,
    /// The deadline passed before a complete reply was parsed.
    TimedOut,
    /// The reply was invalid or too large, or a read failed.
    Failed,
}

#[cfg(windows)]
fn poll_named_pipe_response<T, F>(
    stream: &mut std::fs::File,
    deadline: Instant,
    chunk: &mut [u8],
    response: &mut Vec<u8>,
    mut parse: F,
) -> Result<T, PipeFailure>
where
    F: FnMut(&[u8], bool) -> PipeParseState<T>,
{
    loop {
        match parse(response, false) {
            PipeParseState::Pending => {}
            PipeParseState::Done(value) => return Ok(value),
            PipeParseState::Failed => return Err(PipeFailure::Failed),
        }

        let available = match peek_available_bytes(stream) {
            Some(available) => available,
            None if last_os_error_is_pipe_closed() => {
                return finalize_pipe_parse(response, &mut parse, PipeFailure::Closed);
            }
            None => return Err(PipeFailure::Failed),
        };

        // Checked on every iteration so a daemon that streams continuously
        // cannot hold the request open past the deadline.
        if Instant::now() >= deadline {
            if available == 0 {
                // Idle at the deadline: treat the buffered bytes as the
                // complete response, as before.
                return finalize_pipe_parse(response, &mut parse, PipeFailure::TimedOut);
            }
            debug!("container runtime named pipe still streaming at deadline");
            return Err(PipeFailure::TimedOut);
        }

        if available == 0 {
            std::thread::sleep(PIPE_POLL_INTERVAL);
            continue;
        }

        match read_named_pipe_bytes(stream, available, chunk, response) {
            PipeReadResult::Continue => {}
            PipeReadResult::Eof => {
                return finalize_pipe_parse(response, &mut parse, PipeFailure::Closed);
            }
            PipeReadResult::Failed => return Err(PipeFailure::Failed),
        }
    }
}

/// Parse `response` as complete, failing with `failure` if it is not.
#[cfg(windows)]
fn finalize_pipe_parse<T, F>(
    response: &[u8],
    parse: &mut F,
    failure: PipeFailure,
) -> Result<T, PipeFailure>
where
    F: FnMut(&[u8], bool) -> PipeParseState<T>,
{
    match parse(response, true) {
        PipeParseState::Done(value) => Ok(value),
        PipeParseState::Pending | PipeParseState::Failed => Err(failure),
    }
}

#[cfg(windows)]
fn read_named_pipe_bytes(
    stream: &mut std::fs::File,
    available: u32,
    chunk: &mut [u8],
    response: &mut Vec<u8>,
) -> PipeReadResult {
    let Ok(max_chunk) = u32::try_from(chunk.len()) else {
        return PipeReadResult::Failed;
    };
    let Ok(read_len) = usize::try_from(available.min(max_chunk)) else {
        return PipeReadResult::Failed;
    };

    match Read::read(stream, &mut chunk[..read_len]) {
        Ok(0) => PipeReadResult::Eof,
        Ok(read) => {
            if append_within_limit(response, &chunk[..read], MAX_PIPE_BUFFER) {
                PipeReadResult::Continue
            } else {
                debug!(
                    "container runtime named pipe response exceeded size limit: limit_bytes={MAX_PIPE_BUFFER}"
                );
                PipeReadResult::Failed
            }
        }
        Err(error) if is_pipe_closed_code(error.raw_os_error()) => PipeReadResult::Eof,
        Err(_) => PipeReadResult::Failed,
    }
}

/// Append `bytes` to `response` unless the result would exceed `limit`.
///
/// Returns `false` (leaving `response` unchanged) when the limit would be
/// exceeded.
#[cfg(windows)]
fn append_within_limit(response: &mut Vec<u8>, bytes: &[u8], limit: usize) -> bool {
    if response.len().saturating_add(bytes.len()) > limit {
        return false;
    }
    response.extend_from_slice(bytes);
    true
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

#[cfg(windows)]
fn peek_available_bytes(stream: &std::fs::File) -> Option<u32> {
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
    (success != 0).then_some(available)
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

#[cfg(windows)]
fn last_os_error_is_pipe_closed() -> bool {
    is_pipe_closed_code(std::io::Error::last_os_error().raw_os_error())
}

// ---------------------------------------------------------------------------
// TCP transport
// ---------------------------------------------------------------------------

/// Connect to a Docker/Podman daemon over plain TCP and fetch container JSON.
///
/// Used when `DOCKER_HOST` is set to `tcp://host:port`. Connecting, writing
/// and reading all share `deadline`.
pub fn fetch_tcp_json(addr: &str, deadline: Instant) -> Result<String, FetchError> {
    let stream = connect_tcp_stream(addr, deadline).map_err(FetchError::from_connect_error)?;
    let mut stream = DeadlineStream::new(stream, deadline);
    http::send_http_request(&mut stream).map_err(|error| {
        debug!(
            "container runtime TCP endpoint returned no usable response: tcp={addr} error={error:?}"
        );
        FetchError::from(error)
    })
}

/// Upper bound for one TCP connect attempt.
///
/// A stop request has a long overall deadline ([`STOP_TIMEOUT`]) to cover
/// the grace period, but connecting should be fast. Capping each attempt
/// keeps one unreachable address from using up the whole deadline before
/// the other resolved addresses are tried.
const CONNECT_ATTEMPT_TIMEOUT: Duration = DAEMON_TIMEOUT;

/// Connect to the first reachable address that `addr` resolves to.
///
/// The connect attempts share `deadline`, and each one is further capped
/// at [`CONNECT_ATTEMPT_TIMEOUT`]. Name resolution itself has no timeout in
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
        let attempt_timeout = remaining.min(CONNECT_ATTEMPT_TIMEOUT);
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
    /// it did not answer the `GET /_ping` preflight (for example a
    /// forwarder whose backend is down), or writing the stop request
    /// failed. Other endpoints may safely be tried.
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

/// Whether the ping preflight got any HTTP reply.
///
/// Every failure, including a reply that is partial or malformed, counts as
/// "not answered": the stop request has not been sent yet, so skipping this
/// endpoint can never cause a second daemon to act on the same container.
fn ping_answered(result: Result<u16, http::StatusFailure>, transport: &str) -> bool {
    match result {
        Ok(status_code) => {
            debug!("container runtime answered ping: {transport} status={status_code}");
            true
        }
        Err(failure) => {
            debug!(
                "container runtime did not answer ping, skipping stop: {transport} failure={failure:?}"
            );
            false
        }
    }
}

/// Send a POST and classify the reply of an already connected endpoint.
fn post_status_attempt(stream: &mut (impl Read + Write), endpoint: &str) -> StopAttempt {
    stop_attempt_from(http::send_http_post_status(stream, endpoint))
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

/// Send a POST request to stop or kill a container via a Unix socket.
///
/// The socket must first answer a `GET /_ping` preflight on its own
/// connection; otherwise the stop is not sent.
#[cfg(unix)]
pub fn stop_via_unix_socket(path: &std::path::Path, endpoint: &str) -> StopAttempt {
    let deadline = Instant::now() + STOP_TIMEOUT;
    if !ping_unix_socket(path, ping_deadline(deadline)) {
        return StopAttempt::Unreachable;
    }
    let Ok(stream) = connect_unix_stream(path, " for stop") else {
        return StopAttempt::Unreachable;
    };
    let mut stream = DeadlineStream::new(stream, deadline);
    post_status_attempt(&mut stream, endpoint)
}

/// Whether the daemon behind a Unix socket answers `GET /_ping`.
#[cfg(unix)]
fn ping_unix_socket(path: &std::path::Path, deadline: Instant) -> bool {
    let Ok(stream) = connect_unix_stream(path, " for ping") else {
        return false;
    };
    let mut stream = DeadlineStream::new(stream, deadline);
    let result = http::send_http_status_request(&mut stream, http::PING_HTTP_REQUEST);
    ping_answered(result, &format!("socket={}", path.display()))
}

/// Send a POST request to stop or kill a container via a Windows named pipe.
///
/// The pipe must first answer a `GET /_ping` preflight on its own
/// connection; otherwise the stop is not sent.
#[cfg(windows)]
pub fn stop_via_named_pipe(path: &str, endpoint: &str) -> StopAttempt {
    let deadline = Instant::now() + STOP_TIMEOUT;
    if !ping_named_pipe(path, ping_deadline(deadline)) {
        return StopAttempt::Unreachable;
    }
    let Ok(mut stream) = open_named_pipe(path, deadline, " for stop") else {
        return StopAttempt::Unreachable;
    };
    stop_attempt_from(send_http_status_request_windows(
        &mut stream,
        &http::format_post_request(endpoint),
        deadline,
    ))
}

/// Whether the daemon behind a named pipe answers `GET /_ping`.
#[cfg(windows)]
fn ping_named_pipe(path: &str, deadline: Instant) -> bool {
    let Ok(mut stream) = open_named_pipe(path, deadline, " for ping") else {
        return false;
    };
    let result = send_http_status_request_windows(&mut stream, http::PING_HTTP_REQUEST, deadline);
    ping_answered(result, &format!("pipe={path}"))
}

/// Windows named-pipe polled-IO loop for requests that need only the status
/// code of the reply (no body).
///
/// Classifies failures like [`http::send_http_status_request`]: only a
/// failed write is [`http::StatusFailure::NotSent`]; once the request is
/// written, a closed pipe, a timeout, or a partial or malformed reply is
/// [`http::StatusFailure::NoReply`].
#[cfg(windows)]
fn send_http_status_request_windows(
    stream: &mut std::fs::File,
    request: &[u8],
    deadline: Instant,
) -> Result<u16, http::StatusFailure> {
    stream
        .write_all(request)
        .map_err(|_| http::StatusFailure::NotSent)?;

    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    poll_named_pipe_response(
        stream,
        deadline,
        &mut chunk,
        &mut response,
        parse_pipe_status,
    )
    .map_err(|_| http::StatusFailure::NoReply)
}

/// Pipe parse step that only needs the status code of the reply.
#[cfg(windows)]
fn parse_pipe_status(response: &[u8], _eof: bool) -> PipeParseState<u16> {
    match http::response_header_state(response) {
        http::HeaderState::Pending => PipeParseState::Pending,
        http::HeaderState::Complete(hdr) => PipeParseState::Done(hdr.status_code),
        http::HeaderState::Invalid => PipeParseState::Failed,
    }
}

/// Send a POST request to stop or kill a container via TCP.
pub fn stop_via_tcp(addr: &str, endpoint: &str) -> StopAttempt {
    stop_via_tcp_until(addr, endpoint, Instant::now() + STOP_TIMEOUT)
}

/// Send a stop request over TCP after a `GET /_ping` preflight on its own
/// connection. Connecting is capped per attempt by [`connect_tcp_stream`],
/// while the ping and the stop request share `deadline`.
fn stop_via_tcp_until(addr: &str, endpoint: &str, deadline: Instant) -> StopAttempt {
    if !ping_tcp(addr, ping_deadline(deadline)) {
        return StopAttempt::Unreachable;
    }
    let Ok(stream) = connect_tcp_stream(addr, deadline) else {
        return StopAttempt::Unreachable;
    };
    let mut stream = DeadlineStream::new(stream, deadline);
    post_status_attempt(&mut stream, endpoint)
}

/// Whether the daemon behind a TCP address answers `GET /_ping`.
fn ping_tcp(addr: &str, deadline: Instant) -> bool {
    let Ok(stream) = connect_tcp_stream(addr, deadline) else {
        return false;
    };
    let mut stream = DeadlineStream::new(stream, deadline);
    let result = http::send_http_status_request(&mut stream, http::PING_HTTP_REQUEST);
    ping_answered(result, &format!("tcp={addr}"))
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use std::path::PathBuf;

    use super::*;

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

    /// Whether any recorded request line is a POST (a stop or kill).
    fn any_post(requests: &[String]) -> bool {
        requests.iter().any(|line| line.starts_with("POST "))
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_paths_include_user_scoped_docker_locations() {
        let home = PathBuf::from("/home/tester");
        let paths = unix_socket_paths(1000, Some(home));

        assert!(paths.contains(&PathBuf::from("/run/user/1000/docker.sock")));
        assert!(
            paths.contains(&PathBuf::from("/home/tester/.docker/desktop/docker.sock")),
            "docker desktop linux socket should be probed"
        );
        assert!(
            paths.contains(&PathBuf::from("/home/tester/.docker/run/docker.sock")),
            "legacy user-scoped docker socket should still be probed"
        );
    }

    // ── fetch_all ────────────────────────────────────────────────────

    #[test]
    fn query_budget_is_shorter_than_await_timeout() {
        assert!(
            QUERY_TIMEOUT < DAEMON_TIMEOUT,
            "the detection pass must finish before await_detection gives up"
        );
    }

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
        let started = Instant::now();
        let fan_out = fetch_all(
            [0_u64, 3000],
            |delay_ms| {
                std::thread::sleep(Duration::from_millis(delay_ms));
                delay_ms
            },
            started + Duration::from_millis(300),
        );

        assert_eq!(
            fan_out.results,
            vec![0],
            "the fast answer must survive a stuck sibling"
        );
        assert_eq!(fan_out.unfinished, 1, "the stuck worker is counted");
        assert!(
            started.elapsed() < Duration::from_millis(2000),
            "a stuck worker must be detached, not joined"
        );
    }

    // ── DeadlineStream / TCP ─────────────────────────────────────────

    #[test]
    fn fetch_tcp_json_reads_complete_response() {
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            drop(stream.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n[]"));
        });

        let body = fetch_tcp_json(&addr, Instant::now() + Duration::from_secs(5)).ok();
        drop(server.join());

        assert_eq!(body.as_deref(), Some("[]"), "body should pass through");
    }

    #[test]
    fn fetch_tcp_json_reports_http_status() {
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            drop(stream.write_all(b"HTTP/1.0 403 Forbidden\r\nContent-Length: 0\r\n\r\n"));
        });

        let result = fetch_tcp_json(&addr, Instant::now() + Duration::from_secs(5));
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
    fn fetch_tcp_json_enforces_overall_deadline_against_trickling_daemon() {
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
        let body = fetch_tcp_json(&addr, started + Duration::from_millis(200));
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
    fn stop_via_tcp_reports_status_code_after_ping() {
        let daemon = TestDaemon::start(answer_ping_then_204);

        let attempt = stop_via_tcp(&daemon.addr, "/containers/abc/stop?t=10");
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
    fn stop_via_tcp_classifies_refused_connection_as_unreachable() {
        let (listener, addr) = loopback_listener();
        drop(listener);

        assert_eq!(
            stop_via_tcp_until(
                &addr,
                "/containers/abc/stop",
                // Short on purpose: Windows retries refused loopback
                // connects, so this may end at the deadline, not a refusal.
                Instant::now() + Duration::from_millis(300)
            ),
            StopAttempt::Unreachable,
            "a closed port never received the request"
        );
    }

    #[test]
    fn stop_via_tcp_classifies_silent_daemon_as_no_response() {
        let daemon = TestDaemon::start(|stream, request| {
            if is_ping(request) {
                answer_ping_only(stream, request);
            } else {
                std::thread::sleep(Duration::from_millis(500));
            }
        });

        let attempt = stop_via_tcp_until(
            &daemon.addr,
            "/containers/abc/stop",
            Instant::now() + Duration::from_millis(300),
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
    fn stop_via_tcp_classifies_accept_then_drop_as_unreachable() {
        // A forwarder (socat, SSH tunnel) whose backend is down accepts the
        // connection and closes it without sending a byte.
        let (listener, addr) = loopback_listener();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            drop(stream);
        });

        let attempt = stop_via_tcp_until(
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
    fn stop_via_tcp_never_sends_stop_when_ping_is_dropped() {
        // Reads every request, then closes the connection without a reply.
        let daemon = TestDaemon::start(|_, _| {});

        let attempt = stop_via_tcp_until(
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
    fn stop_via_tcp_never_sends_stop_when_ping_times_out() {
        let daemon = TestDaemon::start(|_, _| std::thread::sleep(Duration::from_millis(600)));

        let attempt = stop_via_tcp_until(
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

    #[test]
    fn stop_via_tcp_classifies_close_after_request_as_no_response() {
        // dockerd starts the grace period, then crashes, or Go net/http
        // recovers a handler panic by closing the connection unanswered.
        let daemon = TestDaemon::start(answer_ping_only);

        let attempt = stop_via_tcp_until(
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
    fn stop_via_tcp_classifies_partial_reply_as_no_response() {
        let daemon = TestDaemon::start(|stream, request| {
            if is_ping(request) {
                answer_ping_only(stream, request);
            } else {
                drop(stream.write_all(b"HTTP/1.0 20"));
            }
        });

        let attempt = stop_via_tcp_until(
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
        let first = TestDaemon::start(answer_ping_only);
        let second = TestDaemon::start(answer_ping_then_204);

        let targets = [(false, first.addr.clone()), (false, second.addr.clone())];
        let attempt = crate::first_stop_owner(targets, |addr| {
            stop_via_tcp(&addr, "/containers/web/stop?t=10")
        });
        let first_requests = first.finish();
        let second_requests = second.finish();

        assert_eq!(
            crate::stop_outcome(attempt, false),
            crate::StopOutcome::Failed,
            "an unanswered stop is a failure"
        );
        assert!(any_post(&first_requests), "the first daemon got the stop");
        assert!(
            second_requests.is_empty(),
            "a same-named container on the second daemon must not be touched"
        );
    }

    #[test]
    fn stop_falls_through_dead_forwarder_to_next_endpoint() {
        let forwarder = TestDaemon::start(|_, _| {});
        let daemon = TestDaemon::start(answer_ping_then_204);

        let targets = [(true, forwarder.addr.clone()), (false, daemon.addr.clone())];
        let attempt = crate::first_stop_owner(targets, |addr| {
            stop_via_tcp(&addr, "/containers/web/stop?t=10")
        });
        let forwarder_requests = forwarder.finish();
        drop(daemon.finish());

        assert_eq!(
            crate::stop_outcome(attempt, false),
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
    fn stop_via_unix_socket_classifies_accept_then_drop_as_unreachable() {
        let path = unix_test_socket_path("stop-drop");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind unix socket");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            drop(stream);
        });

        let attempt = stop_via_unix_socket(&path, "/containers/abc/stop");
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
    fn stop_via_unix_socket_classifies_close_after_request_as_no_response() {
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

        let attempt = stop_via_unix_socket(&path, "/containers/abc/stop");
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
    #[test]
    fn append_within_limit_rejects_overflowing_chunk() {
        let mut response = vec![0_u8; 6];

        assert!(
            append_within_limit(&mut response, &[1, 2], 8),
            "filling up to the limit is allowed"
        );
        assert!(
            !append_within_limit(&mut response, &[3], 8),
            "exceeding the limit must fail"
        );
        assert_eq!(response.len(), 8, "a rejected chunk must not be appended");
    }

    #[cfg(windows)]
    #[test]
    fn poll_named_pipe_response_reports_close_without_reply() {
        let (reader, writer) = std::io::pipe().expect("anonymous pipe");
        let mut reader = std::fs::File::from(std::os::windows::io::OwnedHandle::from(reader));
        drop(writer);

        let mut chunk = [0_u8; 64];
        let mut response = Vec::new();
        let result: Result<u16, PipeFailure> = poll_named_pipe_response(
            &mut reader,
            Instant::now() + Duration::from_secs(5),
            &mut chunk,
            &mut response,
            parse_pipe_status,
        );

        assert_eq!(
            result,
            Err(PipeFailure::Closed),
            "a pipe closed by the server reports Closed, not a generic failure"
        );
        assert!(response.is_empty(), "no reply byte was received");
    }

    #[cfg(windows)]
    #[test]
    fn poll_named_pipe_response_rejects_headers_above_cap() {
        let (reader, mut writer) = std::io::pipe().expect("anonymous pipe");
        let mut reader = std::fs::File::from(std::os::windows::io::OwnedHandle::from(reader));
        let writer_thread = std::thread::spawn(move || {
            drop(writer.write_all(b"HTTP/1.0 200 OK\r\nX-Pad: "));
            let block = [b'a'; 1024];
            for _ in 0..(http::MAX_HEADER_SIZE / block.len() + 2) {
                if writer.write_all(&block).is_err() {
                    return;
                }
            }
            // Keep the pipe open so only the header cap can end the read.
            std::thread::sleep(Duration::from_millis(500));
        });

        let started = Instant::now();
        let mut chunk = [0_u8; 1024];
        let mut response = Vec::new();
        let result: Result<u16, PipeFailure> = poll_named_pipe_response(
            &mut reader,
            started + Duration::from_secs(5),
            &mut chunk,
            &mut response,
            parse_pipe_status,
        );
        let elapsed = started.elapsed();
        drop(reader);
        drop(writer_thread.join());

        assert_eq!(
            result,
            Err(PipeFailure::Failed),
            "oversized headers must fail"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "the header cap, not the deadline, must end the read, took {elapsed:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn poll_named_pipe_response_stops_continuous_stream_at_deadline() {
        let (reader, mut writer) = std::io::pipe().expect("anonymous pipe");
        let mut reader = std::fs::File::from(std::os::windows::io::OwnedHandle::from(reader));
        let writer_thread = std::thread::spawn(move || {
            let block = [b'x'; 512];
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(5) {
                if writer.write_all(&block).is_err() {
                    return;
                }
            }
        });

        let started = Instant::now();
        let mut chunk = [0_u8; 256];
        let mut response = Vec::new();
        let result: Result<(), PipeFailure> = poll_named_pipe_response(
            &mut reader,
            started + Duration::from_millis(200),
            &mut chunk,
            &mut response,
            |_, _| PipeParseState::Pending,
        );
        let elapsed = started.elapsed();
        drop(reader);
        drop(writer_thread.join());

        assert_eq!(
            result,
            Err(PipeFailure::TimedOut),
            "an unfinished response must not succeed"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "a continuous stream must not outlive the deadline, took {elapsed:?}"
        );
    }
}
