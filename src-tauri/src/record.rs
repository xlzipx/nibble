// ============================================================
//  Screen recording (video / GIF) of a selected region
//
//  The capture overlay (capture.html, "Video" mode) picks the region, then
//  record_start hands over to one controller thread:
//    countdown → ffmpeg (ddagrab = Desktop Duplication on the GPU, NVENC/AMF
//    straight from the D3D11 frames; gdigrab fallback) [+ WASAPI audio into a
//    WAV via cpal] → Stop → mux to MP4 (audio aligned by wall-clock time) or
//    convert to GIF (palette) → popup + Recent Activity (+ file on clipboard).
//  The recording bar (recbar.html) is one window around the region: a border
//  just outside it + the timer/stop bar. It's excluded from capture
//  (WDA_EXCLUDEFROMCAPTURE) and click-through except over the bar.
// ============================================================

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct RecOpts {
    format: String,   // "mp4" | "gif"
    countdown: u32,   // seconds before recording starts
    audio: String,    // "off" | "system" | "mic"
    cursor: bool,
    fps: u32,         // 30 | 60
    gif_fps: u32,     // 10 | 15 | 20 | 30
    dir: String,      // "" = Videos\Nibble
    copy: bool,       // put the file on the clipboard afterwards
    max_min: u32,     // stop automatically after N minutes (0 = no limit)
    max_mb: u32,      // … or once the recording reaches N MB (0 = no limit)
}
impl Default for RecOpts {
    fn default() -> Self {
        RecOpts {
            format: "mp4".into(), countdown: 3, audio: "off".into(), cursor: true, fps: 30, gif_fps: 15,
            dir: String::new(), copy: true, max_min: 60, max_mb: 2048,
        }
    }
}

/// Always-on safety net: stop when the target disk gets this low.
const MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

/// Live numbers from ffmpeg's progress pipe.
#[derive(Default)]
struct Stats {
    frames: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
}

#[cfg(windows)]
fn free_bytes(dir: &Path) -> Option<u64> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(dir.as_os_str()), Some(&mut free), None, None) }.ok()?;
    Some(free)
}
#[cfg(not(windows))]
fn free_bytes(_: &Path) -> Option<u64> {
    None
}

const IDLE: u8 = 0;
const COUNTDOWN: u8 = 1;
const RECORDING: u8 = 2;
const SAVING: u8 = 3;
static STATE: AtomicU8 = AtomicU8::new(IDLE);
static STOP: AtomicBool = AtomicBool::new(false);
static CANCEL: AtomicBool = AtomicBool::new(false);

/// A selection, countdown, recording or save is in progress.
pub fn busy() -> bool {
    STATE.load(Ordering::SeqCst) != IDLE
}

#[derive(Clone)]
struct Session {
    /// Display scale of the recorded monitor (the pointer is drawn that big)
    scale: f64,
    /// App under the recorded region ("vlc") — goes into the file name
    app: Option<String>,
    opts: RecOpts,
    mon: (i32, i32, u32, u32),
    hid_main: bool,
    /// Region relative to the monitor, physical px (even sizes)
    rel: (u32, u32, u32, u32),
    /// Bar rect in absolute physical px (for the click-through switch)
    bar_abs: (i32, i32, u32, u32),
    /// HDR desktop + the region shows mostly HDR content (game, HDR video)
    /// → tonemap it instead of dividing by SDR white (crate::hdr)
    tonemap: bool,
}

#[derive(Serialize, Clone)]
struct RecDone {
    ok: bool,
    kind: String,
    path: Option<String>,
    error: Option<String>,
    /// Stopped by a safety limit: "time" | "size" | "disk"
    auto: Option<String>,
}

// ---------- Commands ----------

/// Overlay → start recording the region (frame px, relative to the monitor;
/// no rect = the whole monitor).
#[tauri::command]
pub fn record_start(app: AppHandle, x: Option<u32>, y: Option<u32>, w: Option<u32>, h: Option<u32>, opts: RecOpts) -> Result<(), String> {
    if busy() {
        return Err("Already recording".into());
    }
    // The overlay's monitor DPI, for sizing the bar
    let scale = app.get_webview_window("capture").and_then(|w| w.scale_factor().ok()).unwrap_or(1.0);
    let Some((mon, hid_main, windows, hdr_mask)) = crate::capture::take_frame() else {
        return Err("No capture in progress".into());
    };
    crate::capture::close_overlay(&app);
    let (mut rx, mut ry, mut rw, mut rh) = match (x, y, w, h) {
        (Some(x), Some(y), Some(w), Some(h)) if w >= 16 && h >= 16 => (x, y, w, h),
        _ => (0, 0, mon.2, mon.3),
    };
    rx = rx.min(mon.2.saturating_sub(16));
    ry = ry.min(mon.3.saturating_sub(16));
    rw = rw.min(mon.2 - rx) & !1; // encoders want even sizes
    rh = rh.min(mon.3 - ry) & !1;

    // Layout: border just outside the region + the bar below / above / inside
    let m = (6.0 * scale).round() as i32;
    let (bw, bh, gap) = ((340.0 * scale) as i32, (46.0 * scale) as i32, (10.0 * scale) as i32);
    let (ax, ay) = (mon.0 + rx as i32, mon.1 + ry as i32);
    let mon_bottom = mon.1 + mon.3 as i32;
    let bx = (ax + rw as i32 / 2 - bw / 2).clamp(mon.0 + 8, mon.0 + mon.2 as i32 - bw - 8);
    let by = if ay + rh as i32 + m + gap + bh <= mon_bottom - 4 {
        ay + rh as i32 + m + gap
    } else if ay - m - gap - bh >= mon.1 + 4 {
        ay - m - gap - bh
    } else {
        ay + rh as i32 - bh - gap // inside (the bar is excluded from capture anyway)
    };
    let wx = (ax - m).min(bx);
    let wy = (ay - m).min(by);
    let wr = (ax + rw as i32 + m).max(bx + bw);
    let wb = (ay + rh as i32 + m).max(by + bh);

    let session = Session {
        opts: opts.clone(),
        mon,
        hid_main,
        rel: (rx, ry, rw, rh),
        bar_abs: (bx, by, bw as u32, bh as u32),
        app: crate::capture::app_at(&windows, rx, ry, rw, rh),
        scale,
        tonemap: crate::hdr::hdr_share(&hdr_mask, mon.2, (rx, ry, rw, rh)) >= 0.5,
    };
    STOP.store(false, Ordering::SeqCst);
    CANCEL.store(false, Ordering::SeqCst);
    STATE.store(COUNTDOWN, Ordering::SeqCst);

    if let Some(win) = bar_window(&app) {
        let _ = win.set_position(tauri::PhysicalPosition::new(wx, wy));
        let _ = win.set_size(tauri::PhysicalSize::new((wr - wx) as u32, (wb - wy) as u32));
        let _ = win.set_ignore_cursor_events(true);
        let _ = app.emit("rec-show", serde_json::json!({
            "region": [ax - wx, ay - wy, rw, rh],
            "bar": [bx - wx, by - wy, bw, bh],
            "margin": m,
            "countdown": opts.countdown.min(10),
            "kind": if opts.format == "gif" { "gif" } else { "video" },
            "audio": opts.audio,
        }));
    }
    let app2 = app.clone();
    std::thread::spawn(move || run(app2, session));
    Ok(())
}

