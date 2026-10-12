use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ParamSpec {
    pub name: String,         // e.g. "start", "end"
    pub label: String,        // e.g. "Start time"
    pub default_value: String,
    pub kind: String,         // "time" | "text" | "number"
    pub placeholder: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Preset {
    pub id: u32,
    pub name: String,
    pub desc: String,           // short, shown next to preset name
    pub info: String,           // long, shown in info panel
    pub specs: String,          // single-line tech specs ("H.265 · 60fps · ~40 MB/min")
    pub suffix: String,
    pub ext: String,
    pub star: bool,
    pub category: String,
    pub args: Vec<String>,
    pub params: Vec<ParamSpec>,
    pub target_size_mb: Option<u32>,
    pub hdr_thumbnail: bool,    // special: auto-detect HDR input for thumbnail preset
}

impl Preset {
    fn new(
        id: u32, name: &str, desc: &str,
        suffix: &str, ext: &str, cat: &str, args: &[&str],
    ) -> Self {
        Self {
            id,
            name: name.into(),
            desc: desc.into(),
            info: String::new(),
            specs: String::new(),
            suffix: suffix.into(),
            ext: ext.into(),
            star: false,
            category: cat.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            params: Vec::new(),
            target_size_mb: None,
            hdr_thumbnail: false,
        }
    }

    fn info(mut self, s: &str) -> Self { self.info = s.into(); self }
    fn specs(mut self, s: &str) -> Self { self.specs = s.into(); self }
    fn star(mut self) -> Self { self.star = true; self }
    fn params(mut self, p: Vec<ParamSpec>) -> Self { self.params = p; self }
    fn target_size(mut self, mb: u32) -> Self { self.target_size_mb = Some(mb); self }
    fn hdr_thumb(mut self) -> Self { self.hdr_thumbnail = true; self }
}

fn time_param(name: &str, label: &str, default: &str) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        label: label.into(),
        default_value: default.into(),
        kind: "time".into(),
        placeholder: "HH:MM:SS or seconds".into(),
    }
}

fn number_param(name: &str, label: &str, default: &str, placeholder: &str) -> ParamSpec {
    ParamSpec {
        name: name.into(),
        label: label.into(),
        default_value: default.into(),
        kind: "number".into(),
        placeholder: placeholder.into(),
    }
}

// Encoder tokens (resolved in lib.rs::build_args against the detected GPU):
//   {HWDECODE}        → hardware decode flags (CUDA / auto / none)
//   {VENC:hevc:22}    → best available H.265 encoder at quality 22
//   {VENC:h264:20}    → best available H.264 encoder at quality 20
// These make every "GPU" preset run on NVIDIA (NVENC), AMD (AMF), Intel (QSV),
// or fall back to CPU (libx264/libx265) — instead of failing on non-NVIDIA.

