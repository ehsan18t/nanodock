//! Docker/Podman API JSON response parsing.
//!
//! Deserialises the `GET /containers/json` payload and maps published
//! ports to [`ContainerInfo`] records.

use std::borrow::Cow;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::ops::Deref;
use std::sync::Arc;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, Visitor};

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

#[derive(Deserialize)]
struct DockerPort<'a> {
    #[serde(
        rename = "IP",
        alias = "host_ip",
        default,
        deserialize_with = "deserialize_host_ip"
    )]
    host_ip: HostIp,
    #[serde(rename = "PublicPort", alias = "host_port")]
    public_port: Option<u16>,
    #[serde(rename = "Type", alias = "protocol", borrow)]
    proto: Option<JsonStr<'a>>,
    #[serde(alias = "range")]
    port_range: Option<u16>,
}

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
/// skipped without being decoded or copied.
#[derive(Default)]
struct ComposeLabels<'a> {
    project: Option<JsonStr<'a>>,
    service: Option<JsonStr<'a>>,
    podman_project: Option<JsonStr<'a>>,
}

impl ComposeLabels<'_> {
    /// The Compose project, preferring the Docker label over the Podman one.
    fn project(&self) -> Option<String> {
        non_empty_label(self.project.as_deref())
            .or_else(|| non_empty_label(self.podman_project.as_deref()))
    }

    fn service(&self) -> Option<String> {
        non_empty_label(self.service.as_deref())
    }
}

fn non_empty_label(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
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
            *slot = map.next_value::<Option<JsonStr<'de>>>()?;
        }
        Ok(labels)
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
    let mut map = ContainerPortMap::new();

    // Fast path: the whole array deserializes in one zero-copy pass.
    if let Ok(containers) = serde_json::from_str::<Vec<DockerContainer<'_>>>(json_body) {
        populate_port_map(&mut map, &containers);
        return map;
    }

    // Slow path, only taken for malformed input: decode the array into
    // untyped values, then convert each element independently so one bad
    // container cannot poison the rest.
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

fn populate_port_map(map: &mut ContainerPortMap, containers: &[DockerContainer<'_>]) {
    for container in containers {
        let id = container.id.as_deref().unwrap_or("").to_string();
        let name = container_display_name(container);
        let image = container.image.as_deref().unwrap_or("").to_string();
        let mut info = ContainerInfo::new(id, name, image);
        if let Some(labels) = &container.labels {
            info.compose_project = labels.project();
            info.compose_service = labels.service();
        }
        // Shared by every binding of this container.
        let info = Arc::new(info);

        let Some(ports) = &container.ports else {
            continue;
        };

        for port in ports {
            let Some(public_port) = port.public_port else {
                continue;
            };
            let Some(proto) = parse_port_protocol(port.proto.as_deref()) else {
                continue;
            };
            let host_ip = match port.host_ip {
                HostIp::Any => None,
                HostIp::Addr(ip) => Some(ip),
                HostIp::Unparseable => continue,
            };

            // Podman may report `range: 0` for a single-port binding; treat
            // it like 1 so the binding is not silently dropped.
            let port_count = port.port_range.unwrap_or(1).max(1);
            for offset in 0..port_count {
                let Some(mapped_port) = public_port.checked_add(offset) else {
                    break;
                };

                map.insert(host_ip, mapped_port, proto, Arc::clone(&info));
            }
        }
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

fn container_display_name(container: &DockerContainer<'_>) -> String {
    container
        .names
        .as_ref()
        .and_then(|names| names.iter().find_map(|name| normalize_container_name(name)))
        .or_else(|| {
            container
                .image
                .as_deref()
                .map(str::trim)
                .filter(|image| !image.is_empty())
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            container
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(short_container_id)
        })
        .unwrap_or_else(|| "container".to_string())
}

fn normalize_container_name(name: &str) -> Option<String> {
    let normalized = name.trim().trim_start_matches('/');
    (!normalized.is_empty()).then(|| normalized.to_string())
}

/// Truncate a full container ID to its 12-character short form.
///
/// Docker/Podman container IDs are hex-encoded (ASCII-only), so byte
/// length equals character count and a byte slice is safe.
#[must_use]
pub fn short_container_id(id: &str) -> String {
    id.get(..12).unwrap_or(id).to_string()
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
}
