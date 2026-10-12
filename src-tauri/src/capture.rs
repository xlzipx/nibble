// ============================================================
//  Screenshots (ShareX-style region capture)
//
//  1. capture_start: grab the monitor under the cursor into memory (GDI
//     BitBlt — a frozen frame, so menus/tooltips stay put while selecting).
//  2. A borderless, topmost overlay window (capture.html) shows that frame
//     dimmed; the user drags a rectangle (a plain click = whole screen).
//  3. capture_finish: crop, then copy to the clipboard and/or save a PNG
//     (Settings → Screenshots), and tell the main window ("screenshot-done").
//  The global hotkey (default Ctrl+Shift+A) calls the same start.
// ============================================================

use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// The frozen frame being selected from. BGRA, top-down rows.
struct Frame {
    w: u32,
    h: u32,
    bgra: Vec<u8>,
    /// The main window was hidden for the capture (in-app button) → restore it.
    hid_main: bool,
    /// Visible windows on this monitor (frame coordinates), topmost first —
    /// the overlay highlights / snaps to them like ShareX.
    windows: Vec<WinRect>,
    /// Monitor rect in physical px (for placing the result popup).
    mon: (i32, i32, u32, u32),
}

/// The last finished screenshot, for the result popup (shotpop.html).
pub(crate) struct LastShot {
    /// File to open on click: the saved PNG, or a temp copy when only copied.
    pub(crate) open_path: PathBuf,
    pub(crate) saved: bool,
    pub(crate) copied: bool,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) thumb_png: Vec<u8>,
    /// "image" | "video" | "gif" (popup wording)
    pub(crate) kind: &'static str,
    /// Recording length in seconds (videos)
    pub(crate) secs: f64,
}

pub(crate) static LAST: Mutex<Option<LastShot>> = Mutex::new(None);

/// Annotation layer sent by the overlay for the selected rect.
struct Layer {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    rgba: Vec<u8>,
}
static LAYER: Mutex<Option<Layer>> = Mutex::new(None);

/// Incremented per capture, so a late fallback hide can't hide the next one.
static SESSION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[derive(Serialize, Clone)]
pub struct WinRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    title: String,
    /// Process name of the window ("vlc", "chrome") — goes into the file
    /// name like ShareX does.
    #[serde(skip)]
    app: String,
    /// Visible child elements (menu bar, video area, panes…) in frame
    /// coordinates — hovering picks the smallest one under the cursor.
    children: Vec<ElemRect>,
}

/// The app a capture of rect (x, y, w, h) shows: the topmost window under
/// the rect's centre (works for snapped windows and free selections alike).
pub(crate) fn app_at(windows: &[WinRect], x: u32, y: u32, w: u32, h: u32) -> Option<String> {
    let (cx, cy) = ((x + w / 2) as i32, (y + h / 2) as i32);
    windows.iter()
        .find(|r| cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h)
        // Nibble's own exe may be "nibble-portable-v1.0.0" → just "nibble"
        .map(|r| if r.app.starts_with("nibble") { "nibble".to_string() } else { r.app.clone() })
        .filter(|a| !a.is_empty())
}

/// "vlc_2026-10-13_00-27-41", or `fallback` ("Screenshot" / "Recording")
/// when no app is known.
pub(crate) fn file_stem(app: Option<&str>, fallback: &str) -> String {
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let app: String = app.unwrap_or("").chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .take(40).collect();
    format!("{}_{}", if app.is_empty() { fallback } else { &app }, stamp)
}

#[cfg(windows)]
fn process_name(hwnd: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(h);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default()
    }
}

#[derive(Serialize, Clone)]
pub struct ElemRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

static FRAME: Mutex<Option<Frame>> = Mutex::new(None);

