use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

// ─── Helper: spawn child processes without flashing a console window. ──────
// On Windows, `Command::new` will pop up a brief cmd window for any console
// app (ffmpeg, ffprobe, wmic, nvidia-smi, taskkill, yt-dlp, …) — visible to
// the user. CREATE_NO_WINDOW (0x08000000) suppresses that. No-op on unix.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[inline]
fn cmd_hidden(program: &str) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

// ─── Bundled-tool resolution ───────────────────────────────────────────────
// ffmpeg / ffprobe / yt-dlp are not bundled in the installer. If the user lets
// the app fetch them (welcome setup check / "Install" buttons), they land in
// %APPDATA%\com.zipeek.nibble\bin and we run them from there. Otherwise we
// fall back to a bare name so a system/PATH install still works.

/// Must match `identifier` in tauri.conf.json (Tauri derives app_data_dir from it).
const APP_ID: &str = "com.zipeek.nibble";

fn roaming_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// Portable mode: the exe's file name contains "portable"
/// (`Nibble-Portable-v1.0.0.exe`), or a `portable` / `portable.txt` marker file
/// sits next to it. Then EVERYTHING — history, settings, custom presets, the
/// auto-installed ffmpeg/yt-dlp and the WebView2 profile — lives in
/// "<exe folder>\Nibble Data", so the app can run from a USB stick and leaves
/// nothing in %APPDATA% / %LOCALAPPDATA%.
fn portable_root() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.to_path_buf();
        let name = exe.file_stem()?.to_string_lossy().to_ascii_lowercase();
        let marked = dir.join("portable").exists() || dir.join("portable.txt").exists();
        (name.contains("portable") || marked).then(|| dir.join("Nibble Data"))
    })
    .clone()
}

/// Folder for history / watch config / custom presets.
fn data_dir(app: &AppHandle) -> PathBuf {
    let dir = portable_root().unwrap_or_else(|| {
        app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir())
    });
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn deps_dir() -> PathBuf {
    match portable_root() {
        Some(root) => root.join("bin"),
        None => roaming_dir().join(APP_ID).join("bin"),
    }
}

fn resolve_tool(name: &str) -> String {
    let p = deps_dir().join(format!("{}.exe", name));
    if p.exists() {
        p.to_string_lossy().into_owned()
    } else {
        name.to_string()
    }
}

/// Like `cmd_hidden`, but resolves ffmpeg/ffprobe/yt-dlp to the app-managed
/// copy in %APPDATA%\...\bin when present (else PATH).
#[inline]
fn cmd_tool(name: &str) -> Command {
    cmd_hidden(&resolve_tool(name))
}

mod capture;
mod record;
mod hdr;
mod presets;
use presets::{all_presets, find_preset, Preset};

// ---------- App state ----------

#[derive(Default)]
struct AppState {
    cancel: AtomicBool,
    /// PID of the currently-running ffmpeg child (or 0 if none). Used by
    /// `cancel_conversion` to actually terminate the running process rather
    /// than just wait for the next loop iteration.
    running_pid: AtomicU32,
    watcher: std::sync::Mutex<Option<notify::RecommendedWatcher>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct WatchConfig {
    enabled: bool,
    folder: String,
    preset_id: u32,
    output_dir: Option<String>,
}

fn watch_config_path(app: &AppHandle) -> PathBuf {
    data_dir(app).join("watch.json")
}

fn load_watch_config(app: &AppHandle) -> WatchConfig {
    let path = watch_config_path(app);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

fn save_watch_config(app: &AppHandle, cfg: &WatchConfig) {
    let path = watch_config_path(app);
    if let Ok(data) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(&path, data);
    }
}

// ---------- History ----------

#[derive(Serialize, Deserialize, Clone, Debug)]
struct HistoryEntry {
    when: String,
    preset: String,
    in_file: String,
    out_file: String,
    ok: bool,
    secs: f64,
    size_mb: f64,
    /// Full path of the result (Open / Show in folder in Recent Activity).
    /// Missing in entries written by older versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

fn history_path(app: &AppHandle) -> PathBuf {
    data_dir(app).join("history.json")
}

fn load_history(app: &AppHandle) -> Vec<HistoryEntry> {
    let path = history_path(app);
    if let Ok(data) = std::fs::read_to_string(&path) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn save_history(app: &AppHandle, hist: &[HistoryEntry]) {
    let path = history_path(app);
    if let Ok(data) = serde_json::to_string_pretty(hist) {
        let _ = std::fs::write(&path, data);
    }
}

/// Serializes the read-modify-write of history.json. Without this, the watch
/// thread and the convert batch can both load the same history, each append a
/// row, and the second write clobbers the first's entry. A process-wide lock
/// makes push_history atomic.
static HISTORY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn push_history(app: &AppHandle, entry: HistoryEntry) {
    let _guard = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = load_history(app);
    h.insert(0, entry);
    h.truncate(50);
    save_history(app, &h);
    // Downloads, the watcher and screenshots finish outside the page's own
    // flow — tell it to re-render Recent Activity.
    let _ = app.emit("history-changed", ());
}

// ---------- Events ----------

#[derive(Serialize, Clone)]
struct ProgressEvent {
    file_index: usize,
    total_files: usize,
    file_name: String,
    preset_name: String,
    frame: Option<u64>,
    fps: Option<f64>,
    speed: Option<f64>,
    time_ms: Option<u64>,
    total_ms: Option<u64>,
    bitrate: Option<String>,
}

/// Combined metadata probe: duration + HDR detection in a SINGLE ffprobe spawn.
/// Saves ~100–200 ms per file vs. two separate calls.
#[derive(Clone, Copy, Debug)]
struct ProbedMeta {
    duration_ms: Option<u64>,
    is_hdr: bool,
    fps: Option<f64>,
}

fn probe_metadata(path: &str) -> ProbedMeta {
    // Ask ffprobe for format.duration AND stream.color_transfer AND the stream
    // frame rate in one shot. avg_frame_rate is reported as a rational like
    // "30000/1001" (29.97) or "60/1" — we evaluate it to a float.
    let out = cmd_tool("ffprobe")
        .args(&[
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "format=duration:stream=color_transfer,avg_frame_rate",
            "-of", "default=noprint_wrappers=1",
            path,
        ])
        .output();

    let Ok(out) = out else {
        return ProbedMeta { duration_ms: None, is_hdr: false, fps: None };
    };

    let txt = String::from_utf8_lossy(&out.stdout);
    let mut duration_ms: Option<u64> = None;
    let mut is_hdr = false;
    let mut fps: Option<f64> = None;
    for line in txt.lines() {
        if let Some(v) = line.strip_prefix("duration=") {
            duration_ms = v.trim().parse::<f64>().ok().map(|d| (d * 1000.0) as u64);
        } else if let Some(v) = line.strip_prefix("color_transfer=") {
            let t = v.trim();
            if t == "smpte2084" || t == "arib-std-b67" {
                is_hdr = true;
            }
        } else if let Some(v) = line.strip_prefix("avg_frame_rate=") {
            fps = parse_rational_fps(v.trim());
        }
    }
    ProbedMeta { duration_ms, is_hdr, fps }
}

/// Evaluate ffprobe's rational frame-rate string ("30000/1001", "60/1", "0/0").
/// Returns None for the degenerate "0/0" (unknown) or unparsable input.
fn parse_rational_fps(s: &str) -> Option<f64> {
    if let Some((num, den)) = s.split_once('/') {
        let n: f64 = num.trim().parse().ok()?;
        let d: f64 = den.trim().parse().ok()?;
        if d > 0.0 && n > 0.0 {
            return Some(n / d);
        }
        None
    } else {
        s.parse::<f64>().ok().filter(|f| *f > 0.0)
    }
}


#[derive(Serialize, Clone)]
struct TrimThumbnailsResponse {
    duration_ms: u64,
    thumbnails: Vec<String>, // base64 data URLs
}

/// Transcode an HDR clip into a fast 720p H.264 proxy file with tonemap applied,
/// so the trim editor's <video> element shows correct SDR colors instead of a
/// washed-out HDR-as-SDR misinterpretation.
///
/// Cached by source path: if a proxy exists for the same input, we re-use it
/// (no rebuild). Returns the proxy file path which the frontend hands to
/// convertFileSrc() for asset-protocol loading.
///
/// Async wrapper — the actual work blocks (ffprobe + maybe ffmpeg proxy build),
/// so we hand it off to a worker thread instead of stalling the IPC handler.
/// Without this the UI freezes for a few seconds the first time a file is
/// dropped onto a trim preset.
#[tauri::command]
async fn ensure_hdr_proxy(path: String) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || ensure_hdr_proxy_blocking(path))
        .await
        .map_err(|e| format!("task join: {}", e))?
}

/// HDR preview proxies (720p SDR copies for the trim editor) kept at most.
const HDR_PROXY_KEEP: usize = 10;

/// Proxies unused for a week go, and only the newest `keep` stay — they are
/// full-length videos, so without this %TEMP% would slowly fill up. Runs at
/// startup and before each new proxy.
fn prune_hdr_proxies(keep: usize) {
    let dir = std::env::temp_dir().join("nibble_hdr_proxies");
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = rd
        .flatten()
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    files.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
    for (i, (p, t)) in files.iter().enumerate() {
        let old = t.elapsed().map_or(false, |age| age.as_secs() > 7 * 86_400);
        if old || i >= keep {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn ensure_hdr_proxy_blocking(path: String) -> Result<Option<String>, String> {
    // If the file is still being written (e.g. Shadowplay capture finishing),
    // wait briefly until its size stabilizes so we don't read half of it.
    wait_for_stable_file(std::path::Path::new(&path), 5, 100);

    let meta = probe_metadata(&path);
    if !meta.is_hdr {
        return Ok(None); // No proxy needed for SDR sources
    }
    let proxy_dir = std::env::temp_dir().join("nibble_hdr_proxies");
    let _ = std::fs::create_dir_all(&proxy_dir);

    // Cache key: hash of (path, mtime_ms) so editing the source invalidates
    let key = {
        let p = std::path::Path::new(&path);
        let meta = p.metadata();
        let mtime = meta.as_ref().ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        // Include size so an in-place edit (same mtime, different bytes) busts
        // the cache instead of reusing a stale proxy.
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        path.hash(&mut hasher);
        mtime.hash(&mut hasher);
        size.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    };
    let proxy_path = proxy_dir.join(format!("{}.mp4", key));
    if proxy_path.exists() {
        // Mark as recently used, so the cleanup keeps it
        if let Ok(f) = std::fs::File::options().write(true).open(&proxy_path) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        return Ok(Some(proxy_path.to_string_lossy().to_string()));
    }
    prune_hdr_proxies(HDR_PROXY_KEEP - 1); // room for the one about to be made

    let status = cmd_tool("ffmpeg")
        .args(&[
            "-hide_banner", "-loglevel", "error", "-y",
            "-i", &path,
            // Tonemap HDR → SDR + downscale to 720p height for fast scrubbing
            "-vf", "scale=-2:'min(720,ih)',zscale=tin=smpte2084:min=bt2020nc:pin=bt2020:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p",
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "26",
            "-c:a", "aac", "-b:a", "128k",
            "-movflags", "+faststart",
            proxy_path.to_str().ok_or("bad proxy path")?,
        ])
        .status()
        .map_err(|e| format!("ffmpeg spawn: {}", e))?;

    if !status.success() {
        return Err(format!("proxy generation failed: exit {:?}", status.code()));
    }
    Ok(Some(proxy_path.to_string_lossy().to_string()))
}

/// Async wrapper — extracting 25 thumbnails via ffmpeg takes seconds on long
/// videos. Doing it on the IPC thread would freeze the whole window. See
/// `ensure_hdr_proxy` for the same reasoning.
#[tauri::command]
async fn generate_trim_thumbnails(path: String, count: Option<u32>) -> Result<TrimThumbnailsResponse, String> {
    tauri::async_runtime::spawn_blocking(move || generate_trim_thumbnails_blocking(path, count))
        .await
        .map_err(|e| format!("task join: {}", e))?
}

fn generate_trim_thumbnails_blocking(path: String, count: Option<u32>) -> Result<TrimThumbnailsResponse, String> {
    use base64::Engine;
    let count = count.unwrap_or(25).max(2).min(60);
    // Wait for the file to stop growing before we probe — avoids reading mid-write
    wait_for_stable_file(std::path::Path::new(&path), 5, 100);
    // Single probe for both duration + HDR (saves a second ffprobe spawn)
    let meta = probe_metadata(&path);
    let duration_ms = meta.duration_ms.ok_or_else(|| "Cannot probe duration".to_string())?;
    let duration_s = (duration_ms as f64) / 1000.0;
    if duration_s <= 0.0 {
        return Err("Zero duration".into());
    }
    let period = (duration_s / count as f64).max(0.04);

    let temp_dir = std::env::temp_dir().join("nibble_trim_thumbs");
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
    let pattern = temp_dir.join("thumb_%03d.jpg");

    let vf = if meta.is_hdr {
        format!(
            "fps=1/{:.6},zscale=tin=smpte2084:min=bt2020nc:pin=bt2020:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709,format=yuv420p,scale=160:-1",
            period
        )
    } else {
        format!("fps=1/{:.6},scale=160:-1", period)
    };

    let pattern_str = pattern.to_str().ok_or("invalid thumbnail temp path")?;
    let count_str = count.to_string();
    // `keyframes_only` decodes just the keyframes (-skip_frame nokey) instead of
    // every frame of the file: ~10× faster on long captures (4-min 1080p60:
    // 7.8 s → 0.8 s) and the strip lands on the same evenly spaced timestamps.
    let run = |keyframes_only: bool| -> Result<(), String> {
        let mut args: Vec<&str> = vec!["-hide_banner", "-loglevel", "error", "-y"];
        if keyframes_only {
            args.extend(["-skip_frame", "nokey"]);
        }
        args.extend(["-i", &path, "-vf", &vf, "-frames:v", &count_str, "-q:v", "5", pattern_str]);
        let status = cmd_tool("ffmpeg")
            .args(&args)
            .status()
            .map_err(|e| format!("ffmpeg spawn: {}", e))?;
        if status.success() { Ok(()) } else { Err(format!("ffmpeg failed: exit {:?}", status.code())) }
    };
    let produced = || (1..=count).filter(|i| temp_dir.join(format!("thumb_{:03}.jpg", i)).exists()).count() as u32;

    // Fast path first; clips shorter than a GOP (few keyframes) can yield too
    // few frames — then fall back to the full decode.
    if run(true).is_err() || produced() < count / 2 {
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).map_err(|e| e.to_string())?;
        run(false)?;
    }

    let engine = base64::engine::general_purpose::STANDARD;
    let mut thumbs = Vec::new();
    for i in 1..=count {
        let p = temp_dir.join(format!("thumb_{:03}.jpg", i));
        if let Ok(data) = std::fs::read(&p) {
            thumbs.push(format!("data:image/jpeg;base64,{}", engine.encode(&data)));
        }
    }

    Ok(TrimThumbnailsResponse { duration_ms, thumbnails: thumbs })
}

// ============================================================
//  yt-dlp integration (download from YouTube/Twitter/Reddit/...)
// ============================================================

#[derive(Serialize, Clone)]
struct YtDlpStatus {
    available: bool,
    version: Option<String>,
    /// True when yt-dlp is the app-managed copy in %APPDATA%\…\bin (installed via
    /// "Install automatically"). Determines how an update is applied: re-download
    /// the latest exe for a managed copy, or `yt-dlp -U` for a PATH/standalone one.
    managed: bool,
}

#[derive(Serialize, Clone)]
struct YtDlpInfo {
    title: String,
    thumbnail: Option<String>,
    duration_s: Option<f64>,
    uploader: Option<String>,
    site: Option<String>,
    /// Which cookies source actually worked (None = no cookies). The UI reuses
    /// it for the preview and the download so all three agree.
    cookies_used: Option<String>,
}

// ---------- Cookies: "only when needed" ----------
//
// Logged-in YouTube sessions now require a JavaScript challenge solver that
// yt-dlp doesn't ship with, so passing browser cookies can BREAK public videos
// ("The page needs to be reloaded") that work fine anonymously. Cookies are
// only worth it for private / age-gated / members-only content. So:
//   * "auto" (default): try without cookies; on an auth-type error, try each
//     installed browser until one works.
//   * an explicit browser that fails: retry without cookies.

/// Browsers whose cookie store exists on this machine, best candidates first.
/// Firefox leads: yt-dlp can't decrypt Chrome/Edge cookies protected by
/// app-bound encryption on current Windows builds.
fn installed_cookie_browsers() -> Vec<&'static str> {
    let appdata = std::env::var("APPDATA").map(PathBuf::from).unwrap_or_default();
    let local = std::env::var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
    let candidates: [(&str, PathBuf); 6] = [
        ("firefox", appdata.join("Mozilla").join("Firefox").join("Profiles")),
        ("brave",   local.join("BraveSoftware").join("Brave-Browser").join("User Data")),
        ("vivaldi", local.join("Vivaldi").join("User Data")),
        ("opera",   appdata.join("Opera Software").join("Opera Stable")),
        ("edge",    local.join("Microsoft").join("Edge").join("User Data")),
        ("chrome",  local.join("Google").join("Chrome").join("User Data")),
    ];
    candidates.into_iter().filter(|(_, p)| p.exists()).map(|(b, _)| b).collect()
}

/// Whether a failed anonymous attempt is worth retrying with browser cookies.
/// Sites rarely say "log in" outright — X reports a sensitive / followers-only
/// tweet as "No video could be found", YouTube a restricted one as "This video
/// is unavailable" — so retry on everything except errors cookies can't fix.
fn looks_like_auth_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    !["unsupported url", "is not a valid url", "yt-dlp spawn", "getaddrinfo failed",
      "no such host", "timed out"]
        .iter()
        .any(|k| e.contains(k))
}

/// Run `attempt` with the best cookies choice. Returns the result plus the
/// cookies source that produced it.
fn with_cookie_fallback<T>(
    choice: Option<String>,
    attempt: impl Fn(Option<&str>) -> Result<T, String>,
) -> Result<(T, Option<String>), String> {
    let choice = choice.filter(|s| !s.trim().is_empty() && s != "auto");
    match choice {
        Some(browser) => match attempt(Some(&browser)) {
            Ok(v) => Ok((v, Some(browser))),
            // The chosen browser's session broke it — public content usually
            // works anonymously, so give that a go before reporting the error.
            Err(e) => attempt(None).map(|v| (v, None)).map_err(|_| e),
        },
        None => match attempt(None) {
            Ok(v) => Ok((v, None)),
            Err(e) if looks_like_auth_error(&e) => {
                for b in installed_cookie_browsers() {
                    if let Ok(v) = attempt(Some(b)) {
                        return Ok((v, Some(b.to_string())));
                    }
                }
                Err(e)
            }
            Err(e) => Err(e),
        },
    }
}

// Runs off the main thread: spawns a process / hits the network, which
// would otherwise freeze the window (sync commands run on the UI thread).
#[tauri::command(async)]
fn ytdlp_info(url: String, cookies_browser: Option<String>) -> Result<YtDlpInfo, String> {
    let (mut info, used) = with_cookie_fallback(cookies_browser, |c| ytdlp_info_once(&url, c))?;
    info.cookies_used = used;
    Ok(info)
}

fn ytdlp_info_once(url: &str, cookies_browser: Option<&str>) -> Result<YtDlpInfo, String> {
    let mut cmd = cmd_tool("yt-dlp");
    cmd.args(&[
        "--skip-download",
        "--no-playlist",
        "--no-warnings",
        "--no-update",
        "--no-check-certificate",
        "--encoding", "utf-8",
        "--print", "%(title)s||%(thumbnail)s||%(duration)s||%(uploader)s||%(extractor_key)s",
    ]);
    // Force Python's stdio to UTF-8 instead of the Windows system codepage
    // (defaults to cp1250 in CZ → corrupts non-ASCII titles like "předplatné").
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.env("PYTHONUTF8", "1");
    cmd.stdin(Stdio::null());
    if let Some(b) = cookies_browser.filter(|s| !s.is_empty() && *s != "auto") {
        cmd.arg("--cookies-from-browser").arg(b);
    }
    cmd.arg(&url);
    let out = cmd
        .output()
        .map_err(|e| format!("yt-dlp spawn: {}", e))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let trimmed = err.lines().filter(|l| !l.is_empty()).last().unwrap_or("yt-dlp failed");
        return Err(trimmed.to_string());
    }

    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let parts: Vec<&str> = line.split("||").collect();
    if parts.is_empty() {
        return Err("no info returned".into());
    }
    let clean = |s: &&str| {
        let t = s.trim();
        if t.is_empty() || t == "NA" { None } else { Some(t.to_string()) }
    };
    Ok(YtDlpInfo {
        title: parts.first().map(|s| s.to_string()).unwrap_or_default(),
        thumbnail: parts.get(1).and_then(clean),
        duration_s: parts.get(2).and_then(|s| s.trim().parse::<f64>().ok()),
        uploader: parts.get(3).and_then(clean),
        site: parts.get(4).and_then(clean),
        cookies_used: None,
    })
}