#[tauri::command]
pub fn record_stop() {
    STOP.store(true, Ordering::SeqCst);
}

#[tauri::command]
pub fn record_cancel() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// Recording bar laid out → show it (without taking focus).
#[tauri::command]
pub fn rec_ready(app: AppHandle) {
    if busy() {
        if let Some(w) = app.get_webview_window("recbar") {
            let _ = w.show();
        }
    }
}

/// Global hotkey while busy: stop (during the countdown = cancel).
pub fn stop_from_hotkey(_app: &AppHandle) {
    match STATE.load(Ordering::SeqCst) {
        COUNTDOWN => CANCEL.store(true, Ordering::SeqCst),
        RECORDING => STOP.store(true, Ordering::SeqCst),
        _ => {}
    }
}

// ---------- Recording bar window ----------

fn bar_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(w) = app.get_webview_window("recbar") {
        return Some(w);
    }
    let mut b = tauri::WebviewWindowBuilder::new(app, "recbar", tauri::WebviewUrl::App("recbar.html".into()))
        .title("Nibble recording")
        .decorations(false)
        .transparent(true)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(false)
        .visible(false)
        .inner_size(400.0, 120.0);
    if let Some(root) = crate::portable_root() {
        b = b.data_directory(root.join("WebView2"));
    }
    let win = b.build().ok()?;
    exclude_from_capture(&win);
    Some(win)
}

/// Never part of a screenshot / recording (Windows 10 2004+).
#[cfg(windows)]
fn exclude_from_capture(win: &tauri::WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE};
    if let Ok(h) = win.hwnd() {
        unsafe {
            let _ = SetWindowDisplayAffinity(HWND(h.0 as _), WDA_EXCLUDEFROMCAPTURE);
        }
    }
}
#[cfg(not(windows))]
fn exclude_from_capture(_: &tauri::WebviewWindow) {}

/// Startup: create the bar and let it compose once at 1×1 px (no black flash later).
pub fn prepare(app: &AppHandle) {
    let Some(win) = bar_window(app) else { return };
    let _ = win.set_size(tauri::PhysicalSize::new(1u32, 1u32));
    let _ = win.set_position(tauri::PhysicalPosition::new(0, 0));
    let _ = win.set_ignore_cursor_events(true);
    let _ = win.show();
    if let Some(m) = app.get_webview_window("main") {
        let _ = m.set_focus();
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        if !busy() {
            if let Some(w) = app2.get_webview_window("recbar") {
                let _ = w.hide();
            }
        }
    });
}

fn hide_bar(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("recbar") {
        let _ = w.hide();
        let _ = w.set_ignore_cursor_events(true);
    }
}

