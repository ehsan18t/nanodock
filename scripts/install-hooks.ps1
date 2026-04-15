#!/usr/bin/env pwsh
# nanodock - Install git hooks
# Run once after cloning: .\scripts\install-hooks.ps1

$ErrorActionPreference = "Stop"

$repoRoot = Join-Path $PSScriptRoot ".."
$hooksDir = Join-Path $repoRoot "hooks"

function Get-GitHooksDirectory {
    param([string]$RepositoryRoot)

    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        throw "git is required to install hooks"
    }

    $hookPath = git -C $RepositoryRoot rev-parse --path-format=absolute --git-path hooks
    if ($LASTEXITCODE -ne 0) {
        throw "failed to resolve the git hooks directory; run this script inside a git working tree"
    }

    return $hookPath.Trim()
}

function Install-Hook {
    param(
        [string]$HooksDirectory,
        [string]$HookName
    )

    $source = Join-Path $hooksDir $HookName
    $target = Join-Path $HooksDirectory $HookName

    if (Test-Path $target) {
        Write-Output "  ${HookName}: overwriting existing hook"
    }

    Copy-Item $source $target -Force
    Write-Output "  ${HookName}: installed"
}

$gitHooksDir = Get-GitHooksDirectory -RepositoryRoot $repoRoot

if (-not (Test-Path $gitHooksDir)) {
    New-Item -ItemType Directory -Path $gitHooksDir -Force | Out-Null
}

Write-Output ""
Write-Output "Installing nanodock git hooks..."
Write-Output ""

Install-Hook -HooksDirectory $gitHooksDir -HookName "pre-commit"
Install-Hook -HooksDirectory $gitHooksDir -HookName "pre-push"
Install-Hook -HooksDirectory $gitHooksDir -HookName "commit-msg"

Write-Output ""
Write-Output "Git hooks installed successfully!"
Write-Output ""
Write-Output "Quality gates will now run automatically:"
Write-Output ""
Write-Output "  Commit-msg (validates commit message format):"
Write-Output "    Format: <type>(<scope>): <description>"
Write-Output "    Types:  feat, fix, docs, style, refactor, perf, test, build, ci, chore"
Write-Output ""
Write-Output "  Pre-commit (fast checks before each commit):"
Write-Output "    1. cargo fmt --check    (formatting)"
Write-Output "    2. cross-target clippy  (Linux + Windows cfg lints)"
Write-Output "    3. cargo test           (tests)"
Write-Output ""
Write-Output "  Pre-push (full CI-equivalent checks before each push):"
Write-Output "    1. cargo fmt --check    (formatting)"
Write-Output "    2. cross-target clippy  (Linux + Windows cfg lints)"
Write-Output "    3. cargo test           (tests)"
Write-Output "    4. cargo build          (library build)"
Write-Output "    5. cargo doc            (strict rustdoc checks)"
Write-Output "    6. cargo deny check     (dependency audit)"
Write-Output ""
Write-Output "Install the supported lint targets once:"
Write-Output "  rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-msvc"
