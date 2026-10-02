//! # `nanodock`
//!
//! Minimal-dependency, synchronous Docker/Podman daemon client for container
//! detection, port mapping, and lifecycle control. Runtime dependencies are
//! `serde`, `serde_json`, `httparse`, and `log`, plus `libc` on Unix.
//!
//! ## Overview
//!
//! - [`Client`] holds the daemon settings (home directory, detection
//!   timeout, `DOCKER_HOST` override) and runs detection, stop, and kill
//!   requests. [`detect_containers`], [`start_detection`],
//!   [`stop_container`], and [`kill_container`] are shorthands for a default
//!   client.
//! - Detection returns a [`ContainerPortMap`] from published
//!   `(host_ip, port, protocol)` bindings to [`ContainerInfo`].
//!   [`ContainerPortMap::lookup`] finds the container behind a local socket.
//! - Failures are reported as an [`Error`] that says what happened, and stop
//!   and kill requests as a [`StopOutcome`].
//!
//! ## Cargo features
//!
//! - `serde` (off by default): derives `Serialize` and `Deserialize` for
//!   [`ContainerInfo`], [`Protocol`], [`StopOutcome`], and [`ProxyFallback`].
//!   The daemon's JSON is parsed with `serde` either way; the feature only
//!   adds the derives on the public types.
//!
//! ## Module structure
//!
//! - `error` - [`Error`] and [`ParseError`].
//! - `port_map` - [`Protocol`], [`ContainerInfo`], [`ContainerPortMap`], and
//!   port-to-container matching.
//! - `client` - [`Client`] and [`DetectionHandle`]: endpoint selection,
//!   detection queries, and merging the replies.
//! - `stop` - [`StopOutcome`] and the stop and kill logic.
//! - `api` - JSON response parsing and container name resolution.
//! - `http` - Minimal HTTP/1.0 response parser (headers via `httparse`).
//! - `ipc` - OS-specific transport (Unix socket, Windows named pipe, TCP).
//! - `podman` - Rootless Podman resolver via overlay metadata (lookup runs on
//!   Linux only).
//! - `proxy` - Recognition of container runtime port-proxy processes.
//!
//! ## Quick start
//!
//! ### Best-effort path (background thread, never errors)
//!
//! ```rust,no_run
//! use nanodock::start_detection;
//!
//! let handle = start_detection();
//! // ... do other work while detection runs in the background ...
//! let port_map = handle.wait();
//! for ((ip, port, proto), info) in &port_map {
//!     println!("{proto} port {port} -> {} ({})", info.name, info.image);
//! }
//! ```
//!
//! ### Strict path (synchronous, returns errors)
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use nanodock::{Client, Error};
//!
//! let client = Client::new().timeout(Duration::from_secs(2));
//! match client.detect() {
//!     Ok(port_map) => {
//!         for ((ip, port, proto), info) in &port_map {
//!             println!("{proto} port {port} -> {} ({})", info.name, info.image);
//!         }
//!     }
//!     Err(Error::PermissionDenied { endpoint, .. }) => {
//!         eprintln!("no permission to use {endpoint}");
//!     }
//!     Err(e) => eprintln!("detection failed: {e}"),
//! }
//! ```
//!
//! ### Which container owns a socket?
//!
//! ```rust,no_run
//! use std::net::{IpAddr, Ipv4Addr};
//! use nanodock::{Protocol, ProxyFallback};
//!
//! let port_map = nanodock::start_detection().wait();
//! let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
//! if let Some(info) = port_map
//!     .lookup(ip, 5432, Protocol::Tcp, ProxyFallback::Deny)
//!     .container()
//! {
//!     println!("port 5432 belongs to {info}");
//! }
//! ```

mod api;
mod client;
mod error;
mod http;
mod ipc;
mod podman;
mod port_map;
mod proxy;
mod stop;

// Compiles the README examples as doctests so they cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

// ── Public API re-exports ────────────────────────────────────────────

pub use api::parse_containers_json;
pub use api::parse_containers_json_strict;
pub use api::short_container_id;
pub use client::{Client, DetectionHandle};
pub use error::{Error, ParseError};
pub use podman::RootlessPodmanResolver;
pub use podman::is_podman_rootlessport_process;
pub use port_map::{
    ContainerInfo, ContainerPortMap, PortKey, PortMapIter, Protocol, ProxyFallback,
    PublishedContainerMatch,
};
pub use proxy::is_container_proxy_process;
pub use stop::StopOutcome;

// ── Convenience functions ────────────────────────────────────────────

/// Synchronously detect containers with a default [`Client`].
///
/// Shorthand for `Client::new().detect()`; see [`Client::detect`]. The
/// default client reads `DOCKER_HOST` and the home directory from the
/// environment, so the per-user sockets below the home directory (Docker
/// Desktop on macOS, Colima, Lima, Rancher Desktop, Podman machine, and
/// others) are found. Use [`Client::home`] to search a different home
/// directory.
///
/// # Errors
///
/// See [`Client::detect`].
pub fn detect_containers() -> Result<ContainerPortMap, Error> {
    Client::new().detect()
}

/// Start detection on a background thread with a default [`Client`].
///
/// Shorthand for `Client::new().start_detection()`; see
/// [`Client::start_detection`] and, for the default settings,
/// [`Client::new`].
#[must_use]
pub fn start_detection() -> DetectionHandle {
    Client::new().start_detection()
}

/// Stop a container gracefully with a default [`Client`].
///
/// Shorthand for `Client::new().stop(id)`; see [`Client::stop`] and, for
/// the default settings, [`Client::new`].
#[must_use]
pub fn stop_container(id: &str) -> StopOutcome {
    Client::new().stop(id)
}

/// Kill a container at once with a default [`Client`].
///
/// Shorthand for `Client::new().kill(id)`; see [`Client::kill`] and, for
/// the default settings, [`Client::new`].
#[must_use]
pub fn kill_container(id: &str) -> StopOutcome {
    Client::new().kill(id)
}

// The only crate-level test checks the serde derives across modules.
#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    #[test]
    fn public_types_round_trip_through_serde() {
        let info = ContainerInfo::new("abc", "web", "nginx").with_compose_project("shop");
        let json = serde_json::to_string(&info).expect("serialize");
        let back: ContainerInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, info);

        let legacy: ContainerInfo =
            serde_json::from_str(r#"{"id":"abc","name":"web","image":"nginx"}"#)
                .expect("0.1 records without compose fields still deserialize");
        assert_eq!(legacy, ContainerInfo::new("abc", "web", "nginx"));

        assert_eq!(
            serde_json::to_string(&Protocol::Tcp).expect("serialize"),
            r#""TCP""#
        );
        let outcome = StopOutcome::Rejected { status: 500 };
        let json = serde_json::to_string(&outcome).expect("serialize");
        assert_eq!(
            serde_json::from_str::<StopOutcome>(&json).expect("deserialize"),
            outcome
        );
    }
}
