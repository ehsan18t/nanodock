# Copilot & AI Agent Instructions - nanodock

> This document defines how AI coding agents (GitHub Copilot, Cursor, Windsurf,
> Claude, etc.) must interact with this codebase. Treat every rule here as a
> hard constraint unless the human operator explicitly overrides it.

---

## 1 - Project Identity

| Field      | Value                                          |
| ---------- | ---------------------------------------------- |
| Language   | Rust (edition **2024**)                        |
| Type       | Library crate (published on crates.io)         |
| Platform   | Cross-platform (Linux x86-64, Windows x86-64)  |
| License    | MIT                                            |
| Min Rust   | latest stable (currently 1.93+)                |
| Repository | `https://github.com/ehsan18t/nanodock`         |

### 1.1 - Mission

`nanodock` is the synchronous, minimal-dependency Docker/Podman client for Rust.
It fills a specific gap in the ecosystem: container awareness (detection, port
mapping, lifecycle control) without pulling in an async runtime or a full Docker
SDK.

**Target audience:** CLI tools, system utilities, monitoring agents, and any Rust
program that needs to answer "which container owns this port?" without taking on
50+ transitive dependencies.

### 1.2 - Competitive Positioning

| Crate        | Async | Runtime Deps | Scope             |
| ------------ | ----- | ------------ | ----------------- |
| `bollard`    | Yes   | ~50+         | Full Docker API   |
| `docker-api` | Yes   | ~30+         | Full Docker API   |
| **nanodock** | No    | **4**        | Detection + Ports + Lifecycle |

nanodock does NOT aim to be a full Docker API client. It is the `minreq` of
container libraries: opinionated, minimal, and fast. Guard this positioning
fiercely. Do not creep toward general-purpose Docker API coverage.

### 1.3 - Design Origin

