//! Published port bindings: [`Protocol`], [`ContainerInfo`], and the
//! [`ContainerPortMap`] that maps each binding to its container.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Network transport protocol.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Protocol {
    /// Transmission Control Protocol.
    #[cfg_attr(feature = "serde", serde(rename = "TCP"))]
    Tcp,
    /// User Datagram Protocol.
    #[cfg_attr(feature = "serde", serde(rename = "UDP"))]
    Udp,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp => write!(f, "TCP"),
            Self::Udp => write!(f, "UDP"),
        }
    }
}

// ── Container types ──────────────────────────────────────────────────

/// Metadata about a running container that has published ports.
///
/// The fields are public for reading. The struct is `#[non_exhaustive]`, so
/// code outside this crate builds one with [`ContainerInfo::new`] and the
/// `with_*` methods instead of a struct literal, which lets later releases
/// add fields without a breaking change.
///
/// ```
/// use nanodock::ContainerInfo;
///
/// let info = ContainerInfo::new("abc123", "shop-db-1", "postgres:16")
///     .with_compose_project("shop")
///     .with_compose_service("db");
/// assert_eq!(info.name, "shop-db-1");
/// assert_eq!(info.compose_project.as_deref(), Some("shop"));
/// assert_eq!(info.compose_service.as_deref(), Some("db"));
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ContainerInfo {
    /// Full container ID (hex string) for API calls, empty when unavailable.
    pub id: String,
    /// Container name (e.g. "backend-postgres-1").
    pub name: String,
    /// Container image (e.g. "postgres:16").
    pub image: String,
    /// Compose project the container belongs to, read from the
    /// `com.docker.compose.project` label (or `io.podman.compose.project`
    /// when only that one is set). `None` when the container was not
    /// started by Docker Compose or `podman-compose`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub compose_project: Option<String>,
    /// Compose service name, read from the `com.docker.compose.service`
    /// label. `None` when the label is absent.
    #[cfg_attr(feature = "serde", serde(default))]
    pub compose_service: Option<String>,
}

impl ContainerInfo {
    /// Create container metadata with no Compose labels.
    ///
    /// `id` is the full container ID (empty when unknown), `name` the
    /// container name without a leading `/`, and `image` the image
    /// reference.
    #[must_use]
    pub fn new(id: impl Into<String>, name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            image: image.into(),
            compose_project: None,
            compose_service: None,
        }
    }

    /// Set the Compose project name.
    #[must_use]
    pub fn with_compose_project(mut self, project: impl Into<String>) -> Self {
        self.compose_project = Some(project.into());
        self
    }

    /// Set the Compose service name.
    #[must_use]
    pub fn with_compose_service(mut self, service: impl Into<String>) -> Self {
        self.compose_service = Some(service.into());
        self
    }
}

impl std::fmt::Display for ContainerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.image.is_empty() {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{} ({})", self.name, self.image)
        }
    }
}

/// Key of one published binding in a [`ContainerPortMap`]: host IP (`None`
/// for a wildcard binding on every interface), host port, and protocol.
///
/// Iterating a map yields `(PortKey, &ContainerInfo)` pairs, and a map can
/// be collected from `(PortKey, info)` pairs.
///
/// ```
/// use nanodock::{ContainerInfo, ContainerPortMap, PortKey, Protocol};
///
/// let key: PortKey = (None, 80, Protocol::Tcp);
/// let map: ContainerPortMap = [(key, ContainerInfo::new("abc", "web", "nginx"))]
///     .into_iter()
///     .collect();
/// let keys: Vec<PortKey> = map.iter().map(|(key, _)| key).collect();
/// assert_eq!(keys, vec![key]);
/// ```
pub type PortKey = (Option<IpAddr>, u16, Protocol);

