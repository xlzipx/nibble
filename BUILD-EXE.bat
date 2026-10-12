@echo off
REM ============================================================
REM  Nibble - BUILD release .exe + NSIS / MSI installers
REM
REM  Output:
REM    src-tauri\target\release\nibble.exe                   (raw app)
REM    src-tauri\target\release\bundle\nsis\Nibble_*-setup.exe (NSIS)
REM    src-tauri\target\release\bundle\msi\Nibble_*.msi        (MSI)
REM
REM  After a successful build the installers for THIS version are
REM  copied to installer\Nibble-Setup-v<version>.exe / .msi
REM  (version read from src-tauri\tauri.conf.json), plus the portable
REM  build as installer\Nibble-Portable-v<version>.exe.
REM ============================================================
cd /d "%~dp0"

for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "(Get-Content 'src-tauri\tauri.conf.json' -Raw | ConvertFrom-Json).version"`) do set "VER=%%v"
if "%VER%"=="" (
    echo Could not read the version from src-tauri\tauri.conf.json
    pause
    exit /b 1
)

echo.
echo === Nibble v%VER% - release build ===
echo.
call npm run tauri build
if errorlevel 1 (
    echo.
    echo BUILD FAILED. See output above.
    pause
    exit /b 1
)

echo.
echo === Copying installers to installer\ ===
if not exist "installer" mkdir "installer"

REM Exact file names for this version only - the bundle folders also keep
REM installers from older builds, which a wildcard would happily pick up.
set "NSIS_FILE=src-tauri\target\release\bundle\nsis\Nibble_%VER%_x64-setup.exe"
set "MSI_FILE=src-tauri\target\release\bundle\msi\Nibble_%VER%_x64_en-US.msi"

if exist "%NSIS_FILE%" (
    copy /Y "%NSIS_FILE%" "installer\Nibble-Setup-v%VER%.exe" >nul
    echo   installer\Nibble-Setup-v%VER%.exe   ^(NSIS, recommended^)
) else (
    echo   NSIS installer not found: %NSIS_FILE%
)
if exist "%MSI_FILE%" (
    copy /Y "%MSI_FILE%" "installer\Nibble-Setup-v%VER%.msi" >nul
    echo   installer\Nibble-Setup-v%VER%.msi   ^(MSI, alternative^)
) else (
    echo   MSI installer not found: %MSI_FILE%
)
REM Portable build = the same exe, named *Portable*: it then keeps all its data
REM in a "Nibble Data" folder next to itself (no install, nothing in AppData).
if exist "src-tauri\target\release\nibble.exe" (
    copy /Y "src-tauri\target\release\nibble.exe" "installer\Nibble-Portable-v%VER%.exe" >nul
    echo   installer\Nibble-Portable-v%VER%.exe   ^(portable, no install^)
)

echo.
echo Done. Standalone exe: src-tauri\target\release\nibble.exe
echo      Installers:     installer\
echo.
pause
