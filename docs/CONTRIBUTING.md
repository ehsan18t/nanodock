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

Both scripts install pre-commit, pre-push, and commit-msg hooks that enforce
quality gates locally before CI.

The installers resolve Git's real hooks directory through Git metadata, so they
work from normal clones and linked worktrees instead of assuming `.git/hooks`
is always a plain directory under the working tree.

The Clippy gate uses `scripts/check-platform-clippy.sh` on shell-based setups
and `scripts/check-platform-clippy.ps1` on Windows PowerShell. The host target
still runs with `--all-targets`, while the other supported target lints
`--lib` so Linux-only and Windows-only cfg issues fail locally without
requiring a foreign C toolchain.

---

## Quality Gates

All of the following must pass before merging:

| Gate | Command                                                                  | Purpose                                             |
| ---- | ------------------------------------------------------------------------ | --------------------------------------------------- |
| 1    | `cargo fmt --check`                                                      | Consistent formatting                               |
| 2    | `scripts/check-platform-clippy.sh` / `scripts/check-platform-clippy.ps1` | Zero lint warnings across Linux + Windows cfg paths |
| 3    | `cargo test --lib --tests && cargo test --doc`                           | All tests pass                                      |
| 4    | `cargo bench --no-run`                                                   | Benchmarks compile                                  |
| 5    | `cargo build`                                                            | Library compiles                                    |
| 6    | `cargo doc --no-deps`                                                    | Documentation builds                                |
| 7    | `cargo deny check`                                                       | No vulnerable/banned deps                           |

CI runs on every push to `main` **and** on every pull request targeting `main`,
so cross-platform issues (Linux + Windows matrix) are caught before a PR is merged.

Workflow dependencies in `.github/workflows/` are pinned to full commit SHAs.
When updating an action, keep the trailing version comment (for example `# v6`)
so reviewers can see the intended upstream release at a glance.

For environment-specific diagnostics while developing, enable the `log` crate
at debug level to see Docker/Podman probing and transport fallback messages.

Pull requests also run a Linux benchmark regression job with Gungraun. CI saves
a merge-base baseline from `main`, runs the PR head against that baseline, and
fails the job when the instruction count (`Ir`) regresses beyond the configured
limit. CI uploads a `benchmark-reports-<sha>` artifact that contains the raw
console log plus the generated `target/gungraun/` report tree.

Because Gungraun executes through Valgrind, actual benchmark execution is
Linux-only. Windows contributors can still compile the benchmark harness with
`cargo bench --no-run`, but they cannot run the benchmark suite locally on
Windows.

To run the instruction benchmarks locally on Linux:

```bash
sudo apt-get install valgrind
cargo install --version 0.18.1 gungraun-runner
cargo bench --bench benchmarks
```

To compare against a named baseline and fail on instruction regressions:

```bash
cargo bench --bench benchmarks -- --save-baseline=main --callgrind-metrics=ir
cargo bench --bench benchmarks -- --baseline=main --callgrind-metrics=ir --callgrind-limits='ir=1.0%'
```

---

## Project Structure

```
src/
  lib.rs      — Public API, detection orchestration, port matching
  api.rs      — JSON response parsing, container name resolution
  http.rs     — Minimal HTTP/1.0 response parser (via httparse)
  ipc.rs      — OS-specific transport (Unix socket, named pipe, TCP)
  podman.rs   — Rootless Podman resolver via overlay metadata (Linux)
```

### Architecture Boundaries

- **`lib.rs`** owns the public API surface, detection orchestration, and
  port-to-container matching logic. All public types are defined here.
- **`api.rs`** owns JSON response parsing. It converts raw daemon responses
  into `ContainerPortMap` entries.
- **`http.rs`** owns HTTP protocol handling. It formats requests and parses
  responses using `httparse`. No Docker-specific logic lives here.
- **`ipc.rs`** owns OS-specific transport code. Unix sockets, Windows named
  pipes, TCP connections, and `DOCKER_HOST` parsing all live here.
- **`podman.rs`** owns rootless Podman resolution on Linux. It reads overlay
  storage metadata and OCI runtime configs to match network namespace paths
  to container names.

---

## Coding Standards

- **Clippy:** `all + pedantic + nursery` at deny level
- **Error handling:** `anyhow::Result` with `.context()` for fallible functions
  (currently the crate returns `Option` for daemon queries since detection is
  best-effort, but contributors should use `anyhow` when introducing fallible
  paths)
- **No `unwrap()`** outside of tests
- **Doc comments** on every public item
- **Functions <= 100 lines**, cognitive complexity <= 30
- **No `dbg!()`, `todo!()`, `unimplemented!()`**

---

## Commit Messages

Follow [Conventional Commits](https://www.conventionalcommits.org):

```
<type>(<scope>): <description>
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`,
`ci`, `chore`, `revert`.

Rules:
- Description starts lowercase, 5-200 characters
- No trailing period
- Scope is optional, lowercase, alphanumeric + hyphens

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
- Tests requiring a running Docker/Podman daemon should be `#[ignore]`-d
  with a comment explaining the requirement

```bash
cargo test
```

---

## Dependency Policy

- Prefer `std` over external crates
- Only MIT / Apache-2.0 / BSD / MPL-2.0 licensed crates
- `cargo deny check` must pass
- Do not add new dependencies without maintainer approval