#[derive(Serialize, Clone)]
struct YtDlpPreview {
    /// `nstream` proxy id of a directly-playable media file for the trim
    /// editor, or None when the site only offers HLS/DASH manifests the webview
    /// can't scrub. Falls back to manual start/end entry in that case.
    stream_id: Option<String>,
    /// Separate audio track to play alongside a video-only stream (YouTube no
    /// longer serves any muxed format). None when the stream has its own audio.
    audio_id: Option<String>,
    duration_s: Option<f64>,
}

// Runs off the main thread: spawns a process / hits the network, which
// would otherwise freeze the window (sync commands run on the UI thread).
#[tauri::command(async)]
fn ytdlp_preview(url: String, cookies_browser: Option<String>) -> Result<YtDlpPreview, String> {
    with_cookie_fallback(cookies_browser, |c| ytdlp_preview_once(&url, c)).map(|(p, _)| p)
}

fn ytdlp_preview_once(url: &str, cookies_browser: Option<&str>) -> Result<YtDlpPreview, String> {
    // Resolve directly-playable stream URLs so the webview <video> can scrub
    // them for the trim editor — WITHOUT downloading anything. The webview
    // can't play HLS (.m3u8) or DASH manifests, so only plain HTTP(S) files.
    // The preview prefers ≤360p regardless of the chosen download quality —
    // plenty for picking an in/out point; the real download uses the chosen quality.
    //   1. a muxed file (X/Twitter, Reddit, …). Plain `b` + protocol filters:
    //      X leaves its direct MP4s' codecs unknown, so `[vcodec!=none]` would
    //      wrongly reject them and fall through to HLS.
    //   2. a video-only + audio-only pair (YouTube) — the UI plays the audio
    //      in sync next to the video.
    let mut cmd = cmd_tool("yt-dlp");
    cmd.args(&[
        "--no-playlist",
        "--no-warnings",
        "--no-update",
        "--no-check-certificate",
        "--encoding", "utf-8",
        // Smallest sensible size first, whatever quality the download uses:
        // the preview box is ~600 px wide, so 360p looks sharp there and
        // loads/seeks ~3× faster than 720p. Larger only when nothing ≤360p exists.
        "-f", "b[protocol^=http][protocol!*=dash][ext=mp4][height<=360]\
               /b[protocol^=http][protocol!*=dash][ext=mp4][height<=480]\
               /b[protocol^=http][protocol!*=dash][ext=mp4][height<=720]\
               /b[protocol^=http][protocol!*=dash][ext=mp4]\
               /bv[protocol=https][ext=mp4][vcodec^=avc1][height<=360]+ba[protocol=https][ext=m4a]\
               /bv[protocol=https][ext=mp4][vcodec^=avc1][height<=480]+ba[protocol=https][ext=m4a]\
               /bv[protocol=https][ext=mp4][height<=720]+ba[protocol=https][ext=m4a]\
               /b[protocol^=http][protocol!*=dash]\
               /bv[protocol^=http][protocol!*=dash][height<=720]\
               /b",
        "--print", "%(duration)s||%(protocol)s||%(url)s||%(requested_formats.0.url)s||%(requested_formats.1.url)s",
    ]);
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.env("PYTHONUTF8", "1");
    cmd.stdin(Stdio::null());
    if let Some(b) = cookies_browser.filter(|s| !s.is_empty() && *s != "auto") {
        cmd.arg("--cookies-from-browser").arg(b);
    }
    cmd.arg(url);
    let out = cmd.output().map_err(|e| format!("yt-dlp spawn: {}", e))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let trimmed = err.lines().filter(|l| !l.is_empty()).last().unwrap_or("yt-dlp failed");
        return Err(trimmed.to_string());
    }

    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    // "duration||protocol||url||video url||audio url" (the last two only for
    // a merged video+audio pair, where `url` itself is NA)
    let parts: Vec<&str> = line.split("||").map(str::trim).collect();
    let duration_s = parts.first().and_then(|d| d.parse::<f64>().ok());
    let protocol = parts.get(1).unwrap_or(&"").to_ascii_lowercase();
    let http = |s: Option<&&str>| s.filter(|u| u.starts_with("http") && !u.contains(".m3u8")).map(|u| u.to_string());
    // Only hand the webview something it can actually play. If the only option
    // was a streaming manifest, report "no preview" so the UI switches straight
    // to manual Start/End entry instead of showing a broken player.
    if protocol.contains("m3u8") || protocol.contains("dash") {
        return Ok(YtDlpPreview { stream_id: None, audio_id: None, duration_s });
    }
    let (video, audio) = match http(parts.get(2)) {
        Some(single) => (Some(single), None),
        None => (http(parts.get(3)), http(parts.get(4))),
    };
    Ok(YtDlpPreview {
        stream_id: video.as_deref().map(register_preview_stream),
        audio_id: audio.as_deref().map(register_preview_stream),
        duration_s,
    })
}

// ---------- Live-preview stream proxy ----------
//
// The trim preview doesn't point <video> at the site's CDN directly — it goes
// through the `nstream` URI scheme, served here:
//   * YouTube throttles open-ended range requests ("bytes=N-", which is all
//     Chromium's media loader sends) to roughly real-time speed, so playback
//     stalls and seeking crawls. Bounded ranges come back at full speed, so
//     the proxy reads the file in bounded blocks (see preview_block) and the
//     player simply asks for the next piece.
//   * X/Twitter's CDN answers 403 to any request with a foreign Referer — the
//     webview always sends one; this proxy doesn't.

const PREVIEW_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

fn preview_streams() -> &'static std::sync::Mutex<HashMap<String, String>> {
    static M: std::sync::OnceLock<std::sync::Mutex<HashMap<String, String>>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn register_preview_stream(url: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    let id = format!("s{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let mut streams = preview_streams().lock().unwrap();
    // Only the latest preview or two matter; old CDN URLs expire anyway.
    if streams.len() >= 8 {
        streams.clear();
    }
    streams.insert(id.clone(), url.to_string());
    id
}

/// Shared HTTP agent: keeps TLS connections to the CDN alive between range
/// requests (a fresh handshake per chunk cost ~0.2–0.4 s per seek).
fn preview_agent() -> &'static ureq::Agent {
    static A: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    A.get_or_init(|| {
        ureq::AgentBuilder::new()
            .user_agent(PREVIEW_UA)
            .timeout_connect(std::time::Duration::from_secs(10))
            .timeout_read(std::time::Duration::from_secs(30))
            .max_idle_connections_per_host(8)
            .build()
    })
}

/// Preview data is fetched in aligned PREVIEW_BLOCK pieces and kept in a small
/// in-memory cache: scrubbing back and forth, the separate audio track and
/// Chromium re-reading the same ranges all hit memory instead of the network,
/// and the next blocks are read ahead so playback doesn't stall.
// 512 KB ≈ 18 s of a 360p preview: small enough that a seek shows its first
// frame quickly, big enough that playback needs few requests.
const PREVIEW_BLOCK: u64 = 512 * 1024;
const PREVIEW_CACHE_BLOCKS: usize = 192;  // ≈ 96 MB

#[derive(Default)]
struct PreviewCache {
    blocks: HashMap<(String, u64), std::sync::Arc<Vec<u8>>>,
    order: std::collections::VecDeque<(String, u64)>,
    meta: HashMap<String, (u64, String)>,   // id → (total size, content type)
    inflight: std::collections::HashSet<(String, u64)>,
}

fn preview_cache() -> &'static (std::sync::Mutex<PreviewCache>, std::sync::Condvar) {
    static C: std::sync::OnceLock<(std::sync::Mutex<PreviewCache>, std::sync::Condvar)> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// Block `idx` of stream `id` (from cache, or fetched once even when several
/// requests want it at the same time). Err = upstream HTTP status / 502.
fn preview_block(id: &str, url: &str, idx: u64) -> Result<std::sync::Arc<Vec<u8>>, u16> {
    let key = (id.to_string(), idx);
    let (lock, cv) = preview_cache();
    {
        let mut c = lock.lock().unwrap();
        loop {
            if let Some(b) = c.blocks.get(&key) {
                return Ok(b.clone());
            }
            if !c.inflight.contains(&key) {
                c.inflight.insert(key.clone());
                break;
            }
            c = cv.wait(c).unwrap();   // someone else is fetching it
        }
    }
    let start = idx * PREVIEW_BLOCK;
    let result = (|| {
        let resp = match preview_agent()
            .get(url)
            .set("Range", &format!("bytes={}-{}", start, start + PREVIEW_BLOCK - 1))
            .call()
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Err(code),
            Err(_) => return Err(502),
        };
        if resp.status() != 206 {
            return Err(502);   // server ignores ranges — not usable as blocks
        }
        let total = resp
            .header("Content-Range")
            .and_then(|cr| cr.rsplit('/').next())
            .and_then(|t| t.trim().parse::<u64>().ok());
        let ctype = resp.header("Content-Type").unwrap_or("video/mp4").to_string();
        let mut body = Vec::with_capacity(PREVIEW_BLOCK as usize);
        resp.into_reader().take(PREVIEW_BLOCK).read_to_end(&mut body).map_err(|_| 502u16)?;
        Ok((std::sync::Arc::new(body), total, ctype))
    })();
    let mut c = lock.lock().unwrap();
    c.inflight.remove(&key);
    cv.notify_all();
    let (body, total, ctype) = result?;
    if let Some(t) = total {
        c.meta.insert(id.to_string(), (t, ctype));
    }
    c.blocks.insert(key.clone(), body.clone());
    c.order.push_back(key);
    while c.order.len() > PREVIEW_CACHE_BLOCKS {
        if let Some(old) = c.order.pop_front() {
            c.blocks.remove(&old);
        }
    }
    Ok(body)
}

fn serve_preview_stream(req: &tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::{header, Response};
    let fail = |code: u16| {
        Response::builder()
            .status(code)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(Vec::new())
            .unwrap()
    };
    let id = req.uri().path().trim_matches('/').to_string();
    let Some(upstream) = preview_streams().lock().unwrap().get(&id).cloned() else {
        return fail(404);
    };
    // "bytes=START-" or "bytes=START-END"; no header = from the start.
    let (start, want_end) = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().strip_prefix("bytes="))
        .and_then(|r| {
            let (a, b) = r.split_once('-')?;
            Some((a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()))
        })
        .unwrap_or((0, None));

    let idx = start / PREVIEW_BLOCK;
    let block = match preview_block(&id, &upstream, idx) {
        Ok(b) => b,
        Err(code) => return fail(code),
    };
    let Some((total, ctype)) = preview_cache().0.lock().unwrap().meta.get(&id).cloned() else {
        return fail(502);
    };
    if start >= total {
        return Response::builder().status(416)
            .header(header::CONTENT_RANGE, format!("bytes */{}", total))
            .body(Vec::new()).unwrap();
    }
    // Answer with the rest of this block (Chromium asks for the next part itself).
    let block_start = idx * PREVIEW_BLOCK;
    let block_end = block_start + block.len() as u64 - 1;
    let end = want_end.unwrap_or(u64::MAX).min(block_end).min(total - 1);
    let body = block[(start - block_start) as usize..=(end - block_start) as usize].to_vec();

    // Read ahead the next blocks in the background.
    for next in [idx + 1, idx + 2, idx + 3] {
        if next * PREVIEW_BLOCK < total {
            let (id, url) = (id.clone(), upstream.clone());
            let cached = preview_cache().0.lock().unwrap().blocks.contains_key(&(id.clone(), next));
            if !cached {
                std::thread::spawn(move || { let _ = preview_block(&id, &url, next); });
            }
        }
    }

    Response::builder()
        .status(206)
        .header(header::CONTENT_TYPE, ctype)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CONTENT_LENGTH, body.len())
        .header(header::CONTENT_RANGE, format!("bytes {}-{}/{}", start, end, total))
        .body(body)
        .unwrap_or_else(|_| fail(502))
}

// Runs off the main thread: spawns a process / hits the network, which
// would otherwise freeze the window (sync commands run on the UI thread).
#[tauri::command(async)]
fn reveal_in_explorer(path: String) -> Result<(), String> {
    use std::path::Path;

    let p = Path::new(&path);

    // Strategy: try /select to highlight the file. If that fails (or the file no
    // longer exists), fall back to just opening the parent folder.
    //
    // The /select syntax is finicky with Rust's auto-quoting on Windows — wrapping
    // the path in literal double quotes via raw_arg sidesteps it.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        if p.exists() {
            let arg = format!("/select,\"{}\"", p.display());
            let mut cmd = Command::new("explorer.exe");
            cmd.raw_arg(&arg);
            if cmd.spawn().is_ok() {
                return Ok(());
            }
        }
    }

    // Fallback: open the parent folder (no file highlight)
    let target = if p.is_dir() {
        p.to_path_buf()
    } else {
        p.parent()
            .ok_or_else(|| "No parent folder".to_string())?
            .to_path_buf()
    };

    Command::new("explorer.exe")
        .arg(target.as_os_str())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Failed to launch explorer: {}", e))
}

// ============================================================
//  Hardware detection (one-time at startup; cheap shell-outs)
// ============================================================

#[derive(Serialize, Clone, Default)]
struct HardwareInfo {
    gpu_vendor: Option<String>,   // "NVIDIA" | "AMD" | "Intel" | None
    gpu_name: Option<String>,     // e.g. "NVIDIA GeForce RTX 5080"
    nvenc_available: bool,
    nvenc_av1: bool,              // AV1 encode (RTX 40+)
    cpu_name: Option<String>,
    cpu_cores: Option<u32>,
    cpu_threads: Option<u32>,
    ram_gb: Option<u32>,          // installed (sum of modules)
    ram_type: Option<String>,     // "DDR5"
    ram_mts: Option<u32>,         // configured speed, MT/s
    ram_sticks: Option<u32>,
    os_name: Option<String>,      // "Windows 11 Pro 25H2 (build 26300)"
    user_name: Option<String>,    // the signed-in user's display name
    /// Encoder family the presets actually use: "nvenc" | "amf" | "qsv" | "cpu"
    encoder: String,
    /// Whether ffmpeg was available to check the encoders at all
    ffmpeg_found: bool,
}

