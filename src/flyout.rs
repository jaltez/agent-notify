//! The flyout panel: the platform-neutral row model built from a View
//! (`tray/flyout_rows.go`) plus the panel window itself (`tray/flyout_windows.go`
//! Win32/GDI, re-created here as a cross-platform rune window — a documented
//! improvement over the Go build, where the panel was Windows-only).
//!
//! RUNE MAPPING (verified against the vendored rune source at rev 003a5c7):
//! the panel is a borderless, transparent, always-on-top rune window opened
//! by a windowless [`rune::App`] (`AppHandle::open` on tray primary click —
//! the `examples/launcher` pattern), driven by `AppSpec::poll` for the 1 s
//! refresh cadence and the 30 s auto-close, `.scroll(ScrollSpec)` for wheel
//! scrolling with a drawn thumb, `El::hover` for the row highlight, and
//! `Tx::close_window` for Escape / click-outside / auto-close dismissal.
//!
//! Layout, row heights, colors and interaction ports `flyout_windows.go`
//! faithfully; only the GDI painting idioms change (rune lays the same rows
//! out with flex boxes instead of absolute pixel rectangles).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rune::el::ScrollSpec;
use rune::tok::TextStyle;
use rune::{
    col, el, row, spacer, text, App, AppHandle, AppSpec, Color, Cx, Display, El, Key, Tx,
};
use rune::{KeyEvent, NativeDisplayId, WindowOptions};

use crate::engine::{self, View};
use crate::icon;

// ---------------------------------------------------------------------------
// Row model (flyout_rows.go) — platform-neutral, unit tested
// ---------------------------------------------------------------------------

/// Panel geometry shared by the row model and the painter
/// (`flyout_rows.go` consts). The state bar's segment widths are computed
/// against this base width and scaled to the real one at paint time.
pub const FLY_WIDTH: i32 = 420;
pub const FLY_PAD: i32 = 14;

/// Classifies a flyout row; the painter picks layout, height, and
/// interaction from it. The model itself is platform-neutral so it can be
/// unit-tested anywhere (`tray.rowKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// fleet title line with severity dot.
    Summary,
    /// counts line with the fleet state bar.
    SummarySub,
    /// session header: chevron + counts, click toggles.
    Session,
    /// two-line attention block (dot + status wash).
    Agent,
    /// one collapsed-group agent on a single line.
    AgentCompact,
    /// "N finished"/"N resting" line, click toggles the group.
    Group,
    /// placeholder when nothing is known.
    Empty,
}

/// One segment of the fleet state bar (pixel width precomputed,
/// `tray.barSeg`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarSeg {
    pub col: u32,
    pub px: i32,
}

/// One painted line (or block) of the flyout panel (`tray.flyRow`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FlyRow {
    pub kind: Option<RowKind>,
    /// COLORREF; 0 = no dot (build with [`icon::Rgb::colorref`]).
    pub dot: u32,
    /// line 1 (agent blocks: the title).
    pub text: String,
    /// session rows: the host label; agent rows: host to open on.
    pub host: String,
    /// agent rows: session name the agent belongs to.
    pub sess: String,
    /// agent rows: herdr pane id, when the agent has one.
    pub pane: String,
    /// line 2, agent blocks only (runner · project).
    pub sub: String,
    /// paint in the dim color.
    pub dim: bool,
    /// group key this row toggles ("" = inert).
    pub toggle: String,
    /// effective open state, toggle rows only.
    pub open: bool,
    /// state bar segments, rowSummarySub only.
    pub bar: Vec<BarSeg>,
}

/// Renders an icon color in Windows COLORREF (BGR) layout
/// (`tray.colorRef`).
pub fn colorref(c: icon::Rgb) -> u32 {
    (c.r as u32) | ((c.g as u32) << 8) | ((c.b as u32) << 16)
}

