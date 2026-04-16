@echo off
REM nanodock - Pre-push hook (Windows batch version)
REM Runs the full quality gate before pushing to remote.
REM This mirrors the CI checks so issues are caught locally before a PR.
REM
REM Install: copy this file to .git\hooks\pre-push
REM          (remove the .bat extension when copying)

echo ======================================
echo   nanodock Pre-Push Quality Gate
echo ======================================
echo.

call :ensure_cargo

REM Gate 1: Formatting
echo -^> [1/7] Checking formatting...
cargo fmt --all -- --check
if %ERRORLEVEL% neq 0 (
    echo.
    echo X FORMATTING FAILED
    echo   Run: cargo fmt
    echo   Then try pushing again.
    exit /b 1
)
echo   OK Formatting

REM Gate 2: Clippy across Linux + Windows targets
echo -^> [2/7] Running cross-target clippy...
where pwsh >nul 2>nul
if %ERRORLEVEL% equ 0 (
    pwsh -NoProfile -File scripts\check-platform-clippy.ps1
) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts\check-platform-clippy.ps1
)
if %ERRORLEVEL% neq 0 (
    echo.
    echo X CLIPPY FAILED
    echo   Fix the lint errors above or install the missing rustup targets,
    echo   then try pushing again.
    exit /b 1
)
echo   OK Cross-target clippy

REM Gate 3: Tests
echo -^> [3/7] Running tests...
cargo test --lib --tests
if %ERRORLEVEL% neq 0 (
    echo.
    echo X TESTS FAILED
    echo   Fix the failing unit or integration tests, then try pushing again.
    exit /b 1
)

cargo test --doc
if %ERRORLEVEL% neq 0 (
    echo.
    echo X TESTS FAILED
    echo   Fix the failing doctests, then try pushing again.
    exit /b 1
)
echo   OK Tests

REM Gate 4: Benchmarks compile on all platforms. Actual execution requires
REM valgrind plus gungraun-runner and is enforced in Linux CI.
echo -^> [4/7] Compiling benchmarks...
cargo bench --no-run
if %ERRORLEVEL% neq 0 (
    echo.
    echo X BENCHMARK COMPILE FAILED
    echo   Fix the benchmark build errors, then try pushing again.
    exit /b 1
)
echo   OK Benchmarks

REM Gate 5: Library build
echo -^> [5/7] Building library...
cargo build
if %ERRORLEVEL% neq 0 (
    echo.
    echo X BUILD FAILED
    echo   Fix the build errors, then try pushing again.
    exit /b 1
)
echo   OK Build

REM Gate 6: Documentation
echo -^> [6/7] Building docs...
set "RUSTDOCFLAGS=-D warnings -D rustdoc::bare_urls -D rustdoc::invalid_rust_codeblocks -D rustdoc::private_intra_doc_links -D rustdoc::unescaped_backticks"
cargo doc --no-deps
if %ERRORLEVEL% neq 0 (
    echo.
    echo X DOCUMENTATION BUILD FAILED
    echo   Fix the doc errors, then try pushing again.
    exit /b 1
)
echo   OK Docs

REM Gate 7: Dependency audit (optional)
echo -^> [7/7] Auditing dependencies...
where cargo-deny >nul 2>nul
if %ERRORLEVEL% equ 0 (
    cargo deny check 2>nul
    if %ERRORLEVEL% neq 0 (
        echo   Warning: First attempt failed, clearing advisory-db cache...
        if exist "%USERPROFILE%\.cargo\advisory-dbs" rd /s /q "%USERPROFILE%\.cargo\advisory-dbs"
        if exist "%USERPROFILE%\.cargo\advisory-db" rd /s /q "%USERPROFILE%\.cargo\advisory-db"
        cargo deny check 2>nul
        if %ERRORLEVEL% neq 0 (
            echo.
            echo   WARNING: DEPENDENCY AUDIT FAILED ^(non-blocking^)
            echo   CI will enforce this check on the pull request.
        ) else (
            echo   OK Dependency audit ^(after cache clear^)
        )
    ) else (
        echo   OK Dependency audit
    )
) else (
    echo   SKIP cargo-deny not installed ^(install: cargo install cargo-deny^)
)

echo.
echo All quality gates passed. Pushing...
exit /b 0

:ensure_cargo
where cargo >nul 2>nul
if %ERRORLEVEL% equ 0 exit /b 0

if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
    set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
)

where cargo >nul 2>nul
if %ERRORLEVEL% equ 0 exit /b 0

echo.
echo X RUST TOOLCHAIN NOT FOUND
echo   cargo is not available in the git hook environment.
echo   Install Rust or add %%USERPROFILE%%\.cargo\bin to PATH, then try again.
exit /b 1
