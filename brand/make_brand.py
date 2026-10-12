"""Regenerate all Nibble brand assets from brand/*-source.png.

Usage (from the repo root):
    python brand/make_brand.py . [preview_dir]

Outputs:
  src/assets/logo.png                      wordmark for the UI (720 px wide)
  src/assets/mark.png                      play mark, 128 px (titlebar, loader)
  src-tauri/icons/*                        app icons (png / ico / icns)
  src-tauri/installer-assets/sidebar*.bmp  NSIS welcome/finish panel, 164x314 @100 % + per-DPI copies
  src-tauri/installer-assets/header*.bmp   NSIS header strip, 150x57 @100 % + per-DPI copies
  [preview_dir]/*.png                      optional upscaled previews for QA

After changing icons, clear the Rust build-script cache before building or the
old icon stays embedded in the exe:  rm -rf src-tauri/target/release/build/nibble-*
"""
import sys
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont, ImageFilter

ROOT = Path(sys.argv[1])
OUT_PREVIEW = Path(sys.argv[2]) if len(sys.argv) > 2 else None
ASSETS = ROOT / "src" / "assets"
ICONS = ROOT / "src-tauri" / "icons"
INST = ROOT / "src-tauri" / "installer-assets"

BG = (14, 15, 19)          # #0e0f13 — matches app + installer dialog bg
TEXT = (233, 235, 240)
DIM = (138, 143, 156)
ACCENT = (255, 59, 92)     # pink-red from the logo gradient

def trim(im):
    return im.crop(im.getbbox())