pub fn all_presets() -> Vec<Preset> {
    vec![
        // =========================================================
        //  CONVERT — the everyday, universal re-encode. Hardware-adaptive,
        //  keeps the source frame rate. This is what most people want.
        // =========================================================
        Preset::new(1,
            "Convert to H.265",
            "Smaller files, great quality",
            "_h265", "mp4", "Convert",
            &[
                "{HWDECODE}", "-i", "{IN}",
                "{VENC:hevc:22}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "192k",
                "-movflags", "+faststart", "{OUT}",
            ])
            .star()
            .info("Re-encodes to H.265 (HEVC) at high quality, keeping the source resolution and frame rate. Uses your GPU's hardware encoder when available (NVIDIA NVENC, AMD AMF, Intel QSV) and falls back to CPU (libx265) otherwise. Best balance of quality and file size for general use, sharing, and editing.")
            .specs("H.265 · hardware-adaptive · keeps fps · CQ/CRF 22"),

        Preset::new(2,
            "Convert to H.264",
            "Maximum compatibility, plays everywhere",
            "_h264", "mp4", "Convert",
            &[
                "{HWDECODE}", "-i", "{IN}",
                "{VENC:h264:20}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "192k",
                "-movflags", "+faststart", "{OUT}",
            ])
            .info("Re-encodes to H.264 — the most universally compatible codec. Plays on every device, browser, and older editing software. Slightly larger than H.265 at the same quality. Hardware-accelerated when possible (NVENC/AMF/QSV), CPU (libx264) otherwise.")
            .specs("H.264 · hardware-adaptive · keeps fps · CQ/CRF 20"),

        // =========================================================
        //  COMPRESS — fit under a size budget (Discord / Slack / email).
        //  CPU libx264 with computed bitrate for accurate size targeting.
        // =========================================================
        Preset::new(13,
            "Discord  ≤ 20 MB",
            "Free Discord cap — trim a section for quality",
            "_discord20", "mp4", "Compress",
            &[
                "-ss", "{start}", "-to", "{end}",
                "-i", "{IN}",
                "-c:v", "libx264", "-preset", "medium",
                "-b:v", "{VBITRATE}", "-maxrate", "{VMAXRATE}", "-bufsize", "{VBUFSIZE}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "{AUDIO_KBPS}",
                "-movflags", "+faststart", "{OUT}",
            ])
            .target_size(20)
            .params(vec![
                time_param("start", "Start time", "00:00:00"),
                time_param("end",   "End time",   "00:00:20"),
            ])
            .info("Free Discord caps uploads at 20 MB per file (raised from 10 MB in August 2026). Use the visual trimmer to pick the highlight you actually want to share — bitrate is computed from the SELECTED length, so a 20-second trim looks great while compressing 3 minutes into 20 MB will look rough. Audio at 96 kbps."),

        Preset::new(14,
            "Discord  ≤ 50 MB",
            "Nitro Basic cap — trim or whole clip",
            "_discord50", "mp4", "Compress",
            &[
                "-ss", "{start}", "-to", "{end}",
                "-i", "{IN}",
                "-c:v", "libx264", "-preset", "medium",
                "-b:v", "{VBITRATE}", "-maxrate", "{VMAXRATE}", "-bufsize", "{VBUFSIZE}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "{AUDIO_KBPS}",
                "-movflags", "+faststart", "{OUT}",
            ])
            .target_size(50)
            .params(vec![
                time_param("start", "Start time", "00:00:00"),
                time_param("end",   "End time",   "00:00:50"),
            ])
            .info("Nitro Basic caps uploads at 50 MB. Quality budget is 2.5× higher than the free tier, so a 30-second trim looks excellent. Same trim flow — drop in, pick the section, compress. Audio at 128 kbps."),

        Preset::new(21,
            "Discord  ≤ 500 MB",
            "Full Nitro cap — high-quality compressed share",
            "_discord500", "mp4", "Compress",
            &[
                "-ss", "{start}", "-to", "{end}",
                "-i", "{IN}",
                "-c:v", "libx264", "-preset", "medium",
                "-b:v", "{VBITRATE}", "-maxrate", "{VMAXRATE}", "-bufsize", "{VBUFSIZE}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "{AUDIO_KBPS}",
                "-movflags", "+faststart", "{OUT}",
            ])
            .target_size(500)
            .params(vec![
                time_param("start", "Start time", "00:00:00"),
                time_param("end",   "End time",   "00:02:00"),
            ])
            .info("Full Nitro caps uploads at 500 MB. Plenty of room for high-quality 1080p60 clips up to ~5 minutes, or shorter clips with near-source quality. Audio at 192 kbps. Use trim for longer source material."),

        Preset::new(15,
            "Compress to size",
            "Auto-compress to any target size in MB",
            "_sized", "mp4", "Compress",
            &[
                "-i", "{IN}",
                "-c:v", "libx264", "-preset", "medium",
                "-b:v", "{VBITRATE}", "-maxrate", "{VMAXRATE}", "-bufsize", "{VBUFSIZE}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "{AUDIO_KBPS}",
                "-movflags", "+faststart", "{OUT}",
            ])
            .params(vec![
                number_param("size_mb", "Target size (MB)", "100", "e.g. 100"),
            ])
            .info("Computes the video bitrate so the whole file lands just under your chosen MB target. Useful for Twitter (512 MB), Slack (1 GB), email attachments, etc. To compress a *section*, run Trim first, then Compress to size on the trimmed clip."),

        // =========================================================
        //  RESIZE — downscale to a target resolution. Hardware-adaptive.
        // =========================================================
        Preset::new(16,
            "Downscale to 1080p",
            "Fit to 1920×1080 max, keeps aspect",
            "_1080p", "mp4", "Resize",
            &[
                "{HWDECODE}", "-i", "{IN}",
                "-vf", "scale=-2:'min(1080,ih)'",
                "{VENC:hevc:23}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "192k",
                "-movflags", "+faststart", "{OUT}",
            ])
            .info("Downscales the video to fit within 1920×1080 while keeping aspect ratio. Useful for sharing 4K clips at 1080p to save bandwidth. Hardware-adaptive H.265 (NVENC/AMF/QSV/CPU)."),

        Preset::new(17,
            "Downscale to 720p",
            "Fit to 1280×720 max",
            "_720p", "mp4", "Resize",
            &[
                "{HWDECODE}", "-i", "{IN}",
                "-vf", "scale=-2:'min(720,ih)'",
                "{VENC:hevc:24}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "128k",
                "-movflags", "+faststart", "{OUT}",
            ])
            .info("Downscales to 720p height (1280×720 typical). Good for quick mobile previews or low-bandwidth sharing. Hardware-adaptive H.265."),

        Preset::new(18,
            "Downscale to 480p",
            "Fit to 854×480 — minimal size",
            "_480p", "mp4", "Resize",
            &[
                "{HWDECODE}", "-i", "{IN}",
                "-vf", "scale=-2:'min(480,ih)'",
                "{VENC:hevc:26}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "96k",
                "-movflags", "+faststart", "{OUT}",
            ])
            .info("Downscales to 480p — smallest practical resolution. Useful for archival proxies or text-only previews where image fidelity isn't critical."),

        // =========================================================
        //  TRIM — cut a section by start/end time.
        // =========================================================
        Preset::new(11,
            "Trim section (fast)",
            "Cut a section, no re-encode",
            "_trim", "mp4", "Trim",
            &[
                "-ss", "{start}", "-to", "{end}",
                "-i", "{IN}",
                "-c", "copy", "-avoid_negative_ts", "make_zero",
                "-movflags", "+faststart", "{OUT}",
            ])
            .params(vec![
                time_param("start", "Start time", "00:00:00"),
                time_param("end",   "End time",   "00:00:10"),
            ])
            .info("Cuts the clip between the given start and end times without re-encoding. Very fast (seconds), but may snap to the nearest keyframe — so the actual cut might land slightly before your start time. Use the precise version for frame-exact cuts.")
            .specs("source codec passthrough · keyframe-aligned · instant"),

        Preset::new(12,
            "Trim section (precise)",
            "Cut a section, frame-exact (re-encodes)",
            "_trim_precise", "mp4", "Trim",
            &[
                "-ss", "{start}", "-to", "{end}",
                "-i", "{IN}",
                "-c:v", "libx264", "-preset", "veryfast", "-crf", "18",
                "-c:a", "aac", "-b:a", "192k",
                "-movflags", "+faststart", "{OUT}",
            ])
            .params(vec![
                time_param("start", "Start time", "00:00:00"),
                time_param("end",   "End time",   "00:00:10"),
            ])
            .info("Cuts the clip with frame-exact precision by re-encoding with H.264. Slower than the fast version (5-30 seconds), but the cut lands exactly where you asked.")
            .specs("H.264 · frame-exact · 5-30s to run"),

        // =========================================================
        //  AUDIO — extract or normalize.
        // =========================================================
        Preset::new(7,
            "Extract audio (M4A)",
            "Pull audio out as .m4a — no re-encode",
            "_audio", "m4a", "Audio",
            &["-i", "{IN}", "-vn", "-c:a", "copy", "{OUT}"])
            .info("Extracts the audio track as-is, no re-encoding. Output is an .m4a (AAC stream copied directly from the original). Instant — no quality change. Works best when the source audio is already AAC (most MP4s); for other codecs use a re-encode.")
            .specs("audio passthrough · no re-encode · seconds to run"),

        Preset::new(20,
            "Audio normalize",
            "Match broadcast loudness (EBU R128, -16 LUFS)",
            "_loudnorm", "mp4", "Audio",
            &[
                "-i", "{IN}",
                "-c:v", "copy",
                "-af", "loudnorm=I=-16:TP=-1.5:LRA=11",
                "-c:a", "aac", "-b:a", "192k",
                "-movflags", "+faststart", "{OUT}",
            ])
            .info("Normalizes audio to the EBU R128 broadcast standard (-16 LUFS integrated). Useful for evening out volume across clips from different sources. Video stream is copied as-is — no quality loss on the picture."),

        // =========================================================
        //  EFFECTS — speed change.
        // =========================================================
        Preset::new(19,
            "Speed change",
            "Slow-mo or time-lapse (0.25× to 4×)",
            "_speed", "mp4", "Effects",
            &[
                "-i", "{IN}",
                "-filter_complex", "[0:v]setpts={vpts}*PTS[v];[0:a]{atempo_chain}[a]",
                "-map", "[v]", "-map", "[a]",
                "-c:v", "libx264", "-preset", "fast", "-crf", "20",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "192k",
                "-movflags", "+faststart", "{OUT}",
            ])
            .params(vec![
                number_param("speed", "Speed multiplier", "2.0", "0.25 — 4.0  (>1 = faster)"),
            ])
            .info("Changes playback speed. >1 = time-lapse (faster), <1 = slow-motion. Audio pitch is preserved (atempo), and extreme factors like 4× or 0.25× are handled automatically by chaining multiple atempo stages. Video uses setpts to retime frames."),

        // =========================================================
        //  UTILITIES — quick container/track operations.
        // =========================================================
        Preset::new(8,
            "Mute video",
            "Strip audio track",
            "_muted", "mp4", "Utility",
            &["-i", "{IN}", "-an", "-c:v", "copy", "-movflags", "+faststart", "{OUT}"])
            .info("Drops the audio track. Video stream is copied as-is (no quality loss). Useful when you're laying your own music over the clip. ⚠ Video codec is preserved — if the source is AV1, the muted output stays AV1.")
            .specs("video passthrough · audio removed · seconds to run"),

        Preset::new(9,
            "Remux to MP4",
            "Repack container — no re-encode, instant",
            "_remux", "mp4", "Utility",
            &["-i", "{IN}", "-c", "copy", "-movflags", "+faststart", "{OUT}"])
            .info("Re-wraps the streams into an MP4 container without re-encoding (e.g. MKV with H.264 → MP4 with H.264). Quality identical to the source. ⚠ Codec is unchanged — if your source is AV1, the output stays AV1. Use Convert for a real transcode.")
            .specs("full passthrough · no re-encode · seconds to run"),

        Preset::new(10,
            "Thumbnail PNG",
            "Representative frame as PNG (covers, refs)",
            "_thumb", "png", "Utility",
            &["-i", "{IN}", "-vf", "thumbnail,scale=1920:-1", "-frames:v", "1", "{OUT}"])
            .hdr_thumb()
            .info("Picks the most representative frame from the first ~100 frames using the 'thumbnail' filter (based on histogram analysis). Output PNG at 1920px wide, aspect preserved. Auto-detects HDR input and applies tonemapping so the image looks correct (not washed-out).")
            .specs("single PNG · 1920px wide · HDR-aware"),

        // =========================================================
        //  SHADOWPLAY → AFTER EFFECTS  (the original use case)
        //
        //  NVIDIA Shadowplay records gameplay as AV1, often HDR10 and at
        //  120fps — which After Effects can't read. These presets tonemap
        //  HDR→SDR Rec.709, re-encode to AE-friendly H.265, and cap 120→60fps
        //  (a 120fps source halves; a 60fps source is left alone). On NVIDIA
        //  they use NVENC + CUDA decode; on other GPUs they still work via
        //  AMF/QSV/CPU.
        // =========================================================
        Preset::new(3,
            "HDR → SDR · AE-ready",
            "HDR Shadowplay → After Effects (60fps cap)",
            "_SDR60", "mp4", "Shadowplay",
            &[
                "{HWDECODE}", "-i", "{IN}",
                "-vf", "fps=60,zscale=tin=smpte2084:min=bt2020nc:pin=bt2020:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p",
                "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv",
                "{VENC:hevc:22}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "{OUT}",
            ])
            .info("Tonemaps HDR10 (BT.2020 / PQ) to SDR Rec.709 using the Hable curve, then re-encodes to AE-friendly H.265. Frame rate is capped at 60fps (a 120fps capture drops to 60; a source already ≤60fps keeps its native rate — no frame duplication). After Effects sees a standard SDR clip — no Interpret Footage tweaks needed.")
            .specs("H.265 · hardware-adaptive · ≤60 fps · SDR Rec.709"),

        Preset::new(4,
            "HDR → SDR · keep 120fps",
            "HDR + keeps full frame rate for slow-mo",
            "_SDR120", "mp4", "Shadowplay",
            &[
                "{HWDECODE}", "-i", "{IN}",
                "-vf", "zscale=tin=smpte2084:min=bt2020nc:pin=bt2020:t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p",
                "-color_primaries", "bt709", "-color_trc", "bt709", "-colorspace", "bt709", "-color_range", "tv",
                "{VENC:hevc:22}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "{OUT}",
            ])
            .info("Same Hable tonemap as the AE-ready version, but keeps the original frame rate (e.g. 120fps). Use when you want to slow the clip down 2× in AE for smooth 60fps slow-mo.")
            .specs("H.265 · hardware-adaptive · keeps fps · SDR Rec.709"),

        Preset::new(5,
            "SDR Shadowplay → AE-ready",
            "Non-HDR AV1 capture → AE-friendly H.265",
            "_AE60", "mp4", "Shadowplay",
            &[
                "{HWDECODE}", "-i", "{IN}",
                "-vf", "fps=60",
                "{VENC:hevc:22}",
                "-pix_fmt", "yuv420p",
                "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "{OUT}",
            ])
            .info("For non-HDR Shadowplay captures (already SDR). Re-encodes AV1 → H.265 so After Effects can read it, capping 120fps → 60fps. No tonemapping (source is already SDR). A 60fps source keeps its rate.")
            .specs("H.265 · hardware-adaptive · ≤60 fps · SDR"),
    ]
}

pub fn find_preset(id: u32) -> Option<Preset> {
    all_presets().into_iter().find(|p| p.id == id)
}
