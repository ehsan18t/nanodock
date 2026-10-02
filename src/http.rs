//! Minimal HTTP/1.0 response parser for Docker daemon replies.
//!
//! Header parsing is handled by `httparse`; chunked transfer-encoding
//! body framing uses `httparse::parse_chunk_size`. Every transport (Unix
//! sockets, TCP, and Windows named pipes) is a blocking `Read + Write`
//! stream, read through one `BufReader`: headers are consumed line by line
//! so the reader is left positioned at the body start, and every read is
//! capped so a misbehaving daemon cannot grow a buffer without bound.

use std::io::{self, BufRead, BufReader, Read};

/// Upper bound on a decoded response body (64 MiB).
///
/// A `GET /containers/json` reply for thousands of containers stays far
/// below this. Anything larger is treated as a misbehaving or hostile
/// daemon and rejected before it can exhaust memory.
pub const MAX_RESPONSE_BODY: usize = 64 * 1024 * 1024;

/// Upper bound on the status line plus all header lines (64 KiB).
pub const MAX_HEADER_SIZE: usize = 64 * 1024;

/// Upper bound on a single chunk-size line, including chunk extensions.
const MAX_CHUNK_LINE: usize = 4 * 1024;

/// Initial body buffer capacity when `Content-Length` is known. The header
/// value is never trusted for allocation beyond this; the buffer grows as
/// bytes actually arrive.
const INITIAL_BODY_CAPACITY: usize = 64 * 1024;