/// Clicks pass through the region border; only the bar takes them.
#[cfg(windows)]
fn spawn_hit_test(app: AppHandle, bar: (i32, i32, u32, u32)) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    std::thread::spawn(move || {
        let mut over = false;
        while busy() {
            let mut pt = POINT::default();
            if unsafe { GetCursorPos(&mut pt) }.is_ok() {
                let now = pt.x >= bar.0 && pt.x < bar.0 + bar.2 as i32 && pt.y >= bar.1 && pt.y < bar.1 + bar.3 as i32;
                if now != over {
                    over = now;
                    if let Some(w) = app.get_webview_window("recbar") {
                        let _ = w.set_ignore_cursor_events(!over);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });
}
#[cfg(not(windows))]
fn spawn_hit_test(_: AppHandle, _: (i32, i32, u32, u32)) {}

// ---------- Controller ----------

fn run(app: AppHandle, s: Session) {
    spawn_hit_test(app.clone(), s.bar_abs);
    let end = |app: &AppHandle, s: &Session, done: Option<RecDone>| {
        STATE.store(IDLE, Ordering::SeqCst);
        hide_bar(app);
        crate::capture::restore_main(app, s.hid_main);
        if let Some(d) = done {
            let _ = app.emit("recording-done", d);
        }
    };
    let kind = if s.opts.format == "gif" { "gif" } else { "video" };
    let fail = |e: String| Some(RecDone { ok: false, kind: kind.into(), path: None, error: Some(e), auto: None });

    // Countdown (the bar shows it; Stop / hotkey / Cancel abort it)
    let t_end = Instant::now() + Duration::from_secs(s.opts.countdown.min(10) as u64);
    while Instant::now() < t_end {
        if CANCEL.load(Ordering::SeqCst) || STOP.load(Ordering::SeqCst) {
            end(&app, &s, None);
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // Work files live next to the final file: same disk (that's the one the
    // free-space guard watches, and %TEMP% on C: may be small), quick muxing.
    let dir = if s.opts.dir.trim().is_empty() { default_dir() } else { PathBuf::from(s.opts.dir.trim()) };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        end(&app, &s, fail(format!("folder: {}", e)));
        return;
    }
    if free_bytes(&dir).map_or(false, |f| f < MIN_FREE_BYTES) {
        end(&app, &s, fail(format!("Less than 1 GB free on the disk with {}", dir.display())));
        return;
    }
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let vid = dir.join(format!("~nibble-rec-{}.mkv", stamp));
    let wav = dir.join(format!("~nibble-rec-{}.wav", stamp));

    // Audio opens in parallel with ffmpeg (both take a moment to start; done
    // one after the other the waits added up). It's still running before
    // the first video frame, and the mux aligns both by wall-clock time.
    let audio_mode = if kind == "gif" { "off".to_string() } else { s.opts.audio.clone() };
    let wav2 = wav.clone();
    let audio_start = std::thread::spawn(move || match audio_mode.as_str() {
        "system" => AudioRec::start(true, wav2),
        "mic" => AudioRec::start(false, wav2),
        _ => None,
    });

    let mut child = None;
    let mut last_err = String::new();
    let stats = Arc::new(Stats::default());
    // HDR monitor → float capture + libplacebo (falls back to GDI, which is
    // right for SDR content but clips HDR video at 80 nits); SDR → GPU path.
    let hdr = dxgi_output(s.mon.0, s.mon.1).map_or(false, |(_, h)| h);
    let methods: &[&str] = if hdr { &["ddagrab_hdr", "gdigrab", "ddagrab"] } else { &["ddagrab", "gdigrab"] };
    let mut used_method = "";
    for method in methods {
        if CANCEL.load(Ordering::SeqCst) {
            break;
        }
        let _ = std::fs::remove_file(&vid);
        match spawn_ffmpeg(&s, &vid, method) {
            Ok(c) => match wait_started(c, stats.clone()) {
                Ok(c) => { child = Some(c); used_method = method; break; }
                Err(e) => last_err = format!("{}: {}", method, e),
            },
            Err(e) => last_err = e,
        }
    }
    let audio = audio_start.join().ok().flatten();
    let audio_missing = kind != "gif" && s.opts.audio != "off" && audio.is_none();
    let Some(mut child) = child else {
        if let Some(a) = audio { a.stop(); }
        let _ = std::fs::remove_file(&wav);
        let _ = std::fs::remove_file(&vid);
        let err = if CANCEL.load(Ordering::SeqCst) { None } else { fail(format!("Could not start recording ({})", last_err)) };
        end(&app, &s, err);
        return;
    };

    STATE.store(RECORDING, Ordering::SeqCst);
    // HDR path: track the pointer ourselves (burned in after recording)
    let cursor_track = (used_method == "ddagrab_hdr" && s.opts.cursor).then(|| {
        let fps = if kind == "gif" { s.opts.fps.min(30) } else { s.opts.fps.clamp(10, 60) };
        std::thread::spawn(move || track_cursor(fps))
    });
    let _ = app.emit("rec-started", serde_json::json!({ "audioMissing": audio_missing }));
    let t0 = Instant::now();
    let mut died = false;
    let mut auto: Option<&str> = None;   // which safety limit stopped it
    let mut next_disk_check = Instant::now();
    let (max_secs, max_bytes) = (s.opts.max_min as u64 * 60, s.opts.max_mb as u64 * 1024 * 1024);
    loop {
        if STOP.load(Ordering::SeqCst) || CANCEL.load(Ordering::SeqCst) {
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            died = true;
            break;
        }
        // Safety limits: forgotten recordings must not eat the disk
        if max_secs > 0 && t0.elapsed().as_secs() >= max_secs {
            auto = Some("time");
        } else if max_bytes > 0 && stats.bytes.load(Ordering::SeqCst) >= max_bytes {
            auto = Some("size");
        } else if Instant::now() >= next_disk_check {
            next_disk_check = Instant::now() + Duration::from_secs(2);
            if free_bytes(&dir).map_or(false, |f| f < MIN_FREE_BYTES) {
                auto = Some("disk");
            }
        }
        if auto.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let secs = t0.elapsed().as_secs_f64();

    // Stop ffmpeg cleanly ('q' finishes the file), then the audio
    STATE.store(SAVING, Ordering::SeqCst);
    let _ = app.emit("rec-saving", kind);
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() { break; }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut err_txt = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err_txt);
    }
    let audio_res = audio.and_then(|a| a.stop());
    let cursor_samples = cursor_track.and_then(|h| h.join().ok()).unwrap_or_default();
    // Recording is over: the bar goes right away; a GIF (takes a few
    // seconds) shows its progress in the result popup instead.
    hide_bar(&app);
    if !CANCEL.load(Ordering::SeqCst) {
        crate::capture::restore_main(&app, s.hid_main);
        if kind == "gif" && crate::capture::config().popup {
            crate::capture::open_popup_saving(&app, s.mon, kind);
        }
    }

    let cleanup = || {
        let _ = std::fs::remove_file(&vid);
        let _ = std::fs::remove_file(&wav);
    };
    if CANCEL.load(Ordering::SeqCst) {
        cleanup();
        end(&app, &s, None);
        return;
    }
    if died && std::fs::metadata(&vid).map_or(true, |m| m.len() < 1024) {
        cleanup();
        let msg = err_txt.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("ffmpeg stopped").to_string();
        end(&app, &s, fail(format!("Recording failed: {}", msg)));
        return;
    }

    // Final file
    let ext = if kind == "gif" { "gif" } else { "mp4" };
    let stem = crate::capture::file_stem(s.app.as_deref(), "Recording");
    let mut out = dir.join(format!("{}.{}", stem, ext));
    let mut n = 2;
    while out.exists() {
        out = dir.join(format!("{} ({}).{}", stem, n, ext));
        n += 1;
    }
    // Pointer burned in (HDR path); on failure the video just has no cursor
    if !cursor_samples.is_empty() {
        let burned = dir.join(format!("~nibble-rec-{}-c.mkv", stamp));
        let origin = (s.mon.0 + s.rel.0 as i32, s.mon.1 + s.rel.1 as i32);
        if burn_cursor(&vid, &burned, &cursor_samples, origin, (s.rel.2, s.rel.3), s.scale, &dir, &stamp).is_ok() {
            let _ = std::fs::remove_file(&vid);
            let _ = std::fs::rename(&burned, &vid);
        } else {
            let _ = std::fs::remove_file(&burned);
        }
    }
    let res = if kind == "gif" {
        make_gif(&vid, &out, s.opts.gif_fps.clamp(5, 50))
    } else {
        make_mp4(&vid, audio_res.as_ref(), &out)
    };
    if let Err(e) = res {
        cleanup();
        let _ = std::fs::remove_file(&out);
        crate::capture::shot_popup_close(app.clone());   // drop the "saving" card
        end(&app, &s, fail(e));
        return;
    }
    let thumb = thumbnail_png(if kind == "gif" { &vid } else { &out }).unwrap_or_default();
    cleanup();

    let out_str = out.to_string_lossy().into_owned();
    let copied = s.opts.copy && crate::copy_file_to_clipboard(&out_str).is_ok();
    *crate::capture::LAST.lock().unwrap() = Some(crate::capture::LastShot {
        open_path: out.clone(),
        saved: true,
        copied,
        w: s.rel.2,
        h: s.rel.3,
        thumb_png: thumb,
        kind: if kind == "gif" { "gif" } else { "video" },
        secs,
    });
    let size_mb = std::fs::metadata(&out).map(|m| (m.len() as f64 / 1_048_576.0 * 10.0).round() / 10.0).unwrap_or(0.0);
    let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    crate::push_history(&app, crate::HistoryEntry {
        when: chrono::Local::now().to_rfc3339(),
        preset: if kind == "gif" { "GIF recording".into() } else { "Screen recording".into() },
        in_file: name.clone(),
        out_file: name,
        ok: true,
        secs,
        size_mb,
        path: Some(out_str.clone()),
    });
    STATE.store(IDLE, Ordering::SeqCst);
    hide_bar(&app);
    crate::capture::restore_main(&app, s.hid_main);
    let cfg = crate::capture::config();
    if cfg.sound {
        crate::capture::play_notify_sound();
    }
    if cfg.popup {
        crate::capture::open_popup(&app, s.mon);
    }
    let _ = app.emit("recording-done", RecDone {
        ok: true, kind: kind.into(), path: Some(out_str), error: None, auto: auto.map(String::from),
    });
}

fn default_dir() -> PathBuf {
    let home = std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    home.join("Videos").join("Nibble")
}

// ---------- ffmpeg ----------

/// Frames carry wall-clock timestamps (setpts=RTCTIME, µs, kept by -copyts),
/// so the audio can be lined up exactly afterwards.
const TONEMAP_SHADER: &str = "nibble-hdr-tonemap.glsl";

fn spawn_ffmpeg(s: &Session, out: &Path, method: &str) -> Result<std::process::Child, String> {
    let (x, y, w, h) = s.rel;
    let fps = if s.opts.format == "gif" { s.opts.fps.min(30) } else { s.opts.fps.clamp(10, 60) };
    let cursor = if s.opts.cursor { 1 } else { 0 };
    let backend = crate::encoder_backend("h264");
    let gif = s.opts.format == "gif";
    let mut a: Vec<String> = vec![
        "-hide_banner".into(), "-loglevel".into(), "error".into(),
        // progress on stdout: "frame=N" tells us the recording really runs
        "-progress".into(), "pipe:1".into(), "-stats_period".into(), "0.05".into(),
    ];
    let mut push = |v: &[&str]| a.extend(v.iter().map(|s| s.to_string()));
    // Explicit BGRA → BT.709 YUV. (Feeding NVENC the D3D11 frames directly is
    // lighter, but tags the stream as RGB "gbr", and scale_d3d11 / CUDA
    // mapping aren't usable in common ffmpeg builds.)
    let conv = "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p,settb=AVTB,setpts=RTCTIME";
    if method == "ddagrab_hdr" {
        // HDR desktop: the composed frame in scRGB float (1.0 = 80 nits, SDR
        // white = the Windows "SDR content brightness"). Every 8-bit route
        // (BGRA, GDI, x2bgr10, even swscale's float conversion) clips at 1.0
        // = 80 nits → blown-out video. libplacebo works in float on the GPU:
        // scale SDR white back to 1.0, encode sRGB → SDR UI matches the screen
        // exactly; HDR highlights above SDR white clip there instead of at 80.
        let idx = dxgi_output_index(s.mon.0, s.mon.1).unwrap_or(0);
        let white = sdr_white_scale(s.mon.0, s.mon.1).unwrap_or(1.0).clamp(1.0, 12.5);
        // SDR content: scale SDR white to 1.0. HDR content (games, HDR
        // video): 203-nit reference white + soft highlights, as a user shader
        // (written to %TEMP%, ffmpeg runs there so the path needs no escaping)
        let mut adjust = format!("contrast={:.5}", 1.0 / white);
        if s.tonemap && std::fs::write(std::env::temp_dir().join(TONEMAP_SHADER), crate::hdr::tonemap_shader()).is_ok() {
            adjust = format!("custom_shader_path={}", TONEMAP_SHADER);
        }
        let f = format!(
            "ddagrab=output_idx={}:output_fmt=rgbaf16:force_fmt=1:framerate={}:draw_mouse={}:video_size={}x{}:offset_x={}:offset_y={},\
             hwdownload,format=rgbaf16le,setparams=color_trc=linear:color_primaries=bt709:colorspace=gbr:range=pc,\
             libplacebo=format=yuv420p:color_trc=iec61966-2-1:color_primaries=bt709:colorspace=bt709:range=tv:{},\
             settb=AVTB,setpts=RTCTIME",
            // ddagrab draws the pointer into a float texture without its
            // alpha (an arrow in a black square) → the cursor is burned in
            // afterwards instead (burn_cursor)
            idx, fps, 0, w, h, x, y, adjust
        );
        push(&["-f", "lavfi", "-i", &f]);
    } else if method == "ddagrab" {
        let idx = dxgi_output_index(s.mon.0, s.mon.1).unwrap_or(0);
        let f = format!(
            "ddagrab=output_idx={}:framerate={}:draw_mouse={}:video_size={}x{}:offset_x={}:offset_y={},hwdownload,format=bgra,{}",
            idx, fps, cursor, w, h, x, y, conv
        );
        push(&["-f", "lavfi", "-i", &f]);
    } else {
        let (ox, oy) = (s.mon.0 + x as i32, s.mon.1 + y as i32);
        push(&[
            "-f", "gdigrab", "-framerate", &fps.to_string(), "-draw_mouse", &cursor.to_string(),
            "-offset_x", &ox.to_string(), "-offset_y", &oy.to_string(),
            "-video_size", &format!("{}x{}", w, h), "-i", "desktop",
            "-vf", conv,
        ]);
    }
    // GIF intermediate: near-lossless, it gets re-quantised anyway
    let q = if gif { "16" } else { "21" };
    match backend {
        "nvenc" => push(&["-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", q, "-b:v", "0"]),
        "amf" => push(&["-c:v", "h264_amf", "-quality", "balanced", "-rc", "cqp", "-qp_i", q, "-qp_p", q]),
        "qsv" => push(&["-c:v", "h264_qsv", "-preset", "faster", "-global_quality", q]),
        _ => push(&["-c:v", "libx264", "-preset", "veryfast", "-crf", q]),
    }
    push(&[
        "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv",
        "-copyts", "-f", "matroska", "-y",
    ]);
    a.push(out.to_string_lossy().into_owned());
    crate::cmd_tool("ffmpeg")
        .args(&a)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg: {}", e))
}

/// Recording runs once ffmpeg reports encoded frames ("frame=N" on the
/// progress pipe — the file itself stays 0 bytes until its buffer fills).
fn wait_started(mut c: std::process::Child, stats: Arc<Stats>) -> Result<std::process::Child, String> {
    use std::io::BufRead;
    stats.frames.store(0, Ordering::SeqCst);
    stats.bytes.store(0, Ordering::SeqCst);
    if let Some(out) = c.stdout.take() {
        let st = stats.clone();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(n) = line.strip_prefix("frame=").and_then(|v| v.trim().parse::<u64>().ok()) {
                    st.frames.store(n, Ordering::SeqCst);
                } else if let Some(n) = line.strip_prefix("total_size=").and_then(|v| v.trim().parse::<u64>().ok()) {
                    st.bytes.store(n, Ordering::SeqCst);
                }
            }
        });
    }
    let frames = &stats.frames;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(Some(_)) = c.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = c.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            return Err(err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("ffmpeg exited").to_string());
        }
        if frames.load(Ordering::SeqCst) > 0 {
            return Ok(c);
        }
        if CANCEL.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let _ = c.kill();
    let _ = c.wait();
    Err(if CANCEL.load(Ordering::SeqCst) { "cancelled".into() } else { "timed out".into() })
}

