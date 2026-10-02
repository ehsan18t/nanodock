//! Helpers shared by the live-daemon integration tests (`daemon_it.rs` and
//! `podman_it.rs`).
//!
//! Every container name and port the tests expect comes from an environment
//! variable with a default, so the CI workflow that starts the containers and
//! the tests that inspect them read the same values.

#![allow(
    dead_code,
    reason = "each integration test crate uses a different subset of these helpers"
)]

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::Duration;

use nanodock::{Client, ContainerInfo, ContainerPortMap, Protocol};

/// Whether the opt-in variable `var` is set to `1`. Prints why the test is
/// skipped otherwise, so `--nocapture` output explains an empty run.
pub fn enabled(var: &str, test: &str) -> bool {
    if std::env::var_os(var).is_some_and(|value| value == "1") {
        return true;
    }
    println!("skipping {test}: set {var}=1 to run it against a live daemon");
    false
}

/// The value of `name`, or `default` when it is unset or empty.
pub fn setting(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// A port number read from `name`, or `default` when it is unset or empty.
pub fn port(name: &str, default: u16) -> u16 {
    let raw = setting(name, &default.to_string());
    raw.parse()
        .unwrap_or_else(|error| panic!("{name}={raw:?} is not a port number: {error}"))
}

/// A client with a timeout generous enough for a busy CI runner.
pub fn client() -> Client {
    Client::new().timeout(Duration::from_secs(10))
}

/// Run strict detection and fail the test with the error when it fails.
pub fn detect(client: &Client) -> ContainerPortMap {
    client
        .detect()
        .unwrap_or_else(|error| panic!("detection failed: {error} ({error:?})"))
}

/// The container called `name`, found through any of its bindings.
pub fn find_by_name<'a>(map: &'a ContainerPortMap, name: &str) -> Option<&'a ContainerInfo> {
    map.iter()
        .map(|(_, info)| info)
        .find(|info| info.name == name)
}

/// Every binding the container called `name` publishes, in sorted order.
pub fn bindings_of(
    map: &ContainerPortMap,
    name: &str,
) -> BTreeSet<(Option<IpAddr>, u16, Protocol)> {
    map.iter()
        .filter(|(_, info)| info.name == name)
        .map(|(key, _)| key)
        .collect()
}

/// Names of every detected container, for assertion messages.
pub fn names(map: &ContainerPortMap) -> BTreeSet<String> {
    map.iter().map(|(_, info)| info.name.clone()).collect()
}
