//! Docker/Podman API JSON response parsing.
//!
//! Deserialises the `GET /containers/json` payload and maps published
//! ports to [`ContainerInfo`] records.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::ops::Deref;
use std::sync::Arc;

use log::warn;
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::{ContainerInfo, ContainerPortMap, ParseError, Protocol};

/// A JSON string that borrows from the input when it contains no escape
/// sequences and falls back to an owned copy when it does.
///
/// Plain `&str` fields fail on any escaped string (for example `"a\"b"` or
/// `"/"`), and serde's stock `Cow<str>` impl always allocates when it
/// is wrapped in `Option` or `Vec`, even with `#[serde(borrow)]`. This
/// newtype keeps the zero-copy fast path while accepting every valid JSON
/// string.
struct JsonStr<'a>(Cow<'a, str>);

impl Deref for JsonStr<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for JsonStr<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(JsonStrVisitor).map(JsonStr)
    }
}

struct JsonStrVisitor;

impl<'de> Visitor<'de> for JsonStrVisitor {
    type Value = Cow<'de, str>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string")
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Cow::Owned(value))
    }
}

/// Host address a port is published on, as reported by the daemon.
#[derive(Clone, Copy, Default)]
enum HostIp {
    /// Absent, empty, or unspecified (`0.0.0.0` / `::`): all interfaces.
    #[default]
    Any,
    /// A concrete host address.
    Addr(IpAddr),
    /// Present but not parseable as an IP address. The binding is skipped
    /// because recording it as a wildcard would misattribute traffic on
    /// every other address sharing the port.
    Unparseable,
}

/// One published port, in Docker's format (`IP`, `PublicPort`, `Type`) or
/// Podman's libpod format (`host_ip`, `host_port`, `range`, `protocol`).
#[derive(Deserialize)]
struct DockerPort<'a> {
    #[serde(
        rename = "IP",
        alias = "host_ip",
        default,
        deserialize_with = "deserialize_host_ip"
    )]
    host_ip: HostIp,
    /// Docker format: the one host port of this entry. Docker lists every
    /// port of a published range as its own entry.
    #[serde(rename = "PublicPort")]
    public_port: Option<u16>,
    /// Podman libpod format: the first host port of `range` ports.
    host_port: Option<u16>,
    #[serde(rename = "Type", alias = "protocol", borrow)]
    proto: Option<JsonStr<'a>>,
    /// Podman libpod format: how many consecutive ports, from `host_port`,
    /// the entry publishes. Ignored on a Docker-format entry, which never
    /// carries it.
    range: Option<u16>,
}

impl DockerPort<'_> {
    /// The first and last host port this entry publishes.
    fn host_ports(&self) -> Option<(u16, u16)> {
        if let Some(port) = self.public_port {
            return Some((port, port));
        }
        let first = self.host_port?;
        // Podman may report `range: 0` for a single-port binding; treat it
        // like 1 so the binding is not silently dropped. A range that runs
        // past port 65535 ends there.
        let count = self.range.unwrap_or(1).max(1);
        Some((first, first.saturating_add(count - 1)))
    }
}

/// Most bindings one daemon response may expand to: every port of both
/// protocols. Port ranges make the number of bindings independent of the
/// body size (a few kilobytes of `"range": 65535` entries would otherwise
/// expand to millions of map inserts), so expansion stops at this cap.
const MAX_PORT_BINDINGS: usize = 2 * 65_536;

#[derive(Deserialize)]
struct DockerContainer<'a> {
    #[serde(rename = "Id", borrow)]
    id: Option<JsonStr<'a>>,
    #[serde(rename = "Names", borrow)]
    names: Option<Vec<JsonStr<'a>>>,
    #[serde(rename = "Image", borrow)]
    image: Option<JsonStr<'a>>,
    #[serde(rename = "Labels", borrow)]
    labels: Option<ComposeLabels<'a>>,
    #[serde(rename = "Ports", borrow)]
    ports: Option<Vec<DockerPort<'a>>>,
}

/// Label Docker Compose (and `podman-compose`) sets to the project name.
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
/// Label Docker Compose (and `podman-compose`) sets to the service name.
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";
/// Project label `podman-compose` sets in addition to the Docker one.
const PODMAN_COMPOSE_PROJECT_LABEL: &str = "io.podman.compose.project";

/// The Compose labels of a container, picked out of its `Labels` object.
///
/// Only the labels nanodock reads are kept; every other label value is
/// skipped without being decoded or copied. A value that is not a string,
/// even for a label nanodock reads, is ignored (see [`LabelValue`]) rather
/// than failing the container.
#[derive(Default)]
struct ComposeLabels<'a> {
    project: Option<JsonStr<'a>>,
    service: Option<JsonStr<'a>>,
    podman_project: Option<JsonStr<'a>>,
}

