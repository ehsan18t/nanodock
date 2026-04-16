//! OS-specific transport layer for Docker/Podman daemon communication.
//!
//! Provides Unix socket, Windows named pipe, and TCP connection functions.
//! Each transport connects to the daemon, delegates to the HTTP engine for
//! request/response handling, and returns the raw JSON body string.

use log::debug;

use crate::http;

/// Maximum time to wait for the Docker/Podman daemon to respond.
///
/// On Unix the socket itself has a per-read timeout, but on Windows the
/// named pipe has no built-in timeout support. A thread-level timeout
/// covers both platforms uniformly.
pub const DAEMON_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

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
// Unix socket transport
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub fn unix_socket_paths(uid: u32, home: Option<std::path::PathBuf>) -> Vec<String> {
    let mut socket_paths = Vec::new();

    socket_paths.extend([
        "/var/run/docker.sock".to_string(),
        format!("/run/user/{uid}/docker.sock"),
        format!("/run/user/{uid}/podman/podman.sock"),
        "/run/podman/podman.sock".to_string(),
    ]);

    if let Some(home) = home {
        socket_paths.extend([
            home.join(".docker/desktop/docker.sock")
                .display()
                .to_string(),
            home.join(".docker/run/docker.sock").display().to_string(),
        ]);
    }

    socket_paths
}

#[cfg(unix)]
pub fn fetch_unix_socket_json(path: &std::path::Path) -> Option<String> {
    let mut stream = connect_unix_stream(path, DAEMON_TIMEOUT, "")?;
    let response = http::send_http_request(&mut stream);
    if response.is_none() {
        debug!(
            "container runtime socket returned no usable response: socket={}",
            path.display()
        );
    }
    response
}

#[cfg(unix)]
pub fn fetch_all_successes<P, I, F>(candidates: I, fetch: F) -> Vec<String>
where
    P: Send + 'static,
    I: IntoIterator<Item = P>,
    F: Fn(P) -> Option<String> + Send + Sync + 'static,
{
    fetch_all_successes_with_timeout(candidates, fetch, DAEMON_TIMEOUT)
}

#[cfg(unix)]
fn fetch_all_successes_with_timeout<P, I, F>(
    candidates: I,
    fetch: F,
    timeout: std::time::Duration,
) -> Vec<String>
where
    P: Send + 'static,
    I: IntoIterator<Item = P>,
    F: Fn(P) -> Option<String> + Send + Sync + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let fetch = std::sync::Arc::new(fetch);
    let mut handles = Vec::new();

    for candidate in candidates {
        let tx = tx.clone();
        let fetch = std::sync::Arc::clone(&fetch);
        handles.push(std::thread::spawn(move || {
            if let Some(body) = fetch(candidate) {
                drop(tx.send(body));
            }
        }));
    }

    drop(tx);
    let mut responses = Vec::new();
    let deadline = std::time::Instant::now() + timeout;

    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(body) => responses.push(body),
            Err(
                std::sync::mpsc::RecvTimeoutError::Timeout
                | std::sync::mpsc::RecvTimeoutError::Disconnected,
            ) => {
                break;
            }
        }
    }

    join_worker_threads(handles);

    responses
}

#[cfg(unix)]
fn join_worker_threads(handles: Vec<std::thread::JoinHandle<()>>) {
    for handle in handles {
        drop(handle.join());
    }
}

#[cfg(unix)]
fn connect_unix_stream(
    path: &std::path::Path,
    timeout: std::time::Duration,
    operation_suffix: &str,
) -> Option<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;

    let stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error) => {
            debug!(
                "failed to connect to container runtime socket{operation_suffix}: socket={} error={error}",
                path.display()
            );
            return None;
        }
    };

    // Best-effort timeouts; proceed even if either call fails.
    drop(stream.set_read_timeout(Some(timeout)));
    drop(stream.set_write_timeout(Some(timeout)));
    Some(stream)
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
const PIPE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

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
pub fn fetch_named_pipe_json(path: &str, deadline: std::time::Instant) -> Option<String> {
    let mut stream = open_named_pipe(path, deadline, "")?;
    send_http_request_windows(&mut stream, deadline)
}

