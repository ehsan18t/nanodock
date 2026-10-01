//! Minimal HTTP/1.0 response parser for Docker daemon replies.
//!
//! Header parsing is handled by `httparse`; chunked transfer-encoding
//! body framing uses `httparse::parse_chunk_size`. Two code paths exist:
//!
//! - **Streaming** (`send_http_request`): reads from a `BufRead` trait
//!   object (Unix sockets, TCP). Headers are consumed line-by-line so the
//!   reader is left positioned at the body start.
//!
//! - **Buffered** (`send_http_request_windows`, via `parse_response_headers`):
//!   operates on an already-collected `&[u8]` buffer (Windows named pipes
//!   with polled I/O). Headers are parsed from the accumulated buffer and
//!   the body is extracted once complete.
//!
//! The dual implementation is an architectural necessity: Windows named
//! pipes use non-blocking peek-and-read loops that accumulate into a
//! single buffer, while Unix/TCP sockets use blocking `BufReader` I/O.

use std::io::{BufRead, BufReader, Read};

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
    /// Byte offset where the response body begins (after `\r\n\r\n`).
    #[cfg(any(windows, test))]
    pub body_offset: usize,
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

/// Raw HTTP/1.0 request sent to the Docker/Podman daemon to list running
/// containers. The API version prefix is intentionally omitted so the daemon
/// uses its own default, avoiding 400 errors on older engines.
pub const CONTAINERS_HTTP_REQUEST: &[u8] =
    b"GET /containers/json HTTP/1.0\r\nHost: localhost\r\n\r\n";

// ---------------------------------------------------------------------------
// Streaming path (Unix / TCP)
// ---------------------------------------------------------------------------

/// Send the container-list request and read the complete response body.
pub fn send_http_request(stream: &mut (impl Read + std::io::Write)) -> Option<String> {
    stream.write_all(CONTAINERS_HTTP_REQUEST).ok()?;

    let mut reader = BufReader::new(stream);

    let headers = read_response_headers(&mut reader)?;
    if !headers.status_ok {
        return None;
    }

    read_response_body(&mut reader, &headers)
}

/// Read HTTP response headers from a buffered reader using `httparse`.
///
/// Reads raw bytes until the header/body boundary (empty `\r\n` line),
/// then delegates to `httparse::Response::parse` for robust parsing.
/// The reader is left positioned at the start of the response body.
fn read_response_headers(reader: &mut impl BufRead) -> Option<ParsedHeaders> {
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
        let budget = MAX_HEADER_SIZE.checked_sub(start)?;
        if read_line_bounded(reader, &mut raw, budget)? == 0 {
            return None;
        }
        let line = &raw[start..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }

    let mut headers_buf = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_buf);

    if response.parse(&raw).ok()?.is_partial() {
        return None;
    }

    let status_code = response.code.unwrap_or(0);
    let status_ok = (200..300).contains(&status_code);
    let (content_length, transfer_encoding) = extract_header_metadata(response.headers);

    Some(ParsedHeaders {
        status_ok,
        status_code,
        #[cfg(any(windows, test))]
        body_offset: 0,
        content_length,
        transfer_encoding,
    })
}

fn read_response_body(reader: &mut impl BufRead, headers: &ParsedHeaders) -> Option<String> {
    let body = match headers.transfer_encoding {
        TransferEncoding::Identity => match headers.content_length {
            Some(content_length) => read_exact_body(reader, content_length)?,
            None => read_body_to_eof(reader)?,
        },
        TransferEncoding::Chunked => read_chunked_body(reader)?,
        TransferEncoding::Unsupported => return None,
    };
    String::from_utf8(body).ok()
}

/// Read exactly `content_length` body bytes.
///
/// Lengths above [`MAX_RESPONSE_BODY`] are rejected up front. The buffer is
/// never sized from the header alone: it starts small and grows only as
/// bytes are actually received, so a lying `Content-Length` cannot trigger
/// a huge allocation.
fn read_exact_body(reader: &mut impl BufRead, content_length: usize) -> Option<Vec<u8>> {
    if content_length > MAX_RESPONSE_BODY {
        return None;
    }
    let mut body = Vec::with_capacity(content_length.min(INITIAL_BODY_CAPACITY));
    let limit = u64::try_from(content_length).ok()?;
    let read = Read::take(&mut *reader, limit)
        .read_to_end(&mut body)
        .ok()?;
    // A short read means the daemon closed the connection mid-body.
    (read == content_length).then_some(body)
}

