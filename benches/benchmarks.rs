#![allow(
    missing_docs,
    reason = "gungraun macros generate internal harness items"
)]

//! Benchmarks for `nanodock`.
//!
//! Measures the two hot paths most likely to matter to callers:
//! parsing `/containers/json` payloads and matching host sockets back
//! to published container bindings.

use std::fmt::Write as _;
use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use gungraun::prelude::*;
use nanodock::{
    ContainerInfo, ContainerPortMap, Protocol, PublishedContainerMatch, lookup_published_container,
    parse_containers_json,
};

const LOOKUP_BENCH_ENTRY_COUNT: u16 = 500;
const LARGE_LOOKUP_ENTRY_COUNT: u16 = 4096;
const PORTS_PER_CONTAINER: u16 = 4;

type LookupFixture = (ContainerPortMap, SocketAddr);

#[must_use]
fn container_info(index: u16) -> ContainerInfo {
    ContainerInfo {
        id: format!("{index:012x}"),
        name: format!("container-{index}"),
        image: format!("image:{index}"),
    }
}

fn insert_mapping(
    map: &mut ContainerPortMap,
    host_ip: Option<IpAddr>,
    port: u16,
    proto: Protocol,
    index: u16,
) {
    map.insert((host_ip, port, proto), container_info(index));
}

#[must_use]
fn exact_lookup_fixture(size: u16) -> (ContainerPortMap, SocketAddr) {
    let mut map = ContainerPortMap::with_capacity(usize::from(size));
    let base_port = 20_000_u16;

    for index in 0..size {
        insert_mapping(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            base_port + index,
            Protocol::Tcp,
            index,
        );
    }

    let target_port = base_port + (size / 2);
    (
        map,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), target_port),
    )
}

#[must_use]
fn wildcard_lookup_fixture(size: u16) -> LookupFixture {
    let mut map = ContainerPortMap::with_capacity(usize::from(size));
    let base_port = 26_000_u16;

    for index in 0..size {
        insert_mapping(&mut map, None, base_port + index, Protocol::Tcp, index);
    }

    let target_port = base_port + (size / 2);
    (
        map,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), target_port),
    )
}

#[must_use]
fn proxy_unique_fixture(size: u16) -> LookupFixture {
    let mut map = ContainerPortMap::with_capacity(usize::from(size));
    let base_port = 32_000_u16;

    for index in 0..size.saturating_sub(1) {
        insert_mapping(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            base_port + index,
            Protocol::Tcp,
            index,
        );
    }

    let target_port = base_port + size;
    insert_mapping(
        &mut map,
        Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
        target_port,
        Protocol::Tcp,
        size,
    );

    (
        map,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), target_port),
    )
}

#[must_use]
fn proxy_ambiguous_fixture(size: u16) -> LookupFixture {
    let mut map = ContainerPortMap::with_capacity(usize::from(size));
    let base_port = 38_000_u16;

    for index in 0..size.saturating_sub(2) {
        insert_mapping(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            base_port + index,
            Protocol::Tcp,
            index,
        );
    }

    let target_port = base_port + size;
    insert_mapping(
        &mut map,
        Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
        target_port,
        Protocol::Tcp,
        size,
    );
    insert_mapping(
        &mut map,
        Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))),
        target_port,
        Protocol::Tcp,
        size + 1,
    );

    (
        map,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), target_port),
    )
}

#[must_use]
const fn match_score(result: PublishedContainerMatch<'_>) -> usize {
    match result {
        PublishedContainerMatch::Match(info) => info.name.len(),
        PublishedContainerMatch::Ambiguous => usize::MAX,
        _ => 0,
    }
}