#[cfg(windows)]
fn send_http_request_windows(
    stream: &mut std::fs::File,
    deadline: std::time::Instant,
) -> Option<String> {
    use std::io::Write as _;

    stream.write_all(http::CONTAINERS_HTTP_REQUEST).ok()?;

    let mut response = Vec::with_capacity(8192);
    let mut chunk = [0_u8; 8192];
    let mut headers: Option<http::ParsedHeaders> = None;

    poll_named_pipe_response(
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

            let Some(hdr) = http::parse_response_headers(response) else {
                return PipeParseState::Pending;
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
    )
}

#[cfg(windows)]
fn open_named_pipe(
    path: &str,
    deadline: std::time::Instant,
    operation_suffix: &str,
) -> Option<std::fs::File> {
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

#[cfg(windows)]
fn poll_named_pipe_response<T, F>(
    stream: &mut std::fs::File,
    deadline: std::time::Instant,
    chunk: &mut [u8],
    response: &mut Vec<u8>,
    mut parse: F,
) -> Option<T>
where
    F: FnMut(&[u8], bool) -> PipeParseState<T>,
{
    loop {
        match parse(response, false) {
            PipeParseState::Pending => {}
            PipeParseState::Done(value) => return Some(value),
            PipeParseState::Failed => return None,
        }

        let available = match peek_available_bytes(stream) {
            Some(available) => available,
            None if last_os_error_is(ERROR_BROKEN_PIPE) => {
                return finalize_pipe_parse(response, &mut parse);
            }
            None => return None,
        };

        if available == 0 {
            if std::time::Instant::now() >= deadline {
                return finalize_pipe_parse(response, &mut parse);
            }
            std::thread::sleep(PIPE_POLL_INTERVAL);
            continue;
        }

        match read_named_pipe_bytes(stream, available, chunk, response) {
            PipeReadResult::Continue => {}
            PipeReadResult::Eof => return finalize_pipe_parse(response, &mut parse),
            PipeReadResult::Failed => return None,
        }
    }
}

#[cfg(windows)]
fn finalize_pipe_parse<T, F>(response: &[u8], parse: &mut F) -> Option<T>
where
    F: FnMut(&[u8], bool) -> PipeParseState<T>,
{
    match parse(response, true) {
        PipeParseState::Done(value) => Some(value),
        PipeParseState::Pending | PipeParseState::Failed => None,
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

    match std::io::Read::read(stream, &mut chunk[..read_len]) {
        Ok(0) => PipeReadResult::Eof,
        Ok(read) => {
            response.extend_from_slice(&chunk[..read]);
            PipeReadResult::Continue
        }
        Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE) => PipeReadResult::Eof,
        Err(_) => PipeReadResult::Failed,
    }
}

#[cfg(windows)]
fn wait_named_pipe(path: &str, deadline: std::time::Instant) -> Option<()> {
    let timeout_ms = remaining_timeout_ms(deadline)?;
    let wide_path = wide_string(path);
    let success = unsafe { WaitNamedPipeW(wide_path.as_ptr(), timeout_ms) };
    (success != 0).then_some(())
}

#[cfg(windows)]
fn peek_available_bytes(stream: &std::fs::File) -> Option<u32> {
    let mut available = 0;
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

#[cfg(windows)]
fn remaining_timeout_ms(deadline: std::time::Instant) -> Option<u32> {
    let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
    u32::try_from(remaining.as_millis().min(u128::from(u32::MAX))).ok()
}

#[cfg(windows)]
fn wide_string(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn last_os_error_is(expected: i32) -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(expected)
}

// ---------------------------------------------------------------------------
// TCP transport
// ---------------------------------------------------------------------------

/// Connect to a Docker/Podman daemon over plain TCP and fetch container JSON.
///
/// Used when `DOCKER_HOST` is set to `tcp://host:port`.
pub fn fetch_tcp_json(addr: &str) -> Option<String> {
    let mut stream = connect_tcp_stream(addr)?;
    drop(stream.set_read_timeout(Some(DAEMON_TIMEOUT)));
    drop(stream.set_write_timeout(Some(DAEMON_TIMEOUT)));
    let response = http::send_http_request(&mut stream);
    if response.is_none() {
        debug!("container runtime TCP endpoint returned no usable response: tcp={addr}");
    }
    response
}

fn connect_tcp_stream(addr: &str) -> Option<std::net::TcpStream> {
    use std::net::ToSocketAddrs;

    let socket_addrs = match addr.to_socket_addrs() {
        Ok(socket_addrs) => socket_addrs,
        Err(error) => {
            debug!("failed to resolve container runtime TCP address: tcp={addr} error={error}");
            return None;
        }
    };

    for socket_addr in socket_addrs {
        match std::net::TcpStream::connect_timeout(&socket_addr, DAEMON_TIMEOUT) {
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
pub const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Send a POST request to stop or kill a container via a Unix socket.
#[cfg(unix)]
pub fn stop_via_unix_socket(path: &std::path::Path, endpoint: &str) -> Option<u16> {
    let mut stream = connect_unix_stream(path, STOP_TIMEOUT, " for stop")?;
    http::send_http_post_status(&mut stream, endpoint)
}

/// Send a POST request to stop or kill a container via a Windows named pipe.
#[cfg(windows)]
pub fn stop_via_named_pipe(path: &str, endpoint: &str) -> Option<u16> {
    let deadline = std::time::Instant::now() + STOP_TIMEOUT;
    let mut stream = open_named_pipe(path, deadline, " for stop")?;
    send_http_post_status_windows(&mut stream, endpoint, deadline)
}

/// Windows named-pipe polled-IO loop for POST requests that return only a
/// status code (no body needed).
#[cfg(windows)]
fn send_http_post_status_windows(
    stream: &mut std::fs::File,
    endpoint: &str,
    deadline: std::time::Instant,
) -> Option<u16> {
    use std::io::Write as _;

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
        |response, _eof| {
            http::parse_response_headers(response).map_or(PipeParseState::Pending, |hdr| {
                PipeParseState::Done(hdr.status_code)
            })
        },
    )
}

/// Send a POST request to stop or kill a container via TCP.
pub fn stop_via_tcp(addr: &str, endpoint: &str) -> Option<u16> {
    let mut stream = connect_tcp_stream(addr)?;
    drop(stream.set_read_timeout(Some(STOP_TIMEOUT)));
    drop(stream.set_write_timeout(Some(STOP_TIMEOUT)));
    http::send_http_post_status(&mut stream, endpoint)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[cfg(unix)]
    use std::time::Duration;

    #[cfg(unix)]
    use std::path::PathBuf;

    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_socket_paths_include_user_scoped_docker_locations() {
        let home = PathBuf::from("/home/tester");
        let paths = unix_socket_paths(1000, Some(home));

        assert!(paths.contains(&"/run/user/1000/docker.sock".to_string()));
        assert!(
            paths.contains(&"/home/tester/.docker/desktop/docker.sock".to_string()),
            "docker desktop linux socket should be probed"
        );
        assert!(
            paths.contains(&"/home/tester/.docker/run/docker.sock".to_string()),
            "legacy user-scoped docker socket should still be probed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fetch_all_successes_collects_multiple_responses() {
        let mut responses = fetch_all_successes([1_u8, 2, 3], |candidate| {
            (candidate != 2).then(|| candidate.to_string())
        });
        responses.sort();

        assert_eq!(responses, vec!["1".to_string(), "3".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn fetch_all_successes_waits_for_workers_before_returning() {
        let active_workers = Arc::new(AtomicUsize::new(0));
        let worker_counter = Arc::clone(&active_workers);

        let responses = fetch_all_successes_with_timeout(
            [1_u8, 2],
            move |_candidate| {
                worker_counter.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(25));
                worker_counter.fetch_sub(1, Ordering::SeqCst);
                None
            },
            Duration::from_millis(1),
        );

        assert!(
            responses.is_empty(),
            "workers that return no body should produce no results"
        );
        assert_eq!(
            active_workers.load(Ordering::SeqCst),
            0,
            "worker threads should finish before the helper returns"
        );
    }
}