def square(im, pad_ratio=0.0):
    w, h = im.size
    side = int(max(w, h) * (1 + 2 * pad_ratio))
    c = Image.new("RGBA", (side, side), (0, 0, 0, 0))
    c.alpha_composite(im, ((side - w) // 2, (side - h) // 2))
    return c

def font(size, bold=False):
    names = ["segoeuib.ttf" if bold else "segoeui.ttf", "arialbd.ttf" if bold else "arial.ttf"]
    for n in names:
        p = Path("C:/Windows/Fonts") / n
        if p.exists():
            return ImageFont.truetype(str(p), size)
    return ImageFont.load_default()

logo = trim(Image.open(ROOT / "brand" / "logo-source.png").convert("RGBA"))
mark = trim(Image.open(ROOT / "brand" / "icon-source.png").convert("RGBA"))

# --- UI assets -------------------------------------------------------------
lw = 720
logo.resize((lw, round(logo.height * lw / logo.width)), Image.LANCZOS).save(ASSETS / "logo.png", optimize=True)
square(mark).resize((128, 128), Image.LANCZOS).save(ASSETS / "mark.png", optimize=True)

# --- App icons -------------------------------------------------------------
master = square(mark, pad_ratio=0.03).resize((1024, 1024), Image.LANCZOS)
def sz(n):
    return master.resize((n, n), Image.LANCZOS)
sz(512).save(ICONS / "icon.png")
sz(32).save(ICONS / "32x32.png")
sz(128).save(ICONS / "128x128.png")
sz(256).save(ICONS / "128x128@2x.png")
sz(256).save(ICONS / "icon.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
master.save(ICONS / "icon.icns")

# --- Installer art ---------------------------------------------------------
def streaks(img, corner):
    """Soft diagonal red light streaks like the app background."""
    w, h = img.size
    layer = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    if corner == "bl":
        L = w * 0.55
        t = w * 0.07
        for off, a in [(0, 75), (w * 0.12, 38)]:
            x0 = -w * 0.18 + off
            d.polygon([(x0, h), (x0 + t, h), (x0 + t + L, h - L), (x0 + L, h - L)], fill=ACCENT + (a,))
    else:  # tr
        for off, a in [(0, 60), (20, 30)]:
            x0 = w - 30 - off
            d.polygon([(x0, 0), (x0 + 26, 0), (x0 + 26 - 90, 90), (x0 - 90, 90)], fill=ACCENT + (a,))
    layer = layer.filter(ImageFilter.GaussianBlur(6))
    img.alpha_composite(layer)

def centered_text(d, cx, y, s, f, fill):
    l, t, r, b = d.textbbox((0, 0), s, font=f)
    d.text((cx - (r - l) / 2 - l, y), s, font=f, fill=fill)

# The installer is DPI-aware, so at 125 % / 150 % / … Windows would stretch a
# 100 % bitmap (blurry). Every scale gets its own pixel-exact bitmap instead;
# installer.nsi picks the one matching the monitor's DPI at startup
# (sidebar-125.bmp, header-150.bmp, …). sidebar.bmp / header.bmp = 100 %.
SCALES = [100, 125, 150, 175, 200, 250, 300]
SS = 4   # supersampling: draw at 4x the target, then Lanczos down

def render_sidebar(pct):
    k = pct / 100
    w, h = round(164 * k), round(314 * k)
    S = k * SS                       # design units → supersampled px
    W, H = w * SS, h * SS
    sb = Image.new("RGBA", (W, H), BG + (255,))
    mh = round(74 * S)
    m = mark.resize((round(mark.width * mh / mark.height), mh), Image.LANCZOS)
    sb.alpha_composite(m, ((W - m.width) // 2, round(58 * S)))
    lg_w = round(124 * S)
    lg = logo.resize((lg_w, round(logo.height * lg_w / logo.width)), Image.LANCZOS)
    # Wordmark only (cut away the mark on the left: mark ends at ~23% of logo width)
    cut = round(lg.width * 0.255)
    word = trim(lg.crop((cut, 0, lg.width, lg.height)))
    word = word.resize((round(word.width * 0.92), round(word.height * 0.92)), Image.LANCZOS)
    sb.alpha_composite(word, ((W - word.width) // 2, round(150 * S)))
    d = ImageDraw.Draw(sb)
    f_tag = font(round(9 * S))
    centered_text(d, W / 2, 186 * S, "Capture, trim and share", f_tag, DIM)
    centered_text(d, W / 2, 199 * S, "only what you need.", f_tag, DIM)
    d.line([(W / 2 - 22 * S, 226 * S), (W / 2 + 22 * S, 226 * S)], fill=ACCENT, width=max(1, round(S)))
    centered_text(d, W / 2, 272 * S, "by ZIPEEK", font(round(8 * S)), DIM)
    return sb.resize((w, h), Image.LANCZOS).convert("RGB")

def render_header(pct):
    k = pct / 100
    w, h = round(150 * k), round(57 * k)
    S = k * SS
    W, H = w * SS, h * SS
    hd = Image.new("RGBA", (W, H), BG + (255,))
    hl_w = round(118 * S)
    hl = logo.resize((hl_w, round(logo.height * hl_w / logo.width)), Image.LANCZOS)
    hd.alpha_composite(hl, ((W - hl.width) // 2, (H - hl.height) // 2))
    return hd.resize((w, h), Image.LANCZOS).convert("RGB")

for pct in SCALES:
    sfx = "" if pct == 100 else f"-{pct}"
    render_sidebar(pct).save(INST / f"sidebar{sfx}.bmp")
    render_header(pct).save(INST / f"header{sfx}.bmp")
sidebar = render_sidebar(100)
header = render_header(100)
# --- Previews (upscaled, nearest) -----------------------------------------
if OUT_PREVIEW is None:
    print("ok"); sys.exit(0)
OUT_PREVIEW.mkdir(parents=True, exist_ok=True)
sidebar.resize((328, 628), Image.NEAREST).save(OUT_PREVIEW / "sidebar_preview.png")
header.resize((300, 114), Image.NEAREST).save(OUT_PREVIEW / "header_preview.png")
prev = Image.new("RGBA", (420, 140), BG + (255,))
for i, n in enumerate((16, 32, 48, 128)):
    ic = Image.open(ICONS / "icon.ico").resize((n, n)) if False else master.resize((n, n), Image.LANCZOS)
    prev.alpha_composite(ic, (10 + i * 100, (140 - n) // 2))
prev.save(OUT_PREVIEW / "icons_preview.png")
print("ok", logo.size, mark.size)