fn plural_s(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn or_dash(s: &str) -> String {
    if s.is_empty() {
        "—".to_string()
    } else {
        s.to_string()
    }
}

/// Strips glyphs the Go flyout's Segoe UI text drawing renders as tofu
/// boxes: emoji, pictographs, nerd-font Private Use Area icons, variation
/// selectors, and zero-width joiners that herdr titles carry. Geometric
/// shapes (chevrons) and normal text pass through (`tray.gdiSafe`,
/// flyout_rows.go:77).
///
/// PORT-NOTE: rune shapes text with Parley and renders emoji/pictographs
/// fine, so the stripping is not *needed* here; it is kept byte-for-byte so
/// the row model (and its tests) stays identical to the Go spec. The
/// whitespace collapse (`strings.Fields` join) is preserved.
pub fn gdi_safe(s: &str) -> String {
    let clean: String = s
        .chars()
        .filter(|&r| {
            !(matches!(r, '\u{1F000}'..='\u{1FAFF}')
                || matches!(r, '\u{2600}'..='\u{27BF}')
                || matches!(r, '\u{2B00}'..='\u{2BFF}')
                || matches!(r, '\u{FE00}'..='\u{FE0F}')
                || matches!(r, '\u{200B}'..='\u{200F}')
                || matches!(r, '\u{E000}'..='\u{F8FF}')
                || matches!(r, '\u{F0000}'..='\u{10FFFD}')
                || r == '\u{200D}'
                || r == '\u{20E3}')
        })
        .collect();
    clean.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The expand/collapse marker for toggle rows (`tray.chevron`).
fn chevron(open: bool) -> &'static str {
    if open {
        "▾"
    } else {
        "▸"
    }
}

/// Summarizes a session's agents for its header, e.g.
/// " · 1 working · 1 blocked"; danger first (`tray.tally`).
fn tally(agents: &[engine::AgentView]) -> String {
    let (mut blocked, mut working, mut waiting) = (0, 0, 0);
    for a in agents {
        match engine::row_severity(&a.status, a.wants) {
            "blocked" => blocked += 1,
            "working" => working += 1,
            "waiting" => waiting += 1,
            _ => {}
        }
    }
    let mut out = String::new();
    for (n, w) in [(blocked, "blocked"), (working, "working"), (waiting, "waiting")] {
        if n > 0 {
            out += &format!(" · {n} {w}");
        }
    }
    out
}

/// Splits the panel's inner width across agent states: blocked, offline,
/// working, waiting, then resting gray (`tray.stateBar`).
fn state_bar(v: &View) -> Vec<BarSeg> {
    const INNER: i32 = FLY_WIDTH - 2 * FLY_PAD;
    let (mut blocked, mut down, mut working, mut waiting, mut total) = (0, 0, 0, 0, 0usize);
    for s in &v.sessions {
        if !s.up {
            down += 1;
            continue;
        }
        for a in &s.agents {
            total += 1;
            match engine::row_severity(&a.status, a.wants) {
                "blocked" => blocked += 1,
                "working" => working += 1,
                "waiting" => waiting += 1,
                _ => {}
            }
        }
    }
    if total == 0 {
        return Vec::new();
    }
    let idle = total - blocked - working - waiting;
    let parts = [
        (blocked, colorref(icon::color("blocked"))),
        (down, colorref(icon::color("down"))),
        (working, colorref(icon::color("working"))),
        (waiting, colorref(icon::color("waiting"))),
        (idle, colorref(icon::color("idle"))),
    ];
    let mut segs: Vec<BarSeg> = Vec::with_capacity(parts.len());
    let mut used: i32 = 0;
    for (n, col) in parts {
        if n == 0 {
            continue;
        }
        let mut px = (INNER as f64 * n as f64 / total as f64) as i32;
        if px < 1 {
            px = 1;
        }
        segs.push(BarSeg { col, px });
        used += px;
    }
    if let Some(last) = segs.last_mut() {
        last.px += INNER - used; // last segment absorbs rounding
    }
    segs
}

/// Builds the flyout row model from a View and the user's collapse
/// overrides (`tray.buildRows` in flyout_rows.go). Session headers are
/// always present; a session expands by default only when it has working or
/// blocked agents. Finished (green) and resting agents each collapse into a
/// single per-session line; opened groups list their agents as compact
/// single-line rows. `open_state` and `rest_open` hold the user's click
/// overrides (session key, and group key "session/finished" or
/// "session/resting").
pub fn build_rows(
    v: &View,
    open_state: &HashMap<String, bool>,
    rest_open: &HashMap<String, bool>,
) -> Vec<FlyRow> {
    let mut rows = vec![
        FlyRow {
            kind: Some(RowKind::Summary),
            dot: colorref(icon::color(v.severity())),
            text: "Agent Notify".to_string(),
            ..FlyRow::default()
        },
        FlyRow {
            kind: Some(RowKind::SummarySub),
            text: v.summary(),
            dim: true,
            bar: state_bar(v),
            ..FlyRow::default()
        },
    ];
    if v.sessions.is_empty() {
        rows.push(FlyRow {
            kind: Some(RowKind::Empty),
            text: "No herdr sessions seen yet; start a herdr session".to_string(),
            dim: true,
            ..FlyRow::default()
        });
        return rows;
    }
    for s in &v.sessions {
        let key = format!("{}/{}", s.host, s.name);
        let (mut active, mut finished, mut resting) = (Vec::new(), Vec::new(), Vec::new());
        for a in &s.agents {
            match engine::row_severity(&a.status, a.wants) {
                "working" | "blocked" => active.push(a),
                "waiting" => finished.push(a),
                _ => resting.push(a),
            }
        }
        let mut open = !active.is_empty();
        if let Some(&o) = open_state.get(&key) {
            open = o;
        }
        let state = if s.up { "" } else { " · offline" };
        rows.push(FlyRow {
            kind: Some(RowKind::Session),
            text: format!("{} {}", chevron(open), s.name),
            host: s.host.clone(),
            sub: format!(
                "{} agent{}{}{}",
                s.agents.len(),
                plural_s(s.agents.len()),
                tally(&s.agents),
                state
            ),
            dim: active.is_empty(),
            toggle: key.clone(),
            open,
            ..FlyRow::default()
        });
        if !open {
            continue;
        }
        for a in &active {
            let (mut title, mut info) = (a.title.clone(), a.name.clone());
            if !a.project.is_empty() {
                if title.is_empty() {
                    title = a.project.clone();
                } else {
                    info += &format!(" · {}", a.project);
                }
            }
            rows.push(FlyRow {
                kind: Some(RowKind::Agent),
                dot: colorref(icon::color(engine::row_severity(&a.status, a.wants))),
                text: or_dash(&gdi_safe(&title)), // line 1: what it is doing
                sub: gdi_safe(&info),             // line 2: runner · project, dim
                host: s.host.clone(),
                sess: s.name.clone(),
                pane: a.pane_id.clone(),
                ..FlyRow::default()
            });
        }
        append_group(
            &mut rows,
            &format!("{key}/finished"),
            "finished",
            &finished,
            "waiting",
            rest_open,
            &s.host,
            &s.name,
        );
        append_group(
            &mut rows,
            &format!("{key}/resting"),
            "resting",
            &resting,
            "idle",
            rest_open,
            &s.host,
            &s.name,
        );
    }
    rows
}

/// Appends one collapsible group line and, when the group is open, one
/// compact row per agent (dot in the group's severity color)
/// (`tray.appendGroup`).
#[allow(clippy::too_many_arguments)] // mirrors the Go signature 1:1
fn append_group(
    rows: &mut Vec<FlyRow>,
    key: &str,
    label: &str,
    agents: &[&engine::AgentView],
    sev: &str,
    open: &HashMap<String, bool>,
    host: &str,
    sess: &str,
) {
    if agents.is_empty() {
        return;
    }
    let is_open = open.get(key).copied().unwrap_or(false);
    rows.push(FlyRow {
        kind: Some(RowKind::Group),
        dot: colorref(icon::color(sev)),
        text: format!("{} {} {}", chevron(is_open), agents.len(), label),
        dim: false, // normal weight: collapsibles must outweigh their rows
        toggle: key.to_string(),
        open: is_open,
        ..FlyRow::default()
    });
    if !is_open {
        return;
    }
    for a in agents {
        let title = if a.title.is_empty() {
            a.project.clone()
        } else {
            a.title.clone()
        };
        rows.push(FlyRow {
            kind: Some(RowKind::AgentCompact),
            dot: colorref(icon::color(sev)),
            text: or_dash(&gdi_safe(&title)),
            dim: true,
            host: host.to_string(),
            sess: sess.to_string(),
            pane: a.pane_id.clone(),
            ..FlyRow::default()
        });
    }
}

// ---------------------------------------------------------------------------
// Geometry (flyout_windows.go consts + layout)
// ---------------------------------------------------------------------------

/// Auto-close delay (`autoCloseDelay`): the panel hides itself after this
/// long unless the pointer is over it (then it re-arms) or it is scrollable
/// (then it is never armed).
pub const AUTO_CLOSE_DELAY: Duration = Duration::from_secs(30);
/// View refresh cadence (`refreshEvery`).
pub const REFRESH_EVERY: Duration = Duration::from_secs(1);
// PORT-NOTE: Go's activateGrace (600ms) is not a separate timer here. In the
// Go wndproc, the grace branch only resets `activated` (no hide decision
// depends on it): a focus loss hides iff the panel held foreground, which
// flyout_focus below reproduces directly.

const SECTION_H: f32 = 28.0; // session header
const ROW_H: f32 = 22.0; // summary / single-line rows
const ROW_TITLE_H: f32 = 26.0; // "Agent Notify" line
const ROW_SUB_H: f32 = 32.0; // counts line + state bar
const SEP_AFTER_SUM: f32 = 10.0;
const TITLE_H: f32 = 20.0; // agent block: line 1 (title)
const INFO_H: f32 = 15.0; // agent block: line 2 (agent · project)
const BLOCK_GAP: f32 = 8.0;
const AGENT_BLOCK_H: f32 = TITLE_H + INFO_H + BLOCK_GAP;
const COL_GAP: f32 = 24.0;
const COL_MIN_W: f32 = 170.0;
const COL_MAX_W: f32 = 300.0;
const GRID_ROW_GAP: f32 = 14.0;

/// The painted height of one row (`rowHeight`).
pub fn row_height(kind: Option<RowKind>) -> f32 {
    match kind {
        Some(RowKind::Summary) => ROW_TITLE_H,
        Some(RowKind::SummarySub) => ROW_SUB_H,
        Some(RowKind::Session) => SECTION_H,
        Some(RowKind::Agent) => AGENT_BLOCK_H,
        _ => ROW_H,
    }
}

/// The grid the panel lays its session groups into (`flyout.layout`
/// result): the two summary rows span the full width; session groups fill a
/// columns × rows grid, picking the column count whose rectangle best
/// matches a widescreen ratio (1.5) without overflowing the work area.
pub struct Layout {
    /// Outer panel width in points.
    pub width: f32,
    /// Full content height in points (pre-clamp).
    pub content_h: f32,
    /// Window height in points: as tall as the content, up to
    /// `area_h - 16`; beyond that the panel scrolls.
    pub window_h: f32,
    pub scrollable: bool,
    pub col_w: f32,
    /// Number of leading full-width rows (the summary pair).
    pub header: usize,
    /// Grid rows of session groups; each group lists row indices into the
    /// row model, in paint order.
    pub groups: Vec<Vec<Vec<usize>>>,
}

/// Ports `flyout.layout` (flyout_windows.go:702): arranges the panel as a
/// scalable grid, scoring candidate column counts against the 1.5 aspect
/// ratio. `area_w`/`area_h` are the work area in points.
pub fn layout(rows: &[FlyRow], area_w: f32, area_h: f32) -> Layout {
    let avail = area_w - 2.0 * FLY_PAD as f32;

    // full-width header rows on top
    let mut h = FLY_PAD as f32;
    let mut header = 0;
    while header < rows.len()
        && matches!(rows[header].kind, Some(RowKind::Summary) | Some(RowKind::SummarySub))
    {
        h += row_height(rows[header].kind);
        if rows[header].kind == Some(RowKind::SummarySub) {
            h += SEP_AFTER_SUM;
        }
        header += 1;
    }
    let top_h = h - FLY_PAD as f32;

    // one group per session: everything from a session header up to the
    // next one
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate().skip(header) {
        if r.kind == Some(RowKind::Session) && !cur.is_empty() {
            groups.push(std::mem::take(&mut cur));
        }
        cur.push(i);
    }
    if !cur.is_empty() {
        groups.push(cur);
    }

    let group_h = |g: &[usize]| -> f32 { g.iter().map(|&i| row_height(rows[i].kind)).sum() };

    // candidate column counts: one session wide up to the widest the work
    // area allows at readable column width
    let mut max_cols = 1usize;
    while max_cols < groups.len()
        && ((max_cols as f32 + 1.0) * (COL_MIN_W + COL_GAP) - COL_GAP) <= avail
    {
        max_cols += 1;
    }
    let (mut best_cols, mut best_w, mut best_score) = (1usize, 0.0f32, f32::MAX);
    for c in 1..=max_cols {
        let mut col_w = (avail - (c as f32 - 1.0) * COL_GAP) / c as f32;
        if col_w > COL_MAX_W {
            col_w = COL_MAX_W;
        }
        if col_w < 140.0 {
            continue; // unreadably narrow
        }
        let w = 2.0 * FLY_PAD as f32 + c as f32 * col_w + (c as f32 - 1.0) * COL_GAP;
        let mut hgt = top_h;
        let mut r = 0usize;
        while r * c < groups.len() {
            let end = ((r + 1) * c).min(groups.len());
            let mut row_h = 0.0f32;
            for g in &groups[r * c..end] {
                let gh = group_h(g);
                if gh > row_h {
                    row_h = gh;
                }
            }
            hgt += row_h + GRID_ROW_GAP;
            r += 1;
        }
        hgt += FLY_PAD as f32 - GRID_ROW_GAP;
        if w > area_w || hgt > area_h * 9.0 / 10.0 {
            continue; // overflows: unusable
        }
        let score = (w / hgt - 1.5).abs();
        if score < best_score {
            best_score = score;
            best_cols = c;
            best_w = w;
        }
    }
    if best_w == 0.0 {
        // nothing fits: widest grid, let the panel scroll
        best_cols = max_cols;
    }
    let mut col_w = (avail - (best_cols as f32 - 1.0) * COL_GAP) / best_cols as f32;
    if col_w > COL_MAX_W {
        col_w = COL_MAX_W;
    }
    let width = 2.0 * FLY_PAD as f32 + best_cols as f32 * col_w + (best_cols as f32 - 1.0) * COL_GAP;

    // place the groups: grid rows top-aligned, tallest group per row
    let mut grid: Vec<Vec<Vec<usize>>> = Vec::new();
    let mut row_y = FLY_PAD as f32 + top_h;
    let mut r = 0usize;
    while r * best_cols < groups.len() {
        let end = ((r + 1) * best_cols).min(groups.len());
        let row_groups = &groups[r * best_cols..end];
        let mut row_h = 0.0f32;
        let mut placed = Vec::with_capacity(row_groups.len());
        for g in row_groups {
            placed.push(g.clone());
            let gh = group_h(g);
            if gh > row_h {
                row_h = gh;
            }
        }
        grid.push(placed);
        row_y += row_h + GRID_ROW_GAP;
        r += 1;
    }
    let content_h = row_y - GRID_ROW_GAP + FLY_PAD as f32;
    let max_h = (area_h - 16.0).max(1.0);
    let scrollable = content_h > max_h;
    Layout {
        width,
        content_h,
        window_h: if scrollable { max_h } else { content_h },
        scrollable,
        col_w,
        header,
        groups: grid,
    }
}

// ---------------------------------------------------------------------------
// Palette (flyout_windows.go COLORREF values, 0x00BBGGRR)
// ---------------------------------------------------------------------------

/// Session host label (teal), panel background, border, session header
/// band, text, dim text and neutral hover, straight from the Go painter.
/// The literals are `srgb_decode(rgb/255)` of each COLORREF byte, kept as
/// consts because rune's [`Color`] is linear-light; a test pins them
/// against `rune::color::srgb_decode`.
#[allow(clippy::excessive_precision)] // derived constants, not hand-written
mod palette {
    pub use rune::Color;
    /// 0x00C8C87F — rgb(127, 200, 200).
    pub const HOST: Color = Color { r: 0.212230757, g: 0.577580452, b: 0.577580452, a: 1.0 };
    /// 0x00202020 — rgb(32, 32, 32).
    pub const BG: Color = Color { r: 0.014443845, g: 0.014443845, b: 0.014443845, a: 1.0 };
    /// 0x00453C3C — rgb(60, 60, 69).
    pub const BORDER: Color = Color { r: 0.045186203, g: 0.045186203, b: 0.059511244, a: 1.0 };
    /// 0x002B2B2B — rgb(43, 43, 43).
    pub const BAND: Color = Color { r: 0.024157634, g: 0.024157634, b: 0.024157634, a: 1.0 };
    /// 0x00F2F2F2 — rgb(242, 242, 242).
    pub const TEXT: Color = Color { r: 0.887923121, g: 0.887923121, b: 0.887923121, a: 1.0 };
    /// 0x00A0A0A0 — rgb(160, 160, 160).
    pub const DIM: Color = Color { r: 0.351532638, g: 0.351532638, b: 0.351532638, a: 1.0 };
    /// 0x00303030 — rgb(48, 48, 48).
    pub const HOVER: Color = Color { r: 0.029556835, g: 0.029556835, b: 0.029556835, a: 1.0 };
}

/// A COLORREF (0x00BBGGRR) as an opaque rune [`Color`], sRGB-decoded into
/// rune's linear-light working space so the Go painter's colors land
/// byte-exact on screen.
fn col_of(c: u32) -> Color {
    Color {
        r: rune::color::srgb_decode((c & 0xFF) as f32 / 255.0),
        g: rune::color::srgb_decode(((c >> 8) & 0xFF) as f32 / 255.0),
        b: rune::color::srgb_decode(((c >> 16) & 0xFF) as f32 / 255.0),
        a: 1.0,
    }
}

/// Mixes a COLORREF over the panel background at ratio `t`; `hover` lifts
/// the result toward white so the hovered row reads as selected
/// (`blendColor`, flyout_windows.go:1144 — sRGB math kept exactly).
fn blend_color_ref(dot: u32, t: f64, hover: bool) -> u32 {
    const BG: f64 = 0x20 as f64; // panel background, each channel
    let mix = |a: f64, b: f64| -> u8 { (a * (1.0 - t) + b * t + 0.5) as u8 };
    let mut r = mix(BG, (dot & 0xFF) as f64);
    let mut g = mix(BG, ((dot >> 8) & 0xFF) as f64);
    let mut b = mix(BG, ((dot >> 16) & 0xFF) as f64);
    if hover {
        let lift = |v: u8| -> u8 { (v as f64 * (1.0 - 0.10) + 240.0 * 0.10 + 0.5) as u8 };
        r = lift(r);
        g = lift(g);
        b = lift(b);
    }
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// The whole-block status wash for an agent row: the dot color blended over
/// the panel background at 10% (Go `blendColor(r.dot, 0.10, hovered)`).
fn wash(dot: u32) -> Color {
    col_of(blend_color_ref(dot, 0.10, false))
}

/// Hover overlay drawn over an agent wash: white-ish at 10% alpha, which
/// composites to the Go lift (`v*0.9 + 24`). PORT-NOTE: the lift math is
/// exact in sRGB; rune blends overlays in linear light, so the hovered
/// wash lands close to, not byte-exact on, the Go's.
#[allow(clippy::excessive_precision)] // derived constant
const WASH_HOVER: Color = Color {
    r: 0.871367097, // srgb_decode(240/255)
    g: 0.871367097,
    b: 0.871367097,
    a: 0.10,
};

// Type ramp: the Go painter's GDI fonts (Segoe UI -13/400, -13/600, -11/400,
// -15/600) at the row heights they were centered in.
const FONT_BODY: TextStyle = TextStyle::new(13.0, 400.0);
const FONT_BOLD: TextStyle = TextStyle::new(13.0, 600.0);
const FONT_SMALL: TextStyle = TextStyle::new(11.0, 400.0);
const FONT_HEAD: TextStyle = TextStyle::new(15.0, 600.0);

// ---------------------------------------------------------------------------
// Work area (flyout show() bottom-right anchoring)
// ---------------------------------------------------------------------------

/// The usable desktop rectangle the panel anchors to (`flyout.areaW/...`
/// from SPI_GETWORKAREA). Points are rune's logical units; the `_px` fields
/// are physical desktop pixels for [`AppHandle::set_position`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WorkArea {
    pub x_px: i32,
    pub y_px: i32,
    pub right_px: i32,
    pub bottom_px: i32,
    pub w_pts: f32,
    pub h_pts: f32,
    /// Physical pixels per point (1 where positions are already logical —
    /// Wayland — or unknown).
    pub scale: f32,
}

impl WorkArea {
    /// Bottom-right anchor in physical pixels, the Go margins (12 right, 8
    /// bottom) scaled to points (`flyout.show`: x = right - w - 12,
    /// y = bottom - h - 8).
    pub fn bottom_right_px(&self, w_pts: f32, h_pts: f32) -> (i32, i32) {
        let s = if self.scale > 0.01 { self.scale } else { 1.0 };
        let x = self.right_px - (w_pts * s + 12.0 * s).round() as i32;
        let y = self.bottom_px - (h_pts * s + 8.0 * s).round() as i32;
        (x, y)
    }
}

/// Detects the work area from rune's displays. On Windows the query goes
/// through `SPI_GETWORKAREA` (taskbar-excluded, like the Go panel); on
/// Linux it is best-effort: the primary display's bounds, minus no taskbar
/// (panel may sit under a dock/bar; documented deviation).
pub fn work_area(displays: &[Display]) -> WorkArea {
    let d = displays.iter().find(|d| d.primary).or_else(|| displays.first());
    let (x, y, w, h, scale, wayland) = match d {
        Some(d) => (
            d.position.0,
            d.position.1,
            d.size.0 as i32,
            d.size.1 as i32,
            d.scale as f32,
            matches!(d.native_id(), Some(NativeDisplayId::Wayland(_))),
        ),
        None => (0, 0, 1920, 1080, 1.0, false),
    };
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let (mut x, mut y, mut w, mut h) = (x, y, w, h);
    #[cfg(target_os = "windows")]
    {
        // The Go panel queries the work area at every show; we detect once
        // per run (displays are fixed) and re-anchor on every layout.
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::{
            SystemParametersInfoW, SPI_GETWORKAREA,
        };
        let mut area = RECT::default();
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut area as *mut RECT as *mut core::ffi::c_void),
                windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        if ok.is_ok() {
            x = area.left;
            y = area.top;
            w = area.right - area.left;
            h = area.bottom - area.top;
        }
    }
    // Wayland reports compositor-logical bounds; X11/Windows report physical
    // pixels, which divide by the scale into points.
    let s = if wayland {
        1.0
    } else if scale > 0.01 {
        scale
    } else {
        1.0
    };
    let (w_pts, h_pts) = if wayland {
        (w as f32, h as f32)
    } else {
        (w as f32 / s, h as f32 / s)
    };
    WorkArea {
        x_px: x,
        y_px: y,
        right_px: x + w,
        bottom_px: y + h,
        w_pts,
        h_pts,
        scale: s,
    }
}