// ---------- Pointer for the HDR path (burned in after recording) ----------

/// One pointer state: wall-clock time, screen position, cursor handle
/// (0 = hidden).
#[derive(Clone, Copy, PartialEq)]
struct CursorSample { t: f64, x: i32, y: i32, shape: isize }

/// Samples the pointer while recording (only changes are kept).
#[cfg(windows)]
fn track_cursor(fps: u32) -> Vec<CursorSample> {
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};
    let step = Duration::from_micros(1_000_000 / fps.max(10) as u64);
    let mut out: Vec<CursorSample> = Vec::new();
    while STATE.load(Ordering::SeqCst) == RECORDING {
        let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        if unsafe { GetCursorInfo(&mut ci) }.is_ok() {
            let shown = ci.flags.0 & CURSOR_SHOWING.0 != 0;
            let s = CursorSample { t: epoch_secs(), x: ci.ptScreenPos.x, y: ci.ptScreenPos.y, shape: if shown { ci.hCursor.0 as isize } else { 0 } };
            if out.last().map_or(true, |l| (l.x, l.y, l.shape) != (s.x, s.y, s.shape)) {
                out.push(s);
            }
        }
        std::thread::sleep(step);
    }
    out
}
#[cfg(not(windows))]
fn track_cursor(_: u32) -> Vec<CursorSample> { Vec::new() }