/// Published container ports: maps `(host_ip, host_port, protocol)` to the
/// container that publishes it.
///
/// A host IP of `None` is a wildcard binding (`0.0.0.0`, `::`, or no
/// address). Every binding of one container shares a single
/// [`ContainerInfo`] through an [`Arc`], so a container that publishes a
/// large port range costs one pointer per port rather than one copy.
///
/// ```
/// use std::net::{IpAddr, Ipv4Addr};
/// use nanodock::{ContainerInfo, ContainerPortMap, Protocol, ProxyFallback};
///
/// let mut map = ContainerPortMap::default();
/// map.insert(None, 5432, Protocol::Tcp, ContainerInfo::new("abc", "db", "postgres:16"));
///
/// assert_eq!(map.len(), 1);
/// assert_eq!(map.get(None, 5432, Protocol::Tcp).map(|info| info.name.as_str()), Some("db"));
///
/// // A socket on any address matches the wildcard binding.
/// let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// let found = map.lookup(localhost, 5432, Protocol::Tcp, ProxyFallback::Deny);
/// assert_eq!(found.container().map(|info| info.name.as_str()), Some("db"));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerPortMap {
    pub(crate) bindings: HashMap<PortKey, Arc<ContainerInfo>>,
    truncated: bool,
}

impl ContainerPortMap {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of published bindings (one per host IP, port, and protocol).
    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether no binding is published.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Whether bindings were dropped because a daemon's reply expanded to
    /// more than nanodock keeps.
    ///
    /// Port ranges let a small reply describe millions of bindings, so one
    /// daemon reply expands to at most 131072 bindings (every port of both
    /// protocols). A port entry that would go past that is skipped, the
    /// entries after it are still kept when they fit, and this returns
    /// `true`. A lookup that finds nothing in a truncated map may have
    /// missed a container. A warning is also logged when it happens.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// The container published on exactly this binding.
    ///
    /// This is an exact key lookup: `host_ip` of `None` finds only wildcard
    /// bindings. Use [`ContainerPortMap::lookup`] to match a local socket
    /// address, which also falls back to wildcard bindings.
    #[must_use]
    pub fn get(
        &self,
        host_ip: Option<IpAddr>,
        port: u16,
        proto: Protocol,
    ) -> Option<&ContainerInfo> {
        self.bindings.get(&(host_ip, port, proto)).map(Arc::as_ref)
    }

    /// Record that `info` publishes `(host_ip, port, proto)`, returning the
    /// container previously recorded for that binding, if any.
    ///
    /// Pass an `Arc<ContainerInfo>` to share one container between several
    /// bindings, or a plain [`ContainerInfo`].
    pub fn insert(
        &mut self,
        host_ip: Option<IpAddr>,
        port: u16,
        proto: Protocol,
        info: impl Into<Arc<ContainerInfo>>,
    ) -> Option<Arc<ContainerInfo>> {
        self.bindings.insert((host_ip, port, proto), info.into())
    }

    /// Iterate over every binding in arbitrary order.
    #[must_use]
    pub fn iter(&self) -> PortMapIter<'_> {
        PortMapIter {
            inner: self.bindings.iter(),
        }
    }

    /// Match a local socket address against the published bindings.
    ///
    /// Exact `(ip, port, proto)` matches win first. If the daemon reported an
    /// unspecified host IP (stored as `None`), the wildcard binding is used
    /// next. For known proxy or helper processes (`docker-proxy`,
    /// `rootlessport`, and similar), [`ProxyFallback::Allow`] also accepts a
    /// unique `(port, proto)` match when the proxy's socket address does not
    /// line up with the published host IP; more than one distinct container
    /// on that port and protocol is [`PublishedContainerMatch::Ambiguous`].
    #[must_use]
    pub fn lookup(
        &self,
        ip: IpAddr,
        port: u16,
        proto: Protocol,
        fallback: ProxyFallback,
    ) -> PublishedContainerMatch<'_> {
        if let Some(container) = self.bindings.get(&(Some(ip), port, proto)) {
            return PublishedContainerMatch::Match(container);
        }

        if let Some(container) = self.bindings.get(&(None, port, proto)) {
            return PublishedContainerMatch::Match(container);
        }

        match fallback {
            ProxyFallback::Allow => self.unique_published_container(port, proto),
            ProxyFallback::Deny => PublishedContainerMatch::NotFound,
        }
    }

    fn unique_published_container(
        &self,
        port: u16,
        proto: Protocol,
    ) -> PublishedContainerMatch<'_> {
        let mut matches = self
            .bindings
            .iter()
            .filter(|((_, candidate_port, candidate_proto), _)| {
                *candidate_port == port && *candidate_proto == proto
            })
            .map(|(_, container)| container);

        let Some(first) = matches.next() else {
            return PublishedContainerMatch::NotFound;
        };

        if matches.all(|candidate| Arc::ptr_eq(candidate, first) || candidate == first) {
            PublishedContainerMatch::Match(first)
        } else {
            PublishedContainerMatch::Ambiguous
        }
    }

    /// Make room for at least `additional` more bindings.
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.bindings.reserve(additional);
    }

    /// Record that bindings were dropped while this map was built.
    pub(crate) const fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    /// Add every binding of `other`, replacing bindings with the same key.
    /// The result is truncated when either map was.
    pub(crate) fn merge(&mut self, other: Self) {
        if self.bindings.is_empty() {
            self.bindings = other.bindings;
        } else {
            self.bindings.extend(other.bindings);
        }
        self.truncated |= other.truncated;
    }
}