/// Top-level windows that are actually visible, topmost first, clipped to the
/// monitor rect and translated into frame coordinates.
#[cfg(windows)]
fn visible_windows(mx: i32, my: i32, mw: i32, mh: i32) -> Vec<WinRect> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowTextW, IsIconic, IsWindowVisible};

    struct Ctx { out: Vec<WinRect>, mx: i32, my: i32, mw: i32, mh: i32 }
    unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
        let ctx = &mut *(lp.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return true.into();
        }
        // Click-through overlays (NVIDIA / Discord / game overlays) are
        // invisible layers over the whole screen — not windows to snap to.
        let ex = windows::Win32::UI::WindowsAndMessaging::GetWindowLongW(hwnd, windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE) as u32;
        if ex & windows::Win32::UI::WindowsAndMessaging::WS_EX_TRANSPARENT.0 != 0 {
            return true.into();
        }
        // Fully transparent layered windows (e.g. NVIDIA GeForce Overlay: alpha 0)
        if ex & windows::Win32::UI::WindowsAndMessaging::WS_EX_LAYERED.0 != 0 {
            use windows::Win32::UI::WindowsAndMessaging::{GetLayeredWindowAttributes, LAYERED_WINDOW_ATTRIBUTES_FLAGS, LWA_ALPHA};
            let mut alpha: u8 = 255;
            let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
            if GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)).is_ok()
                && flags.0 & LWA_ALPHA.0 != 0 && alpha == 0
            {
                return true.into();
            }
        }
        // Cloaked = hidden by DWM (other virtual desktops, suspended UWP apps)
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        if cloaked != 0 {
            return true.into();
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut cls) as usize;
        let class = String::from_utf16_lossy(&cls[..n]);
        // The desktop itself is "the whole screen", not a window to snap to
        if class == "Progman" || class == "WorkerW" {
            return true.into();
        }
        // Visible bounds (without Windows 11's invisible resize borders)
        let mut r = RECT::default();
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32).is_err() {
            return true.into();
        }
        let (l, t) = (r.left.max(ctx.mx), r.top.max(ctx.my));
        let (rr, b) = (r.right.min(ctx.mx + ctx.mw), r.bottom.min(ctx.my + ctx.mh));
        if rr - l < 24 || b - t < 24 {
            return true.into();
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, &mut buf) as usize;
        let title = String::from_utf16_lossy(&buf[..len]);

        // Child elements (all descendants), clipped to the window's visible rect
        struct Kids { out: Vec<ElemRect>, clip: (i32, i32, i32, i32), origin: (i32, i32) }
        unsafe extern "system" fn kid_cb(h: HWND, lp: LPARAM) -> BOOL {
            let k = &mut *(lp.0 as *mut Kids);
            if k.out.len() >= 400 {
                return false.into();
            }
            if !IsWindowVisible(h).as_bool() {
                return true.into();
            }
            let mut r = RECT::default();
            if windows::Win32::UI::WindowsAndMessaging::GetWindowRect(h, &mut r).is_err() {
                return true.into();
            }
            let (l, t) = (r.left.max(k.clip.0), r.top.max(k.clip.1));
            let (rr, b) = (r.right.min(k.clip.2), r.bottom.min(k.clip.3));
            if rr - l >= 12 && b - t >= 12 {
                k.out.push(ElemRect { x: l - k.origin.0, y: t - k.origin.1, w: rr - l, h: b - t });
            }
            true.into()
        }
        let mut kids = Kids { out: Vec::new(), clip: (l, t, rr, b), origin: (ctx.mx, ctx.my) };
        let _ = windows::Win32::UI::WindowsAndMessaging::EnumChildWindows(Some(hwnd), Some(kid_cb), LPARAM(&mut kids as *mut Kids as isize));
        // Drop children that just repeat the window rect
        let (wx, wy, ww, wh) = (l - ctx.mx, t - ctx.my, rr - l, b - t);
        kids.out.retain(|c| !(c.x == wx && c.y == wy && c.w == ww && c.h == wh));

        ctx.out.push(WinRect { x: wx, y: wy, w: ww, h: wh, title, app: process_name(hwnd), children: kids.out });
        true.into()
    }
    let mut ctx = Ctx { out: Vec::new(), mx, my, mw, mh };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.out
}

#[cfg(not(windows))]
fn visible_windows(_: i32, _: i32, _: i32, _: i32) -> Vec<WinRect> {
    Vec::new()
}

/// Settings mirrored from the UI (localStorage) by `set_capture_config`.
#[derive(Clone)]
pub(crate) struct CaptureConfig {
    hotkey: String,     // "ctrl+shift+a" | "" (off)
    rec_hotkey: String, // opens the same overlay in video mode | "" (off)
    copy: bool,
    save: bool,
    dir: String,        // empty = Pictures\Nibble
    format: String,     // saved image: "png" | "jpg" | "webp"
    pub(crate) popup: bool,        // result popup with a preview (top centre)
    pub(crate) sound: bool,        // Windows notification sound
}

static CONFIG: Mutex<Option<CaptureConfig>> = Mutex::new(None);

pub(crate) fn config() -> CaptureConfig {
    CONFIG.lock().unwrap().clone().unwrap_or(CaptureConfig {
        hotkey: "ctrl+shift+a".into(),
        rec_hotkey: "ctrl+shift+r".into(),
        copy: true,
        save: false,
        dir: String::new(),
        format: "png".into(),
        popup: true,
        sound: true,
    })
}

/// The registered global shortcuts: (screenshot, recording).
static HOTKEYS: Mutex<(Option<tauri_plugin_global_shortcut::Shortcut>, Option<tauri_plugin_global_shortcut::Shortcut>)> =
    Mutex::new((None, None));

/// Temp folder for "copy only" shots (the popup still needs a file to open).
fn temp_shot_dir() -> PathBuf {
    let d = std::env::temp_dir().join("Nibble Screens");
    prune_temp_shots(TEMP_KEEP - 1); // room for the shot about to be written
    let _ = std::fs::create_dir_all(&d);
    d
}