// ---------------------------------------------------------------------------
// The rune panel window (examples/launcher pattern)
// ---------------------------------------------------------------------------

/// The agent a click focuses: what [`AgentHit`] carries to the CLI's
/// focus callback (`flyRow` host/sess/pane, `flyout.focusAgent`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentHit {
    pub host: String,
    pub sess: String,
    pub pane: String,
}

type FocusAgentFn = Arc<dyn Fn(&AgentHit) + Send + Sync>;

/// Thread-safe control handle for the panel: what the tray thread (and the
/// refresh loop) use to show, hide, wake and quit it. Cheap to clone.
#[derive(Clone)]
pub struct FlyControl {
    me: AppHandle<FlyState>,
    focus_agent: Arc<Mutex<Option<FocusAgentFn>>>,
}

impl FlyControl {
    /// Wires the agent-focus callback (`flyout.focusAgent`'s herdr call;
    /// the CLI injects the actual invocation).
    pub fn set_focus_agent(&self, f: impl Fn(&AgentHit) + Send + Sync + 'static) {
        *self.focus_agent.lock().unwrap() = Some(Arc::new(f));
    }

    /// Shows or hides the panel (tray left click, `flyout.toggle`).
    pub fn toggle(&self) {
        if self.me.is_visible() {
            self.hide();
        } else {
            self.show();
        }
    }