/// CPU, RAM and OS from one PowerShell/CIM call.
struct SystemProbe {
    cpu_name: Option<String>,
    cpu_cores: Option<u32>,
    cpu_threads: Option<u32>,
    ram_gb: Option<u32>,
    ram_type: Option<String>,
    ram_mts: Option<u32>,
    ram_sticks: Option<u32>,
    os_name: Option<String>,
}

fn detect_gpu() -> (Option<String>, Option<String>, bool, bool) {
    // Try nvidia-smi first (most reliable for NVIDIA)
    if let Ok(out) = cmd_hidden("nvidia-smi")
        .args(&["--query-gpu=name", "--format=csv,noheader"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(name) = s.lines().next().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()) {
                // Parse generation from name to flag AV1 NVENC capability
                let upper = name.to_uppercase();
                let nvenc_av1 = upper.contains("RTX 40")
                    || upper.contains("RTX 50")
                    || upper.contains("RTX 60")
                    || upper.contains("RTX 70");
                return (Some("NVIDIA".into()), Some(name), true, nvenc_av1);
            }
        }
    }
    // Fallback: CIM video-controller query for any GPU (AMD/Intel/etc.).
    // (Replaces the old `wmic` call, which is absent on Windows 11 24H2+ and left
    //  AMD/Intel-only machines with NO detected GPU — forcing slow CPU encoding.)
    // We pick the first controller whose name maps to a real vendor, skipping
    // virtual/basic adapters (Meta Virtual Monitor, Microsoft Basic Display, …).
    #[cfg(windows)]
    {
        let script = "\
$ErrorActionPreference='SilentlyContinue';\
Get-CimInstance Win32_VideoController|ForEach-Object { Write-Output (\"NAME=\"+$_.Name) }";
        if let Ok(out) = cmd_hidden("powershell")
            .args([
                "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                "-Command", script,
            ])
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout);
            let mut fallback: Option<(String, String)> = None; // (vendor, name) for "Unknown"
            for line in s.lines() {
                let name = match line.trim().strip_prefix("NAME=") {
                    Some(v) => v.trim(),
                    None => continue,
                };
                if name.is_empty() { continue; }
                let upper = name.to_uppercase();
                let vendor = if upper.contains("NVIDIA") || upper.contains("GEFORCE") { "NVIDIA" }
                             else if upper.contains("AMD") || upper.contains("RADEON") { "AMD" }
                             else if upper.contains("INTEL") { "Intel" }
                             else { "Unknown" };
                if vendor != "Unknown" {
                    let nvenc = vendor == "NVIDIA";
                    return (Some(vendor.into()), Some(name.to_string()), nvenc, false);
                }
                // Remember the first unrecognized adapter only as a last resort.
                fallback.get_or_insert_with(|| ("Unknown".into(), name.to_string()));
            }
            if let Some((vendor, name)) = fallback {
                return (Some(vendor), Some(name), false, false);
            }
        }
    }
    (None, None, false, false)
}

// ============================================================
//  Hardware-adaptive video encoder selection
//
//  Presets reference encoders through `{VENC:<codec>:<q>}` tokens instead of
//  hardcoding `hevc_nvenc`. At build time we resolve the token to whatever the
//  user's machine actually has — NVENC on NVIDIA, AMF on AMD, QSV on Intel, and
//  libx264/libx265 (CPU) as the universal fallback. This is what makes the app
//  work for non-NVIDIA users instead of failing with "unknown encoder".
// ============================================================

/// Set of encoder names ffmpeg reports via `-encoders`, cached once.
fn ffmpeg_encoders() -> &'static std::collections::HashSet<String> {
    use std::sync::OnceLock;
    static ENCODERS: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    ENCODERS.get_or_init(|| {
        let mut set = std::collections::HashSet::new();
        if let Ok(out) = cmd_tool("ffmpeg").args(&["-hide_banner", "-encoders"]).output() {
            let txt = String::from_utf8_lossy(&out.stdout);
            for line in txt.lines() {
                // Rows look like: " V....D hevc_nvenc  NVIDIA NVENC hevc encoder"
                // The 6-char flag column is followed by the encoder name.
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[0].len() == 6 && parts[0].starts_with('V') {
                    set.insert(parts[1].to_string());
                }
            }
        }
        set
    })
}

#[derive(Clone, Copy, PartialEq)]
enum GpuVendor { Nvidia, Amd, Intel, None }

/// GPU detection spawns nvidia-smi and/or PowerShell (~0.3–1 s). Do it once per
/// app run and share the result between the welcome screen and the encoder.
fn detect_gpu_cached() -> (Option<String>, Option<String>, bool, bool) {
    use std::sync::OnceLock;
    static G: OnceLock<(Option<String>, Option<String>, bool, bool)> = OnceLock::new();
    G.get_or_init(detect_gpu).clone()
}

fn gpu_vendor_cached() -> GpuVendor {
    use std::sync::OnceLock;
    static V: OnceLock<u8> = OnceLock::new();
    let code = *V.get_or_init(|| match detect_gpu_cached().0.as_deref() {
        Some("NVIDIA") => 0,
        Some("AMD") => 1,
        Some("Intel") => 2,
        _ => 3,
    });
    match code {
        0 => GpuVendor::Nvidia,
        1 => GpuVendor::Amd,
        2 => GpuVendor::Intel,
        _ => GpuVendor::None,
    }
}

/// Build the full `-c:v … (quality)` argument list for a codec family, picking
/// the best encoder the machine can actually run. `q` is a 0–51-ish quality
/// number interpreted per-encoder (CQ / CRF / global_quality / QP) — lower is
/// higher quality. HEVC outputs also get `-tag:v hvc1` for QuickTime/AE.
/// Which encoder family a codec ("hevc" / "h264") runs on: "nvenc" | "amf" |
/// "qsv" | "cpu". GPU vendor + what this ffmpeg build actually supports.
fn encoder_backend(codec: &str) -> &'static str {
    let enc = ffmpeg_encoders();
    let has = |name: String| enc.is_empty() || enc.contains(&name); // empty set = probe failed → trust vendor
    match gpu_vendor_cached() {
        GpuVendor::Nvidia if has(format!("{}_nvenc", codec)) => "nvenc",
        GpuVendor::Amd if has(format!("{}_amf", codec)) => "amf",
        GpuVendor::Intel if has(format!("{}_qsv", codec)) => "qsv",
        _ => "cpu",
    }
}

fn encoder_args(codec: &str, q: u32) -> Vec<String> {

    let backend = encoder_backend(codec);
    let qs = q.to_string();
    let mut v: Vec<String> = Vec::new();
    let mut push = |args: &[&str]| v.extend(args.iter().map(|s| s.to_string()));

    match (codec, backend) {
        ("hevc", "nvenc") => push(&["-c:v","hevc_nvenc","-preset","p7","-tune","hq","-rc","vbr","-cq",&qs,"-b:v","0","-spatial_aq","1","-temporal_aq","1"]),
        ("hevc", "amf")   => push(&["-c:v","hevc_amf","-quality","quality","-rc","cqp","-qp_i",&qs,"-qp_p",&qs]),
        ("hevc", "qsv")   => push(&["-c:v","hevc_qsv","-preset","veryslow","-global_quality",&qs]),
        ("hevc", _)       => push(&["-c:v","libx265","-preset","medium","-crf",&qs]),
        ("h264", "nvenc") => push(&["-c:v","h264_nvenc","-preset","p7","-tune","hq","-rc","vbr","-cq",&qs,"-b:v","0","-spatial_aq","1"]),
        ("h264", "amf")   => push(&["-c:v","h264_amf","-quality","quality","-rc","cqp","-qp_i",&qs,"-qp_p",&qs]),
        ("h264", "qsv")   => push(&["-c:v","h264_qsv","-preset","veryslow","-global_quality",&qs]),
        ("h264", _)       => push(&["-c:v","libx264","-preset","medium","-crf",&qs]),
        _                 => push(&["-c:v","libx264","-preset","medium","-crf",&qs]),
    }
    if codec == "hevc" {
        push(&["-tag:v", "hvc1"]);
    }
    v
}

/// Decode-acceleration args. `-hwaccel auto` lets ffmpeg pick the best decoder
/// the platform offers (CUDA/NVDEC on NVIDIA, D3D11VA/DXVA2 on AMD/Intel) and
/// — crucially — silently falls back to software decode if the codec isn't
/// hardware-decodable, instead of erroring. Unknown/no GPU → plain CPU decode.
fn hwdecode_args() -> Vec<String> {
    match gpu_vendor_cached() {
        GpuVendor::None => vec![],
        _ => vec!["-hwaccel".into(), "auto".into()],
    }
}

