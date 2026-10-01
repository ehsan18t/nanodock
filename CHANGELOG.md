# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `ContainerInfo` carries the Compose project and service of a container in the new `compose_project` and `compose_service` fields, read from the `com.docker.compose.project` and `com.docker.compose.service` labels. Containers started by `podman-compose` are recognised too, with `io.podman.compose.project` as a fallback for the project.
- `ContainerInfo::new`, `ContainerInfo::with_compose_project`, and `ContainerInfo::with_compose_service` build container metadata outside the crate.
- `PublishedContainerMatch::container` returns the matched container, if any.
- `StopOutcome::is_stopped` tells whether the container is known to be stopped.
- `Error` describes what went wrong: `PermissionDenied { endpoint }` (most often a Linux user outside the `docker` group), `Timeout`, `HttpStatus(u16)`, `InvalidResponse(ParseError)`, and `Io(std::io::Error)`. When every endpoint fails, detection reports the most informative failure, so a permission problem on `/var/run/docker.sock` is no longer hidden behind "daemon not found".

### Changed

- **Breaking:** `ContainerInfo` is `#[non_exhaustive]`. Its fields stay public for reading, but code outside the crate can no longer build it with a struct literal; use `ContainerInfo::new` instead.
- **Breaking:** `ContainerPortMap` is a struct instead of a `HashMap` type alias. It offers `new`, `len`, `is_empty`, `get(host_ip, port, proto)`, `insert(host_ip, port, proto, info)`, `iter`, `lookup`, `Default`, `FromIterator`, and `Extend`, and `&ContainerPortMap` iterates as `((host_ip, port, proto), &ContainerInfo)`. Every binding of one container shares a single `Arc<ContainerInfo>`, so a container that publishes a large port range is no longer cloned once per port.
- **Breaking:** `Error::InvalidJson(serde_json::Error)` is replaced by `Error::InvalidResponse(ParseError)`, and `From<serde_json::Error> for Error` is removed. `ParseError` is an opaque error type, so no `serde_json` type is part of the public API any more.
- **Breaking:** `parse_containers_json_strict` returns `Result<ContainerPortMap, ParseError>` instead of `Result<_, serde_json::Error>`.
- **Breaking:** `Error::DaemonNotFound` now means that no daemon listens on any known endpoint. Failures that were reported as `DaemonNotFound` before (permission denied, timeout, an error status, a malformed reply) now have their own variants.
- `Error`'s `Display` output no longer repeats the message of the underlying error; it is available through `std::error::Error::source`.
- **Breaking:** `StopOutcome::Failed` is split into `StopOutcome::Unreachable` (no daemon could be contacted, so the container was not touched), `StopOutcome::NoResponse` (a daemon received the request but gave no usable reply, so the container may or may not be stopping), and `StopOutcome::Rejected { status }` (the daemon answered with an unexpected HTTP status). The stop semantics are unchanged: the ping preflight, the rule that no second daemon is tried once one may have received the request, and the rule that any reply from the `DOCKER_HOST` daemon ends the search all still apply.

### Removed

- **Breaking:** `lookup_published_container(map, socket, proto, allow_proxy_fallback)` is replaced by the `ContainerPortMap::lookup(ip, port, proto, fallback)` method, and the `allow_proxy_fallback: bool` argument by the `ProxyFallback` enum (`Allow` or `Deny`).

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
