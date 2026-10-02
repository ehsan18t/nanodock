//! Stop and kill requests: container ID validation, the endpoint each
//! request is sent to, choosing the daemon that owns the container, and
//! the [`StopOutcome`] reported to the caller.

use log::debug;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::{ipc, short_container_id};

/// Result of attempting to stop or kill a container via the daemon API.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum StopOutcome {
    /// Container was successfully stopped (HTTP 204).
    Stopped,
    /// Container was already stopped (HTTP 304 for stop, 409 for kill).
    AlreadyStopped,
    /// Container was not found (HTTP 404), or the id cannot name a
    /// container.
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
    pub const fn is_stopped(&self) -> bool {
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

/// Which request ends the container: a graceful stop or an immediate kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// `POST /containers/{id}/stop` with an explicit grace period.
    Graceful,
    /// `POST /containers/{id}/kill`.
    Kill,
}

/// Build the stop or kill endpoint for an already validated container id.
pub fn stop_endpoint(id: &str, kind: StopKind) -> String {
    let endpoint = match kind {
        StopKind::Kill => format!("/containers/{id}/kill"),
        // An explicit grace period keeps the daemon's stop time in line
        // with the transport timeout (`ipc::STOP_TIMEOUT`).
        StopKind::Graceful => format!("/containers/{id}/stop?t={}", ipc::STOP_GRACE_SECS),
    };
    debug!(
        "attempting container stop: id={} kind={kind:?} endpoint={endpoint}",
        short_container_id(id)
    );
    endpoint
}

/// Longest container id or name a stop or kill request accepts. A full ID
/// is 64 hex digits; the cap only keeps a hostile id out of the request.
const MAX_CONTAINER_ID_LEN: usize = 256;