/// How many "copy only" temp shots are kept at most.
const TEMP_KEEP: usize = 10;

/// Temp shots older than a day go, and only the newest `keep` stay. Runs at
/// startup too, so nothing piles up when no new copy-only shot is taken.
pub fn prune_temp_shots(keep: usize) {
    let d = std::env::temp_dir().join("Nibble Screens");
    let Ok(rd) = std::fs::read_dir(&d) else { return };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = rd
        .flatten()
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
    for (i, (p, t)) in files.iter().enumerate() {
        let old = t.elapsed().map_or(false, |age| age.as_secs() > 86_400);
        if old || i >= keep {
            let _ = std::fs::remove_file(p);
        }
    }
    let _ = std::fs::remove_dir(&d); // only succeeds when empty
}

/// Box-filter downscale (RGBA) to fit max_w × max_h — for the popup thumbnail.
fn thumbnail(rgba: &[u8], w: u32, h: u32, max_w: u32, max_h: u32) -> (Vec<u8>, u32, u32) {
    let k = (w as f64 / max_w as f64).max(h as f64 / max_h as f64).max(1.0);
    let (tw, th) = (((w as f64 / k).round() as u32).max(1), ((h as f64 / k).round() as u32).max(1));
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for ty in 0..th {
        let (y0, y1) = ((ty as f64 * k) as u32, (((ty + 1) as f64 * k) as u32).min(h).max((ty as f64 * k) as u32 + 1));
        for tx in 0..tw {
            let (x0, x1) = ((tx as f64 * k) as u32, (((tx + 1) as f64 * k) as u32).min(w).max((tx as f64 * k) as u32 + 1));
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for y in y0..y1 {
                let row = (y * w) as usize * 4;
                for x in x0..x1 {
                    let i = row + x as usize * 4;
                    for c in 0..4 { acc[c] += rgba[i + c] as u32; }
                    n += 1;
                }
            }
            let o = ((ty * tw + tx) * 4) as usize;
            for c in 0..4 { out[o + c] = (acc[c] / n.max(1)) as u8; }
        }
    }
    (out, tw, th)
}

fn png_bytes(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut wr) = enc.write_header() {
            let _ = wr.write_image_data(rgba);
        }
    }
    buf
}

#[cfg(windows)]
pub(crate) fn play_notify_sound() {
    use windows::core::w;
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_NODEFAULT};
    unsafe {
        let _ = PlaySoundW(w!("Notification.Default"), None, SND_ALIAS | SND_ASYNC | SND_NODEFAULT);
    }
}
#[cfg(not(windows))]
pub(crate) fn play_notify_sound() {}

fn default_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    home.join("Pictures").join("Nibble")
}

#[derive(Serialize, Clone)]
struct ScreenshotDone {
    ok: bool,
    copied: bool,
    path: Option<String>,
    width: u32,
    height: u32,
    error: Option<String>,
}

// ---------- Screen grab (Windows GDI) ----------

#[cfg(windows)]
fn grab_monitor_under_cursor() -> Result<(i32, i32, u32, u32, Vec<u8>), String> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).map_err(|e| format!("cursor: {}", e))?;
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return Err("monitor info failed".into());
        }
        let r = mi.rcMonitor;
        let (x, y, w, h) = (r.left, r.top, (r.right - r.left) as u32, (r.bottom - r.top) as u32);

        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| format!("dib: {}", e))?;
        let old = SelectObject(mem, dib.into());
        let blt = BitBlt(mem, 0, 0, w as i32, h as i32, Some(screen), x, y, SRCCOPY | CAPTUREBLT);
        let len = (w * h * 4) as usize;
        let mut out = vec![0u8; len];
        if blt.is_ok() && !bits.is_null() {
            std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), len);
        }
        SelectObject(mem, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        blt.map_err(|e| format!("capture: {}", e))?;
        // GDI leaves alpha at 0 — make it opaque.
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok((x, y, w, h, out))
    }
}

#[cfg(not(windows))]
fn grab_monitor_under_cursor() -> Result<(i32, i32, u32, u32, Vec<u8>), String> {
    Err("Screenshots are only supported on Windows".into())
}