/// Read the body until EOF, failing if it exceeds [`MAX_RESPONSE_BODY`].
fn read_body_to_eof(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    // Allow one byte past the cap so an oversize body is detectable.
    let limit = u64::try_from(MAX_RESPONSE_BODY.checked_add(1)?).ok()?;
    let mut body = Vec::new();
    Read::take(&mut *reader, limit)
        .read_to_end(&mut body)
        .ok()?;
    (body.len() <= MAX_RESPONSE_BODY).then_some(body)
}

/// Append one `\n`-terminated line from `reader` to `buf`, reading at most
/// `max_len` bytes.
///
/// Returns the number of bytes appended (`0` at EOF), or `None` on an I/O
/// error or when the line does not terminate within `max_len` bytes.
fn read_line_bounded(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
    max_len: usize,
) -> Option<usize> {
    // Allow one byte past the budget so an overlong line is detectable.
    let limit = u64::try_from(max_len.checked_add(1)?).ok()?;
    let read = Read::take(&mut *reader, limit)
        .read_until(b'\n', buf)
        .ok()?;
    (read <= max_len).then_some(read)
}

fn read_chunked_body(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut size_line = Vec::with_capacity(16);

    loop {
        size_line.clear();
        if read_line_bounded(reader, &mut size_line, MAX_CHUNK_LINE)? == 0 {
            return None;
        }

        let chunk_size = parse_streaming_chunk_size(&size_line)?;
        if chunk_size == 0 {
            consume_chunked_trailers(reader);
            return Some(body);
        }

        // Enforce the cap on the cumulative decoded body. `checked_add`
        // guards against chunk sizes near `usize::MAX`.
        let expected_len = body.len().checked_add(chunk_size)?;
        if expected_len > MAX_RESPONSE_BODY {
            return None;
        }

        // Grow the buffer as bytes arrive instead of pre-sizing it from
        // the untrusted chunk header.
        let limit = u64::try_from(chunk_size).ok()?;
        Read::take(&mut *reader, limit)
            .read_to_end(&mut body)
            .ok()?;
        if body.len() != expected_len {
            return None;
        }

        let mut chunk_terminator = [0_u8; 2];
        reader.read_exact(&mut chunk_terminator).ok()?;
        if chunk_terminator != *b"\r\n" {
            return None;
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
        let Some(bytes_read) = read_line_bounded(reader, &mut trailer_line, budget) else {
            return;
        };
        if bytes_read == 0 || trailer_line.trim_ascii().is_empty() {
            return;
        }
        budget = budget.saturating_sub(bytes_read);
    }
}

// ---------------------------------------------------------------------------
// POST request helpers (container stop / kill)
// ---------------------------------------------------------------------------

/// Build an HTTP/1.0 POST request for the given daemon endpoint path.
pub fn format_post_request(path: &str) -> Vec<u8> {
    format!("POST {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").into_bytes()
}

/// Send an HTTP POST request and return the response status code.
///
/// Used for Docker API calls that return no body (e.g. container stop/kill
/// which respond with 204 No Content). Only the status code is needed.
pub fn send_http_post_status(stream: &mut (impl Read + std::io::Write), path: &str) -> Option<u16> {
    stream.write_all(&format_post_request(path)).ok()?;
    let mut reader = BufReader::new(stream);
    let headers = read_response_headers(&mut reader)?;
    Some(headers.status_code)
}

// ---------------------------------------------------------------------------
// Buffered path (Windows named pipes / tests)
// ---------------------------------------------------------------------------

/// Progress of parsing the response headers out of a buffered reply.
#[cfg(any(windows, test))]
pub enum HeaderState {
    /// The header/body boundary has not arrived yet.
    Pending,
    /// The headers are complete.
    Complete(ParsedHeaders),
    /// The headers are malformed or exceed [`MAX_HEADER_SIZE`]; more bytes
    /// cannot fix them.
    Invalid,
}

/// Parse the HTTP response headers buffered so far in `response`.
///
/// Enforces [`MAX_HEADER_SIZE`] on the buffered path: once more bytes than
/// the cap are buffered without a complete header block, or a complete
/// block is larger than the cap, the headers are [`HeaderState::Invalid`].
/// An `httparse` error is also invalid rather than "still waiting".
#[cfg(any(windows, test))]
pub fn response_header_state(response: &[u8]) -> HeaderState {
    let mut headers_buf = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers_buf);

    let body_offset = match parsed.parse(response) {
        Ok(httparse::Status::Complete(body_offset)) if body_offset <= MAX_HEADER_SIZE => {
            body_offset
        }
        Ok(httparse::Status::Partial) if response.len() <= MAX_HEADER_SIZE => {
            return HeaderState::Pending;
        }
        Ok(_) | Err(_) => return HeaderState::Invalid,
    };

    let status_code = parsed.code.unwrap_or(0);
    let status_ok = (200..300).contains(&status_code);
    let (content_length, transfer_encoding) = extract_header_metadata(parsed.headers);

    HeaderState::Complete(ParsedHeaders {
        status_ok,
        status_code,
        body_offset,
        content_length,
        transfer_encoding,
    })
}