/// Pre-parsed HTTP response header metadata from a Docker daemon reply.
pub struct ParsedHeaders {
    /// Whether the HTTP status code is 2xx.
    pub status_ok: bool,
    /// Raw HTTP status code (e.g. 200, 204, 304, 404).
    pub status_code: u16,
    /// Value of the `Content-Length` header, if present.
    pub content_length: Option<usize>,
    /// Transfer framing used for the response body.
    pub transfer_encoding: TransferEncoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferEncoding {
    Identity,
    Chunked,
    Unsupported,
}

/// Why a daemon reply could not be turned into a response body.
#[derive(Debug)]
pub enum ResponseError {
    /// Reading or writing the stream failed (including the deadline
    /// passing, which surfaces as [`io::ErrorKind::TimedOut`]).
    Io(io::Error),
    /// The daemon answered with a non-2xx HTTP status.
    Status(u16),
    /// The reply was not a well-formed HTTP response within the size caps.
    Malformed(&'static str),
}

impl From<io::Error> for ResponseError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Reply ended before its headers or body were complete.
pub const INCOMPLETE_RESPONSE: &str = "connection closed before the response was complete";
/// Status line or headers could not be parsed, or exceed the header cap.
pub const MALFORMED_HEADERS: &str = "malformed or oversized response headers";
/// Decoded body exceeds [`MAX_RESPONSE_BODY`].
const BODY_TOO_LARGE: &str = "response body exceeds 64 MiB";
/// Chunked framing is invalid.
const MALFORMED_CHUNKS: &str = "malformed chunked transfer encoding";
/// Body is not UTF-8.
const BODY_NOT_UTF8: &str = "response body is not valid UTF-8";
/// Transfer coding other than `chunked` or `identity`.
const UNSUPPORTED_ENCODING: &str = "unsupported transfer encoding";

/// Raw HTTP/1.0 request sent to the Docker/Podman daemon to list running
/// containers. The API version prefix is intentionally omitted so the daemon
/// uses its own default, avoiding 400 errors on older engines.
pub const CONTAINERS_HTTP_REQUEST: &[u8] =
    b"GET /containers/json HTTP/1.0\r\nHost: localhost\r\n\r\n";

// ---------------------------------------------------------------------------
// Streaming path (Unix / TCP)
// ---------------------------------------------------------------------------

/// Send the container-list request and read the complete response body.
pub fn send_http_request(
    stream: &mut (impl Read + std::io::Write),
) -> Result<String, ResponseError> {
    stream.write_all(CONTAINERS_HTTP_REQUEST)?;

    let mut reader = BufReader::new(stream);

    let headers = read_response_headers(&mut reader)?;
    if !headers.status_ok {
        return Err(ResponseError::Status(headers.status_code));
    }

    read_response_body(&mut reader, &headers)
}

/// Read HTTP response headers from a buffered reader using `httparse`.
///
/// Reads raw bytes until the header/body boundary (empty `\r\n` line),
/// then delegates to `httparse::Response::parse` for robust parsing.
/// The reader is left positioned at the start of the response body.
fn read_response_headers(reader: &mut impl BufRead) -> Result<ParsedHeaders, ResponseError> {
    // Pre-allocate for a typical Docker daemon header payload.
    let mut raw = Vec::with_capacity(1024);

    // Accumulate the status line and all header lines including the
    // final empty CRLF. Using `read_until` instead of `read_line`
    // avoids a per-line String allocation and UTF-8 validation pass
    // since httparse operates on raw bytes anyway. Total accumulation is
    // capped at `MAX_HEADER_SIZE`, enforced while reading so that a
    // newline-free stream cannot be buffered without bound.
    loop {
        let start = raw.len();
        let budget = MAX_HEADER_SIZE
            .checked_sub(start)
            .ok_or(ResponseError::Malformed(MALFORMED_HEADERS))?;
        if read_line_bounded(reader, &mut raw, budget, MALFORMED_HEADERS)? == 0 {
            return Err(ResponseError::Malformed(INCOMPLETE_RESPONSE));
        }
        let line = &raw[start..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }

    let mut headers_buf = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_buf);

    let parsed = response
        .parse(&raw)
        .map_err(|_| ResponseError::Malformed(MALFORMED_HEADERS))?;
    if parsed.is_partial() {
        return Err(ResponseError::Malformed(MALFORMED_HEADERS));
    }

    let status_code = response.code.unwrap_or(0);
    let status_ok = (200..300).contains(&status_code);
    let (content_length, transfer_encoding) = extract_header_metadata(response.headers);

    Ok(ParsedHeaders {
        status_ok,
        status_code,
        content_length,
        transfer_encoding,
    })
}

fn read_response_body(
    reader: &mut impl BufRead,
    headers: &ParsedHeaders,
) -> Result<String, ResponseError> {
    let body = match headers.transfer_encoding {
        TransferEncoding::Identity => match headers.content_length {
            Some(content_length) => read_exact_body(reader, content_length)?,
            None => read_body_to_eof(reader)?,
        },
        TransferEncoding::Chunked => read_chunked_body(reader)?,
        TransferEncoding::Unsupported => {
            return Err(ResponseError::Malformed(UNSUPPORTED_ENCODING));
        }
    };
    String::from_utf8(body).map_err(|_| ResponseError::Malformed(BODY_NOT_UTF8))
}

/// Read exactly `content_length` body bytes.
///
/// Lengths above [`MAX_RESPONSE_BODY`] are rejected up front. The buffer is
/// never sized from the header alone: it starts small and grows only as
/// bytes are actually received, so a lying `Content-Length` cannot trigger
/// a huge allocation.
fn read_exact_body(
    reader: &mut impl BufRead,
    content_length: usize,
) -> Result<Vec<u8>, ResponseError> {
    if content_length > MAX_RESPONSE_BODY {
        return Err(ResponseError::Malformed(BODY_TOO_LARGE));
    }
    let mut body = Vec::with_capacity(content_length.min(INITIAL_BODY_CAPACITY));
    let limit =
        u64::try_from(content_length).map_err(|_| ResponseError::Malformed(BODY_TOO_LARGE))?;
    let read = Read::take(&mut *reader, limit).read_to_end(&mut body)?;
    // A short read means the daemon closed the connection mid-body.
    if read == content_length {
        Ok(body)
    } else {
        Err(ResponseError::Malformed(INCOMPLETE_RESPONSE))
    }
}

/// Read the body until EOF, failing if it exceeds [`MAX_RESPONSE_BODY`].
fn read_body_to_eof(reader: &mut impl BufRead) -> Result<Vec<u8>, ResponseError> {
    // Allow one byte past the cap so an oversize body is detectable.
    let limit = MAX_RESPONSE_BODY
        .checked_add(1)
        .and_then(|limit| u64::try_from(limit).ok())
        .ok_or(ResponseError::Malformed(BODY_TOO_LARGE))?;
    let mut body = Vec::new();
    Read::take(&mut *reader, limit).read_to_end(&mut body)?;
    if body.len() <= MAX_RESPONSE_BODY {
        Ok(body)
    } else {
        Err(ResponseError::Malformed(BODY_TOO_LARGE))
    }
}

/// Append one `\n`-terminated line from `reader` to `buf`, reading at most
/// `max_len` bytes.
///
/// Returns the number of bytes appended (`0` at EOF). Fails on an I/O error,
/// or with `overlong` when the line does not terminate within `max_len`
/// bytes.
fn read_line_bounded(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max_len: usize,
    overlong: &'static str,
) -> Result<usize, ResponseError> {
    // Allow one byte past the budget so an overlong line is detectable.
    let limit = max_len
        .checked_add(1)
        .and_then(|limit| u64::try_from(limit).ok())
        .ok_or(ResponseError::Malformed(overlong))?;
    let read = Read::take(&mut *reader, limit).read_until(b'\n', buf)?;
    if read <= max_len {
        Ok(read)
    } else {
        Err(ResponseError::Malformed(overlong))
    }
}

fn read_chunked_body(reader: &mut impl BufRead) -> Result<Vec<u8>, ResponseError> {
    let mut body = Vec::new();
    let mut size_line = Vec::with_capacity(16);

    loop {
        size_line.clear();
        if read_line_bounded(reader, &mut size_line, MAX_CHUNK_LINE, MALFORMED_CHUNKS)? == 0 {
            return Err(ResponseError::Malformed(INCOMPLETE_RESPONSE));
        }

        let chunk_size = parse_streaming_chunk_size(&size_line)
            .ok_or(ResponseError::Malformed(MALFORMED_CHUNKS))?;
        if chunk_size == 0 {
            consume_chunked_trailers(reader);
            return Ok(body);
        }

        // Enforce the cap on the cumulative decoded body. `checked_add`
        // guards against chunk sizes near `usize::MAX`.
        let expected_len = body
            .len()
            .checked_add(chunk_size)
            .filter(|len| *len <= MAX_RESPONSE_BODY)
            .ok_or(ResponseError::Malformed(BODY_TOO_LARGE))?;

        // Grow the buffer as bytes arrive instead of pre-sizing it from
        // the untrusted chunk header.
        let limit =
            u64::try_from(chunk_size).map_err(|_| ResponseError::Malformed(BODY_TOO_LARGE))?;
        Read::take(&mut *reader, limit).read_to_end(&mut body)?;
        if body.len() != expected_len {
            return Err(ResponseError::Malformed(INCOMPLETE_RESPONSE));
        }

        let mut chunk_terminator = [0_u8; 2];
        reader.read_exact(&mut chunk_terminator)?;
        if chunk_terminator != *b"\r\n" {
            return Err(ResponseError::Malformed(MALFORMED_CHUNKS));
        }
    }
}

/// Parse a chunk size line using `httparse::parse_chunk_size`.
///
/// Wraps the standard httparse API for the streaming (line-at-a-time)
/// reader path where we have a single raw line.
fn parse_streaming_chunk_size(line: &[u8]) -> Option<usize> {
    match httparse::parse_chunk_size(line) {
        Ok(httparse::Status::Complete((_, size))) => usize::try_from(size).ok(),
        _ => None,
    }
}

fn consume_chunked_trailers(reader: &mut impl BufRead) {
    let mut trailer_line = Vec::new();
    let mut budget = MAX_HEADER_SIZE;
    loop {
        trailer_line.clear();
        // EOF, an I/O error, or an oversized trailer section after the
        // terminal chunk all end consumption: the body is already complete.
        let Ok(bytes_read) =
            read_line_bounded(reader, &mut trailer_line, budget, MALFORMED_HEADERS)
        else {
            return;
        };
        if bytes_read == 0 || trailer_line.trim_ascii().is_empty() {
            return;
        }
        budget = budget.saturating_sub(bytes_read);
    }
}

// ---------------------------------------------------------------------------
// Status-only requests (daemon ping, container stop / kill)
// ---------------------------------------------------------------------------

/// Raw HTTP/1.0 request for the daemon's `/_ping` endpoint, used to check
/// that a daemon (and not only a forwarder in front of it) is answering.
pub const PING_HTTP_REQUEST: &[u8] = b"GET /_ping HTTP/1.0\r\nHost: localhost\r\n\r\n";

/// Build an HTTP/1.0 POST request for the given daemon endpoint path.
pub fn format_post_request(path: &str) -> Vec<u8> {
    format!("POST {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").into_bytes()
}

/// Why a status-only request produced no HTTP status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFailure {
    /// The request was never fully written, so no daemon can have acted on
    /// it.
    NotSent,
    /// The request was fully written but no usable reply arrived: the peer
    /// closed or reset the connection, the deadline passed, or the reply was
    /// partial or malformed. A daemon may have received the request and may
    /// still be acting on it.
    NoReply,
}

/// Send `request` and return the status code of the reply.
///
/// Fails with [`StatusFailure::NotSent`] only when writing the request
/// fails. Once the request is written, every failure (EOF, reset, timeout,
/// partial or malformed reply) is [`StatusFailure::NoReply`].
pub fn send_http_status_request(
    stream: &mut (impl Read + std::io::Write),
    request: &[u8],
) -> Result<u16, StatusFailure> {
    stream
        .write_all(request)
        .map_err(|_| StatusFailure::NotSent)?;
    let mut reader = BufReader::new(stream);
    read_response_headers(&mut reader)
        .map(|headers| headers.status_code)
        .map_err(|_| StatusFailure::NoReply)
}

/// Send an HTTP POST request and return the response status code.
///
/// Used for Docker API calls that return no body (e.g. container stop/kill
/// which respond with 204 No Content). Only the status code is needed.
/// Failures are classified as in [`send_http_status_request`].
pub fn send_http_post_status(
    stream: &mut (impl Read + std::io::Write),
    path: &str,
) -> Result<u16, StatusFailure> {
    send_http_status_request(stream, &format_post_request(path))
}

// ---------------------------------------------------------------------------
// Shared header extraction
// ---------------------------------------------------------------------------

/// Extract `Content-Length` and `Transfer-Encoding` from parsed headers.
///
/// Header names are matched case-insensitively, which `httparse` already
/// provides as raw `&[u8]` slices.
fn extract_header_metadata(headers: &[httparse::Header<'_>]) -> (Option<usize>, TransferEncoding) {
    let mut content_length = None;
    let mut transfer_encoding = TransferEncoding::Identity;

    for header in headers {
        if header.name.eq_ignore_ascii_case("Content-Length") {
            if let Ok(value) = std::str::from_utf8(header.value) {
                content_length = value.trim().parse().ok();
            }
        } else if header.name.eq_ignore_ascii_case("Transfer-Encoding")
            && let Ok(value) = std::str::from_utf8(header.value)
        {
            transfer_encoding = parse_transfer_encoding(value);
        }
    }

    (content_length, transfer_encoding)
}

fn parse_transfer_encoding(value: &str) -> TransferEncoding {
    let mut saw_chunked = false;
    let mut saw_unsupported = false;

    for coding in value
        .split(',')
        .map(str::trim)
        .filter(|coding| !coding.is_empty())
    {
        if coding.eq_ignore_ascii_case("chunked") {
            saw_chunked = true;
        } else if !coding.eq_ignore_ascii_case("identity") {
            saw_unsupported = true;
        }
    }

    if saw_unsupported {
        TransferEncoding::Unsupported
    } else if saw_chunked {
        TransferEncoding::Chunked
    } else {
        TransferEncoding::Identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_waits_for_complete_content_length() {
        assert_eq!(
            stream_response(&b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\n12345"[..]).as_deref(),
            Some("12345")
        );
    }

    #[test]
    fn streaming_reads_body_to_eof_without_content_length() {
        assert_eq!(
            stream_response(&b"HTTP/1.0 200 OK\r\nServer: docker\r\n\r\n[1,2]"[..]).as_deref(),
            Some("[1,2]"),
            "without a length the body runs to EOF"
        );
    }

    #[test]
    fn streaming_decodes_chunked_payloads() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n\r\n";
        assert_eq!(stream_response(&response[..]).as_deref(), Some("[]"));
    }

    #[test]
    fn streaming_chunked_body_accepted_at_eof_without_trailing_crlf() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n";
        assert_eq!(
            stream_response(&response[..]).as_deref(),
            Some("[]"),
            "streaming path should accept chunked body when server closes after terminal chunk"
        );
    }

    #[test]
    fn streaming_rejects_unsupported_transfer_encoding() {
        let result = send_http_request(&mut MockStream {
            reader: &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"[..],
        });
        assert!(
            matches!(result, Err(ResponseError::Malformed(UNSUPPORTED_ENCODING))),
            "got {result:?}"
        );
    }

    /// Parse the header block at the start of `response`.
    fn headers_of(response: &[u8]) -> Result<ParsedHeaders, ResponseError> {
        read_response_headers(&mut BufReader::new(response))
    }

    #[test]
    fn headers_report_incomplete_block_at_eof() {
        assert!(
            matches!(
                headers_of(b"HTTP/1.0 200 OK\r\nContent-Len"),
                Err(ResponseError::Malformed(INCOMPLETE_RESPONSE))
            ),
            "headers cut off by EOF are incomplete"
        );
    }

    #[test]
    fn headers_extract_content_length() {
        let hdr = headers_of(b"HTTP/1.0 200 OK\r\nContent-Length: 42\r\n\r\nbody")
            .expect("headers should parse");
        assert!(hdr.status_ok, "status should be ok");
        assert_eq!(hdr.content_length, Some(42));
        assert_eq!(hdr.transfer_encoding, TransferEncoding::Identity);
    }

    #[test]
    fn headers_detect_chunked_transfer_encoding() {
        let hdr = headers_of(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .expect("headers should parse");
        assert_eq!(hdr.transfer_encoding, TransferEncoding::Chunked);
    }

    #[test]
    fn headers_detect_non_2xx_status() {
        let hdr = headers_of(b"HTTP/1.0 404 Not Found\r\n\r\n").expect("headers should parse");
        assert!(!hdr.status_ok, "404 should not be marked as ok");
        assert_eq!(hdr.status_code, 404);
    }

    #[test]
    fn headers_reject_complete_block_above_cap() {
        let mut response = b"HTTP/1.0 200 OK\r\nX-Pad: ".to_vec();
        response.resize(MAX_HEADER_SIZE, b'a');
        response.extend_from_slice(b"\r\n\r\n[]");
        assert!(
            matches!(
                headers_of(&response),
                Err(ResponseError::Malformed(MALFORMED_HEADERS))
            ),
            "a header block larger than the cap must fail"
        );
    }

    /// Daemon stand-in that discards writes and serves reads from `R`.
    struct MockStream<R> {
        reader: R,
    }

    impl<R: Read> Read for MockStream<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reader.read(buf)
        }
    }

    impl<R> std::io::Write for MockStream<R> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn stream_response(reader: impl Read) -> Option<String> {
        send_http_request(&mut MockStream { reader }).ok()
    }

    #[test]
    fn streaming_reports_non_2xx_status() {
        let result = send_http_request(&mut MockStream {
            reader: &b"HTTP/1.0 403 Forbidden\r\nContent-Length: 0\r\n\r\n"[..],
        });
        assert!(
            matches!(result, Err(ResponseError::Status(403))),
            "a non-2xx reply reports its status, got {result:?}"
        );
    }

    #[test]
    fn streaming_reports_malformed_and_truncated_replies() {
        let garbage = send_http_request(&mut MockStream {
            reader: &b"NOT-HTTP garbage\r\n\r\n"[..],
        });
        assert!(
            matches!(garbage, Err(ResponseError::Malformed(MALFORMED_HEADERS))),
            "got {garbage:?}"
        );
        let truncated = send_http_request(&mut MockStream {
            reader: &b"HTTP/1.0 200 OK\r\nContent-Length: 10\r\n\r\n[]"[..],
        });
        assert!(
            matches!(
                truncated,
                Err(ResponseError::Malformed(INCOMPLETE_RESPONSE))
            ),
            "got {truncated:?}"
        );
    }

    #[test]
    fn streaming_reports_io_errors() {
        let mut stream = FailingStream {
            write_error: None,
            read_error: std::io::ErrorKind::TimedOut,
        };
        let result = send_http_request(&mut stream);
        assert!(
            matches!(&result, Err(ResponseError::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut),
            "got {result:?}"
        );
    }

    #[test]
    fn streaming_rejects_content_length_overflowing_usize_range() {
        let response = b"HTTP/1.0 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n[]";
        assert!(
            stream_response(&response[..]).is_none(),
            "u64::MAX content length must be rejected without allocating"
        );
    }

    #[test]
    fn streaming_rejects_content_length_above_cap() {
        let response = format!(
            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n[]",
            MAX_RESPONSE_BODY + 1
        );
        assert!(stream_response(response.as_bytes()).is_none());
    }

    #[test]
    fn streaming_rejects_body_shorter_than_content_length() {
        // In range but lying: must not pre-allocate 32 MiB and must fail
        // cleanly on the short read.
        let response = b"HTTP/1.0 200 OK\r\nContent-Length: 33554432\r\n\r\n[]";
        assert!(stream_response(&response[..]).is_none());
    }

    #[test]
    fn streaming_reads_exact_content_length_body() {
        let response = b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\n[]trailing";
        assert_eq!(stream_response(&response[..]).as_deref(), Some("[]"));
    }

    #[test]
    fn streaming_rejects_overflowing_chunk_size() {
        let response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n[]\r\n0\r\n\r\n";
        assert!(stream_response(&response[..]).is_none());

        let after_first_chunk =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\nffffffffffffffff\r\n";
        assert!(
            stream_response(&after_first_chunk[..]).is_none(),
            "length + chunk size overflow must be an error, not a panic"
        );
    }

    #[test]
    fn streaming_rejects_oversize_chunked_body() {
        // The second chunk pushes the cumulative body one byte past the
        // cap. It must be rejected before any of its data is read.
        let response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n{:x}\r\n",
            MAX_RESPONSE_BODY - 1
        );
        assert!(stream_response(response.as_bytes()).is_none());
    }

    #[test]
    fn streaming_rejects_endless_chunk_size_line() {
        let head = &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1"[..];
        assert!(stream_response(head.chain(std::io::repeat(b'0'))).is_none());
    }

    #[test]
    fn streaming_rejects_endless_header_line() {
        // A newline-free stream must be cut off at the header cap rather
        // than buffered until EOF (which never comes here).
        let head = &b"HTTP/1.0 200 OK\r\nX-Endless: "[..];
        assert!(stream_response(head.chain(std::io::repeat(b'a'))).is_none());
    }

    #[test]
    fn streaming_rejects_unbounded_body_without_content_length() {
        let head = &b"HTTP/1.0 200 OK\r\n\r\n"[..];
        assert!(stream_response(head.chain(std::io::repeat(b'a'))).is_none());
    }

    #[test]
    fn streaming_stops_consuming_endless_trailers() {
        let head =
            &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\nX-T: "[..];
        assert_eq!(
            stream_response(head.chain(std::io::repeat(b'a'))).as_deref(),
            Some("[]"),
            "trailer consumption is bounded and the complete body is kept"
        );
    }

    // ── POST status classification ───────────────────────────────────

    /// Stream whose writes fail and whose reads return `read_error`, if any.
    struct FailingStream {
        write_error: Option<std::io::ErrorKind>,
        read_error: std::io::ErrorKind,
    }

    impl Read for FailingStream {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(self.read_error.into())
        }
    }

