# Contributing to nanodock

Thank you for your interest in contributing!

---

## Development Setup

### Prerequisites

- Rust stable toolchain (1.93+)
- `cargo-deny` (optional, for dependency audit)
- Supported lint targets:

```bash
rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-msvc
```

### Clone and Build

```bash
git clone https://github.com/ehsan18t/nanodock.git
cd nanodock
cargo build
```

### Install Git Hooks

**Windows (PowerShell):**

```powershell
.\scripts\install-hooks.ps1
```

**Linux / macOS:**

```bash
bash scripts/install-hooks.sh
```

Both scripts install pre-commit, pre-push, and commit-msg hooks that enforce quality gates locally before CI.

The installers resolve Git's real hooks directory through Git metadata, so they work from normal clones and linked worktrees instead of assuming `.git/hooks` is always a plain directory under the working tree.

The Clippy gate uses `scripts/check-platform-clippy.sh` on shell-based setups and `scripts/check-platform-clippy.ps1` on Windows PowerShell. The host target still runs with `--all-targets`, while the other supported target lints `--lib` so Linux-only and Windows-only cfg issues fail locally without requiring a foreign C toolchain.

---

## Quality Gates

This is the one list of the quality gates; the hooks, the CI workflow, and the agent instructions refer to it. All of them must pass before merging. The pre-commit hook runs gates 1 to 3 and the pre-push hook runs all seven. CI runs gates 1 to 6 in the quality gate job on Linux, Windows, and macOS (clippy natively on each, with and without `--all-features`) and gate 7 in the audit job.

| Gate | Command                                                                                | Purpose                                             |
| ---- | -------------------------------------------------------------------------------------- | --------------------------------------------------- |
| 1    | `cargo fmt --all -- --check`                                                           | Consistent formatting                               |
| 2    | `scripts/check-platform-clippy.sh` / `scripts/check-platform-clippy.ps1`               | Zero lint warnings across Linux + Windows cfg paths |
| 3    | `cargo test --locked --lib --tests --all-features && cargo test --locked --doc --all-features` | Unit, integration, property, and doc tests pass |
| 4    | `cargo bench --locked --no-run`                                                        | Benchmarks compile                                  |
| 5    | `cargo build --locked`                                                                 | Library compiles                                    |
| 6    | `cargo doc --locked --no-deps --all-features` with the CI `RUSTDOCFLAGS`              | Documentation builds without warnings               |
| 7    | `cargo deny check`                                                                     | No vulnerable, banned, or unlicensed dependencies   |

CI runs on every push to `main` **and** on every pull request targeting `main`, so cross-platform issues (Linux + Windows + macOS matrix) are caught before a PR is merged.

The crate has one optional feature, `serde`, which derives `Serialize` and `Deserialize` on the public data types. The local hooks run the tests and docs with `--all-features`, as CI does, and every hook cargo command uses `--locked`. The cross-target clippy script checks the default features; CI also runs clippy with `--all-features`, so before pushing a change that touches a `cfg_attr(feature = "serde", ...)` attribute, run `cargo clippy --all-targets --all-features -- -D warnings` too.

A separate MSRV job runs `cargo check --locked --all-targets --all-features` on Rust 1.89, the `rust-version` declared in `Cargo.toml`. Development and the lint gates use the latest stable toolchain, but code must keep compiling on 1.89; raising the MSRV is a deliberate change that updates `rust-version`, this job, and the README together.

A package job runs `cargo package --locked --list` and `cargo publish --locked --dry-run`, which builds the packaged crate from only the files Cargo.toml `include` ships. If you add a file the crate needs at build time, add it to `include` too.

Two Linux jobs run the tests that need a live container runtime. `docker-it` starts containers on the runner's Docker Engine and runs `tests/daemon_it.rs` over the Unix socket and then over a TCP `DOCKER_HOST` (a socat forwarder, with the local socket made root-only so the test can only succeed over TCP). `podman-it` installs Podman, enables the rootless API socket, and runs `tests/podman_it.rs`: one detection merging a Podman and a Docker container, the `rootlessport` resolver, and a stop that falls through Docker's 404 to Podman. Container names and ports come from `NANODOCK_IT_*` variables in each job's `env`, which the tests read with the same defaults.

Workflow dependencies in `.github/workflows/` are pinned to full commit SHAs. When updating an action, keep the trailing version comment (for example `# v6`) so reviewers can see the intended upstream release at a glance.

For environment-specific diagnostics while developing, enable the `log` crate at debug level to see Docker/Podman probing and transport fallback messages.

Pull requests also run a Linux benchmark regression job with Gungraun. CI saves a merge-base baseline from `main`, runs the PR head against that baseline, and fails the job when the instruction count (`Ir`) regresses beyond the configured limit. CI uploads a `benchmark-reports-<sha>` artifact that contains the raw console log plus the generated `target/gungraun/` report tree.

Because Gungraun executes through Valgrind, actual benchmark execution is Linux-only. Windows contributors can still compile the benchmark harness with `cargo bench --no-run`, but they cannot run the benchmark suite locally on Windows.

To run the instruction benchmarks locally on Linux:

```bash
sudo apt-get install valgrind
cargo install --version 0.18.2 gungraun-runner
cargo bench --bench benchmarks
```