/// The frozen frame as a top-down 32-bit BMP for the overlay (no encoding cost).
pub fn frame_bmp() -> Option<Vec<u8>> {
    let g = FRAME.lock().unwrap();
    let f = g.as_ref()?;
    let data_len = f.bgra.len() as u32;
    let mut v = Vec::with_capacity(54 + f.bgra.len());
    v.extend_from_slice(b"BM");
    v.extend_from_slice(&(54 + data_len).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(&54u32.to_le_bytes());
    v.extend_from_slice(&40u32.to_le_bytes());
    v.extend_from_slice(&(f.w as i32).to_le_bytes());
    v.extend_from_slice(&(-(f.h as i32)).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    v.extend_from_slice(&data_len.to_le_bytes());
    v.extend_from_slice(&[0u8; 16]);
    v.extend_from_slice(&f.bgra);
    Some(v)
}

// ---------- Overlay window ----------

/// The selection overlay is created ONCE (at startup) and then only shown /
/// hidden. A freshly created WebView2 composes a black frame for ~200 ms the
/// first time it's shown — that was the black flash at the start of a capture.
fn overlay_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(w) = app.get_webview_window("capture") {
        return Ok(w);
    }
    let mut b = tauri::WebviewWindowBuilder::new(app, "capture", tauri::WebviewUrl::App("capture.html".into()))
        .title("Nibble capture")
        .transparent(true)
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .visible(false);
    // Same WebView2 profile as the main window (required for the portable build).
    if let Some(root) = crate::portable_root() {
        b = b.data_directory(root.join("WebView2"));
    }
    b.build().map_err(|e| format!("overlay: {}", e))
}

/// Called from setup(): create the overlay and show it once, invisibly small, so its
/// first (black) composition happens where nobody sees it — afterwards every
/// show is clean. Focus goes straight back to the main window.
pub fn prepare_overlay(app: &AppHandle) {
    std::thread::spawn(|| prune_temp_shots(TEMP_KEEP));
    let Ok(win) = overlay_window(app) else { return };
    // 1×1 px in the corner of the primary screen: on-screen (so Windows really
    // composes it — off-screen windows count as occluded) yet invisible.
    let _ = win.set_size(tauri::PhysicalSize::new(1u32, 1u32));
    let _ = win.set_position(tauri::PhysicalPosition::new(0, 0));
    let _ = win.show();
    if let Some(m) = app.get_webview_window("main") {
        let _ = m.set_focus();
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if FRAME.lock().unwrap().is_none() {
            if let Some(w) = app2.get_webview_window("capture") {
                let _ = w.hide();
            }
        }
    });
}

fn open_overlay(app: &AppHandle, x: i32, y: i32, w: u32, h: u32, mode: &str) -> Result<(), String> {
    let win = overlay_window(app)?;
    SESSION.fetch_add(1, Ordering::SeqCst);
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    let _ = win.set_size(tauri::PhysicalSize::new(w, h));
    // The page loads the new frame and calls capture_ready → shown then.
    // Payload = "image" | "video" (which mode the toolbar opens in).
    let _ = app.emit("capture-begin", mode);   // (emit_to(label) never reached event.listen in the page)
    // Safety net in case the page never answers
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        if FRAME.lock().unwrap().is_some() {
            show_overlay(&app2);
        }
    });
    Ok(())
}

fn show_overlay(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("capture") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}
pub(crate) fn restore_main(app: &AppHandle, hid: bool) {
    if hid {
        if let Some(m) = app.get_webview_window("main") {
            let _ = m.show();
            let _ = m.set_focus();
        }
    }
}

/// Start a region capture. `hide_main` = started from Nibble's own button, so
/// the Nibble window shouldn't be in the shot. `mode` = "image" | "video".
pub fn start(app: &AppHandle, hide_main: bool, mode: &str) -> Result<(), String> {
    if FRAME.lock().unwrap().is_some() || crate::record::busy() {
        return Ok(()); // already selecting / recording
    }
    let main = app.get_webview_window("main");
    let hid = hide_main && main.as_ref().map_or(false, |m| m.is_visible().unwrap_or(false));
    if hid {
        if let Some(m) = &main {
            let _ = m.hide();
        }
        std::thread::sleep(std::time::Duration::from_millis(220)); // let DWM repaint without us
    }
    let (x, y, w, h, bgra) = match grab_monitor_under_cursor() {
        Ok(v) => v,
        Err(e) => {
            restore_main(app, hid);
            return Err(e);
        }
    };
    let windows = visible_windows(x, y, w as i32, h as i32);
    *FRAME.lock().unwrap() = Some(Frame { w, h, bgra, hid_main: hid, windows, mon: (x, y, w, h) });
    if let Err(e) = open_overlay(app, x, y, w, h, mode) {
        *FRAME.lock().unwrap() = None;
        restore_main(app, hid);
        return Err(e);
    }
    Ok(())
}

/// Recording takes over from the overlay: the selected monitor + whether the
/// main window was hidden. The frozen frame itself isn't needed any more.
pub(crate) fn take_frame() -> Option<((i32, i32, u32, u32), bool, Vec<WinRect>)> {
    FRAME.lock().unwrap().take().map(|f| (f.mon, f.hid_main, f.windows))
}

/// The page clears itself first (so a later show can't flash this frame),
/// then asks to be hidden via capture_hide. Fallback hide just in case.
pub(crate) fn close_overlay(app: &AppHandle) {
    let _ = app.emit("capture-reset", ());
    let session = SESSION.load(Ordering::SeqCst);
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        if SESSION.load(Ordering::SeqCst) == session {
            if let Some(w) = app2.get_webview_window("capture") {
                let _ = w.hide();
            }
        }
    });
}