/// Renders a cursor to straight-alpha RGBA (drawn on black and on white to
/// recover alpha, which also handles old monochrome cursors). Returns
/// (rgba, w, h, hotspot x, hotspot y).
#[cfg(windows)]
fn render_cursor(shape: isize) -> Option<(Vec<u8>, u32, u32, i32, i32)> {
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::WindowsAndMessaging::{DrawIconEx, GetIconInfo, DI_NORMAL, HICON, ICONINFO};
    unsafe {
        let icon = HICON(shape as _);
        let mut ii = ICONINFO::default();
        GetIconInfo(icon, &mut ii).ok()?;
        let mut bm = BITMAP::default();
        let src = if !ii.hbmColor.is_invalid() { ii.hbmColor } else { ii.hbmMask };
        GetObjectW(src.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _));
        let (w, mut h) = (bm.bmWidth.max(1) as u32, bm.bmHeight.max(1) as u32);
        if ii.hbmColor.is_invalid() { h /= 2; } // mask = AND + XOR stacked
        let (hx, hy) = (ii.xHotspot as i32, ii.yHotspot as i32);
        if !ii.hbmColor.is_invalid() { let _ = DeleteObject(ii.hbmColor.into()); }
        if !ii.hbmMask.is_invalid() { let _ = DeleteObject(ii.hbmMask.into()); }

        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32, biHeight: -(h as i32), biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(dc, dib.into());
        let n = (w * h * 4) as usize;
        let draw = |bg: u8| -> Vec<u8> {
            std::ptr::write_bytes(bits as *mut u8, bg, n);
            let _ = DrawIconEx(dc, 0, 0, icon, w as i32, h as i32, 0, None, DI_NORMAL);
            std::slice::from_raw_parts(bits as *const u8, n).to_vec()
        };
        let on_black = draw(0);
        let on_white = draw(255);
        SelectObject(dc, old);
        let _ = DeleteObject(dib.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);

        let mut rgba = vec![0u8; n];
        for i in (0..n).step_by(4) {
            // per channel: black = a*c, white = a*c + (1-a)*255
            let diff = (0..3).map(|c| on_white[i + c] as i32 - on_black[i + c] as i32).sum::<i32>() / 3;
            let a = (255 - diff).clamp(0, 255);
            if a == 0 { continue; }
            for c in 0..3 {
                let v = (on_black[i + c] as i32 * 255 / a).clamp(0, 255) as u8;
                rgba[i + 2 - c] = v; // BGR → RGB
            }
            rgba[i + 3] = a as u8;
        }
        Some((rgba, w, h, hx, hy))
    }
}
#[cfg(not(windows))]
fn render_cursor(_: isize) -> Option<(Vec<u8>, u32, u32, i32, i32)> { None }

