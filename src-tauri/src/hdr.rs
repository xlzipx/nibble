// ============================================================
//  HDR displays
//
//  Windows composes an HDR desktop in scRGB float (1.0 = 80 nits). SDR
//  windows top out at the "SDR content brightness" white (e.g. 480 nits =
//  6.0); HDR games and videos go far above it, while their mid-tones sit at
//  only ~10–200 nits. Two conversions to an SDR picture:
//   - SDR content: divide by SDR white → exactly what GDI / BitBlt gives.
//   - HDR content: reference white 203 nits (BT.2408) → 1.0 with a soft
//     shoulder for the highlights. Dividing by SDR white instead looks right
//     on this very monitor but far too dark everywhere else (YouTube, phones);
//     GDI clips HDR content → washed out.
//  A window counts as HDR when it has pixels above SDR white.
// ============================================================

/// scRGB → linear relative to the 203-nit reference white
pub(crate) const HDR_SCALE: f32 = 80.0 / 203.0;
/// Shoulder start (linear output): below it HDR content maps 1:1
const KNEE: f32 = 0.6;

#[inline]
fn shoulder(m: f32) -> f32 {
    if m <= KNEE { m } else { KNEE + (1.0 - KNEE) * (1.0 - (-(m - KNEE) / (1.0 - KNEE)).exp()) }
}

/// The same curve as a libplacebo user shader (video recordings), applied to
/// the linear scRGB frame before libplacebo encodes it as sRGB.
pub(crate) fn tonemap_shader() -> String {
    format!(
        "//!HOOK MAIN\n//!BIND HOOKED\n//!DESC nibble hdr to sdr\n\
         vec4 hook() {{\n\
         \x20   vec4 c = HOOKED_texOff(0);\n\
         \x20   vec3 v = max(c.rgb, vec3(0.0)) * {s:.6};\n\
         \x20   float m = max(max(v.r, v.g), max(v.b, 1e-6));\n\
         \x20   float o = m <= {k:.3} ? m : {k:.3} + {r:.3} * (1.0 - exp(-(m - {k:.3}) / {r:.3}));\n\
         \x20   c.rgb = v * (o / m);\n\
         \x20   return c;\n}}\n",
        s = HDR_SCALE, k = KNEE, r = 1.0 - KNEE
    )
}

/// A monitor frame from an HDR desktop, already converted to 8-bit.
pub(crate) struct HdrFrame {
    pub(crate) bgra: Vec<u8>,
    /// Per pixel: 1 = HDR content (tonemapped), 0 = SDR
    pub(crate) mask: Vec<u8>,
}

/// Share of `rect` (frame px) covered by HDR content.
pub(crate) fn hdr_share(mask: &[u8], frame_w: u32, rect: (u32, u32, u32, u32)) -> f64 {
    let (x, y, w, h) = rect;
    if mask.is_empty() || w == 0 || h == 0 {
        return 0.0;
    }
    let mut n = 0u64;
    for row in y..y + h {
        let start = (row * frame_w + x) as usize;
        if let Some(r) = mask.get(start..start + w as usize) {
            n += r.iter().map(|&v| v as u64).sum::<u64>();
        }
    }
    n as f64 / (w as f64 * h as f64)
}

/// Grab the monitor in float and convert it. `windows` = visible window rects
/// in frame coordinates, topmost first. None when the monitor isn't in HDR
/// mode or anything fails (the caller keeps its GDI frame).
#[cfg(windows)]
pub(crate) fn grab(mon: (i32, i32, u32, u32), windows: &[(i32, i32, i32, i32)]) -> Option<HdrFrame> {
    let (_, hdr_on) = crate::record::dxgi_output(mon.0, mon.1)?;
    if !hdr_on {
        return None;
    }
    let white = crate::record::sdr_white_scale(mon.0, mon.1).unwrap_or(1.0).clamp(1.0, 12.5) as f32;
    let (w, h, half) = grab_float(mon)?;
    if w != mon.2 || h != mon.3 {
        return None;
    }
    Some(convert(w, h, &half, white, windows))
}

#[cfg(not(windows))]
pub(crate) fn grab(_: (i32, i32, u32, u32), _: &[(i32, i32, i32, i32)]) -> Option<HdrFrame> {
    None
}

fn half_lut() -> &'static [f32] {
    static LUT: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        (0..=u16::MAX)
            .map(|u| {
                let (s, e, m) = ((u >> 15) & 1, ((u >> 10) & 31) as i32, (u & 1023) as f32);
                let v = if e == 0 {
                    m / 1024.0 * 2f32.powi(-14)
                } else if e == 31 {
                    65504.0
                } else {
                    (1.0 + m / 1024.0) * 2f32.powi(e - 15)
                };
                if s == 1 { -v } else { v }
            })
            .collect()
    })
}

const SRGB_STEPS: usize = 16384;
fn srgb_lut() -> &'static [u8] {
    static LUT: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    LUT.get_or_init(|| {
        (0..=SRGB_STEPS)
            .map(|i| {
                let x = i as f32 / SRGB_STEPS as f32;
                let v = if x <= 0.003_130_8 { 12.92 * x } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 };
                (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8
            })
            .collect()
    })
}