    impl std::io::Write for FailingStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.write_error
                .map_or(Ok(buf.len()), |kind| Err(kind.into()))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn post_status(reader: impl Read) -> Result<u16, StatusFailure> {
        send_http_post_status(&mut MockStream { reader }, "/containers/abc/stop")
    }

    #[test]
    fn post_status_reads_status_code() {
        assert_eq!(
            post_status(&b"HTTP/1.0 204 No Content\r\n\r\n"[..]),
            Ok(204)
        );
    }

    #[test]
    fn post_status_classifies_eof_after_request_as_no_reply() {
        assert_eq!(
            post_status(&b""[..]),
            Err(StatusFailure::NoReply),
            "a daemon may have read the request before closing without a reply"
        );
    }

    #[test]
    fn post_status_classifies_partial_reply_as_no_reply() {
        assert_eq!(
            post_status(&b"HTTP/1.0 20"[..]),
            Err(StatusFailure::NoReply),
            "a daemon that started replying may have acted on the request"
        );
    }

    #[test]
    fn post_status_classifies_read_errors_after_request_as_no_reply() {
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::WouldBlock,
        ] {
            let mut stream = FailingStream {
                write_error: None,
                read_error: kind,
            };
            assert_eq!(
                send_http_post_status(&mut stream, "/containers/abc/kill"),
                Err(StatusFailure::NoReply),
                "{kind:?} after the request was written means a daemon may be acting on it"
            );
        }
    }

    #[test]
    fn post_status_classifies_failed_write_as_not_sent() {
        let mut stream = FailingStream {
            write_error: Some(std::io::ErrorKind::BrokenPipe),
            read_error: std::io::ErrorKind::TimedOut,
        };
        assert_eq!(
            send_http_post_status(&mut stream, "/containers/abc/kill"),
            Err(StatusFailure::NotSent),
            "a request that was never written cannot have been acted on"
        );
    }

    #[test]
    fn status_request_sends_ping_and_reads_status() {
        let mut stream = MockStream {
            reader: &b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nOK"[..],
        };
        assert_eq!(
            send_http_status_request(&mut stream, PING_HTTP_REQUEST),
            Ok(200),
            "any HTTP status from the ping endpoint is reported"
        );
    }
}