/// The page has cleared itself after capture-reset → hide it.
#[tauri::command]
pub fn capture_hide(app: AppHandle) {
    if let Some(w) = app.get_webview_window("capture") {
        let _ = w.hide();
    }
}

/// Save in the format chosen in Settings (PNG / JPG / WebP).
fn save_image(path: &std::path::Path, format: &str, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    use image::ExtendedColorType;
    match format {
        "jpg" => {
            let file = std::fs::File::create(path).map_err(|e| format!("create {}: {}", path.display(), e))?;
            let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
            image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::BufWriter::new(file), 92)
                .encode(&rgb, w, h, ExtendedColorType::Rgb8)
                .map_err(|e| format!("jpg: {}", e))
        }
        "webp" => {
            let file = std::fs::File::create(path).map_err(|e| format!("create {}: {}", path.display(), e))?;
            image::codecs::webp::WebPEncoder::new_lossless(std::io::BufWriter::new(file))
                .encode(rgba, w, h, ExtendedColorType::Rgba8)
                .map_err(|e| format!("webp: {}", e))
        }
        _ => save_png(path, w, h, rgba),
    }
}

fn save_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("create {}: {}", path.display(), e))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut wr = enc.write_header().map_err(|e| format!("png: {}", e))?;
    wr.write_image_data(rgba).map_err(|e| format!("png: {}", e))
}

fn finish_inner(sel: Option<(u32, u32, u32, u32)>) -> Result<(ScreenshotDone, bool, (i32, i32, u32, u32)), (String, bool)> {
    let frame = FRAME.lock().unwrap().take().ok_or(("No capture in progress".to_string(), false))?;
    let hid = frame.hid_main;
    // Clamp the selection to the frame; tiny / missing selection = whole screen.
    let (sx, sy, sw, sh) = match sel {
        Some((x, y, w, h)) if w >= 4 && h >= 4 => {
            let x = x.min(frame.w - 1);
            let y = y.min(frame.h - 1);
            (x, y, w.min(frame.w - x), h.min(frame.h - y))
        }
        _ => (0, 0, frame.w, frame.h),
    };
    let mut rgba = Vec::with_capacity((sw * sh * 4) as usize);
    for row in sy..sy + sh {
        let start = ((row * frame.w + sx) * 4) as usize;
        for px in frame.bgra[start..start + (sw * 4) as usize].chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    // Drawings from the overlay toolbar (straight-alpha RGBA of the same rect)
    if let Some(l) = LAYER.lock().unwrap().take() {
        if (l.x, l.y, l.w, l.h) == (sx, sy, sw, sh) && l.rgba.len() == rgba.len() {
            for (d, s) in rgba.chunks_exact_mut(4).zip(l.rgba.chunks_exact(4)) {
                let a = s[3] as u32;
                if a == 0 { continue; }
                for c in 0..3 {
                    d[c] = ((s[c] as u32 * a + d[c] as u32 * (255 - a) + 127) / 255) as u8;
                }
            }
        }
    }
    let cfg = config();
    // "vlc_2026-…" — the app under the selection, like ShareX
    let stem = file_stem(app_at(&frame.windows, sx, sy, sw, sh).as_deref(), "Screenshot");
    let (mut copied, mut path) = (false, None);
    if cfg.copy || !cfg.save {
        let mut cb = arboard::Clipboard::new().map_err(|e| (format!("clipboard: {}", e), hid))?;
        cb.set_image(arboard::ImageData {
            width: sw as usize,
            height: sh as usize,
            bytes: std::borrow::Cow::Borrowed(&rgba),
        })
        .map_err(|e| (format!("clipboard: {}", e), hid))?;
        copied = true;
    }
    if cfg.save {
        let dir = if cfg.dir.trim().is_empty() { default_dir() } else { PathBuf::from(cfg.dir.trim()) };
        std::fs::create_dir_all(&dir).map_err(|e| (format!("folder: {}", e), hid))?;
        let ext = match cfg.format.as_str() { "jpg" => "jpg", "webp" => "webp", _ => "png" };
        let mut p = dir.join(format!("{}.{}", stem, ext));
        let mut n = 2;
        while p.exists() {
            p = dir.join(format!("{} ({}).{}", stem, n, ext));
            n += 1;
        }
        save_image(&p, ext, sw, sh, &rgba).map_err(|e| (e, hid))?;
        path = Some(p.to_string_lossy().into_owned());
    }
    // For the result popup and Recent Activity: a thumbnail + a file to open
    // (copy-only shots get a temp copy, cleaned up after a day).
    {
        let open_path = match &path {
            Some(p) => PathBuf::from(p),
            None => {
                let p = temp_shot_dir().join(format!("{}.png", stem));
                let _ = save_png(&p, sw, sh, &rgba);
                p
            }
        };
        let (t, tw, th) = thumbnail(&rgba, sw, sh, 360, 220);
        *LAST.lock().unwrap() = Some(LastShot {
            open_path,
            saved: path.is_some(),
            copied,
            w: sw,
            h: sh,
            thumb_png: png_bytes(tw, th, &t),
            kind: "image",
            secs: 0.0,
        });
    }
    Ok((ScreenshotDone { ok: true, copied, path, width: sw, height: sh, error: None }, hid, frame.mon))
}

// ---------- Result popup (top centre, like ShareX's toast) ----------

/// Like the selection overlay, the popup window is created once (hidden) and
/// reused — a new WebView2 per screenshot took seconds to appear.
fn popup_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(w) = app.get_webview_window("shotpop") {
        return Some(w);
    }
    let mut b = tauri::WebviewWindowBuilder::new(app, "shotpop", tauri::WebviewUrl::App("shotpop.html".into()))
        .title("Nibble screenshot")
        .decorations(false)
        .transparent(true)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(false)
        .visible(false)
        .inner_size(440.0, 128.0);
    if let Some(root) = crate::portable_root() {
        b = b.data_directory(root.join("WebView2"));
    }
    b.build().ok()
}