/// Re-encodes `vid` with the recorded pointer on top: one overlay per
/// cursor shape, moved frame-accurately by a sendcmd script (shapes not in
/// use park off-frame). Timestamps are kept, so audio lines up as before.
fn burn_cursor(vid: &Path, out: &Path, samples: &[CursorSample], origin: (i32, i32), size: (u32, u32), scale: f64, dir: &Path, stamp: &str) -> Result<(), String> {
    let t0 = video_start(vid).ok_or("no video start")?;
    // Up to 6 most used shapes
    let mut counts: std::collections::HashMap<isize, usize> = std::collections::HashMap::new();
    for s in samples.iter().filter(|s| s.shape != 0) { *counts.entry(s.shape).or_default() += 1; }
    let mut shapes: Vec<isize> = counts.keys().copied().collect();
    shapes.sort_by_key(|k| std::cmp::Reverse(counts[k]));
    shapes.truncate(6);
    let mut files = Vec::new();
    let mut hot = Vec::new();
    for (j, &sh) in shapes.iter().enumerate() {
        let Some((rgba, w, h, hx, hy)) = render_cursor(sh) else { continue };
        let p = dir.join(format!("~nibble-rec-{}-cur{}.png", stamp, j));
        image::save_buffer(&p, &rgba, w, h, image::ExtendedColorType::Rgba8).map_err(|e| e.to_string())?;
        files.push((p, sh));
        hot.push(((hx as f64 * scale).round() as i32, (hy as f64 * scale).round() as i32));
    }
    if files.is_empty() {
        return Err("no cursor shapes".into());
    }
    // sendcmd script: at each change, place the active shape, park the others
    let park = -(size.0.max(size.1) as i32) - 4096;
    let mut cmds = String::new();
    for s in samples {
        let t = (s.t - t0).max(0.0);
        let parts: Vec<String> = files.iter().enumerate().map(|(j, (_, sh))| {
            let (x, y) = if *sh == s.shape { (s.x - origin.0 - hot[j].0, s.y - origin.1 - hot[j].1) } else { (park, park) };
            format!("overlay@c{} x {}, overlay@c{} y {}", j, x, j, y)
        }).collect();
        cmds.push_str(&format!("{:.3} {};\n", t, parts.join(", ")));
    }
    let cmd_name = format!("~nibble-rec-{}-cursor.txt", stamp);
    let cmd_path = dir.join(&cmd_name);
    std::fs::write(&cmd_path, cmds).map_err(|e| e.to_string())?;

    let mut graph = format!("[0:v]setpts=PTS-STARTPTS,sendcmd=f={}[v0]", cmd_name);
    for j in 0..files.len() {
        graph.push_str(&format!(";[{}:v]scale=iw*{:.4}:ih*{:.4}:flags=lanczos,format=rgba[k{}];[v{}][k{}]overlay@c{}=x={}:y={}:eval=frame[v{}]", j + 1, scale, scale, j, j, j, j, park, park, j + 1));
    }
    graph.push_str(&format!(";[v{}]setpts=PTS+{:.6}/TB,format=yuv420p[out]", files.len(), t0));

    let s = |v: &str| v.to_string();
    let mut a = vec![s("-y"), s("-hide_banner"), s("-loglevel"), s("error"), s("-i"), vid.to_string_lossy().into_owned()];
    for (p, _) in &files { a.extend([s("-i"), p.to_string_lossy().into_owned()]); }
    a.extend([s("-filter_complex"), graph, s("-map"), s("[out]")]);
    match crate::encoder_backend("h264") {
        "nvenc" => a.extend(["-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", "18", "-b:v", "0"].map(s)),
        "amf" => a.extend(["-c:v", "h264_amf", "-quality", "balanced", "-rc", "cqp", "-qp_i", "18", "-qp_p", "18"].map(s)),
        "qsv" => a.extend(["-c:v", "h264_qsv", "-preset", "faster", "-global_quality", "18"].map(s)),
        _ => a.extend(["-c:v", "libx264", "-preset", "veryfast", "-crf", "18"].map(s)),
    }
    a.extend(["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv", "-copyts", "-f", "matroska"].map(s));
    a.push(out.to_string_lossy().into_owned());
    let res = crate::cmd_tool("ffmpeg").current_dir(dir).args(&a).output().map_err(|e| format!("ffmpeg: {}", e));
    let _ = std::fs::remove_file(&cmd_path);
    for (p, _) in &files { let _ = std::fs::remove_file(p); }
    let o = res?;
    if o.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("ffmpeg failed").to_string())
    }
}

