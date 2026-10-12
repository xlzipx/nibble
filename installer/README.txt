Nibble - Installer
==================

Nibble - capture, trim and share only what you need. Screenshots and
screen recordings, video downloads from 1000+ sites, trimming and
compression that fits Discord.
Made by ZIPEEK. MIT licensed.


WHICH FILE DO I RUN?
--------------------
  Nibble-Setup-v<version>.exe      <-- recommended (installs, Start menu entry)
  Nibble-Setup-v<version>.msi      <-- alternative (MSI, for enterprise/GPO)
  Nibble-Portable-v<version>.exe   <-- portable: no install, just run it

Use the Setup .exe for a normal install. The .msi is only there for people
who specifically need an MSI (managed deployment). Don't run both.


PORTABLE VERSION
----------------
Nibble-Portable-v<version>.exe runs without installing anything. Put it in
any folder (or on a USB stick) and double-click it. On first launch it
creates a "Nibble Data" folder next to itself and keeps EVERYTHING there:
history, settings, custom presets and the auto-installed ffmpeg / yt-dlp.
Nothing is written to AppData, so deleting the folder removes it completely.

Portable mode is decided by the file name: keep "Portable" in it.
(Alternatively, place an empty file called "portable" next to the exe.)
It needs the Microsoft WebView2 runtime, which ships with Windows 10/11.


HOW TO INSTALL
--------------
  1. Double-click Nibble-Setup-v<version>.exe
  2. Follow the wizard (Welcome -> License -> Folder -> Install -> Finish)
  3. Installs for the current user only - no administrator rights needed
  4. Creates a Start-menu entry and (optionally) a desktop shortcut


REQUIREMENTS
------------
Nibble drives external tools - it does NOT bundle them in the installer:

  ffmpeg + ffprobe   REQUIRED   convert / trim / compress / thumbnails
  yt-dlp             optional   "Download from URL" and "Quick share"

You do NOT need to install these by hand. On first launch Nibble shows a
short welcome screen that checks both tools and installs whatever is
missing with one click - into the app's own folder
(%APPDATA%\com.zipeek.nibble\bin, or "Nibble Data\bin" for the portable
version). No PATH changes, no admin. You can run the check again any time:
Settings -> System -> Required tools -> Check again.

yt-dlp needs regular updates as video sites change. The Download view and
Settings -> Downloads show its version and an "Update" button.

Prefer to install them yourself (e.g. to share one copy system-wide)?
On Windows:

  winget install ffmpeg
  winget install yt-dlp        (only if you want URL downloads)

The app uses its own downloaded copy first, then falls back to whatever
is on your PATH.


HARDWARE
--------
The app auto-detects your GPU and picks the best encoder per machine:
NVIDIA -> NVENC, AMD -> AMF, Intel -> QSV, otherwise CPU (libx264/x265).
Nothing to configure - just drop a file and convert.


UNINSTALLING
------------
Uninstall from Windows "Apps & features" (or the Start-menu entry).
The uninstaller removes ALL leftovers (settings, history, presets,
downloaded tools, WebView2 cache and registry keys) - a clean removal
with no orphan files.

Note: because of this, uninstalling wipes your conversion history.


REBUILDING THE INSTALLER (for the developer)
--------------------------------------------
  1. Run BUILD-EXE.bat in the project root
  2. It runs `npm run tauri build` and copies the fresh installer(s)
     into this folder, named after the version in tauri.conf.json.

The standalone app binary lives at:
  src-tauri\target\release\nibble.exe
That one has no installer / shortcuts - the files in THIS folder are the
proper installers you'd share with someone else.