    /// Shows and activates the panel, resetting the scroll, the hover
    /// state and the auto-close clock (`flyout.show`).
    pub fn show(&self) {
        self.me.update(|s: &mut FlyState, tx: &mut Tx| {
            s.shown_at = Some(Instant::now());
            s.armed_evaluated = false; // re-evaluate the auto-close arming
            s.pointer_inside = false;
            s.activated = false;
            s.last_fp.clear(); // force a fresh layout commit on the next poll
            tx.scroll_to_y(CONTENT_KEY, 0.0); // Go show(): scroll = 0
        });
        // open() = show + activate (Go's SetForegroundWindow); on Wayland,
        // where apps can't place or focus windows, this is a plain show.
        self.me.open();
        self.me.wake(); // make poll size and anchor the panel at once
    }

    /// Shows the panel with a compositor activation token the tray handed
    /// over (Wayland `TrayEvent::Activate`), so the panel may take focus.
    pub fn activate(&self, token: impl Into<String>) {
        self.me.update(|s: &mut FlyState, tx: &mut Tx| {
            s.shown_at = Some(Instant::now());
            s.armed_evaluated = false;
            s.pointer_inside = false;
            s.activated = false;
            s.last_fp.clear();
            tx.scroll_to_y(CONTENT_KEY, 0.0);
        });
        self.me.activate(token);
        self.me.wake();
    }

    /// Hides the panel, retaining its state (`flyout.hide` → SW_HIDE).
    pub fn hide(&self) {
        self.me.hide();
    }

