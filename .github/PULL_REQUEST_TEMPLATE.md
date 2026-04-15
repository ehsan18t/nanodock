## Description

<!-- What does this PR do? Keep it brief; link related issues below. -->

## Type of Change

<!-- Check the one that applies: -->

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds functionality)
- [ ] Breaking change (fix or feature that causes existing functionality to change)
- [ ] Refactor (no functional changes)
- [ ] Documentation update
- [ ] CI / build change
- [ ] Performance improvement

## Related Issues

<!-- Link issues: Closes #123, Fixes #456, Relates to #789 -->

## How was this tested?

<!-- Describe the test plan: new unit tests, manual commands, etc. -->

## Quality Checklist

> All gates are enforced by `pre-commit` and `pre-push` hooks.
> Run `cargo fmt && cargo clippy && cargo test` before pushing.

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --all-targets` passes (deny level)
- [ ] `cargo test --all-targets` passes
- [ ] `cargo doc --no-deps` builds without warnings
- [ ] Documentation updated (README, docs) if behaviour changed
- [ ] Commits follow [Conventional Commits](https://www.conventionalcommits.org) format

## Screenshots / Terminal Output

<!-- If applicable, paste terminal output or screenshots showing the change. -->