#[must_use]
fn daemon_response(container_count: u16) -> String {
    let estimated_ports = usize::from(container_count) * usize::from(PORTS_PER_CONTAINER);
    let mut json = String::with_capacity(estimated_ports * 96);
    json.push('[');

    for container_index in 0..container_count {
        if container_index != 0 {
            json.push(',');
        }

        let _ = write!(
            json,
            "{{\"Id\":\"{container_index:012x}\",\"Names\":[\"/service-{container_index}\"],\"Image\":\"image:{container_index}\",\"Ports\":["
        );

        for port_index in 0..PORTS_PER_CONTAINER {
            if port_index != 0 {
                json.push(',');
            }

            let host_ip = match port_index % 4 {
                0 => "",
                1 => "0.0.0.0",
                2 => "127.0.0.1",
                _ => "::",
            };
            let proto = if port_index.is_multiple_of(2) {
                "tcp"
            } else {
                "udp"
            };
            let published_port = 45_000 + (container_index * PORTS_PER_CONTAINER) + port_index;
            let private_port = 8_000 + port_index;

            let _ = write!(
                json,
                "{{\"IP\":\"{host_ip}\",\"PrivatePort\":{private_port},\"PublicPort\":{published_port},\"Type\":\"{proto}\"}}"
            );
        }

        json.push_str("]}");
    }

    json.push(']');
    json
}

#[library_benchmark]
#[bench::entries_128(args = (128), setup = exact_lookup_fixture)]
#[bench::entries_500(args = (LOOKUP_BENCH_ENTRY_COUNT), setup = exact_lookup_fixture)]
#[bench::entries_4096(args = (LARGE_LOOKUP_ENTRY_COUNT), setup = exact_lookup_fixture)]
fn bench_lookup_exact((map, socket): LookupFixture) -> usize {
    black_box(match_score(lookup_published_container(
        black_box(&map),
        black_box(socket),
        Protocol::Tcp,
        false,
    )))
}

#[library_benchmark]
#[bench::entries_128(args = (128), setup = wildcard_lookup_fixture)]
#[bench::entries_500(args = (LOOKUP_BENCH_ENTRY_COUNT), setup = wildcard_lookup_fixture)]
#[bench::entries_4096(args = (LARGE_LOOKUP_ENTRY_COUNT), setup = wildcard_lookup_fixture)]
fn bench_lookup_wildcard((map, socket): LookupFixture) -> usize {
    black_box(match_score(lookup_published_container(
        black_box(&map),
        black_box(socket),
        Protocol::Tcp,
        false,
    )))
}

#[library_benchmark]
#[bench::entries_128(args = (128), setup = proxy_unique_fixture)]
#[bench::entries_500(args = (LOOKUP_BENCH_ENTRY_COUNT), setup = proxy_unique_fixture)]
#[bench::entries_4096(args = (LARGE_LOOKUP_ENTRY_COUNT), setup = proxy_unique_fixture)]
fn bench_lookup_proxy_unique((map, socket): LookupFixture) -> usize {
    black_box(match_score(lookup_published_container(
        black_box(&map),
        black_box(socket),
        Protocol::Tcp,
        true,
    )))
}

#[library_benchmark]
#[bench::entries_128(args = (128), setup = proxy_ambiguous_fixture)]
#[bench::entries_500(args = (LOOKUP_BENCH_ENTRY_COUNT), setup = proxy_ambiguous_fixture)]
#[bench::entries_4096(args = (LARGE_LOOKUP_ENTRY_COUNT), setup = proxy_ambiguous_fixture)]
fn bench_lookup_proxy_ambiguous((map, socket): LookupFixture) -> usize {
    black_box(match_score(lookup_published_container(
        black_box(&map),
        black_box(socket),
        Protocol::Tcp,
        true,
    )))
}

#[library_benchmark]
#[bench::containers_4(args = (4), setup = daemon_response)]
#[bench::containers_16(args = (16), setup = daemon_response)]
#[bench::containers_64(args = (64), setup = daemon_response)]
#[bench::containers_128(args = (128), setup = daemon_response)]
fn bench_parse_containers_json(response: String) -> usize {
    let response = black_box(response);
    black_box(parse_containers_json(response.as_str()).len())
}

library_benchmark_group!(
    name = lookup_group,
    benchmarks = [
        bench_lookup_exact,
        bench_lookup_wildcard,
        bench_lookup_proxy_unique,
        bench_lookup_proxy_ambiguous,
    ]
);

library_benchmark_group!(name = parse_group, benchmarks = bench_parse_containers_json);

main!(library_benchmark_groups = [lookup_group, parse_group]);
