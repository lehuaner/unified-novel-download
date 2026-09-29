@echo off
REM === run-dev.bat - one-click dev environment launcher ===
REM Starts Java sidecar (unidbg) + Rust binary with watch mode.
REM Both processes start/stop together. Ctrl+C kills both.
REM
REM Usage:  run-dev.bat              (watch mode, auto-rebuild on src/ changes)
REM         run-dev.bat --no-watch      (plain cargo run, no auto-rebuild)
REM         run-dev.bat --no-clean      (keep stale dev instances alive)
REM
REM A leftover tomato-novel-downloader.exe (orphaned cargo-watch child, an IDE
REM debug session, a manual cargo run) locks target\debug\*.exe and makes the
REM build fail with:
REM     error: failed to remove file `...\target\debug\tomato-novel-downloader.exe`
REM run-dev.ps1 reaps such stale instances before building and kills the whole
REM process tree on exit, so the lock never survives. Use --no-clean to opt out.

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-dev.ps1" %*
if %ERRORLEVEL% NEQ 0 (
    echo.
    echo [ERROR] Script exited with code %ERRORLEVEL%.
    pause
)
