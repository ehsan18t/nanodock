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
    // since httparse operates on raw bytes anyway.
    loop {
        let start = raw.len();
        if reader.read_until(b'\n', &mut raw).ok()? == 0 {
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
    match headers.transfer_encoding {
        TransferEncoding::Identity => {
            if let Some(content_length) = headers.content_length {
                let mut body = vec![0_u8; content_length];
                reader.read_exact(&mut body).ok()?;
                String::from_utf8(body).ok()
            } else {
                let mut body = Vec::new();
                reader.read_to_end(&mut body).ok()?;
                String::from_utf8(body).ok()
            }
        }
        TransferEncoding::Chunked => {
            let decoded = read_chunked_body(reader)?;
            String::from_utf8(decoded).ok()
        }
        TransferEncoding::Unsupported => None,
    }
}

fn read_chunked_body(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut body = Vec::new();

    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line).ok()? == 0 {
            return None;
        }

        let chunk_size = parse_streaming_chunk_size(&size_line)?;
        if chunk_size == 0 {
            consume_chunked_trailers(reader);
            return Some(body);
        }

        let start = body.len();
        body.resize(start + chunk_size, 0);
        reader.read_exact(&mut body[start..]).ok()?;

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
/// reader path where we have a single line as a string.
fn parse_streaming_chunk_size(line: &str) -> Option<usize> {
    match httparse::parse_chunk_size(line.as_bytes()) {
        Ok(httparse::Status::Complete((_, size))) => usize::try_from(size).ok(),
        _ => None,
    }
}

fn consume_chunked_trailers(reader: &mut impl BufRead) {
    loop {
        let mut trailer_line = String::new();
        // EOF or IO error after the terminal chunk means the body is
        // complete and there are no (more) trailers to consume.
        let bytes_read = reader.read_line(&mut trailer_line).unwrap_or(0);
        if bytes_read == 0 || trailer_line.trim().is_empty() {
            return;
        }
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

/// Try to locate and parse the HTTP response headers in `response`.
///
/// Returns `None` if the header/body boundary (`\r\n\r\n`) has not yet
/// been received. Uses `httparse::Response::parse` for robust parsing.
#[cfg(any(windows, test))]
pub fn parse_response_headers(response: &[u8]) -> Option<ParsedHeaders> {
    let mut headers_buf = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Response::new(&mut headers_buf);

    let Ok(httparse::Status::Complete(body_offset)) = parsed.parse(response) else {
        return None;
    };

    let status_code = parsed.code.unwrap_or(0);
    let status_ok = (200..300).contains(&status_code);
    let (content_length, transfer_encoding) = extract_header_metadata(parsed.headers);

    Some(ParsedHeaders {
        status_ok,
        status_code,
        body_offset,
        content_length,
        transfer_encoding,
    })
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
                if body.len() < content_length {
                    return Ok(None);
                }
                return String::from_utf8(body[..content_length].to_vec())
                    .map(Some)
                    .map_err(|_| ());
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

    #[test]
    fn extract_body_at_eof_falls_back_when_no_headers_parsed() {
        let response = b"HTTP/1.0 200 OK\r\n\r\nhello";
        let body = extract_body_at_eof(response, None);
        assert_eq!(body.as_deref(), Some("hello"));
    }
}