    /// Asks for a poll+repaint from any thread (`flyout.notify`).
    pub fn notify(&self) {
        self.me.wake();
    }

    /// Quits the panel's rune app (the tray's Quit).
    pub fn quit(&self) {
        self.me.quit();
    }

    /// Whether the panel is on screen (`AppHandle::is_visible`).
    pub fn is_visible(&self) -> bool {
        self.me.is_visible()
    }

    /// Lands the work area the run factory detected, before the first
    /// frame (`Tray::run` → `flyout::work_area`).
    pub fn apply_work_area(&self, area: WorkArea) {
        self.me.update(move |s: &mut FlyState, _| {
            s.area = area;
            s.applied_size = None; // re-anchor with the real geometry
            s.applied_pos = None;
        });
    }
}

/// The flyout panel: owns the windowless rune [`App`] (until
/// [`Tray::run`][crate::tray::Tray::run] hands it to the event loop) plus a
/// [`FlyControl`] the tray thread talks to. Left click toggles, state-change
/// wakes refresh it, and agent clicks call the injected focus callback.
pub struct Flyout {
    ctl: FlyControl,
    app: Option<App<FlyState>>,
}

/// The rune [`WindowOptions`] for the panel: borderless, transparent (the
/// rounded corners clip the fill), topmost, non-resizable, no min-size
/// floor (the poll sizes it to its content).
pub fn window_options() -> WindowOptions {
    WindowOptions {
        transparent: true,
        borderless: true,
        click_through: false,
        always_on_top: true,
        visible: false,
        position: None,
        physical_size: None,
        min_size: Some((0.0, 0.0)),
        max_size: None,
        resizable: false,
    }
}

impl Flyout {
    /// Builds the windowless panel app. The OS window itself is created by
    /// the event loop on the first [`FlyControl::show`] — rune window
    /// creation has loop affinity, which is what the Go `InitFlyout`
    /// "before Run" rule guarded.
    pub fn new(eng: Arc<engine::Engine>) -> Flyout {
        let focus_agent: Arc<Mutex<Option<FocusAgentFn>>> = Arc::new(Mutex::new(None));
        let eng2 = Arc::clone(&eng);
        let fa = Arc::clone(&focus_agent);
        let app = App::new(AppSpec {
            poll: Some(flyout_poll),
            on_key: Some(flyout_on_key),
            ..AppSpec::new("agent-notify", (420.0, 300.0), flyout_init, flyout_view)
        })
        .windowless() // no window at launch; open() creates and shows it
        .on_focus(flyout_focus);
        let me = app.handle();
        let me_for_state = me.clone();
        let app = app.init(move || FlyState::new(eng2, me_for_state, fa));
        Flyout {
            ctl: FlyControl {
                me,
                focus_agent,
            },
            app: Some(app),
        }
    }

    /// The thread-safe control handle (clone freely).
    pub fn control(&self) -> FlyControl {
        self.ctl.clone()
    }

    /// Takes the app out for the event loop (`Tray::run`). Panics if the
    /// app was already taken: [`Flyout::new`] and this are a pair.
    pub(crate) fn into_app(mut self) -> App<FlyState> {
        self.app.take().expect("flyout app taken twice")
    }
}

/// The panel's per-window state (`flyout` struct's window-thread fields).
/// `pub(crate)` so the tray can park the opaque [`App`] between init and
/// run; every field stays private.
pub(crate) struct FlyState {
    eng: Arc<engine::Engine>,
    me: AppHandle<FlyState>,
    focus_agent: Arc<Mutex<Option<FocusAgentFn>>>,
    /// collapse state: session key -> user override; defaults live in
    /// `build_rows` (`flyout.openState`).
    open_state: HashMap<String, bool>,
    /// group key ("session/finished" | "session/resting") -> override
    /// (`flyout.restOpen`).
    rest_open: HashMap<String, bool>,
    area: WorkArea,
    /// when the panel was shown: the auto-close clock and the focus grace.
    shown_at: Option<Instant>,
    /// the auto-close arming is evaluated once per show: only a
    /// non-scrollable panel gets one (`flyout.show`).
    armed_evaluated: bool,
    autoclose_armed: bool,
    /// whether we actually hold foreground: the deactivate-dismiss (click
    /// outside) may only fire when we had it, otherwise a lost foreground
    /// race hides the panel instantly (`flyout.activated`).
    activated: bool,
    /// pointer over the panel: the auto-close re-arms while it is.
    pointer_inside: bool,
    /// last applied window size/position, so poll only requests changes.
    applied_size: Option<(f32, f32)>,
    applied_pos: Option<(i32, i32)>,
    /// the engine view the view fn renders; refreshed by poll.
    snapshot: View,
    /// fingerprint of the last rendered row model: skips view re-runs when
    /// a refresh tick found nothing new.
    last_fp: String,
}

impl FlyState {
    fn new(
        eng: Arc<engine::Engine>,
        me: AppHandle<FlyState>,
        focus_agent: Arc<Mutex<Option<FocusAgentFn>>>,
    ) -> FlyState {
        FlyState {
            eng,
            me,
            focus_agent,
            open_state: HashMap::new(),
            rest_open: HashMap::new(),
            // Sane default until the run factory lands the real display
            // geometry (`Tray::run` sends it as the loop's first update).
            area: WorkArea::default(),
            shown_at: None,
            armed_evaluated: true,
            autoclose_armed: false,
            activated: false,
            pointer_inside: false,
            applied_size: None,
            applied_pos: None,
            snapshot: View::default(),
            last_fp: String::new(),
        }
    }
}

/// The app starts windowless: state is built by [`Flyout::new`]'s
/// `App::init` closure (the launcher pattern).
fn flyout_init() -> FlyState {
    unreachable!("FlyState is built by App::init")
}

/// View refresh + window geometry + auto-close (`WM_TIMER` refresh,
/// `flyout.paint`'s re-size and the auto-close timer). `now` is rune's app
/// clock; the wall-clock timers use `Instant` like the Go timers did.
fn flyout_poll(s: &mut FlyState, _now: f64, tx: &mut Tx) -> bool {
    if !s.me.is_visible() {
        // Hidden: the Go panel kills its timers on hide. Stop the cadence
        // and idle until the tray wakes us for the next show.
        return false;
    }
    // The engine view is read on the UI thread; Engine::view takes a
    // short-held mutex, so this is safe (flyout_windows.go header note).
    s.snapshot = s.eng.view();
    let rows = build_rows(&s.snapshot, &s.open_state, &s.rest_open);
    let fp = rows_fingerprint(&rows);
    let changed = fp != s.last_fp;
    s.last_fp = fp;

    let lay = layout(&rows, s.area.w_pts, s.area.h_pts);
    // refresh and in-panel collapse/expand change the content: keep the
    // window inside the work area and resize on the spot
    if s.applied_size != Some((lay.width, lay.window_h)) {
        tx.resize_window_instant((lay.width, lay.window_h));
        let pos = s.area.bottom_right_px(lay.width, lay.window_h);
        if s.applied_pos != Some(pos) {
            s.me.set_position(pos);
            s.applied_pos = Some(pos);
        }
        s.applied_size = Some((lay.width, lay.window_h));
    }
    if !s.armed_evaluated {
        s.armed_evaluated = true;
        // A scrollable panel stays open until dismissed (flyout.show).
        s.autoclose_armed = !lay.scrollable;
    }
    if s.autoclose_armed
        && s.shown_at.is_some_and(|t| t.elapsed() >= AUTO_CLOSE_DELAY)
    {
        if s.pointer_inside {
            // the user is reading the panel: re-arm instead of hiding
            s.shown_at = Some(Instant::now());
        } else {
            tx.close_window();
        }
    }
    tx.wake_after(REFRESH_EVERY);
    changed
}

