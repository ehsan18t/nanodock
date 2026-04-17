//! Docker/Podman API JSON response parsing.
//!
//! Deserialises the `GET /containers/json` payload and maps published
//! ports to [`ContainerInfo`] records.

use std::net::IpAddr;

use serde::Deserialize;

use crate::{ContainerInfo, ContainerPortMap, Protocol};

#[derive(Deserialize)]
struct DockerPort<'a> {
    #[serde(
        rename = "IP",
        alias = "host_ip",
        default,
        deserialize_with = "deserialize_host_ip"
    )]
    host_ip: Option<IpAddr>,
    #[serde(rename = "PublicPort", alias = "host_port")]
    public_port: Option<u16>,
    #[serde(rename = "Type", alias = "protocol")]
    proto: Option<&'a str>,
    #[serde(alias = "range")]
    port_range: Option<u16>,
}

#[derive(Deserialize)]
struct DockerContainer<'a> {
    #[serde(rename = "Id")]
    id: Option<&'a str>,
    #[serde(rename = "Names")]
    names: Option<Vec<&'a str>>,
    #[serde(rename = "Image")]
    image: Option<&'a str>,
    #[serde(rename = "Ports")]
    ports: Option<Vec<DockerPort<'a>>>,
}

/// Parse the JSON response from `GET /containers/json` into a port map.
///
/// Each container may publish multiple ports. The map keys are
/// `(public_ip, public_port, protocol)` tuples.
#[must_use]
pub fn parse_containers_json(json_body: &str) -> ContainerPortMap {
    let mut map = ContainerPortMap::new();

    let Ok(containers) = serde_json::from_str::<Vec<DockerContainer<'_>>>(json_body) else {
        return map;
    };

    populate_port_map(&mut map, &containers);
    map
}

/// Strict variant of [`parse_containers_json`] that propagates JSON
/// deserialization errors instead of silently returning an empty map.
pub fn parse_containers_json_strict(
    json_body: &str,
) -> Result<ContainerPortMap, serde_json::Error> {
    let containers = serde_json::from_str::<Vec<DockerContainer<'_>>>(json_body)?;
    let mut map = ContainerPortMap::new();
    populate_port_map(&mut map, &containers);
    Ok(map)
}

fn populate_port_map(map: &mut ContainerPortMap, containers: &[DockerContainer<'_>]) {
    for container in containers {
        let id = container.id.unwrap_or("").to_string();
        let name = container_display_name(container);
        let image = container.image.unwrap_or("").to_string();
        let info = ContainerInfo { id, name, image };

        let Some(ports) = &container.ports else {
            continue;
        };

        for port in ports {
            let Some(public_port) = port.public_port else {
                continue;
            };
            let Some(proto) = parse_port_protocol(port.proto) else {
                continue;
            };

            let port_count = port.port_range.unwrap_or(1);
            for offset in 0..port_count {
                let Some(mapped_port) = public_port.checked_add(offset) else {
                    break;
                };

                map.insert((port.host_ip, mapped_port, proto), info.clone());
            }
        }
    }
}

fn deserialize_host_ip<'de, D>(deserializer: D) -> Result<Option<IpAddr>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .as_deref()
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .map(str::parse)
        .transpose()
        .map(|host_ip| host_ip.filter(|ip: &IpAddr| !ip.is_unspecified()))
        .map_err(serde::de::Error::custom)
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
        .and_then(|names| names.iter().copied().find_map(normalize_container_name))
        .or_else(|| {
            container
                .image
                .map(str::trim)
                .filter(|image| !image.is_empty())
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            container
                .id
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
        map.get(&(host_ip, public_port, proto))
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
        assert!(map.contains_key(&(None, 8080, Protocol::Tcp)));
        assert!(map.contains_key(&(None, 8443, Protocol::Tcp)));
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
            map.contains_key(&(None, 8080, Protocol::Tcp)),
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
            map.contains_key(&(None, 5353, Protocol::Udp)),
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

        assert!(map.contains_key(&(None, 4510, Protocol::Tcp)));
        assert!(map.contains_key(&(None, 4511, Protocol::Tcp)));
        assert!(map.contains_key(&(None, 4512, Protocol::Tcp)));
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
            !map.contains_key(&(Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED)), 5432, Protocol::Tcp)),
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
            !map.contains_key(&(
                Some(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)),
                5353,
                Protocol::Udp
            )),
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
    fn parse_strict_returns_empty_map_for_empty_array() {
        let map = parse_containers_json_strict("[]")
            .expect("strict parser should succeed on empty array");
        assert!(
            map.is_empty(),
            "empty JSON array should produce an empty map, not an error"
        );
    }
}