/// Startup: create the popup and let it compose once, invisibly (1×1 px).
pub fn prepare_popup(app: &AppHandle) {
    let Some(win) = popup_window(app) else { return };
    let _ = win.set_size(tauri::PhysicalSize::new(1u32, 1u32));
    let _ = win.set_position(tauri::PhysicalPosition::new(0, 0));
    let _ = win.show();
    if let Some(m) = app.get_webview_window("main") {
        let _ = m.set_focus();
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if let Some(w) = app2.get_webview_window("shotpop") {
            let _ = w.hide();
        }
    });
}

/// Top centre of the monitor the shot was taken on.
fn place_popup(app: &AppHandle, mon: (i32, i32, u32, u32)) -> bool {
    let Some(win) = popup_window(app) else { return false };
    let _ = win.set_size(tauri::LogicalSize::new(440.0, 128.0));
    let size = win.outer_size().unwrap_or(tauri::PhysicalSize::new(550, 160));
    let x = mon.0 + (mon.2 as i32 - size.width as i32) / 2;
    let _ = win.set_position(tauri::PhysicalPosition::new(x, mon.1));
    true
}

pub(crate) fn open_popup(app: &AppHandle, mon: (i32, i32, u32, u32)) {
    if place_popup(app, mon) {
        // The page loads the new thumbnail, then calls shot_popup_ready → shown.
        let _ = app.emit("shotpop-show", ());
    }
}

/// "Making the GIF…" card while a recording is being finished; the normal
/// shotpop-show replaces it when the file is ready.
pub(crate) fn open_popup_saving(app: &AppHandle, mon: (i32, i32, u32, u32), kind: &str) {
    if place_popup(app, mon) {
        let _ = app.emit("shotpop-saving", kind);
    }
}

// ---------- Commands ----------

/// In-app "Screenshot" button.
#[tauri::command]
pub fn capture_start(app: AppHandle, mode: Option<String>) -> Result<(), String> {
    // Off the IPC thread: hiding + sleeping + grabbing takes a moment.
    std::thread::spawn(move || {
        let mode = if mode.as_deref() == Some("video") { "video" } else { "image" };
        if let Err(e) = start(&app, true, mode) {
            let _ = app.emit("screenshot-done", ScreenshotDone { ok: false, copied: false, path: None, width: 0, height: 0, error: Some(e) });
        }
    });
    Ok(())
}

/// Overlay → selection in frame pixels (or none = whole screen).
#[tauri::command]
pub fn capture_finish(app: AppHandle, x: Option<u32>, y: Option<u32>, w: Option<u32>, h: Option<u32>) {
    close_overlay(&app);
    let sel = match (x, y, w, h) {
        (Some(x), Some(y), Some(w), Some(h)) => Some((x, y, w, h)),
        _ => None,
    };
    std::thread::spawn(move || {
        let (ev, hid, mon) = match finish_inner(sel) {
            Ok(v) => v,
            Err((e, hid)) => (ScreenshotDone { ok: false, copied: false, path: None, width: 0, height: 0, error: Some(e) }, hid, (0, 0, 0, 0)),
        };
        restore_main(&app, hid);
        if ev.ok {
            let cfg = config();
            if cfg.sound {
                play_notify_sound();
            }
            if cfg.popup {
                open_popup(&app, mon);
            }
        }
        if ev.ok {
            // Recent Activity entry — named like every other file
            // ("Nibble_<date>.png"); a copy-only shot uses its temp copy
            // (kept a day) and says "clipboard" in the type.
            let file = ev.path.clone().or_else(|| LAST.lock().unwrap().as_ref().map(|l| l.open_path.to_string_lossy().into_owned()));
            let name = file.as_deref()
                .and_then(|p| std::path::Path::new(p).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("Screenshot {}×{}", ev.width, ev.height));
            let size_mb = file.as_deref().and_then(|p| std::fs::metadata(p).ok())
                .map(|m| (m.len() as f64 / 1_048_576.0 * 10.0).round() / 10.0).unwrap_or(0.0);
            crate::push_history(&app, crate::HistoryEntry {
                when: chrono::Local::now().to_rfc3339(),
                preset: if ev.path.is_some() { "Screenshot".into() } else { "Screenshot · clipboard".into() },
                in_file: name.clone(),
                out_file: name,
                ok: true,
                secs: 0.0,
                size_mb,
                path: file,
            });
        }
        let _ = app.emit("screenshot-done", ev);
    });
}

