//! OS-specific transport layer for Docker/Podman daemon communication.
//!
//! Provides Unix socket, Windows named pipe, and TCP connection functions.
//! Each transport connects to the daemon, delegates to the HTTP engine for
//! request/response handling, and returns the raw JSON body string.
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

/// Query every candidate on its own thread and collect the successes that
/// arrive before `deadline`.
///
/// Returns as soon as every worker has finished, or at the deadline with
/// whatever has arrived by then. Workers still running at the deadline are
/// detached rather than joined: a stuck endpoint must not delay or discard
/// the answers of the others. Detached workers are bounded by their own
/// transport deadline, and their late results are dropped with the channel.
#[cfg(unix)]
pub fn fetch_all_successes<P, T, I, F>(candidates: I, fetch: F, deadline: Instant) -> Vec<T>
where
    P: Send + 'static,
    T: Send + 'static,
    I: IntoIterator<Item = P>,
    F: Fn(P) -> Option<T> + Send + Sync + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let fetch = std::sync::Arc::new(fetch);

    for candidate in candidates {
        let tx = tx.clone();
        let fetch = std::sync::Arc::clone(&fetch);
        // Detached on purpose: the collector below never joins workers.
        drop(std::thread::spawn(move || {
            if let Some(body) = fetch(candidate) {
                // The receiver is gone once the deadline passed; ignore that.
                drop(tx.send(body));
            }
        }));
    }

    drop(tx);
    let mut responses = Vec::new();

    while let Some(remaining) = remaining_until(deadline) {
        match rx.recv_timeout(remaining) {
            Ok(body) => responses.push(body),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return responses,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
        }
    }

    // Keep anything that landed right at the deadline boundary.
    responses.extend(rx.try_iter());
    responses
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
pub fn fetch_unix_socket_json(path: &std::path::Path, deadline: Instant) -> Option<String> {
    let stream = connect_unix_stream(path, "")?;
    let mut stream = DeadlineStream::new(stream, deadline);
    let response = http::send_http_request(&mut stream);
    if response.is_none() {
        debug!(
            "container runtime socket returned no usable response: socket={}",
            path.display()
        );
    }
    response
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
) -> Option<std::os::unix::net::UnixStream> {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(stream) => Some(stream),
        Err(error) => {
            debug!(
                "failed to connect to container runtime socket{operation_suffix}: socket={} error={error}",
                path.display()
            );
            None
        }
    }
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
pub fn fetch_named_pipe_json(path: &str, deadline: Instant) -> Option<String> {
    let mut stream = open_named_pipe(path, deadline, "")?;
    let response = send_http_request_windows(&mut stream, deadline);
    if response.is_none() {
        debug!("container runtime named pipe returned no usable response: pipe={path}");
    }
    response
}

#[cfg(windows)]
fn send_http_request_windows(stream: &mut std::fs::File, deadline: Instant) -> Option<String> {
    stream.write_all(http::CONTAINERS_HTTP_REQUEST).ok()?;

    let mut response = Vec::with_capacity(8192);
    let mut chunk = [0_u8; 8192];
    let mut headers: Option<http::ParsedHeaders> = None;

    let result = poll_named_pipe_response(
        stream,
        deadline,
        &mut chunk,
        &mut response,
        |response, eof| {
            if eof {
                return http::extract_body_at_eof(response, headers.as_ref())
                    .map_or(PipeParseState::Failed, PipeParseState::Done);
            }
            // Once headers are parsed, continue extracting against the buffered
            // body instead of reparsing the header boundary.
            if let Some(ref hdr) = headers {
                return match http::extract_http_body_from_buffer(response, hdr, eof) {
                    Ok(Some(body)) => PipeParseState::Done(body),
                    Ok(None) => PipeParseState::Pending,
                    Err(()) => PipeParseState::Failed,
                };
            }

            let hdr = match http::response_header_state(response) {
                http::HeaderState::Pending => return PipeParseState::Pending,
                http::HeaderState::Invalid => return PipeParseState::Failed,
                http::HeaderState::Complete(hdr) => hdr,
            };
            if !hdr.status_ok {
                return PipeParseState::Failed;
            }

            match http::extract_http_body_from_buffer(response, &hdr, eof) {
                Ok(Some(body)) => PipeParseState::Done(body),
                Ok(None) => {
                    headers = Some(hdr);
                    PipeParseState::Pending
                }
                Err(()) => PipeParseState::Failed,
            }
        },
    );
    result.ok()
}

#[cfg(windows)]
fn open_named_pipe(path: &str, deadline: Instant, operation_suffix: &str) -> Option<std::fs::File> {
    use std::fs::OpenOptions;

    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(stream) => return Some(stream),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                wait_named_pipe(path, deadline)?;
            }
            Err(error) => {
                debug!(
                    "failed to open container runtime named pipe{operation_suffix}: pipe={path} error={error}"
                );
                return None;
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
    /// The reply was invalid or too large, a read failed, or the deadline
    /// passed.
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
                return finalize_pipe_parse(response, &mut parse, PipeFailure::Failed);
            }
            debug!("container runtime named pipe still streaming at deadline");
            return Err(PipeFailure::Failed);
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

