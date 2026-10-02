//! Integration tests against a live rootless Podman API socket, alongside the
//! rootful Docker Engine of a CI runner.
//!
//! Skipped unless `NANODOCK_IT_PODMAN=1`. The `podman-it` CI job enables the
//! user's Podman socket, starts `pweb` with rootless Podman (slirp4netns, so
//! the `rootlessport` helper holds the host port) and `nd-web` with Docker,
//! and runs each test in its own step. Names and ports come from the
//! `NANODOCK_IT_*` variables below, with the same defaults the workflow uses.
//!
//! `stop_falls_through_docker_to_podman` stops `pweb`, so CI runs it last.

#![allow(missing_docs, reason = "integration tests document behavior via names")]

mod it_support;

use std::net::{IpAddr, Ipv4Addr};

use it_support::{client, detect, enabled, find_by_name, names, port, setting};
use nanodock::{Protocol, ProxyFallback, StopOutcome};

const GATE: &str = "NANODOCK_IT_PODMAN";
const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// The rootless Podman container.
fn podman_name() -> String {
    setting("NANODOCK_IT_PODMAN_NAME", "pweb")
}

/// The Docker container that runs next to it, for the multi-daemon merge.
fn docker_name() -> String {
    setting("NANODOCK_IT_DOCKER_NAME", "nd-web")
}

#[test]
fn detect_merges_rootless_podman_and_docker() {
    if !enabled(GATE, "detect_merges_rootless_podman_and_docker") {
        return;
    }
    let podman = podman_name();
    let docker = docker_name();
    let podman_port = port("NANODOCK_IT_PODMAN_PORT", 18180);
    let docker_port = port("NANODOCK_IT_DOCKER_PORT", 18080);
    let client = client();
    let map = detect(&client);

    let found = |host_port| {
        map.lookup(LOCALHOST, host_port, Protocol::Tcp, ProxyFallback::Deny)
            .container()
            .map(|info| info.name.clone())
    };
    assert_eq!(
        found(podman_port),
        Some(podman.clone()),
        "rootless Podman publishes {podman_port}; detected {:?}",
        names(&map)
    );
    assert_eq!(
        found(docker_port),
        Some(docker),
        "Docker publishes {docker_port} in the same map; detected {:?}",
        names(&map)
    );

    let info = find_by_name(&map, &podman).expect("the Podman container is detected");
    assert!(
        info.image.contains("busybox"),
        "image is reported: {:?}",
        info.image
    );
    assert!(!info.id.is_empty(), "the Podman container id is reported");

    let background = client
        .start_detection()
        .wait_result()
        .unwrap_or_else(|error| panic!("background detection failed: {error} ({error:?})"));
    assert_eq!(
        map, background,
        "both detection paths merge the same daemons"
    );
}

/// Map one `rootlessport` pid to its container.
///
/// The only place that touches the resolver API, so a signature change is a
/// one-line edit. The resolver reads the home directory from the environment.
#[cfg(target_os = "linux")]
fn resolve_rootlessport(pid: u32) -> Option<nanodock::ContainerInfo> {
    nanodock::RootlessPodmanResolver::new().lookup(pid, "rootlessport")
}

/// `NANODOCK_IT_ROOTLESSPORT_PIDS` lists the `rootlessport` pids CI found
/// with `pgrep -x rootlessport`. At least one must resolve to the Podman
/// container (the parent holds the network namespace open), and none may
/// resolve to another container.
#[cfg(target_os = "linux")]
#[test]
fn rootless_resolver_maps_rootlessport_pid() {
    if !enabled(GATE, "rootless_resolver_maps_rootlessport_pid") {
        return;
    }
    let raw = setting("NANODOCK_IT_ROOTLESSPORT_PIDS", "");
    let pids: Vec<u32> = raw
        .split_whitespace()
        .map(|pid| {
            pid.parse()
                .unwrap_or_else(|error| panic!("bad pid {pid:?} in {raw:?}: {error}"))
        })
        .collect();
    assert!(
        !pids.is_empty(),
        "NANODOCK_IT_ROOTLESSPORT_PIDS is empty: no rootlessport process was found"
    );

    let podman = podman_name();
    let resolved: Vec<(u32, Option<String>)> = pids
        .iter()
        .map(|&pid| (pid, resolve_rootlessport(pid).map(|info| info.name)))
        .collect();
    assert!(
        resolved
            .iter()
            .any(|(_, name)| name.as_deref() == Some(podman.as_str())),
        "some rootlessport pid resolves to {podman}: {resolved:?}"
    );
    assert!(
        resolved
            .iter()
            .all(|(_, name)| name.as_deref().is_none_or(|name| name == podman)),
        "no rootlessport pid resolves to another container: {resolved:?}"
    );
}

/// Docker is the first default socket and answers 404 for a Podman
/// container; the stop must move on to the Podman socket.
#[test]
fn stop_falls_through_docker_to_podman() {
    if !enabled(GATE, "stop_falls_through_docker_to_podman") {
        return;
    }
    let podman = podman_name();
    let podman_port = port("NANODOCK_IT_PODMAN_PORT", 18180);
    let client = client();
    assert!(
        find_by_name(&detect(&client), &podman).is_some(),
        "{podman} must be running before the test"
    );

    assert_eq!(client.stop(&podman), StopOutcome::Stopped, "first stop");
    assert_eq!(
        client.stop(&podman),
        StopOutcome::AlreadyStopped,
        "second stop"
    );

    let map = detect(&client);
    assert!(
        map.lookup(LOCALHOST, podman_port, Protocol::Tcp, ProxyFallback::Deny)
            .container()
            .is_none(),
        "port {podman_port} is released"
    );
    assert!(
        find_by_name(&map, &docker_name()).is_some(),
        "the Docker container is untouched"
    );
}
