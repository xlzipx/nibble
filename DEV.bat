@echo off
REM ============================================================
REM  Nibble - DEV launcher (hot-reload)
REM  Runs `tauri dev`. First compile = 5-15 min (cold Rust build).
REM ============================================================
cd /d "%~dp0"
echo Starting tauri dev (first build may take 5-15 min)...
echo.
call npm run tauri dev
pause