/// Try to locate and parse the HTTP response headers in `response`.
///
/// Returns `None` if the header/body boundary (`\r\n\r\n`) has not yet
/// been received or the headers are invalid (see [`response_header_state`]).
#[cfg(any(windows, test))]
pub fn parse_response_headers(response: &[u8]) -> Option<ParsedHeaders> {
    match response_header_state(response) {
        HeaderState::Complete(headers) => Some(headers),
        HeaderState::Pending | HeaderState::Invalid => None,
    }
}

/// Extract the body from a fully received (EOF) response, using
/// pre-parsed headers if available, or falling back to a full parse.
#[cfg(any(windows, test))]
pub fn extract_body_at_eof(response: &[u8], headers: Option<&ParsedHeaders>) -> Option<String> {
    if let Some(hdr) = headers {
        return extract_http_body_from_buffer(response, hdr, true)
            .ok()
            .flatten();
    }
    // Headers not yet parsed at EOF: fall back to a full single-pass parse.
    try_extract_http_body(response, true)
}

#[cfg(any(windows, test))]
pub fn try_extract_http_body(response: &[u8], eof: bool) -> Option<String> {
    let hdr = parse_response_headers(response)?;
    extract_http_body_from_buffer(response, &hdr, eof)
        .ok()
        .flatten()
}

#[cfg(any(windows, test))]
pub fn extract_http_body_from_buffer(
    response: &[u8],
    headers: &ParsedHeaders,
    eof: bool,
) -> Result<Option<String>, ()> {
    if !headers.status_ok {
        return Err(());
    }

    let body = response.get(headers.body_offset..).ok_or(())?;
    match headers.transfer_encoding {
        TransferEncoding::Identity => {
            if let Some(content_length) = headers.content_length {
                if content_length > MAX_RESPONSE_BODY {
                    return Err(());
                }
                if body.len() < content_length {
                    return Ok(None);
                }
                return String::from_utf8(body[..content_length].to_vec())
                    .map(Some)
                    .map_err(|_| ());
            }

            // Without a length, the body runs to EOF. Fail as soon as the
            // buffered bytes exceed the cap so the caller stops reading.
            if body.len() > MAX_RESPONSE_BODY {
                return Err(());
            }

            if eof {
                return String::from_utf8(body.to_vec()).map(Some).map_err(|_| ());
            }

            Ok(None)
        }
        TransferEncoding::Chunked => match decode_chunked_body(body, eof) {
            Ok(Some(decoded)) => String::from_utf8(decoded).map(Some).map_err(|_| ()),
            Ok(None) => Ok(None),
            Err(()) => Err(()),
        },
        TransferEncoding::Unsupported => Err(()),
    }
}

