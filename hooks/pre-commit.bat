@echo off
REM nanodock - Pre-commit hook (Windows batch version)
REM Prevents committing code that doesn't pass quality gates.
REM
REM Install: copy this file to .git\hooks\pre-commit
REM          (remove the .bat extension when copying)

echo ======================================
echo   nanodock Pre-Commit Quality Gate
echo ======================================
echo.

call :ensure_cargo

REM Gate 1: Formatting
echo -^> Checking formatting...
cargo fmt --all -- --check
if %ERRORLEVEL% neq 0 (
    echo.
    echo X FORMATTING FAILED
    echo   Run: cargo fmt
    echo   Then try committing again.
    exit /b 1
)
echo   OK Formatting

REM Gate 2: Clippy across Linux + Windows targets
echo -^> Running cross-target clippy...
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
    echo   then try committing again.
    exit /b 1
)
echo   OK Cross-target clippy

REM Gate 3: Tests
echo -^> Running tests...
cargo test --lib --tests
if %ERRORLEVEL% neq 0 (
    echo.
    echo X TESTS FAILED
    echo   Fix the failing unit or integration tests, then try committing again.
    exit /b 1
)

cargo test --doc
if %ERRORLEVEL% neq 0 (
    echo.
    echo X TESTS FAILED
    echo   Fix the failing doctests, then try committing again.
    exit /b 1
)
echo   OK Tests

echo.
echo All quality gates passed. Committing...
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