impl ComposeLabels<'_> {
    /// The Compose project, preferring the Docker label over the Podman one.
    fn project(&self) -> Option<String> {
        trimmed_non_empty(self.project.as_deref())
            .or_else(|| trimmed_non_empty(self.podman_project.as_deref()))
            .map(ToOwned::to_owned)
    }

    fn service(&self) -> Option<String> {
        trimmed_non_empty(self.service.as_deref()).map(ToOwned::to_owned)
    }
}

/// `value` without surrounding whitespace, or `None` when nothing is left.
fn trimmed_non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

impl<'de: 'a, 'a> Deserialize<'de> for ComposeLabels<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ComposeLabelsVisitor)
    }
}

struct ComposeLabelsVisitor;

impl<'de> Visitor<'de> for ComposeLabelsVisitor {
    type Value = ComposeLabels<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a map of container labels")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut labels = ComposeLabels::default();
        while let Some(key) = map.next_key::<JsonStr<'de>>()? {
            let slot = match &*key {
                COMPOSE_PROJECT_LABEL => &mut labels.project,
                COMPOSE_SERVICE_LABEL => &mut labels.service,
                PODMAN_COMPOSE_PROJECT_LABEL => &mut labels.podman_project,
                _ => {
                    map.next_value::<de::IgnoredAny>()?;
                    continue;
                }
            };
            *slot = map.next_value::<LabelValue<'de>>()?.0;
        }
        Ok(labels)
    }
}

/// One label value: kept when it is a string, `None` for any other JSON
/// value.
///
/// Docker and Podman send string label values, but a number, boolean,
/// `null`, array, or object must not make the whole container fail to
/// parse: in lenient detection that would drop the container and every
/// port it publishes. The value is still consumed so parsing continues.
struct LabelValue<'a>(Option<JsonStr<'a>>);

impl<'de: 'a, 'a> Deserialize<'de> for LabelValue<'a> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer
            .deserialize_any(LabelValueVisitor)
            .map(|value| Self(value.map(JsonStr)))
    }
}

struct LabelValueVisitor;

impl<'de> Visitor<'de> for LabelValueVisitor {
    type Value = Option<Cow<'de, str>>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a label value")
    }

    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        Ok(Some(Cow::Borrowed(value)))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Some(Cow::Owned(value.to_owned())))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(Some(Cow::Owned(value)))
    }

    fn visit_bool<E: de::Error>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E: de::Error>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E: de::Error>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        de::IgnoredAny.visit_seq(seq).map(|_| None)
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        de::IgnoredAny.visit_map(map).map(|_| None)
    }
}

/// Parse the JSON response from `GET /containers/json` into a port map.
///
/// Each container may publish multiple ports. The map keys are
/// `(public_ip, public_port, protocol)` tuples.
///
/// Parsing is lenient per container: a malformed entry is skipped without
/// discarding the other containers in the array. Input that is not a
/// syntactically valid JSON array yields an empty map.
#[must_use]
pub fn parse_containers_json(json_body: &str) -> ContainerPortMap {
    // Fast path: the strict parser takes the whole array in one zero-copy
    // pass.
    if let Ok(map) = parse_containers_json_strict(json_body) {
        return map;
    }

    // Slow path, only taken for malformed input: decode the array into
    // untyped values, then convert each element independently so one bad
    // container cannot poison the rest.
    let mut map = ContainerPortMap::new();
    let Ok(elements) = serde_json::from_str::<Vec<serde_json::Value>>(json_body) else {
        return map;
    };
    let containers: Vec<DockerContainer<'_>> = elements
        .iter()
        .filter_map(|element| DockerContainer::deserialize(element).ok())
        .collect();
    populate_port_map(&mut map, &containers);
    map
}

/// Strict variant of [`parse_containers_json`] that reports invalid JSON
/// instead of skipping it.
///
/// # Errors
///
/// Fails with a [`ParseError`] when the body is not a JSON array of
/// well-formed container objects.
pub fn parse_containers_json_strict(json_body: &str) -> Result<ContainerPortMap, ParseError> {
    let containers =
        serde_json::from_str::<Vec<DockerContainer<'_>>>(json_body).map_err(ParseError::json)?;
    let mut map = ContainerPortMap::new();
    populate_port_map(&mut map, &containers);
    Ok(map)
}

/// One published port entry reduced to what the map stores: host IP, first
/// and last host port, and protocol.
type PortSpan = (Option<IpAddr>, u16, u16, Protocol);

fn port_span(port: &DockerPort<'_>) -> Option<PortSpan> {
    let (first, last) = port.host_ports()?;
    let proto = parse_port_protocol(port.proto.as_deref())?;
    let host_ip = match port.host_ip {
        HostIp::Any => None,
        HostIp::Addr(ip) => Some(ip),
        HostIp::Unparseable => return None,
    };
    Some((host_ip, first, last, proto))
}

fn container_info(container: &DockerContainer<'_>) -> ContainerInfo {
    let id = container.id.as_deref().unwrap_or("");
    let image = container.image.as_deref().unwrap_or("");
    let mut info = ContainerInfo::new(id, container_display_name(container), image);
    if let Some(labels) = &container.labels {
        info.compose_project = labels.project();
        info.compose_service = labels.service();
    }
    info
}