/// First video timestamp (epoch seconds) — "start: 1760212345.678000".
fn video_start(vid: &Path) -> Option<f64> {
    let out = crate::cmd_tool("ffmpeg").args(["-hide_banner", "-i"]).arg(vid).output().ok()?;
    let txt = String::from_utf8_lossy(&out.stderr);
    let i = txt.find("start: ")? + 7;
    txt[i..].split(|c: char| c == ',' || c.is_whitespace()).next()?.parse().ok()
}

fn run_ffmpeg(args: &[String]) -> Result<(), String> {
    let out = crate::cmd_tool("ffmpeg").args(args).output().map_err(|e| format!("ffmpeg: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("ffmpeg failed").to_string())
    }
}

fn make_mp4(vid: &Path, audio: Option<&(PathBuf, f64)>, out: &Path) -> Result<(), String> {
    let s = |v: &str| v.to_string();
    let mut a = vec![s("-y"), s("-hide_banner"), s("-loglevel"), s("error"), s("-i"), vid.to_string_lossy().into_owned()];
    match audio {
        Some((wav, t_audio)) => {
            // Audio usually starts a little before the first frame → skip that bit
            if let Some(t_video) = video_start(vid) {
                let d = t_audio - t_video;
                if d >= 0.0 {
                    a.extend([s("-itsoffset"), format!("{:.3}", d)]);
                } else {
                    a.extend([s("-ss"), format!("{:.3}", -d)]);
                }
            }
            a.extend([s("-i"), wav.to_string_lossy().into_owned()]);
            a.extend(["-map", "0:v:0", "-map", "1:a:0", "-c:v", "copy", "-c:a", "aac", "-b:a", "192k", "-shortest"].map(s));
        }
        None => a.extend(["-map", "0:v:0", "-c:v", "copy"].map(s)),
    }
    a.extend([s("-movflags"), s("+faststart"), out.to_string_lossy().into_owned()]);
    run_ffmpeg(&a)
}

fn make_gif(vid: &Path, out: &Path, fps: u32) -> Result<(), String> {
    let vf = format!(
        "fps={},scale='min(iw,1280)':-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle",
        fps
    );
    let s = |v: &str| v.to_string();
    let a = vec![
        s("-y"), s("-hide_banner"), s("-loglevel"), s("error"), s("-i"), vid.to_string_lossy().into_owned(),
        s("-vf"), vf, s("-loop"), s("0"), out.to_string_lossy().into_owned(),
    ];
    run_ffmpeg(&a)
}

/// First frame, ≤360 px wide, as PNG bytes (popup preview).
fn thumbnail_png(file: &Path) -> Option<Vec<u8>> {
    let out = crate::cmd_tool("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(file)
        .args(["-frames:v", "1", "-vf", "scale='min(360,iw)':-2", "-f", "image2pipe", "-vcodec", "png", "-"])
        .output()
        .ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

/// ddagrab's output_idx: DXGI output of adapter 0 whose desktop rect starts
/// at the monitor's top-left.
#[cfg(windows)]
fn dxgi_output_index(mx: i32, my: i32) -> Option<u32> {
    dxgi_output(mx, my).map(|(i, _)| i)
}

/// (output index, HDR on). Desktop Duplication in 8-bit on an HDR display
/// clips everything above SDR white → washed-out, over-bright video. GDI
/// (BitBlt / gdigrab) gets the properly mapped SDR image, like screenshots.
#[cfg(windows)]
pub(crate) fn dxgi_output(mx: i32, my: i32) -> Option<(u32, bool)> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let adapter = factory.EnumAdapters1(0).ok()?;
        let mut i = 0u32;
        while let Ok(output) = adapter.EnumOutputs(i) {
            if let Ok(desc) = output.GetDesc() {
                let r = desc.DesktopCoordinates;
                if r.left == mx && r.top == my {
                    let hdr = output.cast::<IDXGIOutput6>().ok()
                        .and_then(|o| o.GetDesc1().ok())
                        .map_or(false, |d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020);
                    return Some((i, hdr));
                }
            }
            i += 1;
        }
    }
    None
}
/// Windows "SDR content brightness" of the monitor at (mx, my): SDR white as
/// a multiple of 80 nits (= 1.0 in the scRGB the desktop is composed in).
/// 480-nit SDR white → 6.0. None when HDR is off / unknown.
#[cfg(windows)]
pub(crate) fn sdr_white_scale(mx: i32, my: i32) -> Option<f64> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST};
    unsafe {
        // GDI name of our monitor ("\\.\DISPLAY1")
        let hmon = MonitorFromPoint(POINT { x: mx + 1, y: my + 1 }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut _).as_bool() {
            return None;
        }
        let want = String::from_utf16_lossy(&mi.szDevice).trim_end_matches('\0').to_string();

        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm).is_err() {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
            return None;
        }
        for p in paths.iter().take(np as usize) {
            let mut name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: p.sourceInfo.adapterId,
                id: p.sourceInfo.id,
            };
            if DisplayConfigGetDeviceInfo(&mut name.header) != 0 {
                continue;
            }
            let gdi = String::from_utf16_lossy(&name.viewGdiDeviceName).trim_end_matches('\0').to_string();
            if !gdi.eq_ignore_ascii_case(&want) {
                continue;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            white.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: p.targetInfo.adapterId,
                id: p.targetInfo.id,
            };
            if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel > 0 {
                return Some(white.SDRWhiteLevel as f64 / 1000.0);
            }
        }
    }
    None
}
#[cfg(not(windows))]
pub(crate) fn sdr_white_scale(_: i32, _: i32) -> Option<f64> {
    None
}