#[cfg(windows)]
fn wait_named_pipe(path: &str, deadline: Instant) -> Option<()> {
    let timeout_ms = remaining_timeout_ms(deadline)?;
    let wide_path = wide_string(path);
    // SAFETY: `wide_path` is a valid null-terminated UTF-16 string produced by
    // `wide_string`, and `timeout_ms` is a plain u32. No aliasing or lifetime
    // invariants apply; the kernel copies the string internally.
    let success = unsafe { WaitNamedPipeW(wide_path.as_ptr(), timeout_ms) };
    (success != 0).then_some(())
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
pub fn fetch_tcp_json(addr: &str, deadline: Instant) -> Option<String> {
    let stream = connect_tcp_stream(addr, deadline)?;
    let mut stream = DeadlineStream::new(stream, deadline);
    let response = http::send_http_request(&mut stream);
    if response.is_none() {
        debug!("container runtime TCP endpoint returned no usable response: tcp={addr}");
    }
    response
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
/// resolve locally.
fn connect_tcp_stream(addr: &str, deadline: Instant) -> Option<std::net::TcpStream> {
    use std::net::ToSocketAddrs;

    let socket_addrs = match addr.to_socket_addrs() {
        Ok(socket_addrs) => socket_addrs,
        Err(error) => {
            debug!("failed to resolve container runtime TCP address: tcp={addr} error={error}");
            return None;
        }
    };

    for socket_addr in socket_addrs {
        let Some(remaining) = remaining_until(deadline) else {
            debug!("container runtime TCP connect deadline expired: tcp={addr}");
            return None;
        };
        let attempt_timeout = remaining.min(CONNECT_ATTEMPT_TIMEOUT);
        match std::net::TcpStream::connect_timeout(&socket_addr, attempt_timeout) {
            Ok(stream) => return Some(stream),
            Err(error) => {
                debug!(
                    "failed to connect to container runtime TCP address: socket_addr={socket_addr} error={error}"
                );
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Container stop / kill transport
// ---------------------------------------------------------------------------

/// Timeout for container stop operations.
///
/// Longer than the query timeout since Docker's graceful stop waits up
/// to 10 seconds by default before sending SIGKILL.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// Send a POST request to stop or kill a container via a Unix socket.
#[cfg(unix)]
pub fn stop_via_unix_socket(path: &std::path::Path, endpoint: &str) -> Option<u16> {
    let stream = connect_unix_stream(path, " for stop")?;
    let mut stream = DeadlineStream::new(stream, Instant::now() + STOP_TIMEOUT);
    http::send_http_post_status(&mut stream, endpoint)
}

/// Send a POST request to stop or kill a container via a Windows named pipe.
#[cfg(windows)]
pub fn stop_via_named_pipe(path: &str, endpoint: &str) -> Option<u16> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    let mut stream = open_named_pipe(path, deadline, " for stop")?;
    send_http_post_status_windows(&mut stream, endpoint, deadline)
}

/// Windows named-pipe polled-IO loop for POST requests that return only a
/// status code (no body needed).
#[cfg(windows)]
fn send_http_post_status_windows(
    stream: &mut std::fs::File,
    endpoint: &str,
    deadline: Instant,
) -> Option<u16> {
    stream
        .write_all(&http::format_post_request(endpoint))
        .ok()?;

    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    poll_named_pipe_response(
        stream,
        deadline,
        &mut chunk,
        &mut response,
        parse_pipe_status,
    )
    .ok()
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
pub fn stop_via_tcp(addr: &str, endpoint: &str) -> Option<u16> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    let stream = connect_tcp_stream(addr, deadline)?;
    let mut stream = DeadlineStream::new(stream, deadline);
    http::send_http_post_status(&mut stream, endpoint)
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
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

    // ── fetch_all_successes ──────────────────────────────────────────

    #[test]
    fn query_budget_is_shorter_than_await_timeout() {
        assert!(
            QUERY_TIMEOUT < DAEMON_TIMEOUT,
            "the detection pass must finish before await_detection gives up"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fetch_all_successes_collects_multiple_responses() {
        let mut responses = fetch_all_successes(
            [1_u8, 2, 3],
            |candidate| (candidate != 2).then(|| candidate.to_string()),
            Instant::now() + Duration::from_secs(5),
        );
        responses.sort();

        assert_eq!(
            responses,
            vec!["1".to_string(), "3".to_string()],
            "every successful candidate should be returned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fetch_all_successes_returns_early_when_all_workers_finish() {
        let started = Instant::now();
        let responses = fetch_all_successes(
            [1_u8, 2],
            |candidate| {
                std::thread::sleep(Duration::from_millis(20));
                Some(candidate)
            },
            started + Duration::from_secs(10),
        );

        assert_eq!(responses.len(), 2, "both late-ish answers should be kept");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the collector should not wait for the deadline once all workers are done"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fetch_all_successes_does_not_wait_for_stuck_workers() {
        let started = Instant::now();
        let responses = fetch_all_successes(
            [0_u64, 3000],
            |delay_ms| {
                std::thread::sleep(Duration::from_millis(delay_ms));
                Some(delay_ms)
            },
            started + Duration::from_millis(300),
        );

        assert_eq!(
            responses,
            vec![0],
            "the fast answer must survive a stuck sibling"
        );
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

        let body = fetch_tcp_json(&addr, Instant::now() + Duration::from_secs(5));
        drop(server.join());

        assert_eq!(body.as_deref(), Some("[]"), "body should pass through");
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

        assert!(body.is_none(), "a trickled response must time out");
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

        assert!(
            connect_tcp_stream(&addr, past).is_none(),
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
            Err(PipeFailure::Failed),
            "an unfinished response must not succeed"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "a continuous stream must not outlive the deadline, took {elapsed:?}"
        );
    }
}