/// Insert the bindings of every container, at most [`MAX_PORT_BINDINGS`]
/// in total.
///
/// A port entry repeated within one container is expanded and charged once:
/// Docker lists every port once for `0.0.0.0` and once for `::`, and both
/// map to the same wildcard key. An entry that no longer fits under the cap
/// is skipped, and the entries and containers after it are still inserted
/// when they fit. Any skipped binding marks the map as truncated.
fn populate_port_map(map: &mut ContainerPortMap, containers: &[DockerContainer<'_>]) {
    // Size the map once instead of growing it while a range is expanded.
    let expected = containers
        .iter()
        .filter_map(|container| container.ports.as_deref())
        .flatten()
        .filter_map(port_span)
        .map(|(_, first, last, _)| usize::from(last - first) + 1)
        .fold(0, usize::saturating_add);
    map.reserve(expected.min(MAX_PORT_BINDINGS));

    let mut remaining = MAX_PORT_BINDINGS;
    let mut dropped = 0_usize;
    for container in containers {
        let Some(ports) = &container.ports else {
            continue;
        };
        // Shared by every binding of this container.
        let info = Arc::new(container_info(container));
        // Port entries already expanded for this container.
        let mut expanded = HashSet::new();

        for span @ (host_ip, first, last, proto) in ports.iter().filter_map(port_span) {
            if !expanded.insert(span) {
                continue;
            }
            let count = usize::from(last - first) + 1;
            let Some(left) = remaining.checked_sub(count) else {
                dropped = dropped.saturating_add(count);
                continue;
            };
            remaining = left;
            for port in first..=last {
                map.insert(host_ip, port, proto, Arc::clone(&info));
            }
        }
    }

    if dropped > 0 {
        map.mark_truncated();
        warn!(
            "container list expands to more than {MAX_PORT_BINDINGS} port bindings; {dropped} bindings were dropped"
        );
    }
}

fn deserialize_host_ip<'de, D>(deserializer: D) -> Result<HostIp, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<JsonStr<'de>>::deserialize(deserializer)?;
    Ok(value.map_or(HostIp::Any, |raw| parse_host_ip(&raw)))
}

/// Classify a daemon-reported host IP string.
///
/// IPv6 zone identifiers (`fe80::1%eth0`) are stripped: [`IpAddr`] cannot
/// carry a scope, and the address part is what a socket lookup compares.
fn parse_host_ip(raw: &str) -> HostIp {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return HostIp::Any;
    }

    let parsed = match trimmed.split_once('%') {
        Some((addr, zone)) if !zone.is_empty() => addr.parse::<Ipv6Addr>().map(IpAddr::V6),
        Some(_) => return HostIp::Unparseable,
        None => trimmed.parse::<IpAddr>(),
    };

    match parsed {
        Ok(ip) if ip.is_unspecified() => HostIp::Any,
        Ok(ip) => HostIp::Addr(ip),
        Err(_) => HostIp::Unparseable,
    }
}

const fn parse_port_protocol(proto: Option<&str>) -> Option<Protocol> {
    match proto {
        None => Some(Protocol::Tcp),
        Some(value) if value.eq_ignore_ascii_case("tcp") => Some(Protocol::Tcp),
        Some(value) if value.eq_ignore_ascii_case("udp") => Some(Protocol::Udp),
        Some(_) => None,
    }
}

/// The first non-empty container name without its leading `/`, else the
/// image, else the short ID, else `"container"`.
fn container_display_name(container: &DockerContainer<'_>) -> String {
    container
        .names
        .iter()
        .flatten()
        .map(|name| name.trim().trim_start_matches('/'))
        .find(|name| !name.is_empty())
        .or_else(|| trimmed_non_empty(container.image.as_deref()))
        .or_else(|| trimmed_non_empty(container.id.as_deref()).map(short_container_id))
        .unwrap_or("container")
        .to_owned()
}