/// Whether `id` can name a container: Docker's and Podman's name pattern
/// `[A-Za-z0-9][A-Za-z0-9_.-]*`, which hex IDs and ID prefixes also match,
/// at most [`MAX_CONTAINER_ID_LEN`] bytes long.
///
/// An allow-list keeps everything that could change the request out of the
/// path: `/`, `?`, `#`, `%`, spaces, control characters, `.` and `..`, and
/// every non-ASCII character (line separators, zero-width characters).
pub fn is_safe_container_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() <= MAX_CONTAINER_ID_LEN
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Map the combined result of a stop request to `StopOutcome`.
///
/// "A daemon received the request but gave no usable reply" is
/// [`StopOutcome::NoResponse`], never "not found": the container may or may
/// not have been stopped.
pub fn stop_outcome(attempt: ipc::StopAttempt, kind: StopKind) -> StopOutcome {
    match attempt {
        ipc::StopAttempt::Status(status_code) => interpret_stop_status(status_code, kind),
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
fn interpret_stop_status(status_code: u16, kind: StopKind) -> StopOutcome {
    match status_code {
        204 => StopOutcome::Stopped,
        // POST /containers/{id}/stop returns 304 when already stopped.
        304 => StopOutcome::AlreadyStopped,
        // POST /containers/{id}/kill returns 409 when container is not running.
        409 if kind == StopKind::Kill => StopOutcome::AlreadyStopped,
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
pub fn first_stop_owner<P, I, F>(endpoints: I, mut attempt: F) -> ipc::StopAttempt
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Client;

    // ── interpret_stop_status ────────────────────────────────────────

    #[test]
    fn interpret_stop_status_maps_each_status() {
        use StopKind::{Graceful, Kill};
        let cases = [
            (204, Graceful, StopOutcome::Stopped, "204 means stopped"),
            (204, Kill, StopOutcome::Stopped, "204 means killed"),
            (
                304,
                Graceful,
                StopOutcome::AlreadyStopped,
                "304 from stop means already stopped",
            ),
            (
                409,
                Kill,
                StopOutcome::AlreadyStopped,
                "409 from kill means not running",
            ),
            (
                409,
                Graceful,
                StopOutcome::Rejected { status: 409 },
                "409 on a graceful stop is unexpected",
            ),
            (404, Graceful, StopOutcome::NotFound, "404 means not found"),
            (404, Kill, StopOutcome::NotFound, "404 means not found"),
            (
                500,
                Graceful,
                StopOutcome::Rejected { status: 500 },
                "a server error is reported with its status",
            ),
        ];
        for (status, kind, expected, why) in cases {
            assert_eq!(
                interpret_stop_status(status, kind),
                expected,
                "{status} on {kind:?}: {why}"
            );
        }
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

    // ── is_safe_container_id ─────────────────────────────────────────

    #[test]
    fn safe_id_accepts_only_container_names_and_ids() {
        let longest = "a".repeat(MAX_CONTAINER_ID_LEN);
        let accepted = [
            ("abc123def456", "short hex ID"),
            (
                "e603f8ebd438b8405b9b835b9d38cb913ea2479f5b29f8e4308b88e9a92e8c4b",
                "full hex ID",
            ),
            ("a", "one-character ID prefix"),
            (
                "my-container_1.0",
                "name with hyphens, underscores, and dots",
            ),
            ("9to5", "name starting with a digit"),
            (longest.as_str(), "name at the length cap"),
        ];
        for (id, why) in accepted {
            assert!(is_safe_container_id(id), "{why} should be accepted: {id:?}");
        }

        let too_long = "a".repeat(MAX_CONTAINER_ID_LEN + 1);
        let huge = "a".repeat(100 * 1024);
        let rejected = [
            ("", "empty ID"),
            (".", "current directory"),
            ("..", "parent directory"),
            ("../../../etc/passwd", "path traversal"),
            ("-abc", "leading hyphen"),
            ("_abc", "leading underscore"),
            (".abc", "leading dot"),
            ("abc?signal=SIGKILL", "query injection"),
            ("abc#frag", "fragment"),
            ("abc%2F..", "percent encoding"),
            ("abc def", "space"),
            ("abc\r\nX-Injected: true", "CRLF injection"),
            ("abc\tdef", "tab"),
            ("abc\0", "NUL"),
            ("abc\u{7f}", "DEL"),
            ("abc\u{85}", "C1 control character"),
            ("abc\u{1b}[0m", "escape sequence"),
            ("abc\u{2028}", "line separator"),
            ("abc\u{200b}def", "zero-width space"),
            ("\u{feff}abc", "byte order mark"),
            ("caf\u{e9}-container", "non-ASCII name"),
            (too_long.as_str(), "one byte past the length cap"),
            (huge.as_str(), "100 KiB ID"),
        ];
        for (id, why) in rejected {
            assert!(
                !is_safe_container_id(id),
                "{why} should be rejected: {:?}",
                id.get(..40).unwrap_or(id)
            );
        }
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
            stop_endpoint("abc123", StopKind::Graceful),
            format!("/containers/abc123/stop?t={}", ipc::STOP_GRACE_SECS),
            "graceful stop must pin the grace period the transport timeout is sized for"
        );
        assert_eq!(
            stop_endpoint("abc123", StopKind::Kill),
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

        // Byte 12 falls inside the two-byte 'e9' character. Validation
        // rejects such an id, but logging must not rely on that.
        let id = "aaaaaaaaaaa\u{e9}bc";
        assert_eq!(
            stop_endpoint(id, StopKind::Graceful),
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
            stop_outcome(result, StopKind::Kill),
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
        for kind in [StopKind::Graceful, StopKind::Kill] {
            let attempt = first_stop_owner(
                from_defaults([ipc::StopAttempt::Status(404), ipc::StopAttempt::NoResponse]),
                |attempt| attempt,
            );
            assert_eq!(
                stop_outcome(attempt, kind),
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
            stop_outcome(attempt, StopKind::Graceful),
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
            stop_outcome(attempt, StopKind::Kill),
            StopOutcome::Stopped,
            "only an override that never received the stop falls through"
        );
    }

    #[test]
    fn stop_and_kill_reject_unsafe_ids_before_any_request() {
        let client = Client::new()
            .home(None)
            .docker_host(Some("tcp://127.0.0.1:1".to_string()));
        assert_eq!(client.stop("../etc"), StopOutcome::NotFound);
        assert_eq!(client.kill("abc?signal=HUP"), StopOutcome::NotFound);
    }

    #[test]
    fn stop_outcome_is_checked_by_reference() {
        let outcome = StopOutcome::Rejected { status: 500 };
        assert!(!outcome.is_stopped());
        assert_eq!(
            outcome,
            StopOutcome::Rejected { status: 500 },
            "is_stopped borrows, so the outcome stays usable"
        );
    }
}