/// Escape closes the panel (`WM_KEYDOWN` VK_ESCAPE).
fn flyout_on_key(_s: &mut FlyState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.key == Key::Escape {
        tx.close_window();
        return true;
    }
    false
}

/// Focus transitions (`WM_ACTIVATE`): a focus loss dismisses the panel —
/// but only if we actually held foreground. A panel whose activation was
/// yanked during startup (another window held foreground; `activateGrace`)
/// never had `activated` set, so it stays up until a real dismissal.
fn flyout_focus(s: &mut FlyState, focused: bool, tx: &mut Tx) {
    if focused {
        s.activated = true;
        return;
    }
    if s.activated {
        s.activated = false;
        tx.close_window();
    }
    // No activation held (the startup grace case): keep the panel up, as a
    // real tray click grants foreground and click-outside still dismisses
    // afterwards.
}

/// The scroll viewport's key (`Tx::scroll_to_y` reset on show).
const CONTENT_KEY: &str = "flyout-content";

/// Builds the panel (`flyout.paint`): a rounded, bordered, dark panel; the
/// two full-width summary rows; then the session grid, wheel-scrollable
/// with rune's own scrollbar thumb when the content overflows.
fn flyout_view(s: &FlyState, _cx: &mut Cx<FlyState>) -> El {
    let rows = build_rows(&s.snapshot, &s.open_state, &s.rest_open);
    let lay = layout(&rows, s.area.w_pts, s.area.h_pts);

    let mut header_rows = col().w_full();
    for (i, r) in rows.iter().enumerate().take(lay.header) {
        header_rows = header_rows.child(header_row(r, i, lay.width));
    }
    header_rows = header_rows.child(el().h(SEP_AFTER_SUM)); // sepAfterSum

    let mut grid = col().w_full().gap(GRID_ROW_GAP).items_start();
    for row_groups in &lay.groups {
        let mut gr = row().gap(COL_GAP).items_start();
        for group in row_groups {
            let mut g = col().w(lay.col_w);
            for &i in group {
                g = g.child(session_row_el(&rows[i], i));
            }
            gr = gr.child(g);
        }
        grid = grid.child(gr);
    }

    let content = col()
        .key(CONTENT_KEY)
        .w_full()
        .h_full()
        .scroll(ScrollSpec { stick_bottom: false })
        .child(header_rows)
        .child(grid);

    col()
        .key("panel")
        .w_full()
        .h_full()
        .fill(palette::BG)
        .border(1.0, palette::BORDER)
        .radius(14.0)
        .clip_rounded(14.0)
        .padding(FLY_PAD as f32)
        .on_hover_change(|s: &mut FlyState, hovering: bool, _| {
            s.pointer_inside = hovering; // the auto-close re-arms while over the panel
        })
        .child(content)
}

/// The two full-width header rows (`rowSummary`, `rowSummarySub`).
fn header_row(r: &FlyRow, _i: usize, panel_w: f32) -> El {
    match r.kind {
        Some(RowKind::Summary) => {
            let mut line = row().h(ROW_TITLE_H).center_y().gap(8.0).child(dot(r.dot))
                .child(
                    text("Agent Notify")
                        .font(FONT_HEAD.lh(ROW_TITLE_H))
                        .color(palette::TEXT)
                        .truncate(),
                )
                .child(spacer());
            if crate::buildinfo::is_release() {
                // stamp the build so a stale binary is visible at a glance
                line = line.child(
                    text(crate::buildinfo::VERSION)
                        .font(FONT_SMALL.lh(ROW_TITLE_H))
                        .color(palette::DIM)
                        .truncate(),
                );
            }
            line
        }
        Some(RowKind::SummarySub) => {
            // text on top, state bar further down with air around it
            // (paint(): text cell is rowH-4 tall, bar 4px at y+rowH+3)
            let mut c = col().h(ROW_SUB_H);
            c = c.child(
                row()
                    .h(ROW_H - 4.0)
                    .center_y()
                    .child(
                        text(r.text.clone())
                            .font(FONT_BODY.lh(ROW_H - 4.0))
                            .color(palette::DIM)
                            .truncate(),
                    ),
            );
            c = c.child(el().h(7.0));
            let mut bar = row().h(4.0);
            if !r.bar.is_empty() {
                // segments scale from the model's base width to the real one
                let inner_w = panel_w - 2.0 * FLY_PAD as f32;
                let base_inner = (FLY_WIDTH - 2 * FLY_PAD) as f32;
                for seg in &r.bar {
                    let sw = seg.px as f32 * inner_w / base_inner;
                    bar = bar.child(el().w(sw.max(0.0)).h(4.0).fill(col_of(seg.col)));
                }
            }
            c.child(bar)
        }
        _ => el(),
    }
}

/// Builds one row of the session grid, wiring hover, washes and clicks
/// (`flyout.paint` per-row drawing + `clickRow`). `idx` keys the element.
fn session_row_el(r: &FlyRow, idx: usize) -> El {
    let h = row_height(r.kind);
    match r.kind {
        Some(RowKind::Session) => {
            // session headers get a band so collapsibles outweigh the
            // washed rows they contain
            let mut row = row()
                .key(format!("session:{}:{idx}", r.toggle))
                .h(h)
                .fill(palette::BAND)
                .hover(palette::HOVER)
                .center_y()
                .child(
                    text(r.text.clone())
                        .font(FONT_BOLD.lh(h))
                        .color(palette::TEXT)
                        .truncate(),
                );
            if !r.host.is_empty() {
                row = row.child(
                    text(r.host.clone())
                        .font(FONT_SMALL.lh(h))
                        .color(palette::HOST)
                        .truncate()
                        .ml(6.0),
                );
            }
            if !r.sub.is_empty() {
                row = row.child(
                    text(r.sub.clone())
                        .font(FONT_SMALL.lh(h))
                        .color(palette::DIM)
                        .truncate()
                        .ml(4.0),
                );
            }
            // click toggles the session's expansion
            let key = r.toggle.clone();
            let open = r.open;
            row.on_press_tx(move |s: &mut FlyState, _| {
                let v = !open;
                s.open_state.insert(key.clone(), v);
            })
        }
        Some(RowKind::Group) => {
            // "N finished"/"N resting" line, click toggles the group
            let key = r.toggle.clone();
            let open = r.open;
            row()
                .key(format!("group:{}:{idx}", r.toggle))
                .h(h)
                .hover(palette::HOVER)
                .center_y()
                .gap(8.0)
                .pl(14.0) // nested rows sit inside their session header
                .child(dot(r.dot))
                .child(
                    text(r.text.clone())
                        .font(FONT_BODY.lh(h))
                        .color(if r.dim { palette::DIM } else { palette::TEXT })
                        .truncate(),
                )
                .on_press_tx(move |s: &mut FlyState, _| {
                    let v = !open;
                    s.rest_open.insert(key.clone(), v);
                })
        }
        Some(RowKind::Agent) => {
            // whole-block wash: status color blended over the panel
            agent_row_el(r, h, idx)
        }
        Some(RowKind::AgentCompact) => {
            let hit = (r.host.clone(), r.sess.clone(), r.pane.clone());
            row()
                .key(format!("compact:{idx}"))
                .h(h)
                .hover(palette::HOVER)
                .center_y()
                .gap(8.0)
                .pl(14.0) // nested rows sit inside their session header
                .child(dot(r.dot))
                .child(
                    text(r.text.clone())
                        .font(FONT_BODY.lh(h))
                        .color(palette::DIM)
                        .truncate(),
                )
                .on_press_tx(move |s: &mut FlyState, tx: &mut Tx| {
                    focus_agent(s, tx, &hit.0, &hit.1, &hit.2);
                })
        }
        _ => {
            // rowEmpty placeholder
            row()
                .key(format!("empty:{idx}"))
                .h(h)
                .child(
                    text(r.text.clone())
                        .font(FONT_BODY.lh(h))
                        .color(palette::DIM)
                        .truncate(),
                )
        }
    }
}