/// Font families installed on the system (text tool's font picker).
#[cfg(windows)]
#[tauri::command]
pub fn system_fonts() -> Vec<String> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::Graphics::Gdi::{EnumFontFamiliesExW, GetDC, ReleaseDC, DEFAULT_CHARSET, LOGFONTW, TEXTMETRICW};
    unsafe extern "system" fn cb(lf: *const LOGFONTW, _tm: *const TEXTMETRICW, _ty: u32, lp: LPARAM) -> i32 {
        let out = &mut *(lp.0 as *mut Vec<String>);
        let face = &(*lf).lfFaceName;
        let len = face.iter().position(|&c| c == 0).unwrap_or(face.len());
        let name = String::from_utf16_lossy(&face[..len]);
        // '@' = vertical CJK variants, '<…>' = placeholder entries
        if !name.is_empty() && !name.starts_with('@') && !name.starts_with('<') {
            out.push(name);
        }
        1
    }
    let mut out: Vec<String> = Vec::new();
    unsafe {
        let hdc = GetDC(None);
        let lf = LOGFONTW { lfCharSet: DEFAULT_CHARSET, ..Default::default() };
        EnumFontFamiliesExW(hdc, &lf, Some(cb), LPARAM(&mut out as *mut Vec<String> as isize), 0);
        ReleaseDC(None, hdc);
    }
    out.sort_by_key(|s| s.to_lowercase());
    out.dedup();
    out
}
#[cfg(not(windows))]
#[tauri::command]
pub fn system_fonts() -> Vec<String> {
    Vec::new()
}

/// Overlay with drawings: raw RGBA of the annotation layer for the selected
/// rect (headers x, y, w, h in frame pixels), then the normal finish.
#[tauri::command]
pub fn capture_finish_layer(app: AppHandle, request: tauri::ipc::Request) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw RGBA".into());
    };
    let hdr = |k: &str| request.headers().get(k).and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u32>().ok());
    let (Some(x), Some(y), Some(w), Some(h)) = (hdr("x"), hdr("y"), hdr("w"), hdr("h")) else {
        return Err("missing rect".into());
    };
    if bytes.len() == (w as usize) * (h as usize) * 4 {
        *LAYER.lock().unwrap() = Some(Layer { x, y, w, h, rgba: bytes.clone() });
    }
    capture_finish(app, Some(x), Some(y), Some(w), Some(h));
    Ok(())
}

/// Overlay: windows to highlight / snap to (frame pixels, topmost first).
#[tauri::command]
pub fn capture_windows() -> Vec<WinRect> {
    FRAME.lock().unwrap().as_ref().map(|f| f.windows.clone()).unwrap_or_default()
}

#[tauri::command]
pub fn capture_cancel(app: AppHandle) {
    close_overlay(&app);
    let hid = FRAME.lock().unwrap().take().map_or(false, |f| f.hid_main);
    restore_main(&app, hid);
}

/// Overlay page: the frozen frame is painted → show the (hidden) window.
#[tauri::command]
pub fn capture_ready(app: AppHandle) {
    show_overlay(&app);
}

#[derive(Serialize)]
pub struct ShotInfo {
    name: String,
    saved: bool,
    copied: bool,
    width: u32,
    height: u32,
    kind: String,
    secs: f64,
    size_mb: f64,
}

/// Result popup: what to show.
#[tauri::command]
pub fn shot_popup_info() -> Option<ShotInfo> {
    LAST.lock().unwrap().as_ref().map(|l| ShotInfo {
        name: l.open_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        saved: l.saved,
        copied: l.copied,
        width: l.w,
        height: l.h,
        kind: l.kind.to_string(),
        secs: l.secs,
        size_mb: std::fs::metadata(&l.open_path).map(|m| m.len() as f64 / 1_048_576.0).unwrap_or(0.0),
    })
}

/// Result popup is painted → show it without taking focus.
#[tauri::command]
pub fn shot_popup_ready(app: AppHandle) {
    if let Some(w) = app.get_webview_window("shotpop") {
        let _ = w.show();
    }
}

