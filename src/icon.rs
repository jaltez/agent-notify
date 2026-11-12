//! Renders the agent-tile icon used for the tray icon and toast logo.
//! Port of `internal/icon/icon.go`, redesigned for v0.6: a dark rounded
//! tile with a gray border and the terminal prompt `>_` stroked in the
//! severity color — generated as pixels (32bpp DIB ICO for LoadImage on
//! Windows, PNG elsewhere) with no image assets, plus raw straight-RGBA
//! rasters for [`rune::TrayIcon`], which wants pixels, not files.

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
/// waiting-for-you green, working yellow, idle gray (`icon.severityColors`).
const SEVERITY_COLORS: [(&str, Rgb); 5] = [
    ("blocked", Rgb::new(0xE5, 0x48, 0x4D)),
    ("down", Rgb::new(0xFF, 0xB0, 0x2E)),
    ("waiting", Rgb::new(0x22, 0xC5, 0x5E)),
    ("working", Rgb::new(0xFF, 0xD6, 0x0A)),
    ("idle", Rgb::new(0x6B, 0x72, 0x80)),
];

/// The agent-tile icon's own colors (not severity state): the tile's dark
/// fill (the panel background), the tile's gray border — legible on light
/// and dark taskbars — and the idle-gray prompt of the dimmed blink phase.
const TILE: Rgb = Rgb::new(0x1C, 0x1E, 0x22);
const BORDER: Rgb = Rgb::new(0x9C, 0xA3, 0xAF);
const IDLE: Rgb = Rgb::new(0x6B, 0x72, 0x80);

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
/// (LoadImage-compatible), a PNG elsewhere. The agent tile, prompt in the
/// severity color (`icon.Bytes`).
pub fn bytes(severity: &str, size: usize) -> Vec<u8> {
    render(agent_tile_bright, color(severity), size)
}

/// The dim tile for the blink's "off" phase: same shape, prompt in idle
/// gray (`icon.BytesHollow`).
pub fn bytes_hollow(severity: &str, size: usize) -> Vec<u8> {
    render(agent_tile_dim, color(severity), size)
}

/// Straight (not premultiplied) RGBA rows for a filled severity disc —
/// what [`rune::TrayIcon::rgba`] consumes (width × height × 4 bytes,
/// row-major, top-down).
pub fn rgba(severity: &str, size: usize) -> Vec<u8> {
    raster(agent_tile_bright, color(severity), size)
}