impl<'a> IntoIterator for &'a ContainerPortMap {
    type Item = (PortKey, &'a ContainerInfo);
    type IntoIter = PortMapIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<I: Into<Arc<ContainerInfo>>> FromIterator<(PortKey, I)> for ContainerPortMap {
    fn from_iter<T: IntoIterator<Item = (PortKey, I)>>(iter: T) -> Self {
        let mut map = Self::new();
        map.extend(iter);
        map
    }
}

impl<I: Into<Arc<ContainerInfo>>> Extend<(PortKey, I)> for ContainerPortMap {
    fn extend<T: IntoIterator<Item = (PortKey, I)>>(&mut self, iter: T) {
        self.bindings
            .extend(iter.into_iter().map(|(key, info)| (key, info.into())));
    }
}

/// Iterator over the bindings of a [`ContainerPortMap`], created by
/// [`ContainerPortMap::iter`].
///
/// Yields `((host_ip, port, protocol), container)` in arbitrary order.
#[derive(Debug, Clone)]
pub struct PortMapIter<'a> {
    inner: std::collections::hash_map::Iter<'a, PortKey, Arc<ContainerInfo>>,
}

impl<'a> Iterator for PortMapIter<'a> {
    type Item = (PortKey, &'a ContainerInfo);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(key, info)| (*key, info.as_ref()))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for PortMapIter<'_> {}

impl std::iter::FusedIterator for PortMapIter<'_> {}

/// Whether [`ContainerPortMap::lookup`] may fall back to matching on port
/// and protocol alone.
///
/// Docker's `docker-proxy` and Podman's `rootlessport` hold the host socket
/// for a published port, and the address they listen on does not always
/// equal the host IP the daemon reports. Allow the fallback only for such
/// processes: for any other process a port-only match would attribute an
/// unrelated listener to a container.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ProxyFallback {
    /// Accept a unique container on the same port and protocol when no
    /// address-level binding matches.
    Allow,
    /// Match on the address-level bindings only.
    #[default]
    Deny,
}

/// Result of matching a socket against published container port bindings.
///
/// A match borrows the [`Arc`] the [`ContainerPortMap`] stores, so a caller
/// that keeps the container beyond the map can clone the `Arc` (a reference
/// count increment) instead of the whole [`ContainerInfo`]. Fields and
/// [`Display`](std::fmt::Display) are reachable through the `Arc` directly.
///
/// ```
/// use std::net::{IpAddr, Ipv4Addr};
/// use std::sync::Arc;
/// use nanodock::{ContainerInfo, ContainerPortMap, Protocol, ProxyFallback};
///
/// let mut map = ContainerPortMap::new();
/// map.insert(None, 80, Protocol::Tcp, ContainerInfo::new("abc", "web", "nginx"));
///
/// let found = map.lookup(IpAddr::V4(Ipv4Addr::LOCALHOST), 80, Protocol::Tcp, ProxyFallback::Deny);
/// let kept: Option<Arc<ContainerInfo>> = found.container_arc().cloned();
/// assert_eq!(kept.map(|info| info.name.clone()).as_deref(), Some("web"));
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishedContainerMatch<'a> {
    /// Exactly one container binding matched the socket.
    Match(&'a Arc<ContainerInfo>),
    /// No published container binding matched the socket.
    NotFound,
    /// Multiple distinct published bindings matched and no safe choice exists.
    Ambiguous,
}

impl<'a> PublishedContainerMatch<'a> {
    /// The matched container, or `None` for [`NotFound`](Self::NotFound) and
    /// [`Ambiguous`](Self::Ambiguous).
    #[must_use]
    pub fn container(self) -> Option<&'a ContainerInfo> {
        self.container_arc().map(Arc::as_ref)
    }