/// Click on the popup: open the image in the default viewer.
#[tauri::command]
pub fn shot_open(app: AppHandle) -> Result<(), String> {
    let path = LAST.lock().unwrap().as_ref().map(|l| l.open_path.clone()).ok_or("no screenshot")?;
    if let Some(w) = app.get_webview_window("shotpop") {
        let _ = w.hide();
    }
    std::process::Command::new("explorer.exe").arg(&path).spawn()
        .map(|_| ()).map_err(|e| format!("open: {}", e))
}

/// Popup "folder" button: show the saved file in Explorer.
#[tauri::command]
pub fn shot_reveal(app: AppHandle) -> Result<(), String> {
    let path = LAST.lock().unwrap().as_ref().map(|l| l.open_path.clone()).ok_or("no screenshot")?;
    if let Some(w) = app.get_webview_window("shotpop") {
        let _ = w.hide();
    }
    crate::reveal_in_explorer(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn shot_popup_close(app: AppHandle) {
    if let Some(w) = app.get_webview_window("shotpop") {
        let _ = w.hide();
    }
}

/// UI settings → backend; (re)registers the global hotkey. Returns an error
/// text when the shortcut is taken by another app.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn set_capture_config(app: AppHandle, hotkey: String, copy: bool, save: bool, dir: String,
                          popup: Option<bool>, sound: Option<bool>,
                          rec_hotkey: Option<String>, format: Option<String>) -> Result<(), String> {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};
    let hotkey = hotkey.trim().to_ascii_lowercase();
    let rec_hotkey = rec_hotkey.unwrap_or_default().trim().to_ascii_lowercase();
    let first = CONFIG.lock().unwrap().is_none();
    let old = config();
    let changed = first || old.hotkey != hotkey || old.rec_hotkey != rec_hotkey;
    *CONFIG.lock().unwrap() = Some(CaptureConfig {
        hotkey: hotkey.clone(), rec_hotkey: rec_hotkey.clone(), copy, save, dir,
        format: format.unwrap_or_else(|| "png".into()),
        popup: popup.unwrap_or(true), sound: sound.unwrap_or(true),
    });
    if !changed {
        return Ok(());
    }
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    *HOTKEYS.lock().unwrap() = (None, None);
    let mut errors = Vec::new();
    let mut reg = |key: &str| -> Option<Shortcut> {
        if key.is_empty() {
            return None;
        }
        match key.parse::<Shortcut>() {
            Ok(sc) => match gs.register(sc) {
                Ok(()) => Some(sc),
                Err(e) => { errors.push(format!("{} is already used by another app ({})", key, e)); None }
            },
            Err(e) => { errors.push(format!("Invalid shortcut \"{}\": {:?}", key, e)); None }
        }
    };
    let shot = reg(&hotkey);
    let rec = if rec_hotkey != hotkey { reg(&rec_hotkey) } else { None };
    *HOTKEYS.lock().unwrap() = (shot, rec);
    if errors.is_empty() { Ok(()) } else { Err(errors.join(" · ")) }
}

/// Global-shortcut plugin with the capture handler. While recording, either
/// shortcut stops the recording.
pub fn shortcut_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    use tauri_plugin_global_shortcut::{Builder, ShortcutState};
    Builder::new()
        .with_handler(|app, shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                let app = app.clone();
                let is_rec = HOTKEYS.lock().unwrap().1.as_ref() == Some(shortcut);
                std::thread::spawn(move || {
                    if crate::record::busy() {
                        crate::record::stop_from_hotkey(&app);
                        return;
                    }
                    if let Err(e) = start(&app, false, if is_rec { "video" } else { "image" }) {
                        let _ = app.emit("screenshot-done", ScreenshotDone { ok: false, copied: false, path: None, width: 0, height: 0, error: Some(e) });
                    }
                });
            }
        })
        .build()
}

/// `nshot://localhost/frame.bmp` → the frozen frame for the overlay.
pub fn serve(req: &tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    // /thumb.png → the popup's preview; anything else → the frozen frame
    if req.uri().path().ends_with("thumb.png") {
        let png = LAST.lock().unwrap().as_ref().map(|l| l.thumb_png.clone());
        return match png {
            Some(p) => tauri::http::Response::builder().status(200)
                .header("Content-Type", "image/png").header("Cache-Control", "no-store")
                .header("Access-Control-Allow-Origin", "*").body(p).unwrap(),
            None => tauri::http::Response::builder().status(404).body(Vec::new()).unwrap(),
        };
    }
    match frame_bmp() {
        Some(bmp) => tauri::http::Response::builder()
            .status(200)
            .header("Content-Type", "image/bmp")
            .header("Cache-Control", "no-store")
            .header("Access-Control-Allow-Origin", "*")
            .body(bmp)
            .unwrap(),
        None => tauri::http::Response::builder().status(404).body(Vec::new()).unwrap(),
    }
}
