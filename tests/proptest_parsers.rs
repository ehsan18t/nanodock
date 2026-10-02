//! Property tests for the public parsers and the port map.
//!
//! The daemon JSON parsers face whatever a socket hands them, so they must
//! never panic, and the lenient parser must agree with the strict one whenever
//! the strict one accepts the input. The port map is checked against a plain
//! `HashMap` model. Case counts are kept moderate so the file runs in a few
//! seconds.

#![allow(missing_docs, reason = "integration tests document behavior via names")]

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use nanodock::{
    ContainerInfo, ContainerPortMap, Protocol, ProxyFallback, PublishedContainerMatch,
    is_container_proxy_process, parse_containers_json, parse_containers_json_strict,
    short_container_id,
};
use proptest::prelude::*;
use serde_json::{Value, json};

// ── Daemon JSON model ───────────────────────────────────────────────

/// Host IPs a daemon reports, paired with the map key they must produce.
const HOST_IPS: [(Option<&str>, Option<IpAddr>); 6] = [
    (None, None),
    (Some(""), None),
    (Some("0.0.0.0"), None),
    (Some("::"), None),
    (Some("127.0.0.1"), Some(IpAddr::V4(Ipv4Addr::LOCALHOST))),
    (Some("::1"), Some(IpAddr::V6(Ipv6Addr::LOCALHOST))),
];

/// Protocols a daemon reports, paired with the parsed protocol (`None`:
/// the binding is skipped).
const PROTOCOLS: [(Option<&str>, Option<Protocol>); 5] = [
    (None, Some(Protocol::Tcp)),
    (Some("tcp"), Some(Protocol::Tcp)),
    (Some("UDP"), Some(Protocol::Udp)),
    (Some("udp"), Some(Protocol::Udp)),
    (Some("sctp"), None),
];

#[derive(Debug, Clone)]
struct GenPort {
    host_ip: usize,
    public_port: Option<u16>,
    protocol: usize,
}

#[derive(Debug, Clone)]
struct GenContainer {
    id: String,
    name: String,
    image: String,
    project: Option<String>,
    service: Option<String>,
    ports: Vec<GenPort>,
}

impl GenContainer {
    fn to_json(&self) -> Value {
        let mut labels = serde_json::Map::new();
        if let Some(project) = &self.project {
            labels.insert("com.docker.compose.project".into(), json!(project));
        }
        if let Some(service) = &self.service {
            labels.insert("com.docker.compose.service".into(), json!(service));
        }
        let ports: Vec<Value> = self
            .ports
            .iter()
            .map(|port| {
                let mut object = serde_json::Map::new();
                object.insert("PrivatePort".into(), json!(80));
                if let Some(ip) = HOST_IPS[port.host_ip].0 {
                    object.insert("IP".into(), json!(ip));
                }
                if let Some(public) = port.public_port {
                    object.insert("PublicPort".into(), json!(public));
                }
                if let Some(protocol) = PROTOCOLS[port.protocol].0 {
                    object.insert("Type".into(), json!(protocol));
                }
                Value::Object(object)
            })
            .collect();
        json!({
            "Id": self.id,
            "Names": [format!("/{}", self.name)],
            "Image": self.image,
            "Labels": labels,
            "Ports": ports,
        })
    }

    fn info(&self) -> ContainerInfo {
        let mut info = ContainerInfo::new(&self.id, &self.name, &self.image);
        if let Some(project) = &self.project {
            info = info.with_compose_project(project);
        }
        if let Some(service) = &self.service {
            info = info.with_compose_service(service);
        }
        info
    }
}

fn gen_port() -> impl Strategy<Value = GenPort> {
    (
        0..HOST_IPS.len(),
        proptest::option::weighted(0.9, 1..=6_u16),
        0..PROTOCOLS.len(),
    )
        .prop_map(|(host_ip, public_port, protocol)| GenPort {
            host_ip,
            public_port,
            protocol,
        })
}

fn gen_container() -> impl Strategy<Value = GenContainer> {
    (
        "[0-9a-f]{64}",
        "[a-z][a-z0-9-]{0,11}",
        "[a-z]{1,8}(:[0-9]{1,2})?",
        proptest::option::of("[a-z]{1,6}"),
        proptest::option::of("[a-z]{1,6}"),
        proptest::collection::vec(gen_port(), 0..5),
    )
        .prop_map(|(id, name, image, project, service, ports)| GenContainer {
            id,
            name,
            image,
            project,
            service,
            ports,
        })
}

