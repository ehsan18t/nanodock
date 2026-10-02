# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

This release redesigns the public API ahead of 1.0. Every breaking change is marked **Breaking** below, and [Migrating from 0.1](#migrating-from-01) shows the replacement for each removed or renamed item.

### Added

- `Client` holds the daemon settings and runs detection, stop, and kill requests. `Client::new()` reads `DOCKER_HOST` and the home directory from the environment, and the chainable `home`, `timeout`, and `docker_host` setters replace them. `Client::detect`, `Client::start_detection`, `Client::stop`, and `Client::kill` do what the free functions do; `detect_containers`, `start_detection`, `stop_container`, and the new `kill_container` are shorthands for `Client::new()`. The detection timeout (3 seconds by default) is configurable, and the internal query budget is derived from it so the detection thread always hands over its result before the waiting side gives up. A `DOCKER_HOST` value can be set or ignored per client instead of only through the environment.
- `ContainerInfo` carries the Compose project and service of a container in the new `compose_project` and `compose_service` fields, read from the `com.docker.compose.project` and `com.docker.compose.service` labels. Containers started by `podman-compose` are recognised too, with `io.podman.compose.project` as a fallback for the project. A label value that is not a string (a number, boolean, `null`, array, or object) is ignored instead of making the container fail to parse, so lenient detection never drops a container over its labels.
- `ContainerInfo::new`, `ContainerInfo::with_compose_project`, and `ContainerInfo::with_compose_service` build container metadata outside the crate.
- `ContainerPortMap::truncated` tells whether bindings were dropped because a daemon's reply expanded to more than the 131072 bindings nanodock keeps per reply.
- `PortKey` names the `(Option<IpAddr>, u16, Protocol)` key of a `ContainerPortMap` binding, which iteration, `FromIterator`, and `Extend` use.
- `Error` describes what went wrong: `PermissionDenied { endpoint }` (most often a Linux user outside the `docker` group), `Timeout { endpoint }`, `HttpStatus { status }`, `InvalidResponse { source }` (a `ParseError`), and `Io { source, endpoint }` (a `std::io::Error`). `Timeout` and `Io` name the endpoint when it is known, as an `Option<String>`. Every variant that carries data is a `#[non_exhaustive]` struct variant, so later releases can add fields; match it with `..`. When every endpoint fails, detection reports the most informative failure, so a permission problem on `/var/run/docker.sock` is no longer hidden behind "daemon not found".
- `DetectionHandle::wait_result` reports why background detection produced no containers.
- `ProxyFallback` (`Allow` or `Deny`) says whether `ContainerPortMap::lookup` may match a proxy process on port and protocol alone.
- `PublishedContainerMatch::container` returns the matched container, if any, and `PublishedContainerMatch::container_arc` returns its shared `Arc<ContainerInfo>`, so a caller can keep the container with a reference count increment instead of cloning it.
- `StopOutcome::is_stopped` tells whether the container is known to be stopped.
- An optional `serde` feature derives `Serialize` and `Deserialize` for `ContainerInfo`, `Protocol`, `StopOutcome`, and `ProxyFallback`. docs.rs builds the documentation with every feature enabled.
- Unix socket discovery now also checks `$XDG_RUNTIME_DIR/docker.sock` and `$XDG_RUNTIME_DIR/podman/podman.sock` (ahead of the hardcoded `/run/user/{uid}` paths), Colima (`~/.colima/default/docker.sock`, `~/.colima/docker.sock`), OrbStack (`~/.orbstack/run/docker.sock`), Rancher Desktop (`~/.rd/docker.sock`), Lima (`~/.lima/default/sock/docker.sock`, `~/.lima/docker/sock/docker.sock`), and Podman machine on macOS (`~/.local/share/containers/podman/machine/podman.sock`, `.../machine/qemu/podman.sock`, `.../machine/podman-machine-default/podman.sock`, `$TMPDIR/podman/podman-machine-default-api.sock`). Existing paths keep their relative priority; see the README for the full order. `$TMPDIR` is searched on macOS only, where it is a private per-user directory; on Linux it is usually the shared `/tmp`, where another local user could plant a socket.
- Default Unix socket paths that do not exist are now skipped before any worker thread is spawned, and paths that resolve to the same file (for example `/var/run/docker.sock` symlinked to Docker Desktop's, OrbStack's, or Podman's socket) are queried once, at the first position. `DOCKER_HOST=unix://` still replaces the defaults and is not filtered.
- **Security:** a default Unix socket whose file is owned by neither the current user nor root is skipped, so a socket another local user planted at a well-known path can neither add containers to detection nor receive a stop request. An explicit `DOCKER_HOST=unix://` path is not checked.
- `is_container_proxy_process(name)` recognizes container runtime port-proxy processes (`docker-proxy`, `rootlesskit`, `rootlessport`, `rootlessport-child`, `slirp4netns`, `pasta`, `pasta.avx2`, `com.docker.backend`, `com.docker.vpnkit`, `vpnkit`, `wslrelay`, `gvproxy`, `limactl`), ignoring ASCII case and a trailing `.exe` and accepting names truncated to 15 bytes by Linux or to 16 bytes by macOS (such as `com.docker.backe`). A truncated name matches only when it is exactly 15 or 16 bytes long and is the start of a longer known name. Callers no longer need to keep their own list.
- `RootlessPodmanResolver` and `is_podman_rootlessport_process` are now available on every platform with the same signatures, so callers no longer need `cfg(target_os = "linux")` gates around them. Outside Linux the lookup always returns `None` without touching the filesystem.
- `RootlessPodmanResolver::clear` forgets the cached overlay storage and process answers. The resolver never refreshes its cache on its own, so use one resolver per scan or clear it between scans.
- A rootless Podman container whose first name is empty now takes its next name, or the name in its storage metadata, instead of falling back to its short ID.

### Changed

- **Breaking:** `ContainerInfo` is `#[non_exhaustive]`. Its fields stay public for reading, but code outside the crate can no longer build it with a struct literal; use `ContainerInfo::new`.
- **Breaking:** `ContainerPortMap` is a struct instead of a `HashMap` type alias. It offers `new`, `len`, `is_empty`, `get(host_ip, port, proto)`, `insert(host_ip, port, proto, info)`, `iter`, `lookup`, `Default`, `FromIterator`, and `Extend`, and `&ContainerPortMap` iterates as `((host_ip, port, proto), &ContainerInfo)`. Every binding of one container shares a single `Arc<ContainerInfo>`, so a container that publishes a large port range is no longer cloned once per port.
- **Breaking:** `Error::InvalidJson(serde_json::Error)` is replaced by `Error::InvalidResponse { source: ParseError }`, and `From<serde_json::Error> for Error` is removed. `ParseError` is opaque, so no `serde_json` type is part of the public API any more.
- **Breaking:** `parse_containers_json_strict` returns `Result<ContainerPortMap, ParseError>` instead of `Result<_, serde_json::Error>`.
- **Breaking:** `Error::DaemonNotFound` now means that no daemon listens on any known endpoint. Failures that were reported as `DaemonNotFound` before (permission denied, timeout, an error status, a malformed reply) have their own variants.
- **Breaking:** `StopOutcome::Failed` is split into `StopOutcome::Unreachable` (no daemon could be contacted, so the container was not touched), `StopOutcome::NoResponse` (a daemon received the request but gave no usable reply, so the container may or may not be stopping), and `StopOutcome::Rejected { status }` (the daemon answered with an unexpected HTTP status). The stop semantics are unchanged: the ping preflight, the rule that no second daemon is tried once one may have received the request, and the rule that any reply from the `DOCKER_HOST` daemon ends the search all still apply.
- **Breaking:** the `Serialize` and `Deserialize` derives on `ContainerInfo`, `Protocol`, and `StopOutcome` are behind the `serde` feature, which is off by default. Enable it to keep serializing these types. `StopOutcome` now also derives `Deserialize` under that feature.
- **Breaking:** `detect_containers`, `start_detection`, and `stop_container` no longer take a `home: Option<PathBuf>` argument; they use `Client::new()`, which reads the home directory from the environment. Passing `None` used to skip every per-user socket (Docker Desktop on macOS, Colima, OrbStack, Lima, Rancher Desktop, Podman machine) without any sign. Use `Client::new().home(home)` to search a specific home directory.
- **Breaking:** stopping and killing are separate calls instead of one call with a `force: bool` argument. `stop_container(id)` and `Client::stop(id)` stop the container gracefully (`POST /containers/{id}/stop?t=10`), and `kill_container(id)` and `Client::kill(id)` kill it at once (`POST /containers/{id}/kill`). Both keep the same daemon selection and fail-closed rules.
- **Breaking:** `PublishedContainerMatch::Match` holds `&Arc<ContainerInfo>` instead of `&ContainerInfo`. Field access and `Display` work through the `Arc` as before, but code that returns the bound value as `&ContainerInfo` or clones it into a `ContainerInfo` must say so: use `PublishedContainerMatch::container`, or `ContainerInfo::clone(info)` (or `Arc::clone(info)` to share it).
- **Breaking:** `StopOutcome` no longer implements `Copy`, so a later variant can carry data that is not `Copy`. Clone it where a copy was relied on; `StopOutcome::is_stopped` takes `&self`.
- **Breaking:** `ContainerInfo` and `PublishedContainerMatch` no longer implement `Hash`, so `ContainerInfo` can gain fields that cannot be hashed (such as a label map) in a minor release. Key a set or map on `info.id` instead of the whole `ContainerInfo`.
- **Breaking:** `short_container_id` returns `&str`, borrowed from its argument, instead of a new `String`. Call `.to_owned()` on the result where an owned string is needed.
- **Breaking:** `lookup_rootless_podman_container(pid, name, &mut resolver, home)` is replaced by the method `RootlessPodmanResolver::lookup(pid, name)`. The home directory belongs to the resolver: `RootlessPodmanResolver::new()` reads it from the environment and `.home(home)` replaces it, instead of being passed on every call while only the first call used it.
- `Error`'s `Display` output no longer repeats the message of the underlying error; it is available through `std::error::Error::source`. The same holds for `ParseError`: an invalid container list displays as `invalid container list JSON`, and `source()` returns the JSON parser's error with the position where the JSON broke.
- **Security:** port ranges can no longer make a small reply expand to millions of bindings. One daemon reply expands to at most 131072 bindings (every port of both protocols). A port entry that would go past the cap is skipped, while the entries and containers after it are still kept when they fit; `ContainerPortMap::truncated` then returns `true` and a warning is logged. A port entry repeated within one container is expanded and counted once, whether it is a range or a port Docker lists for both `0.0.0.0` and `::`. Before, a reply of a few kilobytes of `"range": 65535` entries took about 0.7 seconds and millions of map inserts to parse.
- `Client::detect` and `detect_containers` parse each daemon's reply on its own and merge the results, instead of splicing the replies into one JSON array first. A reply that is not a JSON array, such as `{"message": "page not found"}`, now fails with `Error::InvalidResponse`; before, it was spliced into the array as a container without ports and detection succeeded with nothing found. Background detection still skips such a reply.
- **Security:** `stop`, `kill`, `stop_container`, and `kill_container` accept only an id that can name a container: an ASCII letter or digit followed by ASCII letters, digits, `_`, `.`, or `-` (the name pattern Docker and Podman enforce, which hex IDs and ID prefixes also match), at most 256 bytes long. Anything else, such as `.`, `..`, a line separator (U+2028), a zero-width character, or a 100 KB id, is `StopOutcome::NotFound` without contacting a daemon. Before, only `/`, `?`, `#`, `%`, spaces, and control characters were rejected.
- A reply with a large Podman port range parses about twice as fast: the port map is sized once, up to the binding cap, before the bindings are inserted (a 10001-port range takes about 0.5 ms instead of 1 ms).
- The `range` field is read only from Podman's libpod port format (`host_port`). A Docker-format entry (`PublicPort`) always publishes one port, because Docker lists every port of a range as its own entry.
- The background detection wait window is measured from the moment detection started rather than from the call that waits, so it never ends later than the detection timeout after `start_detection`.

### Removed

- **Breaking:** `lookup_published_container(map, socket, proto, allow_proxy_fallback)`. Use `ContainerPortMap::lookup(ip, port, proto, fallback)` with a `ProxyFallback` in place of the `bool`.
- **Breaking:** `await_detection(handle)`. Use `DetectionHandle::wait`, or `DetectionHandle::wait_result` to keep the error.

### Fixed

- On Windows, a daemon that sends a reply without `Content-Length` over a named pipe and then stalls is reported as `Error::Timeout` once the deadline passes. Before, whatever had arrived was taken as the complete body, so a truncated reply could read as an empty container list.
- Named pipes use overlapped I/O instead of being polled every 10 ms. Each read and write waits for the kernel to complete it, for at most the time left before the deadline, and is cancelled when the deadline passes, so a detection or a stop over a named pipe takes well under 1 ms against a local pipe server instead of about 20 ms. A reply without `Content-Length` ends when the pipe is closed or when a message-mode server (Docker's) sends the empty message that marks the end of its output. Named pipes now go through the same HTTP parser as Unix sockets and TCP.
- The `GET /_ping` check before a stop or kill now requires a 2xx status. Any other reply, such as 400 from a TLS port that received plain HTTP or a 5xx from a forwarder whose backend is broken, marks the endpoint unreachable, so the stop is never sent there and the next daemon is tried. Before, any HTTP status passed the check.
- A stale `DOCKER_HOST` that points at a closed loopback port (`tcp://127.0.0.1:2375` or `tcp://localhost:2375`) no longer adds about 2 to 2.5 seconds to every detection, stop, and kill on Windows, which retries a refused connect. Connect attempts to loopback addresses are capped at 250 ms; other addresses keep the 3 second cap. A loopback attempt that the cap ends is reported like a refused connection, so with no daemon running detection still fails with `Error::DaemonNotFound`, as it does on Linux and macOS, instead of `Error::Timeout`.
- `DOCKER_HOST` accepts the `tcp://` forms the Docker CLI accepts: `tcp://host` without a port (2375), an empty host (`tcp://` or `tcp://:2376`, meaning `127.0.0.1`), a value without a scheme (`host:port`, read as `tcp://`), a trailing slash or path after the address (ignored), surrounding whitespace, and IPv6 addresses in brackets (`tcp://[::1]:2375`, `tcp://[::1]`). nanodock also accepts the scheme in any letter case, which the Docker CLI does not. Before, these values were passed to the resolver as written or ignored, so only the default endpoints were used.
- An `npipe://` `DOCKER_HOST` must name a named pipe (`//./pipe/<name>` or `//<host>/pipe/<name>`). A value such as `npipe://C:/x.txt` is ignored instead of having an ordinary file opened and sent HTTP requests. Unsupported schemes (`ssh://`, `fd://`, `http://`) are still ignored and are now logged at debug level.
- Detection no longer panics when the operating system cannot start a thread. `start_detection` reports `Error::Io` through its handle, and an endpoint whose query thread cannot start counts as an `Error::Io` failure while the other endpoints are still queried. The threads are named `nanodock-detect` and `nanodock-query`.

### Migrating from 0.1

Waiting for background detection:

```rust,ignore
// 0.1
let port_map = nanodock::await_detection(handle);
// 0.2
let port_map = handle.wait();
```

Matching a socket against the published ports:

```rust,ignore
// 0.1
let found = nanodock::lookup_published_container(&port_map, socket, proto, is_proxy);
// 0.2
use nanodock::ProxyFallback;
let fallback = if is_proxy { ProxyFallback::Allow } else { ProxyFallback::Deny };
let found = port_map.lookup(socket.ip(), socket.port(), proto, fallback);
```

Building container metadata:

```rust,ignore
// 0.1
let info = ContainerInfo { id: id.to_string(), name: name.to_string(), image: image.to_string() };
// 0.2
let info = ContainerInfo::new(id, name, image);
```

Using `ContainerPortMap`, which is no longer a `HashMap`:

```rust,ignore
// 0.1
let mut map: ContainerPortMap = HashMap::new();
map.insert((host_ip, port, proto), info);
let hit = map.get(&(host_ip, port, proto));
let known = map.contains_key(&(host_ip, port, proto));
// 0.2
let mut map = ContainerPortMap::new();
map.insert(host_ip, port, proto, info);
let hit = map.get(host_ip, port, proto);
let known = map.get(host_ip, port, proto).is_some();
// Iteration keeps its shape; the host IP is now yielded by value.
for ((host_ip, port, proto), info) in &map { /* ... */ }
```

Stopping or killing a container:

```rust,ignore
// 0.1
let outcome = nanodock::stop_container(id, force, home);
// 0.2
let outcome = if force {
    nanodock::kill_container(id)
} else {
    nanodock::stop_container(id)
};
// 0.2, a specific home directory
let client = nanodock::Client::new().home(home);
let outcome = if force { client.kill(id) } else { client.stop(id) };
```

Handling a stop that did not succeed:

```rust,ignore
// 0.1
StopOutcome::Failed => println!("stop failed or its result is unknown"),
// 0.2
StopOutcome::Unreachable => println!("no daemon could be reached"),
StopOutcome::NoResponse => println!("the daemon got the request but did not reply"),
StopOutcome::Rejected { status } => println!("the daemon answered HTTP {status}"),
```

Matching detection errors:

```rust,ignore
// 0.1
Err(Error::InvalidJson(source)) => eprintln!("bad JSON: {source}"),
// 0.2: data-carrying variants are non-exhaustive struct variants, matched with `..`
Err(Error::InvalidResponse { source, .. }) => eprintln!("bad reply: {source}"),
Err(Error::PermissionDenied { endpoint, .. }) => eprintln!("no access to {endpoint}"),
Err(Error::HttpStatus { status, .. }) => eprintln!("the daemon answered HTTP {status}"),
Err(Error::Timeout { endpoint, .. }) => eprintln!("no answer in time from {endpoint:?}"),
```

Keeping the matched container of a lookup:

```rust,ignore
// 0.1
let owned: Option<ContainerInfo> = match found {
    PublishedContainerMatch::Match(info) => Some(info.clone()),
    _ => None,
};
// 0.2: copy the container, or share it through its Arc
let owned: Option<ContainerInfo> = found.container().cloned();
let shared: Option<Arc<ContainerInfo>> = found.container_arc().cloned();
```

`ContainerInfo` no longer implements `Hash`, and `StopOutcome` no longer implements `Copy`:

```rust,ignore
// 0.1
let seen: HashSet<ContainerInfo> = containers.cloned().collect();
let again = outcome; // StopOutcome was Copy
// 0.2: key on the container ID, and clone or borrow the outcome
let seen: HashSet<String> = containers.map(|info| info.id.clone()).collect();
let again = outcome.clone();
```

Resolving a rootless Podman `rootlessport` process:

```rust,ignore
// 0.1
let mut resolver = RootlessPodmanResolver::default();
let found = nanodock::lookup_rootless_podman_container(pid, name, &mut resolver, home.as_deref());
// 0.2, home directory read from the environment
let mut resolver = RootlessPodmanResolver::new();
let found = resolver.lookup(pid, name);
// 0.2, a specific home directory
let mut resolver = RootlessPodmanResolver::new().home(home);
// one resolver per scan, or forget the cache between scans
resolver.clear();
```

`short_container_id` borrows instead of allocating:

```rust,ignore
// 0.1
let short: String = nanodock::short_container_id(&info.id);
// 0.2
let short: &str = nanodock::short_container_id(&info.id);
let owned: String = nanodock::short_container_id(&info.id).to_owned();
```

`parse_containers_json_strict` now fails with `nanodock::ParseError`; code that named `serde_json::Error` should name `ParseError` or use `impl std::error::Error`.

Serializing `ContainerInfo`, `Protocol`, or `StopOutcome`:

```toml
# 0.1
nanodock = "0.1"
# 0.2
nanodock = { version = "0.2", features = ["serde"] }
```

Calling the free functions, which no longer take a home directory:

```rust,ignore
// 0.1
let port_map = nanodock::detect_containers(home)?;
let handle = nanodock::start_detection(home);
// 0.2, home directory read from the environment
let port_map = nanodock::detect_containers()?;
let handle = nanodock::start_detection();
// 0.2, a specific home directory, timeout, or DOCKER_HOST override
let port_map = nanodock::Client::new()
    .home(home)
    .timeout(Duration::from_secs(1))
    .detect()?;
```

## [0.1.2] - 2026-10-01

### Fixed

- Container names, images, and other strings that contain JSON escape sequences (for example `\/` or `\"`) no longer make the whole container list fail to parse.
- A single malformed container entry no longer discards every other container in the response; only the bad entry is skipped.
- A published port whose host IP cannot be parsed is now skipped instead of being recorded as a wildcard binding, which could misattribute traffic on other addresses sharing that port.
- IPv6 host addresses with a zone identifier (for example `fe80::1%eth0`) are now accepted; the zone is dropped and the address is kept.
- Podman port mappings that report `range: 0` now map one port instead of being dropped.
- Detection now runs under one overall 2.5 second budget across every transport, so a daemon that trickles bytes slowly can no longer hold `detect_containers` or the detection thread past the 3 second `await_detection` window.
- A `DOCKER_HOST=tcp://` daemon is now queried concurrently with the local Unix sockets or named pipes. A stale or unreachable TCP address no longer uses up the detection budget and hides a local Docker or Podman daemon.
- On Windows, a named pipe that is busy or never answers can no longer starve the other pipes, and a pipe that streams data without end is cut off at the deadline.
- `stop_container` no longer tries another daemon after one received the request but gave no usable reply, so a same-named container on a second daemon is not stopped by mistake. That case is reported as `StopOutcome::Failed`, including when an earlier daemon answered "not found".
- `stop_container` now checks each daemon with `GET /_ping` on a separate connection (at most 2 seconds) before sending the stop request. A daemon that cannot be reached, or that does not answer the ping, is skipped without receiving the stop, so a `DOCKER_HOST=tcp://` forwarder (socat, SSH tunnel, WSL port forward) whose backend is down no longer makes the stop fail without trying the local daemons. Once the stop request has been sent, no reply is a failure, never a fallthrough: a closed or reset connection, a timeout, or a partial or malformed reply ends the search with `StopOutcome::Failed`, because the daemon may have started stopping the container before it went away.
- A "not found" from a `DOCKER_HOST=tcp://` or `npipe://` daemon now ends `stop_container` with `StopOutcome::NotFound` instead of trying the local daemons, so a same-named container on a daemon the user did not select is not stopped. The local daemons are tried only when the `DOCKER_HOST` daemon cannot be reached.
- With `DOCKER_HOST=unix://`, `stop_container` now tries only that socket, matching detection. Before, a "not found" from that socket fell through to the default sockets, so a same-named container on a daemon the user had excluded could be stopped.
- When two daemons report the same published port, the container that is kept no longer depends on which daemon answered first. Responses are merged in a fixed priority order: the `DOCKER_HOST` endpoint first, then the default sockets or pipes in their documented order.
- A stop request against a `DOCKER_HOST=tcp://` host that resolves to several addresses now caps each connect attempt at 3 seconds, so one unreachable address cannot use up the whole stop timeout.
- Container ids containing any control character are now rejected by `stop_container`, and multi-byte container names no longer panic in debug logging.
- Malformed HTTP responses are handled safely: oversized or overflowing `Content-Length` and chunk sizes, unterminated header and chunk-size lines, endless trailers, and bodies shorter than their declared length all fail cleanly instead of allocating or reading without bound.
- On Windows, a pipe wait with less than one millisecond left no longer falls back to the pipe's default wait time.

### Changed

- Response size caps: a decoded response body is limited to 64 MiB, and the status line plus headers to 64 KiB. The header cap is now enforced while reading on every transport: a socket stream without line breaks is cut off at the cap, and a Windows named pipe reply fails as soon as more than 64 KiB is buffered without a complete header block (malformed headers also fail at once instead of waiting for the deadline). Larger responses are treated as a failed query.
- `parse_containers_json_strict` no longer returns an error for an unparseable host IP; that binding is skipped and the rest of the container is kept.
- A graceful stop now sends `POST /containers/{id}/stop?t=10`. The explicit 10 second grace period overrides the stop timeout configured on the container. The overall stop request timeout is now 20 seconds (grace period plus margin).
- With `DOCKER_HOST=tcp://`, the local Unix sockets or named pipes are now contacted on every detection pass, even when the TCP daemon answers (its answer is still used on its own). Before, they were contacted only when the TCP daemon failed.
- When a `DOCKER_HOST=tcp://` daemon cannot be reached, detection and `stop_container` fall back to the local daemons. This differs from the docker CLI, which uses only `DOCKER_HOST` when it is set. The fallback is not new: earlier releases also tried the local daemons after a failed TCP query.
- On Windows, detection now queries every default named pipe (Docker Desktop and Podman machine) and merges the containers of all that answer, instead of using only the first. A `DOCKER_HOST=npipe://` pipe that answers is still used on its own.

## [0.1.1] - 2026-04-24

- Capped HTTP response header accumulation at 64 KiB, plus dependency updates and documentation improvements.

## [0.1.0] - 2026-04-17

- Initial release: synchronous Docker/Podman container detection, port mapping, and stop/kill over Unix sockets, Windows named pipes, and TCP.

[Unreleased]: https://github.com/ehsan18t/nanodock/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/ehsan18t/nanodock/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ehsan18t/nanodock/compare/0.1.0...v0.1.1
[0.1.0]: https://github.com/ehsan18t/nanodock/releases/tag/0.1.0