nanodock was extracted from [portlens](https://github.com/ehsan18t/portlens)
(a CLI port scanner). Some design decisions were inherited from that context.
When evaluating API changes, always ask: "Is this optimal for a standalone
library crate, or is it a leftover from the embedded-in-portlens era?"

---

## 2 - Coding Philosophy (non-negotiable)

1. **Zero-tolerance linting.** Clippy `all + pedantic + nursery` at **deny** level.
   Every lint violation is a compile error. Never `#[allow(...)]` a lint without a
   neighbouring comment explaining _why_.
2. **Layered Error Handling:**
   - **Best-effort path** (`start_detection` / `await_detection`): Returns
     `Option`. Designed for enrichment use cases where the daemon being down
     is not an error.
   - **Strict path** (`detect_containers`): Returns `Result<_, Error>`.
     Designed for consumers who need to distinguish failure modes.
   - **Internal logic:** Never use `unwrap()` or `expect()` in non-test code.
3. **Synchronous by Design:** Do **not** introduce `async/await`. The library
   queries local IPC sockets (Unix, Named Pipes) where latency is sub-millisecond.
   Introducing an async runtime would destroy the crate's minimal-dependency
   value proposition.
4. **Doc comments on every public item.** Clippy's `missing_docs` lint is active.
   Write idiomatic `///` doc comments.
5. **Functions <= 100 lines** (`too_many_lines` at deny). Split large blocks into
   well-named helpers.
6. **Cognitive complexity <= 30** per function. Prefer early returns and guard
   clauses.
7. **No disallowed macros:** `dbg!()`, `todo!()`, `unimplemented!()` are banned.

- If you find something that contradicts what the human operator is saying, do a
  deep web search, inform the operator, and update the plan.
- Assume the human operator's bug reports are accurate. Do not dismiss them
  based purely on static code analysis.
- Split work into small, discrete tasks using todos. Commit properly after
  _each_ task is completed before moving to the next.
- Never use the em dash (--) anywhere in the codebase except for file structures
  or diagrams.

---

## 3 - API Design Principles

These rules apply to every public type, function, and trait implementation.
They are derived from the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
and tuned for nanodock's library-crate positioning.

### 3.1 - Public Type Discipline

- **Eagerly implement common traits** on all public types (C-COMMON-TRAITS):
  `Debug`, `Clone`, `PartialEq`, `Eq`, `Hash` where semantically valid.
  Add `Serialize` (and `Deserialize` behind a feature gate if needed) for
  types consumers are likely to persist or transmit.
- **Enums must be `#[non_exhaustive]`** when they may gain variants in
  future minor releases (C-SEALED). This includes `Error`, `StopOutcome`,
  `PublishedContainerMatch`, and `Protocol`.
- **Struct fields are currently public** (`ContainerInfo`). This is acceptable
  during the `0.x` series. Before `1.0`, evaluate sealing fields behind
  accessor methods (C-STRUCT-PRIVATE) for future-proofing.
- **Display implementations** on all user-facing types: `Protocol`,
  `StopOutcome`, `Error` (already done), `ContainerInfo`.
- **`std::error::Error`** must be implemented on `Error` with proper
  `source()` chaining (already done).

### 3.2 - Function Signature Guidelines

- **Accept generics where possible** (C-GENERIC): prefer `impl AsRef<Path>`
  over `&Path` for path parameters in public APIs.
- **Return owned data from constructors** (C-CALLER-CONTROL): let the caller
  decide whether to `Arc`/`Rc` the result.
- **The `home` parameter**: Several functions accept `home: Option<PathBuf>` for
  Unix socket path discovery. This is the correct lightweight pattern for this
  crate. Do NOT over-engineer a builder/config struct for this. If future
  configuration needs arise (e.g., custom timeout, custom socket paths), then
  introduce a builder.
- **Timeouts**: The default 3-second daemon timeout is appropriate for
  interactive CLI consumers. If a consumer needs a different timeout, they
  should use `detect_containers` on their own thread with their own deadline.
  Do NOT add timeout parameters to every function.

### 3.3 - Semantic Versioning

| Change type                | Version bump |
| -------------------------- | ------------ |
| New public function/type   | Minor        |
| New method on existing type| Minor        |
| Bug fix                    | Patch        |
| Performance improvement    | Patch        |
| New `#[non_exhaustive]` variant | Minor   |
| Breaking API change        | Major        |
| Removing a public item     | Major        |

---

## 4 - Architecture Rules

```text
src/
  lib.rs      -- Public API, detection orchestration, port matching
  api.rs      -- JSON response parsing, container name resolution
  http.rs     -- Minimal HTTP/1.0 response parser (via httparse)
  ipc.rs      -- OS-specific transport (Unix socket, named pipe, TCP)
  podman.rs   -- Rootless Podman resolver via overlay metadata (Linux)
```

- **Do not create new modules** without explicit human approval.
- **Do not add new dependencies** without explicit human approval.
- **`lib.rs`** owns the public API surface, detection orchestration, and
  port-to-container matching.
- **`api.rs`** owns JSON response parsing and OCI container name resolution.
- **`http.rs`** owns HTTP/1.0 protocol handling. No Docker-specific logic here.
- **`ipc.rs`** owns OS-specific transport code. All socket/pipe/TCP connection
  management lives here.
- **`podman.rs`** owns rootless Podman resolution via overlay filesystem
  inspection (Linux only).

### 4.1 - Module Boundary Enforcement

When adding new functionality, respect existing module boundaries:

| Need to...                        | Put it in...   |
| --------------------------------- | -------------- |
| Parse daemon JSON responses       | `api.rs`       |
| Handle HTTP framing/chunking      | `http.rs`      |
| Connect to sockets/pipes/TCP      | `ipc.rs`       |
| Resolve rootless Podman           | `podman.rs`    |
| Expose public API or orchestrate  | `lib.rs`       |

If new functionality does not clearly fit any module, discuss with the human
operator before creating a new module.

---

## 5 - Core Dependencies

| Crate        | Type | Purpose                                         |
| ------------ | ---- | ----------------------------------------------- |
| `serde`      | Prod | Container metadata serialization                |
| `serde_json` | Prod | JSON response parsing from daemon API           |
| `httparse`   | Prod | HTTP/1.x response header and chunk-size parsing |
| `log`        | Prod | Logging facade for debug diagnostics            |
| `libc`       | Prod | Unix-only: `getuid()` for socket path discovery |
| `gungraun`   | Dev  | Deterministic instruction-counting benchmarks   |
| `tempfile`   | Dev  | File/directory generation for test fixtures     |

_Any attempt to add a new `[dependencies]` entry will be rejected._

### 5.1 - Dependency Philosophy

The runtime dependency count (4 crates + 1 Unix-only) is a **competitive
advantage**. Every proposed dependency must justify itself against the question:
"Can this be done with `std` or an existing dependency?" If yes, do not add it.

---

## 6 - Formatting & Style

- **rustfmt** with `edition = "2024"`, `max_width = 100`.
- Run `cargo fmt` before every commit.
- Use `snake_case` for functions/variables, `PascalCase` for types/enums,
  `SCREAMING_SNAKE_CASE` for constants.
- Line comments (`//`) for implementation notes; doc comments (`///`) for API docs.

---

## 7 - Testing & Benchmarking

- **CRITICAL EXECUTION RULE:** You must strictly run `cargo test --lib --tests`.
  A naked `cargo test` will attempt to execute `gungraun` benchmarks without the
  required Valgrind harness, causing immediate panics.
- Write unit tests for all pure/deterministic logic. Tests live in
  `#[cfg(test)] mod tests`.
- Use `assert_eq!` with descriptive messages:
  `assert_eq!(result, expected, "reason for checking")`.
- **Benchmarking:** Use `cargo bench --bench benchmarks` to run deterministic
  instruction counting via `gungraun`. Do NOT use or optimize for wall-clock
  time (`criterion`).

---

## 8 - Commit Rules

### 8.1 - Commit After Every Completed Task

Agents **must** commit immediately after completing each discrete task or fix.

### 8.2 - Conventional Commits Format

Format: `<type>(<optional-scope>): <lowercase description>`
Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
`build`, `ci`, `chore`, `revert`, `enforce`.

- Description starts lowercase, 5-200 characters.
- No trailing period.

### 8.3 - Commit Description Quality

Good examples:
`feat(api): parse container labels from daemon response`
`perf(http): avoid redundant string allocation in chunk parser`

---

## 9 - Git Hooks (install once)

```powershell
.\scripts\install-hooks.ps1
```

| Hook         | Gates                                                                                      |
| ------------ | ------------------------------------------------------------------------------------------ |
| `pre-commit` | `cargo fmt --check`, cross-target `clippy`, `cargo test --lib --tests`, `cargo test --doc` |
| `pre-push`   | Full CI mirror: fmt, clippy, lib/doc tests, `cargo bench --no-run`, build, docs, deny      |
| `commit-msg` | Conventional Commits format validation via regex                                           |

---

## 10 - Documentation Update Rule

**When you change behaviour, you MUST update documentation in the same commit.**

| What changed             | Update these                          |
| ------------------------ | ------------------------------------- |
| New public API           | Doc comments, README, CONTRIBUTING.md |
| Output format change     | README, docs/CONTRIBUTING.md          |
| Build / CI change        | docs/CONTRIBUTING.md, README          |
| New module               | This file, README                     |
| Dependency added/removed | Cargo.toml, deny.toml                 |

---

## 11 - CI Pipeline

CI runs on pushes to `main` and pull requests targeting `main`. Three primary
jobs:

1. **quality-gate** (Linux + Windows matrix) - fmt, clippy,
   `cargo test --locked`, build, cargo doc.
2. **benchmark-regression** (Linux only) - Uses `gungraun` to verify instruction
   counts against the PR `merge-base`. Enforces a strict
   `--callgrind-limits='ir=1.0%'` ceiling.
3. **audit** - `cargo deny check`.

All gates must pass before merge. See `.github/workflows/ci.yml`.

---

## 12 - What NOT to Do

- Do not use `unwrap()` or `expect()` outside of tests.
- Do not add `#[allow(clippy::*)]` without a comment justifying it.
- Do not change the architecture or add modules without human approval.
- Do not spawn external subprocesses (`docker` or `podman` CLIs). Use the
  IPC/HTTP API only.
- Do not add TLS or HTTP client libraries (`reqwest`, `hyper`). The daemon is
  local-only.
- Do not add an async runtime. This is a synchronous library by design.
- Do not expand scope toward full Docker API coverage. nanodock is focused on
  detection, port mapping, and lifecycle control.
- Do not add configuration builders, traits, or abstractions until there are at
  least 3 independent parameters that need coordinating.
- Do not break existing portlens integration without coordinating both repos.

---

## 13 - Quick Reference for Common Tasks

### Adding a new public function:

1. Define the function in the appropriate module.
2. Add `pub use` re-export in `lib.rs` if needed.
3. Write `///` doc comment with usage example.
4. Add unit tests.
5. Update README.md API reference table.
6. Run `cargo test --lib --tests && cargo doc --no-deps`.

### Adding a new public type:

1. Define in the appropriate module.
2. Derive/implement: `Debug`, `Clone`, `PartialEq`, `Eq`, `Hash` (if applicable).
3. Add `Serialize` if consumers are likely to serialize it.
4. Add `#[non_exhaustive]` if it is an enum that may gain variants.
5. Add `Display` if the type has a human-readable representation.
6. Write `///` doc comment with usage example.
7. Add `pub use` re-export in `lib.rs`.
8. Update README.md API reference table.

### Adding a new enum variant:

1. Ensure the enum is `#[non_exhaustive]` (this is a non-breaking minor change).
2. Add the variant with a doc comment.
3. Update all `match` arms in the codebase.
4. Add tests for the new variant.
5. Update README.md if it documents the enum.

---

## 14 - Roadmap Awareness

These items are known directions for the crate. Do not implement without human
approval, but be aware of them when making design decisions:

- **Trait implementations audit**: Ensure all public types satisfy the Rust API
  Guidelines C-COMMON-TRAITS checklist before 1.0.
- **`#[non_exhaustive]` sweep**: Add to all public enums before 1.0.
- **CHANGELOG.md**: Introduce before the first non-0.1.x release.
- **Cargo.toml metadata**: Add `homepage` and `documentation` fields.
- **Error type enrichment**: Consider finer-grained error variants (transport
  failure, permission denied, timeout) if consumer feedback warrants it.
- **`ContainerInfo` field sealing**: Evaluate private fields with accessor
  methods before 1.0 for future-proofing (C-STRUCT-PRIVATE).
