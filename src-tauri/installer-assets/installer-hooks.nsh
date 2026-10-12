; ============================================================
;  Nibble — NSIS installer hooks
;
;  POSTUNINSTALL: run AFTER Tauri's standard uninstall has removed
;  the binaries, shortcuts and Uninstall registry entry. We then
;  wipe everything else Nibble leaves on the system:
;
;    %APPDATA%\com.zipeek.nibble\       - history, watch.json,
;                                          custom presets, bin\ (ffmpeg, yt-dlp)
;    %LOCALAPPDATA%\com.zipeek.nibble\  - WebView2 cookies + cache + localStorage
;    HKCU\Software\com.zipeek.nibble    - any window-state regs (paranoia)
; ============================================================

!macro NSIS_HOOK_POSTUNINSTALL
  DetailPrint "Cleaning user data..."

  ; AppData — history, watch config, custom presets, auto-installed tools
  RMDir /r "$APPDATA\com.zipeek.nibble"

  ; LocalAppData — WebView2 user data folder (cookies, IndexedDB, cache)
  RMDir /r "$LOCALAPPDATA\com.zipeek.nibble"

  ; Any user-scope registry keys we (or Tauri plugins) might have written.
  DeleteRegKey HKCU "Software\com.zipeek.nibble"

  DetailPrint "User data wiped."
!macroend
