//! Renders the severity-colored disc used for the tray icon and toast logo.
//! Port of `internal/icon/icon.go`: generates bytes directly (32bpp DIB ICO
//! for LoadImage on Windows, PNG elsewhere) with no image assets, plus raw
//! straight-RGBA rasters for [`rune::TrayIcon`], which wants pixels, not
//! files.

// RGB is an 8-bit color channel triple (`icon.RGB`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    /// Windows COLORREF (0x00BBGGRR) layout, used by the flyout painter.
    pub fn colorref(&self) -> u32 {
        (self.r as u32) | ((self.g as u32) << 8) | ((self.b as u32) << 16)
    }
}

/// Severity colors: brand colors — blocked red, offline amber,
/// waiting-for-you green, working blue, idle gray (`icon.severityColors`).
const SEVERITY_COLORS: [(&str, Rgb); 5] = [
    ("blocked", Rgb::new(0xE5, 0x48, 0x4D)),
    ("down", Rgb::new(0xFF, 0xB0, 0x2E)),
    ("waiting", Rgb::new(0x22, 0xC5, 0x5E)),
    ("working", Rgb::new(0x3B, 0x82, 0xF6)),
    ("idle", Rgb::new(0x6B, 0x72, 0x80)),
];

/// The color for a severity, defaulting to idle gray (`icon.Color`).
pub fn color(severity: &str) -> Rgb {
    SEVERITY_COLORS
        .iter()
        .find(|(name, _)| *name == severity)
        .map(|(_, c)| *c)
        .unwrap_or(Rgb::new(0x6B, 0x72, 0x80))
}

/// Colors one pixel of a size×size icon: returns (r, g, b, a) — Go returns
/// the BGRA-premixed tuple; callers below write it in the byte order their
/// container wants (PNG: RGBA; ICO: BGRA; rune TrayIcon: RGBA).
type PixelFn = fn(Rgb, usize, usize, usize) -> (u8, u8, u8, u8);

/// Tray icon bytes for a severity: a 32bpp ICO on Windows
/// (LoadImage-compatible), a PNG elsewhere. Filled disc with a white halo
/// (`icon.Bytes`).
pub fn bytes(severity: &str, size: usize) -> Vec<u8> {
    render(circle_pixel, color(severity), size)
}

/// The severity color as a ring over a white halo on a transparent
/// background: the "off" phase of the attention blink (`icon.BytesHollow`).
pub fn bytes_hollow(severity: &str, size: usize) -> Vec<u8> {
    render(ring_pixel, color(severity), size)
}

/// Straight (not premultiplied) RGBA rows for a filled severity disc —
/// what [`rune::TrayIcon::rgba`] consumes (width × height × 4 bytes,
/// row-major, top-down).
pub fn rgba(severity: &str, size: usize) -> Vec<u8> {
    raster(circle_pixel, color(severity), size)
}

/// RGBA rows of the ring ("off") variant for [`rune::TrayIcon::rgba`].
pub fn rgba_hollow(severity: &str, size: usize) -> Vec<u8> {
    raster(ring_pixel, color(severity), size)
}

/// Encodes a pixel function as an ICO (Windows) or PNG (elsewhere)
/// (`icon.render`).
fn render(pixel: PixelFn, c: Rgb, size: usize) -> Vec<u8> {
    if cfg!(target_os = "windows") {
        ico_from(pixel, c, size)
    } else {
        png_from(pixel, c, size)
    }
}

/// Top-down straight RGBA raster of a pixel function.
fn raster(pixel: PixelFn, c: Rgb, size: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let (r, g, b, a) = pixel(c, size, x, y);
            buf.extend_from_slice(&[r, g, b, a]);
        }
    }
    buf
}