/// RGBA rows of the ring ("off") variant for [`rune::TrayIcon::rgba`].
pub fn rgba_hollow(severity: &str, size: usize) -> Vec<u8> {
    raster(agent_tile_dim, color(severity), size)
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

/// Renders the agent tile as PNG bytes — toast logo, and the app
/// icon the flyout hands to `rune::set_app_icon` (`icon.RenderPNG`).
pub fn render_png(c: Rgb, size: usize) -> Vec<u8> {
    png_from(agent_tile_bright, c, size)
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

/// Composes `top` over `base` with fractional alpha; straight RGBA out
/// with `alpha_out` as the pixel's own coverage.
fn over(base: Rgb, top: Rgb, a: f64, alpha_out: f64) -> (u8, u8, u8, u8) {
    let mix = |b: u8, t: u8| -> u8 {
        (f64::from(b) * (1.0 - a) + f64::from(t) * a + 0.5) as u8
    };
    (
        mix(base.r, top.r),
        mix(base.g, top.g),
        mix(base.b, top.b),
        (alpha_out * 255.0 + 0.5) as u8,
    )
}

/// One pixel of the agent tile (the v0.6 icon, not a bare circle): a dark
/// rounded tile with a gray border — legible on light and dark trays —
/// and the terminal prompt `>_` stroked in `prompt` (the severity; the
/// blink's dim phase passes idle gray). The `>_` is the terminal-agent
/// mark; herdr's own tab labels use the same glyph.
fn agent_tile(prompt: Rgb, size: usize, x: usize, y: usize) -> (u8, u8, u8, u8) {
    let half = size as f64 / 2.0;
    let s = size as f64;
    let px = x as f64 + 0.5 - half;
    let py = y as f64 + 0.5 - half;

    // rounded tile, inset from the icon's edge
    let inset = (s * 0.05).max(0.75);
    let hw = half - inset;
    let radius = (s * 0.16).clamp(1.5, 24.0);
    let qx = px.abs() - hw + radius;
    let qy = py.abs() - hw + radius;
    let d_box = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;

    let stroke = (s * 0.045).max(1.0); // border thickness
    let a_border = (0.5 - (d_box.abs() - stroke / 2.0)).clamp(0.0, 1.0);
    if d_box > stroke / 2.0 + 0.5 {
        return (0, 0, 0, 0); // outside the tile
    }

    // the `>_` prompt, laid out in fractions of the usable half-extent
    let w = hw - stroke;
    let seg = |ax: f64, ay: f64, bx: f64, by: f64| -> f64 {
        let (ax, ay, bx, by) = (ax * w, ay * w, bx * w, by * w);
        let (vx, vy) = (bx - ax, by - ay);
        let t = (((px - ax) * vx + (py - ay) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
        ((px - ax - t * vx)).hypot(py - ay - t * vy)
    };
    let pw = (s * 0.075).max(1.2); // prompt stroke width
    let d_glyph = seg(-0.42, -0.38, -0.10, 0.0) // `>` upper stroke
        .min(seg(-0.10, 0.0, -0.42, 0.38)) // `>` lower stroke
        .min(seg(-0.02, 0.38, 0.42, 0.38)); // `_`
    let a_glyph = (0.5 - (d_glyph - pw / 2.0)).clamp(0.0, 1.0);

    if a_border > 0.0 {
        return over(TILE, BORDER, a_border, 1.0);
    }
    over(TILE, prompt, a_glyph, 1.0)
}

/// The bright tile: prompt in the severity color.
fn agent_tile_bright(c: Rgb, size: usize, x: usize, y: usize) -> (u8, u8, u8, u8) {
    agent_tile(c, size, x, y)
}

/// The dim tile (blink "off" phase): prompt in idle gray, same shape.
fn agent_tile_dim(_c: Rgb, size: usize, x: usize, y: usize) -> (u8, u8, u8, u8) {
    agent_tile(IDLE, size, x, y)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_transparent() {
        let sev = color("working");
        for size in [16, 32, 256] {
            for (x, y) in [(0, 0), (size - 1, 0), (0, size - 1), (size - 1, size - 1)] {
                let (_, _, _, a) = agent_tile_bright(sev, size, x, y);
                assert_eq!(a, 0, "corner ({x},{y}) at {size}px must be transparent");
            }
        }
    }

    #[test]
    fn prompt_paints_in_the_severity_color() {
        let sev = color("working"); // #FFD60A
        let size = 32;
        let mut painted = 0;
        for y in 0..size {
            for x in 0..size {
                let (r, g, b, a) = agent_tile_bright(sev, size, x, y);
                if a == 255 && (r, g, b) == (sev.r, sev.g, sev.b) {
                    painted += 1;
                }
            }
        }
        assert!(
            painted > 20,
            "the >_ prompt must paint in the severity color ({painted} px)"
        );
    }

    #[test]
    fn dim_phase_prompt_is_idle_gray() {
        let size = 32;
        let mut gray = 0;
        for y in 0..size {
            for x in 0..size {
                let (r, g, b, a) = agent_tile_dim(color("working"), size, x, y);
                if a == 255 && (r, g, b) == (IDLE.r, IDLE.g, IDLE.b) {
                    gray += 1;
                }
            }
        }
        assert!(gray > 20, "dim prompt paints idle gray ({gray} px)");
    }

    /// Eyeball helper: writes the tile at several sizes to /tmp
    /// (`cargo test -- --ignored icon_smoke`).
    #[test]
    #[ignore = "writes PNGs for eyeballing: cargo test -- --ignored icon_smoke"]
    fn smoke_writes_icon_pngs() {
        for size in [16, 24, 32, 128, 256] {
            let png = render_png(color("working"), size);
            std::fs::write(format!("/tmp/agent-icon-{size}.png"), png).unwrap();
        }
        let png = render_png(IDLE, 64);
        std::fs::write("/tmp/agent-icon-dim-64.png", png).unwrap();
    }
}
