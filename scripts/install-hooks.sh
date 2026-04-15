#!/bin/sh
# nanodock - Install git hooks
# Run once after cloning: ./scripts/install-hooks.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
HOOKS_DIR="$REPO_ROOT/hooks"

require_command() {
    command_name="$1"

    if command -v "$command_name" >/dev/null 2>&1; then
        return 0
    fi

    echo ""
    echo "X REQUIRED COMMAND NOT FOUND"
    echo "  '$command_name' is required to install hooks."
    exit 1
}

resolve_git_hooks_dir() {
    git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-path hooks
}

require_command git

GIT_HOOKS_DIR="$(resolve_git_hooks_dir)"

# Ensure .git/hooks directory exists
mkdir -p "$GIT_HOOKS_DIR"

install_hook() {
    hook_name="$1"
    source="$HOOKS_DIR/$hook_name"
    target="$GIT_HOOKS_DIR/$hook_name"

    if [ -f "$target" ]; then
        echo "  $hook_name: overwriting existing hook"
    fi

    cp "$source" "$target"
    chmod +x "$target"
    echo "  $hook_name: installed"
}

echo ""
echo "Installing nanodock git hooks..."
echo ""

install_hook "pre-commit"
install_hook "pre-push"
install_hook "commit-msg"

echo ""
echo "Git hooks installed successfully!"
echo ""
echo "Quality gates will now run automatically:"
echo ""
echo "  Commit-msg (validates commit message format):"
echo "    Format: <type>(<scope>): <description>"
echo "    Types:  feat, fix, docs, style, refactor, perf, test, build, ci, chore"
echo ""
echo "  Pre-commit (fast checks before each commit):"
echo "    1. cargo fmt --check    (formatting)"
echo "    2. cross-target clippy  (Linux + Windows cfg lints)"
echo "    3. cargo test           (tests)"
echo ""
echo "  Pre-push (full CI-equivalent checks before each push):"
echo "    1. cargo fmt --check    (formatting)"
echo "    2. cross-target clippy  (Linux + Windows cfg lints)"
echo "    3. cargo test           (tests)"
echo "    4. cargo build          (library build)"
echo "    5. cargo doc            (strict rustdoc checks)"
echo "    6. cargo deny check     (dependency audit)"
echo ""
echo "Install the supported lint targets once:"
echo "  rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-msvc"