    /// The shared [`Arc`] of the matched container, or `None` for
    /// [`NotFound`](Self::NotFound) and [`Ambiguous`](Self::Ambiguous).
    ///
    /// Clone the `Arc` to keep the container without copying it.
    #[must_use]
    pub const fn container_arc(self) -> Option<&'a Arc<ContainerInfo>> {
        match self {
            Self::Match(info) => Some(info),
            Self::NotFound | Self::Ambiguous => None,
        }
    }
}

impl std::fmt::Display for PublishedContainerMatch<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Match(info) => write!(f, "{info}"),
            Self::NotFound => write!(f, "no matching container"),
            Self::Ambiguous => write!(f, "ambiguous match"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use std::net::Ipv4Addr;

    fn insert_test_container(
        map: &mut ContainerPortMap,
        host_ip: Option<IpAddr>,
        port: u16,
        proto: Protocol,
        id: &str,
        name: &str,
        image: &str,
    ) {
        map.insert(host_ip, port, proto, ContainerInfo::new(id, name, image));
    }

    #[test]
    fn lookup_keeps_protocol_bindings_separate() {
        let mut map = ContainerPortMap::new();
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            53,
            Protocol::Tcp,
            "tcp53",
            "dns-tcp",
            "bind9",
        );
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            53,
            Protocol::Udp,
            "udp53",
            "dns-udp",
            "bind9",
        );

        let tcp = map.lookup(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            53,
            Protocol::Tcp,
            ProxyFallback::Deny,
        );
        let udp = map.lookup(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            53,
            Protocol::Udp,
            ProxyFallback::Deny,
        );

        assert!(matches!(
            tcp,
            PublishedContainerMatch::Match(info) if info.name == "dns-tcp"
        ));
        assert!(matches!(
            udp,
            PublishedContainerMatch::Match(info) if info.name == "dns-udp"
        ));
    }

    #[test]
    fn lookup_marks_ambiguous_proxy_matches() {
        let mut map = ContainerPortMap::new();
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            8080,
            Protocol::Tcp,
            "api-a",
            "api-a",
            "node:22",
        );
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))),
            8080,
            Protocol::Tcp,
            "api-b",
            "api-b",
            "node:22",
        );

        let result = map.lookup(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            8080,
            Protocol::Tcp,
            ProxyFallback::Allow,
        );

        assert_eq!(result, PublishedContainerMatch::Ambiguous);
    }

    #[test]
    fn lookup_uses_normalized_wildcard_bindings() {
        let map = api::parse_containers_json(
            r#"[{
                "Names": ["/postgres"],
                "Image": "postgres:16",
                "Ports": [{"IP": "0.0.0.0", "PrivatePort": 5432, "PublicPort": 5432, "Type": "tcp"}]
            }]"#,
        );

        let result = map.lookup(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            5432,
            Protocol::Tcp,
            ProxyFallback::Deny,
        );

        assert!(matches!(
            result,
            PublishedContainerMatch::Match(info) if info.name == "postgres"
        ));
    }

    #[test]
    fn port_range_bindings_share_one_container() {
        let map = api::parse_containers_json(
            r#"[{"Names": ["/range"], "Ports": [{"host_port": 4000, "range": 100, "protocol": "tcp"}]}]"#,
        );
        assert_eq!(map.len(), 100, "every port in the range is mapped");
        let first = map
            .bindings
            .get(&(None, 4000, Protocol::Tcp))
            .expect("first port");
        let last = map
            .bindings
            .get(&(None, 4099, Protocol::Tcp))
            .expect("last port");
        assert!(
            Arc::ptr_eq(first, last),
            "a port range must not clone the container per port"
        );
    }

    #[test]
    fn port_map_collects_and_iterates_bindings() {
        let map: ContainerPortMap = [
            (
                (None, 80, Protocol::Tcp),
                ContainerInfo::new("a", "web", "nginx"),
            ),
            (
                (Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), 53, Protocol::Udp),
                ContainerInfo::new("b", "dns", "bind9"),
            ),
        ]
        .into_iter()
        .collect();

        assert_eq!(map.len(), 2);
        assert!(!map.is_empty());
        let mut names: Vec<_> = map.iter().map(|(_, info)| info.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["dns", "web"]);
        assert_eq!(map.iter().len(), 2, "the iterator knows its length");
        assert!(
            map.get(None, 53, Protocol::Udp).is_none(),
            "get is an exact key lookup"
        );
        assert!(ContainerPortMap::default().is_empty());
    }

    #[test]
    fn lookup_shares_the_stored_arc() {
        let mut map = ContainerPortMap::new();
        let shared = Arc::new(ContainerInfo::new("a", "web", "nginx"));
        map.insert(None, 80, Protocol::Tcp, Arc::clone(&shared));
        // Bound on another address, so the lookup below needs the fallback.
        map.insert(
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            443,
            Protocol::Tcp,
            Arc::clone(&shared),
        );

        for (port, fallback) in [(80, ProxyFallback::Deny), (443, ProxyFallback::Allow)] {
            let found = map.lookup(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                port,
                Protocol::Tcp,
                fallback,
            );
            let arc = found.container_arc().expect("the container is published");
            assert!(
                Arc::ptr_eq(arc, &shared),
                "port {port}: the match hands out the stored Arc, not a copy"
            );
            assert_eq!(
                found.container().map(|info| info.name.as_str()),
                Some("web")
            );
        }
        let missing = map.lookup(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            8080,
            Protocol::Tcp,
            ProxyFallback::Allow,
        );
        assert!(missing.container_arc().is_none());
        assert!(missing.container().is_none());
    }

    #[test]
    fn insert_returns_the_replaced_container() {
        let mut map = ContainerPortMap::new();
        assert!(
            map.insert(
                None,
                80,
                Protocol::Tcp,
                ContainerInfo::new("a", "old", "img")
            )
            .is_none()
        );
        let previous = map.insert(
            None,
            80,
            Protocol::Tcp,
            ContainerInfo::new("b", "new", "img"),
        );
        assert_eq!(
            previous.map(|info| info.name.clone()).as_deref(),
            Some("old")
        );
        assert_eq!(
            map.get(None, 80, Protocol::Tcp)
                .map(|info| info.name.as_str()),
            Some("new")
        );
    }

    #[test]
    fn proxy_fallback_finds_unique_container_on_other_address() {
        let mut map = ContainerPortMap::new();
        insert_test_container(
            &mut map,
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))),
            8080,
            Protocol::Tcp,
            "api",
            "api",
            "node:22",
        );
        let unspecified = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

        assert_eq!(
            map.lookup(unspecified, 8080, Protocol::Tcp, ProxyFallback::Deny),
            PublishedContainerMatch::NotFound,
            "without the fallback only address-level bindings match"
        );
        let found = map.lookup(unspecified, 8080, Protocol::Tcp, ProxyFallback::Allow);
        assert_eq!(
            found.container().map(|info| info.name.as_str()),
            Some("api")
        );
        assert_eq!(ProxyFallback::default(), ProxyFallback::Deny);
    }
}