/// The 12-character short form of a full container ID, borrowed from `id`.
///
/// Docker and Podman IDs are hex, so the first 12 bytes are the first 12
/// characters. An `id` shorter than that, or whose 12th byte falls inside a
/// multi-byte character, is returned whole.
///
/// ```
/// use nanodock::short_container_id;
///
/// let id = "e603f8ebd438b8405b9b835b9d38cb913ea2479f5b29f8e4308b88e9a92e8c4b";
/// assert_eq!(short_container_id(id), "e603f8ebd438");
/// assert_eq!(short_container_id("abc"), "abc");
/// ```
#[must_use]
pub fn short_container_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    use crate::Protocol;

    const SAMPLE_RESPONSE: &str = r#"[
        {
            "Id": "abc123def456",
            "Names": ["/backend-postgres-1"],
            "Image": "postgres:16",
            "Ports": [
                {"PrivatePort": 5432, "PublicPort": 5432, "Type": "tcp"}
            ]
        },
        {
            "Id": "789ghi012jkl",
            "Names": ["/backend-redis-1"],
            "Image": "redis:7-alpine",
            "Ports": [
                {"PrivatePort": 6379, "PublicPort": 6379, "Type": "tcp"}
            ]
        },
        {
            "Names": ["/no-ports"],
            "Image": "busybox",
            "Ports": []
        }
    ]"#;

    fn mapped_container(
        map: &ContainerPortMap,
        host_ip: Option<IpAddr>,
        public_port: u16,
        proto: Protocol,
    ) -> &ContainerInfo {
        map.get(host_ip, public_port, proto)
            .expect("expected container port mapping to exist")
    }

    fn assert_container_mapping(
        map: &ContainerPortMap,
        host_ip: Option<IpAddr>,
        public_port: u16,
        proto: Protocol,
        expected_name: &str,
        expected_image: &str,
    ) {
        let info = mapped_container(map, host_ip, public_port, proto);
        assert_eq!(info.name, expected_name);
        assert_eq!(info.image, expected_image);
    }

    #[test]
    fn parse_valid_response() {
        let map = parse_containers_json(SAMPLE_RESPONSE);
        assert_eq!(map.len(), 2);

        assert_container_mapping(
            &map,
            None,
            5432,
            Protocol::Tcp,
            "backend-postgres-1",
            "postgres:16",
        );
        assert_container_mapping(
            &map,
            None,
            6379,
            Protocol::Tcp,
            "backend-redis-1",
            "redis:7-alpine",
        );
    }

    #[test]
    fn parse_empty_array() {
        let map = parse_containers_json("[]");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_invalid_json_returns_empty() {
        let map = parse_containers_json("not json");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_container_without_public_port() {
        let json = r#"[{
            "Names": ["/internal"],
            "Image": "app:latest",
            "Ports": [{"PrivatePort": 8080, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        assert!(
            map.is_empty(),
            "entries without PublicPort should be skipped"
        );
    }

    #[test]
    fn container_name_strips_leading_slash() {
        let json = r#"[{
            "Names": ["/my-container"],
            "Image": "nginx:latest",
            "Ports": [{"PrivatePort": 80, "PublicPort": 80, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        assert_container_mapping(
            &map,
            None,
            80,
            Protocol::Tcp,
            "my-container",
            "nginx:latest",
        );
    }

    #[test]
    fn parse_multiple_ports_same_container() {
        let json = r#"[{
            "Names": ["/multi"],
            "Image": "app:latest",
            "Ports": [
                {"PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"},
                {"PrivatePort": 443, "PublicPort": 8443, "Type": "tcp"}
            ]
        }]"#;
        let map = parse_containers_json(json);
        assert_eq!(map.len(), 2);
        assert!(map.get(None, 8080, Protocol::Tcp).is_some());
        assert!(map.get(None, 8443, Protocol::Tcp).is_some());
    }

    #[test]
    fn parse_missing_protocol_defaults_to_tcp() {
        let json = r#"[{
            "Names": ["/web"],
            "Image": "nginx:latest",
            "Ports": [{"PrivatePort": 80, "PublicPort": 8080}]
        }]"#;
        let map = parse_containers_json(json);
        assert!(
            map.get(None, 8080, Protocol::Tcp).is_some(),
            "missing Type should default to TCP"
        );
    }

    #[test]
    fn parse_protocol_matching_is_case_insensitive() {
        let json = r#"[{
            "Names": ["/dns"],
            "Image": "bind9:latest",
            "Ports": [{"PrivatePort": 53, "PublicPort": 5353, "Type": "UDP"}]
        }]"#;
        let map = parse_containers_json(json);

        assert!(
            map.get(None, 5353, Protocol::Udp).is_some(),
            "protocol parsing should accept uppercase protocol tokens"
        );
    }

    #[test]
    fn parse_unsupported_protocol_is_skipped() {
        let json = r#"[{
            "Names": ["/sigtran"],
            "Image": "telecom:latest",
            "Ports": [{"PrivatePort": 2905, "PublicPort": 2905, "Type": "sctp"}]
        }]"#;
        let map = parse_containers_json(json);

        assert!(
            map.is_empty(),
            "unsupported protocols should not be coerced into TCP bindings"
        );
    }

    #[test]
    fn parse_podman_style_ports_with_empty_host_ip() {
        let json = r#"[{
            "Names": ["ensurily-postgres-dev"],
            "Image": "docker.io/library/postgres:14-alpine",
            "Ports": [{"host_ip": "", "container_port": 5432, "host_port": 5432, "range": 1, "protocol": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);

        assert_container_mapping(
            &map,
            None,
            5432,
            Protocol::Tcp,
            "ensurily-postgres-dev",
            "docker.io/library/postgres:14-alpine",
        );
    }

    #[test]
    fn parse_podman_style_ports_expand_ranges() {
        let json = r#"[{
            "Names": ["ensurily-localstack-dev"],
            "Image": "docker.io/localstack/localstack:latest",
            "Ports": [{"host_ip": "", "container_port": 4510, "host_port": 4510, "range": 3, "protocol": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);

        assert!(map.get(None, 4510, Protocol::Tcp).is_some());
        assert!(map.get(None, 4511, Protocol::Tcp).is_some());
        assert!(map.get(None, 4512, Protocol::Tcp).is_some());
    }

    #[test]
    fn parse_container_with_empty_name() {
        let json = r#"[{
            "Names": [],
            "Image": "app:latest",
            "Ports": [{"PrivatePort": 80, "PublicPort": 80, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        assert_eq!(
            mapped_container(&map, None, 80, Protocol::Tcp).name,
            "app:latest",
            "containers without names should fall back to their image"
        );
    }

    #[test]
    fn parse_container_without_name_or_image_uses_short_id() {
        let json = r#"[{
            "Id": "0123456789abcdef0123456789abcdef",
            "Names": ["/"],
            "Ports": [{"PrivatePort": 80, "PublicPort": 80, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        let info = mapped_container(&map, None, 80, Protocol::Tcp);
        assert_eq!(
            info.name, "0123456789ab",
            "containers without names or images should fall back to a short id"
        );
    }

    #[test]
    fn parse_container_with_explicit_host_ip() {
        let json = r#"[{
            "Names": ["/api"],
            "Image": "node:22",
            "Ports": [{"IP": "127.0.0.1", "PrivatePort": 3000, "PublicPort": 8080, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);

        assert_container_mapping(
            &map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            8080,
            Protocol::Tcp,
            "api",
            "node:22",
        );
    }

    #[test]
    fn parse_docker_wildcard_host_ip_as_unspecified() {
        let json = r#"[{
            "Names": ["/postgres"],
            "Image": "postgres:16",
            "Ports": [{"IP": "0.0.0.0", "PrivatePort": 5432, "PublicPort": 5432, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);

        assert_container_mapping(&map, None, 5432, Protocol::Tcp, "postgres", "postgres:16");
        assert!(
            map.get(Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), 5432, Protocol::Tcp)
                .is_none(),
            "unspecified IPv4 bindings should be normalized to the wildcard key"
        );
    }

    #[test]
    fn parse_ipv6_wildcard_host_ip_as_unspecified() {
        let json = r#"[{
            "Names": ["/dns"],
            "Image": "bind9:latest",
            "Ports": [{"IP": "::", "PrivatePort": 53, "PublicPort": 5353, "Type": "udp"}]
        }]"#;
        let map = parse_containers_json(json);

        assert_container_mapping(&map, None, 5353, Protocol::Udp, "dns", "bind9:latest");
        assert!(
            map.get(
                Some(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)),
                5353,
                Protocol::Udp
            )
            .is_none(),
            "unspecified IPv6 bindings should be normalized to the wildcard key"
        );
    }

    #[test]
    fn parse_strict_returns_error_on_invalid_json() {
        let result = parse_containers_json_strict("not json");
        assert!(
            result.is_err(),
            "strict parser must propagate deserialization errors"
        );
    }

    #[test]
    fn parse_strict_succeeds_on_valid_json() {
        let map = parse_containers_json_strict(SAMPLE_RESPONSE)
            .expect("strict parser should succeed on valid JSON");
        assert_eq!(map.len(), 2);
        assert_container_mapping(
            &map,
            None,
            5432,
            Protocol::Tcp,
            "backend-postgres-1",
            "postgres:16",
        );
    }

    #[test]
    fn parse_escaped_strings() {
        let json = r#"[{
            "Id": "abc1",
            "Names": ["/web\"edge"],
            "Image": "ngi\"nx:latest",
            "Ports": [{"PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        let info = mapped_container(&map, None, 8080, Protocol::Tcp);
        assert_eq!(info.id, "abc1");
        assert_eq!(info.name, "web\"edge");
        assert_eq!(info.image, "ngi\"nx:latest");

        let strict = parse_containers_json_strict(json).expect("escaped strings are valid JSON");
        assert_eq!(strict, map);
    }

    #[test]
    fn parse_link_local_ipv6_with_zone_id_keeps_other_containers() {
        let json = r#"[
            {
                "Names": ["/linklocal"],
                "Image": "app:latest",
                "Ports": [{"IP": "fe80::1%eth0", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}]
            },
            {
                "Names": ["/web"],
                "Image": "nginx:latest",
                "Ports": [{"IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": 80, "Type": "tcp"}]
            }
        ]"#;
        let expected_ip = Some(IpAddr::V6(std::net::Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1,
        )));

        let map = parse_containers_json(json);
        assert_container_mapping(
            &map,
            expected_ip,
            8080,
            Protocol::Tcp,
            "linklocal",
            "app:latest",
        );
        assert_container_mapping(&map, None, 80, Protocol::Tcp, "web", "nginx:latest");

        let strict = parse_containers_json_strict(json).expect("zone ids are not a parse error");
        assert_eq!(strict, map);
    }

    #[test]
    fn parse_unparseable_host_ip_skips_only_that_binding() {
        let json = r#"[
            {
                "Names": ["/odd"],
                "Image": "app:latest",
                "Ports": [
                    {"IP": "not-an-ip", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"},
                    {"IP": "127.0.0.1", "PrivatePort": 81, "PublicPort": 8081, "Type": "tcp"}
                ]
            },
            {
                "Names": ["/web"],
                "Image": "nginx:latest",
                "Ports": [{"PrivatePort": 80, "PublicPort": 80, "Type": "tcp"}]
            }
        ]"#;
        let map = parse_containers_json(json);

        assert_eq!(map.len(), 2);
        assert!(
            map.get(None, 8080, Protocol::Tcp).is_none(),
            "an unparseable host IP must not be widened to a wildcard binding"
        );
        assert_container_mapping(
            &map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            8081,
            Protocol::Tcp,
            "odd",
            "app:latest",
        );
        assert_container_mapping(&map, None, 80, Protocol::Tcp, "web", "nginx:latest");
    }

    #[test]
    fn parse_malformed_container_does_not_drop_the_rest() {
        let json = r#"[
            {
                "Names": ["/broken"],
                "Image": "app:latest",
                "Ports": [{"PrivatePort": 80, "PublicPort": "eighty", "Type": "tcp"}]
            },
            42,
            {
                "Names": ["/web"],
                "Image": "nginx:latest",
                "Ports": [{"PrivatePort": 80, "PublicPort": 80, "Type": "tcp"}]
            }
        ]"#;
        let map = parse_containers_json(json);

        assert_eq!(map.len(), 1, "only the well-formed container is mapped");
        assert_container_mapping(&map, None, 80, Protocol::Tcp, "web", "nginx:latest");
        assert!(
            parse_containers_json_strict(json).is_err(),
            "strict parser must still report the malformed element"
        );
    }

    #[test]
    fn parse_podman_zero_range_as_single_port() {
        let json = r#"[{
            "Names": ["single"],
            "Image": "app:latest",
            "Ports": [{"host_ip": "", "container_port": 80, "host_port": 8080, "range": 0, "protocol": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        assert_eq!(map.len(), 1);
        assert_container_mapping(&map, None, 8080, Protocol::Tcp, "single", "app:latest");
    }

    #[test]
    fn parse_compose_labels() {
        let json = r#"[{
            "Names": ["/shop-db-1"],
            "Image": "postgres:16",
            "Labels": {
                "com.docker.compose.config-hash": "abc",
                "com.docker.compose.project": "shop",
                "com.docker.compose.service": "db",
                "com.docker.compose.depends_on": {"nested": ["not", "a", "string"]}
            },
            "Ports": [{"PrivatePort": 5432, "PublicPort": 5432, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        let info = mapped_container(&map, None, 5432, Protocol::Tcp);
        assert_eq!(info.compose_project.as_deref(), Some("shop"));
        assert_eq!(info.compose_service.as_deref(), Some("db"));

        let strict = parse_containers_json_strict(json).expect("labels are valid JSON");
        assert_eq!(strict, map, "strict and lenient parsing agree on labels");
    }

    #[test]
    fn parse_non_string_compose_labels_keeps_the_container() {
        let json = r#"[
            {
                "Names": ["/numeric"],
                "Image": "app",
                "Labels": {
                    "com.docker.compose.project": 5,
                    "com.docker.compose.service": "web",
                    "io.podman.compose.project": "shop"
                },
                "Ports": [{"PublicPort": 80, "Type": "tcp"}]
            },
            {
                "Names": ["/odd"],
                "Labels": {
                    "com.docker.compose.project": {"name": "shop"},
                    "com.docker.compose.service": ["db"],
                    "io.podman.compose.project": true,
                    "unrelated": 1.5
                },
                "Ports": [{"PublicPort": 81, "Type": "tcp"}]
            },
            {
                "Names": ["/null"],
                "Labels": {"com.docker.compose.project": null, "com.docker.compose.service": "cache"},
                "Ports": [{"PublicPort": 82, "Type": "tcp"}]
            }
        ]"#;
        let lenient = parse_containers_json(json);
        assert_eq!(
            lenient.len(),
            3,
            "no container is dropped for a label value"
        );

        let numeric = mapped_container(&lenient, None, 80, Protocol::Tcp);
        assert_eq!(
            numeric.compose_project.as_deref(),
            Some("shop"),
            "a non-string Docker project label falls back to the Podman one"
        );
        assert_eq!(numeric.compose_service.as_deref(), Some("web"));

        let odd = mapped_container(&lenient, None, 81, Protocol::Tcp);
        assert_eq!(odd.name, "odd");
        assert_eq!(odd.compose_project, None, "non-string values are ignored");
        assert_eq!(odd.compose_service, None);

        let null = mapped_container(&lenient, None, 82, Protocol::Tcp);
        assert_eq!(null.compose_project, None);
        assert_eq!(null.compose_service.as_deref(), Some("cache"));

        let strict = parse_containers_json_strict(json).expect("label values never fail parsing");
        assert_eq!(strict, lenient, "strict and lenient parsing agree");
    }

    #[test]
    fn parse_podman_compose_project_label_as_fallback() {
        let json = r#"[{
            "Names": ["shop_web_1"],
            "Image": "nginx",
            "Labels": {"io.podman.compose.project": "shop", "com.docker.compose.service": "web"},
            "Ports": [{"host_ip": "", "host_port": 8080, "range": 1, "protocol": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        let info = mapped_container(&map, None, 8080, Protocol::Tcp);
        assert_eq!(
            info.compose_project.as_deref(),
            Some("shop"),
            "podman-compose projects are recognised"
        );
        assert_eq!(info.compose_service.as_deref(), Some("web"));
    }

    #[test]
    fn docker_compose_project_label_wins_over_podman_label() {
        let json = r#"[{
            "Names": ["/web"],
            "Labels": {"io.podman.compose.project": "other", "com.docker.compose.project": "shop"},
            "Ports": [{"PublicPort": 80, "Type": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        assert_eq!(
            mapped_container(&map, None, 80, Protocol::Tcp)
                .compose_project
                .as_deref(),
            Some("shop")
        );
    }

    #[test]
    fn parse_escaped_and_missing_labels() {
        let json = r#"[
            {
                "Names": ["/escaped"],
                "Labels": {"com.docker.compose.project": "my\/proj\"ect", "com.docker.compose.service": ""},
                "Ports": [{"PublicPort": 80, "Type": "tcp"}]
            },
            {
                "Names": ["/plain"],
                "Labels": null,
                "Ports": [{"PublicPort": 81, "Type": "tcp"}]
            }
        ]"#;
        let map = parse_containers_json_strict(json).expect("valid JSON");
        let escaped = mapped_container(&map, None, 80, Protocol::Tcp);
        assert_eq!(escaped.compose_project.as_deref(), Some("my/proj\"ect"));
        assert_eq!(
            escaped.compose_service, None,
            "an empty label value reads as no label"
        );
        let plain = mapped_container(&map, None, 81, Protocol::Tcp);
        assert_eq!(plain.compose_project, None, "null labels mean no project");
    }

    #[test]
    fn parse_strict_returns_empty_map_for_empty_array() {
        let map = parse_containers_json_strict("[]")
            .expect("strict parser should succeed on empty array");
        assert!(
            map.is_empty(),
            "empty JSON array should produce an empty map, not an error"
        );
    }

    /// A container list with one container per entry of `ports`, named
    /// `c0`, `c1`, and so on.
    fn containers_with_ports(ports: &[&str]) -> String {
        let containers: Vec<String> = ports
            .iter()
            .enumerate()
            .map(|(index, ports)| format!(r#"{{"Names": ["/c{index}"], "Ports": [{ports}]}}"#))
            .collect();
        format!("[{}]", containers.join(","))
    }

    #[test]
    fn port_ranges_expand_to_at_most_the_binding_cap() {
        let range = r#"{"host_port": 1, "range": 65535, "protocol": "tcp"}"#;
        let shifted: Vec<String> = (2..200)
            .map(|first| format!(r#"{{"host_port": {first}, "range": 65535, "protocol": "udp"}}"#))
            .collect();
        let mut entries = vec![
            r#"{"IP": "127.0.0.1", "PublicPort": 80, "Type": "tcp"}"#,
            range,
        ];
        entries.extend(shifted.iter().map(String::as_str));
        let json = containers_with_ports(&entries);

        let started = std::time::Instant::now();
        let map = parse_containers_json(&json);
        let elapsed = started.elapsed();

        assert!(map.len() <= MAX_PORT_BINDINGS, "got {} bindings", map.len());
        assert_eq!(
            mapped_container(
                &map,
                Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                80,
                Protocol::Tcp
            )
            .name,
            "c0",
            "containers parsed before the cap are kept"
        );
        assert!(
            map.get(None, 65535, Protocol::Tcp).is_some(),
            "a range that fits under the cap is expanded in full"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "expansion is bounded, took {elapsed:?}"
        );
        assert!(map.truncated(), "the ranges past the cap were dropped");
        let strict = parse_containers_json_strict(&json).expect("valid JSON");
        assert_eq!(strict, map, "strict and lenient parsing apply the same cap");
    }

    /// Two ranges that leave room for exactly `room` (at least 2) more
    /// bindings under the cap: every TCP port, and as many UDP ports from 1
    /// as the rest of the cap allows.
    fn ranges_leaving_room(room: usize) -> String {
        format!(
            r#"{{"host_port": 1, "range": 65535, "protocol": "tcp"}},
               {{"host_port": 1, "range": {}, "protocol": "udp"}}"#,
            MAX_PORT_BINDINGS - 65_535 - room
        )
    }

    #[test]
    fn dual_stack_duplicates_are_charged_once() {
        // Docker lists a port once for 0.0.0.0 and once for ::, which both
        // map to the wildcard key. Charged twice, the pair would use up the
        // room the last container needs.
        let json = containers_with_ports(&[
            &ranges_leaving_room(3),
            r#"{"IP": "0.0.0.0", "PublicPort": 65535, "Type": "udp"},
               {"IP": "::", "PublicPort": 65535, "Type": "udp"}"#,
            r#"{"IP": "127.0.0.1", "PublicPort": 80, "Type": "tcp"},
               {"IP": "127.0.0.1", "PublicPort": 81, "Type": "tcp"}"#,
        ]);
        let map = parse_containers_json(&json);

        assert_eq!(map.len(), MAX_PORT_BINDINGS, "the cap is filled exactly");
        assert_eq!(
            mapped_container(&map, None, 65535, Protocol::Udp).name,
            "c1"
        );
        for port in [80, 81] {
            assert_eq!(
                mapped_container(
                    &map,
                    Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                    port,
                    Protocol::Tcp
                )
                .name,
                "c2",
                "port {port} still fits under the cap"
            );
        }
        assert!(!map.truncated(), "nothing was dropped");
    }

    #[test]
    fn oversized_range_is_skipped_and_later_containers_are_kept() {
        let json = containers_with_ports(&[
            r#"{"host_port": 1, "range": 65535, "protocol": "tcp"}"#,
            r#"{"host_port": 1, "range": 65534, "protocol": "udp"}"#,
            r#"{"host_ip": "127.0.0.1", "host_port": 1, "range": 65535, "protocol": "tcp"},
               {"host_ip": "127.0.0.1", "host_port": 8080, "protocol": "tcp"}"#,
            r#"{"IP": "127.0.0.1", "PublicPort": 9090, "Type": "udp"}"#,
        ]);
        let map = parse_containers_json(&json);
        let localhost = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

        assert!(map.truncated(), "the oversized range was dropped");
        assert!(
            map.get(localhost, 1, Protocol::Tcp).is_none(),
            "the range past the cap is skipped as a whole"
        );
        assert_eq!(
            mapped_container(&map, localhost, 8080, Protocol::Tcp).name,
            "c2",
            "a later binding of the same container still fits"
        );
        assert_eq!(
            mapped_container(&map, localhost, 9090, Protocol::Udp).name,
            "c3",
            "a later container still fits"
        );
        assert_eq!(map.len(), MAX_PORT_BINDINGS - 1);
    }

    #[test]
    fn small_replies_are_not_truncated() {
        let json = containers_with_ports(&[
            r#"{"IP": "0.0.0.0", "PublicPort": 80, "Type": "tcp"}"#,
            r#"{"host_port": 1000, "range": 100, "protocol": "udp"}"#,
        ]);
        assert!(!parse_containers_json(&json).truncated());
        assert!(!parse_containers_json("[]").truncated());
        assert!(!parse_containers_json("not json").truncated());
    }

    #[test]
    fn repeated_port_ranges_are_expanded_once() {
        let range = r#"{"host_port": 1, "range": 65535, "protocol": "tcp"}"#;
        let repeated = [range; 8].join(",");
        let json = containers_with_ports(&[&repeated, r#"{"PublicPort": 80, "Type": "udp"}"#]);
        let map = parse_containers_json(&json);

        assert_eq!(map.len(), 65_535 + 1);
        assert_eq!(
            mapped_container(&map, None, 80, Protocol::Udp).name,
            "c1",
            "a repeated range does not use up the binding cap"
        );
    }

    #[test]
    fn range_applies_only_to_the_podman_format() {
        let json = r#"[{
            "Names": ["/docker"],
            "Ports": [{"PublicPort": 8000, "Type": "tcp", "range": 100}]
        }]"#;
        let map = parse_containers_json(json);
        assert_eq!(map.len(), 1, "a Docker-format entry publishes one port");

        let json = r#"[{
            "Names": ["podman"],
            "Ports": [{"host_port": 65530, "range": 100, "protocol": "tcp"}]
        }]"#;
        let map = parse_containers_json(json);
        assert_eq!(map.len(), 6, "a range ends at port 65535");
        assert!(map.get(None, 65535, Protocol::Tcp).is_some());
    }

    #[test]
    fn reserved_capacity_is_bounded_by_the_binding_cap() {
        let entries: Vec<String> = (1..200)
            .map(|first| format!(r#"{{"host_port": {first}, "range": 65535}}"#))
            .collect();
        let json = containers_with_ports(&[&entries.join(",")]);
        let map = parse_containers_json(&json);
        assert!(
            map.bindings.capacity() < 4 * MAX_PORT_BINDINGS,
            "the map is sized for at most the cap, got capacity {}",
            map.bindings.capacity()
        );

        let range = containers_with_ports(&[r#"{"host_port": 10000, "range": 10001}"#]);
        let map = parse_containers_json(&range);
        assert_eq!(map.len(), 10_001);
        assert!(map.bindings.capacity() >= 10_001);
    }
}