fn gen_containers() -> impl Strategy<Value = Vec<GenContainer>> {
    proptest::collection::vec(gen_container(), 0..5)
}

/// The map the parsers must produce: later containers replace earlier ones
/// on the same binding, as the daemon's list order dictates.
fn expected_map(containers: &[GenContainer]) -> ContainerPortMap {
    let mut map = ContainerPortMap::new();
    for container in containers {
        let info = std::sync::Arc::new(container.info());
        for port in &container.ports {
            let (Some(public), Some(protocol)) = (port.public_port, PROTOCOLS[port.protocol].1)
            else {
                continue;
            };
            map.insert(
                HOST_IPS[port.host_ip].1,
                public,
                protocol,
                std::sync::Arc::clone(&info),
            );
        }
    }
    map
}

fn to_body(containers: &[GenContainer]) -> String {
    Value::Array(containers.iter().map(GenContainer::to_json).collect()).to_string()
}

/// One edit to a JSON body, at char positions so the result stays UTF-8.
#[derive(Debug, Clone)]
enum Mutation {
    Truncate(usize),
    Delete(usize, usize),
    Insert(usize, char),
    Replace(usize, char),
    Duplicate(usize, usize),
}

fn gen_mutation() -> impl Strategy<Value = Mutation> {
    let json_char = prop::sample::select(vec![
        '{', '}', '[', ']', '"', ':', ',', '\\', '0', '9', '-', 'e', 'n', ' ', '\u{e9}',
    ]);
    prop_oneof![
        any::<usize>().prop_map(Mutation::Truncate),
        (any::<usize>(), 1..8_usize).prop_map(|(at, len)| Mutation::Delete(at, len)),
        (any::<usize>(), json_char.clone()).prop_map(|(at, ch)| Mutation::Insert(at, ch)),
        (any::<usize>(), json_char).prop_map(|(at, ch)| Mutation::Replace(at, ch)),
        (any::<usize>(), 1..16_usize).prop_map(|(at, len)| Mutation::Duplicate(at, len)),
    ]
}

fn apply(body: &str, mutation: &Mutation) -> String {
    let mut chars: Vec<char> = body.chars().collect();
    let count = chars.len();
    let pos = |at: usize| if count == 0 { 0 } else { at % (count + 1) };
    match *mutation {
        Mutation::Truncate(at) => chars.truncate(pos(at)),
        Mutation::Delete(at, len) => {
            let start = pos(at);
            chars.drain(start..(start + len).min(count));
        }
        Mutation::Insert(at, ch) => chars.insert(pos(at), ch),
        Mutation::Replace(at, ch) => {
            if count > 0 {
                chars[at % count] = ch;
            }
        }
        Mutation::Duplicate(at, len) => {
            let start = pos(at);
            let slice: Vec<char> = chars[start..(start + len).min(count)].to_vec();
            chars.splice(start..start, slice);
        }
    }
    chars.into_iter().collect()
}

/// Strict success implies the lenient parser returns the same map.
fn assert_parsers_agree(body: &str) -> Result<(), TestCaseError> {
    let lenient = parse_containers_json(body);
    if let Ok(strict) = parse_containers_json_strict(body) {
        prop_assert_eq!(lenient, strict, "body: {}", body);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn parsers_never_panic_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        assert_parsers_agree(&String::from_utf8_lossy(&bytes))?;
    }

    #[test]
    fn parsers_never_panic_on_json_alphabet(body in r#"[\[\]{}":,0-9a-zA-Z .\-\\]{0,96}"#) {
        assert_parsers_agree(&body)?;
    }

    #[test]
    fn parsers_match_the_model_on_valid_lists(containers in gen_containers()) {
        let body = to_body(&containers);
        let expected = expected_map(&containers);
        let strict = parse_containers_json_strict(&body);
        prop_assert!(strict.is_ok(), "strict rejected a valid list: {:?}", strict);
        prop_assert_eq!(strict.ok(), Some(expected.clone()));
        prop_assert_eq!(parse_containers_json(&body), expected);
    }

    #[test]
    fn parsers_agree_on_mutated_lists(
        containers in gen_containers(),
        mutations in proptest::collection::vec(gen_mutation(), 1..4),
    ) {
        let mut body = to_body(&containers);
        for mutation in &mutations {
            body = apply(&body, mutation);
        }
        assert_parsers_agree(&body)?;
    }

    #[test]
    fn lenient_parser_skips_only_the_malformed_container(
        containers in gen_containers(),
        at in any::<usize>(),
        bad in prop::sample::select(vec![
            json!(42),
            json!("container"),
            json!(null),
            json!({"Ports": "not a list"}),
            json!({"Names": [1, 2]}),
            json!({"Ports": [{"PublicPort": 70000}]}),
        ]),
    ) {
        let mut elements: Vec<Value> = containers.iter().map(GenContainer::to_json).collect();
        elements.insert(at % (elements.len() + 1), bad);
        let body = Value::Array(elements).to_string();
        prop_assert!(parse_containers_json_strict(&body).is_err(), "strict must reject {}", body);
        prop_assert_eq!(parse_containers_json(&body), expected_map(&containers));
    }
}