/// Wraps the pixel raster in an ICO container: ICONDIR + one ICONDIRENTRY +
/// DIB (BITMAPINFOHEADER with doubled height, bottom-up BGRA rows, zero AND
/// mask). 32bpp, alpha via the XOR mask (`icon.icoFrom`).
///
/// NOTE: byte-level port of the Go encoder; compiles on every target but
/// has not been exercised against a real Windows LoadImage in this
/// scaffold (the Go implementation it mirrors is the reference).
fn ico_from(pixel: PixelFn, c: Rgb, size: usize) -> Vec<u8> {
    let mut pix = Vec::with_capacity(size * size * 4);
    for y in (0..size).rev() {
        for x in 0..size {
            let (r, g, b, a) = pixel(c, size, x, y);
            pix.extend_from_slice(&[b, g, r, a]); // bottom-up BGRA rows
        }
    }
    let mask_row_bytes = ((size + 7) / 8).div_ceil(4) * 4;
    let mask = vec![0u8; mask_row_bytes * size]; // fully transparent AND mask

    let mut buf = Vec::with_capacity(22 + 40 + pix.len() + mask.len());
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buf.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    buf.extend_from_slice(&1u16.to_le_bytes()); // image count
    let dim: u8 = if size >= 256 { 0 } else { size.min(255) as u8 };
    buf.push(dim); // width
    buf.push(dim); // height
    buf.push(0); // palette colors
    buf.push(0); // reserved
    buf.extend_from_slice(&1u16.to_le_bytes()); // planes
    buf.extend_from_slice(&32u16.to_le_bytes()); // bpp
    // dwBytesInRes covers the DIB header + pixel data + AND mask
    buf.extend_from_slice(&((40 + pix.len() + mask.len()) as u32).to_le_bytes());
    buf.extend_from_slice(&22u32.to_le_bytes()); // offset

    let mut hdr = [0u8; 40];
    hdr[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
    hdr[4..8].copy_from_slice(&(size as u32).to_le_bytes()); // biWidth
    hdr[8..12].copy_from_slice(&((size as u32) * 2).to_le_bytes()); // biHeight (XOR+AND)
    hdr[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
    hdr[14..16].copy_from_slice(&32u16.to_le_bytes()); // biBitCount
    hdr[20..24].copy_from_slice(&(pix.len() as u32).to_le_bytes()); // biSizeImage

    buf.extend_from_slice(&hdr);
    buf.extend_from_slice(&pix);
    buf.extend_from_slice(&mask);
    buf
}

/// Renders the filled severity disc as PNG bytes — toast logo
/// (`icon.RenderPNG`).
pub fn render_png(c: Rgb, size: usize) -> Vec<u8> {
    png_from(circle_pixel, c, size)
}

/// Renders any pixel function into a PNG buffer (`icon.pngFrom`). Returns
/// an empty vector on encode failure, like the Go code returns nil; an
/// in-memory RGBA encode cannot fail in practice.
fn png_from(pixel: PixelFn, c: Rgb, size: usize) -> Vec<u8> {
    let mut img = Vec::with_capacity(size * size * 4);
    for y in 0..size {
        for x in 0..size {
            let (r, g, b, a) = pixel(c, size, x, y);
            img.extend_from_slice(&[r, g, b, a]);
        }
    }
    let encode = |out: &mut Vec<u8>| -> anyhow::Result<()> {
        let mut enc = png::Encoder::new(out, size as u32, size as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(&img)?;
        Ok(())
    };
    let mut out = Vec::new();
    if encode(&mut out).is_err() {
        return Vec::new();
    }
    out
}

/// Composes a colored disc over a slightly larger white halo, both
/// antialiased, over transparency; visible on light and dark trays
/// (`icon.circlePixel`).
fn circle_pixel(c: Rgb, size: usize, x: usize, y: usize) -> (u8, u8, u8, u8) {
    let d = dist(size, x, y);
    let disc_r = center(size) - 3.0;
    let disc_a = coverage(d, disc_r);
    let halo_a = coverage(d, disc_r + 1.5);
    if halo_a <= 0.0 {
        return (0, 0, 0, 0);
    }
    blend_over_white(c, disc_a / halo_a, halo_a)
}

/// Composes a severity-colored ring (3px band) over the same white halo;
/// reads as an "off" phase next to the filled disc (`icon.ringPixel`).
fn ring_pixel(c: Rgb, size: usize, x: usize, y: usize) -> (u8, u8, u8, u8) {
    let d = dist(size, x, y);
    let r = center(size) - 3.0;
    let ring_a = coverage(d, r) - coverage(d, r - 3.0);
    let halo_a = coverage(d, r + 1.5) - coverage(d, r); // halo hugs the ring outside
    if halo_a <= 0.0 && ring_a <= 0.0 {
        return (0, 0, 0, 0);
    }
    let alpha = halo_a.max(ring_a);
    blend_over_white(c, ring_a / alpha, alpha)
}

/// Composes color at mix over a white base with alpha (`icon.blendOverWhite`).
fn blend_over_white(c: Rgb, mix: f64, alpha: f64) -> (u8, u8, u8, u8) {
    let out = |ch: u8| -> u8 {
        let v = 255.0 * (1.0 - mix) + f64::from(ch) * mix;
        let v = v.clamp(0.0, 255.0);
        (v + 0.5) as u8
    };
    (out(c.r), out(c.g), out(c.b), (alpha * 255.0 + 0.5) as u8)
}

fn center(size: usize) -> f64 {
    size as f64 / 2.0
}

fn dist(size: usize, x: usize, y: usize) -> f64 {
    let dx = x as f64 + 0.5 - center(size);
    let dy = y as f64 + 0.5 - center(size);
    dx.hypot(dy)
}

/// Approximates the fraction of the pixel covered by a disc of radius r at
/// distance d (1px linear ramp) (`icon.coverage`).
fn coverage(d: f64, r: f64) -> f64 {
    (r - d + 0.5).clamp(0.0, 1.0)
}