To compare against a named baseline and fail on instruction regressions:

```bash
cargo bench --bench benchmarks -- --save-baseline=main --callgrind-metrics=ir
cargo bench --bench benchmarks -- --baseline=main --callgrind-metrics=ir --callgrind-limits='ir=1.0%'
```

---

## Project Structure

```text
src/
  lib.rs      - Crate docs, module declarations, public re-exports, free functions
  error.rs    - Error and ParseError
  port_map.rs - Protocol, ContainerInfo, ContainerPortMap, port matching
  client.rs   - Client, DetectionHandle, endpoint selection, detection queries
  stop.rs     - StopOutcome and the stop and kill logic
  api.rs      - JSON response parsing, container name resolution
  http.rs     - Minimal HTTP/1.0 response parser (via httparse)
  ipc.rs      - OS-specific transport (Unix socket, named pipe, TCP)
  podman.rs   - Rootless Podman resolver via overlay metadata (lookup runs on Linux only)
  proxy.rs    - Container runtime port-proxy process recognition
```

### Architecture Boundaries

- **`lib.rs`** holds the crate documentation, the module declarations, the re-exports that make up the public API, and the free functions (`detect_containers`, `start_detection`, `stop_container`, `kill_container`). Public types are defined in their modules and re-exported here, so their paths stay `nanodock::<Type>`.
- **`error.rs`** owns `Error` and `ParseError`, and picks the most informative failure when every endpoint fails.
- **`port_map.rs`** owns `Protocol`, `ContainerInfo`, `PortKey`, `ContainerPortMap` and its iterator, `ProxyFallback`, and `PublishedContainerMatch`: the port-to-container matching logic.
- **`client.rs`** owns `Client` and `DetectionHandle`: endpoint selection, the concurrent detection query, and merging the daemons' replies.
- **`stop.rs`** owns `StopOutcome` and the stop and kill logic: container ID validation, the request endpoint, and choosing the daemon that owns the container.
- **`api.rs`** owns JSON response parsing. It converts raw daemon responses into `ContainerPortMap` entries.
- **`http.rs`** owns HTTP protocol handling. It formats requests and parses responses using `httparse`. No Docker-specific logic lives here.
- **`ipc.rs`** owns OS-specific transport code. Unix sockets, Windows named pipes, TCP connections, and `DOCKER_HOST` parsing all live here.
- **`podman.rs`** owns rootless Podman resolution. It reads overlay storage metadata and OCI runtime configs to match network namespace paths to container names. Its public items compile on every platform; the lookup only runs on Linux.
- **`proxy.rs`** owns the list of container runtime port-proxy process names behind `is_container_proxy_process`.

---

## Coding Standards

- **Clippy:** `all + pedantic + nursery` at deny level, plus `unwrap_used` and `undocumented_unsafe_blocks`
- **Error handling:** public fallible functions return the crate's own `Error` (or `ParseError`), never a third-party error type; the best-effort detection path returns an empty map instead of an error
- **No `unwrap()`** outside of tests, and every `unsafe` block has a `// SAFETY:` comment
- **Doc comments** on every public item
- **Functions <= 100 lines**, cognitive complexity <= 30
- **No `dbg!()`, `todo!()`, `unimplemented!()`, or `std::process::abort`**

---

## Commit Messages

Follow [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<scope>): <description>
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`, `enforce`.

Rules:
- Description starts lowercase, 5-200 characters
- No trailing period
- Scope is optional, lowercase, alphanumeric + hyphens
- No `!` breaking-change marker; the commit-msg hook rejects it

Good examples:
```
feat(api): parse container labels from daemon response
fix(ipc): handle named pipe timeout on Windows
perf(http): avoid redundant string allocation in chunk parser
docs: update README with rootless Podman section
```

---

## Testing

- Unit tests live in `#[cfg(test)] mod tests` inside each module
- Use `assert_eq!` with descriptive messages
- Integration tests in `tests/` use only the public API
- `tests/proptest_parsers.rs` holds the property tests: the JSON parsers never panic and agree with each other, and the port map, `is_container_proxy_process`, and `short_container_id` keep their documented contracts. Keep case counts moderate so the file runs in seconds.
- Tests that need a live daemon return early unless their opt-in variable is set, so they pass as no-ops everywhere else. `tests/daemon_it.rs` runs with `NANODOCK_IT=1` (Docker) and its `tcp_` tests with `NANODOCK_IT_TCP_HOST=tcp://host:port`; `tests/podman_it.rs` runs with `NANODOCK_IT_PODMAN=1` (rootless Podman, Linux). Each file's header lists the containers it expects; run them with `--test-threads=1`.

```bash
# A plain `cargo test` would also try to run the Gungraun benchmarks.
cargo test --lib --tests --all-features
cargo test --doc --all-features

# Against a local Docker daemon with the CI containers started:
NANODOCK_IT=1 cargo test --test daemon_it -- --test-threads=1
```

---

## Dependency Policy

- Prefer `std` over external crates
- Only licenses on the `deny.toml` allowlist (currently MIT, Apache-2.0, and Unicode-3.0, alone or as one option of an `OR` expression)
- `cargo deny check` must pass
- Do not add new dependencies without maintainer approval