/// The two-line attention block: title over "runner · project", the whole
/// block washed with the status color, click focuses the agent
/// (`rowAgent` drawing + `focusAgent`).
fn agent_row_el(r: &FlyRow, h: f32, idx: usize) -> El {
    let hit = (r.host.clone(), r.sess.clone(), r.pane.clone());
    let dim = r.dim;
    row()
        .key(format!("agent:{idx}"))
        .h(h)
        .radius(8.0)
        .fill(wash(r.dot))
        .hover(WASH_HOVER)
        .child(
            row()
                .h(TITLE_H)
                .center_y()
                .gap(8.0)
                .child(dot(r.dot))
                .child(
                    text(r.text.clone())
                        .font(FONT_BODY.lh(TITLE_H))
                        .color(if dim { palette::DIM } else { palette::TEXT })
                        .truncate(),
                ),
        )
        .child(
            row()
                .h(INFO_H)
                .pl(17.0) // dot column (9px + 8px gap), as the Go text offset
                .child(
                    text(r.sub.clone())
                        .font(FONT_SMALL.lh(INFO_H))
                        .color(palette::DIM)
                        .truncate(),
                ),
        )
        .on_press_tx(move |s: &mut FlyState, tx: &mut Tx| {
            focus_agent(s, tx, &hit.0, &hit.1, &hit.2);
        })
}

/// The severity dot: a 9px disc (`procEllipse` with d = 9).
fn dot(colorref: u32) -> El {
    if colorref == 0 {
        el()
    } else {
        el().size(9.0, 9.0).radius(4.5).fill(col_of(colorref))
    }
}

/// Agent click: close the panel, then run the injected focus callback off
/// the UI thread (`flyout.focusAgent`: hide, then go func).
fn focus_agent(s: &mut FlyState, tx: &mut Tx, host: &str, sess: &str, pane: &str) {
    if pane.is_empty() {
        return;
    }
    tx.close_window();
    let hit = AgentHit {
        host: host.to_string(),
        sess: sess.to_string(),
        pane: pane.to_string(),
    };
    let f = s.focus_agent.lock().unwrap().clone();
    if let Some(f) = f {
        std::thread::spawn(move || f(&hit));
    }
}

/// Content fingerprint of a row model: identical output ⇒ identical
/// pixels, skip the view re-run (the flyout counterpart of the tray's
/// `fingerprint`).
fn rows_fingerprint(rows: &[FlyRow]) -> String {
    let mut b = String::new();
    for r in rows {
        b.push_str(&format!(
            "|{:?}:{:x}:{text}:{host}:{sess}:{pane}:{sub}:{dim}:{toggle}:{open}",
            r.kind,
            r.dot,
            text = r.text,
            host = r.host,
            sess = r.sess,
            pane = r.pane,
            sub = r.sub,
            dim = r.dim,
            toggle = r.toggle,
            open = r.open,
        ));
        for seg in &r.bar {
            b.push_str(&format!(";{:x}/{px}", seg.col, px = seg.px));
        }
    }
    b
}

