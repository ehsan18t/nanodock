<div align="center">
    <h1>nanodock</h1>
    <p>A lightweight zero-bloat Rust library for detecting Docker and Podman containers, mapping their published ports to host sockets, and controlling container lifecycle. Built for embedding into CLI tools and system utilities that need container awareness without pulling in a full Docker SDK.</p>

[![CI](https://github.com/ehsan18t/nanodock/actions/workflows/ci.yml/badge.svg)](https://github.com/ehsan18t/nanodock/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/nanodock.svg)](https://crates.io/crates/nanodock)
[![docs.rs](https://docs.rs/nanodock/badge.svg)](https://docs.rs/nanodock)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

</div>


## Features

- **Container detection** - Queries Docker and Podman daemons to discover
  running containers and their published port bindings.
- **Port-to-container mapping** - Resolves which container owns a given host
  `(ip, port, protocol)` tuple, with wildcard and proxy-fallback matching.
- **Container lifecycle control** - Stop or kill containers by ID through the
  daemon API (graceful SIGTERM or immediate SIGKILL).
- **Multi-transport support** - Connects via Unix domain sockets, Windows named
  pipes, or TCP (`DOCKER_HOST`), with automatic discovery of socket paths.
- **Rootless Podman support** - Resolves rootless Podman containers on Linux by
  reading overlay storage metadata and matching network namespace paths.
- **Background detection** - Spawns detection on a background thread so callers
  can do other work (socket enumeration, process lookup) concurrently.
- **Minimal dependencies** - Only `serde`, `serde_json`, `httparse`, and `log`
  at runtime. No async runtime, no `tokio`, no `hyper`.
- **Cross-platform** - Works on Linux (x86-64) and Windows (x86-64).

## Quick Start

Add nanodock to your `Cargo.toml`:

```toml
[dependencies]
nanodock = "0.1"
```

### Detect containers and map ports

Two detection paths are available:

**Best-effort path** (background thread, never errors):

```rust,no_run
use nanodock::{start_detection, await_detection};

fn main() {
    // Spawn background detection (queries Docker/Podman daemon).
    let handle = start_detection(None);

    // ... do other work while detection runs ...

    // Collect results (blocks up to 3 seconds).
    let port_map = await_detection(handle);

    for ((ip, port, proto), info) in &port_map {
        println!(
            "{proto} port {port} -> container '{}' (image: {})",
            info.name, info.image
        );
    }
}
```

**Strict path** (synchronous, returns errors):

```rust,no_run
use nanodock::detect_containers;

fn main() {
    match detect_containers(None) {
        Ok(port_map) => {
            for ((ip, port, proto), info) in &port_map {
                println!(
                    "{proto} port {port} -> container '{}' (image: {})",
                    info.name, info.image
                );
            }
        }
        Err(e) => eprintln!("detection failed: {e}"),
    }
}
```

### Look up which container owns a socket

```rust,no_run
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use nanodock::{
    start_detection, await_detection,
    lookup_published_container, PublishedContainerMatch, Protocol,
};

fn main() {
    let port_map = await_detection(start_detection(None));

    let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5432);
    match lookup_published_container(&port_map, socket, Protocol::Tcp, false) {
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

### Stop a container

```rust,no_run
use nanodock::{stop_container, StopOutcome};

fn main() {
    let container_id = "abc123def456";
    match stop_container(container_id, false, None) {
        StopOutcome::Stopped => println!("Container stopped"),
        StopOutcome::AlreadyStopped => println!("Container was already stopped"),
        StopOutcome::NotFound => println!("Container not found"),
        StopOutcome::Failed => println!("Could not reach daemon"),
        _ => println!("Unexpected outcome"),
    }
}
```

## How It Works

nanodock communicates directly with the Docker/Podman daemon using the
`/containers/json` REST API endpoint over local transports:

```
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

1. **`DOCKER_HOST` environment variable** - If set, the specified transport
   (tcp://, unix://, npipe://) is used first.
2. **Platform-native sockets** - On Linux, well-known Unix socket paths are
   probed (rootful Docker, rootless Docker, Podman). On Windows, named pipes
   for Docker Desktop and Podman Machine are tried.
3. **Rootless Podman overlay** (Linux only) - For containers managed by rootless
   Podman, nanodock reads the overlay storage metadata to resolve container
   names from network namespace paths. This handles the case where
   `rootlessport` is the process holding the socket instead of the container
   itself.

### Supported Daemon Paths

| Platform | Transport   | Path                                 |
| -------- | ----------- | ------------------------------------ |
| Linux    | Unix socket | `/var/run/docker.sock`               |
| Linux    | Unix socket | `/run/user/{uid}/docker.sock`        |
| Linux    | Unix socket | `$HOME/.docker/desktop/docker.sock`  |
| Linux    | Unix socket | `$HOME/.docker/run/docker.sock`      |
| Linux    | Unix socket | `/run/user/{uid}/podman/podman.sock` |
| Linux    | Unix socket | `/run/podman/podman.sock`            |
| Windows  | Named pipe  | `\\.\pipe\docker_engine`             |
| Windows  | Named pipe  | `\\.\pipe\podman-machine-default`    |
| Both     | TCP         | `DOCKER_HOST=tcp://host:port`        |

## API Reference

Full API documentation is available on [docs.rs](https://docs.rs/nanodock).

### Core Types

| Type                      | Description                                               |
| ------------------------- | --------------------------------------------------------- |
| `Protocol`                | Network protocol enum (`Tcp`, `Udp`)                      |
| `ContainerInfo`           | Container metadata (id, name, image)                      |
| `ContainerPortMap`        | HashMap mapping `(ip, port, protocol)` to `ContainerInfo` |
| `PublishedContainerMatch` | Result of looking up a socket in the port map             |
| `StopOutcome`             | Result of a stop/kill request                             |
| `DetectionHandle`         | Handle for in-progress background detection               |
| `Error`                   | Error type for strict-path detection failures             |

### Core Functions

| Function                          | Description                                         |
| --------------------------------- | --------------------------------------------------- |
| `detect_containers(home)`         | Synchronous detection, returns `Result<Map, Error>` |
| `start_detection(home)`           | Spawn background daemon query, returns handle       |
| `await_detection(handle)`         | Block for results (3s timeout), returns map         |
| `lookup_published_container()`    | Match a socket against the port map                 |
| `stop_container(id, force, home)` | Stop or kill a container by ID                      |
| `parse_containers_json(body)`     | Parse raw `/containers/json` response               |
| `parse_containers_json_strict()`  | Strict parse that returns `Result` on invalid JSON  |

### Linux-only Functions

| Function                               | Description                               |
| -------------------------------------- | ----------------------------------------- |
| `is_podman_rootlessport_process(name)` | Check if a process name is `rootlessport` |
| `lookup_rootless_podman_container()`   | Resolve container from rootlessport PIDs  |
| `RootlessPodmanResolver`               | Cached resolver for rootless Podman       |

## Architecture

```
src/
├── lib.rs      — Public API, detection orchestration, port matching
├── api.rs      — JSON response parsing, container name resolution
├── http.rs     — Minimal HTTP/1.0 response parser (via httparse)
├── ipc.rs      — OS-specific transport (Unix socket, named pipe, TCP)
└── podman.rs   — Rootless Podman resolver via overlay metadata (Linux)
```

### Module Boundaries

- **`lib.rs`** owns the public API surface, detection orchestration, and
  port-to-container matching logic. All public types are defined here.
- **`api.rs`** owns JSON response parsing. It converts raw daemon responses
  into `ContainerPortMap` entries.
- **`http.rs`** owns HTTP protocol handling. It formats requests and parses
  responses using `httparse`. No Docker-specific logic lives here.
- **`ipc.rs`** owns OS-specific transport code. Unix sockets, Windows named
  pipes, TCP connections, and `DOCKER_HOST` parsing all live here.
- **`podman.rs`** owns rootless Podman resolution. It reads overlay storage
  metadata and OCI runtime configs to match network namespace paths to
  container names.

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

nanodock ships a Gungraun benchmark suite for the two hot paths most likely to
regress in real use: parsing daemon `/containers/json` payloads and matching
host sockets back to published container bindings.

Unlike Criterion, Gungraun measures instruction counts and related Callgrind
metrics instead of wall-clock time. The benchmark output is written under
`target/gungraun/`.

Important constraints from the upstream Gungraun docs:

- benchmark execution requires Linux plus Valgrind
- benchmark execution also requires a version-matched `gungraun-runner` binary
- Windows can compile the benchmark harness with `cargo bench --no-run`, but it
  cannot execute the benchmarks

To install the benchmark runtime on Linux:

```bash
sudo apt-get install valgrind
cargo install --version 0.18.1 gungraun-runner
```

To create and compare a named baseline locally:

```bash
cargo bench --bench benchmarks -- --save-baseline=main --callgrind-metrics=ir
cargo bench --bench benchmarks -- --baseline=main --callgrind-metrics=ir --callgrind-limits='ir=1.0%'
```

Pull requests run a Linux benchmark job that:

- saves a merge-base baseline from `main`
- compares the PR head against that baseline using instruction deltas (`Ir`)
- fails the job if any benchmark regresses by more than 1%
- uploads raw console output plus the generated `target/gungraun/` reports as artifacts

If you update the `gungraun` crate version, update the installed
`gungraun-runner` version in CI and local setup to match.

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

nanodock requires the latest stable Rust toolchain (currently 1.93+) and uses
edition 2024 features.

## Dependencies

nanodock keeps its dependency tree intentionally small:

| Crate        | Purpose                                |
| ------------ | -------------------------------------- |
| `serde`      | Container metadata serialization       |
| `serde_json` | JSON response parsing                  |
| `httparse`   | HTTP/1.x response header parsing       |
| `log`        | Debug diagnostics via log facade       |
| `libc`       | Unix-only: `getuid()` for socket paths |

No async runtime. No TLS. No network client libraries.

## Contributing

See [CONTRIBUTING.md](docs/CONTRIBUTING.md) for development setup, coding
standards, commit message format, and the full quality gate reference.

## License

Licensed under the [MIT License](LICENSE).

Copyright (c) 2026 Ehsan Khan
