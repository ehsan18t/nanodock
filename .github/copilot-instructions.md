# Copilot & AI Agent Instructions - nanodock

> This document defines how AI coding agents (GitHub Copilot, Cursor, Windsurf,
> Claude, etc.) must interact with this codebase. Treat every rule here as a
> hard constraint unless the human operator explicitly overrides it.

---

## 1 - Project Identity

| Field      | Value                                         |
| ---------- | --------------------------------------------- |
| Language   | Rust (edition **2024**)                       |
| Type       | Library crate                                 |
| Platform   | Cross-platform (Linux x86-64, Windows x86-64) |
| License    | MIT                                           |
| Min Rust   | latest stable (currently 1.93+)               |
| Repository | `https://github.com/ehsan18t/nanodock`        |

---

## 2 - Coding Philosophy (non-negotiable)

1. **Zero-tolerance linting.** Clippy `all + pedantic + nursery` at **deny** level.
   Every lint violation is a compile error. Never `#[allow(...)]` a lint without a
   neighbouring comment explaining _why_.
2. **Error handling:** Use `Option` for daemon queries (best-effort detection).
   Use `anyhow::Result` if introducing new fallible paths. Provide context with
   `.context()` / `.with_context()`. Never `unwrap()` in non-test code.
3. **Doc comments on every public item.** Clippy's `missing_docs` lint is active.
   Write idiomatic `///` doc comments.
4. **Functions <= 100 lines** (`too_many_lines` at deny). Split large blocks into
   well-named helpers.
5. **Cognitive complexity <= 30** per function. Prefer early returns and guard
   clauses over deep nesting.
6. **No disallowed macros:** `dbg!()`, `todo!()`, `unimplemented!()` are banned.
   Use proper error handling instead.

- if you think something is wrong or what I am saying or my plan describing not matching about what you think, make sure to do a web search to find the correct information. if you find something that contradicts what I am saying, make sure to tell me about it and update the plan accordingly.
- if I report you about a issue, don't just assume I am a normal noob user who is doing things wrong. Make sure to check even if it seems what I am saying shouldn't happen. BECAUSE I AM REPORTING AFTER SEEING, AND YOU ARE ONLY ASSUMING BASED ON THE CODES.
- do super deep research always
- make the most comprehensive plan
- split the work into small tasks. you can split the tasks into todos if needed.
- maintain todos. make as many todos as needed to cover all the work that needs to be done. make sure to update the todos as you work on the project.
- commit properly. commit by task, not by file. commit right after the task is done, not the end of the session. DO NOT PROCEED TO ANOTHER TASK BEFORE YOU COMMIT THE PREVIOUS TASK. 1 session might have tasks since we are splitting the work into small tasks.
- Never use em dash (--) in anywhere in the codebase except for file structure or diagram or similar places.

---

## 3 - Architecture Rules

```
src/
  lib.rs      — Public API, detection orchestration, port matching
  api.rs      — JSON response parsing, container name resolution
  http.rs     — Minimal HTTP/1.0 response parser (via httparse)
  ipc.rs      — OS-specific transport (Unix socket, named pipe, TCP)
  podman.rs   — Rootless Podman resolver via overlay metadata (Linux)
```

- **Do not create new modules** without explicit human approval.
- **Do not add new dependencies** without explicit human approval.
  If a feature can be implemented with `std` or existing deps, do that.
- **`lib.rs`** owns the public API surface, detection orchestration, and
  port-to-container matching. All public types live here.
- **`api.rs`** owns JSON response parsing. Container name resolution lives here.
- **`http.rs`** owns HTTP protocol handling. No Docker-specific logic here.
- **`ipc.rs`** owns OS-specific transport code. All socket/pipe/TCP connections
  are managed here.
- **`podman.rs`** owns rootless Podman resolution (Linux only).

---

## 4 - Core Dependencies

| Crate      | Purpose                                         |
| ---------- | ----------------------------------------------- |
| serde      | Container metadata serialization                |
| serde_json | JSON response parsing from daemon API           |
| httparse   | HTTP/1.x response header and chunk-size parsing |
| log        | Logging facade for debug diagnostics            |
| libc       | Unix-only: `getuid()` for socket path discovery |

---

## 5 - Formatting & Style

