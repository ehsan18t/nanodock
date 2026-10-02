<div align="center">
    <h1>nanodock</h1>
    <p>A lightweight zero-bloat Rust library for detecting Docker and Podman containers, mapping their published ports to host sockets, and controlling container lifecycle. Built for embedding into CLI tools and system utilities that need container awareness without pulling in a full Docker SDK.</p>

[![CI](https://github.com/ehsan18t/nanodock/actions/workflows/ci.yml/badge.svg)](https://github.com/ehsan18t/nanodock/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/nanodock.svg)](https://crates.io/crates/nanodock)
[![docs.rs](https://docs.rs/nanodock/badge.svg)](https://docs.rs/nanodock)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

</div>


## Features

- **Container detection** - Queries Docker and Podman daemons to discover running containers and their published port bindings.
- **Port-to-container mapping** - Resolves which container owns a given host `(ip, port, protocol)` tuple, with wildcard and proxy-fallback matching.
- **Compose awareness** - Reports the Docker Compose (or `podman-compose`) project and service of each container from its labels.
- **Container lifecycle control** - Stop or kill containers by ID through the daemon API (graceful SIGTERM or immediate SIGKILL), with an outcome that tells "unreachable", "no reply", and "rejected" apart.
- **Actionable errors** - Detection failures say what happened: no daemon, permission denied (with the socket path), timeout, an HTTP error status, or an invalid reply.
- **Multi-transport support** - Connects via Unix domain sockets, Windows named pipes, or TCP (`DOCKER_HOST`), with automatic discovery of socket paths.
- **Rootless Podman support** - Resolves rootless Podman containers on Linux by reading overlay storage metadata and matching network namespace paths.
- **Background detection** - Spawns detection on a background thread so callers can do other work (socket enumeration, process lookup) concurrently.
- **Configurable** - A `Client` sets the detection timeout, the home directory, and the `DOCKER_HOST` override without touching the environment.
- **Minimal dependencies** - Only `serde`, `serde_json`, `httparse`, and `log` at runtime, plus `libc` on Unix. No async runtime, no `tokio`, no `hyper`.
- **Cross-platform** - Tested in CI on Linux (x86-64) and Windows (x86-64). macOS and other Unix targets build through the same `cfg(unix)` code path but are not tested in CI.

## Why nanodock?

Most Rust Docker libraries (`bollard`, `docker-api`) are full API clients that require an async runtime and pull in 30-50+ transitive dependencies. nanodock takes the opposite approach: synchronous, minimal, and focused.

| Crate        | Async | Runtime Deps             | Scope                         |
| ------------ | ----- | ------------------------ | ----------------------------- |
| `bollard`    | Yes   | ~50+                     | Full Docker API               |
| `docker-api` | Yes   | ~30+                     | Full Docker API               |
| **nanodock** | No    | **4** (+ `libc` on Unix) | Detection + Ports + Lifecycle |

Use nanodock when you need container awareness (detection, port mapping, lifecycle control) without pulling in an async runtime or a full Docker SDK. Ideal for CLI tools, system utilities, and monitoring agents.

## Quick Start

Add nanodock to your `Cargo.toml`:

```toml
[dependencies]
nanodock = "0.2"
```

Enable the optional `serde` feature to derive `Serialize` and `Deserialize` for `ContainerInfo`, `Protocol`, `StopOutcome`, and `ProxyFallback`:

```toml
[dependencies]
nanodock = { version = "0.2", features = ["serde"] }
```

### Detect containers and map ports

Two detection paths are available:

**Best-effort path** (background thread, never errors):

```rust,no_run
use nanodock::start_detection;

fn main() {
    // Spawn background detection (queries the Docker/Podman daemons).
    let handle = start_detection();

    // ... do other work while detection runs ...

    // Collect the results (waits at most 3 seconds after the start).
    let port_map = handle.wait();

    for ((ip, port, proto), info) in &port_map {
        let host = ip.map_or_else(|| "*".to_string(), |ip| ip.to_string());
        println!(
            "{proto} {host}:{port} -> container '{}' (image: {})",
            info.name, info.image
        );
    }
}
```

Use `handle.wait_result()` instead of `handle.wait()` to learn why the map is empty.

**Strict path** (synchronous, returns errors):

```rust,no_run
use nanodock::{detect_containers, Error};

fn main() {
    match detect_containers() {
        Ok(port_map) => {
            for ((_, port, proto), info) in &port_map {
                let project = info.compose_project.as_deref().unwrap_or("-");
                println!("{proto} port {port} -> '{}' (compose project: {project})", info.name);
            }
        }
        Err(Error::PermissionDenied { endpoint, .. }) => {
            eprintln!("no permission to use {endpoint}; is this user in the docker group?");
        }
        Err(Error::DaemonNotFound) => eprintln!("no Docker or Podman daemon is running"),
        Err(e) => eprintln!("detection failed: {e}"),
    }
}
```

### Configure the client

The free functions use `Client::new()`, which reads the `DOCKER_HOST` override and the home directory (used on Unix to find per-user sockets such as Docker Desktop's, Colima's, or OrbStack's) from the environment, with a 3 second detection timeout. Build a `Client` to change any of these settings.

```rust,no_run
use std::time::Duration;
use nanodock::Client;

fn main() {
    let client = Client::new()
        .timeout(Duration::from_secs(1))
        // Ignore DOCKER_HOST and query only the default sockets or pipes.
        .docker_host(None);

    match client.detect() {
        Ok(port_map) => println!("{} published ports", port_map.len()),
        Err(e) => eprintln!("detection failed: {e}"),
    }
}
```

### Look up which container owns a socket

```rust,no_run
use std::net::{IpAddr, Ipv4Addr};
use nanodock::{start_detection, Protocol, ProxyFallback, PublishedContainerMatch};

fn main() {
    let port_map = start_detection().wait();

    let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    // `ProxyFallback::Allow` is for proxy processes such as docker-proxy,
    // whose socket address may differ from the published host IP.
    match port_map.lookup(ip, 5432, Protocol::Tcp, ProxyFallback::Deny) {
        PublishedContainerMatch::Match(info) => {
            println!("Port 5432 belongs to '{}' ({})", info.name, info.image);
        }
        PublishedContainerMatch::NotFound => {
            println!("No container found for port 5432");
        }
        PublishedContainerMatch::Ambiguous => {
            println!("Multiple containers match port 5432");
        }
        _ => {}
    }
}
```

### Stop or kill a container

`stop_container` asks the daemon to stop the container gracefully (SIGTERM, then SIGKILL after 10 seconds), and `kill_container` kills it at once. Both return a `StopOutcome`.

```rust,no_run
use nanodock::{stop_container, StopOutcome};

fn main() {
    let container_id = "abc123def456";
    match stop_container(container_id) {
        StopOutcome::Stopped => println!("Container stopped"),
        StopOutcome::AlreadyStopped => println!("Container was already stopped"),
        StopOutcome::NotFound => println!("Container not found"),
        StopOutcome::Unreachable => println!("No daemon could be reached"),
        // The daemon received the request but gave no usable reply.
        StopOutcome::NoResponse => println!("The result of the stop is unknown"),
        StopOutcome::Rejected { status } => println!("The daemon answered HTTP {status}"),
        _ => println!("Unexpected outcome"),
    }
}
```

## How It Works

nanodock communicates directly with the Docker/Podman daemon using the `/containers/json` REST API endpoint over local transports:

```text
┌─────────────┐     HTTP/1.0 GET /containers/json
│  nanodock   │ ──────────────────────────────────────┐
│ (your app)  │                                       │
└─────────────┘                                       ▼
                                              ┌─────────────────┐
    Unix socket  (/var/run/docker.sock)  ───► │                 │
    Named pipe   (\\.\pipe\docker_engine) ──► │  Docker/Podman  │
    TCP          (DOCKER_HOST=tcp://...) ───► │  Daemon         │
                                              │                 │
                                              └─────────────────┘
```

### Transport Discovery Order

1. **`DOCKER_HOST` environment variable** (or `Client::docker_host`) - If set, the specified daemon is preferred. A `tcp://` daemon is queried at the same time as the platform-native sockets and is used on its own when it answers, so a stale address cannot hide a local daemon. A `unix://` path replaces the default Unix sockets. An `npipe://` pipe is queried alongside the default pipes and is used on its own when it answers. Stop and kill requests try the same daemons in the same order (`DOCKER_HOST` first). Before sending the stop or kill to a daemon it checks that the daemon answers `GET /_ping` with a 2xx status on a separate connection, and moves on to the next daemon when it cannot be reached or does not answer the ping that way (for example a forwarder whose backend is down, or a TLS port that answers plain HTTP with 400). Any reply from the `DOCKER_HOST` daemon, including "not found", is final; a "not found" from a default daemon moves on to the next one. Once a daemon has received the stop request, no reply (a closed connection, a timeout, or a partial reply) is reported as `StopOutcome::NoResponse` and no other daemon is tried.

   `DOCKER_HOST` is read like the Docker CLI reads it: surrounding whitespace and the letter case of the scheme are ignored, `tcp://host` uses port 2375, an IPv6 address goes in brackets (`tcp://[::1]:2375` or `tcp://[::1]`), and a trailing slash or path after the address is ignored. An `npipe://` value must name a pipe, `//./pipe/<name>` or `//<host>/pipe/<name>` (either slash direction); anything else, such as `npipe://C:/x.txt`, is ignored rather than opened as a file. The `ssh://`, `fd://`, and `http://` schemes, a value without a scheme, and a malformed address are not supported: they are logged at debug level and only the default endpoints are used.
2. **Platform-native sockets** - On Linux and macOS, the well-known Unix socket paths below are checked (rootful and rootless Docker, Podman, Docker Desktop, Colima, OrbStack, Rancher Desktop, Lima, and Podman machine). Paths that do not exist are skipped before any connection is attempted, and paths that resolve to the same file (for example `/var/run/docker.sock` symlinked to another runtime's socket) are queried once, at the first position. On Windows, the named pipes for Docker Desktop and Podman Machine are queried. All endpoints are queried concurrently under one shared time budget, and the containers of every daemon that answers are merged; when two daemons report the same port, the one earlier in this list wins.
3. **Rootless Podman overlay** (Linux only) - For containers managed by rootless Podman, nanodock reads the overlay storage metadata to resolve container names from network namespace paths. This handles the case where `rootlessport` is the process holding the socket instead of the container itself.

### Supported Daemon Paths

Unix sockets in priority order. `$XDG_RUNTIME_DIR` and `$TMPDIR` are used only when set to an absolute path, and a `$XDG_RUNTIME_DIR` entry equal to its `/run/user/{uid}` fallback is listed once. `$TMPDIR` is searched on macOS only, where it is a private per-user directory; on Linux it is usually the shared `/tmp`. A default socket whose file is owned by neither the current user nor root is skipped, so a socket another local user planted at one of these paths is never queried or sent a stop request. A `DOCKER_HOST=unix://` path is the user's explicit choice and is not checked.

| Platform | Transport   | Path                                                                              | Runtime                                 |
| -------- | ----------- | --------------------------------------------------------------------------------- | --------------------------------------- |
| Unix     | Unix socket | `/var/run/docker.sock`                                                            | Rootful Docker (or a runtime's symlink) |
| Unix     | Unix socket | `$XDG_RUNTIME_DIR/docker.sock`                                                    | Rootless Docker                         |
| Unix     | Unix socket | `/run/user/{uid}/docker.sock`                                                     | Rootless Docker                         |
| Unix     | Unix socket | `$XDG_RUNTIME_DIR/podman/podman.sock`                                             | Rootless Podman                         |
| Unix     | Unix socket | `/run/user/{uid}/podman/podman.sock`                                              | Rootless Podman                         |
| Unix     | Unix socket | `/run/podman/podman.sock`                                                         | Rootful Podman                          |
| Unix     | Unix socket | `$HOME/.docker/desktop/docker.sock`                                               | Docker Desktop for Linux                |
| Unix     | Unix socket | `$HOME/.docker/run/docker.sock`                                                   | Docker Desktop for macOS                |
| Unix     | Unix socket | `$HOME/.colima/default/docker.sock`                                               | Colima (0.4 and later)                  |
| Unix     | Unix socket | `$HOME/.colima/docker.sock`                                                       | Colima (before 0.4)                     |
| Unix     | Unix socket | `$HOME/.orbstack/run/docker.sock`                                                 | OrbStack                                |
| Unix     | Unix socket | `$HOME/.rd/docker.sock`                                                           | Rancher Desktop (moby)                  |
| Unix     | Unix socket | `$HOME/.lima/default/sock/docker.sock`                                            | Lima `default` instance                 |
| Unix     | Unix socket | `$HOME/.lima/docker/sock/docker.sock`                                             | Lima `template://docker` instance       |
| Unix     | Unix socket | `$HOME/.local/share/containers/podman/machine/podman.sock`                        | Podman machine (macOS, Podman 4)        |
| Unix     | Unix socket | `$HOME/.local/share/containers/podman/machine/qemu/podman.sock`                   | Podman machine (macOS, Podman 4)        |
| Unix     | Unix socket | `$HOME/.local/share/containers/podman/machine/podman-machine-default/podman.sock` | Podman machine (macOS, Podman 4)        |
| macOS    | Unix socket | `$TMPDIR/podman/podman-machine-default-api.sock`                                  | Podman machine (macOS, Podman 5)        |
| Windows  | Named pipe  | `\\.\pipe\docker_engine`                                                          | Docker Desktop                          |
| Windows  | Named pipe  | `\\.\pipe\podman-machine-default`                                                 | Podman machine                          |
| Both     | TCP         | `DOCKER_HOST=tcp://host:port`                                                     | Any                                     |

## API Reference

Full API documentation is available on [docs.rs](https://docs.rs/nanodock).

### Core Types

| Type                      | Description                                                                         |
| ------------------------- | ----------------------------------------------------------------------------------- |
| `Client`                  | Daemon settings (home, timeout, `DOCKER_HOST`) with `detect`, `start_detection`, `stop`, `kill` |
| `ContainerInfo`           | Container metadata: id, name, image, Compose project and service                    |
| `ContainerPortMap`        | Map from `(host_ip, port, protocol)` to a shared `ContainerInfo`                    |
| `PortMapIter`             | Iterator over the bindings of a `ContainerPortMap`                                  |
| `ProxyFallback`           | Whether a lookup may match a proxy process on port and protocol alone               |
| `PublishedContainerMatch` | Result of looking up a socket address in the port map                               |
| `DetectionHandle`         | Handle for an in-progress background detection                                      |
| `StopOutcome`             | Result of a stop or kill request                                                    |
| `Error`                   | Why detection failed                                                                |
| `ParseError`              | Opaque error for a reply that is not valid HTTP or container-list JSON              |
| `Protocol`                | Network protocol (`Tcp`, `Udp`)                                                     |

### Core Functions and Methods

| Item                                              | Description                                                |
| ------------------------------------------------- | ---------------------------------------------------------- |
| `Client::new()`                                   | Client configured from `DOCKER_HOST` and the home directory |
| `.home(home)`, `.timeout(t)`, `.docker_host(h)`   | Chainable settings                                         |
| `Client::detect()`                                | Synchronous detection, returns `Result<ContainerPortMap, Error>` |
| `Client::start_detection()`                       | Spawn a background detection, returns a `DetectionHandle`  |
| `Client::stop(id)`                                | Stop a container by ID or name (SIGTERM, 10 second grace)  |
| `Client::kill(id)`                                | Kill a container by ID or name (immediate SIGKILL)         |
| `DetectionHandle::wait()`                         | Wait for the result (empty map on failure or timeout)      |
| `DetectionHandle::wait_result()`                  | Wait for the result, keeping the `Error`                   |
| `ContainerPortMap::lookup(ip, port, proto, fallback)` | Match a socket address against the published ports     |
| `ContainerPortMap::get(host_ip, port, proto)`     | Exact binding lookup                                       |
| `PublishedContainerMatch::container()`            | The matched `&ContainerInfo`, if any                       |
| `PublishedContainerMatch::container_arc()`        | The matched `&Arc<ContainerInfo>`, to keep it without a copy |
| `detect_containers()`                             | Shorthand for `Client::new().detect()`                     |
| `start_detection()`                               | Shorthand for `Client::new().start_detection()`            |
| `stop_container(id)`                              | Shorthand for `Client::new().stop(id)`                     |
| `kill_container(id)`                              | Shorthand for `Client::new().kill(id)`                     |
| `parse_containers_json(body)`                     | Lenient parse of a raw `/containers/json` response         |
| `parse_containers_json_strict(body)`              | Strict parse that fails with `ParseError` on invalid JSON  |
| `short_container_id(id)`                          | The 12-character short form of a container ID              |
| `is_container_proxy_process(name)`                | Whether a process is a container runtime port proxy        |

`is_container_proxy_process` recognizes the host-side helpers that listen on a published port on behalf of a container, so a port scanner can attribute the port to the container instead of the helper: `docker-proxy`, `rootlesskit`, `rootlessport`, `rootlessport-child`, `slirp4netns`, `pasta`, `pasta.avx2`, `com.docker.backend`, `com.docker.vpnkit`, `vpnkit`, `wslrelay`, `gvproxy`, and `limactl`. Matching is ASCII case-insensitive, ignores a trailing `.exe`, and accepts the 15-byte names Linux and the 16-byte names macOS truncate longer process names to (for example `rootlessport-ch` and `com.docker.backe`). Generic forwarders such as `ssh` and `socat` are not recognized.

### Cargo Features

| Feature | Default | Description                                                                                    |
| ------- | ------- | ---------------------------------------------------------------------------------------------- |
| `serde` | Off     | Derives `Serialize` and `Deserialize` for `ContainerInfo`, `Protocol`, `StopOutcome`, `ProxyFallback` |

### Rootless Podman Helpers

These exist on every platform with the same signature, so callers need no `cfg` gates. Only Linux runs `rootlessport` on the host, so on other platforms `lookup_rootless_podman_container` always returns `None` and the resolver stays empty.

| Item                                   | Description                               |
| -------------------------------------- | ----------------------------------------- |
| `is_podman_rootlessport_process(name)` | Check if a process name is `rootlessport` |
| `lookup_rootless_podman_container()`   | Resolve container from rootlessport PIDs  |
| `RootlessPodmanResolver`               | Cached resolver for rootless Podman       |

## Architecture

```text
src/
├── lib.rs      - Public API, detection orchestration, port matching
├── api.rs      - JSON response parsing, container name resolution
├── http.rs     - Minimal HTTP/1.0 response parser (via httparse)
├── ipc.rs      - OS-specific transport (Unix socket, named pipe, TCP)
├── podman.rs   - Rootless Podman resolver via overlay metadata
└── proxy.rs    - Container runtime port-proxy process recognition
```

### Module Boundaries

- **`lib.rs`** owns the public API surface, detection orchestration, and port-to-container matching logic. All public types are defined here.
- **`api.rs`** owns JSON response parsing. It converts raw daemon responses into `ContainerPortMap` entries.
- **`http.rs`** owns HTTP protocol handling. It formats requests and parses responses using `httparse`. No Docker-specific logic lives here.
- **`ipc.rs`** owns OS-specific transport code. Unix sockets, Windows named pipes, TCP connections, and `DOCKER_HOST` parsing all live here.
- **`podman.rs`** owns rootless Podman resolution. It reads overlay storage metadata and OCI runtime configs to match network namespace paths to container names. Its public items compile on every platform; the lookup only runs on Linux.
- **`proxy.rs`** owns the list of container runtime port-proxy process names behind `is_container_proxy_process`.

## Building

```bash
# Debug build
cargo build

# Run tests
cargo test --lib --tests
cargo test --doc

# Compile benchmarks
cargo bench --no-run

# Run benchmarks on Linux with valgrind and gungraun-runner installed
cargo bench --bench benchmarks

# Check formatting
cargo fmt --check

# Run clippy (all+pedantic+nursery at deny level)
cargo clippy --all-targets -- -D warnings

# Build documentation
cargo doc --no-deps --open

# Dependency audit (requires cargo-deny)
cargo deny check
```

## Quality Gates

All of the following must pass before merging:

| Gate | Command                                        | Purpose                   |
| ---- | ---------------------------------------------- | ------------------------- |
| 1    | `cargo fmt --check`                            | Consistent formatting     |
| 2    | `cargo clippy`                                 | Zero lint warnings        |
| 3    | `cargo test --lib --tests && cargo test --doc` | All tests pass            |
| 4    | `cargo bench --no-run`                         | Benchmarks compile        |
| 5    | `cargo build`                                  | Library compiles          |
| 6    | `cargo doc --no-deps`                          | Documentation builds      |
| 7    | `cargo deny check`                             | No vulnerable/banned deps |

## Instruction Benchmarks

nanodock ships deterministic instruction-count benchmarks via [Gungraun](https://crates.io/crates/gungraun) (requires Linux + Valgrind). See [CONTRIBUTING.md](docs/CONTRIBUTING.md) for setup and usage details.

### Git Hooks

Install local quality gates (runs fmt, clippy, and tests before each commit):

**Windows (PowerShell):**
```powershell
.\scripts\install-hooks.ps1
```

**Linux / macOS:**
```bash
bash scripts/install-hooks.sh
```

## Minimum Supported Rust Version

nanodock requires Rust 1.89 or newer (declared as `rust-version` in `Cargo.toml`). It uses edition 2024, and its public `const fn` API relies on `str::eq_ignore_ascii_case` being usable in const context, which was stabilized in 1.89.

## Dependencies

nanodock keeps its dependency tree intentionally small:

| Crate        | Purpose                                |
| ------------ | -------------------------------------- |
| `serde`      | JSON response parsing (and the optional `serde` derives) |
| `serde_json` | JSON response parsing                  |
| `httparse`   | HTTP/1.x response header parsing       |
| `log`        | Debug diagnostics via log facade       |
| `libc`       | Unix-only: `getuid()` for socket paths |

No async runtime. No TLS. No network client libraries.

## Contributing

See [CONTRIBUTING.md](docs/CONTRIBUTING.md) for development setup, coding standards, commit message format, and the full quality gate reference.

## License

Licensed under the [MIT License](LICENSE).

Copyright (c) 2026 Ehsan Khan