// ── Process names and container ids ─────────────────────────────────

/// The documented proxy process names (see `is_container_proxy_process`).
const KNOWN_PROXIES: [&str; 13] = [
    "docker-proxy",
    "rootlesskit",
    "rootlessport",
    "rootlessport-child",
    "slirp4netns",
    "pasta",
    "pasta.avx2",
    "com.docker.backend",
    "com.docker.vpnkit",
    "vpnkit",
    "wslrelay",
    "gvproxy",
    "limactl",
];

/// What the documentation promises for `name` (without an `.exe` suffix):
/// a known name, or a 15 or 16 byte kernel truncation of a longer one.
fn documented_proxy(name: &str) -> bool {
    KNOWN_PROXIES.iter().any(|known| {
        known.eq_ignore_ascii_case(name)
            || ([15, 16].contains(&name.len())
                && known.len() > name.len()
                && known.as_bytes()[..name.len()].eq_ignore_ascii_case(name.as_bytes()))
    })
}

/// Flip the ASCII case of the characters selected by `mask`.
fn mix_case(name: &str, mask: u64) -> String {
    name.chars()
        .enumerate()
        .map(|(index, ch)| {
            if (mask >> (index % 64)) & 1 == 1 {
                ch.to_ascii_uppercase()
            } else {
                ch
            }
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn proxy_check_ignores_ascii_case(name in any::<String>()) {
        let verdict = is_container_proxy_process(&name);
        prop_assert_eq!(is_container_proxy_process(&name.to_ascii_uppercase()), verdict);
        prop_assert_eq!(is_container_proxy_process(&name.to_ascii_lowercase()), verdict);
    }

    #[test]
    fn known_proxies_match_in_any_case_with_exe(
        known in prop::sample::select(KNOWN_PROXIES.to_vec()),
        mask in any::<u64>(),
        exe in prop::sample::select(vec!["", ".exe", ".EXE", ".Exe"]),
    ) {
        let name = format!("{}{exe}", mix_case(known, mask));
        prop_assert!(is_container_proxy_process(&name), "{} is a known proxy", name);
    }

    #[test]
    fn proxy_prefixes_match_only_as_documented(
        known in prop::sample::select(KNOWN_PROXIES.to_vec()),
        cut in any::<usize>(),
        mask in any::<u64>(),
    ) {
        let prefix = mix_case(&known[..cut % known.len()], mask);
        prop_assert_eq!(
            is_container_proxy_process(&prefix),
            documented_proxy(&prefix),
            "prefix {:?} of {}", prefix, known
        );
    }

    #[test]
    fn proxy_names_with_junk_suffix_match_only_as_documented(
        known in prop::sample::select(KNOWN_PROXIES.to_vec()),
        junk in "[a-z0-9_-]{1,8}",
    ) {
        let name = format!("{known}{junk}");
        prop_assert_eq!(
            is_container_proxy_process(&name),
            documented_proxy(&name),
            "{:?}", name
        );
    }

    #[test]
    fn short_id_is_a_bounded_prefix(id in any::<String>()) {
        let short = short_container_id(&id);
        prop_assert!(id.starts_with(short), "{:?} is a prefix of {:?}", short, id);
        prop_assert!(short.len() == 12 || short == id, "{:?} from {:?}", short, id);
        if id.len() <= 12 {
            prop_assert_eq!(&short, &id);
        }
    }

    #[test]
    fn short_id_of_hex_keeps_twelve_chars_in_any_case(id in "[0-9a-fA-F]{0,80}") {
        let short = short_container_id(&id);
        prop_assert_eq!(short.len(), id.len().min(12));
        let upper = id.to_ascii_uppercase();
        prop_assert_eq!(short_container_id(&upper), short.to_ascii_uppercase());
    }
}

// ── ContainerPortMap against a HashMap model ────────────────────────

type Key = (Option<IpAddr>, u16, Protocol);

/// Host IPs bindings use; `None` is the wildcard.
const BIND_IPS: [Option<IpAddr>; 4] = [
    None,
    Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    Some(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
];

/// Socket addresses looked up; the last one is never bound.
const SOCKET_IPS: [IpAddr; 4] = [
    IpAddr::V4(Ipv4Addr::LOCALHOST),
    IpAddr::V6(Ipv6Addr::LOCALHOST),
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
];

const PROTOS: [Protocol; 2] = [Protocol::Tcp, Protocol::Udp];

/// A container determined by its name, so equal names mean equal containers.
fn container(name: u8) -> ContainerInfo {
    ContainerInfo::new(format!("id{name}"), format!("c{name}"), "image")
}

fn gen_insert() -> impl Strategy<Value = (Key, u8)> {
    (0..BIND_IPS.len(), 1..=4_u16, 0..PROTOS.len(), 0..3_u8)
        .prop_map(|(ip, port, proto, name)| ((BIND_IPS[ip], port, PROTOS[proto]), name))
}

/// The documented lookup: exact address, then wildcard, then (with the
/// proxy fallback) a unique container on the port and protocol.
fn model_lookup(
    model: &HashMap<Key, u8>,
    ip: IpAddr,
    port: u16,
    proto: Protocol,
    fallback: ProxyFallback,
) -> Result<Option<u8>, &'static str> {
    if let Some(&name) = model.get(&(Some(ip), port, proto)) {
        return Ok(Some(name));
    }
    if let Some(&name) = model.get(&(None, port, proto)) {
        return Ok(Some(name));
    }
    if fallback == ProxyFallback::Deny {
        return Ok(None);
    }
    let candidates: BTreeSet<u8> = model
        .iter()
        .filter(|((_, candidate_port, candidate_proto), _)| {
            *candidate_port == port && *candidate_proto == proto
        })
        .map(|(_, &name)| name)
        .collect();
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.first().copied()),
        _ => Err("ambiguous"),
    }
}

