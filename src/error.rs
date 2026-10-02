//! The crate's error types: [`Error`] for failed detection and
//! [`ParseError`] for a daemon reply that could not be parsed.

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
///
/// Every variant that carries data is a `#[non_exhaustive]` struct variant,
/// so later releases can add fields: match it with `..`, as in
/// `Error::HttpStatus { status, .. }`.
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
    #[non_exhaustive]
    PermissionDenied {
        /// The endpoint that refused access, such as
        /// `/var/run/docker.sock`, `\\.\pipe\docker_engine`, or
        /// `tcp://host:port`.
        endpoint: String,
    },

    /// No daemon finished answering within the detection timeout.
    #[non_exhaustive]
    Timeout {
        /// The endpoint that was still answering, when one is known. It is
        /// `None` when the [`DetectionHandle`](crate::DetectionHandle)
        /// stopped waiting for the detection thread.
        endpoint: Option<String>,
    },

    /// A daemon answered with an unexpected (non-2xx) HTTP status.
    #[non_exhaustive]
    HttpStatus {
        /// The HTTP status code of the reply.
        status: u16,
    },

    /// A daemon answered, but the reply was not a valid HTTP response or
    /// container list.
    #[non_exhaustive]
    InvalidResponse {
        /// What was wrong with the reply.
        source: ParseError,
    },

    /// Another I/O error occurred while talking to the daemon.
    #[non_exhaustive]
    Io {
        /// The underlying I/O error.
        source: std::io::Error,
        /// The endpoint the error occurred at, when one is known. It is
        /// `None` when the detection thread itself failed.
        endpoint: Option<String>,
    },
}

impl Error {
    /// How much the error tells the caller; when every endpoint fails,
    /// detection reports the error that ranks highest.
    const fn informativeness(&self) -> u8 {
        match self {
            Self::DaemonNotFound => 0,
            Self::Io { .. } => 1,
            Self::Timeout { .. } => 2,
            Self::PermissionDenied { .. } => 3,
            Self::HttpStatus { .. } => 4,
            Self::InvalidResponse { .. } => 5,
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
            Self::Timeout {
                endpoint: Some(endpoint),
            } => write!(
                f,
                "the container runtime daemon at {endpoint} did not answer in time"
            ),
            Self::Timeout { endpoint: None } => {
                f.write_str("the container runtime daemon did not answer in time")
            }
            Self::HttpStatus { status } => write!(
                f,
                "the container runtime daemon answered with HTTP status {status}"
            ),
            Self::InvalidResponse { .. } => {
                f.write_str("the container runtime daemon sent an invalid response")
            }
            Self::Io {
                endpoint: Some(endpoint),
                ..
            } => write!(
                f,
                "I/O error talking to the container runtime daemon at {endpoint}"
            ),
            Self::Io { endpoint: None, .. } => {
                f.write_str("I/O error talking to the container runtime daemon")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidResponse { source } => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::DaemonNotFound
            | Self::PermissionDenied { .. }
            | Self::Timeout { .. }
            | Self::HttpStatus { .. } => None,
        }
    }
}

impl From<ParseError> for Error {
    fn from(error: ParseError) -> Self {
        Self::InvalidResponse { source: error }
    }
}

/// Pick the most informative of several endpoint failures, keeping the
/// earliest (highest priority) one on a tie.
pub fn most_informative(errors: impl IntoIterator<Item = Error>) -> Error {
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
/// detail. Its [`Display`](std::fmt::Display) output says which part of the
/// reply was wrong; for invalid JSON,
/// [`source`](std::error::Error::source) returns the parser's error, which
/// says where the JSON broke.
#[derive(Debug)]
pub struct ParseError(ParseErrorKind);

#[derive(Debug)]
enum ParseErrorKind {
    Json(serde_json::Error),
    Http(&'static str),
}

impl ParseError {
    /// The container list is not valid JSON.
    pub(crate) const fn json(error: serde_json::Error) -> Self {
        Self(ParseErrorKind::Json(error))
    }

    /// The HTTP reply is malformed.
    pub(crate) const fn http(reason: &'static str) -> Self {
        Self(ParseErrorKind::Http(reason))
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            ParseErrorKind::Json(_) => f.write_str("invalid container list JSON"),
            ParseErrorKind::Http(reason) => write!(f, "malformed HTTP response: {reason}"),
        }
    }
}

impl std::error::Error for ParseError {
    /// The JSON parser's error, for a container list that is not valid
    /// JSON. Its type is not part of the API; use it through `Display`.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0 {
            ParseErrorKind::Json(error) => Some(error),
            ParseErrorKind::Http(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;

    fn permission_denied() -> Error {
        Error::PermissionDenied {
            endpoint: "/var/run/docker.sock".to_string(),
        }
    }

    #[test]
    fn most_informative_prefers_answers_and_keeps_priority_on_ties() {
        let error = most_informative([
            Error::DaemonNotFound,
            Error::Timeout { endpoint: None },
            Error::HttpStatus { status: 500 },
            permission_denied(),
            Error::HttpStatus { status: 503 },
        ]);
        assert!(
            matches!(error, Error::HttpStatus { status: 500 }),
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
            Error::HttpStatus { status: 500 }.to_string(),
            "the container runtime daemon answered with HTTP status 500"
        );
        assert_eq!(
            Error::Timeout {
                endpoint: Some("/run/podman/podman.sock".to_string())
            }
            .to_string(),
            "the container runtime daemon at /run/podman/podman.sock did not answer in time"
        );
        assert_eq!(
            Error::Timeout { endpoint: None }.to_string(),
            "the container runtime daemon did not answer in time"
        );

        let json_error = api::parse_containers_json_strict("not json").expect_err("invalid JSON");
        assert_eq!(json_error.to_string(), "invalid container list JSON");
        let serde_message = json_error
            .source()
            .expect("the JSON parser's error is chained")
            .to_string();
        assert!(
            serde_message.contains("line 1"),
            "the source says where the JSON broke, got {serde_message}"
        );
        assert!(ParseError::http("bad framing").source().is_none());
        let error = Error::from(json_error);
        assert!(
            error.source().is_some(),
            "an invalid response chains the parse error"
        );
        let io = Error::Io {
            source: std::io::ErrorKind::ConnectionReset.into(),
            endpoint: Some("tcp://127.0.0.1:2375".to_string()),
        };
        assert!(io.source().is_some(), "an I/O error chains its source");
        assert_eq!(
            io.to_string(),
            "I/O error talking to the container runtime daemon at tcp://127.0.0.1:2375",
            "the message names the endpoint but not the source"
        );
        assert!(Error::Timeout { endpoint: None }.source().is_none());
    }
}