/// The signed-in user's display name as Windows shows it ("ZiP BURTOWSKI"),
/// falling back to the account name.
fn current_user_name() -> Option<String> {
    #[cfg(windows)]
    {
        use windows::core::PWSTR;
        use windows::Win32::Security::Authentication::Identity::{GetUserNameExW, NameDisplay};
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        let ok = unsafe { GetUserNameExW(NameDisplay, Some(PWSTR(buf.as_mut_ptr())), &mut len) };
        if ok {
            let name = String::from_utf16_lossy(&buf[..len as usize]).trim().to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    std::env::var("USERNAME").ok().filter(|s| !s.trim().is_empty())
}

fn detect_system() -> SystemProbe {
    let mut p = SystemProbe { cpu_name: None, cpu_cores: None, cpu_threads: None, ram_gb: None, ram_type: None, ram_mts: None, ram_sticks: None, os_name: None };
    #[cfg(windows)]
    {
        // Single PowerShell/CIM call for CPU, RAM and OS.
        // (We used to shell out to `wmic`, but Windows 11 24H2+ ships without it
        //  — the calls silently failed, so the UI showed no CPU/RAM. CIM is the
        //  supported replacement and is present on every modern Windows.)
        // OS: the CIM caption ("Microsoft Windows 11 Pro") + the feature update
        // ("25H2") and build from the registry, which CIM doesn't expose.
        let script = "\
$ErrorActionPreference='SilentlyContinue';\
[Console]::OutputEncoding=[Text.Encoding]::UTF8;\
$c=Get-CimInstance Win32_Processor|Select-Object -First 1;\
$cs=Get-CimInstance Win32_ComputerSystem;\
$os=Get-CimInstance Win32_OperatingSystem;\
$cv=Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion';\
Write-Output (\"NAME=\"+$c.Name);\
Write-Output (\"CORES=\"+$c.NumberOfCores);\
Write-Output (\"THREADS=\"+$c.NumberOfLogicalProcessors);\
Write-Output (\"RAM=\"+$cs.TotalPhysicalMemory);\
$m=@(Get-CimInstance Win32_PhysicalMemory);\
Write-Output (\"RAMINST=\"+($m|Measure-Object -Property Capacity -Sum).Sum);\
Write-Output (\"RAMSTICKS=\"+$m.Count);\
Write-Output (\"RAMTYPE=\"+($m|Select-Object -First 1).SMBIOSMemoryType);\
Write-Output (\"RAMSPEED=\"+($m|Select-Object -First 1).ConfiguredClockSpeed);\
Write-Output (\"OS=\"+$os.Caption);\
Write-Output (\"OSVER=\"+$cv.DisplayVersion);\
Write-Output (\"OSBUILD=\"+$cv.CurrentBuild)";
        let out = cmd_hidden("powershell")
            .args([
                "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                "-Command", script,
            ])
            .output();
        let (mut os, mut osver, mut osbuild) = (String::new(), String::new(), String::new());
        if let Ok(o) = out {
            let s = String::from_utf8_lossy(&o.stdout);
            for line in s.lines() {
                let line = line.trim();
                let Some((k, v)) = line.split_once('=') else { continue };
                let v = v.trim();
                match k {
                    "NAME" if !v.is_empty() => p.cpu_name = Some(v.to_string()),
                    "CORES" => p.cpu_cores = v.parse().ok(),
                    "THREADS" => p.cpu_threads = v.parse().ok(),
                    // Usable memory (minus what the iGPU / firmware reserve) —
                    // only a fallback; the installed total below is what
                    // CPU-Z and the box say.
                    "RAM" if p.ram_gb.is_none() => {
                        p.ram_gb = v.parse::<u64>().ok().map(|b| ((b as f64) / 1_073_741_824.0).round() as u32)
                    }
                    "RAMINST" => {
                        if let Some(gb) = v.parse::<u64>().ok().filter(|b| *b > 0) {
                            p.ram_gb = Some(((gb as f64) / 1_073_741_824.0).round() as u32);
                        }
                    }
                    "RAMSTICKS" => p.ram_sticks = v.parse().ok().filter(|n| *n > 0),
                    // SMBIOS memory type codes
                    "RAMTYPE" => {
                        p.ram_type = match v {
                            "20" => Some("DDR"), "21" => Some("DDR2"), "24" => Some("DDR3"),
                            "26" => Some("DDR4"), "34" => Some("DDR5"), "35" => Some("LPDDR5"),
                            _ => None,
                        }.map(String::from)
                    }
                    "RAMSPEED" => p.ram_mts = v.parse().ok().filter(|n| *n > 0),
                    "OS" => os = v.trim_start_matches("Microsoft ").to_string(),
                    "OSVER" => osver = v.to_string(),
                    "OSBUILD" => osbuild = v.to_string(),
                    _ => {}
                }
            }
        }
        if !os.is_empty() {
            let mut name = os;
            if !osver.is_empty() { name.push(' '); name.push_str(&osver); }
            if !osbuild.is_empty() { name.push_str(&format!(" (build {})", osbuild)); }
            p.os_name = Some(name);
        }
    }
    p
}

// Runs off the main thread: spawns a process / hits the network, which
// would otherwise freeze the window (sync commands run on the UI thread).
#[tauri::command(async)]
fn detect_hardware() -> HardwareInfo {
    // GPU and CPU/RAM probes are independent process spawns — run them in
    // parallel so startup waits for the slower one, not the sum.
    // The encoder check needs the GPU vendor, so it runs after it in the same
    // thread (ffmpeg -encoders is cached for later conversions anyway).
    let gpu_thread = std::thread::spawn(|| {
        let gpu = detect_gpu_cached();
        let encoder = encoder_backend("hevc");
        (gpu, encoder, !ffmpeg_encoders().is_empty())
    });
    let sys = detect_system();
    let ((gpu_vendor, gpu_name, nvenc_available, nvenc_av1), encoder, ffmpeg_found) =
        gpu_thread.join().unwrap_or(((None, None, false, false), "cpu", false));
    HardwareInfo {
        gpu_vendor,
        gpu_name,
        nvenc_available,
        nvenc_av1,
        cpu_name: sys.cpu_name,
        cpu_cores: sys.cpu_cores,
        cpu_threads: sys.cpu_threads,
        ram_gb: sys.ram_gb,
        ram_type: sys.ram_type,
        ram_mts: sys.ram_mts,
        ram_sticks: sys.ram_sticks,
        os_name: sys.os_name,
        user_name: current_user_name(),
        encoder: encoder.to_string(),
        ffmpeg_found,
    }
}

#[derive(Serialize, Clone)]
struct AppInfo {
    portable: bool,
    data_dir: String,
}

/// Lets the UI show whether this is the portable build and where data lives.
#[tauri::command]
fn app_info(app: AppHandle) -> AppInfo {
    AppInfo {
        portable: portable_root().is_some(),
        data_dir: data_dir(&app).to_string_lossy().into_owned(),
    }
}

/// True when we're running the app-managed yt-dlp copy (vs a PATH/system one).
fn ytdlp_is_managed() -> bool {
    deps_dir().join("yt-dlp.exe").exists()
}

// Runs off the main thread: spawns a process / hits the network, which
// would otherwise freeze the window (sync commands run on the UI thread).
#[tauri::command(async)]
fn ytdlp_status() -> YtDlpStatus {
    let managed = ytdlp_is_managed();
    let out = cmd_tool("yt-dlp").arg("--version").output();
    match out {
        Ok(o) if o.status.success() => YtDlpStatus {
            available: true,
            version: Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
            managed,
        },
        _ => YtDlpStatus { available: false, version: None, managed },
    }
}

/// Latest yt-dlp release tag from GitHub (e.g. "2026.08.19"), for the
/// "update available" check. Uses PowerShell (no extra Rust HTTP dependency).
// Runs off the main thread: spawns a process / hits the network, which
// would otherwise freeze the window (sync commands run on the UI thread).
#[tauri::command(async)]
fn ytdlp_latest() -> Result<String, String> {
    #[cfg(windows)]
    {
        let script = "\
$ProgressPreference='SilentlyContinue';\
$ErrorActionPreference='Stop';\
(Invoke-RestMethod -Uri 'https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest' -Headers @{ 'User-Agent'='Nibble' }).tag_name";
        let out = cmd_hidden("powershell")
            .args([
                "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                "-Command", script,
            ])
            .output()
            .map_err(|e| format!("powershell spawn: {}", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("github query failed").to_string());
        }
        let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if tag.is_empty() { return Err("empty tag".into()); }
        Ok(tag)
    }
    #[cfg(not(windows))]
    {
        Err("unsupported".into())
    }
}

/// Update yt-dlp. For the app-managed copy we re-download the latest exe from
/// GitHub (same source as the auto-installer). For a PATH/standalone install we
/// run `yt-dlp -U` so a user-managed binary self-updates in place. Returns the
/// new version string on success.
#[tauri::command]
async fn ytdlp_update() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(ytdlp_update_blocking)
        .await
        .map_err(|e| format!("task join: {}", e))?
}

#[cfg(windows)]
fn ytdlp_update_blocking() -> Result<String, String> {
    // Not installed anywhere → this is a fresh install into the app's bin
    // (also covers a portable copy on a PC that has ffmpeg but no yt-dlp).
    let missing = !matches!(cmd_tool("yt-dlp").arg("--version").output(), Ok(o) if o.status.success());
    if ytdlp_is_managed() || missing {
        // (Re-)download the latest standalone exe into the app-managed bin.
        let dir = deps_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("create bin dir: {}", e))?;
        let dir_ps = dir.to_string_lossy().replace('\'', "''");
        let script = "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri 'https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe' -OutFile '__DIR__\\yt-dlp.exe'"
            .replace("__DIR__", &dir_ps);
        run_powershell(&script).map_err(|e| format!("download failed: {}", e))?;
    } else {
        // PATH/standalone binary: let yt-dlp update itself in place.
        let out = cmd_tool("yt-dlp")
            .args(["-U", "--no-check-certificate"])
            .output()
            .map_err(|e| format!("yt-dlp spawn: {}", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let msg = err.lines().chain(stdout.lines())
                .filter(|l| !l.trim().is_empty())
                .last()
                .unwrap_or("yt-dlp -U failed");
            // Package-manager installs (pip/winget) can't self-update this way.
            return Err(format!("{} — if yt-dlp was installed via pip or winget, update it there instead.", msg));
        }
    }
    // Report the resulting version.
    let out = cmd_tool("yt-dlp").arg("--version").output()
        .map_err(|e| format!("version check: {}", e))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(not(windows))]
fn ytdlp_update_blocking() -> Result<String, String> {
    Err("Automatic yt-dlp update is only supported on Windows".into())
}

#[derive(Serialize, Clone)]
struct FfmpegStatus {
    ffmpeg: bool,
    ffprobe: bool,
    version: Option<String>,
}

/// ffmpeg + ffprobe are REQUIRED for everything the app does. New users may not
/// have them on PATH — this lets the frontend show clear install guidance
/// instead of letting every conversion fail with a cryptic spawn error.
// Runs off the main thread: spawns a process / hits the network, which
// would otherwise freeze the window (sync commands run on the UI thread).
#[tauri::command(async)]
fn ffmpeg_status() -> FfmpegStatus {
    let ff = cmd_tool("ffmpeg").arg("-version").output();
    let (ffmpeg, version) = match ff {
        Ok(o) if o.status.success() => {
            let txt = String::from_utf8_lossy(&o.stdout);
            // First line: "ffmpeg version 7.1 Copyright ..."
            let ver = txt.lines().next()
                .and_then(|l| l.strip_prefix("ffmpeg version "))
                .and_then(|l| l.split_whitespace().next())
                .map(|s| s.to_string());
            (true, ver)
        }
        _ => (false, None),
    };
    let ffprobe = matches!(
        cmd_tool("ffprobe").arg("-version").output(),
        Ok(o) if o.status.success()
    );
    FfmpegStatus { ffmpeg, ffprobe, version }
}

/// Run a PowerShell snippet hidden, returning stderr on failure. Windows-only
/// path used by the auto-installer (download + unzip via built-in cmdlets, no
/// extra Rust dependencies).
#[cfg(windows)]
fn run_powershell(script: &str) -> Result<(), String> {
    let out = cmd_hidden("powershell")
        .args([
            "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
            "-Command", script,
        ])
        .output()
        .map_err(|e| format!("powershell spawn: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("powershell failed").to_string())
    }
}

/// Download ffmpeg + ffprobe + yt-dlp into %APPDATA%\com.zipeek.nibble\bin so the
/// app works without the user installing anything manually. Emits "dep-progress"
/// events (stage strings) the UI can show.
#[tauri::command]
async fn install_dependencies(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || install_deps_blocking(app))
        .await
        .map_err(|e| format!("task join: {}", e))?
}

#[cfg(windows)]
fn install_deps_blocking(app: AppHandle) -> Result<(), String> {
    let dir = deps_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create bin dir: {}", e))?;
    // Escape single quotes for embedding in a PowerShell single-quoted string.
    let dir_ps = dir.to_string_lossy().replace('\'', "''");

    // 1. yt-dlp — single exe straight from GitHub releases.
    let _ = app.emit("dep-progress", "Downloading yt-dlp…");
    let yt = "$ProgressPreference='SilentlyContinue'; Invoke-WebRequest -Uri 'https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe' -OutFile '__DIR__\\yt-dlp.exe'"
        .replace("__DIR__", &dir_ps);
    run_powershell(&yt).map_err(|e| format!("yt-dlp download failed: {}", e))?;

    // 2. ffmpeg + ffprobe — gyan.dev 'essentials' zip (~110 MB), extract the
    //    two exes (the build folder is versioned, so glob for them).
    //    Skipped when a working ffmpeg is already there (e.g. on PATH).
    let have_ff = ffmpeg_status();
    if have_ff.ffmpeg && have_ff.ffprobe {
        let _ = app.emit("dep-progress", "Done");
        return Ok(());
    }
    let _ = app.emit("dep-progress", "Downloading ffmpeg (~110 MB, one-time)…");
    let ff = r#"
$ProgressPreference='SilentlyContinue'
$zip = Join-Path $env:TEMP 'cfm_ffmpeg.zip'
$ex  = Join-Path $env:TEMP 'cfm_ffmpeg_extract'
Invoke-WebRequest -Uri 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip' -OutFile $zip
if (Test-Path $ex) { Remove-Item -Recurse -Force $ex }
Expand-Archive -Path $zip -DestinationPath $ex -Force
$ff = Get-ChildItem -Path $ex -Recurse -Filter ffmpeg.exe  | Select-Object -First 1
$fp = Get-ChildItem -Path $ex -Recurse -Filter ffprobe.exe | Select-Object -First 1
if (-not $ff -or -not $fp) { throw 'ffmpeg.exe/ffprobe.exe not found in archive' }
Copy-Item $ff.FullName '__DIR__\ffmpeg.exe'  -Force
Copy-Item $fp.FullName '__DIR__\ffprobe.exe' -Force
Remove-Item -Force $zip
Remove-Item -Recurse -Force $ex
"#
    .replace("__DIR__", &dir_ps);
    let _ = app.emit("dep-progress", "Extracting ffmpeg…");
    run_powershell(&ff).map_err(|e| format!("ffmpeg install failed: {}", e))?;

    // Verify
    if !deps_dir().join("ffmpeg.exe").exists() || !deps_dir().join("ffprobe.exe").exists() {
        return Err("ffmpeg.exe / ffprobe.exe missing after install".into());
    }
    let _ = app.emit("dep-progress", "Done");
    Ok(())
}

#[cfg(not(windows))]
fn install_deps_blocking(_app: AppHandle) -> Result<(), String> {
    Err("Automatic install is only supported on Windows".into())
}

#[derive(Serialize, Clone)]
struct DownloadProgressEvent {
    percent: Option<f64>,
    speed: Option<String>,
    eta: Option<String>,
    size: Option<String>,
}

#[derive(Serialize, Clone)]
struct DownloadDoneEvent {
    ok: bool,
    file_path: Option<String>,
    file_name: Option<String>,
    error: Option<String>,
    /// Quick share: the file was put on the clipboard (and lives in the
    /// temporary share folder) instead of being saved to Downloads.
    shared: bool,
    size_mb: f64,
}

fn downloads_dir() -> PathBuf {
    if let Ok(user) = std::env::var("USERPROFILE") {
        let d = PathBuf::from(&user).join("Downloads");
        if d.exists() {
            return d;
        }
        return PathBuf::from(user);
    }
    std::env::temp_dir()
}

fn parse_ytdlp_progress(line: &str) -> Option<DownloadProgressEvent> {
    // Examples:
    //   [download]  50.2% of 100.00MiB at 2.50MiB/s ETA 00:20
    //   [download]   0.5% of ~  10.00MiB at 500.00KiB/s ETA 00:15
    let lower = line.trim_start();
    if !lower.starts_with("[download]") || !lower.contains('%') {
        return None;
    }
    // Percent: number directly before '%'
    let percent = lower
        .split('%')
        .next()
        .and_then(|s| s.rsplit_terminator(|c: char| c.is_whitespace()).next())
        .and_then(|s| s.parse::<f64>().ok());
    // Size: between "of" and "at"
    let size = lower
        .split(" of ").nth(1)
        .and_then(|s| s.split(" at ").next())
        .map(|s| s.trim_start_matches('~').trim().to_string());
    // Speed: between "at " and " ETA"
    let speed = lower
        .split(" at ").nth(1)
        .and_then(|s| s.split(" ETA ").next())
        .map(|s| s.trim().to_string());
    // ETA: after "ETA "
    let eta = lower
        .split(" ETA ").nth(1)
        .map(|s| s.trim().to_string());
    Some(DownloadProgressEvent { percent, speed, eta, size })
}

#[tauri::command]
async fn download_url(
    app: AppHandle,
    url: String,
    quality: String,
    output_dir: Option<String>,
    cookies_browser: Option<String>,
    playlist: Option<bool>,
    write_subs: Option<bool>,
    start_s: Option<f64>,
    end_s: Option<f64>,
    share: Option<bool>,
    fit_mb: Option<u32>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        run_download_blocking(app, url, quality, output_dir, cookies_browser, playlist, write_subs, start_s, end_s, share.unwrap_or(false), fit_mb)
    })
    .await
    .map_err(|e| format!("task join: {}", e))?
}

fn run_download_blocking(
    app: AppHandle,
    url: String,
    quality: String,
    output_dir: Option<String>,
    cookies_browser: Option<String>,
    playlist: Option<bool>,
    write_subs: Option<bool>,
    start_s: Option<f64>,
    end_s: Option<f64>,
    share: bool,
    fit_mb: Option<u32>,
) -> Result<(), String> {
    // Trim a section out of the download when the user picked an in/out point in
    // the URL trim editor. A valid range needs both ends and end > start.
    let trim_section: Option<(f64, f64)> = match (start_s, end_s) {
        (Some(s), Some(e)) if e > s + 0.01 => Some((s.max(0.0), e)),
        _ => None,
    };
    let format_selector = match quality.as_str() {
        "1080p" => "bestvideo[height<=1080]+bestaudio/best[height<=1080]/best",
        "720p"  => "bestvideo[height<=720]+bestaudio/best[height<=720]/best",
        "480p"  => "bestvideo[height<=480]+bestaudio/best[height<=480]/best",
        "audio" => "bestaudio/best",
        _       => "bestvideo+bestaudio/best",
    };

    let dl_dir = if share {
        // Quick share never touches Downloads: a fresh temp folder per share
        // (the previous share's files are wiped first).
        new_share_dir()?
    } else {
        match output_dir.filter(|s| !s.trim().is_empty()) {
            Some(d) => {
                let p = PathBuf::from(&d);
                if p.is_dir() { p } else { downloads_dir() }
            }
            None => downloads_dir(),
        }
    };

    DL_CANCEL.store(false, Ordering::SeqCst);

    // Trim: fetch ONLY the selected part (download_section_fast). If that path
    // fails for any reason, fall back to a full download + local cut below.
    let mut fast_path: Option<String> = None;
    if let Some((s, e)) = trim_section {
        let _ = app.emit("download-stage", "Downloading only the selected part…");
        // Direct HTTPS files first (they go through the chunking proxy); HLS
        // only as a last resort — YouTube throttles its HLS for ffmpeg too.
        let fast_selector = format!("{}/{}", match quality.as_str() {
            "1080p" => "bv[protocol=https][height<=1080]+ba[protocol=https]/b[protocol=https][height<=1080]",
            "720p"  => "bv[protocol=https][height<=720]+ba[protocol=https]/b[protocol=https][height<=720]",
            "480p"  => "bv[protocol=https][height<=480]+ba[protocol=https]/b[protocol=https][height<=480]",
            "audio" => "ba[protocol=https]",
            _       => "bv[protocol=https]+ba[protocol=https]/b[protocol=https]",
        }, format_selector);
        match download_section_fast(&app, &url, &fast_selector, cookies_browser.as_deref(), &dl_dir, s, e, quality == "audio") {
            Ok(p) => fast_path = Some(p),
            Err(err) if !dl_cancelled() => {
                eprintln!("[download] section fast path failed, full download instead: {}", err);
                let _ = app.emit("download-stage", "Preparing full download…");
            }
            Err(_) => {}
        }
    }
    if dl_cancelled() {
        return finish_cancelled(&app, share);
    }
    let (mut ok, mut final_path, mut cut_error, stderr_text, exit_code) = match fast_path {
        Some(p) => (true, Some(p), None, String::new(), Some(0)),
        None => full_download(&app, &url, format_selector, &quality, &dl_dir, cookies_browser.as_deref(),
                              playlist, write_subs, trim_section, share)
            .unwrap_or_else(|e| (false, None, Some(e), String::new(), None)),
    };
    if dl_cancelled() {
        return finish_cancelled(&app, share);
    }

    // Quick share: optionally squeeze under a size cap (Discord), then put the
    // file on the clipboard so it can be pasted straight into a chat.
    if ok && share {
        if let Some(src) = final_path.clone() {
            let res = (|| -> Result<String, String> {
                let mut path = src;
                if let Some(mb) = fit_mb.filter(|_| quality != "audio") {
                    let _ = app.emit("download-stage", format!("Compressing to fit {} MB…", mb));
                    path = compress_to_fit(&path, mb)?;
                }
                let _ = app.emit("download-stage", "Copying to clipboard…");
                copy_file_to_clipboard(&path)?;
                Ok(path)
            })();
            match res {
                Ok(p) => final_path = Some(p),
                Err(e) => {
                    ok = false;
                    cut_error = Some(e);
                }
            }
        }
    }
    if dl_cancelled() {
        return finish_cancelled(&app, share);
    }
    if share && !ok {
        clear_share_files();
    }

    if ok {
        let file_name = final_path
            .as_ref()
            .and_then(|p| std::path::Path::new(p).file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        // Record the download in Recent Activity (downloads were previously
        // missing from history — only conversions were logged).
        let size_mb = final_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len() as f64 / 1_048_576.0)
            .unwrap_or(0.0);
        push_history(&app, HistoryEntry {
            when: chrono::Local::now().to_rfc3339(),
            preset: if share { "Quick share".into() } else { "Download".into() },
            // Show the saved file name in the history row (the "in_file" column
            // is what the UI displays); the URL is long and noisy.
            in_file: file_name.clone().unwrap_or_else(|| "download".into()),
            out_file: file_name.clone().unwrap_or_default(),
            ok: true,
            secs: 0.0,
            size_mb: (size_mb * 10.0).round() / 10.0,
            path: final_path.clone(),
        });
        let _ = app.emit(
            "download-done",
            DownloadDoneEvent {
                ok: true,
                file_path: final_path,
                file_name,
                error: None,
                shared: share,
                size_mb: (size_mb * 10.0).round() / 10.0,
            },
        );
        Ok(())
    } else {
        let trimmed = stderr_text.lines().rev().take(3).collect::<Vec<_>>().join(" / ");
        let message = cut_error.unwrap_or_else(|| {
            if trimmed.is_empty() { format!("exit {:?}", exit_code) } else { trimmed }
        });
        let _ = app.emit(
            "download-done",
            DownloadDoneEvent {
                ok: false,
                file_path: None,
                file_name: None,
                error: Some(message),
                shared: share,
                size_mb: 0.0,
            },
        );
        Err("download failed".into())
    }
}

// ---------- Download cancel ----------

/// Set by `cancel_download`; the running download checks it between steps.
static DL_CANCEL: AtomicBool = AtomicBool::new(false);
/// PID of the yt-dlp / ffmpeg process the download is currently waiting on.
static DL_PID: AtomicU32 = AtomicU32::new(0);

fn dl_cancelled() -> bool {
    DL_CANCEL.load(Ordering::SeqCst)
}

/// Run a download-step command so `cancel_download` can kill it.
fn run_dl_tracked(cmd: &mut Command) -> Result<std::process::Output, String> {
    if dl_cancelled() {
        return Err("Cancelled".into());
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {}", e))?;
    DL_PID.store(child.id(), Ordering::SeqCst);
    let out = child.wait_with_output().map_err(|e| format!("wait: {}", e));
    DL_PID.store(0, Ordering::SeqCst);
    if dl_cancelled() {
        return Err("Cancelled".into());
    }
    out
}

fn finish_cancelled(app: &AppHandle, share: bool) -> Result<(), String> {
    if share {
        clear_share_files();
    }
    let _ = app.emit(
        "download-done",
        DownloadDoneEvent { ok: false, file_path: None, file_name: None, error: Some("Cancelled".into()), shared: share, size_mb: 0.0 },
    );
    Err("cancelled".into())
}

/// Stop the running URL download / share (kills yt-dlp or ffmpeg).
#[tauri::command]
fn cancel_download() {
    DL_CANCEL.store(true, Ordering::SeqCst);
    let pid = DL_PID.swap(0, Ordering::SeqCst);
    if pid != 0 {
        std::thread::spawn(move || {
            let _ = cmd_hidden("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
        });
    }
}

// ---------- Trimmed download: fetch only the selected section ----------
//
// yt-dlp's --download-sections hands the stream URLs to ffmpeg, whose HTTP
// client asks for open-ended ranges ("bytes=N-"). YouTube throttles those to
// a crawl (the old hang). Bounded ranges come back at full speed, so ffmpeg
// reads through a tiny local HTTP proxy (`range_proxy_port`) that serves the
// requested range as ONE continuous response while fetching the upstream in
// bounded chunks. ffmpeg then seeks straight to the start point and only the
// selected part is downloaded (plus the index at the file head).

const PROXY_CHUNK: u64 = 8 * 1024 * 1024;

fn range_proxy_port() -> Option<u16> {
    static PORT: std::sync::OnceLock<Option<u16>> = std::sync::OnceLock::new();
    *PORT.get_or_init(|| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let port = listener.local_addr().ok()?.port();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let _ = serve_proxy_conn(conn);
                });
            }
        });
        Some(port)
    })
}