#[cfg(not(windows))]
fn dxgi_output_index(_: i32, _: i32) -> Option<u32> {
    None
}
#[cfg(not(windows))]
pub(crate) fn dxgi_output(_: i32, _: i32) -> Option<(u32, bool)> {
    None
}

// ---------- Audio (cpal: WASAPI loopback = what the PC plays, or the mic) ----------

struct AudioState {
    writer: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
    frames: u64,
    start: Instant,
    rate: u32,
    ch: u16,
}

impl AudioState {
    /// Loopback delivers nothing while the PC is silent → fill the gaps with
    /// silence, so the track stays in step with the clock (and the video).
    fn push(&mut self, data: &[f32]) {
        let ch = self.ch.max(1) as u64;
        let n = data.len() as u64 / ch;
        let expected = (self.start.elapsed().as_secs_f64() * self.rate as f64) as u64;
        let Some(w) = self.writer.as_mut() else { return };
        if expected > self.frames + n + self.rate as u64 / 10 {
            let gap = expected - n - self.frames;
            for _ in 0..gap * ch {
                let _ = w.write_sample(0.0f32);
            }
            self.frames += gap;
        }
        for &v in data {
            let _ = w.write_sample(v);
        }
        self.frames += n;
    }
    fn finish(&mut self) {
        let expected = (self.start.elapsed().as_secs_f64() * self.rate as f64) as u64;
        if let Some(w) = self.writer.as_mut() {
            if expected > self.frames {
                for _ in 0..(expected - self.frames) * self.ch.max(1) as u64 {
                    let _ = w.write_sample(0.0f32);
                }
            }
        }
        if let Some(w) = self.writer.take() {
            let _ = w.finalize();
        }
    }
}

struct AudioRec {
    stop_tx: mpsc::Sender<()>,
    handle: std::thread::JoinHandle<Option<(PathBuf, f64)>>,
}

fn epoch_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl AudioRec {
    /// The stream lives on its own thread (cpal streams aren't Send).
    fn start(loopback: bool, path: PathBuf) -> Option<AudioRec> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let handle = std::thread::spawn(move || -> Option<(PathBuf, f64)> {
            let fail = |tx: &mpsc::Sender<bool>| { let _ = tx.send(false); None };
            let host = cpal::default_host();
            let dev = if loopback { host.default_output_device() } else { host.default_input_device() };
            let Some(dev) = dev else { return fail(&ready_tx) };
            let cfg = if loopback { dev.default_output_config() } else { dev.default_input_config() };
            let Ok(cfg) = cfg else { return fail(&ready_tx) };
            let (rate, ch) = (cfg.sample_rate().0, cfg.channels());
            let spec = hound::WavSpec { channels: ch, sample_rate: rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
            let Ok(writer) = hound::WavWriter::create(&path, spec) else { return fail(&ready_tx) };
            let st = Arc::new(Mutex::new(AudioState { writer: Some(writer), frames: 0, start: Instant::now(), rate, ch }));
            let sc: cpal::StreamConfig = cfg.clone().into();
            let err_fn = |_e: cpal::StreamError| {};
            let st2 = st.clone();
            let stream = match cfg.sample_format() {
                cpal::SampleFormat::F32 => dev.build_input_stream(&sc, move |d: &[f32], _: &_| st2.lock().unwrap().push(d), err_fn, None),
                cpal::SampleFormat::I16 => dev.build_input_stream(&sc, move |d: &[i16], _: &_| {
                    let v: Vec<f32> = d.iter().map(|&x| x as f32 / 32768.0).collect();
                    st2.lock().unwrap().push(&v)
                }, err_fn, None),
                cpal::SampleFormat::U16 => dev.build_input_stream(&sc, move |d: &[u16], _: &_| {
                    let v: Vec<f32> = d.iter().map(|&x| (x as f32 - 32768.0) / 32768.0).collect();
                    st2.lock().unwrap().push(&v)
                }, err_fn, None),
                _ => return fail(&ready_tx),
            };
            let Ok(stream) = stream else { return fail(&ready_tx) };
            st.lock().unwrap().start = Instant::now();
            let t_start = epoch_secs();
            if stream.play().is_err() {
                return fail(&ready_tx);
            }
            let _ = ready_tx.send(true);
            let _ = stop_rx.recv();
            drop(stream);
            st.lock().unwrap().finish();
            Some((path, t_start))
        });
        match ready_rx.recv_timeout(Duration::from_secs(4)) {
            Ok(true) => Some(AudioRec { stop_tx, handle }),
            _ => None,
        }
    }

    fn stop(self) -> Option<(PathBuf, f64)> {
        let _ = self.stop_tx.send(());
        self.handle.join().ok().flatten()
    }
}
