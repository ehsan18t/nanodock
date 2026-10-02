# AI Agent Instructions - nanodock

These rules bind GitHub Copilot, Claude, Cursor, and every other coding agent working in this repository. Treat each one as a hard constraint unless the human operator explicitly overrides it. Development setup, the quality gates, and the CI jobs are documented once, in [docs/CONTRIBUTING.md](../docs/CONTRIBUTING.md).

## Project Identity

| Field      | Value                                                  |
| ---------- | ------------------------------------------------------ |
| Language   | Rust, edition 2024                                     |
| Type       | Library crate, published on crates.io                  |
| Platforms  | Linux x86-64 and Windows x86-64 (macOS tested in CI)   |
| MSRV       | Rust 1.89 (`rust-version`, enforced by the CI MSRV job) |
| License    | MIT                                                    |
| Repository | `https://github.com/ehsan18t/nanodock`                 |

nanodock is the synchronous, minimal-dependency Docker/Podman client for container detection, port mapping, and lifecycle control (stop and kill). It is the `minreq` of container libraries, not a full Docker API client: guard that scope and do not drift toward general Docker API coverage. It was extracted from [portlens](https://github.com/ehsan18t/portlens); when changing the API, ask whether a design is right for a standalone library or a leftover from the portlens era, and do not break portlens without coordinating both repositories.

## Coding Rules

1. Clippy `all + pedantic + nursery` is denied, plus `unwrap_used` and `undocumented_unsafe_blocks` (see `Cargo.toml` and `clippy.toml`). Never add `#[allow(...)]` without a neighbouring comment saying why.
2. No `unwrap()` or `expect()` outside tests. `dbg!`, `todo!`, `unimplemented!`, and `std::process::abort` are banned.
3. Every `unsafe` block carries a `// SAFETY:` comment that explains why it is sound.
4. Errors are layered. The best-effort path (`start_detection`, `DetectionHandle::wait`) returns an empty map on failure; `DetectionHandle::wait_result` and the strict path (`Client::detect`, `detect_containers`) return the crate's own `Error`, and when every endpoint fails the most informative failure wins.
5. Synchronous by design: no `async`, no async runtime, no TLS or HTTP client crates, no spawning the `docker` or `podman` CLIs. Talk to the daemon through `ipc.rs` and `http.rs` only.
6. Every public item has a `///` doc comment (`missing_docs` is denied).
7. Functions stay at or under 100 lines and cognitive complexity 30. Prefer early returns and well-named helpers.
8. Run `cargo fmt` before every commit (rustfmt edition 2024, `max_width = 100`).

## API Design

- Public types implement the common traits (`Debug`, `Clone`, `PartialEq`, `Eq`, `Hash` where they make sense) and `Display` when they have a human-readable form.
- Enums that may gain variants and structs that may gain fields are `#[non_exhaustive]`. `ContainerInfo` is built with `ContainerInfo::new` and the `with_*` methods; keep that pattern for new public structs.
- No third-party types in public signatures. `ParseError` wraps `serde_json` opaquely.
- Serde derives sit behind the `serde` feature: `#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]`.
- Settings (home directory, timeout, `DOCKER_HOST` override) live on `Client` as chainable setters. The free functions are thin shorthands over `Client::new()`; never add parameters to them.
- The detection timeout defaults to 3 seconds. The internal query budget (`query_budget`) must stay strictly shorter than it so the detection thread always delivers before the handle stops waiting.
- Versioning follows SemVer for 0.x: new public items, methods, and `#[non_exhaustive]` variants are minor; fixes and performance work are patch; removing or changing a public item is breaking.
- Do not add builders, traits, or abstractions until at least three independent parameters need coordinating.
- Before 1.0, decide whether the public `ContainerInfo` fields become accessor methods (C-STRUCT-PRIVATE).

## Architecture

| Module        | Owns                                                                        |
| ------------- | --------------------------------------------------------------------------- |
| `lib.rs`      | Crate docs, module declarations, public re-exports, the free functions      |
| `error.rs`    | `Error`, `ParseError`, and ranking endpoint failures                        |
| `port_map.rs` | `Protocol`, `ContainerInfo`, `PortKey`, `ContainerPortMap`, port matching   |
| `client.rs`   | `Client`, `DetectionHandle`, endpoint selection, query fan-out, merging     |
| `stop.rs`     | `StopOutcome`, ID validation, stop endpoints, choosing the owning daemon    |
| `api.rs`      | Daemon JSON parsing and container name resolution                           |
| `http.rs`     | HTTP/1.0 framing and chunking (via `httparse`), nothing Docker-specific     |
| `ipc.rs`      | OS-specific transport: Unix sockets, named pipes, TCP, `DOCKER_HOST`        |
| `podman.rs`   | Rootless Podman resolution via overlay metadata (lookup runs on Linux)     |
| `proxy.rs`    | Recognition of container runtime port-proxy processes                       |

Do not create modules or add `[dependencies]` without explicit human approval. The runtime dependencies (`serde`, `serde_json`, `httparse`, `log`, plus `libc` on Unix) are a competitive advantage: anything `std` or an existing dependency can do stays that way. Dev-dependencies are `tempfile`, `gungraun`, and `proptest`.

## Testing

- Run `cargo test --lib --tests --all-features` and `cargo test --doc --all-features`. Never run a bare `cargo test`: it also starts the Gungraun benchmarks, which panic without Valgrind.
- Unit tests live in `#[cfg(test)] mod tests` in each module. Use `assert_eq!` with a message that says what is being checked.
- Integration tests under `tests/` use only the public API. Tests that need a live daemon return early unless their opt-in variable is set (`NANODOCK_IT=1` for Docker, `NANODOCK_IT_PODMAN=1` for rootless Podman); CI sets them.
- Benchmarks count instructions with Gungraun (`cargo bench --bench benchmarks`, Linux only). Do not use or optimize for wall-clock benchmarks.

## Commits

- Commit after each completed task, before starting the next one.
- Conventional Commits: `<type>(<optional-scope>): <description>`, with type one of `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`, `enforce`. The description starts lowercase, is 5 to 200 characters long, and has no trailing period. The commit-msg hook rejects anything else, including a `!` breaking marker.
- When behavior changes, update the doc comments, README, and docs/CONTRIBUTING.md in the same commit. A new module also updates this file; a dependency change also updates `deny.toml`.

## Working With the Operator

- Assume the operator's bug reports are accurate; do not dismiss them on static analysis alone.
- If something contradicts what the operator said, research it, tell them, and update the plan.
- Never write the em dash character (U+2014) in prose, comments, docs, or commit messages; use a comma, colon, parentheses, or a separate sentence.