fn serve_proxy_conn(mut conn: std::net::TcpStream) -> std::io::Result<()> {
    use std::io::Write;
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut range: Option<(u64, Option<u64>)> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("range") {
                range = v.trim().strip_prefix("bytes=").and_then(|r| {
                    let (a, b) = r.split_once('-')?;
                    Some((a.trim().parse().ok()?, b.trim().parse().ok()))
                });
            }
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let head_only = request_line.starts_with("HEAD");
    let Some(upstream) = preview_streams().lock().unwrap().get(path.trim_matches('/')).cloned() else {
        return conn.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    };
    let fetch = |from: u64, to: u64| {
        preview_agent()
            .get(&upstream)
            .set("Range", &format!("bytes={}-{}", from, to))
            .timeout(std::time::Duration::from_secs(60))
            .call()
    };
    let (start, want_end) = range.unwrap_or((0, None));
    // First chunk tells us the total size.
    let first = match fetch(start, start + PROXY_CHUNK - 1) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => {
            return conn.write_all(format!("HTTP/1.1 {} Upstream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", code).as_bytes());
        }
        Err(_) => return conn.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    };
    let total: Option<u64> = first
        .header("Content-Range")
        .and_then(|cr| cr.rsplit('/').next())
        .and_then(|t| t.trim().parse().ok());
    let content_type = first.header("Content-Type").unwrap_or("application/octet-stream").to_string();
    let Some(total) = total else {
        // Upstream ignored the range: just pass the body through.
        let mut body = Vec::new();
        first.into_reader().read_to_end(&mut body)?;
        conn.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", content_type, body.len()).as_bytes())?;
        return if head_only { Ok(()) } else { conn.write_all(&body) };
    };
    let end = want_end.unwrap_or(total - 1).min(total - 1);
    let status = if range.is_some() { "206 Partial Content" } else { "200 OK" };
    conn.write_all(format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
        status, content_type, end + 1 - start, start, end, total
    ).as_bytes())?;
    if head_only {
        return Ok(());
    }
    // Stream chunk after chunk; a write error = ffmpeg closed (seek / done).
    let mut pos = start;
    let mut resp = Some(first);
    while pos <= end && !dl_cancelled() {
        let r = match resp.take() {
            Some(r) => r,
            None => match fetch(pos, (pos + PROXY_CHUNK - 1).min(end)) {
                Ok(r) => r,
                Err(_) => break,
            },
        };
        let want = (end + 1 - pos).min(PROXY_CHUNK);
        let copied = std::io::copy(&mut r.into_reader().take(want), &mut conn)?;
        if copied == 0 {
            break;
        }
        pos += copied;
    }
    conn.flush()
}

/// Download only [start, end] of a URL: yt-dlp resolves the stream URLs,
/// ffmpeg reads just that part (through the range proxy for plain HTTP files,
/// directly for HLS) and encodes the clip. Returns the clip path.
#[allow(clippy::too_many_arguments)]
fn download_section_fast(
    app: &AppHandle,
    url: &str,
    format_selector: &str,
    cookies_browser: Option<&str>,
    dl_dir: &std::path::Path,
    start: f64,
    end: f64,
    audio_only: bool,
) -> Result<String, String> {
    // 1) Stream URLs + the file name yt-dlp would use.
    let mut cmd = cmd_tool("yt-dlp");
    cmd.args([
        "-f", format_selector,
        "--no-playlist", "--no-warnings", "--no-update", "--no-check-certificate",
        "--encoding", "utf-8", "--restrict-filenames",
        "-o", "%(title).100s.%(ext)s",
        "--print", "filename",
        "--print", "%(protocol)s",
        "--print", "%(url)s",
        "--print", "%(requested_formats.0.url)s",
        "--print", "%(requested_formats.1.url)s",
    ]);
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.env("PYTHONUTF8", "1");
    if let Some(b) = cookies_browser.filter(|s| !s.is_empty() && *s != "auto") {
        cmd.arg("--cookies-from-browser").arg(b);
    }
    cmd.arg(url);
    let out = run_dl_tracked(&mut cmd)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("yt-dlp failed").to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    if lines.len() < 5 {
        return Err("unexpected yt-dlp output".into());
    }
    let (filename, protocol) = (lines[0], lines[1].to_ascii_lowercase());
    let is_url = |s: &str| s.starts_with("http");
    let inputs: Vec<String> = if is_url(lines[2]) {
        vec![lines[2].to_string()]
    } else if is_url(lines[3]) && is_url(lines[4]) {
        vec![lines[3].to_string(), lines[4].to_string()]
    } else if is_url(lines[3]) {
        vec![lines[3].to_string()]
    } else {
        return Err("no stream URL".into());
    };
    if protocol.contains("dash") || protocol.contains("f4m") {
        return Err(format!("unsupported protocol {}", protocol));
    }
    // HLS goes to ffmpeg directly (it only fetches the needed segments);
    // plain files go through the chunking proxy.
    let port = range_proxy_port().ok_or("proxy unavailable")?;
    let inputs: Vec<String> = inputs
        .into_iter()
        .map(|u| if u.contains(".m3u8") || protocol.contains("m3u8") { u } else {
            format!("http://127.0.0.1:{}/{}", port, register_preview_stream(&u))
        })
        .collect();

    // 2) Output name like the full-download path: "<title>_clip_1m23s-1m33s.mp4"
    let stem = std::path::Path::new(filename).file_stem().and_then(|s| s.to_str()).unwrap_or("clip").to_string();
    let ext = if audio_only { "m4a" } else { "mp4" };
    let base = format!("{}_clip_{}-{}", stem, fmt_clip_ts(start), fmt_clip_ts(end));
    let mut out_path = dl_dir.join(format!("{}.{}", base, ext));
    let mut n = 2;
    while out_path.exists() {
        out_path = dl_dir.join(format!("{} ({}).{}", base, n, ext));
        n += 1;
    }
    let out_str = out_path.to_string_lossy().to_string();

    // 3) ffmpeg: input seek on every input (fast + frame-exact with re-encode).
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-y".into(),
                                     "-nostats".into(), "-progress".into(), "pipe:1".into()];
    for u in &inputs {
        args.extend([
            "-ss".into(), format!("{:.3}", start), "-to".into(), format!("{:.3}", end),
            "-reconnect".into(), "1".into(), "-reconnect_streamed".into(), "1".into(),
            "-i".into(), u.clone(),
        ]);
    }
    if audio_only {
        args.extend(["-map", "0:a:0", "-vn", "-c:a", "aac", "-b:a", "192k"].map(String::from));
    } else {
        args.extend(["-map".into(), "0:v:0".into()]);
        args.extend(["-map".into(), if inputs.len() > 1 { "1:a:0?".into() } else { "0:a:0?".into() }]);
        args.extend(encoder_args("h264", 19));
        args.extend(["-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"].map(String::from));
    }
    args.push(out_str.clone());

    let mut child = cmd_tool("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("ffmpeg spawn: {}", e))?;
    DL_PID.store(child.id(), Ordering::SeqCst);
    let stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr { let _ = e.read_to_string(&mut s); }
        s
    });
    // Watchdog: if the clip position doesn't advance for STALL_S, kill ffmpeg
    // so the caller falls back to a full download instead of hanging.
    const STALL_S: u64 = 40;
    let progress_at = std::sync::Arc::new(AtomicU32::new(0));   // seconds since start of last advance
    let finished = std::sync::Arc::new(AtomicBool::new(false));
    let stalled = std::sync::Arc::new(AtomicBool::new(false));
    {
        let (progress_at, finished, stalled) = (progress_at.clone(), finished.clone(), stalled.clone());
        let pid = child.id();
        let t0 = std::time::Instant::now();
        std::thread::spawn(move || {
            while !finished.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let now = t0.elapsed().as_secs();
                if now.saturating_sub(progress_at.load(Ordering::SeqCst) as u64) > STALL_S && !finished.load(Ordering::SeqCst) {
                    stalled.store(true, Ordering::SeqCst);
                    let _ = cmd_hidden("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
                    break;
                }
            }
        });
    }
    // Progress: out_time_us → percent of the clip length.
    let clip_us = ((end - start) * 1_000_000.0).max(1.0);
    let started = std::time::Instant::now();
    let mut last_emit = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let mut best_us = 0.0_f64;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("out_time_us=") {
                let Ok(us) = v.trim().parse::<f64>() else { continue };
                if us > best_us {
                    best_us = us;
                    progress_at.store(started.elapsed().as_secs() as u32, Ordering::SeqCst);
                }
                let pct = (us / clip_us * 100.0).clamp(0.0, 99.9);
                if last_emit.elapsed() >= std::time::Duration::from_millis(150) {
                    last_emit = std::time::Instant::now();
                    let el = started.elapsed().as_secs_f64();
                    let eta = (pct > 1.0).then(|| {
                        let left = (el / (pct / 100.0) - el).max(0.0) as u64;
                        format!("{}:{:02}", left / 60, left % 60)
                    });
                    let _ = app.emit("download-progress", DownloadProgressEvent {
                        percent: Some(pct),
                        speed: None,
                        eta,
                        size: Some(format!("clip {}", fmt_clip_ts(end - start))),
                    });
                }
            }
        }
    }
    let status = child.wait().map_err(|e| format!("ffmpeg wait: {}", e))?;
    finished.store(true, Ordering::SeqCst);
    DL_PID.store(0, Ordering::SeqCst);
    let err = err_thread.join().unwrap_or_default();
    if dl_cancelled() {
        let _ = std::fs::remove_file(&out_path);
        return Err("Cancelled".into());
    }
    if stalled.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&out_path);
        return Err(format!("no progress for {} s", STALL_S));
    }
    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    if !status.success() || size < 1024 {
        let _ = std::fs::remove_file(&out_path);
        return Err(format!("ffmpeg: {}", err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("failed")));
    }
    let _ = app.emit("download-progress", DownloadProgressEvent { percent: Some(100.0), speed: None, eta: None, size: None });
    Ok(out_str)
}

/// Full yt-dlp download (+ local cut when a trim range is given — only used
/// when the section fast path failed). Returns
/// (ok, final_path, cut_error, stderr, exit code).
#[allow(clippy::too_many_arguments)]
fn full_download(
    app: &AppHandle,
    url: &str,
    format_selector: &str,
    quality: &str,
    dl_dir: &std::path::Path,
    cookies_browser: Option<&str>,
    playlist: Option<bool>,
    write_subs: Option<bool>,
    trim_section: Option<(f64, f64)>,
    share: bool,
) -> Result<(bool, Option<String>, Option<String>, String, Option<i32>), String> {
    // The full file goes into a private work folder next to the destination
    // (same drive, never %TEMP%), and only the clip is kept.
    let work_dir: Option<PathBuf> = match trim_section {
        Some(_) => {
            let d = dl_dir.join(format!(".nibble-trim-{}", chrono::Local::now().timestamp_millis()));
            std::fs::create_dir_all(&d).map_err(|e| format!("create work folder: {}", e))?;
            Some(d)
        }
        None => None,
    };
    let dl_dir_str = work_dir.as_deref().unwrap_or(dl_dir).to_string_lossy().to_string();

    let mut cmd = cmd_tool("yt-dlp");
    cmd.args(&[
        "-f", format_selector,
        // -P sets the OUTPUT DIRECTORY as a literal path (no template parsing,
        //    so parens like "(gameplays)" stay parens, not yt-dlp fields).
        // -o sets only the FILENAME template.
        "-P", &dl_dir_str,
        "-o", "%(title).100s.%(ext)s",
        "--newline",
        "--restrict-filenames",
        "--no-mtime",
        "--no-update",
        "--no-check-certificate",
        "--encoding", "utf-8",
        "--merge-output-format", "mp4",
        // Force progress output. Without this, `--print` switches yt-dlp into
        // "machine-readable" mode and suppresses normal `[download] xx.x% ...`
        // lines — UI would stay stuck on "Connecting…" forever.
        "--progress",
        // Print the final path AFTER yt-dlp has moved the file to its final
        // location. Reliable single source of truth for the output filename.
        "--print", "after_move:OUTFILE=%(filepath)s",
    ]);
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.env("PYTHONUTF8", "1");
    // yt-dlp needs ffmpeg to merge separate video+audio streams and to cut
    // sections (--download-sections / --force-keyframes-at-cuts). It finds ffmpeg
    // in its own folder or on PATH — but if the app-managed ffmpeg lives in our
    // bin while yt-dlp came from PATH (or vice-versa), point yt-dlp at it so the
    // merge/trim step never fails with "ffmpeg not found".
    {
        let ff = deps_dir().join("ffmpeg.exe");
        if ff.exists() {
            cmd.arg("--ffmpeg-location").arg(deps_dir());
        }
    }
    // Playlist toggle: default --no-playlist (single video), opt-in for whole
    // playlist. Trimming a section only makes sense for ONE video, so a trim
    // always forces single-video mode regardless of the playlist checkbox.
    if playlist.unwrap_or(false) && trim_section.is_none() && !share {
        cmd.arg("--yes-playlist");
    } else {
        cmd.arg("--no-playlist");
    }
    // Subtitles: write English subs to a sidecar .srt if available. Skipped for
    // trimmed clips — the timings would refer to the full video.
    if write_subs.unwrap_or(false) && trim_section.is_none() && !share {
        cmd.args(&["--write-subs", "--sub-langs", "en.*,en", "--write-auto-subs"]);
    }
    if let Some(b) = cookies_browser.filter(|s| !s.is_empty() && *s != "auto") {
        cmd.arg("--cookies-from-browser").arg(b);
    }
    // What's already in the folder — a cancel only cleans up NEW files.
    let before: std::collections::HashSet<String> = std::fs::read_dir(&dl_dir_str)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    let mut child = cmd
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn yt-dlp: {}", e))?;
    DL_PID.store(child.id(), Ordering::SeqCst);
    if dl_cancelled() {
        // Cancel clicked while yt-dlp was starting
        let _ = child.kill();
    }

    let stdout = child.stdout.take().ok_or("yt-dlp stdout pipe unavailable")?;
    let app_clone = app.clone();

    // Track final output filename heuristically
    let merged_path = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let last_dest   = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let merged_clone = merged_path.clone();
    let last_dest_clone = last_dest.clone();

    let read_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        // yt-dlp prints progress many times per second; cap UI updates at ~10/s
        // (always let 100 % through so the bar visibly completes).
        let mut last_emit: Option<std::time::Instant> = None;
        for line in reader.lines().flatten() {
            // Progress
            if let Some(ev) = parse_ytdlp_progress(&line) {
                let done = ev.percent.map_or(false, |p| p >= 100.0);
                let due = last_emit.map_or(true, |t| t.elapsed() >= std::time::Duration::from_millis(100));
                if done || due {
                    last_emit = Some(std::time::Instant::now());
                    let _ = app_clone.emit("download-progress", ev);
                }
                continue;
            }
            // PRIMARY: --print after_move:OUTFILE=<path>
            if let Some(rest) = line.strip_prefix("OUTFILE=") {
                *merged_clone.lock().unwrap() = Some(rest.trim().to_string());
                continue;
            }
            // FALLBACK 1: [Merger] Merging formats into "C:\path\file.mp4"
            if let Some(idx) = line.find("Merging formats into") {
                let rest = &line[idx..];
                if let Some(start) = rest.find('"') {
                    if let Some(end) = rest[start + 1..].find('"') {
                        let path = &rest[start + 1..start + 1 + end];
                        if merged_clone.lock().unwrap().is_none() {
                            *merged_clone.lock().unwrap() = Some(path.to_string());
                        }
                    }
                }
            }
            // FALLBACK 2: per-stream destination (single-stream case)
            if let Some(rest) = line.strip_prefix("[download] Destination: ") {
                *last_dest_clone.lock().unwrap() = Some(rest.trim().to_string());
            }
            // FALLBACK 3: ExtractAudio post-processor
            if let Some(rest) = line.strip_prefix("[ExtractAudio] Destination: ") {
                if merged_clone.lock().unwrap().is_none() {
                    *merged_clone.lock().unwrap() = Some(rest.trim().to_string());
                }
            }
        }
    });

    let stderr = child.stderr.take().ok_or("yt-dlp stderr pipe unavailable")?;
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stderr_buf_clone = stderr_buf.clone();
    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().flatten() {
            stderr_buf_clone.lock().unwrap().push_str(&line);
            stderr_buf_clone.lock().unwrap().push('\n');
        }
    });

    let status = child.wait().map_err(|e| format!("wait failed: {}", e))?;
    DL_PID.store(0, Ordering::SeqCst);
    let _ = read_thread.join();
    let _ = stderr_thread.join();

    let mut final_path = merged_path
        .lock()
        .unwrap()
        .clone()
        .or_else(|| last_dest.lock().unwrap().clone());

    // Cancelled: remove what yt-dlp left behind — every partial / fragment /
    // intermediate-format file that appeared in the folder during this run.
    if dl_cancelled() {
        let leftover = |name: &str| {
            name.ends_with(".part") || name.ends_with(".ytdl") || name.contains(".part-Frag")
                || name.ends_with(".temp") || {
                    // intermediate streams before merging: "title.f616.mp4"
                    let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("");
                    stem.rsplit_once(".f").map_or(false, |(_, id)| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || c == '-'))
                }
        };
        // The killed process may hold its files for a moment — retry briefly.
        for _ in 0..10 {
            let mut remaining = 0;
            if let Ok(rd) = std::fs::read_dir(&dl_dir_str) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !before.contains(&name) && leftover(&name) && std::fs::remove_file(e.path()).is_err() {
                        remaining += 1;
                    }
                }
            }
            if remaining == 0 { break; }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        if let Some(wd) = &work_dir {
            let _ = std::fs::remove_dir_all(wd);
        }
        return Ok((false, None, Some("Cancelled".into()), String::new(), None));
    }

    // Trim: cut the selected range out of the freshly downloaded file.
    let mut ok = status.success();
    let mut cut_error: Option<String> = None;
    if ok {
        if let (Some((s, e)), Some(src)) = (trim_section, final_path.clone()) {
            let _ = app.emit("download-stage", "Cutting clip…");
            match cut_clip(&src, dl_dir, s, e, quality == "audio") {
                Ok(clip) => final_path = Some(clip),
                Err(err) => {
                    ok = false;
                    cut_error = Some(err);
                }
            }
        }
    }
    if let Some(wd) = &work_dir {
        let _ = std::fs::remove_dir_all(wd);
    }
    let stderr_text = stderr_buf.lock().unwrap().clone();
    Ok((ok, final_path, cut_error, stderr_text, status.code()))
}

