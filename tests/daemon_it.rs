//! Integration tests against a live Docker daemon.
//!
//! Skipped unless `NANODOCK_IT=1`. The `docker-it` CI job starts the
//! containers these tests inspect and then runs
//! `cargo test --test daemon_it -- --test-threads=1`. Names and ports come
//! from the `NANODOCK_IT_*` variables below, with the same defaults the
//! workflow uses, so the two cannot drift. Each lifecycle test owns its own
//! container, so the order the tests run in does not matter.
//!
//! The TCP tests (names starting with `tcp_`) are gated separately on
//! `NANODOCK_IT_TCP_HOST`, a `tcp://host:port` forwarder to the same daemon.
//! CI runs them with the local socket made unreadable, which proves the
//! requests travel over TCP.

#![allow(missing_docs, reason = "integration tests document behavior via names")]

mod it_support;

use std::net::{IpAddr, Ipv4Addr};

use it_support::{bindings_of, client, detect, enabled, find_by_name, names, port, setting};
use nanodock::{Client, Protocol, ProxyFallback, PublishedContainerMatch, StopOutcome};

const GATE: &str = "NANODOCK_IT";
const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// An address no test container publishes on.
const OTHER_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7));
/// A name no daemon will ever have a container for.
const MISSING: &str = "nanodock-it-no-such-container";

/// The container publishing the wildcard, loopback, range, and UDP ports.
fn web_name() -> String {
    setting("NANODOCK_IT_WEB_NAME", "nd-web")
}

/// The inclusive host port range `NANODOCK_IT_RANGE` (`start-end`).
fn port_range() -> (u16, u16) {
    let raw = setting("NANODOCK_IT_RANGE", "18090-18092");
    let parsed = raw
        .split_once('-')
        .and_then(|(start, end)| Some((start.parse().ok()?, end.parse().ok()?)));
    match parsed {
        Some((start, end)) if start <= end => (start, end),
        _ => panic!("NANODOCK_IT_RANGE={raw:?} is not a `start-end` port range"),
    }
}

#[test]
fn wildcard_port_matches_any_local_address() {
    if !enabled(GATE, "wildcard_port_matches_any_local_address") {
        return;
    }
    let name = web_name();
    let wildcard = port("NANODOCK_IT_WILDCARD_PORT", 18080);
    let map = detect(&client());

    let info = map.get(None, wildcard, Protocol::Tcp).unwrap_or_else(|| {
        panic!(
            "no wildcard binding on {wildcard}/tcp; found {:?}",
            names(&map)
        )
    });
    assert_eq!(
        info.name, name,
        "wildcard port belongs to the web container"
    );
    assert!(
        info.image.contains("busybox"),
        "image is reported: {:?}",
        info.image
    );
    assert!(
        info.id.len() == 64 && info.id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "full container id is reported: {:?}",
        info.id
    );

    for ip in [LOCALHOST, OTHER_IP] {
        let found = map.lookup(ip, wildcard, Protocol::Tcp, ProxyFallback::Deny);
        assert_eq!(
            found.container().map(|info| info.name.as_str()),
            Some(name.as_str()),
            "a socket on {ip} matches the wildcard binding"
        );
    }
}

#[test]
fn loopback_port_is_bound_to_its_address() {
    if !enabled(GATE, "loopback_port_is_bound_to_its_address") {
        return;
    }
    let name = web_name();
    let loopback = port("NANODOCK_IT_LOOPBACK_PORT", 18081);
    let map = detect(&client());

    assert_eq!(
        map.get(Some(LOCALHOST), loopback, Protocol::Tcp)
            .map(|info| info.name.as_str()),
        Some(name.as_str()),
        "the 127.0.0.1 binding is keyed by its address"
    );
    assert!(
        map.get(None, loopback, Protocol::Tcp).is_none(),
        "a loopback binding is not a wildcard binding"
    );
    assert_eq!(
        map.lookup(LOCALHOST, loopback, Protocol::Tcp, ProxyFallback::Deny)
            .container()
            .map(|info| info.name.as_str()),
        Some(name.as_str()),
        "a socket on 127.0.0.1 matches exactly"
    );
    assert_eq!(
        map.lookup(OTHER_IP, loopback, Protocol::Tcp, ProxyFallback::Deny),
        PublishedContainerMatch::NotFound,
        "a socket on another address must not match without the proxy fallback"
    );
    assert_eq!(
        map.lookup(OTHER_IP, loopback, Protocol::Tcp, ProxyFallback::Allow)
            .container()
            .map(|info| info.name.as_str()),
        Some(name.as_str()),
        "the proxy fallback accepts the unique container on that port"
    );
}