#[cfg(any(windows, test))]
fn decode_chunked_body(body: &[u8], eof: bool) -> Result<Option<Vec<u8>>, ()> {
    let mut decoded = Vec::new();
    let mut offset = 0;

    loop {
        let Some(line_end) = find_crlf(body, offset) else {
            // An unterminated chunk-size line longer than any legitimate
            // one will never complete; fail instead of buffering forever.
            if body.len().saturating_sub(offset) > MAX_CHUNK_LINE {
                return Err(());
            }
            return Ok(None);
        };

        let chunk_line = body.get(offset..line_end + 2).ok_or(())?;
        let chunk_size = match httparse::parse_chunk_size(chunk_line) {
            Ok(httparse::Status::Complete((_, size))) => usize::try_from(size).map_err(|_| ())?,
            _ => return Err(()),
        };
        offset = line_end + 2;

        if chunk_size == 0 {
            return parse_chunked_trailers(body, offset, eof)
                .map(|complete| complete.then_some(decoded));
        }

        // Enforce the cap on the cumulative decoded body before waiting
        // for (or copying) the chunk data.
        let decoded_len = decoded.len().checked_add(chunk_size).ok_or(())?;
        if decoded_len > MAX_RESPONSE_BODY {
            return Err(());
        }

        let chunk_end = offset.checked_add(chunk_size).ok_or(())?;
        let terminator_end = chunk_end.checked_add(2).ok_or(())?;
        if body.len() < terminator_end {
            return Ok(None);
        }
        if &body[chunk_end..terminator_end] != b"\r\n" {
            return Err(());
        }

        decoded.extend_from_slice(&body[offset..chunk_end]);
        offset = terminator_end;
    }
}

#[cfg(any(windows, test))]
fn parse_chunked_trailers(body: &[u8], offset: usize, eof: bool) -> Result<bool, ()> {
    let trailers = body.get(offset..).ok_or(())?;
    if trailers.starts_with(b"\r\n") {
        return Ok(true);
    }

    if trailers.windows(4).any(|window| window == b"\r\n\r\n") {
        return Ok(true);
    }

    // At EOF, accept the body even without trailing CRLF since
    // all chunk data including the terminal chunk has been received.
    Ok(eof)
}

