@echo off
REM nanodock - Commit message validation hook (Windows batch version)
REM Enforces Conventional Commits format.
REM
REM Install: copy to .git\hooks\commit-msg (remove .bat extension)

setlocal EnableExtensions

set "commit_msg_file=%~1"

REM Use PowerShell for regex validation (batch regex is too limited)
where pwsh >nul 2>nul
if %ERRORLEVEL% equ 0 goto use_pwsh

powershell -NoProfile -ExecutionPolicy Bypass -File scripts\Test-CommitMessage.ps1 "%commit_msg_file%"
goto handle_result

:use_pwsh
pwsh -NoProfile -File scripts\Test-CommitMessage.ps1 "%commit_msg_file%"

:handle_result
set "result=%ERRORLEVEL%"

if "%result%"=="0" exit /b 0
if "%result%"=="1" goto invalid_message
if "%result%"=="2" goto too_short
if "%result%"=="3" goto too_long
if "%result%"=="4" goto ends_with_period
exit /b 1

:invalid_message
set /p subject=<"%commit_msg_file%"
echo.
echo X COMMIT MESSAGE REJECTED
echo.
echo   Your message:  "%subject%"
echo.
echo   Expected format: ^<type^>(^<scope^>): ^<description^>
echo.
echo   Allowed types:
echo     feat, fix, docs, style, refactor, perf,
echo     test, build, ci, chore, revert, enforce
echo.
echo   Rules:
echo     - Description must start with a lowercase letter
echo     - No period at the end
echo     - Scope is optional: feat^(api^): ...
echo.
echo   Examples:
echo     feat^(api^): parse container labels from daemon response
echo     fix^(ipc^): handle named pipe timeout on Windows
echo.
exit /b 1

:too_short
echo.
echo X COMMIT MESSAGE TOO SHORT
echo   Description must be at least 5 characters.
echo.
exit /b 1

:too_long
echo.
echo X COMMIT MESSAGE TOO LONG
echo   Subject description must be 200 characters or fewer.
echo.
exit /b 1

:ends_with_period
echo.
echo X COMMIT MESSAGE ENDS WITH PERIOD
echo   Do not end the subject line with a period.
echo.
exit /b 1