#[test]
fn port_range_publishes_every_port() {
    if !enabled(GATE, "port_range_publishes_every_port") {
        return;
    }
    let name = web_name();
    let (start, end) = port_range();
    let map = detect(&client());

    for host_port in start..=end {
        assert_eq!(
            map.get(None, host_port, Protocol::Tcp)
                .map(|info| info.name.as_str()),
            Some(name.as_str()),
            "port {host_port} of the published range {start}-{end}"
        );
    }
    let bindings = bindings_of(&map, &name);
    let range_ports = bindings
        .iter()
        .filter(|(ip, host_port, proto)| {
            ip.is_none() && *proto == Protocol::Tcp && (start..=end).contains(host_port)
        })
        .count();
    assert_eq!(
        range_ports,
        usize::from(end - start) + 1,
        "one binding per range port: {bindings:?}"
    );
}

#[test]
fn udp_port_is_distinct_from_tcp() {
    if !enabled(GATE, "udp_port_is_distinct_from_tcp") {
        return;
    }
    let name = web_name();
    let udp = port("NANODOCK_IT_UDP_PORT", 18093);
    let map = detect(&client());

    assert_eq!(
        map.get(None, udp, Protocol::Udp)
            .map(|info| info.name.as_str()),
        Some(name.as_str()),
        "the UDP binding is detected"
    );
    assert!(
        map.get(None, udp, Protocol::Tcp).is_none(),
        "a UDP binding must not answer a TCP lookup"
    );
    assert_eq!(
        map.lookup(LOCALHOST, udp, Protocol::Tcp, ProxyFallback::Allow),
        PublishedContainerMatch::NotFound,
        "even the proxy fallback keeps the protocols apart"
    );
}

#[test]
fn compose_labels_fill_project_and_service() {
    if !enabled(GATE, "compose_labels_fill_project_and_service") {
        return;
    }
    let project = setting("NANODOCK_IT_COMPOSE_PROJECT", "itproj");
    let service = setting("NANODOCK_IT_COMPOSE_SERVICE", "web");
    let compose_port = port("NANODOCK_IT_COMPOSE_PORT", 18100);
    let map = detect(&client());

    let info = map
        .lookup(LOCALHOST, compose_port, Protocol::Tcp, ProxyFallback::Deny)
        .container()
        .unwrap_or_else(|| {
            panic!(
                "no container on {compose_port}/tcp; found {:?}",
                names(&map)
            )
        });
    assert_eq!(info.compose_project.as_deref(), Some(project.as_str()));
    assert_eq!(info.compose_service.as_deref(), Some(service.as_str()));
    assert_eq!(
        info.name,
        format!("{project}-{service}-1"),
        "Compose v2 container naming"
    );

    let web = find_by_name(&map, &web_name()).expect("the web container is detected");
    assert_eq!(
        (
            web.compose_project.as_deref(),
            web.compose_service.as_deref()
        ),
        (None, None),
        "a plain `docker run` container has no Compose labels"
    );
}

#[test]
fn strict_and_background_detection_agree() {
    if !enabled(GATE, "strict_and_background_detection_agree") {
        return;
    }
    let client = client();
    let strict = detect(&client);
    let background = client
        .start_detection()
        .wait_result()
        .unwrap_or_else(|error| panic!("background detection failed: {error} ({error:?})"));
    assert!(!strict.is_empty(), "the CI containers publish ports");
    assert_eq!(
        strict, background,
        "detect() and start_detection().wait_result() see the same daemon state"
    );
}

#[test]
fn stop_twice_then_kill_reports_already_stopped() {
    if !enabled(GATE, "stop_twice_then_kill_reports_already_stopped") {
        return;
    }
    let name = setting("NANODOCK_IT_STOP_NAME", "nd-stop");
    let stop_port = port("NANODOCK_IT_STOP_PORT", 18110);
    let client = client();
    assert!(
        find_by_name(&detect(&client), &name).is_some(),
        "{name} must be running before the test"
    );

    assert_eq!(client.stop(&name), StopOutcome::Stopped, "first stop");
    assert_eq!(
        client.stop(&name),
        StopOutcome::AlreadyStopped,
        "second stop"
    );
    assert_eq!(
        client.kill(&name),
        StopOutcome::AlreadyStopped,
        "kill on a stopped container"
    );

    let map = detect(&client);
    assert!(
        find_by_name(&map, &name).is_none(),
        "a stopped container publishes nothing"
    );
    assert!(
        map.get(None, stop_port, Protocol::Tcp).is_none(),
        "port {stop_port} is released"
    );
}