// ============================================================
//  Quick share — download → (trim / fit) → clipboard → auto-delete
//
//  The Windows clipboard can't hold a video as raw data: "copy a file" puts a
//  file *reference* (CF_HDROP) on it, and apps such as Discord read the file at
//  paste/send time. So the clip lives briefly in a temp share folder and is
//  removed by the UI timer / "Delete now" / the next share / app exit, with a
//  startup sweep for anything left behind (crash, forced kill).
// ============================================================

fn share_root() -> PathBuf {
    match portable_root() {
        Some(root) => root.join("share"),
        None => std::env::temp_dir().join("Nibble Share"),
    }
}

/// Delete every quick-share file. If the clipboard still references one of
/// them, clear it too, so a later Ctrl+V can't paste a dead file link.
fn clear_share_files() {
    let root = share_root();
    if !root.exists() {
        return;
    }
    #[cfg(windows)]
    {
        let root_ps = root.to_string_lossy().replace('\'', "''");
        let script = format!(
            "$ErrorActionPreference='SilentlyContinue';\
             $f = Get-Clipboard -Format FileDropList;\
             if ($f | Where-Object {{ $_.FullName -like '{}*' }}) {{\
               Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Clipboard]::Clear() }}",
            root_ps
        );
        let _ = cmd_hidden("powershell")
            .args(["-NoProfile", "-NonInteractive", "-STA", "-ExecutionPolicy", "Bypass", "-Command", &script])
            .output();
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn new_share_dir() -> Result<PathBuf, String> {
    clear_share_files();
    let dir = share_root().join(chrono::Local::now().timestamp_millis().to_string());
    std::fs::create_dir_all(&dir).map_err(|e| format!("create share folder: {}", e))?;
    Ok(dir)
}

/// Put a file on the clipboard exactly like Explorer's "Copy" does, so it can
/// be pasted into Discord, Messenger, mail, Explorer… with Ctrl+V.
fn copy_file_to_clipboard(path: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let p = path.replace('\'', "''");
        let out = cmd_hidden("powershell")
            .args(["-NoProfile", "-NonInteractive", "-STA", "-ExecutionPolicy", "Bypass",
                   "-Command", &format!("Set-Clipboard -LiteralPath '{}'", p)])
            .output()
            .map_err(|e| format!("powershell spawn: {}", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("Could not copy to clipboard: {}",
                err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("unknown error")));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err("Clipboard sharing is only supported on Windows".into())
    }
}

/// Re-encode `path` to fit under `mb` megabytes using the Discord presets'
/// size-targeted bitrate math (single source of truth). Replaces the file and
/// returns the new path (same name, .mp4).
fn compress_to_fit(path: &str, mb: u32) -> Result<String, String> {
    let preset_id = match mb {
        0..=20 => 13,   // Discord ≤ 20 MB (free)
        21..=50 => 14,  // Discord ≤ 50 MB (Nitro Basic)
        _ => 21,        // Discord ≤ 500 MB (Nitro)
    };
    let preset = find_preset(preset_id).ok_or("size preset missing")?;
    let src = std::path::Path::new(path);
    let dir = src.parent().ok_or("bad path")?;
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("clip").to_string();
    let tmp = dir.join(format!("{}.fit.mp4", stem));
    let meta = probe_metadata(path);
    let dur_s = meta.duration_ms.ok_or("could not read the clip length")? as f64 / 1000.0;
    let mut options = HashMap::new();
    options.insert("start".to_string(), "0".to_string());
    options.insert("end".to_string(), format!("{:.3}", dur_s));
    let tmp_str = tmp.to_string_lossy().to_string();
    let preset_args = build_args(&preset, path, &tmp_str, &options, meta.duration_ms, meta.is_hdr, meta.fps)?;
    let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-y".into()];
    args.extend(preset_args);
    let out = match run_dl_tracked(cmd_tool("ffmpeg").args(&args)) {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    if !out.status.success() || !tmp.exists() {
        let _ = std::fs::remove_file(&tmp);
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("Compression failed: {}",
            err.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("ffmpeg error")));
    }
    let final_path = dir.join(format!("{}.mp4", stem));
    let _ = std::fs::remove_file(src);
    std::fs::rename(&tmp, &final_path).map_err(|e| format!("rename: {}", e))?;
    Ok(final_path.to_string_lossy().into_owned())
}

/// "Copy again" button. Only files inside the share folder are accepted.
#[tauri::command(async)]
fn share_copy(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.starts_with(share_root()) || !p.is_file() {
        return Err("The shared file is no longer available.".into());
    }
    copy_file_to_clipboard(&path)
}

/// Recent Activity "copy" button. Images go on the clipboard as a picture
/// (pastes into chats, editors, Paint); anything else as a file (Ctrl+V into
/// Discord, a folder…). Errors "missing" when the file is gone.
#[tauri::command(async)]
fn copy_to_clipboard(path: String) -> Result<String, String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err("missing".into());
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp") {
        let img = image::open(&p).map_err(|e| format!("open image: {}", e))?.to_rgba8();
        let (w, h) = img.dimensions();
        let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard: {}", e))?;
        cb.set_image(arboard::ImageData { width: w as usize, height: h as usize, bytes: std::borrow::Cow::Owned(img.into_raw()) })
            .map_err(|e| format!("clipboard: {}", e))?;
        return Ok("image".into());
    }
    copy_file_to_clipboard(&path)?;
    Ok("file".into())
}

/// Recent Activity × button: delete the file (to the Recycle Bin unless
/// `permanent`) and drop its history row. "missing" when already gone.
#[tauri::command(async)]
fn delete_file(app: AppHandle, path: String, permanent: bool) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if p.is_file() {
        if permanent {
            std::fs::remove_file(&p).map_err(|e| format!("delete: {}", e))?;
        } else {
            move_to_recycle_bin(&p)?;
        }
    }
    let _guard = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = load_history(&app);
    h.retain(|e| e.path.as_deref() != Some(path.as_str()));
    save_history(&app, &h);
    Ok(())
}

#[cfg(windows)]
fn move_to_recycle_bin(p: &std::path::Path) -> Result<(), String> {
    use windows::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW};
    // pFrom is a double-NUL-terminated list
    let mut from: Vec<u16> = p.as_os_str().encode_wide_vec();
    from.extend([0, 0]);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 || op.fAnyOperationsAborted.as_bool() {
        return Err(format!("Could not move to the Recycle Bin (code {})", r));
    }
    Ok(())
}
#[cfg(not(windows))]
fn move_to_recycle_bin(p: &std::path::Path) -> Result<(), String> {
    std::fs::remove_file(p).map_err(|e| format!("delete: {}", e))
}

#[cfg(windows)]
trait EncodeWideVec { fn encode_wide_vec(&self) -> Vec<u16>; }
#[cfg(windows)]
impl EncodeWideVec for std::ffi::OsStr {
    fn encode_wide_vec(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().collect()
    }
}

/// "Delete now" button and the UI auto-delete timer.
#[tauri::command(async)]
fn share_clear() {
    clear_share_files();
}

/// Default window size as a share of the monitor, so it looks the same on any
/// resolution and any Windows scaling: 1565×1039 physical px on a 3840×2160
/// screen, proportionally smaller/larger elsewhere, never below the 900×600
/// (logical) minimum and never larger than the screen. Big enough that the
/// home screen (actions, Recent Activity, Features) fits without scrolling.
/// The UI *scale* still comes from the 1510×1038 reference (main.js), so a
/// bigger default window means more room, not bigger text.
fn fit_window_to_monitor(window: &tauri::WebviewWindow) {
    let Ok(Some(monitor)) = window.current_monitor().or_else(|_| window.primary_monitor()) else { return };
    let screen = monitor.size();          // physical pixels
    let scale = monitor.scale_factor();   // e.g. 1.25 / 1.5
    let (sw, sh) = (screen.width as f64, screen.height as f64);
    let w = (sw * 1565.0 / 3840.0).max(900.0 * scale).min(sw * 0.95);
    let h = (sh * 1039.0 / 2160.0).max(600.0 * scale).min(sh * 0.92);
    let _ = window.set_size(tauri::PhysicalSize::new(w.round() as u32, h.round() as u32));
    let _ = window.center();
}

// ---------- Remembered window geometry (Settings → "Remember window size") ----------

#[derive(Serialize, Deserialize, Default, Clone)]
struct WindowState {
    remember: bool,
    /// Last geometry in physical px (None until the window has been closed once).
    rect: Option<(i32, i32, u32, u32)>,
    maximized: bool,
}

fn window_state_path(app: &AppHandle) -> PathBuf {
    data_dir(app).join("window.json")
}

fn load_window_state(app: &AppHandle) -> WindowState {
    std::fs::read_to_string(window_state_path(app))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn store_window_state(app: &AppHandle, st: &WindowState) {
    if let Ok(json) = serde_json::to_string(st) {
        let _ = std::fs::write(window_state_path(app), json);
    }
}

/// Restore the saved geometry if it still lands on a connected monitor.
fn restore_window(window: &tauri::WebviewWindow, st: &WindowState) -> bool {
    let Some((x, y, w, h)) = st.rect else { return false };
    let visible = window.available_monitors().unwrap_or_default().iter().any(|m| {
        let (p, s) = (m.position(), m.size());
        let (cx, cy) = (x + w as i32 / 2, y + h as i32 / 2);
        cx >= p.x && cx < p.x + s.width as i32 && cy >= p.y && cy < p.y + s.height as i32
    });
    if !visible || w < 400 || h < 300 {
        return false;
    }
    let _ = window.set_size(tauri::PhysicalSize::new(w, h));
    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
    if st.maximized {
        let _ = window.maximize();
    }
    true
}

/// Remember the geometry on close (only when the setting is on).
fn save_window_geometry(window: &tauri::WebviewWindow) {
    let app = window.app_handle();
    let mut st = load_window_state(app);
    if !st.remember {
        return;
    }
    st.maximized = window.is_maximized().unwrap_or(false);
    // While maximized keep the previous normal rect, so un-maximizing after
    // the next start returns to a sensible size.
    if !st.maximized {
        if let (Ok(p), Ok(s)) = (window.outer_position(), window.inner_size()) {
            st.rect = Some((p.x, p.y, s.width, s.height));
        }
    }
    store_window_state(app, &st);
}

#[tauri::command]
fn set_remember_window(app: AppHandle, enabled: bool) {
    let mut st = load_window_state(&app);
    st.remember = enabled;
    if !enabled {
        st.rect = None;
        st.maximized = false;
    }
    store_window_state(&app, &st);
}

/// Recent Activity → open a result file in its default app, or (reveal)
/// highlight it in Explorer. Errors "missing" when the file is gone, so the
/// page can drop the row (reveal_in_explorer would open the parent instead).
#[tauri::command(async)]
fn open_file(path: String, reveal: Option<bool>) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if !p.is_file() {
        return Err("missing".into());
    }
    if reveal.unwrap_or(false) {
        return reveal_in_explorer(path.clone());
    }
    Command::new("explorer.exe").arg(p).spawn().map(|_| ()).map_err(|e| format!("open: {}", e))
}

/// The author's GitHub ("ZIPEEK" links). Fixed URL — the UI can't open
/// arbitrary addresses through this.
#[tauri::command]
fn open_author_page() -> Result<(), String> {
    Command::new("explorer.exe")
        .arg("https://github.com/xlzipx")
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("open browser: {}", e))
}

/// Back to the default size for this monitor (Settings button).
#[tauri::command]
fn reset_window_size(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unmaximize();
        fit_window_to_monitor(&w);
    }
}

/// "83.3" → "1m23s", "3725" → "1h02m05s" (for readable clip file names).
fn fmt_clip_ts(secs: f64) -> String {
    let t = secs.max(0.0).round() as u64;
    if t >= 3600 {
        format!("{}h{:02}m{:02}s", t / 3600, (t % 3600) / 60, t % 60)
    } else {
        format!("{}m{:02}s", t / 60, t % 60)
    }
}

/// Cut [start, end) out of `src` into `out_dir`, frame-accurately.
/// Video is re-encoded with the machine's best H.264 encoder (NVENC/AMF/QSV/CPU)
/// — a few seconds for a typical clip; input-side -ss keeps seeking fast.
/// Returns the clip path. Never overwrites an existing file.
fn cut_clip(src: &str, out_dir: &std::path::Path, start: f64, end: f64, audio_only: bool) -> Result<String, String> {
    let src_path = std::path::Path::new(src);
    let stem = src_path.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let ext = if audio_only { "m4a" } else { "mp4" };
    let base = format!("{}_clip_{}-{}", stem, fmt_clip_ts(start), fmt_clip_ts(end));
    let mut out = out_dir.join(format!("{}.{}", base, ext));
    let mut n = 2;
    while out.exists() {
        out = out_dir.join(format!("{} ({}).{}", base, n, ext));
        n += 1;
    }
    let out_str = out.to_string_lossy().to_string();

    let mut args: Vec<String> = vec![
        "-hide_banner".into(), "-loglevel".into(), "error".into(), "-y".into(),
        "-ss".into(), format!("{:.3}", start),
        "-i".into(), src.to_string(),
        "-t".into(), format!("{:.3}", end - start),
    ];
    if audio_only {
        args.extend(["-vn", "-c:a", "aac", "-b:a", "192k"].map(String::from));
    } else {
        args.extend(["-map", "0:v:0", "-map", "0:a:0?"].map(String::from));
        args.extend(encoder_args("h264", 19));
        // 8-bit 4:2:0 so HDR / 10-bit sources (VP9.2, AV1) still encode on
        // hardware H.264 and play everywhere (Discord, phones, editors).
        args.extend(["-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"].map(String::from));
    }
    args.push(out_str.clone());

    let res = run_dl_tracked(cmd_tool("ffmpeg").args(&args))?;
    if res.status.success() && out.exists() {
        Ok(out_str)
    } else {
        let _ = std::fs::remove_file(&out);
        let err = String::from_utf8_lossy(&res.stderr);
        let tail = err.lines().filter(|l| !l.trim().is_empty()).rev().take(2).collect::<Vec<_>>().join(" / ");
        Err(format!("Cutting the clip failed: {}", if tail.is_empty() { "ffmpeg error".into() } else { tail }))
    }
}

/// Build an atempo filter chain whose stages multiply to `speed`.
/// atempo accepts only [0.5, 2.0] per stage, so e.g.:
///   4.0  → "atempo=2.0,atempo=2.0"
///   0.25 → "atempo=0.5,atempo=0.5"
///   1.5  → "atempo=1.500000"
fn build_atempo_chain(speed: f64) -> String {
    let mut remaining = speed;
    let mut stages: Vec<String> = Vec::new();
    // Speed-up: peel off 2.0 factors until remaining <= 2.0
    while remaining > 2.0 {
        stages.push("atempo=2.0".into());
        remaining /= 2.0;
    }
    // Slow-down: peel off 0.5 factors until remaining >= 0.5
    while remaining < 0.5 {
        stages.push("atempo=0.5".into());
        remaining /= 0.5;
    }
    stages.push(format!("atempo={:.6}", remaining));
    stages.join(",")
}

fn parse_time_to_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    let total_s = match parts.len() {
        1 => parts[0].parse::<f64>().ok()?,
        2 => parts[0].parse::<f64>().ok()? * 60.0 + parts[1].parse::<f64>().ok()?,
        3 => {
            parts[0].parse::<f64>().ok()? * 3600.0
                + parts[1].parse::<f64>().ok()? * 60.0
                + parts[2].parse::<f64>().ok()?
        }
        _ => return None,
    };
    if total_s < 0.0 {
        return None;
    }
    Some((total_s * 1000.0) as u64)
}