fn found_name(found: PublishedContainerMatch<'_>) -> Result<Option<String>, &'static str> {
    match found {
        PublishedContainerMatch::Match(info) => Ok(Some(info.name.clone())),
        PublishedContainerMatch::NotFound => Ok(None),
        PublishedContainerMatch::Ambiguous => Err("ambiguous"),
        _ => Err("unknown variant"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn port_map_matches_a_hashmap_model(inserts in proptest::collection::vec(gen_insert(), 0..24)) {
        let mut map = ContainerPortMap::new();
        let mut model: HashMap<Key, u8> = HashMap::new();
        for &((ip, port, proto), name) in &inserts {
            let replaced = map.insert(ip, port, proto, container(name));
            prop_assert_eq!(
                replaced.map(|info| info.name.clone()),
                model.insert((ip, port, proto), name).map(|old| container(old).name)
            );
        }

        prop_assert_eq!(map.len(), model.len());
        prop_assert_eq!(map.is_empty(), model.is_empty());
        for (&(ip, port, proto), &name) in &model {
            prop_assert_eq!(map.get(ip, port, proto), Some(&container(name)));
        }
        let iterated: BTreeSet<(Key, String)> =
            map.iter().map(|(key, info)| (key, info.name.clone())).collect();
        let modeled: BTreeSet<(Key, String)> =
            model.iter().map(|(&key, &name)| (key, container(name).name)).collect();
        prop_assert_eq!(iterated, modeled);
        prop_assert_eq!(map.iter().len(), model.len());

        let collected: ContainerPortMap = inserts
            .iter()
            .map(|&(key, name)| (key, container(name)))
            .collect();
        prop_assert_eq!(&collected, &map, "FromIterator agrees with insert");

        for ip in SOCKET_IPS {
            for port in 0..=5 {
                for proto in PROTOS {
                    for fallback in [ProxyFallback::Deny, ProxyFallback::Allow] {
                        let expected = model_lookup(&model, ip, port, proto, fallback)
                            .map(|name| name.map(|name| container(name).name));
                        let actual = found_name(map.lookup(ip, port, proto, fallback));
                        prop_assert_eq!(
                            actual, expected,
                            "lookup({}, {}, {}, {:?})", ip, port, proto, fallback
                        );
                    }
                }
            }
        }
    }
}