// ---------------------------------------------------------------------------
// Tests (flyout_rows_test.go) — platform-neutral row model
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> View {
        View {
            sessions: vec![
                engine::SessionView {
                    host: "wsl".into(),
                    name: "default".into(),
                    up: true,
                    agents: vec![
                        engine::AgentView {
                            name: "pi".into(),
                            status: "working".into(),
                            title: "Refine shapes".into(),
                            project: "Nervefall".into(),
                            ..engine::AgentView::default()
                        },
                        engine::AgentView {
                            name: "omp".into(),
                            status: "idle".into(),
                            title: "old run".into(),
                            ..engine::AgentView::default()
                        },
                        engine::AgentView {
                            name: "omp".into(),
                            status: "done".into(),
                            title: "finished task".into(),
                            ..engine::AgentView::default()
                        },
                        engine::AgentView {
                            name: "pi".into(),
                            status: "done".into(),
                            wants: true,
                            title: "needs you".into(),
                            ..engine::AgentView::default()
                        },
                    ],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "quiet".into(),
                    up: true,
                    agents: vec![
                        engine::AgentView {
                            name: "pi".into(),
                            status: "idle".into(),
                            title: "resting one".into(),
                            ..engine::AgentView::default()
                        },
                        engine::AgentView {
                            name: "pi".into(),
                            status: "idle".into(),
                            title: "resting two".into(),
                            ..engine::AgentView::default()
                        },
                    ],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "justdone".into(),
                    up: true,
                    agents: vec![engine::AgentView {
                        name: "pi".into(),
                        status: "done".into(),
                        wants: true,
                        title: "freshly finished".into(),
                        ..engine::AgentView::default()
                    }],
                },
                engine::SessionView {
                    host: "wsl".into(),
                    name: "down".into(),
                    up: false,
                    agents: vec![engine::AgentView {
                        name: "pi".into(),
                        status: "idle".into(),
                        title: "gone".into(),
                        ..engine::AgentView::default()
                    }],
                },
            ],
        }
    }

    fn texts(rows: &[FlyRow]) -> Vec<String> {
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(r.text.clone());
            if !r.sub.is_empty() {
                out.push(r.sub.clone());
            }
        }
        out
    }

    fn find(rows: &[FlyRow], kind: RowKind, key: &str) -> FlyRow {
        for r in rows {
            if r.kind == Some(kind) && r.toggle == key {
                return r.clone();
            }
        }
        panic!("no {kind:?} row for {key:?} in {texts:?}", texts = texts(rows));
    }

    #[test]
    fn only_working_keeps_full_rows() {
        let rows = build_rows(&view(), &HashMap::new(), &HashMap::new());
        let joined = texts(&rows).join("\n");
        for want in [
            "▾ default",
            "Refine shapes",  // working block
            "pi · Nervefall", // block second line
            "▸ 1 finished",   // latched done collapses (green group)
            "▸ 2 resting",    // idle + unlatched done
            "▸ quiet",
            "2 agents",
            "▸ justdone", // finished-only session collapses
            "1 agent · 1 waiting",
            "▸ down",
            "1 agent · offline",
        ] {
            assert!(joined.contains(want), "missing {want:?} in:\n{joined}");
        }
        for r in &rows {
            if r.kind == Some(RowKind::Agent) && r.text.contains("needs you") {
                panic!("finished agents must not get full blocks");
            }
            if r.kind == Some(RowKind::AgentCompact) {
                panic!(
                    "compact rows must not appear while groups are closed: {:?}",
                    r.text
                );
            }
        }
        let s = find(&rows, RowKind::Session, "wsl/quiet");
        assert!(
            !s.open && s.dim,
            "all-resting session must default to collapsed and dim"
        );
    }

    #[test]
    fn overrides_flip_defaults() {
        let open_state = HashMap::from([("wsl/quiet".to_string(), true), ("wsl/default".to_string(), false)]);
        let rows = build_rows(&view(), &open_state, &HashMap::new());

        let q = find(&rows, RowKind::Session, "wsl/quiet");
        assert!(q.open, "user override must expand the quiet session");
        let compact = rows
            .iter()
            .filter(|r| r.kind == Some(RowKind::AgentCompact))
            .count();
        assert_eq!(
            compact, 0,
            "groups start closed: expected 0 compact rows, got {compact}"
        );

        let d = find(&rows, RowKind::Session, "wsl/default");
        assert!(!d.open, "user override must collapse the active session");
        for r in &rows {
            if r.kind == Some(RowKind::Agent) && r.text.contains("Refine shapes") {
                panic!("collapsed session must hide its attention blocks");
            }
        }
    }

    #[test]
    fn group_toggle_lists_compact_rows() {
        let rest_open = HashMap::from([
            ("wsl/default/finished".to_string(), true),
            ("wsl/default/resting".to_string(), true),
        ]);
        let rows = build_rows(&view(), &HashMap::new(), &rest_open);
        let (mut finished, mut resting) = (0, 0);
        for r in &rows {
            if r.kind == Some(RowKind::Group) && r.toggle == "wsl/default/finished" {
                assert!(
                    r.text.starts_with('▾') && r.open,
                    "finished group should read expanded, got {:?}",
                    r.text
                );
            }
            if r.kind == Some(RowKind::AgentCompact) {
                if r.dot == colorref(icon::color("waiting")) {
                    finished += 1;
                } else {
                    resting += 1;
                }
            }
        }
        assert_eq!(
            (finished, resting),
            (1, 2),
            "compact rows: finished={finished} resting={resting}, want 1 and 2"
        );
    }

    #[test]
    fn state_bar_covers_all_agents() {
        let rows = build_rows(&view(), &HashMap::new(), &HashMap::new());
        let mut px = 0i32;
        let mut segs = 0usize;
        for r in &rows {
            if r.kind == Some(RowKind::SummarySub) {
                segs = r.bar.len();
                for seg in &r.bar {
                    px += seg.px;
                }
            }
        }
        assert_eq!(segs, 4, "bar segments = {segs}, want 4 (blocked, working, waiting, idle)");
        const INNER: i32 = FLY_WIDTH - 2 * FLY_PAD;
        assert_eq!(px, INNER, "bar width = {px}, want {INNER}");
    }

    #[test]
    fn empty_view() {
        let rows = build_rows(&View::default(), &HashMap::new(), &HashMap::new());
        assert!(
            rows.len() == 3 && rows[2].kind == Some(RowKind::Empty),
            "empty view should end with the placeholder row, got {texts:?}",
            texts = texts(&rows)
        );
    }

    #[test]
    fn palette_consts_match_srgb_decode_of_the_go_colorrefs() {
        for (c, want) in [
            (0x00C8C87F, palette::HOST),
            (0x00202020, palette::BG),
            (0x00453C3C, palette::BORDER),
            (0x002B2B2B, palette::BAND),
            (0x00F2F2F2, palette::TEXT),
            (0x00A0A0A0, palette::DIM),
            (0x00303030, palette::HOVER),
        ] {
            let got = col_of(c);
            for (g, w) in [(got.r, want.r), (got.g, want.g), (got.b, want.b)] {
                assert!(
                    (g - w).abs() < 1e-6,
                    "palette 0x{c:06X} channel {g} != srgb_decode {w}"
                );
            }
        }
    }

    #[test]
    fn gdi_safe_strips_tofu_glyphs() {
        let cases: [(&str, &str); 8] = [
            (
                "\u{F06A9} KA: Analyze and align ka agent repo",
                "KA: Analyze and align ka agent repo",
            ), // nerd-font PUA icon
            ("π > \u{F0B63} README: basics", "π > README: basics"), // live title shape
            ("README ⚙\u{FE0F}", "README"),                         // emoji + variation selector
            (
                "▾ wsl/default \u{B7} pi \u{B7} Rediseñar",
                "▾ wsl/default · pi · Rediseñar",
            ), // keep chevrons, dots, accents
            ("a\u{200D}b", "ab"),        // zero-width joiner
            ("\u{2B50} stars", "stars"), // star
            ("日本語 タスク", "日本語 タスク"), // CJK passes
            ("plain", "plain"),
        ];
        for (input, want) in cases {
            let got = gdi_safe(input);
            assert_eq!(got, want, "gdiSafe({input:?}) = {got:?}, want {want:?}");
        }
    }

    /// One deterministic session for the smoke render below.
    struct SmokeBackend;

    impl crate::herdr::Backend for SmokeBackend {
        fn name(&self) -> &'static str {
            "local"
        }
        fn discover(&self, _timeout: Duration) -> anyhow::Result<Vec<crate::herdr::Session>> {
            Ok(vec![crate::herdr::Session {
                host: "wsl".into(),
                name: "default".into(),
                ..crate::herdr::Session::default()
            }])
        }
        fn fetch(
            &self,
            _session: &crate::herdr::Session,
            _timeout: Duration,
        ) -> anyhow::Result<crate::herdr::Snapshot> {
            Ok(crate::herdr::Snapshot {
                agents: vec![
                    crate::herdr::Agent {
                        pane_id: "w1:p1".into(),
                        name: "pi".into(),
                        status: "working".into(),
                        title: "Refine shapes".into(),
                        project: "Nervefall".into(),
                        ..crate::herdr::Agent::default()
                    },
                    crate::herdr::Agent {
                        pane_id: "w1:p2".into(),
                        name: "pi".into(),
                        status: "blocked".into(),
                        title: "Waiting on tests".into(),
                        project: "Nervefall".into(),
                        ..crate::herdr::Agent::default()
                    },
                    crate::herdr::Agent {
                        pane_id: "w1:p3".into(),
                        name: "omp".into(),
                        status: "done".into(),
                        title: "Old run".into(),
                        ..crate::herdr::Agent::default()
                    },
                ],
            })
        }
        fn rescan(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// Smoke test: renders the panel headless to a PNG so the row painters
    /// are verified with pixels, not just the model. Ignored by default
    /// (needs a wgpu adapter — vulkan/lavapipe under WSLg); run it with
    /// `cargo test -- --ignored smoke`.
    #[test]
    #[ignore = "needs a wgpu adapter; run explicitly: cargo test -- --ignored smoke"]
    fn smoke_renders_the_panel_to_png() {
        let eng = Arc::new(engine::Engine::with_backends(
            engine::Config::default(),
            vec![Box::new(SmokeBackend)],
        ));
        eng.probe_once();
        assert!(!eng.view().sessions.is_empty(), "smoke engine must have a session");

        // A standalone app whose state is the panel's, pre-loaded with the
        // engine view (poll is None here: the snapshot must survive).
        let app0 = App::new(AppSpec::new(
            "agent-notify",
            (420.0, 300.0),
            flyout_init,
            flyout_view,
        ));
        let me = app0.handle();
        let state = FlyState {
            snapshot: eng.view(),
            area: WorkArea {
                x_px: 0,
                y_px: 0,
                right_px: 420,
                bottom_px: 300,
                w_pts: 420.0,
                h_pts: 300.0,
                scale: 1.0,
            },
            ..FlyState::new(eng, me.clone(), Arc::new(Mutex::new(None)))
        };
        let app = app0.init(move || state);
        let snap = std::env::temp_dir().join("agent-notify-flyout-smoke.png");
        rune::tooling::headless(
            app,
            rune::tooling::Args::parse_from(vec![
                "--snap".to_string(),
                snap.to_string_lossy().into_owned(),
                "--frames".to_string(),
                "8".to_string(),
            ]),
        );
        let meta = std::fs::metadata(&snap).expect("smoke PNG must exist");
        assert!(meta.len() > 0, "smoke PNG must not be empty");
        eprintln!("flyout smoke render written to {}", snap.display());
    }
}