fn effective_duration_ms(
    duration_ms: Option<u64>,
    options: &HashMap<String, String>,
) -> Option<u64> {
    let full = duration_ms?;
    let user_set_trim = options.contains_key("start") || options.contains_key("end");
    let start = options.get("start").and_then(|s| parse_time_to_ms(s)).unwrap_or(0);
    let end = options
        .get("end")
        .and_then(|s| parse_time_to_ms(s))
        .unwrap_or(full);
    if end > start {
        Some(end - start)
    } else if user_set_trim {
        // User explicitly set trim values but range is invalid (end <= start).
        // Returning None makes the bitrate calc fail clearly upstream instead
        // of silently encoding the full file at the wrong budget.
        None
    } else {
        Some(full)
    }
}


/// Build the args vector for one input file, applying:
///  - param placeholders ({start}, {end}, …) from `options`
///  - target_size_mb → computed video bitrate ({VBITRATE} / {VMAXRATE} / {VBUFSIZE})
///  - HDR thumbnail detection (uses already-probed `is_hdr` to avoid a 2nd ffprobe)
fn build_args(
    preset: &Preset,
    in_file: &str,
    out_path: &str,
    options: &HashMap<String, String>,
    duration_ms: Option<u64>,
    is_hdr: bool,
    src_fps: Option<f64>,
) -> Result<Vec<String>, String> {
    // 1. Param substitution map (user-supplied options + computed values)
    let mut subs: HashMap<String, String> = HashMap::new();
    for (k, v) in options {
        subs.insert(k.clone(), v.clone());
    }
    // Defaults for unset params
    for p in &preset.params {
        subs.entry(p.name.clone())
            .or_insert(p.default_value.clone());
    }

    // 2a. Compute derived value for "Speed change" preset: vpts = 1/speed.
    //     setpts uses 1/speed (faster=lower) while atempo uses speed directly.
    if let Some(speed_str) = options.get("speed") {
        if let Ok(speed) = speed_str.parse::<f64>() {
            if speed > 0.0 {
                let vpts = 1.0 / speed;
                subs.insert("vpts".into(), format!("{:.6}", vpts));
                // ffmpeg's atempo filter only accepts factors in [0.5, 2.0].
                // For anything outside that (e.g. 4× time-lapse, 0.25× slow-mo)
                // we chain multiple atempo stages whose product equals `speed`.
                subs.insert("atempo_chain".into(), build_atempo_chain(speed));
            }
        }
    }

    // 2b. Custom-size compress preset reads "size_mb" param into target_size_mb
    let dynamic_target = preset.target_size_mb.or_else(|| {
        options.get("size_mb").and_then(|s| s.parse::<u32>().ok())
    });

    // 2c. Compute Discord-style bitrate if target_size set
    //    Uses *effective* duration (end - start) when the preset has trim params set.
    if let (Some(mb), Some(eff_ms)) = (
        dynamic_target,
        effective_duration_ms(duration_ms, options),
    ) {
        let duration_s = (eff_ms as f64) / 1000.0;
        // DECIMAL MB (10 MB = 10,000,000 bytes — what Discord & filesystems display).
        let total_kbps = ((mb as f64) * 8000.0) / duration_s.max(1.0);
        // Audio bitrate tiered to target size
        let audio_kbps: u32 = if mb >= 200 { 192 }
                              else if mb >= 50 { 128 }
                              else { 96 };
        // Apply 8% safety margin
        let video_kbps = ((total_kbps - audio_kbps as f64) * 0.92).max(150.0);
        let video_kbps = video_kbps as u32;
        subs.insert("VBITRATE".into(),  format!("{}k", video_kbps));
        subs.insert("VMAXRATE".into(),  format!("{}k", (video_kbps as f64 * 1.25) as u32));
        subs.insert("VBUFSIZE".into(),  format!("{}k", (video_kbps as f64 * 1.5) as u32));
        // Surface the matching audio bitrate so the preset args can reference it
        // (avoids hardcoded -b:a values that don't match the budget tier).
        subs.insert("AUDIO_KBPS".into(), format!("{}k", audio_kbps));
    }

    // 3. HDR thumbnail: if input is HDR + preset opted in, swap -vf
    let hdr_for_thumb = preset.hdr_thumbnail && is_hdr;

    let mut out: Vec<String> = Vec::with_capacity(preset.args.len());
    let mut i = 0;
    while i < preset.args.len() {
        let a = &preset.args[i];
        // HDR thumbnail special: replace the value following -vf
        if hdr_for_thumb && a == "-vf" && i + 1 < preset.args.len() {
            out.push("-vf".into());
            out.push("thumbnail,zscale=tin=smpte2084:min=bt2020nc:pin=bt2020:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709,format=yuv420p,scale=1920:-1".into());
            i += 2;
            continue;
        }
        // Multi-arg expansion tokens (resolve to 0..N args, not a 1:1 string).
        // {HWDECODE} → hardware decode flags for the detected GPU (or nothing).
        if a == "{HWDECODE}" {
            out.extend(hwdecode_args());
            i += 1;
            continue;
        }
        // {VENC:codec:q} → the full hardware-adaptive video encoder arg list.
        if let Some(spec) = a.strip_prefix("{VENC:").and_then(|s| s.strip_suffix('}')) {
            let mut it = spec.split(':');
            let codec = it.next().unwrap_or("h264");
            let q = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(22);
            out.extend(encoder_args(codec, q));
            i += 1;
            continue;
        }
        // FPS cap, not conversion: presets bake in an `fps=N` filter to halve
        // 120fps Shadowplay captures. But `fps=60` UPSAMPLES a 30fps source by
        // duplicating frames (bloat, no benefit). So if the source is already
        // at or below the target, strip the `fps=N` segment and keep native.
        if a == "-vf" && i + 1 < preset.args.len() {
            let vf_val = substitute_placeholders(&preset.args[i + 1], &subs);
            if let Some(stripped) = strip_fps_if_not_downscaling(&vf_val, src_fps) {
                if stripped.is_empty() {
                    // Whole filter was just the fps= step → drop -vf entirely.
                    i += 2;
                    continue;
                }
                out.push("-vf".into());
                out.push(stripped);
                i += 2;
                continue;
            }
        }
        let replaced = match a.as_str() {
            "{IN}"  => in_file.to_string(),
            "{OUT}" => out_path.to_string(),
            other => substitute_placeholders(other, &subs),
        };
        out.push(replaced);
        i += 1;
    }

    // Safety net — fail loudly if one of OUR placeholders survived substitution
    // (e.g. trim range invalid → VBITRATE never computed), instead of letting
    // ffmpeg crash with a cryptic "Unable to parse maxrate {VMAXRATE}".
    //
    // We check ONLY against the set of names we actually substitute (computed
    // tokens + this preset's declared params). This is deliberately precise so
    // it never false-positives on legitimate ffmpeg filter syntax that uses
    // curly braces (e.g. geq/drawtext expressions in a custom preset).
    let mut known: Vec<String> = vec![
        "VBITRATE".into(), "VMAXRATE".into(), "VBUFSIZE".into(),
        "AUDIO_KBPS".into(), "vpts".into(), "atempo_chain".into(),
    ];
    for p in &preset.params {
        known.push(p.name.clone());
    }
    let joined = out.join(" ");
    for name in &known {
        if joined.contains(&format!("{{{}}}", name)) {
            return Err(format!(
                "Placeholder {{{}}} was not substituted. \
                 Check that the preset's params are set and that start/end trim \
                 values form a valid range.",
                name
            ));
        }
    }
    Ok(out)
}

/// If `vf` begins with an `fps=N` step and the source is already at or below
/// N (within a small tolerance), return the filter chain with that step removed
/// (turning a forced conversion into a no-op so native fps is preserved).
///
/// Returns:
///   Some(remaining_chain)  — fps step was present and should be dropped
///   Some("")               — the entire chain was just `fps=N`, caller drops -vf
///   None                   — no leading fps step, or source is faster than the
///                            target (so the down-conversion is legitimate)
fn strip_fps_if_not_downscaling(vf: &str, src_fps: Option<f64>) -> Option<String> {
    // Only act on a leading "fps=" step (matches both "fps=60" and
    // "fps=60,zscale=..."). Other vf chains (scale, etc.) are left untouched.
    let rest = vf.strip_prefix("fps=")?;
    let (num_str, tail) = match rest.split_once(',') {
        Some((n, t)) => (n, Some(t)),
        None => (rest, None),
    };
    let target_fps: f64 = num_str.trim().parse().ok()?;
    let src = src_fps?;
    // Allow a little slack (e.g. 59.94 vs 60) so we don't pointlessly resample.
    if src <= target_fps + 0.5 {
        Some(tail.unwrap_or("").to_string())
    } else {
        None // source genuinely faster → keep the down-conversion
    }
}

fn substitute_placeholders(s: &str, subs: &HashMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in subs {
        out = out.replace(&format!("{{{}}}", k), v);
    }
    out
}

#[derive(Serialize, Clone)]
struct FileDoneEvent {
    file_index: usize,
    file_name: String,
    ok: bool,
    out_file: String,
    /// Full output path (for "Show in folder").
    out_path: String,
    size_mb: f64,
    secs: f64,
    error: Option<String>,
}

#[derive(Serialize, Clone)]
struct BatchDoneEvent {
    ok_count: usize,
    fail_count: usize,
}

// ---------- ffmpeg progress parser ----------

fn parse_stats(line: &str) -> Option<ProgressEvent> {
    // Example: "frame=  120 fps= 60 q=22.0 size=    1234kB time=00:00:02.00 bitrate=5050.0kbits/s speed=2.5x"
    if !line.contains("frame=") {
        return None;
    }
    let frame = line
        .split("frame=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<u64>().ok());
    let fps = line
        .split("fps=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<f64>().ok());
    let speed = line
        .split("speed=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.trim_end_matches('x').parse::<f64>().ok());
    let time_ms = line
        .split("time=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|t| {
            let parts: Vec<&str> = t.split(':').collect();
            if parts.len() == 3 {
                let h: f64 = parts[0].parse().ok()?;
                let m: f64 = parts[1].parse().ok()?;
                let s: f64 = parts[2].parse().ok()?;
                Some(((h * 3600.0 + m * 60.0 + s) * 1000.0) as u64)
            } else {
                None
            }
        });
    let bitrate = line
        .split("bitrate=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        // ffmpeg emits "bitrate=N/A" on VBR-only frames; hide it instead of
        // showing a literal "N/A" in the UI.
        .filter(|s| !s.eq_ignore_ascii_case("N/A"))
        .map(|s| s.to_string());
    Some(ProgressEvent {
        file_index: 0,
        total_files: 0,
        file_name: String::new(),
        preset_name: String::new(),
        frame,
        fps,
        speed,
        time_ms,
        total_ms: None,
        bitrate,
    })
}

// ---------- Commands ----------

fn custom_presets_path(app: &AppHandle) -> PathBuf {
    data_dir(app).join("custom-presets.json")
}

fn load_custom_presets(app: &AppHandle) -> Vec<Preset> {
    let path = custom_presets_path(app);
    let Ok(data) = std::fs::read_to_string(&path) else { return Vec::new() };
    match serde_json::from_str::<Vec<Preset>>(&data) {
        Ok(list) => list,
        Err(e) => {
            eprintln!("Failed to parse custom-presets.json: {}", e);
            Vec::new()
        }
    }
}

#[tauri::command]
fn list_presets(app: AppHandle) -> Vec<Preset> {
    let mut all = all_presets();
    let mut custom = load_custom_presets(&app);
    // Push custom presets after built-in. Tag category with "Custom" if not set.
    for p in &mut custom {
        if p.category.trim().is_empty() {
            p.category = "Custom".to_string();
        }
    }
    all.extend(custom);
    all
}

// Runs off the main thread: spawns a process / hits the network, which
// would otherwise freeze the window (sync commands run on the UI thread).
#[tauri::command(async)]
fn open_custom_presets_file(app: AppHandle) -> Result<String, String> {
    let path = custom_presets_path(&app);
    // Create with a helpful template if it doesn't exist yet
    if !path.exists() {
        let template = r#"[
  {
    "id": 1001,
    "name": "Example: my fast 1080p",
    "desc": "Custom preset example",
    "info": "Edit this JSON to define your own presets. Reload Nibble after changes.",
    "specs": "H.264 CRF 22 · 1080p · custom",
    "suffix": "_my1080p",
    "ext": "mp4",
    "star": false,
    "category": "Custom",
    "args": [
      "-i", "{IN}",
      "-vf", "scale=-2:'min(1080,ih)'",
      "-c:v", "libx264", "-preset", "medium", "-crf", "22",
      "-pix_fmt", "yuv420p",
      "-c:a", "aac", "-b:a", "192k",
      "-movflags", "+faststart", "{OUT}"
    ],
    "params": [],
    "target_size_mb": null,
    "hdr_thumbnail": false
  }
]
"#;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, template).map_err(|e| e.to_string())?;
    }
    // Open in default editor (Notepad etc.) via reveal-style explorer call
    #[cfg(windows)]
    {
        let _ = cmd_hidden("cmd")
            .args(["/c", "start", "", path.to_str().unwrap_or("")])
            .spawn();
    }
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
fn get_history_cmd(app: AppHandle, max_age_days: Option<u32>) -> Vec<HistoryEntry> {
    // Entries whose file was deleted or moved since are dropped — the list is
    // a shortcut to your files, not a log. Old entries without a path stay.
    // Settings → "Keep Recent Activity for" also drops entries older than N days.
    let _guard = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let h = load_history(&app);
    let before = h.len();
    let cutoff = max_age_days.filter(|&d| d > 0).map(|d| chrono::Local::now() - chrono::Duration::days(d as i64));
    let kept: Vec<HistoryEntry> = h
        .into_iter()
        .filter(|e| e.path.as_deref().map_or(true, |p| std::path::Path::new(p).exists()))
        .filter(|e| match (cutoff, chrono::DateTime::parse_from_rfc3339(&e.when)) {
            (Some(c), Ok(t)) => t >= c,
            _ => true,
        })
        .collect();
    if kept.len() != before {
        save_history(&app, &kept);
    }
    kept
}

#[tauri::command]
fn clear_history(app: AppHandle) {
    save_history(&app, &[]);
}

#[tauri::command]
fn delete_history_entry(app: AppHandle, index: usize) {
    let mut h = load_history(&app);
    if index < h.len() {
        h.remove(index);
        save_history(&app, &h);
    }
}

/// Cancel returns IMMEDIATELY — the taskkill (which can take 100-300 ms) is
/// fired off on a separate OS thread so it doesn't tie up the IPC handler,
/// which would freeze the UI.
#[tauri::command]
fn cancel_conversion(state: State<AppState>) {
    state.cancel.store(true, Ordering::SeqCst);
    let pid = state.running_pid.load(Ordering::SeqCst);
    eprintln!("[cancel] flag set, current ffmpeg pid = {}", pid);
    if pid != 0 {
        std::thread::spawn(move || {
            #[cfg(windows)]
            {
                let result = cmd_hidden("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .output();
                match result {
                    Ok(o) if o.status.success() => {
                        eprintln!("[cancel] taskkill ok for pid {}", pid);
                    }
                    Ok(o) => eprintln!(
                        "[cancel] taskkill failed (exit {:?}): {}",
                        o.status.code(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    Err(e) => eprintln!("[cancel] taskkill spawn failed: {}", e),
                }
            }
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .output();
            }
        });
    }
}

// ============================================================
//  Watch folder
// ============================================================

#[tauri::command]
fn get_watch_config(app: AppHandle) -> WatchConfig {
    load_watch_config(&app)
}

#[tauri::command]
fn set_watch_config(
    app: AppHandle,
    state: State<AppState>,
    config: WatchConfig,
) -> Result<(), String> {
    save_watch_config(&app, &config);
    if config.enabled {
        start_watcher(app, state, config)
    } else {
        stop_watcher(state);
        Ok(())
    }
}

#[tauri::command]
fn stop_watch(state: State<AppState>) {
    stop_watcher(state);
}

fn stop_watcher(state: State<AppState>) {
    if let Ok(mut w) = state.watcher.lock() {
        *w = None;
    }
}

fn start_watcher(app: AppHandle, state: State<AppState>, cfg: WatchConfig) -> Result<(), String> {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc;

    if cfg.folder.trim().is_empty() {
        return Err("Watch folder is empty".into());
    }
    let folder_path = PathBuf::from(&cfg.folder);
    if !folder_path.is_dir() {
        return Err(format!("Folder does not exist: {}", cfg.folder));
    }

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| format!("watcher init: {}", e))?;
    watcher
        .watch(&folder_path, RecursiveMode::NonRecursive)
        .map_err(|e| format!("watcher watch: {}", e))?;

    if let Ok(mut w) = state.watcher.lock() {
        *w = Some(watcher);
    }

    // Spawn the receive loop in a worker thread
    let app_clone = app.clone();
    let cfg_clone = cfg.clone();
    // Suffix of this preset's output (e.g. "_H264"). We skip files that already
    // carry it so that converting into the SAME folder we're watching doesn't
    // re-detect our own output and loop forever (out → out_H264 → ...).
    let out_suffix = find_preset(cfg.preset_id).map(|p| p.suffix.to_string()).unwrap_or_default();
    std::thread::spawn(move || {
        // Files we've already started processing this session — guards against
        // duplicate notify events for the same path firing the conversion twice.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Ok(res) = rx.recv() {
            let Ok(event) = res else { continue };
            for path in event.paths {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let e = ext.to_ascii_lowercase();
                    if !matches!(e.as_str(), "mp4" | "mkv" | "mov" | "m4v" | "webm") {
                        continue;
                    }
                } else {
                    continue;
                }
                // Skip our own conversion outputs (filename stem ends with the
                // preset suffix) to prevent an infinite watch→convert→watch loop.
                if !out_suffix.is_empty() {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        if stem.ends_with(&out_suffix) {
                            continue;
                        }
                    }
                }
                let path_str = path.to_string_lossy().to_string();

                // De-dupe: notify often fires multiple events per file write.
                if !seen.insert(path_str.clone()) {
                    continue;
                }

                // Wait until the file is "stable" (size unchanged for ~1s) so we
                // don't try to convert mid-write.
                if !wait_for_stable_file(&path, 6, 200) {
                    seen.remove(&path_str); // allow a retry on a later event
                    continue;
                }

                // Notify the frontend so the user sees it queue up
                let _ = app_clone.emit("watch-detected", &path_str);

                // Run the conversion synchronously inside the worker thread
                let _ = run_single_conversion(
                    &app_clone,
                    cfg_clone.preset_id,
                    &path_str,
                    cfg_clone.output_dir.as_deref(),
                );
            }
        }
    });
    Ok(())
}

fn wait_for_stable_file(path: &std::path::Path, polls: u32, sleep_ms: u64) -> bool {
    let mut last_size: Option<u64> = None;
    let mut stable = 0u32;
    for _ in 0..polls {
        let size = std::fs::metadata(path).map(|m| m.len()).ok();
        if size.is_some() && size == last_size {
            stable += 1;
            if stable >= 2 {
                return true;
            }
        } else {
            stable = 0;
        }
        last_size = size;
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
    }
    false
}

/// Run a single conversion outside of the multi-file convert() command. Reuses
/// build_args + ffmpeg. Emits the same conversion-progress + file-done events
/// so the UI can pick them up via existing listeners (file_index = u32::MAX
/// indicates the source is the watcher, not the picker batch).
fn run_single_conversion(
    app: &AppHandle,
    preset_id: u32,
    in_file: &str,
    output_dir: Option<&str>,
) -> Result<(), String> {
    let preset = find_preset(preset_id).ok_or_else(|| "Unknown preset".to_string())?;
    let options: HashMap<String, String> = HashMap::new();
    let in_path = PathBuf::from(in_file);
    let parent: PathBuf = output_dir
        .filter(|d| !d.trim().is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| in_path.parent().map(|p| p.to_path_buf()).unwrap_or_default());
    let stem = in_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output")
        .to_string();
    // Overwrite: the watcher may legitimately re-process a file, but never
    // write over the input itself.
    let out_path = output_path_for(&parent, &stem, &preset.suffix, &preset.ext, &in_path, true);
    let out_name = out_path.file_name().and_then(|s| s.to_str()).unwrap_or("output").to_string();

    let meta = probe_metadata(in_file);
    let out_path_str = out_path.to_string_lossy().to_string();
    let preset_args = build_args(&preset, in_file, &out_path_str, &options, meta.duration_ms, meta.is_hdr, meta.fps)?;

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
    ];
    args.extend(preset_args);

    let status = cmd_tool("ffmpeg").args(&args).status()
        .map_err(|e| format!("ffmpeg spawn: {}", e))?;
    let ok = status.success() && out_path.exists();
    let size_mb = if ok {
        std::fs::metadata(&out_path).map(|m| m.len() as f64 / 1_048_576.0).unwrap_or(0.0)
    } else { 0.0 };

    let file_name = in_path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
    let _ = app.emit("watch-converted", serde_json::json!({
        "ok": ok,
        "in_file": file_name,
        "out_file": out_name.clone(),
        "size_mb": (size_mb * 10.0).round() / 10.0,
    }));

    // Record in history so the user sees it in Recent Activity
    push_history(app, HistoryEntry {
        when: chrono::Local::now().to_rfc3339(),
        preset: format!("{} (watcher)", preset.name),
        in_file: file_name,
        out_file: out_name,
        ok,
        secs: 0.0,
        size_mb: (size_mb * 10.0).round() / 10.0,
        path: ok.then(|| out_path_str.clone()),
    });
    Ok(())
}