#[cfg(any(windows, test))]
fn find_crlf(body: &[u8], offset: usize) -> Option<usize> {
    body.get(offset..)?
        .windows(2)
        .position(|window| window == b"\r\n")
        .map(|position| offset + position)
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
    fn http_body_parser_waits_for_complete_content_length() {
        let partial = b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\n123";
        assert!(try_extract_http_body(partial, false).is_none());

        let complete = b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\n12345";
        assert_eq!(
            try_extract_http_body(complete, false).as_deref(),
            Some("12345")
        );
    }

    #[test]
    fn http_body_parser_accepts_eof_without_content_length() {
        let response = b"HTTP/1.0 200 OK\r\nServer: docker\r\n\r\n[]";
        assert_eq!(try_extract_http_body(response, true).as_deref(), Some("[]"));
    }

    #[test]
    fn http_body_parser_decodes_chunked_payloads() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n\r\n";
        assert_eq!(
            try_extract_http_body(response, false).as_deref(),
            Some("[]")
        );
    }

    #[test]
    fn http_body_parser_waits_for_complete_chunked_payload() {
        let partial = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n";
        assert!(
            try_extract_http_body(partial, false).is_none(),
            "missing trailing CRLF without EOF should remain incomplete"
        );
    }

    #[test]
    fn chunked_body_accepted_at_eof_without_trailing_crlf() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n";
        assert_eq!(
            try_extract_http_body(response, true).as_deref(),
            Some("[]"),
            "at EOF the body should be accepted since all chunks are complete"
        );
    }

    #[test]
    fn streaming_chunked_body_accepted_at_eof_without_trailing_crlf() {
        struct MockDaemonStream {
            reader: std::io::Cursor<Vec<u8>>,
        }

        impl std::io::Read for MockDaemonStream {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.reader.read(buf)
            }
        }

        impl std::io::Write for MockDaemonStream {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let response_data =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n0\r\n";
        let mut stream = MockDaemonStream {
            reader: std::io::Cursor::new(response_data.to_vec()),
        };
        let body = send_http_request(&mut stream);
        assert_eq!(
            body.as_deref(),
            Some("[]"),
            "streaming path should accept chunked body when server closes after terminal chunk"
        );
    }

    #[test]
    fn http_body_parser_rejects_unsupported_transfer_encoding() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n";
        assert!(try_extract_http_body(response, false).is_none());
    }

    #[test]
    fn parse_response_headers_returns_none_for_incomplete_headers() {
        let partial = b"HTTP/1.0 200 OK\r\nContent-Len";
        assert!(
            parse_response_headers(partial).is_none(),
            "incomplete headers should return None"
        );
    }

    #[test]
    fn parse_response_headers_extracts_content_length_and_offset() {
        let response = b"HTTP/1.0 200 OK\r\nContent-Length: 42\r\n\r\nbody";
        let hdr = parse_response_headers(response).expect("headers should parse");
        assert!(hdr.status_ok, "status should be ok");
        assert_eq!(hdr.content_length, Some(42));
        assert_eq!(hdr.transfer_encoding, TransferEncoding::Identity);
        assert_eq!(hdr.body_offset, 39, "body should start after CRLFCRLF");
    }

    #[test]
    fn parse_response_headers_detects_chunked_transfer_encoding() {
        let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let hdr = parse_response_headers(response).expect("headers should parse");
        assert_eq!(hdr.transfer_encoding, TransferEncoding::Chunked);
    }

    #[test]
    fn parse_response_headers_detects_non_2xx_status() {
        let response = b"HTTP/1.0 404 Not Found\r\n\r\n";
        let hdr = parse_response_headers(response).expect("headers should parse");
        assert!(!hdr.status_ok, "404 should not be marked as ok");
    }

    #[test]
    fn extract_body_at_eof_returns_body_without_content_length() {
        let response = b"HTTP/1.0 200 OK\r\nServer: docker\r\n\r\n[1,2]";
        let hdr = parse_response_headers(response).unwrap();
        let body = extract_body_at_eof(response, Some(&hdr));
        assert_eq!(body.as_deref(), Some("[1,2]"));
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
        send_http_request(&mut MockStream { reader })
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

    #[test]
    fn buffered_rejects_content_length_above_cap() {
        let response = b"HTTP/1.0 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n[]";
        let hdr = parse_response_headers(response).expect("headers should parse");
        assert_eq!(
            extract_http_body_from_buffer(response, &hdr, false),
            Err(())
        );
        assert!(extract_body_at_eof(response, Some(&hdr)).is_none());
    }

    #[test]
    fn buffered_rejects_overflowing_chunk_size() {
        let response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\nffffffffffffffff\r\n";
        let hdr = parse_response_headers(response).expect("headers should parse");
        assert_eq!(
            extract_http_body_from_buffer(response, &hdr, false),
            Err(())
        );
    }

    #[test]
    fn buffered_rejects_oversize_chunked_body() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n[]\r\n{:x}\r\n",
            MAX_RESPONSE_BODY - 1
        );
        let hdr = parse_response_headers(response.as_bytes()).expect("headers should parse");
        assert_eq!(
            extract_http_body_from_buffer(response.as_bytes(), &hdr, false),
            Err(())
        );
    }

    #[test]
    fn buffered_rejects_endless_chunk_size_line() {
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        response.resize(response.len() + MAX_CHUNK_LINE + 1, b'0');
        let hdr = parse_response_headers(&response).expect("headers should parse");
        assert_eq!(
            extract_http_body_from_buffer(&response, &hdr, false),
            Err(())
        );
    }

    #[test]
    fn extract_body_at_eof_falls_back_when_no_headers_parsed() {
        let response = b"HTTP/1.0 200 OK\r\n\r\nhello";
        let body = extract_body_at_eof(response, None);
        assert_eq!(body.as_deref(), Some("hello"));
    }

    // ── buffered header cap ──────────────────────────────────────────

    #[test]
    fn header_state_waits_for_short_partial_headers() {
        assert!(matches!(
            response_header_state(b"HTTP/1.0 200 OK\r\nServer: dock"),
            HeaderState::Pending
        ));
    }

    #[test]
    fn header_state_rejects_partial_headers_above_cap() {
        let mut response = b"HTTP/1.0 200 OK\r\nX-Pad: ".to_vec();
        response.resize(MAX_HEADER_SIZE + 1, b'a');
        assert!(
            matches!(response_header_state(&response), HeaderState::Invalid),
            "an unterminated header block above the cap must fail, not wait"
        );
    }

    #[test]
    fn header_state_rejects_complete_headers_above_cap() {
        let mut response = b"HTTP/1.0 200 OK\r\nX-Pad: ".to_vec();
        response.resize(MAX_HEADER_SIZE, b'a');
        response.extend_from_slice(b"\r\n\r\n[]");
        assert!(
            matches!(response_header_state(&response), HeaderState::Invalid),
            "a header block larger than the cap must fail"
        );
    }

    #[test]
    fn header_state_treats_parse_errors_as_invalid() {
        assert!(
            matches!(
                response_header_state(b"NOT-HTTP garbage\r\n\r\n"),
                HeaderState::Invalid
            ),
            "a malformed status line can never complete"
        );
    }
}