/// rgba16f (half floats, 4 per pixel) → BGRA8 + HDR mask
fn convert(w: u32, h: u32, half: &[u16], white: f32, windows: &[(i32, i32, i32, i32)]) -> HdrFrame {
    let (wu, hu) = (w as usize, h as usize);
    let n = wu * hu;
    let lut = half_lut();
    let srgb = srgb_lut();
    // Which window owns each pixel (topmost wins); 255 = desktop
    let mut owner = vec![255u8; n];
    for (i, &(x, y, ww, wh)) in windows.iter().enumerate().take(254) {
        let (x0, y0) = (x.max(0) as usize, y.max(0) as usize);
        let (x1, y1) = (((x + ww).max(0) as usize).min(wu), ((y + wh).max(0) as usize).min(hu));
        for row in y0..y1 {
            for o in &mut owner[row * wu + x0..row * wu + x1.max(x0)] {
                if *o == 255 {
                    *o = i as u8;
                }
            }
        }
    }
    // A window is HDR when enough of its pixels go above SDR white
    let thr = white * 1.05;
    let mut vis = vec![0u32; 256];
    let mut hot = vec![0u32; 256];
    for p in 0..n {
        let o = owner[p] as usize;
        vis[o] += 1;
        let q = &half[p * 4..p * 4 + 3];
        if lut[q[0] as usize] > thr || lut[q[1] as usize] > thr || lut[q[2] as usize] > thr {
            hot[o] += 1;
        }
    }
    let is_hdr: Vec<bool> = (0..256)
        .map(|i| i != 255 && hot[i] >= 16 && hot[i] as f64 >= vis[i] as f64 * 0.0002)
        .collect();

    let mut bgra = vec![0u8; n * 4];
    let mut mask = vec![0u8; n];
    let inv_white = 1.0 / white;
    let enc = |v: f32| srgb[(v.clamp(0.0, 1.0) * SRGB_STEPS as f32 + 0.5) as usize];
    for p in 0..n {
        let q = &half[p * 4..p * 4 + 3];
        let (mut r, mut g, mut b) = (lut[q[0] as usize].max(0.0), lut[q[1] as usize].max(0.0), lut[q[2] as usize].max(0.0));
        if is_hdr[owner[p] as usize] {
            mask[p] = 1;
            r *= HDR_SCALE;
            g *= HDR_SCALE;
            b *= HDR_SCALE;
            let m = r.max(g).max(b).max(1e-6);
            let f = shoulder(m) / m;
            r *= f;
            g *= f;
            b *= f;
        } else {
            r *= inv_white;
            g *= inv_white;
            b *= inv_white;
        }
        let o = p * 4;
        bgra[o] = enc(b);
        bgra[o + 1] = enc(g);
        bgra[o + 2] = enc(r);
        bgra[o + 3] = 255;
    }
    HdrFrame { bgra, mask }
}

/// One frame of the monitor via DXGI Desktop Duplication in R16G16B16A16_FLOAT.
#[cfg(windows)]
fn grab_float(mon: (i32, i32, u32, u32)) -> Option<(u32, u32, Vec<u16>)> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut found = None;
        let mut ai = 0u32;
        'outer: while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let mut oi = 0u32;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                if let Ok(d) = output.GetDesc() {
                    if d.DesktopCoordinates.left == mon.0 && d.DesktopCoordinates.top == mon.1 {
                        found = Some((adapter.clone(), output));
                        break 'outer;
                    }
                }
                oi += 1;
            }
            ai += 1;
        }
        let (adapter, output) = found?;
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter, D3D_DRIVER_TYPE_UNKNOWN, Default::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut ctx),
        ).ok()?;
        let (device, ctx) = (device?, ctx?);
        let dup = output.cast::<IDXGIOutput5>().ok()?
            .DuplicateOutput1(&device, 0, &[DXGI_FORMAT_R16G16B16A16_FLOAT]).ok()?;
        let dd = dup.GetDesc();
        if dd.Rotation != DXGI_MODE_ROTATION_IDENTITY && dd.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
            return None; // rotated monitor: the texture isn't in desktop orientation
        }
        // The first frame of a new duplication can be empty (black, only the
        // pointer changed: LastPresentTime 0) → wait for a real one. A desktop
        // where nothing moves may never send one → None, GDI is used instead.
        let mut res: Option<IDXGIResource> = None;
        let mut got = false;
        for _ in 0..6 {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut r: Option<IDXGIResource> = None;
            if dup.AcquireNextFrame(60, &mut info, &mut r).is_err() {
                continue;
            }
            if info.LastPresentTime != 0 {
                res = r;
                got = true;
                break;
            }
            let _ = dup.ReleaseFrame();
        }
        if !got {
            return None;
        }
        let tex: ID3D11Texture2D = match res.and_then(|r| r.cast().ok()) {
            Some(t) => t,
            None => {
                let _ = dup.ReleaseFrame();
                return None;
            }
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging: Option<ID3D11Texture2D> = None;
        let made = device.CreateTexture2D(&desc, None, Some(&mut staging));
        let Some(staging) = staging.filter(|_| made.is_ok()) else {
            let _ = dup.ReleaseFrame();
            return None;
        };
        ctx.CopyResource(&staging, &tex);
        let _ = dup.ReleaseFrame();
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map)).ok()?;
        let (w, h) = (desc.Width, desc.Height);
        let mut out = vec![0u16; (w * h * 4) as usize];
        for row in 0..h as usize {
            let src = (map.pData as *const u8).add(row * map.RowPitch as usize) as *const u16;
            std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(row * w as usize * 4), w as usize * 4);
        }
        ctx.Unmap(&staging, 0);
        Some((w, h, out))
    }
}