/// Output file for a conversion: `<stem><suffix>.<ext>` in `dir`.
/// Never the input file itself (a custom preset with an empty suffix would
/// otherwise make ffmpeg `-y` overwrite the source mid-read), and with
/// `overwrite == false` an existing file is kept: "name (2).mp4", "(3)", …
fn output_path_for(dir: &std::path::Path, stem: &str, suffix: &str, ext: &str,
                   input: &std::path::Path, overwrite: bool) -> PathBuf {
    let same_as_input = |p: &std::path::Path| {
        p == input || (p.exists() && std::fs::canonicalize(p).ok() == std::fs::canonicalize(input).ok())
    };
    let base = if suffix.is_empty() { "_out" } else { suffix };
    let first = dir.join(format!("{}{}.{}", stem, base, ext));
    if !same_as_input(&first) && (overwrite || !first.exists()) {
        return first;
    }
    (2..10_000)
        .map(|n| dir.join(format!("{}{} ({}).{}", stem, base, n, ext)))
        .find(|p| !p.exists() && !same_as_input(p))
        .unwrap_or(first)
}

/// Async wrapper: the actual heavy work runs on a dedicated blocking task so
/// it can't possibly tie up Tauri's IPC handler thread (which would otherwise
/// queue up Cancel and freeze the UI during a long encode).
#[tauri::command]
async fn convert(
    app: AppHandle,
    preset_id: u32,
    files: Vec<String>,
    options: Option<HashMap<String, String>>,
    output_dir: Option<String>,
    overwrite: Option<bool>,
) -> Result<(), String> {
    let opts = options.unwrap_or_default();
    let overwrite = overwrite.unwrap_or(true);
    tauri::async_runtime::spawn_blocking(move || {
        run_convert_blocking(app, preset_id, files, opts, output_dir, overwrite)
    })
    .await
    .map_err(|e| format!("task join: {}", e))?
}

fn run_convert_blocking(
    app: AppHandle,
    preset_id: u32,
    files: Vec<String>,
    options: HashMap<String, String>,
    output_dir: Option<String>,
    overwrite: bool,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let preset = find_preset(preset_id).ok_or_else(|| "Unknown preset".to_string())?;
    let total = files.len();

    state.cancel.store(false, Ordering::SeqCst);

    let mut ok_count = 0usize;
    let mut fail_count = 0usize;

    for (idx, in_file) in files.iter().enumerate() {
        if state.cancel.load(Ordering::SeqCst) {
            break;
        }

        let in_path = PathBuf::from(in_file);
        // Output dir: explicit override → fall back to input file's folder.
        let parent: PathBuf = output_dir
            .as_ref()
            .filter(|d| !d.trim().is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| in_path.parent().map(|p| p.to_path_buf()).unwrap_or_default());
        let stem = in_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output")
            .to_string();
        let out_path = output_path_for(&parent, &stem, &preset.suffix, &preset.ext, &in_path, overwrite);
        let out_name = out_path.file_name().and_then(|s| s.to_str()).unwrap_or("output").to_string();

        let file_name = in_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();

        // Single probe call — gets duration AND HDR flag in one ffprobe spawn.
        let meta = probe_metadata(in_file);
        let total_ms = meta.duration_ms;

        // Build args via the helper (handles params, target_size, HDR thumbnail).
        // If the preset+options leave an unsubstituted placeholder (e.g. user
        // gave an invalid trim range so VBITRATE couldn't be computed), emit a
        // fail event for THIS file and move on — don't abort the whole batch.
        let out_path_str = out_path.to_string_lossy().to_string();
        let preset_args = match build_args(&preset, in_file, &out_path_str, &options, total_ms, meta.is_hdr, meta.fps) {
            Ok(v) => v,
            Err(e) => {
                fail_count += 1;
                let _ = app.emit(
                    "file-done",
                    FileDoneEvent {
                        file_index: idx,
                        file_name: file_name.clone(),
                        ok: false,
                        out_file: out_name.clone(),
                        out_path: out_path_str.clone(),
                        size_mb: 0.0,
                        secs: 0.0,
                        error: Some(e),
                    },
                );
                continue;
            }
        };
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-stats".into(),
            "-y".into(),
        ];
        args.extend(preset_args);
        let start = std::time::Instant::now();

        let mut child = cmd_tool("ffmpeg")
            .args(&args)
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn ffmpeg: {}", e))?;

        // Track the child's PID so cancel_conversion can kill it mid-encode.
        state.running_pid.store(child.id(), Ordering::SeqCst);

        let stderr = child.stderr.take().expect("ffmpeg stderr");
        let reader = BufReader::new(stderr);

        // Capture stderr lines NOT recognized as progress so we can surface real
        // ffmpeg error messages instead of just "exit code 1" when the run fails.
        let stderr_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let stderr_log_clone = stderr_log.clone();

        let app_clone = app.clone();
        let file_name_clone = file_name.clone();
        let preset_name_clone = preset.name.clone();
        let read_thread = std::thread::spawn(move || {
            // ffmpeg writes stats on stderr separated by \r — read by byte-chunks
            let mut buf = String::new();
            for byte in reader.bytes().flatten() {
                if byte == b'\r' || byte == b'\n' {
                    if !buf.is_empty() {
                        if let Some(mut ev) = parse_stats(&buf) {
                            ev.file_index = idx;
                            ev.total_files = total;
                            ev.file_name = file_name_clone.clone();
                            ev.preset_name = preset_name_clone.clone();
                            ev.total_ms = total_ms;
                            let _ = app_clone.emit("conversion-progress", ev);
                        } else {
                            // Non-progress line → likely an error/warning. Keep
                            // only the most recent ~5 to bound memory.
                            let mut log = stderr_log_clone.lock().unwrap();
                            log.push(buf.clone());
                            if log.len() > 5 {
                                let drop_n = log.len() - 5;
                                log.drain(..drop_n);
                            }
                        }
                        buf.clear();
                    }
                } else {
                    buf.push(byte as char);
                }
            }
        });

        let status = child.wait().map_err(|e| format!("wait failed: {}", e))?;
        let _ = read_thread.join();
        // Child has exited — clear the PID so a subsequent Cancel doesn't try
        // to kill an unrelated process that happens to inherit the same PID.
        state.running_pid.store(0, Ordering::SeqCst);

        let elapsed = start.elapsed().as_secs_f64();
        let ok = status.success() && out_path.exists();
        let size_mb = if ok {
            std::fs::metadata(&out_path)
                .map(|m| m.len() as f64 / 1_048_576.0)
                .unwrap_or(0.0)
        } else {
            0.0
        };

        // If the encode failed (including user-cancellation via taskkill), the
        // output file is half-written and useless. Delete it so the user doesn't
        // accidentally try to play it or have garbage left around.
        if !ok && out_path.exists() {
            match std::fs::remove_file(&out_path) {
                Ok(_) => eprintln!("[convert] removed partial output: {}", out_path.display()),
                Err(e) => eprintln!("[convert] could not remove partial output {}: {}", out_path.display(), e),
            }
        }

        if ok {
            ok_count += 1;
        } else {
            fail_count += 1;
        }

        let error = if ok {
            None
        } else {
            // Surface the actual ffmpeg error text (last few stderr lines)
            let log = stderr_log.lock().unwrap();
            let detail: String = log
                .iter()
                .filter(|l| !l.trim().is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" · ");
            Some(if detail.is_empty() {
                format!("exit code {:?}", status.code())
            } else {
                format!("exit {:?} · {}", status.code(), detail)
            })
        };
        let _ = app.emit(
            "file-done",
            FileDoneEvent {
                file_index: idx,
                file_name: file_name.clone(),
                ok,
                out_file: out_name.clone(),
                out_path: out_path_str.clone(),
                size_mb: (size_mb * 10.0).round() / 10.0,
                secs: (elapsed * 10.0).round() / 10.0,
                error,
            },
        );

        push_history(
            &app,
            HistoryEntry {
                when: chrono::Local::now().to_rfc3339(),
                preset: preset.name.clone(),
                in_file: file_name,
                out_file: out_name,
                ok,
                secs: (elapsed * 10.0).round() / 10.0,
                size_mb: (size_mb * 10.0).round() / 10.0,
                path: ok.then(|| out_path_str.clone()),
            },
        );
    }

    let _ = app.emit(
        "batch-done",
        BatchDoneEvent {
            ok_count,
            fail_count,
        },
    );

    Ok(())
}

// ---------- Entry ----------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .manage(AppState::default())
        // Live-preview stream proxy (see `serve_preview_stream`). Each request
        // gets its own thread: it blocks on the network.
        .register_asynchronous_uri_scheme_protocol("nstream", |_ctx, request, responder| {
            std::thread::spawn(move || responder.respond(serve_preview_stream(&request)));
        })
        // Screenshot overlay: the frozen frame being selected from.
        .register_uri_scheme_protocol("nshot", |_ctx, request| capture::serve(&request))
        .plugin(capture::shortcut_plugin())
        .invoke_handler(tauri::generate_handler![
            list_presets,
            get_history_cmd,
            clear_history,
            delete_history_entry,
            cancel_conversion,
            convert,
            generate_trim_thumbnails,
            app_info,
            share_copy,
            copy_to_clipboard,
            delete_file,
            share_clear,
            ytdlp_status,
            ytdlp_latest,
            ytdlp_update,
            ffmpeg_status,
            install_dependencies,
            ytdlp_info,
            ytdlp_preview,
            download_url,
            reveal_in_explorer,
            detect_hardware,
            open_custom_presets_file,
            ensure_hdr_proxy,
            get_watch_config,
            set_watch_config,
            stop_watch,
            cancel_download,
            open_author_page,
            open_file,
            capture::capture_start,
            capture::capture_finish,
            capture::capture_finish_layer,
            capture::system_fonts,
            capture::list_captures,
            record::record_start,
            record::record_stop,
            record::record_cancel,
            record::rec_ready,
            capture::capture_cancel,
            capture::capture_windows,
            capture::capture_ready,
            capture::capture_hide,
            capture::shot_popup_info,
            capture::shot_popup_ready,
            capture::shot_open,
            capture::shot_reveal,
            capture::shot_popup_close,
            capture::set_capture_config,
            set_remember_window,
            reset_window_size
        ])
        .setup(|app| {
            // Sweep quick-share leftovers from a previous run (crash / forced
            // kill skipped the exit cleanup). Background thread: never delays startup.
            std::thread::spawn(clear_share_files);

            // The main window is created here (tauri.conf.json has "create": false)
            // so a portable copy can keep its WebView2 profile — localStorage,
            // cookies, cache — inside "Nibble Data" instead of %LOCALAPPDATA%.
            let win_cfg = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .cloned()
                .ok_or("missing 'main' window config")?;
            let mut builder = tauri::WebviewWindowBuilder::from_config(app.handle(), &win_cfg)?;
            if let Some(root) = portable_root() {
                builder = builder.data_directory(root.join("WebView2"));
            }
            // Start hidden, size it to the monitor, then show — no visible jump.
            // A remembered size (opt-in setting) wins over the default.
            let window = builder.visible(false).build()?;
            let saved = load_window_state(app.handle());
            if !(saved.remember && restore_window(&window, &saved)) {
                fit_window_to_monitor(&window);
            }
            let _ = window.show();
            window.on_window_event({
                let w = window.clone();
                move |e| {
                    if let tauri::WindowEvent::CloseRequested { .. } = e {
                        save_window_geometry(&w);
                        // The hidden screenshot overlay would keep the app alive
                        w.app_handle().exit(0);
                    }
                }
            });
            capture::prepare_overlay(app.handle());
            capture::prepare_popup(app.handle());
            record::prepare(app.handle());
            std::thread::spawn(|| prune_hdr_proxies(HDR_PROXY_KEEP));

            // Windows 11: ask DWM to round the window's corners. Without this,
            // `decorations: false` + `transparent: true` strips the default
            // rounding and leaves square corners on the outer window.
            #[cfg(target_os = "windows")]
            {
                use windows::Win32::Foundation::HWND;
                use windows::Win32::Graphics::Dwm::{
                    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
                };
                if let Some(window) = app.get_webview_window("main") {
                    if let Ok(handle) = window.hwnd() {
                        let hwnd = HWND(handle.0 as *mut _);
                        let pref: u32 = DWMWCP_ROUND.0 as u32;
                        unsafe {
                            let _ = DwmSetWindowAttribute(
                                hwnd,
                                DWMWA_WINDOW_CORNER_PREFERENCE,
                                &pref as *const u32 as *const _,
                                std::mem::size_of::<u32>() as u32,
                            );
                        }
                    }
                }
            }

            // Re-enable watch folder if it was active in a previous session
            let handle = app.handle().clone();
            let cfg = load_watch_config(&handle);
            if cfg.enabled && !cfg.folder.trim().is_empty() {
                let state = handle.state::<AppState>();
                if let Err(e) = start_watcher(handle.clone(), state, cfg) {
                    eprintln!("Watcher auto-start failed: {}", e);
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // Quick-share files must not outlive the app.
            if let tauri::RunEvent::Exit = event {
                clear_share_files();
            }
        });
}