- **rustfmt** with `edition = "2024"`, `max_width = 100`.
- Run `cargo fmt` before every commit.
- Use `snake_case` for functions/variables, `PascalCase` for types/enums,
  `SCREAMING_SNAKE_CASE` for constants.
- Prefer `const` over `static` where possible.
- Line comments (`//`) for implementation notes; doc comments (`///`) for API docs.

---

## 6 - Testing

- Write unit tests for all pure/deterministic logic (JSON parsing, HTTP parsing,
  port matching, type conversions).
- Tests live in `#[cfg(test)] mod tests` inside each module.
- Integration tests requiring a running Docker/Podman daemon should be
  `#[ignore]`-d with a comment explaining the requirement.
- Use `assert_eq!` with descriptive messages: `assert_eq!(result, expected, "reason")`.
- Run `cargo test` locally before pushing.

---

## 7 - Commit Rules

### 7.1 - Commit After Every Completed Task

Agents **must** commit immediately after completing each discrete task or fix.
Do not batch multiple unrelated changes into a single commit.

### 7.2 - Conventional Commits Format

```
<type>(<optional-scope>): <lowercase description>
```

Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`,
`build`, `ci`, `chore`, `revert`.

Rules:

- Description starts lowercase, 5-200 characters.
- No trailing period.
- Scope is optional, lowercase, alphanumeric + hyphens.

### 7.3 - Commit Description Quality

Commit messages must be **concise yet descriptive**.

Good examples:

```
feat(api): parse container labels from daemon response
fix(ipc): handle named pipe timeout on Windows
perf(http): avoid redundant string allocation in chunk parser
docs: update README with rootless Podman section
```

---

## 8 - Git Hooks (install once)

```powershell
.\scripts\install-hooks.ps1
```

| Hook         | Gates                                                                   |
| ------------ | ----------------------------------------------------------------------- |
| `pre-commit` | `cargo fmt --check`, `cargo clippy`, `cargo test`                       |
| `pre-push`   | 6-gate quality gate (fmt, clippy, test, build, docs, deny if installed) |
| `commit-msg` | Conventional Commits format validation                                  |

---

## 9 - Documentation Update Rule

**When you change behaviour, you MUST update documentation in the same commit.**

| What changed             | Update these                          |
| ------------------------ | ------------------------------------- |
| New public API           | Doc comments, README, CONTRIBUTING.md |
| Output format change     | README, docs/CONTRIBUTING.md          |
| Build / CI change        | docs/CONTRIBUTING.md, README          |
| New module               | This file, README                     |
| Dependency added/removed | Cargo.toml, deny.toml                 |

---

## 10 - CI Pipeline

CI runs on pushes to `main` and pull requests targeting `main`. Two jobs:

1. **quality-gate** (Linux + Windows matrix) - fmt, clippy, test, build, cargo doc
2. **audit** - `cargo deny check`

All gates must pass before merge. See `.github/workflows/ci.yml`.

---

## 11 - Dependency Policy

- Prefer `std` over external crates.
- Only MIT / Apache-2.0 / BSD / MPL-2.0 licensed crates.
- `cargo deny check` must pass (see `deny.toml`).
- Pin major versions in `Cargo.toml` (e.g., `"1"` not `"*"`).

---

## 12 - What NOT to Do

- Do not introduce async/await. The library is synchronous by design.
- Do not use `unwrap()` or `expect()` outside of tests.
- Do not add `#[allow(clippy::*)]` without a comment justifying it.
- Do not commit without running all quality gates.
- Do not change architecture without human approval.
- Do not skip doc updates when behaviour changes.
- Do not spawn external subprocesses (docker CLI, podman CLI). Use daemon API only.
- Do not add TLS or HTTP client dependencies. The daemon is local-only.

---

## 13 - Quick Reference for Common Tasks

### Adding a new public function:

1. Define the function in the appropriate module.
2. Add `pub use` re-export in `lib.rs` if needed.
3. Write `///` doc comment with usage example.
4. Add unit tests.
5. Update README.md API reference table.
6. Run `cargo test && cargo doc --no-deps`.

### Adding support for a new transport:

1. Implement the transport connection in `ipc.rs`.
2. Add the discovery logic to the appropriate platform block.
3. Wire it into `query_daemon()` in `lib.rs`.
4. Add tests (use `#[ignore]` if they need a running daemon).
5. Update the transport table in README.md.
6. Update docs/CONTRIBUTING.md if the architecture changes.