#[test]
fn kill_running_container_by_id_stops_it() {
    if !enabled(GATE, "kill_running_container_by_id_stops_it") {
        return;
    }
    let name = setting("NANODOCK_IT_KILL_NAME", "nd-kill");
    let client = client();
    let id = find_by_name(&detect(&client), &name)
        .unwrap_or_else(|| panic!("{name} must be running before the test"))
        .id
        .clone();

    assert_eq!(
        client.kill(&id),
        StopOutcome::Stopped,
        "kill by full id {id}"
    );
    // The daemon answers the kill once the signal is delivered; the container
    // exits a moment later, so wait for it to leave the listing.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while find_by_name(&detect(&client), &name).is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "a killed container publishes nothing within 5 s"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn unknown_container_is_not_found() {
    if !enabled(GATE, "unknown_container_is_not_found") {
        return;
    }
    let client = client();
    assert_eq!(client.stop(MISSING), StopOutcome::NotFound, "stop");
    assert_eq!(client.kill(MISSING), StopOutcome::NotFound, "kill");
}

/// A `unix://` `DOCKER_HOST` replaces the default sockets, so a missing
/// socket leaves no daemon at all. Needs no running daemon, so it is not
/// gated.
#[cfg(unix)]
#[test]
fn unix_docker_host_to_missing_socket_is_unreachable() {
    let missing = std::env::temp_dir().join("nanodock-it-missing/docker.sock");
    let client = client().docker_host(Some(format!("unix://{}", missing.display())));

    assert_eq!(client.stop(MISSING), StopOutcome::Unreachable, "stop");
    assert_eq!(client.kill(MISSING), StopOutcome::Unreachable, "kill");
    let error = client
        .detect()
        .expect_err("no daemon behind a missing socket");
    assert!(
        matches!(error, nanodock::Error::DaemonNotFound),
        "missing socket is reported as no daemon, got {error:?}"
    );
}

/// The TCP forwarder named by `NANODOCK_IT_TCP_HOST`, or `None` (and a
/// printed reason) when the TCP tests should skip.
fn tcp_client(test: &str) -> Option<Client> {
    let Some(host) = std::env::var("NANODOCK_IT_TCP_HOST")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        println!("skipping {test}: set NANODOCK_IT_TCP_HOST=tcp://host:port to run it");
        return None;
    };
    Some(client().docker_host(Some(host)))
}

#[test]
fn tcp_docker_host_detects_published_ports() {
    let Some(client) = tcp_client("tcp_docker_host_detects_published_ports") else {
        return;
    };
    let name = web_name();
    let wildcard = port("NANODOCK_IT_WILDCARD_PORT", 18080);
    let map = detect(&client);

    assert_eq!(
        map.lookup(LOCALHOST, wildcard, Protocol::Tcp, ProxyFallback::Deny)
            .container()
            .map(|info| info.name.as_str()),
        Some(name.as_str()),
        "detection over TCP finds the web container"
    );
    let background = client
        .start_detection()
        .wait_result()
        .unwrap_or_else(|error| panic!("background detection over TCP failed: {error}"));
    assert_eq!(map, background, "both detection paths agree over TCP");
}

#[test]
fn tcp_docker_host_stops_and_reports_not_found() {
    let Some(client) = tcp_client("tcp_docker_host_stops_and_reports_not_found") else {
        return;
    };
    assert_eq!(
        client.stop(MISSING),
        StopOutcome::NotFound,
        "the DOCKER_HOST daemon's 404 is the result"
    );

    let name = setting("NANODOCK_IT_TCP_KILL_NAME", "nd-tcp-kill");
    assert_eq!(client.kill(&name), StopOutcome::Stopped, "kill over TCP");
    assert_eq!(
        client.kill(&name),
        StopOutcome::AlreadyStopped,
        "second kill over TCP"
    );
}
